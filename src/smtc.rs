use crate::config::Config;
use crate::events::{MediaEvent, PlaybackState, PlaybackType, TrackInfo, decode_artwork_pm};
use crate::palette::{Palette, palette_from_rgba};
use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Media::Control::{
    CurrentSessionChangedEventArgs, GlobalSystemMediaTransportControlsSession,
    GlobalSystemMediaTransportControlsSessionManager, GlobalSystemMediaTransportControlsSessionPlaybackStatus,
    MediaPropertiesChangedEventArgs, PlaybackInfoChangedEventArgs, SessionsChangedEventArgs,
    TimelinePropertiesChangedEventArgs,
};
use windows::Media::MediaPlaybackType;
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
    Timeline(GlobalSystemMediaTransportControlsSession),
}

struct SessionSubscription {
    session: GlobalSystemMediaTransportControlsSession,
    properties_token: EventRegistrationToken,
    playback_token: EventRegistrationToken,
    timeline_token: EventRegistrationToken,
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
    /// Content type the session last reported (`PlaybackInfo.PlaybackType`).
    /// Used to inherit the type across poll reads (which never report it
    /// freshly) and to suppress `Image` content wholesale.
    playback_type: PlaybackType,
    /// When the first read was deferred waiting for artwork: the pill shows
    /// anyway once this timestamp is older than `ARTWORK_TIMEOUT`.
    deferred_at: Option<Instant>,
    /// Whether the pending `deferred_at` deferral was a stale-art drop (a
    /// same-cover re-read held back for the artwork retry) rather than a
    /// first-read awaiting-artwork deferral. The poll force must not
    /// shortcut the stale-art deferral while the retry budget is unconsumed,
    /// or it would flash an artless pill the retry immediately re-emits
    /// with art.
    deferred_for_stale_art: bool,
    /// Number of poll-driven artwork retries attempted for this session. Bounded
    /// by `ARTWORK_RETRY_BUDGET` so a session that never provides a thumbnail is
    /// not re-read indefinitely (the 2s poll interval keeps this cheap).
    artwork_attempts: u8,
    /// When this session's state was last refreshed by a successful read
    /// (event-driven or poll). The periodic poll skips sessions whose read is
    /// newer than `SESSION_CHECK_INTERVAL` — their state was just re-read by
    /// the event that woke the worker, so a second read is pure WinRT churn.
    last_read_at: Option<Instant>,
    /// Last reported playback position in whole seconds, for seek detection.
    /// Whole seconds (u64) keep LogicalState Eq-derivable; precision is ample
    /// for a 3 s seek threshold. Does NOT drive rendering (that is TrackInfo).
    last_position_secs: Option<u64>,
}

/// Rolling window, threshold and cool-down for the per-source session-churn
/// guard. A source creating more than `CHURN_THRESHOLD` new sessions within
/// `CHURN_WINDOW_MS` (a real client was observed doing ~20 in 8.5s) is
/// excluded from tracking for the cool-down period. Churn is charged only for
/// content-free sessions (see `record_churn`): sessions that never carried a
/// track are identity garbage, while a source that recreates its session per
/// real track change (YouTube Music) carries a title and never counts.
const CHURN_WINDOW_MS: u64 = 2000;
const CHURN_THRESHOLD: usize = 5;
const CHURN_COOLDOWN_MS: u64 = 30_000;
/// A position jump larger than this (seconds) between reads is treated as a
/// user seek, not ordinary playback advance, and re-emits the track so the
/// overlay re-bases its progress estimate. Well above the ~1 s/event cadence
/// of TimelinePropertiesChanged, so ordinary playback never trips it.
const SEEK_DELTA_SECS: f64 = 3.0;

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

/// How often the safety net re-syncs sessions and re-reads subscribed
/// sessions (metadata only). Also the freshness window for the poll skip: a
/// session read within this interval by an event-driven refresh is not
/// re-read by the poll.
const SESSION_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// How long a source may be absent from the session snapshot before the
/// worker settles its terminal `Stopped`. YouTube Music tears down and
/// re-registers its SMTC session on every track change (and periodically
/// mid-song), leaving a gap where the source is absent from one snapshot;
/// settling inside that gap fires a spurious STOP followed by a fresh
/// PLAYING pill for the same song. Twice the check interval: a recreated
/// session re-registers within one poll, and a genuinely quitting app still
/// retires its pill ~4s after the last session goes.
const TERMINAL_STOP_GRACE: Duration = Duration::from_secs(4);

/// Capacity of the signal channel between the WinRT event handlers and the
/// listener loop. `try_send` drops a signal when the queue is full; that is
/// safe because every dropped signal is a coalescible wake-up — the dirty-set
/// membership it would have recorded is re-covered by the periodic safety-net
/// poll within 2s. The bound keeps a session storm from accumulating
/// unbounded queued COM session references.
const SIGNAL_QUEUE_CAP: usize = 256;

/// Hard admission caps defending the worker against a hostile process that
/// registers unbounded GSMTC sessions/sources. A single desktop attacker can
/// create unique sessions and sources at will; without these bounds the
/// subscription map, per-source caches, and retained 8 MiB thumbnails would
/// grow with the attacker-controlled set. The caps prioritize the current
/// session and existing subscriptions, so a compliant active source is never
/// squeezed out by a storm of new ones.
const MAX_TRACKED_SESSIONS: usize = 64;
const MAX_TRACKED_SOURCES: usize = 32;

/// Largest single thumbnail the worker will read from an SMTC session, and the
/// total retained raw-artwork budget across `last_track_per_source`. The
/// per-source cap keeps one absurd thumbnail from consuming the whole budget;
/// the total cap keeps a long-lived session from hoarding art indefinitely.
/// When the total would be exceeded, artwork bytes are dropped while the
/// metadata is retained, so the pill renders a placeholder instead of a stale
/// cover.
const MAX_THUMBNAIL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RETAINED_ARTWORK_BYTES: usize = 64 * 1024 * 1024;
/// Cap for the recoverable output retry mailbox (`pending_output`). The
/// mailbox exists so a briefly-full output channel cannot make a committed
/// state transition permanently invisible: events wait here and are re-sent
/// at the next event-loop turn. 256 matches the per-window queue caps.
const OUTPUT_RETRY_CAP: usize = 256;

/// Minimum gap between two overflow warnings, so a hostile storm of rejected
/// sessions cannot flood the log with one WARN per rejected session.
const OVERFLOW_WARN_INTERVAL: Duration = Duration::from_secs(60);

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
    output: SyncSender<Arc<MediaEvent>>,
    /// Recoverable retry mailbox for events the bounded output channel could
    /// not accept immediately. Bounded; coalesced by (kind, source) with the
    /// newest superseding, drained in arrival order at every event-loop turn.
    /// Never blocks the worker: overflow drops the oldest superseded event,
    /// and the 2-second safety-net poll repairs state on top.
    pending_output: VecDeque<Arc<MediaEvent>>,
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
    /// Source apps whose last session reported Closed (or vanished from the
    /// session snapshot between syncs) and still owe the overlay a terminal
    /// `Stopped`, so a persistent pill does not linger at idle opacity
    /// forever. Each entry records when the source was first seen gone;
    /// `sync_subscriptions` settles the Stopped only once the source has been
    /// absent for `TERMINAL_STOP_GRACE`, so a source that recreates its
    /// session (YouTube Music does this on every track change) does not fire
    /// a spurious STOP in the snapshot gap.
    terminal_pending: HashMap<String, Instant>,
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
    /// The source the overlay's pill is currently displaying, published by
    /// the overlay into a shared cell (see `OverlayState::now_showing`). The
    /// session-recreation dedup below only applies while the pill already
    /// represents this source: an identical re-report after another app's
    /// pill is a switch-back and must re-emit (the pill needs to come back
    /// with the track's artwork), not noise. The overlay owns the truth
    /// here: an event this worker emitted can be queued or superseded on the
    /// overlay side, so attributing from this worker's emissions would
    /// suppress a re-emit whose pill never actually appeared.
    now_showing: Arc<Mutex<Option<String>>>,
    /// Cached app icons keyed by source_app label (derived from AUMID via
    /// `source_app_label`). Populated on first encounter of a source.
    icon_cache: HashMap<String, Option<Arc<[u8]>>>,
    /// When the last overflow warning fired. Bounds the admission-rejection log
    /// to one line per `OVERFLOW_WARN_INTERVAL` during a hostile session storm,
    /// instead of one WARN per rejected session.
    last_overflow_warn: Option<Instant>,
    /// Last-seen `media_sources` config list plus its pre-normalized
    /// patterns. The per-session check runs on the hot path (every re-sync
    /// of every session), so the clone of the config list and the per-pattern
    /// normalization only re-run when the config actually changed.
    cached_allowed: Option<(Vec<String>, Vec<String>)>,
    /// When the last TrackChanged was emitted per source, used to time-gate
    /// the artwork-changed re-emit: SMTC re-reads the thumbnail within ~1s
    /// of a change and may return different bytes for the same cover, which
    /// would otherwise fire a duplicate pill for the same song.
    last_emit_at: HashMap<String, Instant>,
    /// The two-color palette derived per track identity (source + title +
    /// artist), from the first trusted artwork decode for that identity. A
    /// source that re-encodes its thumbnail between reads supplies different
    /// bytes for the same cover; reusing this cache keeps the pill's accent
    /// colors stable for the identity. Cleared when a real cover swap is
    /// detected (the artwork-changed force), so a genuinely new cover
    /// recomputes its palette. Keyed by `palette_cache_key`; bounded by
    /// `PALETTE_CACHE_CAP` and pruned of departed sources in
    /// `sync_subscriptions`.
    palette_per_identity: HashMap<String, Palette>,
}

pub struct SmtcListener {
    output: SyncSender<Arc<MediaEvent>>,
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
    /// Shared now-showing cell (see `ListenerState::now_showing`). Survives
    /// worker restarts: the supervisor spawns every worker with the same
    /// cell, so a session recreated after a restart still compares against
    /// what the user actually sees.
    now_showing: Arc<Mutex<Option<String>>>,
}

