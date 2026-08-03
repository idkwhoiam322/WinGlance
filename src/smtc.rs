use crate::config::Config;
use crate::events::{MediaEvent, PlaybackState, TrackInfo};
use anyhow::{Context, Result, anyhow};
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
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
    config: Arc<RwLock<Config>>,
    output: Sender<MediaEvent>,
    signal_tx: Sender<Signal>,
    /// Every open session's event subscriptions, keyed by session pointer.
    subscriptions: HashMap<usize, SessionSubscription>,
    current_key: Option<usize>,
    recent_playing: Option<GlobalSystemMediaTransportControlsSession>,
    pending_track: Option<(usize, TrackInfo)>,
    pending_deadline: Option<Instant>,
    track_pending_since: Option<Instant>,
    last_content_fingerprint: Option<TrackFingerprint>,
    /// When the tracked session last reported Stopped. A handoff to a new
    /// session with identical content shortly after is the same song under a
    /// new session object (browsers re-create their session per track/tab),
    /// not a change.
    last_stop_at: Option<Instant>,
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
    /// Session keys that have ever produced readable track metadata (non-empty
    /// title). A Paused session is only current-eligible when it has real
    /// content; placeholder sessions (e.g. a client churning empty sessions)
    /// must never displace an actively playing one.
    known_content: HashSet<usize>,
    /// Session keys recently removed from the subscription map (churned out),
    /// with the removal time, so a re-added key is recognized and re-read.
    recently_removed: HashMap<usize, Instant>,
    /// Session-creation counts per source app within a rolling window, for the
    /// churn cool-down.
    churn: HashMap<String, VecDeque<Instant>>,
    /// Source apps currently on cool-down (their sessions are not
    /// current-eligible) until the stored time.
    churn_cooldown: HashMap<String, Instant>,
    /// A SessionsChanged burst is pending its debounce window; the next flush
    /// performs the re-sync + re-resolve once per burst instead of per event.
    sessions_pending: bool,
    /// Last observed timeline position per tracked session, for restart
    /// detection (a position collapse to ~0 on unchanged content).
    last_position: Option<(usize, Duration)>,
    /// Heartbeat touched each loop iteration so the supervisor can detect a
    /// stall and restart the listener.
    heartbeat: Arc<Mutex<Instant>>,
}

/// Window after the tracked session stops in which a handoff to a new session
/// with identical title/artist is treated as the same content continuing under
/// a new session object instead of a change. Browsers re-create their session
/// within this window in practice; a real replay of the same song later is not
/// suppressed.
const HANDOFF_SUPPRESS_MS: u64 = 2000;
/// Rolling window, threshold and cool-down for the per-source session-churn
/// guard. A source creating more than `CHURN_THRESHOLD` new sessions within
/// `CHURN_WINDOW_MS` (a real client was observed doing ~20 in 8.5s) is
/// excluded from current-session resolution for the cool-down period.
const CHURN_WINDOW_MS: u64 = 2000;
const CHURN_THRESHOLD: usize = 5;
const CHURN_COOLDOWN_MS: u64 = 30_000;
/// How long a removed session key stays "recently removed", so re-adding it
/// triggers a proactive metadata re-read.
const RESUBSCRIBE_WINDOW_MS: u64 = 10_000;
/// Upper bound for the album/artwork caches; beyond it they are cleared (the
/// caches are a convenience, not critical state).
const CACHE_CAP: usize = 16;

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
    config: Arc<RwLock<Config>>,
    /// Updated by the event loop every few seconds so a supervisor can detect
    /// a stalled worker (a WinRT call hanging under session churn) and
    /// restart the listener.
    heartbeat: Arc<Mutex<Instant>>,
}

