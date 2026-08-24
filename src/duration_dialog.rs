//! Modal dialog for entering a custom pill duration in seconds.
//!
//! A tiny raw-Win32 popup (label, edit control, OK/Cancel) matching the
//! repo's no-framework approach. The dialog is modal: the parent window is
//! disabled while a nested message loop runs on the UI thread, and the
//! selected value is returned when the loop exits. Input that does not parse
//! or falls outside the config range [0.5, 60] seconds keeps the dialog open
//! with an inline error label; valid input is converted to milliseconds.

use crate::winapi::{create_window, post_message, send_message, set_focus};
use crate::winutil::{StateClaim, register_class_once, release_window_state, set_window_state, wide, window_state};
use log::warn;
use std::ffi::c_void;
use std::sync::OnceLock;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, DEFAULT_GUI_FONT, FillRect, GetStockObject, GetSysColorBrush, HDC, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
/// Test seam constant: the creation parent that makes a window message-only
/// (see TEST_MESSAGE_ONLY_DIALOG). Imported only for the test build, where
/// the seam is compiled in.
#[cfg(test)]
use windows::Win32::UI::WindowsAndMessaging::HWND_MESSAGE;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CREATESTRUCTW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, EN_CHANGE, ES_AUTOHSCROLL, GetClientRect, GetDlgItem, GetMessageW, GetWindowRect, HMENU,
    IDCANCEL, IDOK, IsDialogMessageW, MSG, PostQuitMessage, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER,
    SetForegroundWindow, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND,
    WM_CTLCOLORBTN, WM_CTLCOLORDLG, WM_CTLCOLORSTATIC, WM_DPICHANGED, WM_ERASEBKGND, WM_GETTEXT, WM_NCCREATE,
    WM_NCDESTROY, WM_SETFONT, WM_SETTEXT, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::PCWSTR;

/// Hardcoded input range — derived from the config clamp constants so the
/// dialog can never drift from `normalize()`; a cross-check test pins
/// the equality.
const MIN_SECONDS: f64 = crate::config::Config::DURATION_MIN_MS as f64 / 1000.0;
const MAX_SECONDS: f64 = crate::config::Config::DURATION_MAX_MS as f64 / 1000.0;

const CLASS_NAME: &str = "WinGlanceDurationDialog";
static CLASS_GUARD: OnceLock<()> = OnceLock::new();

/// The dialog's control layout in 96-DPI logical units: (id, x, y, w, h) per
/// child, in creation order. The creation closure and the WM_DPICHANGED
/// re-layout both consume this one table, so the geometry has a single
/// source of truth. Ids: prompt label 103, edit 100 (the gate tests
/// latch it), error label 101, then the standard IDOK/IDCANCEL.
const CONTROLS: [(usize, i32, i32, i32, i32); 5] = [
    (103, 12, 12, 260, 18),
    (100, 12, 34, 260, 22),
    (101, 12, 60, 260, 18),
    (1, 154, 82, 58, 24), // IDOK
    (2, 214, 82, 58, 24), // IDCANCEL
];

/// Set when this window's WM_NCCREATE claims the state box handed over in
/// `lpCreateParams`, so a failed CreateWindowExW can tell whether the box was
/// taken by the system (and freed in WM_NCDESTROY) or still belongs to the
/// caller. Reset before each open. See `winutil::StateClaim` for the shared
/// mechanics.
static DIALOG_STATE_CLAIMED: StateClaim = StateClaim::new();

/// The live custom-duration dialog's hwnd, or 0 while none is open. Quit
/// paths close an open dialog before destroying the main window so the
/// dialog's modal loop drains over a live owner.
static OPEN_DIALOG_HWND: AtomicUsize = AtomicUsize::new(0);

