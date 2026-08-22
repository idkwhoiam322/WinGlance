use crate::events::{MediaEvent, PlaybackState, PlaybackType, TrackInfo, artwork_bytes, decode_artwork_pm};
use crate::palette::{Palette, palette_from_rgba};
use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    CurrentSessionChangedEventArgs, GlobalSystemMediaTransportControlsSession,
    GlobalSystemMediaTransportControlsSessionManager, GlobalSystemMediaTransportControlsSessionPlaybackStatus,
    MediaPropertiesChangedEventArgs, PlaybackInfoChangedEventArgs, SessionsChangedEventArgs,
    TimelinePropertiesChangedEventArgs,
};
use windows::Media::MediaPlaybackType;
use windows::Storage::Streams::{Buffer, DataReader, IRandomAccessStreamReference, InputStreamOptions};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Memory::{GetProcessHeap, HEAP_FLAGS, HeapCompact};
use windows::core::Interface;
use windows_future::AsyncStatus;

/// Configuration commands the main window pushes to the worker through the
/// latest-value control mailbox (see `ControlMailbox`); the worker applies
/// them on the next event-loop turn (`handle_control`) instead of polling
/// the shared config lock — the listener's per-turn config poll and its
/// last-seen markers existed only to paper over the up-to-2s lag of that
/// poll, and a pushed command is current the moment it is applied.
pub(crate) enum ControlCommand {
    /// The user toggled `behavior.notifications_enabled` in the settings
    /// pane or the tray menu. `true` also forces a one-shot re-show of the
    /// current session's track (the old poll's false→true transition), so
    /// the pill surfaces the live state immediately; `false` is a no-op
    /// here, because the overlay owns the actual suppression.
    SetNotificationsEnabled(bool),
    /// The user edited `behavior.media_sources`; the worker re-normalizes
    /// the patterns once at apply time and stores them, replacing its last
    /// pushed copy.
    SetAllowedSources(Vec<String>),
}

/// Latest-value mailbox for worker control commands. The main window
/// overwrites the newest command per kind (`push`); the worker drains it at
/// every event-loop turn (`drain`). Nothing is ever dropped, unlike a
/// bounded channel: a push made while the signal queue is saturated still
/// reaches the worker at its next turn, and — because the mailbox lives in
/// `main` and survives worker restarts — a command posted just before a
/// restart is applied by the replacement worker. Every command carries an
/// absolute value, so newest-wins coalescing is semantics-preserving.
#[derive(Default)]
pub(crate) struct ControlMailbox {
    notifications: Option<bool>,
    allowed_sources: Option<Vec<String>>,
}

impl ControlMailbox {
    /// Stores the command, replacing any older command of the same kind.
    pub(crate) fn push(&mut self, command: ControlCommand) {
        match command {
            ControlCommand::SetNotificationsEnabled(value) => self.notifications = Some(value),
            ControlCommand::SetAllowedSources(sources) => self.allowed_sources = Some(sources),
        }
    }

    /// Takes every pending command (at most one per kind) and clears the
    /// mailbox, in a fixed kind order.
    fn drain(&mut self) -> Vec<ControlCommand> {
        let mut commands = Vec::with_capacity(2);
        if let Some(value) = self.notifications.take() {
            commands.push(ControlCommand::SetNotificationsEnabled(value));
        }
        if let Some(sources) = self.allowed_sources.take() {
            commands.push(ControlCommand::SetAllowedSources(sources));
        }
        commands
    }
}

pub(crate) enum Signal {
    /// Fired by SessionsChanged or CurrentSessionChanged: re-sync the
    /// subscription map at the next flush (one re-sync per burst).
    Sessions,
    MediaProperties(GlobalSystemMediaTransportControlsSession),
    PlaybackInfo(GlobalSystemMediaTransportControlsSession),
    Timeline(GlobalSystemMediaTransportControlsSession),
    /// Best-effort wake-up posted by the main window after it wrote the
    /// control mailbox: it makes a control push apply on the next turn
    /// instead of waiting for the loop's scheduled wake. The wake shares
    /// the bounded queue with the WinRT signals and may be dropped when the
    /// queue is saturated — that only costs latency, never the command,
    /// which the worker drains from the mailbox at its next turn anyway.
    ControlWake,
}

