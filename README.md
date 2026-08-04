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
- `src/main_window.rs` owns a tracking window (current activity + a
  per-source history listbox) and the tray icon/menu. The window starts hidden
  when `behavior.start_in_tray` is on (the default), so launching the app
  produces no pop-up — only the tray icon and the always-visible pill.
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

Rendering uses raw Win32 plus GDI and `UpdateLayeredWindow`. The `image` crate
only decodes SMTC's encoded artwork; it does not add a GPU or webview runtime.

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
(`close_to_tray = true`). Logging is a single per-run file, truncated at startup
and capped at 1 MB so a long session cannot grow the file without bound:

- `data\logs\log-Live.log` contains the current run.

The pill has no config to fine-tune beyond the six anchors and custom position.
Playback state and track changes are coalesced so duplicate
title/artist/album/artwork values do not retrigger the animation; the history
list keeps the most recent 400 events.

## Build

Install the stable MSVC Rust toolchain, then run:

```powershell
.\create_exe.ps1
.\create_exe.ps1 -Release -Start
```

`create_exe.ps1` format-checks, checks all Cargo targets, builds a single
optimized `notch.exe`, runs `cargo-audit` and `cargo-deny`, stops any running
instance, and relaunches it into the tray. Flags:

- `-Release` — build the optimized profile (default is debug).
- `-Start` — relaunch into the tray after building (the default build path).
- `-NoRestart` — do not relaunch after building.
- `-SkipAudit` — skip `cargo-audit`/`cargo-deny` for a fast iterate loop.
- `-NoThrottle` / `-Jobs N` — control build parallelism (all cores vs. `N`).

Install the repository hook once with:

```powershell
.\scripts\setup-hooks.ps1
```

## CI

`ci.yml` runs format/clippy/test/release-build/cargo-deny on every push to
`main` and on PRs. `cargo-deny` runs via `taiki-e/install-action` (the
`EmbarkStudios/cargo-deny-action` Docker container is Linux-only and cannot run
on the Windows runner). `release.yml` builds on a tag `v*` (or manual dispatch)
and attaches the exe plus example config to a GitHub Release.

## Resource footprint and caps

The app keeps a small footprint by design:

- The pill repaints only while expanding, collapsing, or marquee-scrolling;
  a fully-shown static pill does no timer-driven drawing at all.
- Artwork is decoded once per event, cached at a 128×128 premultiplied buffer,
  and shared between the windows as an `Arc<[u8]>` of the raw bytes.
- History is bounded at 400 events.
- Fonts and brushes are cached process-wide instead of recreated per draw.
- The live log is capped at 1 MB.

The pinned `windows 0.58` bindings were checked locally for these behaviors:

- `IAsyncOperation::get()` blocks until SMTC metadata and thumbnail reads finish.
- Event registration returns `EventRegistrationToken` values that must be
  removed when rebinding a session.
- `UpdateLayeredWindow` takes a source `HDC`; it does not accept a pixel-buffer
  pointer directly.

The remaining platform behavior to verify manually on a Windows 10/11 desktop
is provider-specific session timing, multi-monitor placement, and click-through
behavior with the active media sources. The source includes no automated
desktop/window-manager test for those OS behaviors.