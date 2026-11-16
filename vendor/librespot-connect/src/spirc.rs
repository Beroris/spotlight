use crate::{
    LoadContextOptions, LoadRequestOptions, PlayContext,
    context_resolver::{ContextAction, ContextResolver, ResolveContext},
    core::{
        Error, Session, SpotifyUri,
        authentication::Credentials,
        dealer::{
            manager::{BoxedStream, BoxedStreamResult, Reply, RequestReply},
            protocol::{Command, FallbackWrapper, Message, Request},
        },
        session::UserAttributes,
        spclient::TransferRequest,
    },
    model::{LoadRequest, PlayingTrack, SpircPlayStatus},
    playback::{
        mixer::Mixer,
        player::{Player, PlayerEvent, PlayerEventChannel},
    },
    protocol::{
        connect::{Cluster, ClusterUpdate, LogoutCommand, SetVolumeCommand},
        context::Context,
        explicit_content_pubsub::UserAttributesUpdate,
        player::PlayerState,
        playlist4_external::PlaylistModificationInfo,
        social_connect_v2::SessionUpdate,
        transfer_state::TransferState,
        user_attributes::UserAttributesMutation,
    },
    state::{
        context::{ContextType, ResetContext},
        handed_over::HandedOver,
        provider::IsProvider,
        {ConnectConfig, ConnectState},
    },
};
use futures_util::StreamExt;
use librespot_protocol::context_page::ContextPage;
use librespot_protocol::context_track::ContextTrack;
use protobuf::MessageField;
use std::{
    collections::HashSet,
    future::Future,
    sync::Arc,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::{sync::mpsc, time::sleep};

#[derive(Debug, Error)]
enum SpircError {
    #[error("response payload empty")]
    NoData,
    #[error("{0} had no uri")]
    NoUri(&'static str),
    #[error("message pushed for another URI")]
    InvalidUri(String),
    #[error("failed to put connect state for new device")]
    FailedDealerSetup,
    #[error("unknown endpoint: {0:#?}")]
    UnknownEndpoint(serde_json::Value),
}

impl From<SpircError> for Error {
    fn from(err: SpircError) -> Self {
        use SpircError::*;
        match err {
            NoData | NoUri(_) => Error::unavailable(err),
            InvalidUri(_) | FailedDealerSetup => Error::aborted(err),
            UnknownEndpoint(_) => Error::unimplemented(err),
        }
    }
}

struct SpircTask {
    player: Arc<Player>,
    mixer: Arc<dyn Mixer>,

    /// the state management object
    connect_state: ConnectState,
    connect_established: bool,

    play_request_id: Option<u64>,
    play_status: SpircPlayStatus,

    connection_id_update: BoxedStreamResult<String>,
    connect_state_update: BoxedStreamResult<ClusterUpdate>,
    connect_state_volume_update: BoxedStreamResult<(SetVolumeCommand, String)>,
    connect_state_logout_request: BoxedStreamResult<LogoutCommand>,
    playlist_update: BoxedStreamResult<PlaylistModificationInfo>,
    session_update: BoxedStreamResult<FallbackWrapper<SessionUpdate>>,
    connect_state_command: BoxedStream<RequestReply>,
    user_attributes_update: BoxedStreamResult<UserAttributesUpdate>,
    user_attributes_mutation: BoxedStreamResult<UserAttributesMutation>,

    commands: Option<mpsc::UnboundedReceiver<SpircCommand>>,
    player_events: Option<PlayerEventChannel>,

    context_resolver: ContextResolver,

    shutdown: bool,
    session: Session,

    /// is set when transferring, and used after resolving the contexts to finish the transfer
    pub transfer_state: Option<TransferState>,

    /// Spotlight: what the device that has the music is playing, from Spotify's last update
    /// about it. A handover reads the list to carry on with from here. See
    /// `state/handed_over.rs`.
    playing_elsewhere: Option<PlayerState>,
    /// Spotlight: the songs a handover brought, kept while the playlist it names is asked
    /// for, in case that fails.
    handed_over: Option<HandedOver>,

    /// when set to true, it will update the volume after [VOLUME_UPDATE_DELAY],
    /// when no other future resolves, otherwise resets the delay
    update_volume: bool,
    /// Spotlight: the volume waiting to be reported was asked for through Spotify, by another
    /// device or app, not by this one. Said in the app's log once it settles.
    volume_asked_through_spotify: Option<AskedThroughSpotify>,
    /// Spotlight: how many volumes were refused since the log last said so, when it did,
    /// and the last of them in percent.
    volumes_refused: (u32, Option<Instant>, u32),
    /// Spotlight: a volume from elsewhere is only taken when one of Spotlight's apps asked
    /// for it ([`ConnectConfig::only_marked_volume`]).
    only_marked_volume: bool,
    volume_marks: VolumeMarks,

    /// when set to true, it will update the volume after [UPDATE_STATE_DELAY],
    /// when no other future resolves, otherwise resets the delay
    update_state: bool,

    spirc_id: usize,
}

static SPIRC_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
enum SpircCommand {
    Play,
    PlayPause,
    Pause,
    Prev,
    Next,
    VolumeUp,
    VolumeDown,
    Shutdown,
    Shuffle(bool),
    Repeat(bool),
    RepeatTrack(bool),
    Disconnect { pause: bool },
    SetPosition(u32),
    SetVolume(u16),
    SetVolumeFrom {
        disable_volume: bool,
        only_marked_volume: bool,
    },
    Activate,
    Transfer(Option<TransferRequest>),
    Load(LoadRequest),
    SetUpcoming {
        rows_before: Vec<String>,
        rows_after: Vec<String>,
    },
}

const CONTEXT_FETCH_THRESHOLD: usize = 2;

/// What a list of songs plays under, having no playlist or album of its own.
const WEB_API_URI: &str = "spotify:web-api";

/// Spotlight: says what came over with the music, for the app's log.
fn log_carried_on(list: &HandedOver, why: &str) {
    info!(
        target: "spotlight",
        "The music moved here with its list: {} played, {} coming up, {} queued (read from {}){why}",
        list.current.unwrap_or(list.resume_at),
        list.tracks.len().saturating_sub(list.resume_at),
        list.queued.len(),
        list.read_from,
    );
}

/// Spotlight: what a play command asks for: the list it names, or the songs it brought when
/// it names none. A name Spotify can't be asked for, like the "-" a list of songs can come
/// under, also plays the songs brought. Asking for it failed, and nothing started.
/// `playing` is the name of the list this player has now, if it has one and every song
/// brought is in it.
fn named_or_brought(
    named: Option<String>,
    brought: &[ContextTrack],
    playing: Option<&str>,
) -> Option<PlayContext> {
    let asked_for = |uri: &str| {
        HandedOver::can_be_asked_for(uri)
            // A search plays as it did before: its name isn't asked for either, the first
            // song is.
            || uri.starts_with("spotify:search")
            // The list this player already has is played from as it is, as before. Another
            // app picking a song in it names it and may bring only the songs it knows of.
            // Other songs under that name are a new list: made-up names are shared.
            || (!uri.is_empty() && playing == Some(uri))
    };
    match named {
        Some(uri) if brought.is_empty() || asked_for(&uri) => Some(PlayContext::Uri(uri)),
        named if !brought.is_empty() => {
            if let Some(name) = named {
                // Said so the log shows what Spotify calls a list of songs.
                info!(
                    target: "spotlight",
                    "The music came as a list of songs under the name \"{name}\""
                );
            }
            let songs = brought.iter().flat_map(|track| track.uri.clone());
            Some(PlayContext::Tracks(songs.collect()))
        }
        _ => None,
    }
}

/// Spotlight: the volumes asked for through Spotify since the last one was said in the log.
#[derive(Default)]
struct AskedThroughSpotify {
    /// How many came. One is a jump; many in a row are steps, as volume buttons make.
    times: u32,
    /// What came with the last of them, from [`volume_command`].
    came_with: String,
    /// The last of them came with the mark Spotlight's apps send.
    marked: bool,
}

/// Spotlight: a volume as Spotify sends it, in percent.
fn percent_of(volume: u16) -> u32 {
    (u32::from(volume) * 100 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX)
}

/// Spotlight: how one of Spotlight's apps marks a volume it asks another device for, so a
/// player that takes volumes from Spotlight's apps alone ([`ConnectConfig::only_marked_volume`])
/// can tell it from one any other app asked for. Spotify passes on the volume and nothing
/// that says who asked, so the mark is a volume too: the app asks for this one first and
/// for `percent` straight after. It is one percent off, and a player that takes any volume
/// is at it for as long as the second request takes.
pub fn mark_for_volume(percent: u32) -> u32 {
    if percent >= 100 { 99 } else { percent + 1 }
}

/// How long after its mark a volume still counts as marked. The two come a request to
/// Spotify apart, which is meant to be well under a second.
pub const VOLUME_MARK_WINDOW: Duration = Duration::from_secs(3);

/// Spotlight: a volume another device or app asked for.
#[derive(Debug, PartialEq)]
struct VolumeAsk {
    percent: u32,
    came_with: String,
    at: Instant,
}

/// Spotlight: tells the volumes Spotlight's apps ask for from all others, by their mark
/// ([`mark_for_volume`]).
#[derive(Default)]
struct VolumeMarks {
    /// The last ask, not taken: it wasn't marked, and may be the mark of the next.
    unproven: Option<VolumeAsk>,
}

impl VolumeMarks {
    /// Whether a volume asked for at `now` is marked, and the ask before it if that has now
    /// proved to be no mark, so a volume from elsewhere that was refused.
    fn ask(&mut self, percent: u32, came_with: String, now: Instant) -> (bool, Option<VolumeAsk>) {
        let before = self.unproven.take();
        let marked = before.as_ref().is_some_and(|mark| {
            mark.percent == mark_for_volume(percent) && now.duration_since(mark.at) <= VOLUME_MARK_WINDOW
        });
        if marked {
            // The mark is used up, and a marked volume is no mark for the next: a slider
            // drawn slowly down in another app would otherwise mark each step with the last.
            return (true, None);
        }
        self.unproven = Some(VolumeAsk { percent, came_with, at: now });
        (false, before)
    }

    /// Whether an ask is still waiting to prove a mark.
    fn waits(&self) -> bool {
        self.unproven.is_some()
    }

    /// How long the ask that waits still has.
    fn time_left(&self, now: Instant) -> Duration {
        self.unproven
            .as_ref()
            .map_or(Duration::ZERO, |ask| VOLUME_MARK_WINDOW.saturating_sub(now.duration_since(ask.at)))
    }

    /// The ask that waited, whatever time it had left: nothing after it can make it a mark.
    fn forget(&mut self) -> Option<VolumeAsk> {
        self.unproven.take()
    }

    /// The ask that waited, once its time is up: it was no mark.
    fn give_up(&mut self, now: Instant) -> Option<VolumeAsk> {
        self.unproven.take_if(|ask| now.duration_since(ask.at) >= VOLUME_MARK_WINDOW)
    }
}

/// Spotlight: a volume command Spotify passed on, and a line on what came with it. Spotify
/// doesn't say which device or app asked, so this is all there is to tell one asker from
/// another by.
fn volume_command(message: Message) -> Result<(SetVolumeCommand, String), Error> {
    let mut headers: Vec<String> = message
        .headers
        .iter()
        .map(|(name, value)| {
            // The id of this player's connection stays out of the log.
            let lower = name.to_lowercase();
            let secret = ["connection", "token", "authorization"]
                .iter()
                .any(|part| lower.contains(part));
            format!("{name}: {}", if secret { "(hidden)" } else { value.as_str() })
        })
        .collect();
    headers.sort();

    let command: SetVolumeCommand = Message::from_raw(message)?;
    let came_with = format!(
        "connection type {:?}, interaction ids {:?}, page ids {:?}, message id {}, headers {:?}",
        command.connection_type,
        command.logging_params.interaction_ids,
        command.logging_params.page_instance_ids,
        command.command_options.message_id,
        headers
    );
    Ok((command, came_with))
}

/// Spotlight: whether every song a play command brought is in `list`, the songs this player
/// has. Only then does a command naming that list mean "play from it".
fn all_in_list<'a>(brought: &[ContextTrack], list: impl Iterator<Item = &'a str>) -> bool {
    let list: HashSet<&str> = list.collect();
    brought
        .iter()
        .all(|song| song.uri.as_deref().is_some_and(|uri| list.contains(uri)))
}

// delay to update volume after a certain amount of time, instead on each update request
const VOLUME_UPDATE_DELAY: Duration = Duration::from_millis(500);
// to reduce updates to remote, we group some request by waiting for a set amount of time
const UPDATE_STATE_DELAY: Duration = Duration::from_millis(200);
/// Spotlight: the least time between two log lines about refused volumes.
const REFUSED_VOLUME_LOG_GAP: Duration = Duration::from_secs(5);

/// The spotify connect handle
pub struct Spirc {
    commands: mpsc::UnboundedSender<SpircCommand>,
}

impl Spirc {
    /// Initializes a new spotify connect device
    ///
    /// The returned tuple consists out of a handle to the [`Spirc`] that
    /// can control the local connect device when active. And a [`Future`]
    /// which represents the [`Spirc`] event loop that processes the whole
    /// connect device logic.
    pub async fn new(
        config: ConnectConfig,
        session: Session,
        credentials: Credentials,
        player: Arc<Player>,
        mixer: Arc<dyn Mixer>,
    ) -> Result<(Spirc, impl Future<Output = ()>), Error> {
        fn extract_connection_id(msg: Message) -> Result<String, Error> {
            let connection_id = msg
                .headers
                .get("Spotify-Connection-Id")
                .ok_or_else(|| SpircError::InvalidUri(msg.uri.clone()))?;
            Ok(connection_id.to_owned())
        }

        let spirc_id = SPIRC_COUNTER.fetch_add(1, Ordering::AcqRel);
        debug!("new Spirc[{spirc_id}]");

        let only_marked_volume = config.only_marked_volume;
        let connect_state = ConnectState::new(config, &session);

        let connection_id_update = session
            .dealer()
            .listen_for("hm://pusher/v1/connections/", extract_connection_id)?;

        let connect_state_update = session
            .dealer()
            .listen_for("hm://connect-state/v1/cluster", Message::from_raw)?;

        let connect_state_volume_update = session
            .dealer()
            .listen_for("hm://connect-state/v1/connect/volume", volume_command)?;

        let connect_state_logout_request = session
            .dealer()
            .listen_for("hm://connect-state/v1/connect/logout", Message::from_raw)?;

        let playlist_update = session
            .dealer()
            .listen_for("hm://playlist/v2/playlist/", Message::from_raw)?;

        let session_update = session
            .dealer()
            .listen_for("social-connect/v2/session_update", Message::try_from_json)?;

        let user_attributes_update = session
            .dealer()
            .listen_for("spotify:user:attributes:update", Message::from_raw)?;

        // can be trigger by toggling autoplay in a desktop client
        let user_attributes_mutation = session
            .dealer()
            .listen_for("spotify:user:attributes:mutated", Message::from_raw)?;

        let connect_state_command = session
            .dealer()
            .handle_for("hm://connect-state/v1/player/command")?;

        // pre-acquire client_token, preventing multiple request while running
        let _ = session.spclient().client_token().await?;

        // Connect *after* all message listeners are registered
        session.connect(credentials, true).await?;

        // pre-acquire access_token (we need to be authenticated to retrieve a token)
        let _ = session.login5().auth_token().await?;

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let player_events = player.get_player_event_channel();

        let mut task = SpircTask {
            player,
            mixer,

            connect_state,
            connect_established: false,

            play_request_id: None,
            play_status: SpircPlayStatus::Stopped,

            connection_id_update,
            connect_state_update,
            connect_state_volume_update,
            connect_state_logout_request,
            playlist_update,
            session_update,
            connect_state_command,
            user_attributes_update,
            user_attributes_mutation,
            commands: Some(cmd_rx),
            player_events: Some(player_events),

            context_resolver: ContextResolver::new(session.clone()),

            shutdown: false,
            session,

            transfer_state: None,
            playing_elsewhere: None,
            handed_over: None,
            update_volume: false,
            volume_asked_through_spotify: None,
            volumes_refused: (0, None, 0),
            only_marked_volume,
            volume_marks: VolumeMarks::default(),
            update_state: false,

            spirc_id,
        };

        let spirc = Spirc { commands: cmd_tx };

        let initial_volume = task.connect_state.device_info().volume;
        task.connect_state.set_volume(0);

        match initial_volume.try_into() {
            Ok(volume) => {
                task.take_volume(volume);
                // we don't want to update the volume initially,
                // we just want to set the mixer to the correct volume
                task.update_volume = false;
            }
            Err(why) => error!("failed to update initial volume: {why}"),
        };

        Ok((spirc, task.run()))
    }

    /// Safely shutdowns the spirc.
    ///
    /// This pauses the playback, disconnects the connect device and
    /// bring the future initially returned to an end.
    pub fn shutdown(&self) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::Shutdown)?)
    }

    /// Resumes the playback
    ///
    /// Does nothing if we are not the active device, or it isn't paused.
    pub fn play(&self) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::Play)?)
    }

    /// Resumes or pauses the playback
    ///
    /// Does nothing if we are not the active device.
    pub fn play_pause(&self) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::PlayPause)?)
    }

    /// Pauses the playback
    ///
    /// Does nothing if we are not the active device, or if it isn't playing.
    pub fn pause(&self) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::Pause)?)
    }

    /// Seeks to the beginning or skips to the previous track.
    ///
    /// Seeks to the beginning when the current track position
    /// is greater than 3 seconds.
    ///
    /// Does nothing if we are not the active device.
    pub fn prev(&self) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::Prev)?)
    }

    /// Skips to the next track.
    ///
    /// Does nothing if we are not the active device.
    pub fn next(&self) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::Next)?)
    }

    /// Increases the volume by configured steps of [ConnectConfig].
    ///
    /// Does nothing if we are not the active device.
    pub fn volume_up(&self) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::VolumeUp)?)
    }

    /// Decreases the volume by configured steps of [ConnectConfig].
    ///
    /// Does nothing if we are not the active device.
    pub fn volume_down(&self) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::VolumeDown)?)
    }

    /// Shuffles the playback according to the value.
    ///
    /// If true shuffles/reshuffles the playback. Otherwise, does
    /// nothing (if not shuffled) or unshuffles the playback while
    /// resuming at the position of the current track.
    ///
    /// Does nothing if we are not the active device.
    pub fn shuffle(&self, shuffle: bool) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::Shuffle(shuffle))?)
    }

    /// Repeats the playback context according to the value.
    ///
    /// Does nothing if we are not the active device.
    pub fn repeat(&self, repeat: bool) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::Repeat(repeat))?)
    }

    /// Repeats the current track if true.
    ///
    /// Does nothing if we are not the active device.
    ///
    /// Skipping to the next track disables the repeating.
    pub fn repeat_track(&self, repeat: bool) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::RepeatTrack(repeat))?)
    }

    /// Update the volume to the given value.
    ///
    /// Does nothing if we are not the active device.
    pub fn set_volume(&self, volume: u16) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::SetVolume(volume))?)
    }

    /// Spotlight: changes whose volumes the player takes while it runs, as
    /// [`ConnectConfig::disable_volume`] and [`ConnectConfig::only_marked_volume`] set it
    /// when the player is made. Takes effect with the music here or not, and leaves the
    /// music alone.
    pub fn set_volume_from(&self, disable_volume: bool, only_marked_volume: bool) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::SetVolumeFrom {
            disable_volume,
            only_marked_volume,
        })?)
    }

    /// Updates the position to the given value.
    ///
    /// Does nothing if we are not the active device.
    ///
    /// If value is greater than the track duration,
    /// the update is ignored.
    pub fn set_position_ms(&self, position_ms: u32) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::SetPosition(position_ms))?)
    }

    /// Load a new context and replace the current.
    ///
    /// Does nothing if we are not the active device.
    ///
    /// Does not overwrite the queue.
    pub fn load(&self, command: LoadRequest) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::Load(command))?)
    }

    /// Spotlight: puts the songs coming up in a new order without touching the one playing.
    ///
    /// `rows_before` and `rows_after` are the songs the app showed before and after the edit.
    /// Anything else coming up stays behind them. Does nothing unless this device is active.
    pub fn set_upcoming(
        &self,
        rows_before: Vec<String>,
        rows_after: Vec<String>,
    ) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::SetUpcoming {
            rows_before,
            rows_after,
        })?)
    }

    /// Disconnects the current device and pauses the playback according the value.
    ///
    /// Does nothing if we are not the active device.
    pub fn disconnect(&self, pause: bool) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::Disconnect { pause })?)
    }

    /// Acquires the control as active connect device.
    ///
    /// Does not [Spirc::transfer] the playback. Does nothing if we are not the active device.
    pub fn activate(&self) -> Result<(), Error> {
        Ok(self.commands.send(SpircCommand::Activate)?)
    }

    /// Acquires the control as active connect device over the transfer flow.
    ///
    /// Does nothing if we are not the active device.
    pub fn transfer(&self, transfer_request: Option<TransferRequest>) -> Result<(), Error> {
        Ok(self
            .commands
            .send(SpircCommand::Transfer(transfer_request))?)
    }
}

