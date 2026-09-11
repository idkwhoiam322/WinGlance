# External Review: WinGlance

**Audit target:** `idkwhoiam322/WinGlance` @ `dev-audit-fixes` commit `965ea8ff43572c0f8859553831ce2e447751d10a`  
**Audit date:** 2026-09-12  
**Report branch:** `checkpoint`, created directly from the audited commit  
**Mode:** repository-wide read-only technical audit using GitHub source/history and GitHub Actions evidence. No source file was edited, no executable/helper/packaging script was run, no GUI/screenshot tooling was used, and nothing under live `%APPDATA%\WinGlance` was touched. The only repository write performed for this audit is this report on `checkpoint`.

**Verification status before this report commit:** the exact audited head `965ea8f` had no attached GitHub Actions run/status. The immediately preceding release-gate commit `d199566391f3a8aa8ccb3196a07a65cf3bb87f13` did have a successful checkpoint CI run (`34476785314`): Windows fmt, Clippy with `-D warnings`, tests, release build, `cargo audit`, `cargo deny`; Linux static-metrics/cargo-machete; and the selected deterministic-core mutation gate all completed successfully. The only commits after that gate are `e276201` (restrict idle card to Persistent Compact) and `965ea8f` (align tests with that behavior). This report commit is intentionally tagged `[mutation]` so the checkpoint workflow can re-run the current code through the same path. Until that run finishes, current-head build/test status is **Unable to verify**, while the unchanged subsystems remain supported by the prior successful gate plus direct source trace.

## Executive verdict

WinGlance is substantially hardened and much closer to production-ready than the historical `Analysis.md` that was present on this branch. The prior remediation stack genuinely fixed the old media-identity, accessibility, history-truth, signed-tooltip, monitor-identity, hover-reversibility, and CI-coverage findings. I do **not** recommend reopening those areas or rewriting already-bounded/RAII-managed subsystems for stylistic purity.

I retain **five current findings: 1 Critical, 2 High, 2 Medium**.

The release blockers are narrow but real:

1. `create_exe.ps1 -FreshInstall` can still recursively delete the live `%APPDATA%\WinGlance\WinGlance\data` directory, directly violating the audit's sacred user-data rule.
2. The first-ever application launch still opens the maximized tracking window, directly violating the hard no-popup startup contract.
3. A regression after the earlier audit changed the truthful idle card to `PersistentCompact` only; Expanded, Compact and Auto now intentionally stay hidden while idle, directly violating the hard always-visible-pill/no-media scenario contract.

The two Medium findings are release-quality evidence and documentation drift. The repository now has useful complexity/mutation gates, but they intentionally ratchet historical complexity debt instead of proving the requested absolute thresholds, do not produce a defensible CRAP value, and require zero surviving mutants only in a curated deterministic core. Documentation also contains stale one-way-hover and all-layout-idle descriptions after later code changes.

**Release recommendation: not production-ready under the supplied contract.** Fix `DATA-001`, `START-001`, and `OVERLAY-001` before release. Close `QUALITY-001` with measured evidence/explicitly reviewed exceptions rather than metric theater, then reconcile `DOC-001`. No broad architecture rewrite is justified.

## Scope reconciliation / repo-map drift

The supplied repo map is older than the current branch. Current code additionally includes `src/accessibility.rs`, `src/winapi.rs`, `build.rs`, quality scripts, and `docs/quality.md`. Current ownership is coherent:

- `main.rs`: process startup, singleton/restart handoff, crash logging, SMTC supervisor/forwarder, UI message loop.
- `smtc.rs`: isolated MTA SMTC worker, hostile-input sanitization, bounded async reads, bounded artwork decode/admission, session churn handling.
- `overlay/`: passive layered pill, event reduction, state machine, placement/fullscreen logic, render caches, timers/hooks.
- `main_window.rs`: tracking window, bounded history, Settings, tray lifecycle, config ownership/persistence.
- `accessibility.rs`: Settings, picker and pill UI Automation providers with panic containment.
- `winapi.rs` / `winutil.rs`: raw Win32 facade, callback/state ownership helpers, wide-string safety, verified filesystem writes.
- `icon.rs`: isolated bounded shell-icon worker.
- `positioner.rs`, `process_picker.rs`, `duration_dialog.rs`: user-invoked auxiliary UI; config writes still flow back through the main window.

The architecture guardrail that `positioner.rs` must never reload config from disk still holds. The SMTC worker consumes seeded/live control-mailbox state rather than becoming a second config writer.

---

# Section 1 — Safe Optimizations (no runtime behavior change)

| ID | Severity | Pass/Area | Location | Basis | Scenario trigger | Description | Suggested direction |
|---|---|---|---|---|---|---|---|
| QUALITY-001 | **Medium** | D6/D7/D9/F / release engineering | `.github/workflows/ci.yml:52-130`; `scripts/check_quality_metrics.py`; `scripts/check_mutation_core.py`; `docs/quality.md:18-88` | **Verified as gate design; current-head outcome pending** | Treat the branch as a release candidate and require the supplied absolute quality targets | CI's complexity job deliberately **grandfathers** functions already above cyclomatic/cognitive/Halstead targets at baseline `da06088`; it prevents regression but does not prove every current function is under the requested limits. Mutation requires zero survivors only in an explicitly curated deterministic core. `docs/quality.md` explicitly declines to fabricate CRAP because trustworthy function-level coverage/identity is not available. Therefore `Cyclomatic <22`, `Cognitive <22`, `Halstead <80`, `CRAP <25`, and repo-wide `surviving mutants = 0` are not all established release facts. | Keep the useful monotonic ratchet, but add a complete release inventory that lists every current over-threshold function and refactor only genuine maintainability hotspots. Add trustworthy coverage before computing CRAP; if a defensible join is unavailable, keep CRAP explicitly unverified rather than inventing a number. Extend strict mutation coverage to deterministic logic touched by the remaining fixes; do not mutate platform glue merely to manufacture a zero badge. |
| DOC-001 | **Medium** | D8 / documentation drift | `docs/architecture.md:33-35`; `docs/architecture.md:352-389`; `src/config.rs:219-224`; related startup/idle prose in `README.md`, `docs/configuration.md`, `docs/development.md` | **Reasoned but not executed** | Maintainer/auditor uses architecture docs to reason about idle or hover behavior | Architecture still says startup/settled overlay always renders the passive no-media status, while current code limits it to `PersistentCompact`. The same architecture section and a `config.rs` doc comment still call Expanded hover dismissal “one-way” even though the implementation was deliberately made reversible on leave. Startup docs accurately describe the current first-run popup but thereby contradict the hard product mandate rather than the code. These are contract-level docs used by future reviewers, not harmless wording differences. | Update behavior-coupled docs in the same commits that fix startup/idle. Then do one final docs-only sweep for stale hover/media-identity wording and exact quality-gate semantics. Do not change correct code to match stale prose. |

