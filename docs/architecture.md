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
resolves the session to watch with this priority (see `smtc.rs`):

1. The session from `GetCurrentSession()`.
2. A hinted session whose playback state is `Playing`, when the current
   session is not playing.
3. The most recently observed session that was `Playing`.
4. The first `Playing` session from `GetSessions()`.

Documented limitation: SMTC exposes no portable "last active" timestamp, so
fallback 3 is a best-effort heuristic.

## Event pipeline and debounce

`MediaPropertiesChanged` fires frequently, sometimes multiple times per
second for a single visual change. The worker:

1. Reads track title, artist, album, source app, and artwork thumbnail.
2. Computes a fingerprint (title + artist + album) to drop duplicate events.
3. Stages the event with a debounce timer (150–250 ms, clamped) so a burst of
   changes coalesces into one overlay show.
4. Track events are remembered on the overlay side; a later playback-state
   event shows the last track with a light animation instead of a full
   expand.

A Stopped baseline on startup is recorded silently; the overlay only shows a
track when playback is not Stopped.

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
