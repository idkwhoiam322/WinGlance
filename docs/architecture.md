# Architecture

WinGlance is a Rust media overlay for Windows. It watches the System Media
Transport Controls (SMTC) and shows a small, click-through "WinGlance" pill near
the top of the screen when the track or the playback state changes.

## Design principles

- **No UI framework.** The overlay is a raw Win32 layered window rendered with
  GDI. No `winit`, no `tiny-skia`, no `tokio`. This keeps the dependency tree
  small and the binary lean.
- **Two isolated threads.** SMTC work (COM, WinRT async, artwork decoding)
  runs on a dedicated worker thread. The Windows UI runs on the UI thread with a
  classic `GetMessageW` loop. They communicate only through an `mpsc` channel and
  `PostMessageW`.
- **Two windows, two queues.** There is a borderless "WinGlance" pill overlay window
  and a maximized "tracking" window; both register a `WM_MEDIA_EVENT` handler.
  A single forwarder thread drains the SMTC `mpsc` receiver into two window-owned
  queues (`Arc<Mutex<VecDeque<MediaEvent>>>`, one per window) and pokes **both**
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
SMTC worker thread                      UI thread (message loop)
────────────────────—                      ─────────────────────────
CoInitializeEx(MTA)                      RegisterClassExW x2
SystemMediaTransportControls            create_window x2 (pill + main)
  ├─ GetCurrentSession()               install tray icon (main window)
  ├─ get_playback_info()               GetMessageW loop
  ├─ subscribe PlaybackInfoChanged
  ├─ subscribe MediaPropertiesChanged
  │
  │  events → mpsc channel ─────────► forwarder thread
  │                                       │
  │                                       ├──► main queue (Arc<Mutex<VecDeque>>)
  │                                       │       └──► main window drains it
  │                                       ├──► overlay queue (Arc<Mutex<VecDeque>>)
  │                                       │       └──► overlay drains it
  │                                       ├──► PostMessageW(WM_MEDIA_EVENT) to BOTH
  │                                       └──► PostMessageW(WM_TOGGLE) to overlay only
  │
  │  WM_MEDIA_EVENT → receive_events()  (pill + main)
  │  WM_TIMER (debounce) → flush_pending()  (pill)
  │  WM_TIMER (16 ms) → tick() → render()   (pill)
  │  WM_TOGGLE → toggle_enabled()           (pill, from tray menu)
```

- The SMTC worker owns all COM state for its lifetime and initializes COM as
  MTA. Async WinRT calls block on `IAsyncOperation::get()`; blocking the
  worker is acceptable because no other code shares that thread.
- The event forwarder is a thin thread that drains the `mpsc` receiver into the
  two window queues and pokes the UI thread with `PostMessageW`. It exists so
  the UI thread stays responsive even if several SMTC callbacks fire at once.
- The UI thread owns all Win32 windows, the queue, and GDI surfaces. SMTC
  reads (metadata + artwork bytes), app-icon extraction (COM shell calls),
  and image *decoding* (into a fixed `ARTWORK_DECODE`² = 256² premultiplied
  BGRA buffer, once per unique cover) all run on the worker. The UI thread
  only converts that buffer to RGBA in `ensure_art` (~0.1 ms, cached for
  the animation frames); the palette is derived from that same converted
  buffer, so no separate full-resolution decode is ever needed.

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
   subscribed session (metadata only, no artwork) so a missed event still
   surfaces; the same read reports what is already playing at startup.

The overlay holds a small pending queue (cap 4): while a pill is on screen the
next notification waits for the current one to collapse, so simultaneous
events from different sources show one after another instead of clobbering
each other. At the cap, the oldest *unshown* queued event is dropped in favor
of the incoming one; the pill on screen is never pulled.

A Stopped session keeps its stored content, so a state pill after a stop still
shows the last track.

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

Animation is a simple three-phase ease-out: expanding (grow + fade in), light
(short fade for playback-state changes), collapsing (shrink + fade out),
driven by a high-resolution timer matched to the monitor's refresh rate while
the pill animates or a text line scrolls. A fully static pill drops to a
coarse 250 ms tick — the dismiss countdown and hover polling do not need frame
rate — and the pill repaints only while animating or marquee-scrolling; a
static pill does no per-frame drawing. Hovering the cursor over the pill, or
queueing a newer notification, caps the remaining display time at 500 ms.

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
  anchor on the active monitor's work area (the monitor of `GetForegroundWindow`);
- `margin` offsets from the chosen edge in 96-DPI logical pixels (scaled by the
  window DPI);
- `position_x`/`position_y`, when set, override the anchor with an absolute
  location and are clamped to the work area so they stay on-screen.

Because the 16 ms `WM_TIMER` recomputes geometry each tick while the pill is
shown, a monitor removal or resolution change moves the pill back onto the new
work area on the next frame. The tray **Position → Adjust position…** command
opens `src/positioner.rs`, a draggable sample that posts the chosen
`position_x`/`position_y` to the main window via `POSITION_MSG`; the main
window — the single owner of the config — applies and persists them, and
nudges the live overlay via `overlay::set_position` (which calls
`reposition()` without a full redraw).

## Main window: panes and the process picker

The main window (`src/main_window.rs`) is a maximized tracker with a sidebar
switching between two panes:

- **Now Playing** — the current activity (art, state, title/artist/album,
  meta) and the per-session history listbox (newest first, capped at 400
  rows). A native `TOOLTIPS_CLASSW` control shows full row details on hover,
  synced to the visible row band on a 1 Hz timer while the window is visible.
- **Settings** — cards mirroring the tray menu and `[behavior]`/`[overlay]`
  config: notifications toggle, duration presets, start-on-login, close-to-
  tray, allowed apps, position anchors + Reset/Adjust, "Show sample", and the
  "Copy logs" button. The main window is the single writer of the in-memory
  config (see the guardrail in `AGENTS.md`); every change goes through
  `mutate_config` and is persisted.

The **process picker** (`src/process_picker.rs`, ~900 lines) is an
owner-drawn popup opened from the Settings "Allowed apps" card. It lists
visible windows' processes (one Toolhelp snapshot + one `EnumWindows` pass)
plus every open SMTC session source, pre-checks the current allow-list with
the same normalization the worker uses, and posts the confirmed patterns back
to the main window via `PICKER_RESULT_MSG`, which applies them to
`behavior.allowed_sources` — so allow-list changes apply to the live worker
without a restart.

## Configuration

`Config::load()` reads `%APPDATA%\WinGlance\WinGlance\data\config.toml`, falls back
to defaults, clamps every value in `normalize()`, and writes the file only if
it does not exist yet. See `docs/configuration.md` for the full reference.
