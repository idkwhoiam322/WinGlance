use crate::winutil::{clear_window_state, set_window_state, wide, window_state};
use log::warn;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{BOOL, COLORREF, CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BDR_SUNKENOUTER, BF_RECT, BeginPaint, CreateFontW, CreateSolidBrush, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawEdge, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    HBRUSH, HFONT, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, PAINTSTRUCT, SelectObject,
    SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows, GWL_EXSTYLE, GetClientRect, GetParent,
    GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId, HWND_TOPMOST, IsIconic, IsWindowVisible, LB_ADDSTRING,
    LB_GETCOUNT, LB_GETITEMDATA, LB_GETITEMRECT, LB_GETTOPINDEX, LB_SETCURSEL, LB_SETITEMDATA, LB_SETITEMHEIGHT,
    LBS_HASSTRINGS, LBS_NOINTEGRALHEIGHT, LBS_OWNERDRAWFIXED, LoadCursorW, PostMessageW, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SendMessageW, SetCursor, SetWindowPos, ShowWindow, WINDOW_STYLE, WM_APP,
    WM_COMMAND, WM_CREATE, WM_DESTROY, WM_DRAWITEM, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_NCCREATE,
    WM_NCDESTROY, WM_PAINT, WM_SETFONT, WS_BORDER, WS_CHILD, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::PCWSTR;

const CLASS_NAME: &str = "WinGlanceProcessPicker";
const WIDTH: i32 = 400;
const HEADER_H: i32 = 30;
const ROW_HEIGHT: i32 = 22;
const MAX_VISIBLE: usize = 12;
const WS_EX_TOOLWINDOW_STYLE: i32 = 0x80;
const CLOSE_BTN_SIZE: i32 = 20;
const BST_CHECKED: usize = 1;
const BST_UNCHECKED: usize = 0;
/// Checkbox square size in pixels.
const CB_SIZE: i32 = 13;

pub(crate) const PICKER_RESULT_MSG: u32 = WM_APP + 7;

/// Identifier for the listbox's Comctl32 subclass registration.
const LISTBOX_SUBCLASS_ID: usize = 1;

pub(crate) struct ProcessEntry {
    pub display_name: String,
    pub pattern: String,
}

struct PickerState {
    list: Vec<ProcessEntry>,
    listbox: HWND,
    close_hover: bool,
    last_click_item: Option<usize>,
    last_click_time: Option<Instant>,
    /// DPI scale of the picker window at open time; all geometry, row
    /// heights and hit-testing are scaled by it so the picker matches the
    /// DPI-correct main window on high-DPI displays.
    scale: f32,
    /// Fixed GDI objects for the picker's own chrome (header text + fills,
    /// listbox rows), created once per open and freed at teardown so paints
    /// create nothing.
    header_font: HFONT,
    list_font: HFONT,
    header_brush: HBRUSH,
    close_brush: HBRUSH,
    close_hover_brush: HBRUSH,
    /// Owner-draw row background brushes, created once per open instead of
    /// per painted row (every scroll tick repaints the visible rows).
    row_brush: HBRUSH,
    row_selected_brush: HBRUSH,
    /// Shared slot for the confirmed allow-list patterns. The picker writes
    /// the result here and posts a bare `PICKER_RESULT_MSG`; the main window
    /// reads the slot. No pointers ever cross the message boundary.
    result: Arc<Mutex<Option<Vec<String>>>>,
}

static OPEN_PICKER: OnceLock<Mutex<Option<isize>>> = OnceLock::new();

/// Set when this window's WM_NCCREATE claims the state box handed over in
/// `lpCreateParams`, so a failed CreateWindowExW can tell whether the box was
/// taken by the system (and freed in WM_NCDESTROY) or still belongs to the
/// caller. Reset before each open; window creation is single-threaded on the
/// UI thread, so a plain atomic flag is race-free.
static PICKER_STATE_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Guards class registration: registering twice would leak the class brush.
static CLASS_REGISTERED: OnceLock<()> = OnceLock::new();

fn get_open_picker() -> &'static Mutex<Option<isize>> {
    OPEN_PICKER.get_or_init(|| Mutex::new(None))
}

