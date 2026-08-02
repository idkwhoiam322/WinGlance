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
├── README.md               Entry point, SMTC quirks, verification notes
├── create_exe.ps1          NewsAggregator-style build/packaging script
├── scripts/
│   ├── pre-commit          Format/check/audit gate for git commits
│   └── setup-hooks.ps1     Installs the pre-commit hook into .git/hooks
├── docs/
│   ├── architecture.md     Design, threading, SMTC selection, rendering
│   └── configuration.md    Every config option, its range, and its effect
└── src/
    ├── main.rs             Entry point; wires logging, threads, and the loop
    ├── config.rs           Config structs, defaults, load/save, normalize()
    ├── events.rs           Shared event types (TrackInfo, PlaybackState)
    ├── logging.rs          Single-file logger (log-Live.log, current run)
    ├── smtc.rs             Isolated SMTC listener on its own COM thread
    └── overlay.rs          Win32 layered window, GDI rendering, tray icon
```

## Module responsibilities

- **main.rs** — initializes logging, loads config, spawns the SMTC worker,
  runs the overlay message loop, joins the worker, exits. Non-debug builds
  hide the console (`windows_subsystem = "windows"`).
- **events.rs** — the only shared vocabulary between the two threads:
  `MediaEvent::TrackChanged(TrackInfo)` and
  `MediaEvent::PlaybackStateChanged(PlaybackState)`. Kept in its own module
  so neither `smtc.rs` nor `overlay.rs` depends on the other.
- **smtc.rs** — owns the WinRT `SystemMediaTransportControls` handle. It
  resolves the session to watch, subscribes to `PlaybackInfoChanged` and
  `MediaPropertiesChanged`, loads artwork bytes, deduplicates with a
  fingerprint, and sends events down the channel. Unit tests cover the
  debounce clamp and source-app label fallback.
- **overlay.rs** — all Win32 window creation, message handling, GDI drawing,
  DPI math, tray icon/menu, and the expand/light/collapse state machine.
- **logging.rs** — a `log`-crate logger that appends to
  `log-Live.log` inside `<data_dir>\logs`, truncated at each startup so only
  the current run is retained.
- **config.rs** — serde structs with `#[serde(default)]` so missing fields
  never break loading; `normalize()` clamps every value before use. The
  config file lives at `%APPDATA%\notch\notch\data\config.toml`.

## Build and verify

All commands run from the repository root:

```powershell
cargo fmt --all -- --check   # formatting must be clean
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

The packaging script reproduces this flow and adds tool checks and audit
passes:

```powershell
.\create_exe.ps1                 # full run (throttles CPU, restarts app)
.\create_exe.ps1 -SkipAudit -NoThrottle -NoRestart   # quick build
```

Useful flags: `-SkipAudit` skips advisory/dependency checks, `-NoThrottle`
uses all CPU cores, `-NoRestart` leaves a running instance alone.

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
