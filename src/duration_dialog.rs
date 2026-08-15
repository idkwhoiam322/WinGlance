//! Modal dialog for entering a custom pill duration in seconds.
//!
//! A tiny raw-Win32 popup (label, edit control, OK/Cancel) matching the
//! repo's no-framework approach. The dialog is modal: the parent window is
//! disabled while a nested message loop runs on the UI thread, and the
//! selected value is returned when the loop exits. Invalid input keeps the
//! dialog open with a message box; valid input is clamped to the config
//! range [0.5, 60] seconds before it is converted to milliseconds.

use crate::winutil::{register_class_once, wide};
use log::warn;
use std::ffi::c_void;
use std::sync::OnceLock;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, DEFAULT_GUI_FONT, FillRect, GetStockObject, GetSysColorBrush, HDC, SetBkMode, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, ES_AUTOHSCROLL, GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowRect,
    HMENU, IDCANCEL, IDOK, IsDialogMessageW, MB_ICONWARNING, MB_OK, MSG, MessageBoxW, PostQuitMessage, SW_SHOW,
    SendMessageW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLOSE, WM_COMMAND, WM_CTLCOLORBTN, WM_CTLCOLORDLG, WM_CTLCOLORSTATIC, WM_ERASEBKGND, WM_GETTEXT, WM_NCCREATE,
    WM_SETFONT, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE,
};
use windows::core::PCWSTR;

/// Hardcoded input range, matching `config.normalize`/`set_duration`.
const MIN_SECONDS: f64 = 0.5;
const MAX_SECONDS: f64 = 60.0;

const CLASS_NAME: &str = "WinGlanceDurationDialog";
static CLASS_GUARD: OnceLock<()> = OnceLock::new();

struct DialogData {
    chosen: Option<u64>,
    done: bool,
    edit: HWND,
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

        let mut data = DialogData {
            chosen: None,
            done: false,
            edit: HWND::default(),
        };
        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR(wide(CLASS_NAME).as_ptr()),
            PCWSTR(wide("Custom duration").as_ptr()),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            outer_w,
            outer_h,
            parent,
            None,
            instance,
            Some((&mut data as *mut DialogData).cast()),
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                warn!("duration dialog: CreateWindowExW failed: {error}");
                return None;
            }
        };
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let child = |class: &str, text: &str, x: i32, y: i32, w: i32, h: i32, id: usize, style: WINDOW_STYLE| {
            let child = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(wide(class).as_ptr()),
                PCWSTR(wide(text).as_ptr()),
                style,
                (x as f32 * scale).round() as i32,
                (y as f32 * scale).round() as i32,
                (w as f32 * scale).round() as i32,
                (h as f32 * scale).round() as i32,
                hwnd,
                Some(&HMENU(id as *mut c_void)),
                instance,
                None,
            );
            if let Ok(child) = child {
                let _ = SendMessageW(child, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
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
        data.edit = edit;
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
            let _ = SetFocus(edit);
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
            if data.done {
                break;
            }
        }

        let _ = EnableWindow(parent, true);
        let _ = SetForegroundWindow(parent);
        let _ = DestroyWindow(hwnd);
        data.chosen
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
            let data = (*create).lpCreateParams as *mut DialogData;
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, data as isize);
        }
        return LRESULT(1);
    }
    let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DialogData;
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
                let copied = SendMessageW(
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
                    let _ = MessageBoxW(
                        hwnd,
                        PCWSTR(wide("Enter a duration between 0.5 and 60 seconds.").as_ptr()),
                        PCWSTR(wide("Custom duration").as_ptr()),
                        MB_OK | MB_ICONWARNING,
                    );
                    let _ = SetFocus(edit);
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
        WM_ERASEBKGND => {
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let _ = FillRect(HDC(wparam.0 as *mut c_void), &rc, GetSysColorBrush(COLOR_BTNFACE));
            LRESULT(1)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLORDLG => {
            if message == WM_CTLCOLORSTATIC {
                let _ = SetBkMode(HDC(wparam.0 as *mut c_void), TRANSPARENT);
            }
            LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_duration_seconds;

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
