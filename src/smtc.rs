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
    /// Fired by SessionsChanged or CurrentSessionChanged: re-sync the
    /// subscription map at the next flush (one re-sync per burst).
    Sessions,
    MediaProperties(GlobalSystemMediaTransportControlsSession),
    PlaybackInfo(GlobalSystemMediaTransportControlsSession),
}

struct SessionSubscription {
    session: GlobalSystemMediaTransportControlsSession,
    properties_token: EventRegistrationToken,
    playback_token: EventRegistrationToken,
}

/// The last known displayed state of one (source, session). Every field is a
/// field the pill can show; a fresh read is merged into this, diffed against
/// it, and only the fields that actually changed are emitted. There is no
/// "current session": every subscribed session is tracked independently, so
/// simultaneous sources never displace each other.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct LogicalState {
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    subtitle: String,
    has_artwork: bool,
    source_app: String,
    duration_secs: Option<u64>,
    track_number: Option<u32>,
    track_count: Option<u32>,
    genre: Option<String>,
    playback: Option<PlaybackState>,
    /// When the first read was deferred waiting for artwork: the pill shows
    /// anyway once this timestamp is older than `ARTWORK_TIMEOUT`.
    deferred_at: Option<Instant>,
}

/// Rolling window, threshold and cool-down for the per-source session-churn
/// guard. A source creating more than `CHURN_THRESHOLD` new sessions within
/// `CHURN_WINDOW_MS` (a real client was observed doing ~20 in 8.5s) is
/// excluded from tracking for the cool-down period.
const CHURN_WINDOW_MS: u64 = 2000;
const CHURN_THRESHOLD: usize = 5;
const CHURN_COOLDOWN_MS: u64 = 30_000;

/// Maximum time a first-read pill waits for artwork before showing anyway.
/// SMTC populates the thumbnail a moment after the title (observed ~500ms),
/// so a source that never provides one still gets its pill after this.
const ARTWORK_TIMEOUT: Duration = Duration::from_secs(2);

struct ListenerState {
    manager: GlobalSystemMediaTransportControlsSessionManager,
    config: Arc<RwLock<Config>>,
    output: Sender<MediaEvent>,
    signal_tx: Sender<Signal>,
    /// Every open session's event subscriptions, keyed by session pointer.
    subscriptions: HashMap<usize, SessionSubscription>,
    /// Last known displayed state per session key.
    states: HashMap<usize, LogicalState>,
    /// Keys with unprocessed property events in the current tick. The flush
    /// reads each key once, so a burst of events for one session coalesces
    /// into one read + one diff + one emit per debounce window.
    dirty: HashSet<usize>,
    /// A SessionsChanged burst is pending its debounce window; the next flush
    /// performs the re-sync once per burst instead of once per event.
    sessions_pending: bool,
    /// Debounce deadline for pending dirty keys and session bursts.
    pending_deadline: Option<Instant>,
    /// Last time the periodic safety-net poll ran.
    last_session_check: Instant,
    /// Session-creation counts per source app within a rolling window, for the
    /// churn cool-down.
    churn: HashMap<String, VecDeque<Instant>>,
    /// Source apps currently on cool-down (their sessions are not tracked)
    /// until the stored time.
    churn_cooldown: HashMap<String, Instant>,
    /// Keys of rejected sessions already reported to the history, so a
    /// rejected session is logged once per appearance instead of on every
    /// re-sync (the 2-second poll re-lists all sessions).
    rejected_seen: HashSet<usize>,
    /// Heartbeat touched each loop iteration so the supervisor can detect a
    /// stall and restart the listener.
    heartbeat: Arc<Mutex<Instant>>,
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
        // Initial read: report what is already playing so the pill does not
        // wait for the first event.
        state.poll_sessions();
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
            states: HashMap::new(),
            dirty: HashSet::new(),
            sessions_pending: false,
            pending_deadline: None,
            last_session_check: Instant::now(),
            churn: HashMap::new(),
            churn_cooldown: HashMap::new(),
            rejected_seen: HashSet::new(),
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
                .unwrap_or(Duration::from_secs(5))
                // Wake at least every 5s so the heartbeat stays fresh even
                // when nothing is pending.
                .min(Duration::from_secs(5));

