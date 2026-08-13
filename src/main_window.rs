use crate::autostart;
use crate::config::{Config, HorizontalPosition, LayoutMode, MonitorMode, VerticalPosition};
use crate::events::{
    COMPACT_POSITION_MSG, MEDIA_EVENT_MSG, MediaEvent, POSITION_MSG, PlaybackState, TOGGLE_MSG, TrackInfo,
    media_event_into_owned,
};
use crate::gdi::{FontProvider, draw_string};
use crate::overlay::{
    EventQueue, OverlayPos, enumerate_displays_cached, invalidate_display_cache, set_dismiss_on_hover, set_duration,
    set_expand_compact_on_hover, set_layout, set_positions, show_sample,
};
use crate::process_picker;
use crate::process_picker::{AUTO_SOURCES_RESULT_MSG, PICKER_RESULT_MSG};
use crate::winutil::{StateClaim, clear_window_state, set_window_state, wide, window_state};
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use log::{debug, error, info, warn};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, GlobalFree, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CAPTION_COLOR, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, AlphaBlend, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, BeginPaint, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET,
    DEFAULT_PITCH, DIB_RGB_COLORS, DeleteDC, DeleteObject, EndPaint, FF_DONTCARE, FillRect, GetStockObject, HBITMAP,
    HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SelectObject, SetBkColor,
    SetTextColor,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, NMHDR, NMTTDISPINFOW, ODS_SELECTED, TOOLTIPS_CLASSW, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_DELTOOLW,
    TTM_SETMAXTIPWIDTH, TTM_SETTOOLINFOW, TTN_GETDISPINFOW, TTS_ALWAYSTIP, TTS_NOPREFIX, WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VIRTUAL_KEY, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN,
    VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetClientRect, GetCursorPos, HICON, HMENU, HWND_TOP, IDI_APPLICATION, IsWindowVisible, KillTimer, LB_ADDSTRING,
    LB_DELETESTRING, LB_GETCOUNT, LB_GETITEMHEIGHT, LB_GETITEMRECT, LB_GETTOPINDEX, LB_INSERTSTRING, LB_SETITEMHEIGHT,
    LB_SETTOPINDEX, LBS_HASSTRINGS, LBS_NOINTEGRALHEIGHT, LBS_OWNERDRAWFIXED, LoadIconW, MF_CHECKED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, PostMessageW, PostQuitMessage, RegisterWindowMessageW, SW_HIDE, SW_SHOW, SW_SHOWMAXIMIZED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetForegroundWindow, SetTimer, SetWindowPos,
    ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WINDOW_STYLE, WM_APP, WM_CLOSE,
    WM_CREATE, WM_CTLCOLORLISTBOX, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_DRAWITEM, WM_KEYDOWN,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_NOTIFY, WM_NULL, WM_PAINT,
    WM_RBUTTONUP, WM_SETFONT, WM_SIZE, WM_TIMER, WS_CHILD, WS_CLIPCHILDREN, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW,
    WS_POPUP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{PCWSTR, PWSTR};

const WM_TRAY: u32 = WM_APP + 2;
const TRAY_ID: u32 = 1;
const MENU_OPEN_ID: usize = 1001;
const MENU_NOTIFY_ID: usize = 1002;
const MENU_AUTOSTART_ID: usize = 1003;
const MENU_CLOSE_TRAY_ID: usize = 1004;
const MENU_QUIT_ID: usize = 1006;
const MENU_DURATION_2S: usize = 1017;
const MENU_DURATION_3S: usize = 1018;
const MENU_DURATION_5S: usize = 1019;
const MENU_DURATION_10S: usize = 1020;
const MENU_MONITOR_ACTIVE: usize = 1021;
const MENU_MONITOR_PRIMARY: usize = 1022;
/// Display entries in the Monitor submenu use sequential ids starting here;
/// display `i` gets `MENU_MONITOR_DISPLAY_BASE + i`.
const MENU_MONITOR_DISPLAY_BASE: usize = 1023;
/// Layout-mode entries of the tray "Layout" submenu.
const MENU_LAYOUT_EXPANDED: usize = 1024;
const MENU_LAYOUT_COMPACT: usize = 1025;
const MENU_LAYOUT_AUTO: usize = 1026;
const LISTBOX_ID: usize = 2;
/// History rows are kept in the heap (as entries) and duplicated in the
/// listbox as UTF-16 row strings, so the cap directly sizes the app's
/// baseline memory (~1 KB per row across both copies).
const HISTORY_CAP: usize = 400;
/// Timer used to clear the "Copied" feedback on the Copy logs button.
const TIMER_LOGS_ID: usize = 101;
/// Timer used to keep the native history tooltip's item rects in sync (scroll).
const TIMER_TOOLTIPS_ID: usize = 102;
/// One-shot timer that frees the cached artwork blit after the window has
/// been tray-hidden for `IDLE_ART_RELEASE_MS` (see `on_close`). The blit
/// rebuilds lazily at the next paint.
const IDLE_ART_TIMER_ID: usize = 103;
const IDLE_ART_RELEASE_MS: u32 = 30_000;
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
    Layout,
    Position,
    SeparateCompact,
    DismissOnHover,
    ExpandCompactOnHover,
    CompactPosition,
    AutoCompactApps,
    Monitor,
    ShowSample,
    CopyLogs,
    OpenConfig,
}

enum SettingsItem {
    Header { text: &'static str, rect: RECT },
    Row { id: SettingId, rect: RECT },
}

/// A keyboard-focusable Settings-pane control. `cx`/`cy` is the window client
/// coordinate at the control's center — the keyboard handler activates a control
/// by posting a synthetic `WM_LBUTTONDOWN` there, so it reuses the existing mouse
/// click path verbatim. The list order (top-to-bottom, left-to-right within a
/// row) is what Tab/arrows walk.
struct SettingsFocus {
    row_index: usize,
    sub: SettingSub,
    cx: i32,
    cy: i32,
}

const SETTINGS_SURFACE: [u8; 4] = [0x1B, 0x1B, 0x1B, 0xFF];
const SETTINGS_BORDER: [u8; 4] = [0x2D, 0x2D, 0x2D, 0xFF];
const SETTINGS_HOVER: [u8; 4] = [0x24, 0x24, 0x24, 0xFF];
const SETTINGS_TEXT: [u8; 4] = [0xF0, 0xF0, 0xF0, 0xFF];
const SETTINGS_MUTED: [u8; 4] = [0xC8, 0xC8, 0xC8, 0xFF];
const SETTINGS_FAINT: [u8; 4] = [0x7A, 0x7A, 0x7A, 0xFF];

/// Mix weights (toward `SETTINGS_SURFACE`) for the accent soft fills. Kept
/// as named constants so the brush rebuild and the render-time contrast guard
/// below stay in lockstep — a drift between the two would silently recompute
/// the wrong backdrop for the label guard.
const SETTINGS_ACCENT_SOFT_WEIGHT: f32 = 0.28;
const SETTINGS_NEAR_WEIGHT: f32 = 0.55;
const SETTINGS_ADJUST_HOVER_WEIGHT: f32 = 0.45;

/// Blends `a` over `b` (0.0 = b, 1.0 = a).
fn mix(a: [u8; 4], b: [u8; 4], t: f32) -> [u8; 4] {
    [
        (a[0] as f32 * t + b[0] as f32 * (1.0 - t)) as u8,
        (a[1] as f32 * t + b[1] as f32 * (1.0 - t)) as u8,
        (a[2] as f32 * t + b[2] as f32 * (1.0 - t)) as u8,
        0xFF,
    ]
}

/// The settings window's effective accent pair from the playing song's
/// decoded artwork: the album palette's primary (brightened against the
/// settings surface so accent text stays readable, like the pill guards its
/// text) and secondary. When there is no artwork or the pixels yield no
/// palette, both fall back to the configured accent — the default pink
/// theme. `decoded_art` is the worker's premultiplied-BGRA decode, the same
/// buffer the pill palettizes from.
fn accent_from_art(decoded_art: Option<&[u8]>, fallback: [u8; 4]) -> ([u8; 4], [u8; 4]) {
    let Some(palette) = decoded_art
        .and_then(crate::overlay::pm_bgra_to_rgba)
        .and_then(|rgba| crate::palette::palette_from_rgba(&rgba))
    else {
        return (fallback, fallback);
    };
    (
        crate::overlay::ensure_contrast(palette.primary, SETTINGS_SURFACE, crate::overlay::TEXT_CONTRAST_AA),
        palette.secondary,
    )
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
    /// The left half of the Diagnostics row ("Open logs" button).
    Open,
    /// The right half of the Diagnostics row ("Copy logs" button).
    Copy,
    /// The left half of the Config row ("Open config" button).
    OpenConfig,
    /// The right half of the Config row ("Reload config" button).
    ReloadConfig,
}

/// Sub-rects of the Position row: value text, the six anchor segments, the
/// Reset button and the Adjust button. Paint, hit-test and hover all use this.
struct PositionParts {
    value_row: RECT,
    anchors: Vec<RECT>,
    reset: RECT,
    adjust: RECT,
}

/// The label/control split of a settings row: the label takes the first 42%
/// of the row, the control the remainder, both inset from the row edges.
/// Paint, hit-test, click and `position_parts` all derive the same rects.
struct RowSplit {
    label: RECT,
    control: RECT,
}

fn row_split(rect: &RECT, scale: f32) -> RowSplit {
    let label_w = (((rect.right - rect.left) as f32) * 0.42) as i32;
    RowSplit {
        label: RECT {
            left: rect.left + (12.0 * scale) as i32,
            top: rect.top,
            right: rect.left + label_w,
            bottom: rect.bottom,
        },
        control: RECT {
            left: rect.left + label_w + (10.0 * scale) as i32,
            top: rect.top,
            right: rect.right - (10.0 * scale) as i32,
            bottom: rect.bottom,
        },
    }
}

/// Splits `rect` down the middle with a small gap, for rows that host two
/// side-by-side controls (the Diagnostics "Open logs" / "Copy logs" buttons).
fn halve(rect: &RECT, gap: i32) -> (RECT, RECT) {
    let mid = rect.left + (rect.right - rect.left) / 2;
    let half_gap = gap / 2;
    (
        RECT {
            left: rect.left,
            top: rect.top,
            right: mid - half_gap,
            bottom: rect.bottom,
        },
        RECT {
            left: mid + half_gap,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        },
    )
}

/// Whether two rects overlap in area. Used to skip repainting rows that the
/// invalid region does not cover.
fn rects_intersect(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

/// The overlay position as a display string: "Custom (x, y)" for a dragged
/// placement, otherwise "top-center" style. Shared by the Activity pane and
/// the settings value so the wording cannot drift.
fn position_label(config: &Config) -> String {
    if config.overlay.position_x.is_some() {
        format!(
            "Custom ({}, {})",
            config.overlay.position_x.unwrap_or(0),
            config.overlay.position_y.unwrap_or(0)
        )
    } else {
        format!(
            "{}-{}",
            match config.overlay.vertical {
                VerticalPosition::Top => "top",
                VerticalPosition::Bottom => "bottom",
            },
            match config.overlay.horizontal {
                HorizontalPosition::Left => "left",
                HorizontalPosition::Center => "center",
                HorizontalPosition::Right => "right",
            }
        )
    }
}

/// The Compact layout's effective position as a display string, via
/// `compact_effective` (independent fields when `compact_position_separate`
/// is set, otherwise the live Expanded position).
fn compact_position_label(config: &Config) -> String {
    let p = config.overlay.compact_effective();
    if p.x.is_some() {
        format!("Custom ({}, {})", p.x.unwrap_or(0), p.y.unwrap_or(0))
    } else {
        format!(
            "{}-{}",
            match p.vertical {
                VerticalPosition::Top => "top",
                VerticalPosition::Bottom => "bottom",
            },
            match p.horizontal {
                HorizontalPosition::Left => "left",
                HorizontalPosition::Center => "center",
                HorizontalPosition::Right => "right",
            }
        )
    }
}

/// The overlay's target display as a display string, e.g. "Active window",
/// "Primary", "Display 2". An index beyond the currently attached displays
/// still shows the configured intent, flagged as unavailable — the config is
/// not rewritten on a hot-unplug, so the label must not lie about it.
fn monitor_label(config: &Config, display_count: usize) -> String {
    match config.overlay.monitor {
        MonitorMode::ActiveWindow => "Active window".to_string(),
        MonitorMode::Primary => "Primary".to_string(),
        MonitorMode::Index(index) => {
            let n = index as usize;
            if n < display_count {
                format!("Display {}", n + 1)
            } else {
                format!("Display {} (unavailable)", n + 1)
            }
        }
    }
}

/// The next monitor mode when the settings row is clicked: Active window →
/// Primary → Display 1 → Display 2 → … → back to Active window. With fewer
/// than two displays the list degrades gracefully instead of offering an
/// index that could never resolve.
fn next_monitor_mode(current: MonitorMode, display_count: usize) -> MonitorMode {
    match current {
        MonitorMode::ActiveWindow => MonitorMode::Primary,
        MonitorMode::Primary => {
            if display_count > 1 {
                MonitorMode::Index(0)
            } else {
                MonitorMode::ActiveWindow
            }
        }
        MonitorMode::Index(index) => {
            if (index as usize) + 1 < display_count {
                MonitorMode::Index(index + 1)
            } else {
                MonitorMode::ActiveWindow
            }
        }
    }
}

fn position_parts(rect: &RECT, scale: f32) -> PositionParts {
    let control = row_split(rect, scale).control;
    let control_left = control.left;
    let control_right = control.right;
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
    accent: HBRUSH,
    /// Accent blended toward the surface at 28%, the fill of active segments.
    accent_soft: HBRUSH,
    /// Accent blended toward the surface at 55%, the fill of approximate
    /// (non-exact) duration presets.
    near: HBRUSH,
    /// Accent blended toward the surface at 45%, the hovered Adjust fill.
    adjust_hover: HBRUSH,
    /// Flat dark fill of idle outline buttons.
    small_fill: HBRUSH,
    /// Accent-blended hover fill of outline buttons.
    small_hover: HBRUSH,
}

#[allow(clippy::too_many_arguments)]
fn draw_segment_button(
    fonts: &FontProvider,
    hdc: HDC,
    rect: &RECT,
    label: &str,
    active: bool,
    hovered: bool,
    scale: f32,
    brushes: SettingsBrushes,
) {
    unsafe {
        let _ = FillRect(hdc, rect, if active { brushes.accent } else { brushes.border });
    }
    let inner = RECT {
        left: rect.left + 1,
        top: rect.top + 1,
        right: rect.right - 1,
        bottom: rect.bottom - 1,
    };
    unsafe {
        let fill = if active {
            brushes.accent_soft
        } else if hovered {
            brushes.hover
        } else {
            brushes.surface
        };
        let _ = FillRect(hdc, &inner, fill);
    }
    let mut t = inner;
    let tc = if active { SETTINGS_TEXT } else { SETTINGS_MUTED };
    draw_string(fonts, hdc, label, &mut t, (10.0 * scale) as i32, tc, active, true);
}

/// Draws an outline button (accent border, dark fill, accent label).
#[allow(clippy::too_many_arguments)]
fn draw_small_button(
    fonts: &FontProvider,
    hdc: HDC,
    rect: &RECT,
    label: &str,
    accent: [u8; 4],
    hovered: bool,
    scale: f32,
    brushes: SettingsBrushes,
) {
    unsafe {
        let _ = FillRect(hdc, rect, brushes.accent);
    }
    let inner = RECT {
        left: rect.left + 1,
        top: rect.top + 1,
        right: rect.right - 1,
        bottom: rect.bottom - 1,
    };
    unsafe {
        let _ = FillRect(
            hdc,
            &inner,
            if hovered {
                brushes.small_hover
            } else {
                brushes.small_fill
            },
        );
    }
    let mut t = inner;
    draw_string(fonts, hdc, label, &mut t, (10.0 * scale) as i32, accent, true, true);
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
struct HistoryEntry {
    at: DateTime<Local>,
    /// Pre-formatted HH:MM:SS time, so the listbox paint never re-formats
    /// (or allocates) per row per repaint.
    at_label: String,
    track: TrackInfo,
    state: PlaybackState,
    /// Whether the source session passed the `media_sources` filter.
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
    /// Cached GDI source for AlphaBlend of the decoded artwork: a memory DC
    /// with the premultiplied pixels in a DIB section, built once per decode
    /// so repaints blend without per-paint DC/DIB allocation. Built directly
    /// from `track.decoded_art` (the worker's premultiplied BGRA) on the
    /// first paint that needs it — no intermediate byte copy is kept.
    art_blit: Option<ArtBlit>,
    /// FNV-1a of the artwork bytes this cache was decoded from, so a metadata
    /// refresh with unchanged artwork does not re-decode.
    art_fingerprint: Option<u64>,
    /// A decode failure is cached: with this set, paint skips the retry until
    /// the artwork bytes change, so a corrupt cover is attempted once instead
    /// of on every repaint.
    art_decode_failed: bool,
}

/// Cached memory DC + DIB section holding the decoded premultiplied artwork
/// pixels, used as the AlphaBlend source. Freed with the activity (track
/// change) or at window destruction.
struct ArtBlit {
    mem: HDC,
    hbm: HBITMAP,
    old: HGDIOBJ,
    base: i32,
}

impl Drop for ArtBlit {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.mem, self.old);
            let _ = DeleteObject(self.hbm);
            let _ = DeleteDC(self.mem);
        }
    }
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
    fonts: FontProvider,
    gray_brush: HBRUSH,
    accent_brush: HBRUSH,
    black_brush: HBRUSH,
    sidebar_bg_brush: HBRUSH,
    sidebar_highlight_brush: HBRUSH,
    settings_border_brush: HBRUSH,
    settings_surface_brush: HBRUSH,
    settings_hover_brush: HBRUSH,
    settings_accent_soft_brush: HBRUSH,
    settings_near_brush: HBRUSH,
    settings_adjust_hover_brush: HBRUSH,
    settings_small_fill_brush: HBRUSH,
    settings_small_hover_brush: HBRUSH,
    history_header_brush: HBRUSH,
    history_selected_brush: HBRUSH,
    history_row_even_brush: HBRUSH,
    history_row_odd_brush: HBRUSH,
    /// Effective accent: the playing song's album palette primary (guarded
    /// against the settings surface so accent text stays readable), falling
    /// back to the configured accent. Rebuilt when the artwork changes.
    accent_color: [u8; 4],
    /// Effective secondary accent: the album palette secondary, falling back
    /// to the configured accent. Drives the dark highlight surfaces (sidebar
    /// active pane, history selection).
    accent_secondary: [u8; 4],
    /// The decoded artwork the current accent was derived from. The palette
    /// is recomputed only when this `Arc` changes.
    accent_art_source: Option<Arc<[u8]>>,
    active_pane: Pane,
    /// Hovered settings row (row index, sub-control) for highlight.
    settings_hover: Option<(usize, SettingSub)>,
    /// Native TOOLTIPS_CLASS control showing full history details on hover.
    tooltip_ctrl: HWND,
    /// Currently registered tool range [start, end) in the native tooltip:
    /// the visible band of listbox rows. Unchanged (count, top, size) skips
    /// the sync; a scroll only touches the rows that crossed the band.
    tooltip_range: Option<(usize, usize)>,
    /// Set when an event batch changed the list; the tooltips are rebuilt once
    /// per batch instead of once per event.
    tooltips_dirty: bool,
    /// Timestamp of the last "Copy logs" press, for the "Copied" feedback.
    logs_copied_at: Option<Instant>,
    /// Shared slot for the process picker's confirmed allow-list patterns. The
    /// picker writes the result here and posts a bare `PICKER_RESULT_MSG`; no
    /// pointer ever crosses the message boundary.
    picker_result: Arc<Mutex<Option<Vec<String>>>>,
    /// Shared slot for the Auto-compact apps picker, which posts
    /// `AUTO_SOURCES_RESULT_MSG` (same contract as `picker_result`).
    auto_sources_result: Arc<Mutex<Option<Vec<String>>>>,
    /// Last playback state each source app reported, so a new track from a
    /// source starts with its own state instead of inheriting the previous
    /// activity's (which may belong to another app).
    source_states: HashMap<String, PlaybackState>,
    /// Insertion order of `source_states` keys, so the map can be capped by
    /// forgetting the oldest sources first.
    source_order: VecDeque<String>,
    /// Wake flag for the event queue: `true` while a `MEDIA_EVENT_MSG` is in
    /// flight. The forwarder and this window only post when the flag was
    /// clear, so an event burst collapses into one wake message per drain.
    wake: Arc<AtomicBool>,
}