impl SpircTask {
    async fn run(mut self) {
        // simplify unwrapping of received item or parsed result
        macro_rules! unwrap {
            ( $next:expr, |$some:ident| $use_some:expr ) => {
                match $next {
                    Some($some) => $use_some,
                    None => {
                        error!("{} selected, but none received", stringify!($next));
                        break;
                    }
                }
            };
            ( $next:expr, match |$ok:ident| $use_ok:expr ) => {
                unwrap!($next, |$ok| match $ok {
                    Ok($ok) => $use_ok,
                    Err(why) => error!("could not parse {}: {}", stringify!($ok), why),
                })
            };
        }

        if let Err(why) = self.session.dealer().start().await {
            error!("starting dealer failed: {why}");
            return;
        }

        while !self.session.is_invalid() && !self.shutdown {
            let commands = self.commands.as_mut();
            let player_events = self.player_events.as_mut();

            // when state and volume update have a higher priority than context resolving
            // because of that the context resolving has to wait, so that the other tasks can finish
            let allow_context_resolving = !self.update_state && !self.update_volume;

            tokio::select! {
                // startup of the dealer requires a connection_id, which is retrieved at the very beginning
                connection_id_update = self.connection_id_update.next() => unwrap! {
                    connection_id_update,
                    match |connection_id| if let Err(why) = self.handle_connection_id_update(connection_id).await {
                        error!("failed handling connection id update: {why}");
                        break;
                    }
                },
                // main dealer update of any remote device updates
                cluster_update = self.connect_state_update.next() => unwrap! {
                    cluster_update,
                    match |cluster_update| if let Err(e) = self.handle_cluster_update(cluster_update).await {
                        error!("could not dispatch connect state update: {e}");
                    }
                },
                // main dealer request handling (dealer expects an answer)
                request = self.connect_state_command.next() => unwrap! {
                    request,
                    |request| if let Err(e) = self.handle_connect_state_request(request).await {
                        error!("couldn't handle connect state command: {e}");
                    }
                },
                // volume request handling is send separately (it's more like a fire forget)
                volume_update = self.connect_state_volume_update.next() => unwrap! {
                    volume_update,
                    match |volume_update| match volume_update.0.volume.try_into() {
                        Ok(volume) => {
                            let came_with = volume_update.1;
                            let percent = percent_of(volume);
                            if self.connect_state.device_info().capabilities.disable_volume {
                                // Spotlight: this player said its volume isn't others' to set
                                // (`ConnectConfig::disable_volume`). Spotify's own apps then
                                // don't ask, but nothing stops a command that comes anyway.
                                self.refuse_volume(percent, &came_with);
                            } else if self.only_marked_volume {
                                // Spotlight: only a volume one of Spotlight's apps asked for is
                                // taken. An ask that isn't marked may be the mark of the next.
                                let (marked, not_a_mark) = self.volume_marks.ask(percent, came_with.clone(), Instant::now());
                                if let Some(refused) = not_a_mark {
                                    self.refuse_volume(refused.percent, &refused.came_with);
                                }
                                if marked {
                                    self.take_volume_from_elsewhere(volume, came_with, true);
                                }
                            } else {
                                self.take_volume_from_elsewhere(volume, came_with, false);
                            }
                        }
                        Err(why) => error!("can't update volume, failed to parse i32 to u16: {why}")
                    }
                },
                logout_request = self.connect_state_logout_request.next() => unwrap! {
                    logout_request,
                    |logout_request| {
                        error!("received logout request, currently not supported: {logout_request:#?}");
                        // todo: call logout handling
                    }
                },
                playlist_update = self.playlist_update.next() => unwrap! {
                    playlist_update,
                    match |playlist_update| if let Err(why) = self.handle_playlist_modification(playlist_update) {
                        error!("failed to handle playlist modification: {why}")
                    }
                },
                user_attributes_update = self.user_attributes_update.next() => unwrap! {
                    user_attributes_update,
                    match |attributes| self.handle_user_attributes_update(attributes)
                },
                user_attributes_mutation = self.user_attributes_mutation.next() => unwrap! {
                    user_attributes_mutation,
                    match |attributes| self.handle_user_attributes_mutation(attributes)
                },
                session_update = self.session_update.next() => unwrap! {
                    session_update,
                    match |session_update| self.handle_session_update(session_update)
                },
                cmd = async { commands?.recv().await }, if commands.is_some() && self.connect_established => match cmd {
                    Some(cmd) => if let Err(e) = self.handle_command(cmd).await {
                        debug!("could not dispatch command: {e}");
                    },
                    // Spotlight: no one can command this any more. Shutting down beats coming
                    // straight back here forever, and a device no one controls playing on.
                    None => if let Err(e) = self.handle_command(SpircCommand::Shutdown).await {
                        debug!("could not shut down: {e}");
                    },
                },
                event = async { player_events?.recv().await }, if player_events.is_some() => if let Some(event) = event {
                    if let Err(e) = self.handle_player_event(event) {
                        error!("could not dispatch player event: {e}");
                    }
                },
                _ = async { sleep(UPDATE_STATE_DELAY).await }, if self.update_state => {
                    self.update_state = false;

                    if let Err(why) = self.notify().await {
                        error!("state update: {why}")
                    }
                },
                // Spotlight: an ask that could have been the mark of the next one, and no next
                // one came in time. It was a volume from elsewhere, and is said as refused.
                _ = async { sleep(self.volume_marks.time_left(Instant::now())).await }, if self.volume_marks.waits() => {
                    if let Some(refused) = self.volume_marks.give_up(Instant::now()) {
                        self.refuse_volume(refused.percent, &refused.came_with);
                    }
                },
                // Spotlight: the volumes refused after the last line about one, said once no
                // more have come for a while, so their count isn't left unsaid.
                _ = async { sleep(REFUSED_VOLUME_LOG_GAP).await }, if self.volumes_refused.0 > 0 => {
                    info!(
                        target: "spotlight",
                        "Refused {} more volume(s) another device or app asked for, the last of {}%",
                        self.volumes_refused.0,
                        self.volumes_refused.2
                    );
                    self.volumes_refused = (0, Some(Instant::now()), 0);
                },
                _ = async { sleep(VOLUME_UPDATE_DELAY).await }, if self.update_volume => {
                    self.update_volume = false;
                    if let Some(asked) = self.volume_asked_through_spotify.take() {
                        info!(
                            target: "spotlight",
                            "{} set this player's volume to {}%. It asked {} time(s), the last with {}",
                            // By the mark alone: Spotify doesn't say who asked.
                            if asked.marked { "Spotlight on another device, going by its mark," } else { "Another device or app" },
                            (self.connect_state.device_info().volume * 100 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX),
                            asked.times,
                            asked.came_with
                        );
                    }

                    info!("delayed volume update for all devices: volume is now {}", self.connect_state.device_info().volume);
                    if let Err(why) = self.connect_state.notify_volume_changed(&self.session).await {
                        error!("error updating connect state for volume update: {why}")
                    }

                    // for some reason the web-player does need two separate updates, so that the
                    // position of the current track is retained, other clients also send a state
                    // update before they send the volume update
                    if let Err(why) = self.notify().await {
                        error!("error updating connect state for volume update: {why}")
                    }
                },
                // context resolver handling, the idea/reason behind it the following:
                //
                // when we request a context that has multiple pages (for example an artist)
                // resolving all pages at once can take around ~1-30sec, when we resolve
                // everything at once that would block our main loop for that time
                //
                // to circumvent this behavior, we request each context separately here and
                // finish after we received our last item of a type
                next_context = async {
                    self.context_resolver.get_next_context(|| {
                        // Sending local file URIs to this endpoint results in a Bad Request status.
                        // It's likely appropriate to filter them out anyway; Spotify's backend
                        // has no knowledge about these tracks and so can't do anything with them.
                        self.connect_state.recent_track_uris()
                            .into_iter()
                            .filter(|t| !t.starts_with("spotify:local"))
                            .collect::<Vec<_>>()
                    }).await
                }, if allow_context_resolving && self.context_resolver.has_next() => {
                    let update_state = self.handle_next_context(next_context);
                    if update_state {
                        if let Err(why) = self.notify().await {
                            error!("update after context resolving failed: {why}")
                        }
                    }
                },
                else => break
            }
        }

        if !self.shutdown && self.connect_state.is_active() {
            warn!("unexpected shutdown");
            if let Err(why) = self.handle_disconnect().await {
                error!("error during disconnecting: {why}")
            }
        }

        // this should clear the active session id, leaving an empty state
        if let Err(why) = self.session.spclient().delete_connect_state_request().await {
            error!("error during connect state deletion: {why}")
        };

        self.session.dealer().close().await;
    }

