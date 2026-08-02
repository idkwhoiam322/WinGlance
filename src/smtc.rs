use crate::config::Config;
use crate::events::{MediaEvent, PlaybackState, TrackInfo};
use anyhow::{Context, Result};
use log::{debug, info};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Media::Control::{
    CurrentSessionChangedEventArgs, GlobalSystemMediaTransportControlsSession,
    GlobalSystemMediaTransportControlsSessionManager, GlobalSystemMediaTransportControlsSessionPlaybackStatus,
    MediaPropertiesChangedEventArgs, PlaybackInfoChangedEventArgs, SessionsChangedEventArgs,
};
use windows::Storage::Streams::{Buffer, DataReader, InputStreamOptions};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::Interface;

enum Signal {
    /// Fired by SessionsChanged or CurrentSessionChanged: re-sync subscriptions
    /// and re-resolve the current session against GetCurrentSession().
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
    /// Every open session's event subscriptions, keyed by session pointer.
    /// Unlike Windows' native widget we only *display* one session, but we still
    /// subscribe to all of them so background sessions' changes are never missed.
    subscriptions: HashMap<usize, SessionSubscription>,
    current_key: Option<usize>,
    recent_playing: Option<GlobalSystemMediaTransportControlsSession>,
    pending_track: Option<(usize, TrackInfo)>,
    /// Held playback state with the deadline after which it may be emitted. The
    /// short hold coalesces the Paused→track→Playing burst of a song change into
    /// a single track notification: a state change that is overtaken by a track
    /// event within the hold window is dropped. Carries the source app label for
    /// per-source dedup.
    pending_playback: Option<(usize, PlaybackState, Instant, String)>,
    pending_deadline: Option<Instant>,
    track_pending_since: Option<Instant>,
    last_content_fingerprint: Option<TrackFingerprint>,
    /// Last emitted playback state per source app. Sessions of the same app are
    /// re-created constantly (each tab/video = a new session pointer), so a
    /// paused app that re-registers re-fires the same Paused over and over;
    /// keying by (source, state) absorbs that without missing real changes.
    last_state_by_source: HashMap<String, PlaybackState>,
    /// When the last TrackChanged was emitted, to suppress the transition state
    /// blip that follows a song change.
    last_track_emitted: Option<(usize, Instant)>,
    last_session_check: Instant,
    /// Whether the tracked session is currently Playing. Used to decide when a
    /// new source may take over the pill (only when nothing we track is active).
    current_playing: bool,
    /// Album title by (title, artist), so a track read that omits the album can
    /// still carry it. Insert replaces — a newer album always wins over a stale
    /// cached one.
    album_cache: HashMap<(String, String), String>,
    /// Artwork bytes by (title, artist), so a read that omits the thumbnail can
    /// still carry it. Insert replaces — a new cover for the same item replaces
    /// the old cached one.
    artwork_cache: HashMap<(String, String), Vec<u8>>,
}

