use crate::autostart;
use crate::config::{Config, HorizontalPosition, VerticalPosition};
use crate::events::{MEDIA_EVENT_MSG, MediaEvent, POSITION_MSG, PlaybackState, TOGGLE_MSG, TrackInfo};
use crate::overlay::{
    EventQueue, OverlayPos, decode_artwork_pm, draw_string, set_duration, set_position, show_sample, wide,
};
use crate::process_picker;
use crate::process_picker::PICKER_RESULT_MSG;
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use log::{debug, error};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, GlobalFree, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CAPTION_COLOR, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush,
    DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DeleteObject, EndPaint, FF_DONTCARE, FillRect, GetStockObject,
    HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, SetBkColor, SetTextColor,
    StretchDIBits,
};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, NMHDR, NMTTDISPINFOW, ODS_SELECTED, TOOLTIPS_CLASSW, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_DELTOOLW,
    TTM_SETMAXTIPWIDTH, TTN_GETDISPINFOW, TTS_ALWAYSTIP, TTS_NOPREFIX, WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GWLP_USERDATA, GetClientRect, GetCursorPos, GetWindowLongPtrW, HMENU, IDC_ARROW, IDI_APPLICATION, IsWindowVisible,
    KillTimer, LB_ADDSTRING, LB_DELETESTRING, LB_GETCOUNT, LB_GETITEMRECT, LB_GETTOPINDEX, LB_INSERTSTRING,
    LB_SETITEMHEIGHT, LB_SETTOPINDEX, LBS_HASSTRINGS, LBS_NOINTEGRALHEIGHT, LBS_OWNERDRAWFIXED, LoadCursorW, LoadIconW,
    MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, PostMessageW, PostQuitMessage, RegisterClassExW, SW_HIDE, SW_SHOW,
    SW_SHOWMAXIMIZED, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WINDOW_STYLE, WM_APP,
    WM_CLOSE, WM_CREATE, WM_CTLCOLORLISTBOX, WM_DESTROY, WM_DRAWITEM, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_MOUSEMOVE,
    WM_NCCREATE, WM_NCDESTROY, WM_NOTIFY, WM_PAINT, WM_RBUTTONUP, WM_SETFONT, WM_SIZE, WM_TIMER, WNDCLASS_STYLES,
    WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{PCWSTR, PWSTR};

const WM_TRAY: u32 = WM_APP + 2;
const TRAY_ID: u32 = 1;
const MENU_OPEN_ID: usize = 1001;
const MENU_NOTIFY_ID: usize = 1002;
const MENU_AUTOSTART_ID: usize = 1003;
const MENU_CLOSE_TRAY_ID: usize = 1004;
const MENU_QUIT_ID: usize = 1006;
const MENU_POSITION_TOP_LEFT: usize = 1007;
const MENU_POSITION_TOP_CENTER: usize = 1008;
const MENU_POSITION_TOP_RIGHT: usize = 1009;
const MENU_POSITION_BOTTOM_LEFT: usize = 1010;
const MENU_POSITION_BOTTOM_CENTER: usize = 1011;
const MENU_POSITION_BOTTOM_RIGHT: usize = 1012;
const MENU_POSITION_CUSTOM: usize = 1013;
const MENU_POSITION_SAMPLE: usize = 1014;
const MENU_POSITION_RESET: usize = 1015;
const MENU_DURATION_2S: usize = 1017;
const MENU_DURATION_3S: usize = 1018;
const MENU_DURATION_5S: usize = 1019;
const MENU_DURATION_10S: usize = 1020;
const LISTBOX_ID: usize = 2;
/// History rows are kept in the heap (as entries) and duplicated in the
/// listbox as UTF-16 row strings, so the cap directly sizes the app's
/// baseline memory (~1 KB per row across both copies).
const HISTORY_CAP: usize = 400;
/// Artwork decode size in pixels (1.33× the 96 logical tile, so the cached
/// bitmap stays crisp up to ~133% DPI; below that it is only ever
/// downscaled at paint). 128²×4 = 64 KB, versus 147 KB at 192².
const ART_DECODE: u32 = 128;
/// Timer used to clear the "Copied" feedback on the Copy logs button.
const TIMER_LOGS_ID: usize = 101;
/// Timer used to keep the native history tooltip's item rects in sync (scroll).
const TIMER_TOOLTIPS_ID: usize = 102;
/// Win32 LPSTR_TEXTCALLBACK sentinel: fetch tooltip text on demand.
const LPSTR_TEXTCALLBACK: isize = -1;

/// Native TOOLINFOW layout (64-bit), for TTM_ADDTOOLW via SendMessageW.
#[repr(C)]
struct ToolInfo {
    cb_size: u32,
    u_flags: u32,
    hwnd: HWND,
    u_id: usize,
    rect: RECT,
    hinst: HINSTANCE,
    lpsz_text: *mut u16,
    l_param: isize,
    lp_reserved: *mut c_void,
}

