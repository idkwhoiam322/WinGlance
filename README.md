# WinGlance

A passive, always-on-top media overlay for Windows 10/11. When the track or
playback state changes, WinGlance shows a small rounded pill with the album art,
a palette-tinted glow, and the app that's playing — without ever stealing
focus or intercepting clicks. Built for gaming and fullscreen use: it never
interrupts and never needs interaction.

## Features

- **Track and playback pills** — a full notification on track change (art,
  title, artist, duration, album, source app) and a compact state pill on
  play/pause/stop, with a music note on new tracks and ▶/‖/■ on state
  changes.
- **Per-track theming** — two vibrant colors are extracted from the album art
  and drive the accent symbols, the artist/source text tint, the album-art
  rim, a subtle tint in the pill fill, and a soft C₁→C₂ **aura glow** around
  the pill, brighter on the album-art side. A directional edge highlight
  traces the pill's boundary. No palette in the artwork? Falls back to the
  configured accent.
- **Source-app icons** — the app's icon (from its Windows AUMID) renders
  next to the source name.
- **Completely passive** — `WS_EX_TRANSPARENT | WS_EX_NOACTIVATE`: no focus
  steal, no Alt-Tab entry, no click capture. Clicks pass straight through to
  your game.
- **Hover interaction** — hover a compact pill and it expands in place so
  you can read it (a second hover dismisses it); hover an expanded pill and
  it dismisses within 500 ms instead of waiting out its full duration.
  Queueing a newer notification caps the current pill at 500 ms the same
  way. Both behaviors are tunable in the Settings pane.
- **Marquee titles** — long titles scroll only when they actually overflow
  the visible band, then stop when shown.
- **Placement** — anchor to any of six screen edges with a configurable
  margin, or drag to a custom spot with the built-in positioner.
- **Persistent Compact layout** — the pill never fully disappears while
  media is playing: after its duration it rests at idle opacity instead of
  collapsing, so the now-playing info stays on screen.
- **Preferred source (pin)** — with a pin set, the persistent pill returns
  to that app's current track whenever it would fade out, while that source
  is actually playing; a paused/stopped pin is never resurrected, and with
  no pin the pill settles on the most recent source that is still playing.
- **Bounded progress bar** — when the playing app reports timeline position
  via SMTC, a thin accent bar along the pill's bottom edge advances with
  playback, freezes while paused, and re-bases on a seek.
- **Accessible** — the pill exposes the current track as an accessible name
  (UI Automation), and the tracking window's Settings pane is
  keyboard-navigable and exposes a full UIA provider.
- **Small footprint** — raw Win32 + GDI, no UI framework, no webview, no GPU
  runtime. Five isolated threads: SMTC worker, supervisor watchdog, event
  forwarder, icon worker, and the UI thread; the pill repaints only while
  animating or scrolling — and while media plays, the aura comet sweep keeps
  the tick loop alive at a reduced ~15 Hz cadence for its duration.

## Getting started

Download `WinGlance.exe` from the latest [release](../../releases), run it, and
it sits in the tray. Later launches start silently (no window); the very first
run — the one that creates the config — opens the tracking window once. The
pill appears when media plays. Click (or double-click) the tray icon for the
tracking window: it shows the
current activity and a per-source history on the **Now Playing** pane, plus a
**Settings** pane mirroring the tray menu (notifications, duration,
start-on-login, close-to-tray, allowed apps, layout, position, monitor,
preferred source, logs).

### Tray menu

- **Open WinGlance** — the tracking window (current activity + history list)
- **Preview Notification** — show a sample pill
- **Toggle notifications** — enable/disable pills
- **Start with Windows** — launch at logon
- **Close window to tray** — hide instead of quit when the window is closed
- **Monitor** — place the pill on the active window's display, the primary
  display, or a numbered display
- **Duration** — 2 / 3 / 5 / 10 seconds, or a custom value via **Custom…**
- **Layout** — Expanded / Compact / Auto / Persistent Compact
- **Quit**

  Placement (edge anchor, custom coordinates) is not in the tray; it is edited in
  the **Settings** pane's **Position** row, which opens the drag sample.

## Configuration

Config lives at `%APPDATA%\WinGlance\WinGlance\data\config.toml` (created on first
run; see [`config.example.toml`](config.example.toml)). It controls pill
duration/animation, edge anchor and margin, which source apps notify, and the
appearance: background, accent color, corner radius, padding, art size and
fonts. Hand-edits apply after a restart; tray-menu changes apply immediately.
See [`docs/configuration.md`](docs/configuration.md) for the full reference.

Logs: `%APPDATA%\WinGlance\WinGlance\data\logs\log-Live.log` (current run, truncated
at startup and capped at 1 MiB during a run) — useful for answering "why
did/didn't a notification fire".

## Building from source

Requires the stable MSVC Rust toolchain (or use the CI/release builds):

```powershell
.\create_exe.ps1 -Release -Start
```

The script format-checks, lints, tests, builds an optimized `WinGlance.exe`,
runs `cargo-audit`/`cargo-deny`, stops any running instance, and relaunches
it into the tray. Flags: `-NoRestart` (don't relaunch), `-SkipAudit` (fast
loop), `-NoThrottle` / `-Jobs N` (parallelism). Direct checks:
`cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`,
`cargo test --locked`, `cargo build --release --locked`.

## How it works

- `src/smtc.rs` — the SMTC worker thread: subscribes to every session,
  reads metadata/artwork, extracts app icons, and deduplicates (content
  diff, session-recreation, churn cool-down, artwork-change time-gate).
  Async reads are time-bounded, and a source whose session hangs a read is
  excluded from tracking for a cool-down window instead of stalling the
  worker.
- `src/overlay/` — the raw Win32 layered pill, split into `mod` (state,
  tick, window glue), `morph` (springs, hover decisions, geometry),
  `render` (frame composition, text, vector primitives) and `fullscreen`
  (display enumeration and fullscreen detection): expand/light/collapse
  animation, palette + aura rendering, vector glyphs, marquee rows,
  hover expand/dismiss, the persistent-compact idle rest, the progress bar.
- `src/accessibility.rs` — the UI Automation providers: the pill's read-only
  name provider and the Settings-pane fragment provider.
- `src/palette.rs` / `src/icon.rs` — the color quantizer and the shell icon
  extraction.
- `src/main_window.rs` — tracking window, tray icon/menu, history, the
  Settings pane (keyboard-navigable, UIA-provided).
- `src/process_picker.rs` — the owner-drawn popups (allowed apps,
  auto-compact apps, and the single-select pinned-source picker).
- `src/winapi.rs` / `src/winutil.rs` — a version-stable facade over the raw
  Win32 calls plus shared helpers (window-state boxes, NUL-terminated wide
  copies).
- `src/positioner.rs` — the drag-to-place sample window.

The overlay is a click-through layered window rendered with GDI and
`UpdateLayeredWindow`; the `image` crate only decodes the SMTC artwork.
Events cross the worker/UI boundary through a channel. See
[`docs/architecture.md`](docs/architecture.md) for the threading model,
rendering pipeline and dedup design.

## CI and releases

`ci.yml` runs format/lint/test/release-build/cargo-deny on every push and PR, and
publishes a GitHub Release (with the built exe and example config) only when manually
dispatched from the Actions tab.

## License

MIT OR Apache-2.0.