/// Set when this window's WM_NCCREATE claims the state box handed over in
/// `lpCreateParams`, so a failed CreateWindowExW can tell whether the box was
/// taken by the system (and freed in WM_NCDESTROY) or still belongs to the
/// caller. See `winutil::StateClaim` for the shared mechanics.
static MAIN_STATE_CLAIMED: StateClaim = StateClaim::new();

/// Creates the main window: a maximized tracker with current activity,
/// per-session history, and a tray icon. The caller runs the message loop.
pub fn create_window(
    config: Arc<RwLock<Config>>,
    queue: EventQueue,
    overlay_hwnd: HWND,
    wake: Arc<AtomicBool>,
) -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.context("getting the process module")?;
    let instance: HINSTANCE = module.into();
    let class_name = wide("WinGlanceMainWindow");
    register_main_class(instance, &class_name)?;

    let mut state = Box::new(MainWindowState::new(config.clone(), queue, overlay_hwnd, instance));
    state.wake = wake;
    let state_ptr = Box::into_raw(state);
    MAIN_STATE_CLAIMED.reset();
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
            // freed in WM_NCDESTROY. WM_NCCREATE flips MAIN_STATE_CLAIMED when
            // it takes the box; if it never ran (a creation failure before the
            // window object existed), the box still belongs to us and must be
            // freed here — otherwise it leaks. When WM_NCCREATE did run, the
            // system tears the window down through WM_NCDESTROY first, so
            // freeing the box here would double-free it.
            if let Some(state) = MAIN_STATE_CLAIMED.take_unclaimed(state_ptr) {
                drop(state);
            }
            return Err(error.into());
        }
    };

    unsafe {
        if config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .behavior
            .start_in_tray
        {
            let _ = ShowWindow(hwnd, SW_HIDE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
            // The tooltip timer is normally started by show_window(); this
            // visible-at-start path bypasses it, so start it and sync once
            // here (the window is already shown, so sync_tooltips can run).
            let _ = SetTimer(hwnd, TIMER_TOOLTIPS_ID, 1000, None);
            let state_ref = &mut *state_ptr;
            state_ref.sync_tooltips();
        }
    }
    if let Err(error) = install_tray_icon(hwnd) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return Err(error);
    }
    debug!("tray icon installed");
    Ok(hwnd)
}

/// Upper bound on the remembered per-source playback states. A system that
/// churns through many SMTC sources (apps that recreate their session on
/// every launch) must not grow the map without bound; beyond it the oldest
/// sources are forgotten and fall back to the default `Playing` state.
const SOURCE_STATES_CAP: usize = 64;

