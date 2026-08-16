# Architecture

WinGlance is a Rust media overlay for Windows. It watches the System Media
Transport Controls (SMTC) and shows a small, click-through "WinGlance" pill near
the top of the screen when the track or the playback state changes.

## Design principles

- **No UI framework.** The overlay is a raw Win32 layered window rendered with
  GDI. No `winit`, no `tiny-skia`, no `tokio`. This keeps the dependency tree
  small and the binary lean.
- **Five isolated threads.** The SMTC worker runs all COM/WinRT work (session
  reads, artwork bytes, image decode); a supervisor thread watches and
  restarts it; the event forwarder drains the event channel into the two
  window queues; a bounded icon worker performs the shell calls for app
  icons; the UI thread runs every Win32 window with a classic `GetMessageW`
  loop. They communicate only through `mpsc` channels and `PostMessageW`.
- **Two windows, two queues.** There is a borderless "WinGlance" pill overlay window
  and a maximized "tracking" window; both register a `MEDIA_EVENT_MSG` handler.
  A single forwarder thread drains the SMTC `mpsc` receiver into two window-owned
  queues (`Arc<Mutex<VecDeque<Arc<MediaEvent>>>>`, one per window — the transport
  `Arc` is recovered into an owned event on drain) and pokes **both**
  windows with `PostMessageW`, so each can render from the same event stream
  without owning SMTC. Two queues are required: a single shared queue would let
  one window's drain consume events the other still needs — each window owns and
  drains its own copy.
- **Passive pill.** The overlay never takes focus, never appears in
  Alt-Tab, never intercepts mouse clicks, and stays on top of the active
  monitor's work area.
- **Config over code.** All visual and behavioral knobs live in
  `config.toml`, clamped to safe ranges by `Config::normalize()`.

## Threading model

```
supervisor thread                        UI thread (message loop)
─────────────────────                    ─────────────────────────
watchdog on the worker                   RegisterClassExW x2
heartbeat; restarts a                    create_window x2 (pill + main)
stalled/exited worker with               install tray icon (main window)
backoff (max 5 failures,                 GetMessageW loop
then one WorkerFailed on the
status channel)                          ┌────────────────────────────┐
                                         │                           │
SMTC worker thread               events  │        forwarder thread    │
─────────────────────  ──────────────────┘───►      ─────────────────│
CoInitializeEx(MTA)   bounded mpsc channel            drain → main queue │
SystemMediaTransportControls  (cap 1024)              drain → overlay queue│
  ├─ GetCurrentSession()                              PostMessageW(MEDIA_EVENT_MSG) to BOTH
  ├─ get_playback_info()                              PostMessageW(TOGGLE_MSG) to overlay only
  ├─ subscribe PlaybackInfoChanged                    relay WorkerFailed (status channel)
  ├─ subscribe MediaPropertiesChanged
  │
  │  MEDIA_EVENT_MSG → receive_events()  (pill + main)
  │  WM_TIMER (debounce) → flush_pending()  (pill)
  │  WM_TIMER (16 ms) → tick() → render()   (pill)
  │  TOGGLE_MSG → toggle_enabled()         (pill, from tray menu)

icon worker thread
──────────────────
bounded 16-job queue, COM initialized
once per worker lifetime, 1.5 s per-job
timeout, circuit breaker (fail-fast
after one hung shell call)
```

- The SMTC worker owns all COM state for its lifetime and initializes COM as
  MTA. Async WinRT calls block on `IAsyncOperation::get()`; blocking the
  worker is acceptable because no other code shares that thread.
- The supervisor watches a shared worker heartbeat. A worker that stalls
  (30 s without a beat) or exits is restarted with an increasing backoff
  (5 s → 60 s); a worker that runs for two minutes resets the failure
  counter. After five consecutive failures of any kind — spawn, exit, or
  stall — the supervisor stops restarting, logs, and sends one `WorkerFailed`
  event (history row + tray note): media notifications will not resume until
  the app restarts. A hung worker is never joined (it may be blocked inside
  COM forever); the restart cap bounds that leak.
