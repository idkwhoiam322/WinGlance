use log::warn;
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBRUSH, HGDIOBJ};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, IDC_ARROW, LoadCursorW, RegisterClassExW, WNDCLASS_STYLES, WNDCLASSEXW, WNDPROC,
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
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap();
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