const fn colorref(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

// Logical panel geometry (scaled by DPI at paint/layout time).
const PAD: f32 = 16.0;
const HEADER_H: f32 = 22.0;
const ART_Y: f32 = 36.0;
const ART_SIZE: f32 = 96.0;
const SEP_GAP: f32 = 20.0;
const HIST_GAP: f32 = 14.0;
const HIST_H: f32 = 18.0;
const LIST_GAP: f32 = 8.0;
const BOTTOM_GAP: f32 = 16.0;
const SIDEBAR_W: f32 = 80.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Pane {
    Activity,
    Settings,
}

/// Settings rows: section headers with label-left / control-right card rows.
#[derive(Clone, Copy, PartialEq)]
enum SettingId {
    Notifications,
    Duration,
    StartOnLogin,
    CloseToTray,
    AllowedApps,
    Position,
    ShowSample,
    CopyLogs,
}

enum SettingsItem {
    Header { text: &'static str, rect: RECT },
    Row { id: SettingId, rect: RECT },
}

const SETTINGS_SURFACE: [u8; 4] = [0x1B, 0x1B, 0x1B, 0xFF];
const SETTINGS_BORDER: [u8; 4] = [0x2D, 0x2D, 0x2D, 0xFF];
const SETTINGS_HOVER: [u8; 4] = [0x24, 0x24, 0x24, 0xFF];
const SETTINGS_TEXT: [u8; 4] = [0xF0, 0xF0, 0xF0, 0xFF];
const SETTINGS_MUTED: [u8; 4] = [0xC8, 0xC8, 0xC8, 0xFF];
const SETTINGS_FAINT: [u8; 4] = [0x7A, 0x7A, 0x7A, 0xFF];

/// Blends `a` over `b` (0.0 = b, 1.0 = a).
fn mix(a: [u8; 4], b: [u8; 4], t: f32) -> [u8; 4] {
    [
        (a[0] as f32 * t + b[0] as f32 * (1.0 - t)) as u8,
        (a[1] as f32 * t + b[1] as f32 * (1.0 - t)) as u8,
        (a[2] as f32 * t + b[2] as f32 * (1.0 - t)) as u8,
        0xFF,
    ]
}

/// FNV-1a 64 hash, used to detect artwork byte changes cheaply (compared to
/// re-decoding the image to find out).
fn fingerprint(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// Which sub-control of a settings row is being hovered/clicked.
#[derive(Clone, Copy, PartialEq, Debug)]
enum SettingSub {
    None,
    Seg(usize),
    Anchor(usize),
    Reset,
    Adjust,
}

/// Sub-rects of the Position row: value text, the six anchor segments, the
/// Reset button and the Adjust button. Paint, hit-test and hover all use this.
struct PositionParts {
    value_row: RECT,
    anchors: Vec<RECT>,
    reset: RECT,
    adjust: RECT,
}

fn position_parts(rect: &RECT, scale: f32) -> PositionParts {
    let label_w = (((rect.right - rect.left) as f32) * 0.42) as i32;
    let control_left = rect.left + label_w + (10.0 * scale) as i32;
    let control_right = rect.right - (10.0 * scale) as i32;
    let row1_h = (30.0 * scale) as i32;
    let gap = (6.0 * scale) as i32;
    let reset_w = (64.0 * scale) as i32;

    let value_row = RECT {
        left: control_left,
        top: rect.top,
        right: control_right - reset_w - (8.0 * scale) as i32,
        bottom: rect.top + row1_h,
    };
    let reset = RECT {
        left: control_right - reset_w,
        top: rect.top,
        right: control_right,
        bottom: rect.top + row1_h,
    };

    let row2_top = rect.top + row1_h + gap;
    let row2_bottom = rect.bottom - (4.0 * scale) as i32;
    let row2 = RECT {
        left: control_left,
        top: row2_top,
        right: control_right,
        bottom: row2_bottom,
    };
    let seg_gap = (4.0 * scale) as i32;
    let total = row2.right - row2.left;
    let w = (total - seg_gap * 6) / 7;
    let mut anchors = Vec::with_capacity(6);
    for i in 0..6 {
        anchors.push(RECT {
            left: row2.left + i * (w + seg_gap),
            top: row2.top,
            right: row2.left + (i + 1) * (w + seg_gap) - seg_gap,
            bottom: row2.bottom,
        });
    }
    let adjust = RECT {
        left: row2.left + 6 * (w + seg_gap),
        top: row2.top,
        right: row2.left + 7 * (w + seg_gap) - seg_gap,
        bottom: row2.bottom,
    };
    PositionParts {
        value_row,
        anchors,
        reset,
        adjust,
    }
}

const ANCHOR_LABELS: [&str; 6] = ["TL", "TC", "TR", "BL", "BC", "BR"];

/// Draws a small bordered button (active/hover highlighted with accent).
#[allow(clippy::too_many_arguments)]
/// Fixed-color brushes for the settings pane, created once with the window
/// and freed at destroy, so paints no longer create/delete ~40 brushes.
#[derive(Clone, Copy)]
struct SettingsBrushes {
    border: HBRUSH,
    surface: HBRUSH,
    hover: HBRUSH,
}

#[allow(clippy::too_many_arguments)]
fn draw_segment_button(
    hdc: HDC,
    rect: &RECT,
    label: &str,
    active: bool,
    hovered: bool,
    accent: [u8; 4],
    accent_soft: [u8; 4],
    scale: f32,
    brushes: SettingsBrushes,
) {
    if active {
        let b = unsafe { CreateSolidBrush(colorref(accent[0], accent[1], accent[2])) };
        unsafe {
            let _ = FillRect(hdc, rect, b);
        }
        unsafe {
            let _ = DeleteObject(HGDIOBJ(b.0));
        }
    } else {
        unsafe {
            let _ = FillRect(hdc, rect, brushes.border);
        }
    }
    let inner = RECT {
        left: rect.left + 1,
        top: rect.top + 1,
        right: rect.right - 1,
        bottom: rect.bottom - 1,
    };
    if active {
        let f = unsafe { CreateSolidBrush(colorref(accent_soft[0], accent_soft[1], accent_soft[2])) };
        unsafe {
            let _ = FillRect(hdc, &inner, f);
        }
        unsafe {
            let _ = DeleteObject(HGDIOBJ(f.0));
        }
    } else {
        unsafe {
            let _ = FillRect(hdc, &inner, if hovered { brushes.hover } else { brushes.surface });
        }
    }
    let mut t = inner;
    let tc = if active { SETTINGS_TEXT } else { SETTINGS_MUTED };
    draw_string(hdc, label, &mut t, (10.0 * scale) as i32, tc, active, true);
}

/// Draws an outline button (accent border, dark fill, accent label).
fn draw_small_button(hdc: HDC, rect: &RECT, label: &str, accent: [u8; 4], hovered: bool, scale: f32) {
    let b = unsafe { CreateSolidBrush(colorref(accent[0], accent[1], accent[2])) };
    unsafe {
        let _ = FillRect(hdc, rect, b);
    }
    unsafe {
        let _ = DeleteObject(HGDIOBJ(b.0));
    }
    let inner = RECT {
        left: rect.left + 1,
        top: rect.top + 1,
        right: rect.right - 1,
        bottom: rect.bottom - 1,
    };
    let fill = if hovered {
        mix(accent, [0x1B, 0x1B, 0x1B, 0xFF], 0.35)
    } else {
        [0x12, 0x12, 0x12, 0xFF]
    };
    let f = unsafe { CreateSolidBrush(colorref(fill[0], fill[1], fill[2])) };
    unsafe {
        let _ = FillRect(hdc, &inner, f);
    }
    unsafe {
        let _ = DeleteObject(HGDIOBJ(f.0));
    }
    let mut t = inner;
    draw_string(hdc, label, &mut t, (10.0 * scale) as i32, accent, true, true);
}

fn segment_rects(rect: &RECT, count: usize, gap: i32) -> Vec<RECT> {
    let total = rect.right - rect.left;
    let w = (total - gap * (count as i32 - 1)) / count as i32;
    (0..count)
        .map(|i| RECT {
            left: rect.left + (i as i32) * (w + gap),
            top: rect.top,
            right: (rect.left + ((i as i32) + 1) * (w + gap) - gap).min(rect.right),
            bottom: rect.bottom,
        })
        .collect()
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct HistoryEntry {
    at: DateTime<Local>,
    /// Pre-formatted HH:MM:SS time, so the listbox paint never re-formats
    /// (or allocates) per row per repaint.
    at_label: String,
    track: TrackInfo,
    state: PlaybackState,
    /// Whether the source session passed the `allowed_sources` filter.
    /// Accepted entries are highlighted; rejected ones render muted so every
    /// media source is visible in the history.
    accepted: bool,
}

struct History {
    entries: VecDeque<HistoryEntry>,
    cap: usize,
}

impl History {
    fn new(cap: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            cap,
        }
    }

    /// Pushes a new entry at the front: the history is a reverse list, newest
    /// first, matching the listbox rendering order.
    fn push(&mut self, entry: HistoryEntry) {
        self.entries.push_front(entry);
        while self.entries.len() > self.cap {
            self.entries.pop_back();
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter()
    }
}

struct CurrentActivity {
    track: TrackInfo,
    state: PlaybackState,
    /// Decoded artwork: premultiplied BGRA at ART_DECODE×ART_DECODE, cached
    /// so paint is a single StretchDIBits (no decode or conversion per paint).
    /// Filled lazily on the first paint that needs it.
    art: Option<Vec<u8>>,
    /// FNV-1a of the artwork bytes this cache was decoded from, so a metadata
    /// refresh with unchanged artwork does not re-decode.
    art_fingerprint: Option<u64>,
    /// A decode failure is cached: with this set, paint skips the retry until
    /// the artwork bytes change, so a corrupt cover is attempted once instead
    /// of on every repaint.
    art_decode_failed: bool,
}

struct MainWindowState {
    hwnd: HWND,
    instance: HINSTANCE,
    config: Arc<RwLock<Config>>,
    queue: EventQueue,
    overlay_hwnd: HWND,
    listbox: HWND,
    current: Option<CurrentActivity>,
    history: History,
    listbox_font: HFONT,
    gray_brush: HBRUSH,
    accent_brush: HBRUSH,
    black_brush: HBRUSH,
    sidebar_bg_brush: HBRUSH,
    sidebar_highlight_brush: HBRUSH,
    settings_border_brush: HBRUSH,
    settings_surface_brush: HBRUSH,
    settings_hover_brush: HBRUSH,
    history_header_brush: HBRUSH,
    history_selected_brush: HBRUSH,
    history_row_even_brush: HBRUSH,
    history_row_odd_brush: HBRUSH,
    notifications_enabled: bool,
    active_pane: Pane,
    /// Hovered settings row (row index, sub-control) for highlight.
    settings_hover: Option<(usize, SettingSub)>,
    /// Native TOOLTIPS_CLASS control showing full history details on hover.
    tooltip_ctrl: HWND,
    /// Number of tools currently registered in the native tooltip (for resync).
    tooltip_count: usize,
    /// Last synced (item count, top index) of the history listbox; the 1 Hz
    /// tooltip timer skips the full rebuild while both are unchanged.
    tooltip_sync: Option<(usize, usize)>,
    /// Set when an event batch changed the list; the tooltips are rebuilt once
    /// per batch instead of once per event.
    tooltips_dirty: bool,
    /// Timestamp of the last "Copy logs" press, for the "Copied" feedback.
    logs_copied_at: Option<Instant>,
}

/// Creates the main window: a maximized tracker with current activity,
/// per-session history, and a tray icon. The caller runs the message loop.
pub fn create_window(config: Arc<RwLock<Config>>, queue: EventQueue, overlay_hwnd: HWND) -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.context("getting the process module")?;
    let instance: HINSTANCE = module.into();
    let class_name = wide("WinGlanceMainWindow");
    register_main_class(instance, &class_name)?;

    let state = Box::new(MainWindowState::new(config.clone(), queue, overlay_hwnd, instance));
    let state_ptr = Box::into_raw(state);
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("WinGlance").as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            0,
            0,
            800,
            600,
            None,
            None,
            instance,
            Some(state_ptr.cast()),
        )
    };
    let hwnd = match hwnd {
        Ok(hwnd) => hwnd,
        Err(error) => {
            // The state box is owned by the window from WM_NCCREATE onward and
            // freed in WM_NCDESTROY. If CreateWindowExW fails after WM_NCCREATE
            // ran, the system tears the window down through WM_NCDESTROY first,
            // so freeing the box here would double-free it. Freeing here only
            // covers the WM_NCCREATE-never-ran case, which cannot happen
            // because the class was just registered above.
            return Err(error.into());
        }
    };

    unsafe {
        if config.read().unwrap().behavior.start_in_tray {
            let _ = ShowWindow(hwnd, SW_HIDE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
        }
    }
    if let Err(error) = install_tray_icon(hwnd) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return Err(error);
    }
    Ok(hwnd)
}

impl MainWindowState {
    fn cfg(&self) -> std::sync::RwLockReadGuard<'_, Config> {
        self.config.read().unwrap()
    }

    /// Mutates the config under a single write-lock scope, then persists it.
    /// Never call `self.cfg()` (a read lock) from inside `mutate`.
    fn mutate_config(&mut self, mutate: impl FnOnce(&mut Config)) {
        let mut cfg = self.config.write().unwrap();
        mutate(&mut cfg);
        if let Err(error) = cfg.save() {
            error!("saving config after a settings change failed: {error}");
        }
    }

    fn new(config: Arc<RwLock<Config>>, queue: EventQueue, overlay_hwnd: HWND, instance: HINSTANCE) -> Self {
        Self {
            hwnd: HWND::default(),
            instance,
            config,
            queue,
            overlay_hwnd,
            listbox: HWND::default(),
            current: None,
            history: History::new(HISTORY_CAP),
            listbox_font: HFONT::default(),
            gray_brush: HBRUSH::default(),
            accent_brush: HBRUSH::default(),
            black_brush: HBRUSH::default(),
            sidebar_bg_brush: HBRUSH::default(),
            sidebar_highlight_brush: HBRUSH::default(),
            settings_border_brush: HBRUSH::default(),
            settings_surface_brush: HBRUSH::default(),
            settings_hover_brush: HBRUSH::default(),
            history_header_brush: HBRUSH::default(),
            history_selected_brush: HBRUSH::default(),
            history_row_even_brush: HBRUSH::default(),
            history_row_odd_brush: HBRUSH::default(),
            notifications_enabled: true,
            active_pane: Pane::Activity,
            settings_hover: None,
            tooltip_ctrl: HWND::default(),
            tooltip_count: 0,
            tooltip_sync: None,
            tooltips_dirty: false,
            logs_copied_at: None,
        }
    }

    fn create_children(&mut self) {
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let font_name = wide("Segoe UI");
        self.listbox_font = unsafe {
            CreateFontW(
                -((13.0 * scale).round() as i32).max(1),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
                PCWSTR(font_name.as_ptr()),
            )
        };
        self.gray_brush = unsafe { CreateSolidBrush(colorref(0x1E, 0x1E, 0x1E)) };
        let accent = self.cfg().appearance.accent_color;
        self.accent_brush = unsafe { CreateSolidBrush(colorref(accent[0], accent[1], accent[2])) };
        // Fixed-color brushes for the panes, created once instead of per paint
        // (a settings repaint previously created ~40 brushes).
        self.black_brush = unsafe { CreateSolidBrush(COLORREF(0)) };
        self.sidebar_bg_brush = unsafe { CreateSolidBrush(COLORREF(0x0A0A0A)) };
        self.sidebar_highlight_brush = unsafe { CreateSolidBrush(COLORREF(0x1A1A2E)) };
        self.settings_border_brush =
            unsafe { CreateSolidBrush(colorref(SETTINGS_BORDER[0], SETTINGS_BORDER[1], SETTINGS_BORDER[2])) };
        self.settings_surface_brush =
            unsafe { CreateSolidBrush(colorref(SETTINGS_SURFACE[0], SETTINGS_SURFACE[1], SETTINGS_SURFACE[2])) };
        self.settings_hover_brush =
            unsafe { CreateSolidBrush(colorref(SETTINGS_HOVER[0], SETTINGS_HOVER[1], SETTINGS_HOVER[2])) };
        // History-row brushes: a fixed four-color set, created once instead of
        // per owner-draw row (every scroll tick repaints every visible row).
        self.history_header_brush = unsafe { CreateSolidBrush(COLORREF(0x00141414)) };
        self.history_selected_brush = unsafe { CreateSolidBrush(COLORREF(0x001D2B26)) };
        self.history_row_even_brush = unsafe { CreateSolidBrush(COLORREF(0)) };
        self.history_row_odd_brush = unsafe { CreateSolidBrush(COLORREF(0x000E0E0E)) };

        self.listbox = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                PCWSTR(wide("LISTBOX").as_ptr()),
                PCWSTR::null(),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_VSCROLL
                    | WINDOW_STYLE(LBS_OWNERDRAWFIXED as u32 | LBS_HASSTRINGS as u32 | LBS_NOINTEGRALHEIGHT as u32),
                0,
                0,
                0,
                0,
                self.hwnd,
                HMENU(LISTBOX_ID as *mut c_void),
                self.instance,
                None,
            )
        }
        .unwrap_or_default();
        if !self.listbox.0.is_null() {
            unsafe {
                let scale = GetDpiForWindow(self.hwnd).max(96) as f32 / 96.0;
                let item_h = (18.0 * scale).round() as i32;
                let _ = SendMessageW(self.listbox, LB_SETITEMHEIGHT, WPARAM(0), LPARAM(item_h as isize));
                let _ = SendMessageW(
                    self.listbox,
                    WM_SETFONT,
                    WPARAM(self.listbox_font.0 as usize),
                    LPARAM(1),
                );
                let header = wide("TIME     EVENT");
                let _ = SendMessageW(self.listbox, LB_ADDSTRING, WPARAM(0), LPARAM(header.as_ptr() as isize));
            }
            self.layout();
            self.install_tooltip();
        }
    }

    /// Creates the native TOOLTIPS_CLASS control for the history listbox.
    /// Windows subclasses the listbox internally (TTF_SUBCLASS) and fetches
    /// per-item text through TTN_GETDISPINFO, so no custom window procs or
    /// subclassing are needed here.
    fn install_tooltip(&mut self) {
        self.tooltip_ctrl = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST,
                TOOLTIPS_CLASSW,
                PCWSTR::null(),
                WINDOW_STYLE(TTS_NOPREFIX | TTS_ALWAYSTIP) | WS_POPUP,
                0,
                0,
                0,
                0,
                self.hwnd,
                None,
                self.instance,
                None,
            )
        }
        .unwrap_or_default();
        if !self.tooltip_ctrl.0.is_null() {
            unsafe {
                let _ = SendMessageW(self.tooltip_ctrl, TTM_SETMAXTIPWIDTH, WPARAM(0), LPARAM(600));
                let _ = SetTimer(self.hwnd, TIMER_TOOLTIPS_ID, 1000, None);
            }
            self.sync_tooltips();
        }
    }

    /// Rebuilds the per-item tool definitions so rects and row count match
    /// the listbox (rows are fixed-height, so scroll changes the mapping).
    /// The 1 Hz timer calls this constantly, so a full rebuild (3N+1
    /// SendMessageW) is skipped when the item count and scroll position are
    /// unchanged since the last sync. While the window is hidden in the tray
    /// there is nothing to sync, so the timer's two probe messages are
    /// skipped entirely (the show path re-syncs on restore).
    fn sync_tooltips(&mut self) {
        if !unsafe { IsWindowVisible(self.hwnd).as_bool() } {
            return;
        }
        if self.tooltip_ctrl.0.is_null() || self.listbox.0.is_null() {
            return;
        }
        unsafe {
            let count = SendMessageW(self.listbox, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0 as usize;
            let top = SendMessageW(self.listbox, LB_GETTOPINDEX, WPARAM(0), LPARAM(0)).0 as usize;
            if self.tooltip_sync == Some((count, top)) {
                return;
            }
            for index in 0..self.tooltip_count {
                let mut tool = ToolInfo {
                    cb_size: std::mem::size_of::<ToolInfo>() as u32,
                    u_flags: 0,
                    hwnd: self.listbox,
                    u_id: index,
                    rect: RECT::default(),
                    hinst: HINSTANCE::default(),
                    lpsz_text: std::ptr::null_mut(),
                    l_param: 0,
                    lp_reserved: std::ptr::null_mut(),
                };
                let _ = SendMessageW(
                    self.tooltip_ctrl,
                    TTM_DELTOOLW,
                    WPARAM(0),
                    LPARAM(&mut tool as *mut _ as isize),
                );
            }
            for index in 0..count {
                let mut rect = RECT::default();
                let ok = SendMessageW(
                    self.listbox,
                    LB_GETITEMRECT,
                    WPARAM(index),
                    LPARAM(&mut rect as *mut _ as isize),
                );
                if ok.0 == 0 {
                    continue;
                }
                let mut tool = ToolInfo {
                    cb_size: std::mem::size_of::<ToolInfo>() as u32,
                    u_flags: TTF_SUBCLASS.0,
                    hwnd: self.listbox,
                    u_id: index,
                    rect,
                    hinst: HINSTANCE::default(),
                    lpsz_text: LPSTR_TEXTCALLBACK as *mut u16,
                    l_param: 0,
                    lp_reserved: std::ptr::null_mut(),
                };
                let _ = SendMessageW(
                    self.tooltip_ctrl,
                    TTM_ADDTOOLW,
                    WPARAM(0),
                    LPARAM(&mut tool as *mut _ as isize),
                );
            }
            self.tooltip_count = count;
            self.tooltip_sync = Some((count, top));
        }
    }

    /// Text for the native tooltip: the column header for row 0, otherwise
    /// the full details of the entry at the given row.
    fn tooltip_text_for(&self, row: usize) -> Option<String> {
        if row == 0 {
            return Some("TIME | STATE | TITLE | ARTIST | ALBUM | SOURCE".to_string());
        }
        self.history.entries.get(row - 1).map(entry_detail)
    }

    fn receive_events(&mut self) {
        let mut batch = Vec::new();
        if let Ok(mut queue) = self.queue.lock() {
            while let Some(event) = queue.pop_front() {
                batch.push(event);
            }
        }
        for event in batch {
            match event {
                MediaEvent::TrackChanged(track) => self.add_track(track),
                MediaEvent::PlaybackStateChanged(state, _source_app) => {
                    if let Some(current) = &mut self.current {
                        current.state = state;
                        self.add_state_change(state);
                        self.invalidate();
                    }
                }
                MediaEvent::SessionRejected {
                    source_app,
                    title,
                    artist,
                    state,
                    accepted,
                } => self.add_session(source_app, title, artist, state, accepted),
            }
        }
        // One tooltip rebuild per batch: a session-churn burst otherwise
        // rebuilds the full tool set once per event.
        if self.tooltips_dirty {
            self.tooltips_dirty = false;
            self.sync_tooltips();
        }
    }

    /// Appends a history row and syncs the listbox + tooltips. Artwork is
    /// stripped before storing — the history is text-only, and the raw image
    /// bytes would be pure waste across hundreds of rows.
    fn push_history(&mut self, mut track: TrackInfo, state: PlaybackState, accepted: bool) {
        track.artwork = None;
        let at = Local::now();
        let at_label = at.format("%H:%M:%S").to_string();
        let row = history_row(&track, at, state);
        let row = wide(&row);
        let before = self.history.len();
        self.history.push(HistoryEntry {
            at,
            at_label,
            track,
            state,
            accepted,
        });
        if self.history.len() <= before && before > 0 {
            // The cap dropped the oldest entry, which sits at the bottom of
            // the listbox (newest-first rendering, header at index 0).
            let count = unsafe { SendMessageW(self.listbox, LB_GETCOUNT, WPARAM(0), LPARAM(0)) }.0 as usize;
            if count > 0 {
                let _ = unsafe { SendMessageW(self.listbox, LB_DELETESTRING, WPARAM(count - 1), LPARAM(0)) };
            }
        }
        if !self.listbox.0.is_null() {
            unsafe {
                // Insert after the header row (index 0), so the newest entry
                // is the first data row.
                let _ = SendMessageW(self.listbox, LB_INSERTSTRING, WPARAM(1), LPARAM(row.as_ptr() as isize));
                let _ = SendMessageW(self.listbox, LB_SETTOPINDEX, WPARAM(0), LPARAM(0));
            }
        }
        // Tooltip rebuilds are coalesced per event batch (receive_events) or
        // picked up by the 1 Hz timer.
        self.tooltips_dirty = true;
    }

    fn add_state_change(&mut self, state: PlaybackState) {
        let Some(current) = &self.current else {
            return;
        };
        // Skip a state row that duplicates the newest one for the same source
        // (same track, same state). Session recreation re-reports "Playing"
        // for the same song, which would otherwise flood the history with
        // identical rows while the user never changed anything. Rejected
        // sessions from other sources can interleave on top; the newest
        // same-source row is the one to compare against, not just the front.
        if duplicate_state_row(&self.history.entries, current, state) {
            return;
        }
        // Clone text-only: the artwork bytes (up to MBs) are stripped before
        // the clone so they are never copied just to be discarded.
        let mut track = current.track.clone();
        track.artwork = None;
        self.push_history(track, state, true);
    }

    /// Records a session that was seen but not tracked (filtered by
    /// `allowed_sources` or on the churn cool-down). The row renders muted;
    /// it never becomes the "Now Playing" activity.
    fn add_session(&mut self, source_app: String, title: String, artist: String, state: PlaybackState, accepted: bool) {
        let track = TrackInfo {
            title,
            artist,
            source_app,
            ..TrackInfo::default()
        };
        self.push_history(track, state, accepted);
    }

    fn add_track(&mut self, track: TrackInfo) {
        let art_fingerprint = track.artwork.as_deref().map(fingerprint);
        // Metadata refresh for the same song (album/artwork arriving late): update
        // the current activity and the last history row in place instead of
        // appending a duplicate entry.
        let is_update = self
            .current
            .as_ref()
            .is_some_and(|c| c.track.title == track.title && c.track.artist == track.artist);

        if is_update {
            if let Some(current) = &mut self.current {
                current.track = track.clone();
                // Artwork is decoded lazily on first paint; a metadata refresh
                // re-reporting the same cover must not re-decode, so only bump
                // the fingerprint and drop the cached bitmap when bytes changed.
                if current.art_fingerprint != art_fingerprint {
                    current.art = None;
                    current.art_fingerprint = art_fingerprint;
                    current.art_decode_failed = false;
                }
            }
            // Rejected-session rows can be pushed on top of the current
            // track's row, so find the entry by identity instead of assuming
            // it is the newest.
            let entry_index = self.history.entries.iter().position(|e| {
                e.track.title == track.title && e.track.artist == track.artist && e.track.source_app == track.source_app
            });
            if let Some(index) = entry_index {
                let entry = &mut self.history.entries[index];
                entry.track = track.clone();
                entry.track.artwork = None;
                // Keep the row's original timestamp: only the metadata
                // refreshed, and the tooltip formats from the same `at`.
                let row = history_row(
                    &track,
                    entry.at,
                    self.current.as_ref().map(|c| c.state).unwrap_or(PlaybackState::Playing),
                );
                let row = wide(&row);
                if !self.listbox.0.is_null() {
                    unsafe {
                        // The header occupies row 0; data rows mirror the
                        // entries order (newest first).
                        let lb_row = index + 1;
                        let _ = SendMessageW(self.listbox, LB_DELETESTRING, WPARAM(lb_row), LPARAM(0));
                        let _ = SendMessageW(
                            self.listbox,
                            LB_INSERTSTRING,
                            WPARAM(lb_row),
                            LPARAM(row.as_ptr() as isize),
                        );
                    }
                }
            }
            self.tooltips_dirty = true;
            self.invalidate();
            return;
        }

        let state = self.current.as_ref().map(|c| c.state).unwrap_or(PlaybackState::Playing);
        // History row is text-only: strip the artwork bytes before the clone.
        let mut history_track = track.clone();
        history_track.artwork = None;
        self.push_history(history_track, state, true);
        self.current = Some(CurrentActivity {
            track,
            state: self
                .current
                .as_ref()
                .map(|current| current.state)
                .unwrap_or(PlaybackState::Playing),
            // Art is decoded lazily on first paint; the window starts hidden
            // (start_in_tray), so a track that never gets looked at pays no
            // decode cost.
            art: None,
            art_fingerprint,
            art_decode_failed: false,
        });
        self.invalidate();
    }

    fn paint(&mut self) {
        let mut paint = PAINTSTRUCT::default();
        let hdc = unsafe { BeginPaint(self.hwnd, &mut paint) };
        if hdc.0.is_null() {
            return;
        }
        // The history listbox belongs to the Activity pane only.
        unsafe {
            let _ = ShowWindow(
                self.listbox,
                if self.active_pane == Pane::Activity {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            );
        }
        if self.active_pane != Pane::Activity {
            unsafe {
                let _ = ShowWindow(self.tooltip_ctrl, SW_HIDE);
            }
        }
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let (client_w, client_h) = client_size(self.hwnd);
        let sidebar_w = (SIDEBAR_W * scale).round() as i32;
        let content_left = sidebar_w;
        let whole = RECT {
            left: 0,
            top: 0,
            right: client_w,
            bottom: client_h,
        };
        let pad = (PAD * scale) as i32;

        // Fill background
        unsafe {
            let _ = FillRect(hdc, &whole, self.black_brush);
        }

        // Draw sidebar
        let sidebar_rect = RECT {
            left: 0,
            top: 0,
            right: sidebar_w,
            bottom: client_h,
        };
        unsafe {
            let _ = FillRect(hdc, &sidebar_rect, self.sidebar_bg_brush);
        }

        // Sidebar items
        let item_h = (32.0 * scale) as i32;
        let items = [("Now Playing", Pane::Activity), ("Settings", Pane::Settings)];
        let accent = self.cfg().appearance.accent_color;
        for (i, (label, pane)) in items.iter().enumerate() {
            let y = (40.0 * scale) as i32 + (i as i32) * (item_h + (4.0 * scale) as i32);
            let item_rect = RECT {
                left: 4,
                top: y,
                right: sidebar_w - 4,
                bottom: y + item_h,
            };
            if *pane == self.active_pane {
                unsafe {
                    let _ = FillRect(hdc, &item_rect, self.sidebar_highlight_brush);
                }
            }
            let mut text_rect = item_rect;
            draw_string(
                hdc,
                label,
                &mut text_rect,
                (10.0 * scale) as i32,
                if *pane == self.active_pane {
                    accent
                } else {
                    [0x88, 0x88, 0x88, 0xFF]
                },
                *pane == self.active_pane,
                true,
            );
        }

        // Draw content based on active pane
        match self.active_pane {
            Pane::Activity => self.paint_activity(hdc, content_left, client_w, client_h, scale, pad),
            Pane::Settings => self.paint_settings(hdc, content_left, client_w, client_h, scale, pad),
        }

        unsafe {
            let _ = EndPaint(self.hwnd, &paint);
        }
    }

    fn paint_activity(&mut self, hdc: HDC, content_left: i32, client_w: i32, _client_h: i32, scale: f32, pad: i32) {
        let mut header_rect = RECT {
            left: content_left + pad,
            top: pad,
            right: client_w - pad,
            bottom: pad + (HEADER_H * scale) as i32,
        };
        draw_string(
            hdc,
            "NOW PLAYING",
            &mut header_rect,
            (11.0 * scale) as i32,
            self.cfg().appearance.accent_color,
            true,
            false,
        );

        let art = (ART_SIZE * scale).round() as i32;
        let art_x = content_left + pad;
        let art_y = (ART_Y * scale) as i32;
        let text_left = art_x + art + (12.0 * scale) as i32;
        let text_right = client_w - pad;

        let accent_color = self.cfg().appearance.accent_color;
        let text_color = self.cfg().appearance.text_color;

        // Decode lazily here: the window starts hidden, so the first paint is
        // the first time the art is actually needed.
        if let Some(current) = &mut self.current
            && current.art.is_none()
            && !current.art_decode_failed
            && current.art_fingerprint.is_some()
        {
            current.art = current
                .track
                .artwork
                .as_deref()
                .and_then(|data| decode_artwork_pm(data, ART_DECODE as usize));
            current.art_decode_failed = current.art.is_none();
        }

        if let Some(current) = &self.current {
            // Artwork is cached after first paint; paint just blits it.
            if let Some(art_pixels) = current.art.as_deref() {
                draw_art_pm(hdc, art_pixels, ART_DECODE as i32, art, art_x, art_y);
            } else {
                let art_rect = RECT {
                    left: art_x,
                    top: art_y,
                    right: art_x + art,
                    bottom: art_y + art,
                };
                unsafe {
                    let _ = FillRect(hdc, &art_rect, self.accent_brush);
                }
            }
            let state_label = match current.state {
                PlaybackState::Playing => "Playing",
                PlaybackState::Paused => "Paused",
                PlaybackState::Stopped => "Stopped",
                PlaybackState::NowPlaying => "Playing",
            };
            let state_color = if current.state == PlaybackState::Playing {
                accent_color
            } else {
                [0xBB, 0xBB, 0xBB, 0xFF]
            };
            let mut state_rect = RECT {
                left: text_left,
                top: art_y,
                right: text_right,
                bottom: art_y + (18.0 * scale) as i32,
            };
            draw_string(
                hdc,
                state_label,
                &mut state_rect,
                (11.0 * scale) as i32,
                state_color,
                true,
                false,
            );

            let mut title_rect = RECT {
                left: text_left,
                top: art_y + (22.0 * scale) as i32,
                right: text_right,
                bottom: art_y + (48.0 * scale) as i32,
            };
            draw_string(
                hdc,
                &current.track.title,
                &mut title_rect,
                (19.0 * scale) as i32,
                text_color,
                true,
                false,
            );

            let subtitle = if current.track.artist.trim().is_empty() {
                "Unknown"
            } else {
                &current.track.artist
            };
            let mut artist_rect = RECT {
                left: text_left,
                top: art_y + (48.0 * scale) as i32,
                right: text_right,
                bottom: art_y + (72.0 * scale) as i32,
            };
            draw_string(
                hdc,
                subtitle,
                &mut artist_rect,
                (14.0 * scale) as i32,
                [0xCC, 0xCC, 0xCC, 0xFF],
                false,
                false,
            );

            if !current.track.album.trim().is_empty() {
                let mut album_rect = RECT {
                    left: text_left,
                    top: art_y + (70.0 * scale) as i32,
                    right: text_right,
                    bottom: art_y + (86.0 * scale) as i32,
                };
                draw_string(
                    hdc,
                    &current.track.album,
                    &mut album_rect,
                    (12.0 * scale) as i32,
                    [0x99, 0x99, 0x99, 0xFF],
                    false,
                    false,
                );
            }
            let extra = current.track.meta_line(false);
            if !extra.is_empty() {
                let mut extra_rect = RECT {
                    left: text_left,
                    top: art_y + (86.0 * scale) as i32,
                    right: text_right,
                    bottom: art_y + (100.0 * scale) as i32,
                };
                draw_string(
                    hdc,
                    &extra,
                    &mut extra_rect,
                    (11.0 * scale) as i32,
                    [0x88, 0x88, 0x88, 0xFF],
                    false,
                    false,
                );
            }
            if !current.track.source_app.trim().is_empty() {
                let mut app_rect = RECT {
                    left: text_left,
                    top: art_y + (100.0 * scale) as i32,
                    right: text_right,
                    bottom: art_y + (114.0 * scale) as i32,
                };
                draw_string(
                    hdc,
                    &current.track.source_app,
                    &mut app_rect,
                    (10.0 * scale) as i32,
                    [0x77, 0x77, 0x77, 0xFF],
                    false,
                    false,
                );
            }
        } else {
            let mut empty_rect = RECT {
                left: text_left,
                top: art_y + (16.0 * scale) as i32,
                right: text_right,
                bottom: art_y + art,
            };
            draw_string(
                hdc,
                "No media playing",
                &mut empty_rect,
                (15.0 * scale) as i32,
                [0x99, 0x99, 0x99, 0xFF],
                false,
                false,
            );
        }

        let sep_y = art_y + art + (SEP_GAP * scale) as i32;
        let separator = RECT {
            left: content_left,
            top: sep_y,
            right: client_w,
            bottom: sep_y + 1,
        };
        unsafe {
            let _ = FillRect(hdc, &separator, self.gray_brush);
        }

        let mut history_rect = RECT {
            left: content_left + pad,
            top: sep_y + (HIST_GAP * scale) as i32,
            right: client_w - pad,
            bottom: sep_y + ((HIST_GAP + HIST_H) * scale) as i32,
        };
        draw_string(
            hdc,
            "SESSION HISTORY",
            &mut history_rect,
            (11.0 * scale) as i32,
            [0x99, 0x99, 0x99, 0xFF],
            true,
            false,
        );

        let pos_y = history_rect.bottom + (4.0 * scale) as i32;
        let pos_label = if self.cfg().overlay.position_x.is_some() {
            format!(
                "Position: custom ({}, {})",
                self.cfg().overlay.position_x.unwrap_or(0),
                self.cfg().overlay.position_y.unwrap_or(0)
            )
        } else {
            format!(
                "Position: {}-{}",
                match self.cfg().overlay.vertical {
                    VerticalPosition::Top => "top",
                    VerticalPosition::Bottom => "bottom",
                },
                match self.cfg().overlay.horizontal {
                    HorizontalPosition::Left => "left",
                    HorizontalPosition::Center => "center",
                    HorizontalPosition::Right => "right",
                }
            )
        };
        let mut pos_rect = RECT {
            left: content_left + pad,
            top: pos_y,
            right: client_w - pad,
            bottom: pos_y + (16.0 * scale) as i32,
        };
        draw_string(
            hdc,
            &pos_label,
            &mut pos_rect,
            (10.0 * scale) as i32,
            [0x66, 0x66, 0x66, 0xFF],
            false,
            false,
        );
    }

    /// Builds the settings pane items (section headers + interactive rows).
    /// Both painting and hit-testing use this single source of layout truth.
    fn settings_items(&self, content_left: i32, client_w: i32, pad: i32, scale: f32) -> Vec<SettingsItem> {
        let row_h = (34.0 * scale) as i32;
        let gap = (8.0 * scale) as i32;
        let header_h = (18.0 * scale) as i32;
        let left = content_left + pad;
        let right = client_w - pad;
        let mut y = pad + (36.0 * scale) as i32;
        let mut items = Vec::new();

        items.push(SettingsItem::Header {
            text: "Behavior",
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + header_h,
            },
        });
        y += (22.0 * scale) as i32;
        for id in [
            SettingId::Notifications,
            SettingId::Duration,
            SettingId::StartOnLogin,
            SettingId::CloseToTray,
            SettingId::AllowedApps,
        ] {
            items.push(SettingsItem::Row {
                id,
                rect: RECT {
                    left,
                    top: y,
                    right,
                    bottom: y + row_h,
                },
            });
            y += row_h + gap;
        }
        y += (14.0 * scale) as i32;
        items.push(SettingsItem::Header {
            text: "Overlay",
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + header_h,
            },
        });
        y += (22.0 * scale) as i32;
        items.push(SettingsItem::Row {
            id: SettingId::Position,
            rect: RECT {
                left,
                top: y,
                right,
                // Taller row: value/Reset line + anchor segments line.
                bottom: y + (70.0 * scale) as i32,
            },
        });
        y += (70.0 * scale) as i32 + gap;
        items.push(SettingsItem::Row {
            id: SettingId::ShowSample,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        y += (14.0 * scale) as i32;
        items.push(SettingsItem::Header {
            text: "Diagnostics",
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + header_h,
            },
        });
        y += (22.0 * scale) as i32;
        items.push(SettingsItem::Row {
            id: SettingId::CopyLogs,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        items
    }

    fn paint_settings(&self, hdc: HDC, content_left: i32, client_w: i32, _client_h: i32, scale: f32, pad: i32) {
        // Read the config once per paint instead of ~10 lock acquisitions,
        // and snapshot the hover/flag state so the row loop stays pure.
        let cfg = self.cfg();
        let accent = cfg.appearance.accent_color;
        let accent_soft = mix(accent, [0x1B, 0x1B, 0x1B, 0xFF], 0.28);
        let notifications_enabled = self.notifications_enabled;
        let settings_hover = self.settings_hover;
        let duration_ms = cfg.overlay.duration_ms;
        let start_on_login = cfg.behavior.start_on_login;
        let close_to_tray = cfg.behavior.close_to_tray;
        let allowed_sources = cfg.behavior.allowed_sources.join(", ");
        let custom_position = cfg.overlay.position_x.is_some();
        let position_label = self.position_label();

        let mut hdr = RECT {
            left: content_left + pad,
            top: pad,
            right: client_w - pad,
            bottom: pad + (24.0 * scale) as i32,
        };
        draw_string(hdc, "SETTINGS", &mut hdr, (13.0 * scale) as i32, accent, true, false);

        let items = self.settings_items(content_left, client_w, pad, scale);
        let brushes = SettingsBrushes {
            border: self.settings_border_brush,
            surface: self.settings_surface_brush,
            hover: self.settings_hover_brush,
        };
        let mut row_index = 0usize;
        for item in &items {
            match item {
                SettingsItem::Header { text, rect } => {
                    let mut hr = *rect;
                    draw_string(hdc, text, &mut hr, (9.0 * scale) as i32, SETTINGS_FAINT, true, false);
                }
                SettingsItem::Row { id, rect } => {
                    let hovered_row = settings_hover.is_some_and(|(r, _)| r == row_index);
                    let label_w = (((rect.right - rect.left) as f32) * 0.42) as i32;
                    let label_rect = RECT {
                        left: rect.left + (12.0 * scale) as i32,
                        top: rect.top,
                        right: rect.left + label_w,
                        bottom: rect.bottom,
                    };
                    let control_left = rect.left + label_w + (10.0 * scale) as i32;
                    let control_rect = RECT {
                        left: control_left,
                        top: rect.top,
                        right: rect.right - (10.0 * scale) as i32,
                        bottom: rect.bottom,
                    };

                    // Card: border + surface fill (+ hover tint)
                    unsafe {
                        let _ = FillRect(hdc, rect, self.settings_border_brush);
                    }
                    let inner = RECT {
                        left: rect.left + 1,
                        top: rect.top + 1,
                        right: rect.right - 1,
                        bottom: rect.bottom - 1,
                    };
                    unsafe {
                        let bg = if hovered_row {
                            self.settings_hover_brush
                        } else {
                            self.settings_surface_brush
                        };
                        let _ = FillRect(hdc, &inner, bg);
                    }

                    let (label, value_text, value_color) = match id {
                        SettingId::Notifications => (
                            "Notifications",
                            if notifications_enabled {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if notifications_enabled { accent } else { SETTINGS_FAINT },
                        ),
                        SettingId::StartOnLogin => (
                            "Start on login",
                            if start_on_login {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if start_on_login { accent } else { SETTINGS_FAINT },
                        ),
                        SettingId::CloseToTray => (
                            "Close to tray",
                            if close_to_tray {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if close_to_tray { accent } else { SETTINGS_FAINT },
                        ),
                        SettingId::Duration => ("Duration", format!("{}s", duration_ms / 1000), SETTINGS_MUTED),
                        SettingId::Position => ("Position", position_label.clone(), SETTINGS_MUTED),
                        SettingId::AllowedApps => (
                            "Allowed apps",
                            if allowed_sources.is_empty() {
                                "All".to_string()
                            } else {
                                allowed_sources.clone()
                            },
                            SETTINGS_MUTED,
                        ),
                        SettingId::ShowSample => ("Show sample", String::new(), SETTINGS_MUTED),
                        SettingId::CopyLogs => ("Logs", String::new(), SETTINGS_MUTED),
                    };
                    let mut lbl_rect = label_rect;
                    draw_string(
                        hdc,
                        label,
                        &mut lbl_rect,
                        (11.0 * scale) as i32,
                        SETTINGS_MUTED,
                        false,
                        false,
                    );

                    match id {
                        SettingId::Notifications
                        | SettingId::StartOnLogin
                        | SettingId::CloseToTray
                        | SettingId::AllowedApps => {
                            let mut val_rect = control_rect;
                            draw_string(
                                hdc,
                                &value_text,
                                &mut val_rect,
                                (11.0 * scale) as i32,
                                value_color,
                                true,
                                false,
                            );
                        }
                        SettingId::Duration => {
                            let segments = segment_rects(&control_rect, 4, (4.0 * scale) as i32);
                            let values = [2000u64, 3000, 5000, 10000];
                            let exact = values.contains(&duration_ms);
                            // Nearest preset, for when the config holds a value
                            // outside the four presets (e.g. hand-edited).
                            let nearest = values
                                .iter()
                                .enumerate()
                                .min_by_key(|(_, v)| v.abs_diff(duration_ms))
                                .map(|(i, _)| i)
                                .unwrap_or(0);
                            for (i, seg) in segments.iter().enumerate() {
                                let active = duration_ms == values[i];
                                let near = !exact && i == nearest;
                                let seg_hovered = settings_hover == Some((row_index, SettingSub::Seg(i)));
                                let border = if active || near {
                                    colorref(accent[0], accent[1], accent[2])
                                } else {
                                    colorref(SETTINGS_BORDER[0], SETTINGS_BORDER[1], SETTINGS_BORDER[2])
                                };
                                let b = unsafe { CreateSolidBrush(border) };
                                unsafe {
                                    let _ = FillRect(hdc, seg, b);
                                }
                                unsafe {
                                    let _ = DeleteObject(HGDIOBJ(b.0));
                                }
                                let s_inner = RECT {
                                    left: seg.left + 1,
                                    top: seg.top + 1,
                                    right: seg.right - 1,
                                    bottom: seg.bottom - 1,
                                };
                                let fill = if active {
                                    accent_soft
                                } else if near {
                                    // Approximate preset: dimmer accent fill than the
                                    // exact match, so "saved but not exact" is visible.
                                    mix(accent, [0x1B, 0x1B, 0x1B, 0xFF], 0.55)
                                } else if seg_hovered {
                                    SETTINGS_HOVER
                                } else {
                                    SETTINGS_SURFACE
                                };
                                let f = unsafe { CreateSolidBrush(colorref(fill[0], fill[1], fill[2])) };
                                unsafe {
                                    let _ = FillRect(hdc, &s_inner, f);
                                }
                                unsafe {
                                    let _ = DeleteObject(HGDIOBJ(f.0));
                                }
                                let mut t = s_inner;
                                let tc = if active || near { SETTINGS_TEXT } else { SETTINGS_MUTED };
                                let label = if near {
                                    format!("≈{}s", values[i] / 1000)
                                } else {
                                    format!("{}s", values[i] / 1000)
                                };
                                draw_string(hdc, &label, &mut t, (10.0 * scale) as i32, tc, active || near, true);
                            }
                        }
                        SettingId::Position => {
                            let parts = position_parts(rect, scale);
                            let active_anchor = if custom_position {
                                None
                            } else {
                                Some(match (cfg.overlay.vertical, cfg.overlay.horizontal) {
                                    (VerticalPosition::Top, HorizontalPosition::Left) => 0,
                                    (VerticalPosition::Top, HorizontalPosition::Center) => 1,
                                    (VerticalPosition::Top, HorizontalPosition::Right) => 2,
                                    (VerticalPosition::Bottom, HorizontalPosition::Left) => 3,
                                    (VerticalPosition::Bottom, HorizontalPosition::Center) => 4,
                                    (VerticalPosition::Bottom, HorizontalPosition::Right) => 5,
                                })
                            };

                            // Value + Reset button row
                            let mut v = parts.value_row;
                            draw_string(
                                hdc,
                                &value_text,
                                &mut v,
                                (10.0 * scale) as i32,
                                SETTINGS_FAINT,
                                false,
                                false,
                            );
                            let reset_hovered = settings_hover == Some((row_index, SettingSub::Reset));
                            draw_small_button(hdc, &parts.reset, "Reset", accent, reset_hovered, scale);

                            // Anchor segments + Adjust button row
                            for (i, seg) in parts.anchors.iter().enumerate() {
                                let active = active_anchor == Some(i);
                                let seg_hovered = settings_hover == Some((row_index, SettingSub::Anchor(i)));
                                draw_segment_button(
                                    hdc,
                                    seg,
                                    ANCHOR_LABELS[i],
                                    active,
                                    seg_hovered,
                                    accent,
                                    accent_soft,
                                    scale,
                                    brushes,
                                );
                            }
                            let adjust_hovered = settings_hover == Some((row_index, SettingSub::Adjust));
                            let adjust_fill = if adjust_hovered {
                                mix(accent, [0x1B, 0x1B, 0x1B, 0xFF], 0.45)
                            } else {
                                accent_soft
                            };
                            let b =
                                unsafe { CreateSolidBrush(colorref(adjust_fill[0], adjust_fill[1], adjust_fill[2])) };
                            unsafe {
                                let _ = FillRect(hdc, &parts.adjust, b);
                            }
                            unsafe {
                                let _ = DeleteObject(HGDIOBJ(b.0));
                            }
                            let mut bt = parts.adjust;
                            draw_string(hdc, "Adjust…", &mut bt, (10.0 * scale) as i32, accent, true, true);
                        }
                        SettingId::ShowSample => {
                            let btn_rect = RECT {
                                left: control_rect.left,
                                top: control_rect.top,
                                right: control_rect.right,
                                bottom: control_rect.bottom,
                            };
                            let hovered = self.settings_hover == Some((row_index, SettingSub::None));
                            draw_small_button(hdc, &btn_rect, "Preview the notification", accent, hovered, scale);
                        }
                        SettingId::CopyLogs => {
                            let btn_rect = RECT {
                                left: control_rect.left,
                                top: control_rect.top,
                                right: control_rect.right,
                                bottom: control_rect.bottom,
                            };
                            let hovered = self.settings_hover == Some((row_index, SettingSub::None));
                            let copied = self
                                .logs_copied_at
                                .is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                            draw_small_button(
                                hdc,
                                &btn_rect,
                                if copied { "Copied" } else { "Copy logs" },
                                accent,
                                hovered,
                                scale,
                            );
                        }
                    }
                    row_index += 1;
                }
            }
        }
    }

    fn position_label(&self) -> String {
        if self.cfg().overlay.position_x.is_some() {
            format!(
                "Custom ({}, {})",
                self.cfg().overlay.position_x.unwrap_or(0),
                self.cfg().overlay.position_y.unwrap_or(0)
            )
        } else {
            format!(
                "{}-{}",
                match self.cfg().overlay.vertical {
                    VerticalPosition::Top => "top",
                    VerticalPosition::Bottom => "bottom",
                },
                match self.cfg().overlay.horizontal {
                    HorizontalPosition::Left => "left",
                    HorizontalPosition::Center => "center",
                    HorizontalPosition::Right => "right",
                }
            )
        }
    }

    /// Computes which settings control is under a client-space point, for hover
    /// highlighting. Returns (row index, segment index) where segment is None for
    /// whole-row controls and Some(i) for the i-th duration segment.
    fn settings_hover_at(
        &self,
        x: i32,
        y: i32,
        content_left: i32,
        client_w: i32,
        pad: i32,
        scale: f32,
    ) -> Option<(usize, SettingSub)> {
        let items = self.settings_items(content_left, client_w, pad, scale);
        let mut row_index = 0usize;
        for item in &items {
            if let SettingsItem::Row { id, rect } = item
                && y >= rect.top
                && y < rect.bottom
            {
                let label_w = (((rect.right - rect.left) as f32) * 0.42) as i32;
                let control_left = rect.left + label_w + (10.0 * scale) as i32;
                let control_rect = RECT {
                    left: control_left,
                    top: rect.top,
                    right: rect.right - (10.0 * scale) as i32,
                    bottom: rect.bottom,
                };
                if *id == SettingId::Duration {
                    let segments = segment_rects(&control_rect, 4, (4.0 * scale) as i32);
                    let seg = segments.iter().position(|s| x >= s.left && x < s.right);
                    // A click or hover in the gap right of the last segment is
                    // not the first segment; the row stays highlighted.
                    return Some((row_index, seg.map_or(SettingSub::None, SettingSub::Seg)));
                }
                if *id == SettingId::Position {
                    let parts = position_parts(rect, scale);
                    if let Some(i) = parts
                        .anchors
                        .iter()
                        .position(|a| x >= a.left && x < a.right && y >= a.top && y < a.bottom)
                    {
                        return Some((row_index, SettingSub::Anchor(i)));
                    }
                    if x >= parts.reset.left && x < parts.reset.right && y >= parts.reset.top && y < parts.reset.bottom
                    {
                        return Some((row_index, SettingSub::Reset));
                    }
                    if x >= parts.adjust.left
                        && x < parts.adjust.right
                        && y >= parts.adjust.top
                        && y < parts.adjust.bottom
                    {
                        return Some((row_index, SettingSub::Adjust));
                    }
                    return Some((row_index, SettingSub::None));
                }
                return Some((row_index, SettingSub::None));
            }
            // Row index must count rows only, matching paint_settings; headers
            // are skipped here.
            if matches!(item, SettingsItem::Row { .. }) {
                row_index += 1;
            }
        }
        None
    }

    fn layout(&self) {
        if self.listbox.0.is_null() {
            return;
        }
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let (client_w, client_h) = client_size(self.hwnd);
        let pad = (PAD * scale) as i32;
        let top = ((ART_Y + ART_SIZE + SEP_GAP + HIST_GAP + HIST_H + LIST_GAP) * scale).round() as i32;
        let bottom_gap = (BOTTOM_GAP * scale).round() as i32;
        unsafe {
            let _ = SetWindowPos(
                self.listbox,
                HWND::default(),
                pad,
                top,
                (client_w - 2 * pad).max(0),
                (client_h - top - bottom_gap).max(0),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    fn on_close(&self) {
        unsafe {
            if self.cfg().behavior.close_to_tray {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            } else {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    /// Owner-draw handler for the history listbox: paints light text on
    /// alternating black/grey rows, with a distinct header row. Without this,
    /// LBS_OWNERDRAWFIXED items render with default black text on the black
    /// background and are unreadable.
    fn draw_history_item(&self, item: &DRAWITEMSTRUCT) {
        let hdc = item.hDC;
        let index = item.itemID as usize;
        let selected = (item.itemState.0 & ODS_SELECTED.0) != 0;
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;

        let brush = if index == 0 {
            self.history_header_brush
        } else if selected {
            self.history_selected_brush
        } else if index.is_multiple_of(2) {
            self.history_row_even_brush
        } else {
            self.history_row_odd_brush
        };
        unsafe {
            let _ = FillRect(hdc, &item.rcItem, brush);
        }

        // Column layout: TIME | STATE | TITLE | ARTIST | ALBUM | SOURCE.
        let pad = (8.0 * scale) as i32;
        let gap = (4.0 * scale) as i32;
        let time_w = (78.0 * scale) as i32;
        let state_w = (30.0 * scale) as i32;
        let left = item.rcItem.left + pad;
        let rest = (item.rcItem.right - pad - left - time_w - state_w - gap).max(0);
        let title_w = (rest as f32 * 0.34) as i32;
        let artist_w = (rest as f32 * 0.24) as i32;
        let album_w = (rest as f32 * 0.20) as i32;
        let source_w = (rest - title_w - artist_w - album_w).max(0);
        let col_x = [left, left + time_w + gap, left + time_w + gap + state_w + gap];
        let title_x = col_x[2];
        let artist_x = title_x + title_w + gap;
        let album_x = artist_x + artist_w + gap;
        let source_x = album_x + album_w + gap;
        let header_font = (11.0 * scale) as i32;
        let row_font = (13.0 * scale) as i32;
        let header_color = [0x9A, 0x9A, 0x9A, 0xFF];
        let accent_color = self.cfg().appearance.accent_color;

        let cell = |x: i32, w: i32, text: &str, font: i32, color: [u8; 4], bold: bool| {
            if w <= 0 {
                return;
            }
            let mut rect = RECT {
                left: x,
                top: item.rcItem.top,
                right: x + w,
                bottom: item.rcItem.bottom,
            };
            draw_string(hdc, text, &mut rect, font, color, bold, false);
        };

        if index == 0 {
            // Header row.
            cell(col_x[0], time_w, "TIME", header_font, header_color, true);
            cell(col_x[1], state_w, "", header_font, header_color, true);
            cell(title_x, title_w, "TITLE", header_font, header_color, true);
            cell(artist_x, artist_w, "ARTIST", header_font, header_color, true);
            cell(album_x, album_w, "ALBUM", header_font, header_color, true);
            cell(source_x, source_w, "SOURCE", header_font, header_color, true);
        } else if let Some(entry) = self.history.entries.get(index - 1) {
            let status = match entry.state {
                PlaybackState::Playing => "▶",
                PlaybackState::Paused => "‖",
                PlaybackState::Stopped => "■",
                PlaybackState::NowPlaying => "♪",
            };
            let artist = if entry.track.artist.trim().is_empty() {
                ""
            } else {
                &entry.track.artist
            };
            // Accepted sessions are highlighted in pink (the accent color)
            // with bold text; rejected sessions render muted so every media
            // source is visible without stealing attention from tracked ones.
            let (row_color, bold) = if entry.accepted {
                (accent_color, true)
            } else {
                ([0x66, 0x66, 0x66, 0xFF], false)
            };
            cell(col_x[0], time_w, &entry.at_label, row_font, row_color, bold);
            cell(col_x[1], state_w, status, row_font, row_color, bold);
            cell(title_x, title_w, &entry.track.title, row_font, row_color, bold);
            cell(artist_x, artist_w, artist, row_font, row_color, bold);
            cell(album_x, album_w, &entry.track.album, row_font, row_color, bold);
            cell(source_x, source_w, &entry.track.source_app, row_font, row_color, bold);
        }
    }

    fn on_destroy(&mut self) {
        remove_tray_icon(self.hwnd);
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_TOOLTIPS_ID);
            if !self.tooltip_ctrl.0.is_null() {
                let _ = DestroyWindow(self.tooltip_ctrl);
                self.tooltip_ctrl = HWND::default();
            }
            if !self.listbox_font.0.is_null() {
                let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(self.listbox_font.0));
            }
            if !self.gray_brush.0.is_null() {
                let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(self.gray_brush.0));
            }
            if !self.accent_brush.0.is_null() {
                let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(self.accent_brush.0));
            }
            for brush in [
                &self.black_brush,
                &self.sidebar_bg_brush,
                &self.sidebar_highlight_brush,
                &self.settings_border_brush,
                &self.settings_surface_brush,
                &self.settings_hover_brush,
                &self.history_header_brush,
                &self.history_selected_brush,
                &self.history_row_even_brush,
                &self.history_row_odd_brush,
            ] {
                if !brush.0.is_null() {
                    let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(brush.0));
                }
            }
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    /// Invalidates only the given client-space region, so hover highlights
    /// repaint a small band instead of the whole window.
    fn invalidate_rect(&self, rect: &RECT) {
        unsafe {
            let _ = InvalidateRect(self.hwnd, Some(rect), false);
        }
    }

    /// The client-space region the settings pane occupies (right of the
    /// sidebar), repainted on hover changes.
    fn settings_region(&self, client_w: i32, client_h: i32) -> RECT {
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let sidebar_w = (SIDEBAR_W * scale).round() as i32;
        RECT {
            left: sidebar_w,
            top: 0,
            right: client_w,
            bottom: client_h,
        }
    }

    fn show_window(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWMAXIMIZED);
            let _ = SetForegroundWindow(self.hwnd);
        }
        // The window was hidden, so the 1 Hz timer skipped its syncs; rebuild
        // the tool definitions now so hover works immediately on restore.
        self.sync_tooltips();
    }

    /// Copies the current run's log file to the clipboard (UTF-16 with per-line
    /// newlines preserved) and shows a transient "Copied" state.
    fn copy_logs(&mut self) {
        let path = self.cfg().logs_dir().join("log-Live.log");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                debug!("copy logs: reading {path:?} failed: {error}");
                return;
            }
        };
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let bytes = wide.len() * 2;

        unsafe {
            if OpenClipboard(None).is_err() {
                debug!("copy logs: OpenClipboard failed");
                return;
            }
            let _ = EmptyClipboard();
            let ok = GlobalAlloc(GMEM_MOVEABLE, bytes).is_ok_and(|hmem| {
                let ptr = GlobalLock(hmem);
                if ptr.is_null() {
                    // Lock failed: the buffer was never written, so it must
                    // not reach the clipboard (it would hold uninitialized
                    // bytes and the UI would report "Copied" on garbage).
                    let _ = GlobalFree(hmem);
                    return false;
                }
                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr.cast(), wide.len());
                let _ = GlobalUnlock(hmem);
                if SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hmem.0)).is_ok() {
                    true
                } else {
                    // Transfer failed; the memory is still ours to release.
                    let _ = GlobalFree(hmem);
                    false
                }
            });
            let _ = CloseClipboard();
            if !ok {
                debug!("copy logs: clipboard set failed");
                return;
            }
        }

        self.logs_copied_at = Some(Instant::now());
        unsafe {
            let _ = SetTimer(self.hwnd, TIMER_LOGS_ID, 2000, None);
        }
        self.invalidate();
    }

    /// Pins the overlay to a vertical/horizontal anchor: clears any absolute
    /// override, persists the choice, and nudges the live overlay into place.
    fn apply_anchor(&mut self, vertical: VerticalPosition, horizontal: HorizontalPosition) {
        self.mutate_config(|cfg| {
            cfg.overlay.vertical = vertical;
            cfg.overlay.horizontal = horizontal;
            cfg.overlay.position_x = None;
            cfg.overlay.position_y = None;
        });
        set_position(self.overlay_hwnd, OverlayPos::from_config(&self.cfg()));
    }

    /// Clears any custom X/Y override and returns to the default top-center anchor.
    fn reset_position(&mut self) {
        self.apply_anchor(VerticalPosition::Top, HorizontalPosition::Center);
        // If the position adjustor is open, move it back to the default spot too.
        crate::positioner::reset_position();
    }
}

