use crate::winapi::create_file;
use log::{debug, error, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, GetSysColor, HBRUSH, HGDIOBJ};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::Shell::DefSubclassProc;
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, HCURSOR, IDC_ARROW, LoadCursorW, PostQuitMessage,
    RegisterClassExW, SPI_GETCLIENTAREAANIMATION, SPI_GETDISABLEOVERLAPPEDCONTENT, SPI_GETFOCUSBORDERWIDTH,
    SPI_GETHIGHCONTRAST, SPI_GETMESSAGEDURATION, SYSTEM_PARAMETERS_INFO_ACTION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SetWindowLongPtrW, SystemParametersInfoW, WNDCLASS_STYLES, WNDCLASSEXW, WNDPROC,
};
use windows::core::BOOL;
use windows::core::{PCWSTR, PWSTR};

/// Test-only seam for the synchronized swap scenario, compiled out of
/// production builds. While armed, the next `open_verified_file` call on
/// this thread runs the probe at the exact instant the attacker's swap
/// would land — after the parent pin is taken, before anything is opened or
/// truncated. The probe fires exactly once (it is taken), so it cannot leak
/// into later opens, and it is thread-local, so it cannot fire inside
/// another test's open. Nothing here is reachable from production code.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::RefCell;

    thread_local! {
        static OPEN_PROBE: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
    }

    /// Arms the probe for the next `open_verified_file` on this thread.
    pub(crate) fn arm_open_probe(probe: impl FnMut() + 'static) {
        OPEN_PROBE.with(|slot| *slot.borrow_mut() = Some(Box::new(probe)));
    }

    /// Runs and disarms the probe; a no-op when none is armed.
    pub(crate) fn fire_open_probe() {
        let probe = OPEN_PROBE.with(|slot| slot.borrow_mut().take());
        if let Some(mut probe) = probe {
            probe();
        }
    }
}

/// Registers a window class exactly once per window type (guarded by `guard`).
/// The shared boilerplate lives here: arrow cursor, default style and extra
/// bytes, the `RegisterClassExW` call, and the failure path. `background` is
/// called lazily (only when the class is not yet registered) and must return
/// an owned brush the helper deletes if registration fails — stock or null
/// brushes must return `None`, since stock objects must never be deleted.
/// Logs `description` on failure.
pub(crate) fn register_class_once(
    guard: &'static OnceLock<()>,
    instance: HINSTANCE,
    class_name: &[u16],
    window_proc: WNDPROC,
    background: impl FnOnce() -> Option<HBRUSH>,
    description: &str,
) -> Result<(), windows::core::Error> {
    if guard.get().is_some() {
        return Ok(());
    }
    // A null class cursor falls back to the default arrow, so a failed load
    // must never panic window registration.
    let cursor = match unsafe { LoadCursorW(None, IDC_ARROW) } {
        Ok(cursor) => cursor,
        Err(error) => {
            warn!("LoadCursorW(IDC_ARROW) failed: {error}; the class will use the default cursor");
            HCURSOR::default()
        }
    };
    let background = background();
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: window_proc,
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hbrBackground: background.unwrap_or_default(),
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        if let Some(brush) = background {
            let _ = unsafe { crate::winapi::delete_object(HGDIOBJ(brush.0)) };
        }
        warn!("RegisterClassExW failed for {description}");
        return Err(windows::core::Error::from_thread());
    }
    let _ = guard.set(());
    Ok(())
}

/// Stores a window's per-instance state pointer in its GWLP_USERDATA slot.
/// The pointer is a leaked box owned by the window, freed in WM_NCDESTROY.
pub(crate) fn set_window_state<T>(hwnd: HWND, state: *mut T) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    }
}

/// Clears a window's GWLP_USERDATA slot after WM_NCDESTROY freed the state
/// box, so a stale pointer is never read from a reused window handle.
///
/// Deliberately **private**: `release_window_state` is the only path that
/// clears the slot, so a window cannot inline a teardown that drops the box
/// before clearing it (box-first) without the helper — the exact reorder
/// the probe test pins, kept a compile error rather than a review nit. Any
/// raw `SetWindowLongPtrW` write to `GWLP_USERDATA` in a window file is
/// therefore a review must-fix by construction. (The install side,
/// `set_window_state`, stays public — windows must store their box at
/// WM_NCCREATE.)
fn clear_window_state(hwnd: HWND) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }
}

/// Releases a window's per-instance state box at WM_NCDESTROY: frees the
/// box (unless null) and clears the GWLP_USERDATA slot. Canonical order
/// shared by every window — slot clear first, box second — so a stale
/// pointer is never left readable in the slot after the box is freed. The
/// only pub(crate) way to clear the slot, so the order cannot be bypassed
/// by an inlined teardown. `state_ptr` must be the pointer captured at the
/// top of the handler (or, for handlers that read the slot per message,
/// read before this call), and this must run after any teardown that still
/// reads the state (GDI object deletion, hook/timer teardown). Call exactly
/// once per window.
pub(crate) fn release_window_state<T>(hwnd: HWND, state_ptr: *mut T) {
    clear_window_state(hwnd);
    if !state_ptr.is_null() {
        // SAFETY: the caller created the box with `Box::into_raw` at
        // WM_NCCREATE and has not freed it; this is the window's one
        // ownership-release point, called at most once.
        unsafe {
            drop(Box::from_raw(state_ptr));
        }
    }
}

/// Reads a window's per-instance state pointer from GWLP_USERDATA. Returns
/// null when the slot is empty or was cleared.
pub(crate) fn window_state<T>(hwnd: HWND) -> *mut T {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut T }
}

/// System accessibility/appearance preferences, sampled at startup
/// and re-sampled on every `WM_SETTINGCHANGE`. A `Copy`-sized snapshot behind
/// a lock, so any thread reads the current values cheaply; an SPI query that
/// fails keeps the documented default rather than failing the sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SystemPreferences {
    /// `SPI_GETCLIENTAREAANIMATION`: the user allows animated client-area
    /// content. `false` disables every WinGlance motion.
    pub client_area_animation: bool,
    /// `SPI_GETDISABLEOVERLAPPEDCONTENT`: the user asked to minimize
    /// translucent/overlapped decoration.
    pub disable_overlapped_content: bool,
    /// `SPI_GETHIGHCONTRAST` with `HCF_HIGHCONTRASTON`: a high-contrast
    /// theme is active.
    pub high_contrast: bool,
    /// `SPI_GETMESSAGEDURATION`: how long the user wants transient messages
    /// on screen, in milliseconds.
    pub message_duration_ms: u32,
    /// `SPI_GETFOCUSBORDERWIDTH`/`-HEIGHT`: the focus indication thickness
    /// the user chose, in pixels (1 when the query fails).
    pub focus_border_px: u32,
}

impl SystemPreferences {
    /// The safe defaults every failed query falls back to: animation and
    /// color allowed, no overlapped-content restriction, Windows' 5 s toast
    /// default, a 1 px focus border.
    pub(crate) const DEFAULT: SystemPreferences = SystemPreferences {
        client_area_animation: true,
        disable_overlapped_content: false,
        high_contrast: false,
        message_duration_ms: 5_000,
        focus_border_px: 1,
    };

    /// Motion is allowed only when the user has not turned client-area
    /// animation off and has not asked for overlapped/translucent content to
    /// be minimized; either preference alone makes every animation
    /// immediate/static.
    pub(crate) fn animations_enabled(&self) -> bool {
        self.client_area_animation && !self.disable_overlapped_content
    }

