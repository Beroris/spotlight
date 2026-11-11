use std::{net::SocketAddr, sync::Arc};

use anyhow::{anyhow, Context, Result};
use axum::{extract::Query, response::Html, routing::get, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::models::{AppConfig, TokenResponse, TokenStore};
use librespot_oauth::OAuthClientBuilder;

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    error: Option<String>,
}

/// The error from a sign-in stopped by `cancel_sign_in`.
pub const CANCELLED: &str = "sign-in cancelled";

/// Stops whichever sign-in is waiting on the browser, as if the browser had come back without
/// agreeing. librespot's waits in a way nothing else can interrupt, and holds up playing on this
/// PC until it ends.
pub fn cancel_sign_in() {
    use std::io::{Read, Write};
    std::thread::spawn(|| {
        for (port, path) in [(5543, "/callback/"), (5544, "/login")] {
            let address = SocketAddr::from(([127, 0, 0, 1], port));
            let Ok(mut stream) = std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300)) else {
                continue;
            };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            let request = format!("GET {path}?error=cancelled HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
            if stream.write_all(request.as_bytes()).is_ok() {
                let _ = stream.read(&mut [0u8; 1024]);
            }
        }
    });
}

pub async fn login(config: &AppConfig) -> Result<TokenStore> {
    let (verifier, challenge) = pkce_pair();
    let redirect = config.redirect_uri.clone();
    let client_id = config.client_id.clone();
    let (tx, rx) = oneshot::channel::<Result<String, String>>();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));

    let app = Router::new().route("/callback/", get({
        let tx = Arc::clone(&tx);
        move |Query(query): Query<CallbackQuery>| {
            let tx = Arc::clone(&tx);
            async move {
                // Spotify sends back an error, rather than a code, when the person doesn't agree.
                let answer = match (query.code, query.error) {
                    (Some(code), _) => Some(Ok(code)),
                    (None, Some(error)) => Some(Err(error)),
                    (None, None) => None,
                };
                let agreed = matches!(answer, Some(Ok(_)));
                if let Some(answer) = answer {
                    if let Some(sender) = tx.lock().expect("callback mutex").take() {
                        let _ = sender.send(answer);
                    }
                }
                let message = if agreed {
                    "Spotlight is connected. You can close this tab."
                } else {
                    "Spotlight didn't connect. You can close this tab and try again from Spotlight."
                };
                Html(format!("<html><body style=\"font-family:Segoe UI;padding:24px;background:#121212;color:#f4f4f4;\">{message}</body></html>"))
            }
        }
    }));

    let addr: SocketAddr = "127.0.0.1:5543".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("another sign-in is still waiting on the browser. Finish it or close its tab, then try again")?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let scopes = [
        "app-remote-control",
        "streaming",
        "user-read-email",
        "user-read-private",
        "user-read-playback-state",
        "user-modify-playback-state",
        "user-read-currently-playing",
        "playlist-read-private",
        "playlist-read-collaborative",
        "playlist-modify-public",
        "playlist-modify-private",
        "user-library-read",
        "user-library-modify",
        "user-read-recently-played",
    ].join(" ");

    let auth_url = format!(
        "https://accounts.spotify.com/authorize?client_id={}&response_type=code&redirect_uri={}&code_challenge_method=S256&code_challenge={}&scope={}",
        urlencoding::encode(&client_id),
        urlencoding::encode(&redirect),
        urlencoding::encode(&challenge),
        urlencoding::encode(&scopes)
    );

    if let Err(error) = webbrowser::open(&auth_url) {
        server.abort();
        return Err(anyhow!("failed to open browser: {error}"));
    }
    // Don't hold the callback port forever if the user closes the browser tab.
    let code = match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
        Ok(Ok(Ok(code))) => code,
        Ok(Ok(Err(error))) => {
            server.abort();
            return Err(match error.as_str() {
                "cancelled" => anyhow!(CANCELLED),
                "access_denied" => anyhow!("you didn't agree in the browser"),
                other => anyhow!("Spotify turned the sign-in down ({other})"),
            });
        }
        Ok(Err(_)) => {
            server.abort();
            return Err(anyhow!("login callback did not arrive"));
        }
        Err(_) => {
            server.abort();
            return Err(anyhow!("the browser didn't come back within 5 minutes. Try again"));
        }
    };
    server.abort();

    exchange_code(&client_id, &redirect, &verifier, &code).await
}

