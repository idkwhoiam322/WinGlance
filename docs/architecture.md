# Architecture

Notch is a Rust media overlay for Windows. It watches the System Media
Transport Controls (SMTC) and shows a small, click-through "notch" pill near
the top of the screen when the track or the playback state changes.

## Design principles

- **No UI framework.** The overlay is a raw Win32 layered window rendered with
  GDI. No `winit`, no `tiny-skia`, no `tokio`. This keeps the dependency tree
  small and the binary lean.
- **Two isolated threads.** SMTC work (COM, WinRT async, artwork decoding)
  runs on a dedicated worker thread. The Windows UI runs on the UI thread with a
  classic `GetMessageW` loop. They communicate only through an `mpsc` channel and
  `PostMessageW`.
- **Two windows, one queue.** There is a borderless "notch" pill overlay window
  and a maximized "tracking" window; both register a `WM_MEDIA_EVENT` handler.
  A single forwarder thread drains the SMTC `mpsc` receiver into a shared
  `Arc<Mutex<VecDeque<MediaEvent>>` and pokes **both** windows with
  `PostMessageW`, so each can render from the same event stream without owning SMTC.
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
  │                                       ├──► queue (Arc<Mutex<VecDeque>>)
  │                                       │       ├──► both windows read it
  │                                       │       └──► PostMessageW(WM_MEDIA_EVENT) to BOTH
  │                                       │       └──► PostMessageW(WM_TOGGLE) to overlay only
  │                                       └──► shared queue + both HWNDs
  │
  │  WM_MEDIA_EVENT → receive_events()  (pill + main)
  │  WM_TIMER (debounce) → flush_pending()  (pill)
  │  WM_TIMER (16 ms) → tick() → render()   (pill)
  │  WM_TOGGLE → toggle_enabled()           (pill, from tray menu)
```

- The SMTC worker owns all COM state for its lifetime and initializes COM as
  MTA. Async WinRT calls block on `IAsyncOperation::get()`; blocking the
  worker is acceptable because no other code shares that thread.
- The event forwarder is a thin thread that drains the `mpsc` receiver into a
  `Mutex<VecDeque>` and pokes the UI thread with `PostMessageW`. It exists so
  the UI thread stays responsive even if several SMTC callbacks fire at once.
- The UI thread only touches Win32 windows, the queue, and GDI surfaces. All
  long work (artwork resize, decode) happens on the SMTC worker before the
  event is sent.

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

Each frame is rendered into an in-memory RGBA buffer:

1. `draw_pixels` fills a rounded-rect background, draws the album art (or an
   accent-colored placeholder circle), and returns premultiplied BGRA pixels.
2. The buffer is copied into a `CreateDIBSection` backing store.
3. `draw_text` paints title/artist (or a state label) with GDI `DrawTextW`.
4. `UpdateLayeredWindow` with `ULW_ALPHA` composites the window; the window
   uses per-monitor DPI-aware scaling via `GetDpiForWindow`.

The window is created with `WS_EX_LAYERED | WS_EX_TRANSPARENT |
WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`, returns `HTTRANSPARENT` from
`WM_NCHITTEST`, and is shown with `SW_SHOWNOACTIVATE`. It therefore cannot
be clicked through to, activated, or tabbed to.

Animation is a simple three-phase ease-out: expanding (grow + fade in), light
(short fade for playback-state changes), collapsing (shrink + fade out).

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
opens `src/positioner.rs`, a draggable sample that writes `position_x`/`position_y`
to `config.toml` and nudges the live overlay via `overlay::set_position` (which
calls `reposition()` without a full redraw).

## Configuration

`Config::load()` reads `%APPDATA%\notch\notch\data\config.toml`, falls back
to defaults, clamps every value in `normalize()`, and writes the file only if
it does not exist yet. See `docs/configuration.md` for the full reference.
