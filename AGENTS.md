# AGENTS.md

Guidelines for working in this repository (Notch — a Windows SMTC media
overlay, Rust, raw Win32 + GDI).

## Build and verify

The app runs in the system tray by default (`start_in_tray = true`), so
starting it after a build is silent — no windows pop up. Use the packaging
script as the default build path:

```powershell
.\create_exe.ps1 -Release -Start
```

This formats-check, checks all targets, builds the optimized `notch.exe`,
runs cargo-audit + cargo-deny, stops any running instance, and relaunches it
into the tray. Use `-NoRestart` when you do not want it relaunched,
`-SkipAudit` for a quick loop, `-NoThrottle`/`-Jobs N` to control parallelism.

Direct checks (run these before claiming verification):

```powershell
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
cargo deny check
```

Verification claims must be labeled: **Verified** (actually run),
**Reasoned but not executed**, or **Unable to verify**. Never claim tests pass
without running `cargo test`. GUI behavior (pill rendering, tray, positioner,
hover tooltips) needs a live Windows desktop with media playing — cannot be
verified headless.

## Runtime data

- Config: `%APPDATA%\notch\notch\data\config.toml` (hand-edits need a restart;
  no live reload).
- Logs: `%APPDATA%\notch\notch\data\logs\log-Live.log`, truncated at startup,
  at Debug level (session churn, dedup skips and suppressed events are logged
  — use it to answer "why did/didn't a notification fire").
- Never delete user data under `%APPDATA%\notch\notch\data\` (only the
  log file may be truncated at startup). The Settings pane has a "Copy logs"
  button that puts the log on the clipboard.

## Never launch anything

- Never start the app, helper processes, capture scripts, or any command
  that can appear on the user's screen (no `Start-Process`, no `.exe`
  launches, no GUI/screenshot tooling) — the user may be in a fullscreen
  game. This is a hard rule.
- Verify through log files only: `log-Live.log` (+ `crash.log`) under
  `%APPDATA%\notch\notch\data\logs\`, and temporary files under
  `C:\Users\admin\AppData\Local\Temp\opencode\`. All diagnostic output goes
  to those files, never to the screen.
- If a visual check is needed, tell the user what to look at and let them
  restart the app themselves.

## Architecture guardrails

- The pill (`src/overlay.rs`) is strictly passive: no click targets, no focus,
  no keyboard input.
- SMTC worker thread ↔ UI thread split must be preserved: the worker only
  emits `MediaEvent`s over `mpsc`; the UI thread owns all Win32 windows.
- The main window (`src/main_window.rs`) is the single owner/writer of the
  in-memory config; settings changes are pushed to the overlay via
  `overlay::set_position` / `overlay::set_duration`, and the positioner posts
  its result back via `POSITION_MSG` — never reload config from disk in
  `positioner.rs`.
- Smoke test against the log after changing SMTC logic: one song change should
  produce one `track changed` line (with `artwork=` present), no repeated
  `track emit skipped` lines.
- Churn-storm smoke check (after touching `smtc.rs` session handling): start
  an app that recreates its SMTC session rapidly (Riot Client was observed at
  ~20 sessions in 8.5s). Confirm the log shows one
  `SessionsChanged/CurrentSessionChanged (debounced)` line per burst with
  `(coalesced)` lines in between (not one resolve per event), that the
  churning source gets one `WARN ... churning sessions ... excluding it` line
  when it trips the cool-down, and that no `track changed` /
  `playback state changed` line names the churning source while it is on
  cool-down (its sessions never emit a `track changed` /
  `playback state changed` line).

## Git

- Imperative-mood commit subjects, one logical change per commit.
- The repo is committed on `main`; do not push unless asked.