---

# Section 2 — Behavioral / Architectural Refactors

> **Behavior-change warning:** `START-001` and `OVERLAY-001` produce obvious user-visible behavior changes because the supplied hard mandates require behavior different from the current branch. `DATA-001` changes developer tooling semantics, not application UX.

| ID | Severity | Pass/Area | Location | Basis | Scenario trigger | Description | Suggested direction |
|---|---|---|---|---|---|---|---|
| DATA-001 | **Critical** | A3 / D9 / Rule 1 | `create_exe.ps1:85-92`; `AGENTS.md:40-50` | **Reasoned but not executed** | A developer runs `create_exe.ps1 -FreshInstall` on a normal user profile | The script resolves the actual live data root and executes `Remove-Item -Recurse -Force` on it. The current audit contract says that directory must never be deleted/overwritten once it exists; current `AGENTS.md` independently says the same and permits deletion only of the exact randomized temp created by the current save transaction. A prior maintainer decision described this as a developer-only exception, but the supplied audit explicitly marks the data rule sacred, so this audit cannot carry that exception forward. | Remove the ability to point fresh-install simulation at live APPDATA. If the workflow is useful, require an explicit disposable/sandbox root and hard-refuse any resolved path equal to or under the production WinGlance data root. This prevents an entire sanctioned-tool data-loss class. |
| START-001 | **High** | Functionality / Rule 2 / UX truth | `src/main_window.rs:1439-1459`; `src/config.rs` first-run load policy; `README.md` / `docs/configuration.md` first-run prose | **Reasoned but not executed** | First-ever launch where `config.toml` did not exist, or a legacy config with `start_in_tray = false` | `show_window_once` is true when `cfg.first_run || !cfg.behavior.start_in_tray`, and that path calls `ShowWindow(..., SW_SHOWMAXIMIZED)`. This is intentionally documented and tested, but the supplied hard contract says Start-menu/logon launches must produce no popup and the maximized window is optional UI opened only from the tray. Documented intent/test coverage is not a defense under this audit. | Centralize startup policy so process launch never shows the main window. Keep `start_in_tray` readable for additive/backward config compatibility, but treat it as legacy/no-op if necessary; only explicit tray/user action should raise the tracking window. **NOTICEABLE:** first launch no longer opens the setup window. |
| OVERLAY-001 | **High** | Functionality / Rule 5 / scenario 1 / UX truth | `src/overlay/mod.rs:1473-1485`; regression tests `src/overlay/mod.rs:4966-4992`; overlay creation calls `show_idle` near `src/overlay/mod.rs:4620-4635` | **Reasoned but not executed; behavior is unit-test-pinned** | Cold start with default Expanded layout and no SMTC session; last active source retires with no playing successor; notifications are paused in a transient layout | `show_idle()` now immediately returns unless `layout == PersistentCompact`, and the current test `passive_idle_is_persistent_compact_only` explicitly requires Expanded/Compact/Auto to remain `Phase::Hidden`. This regressed the earlier truthful idle-state remediation and directly contradicts the hard “pill always visible while process is alive” rule plus scenario 1's no-media idle expectation. | Restore the passive `No media playing` / `Notifications paused` status for every configured layout, rendered as a static compact status with no dismiss deadline or continuous animation. Preserve existing explicit fullscreen/listed-foreground suppression policy only as a deliberate temporary visibility exception (see `R-01`), and restore idle when suppression clears. **NOTICEABLE:** a status pill remains present when no media is active. |

---

# Depth pass A — Security & threat model

## Threat model

WinGlance is a single-user, offline desktop app running with the user's privileges. It has no accounts and no network/telemetry surface. The practical hostile input is another process in the same session: any media app can register SMTC and therefore controls metadata strings, timeline values and thumbnail bytes; the same user/process can also modify `%APPDATA%` and can attempt reparse-point/path races. This is an availability/integrity threat model, not a privilege boundary.

### A1 — SMTC metadata as untrusted input

**Clean for the inspected static paths; Reasoned but not executed live.** The worker boundary caps/sanitizes displayed/logged metadata, strips control/bidi separators, bounds thumbnail bytes before decode, decodes to a fixed output, bounds async reads, and uses generation/art identity logic so a stale late decode cannot blindly replace newer content. Timeline/progress code clamps or suppresses unusable states rather than dividing by arbitrary duration values. Current `events.rs` media identity also compares every discriminator known on both sides (duration, track numbers/count, album/subtitle/album artist, playback type and artwork generation) before accepting a one-sided-art refresh. The historical genuine-same-title suppression finding is therefore closed.

Live hostile-provider cases (100 KiB title, corrupt stream, 20000×20000 declared image, lone-surrogate-producing provider, stale completion race) were **not executed** because the audit forbids launching providers/helpers. Exact live reproduction remains in scenario 3.

### A2 — `config.toml` as untrusted input

