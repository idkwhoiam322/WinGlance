# windows 0.58 → 0.62: staged migration plan

Status: **done** (executed 2026-08-17; commits listed under "Staged plan"
below). The plan below is kept as the historical record — the headings
marked DONE are the actual landed commits on `dev-audit-fixes`:
`f1eff53` (Commit 1), `aefc915` (facade + call-site migration; the 2a–2c
split was folded into one commit because the `-D warnings` gate rejects the
intermediate states where facade functions would be dead code),
`75f2663` (Commit 3), and the cleanup commit (Commit 4).

Target: `windows = "0.62"` (+ `windows-core = "0.62"`), the current stable line
(0.62.2, released Oct 2025). We are on 0.58.0 from **July 2024** — two years
old. The migration is bundled: Cargo cannot keep two copies of `windows` for
one crate build, so every breaking change ships together. This document breaks
that one big diff into small, *individually green* commits and re-assesses each
breaking API change on its own merits.

## Why migrate at all (the honest case)

No known bug in WinGlance is fixed by 0.62, and none of the breaking changes
adds a feature we would use tomorrow. The value is maintenance and ecosystem
alignment:

- **0.62 is the current stable.** The windows-rs project has shipped no newer
  `windows` crate since (releases 70–73 touched bindgen/metadata/rdl only), so
  this is not chasing a moving target — 0.58 is simply two major metadata
  generations behind.