fn close_btn_rect(client: &RECT, scale: f32) -> RECT {
    let size = (CLOSE_BTN_SIZE as f32 * scale).round() as i32;
    let pad = (4.0 * scale).round() as i32;
    RECT {
        left: client.right - size - pad,
        top: pad,
        right: client.right - pad,
        bottom: pad + size,
    }
}

fn register_class(instance: HINSTANCE) -> bool {
    crate::winutil::register_class_once(
        &CLASS_REGISTERED,
        instance,
        &wide(CLASS_NAME),
        Some(picker_proc),
        || Some(unsafe { CreateSolidBrush(COLORREF(0x001E1E1E)) }),
        "the process picker window",
    )
    .is_ok()
}

/// RAII guard for the Toolhelp process snapshot handle. The kernel handle
/// must reach `CloseHandle` on every exit path (enumeration failure, early
/// return); wrapping it means a future early return cannot reintroduce the
/// leak.
struct SnapshotGuard(HANDLE);

impl Drop for SnapshotGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Collects every process's executable name in one Toolhelp snapshot, so the
/// window scan (which visits hundreds of windows) does not take a snapshot per
/// window.
fn process_names() -> HashMap<u32, String> {
    let mut names = HashMap::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return names;
        };
        let snapshot = SnapshotGuard(snapshot);
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot.0, &mut entry).is_ok() {
            loop {
                names.insert(
                    entry.th32ProcessID,
                    String::from_utf16_lossy(&entry.szExeFile)
                        .trim_end_matches('\0')
                        .to_string(),
                );
                if !Process32NextW(snapshot.0, &mut entry).is_ok() {
                    break;
                }
            }
        }
    }
    names
}

/// Scan state threaded through the EnumWindows callback: the accumulated
/// window entries and the prebuilt pid → exe-name map.
struct WindowScan {
    found: Vec<(u32, ProcessEntry)>,
    exe_by_pid: HashMap<u32, String>,
}

pub(crate) fn enumerate_app_processes() -> Vec<ProcessEntry> {
    let exe_by_pid = process_names();
    let mut scan = WindowScan {
        found: Vec::new(),
        exe_by_pid,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_windows_proc), LPARAM(&mut scan as *mut _ as isize));
    }

    let our_pid = unsafe { GetCurrentProcessId() };
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for (pid, entry) in scan.found {
        if pid == our_pid || !seen.insert(pid) {
            continue;
        }
        entries.push(entry);
    }
    entries.sort_by_key(|a| a.display_name.to_lowercase());
    entries
}

/// Appends every currently open SMTC session source that the window scan did
/// not already find (tray-only apps and background browser tabs have no
/// visible window, so the session list is the only way to surface them).
fn merge_smtc_sources(mut entries: Vec<ProcessEntry>) -> Vec<ProcessEntry> {
    let sources = crate::smtc::active_session_sources();
    if sources.is_empty() {
        return entries;
    }
    let mut seen: std::collections::HashSet<String> = entries.iter().map(|e| normalize_pattern(&e.pattern)).collect();
    for source in sources {
        if normalize_pattern(&source).is_empty() || !seen.insert(normalize_pattern(&source)) {
            continue;
        }
        entries.push(ProcessEntry {
            display_name: pretty_source_label(&source),
            pattern: source,
        });
    }
    entries.sort_by_key(|a| a.display_name.to_lowercase());
    entries
}

/// Same normalization the SMTC worker uses when matching allow-list patterns
/// against AUMIDs, so picker pre-checking agrees with session filtering.
fn normalize_pattern(value: &str) -> String {
    crate::smtc::normalize_for_match(value)
}

/// Turns a session source label like `"youtube-music"` into a readable entry
/// title like `"Youtube Music"` for the picker list.
fn pretty_source_label(value: &str) -> String {
    value
        .split(['-', '_', '.'])
        .filter(|w| !w.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = unsafe { &mut *(lparam.0 as *mut WindowScan) };

    if !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return BOOL(1);
    }
    if unsafe { IsIconic(hwnd).as_bool() } {
        return BOOL(1);
    }

    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    if ex_style & WS_EX_TOOLWINDOW_STYLE as isize != 0 {
        return BOOL(1);
    }

    let mut title_buf = [0u16; 256];
    let len = unsafe { GetWindowTextW(hwnd, &mut title_buf) };
    if len <= 0 {
        return BOOL(1);
    }

    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return BOOL(1);
    }

    let Some(exe_name) = scan.exe_by_pid.get(&pid) else {
        return BOOL(1);
    };

    let title = String::from_utf16_lossy(&title_buf[..len as usize]);
    let title = title.trim().to_string();
    if title.is_empty() {
        return BOOL(1);
    }

    let pattern = exe_name
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .to_lowercase();

    scan.found.push((
        pid,
        ProcessEntry {
            display_name: title,
            pattern,
        },
    ));
    BOOL(1)
}

