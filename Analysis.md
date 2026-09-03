# External Review: WinGlance

**Audit target:** `idkwhoiam322/WinGlance` @ `dev-audit-fixes` commit `32f27897d5f2370997d597b52045279cd96c628c`  
**Audit date:** 2026-09-04  
**Mode:** repository-wide static audit using GitHub source/history and existing workflow metadata only. No executable, helper, packaging script, GUI tool, screenshot tool, or live `%APPDATA%\WinGlance` path was touched.  
**Verification status at audited head:** **Unable to verify** build/test/lint/advisory status. The audited commit has no GitHub combined status/check records and no workflow run associated with the head. I therefore do **not** claim `fmt`, `clippy`, tests, release build, `cargo deny`, `cargo audit`, complexity limits, coverage, or mutation results pass at this head.

## Executive verdict

The branch is substantially more production-hardened than the audit prompt's starting assumptions. The current code already has bounded event/mailbox/window queues, a bounded artwork byte budget, off-UI artwork decode, stale-art generation rejection, per-source/session caps, rate-limited/bounded logging, atomic revision-checked config saves, verified reparse-resistant file opens, callback panic containment, a typed window-state ownership handshake, UIA coverage for the Settings pane, DPI-scoped font ownership, cached render surfaces/text, bounded history, Explorer tray recreation, and explicit shutdown paths.

I found **one Critical, three High, ten Medium, and three Low** findings. The Critical is a direct user-data deletion path in the packaging script. The High findings are the forced first-run window popup, the absence of the mandated no-media idle pill, and a media-identity suppression rule that can merge a genuine same-title transition when one side temporarily lacks artwork. The remaining findings are accessibility/UX truth, multi-monitor semantics, singleton availability, documentation drift, and release-gate coverage.

**Release recommendation:** **not production-ready yet**. Fix `DATA-001`, `START-001`, `OVERLAY-001`, and `DEDUP-001` first; then close the Medium correctness/accessibility findings and establish CI evidence for the release-quality gates. I do **not** recommend broad rewrites of already-hardened subsystems merely to resemble the exemplars in the audit prompt.

## Scope reconciliation / repo-map drift

The provided starting map is stale relative to `dev-audit-fixes`: the branch additionally has `src/accessibility.rs`, `src/winapi.rs`, and `build.rs`, and the current `docs/development.md` describes those roles. That is prompt-map drift, not a repository documentation defect by itself. The important current ownership model is:

- `main.rs`: startup, singleton/restart handoff, crash logging, supervisor/forwarder, UI message loop.
- `smtc.rs`: MTA SMTC worker; hostile-input sanitization, bounded reads/decode/admission, per-source coalescing.
- `overlay/`: passive layered pill, event reduction, state machine, display resolution, render cache, timer/hook lifetime.
- `main_window.rs`: tracking window, bounded history, Settings, tray, config ownership/persistence, picker/dialog integration.
- `accessibility.rs`: Settings/pill UIA providers with callback panic containment.
- `winapi.rs` / `winutil.rs`: raw Win32 facade, state ownership, callback guards, verified filesystem writes, small RAII helpers.
- `icon.rs`: isolated bounded shell-icon worker.
- `positioner.rs`, `process_picker.rs`, `duration_dialog.rs`: user-invoked auxiliary UI; no config reload in the positioner.

---

# Section 1 — Safe Optimizations (no runtime behavior change)

| ID | Severity | Pass/Area | Location | Basis | Scenario trigger | Description | Suggested direction |
|---|---|---|---|---|---|---|---|
| DOC-001 | Medium | D8 / Documentation | `docs/configuration.md:1-18`; `src/config.rs:600-775` | Reasoned but not executed | Start with a syntactically valid TOML file containing a typed-invalid value such as `layout = "bogus"` | Documentation says a typed-invalid section remains persistable and will be canonicalized on the next save. Code intentionally sets `persistable = false` and clears the revision for **any** typed-section failure so no save can overwrite an unrepresentable future/unknown value. The code is safer; the docs are wrong. | Update the configuration reference to state that valid sibling sections apply in memory, but persistence is disabled for the run whenever any typed section fails. Do not weaken the code. |
| DOC-002 | Low | D8 / Documentation | `docs/development.md:151-160`; `src/overlay/mod.rs:214-231` | Reasoned but not executed | Maintainer uses development docs to reason about memory bounds | Development docs still call the overlay track cache “cap-3”; this branch uses `TRACK_CACHE_CAP = 8`, matching architecture commentary. | Change the one stale bound to 8 and state that retention is indefinite but LRU cap-bounded. |
| DOC-003 | Low | D8/D9 / Documentation | `README.md:156-164`; `.github/workflows/ci.yml:1-11` | Reasoned but not executed | Maintainer assumes every branch push receives CI | README says CI runs on every push and PR. Workflow push triggers only `main` (plus tags); PRs and manual dispatch are covered. | Make README wording match the workflow, or deliberately broaden the workflow in `CI-001`. |

---

# Section 2 — Behavioral / Architectural Refactors

> **Behavior-change warning:** `START-001`, `OVERLAY-001`, `DEDUP-001`, `MON-001`, `HOVER-001`, and the effective-color part of `A11Y-002` can visibly change what a user sees. Those changes are called out explicitly below.

