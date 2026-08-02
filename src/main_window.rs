use crate::autostart;
use crate::config::{Config, HorizontalPosition, VerticalPosition};
use crate::events::{MEDIA_EVENT_MSG, MediaEvent, POSITION_MSG, PlaybackState, TOGGLE_MSG, TrackInfo};
use crate::overlay::{
    EventQueue, OverlayPos, decode_artwork, draw_string, set_duration, set_position, show_sample, wide,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use log::{debug, error};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, GlobalFree, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, ClientToScreen, CreateFontW,
    CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CALCRECT, DT_LEFT, DT_NOPREFIX, DT_TOP,
    DeleteObject, DrawTextW, EndPaint, FF_DONTCARE, FillRect, GetDC, GetMonitorInfoW, GetStockObject, HBRUSH, HDC,
    HFONT, HGDIOBJ, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, ReleaseDC, SRCCOPY, SelectObject, SetBkColor, SetBkMode, SetTextColor, StretchDIBits, TRANSPARENT,
};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CallWindowProcW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, GWLP_USERDATA, GWLP_WNDPROC, GetClientRect, GetCursorPos, GetWindowLongPtrW, HMENU, HWND_TOPMOST,
    IDC_ARROW, IDI_APPLICATION, KillTimer, LB_ADDSTRING, LB_DELETESTRING, LB_GETCOUNT, LB_GETTEXT, LB_INSERTSTRING,
    LB_ITEMFROMPOINT, LB_SETITEMHEIGHT, LB_SETTOPINDEX, LBS_HASSTRINGS, LBS_NOINTEGRALHEIGHT, LBS_OWNERDRAWFIXED,
    LoadCursorW, LoadIconW, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, PostMessageW, PostQuitMessage,
    RegisterClassExW, SW_HIDE, SW_SHOW, SW_SHOWMAXIMIZED, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SendMessageW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_CREATE, WM_CTLCOLORLISTBOX, WM_DESTROY,
    WM_DRAWITEM, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY,
    WM_PAINT, WM_RBUTTONUP, WM_SETFONT, WM_SIZE, WM_TIMER, WNDCLASS_STYLES, WNDCLASSEXW, WNDPROC, WS_CHILD,
    WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::PCWSTR;

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
const HISTORY_CAP: usize = 500;
const TOOLTIP_CLASS: &str = "NotchTooltip";
/// Timer used to clear the "Copied" feedback on the Copy logs button.
const TIMER_LOGS_ID: usize = 101;

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

/// Settings rows, mirroring NewsAggregator's settings layout: section headers
/// with label-left / control-right card rows.
#[derive(Clone, Copy, PartialEq)]
enum SettingId {
    Notifications,
    Duration,
    StartOnLogin,
    CloseToTray,
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
fn draw_segment_button(
    hdc: HDC,
    rect: &RECT,
    label: &str,
    active: bool,
    hovered: bool,
    accent: [u8; 4],
    accent_soft: [u8; 4],
    scale: f32,
) {
    let border = if active {
        colorref(accent[0], accent[1], accent[2])
    } else {
        colorref(SETTINGS_BORDER[0], SETTINGS_BORDER[1], SETTINGS_BORDER[2])
    };
    let b = unsafe { CreateSolidBrush(border) };
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
    let fill = if active {
        accent_soft
    } else if hovered {
        SETTINGS_HOVER
    } else {
        SETTINGS_SURFACE
    };
    let f = unsafe { CreateSolidBrush(colorref(fill[0], fill[1], fill[2])) };
    unsafe {
        let _ = FillRect(hdc, &inner, f);
    }
    unsafe {
        let _ = DeleteObject(HGDIOBJ(f.0));
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
    track: TrackInfo,
    state: PlaybackState,
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

    fn push(&mut self, entry: HistoryEntry) {
        self.entries.push_back(entry);
        while self.entries.len() > self.cap {
            self.entries.pop_front();
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
    art: Option<Vec<u8>>,
}

struct MainWindowState {
    hwnd: HWND,
    instance: HINSTANCE,
    config: Config,
    queue: EventQueue,
    overlay_hwnd: HWND,
    listbox: HWND,
    current: Option<CurrentActivity>,
    history: History,
    listbox_font: HFONT,
    gray_brush: HBRUSH,
    accent_brush: HBRUSH,
    notifications_enabled: bool,
    active_pane: Pane,
    /// Hovered settings row (row index, sub-control) for highlight.
    settings_hover: Option<(usize, SettingSub)>,
    /// Previous listbox window proc (subclassing for hover tooltips).
    listbox_prev_proc: isize,
    /// Lazy-created hover tooltip window; null when never shown.
    tooltip_hwnd: HWND,
    /// Full details text of the currently shown tooltip.
    tooltip_text: String,
    /// When the last "Copy logs" click happened, for the transient "Copied" label.
    logs_copied_at: Option<Instant>,
}

/// Creates the main window: a maximized tracker with current activity,
/// per-session history, and a tray icon. The caller runs the message loop.
pub fn create_window(config: Config, queue: EventQueue, overlay_hwnd: HWND) -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.context("getting the process module")?;
    let instance: HINSTANCE = module.into();
    let class_name = wide("NotchMainWindow");
    register_main_class(instance, &class_name)?;
    register_tooltip_class(instance);

    let state = Box::new(MainWindowState::new(config.clone(), queue, overlay_hwnd, instance));
    let state_ptr = Box::into_raw(state);
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("Notch").as_ptr()),
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
            unsafe { drop(Box::from_raw(state_ptr)) };
            return Err(error.into());
        }
    };