pub(crate) fn close_existing() {
    crate::winutil::close_registered(&OPEN_PICKER, |slot| match *slot {
        Some(hwnd) if hwnd != 0 => Some(HWND(hwnd as *mut std::ffi::c_void)),
        _ => None,
    });
}

pub(crate) fn open(
    owner: HWND,
    trigger_rect: &RECT,
    current: &[String],
    result: Arc<Mutex<Option<Vec<String>>>>,
) -> bool {
    let list = merge_smtc_sources(enumerate_app_processes());
    if list.is_empty() {
        warn!("no app processes or SMTC sessions found for picker");
        return false;
    }

    unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(h) => HINSTANCE(h.0),
            Err(_) => return false,
        };
        if !register_class(instance) {
            return false;
        }
        close_existing();

        let item_count = list.len().min(MAX_VISIBLE);
        // The picker is positioned over the owner's control: create it at the
        // owner's DPI, then re-read the window's own DPI once it exists (they
        // agree in practice) so every geometry and hit-test below uses the
        // authoritative scale.
        let owner_scale = GetDpiForWindow(owner).max(96) as f32 / 96.0;
        let height = ((HEADER_H + item_count as i32 * ROW_HEIGHT + 10) as f32 * owner_scale).round() as i32;
        let width = (WIDTH as f32 * owner_scale).round() as i32;

        // Clamp the popup to the owner monitor's work area: the trigger
        // control can sit near the bottom edge, where an unclamped
        // `trigger_rect.bottom + 4` would push the popup under the taskbar
        // or off-screen. Prefer below the control, flip above when that
        // would overflow, then clamp into the work area.
        let (mut x, mut y) = (trigger_rect.left, trigger_rect.bottom + 4);
        let monitor = MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let work = info.rcWork;
            if y + height > work.bottom {
                y = (trigger_rect.top - 4 - height).max(work.top);
            }
            x = x.clamp(work.left, (work.right - width).max(work.left));
            y = y.clamp(work.top, (work.bottom - height).max(work.top));
        }

        // Pre-check with the same normalization the SMTC worker applies to
        // allow-list patterns, so a stored "youtube music" matches the
        // session-derived "youtube-music" entry.
        let norm_current: Vec<String> = current
            .iter()
            .map(|p| normalize_pattern(p))
            .filter(|n| !n.is_empty())
            .collect();
        let checked: Vec<bool> = list
            .iter()
            .map(|e| {
                let ne = normalize_pattern(&e.pattern);
                norm_current.iter().any(|n| ne.contains(n.as_str()) || n.contains(&ne))
            })
            .collect();

        let state = Box::new(PickerState {
            list,
            listbox: HWND::default(),
            close_hover: false,
            last_click_item: None,
            last_click_time: None,
            scale: 1.0,
            header_font: HFONT::default(),
            list_font: HFONT::default(),
            header_brush: HBRUSH::default(),
            close_brush: HBRUSH::default(),
            close_hover_brush: HBRUSH::default(),
            row_brush: HBRUSH::default(),
            row_selected_brush: HBRUSH::default(),
            result,
        });
        let state_ptr = Box::into_raw(state);
        PICKER_STATE_CLAIMED.store(false, Ordering::SeqCst);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(wide(CLASS_NAME).as_ptr()),
            PCWSTR(wide("Select apps").as_ptr()),
            WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN,
            x,
            y,
            width,
            height,
            owner,
            None,
            instance,
            Some(state_ptr.cast()),
        );

        let hwnd = match hwnd {
            Ok(hwnd) => hwnd,
            Err(_) => {
                // The state box is owned by the window from WM_NCCREATE onward
                // and freed in WM_NCDESTROY. WM_NCCREATE flips
                // PICKER_STATE_CLAIMED when it takes the box; if it never ran,
                // the box still belongs to us and must be freed here.
                if !PICKER_STATE_CLAIMED.load(Ordering::SeqCst) {
                    drop(Box::from_raw(state_ptr));
                }
                return false;
            }
        };

        if let Ok(mut guard) = get_open_picker().lock() {
            *guard = Some(hwnd.0 as isize);
        }

        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        // Fixed GDI objects for the picker's own chrome, created once per open
        // (WM_PAINT only reads them) and freed in WM_NCDESTROY.
        let state_ref = &mut *state_ptr;
        state_ref.scale = scale;
        state_ref.header_font = CreateFontW(
            -((14.0 * scale).round() as i32),
            0,
            0,
            0,
            700,
            0,
            0,
            0,
            0x01,
            0,
            0,
            0x02,
            0x00,
            PCWSTR(wide("Segoe UI Semibold").as_ptr()),
        );
        // Listbox font: same Segoe UI metrics the global cache used (13 px,
        // regular weight, quality 0x02) but owned by the picker. The global
        // cache flushes its handles on DPI change, which would leave the
        // listbox with a dangling HFONT.
        state_ref.list_font = CreateFontW(
            -((13.0 * scale).round() as i32).max(1),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            0x01,
            0,
            0,
            0x02,
            0x00,
            PCWSTR(wide("Segoe UI").as_ptr()),
        );
        state_ref.header_brush = CreateSolidBrush(COLORREF(0x002D2D2D));
        state_ref.close_brush = CreateSolidBrush(COLORREF(0x00333333));
        state_ref.close_hover_brush = CreateSolidBrush(COLORREF(0x00404040));
        state_ref.row_brush = CreateSolidBrush(COLORREF(0x001E1E1E));
        state_ref.row_selected_brush = CreateSolidBrush(COLORREF(0x003D3D3D));
        let phys_w = (WIDTH as f32 * scale).round() as i32;
        let phys_h = ((HEADER_H + item_count as i32 * ROW_HEIGHT + 10) as f32 * scale).round() as i32;
        // Reuse the clamped x/y from above: an unclamped reposition here
        // would put the popup back under the taskbar or off-screen despite
        // the initial creation being clamped (the owner and picker DPI agree
        // in practice, so the owner-scale clamp stays valid).
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            phys_w,
            phys_h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );

        let lb = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
            PCWSTR(wide("LISTBOX").as_ptr()),
            PCWSTR::null(),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WS_CLIPCHILDREN
                | WS_BORDER
                | WINDOW_STYLE((LBS_OWNERDRAWFIXED | LBS_HASSTRINGS | LBS_NOINTEGRALHEIGHT) as u32),
            0,
            (HEADER_H as f32 * scale).round() as i32,
            phys_w,
            ((item_count as i32 * ROW_HEIGHT) as f32 * scale).round() as i32,
            hwnd,
            None,
            instance,
            None,
        );

        let lb = match lb {
            Ok(lb) => lb,
            Err(_) => {
                // Without the listbox the popup cannot show anything usable;
                // tear it down instead of leaving a dead frame on screen.
                warn!("creating the picker listbox failed; closing the picker");
                let _ = DestroyWindow(hwnd);
                return false;
            }
        };
        {
            let state_ref = &mut *state_ptr;
            state_ref.listbox = lb;
            state_ref.scale = scale;

            // Route mouse and keyboard messages from the listbox child through
            // our own subclass proc; DefSubclassProc forwards every message we
            // do not consume. Comctl32 tracks the original proc internally, so no
            // GWLP_WNDPROC swap or stored original proc is needed. The parent
            // (picker) HWND is carried in the subclass ref data; PickerState is
            // read from that window's GWLP_USERDATA and the subclass is unhooked
            // on WM_NCDESTROY.
            let _ = SetWindowSubclass(lb, Some(listbox_proc), LISTBOX_SUBCLASS_ID, hwnd.0 as usize);

            let row_h = (ROW_HEIGHT as f32 * scale).round() as i32;
            let _ = SendMessageW(lb, LB_SETITEMHEIGHT, WPARAM(0), LPARAM(row_h as isize));
            let font = state_ref.list_font;
            let _ = SendMessageW(lb, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));

            for (i, entry) in state_ref.list.iter().enumerate() {
                let text = wide(&entry.display_name);
                let idx = SendMessageW(lb, LB_ADDSTRING, WPARAM(0), LPARAM(text.as_ptr() as isize));
                let state_val = if checked[i] { BST_CHECKED } else { BST_UNCHECKED };
                let _ = SendMessageW(lb, LB_SETITEMDATA, WPARAM(idx.0 as usize), LPARAM(state_val as isize));
            }
        }

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        true
    }
}