struct SessionSubscription {
    session: GlobalSystemMediaTransportControlsSession,
    // Windows 0.62's refreshed metadata declares the SMTC event tokens as
    // plain 64-bit integers instead of the `EventRegistrationToken` struct;
    // the worker flows the raw `Value` so the subscription state is
    // version-agnostic (only the register/remove call sites convert).
    properties_token: i64,
    playback_token: i64,
    timeline_token: i64,
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
/// Bound on a single SMTC async read before the worker abandons it and
/// excludes the source (threat-model gap G4): the supervisor's restart
/// budget (`MAX_WORKER_RESTARTS`) is global — the SMTC manager is one
/// process-wide listener, a stall is a hang of that whole thread, and a hung
/// worker cannot report which source it was reading — so a hostile app that
/// supplies a never-completing media-properties operation or thumbnail
/// stream could otherwise hang the worker, get it restarted, and burn the
/// whole budget for every source. Bounding the async waits the *app controls*
/// (its session data and artwork streams) converts that vector into a 10 s
/// hiccup plus a per-source exclusion instead of a global stall. Chosen
/// comfortably below the 30 s `WORKER_STALL_THRESHOLD`: a timed-out read
/// must never push the heartbeat age toward a supervisor stall, and every
/// legitimate SMTC read (a few MiB of local thumbnail at most, system-
/// mediated metadata) completes in a fraction of this. Synchronous WinRT
/// calls (`GetPlaybackInfo`, `GetTimelineProperties`, `GetSessions`) are
/// COM calls that no timeout can bound; a hang there remains covered by the
/// supervisor restart and the global cap (documented residual).
const READ_ASYNC_TIMEOUT: Duration = Duration::from_secs(10);
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

/// Capacity of the merged signal channel between the WinRT event handlers,
/// the main window, and the listener loop. `try_send` drops a signal when
/// the queue is full; that is safe because every dropped signal is a
/// coalescible wake-up — the dirty-set membership it would have recorded is
/// re-covered by the periodic safety-net poll within 2s. Control commands
/// no longer share this queue: they live in the latest-value
/// `ControlMailbox` and are delivered on the next turn no matter what, so
/// only their optional wake-up hint (`Signal::ControlWake`) can be dropped.
/// The bound keeps a session storm from accumulating unbounded queued COM
/// session references.
pub(crate) const SIGNAL_QUEUE_CAP: usize = 256;

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
/// Smallest thumbnail stream WinGlance will read. Legitimate covers can be
/// compact 64-128 px PNGs/JPEGs that compress below 1 KiB; the floor exists
/// only to skip streams that are certainly not a cover (empty or a few stray
/// bytes). Anything below it is dropped with a debug log, never silently.
const THUMBNAIL_MIN_BYTES: u64 = 64;
const MAX_RETAINED_ARTWORK_BYTES: usize = 64 * 1024 * 1024;
/// Cap for the recoverable output retry mailbox (`pending_output`). The
/// mailbox exists so a briefly-full output channel cannot make a committed
/// state transition permanently invisible: events wait here and are re-sent
/// at the next event-loop turn. 256 matches the per-window queue caps.
const OUTPUT_RETRY_CAP: usize = 256;

/// Total artwork bytes the worker will hold in its outbound queues (the
/// event channel plus the retry mailbox) at once. Queue *counts* are capped,
/// but a `TrackChanged` can carry up to `MAX_THUMBNAIL_BYTES` of cover art
/// behind its `Arc`, so count caps alone admit ~1280 × 4 MiB ≈ 5 GiB of
/// queued art while a wedged forwarder drains slowly. The byte budget closes
/// that: when an event would push in-flight artwork past the budget, its
/// payload is dropped at emit time — raw cover, decode and derived palette
/// stripped, metadata kept, the pill renders a placeholder — the same trade
/// `MAX_RETAINED_ARTWORK_BYTES` makes. The shared counter is decremented as
/// the forwarder pops events and as the mailbox frees them, so a legitimate
/// cover flow resumes as soon as the UI catches up. Matches the retained-art
/// budget; normal operation (a few MiB of queued art at most) never
/// approaches it.
const MAX_IN_FLIGHT_ARTWORK_BYTES: u64 = 64 * 1024 * 1024;

/// Minimum gap between two overflow warnings, so a hostile storm of rejected
/// sessions cannot flood the log with one WARN per rejected session.
const OVERFLOW_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Bound on the once-per-appearance reporting sets (`rejected_seen`,
/// `ignored_seen`): a hostile storm of ever-new session keys cannot grow the
/// dedup sets without bound. When the cap is met, an arbitrary recorded key
/// is evicted so the set stays bounded; the evicted session may report once
/// more, which keeps the log volume bounded by unique keys either way.
const MAX_REPORTED_SESSIONS: usize = 1024;

/// Source labels of every currently open SMTC session, refreshed at each
/// subscription re-sync. The process picker reads this so media apps that run
/// without a visible window (tray-only Electron apps, background browser
/// tabs) still appear as selectable entries.
///
/// Deliberately a plain `OnceLock<Mutex<Vec<String>>>`, **not** the guarded
/// `winutil::Registered` slot the positioner/picker registrations use: this
/// is a cross-thread *cache* with whole-value replace semantics — the worker
/// rebuilds the dedup-capped list each re-sync and swaps it in wholesale
/// (the single write path), the picker reads a clone (never a guard), and
/// there is no teardown, no window identity, and no stale-write-vs-newer
/// registration scenario for a match-guard to protect. The newest snapshot
/// is always the truth, so the opaque-mutex machinery `Registered` exists
/// to enforce would guard nothing here.
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
    output: SyncSender<Arc<MediaEvent>>,
    /// Shared in-flight artwork byte counter. The worker adds the artwork
    /// bytes of every event it queues (channel or mailbox) and the forwarder
    /// subtracts them as it pops, so the counter tracks the distinct artwork
    /// allocations held by the outbound queues. `emit` consults it against
    /// `MAX_IN_FLIGHT_ARTWORK_BYTES` and drops the payload (metadata kept)
    /// when the budget would be exceeded; mailbox supersede/over-cap/clear
    /// drops free their bytes too. Shared with the forwarder across worker
    /// restarts, so queued events from a replaced worker stay accounted.
    in_flight_art: Arc<AtomicU64>,
    /// One-shot latch for the user-facing budget warning: set (via `swap`)
    /// the first time `emit` drops a cover payload, and `MediaEvent::
    /// ArtworkBudgetExceeded` is emitted exactly once per app run. Shared
    /// across worker restarts like the counter, so a replacement worker does
    /// not re-warn.
    budget_warned: Arc<AtomicBool>,
    /// Recoverable retry mailbox for events the bounded output channel could
    /// not accept immediately. Bounded; coalesced by (kind, source) with the
    /// newest superseding, drained in arrival order at every event-loop turn.
    /// Never blocks the worker: overflow drops the oldest superseded event,
    /// and the 2-second safety-net poll repairs state on top.
    pending_output: VecDeque<Arc<MediaEvent>>,
    signal_tx: SyncSender<Signal>,
    /// Latest-value mailbox for main-window control commands. Drained at
    /// the top of every event-loop turn (see `drain_control`), so a push is
    /// applied even when its wake-up hint was dropped by a saturated signal
    /// queue. Survives worker restarts: the mailbox is created in `main`
    /// and every worker generation drains the same slot.
    control_mailbox: Arc<Mutex<ControlMailbox>>,
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
    /// Source apps currently excluded from tracking until the stored time:
    /// either the churn cool-down (a session-recreation storm) or the
    /// wedged-read exclusion (an async read that timed out — see
    /// `READ_ASYNC_TIMEOUT`). Both entries mean the same thing downstream:
    /// `session_source_allowed` returns false, so the source's sessions are
    /// never read, subscribed, or emitted. Shared across worker generations
    /// (see `SharedExclusions`): a supervisor restart must not reset the
    /// exclusions the previous worker paid a 10 s read for.
    excluded_sources: SharedExclusions,
    /// Keys of rejected sessions already reported to the history, so a
    /// rejected session is logged once per appearance instead of on every
    /// re-sync (the 2-second poll re-lists all sessions). Bounded by
    /// `MAX_REPORTED_SESSIONS`.
    rejected_seen: HashSet<usize>,
    /// Keys of allowed-but-not-current sessions already reported, so the
    /// once-per-appearance gate applies to the "ignored" detail line the same
    /// way it does to rejected sessions. Bounded like `rejected_seen`.
    ignored_seen: HashSet<usize>,
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
    /// Debounce window for flush scheduling, seeded once at worker startup
    /// (the supervisor samples `behavior.debounce_ms` at each spawn). The
    /// field is set-only after startup: `debounce_ms` has no settings-pane
    /// row, so a hand edit needs a restart anyway; a control command would
    /// only add a write path nothing ever uses.
    debounce: Duration,
    /// Cached app icons keyed by source_app label (derived from AUMID via
    /// `source_app_label`). Populated on first encounter of a source.
    icon_cache: HashMap<String, Option<Arc<[u8]>>>,
    /// When the last overflow warning fired. Bounds the admission-rejection log
    /// to one line per `OVERFLOW_WARN_INTERVAL` during a hostile session storm,
    /// instead of one WARN per rejected session.
    last_overflow_warn: Option<Instant>,
    /// Last pushed `media_sources` list plus its pre-normalized patterns,
    /// seeded at worker startup and replaced by `SetAllowedSources`. The
    /// per-session check runs on the hot path (every re-sync of every
    /// session), so the patterns are normalized once at apply time instead
    /// of per check; there is no comparison against a live config because
    /// the settings UI is the only writer and it pushes every change.
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

/// The worker's config values, sampled once by the supervisor at each spawn.
/// The worker never reads the shared config lock again: live changes arrive
/// through the latest-value `ControlMailbox`, which survives worker
/// restarts, so a command posted just before a restart — the allow list and
/// the notifications toggle alike — is applied by the replacement worker at
/// its first turn. The seed still exists because a brand-new worker must
/// not wait for a push that may never come (the user may never touch the
/// settings again): it carries the config state as of the restart.
pub(crate) struct ListenerSeed {
    pub(crate) media_sources: Vec<String>,
    pub(crate) debounce_ms: u64,
}

/// Exclusion map (churn cool-downs + wedged-read exclusions) shared across
/// worker generations: created in `main` and handed to every worker the
/// supervisor spawns, so a replacement worker does not re-pay a fresh
/// `READ_ASYNC_TIMEOUT` read for every source its predecessor already
/// excluded — the exclusion survives the restart it exists to bound.
pub(crate) type SharedExclusions = Arc<Mutex<HashMap<String, Instant>>>;

/// Creates the process-wide shared exclusion map (see `SharedExclusions`).
pub(crate) fn shared_exclusions() -> SharedExclusions {
    Arc::new(Mutex::new(HashMap::new()))
}

pub struct SmtcListener {
    output: SyncSender<Arc<MediaEvent>>,
    /// Shared in-flight artwork byte counter (see `MAX_IN_FLIGHT_ARTWORK_BYTES`
    /// and `ListenerState::in_flight_art`).
    in_flight_art: Arc<AtomicU64>,
    /// One-shot latch: set when the in-flight artwork budget drops a cover
    /// payload, so the user gets exactly one tray warning per app run (see
    /// `ListenerState::budget_warned`).
    budget_warned: Arc<AtomicBool>,
    seed: ListenerSeed,
    /// Sender half of the merged wake-up channel, cloned into the WinRT
    /// event handlers and used by the main window for control wake-up
    /// hints. The channel itself is created in `main` and survives worker
    /// restarts, so a wake posted by the main window is never lost to a
    /// restart.
    control_tx: SyncSender<Signal>,
    /// Latest-value mailbox for control commands, created in `main` and
    /// shared across worker restarts: the replacement worker drains what
    /// its predecessor left behind, so a command is applied even when it
    /// was posted between a stall and its successor's first turn.
    control_mailbox: Arc<Mutex<ControlMailbox>>,
    /// Receiver half of the same channel. `main` wraps it in a mutex because
    /// a replacement worker must receive from the channel its predecessor
    /// left behind; the worker's event loop is the only receive site and
    /// senders never take this lock, so it is held at most across one
    /// `recv_timeout`.
    control_rx: Arc<Mutex<mpsc::Receiver<Signal>>>,
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
    /// Shared exclusion map (see `ListenerState::excluded_sources`).
    /// Survives worker restarts: exclusions a predecessor paid for carry
    /// into the replacement worker.
    excluded_sources: SharedExclusions,
}

impl SmtcListener {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        output: SyncSender<Arc<MediaEvent>>,
        in_flight_art: Arc<AtomicU64>,
        budget_warned: Arc<AtomicBool>,
        seed: ListenerSeed,
        heartbeat: Arc<Mutex<Instant>>,
        live_generation: Arc<AtomicU64>,
        my_generation: u64,
        shutdown: Arc<AtomicBool>,
        now_showing: Arc<Mutex<Option<String>>>,
        excluded_sources: SharedExclusions,
        control_tx: SyncSender<Signal>,
        control_rx: Arc<Mutex<mpsc::Receiver<Signal>>>,
        control_mailbox: Arc<Mutex<ControlMailbox>>,
    ) -> Self {
        Self {
            output,
            in_flight_art,
            budget_warned,
            seed,
            control_tx,
            control_rx,
            control_mailbox,
            heartbeat,
            live_generation,
            my_generation,
            shutdown,
            now_showing,
            excluded_sources,
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
        // Manager creation is process-local and cannot be blamed on any
        // source's sessions, so it keeps the unbounded wait (a hang here is
        // a startup failure the supervisor's restart budget covers).
        let manager = wait_async(&GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?, None)
            .context("requesting the SMTC session manager")?;
        let signal_tx = self.control_tx.clone();
        let sessions_token = register_sessions_handler(&manager, signal_tx.clone())?;
        let current_token = register_current_session_handler(&manager, signal_tx.clone())?;
        let mut state = ListenerState::new(
            manager,
            self.seed,
            self.output,
            self.in_flight_art,
            self.budget_warned,
            signal_tx,
            self.heartbeat,
            self.live_generation,
            self.my_generation,
            self.shutdown,
            self.now_showing,
            self.excluded_sources,
            self.control_mailbox,
        );

        state.sync_subscriptions();
        // Initial read: report what is already playing so the pill does not
        // wait for the first event.
        state.poll_sessions();
        state.event_loop(self.control_rx.clone())?;

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
        seed: ListenerSeed,
        output: SyncSender<Arc<MediaEvent>>,
        in_flight_art: Arc<AtomicU64>,
        budget_warned: Arc<AtomicBool>,
        signal_tx: SyncSender<Signal>,
        heartbeat: Arc<Mutex<Instant>>,
        live_generation: Arc<AtomicU64>,
        my_generation: u64,
        shutdown: Arc<AtomicBool>,
        now_showing: Arc<Mutex<Option<String>>>,
        excluded_sources: SharedExclusions,
        control_mailbox: Arc<Mutex<ControlMailbox>>,
    ) -> Self {
        Self {
            manager,
            output,
            in_flight_art,
            budget_warned,
            pending_output: VecDeque::new(),
            signal_tx,
            control_mailbox,
            subscriptions: HashMap::new(),
            states: HashMap::new(),
            dirty: VecDeque::new(),
            dirty_seen: HashSet::new(),
            sessions_pending: false,
            pending_deadline: None,
            last_heap_compact: Instant::now(),
            last_session_check: Instant::now(),
            churn: HashMap::new(),
            excluded_sources,
            rejected_seen: HashSet::new(),
            ignored_seen: HashSet::new(),
            terminal_pending: HashMap::new(),
            last_track_per_source: HashMap::new(),
            last_known_playback_per_source: HashMap::new(),
            now_showing,
            // Seed the allow list once; the settings UI keeps it current
            // with `SetAllowedSources` from here on.
            debounce: debounce_duration_ms(seed.debounce_ms),
            icon_cache: HashMap::new(),
            cached_allowed: Some((
                seed.media_sources.clone(),
                seed.media_sources
                    .iter()
                    .map(|pattern| normalize_for_match(pattern))
                    .collect(),
            )),
            last_emit_at: HashMap::new(),
            palette_per_identity: HashMap::new(),
            last_overflow_warn: None,
            heartbeat,
            live_generation,
            my_generation,
            shutdown,
        }
    }

    fn event_loop(&mut self, signal_rx: Arc<Mutex<Receiver<Signal>>>) -> Result<()> {
        loop {
            // Set by main at exit: leave promptly (within the receive
            // timeout) so run_initialized's cleanup unsubscribes every
            // session and uninitializes COM instead of running until process
            // termination.
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            // A stalled worker is leaked, not joined, so this thread can
            // outlive the restart that superseded it. A superseded worker
            // must not take further turns: draining the control mailbox
            // would consume commands meant for the successor, and each
            // poll is wasted COM work. Exit so run_initialized's cleanup
            // unsubscribes this worker's sessions.
            if !self.is_current_generation() {
                debug!("SMTC worker superseded by a newer generation; exiting");
                break;
            }
            // Control commands apply on this turn, before any receive, so a
            // push whose wake-up hint was dropped by a saturated queue still
            // lands within one loop iteration (the loop wakes at least every
            // `SESSION_CHECK_INTERVAL`).
            self.drain_control()?;
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

            // The receiver is shared across worker restarts (`main` owns it),
            // so a replacement worker drains whatever its predecessor left in
            // the queue. Senders use `control_tx`/`signal_tx`, never this
            // mutex. The guard must end with the receive, before the match
            // arms run: a WinRT call hanging inside an arm (the
            // supervisor-restart scenario) would otherwise pin the receiver
            // mutex, and the replacement worker would block on the lock
            // before ever reaching its first receive — its heartbeat would
            // go stale and the supervisor would burn the global restart
            // budget.
            let received = signal_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv_timeout(timeout);
            match received {
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
            Signal::ControlWake => {
                // The mailbox itself is drained at the top of the next loop
                // turn; this arm only consumes the wake-up hint, which exists
                // so a control push is applied promptly while the loop is
                // blocked in its receive.
            }
        }
        Ok(())
    }

    /// Applies every pending control command from the mailbox (newest value
    /// per kind). Runs at the top of each event-loop turn, so a command
    /// pushed while the signal queue was saturated — or whose wake-up hint
    /// was dropped — still lands within one turn; the mailbox survives
    /// worker restarts, so a push made just before a restart is applied by
    /// the replacement worker.
    fn drain_control(&mut self) -> Result<()> {
        // Verify-take under the mailbox lock: the supervisor bumps the
        // generation under this same lock when it restarts the worker, so a
        // superseded worker cannot drain commands pushed for its successor —
        // they stay in the mailbox. The lock covers the drain, not the apply
        // that follows: a bump landing mid-apply only wastes that worker's
        // own state. SetAllowedSources is recovered when the successor is
        // seeded from the shared config, and the notifications re-show is
        // restored by the overlay itself, so no command is ever lost
        // functionally.
        let commands = {
            let mut mailbox = self
                .control_mailbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !self.is_current_generation() {
                return Ok(());
            }
            mailbox.drain()
        };
        for command in commands {
            self.handle_control(command)?;
        }
        Ok(())
    }

    /// Applies a command pushed by the main window. Runs on the next
    /// event-loop turn; commands are settings clicks, so a turn of latency —
    /// the same granularity the deleted per-turn config poll had — is
    /// acceptable.
    fn handle_control(&mut self, command: ControlCommand) -> Result<()> {
        match command {
            ControlCommand::SetNotificationsEnabled(true) => {
                // The overlay flips first (`TOGGLE_MSG` is posted before
                // `mutate_config` persists the write), so the re-emit always
                // lands on an enabled overlay; the overlay's own `last_track`
                // restore covers the narrow case where a queued
                // `MEDIA_EVENT_MSG` is drained and dropped before the toggle
                // flips it. Without this, the worker keeps reading sessions
                // while notifications are off (it only suppresses at the
                // overlay) and has no media event to re-announce an unchanged
                // track, so the pill would wait for the next real action.
                if let Err(error) = self.reshow_current() {
                    warn!("forced re-show on notifications re-enable failed: {error:#}");
                }
            }
            ControlCommand::SetNotificationsEnabled(false) => {
                // The overlay owns suppression while off; the worker keeps
                // reading sessions so the pill restores instantly on
                // re-enable. Nothing to do here.
            }
            ControlCommand::SetAllowedSources(sources) => {
                // Normalize the patterns once at apply time instead of on the
                // hot path. `cached_allowed` stays current from here on: the
                // settings UI is its only writer.
                let normalized = sources.iter().map(|pattern| normalize_for_match(pattern)).collect();
                self.cached_allowed = Some((sources, normalized));
                debug!("media-sources allow list pushed by the settings UI");
            }
        }
        Ok(())
    }

    /// Re-emits the current session's track on a notifications re-enable,
    /// bypassing the diff gate. `refresh_session` only emits on a content diff,
    /// so for an unchanged track there is nothing to re-announce and the pill
    /// would stay hidden until the next media event. This does a fresh,
    /// authoritative read (`read_artwork=true`) so the surfaced track is never
    /// a stale cache entry: `store_last_track` can evict artwork from the cache
    /// past the 64 MB budget, and `retry_artwork` never recovers an evicted
    /// cover (it gates on the never-cleared `LogicalState.has_artwork`), so a
    /// cache lookup would render an artless placeholder instead of the cover.
    fn reshow_current(&mut self) -> Result<()> {
        if !self.is_current_generation() {
            return Ok(());
        }
        let Some(session) = self.manager.GetCurrentSession().ok() else {
            // Re-enable with no current SMTC session means the source the pill
            // is showing stopped or its app quit while notifications were off:
            // no event reached the disabled overlay to settle a terminal
            // Stopped, so `last_track` restored by the fast-path is stale.
            // Emit one for the shown source so it swaps the stale track pill for
            // a correct Stopped pill and dismisses it, rather than lingering.
            // The fast-path restores `last_track` first (TOGGLE_MSG lands before
            // the config write the worker reads), so this corrects it from below.
            // The emit is gated on the source's disappearance being pending in
            // this worker, so a transient `GetCurrentSession` failure while the
            // source is still alive (not pending) never kills a live pill here.
            self.emit_terminal_stopped_if_shown_unsettled();
            return Ok(());
        };
        if !self.session_source_allowed(&session) || !self.should_follow_session(&session) {
            let label = read_source_app(&session);
            debug!("reshow skipped | reason=not-followed | source={label}");
            return Ok(());
        }
        let playback_info = session.GetPlaybackInfo()?;
        let status = playback_info.PlaybackStatus()?;
        if status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Closed {
            return Ok(());
        }
        let playback = snapshot_playback_state(status);
        let playback_type = session_playback_type(&session);
        if playback_type == PlaybackType::Image {
            return Ok(());
        }
        let mut merged = self.read_track_or_exclude_wedged(
            &session,
            true,
            playback,
            playback_info.PlaybackRate().ok().and_then(|r| r.Value().ok()),
            playback_type,
        )?;
        if is_placeholder_like(&merged) {
            debug!("reshow skipped | reason=placeholder | source={}", merged.source_app);
            return Ok(());
        }
        // Inject the cached cover if the live thumbnail stream is still empty
        // (SMTC populates art ~500ms after the title). The identity matches the
        // last-emitted track, so this is the same cover, not a cross-track leak.
        if merged.artwork.is_none()
            && let Some(cached) = self.cached_artwork_for(&merged.source_app, &merged.title, &merged.artist)
        {
            merged.artwork = Some(cached);
        }
        let mut emitted = with_decoded_art(merged.clone(), crate::events::ARTWORK_DECODE as usize);
        emitted.palette = self.palette_for_identity(&merged, emitted.decoded_art.as_deref());
        // `read_track_info` carried the live playback snapshot above onto the
        // track, so the pill does not infer a state from a lagging cache.
        let label = track_label(&emitted);
        info!("notifications re-enabled; track changed | {label}");
        // Keep the caches current so the next diff-gated read does not re-emit
        // the same track and so a recovered/evicted artwork is retained for
        // later session-recreation dedup.
        store_last_track(
            &mut self.last_track_per_source,
            merged.source_app.clone(),
            merged.clone(),
            MAX_RETAINED_ARTWORK_BYTES,
        );
        self.last_emit_at.insert(merged.source_app.clone(), Instant::now());
        self.emit(MediaEvent::TrackChanged(emitted));
        Ok(())
    }

    /// Emits a terminal `Stopped` for the source the overlay's pill is
    /// currently displaying, but only when that source's disappearance is
    /// pending here: a subscribed session vanished and the settle grace has
    /// not yet decided it is really gone (see `terminal_pending`). Pending
    /// membership is snapshot-verified absence that survives until the settle
    /// prunes it — unlike the per-source caches, which the settle already
    /// evicted by the time the restored `last_track` can be stale — so the
    /// gate is true exactly when the fast-path pill can no longer be
    /// corrected from worker state, and false for a source still alive (a
    /// transient `GetCurrentSession` failure must not kill a live pill).
    /// A churning source stays silent while on the cool-down. Called from
    /// `reshow_current` on a notifications re-enable with no current SMTC
    /// session.
    fn emit_terminal_stopped_if_shown_unsettled(&mut self) {
        let Some(source) = self.shown_source() else {
            return;
        };
        if reshow_terminal_stopped_warranted(
            self.terminal_pending.contains_key(&source),
            self.source_on_cooldown(&source),
        ) {
            // Mark the source Stopped so the next `sync_subscriptions` settle
            // does not re-emit a terminal Stopped for the same disappearance: its
            // gate is `terminal_stopped_warranted`, which is false for an
            // already-announced Stopped. Mirrors `refresh_session`, which
            // records the last-known state ahead of its emit.
            self.last_known_playback_per_source
                .insert(source.clone(), PlaybackState::Stopped);
            info!("notifications re-enabled; stopped | source={source}");
            self.emit(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, source));
        }
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
            match self.read_track_or_exclude_wedged(
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
        // genuinely new sessions truncated to the session cap. A hostile
        // storm of new sessions therefore cannot evict existing subscriptions
        // when the caps are applied: overflow is rejected, and a brand-new
        // *current* session displaces the weakest survivor(s) instead of
        // being starved (`displace_survivors`) — the caps still bound the
        // total tracked set. The ordering is computed by the pure
        // `prioritize_sessions` over lightweight (key, source) pairs, shared
        // with the tests so the priority contract is pinned by both; the
        // allow-list and current-session filters are applied within the loop
        // below. The enumeration itself is bounded to the sessions the caps
        // can ever admit — the current session, every surviving
        // subscription, and at most `MAX_TRACKED_SESSIONS` genuinely new
        // candidates — so a hostile snapshot listing thousands of sessions
        // cannot make this pass pay a WinRT source read per entry: the
        // per-sync work stops at the same line the caps draw for everything
        // downstream.
        let mut new_candidates = 0usize;
        let mut dropped_by_bound = 0usize;
        let mut snapshot_keys: Vec<(usize, String)> = Vec::new();
        for session in &sessions {
            let key = session_key(session);
            let is_current = Some(key) == current_key;
            if !is_current && !before.contains(&key) {
                if new_candidates >= MAX_TRACKED_SESSIONS {
                    dropped_by_bound += 1;
                    continue;
                }
                new_candidates += 1;
            }
            snapshot_keys.push((key, read_source_app(session)));
        }
        if dropped_by_bound > 0 {
            debug!("SMTC snapshot enumeration bounded | dropped={dropped_by_bound} entries beyond the admission caps");
        }
        let ordered = prioritize_sessions(
            &snapshot_keys,
            current_key.zip(current_source.clone()),
            &before,
            MAX_TRACKED_SESSIONS,
        );
        let by_key: HashMap<usize, &GlobalSystemMediaTransportControlsSession> =
            sessions.iter().map(|session| (session_key(session), session)).collect();
        let prioritized: Vec<(GlobalSystemMediaTransportControlsSession, usize, String)> = ordered
            .into_iter()
            .filter_map(|(key, source)| by_key.get(&key).map(|s| ((*s).clone(), key, source)))
            .collect();
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
        // Per-sync admission counters feeding one aggregate debug line after
        // the loop, so a hostile session storm cannot erode the live log with
        // one line per candidate per sync.
        let mut rejected_overflow: usize = 0;
        let mut rejected_allow: usize = 0;
        let mut ignored: usize = 0;
        let mut accepted: usize = 0;
        for (session, key, source) in &prioritized {
            let allowed = self.session_source_allowed(session);
            if !allowed {
                // Rejected sessions are reported once per appearance (the
                // history shows every media source, not just the tracked
                // ones); the per-session debug line rides the same gate, so
                // a storm of rejected sessions logs each key once instead of
                // on every 2-second re-sync.
                if note_appearance(&mut self.rejected_seen, *key) {
                    debug!("SMTC session rejected | key={key} | source={source}");
                    let (title, artist) = self.rejected_row_text(session, source);
                    let state = read_session_state(session);
                    self.emit(MediaEvent::SessionRejected {
                        source_app: source.clone(),
                        title,
                        artist,
                        state,
                        accepted: false,
                    });
                }
                rejected_allow += 1;
                // A session that became disallowed (allow-list edit) or whose
                // source tripped the churn cool-down must not keep its event
                // subscriptions: it would otherwise keep firing signals that
                // every path discards.
                self.evict(*key);
                continue;
            }
            if !session_matches_current_source(*key, source, current_key, current_source.as_deref()) {
                // Same once-per-appearance gate: an allowed-but-not-current
                // session (e.g. a second tab of a browser) is uninteresting
                // in volume, and under a storm every new candidate would hit
                // this line per sync.
                if note_appearance(&mut self.ignored_seen, *key) {
                    debug!("SMTC session ignored | reason=not-current-session | key={key} | source={source}");
                }
                ignored += 1;
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
                        continue;
                    }
                    debug!(
                        "SMTC current session admitted via survivor displacement | displaced={displaced} | key={key} | source={source}"
                    );
                } else {
                    rejected_overflow += 1;
                    continue;
                }
            }
            // Admitted sessions keep a trimmed per-session detail line (no
            // allow-list dump): at normal volumes this is the "which session
            // is this event for" trail. Placed after the caps so a saturated
            // sync never relabels rejected storm candidates as accepted; the
            // aggregate below bounds the rejected-side volume.
            accepted += 1;
            debug!("SMTC session accepted | key={key} | source={source}");
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
        // One aggregate debug line per sync when any session was not fully
        // accepted, so the storm cost in the log is O(1) lines per sync (the
        // per-session detail lines are gated once-per-appearance above). The
        // throttled WARN below remains the operator-facing overflow signal.
        if rejected_allow + ignored + rejected_overflow > 0 {
            let allow_list_len = self.cached_allowed.as_ref().map_or(0, |(raw, _)| raw.len());
            debug!(
                "SMTC admission: {accepted} accepted, {rejected_allow} rejected (allow-list/cooldown), {ignored} ignored (not current), {rejected_overflow} cap-rejected | allow_list_len={allow_list_len}"
            );
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
        // Derived from the bounded snapshot pairs instead of a second
        // full-snapshot read pass: a source whose every session fell beyond
        // the enumeration bound is treated as departed — under a hostile
        // storm that is exactly the caps' intent, and under normal volumes
        // the bound never drops anything.
        let alive_sources: HashSet<String> = snapshot_keys.iter().map(|(_, source)| source.clone()).collect();
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
                self.emit(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, source.clone()));
            }
            // A settled source is gone for good: emit the overlay hygiene
            // event unconditionally, even when no terminal Stopped was
            // warranted (the state was already announced, or the source never
            // reported one). Either way the overlay's fast-path standby
            // (`last_track`/`held_content`) still holds this source's last
            // track — it is stale now and must not resurrect on a later
            // notifications re-enable.
            self.emit(MediaEvent::SourceGone { source_app: source });
        }
        // Forget rejected/ignored sessions that vanished so a later
        // reappearance is reported again.
        self.rejected_seen.retain(|key| alive.contains(key));
        self.ignored_seen.retain(|key| alive.contains(key));
        self.excluded_sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, until| *until > Instant::now());
        // Keep the picker's candidate list in sync with what is actually
        // open, including apps whose sessions were rejected: checking them
        // is how the user adds them to the allow-list. Dedup and cap it
        // separately so a hostile session storm cannot grow the picker list
        // without bound.
        let active_sources: Vec<String> = dedup_capped(
            // Truncate the enumeration to the first MAX_TRACKED_SESSIONS
            // sessions: the source cap bounds the distinct sources the
            // picker can list anyway, and bounding the WinRT reads ahead of
            // it keeps a hostile session storm from paying for the whole
            // snapshot.
            sessions
                .iter()
                .take(MAX_TRACKED_SESSIONS)
                .map(read_source_app)
                .collect(),
            MAX_TRACKED_SOURCES,
        );
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
                let status = match playback_info.PlaybackStatus() {
                    Ok(status) => status,
                    Err(error) => {
                        // Count a failed status read against the retry budget
                        // too, exactly like the failed prefetch below: a
                        // session whose reads keep failing must not be
                        // retried forever.
                        if let Some(state) = self.states.get_mut(&key) {
                            state.artwork_attempts += 1;
                        }
                        return Err(error.into());
                    }
                };
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
        let read =
            match self.read_track_or_exclude_wedged(session, true, playback, rate, session_playback_type(session)) {
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
        let mut merged = merge_track(&prev, &read, true);
        // The stale-thumbnail guard from the refresh path applies here too:
        // a retry read can still pair the NEW identity with the PREVIOUS
        // track's bytes (SMTC updates the thumbnail stream after the text
        // fields). The refresh that deferred the emit already recorded the
        // new title, so `emit_track` sees no content change against it — the
        // stale pairing would otherwise surface as an "artwork gain" with
        // the wrong cover. Drop the bytes, refresh the deferral, and let the
        // next retry (or the ARTWORK_TIMEOUT force once the budget is spent)
        // deliver the real cover.
        let stale_dropped = stale_thumbnail(&merged, self.last_track_per_source.get(&merged.source_app));
        if stale_dropped {
            merged.artwork = None;
            if let Some(state) = self.states.get_mut(&key) {
                state.deferred_at = Some(Instant::now());
                state.deferred_for_stale_art = true;
            }
            let label = track_label(&merged);
            debug!("track emit deferred | reason=stale-art-drop (retry) | {label}");
        }
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

    /// Reads a session's track state, applying the wedged-source exclusion
    /// when the read timed out (see `is_wait_timeout`): a source whose own
    /// async operation never completes must be excluded from tracking — not
    /// retried, and certainly not allowed to hang the worker into a
    /// supervisor stall that burns the global restart budget (G4). All three
    /// read paths (event-driven refresh, poll retry, re-enable reshow) route
    /// through this so the exclusion is applied exactly once per timeout.
    fn read_track_or_exclude_wedged(
        &mut self,
        session: &GlobalSystemMediaTransportControlsSession,
        read_artwork: bool,
        playback_state: Option<PlaybackState>,
        playback_rate: Option<f64>,
        playback_type: PlaybackType,
    ) -> Result<TrackInfo> {
        match read_track_info(session, read_artwork, playback_state, playback_rate, playback_type) {
            Ok(info) => Ok(info),
            Err(error) => {
                if is_wait_timeout(&error) {
                    self.exclude_wedged_source(session);
                }
                Err(error)
            }
        }
    }

    /// Excludes a source whose session hung an async read past
    /// `READ_ASYNC_TIMEOUT` (threat-model gap G4): the source is put on the
    /// same tracking exclusion the churn cool-down uses, so its sessions are
    /// not read, subscribed, or emitted for the cool-down period, and the
    /// next `sync_subscriptions` evicts them. One hostile app can therefore
    /// cost at most a single 10 s hiccup plus its own exclusion — never a
    /// worker stall, so it can never burn the global restart budget for
    /// every source.
    fn exclude_wedged_source(&mut self, session: &GlobalSystemMediaTransportControlsSession) {
        let source = read_source_app(session);
        self.excluded_sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                source.clone(),
                Instant::now() + Duration::from_millis(CHURN_COOLDOWN_MS),
            );
        warn!(
            "source {source} did not answer an SMTC read within {READ_ASYNC_TIMEOUT:?}; excluding it from tracking for {CHURN_COOLDOWN_MS}ms"
        );
    }

