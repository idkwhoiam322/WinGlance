use crate::events::{COMPACT_POSITION_MSG, POSITION_MSG};
use crate::winapi::{create_window, delete_object, invalidate_rect, post_message, select_object, set_window_pos};
use crate::winutil::{
    Registered, StateClaim, clear_registered, release_window_state, set_window_state, wide, window_state,
};
use log::debug;
use std::sync::OnceLock;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DrawTextW, EndPaint, FillRect,
    GetMonitorInfoW, HBRUSH, HDC, HGDIOBJ, HPEN, LineTo, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    MoveToEx, PAINTSTRUCT, PS_SOLID, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SHIFT,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, DefWindowProcW, DestroyWindow, GetCursorPos, GetWindowRect, HWND_TOPMOST, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow, ShowWindow, WM_CLOSE, WM_DPICHANGED,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::PCWSTR;

const CLASS_NAME: &str = "WinGlancePositioner";
const WIDTH: i32 = 320;
const HEIGHT: i32 = 60;
const SNAP_THRESHOLD: i32 = 30;
const CLOSE_BTN_W: i32 = 28;
const CLOSE_BTN_H: i32 = 28;
const DEFAULT_MARGIN: f32 = 8.0;
const ACCESSIBLE_INSTRUCTIONS: &str =
    "Place WinGlance. Drag or use arrow keys; hold Shift for 10-pixel moves; Enter saves; Escape cancels.";
const PAINT_INSTRUCTIONS: &str = "Drag / arrows Â· Enter saves Â· Esc cancels";
/// Logical width of the close-button cross pen, scaled by the window's DPI so
/// the drawn lines keep the same visual weight on any display.
const PEN_W: f32 = 2.0;

/// Tracks the currently open positioner window and its overlay, so the settings
/// Reset action can move the adjustor back to the default spot. Stored as raw
/// handle values: HWND is not Send, so the static holds usize.
static OPEN_POSITIONER: Registered<(usize, usize)> = Registered::new(); // (positioner, overlay)

/// Guards class registration: registering twice would leak the class brush.
static CLASS_REGISTERED: OnceLock<()> = OnceLock::new();

struct PositionerState {
    owner: HWND,
    /// The overlay whose placement this sample edits — the commit converts
    /// its drop point with the overlay *target's* DPI (queried live), not the
    /// sample window's own monitor scale.
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
/// caller. Reset before each open. See `winutil::StateClaim` for the shared
/// mechanics.
static POSITIONER_STATE_CLAIMED: StateClaim = StateClaim::new();

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

