# Structure and development

## Project layout

```
WinGlance/
├── Cargo.toml              Crate manifest; pinned windows 0.58, image, serde/toml
├── Cargo.lock              Locked dependency set (build with --locked)
├── deny.toml               cargo-deny policy (license allowlist, duplicate skips)
├── .rustfmt.toml           Formatting options (imports = "granular")
├── .gitignore              Ignores target/, data/, logs/
├── config.example.toml     Documented defaults; copy of what the app generates
├── README.md               Entry point, positioning, verification notes
├── create_exe.ps1          Build/format/check/test/audit packaging script
├── .github/
│   └── workflows/
│       └── ci.yml          fmt / clippy / test / build / cargo-deny on push/PR
├── docs/
│   ├── architecture.md     Design, threading, SMTC selection, rendering, placement
│   ├── development.md      This file
│   ├── configuration.md    Every config option, its range, and its effect
│   └── reviews/            External audit prompt + code-packaging script
│       ├── FULL_CODE_REVIEW_PROMPT.md
│       └── PACKAGE_FOR_REVIEW.ps1
└── src/
    ├── main.rs             Entry point; logging, autostart, threads, message loop
    ├── config.rs           Config structs, defaults, load/save, normalize()
    ├── events.rs           Shared event types + WM_APP message ids
    ├── logging.rs          Single-file logger (log-Live.log, current run)
    ├── smtc.rs             Isolated SMTC listener on its own COM thread
    ├── overlay/            Pill overlay: mod/morph/render/fullscreen (see below)
    ├── icon.rs             Source-app icon extraction (shell COM, worker thread)
    ├── palette.rs          Vibrant-color quantizer for the accent/aura
    ├── main_window.rs      Maximized tracking window + tray icon/menu
    ├── autostart.rs        HKCU Run-key start-on-login sync
    ├── positioner.rs       Draggable sample window for custom placement
    └── process_picker.rs   Owner-drawn app-picker popup for the allow-list
```

## Module responsibilities

- **main.rs** — initializes logging, loads config, applies start-on-login, spawns
  the SMTC worker, creates the overlay and main window, runs the single
  `GetMessageW` loop, tears down both windows on exit. Non-debug builds are
  `windows_subsystem = "windows"` (no console).
- **events.rs** — the shared vocabulary between the worker and both windows:
  `MediaEvent::TrackChanged(TrackInfo)` / `MediaEvent::PlaybackStateChanged`,
  plus `WM_APP` ids (`MEDIA_EVENT_MSG` to both windows, `TOGGLE_MSG` to the
  overlay).
- **smtc.rs** — owns the WinRT `SystemMediaTransportControls` manager on its
  own COM thread. Subscribes to every session's `PlaybackInfoChanged` /
  `MediaPropertiesChanged`, reads metadata and artwork bytes, extracts app
  icons, deduplicates (content diff, session-recreation, artwork-change
  time-gate, per-source churn cool-down), drops stale cross-identity
  thumbnails with a budget-bounded artwork retry, bounds the subscribed set
  by session/source admission caps, and sends events down the channel.