| ID | Severity | Pass/Area | Location | Basis | Scenario trigger | Description | Suggested direction |
|---|---|---|---|---|---|---|---|
| DATA-001 | **Critical** | A3 / D9 / Rule 1 | `create_exe.ps1:85-91` | **Reasoned but not executed** | Run `create_exe.ps1 -FreshInstall` on a real profile | The script recursively deletes `%APPDATA%\WinGlance\WinGlance\data`. This directly violates the sacred rule that user data under that directory is never deleted, and it can destroy `config.toml`, logs, and any future database/cache. “Explicit fresh-install simulation” is not a safe exception. | Remove the live-data deletion capability. If a fresh-install simulation is needed, require an explicitly supplied temporary/sandbox root and refuse any path resolving to the production data root. |
| START-001 | **High** | Functionality / Rule 2 / D8 | `src/config.rs:618-641`; `src/main_window.rs:1384-1410` | **Reasoned but not executed** | Fresh install, or any launch where the config did not exist before startup | `Config::load_from_path` sets `first_run = true`, and main-window creation treats `first_run` as an unconditional reason to `ShowWindow(..., SW_SHOWMAXIMIZED)`. This contradicts the hard no-popup launch contract. README/config docs deliberately document the exception, but documented intent is not a defense against the mandate. | Remove the forced first-run show. Keep first-run discoverability in tray/pill affordances only. The separate question of an explicit `start_in_tray = false` opt-in is retained in the Risk Register because the hard mandate conflicts with that public setting. **Noticeable behavior change:** first launch becomes silent. |
| OVERLAY-001 | **High** | Functionality / UX truth / Rule 5 | `src/overlay/mod.rs:185-198`; `src/overlay/mod.rs:1285-1345`; `src/overlay/mod.rs:2280-2445` | **Reasoned but not executed** | Cold start with no SMTC session; last playing source disappears with no playing successor | Overlay state initializes with `content = None` and `Phase::Hidden`; retirement/no-successor paths call `hide()`. There is no truthful idle pill state, so the pill is not always visible while the process is alive as required by the mandate/scenario 1. | Add an explicit static Idle presentation/state and make “no media” settle into Idle instead of Hidden. Preserve the passive/no-focus window contract. **Noticeable behavior change:** an idle pill remains on screen with no media. Fullscreen suppression scope must be resolved per `R-03`. |
| DEDUP-001 | **High** | A1 / Functionality / UX truth | `src/events.rs:375-414`; callers `src/main_window.rs:2430-2535`, `src/overlay/mod.rs` receive/update paths | **Reasoned but not executed** | Same source emits a genuine new item with identical title+artist while the new/old snapshot has artwork on only one side (common during progressive thumbnail updates) | `TrackInfo::same_media` treats `(None, Some(_))` and `(Some(_), None)` as the same media without consulting duration or any other discriminator. That correctly avoids a duplicate pill for a late thumbnail, but it can also classify a genuine same-title replay/version as a metadata refresh and rewrite/update in place instead of notifying. This is exactly the “anti-spam rule can swallow a genuine user action” class called out by the audit mandate. | Separate **refresh provenance** from **media identity**. Prefer a worker-emitted refresh/new-identity disposition or an identity discriminator that uses duration/playback type/track number/timeline reset when available. Add paired tests: late thumbnail must merge; genuine same-title transition must notify. **Noticeable behavior change:** previously swallowed edge transitions can produce a pill/history row. |
| A11Y-001 | Medium | Accessibility / Settings correctness | `src/main_window.rs:4100-4255` | **Reasoned but not executed** | Scroll Settings to a nonzero offset, then keyboard/UIA-focus a control outside the viewport | `settings_focus_targets` already builds rectangles after applying `settings_scroll_y`, so each target `cy` is a client coordinate. `focus_settings_target` then compares it against `settings_scroll_y + viewport...` and assigns `settings_scroll_y = t.cy - client_h/2`, mixing document and client coordinates. On nonzero scroll this can jump in the wrong direction or fail to bring focus into view. | Compute visibility purely in viewport/client coordinates and adjust the current scroll by the required delta before clamping. Add a pure regression covering a nonzero starting scroll and both upward/downward focus moves. |
| A11Y-002 | Medium | Accessibility / WCAG contrast | `src/config.rs:330-390`; `src/main_window.rs:2600-2765` | **Reasoned but not executed** | Hand-edit `appearance.text_color` near the tracking window's dark background | The Activity title uses the configurable `text_color` directly. The config accepts arbitrary RGBA bytes; there is no effective contrast correction for the tracking-window title, so a black/near-black value can make primary content unreadable (down to ~1:1 on a black surface). The overlay already has shared contrast helpers, demonstrating the repo has a suitable mechanism. | Preserve the serialized user color, but derive an **effective rendered color** that meets at least WCAG AA 4.5:1 against the actual Activity background; warn once when correction is applied. **Noticeable only for low-contrast custom themes.** |
| A11Y-003 | Medium | Accessibility / Process picker | `src/process_picker.rs:620-915` | **Reasoned but not executed** | Open Allowed apps / Auto-compact / Preferred source picker with Narrator or another UIA client | The owner-drawn native listbox stores the app's checked state only in `LB_SETITEMDATA` and paints/toggles it itself. Keyboard Space works, but there is no UIA provider exposing that custom checked/toggle state, so assistive technology can see/select list items without learning the state that will actually be persisted. | Add a small UIA fragment/Toggle provider for picker rows, reusing the Settings provider pattern and the same row/toggle source of truth. Keep mouse/keyboard semantics unchanged. |
| HIST-001 | Medium | UX truth / Diagnostics | `src/main_window.rs:1013-1048`; `src/main_window.rs:2360-2575`; `src/main_window.rs:5070-5145` | **Reasoned but not executed** | Notifications disabled, redundant playback re-report, worker-failure row, or filtered/churned session appears in history | `HistoryEntry.accepted` is documented in one place as “passed `media_sources`”, but callers also use it for “reached the pill”. `entry_detail` renders **every** `accepted == false` row as `(filtered by allowed apps)`. This lies for allowed events muted because notifications were off/redundant, and for internal failure rows. | Split `source_allowed` from a small `HistoryDisposition`/reason (`Shown`, `Redundant`, `NotificationsOff`, `Filtered`, `ChurnExcluded`, `InternalFailure`). Highlight from “shown”; tooltip from the actual reason. |
| TOOLTIP-001 | Medium | UI/UX / Multi-monitor | `src/main_window.rs:1935-2010` | **Reasoned but not executed** | Tracking window/cursor is on a monitor left of or above the primary display (negative virtual-screen X/Y) | Tooltip placement correctly clamps to the nearest monitor work area, then destroys that result by packing `clamped.left.max(0)` and `clamped.top.max(0)` into `TTM_TRACKPOSITION`. Negative virtual-screen coordinates are valid; clamping them to zero can jump the tooltip toward the primary origin. | Pack the signed virtual-screen coordinates using Win32 `LPARAM`/16-bit two's-complement semantics (or an equivalent helper) after monitor-work-area clamping. Add negative-X and negative-Y unit tests for the packer. |
| MON-001 | Medium | D1/D8 / Multi-monitor contract | `src/overlay/fullscreen.rs:211-307`; `docs/configuration.md:86-108` | **Reasoned but not executed** | Configure `monitor = "index-N"`, reorder displays while running, then restart | Documentation says `index-N` resolves against the **current enumeration order every placement**. Runtime adds `resolve_target_sticky`, remembering the first device name for that index and following the same physical display for the process lifetime. That is arguably better during docking, but after restart the unchanged config returns to enumeration semantics and can target a different display. One config therefore has two meanings separated by a restart boundary. | Choose one contract. Preferred direction: persist an **additive stable monitor identity** (device name/identifier) alongside the legacy index, with index/primary fallback; update UI/docs. Do not simply delete stickiness without deciding the intended UX. **Noticeable behavior/config addition.** |
| HOVER-001 | Medium | UX / Behavioral consistency | `src/overlay/mod.rs:67-83`, `src/overlay/mod.rs:640-675`; `docs/configuration.md:120-143` | **Reasoned but not executed** | User moves the pointer onto an Expanded pill to read it, then leaves before 500 ms; or repeatedly crosses its edge | Expanded hover is deliberately a one-way dismissal arm: remaining time is capped at 500 ms and leaving does not cancel it. Compact first-hover does the opposite (expand and hold while cursor remains), then later hover dismisses. The behavior is internally consistent and documented, but it is surprising as a readability interaction and can punish accidental edge crossing. | Keep `dismiss_on_hover` as a setting, but make the arm reversible on leave (or default it off for Expanded) unless the maintainer explicitly prefers the current “hover means I've seen it” model. **Noticeable behavior change; maintainer decision required.** |
| SINGLE-001 | Medium | A5 / Availability / Single-instance | `src/main.rs` singleton acquisition; `docs/architecture.md:84-122` | **Reasoned but not executed** | Another same-session process pre-creates/holds `WinGlanceSingleInstance`, then WinGlance launches | The current design explicitly treats name squatting as a diagnosed availability issue and fails closed so two WinGlance instances cannot race config/log writes. That means a hostile same-user process can hold the known mutex and deny startup indefinitely, contrary to A5's desired hostile-second-instance behavior. Under the stated same-user attacker model, no predictable same-user mutex/file lock can fully prevent DoS; the attacker could also terminate the process. | Keep fail-closed data integrity as the default. If reducing accidental/synthetic squatting is important, add a verified live-instance handshake and an alternate coordination strategy only after proving it cannot create dual writers. Treat complete same-user DoS resistance as out of scope/impossible without changing the threat assumptions. |
| CI-001 | Medium | D6/D7/D9 / Release engineering | `.github/workflows/ci.yml:1-45`; audited head status | **Unable to verify** | Treat `dev-audit-fixes` / `checkpoint` as release candidates | CI gates fmt, clippy, tests, release build, audit and deny on PR/main/manual runs, but the audited branch head has no run/status and the workflow does not gate the requested cyclomatic/cognitive/Halstead/CRAP thresholds or mutation score. Therefore the release-quality targets are not established. | Add pinned, reproducible Rust-oriented complexity/coverage/mutation gates and ensure release-candidate commits are checked (PR/manual or explicit branch policy). Do not refactor solely to satisfy an unmeasured metric; measure first, then simplify only hotspots over threshold. |

---

# Depth pass A — Security & threat model

## Threat model

WinGlance is a **single-user, offline desktop app running with the user's own privileges**. There is no network service, account system, credential store, key material, or telemetry path in the audited code/dependency surface. The practical hostile input is another process in the same interactive session. Any media app can register an SMTC session and therefore controls strings, timeline values, thumbnail bytes, and session identity presented to WinGlance. A same-user process/user can also mutate `%APPDATA%` and attempt reparse-point / replacement races. The correct priorities are therefore memory/crash safety on hostile media/config, user-data integrity, bounded resources, reparse-resistant writes, and graceful behavior under hostile session churn.

### A1 — SMTC metadata is untrusted

**Clean for memory/safety; one behavioral finding (`DEDUP-001`).** `smtc.rs` centralizes metadata sanitization: displayed/logged strings are capped to 256 characters; C0, DEL/C1, bidi override/isolate controls and Unicode line/paragraph separators are removed; logging uses bounded escaped previews. Combining marks and emoji/ZWJ sequences can remain (correctly) but are bounded; drawing is single-line/clipped/marquee-based and no markup/format string parser consumes them. WinRT/Rust string conversion prevents raw invalid UTF-16 from becoming an unchecked Rust `String`.

