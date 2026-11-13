use anyhow::Result;
use librespot::discovery::Discovery;
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::{authentication::Credentials, config::DeviceType, Session, SessionConfig};
use librespot_playback::{
    config::{Bitrate, NormalisationMethod, PlayerConfig, VolumeCtrl},
    mixer::{mappings::MappedCtrl, Mixer, MixerConfig},
    player::{Player, PlayerEventChannel},
};
use std::sync::{atomic::{AtomicU16, Ordering}, Arc};
use tokio::task::JoinHandle;

/// Librespot's own soft mixer sets the volume of audio as it is decoded, so a
/// change only comes out after the buffer. This one sets it at the output, on the
/// same volume curve, and leaves the decoded audio alone.
struct OutputMixer {
    curve: VolumeCtrl,
    volume: AtomicU16,
}

impl Mixer for OutputMixer {
    fn open(config: MixerConfig) -> Result<Self, librespot_core::Error> {
        Ok(Self { curve: config.volume_ctrl, volume: AtomicU16::new(u16::MAX) })
    }

    fn volume(&self) -> u16 {
        self.volume.load(Ordering::Relaxed)
    }

    fn set_volume(&self, volume: u16) {
        self.volume.store(volume, Ordering::Relaxed);
        crate::audio_output::set_player_gain(self.curve.to_mapped(volume) as f32);
    }
}

/// Applies a volume the user just chose to whatever is about to play.
pub fn set_volume_now(volume_percent: u16) {
    let volume = percent_to_spotify_volume(volume_percent);
    let gain = if volume == 0 { 0.0 } else { MixerConfig::default().volume_ctrl.to_mapped(volume) };
    crate::audio_output::set_user_gain(gain as f32);
}

pub struct PlaybackEngine {
    session: Session,
    _discovery: Discovery,
    _spirc: Spirc,
    _spirc_task: JoinHandle<()>,
    player_events: Option<PlayerEventChannel>,
    finished: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl PlaybackEngine {
    pub async fn start(
        access_token: String,
        client_id: String,
        device_name: String,
        audio_quality: String,
        normalize_volume: bool,
        initial_volume_percent: u16,
    ) -> Result<Self> {
        let credentials = Credentials::with_access_token(access_token);
        let mut session_config = SessionConfig::default();
        session_config.client_id = client_id;
        let discovery = Discovery::builder(
            session_config.device_id.clone(),
            session_config.client_id.clone(),
        )
        .name(device_name.clone())
        .device_type(DeviceType::Computer)
        .launch()?;
        let session = Session::new(session_config, None);

        let mixer = OutputMixer::open(MixerConfig::default())?;
        mixer.set_volume(percent_to_spotify_volume(initial_volume_percent));
        let mixer: Arc<dyn Mixer> = Arc::new(mixer);
        let player_config = PlayerConfig {
            bitrate: match audio_quality.as_str() {
                "High" => Bitrate::Bitrate320,
                "Low" => Bitrate::Bitrate96,
                _ => Bitrate::Bitrate160,
            },
            normalisation: normalize_volume,
            // Only ever adjusts gain. The default "dynamic" method runs a
            // limiter over the audio, which colors loud masters.
            normalisation_method: NormalisationMethod::Basic,
            ..PlayerConfig::default()
        };
        let player = Player::new(
            player_config,
            session.clone(),
            mixer.get_soft_volume(),
            crate::audio_output::new_sink,
        );
        let player_events = player.get_player_event_channel();

        let connect_config = ConnectConfig {
            name: device_name.clone(),
            device_type: DeviceType::Computer,
            initial_volume: percent_to_spotify_volume(initial_volume_percent),
            ..Default::default()
        };

        // Register as a Connect device but do NOT call `spirc.activate()`:
        // activating makes this the active device and steals playback from
        // whatever Spotify app is currently playing. Playback is moved here
        // only when the user explicitly starts something on this device.
        let (spirc, spirc_task) = Spirc::new(
            connect_config,
            session.clone(),
            credentials,
            player,
            mixer,
        ).await?;

        // The Connect loop ends for good when the connection to Spotify drops, as after
        // sleep or a network blip, and the device vanishes from Spotify's list with it.
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _ = spirc_task.await;
            let _ = finished_tx.send(());
        });