- The event forwarder is a thin thread that drains the bounded event channel
  into the two window queues and pokes the UI thread with `PostMessageW`. It
  exists so the UI thread stays responsive even if several SMTC callbacks
  fire at once. It also relays the supervisor's one-shot `WorkerFailed` from
  a dedicated unbounded status channel, so that report can never be dropped
  by an overloaded event channel. Each window queue is bounded (cap 256,
  newest wins); a failed wake post clears and accounts the affected queue
  instead of stranding events in it.
- The UI thread owns all Win32 windows, the queues, and GDI surfaces. The
  SMTC worker performs the metadata + artwork reads and the fixed-size
  artwork *decode* (into a fixed `ARTWORK_DECODE`² = 256² premultiplied
  BGRA buffer, once per unique cover). App-icon extraction runs on the
  separate icon worker (see above), so a hung shell extension cannot stall
  the SMTC worker. The UI thread only converts that buffer to RGBA in
  `ensure_art` (~0.1 ms, cached for the animation frames); the palette is
  derived from that same converted buffer, so no separate full-resolution
  decode is ever needed.

## SMTC session selection

SMTC surfaces one "current session" and a list of sessions. The worker
subscribes to **every** open session's `MediaPropertiesChanged` and
`PlaybackInfoChanged` (tracked in a `HashMap`), so changes in background
sessions are never missed — the native media widget does the same. There is no
"current session" concept on the worker side: each `(source_app, session_key)`
is tracked independently with its own `LogicalState` (title, artist, album,
artwork presence, source, duration, track numbers, genre, playback state).

A fresh read is merged into the stored `LogicalState` and diffed against it:

- Fields that actually changed are emitted (`TrackChanged` when a displayed
  content field differs, `PlaybackStateChanged` when playback differs).
- `Stopped` is a normal diffable `playback` value — it produces a pill like any
  other real transition. `Closed` does not go through the diff path at all: it
  triggers immediate eviction of the `(source_app, session_key)` entry, and a
  session that disappears from the session list is evicted at the next re-sync.
- Empty fields inherit from the stored state while the title/artist identity is
  unchanged (SMTC fills metadata progressively); a new identity starts fresh.
- Artwork presence is diffed separately: a late-arriving cover re-emits the
  track (the pill refreshes in place), a disappearing one is stored silently —
  absence is already shown as a placeholder.
- A same-media identity (`TrackInfo::same_media`: source + title + artist +
  artwork identity) drives the overlay's update-vs-new-pill decision: the same
  song with a genuinely different cover queues a fresh pill; missing art on
  either side is tolerated so a late thumbnail updates in place.
- A cover whose *bytes* differ from the last emit forces a re-emit only after
  `ARTWORK_CHANGE_MIN_INTERVAL` (3 s) — SMTC re-reads the thumbnail within a
  second and can return different bytes for the same cover, which would
  otherwise duplicate every pill.
- Events for one session arriving within the debounce window are coalesced:
  the key is marked dirty and read exactly once per window.

Two sources with identical content both notify independently; no cross-source
matching or suppression happens.

The subscribed set is bounded: the worker never tracks more than
`MAX_TRACKED_SESSIONS` sessions across `MAX_TRACKED_SOURCES` sources. Each
re-sync builds a priority-ordered candidate list — the current session first,
then the surviving subscriptions in snapshot order, then genuinely new
sessions truncated at the session cap — via the pure `prioritize_sessions`
function, so overflow candidates are never enumerated past the cap and never
subscribed (the same ordering contract the admission tests pin). A source
that keeps recreating content-free sessions (the churn signature) is charged
per content-free first read and, past the threshold, excluded from emitting
and evicted for a cool-down window.

