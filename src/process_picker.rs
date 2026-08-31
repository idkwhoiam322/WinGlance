use crate::overlay::source_matches_pin;
use crate::winapi::{
    create_font, create_window, delete_object, invalidate_rect, post_message, select_object, send_message, set_cursor,
    set_window_pos,
};
use crate::winutil::{
    Registered, StateClaim, clear_registered, release_window_state, set_window_state, wide, window_state,
};
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    COLORREF, CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BDR_SUNKENOUTER, BF_RECT, BeginPaint, ClientToScreen, CreateSolidBrush, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DrawEdge, DrawTextW, EndPaint, FillRect, GetMonitorInfoW, HBRUSH, HFONT,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, PAINTSTRUCT, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE, VK_SPACE,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, DefWindowProcW, DestroyWindow, EnumWindows, GWL_EXSTYLE, GetClientRect, GetParent,
    GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId, HWND_TOPMOST, IsIconic, IsWindowVisible, LB_ADDSTRING,
    LB_GETCOUNT, LB_GETCURSEL, LB_GETITEMDATA, LB_GETITEMRECT, LB_GETTOPINDEX, LB_SETCURSEL, LB_SETITEMDATA,
    LB_SETITEMHEIGHT, LBS_HASSTRINGS, LBS_NOINTEGRALHEIGHT, LBS_OWNERDRAWFIXED, LoadCursorW, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, ShowWindow, WINDOW_STYLE, WM_APP, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_DPICHANGED, WM_DRAWITEM, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,
    WM_SETFONT, WS_BORDER, WS_CHILD, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    WS_VSCROLL,
};
use windows::core::BOOL;
use windows::core::{PCWSTR, PWSTR};

const CLASS_NAME: &str = "WinGlanceProcessPicker";
const WIDTH: i32 = 400;
const HEADER_H: i32 = 30;
const ROW_HEIGHT: i32 = 22;
const MAX_VISIBLE: usize = 12;
/// 24 logical px so the close target clears the WCAG 2.2 minimum target size
/// even at 100% scaling. It was widened from 20 px to meet the accessibility benchmark.
const CLOSE_BTN_SIZE: i32 = 24;
const BST_CHECKED: usize = 1;
const BST_UNCHECKED: usize = 0;
/// Checkbox square size in pixels.
const CB_SIZE: i32 = 13;

pub(crate) const PICKER_RESULT_MSG: u32 = WM_APP + 7;
/// Result message for the Auto-compact sources picker (same contract as
/// `PICKER_RESULT_MSG`, same picker window, different config field).
pub(crate) const AUTO_SOURCES_RESULT_MSG: u32 = WM_APP + 11;
/// Result message for the pinned-source picker (same contract as
/// `PICKER_RESULT_MSG`, same picker window). This picker runs in single-select
/// mode: checking a row unchecks every other, so the result holds at most one
/// pattern — the config field it feeds (`behavior.pinned_source`) is a single
/// app, not a list.
pub(crate) const PINNED_SOURCE_RESULT_MSG: u32 = WM_APP + 12;

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
    /// The result message posted on confirm: `PICKER_RESULT_MSG` for the
    /// media-sources picker, `AUTO_SOURCES_RESULT_MSG` for the Auto-compact
    /// sources picker, `PINNED_SOURCE_RESULT_MSG` for the pinned-source
    /// picker. The main window distinguishes them by this message and writes
    /// the matching config field.
    result_msg: u32,
    /// Single-select mode (pinned-source picker): checking a row unchecks
    /// every other row, so `read_checked` returns at most one pattern. The
    /// media-sources and Auto-compact pickers are multi-select.
    single: bool,
}

static OPEN_PICKER: Registered<Option<isize>> = Registered::new();

/// Set when this window's WM_NCCREATE claims the state box handed over in
/// `lpCreateParams`, so a failed CreateWindowExW can tell whether the box was
/// taken by the system (and freed in WM_NCDESTROY) or still belongs to the
/// caller. Reset before each open. See `winutil::StateClaim` for the shared
/// mechanics.
static PICKER_STATE_CLAIMED: StateClaim = StateClaim::new();

/// Guards class registration: registering twice would leak the class brush.
static CLASS_REGISTERED: OnceLock<()> = OnceLock::new();

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

/// The executable name of a process, resolved through a targeted handle
/// query instead of a full process-table snapshot. Used by the overlay to
/// identify the foreground window's app for Auto-layout source matching
/// (it fires on each foreground switch; a `CreateToolhelp32Snapshot`
/// enumeration per switch is O(process count) work for one answer).
/// `PROCESS_QUERY_LIMITED_INFORMATION` is the documented minimum for
/// `QueryFullProcessImageNameW` and is granted across elevation for the
/// same user, so an elevated foreground app still resolves (the access-
/// denied case is an elevated process of a *different* user, which cannot
/// own the interactive foreground). `None` when the process does not exist
/// or cannot be read — callers treat that as "no match".
pub(crate) fn exe_name_for_pid(pid: u32) -> Option<String> {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return None;
        };
        let handle = ProcessQueryGuard(handle);
        // Long image paths need more than the 260-char MAX_PATH buffer;
        // retry once at the 32,768-char Windows path limit, the canonical
        // ceiling (the function does not report the required size).
        let mut capacity = 260u32;
        loop {
            let mut buffer = vec![0u16; capacity as usize];
            let mut size = capacity;
            if QueryFullProcessImageNameW(handle.0, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut size).is_ok() {
                buffer.truncate(size as usize);
                let path = String::from_utf16_lossy(&buffer);
                return Some(
                    std::path::Path::new(&path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&path)
                        .to_string(),
                );
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INSUFFICIENT_BUFFER.0 as i32) && capacity < 32768 {
                capacity = 32768;
            } else {
                return None;
            }
        }
    }
}