    fn handle_next_context(&mut self, next_context: Result<Context, Error>) -> bool {
        // Spotlight: whether a handover waits on this one, the playlist it named.
        let for_handover =
            self.transfer_state.is_some() && self.context_resolver.next_is_the_list_itself();

        let next_context = match next_context {
            Err(why) => {
                self.context_resolver.mark_next_unavailable();
                self.context_resolver.remove_used_and_invalid();
                error!("{why}");
                // Spotlight: a handover is finished by the last answer about its playlist. When
                // that is a later page, or the similar songs after it, and it fails, this was
                // the last answer and nothing else would finish it.
                let nothing_more_comes =
                    self.transfer_state.is_some() && !self.context_resolver.has_next();
                return (for_handover || nothing_more_comes) && self.carry_on_with_what_came();
            }
            Ok(ctx) => ctx,
        };

        debug!("handling next context {:?}", next_context.uri);

        match self
            .context_resolver
            .apply_next_context(&mut self.connect_state, next_context)
        {
            Ok(remaining) => {
                if let Some(remaining) = remaining {
                    self.context_resolver.add_list(remaining)
                }
            }
            Err(why) => {
                error!("{why}");
                // Spotlight: a playlist that came back unusable, with no songs in it say.
                if for_handover {
                    self.context_resolver.remove_used_and_invalid();
                    return self.carry_on_with_what_came();
                }
            }
        }

        let update_state = if self
            .context_resolver
            .try_finish(&mut self.connect_state, &mut self.transfer_state)
        {
            self.add_autoplay_resolving_when_required();
            true
        } else {
            false
        };

        self.context_resolver.remove_used_and_invalid();
        // Spotlight: the handover is done, so its songs aren't needed as a fallback.
        if self.transfer_state.is_none() {
            self.handed_over = None;
        }
        update_state
    }

    // todo: is the time_delta still necessary?
    fn now_ms(&self) -> i64 {
        let dur = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|err| err.duration());

