use crate::config::Config;
use crate::events::{ARTWORK_DECODE, MediaEvent, PlaybackState, TrackInfo, decode_artwork_pm};
use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Media::Control::{
    CurrentSessionChangedEventArgs, GlobalSystemMediaTransportControlsSession,
    GlobalSystemMediaTransportControlsSessionManager, GlobalSystemMediaTransportControlsSessionPlaybackStatus,
    MediaPropertiesChangedEventArgs, PlaybackInfoChangedEventArgs, SessionsChangedEventArgs,
};
use windows::Storage::Streams::{Buffer, DataReader, InputStreamOptions};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Memory::{GetProcessHeap, HEAP_FLAGS, HeapCompact};
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
/// it, and only the fields that actually changed are emitted. Same-source
/// sessions are filtered against Windows' current session; different sources
/// remain independent.
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
    /// Number of poll-driven artwork retries attempted for this session. Bounded
    /// by `ARTWORK_RETRY_BUDGET` so a session that never provides a thumbnail is
    /// not re-read indefinitely (the 2s poll interval keeps this cheap).
    artwork_attempts: u8,
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

/// Minimum gap between a TrackChanged emit and a forced artwork-changed
/// re-emit for the same source. SMTC re-reads the thumbnail within ~1s of
/// a change and can return different bytes for the same cover; this keeps
/// that re-read from firing a duplicate pill while still surfacing a
/// genuinely different cover that appears later.
const ARTWORK_CHANGE_MIN_INTERVAL: Duration = Duration::from_secs(3);

/// Maximum number of poll-driven artwork retries per session. A poll that
/// never reads artwork (read_artwork=false) cannot surface a thumbnail, so
/// when a session is still missing art we re-read it on the poll path up to
/// this many times (~6s at the 2s poll interval) before giving up. Once a
/// thumbnail is present, `has_artwork` is true and no further retries run.
const ARTWORK_RETRY_BUDGET: u8 = 3;

/// Capacity of the signal channel between the WinRT event handlers and the
/// listener loop. `try_send` drops a signal when the queue is full; that is
/// safe because every dropped signal is a coalescible wake-up — the dirty-set
/// membership it would have recorded is re-covered by the periodic safety-net
/// poll within 2s. The bound keeps a session storm from accumulating
/// unbounded queued COM session references.
const SIGNAL_QUEUE_CAP: usize = 256;

/// Source labels of every currently open SMTC session, refreshed at each
/// subscription re-sync. The process picker reads this so media apps that run
/// without a visible window (tray-only Electron apps, background browser
/// tabs) still appear as selectable entries.
static ACTIVE_SESSION_SOURCES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

pub(crate) fn active_session_sources() -> Vec<String> {
    let Some(list) = ACTIVE_SESSION_SOURCES.get() else {
        return Vec::new();
    };
    list.lock().map(|guard| guard.clone()).unwrap_or_default()
}

fn set_active_session_sources(sources: Vec<String>) {
    let list = ACTIVE_SESSION_SOURCES.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut guard) = list.lock() {
        *guard = sources;
    }
}

struct ListenerState {
    manager: GlobalSystemMediaTransportControlsSessionManager,
    config: Arc<RwLock<Config>>,
    output: Sender<MediaEvent>,
    signal_tx: SyncSender<Signal>,
    /// Every open session's event subscriptions, keyed by session pointer.
    subscriptions: HashMap<usize, SessionSubscription>,
    /// Last known displayed state per session key.
    states: HashMap<usize, LogicalState>,
    /// Keys with unprocessed property events in the current tick, in arrival
    /// order. The flush reads each key once, so a burst of events for one
    /// session coalesces into one read + one diff + one emit per debounce
    /// window. A `VecDeque` preserves arrival order (a `HashSet` would emit
    /// cross-session events in arbitrary order).
    dirty: VecDeque<usize>,
    /// Membership mirror of `dirty`, so insertion and eviction stay O(1).
    dirty_seen: HashSet<usize>,
    /// A SessionsChanged burst is pending its debounce window; the next flush
    /// performs the re-sync once per burst instead of once per event.
    sessions_pending: bool,
    /// Debounce deadline for pending dirty keys and session bursts.
    pending_deadline: Option<Instant>,
    /// Last time the periodic safety-net poll ran.
    /// Last time the process heap was compacted. The worker and the UI
    /// thread share that heap, and HeapCompact takes its lock, so the
    /// compaction is throttled hard to keep UI-thread allocations
    /// stall-free.
    last_heap_compact: Instant,
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
    /// Generation counter shared with the supervisor. A worker only emits
    /// events and updates the heartbeat while its own generation is still the
    /// current one; a stalled worker that was replaced stops contributing the
    /// moment its successor increments the counter.
    live_generation: Arc<AtomicU64>,
    /// The generation this listener belongs to.
    my_generation: u64,
    /// Set by main at exit; the event loop breaks within its receive timeout.
    shutdown: Arc<AtomicBool>,
    /// Last emitted track per source app (keyed by `source_app`). Persisting
    /// this across session-key changes lets us suppress the duplicate
    /// TrackChanged events a source emits when it recreates its session
    /// (YouTube Music is observed doing this every ~60s and on every song
    /// change): the new session has a default LogicalState, so content_differ
    /// always sees a change. We compare title + artist + artwork-presence so
    /// that a genuine artwork gain still surfaces as an in-place refresh.
    last_track_per_source: HashMap<String, TrackInfo>,
    /// Last playback state each source app reported, surviving session-key
    /// changes. A recreated session (new key, default state) re-reports the
    /// source's current playback; comparing it against this value tells the
    /// session-recreation guard whether that report is noise (state unchanged)
    /// or the user's real pause/play (state changed — YouTube Music recreates
    /// its session when the transport buttons are used, so the fresh session's
    /// first state can be the actual transition).
    last_known_playback_per_source: HashMap<String, PlaybackState>,
    /// Cached app icons keyed by source_app label (derived from AUMID via
    /// `source_app_label`). Populated on first encounter of a source.
    icon_cache: HashMap<String, Option<Arc<[u8]>>>,
    /// When the last TrackChanged was emitted per source, used to time-gate
    /// the artwork-changed re-emit: SMTC re-reads the thumbnail within ~1s
    /// of a change and may return different bytes for the same cover, which
    /// would otherwise fire a duplicate pill for the same song.
    last_emit_at: HashMap<String, Instant>,
}