impl SmtcListener {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        output: SyncSender<Arc<MediaEvent>>,
        config: Arc<RwLock<Config>>,
        heartbeat: Arc<Mutex<Instant>>,
        live_generation: Arc<AtomicU64>,
        my_generation: u64,
        shutdown: Arc<AtomicBool>,
        now_showing: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            output,
            config,
            heartbeat,
            live_generation,
            my_generation,
            shutdown,
            now_showing,
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
            self.now_showing,
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
        output: SyncSender<Arc<MediaEvent>>,
        signal_tx: SyncSender<Signal>,
        heartbeat: Arc<Mutex<Instant>>,
        live_generation: Arc<AtomicU64>,
        my_generation: u64,
        shutdown: Arc<AtomicBool>,
        now_showing: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            manager,
            config,
            output,
            pending_output: VecDeque::new(),
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
            terminal_pending: HashMap::new(),
            last_track_per_source: HashMap::new(),
            last_known_playback_per_source: HashMap::new(),
            now_showing,
            icon_cache: HashMap::new(),
            cached_allowed: None,
            last_emit_at: HashMap::new(),
            palette_per_identity: HashMap::new(),
            last_overflow_warn: None,
            heartbeat,
            live_generation,
            my_generation,
            shutdown,
        }
    }

    fn event_loop(&mut self, signal_rx: Receiver<Signal>) -> Result<()> {
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
                .unwrap_or(SESSION_CHECK_INTERVAL)
                .min(SESSION_CHECK_INTERVAL);

            match signal_rx.recv_timeout(timeout) {
                Ok(signal) => {
                    self.handle_signal(signal)?;
                    // A continuous signal stream must not starve the debounce
                    // flush or the periodic safety net: run both once their
                    // deadline has passed, regardless of how many signals
                    // arrived in between.
                    if self.pending_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                        self.flush();
                    }
                    if self.last_session_check.elapsed() >= SESSION_CHECK_INTERVAL {
                        self.last_session_check = Instant::now();
                        self.sync_subscriptions();
                        self.poll_sessions();
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.flush();
                    // Periodic safety net: re-sync (a session can appear
                    // without a SessionsChanged event) and re-read every
                    // subscribed session (metadata only, no artwork) so a
                    // missed event still surfaces.
                    if self.last_session_check.elapsed() >= SESSION_CHECK_INTERVAL {
                        self.last_session_check = Instant::now();
                        self.sync_subscriptions();
                        self.poll_sessions();
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // The forwarder is gone; nothing queued can be delivered.
                    self.pending_output.clear();
                    break;
                }
            }
            // Recoverable delivery runs once per turn, after the debounce
            // flush and safety-net poll and before the next blocking receive.
            self.flush_output();
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
            Signal::MediaProperties(session) | Signal::PlaybackInfo(session) | Signal::Timeline(session) => {
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
        let playback_info = session.GetPlaybackInfo()?;
        let status = playback_info.PlaybackStatus()?;
        if status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Closed {
            // Closed does not go through the diff/emit path: it usually fires
            // as the app quits, right after a Stopped/Paused already told the
            // user what happened, so the entry is evicted immediately and
            // nothing is emitted. But an app can also quit without a terminal
            // state report — the persistent pill would then keep the last
            // track at idle opacity forever. Remember the source so the next
            // sync can settle a terminal Stopped once the session (and any
            // sibling session) is really gone from the snapshot.
            let source = read_source_app(session);
            debug!("SMTC session closed | key={key} | source={source}");
            self.evict(key);
            self.terminal_pending.insert(source, Instant::now());
            return Ok(());
        }
        let playback = snapshot_playback_state(status);
        let prev = self.states.get(&key).cloned().unwrap_or_default();
        let mut next = prev.clone();
        // True until the first successful read (the stored state is the
        // default): used for the first-read artwork deferral and to charge
        // churn only for brand-new content-free sessions.
        let is_first_read = prev.source_app.is_empty() && prev.title.is_empty();
        let mut events: Vec<MediaEvent> = Vec::new();

        // Image content (slideshows, photo apps) is not "now playing": no
        // pill fires for it — neither the track nor the paired state event.
        // Read early (playback_info is already fetched) and return before the
        // playback diff, so an image session can never push a pill. Logged
        // once per type transition so the 2s poll does not spam.
        let playback_type = session_playback_type(session);
        if playback_type == PlaybackType::Image {
            if prev.playback_type != PlaybackType::Image {
                let label = read_source_app(session);
                debug!("pill suppressed | reason=image-content | source={label}");
            }
            return Ok(());
        }

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
            events.push(MediaEvent::PlaybackStateChanged(state, source.clone()));
        }

        // Content is only diffed while the session is not stopped; a stopped
        // session keeps its stored content (the pill shows the last track).
        if status != GlobalSystemMediaTransportControlsSessionPlaybackStatus::Stopped {
            match read_track_info(
                session,
                read_artwork,
                playback,
                playback_info.PlaybackRate().ok().and_then(|r| r.Value().ok()),
                playback_type,
            ) {
                Ok(read) => {
                    let mut merged = merge_track(&prev, &read, read_artwork);
                    // Record the last reported position (whole seconds) for
                    // seek detection on the next read; position itself is
                    // carried to the overlay via TrackInfo, not LogicalState.
                    next.last_position_secs = read.position_secs.map(|s| s as u64);
                    // Push a lightweight progress update so the overlay bar tracks
                    // live position and seeks directly, without waiting for a
                    // TrackChanged re-emit (which only fires on a content change
                    // or a detected seek).
                    if read.position_secs.is_some() {
                        events.push(MediaEvent::ProgressChanged {
                            source_app: read.source_app.clone(),
                            position_secs: read.position_secs,
                            duration_secs: read.duration_secs,
                            playback_rate: read.playback_rate,
                        });
                    }
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
                    // Stale-thumbnail guard: a transition read can pair the NEW
                    // track identity with the PREVIOUS track's thumbnail bytes
                    // (SMTC updates the thumbnail stream after the text fields).
                    // Byte-equal cross-identity art is dropped here — attaching
                    // it would show the wrong cover and poison the identity-
                    // keyed artwork and palette caches. Same-identity reads
                    // always keep their art, so a legitimately shared cover
                    // within an album survives; the artwork-changed re-emit
                    // surfaces the real cover once the stream catches up.
                    let stale_dropped =
                        read_artwork && stale_thumbnail(&merged, self.last_track_per_source.get(&merged.source_app));
                    if stale_dropped {
                        merged.artwork = None;
                        let label = track_label(&merged);
                        debug!("stale thumbnail dropped | reason=identity-switch | {label}");
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
                    let placeholder = is_placeholder_like(&merged);
                    // Session churn is charged on a session's first read and
                    // only for content-free sessions: a newly-created session
                    // whose title fell back to the source-app label carries no
                    // track (the Riot signature). A legitimately recreated
                    // session from a real skip always reports a title on its
                    // first read and is never counted, so rapid skipping never
                    // trips the cool-down. Charging here (not at admission,
                    // where every new session counted) makes the guard match
                    // what the source actually emitted.
                    if first_read_counts_toward_churn(is_first_read, &merged) {
                        self.record_churn(&merged.source_app);
                    }
                    // A metadata snapshot that is just the source-app fallback (empty
                    // title + empty artist) carries no real track to announce: drop it
                    // everywhere below so it can never flash as a "sample track". A
                    // real MediaPropertiesChanged or the poll supersedes it once the
                    // source populates its metadata.
                    if placeholder {
                        debug!("track emit skipped | reason=placeholder | source={}", merged.source_app);
                    }
                    // Safety net: a first pill deferred for artwork shows
                    // anyway after ARTWORK_TIMEOUT, so a source that never
                    // provides a thumbnail still gets its pill — but never for a
                    // placeholder read (title is just the source-app fallback),
                    // which must not be announced as a "sample track". A real
                    // MediaPropertiesChanged or the poll will surface the actual
                    // track when its metadata lands.
                    if !emit && !placeholder && defer_expired(prev.deferred_at) && poll_force_allowed(&prev) {
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
                            // The user's real pause/play is already in the batch:
                            // YouTube Music re-encodes its thumbnail when it
                            // recreates the session on pause, so the same cover
                            // can re-read with different bytes. Forcing a
                            // TrackChanged here would make the batch rule below
                            // drop the state event, and the pill would show the
                            // track layout instead of the pause. Absorb the
                            // refresh instead: record the new bytes as the
                            // source's last emitted track so a later read dedups
                            // against them, and let the state event carry the
                            // pill. A later genuine cover swap still re-reads
                            // differently and emits normally.
                            if artwork_refresh_absorbed(&events, &merged) {
                                store_last_track(
                                    &mut self.last_track_per_source,
                                    merged.source_app.clone(),
                                    merged.clone(),
                                    MAX_RETAINED_ARTWORK_BYTES,
                                );
                                self.last_emit_at.insert(merged.source_app.clone(), Instant::now());
                                let label = track_label(&merged);
                                debug!("artwork refresh absorbed | reason=state-change-in-batch | {label}");
                            } else {
                                emit = true;
                                // Genuine cover change for the same identity:
                                // invalidate the cached palette so the emit
                                // recomputes from the new bytes instead of
                                // carrying the old cover's accent colors.
                                self.palette_per_identity.remove(&palette_cache_key(
                                    &merged.source_app,
                                    &merged.title,
                                    &merged.artist,
                                ));
                                let label = track_label(&merged);
                                debug!("track emit forced | reason=artwork-changed | {label}");
                            }
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
                    // Suppression applies only while the pill already on screen
                    // belongs to this source (the overlay's now-showing cell): after another
                    // app's pill, the identical re-report is a switch-back that
                    // must re-emit — the overlay's cache for this source may
                    // already be evicted, so the pill needs the fresh track
                    // (with injected art) to come back itself. The suppression
                    // is only reported when an emit would actually have fired
                    // (see the emit gate below): an unchanged re-read cannot be
                    // "suppressed" and must not be logged as one.
                    let shown_source = self.shown_source();
                    let session_recreation = should_suppress_recreation(
                        self.last_track_per_source.get(&merged.source_app),
                        &merged,
                        read_artwork,
                        shown_source.as_deref(),
                    );
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
                        // The TrackChanged carrying this read must not re-introduce
                        // the spurious state just dropped: `merge_track` copies the
                        // snapshot state unconditionally, so null it here or the pill
                        // would show a pause the user never made.
                        merged.playback_state = None;
                    }
                    // Stale-art emit gate: this read's art was byte-equal to the
                    // last emitted track and got dropped as stale — a genuinely
                    // shared album cover, or a transition-window stale buffer.
                    // Do not flash an artless pill here: the artwork retry
                    // (~2s, bypasses the stale guard) delivers the cover, so
                    // the pill appears once, with art. The paired playback
                    // event is held back like the first-read deferral, so a
                    // state pill does not render with the source's previous
                    // track; the deferred track carries the change. A later
                    // read past ARTWORK_TIMEOUT still forces the pill if the
                    // thumbnail stream never recovers, preserving the "always
                    // eventually shows something" guarantee.
                    if emit && stale_dropped {
                        emit = false;
                        next.deferred_at = Some(Instant::now());
                        next.deferred_for_stale_art = true;
                        events.retain(|e| !matches!(e, MediaEvent::PlaybackStateChanged(_, _)));
                        let label = track_label(&merged);
                        debug!("track emit deferred | reason=stale-art-drop | {label}");
                    }
                    if emit && !placeholder && !session_recreation {
                        let label = track_label(&merged);
                        info!("track changed | {label}");
                        let mut emitted = with_decoded_art(merged.clone(), crate::events::ARTWORK_DECODE as usize);
                        // Attach the identity-stable palette so the overlay does
                        // not recompute (and drift) from re-encoded thumbnails.
                        emitted.palette = self.palette_for_identity(&merged, emitted.decoded_art.as_deref());
                        events.push(MediaEvent::TrackChanged(emitted));
                        store_last_track(
                            &mut self.last_track_per_source,
                            merged.source_app.clone(),
                            merged.clone(),
                            MAX_RETAINED_ARTWORK_BYTES,
                        );
                        self.last_emit_at.insert(merged.source_app.clone(), Instant::now());
                        next.deferred_at = None;
                        next.deferred_for_stale_art = false;
                    } else if emit && session_recreation {
                        // Only an emit that would actually fire is worth logging
                        // as suppressed: the 2-second poll re-reads the current
                        // track unchanged (emit=false) and must not be reported
                        // as a suppressed recreation — it never would have
                        // emitted in the first place. Gating the log on `emit`
                        // keeps a steady-state source from flooding the log
                        // with one "suppressed" line per poll pass.
                        let label = track_label(&merged);
                        debug!("track emit suppressed | reason=session-recreation | {label}");
                    } else if artwork_lost {
                        // Absence is already shown as a placeholder: store the
                        // loss (a later reappearance re-emits) without
                        // flashing the same track again.
                        let label = track_label(&merged);
                        debug!("track emit skipped | reason=artwork-removed | {label}");
                    } else {
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
                            next.deferred_for_stale_art = false;
                            let label = track_label(&merged);
                            debug!("track emit deferred | reason=awaiting-artwork | {label}");
                        } else if read_artwork && !stale_dropped {
                            // Event-driven reads only: the 2-second poll re-reads
                            // every session and must not log a duplicate per pass.
                            // A stale-dropped read is already accounted by the
                            // deferral above (reason=stale-art-drop), not a
                            // duplicate of the last emitted track.
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
                    next.playback_type = merged.playback_type;
                    // Marked fresh for the poll skip only on a successful
                    // read: a failed read must not suppress the poll, which
                    // is the safety net for exactly that case.
                    next.last_read_at = Some(Instant::now());
                }
                Err(error) => {
                    if is_session_gone(&error) {
                        debug!("session torn down during read | key={key} | {error:#}");
                    } else {
                        debug!("track read failed | key={key} | {error:#}");
                    }
                    // A session that never yields a successful first read
                    // carries no track by definition, so it is content-free:
                    // charge churn here too, or a storm of sessions that die
                    // before their first read completes dodges the cool-down
                    // (they report nothing but still churn the session list).
                    if is_first_read {
                        self.record_churn(&read_source_app(session));
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
        // the fresh track. Same-track artwork refreshes never reach this
        // rule: the artwork-changed force absorbs them when the state event
        // is in the batch, so a pause cannot be swallowed by a re-read of the
        // same cover (see `artwork_refresh_absorbed`).
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

        // The revalidation check above runs after the read, so a session
        // that went stale mid-read never gets stored or emitted.
        self.states.insert(key, next);
        for event in events {
            self.emit(event);
        }
        Ok(())
    }

    /// Re-syncs the subscription map with the current session list: subscribes
    /// to the current session for an allowed source, drops stale subscriptions
    /// and stored state. Per-source session churn for the cool-down is charged
    /// inside `refresh_session` on first read (content-free sessions only), not
    /// here.
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
        let alive: HashSet<usize> = sessions.iter().map(session_key).collect();

        // Admission priority for the global caps: current session first, then
        // sessions already subscribed (preserving snapshot order), then
        // genuinely new sessions. A hostile storm of new sessions therefore
        // cannot evict existing subscriptions when the caps are applied:
        // overflow is rejected, and a brand-new *current* session displaces
        // the weakest survivor(s) instead of being starved
        // (`displace_survivors`) — the caps still bound the total tracked
        // set. The allow-list and current-session filters are applied within
        // the loop; the cap itself only counts sessions that pass those
        // filters.
        let mut prioritized: Vec<(GlobalSystemMediaTransportControlsSession, usize, String)> =
            Vec::with_capacity(sessions.len());
        if let Some(cur) = current.as_ref() {
            let cur_key = session_key(cur);
            if alive.contains(&cur_key) {
                prioritized.push((cur.clone(), cur_key, read_source_app(cur)));
            }
        }
        for session in &sessions {
            let key = session_key(session);
            if Some(key) == current_key {
                continue;
            }
            if before.contains(&key) {
                prioritized.push((session.clone(), key, read_source_app(session)));
            }
        }
        for session in &sessions {
            let key = session_key(session);
            if Some(key) == current_key || before.contains(&key) {
                continue;
            }
            prioritized.push((session.clone(), key, read_source_app(session)));
        }
        // Running source/session tallies seeded from subscriptions that
        // survive this sync, so the caps bound the total tracked set, not
        // only new additions. A stale subscription being evicted later in
        // this same sync must not occupy a cap slot: the caps therefore
        // count survivors only, which keeps the live loop in lockstep with
        // the `admit_sessions` test model (which also counts survivors and
        // nothing that is about to be evicted).
        let mut admitted_sources: HashSet<String> = self
            .subscriptions
            .values()
            .filter(|s| alive.contains(&session_key(&s.session)))
            .map(|s| read_source_app(&s.session))
            .collect();
        let mut admitted_sessions: usize = self
            .subscriptions
            .values()
            .filter(|s| alive.contains(&session_key(&s.session)))
            .count();
        let mut rejected_overflow: usize = 0;
        for (session, key, source) in &prioritized {
            let allowed = self.session_source_allowed(session);
            debug!(
                "SMTC session {} | key={key} | source={source} | media_sources={:?}",
                if allowed { "accepted" } else { "rejected" },
                self.config
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .behavior
                    .media_sources
            );
            if !allowed {
                // Log rejected sessions once per appearance so the history
                // shows every media source, not just the tracked ones.
                if self.rejected_seen.insert(*key) {
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
                self.evict(*key);
                continue;
            }
            if !session_matches_current_source(*key, source, current_key, current_source.as_deref()) {
                debug!("SMTC session ignored | reason=not-current-session | key={key} | source={source}");
                continue;
            }
            // A previously-rejected session that became allowed (config edit)
            // should re-report as accepted on its next rejection, if any.
            // Only an allowed AND current session clears the marker: a
            // session that stays current-ineligible is not "accepted", so a
            // later rejection after another config change must re-report
            // instead of being swallowed by the stale marker.
            self.rejected_seen.remove(key);
            let is_new = !before.contains(key);
            // Admission caps: a hostile process can register unbounded
            // sessions/sources. Skip overflow entries before any metadata/art/icon
            // read so they never allocate artwork or subscribe handlers. Existing
            // subscriptions are already counted and so never re-blocked here.
            if is_new
                && admission_blocked(
                    admitted_sessions,
                    &admitted_sources,
                    source.as_str(),
                    MAX_TRACKED_SESSIONS,
                    MAX_TRACKED_SOURCES,
                )
            {
                // A brand-new *current* session must not be starved by a
                // saturated set of survivors: the session the user actually
                // switched to is exactly what the caps exist to protect.
                // Displace the weakest survivor(s) instead of rejecting it —
                // bounded, because the tallies updated in place never exceed
                // the caps and the current session is already admitted now.
                if Some(*key) == current_key {
                    let displaced = self.displace_survivors(
                        &prioritized,
                        &alive,
                        *key,
                        source.as_str(),
                        &mut admitted_sessions,
                        &mut admitted_sources,
                    );
                    if displaced == 0 {
                        rejected_overflow += 1;
                        debug!(
                            "SMTC current session not admitted | reason=admission-cap-not-relievable | key={key} | source={source} | sessions={} | sources={}",
                            admitted_sessions,
                            admitted_sources.len()
                        );
                        continue;
                    }
                    debug!(
                        "SMTC current session admitted via survivor displacement | displaced={displaced} | key={key} | source={source}"
                    );
                } else {
                    rejected_overflow += 1;
                    debug!(
                        "SMTC session not admitted | reason=admission-cap | key={key} | source={source} | sessions={} | sources={}",
                        admitted_sessions,
                        admitted_sources.len()
                    );
                    continue;
                }
            }
            let subscribed = match self.ensure_subscribed(session) {
                Ok(subscribed) => subscribed,
                Err(error) => {
                    debug!("subscribe failed for a session: {error:#}");
                    false
                }
            };
            if subscribed && is_new {
                // Immediately read properties for newly discovered sessions.
                // A source may fire MediaPropertiesChanged before we finish
                // registering the event handler (the SessionsChanged event that
                // revealed the session arrives first). Polling TryGetMediaProperties
                // now catches data that would otherwise be lost until the next event
                // burst — Windows's own SMTC widget does the same.
                if let Err(error) = self.refresh_session(session, true) {
                    debug!("initial refresh failed for session {key}: {error:#}");
                }
                admitted_sources.insert(source.clone());
                admitted_sessions += 1;
            }
        }
        if rejected_overflow > 0 && self.may_warn_overflow() {
            warn!(
                "SMTC admission cap reached; rejected {rejected_overflow} overflow session(s) \
                 (max_tracked_sessions={MAX_TRACKED_SESSIONS}, max_tracked_sources={MAX_TRACKED_SOURCES})"
            );
            self.last_overflow_warn = Some(Instant::now());
        }
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
        // A source whose last subscribed session vanished from the snapshot —
        // or reported Closed before the snapshot caught up (see
        // refresh_session) — owes the overlay a terminal Stopped: without one,
        // a persistent pill keeps the last track at idle opacity forever. The
        // vanished sources are collected BEFORE the eviction below, which
        // removes the subscriptions this pass reads; the settlement then uses
        // the full snapshot to decide the source is really gone. Emitted at
        // most once per disappearance. A source that simply recreates its
        // session (YouTube Music does this on every track change) is absent
        // from one snapshot and then returns; the grace window below keeps its
        // entry (and its caches, via the retention predicates further down)
        // alive so no spurious STOP fires in that gap.
        let alive_sources: HashSet<String> = sessions.iter().map(read_source_app).collect();
        for key in &stale {
            if let Some(subscription) = self.subscriptions.get(key) {
                let source = read_source_app(&subscription.session);
                if !alive_sources.contains(&source) {
                    self.terminal_pending.insert(source, Instant::now());
                }
            }
        }
        for key in &stale {
            debug!("SMTC session disappeared | key={key}");
            self.evict(*key);
        }
        let mut settled: Vec<String> = Vec::new();
        self.terminal_pending.retain(|source, absent_since| {
            let alive = alive_sources.contains(source);
            // A source still open restarts its grace from this scan, so a
            // later absence is measured from the last time it was actually
            // seen, not from an earlier Closed report.
            if alive {
                *absent_since = Instant::now();
            }
            let keep = terminal_pending_keep(alive, absent_since.elapsed(), TERMINAL_STOP_GRACE);
            if !keep {
                settled.push(source.clone());
            }
            keep
        });
        for source in settled {
            // Skip sources whose only report was Stopped (already announced)
            // and sources on the churn cool-down (their exit lines are
            // deliberately silent; the cool-down already excluded them).
            if terminal_stopped_warranted(
                self.last_known_playback_per_source.get(&source).copied(),
                self.source_on_cooldown(&source),
            ) {
                info!("playback state changed | state=Stopped | source={source}");
                self.emit(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, source));
            }
        }
        // Forget rejected sessions that vanished so a later reappearance is
        // reported again.
        self.rejected_seen.retain(|key| alive.contains(key));
        self.churn_cooldown.retain(|_, until| *until > Instant::now());
        // Keep the picker's candidate list in sync with what is actually
        // open, including apps whose sessions were rejected: checking them
        // is how the user adds them to the allow-list. Dedup and cap it
        // separately so a hostile session storm cannot grow the picker list
        // without bound.
        let active_sources: Vec<String> =
            dedup_capped(sessions.iter().map(read_source_app).collect(), MAX_TRACKED_SOURCES);
        let active: HashSet<String> = active_sources.iter().cloned().collect();
        set_active_session_sources(active_sources);
        // Evict source-level caches for apps that no longer have an open
        // session: their cached track (with artwork bytes) and icon would
        // otherwise persist forever, growing with every AUMID variant seen.
        // A source still inside the terminal-Stop grace keeps its caches:
        // the settle below reads the last playback state to warrant the
        // Stopped, and the recreation-suppression compares the cached track
        // when the recreated session reports the same song again.
        self.last_track_per_source
            .retain(|source, _| active.contains(source) || self.terminal_pending.contains_key(source));
        self.last_known_playback_per_source
            .retain(|source, _| active.contains(source) || self.terminal_pending.contains_key(source));
        self.icon_cache
            .retain(|source, _| active.contains(source) || self.terminal_pending.contains_key(source));
        self.last_emit_at
            .retain(|source, _| active.contains(source) || self.terminal_pending.contains_key(source));
        // Churn counts for departed sources are worthless and would otherwise
        // accumulate one deque per distinct source ever seen.
        self.churn.retain(|source, _| active.contains(source));
        // Palette identities of departed sources are dead weight: without
        // this prune the cache would accumulate one entry per distinct
        // (source, title, artist) ever seen for the listener's lifetime.
        // A source still inside the terminal-Stop grace keeps its palette,
        // matching the other source-level caches: the same track can
        // re-report during the settle and needs its identity-stable palette.
        self.palette_per_identity.retain(|key, _| {
            let source = palette_key_source(key);
            active.contains(source) || self.terminal_pending.contains_key(source)
        });
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

    /// The identity-stable palette for a track (see the free helper of the
    /// same name). Method form keeps every palette-cache access inside
    /// `ListenerState`, co-located with the prune in `sync_subscriptions`
    /// and the cap in `palette_for_identity`.
    fn palette_for_identity(&mut self, merged: &TrackInfo, decoded_art: Option<&[u8]>) -> Option<Palette> {
        palette_for_identity(&mut self.palette_per_identity, merged, decoded_art)
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
        // Re-read the authoritative playback state alongside the artwork so
        // the retry's TrackChanged carries the same snapshot the event path
        // would (see `snapshot_playback_state`) — without it, the surfaced
        // track reports `playback_state: None` and the pill infers playing
        // even when the source is paused.
        let (playback, rate) = match session.GetPlaybackInfo() {
            Ok(playback_info) => {
                let status = playback_info.PlaybackStatus()?;
                // A session that reported Closed has nothing to surface; the
                // normal refresh path settles its terminal state.
                if status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Closed {
                    return Ok(());
                }
                (
                    snapshot_playback_state(status),
                    playback_info.PlaybackRate().ok().and_then(|r| r.Value().ok()),
                )
            }
            Err(error) => {
                // Count a failed prefetch against the retry budget too: a
                // session whose reads keep failing must not be retried forever.
                if let Some(state) = self.states.get_mut(&key) {
                    state.artwork_attempts += 1;
                }
                return Err(error.into());
            }
        };
        let read = match read_track_info(session, true, playback, rate, session_playback_type(session)) {
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
        let emit = track_changed && !recreation_suppressed && !is_placeholder_like(&merged);
        if let Some(state) = self.states.get_mut(&key) {
            state.has_artwork = merged.artwork.is_some();
            state.artwork_attempts += 1;
        }
        if emit {
            let label = track_label(&merged);
            info!("track changed | {label}");
            store_last_track(
                &mut self.last_track_per_source,
                merged.source_app.clone(),
                merged.clone(),
                MAX_RETAINED_ARTWORK_BYTES,
            );
            let mut emitted = with_decoded_art(merged.clone(), crate::events::ARTWORK_DECODE as usize);
            emitted.palette = self.palette_for_identity(&merged, emitted.decoded_art.as_deref());
            self.emit(MediaEvent::TrackChanged(emitted));
            if let Some(state) = self.states.get_mut(&key) {
                state.deferred_at = None;
                state.deferred_for_stale_art = false;
            }
        } else if is_placeholder_like(&merged) {
            debug!("track emit skipped | reason=placeholder | source={}", merged.source_app);
        } else if recreation_suppressed {
            let label = track_label(&merged);
            debug!("track emit suppressed | reason=session-recreation | {label}");
        }
        Ok(())
    }

    /// Re-reads every subscribed session (metadata only, no artwork) and diffs
    /// it against the stored state. The 2-second safety net; also used for the
    /// startup read so the pill reports what is already playing. A session
    /// whose last successful read is newer than the poll interval (an
    /// event-driven refresh just re-read it) is skipped — re-reading it again
    /// would be pure WinRT churn, and a session that goes quiet ages past the
    /// window and gets polled as before, so the safety net is unchanged. For
    /// sessions still missing artwork, the poll also re-reads the thumbnail
    /// (up to the retry budget) so a slow-to-populate stream still surfaces a
    /// cover.
    fn poll_sessions(&mut self) {
        let keys: Vec<usize> = self.subscriptions.keys().copied().collect();
        for key in keys {
            // Clone the COM interface out so the map borrow ends before the
            // refresh (which can mutate subscriptions via eviction).
            let session = self.subscriptions.get(&key).map(|s| s.session.clone());
            if let Some(session) = session {
                let fresh = self.states.get(&key).is_some_and(|state| {
                    state
                        .last_read_at
                        .is_some_and(|at| at.elapsed() < SESSION_CHECK_INTERVAL)
                });
                if !fresh {
                    let _ = self.refresh_session(&session, false);
                }
                // Independent of the skip: the retry only touches the
                // thumbnail stream, never the metadata the freshness check
                // guards.
                if should_poll_artwork(self.states.get(&key))
                    && let Err(error) = self.retry_artwork(&session)
                {
                    debug!("artwork retry failed for session {key}: {error:#}");
                }
            }
        }
    }

    /// The source the overlay's pill is currently displaying, if any. Read
    /// for every session-recreation candidate so the gate compares against
    /// what the user actually sees, not against what this worker emitted.
    fn shown_source(&self) -> Option<String> {
        self.now_showing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Whether a session's source app is excluded: on the churn cool-down, or
    /// not matching the user's `media_sources` config. When `media_sources`
    /// is empty, all sources are allowed. When non-empty, only sources matching
    /// an entry (case-insensitive substring against the AUMID and its derived
    /// label, after normalizing word-boundary characters) are allowed;
    /// everything else is excluded.
    fn session_source_allowed(&mut self, session: &GlobalSystemMediaTransportControlsSession) -> bool {
        let aumid = session
            .SourceAppUserModelId()
            .map(|value| value.to_string())
            .unwrap_or_default();
        let label = source_app_label(&aumid);
        if self.source_on_cooldown(&label) {
            return false;
        }
        // The media_sources list only changes through the settings UI, so
        // compare the live config against the cached copy (element-wise, no
        // clone) and only re-normalize when it actually differs.
        {
            let cfg = self.config.read().unwrap_or_else(|poisoned| poisoned.into_inner());
            let raw = &cfg.behavior.media_sources;
            let stale = self
                .cached_allowed
                .as_ref()
                .is_none_or(|(cached_raw, _)| cached_raw != raw);
            if stale {
                self.cached_allowed = Some((
                    raw.clone(),
                    raw.iter().map(|pattern| normalize_for_match(pattern)).collect(),
                ));
            }
        }
        let Some((_, normalized)) = &self.cached_allowed else {
            return true;
        };
        if normalized.is_empty() {
            return true;
        }
        let naumid = normalize_for_match(&aumid);
        let nlabel = normalize_for_match(&label);
        normalized.iter().any(|np| naumid.contains(np) || nlabel.contains(np))
    }

    /// True while a source app is on the churn cool-down.
    fn source_on_cooldown(&self, source: &str) -> bool {
        self.churn_cooldown
            .get(source)
            .is_some_and(|until| *until > Instant::now())
    }

    /// Counts a newly-created content-free session for its source; trips the
    /// cool-down once the threshold is exceeded within the window, logging a
    /// WARN so the log explains the exclusion without manual analysis. Called
    /// from `refresh_session` on a session's first read — including a failed
    /// first read, which by definition carries no track — so only sessions
    /// that carry no track (title fell back to the source-app label) count:
    /// a source recreating its session per real track change never reaches
    /// the threshold.
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

    /// Whether enough time has elapsed since the last overflow warning to emit
    /// another, so a hostile session storm logs one WARN per
    /// `OVERFLOW_WARN_INTERVAL` rather than one per rejected session.
    fn may_warn_overflow(&self) -> bool {
        overflow_warn_allowed(self.last_overflow_warn, OVERFLOW_WARN_INTERVAL)
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
            if let Err(e) = properties_tx.try_send(Signal::MediaProperties(properties_session.clone())) {
                debug!("signal dropped | kind=MediaProperties | {e:?}");
            }
            Ok(())
        });
        let playback_handler: TypedEventHandler<
            GlobalSystemMediaTransportControlsSession,
            PlaybackInfoChangedEventArgs,
        > = TypedEventHandler::new(move |_, _| {
            if let Err(e) = playback_tx.try_send(Signal::PlaybackInfo(playback_session.clone())) {
                debug!("signal dropped | kind=PlaybackInfo | {e:?}");
            }
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
        // Registered last: if it fails, both earlier handlers are rolled back
        // so no dangling registration outlives the failed subscribe.
        let timeline_session = session.clone();
        let timeline_tx = self.signal_tx.clone();
        let timeline_handler: TypedEventHandler<
            GlobalSystemMediaTransportControlsSession,
            TimelinePropertiesChangedEventArgs,
        > = TypedEventHandler::new(move |_, _| {
            if let Err(e) = timeline_tx.try_send(Signal::Timeline(timeline_session.clone())) {
                debug!("signal dropped | kind=Timeline | {e:?}");
            }
            Ok(())
        });
        let timeline_token = match session.TimelinePropertiesChanged(&timeline_handler) {
            Ok(token) => token,
            Err(error) => {
                let _ = session.RemoveMediaPropertiesChanged(properties_token);
                let _ = session.RemovePlaybackInfoChanged(playback_token);
                return Err(error.into());
            }
        };
        Ok(SessionSubscription {
            session: session.clone(),
            properties_token,
            playback_token,
            timeline_token,
        })
    }

    /// Returns `Ok(true)` when the session is (or already was) subscribed and
    /// `Ok(false)` when a cap rejected it, so callers can tell a deliberate
    /// cap rejection apart from a successful subscribe: the sync loop must
    /// not count a cap-rejected session as admitted.
    fn ensure_subscribed(&mut self, session: &GlobalSystemMediaTransportControlsSession) -> Result<bool> {
        let key = session_key(session);
        if self.subscriptions.contains_key(&key) {
            return Ok(true);
        }
        // The sync loop enforces the global admission caps, but event-driven
        // subscriptions race ahead of the next sync: cap the session count here
        // too, so a burst of current-session events for one source cannot exceed
        // MAX_TRACKED_SESSIONS between syncs. The distinct-source cap is applied
        // the same way, so a storm of events from many different sources cannot
        // subscribe more than MAX_TRACKED_SOURCES before the next sync.
        if self.subscriptions.len() >= MAX_TRACKED_SESSIONS {
            debug!("SMTC session not subscribed | reason=session-cap | key={key}");
            return Ok(false);
        }
        let source = read_source_app(session);
        let distinct_sources: HashSet<String> = self
            .subscriptions
            .values()
            .map(|s| read_source_app(&s.session))
            .collect();
        if !distinct_sources.contains(&source) && distinct_sources.len() >= MAX_TRACKED_SOURCES {
            debug!("SMTC session not subscribed | reason=source-cap | key={key} | source={source}");
            return Ok(false);
        }
        let subscription = self.subscribe(session)?;
        debug!("subscribed to SMTC session {key} | source={source}");
        self.subscriptions.insert(key, subscription);
        Ok(true)
    }

    /// Makes room for a brand-new current session when the admission caps are
    /// saturated, by displacing survivors working from the weakest: dead
    /// subscriptions (absent from the live snapshot — the stale prune would
    /// evict them anyway, so displacing costs nothing) first, then the
    /// existing subscription furthest from the current session in admission
    /// order. Stops as soon as the caps would admit `incoming` (and the live
    /// subscription count is under the session cap again). Returns the number
    /// displaced; 0 means the caps would still reject `incoming` and nothing
    /// was evicted.
    ///
    /// The admitted tallies are updated in place so the caller keeps counting
    /// exactly what is now really subscribed. Displacement is itself bounded
    /// by the caps — the admitted tally never exceeds them, so a hostile
    /// storm cannot use it to grow the tracked set.
    fn displace_survivors(
        &mut self,
        prioritized: &[(GlobalSystemMediaTransportControlsSession, usize, String)],
        alive: &HashSet<usize>,
        incoming: usize,
        incoming_source: &str,
        admitted_sessions: &mut usize,
        admitted_sources: &mut HashSet<String>,
    ) -> usize {
        // The distinct-source cap can only be relieved by displacing a
        // survivor whose source is exclusively subscribed; if no such
        // survivor exists, displacement cannot help and evicting a live
        // session for nothing is pure harm. Refuse up front. (The caller
        // processes the incoming current session first, so nothing has been
        // evicted yet and `subscriptions` still equals the pre-sync set.)
        let source_cap_unrelievable = !admitted_sources.contains(incoming_source)
            && admitted_sources.len() >= MAX_TRACKED_SOURCES
            && !prioritized.iter().any(|(_, key, source)| {
                *key != incoming
                    && self.subscriptions.contains_key(key)
                    && !self
                        .subscriptions
                        .iter()
                        .any(|(k, s)| k != key && read_source_app(&s.session) == *source)
            });
        if source_cap_unrelievable {
            return 0;
        }
        // Weakest-first key order, built once (it never changes during
        // displacement; `subscribed` below does the shrinking).
        let weakest_first: Vec<usize> = prioritized.iter().rev().map(|(_, key, _)| *key).collect();
        let mut displaced = 0;
        loop {
            if !admission_blocked(
                *admitted_sessions,
                admitted_sources,
                incoming_source,
                MAX_TRACKED_SESSIONS,
                MAX_TRACKED_SOURCES,
            ) && self.subscriptions.len() < MAX_TRACKED_SESSIONS
            {
                break;
            }
            let subscribed_keys: HashSet<usize> = self.subscriptions.keys().copied().collect();
            let Some(victim) = displacement_victim(&weakest_first, alive, &subscribed_keys, incoming) else {
                break;
            };
            let v_source = read_source_app(&self.subscriptions[&victim].session);
            let v_exclusive = !self
                .subscriptions
                .iter()
                .any(|(key, s)| *key != victim && read_source_app(&s.session) == v_source);
            let v_alive = alive.contains(&victim);
            self.evict(victim);
            displaced += 1;
            if v_alive {
                *admitted_sessions = admitted_sessions.saturating_sub(1);
                if v_exclusive {
                    admitted_sources.remove(&v_source);
                }
            }
        }
        displaced
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
            let _ = subscription
                .session
                .RemoveTimelinePropertiesChanged(subscription.timeline_token);
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
    /// after its successor took over. The event travels as one shared `Arc`
    /// allocation that the forwarder clones into both window queues, so the
    /// fan-out never copies it. The channel is bounded and never blocks the
    /// worker: when the forwarder cannot keep up, the event waits in the
    /// bounded retry mailbox (`pending_output`) and is re-sent at the next
    /// event-loop turn instead of being permanently dropped.
    fn emit(&mut self, event: MediaEvent) {
        if !self.is_current_generation() {
            return;
        }
        match self.output.try_send(Arc::new(event)) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(returned)) => {
                let dropped = coalesce_pending_event(&mut self.pending_output, returned, OUTPUT_RETRY_CAP);
                if dropped > 0 && self.may_warn_overflow() {
                    warn!(
                        "SMTC output retry mailbox overflowed: {dropped} queued event(s) dropped \
                         (UI is not keeping up)"
                    );
                    self.last_overflow_warn = Some(Instant::now());
                }
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.pending_output.clear();
                debug!("signal dropped | kind=MediaEvent | reason=closed");
            }
        }
    }

    /// Re-sends every queued event, oldest first, after the event loop's
    /// regular work for this turn. Stops at the first full send so arrival
    /// order (across sources and kinds) is preserved; a disconnected channel
    /// drops the mailbox — the forwarder is gone, nothing can be delivered.
    fn flush_output(&mut self) {
        if drain_pending_to_channel(&mut self.pending_output, &self.output) {
            self.pending_output.clear();
        }
    }

    fn is_current_generation(&self) -> bool {
        self.live_generation.load(Ordering::SeqCst) == self.my_generation
    }
}

/// Coalesce key for the retry mailbox: two events that would render/replace
/// the same downstream state (same kind, same source) may be superseded by
/// the newest of the pair. `WorkerFailed` has no source and is only ever
/// emitted once by the supervisor, so it never collides.
fn event_coalesce_key(event: &MediaEvent) -> (&'static str, Option<&str>) {
    match event {
        MediaEvent::TrackChanged(track) => ("track", Some(track.source_app.as_str())),
        MediaEvent::PlaybackStateChanged(_, source) => ("playback", Some(source.as_str())),
        MediaEvent::SessionRejected { source_app, .. } => ("rejected", Some(source_app.as_str())),
        MediaEvent::ProgressChanged { source_app, .. } => ("progress", Some(source_app.as_str())),
        MediaEvent::WorkerFailed { .. } => ("worker-failed", None),
    }
}

/// Inserts an event into the bounded retry mailbox. An older event with the
/// same coalesce key is superseded in place — the newest authoritative state
/// wins — while events for different sources/kinds keep their arrival order.
/// On over-cap the oldest queued event is dropped, never the newest; returns
/// how many were dropped so the caller can report the overflow.
fn coalesce_pending_event(queue: &mut VecDeque<Arc<MediaEvent>>, event: Arc<MediaEvent>, cap: usize) -> usize {
    let key = event_coalesce_key(&event);
    if let Some(index) = queue.iter().position(|queued| event_coalesce_key(queued) == key) {
        queue.remove(index);
    }
    queue.push_back(event);
    let mut dropped = 0;
    while queue.len() > cap {
        queue.pop_front();
        dropped += 1;
    }
    dropped
}

/// Drains a retry mailbox into the output channel, oldest first. Stops at the
/// first full send so ordering is preserved; returns true when the channel
/// disconnected so the caller can clear the mailbox.
fn drain_pending_to_channel(queue: &mut VecDeque<Arc<MediaEvent>>, output: &SyncSender<Arc<MediaEvent>>) -> bool {
    while let Some(event) = queue.front().cloned() {
        match output.try_send(event) {
            Ok(()) => {
                queue.pop_front();
            }
            Err(mpsc::TrySendError::Full(_)) => return false,
            Err(mpsc::TrySendError::Disconnected(_)) => return true,
        }
    }
    false
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

/// Whether `merged`'s artwork is likely the PREVIOUS track's thumbnail served
/// stale during a transition: the track identity (title + artist) differs from
/// the last emitted track, yet the freshly read bytes are identical to that
/// track's artwork. SMTC updates the thumbnail stream after the text fields,
/// so a read inside that window pairs the new identity with the old cover.
/// Attaching it would show the wrong cover (e.g. the previous song's album
/// art) and poison the identity-keyed artwork and palette caches, so callers
/// drop the artwork and let the artwork-changed re-emit surface the real
/// cover once the stream catches up. Same-identity reads always keep their
/// art (a legitimately identical album cover across a playlist is correct).
fn stale_thumbnail(merged: &TrackInfo, last_emitted: Option<&TrackInfo>) -> bool {
    let Some(last) = last_emitted else {
        return false;
    };
    if last.title == merged.title && last.artist == merged.artist {
        return false;
    }
    match (&merged.artwork, &last.artwork) {
        (Some(new_b), Some(old_b)) => Arc::ptr_eq(new_b, old_b) || new_b.as_ref() == old_b.as_ref(),
        _ => false,
    }
}

/// Whether admitting one more session of `source` would breach the global SMTC
/// caps: either the session cap is already at its ceiling, or `source` is a new
/// identity and the distinct-source cap is already at its ceiling. A source
/// already counted in `admitted_sources` only consumes a session slot, never a
/// fresh source slot, so a churning source is bounded by the session cap rather
/// than the source cap. This is the single cap-decision function: the live sync
/// loop and the test-only `admit_sessions` model both call it, so they can never
/// disagree on an admission-cap rejection.
pub(crate) fn admission_blocked(
    session_count: usize,
    admitted_sources: &HashSet<String>,
    source: &str,
    session_cap: usize,
    source_cap: usize,
) -> bool {
    session_count >= session_cap || (!admitted_sources.contains(source) && admitted_sources.len() >= source_cap)
}

/// Weakest survivor to displace when a brand-new current session must be
/// admitted past a saturated cap. `weakest_first` must list the subscribed
/// keys in reverse admission priority (weakest first); among them this
/// prefers a dead survivor (absent from the live snapshot — displacement is
/// free, the stale prune would evict it anyway), then the survivor furthest
/// from the current session. `subscribed` is the set of keys still actually
/// subscribed, which shrinks as displacement evicts. `incoming` is never its
/// own victim.
fn displacement_victim(
    weakest_first: &[usize],
    alive: &HashSet<usize>,
    subscribed: &HashSet<usize>,
    incoming: usize,
) -> Option<usize> {
    weakest_first
        .iter()
        .copied()
        .find(|key| subscribed.contains(key) && *key != incoming && !alive.contains(key))
        .or_else(|| {
            weakest_first
                .iter()
                .copied()
                .find(|key| subscribed.contains(key) && *key != incoming)
        })
}

/// Priority-ordered, cap-enforced admission of an already-filtered (allowed +
/// current-source) session list. `ordered` must be in admission priority:
/// current session first, then surviving existing subscriptions, then new
/// sessions. `existing_keys`/`existing_sources` are the keys/sources the worker
/// already holds before this sync, and are pre-seeded so the caps bound the
/// total, not only new additions. Returns the full admitted key set (existing +
/// newly admitted) and the count of sessions the caps rejected.
///
/// Test-only model of the sync loop's admission pass: the live loop applies
/// `admission_blocked` inline (its decisions interleave with subscription,
/// displacement and eviction side effects), so this function exists purely to
/// make the current-first / existing-before-new priority contract directly
/// testable. The model assumes every admitted session subscribes successfully;
/// in the live loop a session can still fail to subscribe (cap race with the
/// event-driven path, or a WinRT error) without the caps reconsidering it here,
/// and a brand-new *current* session displaces survivors instead of being
/// rejected (see `displace_survivors`) — neither is modeled.
#[cfg(test)]
fn admit_sessions(
    ordered: &[(usize, String)],
    existing_keys: &HashSet<usize>,
    existing_sources: &HashSet<String>,
    session_cap: usize,
    source_cap: usize,
) -> (HashSet<usize>, usize) {
    let mut admitted_keys: HashSet<usize> = existing_keys.clone();
    let mut admitted_sources: HashSet<String> = existing_sources.clone();
    let mut rejected = 0;
    for (key, source) in ordered {
        if admitted_keys.contains(key) {
            continue;
        }
        if admission_blocked(admitted_keys.len(), &admitted_sources, source, session_cap, source_cap) {
            rejected += 1;
            continue;
        }
        admitted_keys.insert(*key);
        admitted_sources.insert(source.clone());
    }
    (admitted_keys, rejected)
}

/// Total raw-artwork bytes retained across a source-keyed last-emitted track
/// map. Used to enforce `MAX_RETAINED_ARTWORK_BYTES` on insertion.
fn retained_art_bytes(last_track: &HashMap<String, TrackInfo>) -> usize {
    last_track
        .values()
        .map(|t| t.artwork.as_ref().map_or(0, |a| a.len()))
        .sum()
}

/// Inserts (or replaces) a source's last-emitted track, enforcing the retained
/// artwork budget: if the new artwork would push the total past
/// `MAX_RETAINED_ARTWORK_BYTES`, the artwork bytes are dropped (metadata
/// retained) so the pill renders a placeholder instead of holding stale cover
/// bytes. Returns whether the artwork was kept (false => placeholder retained).
fn store_last_track(
    last_track: &mut HashMap<String, TrackInfo>,
    source: String,
    mut track: TrackInfo,
    budget: usize,
) -> bool {
    let existing_art = last_track
        .get(&source)
        .and_then(|t| t.artwork.as_ref())
        .map_or(0, |a| a.len());
    let new_art = track.artwork.as_ref().map_or(0, |a| a.len());
    let projected = retained_art_bytes(last_track) - existing_art + new_art;
    let over_budget = projected > budget && new_art > 0;
    if over_budget {
        track.artwork = None;
    }
    last_track.insert(source, track);
    !over_budget
}

/// Deduplicates a source list (preserving first-occurrence order) and caps it to
/// `cap` entries, so the picker's candidate list cannot grow with a hostile
/// session storm.
fn dedup_capped(mut sources: Vec<String>, cap: usize) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::with_capacity(sources.len());
    let mut out: Vec<String> = Vec::with_capacity(sources.len().min(cap));
    for s in sources.drain(..) {
        if seen.insert(s.clone()) && out.len() < cap {
            out.push(s);
        }
    }
    out
}

/// Whether an overflow warning may fire now: `last` is the instant of the last
/// warning (or `None` if never), and `window` is the minimum gap between two.
/// Pure so the warn-rate contract is directly testable without a listener.
fn overflow_warn_allowed(last: Option<Instant>, window: Duration) -> bool {
    last.is_none_or(|t| t.elapsed() >= window)
}

/// The identity-stable palette to attach to an emitted track: reuses the
/// per-identity (source + title + artist) cache when present, so a source
/// that re-encodes its thumbnail between reads (different bytes, same cover)
/// can never shift the pill's accent colors. Otherwise derives the palette
/// from the freshly decoded buffer (itself deterministic per bytes: the
/// worker decodes at a fixed size) and caches it. Returns `None` when the
/// identity has no trusted artwork yet — the UI falls back to computing from
/// `decoded_art`.
/// Upper bound for `palette_per_identity`. `sync_subscriptions` prunes the
/// entries of departed sources, but a single long-lived source (a 24/7
/// jukebox) can still accumulate thousands of distinct identities; entries
/// are recomputable from the decoded artwork, so past the bound an arbitrary
/// entry is dropped.
const PALETTE_CACHE_CAP: usize = 256;

/// Composite palette-cache key: source, title, artist, NUL-joined. A single
/// allocation per lookup instead of the three strings the tuple form needed
/// (the key is built even on cache hits). Fields read back via
/// `palette_key_source`.
fn palette_cache_key(source: &str, title: &str, artist: &str) -> String {
    let mut key = String::with_capacity(source.len() + title.len() + artist.len() + 2);
    key.push_str(source);
    key.push('\0');
    key.push_str(title);
    key.push('\0');
    key.push_str(artist);
    key
}

/// The source field of a `palette_cache_key` (everything up to the first NUL).
fn palette_key_source(key: &str) -> &str {
    key.split('\0').next().unwrap_or_default()
}

fn palette_for_identity(
    cache: &mut HashMap<String, Palette>,
    merged: &TrackInfo,
    decoded_art: Option<&[u8]>,
) -> Option<Palette> {
    let key = palette_cache_key(&merged.source_app, &merged.title, &merged.artist);
    if let Some(palette) = cache.get(&key) {
        return Some(*palette);
    }
    let palette = decoded_art
        .and_then(crate::overlay::pm_bgra_to_rgba)
        .and_then(|rgba| palette_from_rgba(&rgba));
    if let Some(palette) = palette {
        if cache.len() >= PALETTE_CACHE_CAP {
            // Dropping an arbitrary entry is fine: palettes are recomputable
            // from `decoded_art` on the next miss.
            if let Some(stale) = cache.keys().next().cloned() {
                cache.remove(&stale);
            }
        }
        cache.insert(key, palette);
    }
    palette
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

/// Whether a same-track re-report should be suppressed as recreation noise.
/// `is_session_recreation` is time-blind: it compares only against the last
/// track emitted per source, so a re-report arriving long after that emit
/// (another app's pill in between — a switch-back) would be wrongly
/// suppressed. Suppress only while the pill already on screen belongs to the
/// re-reporting source (`shown_source` — the overlay's published now-showing
/// cell); after another app's pill, the re-report is a real re-emit — the
/// overlay's cache for the source may already be evicted, and the pill needs
/// the fresh track (cached art injected above) to come back itself.
fn should_suppress_recreation(
    last_track: Option<&TrackInfo>,
    merged: &TrackInfo,
    read_artwork: bool,
    shown_source: Option<&str>,
) -> bool {
    last_track.is_some_and(|prev_track| {
        is_session_recreation(prev_track, merged, read_artwork) && shown_source == Some(merged.source_app.as_str())
    })
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

/// Whether the poll-time "artwork-timeout" force may fire for a deferred
/// read. A stale-art deferral is resolved by the pending artwork retry in
/// the same poll pass; forcing an artless emit there flashes a pill the
/// retry immediately re-emits with art, so it is suppressed while the retry
/// budget is unconsumed. Every other deferral (first-read awaiting-artwork)
/// keeps the timeout guarantee: the pill shows anyway after `ARTWORK_TIMEOUT`.
fn poll_force_allowed(prev: &LogicalState) -> bool {
    !(prev.deferred_for_stale_art && prev.artwork_attempts < ARTWORK_RETRY_BUDGET)
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

/// Whether an artwork-changed TrackChanged for a same-title+artist track must
/// be absorbed instead of emitted: a real playback state event for the same
/// source is already in the batch. The refresh is not new content — emitting
/// it would make the batch rule drop the user's pause/play and the pill would
/// show the track layout instead of the state (observed with YouTube Music,
/// which re-encodes its thumbnail when the session is recreated on pause:
/// same cover, different bytes). The caller records the refresh as the
/// source's last emitted track so a later read dedups against the new bytes.
/// Absent artwork is never absorbed: the artwork-timeout first pill and the
/// artwork-lost path must keep their existing behavior.
fn artwork_refresh_absorbed(events: &[MediaEvent], merged: &TrackInfo) -> bool {
    merged.artwork.is_some()
        && events
            .iter()
            .any(|e| matches!(e, MediaEvent::PlaybackStateChanged(_, source) if source == &merged.source_app))
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
        // Position is re-read on every pass (read_track_info always reads the
        // timeline). Inherit the last whole-second value only when this pass
        // returned nothing and the identity is unchanged, so a transient
        // empty read can't blank an in-flight bar.
        position_secs: if same_identity {
            read.position_secs.or_else(|| prev.last_position_secs.map(|s| s as f64))
        } else {
            read.position_secs
        },
        playback_rate: read.playback_rate,
        // The type is inherited across poll reads (which never re-report it):
        // an Unknown read on the same identity keeps the last known type, so
        // a video session is not demoted to the music glyph by the poll.
        playback_type: if same_identity && read.playback_type == PlaybackType::Unknown {
            prev.playback_type
        } else {
            read.playback_type
        },
        playback_state: read.playback_state,
        position_updated_at: read.position_updated_at,
        // The identity-stable palette is attached at emit time (after the
        // decode), never merged here: the merge inherits the previous track's
        // identity fields, and a stale palette must not carry over.
        palette: None,
    }
}

/// Maps SMTC's `MediaPlaybackType` onto the overlay's. `Unknown` and any
/// variant the installed windows crate does not generate map to
/// `PlaybackType::Unknown`.
fn map_playback_type(ty: MediaPlaybackType) -> PlaybackType {
    match ty {
        MediaPlaybackType::Music => PlaybackType::Music,
        MediaPlaybackType::Video => PlaybackType::Video,
        MediaPlaybackType::Image => PlaybackType::Image,
        _ => PlaybackType::Unknown,
    }
}

/// The session's reported content type; `Unknown` when the session is gone
/// or the OS does not report one.
fn session_playback_type(session: &GlobalSystemMediaTransportControlsSession) -> PlaybackType {
    session
        .GetPlaybackInfo()
        .ok()
        .and_then(|info| info.PlaybackType().ok())
        .and_then(|ty| ty.Value().ok())
        .map(map_playback_type)
        .unwrap_or(PlaybackType::Unknown)
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
    // Seek detection: a position jump beyond the threshold (or a presence
    // flip in reported position) re-emits so the overlay re-bases instead of
    // drifting from a stale base. Position is excluded from content_differ.
    // The threshold scales with the playback rate: a normal advance between
    // two reads is roughly `rate` times the wall-clock gap, so a fixed delta
    // would flag a false seek during 2x+ playback (~2 s cadence at 2x moves
    // ~4 s). Clamp the rate to a floor so a slow/stopped source keeps the
    // base threshold and a genuine seek is still adopted.
    // A fresh session's first read always shows a position presence flip, so
    // the seek term must not override the artwork deferral: sources that
    // recreate their session per track change would otherwise emit a
    // title-only pill while SMTC populates the thumbnail (~500 ms later), and
    // the cover would then swap in under it. Established sessions have
    // `defer_first == false`, so their seek re-emits are unaffected.
    let rate = merged.playback_rate.unwrap_or(1.0).max(1.0);
    let seek = match (merged.position_secs, prev.last_position_secs) {
        (Some(rp), Some(pp)) => (rp - pp as f64).abs() > SEEK_DELTA_SECS * rate,
        (Some(_), None) | (None, Some(_)) => true,
        _ => false,
    };
    (
        (content_changed || seek) && !defer_first || artwork_gained,
        artwork_lost,
    )
}

/// Whether a deferred first pill has waited past the artwork timeout and
/// should be emitted anyway, artwork or not.
fn defer_expired(deferred_at: Option<Instant>) -> bool {
    deferred_at.is_some_and(|t| t.elapsed() >= ARTWORK_TIMEOUT)
}

/// Attaches the worker's fixed-size artwork decode to a track about to be
/// emitted. Called only on the emit paths (never on poll/merge reads), so the
/// image decode runs once per actually-emitted track — on the worker thread,
/// never on a window's UI thread. `ARTWORK_DECODE`² is fixed for every display
/// (see `events::ARTWORK_DECODE`), so the same cover always decodes to the
/// same buffer and the palette derived from it cannot shift between pill
/// shows; both windows derive the side from the buffer length.
fn with_decoded_art(mut track: TrackInfo, size: usize) -> TrackInfo {
    track.decoded_art = track
        .artwork
        .as_deref()
        .and_then(|bytes| decode_artwork_pm(bytes, size))
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

/// A metadata snapshot that carries no real content: the title is just the
/// source-app fallback (empty `properties.Title()`) and the artist is also empty.
/// Unlike `is_placeholder_read` this is independent of whether the session is
/// first-read — the worker can land this snapshot on any read during a transition
/// for a source that recreates its session — so any such read is suppressed and
/// never announced as a (fake) "sample track". A real `MediaPropertiesChanged` or
/// the periodic poll supersedes it once metadata lands.
fn is_placeholder_like(merged: &TrackInfo) -> bool {
    merged.artist.is_empty() && merged.title == merged.source_app
}

/// Whether a session's first read charges churn for its source. Only
/// content-free sessions count: a newly-created session whose title fell back
/// to the source-app label (empty `properties.Title()`) carries no track and
/// is identity garbage (the Riot Client signature). A legitimately recreated
/// session from a real track change always reports a title on its first read
/// and never counts, so rapid skipping cannot trip the cool-down.
fn first_read_counts_toward_churn(is_first_read: bool, merged: &TrackInfo) -> bool {
    is_first_read && is_placeholder_like(merged)
}

fn register_sessions_handler(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
    signal_tx: SyncSender<Signal>,
) -> Result<EventRegistrationToken> {
    let handler: TypedEventHandler<GlobalSystemMediaTransportControlsSessionManager, SessionsChangedEventArgs> =
        TypedEventHandler::new(move |_, _| {
            if let Err(e) = signal_tx.try_send(Signal::Sessions) {
                debug!("signal dropped | kind=Sessions | {e:?}");
            }
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
            if let Err(e) = signal_tx.try_send(Signal::Sessions) {
                debug!("signal dropped | kind=Sessions | {e:?}");
            }
            Ok(())
        });
    Ok(manager.CurrentSessionChanged(&handler)?)
}

/// Maps a session to the identity key used across `subscriptions`, `states`
/// and the dirty queues. The raw COM pointer is a sound key under three
/// invariants:
///
/// - Identity: COM guarantees that, for one interface, two pointers are
///   equal if and only if they refer to the same object (the identity rule),
///   so pointer equality here *is* object identity.
/// - Liveness: every key originates from a session object that is alive at
///   the moment it is taken (fetched from the manager, or delivered by a
///   handler), and `SessionSubscription` keeps a strong reference to it —
///   the address cannot be freed and recycled while its key is stored.
/// - Staleness: `sync_subscriptions` evicts every key whose object is no
///   longer in the manager's session list, so a recycled address cannot be
///   mistaken for a live session.
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
///
/// Every displayed field passes through here — including source labels, see
/// `source_app_label` — so the C0/C1 and directional-control stripping and
/// the character cap apply wherever metadata is stored or rendered.
fn cap_meta(value: String) -> String {
    let trimmed = value.trim();
    if trimmed.len() != value.len() {
        // The raw value must never be logged in full (it can be arbitrarily
        // long): a bounded escaped preview plus the omitted count keeps the
        // log line independent of the raw metadata length.
        let (preview, omitted) = metadata_preview(trimmed);
        let omitted_note = if omitted > 0 {
            format!(" (+{omitted} omitted)")
        } else {
            String::new()
        };
        debug!(
            "metadata normalized | trimmed {} -> {} chars | value={preview}{omitted_note}",
            value.chars().count(),
            trimmed.chars().count()
        );
    }
    let safe: String = trimmed.chars().filter(|c| !display_unsafe(*c)).collect();
    if safe.chars().count() > MAX_META_CHARS {
        safe.chars().take(MAX_META_CHARS).collect()
    } else {
        safe
    }
}

/// Whether a character must never reach displayed metadata: the C0 control
/// range, DEL plus the C1 range, and the Unicode directional
/// formatting/override/isolate command characters (bidi embeddings,
/// overrides, isolates). Ordinary RTL letters, combining marks, emoji and ZWJ
/// sequences are all preserved — only the directionality *commands* are
/// stripped, so a legitimate RTL title still orders right-to-left by its
/// letters.
fn display_unsafe(c: char) -> bool {
    let code = c as u32;
    (0x0000..=0x001F).contains(&code)
        || (0x007F..=0x009F).contains(&code)
        || (0x202A..=0x202E).contains(&code)
        || (0x2066..=0x2069).contains(&code)
}

const MAX_META_CHARS: usize = 256;

/// Bounded, escaped preview of a metadata value for a log line: at most
/// `MAX_PREVIEW_CHARS` scalar values, each escaped so control and invisible
/// characters are visible, plus the number of characters omitted. Keeps log
/// formatting allocations independent of the raw metadata length.
fn metadata_preview(value: &str) -> (String, usize) {
    let mut preview = String::new();
    for (i, c) in value.chars().enumerate() {
        if i >= MAX_PREVIEW_CHARS {
            return (preview, value.chars().count() - MAX_PREVIEW_CHARS);
        }
        preview.extend(c.escape_debug());
    }
    (preview, 0)
}

const MAX_PREVIEW_CHARS: usize = 128;

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

/// A stopped session is "not playing" no matter what status arrives, so the
/// TrackInfo snapshot always carries a playback state consistent with the
/// session info it was read alongside. A stopped session is likely torn down
/// (the app is dying or lost the SMTC connection), so its snapshot must not
/// claim the pill should be playing.
fn snapshot_playback_state(status: GlobalSystemMediaTransportControlsSessionPlaybackStatus) -> Option<PlaybackState> {
    use GlobalSystemMediaTransportControlsSessionPlaybackStatus as S;
    if status == S::Stopped {
        return Some(PlaybackState::Stopped);
    }
    // Opened/Changing and unknown statuses are transitional: ignored.
    match status {
        S::Playing => Some(PlaybackState::Playing),
        S::Paused => Some(PlaybackState::Paused),
        _ => None,
    }
}

fn read_track_info(
    session: &GlobalSystemMediaTransportControlsSession,
    read_artwork: bool,
    playback_state: Option<PlaybackState>,
    playback_rate: Option<f64>,
    playback_type: PlaybackType,
) -> Result<TrackInfo> {
    let source_app = read_source_app(session);
    let properties = session.TryGetMediaPropertiesAsync()?.get()?;
    let title = cap_meta(non_empty(properties.Title()?.to_string(), &source_app));
    // Keep artist empty when the app has not provided it yet; the pill and
    // the Activity pane show "Unknown Artist" as a placeholder so the row
    // is never blank.
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
    // One timeline read yields the raw duration + live position + read
    // instant; position is re-estimated on the UI thread between these reads.
    // The raw tick values are normalized through `normalize_timeline` (checked
    // subtraction, finite/clamp) here, at the worker boundary.
    let (start_100ns, end_100ns, position_100ns, position_updated_at) = read_timeline(session);
    let (duration_secs, position_secs, playback_rate) =
        normalize_timeline(start_100ns, end_100ns, position_100ns, playback_rate);
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
        position_secs,
        playback_rate,
        playback_type,
        playback_state,
        position_updated_at: Some(position_updated_at),
        track_number,
        track_count,
        genre,
        palette: None,
    })
}

/// Reads the session's timeline raw fields in one `GetTimelineProperties()`
/// call. Returns `(start_100ns, end_100ns, position_100ns, read_instant)` —
/// the raw 100 ns tick values Windows reports and the monotonic instant the
/// read happened at. Nothing here is normalized: checked subtraction,
/// finite checks and clamping are the pure `normalize_timeline`'s job, so the
/// hostile-value policy lives in one testable place.
fn read_timeline(
    session: &GlobalSystemMediaTransportControlsSession,
) -> (Option<i64>, Option<i64>, Option<i64>, Instant) {
    match session.GetTimelineProperties() {
        Ok(t) => (
            t.StartTime().ok().map(|ts| ts.Duration),
            t.EndTime().ok().map(|ts| ts.Duration),
            t.Position().ok().map(|ts| ts.Duration),
            Instant::now(),
        ),
        Err(_) => (None, None, None, Instant::now()),
    }
}

/// One pure normalization pass over the raw timeline a source reports. All
/// inputs are Windows `TimeSpan` tick counts (100 ns units) except `rate`,
/// which is the playback rate as reported. Returns `(duration_secs,
/// position_secs, rate)`:
///
/// - The duration uses `checked_sub`, so `end - start` cannot wrap or panic
///   on extreme values; a span of `<= 0` is no duration, and the whole-second
///   duration is only reported when it is at least 1 second (a sub-second
///   span has no whole-second duration).
/// - The position must be finite and stays inside `0..=duration` when a
///   duration is known, `0..` otherwise — a hostile negative or past-the-end
///   position can never reach the overlay.
/// - The rate must be finite and within `0.0..=16.0`; anything else is
///   dropped. An absurd or non-finite rate would otherwise poison the
///   overlay's `estimate_position` interpolation.
fn normalize_timeline(
    start_100ns: Option<i64>,
    end_100ns: Option<i64>,
    position_100ns: Option<i64>,
    rate: Option<f64>,
) -> (Option<u64>, Option<f64>, Option<f64>) {
    let duration_secs = match (start_100ns, end_100ns) {
        (Some(start), Some(end)) => end
            .checked_sub(start)
            .filter(|span| *span > 0)
            .map(|span| (span / 10_000_000) as u64)
            .filter(|secs| *secs >= 1),
        _ => None,
    };
    let position_secs = position_100ns
        .map(|ticks| ticks as f64 / 10_000_000.0)
        .filter(|pos| pos.is_finite())
        .map(|pos| {
            let bounded = if let Some(duration) = duration_secs {
                pos.min(duration as f64)
            } else {
                pos
            };
            bounded.max(0.0)
        });
    let rate = rate.filter(|r| r.is_finite() && (0.0..=16.0).contains(r));
    (duration_secs, position_secs, rate)
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
    if size == 0 || !(1024..=MAX_THUMBNAIL_BYTES).contains(&size) || size > u32::MAX as u64 {
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
    // Same display normalization every other field gets: a hostile AUMID must
    // not smuggle control characters or unbounded length into the label
    // as well.
    cap_meta(non_empty(value.to_string(), "Media"))
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

/// Whether a source that just lost its last session still owes the overlay a
/// terminal `Stopped`: only sources that last reported Playing or Paused need
/// one (Stopped was already announced, and a source that never reported a
/// state never showed anything), and a source on the churn cool-down must stay
/// silent per the cool-down contract.
fn terminal_stopped_warranted(last_known: Option<PlaybackState>, on_cooldown: bool) -> bool {
    !on_cooldown && matches!(last_known, Some(PlaybackState::Playing | PlaybackState::Paused))
}

/// Whether a pending terminal-Stopped entry survives this sync pass. A source
/// that still has an open session in the snapshot is not gone at all; a source
/// missing from the snapshot for less than `grace` may simply be mid-way
/// through recreating its session (YouTube Music tears down and re-registers
/// on every track change), so it must not be settled yet. Only a source
/// absent for the full grace settles, retiring a genuine last-source quit.
fn terminal_pending_keep(alive_in_snapshot: bool, absent_for: Duration, grace: Duration) -> bool {
    alive_in_snapshot || absent_for < grace
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn terminal_stopped_warranted_only_for_a_real_last_state_off_cooldown() {
        // A source that last played or paused owes the overlay a terminal
        // Stopped when its session vanishes.
        assert!(terminal_stopped_warranted(Some(PlaybackState::Playing), false));
        assert!(terminal_stopped_warranted(Some(PlaybackState::Paused), false));
        // A Stopped state was already announced, and a source that never
        // reported a state never showed anything.
        assert!(!terminal_stopped_warranted(Some(PlaybackState::Stopped), false));
        assert!(!terminal_stopped_warranted(None, false));
        // A churning source stays silent while on the cool-down.
        assert!(!terminal_stopped_warranted(Some(PlaybackState::Playing), true));
    }

    #[test]
    fn first_read_counts_toward_churn_only_for_content_free_sessions() {
        // A newly-created session whose title fell back to the source-app
        // label (empty properties.Title(), empty artist) is identity garbage:
        // charge it on its first read.
        let trackless = TrackInfo {
            source_app: "riot".into(),
            title: "riot".into(), // title fell back to the source label
            artist: String::new(),
            ..TrackInfo::default()
        };
        assert!(first_read_counts_toward_churn(true, &trackless));
        // The same read on a later pass (not first) is not new churn.
        assert!(!first_read_counts_toward_churn(false, &trackless));
        // A real skip reports a title on its first read: never charged, no
        // matter how fast the user skips.
        let real = TrackInfo {
            source_app: "youtube-music".into(),
            title: "The Emptiness Machine".into(),
            artist: "Linkin Park".into(),
            ..TrackInfo::default()
        };
        assert!(!first_read_counts_toward_churn(true, &real));
        // A session that reports a title but no artist yet is not placeholder
        // (title != source-app fallback), so it is not charged either.
        let titled = TrackInfo {
            source_app: "spotify".into(),
            title: "Song".into(),
            artist: String::new(),
            ..TrackInfo::default()
        };
        assert!(!first_read_counts_toward_churn(true, &titled));
    }

    #[test]
    fn terminal_pending_keep_walks_the_grace_window() {
        // A source with an open session is never settled, no matter how long
        // the entry has been around.
        assert!(terminal_pending_keep(
            true,
            Duration::from_secs(60),
            TERMINAL_STOP_GRACE
        ));
        // A source absent for less than the grace is kept: it may be
        // mid-recreation (the new session registers within one poll).
        assert!(terminal_pending_keep(
            false,
            TERMINAL_STOP_GRACE / 2,
            TERMINAL_STOP_GRACE
        ));
        // The boundary itself settles: exactly the grace means the source is
        // really gone.
        assert!(!terminal_pending_keep(false, TERMINAL_STOP_GRACE, TERMINAL_STOP_GRACE));
        // Absence beyond the grace settles too.
        assert!(!terminal_pending_keep(
            false,
            TERMINAL_STOP_GRACE + Duration::from_secs(1),
            TERMINAL_STOP_GRACE
        ));
    }

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
    fn normalize_timeline_is_safe_on_extreme_and_missing_values() {
        // checked_sub: end - start over the whole i64 span must be None, never
        // a panic or a wrapped value.
        let (duration, _, _) = normalize_timeline(Some(i64::MIN), Some(i64::MAX), None, None);
        assert_eq!(duration, None);
        // A span that ends before it starts (negative) is no duration.
        let (duration, _, _) = normalize_timeline(Some(200), Some(100), None, None);
        assert_eq!(duration, None);
        // A sub-second span has no whole-second duration.
        let (duration, _, _) = normalize_timeline(Some(0), Some(5_000_000), None, None);
        assert_eq!(duration, None);
        // Exactly one second is the lower bound of the reported range.
        let (duration, _, _) = normalize_timeline(Some(0), Some(10_000_000), None, None);
        assert_eq!(duration, Some(1));
        // 300_000_000_000 ticks = 30 000 seconds.
        let (duration, _, _) = normalize_timeline(Some(0), Some(300_000_000_000), None, None);
        assert_eq!(duration, Some(30_000));
        // A missing boundary yields no duration.
        let (duration, _, _) = normalize_timeline(None, Some(100), None, None);
        assert_eq!(duration, None);
    }

    #[test]
    fn normalize_timeline_clamps_position_and_cleans_rate() {
        // A negative position (timeline before the start mark) clamps to 0.0.
        let (_, position, _) = normalize_timeline(None, None, Some(-50_000_000), None);
        assert_eq!(position, Some(0.0));
        // A position past the end clamps to the duration.
        let (duration, position, _) = normalize_timeline(
            Some(0),
            Some(120_000_000_000),       // 12 000 s
            Some(1_000_000_000_000_000), // 100 000 000 s, far past the end
            None,
        );
        assert_eq!(duration, Some(12_000));
        assert_eq!(position, Some(12_000.0));
        // Without a duration, an in-range position is clamped to >= 0 only.
        let (duration, position, _) = normalize_timeline(None, None, Some(50_000_000), None);
        assert_eq!(duration, None);
        assert_eq!(position, Some(5.0));
        // A position inside the track passes through unchanged.
        let (_, position, _) = normalize_timeline(Some(0), Some(120_000_000_000), Some(65_000_000), None);
        assert_eq!(position, Some(6.5));

        // Rates outside 0.0..=16.0, or non-finite, are dropped entirely.
        assert_eq!(normalize_timeline(None, None, None, Some(f64::NAN)).2, None);
        assert_eq!(normalize_timeline(None, None, None, Some(f64::INFINITY)).2, None);
        assert_eq!(normalize_timeline(None, None, None, Some(f64::NEG_INFINITY)).2, None);
        assert_eq!(normalize_timeline(None, None, None, Some(-1.0)).2, None);
        assert_eq!(normalize_timeline(None, None, None, Some(17.5)).2, None);
        // The accepted band is inclusive at both ends.
        assert_eq!(normalize_timeline(None, None, None, Some(0.0)).2, Some(0.0));
        assert_eq!(normalize_timeline(None, None, None, Some(16.0)).2, Some(16.0));
        assert_eq!(normalize_timeline(None, None, None, Some(2.0)).2, Some(2.0));
    }

    #[test]
    fn metadata_preview_is_bounded_and_escaped() {
        // Control characters are escaped, so raw control bytes never reach
        // the log output verbatim. `escape_debug` writes NUL as `\0`.
        let (preview, omitted) = metadata_preview("a\u{0}b\nc");
        assert_eq!(preview, "a\\0b\\nc");
        assert_eq!(omitted, 0);
        // A long value is cut at the preview cap and reports what was left
        // out.
        let (preview, omitted) = metadata_preview(&"x".repeat(300));
        assert_eq!(preview, "x".repeat(128));
        assert_eq!(omitted, 172);
        // The boundary itself: exactly MAX_PREVIEW_CHARS omits nothing.
        let (preview, omitted) = metadata_preview(&"y".repeat(128));
        assert_eq!(preview, "y".repeat(128));
        assert_eq!(omitted, 0);
    }

    #[test]
    fn cap_meta_strips_display_unsafe_controls_and_preserves_unicode() {
        // C0 controls: NUL and newline must never reach a displayed field.
        assert_eq!(cap_meta("So\u{0}ng\nArtist".into()), "SongArtist");
        // DEL (0x7F) and the C1 range are stripped too.
        assert_eq!(cap_meta("So\u{7f}ng".into()), "Song");
        assert_eq!(cap_meta("Song\u{85}padded".into()), "Songpadded");
        // Bidi formatting / override / isolate commands are removed...
        assert_eq!(cap_meta("S\u{202E}ong".into()), "Song");
        assert_eq!(cap_meta("Sp\u{202D}otify".into()), "Spotify");
        assert_eq!(cap_meta("S\u{2066}ong".into()), "Song");
        // ...while ordinary RTL letters, combining marks, emoji and ZWJ
        // sequences are preserved: only directionality commands are stripped,
        // never scripts or joiners.
        let rtl = "سلام عليكم";
        assert_eq!(cap_meta(rtl.into()), rtl);
        let combining = "e\u{301}";
        assert_eq!(cap_meta(combining.into()), combining);
        let emoji = "🎵💿";
        assert_eq!(cap_meta(emoji.into()), emoji);
        let zwj = "👨\u{200D}👩\u{200D}👧";
        assert_eq!(cap_meta(zwj.into()), zwj);
        // The character cap still applies after stripping.
        let long = format!("{}\u{0}", "x".repeat(300));
        assert_eq!(cap_meta(long).chars().count(), MAX_META_CHARS);
    }

    #[test]
    fn source_app_label_is_capped_and_sanitized() {
        // A hostile AUMID with none of the shrinking separators falls back to
        // the same cap as every other metadata field.
        let label = source_app_label(&"A".repeat(500));
        assert_eq!(label.chars().count(), MAX_META_CHARS);
        // Control characters cannot smuggle into the label.
        assert_eq!(source_app_label("Publisher!Spo\u{0}tify"), "Spotify");
        // A label derived from a huge suffix is capped too.
        let label = source_app_label(&format!("Spotify;{}", "0".repeat(300)));
        assert_eq!(label.chars().count(), MAX_META_CHARS);
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
        // An empty artist on a new title stays empty (the pill shows
        // "Unknown Artist" as a placeholder).
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
    fn map_playback_type_maps_every_os_variant() {
        assert_eq!(
            map_playback_type(windows::Media::MediaPlaybackType::Music),
            PlaybackType::Music
        );
        assert_eq!(
            map_playback_type(windows::Media::MediaPlaybackType::Video),
            PlaybackType::Video
        );
        assert_eq!(
            map_playback_type(windows::Media::MediaPlaybackType::Image),
            PlaybackType::Image
        );
        assert_eq!(
            map_playback_type(windows::Media::MediaPlaybackType::Unknown),
            PlaybackType::Unknown
        );
    }

    #[test]
    fn merge_track_inherits_playback_type_across_stable_identity() {
        // A poll read (which never re-reports the type) keeps the last known
        // type on the same identity: a video session must not be demoted to
        // the music glyph by the poll.
        let prev = LogicalState {
            playback_type: PlaybackType::Video,
            ..state("Song", "Artist")
        };
        assert_eq!(
            merge_track(&prev, &track("Song", "Artist"), false).playback_type,
            PlaybackType::Video
        );
        // An explicitly reported type always wins over the inherited one.
        let video_read = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            playback_type: PlaybackType::Music,
            ..TrackInfo::default()
        };
        assert_eq!(
            merge_track(&prev, &video_read, false).playback_type,
            PlaybackType::Music
        );
        // A different identity never inherits the previous track's type.
        let other = TrackInfo {
            title: "Other".into(),
            artist: "Artist".into(),
            playback_type: PlaybackType::Unknown,
            ..TrackInfo::default()
        };
        assert_eq!(merge_track(&prev, &other, false).playback_type, PlaybackType::Unknown);
    }

    #[test]
    fn content_differ_ignores_playback_type() {
        // The type only selects the glyph; a type change alone on the same
        // track must not re-emit (the pill keeps its glyph until the next
        // track change).
        let video = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            playback_type: PlaybackType::Video,
            ..TrackInfo::default()
        };
        assert!(!content_differ(&state("Song", "Artist"), &video));
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
    fn is_placeholder_like_rejects_source_app_fallback_independent_of_first_read() {
        // Title fell back to source_app and artist is empty → placeholder, and
        // this does NOT depend on it being a first read (the bug that flashed a
        // "sample track" on a re-created session's non-first placeholder read).
        let placeholder = TrackInfo {
            title: "spotify".into(),
            artist: "".into(),
            source_app: "spotify".into(),
            ..TrackInfo::default()
        };
        assert!(is_placeholder_like(&placeholder));

        // Real title + source as artist is still a real track, not a placeholder.
        assert!(!is_placeholder_like(&track("Payphone", "Artist")));

        // A real title with an empty artist is NOT the source-app fallback, so it
        // is a real track that should still be announced.
        let empty_artist = TrackInfo {
            title: "Payphone".into(),
            artist: "".into(),
            source_app: "spotify".into(),
            ..TrackInfo::default()
        };
        assert!(!is_placeholder_like(&empty_artist));
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
    fn stale_thumbnail_drops_byte_equal_art_only_for_a_new_identity() {
        let art_a = Arc::<[u8]>::from(vec![1u8, 2, 3, 4]);
        let last = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            artwork: Some(art_a.clone()),
            ..TrackInfo::default()
        };
        // A different identity re-reading the previous track's exact bytes is
        // the stale-thumbnail signature (SMTC updates the thumbnail stream
        // after the text fields): dropped.
        let next = TrackInfo {
            title: "Other".into(),
            artist: "Artist".into(),
            artwork: Some(art_a.clone()),
            ..TrackInfo::default()
        };
        assert!(stale_thumbnail(&next, Some(&last)));
        // Same identity keeps byte-equal art: a legitimately shared album
        // cover across a playlist must never be dropped.
        let same_identity = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            artwork: Some(art_a.clone()),
            ..TrackInfo::default()
        };
        assert!(!stale_thumbnail(&same_identity, Some(&last)));
        // Different bytes for a different identity are the real cover: kept.
        let real_cover = TrackInfo {
            title: "Other".into(),
            artist: "Artist".into(),
            artwork: Some(Arc::<[u8]>::from(vec![9u8, 8, 7, 6])),
            ..TrackInfo::default()
        };
        assert!(!stale_thumbnail(&real_cover, Some(&last)));
        // No last emit, or no art on either side: never stale.
        assert!(!stale_thumbnail(&next, None));
        let artless_last = TrackInfo {
            artwork: None,
            ..last.clone()
        };
        assert!(!stale_thumbnail(&next, Some(&artless_last)));
    }

    #[test]
    fn stale_art_drop_defers_the_emit_until_the_retry_surfaces_the_real_cover() {
        // The transition window pairs the NEW identity with the PREVIOUS
        // track's exact thumbnail bytes (SMTC updates the thumbnail stream
        // after the text fields). The stale guard drops that art, and the
        // read would otherwise emit an ARTLESS pill for the new track; the
        // ~2s artwork retry (which bypasses the stale guard) then emits the
        // SAME track again WITH art — two pills for one transition, the
        // first coverless. The emit gate must therefore defer the artless
        // variant. Assert the two predicates the gate composes: the read is
        // stale-flagged, and the merged read would emit.
        let art = Arc::<[u8]>::from(vec![1u8, 2, 3, 4]);
        let last = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            artwork: Some(art.clone()),
            ..TrackInfo::default()
        };
        // New identity (title differs), same byte-equal cover as last emitted.
        let next = TrackInfo {
            title: "Other".into(),
            artist: "Artist".into(),
            artwork: Some(art),
            ..TrackInfo::default()
        };
        assert!(
            stale_thumbnail(&next, Some(&last)),
            "the transition read must be stale-flagged"
        );
        // The stale guard drops the art before the emit decision runs.
        let artless = TrackInfo {
            artwork: None,
            ..next.clone()
        };
        let prev = LogicalState {
            title: "Song".into(),
            artist: "Artist".into(),
            has_artwork: true,
            ..LogicalState::default()
        };
        assert!(
            emit_track(&prev, &artless, true).0,
            "without the gate the dropped-art read would still emit an artless pill"
        );
        // The gate defers; it works when the retry reads the REAL cover
        // (different bytes) for a track not already shown with art: emit.
        let real_cover = TrackInfo {
            title: "Other".into(),
            artist: "Artist".into(),
            artwork: Some(Arc::<[u8]>::from(vec![9u8, 8, 7, 6])),
            ..TrackInfo::default()
        };
        assert!(retry_should_emit(&real_cover, Some(&last)));
        // A recreated session re-reporting a track whose cover is already
        // shown must not re-emit (the duplicate-pill case the gate fixes).
        assert!(!retry_should_emit(&real_cover, Some(&real_cover)));
        // No real cover yet (art still absent from the stream): nothing to
        // surface, so the retry does not emit a second pill.
        assert!(!retry_should_emit(&artless, Some(&last)));
    }

    #[test]
    fn palette_for_identity_reuses_the_cached_palette_across_byte_changes() {
        let mut cache = HashMap::new();
        // A plausible solid RGBA cover (all white) at the palette grid size:
        // derives a monochrome palette.
        let cover: Vec<u8> = vec![255u8; 16 * 16 * 4];
        let first = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        };
        let palette =
            palette_for_identity(&mut cache, &first, Some(&cover)).expect("a valid cover must yield a palette");
        assert_eq!(cache.len(), 1);
        // The same identity re-encoded (different bytes, same cover): the
        // cache serves the original palette instead of recomputing — this is
        // the guarantee that keeps the pill's accent stable.
        let reencoded: Vec<u8> = vec![254u8; 16 * 16 * 4];
        let again = palette_for_identity(&mut cache, &first, Some(&reencoded));
        assert_eq!(again, Some(palette));
        assert_eq!(cache.len(), 1, "a re-encode must not replace the cached palette");
        // A different identity derives and caches its own palette.
        let other = TrackInfo {
            title: "Other".into(),
            artist: "Artist".into(),
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        };
        let other_palette = palette_for_identity(&mut cache, &other, Some(&cover));
        assert!(other_palette.is_some());
        assert_eq!(cache.len(), 2);
        // A cached identity without fresh decoded art still serves its
        // palette; an identity that never had art stays None and caches
        // nothing.
        assert_eq!(palette_for_identity(&mut cache, &first, None), Some(palette));
        let artless = TrackInfo {
            title: "Artless".into(),
            artist: "Artist".into(),
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        };
        assert_eq!(palette_for_identity(&mut cache, &artless, None), None);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn palette_cache_is_capped_at_the_constant() {
        let mut cache = HashMap::new();
        let cover: Vec<u8> = vec![255u8; 16 * 16 * 4];
        // More distinct identities than the cap, from one long-lived source:
        // the bound must hold and every miss must still derive a palette.
        for i in 0..(PALETTE_CACHE_CAP + 10) {
            let track = TrackInfo {
                source_app: "youtube-music".into(),
                title: format!("Song-{i}"),
                artist: "Artist".into(),
                ..TrackInfo::default()
            };
            let palette = palette_for_identity(&mut cache, &track, Some(&cover));
            assert!(palette.is_some(), "identity {i} must derive a palette");
        }
        assert_eq!(cache.len(), PALETTE_CACHE_CAP);
    }

    #[test]
    fn palette_cache_keys_round_trip_the_source_field() {
        let key = palette_cache_key("youtube-music", "Some Title", "Some Artist");
        assert_eq!(palette_key_source(&key), "youtube-music");
        // Exactly three NUL-separated segments: source, title, artist.
        assert_eq!(key.split('\0').count(), 3);
        assert_eq!(palette_key_source("no-separator"), "no-separator");
        assert_eq!(palette_key_source(""), "");
    }

    #[test]
    fn palette_cache_prunes_departed_sources_like_sync_subscriptions() {
        let mut cache = HashMap::new();
        let cover: Vec<u8> = vec![255u8; 16 * 16 * 4];
        let track = |source: &str, title: &str| TrackInfo {
            source_app: source.into(),
            title: title.into(),
            artist: "Artist".into(),
            ..TrackInfo::default()
        };
        for (source, title) in [("alpha", "A"), ("alpha", "B"), ("zeta", "Z")] {
            let _ = palette_for_identity(&mut cache, &track(source, title), Some(&cover));
        }
        assert_eq!(cache.len(), 3);
        // The exact retain the production sync uses: departed source gone,
        // surviving source's entries kept.
        let active: HashSet<String> = ["zeta"].into_iter().map(str::to_owned).collect();
        cache.retain(|key, _| active.contains(palette_key_source(key)));
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key(&palette_cache_key("zeta", "Z", "Artist")));
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
    fn recreation_suppression_only_applies_while_own_pill_is_shown() {
        // The ZuneMusic -> YouTube Music switch-back case: the last emitted
        // track for youtube-music ("All Fall Down", 19 min ago) is re-reported
        // by a recreated session, but the last pill on screen belongs to
        // ZuneMusic. The re-report must re-emit (the overlay cache was evicted
        // in the meantime), not be suppressed as recreation noise.
        let source = "youtube-music";
        let prev = TrackInfo {
            source_app: source.into(),
            ..track("All Fall Down", "OneRepublic")
        };
        let same = TrackInfo {
            source_app: source.into(),
            ..track("All Fall Down", "OneRepublic")
        };
        // Pill already shows this source's track: recreation noise.
        assert!(should_suppress_recreation(Some(&prev), &same, true, Some(source)));
        // Another app's pill was the last thing shown: switch-back re-emits.
        assert!(!should_suppress_recreation(Some(&prev), &same, true, Some("ZuneMusic")));
        // No pill shown yet (first emit of the session): never suppressed.
        assert!(!should_suppress_recreation(Some(&prev), &same, true, None));
        // No prior emit for this source: no dedup baseline.
        assert!(!should_suppress_recreation(None, &same, true, Some(source)));
        // A different track is never recreation noise.
        assert!(!should_suppress_recreation(
            Some(&prev),
            &TrackInfo {
                source_app: source.into(),
                ..track("Tyrant", "OneRepublic")
            },
            true,
            Some(source)
        ));
        // Poll reads (no artwork read) keep the same suppression behavior.
        assert!(should_suppress_recreation(Some(&prev), &same, false, Some(source)));
        assert!(!should_suppress_recreation(
            Some(&prev),
            &same,
            false,
            Some("ZuneMusic")
        ));
        // Artwork gained on the re-report is never suppressed: the pill must
        // refresh with the cover even while its own pill is on screen.
        let with_art = TrackInfo {
            artwork: Some(Arc::from(vec![1])),
            ..same.clone()
        };
        assert!(!should_suppress_recreation(Some(&prev), &with_art, true, Some(source)));
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
    fn poll_force_defers_stale_art_to_the_retry_within_budget() {
        // A first-read awaiting-artwork deferral is never gated by the
        // retry budget: the timeout force still guarantees the pill shows.
        let s = LogicalState::default();
        assert!(poll_force_allowed(&s));
        // A stale-art deferral inside the budget is resolved by the pending
        // retry in the same poll pass; forcing now would flash an artless
        // pill the retry immediately re-emits with art.
        let mut stale = LogicalState {
            deferred_for_stale_art: true,
            ..LogicalState::default()
        };
        assert!(!poll_force_allowed(&stale));
        // Budget exhausted → the "always eventually shows something"
        // guarantee wins over the flash concern.
        stale.artwork_attempts = ARTWORK_RETRY_BUDGET;
        assert!(poll_force_allowed(&stale));
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

    #[test]
    fn artwork_refresh_absorbed_when_a_state_event_is_in_the_batch() {
        let with_art = TrackInfo {
            title: "Battle Symphony".into(),
            artist: "Linkin Park".into(),
            source_app: "youtube-music".into(),
            artwork: Some(Arc::from(vec![1])),
            ..TrackInfo::default()
        };
        // The observed pause case: Paused for the same source is in the batch
        // when the artwork-changed force would fire → absorb.
        let paused = vec![MediaEvent::PlaybackStateChanged(
            PlaybackState::Paused,
            "youtube-music".into(),
        )];
        assert!(artwork_refresh_absorbed(&paused, &with_art));

        // Playing for the same source absorbs too (play after a cover swap).
        let playing = vec![MediaEvent::PlaybackStateChanged(
            PlaybackState::Playing,
            "youtube-music".into(),
        )];
        assert!(artwork_refresh_absorbed(&playing, &with_art));

        // No state event in the batch → a genuine cover swap still emits.
        assert!(!artwork_refresh_absorbed(&[], &with_art));

        // A state event from another source does not absorb this refresh.
        let other_source = vec![MediaEvent::PlaybackStateChanged(
            PlaybackState::Paused,
            "spotify".into(),
        )];
        assert!(!artwork_refresh_absorbed(&other_source, &with_art));

        // Artwork absent is never absorbed: the artwork-timeout first pill
        // and the artwork-lost path keep their existing behavior.
        let no_art = track("Battle Symphony", "Linkin Park");
        assert!(!artwork_refresh_absorbed(&paused, &no_art));
    }

    #[test]
    fn content_differ_excludes_position() {
        let prev = LogicalState {
            title: "Song".into(),
            artist: "Artist".into(),
            source_app: "spotify".into(),
            ..LogicalState::default()
        };
        let read = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            source_app: "spotify".into(),
            ..TrackInfo::default()
        };
        let mut advanced = read.clone();
        advanced.position_secs = Some(42.0);
        // Title/artist/source match prev, so content is unchanged; the only
        // difference is the live position, which must not trigger a re-emit.
        assert!(!content_differ(&prev, &read));
        assert!(!content_differ(&prev, &advanced));
        // A genuine content change still differs.
        let mut renamed = read.clone();
        renamed.title = "Other".into();
        assert!(content_differ(&prev, &renamed));
    }

    #[test]
    fn merge_track_carries_the_snapshot_playback_state() {
        // The TrackChanged snapshot is authoritative. The playback state
        // read in the same `GetPlaybackInfo` call travels with the track through
        // `merge_track`, so the pill never infers it from event ordering.
        let prev = LogicalState::default();
        let mut read = track("Song", "Artist");
        for state in [PlaybackState::Playing, PlaybackState::Paused, PlaybackState::Stopped] {
            read.playback_state = Some(state);
            assert_eq!(
                merge_track(&prev, &read, false).playback_state,
                Some(state),
                "merge_track must carry {state:?} out of the read snapshot"
            );
        }
        // Transitional/unknown reads arrive as None and pass through unchanged,
        // so the caller falls back to the remembered source state, then Playing.
        read.playback_state = None;
        assert_eq!(merge_track(&prev, &read, false).playback_state, None);
    }

    #[test]
    fn snapshot_playback_state_maps_terminal_statuses_only() {
        use GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status;
        assert_eq!(snapshot_playback_state(Status::Playing), Some(PlaybackState::Playing));
        assert_eq!(snapshot_playback_state(Status::Paused), Some(PlaybackState::Paused));
        // A stopped session is "not playing": the snapshot must not claim the
        // pill should play when the session is torn down.
        assert_eq!(snapshot_playback_state(Status::Stopped), Some(PlaybackState::Stopped));
        // Transitional and closed statuses carry no authoritative state: the
        // retry path skips Closed entirely, and a transitional status leaves
        // the pill with the historical "a track pill plays" behavior.
        for transitional in [Status::Opened, Status::Changing, Status::Closed] {
            assert_eq!(snapshot_playback_state(transitional), None);
        }
    }

    #[test]
    fn seek_detection_emits_on_position_jump() {
        let prev = LogicalState {
            title: "Song".into(),
            artist: "Artist".into(),
            source_app: "spotify".into(),
            last_position_secs: Some(10),
            ..LogicalState::default()
        };
        let make = |pos: Option<f64>| TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            source_app: "spotify".into(),
            position_secs: pos,
            ..TrackInfo::default()
        };
        // A 40 s jump is a seek → re-emit.
        assert!(emit_track(&prev, &make(Some(50.0)), false).0);
        // A normal ~1 s advance is not a seek → no emit (content unchanged).
        assert!(!emit_track(&prev, &make(Some(11.0)), false).0);
        // Presence flip (position appeared) → re-emit.
        let prev_none = LogicalState {
            last_position_secs: None,
            ..prev.clone()
        };
        assert!(emit_track(&prev_none, &make(Some(5.0)), false).0);
        // Presence flip (position vanished) → re-emit.
        assert!(emit_track(&prev, &make(None), false).0);
    }

    #[test]
    fn seek_delta_scales_with_playback_rate() {
        let prev = LogicalState {
            title: "Song".into(),
            artist: "Artist".into(),
            source_app: "spotify".into(),
            last_position_secs: Some(10),
            ..LogicalState::default()
        };
        let make = |pos: Option<f64>, rate: Option<f64>| TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            source_app: "spotify".into(),
            position_secs: pos,
            playback_rate: rate,
            ..TrackInfo::default()
        };
        // At 2x playback a ~4 s advance between two reads is normal cadence
        // (~2 s poll gap scaled by the rate), not a seek. The fixed 3.0 s
        // delta would have flagged it; the rate-scaled threshold (6.0 s) does
        // not.
        assert!(!emit_track(&prev, &make(Some(14.0), Some(2.0)), false).0);
        // The same 4 s delta at 1x IS a genuine seek.
        assert!(emit_track(&prev, &make(Some(14.0), Some(1.0)), false).0);
        // A real 10 s jump at 2x clears the scaled threshold and re-emits.
        assert!(emit_track(&prev, &make(Some(20.0), Some(2.0)), false).0);
        // No rate reported: the base delta applies unchanged.
        assert!(!emit_track(&prev, &make(Some(11.0), None), false).0);
        assert!(emit_track(&prev, &make(Some(50.0), None), false).0);
    }

    #[test]
    fn fresh_session_first_read_defers_for_artwork_over_seek() {
        // A recreated session (sources recreate one per track change) starts
        // from a default LogicalState: its first read reports a position
        // presence flip, which must NOT bypass the artwork deferral — the
        // thumbnail populates ~500 ms after the title, so emitting here would
        // show a title-only pill and swap the cover in under it.
        let prev = LogicalState::default();
        let read = TrackInfo {
            title: "New Song".into(),
            artist: "New Artist".into(),
            source_app: "spotify".into(),
            position_secs: Some(0.0),
            ..TrackInfo::default()
        };
        // Artwork not readable yet: defer despite the position flip.
        assert!(!emit_track(&prev, &read, true).0);
        // Artwork readable on the same first read: emit normally.
        let with_art = TrackInfo {
            artwork: Some(Arc::from([1u8, 2, 3, 4])),
            ..read
        };
        assert!(emit_track(&prev, &with_art, true).0);
    }

    #[test]
    fn admission_blocked_respects_session_and_source_caps() {
        let empty: HashSet<String> = HashSet::new();
        // Session cap ceiling hits first.
        assert!(admission_blocked(64, &empty, "A", 64, 32));
        assert!(!admission_blocked(63, &empty, "A", 64, 32));
        // A brand-new source at the source cap is blocked.
        let full: HashSet<String> = (0..32).map(|i| format!("s{i}")).collect();
        assert!(admission_blocked(0, &full, "new", 64, 32));
        // An already-tracked source never trips the source cap.
        assert!(!admission_blocked(63, &full, "s0", 64, 32));
    }

    #[test]
    fn admit_sessions_keeps_current_first_under_a_tight_session_cap() {
        // The first entry is the current session; with a session cap of 1 it
        // must be the one retained, not a later new session.
        let ordered: Vec<(usize, String)> = vec![(1, "A".into()), (2, "A".into()), (3, "A".into())];
        let (admitted, rejected) = admit_sessions(&ordered, &HashSet::new(), &HashSet::new(), 1, 100);
        assert!(admitted.contains(&1), "current session must be admitted first");
        assert!(!admitted.contains(&2));
        assert_eq!(rejected, 2);
    }

    #[test]
    fn admit_sessions_retains_existing_before_new() {
        // A surviving existing subscription fills its slot; a genuinely new
        // session is rejected by the cap instead of evicting a live one.
        let ordered: Vec<(usize, String)> = vec![(2, "A".into())];
        let existing_keys: HashSet<usize> = [1].into_iter().collect();
        let existing_sources: HashSet<String> = ["A".to_string()].into_iter().collect();
        let (admitted, rejected) = admit_sessions(&ordered, &existing_keys, &existing_sources, 1, 100);
        assert!(admitted.contains(&1), "existing subscription is retained");
        assert!(!admitted.contains(&2));
        assert_eq!(rejected, 1);
    }

    #[test]
    fn admit_sessions_rejects_the_65th_session_only() {
        // 65 sessions of one source, session cap 64: 64 admitted, 1 rejected.
        let ordered: Vec<(usize, String)> = (0..65).map(|k| (k, "youtube-music".to_string())).collect();
        let (admitted, rejected) = admit_sessions(&ordered, &HashSet::new(), &HashSet::new(), 64, 100);
        assert_eq!(admitted.len(), 64);
        assert_eq!(rejected, 1);
    }

    #[test]
    fn admit_sessions_rejects_the_33rd_source_only() {
        // 33 distinct sources, source cap 32: 32 admitted, 1 rejected.
        let ordered: Vec<(usize, String)> = (0..33).map(|k| (k, format!("src-{k}"))).collect();
        let (admitted, rejected) = admit_sessions(&ordered, &HashSet::new(), &HashSet::new(), 100, 32);
        assert_eq!(admitted.len(), 32);
        assert_eq!(rejected, 1);
    }

    #[test]
    fn displacement_victim_prefers_dead_then_weakest() {
        // Candidate keys in reverse admission priority: the current session
        // (10) is last; before-group members 3, 2, 1 appear weakest-first;
        // 9 is a new-group key (never a candidate — it is not subscribed).
        let weakest_first: Vec<usize> = vec![9, 3, 2, 1, 10];
        let alive: HashSet<usize> = [1, 3].into_iter().collect();
        let subscribed: HashSet<usize> = [1, 2, 3].into_iter().collect();
        // Survivor 2 is dead (subscribed but not alive) → displaced before
        // the weaker-but-alive 3.
        assert_eq!(displacement_victim(&weakest_first, &alive, &subscribed, 10), Some(2));
        // Once 2 is evicted, the weakest alive survivor is next.
        let subscribed_after: HashSet<usize> = [1, 3].into_iter().collect();
        assert_eq!(
            displacement_victim(&weakest_first, &alive, &subscribed_after, 10),
            Some(3)
        );
        // The incoming current session is never its own victim.
        assert_eq!(displacement_victim(&weakest_first, &alive, &subscribed, 10), Some(2));
        // Nothing left to displace → none.
        assert_eq!(displacement_victim(&weakest_first, &alive, &HashSet::new(), 10), None);
    }

    #[test]
    fn dedup_capped_keeps_insertion_order_under_cap() {
        let out = dedup_capped(vec!["b".into(), "a".into(), "b".into(), "c".into(), "a".into()], 2);
        assert_eq!(out, vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn retained_art_bytes_counts_every_entry_even_when_buffers_are_shared() {
        // The budget is a conservative per-entry sum: it cannot see that two
        // entries clone the same Arc, so sharing over-counts. Err safe: the
        // retained bytes never exceed the cap even when buffers are shared.
        let mut map: HashMap<String, TrackInfo> = HashMap::new();
        let art: Arc<[u8]> = Arc::from([7u8; 1024]);
        map.insert(
            "a".into(),
            TrackInfo {
                artwork: Some(art.clone()),
                ..TrackInfo::default()
            },
        );
        map.insert(
            "b".into(),
            TrackInfo {
                artwork: Some(art.clone()),
                ..TrackInfo::default()
            },
        );
        assert_eq!(retained_art_bytes(&map), 2048);
        // A distinct identity with its own artwork adds to the total.
        map.insert(
            "c".into(),
            TrackInfo {
                artwork: Some(Arc::<[u8]>::from([9u8; 512])),
                ..TrackInfo::default()
            },
        );
        assert_eq!(retained_art_bytes(&map), 2560);
    }

    #[test]
    fn store_last_track_replaces_under_the_art_budget() {
        let mut map: HashMap<String, TrackInfo> = HashMap::new();
        let big: Arc<[u8]> = Arc::from(vec![1u8; MAX_RETAINED_ARTWORK_BYTES]);
        let small: Arc<[u8]> = Arc::from([2u8; 4]);
        map.insert(
            "only".into(),
            TrackInfo {
                artwork: Some(big),
                ..TrackInfo::default()
            },
        );
        store_last_track(
            &mut map,
            "only".to_string(),
            TrackInfo {
                artwork: Some(small),
                ..TrackInfo::default()
            },
            MAX_RETAINED_ARTWORK_BYTES,
        );
        assert_eq!(
            map["only"].artwork,
            Some(Arc::<[u8]>::from([2u8; 4])),
            "replacement must overwrite the old entry"
        );
        assert_eq!(
            retained_art_bytes(&map),
            4,
            "budget accounting must follow the replacement"
        );
    }

    #[test]
    fn store_last_track_drops_artwork_over_budget_and_keeps_the_placeholder() {
        // Inserting artwork that would push the total past the budget must
        // evict the bytes (false) while retaining the metadata, so the pill
        // renders a placeholder instead of a stale cover. The budget never
        // grows past the cap even across a replacement.
        let mut map: HashMap<String, TrackInfo> = HashMap::new();
        let big: Arc<[u8]> = Arc::from(vec![1u8; MAX_RETAINED_ARTWORK_BYTES]);
        let full = TrackInfo {
            artwork: Some(big),
            ..TrackInfo::default()
        };
        store_last_track(&mut map, "only".to_string(), full, MAX_RETAINED_ARTWORK_BYTES);
        assert_eq!(
            map["only"].artwork.as_deref().map(<[u8]>::len),
            Some(MAX_RETAINED_ARTWORK_BYTES)
        );

        // A second, artwork-bearing track cannot fit within the budget.
        let kept = store_last_track(
            &mut map,
            "second".to_string(),
            TrackInfo {
                artwork: Some(Arc::<[u8]>::from([3u8; 8])),
                ..TrackInfo::default()
            },
            MAX_RETAINED_ARTWORK_BYTES,
        );
        assert!(!kept, "over-budget artwork must be dropped");
        assert_eq!(
            map["second"].artwork, None,
            "the placeholder retains metadata, never stale cover bytes"
        );
        assert_eq!(
            retained_art_bytes(&map),
            MAX_RETAINED_ARTWORK_BYTES,
            "the retained budget must not exceed the cap after eviction"
        );
    }

    #[test]
    fn overflow_warn_allowed_is_unthrottled_without_a_last_warn() {
        let window = Duration::from_millis(5_000);
        assert!(overflow_warn_allowed(None, window));
    }

    #[test]
    fn overflow_warn_allowed_gates_by_the_warn_window() {
        let window = Duration::from_millis(5_000);
        // Just inside the window: still throttled.
        let recent = Instant::now() - Duration::from_millis(4_999);
        assert!(!overflow_warn_allowed(Some(recent), window));
        // Just outside the window: a new warn is allowed.
        let stale = Instant::now() - Duration::from_millis(5_001);
        assert!(overflow_warn_allowed(Some(stale), window));
    }

    #[test]
    fn coalesce_pending_event_newest_replaces_older_for_same_source() {
        // Two playback states for the same source supersede to the newest:
        // the retry mailbox must never deliver the older one after the newer
        // one was committed, or the UI would regress to a stale state.
        let mut queue = VecDeque::new();
        let first = Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Paused, "src".into()));
        let second = Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Playing, "src".into()));
        coalesce_pending_event(&mut queue, first, OUTPUT_RETRY_CAP);
        coalesce_pending_event(&mut queue, second.clone(), OUTPUT_RETRY_CAP);
        assert_eq!(queue.len(), 1, "the superseded event must be removed");
        assert!(Arc::ptr_eq(&queue[0], &second), "the newest event must be the survivor");
        // Progress updates for one source coalesce the same way (latest position wins).
        let mut queue = VecDeque::new();
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::ProgressChanged {
                source_app: "src".into(),
                position_secs: Some(5.0),
                duration_secs: Some(10),
                playback_rate: Some(1.0),
            }),
            OUTPUT_RETRY_CAP,
        );
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::ProgressChanged {
                source_app: "src".into(),
                position_secs: Some(9.0),
                duration_secs: Some(10),
                playback_rate: Some(1.0),
            }),
            OUTPUT_RETRY_CAP,
        );
        assert_eq!(queue.len(), 1, "progress coalesces per source");
        match queue[0].as_ref() {
            MediaEvent::ProgressChanged { position_secs, .. } => assert_eq!(*position_secs, Some(9.0)),
            other => panic!("expected ProgressChanged, got {other:?}"),
        }
    }

    #[test]
    fn coalesce_keeps_cross_source_order_and_drops_oldest_on_overflow() {
        // TrackChanged for different sources is distinct state: both must be
        // kept, in arrival order.
        let mut queue = VecDeque::new();
        let mut ta = track("A", "1");
        ta.source_app = "src-a".into();
        let mut tb = track("B", "1");
        tb.source_app = "src-b".into();
        let track_a = Arc::new(MediaEvent::TrackChanged(ta));
        let track_b = Arc::new(MediaEvent::TrackChanged(tb));
        coalesce_pending_event(&mut queue, track_a.clone(), OUTPUT_RETRY_CAP);
        coalesce_pending_event(&mut queue, track_b.clone(), OUTPUT_RETRY_CAP);
        assert_eq!(queue.len(), 2, "cross-source tracks keep arrival order");
        assert!(Arc::ptr_eq(&queue[0], &track_a), "first-arrived track stays first");

        // Over-cap drops the oldest queued event, never the newest
        // authoritative state just committed.
        let mut queue = VecDeque::new();
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "a".into())),
            2,
        );
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "b".into())),
            2,
        );
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "c".into())),
            2,
        );
        assert_eq!(queue.len(), 2, "cap must hold");
        match queue[0].as_ref() {
            MediaEvent::PlaybackStateChanged(_, source) => {
                assert_eq!(source, "b", "the oldest ('a') is dropped, newest survive")
            }
            other => panic!("expected PlaybackStateChanged, got {other:?}"),
        }
        match queue[1].as_ref() {
            MediaEvent::PlaybackStateChanged(_, source) => {
                assert_eq!(source, "c", "the newest authoritative state must survive")
            }
            other => panic!("expected PlaybackStateChanged, got {other:?}"),
        }
    }

    #[test]
    fn full_output_channel_replays_latest_state_after_drain() {
        // Capacity-1 channel: fill it, commit two playback states (the newer
        // supersedes the older in the mailbox), drain the channel, then flush.
        // Acceptance: the latest authoritative state arrives; nothing is
        // permanently invisible just because the channel was briefly full.
        let (tx, rx) = mpsc::sync_channel(1);
        tx.try_send(Arc::new(MediaEvent::PlaybackStateChanged(
            PlaybackState::Stopped,
            "occupy".into(),
        )))
        .unwrap();
        let mut queue = VecDeque::new();
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Paused, "src".into())),
            OUTPUT_RETRY_CAP,
        );
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Playing, "src".into())),
            OUTPUT_RETRY_CAP,
        );
        assert_eq!(queue.len(), 1, "the older Paused was superseded in the mailbox");
        let _ = rx.recv().unwrap();
        assert!(
            !drain_pending_to_channel(&mut queue, &tx),
            "channel is live again after the drain"
        );
        assert!(queue.is_empty(), "the queued state was delivered");
        match rx.try_recv() {
            Ok(event) => match event.as_ref() {
                MediaEvent::PlaybackStateChanged(PlaybackState::Playing, source) => assert_eq!(source, "src"),
                other => panic!("expected the newest Playing state, got {other:?}"),
            },
            Err(_) => panic!("the committed state must be delivered once the channel drains"),
        }
    }

    #[test]
    fn drain_stops_at_the_first_full_send_and_preserves_order() {
        // Capacity-1: A is sent, filling the channel; B stays queued. After
        // the receiver drains A, the next flush delivers B — in order.
        let (tx, rx) = mpsc::sync_channel(1);
        let mut queue = VecDeque::new();
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::TrackChanged(track("A", "1"))),
            OUTPUT_RETRY_CAP,
        );
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "a".into())),
            OUTPUT_RETRY_CAP,
        );
        assert!(
            !drain_pending_to_channel(&mut queue, &tx),
            "A filled the channel, B stays"
        );
        assert_eq!(queue.len(), 1, "only B is still queued");
        assert!(!drain_pending_to_channel(&mut queue, &tx), "channel still holds A");
        let _ = rx.recv().unwrap();
        assert!(!drain_pending_to_channel(&mut queue, &tx));
        assert!(queue.is_empty(), "B delivered after A was read");
        match rx.try_recv() {
            Ok(event) => match event.as_ref() {
                MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, source) => assert_eq!(source, "a"),
                other => panic!("expected B, got {other:?}"),
            },
            Err(_) => panic!("B must arrive after the channel drains"),
        }
    }

    #[test]
    fn drain_on_disconnected_channel_reports_so_the_mailbox_is_cleared() {
        // The forwarder is gone: nothing queued can ever be delivered, so the
        // caller clears the mailbox instead of retrying forever.
        let (tx, rx) = mpsc::sync_channel(1);
        let mut queue = VecDeque::new();
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "src".into())),
            OUTPUT_RETRY_CAP,
        );
        drop(rx);
        assert!(drain_pending_to_channel(&mut queue, &tx), "disconnect must be reported");
        queue.clear();
        assert!(queue.is_empty());
    }
}