            match signal_rx.recv_timeout(timeout) {
                Ok(signal) => self.handle_signal(signal)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.flush();
                    // Periodic safety net: re-sync (a session can appear
                    // without a SessionsChanged event) and re-read every
                    // subscribed session (metadata only, no artwork) so a
                    // missed event still surfaces.
                    if self.last_session_check.elapsed() >= session_check_interval {
                        self.last_session_check = Instant::now();
                        self.sync_subscriptions();
                        self.poll_sessions();
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
                // burst into one re-sync at the next flush.
                if self.sessions_pending {
                    debug!("SMTC SessionsChanged/CurrentSessionChanged (coalesced)");
                } else {
                    self.sessions_pending = true;
                    debug!("SMTC SessionsChanged/CurrentSessionChanged (debounced)");
                }
                self.schedule_flush();
            }
            Signal::MediaProperties(session) | Signal::PlaybackInfo(session) => {
                let key = session_key(&session);
                if !self.subscriptions.contains_key(&key) {
                    // An event for a session we are not tracking (it appeared
                    // between syncs): subscribe now so its state is tracked.
                    if let Err(error) = self.ensure_subscribed(&session) {
                        debug!("subscribe failed for session {key}: {error:#}");
                        return Ok(());
                    }
                }
                // Coalesce per key: a burst of MediaProperties/PlaybackInfo
                // events for the same session is resolved once at the flush.
                self.dirty.insert(key);
                self.schedule_flush();
            }
        }
        Ok(())
    }

    /// Resolves everything pending at the deadline: a debounced session burst
    /// (one re-sync) and each dirty key (one read + one diff + one emit).
    fn flush(&mut self) {
        self.pending_deadline = None;
        if self.sessions_pending {
            self.sessions_pending = false;
            self.sync_subscriptions();
        }
        if !self.dirty.is_empty() {
            let keys: Vec<usize> = self.dirty.drain().collect();
            for key in keys {
                // Clone the COM interface out so the map borrow ends before the
                // refresh (which can mutate subscriptions via eviction).
                let session = self.subscriptions.get(&key).map(|s| s.session.clone());
                if let Some(session) = session
                    && let Err(error) = self.refresh_session(&session, true)
                {
                    debug!("refresh failed for session {key}: {error:#}");
                }
            }
        }
    }

    /// Reads a session's current state, merges it into the stored logical
    /// state, and emits an event for every field that actually changed.
    /// `read_artwork` is false for the periodic safety-net poll, which must
    /// not re-read (or clear) artwork: it only catches missed content and
    /// playback changes.
    fn refresh_session(
        &mut self,
        session: &GlobalSystemMediaTransportControlsSession,
        read_artwork: bool,
    ) -> Result<()> {
        let key = session_key(session);
        if !self.session_source_allowed(session) {
            return Ok(());
        }
        let status = session.GetPlaybackInfo()?.PlaybackStatus()?;
        if status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Closed {
            // Closed does not go through the diff/emit path: it usually fires
            // as the app quits, right after a Stopped/Paused already told the
            // user what happened, so the entry is evicted immediately and
            // nothing is emitted.
            debug!("SMTC session closed | key={key} | source={}", read_source_app(session));
            self.evict(key);
            return Ok(());
        }
        let playback = match status {
            GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing => Some(PlaybackState::Playing),
            GlobalSystemMediaTransportControlsSessionPlaybackStatus::Paused => Some(PlaybackState::Paused),
            GlobalSystemMediaTransportControlsSessionPlaybackStatus::Stopped => Some(PlaybackState::Stopped),
            // Opened/Changing and unknown statuses are transitional: ignored.
            _ => None,
        };
        let prev = self.states.get(&key).cloned().unwrap_or_default();
        let mut next = prev.clone();
        let mut events: Vec<MediaEvent> = Vec::new();

        // Playback is a normal diffable field: Stopped goes through the same
        // path as Playing/Paused and can produce a pill like any other real
        // transition. Transitional statuses leave the stored state untouched.
        if playback != prev.playback
            && let Some(state) = playback
        {
            next.playback = Some(state);
            info!(
                "playback state changed | state={state:?} | source={}",
                read_source_app(session)
            );
            events.push(MediaEvent::PlaybackStateChanged(state, read_source_app(session)));
        }

        // Content is only diffed while the session is not stopped; a stopped
        // session keeps its stored content (the pill shows the last track).
        if status != GlobalSystemMediaTransportControlsSessionPlaybackStatus::Stopped {
            match read_track_info(session, read_artwork) {
                Ok(read) => {
                    let merged = merge_track(&prev, &read, read_artwork);
                    let (mut emit, artwork_lost) = emit_track(&prev, &merged, read_artwork);
                    // Safety net: a first pill deferred for artwork shows
                    // anyway after ARTWORK_TIMEOUT, so a source that never
                    // provides a thumbnail still gets its pill.
                    if !emit && defer_expired(prev.deferred_at) {
                        emit = true;
                        let label = track_label(&merged);
                        debug!("track emit forced | reason=artwork-timeout | {label}");
                    }
                    if emit {
                        let label = track_label(&merged);
                        info!("track changed | {label}");
                        events.push(MediaEvent::TrackChanged(merged.clone()));
                        next.deferred_at = None;
                    } else if artwork_lost {
                        // Absence is already shown as a placeholder: store the
                        // loss (a later reappearance re-emits) without
                        // flashing the same track again.
                        let label = track_label(&merged);
                        debug!("track emit skipped | reason=artwork-removed | {label}");
                    } else {
                        let is_first_read = prev.source_app.is_empty() && prev.title.is_empty();
                        if is_first_read && read_artwork && merged.artwork.is_none() {
                            next.deferred_at = Some(Instant::now());
                            let label = track_label(&merged);
                            debug!("track emit deferred | reason=awaiting-artwork | {label}");
                        } else if read_artwork {
                            // Event-driven reads only: the 2-second poll re-reads
                            // every session and must not log a duplicate per pass.
                            let label = track_label(&merged);
                            debug!("track emit skipped | reason=duplicate | {label}");
                        }
                    }
                    next.title = merged.title;
                    next.artist = merged.artist;
                    next.album = merged.album;
                    next.album_artist = merged.album_artist;
                    next.subtitle = merged.subtitle;
                    next.has_artwork = if read_artwork {
                        merged.artwork.is_some()
                    } else {
                        prev.has_artwork
                    };
                    next.source_app = merged.source_app;
                    next.duration_secs = merged.duration_secs;
                    next.track_number = merged.track_number;
                    next.track_count = merged.track_count;
                    next.genre = merged.genre;
                }
                Err(error) => {
                    if is_session_gone(&error) {
                        debug!("session torn down during read | key={key} | {error:#}");
                    } else {
                        debug!("track read failed | key={key} | {error:#}");
                    }
                }
            }
        }

        // A TrackChanged already surfaces the new content in the pill. A
        // simultaneous PlaybackStateChanged reported by the same session
        // refresh — Playing from a freshly adopted session, or Paused/Stopped
        // from the previous session during a session switch — is redundant,
        // and showing both would flash two pills for one transition. The
        // TrackChanged alone is enough; the state the pill shows is implied by
        // the fresh track.
        if events.iter().any(|e| matches!(e, MediaEvent::TrackChanged(_))) {
            events.retain(|e| !matches!(e, MediaEvent::PlaybackStateChanged(_, _)));
        }

        self.states.insert(key, next);
        for event in events {
            let _ = self.output.send(event);
        }
        Ok(())
    }

    /// Re-syncs the subscription map with the current session list: subscribes
    /// to every open session from an allowed source, drops subscriptions and
    /// stored state for sessions that disappeared, and accounts per-source
    /// session churn for the cool-down.
    fn sync_subscriptions(&mut self) {
        let Ok(sessions) = self.manager.GetSessions() else {
            debug!("SMTC GetSessions failed; keeping the current subscription map");
            return;
        };
        let sessions: Vec<_> = sessions.into_iter().collect();
        let before: HashSet<usize> = self.subscriptions.keys().copied().collect();
        for session in &sessions {
            let key = session_key(session);
            let allowed = self.session_source_allowed(session);
            debug!(
                "SMTC session {} | key={key} | source={} | allowed_sources={:?}",
                if allowed { "accepted" } else { "rejected" },
                read_source_app(session),
                self.config.read().unwrap().behavior.allowed_sources
            );
            if !allowed {
                // Log rejected sessions once per appearance so the history
                // shows every media source, not just the tracked ones.
                if self.rejected_seen.insert(key) {
                    let source_app = read_source_app(session);
                    let (title, artist) = read_session_text(session, &source_app);
                    let state = read_session_state(session);
                    let _ = self.output.send(MediaEvent::SessionRejected {
                        source_app,
                        title,
                        artist,
                        state,
                        accepted: false,
                    });
                }
                continue;
            }
            // A previously-rejected session that became allowed (config edit)
            // should re-report as accepted on its next rejection, if any.
            self.rejected_seen.remove(&key);
            if !before.contains(&key) {
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
            debug!("SMTC session disappeared | key={key}");
            self.evict(key);
        }
        // Forget rejected sessions that vanished so a later reappearance is
        // reported again.
        self.rejected_seen.retain(|key| alive.contains(key));
        self.churn_cooldown.retain(|_, until| *until > Instant::now());
    }

    /// Re-reads every subscribed session (metadata only, no artwork) and diffs
    /// it against the stored state. The 2-second safety net; also used for the
    /// startup read so the pill reports what is already playing.
    fn poll_sessions(&mut self) {
        let keys: Vec<usize> = self.subscriptions.keys().copied().collect();
        for key in keys {
            // Clone the COM interface out so the map borrow ends before the
            // refresh (which can mutate subscriptions via eviction).
            let session = self.subscriptions.get(&key).map(|s| s.session.clone());
            if let Some(session) = session
                && let Err(error) = self.refresh_session(&session, false)
            {
                debug!("poll refresh failed for session {key}: {error:#}");
            }
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
            return false;
        }
        let allowed = self.config.read().unwrap().behavior.allowed_sources.clone();
        if allowed.is_empty() {
            return true;
        }
        let naumid = normalize_for_match(&aumid);
        let nlabel = normalize_for_match(&label);
        allowed.iter().any(|pattern| {
            let np = normalize_for_match(pattern);
            naumid.contains(&np) || nlabel.contains(&np)
        })
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
                "source {source} is churning sessions ({CHURN_THRESHOLD}+ new sessions in {CHURN_WINDOW_MS}ms); excluding it from tracking for {CHURN_COOLDOWN_MS}ms"
            );
        }
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
        Ok(SessionSubscription {
            session: session.clone(),
            properties_token,
            playback_token,
        })
    }

    fn ensure_subscribed(&mut self, session: &GlobalSystemMediaTransportControlsSession) -> Result<()> {
        let key = session_key(session);
        if self.subscriptions.contains_key(&key) {
            return Ok(());
        }
        let subscription = self.subscribe(session)?;
        debug!("subscribed to SMTC session {key} | source={}", read_source_app(session));
        self.subscriptions.insert(key, subscription);
        Ok(())
    }

    /// Removes a session's subscription and stored state. Called when a
    /// session reports Closed and when it disappears from the session list.
    fn evict(&mut self, key: usize) {
        self.dirty.remove(&key);
        if let Some(subscription) = self.subscriptions.remove(&key) {
            let _ = subscription
                .session
                .RemoveMediaPropertiesChanged(subscription.properties_token);
            let _ = subscription
                .session
                .RemovePlaybackInfoChanged(subscription.playback_token);
        }
        if self.states.remove(&key).is_some() {
            debug!("evicted SMTC state | key={key}");
        }
    }

    fn remove_all_subscriptions(&mut self) {
        let keys: Vec<usize> = self.subscriptions.keys().copied().collect();
        for key in keys {
            self.evict(key);
        }
    }

    fn schedule_flush(&mut self) {
        let deadline = Instant::now() + debounce_duration(&self.config.read().unwrap());
        self.pending_deadline = Some(self.pending_deadline.map_or(deadline, |d| d.min(deadline)));
    }
}