impl SmtcListener {
    pub fn new(output: Sender<MediaEvent>, config: Arc<RwLock<Config>>, heartbeat: Arc<Mutex<Instant>>) -> Self {
        Self {
            output,
            config,
            heartbeat,
        }
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
        let mut state = ListenerState::new(manager, self.config, self.output, signal_tx, self.heartbeat);

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
        config: Arc<RwLock<Config>>,
        output: Sender<MediaEvent>,
        signal_tx: Sender<Signal>,
        heartbeat: Arc<Mutex<Instant>>,
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
            pending_deadline: None,
            track_pending_since: None,
            last_content_fingerprint: None,
            last_stop_at: None,
            last_session_check: Instant::now(),
            current_playing: false,
            album_cache: HashMap::new(),
            artwork_cache: HashMap::new(),
            known_content: HashSet::new(),
            recently_removed: HashMap::new(),
            churn: HashMap::new(),
            churn_cooldown: HashMap::new(),
            sessions_pending: false,
            last_position: None,
            heartbeat,
        }
    }

    fn event_loop(&mut self, signal_rx: Receiver<Signal>) -> Result<()> {
        let session_check_interval = Duration::from_secs(2);
        loop {
            *self.heartbeat.lock().unwrap() = Instant::now();
            let timeout = self
                .pending_deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_secs(24 * 60 * 60))
                // Wake at least every 5s so the heartbeat stays fresh even
                // when nothing is pending.
                .min(Duration::from_secs(5));

            match signal_rx.recv_timeout(timeout) {
                Ok(signal) => self.handle_signal(signal)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.flush_pending();
                    // Periodically re-check sessions to catch missed changes. emit_initial=true
                    // so a newly-detected current session reports its track+state immediately.
                    // Always re-sync subscriptions first so sessions created outside a
                    // SessionsChanged burst (e.g. a browser tab that just started a video)
                    // are discovered and subscribed before the resolve.
                    if self.last_session_check.elapsed() >= session_check_interval {
                        self.last_session_check = Instant::now();
                        self.sync_subscriptions();
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
                // A session storm (one app recreating its SMTC session many
                // times a second) fires these in bursts. Debounce: collapse a
                // burst into one re-sync + re-resolve at the next flush.
                if self.sessions_pending {
                    debug!("SMTC SessionsChanged/CurrentSessionChanged (coalesced)");
                } else {
                    self.sessions_pending = true;
                    debug!("SMTC SessionsChanged/CurrentSessionChanged (debounced)");
                }
                let deadline = Instant::now() + debounce_duration(&self.config.read().unwrap());
                self.pending_deadline = Some(self.pending_deadline.map_or(deadline, |d| d.min(deadline)));
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
                    self.remember_content(key, &track);
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
                    // A track change is imminent: the new track notification
                    // will carry the new state. Flush pending track.
                    self.schedule_flush();
                }
            }
            Signal::PlaybackInfo(session) => {
                let key = session_key(&session);
                if self.current_key == Some(key) {
                    let source = read_source_app(&session);
                    if let Some(state) = read_playback_state(&session)? {
                        if state == PlaybackState::Stopped {
                            // The tracked session stopped: hand off to whatever else
                            // is playing right now instead of waiting for the poll.
                            self.last_stop_at = Some(Instant::now());
                            let before = self.current_key;
                            self.refresh_current_session(None, true, false)?;
                            if self.current_key == before {
                                // Nothing else took over — report the stop.
                                self.current_playing = false;
                                self.emit_playback_state(state, source.clone());
                            }
                            return Ok(());
                        }
                        if state == PlaybackState::Playing {
                            self.recent_playing = Some(session.clone());
                        }
                        self.current_playing = state == PlaybackState::Playing;
                        self.detect_restart(&session, key);
                        self.emit_playback_state(state, source);
                    }
                }
                // Non-current session: ignore entirely. We track only the
                // current session.
            }
        }
        Ok(())
    }

    /// Detects a restart of the current track (Prev button, or a looping
    /// track): the timeline position collapses to ~0 while the content is
    /// unchanged. Re-shows the pill briefly via TrackRestarted instead of
    /// deduplicating the restart away.
    fn detect_restart(&mut self, session: &GlobalSystemMediaTransportControlsSession, key: usize) {
        let position = read_position(session);
        let restarted = self.last_position.as_ref().is_some_and(|(last_key, last_pos)| {
            *last_key == key
                && *last_pos > Duration::from_secs(3)
                && position.is_some_and(|p| p < Duration::from_secs(1))
        });
        self.last_position = position.map(|p| (key, p));
        if !restarted {
            return;
        }
        // A new track also starts at position ~0; only treat this as a restart
        // when we are still showing this exact content.
        if let Ok(mut track) = read_track_info(session) {
            let same_content = self
                .last_content_fingerprint
                .as_ref()
                .is_some_and(|last| track.title == last.title && track.artist == last.artist);
            if same_content {
                self.apply_cache(&mut track);
                debug!(
                    "track restart detected | title={:?} | artist={:?}",
                    track.title, track.artist
                );
                let _ = self.output.send(MediaEvent::TrackRestarted(track));
            }
        }
    }

    /// Emits a playback state change immediately, with no hold or deduplication
    /// — every state change is delivered so the history never misses one.
    fn emit_playback_state(&mut self, state: PlaybackState, source: String) {
        info!("playback state changed | state={state:?} | source={source}");
        let _ = self.output.send(MediaEvent::PlaybackStateChanged(state));
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

    /// Marks a session as having produced real content, making it eligible as
    /// current even while paused.
    fn remember_content(&mut self, key: usize, track: &TrackInfo) {
        if !track.title.trim().is_empty() {
            self.known_content.insert(key);
        }
    }

    /// True when a session that was just resolved carries the content we are
    /// already showing, right after the previous session stopped. Browsers
    /// re-create their SMTC session per track/tab, so this is the same song
    /// continuing under a new session object — not a change to report.
    fn handoff_is_same_content(&self, track: &TrackInfo) -> bool {
        let Some(last) = &self.last_content_fingerprint else {
            return false;
        };
        let Some(stopped_at) = self.last_stop_at else {
            return false;
        };
        if stopped_at.elapsed() > Duration::from_millis(HANDOFF_SUPPRESS_MS) {
            return false;
        }
        is_same_content(track, last)
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
                if playback != Some(PlaybackState::Stopped) {
                    match read_track_info(&session) {
                        Ok(mut track) => {
                            self.apply_cache(&mut track);
                            self.remember_content(session_key(&session), &track);
                            // Same-content handoff: adopt the re-created session
                            // silently (the pill keeps tracking it) and let its
                            // property events emit only when the content differs.
                            if self.handoff_is_same_content(&track) {
                                if playback == Some(PlaybackState::Playing) {
                                    self.recent_playing = Some(session.clone());
                                }
                                debug!(
                                    "session handoff, content unchanged | source={}",
                                    read_source_app(&session)
                                );
                                return Ok(());
                            }
                            self.pending_track = Some((session_key(&session), track));
                            self.schedule_flush();
                        }
                        Err(_) => {
                            // The new session's metadata is not readable yet
                            // (transitional state). Announce the source app so
                            // the pill never shows the previous session's track
                            // for this adoption; the real metadata replaces it
                            // when MediaProperties arrives.
                            let key = session_key(&session);
                            let source = read_source_app(&session);
                            self.pending_track = Some((
                                key,
                                TrackInfo {
                                    title: source.clone(),
                                    source_app: source,
                                    ..TrackInfo::default()
                                },
                            ));
                            self.schedule_flush();
                        }
                    }
                }
                if let Some(state) = playback {
                    if state == PlaybackState::Playing {
                        self.recent_playing = Some(session.clone());
                    }
                    self.current_playing = state == PlaybackState::Playing;
                    self.emit_playback_state(state, read_source_app(&session));
                }
            }
        }
        Ok(())
    }

    /// Whether a session may become (or keep) current: actively playing, or
    /// paused but with real content this run has seen. A placeholder session
    /// that is perpetually Paused with no metadata (a client churning empty
    /// sessions) is never eligible, regardless of what GetCurrentSession()
    /// reports. Disallowed and cool-down sources are excluded outright.
    fn session_is_eligible(&self, session: &GlobalSystemMediaTransportControlsSession) -> bool {
        if !self.session_source_allowed(session) {
            return false;
        }
        match read_playback_state(session) {
            Ok(Some(PlaybackState::Playing)) => true,
            Ok(Some(PlaybackState::Paused)) => self.known_content.contains(&session_key(session)),
            _ => false,
        }
    }

    /// Whether a session's source app is excluded: on the churn cool-down, or
    /// not matching the user's `allowed_sources` config. When `allowed_sources`
    /// is empty, all sources are allowed. When non-empty, only sources matching
    /// an entry (case-insensitive substring against the AUMID and its derived
    /// label, after normalizing word-boundary characters) are allowed;
    /// everything else is excluded.
    fn session_source_allowed(&self, session: &GlobalSystemMediaTransportControlsSession) -> bool {
        let aumid = session
            .SourceAppUserModelId()
            .map(|value| value.to_string())
            .unwrap_or_default();
        let label = source_app_label(&aumid);
        if self.source_on_cooldown(&label) {
            debug!("SMTC session rejected (cooldown) | aumid={} | label={}", aumid, label);
            return false;
        }
        let allowed = self.config.read().unwrap().behavior.allowed_sources.clone();
        if allowed.is_empty() {
            return true;
        }
        let naumid = normalize_for_match(&aumid);
        let nlabel = normalize_for_match(&label);
        let result = allowed.iter().any(|pattern| {
            let np = normalize_for_match(pattern);
            naumid.contains(&np) || nlabel.contains(&np)
        });
        debug!(
            "SMTC session {} | aumid={} | label={} | allowed_sources={:?}",
            if result { "accepted" } else { "rejected" },
            aumid,
            label,
            allowed
        );
        result
    }

    /// True while a source app is on the churn cool-down.
    fn source_on_cooldown(&self, source: &str) -> bool {
        self.churn_cooldown
            .get(source)
            .is_some_and(|until| *until > Instant::now())
    }

    /// Counts a newly-created session for its source; trips the cool-down once
    /// the threshold is exceeded within the window, logging a WARN so the log
    /// explains the exclusion without manual analysis.
    fn record_churn(&mut self, source: &str) {
        let now = Instant::now();
        let events = self.churn.entry(source.to_string()).or_default();
        events.push_back(now);
        while events
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_millis(CHURN_WINDOW_MS))
        {
            events.pop_front();
        }
        if events.len() >= CHURN_THRESHOLD && !self.source_on_cooldown(source) {
            self.churn_cooldown
                .insert(source.to_string(), now + Duration::from_millis(CHURN_COOLDOWN_MS));
            warn!(
                "source {source} is churning sessions ({CHURN_THRESHOLD}+ new sessions in {CHURN_WINDOW_MS}ms); excluding it from current-session resolution for {CHURN_COOLDOWN_MS}ms"
            );
        }
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
            && self.session_is_eligible(session)
        {
            // Don't let a non-Playing session steal the current slot from
            // one that is actively playing.  A paused background session with
            // known content (e.g. YouTube Music sitting minimized while Brave
            // plays in the foreground) must not displace the foreground app.
            let hint_is_current = self.current_key == Some(session_key(session));
            let hint_is_playing = read_playback_state(session)
                .ok()
                .flatten()
                .is_some_and(|s| s == PlaybackState::Playing);
            if hint_is_current || !self.current_playing || hint_is_playing {
                return Some(session.clone());
            }
        }

        // 1. GetCurrentSession() is the pointer Windows itself maintains (the
        //    native media widget follows it); consult it fresh on every resolve.
        //    It must be eligible, not merely "not Stopped": a Paused, empty
        //    placeholder session must never displace one that is playing.
        if let Ok(session) = self.manager.GetCurrentSession()
            && self.session_is_eligible(&session)
        {
            return Some(session);
        }

        // 2. The session that caused the event, when it is eligible.
        if let Some(session) = hint
            && self.session_is_eligible(session)
        {
            return Some(session.clone());
        }

        // 3. The last observed playing session (keeps the current one stable).
        if let Some(session) = self.recent_playing.clone()
            && self.session_is_eligible(&session)
        {
            return Some(session);
        }

        // 4. Any eligible session.
        if let Ok(sessions) = self.manager.GetSessions()
            && let Some(playing) = sessions.into_iter().find(|s| self.session_is_eligible(s))
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

    /// Subscribes to a session unless it is already subscribed. A key that
    /// churned out and back (recently removed) is re-read proactively: it may
    /// have changed its metadata while briefly unsubscribed, and re-subscribing
    /// alone would wait for the next event that may never come.
    fn ensure_subscribed(&mut self, session: &GlobalSystemMediaTransportControlsSession) -> Result<()> {
        let key = session_key(session);
        if self.subscriptions.contains_key(&key) {
            return Ok(());
        }
        let was_removed_recently = self.recently_removed.remove(&key).is_some();
        let subscription = self.subscribe(session)?;
        self.subscriptions.insert(key, subscription);
        if was_removed_recently && self.current_key == Some(key) {
            debug!("resubscribed session {key}; re-reading its state");
            if let Ok(Some(state)) = read_playback_state(session)
                && state != PlaybackState::Stopped
            {
                if let Ok(mut track) = read_track_info(session) {
                    self.apply_cache(&mut track);
                    self.remember_content(key, &track);
                    self.pending_track = Some((key, track));
                    self.schedule_flush();
                } else {
                    self.emit_playback_state(state, read_source_app(session));
                }
            }
        }
        Ok(())
    }

    /// Re-syncs the subscription map with the current session list: subscribes
    /// to every open (non-ignored) session, drops subscriptions for removed
    /// sessions, and accounts per-source session churn for the cool-down.
    fn sync_subscriptions(&mut self) {
        let Some(sessions) = self.manager.GetSessions().ok() else {
            return;
        };
        let sessions: Vec<_> = sessions.into_iter().collect();
        let before: HashSet<usize> = self.subscriptions.keys().copied().collect();
        for session in &sessions {
            if !self.session_source_allowed(session) {
                continue;
            }
            if !before.contains(&session_key(session)) {
                self.record_churn(&read_source_app(session));
            }
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
        self.churn_cooldown.retain(|_, until| *until > Instant::now());
        self.recently_removed
            .retain(|_, at| at.elapsed() < Duration::from_millis(RESUBSCRIBE_WINDOW_MS));
        self.known_content.retain(|key| self.subscriptions.contains_key(key));
    }

    fn remove_subscription(&mut self, key: usize) {
        if let Some(subscription) = self.subscriptions.remove(&key) {
            self.recently_removed.insert(key, Instant::now());
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
        self.pending_deadline = Some(Instant::now() + debounce_duration(&self.config.read().unwrap()));
    }

    fn flush_pending(&mut self) {
        self.pending_deadline = None;

        // Debounced session-list changes: one re-sync + re-resolve per burst
        // instead of one per SessionsChanged/CurrentSessionChanged event.
        if self.sessions_pending {
            self.sessions_pending = false;
            self.sync_subscriptions();
            if let Err(error) = self.refresh_current_session(None, true, false) {
                debug!("session re-sync after burst failed: {error:#}");
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
        if let Some((_, track)) = self.pending_track.take() {
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
    // Keep artist empty when the app has not provided it yet; the pill hides
    // the artist row instead of showing "Unknown" (which duplicates the
    // source-app line and shows a made-up name).
    let artist = non_empty(properties.Artist()?.to_string(), "");
    // Keep album empty when the app has not provided it yet; renderers hide the
    // album line until real data arrives (prevents a bogus "Unknown album").
    let album = non_empty(properties.AlbumTitle()?.to_string(), "");
    // Artwork reads fail transiently under heavy session churn (overlapping
    // async WinRT calls on one thread); retry once before giving up, and log
    // which call failed with its raw HRESULT.
    let artwork = match read_artwork(&properties) {
        Ok(artwork) => artwork,
        Err(first) => {
            debug!("album-art read failed (attempt 1): {first:#}");
            match read_artwork(&properties) {
                Ok(artwork) => artwork,
                Err(second) => {
                    debug!("album-art read failed (attempt 2): {second:#}");
                    None
                }
            }
        }
    };
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

/// Current timeline position of a session, for restart detection. TimeSpan
/// durations are in 100-nanosecond units.
fn read_position(session: &GlobalSystemMediaTransportControlsSession) -> Option<Duration> {
    let position_100ns = session.GetTimelineProperties().ok()?.Position().ok()?.Duration;
    if position_100ns <= 0 {
        Some(Duration::ZERO)
    } else {
        Some(Duration::from_nanos((position_100ns * 100) as u64))
    }
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
    let reference = properties
        .Thumbnail()
        .map_err(|e| anyhow!("Thumbnail failed: {e:?} (hr=0x{:08X})", e.code().0 as u32))?;
    let stream = reference
        .OpenReadAsync()
        .map_err(|e| anyhow!("OpenReadAsync failed: {e:?} (hr=0x{:08X})", e.code().0 as u32))?
        .get()
        .map_err(|e| anyhow!("OpenReadAsync get failed: {e:?} (hr=0x{:08X})", e.code().0 as u32))?;
    let size = stream
        .Size()
        .map_err(|e| anyhow!("Size failed: {e:?} (hr=0x{:08X})", e.code().0 as u32))?;
    if size == 0 || size > 8 * 1024 * 1024 || size > u32::MAX as u64 {
        return Ok(None);
    }
    let size = size as u32;
    let buffer =
        Buffer::Create(size).map_err(|e| anyhow!("Buffer::Create failed: {e:?} (hr=0x{:08X})", e.code().0 as u32))?;
    stream
        .ReadAsync(&buffer, size, InputStreamOptions::None)
        .map_err(|e| anyhow!("ReadAsync failed: {e:?} (hr=0x{:08X})", e.code().0 as u32))?
        .get()
        .map_err(|e| anyhow!("ReadAsync get failed: {e:?} (hr=0x{:08X})", e.code().0 as u32))?;
    let reader = DataReader::FromBuffer(&buffer)
        .map_err(|e| anyhow!("DataReader::FromBuffer failed: {e:?} (hr=0x{:08X})", e.code().0 as u32))?;
    let mut data = vec![0u8; size as usize];
    reader
        .ReadBytes(&mut data)
        .map_err(|e| anyhow!("ReadBytes failed: {e:?} (hr=0x{:08X})", e.code().0 as u32))?;
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
    let value = if value.contains('.') {
        value.rsplit('.').next().unwrap_or(value)
    } else {
        value
    };
    non_empty(value.to_string(), "Media")
}

/// Normalizes a string for fuzzy matching against AUMIDs and derived labels.
/// Strips common word-boundary characters (`-`, `_`, `.`, ` `) and lowercases,
/// so that `"youtube music"` matches `"youtube-music"` in an AUMID like
/// `com.github.th-ch.youtube-music`.
fn normalize_for_match(s: &str) -> String {
    s.to_lowercase().replace(['-', '_', '.', ' '], "")
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

/// Whether a freshly-read track is the content we are already showing. Used to
/// recognize a session handoff as no change. Title+artist equality, exact.
fn is_same_content(track: &TrackInfo, last: &TrackFingerprint) -> bool {
    track.title == last.title && track.artist == last.artist
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
        assert_eq!(source_app_label("com.github.th-ch.youtube-music"), "youtube-music");
        assert_eq!(source_app_label("com.riotgames.RiotGames.RiotClient"), "RiotClient");
    }

    #[test]
    fn normalize_for_match_strips_word_boundaries() {
        assert_eq!(normalize_for_match("youtube music"), "youtubemusic");
        assert_eq!(normalize_for_match("youtube-music"), "youtubemusic");
        assert_eq!(normalize_for_match("YouTube.Music"), "youtubemusic");
        assert_eq!(normalize_for_match("YOUTUBE_MUSIC"), "youtubemusic");
        assert_eq!(
            normalize_for_match("com.github.th-ch.youtube-music"),
            "comgithubthchyoutubemusic"
        );
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

    #[test]
    fn same_content_matches_title_and_artist_only() {
        let track = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            ..TrackInfo::default()
        };
        let last = TrackFingerprint {
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            has_artwork: true,
        };
        assert!(is_same_content(&track, &last));
        // A different artist is a real change.
        let other = TrackInfo {
            title: "Song".into(),
            artist: "Other".into(),
            ..TrackInfo::default()
        };
        assert!(!is_same_content(&other, &last));
        // A momentarily blank artist must not match (fall back to the normal
        // path; the fingerprint dedup is the backstop).
        let blank = TrackInfo {
            title: "Song".into(),
            artist: String::new(),
            ..TrackInfo::default()
        };
        assert!(!is_same_content(&blank, &last));
    }
}