**Clean; prior unit gate verified at `d199566`, direct code trace unchanged afterward.** The loader has a 1 MiB size bound, staged parsing, per-section typed fallback with warnings, unknown-key preservation, BOM/normal text handling, normalization/clamping, and persistence disablement when an invalid section cannot be safely round-tripped. Save uses revision/conflict checking so a hand edit after load is not silently overwritten. Syntactically invalid or oversized config falls back in memory without replacing the source file.

### A3 — filesystem / reparse / TOCTOU

**Finding filed: `DATA-001`.** Runtime config/log write helpers use verified handles/atomic-save discipline and the app does not need startup data cleanup. The remaining unacceptable deletion surface is the sanctioned developer script's `-FreshInstall` path, which targets the real live data directory.

### A4 — spawn / exec surfaces

**Clean in application code; Reasoned but not executed.** Open/copy/restart actions use app-owned paths and explicit APIs rather than interpolating arbitrary shell command strings. Failures are logged/degraded rather than showing startup dialogs. The audit did not run the packaging script or restart helper path.

### A5 — single-instance protocol

**No code finding; risk retained as `R-03`.** The session-scoped named mutex handles abandoned ownership and restart handoff, and live duplicates fail closed before config/log side effects. A same-user hostile process can still pre-create the predictable mutex and deny launch; the code diagnoses that case. Under the stated same-user attacker model, complete DoS resistance is not achievable with a predictable same-user coordination primitive without risking two writers, so I do not recommend a fail-open “fix.”

### A6 — information disclosure / log growth

**Clean.** `log-Live.log` is deliberately truncated on plain startup and capped during a run; restart preserves a bounded diagnostic chain. `crash.log` has a fixed byte budget and append-only retained-handle path. Raw thumbnail buffers are not logged. Metadata previews are sanitized/escaped and bounded before logging.

### A7 — data integrity during saves

**Clean.** Config persistence is atomic/revision-checked, refuses to overwrite a file externally changed since load, preserves unknown keys, and does not touch `logs/`. `DATA-001` is outside the runtime save path and must not be mistaken for a weakness in `Config::save_checked`.

### A8 — `unsafe` / FFI boundaries

**Clean for inspected boundaries.** Window state ownership uses a claim/`WM_NCDESTROY` handshake; WNDPROCs and WinEvent callbacks are panic-contained; UIA COM methods convert panics into safe error responses; WinRT state remains worker-owned. No unchecked hostile-input pointer dereference was found outside the intended small Win32 boundaries.

---

# Depth pass B — Memory, GDI/USER handles & boundedness

GUI handle counts cannot be measured without running the app, so every row below is **Reasoned but not executed** unless it describes a pure container bound. No runtime count is claimed.

| Object / resource | Creation / lifetime citation | Owner / bound | Pairing / boundedness | Basis |
|---|---|---|---|---|
| Overlay top-level HWND + `OverlayState` | `src/overlay/mod.rs:4540-4735` | UI thread; one | `WM_NCDESTROY` owns final state release; creation-failure claim handshake prevents leak/double-free | Reasoned but not executed |
| Main HWND + `MainWindowState` | `src/main_window.rs:1390-1475` + main wndproc teardown | UI thread; one | state claim transfers at create; destroy path releases once | Reasoned but not executed |
| Main listbox / native tooltip children | `src/main_window.rs` child-control initialization and tooltip code around `:1800-2110` | parent-owned fixed count | child HWNDs die with parent; tooltip timer/track deactivated on teardown | Reasoned but not executed |
| Process-picker popup + listbox | `src/process_picker.rs:560-980` | at most one open picker | failed create drops unclaimed state; success destroys popup/listbox and removes subclass on teardown | Reasoned but not executed |
| Picker fonts/brushes | `src/process_picker.rs:800-870` | fixed set per picker | DPI rebuild deletes old fonts; teardown deletes current objects | Reasoned but not executed |
| Positioner window/GDI objects | `src/positioner.rs` | one user-invoked sample | fixed object set; explicit teardown | Reasoned but not executed |
| Duration dialog/children | `src/duration_dialog.rs` | one modal invocation | parent/explicit destroy semantics | Reasoned but not executed |
| Overlay fonts | `src/gdi.rs`; overlay font provider use in `src/overlay/mod.rs` | DPI/content-scoped provider | replacement/drop deletes owned fonts; no per-frame HFONT creation | Reasoned but not executed |
| Overlay DIB/DC/backing bitmap | `src/overlay/render.rs` render cache | one reusable backing surface plus bounded text/chrome caches | selected objects restored before replacement/deletion | Reasoned but not executed |
| Main artwork blit DC/HBITMAP | `src/main_window.rs` `ArtBlit`/`build_art_blit` | current art/icon only | `Drop` restores selection, deletes bitmap/DC | Reasoned but not executed |
| Tray icon | `src/main_window.rs` tray install/remove + `TaskbarCreated` handler | one `(HWND,uID)` | `NIM_DELETE` on teardown; re-add on Explorer restart | Reasoned but not executed |
| Tray menus | `src/main_window.rs` tray menu builder | one menu tree per open | root destroy owns submenu teardown | Reasoned but not executed |
| Window timers | overlay/main fixed timer IDs | finite named IDs | same-ID `SetTimer` replaces; state transitions/destroy kill timers | Reasoned but not executed |
| Foreground WinEvent hook | `src/overlay/mod.rs:4610-4675` | one hook | unhooked before state release; racing callback posts only through atomic HWND | Reasoned but not executed |
| Toolhelp / process query handles | `src/process_picker.rs`, `src/overlay/fullscreen.rs`, `src/main.rs` | transient | RAII guards / explicit close | Reasoned but not executed |
| Singleton mutex / restart event | `src/main.rs` singleton/relaunch code | fixed per process/handoff | guard/explicit close/process teardown | Reasoned but not executed |
| SMTC subscriptions / WinRT refs | `src/smtc.rs` session table | capped sessions/sources | worker apartment owns refs; resync/teardown drops subscriptions | Reasoned but not executed |
| Hung SMTC workers | `src/main.rs` supervisor | process-lifetime `MAX_LEAKED_WORKERS` budget | wedged COM thread may be abandoned but cannot grow without bound | Reasoned but not executed |
| Icon jobs / shell extraction | `src/icon.rs` | bounded worker queue | one worker; per-job handles cleaned; breaker prevents unbounded blocked jobs | Reasoned but not executed |
| Worker event channel | `src/main.rs` / `src/smtc.rs` | cap 1024 | full -> retry/coalesce rather than grow | Static bound |
| Worker retry mailbox | `src/smtc.rs` | cap 256 | newest authoritative state supersedes/coalesces | Static bound |
| Main + overlay forwarder queues | `src/main.rs` / `src/events.rs` | cap 256 each | newest-wins/drop policy; failed wake clears affected queue | Static bound |
| Overlay pending notifications | `src/overlay/mod.rs` | cap 4 | oldest unshown dropped; current pill not pulled | Static bound |
| Overlay track cache | `src/overlay/mod.rs` | cap 8 | LRU bounded | Static bound |
| Playback/source ledger | `src/overlay/mod.rs`, `src/main_window.rs` | cap 64 | stopped-first/defined eviction | Static bound |
| Main history | `src/main_window.rs` `History::new`/`push` | cap 400 | oldest rows evicted | Static bound |
| Artwork in flight | `src/events.rs` artwork lifetime + `src/smtc.rs` admission | 64 MiB budget | final shared lifetime token releases reservation; over-budget art stripped while metadata survives | Static bound |
| `log-Live.log` | `src/logging.rs` | 1 MiB run cap | stops accepting complete lines at cap | Static bound |
| `crash.log` | `src/main.rs:100-150` and crash init | 8 MiB cap | atomic reservation prevents concurrent overrun | Static bound |