impl MainWindowState {
    fn cfg(&self) -> std::sync::RwLockReadGuard<'_, Config> {
        self.config.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Mutates the config under a single write-lock scope, then persists it.
    /// Never call `self.cfg()` (a read lock) from inside `mutate`. The lock
    /// is released before `save()`: the disk write would otherwise stall
    /// every config read (the SMTC worker's flush decisions, the overlay's
    /// behavior flags) for its duration. The clone is safe because the main
    /// window is the single writer — no other site can change the config
    /// between the lock release and the save.
    fn mutate_config(&mut self, mutate: impl FnOnce(&mut Config)) {
        // A poisoned lock still yields the (possibly stale) config;
        // recovering beats panicking on the UI thread for the rest of
        // the run.
        let changed = {
            let mut cfg = self.config.write().unwrap_or_else(|poisoned| poisoned.into_inner());
            mutate(&mut cfg);
            cfg.clone()
        };
        if let Err(error) = changed.save() {
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
            fonts: FontProvider::new(96),
            gray_brush: HBRUSH::default(),
            accent_brush: HBRUSH::default(),
            black_brush: HBRUSH::default(),
            sidebar_bg_brush: HBRUSH::default(),
            sidebar_highlight_brush: HBRUSH::default(),
            settings_border_brush: HBRUSH::default(),
            settings_surface_brush: HBRUSH::default(),
            settings_hover_brush: HBRUSH::default(),
            settings_accent_soft_brush: HBRUSH::default(),
            settings_near_brush: HBRUSH::default(),
            settings_adjust_hover_brush: HBRUSH::default(),
            settings_small_fill_brush: HBRUSH::default(),
            settings_small_hover_brush: HBRUSH::default(),
            history_header_brush: HBRUSH::default(),
            history_selected_brush: HBRUSH::default(),
            history_row_even_brush: HBRUSH::default(),
            history_row_odd_brush: HBRUSH::default(),
            accent_color: [0, 0, 0, 255],
            accent_secondary: [0, 0, 0, 255],
            accent_art_source: None,
            active_pane: Pane::Activity,
            settings_hover: None,
            tooltip_ctrl: HWND::default(),
            tooltip_range: None,
            tooltips_dirty: false,
            logs_copied_at: None,
            picker_result: Arc::new(Mutex::new(None)),
            auto_sources_result: Arc::new(Mutex::new(None)),
            source_states: HashMap::new(),
            source_order: VecDeque::new(),
            wake: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Creates the history listbox font at the given scale, matching the
    /// height the rows are laid out at. Recreated when the DPI changes.
    fn make_listbox_font(scale: f32) -> HFONT {
        let font_name = wide("Segoe UI");
        unsafe {
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
        }
    }

    /// Reacts to the window moving to a monitor with a different DPI: the
    /// listbox font, the row height and the tooltip geometry are frozen at
    /// the creation DPI otherwise, leaving rows overlapping the header after
    /// a cross-DPI move.
    fn on_dpi_changed(&mut self, dpi: u32) {
        debug!("DPI changed to {dpi}");
        unsafe {
            if !self.listbox_font.0.is_null() {
                let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(self.listbox_font.0));
            }
        }
        let scale = dpi.max(96) as f32 / 96.0;
        self.listbox_font = Self::make_listbox_font(scale);
        self.fonts = FontProvider::new(dpi);
        if !self.listbox.0.is_null() {
            unsafe {
                let item_h = (18.0 * scale).round() as i32;
                let _ = SendMessageW(self.listbox, LB_SETITEMHEIGHT, WPARAM(0), LPARAM(item_h as isize));
                let _ = SendMessageW(
                    self.listbox,
                    WM_SETFONT,
                    WPARAM(self.listbox_font.0 as usize),
                    LPARAM(1),
                );
            }
        }
        self.layout();
        self.sync_tooltips();
        self.invalidate();
    }

    fn create_children(&mut self) {
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        self.listbox_font = Self::make_listbox_font(scale);
        self.gray_brush = unsafe { CreateSolidBrush(colorref(0x1E, 0x1E, 0x1E)) };
        // Fixed-color brushes for the panes, created once instead of per paint
        // (a settings repaint previously created ~40 brushes).
        self.black_brush = unsafe { CreateSolidBrush(COLORREF(0)) };
        self.sidebar_bg_brush = unsafe { CreateSolidBrush(COLORREF(0x0A0A0A)) };
        self.settings_border_brush =
            unsafe { CreateSolidBrush(colorref(SETTINGS_BORDER[0], SETTINGS_BORDER[1], SETTINGS_BORDER[2])) };
        self.settings_surface_brush =
            unsafe { CreateSolidBrush(colorref(SETTINGS_SURFACE[0], SETTINGS_SURFACE[1], SETTINGS_SURFACE[2])) };
        self.settings_hover_brush =
            unsafe { CreateSolidBrush(colorref(SETTINGS_HOVER[0], SETTINGS_HOVER[1], SETTINGS_HOVER[2])) };
        self.settings_small_fill_brush = unsafe { CreateSolidBrush(COLORREF(0x00121212)) };
        // History-row brushes: a fixed four-color set, created once instead of
        // per owner-draw row (every scroll tick repaints every visible row).
        self.history_header_brush = unsafe { CreateSolidBrush(COLORREF(0x00141414)) };
        self.history_row_even_brush = unsafe { CreateSolidBrush(COLORREF(0)) };
        self.history_row_odd_brush = unsafe { CreateSolidBrush(COLORREF(0x000E0E0E)) };
        // The accent-derived brushes start from the configured accent (the
        // default pink theme) and are rebuilt when the playing song's artwork
        // changes (see `update_accent`). The highlight surfaces (sidebar
        // active pane, history selection) are dark tints of the secondary
        // accent — the whole theme is accent-based, with no fixed blue/green
        // tones.
        (self.accent_color, self.accent_secondary) = accent_from_art(None, self.cfg().appearance.accent_color);
        self.rebuild_accent_brushes();

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
            }
            self.sync_tooltips();
        }
    }

    /// Shows or hides the pane-owned child windows (history listbox and its
    /// tooltip) to match the active pane. Called on pane switches and on
    /// window show/hide — not from WM_PAINT, which would call ShowWindow on
    /// every repaint.
    fn apply_pane(&self) {
        unsafe {
            let _ = ShowWindow(
                self.listbox,
                if self.active_pane == Pane::Activity {
                    SW_SHOW
                } else {
                    SW_HIDE
                },
            );
            if self.active_pane != Pane::Activity {
                let _ = ShowWindow(self.tooltip_ctrl, SW_HIDE);
            }
        }
    }

    /// Rebuilds the per-item tool definitions so rects and row count match
    /// the listbox (rows are fixed-height, so scroll changes the mapping).
    /// The 1 Hz timer calls this constantly, so the full rebuild (3N+1
    /// SendMessageW) is skipped when the item count and scroll position are
    /// unchanged since the last sync. Only the *visible* band of rows is
    /// registered (off-screen rows cannot be hovered): a scroll updates the
    /// band's rects in place via TTM_NEWTOOLW and drops the rows that
    /// scrolled out, so the per-tick message count is bounded by the visible
    /// row count instead of the history size. While the window is hidden in
    /// the tray there is nothing to sync, so the timer's probe messages are
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
            let mut client = RECT::default();
            let _ = GetClientRect(self.listbox, &mut client);
            let item_h = SendMessageW(self.listbox, LB_GETITEMHEIGHT, WPARAM(0), LPARAM(0)).0 as usize;
            let visible = client.bottom as usize / item_h.max(1) + 1;
            let end = (top + visible).min(count);
            if self.tooltip_range == Some((top, end)) {
                return;
            }
            let (old_start, old_end) = self.tooltip_range.unwrap_or((0, 0));
            for index in old_start..old_end {
                if index < top || index >= end {
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
            }
            for index in top..end {
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
                // Adds the tool, or updates the existing one's rect in place
                // (the row's client position moved with the scroll).
                let message = if index >= old_start && index < old_end {
                    TTM_SETTOOLINFOW
                } else {
                    TTM_ADDTOOLW
                };
                let _ = SendMessageW(
                    self.tooltip_ctrl,
                    message,
                    WPARAM(0),
                    LPARAM(&mut tool as *mut _ as isize),
                );
            }
            self.tooltip_range = Some((top, end));
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

    /// Records a source's playback state, capping the remembered set so a
    /// system that churns through many apps cannot grow it without bound.
    /// Forgetting the oldest source only falls back to the default state for
    /// its next track; it never drops user data.
    fn remember_source_state(&mut self, source: String, state: PlaybackState) {
        if !self.source_states.contains_key(&source) {
            self.source_order.push_back(source.clone());
        }
        self.source_states.insert(source, state);
        while self.source_order.len() > SOURCE_STATES_CAP {
            let Some(oldest) = self.source_order.pop_front() else {
                break;
            };
            self.source_states.remove(&oldest);
        }
    }

    /// Recreates the accent-derived brushes from the current effective
    /// colors: the accent brush + the four soft fills derive from
    /// `accent_color`, and the two highlight surfaces (sidebar active pane,
    /// history selection) derive from `accent_secondary`. Called once at
    /// window creation and whenever the playing song's palette changes; the
    /// old brushes are deleted first, so every paint site picks up the new
    /// accent without per-paint brush allocation.
    fn rebuild_accent_brushes(&mut self) {
        unsafe {
            for brush in [
                &mut self.accent_brush,
                &mut self.settings_accent_soft_brush,
                &mut self.settings_near_brush,
                &mut self.settings_adjust_hover_brush,
                &mut self.settings_small_hover_brush,
                &mut self.sidebar_highlight_brush,
                &mut self.history_selected_brush,
            ] {
                if !brush.0.is_null() {
                    let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(brush.0));
                }
            }
            let accent = self.accent_color;
            self.accent_brush = CreateSolidBrush(colorref(accent[0], accent[1], accent[2]));
            let soft = |weight: f32| -> HBRUSH {
                let c = mix(accent, SETTINGS_SURFACE, weight);
                CreateSolidBrush(colorref(c[0], c[1], c[2]))
            };
            self.settings_accent_soft_brush = soft(SETTINGS_ACCENT_SOFT_WEIGHT);
            self.settings_near_brush = soft(SETTINGS_NEAR_WEIGHT);
            self.settings_adjust_hover_brush = soft(SETTINGS_ADJUST_HOVER_WEIGHT);
            self.settings_small_hover_brush = soft(0.35);
            let highlight = |weight: f32| -> HBRUSH {
                let c = mix(self.accent_secondary, [0x0A, 0x0A, 0x0A, 0xFF], weight);
                CreateSolidBrush(colorref(c[0], c[1], c[2]))
            };
            self.sidebar_highlight_brush = highlight(0.15);
            self.history_selected_brush = highlight(0.20);
        }
    }

    /// Re-derives the accent from the current song's artwork after an event
    /// batch. The palette is recomputed only when the decoded-art `Arc`
    /// changed (a metadata refresh re-reporting the same cover must not
    /// recompute); the brushes are rebuilt and the window repainted only
    /// when the accent actually changed.
    fn update_accent(&mut self) {
        let art = self.current.as_ref().and_then(|c| c.track.decoded_art.clone());
        let unchanged = match (&self.accent_art_source, &art) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return;
        }
        self.accent_art_source = art.clone();
        let (primary, secondary) = accent_from_art(art.as_deref(), self.cfg().appearance.accent_color);
        if primary != self.accent_color || secondary != self.accent_secondary {
            self.accent_color = primary;
            self.accent_secondary = secondary;
            self.rebuild_accent_brushes();
            // Re-color the window title bar to match (COLORREF is 0x00BBGGRR,
            // hence the swapped red/blue channels).
            let color = COLORREF(((primary[2] as u32) << 16) | ((primary[1] as u32) << 8) | primary[0] as u32);
            let _ = unsafe {
                DwmSetWindowAttribute(
                    self.hwnd,
                    DWMWA_CAPTION_COLOR,
                    &color as *const COLORREF as *const c_void,
                    size_of::<u32>() as u32,
                )
            };
            debug!(
                "settings accent: primary=#{:02X}{:02X}{:02X} secondary=#{:02X}{:02X}{:02X}",
                primary[0], primary[1], primary[2], secondary[0], secondary[1], secondary[2]
            );
            self.invalidate();
        }
    }

    fn receive_events(&mut self) {
        // Clear the wake flag before draining; an event pushed while we drain
        // re-arms it (and possibly posts), so nothing stays stuck.
        self.wake.store(false, Ordering::Relaxed);
        let mut batch = Vec::new();
        if let Ok(mut queue) = self.queue.lock() {
            batch.extend(queue.drain(..));
        }
        // The queue carries Arc<MediaEvent> so the fan-out to both windows
        // never copies the event; recover the owned event here (zero-copy
        // when this window is the last holder, a clone otherwise).
        // Invalidation is deferred until after the batch: a burst of events
        // (session churn, a gapless album) redraws the current-activity area
        // once instead of once per event.
        let mut dirty = false;
        for event in batch.into_iter().map(media_event_into_owned) {
            match event {
                MediaEvent::TrackChanged(track) => {
                    self.add_track(track);
                    dirty = true;
                }
                MediaEvent::PlaybackStateChanged(state, source_app) => {
                    // Remember the state per source so a later track from the
                    // same source starts with the right state. The event only
                    // applies to the activity it belongs to: a playback change
                    // from another app must not rewrite the currently
                    // displayed track's state or push a history row under it.
                    self.remember_source_state(source_app.clone(), state);
                    if let Some(current) = &mut self.current
                        && current.track.source_app == source_app
                    {
                        current.state = state;
                        self.add_state_change(state);
                        dirty = true;
                    }
                }
                MediaEvent::SessionRejected {
                    source_app,
                    title,
                    artist,
                    state,
                    accepted,
                } => self.add_session(source_app, title, artist, state, accepted),
                MediaEvent::WorkerFailed { reason } => self.add_worker_failure(&reason),
            }
        }
        if dirty {
            self.invalidate();
        }
        // The accent follows the playing song's artwork; this runs even when
        // the batch only refreshed metadata (late cover arrival changes the
        // palette).
        self.update_accent();
        // One tooltip rebuild per batch: a session-churn burst otherwise
        // rebuilds the full tool set once per event.
        if self.tooltips_dirty {
            self.tooltips_dirty = false;
            self.sync_tooltips();
        }
        // Events that arrived while we were draining need a wake-up: re-arm
        // and post only if no wake message is already in flight. A failed
        // post drops the pending batch (and accounts for it) instead of
        // stranding events without a wake.
        crate::repost_if_pending(&self.queue, &self.wake, self.hwnd, "main window");
    }

    /// Appends a history row and syncs the listbox + tooltips. Artwork, the
    /// app icon and the decoded cover buffer are stripped before storing —
    /// the history is text-only, and the image bytes would be pure waste
    /// across hundreds of rows.
    fn push_history(&mut self, mut track: TrackInfo, state: PlaybackState, accepted: bool) {
        track.artwork = None;
        track.app_icon = None;
        track.decoded_art = None;
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
        // Clone text-only: the artwork and icon bytes (up to MBs) are stripped
        // before the clone so they are never copied just to be discarded.
        let mut track = current.track.clone();
        track.artwork = None;
        track.app_icon = None;
        self.push_history(track, state, true);
    }

    /// Records a session that was seen but not tracked (filtered by
    /// `media_sources` or on the churn cool-down). The row renders muted;
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

    /// Adds a prominent history row when the SMTC worker gave up permanently:
    /// the user must see that notifications stopped and need a restart to
    /// resume, instead of the app silently going quiet. The tray note makes
    /// the failure visible even while this window is hidden (start in tray).
    fn add_worker_failure(&mut self, reason: &str) {
        let track = TrackInfo {
            title: "Media notifications stopped".into(),
            artist: reason.to_string(),
            source_app: "WinGlance".into(),
            ..TrackInfo::default()
        };
        self.push_history(track, PlaybackState::Stopped, false);
        show_tray_note(self.hwnd, "Media notifications stopped", reason);
    }

    /// Updates the current activity (and its history row) for a track
    /// change. Called only from `receive_events`, which invalidates the
    /// window once after the whole batch — this method does not repaint.
    fn add_track(&mut self, track: TrackInfo) {
        let art_fingerprint = track.artwork.as_deref().map(fingerprint);
        // Metadata refresh for the same song (album/artwork arriving late):
        // update the current activity and the last history row in place
        // instead of appending a duplicate entry. Identity is the shared
        // `same_media` rule the overlay uses: a genuinely different cover for
        // the same title+artist (video vs audio version) is *new* media, so
        // it gets a fresh history row instead of silently overwriting the
        // previous song's row.
        let is_update = self.current.as_ref().is_some_and(|c| c.track.same_media(&track));

        if is_update {
            if let Some(current) = &mut self.current {
                current.track = track.clone();
                // Artwork is decoded lazily on first paint; a metadata refresh
                // re-reporting the same cover must not re-decode, so only bump
                // the fingerprint and drop the cached bitmap when bytes changed.
                if current.art_fingerprint != art_fingerprint {
                    free_art_blit(&mut current.art_blit);
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
            return;
        }

        // The new activity starts with the source's own last reported state
        // (the worker suppresses the paired playback event when it emits a
        // TrackChanged, so inheriting the previous activity's state could
        // show another app's Playing/Paused/Stopped).
        let state = self
            .source_states
            .get(&track.source_app)
            .copied()
            .unwrap_or(PlaybackState::Playing);
        // History row is text-only: strip the artwork bytes before the clone.
        let mut history_track = track.clone();
        history_track.artwork = None;
        self.push_history(history_track, state, true);
        self.current = Some(CurrentActivity {
            track,
            state,
            // The blit is built lazily on first paint; the window starts
            // hidden (start_in_tray), so a track that never gets looked at
            // pays no GDI cost.
            art_blit: None,
            art_fingerprint,
            art_decode_failed: false,
        });
    }

    fn paint(&mut self) {
        let mut paint = PAINTSTRUCT::default();
        let hdc = unsafe { BeginPaint(self.hwnd, &mut paint) };
        if hdc.0.is_null() {
            return;
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
        let accent = self.accent_color;
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
                &self.fonts,
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

        // Draw content based on active pane. The settings pane skips rows that
        // do not intersect the invalid region, so hover repaints only touch the
        // rows that changed instead of the whole maximized window.
        match self.active_pane {
            Pane::Activity => self.paint_activity(hdc, content_left, client_w, client_h, scale, pad),
            Pane::Settings => self.paint_settings(hdc, content_left, client_w, client_h, scale, pad, &paint.rcPaint),
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
            &self.fonts,
            hdc,
            "NOW PLAYING",
            &mut header_rect,
            (11.0 * scale) as i32,
            self.accent_color,
            true,
            false,
        );

        let art = (ART_SIZE * scale).round() as i32;
        let art_x = content_left + pad;
        let art_y = (ART_Y * scale) as i32;
        let text_left = art_x + art + (12.0 * scale) as i32;
        let text_right = client_w - pad;

        let accent_color = self.accent_color;
        let text_color = self.cfg().appearance.text_color;

        // The SMTC worker already decoded the artwork once at event time (see
        // smtc.rs `with_decoded_art`); the UI thread only ever copies the
        // cached pixels, never decodes an image. The decode side is adaptive
        // (per-DPI), so derive it from the buffer length.
        if let Some(current) = &mut self.current
            && current.art_blit.is_none()
            && !current.art_decode_failed
            && current.art_fingerprint.is_some()
        {
            match current.track.decoded_art.as_deref() {
                Some(pm) => {
                    let base = ((pm.len() / 4) as f64).sqrt() as i32;
                    current.art_blit = build_art_blit(pm, base);
                    if current.art_blit.is_none() {
                        log_art_blit_failure();
                    }
                }
                // No decoded pixels (the worker's decode failed): cache the
                // miss so paint does not retry until the artwork bytes change.
                None => {
                    current.art_decode_failed = true;
                }
            }
        }

        if let Some(current) = &self.current {
            // Artwork is cached after first paint; paint just blends it.
            if let Some(blit) = &current.art_blit {
                draw_art_blit(hdc, blit, art, art_x, art_y);
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
                &self.fonts,
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
                &self.fonts,
                hdc,
                &current.track.title,
                &mut title_rect,
                (19.0 * scale) as i32,
                text_color,
                true,
                false,
            );

            let subtitle = if current.track.artist.trim().is_empty() {
                "Unknown Artist"
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
                &self.fonts,
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
                    &self.fonts,
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
                    &self.fonts,
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
                    &self.fonts,
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
                &self.fonts,
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
            &self.fonts,
            hdc,
            "SESSION HISTORY",
            &mut history_rect,
            (11.0 * scale) as i32,
            [0x99, 0x99, 0x99, 0xFF],
            true,
            false,
        );

        let pos_y = history_rect.bottom + (4.0 * scale) as i32;
        let pos_label = format!("Position: {}", position_label(&self.cfg()));
        let mut pos_rect = RECT {
            left: content_left + pad,
            top: pos_y,
            right: client_w - pad,
            bottom: pos_y + (16.0 * scale) as i32,
        };
        draw_string(
            &self.fonts,
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
            id: SettingId::Layout,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
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
            id: SettingId::SeparateCompact,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        items.push(SettingsItem::Row {
            id: SettingId::CompactPosition,
            rect: RECT {
                left,
                top: y,
                right,
                // Same two-line layout as Position (value/Reset + anchors).
                bottom: y + (70.0 * scale) as i32,
            },
        });
        y += (70.0 * scale) as i32 + gap;
        items.push(SettingsItem::Row {
            id: SettingId::DismissOnHover,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        items.push(SettingsItem::Row {
            id: SettingId::ExpandCompactOnHover,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        items.push(SettingsItem::Row {
            id: SettingId::AutoCompactApps,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        items.push(SettingsItem::Row {
            id: SettingId::Monitor,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
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
        y += row_h + gap;
        items.push(SettingsItem::Row {
            id: SettingId::OpenConfig,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        items
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_settings(
        &self,
        hdc: HDC,
        content_left: i32,
        client_w: i32,
        _client_h: i32,
        scale: f32,
        pad: i32,
        invalid: &RECT,
    ) {
        // Read the config once per paint instead of ~10 lock acquisitions,
        // and snapshot the hover/flag state so the row loop stays pure.
        let cfg = self.cfg();
        let accent = self.accent_color;
        let notifications_enabled = cfg.behavior.notifications_enabled;
        let settings_hover = self.settings_hover;
        let duration_ms = cfg.overlay.duration_ms;
        let start_on_login = cfg.behavior.start_on_login;
        let close_to_tray = cfg.behavior.close_to_tray;
        let media_sources = cfg.behavior.media_sources.join(", ");
        let custom_position = cfg.overlay.position_x.is_some();
        let position_label = position_label(&cfg);
        let layout_mode = cfg.overlay.layout;
        let compact_separate = cfg.overlay.compact_position_separate;
        let dismiss_on_hover = cfg.overlay.dismiss_on_hover;
        let expand_compact_on_hover = cfg.overlay.expand_compact_on_hover;
        let compact_position_label = compact_position_label(&cfg);
        let compact_custom = cfg.overlay.compact_effective().x.is_some();
        let auto_compact_sources = cfg.behavior.auto_compact_sources.join(", ");
        let display_count = enumerate_displays_cached().len();

        let mut hdr = RECT {
            left: content_left + pad,
            top: pad,
            right: client_w - pad,
            bottom: pad + (24.0 * scale) as i32,
        };
        if rects_intersect(invalid, &hdr) {
            draw_string(
                &self.fonts,
                hdc,
                "SETTINGS",
                &mut hdr,
                (13.0 * scale) as i32,
                accent,
                true,
                false,
            );
        }

        let items = self.settings_items(content_left, client_w, pad, scale);
        let brushes = SettingsBrushes {
            border: self.settings_border_brush,
            surface: self.settings_surface_brush,
            hover: self.settings_hover_brush,
            accent: self.accent_brush,
            accent_soft: self.settings_accent_soft_brush,
            near: self.settings_near_brush,
            adjust_hover: self.settings_adjust_hover_brush,
            small_fill: self.settings_small_fill_brush,
            small_hover: self.settings_small_hover_brush,
        };
        let mut row_index = 0usize;
        for item in &items {
            match item {
                SettingsItem::Header { text, rect } => {
                    if rects_intersect(invalid, rect) {
                        let mut hr = *rect;
                        draw_string(
                            &self.fonts,
                            hdc,
                            text,
                            &mut hr,
                            (9.0 * scale) as i32,
                            SETTINGS_FAINT,
                            true,
                            false,
                        );
                    }
                }
                SettingsItem::Row { id, rect } => {
                    // Row ordinals count rows only (headers are skipped), and
                    // must stay in sync with settings_hover_at even when the
                    // row is skipped for repainting.
                    let current_row = row_index;
                    row_index += 1;
                    if !rects_intersect(invalid, rect) {
                        continue;
                    }
                    let hovered_row = settings_hover.is_some_and(|(r, _)| r == current_row);
                    let split = row_split(rect, scale);
                    let label_rect = split.label;
                    let control_rect = split.control;

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
                        SettingId::Layout => ("Layout", String::new(), SETTINGS_MUTED),
                        SettingId::Position => ("Expanded Position", position_label.clone(), SETTINGS_MUTED),
                        SettingId::SeparateCompact => (
                            // Displayed polarity is inverted from the persisted
                            // `compact_position_separate` field so the label
                            // reads naturally: ON means the Compact pill
                            // follows the Expanded position (field `false`),
                            // OFF means independent (field `true`). Do NOT
                            // rename the TOML key to match the label — it is a
                            // documented persisted setting.
                            "Compact Position follows Expanded Position",
                            if compact_separate {
                                "OFF".to_string()
                            } else {
                                "ON".to_string()
                            },
                            if compact_separate { SETTINGS_FAINT } else { accent },
                        ),
                        SettingId::CompactPosition => {
                            ("Compact position", compact_position_label.clone(), SETTINGS_MUTED)
                        }
                        SettingId::DismissOnHover => (
                            "Dismiss on hover",
                            if dismiss_on_hover {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if dismiss_on_hover { accent } else { SETTINGS_FAINT },
                        ),
                        SettingId::ExpandCompactOnHover => (
                            "Expand compact on hover",
                            if expand_compact_on_hover {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if expand_compact_on_hover {
                                accent
                            } else {
                                SETTINGS_FAINT
                            },
                        ),
                        SettingId::AutoCompactApps => (
                            "Auto-compact apps",
                            // Fullscreen apps always compact under Auto (see
                            // `decide_layout`), so the coverage leads the
                            // value regardless of the selected apps.
                            if auto_compact_sources.is_empty() {
                                "Full screen apps".to_string()
                            } else {
                                format!("Full screen apps, {auto_compact_sources}")
                            },
                            SETTINGS_MUTED,
                        ),
                        SettingId::Monitor => ("Monitor", monitor_label(&cfg, display_count), SETTINGS_MUTED),
                        SettingId::AllowedApps => (
                            "Allowed apps",
                            if media_sources.is_empty() {
                                "All".to_string()
                            } else {
                                media_sources.clone()
                            },
                            SETTINGS_MUTED,
                        ),
                        SettingId::ShowSample => ("Show sample", String::new(), SETTINGS_MUTED),
                        SettingId::CopyLogs => ("Logs", String::new(), SETTINGS_MUTED),
                        SettingId::OpenConfig => ("Config", String::new(), SETTINGS_MUTED),
                    };
                    let mut lbl_rect = label_rect;
                    draw_string(
                        &self.fonts,
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
                        | SettingId::AllowedApps
                        | SettingId::SeparateCompact
                        | SettingId::DismissOnHover
                        | SettingId::ExpandCompactOnHover
                        | SettingId::AutoCompactApps
                        | SettingId::Monitor => {
                            let mut val_rect = control_rect;
                            draw_string(
                                &self.fonts,
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
                                let seg_hovered = settings_hover == Some((current_row, SettingSub::Seg(i)));
                                unsafe {
                                    let _ = FillRect(
                                        hdc,
                                        seg,
                                        if active || near { brushes.accent } else { brushes.border },
                                    );
                                }
                                let s_inner = RECT {
                                    left: seg.left + 1,
                                    top: seg.top + 1,
                                    right: seg.right - 1,
                                    bottom: seg.bottom - 1,
                                };
                                // Approximate preset: dimmer accent fill than
                                // the exact match, so "saved but not exact" is
                                // visible.
                                let fill = if active {
                                    brushes.accent_soft
                                } else if near {
                                    brushes.near
                                } else if seg_hovered {
                                    brushes.hover
                                } else {
                                    brushes.surface
                                };
                                unsafe {
                                    let _ = FillRect(hdc, &s_inner, fill);
                                }
                                let mut t = s_inner;
                                let tc = if active || near { SETTINGS_TEXT } else { SETTINGS_MUTED };
                                // The near-segment fill is a tint of the accent;
                                // for a light accent that tint can sit too close
                                // to white text. Clamp the label against the
                                // actual fill color — a no-op for accents dark
                                // enough to already pass AA.
                                let tc = if near {
                                    let fill = mix(accent, SETTINGS_SURFACE, SETTINGS_NEAR_WEIGHT);
                                    crate::overlay::ensure_contrast(tc, fill, crate::overlay::TEXT_CONTRAST_AA)
                                } else {
                                    tc
                                };
                                let label = if near {
                                    format!("≈{}s", values[i] / 1000)
                                } else {
                                    format!("{}s", values[i] / 1000)
                                };
                                draw_string(
                                    &self.fonts,
                                    hdc,
                                    &label,
                                    &mut t,
                                    (10.0 * scale) as i32,
                                    tc,
                                    active || near,
                                    true,
                                );
                            }
                        }
                        SettingId::Layout => {
                            // Three segments mirroring the LayoutMode variants;
                            // the same accent/hover treatment as Duration.
                            let segments = segment_rects(&control_rect, 3, (4.0 * scale) as i32);
                            let values = [LayoutMode::Expanded, LayoutMode::Compact, LayoutMode::Auto];
                            let labels = ["Expanded", "Compact", "Auto"];
                            for (i, seg) in segments.iter().enumerate() {
                                let active = layout_mode == values[i];
                                let seg_hovered = settings_hover == Some((current_row, SettingSub::Seg(i)));
                                unsafe {
                                    let _ = FillRect(hdc, seg, if active { brushes.accent } else { brushes.border });
                                }
                                let s_inner = RECT {
                                    left: seg.left + 1,
                                    top: seg.top + 1,
                                    right: seg.right - 1,
                                    bottom: seg.bottom - 1,
                                };
                                let fill = if active {
                                    brushes.accent_soft
                                } else if seg_hovered {
                                    brushes.hover
                                } else {
                                    brushes.surface
                                };
                                unsafe {
                                    let _ = FillRect(hdc, &s_inner, fill);
                                }
                                let mut t = s_inner;
                                let tc = if active { SETTINGS_TEXT } else { SETTINGS_MUTED };
                                draw_string(
                                    &self.fonts,
                                    hdc,
                                    labels[i],
                                    &mut t,
                                    (10.0 * scale) as i32,
                                    tc,
                                    active,
                                    true,
                                );
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
                                &self.fonts,
                                hdc,
                                &value_text,
                                &mut v,
                                (10.0 * scale) as i32,
                                SETTINGS_FAINT,
                                false,
                                false,
                            );
                            let reset_hovered = settings_hover == Some((current_row, SettingSub::Reset));
                            draw_small_button(
                                &self.fonts,
                                hdc,
                                &parts.reset,
                                "Reset",
                                accent,
                                reset_hovered,
                                scale,
                                brushes,
                            );

                            // Anchor segments + Adjust button row
                            for (i, seg) in parts.anchors.iter().enumerate() {
                                let active = active_anchor == Some(i);
                                let seg_hovered = settings_hover == Some((current_row, SettingSub::Anchor(i)));
                                draw_segment_button(
                                    &self.fonts,
                                    hdc,
                                    seg,
                                    ANCHOR_LABELS[i],
                                    active,
                                    seg_hovered,
                                    scale,
                                    brushes,
                                );
                            }
                            let adjust_hovered = settings_hover == Some((current_row, SettingSub::Adjust));
                            unsafe {
                                let _ = FillRect(
                                    hdc,
                                    &parts.adjust,
                                    if adjust_hovered {
                                        brushes.adjust_hover
                                    } else {
                                        brushes.accent_soft
                                    },
                                );
                            }
                            // Clamp the accent label against the Adjust button's
                            // soft fill so a light accent stays readable on hover
                            // (no-op for accents that already pass AA). The fill
                            // color mirrors `brushes.adjust_hover` / `accent_soft`
                            // so the guard targets the exact backdrop being drawn.
                            let fill_weight = if adjust_hovered {
                                SETTINGS_ADJUST_HOVER_WEIGHT
                            } else {
                                SETTINGS_ACCENT_SOFT_WEIGHT
                            };
                            let label_color = crate::overlay::ensure_contrast(
                                accent,
                                mix(accent, SETTINGS_SURFACE, fill_weight),
                                crate::overlay::TEXT_CONTRAST_AA,
                            );
                            let mut bt = parts.adjust;
                            draw_string(
                                &self.fonts,
                                hdc,
                                "Adjust…",
                                &mut bt,
                                (10.0 * scale) as i32,
                                label_color,
                                true,
                                true,
                            );
                        }
                        SettingId::CompactPosition => {
                            // The compact row mirrors the Position row, but on
                            // the Compact position fields and through
                            // `compact_effective`: the row always shows where
                            // the compact pill currently sits — the Expanded
                            // position while "follows Expanded" is ON — and it
                            // is always editable. Edits land in the raw
                            // `compact_*` fields and take visible effect once
                            // the follow toggle is OFF (independent) and the
                            // pill is actually compact; while following, the
                            // stored values are simply waiting (the
                            // copy-on-first-enable in `set_compact_separate`
                            // skips them, since they are no longer default).
                            let parts = position_parts(rect, scale);
                            let effective = cfg.overlay.compact_effective();
                            let active_anchor = if compact_custom {
                                None
                            } else {
                                Some(match (effective.vertical, effective.horizontal) {
                                    (VerticalPosition::Top, HorizontalPosition::Left) => 0,
                                    (VerticalPosition::Top, HorizontalPosition::Center) => 1,
                                    (VerticalPosition::Top, HorizontalPosition::Right) => 2,
                                    (VerticalPosition::Bottom, HorizontalPosition::Left) => 3,
                                    (VerticalPosition::Bottom, HorizontalPosition::Center) => 4,
                                    (VerticalPosition::Bottom, HorizontalPosition::Right) => 5,
                                })
                            };

                            let mut v = parts.value_row;
                            draw_string(
                                &self.fonts,
                                hdc,
                                &value_text,
                                &mut v,
                                (10.0 * scale) as i32,
                                SETTINGS_FAINT,
                                false,
                                false,
                            );
                            let reset_hovered = settings_hover == Some((current_row, SettingSub::Reset));
                            draw_small_button(
                                &self.fonts,
                                hdc,
                                &parts.reset,
                                "Reset",
                                accent,
                                reset_hovered,
                                scale,
                                brushes,
                            );
                            for (i, seg) in parts.anchors.iter().enumerate() {
                                let active = active_anchor == Some(i);
                                let seg_hovered = settings_hover == Some((current_row, SettingSub::Anchor(i)));
                                draw_segment_button(
                                    &self.fonts,
                                    hdc,
                                    seg,
                                    ANCHOR_LABELS[i],
                                    active,
                                    seg_hovered,
                                    scale,
                                    brushes,
                                );
                            }
                            let adjust_hovered = settings_hover == Some((current_row, SettingSub::Adjust));
                            unsafe {
                                let _ = FillRect(
                                    hdc,
                                    &parts.adjust,
                                    if adjust_hovered {
                                        brushes.adjust_hover
                                    } else {
                                        brushes.accent_soft
                                    },
                                );
                            }
                            // Clamp the accent label against the Adjust button's
                            // soft fill so a light accent stays readable on hover
                            // (no-op for accents that already pass AA). The fill
                            // color mirrors `brushes.adjust_hover` / `accent_soft`
                            // so the guard targets the exact backdrop being drawn.
                            let fill_weight = if adjust_hovered {
                                SETTINGS_ADJUST_HOVER_WEIGHT
                            } else {
                                SETTINGS_ACCENT_SOFT_WEIGHT
                            };
                            let label_color = crate::overlay::ensure_contrast(
                                accent,
                                mix(accent, SETTINGS_SURFACE, fill_weight),
                                crate::overlay::TEXT_CONTRAST_AA,
                            );
                            let mut bt = parts.adjust;
                            draw_string(
                                &self.fonts,
                                hdc,
                                "Adjust…",
                                &mut bt,
                                (10.0 * scale) as i32,
                                label_color,
                                true,
                                true,
                            );
                        }
                        SettingId::ShowSample => {
                            let btn_rect = RECT {
                                left: control_rect.left,
                                top: control_rect.top,
                                right: control_rect.right,
                                bottom: control_rect.bottom,
                            };
                            let hovered = self.settings_hover == Some((current_row, SettingSub::None));
                            draw_small_button(
                                &self.fonts,
                                hdc,
                                &btn_rect,
                                "Preview the notification",
                                accent,
                                hovered,
                                scale,
                                brushes,
                            );
                        }
                        SettingId::CopyLogs => {
                            // Diagnostics row hosts two side-by-side buttons:
                            // the left half opens the log in the default editor,
                            // the right half copies it to the clipboard. Each
                            // button highlights only when the cursor is over
                            // its own half.
                            let gap = (4.0 * scale) as i32;
                            let (open_rect, copy_rect) = halve(&control_rect, gap);
                            let hovered_open = self.settings_hover == Some((current_row, SettingSub::Open));
                            let hovered_copy = self.settings_hover == Some((current_row, SettingSub::Copy));
                            let copied = self
                                .logs_copied_at
                                .is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                            draw_small_button(
                                &self.fonts,
                                hdc,
                                &open_rect,
                                "Open logs",
                                accent,
                                hovered_open,
                                scale,
                                brushes,
                            );
                            draw_small_button(
                                &self.fonts,
                                hdc,
                                &copy_rect,
                                if copied { "Copied" } else { "Copy logs" },
                                accent,
                                hovered_copy,
                                scale,
                                brushes,
                            );
                        }
                        SettingId::OpenConfig => {
                            // Config row hosts two side-by-side buttons: the
                            // left half opens config.toml in the default editor
                            // (see open_config); the right half relaunches the
                            // app so the edited config.toml is reloaded (see
                            // reload_config). Each button highlights only when
                            // the cursor is over its own half.
                            let gap = (4.0 * scale) as i32;
                            let (open_rect, reload_rect) = halve(&control_rect, gap);
                            let hovered_open = self.settings_hover == Some((current_row, SettingSub::OpenConfig));
                            let hovered_reload = self.settings_hover == Some((current_row, SettingSub::ReloadConfig));
                            draw_small_button(
                                &self.fonts,
                                hdc,
                                &open_rect,
                                "Open config",
                                accent,
                                hovered_open,
                                scale,
                                brushes,
                            );
                            draw_small_button(
                                &self.fonts,
                                hdc,
                                &reload_rect,
                                "Reload config",
                                accent,
                                hovered_reload,
                                scale,
                                brushes,
                            );
                        }
                    }
                }
            }
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
                let control_rect = row_split(rect, scale).control;
                if *id == SettingId::Duration {
                    let segments = segment_rects(&control_rect, 4, (4.0 * scale) as i32);
                    let seg = segments.iter().position(|s| x >= s.left && x < s.right);
                    // A click or hover in the gap right of the last segment is
                    // not the first segment; the row stays highlighted.
                    return Some((row_index, seg.map_or(SettingSub::None, SettingSub::Seg)));
                }
                if *id == SettingId::Layout {
                    let segments = segment_rects(&control_rect, 3, (4.0 * scale) as i32);
                    let seg = segments.iter().position(|s| x >= s.left && x < s.right);
                    return Some((row_index, seg.map_or(SettingSub::None, SettingSub::Seg)));
                }
                if *id == SettingId::CopyLogs {
                    // Per-button hover for the two side-by-side buttons: the
                    // left half is "Open logs", the right half "Copy logs".
                    let gap = (4.0 * scale) as i32;
                    let (open_rect, copy_rect) = halve(&control_rect, gap);
                    if x >= open_rect.left && x < open_rect.right {
                        return Some((row_index, SettingSub::Open));
                    }
                    if x >= copy_rect.left && x < copy_rect.right {
                        return Some((row_index, SettingSub::Copy));
                    }
                    return Some((row_index, SettingSub::None));
                }
                if *id == SettingId::OpenConfig {
                    // Per-button hover for the two side-by-side buttons: the
                    // left half is "Open config", the right half "Reload config".
                    let gap = (4.0 * scale) as i32;
                    let (open_rect, reload_rect) = halve(&control_rect, gap);
                    if x >= open_rect.left && x < open_rect.right {
                        return Some((row_index, SettingSub::OpenConfig));
                    }
                    if x >= reload_rect.left && x < reload_rect.right {
                        return Some((row_index, SettingSub::ReloadConfig));
                    }
                    return Some((row_index, SettingSub::None));
                }
                if *id == SettingId::Position || *id == SettingId::CompactPosition {
                    let parts = position_parts(rect, scale);
                    if let Some(i) = parts
                        .anchors
                        .iter()
                        .position(|a| x >= a.left && x < a.right && y >= a.top && y < a.bottom)
                    {
                        return Some((row_index, SettingSub::Anchor(i)));
                    }
                    // The compact row's action buttons are always painted and
                    // clickable (its edits are stored even while "follows
                    // Expanded" is ON), so the hits below always register.
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

    /// Enumerates every keyboard-focusable control in the Settings pane, in the
    /// same top-to-bottom, left-to-right order `settings_hover_at` would visit
    /// them, each with the client coordinate a click on its center carries. The
    /// keyboard handler reuses the mouse click path by posting `WM_LBUTTONDOWN`
    /// at `(cx, cy)`, so this enumeration must stay in lockstep with the hover
    /// geometry in `settings_hover_at`.
    fn settings_focus_targets(&self, content_left: i32, client_w: i32, pad: i32, scale: f32) -> Vec<SettingsFocus> {
        let items = self.settings_items(content_left, client_w, pad, scale);
        let mut out = Vec::new();
        let gap = (4.0 * scale) as i32;
        let mut row_index = 0usize;
        for item in &items {
            if let SettingsItem::Row { id, rect } = item {
                let control_rect = row_split(rect, scale).control;
                match *id {
                    SettingId::Duration => {
                        for (i, s) in segment_rects(&control_rect, 4, gap).iter().enumerate() {
                            out.push(SettingsFocus {
                                row_index,
                                sub: SettingSub::Seg(i),
                                cx: (s.left + s.right) / 2,
                                cy: (s.top + s.bottom) / 2,
                            });
                        }
                    }
                    SettingId::Layout => {
                        for (i, s) in segment_rects(&control_rect, 3, gap).iter().enumerate() {
                            out.push(SettingsFocus {
                                row_index,
                                sub: SettingSub::Seg(i),
                                cx: (s.left + s.right) / 2,
                                cy: (s.top + s.bottom) / 2,
                            });
                        }
                    }
                    SettingId::CopyLogs => {
                        let (open_rect, copy_rect) = halve(&control_rect, gap);
                        out.push(SettingsFocus {
                            row_index,
                            sub: SettingSub::Open,
                            cx: (open_rect.left + open_rect.right) / 2,
                            cy: (open_rect.top + open_rect.bottom) / 2,
                        });
                        out.push(SettingsFocus {
                            row_index,
                            sub: SettingSub::Copy,
                            cx: (copy_rect.left + copy_rect.right) / 2,
                            cy: (copy_rect.top + copy_rect.bottom) / 2,
                        });
                    }
                    SettingId::OpenConfig => {
                        let (open_rect, reload_rect) = halve(&control_rect, gap);
                        out.push(SettingsFocus {
                            row_index,
                            sub: SettingSub::OpenConfig,
                            cx: (open_rect.left + open_rect.right) / 2,
                            cy: (open_rect.top + open_rect.bottom) / 2,
                        });
                        out.push(SettingsFocus {
                            row_index,
                            sub: SettingSub::ReloadConfig,
                            cx: (reload_rect.left + reload_rect.right) / 2,
                            cy: (reload_rect.top + reload_rect.bottom) / 2,
                        });
                    }
                    SettingId::Position | SettingId::CompactPosition => {
                        let parts = position_parts(rect, scale);
                        for (i, a) in parts.anchors.iter().enumerate() {
                            out.push(SettingsFocus {
                                row_index,
                                sub: SettingSub::Anchor(i),
                                cx: (a.left + a.right) / 2,
                                cy: (a.top + a.bottom) / 2,
                            });
                        }
                        out.push(SettingsFocus {
                            row_index,
                            sub: SettingSub::Reset,
                            cx: (parts.reset.left + parts.reset.right) / 2,
                            cy: (parts.reset.top + parts.reset.bottom) / 2,
                        });
                        out.push(SettingsFocus {
                            row_index,
                            sub: SettingSub::Adjust,
                            cx: (parts.adjust.left + parts.adjust.right) / 2,
                            cy: (parts.adjust.top + parts.adjust.bottom) / 2,
                        });
                    }
                    _ => {
                        out.push(SettingsFocus {
                            row_index,
                            sub: SettingSub::None,
                            cx: (control_rect.left + control_rect.right) / 2,
                            cy: (control_rect.top + control_rect.bottom) / 2,
                        });
                    }
                }
            }
            // Row index counts rows only, matching `settings_hover_at` and
            // `paint_settings`; headers are skipped here.
            if matches!(item, SettingsItem::Row { .. }) {
                row_index += 1;
            }
        }
        out
    }

    /// Moves the keyboard focus cursor onto `targets[idx]` and repaints the rows
    /// that changed. The cursor reuses `settings_hover`, so the existing hover
    /// highlight doubles as the focus ring — no separate paint path.
    fn focus_settings_target(&mut self, targets: &[SettingsFocus], idx: usize, client_w: i32) {
        let t = &targets[idx];
        let new_hover = Some((t.row_index, t.sub));
        if new_hover != self.settings_hover {
            let old = self.settings_hover;
            self.settings_hover = new_hover;
            self.invalidate_hover_rows(client_w, old, new_hover);
        }
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
                debug!("window close requested; hiding to the tray");
                let _ = ShowWindow(self.hwnd, SW_HIDE);
                // The tooltip sync timer only runs while the window is
                // visible; stop it so a tray-hidden window stops waking the
                // UI thread once a second.
                let _ = KillTimer(self.hwnd, TIMER_TOOLTIPS_ID);
                // Arm the idle release so a long tray-hidden window drops its
                // cached artwork blit (a few hundred KB); show_window() kills
                // the timer on restore.
                let _ = SetTimer(self.hwnd, IDLE_ART_TIMER_ID, IDLE_ART_RELEASE_MS, None);
            } else {
                debug!("window close requested; quitting (close to tray is off)");
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
        let accent_color = self.accent_color;

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
            draw_string(&self.fonts, hdc, text, &mut rect, font, color, bold, false);
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
        info!("main window destroyed; app quitting");
        remove_tray_icon(self.hwnd);
        if let Some(current) = &mut self.current {
            free_art_blit(&mut current.art_blit);
        }
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
                &self.settings_accent_soft_brush,
                &self.settings_near_brush,
                &self.settings_adjust_hover_brush,
                &self.settings_small_fill_brush,
                &self.settings_small_hover_brush,
            ] {
                if !brush.0.is_null() {
                    let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(brush.0));
                }
            }
        }
    }

    /// Marks the whole window for repaint on the next WM_PAINT. Deliberately
    /// cheap — it only invalidates the client area and does no work at call
    /// time (no DIB recreation, no font/brush setup), so settings-mutating
    /// click arms may call it freely after every mutation to repaint the new
    /// value and hover state in the same frame.
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

    /// Invalidates only the settings rows that changed hover state
    /// (pixel-identical to repainting the whole pane: every other row is
    /// unchanged). The row rects come from the same layout the hover
    /// hit-testing and the paint use.
    fn invalidate_hover_rows(&self, client_w: i32, old: Option<(usize, SettingSub)>, new: Option<(usize, SettingSub)>) {
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let sidebar_w = (SIDEBAR_W * scale).round() as i32;
        let pad = (PAD * scale) as i32;
        let items = self.settings_items(sidebar_w, client_w, pad, scale);
        for (row, _) in [old, new].into_iter().flatten() {
            // Row indices count rows only (headers are skipped), so walk the
            // row items to find the rect at that ordinal.
            let rect = items
                .iter()
                .filter_map(|i| match i {
                    SettingsItem::Row { rect, .. } => Some(rect),
                    _ => None,
                })
                .nth(row);
            if let Some(rect) = rect {
                self.invalidate_rect(rect);
            }
        }
    }

    fn show_window(&mut self) {
        unsafe {
            // A restored window invalidates the idle-release deadline; if the
            // blit was released, paint rebuilds it lazily.
            let _ = KillTimer(self.hwnd, IDLE_ART_TIMER_ID);
            let _ = ShowWindow(self.hwnd, SW_SHOWMAXIMIZED);
            // The foreground lock can reject SetForegroundWindow (the thread
            // never held the foreground); without a fallback the window would
            // open silently behind the current app. Bring it to the top of the
            // z-order without stealing focus instead.
            if !SetForegroundWindow(self.hwnd).as_bool() {
                let _ = SetWindowPos(
                    self.hwnd,
                    HWND_TOP,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        self.apply_pane();
        // The tooltip timer only runs while the window is visible (see
        // install_tooltip and on_close), so it must be (re)started here.
        unsafe {
            let _ = SetTimer(self.hwnd, TIMER_TOOLTIPS_ID, 1000, None);
        }
        // The window was hidden, so the timer skipped its syncs; rebuild
        // the tool definitions now so hover works immediately on restore.
        self.sync_tooltips();
        debug!("main window shown");
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
        info!("copied the live log to the clipboard");
        unsafe {
            let _ = SetTimer(self.hwnd, TIMER_LOGS_ID, 2000, None);
        }
        self.invalidate();
    }

    /// Opens `path` with the OS's default handler, from the UI thread, with a
    /// COM apartment active for the call: the shell's documentation requires
    /// COM initialized before `ShellExecuteW`, and the UI thread otherwise
    /// has none. `CoUninitialize` runs only when this call's own init
    /// succeeded, so a thread that already initialized COM (either apartment
    /// model) is left exactly as it was found. Returns the raw
    /// `ShellExecuteW` result; callers treat values <= 32 as failure.
    fn shell_open(&self, path: &std::path::Path) -> i32 {
        let file = wide(&path.to_string_lossy());
        let verb = wide("open");
        unsafe {
            let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
            let result = ShellExecuteW(
                self.hwnd,
                PCWSTR(verb.as_ptr()),
                PCWSTR(file.as_ptr()),
                None,
                None,
                SW_SHOW,
            );
            if initialized {
                CoUninitialize();
            }
            result.0 as i32
        }
    }

    /// Opens the current run's log file (`log-Live.log`) in the default
    /// application registered for its extension (i.e. the user's preferred
    /// text editor), mirroring `copy_logs`, which reads the same path. The OS
    /// picks the handler; `ShellExecuteW` returns a value <= 32 on failure,
    /// which is surfaced to the debug log rather than the screen.
    fn open_logs(&self) {
        let path = self.cfg().logs_dir().join("log-Live.log");
        let code = self.shell_open(&path);
        if code <= 32 {
            debug!("open logs: ShellExecuteW failed (code {code}) for {path:?}");
        } else {
            info!("opened the live log in the default editor");
        }
    }

    /// Opens `config.toml` in the default application registered for its
    /// extension (i.e. the user's preferred text editor), mirroring
    /// `open_logs`. The path is resolved via `Config::config_path`, the same
    /// path `save()` writes. The OS picks the handler; `ShellExecuteW` returns
    /// a value <= 32 on failure, which is surfaced to the debug log rather
    /// than the screen. Hand-edits apply on the next launch (no live reload).
    fn open_config(&self) {
        let path = match Config::config_path() {
            Ok(path) => path,
            Err(error) => {
                debug!("open config: resolving the config path failed: {error:#}");
                return;
            }
        };
        let code = self.shell_open(&path);
        if code <= 32 {
            debug!("open config: ShellExecuteW failed (code {code}) for {path:?}");
        } else {
            info!("opened config.toml in the default editor");
        }
    }

    /// Relaunches the app so the on-disk `config.toml` is reloaded. The new
    /// process re-acquires the single-instance mutex (released by
    /// `crate::relaunch_self` before it spawns) and loads config from disk;
    /// no app data is cleared, so any on-disk cache survives. See
    /// `crate::relaunch_self`.
    fn reload_config(&self) {
        crate::relaunch_self();
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
        info!("overlay position set: vertical={vertical:?} horizontal={horizontal:?}");
        let cfg = self.cfg();
        set_positions(
            self.overlay_hwnd,
            OverlayPos::from_config(&cfg),
            OverlayPos::compact_from_config(&cfg),
        );
    }

    /// Clears any custom X/Y override and returns to the default top-center anchor.
    fn reset_position(&mut self) {
        self.apply_anchor(VerticalPosition::Top, HorizontalPosition::Center);
        // If the position adjustor is open, move it back to the default spot too.
        crate::positioner::reset_position();
    }

    /// Pins the Compact layout to a vertical/horizontal anchor (independent
    /// position): clears any absolute override, persists, and nudges the live
    /// overlay into place.
    fn apply_compact_anchor(&mut self, vertical: VerticalPosition, horizontal: HorizontalPosition) {
        self.mutate_config(|cfg| {
            cfg.overlay.compact_vertical = vertical;
            cfg.overlay.compact_horizontal = horizontal;
            cfg.overlay.compact_position_x = None;
            cfg.overlay.compact_position_y = None;
        });
        info!("compact position set: vertical={vertical:?} horizontal={horizontal:?}");
        let cfg = self.cfg();
        set_positions(
            self.overlay_hwnd,
            OverlayPos::from_config(&cfg),
            OverlayPos::compact_from_config(&cfg),
        );
    }

    /// Clears the Compact layout's custom X/Y override and returns it to the
    /// default top-center anchor. The shared adjustor moves with it when open.
    fn reset_compact_position(&mut self) {
        self.apply_compact_anchor(VerticalPosition::Top, HorizontalPosition::Center);
        crate::positioner::reset_position();
    }

    /// Enables or disables the independent Compact position. The first enable
    /// initializes the Compact fields from the current Expanded position
    /// (Compact never starts from a hard-coded spot); later re-enables restore
    /// the previously customized values instead. The compact row is editable
    /// even while following, so values the user set there count as
    /// customization (`compact_is_default` returns false) and the first-enable
    /// copy is skipped, preserving them. See
    /// `config::OverlayConfig::compact_is_default`.
    fn set_compact_separate(&mut self, separate: bool) {
        // Same lock discipline as `mutate_config`: the write guard covers only
        // the in-memory mutation, and `save()` runs after the lock is released
        // so the disk write never stalls other config readers.
        let changed = {
            let mut cfg = self.config.write().unwrap_or_else(|poisoned| poisoned.into_inner());
            if separate {
                if cfg.overlay.compact_is_default() {
                    cfg.overlay.compact_vertical = cfg.overlay.vertical;
                    cfg.overlay.compact_horizontal = cfg.overlay.horizontal;
                    cfg.overlay.compact_margin = cfg.overlay.margin;
                    cfg.overlay.compact_position_x = cfg.overlay.position_x;
                    cfg.overlay.compact_position_y = cfg.overlay.position_y;
                    cfg.overlay.compact_monitor = cfg.overlay.monitor;
                }
                cfg.overlay.compact_position_separate = true;
            } else {
                cfg.overlay.compact_position_separate = false;
            }
            cfg.clone()
        };
        if let Err(error) = changed.save() {
            error!("saving config after the compact position follow toggle change failed: {error}");
        }
        // The log keeps the raw field value (greppable) and the displayed
        // polarity (ON = follows Expanded = field false).
        info!(
            "compact_position_separate set to {separate} (display: {})",
            if separate { "OFF" } else { "ON" }
        );
        crate::overlay::set_compact_separate(self.overlay_hwnd, separate);
    }

    /// Sets the target display mode, persists it, and nudges the live overlay.
    /// The overlay resolves the target against the current display layout
    /// itself on every placement, so no display handles are exchanged here.
    fn apply_monitor(&mut self, mode: MonitorMode) {
        self.mutate_config(|cfg| cfg.overlay.monitor = mode);
        info!("overlay monitor set: {mode:?}");
        let cfg = self.cfg();
        set_positions(
            self.overlay_hwnd,
            OverlayPos::from_config(&cfg),
            OverlayPos::compact_from_config(&cfg),
        );
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

/// Last time a persistent artwork-blit failure was logged, so a broken blit
/// cannot flood the log at repaint rate: one line per 30 s of continuous
/// failure instead.
static LAST_STRETCH_LOG: Mutex<Option<Instant>> = Mutex::new(None);

fn log_art_blit_failure() {
    let mut last = LAST_STRETCH_LOG.lock().unwrap_or_else(|p| p.into_inner());
    if last.is_none_or(|t| t.elapsed() >= Duration::from_secs(30)) {
        *last = Some(Instant::now());
        error!("artwork blit failed");
    }
}

/// Builds the cached AlphaBlend source for the decoded premultiplied artwork:
/// a memory DC with the pixels in a DIB section. Built once per decode (the
/// pixels never change between repaints), so the paint path performs no
/// per-paint GDI allocation. The buffer must be exactly `base² × 4` bytes.
fn build_art_blit(pm: &[u8], base: i32) -> Option<ArtBlit> {
    if base <= 0 || pm.len() != base as usize * base as usize * 4 {
        return None;
    }
    let mem = unsafe { CreateCompatibleDC(None) };
    if mem.0.is_null() {
        return None;
    }
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
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbm = unsafe { CreateDIBSection(mem, &info, DIB_RGB_COLORS, &mut bits, None, 0) };
    let Ok(hbm) = hbm else {
        unsafe {
            let _ = DeleteDC(mem);
        }
        return None;
    };
    if bits.is_null() {
        unsafe {
            let _ = DeleteObject(hbm);
            let _ = DeleteDC(mem);
        }
        return None;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(pm.as_ptr(), bits.cast::<u8>(), pm.len());
        let old = SelectObject(mem, hbm);
        Some(ArtBlit { mem, hbm, old, base })
    }
}

/// Frees a cached art-blit source. The caller keeps it alive across repaints
/// and drops it when the artwork changes or the window is destroyed.
fn free_art_blit(blit: &mut Option<ArtBlit>) {
    blit.take();
}

/// Blits the cached premultiplied artwork into the tile at `px` pixels.
/// `AlphaBlend` with `AC_SRC_ALPHA` composites the premultiplied pixels
/// source-over, so translucent artwork blends correctly over whatever is
/// beneath the tile; a plain SRCCOPY copy would paste the premultiplied
/// values verbatim, darkening translucent pixels.
fn draw_art_blit(hdc: HDC, blit: &ArtBlit, px: i32, x: i32, y: i32) {
    if px <= 0 {
        return;
    }
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let drawn = unsafe { AlphaBlend(hdc, x, y, px, px, blit.mem, 0, 0, blit.base, blit.base, blend) };
    if !drawn.as_bool() {
        log_art_blit_failure();
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
    Ok(crate::winutil::register_class_once(
        &REGISTERED,
        instance,
        class_name,
        Some(window_proc),
        || None,
        "the main window",
    )?)
}

static REGISTERED: OnceLock<()> = OnceLock::new();

/// The shared system application icon, loaded once and reused for every tray
/// (re)add. `LoadIconW(None, IDI_APPLICATION)` returns a system-owned handle
/// that must never be destroyed, so a single load serves the initial add and
/// every `TaskbarCreated` re-add after an Explorer restart. The handle is
/// stored as a raw value: `HICON` is not `Send`/`Sync`.
static TRAY_ICON: OnceLock<isize> = OnceLock::new();

fn tray_icon() -> HICON {
    let raw = *TRAY_ICON.get_or_init(|| {
        unsafe { LoadIconW(None, IDI_APPLICATION) }
            .expect("the system application icon should always load")
            .0 as isize
    });
    HICON(raw as *mut c_void)
}

fn install_tray_icon(hwnd: HWND) -> Result<()> {
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &tray_data(hwnd)) }.as_bool() {
        anyhow::bail!("Shell_NotifyIconW(NIM_ADD) failed");
    }
    Ok(())
}

/// The `TaskbarCreated` message, registered once. Windows broadcasts it after
/// Explorer (re)starts; the tray icon must be re-added then or it stays gone
/// until the app restarts.
fn taskbar_created_msg() -> u32 {
    *TASKBAR_CREATED_MSG.get_or_init(|| unsafe { RegisterWindowMessageW(PCWSTR(wide("TaskbarCreated").as_ptr())) })
}

static TASKBAR_CREATED_MSG: OnceLock<u32> = OnceLock::new();

fn remove_tray_icon(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
    }
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: tray_icon(),
        ..Default::default()
    };
    let tip = wide("WinGlance media overlay");
    let count = tip.len().min(data.szTip.len());
    data.szTip[..count].copy_from_slice(&tip[..count]);
    data
}

/// Shows a one-shot balloon note on the tray icon (NIF_INFO). Used for the
/// permanent SMTC worker failure: the note is visible even while the tracking
/// window is hidden (start in tray), unlike the history row alone.
fn show_tray_note(hwnd: HWND, title: &str, text: &str) {
    let mut data = tray_data(hwnd);
    data.uFlags |= NIF_INFO;
    data.dwInfoFlags = NIIF_ERROR;
    let title_wide = wide(title);
    let count = title_wide.len().min(data.szInfoTitle.len());
    data.szInfoTitle[..count].copy_from_slice(&title_wide[..count]);
    let text_wide = wide(text);
    let count = text_wide.len().min(data.szInfo.len());
    data.szInfo[..count].copy_from_slice(&text_wide[..count]);
    unsafe {
        if Shell_NotifyIconW(NIM_MODIFY, &data).0 == 0 {
            // The balloon is best-effort (the tray icon may be gone or the
            // notification area unavailable); the history row remains the
            // reliable fallback, and this log makes a failed update
            // diagnosable instead of silent.
            warn!("tray note failed (NIM_MODIFY)");
        }
    }
}

fn show_tray_menu(state: &mut MainWindowState) {
    debug!("tray menu opened");
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let open_flags = MF_STRING;
    let mut notify_flags = MF_STRING;
    if state.cfg().behavior.notifications_enabled {
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
        // Monitor submenu: which display the pill is placed on. The display
        // entries mirror the current enumeration (Display 1 is index 0), so
        // the checkmarks line up with what the overlay resolves at placement.
        let Ok(monitor_menu) = CreatePopupMenu() else {
            let _ = DestroyMenu(menu);
            return;
        };
        // Snapshot the configured mode (Copy type) so the read guard is
        // released before the TrackPopupMenu loop below, which calls
        // mutate_config on selection.
        let monitor_mode = state.cfg().overlay.monitor;
        let displays = enumerate_displays_cached();
        let monitor_flags = |mode: MonitorMode| {
            if monitor_mode == mode {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            }
        };
        let _ = AppendMenuW(
            monitor_menu,
            monitor_flags(MonitorMode::ActiveWindow),
            MENU_MONITOR_ACTIVE,
            PCWSTR(wide("Active window").as_ptr()),
        );
        let _ = AppendMenuW(
            monitor_menu,
            monitor_flags(MonitorMode::Primary),
            MENU_MONITOR_PRIMARY,
            PCWSTR(wide("Primary").as_ptr()),
        );
        if !displays.is_empty() {
            let _ = AppendMenuW(monitor_menu, MF_SEPARATOR, 0, PCWSTR::null());
            for (i, display) in displays.iter().enumerate() {
                let label = if display.primary {
                    format!("Display {} (primary)", i + 1)
                } else {
                    format!("Display {}", i + 1)
                };
                let label_wide = wide(&label);
                let flags = if monitor_mode == MonitorMode::Index(i as u32) {
                    MF_STRING | MF_CHECKED
                } else {
                    MF_STRING
                };
                let _ = AppendMenuW(
                    monitor_menu,
                    flags,
                    MENU_MONITOR_DISPLAY_BASE + i,
                    PCWSTR(label_wide.as_ptr()),
                );
            }
        }
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            monitor_menu.0 as usize,
            PCWSTR(wide("Monitor").as_ptr()),
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
        // Layout submenu: which pill layout is used. The current mode carries
        // the checkmark; Auto additionally resolves per-foreground at runtime.
        let Ok(layout_menu) = CreatePopupMenu() else {
            let _ = DestroyMenu(menu);
            return;
        };
        let layout_mode = state.cfg().overlay.layout;
        let layout_flags = |mode: LayoutMode| {
            if layout_mode == mode {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            }
        };
        let _ = AppendMenuW(
            layout_menu,
            layout_flags(LayoutMode::Expanded),
            MENU_LAYOUT_EXPANDED,
            PCWSTR(wide("Expanded").as_ptr()),
        );
        let _ = AppendMenuW(
            layout_menu,
            layout_flags(LayoutMode::Compact),
            MENU_LAYOUT_COMPACT,
            PCWSTR(wide("Compact").as_ptr()),
        );
        let _ = AppendMenuW(
            layout_menu,
            layout_flags(LayoutMode::Auto),
            MENU_LAYOUT_AUTO,
            PCWSTR(wide("Auto").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_POPUP, layout_menu.0 as usize, PCWSTR(wide("Layout").as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT_ID, PCWSTR(wide("Quit").as_ptr()));

        let mut point = POINT::default();
        if GetCursorPos(&mut point).is_ok() {
            // The owner must be the foreground window before TrackPopupMenu,
            // or the menu will not disappear when the user clicks away from
            // it or presses Esc (documented Shell_NotifyIcon requirement).
            let _ = SetForegroundWindow(state.hwnd);
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
                    let new_value = !state.cfg().behavior.notifications_enabled;
                    // Flip the overlay first; persist only when the toggle
                    // reaches it, so the config and the pill can never desync.
                    if PostMessageW(state.overlay_hwnd, TOGGLE_MSG, WPARAM(0), LPARAM(0)).is_err() {
                        error!("posting the notifications toggle to the overlay failed");
                    } else {
                        state.mutate_config(|cfg| cfg.behavior.notifications_enabled = new_value);
                    }
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
                    info!(
                        "close to tray {} (tray menu)",
                        if new_value { "enabled" } else { "disabled" }
                    );
                }
                MENU_QUIT_ID => {
                    info!("quit requested from the tray menu");
                    let _ = DestroyWindow(state.hwnd);
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
                MENU_MONITOR_ACTIVE => state.apply_monitor(MonitorMode::ActiveWindow),
                MENU_MONITOR_PRIMARY => state.apply_monitor(MonitorMode::Primary),
                _ if command >= MENU_MONITOR_DISPLAY_BASE && command < MENU_MONITOR_DISPLAY_BASE + displays.len() => {
                    state.apply_monitor(MonitorMode::Index((command - MENU_MONITOR_DISPLAY_BASE) as u32));
                }
                MENU_LAYOUT_EXPANDED => {
                    state.mutate_config(|cfg| cfg.overlay.layout = LayoutMode::Expanded);
                    set_layout(state.overlay_hwnd, LayoutMode::Expanded);
                }
                MENU_LAYOUT_COMPACT => {
                    state.mutate_config(|cfg| cfg.overlay.layout = LayoutMode::Compact);
                    set_layout(state.overlay_hwnd, LayoutMode::Compact);
                }
                MENU_LAYOUT_AUTO => {
                    state.mutate_config(|cfg| cfg.overlay.layout = LayoutMode::Auto);
                    set_layout(state.overlay_hwnd, LayoutMode::Auto);
                }
                _ => {}
            }
        }
        // After the modal menu loop, flush the queue with a no-op message so
        // the popup fully tears down when it was dismissed by clicking away.
        let _ = PostMessageW(state.hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Explorer (re)started and rebuilt the notification area: re-add the
    // tray icon, which Explorer's restart wiped.
    if message == taskbar_created_msg() {
        debug!("Explorer restarted the notification area; re-adding the tray icon");
        if let Err(error) = install_tray_icon(hwnd) {
            error!("re-adding the tray icon after an Explorer restart failed: {error}");
        }
        return LRESULT(0);
    }
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            let state = (*create).lpCreateParams as *mut MainWindowState;
            if !state.is_null() {
                set_window_state(hwnd, state);
                (*state).hwnd = hwnd;
                MAIN_STATE_CLAIMED.claim();
            }
        }
    }

    let state_ptr = window_state::<MainWindowState>(hwnd);
    match message {
        WM_CREATE => {
            if !state_ptr.is_null() {
                (*state_ptr).create_children();
            }
            // Color the window title bar with the effective accent so the
            // app reads as one theme. Applied here, after the frame is
            // realized, rather than right after CreateWindowExW. COLORREF is
            // 0x00BBGGRR, hence the swapped red/blue channels.
            let accent = if state_ptr.is_null() {
                [240, 110, 155, 255]
            } else {
                (*state_ptr).accent_color
            };
            let color = COLORREF(((accent[2] as u32) << 16) | ((accent[1] as u32) << 8) | accent[0] as u32);
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
        WM_DPICHANGED => {
            if !state_ptr.is_null() {
                (*state_ptr).on_dpi_changed((wparam.0 >> 16) as u32);
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
        WM_TIMER if wparam.0 == IDLE_ART_TIMER_ID => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                // show_window() kills this timer, so firing while the window
                // is visible would be a logic error elsewhere; the check
                // keeps the free from ever racing a paint.
                if !unsafe { IsWindowVisible(hwnd).as_bool() }
                    && let Some(current) = &mut state.current
                {
                    free_art_blit(&mut current.art_blit);
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
        WM_KEYDOWN => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let k = VIRTUAL_KEY(wparam.0 as u16);
                // From the Activity pane, Tab / Enter / Down / Right steps into
                // the Settings pane and focuses its first control.
                if state.active_pane != Pane::Settings {
                    if matches!(k, VK_TAB | VK_RETURN | VK_SPACE | VK_DOWN | VK_RIGHT) {
                        state.active_pane = Pane::Settings;
                        state.apply_pane();
                        let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
                        let (client_w, _) = client_size(hwnd);
                        let sidebar_w = (SIDEBAR_W * scale).round() as i32;
                        let pad = (PAD * scale) as i32;
                        let targets = state.settings_focus_targets(sidebar_w, client_w, pad, scale);
                        if let Some(first) = targets.first() {
                            let new_hover = Some((first.row_index, first.sub));
                            let old = state.settings_hover;
                            state.settings_hover = new_hover;
                            state.invalidate_hover_rows(client_w, old, new_hover);
                        }
                        state.invalidate();
                    }
                    return LRESULT(0);
                }
                // Settings pane: walk focusable controls and activate one.
                let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
                let (client_w, _) = client_size(hwnd);
                let sidebar_w = (SIDEBAR_W * scale).round() as i32;
                let pad = (PAD * scale) as i32;
                let targets = state.settings_focus_targets(sidebar_w, client_w, pad, scale);
                if targets.is_empty() {
                    return LRESULT(0);
                }
                let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 };
                let idx = state
                    .settings_hover
                    .and_then(|(r, s)| targets.iter().position(|t| t.row_index == r && t.sub == s))
                    .unwrap_or(0);
                match k {
                    VK_TAB => {
                        let next = if shift {
                            (idx + targets.len() - 1) % targets.len()
                        } else {
                            (idx + 1) % targets.len()
                        };
                        state.focus_settings_target(&targets, next, client_w);
                    }
                    VK_DOWN | VK_RIGHT => {
                        state.focus_settings_target(&targets, (idx + 1) % targets.len(), client_w);
                    }
                    VK_UP | VK_LEFT => {
                        state.focus_settings_target(&targets, (idx + targets.len() - 1) % targets.len(), client_w);
                    }
                    VK_RETURN | VK_SPACE => {
                        if let Some(t) = targets.get(idx) {
                            let lp = LPARAM(t.cx as isize | (t.cy as isize) << 16);
                            let _ = unsafe { PostMessageW(hwnd, WM_LBUTTONDOWN, WPARAM(0), lp) };
                        }
                    }
                    VK_ESCAPE => {
                        // Return to the Activity pane and clear the keyboard
                        // focus highlight so the next Tab starts fresh.
                        state.active_pane = Pane::Activity;
                        let old = state.settings_hover;
                        state.settings_hover = None;
                        state.apply_pane();
                        state.invalidate_hover_rows(client_w, old, None);
                        state.invalidate();
                    }
                    _ => {}
                }
                return LRESULT(0);
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
                    let previous = state.active_pane;
                    if y >= item0_y && y < item0_y + item_h {
                        state.active_pane = Pane::Activity;
                    } else if y >= item1_y && y < item1_y + item_h {
                        state.active_pane = Pane::Settings;
                    }
                    if previous != state.active_pane {
                        debug!("switched to the {:?} pane", state.active_pane);
                        // When entering Settings via mouse, set keyboard focus
                        // on the first control so the next Tab starts from a
                        // known position instead of a stale or absent highlight.
                        if state.active_pane == Pane::Settings {
                            let targets = state.settings_focus_targets(sidebar_w, client_w, pad, scale);
                            if let Some(first) = targets.first() {
                                let new_hover = Some((first.row_index, first.sub));
                                let old = state.settings_hover;
                                state.settings_hover = new_hover;
                                state.invalidate_hover_rows(client_w, old, new_hover);
                            }
                        }
                        // Clear the Settings focus highlight when leaving, so
                        // it does not persist behind the Activity pane.
                        if state.active_pane == Pane::Activity {
                            let old = state.settings_hover;
                            state.settings_hover = None;
                            state.invalidate_hover_rows(client_w, old, None);
                        }
                    }
                    state.apply_pane();
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
                            let control_rect = row_split(rect, scale).control;
                            match id {
                                SettingId::Notifications => {
                                    let new_value = !state.cfg().behavior.notifications_enabled;
                                    // Flip the overlay first; persist only when
                                    // the toggle reaches it, so the config and
                                    // the pill can never desync.
                                    if unsafe { PostMessageW(state.overlay_hwnd, TOGGLE_MSG, WPARAM(0), LPARAM(0)) }
                                        .is_err()
                                    {
                                        error!("posting the notifications toggle to the overlay failed");
                                    } else {
                                        state.mutate_config(|cfg| cfg.behavior.notifications_enabled = new_value);
                                    }
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
                                    info!("close to tray {}", if new_value { "enabled" } else { "disabled" });
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
                                SettingId::Layout => {
                                    let segments = segment_rects(&control_rect, 3, (4.0 * scale) as i32);
                                    let values = [LayoutMode::Expanded, LayoutMode::Compact, LayoutMode::Auto];
                                    if let Some((i, _)) =
                                        segments.iter().enumerate().find(|(_, s)| x >= s.left && x < s.right)
                                    {
                                        let mode = values[i];
                                        state.mutate_config(|cfg| cfg.overlay.layout = mode);
                                        set_layout(state.overlay_hwnd, mode);
                                        info!("layout mode set: {mode:?}");
                                        state.invalidate();
                                    }
                                }
                                SettingId::DismissOnHover => {
                                    let new_value = !state.cfg().overlay.dismiss_on_hover;
                                    state.mutate_config(|cfg| cfg.overlay.dismiss_on_hover = new_value);
                                    set_dismiss_on_hover(state.overlay_hwnd, new_value);
                                    info!("dismiss on hover set: {new_value}");
                                    state.invalidate();
                                }
                                SettingId::ExpandCompactOnHover => {
                                    let new_value = !state.cfg().overlay.expand_compact_on_hover;
                                    state.mutate_config(|cfg| cfg.overlay.expand_compact_on_hover = new_value);
                                    set_expand_compact_on_hover(state.overlay_hwnd, new_value);
                                    info!("expand compact on hover set: {new_value}");
                                    state.invalidate();
                                }
                                SettingId::SeparateCompact => {
                                    let new_value = !state.cfg().overlay.compact_position_separate;
                                    state.set_compact_separate(new_value);
                                    state.invalidate();
                                }
                                SettingId::CompactPosition => {
                                    // Always editable: clicks land in the raw
                                    // compact_* fields. While "follows
                                    // Expanded" is ON the row mirrors the
                                    // Expanded position and these edits are
                                    // stored, taking visible effect once the
                                    // follow toggle is OFF or the pill is
                                    // actually compact.
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
                                        state.apply_compact_anchor(v, h);
                                    } else if x >= parts.reset.left
                                        && x < parts.reset.right
                                        && y >= parts.reset.top
                                        && y < parts.reset.bottom
                                    {
                                        state.reset_compact_position();
                                    } else if x >= parts.adjust.left
                                        && x < parts.adjust.right
                                        && y >= parts.adjust.top
                                        && y < parts.adjust.bottom
                                    {
                                        let _ = crate::positioner::open_compact(hwnd, state.overlay_hwnd);
                                    }
                                    state.invalidate();
                                }
                                SettingId::AutoCompactApps => {
                                    if !process_picker::open(
                                        hwnd,
                                        &control_rect,
                                        &state.cfg().behavior.auto_compact_sources,
                                        state.auto_sources_result.clone(),
                                        AUTO_SOURCES_RESULT_MSG,
                                    ) {
                                        debug!("auto-compact sources picker failed to open");
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
                                    // Repaint in the same frame the anchor was
                                    // clicked: the row's highlighted anchor and
                                    // value changed, and — while the compact
                                    // pill follows — the compact row's mirror
                                    // must update without waiting for the next
                                    // mouse move.
                                    state.invalidate();
                                }
                                SettingId::ShowSample => {
                                    show_sample(state.overlay_hwnd);
                                }
                                SettingId::Monitor => {
                                    // One click steps to the next choice
                                    // (Active window → Primary → Display 1 →
                                    // … → back); the tray menu offers direct
                                    // selection.
                                    let displays = enumerate_displays_cached();
                                    let next = next_monitor_mode(state.cfg().overlay.monitor, displays.len());
                                    state.apply_monitor(next);
                                    state.invalidate();
                                }
                                SettingId::CopyLogs => {
                                    let gap = (4.0 * scale) as i32;
                                    let (open_rect, _copy_rect) = halve(&control_rect, gap);
                                    if x >= open_rect.left && x < open_rect.right {
                                        state.open_logs();
                                    } else {
                                        state.copy_logs();
                                    }
                                }
                                SettingId::OpenConfig => {
                                    let gap = (4.0 * scale) as i32;
                                    let (open_rect, _reload_rect) = halve(&control_rect, gap);
                                    if x >= open_rect.left && x < open_rect.right {
                                        state.open_config();
                                    } else {
                                        state.reload_config();
                                    }
                                }
                                SettingId::AllowedApps => {
                                    if !process_picker::open(
                                        hwnd,
                                        &control_rect,
                                        &state.cfg().behavior.media_sources,
                                        state.picker_result.clone(),
                                        PICKER_RESULT_MSG,
                                    ) {
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
                    let (client_w, _client_h) = client_size(hwnd);
                    let hover = if x < sidebar_w {
                        None
                    } else {
                        state.settings_hover_at(x, y, sidebar_w, client_w, pad, scale)
                    };
                    if hover != state.settings_hover {
                        let old = state.settings_hover;
                        state.settings_hover = hover;
                        state.invalidate_hover_rows(client_w, old, hover);
                    }
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                } else if let Some(old) = state.settings_hover {
                    state.settings_hover = None;
                    let (client_w, _client_h) = client_size(hwnd);
                    state.invalidate_hover_rows(client_w, Some(old), None);
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if let Some(old) = state.settings_hover {
                    state.settings_hover = None;
                    let (client_w, _client_h) = client_size(hwnd);
                    state.invalidate_hover_rows(client_w, Some(old), None);
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
                info!("overlay position set from the adjustor: ({x}, {y})");
                let cfg = state.cfg();
                set_positions(
                    state.overlay_hwnd,
                    OverlayPos::from_config(&cfg),
                    OverlayPos::compact_from_config(&cfg),
                );
            }
            LRESULT(0)
        }
        COMPACT_POSITION_MSG => {
            // Custom Compact position posted by the positioner (logical
            // pixels). Applied unconditionally: the compact row and tray
            // submenu are always editable, so a legitimately open adjustor
            // must persist even while "follows Expanded" is ON — the values
            // are stored and take effect once the follow toggle is OFF.
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let x = wparam.0 as i32;
                let y = lparam.0 as i32;
                state.mutate_config(|cfg| {
                    cfg.overlay.compact_position_x = Some(x);
                    cfg.overlay.compact_position_y = Some(y);
                });
                info!("compact position set from the adjustor: ({x}, {y})");
                let cfg = state.cfg();
                set_positions(
                    state.overlay_hwnd,
                    OverlayPos::from_config(&cfg),
                    OverlayPos::compact_from_config(&cfg),
                );
            }
            LRESULT(0)
        }
        PICKER_RESULT_MSG => {
            // The picker writes its confirmed patterns into the shared result
            // slot (never into the message itself) and posts this bare
            // message. Taking the slot is safe even when the message was
            // posted by a foreign process: it can only deliver values this
            // process's own picker produced.
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let patterns = state
                    .picker_result
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                if let Some(patterns) = patterns {
                    state.mutate_config(|cfg| cfg.behavior.media_sources = patterns);
                    state.invalidate();
                }
            }
            LRESULT(0)
        }
        AUTO_SOURCES_RESULT_MSG => {
            // Same contract as PICKER_RESULT_MSG, but for the Auto-compact
            // sources picker: the confirmed patterns land in the shared slot
            // and are taken here into `behavior.auto_compact_sources`.
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let patterns = state
                    .auto_sources_result
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                if let Some(patterns) = patterns {
                    state.mutate_config(|cfg| cfg.behavior.auto_compact_sources = patterns);
                    state.invalidate();
                }
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            // A display was added, removed, or reordered (or its resolution
            // changed). Invalidate the shared display cache so the next tray
            // menu or settings paint picks up the new layout.
            invalidate_display_cache();
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
            clear_window_state(hwnd);
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
            decoded_art: None,
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
    fn accent_from_art_uses_the_album_palette_and_falls_back() {
        let fallback = [240, 110, 155, 255];
        // No artwork: the configured accent stands in for both.
        assert_eq!(accent_from_art(None, fallback), (fallback, fallback));
        // Truncated/garbage bytes (not pixel-aligned): same fallback.
        assert_eq!(accent_from_art(Some(&[0, 0, 255]), fallback), (fallback, fallback));
        // A solid white cover (premultiplied BGRA) yields a palette: the
        // primary and secondary leave the pink fallback behind, and the
        // monochrome palette keeps both equal.
        let white: Vec<u8> = vec![255u8; 8 * 8 * 4];
        let (primary, secondary) = accent_from_art(Some(&white), fallback);
        assert_ne!(primary, fallback, "a cover must recolor the accent");
        assert_ne!(secondary, fallback, "a cover must recolor the secondary");
        assert_eq!(primary, secondary, "monochrome art keeps primary == secondary");
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
            art_blit: None,
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