- **Windows metadata refresh** (windows-rs release 69, #3729). 0.62 carries the
  updated Win32/WinRT metadata; our SMTC surface
  (`Media_Control`, `Storage_Streams`) inherits any metadata-level fixes.
- **raw-dylib via windows-link** (release 69). 0.62 links through
  `windows-link` and drops the `windows-targets` import-lib crates
  (`windows_x86_64_msvc 0.52.x` etc. vanish from the lock; the 0.48.5 copy that
  remains is `windows-sys` 0.48's, pulled by a third-party dep, not ours). Less
  supply-chain surface, simpler linking.
- **windows-core 0.62 dropped its ole32.dll dependency** — fewer runtime DLL
  requirements (release 69, #3743).
- **Calling-convention fix for current toolchains** (release 66, #3622;
  rust-lang/rust#142330). We build on rustc 1.97; 0.58's generated `extern`
  declarations are the ones that generate future-incompatibility warnings on
  newer compilers.
- **Future-proofing against type collisions.** Today no dependency pulls
  `windows`, so the classic two-copies hazard (0.58 `HWND` vs 0.62 `HWND` are
  different types and cannot interoperate) does not exist. Every new dependency
  that bumps to ≥0.59 would create it; the longer we stay pinned, the more
  likely that becomes, and the bigger the eventual migration.

Honest counter-case: if the project is in "don't touch what works" mode, the
churn is pure cost today. Revisit this decision when (a) a dependency forces a
`windows ≥ 0.59` requirement, or (b) the next major metadata refresh lands —
the diff will only grow, never shrink.

## The inventory (168 errors, 8 categories)

Re-derived against `windows 0.62.2` with the `implement` feature removed from
`windows` (it moved to `windows-implement`, re-exported via `windows-core`).
154 of the 168 errors are one of two mechanical patterns.

| # | Breaking change | Sites | Nature |
|---|-----------------|-------|--------|
| 1 | Handle/pointer params became `Option<T>` (`SendMessageW` 39, `PostMessageW` 13, `SetWindowPos` 12, `KillTimer` 10, `SetTimer` 9, `CreateWindowExW` 9, `InvalidateRect` 7, `SetFocus` 2, `RegSetValueExW` 2, `PeekMessageW` 2, `GlobalFree` 2, `CreateDIBSection` 2, + 11 singles: `ValidateRect`, `UpdateLayeredWindow`, `TrackPopupMenu`, `ShellExecuteW`, `SetWinEventHook`, `SetCursor`, `SetClipboardData`, `MessageBoxW`, `IsWindow`, `DeleteTimerQueueTimer`, `CreateFileW`) | ~120 | wrap in `Some(...)` — ABI-identical (null-pointer optimization); `CreateWindowExW`'s hmenu went the other way (`Some(&HMENU)` → plain `HMENU`) |
| 2 | GDI object handles need `.into()` to `HGDIOBJ` (`DeleteObject` 19, `SelectObject` 9) | ~28 | `h.into()` |
| 3 | `CreateFontW` charset/precision/clip/quality became typed newtypes (`FONT_CHARSET` etc.) | ~5 | construct the newtypes |
| 4 | `BOOL`/`BOOLEAN` moved out of `Win32::Foundation` (now `windows::core::BOOL`/`BOOLEAN`, from `windows-result`) | 4 | import swap |
| 5 | `VARIANT` moved from `windows::core` to `windows::Win32::System::Variant` | 3 | import/path swap (feature `Win32_System_Variant` already enabled) |
| 6 | `EventRegistrationToken` moved from `windows::Foundation` to `windows::core` | 1 | import swap |
| 7 | `Error::from_win32()` (no-arg) removed | 2 | `Error::from_thread()` — same semantics (captures `GetLastError`) |
| 8 | `IAsyncOperation::get()` removed | 4 | local sync-wait helper (see below) — the only non-mechanical item |

Error kinds: 103× `E0308 mismatched types` + 51× `E0308 arguments incorrect`
(categories 1–3), 4× `E0599` `get` (category 8), 9× import/path (categories
4–6), 2× `E0599 from_win32` (category 7).

Primary spans by file (of the 139 errors whose primary span is in our source):
`main_window.rs` 57, `process_picker.rs` 45, `duration_dialog.rs` 8,
`positioner.rs` 8, `smtc.rs` 5, `gdi.rs` 4, `accessibility.rs` 3, `main.rs` 3,
`winutil.rs` 2, `autostart.rs` 2, `icon.rs` 2 (the rest point at the
`overlay/` render paths and the generated crate).

## Is each change worth the churn?

They only ship bundled, so the real question is *"is any category a
deal-breaker?"* — none is:

| # | Verdict | Why |
|---|---------|-----|
| 1 | **Yes, bundled — churn only** | Ergonomics change (NULL handles now explicit `Option`); zero functional gain, zero risk (`Some(T)` is ABI-identical). ~80 % of the total diff. This is what the seam below hides. |
| 2 | **Yes, bundled** | Marginal type-safety gain; mechanical. |
| 3 | **Yes, bundled** | Marginal type-safety gain (charset/precision/quality can no longer be mixed up); mechanical. |
| 4 | **Yes, bundled** | Part of the core-crate decoupling (`windows-result` split) that makes the *core* crates usable without the full `windows` crate; not a gain for us directly. Mechanical. |
| 5 | **Yes, bundled** | Same refactor; mechanical. |
| 6 | **Yes, bundled** | Same refactor; one line. |
| 7 | **Yes, bundled** | Naming clarity (`from_thread` = last error of this thread); semantics preserved. Two lines. |
| 8 | **Yes, bundled — needs care** | The only substantive removal: a convenience sync-blocker. We do synchronous waits on the SMTC worker thread, so we re-implement the ~15-line waiter 0.58 used to generate (`Status()` → `SetCompleted` waiter → `GetResults()`). Low risk but must be tested against a live session (can't hang or return before completion). |

**Aggregate verdict: migrate.** ~168 errors sounds intimidating; in reality it
is ~154 mechanical edits (two patterns), 9 import lines, 2 renamed calls, and
one small helper. The plan below turns it into ~8 small, each-green commits
that leave a permanent thin Win32 facade making future bumps nearly free.

## Staged plan — every commit green

CI runs on push (`.github/workflows/ci.yml`), so no intermediate commit may
break the build. A naive "bump first, fix after" sequence fails that bar — the
manifest bump alone produces the 168 errors, and call sites cannot be fixed
against 0.58 (0.58's signatures reject `Some(hwnd)`). The plan therefore
introduces a **thin `src/winapi.rs` facade first**: wrappers whose *signatures*
are identical on 0.58 and 0.62 (e.g. `post_message(hwnd: HWND, …)`,
`delete_object(h: impl Into<HGDIOBJ>)`, `create_font(…, charset: u32, …)`),
migrate every call site onto them **while still on 0.58** (each commit green),
and let the final bump commit touch only the wrapper bodies + the ~11
non-signature lines. The facade is worth keeping permanently — it centralizes
the `unsafe` and makes the next windows bump a wrapper-body edit.

### Commit 1 — async sync-wait helper (green on 0.58) — DONE

`wait_async` in `src/smtc.rs` replicating the removed generated `get()`
(status check → completed-handler waiter → `GetResults()`), generated by a
small macro into two instantiations covering the operation shapes the worker
awaits: `IAsyncOperation<T>` and — the artwork `ReadAsync` path —
`IAsyncOperationWithProgress<T, u32>`. Migrate **5** call sites (`RequestAsync()`,
two `TryGetMediaPropertiesAsync()`, `OpenReadAsync()`, `ReadAsync()`; the
`ReadAsync` site was a second error masked in the bump tally — 0.62 has no
`get()` on the progress variant either). Commit 1 also moves the SMTC event
tokens to `i64` in our code (`SessionSubscription` fields, the two register
helpers, and the unregister sites now wrap/unwrap `EventRegistrationToken.Value`)
so the subscription state is version-agnostic. Gate (0.58): fmt, clippy
`-D warnings`, 451 tests. Live smoke check (log shows one `track changed` per
change, no hang) still needs a desktop.

**Forward note (0.62)**: the *mechanism* is version-stable
(`Status`/`SetCompleted`/`GetResults` are stable WinRT methods), but the
helper's *type paths are not*: 0.58 exposes the operations as
`windows::Foundation::IAsyncOperation`, while 0.62 exposes the same
interfaces as `windows_future::IAsyncOperation` (`windows_future` re-exports
its bindings publicly). Commit 3 therefore flips the helper's four type paths
(operation types + the two completed-handler types) to `windows_future::*` —
~4 one-line edits, listed so the bump does not surprise.

### Commits 2a–2c — the facade, one consumer area at a time (green on 0.58)

Add `src/winapi.rs` with the ~26 thin wrappers (messaging/timers: `post_message`,
`send_message`, `peek_message`, `message_box`, `set_focus`, `set_cursor`,
`set_timer`, `kill_timer`, `set_window_pos`, `invalidate_rect`,
`validate_rect`, `is_window`, `track_popup_menu`, `update_layered_window`,
`shell_execute`, `set_clipboard_data`, `set_win_event_hook`,
`delete_timer_queue_timer`; registry/files: `reg_set_value`, `create_file`,
`global_free`, `create_dib_section`; window creation: `create_window`; GDI:
`select_object`, `delete_object`, `create_font`). Bodies are today's 0.58 forms.
Then migrate call sites, grouped so each commit is a reviewable unit:

- **2a — `main_window.rs`** (~57 primary errors; messaging, timers, fonts,
  `CreateWindowExW`, `DeleteObject`, `VARIANT` untouched here — imports move in
  the bump).
- **2b — `process_picker.rs`** (~45; messaging, fonts, brushes, `SetCursor`,
  `InvalidateRect`).
- **2c — the rest** (`overlay/mod.rs` + `render.rs` + `positioner.rs` +
  `duration_dialog.rs` + `gdi.rs` + `icon.rs` + `autostart.rs` + `main.rs` +
  `accessibility.rs` + `fullscreen.rs`): ~35 sites. Split further if desired.

Each commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test` — green, because the wrappers compile on 0.58 and the call sites
now use only the stable facade API.

### Commit 3 — the bump (small, green)

- `Cargo.toml`: `windows = "0.62"` (drop the `implement` feature — now
  re-exported through `windows-core`), `windows-core = "0.62"` (direct dep stays
  for the `implement!` codegen reference).
- `cargo update -p windows --precise 0.62.2` (+ `windows-core`).
- Flip the ~26 wrapper bodies to 0.62 forms (`Some(...)` wrapping, `h.into()`
  for `HGDIOBJ`, `FONT_*` newtype construction in `create_font`).
- Fix the ~11 non-signature lines: import paths for `BOOL` ×3, `BOOLEAN` ×1,
  `VARIANT` ×3 (incl. `main_window.rs:6229/6230` `windows::core::VARIANT` →
  `windows::Win32::System::Variant::VARIANT`), `EventRegistrationToken` ×1;
  `Error::from_win32()` → `from_thread()` ×2.
- Full gate: `create_exe.ps1 -Release` (fmt, all-targets check, clippy
  `-D warnings`, tests, release build, deny/audit) — or the raw commands if the
  packaging script is not to be run.

### Commit 4 — cleanup (green)

Re-sweep for any remaining direct raw calls that bypass the facade (should be
none after 2a–2c), confirm no `windows-targets` ≥0.52 remains in the lock,
re-run the full gate, and mark this document's status column "done".

**Total: ~8 commits** (landed as 4: the 2a–2c split folded — see the status
note at the top), each independently reviewable and CI-green, with the bump
itself reduced to a diff of wrapper bodies + ~11 one-line changes.

**Delivered in Commit 4's cleanup sweep**: three raw `CreateFileW` calls in
`winutil.rs` (passing `None` template — already 0.62-compatible, hence
absent from the error inventory) were moved onto the facade's `create_file`
for consistency; the sweep is otherwise empty.

## Alternative: no facade, single categorized commit

If the permanent indirection layer is unwanted, do the bump as one commit but
order the diff for review: (1) imports/moves, (2) `Some()` wrapping, (3)
`.into()` + `FONT_*` newtypes, (4) `from_thread`, (5) the async helper. The
diff is ~168 mechanical lines across 15 files — honest to review but atomic,
and the next bump repeats the whole exercise.

## SMTC metadata delta (0.58 → 0.62) — audited

Method-level diff of the generated `Media_Control` and `Storage_Streams`
bindings between windows 0.58.0 and 0.62.2 (both in the local registry),
against the exact surface WinGlance uses (`src/smtc.rs`).

**`GlobalSystemMediaTransportControlsSessionManager`** — one delta: every
SMTC event registration changed its token type from the `EventRegistrationToken`
struct to a plain `i64` (the refreshed WinRT metadata now declares these event
tokens as 64-bit integers; the struct still exists in `windows::core` for APIs
that still use it):

- `SessionsChanged` / `CurrentSessionChanged` now return `Result<i64>` (were
  `Result<EventRegistrationToken>`).
- `RemoveSessionsChanged` / `RemoveCurrentSessionChanged` now take `i64`.
- `RequestAsync()` and `GetSessions()` are unchanged.

**`GlobalSystemMediaTransportControlsSession`** — the same token delta on
`MediaPropertiesChanged` / `PlaybackInfoChanged` / `TimelinePropertiesChanged`
(add returns `i64`, remove takes `i64`). Everything else is identical:
`SourceAppUserModelId()`, `TryGetMediaPropertiesAsync()`, `GetPlaybackInfo()`,
`GetTimelineProperties()`, and the `MediaProperties` (10 methods),
`PlaybackInfo` (6), `TimelineProperties` (6) surfaces are unchanged, as are
the `PlaybackStatus`/`PlaybackType` enums and the `TypedEventHandler` types.

**Artwork stream path (`Storage_Streams`)** — completely identical:
`IRandomAccessStreamReference`, `IRandomAccessStream`,
`IRandomAccessStreamWithContentType`, `IBuffer`, `Buffer`, `DataReader` and
`InputStreamOptions` all match, so `Thumbnail()` → `OpenReadAsync()` →
`Size()` → `ReadAsync()` → `Buffer::Create()` → `DataReader::FromBuffer()` →
`ReadBytes()` behaves exactly as on 0.58. The only artwork-path change is the
shared `IAsyncOperation::get` / `IAsyncOperationWithProgress::get` removal
(`OpenReadAsync().get()` and `ReadAsync().get()`, both covered by Commit 1's
helpers).

**Migration impact on `smtc.rs` (corrected from the raw tally)**: the bump
error count showed 5 errors in smtc.rs (1 import + 4 `get`); the reality is
larger and partly masked: **5** `get` sites (the `ReadAsync` one was
deduped out of the tally) plus the event-token change — also masked, because
the unresolved `EventRegistrationToken` import suppressed the cascading type
mismatches. The token work landed in Commit 1 (fields, register helpers, and
unregister sites now flow `i64` with `EventRegistrationToken { Value }`
wrapping at the 0.58 boundary); Commit 3 only strips that wrapping and the
`.Value` unwraps (~8 one-line edits) plus the import. Nothing else in the
SMTC surface changes for us.

## Out of scope / notes

- `windows-sys 0.48` (third-party dep chain) stays as-is; it is a different
  crate family with no type interop.
- The `implement` feature removal is invisible to our code:
  `windows::core::implement!` resolves in 0.62 without it (verified in the
  scratch attempt — no macro-related errors).
- Behavior-sensitive change is only the async helper (category 8); everything
  else is type-level and covered by the existing 447 tests + clippy gate. The
  pill/tray/SMTC smoke checks still need a live Windows desktop per repo rules.