pub struct SmtcListener {
    output: Sender<MediaEvent>,
    config: Arc<RwLock<Config>>,
    /// Updated by the event loop every few seconds so a supervisor can detect
    /// a stalled worker (a WinRT call hanging under session churn) and
    /// restart the listener.
    heartbeat: Arc<Mutex<Instant>>,
    /// Worker generation guard (see `ListenerState`).
    live_generation: Arc<AtomicU64>,
    my_generation: u64,
    /// Set by main when the process is exiting; the event loop breaks within
    /// its receive timeout so the worker unsubscribes and releases COM
    /// promptly instead of running until process termination.
    shutdown: Arc<AtomicBool>,
}

impl SmtcListener {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        output: Sender<MediaEvent>,
        config: Arc<RwLock<Config>>,
        heartbeat: Arc<Mutex<Instant>>,
        live_generation: Arc<AtomicU64>,
        my_generation: u64,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            output,
            config,
            heartbeat,
            live_generation,
            my_generation,
            shutdown,
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
        let (signal_tx, signal_rx) = mpsc::sync_channel(SIGNAL_QUEUE_CAP);
        let sessions_token = register_sessions_handler(&manager, signal_tx.clone())?;
        let current_token = register_current_session_handler(&manager, signal_tx.clone())?;
        let mut state = ListenerState::new(
            manager,
            self.config,
            self.output,
            signal_tx,
            self.heartbeat,
            self.live_generation,
            self.my_generation,
            self.shutdown,
        );

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
    #[allow(clippy::too_many_arguments)]
    fn new(
        manager: GlobalSystemMediaTransportControlsSessionManager,
        config: Arc<RwLock<Config>>,
        output: Sender<MediaEvent>,
        signal_tx: SyncSender<Signal>,
        heartbeat: Arc<Mutex<Instant>>,
        live_generation: Arc<AtomicU64>,
        my_generation: u64,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            manager,
            config,
            output,
            signal_tx,
            subscriptions: HashMap::new(),
            states: HashMap::new(),
            dirty: VecDeque::new(),
            dirty_seen: HashSet::new(),
            sessions_pending: false,
            pending_deadline: None,
            last_heap_compact: Instant::now(),
            last_session_check: Instant::now(),
            churn: HashMap::new(),
            churn_cooldown: HashMap::new(),
            rejected_seen: HashSet::new(),
            last_track_per_source: HashMap::new(),
            last_known_playback_per_source: HashMap::new(),
            icon_cache: HashMap::new(),
            last_emit_at: HashMap::new(),
            heartbeat,
            live_generation,
            my_generation,
            shutdown,
        }
    }

    fn event_loop(&mut self, signal_rx: Receiver<Signal>) -> Result<()> {
        let session_check_interval = Duration::from_secs(2);
        loop {
            // Set by main at exit: leave promptly (within the receive
            // timeout) so run_initialized's cleanup unsubscribes every
            // session and uninitializes COM instead of running until process
            // termination.
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            // Only the current worker generation may keep the heartbeat
            // fresh: a stale worker that wakes after being replaced must not
            // mask a stall in its successor.
            if self.is_current_generation() {
                *self.heartbeat.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
            }
            // The Windows heap keeps freed blocks (artwork decodes, thumbnail
            // bytes) in its free lists instead of returning them to the OS,
            // so RSS climbs as songs change. Compacting on a 60s cadence
            // releases that free space back to the OS. The worker and the UI
            // thread share this heap and HeapCompact takes its lock, so a
            // shorter cadence would stall UI-thread allocations on every
            // compact.
            if self.last_heap_compact.elapsed() >= Duration::from_secs(60) {
                self.last_heap_compact = Instant::now();
                unsafe {
                    if let Ok(heap) = GetProcessHeap() {
                        let _ = HeapCompact(heap, HEAP_FLAGS(0));
                    }
                }
            }
            let timeout = self
                .pending_deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                // Wake at least every 2 s when nothing is pending, so the
                // heartbeat stays fresh and the periodic safety-net poll below
                // keeps its documented cadence.
                .unwrap_or(session_check_interval)
                .min(session_check_interval);

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
            // A continuous signal stream must not starve the debounce flush
            // or the periodic safety net: run both once their deadline has
            // passed, regardless of how many signals arrived in between.
            if self.pending_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                self.flush();
            }
            if self.last_session_check.elapsed() >= session_check_interval {
                self.last_session_check = Instant::now();
                self.sync_subscriptions();
                self.poll_sessions();
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
                if !self.should_follow_session(&session) {
                    debug!(
                        "SMTC session event skipped | reason=not-current-session | key={key} | source={}",
                        read_source_app(&session)
                    );
                    return Ok(());
                }
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
                // The deque preserves arrival order across sessions; the
                // membership set keeps dedup O(1).
                if self.dirty_seen.insert(key) {
                    self.dirty.push_back(key);
                }
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
            let keys: Vec<usize> = self.dirty.drain(..).collect();
            self.dirty_seen.clear();
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
        if !self.should_follow_session(session) {
            debug!(
                "SMTC session read skipped | reason=not-current-session | key={key} | source={}",
                read_source_app(session)
            );
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
        let mut known_playback = None;
        if playback != prev.playback
            && let Some(state) = playback
        {
            // The last state this source reported, captured before this read
            // overwrites it. The session-recreation guard below compares the
            // fresh session's report against it: a state that actually changed
            // is the user's own pause/play, not recreation noise.
            let source = read_source_app(session);
            known_playback = self.last_known_playback_per_source.get(&source).copied();
            self.last_known_playback_per_source.insert(source.clone(), state);
            next.playback = Some(state);
            info!("playback state changed | state={state:?} | source={source}");
            events.push(MediaEvent::PlaybackStateChanged(state, source));
        }

        // Content is only diffed while the session is not stopped; a stopped
        // session keeps its stored content (the pill shows the last track).
        if status != GlobalSystemMediaTransportControlsSessionPlaybackStatus::Stopped {
            match read_track_info(session, read_artwork) {
                Ok(read) => {
                    let mut merged = merge_track(&prev, &read, read_artwork);
                    // Session-recreation recovery: when a source recreates its
                    // session (new key, default prev state) its first event-driven
                    // read often grabs an empty thumbnail stream (SMTC populates art
                    // ~500ms after title). Inject the cached artwork for the same
                    // title+artist identity so the dedup predicate sees present==
                    // present and suppresses the duplicate — the cover is already
                    // known, just not re-readable on this fresh session yet.
                    if read_artwork
                        && merged.artwork.is_none()
                        && let Some(cached) = self.cached_artwork_for(&merged.source_app, &merged.title, &merged.artist)
                    {
                        merged.artwork = Some(cached);
                    }
                    // App icon extraction: one icon per source app, cached
                    // (keyed by the source_app label, derived from the AUMID).
                    // The AUMID is read from the live session; the icon is
                    // attached to the track so the overlay can render it.
                    if merged.app_icon.is_none() {
                        if let Some(cached_icon) = self.icon_cache.get(&merged.source_app) {
                            merged.app_icon = cached_icon.clone();
                        } else if let Ok(aumid) = session.SourceAppUserModelId() {
                            let aumid_str = aumid.to_string();
                            let extracted = crate::icon::extract_app_icon(&aumid_str, 24);
                            let cached = extracted.as_ref().map(|p| Arc::from(p.as_slice()));
                            self.icon_cache.insert(merged.source_app.clone(), cached.clone());
                            merged.app_icon = cached;
                        }
                    }
                    let (mut emit, artwork_lost) = emit_track(&prev, &merged, read_artwork);
                    // Safety net: a first pill deferred for artwork shows
                    // anyway after ARTWORK_TIMEOUT, so a source that never
                    // provides a thumbnail still gets its pill.
                    if !emit && defer_expired(prev.deferred_at) {
                        emit = true;
                        let label = track_label(&merged);
                        debug!("track emit forced | reason=artwork-timeout | {label}");
                    }
                    // Same song, different cover: some sources swap album art
                    // for the same title+artist (e.g. a video vs audio
                    // version). content_differ only compares text fields, so
                    // compare the artwork bytes against the last emitted
                    // track and surface the new cover as a refresh. Gated by
                    // ARTWORK_CHANGE_MIN_INTERVAL: SMTC re-reads the
                    // thumbnail within ~1s of a change and can return
                    // different bytes for the same cover, which would
                    // otherwise fire a duplicate pill for the same song.
                    if !emit
                        && read_artwork
                        && let Some(prev_track) = self.last_track_per_source.get(&merged.source_app)
                        && prev_track.title == merged.title
                        && prev_track.artist == merged.artist
                        && self
                            .last_emit_at
                            .get(&merged.source_app)
                            .is_none_or(|t| t.elapsed() >= ARTWORK_CHANGE_MIN_INTERVAL)
                    {
                        let art_changed = match (&prev_track.artwork, &merged.artwork) {
                            (Some(a), Some(b)) => !Arc::ptr_eq(a, b) && a.as_ref() != b.as_ref(),
                            (None, Some(_)) | (Some(_), None) => true,
                            (None, None) => false,
                        };
                        if art_changed {
                            emit = true;
                            let label = track_label(&merged);
                            debug!("track emit forced | reason=artwork-changed | {label}");
                        }
                    }
                    // Per-source session-recreation dedup: a source that
                    // recreates its session (e.g. YouTube Music ~60s and on
                    // every song change) re-reports the same track on a new
                    // session key. Since the new session starts with a default
                    // LogicalState, content_differ always sees a change. Compare
                    // against the last track actually emitted per source
                    // (title + artist + artwork-presence); a genuine artwork
                    // gain still surfaces because is_some() changes. When the
                    // read did not read artwork (the 2s poll: read_artwork=false),
                    // the artwork clause is skipped — the poll always produces
                    // artwork=None, which would otherwise mismatch the last emit's
                    // Some and escape dedup as a duplicate pill (see the Bleed It
                    // Out case: same session key, duration drift, poll read art=None
                    // vs last emit art=Some). Cached artwork injection (above) makes
                    // event reads for recreated sessions also see Some==Some.
                    let session_recreation = self
                        .last_track_per_source
                        .get(&merged.source_app)
                        .is_some_and(|prev_track| is_session_recreation(prev_track, &merged, read_artwork));
                    // A recreated session starts from a default LogicalState, so
                    // its first read reports the new session's default playback
                    // state (e.g. Paused while the user never touched anything)
                    // as if it were a real transition. When the track identifies
                    // the whole read as a session recreation, the paired
                    // playback event is usually spurious too: drop it so a
                    // source that re-creates its session while paused does not
                    // fire pills. The exception is a state that actually changed
                    // since the source last reported it — YouTube Music
                    // recreates its session when the user presses pause/play,
                    // so the fresh session's first state can be the real
                    // transition and must be shown.
                    if session_recreation
                        && prev.source_app.is_empty()
                        && prev.title.is_empty()
                        && spurious_recreated_playback(known_playback, playback)
                    {
                        events.retain(|e| !matches!(e, MediaEvent::PlaybackStateChanged(_, _)));
                    }
                    if emit && !session_recreation {
                        let label = track_label(&merged);
                        info!("track changed | {label}");
                        let emitted = with_decoded_art(merged.clone());
                        events.push(MediaEvent::TrackChanged(emitted));
                        self.last_track_per_source
                            .insert(merged.source_app.clone(), merged.clone());
                        self.last_emit_at.insert(merged.source_app.clone(), Instant::now());
                        next.deferred_at = None;
                    } else if session_recreation {
                        let label = track_label(&merged);
                        debug!("track emit suppressed | reason=session-recreation | {label}");
                    } else if artwork_lost {
                        // Absence is already shown as a placeholder: store the
                        // loss (a later reappearance re-emits) without
                        // flashing the same track again.
                        let label = track_label(&merged);
                        debug!("track emit skipped | reason=artwork-removed | {label}");
                    } else {
                        let is_first_read = prev.source_app.is_empty() && prev.title.is_empty();
                        if is_first_read
                            && read_artwork
                            && !is_placeholder_read(&prev, &merged)
                            && merged.artwork.is_none()
                        {
                            // The paired playback event (already queued above)
                            // would reach the overlay before the deferred
                            // track and render a state pill with the source's
                            // *previous* track. Hold it back: the deferred
                            // track carries the change.
                            events.retain(|e| !matches!(e, MediaEvent::PlaybackStateChanged(_, _)));
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

        // The session may have stopped being current, or its source may have
        // been disallowed, while the slow reads above were running. Revalidate
        // before storing state or emitting, so a stale read cannot surface a
        // track or playback pill after the current session moved on.
        if !self.should_follow_session(session) || !self.session_source_allowed(session) {
            debug!("SMTC session changed during read; discarding pending events | key={key}");
            return Ok(());
        }

        self.states.insert(key, next);
        for event in events {
            self.emit(event);
        }
        Ok(())
    }

    /// Re-syncs the subscription map with the current session list: subscribes
    /// to the current session for an allowed source, drops stale subscriptions
    /// and stored state, and accounts per-source session churn for the cool-down.
    fn sync_subscriptions(&mut self) {
        let Ok(sessions) = self.manager.GetSessions() else {
            debug!("SMTC GetSessions failed; keeping the current subscription map");
            return;
        };
        let mut sessions: Vec<_> = sessions.into_iter().collect();
        let current = self.manager.GetCurrentSession().ok();
        let current_key = current.as_ref().map(session_key);
        let current_source = current.as_ref().map(read_source_app);
        if let (Some(key), Some(source)) = (current_key, current_source.as_deref()) {
            debug!("SMTC current session | key={key} | source={source}");
        }
        // Under browser session churn, GetCurrentSession can briefly return a
        // session that is missing from the GetSessions snapshot. It is still
        // authoritative, so include it explicitly instead of filtering every
        // listed session and ending up with no subscribed source.
        if let Some(current) = current.as_ref()
            && !sessions
                .iter()
                .any(|session| session_key(session) == session_key(current))
        {
            sessions.push(current.clone());
        }
        let before: HashSet<usize> = self.subscriptions.keys().copied().collect();
        for session in &sessions {
            let key = session_key(session);
            let source = read_source_app(session);
            let allowed = self.session_source_allowed(session);
            debug!(
                "SMTC session {} | key={key} | source={} | allowed_sources={:?}",
                if allowed { "accepted" } else { "rejected" },
                read_source_app(session),
                self.config
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .behavior
                    .allowed_sources
            );
            if !allowed {
                // Log rejected sessions once per appearance so the history
                // shows every media source, not just the tracked ones.
                if self.rejected_seen.insert(key) {
                    let source_app = read_source_app(session);
                    let (title, artist) = read_session_text(session, &source_app);
                    let state = read_session_state(session);
                    self.emit(MediaEvent::SessionRejected {
                        source_app,
                        title,
                        artist,
                        state,
                        accepted: false,
                    });
                }
                // A session that became disallowed (allow-list edit) or whose
                // source tripped the churn cool-down must not keep its event
                // subscriptions: it would otherwise keep firing signals that
                // every path discards.
                self.evict(key);
                continue;
            }
            if !session_matches_current_source(key, &source, current_key, current_source.as_deref()) {
                debug!("SMTC session ignored | reason=not-current-session | key={key} | source={source}");
                continue;
            }
            // A previously-rejected session that became allowed (config edit)
            // should re-report as accepted on its next rejection, if any.
            self.rejected_seen.remove(&key);
            if !before.contains(&key) {
                self.record_churn(&read_source_app(session));
            }
            let is_new = !before.contains(&key);
            if let Err(error) = self.ensure_subscribed(session) {
                debug!("subscribe failed for a session: {error:#}");
            } else if is_new {
                // Immediately read properties for newly discovered sessions.
                // A source may fire MediaPropertiesChanged before we finish
                // registering the event handler (the SessionsChanged event that
                // revealed the session arrives first). Polling TryGetMediaProperties
                // now catches data that would otherwise be lost until the next event
                // burst — Windows's own SMTC widget does the same.
                if let Err(error) = self.refresh_session(session, true) {
                    debug!("initial refresh failed for session {key}: {error:#}");
                }
            }
        }
        let alive: HashSet<usize> = sessions.iter().map(session_key).collect();
        let mut stale: Vec<usize> = self
            .subscriptions
            .keys()
            .filter(|k| !alive.contains(k))
            .copied()
            .collect();
        if let (Some(current_key), Some(current_source)) = (current_key, current_source.as_deref()) {
            for (key, subscription) in &self.subscriptions {
                if *key != current_key && read_source_app(&subscription.session) == current_source {
                    stale.push(*key);
                }
            }
        }
        stale.sort_unstable();
        stale.dedup();
        for key in stale {
            debug!("SMTC session disappeared | key={key}");
            self.evict(key);
        }
        // Forget rejected sessions that vanished so a later reappearance is
        // reported again.
        self.rejected_seen.retain(|key| alive.contains(key));
        self.churn_cooldown.retain(|_, until| *until > Instant::now());
        // Keep the picker's candidate list in sync with what is actually
        // open, including apps whose sessions were rejected: checking them
        // is how the user adds them to the allow-list.
        let active_sources: Vec<String> = sessions.iter().map(read_source_app).collect();
        let active: HashSet<String> = active_sources.iter().cloned().collect();
        set_active_session_sources(active_sources);
        // Evict source-level caches for apps that no longer have an open
        // session: their cached track (with artwork bytes) and icon would
        // otherwise persist forever, growing with every AUMID variant seen.
        self.last_track_per_source.retain(|source, _| active.contains(source));
        self.last_known_playback_per_source
            .retain(|source, _| active.contains(source));
        self.icon_cache.retain(|source, _| active.contains(source));
        self.last_emit_at.retain(|source, _| active.contains(source));
        // Churn counts for departed sources are worthless and would otherwise
        // accumulate one deque per distinct source ever seen.
        self.churn.retain(|source, _| active.contains(source));
    }

    /// YouTube Music and similar browser clients can leave several sessions
    /// open for one source. Keep other sources independent, but for the source
    /// owning Windows' current session only that exact session may emit. If
    /// Windows cannot answer the current-session query during a transition,
    /// keep the permissive fallback until the next sync.
    fn should_follow_session(&self, session: &GlobalSystemMediaTransportControlsSession) -> bool {
        let Ok(current) = self.manager.GetCurrentSession() else {
            return true;
        };
        let key = session_key(session);
        let source = read_source_app(session);
        let current_key = session_key(&current);
        let current_source = read_source_app(&current);
        session_matches_current_source(key, &source, Some(current_key), Some(&current_source))
    }

    /// Returns the last emitted artwork bytes for `source_app` if the cached
    /// track matches the given title+artist identity. This only returns art for
    /// the *same track* — never cross-track — so a recreated session reports the
    /// cover without re-reading the (often transiently-empty) thumbnail stream.
    fn cached_artwork_for(&self, source_app: &str, title: &str, artist: &str) -> Option<Arc<[u8]>> {
        cached_artwork_for(&self.last_track_per_source, source_app, title, artist)
    }

    /// Re-reads a session's artwork on the poll path (read_artwork=true), then
    /// re-runs the full refresh so a newly-arrived thumbnail surfaces a
    /// TrackChanged / artwork-gain event in place. The retry counter is bumped
    /// so a session that never provides art stops being polled after the budget
    /// is exhausted.
    fn retry_artwork(&mut self, session: &GlobalSystemMediaTransportControlsSession) -> Result<()> {
        let key = session_key(session);
        // The source may have entered the churn cool-down (or been filtered
        // out) since this session was subscribed; the retry must not emit
        // events for it any more than the normal refresh path would.
        if !self.should_follow_session(session)
            || !self.session_source_allowed(session)
            || !should_poll_artwork(self.states.get(&key))
        {
            return Ok(());
        }
        let read = match read_track_info(session, true) {
            Ok(read) => read,
            Err(error) => {
                // Count a failed read against the budget too: a session whose
                // reads keep failing must not be retried forever.
                if let Some(state) = self.states.get_mut(&key) {
                    state.artwork_attempts += 1;
                }
                return Err(error);
            }
        };
        let prev = self.states.get(&key).cloned().unwrap_or_default();
        let merged = merge_track(&prev, &read, true);
        // The retry only surfaces artwork the normal path missed: no artwork
        // found, or a recreated session re-reporting a track whose cover is
        // already shown, must not emit a duplicate pill.
        let track_changed = emit_track(&prev, &merged, true).0;
        let recreation_suppressed =
            merged.artwork.is_some() && !retry_should_emit(&merged, self.last_track_per_source.get(&merged.source_app));
        let emit = track_changed && !recreation_suppressed;
        if let Some(state) = self.states.get_mut(&key) {
            state.has_artwork = merged.artwork.is_some();
            state.artwork_attempts += 1;
        }
        if emit {
            let label = track_label(&merged);
            info!("track changed | {label}");
            self.last_track_per_source
                .insert(merged.source_app.clone(), merged.clone());
            self.emit(MediaEvent::TrackChanged(with_decoded_art(merged.clone())));
            if let Some(state) = self.states.get_mut(&key) {
                state.deferred_at = None;
            }
        } else if recreation_suppressed {
            let label = track_label(&merged);
            debug!("track emit suppressed | reason=session-recreation | {label}");
        }
        Ok(())
    }

    /// Re-reads every subscribed session (metadata only, no artwork) and diffs
    /// it against the stored state. The 2-second safety net; also used for the
    /// startup read so the pill reports what is already playing. For sessions
    /// still missing artwork, the poll also re-reads the thumbnail (up to the
    /// retry budget) so a slow-to-populate stream still surfaces a cover.
    fn poll_sessions(&mut self) {
        let keys: Vec<usize> = self.subscriptions.keys().copied().collect();
        for key in keys {
            // Clone the COM interface out so the map borrow ends before the
            // refresh (which can mutate subscriptions via eviction).
            let session = self.subscriptions.get(&key).map(|s| s.session.clone());
            if let Some(session) = session {
                let _ = self.refresh_session(&session, false);
                if should_poll_artwork(self.states.get(&key))
                    && let Err(error) = self.retry_artwork(&session)
                {
                    debug!("artwork retry failed for session {key}: {error:#}");
                }
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
        let allowed = self
            .config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .behavior
            .allowed_sources
            .clone();
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
            let _ = properties_tx.try_send(Signal::MediaProperties(properties_session.clone()));
            Ok(())
        });
        let playback_handler: TypedEventHandler<
            GlobalSystemMediaTransportControlsSession,
            PlaybackInfoChangedEventArgs,
        > = TypedEventHandler::new(move |_, _| {
            let _ = playback_tx.try_send(Signal::PlaybackInfo(playback_session.clone()));
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
    /// session reports Closed, disappears from the session list, or its
    /// source becomes disallowed (allow-list edit or churn cool-down).
    fn evict(&mut self, key: usize) {
        if self.dirty_seen.remove(&key) {
            self.dirty.retain(|k| *k != key);
        }
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
        let deadline = Instant::now() + debounce_duration(&self.config.read().unwrap_or_else(|p| p.into_inner()));
        self.pending_deadline = Some(self.pending_deadline.map_or(deadline, |d| d.min(deadline)));
    }

    /// Emits an event only while this worker generation is still current. A
    /// worker that stalled and was replaced must not keep producing events
    /// after its successor took over.
    fn emit(&self, event: MediaEvent) {
        if self.is_current_generation() {
            let _ = self.output.send(event);
        }
    }

    fn is_current_generation(&self) -> bool {
        self.live_generation.load(Ordering::SeqCst) == self.my_generation
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

/// If Windows reports a current session for a source, only that exact session
/// is authoritative for the source. Other sources remain independent so one
/// app becoming current does not erase another app's session state.
fn session_matches_current_source(
    key: usize,
    source: &str,
    current_key: Option<usize>,
    current_source: Option<&str>,
) -> bool {
    match (current_key, current_source) {
        (Some(current_key), Some(current_source)) if current_source == source => key == current_key,
        _ => true,
    }
}

/// Returns the last emitted artwork bytes for `source_app` if the cached
/// track matches the given title+artist identity. This only returns art for
/// the same track identity (title + artist) — never cross-track, so a
/// recreated session reports the cover without re-reading the
/// (often transiently-empty) thumbnail stream.
fn cached_artwork_for(
    last_track_per_source: &HashMap<String, TrackInfo>,
    source_app: &str,
    title: &str,
    artist: &str,
) -> Option<Arc<[u8]>> {
    last_track_per_source.get(source_app).and_then(|cached| {
        if cached.title == title && cached.artist == artist {
            cached.artwork.clone()
        } else {
            None
        }
    })
}

/// Whether a read of `merged` is a session recreation of the last emitted
/// track `prev_track` (same title + artist, same artwork identity). When
/// `read_artwork` is false (the 2s poll, which never reads art), the artwork
/// clause is skipped — a poll always produces None, which would otherwise
/// mismatch a last emit's Some and escape dedup as a duplicate pill. Artwork
/// is compared by bytes, not presence: a recreated session that re-reports
/// the same track with a *different* cover (video vs audio version) is not a
/// recreation — the artwork-changed emit must pass through, not be
/// suppressed. Used both by `refresh_session` and the tests, so the mirror
/// cannot drift.
fn is_session_recreation(prev_track: &TrackInfo, merged: &TrackInfo, read_artwork: bool) -> bool {
    prev_track.title == merged.title
        && prev_track.artist == merged.artist
        && if !read_artwork {
            true
        } else {
            match (&prev_track.artwork, &merged.artwork) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a.as_ref() == b.as_ref(),
                (None, None) => true,
                // Art gained or lost on the recreated session: emit so the
                // pill refreshes with the cover instead of keeping the stale
                // one.
                _ => false,
            }
        }
}

/// Whether a recreated session's first playback report is spurious noise
/// rather than a real transition. A source recreating its session (new key,
/// default state) re-reports its current playback state; when it matches the
/// last state the source reported it is noise (the "Paused while the user
/// never touched anything" case) and the paired TrackChanged, if any, already
/// covers the pill. When it differs, the recreation was caused by the user's
/// own pause/play and the event is the real transition. `None` — the source's
/// first session, or a transitional status that produced no event — is
/// treated as spurious, matching the historical behavior of dropping
/// unconditionally.
fn spurious_recreated_playback(known: Option<PlaybackState>, reported: Option<PlaybackState>) -> bool {
    known.is_none() || known == reported
}

/// True if a session still needs a poll-driven artwork read: it has no artwork
/// yet and has not exhausted its retry budget. Used by the 2-second safety-net
/// poll to chase a thumbnail that an earlier event-driven read missed (SMTC
/// populates the thumbnail a moment after the title), without re-reading art
/// on every poll pass for sessions that already have it.
fn should_poll_artwork(state: Option<&LogicalState>) -> bool {
    let prev = match state {
        Some(p) => p,
        None => return false,
    };
    !prev.has_artwork && prev.artwork_attempts < ARTWORK_RETRY_BUDGET
}

/// Whether a retry-driven artwork read should emit a TrackChanged. The retry
/// exists to surface artwork the event path missed, so it only emits when the
/// read found artwork AND that artwork is not already known for this track
/// (same title + artist as the last emitted track). A recreated session
/// re-reporting a track whose cover is already shown must not re-emit.
fn retry_should_emit(merged: &TrackInfo, last_track: Option<&TrackInfo>) -> bool {
    merged.artwork.is_some()
        && !last_track.is_some_and(|cached| {
            cached.title == merged.title && cached.artist == merged.artist && cached.artwork.is_some()
        })
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
        decoded_art: None,
        app_icon: read.app_icon.clone(),
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

/// Attaches the worker's fixed-size artwork decode to a track about to be
/// emitted. Called only on the emit paths (never on poll/merge reads), so the
/// image decode runs once per actually-emitted track — on the worker thread,
/// never on a window's UI thread.
fn with_decoded_art(mut track: TrackInfo) -> TrackInfo {
    track.decoded_art = track
        .artwork
        .as_deref()
        .and_then(|bytes| decode_artwork_pm(bytes, ARTWORK_DECODE as usize))
        .map(Arc::from);
    track
}

/// Whether a fresh-session read returned no real metadata — the title is just
/// the source-app fallback (i.e. `properties.Title()` was empty) and the artist
/// is also empty. Such a read is a placeholder: it should not defer the pill
/// (which would force-show a spurious title after the 2s timeout). The real
/// `MediaPropertiesChanged` event, or the periodic poll, will surface actual
/// data when YouTube Music (or another source) populates it.
fn is_placeholder_read(prev: &LogicalState, merged: &TrackInfo) -> bool {
    let is_first_read = prev.source_app.is_empty() && prev.title.is_empty();
    is_first_read && merged.artist.is_empty() && merged.title == merged.source_app
}

fn register_sessions_handler(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
    signal_tx: SyncSender<Signal>,
) -> Result<EventRegistrationToken> {
    let handler: TypedEventHandler<GlobalSystemMediaTransportControlsSessionManager, SessionsChangedEventArgs> =
        TypedEventHandler::new(move |_, _| {
            let _ = signal_tx.try_send(Signal::Sessions);
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
    signal_tx: SyncSender<Signal>,
) -> Result<EventRegistrationToken> {
    let handler: TypedEventHandler<GlobalSystemMediaTransportControlsSessionManager, CurrentSessionChangedEventArgs> =
        TypedEventHandler::new(move |_, _| {
            let _ = signal_tx.try_send(Signal::Sessions);
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

/// Bounds and canonicalizes an SMTC-provided metadata string. Sources are
/// untrusted input from other applications: a pathological value must not be
/// retained at arbitrary length in history rows, tooltips or the pill, and
/// cosmetic whitespace must not split one track into two.
///
/// Trailing/leading whitespace is trimmed because some sources report the
/// same title inconsistently (Brave emits a YouTube title once clean and once
/// padded with trailing spaces ~450 ms later). Every dedup and identity
/// comparison downstream (`content_differ`, `is_session_recreation`,
/// `cached_artwork_for`) compares byte-exact, so without trimming the padded
/// variant escapes dedup and fires a duplicate pill. Whitespace never renders,
/// so the trim is invisible; when it does bite, the normalization log line
/// below makes it auditable.
fn cap_meta(value: String) -> String {
    let trimmed = value.trim();
    if trimmed.len() != value.len() {
        debug!(
            "metadata normalized | raw={value:?} ({} chars) | trimmed={trimmed:?} ({} chars)",
            value.chars().count(),
            trimmed.chars().count()
        );
    }
    if trimmed.chars().count() > MAX_META_CHARS {
        trimmed.chars().take(MAX_META_CHARS).collect()
    } else {
        trimmed.to_string()
    }
}

const MAX_META_CHARS: usize = 256;

/// Best-effort title/artist for a session's history row. Reads can fail or
/// return empty for freshly-created sessions; the title falls back to the
/// source label so the row always names the app.
fn read_session_text(session: &GlobalSystemMediaTransportControlsSession, source_app: &str) -> (String, String) {
    let Ok(properties) = session.TryGetMediaPropertiesAsync().and_then(|op| op.get()) else {
        return (source_app.to_string(), String::new());
    };
    let title = cap_meta(non_empty(
        properties.Title().map(|v| v.to_string()).unwrap_or_default(),
        source_app,
    ));
    let artist = cap_meta(non_empty(
        properties.Artist().map(|v| v.to_string()).unwrap_or_default(),
        "",
    ));
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
    let title = cap_meta(non_empty(properties.Title()?.to_string(), &source_app));
    // Keep artist empty when the app has not provided it yet; the pill hides
    // the artist row instead of showing "Unknown" (which duplicates the
    // source-app line and shows a made-up name).
    let artist = cap_meta(non_empty(properties.Artist()?.to_string(), ""));
    // Keep album empty when the app has not provided it yet; renderers hide the
    // album line until real data arrives (prevents a bogus "Unknown album").
    let album = cap_meta(non_empty(properties.AlbumTitle()?.to_string(), ""));
    // Album artist and subtitle are read as additional data sources. Some apps
    // (e.g. YouTube Music) populate only Title/Artist and leave these empty,
    // but others may fill one but not the album title — the pill falls back to
    // whichever is available.
    let album_artist = cap_meta(non_empty(properties.AlbumArtist()?.to_string(), ""));
    let subtitle = cap_meta(non_empty(properties.Subtitle()?.to_string(), ""));
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
    // Share the byte buffer via Arc: the event is cloned into two window
    // queues, so a per-clone copy of a multi-MB thumbnail is pure waste.
    let artwork = artwork.map(Arc::from);
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
        let joined = cap_meta(genres.join(", "));
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
        decoded_art: None,
        app_icon: None,
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
    // Compare the HRESULT codes of the windows error in the chain (raw when
    // propagated with `?`, or wrapped as the source by read_thumbnail) —
    // never the formatted error text.
    const RPC_SERVER_UNAVAILABLE: u32 = 0x8007_06BA;
    const DEVICE_NOT_READY: u32 = 0x8007_0015;
    error.chain().any(|cause| {
        cause
            .downcast_ref::<windows::core::Error>()
            .is_some_and(|e| matches!(e.code().0 as u32, RPC_SERVER_UNAVAILABLE | DEVICE_NOT_READY))
    })
}

fn read_thumbnail(
    properties: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties,
) -> Result<Option<Vec<u8>>> {
    let reference = properties
        .Thumbnail()
        .map_err(|e| anyhow::Error::new(e).context("Thumbnail failed"))?;
    let stream = reference
        .OpenReadAsync()
        .map_err(|e| anyhow::Error::new(e).context("OpenReadAsync failed"))?
        .get()
        .map_err(|e| anyhow::Error::new(e).context("OpenReadAsync get failed"))?;
    let size = stream
        .Size()
        .map_err(|e| anyhow::Error::new(e).context("Size failed"))?;
    if size == 0 || !(1024..=8 * 1024 * 1024).contains(&size) || size > u32::MAX as u64 {
        return Ok(None);
    }
    let size = size as u32;
    let buffer = Buffer::Create(size).map_err(|e| anyhow::Error::new(e).context("Buffer::Create failed"))?;
    stream
        .ReadAsync(&buffer, size, InputStreamOptions::None)
        .map_err(|e| anyhow::Error::new(e).context("ReadAsync failed"))?
        .get()
        .map_err(|e| anyhow::Error::new(e).context("ReadAsync get failed"))?;
    let reader =
        DataReader::FromBuffer(&buffer).map_err(|e| anyhow::Error::new(e).context("DataReader::FromBuffer failed"))?;
    let mut data = vec![0u8; size as usize];
    reader
        .ReadBytes(&mut data)
        .map_err(|e| anyhow::Error::new(e).context("ReadBytes failed"))?;
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
pub(crate) fn normalize_for_match(s: &str) -> String {
    s.to_lowercase().replace(['-', '_', '.', ' '], "")
}

fn debounce_duration(config: &Config) -> Duration {
    Duration::from_millis(config.behavior.debounce_ms.clamp(150, 250))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

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
    fn current_session_filter_rejects_stale_sessions_for_the_same_source() {
        assert!(session_matches_current_source(10, "spotify", Some(10), Some("spotify")));
        assert!(!session_matches_current_source(
            11,
            "spotify",
            Some(10),
            Some("spotify")
        ));
        // A different source remains independent.
        assert!(session_matches_current_source(
            11,
            "youtube-music",
            Some(10),
            Some("spotify")
        ));
        // A transient GetCurrentSession failure uses the permissive fallback.
        assert!(session_matches_current_source(11, "spotify", None, None));
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
    fn cap_meta_trims_whitespace_and_caps_length() {
        // The Brave case: same title reported once clean, once padded with
        // trailing spaces. After normalization both must compare equal, so
        // content_differ / is_session_recreation cannot split one track into
        // two duplicate pills.
        assert_eq!(cap_meta("  Song  ".into()), "Song");
        assert_eq!(cap_meta("Song \u{2009}".into()), "Song");
        assert_eq!(cap_meta("Song".into()), "Song");
        assert_eq!(cap_meta("   ".into()), "");
        // The length cap still applies after trimming.
        let long = format!("{}{}", "x".repeat(300), "   ");
        assert_eq!(cap_meta(long).chars().count(), MAX_META_CHARS);
    }

    #[test]
    fn whitespace_padded_title_does_not_escape_dedup() {
        // Regression: Brave reported "The Season 5 Premiere Is Worse Than
        // Anyone Expected" and the same title with 14 trailing spaces ~450ms
        // later; byte-exact comparison treated the padded variant as a new
        // track and fired a duplicate pill. Every read passes through
        // cap_meta at the boundary, so by the time content_differ and
        // is_session_recreation compare, both variants are the same string:
        let clean = cap_meta("The Season 5 Premiere Is Worse Than Anyone Expected".into());
        let padded = cap_meta("The Season 5 Premiere Is Worse Than Anyone Expected              ".into());
        assert_eq!(clean, padded);
        let prev = track(&clean, "Artist");
        let merged = track(&padded, "Artist");
        assert!(!content_differ(&state(&clean, "Artist"), &merged));
        assert!(is_session_recreation(&prev, &merged, true));
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
            artwork: Some(Arc::from(vec![1])),
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
    fn is_placeholder_read_detects_empty_metadata_on_new_session() {
        let empty = LogicalState::default();
        // Title fell back to source_app, artist is empty → placeholder.
        let placeholder = TrackInfo {
            title: "youtube-music".into(),
            artist: "".into(),
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        };
        assert!(is_placeholder_read(&empty, &placeholder));

        // Real title + empty artist → not a placeholder (source provided metadata).
        let real_title = TrackInfo {
            title: "Song".into(),
            artist: "".into(),
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        };
        assert!(!is_placeholder_read(&empty, &real_title));

        // Real title + real artist → not a placeholder.
        let real = track("Song", "Artist");
        assert!(!is_placeholder_read(&empty, &real));

        // Not a first read → not a placeholder (even if fields are empty).
        let stored = state("Song", "Artist");
        assert!(!is_placeholder_read(&stored, &placeholder));
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
        // The thumbnail path wraps the same windows error as the chain
        // source with context; the code must still match through it.
        let wrapped =
            anyhow::Error::new(windows::core::Error::from(HRESULT(0x8007_06BAu32 as i32))).context("Thumbnail failed");
        assert!(is_session_gone(&wrapped));
    }

    #[test]
    fn cached_artwork_reused_only_for_same_track_identity() {
        // Build a last_track_per_source map directly — no ListenerState needed.
        let mut last_track_per_source = HashMap::new();
        let art: Arc<[u8]> = Arc::from(vec![0x89, 0x50, 0x4E, 0x47]);
        last_track_per_source.insert(
            "youtube-music".to_string(),
            TrackInfo {
                title: "Song".into(),
                artist: "Artist".into(),
                artwork: Some(art.clone()),
                ..TrackInfo::default()
            },
        );

        // Same source + title + artist → cached artwork returned.
        assert_eq!(
            cached_artwork_for(&last_track_per_source, "youtube-music", "Song", "Artist"),
            Some(art.clone())
        );

        // Cross-track (same source, different title) → None (no bleed).
        assert_eq!(
            cached_artwork_for(&last_track_per_source, "youtube-music", "Other", "Artist"),
            None
        );

        // Cross-source → None.
        assert_eq!(
            cached_artwork_for(&last_track_per_source, "spotify", "Song", "Artist"),
            None
        );

        // No cached entry → None.
        assert_eq!(cached_artwork_for(&HashMap::new(), "unknown", "Song", "Artist"), None);
    }

    #[test]
    fn session_recreation_dedup_logic() {
        // Same track, same artwork presence → suppressed (session recreation noise).
        let prev = track("Song", "Artist");
        let merged = track("Song", "Artist");
        assert!(is_session_recreation(&prev, &merged, true));

        // Same track, artwork gained → NOT suppressed (legitimate in-place refresh).
        let with_art = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            artwork: Some(Arc::from(vec![1])),
            ..TrackInfo::default()
        };
        assert!(!is_session_recreation(&prev, &with_art, true));

        // Same track, artwork lost → NOT suppressed.
        assert!(!is_session_recreation(&with_art, &prev, true));

        // Different track → NOT suppressed.
        assert!(!is_session_recreation(&prev, &track("Other", "Artist"), true));

        // Different artist → NOT suppressed.
        assert!(!is_session_recreation(
            &track("Song", "Artist"),
            &track("Song", "Other"),
            true,
        ));
    }

    #[test]
    fn session_recreation_does_not_suppress_a_cover_swap() {
        // Same track re-reported with a different cover (video vs audio
        // version): artwork bytes differ, so it is NOT a recreation — the
        // artwork-changed emit must pass through instead of being suppressed.
        let prev = TrackInfo {
            title: "Love Me Not".into(),
            artist: "Ravyn Lenae".into(),
            artwork: Some(Arc::from(vec![1])),
            ..TrackInfo::default()
        };
        let swapped = TrackInfo {
            artwork: Some(Arc::from(vec![2])),
            ..prev.clone()
        };
        assert!(!is_session_recreation(&prev, &swapped, true));
        // Identical bytes (fresh Arc, same content) still count as recreation.
        let same_bytes = TrackInfo {
            artwork: Some(Arc::from(vec![1])),
            ..prev.clone()
        };
        assert!(is_session_recreation(&prev, &same_bytes, true));
    }

    #[test]
    fn session_recreation_dedup_skips_artwork_clause_for_poll_reads() {
        // The Bleed It Out case: last emit had art (Some), but the poll read
        // always produces artwork=None (read_artwork=false). Without the fix,
        // is_some()==is_some() → Some==None → false → emitted as a duplicate.
        let last_emit = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            artwork: Some(Arc::from(vec![1])),
            ..TrackInfo::default()
        };
        let poll_read = track("Song", "Artist");

        // OLD (buggy) behavior: suppressed == false (escapes dedup → duplicate).
        assert!(!old_is_session_recreation(&last_emit, &poll_read));

        // NEW behavior: poll read skips artwork clause → suppressed.
        assert!(is_session_recreation(&last_emit, &poll_read, false));

        // Event read with art still gained → NOT suppressed (in-place refresh).
        assert!(!is_session_recreation(&last_emit, &poll_read, true));
    }

    /// OLD (buggy) predicate, for diff regression testing: compares artwork
    /// presence unconditionally, which misfires when a poll read (always None)
    /// follows an emit that had art.
    fn old_is_session_recreation(prev: &TrackInfo, merged: &TrackInfo) -> bool {
        prev.title == merged.title && prev.artist == merged.artist && prev.artwork.is_some() == merged.artwork.is_some()
    }

    #[test]
    fn recreated_session_playback_guard_keeps_real_state_changes() {
        // Unknown source (first-ever session): treat as spurious, as before.
        assert!(spurious_recreated_playback(None, Some(PlaybackState::Paused)));
        assert!(spurious_recreated_playback(None, Some(PlaybackState::Playing)));
        // State unchanged (recreation while paused/playing, user idle): noise.
        assert!(spurious_recreated_playback(
            Some(PlaybackState::Paused),
            Some(PlaybackState::Paused)
        ));
        assert!(spurious_recreated_playback(
            Some(PlaybackState::Playing),
            Some(PlaybackState::Playing)
        ));
        // State actually changed: the recreation was the user's pause/play.
        assert!(!spurious_recreated_playback(
            Some(PlaybackState::Playing),
            Some(PlaybackState::Paused)
        ));
        assert!(!spurious_recreated_playback(
            Some(PlaybackState::Paused),
            Some(PlaybackState::Playing)
        ));
        // Transitional status (no report) with a known state: nothing to
        // drop — the retain is a no-op without a playback event anyway.
        assert!(!spurious_recreated_playback(Some(PlaybackState::Paused), None));
    }

    #[test]
    fn artwork_retry_budget_bounds_poll_attempts() {
        // No state yet: no retry.
        assert!(!should_poll_artwork(None));
        // No art + attempts < budget → retry.
        let mut s = LogicalState::default();
        assert!(should_poll_artwork(Some(&s)));
        // Budget exhausted → stop retrying.
        s.artwork_attempts = ARTWORK_RETRY_BUDGET;
        assert!(!should_poll_artwork(Some(&s)));
        // Artwork present → no retry.
        s.artwork_attempts = 0;
        s.has_artwork = true;
        assert!(!should_poll_artwork(Some(&s)));
    }

    #[test]
    fn retry_artwork_only_surfaces_new_artwork() {
        // Artwork found, no cached track → emit (genuine new track).
        let with_art = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            artwork: Some(Arc::from(vec![1])),
            ..TrackInfo::default()
        };
        assert!(retry_should_emit(&with_art, None));

        // Artwork found, same track already shown WITH art → suppressed
        // (the recreation case that produced the paused-pill spam).
        assert!(!retry_should_emit(&with_art, Some(&with_art)));

        // Artwork found, same track shown WITHOUT art → emit (artwork gain).
        let no_art = track("Song", "Artist");
        assert!(retry_should_emit(&with_art, Some(&no_art)));

        // No artwork found → never emit (nothing to surface).
        assert!(!retry_should_emit(&no_art, None));
        assert!(!retry_should_emit(&no_art, Some(&no_art)));
        assert!(!retry_should_emit(&no_art, Some(&with_art)));

        // Different track with art → emit.
        let other = TrackInfo {
            title: "Other".into(),
            artist: "Artist".into(),
            artwork: Some(Arc::from(vec![2])),
            ..TrackInfo::default()
        };
        assert!(retry_should_emit(&other, Some(&with_art)));
    }
}