        dur.as_millis() as i64 + 1000 * self.session.time_delta()
    }

    async fn handle_command(&mut self, cmd: SpircCommand) -> Result<(), Error> {
        trace!("Received SpircCommand::{cmd:?}");
        match cmd {
            SpircCommand::Shutdown => {
                trace!("Received SpircCommand::Shutdown");
                self.handle_pause();
                // Spotlight: shuts down even when Spotify can't be told, as when the network is
                // gone. Before, a failed disconnect left this running with no one to command it.
                let disconnected = self.handle_disconnect().await;
                self.shutdown = true;
                if let Some(rx) = self.commands.as_mut() {
                    rx.close()
                }
                disconnected?;
            }
            SpircCommand::Transfer(request) if !self.connect_state.is_active() => {
                let device_id = self.session.device_id();
                self.session
                    .spclient()
                    .transfer(device_id, device_id, request.as_ref())
                    .await?;
                return Ok(());
            }
            SpircCommand::Activate if !self.connect_state.is_active() => {
                trace!("Received SpircCommand::{cmd:?}");
                self.handle_activate();
                return self.notify().await;
            }
            SpircCommand::Transfer(..) | SpircCommand::Activate => {
                warn!("SpircCommand::{cmd:?} will be ignored while already active")
            }
            // Spotlight: the volume is this device's own, with the music here or not, as it
            // is for one Spotify passes on. Before, one chosen here while the music was
            // elsewhere or nowhere was dropped, and the old one came back with the music.
            SpircCommand::SetVolume(volume) if !self.connect_state.is_active() => {
                self.set_volume_chosen_here(volume);
                return Ok(());
            }
            // Spotlight: whose volumes are taken is the device's own business as well.
            SpircCommand::SetVolumeFrom {
                disable_volume,
                only_marked_volume,
            } => {
                self.take_volumes_from(disable_volume, only_marked_volume).await;
                return Ok(());
            }
            _ if !self.connect_state.is_active() => {
                warn!("SpircCommand::{cmd:?} will be ignored while Not Active")
            }
            SpircCommand::Disconnect { pause } => {
                if pause {
                    self.handle_pause()
                }
                return self.handle_disconnect().await;
            }
            SpircCommand::Play => self.handle_play(),
            SpircCommand::PlayPause => self.handle_play_pause(),
            SpircCommand::Pause => self.handle_pause(),
            SpircCommand::Prev => self.handle_prev()?,
            SpircCommand::Next => self.handle_next(None)?,
            SpircCommand::VolumeUp => self.handle_volume_up(),
            SpircCommand::VolumeDown => self.handle_volume_down(),
            SpircCommand::Shuffle(shuffle) => self.handle_shuffle(shuffle)?,
            SpircCommand::Repeat(repeat) => self.handle_repeat_context(repeat)?,
            SpircCommand::RepeatTrack(repeat) => self.handle_repeat_track(repeat),
            SpircCommand::SetPosition(position) => self.handle_seek(position),
            SpircCommand::SetVolume(volume) => self.set_volume_chosen_here(volume),
            SpircCommand::Load(command) => {
                self.handle_load(command, None, None, Vec::new()).await?
            }
            SpircCommand::SetUpcoming {
                rows_before,
                rows_after,
            } => {
                self.connect_state
                    .set_upcoming(&rows_before, &rows_after);
                // The old next song may already be buffering. This swaps it, so the change
                // of song stays gapless even when the list changes near the end of one.
                let preloading = matches!(
                    self.play_status,
                    SpircPlayStatus::Playing {
                        preloading_of_next_track_triggered: true,
                        ..
                    } | SpircPlayStatus::Paused {
                        preloading_of_next_track_triggered: true,
                        ..
                    }
                );
                if preloading {
                    self.handle_preload_next_track();
                }
                // Told to Spotify with the next batch, so a run of quick edits is one update.
                self.update_state = true;
                return Ok(());
            }
        };

        self.notify().await
    }

    fn handle_player_event(&mut self, event: PlayerEvent) -> Result<(), Error> {
        if let PlayerEvent::TrackChanged { audio_item } = event {
            self.connect_state.update_duration(audio_item.duration_ms);
            self.update_state = true;
            return Ok(());
        }

        // update play_request_id
        if let PlayerEvent::PlayRequestIdChanged { play_request_id } = event {
            self.play_request_id = Some(play_request_id);
            return Ok(());
        }

        let is_current_track = matches! {
            (event.get_play_request_id(), self.play_request_id),
            (Some(event_id), Some(current_id)) if event_id == current_id
        };

        // we only process events if the play_request_id matches. If it doesn't, it is
        // an event that belongs to a previous track and only arrives now due to a race
        // condition. In this case we have updated the state already and don't want to
        // mess with it.
        if !is_current_track {
            return Ok(());
        }

        match event {
            PlayerEvent::EndOfTrack { .. } => {
                let next_track = self
                    .connect_state
                    .repeat_track()
                    .then(|| self.connect_state.current_track(|t| t.uri.clone()));

                self.handle_next(next_track)?
            }
            PlayerEvent::Loading { .. } => match self.play_status {
                SpircPlayStatus::LoadingPlay { position_ms } => {
                    self.connect_state
                        .update_position(position_ms, self.now_ms());
                    trace!("==> LoadingPlay");
                }
                SpircPlayStatus::LoadingPause { position_ms } => {
                    self.connect_state
                        .update_position(position_ms, self.now_ms());
                    trace!("==> LoadingPause");
                }
                _ => {
                    self.connect_state.update_position(0, self.now_ms());
                    trace!("==> Loading");
                }
            },
            PlayerEvent::Seeked { position_ms, .. } => {
                trace!("==> Seeked");
                self.connect_state
                    .update_position(position_ms, self.now_ms())
            }
            PlayerEvent::Playing { position_ms, .. }
            | PlayerEvent::PositionCorrection { position_ms, .. } => {
                trace!("==> Playing");
                let new_nominal_start_time = self.now_ms() - position_ms as i64;
                match self.play_status {
                    SpircPlayStatus::Playing {
                        ref mut nominal_start_time,
                        ..
                    } => {
                        if (*nominal_start_time - new_nominal_start_time).abs() > 100 {
                            *nominal_start_time = new_nominal_start_time;
                            self.connect_state
                                .update_position(position_ms, self.now_ms());
                        } else {
                            return Ok(());
                        }
                    }
                    SpircPlayStatus::LoadingPlay { .. } | SpircPlayStatus::LoadingPause { .. } => {
                        self.connect_state
                            .update_position(position_ms, self.now_ms());
                        self.play_status = SpircPlayStatus::Playing {
                            nominal_start_time: new_nominal_start_time,
                            preloading_of_next_track_triggered: false,
                        };
                    }
                    _ => return Ok(()),
                }
            }
            PlayerEvent::Paused {
                position_ms: new_position_ms,
                ..
            } => {
                trace!("==> Paused");
                match self.play_status {
                    SpircPlayStatus::Paused { .. } | SpircPlayStatus::Playing { .. } => {
                        self.connect_state
                            .update_position(new_position_ms, self.now_ms());
                        self.play_status = SpircPlayStatus::Paused {
                            position_ms: new_position_ms,
                            preloading_of_next_track_triggered: false,
                        };
                    }
                    SpircPlayStatus::LoadingPlay { .. } | SpircPlayStatus::LoadingPause { .. } => {
                        self.connect_state
                            .update_position(new_position_ms, self.now_ms());
                        self.play_status = SpircPlayStatus::Paused {
                            position_ms: new_position_ms,
                            preloading_of_next_track_triggered: false,
                        };
                    }
                    _ => return Ok(()),
                }
            }
            PlayerEvent::Stopped { .. } => {
                trace!("==> Stopped");
                match self.play_status {
                    SpircPlayStatus::Stopped => return Ok(()),
                    _ => self.play_status = SpircPlayStatus::Stopped,
                }
            }
            PlayerEvent::TimeToPreloadNextTrack { .. } => {
                self.handle_preload_next_track();
                return Ok(());
            }
            PlayerEvent::Unavailable { track_id, .. } => {
                self.handle_unavailable(&track_id)?;
                if self.connect_state.current_track(|t| &t.uri) == &track_id.to_uri()? {
                    self.handle_next(None)?
                }
            }
            _ => return Ok(()),
        }

        self.update_state = true;
        Ok(())
    }

    async fn handle_connection_id_update(&mut self, connection_id: String) -> Result<(), Error> {
        trace!("Received connection ID update: {connection_id:?}");
        self.session.set_connection_id(&connection_id);

        let mut cluster = match self
            .connect_state
            .notify_new_device_appeared(&self.session)
            .await
        {
            Ok(res) => Cluster::parse_from_bytes(&res).ok(),
            Err(why) => {
                error!("{why:?}");
                None
            }
        }
        .ok_or(SpircError::FailedDealerSetup)?;

        debug!(
            "successfully put connect state for {} with connection-id {connection_id}",
            self.session.device_id()
        );

        self.connect_established = true;

        let same_session = cluster.player_state.session_id == self.session.session_id()
            || cluster.player_state.session_id.is_empty();
        if !cluster.active_device_id.is_empty() || !same_session {
            info!(
                "active device is <{}> with session <{}>",
                cluster.active_device_id, cluster.player_state.session_id
            );
            // Spotlight: what it plays is kept from the start, for a handover that comes
            // before its next update does.
            if cluster.active_device_id != self.session.device_id() {
                self.playing_elsewhere = cluster.player_state.take();
            }
            return Ok(());
        } else if cluster.transfer_data.is_empty() {
            debug!("got empty transfer state, do nothing");
            return Ok(());
        } else {
            info!(
                "trying to take over control automatically, session_id: {}",
                cluster.player_state.session_id
            )
        }

        use protobuf::Message;

        // Spotlight: the session being picked up, as its last device reported it.
        self.playing_elsewhere = cluster.player_state.take();

        match TransferState::parse_from_bytes(&cluster.transfer_data) {
            Ok(transfer_state) => self.handle_transfer(transfer_state)?,
            Err(why) => error!("failed to take over control: {why}"),
        }

        Ok(())
    }

    fn handle_user_attributes_update(&mut self, update: UserAttributesUpdate) {
        trace!("Received attributes update: {update:#?}");
        let attributes: UserAttributes = update
            .pairs
            .iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect();
        self.session.set_user_attributes(attributes)
    }

    fn handle_user_attributes_mutation(&mut self, mutation: UserAttributesMutation) {
        for attribute in mutation.fields.iter() {
            let key = &attribute.name;

            if key == "autoplay" && self.session.config().autoplay.is_some() {
                trace!("Autoplay override active. Ignoring mutation.");
                continue;
            }

            if let Some(old_value) = self.session.user_data().attributes.get(key) {
                let new_value = match old_value.as_ref() {
                    "0" => "1",
                    "1" => "0",
                    _ => old_value,
                };
                self.session.set_user_attribute(key, new_value);

                trace!("Received attribute mutation, {key} was {old_value} is now {new_value}");

                if key == "filter-explicit-content" && new_value == "1" {
                    self.player
                        .emit_filter_explicit_content_changed_event(matches!(new_value, "1"));
                }

                if key == "autoplay" && old_value != new_value {
                    self.player
                        .emit_auto_play_changed_event(matches!(new_value, "1"));

                    self.add_autoplay_resolving_when_required()
                }
            } else {
                trace!("Received attribute mutation for {key} but key was not found!");
            }
        }
    }

    async fn handle_cluster_update(
        &mut self,
        mut cluster_update: ClusterUpdate,
    ) -> Result<(), Error> {
        let reason = cluster_update.update_reason.enum_value();

        let device_ids = cluster_update.devices_that_changed.join(", ");
        debug!(
            "cluster update: {reason:?} from {device_ids}, active device: {}",
            cluster_update.cluster.active_device_id
        );

        if let Some(mut cluster) = cluster_update.cluster.take() {
            // Spotlight: what another device plays is kept, to carry on from if the music
            // moves here. While this device has the music the update is about itself.
            if cluster.active_device_id != self.session.device_id() {
                self.playing_elsewhere = cluster.player_state.take();
            }
            let became_inactive = self.connect_state.is_active()
                && cluster.active_device_id != self.session.device_id();
            if became_inactive {
                info!("device became inactive");
                // Spotlight: stops playing even when telling Spotify fails. Another device has
                // the music now, and two shouldn't be heard at once.
                let disconnected = self.handle_disconnect().await;
                self.handle_stop();
                disconnected?;
            } else if self.connect_state.is_active() {
                // fixme: workaround fix, because of missing information why it behaves like it does
                //  background: when another device sends a connect-state update, some player's position de-syncs
                //  tried: providing session_id, playback_id, track-metadata "track_player"
                self.update_state = true;
            }
        } else if self.connect_state.is_active() {
            // Spotlight: says so here too, as `handle_disconnect` does, and whether or not
            // Spotify could be told. The app sends its commands straight to this player only
            // while it's the active device.
            let told = self.connect_state.became_inactive(&self.session).await;
            self.player.emit_session_disconnected_event(
                self.session.connection_id(),
                self.session.username(),
            );
            told?;
        }

        Ok(())
    }

    async fn handle_connect_state_request(
        &mut self,
        (request, sender): RequestReply,
    ) -> Result<(), Error> {
        self.connect_state.set_last_command(request.clone());

        debug!(
            "handling: '{}' from {}",
            request.command, request.sent_by_device_id
        );

        let response = match self.handle_request(request).await {
            Ok(_) => Reply::Success,
            Err(why) => {
                error!("failed to handle request: {why}");
                Reply::Failure
            }
        };

        sender.send(response).map_err(Into::into)
    }

    async fn handle_request(&mut self, request: Request) -> Result<(), Error> {
        use Command::*;

        match request.command {
            // errors and unknown commands
            Transfer(transfer) if transfer.data.is_none() => {
                warn!("transfer endpoint didn't contain any data to transfer");
                Err(SpircError::NoData)?
            }
            Unknown(unknown) => Err(SpircError::UnknownEndpoint(unknown))?,
            // implicit update of the connect_state
            UpdateContext(update_context) => {
                if matches!(update_context.context.uri, Some(ref uri) if uri != self.connect_state.context_uri())
                {
                    debug!(
                        "ignoring context update for <{:?}>, because it isn't the current context <{}>",
                        update_context.context.uri,
                        self.connect_state.context_uri()
                    )
                } else {
                    self.context_resolver.add(ResolveContext::from_context(
                        update_context.context,
                        ContextType::Default,
                        ContextAction::Replace,
                    ))
                }
                return Ok(());
            }
            // modification and update of the connect_state
            Transfer(transfer) => {
                self.handle_transfer(transfer.data.expect("by condition checked"))?;
                return self.notify().await;
            }
            Play(mut play) => {
                if !self.connect_state.is_active() {
                    self.handle_activate()
                }

                // Spotlight: the songs the command brought along, as they came: each with the
                // id Spotify knows it by, which tells two of the same song apart.
                let brought: Vec<ContextTrack> = play
                    .context
                    .pages
                    .iter()
                    .flat_map(|page| page.tracks.iter().cloned())
                    .collect();
                let playing = self
                    .connect_state
                    .get_context(ContextType::Default)
                    .is_ok_and(|list| {
                        all_in_list(&brought, list.tracks.iter().map(|song| song.uri.as_str()))
                    })
                    .then(|| self.connect_state.context_uri().clone());

                let context =
                    named_or_brought(play.context.uri.take(), &brought, playing.as_deref())
                        .ok_or(SpircError::NoUri("context"))?;

                let context_options = play
                    .options
                    .player_options_override
                    .map(Into::into)
                    .map(LoadContextOptions::Options);

                let fallback_index = play
                    .options
                    .skip_to
                    .as_ref()
                    .and_then(|s| s.track_index)
                    .map(|i| i as usize);

                self.handle_load(
                    LoadRequest {
                        context,
                        options: LoadRequestOptions {
                            start_playing: true,
                            seek_to: play.options.seek_to.unwrap_or_default(),
                            playing_track: play.options.skip_to.and_then(|s| s.try_into().ok()),
                            context_options,
                        },
                    },
                    play.context.pages.pop(),
                    fallback_index,
                    brought,
                )
                .await?;

                self.connect_state.set_origin(play.play_origin)
            }
            Pause(_) => self.handle_pause(),
            SeekTo(seek_to) => {
                // for some reason the position is stored in value, not in position
                trace!("seek to {seek_to:?}");
                self.handle_seek(seek_to.value)
            }
            SetShufflingContext(shuffle) => self.handle_shuffle(shuffle.value)?,
            SetRepeatingContext(repeat_context) => {
                self.handle_repeat_context(repeat_context.value)?
            }
            SetRepeatingTrack(repeat_track) => self.handle_repeat_track(repeat_track.value),
            AddToQueue(add_to_queue) => self.connect_state.add_to_queue(add_to_queue.track, true),
            SetQueue(set_queue) => self.connect_state.handle_set_queue(set_queue),
            SetOptions(set_options) => {
                if let Some(repeat_context) = set_options.repeating_context {
                    self.handle_repeat_context(repeat_context)?
                }

                if let Some(repeat_track) = set_options.repeating_track {
                    self.handle_repeat_track(repeat_track)
                }

                let shuffle = set_options.shuffling_context;
                if let Some(shuffle) = shuffle {
                    self.handle_shuffle(shuffle)?;
                }
            }
            SkipNext(skip_next) => self.handle_next(skip_next.track.map(|t| t.uri))?,
            SkipPrev(_) => self.handle_prev()?,
            Resume(_) if matches!(self.play_status, SpircPlayStatus::Stopped) => {
                self.load_track(true, 0)?
            }
            Resume(_) => self.handle_play(),
        }

        self.update_state = true;
        Ok(())
    }

    fn handle_transfer(&mut self, mut transfer: TransferState) -> Result<(), Error> {
        let mut ctx_uri = match transfer.current_session.context.uri {
            None => Err(SpircError::NoUri("transfer context"))?,
            // can apparently happen when a state is transferred and was started with "uris" via the api
            // Spotlight: the same goes for every name that isn't a list Spotify can be asked
            // for, "spotify:web-api" above all: what a list of songs started on a librespot
            // device plays under, and so most of what Spotlight plays. Asking for it failed,
            // and the songs coming up were gone after the switch.
            Some(ref uri) if !HandedOver::can_be_asked_for(uri) => None,
            Some(ref uri) => Some(uri.clone()),
        };

        // Spotlight: a handover still waiting on its playlist is replaced by this one, and
        // that playlist is no longer waited for: arriving late, it would replace this music.
        if self.transfer_state.take().is_some() {
            self.context_resolver.clear();
        }
        self.handed_over = None;
        // The music can also be handed to the device that has it, by picking it again. What
        // it plays is then what this player has, and the handover holds less of it.
        // Anything kept of another device is from before the music came here.
        let elsewhere = match self.playing_elsewhere.take() {
            _ if self.connect_state.is_active() => Some(self.connect_state.player().clone()),
            elsewhere => elsewhere,
        };

        self.connect_state.reset_context(
            ctx_uri
                .as_deref()
                .map(ResetContext::WhenDifferent)
                .unwrap_or(ResetContext::Completely),
        );

        match self.connect_state.current_track_from_transfer(&transfer) {
            Err(why) => warn!("didn't find initial track: {why}"),
            Ok(track) => {
                debug!("found initial track <{}>", track.uri);
                self.connect_state.set_track(track)
            }
        };

        let autoplay = self.connect_state.current_track(|t| t.is_autoplay());
        if autoplay {
            ctx_uri = ctx_uri.map(|c| c.replace("station:", ""));
        }

        let fallback = self.connect_state.current_track(|t| &t.uri).clone();

        // Spotlight: the songs that came over, read before the handover is taken apart below.
        let mut list = HandedOver::of(&transfer, elsewhere.as_ref(), &fallback);

        // Spotlight: a playlist Spotify couldn't give before isn't asked for again, so
        // waiting on it would never end. The songs that came over are used at once.
        let playlist = ctx_uri.as_ref().map(|uri| {
            ResolveContext::from_uri(
                uri.clone(),
                &fallback,
                ContextType::Default,
                ContextAction::Replace,
            )
        });
        if list.is_some() && playlist.as_ref().is_some_and(|playlist| self.context_resolver.gave_up_on(playlist)) {
            ctx_uri = None;
        }
        let load_from_context_uri = ctx_uri.is_some();

        match (ctx_uri.is_some(), playlist) {
            (true, Some(playlist)) => {
                self.context_resolver.add(playlist);
                // Spotlight: kept in case the playlist can't be had.
                self.handed_over = list.take();
            }
            // Spotlight: this used the handover's pages as they came, and so lost the songs
            // already played and any queued ones.
            _ => match list.as_ref() {
                Some(list) => self.load_context_from_tracks(list.tracks.clone())?,
                None => {
                    warn!(
                        "tried to transfer with an invalid state, using fallback as ctx_uri ({fallback})"
                    );
                    ctx_uri = Some(fallback.clone())
                }
            },
        };

        self.handle_activate();

        let timestamp = self.now_ms();
        let state = &mut self.connect_state;
        // Spotlight: a list that was handed over goes by the name a list started here has.
        let named = match list {
            Some(_) => Some(WEB_API_URI.to_string()),
            None => ctx_uri.clone(),
        };
        state.handle_initial_transfer(&mut transfer, named);

        // adjust active context, so resolve knows for which context it should set up the state
        state.active_context = if autoplay {
            ContextType::Autoplay
        } else {
            ContextType::Default
        };

        // update position if the track continued playing
        let transfer_timestamp = transfer.playback.timestamp.unwrap_or_default();
        let position = match transfer.playback.position_as_of_timestamp {
            Some(position) if transfer.playback.is_paused.unwrap_or_default() => position.into(),
            // update position if the track continued playing
            Some(position) if position > 0 => {
                let time_since_position_update = timestamp - transfer_timestamp;
                i64::from(position) + time_since_position_update
            }
            _ => 0,
        };

        let is_playing = !transfer.playback.is_paused();

        if self.connect_state.current_track(|t| t.is_autoplay()) || autoplay {
            if let Some(ctx_uri) = ctx_uri {
                debug!("currently in autoplay context, async resolving autoplay for {ctx_uri}");
                self.context_resolver.add(ResolveContext::from_uri(
                    ctx_uri,
                    fallback,
                    ContextType::Autoplay,
                    ContextAction::Replace,
                ))
            } else {
                warn!("couldn't resolve autoplay context without a context uri");
            }
        }

        // Spotlight: the list that came over is carried on from. If that fails the song still
        // plays, set up below as it was before.
        let carried_on = !load_from_context_uri
            && list.as_ref().is_some_and(|list| {
                match self.connect_state.carry_on_from(list) {
                    Ok(()) => {
                        log_carried_on(list, "");
                        true
                    }
                    Err(why) => {
                        warn!(target: "spotlight", "The songs coming up didn't come over: {why}");
                        false
                    }
                }
            });

        if load_from_context_uri {
            self.transfer_state = Some(transfer);
        } else if !carried_on {
            match self.connect_state.get_context(ContextType::Default) {
                Err(why) => {
                    warn!("continuing transfer in an unknown state. {why}");
                    self.transfer_state = Some(transfer);
                }
                Ok(ctx) => {
                    let idx = ConnectState::find_index_in_context(ctx, |pt| {
                        self.connect_state.current_track(|t| pt.uri == t.uri)
                    })?;
                    self.connect_state.reset_playback_to_position(Some(idx))?;
                }
            }
        }

        self.load_track(is_playing, position.try_into()?)
    }

    async fn handle_disconnect(&mut self) -> Result<(), Error> {
        self.context_resolver.clear();
        // Spotlight: a handover still waiting on its playlist ends with the music leaving.
        self.transfer_state = None;
        self.handed_over = None;

        self.play_status = SpircPlayStatus::Stopped {};
        self.connect_state
            .update_position_in_relation(self.now_ms());
        self.notify().await?;

        // Spotlight: the state is inactive from here on, even when telling Spotify fails, so
        // the event goes out either way. Before, a failed request left the app believing
        // this player still took commands.
        let told = self.connect_state.became_inactive(&self.session).await;

        self.player
            .emit_session_disconnected_event(self.session.connection_id(), self.session.username());

        told?;
        Ok(())
    }

    fn handle_stop(&mut self) {
        self.player.stop();
        self.connect_state.update_position(0, self.now_ms());
        self.connect_state.clear_next_tracks();

        if let Err(why) = self.connect_state.reset_playback_to_position(None) {
            warn!("failed filling up next_track during stopping: {why}")
        }
    }

    fn handle_activate(&mut self) {
        self.connect_state.set_active(true);
        self.player
            .emit_session_connected_event(self.session.connection_id(), self.session.username());
        self.player.emit_session_client_changed_event(
            self.session.client_id(),
            self.session.client_name(),
            self.session.client_brand_name(),
            self.session.client_model_name(),
        );

        self.player
            .emit_volume_changed_event(self.connect_state.device_info().volume as u16);

        self.player
            .emit_auto_play_changed_event(self.session.autoplay());

        self.player
            .emit_filter_explicit_content_changed_event(self.session.filter_explicit_content());

        self.player
            .emit_shuffle_changed_event(self.connect_state.shuffling_context());

        self.player.emit_repeat_changed_event(
            self.connect_state.repeat_context(),
            self.connect_state.repeat_track(),
        );
    }

    async fn handle_load(
        &mut self,
        cmd: LoadRequest,
        mut page: Option<ContextPage>,
        fallback_index: Option<usize>,
        // Spotlight: the songs a play command brought along, if any.
        brought: Vec<ContextTrack>,
    ) -> Result<(), Error> {
        // Spotlight: new music replaces a handover still waiting on its playlist. Left in
        // place, it was finished by this music's list and moved it to the handover's song.
        // The playlist isn't waited for either: arriving late, it would replace this music.
        if self.transfer_state.take().is_some() {
            self.context_resolver.clear();
        }
        self.handed_over = None;

        self.connect_state
            .reset_context(if let PlayContext::Uri(ref uri) = cmd.context {
                ResetContext::WhenDifferent(uri)
            } else {
                ResetContext::Completely
            });

        self.connect_state.reset_options();

        let autoplay = matches!(cmd.context_options, Some(LoadContextOptions::Autoplay));
        match cmd.context {
            PlayContext::Uri(uri) => {
                self.load_context_from_uri(uri.clone(), page.as_ref(), autoplay)
                    .await?;
                // Spotlight: Spotify couldn't give the list the command named, so the songs
                // it brought play. Before, the command failed here and nothing started.
                let nothing_to_play = self
                    .connect_state
                    .get_context(ContextType::Default)
                    .is_err();
                if nothing_to_play && !brought.is_empty() {
                    warn!(
                        target: "spotlight",
                        "Spotify couldn't give the list this music named ({uri}), so the songs sent with it play"
                    );
                    self.load_context_from_tracks(brought)?;
                    // They came whole, ids and all, so there is nothing to add from the page.
                    page = None;
                }
            }
            PlayContext::Tracks(tracks) => {
                // Spotlight: more pages of the list before, or the similar songs after it,
                // may still be asked for. Arriving later, they would be added to this one.
                self.context_resolver.clear();
                if brought.is_empty() {
                    self.load_context_from_tracks(tracks)?
                } else {
                    // Spotlight: a play command's songs are loaded as they came. From their
                    // uris alone, the ids were put back by song, on the first of two alike.
                    self.load_context_from_tracks(brought)?;
                    page = None;
                }
            }
        }

        let cmd_options = cmd.options;

        self.connect_state.set_active_context(ContextType::Default);

        // for play commands with skip by uid, the context of the command contains
        // tracks with uri and uid, so we merge the new context with the resolved/existing context
        self.connect_state.merge_context(page);

        // load here, so that we clear the queue only after we definitely retrieved a new context
        self.connect_state.clear_next_tracks();
        self.connect_state.clear_restrictions();

        debug!("play track <{:?}>", cmd_options.playing_track);

        let index = match cmd_options.playing_track {
            None => None,
            Some(ref playing_track) => Some(match playing_track {
                PlayingTrack::Index(i) => Ok(*i as usize),
                PlayingTrack::Uri(uri) => {
                    let ctx = self.connect_state.get_context(ContextType::Default)?;
                    ConnectState::find_index_in_context(ctx, |t| &t.uri == uri)
                }
                PlayingTrack::Uid(uid) => {
                    let ctx = self.connect_state.get_context(ContextType::Default)?;
                    ConnectState::find_index_in_context(ctx, |t| &t.uid == uid)
                }
            }),
        }
        .map(|i| {
            i.unwrap_or_else(|why| {
                warn!(
                    "Failed to resolve index by {:?}, using fallback index: {:?} (Error: {why})",
                    cmd_options.playing_track, fallback_index
                );
                fallback_index.unwrap_or_default()
            })
        });

        if let Some(LoadContextOptions::Options(ref options)) = cmd_options.context_options {
            debug!(
                "loading with shuffle: <{}>, repeat track: <{}> context: <{}>",
                options.shuffle, options.repeat, options.repeat_track
            );

            self.connect_state.set_shuffle(options.shuffle);
            self.connect_state.set_repeat_context(options.repeat);
            self.connect_state.set_repeat_track(options.repeat_track);
        }

        if matches!(cmd_options.context_options, Some(LoadContextOptions::Options(ref o)) if o.shuffle)
        {
            if let Some(index) = index {
                self.connect_state.set_current_track(index)?;
            } else {
                self.connect_state.set_current_track_random()?;
            }

            if self.context_resolver.has_next() {
                self.connect_state.update_queue_revision()
            } else {
                self.connect_state.shuffle_new()?;
                self.add_autoplay_resolving_when_required();
            }
        } else {
            self.connect_state
                .set_current_track(index.unwrap_or_default())?;
            self.connect_state.reset_playback_to_position(index)?;
            self.add_autoplay_resolving_when_required();
        }

        if self.connect_state.current_track(MessageField::is_some) {
            self.load_track(cmd_options.start_playing, cmd_options.seek_to)?;
        } else {
            info!("No active track, stopping");
            self.handle_stop()
        }

        Ok(())
    }

    async fn load_context_from_uri(
        &mut self,
        context_uri: String,
        page: Option<&ContextPage>,
        autoplay: bool,
    ) -> Result<(), Error> {
        if !self.connect_state.is_active() {
            self.handle_activate();
        }

        let update_context = if autoplay {
            ContextType::Autoplay
        } else {
            ContextType::Default
        };

        self.connect_state.set_active_context(update_context);

        let fallback = match page {
            // check that the uri is valid or the page has a valid uri that can be used
            Some(page) => match ConnectState::find_valid_uri(Some(&context_uri), Some(page)) {
                Some(ctx_uri) => ctx_uri,
                None => return Err(SpircError::InvalidUri(context_uri).into()),
            },
            // when there is no page, the uri should be valid
            None => &context_uri,
        };

        let current_context_uri = self.connect_state.context_uri();

        // Spotlight: only a list that is here can be played from as it is. A handover names
        // its playlist before it has it, and when that never came there is nothing behind
        // the name: the same playlist asked for again was then not fetched at all.
        let have_it = self
            .connect_state
            .get_context(ContextType::Default)
            .is_ok();

        if have_it && current_context_uri == &context_uri && fallback == context_uri {
            debug!("context <{current_context_uri}> didn't change, no resolving required")
        } else {
            debug!("resolving context for load command");
            self.context_resolver.clear();
            self.context_resolver.add(ResolveContext::from_uri(
                &context_uri,
                fallback,
                update_context,
                ContextAction::Replace,
            ));
            let context = self.context_resolver.get_next_context(Vec::new).await;
            self.handle_next_context(context);
        }

        Ok(())
    }

    /// Spotlight: the playlist a handover named couldn't be had from Spotify, so the music
    /// carries on with the songs the handover brought. Before, only the song playing was
    /// left, and the handover stayed waiting, to be finished by whatever list was asked for
    /// next. Returns whether anything changed.
    fn carry_on_with_what_came(&mut self) -> bool {
        self.transfer_state = None;
        let Some(list) = self.handed_over.take() else {
            return false;
        };
        let carried_on = self
            .load_context_from_tracks(list.tracks.clone())
            .and_then(|()| self.connect_state.carry_on_from(&list));
        match carried_on {
            Ok(()) => {
                log_carried_on(&list, ", as its playlist couldn't be had from Spotify");
                self.add_autoplay_resolving_when_required();
                true
            }
            Err(why) => {
                warn!(target: "spotlight", "The songs coming up didn't come over: {why}");
                false
            }
        }
    }

    fn load_context_from_tracks(&mut self, tracks: impl Into<ContextPage>) -> Result<(), Error> {
        let ctx = Context {
            // by providing values for uri/url the player in the official client's isn't frozen
            uri: Some(WEB_API_URI.into()),
            url: Some(format!("context://{WEB_API_URI}")),
            pages: vec![tracks.into()],
            ..Default::default()
        };

        let _ = self
            .connect_state
            .update_context(ctx, ContextType::Default)?;

        Ok(())
    }

    fn handle_play(&mut self) {
        match self.play_status {
            SpircPlayStatus::Paused {
                position_ms,
                preloading_of_next_track_triggered,
            } => {
                self.player.play();
                self.connect_state
                    .update_position(position_ms, self.now_ms());
                self.play_status = SpircPlayStatus::Playing {
                    nominal_start_time: self.now_ms() - position_ms as i64,
                    preloading_of_next_track_triggered,
                };
            }
            SpircPlayStatus::LoadingPause { position_ms } => {
                self.player.play();
                self.play_status = SpircPlayStatus::LoadingPlay { position_ms };
            }
            _ => return,
        }

        // Synchronize the volume from the mixer. This is useful on
        // systems that can switch sources from and back to librespot.
        let current_volume = self.mixer.volume();
        self.set_volume(current_volume);
    }

    fn handle_play_pause(&mut self) {
        match self.play_status {
            SpircPlayStatus::Paused { .. } | SpircPlayStatus::LoadingPause { .. } => {
                self.handle_play()
            }
            SpircPlayStatus::Playing { .. } | SpircPlayStatus::LoadingPlay { .. } => {
                self.handle_pause()
            }
            _ => (),
        }
    }

    fn handle_pause(&mut self) {
        match self.play_status {
            SpircPlayStatus::Playing {
                nominal_start_time,
                preloading_of_next_track_triggered,
            } => {
                self.player.pause();
                let position_ms = (self.now_ms() - nominal_start_time) as u32;
                self.connect_state
                    .update_position(position_ms, self.now_ms());
                self.play_status = SpircPlayStatus::Paused {
                    position_ms,
                    preloading_of_next_track_triggered,
                };
            }
            SpircPlayStatus::LoadingPlay { position_ms } => {
                self.player.pause();
                self.play_status = SpircPlayStatus::LoadingPause { position_ms };
            }
            _ => (),
        }
    }

    fn handle_seek(&mut self, position_ms: u32) {
        let duration = self.connect_state.player().duration;
        if i64::from(position_ms) > duration {
            warn!("tried to seek to {position_ms}ms of {duration}ms");
            return;
        }

        self.connect_state
            .update_position(position_ms, self.now_ms());
        self.player.seek(position_ms);
        let now = self.now_ms();
        match self.play_status {
            SpircPlayStatus::Stopped => (),
            SpircPlayStatus::LoadingPause {
                position_ms: ref mut position,
            }
            | SpircPlayStatus::LoadingPlay {
                position_ms: ref mut position,
            }
            | SpircPlayStatus::Paused {
                position_ms: ref mut position,
                ..
            } => *position = position_ms,
            SpircPlayStatus::Playing {
                ref mut nominal_start_time,
                ..
            } => *nominal_start_time = now - position_ms as i64,
        };
    }

    fn handle_shuffle(&mut self, shuffle: bool) -> Result<(), Error> {
        self.player.emit_shuffle_changed_event(shuffle);
        self.connect_state.handle_shuffle(shuffle)
    }

    fn handle_repeat_context(&mut self, repeat: bool) -> Result<(), Error> {
        self.player
            .emit_repeat_changed_event(repeat, self.connect_state.repeat_track());
        self.connect_state.handle_set_repeat_context(repeat)
    }

    fn handle_repeat_track(&mut self, repeat: bool) {
        self.player
            .emit_repeat_changed_event(self.connect_state.repeat_context(), repeat);
        self.connect_state.set_repeat_track(repeat);
    }

    fn handle_preload_next_track(&mut self) {
        // Requests the player thread to preload the next track
        match self.play_status {
            SpircPlayStatus::Paused {
                ref mut preloading_of_next_track_triggered,
                ..
            }
            | SpircPlayStatus::Playing {
                ref mut preloading_of_next_track_triggered,
                ..
            } => {
                *preloading_of_next_track_triggered = true;
            }
            _ => (),
        }

        if let Some(track_id) = self.connect_state.preview_next_track() {
            self.player.preload(track_id);
        }
    }

    // Mark unavailable tracks so we can skip them later
    fn handle_unavailable(&mut self, track_id: &SpotifyUri) -> Result<(), Error> {
        self.connect_state.mark_unavailable(track_id)?;
        self.handle_preload_next_track();

        Ok(())
    }

    fn add_autoplay_resolving_when_required(&mut self) {
        let require_load_new = !self
            .connect_state
            .has_next_tracks(Some(CONTEXT_FETCH_THRESHOLD))
            && self.session.autoplay()
            && !self.connect_state.context_uri().is_empty();

        if !require_load_new {
            return;
        }

        let current_context = self.connect_state.context_uri();
        let fallback = self.connect_state.current_track(|t| &t.uri);

        let has_tracks = self
            .connect_state
            .get_context(ContextType::Autoplay)
            .map(|c| !c.tracks.is_empty())
            .unwrap_or_default();

        let resolve = ResolveContext::from_uri(
            current_context,
            fallback,
            ContextType::Autoplay,
            if has_tracks {
                ContextAction::Append
            } else {
                ContextAction::Replace
            },
        );

        self.context_resolver.add(resolve);
    }

    fn handle_next(&mut self, track_uri: Option<String>) -> Result<(), Error> {
        let continue_playing = self.connect_state.is_playing();

        let current_uri = self.connect_state.current_track(|t| &t.uri);
        let mut has_next_track =
            matches!(track_uri, Some(ref track_uri) if current_uri == track_uri);

        if !has_next_track {
            has_next_track = loop {
                let index = self.connect_state.next_track()?;

                let current_uri = self.connect_state.current_track(|t| &t.uri);
                if matches!(track_uri, Some(ref track_uri) if current_uri != track_uri) {
                    continue;
                } else {
                    break index.is_some();
                }
            };
        };

        if has_next_track {
            self.add_autoplay_resolving_when_required();
            self.load_track(continue_playing, 0)
        } else {
            info!("Not playing next track because there are no more tracks left in queue.");
            self.handle_stop();
            Ok(())
        }
    }

    fn handle_prev(&mut self) -> Result<(), Error> {
        // Previous behaves differently based on the position
        // Under 3s it goes to the previous song (starts playing)
        // Over 3s it seeks to zero (retains previous play status)
        if self.position() < 3000 {
            let repeat_context = self.connect_state.repeat_context();
            match self.connect_state.prev_track()? {
                None if repeat_context => self.connect_state.reset_playback_to_position(None)?,
                None => {
                    self.connect_state.reset_playback_to_position(None)?;
                    self.handle_stop()
                }
                Some(_) => self.load_track(self.connect_state.is_playing(), 0)?,
            }
        } else {
            self.handle_seek(0);
        }

        Ok(())
    }

    fn handle_volume_up(&mut self) {
        let volume = (self.connect_state.device_info().volume as u16)
            .saturating_add(self.connect_state.volume_step_size);

        self.set_volume_chosen_here(volume);
    }

    fn handle_volume_down(&mut self) {
        let volume = (self.connect_state.device_info().volume as u16)
            .saturating_sub(self.connect_state.volume_step_size);

        self.set_volume_chosen_here(volume);
    }

    /// Spotlight: takes a volume Spotify passed on, from another device or app, and notes it
    /// for the log: a volume no one chose here is otherwise a riddle. `marked` is whether it
    /// came marked as asked for by one of Spotlight's apps.
    fn take_volume_from_elsewhere(&mut self, volume: u16, came_with: String, marked: bool) {
        let before = self.connect_state.device_info().volume;
        self.set_volume(volume);
        // Every command counts once one of them changed the volume, also one that asks for
        // the volume it already has.
        let changed = self.connect_state.device_info().volume != before;
        if changed || self.volume_asked_through_spotify.is_some() {
            let asked = self.volume_asked_through_spotify.get_or_insert_default();
            asked.times += 1;
            asked.came_with = came_with;
            asked.marked = marked;
        }
    }

    /// Spotlight: notes a volume from elsewhere that wasn't taken. Nothing is said back to
    /// Spotify: an asker that sends again whenever the volume isn't the one it wants would
    /// never stop. The log says it at most every few seconds for the same reason.
    fn refuse_volume(&mut self, percent: u32, came_with: &str) {
        self.volumes_refused.0 += 1;
        self.volumes_refused.2 = percent;
        if self.volumes_refused.1.is_none_or(|said| said.elapsed() >= REFUSED_VOLUME_LOG_GAP) {
            info!(
                target: "spotlight",
                "Refused a volume of {percent}% another device or app asked for ({} refused since this was last said). It came with {came_with}",
                self.volumes_refused.0
            );
            self.volumes_refused = (0, Some(Instant::now()), 0);
        }
    }

    /// Spotlight: changes whose volumes are taken from now on ([`Spirc::set_volume_from`]).
    /// A volume still waiting to prove a mark was asked for under the rule before, which
    /// didn't take it, so it is refused.
    async fn take_volumes_from(&mut self, disable_volume: bool, only_marked_volume: bool) {
        if let Some(refused) = self.volume_marks.forget() {
            self.refuse_volume(refused.percent, &refused.came_with);
        }
        self.only_marked_volume = only_marked_volume;
        if !self.connect_state.set_disable_volume(disable_volume) {
            return;
        }
        // Spotify is told, so that its apps show or hide this device's volume. The player
        // goes by its own word either way, so failing to tell changes nothing here.
        if let Err(why) = self.notify().await {
            warn!("couldn't tell Spotify whether this device's volume is for others to set: {why}");
        }
    }

    /// Spotlight: a volume this app asked for. It is the latest word, so one Spotify passed
    /// on just before is no longer what the log should speak of.
    fn set_volume_chosen_here(&mut self, volume: u16) {
        self.volume_asked_through_spotify = None;
        self.set_volume(volume);
    }

    fn handle_playlist_modification(
        &mut self,
        playlist_modification_info: PlaylistModificationInfo,
    ) -> Result<(), Error> {
        let uri = playlist_modification_info
            .uri
            .ok_or(SpircError::NoUri("playlist modification"))?;
        let uri = String::from_utf8(uri)?;

        if self.connect_state.context_uri() != &uri {
            debug!(
                "ignoring playlist modification update for playlist <{uri}>, because it isn't the current context"
            );
            return Ok(());
        }

        debug!("playlist modification for current context: {uri}");
        self.context_resolver.add(ResolveContext::from_uri(
            uri,
            self.connect_state.current_track(|t| &t.uri),
            ContextType::Default,
            ContextAction::Replace,
        ));

        Ok(())
    }

    fn handle_session_update(&mut self, session_update: FallbackWrapper<SessionUpdate>) {
        // we know that this enum value isn't present in our current proto definitions, by that
        // the json parsing fails because the enum isn't known as proto representation
        const WBC: &str = "WIFI_BROADCAST_CHANGED";

        let mut session_update = match session_update {
            FallbackWrapper::Inner(update) => update,
            FallbackWrapper::Fallback(value) => {
                let fallback_inner = value.to_string();
                if fallback_inner.contains(WBC) {
                    log::debug!("Received SessionUpdate::{WBC}");
                } else {
                    log::warn!("SessionUpdate couldn't be parse correctly: {value:?}");
                }
                return;
            }
        };

        let reason = session_update.reason.enum_value();

        let mut session = match session_update.session.take() {
            None => return,
            Some(session) => session,
        };

        let active_device = session.host_active_device_id.take();
        if matches!(active_device, Some(ref device) if device == self.session.device_id()) {
            info!(
                "session update: <{:?}> for self, current session_id {}, new session_id {}",
                reason,
                self.session.session_id(),
                session.session_id
            );

            if self.session.session_id() != session.session_id {
                self.session.set_session_id(&session.session_id);
                self.connect_state.set_session_id(session.session_id);
            }
        } else {
            debug!("session update: <{reason:?}> from active session host: <{active_device:?}>");
        }

        // this seems to be used for jams or handling the current session_id
        //
        // handling this event was intended to keep the playback when other clients (primarily
        // mobile) connects, otherwise they would steel the current playback when there was no
        // session_id provided on the initial PutStateReason::NEW_DEVICE state update
        //
        // by generating an initial session_id from the get-go we prevent that behavior and
        // currently don't need to handle this event, might still be useful for later "jam" support
    }

    fn position(&mut self) -> u32 {
        match self.play_status {
            SpircPlayStatus::Stopped => 0,
            SpircPlayStatus::LoadingPlay { position_ms }
            | SpircPlayStatus::LoadingPause { position_ms }
            | SpircPlayStatus::Paused { position_ms, .. } => position_ms,
            SpircPlayStatus::Playing {
                nominal_start_time, ..
            } => (self.now_ms() - nominal_start_time) as u32,
        }
    }

    fn load_track(&mut self, start_playing: bool, position_ms: u32) -> Result<(), Error> {
        if self.connect_state.current_track(MessageField::is_none) {
            debug!("current track is none, stopping playback");
            self.handle_stop();
            return Ok(());
        }

        let current_uri = self.connect_state.current_track(|t| &t.uri);
        let id = SpotifyUri::from_uri(current_uri)?;
        self.player.load(id, start_playing, position_ms);

        self.connect_state
            .update_position(position_ms, self.now_ms());
        if start_playing {
            self.play_status = SpircPlayStatus::LoadingPlay { position_ms };
        } else {
            self.play_status = SpircPlayStatus::LoadingPause { position_ms };
        }
        self.connect_state.set_status(&self.play_status);

        Ok(())
    }

    async fn notify(&mut self) -> Result<(), Error> {
        self.connect_state.set_status(&self.play_status);

        if self.connect_state.is_playing() {
            self.connect_state
                .update_position_in_relation(self.now_ms());
        }

        self.connect_state.set_now(self.now_ms() as u64);

        self.connect_state
            .send_state(&self.session)
            .await
            .map(|_| ())
    }

    fn set_volume(&mut self, volume: u16) {
        // Spotlight: said whether or not the music is here. An app that keeps the volume for
        // the player's next start would otherwise never hear of one set while it's elsewhere.
        if self.take_volume(volume) {
            self.player.emit_volume_changed_event(volume);
        }
    }

    /// Spotlight: sets the volume without saying so, and returns whether it changed. Alone
    /// it is for the volume the player starts with, which the app gave and needn't hear back:
    /// said late, it would undo a volume chosen while the player was still connecting.
    fn take_volume(&mut self, volume: u16) -> bool {
        debug!("SpircTask::set_volume({volume})");

        let old_volume = self.connect_state.device_info().volume;
        let new_volume = volume as u32;
        let changed = old_volume != new_volume || self.mixer.volume() != volume;
        if changed {
            self.update_volume = true;

            self.connect_state.set_volume(new_volume);
            self.mixer.set_volume(volume);
            if let Some(cache) = self.session.cache() {
                cache.save_volume(volume)
            }
        }
        changed
    }
}

