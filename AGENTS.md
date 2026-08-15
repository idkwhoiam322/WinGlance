# AGENTS.md

Guidelines for working in this repository (WinGlance — a Windows SMTC media
overlay, Rust, raw Win32 + GDI).

## Build and verify

The packaging script is the sanctioned build-and-verify command for this
repo — you are allowed to run it, and one invocation covers the whole gate:

```powershell
.\create_exe.ps1 -Release -Start
```

It formats-checks, checks all targets, clippy with `-D warnings`, runs
`cargo test`, builds the optimized `WinGlance.exe`, runs cargo-audit +
cargo-deny, stops any running instance, and relaunches it into the tray
(silent — `start_in_tray = true`). Use `-NoRestart` when you do not want it
relaunched, `-SkipAudit` for a quick loop, `-NoThrottle`/`-Jobs N` to control
parallelism.

When the script exits successfully, label the checks it ran as **Verified**
(its output records each step). Fall back to the raw commands only for
targeted re-checks during a fix loop:

```powershell
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
cargo deny check
```

Verification claims must be labeled: **Verified** (actually run),
**Reasoned but not executed**, or **Unable to verify**. Never claim tests
pass without running `cargo test` (the script runs it). GUI behavior (pill
rendering, tray, positioner, hover tooltips) needs a live Windows desktop
with media playing — cannot be verified headless.

## Runtime data

- Config: `%APPDATA%\WinGlance\WinGlance\data\config.toml` (hand-edits need a restart;
  no live reload).
- Logs: `%APPDATA%\WinGlance\WinGlance\data\logs\log-Live.log`, truncated at startup
  on plain launches but preserved (appended to, with a restart boundary line)
  after an in-app "Restart app", at Debug level (session churn, dedup skips
  and suppressed events are logged
  — use it to answer "why did/didn't a notification fire").
- Never delete user data under `%APPDATA%\WinGlance\WinGlance\data\` (only the
  log file may be truncated at startup). The Settings pane has a "Copy logs"
  button that puts the log on the clipboard.

## Never launch anything

- Never start the app, helper processes, capture scripts, or any command
  that can appear on the user's screen (no `Start-Process`, no `.exe`
  launches, no GUI/screenshot tooling) — the user may be in a fullscreen
  game. This is a hard rule.
- The single exception: running `.\create_exe.ps1 -Release -Start` as the
  build-and-verify gate (see Build and verify) is explicitly allowed — its
  relaunch into the tray is silent (`start_in_tray = true`) and shows no
  window. No other launch path is permitted.
- Verify through log files only: `log-Live.log` (+ `crash.log`) under
  `%APPDATA%\WinGlance\WinGlance\data\logs\`, and temporary files under
  `C:\Users\admin\AppData\Local\Temp\opencode\`. All diagnostic output goes
  to those files, never to the screen.
- If a visual check is needed, tell the user what to look at and let them
  restart the app themselves.

## Architecture guardrails

- The pill (`src/overlay/`) is strictly passive: no click targets, no focus,
  no keyboard input.
- SMTC worker thread ↔ UI thread split must be preserved: the worker only
  emits `MediaEvent`s over `mpsc`; the UI thread owns all Win32 windows.
- The main window (`src/main_window.rs`) is the single owner/writer of the
  in-memory config; settings changes are pushed to the overlay via
  `overlay::set_positions` / `overlay::set_duration`, and the positioner posts
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
- Day-to-day work is committed on `dev` (the checked development branch);
  `main` is the default/release branch. Do not push either branch unless
  asked.