Timeline data is normalized before the UI: non-finite/negative positions/rates are rejected/clamped, progress rendering guards missing/zero duration, and displayed position is bounded against duration. Rapid changes are coalesced/bounded rather than stored without limit.

Thumbnail safety is strong: stream bytes are capped at 4 MiB **before** bulk allocation/read completion, decoded dimensions are capped before decode, UI receives a fixed 256×256 premultiplied buffer, corrupt data degrades to no art, decoding happens off the UI thread, and a per-source generation rejects stale late artwork. The remaining media-identity problem is semantic rather than memory unsafe: see `DEDUP-001`.

### A2 — `config.toml` is untrusted

**Clean for data integrity; docs drift `DOC-001`.** Config input is capped at 1 MiB, so the requested 10 MiB torture case is rejected before parse/buffering where possible. Syntax/unreadable/oversized failures use defaults in memory, log a warning, disable persistence, and leave the file untouched. Typed-invalid sections retain valid siblings in memory but also disable persistence, intentionally fail-closed. Unknown top-level and per-section keys are captured with `#[serde(flatten)]` and round-trip on successful saves.

Out-of-range values are normalized with warning logs. External edits are detected by an exact byte revision plus file identity; a mismatch returns Conflict and does not save. UTF-8 BOM acceptance is parser-version behavior not proven here; if rejected, the failure path remains safe/non-destructive. CRLF/LF are ordinary TOML whitespace.

### A3 — Filesystem / TOCTOU / reparse points

**Production app paths clean; Critical tooling violation `DATA-001`.** Config/log/crash writes use `winutil` verified-open/atomic-replace helpers that pin and validate the parent, reject final-component reparse points, verify final handle identity/path, create randomized `CREATE_NEW` temporary files, re-check target identity/bytes at commit, rename atomically, and flush the parent. That is the right same-user TOCTOU posture.

`create_exe.ps1 -FreshInstall`, however, directly removes the live data tree and violates the user-data mandate even without an attacker.

### A4 — Spawn / exec surfaces

**Clean trace.** User-invoked “open config/logs” resolves the app's own known path and uses Shell execution as a file open, not a string-built shell command. Restart spawns the current executable with explicit internal arguments/nonce. No SMTC/user string is interpolated into an executable command line. Failures are logged/surfaced in-app rather than startup message boxes.

### A5 — Single instance

