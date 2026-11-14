//! New versions of Spotlight, from the releases on its GitHub page.
//!
//! - The installed copy downloads the new Setup and runs it in its update mode. Setup asks
//!   Spotlight to quit, replaces the exe and starts the new version.
//! - The portable copy swaps its own exe for the new one under the same name, so its
//!   shortcuts and taskbar pin keep working, and starts it.
//! - Any other copy, like a build from source, only links to the release page.
//!
//! The check asks GitHub for the latest release and nothing else: no account, nothing about
//! the user. Downloads come only from github.com, and are checked against the size and
//! SHA-256 GitHub lists for them.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{header, Client, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Whether Spotlight looks for new versions by itself. Not on a Mac yet: no release has a file
/// for one, so word of a new version would lead nowhere.
pub const OFFERED_HERE: bool = cfg!(not(target_os = "macos"));
const LATEST_RELEASE: &str = "https://api.github.com/repos/Beroris/spotlight/releases/latest";
/// Where a release's files may come from.
const DOWNLOADS_FROM: &str = "https://github.com/";
/// The installer's AppId, as Windows lists the installed app under it.
#[cfg(target_os = "windows")]
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{A87CA1A7-A57E-4C0B-8B55-1346E73C3AAB}_is1";
/// The installed copy's downloads wait here, in Windows' temp folder.
const DOWNLOAD_FOLDER: &str = "Spotlight update";
/// Passed to the new portable copy, with the old one's process id, so it waits for it to go.
pub const AFTER_UPDATE_ARG: &str = "--after-update";

/// How this copy of Spotlight gets a new version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Installed by Setup: runs the new Setup.
    Installed,
    /// Named "…Portable…": replaces its own exe.
    Portable,
    /// Anything else, like a build from source: links to the release page.
    Other,
}

impl Kind {
    pub fn installs_itself(self) -> bool {
        self != Kind::Other
    }

    /// The release file this kind of copy installs from.
    fn asset_suffix(self) -> Option<&'static str> {
        match self {
            Kind::Installed => Some("-Setup.exe"),
            Kind::Portable => Some("-Portable.exe"),
            Kind::Other => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// Such as "1.1.0".
    pub version: String,
    /// The release's page, with its notes.
    pub page: String,
    /// The file to install from, if this kind of copy can install it.
    pub download: Option<Download>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Download {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Lowercase hex, when GitHub lists one.
    pub sha256: Option<String>,
}

#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Deserialize)]
struct AssetJson {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    /// Such as "sha256:0123…".
    #[serde(default)]
    digest: Option<String>,
}

/// "1.2.3" or "v1.2.3". Anything else, like "1.2.3-beta", isn't a release to offer.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim();
    let text = text.strip_prefix(|c: char| c == 'v' || c == 'V').unwrap_or(text);
    let mut parts = text.split('.');
    let mut number = || -> Option<u64> {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let version = (number()?, number()?, number()?);
    parts.next().is_none().then_some(version)
}

/// Reads GitHub's answer for the latest release. `None` when it isn't newer than `current`.
pub fn parse_release(json: &[u8], kind: Kind, current: &str) -> Result<Option<Release>> {
    let release: ReleaseJson = serde_json::from_slice(json).context("GitHub's answer didn't make sense")?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let (Some(version), Some(current)) = (parse_version(&release.tag_name), parse_version(current)) else {
        return Ok(None);
    };
    if version <= current {
        return Ok(None);
    }
    let download = kind.asset_suffix().and_then(|suffix| {
        release
            .assets
            .into_iter()
            .find(|asset| {
                asset.name.ends_with(suffix)
                    && is_plain_file_name(&asset.name)
                    && asset.browser_download_url.starts_with(DOWNLOADS_FROM)
            })
            .map(|asset| Download {
                sha256: asset
                    .digest
                    .as_deref()
                    .and_then(|digest| digest.strip_prefix("sha256:"))
                    .map(str::to_ascii_lowercase),
                name: asset.name,
                url: asset.browser_download_url,
                size: asset.size,
            })
    });
    let page = if release.html_url.starts_with(DOWNLOADS_FROM) {
        release.html_url
    } else {
        releases_page()
    };
    Ok(Some(Release {
        version: format!("{}.{}.{}", version.0, version.1, version.2),
        page,
        download,
    }))
}

/// The latest release's page, for when there's no particular one to link to.
pub fn releases_page() -> String {
    format!("{DOWNLOADS_FROM}Beroris/spotlight/releases/latest")
}

fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty() && Path::new(name).file_name().is_some_and(|file| file == name) && !name.contains(['/', '\\', ':'])
}

fn user_agent() -> String {
    format!("Spotlight/{CURRENT_VERSION}")
}

/// Asks GitHub for the latest release. `None` when this copy is up to date.
pub async fn check(http: &Client, kind: Kind) -> Result<Option<Release>> {
    let response = http
        .get(LATEST_RELEASE)
        .header(header::USER_AGENT, user_agent())
        .header(header::ACCEPT, "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| anyhow!("Couldn't reach GitHub. Check your connection."))?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::NOT_FOUND => bail!("Couldn't find Spotlight's releases on GitHub."),
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => bail!("GitHub is busy. Try again later."),
        status => bail!("GitHub answered {}.", status.as_u16()),
    }
    let body = response.bytes().await.map_err(|_| anyhow!("Couldn't reach GitHub. Check your connection."))?;
    parse_release(&body, kind, CURRENT_VERSION)
}

/// A download on its way to disk: written beside its final name, then checked.
struct Receiving {
    file: std::io::BufWriter<fs::File>,
    partial: PathBuf,
    hasher: Sha256,
    received: u64,
}

impl Receiving {
    fn start(partial: PathBuf) -> Result<Self> {
        let file = fs::File::create(&partial).with_context(|| format!("Couldn't save the update in {}", partial.parent().unwrap_or(&partial).display()))?;
        Ok(Self { file: std::io::BufWriter::with_capacity(64 * 1024, file), partial, hasher: Sha256::new(), received: 0 })
    }