fn read_checked(hwnd: HWND, lb: HWND) -> Vec<String> {
    let count = unsafe { SendMessageW(lb, LB_GETCOUNT, WPARAM(0), LPARAM(0)) };
    if count.0 <= 0 {
        return Vec::new();
    }
    let state_ptr = window_state::<PickerState>(hwnd);
    if state_ptr.is_null() {
        return Vec::new();
    }
    let state = unsafe { &*state_ptr };
    let mut result = Vec::new();
    for i in 0..(count.0 as usize) {
        let data = unsafe { SendMessageW(lb, LB_GETITEMDATA, WPARAM(i), LPARAM(0)) };
        if data.0 as usize == BST_CHECKED
            && let Some(entry) = state.list.get(i)
        {
            result.push(entry.pattern.clone());
        }
    }
    result
}

fn post_result(hwnd: HWND, cancelled: bool) {
    let state_ptr = window_state::<PickerState>(hwnd);
    if state_ptr.is_null() {
        return;
    }
    let state = unsafe { &*state_ptr };
    let owner = unsafe { GetParent(hwnd).unwrap_or_default() };

    // The selected patterns travel through the shared result slot, never as a
    // pointer in the message. The main window takes the slot on
    // PICKER_RESULT_MSG; if the post fails the slot is simply never read and
    // the next picker open overwrites it.
    if let Ok(mut slot) = state.result.lock() {
        *slot = if cancelled {
            None
        } else {
            Some(read_checked(hwnd, state.listbox))
        };
    }

    if unsafe { PostMessageW(owner, PICKER_RESULT_MSG, WPARAM(0), LPARAM(0)) }.is_err() {
        warn!("posting the picker result failed");
    }
}