- **overlay/** — the passive pill, split into `mod` (state, tick, events,
  hover handling, window/timer glue), `morph` (springs, hover decisions,
  pill geometry), `render` (frame composition, text rasterization, vector
  primitives) and `fullscreen` (display enumeration, target resolution,
  fullscreen detection). Rendering: `UpdateLayeredWindow`, DPI-aware
  position, the expand/light/collapse state machine, the palette aura, the
  per-track fill tint, the directional edge highlight, vector playback
  glyphs (play/pause/stop/music note), marquee rows, and the hover
  expand/dismiss interaction. The `set_*` push functions and `show_sample`
  are the only entry points other windows reach into.
- **icon.rs** — resolves a source app's icon from its AUMID through the shell
  (`SHCreateItemFromParsingName` + `IShellItemImageFactory`), cached per
  source. All shell calls run on one persistent bounded worker thread (16-job
  queue) with its own COM apartment, initialized once for the thread's
  lifetime. Each extraction is time-boxed to 1.5 s, and the worker has a
  circuit breaker: once a call actually hangs, the worker trips and all
  further requests fail fast (no new shell calls), so a hung shell extension
  cannot stall the SMTC worker or pile up blocked jobs. The breaker resets
  only when the app restarts.
- **palette.rs** — the two-color quantizer (4-bit histogram, saturation/
  luminance guard, ≥ 30° hue separation) that feeds the accents and the aura.
- **main_window.rs** — the tracking window (current activity + history) and the
  full tray menu: Open, Toggle notifications, Start with Windows, Close to tray,
  and the Monitor, Duration, and Layout submenus, and Quit.
- **autostart.rs** — reads/writes the `HKCU ...\Run` entry for start-on-login.
- **positioner.rs** — the in-app floating sample used to drag-place the pill; it
  posts the chosen `position_x`/`position_y` to the main window via
  `POSITION_MSG` (the main window owns the config, applies, and persists), and
  the overlay is nudged via `overlay::set_positions`.
- **process_picker.rs** — the owner-drawn picker popups opened from the
  Settings pane: the "Allowed apps" picker (lists running processes and open
  SMTC session sources, pre-checks the allow-list, posts via
  `PICKER_RESULT_MSG` to `behavior.media_sources`), the Auto-compact apps
  picker (`AUTO_SOURCES_RESULT_MSG`), and the single-select pinned-source
  picker (`PINNED_SOURCE_RESULT_MSG`), which only offers apps on the
  allow-list — a pin outside it could never fire.
- **accessibility.rs** — the UI Automation providers: a read-only name
  provider for the passive pill (current track as the accessible name, no
  patterns, never focusable) and the Settings-pane fragment provider with
  keyboard-focus/toggle-state events. Providers read shared snapshots or
  cells, never window state off the UI thread, and are detached in
  `WM_DESTROY`/`WM_NCDESTROY`.
- **winapi.rs / winutil.rs** — `winapi.rs` is a thin version-stable facade
  over the raw Win32/WinRT calls (signatures identical on the pinned
  `windows` line, so a future bump is a diff of wrapper bodies only);
  `winutil.rs` holds the shared helpers — the `StateClaim` window-state-box
  handshake, `register_class_once`, `copy_wide_terminated` (the required
  path for every hand-written fixed-size wide-string copy — a raw
  `copy_from_slice` into a `[u16; N]` is a review must-fix, see
  `docs/architecture.md`), verified-file writes, system preference
  sampling.
- **gdi.rs** — the shared GDI text drawing (`draw_string`); **duration_dialog.rs**
  — the custom pill-duration prompt.

## Build and verify

Run from the repository root:

```powershell
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
cargo deny check
.\create_exe.ps1 -Release -Start
```

The default build is `.\create_exe.ps1 -Release -Start`: it formats, lints
(all targets), tests, builds the optimized `WinGlance.exe`, audits, and
relaunches it into the tray (silent — the app runs in the background by
default). Useful flags: `-NoRestart` leaves the previous instance stopped,
`-SkipAudit` skips advisory/dependency checks, `-NoThrottle` uses all CPU
cores.

## Self-contained distribution

The release build produces a single `target\release\WinGlance.exe` (profile:
`codegen-units = 1`, `lto = "fat"`, `strip = "symbols"`). It has no data
dependencies: config and logs are created at first run under
`%APPDATA%\WinGlance\WinGlance\data\`, and every icon/resource is drawn with system GDI
calls. Launching from the Start menu (or at logon) surfaces only the tray icon and
the always-visible pill — no window, no console, no dialogs.

## Runtime data

| What          | Where                                            |
|---------------|--------------------------------------------------|
| Config        | `%APPDATA%\WinGlance\WinGlance\data\config.toml`         |
| Logs          | `%APPDATA%\WinGlance\WinGlance\data\logs\log-Live.log`  |
| Artwork cache | In memory only: one decoded buffer per unique cover; the overlay's cap-3 track cache (indefinite retention) plus its 64-entry playback-state ledger; per-source icon caches evicted when a session closes |

## Testing notes

Unit tests cover the pure logic: config clamping and normalization, debounce
and queue bounds, event dedup and the stale-art guard, session admission
ordering, the overlay's retirement/succession ledger, settings-layout purity,
autostart safety, and verified-file writes (reparse-point rejection).
OS-level behavior — click-through, focus avoidance, tray menu, real SMTC
provider timing — needs a live Windows desktop with media playing and cannot
be exercised in CI or a headless shell.
