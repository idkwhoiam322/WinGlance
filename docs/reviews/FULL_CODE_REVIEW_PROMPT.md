# External Review: WinGlance

Perform a deep technical audit of the provided repository. This is a **Windows-native
(Win32 + [windows-rs](https://github.com/microsoft/windows-rs)) media overlay called
**WinGlance**: an always-on, borderless SMTC (System Media Transport Control) listener that
shows a compact WinGlance pill plus an optional maximized tracking window with per-session
history, all driven by the tray icon. Ships as a single self-contained `WinGlance.exe`; all
mutable user data lives under `%APPDATA%\WinGlance\WinGlance\data\config.toml` (+ `logs/`). No
accounts, no keys, no telemetry.

**Mandate: flag everything.** Do not limit yourself to the focus areas below — they are a
floor, not a ceiling. Flag any and all of the following, each with a severity and a clear
description of the scenario that exposes it:

- **Bugs** — logic errors, off-by-one mistakes, incorrect or missing conditionals,
  unreachable branches, wrong values, edge cases (empty/invalid/overflow input, missing
  data, error paths, races, rapid repeated input).
- **Functional flaws** — behavior that does not match its intent, the config docs, or the
  README; features that silently do nothing; settings that have no visible effect.
- **UI/UX issues and improvements** — visual inconsistencies (the same element rendered
  differently across layouts, modes, DPI scales, or code paths), color/accent mismatches,
  contrast problems, animation timing or easing that reads wrong, abrupt cuts where a
  transition is expected, hover/interaction behavior that surprises, misleading labels or
  settings rows, clipping/truncation, focus/keyboard gaps, tooltip problems.
- **Accessibility** — WCAG contrast failures, keyboard-only reachability, high-DPI
  breakage, tiny hit targets.
- **Performance, memory, safety, concurrency, architecture, maintainability, dead code,
  documentation drift** — anything listed in the focus areas below or otherwise
  noteworthy.
- **Improvements of any kind** — polish, simplifications, UX refinements, potential new
  behavior. These are welcome as *suggested directions* with reasoning; do not require
  them to be bugs. The maintainer decides what to act on.

Do not self-censor because something looks deliberate or "probably intended": flag it and
state clearly when a behavior looks like a deliberate design choice that only the
maintainer can confirm. It is more useful to be wrong about a low-severity item than to
stay silent about a real one.

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
  overlay/             borderless WinGlance pill overlay (layered, click-through):
    mod.rs             state, tick, events, hover handling, window/timer glue
    morph.rs           springs, hover decisions, pill geometry
    render.rs          frame composition, text rasterization, vector primitives
    fullscreen.rs      display enumeration, target resolution, fullscreen detection
  main_window.rs       maximized tracking window, history listbox, tray icon + menu,
                       autostart toggle
  autostart.rs         HKCU Run-key start-on-login sync
  logging.rs           single-run log-Live.log (truncated at startup)
docs/
  architecture.md, development.md, configuration.md
.github/workflows/
  ci.yml      fmt / clippy / test / build / cargo-deny
config.example.toml
```

## Severity scale
- Critical: security vulnerability, memory-unsafe crash, system/data risk, or production
  outage risk.
- High: significant performance bottleneck, memory/resource leak, race condition, major
  functional flaw with no easy workaround, or a UI/UX flaw that breaks a core interaction
  (e.g. a pill state the user cannot escape, an always-wrong color/position).
- Medium: code anti-pattern, edge-case failure, sub-optimal logic, notable
  maintainability debt, or a visible-but-workaroundable UI/UX inconsistency.
- Low: minor dead code, non-blocking optimization, small cleanup, style inconsistency,
  or polish-level UX suggestion.

## Focus areas
1. Correctness & Logic: wrong conditionals/values, off-by-one errors, unreachable
   branches, schema mismatches on external data (config/TOML, SMTC events), silent
   fallbacks that hide failures, edge cases (empty, missing, invalid, overflow, rapid
   repetition), error paths.
2. Functionality & Behavior: anything that does not match its documented intent
   (config.example.toml, docs/configuration.md, README), settings with no visible
   effect, dedup/coalescing mistakes in the SMTC pipeline, dismissal/hover timer
   surprises.
3. UI/UX & Visual: the same element rendered differently across layouts (compact vs
   expanded), modes, DPI scales, or code paths; color/accent drift between the pill, the
   settings window, and the history; contrast and readability; animation timing, easing,
   and abrupt cuts; hover-expand/dismiss interaction friction; settings pane layout,
   labels, and misleading rows; text clipping, truncation, marquee glitches; tooltip
   accuracy; tray-menu drift from the settings pane.
4. Accessibility: WCAG AA contrast failures, keyboard-only reachability, high-DPI
   breakage, small hit targets.
5. Perf & Memory: leaks (HWND/Bitmap/DC/GDI handles across the two windows + tray icon),
   excessive `InvalidateRect`/timer churn, redundant event queueing in the forwarder.
6. Safety & Concurrency: `unsafe` correctness, the shared `EventQueue` across two windows
   + SMTC thread + forwarder thread, lifetime of `Box<State>` stored in `GWLP_USERDATA`,
   double-free / use-after-free of window state, COM apartment requirements for
   `Windows::Media::Control`.
7. Architecture & Maintainability: dead/temporary code, unused features/dependencies,
   divergence between the overlay and main-window render paths, module boundaries that
   have drifted, tray menu / autostart lifecycle.
8. Documentation: README/`docs/`/`config.example.toml` drift from actual behavior,
   missing docs for new settings or interactions.

## Mandatory rules & constraints (flag violations as High or Critical)
1. User data under `%APPDATA%\WinGlance\WinGlance\data\` (config.toml, logs, future db/cache) must
   NEVER be deleted or overwritten with defaults by the app once it exists. Unknown config
   fields must be preserved on save (or explicitly stated otherwise). `log-Live.log` may be
   truncated at startup (it is).
2. Single-exe, fully self-contained distribution — everything ships in `WinGlance.exe`; the app
   creates its data dir at first run. No bundled data files, no external DLLs, no installed
   runtimes. Launching from the Start menu (or at Windows logon) must produce **no pop-ups**:
   no console, no dialogs, no UAC, no message boxes — only the tray icon + WinGlance pill.
3. Backward compatibility: `config.toml` is additive only; new `[behavior]` keys
   (`start_in_tray`, `start_on_login`, `close_to_tray`) have safe defaults and must not break
   existing user files.
4. No accounts, no keys, no telemetry — SMTC metadata comes only from the OS; artwork is
   kept in-memory only.
5. The WinGlance pill is always visible while the process is alive; the maximized window is
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

**Verification labeling.** GUI behavior (pill rendering, hover interaction, settings pane,
tray) cannot be observed headless. For every UI/UX or behavioral finding, label the basis:
**Verified** (exercised by a runnable test or the build), **Reasoned but not executed**
(derived from a direct code trace), or **Unable to verify** (sandbox limitation) — and
always cite the exact code path you traced, so the maintainer can re-check it live.

## Output
- Produce a report formatted for `Analysis.md` (this file). No code changes.
- Two sections:
  - Section 1: Safe Optimizations (no behavior change).
  - Section 2: Behavioral/Architectural Refactors (preserving the public surface above).
- All finding types — bugs, functional flaws, UI/UX, accessibility, polish suggestions —
  belong in one of the two sections based on whether they change behavior; do not drop a
  finding because it does not fit the old categories. Every finding carries the severity,
  the exact location, the scenario that exposes it, and (for improvement suggestions) a
  concrete proposed direction.
- End with a Findings Summary Table and a Major Refactors Table (columns specified in each).
- Also note anything that looks like a deliberate design decision you could not confirm,
  so the maintainer can mark it intended or not.
