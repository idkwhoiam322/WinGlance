use crate::events::POSITION_MSG;
use log::debug;
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint,
    FillRect, GetMonitorInfoW, HBRUSH, HDC, HGDIOBJ, LineTo, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    MoveToEx, PAINTSTRUCT, PS_SOLID, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetCursorPos, GetWindowLongPtrW,
    GetWindowRect, HWND_TOPMOST, IDC_ARROW, LoadCursorW, PostMessageW, RegisterClassExW, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_CLOSE,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WNDCLASS_STYLES,
    WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::PCWSTR;

const CLASS_NAME: &str = "NotchPositioner";
const WIDTH: i32 = 320;
const HEIGHT: i32 = 60;
const SNAP_THRESHOLD: i32 = 30;
const CLOSE_BTN_W: i32 = 28;
const CLOSE_BTN_H: i32 = 28;
const DEFAULT_MARGIN: f32 = 8.0;

/// Tracks the currently open positioner window and its overlay, so the settings
/// Reset action can move the adjustor back to the default spot. Stored as raw
/// handle values: HWND is not Send, so the static holds usize.
static OPEN_POSITIONER: OnceLock<Mutex<(usize, usize)>> = OnceLock::new(); // (positioner, overlay)

struct PositionerState {
    owner: HWND,
    overlay: HWND,
    dragging: bool,
    drag_offset: POINT,
}

/// Opens a floating sample notification that the user can drag to set the notch's
/// placement. The window stays open until the user clicks X or presses Escape.
pub(crate) fn open(owner: HWND, overlay: HWND) -> bool {
    unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(handle) => HINSTANCE(handle.0),
            Err(_) => return false,
        };
        let class_name = wide(CLASS_NAME);
        register_class(instance, &class_name);

        let state = Box::new(PositionerState {
            owner,
            overlay,
            dragging: false,
            drag_offset: POINT::default(),
        });
        let state_ptr = Box::into_raw(state);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("Place the notch").as_ptr()),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            WIDTH,
            HEIGHT,
            owner,
            None,
            instance,
            Some(state_ptr.cast()),
        );
        match hwnd {
            Ok(hwnd) => {
                if let Ok(mut guard) = OPEN_POSITIONER.get_or_init(|| Mutex::new((0, 0))).lock() {
                    *guard = (hwnd.0 as usize, overlay.0 as usize);
                }
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                true
            }
            Err(_) => {
                drop(Box::from_raw(state_ptr));
                false
            }
        }
    }
}

/// Moves the open positioner window back to the default top-center spot (the
/// same place the settings Reset applies to the pill). No-op when the positioner
/// is not open.
pub(crate) fn reset_position() {
    let Some(m) = OPEN_POSITIONER.get() else {
        return;
    };
    let Ok(guard) = m.lock() else {
        return;
    };
    let (hwnd, overlay) = *guard;
    if hwnd == 0 || overlay == 0 {
        return;
    }
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    let overlay = HWND(overlay as *mut std::ffi::c_void);
    unsafe {
        let scale = GetDpiForWindow(overlay).max(96) as f32 / 96.0;
        let work = monitor_work_area(overlay);
        let w = (WIDTH as f32 * scale).round() as i32;
        let margin = (DEFAULT_MARGIN * scale).round() as i32;
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.top + margin;
        if let Err(error) = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        ) {
            debug!("positioner reset SetWindowPos failed: {error}");
        }
    }
}

fn register_class(instance: HINSTANCE, class_name: &[u16]) {
    unsafe {
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(positioner_proc),
            hInstance: instance,
            hCursor: cursor,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hbrBackground: CreateSolidBrush(COLORREF(0x00121212)),
            ..Default::default()
        };
        let _ = RegisterClassExW(&class);
    }
}

fn monitor_work_area(hwnd: HWND) -> RECT {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let _ = GetMonitorInfoW(monitor, &mut info);
        info.rcWork
    }
}

/// Snaps a value to the nearest edge if within SNAP_THRESHOLD.
fn snap(val: i32, edge: i32) -> i32 {
    if (val - edge).abs() <= SNAP_THRESHOLD {
        edge
    } else {
        val
    }
}

/// Persists the positioner window's current screen position as absolute overlay
/// coordinates and nudges the live overlay (without dismissing the window).
/// The chosen position is posted to the main window, which owns the config and
/// applies it — one writer, no disk round-trip that could clobber other settings.
fn commit(hwnd: HWND, state: &mut PositionerState) {
    let mut rect = RECT::default();
    if let Err(error) = unsafe { GetWindowRect(hwnd, &mut rect) } {
        debug!("positioner GetWindowRect failed: {error}");
        return;
    }
    let scale = unsafe { GetDpiForWindow(state.overlay).max(96) } as f32 / 96.0;
    let work = monitor_work_area(state.overlay);
    let sample_w = (WIDTH as f32 * scale).round() as i32;
    let sample_h = (HEIGHT as f32 * scale).round() as i32;

    // Snap to edges and center
    let mut phys_x = rect.left;
    let mut phys_y = rect.top;
    phys_x = snap(phys_x, work.left);
    phys_x = snap(phys_x, work.right - sample_w);
    phys_x = snap(phys_x, work.left + (work.right - work.left - sample_w) / 2);
    phys_y = snap(phys_y, work.top);
    phys_y = snap(phys_y, work.bottom - sample_h);
    phys_x = phys_x.clamp(work.left, (work.right - sample_w).max(work.left));
    phys_y = phys_y.clamp(work.top, (work.bottom - sample_h).max(work.top));

    let log_x = (phys_x as f32 / scale).round() as i32;
    let log_y = (phys_y as f32 / scale).round() as i32;

    if let Err(error) = unsafe {
        PostMessageW(
            state.owner,
            POSITION_MSG,
            WPARAM(log_x as usize),
            LPARAM(log_y as isize),
        )
    } {
        debug!("positioner PostMessageW failed: {error}");
    }
}