/// Whether a state row for `current` would duplicate the newest history row
/// of the same source (same track, same state). Rejected sessions from other
/// sources can sit on top of the row in question, so the comparison skips
/// interleaved foreign rows instead of only checking the front.
fn duplicate_state_row(entries: &VecDeque<HistoryEntry>, current: &CurrentActivity, state: PlaybackState) -> bool {
    entries
        .iter()
        .find(|e| e.track.source_app == current.track.source_app)
        .is_some_and(|last| {
            last.state == state && last.track.title == current.track.title && last.track.artist == current.track.artist
        })
}

fn history_row(track: &TrackInfo, at: DateTime<Local>, state: PlaybackState) -> String {
    let artist = if track.artist.trim().is_empty() {
        ""
    } else {
        &track.artist
    };
    let status = match state {
        PlaybackState::Playing => "▶",
        PlaybackState::Paused => "‖",
        PlaybackState::Stopped => "■",
        PlaybackState::NowPlaying => "♪",
    };
    let mut row = format!("{}  {}  {} — {}", at.format("%H:%M:%S"), status, track.title, artist);
    if !track.album.trim().is_empty() {
        row.push_str(&format!(" — {}", track.album));
    }
    row
}

/// Blits the cached premultiplied BGRA artwork (decoded once at `base` size
/// when the track changed) into the tile at `px` pixels — no per-paint
/// decode or pixel conversion.
fn draw_art_pm(hdc: HDC, pm: &[u8], base: i32, px: i32, x: i32, y: i32) {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: base,
            biHeight: -base,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let drawn = unsafe {
        StretchDIBits(
            hdc,
            x,
            y,
            px,
            px,
            0,
            0,
            base,
            base,
            Some(pm.as_ptr().cast()),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        )
    };
    if drawn == 0 {
        error!("StretchDIBits failed while drawing artwork");
    }
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rect);
    }
    (rect.right, rect.bottom)
}