/// Every field that is actually displayed, for diagnosable logs.
fn track_label(track: &TrackInfo) -> String {
    let track_no = track
        .track_number
        .map(|n| format!("{n}/{}", track.track_count.unwrap_or(0)))
        .unwrap_or_else(|| "-".into());
    format!(
        "title={:?} | artist={:?} | album={:?} | album_artist={:?} | subtitle={:?} | artwork={} | duration={:?}s | track={track_no} | genre={:?} | source={:?}",
        track.title,
        track.artist,
        track.album,
        track.album_artist,
        track.subtitle,
        if track.artwork.is_some() { "yes" } else { "no" },
        track.duration_secs,
        track.genre,
        track.source_app,
    )
}

/// Merges a fresh read into the stored state. Within the same title/artist
/// identity, empty fields inherit from the stored state (SMTC fills metadata
/// progressively: title -> artist -> album -> artwork); a new identity starts
/// fresh. Artwork presence follows `read_artwork` — a poll that did not read
/// artwork must not touch it.
fn merge_track(prev: &LogicalState, read: &TrackInfo, read_artwork: bool) -> TrackInfo {
    let same_identity = read.title == prev.title && read.artist == prev.artist;
    let inherit = |value: &str, fallback: &str| {
        if same_identity && value.trim().is_empty() {
            fallback.to_string()
        } else {
            value.to_string()
        }
    };
    TrackInfo {
        title: read.title.clone(),
        artist: inherit(&read.artist, &prev.artist),
        album: inherit(&read.album, &prev.album),
        album_artist: inherit(&read.album_artist, &prev.album_artist),
        subtitle: inherit(&read.subtitle, &prev.subtitle),
        artwork: if read_artwork { read.artwork.clone() } else { None },
        source_app: read.source_app.clone(),
        duration_secs: if same_identity {
            read.duration_secs.or(prev.duration_secs)
        } else {
            read.duration_secs
        },
        track_number: if same_identity {
            read.track_number.or(prev.track_number)
        } else {
            read.track_number
        },
        track_count: if same_identity {
            read.track_count.or(prev.track_count)
        } else {
            read.track_count
        },
        genre: if same_identity {
            read.genre.clone().or_else(|| prev.genre.clone())
        } else {
            read.genre.clone()
        },
    }
}