Artwork is a stream that lags the text fields: a transition read can pair a
NEW track identity with the PREVIOUS track's thumbnail bytes (SMTC updates
the thumbnail stream after the text). The byte-equal cross-identity cover is
dropped and the emit deferred (`track emit deferred | reason=stale-art-drop`);
a budget-bounded retry (~2 s) re-reads the thumbnail, and an artwork-timeout
force shows the pill with a placeholder if the stream never recovers — the
pill always eventually shows something, but never a wrong cover on a new
track.

## Event pipeline and debounce

`MediaPropertiesChanged` / `PlaybackInfoChanged` fire frequently, sometimes
multiple times per second for a single visual change. The worker:

1. Marks the session key dirty and schedules a flush 150–250 ms later (clamped
   by config), so a burst of events for one session collapses into one read.
2. At the flush, reads each dirty key once, merges the read into the stored
   state, and emits one event per changed field.
3. A `SessionsChanged`/`CurrentSessionChanged` burst is debounced the same
   way: one subscription re-sync per burst.
4. A 2-second periodic safety net re-syncs subscriptions and re-reads every
   subscribed session (metadata only — except a bounded thumbnail re-read
   for sessions still missing artwork) so a missed event still surfaces; the
   same read reports what is already playing at startup.

The overlay holds a small pending queue (cap 4): while a pill is on screen the
next notification waits for the current one to collapse, so simultaneous
events from different sources show one after another instead of clobbering
each other. At the cap, the oldest *unshown* queued event is dropped in favor
of the incoming one; the pill on screen is never pulled.

A Stopped session keeps its stored content, so a state pill after a stop still
shows the last track.