**B2 conclusion:** no unpaired GDI/USER creation site was found in the inspected paths. Some auxiliary windows still use explicit manual Win32 cleanup, but it is fixed-count and paired; converting every handle to a new wrapper solely for aesthetic uniformity would increase change risk without solving an observed leak.

**B3 conclusion:** the event path, history, track caches, artwork payloads and logs are all bounded. The prompt's exemplar “replace unbounded `mpsc`” work is already materially done.

**B4 conclusion:** shutdown ordering is coherent by trace: UI-owned timers/hooks/tray/window resources are detached before state destruction; forwarder/control threads have explicit termination paths; COM worker ownership is kept on its apartment, with bounded intentional abandonment only for an irrecoverably wedged worker.

---

# Depth pass C — Performance & hot paths

## C1 — hot-path trace

The current render architecture already contains the high-value optimizations the audit program asks for:

- reusable layered-window DIB/backing memory rather than new bitmap/DC construction each frame;
- reusable UTF-16/frame scratch;
- DPI-scoped font ownership;
- pre-resolved/cached pill text and marquee rasters;
- static chrome caching;
- render dirty-gating so unchanged frames skip raster/upload;
- monitor/config-limited animation cadence, with slower static/aura cadence;
- artwork read/decode on the worker, not the UI thread;
- bounded/coalesced event transport using shared `Arc<MediaEvent>` payloads;
- cached palette/art state instead of re-decoding every frame.

I found no evidence justifying another render rewrite before profiling.

## C2 — order-of-magnitude frame budget

These are **estimates, not measurements**. At the shipped `max_width = 340` logical px and a roughly 150–180 px expanded height, a 32-bit layered surface is about:

- 100% DPI: ~0.2–0.3 MiB,
- 150% DPI: ~0.45–0.65 MiB,
- 200% DPI: ~0.8–1.1 MiB.

A dirty 60 Hz animation therefore implies roughly **10–70 MiB/s** of full-surface upload traffic; a ~15 Hz visual-only cadence is roughly **3–17 MiB/s**. The important current behavior is that no-change frames can skip the upload entirely.

Warm-frame heap allocation is approximately **O(0)** in the common path because frame/text/font/chrome buffers are retained; content/DPI/cache misses allocate. A large but valid compressed image decode may plausibly cost **~10–100 ms** on commodity hardware, but current decode runs off the UI thread and emits a fixed-size decoded buffer, so the failure mode is delayed artwork, not a blocked window message pump.

## C3 — improvement directions

No new runtime dependency or broad cache layer is recommended. When `OVERLAY-001` is fixed, keep the idle card fully static: one upload when content/placement changes, then zero continuous repaint traffic while idle. Add developer-only/profile instrumentation only if a future performance complaint gives a concrete target.

---

# Depth pass D — Architecture, structure, dependencies, docs drift

### D1 — boundaries and invariants

- **Passive pill:** holds. No focus/click/keyboard interaction surface is introduced by hover; hover is observation of cursor position.
- **UI-thread ownership:** holds. Win32 windows are UI-thread-owned; worker/forwarder communication is channel + `PostMessage`, not synchronous cross-thread `SendMessage` into UI state.
- **Config ownership:** holds. Main window remains the writer; overlay receives pushed state; positioner returns results rather than loading config.
- **SMTC config isolation:** holds. Worker receives seed/control-mailbox values rather than taking the shared config lock.
- **Event ordering/bounds:** explicit bounded queues and coalesce/drop semantics replace unbounded backlog.

### D2 — dead/redundant code

No obvious dead module or parsed-but-never-used current feature remains in the inspected tree. The prior branch work removed/rewrote several stale paths. However, the absolute requirements **Dead code = 0** and **Redundant code = 0** are stronger than a static visual review can certify; current-head Clippy/metrics evidence must be attached before release. Do not force abstraction of deliberately explicit Win32 teardown code merely to make two blocks look less repetitive.

### D3 — error handling

External/config/file failures generally log and degrade rather than panic. Lock poisoning is usually recovered with `into_inner`; callback bodies are contained. Startup has defined degraded modes for tray installation, media worker failure and icon extraction. No hostile-input `unwrap`/index crash class was found in the audited paths.

### D4 — panic/unwind safety across FFI