fn hit_test_close(hwnd: HWND, x: i32, y: i32) -> bool {
    let mut client = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut client) };
    let scale = unsafe { GetDpiForWindow(hwnd).max(96) as f32 / 96.0 };
    let r = close_btn_rect(&client, scale);
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// Comctl32 subclass proc for the picker's listbox. Mouse and keyboard
/// messages are delivered to the listbox child rather than the picker window,
/// so click-to-toggle and double-click-to-confirm are handled here. The parent
/// (picker) HWND is carried in `ref_data`; PickerState is read from that
/// window's GWLP_USERDATA. Every message we do not consume is forwarded via
/// DefSubclassProc, which dispatches to the original listbox proc that Comctl32
/// tracks internally — no GWLP_WNDPROC swap or stored original proc is needed.
/// The subclass is unhooked on WM_NCDESTROY.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn listbox_proc(
    lb: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    ref_data: usize,
) -> LRESULT {
    let parent = HWND(ref_data as *mut std::ffi::c_void);
    match message {
        WM_NCDESTROY => {
            // Unhook cleanly before deflecting the rest of destruction.
            let _ = unsafe { RemoveWindowSubclass(lb, Some(listbox_proc), LISTBOX_SUBCLASS_ID) };
            unsafe { DefSubclassProc(lb, message, wparam, lparam) }
        }
        WM_LBUTTONDOWN => {
            let state_ptr = window_state::<PickerState>(parent);
            if !state_ptr.is_null() {
                let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
                // The row height is DPI-scaled like the listbox item height,
                // so hit-testing matches the rendered rows on any display.
                // The listbox scrolls (only MAX_VISIBLE rows fit), so the
                // clicked client row is relative to the top index.
                let scale = unsafe { (*state_ptr).scale };
                let row_h = (ROW_HEIGHT as f32 * scale).round() as i32;
                let top = unsafe { SendMessageW(lb, LB_GETTOPINDEX, WPARAM(0), LPARAM(0)) }.0 as i32;
                let item_idx = top + y / row_h.max(1);
                let count = unsafe { SendMessageW(lb, LB_GETCOUNT, WPARAM(0), LPARAM(0)) }.0 as i32;
                if item_idx >= 0 && item_idx < count {
                    let i = item_idx as usize;
                    let state = unsafe { &mut *state_ptr };

                    // Double-click on the same item within 400ms confirms and
                    // closes, applying the state left by the first click.
                    let now = Instant::now();
                    let is_double = state.last_click_item == Some(i)
                        && state
                            .last_click_time
                            .is_some_and(|t| t.elapsed() < Duration::from_millis(400));
                    state.last_click_item = Some(i);
                    state.last_click_time = Some(now);

                    if is_double {
                        post_result(parent, false);
                        let _ = unsafe { DestroyWindow(parent) };
                        return LRESULT(0);
                    }

                    // Single click: toggle the checkbox and repaint the row.
                    let data = unsafe { SendMessageW(lb, LB_GETITEMDATA, WPARAM(i), LPARAM(0)) };
                    let toggled = if data.0 as usize == BST_CHECKED {
                        BST_UNCHECKED
                    } else {
                        BST_CHECKED
                    };
                    let _ = unsafe { SendMessageW(lb, LB_SETITEMDATA, WPARAM(i), LPARAM(toggled as isize)) };
                    let _ = unsafe { SendMessageW(lb, LB_SETCURSEL, WPARAM(i), LPARAM(0)) };
                    let mut item_rect = RECT::default();
                    let _ = unsafe {
                        SendMessageW(
                            lb,
                            LB_GETITEMRECT,
                            WPARAM(i),
                            LPARAM(&mut item_rect as *mut RECT as isize),
                        )
                    };
                    unsafe {
                        let _ = InvalidateRect(lb, Some(&item_rect), false);
                    }
                    return LRESULT(0);
                }
            }
            unsafe { DefSubclassProc(lb, message, wparam, lparam) }
        }
        WM_KEYDOWN => {
            // The listbox takes keyboard focus when clicked, so Enter/Esc must
            // work here as well as on the picker window itself.
            let key = wparam.0 as u16;
            if key == VK_ESCAPE.0 {
                post_result(parent, true);
                let _ = unsafe { DestroyWindow(parent) };
                return LRESULT(0);
            }
            if key == 0x0D {
                post_result(parent, false);
                let _ = unsafe { DestroyWindow(parent) };
                return LRESULT(0);
            }
            unsafe { DefSubclassProc(lb, message, wparam, lparam) }
        }
        _ => unsafe { DefSubclassProc(lb, message, wparam, lparam) },
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn picker_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = lparam.0 as *const CREATESTRUCTW;
            if !create.is_null() {
                let state = (*create).lpCreateParams as *mut PickerState;
                set_window_state(hwnd, state);
                PICKER_STATE_CLAIMED.store(true, Ordering::SeqCst);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_CREATE => LRESULT(0),
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };

            let state_ptr = window_state::<PickerState>(hwnd);
            let scale = if state_ptr.is_null() {
                unsafe { GetDpiForWindow(hwnd).max(96) as f32 / 96.0 }
            } else {
                unsafe { (*state_ptr).scale }
            };
            let header_h = (HEADER_H as f32 * scale).round() as i32;
            let close_size = (CLOSE_BTN_SIZE as f32 * scale).round() as i32;
            let pad6 = (6.0 * scale) as i32;
            let pad12 = (12.0 * scale) as i32;

            let mut client = RECT::default();
            let _ = unsafe { GetClientRect(hwnd, &mut client) };

            // Chrome GDI objects are cached on the state; the null-state path
            // (window already tearing down) falls back to transient ones that
            // are deleted at the end of this paint.
            let (header_font, header_brush, close_brush, close_hover_brush, transient) = if state_ptr.is_null() {
                let font = unsafe {
                    CreateFontW(
                        -((14.0 * scale).round() as i32),
                        0,
                        0,
                        0,
                        700,
                        0,
                        0,
                        0,
                        0x01,
                        0,
                        0,
                        0x02,
                        0x00,
                        PCWSTR(wide("Segoe UI Semibold").as_ptr()),
                    )
                };
                (
                    font,
                    unsafe { CreateSolidBrush(COLORREF(0x002D2D2D)) },
                    unsafe { CreateSolidBrush(COLORREF(0x00333333)) },
                    unsafe { CreateSolidBrush(COLORREF(0x00404040)) },
                    true,
                )
            } else {
                let state = unsafe { &*state_ptr };
                (
                    state.header_font,
                    state.header_brush,
                    state.close_brush,
                    state.close_hover_brush,
                    false,
                )
            };

            let _ = unsafe {
                FillRect(
                    hdc,
                    &RECT {
                        left: client.left,
                        top: client.top,
                        right: client.right,
                        bottom: client.top + header_h,
                    },
                    header_brush,
                )
            };

            let old_font = if header_font.0.is_null() {
                None
            } else {
                Some(unsafe { SelectObject(hdc, header_font) })
            };
            let _ = unsafe { SetBkMode(hdc, TRANSPARENT) };
            let _ = unsafe { SetTextColor(hdc, COLORREF(0x00F0F0F0)) };
            let mut title = wide("Select apps (Enter=apply, Esc=cancel)");
            let _ = unsafe {
                DrawTextW(
                    hdc,
                    &mut title,
                    &mut RECT {
                        left: client.left + pad12,
                        top: client.top + pad6,
                        right: client.right - pad12 - close_size,
                        bottom: client.top + header_h - pad6,
                    },
                    DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_END_ELLIPSIS | DT_NOPREFIX,
                )
            };

            // Draw close button (X)
            let btn = close_btn_rect(&client, scale);
            let hover = !state_ptr.is_null() && unsafe { (*state_ptr).close_hover };
            let _ = unsafe { FillRect(hdc, &btn, if hover { close_hover_brush } else { close_brush }) };

            let _ = unsafe { SetTextColor(hdc, COLORREF(0x00F0F0F0)) };
            let mut x_text = wide("\u{00D7}");
            let _ = unsafe {
                DrawTextW(
                    hdc,
                    &mut x_text,
                    &mut RECT {
                        left: btn.left,
                        top: btn.top,
                        right: btn.right,
                        bottom: btn.bottom,
                    },
                    DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
                )
            };

            if let Some(old_font) = old_font {
                let _ = unsafe { SelectObject(hdc, old_font) };
            }
            if transient {
                let _ = unsafe { DeleteObject(header_font) };
                let _ = unsafe { DeleteObject(header_brush) };
                let _ = unsafe { DeleteObject(close_brush) };
                let _ = unsafe { DeleteObject(close_hover_brush) };
            }

            let _ = unsafe { EndPaint(hwnd, &ps) };
            LRESULT(0)
        }
        WM_DRAWITEM => {
            let draw_ptr = lparam.0 as *const DRAWITEMSTRUCT;
            if draw_ptr.is_null() {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            }
            let draw = unsafe { &*draw_ptr };
            let state_ptr = window_state::<PickerState>(hwnd);
            if state_ptr.is_null() || draw.itemID as usize >= unsafe { (*state_ptr).list.len() } {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            }
            let state = unsafe { &*state_ptr };
            let entry = &state.list[draw.itemID as usize];
            let scale = state.scale;
            unsafe {
                // Background: highlight selected items. Brushes are cached on
                // the state; every scroll tick repaints the visible rows, so
                // per-row CreateSolidBrush/DeleteObject would churn GDI.
                let bg_brush = if draw.itemState.0 & ODS_SELECTED.0 != 0 {
                    state.row_selected_brush
                } else {
                    state.row_brush
                };
                FillRect(draw.hDC, &draw.rcItem, bg_brush);

                // Checkbox square on the left.
                let mid = (draw.rcItem.top + draw.rcItem.bottom) / 2;
                let cb_size = (CB_SIZE as f32 * scale).round() as i32;
                let pad6 = (6.0 * scale) as i32;
                let mut cb = RECT {
                    left: draw.rcItem.left + pad6,
                    top: mid - cb_size / 2,
                    right: draw.rcItem.left + pad6 + cb_size,
                    bottom: mid - cb_size / 2 + cb_size,
                };
                let _ = DrawEdge(draw.hDC, &mut cb, BDR_SUNKENOUTER, BF_RECT);
                if draw.itemData == BST_CHECKED {
                    SetTextColor(draw.hDC, COLORREF(0x00F0F0F0));
                    SetBkMode(draw.hDC, TRANSPARENT);
                    let mut tick = wide("X");
                    DrawTextW(
                        draw.hDC,
                        &mut tick,
                        &mut RECT {
                            left: cb.left,
                            top: cb.top,
                            right: cb.right,
                            bottom: cb.bottom,
                        },
                        DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX,
                    );
                }

                // Entry text.
                SetTextColor(draw.hDC, COLORREF(0x00F0F0F0));
                SetBkMode(draw.hDC, TRANSPARENT);
                let mut name = wide(&entry.display_name);
                DrawTextW(
                    draw.hDC,
                    &mut name,
                    &mut RECT {
                        left: draw.rcItem.left + (24.0 * scale) as i32,
                        right: draw.rcItem.right - (4.0 * scale) as i32,
                        top: draw.rcItem.top,
                        bottom: draw.rcItem.bottom,
                    },
                    DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let state_ptr = window_state::<PickerState>(hwnd);
            if state_ptr.is_null() {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            }
            let hovering = hit_test_close(hwnd, x, y);
            let was_hover = unsafe { (*state_ptr).close_hover };
            if hovering != was_hover {
                unsafe { (*state_ptr).close_hover = hovering };
                unsafe {
                    let mut client = RECT::default();
                    let _ = GetClientRect(hwnd, &mut client);
                    let scale = (*state_ptr).scale;
                    let btn = close_btn_rect(&client, scale);
                    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(hwnd, Some(&btn), false);
                };
                // Change cursor; a failed load (broken system resources) just
                // keeps the current cursor instead of panicking the picker.
                if let Ok(cursor) = unsafe { LoadCursorW(None, windows::Win32::UI::WindowsAndMessaging::IDC_HAND) } {
                    unsafe {
                        SetCursor(cursor);
                    }
                } else {
                    warn!("LoadCursorW(IDC_HAND) failed; keeping the default cursor");
                }
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;

            if hit_test_close(hwnd, x, y) {
                post_result(hwnd, true);
                let _ = unsafe { DestroyWindow(hwnd) };
                return LRESULT(0);
            }

            // Clicks inside the listbox are delivered to the listbox and
            // handled by its subclassed proc; this handler only covers the
            // picker's own surface (header and padding). Clicks outside the
            // picker dismiss it.
            let mut client = RECT::default();
            let _ = unsafe { GetClientRect(hwnd, &mut client) };
            if x < client.left || x >= client.right || y < client.top || y >= client.bottom {
                post_result(hwnd, true);
                let _ = unsafe { DestroyWindow(hwnd) };
                return LRESULT(0);
            }
            LRESULT(0)
        }
        WM_COMMAND => DefWindowProcW(hwnd, message, wparam, lparam),
        WM_KEYDOWN => {
            if wparam.0 as u16 == VK_ESCAPE.0 {
                post_result(hwnd, true);
                let _ = unsafe { DestroyWindow(hwnd) };
                return LRESULT(0);
            }
            if wparam.0 as u16 == 0x0D {
                post_result(hwnd, false);
                let _ = unsafe { DestroyWindow(hwnd) };
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_DESTROY => {
            if let Ok(mut guard) = get_open_picker().lock() {
                *guard = None;
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // Free the heap-allocated PickerState stashed at WM_NCCREATE, and
            // its fixed chrome GDI objects. Every close path
            // (Escape/Enter/click-outside) goes through DestroyWindow; without
            // this the state leaked on each open.
            let state_ptr = window_state::<PickerState>(hwnd);
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let _ = unsafe { DeleteObject(state.header_font) };
                let _ = unsafe { DeleteObject(state.list_font) };
                let _ = unsafe { DeleteObject(state.header_brush) };
                let _ = unsafe { DeleteObject(state.close_brush) };
                let _ = unsafe { DeleteObject(state.close_hover_brush) };
                let _ = unsafe { DeleteObject(state.row_brush) };
                let _ = unsafe { DeleteObject(state.row_selected_brush) };
                drop(Box::from_raw(state_ptr));
            }
            clear_window_state(hwnd);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}
