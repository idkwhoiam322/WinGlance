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
│       ├── ci.yml          fmt / clippy / test / build / cargo-deny on push/PR
│       └── release.yml     builds the self-contained notch.exe and attaches it
│                           to a GitHub Release on tag push
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
    ├── overlay.rs          Win32 layered pill; GDI rendering, position math
    ├── main_window.rs      Maximized tracking window + tray icon/menu
    ├── autostart.rs        HKCU Run-key start-on-login sync
    └── positioner.rs       Draggable sample window for custom placement
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
- **smtc.rs** — owns the WinRT `SystemMediaTransportControls` handle. Resolves
  the session, subscribes to `PlaybackInfoChanged`/`MediaPropertiesChanged`,
  loads artwork bytes, deduplicates, and sends events down the channel.
- **overlay.rs** — passive pill: `UpdateLayeredWindow`, DPI-aware position, and
  the expand/light/collapse state machine. `set_position`/`show_sample` are the
  only entry points other windows reach into.
- **main_window.rs** — the tracking window (current activity + history) and the
  full tray menu: Open, Toggle notifications, Start with Windows, Close to tray,
  Position submenu, and Quit.
- **autostart.rs** — reads/writes the `HKCU ...\Run` entry for start-on-login.
- **positioner.rs** — the in-app floating sample used to drag-place the pill; it
  writes `position_x`/`position_y` back to `config.toml` and calls
  `overlay::set_position`.

## Build and verify

Run from the repository root:

```powershell
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
cargo deny check
.\create_exe.ps1 -NoRestart
```

Useful `create_exe.ps1` flags: `-SkipAudit` skips advisory/dependency checks,
`-NoThrottle` uses all CPU cores, `-NoRestart` leaves a running instance alone.

## Self-contained distribution

The release build produces a single `target\release\notch.exe` (profile:
`codegen-units = 1`, `lto = "thin"`, `strip = "symbols"`). It has no data
dependencies: config and logs are created at first run under
`%APPDATA%\notch\notch\data\`, and every icon/resource is drawn with system GDI
calls. Launching from the Start menu (or at logon) surfaces only the tray icon and
the always-visible pill — no window, no console, no dialogs.

## Runtime data

| What          | Where                                            |
|---------------|--------------------------------------------------|
| Config        | `%APPDATA%\notch\notch\data\config.toml`         |
| Logs          | `%APPDATA%\notch\notch\data\logs\log-Live.log`  |
| Artwork cache | none (decoded in memory per event)               |

## Testing notes

Unit tests cover pure logic only (config clamping, debounce bounds, event
types). OS-level behavior — click-through, focus avoidance, tray menu, real
provider timing — needs a live Windows desktop with media playing and cannot
be exercised in CI or a headless shell.