/// RAII guard for the process handle opened by `exe_name_for_pid`.
struct ProcessQueryGuard(HANDLE);

impl Drop for ProcessQueryGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
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
    // One row per distinct app pattern: a launcher process and its main
    // process (identical normalized exe names) must not produce two
    // indistinguishable rows — confirming both would write the pattern
    // twice into the allow list.
    let mut seen_patterns = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for (pid, entry) in scan.found {
        if pid == our_pid || !seen.insert(pid) {
            continue;
        }
        if !seen_patterns.insert(normalize_pattern(&entry.pattern)) {
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

/// Builds the picker's row list from the live process/session set plus the
/// user's stored allow-list. Every allow-list pattern that has no live
/// matching entry is added as a pre-checked row labeled "… (not running)" so
/// closing the picker can never silently drop a previously-enabled source:
/// the main window replaces (not merges) the allow-list with the picker's
/// checked result, so anything not shown above a checkbox would be lost.
/// Not-running rows are pinned above the live apps (each group sorted by
/// name), so a stored source stays visible even when its app is closed.
fn build_picker_list(current: &[String], mut entries: Vec<ProcessEntry>) -> Vec<ProcessEntry> {
    // Normalized patterns already represented by a live entry. Match with the
    // same bidirectional-contains rule the pre-check uses, so a "discord" entry
    // is not duplicated by a "discord-helper" running process, and vice-versa.
    let seen: HashSet<String> = entries.iter().map(|e| normalize_pattern(&e.pattern)).collect();
    let mut not_running = Vec::new();
    for pattern in current {
        let norm = normalize_pattern(pattern);
        if norm.is_empty() || seen.iter().any(|e| e.contains(&norm) || norm.contains(e)) {
            continue;
        }
        // Not currently running: keep it in the row set, pre-checked, with a
        // label that makes its absence from the live process list obvious.
        not_running.push(ProcessEntry {
            display_name: format!("{} (not running)", pretty_source_label(pattern)),
            pattern: pattern.clone(),
        });
    }
    entries.sort_by_key(|a| a.display_name.to_lowercase());
    not_running.sort_by_key(|a| a.display_name.to_lowercase());
    not_running.extend(entries);
    not_running
}

/// The pinned-source picker's row list: the live app/session set filtered to
/// the user's allowed sources (`media_sources`; an empty allow-list means all
/// apps are allowed, so nothing is filtered), plus the stored pin — kept
/// visible even when it no longer matches the allow-list so it can be cleared,
/// labeled "(not allowed)" instead of the misleading "(not running)". The same
/// identity rule the pin uses at runtime (`source_matches_pin`) drives the
/// filter, so every row offered here, if chosen, actually matches the pin.
fn build_pinned_source_list(current: &[String], allowed: &[String], entries: Vec<ProcessEntry>) -> Vec<ProcessEntry> {
    let allowed_entries = if allowed.is_empty() {
        entries
    } else {
        entries
            .into_iter()
            .filter(|e| allowed.iter().any(|a| source_matches_pin(&e.pattern, a)))
            .collect()
    };
    let current_allowed: Vec<String> = current
        .iter()
        .filter(|p| allowed.is_empty() || allowed.iter().any(|a| source_matches_pin(p, a)))
        .cloned()
        .collect();
    let list = build_picker_list(&current_allowed, allowed_entries);
    // A stored pin that no longer matches the allow-list (media_sources was
    // narrowed after pinning) stays visible and pre-checked so the user can
    // clear it; the label says why it would not work. Pinned above the
    // "(not running)" rows so it is not missed.
    let mut not_allowed: Vec<ProcessEntry> = current
        .iter()
        .filter(|p| !current_allowed.iter().any(|c| c == *p))
        .map(|p| ProcessEntry {
            display_name: format!("{} (not allowed)", pretty_source_label(p)),
            pattern: p.clone(),
        })
        .collect();
    not_allowed.sort_by_key(|e| e.display_name.to_lowercase());
    not_allowed.extend(list);
    not_allowed
}

/// The Auto-compact picker's row list: the pinned "Full screen apps" status
/// row is always the first entry — fullscreen apps compact regardless of the
/// app list (see `decide_layout`), so the coverage stays visible even after
/// apps are selected. Its empty pattern never matches, it is always checked
/// and clicks never toggle it, and `read_checked` skips it.
fn build_auto_compact_list(current: &[String], entries: Vec<ProcessEntry>) -> Vec<ProcessEntry> {
    let mut list = Vec::with_capacity(entries.len() + 1);
    list.push(ProcessEntry {
        display_name: "Full screen apps".into(),
        pattern: String::new(),
    });
    list.extend(build_picker_list(current, entries));
    list
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

/// Strips the `.exe` (or `.EXE`) suffix from an executable name without
/// corrupting stem characters ending in 'e', 'x', or '.' (which a character-set
/// trim would do for names like "firefox.exe", "Plex.exe", "Roblox.exe", "code.exe").
pub(crate) fn strip_exe_suffix(name: &str) -> &str {
    // `name.len() - 4` is a char boundary exactly when the last four bytes
    // are the ASCII ".exe" suffix; for any other name the checked `get`
    // returns None instead of slicing mid-character (a plain index would
    // panic on e.g. "a€bcd", and this function is called from an
    // extern "system" callback where a panic aborts the process).
    if name.len() >= 4
        && name
            .get(name.len() - 4..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(".exe"))
    {
        &name[..name.len() - 4]
    } else {
        name
    }
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // A contained panic stops the enumeration; the scan keeps what
    // it gathered.
    crate::winutil::guarded_enum("the picker's window enumeration", || unsafe {
        enum_windows_proc_body(hwnd, lparam)
    })
}

unsafe fn enum_windows_proc_body(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = unsafe { &mut *(lparam.0 as *mut WindowScan) };

    if !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return BOOL(1);
    }
    if unsafe { IsIconic(hwnd).as_bool() } {
        return BOOL(1);
    }

    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    if ex_style & windows::Win32::UI::WindowsAndMessaging::WS_EX_TOOLWINDOW.0 as isize != 0 {
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

    let pattern = strip_exe_suffix(exe_name).to_lowercase();

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
    allowed: &[String],
    result: Arc<Mutex<Option<Vec<String>>>>,
    result_msg: u32,
) -> bool {
    let entries = merge_smtc_sources(enumerate_app_processes());
    if entries.is_empty() {
        warn!("no app processes or SMTC sessions found for picker");
        return false;
    }
    let auto_picker = result_msg == AUTO_SOURCES_RESULT_MSG;
    // The pinned-source picker is single-select: the config field it feeds is
    // one app, never a list. It is also restricted to the user's allowed
    // sources: the SMTC worker excludes any session not matching
    // `media_sources` (when non-empty), so a pin outside the allow-list could
    // never fire — offering such rows would let the user pin something dead.
    let single = result_msg == PINNED_SOURCE_RESULT_MSG;
    let list = if single {
        build_pinned_source_list(current, allowed, entries)
    } else if auto_picker {
        build_auto_compact_list(current, entries)
    } else {
        build_picker_list(current, entries)
    };

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

        // The trigger rect is in the owner window's *client* coordinates, but
        // CreateWindowExW positions the popup in *screen* coordinates. Translate
        // the anchor edge into screen space first, then clamp in screen space
        // (the earlier client-coordinate clamp clamped the wrong numbers).
        let mut below = POINT {
            x: trigger_rect.left,
            y: trigger_rect.bottom,
        };
        let mut above = POINT {
            x: trigger_rect.left,
            y: trigger_rect.top,
        };
        let _ = ClientToScreen(owner, &mut below);
        let _ = ClientToScreen(owner, &mut above);
        let (mut x, mut y) = (below.x, below.y + 4);
        let monitor = MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let work = info.rcWork;
            if y + height > work.bottom {
                y = (above.y - 4 - height).max(work.top);
            }
            x = x.clamp(work.left, (work.right - width).max(work.left));
            y = y.clamp(work.top, (work.bottom - height).max(work.top));
        }

        // Pre-check with the same normalization the SMTC worker applies to
        // allow-list patterns, so a stored "youtube music" matches the
        // session-derived "youtube-music" entry. The Auto-compact picker's
        // "Full screen apps" status row is always checked — fullscreen
        // coverage is unconditional — and the app rows' pre-check must not
        // mark it via the empty-pattern contains rule.
        let norm_current: Vec<String> = current
            .iter()
            .map(|p| normalize_pattern(p))
            .filter(|n| !n.is_empty())
            .collect();
        let checked: Vec<bool> = list
            .iter()
            .enumerate()
            .map(|(i, e)| {
                if auto_picker && i == 0 {
                    true
                } else {
                    let ne = normalize_pattern(&e.pattern);
                    // An entry that normalizes to nothing (an image named
                    // literally ".exe") must never pre-check: the empty
                    // pattern is contained in every stored pattern, which
                    // would check the row against the whole allow list.
                    !ne.is_empty() && norm_current.iter().any(|n| ne.contains(n.as_str()) || n.contains(&ne))
                }
            })
            .collect();
        // Single-select invariant at open time: a short stored pin can
        // substring-match several offered rows, which would open the pinned
        // picker with multiple checked rows even though the click path only
        // ever allows one. Prefer an exact normalized identity match (the
        // true stored pin), else keep the first contains-match.
        let mut checked = checked;
        if single {
            let exact = list.iter().position(|e| {
                let ne = normalize_pattern(&e.pattern);
                norm_current.contains(&ne)
            });
            let chosen = exact.or_else(|| checked.iter().position(|&c| c));
            for (i, slot) in checked.iter_mut().enumerate() {
                *slot = Some(i) == chosen;
            }
        }

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
            result_msg,
            single,
        });
        let state_ptr = Box::into_raw(state);
        PICKER_STATE_CLAIMED.reset();

        let hwnd = create_window(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(wide(CLASS_NAME).as_ptr()),
            PCWSTR(wide("Select apps").as_ptr()),
            WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN,
            x,
            y,
            width,
            height,
            Some(owner),
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
                if let Some(state) = PICKER_STATE_CLAIMED.take_unclaimed(state_ptr) {
                    drop(state);
                }
                return false;
            }
        };

        OPEN_PICKER.set(Some(hwnd.0 as isize));

        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        // Fixed GDI objects for the picker's own chrome, created once per open
        // (WM_PAINT only reads them) and freed in WM_NCDESTROY.
        let state_ref = &mut *state_ptr;
        state_ref.scale = scale;
        rebuild_picker_fonts(state_ref, scale);
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
        let _ = set_window_pos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            phys_w,
            phys_h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );

        let lb = create_window(
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
            Some(hwnd),
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
            // Without the subclass the click-to-toggle and keyboard behavior
            // would be silently missing while `open` reported success, so a
            // failed install is a hard error: log it and close the picker.
            if !SetWindowSubclass(lb, Some(listbox_proc), LISTBOX_SUBCLASS_ID, hwnd.0 as usize).as_bool() {
                warn!("installing the picker listbox subclass failed; closing the picker");
                let _ = DestroyWindow(hwnd);
                return false;
            }

            let row_h = (ROW_HEIGHT as f32 * scale).round() as i32;
            let _ = send_message(lb, LB_SETITEMHEIGHT, WPARAM(0), LPARAM(row_h as isize));
            let font = state_ref.list_font;
            let _ = send_message(lb, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));

            for (i, entry) in state_ref.list.iter().enumerate() {
                let text = wide(&entry.display_name);
                let idx = send_message(lb, LB_ADDSTRING, WPARAM(0), LPARAM(text.as_ptr() as isize));
                let state_val = if checked[i] { BST_CHECKED } else { BST_UNCHECKED };
                let _ = send_message(lb, LB_SETITEMDATA, WPARAM(idx.0 as usize), LPARAM(state_val as isize));
            }
        }

        // Select and focus the list so keyboard navigation works from the
        // first keystroke: the picker's Enter/Esc/Space handling exists
        // precisely for keyboard use, and without focus the keys would
        // scroll the Settings pane underneath instead. The listbox handle
        // lives in the state (the block above scoped its own binding).
        // Capturing the mouse here routes every click to the listbox, so a
        // click anywhere outside the popup dismisses it (the listbox proc
        // detects out-of-client coordinates) — the standard popup
        // convention. Capture is released on the first click, and
        // implicitly when the window is destroyed.
        let lb = (*state_ptr).listbox;
        if let Some(first) = checked.iter().position(|&checked| checked) {
            let _ = send_message(lb, LB_SETCURSEL, WPARAM(first), LPARAM(0));
        }
        let _ = SetFocus(Some(lb));
        let _ = SetCapture(lb);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        debug!("process picker opened");
        true
    }
}

