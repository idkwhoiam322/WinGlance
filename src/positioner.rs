use crate::events::{COMPACT_POSITION_MSG, POSITION_MSG};
use crate::winutil::{clear_window_state, set_window_state, wide, window_state};
use log::debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint,
    FillRect, GetMonitorInfoW, HBRUSH, HDC, HGDIOBJ, HPEN, LineTo, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromWindow, MoveToEx, PAINTSTRUCT, PS_SOLID, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetWindowRect, HWND_TOPMOST,
    PostMessageW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos,
    ShowWindow, WM_CLOSE, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::PCWSTR;

const CLASS_NAME: &str = "WinGlancePositioner";
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

/// Guards class registration: registering twice would leak the class brush.
static CLASS_REGISTERED: OnceLock<()> = OnceLock::new();

struct PositionerState {
    owner: HWND,
    overlay: HWND,
    /// Which position the commit applies to: `POSITION_MSG` for the expanded
    /// pill, `COMPACT_POSITION_MSG` for the independent compact position.
    /// The message routes through the main window (the single config owner),
    /// which writes the matching `position_*` or `compact_position_*` fields.
    result_msg: u32,
    dragging: bool,
    drag_offset: POINT,
    /// Last position committed to the main window (logical coords), so a
    /// release that did not move the pill — a click without a drag, or a
    /// drag back to the same spot — skips the redundant post.
    last_commit: Option<(i32, i32)>,
    /// Fixed paint objects, created at open and freed at teardown so the
    /// paint path creates nothing per repaint.
    bg_brush: HBRUSH,
    x_brush: HBRUSH,
    pen: HPEN,
}

/// Set when this window's WM_NCCREATE claims the state box handed over in
/// `lpCreateParams`, so a failed CreateWindowExW can tell whether the box was
/// taken by the system (and freed in WM_NCDESTROY) or still belongs to the
/// caller. Reset before each open; window creation is single-threaded on the
/// UI thread, so a plain atomic flag is race-free.
static POSITIONER_STATE_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Opens a floating sample notification that the user can drag to set WinGlance's
/// placement. The window stays open until the user clicks X or presses Escape.
pub(crate) fn open(owner: HWND, overlay: HWND) -> bool {
    open_with(owner, overlay, POSITION_MSG)
}

/// Opens the position adjustor for the independent Compact position. The
/// commit is posted with `COMPACT_POSITION_MSG`, so the main window writes the
/// `compact_position_*` fields instead of the expanded ones.
pub(crate) fn open_compact(owner: HWND, overlay: HWND) -> bool {
    open_with(owner, overlay, COMPACT_POSITION_MSG)
}

fn open_with(owner: HWND, overlay: HWND, result_msg: u32) -> bool {
    unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(handle) => HINSTANCE(handle.0),
            Err(_) => return false,
        };
        let class_name = wide(CLASS_NAME);
        if !register_class(instance, &class_name) {
            return false;
        }
        close_existing();

        let state = Box::new(PositionerState {
            owner,
            overlay,
            result_msg,
            dragging: false,
            drag_offset: POINT::default(),
            last_commit: None,
            bg_brush: CreateSolidBrush(COLORREF(0x00121212)),
            x_brush: CreateSolidBrush(COLORREF(0x333333)),
            pen: CreatePen(PS_SOLID, 2, COLORREF(0x999999)),
        });
        let state_ptr = Box::into_raw(state);
        POSITIONER_STATE_CLAIMED.store(false, Ordering::SeqCst);
        // The positioner sits on the owner's monitor: create it at the owner's
        // DPI so the sample box and its close button are sized like the rest
        // of the UI on high-DPI displays.
        let scale = GetDpiForWindow(owner).max(96) as f32 / 96.0;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("Place the WinGlance").as_ptr()),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            (WIDTH as f32 * scale).round() as i32,
            (HEIGHT as f32 * scale).round() as i32,
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
                debug!("position adjustor opened");
                true
            }
            Err(_) => {
                // The state box is owned by the window from WM_NCCREATE onward
                // and freed in WM_NCDESTROY. WM_NCCREATE flips
                // POSITIONER_STATE_CLAIMED when it takes the box; if it never
                // ran, the box still belongs to us and must be freed here —
                // including its fixed GDI objects.
                if !POSITIONER_STATE_CLAIMED.load(Ordering::SeqCst) {
                    let state = Box::from_raw(state_ptr);
                    let _ = DeleteObject(HGDIOBJ(state.bg_brush.0));
                    let _ = DeleteObject(HGDIOBJ(state.x_brush.0));
                    let _ = DeleteObject(HGDIOBJ(state.pen.0));
                    drop(state);
                }
                false
            }
        }
    }
}

/// Closes an already-open positioner window, so opening a second one cannot
/// stack two draggable samples. No-op when none is open.
pub(crate) fn close_existing() {
    crate::winutil::close_registered(&OPEN_POSITIONER, |(positioner, _)| {
        (*positioner != 0).then_some(HWND(*positioner as *mut std::ffi::c_void))
    });
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
        } else {
            debug!("position adjustor reset to the default spot");
        }
    }
}