    /// Title/artist for a rejected session's history row. Never pays a
    /// metadata read for an already-excluded source (churn cool-down or a
    /// previous wedged read), and routes a timed-out read through the
    /// wedged-read exclusion exactly like the tracked paths: this was the
    /// one read path that swallowed its `AsyncReadTimeout` marker, so a
    /// hostile source minting fresh session keys could cost one 10 s wedge
    /// per key and burn the global restart budget into a permanent
    /// `WorkerFailed`.
    fn rejected_row_text(
        &mut self,
        session: &GlobalSystemMediaTransportControlsSession,
        source: &str,
    ) -> (String, String) {
        if self.source_on_cooldown(source) {
            return (source.to_string(), String::new());
        }
        match read_session_text(session, source) {
            Ok(pair) => pair,
            Err(error) => {
                if is_wait_timeout(&error) {
                    self.exclude_wedged_source(session);
                } else {
                    debug!("rejected-session metadata unreadable | source={source} | error={error:#}");
                }
                (source.to_string(), String::new())
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
        // The allow list is seeded at worker startup and replaced by
        // `SetAllowedSources` pushes from the settings UI, so the cached copy
        // is always current; no live config read on this hot path.
        let Some((_, normalized)) = &self.cached_allowed else {
            return true;
        };
        if normalized.is_empty() {
            return true;
        }
        let naumid = normalize_for_match(&aumid);
        let nlabel = normalize_for_match(&label);
        // Empty normalized patterns are skipped so a hand-edited "" cannot
        // match via the empty-substring rule (see `pattern_matches`).
        normalized
            .iter()
            .any(|np| !np.is_empty() && (naumid.contains(np) || nlabel.contains(np)))
    }

    /// True while a source app is excluded from tracking (churn cool-down or
    /// wedged-read exclusion).
    fn source_on_cooldown(&self, source: &str) -> bool {
        self.excluded_sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            self.excluded_sources
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            contained_winrt_event("the media-properties handler", || {
                if let Err(e) = properties_tx.try_send(Signal::MediaProperties(properties_session.clone())) {
                    debug!("signal dropped | kind=MediaProperties | {e:?}");
                }
            })
        });
        let playback_handler: TypedEventHandler<
            GlobalSystemMediaTransportControlsSession,
            PlaybackInfoChangedEventArgs,
        > = TypedEventHandler::new(move |_, _| {
            contained_winrt_event("the playback-info handler", || {
                if let Err(e) = playback_tx.try_send(Signal::PlaybackInfo(playback_session.clone())) {
                    debug!("signal dropped | kind=PlaybackInfo | {e:?}");
                }
            })
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
            contained_winrt_event("the timeline handler", || {
                if let Err(e) = timeline_tx.try_send(Signal::Timeline(timeline_session.clone())) {
                    debug!("signal dropped | kind=Timeline | {e:?}");
                }
            })
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
        let deadline = Instant::now() + self.debounce;
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
    ///
    /// The in-flight artwork byte budget is applied here, before anything is
    /// queued: a hostile source emitting byte-distinct covers back-to-back
    /// while the forwarder is wedged must not be able to hold ~5 GiB of
    /// artwork in the channel + mailbox (count caps alone allow 1280 ×
    /// `MAX_THUMBNAIL_BYTES`). When the budget would be exceeded, the
    /// payload is dropped — metadata kept, pill renders a placeholder — and
    /// the bytes are never counted.
    fn emit(&mut self, event: MediaEvent) {
        if !self.is_current_generation() {
            return;
        }
        let mut event = event;
        let in_flight = self.in_flight_art.load(Ordering::Relaxed);
        let had_art = matches!(&event, MediaEvent::TrackChanged(track) if track.artwork.is_some());
        let bytes = budget_artwork(&mut event, in_flight, MAX_IN_FLIGHT_ARTWORK_BYTES);
        if had_art && matches!(&event, MediaEvent::TrackChanged(track) if track.artwork.is_none()) {
            // Stripped: the bytes were not counted, so nothing is added. The
            // event is still queued — the metadata (and thus the pill) is the
            // authoritative state; only the cover is missing.
            if let MediaEvent::TrackChanged(track) = &event {
                let label = track_label(track);
                debug!(
                    "artwork dropped from queued event | reason=in-flight-byte-budget | \
                     in_flight={in_flight} | {label}"
                );
            }
            // One-shot user-facing warning: the budget tripped because the
            // UI is not keeping up. The tray note fires once per app run (the
            // latch is shared across worker restarts), not on every dropped
            // cover. The warning travels through the normal event path, so it
            // is delivered (via the retry mailbox) as soon as the forwarder
            // drains.
            if !self.budget_warned.swap(true, Ordering::Relaxed) {
                self.emit(MediaEvent::ArtworkBudgetExceeded);
            }
        } else {
            self.in_flight_art.fetch_add(bytes, Ordering::Relaxed);
        }
        match self.output.try_send(Arc::new(event)) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(returned)) => {
                let dropped = coalesce_pending_event(
                    &mut self.pending_output,
                    returned,
                    OUTPUT_RETRY_CAP,
                    &self.in_flight_art,
                    &self.budget_warned,
                );
                if dropped > 0 && self.may_warn_overflow() {
                    warn!(
                        "SMTC output retry mailbox overflowed: {dropped} queued event(s) dropped \
                         (UI is not keeping up)"
                    );
                    self.last_overflow_warn = Some(Instant::now());
                }
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.clear_pending_output();
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
            self.clear_pending_output();
        }
    }

    /// Drops every event still in the retry mailbox and frees its in-flight
    /// artwork accounting. Called when the output channel disconnected (the
    /// forwarder is gone, so nothing queued can ever be delivered) and from
    /// `Drop`, so a worker ending with a non-empty mailbox returns its bytes
    /// to the shared counter instead of starving the next worker's budget.
    ///
    /// If the discarded mailbox holds the one-shot budget warning, the
    /// `budget_warned` latch is reset: the warning was emitted but never
    /// delivered, and the latch is shared across worker restarts — leaving
    /// it set would permanently lose the "the UI is not keeping up" tray
    /// note for the rest of the app run even though the condition is real.
    /// The note is one per *delivery*, not one per worker: resetting here
    /// lets the replacement worker re-warn on the next budget strip. (A hard
    /// stall — where the worker thread is leaked mid-call and `drop` never
    /// runs — cannot run this at all; the supervisor covers that case by
    /// resetting the latch on every stall, see the `WorkerExit::Stalled`
    /// branch in main.rs.)
    fn clear_pending_output(&mut self) {
        if mailbox_holds_budget_warning(&self.pending_output) {
            self.budget_warned.store(false, Ordering::Relaxed);
            debug!("budget-warning latch reset | reason=undelivered-warning-discarded");
        }
        release_pending_bytes(&mut self.pending_output, &self.in_flight_art);
    }

    fn is_current_generation(&self) -> bool {
        self.live_generation.load(Ordering::SeqCst) == self.my_generation
    }
}

impl Drop for ListenerState {
    fn drop(&mut self) {
        // Return the artwork accounting of whatever is still in the retry
        // mailbox to the shared counter when this worker ends (clean exit,
        // shutdown join, or panic unwind). The counter survives into the
        // replacement worker, and every mailbox event's bytes were counted
        // at emit time and only freed when the event left the mailbox — so
        // without this, a worker that dies with a non-empty mailbox (a full
        // output channel at exit) would leave its bytes counted forever and
        // the next worker would strip every cover, permanently, until the
        // app restarts. The one path this cannot cover is a hard stall,
        // where the worker thread is leaked mid-call and `drop` never runs:
        // those bytes stay counted (and their memory stays live), bounded by
        // the mailbox cap and documented as a residual in the threat model.
        // The *warning* half of that leak is covered from the other side:
        // the supervisor resets `budget_warned` on every stall (see the
        // `WorkerExit::Stalled` branch in main.rs), so a warning stranded in
        // the leaked mailbox cannot lose the note for the rest of the run.
        self.clear_pending_output();
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
        MediaEvent::SourceGone { source_app } => ("gone", Some(source_app.as_str())),
        MediaEvent::ProgressChanged { source_app, .. } => ("progress", Some(source_app.as_str())),
        MediaEvent::WorkerFailed { .. } => ("worker-failed", None),
        MediaEvent::ArtworkBudgetExceeded => ("art-budget", None),
    }
}

/// Applies the in-flight artwork byte budget to an event about to be queued:
/// when adding its artwork would exceed the budget, the payload is dropped —
/// raw cover, decode and derived palette stripped — while the metadata is
/// kept, so the pill renders a placeholder instead of pinning megabytes
/// behind a queued event. Returns the artwork bytes the caller must add to
/// the shared in-flight counter: the event's full artwork when it fits
/// within the budget, or 0 when it was stripped (nothing was queued). Pure,
/// so the budget decision is unit-testable without a live session manager.
fn budget_artwork(event: &mut MediaEvent, in_flight: u64, budget: u64) -> u64 {
    let bytes = artwork_bytes(event);
    if bytes > 0 && in_flight.saturating_add(bytes) > budget {
        if let MediaEvent::TrackChanged(track) = event {
            track.artwork = None;
            track.decoded_art = None;
            track.palette = None;
        }
        0
    } else {
        bytes
    }
}

/// Inserts an event into the bounded retry mailbox. An older event with the
/// same coalesce key is superseded in place — the newest authoritative state
/// wins — while events for different sources/kinds keep their arrival order.
/// On over-cap the oldest queued event is dropped, never the newest; returns
/// how many were dropped so the caller can report the overflow. Every event
/// that leaves the mailbox (superseded or over-cap) had its artwork bytes
/// counted when it was queued, so those bytes are freed from `in_flight`
/// here — the counter tracks distinct live allocations, not queue slots.
///
/// The one-shot budget warning cannot leave the mailbox by supersession (a
/// queued warning implies its latch is set, so no newer same-key warning can
/// ever arrive), so its only discard path is the over-cap pop: when that pop
/// drops it undelivered, `budget_warned` is reset so the next strip can
/// re-warn — the same rule `clear_pending_output` applies to a mailbox
/// cleared at worker teardown, so the warning is one per *delivery*, never
/// silently lost for the rest of the app run.
fn coalesce_pending_event(
    queue: &mut VecDeque<Arc<MediaEvent>>,
    event: Arc<MediaEvent>,
    cap: usize,
    in_flight: &AtomicU64,
    budget_warned: &AtomicBool,
) -> usize {
    let key = event_coalesce_key(&event);
    if let Some(index) = queue.iter().position(|queued| event_coalesce_key(queued) == key)
        && let Some(superseded) = queue.remove(index)
    {
        in_flight.fetch_sub(artwork_bytes(&superseded), Ordering::Relaxed);
    }
    queue.push_back(event);
    let mut dropped = 0;
    while queue.len() > cap {
        if let Some(oldest) = queue.pop_front() {
            in_flight.fetch_sub(artwork_bytes(&oldest), Ordering::Relaxed);
            if matches!(oldest.as_ref(), MediaEvent::ArtworkBudgetExceeded) {
                // The queued one-shot budget warning was discarded undelivered
                // by the over-cap pop: reset its latch so a later strip
                // re-warns, instead of the note being lost for the rest of
                // the app run (mirrors the mailbox-clear reset).
                budget_warned.store(false, Ordering::Relaxed);
                debug!("budget-warning latch reset | reason=over-cap-pop-discarded-warning");
            }
            dropped += 1;
        }
    }
    dropped
}

/// Whether the retry mailbox holds the one-shot budget warning, which is
/// about to be discarded undelivered (see `clear_pending_output`). Pure, so
/// the reset decision is unit-testable without a live session manager.
fn mailbox_holds_budget_warning(queue: &VecDeque<Arc<MediaEvent>>) -> bool {
    queue
        .iter()
        .any(|event| matches!(event.as_ref(), MediaEvent::ArtworkBudgetExceeded))
}

/// Drops every event in a mailbox and frees its in-flight artwork accounting
/// (the inverse of the queueing-time `fetch_add` in `emit`). Used when the
/// output channel disconnects and nothing queued can ever be delivered.
fn release_pending_bytes(queue: &mut VecDeque<Arc<MediaEvent>>, in_flight: &AtomicU64) {
    while let Some(event) = queue.pop_front() {
        in_flight.fetch_sub(artwork_bytes(&event), Ordering::Relaxed);
    }
}

/// Drains a retry mailbox into the output channel, oldest first. Stops at the
/// first full send so ordering is preserved; returns true when the channel
/// disconnected so the caller can clear the mailbox.
fn drain_pending_to_channel(queue: &mut VecDeque<Arc<MediaEvent>>, output: &SyncSender<Arc<MediaEvent>>) -> bool {
    while let Some(event) = queue.pop_front() {
        match output.try_send(event) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(event)) => {
                // The event did not fit: put it back at the front so the
                // next drain pass retries it oldest-first.
                queue.push_front(event);
                return false;
            }
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

/// Records `key` in a once-per-appearance reporting set (`rejected_seen`,
/// `ignored_seen`), evicting an arbitrary key first when the set is at
/// `MAX_REPORTED_SESSIONS` so a hostile storm of ever-new session keys cannot
/// grow the dedup sets without bound. Returns whether this was a new
/// appearance (the caller reports the session once).
fn note_appearance(seen: &mut HashSet<usize>, key: usize) -> bool {
    if seen.len() >= MAX_REPORTED_SESSIONS
        && let Some(victim) = seen.iter().next().copied()
    {
        seen.remove(&victim);
    }
    seen.insert(key)
}

/// Priority-ordered candidate list for the admission caps, computed over
/// lightweight (key, source) pairs so the live sync loop and the tests share
/// one ordering contract: current session first, then surviving existing
/// subscriptions (snapshot order), then genuinely new sessions truncated to
/// `session_cap` (an overflow candidate can never be admitted this sync, so
/// enumerating beyond the cap is wasted storm-time work). The current session
/// is included even when absent from the snapshot (browser churn makes
/// `GetCurrentSession` authoritative over a stale `GetSessions` list); the
/// loops below skip it by key, so it is never duplicated.
fn prioritize_sessions(
    snapshot: &[(usize, String)],
    current: Option<(usize, String)>,
    before: &HashSet<usize>,
    session_cap: usize,
) -> Vec<(usize, String)> {
    let mut ordered = Vec::with_capacity(snapshot.len() + usize::from(current.is_some()));
    if let Some((cur_key, cur_source)) = current.as_ref() {
        ordered.push((*cur_key, cur_source.clone()));
    }
    for (key, source) in snapshot {
        if Some(*key) == current.as_ref().map(|(k, _)| *k) {
            continue;
        }
        if before.contains(key) {
            ordered.push((*key, source.clone()));
        }
    }
    let mut new_candidates = 0usize;
    for (key, source) in snapshot {
        if Some(*key) == current.as_ref().map(|(k, _)| *k) || before.contains(key) {
            continue;
        }
        if new_candidates >= session_cap {
            break;
        }
        new_candidates += 1;
        ordered.push((*key, source.clone()));
    }
    ordered
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
/// make the cap decisions directly testable. Callers build `ordered` through
/// the shared `prioritize_sessions` — the same current-first /
/// existing-before-new ordering the live loop uses — so the priority contract
/// is pinned by both. The model assumes every admitted session subscribes
/// successfully; in the live loop a session can still fail to subscribe (cap
/// race with the event-driven path, or a WinRT error) without the caps
/// reconsidering it here, and a brand-new *current* session displaces
/// survivors instead of being rejected (see `displace_survivors`) — neither
/// is modeled.
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
    let mut seen: HashSet<String> = HashSet::with_capacity(sources.len().min(cap));
    let mut out: Vec<String> = Vec::with_capacity(sources.len().min(cap));
    for s in sources.drain(..) {
        // Once the cap is full no later source can be pushed: stop hashing
        // into the seen-set (unobservable beyond this point) to bound
        // storm-time work.
        if out.len() == cap {
            break;
        }
        if seen.insert(s.clone()) {
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

/// Runs a WinRT event body with panics contained: the handler
/// answers with an error result instead of unwinding across the WinRT ABI.
fn contained_winrt_event(context: &str, body: impl FnOnce()) -> windows::core::Result<()> {
    crate::winutil::catch_callback_panic(context, body)
        .map_err(|_| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))
}

#[cfg(test)]
mod winrt_containment_tests {
    use super::contained_winrt_event;

    #[test]
    fn a_panicking_winrt_body_yields_an_error_result() {
        assert!(contained_winrt_event("test handler", || ()).is_ok());
        let error = contained_winrt_event("test handler", || panic!("injected"))
            .expect_err("the panic must surface as an error result");
        assert_eq!(error.code(), windows::Win32::Foundation::E_FAIL);
    }
}

fn register_sessions_handler(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
    signal_tx: SyncSender<Signal>,
) -> Result<i64> {
    let handler: TypedEventHandler<GlobalSystemMediaTransportControlsSessionManager, SessionsChangedEventArgs> =
        TypedEventHandler::new(move |_, _| {
            contained_winrt_event("the sessions handler", || {
                if let Err(e) = signal_tx.try_send(Signal::Sessions) {
                    debug!("signal dropped | kind=Sessions | {e:?}");
                }
            })
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
) -> Result<i64> {
    let handler: TypedEventHandler<GlobalSystemMediaTransportControlsSessionManager, CurrentSessionChangedEventArgs> =
        TypedEventHandler::new(move |_, _| {
            contained_winrt_event("the sessions handler", || {
                if let Err(e) = signal_tx.try_send(Signal::Sessions) {
                    debug!("signal dropped | kind=Sessions | {e:?}");
                }
            })
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
        let (preview, omitted) = crate::winutil::log_preview(trimmed, MAX_PREVIEW_CHARS);
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
    // Filtering can expose boundary whitespace that an unsafe character had
    // shielded from the first trim (`"Song\u{0} "` -> filter -> `"Song "`);
    // trim again so the returned value is always boundary-clean. Without
    // this, a shielded space would defeat the whitespace normalization that
    // keeps one track from splitting into two duplicate pills.
    let safe = safe.trim();
    if safe.chars().count() > MAX_META_CHARS {
        safe.chars().take(MAX_META_CHARS).collect()
    } else {
        safe.to_string()
    }
}

/// Whether a character must never reach displayed metadata: the C0 control
/// range, DEL plus the C1 range, the Unicode directional
/// formatting/override/isolate command characters (bidi embeddings,
/// overrides, isolates), and the Zl/Zp line/paragraph separators
/// (U+2028/U+2029). Ordinary RTL letters, combining marks, emoji and ZWJ
/// sequences are all preserved — only the directionality *commands* are
/// stripped, so a legitimate RTL title still orders right-to-left by its
/// letters.
///
/// Zl/Zp are stripped on the same basis as the bidi commands: they are
/// display commands, not content — a renderer that honors them forces a
/// visible line break, which a single-line pill, history row or tooltip
/// cannot represent (and some log viewers render as a forged record split).
/// Boundary occurrences would be removed by `trim()` anyway (Zl/Zp are
/// Unicode White_Space), but an interior separator survives trimming and
/// forces a mid-row break, so the strip set is the only gate for it. A
/// legitimate title containing a real line separator is vanishingly rare and
/// the pill would mangle it anyway, so the display-command rationale
/// outweighs preservation here — the choice is pinned by
/// `cap_meta_strips_zl_zp_separators`.
fn display_unsafe(c: char) -> bool {
    let code = c as u32;
    (0x0000..=0x001F).contains(&code)
        || (0x007F..=0x009F).contains(&code)
        || (0x2028..=0x2029).contains(&code)
        || (0x202A..=0x202E).contains(&code)
        || (0x2066..=0x2069).contains(&code)
}

const MAX_META_CHARS: usize = 256;

/// The shared preview cap (see `winutil::log_preview`).
const MAX_PREVIEW_CHARS: usize = 128;

/// Best-effort title/artist for a session's history row. Reads can fail or
/// return empty for freshly-created sessions; the title falls back to the
/// source label so the row always names the app.
fn read_session_text(
    session: &GlobalSystemMediaTransportControlsSession,
    source_app: &str,
) -> Result<(String, String), anyhow::Error> {
    // Bounded: this runs for *rejected* sessions (the history row), and a
    // rejected session's own operation must not be able to hang the worker
    // forever — the supervisor would stall and burn the global restart
    // budget. The timeout is surfaced as an `is_wait_timeout` error rather
    // than swallowed, so the caller routes the source through the
    // wedged-read exclusion exactly like the tracked read paths.
    let operation = session
        .TryGetMediaPropertiesAsync()
        .context("requesting rejected-session properties")?;
    let properties = wait_async(&operation, Some(READ_ASYNC_TIMEOUT)).context("reading rejected-session properties")?;
    let mut title = cap_meta(non_empty(
        properties.Title().map(|v| v.to_string()).unwrap_or_default(),
        source_app,
    ));
    // Same strippable-title fallback as `read_track_info`: an
    // all-controls title must not leave a blank history row.
    if title.is_empty() {
        title = source_app.to_string();
    }
    let artist = cap_meta(non_empty(
        properties.Artist().map(|v| v.to_string()).unwrap_or_default(),
        "",
    ));
    Ok((title, artist))
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
    let properties = wait_async(&session.TryGetMediaPropertiesAsync()?, Some(READ_ASYNC_TIMEOUT))?;
    let mut title = cap_meta(non_empty(properties.Title()?.to_string(), &source_app));
    // A title made solely of strippable characters (controls, bidi commands)
    // passes `non_empty` on its raw form and then sanitizes to "" — which
    // would bypass the placeholder gate (`is_placeholder_like` matches the
    // fallback shape) and emit an empty-title pill. Re-apply the fallback so
    // the invariant "empty title ⇒ equals the source label" survives
    // sanitization.
    if title.is_empty() {
        title = source_app.clone();
    }
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
        // async WinRT calls on one thread); the retry decision tree in
        // read_artwork_with_retry tries once more before giving up, and logs
        // which call failed with its raw HRESULT. A session-gone failure
        // (RPC-unavailable / device-not-ready) cannot succeed on retry; a
        // wedged thumbnail stream (the source's own async operation never
        // completes) is definitive, not transient — both surface/exclude via
        // the extracted helper, which the seam tests drive with the mocked
        // reference stack.
        read_artwork_with_retry(|| read_thumbnail(&properties))?
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
        // Accumulate only while under the display cap: a hostile
        // genre list previously materialized the full join before
        // `cap_meta` truncated it.
        let mut joined = String::new();
        for g in properties.Genres()?.into_iter() {
            if !joined.is_empty() {
                joined.push_str(", ");
            }
            joined.push_str(&g.to_string());
            if joined.len() >= MAX_META_CHARS {
                break;
            }
        }
        let joined = cap_meta(joined);
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

/// Synchronously waits for a WinRT async operation to complete and returns
/// its result — the replacement for the `IAsyncOperation::get()` convenience
/// method that windows 0.62 removes (windows 0.58 still generates `get()`
/// with identical semantics: check `Status`; while it is `Started`, install a
/// completed handler that signals a thread waiter and block until it fires;
/// then return `GetResults`). The SMTC worker runs its WinRT calls
/// synchronously on its own worker thread, so this mirrors the removed
/// generated code exactly. Must only be called from a thread that may block
/// (never the UI thread). Two instantiations cover the operation shapes the
/// worker awaits — `IAsyncOperation<T>` and the artwork path's
/// `IAsyncOperationWithProgress<T, u32>` — both of which lose `get()` in
/// windows 0.62.
///
/// `timeout` bounds the block: `Some(limit)` abandons a never-completing
/// operation after `limit` (returning an `AsyncReadTimeout` error, which the
/// caller turns into a per-source exclusion — see `READ_ASYNC_TIMEOUT`),
/// while `None` preserves the original unbounded `get()` semantics for the
/// one site that cannot be blamed on a source (manager creation at worker
/// startup). The abandoned operation is harmless: its completion handler
/// fires later, the `send` lands in a channel nobody reads, and the handler
/// is dropped with the operation.
macro_rules! wait_async_op {
    ($name:ident, $operation:ty, $handler:ty) => {
        fn $name<TResult>(operation: &$operation, timeout: Option<Duration>) -> Result<TResult>
        where
            TResult: windows::core::RuntimeType + 'static,
        {
            if operation.Status()? == AsyncStatus::Started {
                let (signal_tx, signal_rx) = mpsc::channel::<()>();
                operation.SetCompleted(&<$handler>::new(move |_sender, _args| {
                    let _ = signal_tx.send(());
                    Ok(())
                }))?;
                match timeout {
                    Some(limit) => {
                        wait_outcome(signal_rx.recv_timeout(limit), limit)?;
                    }
                    None => {
                        // Unbounded: block until the operation completes,
                        // exactly like the removed `get()`.
                        let _ = signal_rx.recv();
                    }
                }
            }
            operation.GetResults().map_err(anyhow::Error::from)
        }
    };
}

wait_async_op!(
    wait_async,
    windows_future::IAsyncOperation<TResult>,
    windows_future::AsyncOperationCompletedHandler<TResult>
);
wait_async_op!(
    wait_async_progress,
    windows_future::IAsyncOperationWithProgress<TResult, u32>,
    windows_future::AsyncOperationWithProgressCompletedHandler<TResult, u32>
);

/// Classifies the outcome of the wait for an async operation's completion
/// signal. `Ok(())` (the completion handler fired) and a disconnected
/// channel (the operation completed but its handler was replaced or dropped
/// without signalling) both mean the read may proceed to `GetResults`; a
/// timeout is the wedged-read marker — the operation is never completing,
/// so the caller excludes the source instead of retrying a hung read.
fn wait_outcome(outcome: Result<(), mpsc::RecvTimeoutError>, limit: Duration) -> Result<(), AsyncReadTimeout> {
    match outcome {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => Ok(()),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(AsyncReadTimeout { secs: limit.as_secs() }),
    }
}

/// Marker error for an SMTC async read that did not complete within
/// `READ_ASYNC_TIMEOUT`. Deliberately distinct from the ordinary read
/// failures (which are transient and retried): a timeout means the source's
/// own operation is wedged, so the caller excludes the source instead of
/// retrying a hung read. Carries the wait bound so the WARN explains itself.
#[derive(Debug)]
struct AsyncReadTimeout {
    secs: u64,
}

impl std::fmt::Display for AsyncReadTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SMTC async read did not complete within {}s", self.secs)
    }
}

impl std::error::Error for AsyncReadTimeout {}

/// Whether an error marks a wedged async read (an `AsyncReadTimeout`)
/// rather than a transient failure. Drives the per-source exclusion: only
/// timeouts exclude; an RPC-unavailable session, a failed `Thumbnail()` or
/// a `Size()` rejection all stay retryable. `downcast_ref` searches the
/// whole error structure, so the marker is found even under the contexts
/// `read_thumbnail_from` wraps it in.
fn is_wait_timeout(error: &anyhow::Error) -> bool {
    error.downcast_ref::<AsyncReadTimeout>().is_some()
}

fn read_thumbnail(
    properties: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties,
) -> Result<Option<Vec<u8>>> {
    let reference = properties
        .Thumbnail()
        .map_err(|e| anyhow::Error::new(e).context("Thumbnail failed"))?;
    read_thumbnail_from(&reference, Some(READ_ASYNC_TIMEOUT))
}

/// Reads the artwork with its single retry: attempt 1, and on a transient
/// failure (a `Thumbnail()` fetch error or a Size/read rejection — not a
/// session-gone error, not the wedged-read marker) attempt 2. The wedged
/// marker (`AsyncReadTimeout`) is definitive on either attempt and surfaces
/// so the caller excludes the source instead of retrying a hung read; a
/// session-gone failure cannot succeed on retry and yields None
/// immediately; two transient failures yield None. Parameterized by the
/// attempt closure so the decision tree is headless-testable with the
/// mocked reference stack — production passes `read_thumbnail(&properties)`,
/// which re-fetches the thumbnail reference on every attempt.
fn read_artwork_with_retry(read: impl Fn() -> Result<Option<Vec<u8>>>) -> Result<Option<Vec<u8>>> {
    match read() {
        Ok(artwork) => Ok(artwork),
        Err(first) => {
            debug!("album-art read failed (attempt 1): {first:#}");
            if is_session_gone(&first) {
                Ok(None)
            } else if is_wait_timeout(&first) {
                Err(first)
            } else {
                match read() {
                    Ok(artwork) => Ok(artwork),
                    Err(second) => {
                        debug!("album-art read failed (attempt 2): {second:#}");
                        if is_wait_timeout(&second) {
                            Err(second)
                        } else {
                            Ok(None)
                        }
                    }
                }
            }
        }
    }
}

/// The artwork pipeline from the stream reference onward. Split out of
/// `read_thumbnail` (whose `Thumbnail()` fetch stays untouched; the retry
/// decision tree lives in `read_artwork_with_retry`) and parameterized by
/// the wait bound so the
/// `OpenReadAsync`/`ReadAsync` timeouts are headless-testable with a mocked
/// reference; production calls it with `READ_ASYNC_TIMEOUT`. Both wait
/// errors are wrapped in contexts here; `is_wait_timeout` still recognizes
/// the marker through the wrapping, which the seam test pins.
fn read_thumbnail_from(reference: &IRandomAccessStreamReference, timeout: Option<Duration>) -> Result<Option<Vec<u8>>> {
    let stream = wait_async(
        &reference
            .OpenReadAsync()
            .map_err(|e| anyhow::Error::new(e).context("OpenReadAsync failed"))?,
        timeout,
    )
    .map_err(|e| e.context("OpenReadAsync get failed"))?;
    let size = stream
        .Size()
        .map_err(|e| anyhow::Error::new(e).context("Size failed"))?;
    if !thumbnail_stream_size_acceptable(size) {
        debug!(
            "thumbnail dropped | reason=stream-size | size={size} | floor={THUMBNAIL_MIN_BYTES} | cap={MAX_THUMBNAIL_BYTES}"
        );
        return Ok(None);
    }
    let size = size as u32;
    let buffer = Buffer::Create(size).map_err(|e| anyhow::Error::new(e).context("Buffer::Create failed"))?;
    wait_async_progress(
        &stream
            .ReadAsync(&buffer, size, InputStreamOptions::None)
            .map_err(|e| anyhow::Error::new(e).context("ReadAsync failed"))?,
        timeout,
    )
    .map_err(|e| e.context("ReadAsync get failed"))?;
    let reader =
        DataReader::FromBuffer(&buffer).map_err(|e| anyhow::Error::new(e).context("DataReader::FromBuffer failed"))?;
    let mut data = vec![0u8; size as usize];
    reader
        .ReadBytes(&mut data)
        .map_err(|e| anyhow::Error::new(e).context("ReadBytes failed"))?;
    Ok(Some(data))
}

/// Whether a thumbnail stream's declared size is worth reading. Empty and
/// sub-floor streams are not covers (a compact 64-128 px PNG/JPEG cover can
/// legitimately compress below 1 KiB, so the floor is low); anything above
/// the per-stream byte cap is rejected before buffering so a hostile size
/// cannot drive allocation.
fn thumbnail_stream_size_acceptable(size: u64) -> bool {
    (THUMBNAIL_MIN_BYTES..=MAX_THUMBNAIL_BYTES).contains(&size) && size <= u32::MAX as u64
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

/// Whether one allow-list / auto-compact pattern matches a normalized source
/// identity. A pattern that normalizes to nothing (empty, whitespace or
/// separator characters only) never matches — the same rule the overlay's
/// pin matcher applies — so a hand-edited empty entry cannot silently mean
/// "allow every app" or "compact everywhere" via the empty-substring rule.
/// The hot allow-list path inlines the same guard over its precomputed
/// patterns; keep the two in lockstep.
pub(crate) fn pattern_matches(normalized_identity: &str, pattern: &str) -> bool {
    let normalized_pattern = normalize_for_match(pattern);
    !normalized_pattern.is_empty() && normalized_identity.contains(&normalized_pattern)
}

/// The flush-scheduling debounce window. The worker's value comes from the
/// supervisor's seed, not from a live config, so it is clamped here to the
/// coalescing range (150–250 ms): a mis-set config value can starve (too
/// long) or flood (too short) the pill.
fn debounce_duration_ms(ms: u64) -> Duration {
    Duration::from_millis(ms.clamp(150, 250))
}

/// Whether a source that just lost its last session still owes the overlay a
/// terminal `Stopped`: only sources that last reported Playing or Paused need
/// one (Stopped was already announced, and a source that never reported a
/// state never showed anything), and a source on the churn cool-down must stay
/// silent per the cool-down contract.
fn terminal_stopped_warranted(last_known: Option<PlaybackState>, on_cooldown: bool) -> bool {
    !on_cooldown && matches!(last_known, Some(PlaybackState::Playing | PlaybackState::Paused))
}

/// Whether a notifications re-enable with no current SMTC session should emit a
/// terminal `Stopped` for the source the overlay's pill is currently showing.
/// `reshow_current` surfaces the current session's track via a live read, but only
/// when a current session exists to read from: if the shown source stopped or quit
/// while notifications were off, there is no session to re-read, so without this
/// the stale `last_track` the fast-path restored would linger on the pill. The
/// gate is the source's disappearance being pending (absent from the snapshot,
/// inside the settle grace): that is verified absence that survives the cache
/// evictions the settle runs, so it holds exactly when the restored pill can be
/// stale — and never for a source still alive (a transient `GetCurrentSession`
/// failure), nor for one already settled (its overlay standby was retired by
/// `SourceGone`). A churning source stays silent while on the cool-down; the
/// grace has already kept a mid-recreation source's entry alive, so the same
/// 4 s tolerance that guards the settle also guards this emit.
fn reshow_terminal_stopped_warranted(in_terminal_pending: bool, on_cooldown: bool) -> bool {
    in_terminal_pending && !on_cooldown
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
    fn thumbnail_stream_size_accepts_compact_covers_and_rejects_hostile_sizes() {
        // Compact 64-128 px covers can compress below 1 KiB: the floor is low
        // enough to admit them.
        assert!(thumbnail_stream_size_acceptable(64));
        assert!(thumbnail_stream_size_acceptable(512));
        assert!(thumbnail_stream_size_acceptable(1024));
        assert!(thumbnail_stream_size_acceptable(MAX_THUMBNAIL_BYTES));
        // Empty and sub-floor streams are never covers.
        assert!(!thumbnail_stream_size_acceptable(0));
        assert!(!thumbnail_stream_size_acceptable(63));
        // Above the per-stream cap a hostile declared size cannot drive a
        // Buffer::Create allocation.
        assert!(!thumbnail_stream_size_acceptable(MAX_THUMBNAIL_BYTES + 1));
        // The WinRT buffer is u32-sized; larger streams are rejected too.
        assert!(!thumbnail_stream_size_acceptable(u64::from(u32::MAX) + 1));
    }

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
    fn reshow_terminal_stopped_warrants_only_for_a_pending_absence_off_cooldown() {
        // A source whose subscribed session vanished (absent from the
        // snapshot, inside the settle grace) is settled on re-enable with no
        // current session, retiring the stale fast-path pill. Pending
        // membership is the evidence that survives the settle's cache
        // eviction, so a re-enable well after the settle still retires an
        // already-restored stale pill.
        assert!(reshow_terminal_stopped_warranted(true, false));
        // A source that is not pending owes nothing: it is alive (a transient
        // GetCurrentSession failure must not kill a live pill), or it settled
        // and SourceGone already cleaned the overlay standby.
        assert!(!reshow_terminal_stopped_warranted(false, false));
        // A churning source stays silent while on the cool-down.
        assert!(!reshow_terminal_stopped_warranted(true, true));
        assert!(!reshow_terminal_stopped_warranted(false, true));
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
    fn pattern_matches_rejects_empty_normalized_patterns() {
        // An entry that normalizes to nothing must never match: the empty
        // substring is contained in every identity, so allowing it would
        // make media_sources = [""] mean "allow every app".
        assert!(pattern_matches("youtubemusic", "youtube"));
        assert!(!pattern_matches("youtubemusic", ""));
        assert!(!pattern_matches("youtubemusic", "   "));
        assert!(!pattern_matches("youtubemusic", "-_. "));
        assert!(!pattern_matches("", "youtube"));
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
        let (preview, omitted) = crate::winutil::log_preview("a\u{0}b\nc", MAX_PREVIEW_CHARS);
        assert_eq!(preview, "a\\0b\\nc");
        assert_eq!(omitted, 0);
        // A long value is cut at the preview cap and reports what was left
        // out.
        let (preview, omitted) = crate::winutil::log_preview(&"x".repeat(300), MAX_PREVIEW_CHARS);
        assert_eq!(preview, "x".repeat(128));
        assert_eq!(omitted, 172);
        // The boundary itself: exactly MAX_PREVIEW_CHARS omits nothing.
        let (preview, omitted) = crate::winutil::log_preview(&"y".repeat(128), MAX_PREVIEW_CHARS);
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
    fn cap_meta_strips_zl_zp_separators() {
        // The Zl/Zp decision (threat-model gap G2): U+2028/U+2029 are display
        // commands — a renderer that honors them forces a visible line break,
        // visually splitting a single-line pill row, history line, tooltip,
        // or log line — so they are stripped like the bidi commands, not
        // preserved like ordinary letters. Boundary occurrences would be
        // trimmed away anyway (Zl/Zp are Unicode White_Space), but an
        // *interior* separator survives `trim()` and forces a mid-row break;
        // the strip set is the only gate for it. Interior separators are
        // removed...
        assert_eq!(cap_meta("Song\u{2028}Artist".into()), "SongArtist");
        assert_eq!(cap_meta("Song\u{2029}Artist".into()), "SongArtist");
        // ...and boundary ones too (the strip set covers them regardless of
        // the trim path).
        assert_eq!(cap_meta("\u{2028}Song\u{2029}".into()), "Song");
        assert_eq!(cap_meta("Song\u{2028}\u{2029} ".into()), "Song");
        assert_eq!(cap_meta("Song".into()), "Song");
        // Pin the boundary semantics so the strip-set rationale stays honest:
        // trim() does remove Zl/Zp at the edges (they are White_Space), but
        // never an interior one — which is exactly what the strip set adds.
        assert_eq!("\u{2028}Song\u{2029}".trim(), "Song");
        assert_eq!("Song\u{2028}Artist".trim(), "Song\u{2028}Artist");
        // The strip-set decision itself: these characters are display-unsafe.
        assert!(display_unsafe('\u{2028}'));
        assert!(display_unsafe('\u{2029}'));
        // Ordinary whitespace remains preserved (trimmed only at boundaries).
        assert!(!display_unsafe(' '));
        assert!(!display_unsafe('\u{2009}'));
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
        assert_eq!(debounce_duration_ms(1), Duration::from_millis(150));
        assert_eq!(debounce_duration_ms(1000), Duration::from_millis(250));
        assert_eq!(debounce_duration_ms(200), Duration::from_millis(200));
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

    /// Deterministic xorshift64 for the fuzz sweeps below: the same seed
    /// always yields the same sequence, so a failure reproduces exactly and
    /// the tests never depend on ambient randomness.
    struct FuzzRng(u64);

    impl FuzzRng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn pick(&mut self, alphabet: &[char]) -> char {
            alphabet[(self.next() as usize) % alphabet.len()]
        }
    }

    #[test]
    fn cap_meta_never_emits_hostile_characters_or_grows_the_input() {
        // Fuzz-style sweep: whatever mix of hostile and benign characters the
        // generated input carries — controls, C1, DEL, bidi commands, Zl/Zp
        // separators, whitespace, path punctuation, RTL letters, emoji — the
        // output must always (a) contain no display-unsafe character (the
        // log/display injection invariant: no newline, NUL, bidi override, or
        // line separator can ride into a rendered field or a log line), (b)
        // be trimmed, (c) respect the character cap, (d) never be longer than
        // the input, (e) be a subsequence of it (sanitization never reorders
        // or invents text), and (f) be stable under reprocessing.
        fn is_subsequence(needle: &str, haystack: &str) -> bool {
            let mut it = haystack.chars();
            needle.chars().all(|c| it.any(|h| h == c))
        }
        const HOSTILE: &[char] = &[
            // display-unsafe: C0, DEL, C1, bidi commands, Zl/Zp separators.
            '\u{0}', '\u{1}', '\u{7}', '\u{1F}', '\u{7F}', '\u{80}', '\u{85}', '\u{9F}', '\u{202A}', '\u{202E}',
            '\u{2028}', '\u{2029}', '\u{2066}', '\u{2069}', // whitespace (trim boundaries and interior).
            ' ', '\t', '\n', '\r', '\u{2009}',
            // benign ASCII: letters, digits, punctuation including path chars.
            'a', 'z', 'A', 'Z', '0', '9', '.', '-', '_', '!', '/', '\\', ':', '?', '*', '"', '<', '>',
            // benign Unicode: accented, CJK, emoji, ZWJ, RTL letter, combining.
            'é', '你', '🎵', '\u{200D}', 'א', '\u{301}',
        ];
        let mut rng = FuzzRng(0xC0FF_EE00_C0FF_EE00);
        for _ in 0..2000 {
            // Straddle the 256-char cap, the empty string, and the trim path.
            let len = (rng.next() % 320) as usize;
            let input: String = (0..len).map(|_| rng.pick(HOSTILE)).collect();
            let out = cap_meta(input.clone());
            assert!(
                out.chars().all(|c| !display_unsafe(c)),
                "a display-unsafe character survived {out:?} from {input:?}"
            );
            assert_eq!(out.trim(), out, "output must be trimmed: {out:?} from {input:?}");
            assert!(
                out.chars().count() <= MAX_META_CHARS,
                "output exceeds the cap: {out:?} from {input:?}"
            );
            assert!(
                out.chars().count() <= input.chars().count(),
                "output grew the input: {out:?} from {input:?}"
            );
            assert!(
                is_subsequence(&out, &input),
                "output must be a subsequence of the input: {out:?} from {input:?}"
            );
            assert_eq!(cap_meta(out.clone()), out, "cap_meta must be idempotent: {out:?}");
        }
    }

    #[test]
    fn cap_meta_is_identity_for_benign_short_inputs() {
        // A clean, short, already-normalized string passes through unchanged
        // apart from trimming: the sanitizer must never mangle legitimate
        // metadata (titles with apostrophes, CJK, RTL letters, emoji).
        const SAFE: &[char] = &[
            'a', 'z', 'A', 'Z', '0', '9', ' ', '.', ',', '-', '_', '!', '?', 'é', '你', 'א', '🎵', '\u{200D}',
            '\u{301}',
        ];
        let mut rng = FuzzRng(0xBEEF_CAFE_BEEF_CAFE);
        for _ in 0..1000 {
            let len = 1 + (rng.next() % 200) as usize;
            let input: String = (0..len).map(|_| rng.pick(SAFE)).collect();
            assert_eq!(
                cap_meta(input.clone()),
                input.trim(),
                "benign input was mangled: {input:?}"
            );
        }
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
    fn strippable_title_falls_back_to_source_label_instead_of_empty() {
        // A title whose raw text is non-empty but made entirely of
        // characters `cap_meta` strips must not emit an empty-title pill.
        // The read path re-applies the source-app fallback after
        // sanitization, so the placeholder gate keeps working. This test
        // pins that re-application: sanitize-then-fallback equals the
        // fallback shape the gate already recognizes.
        let hostile = "\u{0}\u{1F}\u{202E}";
        let sanitized = cap_meta(non_empty(hostile.to_string(), "spotify"));
        assert!(sanitized.is_empty(), "precondition: controls strip to nothing");
        let merged = TrackInfo {
            title: sanitized,
            artist: "".into(),
            source_app: "spotify".into(),
            ..TrackInfo::default()
        };
        // With the fallback re-applied (as read_track_info now does), the
        // snapshot is exactly the placeholder shape.
        let mut with_fallback = merged.clone();
        with_fallback.title = with_fallback.source_app.clone();
        assert!(is_placeholder_like(&with_fallback));
        // And the un-repaired form would have slipped past — documenting
        // why the fix lives at the read site.
        assert!(!is_placeholder_like(&merged));
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
    fn wait_timeout_marker_is_recognized_only_for_async_read_timeouts() {
        use windows::core::HRESULT;
        // G4: the marker drives the wedged-source exclusion, so recognition
        // must be precise — a timed-out async read excludes the source,
        // while every transient failure (RPC-unavailable session, failed
        // Thumbnail, Size rejection) stays retryable and must not exclude.
        let timeout = anyhow::Error::new(AsyncReadTimeout { secs: 10 });
        assert!(is_wait_timeout(&timeout), "the marker itself must be recognized");
        // The artwork path wraps the raw error with context; the marker must
        // still be found through the chain (anyhow downcast_ref walks it).
        let wrapped = timeout.context("OpenReadAsync get failed");
        assert!(is_wait_timeout(&wrapped));
        let rpc = anyhow::Error::new(windows::core::Error::from(HRESULT(0x8007_06BAu32 as i32)));
        assert!(
            !is_wait_timeout(&rpc),
            "an RPC-unavailable session is transient, not wedged"
        );
        let other = anyhow!("GetPlaybackInfo failed: hr=0x80004005");
        assert!(!is_wait_timeout(&other));
    }

    /// A ListenerState for tests that only touch the exclusion map: the
    /// manager is a null handle (never dereferenced by these paths), the
    /// channels are drained nowhere, and every read-only field gets a default.
    /// Callers MUST NOT drop the returned state (and should `mem::forget` it):
    /// the null manager's `IUnknown` Drop calls `Release` on a null pointer,
    /// which crashes — the test process exit reclaims everything instead.
    fn listener_state_for_exclusion_tests() -> ListenerState {
        listener_state_with_exclusions(shared_exclusions())
    }

    /// Same as `listener_state_for_exclusion_tests`, but with a caller-owned
    /// shared exclusion map — the seam that pins the cross-generation
    /// lifetime of exclusions.
    fn listener_state_with_exclusions(excluded_sources: SharedExclusions) -> ListenerState {
        let (output, _rx) = mpsc::sync_channel(1);
        let (signal_tx, _signal_rx) = mpsc::sync_channel(1);
        ListenerState::new(
            unsafe { GlobalSystemMediaTransportControlsSessionManager::from_raw(std::ptr::null_mut()) },
            ListenerSeed {
                media_sources: Vec::new(),
                debounce_ms: 1,
            },
            output,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            signal_tx,
            Arc::new(Mutex::new(Instant::now())),
            Arc::new(AtomicU64::new(0)),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            excluded_sources,
            Arc::new(Mutex::new(ControlMailbox::default())),
        )
    }

    #[test]
    fn seed_snapshots_the_allow_list_and_clamps_the_debounce() {
        // The worker never reads the shared config again after `new`: the
        // seed is the only source for the allow list and the debounce, so
        // the snapshot must be normalized at construction and the debounce
        // clamped to the coalescing range like the old live read was.
        let (output, _rx) = mpsc::sync_channel(1);
        let (signal_tx, _signal_rx) = mpsc::sync_channel(1);
        let state = ListenerState::new(
            unsafe { GlobalSystemMediaTransportControlsSessionManager::from_raw(std::ptr::null_mut()) },
            ListenerSeed {
                media_sources: vec!["YouTube-Music".to_string()],
                debounce_ms: 1,
            },
            output,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            signal_tx,
            Arc::new(Mutex::new(Instant::now())),
            Arc::new(AtomicU64::new(0)),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(ControlMailbox::default())),
        );
        let (raw, normalized) = state.cached_allowed.as_ref().expect("seed cached");
        assert_eq!(raw, &["YouTube-Music".to_string()]);
        assert_eq!(normalized, &["youtubemusic".to_string()]);
        assert_eq!(
            state.debounce,
            Duration::from_millis(150),
            "a sub-floor seed clamps to the coalescing floor"
        );
        std::mem::forget(state);
    }

    #[test]
    fn control_set_allowed_sources_replaces_the_cached_allow_list() {
        // The settings UI pushes the confirmed patterns as a command; the
        // worker must store and normalize them once at apply time, replacing
        // the snapshot the seed took.
        let (output, _rx) = mpsc::sync_channel(1);
        let (signal_tx, _signal_rx) = mpsc::sync_channel(1);
        let mut state = ListenerState::new(
            unsafe { GlobalSystemMediaTransportControlsSessionManager::from_raw(std::ptr::null_mut()) },
            ListenerSeed {
                media_sources: vec!["old-app".to_string()],
                debounce_ms: 200,
            },
            output,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            signal_tx,
            Arc::new(Mutex::new(Instant::now())),
            Arc::new(AtomicU64::new(0)),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(ControlMailbox::default())),
        );
        state
            .handle_control(ControlCommand::SetAllowedSources(vec![
                " Spotify ".to_string(),
                "youtube-music".to_string(),
            ]))
            .unwrap();
        let (raw, normalized) = state.cached_allowed.as_ref().expect("allow list stored");
        assert_eq!(raw, &[" Spotify ".to_string(), "youtube-music".to_string()]);
        assert_eq!(normalized, &["spotify".to_string(), "youtubemusic".to_string()]);
        std::mem::forget(state);
    }

    #[test]
    fn control_mailbox_coalesces_newest_wins_per_kind() {
        // The mailbox keeps one slot per command kind: an older push of the
        // same kind is superseded, so the drain yields at most the newest
        // value of each. Kinds are independent (absolute values), so
        // coalescing cannot skip a needed transition.
        let mut mailbox = ControlMailbox::default();
        mailbox.push(ControlCommand::SetAllowedSources(vec!["first-app".to_string()]));
        mailbox.push(ControlCommand::SetNotificationsEnabled(true));
        mailbox.push(ControlCommand::SetAllowedSources(vec!["newest-app".to_string()]));
        mailbox.push(ControlCommand::SetNotificationsEnabled(false));

        let commands = mailbox.drain();
        assert_eq!(commands.len(), 2, "one command per kind, newest first");
        assert!(
            matches!(&commands[0], ControlCommand::SetNotificationsEnabled(false)),
            "the newest notifications value wins"
        );
        assert!(
            matches!(&commands[1], ControlCommand::SetAllowedSources(sources) if sources == &["newest-app"]),
            "the newest allow list wins"
        );
        assert!(mailbox.drain().is_empty(), "drain clears the mailbox");
    }

    #[test]
    fn control_mailbox_delivers_under_a_saturated_signal_channel() {
        // Capacity-saturation proof of the control path: the old channel-borne
        // commands were dropped when the 256-entry queue was full, leaving the
        // worker with its stale allow list. Saturation must not affect the
        // mailbox: the push lands, the wake-up hint is the only casualty, and
        // the worker applies the newest list at its next turn.
        let (output, _rx) = mpsc::sync_channel(1);
        let (signal_tx, signal_rx) = mpsc::sync_channel::<Signal>(SIGNAL_QUEUE_CAP);
        let mailbox = Arc::new(Mutex::new(ControlMailbox::default()));
        let mut state = ListenerState::new(
            unsafe { GlobalSystemMediaTransportControlsSessionManager::from_raw(std::ptr::null_mut()) },
            ListenerSeed {
                media_sources: vec!["old-app".to_string()],
                debounce_ms: 200,
            },
            output,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            signal_tx.clone(),
            Arc::new(Mutex::new(Instant::now())),
            Arc::new(AtomicU64::new(0)),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            shared_exclusions(),
            mailbox.clone(),
        );
        // Fill the queue exactly to capacity: the push that fails proves the
        // queue is saturated, so the wake-up hint below is known-dropped.
        while signal_tx.try_send(Signal::Sessions).is_ok() {}
        // The control push succeeds regardless of saturation.
        mailbox
            .lock()
            .unwrap()
            .push(ControlCommand::SetAllowedSources(vec!["new-app".to_string()]));
        assert!(
            signal_tx.try_send(Signal::ControlWake).is_err(),
            "with the queue saturated the wake-up hint is dropped"
        );
        // The worker's per-turn drain delivers the command even though the
        // wake never arrived.
        state.drain_control().unwrap();
        let (raw, normalized) = state.cached_allowed.as_ref().expect("allow list stored");
        assert_eq!(raw, &["new-app".to_string()]);
        assert_eq!(normalized, &["newapp".to_string()]);
        drop(signal_rx);
        std::mem::forget(state);
    }

    #[test]
    fn stale_worker_drain_leaves_commands_for_the_successor() {
        // A worker superseded by a restart must not consume control commands:
        // the mailbox survives restarts precisely so the replacement worker
        // applies them. The drain's verify-take under the mailbox lock is
        // what enforces this; the supervisor's bump takes the same lock, so
        // verify-and-take is atomic with the restart.
        let (output, _rx) = mpsc::sync_channel(1);
        let (signal_tx, _signal_rx) = mpsc::sync_channel::<Signal>(SIGNAL_QUEUE_CAP);
        let mailbox = Arc::new(Mutex::new(ControlMailbox::default()));
        let live_generation = Arc::new(AtomicU64::new(0));
        let mut state = ListenerState::new(
            unsafe { GlobalSystemMediaTransportControlsSessionManager::from_raw(std::ptr::null_mut()) },
            ListenerSeed {
                media_sources: vec!["seed-app".to_string()],
                debounce_ms: 200,
            },
            output,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            signal_tx,
            Arc::new(Mutex::new(Instant::now())),
            live_generation.clone(),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            shared_exclusions(),
            mailbox.clone(),
        );
        mailbox
            .lock()
            .unwrap()
            .push(ControlCommand::SetAllowedSources(vec!["new-app".to_string()]));
        // The supervisor bumps the generation the moment it restarts the
        // worker; the stale worker's drain must then leave the mailbox alone.
        live_generation.store(1, Ordering::SeqCst);
        state.drain_control().unwrap();
        // The command is still pending for the successor.
        let pending = mailbox.lock().unwrap().drain();
        assert!(
            matches!(&pending[0], ControlCommand::SetAllowedSources(sources) if sources == &["new-app".to_string()]),
            "a superseded worker must not consume the successor's commands"
        );
        // And the stale worker's own state was untouched.
        assert_eq!(
            state.cached_allowed.as_ref().expect("seed allow list stored").0,
            &["seed-app".to_string()]
        );
        std::mem::forget(state);
    }

    #[test]
    fn superseded_worker_exits_its_event_loop_promptly() {
        // The turn-top generation check makes a superseded worker leave
        // without draining the mailbox, polling sessions, or waiting out its
        // receive timeout. Without the check the loop runs until `shutdown`
        // (never set here), so the elapsed assertion fails instead of the
        // test hanging.
        let (output, _rx) = mpsc::sync_channel(1);
        let (signal_tx, signal_rx) = mpsc::sync_channel::<Signal>(SIGNAL_QUEUE_CAP);
        let mailbox = Arc::new(Mutex::new(ControlMailbox::default()));
        let live_generation = Arc::new(AtomicU64::new(0));
        let mut state = ListenerState::new(
            unsafe { GlobalSystemMediaTransportControlsSessionManager::from_raw(std::ptr::null_mut()) },
            ListenerSeed {
                media_sources: vec![],
                debounce_ms: 200,
            },
            output,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            signal_tx.clone(),
            Arc::new(Mutex::new(Instant::now())),
            live_generation.clone(),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            shared_exclusions(),
            mailbox.clone(),
        );
        live_generation.store(1, Ordering::SeqCst);
        let started = Instant::now();
        state
            .event_loop(Arc::new(Mutex::new(signal_rx)))
            .expect("a superseded worker exits cleanly");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a superseded worker must exit without waiting out its receive timeout"
        );
        std::mem::forget(state);
    }

    #[test]
    fn churn_trips_the_exclusion_and_gates_the_source() {
        // A source that recreates its session CHURN_THRESHOLD times inside
        // the window lands on the shared exclusion map, and the map gates it:
        // `source_on_cooldown` is what the churn path and the G4 wedged-read
        // path share, so the trip must be visible through it.
        let mut state = listener_state_for_exclusion_tests();
        assert!(!state.source_on_cooldown("spotify"), "a fresh source is not excluded");
        for _ in 0..CHURN_THRESHOLD {
            state.record_churn("spotify");
        }
        assert!(
            state.source_on_cooldown("spotify"),
            "the churn cool-down must gate the source"
        );
        // Churn while already excluded is absorbed: the source stays gated
        // and the exclusion is not extended (the guard prevents re-insert).
        state.record_churn("spotify");
        assert!(state.source_on_cooldown("spotify"));
        assert_eq!(
            state
                .excluded_sources
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1,
            "one exclusion per source"
        );
        std::mem::forget(state);
    }

    #[test]
    fn an_excluded_source_never_pays_a_rejected_row_read() {
        // The rejected-row read gate: a source already on the churn/wedged
        // cool-down must never issue another metadata read — the early
        // return happens before any session call, so a hostile source
        // minting fresh session keys cannot cost a fresh 10 s wedge per key
        //. The null session proves the point: had the read started,
        // the null dereference would crash the test.
        let mut state = listener_state_for_exclusion_tests();
        state
            .excluded_sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                "spotify".to_string(),
                Instant::now() + Duration::from_millis(CHURN_COOLDOWN_MS),
            );
        let session = unsafe { GlobalSystemMediaTransportControlsSession::from_raw(std::ptr::null_mut()) };
        let (title, artist) = state.rejected_row_text(&session, "spotify");
        assert_eq!(title, "spotify", "the row falls back to the source label");
        assert_eq!(artist, "");
        std::mem::forget(session);
        std::mem::forget(state);
    }

    #[test]
    fn exclusions_survive_into_a_replacement_worker() {
        // The exclusion map is created in `main` and shared across worker
        // generations: a supervisor restart must not reset the exclusions
        // the previous worker paid for — a replacement worker re-excluding
        // a wedged source costs a fresh READ_ASYNC_TIMEOUT read each time.
        let shared = shared_exclusions();
        let predecessor = listener_state_with_exclusions(shared.clone());
        {
            let mut map = predecessor
                .excluded_sources
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            map.insert(
                "spotify".to_string(),
                Instant::now() + Duration::from_millis(CHURN_COOLDOWN_MS),
            );
        }
        // The replacement worker is constructed with the same cell (exactly
        // how `main` hands it to every spawn) and must observe the
        // predecessor's exclusion without any write of its own.
        let replacement = listener_state_with_exclusions(shared.clone());
        assert!(
            replacement.source_on_cooldown("spotify"),
            "an exclusion written by the predecessor worker must gate the replacement"
        );
        std::mem::forget(predecessor);
        std::mem::forget(replacement);
    }

    #[test]
    fn cooldown_expiry_releases_the_source() {
        // Both exclusion writers — the churn cool-down and the G4 wedged-read
        // path — insert into the same `excluded_sources` map, so the expiry
        // semantics are shared: a source whose deadline has passed is
        // re-admitted and re-tested.
        let state = listener_state_for_exclusion_tests();
        state
            .excluded_sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                "spotify".to_string(),
                Instant::now() + Duration::from_millis(CHURN_COOLDOWN_MS),
            );
        assert!(state.source_on_cooldown("spotify"), "a live exclusion gates the source");
        state
            .excluded_sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert("spotify".to_string(), Instant::now() - Duration::from_millis(1));
        assert!(
            !state.source_on_cooldown("spotify"),
            "an expired exclusion releases the source"
        );
        std::mem::forget(state);
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
    fn stale_thumbnail_pairing_cannot_pass_the_retry_emit_gate() {
        // The retry path drops stale art BEFORE the emit gate (the same
        // `stale_thumbnail` guard the refresh path applies), so a read
        // pairing the NEW identity with the PREVIOUS track's bytes can never
        // surface the wrong cover. This pins the invariant the drop relies
        // on: `retry_should_emit` ALONE would let the stale pairing through
        // (identity differs, art present) — the guard must be applied, not
        // skipped, on the retry path.
        let old = TrackInfo {
            title: "Old".into(),
            artist: "Artist".into(),
            artwork: Some(Arc::from(vec![9])),
            ..TrackInfo::default()
        };
        let stale = TrackInfo {
            title: "New".into(),
            artist: "Artist".into(),
            artwork: Some(Arc::from(vec![9])),
            ..TrackInfo::default()
        };
        assert!(stale_thumbnail(&stale, Some(&old)));
        assert!(retry_should_emit(&stale, Some(&old)));
        // The real cover (different bytes) passes the guard and the gate.
        let real = TrackInfo {
            title: "New".into(),
            artist: "Artist".into(),
            artwork: Some(Arc::from(vec![10])),
            ..TrackInfo::default()
        };
        assert!(!stale_thumbnail(&real, Some(&old)));
        assert!(retry_should_emit(&real, Some(&old)));
        // Same identity with art already shown stays suppressed (recreation).
        assert!(!retry_should_emit(&old, Some(&old)));
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
    fn prioritize_sessions_orders_current_existing_then_new() {
        // Current session first, then surviving existing subscriptions in
        // snapshot order, then genuinely new sessions — the exact contract
        // the live sync loop feeds to the caps.
        let before: HashSet<usize> = [4, 5].into_iter().collect();
        let snapshot: Vec<(usize, String)> = vec![
            (1, "one".into()),
            (2, "two".into()),
            (3, "three".into()),
            (4, "four".into()),
            (5, "five".into()),
            (6, "six".into()),
        ];
        let ordered = prioritize_sessions(&snapshot, Some((2, "two".into())), &before, 64);
        let keys: Vec<usize> = ordered.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![2, 4, 5, 1, 3, 6]);
    }

    #[test]
    fn prioritize_sessions_truncates_new_candidates_to_the_session_cap() {
        // A hostile storm of brand-new sessions is truncated at the session
        // cap before the loop, so the per-sync work (and the log lines) stay
        // bounded; overflow candidates are never even enumerated.
        let before: HashSet<usize> = [1, 2].into_iter().collect();
        let snapshot: Vec<(usize, String)> = (1..=100).map(|k| (k, format!("src-{k}"))).collect();
        let ordered = prioritize_sessions(&snapshot, None, &before, MAX_TRACKED_SESSIONS);
        let keys: Vec<usize> = ordered.iter().map(|(k, _)| *k).collect();
        // Existing subscriptions first (1, 2), then new candidates truncated
        // at the session cap (3..=66); the overflow is never enumerated.
        assert_eq!(keys, (1..=(2 + MAX_TRACKED_SESSIONS)).collect::<Vec<usize>>());
        assert_eq!(ordered.len(), 2 + MAX_TRACKED_SESSIONS);
    }

    #[test]
    fn prioritize_sessions_includes_a_current_session_missing_from_the_snapshot() {
        // GetCurrentSession can outrun a stale GetSessions snapshot (browser
        // churn); the current session is authoritative and comes first.
        let before: HashSet<usize> = HashSet::new();
        let snapshot: Vec<(usize, String)> = vec![(1, "one".into()), (2, "two".into())];
        let ordered = prioritize_sessions(&snapshot, Some((9, "nine".into())), &before, 64);
        let keys: Vec<usize> = ordered.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![9, 1, 2], "the current session must lead and not duplicate");
    }

    #[test]
    fn prioritize_sessions_feeds_admit_sessions_like_the_live_loop() {
        // The exact chain the live loop performs: prioritize, then cap. A
        // storm of 100 new sessions with a session cap of 64 — the caps bind
        // only after truncation, existing subscriptions survive, and the
        // overflow is never enumerated (bounded per-sync work).
        let existing_keys: HashSet<usize> = [1].into_iter().collect();
        let existing_sources: HashSet<String> = ["spotify".to_string()].into_iter().collect();
        // One hostile source with 99 sessions: only the session cap can bind.
        let snapshot: Vec<(usize, String)> = std::iter::once((1, "spotify".to_string()))
            .chain((2..=100).map(|k| (k, "storm-src".to_string())))
            .collect();
        let ordered = prioritize_sessions(&snapshot, None, &existing_keys, MAX_TRACKED_SESSIONS);
        let (admitted, rejected) = admit_sessions(
            &ordered,
            &existing_keys,
            &existing_sources,
            MAX_TRACKED_SESSIONS,
            MAX_TRACKED_SOURCES,
        );
        assert!(admitted.contains(&1), "the existing subscription survives the storm");
        assert_eq!(admitted.len(), MAX_TRACKED_SESSIONS);
        // The session cap binds within the truncated enumeration (the 65th
        // session is rejected), and the storm candidates beyond the
        // truncation are never seen by the caps at all — that is the point
        // of the enumeration cap.
        assert_eq!(rejected, 1);
        assert!(ordered.iter().all(|(k, _)| *k <= 2 + MAX_TRACKED_SESSIONS));
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
        coalesce_pending_event(
            &mut queue,
            first,
            OUTPUT_RETRY_CAP,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
        );
        coalesce_pending_event(
            &mut queue,
            second.clone(),
            OUTPUT_RETRY_CAP,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
        );
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
            &AtomicU64::new(0),
            &AtomicBool::new(false),
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
            &AtomicU64::new(0),
            &AtomicBool::new(false),
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
        coalesce_pending_event(
            &mut queue,
            track_a.clone(),
            OUTPUT_RETRY_CAP,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
        );
        coalesce_pending_event(
            &mut queue,
            track_b.clone(),
            OUTPUT_RETRY_CAP,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
        );
        assert_eq!(queue.len(), 2, "cross-source tracks keep arrival order");
        assert!(Arc::ptr_eq(&queue[0], &track_a), "first-arrived track stays first");

        // Over-cap drops the oldest queued event, never the newest
        // authoritative state just committed.
        let mut queue = VecDeque::new();
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "a".into())),
            2,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
        );
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "b".into())),
            2,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
        );
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "c".into())),
            2,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
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
    fn over_cap_pop_of_the_budget_warning_resets_the_latch() {
        // The one-shot budget warning queued at the head of a full mailbox is
        // discarded undelivered by the over-cap pop; its latch must reset so
        // a later strip re-warns instead of the note being lost for the rest
        // of the app run — the second warning-loss path (the first, the
        // mailbox-clear at worker teardown, is covered alongside
        // `mailbox_holds_budget_warning`).
        let mut queue = VecDeque::new();
        let latch = AtomicBool::new(true); // the warning was emitted; latch set
        queue.push_back(Arc::new(MediaEvent::ArtworkBudgetExceeded));
        queue.push_back(Arc::new(MediaEvent::PlaybackStateChanged(
            PlaybackState::Stopped,
            "a".into(),
        )));
        let dropped = coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "b".into())),
            2,
            &AtomicU64::new(0),
            &latch,
        );
        assert_eq!(dropped, 1, "the warning aged to the head and was popped");
        assert!(
            !latch.load(Ordering::Relaxed),
            "the discarded warning must reset the latch"
        );
        match queue[0].as_ref() {
            MediaEvent::PlaybackStateChanged(_, source) => assert_eq!(source, "a"),
            other => panic!("expected the surviving 'a' event, got {other:?}"),
        }

        // The reset is event-precise, not count-precise: the same pop that
        // removes a different oldest event leaves the still-queued warning's
        // latch set — the note is pending delivery, so no re-warn may fire.
        let mut queue = VecDeque::new();
        let latch = AtomicBool::new(true);
        queue.push_back(Arc::new(MediaEvent::PlaybackStateChanged(
            PlaybackState::Stopped,
            "a".into(),
        )));
        queue.push_back(Arc::new(MediaEvent::ArtworkBudgetExceeded));
        let dropped = coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "b".into())),
            2,
            &AtomicU64::new(0),
            &latch,
        );
        assert_eq!(dropped, 1, "the over-cap pop still ran");
        assert!(latch.load(Ordering::Relaxed), "the surviving warning keeps its latch");
        assert!(
            matches!(queue[0].as_ref(), MediaEvent::ArtworkBudgetExceeded),
            "the warning must still be queued for delivery"
        );
    }

    #[test]
    fn full_output_channel_replays_latest_state_after_drain() {
        // Capacity-1 channel: fill it, commit two playback states (the newer
        // supersedes the older in the mailbox), drain the channel, then flush.
        // The latest authoritative state arrives; nothing is permanently
        // invisible just because the channel was briefly full.
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
            &AtomicU64::new(0),
            &AtomicBool::new(false),
        );
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Playing, "src".into())),
            OUTPUT_RETRY_CAP,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
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
            &AtomicU64::new(0),
            &AtomicBool::new(false),
        );
        coalesce_pending_event(
            &mut queue,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "a".into())),
            OUTPUT_RETRY_CAP,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
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
            &AtomicU64::new(0),
            &AtomicBool::new(false),
        );
        drop(rx);
        assert!(drain_pending_to_channel(&mut queue, &tx), "disconnect must be reported");
        queue.clear();
        assert!(queue.is_empty());
    }

    #[test]
    fn artwork_bytes_counts_only_track_payloads() {
        // Only TrackChanged carries image payloads; the raw cover plus the
        // fixed decode are what a wedged forwarder would retain.
        let with_art = TrackInfo {
            title: "Song".into(),
            artwork: Some(Arc::from(vec![0u8; 8])),
            decoded_art: Some(Arc::from(vec![0u8; 4])),
            ..TrackInfo::default()
        };
        assert_eq!(artwork_bytes(&MediaEvent::TrackChanged(with_art)), 12);
        // An artless track and every non-track variant count nothing.
        assert_eq!(artwork_bytes(&MediaEvent::TrackChanged(track("Song", "A"))), 0);
        assert_eq!(
            artwork_bytes(&MediaEvent::PlaybackStateChanged(PlaybackState::Playing, "s".into())),
            0
        );
        assert_eq!(
            artwork_bytes(&MediaEvent::ProgressChanged {
                source_app: "s".into(),
                position_secs: Some(1.0),
                duration_secs: None,
                playback_rate: None,
            }),
            0
        );
    }

    #[test]
    fn budget_artwork_strips_only_when_the_budget_would_be_exceeded() {
        // The G1 budget decision: an event whose artwork would push the
        // in-flight bytes past the budget loses its payload (raw cover,
        // decode and derived palette) while the metadata survives, so the
        // pill still renders the track with a placeholder instead of pinning
        // megabytes behind a queued event.
        let art = |len: usize| TrackInfo {
            title: "Song".into(),
            artwork: Some(Arc::from(vec![0u8; len])),
            decoded_art: Some(Arc::from(vec![0u8; 4])),
            palette: Some(Palette {
                primary: [0; 4],
                secondary: [0; 4],
            }),
            ..TrackInfo::default()
        };
        // Fits: the full artwork is counted (raw + decode) and untouched.
        let mut event = MediaEvent::TrackChanged(art(10));
        assert_eq!(budget_artwork(&mut event, 0, 100), 14);
        match &event {
            MediaEvent::TrackChanged(track) => assert!(track.artwork.is_some()),
            other => panic!("expected TrackChanged, got {other:?}"),
        }
        // Exactly at the budget still fits.
        let mut event = MediaEvent::TrackChanged(art(96));
        assert_eq!(budget_artwork(&mut event, 0, 100), 100);
        // Over the budget: stripped, nothing counted, metadata kept.
        let mut event = MediaEvent::TrackChanged(art(97));
        assert_eq!(budget_artwork(&mut event, 0, 100), 0);
        match event {
            MediaEvent::TrackChanged(track) => {
                assert!(track.artwork.is_none(), "the raw cover must be dropped");
                assert!(track.decoded_art.is_none(), "the decode must be dropped too");
                assert!(track.palette.is_none(), "the cover-derived palette must go with it");
                assert_eq!(track.title, "Song", "the metadata must survive the strip");
            }
            other => panic!("expected TrackChanged, got {other:?}"),
        }
        // The budget counts against in-flight bytes, not per event: a second
        // event that fits alone is still stripped when the first already
        // consumed the budget. art(60) counts 64 (60 raw + 4 decode).
        let mut first = MediaEvent::TrackChanged(art(60));
        let mut second = MediaEvent::TrackChanged(art(60));
        assert_eq!(budget_artwork(&mut first, 0, 100), 64);
        assert_eq!(budget_artwork(&mut second, 64, 100), 0, "64 of 100 already in flight");
        // Artless and non-track events are never counted or touched.
        let mut artless = MediaEvent::TrackChanged(track("Song", "A"));
        assert_eq!(budget_artwork(&mut artless, 100, 100), 0);
        let mut playback = MediaEvent::PlaybackStateChanged(PlaybackState::Playing, "s".into());
        assert_eq!(budget_artwork(&mut playback, u64::MAX, 1), 0);
    }

    #[test]
    fn coalesce_pending_event_frees_bytes_for_dropped_events() {
        // The mailbox accounting mirrors `emit`: queueing adds bytes, and
        // every event that leaves the mailbox — superseded by a newer
        // same-key event, or popped by the over-cap rule — frees them. The
        // counter must track distinct live allocations, not queue slots.
        // The source distinguishes coalesce keys (same source supersedes,
        // different sources coexist).
        let art = |len: usize, source: &str| {
            Arc::new(MediaEvent::TrackChanged(TrackInfo {
                title: "Song".into(),
                source_app: source.into(),
                artwork: Some(Arc::from(vec![0u8; len])),
                ..TrackInfo::default()
            }))
        };
        // Supersede: the older same-source event's bytes are freed.
        let mut queue = VecDeque::new();
        let counter = AtomicU64::new(0);
        counter.fetch_add(10, Ordering::Relaxed); // emit counted art(10)
        coalesce_pending_event(
            &mut queue,
            art(10, "src"),
            OUTPUT_RETRY_CAP,
            &counter,
            &AtomicBool::new(false),
        );
        assert_eq!(counter.load(Ordering::Relaxed), 10, "queued bytes stay counted");
        counter.fetch_add(20, Ordering::Relaxed); // emit counted art(20)
        coalesce_pending_event(
            &mut queue,
            art(20, "src"),
            OUTPUT_RETRY_CAP,
            &counter,
            &AtomicBool::new(false),
        );
        assert_eq!(
            counter.load(Ordering::Relaxed),
            20,
            "the superseded 10-byte event is freed"
        );
        // Over-cap: the oldest event's bytes are freed, never the newest's.
        let mut queue = VecDeque::new();
        let counter = AtomicU64::new(0);
        for (len, source) in [(10u64, "a"), (20, "b"), (30, "c")] {
            counter.fetch_add(len, Ordering::Relaxed); // emit counted each
            coalesce_pending_event(
                &mut queue,
                art(len as usize, source),
                2,
                &counter,
                &AtomicBool::new(false),
            );
        }
        assert_eq!(queue.len(), 2, "the cap must hold");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            50,
            "the oldest 10-byte event was freed by the over-cap pop"
        );
    }

    #[test]
    fn release_pending_bytes_frees_every_queued_payload() {
        // The disconnect path drops the whole mailbox; every queued payload's
        // bytes must be freed so the counter ends where it started.
        let mut queue = VecDeque::new();
        let counter = AtomicU64::new(100);
        let art = |len: usize| {
            Arc::new(MediaEvent::TrackChanged(TrackInfo {
                title: "Song".into(),
                artwork: Some(Arc::from(vec![0u8; len])),
                ..TrackInfo::default()
            }))
        };
        queue.push_back(art(10));
        queue.push_back(art(30));
        queue.push_back(Arc::new(MediaEvent::PlaybackStateChanged(
            PlaybackState::Stopped,
            "s".into(),
        )));
        release_pending_bytes(&mut queue, &counter);
        assert!(queue.is_empty(), "the mailbox must be fully drained");
        assert_eq!(counter.load(Ordering::Relaxed), 60, "the two payloads' bytes are freed");
    }

    #[test]
    fn mailbox_holds_budget_warning_recognizes_the_warning() {
        // The latch-reset edge: when the worker discards a mailbox that still
        // holds the undelivered one-shot budget warning, `clear_pending_output`
        // must reset `budget_warned` so the replacement worker can re-warn.
        // Recognition must be precise — only the warning itself counts, not
        // the ordinary events that share the mailbox.
        let mut queue = VecDeque::new();
        assert!(
            !mailbox_holds_budget_warning(&queue),
            "an empty mailbox holds no warning"
        );
        queue.push_back(Arc::new(MediaEvent::TrackChanged(TrackInfo {
            title: "Song".into(),
            source_app: "spotify".into(),
            ..TrackInfo::default()
        })));
        assert!(
            !mailbox_holds_budget_warning(&queue),
            "an ordinary track event is not the warning"
        );
        queue.push_back(Arc::new(MediaEvent::ArtworkBudgetExceeded));
        assert!(mailbox_holds_budget_warning(&queue), "the warning must be recognized");
    }

    #[test]
    fn wait_outcome_timeout_is_the_wedged_read_marker() {
        // A timed-out wait is the marker error, carrying the bound for the
        // WARN; is_wait_timeout recognizes it, driving the per-source
        // exclusion instead of a retry.
        let outcome = wait_outcome(Err(mpsc::RecvTimeoutError::Timeout), Duration::from_secs(10));
        let error = outcome.expect_err("a timeout is an error");
        assert_eq!(error.secs, 10);
        assert_eq!(error.to_string(), "SMTC async read did not complete within 10s");
        assert!(
            is_wait_timeout(&anyhow::Error::new(error)),
            "the marker must drive the exclusion"
        );
    }

    #[test]
    fn wait_outcome_ok_and_disconnected_both_proceed() {
        // The handler fired, or the channel disconnected because the handler
        // was replaced or dropped: both mean the read may proceed to
        // GetResults.
        assert!(wait_outcome(Ok(()), Duration::from_secs(10)).is_ok());
        assert!(
            wait_outcome(Err(mpsc::RecvTimeoutError::Disconnected), Duration::from_secs(10)).is_ok(),
            "a disconnected channel is a completed operation, not a timeout"
        );
    }

    #[test]
    fn is_wait_timeout_rejects_transient_read_failures() {
        // Only the marker excludes a source; ordinary read failures stay
        // retryable.
        assert!(!is_wait_timeout(&anyhow!("RPC-unavailable session")));
        assert!(!is_wait_timeout(&anyhow::Error::from(std::io::Error::other(
            "read rejected"
        ))));
    }

    // ----------------------------------------------------------------------
    // Shared COM mock for the plain `IAsyncOperation<i32>` wait path.
    //
    // The two wait_async race tests used to hand-roll two inline structs
    // (NeverCompleting, CompletesInTime) with identical
    // IAsyncInfo/IAsyncOperation boilerplate; MockAsyncOp replaces both.
    // `fire_after` selects the behavior and the mock manages its own firing
    // thread, so each test shrinks to a constructor call plus the wait.
    // ----------------------------------------------------------------------
    use windows::core::implement;
    use windows_future::{
        AsyncOperationCompletedHandler, AsyncOperationProgressHandler, AsyncOperationWithProgressCompletedHandler,
        IAsyncInfo, IAsyncInfo_Impl, IAsyncOperation, IAsyncOperation_Impl, IAsyncOperationWithProgress,
        IAsyncOperationWithProgress_Impl,
    };

    /// One `IAsyncInfo_Impl` implementation serving every operation mock —
    /// the single source of truth for the five hand-rolled copies
    /// (MockAsyncOp, MockAsyncOpProgress, ReadyStreamOp, MockProgressReadOp,
    /// MockNeverCompletingStreamOp) that were byte-identical except
    /// `Status`. `$impl_ty` is the `_Impl` type `#[implement]` generates
    /// from the struct name; `$status` is the operation's initial status —
    /// `Started` for the mocks that drive the wait path, `Completed` for
    /// `ReadyStreamOp` (whose fast-path role the OpenReadAsync success seam
    /// leans on). A change to any IAsyncInfo method is one edit instead of
    /// five, and the copies can never drift again.
    macro_rules! mock_async_info {
        ($impl_ty:ty, $status:expr) => {
            impl IAsyncInfo_Impl for $impl_ty {
                fn Id(&self) -> windows::core::Result<u32> {
                    Ok(1)
                }
                fn Status(&self) -> windows::core::Result<AsyncStatus> {
                    Ok($status)
                }
                fn ErrorCode(&self) -> windows::core::Result<windows::core::HRESULT> {
                    Ok(windows::core::HRESULT(0))
                }
                fn Cancel(&self) -> windows::core::Result<()> {
                    Ok(())
                }
                fn Close(&self) -> windows::core::Result<()> {
                    Ok(())
                }
            }
        };
    }

    // The windows delegate is not `Send` (its IUnknown is a NonNull), but
    // its callback is Send by `AsyncOperationCompletedHandler::new`'s
    // bound, and cross-thread invocation is the designed use of a
    // completion handler — WinRT fires them on the completing thread, which
    // is often not the thread that created them. So wrapping it for the
    // firing thread is sound.
    #[derive(Clone)]
    struct SendHandler(AsyncOperationCompletedHandler<i32>);
    // SAFETY: invoking the delegate from another thread runs its Send
    // closure there — the designed use of an async completion handler.
    unsafe impl Send for SendHandler {}

    /// State shared between the mock and its firing thread: the retained
    /// completion handler, and the operation handle the thread passes to
    /// `Invoke`. The handle is wired by the constructor only in the firing
    /// case and `take()`n back by the thread at fire time, so the COM object
    /// never holds a reference to itself past the fire (a retained
    /// self-reference would leak the object past the test).
    struct MockShared {
        handler: Option<SendHandler>,
        op: Option<IAsyncOperation<i32>>,
    }

    /// One COM mock serving both plain `IAsyncOperation<i32>` races.
    /// `fire_after: None` retains the completion handler and never invokes
    /// it — the wedged read, so the wait must time out. (Retention is what
    /// keeps the signal channel open: the macro's own handler temporary is
    /// dropped at the end of the `SetCompleted` statement, so a dropped
    /// mock-side handler would disconnect the channel and make the wait
    /// *proceed* to `GetResults` — the opposite outcome.) `fire_after:
    /// Some(delay)` additionally spawns a thread that invokes the retained
    /// handler `delay` after `SetCompleted` installs it — the completion
    /// race, so the wait must return `result` instead of excluding the
    /// source.
    #[implement(IAsyncOperation<i32>, IAsyncInfo)]
    struct MockAsyncOp {
        fire_after: Option<Duration>,
        result: i32,
        shared: Arc<Mutex<MockShared>>,
    }

    impl MockAsyncOp {
        // The COM mock is consumed by `.into()` the moment it exists, so the
        // construction point must hand back the interface it produced rather
        // than `Self` — the mock cannot outlive its conversion.
        #[allow(clippy::new_ret_no_self)]
        fn new(fire_after: Option<Duration>, result: i32) -> IAsyncOperation<i32> {
            let shared = Arc::new(Mutex::new(MockShared {
                handler: None,
                op: None,
            }));
            let op: IAsyncOperation<i32> = MockAsyncOp {
                fire_after,
                result,
                shared: shared.clone(),
            }
            .into();
            if fire_after.is_some() {
                // The firing thread must pass the completing operation to
                // Invoke, and the operation cannot know itself before
                // `.into()`, so the constructor wires the handle in once it
                // exists.
                shared.lock().unwrap().op = Some(op.clone());
            }
            op
        }
    }

    mock_async_info!(MockAsyncOp_Impl, AsyncStatus::Started);

    impl IAsyncOperation_Impl<i32> for MockAsyncOp_Impl {
        fn SetCompleted(
            &self,
            handler: windows::core::Ref<AsyncOperationCompletedHandler<i32>>,
        ) -> windows::core::Result<()> {
            let mut guard = self.shared.lock().unwrap();
            guard.handler = handler.ok().ok().cloned().map(SendHandler);
            drop(guard);
            if let Some(delay) = self.fire_after {
                let shared = self.shared.clone();
                // Fires on a detached background thread: a panic here unwinds
                // the thread and aborts the whole test process (an AV-shaped
                // crash, not a caught test failure). If the op/handler clones
                // are already gone, the test ended first — return quietly
                // rather than expect-ing into an abort. `Invoke`'s result is
                // likewise ignored: the mock delegate is fire-and-forget.
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    let (handler, op) = {
                        let mut guard = shared.lock().unwrap();
                        match (guard.handler.clone(), guard.op.take()) {
                            (Some(handler), Some(op)) => (handler, op),
                            _ => return,
                        }
                    };
                    let _ = handler.0.Invoke(&op, AsyncStatus::Completed);
                });
            }
            Ok(())
        }
        fn Completed(&self) -> windows::core::Result<AsyncOperationCompletedHandler<i32>> {
            Err(windows::core::Error::empty())
        }
        fn GetResults(&self) -> windows::core::Result<i32> {
            Ok(self.result)
        }
    }

    // The progress-shape twin of `MockAsyncOp`: the two race tests for
    // `IAsyncOperationWithProgress<i32, u32>` (NeverCompletingProgress,
    // CompletesInTimeProgress) used to hand-roll the same
    // IAsyncInfo/IAsyncOperationWithProgress boilerplate this one struct now
    // serves. `fire_after` selects the behavior exactly like the plain mock.
    #[derive(Clone)]
    struct SendHandlerProgress(AsyncOperationWithProgressCompletedHandler<i32, u32>);
    // SAFETY: invoking the delegate from another thread runs its Send
    // closure there — the designed use of an async completion handler.
    unsafe impl Send for SendHandlerProgress {}

    // The progress delegate is not `Send` either (same IUnknown NonNull);
    // wrapping it for the firing thread follows the completed-handler
    // pattern — cross-thread invocation is the designed use.
    #[derive(Clone)]
    struct SendProgressHandler(AsyncOperationProgressHandler<i32, u32>);
    // SAFETY: invoking the delegate from another thread runs its Send
    // closure there — the designed use of an async progress handler.
    unsafe impl Send for SendProgressHandler {}

    /// State shared between the progress mock and its firing thread — same
    /// contract as `MockShared`, with the progress handler/operation types.
    /// `progress` holds the handler installed via SetProgress, so the firing
    /// thread can report progress while the wait blocks.
    struct MockSharedProgress {
        handler: Option<SendHandlerProgress>,
        progress: Option<SendProgressHandler>,
        op: Option<IAsyncOperationWithProgress<i32, u32>>,
    }

    /// One COM mock serving both `IAsyncOperationWithProgress<i32, u32>`
    /// races, mirroring `MockAsyncOp`: `fire_after: None` retains the
    /// completed handler and never invokes it (wedged read — the wait must
    /// time out); `Some(delay)` spawns a thread that invokes it `delay`
    /// after `SetCompleted` installs it (completion race — the wait must
    /// return `result`).
    #[implement(IAsyncOperationWithProgress<i32, u32>, IAsyncInfo)]
    struct MockAsyncOpProgress {
        fire_after: Option<Duration>,
        result: i32,
        shared: Arc<Mutex<MockSharedProgress>>,
    }

    impl MockAsyncOpProgress {
        // Same construction contract as `MockAsyncOp::new`: the COM mock is
        // consumed by `.into()` the moment it exists, so the construction
        // point must hand back the interface it produced rather than `Self`.
        #[allow(clippy::new_ret_no_self)]
        fn new(fire_after: Option<Duration>, result: i32) -> IAsyncOperationWithProgress<i32, u32> {
            let shared = Arc::new(Mutex::new(MockSharedProgress {
                handler: None,
                progress: None,
                op: None,
            }));
            let op: IAsyncOperationWithProgress<i32, u32> = MockAsyncOpProgress {
                fire_after,
                result,
                shared: shared.clone(),
            }
            .into();
            if fire_after.is_some() {
                // The firing thread must pass the completing operation to
                // Invoke, and the operation cannot know itself before
                // `.into()`, so the constructor wires the handle in once it
                // exists.
                shared.lock().unwrap().op = Some(op.clone());
            }
            op
        }
    }

    mock_async_info!(MockAsyncOpProgress_Impl, AsyncStatus::Started);

    impl IAsyncOperationWithProgress_Impl<i32, u32> for MockAsyncOpProgress_Impl {
        fn SetProgress(
            &self,
            handler: windows::core::Ref<AsyncOperationProgressHandler<i32, u32>>,
        ) -> windows::core::Result<()> {
            // Retain the progress handler alongside the completed handler so
            // the firing thread can report progress while the wait blocks;
            // the progress-report seam test drives this path.
            self.shared.lock().unwrap().progress = handler.ok().ok().cloned().map(SendProgressHandler);
            Ok(())
        }
        fn Progress(&self) -> windows::core::Result<AsyncOperationProgressHandler<i32, u32>> {
            Err(windows::core::Error::empty())
        }
        fn SetCompleted(
            &self,
            handler: windows::core::Ref<AsyncOperationWithProgressCompletedHandler<i32, u32>>,
        ) -> windows::core::Result<()> {
            let mut guard = self.shared.lock().unwrap();
            guard.handler = handler.ok().ok().cloned().map(SendHandlerProgress);
            drop(guard);
            if let Some(delay) = self.fire_after {
                let shared = self.shared.clone();
                // Fires on a detached background thread: a panic here unwinds
                // the thread and aborts the whole test process (an AV-shaped
                // crash, not a caught test failure). Return quietly if the
                // op/handler clones are already gone, and ignore every
                // `Invoke` result — the mock delegates are fire-and-forget.
                std::thread::spawn(move || {
                    // Report progress partway through the work, then
                    // complete: WinRT operations report progress as they
                    // run. The reports land while the wait is still
                    // blocking — the completion signal that wakes it is only
                    // sent at the very end, after both reports.
                    let third = delay / 3;
                    std::thread::sleep(third);
                    let (handler, op, progress) = {
                        let mut guard = shared.lock().unwrap();
                        match (guard.handler.clone(), guard.op.take(), guard.progress.clone()) {
                            (Some(handler), Some(op), progress) => (handler, op, progress),
                            _ => return,
                        }
                    };
                    if let Some(progress) = progress.as_ref() {
                        let _ = progress.0.Invoke(&op, 1);
                    }
                    std::thread::sleep(third);
                    if let Some(progress) = progress.as_ref() {
                        let _ = progress.0.Invoke(&op, 2);
                    }
                    std::thread::sleep(third);
                    let _ = handler.0.Invoke(&op, AsyncStatus::Completed);
                });
            }
            Ok(())
        }
        fn Completed(&self) -> windows::core::Result<AsyncOperationWithProgressCompletedHandler<i32, u32>> {
            Err(windows::core::Error::empty())
        }
        fn GetResults(&self) -> windows::core::Result<i32> {
            Ok(self.result)
        }
    }

    // ----------------------------------------------------------------------
    // Shared artwork-pipeline mocks.
    //
    // The read_thumbnail_from seam tests share one stream stack: a reference
    // whose OpenReadAsync returns the configured operation, a stream that
    // answers Size() and fills the real Buffer on ReadAsync, and a read
    // operation whose completion behavior is configurable (never completes
    // → the ReadAsync-timeout seam; fires after a delay → the success seam).
    // These were previously inlined per test; hoisting them here makes each
    // seam test a construction site instead of a second copy of the stack.
    // ----------------------------------------------------------------------
    use windows::Foundation::{IClosable, IClosable_Impl};
    use windows::Storage::Streams::{
        IBuffer, IContentTypeProvider, IContentTypeProvider_Impl, IInputStream, IInputStream_Impl, IOutputStream,
        IOutputStream_Impl, IRandomAccessStream, IRandomAccessStream_Impl, IRandomAccessStreamReference,
        IRandomAccessStreamReference_Impl, IRandomAccessStreamWithContentType, IRandomAccessStreamWithContentType_Impl,
        InputStreamOptions,
    };
    use windows::Win32::System::WinRT::IBufferByteAccess;

    /// A reference whose OpenReadAsync returns the configured operation —
    /// the seam tests pick ready (Completed) or never-completing per case.
    #[implement(IRandomAccessStreamReference)]
    struct MockReference {
        op: IAsyncOperation<IRandomAccessStreamWithContentType>,
    }
    impl IRandomAccessStreamReference_Impl for MockReference_Impl {
        fn OpenReadAsync(&self) -> windows::core::Result<IAsyncOperation<IRandomAccessStreamWithContentType>> {
            Ok(self.op.clone())
        }
    }

    /// The OpenReadAsync operation already completed: wait_async takes the
    /// fast path and GetResults hands the stream straight back.
    #[implement(IAsyncOperation<IRandomAccessStreamWithContentType>, IAsyncInfo)]
    struct ReadyStreamOp {
        stream: IRandomAccessStreamWithContentType,
    }
    mock_async_info!(ReadyStreamOp_Impl, AsyncStatus::Completed);
    impl IAsyncOperation_Impl<IRandomAccessStreamWithContentType> for ReadyStreamOp_Impl {
        fn SetCompleted(
            &self,
            _handler: windows::core::Ref<AsyncOperationCompletedHandler<IRandomAccessStreamWithContentType>>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn Completed(
            &self,
        ) -> windows::core::Result<AsyncOperationCompletedHandler<IRandomAccessStreamWithContentType>> {
            Err(windows::core::Error::empty())
        }
        fn GetResults(&self) -> windows::core::Result<IRandomAccessStreamWithContentType> {
            Ok(self.stream.clone())
        }
    }

    // The windows delegate is not `Send` (see the wait_async race mocks);
    // wrapping it for the firing thread is sound — the callback closure is
    // Send by contract, and cross-thread invocation is the designed use of a
    // completion handler.
    #[derive(Clone)]
    struct SendReadHandler(AsyncOperationWithProgressCompletedHandler<IBuffer, u32>);
    // SAFETY: invoking the delegate from another thread runs its Send
    // closure there — the designed use of an async completion handler.
    unsafe impl Send for SendReadHandler {}

    /// State shared between the read op and its firing thread: the retained
    /// completed handler, and the operation handle the thread passes to
    /// `Invoke` (wired by `MockStream::ReadAsync`, `take()`n back at fire
    /// time so the op never retains a self-reference past the fire).
    struct ReadShared {
        handler: Option<SendReadHandler>,
        op: Option<IAsyncOperationWithProgress<IBuffer, u32>>,
    }

    /// The ReadAsync operation, configurable like the wait mocks:
    /// `fire_after: None` retains the completed handler and never invokes it
    /// — the wedged read, so wait_async_progress times out; `Some(delay)`
    /// spawns a thread that invokes it `delay` after SetCompleted installs
    /// it — the completion race, so the wait wakes on the signal. The buffer
    /// the stream filled is what GetResults hands back.
    #[implement(IAsyncOperationWithProgress<IBuffer, u32>, IAsyncInfo)]
    struct MockProgressReadOp {
        buffer: IBuffer,
        fire_after: Option<Duration>,
        shared: Arc<Mutex<ReadShared>>,
    }
    mock_async_info!(MockProgressReadOp_Impl, AsyncStatus::Started);
    impl IAsyncOperationWithProgress_Impl<IBuffer, u32> for MockProgressReadOp_Impl {
        fn SetProgress(
            &self,
            _handler: windows::core::Ref<AsyncOperationProgressHandler<IBuffer, u32>>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn Progress(&self) -> windows::core::Result<AsyncOperationProgressHandler<IBuffer, u32>> {
            Err(windows::core::Error::empty())
        }
        fn SetCompleted(
            &self,
            handler: windows::core::Ref<AsyncOperationWithProgressCompletedHandler<IBuffer, u32>>,
        ) -> windows::core::Result<()> {
            let mut guard = self.shared.lock().unwrap();
            guard.handler = handler.ok().ok().cloned().map(SendReadHandler);
            drop(guard);
            if let Some(delay) = self.fire_after {
                let shared = self.shared.clone();
                // See the completed-handler mock: a panic on this detached
                // thread aborts the test process. Return quietly if the
                // op/handler clones are already gone, and ignore `Invoke`'s
                // result.
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    let (handler, op) = {
                        let mut guard = shared.lock().unwrap();
                        match (guard.handler.clone(), guard.op.take()) {
                            (Some(handler), Some(op)) => (handler, op),
                            _ => return,
                        }
                    };
                    let _ = handler.0.Invoke(&op, AsyncStatus::Completed);
                });
            }
            Ok(())
        }
        fn Completed(&self) -> windows::core::Result<AsyncOperationWithProgressCompletedHandler<IBuffer, u32>> {
            Err(windows::core::Error::empty())
        }
        fn GetResults(&self) -> windows::core::Result<IBuffer> {
            Ok(self.buffer.clone())
        }
    }

    /// A stream whose Size() answers and whose ReadAsync fills the real
    /// Buffer (through its byte-access interface) and returns a
    /// `MockProgressReadOp` with the configured behavior — `None` for the
    /// ReadAsync-timeout seam (the op never completes), `Some(delay)` for
    /// the success seam (the op fires `delay` after install).
    #[implement(
        IRandomAccessStreamWithContentType,
        IRandomAccessStream,
        IContentTypeProvider,
        IInputStream,
        IOutputStream,
        IClosable
    )]
    struct MockStream {
        bytes: Vec<u8>,
        read_fire_after: Option<Duration>,
    }
    impl IClosable_Impl for MockStream_Impl {
        fn Close(&self) -> windows::core::Result<()> {
            Ok(())
        }
    }
    impl IContentTypeProvider_Impl for MockStream_Impl {
        fn ContentType(&self) -> windows::core::Result<windows::core::HSTRING> {
            Ok(windows::core::HSTRING::from("image/png"))
        }
    }
    impl IInputStream_Impl for MockStream_Impl {
        fn ReadAsync(
            &self,
            buffer: windows::core::Ref<IBuffer>,
            count: u32,
            _options: InputStreamOptions,
        ) -> windows::core::Result<windows_future::IAsyncOperationWithProgress<IBuffer, u32>> {
            let ibuf = buffer.ok().expect("the pipeline must pass a buffer");
            // Fill the real buffer through its byte-access interface and
            // report the filled length, so the pipeline's DataReader reads
            // the artwork back (success seam; harmless on the timeout seam,
            // whose wait never reaches the read).
            let access: IBufferByteAccess = ibuf.cast()?;
            let data = unsafe { access.Buffer()? };
            let n = count.min(self.bytes.len() as u32);
            unsafe { std::ptr::copy_nonoverlapping(self.bytes.as_ptr(), data, n as usize) };
            ibuf.SetLength(n)?;
            let shared = Arc::new(Mutex::new(ReadShared {
                handler: None,
                op: None,
            }));
            let op: IAsyncOperationWithProgress<IBuffer, u32> = MockProgressReadOp {
                buffer: ibuf.clone(),
                fire_after: self.read_fire_after,
                shared: shared.clone(),
            }
            .into();
            if self.read_fire_after.is_some() {
                // The firing thread must pass the completing operation to
                // Invoke, and the operation cannot know itself before
                // `.into()`, so wire the handle in once it exists.
                shared.lock().unwrap().op = Some(op.clone());
            }
            Ok(op)
        }
    }
    impl IOutputStream_Impl for MockStream_Impl {
        fn WriteAsync(
            &self,
            _buffer: windows::core::Ref<IBuffer>,
        ) -> windows::core::Result<windows_future::IAsyncOperationWithProgress<u32, u32>> {
            // Unreachable on the read path.
            Err(windows::core::Error::empty())
        }
        fn FlushAsync(&self) -> windows::core::Result<windows_future::IAsyncOperation<bool>> {
            Err(windows::core::Error::empty())
        }
    }
    impl IRandomAccessStream_Impl for MockStream_Impl {
        fn Size(&self) -> windows::core::Result<u64> {
            Ok(self.bytes.len() as u64)
        }
        fn SetSize(&self, _value: u64) -> windows::core::Result<()> {
            Ok(())
        }
        fn GetInputStreamAt(&self, _position: u64) -> windows::core::Result<IInputStream> {
            Err(windows::core::Error::empty())
        }
        fn GetOutputStreamAt(&self, _position: u64) -> windows::core::Result<IOutputStream> {
            Err(windows::core::Error::empty())
        }
        fn Position(&self) -> windows::core::Result<u64> {
            Ok(0)
        }
        fn Seek(&self, _position: u64) -> windows::core::Result<()> {
            Ok(())
        }
        fn CloneStream(&self) -> windows::core::Result<IRandomAccessStream> {
            Err(windows::core::Error::empty())
        }
        fn CanRead(&self) -> windows::core::Result<bool> {
            Ok(true)
        }
        fn CanWrite(&self) -> windows::core::Result<bool> {
            Ok(false)
        }
    }
    impl IRandomAccessStreamWithContentType_Impl for MockStream_Impl {}

    /// A stream operation that is alive (handler installed, channel open)
    /// yet never completes — the wedged OpenReadAsync, so wait_async times
    /// out and the AsyncReadTimeout marker surfaces. Used by the
    /// OpenReadAsync timeout seam and the artwork-retry seam's wedged case.
    #[implement(IAsyncOperation<IRandomAccessStreamWithContentType>, IAsyncInfo)]
    struct MockNeverCompletingStreamOp {
        // Retained but never invoked — the operation is alive (channel
        // open) yet never completes, exactly like the wait_async mocks.
        completed: std::sync::Mutex<Option<AsyncOperationCompletedHandler<IRandomAccessStreamWithContentType>>>,
    }
    mock_async_info!(MockNeverCompletingStreamOp_Impl, AsyncStatus::Started);
    impl IAsyncOperation_Impl<IRandomAccessStreamWithContentType> for MockNeverCompletingStreamOp_Impl {
        fn SetCompleted(
            &self,
            handler: windows::core::Ref<AsyncOperationCompletedHandler<IRandomAccessStreamWithContentType>>,
        ) -> windows::core::Result<()> {
            *self.completed.lock().unwrap() = handler.ok().ok().cloned();
            Ok(())
        }
        fn Completed(
            &self,
        ) -> windows::core::Result<AsyncOperationCompletedHandler<IRandomAccessStreamWithContentType>> {
            Err(windows::core::Error::empty())
        }
        fn GetResults(&self) -> windows::core::Result<IRandomAccessStreamWithContentType> {
            // Never reached on the timeout path.
            Err(windows::core::Error::empty())
        }
    }

    /// Drives one wait shape through its macro expansion with the operation
    /// already built, and asserts the timeout contract both shapes share:
    /// the wait blocks the full bound and the AsyncReadTimeout marker
    /// survives unchanged — the Started status plus the retained handler are
    /// what make it block, and the elapsed floor is what pins that, not the
    /// marker alone. `wait` is the shape's macro entry (`wait_async` or
    /// `wait_async_progress`) closed over its mock operation; both shapes
    /// answer `i32`, so the one driver serves all four wait tests.
    fn assert_wait_times_out(wait: impl FnOnce(Option<Duration>) -> anyhow::Result<i32>) {
        const BOUND: Duration = Duration::from_millis(100);
        let started = std::time::Instant::now();
        let err = wait(Some(BOUND)).expect_err("a never-signalling operation must time out");
        assert!(
            started.elapsed() >= BOUND,
            "the wait must actually block for the bound before timing out"
        );
        assert!(
            is_wait_timeout(&err),
            "the AsyncReadTimeout marker must survive the macro unchanged"
        );
    }

    /// Drives one wait shape whose mock fires FIRE_AFTER into the BOUND and
    /// asserts the completion contract both shapes share: the wait wakes on
    /// the completion signal — well before the bound expires — and
    /// GetResults answers `expected`, pinning the *value* flow through the
    /// macro, not just Ok. A lost signal would race the deadline instead.
    fn assert_wait_completes_with(expected: i32, wait: impl FnOnce(Option<Duration>) -> anyhow::Result<i32>) {
        const BOUND: Duration = Duration::from_millis(500);
        let started = std::time::Instant::now();
        let result = wait(Some(BOUND)).expect("a completed wait must return the result, not the timeout marker");
        assert_eq!(
            result, expected,
            "GetResults must answer the mock's value after the handler fired"
        );
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "the wait must complete on the signal, well before the 500 ms bound"
        );
    }

    #[test]
    fn wait_async_timeout_marker_flows_through_the_macro_unchanged() {
        // Drives wait_outcome through the FULL wait_async_op macro expansion
        // with a simulated never-signalling operation. The shared mock
        // (fire_after = None) reports Started and retains the completion
        // handler without ever invoking it — the retained handler keeps the
        // signal channel open, so the wait must block for the whole bound,
        // and the error that propagates out of the macro must be the
        // AsyncReadTimeout marker is_wait_timeout recognizes, unchanged
        // through the `?`.
        let op = MockAsyncOp::new(None, 0);
        assert_wait_times_out(|bound| wait_async(&op, bound));
    }

    #[test]
    fn wait_async_progress_timeout_marker_flows_through_the_macro_unchanged() {
        // The same wedged-read contract for the progress shape: the shared
        // progress mock (fire_after = None) retains the completed handler
        // without ever invoking it, so the wait times out and the
        // AsyncReadTimeout marker must propagate out of wait_async_progress
        // — the IAsyncOperationWithProgress<TResult, u32> macro expansion —
        // unchanged, recognized by is_wait_timeout to drive the per-source
        // exclusion.
        let op = MockAsyncOpProgress::new(None, 0);
        assert_wait_times_out(|bound| wait_async_progress(&op, bound));
    }

    #[test]
    fn wait_async_returns_the_result_when_the_handler_fires_before_the_timeout() {
        // The completion race won: the shared mock's firing thread invokes
        // the retained handler FIRE_AFTER into the wait bound, so
        // recv_timeout returns the signal and wait_async proceeds to
        // GetResults — the result comes back and the source is never
        // excluded. If the signal were lost or the wait misread a completed
        // operation, the bound would expire and the AsyncReadTimeout marker
        // would surface instead.
        let op = MockAsyncOp::new(Some(Duration::from_millis(300)), 0);
        assert_wait_completes_with(0, |bound| wait_async(&op, bound));
    }

    #[test]
    fn wait_async_progress_returns_the_result_when_the_handler_fires_before_the_timeout() {
        // The progress shape's completion race won: the shared mock's firing
        // thread invokes the retained completed handler FIRE_AFTER into the
        // wait bound, so recv_timeout returns the signal and
        // wait_async_progress proceeds to GetResults — the value comes back
        // and the source is never excluded. The opposite outcome (the wedged
        // read) is pinned by the sibling progress timeout test; this one
        // proves the result survives the macro expansion too.
        let op = MockAsyncOpProgress::new(Some(Duration::from_millis(300)), 7);
        assert_wait_completes_with(7, |bound| wait_async_progress(&op, bound));
    }

    #[test]
    fn wait_async_progress_reports_progress_while_the_wait_blocks() {
        // The progress side of IAsyncOperationWithProgress, driven through
        // the shared mock: the test installs a progress handler via
        // SetProgress, and while wait_async_progress blocks for the
        // operation, the mock's firing thread reports progress partway
        // through (1 then 2) before completing. The reports must actually
        // reach the installed handler — a no-op SetProgress would drop them,
        // leaving the sequence empty — and the first must land before the
        // wait returns, i.e. while it is still blocking.
        const BOUND: Duration = Duration::from_millis(500);
        const FIRE_AFTER: Duration = Duration::from_millis(300);

        let reported = Arc::new(Mutex::new((None::<Instant>, Vec::<u32>::new())));
        let op = MockAsyncOpProgress::new(Some(FIRE_AFTER), 7);
        let progress_handler = AsyncOperationProgressHandler::new({
            let reported = reported.clone();
            move |_op, progress| {
                let mut guard = reported.lock().unwrap();
                if guard.0.is_none() {
                    guard.0 = Some(Instant::now());
                }
                guard.1.push(*progress);
                Ok(())
            }
        });
        op.SetProgress(&progress_handler)
            .expect("the progress handler must install");

        let started = Instant::now();
        let result = wait_async_progress(&op, Some(BOUND))
            .expect("a completed progress wait must return the result, not the timeout marker");
        let wait_returned = Instant::now();
        assert_eq!(result, 7, "GetResults answers 7 after the handler fired");
        let (first_report, values) = &*reported.lock().unwrap();
        assert_eq!(
            *values,
            vec![1, 2],
            "the progress reports must reach the installed handler"
        );
        assert!(
            first_report.is_some_and(|at| at < wait_returned),
            "the first progress report must land while the wait is still blocking"
        );
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "the wait must complete on the signal, well before the 500 ms bound"
        );
    }

    #[test]
    fn is_wait_timeout_recognizes_the_marker_under_read_thumbnail_contexts() {
        // read_thumbnail_from wraps the marker in contexts ("OpenReadAsync
        // get failed"); the exclusion check must still recognize it — the
        // seam test proves the same end-to-end, this pins the unit contract.
        let wrapped = anyhow::Error::new(AsyncReadTimeout { secs: 10 }).context("OpenReadAsync get failed");
        assert!(is_wait_timeout(&wrapped), "the marker must be found under the context");
        assert!(is_wait_timeout(&wrapped.context("a second wrap")));
    }

    #[test]
    fn read_thumbnail_openreadasync_timeout_flows_through_the_pipeline() {
        // The artwork pipeline's wedged read, driven through the real
        // read_thumbnail_from: the mocked reference's OpenReadAsync returns
        // a never-completing operation, so the wait times out and the error
        // must come back through the pipeline's context wraps still
        // recognizable as the AsyncReadTimeout marker — read_track_info
        // relies on that to surface it and exclude the source instead of
        // retrying a hung read.
        // The reference and its never-completing operation are the shared
        // stream-stack mocks (MockReference, MockNeverCompletingStreamOp).
        let op: IAsyncOperation<IRandomAccessStreamWithContentType> = MockNeverCompletingStreamOp {
            completed: std::sync::Mutex::new(None),
        }
        .into();
        let reference: IRandomAccessStreamReference = MockReference { op }.into();
        let started = std::time::Instant::now();
        let err = read_thumbnail_from(&reference, Some(Duration::from_millis(100)))
            .expect_err("a wedged OpenReadAsync must surface as a timeout");
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the wait must actually block for the bound before timing out"
        );
        assert!(
            is_wait_timeout(&err),
            "the marker must survive read_thumbnail_from's context wraps"
        );
    }

    #[test]
    fn read_thumbnail_completion_flows_through_the_pipeline() {
        // The success path end to end, through the real read_thumbnail_from
        // and the shared stream stack: OpenReadAsync is already completed
        // (wait_async fast path), the stream answers Size() and fills the
        // real Buffer on ReadAsync, and ReadAsync returns a Started op whose
        // handler fires FIRE_AFTER later — so wait_async_progress actually
        // blocks and wakes on the completion signal, the progress-wait shape
        // running through the pipeline rather than the Completed-skip fast
        // path.
        const BOUND: Duration = Duration::from_millis(100);
        // ReadAsync's handler fires well inside the bound (30ms of 100ms),
        // so the wait returns the completion signal instead of racing the
        // deadline, while the timing floor below still proves it blocked.
        const FIRE_AFTER: Duration = Duration::from_millis(30);

        // The artwork the stream hands out: 128 bytes, within the accepted
        // thumbnail range and distinctive enough to assert byte-for-byte.
        let artwork: Vec<u8> = (0..128u8).collect();
        let stream: IRandomAccessStreamWithContentType = MockStream {
            bytes: artwork.clone(),
            read_fire_after: Some(FIRE_AFTER),
        }
        .into();
        let op: IAsyncOperation<IRandomAccessStreamWithContentType> = ReadyStreamOp { stream: stream.clone() }.into();
        let reference: IRandomAccessStreamReference = MockReference { op }.into();

        // The pipeline activates real WinRT objects (Buffer, DataReader),
        // which need an initialized apartment on this thread — the same
        // MTA convention the worker uses.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok();
        let started = std::time::Instant::now();
        let result = read_thumbnail_from(&reference, Some(BOUND));
        let call_elapsed = started.elapsed();
        unsafe { CoUninitialize() };

        match result {
            Ok(Some(bytes)) => {
                assert_eq!(bytes, artwork, "the artwork must flow through the full pipeline");
                // The ReadAsync wait must actually block until the operation
                // fires (~FIRE_AFTER): if the op reported Completed, the wait
                // would be skipped and the call would return in well under
                // this floor even though the bytes still come back.
                assert!(
                    call_elapsed >= Duration::from_millis(20),
                    "the ReadAsync wait must block for the operation's completion signal"
                );
            }
            other => panic!("the pipeline must return the artwork, got {other:?}"),
        }
    }

    #[test]
    fn read_thumbnail_readasync_timeout_flows_through_the_pipeline() {
        // The second artwork wait, closed: OpenReadAsync succeeds (the
        // completed fast path), Size() is acceptable, and ReadAsync returns
        // a never-completing progress operation (read_fire_after = None) —
        // so wait_async_progress blocks for the whole bound and the
        // AsyncReadTimeout marker must come back through the pipeline's
        // "ReadAsync get failed" context wraps, exactly the way the
        // OpenReadAsync seam pins its side.
        let stream: IRandomAccessStreamWithContentType = MockStream {
            bytes: vec![0xAB; 128],
            read_fire_after: None,
        }
        .into();
        let op: IAsyncOperation<IRandomAccessStreamWithContentType> = ReadyStreamOp { stream: stream.clone() }.into();
        let reference: IRandomAccessStreamReference = MockReference { op }.into();

        // Buffer::Create runs before ReadAsync, so the pipeline needs an
        // apartment even though the wedged read never produces artwork.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok();
        let started = std::time::Instant::now();
        let err = read_thumbnail_from(&reference, Some(Duration::from_millis(100)))
            .expect_err("a wedged ReadAsync must surface as a timeout");
        unsafe { CoUninitialize() };
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the wait must actually block for the bound before timing out"
        );
        assert!(
            is_wait_timeout(&err),
            "the marker must survive read_thumbnail_from's ReadAsync context wraps"
        );
    }

    #[test]
    fn read_artwork_with_retry_recovers_after_a_transient_first_attempt() {
        // The attempt-1-then-retry behavior, end-to-end through the shared
        // stream stack: attempt 1 fails transiently (like a Thumbnail fetch
        // error under session churn), attempt 2 succeeds — the artwork comes
        // back and the retry ran exactly once.
        const FIRE_AFTER: Duration = Duration::from_millis(5);
        let artwork: Vec<u8> = (0..128u8).collect();
        let stream: IRandomAccessStreamWithContentType = MockStream {
            bytes: artwork.clone(),
            read_fire_after: Some(FIRE_AFTER),
        }
        .into();
        let op: IAsyncOperation<IRandomAccessStreamWithContentType> = ReadyStreamOp { stream: stream.clone() }.into();
        let reference: IRandomAccessStreamReference = MockReference { op }.into();
        let attempts = std::cell::Cell::new(0u32);
        // The healthy attempt activates real WinRT objects (Buffer,
        // DataReader), which need an initialized apartment on this thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok();
        let result = read_artwork_with_retry(|| {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                // A transient Thumbnail-fetch failure: retryable, neither a
                // session-gone error nor the wedged marker.
                Err(anyhow!("Thumbnail failed (transient)"))
            } else {
                read_thumbnail_from(&reference, Some(Duration::from_millis(100)))
            }
        });
        unsafe { CoUninitialize() };
        assert_eq!(attempts.get(), 2, "a transient failure must trigger exactly one retry");
        match result {
            Ok(Some(bytes)) => assert_eq!(bytes, artwork, "the retried read must return the artwork"),
            other => panic!("the retried read must succeed, got {other:?}"),
        }
    }

    #[test]
    fn read_artwork_with_retry_surfaces_a_wedged_first_attempt_without_retrying() {
        // A wedged reference (OpenReadAsync never completes) is definitive,
        // not a transient failure: the marker surfaces and the attempt
        // closure is NOT called again — the source is excluded instead of
        // retrying a hung read.
        let op: IAsyncOperation<IRandomAccessStreamWithContentType> = MockNeverCompletingStreamOp {
            completed: std::sync::Mutex::new(None),
        }
        .into();
        let reference: IRandomAccessStreamReference = MockReference { op }.into();
        let attempts = std::cell::Cell::new(0u32);
        let started = std::time::Instant::now();
        let err = read_artwork_with_retry(|| {
            attempts.set(attempts.get() + 1);
            read_thumbnail_from(&reference, Some(Duration::from_millis(100)))
        })
        .expect_err("a wedged first attempt must surface as a timeout");
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the wait must actually block for the bound before timing out"
        );
        assert!(is_wait_timeout(&err), "the marker must drive the exclusion");
        assert_eq!(attempts.get(), 1, "a wedged read must not be retried");
    }

    #[test]
    fn read_artwork_with_retry_returns_none_after_two_transient_failures() {
        let attempts = std::cell::Cell::new(0u32);
        let result = read_artwork_with_retry(|| {
            attempts.set(attempts.get() + 1);
            Err(anyhow!("Thumbnail failed (transient #{})", attempts.get()))
        });
        assert_eq!(attempts.get(), 2, "two transient failures consume the retry budget");
        match result {
            Ok(None) => {}
            other => panic!("two transient failures must yield None, got {other:?}"),
        }
    }

    #[test]
    fn read_artwork_with_retry_surfaces_a_wedged_second_attempt() {
        // Transient attempt 1, wedged attempt 2: the retry runs, and the
        // second attempt's marker surfaces — a retry never masks a wedged
        // read.
        let op: IAsyncOperation<IRandomAccessStreamWithContentType> = MockNeverCompletingStreamOp {
            completed: std::sync::Mutex::new(None),
        }
        .into();
        let reference: IRandomAccessStreamReference = MockReference { op }.into();
        let attempts = std::cell::Cell::new(0u32);
        let err = read_artwork_with_retry(|| {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                Err(anyhow!("Thumbnail failed (transient)"))
            } else {
                read_thumbnail_from(&reference, Some(Duration::from_millis(100)))
            }
        })
        .expect_err("a wedged second attempt must surface as a timeout");
        assert_eq!(
            attempts.get(),
            2,
            "the retry must run before the wedged attempt surfaces"
        );
        assert!(
            is_wait_timeout(&err),
            "the second attempt's marker must drive the exclusion"
        );
    }
}

