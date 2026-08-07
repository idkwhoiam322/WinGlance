use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;

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