/// (Re)creates the picker's chrome fonts at `scale` — the 14 px semibold title
/// and 13 px regular list rows, matching the metrics the earlier global font
/// cache used. Brushes are solid colors and DPI-independent, so only the fonts
/// travel with a DPI change. Any previous handles are deleted first, so
/// WM_NCDESTROY always frees exactly the current set.
fn rebuild_picker_fonts(state: &mut PickerState, scale: f32) {
    if !state.header_font.0.is_null() {
        unsafe {
            let _ = delete_object(state.header_font);
        }
    }
    if !state.list_font.0.is_null() {
        unsafe {
            let _ = delete_object(state.list_font);
        }
    }
    state.header_font = unsafe {
        create_font(
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
    // Listbox font: same Segoe UI metrics the global cache used (13 px,
    // regular weight, quality 0x02) but owned by the picker. The global
    // cache flushes its handles on DPI change, which would leave the
    // listbox with a dangling HFONT.
    state.list_font = unsafe {
        create_font(
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
        )
    };
}

fn read_checked(hwnd: HWND, lb: HWND) -> Vec<String> {
    let count = unsafe { send_message(lb, LB_GETCOUNT, WPARAM(0), LPARAM(0)) };
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
        let data = unsafe { send_message(lb, LB_GETITEMDATA, WPARAM(i), LPARAM(0)) };
        if data.0 as usize == BST_CHECKED
            && let Some(entry) = state.list.get(i)
            // The Auto-compact picker's "Full screen only" mode row has an
            // empty pattern: it is a mode, never an app pattern to store.
            && !entry.pattern.is_empty()
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
    // pointer in the message. The main window takes the slot on the posted
    // result message (PICKER_RESULT_MSG / AUTO_SOURCES_RESULT_MSG); if the
    // post fails the slot is simply never read and the next picker open
    // overwrites it.
    let patterns = if cancelled {
        None
    } else {
        Some(read_checked(hwnd, state.listbox))
    };
    if let Ok(mut slot) = state.result.lock() {
        *slot = patterns.clone();
    }

    if unsafe { post_message(owner, state.result_msg, WPARAM(0), LPARAM(0)) }.is_err() {
        warn!("posting the picker result failed");
    } else if let Some(patterns) = patterns {
        info!("picker result updated to {patterns:?}");
    } else {
        debug!("process picker cancelled; source list unchanged");
    }
}

fn hit_test_close(hwnd: HWND, x: i32, y: i32) -> bool {
    let mut client = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut client) };
    let scale = unsafe { GetDpiForWindow(hwnd).max(96) as f32 / 96.0 };
    let r = close_btn_rect(&client, scale);
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// Toggles one row's checkbox through the exact semantics of a mouse click:
/// flip the stored state, keep the auto-picker's pinned status row fixed,
/// enforce single-select exclusivity on the pinned-source picker, and
/// repaint the affected rows. Shared by WM_LBUTTONDOWN and the keyboard
/// Space handler so both input paths behave identically.
fn toggle_picker_row(lb: HWND, state: &mut PickerState, i: usize) {
    // The Auto-compact picker's first row is the pinned "Full screen apps"
    // status row: fullscreen coverage is unconditional, so its check is
    // fixed — selecting the row never toggles it.
    let pinned_row = state.list.first().is_some_and(|e| e.pattern.is_empty()) && i == 0;
    if pinned_row {
        return;
    }
    let data = unsafe { send_message(lb, LB_GETITEMDATA, WPARAM(i), LPARAM(0)) };
    let toggled = if data.0 as usize == BST_CHECKED {
        BST_UNCHECKED
    } else {
        BST_CHECKED
    };
    let _ = unsafe { send_message(lb, LB_SETCURSEL, WPARAM(i), LPARAM(0)) };
    let _ = unsafe { send_message(lb, LB_SETITEMDATA, WPARAM(i), LPARAM(toggled as isize)) };
    if state.single {
        // Single-select (pinned-source picker): checking one row clears
        // every other, so at most one pattern is ever confirmed — toggling
        // the checked row unchecks it (clearing the pin), toggling any
        // other row moves the pin. The whole list repaints because another
        // checked row just flipped too.
        let count = unsafe { send_message(lb, LB_GETCOUNT, WPARAM(0), LPARAM(0)) }.0 as usize;
        for j in 0..count {
            if j != i && unsafe { send_message(lb, LB_GETITEMDATA, WPARAM(j), LPARAM(0)) }.0 as usize == BST_CHECKED {
                let _ = unsafe { send_message(lb, LB_SETITEMDATA, WPARAM(j), LPARAM(BST_UNCHECKED as isize)) };
            }
        }
        unsafe {
            let _ = invalidate_rect(lb, None, false);
        }
    } else {
        let mut item_rect = RECT::default();
        let _ = unsafe {
            send_message(
                lb,
                LB_GETITEMRECT,
                WPARAM(i),
                LPARAM(&mut item_rect as *mut RECT as isize),
            )
        };
        unsafe {
            let _ = invalidate_rect(lb, Some(&item_rect), false);
        }
    }
}

/// Comctl32 subclass proc for the picker's listbox. Mouse and keyboard
/// messages are delivered to the listbox child rather than the picker window,
/// so click-to-toggle, Space-to-toggle and double-click-to-confirm are
/// handled here. The parent (picker) HWND is carried in `ref_data`; PickerState
/// is read from that window's GWLP_USERDATA. Every message we do not consume
/// is forwarded via DefSubclassProc, which dispatches to the original listbox
/// proc that Comctl32 tracks internally — no GWLP_WNDPROC swap or stored
/// original proc is needed. The subclass is unhooked on WM_NCDESTROY.
unsafe extern "system" fn listbox_proc(
    lb: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    ref_data: usize,
) -> LRESULT {
    // Panic-contained; a panic logs and defers to the original
    // listbox procedure.
    crate::winutil::guarded_subclass(
        lb,
        message,
        wparam,
        lparam,
        "the picker's listbox subclass",
        || unsafe { listbox_proc_body(lb, message, wparam, lparam, ref_data) },
    )
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn listbox_proc_body(lb: HWND, message: u32, wparam: WPARAM, lparam: LPARAM, ref_data: usize) -> LRESULT {
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
                let x = (lparam.0 as u16) as i16 as i32;
                let y = ((lparam.0 >> 16) as u16) as i16 as i32;
                // Mouse capture is held from open(): every click anywhere on
                // screen is delivered to this listbox. A click outside the
                // listbox client is a click outside the popup — the standard
                // dismiss convention — so release the capture and cancel.
                let mut rc = RECT::default();
                let _ = unsafe { GetClientRect(lb, &mut rc) };
                let cw = rc.right;
                let ch = rc.bottom;
                if x < 0 || y < 0 || x >= cw || y >= ch {
                    let _ = ReleaseCapture();
                    let _ = unsafe { DestroyWindow(parent) };
                    return LRESULT(0);
                }
                // The row height is DPI-scaled like the listbox item height,
                // so hit-testing matches the rendered rows on any display.
                // The listbox scrolls (only MAX_VISIBLE rows fit), so the
                // clicked client row is relative to the top index.
                let scale = unsafe { (*state_ptr).scale };
                let row_h = (ROW_HEIGHT as f32 * scale).round() as i32;
                let top = unsafe { send_message(lb, LB_GETTOPINDEX, WPARAM(0), LPARAM(0)) }.0 as i32;
                let item_idx = top + y / row_h.max(1);
                let count = unsafe { send_message(lb, LB_GETCOUNT, WPARAM(0), LPARAM(0)) }.0 as i32;
                if item_idx >= 0 && item_idx < count {
                    let i = item_idx as usize;
                    let state = unsafe { &mut *state_ptr };

                    // Double-click on the same item within the system's
                    // double-click interval confirms and closes, applying
                    // the state left by the first click.
                    let now = Instant::now();
                    let double_click_ms = u64::from(unsafe { GetDoubleClickTime() });
                    let is_double = state.last_click_item == Some(i)
                        && state
                            .last_click_time
                            .is_some_and(|t| t.elapsed() < Duration::from_millis(double_click_ms));
                    state.last_click_item = Some(i);
                    state.last_click_time = Some(now);

                    if is_double {
                        post_result(parent, false);
                        let _ = unsafe { DestroyWindow(parent) };
                        return LRESULT(0);
                    }

                    // Single click: toggle through the shared row path.
                    toggle_picker_row(lb, state, i);
                    return LRESULT(0);
                }
            }
            unsafe { DefSubclassProc(lb, message, wparam, lparam) }
        }
        WM_KEYDOWN => {
            // The listbox takes keyboard focus when clicked, so Enter/Esc must
            // work here as well as on the picker window itself. Space toggles
            // the selected row: without it a keyboard user could
            // navigate and confirm but never change a check — the check state
            // lived only in WM_LBUTTONDOWN.
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
            if key == VK_SPACE.0 {
                let sel = unsafe { send_message(lb, LB_GETCURSEL, WPARAM(0), LPARAM(0)) }.0;
                if sel >= 0 {
                    let state_ptr = window_state::<PickerState>(parent);
                    if !state_ptr.is_null() {
                        let state = unsafe { &mut *state_ptr };
                        toggle_picker_row(lb, state, sel as usize);
                    }
                }
                return LRESULT(0);
            }
            unsafe { DefSubclassProc(lb, message, wparam, lparam) }
        }
        _ => unsafe { DefSubclassProc(lb, message, wparam, lparam) },
    }
}