/// Spotify turned a refresh token down, so only signing in again will do. Any other failure,
/// such as no network yet after the PC wakes, is worth trying again later instead.
#[derive(Debug)]
pub struct SignInExpired(String);

impl std::fmt::Display for SignInExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "token refresh failed: {}", self.0)
    }
}

impl std::error::Error for SignInExpired {}

/// Talks to Spotify's sign-in service. A connection that stalls, say just after the PC wakes,
/// gives up rather than holding up everything that waits for a token.
fn accounts_client() -> Result<Client> {
    Ok(Client::builder().timeout(std::time::Duration::from_secs(15)).build()?)
}

pub async fn refresh_token(client_id: &str, refresh_token: &str) -> Result<TokenStore> {
    let client = accounts_client()?;
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];

    let response = client
        .post("https://accounts.spotify.com/api/token")
        .form(&params)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SignInExpired(body).into());
        }
        return Err(anyhow!("token refresh failed ({status}): {body}"));
    }

    let token: TokenResponse = response.json().await?;
    Ok(TokenStore {
        access_token: token.access_token,
        refresh_token: token.refresh_token.unwrap_or_else(|| refresh_token.to_string()),
        expires_at_epoch: crate::config::now_epoch() + token.expires_in.saturating_sub(30),
    })
}

pub async fn login_for_local_playback(client_id: &str) -> Result<TokenStore> {
    let scopes = vec![
        "app-remote-control",
        "playlist-modify",
        "playlist-modify-private",
        "playlist-modify-public",
        "playlist-read",
        "playlist-read-collaborative",
        "playlist-read-private",
        "streaming",
        "ugc-image-upload",
        "user-follow-modify",
        "user-follow-read",
        "user-library-modify",
        "user-library-read",
        "user-modify",
        "user-modify-playback-state",
        "user-modify-private",
        "user-personalized",
        "user-read-birthdate",
        "user-read-currently-playing",
        "user-read-email",
        "user-read-play-history",
        "user-read-playback-position",
        "user-read-playback-state",
        "user-read-private",
        "user-read-recently-played",
        "user-top-read",
    ];

    let oauth = OAuthClientBuilder::new(client_id, "http://127.0.0.1:5544/login", scopes)
        .open_in_browser()
        .with_custom_message("Spotify local playback login complete. You can close this window.")
        .build()
        .map_err(|e| anyhow!("failed to create librespot oauth client: {e}"))?;

    let token = oauth
        .get_access_token_async()
        .await
        .map_err(|e| anyhow!("failed to get local playback token: {e}"))?;

    Ok(TokenStore {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at_epoch: crate::config::now_epoch() + 3500,
    })
}

async fn exchange_code(client_id: &str, redirect_uri: &str, verifier: &str, code: &str) -> Result<TokenStore> {
    let client = accounts_client()?;
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];

    let response = client
        .post("https://accounts.spotify.com/api/token")
        .form(&params)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow!("token exchange failed: {}", response.text().await.unwrap_or_default()));
    }

    let token: TokenResponse = response.json().await?;
    let refresh = token.refresh_token.context("spotify did not return a refresh token")?;
    Ok(TokenStore {
        access_token: token.access_token,
        refresh_token: refresh,
        expires_at_epoch: crate::config::now_epoch() + token.expires_in.saturating_sub(30),
    })
}

fn pkce_pair() -> (String, String) {
    let mut bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}