Strong. Overlay/main WNDPROCs, WinEvent/subclass callbacks and UIA COM methods are wrapped so Rust panics do not cross `extern "system"`/COM ABI boundaries. UIA uses `E_FAIL` for caught callback panic rather than pretending a panic is a normal empty provider answer.

### D5 — concurrency

Thread-affine COM/WinRT ownership remains worker-local; UI state is not stored in cross-thread globals except safe identifiers/snapshots. Atomics used for lifecycle/budget coordination are paired with appropriate stronger orderings where ordering matters; relaxed counters are accounting-only. No synchronous worker-to-UI `SendMessage` deadlock path was found.

### D6 — testability

The remediation branch has useful pure seams for config normalization/save conflict, queue bounds, media identity, morph/hover math, monitor resolution, history helpers, state ownership and signed coordinate packing. `START-001` and `OVERLAY-001` already have policy/state tests, but those tests currently pin the wrong hard-contract behavior. Fix the policy, then mutate/test the corrected decision functions rather than testing only Win32 side effects.

### D7 — dependency hygiene

Direct dependencies all have visible roles: `windows`, `windows-core`, `windows-future`, `anyhow`, `chrono`, `dirs`, `image` (JPEG/PNG), `log`, `serde`, `toml`, and build-only `embed-manifest`. `cargo-machete` is already in the metrics job, with `windows-core` documented as a macro-expansion scanner exception. `cargo audit`/`cargo deny` passed at `d199566`; current-head evidence is pending the report-triggered checkpoint run. No new runtime dependency is proposed anywhere in this plan.

### D8 — docs/config drift

**Finding filed: `DOC-001`.** Config example/defaults are materially aligned and the prior invalid-section/monitor/cache docs drift was fixed. Remaining drift is concentrated around idle/hover behavior plus startup wording that must change with the mandated startup fix.

### D9 — repo hygiene / CI

The repository ignores build/data/log/package outputs, contains both license files for the dual-license claim, and separates release write permission from untrusted check jobs. The release-quality evidence gap is `QUALITY-001`, not an absence of CI.

---

# Depth pass E — scenario walkthroughs

Unless explicitly tied to a unit test or prior CI result, GUI/SMTC outcomes below are **Reasoned but not executed**. No app/provider/helper was launched.

| # | Scenario | Trace / result | Live reproduction + expected evidence |
|---:|---|---|---|
| 1 | Cold start, no media | **Findings filed: `START-001`, `OVERLAY-001`.** `main` loads config, creates overlay/main window, then current main-window startup can maximize on first run while transient layouts refuse idle content. | In a disposable Windows profile with no media session, launch normally; expect after fixes: no tracking window popup and a static `No media playing` pill. Start/stop a compliant player ×3 within 2 s; each real event replaces idle and the final retirement returns to idle. |
| 2 | Churn storm | **Clean by static trace.** Session dirtying/resync is debounced, churn is tracked per source, excluded sources cannot emit while cooled down, and compliant sources retain independent state. | Use a session-recreating source (~20/8.5 s) plus a compliant player changing tracks. Expect debounced/coalesced log lines, one churn warning when threshold trips, no media emits for excluded source, normal source still emits. |
| 3 | Hostile metadata live trace | **Clean by boundary trace; not live-executed.** Metadata/artwork/read bounds and stale-art defenses are present. | Feed 100 KiB title, controls/bidi/emoji, zero/corrupt image, huge declared dimensions, and out-of-order decode completions. Expect sanitized/bounded text, no crash, decode failure placeholder/retained safe state, and no late old art replacing newer track. |
| 4 | Playback-control storm | **Clean by bounded/coalesced trace.** Newest authoritative state survives queue pressure; progress path handles absent/invalid duration defensively. | Send 50 play/pause/seek events in 2 s incl. zero duration, negative/overshoot position. Final pill/state must match latest authoritative state; no division/overflow crash. |
| 5 | Hover storm | **Clean after remediation.** Expanded hover cap is reversible on debounced leave; Compact morph has dedicated reversal/hold logic; fixed timer IDs prevent timer accumulation. | Move in/out ×10 then park on edge. Expect no permanent stuck state or deadline extension loop; leaving before Expanded cap restores pre-hover deadline. |
| 6 | DPI changes | **Clean by resource/geometry trace.** DPI-scoped fonts/surfaces are replaced rather than accumulated; layout uses target monitor DPI. | Move 100%→150%, change system DPI, repeat with tracking/history open. Check text clipping and GDI counts manually if desired; audit does not claim runtime counts. |
| 7 | Multi-monitor + fullscreen | **Finding relevance: `OVERLAY-001`; risk `R-01`.** Stable monitor identity and signed tooltip fixes are present. Fullscreen/listed-foreground suppression remains a deliberate behavior. | Move between mixed-DPI monitors, toggle fullscreen ×5, remove/re-add target. Pill should clamp/recover and use stable identity. After idle fix, suppressed fullscreen may temporarily hide only according to finalized `R-01` policy, then idle/media state must restore. |
| 8 | Config torture | **Clean by parser/save trace.** Oversize/invalid parse does not overwrite source; invalid typed section disables persistence; unknown keys survive; external edit conflicts refuse save. | Use a disposable copied config, never live production data. Test corrupt/10 MB/unknown/out-of-range/hand-edit cases; expect warning + in-memory fallback/conflict without file clobber. |
| 9 | Rapid restarts | **Clean by singleton/handoff trace; `R-03` retained.** Abandoned mutex can be acquired; duplicate live instance fails closed; restart nonce/event handoff is bounded. | Launch/kill only in maintainer-controlled live test. Verify no stale mutex blocks recovery and crash/restart boundaries follow logging contract. Hostile same-user mutex squatting remains a diagnosed availability limitation. |
| 10 | Display topology changes | **Clean by current monitor identity/cache invalidation trace.** `WM_DISPLAYCHANGE`, re-enumeration and persisted device identity prevent the old restart-dependent index ambiguity. | Sleep/wake/remove target/change resolution. Expect primary fallback while absent, restoration by stable identity when it returns, and no orphaned off-screen pill. |
| 11 | Long-run log growth | **Clean.** Live log is capped at ~1 MiB; crash log at ~8 MiB. | Churn for a long session; size must plateau at caps. Plain launch truncation remains intended; restart chain preserves bounded diagnostics. |
| 12 | Tray lifecycle + Explorer restart | **Clean by static trace.** `TaskbarCreated` handling re-adds icon; retry/backoff covers initial shell absence; autostart owns only its Run value. | Restart Explorer, open/close menu rapidly, exit with menu open. Tray icon should return and no stale menu/icon remain. |
| 13 | Shutdown ordering | **Clean by static trace.** Hooks/timers/tray/window state have ordered teardown; worker/control paths have bounded shutdown semantics; wedged COM worker is only abandoned under a process-lifetime budget. | Quit with media/settings/history active, then Windows session end/hard kill in controlled test. Relaunch must not see stale ownership or corrupt config/log state. |
| 14 | History long-run | **Clean.** History is cap 400; text-only history drops image payloads; per-source/track ledgers are bounded; insertion keeps reader scroll stable. | Generate >400 transitions. Row count must remain capped, scrolling remain stable, and memory must not retain art per historical row. |