    /// Queries the live system preferences. Every query failure keeps the
    /// corresponding default and logs at debug level: preference sampling
    /// must never be a startup failure.
    pub(crate) fn sample() -> Self {
        let mut prefs = Self::DEFAULT;
        unsafe {
            query_bool(SPI_GETCLIENTAREAANIMATION, &mut prefs.client_area_animation);
            query_bool(SPI_GETDISABLEOVERLAPPEDCONTENT, &mut prefs.disable_overlapped_content);
            let mut high_contrast = HIGHCONTRASTW {
                cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
                dwFlags: HCF_HIGHCONTRASTON,
                lpszDefaultScheme: PWSTR::null(),
            };
            if let Ok(()) = SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                0,
                Some(&mut high_contrast as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            ) {
                prefs.high_contrast = (high_contrast.dwFlags & HCF_HIGHCONTRASTON).0 != 0;
            } else {
                debug!("SPI_GETHIGHCONTRAST failed; keeping the default");
            }
            let mut duration: u32 = 0;
            // SPI_GETMESSAGEDURATION reports whole seconds (the live system
            // answers 5 for Windows' default "5 seconds" toast duration).
            if let Ok(()) = SystemParametersInfoW(
                SPI_GETMESSAGEDURATION,
                0,
                Some(&mut duration as *mut u32 as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            ) && duration > 0
            {
                prefs.message_duration_ms = duration.saturating_mul(1000);
            }
            let mut border: u32 = 0;
            if let Ok(()) = SystemParametersInfoW(
                SPI_GETFOCUSBORDERWIDTH,
                0,
                Some(&mut border as *mut u32 as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            ) && border > 0
            {
                prefs.focus_border_px = border;
            }
        }
        prefs
    }
}

/// Reads a boolean SPI into `out` on success; a failure keeps the default.
unsafe fn query_bool(action: SYSTEM_PARAMETERS_INFO_ACTION, out: &mut bool) {
    let mut value = BOOL(0);
    let queried = unsafe {
        SystemParametersInfoW(
            action,
            0,
            Some(&mut value as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if queried.is_ok() {
        *out = value.as_bool();
    } else {
        debug!("SystemParametersInfoW({action:?}) failed; keeping the default");
    }
}

static SYSTEM_PREFERENCES: RwLock<SystemPreferences> = RwLock::new(SystemPreferences::DEFAULT);

/// Re-samples the system preferences, stores them, and returns the snapshot.
/// Called at startup and from `WM_SETTINGCHANGE`.
pub(crate) fn refresh_system_preferences() -> SystemPreferences {
    let sampled = SystemPreferences::sample();
    match SYSTEM_PREFERENCES.write() {
        Ok(mut slot) => *slot = sampled,
        Err(poisoned) => *poisoned.into_inner() = sampled,
    }
    sampled
}

/// The most recently sampled system preferences.
pub(crate) fn system_preferences() -> SystemPreferences {
    match SYSTEM_PREFERENCES.read() {
        Ok(slot) => *slot,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// Bounded, escaped preview of an untrusted string for log output: at most
/// `cap` scalar values, each escaped so control and invisible characters are
/// visible, plus the count of characters omitted. Keeps log-line allocations
/// independent of the raw input length. Shared by `smtc` and `icon` so the
/// two bounded-preview sites cannot drift apart.
pub(crate) fn log_preview(value: &str, cap: usize) -> (String, usize) {
    let mut preview = String::new();
    for (i, c) in value.chars().enumerate() {
        if i >= cap {
            return (preview, value.chars().count() - cap);
        }
        preview.extend(c.escape_debug());
    }
    (preview, 0)
}

/// Whether motion is currently allowed (client-area animation on, overlapped
/// content not minimized).
pub(crate) fn animations_enabled() -> bool {
    system_preferences().animations_enabled()
}

/// The current system window color as an opaque RGBA array (high-contrast
/// themes repaint their surfaces through this).
pub(crate) fn system_window_color() -> [u8; 4] {
    let color = unsafe { GetSysColor(COLOR_WINDOW) };
    [
        (color & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        0xFF,
    ]
}

/// Runs a foreign-callback body with panics contained. A Rust
/// panic that unwinds out of an `extern "system"` fn crosses the OS ABI
/// boundary, which is undefined behavior on Windows — every callback routes
/// its body through this guard so a bug degrades into a logged error and a
/// typed fallback instead. Returns the body's value, or the panic payload
/// after logging `context`.
pub(crate) fn catch_callback_panic<T>(
    context: &str,
    body: impl FnOnce() -> T,
) -> Result<T, Box<dyn std::any::Any + Send>> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).inspect_err(|panic| {
        error!("{context} panicked (contained): {panic:?}");
    })
}

/// Best-effort cleanup for a contained wndproc panic. A window that owns
/// resources the OS will not reclaim cleanly on a hard exit (the tray icon —
/// Explorer reaps it only on hover) registers a closure here; the
/// containment arm of `guarded_wndproc` runs it before posting the quit.
/// First registration wins; the cleanup itself is panic-contained so a
/// broken cleanup cannot turn a contained panic into an abort.
static PANIC_CLEANUP: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Registers the process-wide contained-panic cleanup (see `PANIC_CLEANUP`).
pub(crate) fn set_panic_cleanup(cleanup: Box<dyn Fn() + Send + Sync>) {
    let _ = PANIC_CLEANUP.set(cleanup);
}

/// Runs the registered panic cleanup, if any, panic-contained.
fn run_panic_cleanup() {
    if let Some(cleanup) = PANIC_CLEANUP.get() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup));
        if result.is_err() {
            // Never panics out of the containment arm itself; a failed
            // cleanup is logged best-effort through the ordinary logger.
            log::debug!("panic cleanup itself panicked; ignored");
        }
    }
}

/// WNDPROC wrapper: on a contained panic it logs, runs the registered
/// panic cleanup (e.g. removing the tray icon — the normal WM_DESTROY
/// teardown is skipped), asks the thread's message loop to quit so the
/// process exits through its normal teardown path, and answers with the
/// default window procedure's result for the message so the OS sees a
/// well-formed reply.
pub(crate) fn guarded_wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    context: &str,
    body: impl FnOnce() -> LRESULT,
) -> LRESULT {
    match catch_callback_panic(context, body) {
        Ok(result) => result,
        Err(_) => unsafe {
            run_panic_cleanup();
            PostQuitMessage(0);
            DefWindowProcW(hwnd, message, wparam, lparam)
        },
    }
}

/// Subclass-proc wrapper: on a contained panic it defers to the
/// next subclass in the chain / the original window procedure, so a broken
/// subclass degrades to default behavior instead of unwinding.
pub(crate) fn guarded_subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    context: &str,
    body: impl FnOnce() -> LRESULT,
) -> LRESULT {
    match catch_callback_panic(context, body) {
        Ok(result) => result,
        Err(_) => unsafe { DefSubclassProc(hwnd, message, wparam, lparam) },
    }
}

/// Enumeration-callback wrapper: on a contained panic it stops the
/// enumeration (returns FALSE). Callers that accumulate results keep what
/// they gathered so far; display/topology caches re-enumerate on the next
/// change event, so a stopped pass is stale, not corrupt.
pub(crate) fn guarded_enum(context: &str, body: impl FnOnce() -> BOOL) -> BOOL {
    catch_callback_panic(context, body).unwrap_or(BOOL(0))
}

/// Void-callback wrapper for timers, WinEvent hooks and queue-timer
/// callbacks: on a contained panic it simply no-ops; the next tick or event
/// retries.
pub(crate) fn guarded_void(context: &str, body: impl FnOnce()) {
    let _ = catch_callback_panic(context, body);
}

/// Tracks whether a window's WM_NCCREATE took ownership of the state box
/// passed through `lpCreateParams`, so the box's creator can tell, after a
/// failed `CreateWindowExW`, whether the window claimed it (and frees it in
/// WM_NCDESTROY) or whether it never materialized and the creator must free
/// it. Window creation is single-threaded on the UI thread, so a plain
/// atomic flag is race-free.
pub(crate) struct StateClaim {
    claimed: AtomicBool,
}

impl StateClaim {
    pub(crate) const fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
        }
    }

    /// Arms the flag before `CreateWindowExW`; `WM_NCCREATE` flips it via
    /// `claim` when the window object materializes.
    pub(crate) fn reset(&self) {
        self.claimed.store(false, Ordering::SeqCst);
    }

    /// Marks the state box as taken by the window. WM_NCCREATE calls this
    /// after storing `lpCreateParams` in GWLP_USERDATA.
    pub(crate) fn claim(&self) {
        self.claimed.store(true, Ordering::SeqCst);
    }

    /// Returns the state box to the caller when WM_NCCREATE never ran (a
    /// creation failure before the window object existed). `None` when the
    /// window owns the box — freeing it there would double-free, because the
    /// system tears the window down through WM_NCDESTROY first.
    pub(crate) fn take_unclaimed<T>(&self, state_ptr: *mut T) -> Option<Box<T>> {
        if self.claimed.load(Ordering::SeqCst) {
            None
        } else {
            // SAFETY: the caller created the box with `Box::into_raw` and
            // has not freed it; an unclaimed box is still owned by the
            // caller, so taking it back is sound.
            Some(unsafe { Box::from_raw(state_ptr) })
        }
    }
}

/// A window-registration slot (used by the positioner and the process
/// picker), the guarded form of `OnceLock<Mutex<T>>`. The inner mutex is
/// **private** and never exposed — the only write paths are `set` (install,
/// at open time) and the guarded `clear_registered`/`close_registered` free
/// functions — so a window cannot inline a clear that skips the match guard,
/// mirroring how `clear_window_state` makes `release_window_state` the only
/// way to clear `GWLP_USERDATA`. Any direct lock of the slot in a window
/// file is therefore a compile error by construction; `read` returns a copy
/// and never a guard, so a reader cannot hold the mutex across a call or
/// write through it.
pub(crate) struct Registered<T> {
    slot: OnceLock<Mutex<T>>,
}

impl<T> Registered<T> {
    pub(crate) const fn new() -> Self {
        Self { slot: OnceLock::new() }
    }

