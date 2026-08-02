use crate::config::Config;
use crate::events::{MediaEvent, PlaybackState, TrackInfo};
use anyhow::{Context, Result};
use log::{debug, info};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession, GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus, MediaPropertiesChangedEventArgs,
    PlaybackInfoChangedEventArgs, SessionsChangedEventArgs,
};
use windows::Storage::Streams::{Buffer, DataReader, InputStreamOptions};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::Interface;

enum Signal {
    Sessions,
    MediaProperties(GlobalSystemMediaTransportControlsSession),
    PlaybackInfo(GlobalSystemMediaTransportControlsSession),
}

struct SessionSubscription {
    session: GlobalSystemMediaTransportControlsSession,
    properties_token: EventRegistrationToken,
    playback_token: EventRegistrationToken,
}

struct ListenerState {
    manager: GlobalSystemMediaTransportControlsSessionManager,
    config: Config,
    output: Sender<MediaEvent>,
    signal_tx: Sender<Signal>,
    subscription: Option<SessionSubscription>,
    current_key: Option<usize>,
    recent_playing: Option<GlobalSystemMediaTransportControlsSession>,
    pending_track: Option<(usize, TrackInfo)>,
    pending_playback: Option<(usize, PlaybackState)>,
    pending_deadline: Option<Instant>,
    track_pending_since: Option<Instant>,
    last_content_fingerprint: Option<TrackFingerprint>,
    last_playback: Option<(usize, PlaybackState)>,
    last_session_check: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackFingerprint {
    title: String,
    artist: String,
    album: String,
    artwork_hash: u64,
}

pub struct SmtcListener {
    output: Sender<MediaEvent>,
    config: Config,
}

impl SmtcListener {
    pub fn new(output: Sender<MediaEvent>, config: Config) -> Self {
        Self { output, config }
    }

    pub fn run(self) -> Result<()> {
        // WinRT factory calls and blocking IAsyncOperation::get require an apartment
        // on this worker. Keeping it MTA avoids coupling the UI thread to COM.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        let result = self.run_initialized();
        unsafe { CoUninitialize() };
        result
    }

    fn run_initialized(self) -> Result<()> {
        let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?
            .get()
            .context("requesting the SMTC session manager")?;
        let (signal_tx, signal_rx) = mpsc::channel();
        let sessions_token = register_sessions_handler(&manager, signal_tx.clone())?;
        let mut state = ListenerState::new(manager, self.config, self.output, signal_tx);

        state.refresh_current_session(None, true)?;
        state.event_loop(signal_rx)?;

        let _ = state.manager.RemoveSessionsChanged(sessions_token);
        state.unsubscribe();
        Ok(())
    }
}

impl ListenerState {
    fn new(
        manager: GlobalSystemMediaTransportControlsSessionManager,
        config: Config,
        output: Sender<MediaEvent>,
        signal_tx: Sender<Signal>,
    ) -> Self {
        Self {
            manager,
            config,
            output,
            signal_tx,
            subscription: None,
            current_key: None,
            recent_playing: None,
            pending_track: None,
            pending_playback: None,
            pending_deadline: None,
            track_pending_since: None,
            last_content_fingerprint: None,
            last_playback: None,
            last_session_check: Instant::now(),
        }
    }