---

# Depth pass F — perfect-state enhancement program

The prompt's exemplar P0/P1 work should **not** be reimplemented where the branch already solved it. Current status:

| Exemplar family | Current state | Audit decision |
|---|---|---|
| GDI/resource ownership | Main/overlay hot resources already use owned/drop discipline; auxiliary manual objects are fixed-count and paired | **No rewrite recommended** |
| Bounded/drop-oldest event transport | Worker channel/mailbox/window queues/pending pill queue are bounded | **Already satisfied** |
| Artwork decode off UI + generation/stale-art defense | Worker decode and stale identity/generation defenses are present | **Already satisfied** |
| Atomic config save / conflict detection | Temp/verified/checked save path present | **Already satisfied** |
| Typed `GWLP_USERDATA` single teardown owner | Claim/`WM_NCDESTROY` ownership helper present | **Already satisfied** |
| Allocation-reduced render / cached text/chrome | Reusable DIB/scratch/cache/dirty gating present | **Already satisfied** |
| Per-DPI artwork/cache bound | Fixed decode + bounded cache; no unbounded per-size cache observed | **Do not add one without profiling** |
| Palette caching | Current art/palette state cached | **Already satisfied** |
| Timer/invalidate coalescing | Fixed timers + dirty render gating | **Already satisfied** |
| Crash-log rotation/bound | Hard byte cap exists; rotation is unnecessary for the stated safety goal | **Already satisfied** |
| Per-session/history cap | History and source/session structures bounded | **Already satisfied** |
| Pure test seams | Substantial pure helper coverage now exists | **Extend only around remaining fixes** |
| Docs/config parity | Mostly improved; `DOC-001` remains | **P2 active** |

Active enhancement/fix program:

- **[P0-01] `create_exe.ps1` — isolate fresh-install simulation from live APPDATA; prevents sanctioned-tool user-data deletion as a class; (effort S; public-surface impact: preserved, developer-tool contract changes).**
- **[P1-01] `main_window.rs` startup policy — make every process launch main-window-hidden and require explicit tray/user action to show it; closes a hard startup-contract defect; (effort S; public-surface impact: changed, first-launch popup removed).**
- **[P1-02] `overlay/mod.rs` idle reducer — make the truthful passive idle card layout-independent while keeping it static; closes hidden-idle state and keeps no-change render cost ~zero; (effort S/M; public-surface impact: changed, idle pill visible).**
- **[P2-01] CI/quality scripts — publish a complete absolute complexity inventory and trustworthy coverage identity before CRAP; retain the monotonic ratchet and strict deterministic mutation gate; (effort M; public-surface impact: preserved).**
- **[P2-02] deterministic tests/mutation scope — include corrected startup and all-layout idle policy decisions, including disabled-notification/retirement restoration; (effort S/M; public-surface impact: preserved).**
- **[P2-03] docs — reconcile hover, idle, startup and exact quality semantics after behavior fixes; (effort S; public-surface impact: preserved).**

No new runtime crate is justified. For quality tooling, CI-only pinned tools remain lighter and safer than shipping analysis dependencies in `WinGlance.exe`.

---

# Release-quality metric assessment

| Requested target | Current audit result | Release action |
|---|---|---|
| Cyclomatic Complexity `< 22` | **Not established repo-wide.** CI ratchets against `da06088` and explicitly grandfathers existing debt. | Emit complete inventory; refactor only genuine over-threshold hotspots until the release policy is satisfied or a narrowly documented platform-boundary waiver is approved. |
| Cognitive Complexity `< 22` | **Not established repo-wide** for the same reason. | Same. Prefer extracting pure decision helpers over reshuffling Win32 code solely for score. |
| Halstead Difficulty `< 80` | **Not established repo-wide**; current analyzer is present but ratcheted. | Same complete-inventory policy. |
| CRAP `< 25` | **Unable to verify.** Current docs correctly state that trustworthy per-function coverage identity is unavailable. | Add trustworthy coverage+identity first; do not fabricate CRAP. If tooling remains unreliable, record an explicit release exception rather than a fake pass. |
| Surviving mutants `0` | **Verified only for selected deterministic remediation core at `d199566`; not repo-wide.** | Keep zero-survivor requirement for deterministic logic; expand to newly changed startup/idle helpers. Treat equivalent/platform-boundary mutants by explicit review, not blanket skips. |
| Dead code `0` | **No obvious dead production module found; not yet certified at current head.** | Require current checkpoint Clippy + unused-dependency/static scan. |
| Redundant code `0` | **Cannot be mechanically certified as an absolute.** | Use Clippy + architecture review; do not collapse explicit teardown/error paths when duplication improves safety. |
| `any` / `unknown` types `0` | **Not a Rust-language metric.** | Audit `dyn Any`, raw opaque payloads and unchecked casts instead. Do not confuse required TOML unknown-key preservation with a type escape hatch. |

