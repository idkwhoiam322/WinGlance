use crate::overlay::wide;
use log::warn;
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::{BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE,
    DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FillRect, PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows, GWL_EXSTYLE, GWLP_USERDATA,
    GetClientRect, GetParent, GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId, HWND_TOPMOST, IDC_ARROW,
    IsIconic, IsWindowVisible, LB_ADDSTRING, LB_GETCOUNT, LB_GETITEMDATA, LB_SETITEMDATA, LB_SETITEMHEIGHT,
    LBS_HASSTRINGS, LBS_NOINTEGRALHEIGHT, LoadCursorW, PostMessageW, RegisterClassExW, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SendMessageW, SetCursor, SetWindowLongPtrW, SetWindowPos, ShowWindow, WINDOW_STYLE,
    WM_APP, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_NCCREATE, WM_PAINT,
    WM_SETFONT, WNDCLASS_STYLES, WNDCLASSEXW, WS_BORDER, WS_CHILD, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::PCWSTR;

const CLASS_NAME: &str = "NotchProcessPicker";
const WIDTH: i32 = 400;
const HEADER_H: i32 = 30;
const ROW_HEIGHT: i32 = 22;
const MAX_VISIBLE: usize = 12;
const WS_EX_TOOLWINDOW_STYLE: i32 = 0x80;
const CLOSE_BTN_SIZE: i32 = 20;
const BST_CHECKED: usize = 1;
const BST_UNCHECKED: usize = 0;

pub(crate) const PICKER_RESULT_MSG: u32 = WM_APP + 7;

pub(crate) struct ProcessEntry {
    pub display_name: String,
    pub pattern: String,
}

struct PickerState {
    list: Vec<ProcessEntry>,
    listbox: HWND,
    close_hover: bool,
}

static OPEN_PICKER: OnceLock<Mutex<Option<isize>>> = OnceLock::new();

fn get_open_picker() -> &'static Mutex<Option<isize>> {
    OPEN_PICKER.get_or_init(|| Mutex::new(None))
}

fn close_btn_rect(client: &RECT) -> RECT {
    RECT {
        left: client.right - CLOSE_BTN_SIZE - 4,
        top: 4,
        right: client.right - 4,
        bottom: 4 + CLOSE_BTN_SIZE,
    }
}

fn register_class(instance: HINSTANCE) {
    unsafe {
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(picker_proc),
            hInstance: instance,
            hCursor: cursor,
            lpszClassName: PCWSTR(wide(CLASS_NAME).as_ptr()),
            hbrBackground: CreateSolidBrush(COLORREF(0x001E1E1E)),
            ..Default::default()
        };
        let _ = RegisterClassExW(&class);
    }
}

pub(crate) fn enumerate_app_processes() -> Vec<ProcessEntry> {
    let mut found: Vec<(u32, ProcessEntry)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_windows_proc), LPARAM(&mut found as *mut _ as isize));
    }

    let our_pid = unsafe { GetCurrentProcessId() };
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for (pid, entry) in found {
        if pid == our_pid || !seen.insert(pid) {
            continue;
        }
        entries.push(entry);
    }
    entries.sort_by_key(|a| a.display_name.to_lowercase());
    entries
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let found = unsafe { &mut *(lparam.0 as *mut Vec<(u32, ProcessEntry)>) };

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

    let exe_name = process_name_for_pid(pid);
    if exe_name.is_empty() {
        return BOOL(1);
    }

    let title = String::from_utf16_lossy(&title_buf[..len as usize]);
    let title = title.trim().to_string();
    if title.is_empty() {
        return BOOL(1);
    }

    let pattern = exe_name
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .to_lowercase();

    found.push((
        pid,
        ProcessEntry {
            display_name: title,
            pattern,
        },
    ));
    BOOL(1)
}

fn process_name_for_pid(pid: u32) -> String {
    unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, pid) {
            Ok(h) => h,
            Err(_) => return String::new(),
        };

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        if !Process32FirstW(snapshot, &mut entry).is_ok() {
            return String::new();
        }

        loop {
            if entry.th32ProcessID == pid {
                return String::from_utf16_lossy(&entry.szExeFile)
                    .trim_end_matches('\0')
                    .to_string();
            }
            if !Process32NextW(snapshot, &mut entry).is_ok() {
                break;
            }
        }

        String::new()
    }
}

pub(crate) fn close_existing() {
    let Some(m) = OPEN_PICKER.get() else { return };
    if let Ok(guard) = m.lock()
        && let Some(hwnd_val) = *guard
        && hwnd_val != 0
    {
        unsafe {
            let _ = DestroyWindow(HWND(hwnd_val as *mut std::ffi::c_void));
        }
    }
}