/// Whether any displayed content field differs from the stored state.
fn content_differ(prev: &LogicalState, read: &TrackInfo) -> bool {
    read.title != prev.title
        || read.artist != prev.artist
        || read.album != prev.album
        || read.album_artist != prev.album_artist
        || read.subtitle != prev.subtitle
        || read.source_app != prev.source_app
        || read.duration_secs != prev.duration_secs
        || read.track_number != prev.track_number
        || read.track_count != prev.track_count
        || read.genre != prev.genre
}

/// Decides whether a merged read should emit a TrackChanged, and whether the
/// stored artwork presence should be updated to absent. Artwork presence
/// follows the read only when artwork was actually read; a gain re-emits (the
/// pill refreshes the cover in place), a loss is stored silently — absence is
/// already shown as a placeholder, so re-emitting would flash the same track.
///
/// On the first event-driven read for a session (the stored state is still
/// empty), if artwork has not arrived yet, the emit is deferred until the
/// `has_artwork` gained diff fires on a later read (or until `ARTWORK_TIMEOUT`
/// expires). This eliminates the artwork=no→artwork=yes double-pill: the app
/// reports title/artist first, then fires a second event with the thumbnail.
/// Poll reads (`read_artwork=false`) never defer: they cannot fetch artwork,
/// so the startup pill must show immediately rather than wait for a timeout.
fn emit_track(prev: &LogicalState, merged: &TrackInfo, read_artwork: bool) -> (bool, bool) {
    let content_changed = content_differ(prev, merged);
    let artwork_gained = read_artwork && merged.artwork.is_some() && !prev.has_artwork;
    let artwork_lost = read_artwork && merged.artwork.is_none() && prev.has_artwork;
    let is_first_read = prev.source_app.is_empty() && prev.title.is_empty();
    let defer_first = is_first_read && read_artwork && merged.artwork.is_none();
    (content_changed && !defer_first || artwork_gained, artwork_lost)
}