This is deliberately strict about what is evidence versus aspiration. A release report should never print a synthetic zero for a metric the toolchain did not actually measure.

---

# Findings Summary Table

| ID | Severity | Area | Location | Issue (one line) | Scenario | Basis |
|---|---|---|---|---|---|---|
| DATA-001 | **Critical** | User data / developer tooling | `create_exe.ps1:85-92` | `-FreshInstall` recursively deletes the live sacred data directory. | Tooling / A3 | Reasoned but not executed |
| START-001 | **High** | Startup / UX contract | `main_window.rs:1439-1459` | First run/legacy false can maximize the tracking window during process launch. | 1 | Reasoned but not executed |
| OVERLAY-001 | **High** | Overlay state / UX truth | `overlay/mod.rs:1473-1485`, `4966-4992` | Transient layouts intentionally remain hidden instead of showing the mandated idle pill. | 1, 7 | Reasoned but not executed; unit-test-pinned |
| QUALITY-001 | **Medium** | CI / release quality | `ci.yml:52-130`; `docs/quality.md:18-88` | Current gates do not establish all requested absolute complexity/CRAP/repo-wide mutation targets. | Release gate | Gate design verified; current-head run pending |
| DOC-001 | **Medium** | Documentation | `architecture.md:33-35,352-389`; `config.rs:219-224` | Architecture/commentary contradict current idle and reversible-hover behavior. | D8 / future maintenance | Reasoned but not executed |

---

# Major Refactors / Improvements Table

| ID | Refactor | Behavior change? | Modules touched | Effort | Priority | Why it matters |
|---|---|---|---|---|---|---|
| P0-01 | Make fresh-install tooling physically incapable of targeting live data | Developer tooling only | `create_exe.ps1`, optional script tests/docs | S | P0 | Prevents the data-loss class rather than relying on operator care. |
| P1-01 | Central startup visibility policy | **Yes** | `main_window.rs`, startup policy tests, coupled docs | S | P1 | Makes launch-time popup impossible and satisfies the hard startup contract. |
| P1-02 | Layout-independent passive idle state | **Yes** | `overlay/mod.rs`, overlay policy tests/UIA text, coupled docs | S/M | P1 | Restores truthful always-present status without continuous render cost. |
| P2-01 | Complete quality inventory + trustworthy coverage basis | No runtime | CI/scripts/docs | M | P2 | Converts release targets into evidence while avoiding fake metrics. |
| P2-02 | Targeted metric hotspot reductions, only where inventory proves genuine debt | No intended runtime | affected source modules discovered by inventory | M/L | P2 | Brings real maintainability hotspots under target without risky broad rewrites. |
| P2-03 | Contract documentation sweep | No runtime | architecture/config/readme/development/quality docs | S | P2 | Prevents future auditors/maintainers from coding to stale behavior. |

---

# Risk Register

| ID | Area | Open question / suspected deliberate design | Why it matters | How to confirm |
|---|---|---|---|---|
| R-01 | Always-visible rule vs fullscreen suppression | The supplied hard rule says the pill is always visible, while scenario 7 and current `hide_for_auto_compact_sources` behavior explicitly expect hide/show around fullscreen/listed foregrounds. | Literal “never hidden for any reason” would remove a deliberate gaming behavior and conflict with another required scenario. | Maintainer decision during `OVERLAY-001`: recommended interpretation is “idle/media pill is always present whenever overlay display is not explicitly suppressed by the user's fullscreen/listed-foreground policy; suppression is temporary and the correct state restores immediately afterward.” Live-toggle fullscreen and verify restore. |
| R-02 | Exact-identical media replay identity | `same_media` now checks all known discriminators, but two genuinely distinct plays can be observationally identical if source/title/artist/all optional identity fields and cover are identical or absent. | No algorithm can distinguish events the provider makes identical without another trustworthy signal; over-tightening can reintroduce duplicate late-metadata pills. | Live provider trace. If a real swallowed replay is observed, add a worker provenance/timeline-reset discriminator backed by logs/tests. Do **not** change current identity code speculatively. |
| R-03 | Same-user singleton name squatting | Current fail-closed named mutex is intentionally susceptible to a same-user process that holds the known name. | A fail-open workaround risks two WinGlance writers racing `config.toml` and logs; under same-user threat assumptions the attacker can also terminate the process, so full DoS resistance is not a meaningful security boundary. | Keep the diagnostic live-instance probe. Only redesign if the product threat model changes; any replacement must prove single-writer integrity before it is preferred over the current limitation. |
| R-04 | Historical `start_in_tray = false` meaning | Additive compatibility keeps the key readable, but the hard startup contract leaves no safe launch-time meaning for `false`. | Removing the field would break additive config compatibility; honoring it would reintroduce `START-001`. | Maintainer decision: recommended contract is parse/preserve the legacy key but never auto-show on process launch. Explicit tray action remains the only main-window opening path. |

---

# Architecture-reviewed minimal implementation plan

This plan is intentionally short. The branch already contains the hardening work that the audit prompt lists as exemplars; repeating it would add risk without production value. Execute each commit with the requested loop: **Architect review → implement → Architect review/amend until satisfied → move to next commit only when satisfied**. No source change should begin until maintainer go-ahead.

1. **`fix(tooling): isolate fresh-install simulation from live app data`**  
   Remove `-FreshInstall`'s direct `%APPDATA%\WinGlance\WinGlance\data` delete. Preferred implementation: require an explicit disposable test root, canonicalize/resolve it, and hard-refuse the production data root (and descendants/aliases that resolve there). Add a script-level pure/path guard test if practical. **Application UX: unchanged. Developer tooling behavior: intentionally changed.** This closes `DATA-001` and is first because no later audit/build workflow should retain a sanctioned data-destructive path.