**Source retirement and succession.** When a source settles — its last session
disappears from the list (the worker emits `SourceGone`) or it is removed from
the allow-list / evicted as churning — the overlay retires it: its content is
removed from the pill, the persistent-compact resume hold, the settings sample
pill, and any queued notification (a queued event from a source that settled
before its turn is dropped by the queue's SourceGone pre-gate, never shown).
If the retiring source's content was on the pill, the overlay may swap it: the
successor is the most recent cached track of a source whose last known
playback state is **Playing**, chosen from a per-source playback-state ledger
(fed by `TrackChanged` snapshots and `PlaybackStateChanged` events — updated
even while notifications are disabled, capped at 64 entries, evicting Stopped
entries first). Paused, stopped, or unknown sources never qualify: a settle
with nothing actually playing hides the pill instead of announcing stale
"now playing" content, and a live source re-shows the pill on its own next
event. The track cache itself is cap-bounded (3 entries, text plus one
decoded cover each) with indefinite retention — a playing source's track is
never evicted by time, only by newer inserts.

## Rendering pipeline

Each frame is rendered into an in-memory premultiplied-BGRA buffer:

1. `draw_pixels` resolves the artwork and converts the worker's decoded
   premultiplied-BGRA buffer once per unique cover (`ensure_art`, keyed by
   the decoded pixels), then draws — in order — the
   palette aura ring in the DIB margin, the near-opaque palette-tinted rounded-rect
   body (fill alpha from `background_color[3]`, default 235) with its directional edge highlight, the album art with its accent
   glow and rim, and the vector playback glyph.
2. `draw_text_pixels` paints the title/artist/meta/source-app rows with GDI
   `DrawTextW` into a scratch DIB and composites them alpha-correctly; rows
   marquee-scroll only while their text overflows the visible band.
3. `UpdateLayeredWindow` with `ULW_ALPHA` composites the window; the window
   uses per-monitor DPI-aware scaling via `GetDpiForWindow`.

The window is created with `WS_EX_LAYERED | WS_EX_TRANSPARENT |
WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`, returns `HTTRANSPARENT` from
`WM_NCHITTEST`, and is shown with `SW_SHOWNOACTIVATE`. It therefore cannot
be clicked through to, activated, or tabbed to.

Animation runs through three phases — expanding (grow + fade in), light (a
short 120 ms fade for playback-state changes), collapsing (shrink + fade out)
— on the spring model described in "Morph springs" below, driven by a
high-resolution timer matched to the monitor's refresh rate while the pill
animates or a text line scrolls (the period is capped by `max_tick_hz`,
default 60 Hz). A fully static pill drops to a coarse 250 ms tick — the
dismiss countdown and hover polling do not need frame rate — and the pill
repaints only while animating or marquee-scrolling; a static pill does no
per-frame drawing. Queueing a newer notification caps the remaining display
time at 500 ms. What hovering does to the pill is described in "Hover
behavior" below.

## Hover behavior

Hovering is polled from the tick: the pill has no mouse capture, so the
cursor is hit-tested against the rendered bounds every frame. A leave is only
trusted after the cursor has stayed away for `LEAVE_DEBOUNCE` (60 ms), so
boundary jitter cannot cancel an interaction the moment it starts; re-entering
clears the leave. What a hover does follows the pill's *effective* layout
(an Auto pill follows whichever layout is currently in effect) and the two
toggles `dismiss_on_hover` and `expand_compact_on_hover`:

- **Expanded layout** — hovering has no interaction with the pill: the
  countdown is never deferred for the cursor, so the pill dismisses on its
  deadline even under it. With `dismiss_on_hover` (default) the first hover
  tick arms the one-way 500 ms dismiss (`EARLY_EXIT_MS`); without it,
  hovering changes nothing.
- **Compact layout, `expand_compact_on_hover` (default)** — the first hover
  of a showing starts an in-place morph to the expanded layout (the pill's
  layout and position stay Compact-anchored for the whole morph) and resets
  the dismissal clock to the full configured duration. The morph-origin
  expanded state is an interaction: while the cursor stays on it, its
  countdown is deferred (`held`), so it is never dismissed mid-read, and a
  queued notification waits with it (updates route in place — `held_expanded`
  gives the event-receiving paths the same hold decision between ticks).
  Leaving — mid-morph or after the pin — runs the collapse leg back to
  compact and resets the countdown to the full duration. With
  `dismiss_on_hover` (default), later hovers over the compact pill dismiss
  instead — the second hover dismisses (one-way 500 ms arm); without it,
  every hover re-expands and resets again.
- **Compact layout, `expand_compact_on_hover` off** — the pill behaves
  exactly like an Expanded-layout one: `dismiss_on_hover` applies, no morph.

The hover decisions apply only to a fully-shown pill; hovering during the
entrance/exit animations keeps the plain Expanded-rule arming (`dismiss_on_hover`
and the layout is Expanded). A due dismissal loses to an in-flight collapse
leg (which runs to completion so it cannot snap mid-shrink) but wins over an
in-flight expand leg (the pill collapses from the plain compact shape).

## Morph springs

Every size transition — the entrance grow, the exit shrink, the in-place
hover morph — runs the same damped-oscillator model. A `Spring` is a damped
harmonic oscillator (damping ratio ζ, angular frequency ω) whose closed-form
response is evaluated at normalized leg time. `value_at` pins the endpoint:
at or past the leg end the curve is exactly 1.0, so the resting pill always
renders at the exact compact/expanded size, never a hair short. The renderers clamp the curve into the endpoint interval, so the expand
spring's ~5 % overshoot (ζ = 0.7, 2.8π) never reaches the hover morph's
geometry, while the entrance bounce and the settle-bounce scale the rendered
pill as a whole (`BOUNCE_OVER` past the endpoint, and on compaction the whole
pill dips to (1 − `BOUNCE_UNDER`) of its final size). The collapse spring
(ζ = 0.6) undershoots below compact by ~9.5 % of the remaining distance, and
because that undershoot spreads over the tail of a leg running 4/5 of the
entrance duration, the compaction bounce reads as a slow, pronounced settle
rather than a fast blip.

The width axis leads and the height axis chases it with `MORPH_LAG` (0.12 of
the leg): the follower evaluates the leader's curve at a delayed, compressed
local time (`(t − lag) / (1 − lag)`), so the card widens before it grows tall,
never overtakes the leader, and pins at its endpoint exactly when the leg
ends. A reversal (the cursor leaves mid-expand) does not restart the
animation: the collapse leg is the expand curve's mirror, seeded with the
exact progress and per-second velocity at the reversal moment (converted
across the two leg lengths by `reversal_seed`), so the motion continues
through the flip without a kink — the pill may travel a little farther before
turning, then settles exactly at compact. A reversal from less than
`REVERSAL_MIN_PROGRESS` (0.05) drops the morph instead of running a seeded
release, which would balloon a pill that barely left compact.

During the morph the shared elements — the title, the playback symbol and
the artwork — never fade: they travel from their compact positions to their
expanded positions on each axis's own progress (the artwork's side length
lerps while it stays centered in the growing body, the title and symbol
track their bands), so the morph reads as the card unfolding in place.
Only the layout-exclusive elements fade, keyed to the *less-advanced* axis
so nothing renders outside the visible card: the compact app icon dissolves
out over 0.05..0.20 of the shape progress and the expanded extra rows
(artist, meta, app) fade in over 0.25..0.60 — disjoint windows, in both
directions, so the icon and the app row never coexist. The content is drawn
progressively into the single reusable DIB, and the expanded extra rows
unveil as the pill's animated bottom edge sweeps past their bands.