/// Full details of a history entry, shown in the hover tooltip.
fn entry_detail(entry: &HistoryEntry) -> String {
    let mut parts = vec![
        format!(
            "{}  {}",
            entry.at.format("%H:%M:%S"),
            match entry.state {
                PlaybackState::Playing => "Playing",
                PlaybackState::Paused => "Paused",
                PlaybackState::Stopped => "Stopped",
                PlaybackState::NowPlaying => "Playing",
            }
        ),
        entry.track.title.clone(),
    ];
    if !entry.accepted {
        parts.push("(filtered by allowed apps)".to_string());
    }
    if !entry.track.artist.trim().is_empty() {
        parts.push(entry.track.artist.clone());
    }
    if !entry.track.album.trim().is_empty() {
        parts.push(entry.track.album.clone());
    }
    // Subtitle and album artist carry useful context when the album title is
    // empty (some apps populate one but not the other).
    if entry.track.album.trim().is_empty() {
        if !entry.track.subtitle.trim().is_empty() {
            parts.push(entry.track.subtitle.clone());
        }
        if !entry.track.album_artist.trim().is_empty() {
            parts.push(entry.track.album_artist.clone());
        }
    }
    let meta = entry.track.meta_line(entry.track.album.trim().is_empty());
    if !meta.is_empty() {
        parts.push(meta);
    }
    if !entry.track.source_app.trim().is_empty() {
        parts.push(entry.track.source_app.clone());
    }
    parts.join("\n")
}

