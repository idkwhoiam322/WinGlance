//! Modal dialog for entering a custom pill duration in seconds.
//!
//! A tiny raw-Win32 popup (label, edit control, OK/Cancel) matching the
//! repo's no-framework approach. The dialog is modal: the parent window is
//! disabled while a nested message loop runs on the UI thread, and the
//! selected value is returned when the loop exits. Invalid input keeps the
//! dialog open with an inline error label; valid input is clamped to the config
//! range [0.5, 60] seconds before it is converted to milliseconds.

use crate::winapi::{create_window, send_message, set_focus};
use crate::winutil::{StateClaim, register_class_once, release_window_state, set_window_state, wide, window_state};
use log::warn;
use std::ffi::c_void;
use std::sync::OnceLock;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, DEFAULT_GUI_FONT, FillRect, GetStockObject, GetSysColorBrush, HDC, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CREATESTRUCTW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, ES_AUTOHSCROLL, GetClientRect, GetMessageW, GetWindowRect, HMENU, IDCANCEL, IDOK,
    IsDialogMessageW, MSG, PostQuitMessage, SW_SHOW, SetForegroundWindow, ShowWindow, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CTLCOLORBTN, WM_CTLCOLORDLG, WM_CTLCOLORSTATIC,
    WM_ERASEBKGND, WM_GETTEXT, WM_NCCREATE, WM_NCDESTROY, WM_SETFONT, WM_SETTEXT, WS_BORDER, WS_CAPTION, WS_CHILD,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::PCWSTR;

/// Hardcoded input range, matching `config.normalize`/`set_duration`.
const MIN_SECONDS: f64 = 0.5;
const MAX_SECONDS: f64 = 60.0;

const CLASS_NAME: &str = "WinGlanceDurationDialog";
static CLASS_GUARD: OnceLock<()> = OnceLock::new();

/// Set when this window's WM_NCCREATE claims the state box handed over in
/// `lpCreateParams`, so a failed CreateWindowExW can tell whether the box was
/// taken by the system (and freed in WM_NCDESTROY) or still belongs to the
/// caller. Reset before each open. See `winutil::StateClaim` for the shared
/// mechanics.
static DIALOG_STATE_CLAIMED: StateClaim = StateClaim::new();

struct DialogData {
    chosen: Option<u64>,
    done: bool,
    edit: HWND,
    /// Hidden error label, shown while the entered text does not parse.
    error_label: HWND,
}

/// Parses an entered duration (seconds) into milliseconds, clamped to
/// [0.5, 60] seconds. `None` on unparseable input (empty, non-numeric,
/// NaN/Infinity).
fn parse_duration_seconds(text: &str) -> Option<u64> {
    let value = text.trim().parse::<f64>().ok()?;
    if !value.is_finite() {
        return None;
    }
    Some((value.clamp(MIN_SECONDS, MAX_SECONDS) * 1000.0).round() as u64)
}