## Palette and aura

Two vibrant colors are extracted from the album art (a 4-bit-per-channel
histogram over the decoded display buffer, guarded by saturation ≥ 0.25 and
luminance 0.20–0.85, secondary ≥ 30° away in hue). The primary recolors the
playback symbols, the clock icon and the music note; a muted pastel variant
tints the artist and source-app rows; and a 16% blend of the primary tints the
pill fill itself, so the near-opaque body picks up a hint of the cover's hue. The
aura is a soft C₁→C₂ glow drawn in the DIB margin around the pill, brighter on
the album-art side, with peak alpha ~140 at the pill boundary fading
exponentially over a 6 logical-px halo — the window is inflated by the halo
extent so the glow extends into the desktop rather than being clipped. A
directional edge highlight (white, brighter at the top-left) traces the pill's
own boundary, drawn as a supersampled coverage ring so the corners stay
anti-aliased.

## Placement and resolution

`overlay::position()` resolves the pill's screen top-left from `[overlay]`:

- `vertical` (`top`/`bottom`) × `horizontal` (`left`/`center`/`right`) pick an
  anchor on the target display's work area;
- `margin` offsets from the chosen edge in 96-DPI logical pixels (scaled by the
  target display DPI);
- `position_x`/`position_y`, when set, override the anchor with an absolute
  location (virtual-screen coordinates) and are clamped to the target work
  area so they stay on-screen.

The target display comes from `monitor`: `active-window` (the monitor of the
foreground window — see "Foreground tracking" below), `primary` (the display
flagged `MONITORINFOF_PRIMARY`), or `index-N` (the (N+1)-th display in
`EnumDisplayMonitors` order). `overlay::enumerate_displays()` takes a fresh
snapshot on every placement — handles are never cached — and
`resolve_target()` maps the mode onto it; an out-of-range index falls back to
the primary (with a throttled warning) without touching the config, so the
setting reapplies when the display returns. `position_x`/`position_y` retain
their existing semantics — absolute virtual-screen coordinates in 96-DPI
logical pixels — and the resulting position is clamped into the target
display's work area.

DPI for sizing, fonts, and the margin comes from the target display via
`GetDpiForMonitor(MDT_EFFECTIVE_DPI)` (`overlay::monitor_dpi()`), not from
the monitor the overlay window currently sits on — the first frame after a
display switch is already scaled correctly. The animation timer's refresh
period is queried against the overlay window itself (it sits on the target
while animating), falling back to the target's display-mode frequency.