fn register_main_class(instance: HINSTANCE, class_name: &[u16]) -> Result<()> {
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: HBRUSH(unsafe { GetStockObject(windows::Win32::Graphics::Gdi::BLACK_BRUSH) }.0),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: Default::default(),
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        anyhow::bail!("RegisterClassExW failed for main window");
    }
    Ok(())
}

fn install_tray_icon(hwnd: HWND) -> Result<()> {
    let data = tray_data(hwnd)?;
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        anyhow::bail!("Shell_NotifyIconW(NIM_ADD) failed");
    }
    Ok(())
}

fn remove_tray_icon(hwnd: HWND) {
    if let Ok(data) = tray_data(hwnd) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }
}

fn tray_data(hwnd: HWND) -> Result<NOTIFYICONDATAW> {
    let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }?;
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    let tip = wide("WinGlance media overlay");
    let count = tip.len().min(data.szTip.len());
    data.szTip[..count].copy_from_slice(&tip[..count]);
    Ok(data)
}

fn show_tray_menu(state: &mut MainWindowState) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let open_flags = MF_STRING;
    let mut notify_flags = MF_STRING;
    if state.notifications_enabled {
        notify_flags |= MF_CHECKED;
    }
    let mut autostart_flags = MF_STRING;
    if state.cfg().behavior.start_on_login {
        autostart_flags |= MF_CHECKED;
    }
    let mut close_tray_flags = MF_STRING;
    if state.cfg().behavior.close_to_tray {
        close_tray_flags |= MF_CHECKED;
    }
    unsafe {
        let _ = AppendMenuW(menu, open_flags, MENU_OPEN_ID, PCWSTR(wide("Open WinGlance").as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            notify_flags,
            MENU_NOTIFY_ID,
            PCWSTR(wide("Toggle notifications").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            autostart_flags,
            MENU_AUTOSTART_ID,
            PCWSTR(wide("Start with Windows").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            close_tray_flags,
            MENU_CLOSE_TRAY_ID,
            PCWSTR(wide("Close window to tray").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_TOP_LEFT,
            PCWSTR(wide("Position: top-left").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_TOP_CENTER,
            PCWSTR(wide("Position: top-center").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_TOP_RIGHT,
            PCWSTR(wide("Position: top-right").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_BOTTOM_LEFT,
            PCWSTR(wide("Position: bottom-left").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_BOTTOM_CENTER,
            PCWSTR(wide("Position: bottom-center").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_BOTTOM_RIGHT,
            PCWSTR(wide("Position: bottom-right").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_CUSTOM,
            PCWSTR(wide("Adjust position…").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_SAMPLE,
            PCWSTR(wide("Show sample").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_RESET,
            PCWSTR(wide("Reset position").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        // Duration submenu
        let Ok(duration_menu) = CreatePopupMenu() else {
            let _ = DestroyMenu(menu);
            return;
        };
        let current_secs = state.cfg().overlay.duration_ms / 1000;
        let dur_2s_flags = if current_secs == 2 {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let dur_3s_flags = if current_secs == 3 {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let dur_5s_flags = if current_secs == 5 {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let dur_10s_flags = if current_secs == 10 {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let _ = AppendMenuW(
            duration_menu,
            dur_2s_flags,
            MENU_DURATION_2S,
            PCWSTR(wide("2 seconds").as_ptr()),
        );
        let _ = AppendMenuW(
            duration_menu,
            dur_3s_flags,
            MENU_DURATION_3S,
            PCWSTR(wide("3 seconds").as_ptr()),
        );
        let _ = AppendMenuW(
            duration_menu,
            dur_5s_flags,
            MENU_DURATION_5S,
            PCWSTR(wide("5 seconds").as_ptr()),
        );
        let _ = AppendMenuW(
            duration_menu,
            dur_10s_flags,
            MENU_DURATION_10S,
            PCWSTR(wide("10 seconds").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            duration_menu.0 as usize,
            PCWSTR(wide("Duration").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT_ID, PCWSTR(wide("Quit").as_ptr()));

        let mut point = POINT::default();
        if GetCursorPos(&mut point).is_ok() {
            let command = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                point.x,
                point.y,
                0,
                state.hwnd,
                None,
            )
            .0 as usize;
            match command {
                MENU_OPEN_ID => state.show_window(),
                MENU_NOTIFY_ID => {
                    state.notifications_enabled = !state.notifications_enabled;
                    let _ = PostMessageW(state.overlay_hwnd, TOGGLE_MSG, WPARAM(0), LPARAM(0));
                }
                MENU_AUTOSTART_ID => {
                    let new_value = !state.cfg().behavior.start_on_login;
                    // Write the registry entry before committing the config
                    // value: a failed write must not persist a state the
                    // registry does not reflect.
                    if let Err(error) = autostart::apply(new_value) {
                        error!("start-on-login update failed: {error:#}");
                    } else {
                        state.mutate_config(|cfg| cfg.behavior.start_on_login = new_value);
                    }
                }
                MENU_CLOSE_TRAY_ID => {
                    let new_value = !state.cfg().behavior.close_to_tray;
                    state.mutate_config(|cfg| cfg.behavior.close_to_tray = new_value);
                }
                MENU_QUIT_ID => {
                    let _ = DestroyWindow(state.hwnd);
                }
                MENU_POSITION_TOP_LEFT => state.apply_anchor(VerticalPosition::Top, HorizontalPosition::Left),
                MENU_POSITION_TOP_CENTER => state.apply_anchor(VerticalPosition::Top, HorizontalPosition::Center),
                MENU_POSITION_TOP_RIGHT => state.apply_anchor(VerticalPosition::Top, HorizontalPosition::Right),
                MENU_POSITION_BOTTOM_LEFT => state.apply_anchor(VerticalPosition::Bottom, HorizontalPosition::Left),
                MENU_POSITION_BOTTOM_CENTER => state.apply_anchor(VerticalPosition::Bottom, HorizontalPosition::Center),
                MENU_POSITION_BOTTOM_RIGHT => state.apply_anchor(VerticalPosition::Bottom, HorizontalPosition::Right),
                MENU_POSITION_CUSTOM => {
                    let _ = crate::positioner::open(state.hwnd, state.overlay_hwnd);
                }
                MENU_POSITION_SAMPLE => {
                    show_sample(state.overlay_hwnd);
                }
                MENU_POSITION_RESET => {
                    state.reset_position();
                }
                MENU_DURATION_2S => {
                    state.mutate_config(|cfg| cfg.overlay.duration_ms = 2000);
                    set_duration(state.overlay_hwnd, 2000);
                }
                MENU_DURATION_3S => {
                    state.mutate_config(|cfg| cfg.overlay.duration_ms = 3000);
                    set_duration(state.overlay_hwnd, 3000);
                }
                MENU_DURATION_5S => {
                    state.mutate_config(|cfg| cfg.overlay.duration_ms = 5000);
                    set_duration(state.overlay_hwnd, 5000);
                }
                MENU_DURATION_10S => {
                    state.mutate_config(|cfg| cfg.overlay.duration_ms = 10000);
                    set_duration(state.overlay_hwnd, 10000);
                }
                _ => {}
            }
        }
        let _ = DestroyMenu(menu);
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            let state = (*create).lpCreateParams as *mut MainWindowState;
            if !state.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                (*state).hwnd = hwnd;
            }
        }
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;
    match message {
        WM_CREATE => {
            if !state_ptr.is_null() {
                (*state_ptr).create_children();
            }
            // Color the window title bar with the pill's pink accent so the
            // app reads as one theme. Applied here, after the frame is
            // realized, rather than right after CreateWindowExW. COLORREF is
            // 0x00BBGGRR, hence the swapped red/blue channels.
            let color = COLORREF(0x009F_6CE0);
            let result = unsafe {
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_CAPTION_COLOR,
                    &color as *const COLORREF as *const c_void,
                    std::mem::size_of::<COLORREF>() as u32,
                )
            };
            if let Err(error) = result {
                debug!("DwmSetWindowAttribute(CAPTION_COLOR) failed: {error}");
            }
            LRESULT(0)
        }
        WM_PAINT => {
            if !state_ptr.is_null() {
                (*state_ptr).paint();
            }
            LRESULT(0)
        }
        WM_SIZE => {
            if !state_ptr.is_null() {
                (*state_ptr).layout();
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_LOGS_ID => {
            unsafe {
                let _ = KillTimer(hwnd, TIMER_LOGS_ID);
            }
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                state.logs_copied_at = None;
                state.invalidate();
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_TOOLTIPS_ID => {
            if !state_ptr.is_null() {
                unsafe {
                    (*state_ptr).sync_tooltips();
                }
            }
            LRESULT(0)
        }
        WM_NOTIFY => {
            // The native history tooltip requests the per-item text on demand.
            if !state_ptr.is_null() && lparam.0 != 0 {
                let header = unsafe { &*(lparam.0 as *const NMHDR) };
                if header.code == TTN_GETDISPINFOW {
                    let state = &mut *state_ptr;
                    if let Some(text) = state.tooltip_text_for(header.idFrom) {
                        let wide = wide(&text);
                        let info = unsafe { &mut *(lparam.0 as *mut NMTTDISPINFOW) };
                        let copy = wide.len().min(info.szText.len() - 1);
                        info.szText[..copy].copy_from_slice(&wide[..copy]);
                        info.szText[copy] = 0;
                        info.lpszText = PWSTR::null();
                        info.hinst = HINSTANCE::default();
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
                let x = (lparam.0 & 0xFFFF) as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
                let sidebar_w = (SIDEBAR_W * scale).round() as i32;
                let pad = (PAD * scale) as i32;
                let (client_w, _client_h) = client_size(hwnd);

                // Check sidebar clicks
                if x < sidebar_w {
                    let item_h = (32.0 * scale) as i32;
                    let item0_y = (40.0 * scale) as i32;
                    let item1_y = item0_y + item_h + (4.0 * scale) as i32;
                    if y >= item0_y && y < item0_y + item_h {
                        state.active_pane = Pane::Activity;
                    } else if y >= item1_y && y < item1_y + item_h {
                        state.active_pane = Pane::Settings;
                    }
                    state.invalidate();
                } else if state.active_pane == Pane::Activity {
                    // Check position area click in Activity pane
                    let pos_y = ((ART_Y + ART_SIZE + SEP_GAP + HIST_GAP + HIST_H) * scale).round() as i32
                        + (4.0 * scale) as i32;
                    let pos_bottom = pos_y + (16.0 * scale) as i32;
                    if y >= pos_y && y <= pos_bottom {
                        let _ = crate::positioner::open(hwnd, state.overlay_hwnd);
                    }
                } else if state.active_pane == Pane::Settings {
                    // Hit-test against the same layout used by paint_settings.
                    let items = state.settings_items(sidebar_w, client_w, pad, scale);
                    for item in &items {
                        if let SettingsItem::Row { id, rect } = item
                            && y >= rect.top
                            && y < rect.bottom
                        {
                            let label_w = (((rect.right - rect.left) as f32) * 0.42) as i32;
                            let control_left = rect.left + label_w + (10.0 * scale) as i32;
                            let control_rect = RECT {
                                left: control_left,
                                top: rect.top,
                                right: rect.right - (10.0 * scale) as i32,
                                bottom: rect.bottom,
                            };
                            match id {
                                SettingId::Notifications => {
                                    state.notifications_enabled = !state.notifications_enabled;
                                    let _ = PostMessageW(state.overlay_hwnd, TOGGLE_MSG, WPARAM(0), LPARAM(0));
                                    state.invalidate();
                                }
                                SettingId::StartOnLogin => {
                                    let new_value = !state.cfg().behavior.start_on_login;
                                    // Write the registry entry before committing
                                    // the config value: a failed write must not
                                    // persist a state the registry does not
                                    // reflect.
                                    if let Err(error) = autostart::apply(new_value) {
                                        error!("start-on-login update failed: {error:#}");
                                    } else {
                                        state.mutate_config(|cfg| cfg.behavior.start_on_login = new_value);
                                    }
                                    state.invalidate();
                                }
                                SettingId::CloseToTray => {
                                    let new_value = !state.cfg().behavior.close_to_tray;
                                    state.mutate_config(|cfg| cfg.behavior.close_to_tray = new_value);
                                    state.invalidate();
                                }
                                SettingId::Duration => {
                                    let segments = segment_rects(&control_rect, 4, (4.0 * scale) as i32);
                                    let values = [2000u64, 3000, 5000, 10000];
                                    if let Some((i, _)) =
                                        segments.iter().enumerate().find(|(_, s)| x >= s.left && x < s.right)
                                    {
                                        let duration = values[i];
                                        state.mutate_config(|cfg| cfg.overlay.duration_ms = duration);
                                        set_duration(state.overlay_hwnd, duration);
                                        state.invalidate();
                                    }
                                }
                                SettingId::Position => {
                                    let parts = position_parts(rect, scale);
                                    if let Some((i, _)) = parts
                                        .anchors
                                        .iter()
                                        .enumerate()
                                        .find(|(_, a)| x >= a.left && x < a.right && y >= a.top && y < a.bottom)
                                    {
                                        let (v, h) = match i {
                                            0 => (VerticalPosition::Top, HorizontalPosition::Left),
                                            1 => (VerticalPosition::Top, HorizontalPosition::Center),
                                            2 => (VerticalPosition::Top, HorizontalPosition::Right),
                                            3 => (VerticalPosition::Bottom, HorizontalPosition::Left),
                                            4 => (VerticalPosition::Bottom, HorizontalPosition::Center),
                                            _ => (VerticalPosition::Bottom, HorizontalPosition::Right),
                                        };
                                        state.apply_anchor(v, h);
                                    } else if x >= parts.reset.left
                                        && x < parts.reset.right
                                        && y >= parts.reset.top
                                        && y < parts.reset.bottom
                                    {
                                        state.reset_position();
                                    } else if x >= parts.adjust.left
                                        && x < parts.adjust.right
                                        && y >= parts.adjust.top
                                        && y < parts.adjust.bottom
                                    {
                                        let _ = crate::positioner::open(hwnd, state.overlay_hwnd);
                                    }
                                }
                                SettingId::ShowSample => {
                                    show_sample(state.overlay_hwnd);
                                }
                                SettingId::CopyLogs => {
                                    state.copy_logs();
                                }
                                SettingId::AllowedApps => {
                                    if !process_picker::open(hwnd, &control_rect, &state.cfg().behavior.allowed_sources)
                                    {
                                        debug!("process picker failed to open");
                                    }
                                }
                            }
                            return LRESULT(0);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if state.active_pane == Pane::Settings {
                    let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
                    let x = (lparam.0 & 0xFFFF) as i32;
                    let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
                    let sidebar_w = (SIDEBAR_W * scale).round() as i32;
                    let pad = (PAD * scale) as i32;
                    let (client_w, client_h) = client_size(hwnd);
                    let hover = if x < sidebar_w {
                        None
                    } else {
                        state.settings_hover_at(x, y, sidebar_w, client_w, pad, scale)
                    };
                    if hover != state.settings_hover {
                        state.settings_hover = hover;
                        let region = state.settings_region(client_w, client_h);
                        state.invalidate_rect(&region);
                    }
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                } else if state.settings_hover.is_some() {
                    state.settings_hover = None;
                    let (client_w, client_h) = client_size(hwnd);
                    let region = state.settings_region(client_w, client_h);
                    state.invalidate_rect(&region);
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if state.settings_hover.is_some() {
                    state.settings_hover = None;
                    let (client_w, client_h) = client_size(hwnd);
                    let region = state.settings_region(client_w, client_h);
                    state.invalidate_rect(&region);
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLORLISTBOX => {
            let hdc = HDC(wparam.0 as *mut c_void);
            SetTextColor(hdc, colorref(0xE6, 0xE6, 0xE6));
            SetBkColor(hdc, colorref(0, 0, 0));
            LRESULT(GetStockObject(windows::Win32::Graphics::Gdi::BLACK_BRUSH).0 as isize)
        }
        WM_DRAWITEM => {
            if !state_ptr.is_null() && lparam.0 != 0 {
                let item = unsafe { &*(lparam.0 as *const DRAWITEMSTRUCT) };
                if item.CtlID as usize == LISTBOX_ID && item.hwndItem == (*state_ptr).listbox {
                    (*state_ptr).draw_history_item(item);
                }
            }
            LRESULT(1)
        }
        MEDIA_EVENT_MSG => {
            if !state_ptr.is_null() {
                (*state_ptr).receive_events();
            }
            LRESULT(0)
        }
        POSITION_MSG => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                // Custom position posted by the positioner (logical pixels).
                let x = wparam.0 as i32;
                let y = lparam.0 as i32;
                state.mutate_config(|cfg| {
                    cfg.overlay.position_x = Some(x);
                    cfg.overlay.position_y = Some(y);
                });
                set_position(state.overlay_hwnd, OverlayPos::from_config(&state.cfg()));
            }
            LRESULT(0)
        }
        PICKER_RESULT_MSG => {
            // Confirmed results carry a heap-allocated Vec<String> in lparam
            // that must be reclaimed even when the window is being destroyed
            // (state_ptr null) — the Box::from_raw must live outside the
            // state_ptr guard or the allocation leaks on teardown. When the
            // window is still alive, the patterns are applied to config.
            if lparam.0 != 0 {
                let patterns = unsafe { Box::from_raw(lparam.0 as *mut Vec<String>) };
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    let patterns = *patterns;
                    state.mutate_config(|cfg| cfg.behavior.allowed_sources = patterns);
                    state.invalidate();
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if !state_ptr.is_null() {
                (*state_ptr).on_close();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if !state_ptr.is_null() {
                (*state_ptr).on_destroy();
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_TRAY => {
            let event = lparam.0 as u32;
            if event == WM_RBUTTONUP && !state_ptr.is_null() {
                show_tray_menu(&mut *state_ptr);
            } else if event == WM_LBUTTONDBLCLK && !state_ptr.is_null() {
                (*state_ptr).show_window();
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !state_ptr.is_null() {
                drop(Box::from_raw(state_ptr));
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str) -> TrackInfo {
        TrackInfo {
            title: title.into(),
            artist: "The Artist".into(),
            album: "The Album".into(),
            album_artist: String::new(),
            subtitle: String::new(),
            artwork: None,
            app_icon: None,
            source_app: "Spotify".into(),
            duration_secs: None,
            track_number: None,
            track_count: None,
            genre: None,
        }
    }

    #[test]
    fn history_keeps_cap_and_order() {
        let mut history = History::new(3);
        for index in 0..5 {
            history.push(HistoryEntry {
                at: Local::now(),
                at_label: String::new(),
                track: track(&format!("Track {index}")),
                state: PlaybackState::Playing,
                accepted: true,
            });
        }
        assert_eq!(history.len(), 3);
        // Newest first: the last pushed entry is at the front.
        let titles: Vec<_> = history.iter().map(|entry| entry.track.title.as_str()).collect();
        assert_eq!(titles, ["Track 4", "Track 3", "Track 2"]);
    }

    #[test]
    fn history_keeps_accepted_flag_with_newest_first() {
        let mut history = History::new(3);
        history.push(HistoryEntry {
            at: Local::now(),
            at_label: String::new(),
            track: track("Track A"),
            state: PlaybackState::Playing,
            accepted: true,
        });
        history.push(HistoryEntry {
            at: Local::now(),
            at_label: String::new(),
            track: track("Track B"),
            state: PlaybackState::Paused,
            accepted: false,
        });
        let entries: Vec<_> = history.iter().collect();
        // Newest first, and the accepted flag travels with its entry.
        assert_eq!(entries[0].track.title, "Track B");
        assert!(!entries[0].accepted);
        assert_eq!(entries[1].track.title, "Track A");
        assert!(entries[1].accepted);
    }

    fn current_activity(track: TrackInfo, state: PlaybackState) -> CurrentActivity {
        CurrentActivity {
            track,
            state,
            art: None,
            art_fingerprint: None,
            art_decode_failed: false,
        }
    }

    fn history_entry(track: TrackInfo, state: PlaybackState) -> HistoryEntry {
        HistoryEntry {
            at: Local::now(),
            at_label: String::new(),
            track,
            state,
            accepted: true,
        }
    }

    #[test]
    fn state_row_dedup_skips_interleaved_foreign_rows() {
        let current = current_activity(track("Song"), PlaybackState::Playing);

        // Empty history: not a duplicate.
        let history = History::new(10);
        assert!(!duplicate_state_row(&history.entries, &current, PlaybackState::Playing));

        // Same source, same track, same state on top: duplicate.
        let mut history = History::new(10);
        history.push(history_entry(track("Song"), PlaybackState::Playing));
        assert!(duplicate_state_row(&history.entries, &current, PlaybackState::Playing));
        // Same source, same track, different state: a real change, new row.
        assert!(!duplicate_state_row(&history.entries, &current, PlaybackState::Paused));

        // A rejected row from another source interleaves on top: the newest
        // same-source row below it still dedups.
        let mut other = track("Song");
        other.source_app = "other-app".into();
        history.push(history_entry(other.clone(), PlaybackState::Playing));
        assert!(
            duplicate_state_row(&history.entries, &current, PlaybackState::Playing),
            "interleaved foreign rows must not defeat the dedup"
        );

        // Only a foreign row exists: nothing to dedup against.
        let mut foreign_only = History::new(10);
        foreign_only.push(history_entry(other, PlaybackState::Playing));
        assert!(!duplicate_state_row(
            &foreign_only.entries,
            &current,
            PlaybackState::Playing
        ));
    }

    #[test]
    fn row_omits_artist_when_blank() {
        let mut blank = track("Song");
        blank.artist = "   ".into();
        let row = history_row(&blank, Local::now(), PlaybackState::Playing);
        assert!(row.contains("Song"));
        assert!(!row.contains("Unknown"));

        let titled = track("Song");
        let row = history_row(&titled, Local::now(), PlaybackState::Paused);
        assert!(row.contains("The Artist"));
    }
}