/// Deterministic hostile-Unicode fuzz sweeps over the dedup/identity
/// comparisons and the log path — the layer below the `cap_meta` sweep in
/// `mod tests`. Everything SMTC reads from other processes (title, artist,
/// album, genre, the AUMID-derived source label) is sanitized by `cap_meta`
/// at the worker boundary; these sweeps push that sanitized output — plus the
/// raw hostile strings themselves — through every comparison that decides
/// "same track / recreated session / emit vs. suppress / cache identity", and
/// through the log-line composition, asserting the downstream invariants:
///
/// - no panic on any input (a panic fails the test outright);
/// - no growth: every output below `cap_meta` stays bounded by a constant
///   independent of the raw input length (the 256-char cap is the choke
///   point), so an arbitrarily long hostile title cannot bloat history rows,
///   tooltips, pill text, marquee strips, or log lines;
/// - no log injection: no ASCII newline or carriage return can reach a log
///   line — neither through `track_label`'s `{:?}` escaping nor through
///   `log_preview`'s `escape_debug` — so hostile metadata cannot forge or
///   split log records.
///
/// Same deterministic xorshift64 convention as `mod tests::FuzzRng` and the
/// `icon.rs` grammar sweeps: fixed seeds reproduce any failure exactly and
/// the tests never depend on ambient randomness.
#[cfg(test)]
mod hostile_identity_fuzz {
    use super::*;
    use crate::events::artwork_same;