        Ok(Self {
            session,
            _discovery: discovery,
            _spirc: spirc,
            _spirc_task: handle,
            player_events: Some(player_events),
            finished: Some(finished_rx),
        })
    }

    /// The connection to Spotify, for reading what the public API doesn't offer.
    pub fn session(&self) -> Session {
        self.session.clone()
    }

    pub fn take_player_events(&mut self) -> Option<PlayerEventChannel> {
        self.player_events.take()
    }

    /// Resolves once the player has stopped, for whatever reason.
    pub fn take_finished_signal(&mut self) -> Option<tokio::sync::oneshot::Receiver<()>> {
        self.finished.take()
    }

    pub fn play_pause(&self) -> Result<()> {
        self._spirc.play_pause()?;
        Ok(())
    }

    /// Resumes if paused. Does nothing if already playing.
    pub fn play(&self) -> Result<()> {
        self._spirc.play()?;
        Ok(())
    }

    pub fn is_finished(&self) -> bool {
        self._spirc_task.is_finished()
    }
    pub fn next(&self) -> Result<()> {
        self._spirc.next()?;
        Ok(())
    }

    pub fn previous(&self) -> Result<()> {
        self._spirc.prev()?;
        Ok(())
    }

    pub fn set_volume(&self, volume_percent: u16) -> Result<()> {
        self._spirc.set_volume(percent_to_spotify_volume(volume_percent))?;
        Ok(())
    }

    /// Stops the Connect session so the device disappears from Spotify.
    pub fn shutdown(&self) {
        let _ = self._spirc.shutdown();
    }

    /// Puts the songs coming up in a new order, leaving the one playing alone.
    pub fn set_upcoming(&self, rows_before: Vec<String>, rows_after: Vec<String>) -> Result<()> {
        self._spirc.set_upcoming(rows_before, rows_after)?;
        Ok(())
    }

    pub fn seek(&self, position_ms: u32) -> Result<()> {
        self._spirc.set_position_ms(position_ms)?;
        Ok(())
    }

    /// Plays a list here, from a point in its first song, as it was left: the songs after it
    /// in the order given, also in shuffle, and repeat as it was. This PC becomes the device
    /// playing, so it's only for when nothing else is.
    pub fn resume_list(&self, uris: Vec<String>, position_ms: u32, shuffle: bool, repeat: &str, shown: Vec<String>) -> Result<()> {
        use librespot_connect::{LoadContextOptions, LoadRequest, LoadRequestOptions, Options, PlayingTrack};
        self._spirc.activate()?;
        let options = LoadRequestOptions {
            start_playing: true,
            seek_to: position_ms,
            context_options: Some(LoadContextOptions::Options(Options {
                shuffle: false,
                repeat: repeat == "context",
                repeat_track: repeat == "track",
            })),
            playing_track: Some(PlayingTrack::Index(0)),
        };
        self._spirc.load(LoadRequest::from_tracks(uris, options))?;
        if shuffle {
            // Shuffling deals a new order, so the songs Next up showed are put back in theirs.
            self._spirc.shuffle(true)?;
            self._spirc.set_upcoming(shown.clone(), shown)?;
        }
        Ok(())
    }
}

pub fn spotify_volume_to_percent(volume: u16) -> i32 {
    ((volume as u32 * 100 + u16::MAX as u32 / 2) / u16::MAX as u32) as i32
}

fn percent_to_spotify_volume(volume_percent: u16) -> u16 {
    let clamped = volume_percent.min(100);
    ((clamped as u32 * u16::MAX as u32) / 100) as u16
}