    fn event_loop(&mut self, signal_rx: Receiver<Signal>) -> Result<()> {
        let session_check_interval = Duration::from_secs(2);
        loop {
            let timeout = self
                .pending_deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_secs(24 * 60 * 60));

            match signal_rx.recv_timeout(timeout) {
                Ok(signal) => self.handle_signal(signal)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.flush_pending();
                    // Periodically re-check sessions to catch missed changes. emit_initial=true
                    // so a newly-detected current session reports its track+state immediately.
                    if self.last_session_check.elapsed() >= session_check_interval {
                        self.last_session_check = Instant::now();
                        let _ = self.refresh_current_session(None, true);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }

    fn handle_signal(&mut self, signal: Signal) -> Result<()> {
        match signal {
            Signal::Sessions => {
                debug!("SMTC SessionsChanged");
                self.refresh_current_session(None, true)?;
            }
            Signal::MediaProperties(session) => {
                // emit_initial=true: when this event changes the current session,
                // immediately report the new session's track+state instead of
                // showing stale info from the previous app.
                self.refresh_current_session(Some(&session), true)?;
                if self.current_key == Some(session_key(&session))
                    && read_playback_state(&session)? != Some(PlaybackState::Stopped)
                    && let Ok(track) = read_track_info(&session)
                {
                    let key = session_key(&session);
                    // SMTC fills metadata progressively (title -> artist -> album ->
                    // artwork). If a read for the same song arrives, merge the richer
                    // fields into the pending track instead of replacing it, so the
                    // notification is shown once, complete.
                    let is_merge = self
                        .pending_track
                        .as_ref()
                        .is_some_and(|(k, p)| *k == key && p.title == track.title && p.artist == track.artist);
                    if is_merge {
                        if let Some((_, p)) = self.pending_track.as_mut() {
                            if !track.album.trim().is_empty() {
                                p.album = track.album;
                            }
                            if track.artwork.is_some() {
                                p.artwork = track.artwork;
                            }
                            if !track.source_app.trim().is_empty() && p.source_app.trim().is_empty() {
                                p.source_app = track.source_app;
                            }
                        }
                    } else {
                        self.pending_track = Some((key, track));
                    }
                    self.schedule_flush();
                }
            }
            Signal::PlaybackInfo(session) => {
                self.refresh_current_session(Some(&session), true)?;
                if self.current_key == Some(session_key(&session))
                    && let Some(state) = read_playback_state(&session)?
                {
                    if state == PlaybackState::Playing {
                        self.recent_playing = Some(session.clone());
                    }
                    self.pending_playback = Some((session_key(&session), state));
                    self.schedule_flush();
                }
            }
        }
        Ok(())
    }

    fn refresh_current_session(
        &mut self,
        hint: Option<&GlobalSystemMediaTransportControlsSession>,
        emit_initial: bool,
    ) -> Result<()> {
        let resolved = self.resolve_current_session(hint);
        let new_key = resolved.as_ref().map(session_key);
        if new_key == self.current_key {
            return Ok(());
        }

        self.unsubscribe();
        self.current_key = new_key;
        if let Some(session) = resolved {
            self.subscribe(&session)?;
            if emit_initial {
                let playback = read_playback_state(&session)?;
                if playback != Some(PlaybackState::Stopped)
                    && let Ok(track) = read_track_info(&session)
                {
                    self.pending_track = Some((session_key(&session), track));
                }
                if let Some(state) = playback {
                    if state == PlaybackState::Playing {
                        self.recent_playing = Some(session.clone());
                    }
                    if state == PlaybackState::Stopped {
                        // Establish a baseline without showing an empty/stopped
                        // notification when the app starts with no active media.
                        self.last_playback = Some((session_key(&session), state));
                    } else {
                        self.pending_playback = Some((session_key(&session), state));
                    }
                }
                self.flush_pending();
            }
        }
        Ok(())
    }

    fn resolve_current_session(
        &mut self,
        hint: Option<&GlobalSystemMediaTransportControlsSession>,
    ) -> Option<GlobalSystemMediaTransportControlsSession> {
        // First, try to find any Playing session from all sessions.
        if let Ok(sessions) = self.manager.GetSessions()
            && let Some(playing) = sessions
                .into_iter()
                .find(|s| matches!(read_playback_state(s), Ok(Some(PlaybackState::Playing))))
        {
            return Some(playing);
        }

        // Fall back to the current session if it's not Stopped.
        if let Ok(session) = self.manager.GetCurrentSession()
            && !matches!(read_playback_state(&session), Ok(Some(PlaybackState::Stopped)))
        {
            return Some(session);
        }

        // Prefer the hint session if it's not Stopped.
        if let Some(session) = hint
            && !matches!(read_playback_state(session), Ok(Some(PlaybackState::Stopped)))
        {
            return Some(session.clone());
        }

        // Fall back to the last observed playing session.
        if let Some(session) = self.recent_playing.clone()
            && !matches!(read_playback_state(&session), Ok(Some(PlaybackState::Stopped)))
        {
            return Some(session);
        }

        None
    }

    fn subscribe(&mut self, session: &GlobalSystemMediaTransportControlsSession) -> Result<()> {
        let properties_session = session.clone();
        let playback_session = session.clone();
        let properties_tx = self.signal_tx.clone();
        let playback_tx = self.signal_tx.clone();
        let properties_handler: TypedEventHandler<
            GlobalSystemMediaTransportControlsSession,
            MediaPropertiesChangedEventArgs,
        > = TypedEventHandler::new(move |_, _| {
            let _ = properties_tx.send(Signal::MediaProperties(properties_session.clone()));
            Ok(())
        });
        let playback_handler: TypedEventHandler<
            GlobalSystemMediaTransportControlsSession,
            PlaybackInfoChangedEventArgs,
        > = TypedEventHandler::new(move |_, _| {
            let _ = playback_tx.send(Signal::PlaybackInfo(playback_session.clone()));
            Ok(())
        });

        let properties_token = session.MediaPropertiesChanged(&properties_handler)?;
        let playback_token = match session.PlaybackInfoChanged(&playback_handler) {
            Ok(token) => token,
            Err(error) => {
                let _ = session.RemoveMediaPropertiesChanged(properties_token);
                return Err(error.into());
            }
        };
        self.subscription = Some(SessionSubscription {
            session: session.clone(),
            properties_token,
            playback_token,
        });
        info!("subscribed to SMTC session {}", session_key(session));
        Ok(())
    }

    fn unsubscribe(&mut self) {
        if let Some(subscription) = self.subscription.take() {
            let _ = subscription
                .session
                .RemoveMediaPropertiesChanged(subscription.properties_token);
            let _ = subscription
                .session
                .RemovePlaybackInfoChanged(subscription.playback_token);
        }
    }

    fn schedule_flush(&mut self) {
        if self.track_pending_since.is_none() {
            self.track_pending_since = Some(Instant::now());
        }
        self.pending_deadline = Some(Instant::now() + debounce_duration(&self.config));
    }

    fn flush_pending(&mut self) {
        self.pending_deadline = None;
        // Only send when artwork is ready. Re-schedule every 100ms up to 3s.
        if let Some((_, track)) = &self.pending_track
            && track.artwork.is_none()
        {
            let elapsed = self.track_pending_since.map(|t| t.elapsed()).unwrap_or(Duration::ZERO);
            if elapsed < Duration::from_millis(3000) {
                self.pending_deadline = Some(Instant::now() + Duration::from_millis(100));
                return;
            }
        }
        self.track_pending_since = None;
        if let Some((_key, track)) = self.pending_track.take() {
            let fingerprint = track_fingerprint(&track);
            if self.last_content_fingerprint.as_ref() != Some(&fingerprint) {
                self.last_content_fingerprint = Some(fingerprint);
                info!(
                    "track changed | title={:?} | artist={:?} | album={:?} | source={:?}",
                    track.title, track.artist, track.album, track.source_app
                );
                let _ = self.output.send(MediaEvent::TrackChanged(track));
            }
        }
        if let Some((key, state)) = self.pending_playback.take()
            && self.last_playback != Some((key, state))
        {
            self.last_playback = Some((key, state));
            info!("playback state changed | state={state:?}");
            let _ = self.output.send(MediaEvent::PlaybackStateChanged(state));
        }
    }
}

fn register_sessions_handler(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
    signal_tx: Sender<Signal>,
) -> Result<EventRegistrationToken> {
    let handler: TypedEventHandler<GlobalSystemMediaTransportControlsSessionManager, SessionsChangedEventArgs> =
        TypedEventHandler::new(move |_, _| {
            let _ = signal_tx.send(Signal::Sessions);
            Ok(())
        });
    Ok(manager.SessionsChanged(&handler)?)
}

fn session_key(session: &GlobalSystemMediaTransportControlsSession) -> usize {
    session.as_raw() as usize
}

fn read_track_info(session: &GlobalSystemMediaTransportControlsSession) -> Result<TrackInfo> {
    let source_app = session
        .SourceAppUserModelId()
        .map(|value| source_app_label(&value.to_string()))
        .unwrap_or_else(|_| "Media".to_string());
    let properties = session.TryGetMediaPropertiesAsync()?.get()?;
    let title = non_empty(properties.Title()?.to_string(), &source_app);
    let artist = non_empty(properties.Artist()?.to_string(), &source_app);
    // Keep album empty when the app has not provided it yet; renderers hide the
    // album line until real data arrives (prevents a bogus "Unknown album").
    let album = non_empty(properties.AlbumTitle()?.to_string(), "");
    let artwork = read_artwork(&properties).unwrap_or_else(|error| {
        debug!("album-art read failed: {error:#}");
        None
    });
    let track_number = {
        let n = properties.TrackNumber()?;
        if n > 0 { Some(n as u32) } else { None }
    };
    let track_count = {
        let n = properties.AlbumTrackCount()?;
        if n > 0 { Some(n as u32) } else { None }
    };
    let genre = {
        let genres: Vec<String> = properties.Genres()?.into_iter().map(|g| g.to_string()).collect();
        let joined = genres.join(", ");
        if joined.trim().is_empty() { None } else { Some(joined) }
    };
    // Total duration is static per track (EndTime - StartTime); fine to read
    // once at track-change time without any continuous timeline updates.
    let duration_secs = read_duration(session);
    Ok(TrackInfo {
        title,
        artist,
        album,
        artwork,
        source_app,
        duration_secs,
        track_number,
        track_count,
        genre,
    })
}

fn read_duration(session: &GlobalSystemMediaTransportControlsSession) -> Option<u64> {
    let timeline = session.GetTimelineProperties().ok()?;
    let start = timeline.StartTime().ok()?.Duration;
    let end = timeline.EndTime().ok()?.Duration;
    let duration_100ns = end - start;
    if duration_100ns <= 0 {
        return None;
    }
    // TimeSpan.Duration is in 100-nanosecond units.
    Some((duration_100ns / 10_000_000) as u64)
}

fn read_playback_state(session: &GlobalSystemMediaTransportControlsSession) -> Result<Option<PlaybackState>> {
    let status = session.GetPlaybackInfo()?.PlaybackStatus()?;
    Ok(match status {
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing => Some(PlaybackState::Playing),
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Paused => Some(PlaybackState::Paused),
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Stopped
        | GlobalSystemMediaTransportControlsSessionPlaybackStatus::Closed => Some(PlaybackState::Stopped),
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Opened
        | GlobalSystemMediaTransportControlsSessionPlaybackStatus::Changing => None,
        _ => None,
    })
}

fn read_artwork(
    properties: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties,
) -> Result<Option<Vec<u8>>> {
    let reference = properties.Thumbnail()?;
    let stream = reference.OpenReadAsync()?.get()?;
    let size = stream.Size()?;
    if size == 0 || size > 8 * 1024 * 1024 || size > u32::MAX as u64 {
        return Ok(None);
    }
    let size = size as u32;
    let buffer = Buffer::Create(size)?;
    stream.ReadAsync(&buffer, size, InputStreamOptions::None)?.get()?;
    let reader = DataReader::FromBuffer(&buffer)?;
    let mut data = vec![0u8; size as usize];
    reader.ReadBytes(&mut data)?;
    Ok(Some(data))
}

fn non_empty(value: String, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn source_app_label(value: &str) -> String {
    let value = value.rsplit('!').next().unwrap_or(value);
    let value = value.split('_').next().unwrap_or(value);
    non_empty(value.to_string(), "Media")
}

fn track_fingerprint(track: &TrackInfo) -> TrackFingerprint {
    // Content-only fingerprint: title+artist+album. Artwork is excluded so the
    // same track is recognized even when the thumbnail arrives on a later event.
    TrackFingerprint {
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        artwork_hash: 0,
    }
}

fn debounce_duration(config: &Config) -> Duration {
    Duration::from_millis(config.behavior.debounce_ms.clamp(150, 250))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_app_label_uses_a_readable_fallback() {
        assert_eq!(source_app_label("SpotifyAB.SpotifyMusic_abc!Spotify"), "Spotify");
        assert_eq!(source_app_label("browser"), "browser");
        assert_eq!(source_app_label(""), "Media");
    }

    #[test]
    fn debounce_window_is_clamped_to_the_coalescing_range() {
        let mut config = Config::default();
        config.behavior.debounce_ms = 1;
        assert_eq!(debounce_duration(&config), Duration::from_millis(150));
        config.behavior.debounce_ms = 1000;
        assert_eq!(debounce_duration(&config), Duration::from_millis(250));
    }
}