    /// Installs (or replaces) the registration at open time — the only
    /// write path that puts a *non-empty* value in the slot. The slot is
    /// initialized lazily with the type's empty value (`T::default()`),
    /// then overwritten. Window creation is single-threaded on the UI
    /// thread, so the get-or-init is purely defensive.
    pub(crate) fn set(&self, value: T)
    where
        T: Default,
    {
        if let Ok(mut guard) = self.slot.get_or_init(|| Mutex::new(T::default())).lock() {
            *guard = value;
        }
    }

    /// Reads the current registration as a copy; `None` when the slot was
    /// never opened. Never returns the mutex, so the caller cannot hold the
    /// guard (e.g. across `DestroyWindow`) or write through it.
    pub(crate) fn read(&self) -> Option<T>
    where
        T: Copy,
    {
        let m = self.slot.get()?;
        let guard = m.lock().ok()?;
        Some(*guard)
    }
}

/// Closes a window whose handle is registered in a `Registered<T>` slot,
/// e.g. the positioner or the process picker. `extract` pulls the window
/// handle out of the slot's value (returning `None` when no window is
/// open). The handle is copied out and the guard released before
/// `DestroyWindow`: the destruction messages (WM_DESTROY/WM_NCDESTROY) lock
/// the same slot again, and holding the mutex across `DestroyWindow` would
/// deadlock the UI thread.
pub(crate) fn close_registered<T>(slot: &Registered<T>, extract: impl FnOnce(&T) -> Option<HWND>) {
    let Some(m) = slot.slot.get() else {
        return;
    };
    let hwnd = {
        let Ok(guard) = m.lock() else {
            return;
        };
        extract(&*guard)
    };
    if let Some(hwnd) = hwnd {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// Clears a registered window's slot at teardown, but only when it still
/// names the window being torn down — the guarded form the positioner and
/// picker use, so a stale teardown can never clear a newer window's
/// registration. `matches` tests the current slot value (the closure
/// captures the dying window's handle). The cleared value is always the
/// slot's empty value (`T::default()`), never an arbitrary caller-supplied
/// state, so `set` is the only path that can leave a non-empty
/// registration. This is the only way a window can clear a registration:
/// the slot's mutex is private, so a direct `*guard = empty` in a window
/// file is a compile error. Window teardown is single-threaded, so the
/// guard is defensive — but it is the shared contract, and both windows
/// must use it.
pub(crate) fn clear_registered<T>(slot: &Registered<T>, matches: impl Fn(&T) -> bool)
where
    T: Default,
{
    let Some(m) = slot.slot.get() else {
        return;
    };
    if let Ok(mut guard) = m.lock()
        && matches(&guard)
    {
        *guard = T::default();
    }
}

/// UTF-16-encodes `value` with a trailing NUL terminator suitable for the
/// `PCWSTR` Win32 APIs. Single source of truth — used by `overlay`,
/// `main_window`, `positioner`, `autostart`, `process_picker`, and `main`.
pub(crate) fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copies `value` into a fixed-size wide-string buffer, always leaving the
/// array NUL-terminated: at most `buffer.len() - 1` code units are copied
/// and the terminator is written explicitly, never relying on the buffer's
/// prior contents (e.g. a zero-filled struct) for termination — so the
/// truncation boundary is terminated exactly like every shorter copy.
///
/// **Required pattern:** every hand-written copy into a fixed-size `[u16; N]`
/// buffer must use this helper. Hand-rolling it — `copy_from_slice` with
/// `len().min(N)`, leaning on the struct's zero-fill for the terminator —
/// leaves the array *unterminated* exactly at the truncation boundary
/// (`count == N`), and the Win32 reader (`Shell_NotifyIconW`, …) reads past
/// the end of the buffer. (API-managed buffers such as `GetClassNameW` /
/// `WM_GETTEXT`, and grow-until-fits idioms with explicit length checks, are
/// exempt — see `docs/architecture.md`.) Truncating a long value is the
/// caller's display concern, never an unterminated-buffer hazard.
pub(crate) fn copy_wide_terminated(buffer: &mut [u16], value: &str) {
    if buffer.is_empty() {
        return;
    }
    let mut count = 0;
    for unit in value.encode_utf16() {
        if count + 1 >= buffer.len() {
            break;
        }
        buffer[count] = unit;
        count += 1;
    }
    buffer[count] = 0;
}

// ────────────────────────────────────────────────────────────────────────────
// Verified app-data writes (reparse-safe: the data root and temp file are identity-checked)
//
// The threat model: path-based opens follow reparse points. A junction swapped
// into the data/log directory, or a symlink pre-created at a fixed temp name,
// can redirect a write to a location the user did not approve (confused
// deputy). Every write in the app therefore lands either verified or not at
// all:
//   - the target's parent is opened WITHOUT `FILE_SHARE_DELETE`, pinning it so
//     it cannot be renamed or removed for the duration of the operation, and
//     with `FILE_FLAG_OPEN_REPARSE_POINT` so the opened object's own reparse
//     attribute is visible;
//   - the opened parent is rejected if it IS a reparse point, is not a
//     directory, or its canonical final handle path does not equal the
//     caller's expected path (this comparison also rejects any junction in an
//     intermediate component — the final path would resolve to the link
//     target, not the expected path);
//   - temp files use randomized `CREATE_NEW` names with
//     `FILE_FLAG_OPEN_REPARSE_POINT`, so a pre-created link at that name can
//     never be followed (the create fails instead);
//   - the commit closes the temp handle and renames the temp onto the target
//     name through `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` and
//     extended `\\?\` paths, which exchanges the directory entry atomically
//     without following the target's own reparse point. Closing the handle
//     before the commit lets security-shell filters settle on the
//     just-written temp: a rename issued through the still-open handle
//     (`SetFileInformationByHandle(FileRenameInfo)`) intermittently fails
//     with ERROR_INVALID_NAME while such a filter is scanning the file. The
//     parent held pinned for the transaction makes the path un-redirectable
//     while the commit runs, and transient interference is retried with
//     backoff. The parent directory handle is then flushed (opened with
//     `FILE_WRITE_DATA`/`FILE_APPEND_DATA` directory-equivalents so the
//     flush is permitted) for the rename's write-through durability;
//   - on any pre-commit failure the temp is deleted via its handle and the
//     error is returned; callers log it and never fall back to a plain
//     relative path.
// ────────────────────────────────────────────────────────────────────────────

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Seek, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD, FILE_DISPOSITION_FLAG_DELETE, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FlushFileBuffers,
    GetFileInformationByHandle, GetFinalPathNameByHandleW, GetLongPathNameW, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_ALWAYS, OPEN_EXISTING, SetEndOfFile, SetFileInformationByHandle,
    SetFilePointer, WriteFile,
};

/// The Win32 DELETE access right (0x0001_0000); `windows` 0.58 does not export
/// it. Needed so the temp's handle can also delete it (disposition delete).
const DELETE_ACCESS: u32 = 0x0001_0000;
/// `SetFileInformationByHandle` disposition-ex information class from winnt.h.
/// The `windows` crate exports the `FileDispositionInfoEx` *value* but not
/// the struct definition it pairs with, so both live here (documented, stable
/// ABI: 21 = disposition-ex).
const DISPOSITION_INFO_EX_CLASS: i32 = 21;

/// `FileDispositionInfoEx` (winnt.h).
#[repr(C)]
#[derive(Clone, Copy)]
struct FileDispositionInfoEx {
    flags: u32,
}

/// Converts a `windows`-crate error into a `std::io::Error`, extracting the
/// Win32 code (facility 7) so callers see the underlying OS error.
fn to_io(error: windows::core::Error) -> io::Error {
    let code = error.code().0 as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        io::Error::other(error)
    }
}

/// True when the Win32 error carries the given Win32 code.
fn is_win32_code(error: &windows::core::Error, code: u32) -> bool {
    let raw = error.code().0 as u32;
    (raw & 0xFFFF_0000) == 0x8007_0000 && (raw & 0xFFFF) == code
}

/// Case-insensitive comparison of two paths on their UTF-16 forms, so a
/// `\\?\C:\...` final handle path compares equal to the caller's expected
/// path regardless of casing. The fold uses Unicode default lowercasing
///: the previous ASCII-only fold rejected a legitimate match when a
/// localized path component differed by non-ASCII casing, which made the
/// app run without logs. `to_lowercase` handles the common non-ASCII cases
/// without pulling in the `Win32_Globalization` feature for
/// `LCMapStringEx`; Turkic special-casing is out of scope here because both
/// sides come from the same machine's APIs.
pub(crate) fn paths_equal(a: &Path, b: &Path) -> bool {
    let fa = a.as_os_str().to_string_lossy().to_lowercase();
    let fb = b.as_os_str().to_string_lossy().to_lowercase();
    fa == fb
}

