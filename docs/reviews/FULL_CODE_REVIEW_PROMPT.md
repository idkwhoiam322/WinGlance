# External Review: WinGlance

Perform a deep technical audit of the provided repository. This is a **Windows-native
(Win32 + [windows-rs](https://github.com/microsoft/windows-rs)) media overlay called
**WinGlance**: an always-on, borderless SMTC (System Media Transport Control) listener that
shows a compact WinGlance pill plus an optional maximized tracking window with per-session
history, all driven by the tray icon. Ships as a single self-contained `WinGlance.exe`; all
mutable user data lives under `%APPDATA%\WinGlance\WinGlance\data\config.toml` (+ `logs/`). No
accounts, no keys, no telemetry.

This prompt is a full-audit program, not a checklist: a coverage floor, six depth passes
(A–F), a fixed scenario-walkthrough list, a mandatory enhancement program, and a risk
register. Run the passes in any order, but run them all; the scenario walkthroughs are the
integration pass that ties static findings to observed behavior.

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

**Before you start** (read all of these, in order):
1. `README.md`, `docs/architecture.md`, `docs/development.md`, `docs/configuration.md`,
   `docs/research.md`, `config.example.toml`, `AGENTS.md` (build + runtime rules),
   `Cargo.toml` (dependency manifest), `deny.toml` (dependency policy), and
   `.github/workflows/ci.yml` (what the CI gate actually runs).
2. Validate that nothing you flag violates those mandates by accident (e.g., do not flag
   `log-Live.log` truncation at startup — `AGENTS.md` states it is intended).
3. **Suggested run order**: docs → coverage-floor scan → passes A–D (static) → pass E
   (scenario walkthroughs, integrating) → pass F (enhancement program) → risk register →
   report.

## Repo map

This map is a starting point, not gospel: reconcile it against the code as you audit, and
report any drift you find (Low, Documentation).

```
src/
  main.rs              process entry: logging, autostart, SMTC listener thread,
                       message loop, dual-window event forwarder
  config.rs            Config load/save/normalize (TOML); Overlay/Behavior/AppearanceConfig
  events.rs            TrackInfo/PlaybackState/MediaEvent + WM_APP message ids
  smtc.rs              Windows.Media.Control SMTC listener; coalesce/debounce/churn handling
  overlay/
    mod.rs             state, tick, events, hover handling, window/timer glue
    morph.rs           springs, hover decisions, pill geometry
    render.rs          frame composition, text rasterization, vector primitives
    fullscreen.rs      display enumeration, target resolution, fullscreen detection
  main_window.rs       maximized tracking window, history listbox, tray icon + menu,
                       autostart toggle
  positioner.rs        pill positioning; receives pushes, posts result via POSITION_MSG;
                       must never reload config from disk (AGENTS.md)
  palette.rs           artwork-derived color palette (confirm exact role from code)
  gdi.rs               GDI helpers (confirm exact role from code)
  icon.rs              icon handling/resources (confirm exact role from code)
  winutil.rs           Win32 utility wrappers (confirm exact role from code)
  process_picker.rs    process-selection surface (confirm exact role from code)
  duration_dialog.rs   duration settings dialog (confirm exact role from code)
  autostart.rs         HKCU Run-key start-on-login sync
  logging.rs           single-run log-Live.log (truncated at startup)
docs/
  architecture.md, development.md, configuration.md, research.md
.github/workflows/
  ci.yml      fmt / clippy / test / build / cargo-deny
config.example.toml
```

## Severity scale
- Critical: security vulnerability, memory-unsafe crash, system/data risk, or production
  outage risk. Includes: user-data loss or corruption, memory corruption reachable from
  untrusted input, and resource exhaustion that crashes the process.
- High: significant performance bottleneck, memory/resource leak, race condition, major
  functional flaw with no easy workaround, unbounded memory/handle growth that degrades the
  process over time, or a UI/UX flaw that breaks a core interaction (e.g. a pill state the
  user cannot escape, an always-wrong color/position).
- Medium: code anti-pattern, edge-case failure, sub-optimal logic, notable
  maintainability debt, or a visible-but-workaroundable UI/UX inconsistency.
- Low: minor dead code, non-blocking optimization, small cleanup, style inconsistency,
  or polish-level UX suggestion.

**Improvement priority scale (separate from severity — used only in Depth pass F):**
- P0 — re-architectures that prevent whole bug classes (leaks, crash-at-boundary, data
  loss). Should be argued as "prevents class X of findings."
- P1 — measurable quality/performance wins with stated order-of-magnitude impact.
- P2 — maintainability, testability, docs, hygiene. Low risk, low urgency.

## Coverage floor (minimum bar; depth passes A–F deepen these, they do not replace them)
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
   (Methodology: Depth passes B and C.)
6. Safety & Concurrency: `unsafe` correctness, the shared `EventQueue` across two windows
   + SMTC thread + forwarder thread, lifetime of `Box<State>` stored in `GWLP_USERDATA`,
   double-free / use-after-free of window state, COM apartment requirements for
   `Windows::Media::Control`. (Methodology: Depth pass D.)
7. Architecture & Maintainability: dead/temporary code, unused features/dependencies,
   divergence between the overlay and main-window render paths, module boundaries that
   have drifted, tray menu / autostart lifecycle. (Methodology: Depth pass D.)
8. Documentation: README/`docs/`/`config.example.toml`/`AGENTS.md` drift from actual
   behavior, missing docs for new settings or interactions. (Methodology: Depth pass D.)

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

## Depth pass A — Security & threat model

**Threat model (state this explicitly in the report's security pass):** single-user,
offline desktop app running with the user's own privileges. No network, no accounts. The
practical attacker is *any other process on the same session* — and one of them (any media
app) controls the SMTC inputs. Consequences you must assume:

- Any co-running process can register an SMTC session, so **all SMTC strings and thumbnail
  bytes are untrusted input**, as is any metadata a compromised media app forwards.
- Any same-user process (or the user) can write to the app's `%APPDATA%` directory and
  replace files or directory entries with reparse points.
- The app must therefore never: crash or corrupt memory on hostile metadata/config
  (Critical), damage or overwrite user data (Critical, rule 1), write through a reparse
  point to an unintended location (High/Mixed), disclose more than necessary in logs or
  `crash.log` (Low–Medium), or misbehave when a hostile second instance appears (Medium).

Audit checklist (verify each; report findings or one-line evidence of a clean trace):

- **A1. SMTC metadata as untrusted input.** For every field that flows from
  `Windows.Media.Control` into events, rendering, or logging:
  - Strings (`track`, `artist`, `album`, app/session id): oversized (>64 KiB), embedded
    NUL, control characters and C1 controls, lone surrogates, ANSI escape bytes, combining
    marks, emoji ZWJ sequences, RTL/bidi text, newlines (multi-line marquee/clipping), and
    strings crafted to look like markup/format specifiers. Check that every downstream
    consumer (marquee, DrawText, history listbox, tooltips, log lines) handles them
    without panicking, cropping wrongly, or misrendering.
  - Numeric fields (`position`, `duration`): negative, zero (division by zero in
    progress-bar/percentage math), `position > duration`, absurd magnitudes (>2^53), and
    rapid changes — verify arithmetic overflow behavior and what the UI shows for each.
  - Thumbnail streams: zero-byte, huge declared pixel dimensions (decompression-bomb —
    verify a pixel/byte cap exists *before* decode, not after), corrupt/truncated frames,
    and a decode failure arriving *while a previous thumbnail is still displayed* (is the
    old artwork kept, leaked, or blanked?).
- **A2. config.toml as untrusted input (hand-edited or process-mutated).** For every
  field: type mismatches (`start_in_tray = "yes"`), out-of-range numbers (i64 overflow),
  invalid enum variants, unknown keys (must be *preserved*, rule 1 — verify the struct
  does not `deny_unknown_fields` yet round-trips unknowns), unparseable TOML (app must
  start with defaults, log the failure, and **not overwrite the file**), 10 MB configs
  (parse time/memory), CRLF/LF/BOM handling. Watch for the silent-fallback anti-pattern:
  an invalid value that quietly resets to a default *without logging* while the file is
  preserved — flag as Medium (silent no-op) or High (if the user's intent is silently lost
  with no log). Check whether config save is atomic (temp file + rename) so a crash
  mid-write cannot corrupt the file.
- **A3. Filesystem: TOCTOU, symlinks, reparse points.** The data dir is user-controlled.
  Check: the config-save and log-truncate paths do not follow a reparse point that
  redirects the write outside the expected directory; the app validates the *identity* of
  the resolved parent directory, not just the path string; a file swapped between read and
  write (TOCTOU) cannot cause data loss or a write elsewhere. Same-user attacker, so
  severity is Medium by default — escalate only if a write can be redirected to a
  location the user could not otherwise modify.
- **A4. Spawn/exec surfaces.** "Open config" / "Open logs" / "Copy logs" / any
  `ShellExecuteW`/`Process::Command`/`explorer.exe` use: the target must be exactly the
  app's own file (never a user-controlled string interpolated into a command line), arg
  lists must be explicit (no string-built command lines — `explorer.exe` miscounts
  comma/flags in paths), and failures (file missing) must be silent-logged, not dialogs.
- **A5. Single-instance protocol.** Mutex name scope (`Local\` vs `Global\`), what a
  second instance does on contention (must exit silently — no pop-ups, rule 2), the race
  where instance 2 checks the mutex before instance 1 has finished creating its windows
  (handoff must not deadlock), mutex abandonment (owner crashed — takeover must be clean),
  and hostile second instances (a process that opens the mutex and holds it must not
  brick the app or cause a popup).
- **A6. Information disclosure in logs and crash.log.** What does Debug logging write:
  raw metadata strings (fine — local), full paths including `%USERNAME%` (acceptable but
  note it), session/app ids (fine); what does the panic handler write to `crash.log`
  (current track title? paths? — and can a hostile 100 KiB metadata string blow up the
  crash-log write itself?). Check `crash.log` growth: it is appended — is it bounded or
  rotated? (An unbounded crash loop on a broken install must not fill the disk.) Verify no
  raw thumbnail bytes, tokens, or file buffers are ever logged.
- **A7. Data integrity during saves.** The settings-save path must: (a) never write
  partial TOML, (b) never clobber a hand-edit made after the in-memory config was loaded
  without telling the user, (c) preserve unknown keys (A2), (d) keep `logs/` untouched.
- **A8. `unsafe` boundary audit.** For every `unsafe` block (including
  `#[allow(unsafe_op_in_unsafe_fn)]` sites): state the invariants it relies on, verify
  the `GWLP_USERDATA` `Box<State>` cannot be double-freed or used after destroy (no
  re-entrancy where a message handler frees the window class data), and verify COM
  objects never cross apartment/thread boundaries (created, used, and dropped on the same
  thread).

## Depth pass B — Memory, GDI/USER handle & leak audit (methodology)

Windows quotas make handle leaks fatal over time: by default each process is limited to
~10,000 GDI objects and ~10,000 USER objects. This pass must prove boundedness by trace,
not by hope.

- **B1. Inventory every object the app creates.** Build the inventory grid explicitly in
  the report. For each object class, list every creation site and assign a lifetime
  bucket — **per-window / per-render / per-track / per-tick**:
  - GDI: font objects (HFONT — created per DPI scale? per state? freed on
    `WM_DPICHANGED`?), brushes (HBRUSH per pill state/hover?), bitmaps (HBITMAP: pill
    buffer, artwork bitmap(s), scaled variants), DCs (window DC, memory DC, layered-window
    GDI-compatible DCs), pens, regions (rounded-rect paths rebuilt per frame?),
    image lists (history listbox), and any stock-vs-created object mismatch.
  - USER: windows (top-level + children: static/button/listbox/tooltips), menus (tray
    menu rebuilt per open?), timers, icons (HICON: tray, window, artwork-derived),
    hooks, hotkeys, clipboard.
  - For **every** object: creation point, owner, and *every* exit path — normal teardown,
    error returns, early returns, `WM_NCDESTROY`, and the "last write" paths in
    `overlay::set_positions`/`set_duration`-style pushes.
- **B2. Pairing discipline.** Verify each `Create*/Get*` has a matching
  `DeleteObject/DestroyIcon/DeleteDC/ReleaseDC/DestroyWindow/RemoveMenu`. Special
  attention: per-frame `SelectObject` must restore the previous object before the DC or
  object is deleted (deleting a still-selected object is a silent leak); the artwork swap
  path must free the *previous* bitmap exactly once even when the new decode fails; icon
  recreation on session change must free the old icon; fonts recreated on DPI change must
  free the old font *before* or atomically *with* replacement.
- **B3. Boundedness of every cache, buffer, and channel.** For each of these, state the
  bound, where it is enforced, and what happens at the bound (evict? drop-newest? drop-
  oldest? grow?):
  - artwork buffers (original + per-DPI scaled variants — is a variant created per size
    *ad infinitum* on drag across monitors?),
  - icon/palette/track caches (is `palette.rs` output cached per artwork and bounded?),
  - the maximized window's per-session history (entries per session, sessions stored,
    fields stored, eviction policy — or unbounded?), the pill's recent-track cache, and
    any period/duration caches,
  - the event channel(s) between SMTC thread, forwarder thread, and the two windows:
    std mpsc is unbounded — what happens to queue depth when the UI thread stalls under a
    churn burst? Is there a drop/coalesce policy on the receiving side?
  - the log: `log-Live.log` truncation at startup (intended, AGENTS.md) vs. crash.log
    growth (see A6).
  Unbounded growth anywhere = High; unbounded growth that can exhaust the process = Critical.
- **B4. Teardown and shutdown ordering** (see also scenario 13): windows destroyed in the
  right order, tray icon removed (`NIM_DELETE`) before its window, timers killed, SMTC
  event registrations revoked and COM refs dropped, the forwarder thread joined (or
  provably safe if detached), and no object referenced after its owner window is
  destroyed.
- **B5. Evidence rule.** Runtime handle counts are not observable headless — do **not**
  claim counts. Deliverable is the completed inventory grid with paired/unpaired status
  per site, each line labeled **Reasoned but not executed** with its code-path citation.

## Depth pass C — Performance & hot paths

- **C1. Identify the hot paths** and trace them line by line:
  - the pill tick + morph spring (frequency? per-tick allocations? springs recomputed
    when nothing changed?),
  - per-frame rendering in `overlay/render.rs`: text rasterization (DrawText/marquee per
    frame on *unchanged* text?), vector primitives rebuilt per frame, alpha compositing,
    and especially the layered-window upload (`UpdateLayeredWindow` copies the full
    surface every frame — check whether unchanged regions are re-uploaded),
  - artwork decode + palette extraction (on which thread? a large JPEG decode is
    10–100 ms — if it runs on the UI thread it stalls frames; is the decode cached?),
  - event forwarding: per-event allocations (String clones), clones duplicated to both
    windows, and queue growth under bursts (B3).
- **C2. Per-frame budgets.** From the trace, state an order-of-magnitude budget: API
  calls/frame, allocations/frame, and bytes uploaded/frame. Flag:
  - per-frame GDI object creation (B2) and per-frame `Vec`/`String`/`Path` construction
    where a reused buffer would do,
  - per-frame `InvalidateRect`/timer churn — full re-render when only the position moved,
    and timers that fire when nothing visual changed (no-change frames should skip
    redraw; coalesce invalidations to once per frame),
  - `DrawText` re-rasterizing identical text every frame (cache the rasterized result;
    marquee only re-rasterizes on text/shift change),
  - wasteful recomputation: geometry, palette, or springs recomputed per frame from the
    same inputs.
- **C3. Improvement directions with estimates.** Every direction must carry an
  order-of-magnitude estimate labeled as an estimate (e.g., "caching the scaled artwork
  per DPI costs ~1–10 MB and removes a ~1–10 ms decode+scale from each track change";
  "skipping DrawText on unchanged text saves roughly X µs/frame at Y ms per call").
  Never present estimates as measurements. Respect the public surface and the single-exe
  rule in every proposal.

## Depth pass D — Architecture, structure, dependencies, docs drift

- **D1. Module boundaries & enforced invariants.** Verify the repo's stated architecture
  actually holds:
  - the pill (`src/overlay/`) is strictly passive: no click targets, no focus, no
    keyboard input;
  - the SMTC worker thread only emits `MediaEvent`s over `mpsc`; the UI thread owns all
    Win32 windows (any window created on the worker or messages sent cross-thread other
    than via `PostMessage`/WM_APP = High);
  - `main_window.rs` is the single owner/writer of in-memory config; settings changes are
    pushed to the overlay via `overlay::set_positions`/`set_duration`, and the positioner
    posts results back via `POSITION_MSG` — **confirm `positioner.rs` never reloads
    config from disk** (AGENTS.md);
  - the shared `EventQueue` (two windows + SMTC thread + forwarder): who locks what, and
    are ordering guarantees (per-session FIFO) actually preserved under concurrency?
- **D2. Dead code & drift.** Unused functions/fields/variants, `#[allow(dead_code)]` and
  stale `#[allow(clippy::*)]` that mask real warnings, features with no visible effect
  (config keys parsed but never read by any render path), duplicate logic between
  `overlay/render.rs` and the main-window render path (a fix applied to one and not the
  other = High), and any TODO/FIXME/XXX/temporary code (`git` history and comments).
- **D3. Error-handling consistency.** Every error path is logged or surfaced — never a
  silent `unwrap`-free fallback that hides failure. Audit every `unwrap`/`expect`/
  `panic!`/`unreachable!`/indexing that can fire on external input (see A1/A2) or during
  teardown: each one is a potential crash class. Initialization failures must have a
  defined degraded mode (e.g., SMTC unavailable → tray + idle pill, logged).
- **D4. Panic/unwind safety across FFI.** On this toolchain (edition 2024, default
  `panic=unwind`), a panic that reaches an `extern "system"` boundary aborts the process.
  Audit every Rust callback invoked by the OS: WNDPROCs, timer procs, dialog procs, COM
  interface methods (the SMTC event handler is a Rust-implemented vtable method). Panics
  in those paths (unwrap on message data, indexing, `?` on poisoned locks) are
  High/Critical crash classes — verify each is panic-free or wrapped in `catch_unwind`.
- **D5. Concurrency correctness beyond the queue.** Atomic statics: verify the ordering
  is correct (Acquire/Release pairs where happens-before matters; flag Relaxed-only state
  used for control flow) and that every `static`/`OnceCell`-held value is `Send`/`Sync`
  and stores no thread-affine window state. Cross-thread window access: only
  `PostMessage` from non-owner threads — flag `SendMessage` from the worker (deadlock/
  re-entrancy). Verify no COM object created on the SMTC thread is dropped on another
  thread (apartment violation).
- **D6. Testability.** What do the existing tests cover (config round-trip? morph math?
  coalescing?)? Recommend extraction of pure logic (event dedup, springs, config
  normalize) into testable functions where message-handler code currently embeds it.
  Propose seams — do not write the tests.
- **D7. Dependency hygiene.** For every entry in `Cargo.toml` (and every windows-rs
  feature): confirm it is actually used (`cargo tree -d` for duplicates; grep for usage
  of each crate and feature). Check `deny.toml`: advisory policy (advisory-only by
  intent — do not re-flag allow-listed advisories, only NEW ones), license allowlist vs.
  each dependency's license (MIT OR Apache-2.0 project), and whether the CI gate and
  `create_exe.ps1` gate run the same deny config. Flag unused crates/features as Low
  (they bloat the exe — LTO/fat/strip is set, so the cost is mostly compile time and
  attack surface) and missing advisories policy as Medium. Any *new* runtime dependency
  proposed in pass F must name the lighter alternative rejected.
- **D8. Docs & config drift.** Reconcile four ways, reporting every mismatch:
  - `config.example.toml` ↔ `config.rs`: every key in code appears in the example with
    matching type/default, and no example key is ignored by code;
  - `docs/configuration.md` ↔ defaults and behavior;
  - `README.md` ↔ actual behavior (features documented exist and behave as described);
  - `AGENTS.md` ↔ code, including the log-contract claims: one `track changed` per song
    change, `track emit skipped` on dedup, `(coalesced)` lines, `WARN ... churning
    sessions ... excluding it`, restart-boundary lines in `log-Live.log`, `start_in_tray`
    semantics. If an AGENTS.md claim has rotted, flag it (Medium) — it misleads future
    auditors.
- **D9. Repo hygiene.** CI (`.github/workflows/ci.yml`) runs the same gates as the local
  pipeline; no build artifacts tracked; `.gitignore` covers `target/` and packaging
  outputs; licenses present for the dual-license claim.

## Depth pass E — Scenario walkthroughs (integration pass — run ALL of them)

For each scenario: trace the code path end-to-end, name every module involved, and report
findings — or state "clean" with one line of evidence from the trace. If the scenario
cannot be exercised headless, label **Reasoned but not executed** and give the maintainer
the exact reproduction steps plus the exact log lines to expect (see AGENTS.md for log
semantics). **Silence is not coverage: record every scenario explicitly.**

1. **Cold start, no media.** No SMTC session at launch → pill idle; a media app starts →
   transition; the app stops → session removed → return to idle; start/stop ×3 within 2 s.
2. **Churn storm.** An app recreates its SMTC session rapidly (~20 sessions/8.5 s, e.g.
   Riot Client behavior). Against AGENTS.md's contract: one debounced
   `SessionsChanged/CurrentSessionChanged` per burst with `(coalesced)` lines, one
   `WARN ... churning sessions ... excluding it` when the cool-down trips, and **no**
   `track changed`/`playback state changed` naming the churning source while on cool-down.
   Also: real track changes *during* churn from a compliant app must still emit.
3. **Hostile metadata live trace.** Feed the A1 battery through a real session: 100 KiB
   title, embedded NUL, lone surrogates, emoji/ZWJ, RTL, ANSI escapes, all-control-char
   title; zero-length thumbnail; 20000×20000 "JPEG"; corrupt stream; and two rapid track
   changes where the second decode finishes before the first (stale-decode race — does a
   late decode overwrite newer artwork?).
4. **Playback-control storm.** 50 play/pause/seek events in 2 s: coalescing holds, no
   event loss that leaves the pill stuck on a stale state, `duration = 0` and
   `position > duration` progress math, negative position.
5. **Hover storm.** Mouse in/out ×10 in 2 s, then park on the edge: springs settle with no
   oscillation, no double-armed timers (timer re-arm leaks a timer or double-fires),
   dismiss timer races with re-entry, pill never ends un-escaped (High if it can).
6. **DPI changes.** Pill on 100% monitor moved to 150% monitor; systemwide DPI change
   while running; DPI change while the maximized window + history listbox are open.
   Verify fonts/bitmaps recreate and old ones free, scaled artwork variants don't
   accumulate (B3), text not clipped.
7. **Multi-monitor + fullscreen toggles.** Pill on secondary monitor with different
   scaling; move across monitors (positioner correctness, partial-off-screen clamping at
   boundaries); fullscreen app on each display → pill hides/shows/fades per fullscreen.rs;
   fullscreen on/off ×5 rapidly; pill straddling two monitors.
8. **Config torture battery.** (a) Corrupt/binary config at startup → app starts with
   defaults, logs a failure, and **does not overwrite the file**; (b) unknown keys
   preserved after a settings save; (c) invalid enum value and out-of-range number →
   logged fallback, no silent default; (d) 10 MB config → parse behavior; (e) hand-edit
   while running, then app-side settings change → does the save clobber the hand-edit?
   (f) key added in-app not in the example file, and vice versa (D8).
9. **Rapid restarts.** 5 launches in 10 s, each killed after 2 s: no stale mutex blocking
   the next start, no orphaned tray icon, no leftover windows, mutex abandonment path to
   clean takeover. Trace the crash path in code (panic in WNDPROC → panic handler →
   crash.log write must not panic itself; a corrupt crash.log must not break the handler).
10. **Display topology changes.** Monitor sleep/wake (pill repositions on wake, monitors
    re-enumerated); monitor removal while the pill sits on it (must not stay orphaned
    off-screen on a dead monitor); resolution change while maximized window is open.
11. **Long-run log growth.** Estimate bytes/day from trace (line lengths × realistic event
    rates) for `log-Live.log` (truncated at startup — intended) and `crash.log`
    (appended — bounded? rotated? A6).
12. **Tray lifecycle + explorer restart.** Tray icon recreated when explorer restarts
    (is the classic `RegisterWindowMessageW("TaskbarCreated")` re-registration handled?
    absent = Medium UX); menu open/close rapid cycles; exit-while-menu-open; autostart
    toggle writes/removes only the app's own HKCU Run key.
13. **Shutdown ordering.** Every exit path: tray exit with media playing, with settings
    open, with maximized window open, with an artwork decode in flight; `WM_ENDSESSION`/
    `WM_QUERYENDSESSION` while running; hard kill (`taskkill`) then relaunch → verify
    AGENTS.md's restart-boundary line behavior (in-app restart preserves the log with a
    boundary; plain launch truncates).
14. **History window long-run.** 100s of sessions accumulate: listbox item count and
    per-session state boundedness (B3), scroll behavior at thousands of entries,
    memory/period caches, and any O(n²) rebuilds when appending.

## Depth pass F — Perfect-state enhancement program

Beyond bug-fixing, propose a concrete program of improvements, each prioritized
**P0/P1/P2** and formatted as: `[Px-0n] Module — change; why it matters; (effort S/M/L;
public-surface impact: preserved / changed, argued)`. P0 items must state which bug class
they make impossible. At minimum cover these families (exemplars, not the ceiling):

- P0: RAII wrappers for GDI objects (Drop-guard deletes) so leak classes in B2 become
  impossible by construction; a bounded/drop-oldest event channel replacing unbounded
  mpsc; artwork decode moved off the UI thread with a generation token (kills the
  stale-decode race); atomic config save (temp+rename) to make corruption impossible;
  typed `GWLP_USERDATA` wrapper with a single teardown owner.
- P1: allocation-free-per-frame render fast path; cached text rasterization; per-DPI
  scaled-artwork cache with LRU bound; palette caching; timer/invalidate coalescing;
  crash.log rotation; per-session history cap.
- P2: unit-test seams for dedup/morph/config-normalize; a test that regenerates or
  validates `config.example.toml` against `config.rs` defaults (kills the D8 drift class);
  CI additions; doc pass.
- Constraint: every proposal must respect the single-exe rule, the additive-config rule,
  and the no-telemetry rule; behavior-changing proposals go into report Section 2.

## Risk register (mandatory — may legitimately be empty, then say "none")

Maintain a table: `ID | Area | Open question / suspected deliberate design | Why it
matters | How to confirm (live step, expected log line, or maintainer decision)`.
Candidates include: history bound is intentional? pill staying visible while the maximized
window is open is intentional? `start_in_tray` default? any `#[allow]` that hides a
latent bug? Anything you flagged but could not resolve one way or the other lands here —
and *stays* here even if you also filed it as a finding.

## Verification tools (read-only; never launch the exe)

You may run only these. **You must never**: launch `WinGlance.exe` or any helper,
run `create_exe.ps1` (neither `-Start` nor `-NoRestart`), use `Start-Process`, take
screenshots, run GUI tooling, or execute anything that could appear on the user's screen —
the user may be in a fullscreen game. **Git is read-only**: `git status`/`git diff`/
`git log` may be used to understand history; no commits, no checkout/reset/stash/clean,
no state changes of any kind. **Never write under the live `%APPDATA%\WinGlance` data
dir**; if a test or command would touch it, do not run it and flag why.

- `cargo fmt --all --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test -j4 --locked` (in-process tests only; verify no test writes to `%APPDATA%`)
- `cargo build --release --locked` (build only — do not run)
- `cargo deny check` (policy in `deny.toml`; advisory-only by intent — do not re-flag
  allow-listed advisories, only NEW ones)
- `cargo tree -d` (duplicate/conflicting transitive deps — informational)

**Verification labeling.** GUI behavior (pill rendering, hover interaction, settings pane,
tray) cannot be observed headless. For every UI/UX or behavioral finding, label the basis:
**Verified** (exercised by a runnable test or the build), **Reasoned but not executed**
(derived from a direct code trace), or **Unable to verify** (sandbox limitation) — and
always cite the exact code path with `file:line` references, so the maintainer can
re-check it live. A finding without a `file:line` citation is incomplete; add it before
reporting.

## Output
- Produce a report formatted for `Analysis.md` (this file). **No code changes, no file
  edits, no git state changes.**
- **Finding schema** — every finding carries: `ID | Severity | Pass/Area | Location
  (file:line) | Basis (Verified / Reasoned but not executed / Unable to verify) |
  Scenario trigger | Description | Suggested direction (for improvements)`.
- Two sections:
  - Section 1: Safe Optimizations (no behavior change).
  - Section 2: Behavioral/Architectural Refactors (preserving the public surface above;
    if a proposal must change the public surface, argue it explicitly here).
  - All finding types — bugs, functional flaws, UI/UX, accessibility, polish suggestions —
    belong in one of the two sections based on whether they change behavior; do not drop a
    finding because it does not fit the old categories.
- End with:
  - **Findings Summary Table** (columns: `ID | Severity | Area | Location | Issue
    (one line) | Scenario | Basis`),
  - **Major Refactors Table** (columns: `ID | Refactor | Behavior change? | Modules
    touched | Effort | Priority (P0/P1/P2) | Why it matters`),
  - **Risk Register** (per the section above).
- **Coverage statement (mandatory).** Close with a per-pass and per-scenario accounting:
  for each of passes A–F and each of the 14 scenarios, state "findings filed" or "clean —
  evidence: <one line>". A pass or scenario with no entry is treated as not performed. A
  pass that found nothing must say so in one line with its evidence. The mandate above is
  a floor: if this audit produces fewer than a handful of findings, you are not done —
  you have not looked hard enough.