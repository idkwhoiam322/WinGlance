# External Review: WinGlance (notch)

Perform a deep technical audit of the provided repository. This is a **Windows-native
(Win32 + [windows-rs](https://github.com/microsoft/windows-rs)) media overlay called
**Notch**: an always-on, borderless SMTC (System Media Transport Control) listener that
shows a compact "notch" pill plus an optional maximized tracking window with per-session
history, all driven by the tray icon. Ships as a single self-contained `notch.exe`; all
mutable user data lives under `%APPDATA%\notch\notch\data\config.toml` (+ `logs/`). No
accounts, no keys, no telemetry.

**Before you start:** read `README.md`, `docs/architecture.md`, `docs/development.md`,
`docs/configuration.md`, and `AGENTS.md` (build rules). Validate nothing you flag violates
those mandates by accident.

## Repo map

```
src/
  main.rs              process entry: logging, autostart, SMTC listener thread,
                       message loop, dual-window event forwarder
  config.rs            Config load/save/normalize (TOML); Overlay/Behavior/AppearanceConfig
  events.rs            TrackInfo/PlaybackState/MediaEvent + WM_APP message ids
  smtc.rs              Windows.Media.Control SMTC listener; coalesce/debounce
  overlay.rs           borderless notch pill overlay (layered, click-through)
  main_window.rs       maximized tracking window, history listbox, tray icon + menu,
                       autostart toggle
  autostart.rs         HKCU Run-key start-on-login sync
  logging.rs           single-run log-Live.log (truncated at startup)
docs/
  architecture.md, development.md, configuration.md
.github/workflows/
  ci.yml      fmt / clippy / test / build / cargo-deny
  release.yml build release exe on tag push, attach to GitHub Release
config.example.toml
```

## Severity scale
- Critical: security vulnerability, memory-unsafe crash, system/data risk, or production
  outage risk.
- High: significant performance bottleneck, memory/resource leak, race condition, or
  major functional flaw with no easy workaround.
- Medium: code anti-pattern, edge-case failure, sub-optimal logic, or notable
  maintainability debt.
- Low: minor dead code, non-blocking optimization, small cleanup, or style inconsistency.

## Focus areas
1. Perf & Memory: leaks (HWND/Bitmap/DC/GDI handles across the two windows + tray icon),
   excessive `InvalidateRect`/timer churn, redundant event queueing in the forwarder.
2. Safety & Concurrency: `unsafe` correctness, the shared `EventQueue` across two windows
   + SMTC thread + forwarder thread, lifetime of `Box<State>` stored in `GWLP_USERDATA`,
   double-free / use-after-free of window state, COM apartment requirements for
   `Windows::Media::Control`.
3. Architecture: dead/temporary code, unused features/dependencies, divergence between the
   overlay and main-window render paths, tray menu / autostart lifecycle.

## Mandatory rules & constraints (flag violations as High or Critical)
1. User data under `%APPDATA%\notch\notch\data\` (config.toml, logs, future db/cache) must
   NEVER be deleted or overwritten with defaults by the app once it exists. Unknown config
   fields must be preserved on save (or explicitly stated otherwise). `log-Live.log` may be
   truncated at startup (it is).
2. Single-exe, fully self-contained distribution — everything ships in `notch.exe`; the app
   creates its data dir at first run. No bundled data files, no external DLLs, no installed
   runtimes. Launching from the Start menu (or at Windows logon) must produce **no pop-ups**:
   no console, no dialogs, no UAC, no message boxes — only the tray icon + notch pill.
3. Backward compatibility: `config.toml` is additive only; new `[behavior]` keys
   (`start_in_tray`, `start_on_login`, `close_to_tray`) have safe defaults and must not break
   existing user files.
4. No accounts, no keys, no telemetry — SMTC metadata comes only from the OS; artwork is
   kept in-memory only.
5. The notch pill is always visible while the process is alive; the maximized window is
   optional UI opened only from the tray.
6. `unsafe` is confined to small, audited Win32 boundaries with `#[allow(unsafe_op_in_unsafe_fn)]`
   where needed; no unchecked raw-pointer derefs outside those boundaries.

## Verification tools (read-only; never launch the exe)
- `cargo fmt --all --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test -j4 --locked`
- `cargo build --release --locked` (build only — do not run)
- `cargo deny check` (policy in `deny.toml`; advisory-only by intent — do not re-flag
  allow-listed advisories, only NEW ones).
- `.\create_exe.ps1 -NoRestart` runs the full local pipeline; do NOT run it under review
  unless reproducing a packaging issue.

## Output
- Produce a report formatted for `Analysis.md` (this file). No code changes.
- Two sections:
  - Section 1: Safe Optimizations (no behavior change).
  - Section 2: Behavioral/Architectural Refactors (preserving the public surface above).
- End with a Findings Summary Table and a Major Refactors Table (columns specified in each).