pub(crate) fn open(owner: HWND, trigger_rect: &RECT, current: &[String]) -> bool {
    let list = enumerate_app_processes();
    if list.is_empty() {
        warn!("no visible app processes found for picker");
        return false;
    }

    unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(h) => HINSTANCE(h.0),
            Err(_) => return false,
        };
        register_class(instance);
        close_existing();

        let item_count = list.len().min(MAX_VISIBLE);
        let height = HEADER_H + item_count as i32 * ROW_HEIGHT + 10;

        let checked: Vec<bool> = list
            .iter()
            .map(|e| current.iter().any(|p| e.pattern.contains(p) || p.contains(&e.pattern)))
            .collect();

        let state = Box::new(PickerState {
            list,
            listbox: HWND::default(),
            close_hover: false,
        });
        let state_ptr = Box::into_raw(state);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(wide(CLASS_NAME).as_ptr()),
            PCWSTR(wide("Select apps").as_ptr()),
            WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN,
            trigger_rect.left,
            trigger_rect.bottom + 4,
            WIDTH,
            height,
            owner,
            None,
            instance,
            Some(state_ptr.cast()),
        );

        let hwnd = match hwnd {
            Ok(hwnd) => hwnd,
            Err(_) => {
                drop(Box::from_raw(state_ptr));
                return false;
            }
        };

        if let Ok(mut guard) = get_open_picker().lock() {
            *guard = Some(hwnd.0 as isize);
        }

        let scale = GetDpiForWindow(owner).max(96) as f32 / 96.0;
        let phys_w = (WIDTH as f32 * scale).round() as i32;
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            trigger_rect.left,
            trigger_rect.bottom + (4.0 * scale) as i32,
            phys_w,
            height,
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
                | WINDOW_STYLE((LBS_HASSTRINGS | LBS_NOINTEGRALHEIGHT | 0x0080) as u32),
            0,
            HEADER_H,
            phys_w,
            item_count as i32 * ROW_HEIGHT,
            hwnd,
            None,
            instance,
            None,
        );

        if let Ok(lb) = lb {
            let state_ref = &mut *state_ptr;
            state_ref.listbox = lb;

            let _ = SendMessageW(lb, LB_SETITEMHEIGHT, WPARAM(0), LPARAM(ROW_HEIGHT as isize));
            let font = CreateFontW(
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
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PickerState };
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
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PickerState };
    if state_ptr.is_null() {
        return;
    }
    let state = unsafe { &*state_ptr };
    let owner = unsafe { GetParent(hwnd).unwrap_or_default() };

    let lparam = if cancelled {
        0
    } else {
        let selected = read_checked(hwnd, state.listbox);
        Box::into_raw(Box::new(selected)) as isize
    };

    unsafe {
        let _ = PostMessageW(
            owner,
            PICKER_RESULT_MSG,
            WPARAM(if cancelled { 1 } else { 0 }),
            LPARAM(lparam),
        );
    }
}

fn hit_test_close(hwnd: HWND, x: i32, y: i32) -> bool {
    let mut client = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut client) };
    let r = close_btn_rect(&client);
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn picker_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = lparam.0 as *const CREATESTRUCTW;
            if !create.is_null() {
                let state = (*create).lpCreateParams as *mut PickerState;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_CREATE => LRESULT(0),
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };

            let mut client = RECT::default();
            let _ = unsafe { GetClientRect(hwnd, &mut client) };

            let header_brush = unsafe { CreateSolidBrush(COLORREF(0x002D2D2D)) };
            let _ = unsafe {
                FillRect(
                    hdc,
                    &RECT {
                        left: client.left,
                        top: client.top,
                        right: client.right,
                        bottom: client.top + HEADER_H,
                    },
                    header_brush,
                )
            };
            let _ = unsafe { DeleteObject(header_brush) };

            let font = unsafe {
                CreateFontW(
                    -14,
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
            let old_font = unsafe { SelectObject(hdc, font) };
            let _ = unsafe { SetBkMode(hdc, TRANSPARENT) };
            let _ = unsafe { SetTextColor(hdc, COLORREF(0x00F0F0F0)) };
            let title = wide("Select apps (Enter=apply, Esc=cancel)");
            let _ = unsafe {
                DrawTextW(
                    hdc,
                    &mut title.clone(),
                    &mut RECT {
                        left: client.left + 12,
                        top: client.top + 6,
                        right: client.right - 12 - CLOSE_BTN_SIZE,
                        bottom: client.top + HEADER_H - 6,
                    },
                    DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_END_ELLIPSIS | DT_NOPREFIX,
                )
            };

            // Draw close button (X)
            let btn = close_btn_rect(&client);
            let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PickerState };
            let hover = !state_ptr.is_null() && unsafe { (*state_ptr).close_hover };
            let btn_brush = unsafe { CreateSolidBrush(COLORREF(if hover { 0x00404040 } else { 0x00333333 })) };
            let _ = unsafe { FillRect(hdc, &btn, btn_brush) };
            let _ = unsafe { DeleteObject(btn_brush) };

            let _ = unsafe { SetTextColor(hdc, COLORREF(0x00F0F0F0)) };
            let x_text = wide("\u{00D7}");
            let _ = unsafe {
                DrawTextW(
                    hdc,
                    &mut x_text.clone(),
                    &mut RECT {
                        left: btn.left,
                        top: btn.top,
                        right: btn.right,
                        bottom: btn.bottom,
                    },
                    DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
                )
            };

            let _ = unsafe { SelectObject(hdc, old_font) };
            let _ = unsafe { DeleteObject(font) };

            let _ = unsafe { EndPaint(hwnd, &ps) };
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PickerState };
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
                    let btn = close_btn_rect(&client);
                    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(hwnd, Some(&btn), false);
                };
                // Change cursor
                let cursor = unsafe { LoadCursorW(None, windows::Win32::UI::WindowsAndMessaging::IDC_HAND).unwrap() };
                unsafe {
                    SetCursor(cursor);
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

            let mut client = RECT::default();
            let _ = unsafe { GetClientRect(hwnd, &mut client) };
            if x < client.left || x >= client.right || y < client.top || y >= client.bottom {
                post_result(hwnd, true);
                let _ = unsafe { DestroyWindow(hwnd) };
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_COMMAND => {
            let notif = (wparam.0 as u32) >> 16;
            if notif == 0xFFFF {
                // LBN_DBLCLK
                post_result(hwnd, false);
                let _ = unsafe { DestroyWindow(hwnd) };
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
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
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}