/// The `\\?\` extended-length form of `path`, the form
/// `GetFinalPathNameByHandleW` yields, so the two are directly comparable.
pub(crate) fn extended_path(path: &Path) -> PathBuf {
    let raw = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let already_extended = raw.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]);
    if already_extended {
        path.to_path_buf()
    } else {
        let mut prefixed = Vec::with_capacity(raw.len() + 4);
        prefixed.extend_from_slice(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]);
        prefixed.extend_from_slice(&raw);
        PathBuf::from(OsString::from_wide(&prefixed))
    }
}

/// Expands DOS 8.3 aliases while preserving the path's directory entries.
/// Unlike a canonicalizing open, `GetLongPathNameW` does not replace a
/// junction with its target, so comparing this spelling with a final handle
/// path still rejects reparse points in intermediate components.
fn long_extended_path(path: &Path) -> io::Result<PathBuf> {
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let needed = unsafe { GetLongPathNameW(windows::core::PCWSTR(wide.as_ptr()), None) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u16; needed as usize];
    loop {
        let written = unsafe { GetLongPathNameW(windows::core::PCWSTR(wide.as_ptr()), Some(buf.as_mut_slice())) };
        if written == 0 {
            return Err(io::Error::last_os_error());
        }
        if written as usize >= buf.len() {
            buf.resize(written as usize + 1, 0);
            continue;
        }
        let long = PathBuf::from(OsString::from_wide(&buf[..written as usize]));
        return Ok(extended_path(&long));
    }
}

/// Canonical final path (`\\?\C:\...`) of an open handle, via
/// `GetFinalPathNameByHandleW`. The call describes the opened object itself —
/// it never re-resolves the path through the filesystem — so a handle opened
/// with `FILE_FLAG_OPEN_REPARSE_POINT` reports the link's own path, and a
/// handle opened normally reports the resolved target.
fn final_path_of_raw(handle: HANDLE) -> io::Result<PathBuf> {
    let mut capacity = 256u32;
    loop {
        let mut buf = vec![0u16; capacity as usize];
        let len = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                &mut buf,
                windows::Win32::Storage::FileSystem::GETFINALPATHNAMEBYHANDLE_FLAGS(0),
            )
        };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if (len as usize) < buf.len() {
            let mut units = &buf[..len as usize];
            if units.last() == Some(&0) {
                units = &units[..units.len() - 1];
            }
            return Ok(PathBuf::from(OsString::from_wide(units)));
        }
        capacity = len + 1;
    }
}

/// A pinned, verified directory handle. While the guard lives the directory
/// cannot be renamed or removed (opened without `FILE_SHARE_DELETE`) and the
/// opened object is a plain directory (no reparse attribute).
pub(crate) struct DirGuard {
    handle: HANDLE,
}

impl Drop for DirGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Opens `dir`, pinned and verified, as the root for a write transaction.
/// Rejects: a missing directory (caller creates it first), a reparse point,
/// a non-directory, or a final handle path that differs from the expected
/// path (which also flags junctions in intermediate components).
pub(crate) fn open_pinned_parent(dir: &Path) -> io::Result<DirGuard> {
    let desired = (FILE_LIST_DIRECTORY
        | FILE_READ_ATTRIBUTES
        | FILE_WRITE_ATTRIBUTES
        | FILE_TRAVERSE
        | FILE_WRITE_DATA // = FILE_ADD_FILE: lets the flush write directory entries through
        | FILE_APPEND_DATA // = FILE_ADD_SUBDIRECTORY
        | FILE_DELETE_CHILD)
        .0;
    let wide = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        create_file(
            windows::core::PCWSTR(wide.as_ptr()),
            desired,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            HANDLE::default(),
        )
    }
    .map_err(to_io)?;

    let reject = |message: &str| {
        unsafe {
            let _ = CloseHandle(handle);
        }
        Err(io::Error::other(format!("{message} ({})", dir.display())))
    };

    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if let Err(error) = unsafe { GetFileInformationByHandle(handle, &mut info) } {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(to_io(error));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return reject("refusing to write through a reparse point");
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        return reject("write root is not a directory");
    }
    let final_path = final_path_of_raw(handle)?;
    let expected_path = match long_extended_path(dir) {
        Ok(path) => path,
        Err(error) => {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(error);
        }
    };
    if !paths_equal(&final_path, &expected_path) {
        return reject(&format!(
            "write root final path does not match the expected path (resolved to {})",
            final_path.display()
        ));
    }
    Ok(DirGuard { handle })
}

/// Randomized temp name: pid + sequence + subsecond clock, so no fixed name
/// can be pre-armed (CREATE_NEW + OPEN_REPARSE_POINT defeat a guessed link
/// anyway).
fn temp_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("wg-{:x}-{:x}-{:x}.tmp", std::process::id(), seq, nanos)
}

/// Deletes the temp through its own handle (disposition-delete) and closes
/// The pattern `sweep_orphan_temps` matches: only files the save path
/// itself creates (`wg-<pid>-<seq>-<nanos>.tmp`) are ever removed, so a
/// directory shared with anything else is untouched.
pub(crate) const ORPHAN_TEMP_PATTERN: (&str, &str) = ("wg-", ".tmp");

/// Best-effort removal of orphaned config-save temps: a hard crash
/// between temp creation and the rename commit leaves one randomized file
/// next to config.toml forever. Called once at startup from `main`, against
/// the data dir; matches ONLY this app's own temp naming. Failures are
/// silently ignored — a leftover temp is cosmetic, and deleting files on a
/// best-effort sweep must never risk user data.
pub(crate) fn sweep_orphan_temps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(ORPHAN_TEMP_PATTERN.0) && name.ends_with(ORPHAN_TEMP_PATTERN.1) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Deletes the temp through its own handle (disposition-delete) and closes
/// it. Only called on failure paths; best-effort.
fn delete_temp(handle: HANDLE) {
    let info = FileDispositionInfoEx {
        flags: FILE_DISPOSITION_FLAG_DELETE.0,
    };
    unsafe {
        let _ = SetFileInformationByHandle(
            handle,
            windows::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS(DISPOSITION_INFO_EX_CLASS),
            (&info as *const FileDispositionInfoEx).cast(),
            std::mem::size_of::<FileDispositionInfoEx>() as u32,
        );
        let _ = CloseHandle(handle);
    }
}