/// How long a playback state change is held before emission, so a song-change
/// burst (Paused → track → Playing) collapses into the track notification.
const STATE_HOLD_MS: u64 = 500;
/// Window after a TrackChanged in which same-session playback state changes are
/// suppressed (they are the transition blips of the song change itself).
const TRACK_TRANSITION_MS: u64 = 400;
/// Upper bound for the album/artwork caches; beyond it they are cleared (the
/// caches are a convenience, not critical state).
const CACHE_CAP: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackFingerprint {
    title: String,
    artist: String,
    album: String,
    /// Whether artwork was present. Artwork often arrives in a later event than
    /// title/artist; including its presence means that late artwork re-emits the
    /// track (the UI updates it in place) instead of being deduplicated away.
    has_artwork: bool,
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
        let current_token = register_current_session_handler(&manager, signal_tx.clone())?;
        let mut state = ListenerState::new(manager, self.config, self.output, signal_tx);

        state.sync_subscriptions();
        state.refresh_current_session(None, true, false)?;
        state.event_loop(signal_rx)?;

        let _ = state.manager.RemoveSessionsChanged(sessions_token);
        let _ = state.manager.RemoveCurrentSessionChanged(current_token);
        state.remove_all_subscriptions();
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
            subscriptions: HashMap::new(),
            current_key: None,
            recent_playing: None,
            pending_track: None,
            pending_playback: None,
            pending_deadline: None,
            track_pending_since: None,
            last_content_fingerprint: None,
            last_state_by_source: HashMap::new(),
            last_track_emitted: None,
            last_session_check: Instant::now(),
            current_playing: false,
            album_cache: HashMap::new(),
            artwork_cache: HashMap::new(),
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
                        let _ = self.refresh_current_session(None, true, false);
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
                debug!("SMTC SessionsChanged/CurrentSessionChanged");
                // Re-sync subscriptions with the current session list, then re-resolve
                // the current session against GetCurrentSession() — the pointer
                // Windows itself maintains.
                self.sync_subscriptions();
                self.refresh_current_session(None, true, false)?;
            }
            Signal::MediaProperties(session) => {
                // emit_initial=true: when this event changes the current session,
                // immediately report the new session's track+state instead of
                // showing stale info from the previous app.
                self.refresh_current_session(Some(&session), true, true)?;
                if self.current_key == Some(session_key(&session))
                    && read_playback_state(&session)? != Some(PlaybackState::Stopped)
                    && let Ok(mut track) = read_track_info(&session)
                {
                    self.apply_cache(&mut track);
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
                    // A track change is imminent: drop any held state so the song
                    // change collapses into a single track notification.
                    self.pending_playback = None;
                    self.schedule_flush();
                }
            }
            Signal::PlaybackInfo(session) => {
                let key = session_key(&session);
                let source = read_source_app(&session);
                if self.current_key == Some(key) {
                    if let Some(state) = read_playback_state(&session)? {
                        if state == PlaybackState::Stopped {
                            // The tracked session stopped: hand off to whatever else
                            // is playing right now instead of waiting for the poll.
                            let before = self.current_key;
                            self.refresh_current_session(None, true, false)?;
                            if self.current_key == before {
                                // Nothing else took over — report the stop.
                                self.current_playing = false;
                                self.pending_playback = Some((key, state, Instant::now(), source));
                                self.schedule_flush();
                            }
                            return Ok(());
                        }
                        if state == PlaybackState::Playing {
                            self.recent_playing = Some(session.clone());
                        }
                        self.current_playing = state == PlaybackState::Playing;
                        // Coalescing: skip state changes that are part of a song
                        // change — a track is pending, or one was just emitted for
                        // this session. Genuine pauses (no track activity) pass
                        // through the hold window.
                        let track_pending = self.pending_track.as_ref().is_some_and(|(k, _)| *k == key);
                        let just_emitted = self.last_track_emitted.is_some_and(|(k, at)| {
                            k == key && at.elapsed() < Duration::from_millis(TRACK_TRANSITION_MS)
                        });
                        if track_pending || just_emitted {
                            self.last_state_by_source.insert(source, state);
                            return Ok(());
                        }
                        self.pending_playback = Some((
                            key,
                            state,
                            Instant::now() + Duration::from_millis(STATE_HOLD_MS),
                            source,
                        ));
                        self.schedule_flush();
                    }
                } else if !self.current_playing
                    && matches!(read_playback_state(&session)?, Some(PlaybackState::Playing))
                {
                    // A new source started playing while nothing we track is active.
                    // Adopt it; the pill only follows actively playing media, so a
                    // paused/stale session never steals the current one.
                    self.refresh_current_session(Some(&session), true, false)?;
                }
            }
        }
        Ok(())
    }

    /// Updates the album/artwork caches from a fresh read (replacing any older
    /// entries for the same item) and back-fills any missing fields from cache,
    /// so a notification can carry album/cover even when the event itself omits
    /// them.
    fn apply_cache(&mut self, track: &mut TrackInfo) {
        let key = (track.title.clone(), track.artist.clone());
        if !track.album.trim().is_empty() {
            self.album_cache.insert(key.clone(), track.album.clone());
        } else if let Some(album) = self.album_cache.get(&key) {
            track.album = album.clone();
        }
        if track.artwork.is_some() {
            if let Some(bytes) = &track.artwork {
                self.artwork_cache.insert(key.clone(), bytes.clone());
            }
        } else if let Some(bytes) = self.artwork_cache.get(&key) {
            track.artwork = Some(bytes.clone());
        }
        if self.album_cache.len() > CACHE_CAP {
            self.album_cache.clear();
        }
        if self.artwork_cache.len() > CACHE_CAP {
            self.artwork_cache.clear();
        }
    }

    fn refresh_current_session(
        &mut self,
        hint: Option<&GlobalSystemMediaTransportControlsSession>,
        emit_initial: bool,
        prefer_hint: bool,
    ) -> Result<()> {
        let resolved = self.resolve_current_session(hint, prefer_hint);
        let new_key = resolved.as_ref().map(session_key);
        if new_key == self.current_key {
            return Ok(());
        }

        self.current_key = new_key;
        self.current_playing = false;
        if let Some(session) = resolved {
            self.ensure_subscribed(&session)?;
            let playback = read_playback_state(&session)?;
            self.current_playing = matches!(playback, Some(PlaybackState::Playing));
            if emit_initial {
                if playback != Some(PlaybackState::Stopped)
                    && let Ok(mut track) = read_track_info(&session)
                {
                    self.apply_cache(&mut track);
                    self.pending_track = Some((session_key(&session), track));
                }
                if let Some(state) = playback {
                    if state == PlaybackState::Playing {
                        self.recent_playing = Some(session.clone());
                    }
                    if state == PlaybackState::Stopped {
                        // Establish a baseline without showing an empty/stopped
                        // notification when the app starts with no active media.
                        self.last_state_by_source.insert(read_source_app(&session), state);
                    } else {
                        self.pending_playback =
                            Some((session_key(&session), state, Instant::now(), read_source_app(&session)));
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
        prefer_hint: bool,
    ) -> Option<GlobalSystemMediaTransportControlsSession> {
        // Media-property events (a track/media change) follow the event source
        // itself: the pill should reflect the app + media that CHANGED, like the
        // native widget's per-session entries. State/periodic paths use the
        // native GetCurrentSession() pointer instead.
        if prefer_hint
            && let Some(session) = hint
            && !matches!(read_playback_state(session), Ok(Some(PlaybackState::Stopped)))
        {
            return Some(session.clone());
        }

        // 1. GetCurrentSession() is the pointer Windows itself maintains (the
        //    native media widget follows it); consult it fresh on every resolve.
        if let Ok(session) = self.manager.GetCurrentSession()
            && !matches!(read_playback_state(&session), Ok(Some(PlaybackState::Stopped)))
        {
            return Some(session);
        }

        // 2. The session that caused the event, when it is not stopped.
        if let Some(session) = hint
            && !matches!(read_playback_state(session), Ok(Some(PlaybackState::Stopped)))
        {
            return Some(session.clone());
        }

        // 3. The last observed playing session (keeps the current one stable).
        if let Some(session) = self.recent_playing.clone()
            && matches!(read_playback_state(&session), Ok(Some(PlaybackState::Playing)))
        {
            return Some(session);
        }

        // 4. Any playing session.
        if let Ok(sessions) = self.manager.GetSessions()
            && let Some(playing) = sessions
                .into_iter()
                .find(|s| matches!(read_playback_state(s), Ok(Some(PlaybackState::Playing))))
        {
            return Some(playing);
        }

        None
    }

    fn subscribe(&mut self, session: &GlobalSystemMediaTransportControlsSession) -> Result<SessionSubscription> {
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
        debug!("subscribed to SMTC session {}", session_key(session));
        Ok(SessionSubscription {
            session: session.clone(),
            properties_token,
            playback_token,
        })
    }

    /// Subscribes to a session unless it is already subscribed.
    fn ensure_subscribed(&mut self, session: &GlobalSystemMediaTransportControlsSession) -> Result<()> {
        let key = session_key(session);
        if self.subscriptions.contains_key(&key) {
            return Ok(());
        }
        let subscription = self.subscribe(session)?;
        self.subscriptions.insert(key, subscription);
        Ok(())
    }

    /// Re-syncs the subscription map with the current session list: subscribes
    /// to every open session and drops subscriptions for removed sessions.
    fn sync_subscriptions(&mut self) {
        let Some(sessions) = self.manager.GetSessions().ok() else {
            return;
        };
        let sessions: Vec<_> = sessions.into_iter().collect();
        for session in &sessions {
            if let Err(error) = self.ensure_subscribed(session) {
                debug!("subscribe failed for a session: {error:#}");
            }
        }
        let alive: HashSet<usize> = sessions.iter().map(session_key).collect();
        let stale: Vec<usize> = self
            .subscriptions
            .keys()
            .filter(|k| !alive.contains(k))
            .copied()
            .collect();
        for key in stale {
            self.remove_subscription(key);
        }
    }

    fn remove_subscription(&mut self, key: usize) {
        if let Some(subscription) = self.subscriptions.remove(&key) {
            let _ = subscription
                .session
                .RemoveMediaPropertiesChanged(subscription.properties_token);
            let _ = subscription
                .session
                .RemovePlaybackInfoChanged(subscription.playback_token);
        }
    }

    fn remove_all_subscriptions(&mut self) {
        let keys: Vec<usize> = self.subscriptions.keys().copied().collect();
        for key in keys {
            self.remove_subscription(key);
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

        // Held playback state: emit once its hold window expires. State changes
        // never wait for track completeness — a play/pause is delivered even
        // while artwork/album of a slow track is pending. Duplicate states for
        // the same source are deduplicated (sessions of an app are re-created
        // constantly, so the same state re-fires over and over).
        if let Some((_key, state, deadline, source)) = self.pending_playback.take() {
            if Instant::now() >= deadline && self.last_state_by_source.get(&source) != Some(&state) {
                self.last_state_by_source.insert(source.clone(), state);
                info!("playback state changed | state={state:?} | source={source}");
                let _ = self.output.send(MediaEvent::PlaybackStateChanged(state));
            } else {
                debug!("playback state suppressed | state={state:?} | source={source}");
            }
        }

        // Track: wait until complete before sending. Artwork must be ready, and a
        // freshly-pending track also gets a short unconditional grace window for
        // its album field (title/artist/artwork and album frequently arrive as
        // separate MediaPropertiesChanged events). Re-schedule every 100ms;
        // artwork waits up to 3s, album grace is fixed and short.
        let incomplete = self.pending_track.as_ref().is_some_and(|(_, track)| {
            let elapsed = self.track_pending_since.map(|t| t.elapsed()).unwrap_or(Duration::ZERO);
            track.artwork.is_none() || (track.album.trim().is_empty() && elapsed < album_grace())
        });
        if incomplete {
            if self.track_pending_since.is_none() {
                self.track_pending_since = Some(Instant::now());
            }
            let elapsed = self.track_pending_since.unwrap().elapsed();
            if elapsed < Duration::from_millis(3000) {
                self.pending_deadline = Some(Instant::now() + Duration::from_millis(100));
                return;
            }
        }
        self.track_pending_since = None;
        if let Some((key, track)) = self.pending_track.take() {
            let fingerprint = track_fingerprint(&track);
            let differs = self.last_content_fingerprint.as_ref() != Some(&fingerprint);
            // Do not re-emit when only the artwork disappeared (thumbnail reads
            // toggle): absence is already shown as a placeholder, and re-emitting
            // it would produce the repeated notifications seen in the logs.
            let artwork_only_removed = self.last_content_fingerprint.as_ref().is_some_and(|last| {
                last.has_artwork
                    && !fingerprint.has_artwork
                    && last.title == fingerprint.title
                    && last.artist == fingerprint.artist
                    && last.album == fingerprint.album
            });
            if differs && !artwork_only_removed {
                self.last_content_fingerprint = Some(fingerprint);
                self.last_track_emitted = Some((key, Instant::now()));
                let track_label = track_label(&track);
                info!("track changed | {track_label}");
                let _ = self.output.send(MediaEvent::TrackChanged(track));
            } else if artwork_only_removed {
                let label = track_label(&track);
                info!("track emit skipped | reason=artwork-removed | {label}");
            } else {
                let label = track_label(&track);
                debug!("track emit skipped | reason=duplicate-fingerprint | {label}");
            }
        }
    }
}

/// Every field that is actually displayed, for diagnosable logs.
fn track_label(track: &TrackInfo) -> String {
    let track_no = track
        .track_number
        .map(|n| format!("{n}/{}", track.track_count.unwrap_or(0)))
        .unwrap_or_else(|| "-".into());
    format!(
        "title={:?} | artist={:?} | album={:?} | artwork={} | duration={:?}s | track={track_no} | genre={:?} | source={:?}",
        track.title,
        track.artist,
        track.album,
        if track.artwork.is_some() { "yes" } else { "no" },
        track.duration_secs,
        track.genre,
        track.source_app,
    )
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

/// Registers CurrentSessionChanged, the event Windows fires when its internal
/// "current session" pointer moves (focus heuristics, a new app starting, etc.).
/// Session-list events alone do not cover those cases.
fn register_current_session_handler(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
    signal_tx: Sender<Signal>,
) -> Result<EventRegistrationToken> {
    let handler: TypedEventHandler<GlobalSystemMediaTransportControlsSessionManager, CurrentSessionChangedEventArgs> =
        TypedEventHandler::new(move |_, _| {
            let _ = signal_tx.send(Signal::Sessions);
            Ok(())
        });
    Ok(manager.CurrentSessionChanged(&handler)?)
}

fn session_key(session: &GlobalSystemMediaTransportControlsSession) -> usize {
    session.as_raw() as usize
}

fn read_source_app(session: &GlobalSystemMediaTransportControlsSession) -> String {
    session
        .SourceAppUserModelId()
        .map(|value| source_app_label(&value.to_string()))
        .unwrap_or_else(|_| "Media".to_string())
}

fn read_track_info(session: &GlobalSystemMediaTransportControlsSession) -> Result<TrackInfo> {
    let source_app = read_source_app(session);
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
    // Content fingerprint: title+artist+album plus artwork presence. Artwork
    // presence is part of the fingerprint so a thumbnail arriving on a later
    // event re-emits the track (the UI refreshes it in place) instead of being
    // deduplicated away forever.
    TrackFingerprint {
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        has_artwork: track.artwork.is_some(),
    }
}

fn debounce_duration(config: &Config) -> Duration {
    Duration::from_millis(config.behavior.debounce_ms.clamp(150, 250))
}

/// Grace window granted for the album field of a freshly-pending track. SMTC
/// often delivers title/artist/artwork and album as separate events, so this
/// fixed window lets the first track of a session pick up its album without
/// relying on history. Sources that never provide album flush after this delay.
fn album_grace() -> Duration {
    Duration::from_millis(400)
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

    #[test]
    fn album_grace_is_a_fixed_short_window() {
        // Short enough to bound browser latency, long enough to catch the
        // album event that typically follows artwork.
        assert_eq!(album_grace(), Duration::from_millis(400));
    }
}