Because the 16 ms `WM_TIMER` recomputes geometry each tick while the pill is
shown, a monitor removal or resolution change moves the pill back onto the
new work area on the next frame; `WM_DISPLAYCHANGE` additionally repositions
a visible pill immediately. The **Adjust position…** command in the Settings
pane opens `src/positioner.rs`, a draggable sample that posts the chosen
`position_x`/`position_y` to the main window via `POSITION_MSG`; the main
window — the single owner of the config — applies and persists them, and
nudges the live overlay via `overlay::set_positions` (which calls
`reposition()` without a full redraw). `OverlayPos` carries the monitor mode,
so anchor, custom-position and monitor changes all flow through the same
push path.

## Foreground tracking

`MonitorMode::ActiveWindow` and the Auto layout both depend on the foreground
window, so the overlay tracks it event-driven, with a polling fallback:

- A `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` hook, installed when the
  overlay window is created (`WINEVENT_OUTOFCONTEXT`, so the system delivers
  the callback on the UI thread), notices every foreground change. The
  callback only posts `FOREGROUND_CHANGE_MSG` — it never touches overlay
  state. The message handler re-resolves the Auto layout and the effective
  work-area anchor and repositions immediately, instead of waiting for the
  next media event or the 250 ms static tick. A switch that changes nothing
  (e.g. Alt-Tab between two normal apps on the same monitor) is skipped by
  comparing the re-resolved anchor with the last one. When the Auto layout
  flipped (Compact ↔ Expanded) the pill is fully re-rendered; the animation
  phase is preserved, so a foreground switch never restarts it.
- The 250 ms static tick is the fallback: `tick_layout_check` reacts to a
  foreground change within one static tick even when no media event arrives
  (e.g. an Alt-Tab into a fullscreen game while a pill is up). The full
  decision runs only when the foreground *HWND* changed; the same-window
  geometry (a resize into fullscreen) is re-checked at most once per second.
  The tick also covers a failed hook install (logged; the overlay degrades
  to polling) and the teardown race: the hook is unhooked in WM_NCDESTROY
  before the window state is released, and a racing callback sees a null
  overlay handle and no-ops.

The executable identity the Auto layout matches against
(`auto_compact_sources`) is cached with the foreground HWND, so the process
lookup (`exe_name_for_pid`, a targeted `OpenProcess` + image-name query
rather than a process-table walk) runs only when the foreground window
actually changes; the fullscreen verdict is recomputed cheaply from the
window rect on every sample. Both decisions feed `decide_layout`, the single
pure resolver of Auto.

## Main window: panes and the process picker

The main window (`src/main_window.rs`) is a maximized tracker with a sidebar
switching between two panes:

- **Now Playing** — the current activity (art, state, title/artist/album,
  meta) and the per-session history listbox (newest first, capped at 400
  rows, auto-pinned to the newest entry via `LB_SETTOPINDEX` on every insert —
  intended). A native `TOOLTIPS_CLASSW` control shows full row details on hover,
  synced to the visible row band on a 1 Hz timer while the window is visible.
- **Settings** — cards mirroring the tray menu and `[behavior]`/`[overlay]`
  config: notifications toggle, duration presets and the respect-system-
  duration toggle, start-on-login, close-to-tray, allowed apps, position
  anchors + Reset/Adjust, target display selection, "Preview Notification",
  and the "Copy logs" button. The main window is the
  single writer of the in-memory config (see the guardrail in `AGENTS.md`);
  every change goes through `mutate_config` and is persisted.

The **process picker** (`src/process_picker.rs`, ~900 lines) is an
owner-drawn popup opened from the Settings "Allowed apps" card. It lists
visible windows' processes (one Toolhelp snapshot + one `EnumWindows` pass)
plus every open SMTC session source, pre-checks the current allow-list with
the same normalization the worker uses, and posts the confirmed patterns back
to the main window via `PICKER_RESULT_MSG`, which applies them to
`behavior.media_sources` — so allow-list changes apply to the live worker
without a restart.

## Configuration

`Config::load()` reads `%APPDATA%\WinGlance\WinGlance\data\config.toml`, falls back
to defaults, clamps every value in `normalize()`, and writes the file only if
it does not exist yet. See `docs/configuration.md` for the full reference.