/// Returns true if the click point (in client coords) is on the X button area.
fn hit_close_button(cx: i32, cy: i32) -> bool {
    (WIDTH - CLOSE_BTN_W - 6..=WIDTH - 6).contains(&cx) && (6..=6 + CLOSE_BTN_H).contains(&cy)
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn positioner_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PositionerState };
    match message {
        WM_NCCREATE => {
            let create = lparam.0 as *const CREATESTRUCTW;
            if !create.is_null() {
                let state = (*create).lpCreateParams as *mut PositionerState;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            if !state_ptr.is_null() {
                let cx = (lparam.0 & 0xFFFF) as i32;
                let cy = ((lparam.0 >> 16) & 0xFFFF) as i32;
                if hit_close_button(cx, cy) {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                let state = &mut *state_ptr;
                let mut cursor = POINT::default();
                if GetCursorPos(&mut cursor).is_ok() {
                    let mut rect = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rect);
                    state.dragging = true;
                    state.drag_offset.x = rect.left - cursor.x;
                    state.drag_offset.y = rect.top - cursor.y;
                    let _ = SetCapture(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if !state_ptr.is_null() && (*state_ptr).dragging {
                let state = &mut *state_ptr;
                let mut cursor = POINT::default();
                if GetCursorPos(&mut cursor).is_ok() {
                    let x = cursor.x + state.drag_offset.x;
                    let y = cursor.y + state.drag_offset.y;
                    if let Err(error) = SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        x,
                        y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    ) {
                        debug!("positioner drag SetWindowPos failed: {error}");
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                state.dragging = false;
                let _ = ReleaseCapture();
                commit(hwnd, state);
            }
            // Don't destroy — keep window open so user can fine-tune
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let hdc: HDC = BeginPaint(hwnd, &mut paint);
            if !hdc.0.is_null() {
                unsafe {
                    let brush: HBRUSH = CreateSolidBrush(COLORREF(0x00121212));
                    let whole = RECT {
                        left: 0,
                        top: 0,
                        right: WIDTH,
                        bottom: HEIGHT,
                    };
                    let _ = FillRect(hdc, &whole, brush);

                    // Draw instruction text
                    let mut text_rect = RECT {
                        left: 12,
                        top: 0,
                        right: WIDTH - CLOSE_BTN_W - 16,
                        bottom: HEIGHT,
                    };
                    let _ = SetBkMode(hdc, TRANSPARENT);
                    let _ = SetTextColor(hdc, COLORREF(0xCCCCCC));
                    let mut text = wide("Drag to place the notch");
                    let _ = DrawTextW(hdc, &mut text, &mut text_rect, DT_SINGLELINE | DT_CENTER | DT_VCENTER);

                    // Draw X button (cross lines — always perfectly centered,
                    // unlike a glyph drawn with the default window font)
                    let x_brush = CreateSolidBrush(COLORREF(0x333333));
                    let x_rect = RECT {
                        left: WIDTH - CLOSE_BTN_W - 6,
                        top: 6,
                        right: WIDTH - 6,
                        bottom: 6 + CLOSE_BTN_H,
                    };
                    let _ = FillRect(hdc, &x_rect, x_brush);
                    let _ = DeleteObject(HGDIOBJ(x_brush.0));

                    let pen = CreatePen(PS_SOLID, 2, COLORREF(0x999999));
                    let old_pen = SelectObject(hdc, pen);
                    let inset = 8;
                    let _ = MoveToEx(hdc, x_rect.left + inset, x_rect.top + inset, None);
                    let _ = LineTo(hdc, x_rect.right - inset, x_rect.bottom - inset);
                    let _ = MoveToEx(hdc, x_rect.right - inset, x_rect.top + inset, None);
                    let _ = LineTo(hdc, x_rect.left + inset, x_rect.bottom - inset);
                    SelectObject(hdc, old_pen);
                    let _ = DeleteObject(HGDIOBJ(pen.0));

                    let _ = DeleteObject(HGDIOBJ(brush.0));
                }
            }
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            if !state_ptr.is_null() {
                drop(Box::from_raw(state_ptr));
            }
            if let Some(m) = OPEN_POSITIONER.get()
                && let Ok(mut guard) = m.lock()
                && guard.0 == hwnd.0 as usize
            {
                *guard = (0, 0);
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
