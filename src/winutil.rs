use log::warn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBRUSH, HGDIOBJ};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, HCURSOR, IDC_ARROW, LoadCursorW, RegisterClassExW,
    SetWindowLongPtrW, WNDCLASS_STYLES, WNDCLASSEXW, WNDPROC,
};
use windows::core::PCWSTR;

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
            let _ = unsafe { DeleteObject(HGDIOBJ(brush.0)) };
        }
        warn!("RegisterClassExW failed for {description}");
        return Err(windows::core::Error::from_win32());
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
pub(crate) fn clear_window_state(hwnd: HWND) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }
}

/// Reads a window's per-instance state pointer from GWLP_USERDATA. Returns
/// null when the slot is empty or was cleared.
pub(crate) fn window_state<T>(hwnd: HWND) -> *mut T {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut T }
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

/// Closes a window whose handle is registered in a `OnceLock<Mutex<T>>`
/// slot, e.g. the positioner or the process picker. `extract` pulls the
/// window handle out of the slot's value (returning `None` when no window is
/// open). The handle is copied out and the guard released before
/// `DestroyWindow`: the destruction messages (WM_DESTROY/WM_NCDESTROY) lock
/// the same slot again, and holding the mutex across `DestroyWindow` would
/// deadlock the UI thread.
pub(crate) fn close_registered<T>(slot: &OnceLock<Mutex<T>>, extract: impl FnOnce(&T) -> Option<HWND>) {
    let Some(m) = slot.get() else {
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

/// UTF-16-encodes `value` with a trailing NUL terminator suitable for the
/// `PCWSTR` Win32 APIs. Single source of truth — used by `overlay`,
/// `main_window`, `positioner`, `autostart`, `process_picker`, and `main`.
pub(crate) fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