    fn take(&mut self, chunk: &[u8]) -> Result<()> {
        self.file.write_all(chunk).context("Couldn't save the update")?;
        self.hasher.update(chunk);
        self.received += chunk.len() as u64;
        Ok(())
    }

    /// Moves the file to `destination` if it's whole and matches what GitHub lists.
    fn finish(self, expected: &Download, destination: &Path) -> Result<()> {
        let Receiving { file, partial, hasher, received } = self;
        let result = (|| {
            let file = file.into_inner().map_err(|error| error.into_error()).context("Couldn't save the update")?;
            file.sync_all().context("Couldn't save the update")?;
            drop(file);
            if expected.size > 0 && received != expected.size {
                bail!("The download was cut short. Try again.");
            }
            if let Some(sha256) = &expected.sha256 {
                if format!("{:x}", hasher.finalize()) != *sha256 {
                    bail!("The download didn't match the release. Try again.");
                }
            }
            fs::rename(&partial, destination).context("Couldn't save the update")
        })();
        if result.is_err() {
            let _ = fs::remove_file(&partial);
        }
        result
    }

    fn abandon(self) {
        let Receiving { file, partial, .. } = self;
        drop(file);
        let _ = fs::remove_file(partial);
    }
}

/// Where this kind of copy keeps the new version until it's installed.
pub fn download_path(kind: Kind, download: &Download) -> Result<PathBuf> {
    match kind {
        Kind::Installed => Ok(std::env::temp_dir().join(DOWNLOAD_FOLDER).join(&download.name)),
        // Beside the exe, so moving it into place can't fail for being on another drive.
        Kind::Portable => {
            let exe = std::env::current_exe()?;
            Ok(sibling(&exe, ".new"))
        }
        Kind::Other => bail!("This copy of Spotlight can't update itself."),
    }
}

/// `path` with `suffix` added to its whole file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Downloads the new version to `destination`. `progress` hears the share done, from 0 to 1,
/// each time it moves on by a whole percent.
pub async fn download(http: &Client, download: &Download, destination: &Path, mut progress: impl FnMut(f32)) -> Result<()> {
    if let Some(folder) = destination.parent() {
        fs::create_dir_all(folder).with_context(|| format!("Couldn't save the update in {}", folder.display()))?;
    }
    let mut response = http
        .get(&download.url)
        .header(header::USER_AGENT, user_agent())
        // Long enough for a slow connection. The Setup is about 8 MB.
        .timeout(Duration::from_secs(15 * 60))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| anyhow!("Couldn't download the update. Check your connection."))?;
    let total = response.content_length().filter(|&length| length > 0).unwrap_or(download.size).max(1);
    let mut receiving = Receiving::start(sibling(destination, ".part"))?;
    let mut percent = 0;
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => {
                receiving.abandon();
                bail!("The download stopped. Check your connection and try again.");
            }
        };
        if let Err(error) = receiving.take(&chunk) {
            receiving.abandon();
            return Err(error);
        }
        let now = (receiving.received.saturating_mul(100) / total).min(100);
        if now != percent {
            percent = now;
            progress(now as f32 / 100.0);
        }
    }
    receiving.finish(download, destination)
}