    /// Deterministic xorshift64 (the shared fuzz-sweep convention).
    struct FuzzRng(u64);

    impl FuzzRng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn pick(&mut self, alphabet: &[char]) -> char {
            alphabet[(self.next() as usize) % alphabet.len()]
        }

        /// A uniform random Unicode scalar, excluding surrogate halves (which
        /// cannot appear in a Rust `&str` anyway). Covers rare scripts,
        /// control planes and formatting chars the curated alphabet misses.
        fn scalar(&mut self) -> char {
            loop {
                let value = (self.next() % 0x11_0000) as u32;
                if !(0xD800..=0xDFFF).contains(&value) {
                    return char::from_u32(value).unwrap_or('\u{FFFD}');
                }
            }
        }
    }

    /// Characters every generated string draws from: display-unsafe
    /// controls, bidi commands, whitespace, line/paragraph separators,
    /// benign ASCII (including path punctuation), and benign Unicode
    /// (accented, CJK, emoji, ZWJ, RTL letters, combining marks).
    const HOSTILE: &[char] = &[
        // display-unsafe: C0, DEL, C1, bidi commands.
        '\u{0}', '\u{1}', '\u{7}', '\u{1F}', '\u{7F}', '\u{80}', '\u{85}', '\u{9F}', '\u{202A}', '\u{202E}', '\u{2066}',
        '\u{2069}', // whitespace and Zl/Zp separators (trim boundaries, interior).
        ' ', '\t', '\n', '\r', '\u{2009}', '\u{2028}', '\u{2029}', // benign ASCII.
        'a', 'z', 'A', 'Z', '0', '9', '.', '-', '_', '!', '/', '\\', ':', '?', '*', '"', '<', '>',
        // benign Unicode.
        'é', '你', '🎵', '\u{200D}', 'א', '\u{301}',
    ];

    /// A random string of 0..320 chars from the curated hostile alphabet, so
    /// the corpus straddles the 256-char cap, the empty string, and the trim
    /// path.
    fn hostile_string(rng: &mut FuzzRng) -> String {
        let len = (rng.next() % 320) as usize;
        (0..len).map(|_| rng.pick(HOSTILE)).collect()
    }

    /// A random string of 0..320 uniform Unicode scalars — the broad plane
    /// sweep that complements the curated alphabet.
    fn hostile_scalars(rng: &mut FuzzRng) -> String {
        let len = (rng.next() % 320) as usize;
        (0..len).map(|_| rng.scalar()).collect()
    }

    /// Builds a `TrackInfo` whose every metadata field is the capped
    /// sanitization of an independent hostile string, with random artwork
    /// presence — the shape a hostile source's read produces after the
    /// worker boundary.
    fn hostile_track(rng: &mut FuzzRng, source_label: &str) -> TrackInfo {
        let artwork = if rng.next() & 1 == 0 {
            None
        } else {
            // Arbitrary bytes: hostile "cover" data is opaque input too.
            let len = (rng.next() % 64) as usize;
            Some(Arc::from((0..len).map(|_| rng.next() as u8).collect::<Vec<u8>>()))
        };
        TrackInfo {
            title: cap_meta(hostile_string(rng)),
            artist: cap_meta(hostile_string(rng)),
            album: cap_meta(hostile_string(rng)),
            album_artist: cap_meta(hostile_string(rng)),
            subtitle: cap_meta(hostile_string(rng)),
            genre: Some(cap_meta(hostile_string(rng))),
            source_app: source_label.to_string(),
            duration_secs: Some(rng.next() % 1_000_000),
            track_number: Some((rng.next() % 100_000) as u32),
            track_count: Some((rng.next() % 100_000) as u32),
            artwork,
            ..TrackInfo::default()
        }
    }

    /// The `LogicalState` counterpart of a track, mirroring the fields
    /// `content_differ`/`emit_track` compare so those comparisons run against
    /// real values rather than defaults.
    fn state_from(track: &TrackInfo) -> LogicalState {
        LogicalState {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            album_artist: track.album_artist.clone(),
            subtitle: track.subtitle.clone(),
            source_app: track.source_app.clone(),
            duration_secs: track.duration_secs,
            track_number: track.track_number,
            track_count: track.track_count,
            genre: track.genre.clone(),
            has_artwork: track.artwork.is_some(),
            ..LogicalState::default()
        }
    }

    /// Asserts the cap_meta contract on every string field of a track: no
    /// display-unsafe character (the log/display injection invariant) and no
    /// value past the character cap (the no-growth invariant).
    fn assert_sane_fields(track: &TrackInfo, context: &str) {
        for (name, value) in [
            ("title", &track.title),
            ("artist", &track.artist),
            ("album", &track.album),
            ("album_artist", &track.album_artist),
            ("subtitle", &track.subtitle),
            ("source_app", &track.source_app),
        ] {
            assert!(
                value.chars().all(|c| !display_unsafe(c)),
                "{context}: {name} carries a display-unsafe char: {value:?}"
            );
            assert!(
                value.chars().count() <= MAX_META_CHARS,
                "{context}: {name} exceeds the {MAX_META_CHARS}-char cap ({})",
                value.chars().count()
            );
        }
    }

    #[test]
    fn identity_comparisons_are_total_and_deterministic_on_hostile_metadata() {
        // Every dedup/identity comparison must answer for any hostile
        // metadata (no panic), agree with itself on a second call
        // (deterministic), and never let a display-unsafe or over-cap value
        // into stored/merged state.
        let mut rng = FuzzRng(0x0DEA_DBEE_0DEA_DBEE);
        for iteration in 0..2000 {
            let source_a = source_app_label(&hostile_string(&mut rng));
            let source_b = source_app_label(&hostile_string(&mut rng));
            let a = hostile_track(&mut rng, &source_a);
            let b = hostile_track(&mut rng, &source_b);
            let context = format!("iteration {iteration}");

            assert_sane_fields(&a, &context);
            assert_sane_fields(&b, &context);
            assert!(
                source_a.chars().all(|c| !display_unsafe(c)),
                "{context}: source label is dirty: {source_a:?}"
            );
            assert!(
                source_b.chars().all(|c| !display_unsafe(c)),
                "{context}: source label is dirty: {source_b:?}"
            );

            // Content-diff and session-recreation dedup: total, deterministic.
            let diff = content_differ(&state_from(&a), &b);
            assert_eq!(
                content_differ(&state_from(&a), &b),
                diff,
                "{context}: content_differ is nondeterministic"
            );
            let recreation = is_session_recreation(&a, &b, true);
            assert_eq!(
                is_session_recreation(&a, &b, true),
                recreation,
                "{context}: is_session_recreation is nondeterministic"
            );
            assert_eq!(
                is_session_recreation(&a, &b, false),
                is_session_recreation(&a, &b, false),
                "{context}: is_session_recreation(false) is nondeterministic"
            );

            // Emit/suppress decisions, stale-thumbnail pairing, placeholder
            // and churn gates: total over hostile metadata (no panic).
            let _ = should_suppress_recreation(Some(&a), &b, true, Some(&source_b));
            let _ = emit_track(&state_from(&a), &b, true);
            let _ = stale_thumbnail(&b, Some(&a));
            let _ = is_placeholder_like(&b);
            let _ = first_read_counts_toward_churn(true, &b);
            let _ = merge_track(&state_from(&a), &b, true);

            // The overlay's same-media dedup decision: reflexive and
            // symmetric for any hostile pair.
            assert!(a.same_media(&a), "{context}: same_media is not reflexive");
            assert_eq!(
                a.same_media(&b),
                b.same_media(&a),
                "{context}: same_media is asymmetric"
            );
            let _ = artwork_same(a.artwork.as_deref(), b.artwork.as_deref());

            // A late-metadata merge must stay within the cap and keep every
            // field clean (no growth, no re-introduced hostile char).
            let mut merged = a.clone();
            merged.merge_late_metadata(&b);
            assert_sane_fields(&merged, &context);

            // Allow-list normalization: total, deterministic, and bounded
            // (to_lowercase can expand by at most a small factor; the strip
            // only shrinks), so a huge hostile title cannot blow it up.
            let norm = normalize_for_match(&b.title);
            assert_eq!(
                normalize_for_match(&b.title),
                norm,
                "{context}: normalize_for_match is nondeterministic"
            );
            assert!(
                norm.len() <= b.title.len() * 4,
                "{context}: normalize_for_match grew the input ({} -> {})",
                b.title.len(),
                norm.len()
            );
            assert!(
                norm.chars().all(|c| !display_unsafe(c)),
                "{context}: normalize_for_match produced a display-unsafe char: {norm:?}"
            );
        }
    }

    #[test]
    fn identity_comparisons_are_total_on_uniform_unicode_scalars() {
        // The broad-plane complement to the curated-alphabet sweep above:
        // arbitrary Unicode scalars (rare scripts, exotic controls, unpaired
        // combining marks) through the same pipeline with the same
        // invariants.
        let mut rng = FuzzRng(0x5EED_CAFE_5EED_CAFE);
        for iteration in 0..1000 {
            let source = source_app_label(&hostile_scalars(&mut rng));
            let a = hostile_track(&mut rng, &source);
            let b = hostile_track(&mut rng, &source);
            let context = format!("iteration {iteration}");

            assert_sane_fields(&a, &context);
            assert_sane_fields(&b, &context);
            let _ = content_differ(&state_from(&a), &b);
            let _ = is_session_recreation(&a, &b, true);
            let _ = emit_track(&state_from(&a), &b, true);
            let _ = stale_thumbnail(&b, Some(&a));
            let _ = merge_track(&state_from(&a), &b, true);
            let _ = a.same_media(&b);
            let mut merged = a.clone();
            merged.merge_late_metadata(&b);
            assert_sane_fields(&merged, &context);
        }
    }

    #[test]
    fn log_lines_cannot_be_injected_through_hostile_metadata() {
        // A log record is a `\n`-terminated line; injection means hostile
        // metadata forging a new record or altering a boundary. The layers
        // that carry metadata into the log are `track_label` (the worker's
        // `track changed | ...` line, Debug-escaped) and `log_preview`
        // (bounded escaped previews for raw values). Neither may ever emit a
        // raw newline or carriage return, and both must stay bounded
        // regardless of the raw input length. Debug-format escaping of the
        // raw string is the belt-and-braces layer for any direct
        // `format!("{value:?}")` log site.
        let mut rng = FuzzRng(0xFEED_FACE_FEED_FACE);
        for iteration in 0..2000 {
            let raw = hostile_string(&mut rng);
            let source = source_app_label(&raw.clone());
            let track = hostile_track(&mut rng, &source);
            let context = format!("iteration {iteration}");

            // `track changed | title=.. | artist=.. | ...` line: Debug
            // escapes everything, so no raw line break can ride in.
            let label = track_label(&track);
            assert!(
                !label.contains('\n') && !label.contains('\r'),
                "{context}: track_label carries a raw newline: {label:?}"
            );
            assert!(
                label.len() <= 20_000,
                "{context}: track_label grew unbounded ({} bytes)",
                label.len()
            );

            // The pill/history meta line is composed from the same capped
            // fields: bounded and line-break-free.
            let line = track.meta_line(true);
            assert!(
                !line.contains('\n') && !line.contains('\r'),
                "{context}: meta_line carries a raw newline: {line:?}"
            );
            assert!(
                line.len() <= 4_096,
                "{context}: meta_line grew unbounded ({} bytes)",
                line.len()
            );

            // Raw values only ever enter the log through log_preview, which
            // escapes and caps them: the preview represents at most `cap`
            // input scalar values, each escaped to at most 10 characters
            // (`\u{10ffff}`), so the preview string — and thus the log line
            // — is bounded by `cap * 10` chars no matter how long the raw
            // input is. It must also carry no line breaks, and its omitted
            // count must account for every scalar beyond the cap.
            let (preview, omitted) = crate::winutil::log_preview(&raw, 128);
            assert!(
                preview.chars().count() <= 128 * 10,
                "{context}: log_preview exceeded its {} * 10 char bound ({} chars)",
                128,
                preview.chars().count()
            );
            assert!(
                !preview.contains('\n') && !preview.contains('\r'),
                "{context}: log_preview carries a raw newline: {preview:?}"
            );
            let count = raw.chars().count();
            assert_eq!(
                omitted,
                count.saturating_sub(128),
                "{context}: log_preview's omitted count is inconsistent ({count} chars in)"
            );

            // Debug formatting of the raw string itself (any future direct
            // log site) never emits a raw line break.
            let debug = format!("{raw:?}");
            assert!(
                !debug.contains('\n') && !debug.contains('\r'),
                "{context}: Debug formatting leaked a raw newline"
            );
        }
    }

    #[test]
    fn cached_artwork_identity_is_sound_under_hostile_metadata() {
        // The artwork cache is keyed by exact (source, title, artist)
        // identity; a hostile metadata pair must never cross-return another
        // identity's cover, and the byte comparison must be total.
        let mut rng = FuzzRng(0xABCD_EF01_ABCD_EF01);
        for iteration in 0..2000 {
            let source = source_app_label(&hostile_string(&mut rng));
            let title = cap_meta(hostile_string(&mut rng));
            let artist = cap_meta(hostile_string(&mut rng));
            let cover: Arc<[u8]> = Arc::from(vec![(rng.next() % 256) as u8; (rng.next() % 32) as usize]);
            let mut map = HashMap::new();
            map.insert(
                source.clone(),
                TrackInfo {
                    title: title.clone(),
                    artist: artist.clone(),
                    artwork: Some(cover.clone()),
                    ..TrackInfo::default()
                },
            );
            let context = format!("iteration {iteration}");

            // Exact identity (source + title + artist) returns the cover.
            assert_eq!(
                cached_artwork_for(&map, &source, &title, &artist),
                Some(cover.clone()),
                "{context}: the exact identity missed the cache"
            );

            // A different title or artist must never return it — the
            // cross-identity leak that would attach the previous song's
            // cover to a new one.
            let other_title = cap_meta(hostile_string(&mut rng));
            let other_artist = cap_meta(hostile_string(&mut rng));
            if other_title != title {
                assert_eq!(
                    cached_artwork_for(&map, &source, &other_title, &artist),
                    None,
                    "{context}: a different title returned the cached cover"
                );
            }
            if other_artist != artist {
                assert_eq!(
                    cached_artwork_for(&map, &source, &title, &other_artist),
                    None,
                    "{context}: a different artist returned the cached cover"
                );
            }

            // Artwork byte comparison is total over presence combinations.
            let _ = artwork_same(Some(cover.as_ref()), None);
            let _ = artwork_same(None, Some(cover.as_ref()));
            let _ = artwork_same(Some(cover.as_ref()), Some(cover.as_ref()));
        }
    }
}