**Finding `SINGLE-001`; otherwise structurally strong.** The mutex is session-scoped (no `Global\`), live duplicates exit without a popup, abandoned mutex ownership is accepted, and restart uses a nonce-named ready event plus bounded waits. The deliberate fail-closed response to mutex squatting preserves data integrity but allows same-user denial of startup.

### A6 — Information disclosure / crash logging

**Clean trace.** Logs are local-only. Hostile metadata is bounded/escaped before log formatting; no raw thumbnail bytes/tokens/file buffers were observed in logs. `log-Live.log` is capped at 1 MiB and plain-launch truncation is the documented/sacred exception. `crash.log` uses a verified retained append handle and a process-wide 8 MiB cap. The vectored access-violation path builds its record in stack buffers; panic reporting is bounded, so a hostile 100 KiB title cannot directly become a 100 KiB crash record.

### A7 — Data integrity during saves

**Clean trace.** Successful saves are atomic replacements and update the revision only after replacement. External edit/replacement/growth/deletion causes a conflict, not clobber. Unknown fields are preserved on representable configs. Logs are separate and untouched by config save. Invalid typed sections intentionally disable persistence rather than canonicalizing over unknown future values.

### A8 — `unsafe` boundary audit

**No memory-unsafe finding identified.** Unsafe use is concentrated in Win32/COM boundaries: WNDPROCs/subclass/enumeration/timer callbacks; `GWLP_USERDATA`; GDI object calls; raw file/kernel handles; COM/UIA vtables. Important invariants are enforced as follows:

- `StateClaim` distinguishes whether `WM_NCCREATE` claimed a boxed state when creation fails.
- `release_window_state` clears `GWLP_USERDATA` before reconstructing/dropping the `Box`, preventing re-entrant second ownership.
- OS callbacks are wrapped by `guarded_wndproc`, `guarded_subclass`, `guarded_enum`, `guarded_void`, or UIA `catch_uia`, so Rust panics do not unwind across foreign frames.
- Timer-queue callback posts to the UI thread instead of calling window state synchronously; teardown waits/deletes the timer.
- WinRT SMTC objects stay on the worker apartment; shell icon COM objects stay on the icon worker apartment.
- `unsafe impl Send` on the display-cache holder only transports opaque `HMONITOR` values behind a mutex; it does not move a dereferenceable Rust/COM object across threads.
- Raw pointer dereferences observed are message/callback state pointers guarded by the HWND/state lifecycle, not general application data structures.

No unchecked external-input indexing or raw-memory write reachable from media/config was found.

---

# Depth pass B — Memory, GDI/USER handle & leak audit

Runtime GDI/USER counts cannot be observed without launching GUI code, so **every inventory row below is Reasoned but not executed**. I am not claiming handle counts.

| Object / class | Creation site / lifetime bucket | Owner / bound | Pairing / teardown | Status / basis |
|---|---|---|---|---|
| Overlay top-level HWND + `OverlayState` | `overlay::create_window`; per-window | UI thread; one | `DestroyWindow`; `WM_NCDESTROY` clears/releases boxed state | Paired — Reasoned but not executed |
| Main top-level HWND + `MainWindowState` | `main_window::create_window`; per-window | UI thread; one | quit/close path → destroy; `WM_NCDESTROY` releases state | Paired — Reasoned but not executed |
| Main history listbox / tooltip child HWNDs | main-window child creation; per-window | Parent-owned; fixed count | destroyed with parent; active timers killed | Paired — Reasoned but not executed |
| Process-picker HWND + listbox | `process_picker::open`; per user-open | global single-open slot; one popup | destroy on confirm/cancel/error; subclass removed at teardown; state released | Paired — Reasoned but not executed |
| Positioner sample HWND | `positioner`; per adjustment | one sample | result/cancel destroys; state release | Paired — Reasoned but not executed |
| Duration dialog + child controls | `duration_dialog::show_duration_dialog`; per invocation | modal single dialog | close/destroy before owner teardown; child windows parent-owned | Paired — Reasoned but not executed |
| Overlay `FontProvider` HFONTs | `gdi.rs`; per overlay/DPI/font key | DPI-scoped cache | provider swap/drop deletes old fonts | Paired/bounded — Reasoned but not executed |
| Main fonts/owned brushes | `main_window.rs`; per main window/DPI | fixed fields/caches | owned wrappers `Drop` / DPI replacement | Paired/bounded — Reasoned but not executed |
| Main `ArtBlit` memory DC + HBITMAP | `build_art_blit`; per current artwork/icon | at most current art + icon | `Drop`: restore selected object, `DeleteObject`, `DeleteDC` | Paired — Reasoned but not executed |
| Overlay reusable DIB DC + HBITMAP | render cache; per overlay | one current backing surface, resized/replaced | restores old bitmap; drop deletes bitmap/DC | Paired — Reasoned but not executed |
| Overlay text scratch / marquee strips / chrome raster | render cache; per window/row/content | 4 marquee rows; one chrome cache | replaced/dropped with state/content/DPI | Bounded — Reasoned but not executed |
| Picker fonts/brushes | picker open / DPI change | fixed small set per popup | old fonts deleted on DPI rebuild; fonts/brushes deleted at `WM_NCDESTROY` | Paired but manually owned — Reasoned but not executed |
| Positioner pens/brushes | sample window | fixed small set | explicit teardown/error cleanup | Paired but manually owned — Reasoned but not executed |
| Tray/menu HMENU objects | tray menu open | one menu tree per invocation | root menu destroyed after `TrackPopupMenu`; child submenus owned by root | Paired — Reasoned but not executed |
| Tray notification icon registration | `install_tray_icon` | one `(hwnd,uID)` | `NIM_DELETE` on destroy/session end; TaskbarCreated re-add | Bounded — Reasoned but not executed |
| Win32 window timers | overlay/main named IDs | finite fixed IDs; `SetTimer` replaces by id | `KillTimer` on state transition/destroy | Bounded — Reasoned but not executed |
| High-resolution animation timer | overlay timer queue | one | `DeleteTimerQueueTimer` / fallback kill on teardown | Paired — Reasoned but not executed |
| Foreground WinEvent hook | overlay creation | one | unhooked on `WM_NCDESTROY` | Paired — Reasoned but not executed |
| Toolhelp snapshot HANDLE | `process_picker::process_names` | per enumeration | `SnapshotGuard::Drop` → `CloseHandle` | Paired — Reasoned but not executed |
| Process-query HANDLE | `exe_name_for_pid` | per lookup | `ProcessQueryGuard::Drop` | Paired — Reasoned but not executed |
| Singleton mutex / restart event handles | `main.rs` | fixed per process/handoff | RAII/explicit close/OS process cleanup | Bounded — Reasoned but not executed |
| SMTC subscriptions / WinRT refs | `smtc.rs` worker | session/source admission caps | worker-owned apartment; unsubscribe/drop during resync/teardown | Bounded — Reasoned but not executed |
| Hung SMTC worker threads | supervisor restart path | process-lifetime `MAX_LEAKED_WORKERS` budget | intentionally not forcibly joined if COM is wedged | Bounded degradation, not leak-to-infinity — Reasoned but not executed |
| Icon worker HBITMAP/DC | `icon.rs` per job | single worker, queue cap 16 | HBITMAP delete; `DcGuard`; COM refs worker-local | Paired/bounded — Reasoned but not executed |
| Worker event channel | `main.rs`/SMTC | cap 1024 | full → retry/coalesce, not growth | Bounded |
| Worker retry mailbox | SMTC | cap 256 | newest authoritative value supersedes/coalesces | Bounded |
| Main / overlay event queues | forwarder | cap 256 **each** | newest wins; failed wake clears affected queue | Bounded |
| Overlay pending notification queue | overlay | cap 4 | oldest unshown dropped at cap; current pill never pulled | Bounded |
| Overlay track cache | overlay | cap 8 | LRU eviction | Bounded |
| Overlay source-state ledger | overlay | cap 64 | evicts stopped entries first | Bounded |
| Main history | `main_window::History` | cap 400 | oldest evicted | Bounded |
| Main per-source state | main window | cap-bounded (64 in current design) | state eviction policy | Bounded |
| Artwork payloads across transport | `TrackInfo` lifetime token | 64 MiB in-flight budget | final `Arc<ArtworkLifetime>` drop releases reservation | Bounded |
| `log-Live.log` | logger | 1 MiB total per plain run / preserved restart chain | stops accepting complete lines at cap | Bounded |
| `crash.log` | main crash logger | 8 MiB total | append stops at cap | Bounded |

**B2 pairing conclusion:** no currently unpaired GDI/USER site was found. The main-window and overlay hot objects already follow RAII. Picker/positioner have some manual fixed-object cleanup, but the paths are paired; converting them solely for stylistic purity is not a release blocker.

**B3 boundedness conclusion:** the requested unbounded structures are already bounded. No High/Critical unbounded cache/channel/history/log growth finding remains.

**B4 shutdown conclusion:** static ordering is coherent: auxiliary modal UI is closed, tray icon removed, timers/hooks detached, windows destroyed, worker/control paths stopped/joined where join is safe, and state boxes are released at `WM_NCDESTROY`. A wedged COM worker is intentionally abandoned only within a process-lifetime budget.

---

# Depth pass C — Performance & hot paths

## C1 — Hot-path trace

### Overlay tick/morph

- Static shown state drops to a 250 ms tick.
- Aura-only steady playback uses a ~66 ms cadence (~15 Hz).
- Animation uses monitor/config-limited cadence (default cap 60 Hz).
- `should_render_this_tick` skips raster/upload when no visual stage is dirty.
- Progress only marks a frame dirty once the painted bar moves meaningfully; z-order reassert is throttled.
- Springs/morphs are time-based; delayed ticks do not accumulate fixed-step drift.

### Per-frame rendering

The branch already avoids the major anti-patterns the prompt asks to find:

- reusable DIB/backing surfaces rather than `CreateDIBSection` per frame;
- reusable `frame_scratch`/UTF-16 text scratch;
- DPI-scoped font cache;
- pre-resolved pill text;
- cached marquee rasters and static chrome;
- progress-bar repaint separate from static chrome;
- no UI-thread image decode;
- one `UpdateLayeredWindow` only on dirty frames.

### Artwork / palette

SMTC reads/decode run on the worker. Input is capped; decode output is fixed 256² BGRA (~256 KiB). UI conversion/palette work is cached per content/art identity. App icon extraction is moved further onto its own bounded worker so shell extension stalls do not pin SMTC.

### Event forwarding

Transport uses `Arc<MediaEvent>` so two window queues share the event/art allocations; window drain owns/clones only as required. Every queue/mailbox has a cap, and same-key events are superseded/coalesced rather than allowed to backlog indefinitely.

## C2 — Order-of-magnitude frame budget (estimates, **not measurements**)

Using the shipped `max_width = 340` logical px and an expanded frame on the order of ~150–180 logical px high, a premultiplied 32-bit surface is roughly:

- 100% DPI: ~0.2–0.3 MiB/frame;
- 150% DPI: ~0.45–0.65 MiB/frame;
- 200% DPI: ~0.8–1.1 MiB/frame.

Because a dirty layered-window update copies the whole surface, a 60 Hz animation is therefore on the order of **10–70 MiB/s** of surface traffic; the ~15 Hz comet steady state is roughly **3–17 MiB/s**. These are dimensional estimates, not profiler data.

Warm-frame heap allocations are approximately **O(0)** on the common render path: buffers, fonts, static chrome, text widths and marquee strips are reused. Content changes, DPI changes and cache misses allocate; steady frames generally do not. API-call count is order **tens** of drawing/composition operations plus one layered-window upload on a dirty full frame, substantially fewer on a cache-hit foreground-only pass.

A large but valid compressed JPEG decode can plausibly cost **~10–100 ms** on commodity CPUs (estimate), but it occurs on the worker and produces a fixed ~256 KiB output; it should not stall the UI message loop. The price is delayed art, not frozen rendering.

## C3 — Improvement directions

No speculative render rewrite is justified before profiling. The biggest previously-obvious wins (cached text, reusable DIBs, no-change frame skip, reduced aura cadence, worker decode, bounded queues) are already implemented. The proposed program therefore focuses on correctness and measurement:

- Add CI/perf instrumentation or a developer-only benchmark for pure render stages before changing them (**P2**, no product behavior).
- When the mandated Idle pill is added, keep it **fully static** unless content/layout/foreground actually changes; target zero continuous raster uploads while idle. Estimate: avoids whatever 15–60 Hz layered-window traffic a naive idle implementation would introduce (~3–70 MiB/s from the dimensional range above).
- Do not add a per-DPI artwork-variant cache unless profiling proves scaling hot: the current fixed decode + draw scaling has no variant-growth bug. An LRU that costs ~1–10 MiB would be justified only if it removes measured repeated scaling work.

---

# Depth pass D — Architecture, structure, dependencies, docs drift

### D1 — Boundaries / invariants

- **Passive pill:** holds. Overlay is `WS_EX_TRANSPARENT`/no-activate and has no click/keyboard action surface; hover is observational only.
- **UI-thread Win32 ownership:** holds. SMTC emits events/control status; worker threads use `PostMessage`, not synchronous cross-thread `SendMessage` into UI ownership.
- **Config ownership:** holds. Main window owns writes; overlay receives pushed values. `positioner.rs` posts results and does not reload config from disk.
- **Event ordering:** per producer/session ordering is preserved through the worker and bounded channel. Fanout puts the same logical event into both per-window queues. At overload, explicit newest-wins/drop/coalesce semantics can discard intermediate reports but retain authoritative state; no unbounded FIFO is hidden behind the forwarder.

### D2 — Dead/redundant code and drift

No obvious orphan module or “feature parsed but never read” was found in the audited paths. The latest commit explicitly decomposes WNDPROC/control-flow hotspots. However, **Dead code = 0** and **Redundant code = 0** cannot be certified without current clippy/static-analysis output; see Quality Gates. Stale documentation findings are `DOC-001`–`DOC-003` and `MON-001`.

### D3 — Error handling

External-input/file errors generally log and degrade instead of `unwrap`ing. `expect` uses observed in production code are for operations such as formatting into a `String` that cannot fail, not hostile-input bounds. Lock poisoning is commonly recovered with `into_inner`; callback errors are contained. Initialization has degraded modes (missing log, missing tray retry, SMTC worker supervisor/failure note, icon worker no-icon fallback).

### D4 — Panic/unwind safety across FFI

Strong. WNDPROCs, subclass procedures, enum callbacks, timer callbacks and UIA COM methods have catch boundaries. A panic is converted to a safe default/`E_FAIL`/DefWindowProc path and logged instead of unwinding through an `extern "system"` boundary.

### D5 — Concurrency

Atomics used for cross-thread lifecycle/budgets generally use Acquire/Release/AcqRel or SeqCst where a control/lifetime transition matters; Relaxed accounting is used where the value is only a bounded counter token. No thread-affine COM object was found stored in a cross-thread shared cell. The display cache's `unsafe impl Send` contains only opaque monitor handles plus owned strings/rects behind a mutex.

### D6 — Testability

The branch already extracts substantial pure logic: config normalize/save conflict, queue bounds, event reduction, morph math, display selection, history helpers, state ownership and reparse-safe file operations. Missing seams that are worth adding are exactly those exposed by this audit: nonzero-scroll focus math, media refresh provenance, signed tooltip coordinate packing, monitor identity persistence semantics, history disposition, and picker UIA toggle state.

### D7 — Dependencies

Every direct dependency in `Cargo.toml` has a visible role: `windows`/`windows-core`/`windows-future`, `anyhow`, `chrono`, `dirs`, `image` (JPEG/PNG only), `log`, `serde`, `toml`, and build-only `embed-manifest`. No unused direct crate is identified.

**Unable to verify:** `cargo tree -d`, transitive duplicate versions, current advisories, and license resolution at the audited head because no branch-head CI/artifact is available and no local cargo command was run. `deny.toml` is advisory/license/source policy by intent; no allow-listed advisory is re-flagged here.

No new runtime dependency proposed below is necessary. In particular, the fixes can use the existing standard library/Win32/windows-rs stack.

### D8 — Docs/config reconciliation

- `config.example.toml` and the inspected defaults are materially aligned; no ignored example key was found.
- Typed-invalid-section persistence is documented incorrectly (`DOC-001`).
- Development cache bound is stale (`DOC-002`).
- README CI trigger wording is stale (`DOC-003`).
- First-run popup docs accurately describe **current** behavior but conflict with the hard product mandate (`START-001`), so docs must change with the fix.
- `index-N` docs do not describe sticky-in-process physical-monitor behavior (`MON-001`).
- AGENTS log-truncation exception matches code. Churn/dedup logging contracts have corresponding worker logic; live runtime lines remain Reasoned but not executed.

### D9 — Repo hygiene / CI

`.gitignore` covers build/data/log/package outputs; both MIT and Apache license files exist; no tracked build output was identified in the tree. CI uses a read-only token for untrusted check jobs and isolates release write permission. `CI-001` remains because this exact branch head is unchecked and the requested production metrics/mutation score are not gates.

---

# Depth pass E — Scenario walkthroughs

All GUI/SMTC scenarios are **Reasoned but not executed**. Reproduction is for the maintainer on a live Windows desktop; no executable was launched during this audit.

| # | Scenario | End-to-end trace / modules | Result | Live reproduction and expected evidence |
|---:|---|---|---|---|
| 1 | Cold start, no media | `main` → config → SMTC supervisor/worker → overlay state → main window/tray | **Findings filed: START-001, OVERLAY-001.** Startup can show the first-run tracking window and overlay starts Hidden rather than idle. | Delete **only in a disposable test profile/sandbox**, not real user data; launch with no media; then start/stop a compliant player ×3/2 s. Expect one startup/config line, track/state lines when media appears, source-retirement lines when it disappears. The target behavior after fixes is a silent launch + idle pill throughout. |
| 2 | Churn storm | SMTC callbacks → debounce dirty set → resync/admission → churn exclusion → event channel | **Clean — evidence:** per-source churn accounting/exclusion occurs before event emission; compliant source identities remain independent. | Use a session-recreating source (~20/8.5 s) plus a normal player changing tracks. AGENTS-required log substrings: one `SessionsChanged/CurrentSessionChanged (debounced)` per burst, `(coalesced)` lines, one `WARN ... churning sessions ... excluding it`, and **no** `track changed`/`playback state changed` naming the excluded source during cooldown. Normal player's real track change must still appear. |
| 3 | Hostile metadata live trace | SMTC read → `cap_meta`/timeline sanitize → bounded thumbnail read/decode → generation token → both window queues/render/history | **Finding filed: DEDUP-001; safety trace otherwise clean.** | Feed 100 KiB/control/NUL/bidi/emoji/ZWJ/RTL strings, zero/corrupt/20000×20000 art, then two fast same-source transitions. Expect bounded escaped log previews, oversized dimension/stream rejection or placeholder, no crash. A late older art generation must be dropped. Same-title/artist with one-side-missing art is the semantic regression case to verify. |
| 4 | Playback-control storm | playback callback → per-session debounce/read → bounded channel → overlay reducer/progress → main history | **Clean — evidence:** bounded/coalesced transport and finite timeline normalization; no divide-by-zero path found. | Generate 50 play/pause/seek changes/2 s, including duration 0, negative position and position > duration. Expect latest authoritative state to settle, progress bar absent/frozen/clamped where invalid, and no unbounded queue/log growth. |
| 5 | Hover storm | cursor sample → hover state (`hover_expand`, leave debounce, one-way dismiss arm) → animation timer → render | **Finding filed: HOVER-001.** No timer accumulation leak found. | Mouse in/out ×10/2 s, park on edge. Expanded hover currently arms ≤500 ms dismissal even if cursor leaves; compact first hover holds/expands. Expect no duplicate timer ids/oscillation after spring settles. Decide whether one-way Expanded behavior remains desired. |
| 6 | DPI changes | `WM_DPICHANGED` → font/provider rebuild → DIB/cache invalidation; main/picker font rebuild; layout | **Clean — evidence:** old fonts/DIBs are replaced/freed and caches are invalidated; no per-DPI artwork-variant map exists to grow. | Move 100%→150%, change DPI with main/history open. Inspect text clipping and handle stability live. Expected debug evidence is DPI/layout/reposition activity, with no repeated art decode for unchanged content. |
| 7 | Multi-monitor + fullscreen | display enumeration/cache → sticky target resolve → placement/fullscreen verdict → overlay hide/resume; main tooltip independent | **Findings filed: MON-001, TOOLTIP-001; R-03.** Overlay work-area clamping itself is sound. | Place secondary left of primary (negative X), configure `index-N`, toggle fullscreen ×5, reorder monitors, restart. Verify pill target before/after restart and history tooltip near cursor. Current tooltip can jump to x/y=0; index can mean physical device during run and enumeration slot after restart. |
| 8 | Config torture battery | `Config::load_from_path` → staged parse/normalize → revision → save conflict/atomic replace | **Finding filed: DOC-001; implementation otherwise clean.** | (a) corrupt file → defaults + warning + no write; (b) unknown key → preserved on representable save; (c) typed bad field → section default **and persistence disabled**; (d) 10 MiB → rejected by 1 MiB cap; (e) hand-edit after load → Conflict/no clobber; (f) compare example/defaults. Log must explicitly report invalid/oversized/conflict, never silent default. |
| 9 | Rapid restarts | singleton mutex → supervisor/UI → tray; crash handler/panic hook → bounded crash log | **Clean — evidence:** abandoned mutex takeover, restart handshake, tray teardown and bounded crash writers exist. Runtime handoff timing remains unexecuted. | Launch/kill ×5 in disposable live test. Each relaunch should acquire or `WAIT_ABANDONED` cleanly, no popup/ghost tray. Induce only a safe test panic seam if one exists; `crash.log` must remain writable/bounded. Do not corrupt real logs. |
| 10 | Display topology changes | `WM_DISPLAYCHANGE` → display-cache invalidate → enumerate → target fallback/reposition; main layout resize | **Finding filed: MON-001 (and TOOLTIP-001 if negative topology).** | Sleep/wake monitor, unplug target, change resolution with main open, reconnect/reorder. Expect fallback-primary warning for missing index and reposition rather than orphaning off-screen; compare target after restart for sticky-contract inconsistency. |
| 11 | Long-run log growth | `FileLogger` cap accounting; retained crash append handle/counter | **Clean — evidence:** live 1 MiB cap and crash 8 MiB cap are hard bounds. | Estimated raw generation before cap: ~150–300 B/line × 1–10 significant lines/s ≈ ~13–260 MB/day **if uncapped**. Actual writer stops at **1 MiB per plain run/preserved restart chain**; crash log stops at **8 MiB total**. Verify files stop growing at those bounds. |
| 12 | Tray lifecycle + Explorer restart | main WNDPROC → registered `TaskbarCreated` → `NIM_ADD` retry; menu build/track/destroy; autostart helper | **Clean — evidence:** Explorer broadcast re-add and retry budget are implemented; autostart only touches its own Run value. | Restart Explorer, open/close menu rapidly, quit while menu open, toggle autostart. Expect `Explorer restarted the notification area; re-adding the tray icon` / retry success or bounded failure logging; no duplicated tray icons. |
| 13 | Shutdown ordering | tray quit/session-end/main destroy → aux-dialog close → overlay destroy → timer/hook/state cleanup → worker/forwarder shutdown | **Clean — evidence:** destruction/state ownership is explicit and bounded; hard kill relies on kernel/process cleanup. | Exit with media, Settings, maximized window, and art activity; then `WM_QUERYENDSESSION`/`WM_ENDSESSION` in a test session; hard-kill and relaunch. In-app restart should append a restart-boundary line; ordinary launch truncates `log-Live.log` as intended. |
| 14 | History window long-run | event → `push_history` → `VecDeque` cap400 → listbox insert/delete; precomputed cells; art stripped from history | **Finding filed: HIST-001 for reason truth; memory/perf clean.** | Generate >400 history rows across many sessions. Mirror/listbox should stay near cap 400, oldest rows evict, scrolling should not allocate/reformat each cell, and muted-row tooltips must be checked for the incorrect “filtered by allowed apps” reason. |

---

# Depth pass F — Perfect-state enhancement program

## Hardening exemplars already satisfied — **do not re-implement**

The following prompt exemplars are already present in substance and should **not** generate gratuitous commits:

- **Bounded/drop/coalesce event transport:** worker channel 1024, retry mailbox 256, each window queue 256, overlay pending 4.
- **Artwork off UI + generation token:** worker decode to fixed 256² buffer; late generations rejected; 64 MiB in-flight budget.
- **Atomic config save:** revision/file-identity checked verified temp+rename+parent flush; external edits refuse save.
- **Typed `GWLP_USERDATA` ownership:** `StateClaim` + clear-before-drop release discipline across windows/dialogs.
- **GDI RAII on hot ownership:** `FontProvider`, main owned brushes/fonts, `ArtBlit`, DIB/DC guards; remaining manual popup handles are paired and bounded.
- **Allocation-minimized render fast path:** reusable DIB/frame/text buffers, static chrome/marquee cache, dirty-frame predicate.
- **Palette/text caching and timer coalescing:** already present.
- **Crash-log boundedness:** hard 8 MiB cap rather than unbounded append.
- **History cap:** 400 rows; artwork stripped from history entries.

Changing these solely to match the wording of an exemplar would add risk without fixing a present defect.

## Proposed program

- **[P0-01] `create_exe.ps1` — remove any ability to recursively delete the live data root; prevents the entire user-data-loss class represented by `DATA-001`; (effort S; public-surface impact: preserved for WinGlance users, developer-tool `-FreshInstall` semantics changed/removed).**
- **[P0-02] `overlay/mod.rs`, render/accessibility surfaces — introduce an explicit no-media Idle pill invariant; prevents the hidden-while-process-alive class (`OVERLAY-001`); (effort M; public-surface impact: changed, required and clearly visible).** Keep the idle frame static so it does not create a new continuous upload/timer cost.
- **[P0-03] `smtc.rs` + `events.rs` + both consumers — carry refresh-vs-new-media provenance or strengthen identity with reliable secondary fields; prevents the genuine-event-suppression class (`DEDUP-001`); (effort M; public-surface impact: changed only for transitions currently swallowed).**
- **[P1-01] `main_window.rs` — correct Settings focus auto-scroll in one coordinate space; improves keyboard/UIA reachability; (effort S; public-surface impact: preserved except bug correction).**
- **[P1-02] `main_window.rs` history — separate source acceptance, pill disposition and reason; fixes misleading diagnostics and makes future suppression debugging reliable; (effort S/M; public-surface impact: changed text/highlighting only where currently false).**
- **[P1-03] `main_window.rs` tooltip — preserve signed virtual-screen coordinates; fixes negative-monitor placement; (effort S; public-surface impact: preserved except bug correction).**
- **[P1-04] monitor configuration/fullscreen — persist a stable physical monitor identity additively (with legacy index fallback) if stable-device semantics are confirmed; eliminates restart-dependent meaning of `index-N`; (effort M; public-surface impact: changed/additive config, argued because current behavior is already inconsistent).** No new crate is needed; lighter alternative to any display-enumeration dependency is the existing Win32 device name/monitor APIs.
- **[P1-05] `process_picker.rs` + accessibility provider — expose row checked state as UIA Toggle; gives assistive technology the state that will actually be persisted; (effort M; public-surface impact: visual behavior preserved, accessibility surface expanded).**
- **[P1-06] main Activity rendering — derive an AA-compliant effective title color with the existing contrast helper; prevents unreadable custom themes; (effort S; public-surface impact: changed only for failing custom colors).**
- **[P1-07] overlay hover policy — make Expanded dismiss-on-hover reversible on leave (preferred) while keeping the setting; reduces accidental dismissal without removing the feature; (effort S/M; public-surface impact: changed and noticeable; execute only after maintainer decision `R-02`).**
- **[P2-01] CI — add pinned complexity, coverage/CRAP and mutation gates and ensure release-candidate refs get a check; converts the requested release thresholds from aspirations into evidence; (effort M; public-surface impact: preserved).** Avoid a new runtime dependency; CI-only tools are preferable to shipping analysis crates.
- **[P2-02] tests — add pure seams/regressions for nonzero-scroll focus, refresh provenance, signed tooltip pack, monitor identity restart semantics, history disposition and picker UIA toggles; (effort M; public-surface impact: preserved).**
- **[P2-03] docs — reconcile config persistence, cache bounds, CI triggers, startup/no-media/monitor contracts after the chosen behavior lands; kills the current D8 drift class; (effort S; public-surface impact: preserved).** A config-example/default parity test should be added if CI does not already cover the complete file; do not generate/overwrite the user's config as part of that test.

---

# Release-quality metric assessment

The requested thresholds are sensible **gates**, but they are not currently evidenced at the audited head. The latest commit is explicitly a control-flow-hotspot refactor, which is encouraging, not proof.

| Target | Audit result at `32f27897…` | Release action |
|---|---|---|
| Cyclomatic Complexity < 22 | **Unable to verify** | Measure per function in CI; refactor only functions over threshold. |
| Cognitive Complexity < 22 | **Unable to verify** | Same; keep callback dispatch decompositions if they measure below limit. |
| Halstead Difficulty < 80 | **Unable to verify** | Add reproducible analyzer; do not alter Win32 wrappers merely to lower token metrics. |
| CRAP < 25 | **Unable to verify** | Requires complexity + coverage evidence; gate changed functions first if full-repo tool is noisy. |
| Surviving mutants = 0 | **Unable to verify** | Add mutation job; zero survivors before release for deterministic pure modules. Win32 integration mutants may need explicit equivalent-mutant review. |
| Dead code = 0 | **Unable to certify** | Current source shows no obvious dead module; require current `clippy -D warnings` plus explicit dead-code scan. |
| Redundant code = 0 | **Unable to certify** | Use clippy + structural review; “zero” must not force harmful abstraction of intentionally explicit Win32 teardown paths. |
| `any` / `unknown` types = 0 | **Not directly applicable as a Rust type-system metric** | Rust has no TypeScript-style `any`/`unknown`. Audit `dyn Any`, raw opaque payloads and unchecked casts instead. **Do not** treat mandatory TOML “unknown keys” preservation as a type violation. |

Recommended CI interpretation: fail on measured over-threshold functions/mutants, publish the report artifact, and make metric-tool versions explicit. This keeps the goal objective without creating refactors for metric theater.

---

# Architecture-reviewed minimal implementation plan

This plan is intentionally smaller than the prompt's exemplar program because the branch already completed many of those hardening projects. Each commit is a coherent review unit; before moving to the next, perform the requested **Architect review → implementation → Architect review/amend-until-satisfied** loop. Where a commit contains a visible behavior change, it is called out.

1. **`fix(tooling): make fresh-install simulation incapable of deleting live user data`**  
   Remove the live `%APPDATA%` recursive delete path. If retained, fresh-install simulation must operate only under an explicit disposable root with a hard production-root refusal. Add a script-level guard/test that cannot touch the real data root. **No application UX change; developer tooling semantics change.**

2. **`fix(startup): honor the silent first-launch contract`**  
   Remove `first_run` as a reason to show the maximized tracking window. Keep discoverability through tray/pill only. Update first-run tests and the immediately-coupled README/config/AGENTS wording in the same commit. **NOTICEABLE:** first launch no longer opens the tracking window. Treat explicit `start_in_tray = false` per `R-04` before implementation; my preferred production contract is that only an explicit user action after launch opens the main window.

3. **`feat(overlay): add a truthful no-media idle pill state`**  
   Add an explicit Idle content/phase, show it at cold start, and settle to it when the final source retires/no playing successor exists. Keep it static (no continuous animation/render) and passive. Update pill UIA accessible name for idle. **NOTICEABLE:** pill remains visible with no media. Resolve the fullscreen-suppression scope in `R-03` during Architect review; do not accidentally remove deliberate game/fullscreen suppression without a decision.

4. **`fix(events): distinguish metadata refreshes from genuine media transitions`**  
   Replace the one-side-missing-art identity shortcut with explicit refresh provenance or a richer stable identity decision shared by worker, overlay and history. Tests must prove both sides: late art/metadata updates in place; genuine same-title transitions notify. **NOTICEABLE only in the previously swallowed edge case.**

5. **`fix(accessibility): close settings, picker, and contrast gaps`**  
   Fix nonzero-scroll focus math; expose process-picker checked state through UIA Toggle; derive an AA-compliant effective Activity title color while preserving stored config. Grouped because these are all accessibility truth/reachability fixes and share no product-state redesign. **NOTICEABLE only for broken keyboard focus, assistive technology, or low-contrast custom themes.**

6. **`fix(history): make history diagnostics truthful and monitor-correct`**  
   Split `accepted` into source acceptance + display disposition/reason; render the actual reason in tooltips/highlight. Add a signed virtual-screen coordinate pack helper and use it for track tooltips. Tests cover notifications-off/redundant/filtered/internal rows and negative X/Y monitors. **Visible correction, no intended workflow change.**

7. **`fix(display): make configured monitor identity deterministic across restarts`**  
   Preferred design: add an additive stable device-identity field resolved from the existing Win32 display data, retain legacy `index-N` as fallback/migration, and make Settings/docs show the real semantics. No dependency addition. **NOTICEABLE on dock/reorder/restart; config surface additive.** If the maintainer instead chooses pure enumeration-index semantics, remove stickiness and document that choice—but make one contract true everywhere.

8. **`refine(overlay): make dismiss-on-hover recover from accidental leave`**  
   After explicit maintainer approval of `R-02`, keep `dismiss_on_hover` but cancel/release the 500 ms arm when the pointer leaves before expiry; preserve compact first-hover hold. Add edge-jitter regression tests. **NOTICEABLE BEHAVIOR CHANGE.** If the current one-way policy is reaffirmed, skip this commit and record that decision rather than changing for taste.

9. **`chore(ci): enforce release-quality and mutation gates`**  
   Add pinned CI tooling for cyclomatic/cognitive/Halstead/CRAP and mutation; run it on the release-candidate path used by this project (PR/manual/selected branches). Keep existing fmt/clippy/test/release-build/audit/deny. Measure first; only then create follow-up hotspot refactors if a function genuinely exceeds thresholds. No runtime dependency.

10. **`docs: reconcile runtime contracts and audit-facing documentation`**  
    Sweep only remaining documentation not already updated beside its behavior: typed-invalid-section persistence, overlay cache 8, exact CI triggers, monitor model, architecture/repo map and the finalized idle/fullscreen/startup semantics. Add/confirm a CI test that validates `config.example.toml` against code defaults without touching live APPDATA.

**No separate commits are recommended for** queue bounds, config atomicity, artwork decode threading/generation, crash-log rotation/cap, broad GDI RAII, frame-allocation cleanup, palette caching, history cap, or typed `GWLP_USERDATA`: those are already materially solved on this branch. Rewriting them now would be change for change's sake.

---

# Findings Summary Table

| ID | Severity | Area | Location | Issue (one line) | Scenario | Basis |
|---|---|---|---|---|---|---|
| DATA-001 | Critical | User data / tooling | `create_exe.ps1:85-91` | `-FreshInstall` recursively deletes the sacred live data directory. | 8 / tooling | Reasoned but not executed |
| START-001 | High | Startup / UX | `config.rs:618-641`; `main_window.rs:1384-1410` | First run forces a maximized window, violating silent-launch mandate. | 1 | Reasoned but not executed |
| OVERLAY-001 | High | Overlay state / UX | `overlay/mod.rs:185-198,1285-1345,2280-2445` | No-media state is Hidden, not mandated always-present Idle pill. | 1, 7 | Reasoned but not executed |
| DEDUP-001 | High | Media identity / suppression | `events.rs:375-414` | One-side-missing artwork can make genuine same-title media look like a refresh. | 3, 4 | Reasoned but not executed |
| A11Y-001 | Medium | Keyboard/UIA | `main_window.rs:4100-4255` | Settings focus auto-scroll mixes document and client coordinates. | 6 / Settings | Reasoned but not executed |
| A11Y-002 | Medium | Contrast | `config.rs:330-390`; `main_window.rs:2600-2765` | Arbitrary `text_color` can make Activity title fail WCAG AA. | UI config | Reasoned but not executed |
| A11Y-003 | Medium | Process picker UIA | `process_picker.rs:620-915` | Custom checked state is not exposed to assistive technology. | Picker | Reasoned but not executed |
| HIST-001 | Medium | UX truth / history | `main_window.rs:1013-1048,2360-2575,5070-5145` | Muted history rows can falsely say “filtered by allowed apps”. | 14 | Reasoned but not executed |
| TOOLTIP-001 | Medium | Multi-monitor tooltip | `main_window.rs:1935-2010` | Negative virtual-screen tooltip coordinates are clamped to zero. | 7, 10 | Reasoned but not executed |
| MON-001 | Medium | Monitor model | `overlay/fullscreen.rs:211-307` | `index-N` means sticky physical display in-run but enumeration slot after restart. | 7, 10 | Reasoned but not executed |
| HOVER-001 | Medium | Hover UX | `overlay/mod.rs:67-83,640-675` | Expanded hover arms one-way dismissal even after pointer leaves. | 5 | Reasoned but not executed |
| SINGLE-001 | Medium | Single-instance availability | `main.rs` singleton; `architecture.md:84-122` | Known mutex can be squatted to deny startup; deliberate fail-closed choice. | 9 / hostile second instance | Reasoned but not executed |
| CI-001 | Medium | CI / quality | `.github/workflows/ci.yml:1-45` | Audited head has no status and requested complexity/mutation gates do not exist. | Release gate | Unable to verify |
| DOC-001 | Medium | Documentation | `configuration.md:1-18`; `config.rs:600-775` | Typed-invalid-section persistence behavior is documented incorrectly. | 8 | Reasoned but not executed |
| DOC-002 | Low | Documentation | `development.md:151-160`; `overlay/mod.rs:214-231` | Development docs say track cache cap 3; branch uses 8. | 14 / memory audit | Reasoned but not executed |
| DOC-003 | Low | Documentation | `README.md:156-164`; `ci.yml:1-11` | README says every push gets CI; workflow push is main-only. | Release gate | Reasoned but not executed |

---

# Major Refactors Table

| ID | Refactor | Behavior change? | Modules touched | Effort | Priority | Why it matters |
|---|---|---|---|---|---|---|
| P0-01 | Remove live-data fresh-install deletion | Developer tooling only | `create_exe.ps1` | S | P0 | Makes accidental sanctioned-tool data loss impossible. |
| P0-02 | Explicit no-media Idle pill state | **Yes** | `overlay/mod.rs`, render/UIA, startup tests | M | P0 | Makes hidden-while-alive/no-media state impossible. |
| P0-03 | Refresh provenance / stronger media identity | **Yes, edge case** | `smtc.rs`, `events.rs`, overlay, main history | M | P0 | Makes genuine same-title event suppression substantially harder/impossible by construction. |
| P1-01 | Settings focus coordinate fix | Bug correction | `main_window.rs` | S | P1 | Keyboard/UIA focus remains reachable at nonzero scroll. |
| P1-02 | History disposition model | Tooltip/highlight correction | `main_window.rs` | S/M | P1 | UI tells the truth about why an event was muted. |
| P1-03 | Signed tooltip position helper | Bug correction | `main_window.rs` | S | P1 | Correct negative-coordinate monitor support. |
| P1-04 | Stable persisted monitor identity | **Yes/additive config** | config, fullscreen, Settings/docs | M | P1 | Removes restart-dependent monitor semantics. |
| P1-05 | Picker UIA Toggle provider | Accessibility surface only | `process_picker.rs`, accessibility | M | P1 | Screen readers can perceive the state users persist. |
| P1-06 | Effective AA Activity text color | **Yes for bad custom colors** | main render, shared contrast helper | S | P1 | Prevents unreadable primary title text. |
| P1-07 | Reversible hover dismissal | **Yes** | overlay hover/morph tests/docs | S/M | P1 | Reduces accidental disappearance; requires maintainer decision. |
| P2-01 | Complexity/coverage/mutation CI gates | No runtime | CI | M | P2 | Converts release-quality targets into reproducible evidence. |
| P2-02 | Targeted regression seams | No | tests/pure helpers | M | P2 | Pins the exact boundary mistakes found by this audit. |
| P2-03 | Documentation/config parity sweep | No runtime | docs/config tests | S | P2 | Prevents future contract drift. |

---

# Risk Register

| ID | Area | Open question / suspected deliberate design | Why it matters | How to confirm |
|---|---|---|---|---|
| R-01 | Media identity | One-side-missing art is intentionally treated as same media to absorb late thumbnails. | Tightening blindly would reintroduce duplicate pills; leaving it can swallow a genuine replay/version. | Live two-case test: same track gets art late → exactly one pill; same title/artist genuine new item with delayed/missing art → two transitions. Decide identity contract from logs. |
| R-02 | Hover | One-way Expanded dismissal is explicitly documented as “hover means I've seen it”. | It may be intentional despite surprising UX; changing it is noticeable. | Maintainer decision + live mouse-edge test. If retained, document accessibility rationale; if changed, verify leave cancels without timer oscillation. |
| R-03 | “Always visible” vs fullscreen | Hard rule 5 says pill always visible, while scenario 7/current config explicitly expect fullscreen/listed-foreground hide/show. | Literal enforcement would remove a deliberate gaming behavior and contradict another audit scenario. | Maintainer decision: interpret “always visible” as “never absent solely because there is no media” (my recommendation), or prohibit fullscreen hiding too. |
| R-04 | `start_in_tray = false` | Hard no-popup startup contract conflicts with an existing explicit setting whose purpose is to open the window. | Forced first-run popup is clearly wrong; explicit user opt-in is ambiguous. | Maintainer decision. Recommended production rule: startup itself remains silent; main window opens only from tray. If retaining the key, document it as an intentional exception to the global no-popup sentence. |
| R-05 | Monitor model | Sticky physical device during a run looks deliberate and improves docking stability. | Current docs/config name `index-N` cannot promise both stickiness and enumeration semantics across restart. | Dock/reorder/restart live test; maintainer selects stable-device vs pure-index contract. |
| R-06 | Picker accessibility | Native owner-drawn listbox may expose label/selection but not custom `LB_SETITEMDATA` checkbox state. | Keyboard works while screen-reader truth may not. | Inspect with Narrator/Accessibility Insights: each row must announce checked/unchecked and state changes on Space. |
| R-07 | Release metrics | No branch-head workflow/status or metric artifact exists. | Numeric production gates cannot be truthfully signed off. | Run the finalized CI on checkpoint/PR and retain complexity/coverage/mutation reports. |
| R-08 | History semantics | `accepted` has drifted from “source allowed” into “event reached pill”. | A single boolean cannot explain all muted-row reasons. | Maintainer chooses desired row taxonomy; verify notifications-off/redundant/filter/churn/failure rows. |
| R-09 | Singleton squat | Fail-closed is deliberate to protect config/log from dual writers. | Same-user process can deny startup, but same-user attackers can also kill the app; full DoS prevention is not a realistic security boundary. | Maintainer threat-model decision. If attempting mitigation, concurrency-test two legitimate simultaneous launches plus a fake holder before adopting it. |

---

# Coverage statement

## Per depth pass

- **Pass A — findings filed:** `DATA-001`, `DEDUP-001`, `SINGLE-001`. Clean evidence elsewhere: bounded/sanitized hostile SMTC input, fail-closed config, verified atomic production writes, bounded logs/crash log, panic-contained FFI and thread-affine COM ownership.
- **Pass B — findings filed:** none for current leak/boundedness defects. **Clean — evidence:** explicit inventory above pairs created GDI/USER/kernel objects and all requested caches/channels/logs have finite bounds. Runtime counts are **Reasoned but not executed**.
- **Pass C — findings filed:** none requiring a performance rewrite. **Clean — evidence:** reusable surfaces/text buffers, cached chrome/marquee, dirty-frame skip, reduced static/comet cadence, off-UI fixed-size decode, bounded fanout. Estimates supplied; no measurements claimed.
- **Pass D — findings filed:** `START-001`, `OVERLAY-001`, `A11Y-001`, `A11Y-002`, `A11Y-003`, `HIST-001`, `TOOLTIP-001`, `MON-001`, `CI-001`, `DOC-001`, `DOC-002`, `DOC-003`. Core thread/config/state boundaries otherwise hold.
- **Pass E — findings filed:** scenarios 1, 3, 5, 7, 8, 10 and 14 expose filed findings; every scenario is accounted for above. Scenarios 2, 4, 6, 9, 11, 12 and 13 are clean by code trace but **Reasoned but not executed**.
- **Pass F — findings/program filed:** P0–P2 program above. Existing bounded queues, atomic save, off-UI decode/generation, crash cap, history cap, render caches and principal RAII are explicitly recognized as already satisfied so they are not duplicated.

## Per scenario

1. **Findings filed:** `START-001`, `OVERLAY-001`.
2. **Clean — evidence:** per-source debounced resync/churn exclusion before emit; independent compliant-source processing.
3. **Findings filed:** `DEDUP-001`; hostile-input memory/decode bounds otherwise clean.
4. **Clean — evidence:** bounded/coalesced event pipeline and normalized timeline math; no zero-duration division path found.
5. **Finding filed:** `HOVER-001`; timer IDs/lifetime remain bounded.
6. **Clean — evidence:** DPI swaps invalidate and free font/DIB caches; no unbounded scaled-art variant cache.
7. **Findings filed:** `MON-001`, `TOOLTIP-001`; fullscreen-vs-always-visible ambiguity retained as `R-03`.
8. **Finding filed:** `DOC-001`; implementation protects corrupt/oversized/external-edited config without overwrite.
9. **Clean — evidence:** abandoned-mutex takeover, bounded crash log, tray teardown/restart handshake; live timing unexecuted. `SINGLE-001` covers hostile name squatting specifically.
10. **Finding filed:** `MON-001` (and `TOOLTIP-001` for negative topology); cache invalidation/fallback prevents ordinary orphaning.
11. **Clean — evidence:** 1 MiB live-log and 8 MiB crash-log hard caps.
12. **Clean — evidence:** `TaskbarCreated` re-add + retry, per-open menu destruction, autostart own-key ownership.
13. **Clean — evidence:** explicit timer/hook/tray/window/state teardown; hung COM worker abandonment is globally bounded.
14. **Finding filed:** `HIST-001`; history count/memory is bounded at 400 and paint strings are precomputed.

**Final accounting:** all passes A–F and all 14 scenarios were performed statically. GUI/runtime observations, Windows handle counts and branch-head build/test results were not executed and are not represented as Verified.
