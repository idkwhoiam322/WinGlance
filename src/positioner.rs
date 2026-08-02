use crate::config::Config;
use crate::overlay::{self, OverlayPos};
use log::error;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetMonitorInfoW, HBRUSH, HDC, HGDIOBJ, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, PAINTSTRUCT,
    SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetCursorPos, GetWindowLongPtrW,
    GetWindowRect, HWND_TOPMOST, IDC_ARROW, LoadCursorW, RegisterClassExW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_CLOSE, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WNDCLASS_STYLES, WNDCLASSEXW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::PCWSTR;

const CLASS_NAME: &str = "NotchPositioner";
const WIDTH: i32 = 240;
const HEIGHT: i32 = 60;

struct PositionerState {
    overlay: HWND,
    dragging: bool,
    drag_offset: POINT,
}

/// Opens a floating sample notification that the user can drag to set the notch's
/// placement. On release the chosen X/Y are written to config and the live overlay
/// is repositioned (and briefly previewed).
pub(crate) fn open(owner: HWND, overlay: HWND) -> bool {
    unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(handle) => HINSTANCE(handle.0),
            Err(_) => return false,
        };
        let class_name = wide(CLASS_NAME);
        register_class(instance, &class_name);

        let state = Box::new(PositionerState {
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

/// Persists the positioner window's current screen position as absolute overlay
/// coordinates and nudges the live overlay, then dismisses the mini-window.
fn commit(hwnd: HWND, state: &mut PositionerState) {
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    let scale = unsafe { GetDpiForWindow(state.overlay).max(96) } as f32 / 96.0;
    let work = monitor_work_area(state.overlay);
    let sample_w = (WIDTH as f32 * scale).round() as i32;
    let sample_h = (HEIGHT as f32 * scale).round() as i32;
    let phys_x = rect.left.clamp(work.left, (work.right - sample_w).max(work.left));
    let phys_y = rect.top.clamp(work.top, (work.bottom - sample_h).max(work.top));
    let log_x = (phys_x as f32 / scale).round() as i32;
    let log_y = (phys_y as f32 / scale).round() as i32;

    if let Ok(mut config) = Config::load() {
        config.overlay.position_x = Some(log_x);
        config.overlay.position_y = Some(log_y);
        if let Err(error) = config.save() {
            error!("saving position config: {error:#}");
        }
        let pos = OverlayPos::from_config(&config);
        overlay::set_position(state.overlay, pos);
    }
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
                    let _ = SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        x,
                        y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
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
            let _ = DestroyWindow(hwnd);
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
                    let mut whole = RECT {
                        left: 0,
                        top: 0,
                        right: WIDTH,
                        bottom: HEIGHT,
                    };
                    let _ = FillRect(hdc, &whole, brush);
                    let _ = SetBkMode(hdc, TRANSPARENT);
                    let _ = SetTextColor(hdc, COLORREF(0xE6E6E6));
                    let mut text = wide("Notch - drag to place");
                    let _ = DrawTextW(hdc, &mut text, &mut whole, DT_SINGLELINE | DT_CENTER | DT_VCENTER);
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
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