/// Best-effort close of an open custom-duration dialog (its cancel path):
/// a Quit that destroys the owner underneath the dialog's modal loop would
/// leave the loop running over a dead parent — close it first so the loop
/// ends cleanly.
pub(crate) fn close_if_open() {
    let raw = OPEN_DIALOG_HWND.load(Ordering::SeqCst);
    if raw != 0 {
        let _ = unsafe { post_message(HWND(raw as *mut c_void), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
}

/// Test seam for the duration-dialog gate tests: while armed,
/// `show_duration_dialog` creates the dialog against `HWND_MESSAGE` instead
/// of the real parent. A message-only window can never be displayed and is
/// invisible to FindWindowW/EnumWindows, so the gate's test phase cannot
/// flash the dialog no matter how it runs. Production never arms the flag;
/// the real `parent` keeps driving `EnableWindow` and centering.
#[cfg(test)]
static TEST_MESSAGE_ONLY_DIALOG: AtomicBool = AtomicBool::new(false);

/// Test seam: the handle of the message-only dialog, latched at WM_NCCREATE
/// while TEST_MESSAGE_ONLY_DIALOG is armed. EnumThreadWindows does not
/// enumerate message-only windows, so the gate tests cannot discover the
/// dialog through the OS — they poll this latch instead. The handle is only
/// acceptable while `TEST_DIALOG_EPOCH` carries the polling test's
/// generation (see `find_dialog_on_thread`), so a stale handle from a
/// previous test can never be mistaken for this test's dialog.
#[cfg(test)]
static TEST_DIALOG_HWND: AtomicUsize = AtomicUsize::new(0);

/// Test seam: handles of the dialog's two id-interesting children (the
/// duration edit, id 100, and the hidden error label, id 101), latched
/// when the dialog thread creates them. Together with TEST_DIALOG_HWND
/// they let a gate test wait for the whole dialog — window and controls —
/// in one poll instead of racing the dialog thread's control creation
/// with a second deadline. Same generation rule as the window latch.
#[cfg(test)]
static TEST_DIALOG_EDIT: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static TEST_DIALOG_LABEL: AtomicUsize = AtomicUsize::new(0);

/// Test seam generation counter: bumped by each gate test before it opens
/// its dialog. The poll accepts a handle only while the generation matches
/// — the latched values carry no generation themselves, so the tests also
/// clear them after the bump; the epoch alone cannot tell a stale value
/// from a fresh one, and the clear alone would race the dialog's own
/// WM_NCCREATE store.
#[cfg(test)]
static TEST_DIALOG_EPOCH: AtomicU64 = AtomicU64::new(0);

struct DialogData {
    chosen: Option<u64>,
    done: bool,
    /// The dialog's current DPI scale (96-DPI = 1.0), updated by
    /// WM_DPICHANGED so a drag across monitors can re-layout the controls
    /// from the shared table.
    scale: f32,
    edit: HWND,
    /// Hidden error label, shown while the entered text does not parse or
    /// falls outside the range.
    error_label: HWND,
}

/// Parses an entered duration (seconds) into milliseconds. `None` when the
/// input does not parse (empty, non-numeric, NaN/Infinity) or falls outside
/// the hardcoded [0.5, 60] seconds range — the dialog reports those instead
/// of silently clamping, so the committed value always equals what was typed.
fn parse_duration_seconds(text: &str) -> Option<u64> {
    let value = text.trim().parse::<f64>().ok()?;
    if !value.is_finite() || !(MIN_SECONDS..=MAX_SECONDS).contains(&value) {
        return None;
    }
    Some((value * 1000.0).round() as u64)
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
            scale: GetDpiForWindow(parent).max(96) as f32 / 96.0,
            edit: HWND::default(),
            error_label: HWND::default(),
        }));
        DIALOG_STATE_CLAIMED.reset();
        // Test seam: the gate tests arm TEST_MESSAGE_ONLY_DIALOG so this
        // window is created against HWND_MESSAGE — such a window can never
        // be displayed and is invisible to FindWindowW/EnumWindows, so the
        // gate's test phase cannot flash the dialog regardless of the
        // window station or what else is running. Production never arms the
        // flag; the real `parent` still drives EnableWindow and centering.
        #[cfg(test)]
        let creation_parent = if TEST_MESSAGE_ONLY_DIALOG.load(Ordering::SeqCst) {
            HWND_MESSAGE
        } else {
            parent
        };
        #[cfg(not(test))]
        let creation_parent = parent;
        let hwnd = match create_window(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR(wide(CLASS_NAME).as_ptr()),
            PCWSTR(wide("Custom duration").as_ptr()),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            outer_w,
            outer_h,
            Some(creation_parent),
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
        OPEN_DIALOG_HWND.store(hwnd.0 as usize, Ordering::SeqCst);
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
                // Test seam: latch the duration edit (id 100) and the error
                // label (id 101) the moment they materialize, so a gate test
                // can wait for the full dialog in one poll (see
                // `find_dialog_on_thread`).
                #[cfg(test)]
                if TEST_MESSAGE_ONLY_DIALOG.load(Ordering::SeqCst) {
                    match id {
                        100 => TEST_DIALOG_EDIT.store(child.0 as usize, Ordering::SeqCst),
                        101 => TEST_DIALOG_LABEL.store(child.0 as usize, Ordering::SeqCst),
                        _ => {}
                    }
                }
                let _ = send_message(child, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
                child
            } else {
                HWND::default()
            }
        };
        let label = CONTROLS[0];
        let _ = child(
            "STATIC",
            "Duration (seconds):",
            label.1,
            label.2,
            label.3,
            label.4,
            label.0,
            WS_CHILD | WS_VISIBLE,
        );
        let edit_ctl = CONTROLS[1];
        let edit = child(
            "EDIT",
            &format!("{}", current_ms as f64 / 1000.0),
            edit_ctl.1,
            edit_ctl.2,
            edit_ctl.3,
            edit_ctl.4,
            edit_ctl.0,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
        );
        (*state_ptr).edit = edit;
        let err = CONTROLS[2];
        let error_label = child("STATIC", "", err.1, err.2, err.3, err.4, err.0, WS_CHILD);
        (*state_ptr).error_label = error_label;
        let ok = CONTROLS[3];
        let _ = child(
            "BUTTON",
            "OK",
            ok.1,
            ok.2,
            ok.3,
            ok.4,
            ok.0,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
        );
        let cancel = CONTROLS[4];
        let _ = child(
            "BUTTON",
            "Cancel",
            cancel.1,
            cancel.2,
            cancel.3,
            cancel.4,
            cancel.0,
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

/// Repositions and resizes every child from the shared CONTROLS table at
/// `scale`: the WM_DPICHANGED handler calls this after the frame has
/// moved, so a dialog dragged across monitors keeps its controls laid out.
fn relayout_controls(hwnd: HWND, scale: f32) {
    for &(id, x, y, w, h) in &CONTROLS {
        let child = unsafe { GetDlgItem(Some(hwnd), id as i32) };
        if let Ok(child) = child {
            let _ = unsafe {
                crate::winapi::set_window_pos(
                    child,
                    HWND::default(),
                    (x as f32 * scale).round() as i32,
                    (y as f32 * scale).round() as i32,
                    (w as f32 * scale).round() as i32,
                    (h as f32 * scale).round() as i32,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn dialog_proc_body(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        // Test seam: latch the dialog handle the moment the window
        // materializes. The gate tests poll this because their message-only
        // dialog is invisible to FindWindowW/EnumWindows and to
        // EnumThreadWindows (compiled into test builds only).
        #[cfg(test)]
        if TEST_MESSAGE_ONLY_DIALOG.load(Ordering::SeqCst) {
            TEST_DIALOG_HWND.store(hwnd.0 as usize, Ordering::SeqCst);
        }
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
            let notification = ((wparam.0 >> 16) & 0xFFFF) as u32;
            if id == 100 && notification == EN_CHANGE {
                // Editing hides the error again: the label describes the last
                // OK click, not the text currently being typed.
                let _ = ShowWindow((*data_ptr).error_label, SW_HIDE);
            }
            if id == IDOK.0 as u32 {
                // OK: read the edit text, parse it, and either commit or
                // keep the dialog open with the inline error label.
                let edit = (*data_ptr).edit;
                let mut buffer = [0u16; 64];
                let copied = send_message(
                    edit,
                    WM_GETTEXT,
                    WPARAM(buffer.len()),
                    LPARAM(buffer.as_mut_ptr() as isize),
                )
                .0 as usize;
                // A paste longer than the read buffer is rejected rather
                // than silently parsed from its truncated prefix: a 63-char
                // numeric paste could truncate to exactly "60" and commit an
                // out-of-range duration the user never typed. The empty
                // string fails the parse below, which shows the inline
                // error.
                let text = if copied >= buffer.len() - 1 {
                    String::new()
                } else {
                    String::from_utf16_lossy(&buffer[..copied.min(buffer.len())])
                };
                if let Some(ms) = parse_duration_seconds(&text) {
                    (*data_ptr).chosen = Some(ms);
                    (*data_ptr).done = true;
                } else {
                    let error_text = wide(&format!(
                        "Enter a duration between {} and {} seconds.",
                        MIN_SECONDS, MAX_SECONDS
                    ));
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
        WM_DPICHANGED => {
            // Re-layout from the shared table at the new scale: the
            // suggested rect keeps the frame right, the relayout keeps the
            // controls on it. A modal dialog dragged across monitors is
            // rare, but a stretched control row is exactly the kind of
            // breakage that reads as a bug when it happens.
            if !data_ptr.is_null() {
                let data = &mut *data_ptr;
                data.scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
                relayout_controls(hwnd, data.scale);
            }
            let suggested = lparam.0 as *const windows::Win32::Foundation::RECT;
            if !suggested.is_null() {
                let rect = unsafe { *suggested };
                let _ = crate::winapi::set_window_pos(
                    hwnd,
                    HWND::default(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
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
            OPEN_DIALOG_HWND.store(0, Ordering::SeqCst);
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
        CLASS_GUARD, CLASS_NAME, DIALOG_STATE_CLAIMED, DialogData, MAX_SECONDS, MIN_SECONDS, TEST_DIALOG_EDIT,
        TEST_DIALOG_EPOCH, TEST_DIALOG_HWND, TEST_DIALOG_LABEL, TEST_MESSAGE_ONLY_DIALOG, dialog_proc,
        parse_duration_seconds, show_duration_dialog,
    };
    use crate::winapi::{create_window, post_message, send_message};
    use crate::winutil::{register_class_once, wide, window_state};
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;

    #[test]
    fn dialog_range_matches_the_config_clamp() {
        // Drift guard: the dialog's parse range is DERIVED from the
        // config clamp constants; these asserts fail loudly if anyone
        // reintroduces a second hardcoded range on either side.
        assert_eq!(MIN_SECONDS * 1000.0, crate::config::Config::DURATION_MIN_MS as f64);
        assert_eq!(MAX_SECONDS * 1000.0, crate::config::Config::DURATION_MAX_MS as f64);
        // The boundaries themselves parse; one step past each does not.
        assert_eq!(parse_duration_seconds("0.5"), Some(500));
        assert_eq!(parse_duration_seconds("60"), Some(60_000));
        assert_eq!(parse_duration_seconds("0.4"), None);
        assert_eq!(parse_duration_seconds("60.1"), None);
    }
    use std::sync::{Mutex, OnceLock, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, DestroyWindow, GWL_STYLE, GetWindowLongW, HWND_MESSAGE, IDOK, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_CLOSE, WM_COMMAND, WM_NCDESTROY, WM_SETTEXT, WS_CAPTION, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        WS_SYSMENU, WS_VISIBLE,
    };
    use windows::core::PCWSTR;

    /// Trivial default proc for the test parent window: the dialog's
    /// `EnableWindow(parent, false)` and `SetForegroundWindow(parent)` only
    /// need the parent to exist; `DefWindowProcW` is a Rust fn (not
    /// `extern "system"`), so it cannot be the class proc directly.
    unsafe extern "system" fn parent_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    // The three dialog tests below all create windows of the dialog class
    // and all touch DIALOG_STATE_CLAIMED, so they must not interleave — the
    // dialog harness would otherwise race the class registration and the
    // state claim. Serialize like the overlay wndproc harness does. The
    // lock is taken poison-tolerant: a panicking test poisons the mutex,
    // but its contents stay valid, so the next test must still run —
    // otherwise one failure cascades into the whole family.
    static DIALOG_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Bounded on-drop cleanup for a dialog-test worker thread. If a test
    /// panics mid-flight, its worker is left blocked inside the modal loop
    /// forever — nothing will ever close its dialog — holding the armed seam
    /// flag and the DIALOG_STATE_CLAIMED slot the next dialog test needs.
    /// The guard closes this generation's latched dialog (if one exists) and
    /// waits up to 5s for the worker to unwind; on the normal path the test
    /// already joined the worker, so the guard is a no-op.
    struct DialogWorkerGuard {
        worker: Option<std::thread::JoinHandle<()>>,
        epoch: u64,
    }

    impl DialogWorkerGuard {
        fn new(worker: std::thread::JoinHandle<()>, epoch: u64) -> Self {
            Self {
                worker: Some(worker),
                epoch,
            }
        }

        /// Normal-path completion: the test verified the result channel, so
        /// join the worker now. Consumes the guard; its Drop impl then sees
        /// a finished worker and is a no-op.
        fn join(mut self) {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    impl Drop for DialogWorkerGuard {
        fn drop(&mut self) {
            let Some(worker) = self.worker.as_ref() else {
                return;
            };
            if worker.is_finished() {
                return;
            }
            if TEST_DIALOG_EPOCH.load(Ordering::SeqCst) == self.epoch {
                let hwnd = HWND(TEST_DIALOG_HWND.load(Ordering::SeqCst) as *mut c_void);
                if !hwnd.0.is_null() {
                    let _ = unsafe { post_message(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)) };
                }
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if worker.is_finished() {
                let _ = self.worker.take().expect("the worker handle is present").join();
            }
        }
    }

    // Test-only parent window class, shared by the tests that run the real
    // modal dialog: like the dialog class itself, it is registered once per
    // process and reused (a per-test registration would hit
    // ERROR_CLASS_ALREADY_EXISTS on the runner-up).
    static PARENT_GUARD: OnceLock<()> = OnceLock::new();
    const PARENT_CLASS: &str = "WinGlanceDialogTestParent";

    /// Polls the dialog latch until the message-only dialog and its two
    /// id-bearing children (duration edit, error label) have all appeared.
    /// WM_NCCREATE fills the window latch while the test seam is armed, and
    /// the child helper latches the edit/label handles when it creates them;
    /// window enumeration cannot find a message-only window (FindWindowW,
    /// EnumWindows and EnumThreadWindows all skip it), so the latch is the
    /// only observable the tests have. `epoch` is this test's latch
    /// generation (bumped before spawning its worker): the test clears the
    /// latches after the bump, so under the matching generation every
    /// non-zero value is this generation's handle — a stale handle from a
    /// previous test's window can never satisfy the poll.
    fn find_dialog_on_thread(epoch: u64, timeout: Duration) -> (HWND, HWND, HWND) {
        let deadline = Instant::now() + timeout;
        let mut dialog = HWND::default();
        let mut edit = HWND::default();
        let mut label = HWND::default();
        while Instant::now() < deadline {
            if TEST_DIALOG_EPOCH.load(Ordering::SeqCst) == epoch {
                let window = TEST_DIALOG_HWND.load(Ordering::SeqCst);
                let edit_hwnd = TEST_DIALOG_EDIT.load(Ordering::SeqCst);
                let label_hwnd = TEST_DIALOG_LABEL.load(Ordering::SeqCst);
                if window != 0 && edit_hwnd != 0 && label_hwnd != 0 {
                    dialog = HWND(window as *mut c_void);
                    edit = HWND(edit_hwnd as *mut c_void);
                    label = HWND(label_hwnd as *mut c_void);
                    break;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        (dialog, edit, label)
    }

    /// Whether `hwnd` carries the WS_VISIBLE style bit. IsWindowVisible cannot
    /// answer for the dialog's descendants: their ancestor walk ends at the
    /// HWND_MESSAGE pseudo-parent, which has no style bits, so
    /// IsWindowVisible reads false even while the window is shown. The
    /// style bit is the visibility state the dialog's own ShowWindow calls
    /// control, so that is what the tests assert.
    fn style_visible(hwnd: HWND) -> bool {
        unsafe { (GetWindowLongW(hwnd, GWL_STYLE) as u32) & WS_VISIBLE.0 != 0 }
    }

    #[test]
    fn dialog_state_box_installs_through_nccreate_and_frees_through_ncdestroy() {
        let _serialize = DIALOG_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
            // Test helper: the message-only parent has no meaningful DPI;
            // the layout math never runs for this instance.
            scale: 1.0,
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
        let _serialize = DIALOG_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
        // Reserve this test's latch generation before the dialog can open:
        // the poll below accepts only a handle latched under this epoch, so
        // a stale handle from a previous test can never satisfy it.
        let epoch = TEST_DIALOG_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
        // The latch values carry no generation of their own: a poll under
        // this epoch would otherwise accept the previous test's dead window
        // handles the moment the epoch counter matches. Clear them on the
        // main thread before the worker can store this generation's handles.
        TEST_DIALOG_HWND.store(0, Ordering::SeqCst);
        TEST_DIALOG_EDIT.store(0, Ordering::SeqCst);
        TEST_DIALOG_LABEL.store(0, Ordering::SeqCst);
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
                // Message-only, like the dialog: a window created against
                // HWND_MESSAGE can never be displayed and is invisible to
                // FindWindowW/EnumWindows, so the gate's test phase cannot
                // flash the parent or the dialog no matter what else is
                // running on the machine.
                Some(HWND_MESSAGE),
                None,
                instance,
                None,
            )
            .expect("the parent window must be created");
            let _ = parent_tx.send((parent.0 as usize, epoch));
            // Armed only around the dialog's own open, so the flag never
            // leaks into another call on this thread (and the tests are
            // serialized by DIALOG_TEST_LOCK anyway).
            TEST_MESSAGE_ONLY_DIALOG.store(true, Ordering::SeqCst);
            let chosen = show_duration_dialog(parent, 3000);
            TEST_MESSAGE_ONLY_DIALOG.store(false, Ordering::SeqCst);
            // The parent is enabled/disabled on this thread, so observe its
            // state here while the handle is still valid.
            let parent_enabled = IsWindowEnabled(parent).as_bool();
            let _ = result_tx.send((chosen, parent_enabled));
            let _ = DestroyWindow(parent);
        });
        // On a panic this guard closes the dialog and waits for the worker
        // to unwind, instead of leaving it in the modal loop forever.
        let _worker_guard = DialogWorkerGuard::new(worker, epoch);
        // Wait for the parent window to exist, then for the dialog window to
        // appear, then close it. Both windows are message-only, so no window
        // enumeration can reach them — the dialog announces itself through
        // the WM_NCCREATE latch instead.
        let (_parent, epoch) = parent_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the parent window must be created on the dialog thread");
        let (dialog, _, _) = find_dialog_on_thread(epoch, Duration::from_secs(15));
        assert!(!dialog.0.is_null(), "the dialog window must appear");
        let _ = unsafe { post_message(dialog, WM_CLOSE, WPARAM(0), LPARAM(0)) };
        let (chosen, parent_enabled) = result_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the dialog thread must return after WM_CLOSE");
        _worker_guard.join();
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
        let _serialize = DIALOG_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
        // Reserve this test's latch generation (see the re-enable test).
        let epoch = TEST_DIALOG_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
        // Clear the previous generation's latched handles (see the re-enable
        // test): the poll must not accept the past test's dead windows.
        TEST_DIALOG_HWND.store(0, Ordering::SeqCst);
        TEST_DIALOG_EDIT.store(0, Ordering::SeqCst);
        TEST_DIALOG_LABEL.store(0, Ordering::SeqCst);
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
                // Message-only, like the dialog: neither window can ever be
                // displayed or found by FindWindowW/EnumWindows, so the
                // gate's test phase cannot flash them.
                Some(HWND_MESSAGE),
                None,
                instance,
                None,
            )
            .expect("the parent window must be created");
            let _ = parent_tx.send((parent.0 as usize, epoch));
            TEST_MESSAGE_ONLY_DIALOG.store(true, Ordering::SeqCst);
            let chosen = show_duration_dialog(parent, 3000);
            TEST_MESSAGE_ONLY_DIALOG.store(false, Ordering::SeqCst);
            let parent_enabled = IsWindowEnabled(parent).as_bool();
            let _ = result_tx.send((chosen, parent_enabled));
            let _ = DestroyWindow(parent);
        });
        // On a panic this guard closes the dialog and waits for the worker
        // to unwind (see the re-enable test).
        let _worker_guard = DialogWorkerGuard::new(worker, epoch);
        let (_parent, epoch) = parent_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the parent window must be created on the dialog thread");
        // One wait for the whole dialog: the seam latches the window at
        // WM_NCCREATE and the edit/label handles when the dialog thread
        // creates the children, so a single generation-tagged poll covers
        // both — no second deadline racing the control creation.
        let (dialog, edit, error_label) = find_dialog_on_thread(epoch, Duration::from_secs(15));
        assert!(
            !dialog.0.is_null() && !edit.0.is_null() && !error_label.0.is_null(),
            "the dialog, edit and error label must appear"
        );
        let set_text = |text: &str, label: HWND| unsafe {
            let _ = crate::winapi::send_message(label, WM_SETTEXT, WPARAM(0), LPARAM(wide(text).as_ptr() as isize));
        };
        // Invalid input: the label appears and the dialog stays open.
        set_text("abc", edit);
        let _ = unsafe { crate::winapi::send_message(dialog, WM_COMMAND, WPARAM(IDOK.0 as usize), LPARAM(0)) };
        assert!(
            style_visible(error_label),
            "the inline error label must be shown for invalid input"
        );
        assert!(
            unsafe { crate::winapi::is_window(dialog) },
            "the dialog must stay open after invalid input"
        );
        // Corrected input hides the error while typing, then commits and
        // closes the dialog like the old path.
        set_text("7", edit);
        assert!(
            !style_visible(error_label),
            "the inline error label must hide when the text changes"
        );
        let _ = unsafe { crate::winapi::send_message(dialog, WM_COMMAND, WPARAM(IDOK.0 as usize), LPARAM(0)) };
        thread::sleep(Duration::from_millis(50));
        let (chosen, parent_enabled) = result_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the dialog thread must return after the corrected entry");
        _worker_guard.join();
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
    fn rejects_out_of_range_input() {
        assert_eq!(parse_duration_seconds("0.25"), None);
        assert_eq!(parse_duration_seconds("0.1"), None);
        assert_eq!(parse_duration_seconds("-2"), None);
        assert_eq!(parse_duration_seconds("70"), None);
        assert_eq!(parse_duration_seconds("120"), None);
        // Inclusive bounds still commit.
        assert_eq!(parse_duration_seconds("0.5"), Some(500));
        assert_eq!(parse_duration_seconds("60"), Some(60_000));
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