unsafe extern "system" fn picker_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Panic-contained; a panic logs, posts quit and falls back to
    // DefWindowProcW.
    crate::winutil::guarded_wndproc(
        hwnd,
        message,
        wparam,
        lparam,
        "the process picker window procedure",
        || unsafe { picker_proc_body(hwnd, message, wparam, lparam) },
    )
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn picker_proc_body(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = lparam.0 as *const CREATESTRUCTW;
            if !create.is_null() {
                let state = (*create).lpCreateParams as *mut PickerState;
                // Same guard as the positioner, main window, and duration
                // dialog: a null param must not flip PICKER_STATE_CLAIMED
                // while GWLP_USERDATA stays empty — WM_NCDESTROY would free
                // nothing and take_unclaimed would refuse to return the box,
                // leaking it with its chrome GDI objects on a failed create.
                // With the guard, an unclaimed box returns to the caller's
                // failure branch.
                if !state.is_null() {
                    set_window_state(hwnd, state);
                    PICKER_STATE_CLAIMED.claim();
                }
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_CREATE => LRESULT(0),
        WM_DPICHANGED => {
            // The owner was dragged to a display with a different DPI while
            // the picker was open (or the picker followed it there). Rebuild
            // the DPI-sized chrome — fonts, row height, window and listbox
            // geometry — and apply the system's suggested rect, which
            // preserves the picker's logical position across the transition.
            // `state.scale` is updated first so the paint and hit-testing
            // paths agree with the new row height immediately.
            let state_ptr = window_state::<PickerState>(hwnd);
            if !state_ptr.is_null() && lparam.0 != 0 {
                let suggested = unsafe { &*(lparam.0 as *const RECT) };
                let new_dpi = (wparam.0 >> 16) as u32;
                let scale = new_dpi.max(96) as f32 / 96.0;
                let state = unsafe { &mut *state_ptr };
                let rows = state.list.len().min(MAX_VISIBLE);
                let row_h = (ROW_HEIGHT as f32 * scale).round() as i32;
                let header_h = (HEADER_H as f32 * scale).round() as i32;
                let phys_w = suggested.right - suggested.left;
                let phys_h = suggested.bottom - suggested.top;
                rebuild_picker_fonts(state, scale);
                state.scale = scale;
                let _ = unsafe {
                    set_window_pos(
                        hwnd,
                        HWND_TOPMOST,
                        suggested.left,
                        suggested.top,
                        phys_w,
                        phys_h,
                        SWP_NOACTIVATE,
                    )
                };
                // The listbox re-flows at the new row height and tracks the
                // new window size; LB_SETITEMHEIGHT makes its scrollbar
                // recalculate. The old font was deleted by
                // `rebuild_picker_fonts`, so the listbox must be handed the
                // new handle too — otherwise its owner-draw rows keep
                // selecting the dangling font and render in the default
                // system font, mirroring the open path below.
                if !state.listbox.0.is_null() {
                    let _ = unsafe {
                        set_window_pos(
                            state.listbox,
                            HWND::default(),
                            0,
                            header_h,
                            phys_w,
                            rows as i32 * row_h,
                            SWP_NOACTIVATE | SWP_NOZORDER,
                        )
                    };
                    let _ = unsafe { send_message(state.listbox, LB_SETITEMHEIGHT, WPARAM(0), LPARAM(row_h as isize)) };
                    let _ = unsafe {
                        send_message(state.listbox, WM_SETFONT, WPARAM(state.list_font.0 as usize), LPARAM(1))
                    };
                }
                let _ = unsafe { invalidate_rect(hwnd, None, true) };
            }
            LRESULT(0)
        }
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
                    create_font(
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
                Some(unsafe { select_object(hdc, header_font) })
            };
            let _ = unsafe { SetBkMode(hdc, TRANSPARENT) };
            let _ = unsafe { SetTextColor(hdc, COLORREF(0x00F0F0F0)) };
            let mut title = wide("Select apps (Enter=apply, Esc=cancel)").into_vec();
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
            let mut x_text = wide("\u{00D7}").into_vec();
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
                let _ = unsafe { select_object(hdc, old_font) };
            }
            if transient {
                let _ = unsafe { delete_object(header_font) };
                let _ = unsafe { delete_object(header_brush) };
                let _ = unsafe { delete_object(close_brush) };
                let _ = unsafe { delete_object(close_hover_brush) };
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
                    let mut tick = wide("X").into_vec();
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

                // Entry text. The pinned "Full screen apps" status row (empty
                // pattern) renders muted to read as a status line rather than
                // a selectable app.
                SetTextColor(
                    draw.hDC,
                    COLORREF(if entry.pattern.is_empty() {
                        0x00C8C8C8
                    } else {
                        0x00F0F0F0
                    }),
                );
                SetBkMode(draw.hDC, TRANSPARENT);
                let mut name = wide(&entry.display_name).into_vec();
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
                    let _ = invalidate_rect(hwnd, Some(&btn), false);
                };
                // Change cursor; a failed load (broken system resources) just
                // keeps the current cursor instead of panicking the picker.
                if let Ok(cursor) = unsafe { LoadCursorW(None, windows::Win32::UI::WindowsAndMessaging::IDC_HAND) } {
                    unsafe {
                        set_cursor(cursor);
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
            if wparam.0 as u16 == VK_SPACE.0 {
                // Focus has not entered the listbox yet: route Space to the
                // selected row anyway so keyboard-only use never depends on
                // a prior mouse click. No selection = no-op.
                let state_ptr = window_state::<PickerState>(hwnd);
                if !state_ptr.is_null() {
                    let state = unsafe { &mut *state_ptr };
                    if !state.listbox.0.is_null() {
                        let sel = unsafe { send_message(state.listbox, LB_GETCURSEL, WPARAM(0), LPARAM(0)) }.0;
                        if sel >= 0 {
                            toggle_picker_row(state.listbox, state, sel as usize);
                        }
                    }
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_DESTROY => {
            // Guarded by hwnd via the shared helper: a stale teardown must
            // never clear a newer picker's registration.
            clear_registered(&OPEN_PICKER, |guard| *guard == Some(hwnd.0 as isize));
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
                let _ = unsafe { delete_object(state.header_font) };
                let _ = unsafe { delete_object(state.list_font) };
                let _ = unsafe { delete_object(state.header_brush) };
                let _ = unsafe { delete_object(state.close_brush) };
                let _ = unsafe { delete_object(state.close_hover_brush) };
                let _ = unsafe { delete_object(state.row_brush) };
                let _ = unsafe { delete_object(state.row_selected_brush) };
                // Slot clear first, box second — the canonical order every
                // window applies via the shared helper.
                release_window_state(hwnd, state_ptr);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nccreate_with_null_create_params_leaves_the_box_unclaimed() {
        // A valid CREATESTRUCTW whose lpCreateParams is null (WM_NCCREATE
        // with lparam naming it) must not flip PICKER_STATE_CLAIMED while
        // the slot stays empty: on a failed create the caller's
        // take_unclaimed would then refuse to return the box, leaking it
        // with its chrome GDI objects. The null-guard keeps an unclaimed
        // box returnable — the same contract the positioner, main window,
        // and duration dialog apply. (lparam == 0, the null-CREATESTRUCTW
        // case, is caught by the outer guard and is not what this pins.)
        PICKER_STATE_CLAIMED.reset();
        let state_ptr = Box::into_raw(Box::new(PickerState {
            listbox: HWND::default(),
            list: Vec::new(),
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
            result: Arc::new(Mutex::new(None)),
            result_msg: 0,
            single: false,
        }));
        let create = CREATESTRUCTW {
            lpCreateParams: std::ptr::null_mut(),
            ..Default::default()
        };
        let lparam = LPARAM((&create as *const CREATESTRUCTW) as isize);
        let _ = unsafe { picker_proc_body(HWND::default(), WM_NCCREATE, WPARAM(0), lparam) };
        let boxed = PICKER_STATE_CLAIMED
            .take_unclaimed(state_ptr)
            .expect("a null lpCreateParams must leave the box unclaimed and returnable to the caller");
        drop(boxed);
    }

    #[test]
    fn exe_name_for_pid_resolves_the_current_process() {
        // The targeted query must resolve a live process to its executable
        // name; under test that is the WinGlance test binary.
        let name = exe_name_for_pid(unsafe { GetCurrentProcessId() });
        assert!(
            name.as_deref().is_some_and(|n| n.ends_with(".exe")),
            "the current process must resolve to an .exe name, got {name:?}"
        );
    }

    #[test]
    fn exe_name_for_pid_returns_none_for_a_missing_process() {
        // pid 0 is never a valid process handle target.
        assert_eq!(exe_name_for_pid(0), None);
    }

    fn entry(pattern: &str) -> ProcessEntry {
        ProcessEntry {
            display_name: pretty_source_label(pattern).to_string(),
            pattern: pattern.to_string(),
        }
    }

    #[test]
    fn running_app_is_not_duplicated_with_a_not_running_row() {
        let list = build_picker_list(&["telegram".to_string()], vec![entry("telegram")]);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].pattern, "telegram");
        assert!(!list[0].display_name.contains("not running"));
    }

    #[test]
    fn not_running_configured_source_is_retained_and_pre_checkable() {
        let current = vec!["discord".to_string()];
        let list = build_picker_list(&current, vec![]);
        assert_eq!(list.len(), 1);
        assert!(list[0].display_name.ends_with("(not running)"));
        // The row's pattern is the stored pattern verbatim, so the existing
        // pre-check (which matches normalized current patterns) marks it checked
        // and it survives the main window's full-replace-on-save.
        assert_eq!(list[0].pattern, "discord");
        let norm = normalize_pattern(&list[0].pattern);
        assert!(current.iter().any(|p| normalize_pattern(p) == norm));
    }

    #[test]
    fn empty_patterns_add_no_rows() {
        assert!(build_picker_list(&[" ".to_string(), "".to_string()], vec![]).is_empty());
    }

    #[test]
    fn build_auto_compact_list_prepends_the_fullscreen_status_row() {
        // The Auto-compact picker always leads with the pinned "Full screen
        // apps" status row: fullscreen apps compact regardless of the app
        // list, so the coverage stays visible even after apps are selected.
        // Its empty pattern never matches and `read_checked` skips it.
        let list = build_auto_compact_list(&["spotify".to_string()], vec![entry("spotify"), entry("netflix")]);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].display_name, "Full screen apps");
        assert!(list[0].pattern.is_empty(), "the status row must never store a pattern");
        // The app rows keep their usual (alphabetical) order below the status.
        let apps: Vec<&str> = list[1..].iter().map(|e| e.pattern.as_str()).collect();
        assert_eq!(apps, ["netflix", "spotify"]);
        // An empty app list still gets the status row.
        let only = build_auto_compact_list(&[], vec![entry("youtube-music")]);
        assert_eq!(only.len(), 2);
        assert!(only[0].pattern.is_empty());
    }

    #[test]
    fn running_and_configured_sources_coexist() {
        let list = build_picker_list(&["spotify".to_string(), "discord".to_string()], vec![entry("spotify")]);
        assert_eq!(list.len(), 2);
        let by_pattern: HashMap<&str, &ProcessEntry> = list.iter().map(|e| (e.pattern.as_str(), e)).collect();
        assert!(!by_pattern["spotify"].display_name.contains("not running"));
        assert!(by_pattern["discord"].display_name.contains("not running"));
        assert_eq!(by_pattern["spotify"].pattern, "spotify");
        assert_eq!(by_pattern["discord"].pattern, "discord");
    }

    #[test]
    fn not_running_sources_pin_to_the_top_of_the_list() {
        // The configured-but-closed app must appear above every running app,
        // regardless of alphabetical order, so a stored source is never lost
        // below the fold of a long list.
        let list = build_picker_list(
            &["zebra".to_string(), "alpha".to_string()],
            vec![entry("mango"), entry("banana")],
        );
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].pattern, "alpha");
        assert_eq!(list[1].pattern, "zebra");
        assert_eq!(list[2].pattern, "banana");
        assert_eq!(list[3].pattern, "mango");
        assert!(list[0].display_name.contains("not running"));
        assert!(list[1].display_name.contains("not running"));
        assert!(!list[2].display_name.contains("not running"));
        assert!(!list[3].display_name.contains("not running"));
    }

    #[test]
    fn not_running_group_stays_alphabetically_sorted() {
        let list = build_picker_list(&["zeta".to_string(), "alpha".to_string()], vec![]);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].pattern, "alpha");
        assert_eq!(list[1].pattern, "zeta");
    }

    #[test]
    fn pinned_source_list_is_restricted_to_allowed_sources() {
        // The pinned-source picker may only offer apps matching the user's
        // Allowed Sources: the worker excludes anything outside `media_sources`
        // (non-empty), so a pin outside it could never fire.
        let entries = vec![entry("spotify"), entry("spotifyhelper"), entry("chrome")];
        let allowed = vec!["Spotify".to_string()];
        let list = build_pinned_source_list(&[], &allowed, entries);
        let patterns: Vec<&str> = list.iter().map(|e| e.pattern.as_str()).collect();
        assert_eq!(
            patterns,
            ["spotify", "spotifyhelper"],
            "only sources matching the allow-list may be pinned"
        );
    }

    #[test]
    fn pinned_source_list_with_empty_allow_list_shows_everything() {
        // Empty `media_sources` = all apps allowed (documented semantics), so
        // the pinned picker is unfiltered in that case.
        let entries = vec![entry("spotify"), entry("chrome")];
        let list = build_pinned_source_list(&[], &[], entries);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn pinned_source_list_matches_allow_patterns_bidirectionally() {
        // The same identity rule as the pin at runtime: an allow pattern
        // stored as "youtube music" matches the session-derived
        // "youtube-music" entry, so the row set agrees with the worker.
        let entries = vec![entry("youtube-music"), entry("brave")];
        let allowed = vec!["youtube music".to_string()];
        let list = build_pinned_source_list(&[], &allowed, entries);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].pattern, "youtube-music");
    }

    #[test]
    fn disallowed_stored_pin_stays_visible_and_clearable() {
        // A pin that no longer matches the allow-list (media_sources was
        // narrowed after pinning) must not vanish: it stays as a
        // "(not allowed)" row — an accurate label even when the app is
        // running — with the pattern verbatim so the pre-check marks it and
        // the user can clear it.
        let entries = vec![entry("foobar"), entry("spotify")];
        let allowed = vec!["spotify".to_string()];
        let list = build_pinned_source_list(&["foobar".to_string()], &allowed, entries);
        assert_eq!(list.len(), 2, "the disallowed pin row plus the allowed live app");
        assert!(
            list[0].display_name.contains("not allowed"),
            "label must read '(not allowed)', got '{}'",
            list[0].display_name
        );
        assert_eq!(list[0].pattern, "foobar");
        assert_eq!(list[1].pattern, "spotify");
        // The stored-pin row's pattern is verbatim, so the pre-check (which
        // matches normalized current patterns) marks it checked and it
        // survives the single-select replace-on-save.
        assert_eq!(normalize_pattern(&list[0].pattern), normalize_pattern("foobar"));
    }

    #[test]
    fn a_still_allowed_stored_pin_is_offered_as_a_normal_row() {
        // The happy-path complement to the disallowed-pin test: a stored pin
        // that still matches the allow-list is a normal pre-checked row — no
        // "(not allowed)" label, no "(not running)" duplicate.
        let entries = vec![entry("spotify"), entry("chrome")];
        let allowed = vec!["spotify".to_string()];
        let list = build_pinned_source_list(&["spotify".to_string()], &allowed, entries);
        assert_eq!(list.len(), 1, "chrome is filtered, spotify is offered");
        assert_eq!(list[0].pattern, "spotify");
        assert!(
            !list[0].display_name.contains("not allowed"),
            "a still-allowed pin must not be labeled, got '{}'",
            list[0].display_name
        );
    }

    #[test]
    fn disallowed_stored_pins_sort_above_the_live_rows() {
        // Multiple disallowed pins are sorted by name and pinned above the
        // live allowed rows, so the clearable group is always findable.
        let entries = vec![entry("spotify")];
        let allowed = vec!["spotify".to_string()];
        let current = vec!["zebra".to_string(), "alpha".to_string()];
        let list = build_pinned_source_list(&current, &allowed, entries);
        let patterns: Vec<&str> = list.iter().map(|e| e.pattern.as_str()).collect();
        assert_eq!(
            patterns,
            ["alpha", "zebra", "spotify"],
            "sorted disallowed pins first, the live allowed row last"
        );
        assert!(list[0].display_name.contains("not allowed"));
        assert!(list[1].display_name.contains("not allowed"));
        assert!(!list[2].display_name.contains("not allowed"));
    }

    #[test]
    fn strip_exe_suffix_preserves_stem_ending_in_e_x_or_dot() {
        assert_eq!(strip_exe_suffix("firefox.exe"), "firefox");
        assert_eq!(strip_exe_suffix("Plex.exe"), "Plex");
        assert_eq!(strip_exe_suffix("Roblox.exe"), "Roblox");
        assert_eq!(strip_exe_suffix("code.exe"), "code");
        assert_eq!(strip_exe_suffix("explorer.EXE"), "explorer");
        assert_eq!(strip_exe_suffix("app.name.exe"), "app.name");
        assert_eq!(strip_exe_suffix("sample"), "sample");
        assert_eq!(strip_exe_suffix(".exe"), "");
        // Non-.exe names where `len - 4` would land mid-way through a
        // multi-byte character must not panic and must not strip.
        assert_eq!(strip_exe_suffix("€€"), "€€");
        assert_eq!(strip_exe_suffix("a€bcd"), "a€bcd");
        // A non-ASCII stem with an ASCII ".exe" suffix still strips.
        assert_eq!(strip_exe_suffix("名前.exe"), "名前");
    }
}