impl Drop for SpircTask {
    fn drop(&mut self) {
        debug!("drop Spirc[{}]", self.spirc_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn songs(uris: &[&str]) -> Vec<ContextTrack> {
        uris.iter()
            .map(|uri| ContextTrack {
                uri: Some(uri.to_string()),
                ..Default::default()
            })
            .collect()
    }

    fn uris(tracks: &[ContextTrack]) -> Vec<String> {
        tracks.iter().flat_map(|track| track.uri.clone()).collect()
    }

    /// What `VolumeMarks` makes of volumes asked for at these seconds: which were taken, and
    /// which proved to be from elsewhere and were refused.
    fn marks(asks: &[(u64, u32)]) -> (Vec<u32>, Vec<u32>) {
        let start = Instant::now();
        let mut marks = VolumeMarks::default();
        let (mut taken, mut refused) = (Vec::new(), Vec::new());
        for &(second, percent) in asks {
            let (marked, not_a_mark) = marks.ask(percent, String::new(), start + Duration::from_secs(second));
            refused.extend(not_a_mark.map(|ask| ask.percent));
            if marked {
                taken.push(percent);
            }
        }
        let end = start + Duration::from_secs(asks.last().map_or(0, |ask| ask.0)) + VOLUME_MARK_WINDOW;
        refused.extend(marks.give_up(end).map(|ask| ask.percent));
        assert!(!marks.waits());
        (taken, refused)
    }

    #[test]
    fn a_volume_is_taken_when_its_mark_came_just_before_it() {
        for percent in 0..=100 {
            let (taken, refused) = marks(&[(0, mark_for_volume(percent)), (0, percent)]);
            assert_eq!((taken, refused), (vec![percent], vec![]), "{percent}%");
            assert_eq!(mark_for_volume(percent).abs_diff(percent), 1, "a mark is one percent off");
        }
    }

    #[test]
    fn a_volume_without_its_mark_is_refused() {
        // What the log showed: jumps between an iPhone's volume steps, one command each.
        let (taken, refused) = marks(&[(0, 25), (1, 100), (2, 25), (40, 6), (41, 25)]);
        assert_eq!((taken, refused), (vec![], vec![25, 100, 25, 6, 25]));
        // The same volume twice, and a mark that came too long before.
        assert_eq!(marks(&[(0, 50), (1, 50)]), (vec![], vec![50, 50]));
        let late = VOLUME_MARK_WINDOW.as_secs() + 1;
        assert_eq!(marks(&[(0, 51), (late, 50)]), (vec![], vec![51, 50]));
        // One percent the other way is no mark.
        assert_eq!(marks(&[(0, 49), (0, 50)]), (vec![], vec![49, 50]));
        // A mark that came the whole of its time before still counts.
        assert_eq!(marks(&[(0, 51), (VOLUME_MARK_WINDOW.as_secs(), 50)]), (vec![50], vec![]));
    }

    #[test]
    fn only_the_one_volume_before_it_marks_a_volume() {
        for before in 0..=100 {
            for percent in 0..=100 {
                let (taken, refused) = marks(&[(0, before), (0, percent)]);
                if before == mark_for_volume(percent) {
                    assert_eq!((taken, refused), (vec![percent], vec![]), "{before}% then {percent}%");
                } else {
                    assert_eq!((taken, refused), (vec![], vec![before, percent]), "{before}% then {percent}%");
                }
            }
        }
    }

    #[test]
    fn a_mark_and_its_volume_the_wrong_way_round_are_refused() {
        for percent in 0..99 {
            let (taken, refused) = marks(&[(0, percent), (0, mark_for_volume(percent))]);
            assert_eq!((taken, refused), (vec![], vec![percent, mark_for_volume(percent)]), "{percent}%");
        }
        // All but the two at the top, where each is the other's mark: 100% arriving before
        // its mark leaves the player at 99%.
        assert_eq!(marks(&[(0, 100), (0, 99)]), (vec![99], vec![]));
    }

    #[test]
    fn a_slider_drawn_up_to_the_top_in_another_app_gets_there() {
        // Not wanted, but known: the last step of such a drag is 99% and then 100%, which
        // is what Spotlight sends for 100%.
        assert_eq!(marks(&[(0, 97), (0, 98), (0, 99), (0, 100)]), (vec![100], vec![97, 98]));
    }

    #[test]
    fn a_marked_volume_is_no_mark_for_the_next() {
        // A slider drawn slowly down in another app: 52, 51, 50, 49. The first pair looks
        // marked; the steps after it don't ride on it.
        let (taken, refused) = marks(&[(0, 52), (0, 51), (0, 50), (0, 49)]);
        assert_eq!((taken, refused), (vec![51, 49], vec![]));
        let (taken, refused) = marks(&[(0, 52), (0, 51), (0, 50)]);
        assert_eq!((taken, refused), (vec![51], vec![50]));
        // Two drags from one of Spotlight's apps, each with its own mark.
        let (taken, refused) = marks(&[(0, 31), (0, 30), (1, 41), (1, 40)]);
        assert_eq!((taken, refused), (vec![30, 40], vec![]));
    }

    #[test]
    fn an_ask_forgotten_marks_nothing_after_it() {
        // Whose volumes are taken changed while a volume waited: it is handed back to be
        // refused, and the volume it would have marked comes unmarked.
        let start = Instant::now();
        let mut marks = VolumeMarks::default();
        assert_eq!(marks.ask(51, "how".into(), start), (false, None));
        assert_eq!(marks.forget().map(|ask| ask.percent), Some(51));
        assert!(!marks.waits());
        assert_eq!(marks.forget(), None);
        assert_eq!(marks.ask(50, String::new(), start).0, false);
    }

    #[test]
    fn an_ask_waits_for_its_time_to_prove_a_mark() {
        let start = Instant::now();
        let mut marks = VolumeMarks::default();
        assert_eq!(marks.ask(25, "how".into(), start), (false, None));
        assert!(marks.waits());
        assert_eq!(marks.time_left(start + Duration::from_secs(1)), VOLUME_MARK_WINDOW - Duration::from_secs(1));
        assert_eq!(marks.give_up(start + Duration::from_secs(1)), None, "not yet");
        assert_eq!(marks.time_left(start + VOLUME_MARK_WINDOW * 2), Duration::ZERO);
        let refused = marks.give_up(start + VOLUME_MARK_WINDOW).unwrap();
        assert_eq!((refused.percent, refused.came_with.as_str()), (25, "how"));
        assert!(!marks.waits());
    }

    #[test]
    fn a_volume_command_keeps_what_came_with_it_but_not_the_connection_id() {
        use crate::core::dealer::protocol::PayloadValue;
        use crate::protocol::connect::ConnectLoggingParams;
        use protobuf::Message as _;

        let sent = SetVolumeCommand {
            volume: 16384,
            connection_type: "wlan".to_string(),
            logging_params: MessageField::some(ConnectLoggingParams {
                interaction_ids: vec!["tap-1".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let message = Message {
            headers: [
                ("Content-Type".to_string(), "application/x-protobuf".to_string()),
                ("Spotify-Connection-Id".to_string(), "private-id".to_string()),
            ]
            .into(),
            payload: PayloadValue::Raw(sent.write_to_bytes().unwrap()),
            uri: "hm://connect-state/v1/connect/volume".to_string(),
        };

        let (command, came_with) = volume_command(message).unwrap();
        assert_eq!(command.volume, 16384);
        assert!(came_with.contains(r#"connection type "wlan""#), "{came_with}");
        assert!(came_with.contains(r#"interaction ids ["tap-1"]"#), "{came_with}");
        assert!(came_with.contains("page ids []"), "{came_with}");
        assert!(came_with.contains("Content-Type: application/x-protobuf"), "{came_with}");
        assert!(came_with.contains("Spotify-Connection-Id: (hidden)"), "{came_with}");
        assert!(!came_with.contains("private-id"), "{came_with}");
    }

    #[test]
    fn a_play_command_that_names_no_real_list_plays_the_songs_it_brought() {
        let brought = songs(&["spotify:track:a", "spotify:track:b"]);
        for made_up in ["-", "", "spotify:web-api", "spotify:unknown"] {
            let plays = named_or_brought(Some(made_up.to_string()), &brought, None);
            assert!(
                matches!(plays, Some(PlayContext::Tracks(ref tracks)) if *tracks == uris(&brought)),
                "{made_up:?} should play the songs brought, not be asked for: {plays:?}"
            );
        }
        let unnamed = named_or_brought(None, &brought, None);
        assert!(matches!(unnamed, Some(PlayContext::Tracks(ref tracks)) if *tracks == uris(&brought)));
        // Another list playing under a made-up name is no reason to ask for this one.
        let plays = named_or_brought(Some("-".to_string()), &brought, Some("spotify:web-api"));
        assert!(matches!(plays, Some(PlayContext::Tracks(_))), "{plays:?}");
    }

    #[test]
    fn a_play_command_that_names_a_list_still_asks_for_it() {
        let brought = songs(&["spotify:track:a"]);
        for named in ["spotify:album:x", "spotify:playlist:y", "spotify:search:some+words"] {
            for brought in [&brought[..], &[]] {
                let plays = named_or_brought(Some(named.to_string()), brought, None);
                assert!(matches!(plays, Some(PlayContext::Uri(ref uri)) if uri == named), "{named}: {plays:?}");
            }
        }
        // With no songs to fall back on, a made-up name is all there is to go on.
        assert!(matches!(named_or_brought(Some("-".to_string()), &[], None), Some(PlayContext::Uri(_))));
        assert!(named_or_brought(None, &[], None).is_none());
    }

    #[test]
    fn a_song_picked_in_the_list_playing_here_is_played_from_that_list() {
        // Another app names the list this player has and may bring only some of its songs.
        let brought = songs(&["spotify:track:a"]);
        let plays = named_or_brought(Some("spotify:web-api".to_string()), &brought, Some("spotify:web-api"));
        assert!(matches!(plays, Some(PlayContext::Uri(ref uri)) if uri == "spotify:web-api"), "{plays:?}");
        // An empty name is never the name of a list that is here.
        let plays = named_or_brought(Some(String::new()), &brought, Some(""));
        assert!(matches!(plays, Some(PlayContext::Tracks(_))), "{plays:?}");
    }

    #[test]
    fn other_songs_under_the_name_of_the_list_here_are_a_new_list() {
        let here = ["spotify:track:a", "spotify:track:b"];
        assert!(all_in_list(&songs(&["spotify:track:b"]), here.into_iter()));
        assert!(all_in_list(&songs(&["spotify:track:b", "spotify:track:a"]), here.into_iter()));
        assert!(!all_in_list(&songs(&["spotify:track:a", "spotify:track:c"]), here.into_iter()));
        // A song that came without its uri can't be found in the list, so it is loaded.
        assert!(!all_in_list(&[ContextTrack::default()], here.into_iter()));
    }
}