/// Shows the modal duration dialog centered over `parent`, pre-filled with
/// the current duration. Returns the chosen duration in milliseconds, or
/// `None` when the user cancels (or the dialog cannot be created).
pub fn show_duration_dialog(parent: HWND, current_ms: u64) -> Option<u64> {
    unsafe {
        let instance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(module) => module.into(),
            Err(error) => {
                warn!("duration dialog: GetModuleHandleW failed: {error}");
                return None;
            }
        };
        if let Err(error) = register_class_once(
            &CLASS_GUARD,
            instance,
            &wide(CLASS_NAME),
            Some(dialog_proc),
            || None,
            "the duration dialog",
        ) {
            warn!("duration dialog: class registration failed: {error}");
            return None;
        }

        // Geometry at 96 DPI, scaled to the parent's DPI like the rest of
        // the app (per-monitor DPI aware).
        let scale = GetDpiForWindow(parent).max(96) as f32 / 96.0;
        let (width, height) = ((284.0 * scale).round() as i32, (124.0 * scale).round() as i32);
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        let _ = AdjustWindowRectEx(&mut rc, WS_POPUP | WS_CAPTION | WS_SYSMENU, false, WS_EX_TOOLWINDOW);
        let (outer_w, outer_h) = (rc.right - rc.left, rc.bottom - rc.top);
        let mut parent_rect = RECT::default();
        let _ = GetWindowRect(parent, &mut parent_rect);
        let x = parent_rect.left + (parent_rect.right - parent_rect.left - outer_w) / 2;
        let y = parent_rect.top + (parent_rect.bottom - parent_rect.top - outer_h) / 2;

        // The state box is heap-allocated and handed to the window through
        // `lpCreateParams`: WM_NCCREATE stores it via `set_window_state`
        // (claiming it), and WM_NCDESTROY frees it through
        // `release_window_state` — the same shared guard every window uses.
        // The caller keeps the raw pointer to read `done`/`chosen` while the
        // window is alive, and reads the result *before* DestroyWindow frees
        // the box.
        let state_ptr = Box::into_raw(Box::new(DialogData {
            chosen: None,
            done: false,
            edit: HWND::default(),
            error_label: HWND::default(),
        }));
        DIALOG_STATE_CLAIMED.reset();
        let hwnd = match create_window(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR(wide(CLASS_NAME).as_ptr()),
            PCWSTR(wide("Custom duration").as_ptr()),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            outer_w,
            outer_h,
            Some(parent),
            None,
            instance,
            Some(state_ptr.cast()),
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                warn!("duration dialog: CreateWindowExW failed: {error}");
                // The state box is owned by the window from WM_NCCREATE
                // onward and freed in WM_NCDESTROY. WM_NCCREATE flips
                // DIALOG_STATE_CLAIMED when it takes the box; if it never
                // ran, the box still belongs to us and must be freed here.
                if let Some(state) = DIALOG_STATE_CLAIMED.take_unclaimed(state_ptr) {
                    drop(state);
                }
                return None;
            }
        };
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let child = |class: &str, text: &str, x: i32, y: i32, w: i32, h: i32, id: usize, style: WINDOW_STYLE| {
            let child = create_window(
                WINDOW_EX_STYLE(0),
                PCWSTR(wide(class).as_ptr()),
                PCWSTR(wide(text).as_ptr()),
                style,
                (x as f32 * scale).round() as i32,
                (y as f32 * scale).round() as i32,
                (w as f32 * scale).round() as i32,
                (h as f32 * scale).round() as i32,
                Some(hwnd),
                Some(HMENU(id as *mut c_void)),
                instance,
                None,
            );
            if let Ok(child) = child {
                let _ = send_message(child, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
                child
            } else {
                HWND::default()
            }
        };
        let _ = child(
            "STATIC",
            "Duration (seconds):",
            12,
            12,
            260,
            18,
            0,
            WS_CHILD | WS_VISIBLE,
        );
        let edit = child(
            "EDIT",
            &format!("{}", current_ms as f64 / 1000.0),
            12,
            34,
            260,
            22,
            100,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
        );
        (*state_ptr).edit = edit;
        let error_label = child(
            "STATIC", "", 12, 60, 260, 18, 101, // Hidden until the entered text fails to parse.
            WS_CHILD,
        );
        (*state_ptr).error_label = error_label;
        let _ = child(
            "BUTTON",
            "OK",
            154,
            82,
            58,
            24,
            IDOK.0 as usize,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
        );
        let _ = child(
            "BUTTON",
            "Cancel",
            214,
            82,
            58,
            24,
            IDCANCEL.0 as usize,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
        );

        let _ = EnableWindow(parent, false);
        let _ = ShowWindow(hwnd, SW_SHOW);
        if !edit.0.is_null() {
            let _ = set_focus(edit);
        }

        // Modal loop: IsDialogMessageW gives Enter (default button) and Esc
        // (IDCANCEL) for free. Other windows' messages (overlay ticks, etc.)
        // dispatch normally.
        let mut msg = MSG::default();
        loop {
            let result = GetMessageW(&mut msg, None, 0, 0);
            if result.0 <= 0 {
                // GetMessageW returns 0 on WM_QUIT. A quit posted while this
                // nested loop runs (e.g. tray Exit with the dialog open) would
                // otherwise be consumed here, and the main loop — which exits
                // only on WM_QUIT — would never see it, leaving the app as a
                // zombie process. Forward it and exit the loop.
                if result.0 == 0 {
                    PostQuitMessage(msg.wParam.0 as i32);
                }
                break;
            }
            // Enter/Esc set `done` inside IsDialogMessageW, every other
            // decision message inside DispatchMessageW. Checking after both
            // paths exits the loop without blocking on a next GetMessageW.
            let handled = IsDialogMessageW(hwnd, &msg).as_bool();
            if !handled {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
            if (*state_ptr).done {
                break;
            }
        }

        // Read the result while the box is still alive; DestroyWindow below
        // frees it in WM_NCDESTROY. The parent was disabled before the modal
        // loop (EnableWindow(parent, false) above); re-enable it before
        // returning or the settings window stays disabled until restart.
        let chosen = (*state_ptr).chosen;
        let _ = EnableWindow(parent, true);
        let _ = SetForegroundWindow(parent);
        let _ = DestroyWindow(hwnd);
        chosen
    }
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A contained panic answers "not handled" (0) so the dialog
    // manager's own default handling continues instead of unwinding.
    crate::winutil::catch_callback_panic("the duration dialog procedure", || unsafe {
        dialog_proc_body(hwnd, message, wparam, lparam)
    })
    .unwrap_or(LRESULT(0))
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn dialog_proc_body(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            let state = (*create).lpCreateParams as *mut DialogData;
            // Null-guard like the other windows: a null param must not flip
            // DIALOG_STATE_CLAIMED while the slot stays empty — WM_NCDESTROY
            // would free nothing and take_unclaimed would refuse to return
            // the box, leaking it on a failed create.
            if !state.is_null() {
                set_window_state(hwnd, state);
                DIALOG_STATE_CLAIMED.claim();
            }
        }
        return LRESULT(1);
    }
    let data_ptr = window_state::<DialogData>(hwnd);
    match message {
        WM_COMMAND => {
            if data_ptr.is_null() {
                return LRESULT(0);
            }
            let id = (wparam.0 & 0xFFFF) as u32;
            if id == IDOK.0 as u32 {
                // OK: read the edit text, parse it, and either commit or
                // keep the dialog open with an error box.
                let edit = (*data_ptr).edit;
                let mut buffer = [0u16; 64];
                let copied = send_message(
                    edit,
                    WM_GETTEXT,
                    WPARAM(buffer.len()),
                    LPARAM(buffer.as_mut_ptr() as isize),
                )
                .0 as usize;
                let text = String::from_utf16_lossy(&buffer[..copied.min(buffer.len())]);
                if let Some(ms) = parse_duration_seconds(&text) {
                    (*data_ptr).chosen = Some(ms);
                    (*data_ptr).done = true;
                } else {
                    let error_text = wide("Enter a duration between 0.5 and 60 seconds.");
                    let _ = send_message(
                        (*data_ptr).error_label,
                        WM_SETTEXT,
                        WPARAM(0),
                        LPARAM(error_text.as_ptr() as isize),
                    );
                    let _ = ShowWindow((*data_ptr).error_label, SW_SHOW);
                    let _ = set_focus(edit);
                }
            } else if id == IDCANCEL.0 as u32 {
                (*data_ptr).done = true;
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if !data_ptr.is_null() {
                (*data_ptr).done = true;
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // Free the heap-allocated DialogData stashed at WM_NCCREATE via
            // the shared helper — slot clear first, box second, the canonical
            // order every window applies. DialogData owns no GDI objects, so
            // nothing else tears down here. Every close path (OK/Cancel/
            // close button) goes through DestroyWindow; without this the box
            // leaked on each open.
            release_window_state(hwnd, data_ptr);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_ERASEBKGND => {
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let _ = FillRect(HDC(wparam.0 as *mut c_void), &rc, GetSysColorBrush(COLOR_BTNFACE));
            LRESULT(1)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLORDLG => {
            if message == WM_CTLCOLORSTATIC {
                let hdc = HDC(wparam.0 as *mut c_void);
                let _ = SetBkMode(hdc, TRANSPARENT);
                // The error label renders in red (COLORREF is 0x00BBGGRR, so
                // 0x000000FF is pure red) so invalid input reads as an error,
                // not as a caption.
                if !data_ptr.is_null() && HWND(lparam.0 as *mut c_void) == (*data_ptr).error_label {
                    let _ = SetTextColor(hdc, COLORREF(0x000000FF));
                }
            }
            LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CLASS_GUARD, CLASS_NAME, DIALOG_STATE_CLAIMED, DialogData, dialog_proc, parse_duration_seconds,
        show_duration_dialog,
    };
    use crate::winapi::{create_window, post_message, send_message};
    use crate::winutil::{register_class_once, wide, window_state};
    use std::ffi::c_void;
    use std::sync::{Mutex, OnceLock, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, DestroyWindow, FindWindowW, GetDlgItem, IDOK, IsWindowVisible, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_CLOSE, WM_COMMAND, WM_NCDESTROY, WM_SETTEXT, WS_CAPTION, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        WS_SYSMENU,
    };
    use windows::core::PCWSTR;

    /// Trivial default proc for the test parent window: the dialog's
    /// `EnableWindow(parent, false)` and `SetForegroundWindow(parent)` only
    /// need the parent to exist; `DefWindowProcW` is a Rust fn (not
    /// `extern "system"`), so it cannot be the class proc directly.
    unsafe extern "system" fn parent_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    // The two dialog tests below both create real windows of the dialog class
    // and both touch DIALOG_STATE_CLAIMED, so they must not interleave — a
    // concurrent FindWindowW (in the re-enable test) could otherwise latch
    // onto the other test's same-class window. Serialize like the overlay
    // wndproc harness does.
    static DIALOG_TEST_LOCK: Mutex<()> = Mutex::new(());

    // Test-only parent window class, shared by the tests that run the real
    // modal dialog: like the dialog class itself, it is registered once per
    // process and reused (a per-test registration would hit
    // ERROR_CLASS_ALREADY_EXISTS on the runner-up).
    static PARENT_GUARD: OnceLock<()> = OnceLock::new();
    const PARENT_CLASS: &str = "WinGlanceDialogTestParent";

    #[test]
    fn dialog_state_box_installs_through_nccreate_and_frees_through_ncdestroy() {
        let _serialize = DIALOG_TEST_LOCK.lock().unwrap();
        // The DialogData lifecycle runs through the shared state-slot guard:
        // WM_NCCREATE installs the heap box via set_window_state, WM_NCDESTROY
        // frees it and clears the slot via release_window_state. Driving the
        // real class and wndproc end to end pins that wiring — the slot holds
        // the box after create and is empty after destroy, so the dialog can
        // never drift back to a hand-rolled SetWindowLongPtrW/GetWindowLongPtrW
        // pair. (The box being *freed* — not just the slot cleared — is pinned
        // by the winutil probe test; this e2e path cannot observe the free.)
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.expect("process module").into();
        register_class_once(
            &CLASS_GUARD,
            instance,
            &wide(CLASS_NAME),
            Some(dialog_proc),
            || None,
            "the duration dialog",
        )
        .expect("the dialog class registers");
        DIALOG_STATE_CLAIMED.reset();
        let state_ptr = Box::into_raw(Box::new(DialogData {
            chosen: None,
            done: false,
            edit: HWND::default(),
            error_label: HWND::default(),
        }));
        let hwnd = unsafe {
            create_window(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                PCWSTR(wide(CLASS_NAME).as_ptr()),
                PCWSTR(wide("test").as_ptr()),
                WS_POPUP | WS_CAPTION | WS_SYSMENU,
                0,
                0,
                100,
                100,
                None,
                None,
                instance,
                Some(state_ptr.cast()),
            )
        }
        .expect("the test dialog window must be created");
        assert_eq!(
            window_state::<DialogData>(hwnd),
            state_ptr,
            "WM_NCCREATE must install the box through the shared guard"
        );
        // Teardown is observed while the window is still alive: a plain
        // DestroyWindow-then-read cannot discriminate (GetWindowLongPtrW on
        // a destroyed handle reads 0 regardless of whether WM_NCDESTROY
        // cleared the slot), so WM_NCDESTROY is driven directly through the
        // real wndproc. The box is freed here — the second release during
        // the real DestroyWindow below is a no-op on the cleared slot.
        unsafe {
            let _ = send_message(hwnd, WM_NCDESTROY, WPARAM(0), LPARAM(0));
        }
        assert!(
            window_state::<DialogData>(hwnd).is_null(),
            "WM_NCDESTROY must clear the slot through the shared guard"
        );
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn closing_the_dialog_re_enables_the_parent_window() {
        // Regression pin for the EnableWindow(parent, true) restore on the
        // dialog's exit path. The dialog runs a real modal loop, so it must
        // live on its own thread: the test finds the dialog window by class
        // name (the serialization lock guarantees no other same-class window
        // exists), closes it via WM_CLOSE through the real wndproc, and
        // asserts the parent — disabled when the dialog opened — is enabled
        // again. Without the restore line the settings window would stay
        // permanently disabled after the dialog closes.
        let _serialize = DIALOG_TEST_LOCK.lock().unwrap();
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.expect("process module").into();
        register_class_once(
            &CLASS_GUARD,
            instance,
            &wide(CLASS_NAME),
            Some(dialog_proc),
            || None,
            "the duration dialog",
        )
        .expect("the dialog class registers");
        register_class_once(
            &PARENT_GUARD,
            instance,
            &wide(PARENT_CLASS),
            Some(parent_proc),
            || None,
            "the dialog-test parent",
        )
        .expect("the parent class registers");
        // The parent window must be created on the SAME thread that runs the
        // dialog: the dialog's `EnableWindow(parent, false)` sends a
        // WM_CANCELMODE synchronously to the owner of `parent`, and a
        // cross-thread SendMessage blocks until that thread pumps — the test
        // thread never pumps, so the worker would stall before the modal loop.
        let (parent_tx, parent_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let instance_raw = instance.0 as usize;
        let worker = thread::spawn(move || unsafe {
            let instance = HINSTANCE(instance_raw as *mut c_void);
            let parent = create_window(
                WINDOW_EX_STYLE(0),
                PCWSTR(wide(PARENT_CLASS).as_ptr()),
                PCWSTR(wide("dialog-test parent").as_ptr()),
                WINDOW_STYLE(0),
                0,
                0,
                200,
                120,
                None,
                None,
                instance,
                None,
            )
            .expect("the parent window must be created");
            let _ = parent_tx.send(parent.0 as usize);
            let chosen = show_duration_dialog(parent, 3000);
            // The parent is enabled/disabled on this thread, so observe its
            // state here while the handle is still valid.
            let parent_enabled = IsWindowEnabled(parent).as_bool();
            let _ = result_tx.send((chosen, parent_enabled));
            let _ = DestroyWindow(parent);
        });
        // Wait for the parent window to exist, then for the dialog window to
        // appear, then close it. Both are top-level (WS_POPUP), so
        // FindWindowW by class reaches them.
        let _parent = parent_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the parent window must be created on the dialog thread");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut dialog = HWND::default();
        while Instant::now() < deadline {
            if let Ok(found) = unsafe { FindWindowW(PCWSTR(wide(CLASS_NAME).as_ptr()), PCWSTR::null()) }
                && !found.0.is_null()
            {
                dialog = found;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!dialog.0.is_null(), "the dialog window must appear");
        let _ = unsafe { post_message(dialog, WM_CLOSE, WPARAM(0), LPARAM(0)) };
        let (chosen, parent_enabled) = result_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the dialog thread must return after WM_CLOSE");
        let _ = worker.join();
        assert!(chosen.is_none(), "WM_CLOSE cancels the dialog");
        assert!(
            parent_enabled,
            "the parent window must be re-enabled after the dialog closes"
        );
    }

    #[test]
    fn invalid_input_shows_the_inline_error_and_keeps_the_dialog_open() {
        // Regression pin for the inline-error replacement of the old modal
        // message box: an unparseable entry must show the dialog's own error
        // label and keep the dialog open, and a corrected entry must then
        // commit normally. Driven end to end like the re-enable test above:
        // the dialog (and its parent) live on a worker thread so the modal
        // loop is genuinely pumping.
        let _serialize = DIALOG_TEST_LOCK.lock().unwrap();
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.expect("process module").into();
        register_class_once(
            &CLASS_GUARD,
            instance,
            &wide(CLASS_NAME),
            Some(dialog_proc),
            || None,
            "the duration dialog",
        )
        .expect("the dialog class registers");
        register_class_once(
            &PARENT_GUARD,
            instance,
            &wide(PARENT_CLASS),
            Some(parent_proc),
            || None,
            "the dialog-test parent",
        )
        .expect("the parent class registers");
        let (parent_tx, parent_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let instance_raw = instance.0 as usize;
        let worker = thread::spawn(move || unsafe {
            let instance = HINSTANCE(instance_raw as *mut c_void);
            let parent = create_window(
                WINDOW_EX_STYLE(0),
                PCWSTR(wide(PARENT_CLASS).as_ptr()),
                PCWSTR(wide("dialog-test parent").as_ptr()),
                WINDOW_STYLE(0),
                0,
                0,
                200,
                120,
                None,
                None,
                instance,
                None,
            )
            .expect("the parent window must be created");
            let _ = parent_tx.send(parent.0 as usize);
            let chosen = show_duration_dialog(parent, 3000);
            let parent_enabled = IsWindowEnabled(parent).as_bool();
            let _ = result_tx.send((chosen, parent_enabled));
            let _ = DestroyWindow(parent);
        });
        let _parent = parent_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the parent window must be created on the dialog thread");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut dialog = HWND::default();
        while Instant::now() < deadline {
            if let Ok(found) = unsafe { FindWindowW(PCWSTR(wide(CLASS_NAME).as_ptr()), PCWSTR::null()) }
                && !found.0.is_null()
            {
                dialog = found;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!dialog.0.is_null(), "the dialog window must appear");
        // The window becomes findable the moment CreateWindowExW returns,
        // while its child controls are still being created on the dialog
        // thread — poll until both exist so the test never races the control
        // creation.
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut edit, mut error_label) = (HWND::default(), HWND::default());
        while Instant::now() < deadline {
            if let Ok(found_edit) = unsafe { GetDlgItem(Some(dialog), 100) }
                && let Ok(found_label) = unsafe { GetDlgItem(Some(dialog), 101) }
                && !found_edit.0.is_null()
                && !found_label.0.is_null()
            {
                edit = found_edit;
                error_label = found_label;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !edit.0.is_null() && !error_label.0.is_null(),
            "the edit and error label must exist"
        );
        let set_text = |text: &str, label: HWND| unsafe {
            let _ = crate::winapi::send_message(label, WM_SETTEXT, WPARAM(0), LPARAM(wide(text).as_ptr() as isize));
        };
        // Invalid input: the label appears and the dialog stays open.
        set_text("abc", edit);
        let _ = unsafe { crate::winapi::send_message(dialog, WM_COMMAND, WPARAM(IDOK.0 as usize), LPARAM(0)) };
        assert!(
            unsafe { IsWindowVisible(error_label).as_bool() },
            "the inline error label must be shown for invalid input"
        );
        assert!(
            !unsafe { FindWindowW(PCWSTR(wide(CLASS_NAME).as_ptr()), PCWSTR::null()) }
                .expect("FindWindowW never fails")
                .0
                .is_null(),
            "the dialog must stay open after invalid input"
        );
        // Corrected input commits and closes the dialog like the old path.
        set_text("7", edit);
        let _ = unsafe { crate::winapi::send_message(dialog, WM_COMMAND, WPARAM(IDOK.0 as usize), LPARAM(0)) };
        thread::sleep(Duration::from_millis(50));
        let (chosen, parent_enabled) = result_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the dialog thread must return after the corrected entry");
        let _ = worker.join();
        assert_eq!(chosen, Some(7000), "the corrected duration must commit");
        assert!(
            parent_enabled,
            "the parent window must be re-enabled after the dialog closes"
        );
    }

    #[test]
    fn parses_valid_durations_into_milliseconds() {
        assert_eq!(parse_duration_seconds("3.5"), Some(3500));
        assert_eq!(parse_duration_seconds("7"), Some(7000));
        assert_eq!(parse_duration_seconds("0.5"), Some(500));
        assert_eq!(parse_duration_seconds("60"), Some(60_000));
        assert_eq!(parse_duration_seconds("1.234"), Some(1234));
        assert_eq!(parse_duration_seconds(" 4 "), Some(4000));
    }

    #[test]
    fn clamps_out_of_range_input_to_the_hardcoded_range() {
        assert_eq!(parse_duration_seconds("0.1"), Some(500));
        assert_eq!(parse_duration_seconds("120"), Some(60_000));
        assert_eq!(parse_duration_seconds("-2"), Some(500));
    }

    #[test]
    fn rejects_unparseable_input() {
        assert_eq!(parse_duration_seconds("abc"), None);
        assert_eq!(parse_duration_seconds(""), None);
        assert_eq!(parse_duration_seconds("  "), None);
        assert_eq!(parse_duration_seconds("nan"), None);
        assert_eq!(parse_duration_seconds("inf"), None);
    }
}