fn register_class(instance: HINSTANCE, class_name: &[u16]) -> bool {
    crate::winutil::register_class_once(
        &CLASS_REGISTERED,
        instance,
        class_name,
        Some(positioner_proc),
        || Some(unsafe { CreateSolidBrush(COLORREF(0x00121212)) }),
        "the positioner window",
    )
    .is_ok()
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

/// Snaps a value to the nearest edge if within `threshold` physical pixels.
fn snap(val: i32, edge: i32, threshold: i32) -> i32 {
    if (val - edge).abs() <= threshold { edge } else { val }
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

    // Snap to edges and center; the threshold is a logical-pixel distance,
    // scaled to this monitor so the snap feels the same at any DPI.
    let snap_threshold = (SNAP_THRESHOLD as f32 * scale).round() as i32;
    let mut phys_x = rect.left;
    let mut phys_y = rect.top;
    phys_x = snap(phys_x, work.left, snap_threshold);
    phys_x = snap(phys_x, work.right - sample_w, snap_threshold);
    phys_x = snap(
        phys_x,
        work.left + (work.right - work.left - sample_w) / 2,
        snap_threshold,
    );
    phys_y = snap(phys_y, work.top, snap_threshold);
    phys_y = snap(phys_y, work.bottom - sample_h, snap_threshold);
    phys_x = phys_x.clamp(work.left, (work.right - sample_w).max(work.left));
    phys_y = phys_y.clamp(work.top, (work.bottom - sample_h).max(work.top));

    let log_x = (phys_x as f32 / scale).round() as i32;
    let log_y = (phys_y as f32 / scale).round() as i32;

    // Skip when the release did not move the pill: a click without a drag
    // (or a drag back to the same spot) must not re-persist the config and
    // re-nudge the overlay.
    if state.last_commit == Some((log_x, log_y)) {
        return;
    }
    if let Err(error) = unsafe {
        PostMessageW(
            state.owner,
            state.result_msg,
            WPARAM(log_x as usize),
            LPARAM(log_y as isize),
        )
    } {
        debug!("positioner PostMessageW failed: {error}");
    } else {
        state.last_commit = Some((log_x, log_y));
        debug!("position adjustor committed ({log_x}, {log_y})");
    }
}

/// Returns true if the click point (in client coords) is on the X button area.
/// The button geometry is scaled by the window's DPI.
fn hit_close_button(hwnd: HWND, cx: i32, cy: i32) -> bool {
    let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
    let w = (CLOSE_BTN_W as f32 * scale).round() as i32;
    let h = (CLOSE_BTN_H as f32 * scale).round() as i32;
    let width = (WIDTH as f32 * scale).round() as i32;
    let pad = (6.0 * scale).round() as i32;
    (width - w - pad..=width - pad).contains(&cx) && (pad..=pad + h).contains(&cy)
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn positioner_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state_ptr = window_state::<PositionerState>(hwnd);
    match message {
        WM_NCCREATE => {
            let create = lparam.0 as *const CREATESTRUCTW;
            if !create.is_null() {
                let state = (*create).lpCreateParams as *mut PositionerState;
                set_window_state(hwnd, state);
                POSITIONER_STATE_CLAIMED.store(true, Ordering::SeqCst);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            if !state_ptr.is_null() {
                let cx = (lparam.0 & 0xFFFF) as i32;
                let cy = ((lparam.0 >> 16) & 0xFFFF) as i32;
                if hit_close_button(hwnd, cx, cy) {
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
            if !hdc.0.is_null() && !state_ptr.is_null() {
                let state = &*state_ptr;
                unsafe {
                    let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
                    let width = (WIDTH as f32 * scale).round() as i32;
                    let height = (HEIGHT as f32 * scale).round() as i32;
                    let close_w = (CLOSE_BTN_W as f32 * scale).round() as i32;
                    let close_h = (CLOSE_BTN_H as f32 * scale).round() as i32;
                    let pad = (6.0 * scale).round() as i32;
                    let whole = RECT {
                        left: 0,
                        top: 0,
                        right: width,
                        bottom: height,
                    };
                    let _ = FillRect(hdc, &whole, state.bg_brush);

                    // Draw instruction text
                    let mut text_rect = RECT {
                        left: 12,
                        top: 0,
                        right: width - close_w - 16,
                        bottom: height,
                    };
                    let _ = SetBkMode(hdc, TRANSPARENT);
                    let _ = SetTextColor(hdc, COLORREF(0xCCCCCC));
                    let mut text = wide("Drag to place the WinGlance");
                    let _ = DrawTextW(hdc, &mut text, &mut text_rect, DT_SINGLELINE | DT_CENTER | DT_VCENTER);

                    // Draw X button (cross lines — always perfectly
                    // centered, unlike a glyph drawn with the default
                    // window font)
                    let x_rect = RECT {
                        left: width - close_w - pad,
                        top: pad,
                        right: width - pad,
                        bottom: pad + close_h,
                    };
                    let _ = FillRect(hdc, &x_rect, state.x_brush);

                    let old_pen = SelectObject(hdc, state.pen);
                    let inset = (8.0 * scale).round() as i32;
                    let _ = MoveToEx(hdc, x_rect.left + inset, x_rect.top + inset, None);
                    let _ = LineTo(hdc, x_rect.right - inset, x_rect.bottom - inset);
                    let _ = MoveToEx(hdc, x_rect.right - inset, x_rect.top + inset, None);
                    let _ = LineTo(hdc, x_rect.left + inset, x_rect.bottom - inset);
                    SelectObject(hdc, old_pen);
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
            debug!("position adjustor closed");
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(state.bg_brush.0));
                    let _ = DeleteObject(HGDIOBJ(state.x_brush.0));
                    let _ = DeleteObject(HGDIOBJ(state.pen.0));
                }
                drop(Box::from_raw(state_ptr));
            }
            if let Some(m) = OPEN_POSITIONER.get()
                && let Ok(mut guard) = m.lock()
                && guard.0 == hwnd.0 as usize
            {
                *guard = (0, 0);
            }
            clear_window_state(hwnd);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}
