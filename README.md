# Notch

Notch is a Windows 10/11 Rust utility that displays a short, passive media
overlay when the current System Media Transport Controls (SMTC) session
changes.

## Architecture

- `src/smtc.rs` owns `GlobalSystemMediaTransportControlsSessionManager` and
  registers `SessionsChanged`, `MediaPropertiesChanged`, and
  `PlaybackInfoChanged` callbacks.
- `src/overlay.rs` owns the raw Win32 layered popup, its timer-driven
  expand/light/collapse animation, and GDI rendering. Click-through and
  focus-avoidance match the spec above.
- `src/main_window.rs` owns a maximized tracking window (current activity + a
  per-session history listbox) and the tray icon/menu. The window starts
  hidden when `behavior.start_in_tray` is on (the default), so launching the
  app produces no pop-up — only the tray icon and the always-visible pill.
- `src/autostart.rs` toggles the `HKCU ...\Run` entry for start-on-login.
- `src/positioner.rs` is a small floating sample window opened from the tray
  **Position → Adjust position…** menu item; dragging it writes the absolute
  `position_x`/`position_y` to `config.toml` and repositions the live overlay.
  The overlay re-anchors on resolution/monitor changes.

Events cross the worker/UI boundary through a channel and `PostMessageW`; the UI
thread blocks in `GetMessageW` when idle. A foreground monitor (`GetForegroundWindow`)
picks the monitor the user is on, and `WM_TIMER` (16 ms) keeps the pill anchored to
that monitor's work area, so placement tracks monitor/resolution changes.
- The overlay uses `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW |
  WS_EX_NOACTIVATE`, returns `HTTRANSPARENT`, and handles
  `WM_MOUSEACTIVATE` with `MA_NOACTIVATE`.
- Events cross the worker/UI boundary through a standard channel and
  `PostMessageW`; the UI thread blocks in `GetMessageW` when idle.
- A raw Win32 notification-area icon provides enable/disable and quit. The
  overlay itself has no input controls.

Rendering uses Option A from the build specification: raw Win32 plus GDI and
`UpdateLayeredWindow`. The `image` crate only decodes SMTC's encoded artwork; it
does not add a GPU or webview runtime.

## Positioning the notification

The pill can be anchored to any of six edges:

- **top-left / top-center / top-right**
- **bottom-left / bottom-center / bottom-right**

Choose one from the tray **Position** submenu. The edge offset is `overlay.margin`
(logical pixels). For a custom spot, open **Position → Adjust position…**: a
floating sample appears; drag it where you want the pill, release, and the app
writes `overlay.position_x`/`position_y` to `config.toml` and repositions the
live overlay (preview it with **Position → Show sample**, or **Reset position**
to return to the anchor). Absolute overrides are clamped to the current monitor
work area, so they stay valid after a resolution change or monitor switch.

## Configuration and logs

The first run creates:

`%APPDATA%\notch\notch\data\config.toml`

Copy the values from `config.example.toml` to edit them. The defaults launch the
app quietly to the tray (`start_in_tray = true`), start only on explicit launch
(`start_on_login = false`), and hide the tracking window to the tray on close
(`close_to_tray = true`). Logging is a single per-run file, truncated at startup:

- `data\logs\log-Live.log` contains the current run.

## Build

Install the stable MSVC Rust toolchain, then run:

```powershell
.\create_exe.ps1
.\create_exe.ps1 -Release -Start
```

The script checks formatting, checks all Cargo targets, builds a single

Install the repository hook once with:

```powershell
.\scripts\setup-hooks.ps1
```

## Session priority

The listener uses SMTC's `GetCurrentSession()` first. If that is unavailable,
it prefers the session that caused the callback when it is Playing, then the
most recently observed Playing session, then the first Playing session in
`GetSessions()` order. SMTC does not expose a portable last-active timestamp,
so the final enumeration fallback cannot provide a stronger ordering.

Playback position/timeline changes are ignored because only the playback
status is compared. Track properties are coalesced for 150-250 ms and
duplicate title/artist/album/artwork values do not retrigger the full animation.

## Verification notes

The pinned `windows 0.58` bindings were checked locally for these behaviors:

- `IAsyncOperation::get()` blocks until SMTC metadata and thumbnail reads finish.
- Event registration returns `EventRegistrationToken` values that must be
  removed when rebinding a session.
- `UpdateLayeredWindow` takes a source `HDC`; it does not accept a pixel-buffer
  pointer directly.

The remaining platform behavior to verify manually on a Windows 10/11 desktop
is provider-specific session timing, multi-monitor placement, and click-through
behavior with the user's active media sources. The source includes no automated
desktop/window-manager test for those OS behaviors.