    unsafe {
        if config.behavior.start_in_tray {
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
    fn new(config: Config, queue: EventQueue, overlay_hwnd: HWND, instance: HINSTANCE) -> Self {
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
            notifications_enabled: true,
            active_pane: Pane::Activity,
            settings_hover: None,
            listbox_prev_proc: 0,
            tooltip_hwnd: HWND::default(),
            tooltip_text: String::new(),
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
        let accent = self.config.appearance.accent_color;
        self.accent_brush = unsafe { CreateSolidBrush(colorref(accent[0], accent[1], accent[2])) };

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
            // Subclass the listbox to show a hover tooltip with the full details
            // of each history entry.
            unsafe {
                let state_ptr = self as *const MainWindowState as *mut MainWindowState as isize;
                SetWindowLongPtrW(self.listbox, GWLP_USERDATA, state_ptr);
                self.listbox_prev_proc =
                    SetWindowLongPtrW(self.listbox, GWLP_WNDPROC, listbox_subclass_proc as *const () as isize);
            }
        }
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
                MediaEvent::PlaybackStateChanged(state) => {
                    if let Some(current) = &mut self.current {
                        current.state = state;
                        self.add_state_change(state);
                        self.invalidate();
                    }
                }
            }
        }
    }

    fn add_state_change(&mut self, state: PlaybackState) {
        let track = self.current.as_ref().map(|c| c.track.clone()).unwrap_or_default();
        let before = self.history.len();
        self.history.push(HistoryEntry {
            at: Local::now(),
            track: track.clone(),
            state,
        });
        if self.history.len() <= before && before > 0 {
            let _ = unsafe { SendMessageW(self.listbox, LB_DELETESTRING, WPARAM(0), LPARAM(0)) };
        }
        let row = history_row(&track, Local::now(), state);
        let row = wide(&row);
        if !self.listbox.0.is_null() {
            unsafe {
                let _ = SendMessageW(self.listbox, LB_ADDSTRING, WPARAM(0), LPARAM(row.as_ptr() as isize));
                let count = SendMessageW(self.listbox, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0 as usize;
                if count > 0 {
                    let _ = SendMessageW(self.listbox, LB_SETTOPINDEX, WPARAM(count - 1), LPARAM(0));
                }
            }
        }
    }

    fn add_track(&mut self, track: TrackInfo) {
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
                current.art = None;
            }
            if let Some(last) = self.history.entries.back_mut() {
                last.track = track.clone();
            }
            let row = history_row(
                &track,
                Local::now(),
                self.current.as_ref().map(|c| c.state).unwrap_or(PlaybackState::Playing),
            );
            let row = wide(&row);
            if !self.listbox.0.is_null() {
                unsafe {
                    let count = SendMessageW(self.listbox, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0 as usize;
                    if count > 0 {
                        let _ = SendMessageW(self.listbox, LB_DELETESTRING, WPARAM(count - 1), LPARAM(0));
                        let _ = SendMessageW(
                            self.listbox,
                            LB_INSERTSTRING,
                            WPARAM(count - 1),
                            LPARAM(row.as_ptr() as isize),
                        );
                    }
                }
            }
            self.invalidate();
            return;
        }

        let state = self.current.as_ref().map(|c| c.state).unwrap_or(PlaybackState::Playing);
        let before = self.history.len();
        self.history.push(HistoryEntry {
            at: Local::now(),
            track: track.clone(),
            state,
        });
        if self.history.len() <= before && before > 0 {
            let _ = unsafe { SendMessageW(self.listbox, LB_DELETESTRING, WPARAM(0), LPARAM(0)) };
        }
        let row = history_row(&track, Local::now(), state);
        let row = wide(&row);
        if !self.listbox.0.is_null() {
            unsafe {
                let _ = SendMessageW(self.listbox, LB_ADDSTRING, WPARAM(0), LPARAM(row.as_ptr() as isize));
                let count = SendMessageW(self.listbox, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0 as usize;
                if count > 0 {
                    let _ = SendMessageW(self.listbox, LB_SETTOPINDEX, WPARAM(count - 1), LPARAM(0));
                }
            }
        }
        self.current = Some(CurrentActivity {
            track,
            state: self
                .current
                .as_ref()
                .map(|current| current.state)
                .unwrap_or(PlaybackState::Playing),
            art: None,
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
            self.hide_tooltip();
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
            let black = CreateSolidBrush(COLORREF(0));
            let _ = FillRect(hdc, &whole, black);
            let _ = DeleteObject(black);
        }

        // Draw sidebar
        let sidebar_rect = RECT {
            left: 0,
            top: 0,
            right: sidebar_w,
            bottom: client_h,
        };
        unsafe {
            let sidebar_bg = CreateSolidBrush(COLORREF(0x0A0A0A));
            let _ = FillRect(hdc, &sidebar_rect, sidebar_bg);
            let _ = DeleteObject(sidebar_bg);
        }

        // Sidebar items
        let item_h = (32.0 * scale) as i32;
        let items = [("Now Playing", Pane::Activity), ("Settings", Pane::Settings)];
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
                    let highlight = CreateSolidBrush(COLORREF(0x1A1A2E));
                    let _ = FillRect(hdc, &item_rect, highlight);
                    let _ = DeleteObject(highlight);
                }
            }
            let mut text_rect = item_rect;
            draw_string(
                hdc,
                label,
                &mut text_rect,
                (10.0 * scale) as i32,
                if *pane == self.active_pane {
                    self.config.appearance.accent_color
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
            self.config.appearance.accent_color,
            true,
            false,
        );

        let art = (ART_SIZE * scale).round() as i32;
        let art_x = content_left + pad;
        let art_y = (ART_Y * scale) as i32;
        let text_left = art_x + art + (12.0 * scale) as i32;
        let text_right = client_w - pad;

        if let Some(current) = &mut self.current {
            if current.art.is_none() {
                current.art = current
                    .track
                    .artwork
                    .as_deref()
                    .and_then(|data| decode_artwork(data, (ART_SIZE * scale).round() as usize));
            }
            if let Some(art_pixels) = current.art.as_deref() {
                draw_art(hdc, art_pixels, art, art_x, art_y);
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
            };
            let state_color = if current.state == PlaybackState::Playing {
                self.config.appearance.accent_color
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
                self.config.appearance.text_color,
                true,
                false,
            );

            let subtitle = if current.track.artist.trim().is_empty() {
                &current.track.source_app
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
        let pos_label = if self.config.overlay.position_x.is_some() {
            format!(
                "Position: custom ({}, {})",
                self.config.overlay.position_x.unwrap_or(0),
                self.config.overlay.position_y.unwrap_or(0)
            )
        } else {
            format!(
                "Position: {}-{}",
                match self.config.overlay.vertical {
                    VerticalPosition::Top => "top",
                    VerticalPosition::Bottom => "bottom",
                },
                match self.config.overlay.horizontal {
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
        let accent = self.config.appearance.accent_color;
        let accent_soft = mix(accent, [0x1B, 0x1B, 0x1B, 0xFF], 0.28);

        let mut hdr = RECT {
            left: content_left + pad,
            top: pad,
            right: client_w - pad,
            bottom: pad + (24.0 * scale) as i32,
        };
        draw_string(hdc, "SETTINGS", &mut hdr, (13.0 * scale) as i32, accent, true, false);

        let items = self.settings_items(content_left, client_w, pad, scale);
        let mut row_index = 0usize;
        for item in &items {
            match item {
                SettingsItem::Header { text, rect } => {
                    let mut hr = *rect;
                    draw_string(hdc, text, &mut hr, (9.0 * scale) as i32, SETTINGS_FAINT, true, false);
                }
                SettingsItem::Row { id, rect } => {
                    let hovered_row = self.settings_hover.is_some_and(|(r, _)| r == row_index);
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
                    let border_brush = unsafe {
                        CreateSolidBrush(colorref(SETTINGS_BORDER[0], SETTINGS_BORDER[1], SETTINGS_BORDER[2]))
                    };
                    unsafe {
                        let _ = FillRect(hdc, rect, border_brush);
                    }
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(border_brush.0));
                    }
                    let inner = RECT {
                        left: rect.left + 1,
                        top: rect.top + 1,
                        right: rect.right - 1,
                        bottom: rect.bottom - 1,
                    };
                    let bg = if hovered_row { SETTINGS_HOVER } else { SETTINGS_SURFACE };
                    let bg_brush = unsafe { CreateSolidBrush(colorref(bg[0], bg[1], bg[2])) };
                    unsafe {
                        let _ = FillRect(hdc, &inner, bg_brush);
                    }
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(bg_brush.0));
                    }

                    let (label, value_text, value_color) = match id {
                        SettingId::Notifications => (
                            "Notifications",
                            if self.notifications_enabled {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if self.notifications_enabled {
                                accent
                            } else {
                                SETTINGS_FAINT
                            },
                        ),
                        SettingId::StartOnLogin => (
                            "Start on login",
                            if self.config.behavior.start_on_login {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if self.config.behavior.start_on_login {
                                accent
                            } else {
                                SETTINGS_FAINT
                            },
                        ),
                        SettingId::CloseToTray => (
                            "Close to tray",
                            if self.config.behavior.close_to_tray {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if self.config.behavior.close_to_tray {
                                accent
                            } else {
                                SETTINGS_FAINT
                            },
                        ),
                        SettingId::Duration => (
                            "Duration",
                            format!("{}s", self.config.overlay.duration_ms / 1000),
                            SETTINGS_MUTED,
                        ),
                        SettingId::Position => ("Position", self.position_label(), SETTINGS_MUTED),
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
                        SettingId::Notifications | SettingId::StartOnLogin | SettingId::CloseToTray => {
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
                            let duration_ms = self.config.overlay.duration_ms;
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
                                let seg_hovered = self.settings_hover == Some((row_index, SettingSub::Seg(i)));
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
                            let custom = self.config.overlay.position_x.is_some();
                            let active_anchor = if custom {
                                None
                            } else {
                                Some(match (self.config.overlay.vertical, self.config.overlay.horizontal) {
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
                            let reset_hovered = self.settings_hover == Some((row_index, SettingSub::Reset));
                            draw_small_button(hdc, &parts.reset, "Reset", accent, reset_hovered, scale);

                            // Anchor segments + Adjust button row
                            for (i, seg) in parts.anchors.iter().enumerate() {
                                let active = active_anchor == Some(i);
                                let seg_hovered = self.settings_hover == Some((row_index, SettingSub::Anchor(i)));
                                draw_segment_button(
                                    hdc,
                                    seg,
                                    ANCHOR_LABELS[i],
                                    active,
                                    seg_hovered,
                                    accent,
                                    accent_soft,
                                    scale,
                                );
                            }
                            let adjust_hovered = self.settings_hover == Some((row_index, SettingSub::Adjust));
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
        if self.config.overlay.position_x.is_some() {
            format!(
                "Custom ({}, {})",
                self.config.overlay.position_x.unwrap_or(0),
                self.config.overlay.position_y.unwrap_or(0)
            )
        } else {
            format!(
                "{}-{}",
                match self.config.overlay.vertical {
                    VerticalPosition::Top => "top",
                    VerticalPosition::Bottom => "bottom",
                },
                match self.config.overlay.horizontal {
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
                    return Some((row_index, SettingSub::Seg(seg.unwrap_or(0))));
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
            if self.config.behavior.close_to_tray {
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

        let bg = if index == 0 {
            COLORREF(0x00141414)
        } else if selected {
            COLORREF(0x001D2B26)
        } else if index.is_multiple_of(2) {
            COLORREF(0x00000000)
        } else {
            COLORREF(0x000E0E0E)
        };
        unsafe {
            let brush = CreateSolidBrush(bg);
            let _ = FillRect(hdc, &item.rcItem, brush);
            let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(brush.0));
        }

        let mut buf = vec![0u16; 512];
        let len = unsafe {
            SendMessageW(
                item.hwndItem,
                LB_GETTEXT,
                WPARAM(index),
                LPARAM(buf.as_mut_ptr() as isize),
            )
            .0 as usize
        };
        buf.truncate(len.min(buf.len()));
        let text = String::from_utf16_lossy(&buf);

        let mut text_rect = item.rcItem;
        text_rect.left += (8.0 * scale) as i32;
        text_rect.right -= (8.0 * scale) as i32;
        let color = if index == 0 {
            [0x9A, 0x9A, 0x9A, 0xFF]
        } else {
            [0xE6, 0xE6, 0xE6, 0xFF]
        };
        draw_string(
            hdc,
            &text,
            &mut text_rect,
            ((if index == 0 { 11.0 } else { 13.0 }) * scale) as i32,
            color,
            index == 0,
            false,
        );
    }

    fn on_destroy(&mut self) {
        remove_tray_icon(self.hwnd);
        unsafe {
            if !self.tooltip_hwnd.0.is_null() {
                let _ = DestroyWindow(self.tooltip_hwnd);
                self.tooltip_hwnd = HWND::default();
            }
            if !self.listbox.0.is_null() && self.listbox_prev_proc != 0 {
                SetWindowLongPtrW(self.listbox, GWLP_WNDPROC, self.listbox_prev_proc);
                SetWindowLongPtrW(self.listbox, GWLP_USERDATA, 0);
                self.listbox_prev_proc = 0;
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
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    fn show_window(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWMAXIMIZED);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    /// Creates the hover tooltip window on first use.
    fn ensure_tooltip(&mut self) -> HWND {
        if !self.tooltip_hwnd.0.is_null() {
            return self.tooltip_hwnd;
        }
        let state_ptr = self as *const MainWindowState as *mut MainWindowState;
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                PCWSTR(wide(TOOLTIP_CLASS).as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                self.instance,
                Some(state_ptr.cast()),
            )
        };
        if let Ok(hwnd) = hwnd {
            self.tooltip_hwnd = hwnd;
        }
        self.tooltip_hwnd
    }

    /// Shows the tooltip with `text` near the given screen point.
    fn show_tooltip(&mut self, text: &str, at: POINT) {
        let hwnd = self.ensure_tooltip();
        if hwnd.0.is_null() {
            return;
        }
        self.tooltip_text = text.to_string();
        let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
        unsafe {
            let hdc = GetDC(HWND::default());
            let font_name = wide("Segoe UI");
            let font = CreateFontW(
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
            );
            let old = SelectObject(hdc, font);
            let mut text_w = wide(text);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 480,
                bottom: 0,
            };
            let _ = DrawTextW(
                hdc,
                &mut text_w,
                &mut rect,
                DT_CALCRECT | DT_LEFT | DT_TOP | DT_NOPREFIX,
            );
            SelectObject(hdc, old);
            let _ = DeleteObject(font);
            let _ = ReleaseDC(HWND::default(), hdc);
            let w = rect.right + (16.0 * scale) as i32;
            let h = rect.bottom + (12.0 * scale) as i32;
            let work = tooltip_work_area(self.hwnd);
            let x = at.x.clamp(work.left, (work.right - w).max(work.left));
            let y = at.y.clamp(work.top, (work.bottom - h).max(work.top));
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(hwnd, None, false);
        }
    }

    fn hide_tooltip(&mut self) {
        if !self.tooltip_hwnd.0.is_null() {
            unsafe {
                let _ = ShowWindow(self.tooltip_hwnd, SW_HIDE);
            }
        }
    }

    /// Copies the current run's log file to the clipboard (UTF-16 with per-line
    /// newlines preserved) and shows a transient "Copied" state.
    fn copy_logs(&mut self) {
        let path = self.config.logs_dir().join("log-Live.log");
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
                if !ptr.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr.cast(), wide.len());
                    let _ = GlobalUnlock(hmem);
                }
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
        self.config.overlay.vertical = vertical;
        self.config.overlay.horizontal = horizontal;
        self.config.overlay.position_x = None;
        self.config.overlay.position_y = None;
        let _ = self.config.save();
        set_position(self.overlay_hwnd, OverlayPos::from_config(&self.config));
    }

    /// Clears any custom X/Y override and returns to the default top-center anchor.
    fn reset_position(&mut self) {
        self.apply_anchor(VerticalPosition::Top, HorizontalPosition::Center);
        // If the position adjustor is open, move it back to the default spot too.
        crate::positioner::reset_position();
    }
}

fn history_row(track: &TrackInfo, at: DateTime<Local>, state: PlaybackState) -> String {
    let artist = if track.artist.trim().is_empty() {
        &track.source_app
    } else {
        &track.artist
    };
    let status = match state {
        PlaybackState::Playing => "▶",
        PlaybackState::Paused => "‖",
        PlaybackState::Stopped => "■",
    };
    let mut row = format!("{}  {}  {} — {}", at.format("%H:%M:%S"), status, track.title, artist);
    if !track.album.trim().is_empty() {
        row.push_str(&format!(" — {}", track.album));
    }
    row
}

fn draw_art(hdc: HDC, rgba: &[u8], px: i32, x: i32, y: i32) {
    let mut bgra = Vec::with_capacity(rgba.len());
    for chunk in rgba.chunks_exact(4) {
        let [r, g, b, a] = [chunk[0], chunk[1], chunk[2], chunk[3]];
        bgra.extend([
            (b as u32 * a as u32 / 255) as u8,
            (g as u32 * a as u32 / 255) as u8,
            (r as u32 * a as u32 / 255) as u8,
            a,
        ]);
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: px,
            biHeight: -px,
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
            px,
            px,
            Some(bgra.as_ptr().cast()),
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
            }
        ),
        entry.track.title.clone(),
    ];
    if !entry.track.artist.trim().is_empty() {
        parts.push(entry.track.artist.clone());
    }
    if !entry.track.album.trim().is_empty() {
        parts.push(entry.track.album.clone());
    }
    let meta = entry.track.meta_line(false);
    if !meta.is_empty() {
        parts.push(meta);
    }
    if !entry.track.source_app.trim().is_empty() {
        parts.push(entry.track.source_app.clone());
    }
    parts.join("\n")
}

fn tooltip_work_area(hwnd: HWND) -> RECT {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let _ = GetMonitorInfoW(monitor, &mut info);
        info.rcWork
    }
}

fn register_tooltip_class(instance: HINSTANCE) {
    unsafe {
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(tooltip_proc),
            hInstance: instance,
            hCursor: cursor,
            lpszClassName: PCWSTR(wide(TOOLTIP_CLASS).as_ptr()),
            hbrBackground: CreateSolidBrush(colorref(0x1B, 0x1B, 0x1B)),
            ..Default::default()
        };
        let _ = RegisterClassExW(&class);
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn tooltip_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = lparam.0 as *const CREATESTRUCTW;
            if !create.is_null() {
                let state = (*create).lpCreateParams as *mut MainWindowState;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut paint);
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;
            if !hdc.0.is_null() && !state_ptr.is_null() {
                let state = &*state_ptr;
                let (cw, ch) = client_size(hwnd);
                let whole = RECT {
                    left: 0,
                    top: 0,
                    right: cw,
                    bottom: ch,
                };
                let border = CreateSolidBrush(colorref(0x2D, 0x2D, 0x2D));
                let _ = FillRect(hdc, &whole, border);
                let _ = DeleteObject(HGDIOBJ(border.0));
                let inner = RECT {
                    left: 1,
                    top: 1,
                    right: cw - 1,
                    bottom: ch - 1,
                };
                let bg = CreateSolidBrush(colorref(0x1B, 0x1B, 0x1B));
                let _ = FillRect(hdc, &inner, bg);
                let _ = DeleteObject(HGDIOBJ(bg.0));

                let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
                let font_name = wide("Segoe UI");
                let font = CreateFontW(
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
                );
                let old = SelectObject(hdc, font);
                let _ = SetBkMode(hdc, TRANSPARENT);
                let _ = SetTextColor(hdc, colorref(0xE6, 0xE6, 0xE6));
                let mut text_rect = RECT {
                    left: inner.left + (8 * scale as i32),
                    top: inner.top + (6 * scale as i32),
                    right: inner.right - (8 * scale as i32),
                    bottom: inner.bottom - (6 * scale as i32),
                };
                let mut text_w = wide(&state.tooltip_text);
                let _ = DrawTextW(hdc, &mut text_w, &mut text_rect, DT_LEFT | DT_TOP | DT_NOPREFIX);
                SelectObject(hdc, old);
                let _ = DeleteObject(font);
            }
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// Subclassed listbox window proc: shows the full-details tooltip while the
/// mouse hovers a history row, and hides it on leave/click/scroll.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn listbox_subclass_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;

    if matches!(message, WM_LBUTTONDOWN | WM_MOUSEWHEEL | WM_KEYDOWN) && !state_ptr.is_null() {
        (*state_ptr).hide_tooltip();
    }
    if message == WM_MOUSEMOVE {
        if !state_ptr.is_null() {
            let state = &mut *state_ptr;
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let packed = (((y & 0xFFFF) << 16) | (x & 0xFFFF)) as isize;
            let result = SendMessageW(hwnd, LB_ITEMFROMPOINT, WPARAM(0), LPARAM(packed)).0 as u32;
            let in_bounds = (result >> 16) == 0;
            let index = (result & 0xFFFF) as usize;
            if in_bounds
                && index >= 1
                && let Some(entry) = state.history.entries.get(index - 1)
            {
                let text = entry_detail(entry);
                let mut pt = POINT { x, y };
                let _ = ClientToScreen(hwnd, &mut pt);
                pt.x += 12;
                pt.y += 18;
                state.show_tooltip(&text, pt);
                return LRESULT(0);
            }
            state.hide_tooltip();
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
        }
        return LRESULT(0);
    }
    if message == WM_MOUSELEAVE {
        if !state_ptr.is_null() {
            (*state_ptr).hide_tooltip();
        }
        return LRESULT(0);
    }

    let prev: isize = if !state_ptr.is_null() {
        (*state_ptr).listbox_prev_proc
    } else {
        0
    };
    if prev != 0 {
        let prev_proc: WNDPROC = std::mem::transmute(prev);
        CallWindowProcW(prev_proc, hwnd, message, wparam, lparam)
    } else {
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
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
    let tip = wide("Notch media overlay");
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
    if state.config.behavior.start_on_login {
        autostart_flags |= MF_CHECKED;
    }
    let mut close_tray_flags = MF_STRING;
    if state.config.behavior.close_to_tray {
        close_tray_flags |= MF_CHECKED;
    }
    unsafe {
        let _ = AppendMenuW(menu, open_flags, MENU_OPEN_ID, PCWSTR(wide("Open Notch").as_ptr()));
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
        let current_secs = state.config.overlay.duration_ms / 1000;
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
                    state.config.behavior.start_on_login = !state.config.behavior.start_on_login;
                    let _ = state.config.save();
                    if let Err(error) = autostart::apply(state.config.behavior.start_on_login) {
                        error!("start-on-login update failed: {error:#}");
                    }
                }
                MENU_CLOSE_TRAY_ID => {
                    state.config.behavior.close_to_tray = !state.config.behavior.close_to_tray;
                    let _ = state.config.save();
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
                    state.config.overlay.duration_ms = 2000;
                    let _ = state.config.save();
                    set_duration(state.overlay_hwnd, 2000);
                }
                MENU_DURATION_3S => {
                    state.config.overlay.duration_ms = 3000;
                    let _ = state.config.save();
                    set_duration(state.overlay_hwnd, 3000);
                }
                MENU_DURATION_5S => {
                    state.config.overlay.duration_ms = 5000;
                    let _ = state.config.save();
                    set_duration(state.overlay_hwnd, 5000);
                }
                MENU_DURATION_10S => {
                    state.config.overlay.duration_ms = 10000;
                    let _ = state.config.save();
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
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            (*state).hwnd = hwnd;
        }
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;
    match message {
        WM_CREATE => {
            if !state_ptr.is_null() {
                (*state_ptr).create_children();
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
        WM_TIMER if wparam.0 as usize == TIMER_LOGS_ID => {
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
                                    state.config.behavior.start_on_login = !state.config.behavior.start_on_login;
                                    let _ = state.config.save();
                                    if let Err(error) = autostart::apply(state.config.behavior.start_on_login) {
                                        error!("start-on-login update failed: {error:#}");
                                    }
                                    state.invalidate();
                                }
                                SettingId::CloseToTray => {
                                    state.config.behavior.close_to_tray = !state.config.behavior.close_to_tray;
                                    let _ = state.config.save();
                                    state.invalidate();
                                }
                                SettingId::Duration => {
                                    let segments = segment_rects(&control_rect, 4, (4.0 * scale) as i32);
                                    let values = [2000u64, 3000, 5000, 10000];
                                    if let Some((i, _)) =
                                        segments.iter().enumerate().find(|(_, s)| x >= s.left && x < s.right)
                                    {
                                        state.config.overlay.duration_ms = values[i];
                                        let _ = state.config.save();
                                        set_duration(state.overlay_hwnd, values[i]);
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
                    let (client_w, _) = client_size(hwnd);
                    let hover = if x < sidebar_w {
                        None
                    } else {
                        state.settings_hover_at(x, y, sidebar_w, client_w, pad, scale)
                    };
                    if hover != state.settings_hover {
                        state.settings_hover = hover;
                        state.invalidate();
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
                    state.invalidate();
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if state.settings_hover.is_some() {
                    state.settings_hover = None;
                    state.invalidate();
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
                state.config.overlay.position_x = Some(wparam.0 as i32);
                state.config.overlay.position_y = Some(lparam.0 as i32);
                let _ = state.config.save();
                set_position(state.overlay_hwnd, OverlayPos::from_config(&state.config));
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
            artwork: None,
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
                track: track(&format!("Track {index}")),
                state: PlaybackState::Playing,
            });
        }
        assert_eq!(history.len(), 3);
        let titles: Vec<_> = history.iter().map(|entry| entry.track.title.as_str()).collect();
        assert_eq!(titles, ["Track 2", "Track 3", "Track 4"]);
    }

    #[test]
    fn row_falls_back_to_source_app_when_artist_is_blank() {
        let mut blank = track("Song");
        blank.artist = "   ".into();
        let row = history_row(&blank, Local::now(), PlaybackState::Playing);
        assert!(row.contains("Song"));
        assert!(row.contains("Spotify"));

        let titled = track("Song");
        let row = history_row(&titled, Local::now(), PlaybackState::Paused);
        assert!(row.contains("The Artist"));
    }
}