/// Whether a deferred first pill has waited past the artwork timeout and
/// should be emitted anyway, artwork or not.
fn defer_expired(deferred_at: Option<Instant>) -> bool {
    deferred_at.is_some_and(|t| t.elapsed() >= ARTWORK_TIMEOUT)
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
/// Session-list events alone do not cover those cases; both collapse into the
/// same debounced re-sync.
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

/// Best-effort title/artist for a session's history row. Reads can fail or
/// return empty for freshly-created sessions; the title falls back to the
/// source label so the row always names the app.
fn read_session_text(session: &GlobalSystemMediaTransportControlsSession, source_app: &str) -> (String, String) {
    let Ok(properties) = session.TryGetMediaPropertiesAsync().and_then(|op| op.get()) else {
        return (source_app.to_string(), String::new());
    };
    let title = non_empty(
        properties.Title().map(|v| v.to_string()).unwrap_or_default(),
        source_app,
    );
    let artist = non_empty(properties.Artist().map(|v| v.to_string()).unwrap_or_default(), "");
    (title, artist)
}

/// Best-effort playback status for a session's history row. Unknown statuses
/// (Opened/Changing) are reported as Playing — a live session is assumed to
/// be playing unless it explicitly says otherwise.
fn read_session_state(session: &GlobalSystemMediaTransportControlsSession) -> PlaybackState {
    match session.GetPlaybackInfo().and_then(|info| info.PlaybackStatus()) {
        Ok(GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing) => PlaybackState::Playing,
        Ok(GlobalSystemMediaTransportControlsSessionPlaybackStatus::Paused) => PlaybackState::Paused,
        Ok(GlobalSystemMediaTransportControlsSessionPlaybackStatus::Stopped) => PlaybackState::Stopped,
        _ => PlaybackState::Playing,
    }
}

fn read_track_info(session: &GlobalSystemMediaTransportControlsSession, read_artwork: bool) -> Result<TrackInfo> {
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
    // Album artist and subtitle are read as additional data sources. Some apps
    // (e.g. YouTube Music) populate only Title/Artist and leave these empty,
    // but others may fill one but not the album title — the pill falls back to
    // whichever is available.
    let album_artist = non_empty(properties.AlbumArtist()?.to_string(), "");
    let subtitle = non_empty(properties.Subtitle()?.to_string(), "");
    let artwork = if read_artwork {
        // Artwork reads fail transiently under heavy session churn (overlapping
        // async WinRT calls on one thread); retry once before giving up, and
        // log which call failed with its raw HRESULT. When the session itself
        // is gone (RPC-unavailable / device-not-ready), retrying cannot
        // succeed — return None immediately.
        match read_thumbnail(&properties) {
            Ok(artwork) => artwork,
            Err(first) => {
                debug!("album-art read failed (attempt 1): {first:#}");
                if is_session_gone(&first) {
                    None
                } else {
                    match read_thumbnail(&properties) {
                        Ok(artwork) => artwork,
                        Err(second) => {
                            debug!("album-art read failed (attempt 2): {second:#}");
                            None
                        }
                    }
                }
            }
        }
    } else {
        None
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
        album_artist,
        subtitle,
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

/// Whether an error is one of the HRESULTs WinRT raises while a session is
/// torn down mid-read (RPC server unavailable / device not ready). Expected
/// under session churn: the event fired, then the session died before the
/// read completed. A retry cannot succeed, so fail fast instead of logging
/// an anomaly. Mirrors WindowsMediaController's message-based suppression.
fn is_session_gone(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("0x800706BA") || text.contains("0x80070015")
}

fn read_thumbnail(
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
    if size == 0 || !(1024..=8 * 1024 * 1024).contains(&size) || size > u32::MAX as u64 {
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

    fn track(title: &str, artist: &str) -> TrackInfo {
        TrackInfo {
            title: title.into(),
            artist: artist.into(),
            ..TrackInfo::default()
        }
    }

    fn state(title: &str, artist: &str) -> LogicalState {
        LogicalState {
            title: title.into(),
            artist: artist.into(),
            ..LogicalState::default()
        }
    }

    #[test]
    fn merge_inherits_missing_fields_within_the_same_identity() {
        let prev = LogicalState {
            album: "Album".into(),
            genre: Some("Rock".into()),
            track_number: Some(3),
            track_count: Some(10),
            duration_secs: Some(200),
            has_artwork: true,
            ..state("Song", "Artist")
        };
        let read = track("Song", "Artist");
        let merged = merge_track(&prev, &read, true);
        assert_eq!(merged.album, "Album");
        assert_eq!(merged.genre.as_deref(), Some("Rock"));
        assert_eq!(merged.track_number, Some(3));
        assert_eq!(merged.track_count, Some(10));
        assert_eq!(merged.duration_secs, Some(200));
        // A read without artwork must not inherit stored artwork bytes.
        assert!(merged.artwork.is_none());
    }

    #[test]
    fn merge_starts_fresh_when_the_identity_changes() {
        let prev = LogicalState {
            album: "Album".into(),
            genre: Some("Rock".into()),
            track_number: Some(3),
            ..state("Song", "Artist")
        };
        let merged = merge_track(&prev, &track("Next", "Artist"), true);
        assert_eq!(merged.album, "");
        assert_eq!(merged.genre, None);
        assert_eq!(merged.track_number, None);
        // An empty artist on a new title stays empty (the pill hides the row).
        let merged = merge_track(&prev, &track("Next", ""), true);
        assert_eq!(merged.artist, "");
    }

    #[test]
    fn merge_poll_read_never_carries_artwork() {
        let prev = LogicalState {
            has_artwork: true,
            ..state("Song", "Artist")
        };
        let merged = merge_track(&prev, &track("Song", "Artist"), false);
        assert!(merged.artwork.is_none());
    }

    #[test]
    fn content_differ_sees_every_displayed_field() {
        let prev = state("Song", "Artist");
        assert!(!content_differ(&prev, &track("Song", "Artist")));
        assert!(content_differ(&prev, &track("Other", "Artist")));
        assert!(content_differ(&prev, &track("Song", "Other")));
        let album_only = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            ..TrackInfo::default()
        };
        assert!(content_differ(&prev, &album_only));
    }

    #[test]
    fn emit_track_decides_emits_and_artwork_losses() {
        let prev = state("Song", "Artist");
        // Unchanged: no emit.
        assert_eq!(emit_track(&prev, &track("Song", "Artist"), true), (false, false));
        // Content change: emit.
        assert_eq!(emit_track(&prev, &track("Other", "Artist"), true), (true, false));
        // Artwork gained: emit (the cover refreshes the pill in place).
        let with_art = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            artwork: Some(vec![1]),
            ..TrackInfo::default()
        };
        assert_eq!(emit_track(&prev, &with_art, true), (true, false));
        // Artwork lost: store the loss, no emit.
        let had_art = LogicalState {
            has_artwork: true,
            ..state("Song", "Artist")
        };
        assert_eq!(emit_track(&had_art, &track("Song", "Artist"), true), (false, true));
        // A poll that did not read artwork never touches presence.
        assert_eq!(emit_track(&had_art, &track("Song", "Artist"), false), (false, false));
        assert_eq!(emit_track(&prev, &with_art, false), (false, false));
        // First read without artwork: deferred on event-driven reads (awaits
        // the thumbnail), but a poll first-read emits immediately — the
        // startup read must not wait for artwork a poll cannot fetch.
        let first = LogicalState::default();
        assert_eq!(emit_track(&first, &track("Song", "Artist"), true), (false, false));
        assert_eq!(emit_track(&first, &track("Song", "Artist"), false), (true, false));
        // First read WITH artwork: emits immediately (no double-pill).
        assert_eq!(emit_track(&first, &with_art, true), (true, false));
        // After a deferred first read the state holds the track info; a
        // subsequent poll (no artwork read) sees no content change and
        // correctly emits nothing.
        let after_defer = LogicalState {
            title: "Song".into(),
            artist: "Artist".into(),
            ..LogicalState::default()
        };
        assert_eq!(
            emit_track(&after_defer, &track("Song", "Artist"), false),
            (false, false)
        );
    }

    #[test]
    fn defer_timeout_expires_only_after_the_artwork_window() {
        let now = Instant::now();
        // No deferral: never expired.
        assert!(!defer_expired(None));
        // Just deferred: not expired.
        assert!(!defer_expired(Some(now)));
        // Deferred recently (artwork normally arrives ~500ms later): not expired.
        let recent = now.checked_sub(Duration::from_millis(1500)).unwrap();
        assert!(!defer_expired(Some(recent)));
        // Deferred past the timeout: expired, the pill must fire anyway.
        let old = now.checked_sub(Duration::from_secs(3)).unwrap();
        assert!(defer_expired(Some(old)));
        // Exactly at the boundary: expired.
        let boundary = now.checked_sub(ARTWORK_TIMEOUT).unwrap();
        assert!(defer_expired(Some(boundary)));
    }

    #[test]
    fn session_gone_detects_teardown_hresults() {
        use windows::core::HRESULT;
        let rpc = anyhow::Error::new(windows::core::Error::from(HRESULT(0x8007_06BAu32 as i32)));
        let device = anyhow::Error::new(windows::core::Error::from(HRESULT(0x8007_0015u32 as i32)));
        let other = anyhow!("disk read failed (hr=0x80004005)");
        assert!(is_session_gone(&rpc));
        assert!(is_session_gone(&device));
        assert!(!is_session_gone(&other));
    }
}