/// Atomically replaces `target` with `content` under the verified-write
/// discipline above. On any pre-commit failure the temp is deleted and `Err`
/// returns; the existing `target` entry is untouched. The write is durable
/// through the rename: the data is flushed, the directory entry is exchanged
/// relative to the held parent, and the parent directory handle is flushed
/// for the metadata change.
pub(crate) fn atomic_replace_file(target: &Path, content: &[u8]) -> io::Result<()> {
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"))?;
    let name_units = name.encode_wide().collect::<Vec<u16>>();
    if name_units.is_empty()
        || name_units.len() > 255
        || name_units.contains(&0)
        || name_units.contains(&(b'\\' as u16))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target name is not a single path component",
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no parent directory"))?;
    if !parent.exists() {
        std::fs::create_dir_all(parent)?;
    }
    let guard = open_pinned_parent(parent)?;

    for _ in 0..4 {
        let tmp_name = temp_name();
        let tmp_path = parent.join(&tmp_name);
        let wide = tmp_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let temp_handle = match unsafe {
            create_file(
                windows::core::PCWSTR(wide.as_ptr()),
                FILE_GENERIC_WRITE.0 | DELETE_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                HANDLE::default(),
            )
        } {
            Ok(handle) => handle,
            Err(error) if is_win32_code(&error, 183) => continue, // ERROR_ALREADY_EXISTS: name collision, retry
            Err(error) => return Err(to_io(error)),
        };

        // Belt and braces: the temp we just created resolves exactly where the
        // pinned parent says it should. Unreachable in practice (the parent
        // cannot move and CREATE_NEW cannot follow a link), kept as a hard
        // invariant. The expected path is the temp's own entry inside the
        // pinned parent, not the final target name.
        let temp_expected = match long_extended_path(&tmp_path) {
            Ok(path) => path,
            Err(error) => {
                delete_temp(temp_handle);
                return Err(error);
            }
        };
        let fail = move |message: &str, handle: HANDLE| {
            delete_temp(handle);
            Err(io::Error::other(message))
        };
        let temp_final = match final_path_of_raw(temp_handle) {
            Ok(path) => path,
            Err(error) => {
                delete_temp(temp_handle);
                return Err(error);
            }
        };
        if !paths_equal(&temp_final, &temp_expected) {
            return fail(
                &format!(
                    "temp file resolved outside the verified parent ({} vs {})",
                    temp_final.display(),
                    temp_expected.display()
                ),
                temp_handle,
            );
        }

        if let Err(error) = unsafe { WriteFile(temp_handle, Some(content), None, None) } {
            delete_temp(temp_handle);
            return Err(to_io(error));
        }
        if let Err(error) = unsafe { FlushFileBuffers(temp_handle) } {
            delete_temp(temp_handle);
            return Err(to_io(error));
        }

        // Commit: rename the temp onto the target name. The temp handle is
        // closed FIRST, then the commit goes through `MoveFileExW` with
        // extended paths. Committing while still holding the temp handle
        // (`SetFileInformationByHandle` + `FileRenameInfo`) races the
        // security-shell's scan of the just-written temp: its filter holds
        // the file through an oplock and the rename intermittently fails
        // with `ERROR_INVALID_NAME` for as long as the scan queue is busy —
        // observed at ~25% of saves under file churn, immune to POSIX-
        // semantics renames and to multi-second retries. Closing the handle
        // before the commit lets the filter settle, which is why every
        // mainstream runtime (Rust std, Go, .NET, Chromium) commits through
        // `MoveFileExW` instead. The parent stays pinned throughout, so the
        // directory cannot be swapped while the path below resolves; the
        // target entry (if any) is replaced atomically and its own reparse
        // point is never followed.
        let _ = unsafe { CloseHandle(temp_handle) };
        let src_units = extended_path(&tmp_path)
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>();
        let dst_units = extended_path(target)
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>();
        // Backoff between attempts: rides out a residual transient holder on
        // either path. The schedule is deliberately short (~82 ms total):
        // saves run on the UI thread, and the interference that used to need
        // seconds was the filter race on the held handle, which the
        // close-before-commit design removed.
        const BACKOFF_MS: [u64; 3] = [2, 16, 64];
        let mut move_err: Option<io::Error> = None;
        for attempt in 0..BACKOFF_MS.len() + 1 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(BACKOFF_MS[attempt - 1]));
            }
            match unsafe {
                MoveFileExW(
                    windows::core::PCWSTR(src_units.as_ptr()),
                    windows::core::PCWSTR(dst_units.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } {
                Ok(()) => {
                    move_err = None;
                    break;
                }
                Err(error) => {
                    move_err = Some(to_io(error));
                }
            }
        }
        if let Some(error) = move_err {
            // The commit failed outright: best-effort remove the temp so no
            // stray state is left next to the target.
            let _ = std::fs::remove_file(&tmp_path);
            return Err(io::Error::other(format!("rename commit failed: {error}")));
        }

        // Durability of the directory-entry change (the rename's write-through
        // intent): flush the parent directory handle. Best-effort: the
        // rename has already committed — the target holds the new content on
        // disk — so a flush failure must NOT be reported as a save failure.
        // The caller would then treat an applied change as unsaved (banner +
        // in-memory-only messaging) while the next launch reads the new
        // config. Log it and succeed; durability here is a power-loss
        // guarantee, not a correctness one, and the data dir sits on the same
        // volume the OS just performed a metadata-only rename on.
        if let Err(error) = unsafe { FlushFileBuffers(guard.handle) } {
            log::warn!(
                "config save: post-commit directory flush failed (data is written): {}",
                to_io(error)
            );
        }
        return Ok(());
    }
    Err(io::Error::other("could not create a unique temp file after 4 attempts"))
}

/// Opens `path` for writing under the verified-write discipline, with the
/// parent pinned and identity-verified through the open, the final component
/// opened with `FILE_FLAG_OPEN_REPARSE_POINT` so a pre-created link is never
/// followed, and the handle's final path checked against the expected path
/// BEFORE any mutation happens. The opened object is additionally rejected
/// when it carries the reparse attribute or is a directory, so a
/// final-component link is refused outright instead of being written through
/// or truncated. `truncate` (the caller's one destructive open) applies only
/// to the validated object. On any rejection the target entry and everything
/// outside the verified parent are byte-identical; nothing is ever created,
/// truncated, or written through a link or a swapped parent.
pub(crate) fn open_verified_file(path: &Path, truncate: bool) -> io::Result<File> {
    let parent = path.parent();
    if let Some(parent) = parent
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }
    // Held through the open, validation and (when requested) the truncate:
    // while the guard lives the parent cannot be renamed or removed, so no
    // swap synchronized between validation and open can redirect the write.
    let _guard = match parent {
        Some(parent) => Some(open_pinned_parent(parent)?),
        None => None,
    };
    // Race probe: the regression test runs its attacker's swap
    // attempt at this exact point — pin taken, open not yet started — to
    // prove the pin spans the whole validation-to-open window.
    #[cfg(test)]
    test_hooks::fire_open_probe();
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        create_file(
            windows::core::PCWSTR(wide.as_ptr()),
            (FILE_APPEND_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            HANDLE::default(),
        )
    }
    .map_err(to_io)?;

    let reject = |message: &str| {
        unsafe {
            let _ = CloseHandle(handle);
        }
        Err(io::Error::other(message))
    };

    // Identity checks on the OPENED OBJECT (the handle names it; the path
    // cannot be re-resolved) BEFORE any truncation or append.
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if let Err(error) = unsafe { GetFileInformationByHandle(handle, &mut info) } {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(to_io(error));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return reject(&format!(
            "refusing to open a reparse point as a log file ({} resolves to a link)",
            path.display()
        ));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        return reject(&format!(
            "refusing to open a directory as a log file ({})",
            path.display()
        ));
    }
    let final_path = match final_path_of_raw(handle) {
        Ok(path) => path,
        Err(error) => {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(error);
        }
    };
    let expected_path = match long_extended_path(path) {
        Ok(path) => path,
        Err(error) => {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(error);
        }
    };
    if !paths_equal(&final_path, &expected_path) {
        return reject(&format!(
            "open target final path does not match the expected path (resolved to {})",
            final_path.display()
        ));
    }

    if truncate {
        unsafe {
            let _ = SetFilePointer(handle, 0, None, windows::Win32::Storage::FileSystem::FILE_BEGIN);
            let _ = SetEndOfFile(handle);
        }
    }
    // SAFETY: `handle` is a real, owned, non-null kernel handle with no other
    // owner; the returned File closes it on drop.
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