2. **`fix(startup): make process launch tray-only`**  
   Centralize startup visibility so `create_window` always starts the tracking window hidden. Preserve parsing/round-tripping of legacy `start_in_tray`; do not let it re-enable launch popups. Update startup unit tests and the directly coupled README/configuration/development wording in this same commit. **NOTICEABLE USER BEHAVIOR CHANGE:** first-ever launch no longer opens the maximized setup window. This is required by the supplied hard contract, not a discretionary UX tweak.

3. **`fix(overlay): restore truthful idle status across layouts`**  
   Remove the `PersistentCompact`-only early return from the passive status policy. Expanded/Compact/Auto should settle into the same static compact `No media playing` / `Notifications paused` status when no real event is active; real events still use their configured layout. Keep the status free of dismiss deadline, marquee/progress/comet/hover actions, so it uploads once and stays cold. Preserve the finalized `R-01` fullscreen/listed-foreground suppression exception and restore the appropriate idle/media state as soon as suppression clears. Update the unit tests that currently require transient `Phase::Hidden`, and include the corrected policy in mutation scope. **NOTICEABLE USER BEHAVIOR CHANGE:** an idle status remains visible outside deliberate suppression.

4. **`chore(quality): make release metrics complete and auditable`**  
   Keep the existing monotonic complexity ratchet but also publish a complete current-head inventory of functions over cyclomatic/cognitive/Halstead targets. Add a trustworthy function-identity coverage report before enabling CRAP; if the toolchain still cannot support that join, make the release exception explicit rather than synthetic. Expand deterministic mutation selection to the startup/idle decision helpers changed above. Keep runtime dependencies unchanged. **No application behavior change.**

5. **`refactor(quality): reduce genuine remaining metric hotspots`**  
   Using commit 4's inventory, refactor only functions that actually exceed the requested thresholds *and* where extraction improves readability/testability. Prefer pure decision helpers and smaller WNDPROC/render dispatch functions. Do not alter behavior, FFI ownership ordering, or duplicate explicit teardown merely to game a score. If the inventory proves a platform-boundary function cannot be reduced safely, record a narrow reviewed waiver rather than forcing a riskier rewrite. **No intended user-visible behavior change.** Architect review may split this into one commit per unrelated module if the inventory reveals multiple independent hotspots; do not combine unrelated source rewrites merely to keep the nominal commit count low.

6. **`docs: reconcile architecture and release contracts`**  
   Final docs-only sweep after the behavior/quality commits: remove stale one-way-hover prose, make idle/fullscreen/startup semantics consistent across `architecture.md`, `configuration.md`, `development.md`, README and `config.rs` comments, update media-identity wording to reflect all known discriminators, and describe the exact quality evidence/waivers actually enforced. **No runtime behavior change.**

**No implementation commits are recommended for** event-channel bounds, config atomicity, artwork threading/generation, crash-log cap, broad GDI RAII, render scratch allocation, palette caching, stable monitor identity, history cap/disposition, signed tooltip positioning, picker UIA, Settings focus math, Activity contrast, or reversible hover dismissal. Those are already materially correct on this branch; changing them again would be change for change's sake.

---

# Coverage statement

## Depth passes

- **Pass A — findings filed:** `DATA-001`; singleton and exact-media ambiguity retained as risks. Evidence: hostile-input/config/filesystem/singleton/log/unsafe traces above.
- **Pass B — clean:** no unpaired or unbounded resource class found in the inventory; runtime handle counts not executed.
- **Pass C — clean:** major hot-path anti-patterns already addressed; no speculative rewrite justified without profiling.
- **Pass D — findings filed:** `QUALITY-001`, `DOC-001`, plus `START-001`/`OVERLAY-001` as hard architecture/behavior contract violations.
- **Pass E — findings filed:** scenario 1 exposes `START-001` + `OVERLAY-001`; scenario 7 depends on the `R-01` visibility interpretation; all 14 scenarios are accounted for above.
- **Pass F — findings/program filed:** six-commit minimal production-readiness plan; exemplar hardening already present is explicitly marked no-change.

## Scenario accounting

1. **Cold start, no media — findings filed:** `START-001`, `OVERLAY-001`.
2. **Churn storm — clean:** per-source debounced resync + cool-down exclusion; compliant sources remain independent.
3. **Hostile metadata — clean by trace / live unable to verify:** bounded/sanitized metadata, bounded decode, stale-art defense.
4. **Playback-control storm — clean by trace:** bounded newest-state transport and defensive progress math.
5. **Hover storm — clean:** reversible Expanded cap + Compact morph/leave logic; fixed timer IDs.
6. **DPI changes — clean by trace:** DPI-scoped resources replaced, not accumulated; target-DPI placement.
7. **Multi-monitor/fullscreen — finding/risk accounting:** overlay idle fix needed; stable identity is fixed; fullscreen visibility interpretation is `R-01`.
8. **Config torture — clean by trace/prior tests:** staged fallback, size cap, unknown preservation, conflict-safe save.
9. **Rapid restarts — clean by trace; risk retained:** abandoned takeover/restart handoff bounded; same-user name squatting is `R-03`.
10. **Display topology — clean by trace:** cache invalidation + persisted stable monitor identity + fallback/recovery.
11. **Long-run logs — clean:** live/crash logs hard-capped.
12. **Tray lifecycle + Explorer restart — clean by trace:** TaskbarCreated re-add + retry/backoff + owned Run key.
13. **Shutdown ordering — clean by trace:** timers/hooks/tray/windows/state teardown ordered; wedged worker abandonment bounded.
14. **History long-run — clean:** cap 400, text-only retained rows, bounded source caches, scroll-preserving insert.

## Final release decision

Under the supplied mandates, the current audited code should **not** ship until `DATA-001`, `START-001`, and `OVERLAY-001` are corrected. The remaining work after those is evidence/hygiene: make release-quality claims match what is actually measured, reduce only genuine measured hotspots, and reconcile stale docs. The branch does **not** need another broad hardening rewrite.