/// Starts installing the downloaded version. Spotlight must quit straight after.
pub fn start_install(kind: Kind, file: &Path) -> Result<()> {
    match kind {
        Kind::Installed => {
            // Setup's update mode keeps the user's choices as they are and starts Spotlight
            // again when it's done. It shows only its progress.
            std::process::Command::new(file)
                .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/UPDATE"])
                .spawn()
                .context("Couldn't start the installer")?;
        }
        Kind::Portable => {
            let exe = std::env::current_exe()?;
            swap_in(&exe, file)?;
            if let Err(error) = std::process::Command::new(&exe)
                .arg(AFTER_UPDATE_ARG)
                .arg(std::process::id().to_string())
                .spawn()
            {
                // The new version is in place, so opening Spotlight again starts it.
                return Err(error).context("Updated. Open Spotlight again to start the new version");
            }
        }
        Kind::Other => bail!("This copy of Spotlight can't update itself."),
    }
    Ok(())
}

/// Puts `new` where `exe` is, keeping the old one as "….old" until it has quit. Windows lets a
/// running exe be renamed, though not deleted.
fn swap_in(exe: &Path, new: &Path) -> Result<()> {
    let old = sibling(exe, ".old");
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old).context("Couldn't make room for the new version")?;
    if let Err(error) = fs::rename(new, exe) {
        let _ = fs::rename(&old, exe);
        return Err(error).context("Couldn't put the new version in place");
    }
    Ok(())
}

/// Run first thing at startup. After a portable update, waits for the old version to quit, so
/// the new one isn't turned away as a second copy, and removes it.
pub fn finish_update(args: &[String]) {
    let Some(at) = args.iter().position(|arg| arg == AFTER_UPDATE_ARG) else {
        return;
    };
    if let Some(process_id) = args.get(at + 1).and_then(|id| id.parse::<u32>().ok()) {
        wait_for_exit(process_id, Duration::from_secs(10));
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(sibling(&exe, ".old"));
    }
}

/// Clears what an update left behind: the old portable exe, a download that never finished,
/// and the Setup that ran. Setup may still be finishing when the new version starts, so this
/// runs a little later, with the first check.
pub fn clean_up() {
    if let Ok(exe) = std::env::current_exe() {
        for suffix in [".old", ".new", ".new.part"] {
            let leftover = sibling(&exe, suffix);
            if leftover.exists() {
                let _ = fs::remove_file(leftover);
            }
        }
    }
    let folder = std::env::temp_dir().join(DOWNLOAD_FOLDER);
    if folder.exists() {
        let _ = fs::remove_dir_all(folder);
    }
}

/// How this copy updates: by the portable name, or by being where Setup installed Spotlight.
pub fn this_copy() -> Kind {
    if crate::config::is_portable() {
        return Kind::Portable;
    }
    let (Ok(exe), Some(installed)) = (std::env::current_exe(), installed_folder()) else {
        return Kind::Other;
    };
    let same = |a: &Path, b: &Path| match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()),
        _ => false,
    };
    match exe.parent() {
        Some(folder) if same(folder, &installed) => Kind::Installed,
        _ => Kind::Other,
    }
}