/// Appends `data` to `path` through `open_verified_file` under a size cap:
/// the parent is pinned through the open, the final component is opened
/// without following a reparse point and rejected if it IS one, and the
/// identity check lands before any mutation. Used by the crash.log writers,
/// which run where a full temp+rename transaction is not warranted. When the
/// file already exceeds `cap`, it is truncated to zero before the append, so
/// a crash loop cannot grow it without bound (the allocation-free vectored
/// handler enforces the same budget with its own byte counter). The
/// truncation happens through the already-verified handle, never through a
/// re-resolved path.
pub(crate) fn append_verified_bounded(path: &Path, data: &[u8], cap: u64) -> io::Result<()> {
    let mut file = open_verified_file(path, false)?;
    if cap != u64::MAX && file.metadata()?.len() > cap {
        file.set_len(0)?;
    }
    // With FILE_WRITE_DATA granted (needed for the cap truncation) the OS no
    // longer writes at EOF automatically, so position the pointer explicitly
    // before every append.
    file.seek(std::io::SeekFrom::End(0))?;
    file.write_all(data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn orphan_temp_sweep_removes_only_matching_names() {
        // The sweep must delete exactly this app's `wg-*.tmp` files
        // and nothing else — a foreign temp-looking file or an unrelated
        // document stays put.
        let dir = std::env::temp_dir().join(format!("wg-sweep-{}-{}", std::process::id(), std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let keep = ["config.toml", "not-wg.tmp", "wg-preserved.tmp.bak", "random.tmp"];
        let remove = ["wg-1234-0-5678.tmp", "wg-abcd-ef-1234.tmp"];
        for name in keep.iter().chain(remove.iter()) {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        sweep_orphan_temps(&dir);
        for name in &keep {
            assert!(dir.join(name).is_file(), "{name} must survive the sweep");
        }
        for name in &remove {
            assert!(!dir.join(name).exists(), "{name} must be swept");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn callback_panics_are_contained_and_propagate_the_payload() {
        // The guard converts a panic into a logged Err instead of an
        // unwind across the ABI; a calm body passes through untouched.
        assert_eq!(catch_callback_panic("test callback", || 7).unwrap(), 7);
        let caught = catch_callback_panic("test callback", || panic!("injected"));
        let payload = caught.expect_err("the panic must be contained, not unwound");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"injected"));
    }

    #[test]
    fn wndproc_panic_falls_back_to_defwindowproc_and_posts_quit() {
        use windows::Win32::UI::WindowsAndMessaging::WM_NULL;
        // Reaching the assert at all proves the panic did not abort the
        // process; DefWindowProcW on a null window answers 0.
        let result = guarded_wndproc(HWND::default(), WM_NULL, WPARAM(0), LPARAM(0), "test wndproc", || {
            panic!("injected")
        });
        assert_eq!(result.0, 0);
    }

    #[test]
    fn a_contained_wndproc_panic_runs_the_registered_cleanup() {
        use windows::Win32::UI::WindowsAndMessaging::WM_NULL;
        // First registration wins process-wide, so this test both registers
        // and proves the containment arm runs it before the quit. A second
        // registration (the app's tray cleanup in a live run) would be
        // ignored — exactly the set-once semantics the tray path relies on.
        static RAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        set_panic_cleanup(Box::new(|| {
            RAN.store(true, Ordering::SeqCst);
        }));
        let result = guarded_wndproc(HWND::default(), WM_NULL, WPARAM(0), LPARAM(0), "test wndproc", || {
            panic!("injected")
        });
        assert_eq!(result.0, 0);
        assert!(
            RAN.load(Ordering::SeqCst),
            "the containment arm must run the registered cleanup"
        );
    }

    #[test]
    fn copy_wide_terminated_fills_fits_and_terminates() {
        let mut buf = [0u16; 16];
        copy_wide_terminated(&mut buf, "Song");
        assert_eq!(&buf[..5], &[b'S' as u16, b'o' as u16, b'n' as u16, b'g' as u16, 0]);
        assert_eq!(buf[5], 0, "the untouched tail of the zero-filled buffer stays zero");
        // A value that exactly fills len-1 slots gets its terminator in the
        // last slot, not one past it.
        let mut exact = [0u16; 4];
        copy_wide_terminated(&mut exact, "abc");
        assert_eq!(&exact, &[b'a' as u16, b'b' as u16, b'c' as u16, 0]);
    }

    /// Recursively collects the `.rs` files under `dir`.
    fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("src must be readable") {
            let path = entry.expect("a readable directory entry").path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Whether a `copy_from_slice` at 0-based line `copy_idx` looks like a
    /// hand-rolled wide-string copy: a wide marker within `±WINDOW` lines.
    /// The pre-helper tray pattern — `dest[..n].copy_from_slice(&src[..n])`
    /// with the `wide(...)` source created a line or two above — always
    /// carries the marker inside the window; a plain u8/binary copy (crash
    /// writer, DIB blits, palette fills, row blits) carries none.
    fn is_wide_copy_window(lines: &[&str], copy_idx: usize) -> bool {
        const WINDOW: usize = 3;
        const MARKERS: [&str; 4] = ["u16", "encode_utf16", "wide(", "as_utf16"];
        let lo = copy_idx.saturating_sub(WINDOW);
        let hi = (copy_idx + WINDOW + 1).min(lines.len());
        lines[lo..hi]
            .iter()
            .any(|line| MARKERS.iter().any(|marker| line.contains(marker)))
    }

    fn is_zero_fill_reliant_read_window(lines: &[&str], read_idx: usize) -> bool {
        // The read side of the fixed-array wide contract: an API-managed fill
        // (GetClassNameW, GetWindowTextW, WM_GETTEXT, …) writes a
        // NUL-terminated string into a fixed [u16; N] buffer, and the code
        // must read it back with an explicit length (the API's return value
        // or a NUL scan) — never the whole buffer, whose termination would
        // then rest on the zero-fill of `[0u16; N]` (and which embeds the
        // terminator and padding in the string when read whole). Flag a
        // non-sliced `from_utf16`/`from_utf16_lossy` read within ±3 lines of
        // such a fill. Capacity-idiom APIs that report size in/out
        // (QueryFullProcessImageNameW) and struct fills (Toolhelp's
        // szExeFile) are deliberately out of scope — the former's
        // truncate-to-size is the fix, the latter is the documented
        // struct-field limitation.
        const WINDOW: usize = 3;
        const FILL_MARKERS: [&str; 7] = [
            "GetClassNameW",
            "GetWindowTextW",
            "GetFinalPathNameByHandleW",
            "GetModuleBaseNameW",
            "GetModuleFileNameW",
            "GetModuleFileNameExW",
            "WM_GETTEXT",
        ];
        let line = lines[read_idx];
        if !line.contains("from_utf16") || line.contains("[..") {
            return false;
        }
        let lo = read_idx.saturating_sub(WINDOW);
        let hi = (read_idx + WINDOW + 1).min(lines.len());
        lines[lo..hi]
            .iter()
            .any(|candidate| FILL_MARKERS.iter().any(|marker| candidate.contains(marker)))
    }

    #[test]
    fn wide_copies_stay_in_winutil() {
        // Enforces the copy_wide_terminated contract mechanically: a
        // hand-written copy_from_slice into a fixed-size wide buffer must not
        // appear outside winutil.rs (the sanctioned home of the helper). Any
        // `copy_from_slice` whose ±3-line window carries a wide marker is
        // flagged; the plain u8/binary copies (crash-log writer in main.rs,
        // palette fills, overlay row blits, test pixel writes) carry no
        // marker and stay unflagged — the self-test below proves both sides.
        // Known limit (documented): a source slice stored far from the copy
        // (e.g. a struct field) without a nearby `u16`/`wide(` marker can
        // evade the lexical scan; the window catches the historical tray
        // shape and every idiomatic form.
        let mut files = Vec::new();
        collect_rs_files(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for path in files {
            if path.file_name().is_some_and(|name| name == "winutil.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source files must be readable");
            let lines: Vec<&str> = text.lines().collect();
            for (idx, line) in lines.iter().enumerate() {
                if line.contains("copy_from_slice") && is_wide_copy_window(&lines, idx) {
                    offenders.push(format!("{}:{}", path.display(), idx + 1));
                }
            }
            scanned += 1;
        }
        assert!(
            scanned >= 15,
            "the scan must cover the source tree, scanned {scanned} files"
        );
        assert!(
            offenders.is_empty(),
            "hand-rolled wide-string copies outside winutil.rs — route fixed-size [u16; N] \
            copies through winutil::copy_wide_terminated:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn wide_fixed_buffer_reads_use_explicit_lengths() {
        // The read side of the fixed-array wide contract, enforced
        // mechanically: an API-managed fill into a fixed [u16; N] buffer must
        // be read back with an explicit length (the API's return value or a
        // NUL scan), never the whole buffer — a whole-buffer read leans on
        // the `[0u16; N]` zero-fill for termination and embeds the terminator
        // and padding in the string. Any non-sliced `from_utf16`/
        // `from_utf16_lossy` read within ±3 lines of a wide-API fill
        // (GetClassNameW, GetWindowTextW, WM_GETTEXT, …) is flagged. Every
        // current site is compliant — each read is sliced to the returned
        // length or explicitly NUL-trimmed — so the guard passes today and
        // catches a reintroduction.
        let mut files = Vec::new();
        collect_rs_files(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for path in files {
            if path.file_name().is_some_and(|name| name == "winutil.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source files must be readable");
            let lines: Vec<&str> = text.lines().collect();
            for (idx, line) in lines.iter().enumerate() {
                if line.contains("from_utf16") && is_zero_fill_reliant_read_window(&lines, idx) {
                    offenders.push(format!("{}:{}", path.display(), idx + 1));
                }
            }
            scanned += 1;
        }
        assert!(
            scanned >= 15,
            "the scan must cover the source tree, scanned {scanned} files"
        );
        assert!(
            offenders.is_empty(),
            "whole-buffer from_utf16 reads of wide-API fills — slice to the returned length \
            (or NUL-scan) instead of relying on the zero-fill:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn wide_copy_window_flags_the_banned_pattern_and_passes_u8_copies() {
        // The pre-helper tray shape: the wide(...) source sits a line or two
        // above the copy — inside the window, so it must be flagged.
        let banned = vec![
            "    let tip = wide(\"Song — Artist\");",
            "    let count = tip.len().min(data.szTip.len() - 1);",
            "    data.szTip[..count].copy_from_slice(&tip[..count]);",
            "    data.szTip[count] = 0;",
        ];
        assert!(
            is_wide_copy_window(&banned, 2),
            "the pre-helper tray pattern must be flagged"
        );
        // A plain byte copy — no wide marker in the window — must pass.
        let legit = vec![
            "fn crash_write_str(buf: &mut [u8], pos: usize, s: &[u8]) -> usize {",
            "    let end = (pos + s.len()).min(buf.len());",
            "    buf[pos..end].copy_from_slice(&s[..end - pos]);",
            "    end",
            "}",
        ];
        assert!(!is_wide_copy_window(&legit, 2), "a u8 copy must not be flagged");
    }

    #[test]
    fn wide_read_window_flags_the_zero_fill_reliant_shape_and_passes_sliced_reads() {
        // The banned read shape: GetClassNameW fills a fixed [0u16; N] buffer
        // and the code reads the whole buffer back — termination would rest
        // on the zero-fill, and the terminator + padding land inside the
        // string. Must be flagged.
        let banned = vec![
            "        let mut class = [0u16; 256];",
            "        let len = GetClassNameW(hwnd, &mut class);",
            "        let name = String::from_utf16_lossy(&class);",
        ];
        assert!(
            is_zero_fill_reliant_read_window(&banned, 2),
            "a whole-buffer read of a wide-API fill must be flagged"
        );
        // The compliant shape: the same fill, read with the returned length.
        let legit = vec![
            "        let mut class = [0u16; 256];",
            "        let len = GetClassNameW(hwnd, &mut class);",
            "        let name = String::from_utf16_lossy(&class[..len as usize]);",
        ];
        assert!(
            !is_zero_fill_reliant_read_window(&legit, 2),
            "a sliced read must not be flagged"
        );
        // A whole-buffer read with no wide-API fill in the window (a Vec
        // truncated to the API's size, or a struct field) is out of the
        // guard's scope and must pass.
        let no_fill = vec![
            "        buffer.truncate(size as usize);",
            "        let path = String::from_utf16_lossy(&buffer);",
        ];
        assert!(
            !is_zero_fill_reliant_read_window(&no_fill, 1),
            "reads without a wide-API fill marker must not be flagged"
        );
    }

    #[test]
    fn copy_wide_terminated_truncates_with_an_explicit_terminator() {
        // The truncation boundary is the case that must never be left
        // unterminated. The buffer is poisoned (no zero-fill to lean on): a
        // longer value fills up to len-1 and the explicit terminator lands in
        // the last slot, so the array is always terminated.
        let mut buf = [0xFFFFu16; 8];
        copy_wide_terminated(&mut buf, "abcdefghijklmnop");
        assert_eq!(
            &buf,
            &[
                b'a' as u16,
                b'b' as u16,
                b'c' as u16,
                b'd' as u16,
                b'e' as u16,
                b'f' as u16,
                b'g' as u16,
                0
            ]
        );
    }

    #[test]
    fn copy_wide_terminated_handles_empty_input_and_empty_buffer() {
        let mut buf = [0u16; 4];
        copy_wide_terminated(&mut buf, "");
        assert_eq!(buf[0], 0, "an empty value leaves a lone terminator");
        let mut empty: [u16; 0] = [];
        copy_wide_terminated(&mut empty, "x"); // must not panic
    }

    #[test]
    fn enum_panic_stops_the_enumeration_with_false() {
        assert_eq!(guarded_enum("test enum", || panic!("injected")).0, 0);
        assert_eq!(guarded_enum("test enum", || BOOL(1)).0, 1);
    }

    #[test]
    fn void_panic_no_ops_instead_of_unwinding() {
        // Reaching the assert proves containment.
        guarded_void("test timer", || panic!("injected"));
        let mut ran = false;
        guarded_void("test timer", || ran = true);
        assert!(ran);
    }

    #[test]
    fn preference_motion_mapping_requires_animation_and_plain_content() {
        // Motion needs client-area animation allowed AND overlapped content
        // not minimized; either restriction alone makes animations
        // immediate/static when system preferences disallow motion.
        assert!(SystemPreferences::DEFAULT.animations_enabled());
        let base = SystemPreferences::DEFAULT;
        assert!(
            !SystemPreferences {
                client_area_animation: false,
                ..base
            }
            .animations_enabled()
        );
        assert!(
            !SystemPreferences {
                disable_overlapped_content: true,
                ..base
            }
            .animations_enabled()
        );
        assert!(
            !SystemPreferences {
                client_area_animation: false,
                disable_overlapped_content: true,
                ..base
            }
            .animations_enabled()
        );
    }

    #[test]
    fn sampled_preferences_stay_within_sane_bounds() {
        // Sampling runs in the live session: values outside these bounds
        // would mean a broken query result, not a user preference.
        let prefs = SystemPreferences::sample();
        assert!(prefs.message_duration_ms > 0 && prefs.message_duration_ms <= 600_000);
        assert!(prefs.focus_border_px >= 1 && prefs.focus_border_px <= 32);
    }

    #[test]
    fn refresh_returns_and_stores_the_same_snapshot() {
        let sampled = refresh_system_preferences();
        let read_back = system_preferences();
        assert_eq!(sampled, read_back);
        // Restore whatever the session actually reports so other tests (and
        // this one, re-run) observe live values, not this test's write.
        refresh_system_preferences();
    }

    /// A uniquely-named temporary directory removed on drop.
    struct TestDir {
        dir: PathBuf,
    }

    impl TestDir {
        fn new(tag: &str) -> Self {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("winglance-winutil-{tag}-{}-{stamp}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn sibling_names(dir: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn normal_replace_writes_content_and_leaves_no_temp() {
        let guard = TestDir::new("replace");
        let target = guard.dir.join("config.toml");
        std::fs::write(&target, b"old").unwrap();

        atomic_replace_file(&target, b"new content").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new content");
        assert_eq!(sibling_names(&guard.dir), vec!["config.toml"], "no temp may remain");
    }

    #[test]
    fn verified_writes_accept_a_dos_short_path_alias() {
        let temp = std::env::temp_dir();
        if paths_equal(&extended_path(&temp), &long_extended_path(&temp).unwrap()) {
            return;
        }

        let guard = TestDir::new("short-path");
        let target = guard.dir.join("config.toml");
        atomic_replace_file(&target, b"content").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"content");
    }

    #[test]
    fn replace_creates_a_missing_parent_chain() {
        let guard = TestDir::new("deep");
        let target = guard.dir.join("a").join("b").join("config.toml");
        atomic_replace_file(&target, b"x").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"x");
    }

    #[test]
    fn replace_over_a_directory_fails_and_cleans_the_temp() {
        let guard = TestDir::new("over-dir");
        let target = guard.dir.join("config.toml");
        std::fs::create_dir(&target).unwrap();

        assert!(atomic_replace_file(&target, b"x").is_err());
        assert!(target.is_dir(), "the target entry must be left as-is");
        assert_eq!(sibling_names(&guard.dir), vec!["config.toml"], "no temp may remain");
    }

    #[test]
    fn replace_rejects_a_reparse_point_parent() {
        // Needs SeCreateSymbolicLinkPrivilege (Developer Mode / admin);
        // skipped when the OS refuses to create the link.
        let guard = TestDir::new("reparse-parent");
        let link = guard.dir.join("evil");
        let real = guard.dir.join("real");
        std::fs::create_dir_all(real.join("target")).unwrap();
        if std::os::windows::fs::symlink_dir(&real, &link).is_err() {
            return;
        }

        let target = link.join("config.toml");
        assert!(atomic_replace_file(&target, b"x").is_err());
        assert!(
            !real.join("target").join("config.toml").exists(),
            "nothing may be written through the link"
        );
    }

    #[test]
    fn replace_rejects_a_reparse_point_in_an_intermediate_component() {
        // `evil` is a directory link whose target is `real`: the write root
        // `evil\sub` resolves through the link, so its final handle path is
        // `real\sub` — the identity check must reject it even though the
        // opened component itself is an ordinary directory.
        let guard = TestDir::new("reparse-mid");
        let real = guard.dir.join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let evil = guard.dir.join("evil");
        if std::os::windows::fs::symlink_dir(&real, &evil).is_err() {
            return;
        }

        let target = evil.join("sub").join("config.toml");
        assert!(atomic_replace_file(&target, b"x").is_err());
        assert!(
            !real.join("sub").join("config.toml").exists(),
            "nothing may be written through the link"
        );
    }

    #[test]
    fn replace_replaces_a_target_symlink_without_following_it() {
        let guard = TestDir::new("target-link");
        let real = guard.dir.join("real-file");
        std::fs::write(&real, b"target").unwrap();
        let target = guard.dir.join("config.toml");
        if std::os::windows::fs::symlink_file(&real, &target).is_err() {
            return;
        }

        atomic_replace_file(&target, b"x").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"x", "the link entry must be replaced");
        assert_eq!(
            std::fs::read(&real).unwrap(),
            b"target",
            "the link target must never receive the write"
        );
    }

    #[test]
    fn append_verified_writes_and_obeys_the_cap() {
        let guard = TestDir::new("append");
        let path = guard.dir.join("crash.log");

        append_verified_bounded(&path, b"first\n", 100).unwrap();
        append_verified_bounded(&path, b"second\n", 100).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first\nsecond\n");

        // A file past the cap is truncated before the next append, so a crash
        // loop stays bounded: the file ends with only the latest line.
        append_verified_bounded(&path, b"third\n", 6).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"third\n");
    }

    #[test]
    fn append_verified_rejects_a_reparse_point_parent() {
        let guard = TestDir::new("append-reparse");
        let link = guard.dir.join("evil");
        let real = guard.dir.join("real");
        std::fs::create_dir_all(real.join("target")).unwrap();
        if std::os::windows::fs::symlink_dir(&real, &link).is_err() {
            return;
        }

        assert!(append_verified_bounded(&link.join("crash.log"), b"x", u64::MAX).is_err());
        assert!(!real.join("target").join("crash.log").exists());
    }

    #[test]
    fn open_verified_file_truncates_only_after_validation_and_appends_on_request() {
        // Truncating open: prior content is removed only after the object was
        // identity-validated (it is a plain file at the expected path).
        let guard = TestDir::new("open-truncate");
        let path = guard.dir.join("live.log");
        std::fs::write(&path, b"previous session\n").unwrap();
        let mut file = open_verified_file(&path, true).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0, "the validated file is truncated");
        file.write_all(b"new session\n").unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"new session\n");

        // Non-truncating open (the append path): existing content survives
        // and writes land at EOF.
        let mut file = open_verified_file(&path, false).unwrap();
        file.seek(std::io::SeekFrom::End(0)).unwrap();
        file.write_all(b"second line\n").unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"new session\nsecond line\n");
    }

    #[test]
    fn open_verified_file_creates_a_missing_parent_chain() {
        let guard = TestDir::new("open-deep");
        let path = guard.dir.join("a").join("b").join("live.log");
        let mut file = open_verified_file(&path, true).unwrap();
        file.write_all(b"x").unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn open_verified_file_rejects_a_final_component_link_and_leaves_the_target() {
        // A pre-created link at the final name must never be followed: the
        // open refuses it outright (the object carries the reparse
        // attribute), and the external target stays byte-identical.
        let guard = TestDir::new("open-final-link");
        let real = guard.dir.join("victim.log");
        let original = b"EXTERNAL TARGET DATA";
        std::fs::write(&real, original).unwrap();
        let link = guard.dir.join("log-Live.log");
        if std::os::windows::fs::symlink_file(&real, &link).is_err() {
            return;
        }

        assert!(open_verified_file(&link, true).is_err(), "a link must be rejected");
        assert!(
            std::fs::read(&real).unwrap() == original,
            "the external target must stay byte-identical"
        );
        assert!(
            std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
            "the link entry must not have been replaced or written through"
        );
    }

    #[test]
    fn open_verified_file_rejects_a_parent_junction_and_creates_nothing() {
        // A junction swapped into the parent chain must reject the open
        // before any create: nothing may be created inside the link target.
        let guard = TestDir::new("open-parent-junction");
        let real = guard.dir.join("real");
        std::fs::create_dir_all(real.join("logs")).unwrap();
        let evil = guard.dir.join("evil");
        if std::os::windows::fs::symlink_dir(&real, &evil).is_err() {
            return;
        }

        let target = evil.join("logs").join("live.log");
        assert!(open_verified_file(&target, true).is_err());
        assert!(
            !real.join("logs").join("live.log").exists(),
            "no file may be created through the junction"
        );
    }

    #[test]
    fn pinned_parent_rejects_a_rename_while_the_guard_is_held() {
        // The pin is what closes the validation-to-open swap window: while
        // the guard lives the parent directory cannot be renamed or removed,
        // so a swap synchronized between validation and open cannot land.
        // After the guard drops the rename succeeds, proving the guard was
        // the blocker.
        let guard = TestDir::new("pin-rename");
        let pinned = guard.dir.join("logs");
        std::fs::create_dir_all(&pinned).unwrap();
        let pinned_guard = open_pinned_parent(&pinned).unwrap();
        let renamed = guard.dir.join("logs-renamed");
        assert!(
            std::fs::rename(&pinned, &renamed).is_err(),
            "the pinned parent must refuse a rename while the guard is held"
        );
        drop(pinned_guard);
        std::fs::rename(&pinned, &renamed).unwrap();
        assert!(renamed.is_dir());
    }

    #[test]
    fn open_verified_file_blocks_a_parent_swap_inside_the_open() {
        // The synchronized scenario, end to end: the attacker attempts
        // the parent rename at the exact instant inside `open_verified_file`
        // — pin taken, open not yet started. The pin must refuse the swap,
        // the open must complete against the original path, and the write
        // must land in the verified location with nothing created anywhere
        // else.
        let guard = TestDir::new("open-swap");
        let logs = guard.dir.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let swapped = guard.dir.join("logs-swapped");
        let probe_logs = logs.clone();
        let probe_swapped = swapped.clone();
        super::test_hooks::arm_open_probe(move || {
            assert!(
                std::fs::rename(&probe_logs, &probe_swapped).is_err(),
                "the pinned parent must refuse a swap while the open is in flight"
            );
        });

        let target = logs.join("live.log");
        use std::io::Write;
        let mut file = open_verified_file(&target, true).expect("the open must complete against the original path");
        file.write_all(b"swap-probe record\n").unwrap();
        drop(file);

        // The swap never landed: the only entry under the test root is the
        // verified logs directory, the swapped name does not exist, and the
        // bytes reached the expected file.
        assert!(!swapped.exists(), "a swap must never be partially applied");
        assert_eq!(
            sibling_names(&guard.dir),
            vec!["logs"],
            "nothing may be created outside the verified parent"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"swap-probe record\n");
    }

    #[test]
    fn release_window_state_drops_the_box_and_tolerates_null() {
        // The box must actually be freed (a Drop probe observes it); a null
        // pointer is a no-op. The slot-clear half writes to a null window,
        // which SetWindowLongPtrW safely rejects.
        struct Probe(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state_ptr = Box::into_raw(Box::new(Probe(dropped.clone())));
        release_window_state(HWND::default(), state_ptr);
        assert!(dropped.load(Ordering::SeqCst), "the state box must be freed");
        release_window_state(HWND::default(), std::ptr::null_mut::<Probe>()); // must not panic
    }

    #[test]
    fn release_window_state_clears_the_slot_before_dropping_the_box() {
        // The canonical slot-first order, pinned structurally. A black-box
        // e2e teardown test cannot observe the slot-vs-box sequence: the
        // window is mid-destroy, so nothing reads the slot between the two
        // operations (the mutation — box drop before slot clear — passes
        // every e2e test unchanged). The box's own Drop can: a probe that
        // reads the slot at drop time discriminates the order, since
        // slot-first leaves it null and box-first still holds the pointer.
        struct Probe {
            hwnd: HWND,
            slot_was_cleared: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl Drop for Probe {
            fn drop(&mut self) {
                let slot = window_state::<Probe>(self.hwnd);
                self.slot_was_cleared.store(slot.is_null(), Ordering::SeqCst);
            }
        }
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{WINDOW_EX_STYLE, WS_OVERLAPPED};
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.expect("process module").into();
        // The predefined "STATIC" class needs no registration or wndproc.
        let hwnd = unsafe {
            crate::winapi::create_window(
                WINDOW_EX_STYLE(0),
                PCWSTR(wide("STATIC").as_ptr()),
                PCWSTR(wide("teardown-order probe").as_ptr()),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                instance,
                None,
            )
        }
        .expect("the probe window must be created");
        let cleared = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state_ptr = Box::into_raw(Box::new(Probe {
            hwnd,
            slot_was_cleared: cleared.clone(),
        }));
        set_window_state(hwnd, state_ptr);
        release_window_state(hwnd, state_ptr);
        assert!(
            cleared.load(Ordering::SeqCst),
            "the slot must be cleared before the state box is dropped"
        );
        unsafe {
            let _ = DestroyWindow(hwnd);
        };
    }

    #[test]
    fn registered_slot_set_read_and_clear_contract() {
        // The guarded contract: a stale teardown must never clear a newer
        // window's registration, and a clear always resets to the empty
        // value — `set` is the only path that can leave a non-empty value.
        let slot = Registered::new();
        assert_eq!(slot.read(), None, "a never-opened slot reads None");
        slot.set(7);
        assert_eq!(slot.read(), Some(7), "set installs the registration");
        slot.set(42); // a second open replaces the registration
        assert_eq!(slot.read(), Some(42));
        clear_registered(&slot, |v| *v == 8);
        assert_eq!(slot.read(), Some(42), "non-matching hwnd must not clear");
        clear_registered(&slot, |v| *v == 42);
        assert_eq!(slot.read(), Some(0), "matching hwnd clears to the empty value");
    }

    #[test]
    fn close_registered_destroys_the_registered_window() {
        // The extract-then-destroy contract: the handle is copied out and
        // the guard released before DestroyWindow (holding it would
        // deadlock the UI thread when the destruction messages relock the
        // slot). DestroyWindow is synchronous, so IsWindow reports the
        // handle gone once it returns.
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, WINDOW_EX_STYLE, WS_OVERLAPPED};
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.expect("process module").into();
        let hwnd = unsafe {
            crate::winapi::create_window(
                WINDOW_EX_STYLE(0),
                PCWSTR(wide("STATIC").as_ptr()),
                PCWSTR(wide("close-registered probe").as_ptr()),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                instance,
                None,
            )
        }
        .expect("the probe window must be created");
        assert!(unsafe { IsWindow(Some(hwnd)).as_bool() }, "the probe window is alive");
        let slot = Registered::new();
        slot.set(hwnd.0 as usize);
        close_registered(&slot, |v| (*v != 0).then_some(HWND(*v as *mut std::ffi::c_void)));
        assert!(
            !unsafe { IsWindow(Some(hwnd)).as_bool() },
            "close must destroy the registered window"
        );
    }
}