        // Create the window at the owner's DPI so the sample box, its close
        // button and the cross pen are sized like the rest of the UI on
        // high-DPI displays.
        let scale = GetDpiForWindow(owner).max(96) as f32 / 96.0;
        let state = Box::new(PositionerState {
            owner,
            overlay,
            result_msg,
            dragging: false,
            drag_offset: POINT::default(),
            last_commit: None,
            bg_brush: CreateSolidBrush(COLORREF(0x00121212)),
            x_brush: CreateSolidBrush(COLORREF(0x333333)),
            pen: CreatePen(PS_SOLID, (PEN_W * scale).round().max(1.0) as i32, COLORREF(0x999999)),
        });
        let state_ptr = Box::into_raw(state);
        POSITIONER_STATE_CLAIMED.reset();
        let hwnd = create_window(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide(ACCESSIBLE_INSTRUCTIONS).as_ptr()),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            (WIDTH as f32 * scale).round() as i32,
            (HEIGHT as f32 * scale).round() as i32,
            Some(owner),
            None,
            instance,
            Some(state_ptr.cast()),
        );
        match hwnd {
            Ok(hwnd) => {
                OPEN_POSITIONER.set((hwnd.0 as usize, overlay.0 as usize));
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                // Start the sample where the pill currently lives, so the user
                // drags from the right spot; fall back to the default top-center
                // placement (the same target the Settings Reset applies to the
                // pill) when the overlay has no on-screen window. The sample is
                // placed on the overlay's target monitor and clamped into its
                // work area, so it can never launch off-monitor.
                let scale = GetDpiForWindow(overlay).max(96) as f32 / 96.0;
                let work = monitor_work_area(overlay);
                let pw = (WIDTH as f32 * scale).round() as i32;
                let ph = (HEIGHT as f32 * scale).round() as i32;
                let mut pill_rect = RECT::default();
                let (x, y) = if GetWindowRect(overlay, &mut pill_rect).is_ok()
                    && pill_rect.left != pill_rect.right
                    && pill_rect.top != pill_rect.bottom
                {
                    (
                        pill_rect.left.clamp(work.left, (work.right - pw).max(work.left)),
                        pill_rect.top.clamp(work.top, (work.bottom - ph).max(work.top)),
                    )
                } else {
                    let margin = (DEFAULT_MARGIN * scale).round() as i32;
                    (work.left + (work.right - work.left - pw) / 2, work.top + margin)
                };
                let _ = set_window_pos(
                    hwnd,
                    HWND_TOPMOST,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                // The positioner is opened only from an explicit Settings
                // action, so it is allowed to become the active keyboard target.
                // The pill itself remains passive/no-activate; this temporary
                // editor is the deliberate interactive exception.
                if !SetForegroundWindow(hwnd).as_bool() {
                    debug!(
                        "positioner SetForegroundWindow was refused; SetFocus will still target the editor on this UI thread"
                    );
                }
                let _ = SetFocus(Some(hwnd));
                debug!("position adjustor opened at ({x}, {y})");
                true
            }
            Err(_) => {
                // The state box is owned by the window from WM_NCCREATE onward
                // and freed in WM_NCDESTROY. WM_NCCREATE flips
                // POSITIONER_STATE_CLAIMED when it takes the box; if it never
                // ran, the box still belongs to us and must be freed here —
                // including its fixed GDI objects.
                if let Some(state) = POSITIONER_STATE_CLAIMED.take_unclaimed(state_ptr) {
                    let _ = delete_object(HGDIOBJ(state.bg_brush.0));
                    let _ = delete_object(HGDIOBJ(state.x_brush.0));
                    let _ = delete_object(HGDIOBJ(state.pen.0));
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
    let Some((hwnd, overlay)) = OPEN_POSITIONER.read() else {
        return;
    };
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
        if let Err(error) = set_window_pos(
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PositionerKeyAction {
    Nudge(i32, i32),
    Commit,
    Cancel,
    Ignore,
}

/// Pure keyboard mapping for the interactive positioner. Arrow keys move one
/// logical pixel, or ten while Shift is held; Enter confirms once and Escape
/// closes without committing the unconfirmed keyboard preview.
fn positioner_key_action(key: u16, shift: bool) -> PositionerKeyAction {
    let step = if shift { 10 } else { 1 };
    match key {
        k if k == VK_LEFT.0 => PositionerKeyAction::Nudge(-step, 0),
        k if k == VK_RIGHT.0 => PositionerKeyAction::Nudge(step, 0),
        k if k == VK_UP.0 => PositionerKeyAction::Nudge(0, -step),
        k if k == VK_DOWN.0 => PositionerKeyAction::Nudge(0, step),
        k if k == VK_RETURN.0 => PositionerKeyAction::Commit,
        k if k == VK_ESCAPE.0 => PositionerKeyAction::Cancel,
        _ => PositionerKeyAction::Ignore,
    }
}

/// Applies a keyboard preview nudge in physical pixels and clamps the sample
/// into the current monitor work area. Snap is intentionally deferred to
/// `commit`, exactly like mouse dragging: applying snap after every 1 px key
/// press would make a snapped edge impossible to leave.
fn nudge_clamped_position(
    origin: (i32, i32),
    delta_logical: (i32, i32),
    scale: f32,
    work: RECT,
    sample_size: (i32, i32),
) -> (i32, i32) {
    let (left, top) = origin;
    let (dx_logical, dy_logical) = delta_logical;
    let (sample_w, sample_h) = sample_size;
    let dx = (dx_logical as f32 * scale).round() as i32;
    let dy = (dy_logical as f32 * scale).round() as i32;
    (
        (left + dx).clamp(work.left, (work.right - sample_w).max(work.left)),
        (top + dy).clamp(work.top, (work.bottom - sample_h).max(work.top)),
    )
}

fn nudge_positioner(hwnd: HWND, dx_logical: i32, dy_logical: i32) {
    unsafe {
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let work = monitor_work_area(hwnd);
        let sample_w = (WIDTH as f32 * scale).round() as i32;
        let sample_h = (HEIGHT as f32 * scale).round() as i32;
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return;
        }
        let (x, y) = nudge_clamped_position(
            (rect.left, rect.top),
            (dx_logical, dy_logical),
            scale,
            work,
            (sample_w, sample_h),
        );
        if let Err(error) = set_window_pos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_SHOWWINDOW,
        ) {
            debug!("positioner keyboard SetWindowPos failed: {error}");
        }
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
    // The snap edges, threshold and the work-area clamp describe the drag
    // itself, so they come from the sample window's own DPI and work area.
    // The *stored* value, however, is re-scaled by the overlay target's DPI
    // when placement applies it (`fullscreen::placement` multiplies the
    // logical override by the target monitor's scale), so the
    // physical→logical conversion must use that same target scale — a
    // mixed-DPI drag (sample monitor ≠ pill target) would otherwise land
    // displaced.
    let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
    let target_scale =
        crate::overlay::dpi_for_position(state.overlay, state.result_msg == COMPACT_POSITION_MSG).max(96) as f32 / 96.0;
    let work = monitor_work_area(hwnd);
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

    let log_x = to_logical(phys_x, target_scale);
    let log_y = to_logical(phys_y, target_scale);

    // Skip when the release did not move the pill: a click without a drag
    // (or a drag back to the same spot) must not re-persist the config and
    // re-nudge the overlay.
    if state.last_commit == Some((log_x, log_y)) {
        return;
    }
    if let Err(error) = unsafe {
        post_message(
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

unsafe extern "system" fn positioner_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Panic-contained; a panic logs, posts quit and falls back to
    // DefWindowProcW.
    crate::winutil::guarded_wndproc(
        hwnd,
        message,
        wparam,
        lparam,
        "the positioner window procedure",
        || unsafe { positioner_proc_body(hwnd, message, wparam, lparam) },
    )
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn positioner_proc_body(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state_ptr = window_state::<PositionerState>(hwnd);
    match message {
        WM_NCCREATE => {
            let create = lparam.0 as *const CREATESTRUCTW;
            if !create.is_null() {
                let state = (*create).lpCreateParams as *mut PositionerState;
                // Same guard as the overlay and main window: a null param must
                // not flip POSITIONER_STATE_CLAIMED while GWLP_USERDATA stays
                // empty — WM_NCDESTROY would free nothing and take_unclaimed
                // would refuse to return the box, leaking it with its GDI
                // objects on a failed create. With the guard, an unclaimed
                // box returns to the caller's failure branch.
                if !state.is_null() {
                    set_window_state(hwnd, state);
                    POSITIONER_STATE_CLAIMED.claim();
                }
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
                    if let Err(error) = set_window_pos(
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
        WM_KEYDOWN => {
            let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
            match positioner_key_action(wparam.0 as u16, shift) {
                PositionerKeyAction::Nudge(dx, dy) => {
                    nudge_positioner(hwnd, dx, dy);
                    LRESULT(0)
                }
                PositionerKeyAction::Commit => {
                    if !state_ptr.is_null() {
                        commit(hwnd, &mut *state_ptr);
                    }
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                PositionerKeyAction::Cancel => {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                PositionerKeyAction::Ignore => DefWindowProcW(hwnd, message, wparam, lparam),
            }
        }
        WM_DPICHANGED => {
            // The user dragged the sample onto a display with a different DPI.
            // Rebuild the DPI-sized resources: the cross pen is a fixed GDI
            // object cached on the state (brushes are solid colors and need no
            // rebuild), and the window grows to the new physical size. The
            // top-left is kept where the user placed it; the paint and
            // close-button paths read the DPI live, so they follow along.
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let new_dpi = (wparam.0 >> 16) as u32;
                let scale = new_dpi.max(96) as f32 / 96.0;
                unsafe {
                    let _ = delete_object(HGDIOBJ(state.pen.0));
                }
                state.pen = unsafe { CreatePen(PS_SOLID, (PEN_W * scale).round().max(1.0) as i32, COLORREF(0x999999)) };
                let w = (WIDTH as f32 * scale).round() as i32;
                let h = (HEIGHT as f32 * scale).round() as i32;
                let mut rect = RECT::default();
                let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
                let _ = unsafe {
                    set_window_pos(
                        hwnd,
                        HWND_TOPMOST,
                        rect.left,
                        rect.top,
                        w,
                        h,
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    )
                };
                let _ = unsafe { invalidate_rect(hwnd, None, true) };
            }
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
                    let mut text = wide(PAINT_INSTRUCTIONS).into_vec();
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

                    let old_pen = select_object(hdc, state.pen);
                    let inset = (8.0 * scale).round() as i32;
                    let _ = MoveToEx(hdc, x_rect.left + inset, x_rect.top + inset, None);
                    let _ = LineTo(hdc, x_rect.right - inset, x_rect.bottom - inset);
                    let _ = MoveToEx(hdc, x_rect.right - inset, x_rect.top + inset, None);
                    let _ = LineTo(hdc, x_rect.left + inset, x_rect.bottom - inset);
                    select_object(hdc, old_pen);
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
                    let _ = delete_object(HGDIOBJ(state.bg_brush.0));
                    let _ = delete_object(HGDIOBJ(state.x_brush.0));
                    let _ = delete_object(HGDIOBJ(state.pen.0));
                }
                // Slot clear first, box second — the canonical order every
                // window applies via the shared helper.
                release_window_state(hwnd, state_ptr);
            }
            clear_registered(&OPEN_POSITIONER, |guard| guard.0 == hwnd.0 as usize);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// Converts a dropped physical coordinate into the stored 96-DPI logical
/// value using the *target* monitor's scale — the same scale `placement`
/// re-applies — so the pill lands at the exact drop point on mixed-DPI
/// systems (the sample's own scale governs only snapping and clamping,
/// where the drag physically happens).
fn to_logical(phys: i32, target_scale: f32) -> i32 {
    (phys as f32 / target_scale).round() as i32
}

#[cfg(test)]
mod tests {
    use super::{PositionerKeyAction, nudge_clamped_position, positioner_key_action, to_logical};
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP};

    #[test]
    fn keyboard_actions_cover_fine_coarse_commit_and_cancel() {
        assert_eq!(
            positioner_key_action(VK_LEFT.0, false),
            PositionerKeyAction::Nudge(-1, 0)
        );
        assert_eq!(
            positioner_key_action(VK_RIGHT.0, true),
            PositionerKeyAction::Nudge(10, 0)
        );
        assert_eq!(positioner_key_action(VK_UP.0, true), PositionerKeyAction::Nudge(0, -10));
        assert_eq!(
            positioner_key_action(VK_DOWN.0, false),
            PositionerKeyAction::Nudge(0, 1)
        );
        assert_eq!(positioner_key_action(VK_RETURN.0, false), PositionerKeyAction::Commit);
        assert_eq!(positioner_key_action(VK_ESCAPE.0, true), PositionerKeyAction::Cancel);
        assert_eq!(positioner_key_action(0, false), PositionerKeyAction::Ignore);
    }

    #[test]
    fn keyboard_nudge_clamps_to_the_work_area_without_edge_locking() {
        let work = RECT {
            left: 100,
            top: 200,
            right: 500,
            bottom: 500,
        };
        // Fine movement away from an edge is allowed immediately; snap is
        // deferred to commit, so the user is never trapped on the edge.
        assert_eq!(
            nudge_clamped_position((100, 200), (1, 1), 1.0, work, (100, 60)),
            (101, 201)
        );
        // Movement beyond the far edge clamps the whole sample into rcWork.
        assert_eq!(
            nudge_clamped_position((400, 440), (10, 10), 1.0, work, (100, 60)),
            (400, 440)
        );
        // Logical movement scales with DPI.
        assert_eq!(
            nudge_clamped_position((200, 300), (10, -10), 1.5, work, (100, 60)),
            (215, 285)
        );
    }

    #[test]
    fn the_stored_point_converts_with_the_target_scale() {
        // Mixed-DPI drag: the sample sits on a 100% monitor while the pill
        // targets 150%. A drop at physical x=2400 must store 1600 so that
        // placement's `logical * target_scale` lands back at 2400 — the
        // exact drop point, not a displaced coordinate.
        let log = to_logical(2400, 1.5);
        assert_eq!(log, 1600);
        assert_eq!((log as f32 * 1.5).round() as i32, 2400);
    }

    #[test]
    fn a_same_monitor_drag_keeps_the_identity_conversion() {
        assert_eq!(to_logical(1234, 1.0), 1234);
        // A 200% target halves the physical distance.
        assert_eq!(to_logical(2000, 2.0), 1000);
    }
}