#[cfg(target_os = "windows")]
fn installed_folder() -> Option<PathBuf> {
    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ},
    };
    let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let mut buffer = [0u16; 1024];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(UNINSTALL_KEY).as_ptr(),
            wide("InstallLocation").as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != ERROR_SUCCESS {
        return None;
    }
    let length = (size as usize / 2).saturating_sub(1);
    let folder = String::from_utf16_lossy(&buffer[..length]);
    (!folder.trim().is_empty()).then(|| PathBuf::from(folder.trim()))
}

#[cfg(not(target_os = "windows"))]
fn installed_folder() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "windows")]
fn wait_for_exit(process_id: u32, timeout: Duration) {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    unsafe {
        let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id);
        if process.is_null() {
            return;
        }
        WaitForSingleObject(process, timeout.as_millis().min(u128::from(u32::MAX)) as u32);
        CloseHandle(process);
    }
}

#[cfg(not(target_os = "windows"))]
fn wait_for_exit(process_id: u32, timeout: Duration) {
    // Sending no signal at all only asks whether the process is still there.
    let until = std::time::Instant::now() + timeout;
    while unsafe { libc::kill(process_id as libc::pid_t, 0) } == 0 && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_json(tag: &str, extra: &str) -> String {
        format!(
            r#"{{
                "tag_name": "{tag}",
                "html_url": "https://github.com/Beroris/spotlight/releases/tag/{tag}",
                {extra}
                "assets": [
                    {{ "name": "Spotlight-1.1.0-Setup.exe", "size": 8047861,
                       "browser_download_url": "https://github.com/Beroris/spotlight/releases/download/v1.1.0/Spotlight-1.1.0-Setup.exe",
                       "digest": "sha256:ABCDEF0123" }},
                    {{ "name": "Spotlight-1.1.0-Portable.exe", "size": 21653504,
                       "browser_download_url": "https://github.com/Beroris/spotlight/releases/download/v1.1.0/Spotlight-1.1.0-Portable.exe" }}
                ]
            }}"#
        )
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert_eq!(parse_version("v1.10.0"), Some((1, 10, 0)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert!(parse_version("1.10.0") > parse_version("1.9.9"));
        for not_a_release in ["1.2", "1.2.3.4", "1.2.3-beta", "v", "", "1..2", "1.2.+3", "latest"] {
            assert_eq!(parse_version(not_a_release), None, "{not_a_release}");
        }
    }

    #[test]
    fn only_a_newer_release_is_offered() {
        let json = release_json("v1.1.0", "");
        assert!(parse_release(json.as_bytes(), Kind::Installed, "1.0.0").unwrap().is_some());
        assert!(parse_release(json.as_bytes(), Kind::Installed, "1.1.0").unwrap().is_none());
        assert!(parse_release(json.as_bytes(), Kind::Installed, "1.2.0").unwrap().is_none());
        let draft = release_json("v1.1.0", r#""draft": true,"#);
        assert!(parse_release(draft.as_bytes(), Kind::Installed, "1.0.0").unwrap().is_none());
        let prerelease = release_json("v1.1.0", r#""prerelease": true,"#);
        assert!(parse_release(prerelease.as_bytes(), Kind::Installed, "1.0.0").unwrap().is_none());
        let odd_tag = release_json("nightly", "");
        assert!(parse_release(odd_tag.as_bytes(), Kind::Installed, "1.0.0").unwrap().is_none());
        assert!(parse_release(b"not json", Kind::Installed, "1.0.0").is_err());
    }

    #[test]
    fn each_kind_of_copy_gets_its_own_file() {
        let json = release_json("v1.1.0", "");
        let installed = parse_release(json.as_bytes(), Kind::Installed, "1.0.0").unwrap().unwrap();
        assert_eq!(installed.version, "1.1.0");
        let setup = installed.download.unwrap();
        assert_eq!(setup.name, "Spotlight-1.1.0-Setup.exe");
        assert_eq!(setup.size, 8047861);
        assert_eq!(setup.sha256.as_deref(), Some("abcdef0123"));

        let portable = parse_release(json.as_bytes(), Kind::Portable, "1.0.0").unwrap().unwrap().download.unwrap();
        assert_eq!(portable.name, "Spotlight-1.1.0-Portable.exe");
        assert_eq!(portable.sha256, None);

        let other = parse_release(json.as_bytes(), Kind::Other, "1.0.0").unwrap().unwrap();
        assert_eq!(other.download, None);
        assert_eq!(other.page, "https://github.com/Beroris/spotlight/releases/tag/v1.1.0");
    }

    #[test]
    fn files_from_anywhere_but_github_or_with_odd_names_are_ignored() {
        let json = r#"{
            "tag_name": "v1.1.0", "html_url": "https://example.com/release",
            "assets": [
                { "name": "Spotlight-1.1.0-Setup.exe", "size": 1, "browser_download_url": "https://example.com/Spotlight-1.1.0-Setup.exe" },
                { "name": "..\\Spotlight-1.1.0-Setup.exe", "size": 1, "browser_download_url": "https://github.com/x/Spotlight-1.1.0-Setup.exe" }
            ]
        }"#;
        let release = parse_release(json.as_bytes(), Kind::Installed, "1.0.0").unwrap().unwrap();
        assert_eq!(release.download, None);
        assert_eq!(release.page, releases_page());
    }

    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("spotlight-updates-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn expected(bytes: &[u8], sha256: bool) -> Download {
        Download {
            name: "Spotlight-1.1.0-Setup.exe".into(),
            url: String::new(),
            size: bytes.len() as u64,
            sha256: sha256.then(|| format!("{:x}", Sha256::digest(bytes))),
        }
    }

    #[test]
    fn a_whole_matching_download_is_kept_and_anything_else_thrown_away() {
        let folder = scratch("receive");
        let destination = folder.join("Spotlight-1.1.0-Setup.exe");
        let partial = sibling(&destination, ".part");
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();

        let mut receiving = Receiving::start(partial.clone()).unwrap();
        for chunk in bytes.chunks(16 * 1024) {
            receiving.take(chunk).unwrap();
        }
        receiving.finish(&expected(&bytes, true), &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), bytes);
        assert!(!partial.exists());
        fs::remove_file(&destination).unwrap();

        // Cut short.
        let mut receiving = Receiving::start(partial.clone()).unwrap();
        receiving.take(&bytes[..1000]).unwrap();
        let error = receiving.finish(&expected(&bytes, false), &destination).unwrap_err();
        assert!(error.to_string().contains("cut short"), "{error}");
        assert!(!destination.exists() && !partial.exists());

        // Right size, wrong bytes.
        let mut tampered = bytes.clone();
        tampered[1234] ^= 1;
        let mut receiving = Receiving::start(partial.clone()).unwrap();
        receiving.take(&tampered).unwrap();
        let error = receiving.finish(&expected(&bytes, true), &destination).unwrap_err();
        assert!(error.to_string().contains("didn't match"), "{error}");
        assert!(!destination.exists() && !partial.exists());

        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn the_portable_copy_swaps_in_the_new_exe_under_its_own_name() {
        let folder = scratch("swap");
        let exe = folder.join("Spotlight-1.0.0-Portable.exe");
        let new = sibling(&exe, ".new");
        fs::write(&exe, b"old version").unwrap();
        fs::write(&new, b"new version").unwrap();
        swap_in(&exe, &new).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new version");
        assert_eq!(fs::read(sibling(&exe, ".old")).unwrap(), b"old version");
        assert!(!new.exists());

        // Nothing to swap in: the copy stays as it was.
        fs::remove_file(sibling(&exe, ".old")).unwrap();
        assert!(swap_in(&exe, &new).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"new version");
        fs::remove_dir_all(folder).unwrap();
    }
}
