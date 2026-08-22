use crate::autostart;
use crate::config::{Config, HorizontalPosition, LayoutMode, MonitorMode, SaveOutcome, VerticalPosition};
use crate::events::{
    COMPACT_POSITION_MSG, MEDIA_EVENT_MSG, MediaEvent, POSITION_MSG, PlaybackState, TOGGLE_MSG, TrackInfo,
    media_event_into_owned,
};
use crate::gdi::{FontProvider, draw_string};
use crate::overlay::{
    EventQueue, OverlayPos, enumerate_displays_cached, invalidate_display_cache, set_dismiss_on_hover, set_duration,
    set_expand_compact_on_hover, set_fade_persistent_pill, set_hide_for_auto_compact_sources, set_layout,
    set_pinned_source, set_positions, show_sample,
};
use crate::process_picker;
use crate::process_picker::{AUTO_SOURCES_RESULT_MSG, PICKER_RESULT_MSG, PINNED_SOURCE_RESULT_MSG};
use crate::smtc::{ControlCommand, ControlMailbox, Signal};
use crate::winapi::{
    create_dib_section, create_font, delete_object, global_free, invalidate_rect, is_window, kill_timer, post_message,
    select_object, send_message, set_clipboard_data, set_timer, set_window_pos, shell_execute, track_popup_menu,
};
use crate::winutil::{StateClaim, release_window_state, set_window_state, wide, window_state};
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use log::{debug, error, info, warn};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CAPTION_COLOR, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, AlphaBlend, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, BeginPaint, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOWFRAME, COLOR_WINDOWTEXT,
    CreateCompatibleDC, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DeleteDC, EndPaint,
    FF_DONTCARE, FillRect, FrameRect, GetStockObject, GetSysColor, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT, SYS_COLOR_INDEX, SetBkColor, SetTextColor,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Threading::{CreateEventW, GetCurrentThreadId, SetEvent, WaitForSingleObject};
use windows::Win32::UI::Accessibility::{
    ToggleState_Off, ToggleState_On, UIA_AutomationFocusChangedEventId, UIA_ButtonControlTypeId,
    UIA_CheckBoxControlTypeId, UIA_NamePropertyId, UIA_ToggleToggleStatePropertyId, UiaRaiseAutomationEvent,
    UiaRaiseAutomationPropertyChangedEvent, UiaReturnRawElementProvider, UiaRootObjectId,
};
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, NMHDR, NMTTDISPINFOW, ODS_SELECTED, SetScrollInfo, ShowScrollBar, TOOLTIPS_CLASSW, TTF_SUBCLASS,
    TTM_ADDTOOLW, TTM_DELTOOLW, TTM_SETMAXTIPWIDTH, TTM_SETTOOLINFOW, TTN_GETDISPINFOW, TTS_ALWAYSTIP, TTS_NOPREFIX,
    WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VIRTUAL_KEY, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME,
    VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFY_ICON_INFOTIP_FLAGS, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CreatePopupMenu, DefWindowProcW, DestroyMenu, DestroyWindow, GetClientRect,
    GetCursorPos, HICON, HMENU, HWND_TOP, IDI_APPLICATION, IsWindowVisible, LB_ADDSTRING, LB_DELETESTRING, LB_GETCOUNT,
    LB_GETITEMHEIGHT, LB_GETITEMRECT, LB_GETTOPINDEX, LB_INSERTSTRING, LB_SETITEMHEIGHT, LB_SETTOPINDEX,
    LBS_HASSTRINGS, LBS_NOINTEGRALHEIGHT, LBS_OWNERDRAWFIXED, LoadIconW, MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, PostQuitMessage, RegisterWindowMessageW, SB_BOTTOM, SB_LINEDOWN, SB_LINEUP, SB_PAGEDOWN,
    SB_PAGEUP, SB_THUMBPOSITION, SB_THUMBTRACK, SB_TOP, SB_VERT, SCROLLBAR_COMMAND, SCROLLINFO, SIF_PAGE, SIF_POS,
    SIF_RANGE, SW_HIDE, SW_SHOW, SW_SHOWMAXIMIZED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SetForegroundWindow, ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_STYLE, WM_APP, WM_CLOSE,
    WM_CREATE, WM_CTLCOLORLISTBOX, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_DRAWITEM, WM_ENDSESSION,
    WM_GETOBJECT, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY,
    WM_NOTIFY, WM_NULL, WM_PAINT, WM_RBUTTONUP, WM_SETFONT, WM_SETTINGCHANGE, WM_SIZE, WM_TIMER, WM_VSCROLL, WS_CHILD,
    WS_CLIPCHILDREN, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{BSTR, PCWSTR, PWSTR};

pub(crate) const WM_TRAY: u32 = WM_APP + 2;

/// UIA activation: a Settings control's stable runtime id, posted by the
/// provider's Invoke/Toggle. The id is re-resolved against the live layout in
/// the handler, so a scrolled or rebuilt pane can never act on a different
/// control than the one invoked.
pub(crate) const WM_SETTINGS_ACTIVATE_MSG: u32 = WM_APP + 13;
/// UIA provider threads post this to ask the UI thread (the only thread that
/// may touch the window-state box) to rebuild the Settings snapshot. The
/// requesting thread waits on `SETTINGS_SNAPSHOT_EVENT` for the answer.
pub(crate) const WM_SETTINGS_SNAPSHOT_MSG: u32 = WM_APP + 10;
/// A UIA `SetFocus` handoff: the focus state lives in the window-state box,
/// so provider threads post this instead of mutating it themselves.
pub(crate) const WM_SETTINGS_FOCUS_MSG: u32 = WM_APP + 8;
const TRAY_ID: u32 = 1;
const MENU_OPEN_ID: usize = 1001;
const MENU_PREVIEW_NOTIFY_ID: usize = 1029;
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
/// display `i` gets `MENU_MONITOR_DISPLAY_BASE + i`. The base sits in its
/// own id namespace (every fixed command id stays below it), so a display
/// entry can never collide with — and be mis-dispatched as — a fixed
/// command, no matter how many displays are attached. The collision test
/// below pins this.
const MENU_MONITOR_DISPLAY_BASE: usize = 1100;
/// Layout-mode entries of the tray "Layout" submenu.
const MENU_LAYOUT_EXPANDED: usize = 1024;
const MENU_LAYOUT_COMPACT: usize = 1025;
const MENU_LAYOUT_AUTO: usize = 1026;
const MENU_LAYOUT_PERSISTENT_COMPACT: usize = 1028;
/// Duration submenu: shown only when the current duration is not a preset.
const MENU_DURATION_CUSTOM: usize = 1027;
const LISTBOX_ID: usize = 2;
/// History rows are kept in the heap (as entries) and duplicated in the
/// listbox as UTF-16 row strings, so the cap directly sizes the app's
/// baseline memory (~1 KB per row across both copies).
const HISTORY_CAP: usize = 400;
/// Timer used to clear the "Copied" feedback on the Copy logs button.
const TIMER_LOGS_ID: usize = 101;
/// Timer used to clear the "Opened" feedback on the Open logs/Open config buttons.
const TIMER_OPENED_ID: usize = 104;
/// Timer used to keep the native history tooltip's item rects in sync (scroll).
/// Timer that retries a failed initial tray add: Explorer may not have built
/// the notification area yet at logon, which makes the first `NIM_ADD` fail.
const TRAY_RETRY_TIMER_ID: usize = 105;
/// Retry cadence and budget for the tray add: five attempts two seconds
/// apart (~10 s), then one bounded error instead of failing startup.
const TRAY_RETRY_INTERVAL_MS: u32 = 2000;
const TRAY_RETRY_MAX_ATTEMPTS: u32 = 5;
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

/// Persistent settings-pane status about config persistence. Painted as a
/// warning banner with the Open config / Restart app actions directly below
/// it, every repaint, until the situation clears (a later successful save
/// clears every variant; `PersistenceDisabled` lasts the whole run — the
/// startup file was invalid and is deliberately left untouched).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConfigStatus {
    /// The on-disk file was edited outside the app after load: the session's
    /// settings changes are kept in memory only, nothing was written.
    Conflict,
    /// The startup file was invalid, unreadable, or oversized: nothing is
    /// ever persisted this run.
    PersistenceDisabled,
    /// A generic save failure (disk full, access denied, …) after the change
    /// was applied in memory: the disk still holds the previous value and the
    /// UI must not look successful. The detailed OS error stays in the log;
    /// the banner carries only the bounded category.
    SaveFailed(SaveFailKind),
}

/// The bounded, user-facing category of a generic config-save failure. Kept
/// tiny and `Copy` so the persistent status (and the layout key it feeds)
/// never grows unbounded state; the exact OS error text is logged, not shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SaveFailKind {
    /// `ERROR_DISK_FULL` / `ERROR_HANDLE_DISK_FULL`: the volume ran out of space.
    DiskFull,
    /// `ERROR_ACCESS_DENIED` / sharing or lock violation: the file or its
    /// parent rejects the write.
    Permission,
    /// Anything else (device errors, injected test failures, …).
    Other,
}

impl SaveFailKind {
    /// Classifies an `anyhow` save error into the bounded category. The
    /// chain is searched for a `std::io::Error` (the error type
    /// `write_temp_and_rename` propagates); anything else is `Other`. The
    /// full chain remains visible in the log line the caller writes.
    fn from_error(error: &anyhow::Error) -> Self {
        const ERROR_DISK_FULL: i32 = 112;
        const ERROR_HANDLE_DISK_FULL: i32 = 39;
        const ERROR_ACCESS_DENIED: i32 = 5;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        const ERROR_LOCK_VIOLATION: i32 = 33;
        match error.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error()) {
            Some(ERROR_DISK_FULL | ERROR_HANDLE_DISK_FULL) => Self::DiskFull,
            Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION) => Self::Permission,
            _ => Self::Other,
        }
    }
}

/// Settings rows: section headers with label-left / control-right card rows.
#[derive(Clone, Copy, PartialEq)]
enum SettingId {
    Notifications,
    Duration,
    RespectSystemDuration,
    StartOnLogin,
    CloseToTray,
    AllowedApps,
    Layout,
    Position,
    SeparateCompact,
    DismissOnHover,
    ExpandCompactOnHover,
    HideForAutoCompactSources,
    FadePersistentPill,
    PinnedSource,
    CompactPosition,
    AutoCompactApps,
    Monitor,
    ShowSample,
    CopyLogs,
    OpenConfig,
}

#[derive(Clone)]
enum SettingsItem {
    Header {
        text: &'static str,
        rect: RECT,
    },
    Row {
        id: SettingId,
        rect: RECT,
    },
    /// Non-interactive warning banner above the config/restart actions; it is
    /// never a row (hover and focus enumerations skip it like headers).
    Banner {
        text: &'static str,
        rect: RECT,
    },
}

/// The laid-out settings items plus the natural (unscrolled) document bottom in
/// client pixels. `content_extent` lets the scroll range be derived from a layout
/// that ignores the live `settings_scroll_y`, so the same geometry powers paint,
/// hit-test, focus, scroll and the UIA provider.
#[derive(Clone)]
struct SettingsLayout {
    items: Vec<SettingsItem>,
    content_extent: i32,
}

/// Memoization key for the laid-out Settings pane: every input
/// `build_settings_layout` reads that is not a constant — client width, scroll
/// offset, DPI-derived geometry, and whether the persistence banner is shown.
/// The key covers all inputs, so the cache can never serve a stale layout.
#[derive(Clone, Copy, PartialEq)]
struct SettingsLayoutKey {
    content_left: i32,
    client_w: i32,
    pad: i32,
    scale: u32,
    scroll_y: i32,
    status: Option<ConfigStatus>,
}

/// A keyboard-focusable Settings-pane control. `rect` is the exact interaction
/// rectangle (client coordinates) of the control — the same geometry the mouse
/// hit-test and the paint derive from `segment_rects`/`halve`/`position_parts`/
/// `row_split`, so keyboard focus, the focus outline and the UIA bounds all
/// agree with what is clickable. `cx`/`cy` is that rect's center; the keyboard
/// handler activates a control by posting a synthetic `WM_LBUTTONDOWN` there,
/// so it reuses the existing mouse click path verbatim. The list order
/// (top-to-bottom, left-to-right within a row) is what Tab/arrows walk.
struct SettingsFocus {
    row_index: usize,
    sub: SettingSub,
    rect: RECT,
    cx: i32,
    cy: i32,
}

const SETTINGS_SURFACE: [u8; 4] = [0x1B, 0x1B, 0x1B, 0xFF];
const SETTINGS_BORDER: [u8; 4] = [0x2D, 0x2D, 0x2D, 0xFF];
const SETTINGS_HOVER: [u8; 4] = [0x24, 0x24, 0x24, 0xFF];
const SETTINGS_TEXT: [u8; 4] = [0xF0, 0xF0, 0xF0, 0xFF];
const SETTINGS_MUTED: [u8; 4] = [0xC8, 0xC8, 0xC8, 0xFF];
const SETTINGS_FAINT: [u8; 4] = [0x7A, 0x7A, 0x7A, 0xFF];
/// Warm amber for the config-persistence warning banner.
const SETTINGS_WARN: [u8; 4] = [0xFF, 0xB7, 0x4D, 0xFF];

/// An sRGB system color as an opaque RGBA array.
fn sys_color_rgba(index: SYS_COLOR_INDEX) -> [u8; 4] {
    let color = unsafe { GetSysColor(index) };
    [
        (color & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        0xFF,
    ]
}

/// The Settings pane's effective color set. Normally the
/// fixed dark theme, except the faint state text is lifted through the
/// shared contrast helper to the 4.5:1 AA floor against the surface it is
/// actually painted on (the raw `SETTINGS_FAINT` manages only ~4.0:1). Under
/// a high-contrast theme the system colors take over: window surface, window
/// text, gray text, and the window frame as border, with the same contrast
/// helper re-checking every derived value against the system surface.
#[derive(Clone, Copy)]
struct SettingsColors {
    surface: [u8; 4],
    border: [u8; 4],
    hover: [u8; 4],
    text: [u8; 4],
    muted: [u8; 4],
    faint: [u8; 4],
    warn: [u8; 4],
    /// The keyboard-focus outline color (checked at 3:1 against both the
    /// surface and the hover fill).
    focus: [u8; 4],
    /// The accent for emphasized text (ON values, the SETTINGS header, small
    /// button labels). Art-derived normally; under a high-contrast theme it
    /// is derived from the system highlight color and lifted to the AA floor
    /// against the system surface, so emphasized labels stay readable on
    /// light and dark system surfaces alike.
    accent: [u8; 4],
    /// The fill color of active segments/buttons: the art accent normally,
    /// the raw system highlight under a high-contrast theme (paired with
    /// `accent_fill_text`, whose contrast Windows itself guarantees against
    /// it).
    accent_fill: [u8; 4],
    /// The label color drawn on an `accent_fill` backdrop: `COLOR_HIGHLIGHTTEXT`
    /// under a high-contrast theme (the standard HC button pairing), unused by
    /// the normal theme (which draws active labels in the surface text color).
    accent_fill_text: [u8; 4],
    /// Whether this palette is the high-contrast system-derived set (vs. the
    /// fixed dark theme). The paint path picks the system accent only when
    /// true — the art-derived accent stays in charge of the normal theme.
    high_contrast: bool,
}

fn settings_colors_for(prefs: &crate::winutil::SystemPreferences) -> SettingsColors {
    if prefs.high_contrast {
        let surface = crate::winutil::system_window_color();
        let text = sys_color_rgba(COLOR_WINDOWTEXT);
        SettingsColors {
            border: sys_color_rgba(COLOR_WINDOWFRAME),
            hover: mix(sys_color_rgba(COLOR_HIGHLIGHT), surface, 0.25),
            faint: crate::overlay::ensure_contrast(
                sys_color_rgba(COLOR_GRAYTEXT),
                surface,
                crate::overlay::TEXT_CONTRAST_AA,
            ),
            muted: crate::overlay::ensure_contrast(text, surface, crate::overlay::TEXT_CONTRAST_AA),
            warn: crate::overlay::ensure_contrast(SETTINGS_WARN, surface, crate::overlay::TEXT_CONTRAST_AA),
            focus: crate::overlay::ensure_contrast(sys_color_rgba(COLOR_HIGHLIGHT), surface, 3.0),
            accent: crate::overlay::ensure_contrast(
                sys_color_rgba(COLOR_HIGHLIGHT),
                surface,
                crate::overlay::TEXT_CONTRAST_AA,
            ),
            // The standard HC button pairing: the raw highlight fill and the
            // system's own highlight-text color, whose contrast Windows
            // guarantees against the highlight.
            accent_fill: sys_color_rgba(COLOR_HIGHLIGHT),
            accent_fill_text: sys_color_rgba(COLOR_HIGHLIGHTTEXT),
            high_contrast: true,
            surface,
            text,
        }
    } else {
        SettingsColors {
            surface: SETTINGS_SURFACE,
            border: SETTINGS_BORDER,
            hover: SETTINGS_HOVER,
            text: SETTINGS_TEXT,
            muted: SETTINGS_MUTED,
            faint: crate::overlay::ensure_contrast(SETTINGS_FAINT, SETTINGS_SURFACE, crate::overlay::TEXT_CONTRAST_AA),
            warn: SETTINGS_WARN,
            focus: crate::overlay::ensure_contrast(SETTINGS_TEXT, SETTINGS_SURFACE, 3.0),
            // Unused by the paint path (which keeps the art-derived accent
            // for the normal theme); sane placeholders.
            accent: SETTINGS_TEXT,
            accent_fill: SETTINGS_TEXT,
            accent_fill_text: SETTINGS_TEXT,
            high_contrast: false,
        }
    }
}

/// Banner text for a `ConfigStatus`; every variant describes the current
/// persistence state and points at the Open config / Restart app actions
/// directly below the banner. A save failure names the bounded category so
/// the user can act on it (free space, fix permissions) while the detailed
/// OS error stays in the log.
fn banner_text(status: ConfigStatus) -> &'static str {
    match status {
        ConfigStatus::Conflict => {
            "config.toml was edited on disk; changes apply in memory only — open it or restart to pick up the other edits"
        }
        ConfigStatus::PersistenceDisabled => {
            "config.toml could not be saved; settings apply in memory only for this run"
        }
        ConfigStatus::SaveFailed(kind) => match kind {
            SaveFailKind::DiskFull => {
                "config.toml could not be saved — the disk is full; changes apply in memory only — free space and change a setting to retry, or restart"
            }
            SaveFailKind::Permission => {
                "config.toml could not be saved — access denied; changes apply in memory only — fix the file permissions and change a setting to retry, or restart"
            }
            SaveFailKind::Other => {
                "config.toml could not be saved; changes apply in memory only — change a setting to retry, or restart"
            }
        },
    }
}

/// Mix weights (toward `SETTINGS_SURFACE`) for the accent soft fills. Kept
/// as named constants so the brush rebuild and the render-time contrast guard
/// below stay in lockstep — a drift between the two would silently recompute
/// the wrong backdrop for the label guard.
const SETTINGS_ACCENT_SOFT_WEIGHT: f32 = 0.28;
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
/// text) and secondary. The worker's identity-stable palette (`art_palette`,
/// derived once from the fixed-size decode) is preferred when present, so a
/// re-encoded thumbnail — different bytes, same cover — cannot shift the
/// window accent either; otherwise the palette is derived from
/// `decoded_art` (the worker's premultiplied-BGRA decode, the same buffer
/// the pill palettizes from). When neither yields a palette, both fall back
/// to the configured accent — the default pink theme.
fn accent_from_art(
    decoded_art: Option<&[u8]>,
    art_palette: Option<crate::palette::Palette>,
    fallback: [u8; 4],
) -> ([u8; 4], [u8; 4]) {
    let Some(palette) = art_palette.or_else(|| {
        decoded_art
            .and_then(crate::overlay::pm_bgra_to_rgba)
            .and_then(|rgba| crate::palette::palette_from_rgba(&rgba))
    }) else {
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
pub(crate) enum SettingSub {
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
    /// The right half of the Config row ("Restart app" button).
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

/// Shifts a rect vertically by `dy` client pixels, applying the shared Settings
/// scroll offset. Width and horizontal position are unchanged.
fn offset_rect(rect: RECT, dy: i32) -> RECT {
    RECT {
        top: rect.top + dy,
        bottom: rect.bottom + dy,
        ..rect
    }
}

/// Clamps a Settings-pane scroll offset to the reachable range. The largest
/// offset keeps the document bottom flush with the viewport bottom; when the
/// content fits, the only valid offset is zero. Pure and testable.
fn clamp_settings_scroll(scroll_y: i32, content_extent: i32, client_h: i32) -> i32 {
    let max = (content_extent - client_h).max(0);
    scroll_y.clamp(0, max)
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

/// Formats a pill duration for user-facing text: whole seconds render
/// without a fraction ("5s"), anything else keeps its exact sub-second value
/// ("1.5s"), so a custom 500 ms or 1500 ms duration never reads as "0s"/"1s".
fn format_duration_label(duration_ms: u64) -> String {
    if duration_ms.is_multiple_of(1000) {
        format!("{}s", duration_ms / 1000)
    } else {
        format!("{}s", duration_ms as f64 / 1000.0)
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
    colors: SettingsColors,
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
    // Active labels sit on the accent fill: the system highlight-text pairing
    // under high contrast (guaranteed against the highlight), the surface text
    // otherwise. Inactive labels use the palette's muted color, which is
    // AA-lifted against the system surface under high contrast.
    let tc = if active {
        if colors.high_contrast {
            colors.accent_fill_text
        } else {
            colors.text
        }
    } else {
        colors.muted
    };
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
    /// Cached GDI source for the source-app icon. Built lazily from
    /// `track.app_icon` (the worker's premultiplied BGRA at 24×24) on first
    /// paint. The icon data is already in memory (Arc-shared); this blit
    /// adds ~2.4 KB (24×24×4 pixel data + GDI handles).
    icon_blit: Option<ArtBlit>,
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
            let _ = select_object(self.mem, self.old);
            let _ = delete_object(self.hbm);
            let _ = DeleteDC(self.mem);
        }
    }
}

/// An owned GDI brush handle deleted exactly once on drop. Wraps the main
/// window's long-lived brushes so ownership cannot drift between creation and
/// teardown — a brush field added without extending WM_DESTROY previously
/// leaked its handle.
struct OwnedBrush(HBRUSH);

impl OwnedBrush {
    fn null() -> Self {
        Self(HBRUSH::default())
    }

    fn new(brush: HBRUSH) -> Self {
        Self(brush)
    }

    fn get(&self) -> HBRUSH {
        self.0
    }
}

impl Drop for OwnedBrush {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                let _ = delete_object(self.0);
            }
        }
    }
}

/// `OwnedBrush` for the history listbox font (same ownership contract).
struct OwnedFont(HFONT);

impl OwnedFont {
    fn null() -> Self {
        Self(HFONT::default())
    }

    fn new(font: HFONT) -> Self {
        Self(font)
    }

    fn get(&self) -> HFONT {
        self.0
    }
}

impl Drop for OwnedFont {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                let _ = delete_object(self.0);
            }
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
    listbox_font: OwnedFont,
    fonts: FontProvider,
    gray_brush: OwnedBrush,
    accent_brush: OwnedBrush,
    black_brush: OwnedBrush,
    sidebar_bg_brush: OwnedBrush,
    sidebar_highlight_brush: OwnedBrush,
    settings_border_brush: OwnedBrush,
    settings_surface_brush: OwnedBrush,
    settings_hover_brush: OwnedBrush,
    /// The Settings pane's effective colors (see `settings_colors_for`),
    /// snapshotted whenever the brushes are (re)built so the paint reads one
    /// consistent set per frame.
    settings_colors: SettingsColors,
    /// Brush for the dedicated keyboard-focus outline.
    settings_focus_brush: OwnedBrush,
    settings_accent_soft_brush: OwnedBrush,
    settings_adjust_hover_brush: OwnedBrush,
    settings_small_fill_brush: OwnedBrush,
    settings_small_hover_brush: OwnedBrush,
    history_header_brush: OwnedBrush,
    history_selected_brush: OwnedBrush,
    history_row_even_brush: OwnedBrush,
    history_row_odd_brush: OwnedBrush,
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
    /// Vertical scroll offset (client pixels) for the Settings pane. Every
    /// settings layout rect is shifted up by this amount, so all controls stay
    /// reachable on a small viewport. Zero when the content fits.
    settings_scroll_y: i32,
    /// Memoized laid-out Settings pane (see `settings_items`); keyed by
    /// `SettingsLayoutKey` so it can never serve a stale layout.
    settings_layout_cache: RefCell<Option<(SettingsLayoutKey, SettingsLayout)>>,
    /// Native TOOLTIPS_CLASS control showing full history details on hover.
    tooltip_ctrl: HWND,
    /// UTF-16 buffer backing the native tooltip's `lpszText` pointer. The
    /// tooltip control requests text on demand via `TTN_GETDISPINFO` and reads
    /// the pointer while shown; a single window-owned buffer is enough because
    /// the previous tooltip is already hidden when the next request arrives.
    /// `winutil::wide` appends the trailing NUL.
    tooltip_text: Vec<u16>,
    /// Currently registered tool range [start, end) in the native tooltip:
    /// the visible band of listbox rows. Unchanged (count, top, size) skips
    /// the sync; a scroll only touches the rows that crossed the band.
    tooltip_range: Option<(usize, usize)>,
    /// Set when an event batch changed the list; the tooltips are rebuilt once
    /// per batch instead of once per event.
    tooltips_dirty: bool,
    /// Timestamp of the last "Copy logs" press, for the "Copied" feedback.
    logs_copied_at: Option<Instant>,
    /// Timestamp of the last "Open logs" press, for the "Opened" feedback.
    logs_opened_at: Option<Instant>,
    /// Timestamp of the last "Open config" press, for the "Opened" feedback.
    config_opened_at: Option<Instant>,
    /// Persistent warning when the config cannot be (or was not) persisted;
    /// painted as a banner in the Settings pane until a save succeeds or the
    /// run ends. None while persistence works normally.
    config_status: Option<ConfigStatus>,
    /// Shared slot for the process picker's confirmed allow-list patterns. The
    /// picker writes the result here and posts a bare `PICKER_RESULT_MSG`; no
    /// pointer ever crosses the message boundary. A same-thread handoff: the
    /// write (the picker's wndproc) and the take (this handler) both run in
    /// UI-thread message handlers serialized by the loop, so the slot is a
    /// memory-sharing vehicle, not a cross-thread guard — a stale or
    /// foreign-posted message takes `None` and is a no-op.
    picker_result: Arc<Mutex<Option<Vec<String>>>>,
    /// Shared slot for the Auto-compact apps picker, which posts
    /// `AUTO_SOURCES_RESULT_MSG` (same contract as `picker_result`).
    auto_sources_result: Arc<Mutex<Option<Vec<String>>>>,
    /// Shared slot for the pinned-source picker's confirmed pattern(s). The
    /// picker runs single-select, so the slot holds at most one entry, posted
    /// via `PINNED_SOURCE_RESULT_MSG` (same contract as `picker_result`).
    pinned_source_result: Arc<Mutex<Option<Vec<String>>>>,
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
    /// Sender half of the worker's merged signal channel, used to post
    /// best-effort wake-up hints after a control-mailbox push. The worker
    /// never polls the shared config, so every settings/tray change that
    /// affects its behavior (notifications toggle, media-sources allow list)
    /// is pushed into `control_mailbox` here.
    control_tx: SyncSender<Signal>,
    /// Latest-value mailbox carrying worker control commands (see
    /// `smtc::ControlMailbox`). Pushes never drop and survive worker
    /// restarts, unlike the channel-borne `Signal::Control` commands this
    /// replaced.
    control_mailbox: Arc<Mutex<ControlMailbox>>,
    /// Whether the position indicator in the Activity pane is hovered.
    position_hover: bool,
    /// Attempts consumed by the initial tray-add retry timer (see
    /// `TRAY_RETRY_TIMER_ID`). Zero while the icon is up or the budget is
    /// spent.
    tray_add_attempts: u32,
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
    control_tx: SyncSender<Signal>,
    control_mailbox: Arc<Mutex<ControlMailbox>>,
) -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.context("getting the process module")?;
    let instance: HINSTANCE = module.into();
    let class_name = wide("WinGlanceMainWindow");
    register_main_class(instance, &class_name)?;

    let mut state = Box::new(MainWindowState::new(
        config.clone(),
        queue,
        overlay_hwnd,
        instance,
        control_tx,
        control_mailbox,
    ));
    state.wake = wake;
    let state_ptr = Box::into_raw(state);
    MAIN_STATE_CLAIMED.reset();
    let hwnd = unsafe {
        crate::winapi::create_window(
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

    // Hide-on-start unless this is the very first run (the launch that just
    // created config.toml): a first manual launch shows the tracking window
    // once so the app is discoverable, while `start_in_tray` keeps every
    // later launch — autostart at logon included — silent.
    let show_window_once = {
        let cfg = config.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        cfg.first_run || !cfg.behavior.start_in_tray
    };
    unsafe {
        if show_window_once {
            let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
            // The tooltip timer is normally started by show_window(); this
            // visible-at-start path bypasses it, so start it and sync once
            // here (the window is already shown, so sync_tooltips can run).
            let _ = set_timer(hwnd, TIMER_TOOLTIPS_ID, 1000, None);
            let state_ref = &mut *state_ptr;
            state_ref.sync_tooltips();
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
    if let Err(error) = install_tray_icon(hwnd) {
        // A failed initial add (Explorer not up yet at logon, a transient
        // shell state) must not abort startup silently: retry on a window
        // timer for ~10 s, then give up with one bounded error. A later
        // Explorer restart still recovers the icon via TaskbarCreated.
        warn!("initial tray add failed ({error}); retrying every {TRAY_RETRY_INTERVAL_MS} ms");
        unsafe {
            (*state_ptr).tray_add_attempts = 1;
            let _ = set_timer(hwnd, TRAY_RETRY_TIMER_ID, TRAY_RETRY_INTERVAL_MS, None);
        }
    } else {
        debug!("tray icon installed");
    }
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

    /// Mutates the config under a single write-lock scope, then persists it
    /// through `save_checked` (which refuses to clobber a file edited on disk
    /// since load). Never call `self.cfg()` (a read lock) from inside
    /// `mutate`. The lock is released before `save_checked`: the disk write
    /// would otherwise stall every config read (the SMTC worker's flush
    /// decisions, the overlay's behavior flags) for its duration. The clone is
    /// safe because the main window is the single writer — no other site can
    /// change the config between the lock release and the save.
    fn mutate_config(&mut self, mutate: impl FnOnce(&mut Config)) {
        // A poisoned lock still yields the (possibly stale) config;
        // recovering beats panicking on the UI thread for the rest of
        // the run.
        let mut changed = {
            let mut cfg = self.config.write().unwrap_or_else(|poisoned| poisoned.into_inner());
            mutate(&mut cfg);
            cfg.clone()
        };
        self.persist_change(&mut changed);
    }

    /// Runs `save_checked` on the already-mutated clone and mirrors the
    /// outcome into `config_status` (the persistent Settings banner) and back
    /// into the shared config's revision on success. Logs the outcome so a
    /// conflict or disabled persistence is never silent.
    fn persist_change(&mut self, changed: &mut Config) {
        match changed.save_checked() {
            Ok(SaveOutcome::Saved(revision)) => {
                self.config
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .revision = Some(revision);
                if self.config_status.is_some() {
                    self.config_status = None;
                    self.invalidate();
                }
            }
            Ok(SaveOutcome::Conflict) => {
                warn!(
                    "config.toml changed on disk since it was loaded; this settings change applies in memory only, nothing was written"
                );
                if self.config_status != Some(ConfigStatus::Conflict) {
                    self.config_status = Some(ConfigStatus::Conflict);
                    self.invalidate();
                }
            }
            Ok(SaveOutcome::PersistenceDisabled) => {
                if self.config_status != Some(ConfigStatus::PersistenceDisabled) {
                    self.config_status = Some(ConfigStatus::PersistenceDisabled);
                    self.invalidate();
                }
            }
            Err(error) => {
                // The setting already changed in memory; without this banner
                // the UI would look persisted until a restart silently
                // discards the change. The detailed error chain goes to the
                // log; the banner carries only the bounded category.
                error!("saving config after a settings change failed: {error:#}");
                let status = ConfigStatus::SaveFailed(SaveFailKind::from_error(&error));
                if self.config_status != Some(status) {
                    self.config_status = Some(status);
                    self.invalidate();
                }
            }
        }
    }

    fn new(
        config: Arc<RwLock<Config>>,
        queue: EventQueue,
        overlay_hwnd: HWND,
        instance: HINSTANCE,
        control_tx: SyncSender<Signal>,
        control_mailbox: Arc<Mutex<ControlMailbox>>,
    ) -> Self {
        Self {
            hwnd: HWND::default(),
            instance,
            config,
            queue,
            overlay_hwnd,
            listbox: HWND::default(),
            current: None,
            history: History::new(HISTORY_CAP),
            listbox_font: OwnedFont::null(),
            fonts: FontProvider::new(96),
            gray_brush: OwnedBrush::null(),
            accent_brush: OwnedBrush::null(),
            black_brush: OwnedBrush::null(),
            sidebar_bg_brush: OwnedBrush::null(),
            sidebar_highlight_brush: OwnedBrush::null(),
            settings_border_brush: OwnedBrush::null(),
            settings_surface_brush: OwnedBrush::null(),
            settings_hover_brush: OwnedBrush::null(),
            settings_colors: settings_colors_for(&crate::winutil::SystemPreferences::DEFAULT),
            settings_focus_brush: OwnedBrush::null(),
            settings_accent_soft_brush: OwnedBrush::null(),
            settings_adjust_hover_brush: OwnedBrush::null(),
            settings_small_fill_brush: OwnedBrush::null(),
            settings_small_hover_brush: OwnedBrush::null(),
            history_header_brush: OwnedBrush::null(),
            history_selected_brush: OwnedBrush::null(),
            history_row_even_brush: OwnedBrush::null(),
            history_row_odd_brush: OwnedBrush::null(),
            accent_color: [0, 0, 0, 255],
            accent_secondary: [0, 0, 0, 255],
            accent_art_source: None,
            active_pane: Pane::Activity,
            settings_hover: None,
            settings_scroll_y: 0,
            settings_layout_cache: RefCell::new(None),
            tooltip_ctrl: HWND::default(),
            tooltip_text: Vec::new(),
            tooltip_range: None,
            tooltips_dirty: false,
            logs_copied_at: None,
            logs_opened_at: None,
            config_opened_at: None,
            config_status: None,
            picker_result: Arc::new(Mutex::new(None)),
            auto_sources_result: Arc::new(Mutex::new(None)),
            pinned_source_result: Arc::new(Mutex::new(None)),
            source_states: HashMap::new(),
            source_order: VecDeque::new(),
            wake: Arc::new(AtomicBool::new(false)),
            control_tx,
            control_mailbox,
            position_hover: false,
            tray_add_attempts: 0,
        }
    }

    /// Creates the history listbox font at the given scale, matching the
    /// height the rows are laid out at. Recreated when the DPI changes.
    fn make_listbox_font(scale: f32) -> HFONT {
        let font_name = wide("Segoe UI");
        unsafe {
            create_font(
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
        let scale = dpi.max(96) as f32 / 96.0;
        // Assigning over the field drops the previous font exactly once.
        self.listbox_font = OwnedFont::new(Self::make_listbox_font(scale));
        self.fonts = FontProvider::new(dpi);
        if !self.listbox.0.is_null() {
            unsafe {
                let item_h = (18.0 * scale).round() as i32;
                let _ = send_message(self.listbox, LB_SETITEMHEIGHT, WPARAM(0), LPARAM(item_h as isize));
                let _ = send_message(
                    self.listbox,
                    WM_SETFONT,
                    WPARAM(self.listbox_font.get().0 as usize),
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
        self.listbox_font = OwnedFont::new(Self::make_listbox_font(scale));
        self.gray_brush = OwnedBrush::new(unsafe { CreateSolidBrush(colorref(0x1E, 0x1E, 0x1E)) });
        // Fixed-color brushes for the panes, created once instead of per paint
        // (a settings repaint previously created ~40 brushes).
        self.black_brush = OwnedBrush::new(unsafe { CreateSolidBrush(COLORREF(0)) });
        self.sidebar_bg_brush = OwnedBrush::new(unsafe { CreateSolidBrush(COLORREF(0x0A0A0A)) });
        // The settings brushes (surface/border/hover/focus) come from the
        // effective color set — see `rebuild_settings_appearance`.
        self.rebuild_settings_appearance();
        self.settings_small_fill_brush = OwnedBrush::new(unsafe { CreateSolidBrush(COLORREF(0x00121212)) });
        // History-row brushes: a fixed four-color set, created once instead of
        // per owner-draw row (every scroll tick repaints every visible row).
        self.history_header_brush = OwnedBrush::new(unsafe { CreateSolidBrush(COLORREF(0x00141414)) });
        self.history_row_even_brush = OwnedBrush::new(unsafe { CreateSolidBrush(COLORREF(0)) });
        self.history_row_odd_brush = OwnedBrush::new(unsafe { CreateSolidBrush(COLORREF(0x000E0E0E)) });
        // The accent-derived brushes start from the configured accent (the
        // default pink theme) and are rebuilt when the playing song's artwork
        // changes (see `update_accent`). The highlight surfaces (sidebar
        // active pane, history selection) are dark tints of the secondary
        // accent — the whole theme is accent-based, with no fixed blue/green
        // tones.
        (self.accent_color, self.accent_secondary) = accent_from_art(None, None, self.cfg().appearance.accent_color);
        self.rebuild_accent_brushes();

        self.listbox = unsafe {
            crate::winapi::create_window(
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
                Some(self.hwnd),
                Some(HMENU(LISTBOX_ID as *mut c_void)),
                self.instance,
                None,
            )
        }
        .unwrap_or_default();
        if !self.listbox.0.is_null() {
            unsafe {
                let scale = GetDpiForWindow(self.hwnd).max(96) as f32 / 96.0;
                let item_h = (18.0 * scale).round() as i32;
                let _ = send_message(self.listbox, LB_SETITEMHEIGHT, WPARAM(0), LPARAM(item_h as isize));
                let _ = send_message(
                    self.listbox,
                    WM_SETFONT,
                    WPARAM(self.listbox_font.get().0 as usize),
                    LPARAM(1),
                );
                let header = wide("TIME     EVENT");
                let _ = send_message(self.listbox, LB_ADDSTRING, WPARAM(0), LPARAM(header.as_ptr() as isize));
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
            crate::winapi::create_window(
                WS_EX_TOPMOST,
                TOOLTIPS_CLASSW,
                PCWSTR::null(),
                WINDOW_STYLE(TTS_NOPREFIX | TTS_ALWAYSTIP) | WS_POPUP,
                0,
                0,
                0,
                0,
                Some(self.hwnd),
                None,
                self.instance,
                None,
            )
        }
        .unwrap_or_default();
        if !self.tooltip_ctrl.0.is_null() {
            unsafe {
                let _ = send_message(self.tooltip_ctrl, TTM_SETMAXTIPWIDTH, WPARAM(0), LPARAM(600));
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
                // The dynamic settings scrollbar belongs to the Settings pane
                // alone; leaving the pane hides it even when the document
                // still overflows.
                let _ = ShowScrollBar(self.hwnd, SB_VERT, false);
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
            let count = send_message(self.listbox, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0 as usize;
            let top = send_message(self.listbox, LB_GETTOPINDEX, WPARAM(0), LPARAM(0)).0 as usize;
            let mut client = RECT::default();
            let _ = GetClientRect(self.listbox, &mut client);
            let item_h = send_message(self.listbox, LB_GETITEMHEIGHT, WPARAM(0), LPARAM(0)).0 as usize;
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
                    let _ = send_message(
                        self.tooltip_ctrl,
                        TTM_DELTOOLW,
                        WPARAM(0),
                        LPARAM(&mut tool as *mut _ as isize),
                    );
                }
            }
            for index in top..end {
                let mut rect = RECT::default();
                let ok = send_message(
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
                let _ = send_message(
                    self.tooltip_ctrl,
                    message,
                    WPARAM(0),
                    LPARAM(&mut tool as *mut _ as isize),
                );
            }
            self.tooltip_range = Some((top, end));
        }
    }

    /// Where the listbox should rest after a history row has been inserted at
    /// the top (after the header). Follow the newest row when the reader was
    /// already at the top (top indices 0 and 1, where the insert is always
    /// visible), so the activity stream stays in view; otherwise shift the view
    /// down by one row so the row the reader was looking at stays under the
    /// cursor instead of being yanked to the newest row on every track change.
    /// `item_count` is the pre-insert row count; the result clamps to the
    /// post-insert last index so a reader parked at the very bottom does not
    /// scroll past the end.
    fn history_top_after_insert(old_top: usize, item_count: usize) -> usize {
        if old_top <= 1 {
            old_top
        } else {
            (old_top + 1).min(item_count)
        }
    }

    /// Fills `buffer` with the NUL-terminated UTF-16 tooltip text and returns a
    /// pointer to it for `NMTTDISPINFOW.lpszText`. The buffer must stay alive
    /// while the tooltip is shown; the caller keeps it on the window state, so
    /// the previous tooltip is already gone by the time the next request
    /// overwrites it.
    fn tooltip_text_buffer(buffer: &mut Vec<u16>, text: &str) -> PWSTR {
        *buffer = wide(text);
        PWSTR(buffer.as_mut_ptr())
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

    /// Recomputes the Settings pane's effective colors from
    /// the live system preferences and rebuilds the settings brushes — the
    /// surface/border/hover cards and the dedicated focus-outline brush — so
    /// a `WM_SETTINGCHANGE` (e.g. toggling a high-contrast theme) repaints
    /// correctly without a restart. Called once at creation as well.
    fn rebuild_settings_appearance(&mut self) {
        let colors = settings_colors_for(&crate::winutil::system_preferences());
        self.settings_colors = colors;
        // Assigning over a field drops the previous brush exactly once (see
        // `OwnedBrush`), so a theme change cannot leak the old handles.
        let solid =
            |c: [u8; 4]| -> OwnedBrush { OwnedBrush::new(unsafe { CreateSolidBrush(colorref(c[0], c[1], c[2])) }) };
        self.settings_border_brush = solid(colors.border);
        self.settings_surface_brush = solid(colors.surface);
        self.settings_hover_brush = solid(colors.hover);
        self.settings_focus_brush = solid(colors.focus);
    }

    /// Pushes the effective pill duration to the overlay: the
    /// configured value, or — while `respect_system_message_duration` is on —
    /// the larger of it and the system message-duration preference. Called on
    /// every duration change, at creation, and when the preference changes.
    fn push_effective_duration(&self) {
        let (duration_ms, respect) = {
            let cfg = self.cfg();
            (cfg.overlay.duration_ms, cfg.overlay.respect_system_message_duration)
        };
        let effective = crate::config::effective_display_duration(
            duration_ms,
            crate::winutil::system_preferences().message_duration_ms,
            respect,
        );
        set_duration(self.overlay_hwnd, effective);
    }

    /// Recreates the accent-derived brushes from the current effective
    /// colors: the accent brush + the four soft fills derive from
    /// `accent_color`, and the two highlight surfaces (sidebar active pane,
    /// history selection) derive from `accent_secondary`. Called once at
    /// window creation and whenever the playing song's palette changes;
    /// replacing a brush drops the old one, so every paint site picks up the
    /// new accent without per-paint brush allocation.
    fn rebuild_accent_brushes(&mut self) {
        // Under a high-contrast theme the accent family derives from the
        // system highlight (paired with COLOR_HIGHLIGHTTEXT labels), not the
        // art-derived accent — the configured pink would break the standard
        // HC control pairing and can fall below the palette's contrast floor.
        // Blends target the system surface so the soft fills stay in family.
        let colors = self.settings_colors;
        let hc = colors.high_contrast;
        let accent = if hc { colors.accent_fill } else { self.accent_color };
        let blend_surface = if hc { colors.surface } else { SETTINGS_SURFACE };
        let highlight_base = if hc { colors.accent_fill } else { self.accent_secondary };
        let highlight_surface = if hc { colors.surface } else { [0x0A, 0x0A, 0x0A, 0xFF] };
        // Assigning over a field drops the previous brush exactly once (see
        // `OwnedBrush`), so an accent change cannot leak the old set.
        self.accent_brush = OwnedBrush::new(unsafe { CreateSolidBrush(colorref(accent[0], accent[1], accent[2])) });
        let soft = |weight: f32| -> OwnedBrush {
            let c = mix(accent, blend_surface, weight);
            OwnedBrush::new(unsafe { CreateSolidBrush(colorref(c[0], c[1], c[2])) })
        };
        self.settings_accent_soft_brush = soft(SETTINGS_ACCENT_SOFT_WEIGHT);
        self.settings_adjust_hover_brush = soft(SETTINGS_ADJUST_HOVER_WEIGHT);
        self.settings_small_hover_brush = soft(0.35);
        let highlight = |weight: f32| -> OwnedBrush {
            let c = mix(highlight_base, highlight_surface, weight);
            OwnedBrush::new(unsafe { CreateSolidBrush(colorref(c[0], c[1], c[2])) })
        };
        self.sidebar_highlight_brush = highlight(0.15);
        self.history_selected_brush = highlight(0.20);
    }

    /// Colors the window title bar to match the effective accent — the system
    /// highlight under a high-contrast theme (`accent_fill`, the same color
    /// the settings brushes derive from), the art-derived accent otherwise —
    /// so the frame reads as one theme with the Settings pane. COLORREF is
    /// 0x00BBGGRR, hence the swapped red/blue channels. Called at creation,
    /// on accent changes, and after a `WM_SETTINGCHANGE` re-samples the
    /// high-contrast state.
    fn apply_title_bar_color(&self) {
        let accent = if self.settings_colors.high_contrast {
            self.settings_colors.accent_fill
        } else {
            self.accent_color
        };
        let color = COLORREF(((accent[2] as u32) << 16) | ((accent[1] as u32) << 8) | accent[0] as u32);
        let result = unsafe {
            DwmSetWindowAttribute(
                self.hwnd,
                DWMWA_CAPTION_COLOR,
                &color as *const COLORREF as *const c_void,
                size_of::<u32>() as u32,
            )
        };
        if let Err(error) = result {
            debug!("DwmSetWindowAttribute(CAPTION_COLOR) failed: {error}");
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
        let art_palette = self.current.as_ref().and_then(|c| c.track.palette);
        let (primary, secondary) = accent_from_art(art.as_deref(), art_palette, self.cfg().appearance.accent_color);
        if primary != self.accent_color || secondary != self.accent_secondary {
            self.accent_color = primary;
            self.accent_secondary = secondary;
            self.rebuild_accent_brushes();
            self.apply_title_bar_color();
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
                        // Redundancy is decided against what the activity
                        // already displays — the same predicate the overlay
                        // applies when it suppresses the pill update — so a
                        // row is highlighted exactly when its state reached
                        // the pill. Every reported transition is recorded;
                        // redundant ones just record grey.
                        let redundant = redundant_state_row(current.state, state);
                        current.state = state;
                        self.add_state_change(state, redundant);
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
                MediaEvent::ArtworkBudgetExceeded => self.add_budget_warning(),
                // A live position refresh is not a notification: it does not add
                // a history row, only updates the in-memory progress state below.
                MediaEvent::ProgressChanged { .. } => {}
                // The settle-time terminal Stopped (if any) already recorded the
                // history row; the active-source list follows the session
                // snapshot, not events. Overlay-standby hygiene only.
                MediaEvent::SourceGone { .. } => {}
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

    /// Authoritative playback state for a new activity: the snapshot's own
    /// state (the worker suppresses the paired event when it emits a
    /// TrackChanged, so the state must arrive on the track), then the source's
    /// last remembered state, then Playing.
    fn resolve_track_state(track: &TrackInfo, source_states: &HashMap<String, PlaybackState>) -> PlaybackState {
        track
            .playback_state
            .or_else(|| source_states.get(&track.source_app).copied())
            .unwrap_or(PlaybackState::Playing)
    }

    /// Appends a history row and syncs the listbox + tooltips. The track is
    /// converted to its text-only form first (artwork, its decode, the app
    /// icon and the palette stripped) — the history renders text only, and
    /// `Arc`-pinned image buffers would be pure waste across hundreds of rows.
    /// The listbox top stays where the reader left it: rows above the new one
    /// only shift by one, so a scroll position mid-history is not yanked back
    /// to the newest row on every track change.
    fn push_history(&mut self, track: TrackInfo, state: PlaybackState, accepted: bool) {
        let track = track.into_history_text();
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
            let count = unsafe { send_message(self.listbox, LB_GETCOUNT, WPARAM(0), LPARAM(0)) }.0 as usize;
            if count > 0 {
                let _ = unsafe { send_message(self.listbox, LB_DELETESTRING, WPARAM(count - 1), LPARAM(0)) };
            }
        }
        if !self.listbox.0.is_null() {
            unsafe {
                let count = send_message(self.listbox, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0 as usize;
                // Where the reader was before the insert: follow only when the
                // new row would be visible anyway (top rows), otherwise shift
                // the view down by one so the row under the cursor stays put.
                let old_top = send_message(self.listbox, LB_GETTOPINDEX, WPARAM(0), LPARAM(0)).0 as usize;
                let _ = send_message(self.listbox, LB_INSERTSTRING, WPARAM(1), LPARAM(row.as_ptr() as isize));
                let new_top = Self::history_top_after_insert(old_top, count);
                let _ = send_message(self.listbox, LB_SETTOPINDEX, WPARAM(new_top), LPARAM(0));
            }
        }
        // Tooltip rebuilds are coalesced per event batch (receive_events) or
        // picked up by the 1 Hz timer.
        self.tooltips_dirty = true;
    }

    /// Records one playback transition of the current activity. Every
    /// reported transition lands in the history — spam included — but only
    /// the ones that changed what the pill displays are highlighted: a
    /// `redundant` row records grey, mirroring the overlay's suppression.
    fn add_state_change(&mut self, state: PlaybackState, redundant: bool) {
        let Some(current) = &self.current else {
            return;
        };
        // Bright means the state reached the pill: a redundant re-report is
        // exactly what the overlay suppresses, and with notifications off
        // nothing reaches the pill at all.
        let reached = !redundant && self.cfg().behavior.notifications_enabled;
        // Convert to the history's text-only form before the clone so the
        // image buffers (Arc-pinned covers, app icon, palette) are never copied
        // just to be discarded.
        let track = current.track.clone().into_history_text();
        self.push_history(track, state, reached);
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
        show_tray_note(self.hwnd, "Media notifications stopped", reason, NIIF_ERROR);
    }

    /// Surfaces the one-shot in-flight artwork budget warning as a tray note
    /// (emitted at most once per app run by the worker). The budget tripping
    /// means the UI was not keeping up and some cover payloads were skipped;
    /// it is transient — covers return as soon as the UI drains — so there is
    /// no history row, only the note. Informational, not an error.
    fn add_budget_warning(&mut self) {
        show_tray_note(
            self.hwnd,
            "Album covers skipped",
            "The app could not keep up with media updates, so some album covers \
             were skipped. Covers return once it catches up.",
            NIIF_INFO,
        );
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
                // Free the icon blit if the source app changed (icon is per-source).
                if current.track.source_app != track.source_app {
                    free_art_blit(&mut current.icon_blit);
                }
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
                entry.track = track.clone().into_history_text();
                // Keep the row's original timestamp: only the metadata
                // refreshed, and the tooltip formats from the same `at`.
                let row = history_row(
                    &track,
                    entry.at,
                    track
                        .playback_state
                        .unwrap_or_else(|| self.current.as_ref().map(|c| c.state).unwrap_or(PlaybackState::Playing)),
                );
                let row = wide(&row);
                if !self.listbox.0.is_null() {
                    unsafe {
                        // The header occupies row 0; data rows mirror the
                        // entries order (newest first).
                        let lb_row = index + 1;
                        let _ = send_message(self.listbox, LB_DELETESTRING, WPARAM(lb_row), LPARAM(0));
                        let _ = send_message(
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

        // The new activity starts with the snapshot's own playback state (the
        // worker suppresses the paired playback event when it emits a
        // TrackChanged, so the state must arrive on the track itself to avoid
        // inheriting another app's Playing/Paused/Stopped), then the source's
        // last remembered state, then Playing.
        let state = Self::resolve_track_state(&track, &self.source_states);
        // History row is text-only: drop the image buffers (consume a clone).
        // Bright means the state reached the pill; with notifications off
        // nothing does.
        let reached = self.cfg().behavior.notifications_enabled;
        let history_track = track.clone().into_history_text();
        self.push_history(history_track, state, reached);
        self.current = Some(CurrentActivity {
            track,
            state,
            // The blit is built lazily on first paint; the window starts
            // hidden (start_in_tray), so a track that never gets looked at
            // pays no GDI cost.
            art_blit: None,
            art_fingerprint,
            art_decode_failed: false,
            icon_blit: None,
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
            let _ = FillRect(hdc, &whole, self.black_brush.get());
        }

        // Draw sidebar
        let sidebar_rect = RECT {
            left: 0,
            top: 0,
            right: sidebar_w,
            bottom: client_h,
        };
        unsafe {
            let _ = FillRect(hdc, &sidebar_rect, self.sidebar_bg_brush.get());
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
                    let _ = FillRect(hdc, &item_rect, self.sidebar_highlight_brush.get());
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
        // cached pixels, never decodes an image. The decode side is fixed
        // (`ARTWORK_DECODE`), so derive it from the buffer length.
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
        // Build the app-icon blit lazily from the worker's decoded pixels.
        // The icon is 24×24 premultiplied BGRA (Arc-shared); the blit adds
        // ~2.4 KB (pixel data + GDI handles).
        if let Some(current) = &mut self.current
            && current.icon_blit.is_none()
            && let Some(icon) = current.track.app_icon.as_deref()
        {
            let base = ((icon.len() / 4) as f64).sqrt() as i32;
            current.icon_blit = build_art_blit(icon, base);
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
                    let _ = FillRect(hdc, &art_rect, self.accent_brush.get());
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
                let app_y = art_y + (100.0 * scale) as i32;
                let icon_size = (16.0 * scale).round() as i32;
                let icon_gap = (4.0 * scale).round() as i32;
                // Render the app icon before the source name, matching the
                // pill's app-row convention (icon left, name right).
                let app_text_left = if let Some(icon_blit) = &current.icon_blit {
                    draw_art_blit(hdc, icon_blit, icon_size, text_left, app_y);
                    text_left + icon_size + icon_gap
                } else {
                    text_left
                };
                let mut app_rect = RECT {
                    left: app_text_left,
                    top: app_y,
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
            let _ = FillRect(hdc, &separator, self.gray_brush.get());
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
        let pos_color = if self.position_hover {
            [0x99, 0x99, 0x99, 0xFF]
        } else {
            [0x66, 0x66, 0x66, 0xFF]
        };
        draw_string(
            &self.fonts,
            hdc,
            &pos_label,
            &mut pos_rect,
            (10.0 * scale) as i32,
            pos_color,
            false,
            false,
        );
    }

    /// Builds the settings pane items (section headers + interactive rows).
    /// Both painting and hit-testing use this single source of layout truth.
    /// Lays out the Settings pane items. `scroll_y` shifts every rect up by that
    /// many client pixels; `content_extent` reports the natural document bottom
    /// (ignoring `scroll_y`) so the scroll range can be derived without a second
    /// layout. Paint, hit-test, hover, focus and the UIA provider all read the
    /// same offset rects, so scrolling stays consistent across every path.
    fn settings_items(&self, content_left: i32, client_w: i32, pad: i32, scale: f32, scroll_y: i32) -> SettingsLayout {
        // Memoized: the layout is a pure function of (client geometry, scroll
        // offset, banner status), so the ~20-item Vec (and its per-call
        // allocation) is rebuilt only when one of those changes — paint,
        // hover hit-test, focus walk, scroll sync and UIA queries all share
        // one layout. The key covers every input, so the cache cannot go
        // stale.
        let key = SettingsLayoutKey {
            content_left,
            client_w,
            pad,
            scale: scale.to_bits(),
            scroll_y,
            status: self.config_status,
        };
        if let Some((cached_key, cached)) = &*self.settings_layout_cache.borrow()
            && *cached_key == key
        {
            return cached.clone();
        }
        let layout = Self::build_settings_layout(content_left, client_w, pad, scale, scroll_y, key.status);
        *self.settings_layout_cache.borrow_mut() = Some((key, layout.clone()));
        layout
    }

    /// Pure layout builder for the Settings pane (see `settings_items` for the
    /// memoized wrapper). `status` controls whether the persistence banner is
    /// inserted — the only non-geometric input the layout depends on.
    fn build_settings_layout(
        content_left: i32,
        client_w: i32,
        pad: i32,
        scale: f32,
        scroll_y: i32,
        status: Option<ConfigStatus>,
    ) -> SettingsLayout {
        let row_h = (34.0 * scale) as i32;
        let gap = (8.0 * scale) as i32;
        let header_h = (18.0 * scale) as i32;
        let left = content_left + pad;
        let right = client_w - pad;
        let mut y = pad + (36.0 * scale) as i32;
        let mut natural = Vec::new();

        natural.push(SettingsItem::Header {
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
            SettingId::RespectSystemDuration,
            SettingId::StartOnLogin,
            SettingId::CloseToTray,
            SettingId::AllowedApps,
        ] {
            natural.push(SettingsItem::Row {
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
        natural.push(SettingsItem::Header {
            text: "Overlay",
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + header_h,
            },
        });
        y += (22.0 * scale) as i32;
        natural.push(SettingsItem::Row {
            id: SettingId::Layout,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        natural.push(SettingsItem::Row {
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
        natural.push(SettingsItem::Row {
            id: SettingId::SeparateCompact,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        natural.push(SettingsItem::Row {
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
        natural.push(SettingsItem::Row {
            id: SettingId::DismissOnHover,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        natural.push(SettingsItem::Row {
            id: SettingId::ExpandCompactOnHover,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        natural.push(SettingsItem::Row {
            id: SettingId::AutoCompactApps,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        natural.push(SettingsItem::Row {
            id: SettingId::HideForAutoCompactSources,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        natural.push(SettingsItem::Row {
            id: SettingId::FadePersistentPill,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        natural.push(SettingsItem::Row {
            id: SettingId::PinnedSource,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        natural.push(SettingsItem::Row {
            id: SettingId::Monitor,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        natural.push(SettingsItem::Row {
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
        natural.push(SettingsItem::Header {
            text: "Diagnostics",
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + header_h,
            },
        });
        y += (22.0 * scale) as i32;
        natural.push(SettingsItem::Row {
            id: SettingId::CopyLogs,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        y += row_h + gap;
        if let Some(status) = status {
            natural.push(SettingsItem::Banner {
                text: banner_text(status),
                rect: RECT {
                    left,
                    top: y,
                    right,
                    bottom: y + row_h,
                },
            });
            y += row_h + gap;
            y += (8.0 * scale) as i32;
        }
        natural.push(SettingsItem::Row {
            id: SettingId::OpenConfig,
            rect: RECT {
                left,
                top: y,
                right,
                bottom: y + row_h,
            },
        });
        // The document bottom is the lowest item bottom (not the running `y`,
        // which is not advanced past a final row) plus a pad of breathing room
        // so the final control is not flush against the edge.
        let content_extent = natural
            .iter()
            .map(|item| match item {
                SettingsItem::Header { rect, .. }
                | SettingsItem::Row { rect, .. }
                | SettingsItem::Banner { rect, .. } => rect.bottom,
            })
            .max()
            .unwrap_or(0)
            + pad;
        let dy = -scroll_y;
        let items = natural
            .into_iter()
            .map(|item| match item {
                SettingsItem::Header { text, rect } => SettingsItem::Header {
                    text,
                    rect: offset_rect(rect, dy),
                },
                SettingsItem::Row { id, rect } => SettingsItem::Row {
                    id,
                    rect: offset_rect(rect, dy),
                },
                SettingsItem::Banner { text, rect } => SettingsItem::Banner {
                    text,
                    rect: offset_rect(rect, dy),
                },
            })
            .collect();
        SettingsLayout { items, content_extent }
    }

    /// Natural document height of the Settings pane, independent of the live
    /// scroll offset. Used to size the scroll range and clamp the thumb.
    fn settings_content_extent(&self, content_left: i32, client_w: i32, pad: i32, scale: f32) -> i32 {
        self.settings_items(content_left, client_w, pad, scale, 0)
            .content_extent
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
        // The effective Settings color set: AA-lifted state text normally,
        // system colors under a high-contrast theme.
        let colors = self.settings_colors;
        // Emphasized text uses the art-derived accent for the normal theme,
        // and the system-derived accent (COLOR_HIGHLIGHT lifted to the AA
        // floor) under a high-contrast theme — the configured pink would
        // otherwise fall below the contrast floor the palette enforces.
        let accent = if colors.high_contrast {
            colors.accent
        } else {
            self.accent_color
        };
        let notifications_enabled = cfg.behavior.notifications_enabled;
        let settings_hover = self.settings_hover;
        let duration_ms = cfg.overlay.duration_ms;
        // The value the pill actually applies: while
        // `respect_system_message_duration` is on, the effective duration is
        // the larger of the configured value and the system message-duration
        // preference. Surfaced in the row below when it differs, so the UI
        // never shows a value the pill does not honor.
        let duration_effective_ms = crate::config::effective_display_duration(
            duration_ms,
            crate::winutil::system_preferences().message_duration_ms,
            cfg.overlay.respect_system_message_duration,
        );
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
        let hide_for_auto_compact = cfg.behavior.hide_for_auto_compact_sources;
        let fade_persistent_pill = cfg.overlay.fade_persistent_pill;
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

        let items = self
            .settings_items(content_left, client_w, pad, scale, self.settings_scroll_y)
            .items;
        let brushes = SettingsBrushes {
            border: self.settings_border_brush.get(),
            surface: self.settings_surface_brush.get(),
            hover: self.settings_hover_brush.get(),
            accent: self.accent_brush.get(),
            accent_soft: self.settings_accent_soft_brush.get(),
            adjust_hover: self.settings_adjust_hover_brush.get(),
            small_fill: self.settings_small_fill_brush.get(),
            small_hover: self.settings_small_hover_brush.get(),
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
                            colors.faint,
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
                        let _ = FillRect(hdc, rect, self.settings_border_brush.get());
                    }
                    let inner = RECT {
                        left: rect.left + 1,
                        top: rect.top + 1,
                        right: rect.right - 1,
                        bottom: rect.bottom - 1,
                    };
                    unsafe {
                        let bg = if hovered_row {
                            self.settings_hover_brush.get()
                        } else {
                            self.settings_surface_brush.get()
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
                            if notifications_enabled { accent } else { colors.faint },
                        ),
                        SettingId::StartOnLogin => (
                            "Start on login",
                            if start_on_login {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if start_on_login { accent } else { colors.faint },
                        ),
                        SettingId::CloseToTray => (
                            "Close to tray",
                            if close_to_tray {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if close_to_tray { accent } else { colors.faint },
                        ),
                        SettingId::Duration => {
                            let value = if duration_effective_ms > duration_ms {
                                format!(
                                    "{} (system {})",
                                    format_duration_label(duration_ms),
                                    format_duration_label(duration_effective_ms)
                                )
                            } else {
                                format_duration_label(duration_ms)
                            };
                            ("Duration", value, colors.muted)
                        }
                        SettingId::RespectSystemDuration => (
                            "Respect system message duration",
                            if cfg.overlay.respect_system_message_duration {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if cfg.overlay.respect_system_message_duration {
                                accent
                            } else {
                                colors.faint
                            },
                        ),
                        SettingId::Layout => ("Layout", String::new(), colors.muted),
                        SettingId::Position => ("Expanded Position", position_label.clone(), colors.muted),
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
                            if compact_separate { colors.faint } else { accent },
                        ),
                        SettingId::CompactPosition => {
                            ("Compact position", compact_position_label.clone(), colors.muted)
                        }
                        SettingId::DismissOnHover => (
                            "Dismiss on hover",
                            if dismiss_on_hover {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if dismiss_on_hover { accent } else { colors.faint },
                        ),
                        SettingId::ExpandCompactOnHover => (
                            "Expand compact on hover",
                            if expand_compact_on_hover {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if expand_compact_on_hover { accent } else { colors.faint },
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
                            colors.muted,
                        ),
                        SettingId::HideForAutoCompactSources => (
                            "Hide Persistent Compact Pill for Auto-compact Apps",
                            if hide_for_auto_compact {
                                "ON".to_string()
                            } else {
                                "OFF".to_string()
                            },
                            if hide_for_auto_compact { accent } else { colors.faint },
                        ),
                        SettingId::FadePersistentPill => (
                            "Fade Persistent Compact Pill after duration",
                            if fade_persistent_pill {
                                "Yes".to_string()
                            } else {
                                "No".to_string()
                            },
                            if fade_persistent_pill { accent } else { colors.faint },
                        ),
                        SettingId::PinnedSource => (
                            "Pinned source",
                            match &cfg.behavior.pinned_source {
                                Some(pin) => pin.clone(),
                                None => "None".to_string(),
                            },
                            colors.muted,
                        ),
                        SettingId::Monitor => ("Monitor", monitor_label(&cfg, display_count), colors.muted),
                        SettingId::AllowedApps => (
                            "Allowed apps",
                            if media_sources.is_empty() {
                                "All".to_string()
                            } else {
                                media_sources.clone()
                            },
                            colors.muted,
                        ),
                        SettingId::ShowSample => ("Preview Notification", String::new(), colors.muted),
                        SettingId::CopyLogs => ("Logs", String::new(), colors.muted),
                        SettingId::OpenConfig => ("Config", String::new(), colors.muted),
                    };
                    let mut lbl_rect = label_rect;
                    draw_string(
                        &self.fonts,
                        hdc,
                        label,
                        &mut lbl_rect,
                        (11.0 * scale) as i32,
                        colors.muted,
                        false,
                        false,
                    );

                    match id {
                        SettingId::Notifications
                        | SettingId::RespectSystemDuration
                        | SettingId::StartOnLogin
                        | SettingId::CloseToTray
                        | SettingId::AllowedApps
                        | SettingId::SeparateCompact
                        | SettingId::DismissOnHover
                        | SettingId::ExpandCompactOnHover
                        | SettingId::HideForAutoCompactSources
                        | SettingId::FadePersistentPill
                        | SettingId::PinnedSource
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
                            // Four preset tiles plus a Custom tile that opens
                            // the input dialog; a value outside the presets
                            // (dialog or hand-edited) activates Custom.
                            let segments = segment_rects(&control_rect, 5, (4.0 * scale) as i32);
                            let values = [2000u64, 3000, 5000, 10000];
                            let exact = values.contains(&duration_ms);
                            for (i, seg) in segments.iter().enumerate() {
                                let is_custom = i == 4;
                                let active = if is_custom { !exact } else { duration_ms == values[i] };
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
                                // On an active segment the label sits on the
                                // accent fill: under high contrast it must be
                                // the system highlight-text pairing.
                                let tc = if active && colors.high_contrast {
                                    colors.accent_fill_text
                                } else if active {
                                    colors.text
                                } else {
                                    colors.muted
                                };
                                // The Custom tile shows the actual value while
                                // active, like the tray menu's "Custom (Xs)".
                                let label = if is_custom {
                                    if exact {
                                        "Custom".to_string()
                                    } else {
                                        format!("{}s", duration_ms as f64 / 1000.0)
                                    }
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
                                    active,
                                    true,
                                );
                            }
                        }
                        SettingId::Layout => {
                            // Four segments mirroring the LayoutMode variants;
                            // the same accent/hover treatment as Duration.
                            let segments = segment_rects(&control_rect, 4, (4.0 * scale) as i32);
                            let values = [
                                LayoutMode::Expanded,
                                LayoutMode::Compact,
                                LayoutMode::Auto,
                                LayoutMode::PersistentCompact,
                            ];
                            let labels = ["Expanded", "Compact", "Auto", "Persistent Compact"];
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
                                // On an active segment the label sits on the
                                // accent fill: under high contrast it must be
                                // the system highlight-text pairing.
                                let tc = if active && colors.high_contrast {
                                    colors.accent_fill_text
                                } else if active {
                                    colors.text
                                } else {
                                    colors.muted
                                };
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
                                colors.faint,
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
                                    colors,
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
                            // Under high contrast the Adjust label uses the
                            // guaranteed highlight-text pairing (the fill is
                            // the system highlight); otherwise clamp the
                            // accent against the soft fill as before.
                            let label_color = if colors.high_contrast {
                                colors.accent_fill_text
                            } else {
                                crate::overlay::ensure_contrast(
                                    accent,
                                    mix(accent, SETTINGS_SURFACE, fill_weight),
                                    crate::overlay::TEXT_CONTRAST_AA,
                                )
                            };
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
                                colors.faint,
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
                                    colors,
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
                            // Under high contrast the Adjust label uses the
                            // guaranteed highlight-text pairing (the fill is
                            // the system highlight); otherwise clamp the
                            // accent against the soft fill as before.
                            let label_color = if colors.high_contrast {
                                colors.accent_fill_text
                            } else {
                                crate::overlay::ensure_contrast(
                                    accent,
                                    mix(accent, SETTINGS_SURFACE, fill_weight),
                                    crate::overlay::TEXT_CONTRAST_AA,
                                )
                            };
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
                            let opened = self
                                .logs_opened_at
                                .is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                            draw_small_button(
                                &self.fonts,
                                hdc,
                                &open_rect,
                                if opened { "Opened" } else { "Open logs" },
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
                            let opened = self
                                .config_opened_at
                                .is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                            draw_small_button(
                                &self.fonts,
                                hdc,
                                &open_rect,
                                if opened { "Opened" } else { "Open config" },
                                accent,
                                hovered_open,
                                scale,
                                brushes,
                            );
                            draw_small_button(
                                &self.fonts,
                                hdc,
                                &reload_rect,
                                "Restart app",
                                accent,
                                hovered_reload,
                                scale,
                                brushes,
                            );
                        }
                    }
                    // The dedicated keyboard-focus outline: the exact
                    // interaction rectangle of the focused sub-control (the
                    // same rect the UIA bounds and the click point derive
                    // from) gets a border whose color is checked against the
                    // painted surface at 3:1, instead of relying on the
                    // low-delta hover fill. The width follows the user's
                    // focus-border metric, clamped to a sane 1-3 px so a huge
                    // metric cannot swallow a 34 px row.
                    if let Some((_, sub)) = settings_hover.filter(|(r, _)| *r == current_row) {
                        let focus_rect = setting_sub_rect(*id, sub, rect, scale).unwrap_or(*rect);
                        let width = crate::winutil::system_preferences().focus_border_px.clamp(1, 3) as i32;
                        for inset in 0..width {
                            let outline = RECT {
                                left: focus_rect.left + inset,
                                top: focus_rect.top + inset,
                                right: focus_rect.right - inset,
                                bottom: focus_rect.bottom - inset,
                            };
                            unsafe {
                                let _ = FrameRect(hdc, &outline, self.settings_focus_brush.get());
                            }
                        }
                    }
                }
                SettingsItem::Banner { text, rect } => {
                    if rects_intersect(invalid, rect) {
                        // Card like the rows (surface inner on a border fill),
                        // with the warning text in amber. Never interactive:
                        // `settings_hover_at` and `focus_targets` ignore
                        // Banner items entirely.
                        unsafe {
                            let _ = FillRect(hdc, rect, brushes.border);
                        }
                        let inner = RECT {
                            left: rect.left + 1,
                            top: rect.top + 1,
                            right: rect.right - 1,
                            bottom: rect.bottom - 1,
                        };
                        unsafe {
                            let _ = FillRect(hdc, &inner, brushes.surface);
                        }
                        let mut tr = RECT {
                            left: rect.left + (12.0 * scale) as i32,
                            top: rect.top,
                            right: rect.right - (12.0 * scale) as i32,
                            bottom: rect.bottom,
                        };
                        draw_string(
                            &self.fonts,
                            hdc,
                            text,
                            &mut tr,
                            (12.0 * scale) as i32,
                            colors.warn,
                            true,
                            false,
                        );
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
        let items = self
            .settings_items(content_left, client_w, pad, scale, self.settings_scroll_y)
            .items;
        let mut row_index = 0usize;
        for item in &items {
            if let SettingsItem::Row { id, rect } = item
                && y >= rect.top
                && y < rect.bottom
            {
                let control_rect = row_split(rect, scale).control;
                if *id == SettingId::Duration {
                    let segments = segment_rects(&control_rect, 5, (4.0 * scale) as i32);
                    let seg = segments.iter().position(|s| x >= s.left && x < s.right);
                    // A click or hover in the gap right of the last segment is
                    // not the first segment; the row stays highlighted.
                    return Some((row_index, seg.map_or(SettingSub::None, SettingSub::Seg)));
                }
                if *id == SettingId::Layout {
                    let segments = segment_rects(&control_rect, 4, (4.0 * scale) as i32);
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
                    // left half is "Open config", the right half "Restart app".
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
    /// them, each carrying the EXACT interaction rectangle (and its center, the
    /// client coordinate a click on the control carries). The keyboard handler
    /// reuses the mouse click path by posting `WM_LBUTTONDOWN` at `(cx, cy)`, so
    /// this enumeration must stay in lockstep with the hover geometry in
    /// `settings_hover_at` — both now derive from the single `setting_sub_rect`.
    fn settings_focus_targets(&self, content_left: i32, client_w: i32, pad: i32, scale: f32) -> Vec<SettingsFocus> {
        let items = self
            .settings_items(content_left, client_w, pad, scale, self.settings_scroll_y)
            .items;
        let mut out = Vec::new();
        let mut row_index = 0usize;
        for item in &items {
            if let SettingsItem::Row { id, rect } = item {
                let subs: Vec<SettingSub> = match *id {
                    SettingId::Duration | SettingId::Layout => {
                        (0..setting_segment_count(*id)).map(SettingSub::Seg).collect()
                    }
                    SettingId::CopyLogs => vec![SettingSub::Open, SettingSub::Copy],
                    SettingId::OpenConfig => vec![SettingSub::OpenConfig, SettingSub::ReloadConfig],
                    SettingId::Position | SettingId::CompactPosition => {
                        let mut subs: Vec<SettingSub> = (0..6).map(SettingSub::Anchor).collect();
                        subs.push(SettingSub::Reset);
                        subs.push(SettingSub::Adjust);
                        subs
                    }
                    _ => vec![SettingSub::None],
                };
                for sub in subs {
                    if let Some(control) = setting_sub_rect(*id, sub, rect, scale) {
                        out.push(SettingsFocus {
                            row_index,
                            sub,
                            rect: control,
                            cx: (control.left + control.right) / 2,
                            cy: (control.top + control.bottom) / 2,
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
        // Keep the focused control on screen: when it would fall outside the
        // visible band, recenter it in the viewport. The scroll change repaints
        // and keeps the native scrollbar in sync.
        let (_, client_h) = client_size(self.hwnd);
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let row_h = (34.0 * scale) as i32;
        if t.cy < self.settings_scroll_y + row_h / 2 || t.cy > self.settings_scroll_y + client_h - row_h / 2 {
            self.settings_scroll_y = t.cy - client_h / 2;
            self.sync_settings_scroll(client_w, client_h);
        }
        raise_settings_focus_event(self.hwnd, new_hover);
    }

    /// Clamps the live Settings scroll offset to the reachable range, repaints
    /// when it moves, and keeps the native vertical scrollbar synced. The main
    /// window is created without WS_VSCROLL: the scrollbar is shown dynamically
    /// (only while Settings is active and the content overflows), so the
    /// Activity pane never shows it. Call after any scroll change (wheel,
    /// keyboard, thumb, focus auto-scroll, pane switch, resize).
    fn sync_settings_scroll(&mut self, client_w: i32, client_h: i32) {
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let pad = (PAD * scale) as i32;
        let sidebar_w = (SIDEBAR_W * scale).round() as i32;
        let extent = self.settings_content_extent(sidebar_w, client_w, pad, scale);
        let new_y = clamp_settings_scroll(self.settings_scroll_y, extent, client_h);
        if new_y != self.settings_scroll_y {
            self.settings_scroll_y = new_y;
            self.invalidate();
        }
        if self.active_pane == Pane::Settings && !self.hwnd.0.is_null() {
            let scrollable = extent > client_h;
            unsafe {
                let si = SCROLLINFO {
                    cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                    nMin: 0,
                    nMax: extent,
                    nPage: client_h as u32,
                    nPos: self.settings_scroll_y,
                    ..Default::default()
                };
                let _ = SetScrollInfo(self.hwnd, SB_VERT, &si, true);
                let _ = ShowScrollBar(self.hwnd, SB_VERT, scrollable);
            }
        }
    }

    /// Applies a scroll delta (client pixels) then syncs the range.
    fn scroll_settings_by(&mut self, delta: i32, client_w: i32, client_h: i32) {
        self.settings_scroll_y = self.settings_scroll_y.saturating_add(delta);
        self.sync_settings_scroll(client_w, client_h);
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
            let _ = set_window_pos(
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
                let _ = kill_timer(self.hwnd, TIMER_TOOLTIPS_ID);
                // Arm the idle release so a long tray-hidden window drops its
                // cached artwork blit (a few hundred KB); show_window() kills
                // the timer on restore.
                let _ = set_timer(self.hwnd, IDLE_ART_TIMER_ID, IDLE_ART_RELEASE_MS, None);
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
            self.history_header_brush.get()
        } else if selected {
            self.history_selected_brush.get()
        } else if index.is_multiple_of(2) {
            self.history_row_even_brush.get()
        } else {
            self.history_row_odd_brush.get()
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
            // Rows whose state reached the pill are highlighted in pink (the
            // accent color) with bold text; redundant re-reports and rejected
            // sessions render muted, so the bright rows are exactly what the
            // pill showed.
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
            free_art_blit(&mut current.icon_blit);
        }
        unsafe {
            let _ = kill_timer(self.hwnd, TIMER_TOOLTIPS_ID);
            let _ = kill_timer(self.hwnd, TRAY_RETRY_TIMER_ID);
            if !self.tooltip_ctrl.0.is_null() {
                let _ = DestroyWindow(self.tooltip_ctrl);
                self.tooltip_ctrl = HWND::default();
            }
        }
        // The listbox font and every brush are `Owned*` handles: they drop
        // with the state box at WM_NCDESTROY and delete themselves exactly
        // once — there is no manual free list to keep in sync here.
    }

    /// Marks the whole window for repaint on the next WM_PAINT. Deliberately
    /// cheap — it only invalidates the client area and does no work at call
    /// time (no DIB recreation, no font/brush setup), so settings-mutating
    /// click arms may call it freely after every mutation to repaint the new
    /// value and hover state in the same frame.
    fn invalidate(&self) {
        unsafe {
            let _ = invalidate_rect(self.hwnd, None, false);
        }
    }

    /// Invalidates only the given client-space region, so hover highlights
    /// repaint a small band instead of the whole window.
    fn invalidate_rect(&self, rect: &RECT) {
        unsafe {
            let _ = invalidate_rect(self.hwnd, Some(rect), false);
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
        let items = self
            .settings_items(sidebar_w, client_w, pad, scale, self.settings_scroll_y)
            .items;
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
            let _ = kill_timer(self.hwnd, IDLE_ART_TIMER_ID);
            let _ = ShowWindow(self.hwnd, SW_SHOWMAXIMIZED);
            // The foreground lock can reject SetForegroundWindow (the thread
            // never held the foreground); without a fallback the window would
            // open silently behind the current app. Bring it to the top of the
            // z-order without stealing focus instead.
            if !SetForegroundWindow(self.hwnd).as_bool() {
                let _ = set_window_pos(
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
            let _ = set_timer(self.hwnd, TIMER_TOOLTIPS_ID, 1000, None);
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
        let clipboard = wide(&text);
        let bytes = clipboard.len() * 2;

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
                    let _ = global_free(hmem);
                    return false;
                }
                std::ptr::copy_nonoverlapping(clipboard.as_ptr(), ptr.cast(), clipboard.len());
                let _ = GlobalUnlock(hmem);
                if set_clipboard_data(CF_UNICODETEXT.0 as u32, HANDLE(hmem.0)).is_ok() {
                    true
                } else {
                    // Transfer failed; the memory is still ours to release.
                    let _ = global_free(hmem);
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
            let _ = set_timer(self.hwnd, TIMER_LOGS_ID, 2000, None);
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
            let result = shell_execute(
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
            result as i32
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
        // the in-memory mutation, and `save_checked` runs after the lock is
        // released so the disk write never stalls other config readers.
        let mut changed = {
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
        self.persist_change(&mut changed);
        // The log keeps the raw field value (greppable) and the displayed
        // polarity (ON = follows Expanded = field false).
        info!(
            "compact_position_separate set to {separate} ({})",
            if separate {
                "compact position: independent"
            } else {
                "compact position: follows expanded"
            }
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

/// Whether an incoming playback state repeats what the current activity
/// already displays. This mirrors the overlay's suppression predicate: such
/// a re-report never changes the pill, so its history row records grey
/// instead of highlighting.
fn redundant_state_row(displayed: PlaybackState, incoming: PlaybackState) -> bool {
    displayed == incoming
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
    let hbm = unsafe { create_dib_section(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0) };
    let Ok(hbm) = hbm else {
        unsafe {
            let _ = DeleteDC(mem);
        }
        return None;
    };
    if bits.is_null() {
        unsafe {
            let _ = delete_object(hbm);
            let _ = DeleteDC(mem);
        }
        return None;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(pm.as_ptr(), bits.cast::<u8>(), pm.len());
        let old = select_object(mem, hbm);
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

fn tray_icon() -> Result<HICON> {
    let raw = *TRAY_ICON.get_or_init(|| {
        match unsafe { LoadIconW(None, IDI_APPLICATION) } {
            Ok(icon) => icon.0 as isize,
            Err(error) => {
                // A failed stock-icon load (broken system resources) is a
                // normal initialization failure, not a panic. The
                // failure is cached: a re-add would log the same error.
                error!("LoadIconW(IDI_APPLICATION) failed: {error}");
                0
            }
        }
    });
    if raw == 0 {
        anyhow::bail!("the tray icon could not be loaded");
    }
    Ok(HICON(raw as *mut c_void))
}

fn install_tray_icon(hwnd: HWND) -> Result<()> {
    // A contained wndproc panic skips the normal WM_DESTROY teardown, which
    // would leave a ghost tray icon (Explorer reaps it only on hover): give
    // the panic containment a best-effort removal for this window. First
    // registration wins, and every install uses the same main-window hwnd.
    // The raw handle travels as a usize so the closure stays Send + Sync.
    let hwnd_raw = hwnd.0 as usize;
    crate::winutil::set_panic_cleanup(Box::new(move || {
        remove_tray_icon(HWND(hwnd_raw as *mut core::ffi::c_void));
    }));
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &tray_data(hwnd)?) }.as_bool() {
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
    let Ok(data) = tray_data(hwnd) else {
        return;
    };
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

fn tray_data(hwnd: HWND) -> Result<NOTIFYICONDATAW> {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: tray_icon()?,
        ..Default::default()
    };
    crate::winutil::copy_wide_terminated(&mut data.szTip, "WinGlance media overlay");
    Ok(data)
}

/// Shows a one-shot balloon note on the tray icon (NIF_INFO), with the given
/// info flag (error vs informational). Used for the permanent SMTC worker
/// failure and the one-shot budget-drop warning: both are visible even while
/// the tracking window is hidden (start in tray).
fn show_tray_note(hwnd: HWND, title: &str, text: &str, flags: NOTIFY_ICON_INFOTIP_FLAGS) {
    // Best-effort by design (see below): a missing icon skips the balloon.
    let Ok(mut data) = tray_data(hwnd) else {
        return;
    };
    data.uFlags |= NIF_INFO;
    data.dwInfoFlags = flags;
    // NUL-terminate explicitly via the shared helper (`copy_wide_terminated`
    // caps at len-1 and writes the terminator), so a truncated title/text
    // never leaves the fixed-size array unterminated — `Shell_NotifyIconW`
    // must never read past the buffer.
    crate::winutil::copy_wide_terminated(&mut data.szInfoTitle, title);
    crate::winutil::copy_wide_terminated(&mut data.szInfo, text);
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

fn show_tray_menu(hwnd: HWND) {
    debug!("tray menu opened");
    // Modal-borrow discipline: the menu is built from an immutable read whose borrow
    // ends before the TrackPopupMenu modal loop — messages dispatched inside
    // the loop re-enter the wndproc and take the window state themselves, so
    // this frame must never hold a competing borrow across a modal scope.
    // The dispatch after the loop re-fetches the pointer for the same
    // reason.
    let state_ptr = window_state::<MainWindowState>(hwnd);
    if state_ptr.is_null() {
        return;
    }
    let state = unsafe { &*state_ptr };
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
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_PREVIEW_NOTIFY_ID,
            PCWSTR(wide("Preview Notification").as_ptr()),
        );
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
        let current_ms = state.cfg().overlay.duration_ms;
        let presets: [(u64, usize, &str); 4] = [
            (2, MENU_DURATION_2S, "2 seconds"),
            (3, MENU_DURATION_3S, "3 seconds"),
            (5, MENU_DURATION_5S, "5 seconds"),
            (10, MENU_DURATION_10S, "10 seconds"),
        ];
        // Preset membership compares milliseconds exactly: a 2999 ms custom
        // value must not light up the "2 seconds" checkmark.
        let is_preset = presets.iter().any(|(s, _, _)| current_ms == s * 1000);
        for (secs, id, label) in presets {
            let flags = if current_ms == secs * 1000 {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            };
            let _ = AppendMenuW(duration_menu, flags, id, PCWSTR(wide(label).as_ptr()));
        }
        // Always-available custom duration: plain "Custom" when a preset is
        // active, checked "Custom (Xs)" when the value is outside the presets.
        // Both open the input dialog.
        let custom_label = if is_preset {
            "Custom".to_string()
        } else {
            format!("Custom ({})", format_duration_label(current_ms))
        };
        let _ = AppendMenuW(
            duration_menu,
            if is_preset { MF_STRING } else { MF_STRING | MF_CHECKED },
            MENU_DURATION_CUSTOM,
            PCWSTR(wide(&custom_label).as_ptr()),
        );
        // Truth about what the pill actually applies: while
        // `respect_system_message_duration` is on, the effective duration is
        // the larger of the chosen value and the system message-duration
        // preference, so the menu can check "2 seconds" while the pill stays
        // up for 5. A grayed info line surfaces the effective value when it
        // differs, so the presets never read as silently broken.
        {
            let cfg = state.cfg();
            if cfg.overlay.respect_system_message_duration {
                let effective = crate::config::effective_display_duration(
                    cfg.overlay.duration_ms,
                    crate::winutil::system_preferences().message_duration_ms,
                    true,
                );
                if effective > cfg.overlay.duration_ms {
                    let label = format!("Currently applied: {} (system)", format_duration_label(effective));
                    let _ = AppendMenuW(
                        duration_menu,
                        MF_STRING | MF_DISABLED | MF_GRAYED,
                        0,
                        PCWSTR(wide(&label).as_ptr()),
                    );
                }
            }
        }
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
        let _ = AppendMenuW(
            layout_menu,
            layout_flags(LayoutMode::PersistentCompact),
            MENU_LAYOUT_PERSISTENT_COMPACT,
            PCWSTR(wide("Persistent Compact").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_POPUP, layout_menu.0 as usize, PCWSTR(wide("Layout").as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT_ID, PCWSTR(wide("Quit").as_ptr()));

        let mut point = POINT::default();
        if GetCursorPos(&mut point).is_ok() {
            // The owner must be the foreground window before TrackPopupMenu,
            // or the menu will not disappear when the user clicks away from
            // it or presses Esc (documented Shell_NotifyIcon requirement).
            let _ = SetForegroundWindow(hwnd);
            let command = track_popup_menu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                point.x,
                point.y,
                0,
                hwnd,
                None,
            ) as usize;
            // Re-fetch the state after the modal loop: dispatched messages
            // may have mutated — or, on shutdown, destroyed — the window
            // state while the loop ran; the dispatch below must operate on
            // the current one (the build-phase borrow above ended before
            // the loop).
            let live_ptr = window_state::<MainWindowState>(hwnd);
            if live_ptr.is_null() {
                let _ = post_message(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
                let _ = DestroyMenu(menu);
                return;
            }
            let state = &mut *live_ptr;
            match command {
                MENU_OPEN_ID => state.show_window(),
                // Show the pill now with the current track (or the sample
                // when nothing has played this session), dismissing after
                // the configured duration — the same path the settings
                // "Preview the notification" button uses.
                MENU_PREVIEW_NOTIFY_ID => show_sample(state.overlay_hwnd),
                MENU_NOTIFY_ID => {
                    let new_value = !state.cfg().behavior.notifications_enabled;
                    // Flip the overlay first; persist only when the toggle
                    // reaches it, so the config and the pill can never desync.
                    if post_message(state.overlay_hwnd, TOGGLE_MSG, WPARAM(0), LPARAM(0)).is_err() {
                        error!("posting the notifications toggle to the overlay failed");
                    } else {
                        state.mutate_config(|cfg| cfg.behavior.notifications_enabled = new_value);
                        push_control(
                            &state.control_mailbox,
                            &state.control_tx,
                            ControlCommand::SetNotificationsEnabled(new_value),
                        );
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
                    // Close an open modal child first (the custom-duration
                    // dialog): its modal loop must drain over a live owner
                    // instead of running on after the owner is destroyed
                    // underneath it. Modeless owned popups (the pickers,
                    // the positioner) are destroyed with their owner by the
                    // OS and need no help.
                    crate::duration_dialog::close_if_open();
                    let _ = DestroyWindow(hwnd);
                }
                MENU_DURATION_2S => {
                    state.mutate_config(|cfg| cfg.overlay.duration_ms = 2000);
                    state.push_effective_duration();
                }
                MENU_DURATION_3S => {
                    state.mutate_config(|cfg| cfg.overlay.duration_ms = 3000);
                    state.push_effective_duration();
                }
                MENU_DURATION_5S => {
                    state.mutate_config(|cfg| cfg.overlay.duration_ms = 5000);
                    state.push_effective_duration();
                }
                MENU_DURATION_10S => {
                    state.mutate_config(|cfg| cfg.overlay.duration_ms = 10000);
                    state.push_effective_duration();
                }
                // Custom duration opens a modal dialog so an arbitrary value
                // can be entered; the change is applied the same way as the
                // presets, via the overlay message. The dialog runs its own
                // modal loop, so this frame's re-fetched borrow must not be
                // live across it either: capture what it needs, run the
                // dialog, then fetch once more to apply.
                MENU_DURATION_CUSTOM => {
                    let current_ms = state.cfg().overlay.duration_ms;
                    let chosen = crate::duration_dialog::show_duration_dialog(hwnd, current_ms);
                    let state_ptr = window_state::<MainWindowState>(hwnd);
                    if !state_ptr.is_null() {
                        let state = &mut *state_ptr;
                        if let Some(duration) = chosen {
                            state.mutate_config(|cfg| cfg.overlay.duration_ms = duration);
                            state.push_effective_duration();
                            info!("custom overlay duration set to {duration} ms");
                        }
                    }
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
                MENU_LAYOUT_PERSISTENT_COMPACT => {
                    state.mutate_config(|cfg| cfg.overlay.layout = LayoutMode::PersistentCompact);
                    set_layout(state.overlay_hwnd, LayoutMode::PersistentCompact);
                }
                _ => {}
            }
        }
        // After the modal menu loop, flush the queue with a no-op message so
        // the popup fully tears down when it was dismissed by clicking away.
        let _ = post_message(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

/// The number of equal segments a settings row's control is split into for its
/// segmented controls. Must stay in lockstep with `settings_focus_targets` and
/// the row's click handler.
fn setting_segment_count(id: SettingId) -> usize {
    match id {
        SettingId::Duration => 5,
        SettingId::Layout => 4,
        _ => 1,
    }
}

/// The exact interaction rectangle (client coordinates) of a settings
/// sub-control inside its row's `rect`, from the same geometry the paint and
/// mouse hit-test use (`segment_rects`, `halve`, `position_parts`, or the
/// control split). `None` when the layout has no such sub-control (a stale or
/// foreign encoded tag). Single geometry source for keyboard focus, the focus
/// outline, the click point, and the UIA bounds — the four must never drift
/// apart.
fn setting_sub_rect(id: SettingId, sub: SettingSub, rect: &RECT, scale: f32) -> Option<RECT> {
    let control = row_split(rect, scale).control;
    match sub {
        SettingSub::None => Some(control),
        SettingSub::Seg(i) => segment_rects(&control, setting_segment_count(id), (4.0 * scale) as i32)
            .get(i)
            .copied(),
        SettingSub::Anchor(i) => position_parts(rect, scale).anchors.get(i).copied(),
        SettingSub::Reset => Some(position_parts(rect, scale).reset),
        SettingSub::Adjust => Some(position_parts(rect, scale).adjust),
        SettingSub::Open | SettingSub::OpenConfig => Some(halve(&control, (4.0 * scale) as i32).0),
        SettingSub::Copy | SettingSub::ReloadConfig => Some(halve(&control, (4.0 * scale) as i32).1),
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
/// The point inside a settings row's `rect` that activates `sub` — the center
/// of the exact interaction rectangle (`setting_sub_rect`), so the UIA
/// activation path clicks the same geometry the provider enumerates. `None`
/// when the layout has no such sub-control (a stale or foreign encoded tag).
fn setting_sub_click_point(id: SettingId, sub: SettingSub, rect: &RECT, scale: f32) -> Option<(i32, i32)> {
    setting_sub_rect(id, sub, rect, scale).map(|r| ((r.left + r.right) / 2, (r.top + r.bottom) / 2))
}

/// Applies one settings row's action exactly as the mouse path does: `id` is
/// activated for a click at client `(x, y)` within its `rect`. Shared by the
/// real `WM_LBUTTONDOWN` hit-test and the UIA activation message, so both
/// stay on a single dispatch.
/// Single dispatch point for every way a settings row is activated (mouse
/// click, UIA invoke, UIA toggle); the argument list mirrors the click data.
#[allow(clippy::too_many_arguments)]
fn apply_settings_row_click(hwnd: HWND, id: &SettingId, row_index: usize, rect: &RECT, x: i32, y: i32, scale: f32) {
    // Modal-borrow discipline: this handler can open a modal (the custom-duration
    // dialog), so it owns its window-state borrow itself instead of running
    // under the caller's — the wndproc arm passes only the hwnd, and the
    // borrow taken here is provably dead across the modal scope (the
    // custom-duration arm re-fetches and returns before any later use).
    let state_ptr = window_state::<MainWindowState>(hwnd);
    if state_ptr.is_null() {
        return;
    }
    let state = unsafe { &mut *state_ptr };
    let control_rect = row_split(rect, scale).control;
    let toggle_before = setting_toggle_on(*id, &state.cfg());
    // Capture the row's UIA name before the click mutates the config, so a
    // value change (toggle, segment, anchor, custom duration) can be
    // announced after; a click that leaves the value unchanged
    // (picker-opener, copy/open) no-ops.
    let before_name = setting_row_name(*id, &state.cfg());
    match id {
        SettingId::Notifications => {
            let new_value = !state.cfg().behavior.notifications_enabled;
            // Flip the overlay first; persist only when
            // the toggle reaches it, so the config and
            // the pill can never desync.
            if unsafe { post_message(state.overlay_hwnd, TOGGLE_MSG, WPARAM(0), LPARAM(0)) }.is_err() {
                error!("posting the notifications toggle to the overlay failed");
            } else {
                state.mutate_config(|cfg| cfg.behavior.notifications_enabled = new_value);
                push_control(
                    &state.control_mailbox,
                    &state.control_tx,
                    ControlCommand::SetNotificationsEnabled(new_value),
                );
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
            let segments = segment_rects(&control_rect, 5, (4.0 * scale) as i32);
            let values = [2000u64, 3000, 5000, 10000];
            if let Some((i, _)) = segments.iter().enumerate().find(|(_, s)| x >= s.left && x < s.right) {
                // The Custom tile asks for a value; the
                // chosen one is applied like a preset. The dialog runs its
                // own modal loop: this frame's borrow must be dead across
                // it, so the arm re-fetches and finishes the row (apply,
                // invalidate, announce) on the fresh state and returns —
                // the shared tail below would otherwise keep the borrow
                // alive across the modal scope.
                if i == 4 {
                    let current_ms = state.cfg().overlay.duration_ms;
                    let chosen = crate::duration_dialog::show_duration_dialog(hwnd, current_ms);
                    let state_ptr = window_state::<MainWindowState>(hwnd);
                    if !state_ptr.is_null() {
                        let state = unsafe { &mut *state_ptr };
                        if let Some(duration) = chosen {
                            state.mutate_config(|cfg| cfg.overlay.duration_ms = duration);
                            state.push_effective_duration();
                            info!("custom overlay duration set to {duration} ms");
                        }
                        state.invalidate();
                        raise_settings_name_changed(
                            hwnd,
                            row_index,
                            &before_name,
                            &setting_row_name(*id, &state.cfg()),
                        );
                    }
                    return;
                } else {
                    let duration = values[i];
                    state.mutate_config(|cfg| cfg.overlay.duration_ms = duration);
                    state.push_effective_duration();
                }
                state.invalidate();
            }
        }
        SettingId::RespectSystemDuration => {
            let new_value = !state.cfg().overlay.respect_system_message_duration;
            state.mutate_config(|cfg| cfg.overlay.respect_system_message_duration = new_value);
            // The effective pill duration may change with
            // the flag, so re-push it (and the Duration
            // row's "(system Ns)" suffix updates on the
            // repaint below).
            state.push_effective_duration();
            info!("respect system message duration set: {new_value}");
            state.invalidate();
        }
        SettingId::Layout => {
            let segments = segment_rects(&control_rect, 4, (4.0 * scale) as i32);
            let values = [
                LayoutMode::Expanded,
                LayoutMode::Compact,
                LayoutMode::Auto,
                LayoutMode::PersistentCompact,
            ];
            if let Some((i, _)) = segments.iter().enumerate().find(|(_, s)| x >= s.left && x < s.right) {
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
        SettingId::HideForAutoCompactSources => {
            let new_value = !state.cfg().behavior.hide_for_auto_compact_sources;
            state.mutate_config(|cfg| cfg.behavior.hide_for_auto_compact_sources = new_value);
            set_hide_for_auto_compact_sources(state.overlay_hwnd, new_value);
            info!("hide for auto compact sources set: {new_value}");
            state.invalidate();
        }
        SettingId::FadePersistentPill => {
            let new_value = !state.cfg().overlay.fade_persistent_pill;
            state.mutate_config(|cfg| cfg.overlay.fade_persistent_pill = new_value);
            set_fade_persistent_pill(state.overlay_hwnd, new_value);
            info!("fade persistent pill set: {new_value}");
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
            } else if x >= parts.reset.left && x < parts.reset.right && y >= parts.reset.top && y < parts.reset.bottom {
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
                &[],
                state.auto_sources_result.clone(),
                AUTO_SOURCES_RESULT_MSG,
            ) {
                debug!("auto-compact sources picker failed to open");
            }
        }
        SettingId::PinnedSource => {
            // Single-select picker: the confirmed
            // result holds at most one pattern. The
            // stored pin (a single string) is passed
            // as the current selection slice, and the
            // row set is restricted to the user's
            // Allowed Sources — a pin outside
            // `media_sources` could never fire (the
            // worker excludes non-allowed sessions).
            let current: Vec<String> = state.cfg().behavior.pinned_source.clone().into_iter().collect();
            if !process_picker::open(
                hwnd,
                &control_rect,
                &current,
                &state.cfg().behavior.media_sources,
                state.pinned_source_result.clone(),
                PINNED_SOURCE_RESULT_MSG,
            ) {
                debug!("pinned-source picker failed to open");
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
            } else if x >= parts.reset.left && x < parts.reset.right && y >= parts.reset.top && y < parts.reset.bottom {
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
                state.logs_opened_at = Some(Instant::now());
                unsafe { set_timer(hwnd, TIMER_OPENED_ID, 2000, None) };
                state.invalidate();
            } else {
                state.copy_logs();
            }
        }
        SettingId::OpenConfig => {
            let gap = (4.0 * scale) as i32;
            let (open_rect, _reload_rect) = halve(&control_rect, gap);
            if x >= open_rect.left && x < open_rect.right {
                state.open_config();
                state.config_opened_at = Some(Instant::now());
                unsafe { set_timer(hwnd, TIMER_OPENED_ID, 2000, None) };
                state.invalidate();
            } else {
                state.reload_config();
            }
        }
        SettingId::AllowedApps => {
            if !process_picker::open(
                hwnd,
                &control_rect,
                &state.cfg().behavior.media_sources,
                &[],
                state.picker_result.clone(),
                PICKER_RESULT_MSG,
            ) {
                debug!("process picker failed to open");
            }
        }
    }
    if setting_is_toggle(*id) {
        raise_settings_toggle_event(hwnd, row_index, toggle_before, setting_toggle_on(*id, &state.cfg()));
    }
    // Announce the row's new name when the click changed its value — the
    // settings analogue of the pill's name-changed raise; a no-op otherwise.
    raise_settings_name_changed(hwnd, row_index, &before_name, &setting_row_name(*id, &state.cfg()));
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // The body is panic-contained; a panic logs, posts quit (normal
    // teardown) and answers with DefWindowProcW instead of unwinding across
    // the ABI.
    crate::winutil::guarded_wndproc(hwnd, message, wparam, lparam, "the main window procedure", || unsafe {
        window_proc_body(hwnd, message, wparam, lparam)
    })
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn window_proc_body(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Explorer (re)started and rebuilt the notification area: re-add the
    // tray icon, which Explorer's restart wiped.
    if message == taskbar_created_msg() {
        debug!("Explorer restarted the notification area; re-adding the tray icon");
        match install_tray_icon(hwnd) {
            Ok(()) => {
                // A success while the startup retry budget is still armed
                // (e.g. the timer fired between Explorer's restart and this
                // broadcast): clear it, or the timer's next NIM_ADD would
                // target an already-installed (hwnd, uID) and march the
                // budget to its give-up error line for nothing.
                let state_ptr = window_state::<MainWindowState>(hwnd);
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if state.tray_add_attempts != 0 {
                        state.tray_add_attempts = 0;
                        let _ = kill_timer(hwnd, TRAY_RETRY_TIMER_ID);
                        info!("tray icon re-added after an Explorer restart; retry budget cleared");
                    }
                }
            }
            Err(error) => {
                error!("re-adding the tray icon after an Explorer restart failed: {error}");
                // Explorer will not broadcast again until its next restart,
                // so a failed re-add must re-arm the retry budget rather
                // than leave the app icon-less until then.
                let state_ptr = window_state::<MainWindowState>(hwnd);
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    state.tray_add_attempts = 1;
                    let _ = set_timer(hwnd, TRAY_RETRY_TIMER_ID, TRAY_RETRY_INTERVAL_MS, None);
                }
            }
        }
        return LRESULT(0);
    }
    // Session end (logoff/shutdown): consent through DefWindowProcW, but
    // remove the tray icon first so no ghost icon lingers in a session that
    // is about to disappear — only when the session is *really* ending.
    // wParam == FALSE means another app vetoed the shutdown after our
    // WM_QUERYENDSESSION consent and the session continues; removing the
    // icon there would strand the app without a tray until the next
    // Explorer restart. Best-effort; config saves are atomic renames, so
    // there is nothing else to flush here.
    if message == WM_ENDSESSION {
        if wparam.0 != 0 {
            remove_tray_icon(hwnd);
        }
        return DefWindowProcW(hwnd, message, wparam, lparam);
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
                // The overlay received the raw config at creation; apply the
                // system message-duration preference to its copy now.
                (*state_ptr).push_effective_duration();
            }
            // Color the window title bar with the effective accent (system
            // highlight under high contrast, art accent otherwise) so the
            // frame reads as one theme. Applied here, after the frame is
            // realized, rather than right after CreateWindowExW.
            if !state_ptr.is_null() {
                (*state_ptr).apply_title_bar_color();
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
                // A new client height re-clamps the Settings scroll range and
                // may show/hide the dynamic scrollbar.
                let (client_w, client_h) = client_size(hwnd);
                (*state_ptr).sync_settings_scroll(client_w, client_h);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            if !state_ptr.is_null() {
                (*state_ptr).on_dpi_changed((wparam.0 >> 16) as u32);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TRAY_RETRY_TIMER_ID => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if install_tray_icon(hwnd).is_ok() {
                    info!("tray icon installed on retry {}", state.tray_add_attempts);
                    state.tray_add_attempts = 0;
                    let _ = kill_timer(hwnd, TRAY_RETRY_TIMER_ID);
                } else {
                    state.tray_add_attempts += 1;
                    if state.tray_add_attempts >= TRAY_RETRY_MAX_ATTEMPTS {
                        error!(
                            "the tray icon could not be added after {} attempts over {} ms; \
                             pills are unaffected and an Explorer restart will re-add it",
                            state.tray_add_attempts,
                            TRAY_RETRY_MAX_ATTEMPTS * TRAY_RETRY_INTERVAL_MS
                        );
                        state.tray_add_attempts = 0;
                        let _ = kill_timer(hwnd, TRAY_RETRY_TIMER_ID);
                    }
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_LOGS_ID => {
            unsafe {
                let _ = kill_timer(hwnd, TIMER_LOGS_ID);
            }
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                state.logs_copied_at = None;
                state.invalidate();
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_OPENED_ID => {
            unsafe {
                let _ = kill_timer(hwnd, TIMER_OPENED_ID);
            }
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                state.logs_opened_at = None;
                state.config_opened_at = None;
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
                    free_art_blit(&mut current.icon_blit);
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
                        let info = unsafe { &mut *(lparam.0 as *mut NMTTDISPINFOW) };
                        // Point lpszText at a window-owned buffer instead of
                        // copying into the built-in szText (bounded at 80 u16):
                        // a long details string would otherwise truncate at 79
                        // chars. The buffer stays valid until the next tooltip
                        // request overwrites it, by which point this tooltip is
                        // no longer being shown.
                        info.lpszText = MainWindowState::tooltip_text_buffer(&mut state.tooltip_text, &text);
                        info.hinst = HINSTANCE::default();
                    }
                }
            }
            LRESULT(0)
        }
        WM_VSCROLL => {
            // The dynamic vertical scrollbar drives Settings-pane scrolling;
            // outside the Settings pane the message belongs to the default
            // handler (the Activity listbox has its own scrollbar).
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if state.active_pane == Pane::Settings {
                    let (client_w, client_h) = client_size(state.hwnd);
                    let scale = unsafe { GetDpiForWindow(state.hwnd).max(96) } as f32 / 96.0;
                    let row_h = (34.0 * scale) as i32;
                    let page = (client_h - row_h).max(row_h);
                    let code = (wparam.0 & 0xFFFF) as i32;
                    let delta = match SCROLLBAR_COMMAND(code) {
                        SB_LINEUP => -row_h,
                        SB_LINEDOWN => row_h,
                        SB_PAGEUP => -page,
                        SB_PAGEDOWN => page,
                        // Absolute jumps: a delta that the clamp turns into
                        // the real top/bottom without risking overflow.
                        SB_TOP => -state.settings_scroll_y,
                        SB_BOTTOM => i32::MAX,
                        SB_THUMBPOSITION | SB_THUMBTRACK => {
                            let requested = (wparam.0 >> 16) as i32;
                            requested - state.settings_scroll_y
                        }
                        _ => 0,
                    };
                    state.scroll_settings_by(delta, client_w, client_h);
                    return LRESULT(0);
                }
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_MOUSEWHEEL => {
            // In the Settings pane the wheel scrolls the settings document;
            // everywhere else it falls through so children (the history
            // listbox) and the default handler keep their wheel behavior.
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if state.active_pane == Pane::Settings {
                    let (client_w, client_h) = client_size(state.hwnd);
                    let scale = unsafe { GetDpiForWindow(state.hwnd).max(96) } as f32 / 96.0;
                    let row_h = (34.0 * scale) as i32;
                    // Three rows per wheel notch; wheel-up (positive) scrolls the
                    // content up (toward smaller offsets).
                    let notches = (((wparam.0 >> 16) as i16 as i32) / 120).clamp(-128, 128);
                    state.scroll_settings_by(-notches * 3 * row_h, client_w, client_h);
                    return LRESULT(0);
                }
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_GETOBJECT => {
            // UI Automation asks for a provider with lParam == UiaRootObjectId;
            // MSAA OBJID_* queries keep the DefWindowProcW answer. Only the
            // Settings pane is exposed, and only once the window state exists
            // (installed at WM_NCCREATE, before any UIA query can arrive on a
            // created window). Provider construction is panic-contained so an
            // internal error can never unwind across the OS callback boundary.
            if lparam.0 == UiaRootObjectId as isize
                && !state_ptr.is_null()
                && unsafe { (*state_ptr).active_pane == Pane::Settings }
            {
                let provider = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::accessibility::settings_provider(hwnd)
                }));
                match provider {
                    Ok(Some(provider)) => {
                        return UiaReturnRawElementProvider(hwnd, wparam, lparam, &provider);
                    }
                    Ok(None) => {}
                    Err(panic) => {
                        error!("the settings UIA provider panicked: {panic:?}");
                    }
                }
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
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
                            raise_settings_focus_event(hwnd, new_hover);
                        }
                        let (client_w, client_h) = client_size(hwnd);
                        state.sync_settings_scroll(client_w, client_h);
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
                            let _ = unsafe { post_message(hwnd, WM_LBUTTONDOWN, WPARAM(0), lp) };
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
                    VK_PRIOR => {
                        // PageUp: scroll one viewport up without moving focus.
                        let (_, client_h) = client_size(hwnd);
                        let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
                        let row_h = (34.0 * scale) as i32;
                        state.scroll_settings_by(-(client_h - row_h).max(row_h), client_w, client_h);
                    }
                    VK_NEXT => {
                        // PageDown: scroll one viewport down.
                        let (_, client_h) = client_size(hwnd);
                        let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
                        let row_h = (34.0 * scale) as i32;
                        state.scroll_settings_by((client_h - row_h).max(row_h), client_w, client_h);
                    }
                    VK_HOME => {
                        let (_, client_h) = client_size(hwnd);
                        state.settings_scroll_y = 0;
                        state.sync_settings_scroll(client_w, client_h);
                    }
                    VK_END => {
                        let (_, client_h) = client_size(hwnd);
                        state.settings_scroll_y = i32::MAX;
                        state.sync_settings_scroll(client_w, client_h);
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
                                raise_settings_focus_event(hwnd, new_hover);
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
                    if state.active_pane == Pane::Settings {
                        // Entering Settings re-evaluates whether the document
                        // overflows and shows the dynamic scrollbar if so.
                        let (client_w, client_h) = client_size(hwnd);
                        state.sync_settings_scroll(client_w, client_h);
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
                    // Row index counts rows only, like `settings_hover_at`.
                    let items = state
                        .settings_items(sidebar_w, client_w, pad, scale, state.settings_scroll_y)
                        .items;
                    let mut row_index = 0usize;
                    for item in &items {
                        if let SettingsItem::Row { id, rect } = item
                            && y >= rect.top
                            && y < rect.bottom
                        {
                            apply_settings_row_click(hwnd, id, row_index, rect, x, y, scale);
                            return LRESULT(0);
                        }
                        if matches!(item, SettingsItem::Row { .. }) {
                            row_index += 1;
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_SETTINGS_ACTIVATE_MSG => {
            // A Settings control's stable runtime id, posted by the UIA
            // provider's Invoke/Toggle. Re-resolved against the LIVE layout:
            // a stale provider (scrolled, rebuilt, torn down) finds no row
            // and the activation is dropped instead of acting on whatever
            // control now occupies the old position.
            if !state_ptr.is_null() {
                let state = unsafe { &mut *state_ptr };
                let encoded = wparam.0 as i32;
                if encoded & 0x80 != 0 {
                    let row_index = (encoded >> 8) as usize;
                    if let Some(sub) = setting_sub_from_tag(encoded & 0x7F) {
                        let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
                        let sidebar_w = (SIDEBAR_W * scale).round() as i32;
                        let pad = (PAD * scale) as i32;
                        let (client_w, client_h) = client_size(hwnd);
                        // A Settings control was explicitly activated: bring
                        // the Settings pane up first, exactly like a sidebar
                        // click on it would.
                        if state.active_pane != Pane::Settings {
                            state.active_pane = Pane::Settings;
                            state.apply_pane();
                            state.sync_settings_scroll(client_w, client_h);
                        }
                        let items = state
                            .settings_items(sidebar_w, client_w, pad, scale, state.settings_scroll_y)
                            .items;
                        let mut row = 0usize;
                        for item in &items {
                            if let SettingsItem::Row { id, rect } = item {
                                if row == row_index {
                                    if let Some((x, y)) = setting_sub_click_point(*id, sub, rect, scale) {
                                        apply_settings_row_click(hwnd, id, row_index, rect, x, y, scale);
                                    }
                                    break;
                                }
                                row += 1;
                            }
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
                // Position indicator hover in the Activity pane.
                if state.active_pane == Pane::Activity {
                    let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
                    let x = (lparam.0 & 0xFFFF) as i32;
                    let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
                    let sidebar_w = (SIDEBAR_W * scale).round() as i32;
                    let pad = (PAD * scale) as i32;
                    let (client_w, _) = client_size(hwnd);
                    let content_left = sidebar_w;
                    let art = (ART_SIZE * scale).round() as i32;
                    let art_y = (ART_Y * scale) as i32;
                    let sep_y = art_y + art + (SEP_GAP * scale) as i32;
                    let hist_bottom = sep_y + ((HIST_GAP + HIST_H) * scale) as i32;
                    let pos_y = hist_bottom + (4.0 * scale) as i32;
                    let pos_bottom = pos_y + (16.0 * scale) as i32;
                    let over = x >= content_left + pad && x < client_w - pad && y >= pos_y && y <= pos_bottom;
                    if over != state.position_hover {
                        state.position_hover = over;
                        let pos_rect = RECT {
                            left: content_left + pad,
                            top: pos_y,
                            right: client_w - pad,
                            bottom: pos_bottom,
                        };
                        state.invalidate_rect(&pos_rect);
                    }
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                } else if state.position_hover {
                    state.position_hover = false;
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
                if state.position_hover {
                    state.position_hover = false;
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
                    apply_and_announce_settings_row(state, hwnd, SettingId::AllowedApps, |state| {
                        let command = ControlCommand::SetAllowedSources(patterns.clone());
                        state.mutate_config(|cfg| cfg.behavior.media_sources = patterns);
                        push_control(&state.control_mailbox, &state.control_tx, command);
                        state.invalidate();
                    });
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
                    apply_and_announce_settings_row(state, hwnd, SettingId::AutoCompactApps, |state| {
                        state.mutate_config(|cfg| cfg.behavior.auto_compact_sources = patterns);
                        state.invalidate();
                    });
                }
            }
            LRESULT(0)
        }
        PINNED_SOURCE_RESULT_MSG => {
            // Same contract as PICKER_RESULT_MSG, but for the single-select
            // pinned-source picker: at most one pattern lands in the shared
            // slot and is taken here into `behavior.pinned_source` (an empty
            // result clears the pin). The live overlay keeps its own config
            // snapshot, so the change is pushed there too — the pin only
            // decides what the persistent pill returns to after a dismiss.
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let patterns = state
                    .pinned_source_result
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                if let Some(patterns) = patterns {
                    let pin = patterns.into_iter().next();
                    apply_and_announce_settings_row(state, hwnd, SettingId::PinnedSource, |state| {
                        state.mutate_config(|cfg| cfg.behavior.pinned_source = pin);
                        set_pinned_source(state.overlay_hwnd, state.cfg().behavior.pinned_source.clone());
                        state.invalidate();
                    });
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
        WM_SETTINGCHANGE => {
            // A system preference changed: re-sample animation,
            // overlapped-content, high-contrast and message-duration
            // preferences, rebuild the settings chrome from the new colors,
            // and re-check the effective pill duration — all without a
            // restart. The overlay consults the shared preference snapshot at
            // render time, so the next pill frame picks the change up.
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let prefs = crate::winutil::refresh_system_preferences();
                debug!("re-sampled system preferences after WM_SETTINGCHANGE: {prefs:?}");
                state.rebuild_settings_appearance();
                // The effective accent may have flipped (e.g. high contrast
                // toggled): re-color the title bar from the new state.
                state.apply_title_bar_color();
                state.push_effective_duration();
                state.invalidate();
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_DESTROY => {
            // Disconnect the UIA provider while the window and its state still
            // exist — the same defensive detach the overlay applies.
            crate::accessibility::detach_hwnd_provider(hwnd);
            if !state_ptr.is_null() {
                (*state_ptr).on_destroy();
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_TRAY => {
            let event = lparam.0 as u32;
            if event == WM_RBUTTONUP {
                // The menu owns its window-state borrows itself: the
                // modal loop must never run under this frame's borrow.
                show_tray_menu(hwnd);
            } else if event == WM_LBUTTONDBLCLK && !state_ptr.is_null() {
                (*state_ptr).show_window();
            }
            LRESULT(0)
        }
        WM_SETTINGS_SNAPSHOT_MSG => {
            // A UIA provider thread asked for a fresh Settings snapshot. The
            // UI thread is the only one allowed to read the window-state box,
            // so it builds the snapshot here; the requesting thread waits on
            // the event this build signals.
            if !state_ptr.is_null() {
                build_settings_ui_snapshot(hwnd);
            }
            LRESULT(0)
        }
        WM_SETTINGS_FOCUS_MSG => {
            // A provider SetFocus arrived from a UIA thread; move the focus
            // here, on the owner thread.
            if !state_ptr.is_null() {
                focus_setting_at_body(
                    hwnd,
                    wparam.0,
                    setting_sub_from_tag(lparam.0 as i32).unwrap_or(SettingSub::None),
                );
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // Slot clear first, box second — the canonical order every window
            // applies via the shared helper.
            release_window_state(hwnd, state_ptr);
            // Drop the shared snapshot so a provider that outlives the window
            // can no longer read window data; its next request fails to post
            // and degrades to empty answers. Same teardown contract as the
            // overlay's name-cell null (accessibility::clear_uia_provider_state).
            crate::accessibility::clear_uia_provider_state(&SETTINGS_UI_SNAPSHOT);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Accessibility: the Settings pane is owner-drawn, so UI Automation
// needs an explicit fragment provider. These free functions read the window
// state by `HWND` (the provider runs on UIA's thread, not the wndproc borrow)
// and hand the live layout/focus to `accessibility::settings_provider`.
// ────────────────────────────────────────────────────────────────────────────

/// Human-readable label for a settings row, matching the painted text.
fn setting_label(id: SettingId) -> &'static str {
    match id {
        SettingId::Notifications => "Notifications",
        SettingId::Duration => "Duration",
        SettingId::RespectSystemDuration => "Respect system message duration",
        SettingId::StartOnLogin => "Start on login",
        SettingId::CloseToTray => "Close to tray",
        SettingId::AllowedApps => "Allowed apps",
        SettingId::Layout => "Layout",
        SettingId::Position => "Expanded Position",
        SettingId::SeparateCompact => "Compact Position follows Expanded Position",
        SettingId::CompactPosition => "Compact Position",
        SettingId::DismissOnHover => "Dismiss on hover",
        SettingId::ExpandCompactOnHover => "Expand compact pill on hover",
        SettingId::HideForAutoCompactSources => "Hide for auto-compact sources",
        SettingId::FadePersistentPill => "Fade persistent pill",
        SettingId::PinnedSource => "Pinned source",
        SettingId::Monitor => "Monitor",
        SettingId::ShowSample => "Show sample",
        SettingId::CopyLogs => "Diagnostics",
        SettingId::OpenConfig => "Config",
        SettingId::AutoCompactApps => "Auto-compact apps",
    }
}

/// Whether the row is an ON/OFF toggle (versus a button or segmented control).
fn setting_is_toggle(id: SettingId) -> bool {
    matches!(
        id,
        SettingId::Notifications
            | SettingId::RespectSystemDuration
            | SettingId::StartOnLogin
            | SettingId::CloseToTray
            | SettingId::SeparateCompact
            | SettingId::DismissOnHover
            | SettingId::ExpandCompactOnHover
            | SettingId::HideForAutoCompactSources
            | SettingId::FadePersistentPill
    )
}

/// The UIA name a settings row answers with — the label alone when the
/// value is empty, else "Label: value". Single source for both the
/// provider (`settings_children_from`) and the name-changed announcements,
/// so the announced name can never drift from what a client would read.
fn setting_row_name(id: SettingId, cfg: &Config) -> String {
    let value = setting_value(id, cfg);
    if value.is_empty() {
        setting_label(id).to_string()
    } else {
        format!("{}: {}", setting_label(id), value)
    }
}

/// The current displayed value text for a row (used to build the UIA name).
fn setting_value(id: SettingId, cfg: &Config) -> String {
    match id {
        SettingId::Notifications => on_off(cfg.behavior.notifications_enabled),
        SettingId::RespectSystemDuration => on_off(cfg.overlay.respect_system_message_duration),
        SettingId::StartOnLogin => on_off(cfg.behavior.start_on_login),
        SettingId::CloseToTray => on_off(cfg.behavior.close_to_tray),
        // Polarity is inverted from the persisted field: "ON" means the Compact
        // pill follows the Expanded position (field `false`).
        SettingId::SeparateCompact => on_off(!cfg.overlay.compact_position_separate),
        SettingId::DismissOnHover => on_off(cfg.overlay.dismiss_on_hover),
        SettingId::ExpandCompactOnHover => on_off(cfg.overlay.expand_compact_on_hover),
        SettingId::HideForAutoCompactSources => on_off(cfg.behavior.hide_for_auto_compact_sources),
        SettingId::FadePersistentPill => on_off(cfg.overlay.fade_persistent_pill),
        // No pin is spelled out (like the empty Auto-compact list) so the UIA
        // name never reads a bare "Pinned source:".
        SettingId::PinnedSource => cfg.behavior.pinned_source.clone().unwrap_or_else(|| "None".into()),
        SettingId::ShowSample => "Show a sample notification".into(),
        SettingId::Duration => format_duration_label(cfg.overlay.duration_ms),
        SettingId::Layout => format!("{:?}", cfg.overlay.layout),
        SettingId::Monitor => format!("{:?}", cfg.overlay.monitor),
        SettingId::Position => position_label(cfg),
        SettingId::CompactPosition => compact_position_label(cfg),
        // An empty allow-list means every source is allowed; an empty
        // auto-compact list means none. Spell that out instead of an empty
        // UIA value.
        SettingId::AllowedApps => {
            if cfg.behavior.media_sources.is_empty() {
                "All apps".into()
            } else {
                cfg.behavior.media_sources.join(", ")
            }
        }
        SettingId::AutoCompactApps => {
            if cfg.behavior.auto_compact_sources.is_empty() {
                "None".into()
            } else {
                cfg.behavior.auto_compact_sources.join(", ")
            }
        }
        SettingId::CopyLogs => "Copy logs / Open logs".into(),
        SettingId::OpenConfig => "Open config / Restart app".into(),
    }
}

/// The current toggle state for a row, when it is a toggle.
fn setting_toggle_on(id: SettingId, cfg: &Config) -> bool {
    match id {
        SettingId::Notifications => cfg.behavior.notifications_enabled,
        SettingId::RespectSystemDuration => cfg.overlay.respect_system_message_duration,
        SettingId::StartOnLogin => cfg.behavior.start_on_login,
        SettingId::CloseToTray => cfg.behavior.close_to_tray,
        SettingId::SeparateCompact => !cfg.overlay.compact_position_separate,
        SettingId::DismissOnHover => cfg.overlay.dismiss_on_hover,
        SettingId::ExpandCompactOnHover => cfg.overlay.expand_compact_on_hover,
        SettingId::HideForAutoCompactSources => cfg.behavior.hide_for_auto_compact_sources,
        SettingId::FadePersistentPill => cfg.overlay.fade_persistent_pill,
        _ => false,
    }
}

fn on_off(value: bool) -> String {
    if value { "On" } else { "Off" }.into()
}

/// The Settings content rectangle (client coordinates) for the provider's root
/// bounding box. Null-safe for a state-less window.
pub(crate) fn settings_content_rect(hwnd: HWND) -> RECT {
    let (client_w, client_h) = client_size(hwnd);
    let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
    let sidebar_w = (SIDEBAR_W * scale).round() as i32;
    RECT {
        left: sidebar_w,
        top: 0,
        right: client_w,
        bottom: client_h,
    }
}

/// An immutable picture of the Settings pane for UI Automation. Built on the
/// UI thread (the only thread that may read the window-state box) and shared
/// by `Arc`; provider threads read this snapshot instead of the live state,
/// so their reads can never race the UI thread's writes or deref a box that
/// teardown already freed. Every field is owned, so the snapshot stays valid
/// even after the window is gone.
#[derive(Clone, Default)]
pub(crate) struct SettingsUiSnapshot {
    /// Every focusable control with the fields the provider answers from.
    pub children: Vec<crate::accessibility::SettingChild>,
    /// The currently focused (hovered) control, if any.
    pub focus: Option<(usize, SettingSub)>,
}

/// The id of the thread that owns the window state. Set once at startup;
/// provider helpers use it to detect calls that already run on the UI thread
/// (those read the state directly — a posted request would never be
/// processed by the very thread making it).
static UI_THREAD_ID: OnceLock<u32> = OnceLock::new();

/// The last Settings snapshot the UI thread built, tagged with a strictly
/// increasing build generation. Provider threads wait — bounded — for a
/// generation newer than the one they saw before posting their request, so a
/// read is never answered with a build that predates its own request. The UI
/// thread replaces the slot wholesale under the lock, so a reader always
/// sees one complete snapshot.
///
/// The UI thread is the **only** writer: stores are serialized and strictly
/// monotonic in generation, and the WM_NCDESTROY clear is the terminal write
/// (no store can follow a destroyed window), so the guarded-write question —
/// a stale writer clobbering a newer value — never arises; provider threads
/// only ever read clones of the Arc.
static SETTINGS_UI_SNAPSHOT: Mutex<Option<(u64, Arc<SettingsUiSnapshot>)>> = Mutex::new(None);

/// How long a provider thread waits for the UI thread to rebuild the
/// snapshot. The rebuild normally lands within the same scheduler quantum as
/// the posted message (~1 ms); the bound only bites when the UI thread is
/// wedged or the window is tearing down, and assistive tech must never be
/// stalled indefinitely by either.
const SETTINGS_SNAPSHOT_WAIT_MS: u64 = 250;

/// Signals a requesting provider thread that a fresh snapshot is stored.
/// Auto-reset: one waiter wakes per build; later waiters time out and read
/// the slot, which still holds a complete snapshot.
fn settings_snapshot_event() -> HANDLE {
    // The handle value is inert data — the object it names is kernel-owned and
    // safely shared — so the Send+Sync wrapper is sound for the static.
    struct SnapshotEvent(HANDLE);
    unsafe impl Send for SnapshotEvent {}
    unsafe impl Sync for SnapshotEvent {}
    static EVENT: OnceLock<SnapshotEvent> = OnceLock::new();
    EVENT
        .get_or_init(|| SnapshotEvent(unsafe { CreateEventW(None, false, false, None).unwrap_or_default() }))
        .0
}

/// Records the thread that owns the windows, so provider helpers can tell
/// whether they already run on it. Called from `main` before any window is
/// created.
pub(crate) fn mark_ui_thread() {
    let _ = UI_THREAD_ID.set(unsafe { GetCurrentThreadId() });
}

fn on_ui_thread() -> bool {
    UI_THREAD_ID
        .get()
        .is_some_and(|id| *id == unsafe { GetCurrentThreadId() })
}

/// Stores a snapshot under the next generation and wakes any thread waiting
/// for a build.
fn store_settings_ui_snapshot(snapshot: Arc<SettingsUiSnapshot>) {
    let mut slot = SETTINGS_UI_SNAPSHOT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = slot.as_ref().map(|(generation, _)| *generation + 1).unwrap_or(1);
    *slot = Some((generation, snapshot));
    // Best-effort: a failed signal only costs a waiting thread its next poll
    // slice before it reads the slot anyway.
    unsafe {
        let _ = SetEvent(settings_snapshot_event());
    }
}

/// The generation of the newest stored build, or 0 when none was ever built.
/// A waiter captures it before posting its request and then waits for a
/// strictly newer generation, so a stale signal can never satisfy it.
fn snapshot_generation() -> u64 {
    SETTINGS_UI_SNAPSHOT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|(generation, _)| *generation)
        .unwrap_or(0)
}

/// The newest stored build, or None when nothing was ever built (no window,
/// or teardown cleared it). Clones the Arc so the caller never holds the
/// lock.
fn snapshot_slot() -> Option<(u64, Arc<SettingsUiSnapshot>)> {
    SETTINGS_UI_SNAPSHOT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// The last complete snapshot, or an empty default when none was ever built.
/// A provider must degrade to empty answers, never crash.
fn current_settings_ui_snapshot() -> Arc<SettingsUiSnapshot> {
    snapshot_slot().map(|(_, snapshot)| snapshot).unwrap_or_default()
}

/// Builds the snapshot from the live window state. Only ever called on the
/// UI thread, which owns the box; returns an empty snapshot when the window
/// state is gone.
fn build_settings_ui_snapshot(hwnd: HWND) -> Arc<SettingsUiSnapshot> {
    let state = crate::winutil::window_state::<MainWindowState>(hwnd);
    let snapshot = if state.is_null() {
        SettingsUiSnapshot::default()
    } else {
        let state = unsafe { &*state };
        SettingsUiSnapshot {
            children: settings_children_from(state, hwnd),
            focus: state.settings_hover,
        }
    };
    let snapshot = Arc::new(snapshot);
    store_settings_ui_snapshot(snapshot.clone());
    snapshot
}

/// The snapshot to answer a provider call with. On the UI thread the live
/// state is read directly; anywhere else the UI thread is asked (by message)
/// to rebuild it. The answer is returned once a build strictly newer than
/// the request lands — the posted message is what triggers it — or, after a
/// bounded wait, a failed post, or a window that is gone, from the last
/// complete build. The window-state box is never dereferenced off the UI
/// thread, and assistive tech is never stalled beyond the bound.
fn settings_ui_snapshot(hwnd: HWND) -> Arc<SettingsUiSnapshot> {
    if on_ui_thread() {
        return build_settings_ui_snapshot(hwnd);
    }
    let wanted = snapshot_generation();
    let event = settings_snapshot_event();
    let posted = unsafe { post_message(hwnd, WM_SETTINGS_SNAPSHOT_MSG, WPARAM(0), LPARAM(0)) };
    let deadline = Instant::now() + Duration::from_millis(SETTINGS_SNAPSHOT_WAIT_MS);
    while posted.is_ok() && !event.0.is_null() {
        // The UI thread's answer may already be stored (or a concurrent
        // request's answer — any build newer than ours is equally valid).
        if let Some((generation, snapshot)) = snapshot_slot().as_ref()
            && *generation > wanted
        {
            return Arc::clone(snapshot);
        }
        // The window is gone; the posted message was discarded and no build
        // will ever land.
        if !unsafe { is_window(hwnd) } {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        // Short slices so a destroyed window is noticed promptly; a signal
        // wakes the wait immediately.
        let slice = deadline.saturating_duration_since(now).as_millis().min(50) as u32;
        unsafe { WaitForSingleObject(event, slice) };
    }
    current_settings_ui_snapshot()
}

/// The current keyboard focus, as the provider uses it for `HasKeyboardFocus`.
/// Null-safe: no window state means no focus.
pub(crate) fn settings_focus(hwnd: HWND) -> Option<(usize, SettingSub)> {
    settings_ui_snapshot(hwnd).focus
}

/// A stable small tag per `SettingSub` variant, used to derive UIA runtime ids
/// that survive provider rebuilds. Distinct variants get distinct tags; the
/// `Seg`/`Anchor` indices fold into the same tag space via their payload.
/// Layouts today never exceed a handful of segments or anchors; a row that
/// would overflow the 16-slot tag space is a programming error, so it panics
/// here instead of silently colliding with another control's runtime id.
pub(crate) fn setting_sub_tag(sub: SettingSub) -> i32 {
    match sub {
        SettingSub::None => 0x00,
        SettingSub::Reset => 0x10,
        SettingSub::Adjust => 0x11,
        SettingSub::Open => 0x20,
        SettingSub::Copy => 0x21,
        SettingSub::OpenConfig => 0x22,
        SettingSub::ReloadConfig => 0x23,
        SettingSub::Seg(i) => {
            assert!(i <= 0x0F, "a settings row cannot exceed 16 segments");
            0x40 + i as i32
        }
        SettingSub::Anchor(i) => {
            assert!(i <= 0x0F, "a settings row cannot exceed 16 anchor targets");
            0x50 + i as i32
        }
    }
}

/// The stable UIA runtime id for one focusable Settings control. Same
/// (row, sub) always maps to the same non-zero id.
pub(crate) fn setting_runtime_id(row_index: usize, sub: SettingSub) -> i32 {
    ((row_index as i32) << 8) | 0x80 | setting_sub_tag(sub)
}

/// Inverse of `setting_sub_tag` for the tags this app encodes. Unknown tags
/// (a corrupt or foreign activation message) map to `None`, so a message can
/// never be decoded into a control that does not exist.
pub(crate) fn setting_sub_from_tag(tag: i32) -> Option<SettingSub> {
    match tag {
        0x00 => Some(SettingSub::None),
        0x10 => Some(SettingSub::Reset),
        0x11 => Some(SettingSub::Adjust),
        0x20 => Some(SettingSub::Open),
        0x21 => Some(SettingSub::Copy),
        0x22 => Some(SettingSub::OpenConfig),
        0x23 => Some(SettingSub::ReloadConfig),
        0x40..=0x4F => Some(SettingSub::Seg((tag - 0x40) as usize)),
        0x50..=0x5F => Some(SettingSub::Anchor((tag - 0x50) as usize)),
        _ => None,
    }
}

/// Snapshot of the focusable Settings controls for the UIA provider. Null-safe:
/// an empty list means "nothing to expose". Answered from the UI-thread
/// snapshot; the caller never dereferences the window-state box itself.
pub(crate) fn settings_accessibility_children(hwnd: HWND) -> Vec<crate::accessibility::SettingChild> {
    settings_ui_snapshot(hwnd).children.clone()
}

/// Materializes the focusable Settings controls from a live window state.
/// UI-thread only — `build_settings_ui_snapshot` calls it while holding the
/// state the UI thread owns.
fn settings_children_from(state: &MainWindowState, hwnd: HWND) -> Vec<crate::accessibility::SettingChild> {
    let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
    let (client_w, _) = client_size(hwnd);
    let sidebar_w = (SIDEBAR_W * scale).round() as i32;
    let pad = (PAD * scale) as i32;
    let cfg = state.cfg();
    // Row index -> SettingId (headers are not rows).
    let row_ids: Vec<SettingId> = state
        .settings_items(sidebar_w, client_w, pad, scale, 0)
        .items
        .iter()
        .filter_map(|it| match it {
            SettingsItem::Row { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    state
        .settings_focus_targets(sidebar_w, client_w, pad, scale)
        .into_iter()
        .map(|t| {
            let (name, control_type, toggle) = match row_ids.get(t.row_index).copied() {
                Some(id) => {
                    let name = setting_row_name(id, &cfg);
                    let control_type = if setting_is_toggle(id) {
                        UIA_CheckBoxControlTypeId
                    } else {
                        UIA_ButtonControlTypeId
                    };
                    let toggle = if setting_is_toggle(id) {
                        Some(setting_toggle_on(id, &cfg))
                    } else {
                        None
                    };
                    (name, control_type, toggle)
                }
                None => ("Setting".to_string(), UIA_ButtonControlTypeId, None),
            };
            // The bounds are the control's EXACT interaction rectangle from
            // the shared layout model (never a fabricated box): UIA bounds
            // equal the clickable visual target, so hit-testing and Narrator
            // agree with the painted control and adjacent segments cannot
            // overlap in UIA space.
            crate::accessibility::SettingChild {
                row_index: t.row_index,
                sub: t.sub,
                rect: t.rect,
                name,
                control_type,
                toggle,
                runtime_id: setting_runtime_id(t.row_index, t.sub),
            }
        })
        .collect()
}

/// The row index (rows only, matching the UIA child enumeration and the
/// click handler's count) of the settings row for `id` in the current
/// layout, or None when the row is not laid out. Used to raise name-changed
/// events from the async picker-result paths, where the click-time row is
/// no longer in scope.
fn settings_row_index(state: &MainWindowState, hwnd: HWND, id: SettingId) -> Option<usize> {
    let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
    let (client_w, _) = client_size(hwnd);
    let sidebar_w = (SIDEBAR_W * scale).round() as i32;
    let pad = (PAD * scale) as i32;
    let mut row_index = 0usize;
    for item in &state
        .settings_items(sidebar_w, client_w, pad, scale, state.settings_scroll_y)
        .items
    {
        if let SettingsItem::Row { id: item_id, .. } = item {
            if *item_id == id {
                return Some(row_index);
            }
            row_index += 1;
        }
    }
    None
}

/// Raises the UIA focus-changed event for the Settings control the keyboard
/// cursor moved onto, so assistive tech follows the same focus the window
/// paints. No UIA client listening is normal, so failures log at debug level.
fn raise_settings_focus_event(hwnd: HWND, focus: Option<(usize, SettingSub)>) {
    let Some((row, sub)) = focus else {
        return;
    };
    if hwnd.0.is_null() {
        return;
    }
    if let Some(provider) = crate::accessibility::settings_child_provider(hwnd, row, sub)
        && let Err(error) = unsafe { UiaRaiseAutomationEvent(&provider, UIA_AutomationFocusChangedEventId) }
    {
        debug!("raising the settings focus-changed UIA event failed: {error}");
    }
}

/// Raises the UIA toggle property-changed event for a Settings row whose ON/OFF
/// value just flipped. `before`/`after` are the displayed toggle states.
fn raise_settings_toggle_event(hwnd: HWND, row_index: usize, before: bool, after: bool) {
    if hwnd.0.is_null() || before == after {
        return;
    }
    let Some(provider) = crate::accessibility::settings_child_provider(hwnd, row_index, SettingSub::None) else {
        return;
    };
    let old = windows::Win32::System::Variant::VARIANT::from(if before { ToggleState_On.0 } else { ToggleState_Off.0 });
    let new = windows::Win32::System::Variant::VARIANT::from(if after { ToggleState_On.0 } else { ToggleState_Off.0 });
    if let Err(error) =
        unsafe { UiaRaiseAutomationPropertyChangedEvent(&provider, UIA_ToggleToggleStatePropertyId, &old, &new) }
    {
        debug!("raising the settings toggle UIA event failed: {error}");
    }
}

/// Raises the UIA name property-changed event for a Settings control whose
/// displayed name just changed — the settings-pane analogue of the pill's
/// name-changed raise. The accessible name embeds the current value
/// ("Label: value"), so a value change is a name change; announcing it keeps
/// a screen reader tracking the focused control in sync with what the pane
/// shows after the rebuild. Same fresh-provider-per-event pattern as the
/// toggle event; failures log at debug level.
fn raise_settings_name_changed(hwnd: HWND, row_index: usize, before: &str, after: &str) {
    if hwnd.0.is_null() || before == after {
        return;
    }
    let Some(provider) = crate::accessibility::settings_child_provider(hwnd, row_index, SettingSub::None) else {
        return;
    };
    let old = windows::Win32::System::Variant::VARIANT::from(BSTR::from(before));
    let new = windows::Win32::System::Variant::VARIANT::from(BSTR::from(after));
    if let Err(error) = unsafe { UiaRaiseAutomationPropertyChangedEvent(&provider, UIA_NamePropertyId, &old, &new) } {
        debug!("raising the settings name-changed UIA event failed: {error}");
    }
}

/// Applies a picker result to a settings row and announces the row's new
/// name when the displayed value actually changed. Captures the row's name
/// (and index) before `apply` runs, applies it, then raises name-changed —
/// a no-op when the row is not laid out or the confirmed result left the
/// value unchanged.
fn apply_and_announce_settings_row(
    state: &mut MainWindowState,
    hwnd: HWND,
    id: SettingId,
    apply: impl FnOnce(&mut MainWindowState),
) {
    let row = settings_row_index(state, hwnd, id);
    let before = row.map(|_| setting_row_name(id, &state.cfg()));
    apply(state);
    if let (Some(row), Some(before)) = (row, before) {
        raise_settings_name_changed(hwnd, row, &before, &setting_row_name(id, &state.cfg()));
    }
}

/// Pushes a worker control command into the latest-value control mailbox
/// and posts a best-effort wake-up hint onto the merged signal channel. The
/// worker never polls the shared config anymore, so every behavior change
/// made here must be pushed, or the worker would keep its stale snapshot
/// until the next restart. The mailbox never drops: it keeps the newest
/// value per command kind until the worker drains it at its next
/// event-loop turn — even when a saturated signal queue drops the wake-up
/// (that only costs latency), and even across worker restarts (the mailbox
/// is shared, and a replacement worker drains what its predecessor left).
fn push_control(mailbox: &Arc<Mutex<ControlMailbox>>, wake_tx: &SyncSender<Signal>, command: ControlCommand) {
    mailbox
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(command);
    let _ = wake_tx.try_send(Signal::ControlWake);
}

/// Moves the keyboard focus to a control (used by the provider's SetFocus and
/// Invoke/Toggle). Reuses the focus path including auto-scroll, so the control
/// stays on screen. The focus state lives in the window-state box, which only
/// the UI thread may touch: on the UI thread the move happens in place,
/// anywhere else it is handed off by message.
pub(crate) fn focus_setting_at(hwnd: HWND, row_index: usize, sub: SettingSub) {
    if on_ui_thread() {
        focus_setting_at_body(hwnd, row_index, sub);
        return;
    }
    let tag = setting_sub_tag(sub);
    let _ = unsafe { post_message(hwnd, WM_SETTINGS_FOCUS_MSG, WPARAM(row_index), LPARAM(tag as isize)) };
}

/// The UI-thread half of `focus_setting_at`; runs on the thread that owns the
/// window state (either directly, or via `WM_SETTINGS_FOCUS_MSG`).
fn focus_setting_at_body(hwnd: HWND, row_index: usize, sub: SettingSub) {
    let state = crate::winutil::window_state::<MainWindowState>(hwnd);
    if state.is_null() {
        return;
    }
    let state = unsafe { &mut *state };
    let new_hover = Some((row_index, sub));
    if new_hover == state.settings_hover {
        return;
    }
    let old = state.settings_hover;
    let (client_w, client_h) = client_size(hwnd);
    let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
    let sidebar_w = (SIDEBAR_W * scale).round() as i32;
    let pad = (PAD * scale) as i32;
    // Commit hover only when the pair exists in the live layout: a stale
    // provider snapshot (taken before a settings change removed the row) or
    // a crafted message must not leave hover pointing at a control that does
    // not exist. Mirrors the strict validation on the activate path.
    let targets = state.settings_focus_targets(sidebar_w, client_w, pad, scale);
    let Some(target) = targets.iter().find(|t| t.row_index == row_index && t.sub == sub) else {
        return;
    };
    state.settings_hover = new_hover;
    // Recenter the focused control if it would fall outside the visible band
    // (mirrors `focus_settings_target`).
    let row_h = (34.0 * scale) as i32;
    if target.cy < state.settings_scroll_y + row_h / 2 || target.cy > state.settings_scroll_y + client_h - row_h / 2 {
        state.settings_scroll_y = target.cy - client_h / 2;
        state.sync_settings_scroll(client_w, client_h);
    }
    state.invalidate_hover_rows(client_w, old, new_hover);
    state.invalidate();
    raise_settings_focus_event(hwnd, new_hover);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_menu_fixed_ids_never_intrude_into_the_display_range() {
        // Display entries own the id namespace from MENU_MONITOR_DISPLAY_BASE
        // upward; a fixed command id landing inside that range would be
        // mis-dispatched as a display switch (or vice versa) once enough
        // displays are attached — exactly the Display 5 → Custom-Duration /
        // Display 7 → Preview collisions the old 1023 base produced.
        let fixed = [
            MENU_OPEN_ID,
            MENU_NOTIFY_ID,
            MENU_AUTOSTART_ID,
            MENU_CLOSE_TRAY_ID,
            MENU_QUIT_ID,
            MENU_DURATION_2S,
            MENU_DURATION_3S,
            MENU_DURATION_5S,
            MENU_DURATION_10S,
            MENU_MONITOR_ACTIVE,
            MENU_MONITOR_PRIMARY,
            MENU_LAYOUT_EXPANDED,
            MENU_LAYOUT_COMPACT,
            MENU_LAYOUT_AUTO,
            MENU_LAYOUT_PERSISTENT_COMPACT,
            MENU_DURATION_CUSTOM,
            MENU_PREVIEW_NOTIFY_ID,
        ];
        for id in fixed {
            assert!(
                id < MENU_MONITOR_DISPLAY_BASE,
                "fixed tray id {id} intrudes into the display-entry range starting at {MENU_MONITOR_DISPLAY_BASE}"
            );
        }
    }

    /// A uniquely-named temporary directory removed on drop, so the
    /// settings-save regression can run the full save path against a temp
    /// config instead of touching live `%APPDATA%`.
    struct TempDir {
        dir: std::path::PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("winglance-mainwin-{tag}-{}-{stamp}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn failed_save_shows_a_persistent_banner_cleared_by_a_later_successful_save() {
        use crate::config::test_hooks;
        // The save path must run against a temp file, never live %APPDATA%.
        let guard = TempDir::new("save-fail-banner");
        let config_path = guard.dir.join("config.toml");
        std::fs::write(&config_path, "overlay.duration_ms = 4000\n").unwrap();
        let _ = test_hooks::CONFIG_PATH_OVERRIDE.set(config_path.clone());

        let config = Config::load().expect("the temp config must load");
        let mut state = MainWindowState::new(
            Arc::new(RwLock::new(config)),
            EventQueue::default(),
            HWND::default(),
            HINSTANCE::default(),
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(1);
                tx
            },
            Arc::new(Mutex::new(ControlMailbox::default())),
        );
        let disk_before = std::fs::read(&config_path).unwrap();

        // A failed save: the in-memory change applies, the disk does not move,
        // and the persistent SaveFailed banner appears (category Other for the
        // injected non-OS error).
        test_hooks::set_fail_next_save(&config_path);
        state.mutate_config(|c| c.overlay.duration_ms = 5000);
        assert_eq!(state.config_status, Some(ConfigStatus::SaveFailed(SaveFailKind::Other)));
        assert_eq!(state.cfg().overlay.duration_ms, 5000, "the change applies in memory");
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            disk_before,
            "disk bytes must be untouched after a failed save"
        );

        // The banner is part of the Settings layout whenever the pane is
        // (re)shown, exactly like the Conflict banner — pane hide/show never
        // clears the status (only persist_change writes it).
        let with_banner = MainWindowState::build_settings_layout(
            100,
            800,
            16,
            1.0,
            0,
            Some(ConfigStatus::SaveFailed(SaveFailKind::Other)),
        );
        assert!(
            with_banner
                .items
                .iter()
                .any(|i| matches!(i, SettingsItem::Banner { .. })),
            "the SaveFailed status must render a banner when the pane is shown"
        );
        assert!(with_banner.content_extent > 800, "the banner adds document height");

        // The banner persists across further failed saves.
        test_hooks::set_fail_next_save(&config_path);
        state.mutate_config(|c| c.overlay.duration_ms = 6000);
        assert_eq!(
            state.config_status,
            Some(ConfigStatus::SaveFailed(SaveFailKind::Other)),
            "a second failed save must not clear the banner"
        );

        // A later successful save writes the disk and clears the banner.
        state.mutate_config(|c| c.overlay.duration_ms = 7000);
        assert_eq!(state.config_status, None, "a successful save clears the banner");
        let reloaded: Config = toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(reloaded.overlay.duration_ms, 7000, "the successful save persists");
    }

    /// Reads the BSTR payload of a VT_BSTR VARIANT. The layout is the
    /// windows 0.62 generated `VARIANT` (VARIANT -> VARIANT_0 ->
    /// VARIANT_0_0, whose `bstrVal` union field holds a `ManuallyDrop<BSTR>`)
    /// — pinned here so the name assertions don't depend on the crate's
    /// feature-gated `TryFrom<&VARIANT> for BSTR` conversion.
    fn variant_bstr(variant: &windows::Win32::System::Variant::VARIANT) -> String {
        use windows::Win32::System::Variant::VT_BSTR;
        unsafe {
            let v = &variant.Anonymous.Anonymous;
            assert_eq!(v.vt, VT_BSTR, "expected a BSTR variant");
            // `bstrVal` is a union field (VARIANT_0_0_0), so reading it is
            // unsafe; it holds a ManuallyDrop<BSTR> for a VT_BSTR value.
            let bstr: BSTR = (*v.Anonymous.bstrVal).clone();
            bstr.to_string()
        }
    }

    #[test]
    fn retained_settings_provider_resolves_live_state_and_degrades_after_teardown() {
        // Acceptance: one provider retained across a toggle, a
        // scroll/DPI rebuild, and teardown must answer the CURRENT name,
        // toggle state, focus, and bounds on every query — and unavailable
        // (empty) state after the snapshot is cleared, never stale window
        // data. The provider resolves by (row, sub) identity against the
        // generation-tagged snapshot, so the same COM object tracks every
        // change.
        use std::convert::TryFrom;
        use windows::Win32::UI::Accessibility::{
            IRawElementProviderFragment, IToggleProvider, ToggleState_Off, ToggleState_On,
            UIA_HasKeyboardFocusPropertyId, UIA_NamePropertyId, UIA_TogglePatternId,
        };
        use windows::core::Interface;
        let _guard = SNAPSHOT_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::accessibility::clear_uia_provider_state(&SETTINGS_UI_SNAPSHOT);

        let make_child = |toggle: bool, rect: RECT| crate::accessibility::SettingChild {
            row_index: 3,
            sub: SettingSub::None,
            rect,
            name: format!("Notifications: {}", if toggle { "On" } else { "Off" }),
            control_type: UIA_CheckBoxControlTypeId,
            toggle: Some(toggle),
            runtime_id: setting_runtime_id(3, SettingSub::None),
        };
        // Snapshot 1: toggle ON, control at rect A.
        let rect_a = RECT {
            left: 100,
            top: 200,
            right: 300,
            bottom: 234,
        };
        store_settings_ui_snapshot(Arc::new(SettingsUiSnapshot {
            children: vec![make_child(true, rect_a)],
            focus: None,
        }));

        // Retain ONE provider (UIA core holds it across every later change).
        let provider = crate::accessibility::settings_child_provider(HWND::default(), 3, SettingSub::None)
            .expect("the control must exist in snapshot 1");
        let fragment: IRawElementProviderFragment = provider.cast().unwrap();
        // The windows 0.62 COM interface methods are `unsafe fn`.
        let toggle: IToggleProvider = unsafe { provider.GetPatternProvider(UIA_TogglePatternId) }
            .unwrap()
            .cast()
            .unwrap();

        // Snapshot 1 answers: ON, rect A, current name.
        assert_eq!(unsafe { toggle.ToggleState() }.unwrap().0, ToggleState_On.0);
        let bounds = unsafe { fragment.BoundingRectangle() }.unwrap();
        assert_eq!(
            (bounds.left, bounds.top, bounds.width, bounds.height),
            (100.0, 200.0, 200.0, 34.0),
            "bounds must be the exact rect from the snapshot"
        );
        assert_eq!(
            variant_bstr(&unsafe { provider.GetPropertyValue(UIA_NamePropertyId) }.unwrap()),
            "Notifications: On"
        );

        // The toggle flips and a scroll/DPI rebuild moves the control (rect
        // B): the SAME retained provider must now answer OFF at rect B.
        let rect_b = RECT {
            left: 40,
            top: 40,
            right: 240,
            bottom: 74,
        };
        store_settings_ui_snapshot(Arc::new(SettingsUiSnapshot {
            children: vec![make_child(false, rect_b)],
            focus: None,
        }));
        assert_eq!(
            unsafe { toggle.ToggleState() }.unwrap().0,
            ToggleState_Off.0,
            "no stale toggle state"
        );
        let bounds = unsafe { fragment.BoundingRectangle() }.unwrap();
        assert_eq!(
            (bounds.left, bounds.top, bounds.width, bounds.height),
            (40.0, 40.0, 200.0, 34.0),
            "no stale bounds after the layout change"
        );
        assert_eq!(
            variant_bstr(&unsafe { provider.GetPropertyValue(UIA_NamePropertyId) }.unwrap()),
            "Notifications: Off",
            "no stale name after the toggle"
        );

        // Keyboard focus tracks the live focus cell.
        store_settings_ui_snapshot(Arc::new(SettingsUiSnapshot {
            children: vec![make_child(false, rect_b)],
            focus: Some((3, SettingSub::None)),
        }));
        let focused: bool =
            bool::try_from(&unsafe { provider.GetPropertyValue(UIA_HasKeyboardFocusPropertyId) }.unwrap()).unwrap();
        assert!(focused, "HasKeyboardFocus must follow the live focus cell");

        // Teardown (WM_NCDESTROY clears the snapshot): the SAME retained
        // provider must degrade to unavailable answers, never stale window
        // data.
        crate::accessibility::clear_uia_provider_state(&SETTINGS_UI_SNAPSHOT);
        assert_eq!(
            unsafe { toggle.ToggleState() }.unwrap().0,
            ToggleState_Off.0,
            "no stale toggle state after teardown"
        );
        assert_eq!(
            unsafe { fragment.BoundingRectangle() }.unwrap().width,
            0.0,
            "no stale bounds after teardown"
        );
        assert_eq!(
            variant_bstr(&unsafe { provider.GetPropertyValue(UIA_NamePropertyId) }.unwrap()),
            "",
            "no stale name after teardown"
        );
        let focused: bool =
            bool::try_from(&unsafe { provider.GetPropertyValue(UIA_HasKeyboardFocusPropertyId) }.unwrap()).unwrap();
        assert!(!focused, "no focus after teardown");
        assert!(
            unsafe { provider.GetPatternProvider(UIA_TogglePatternId) }.is_err(),
            "no patterns after teardown"
        );
    }

    #[test]
    fn setting_row_name_pins_the_provider_name_format() {
        // The name-changed raise and the provider both build names through
        // setting_row_name; these golden strings pin the "Label: value"
        // format and the value spellings, so any drift — a format change, a
        // re-spelled value, a reordered label — fails here and becomes a
        // conscious, reviewed edit.
        let mut cfg = Config::default();

        // Toggles: label + On/Off.
        cfg.behavior.notifications_enabled = true;
        assert_eq!(setting_row_name(SettingId::Notifications, &cfg), "Notifications: On");
        cfg.behavior.notifications_enabled = false;
        assert_eq!(setting_row_name(SettingId::Notifications, &cfg), "Notifications: Off");
        cfg.overlay.dismiss_on_hover = true;
        assert_eq!(
            setting_row_name(SettingId::DismissOnHover, &cfg),
            "Dismiss on hover: On"
        );

        // Formatted values.
        cfg.overlay.duration_ms = 5000;
        assert_eq!(setting_row_name(SettingId::Duration, &cfg), "Duration: 5s");
        cfg.overlay.layout = LayoutMode::PersistentCompact;
        assert_eq!(setting_row_name(SettingId::Layout, &cfg), "Layout: PersistentCompact");
        cfg.overlay.monitor = MonitorMode::ActiveWindow;
        assert_eq!(
            setting_row_name(SettingId::Monitor, &cfg),
            "Monitor: ActiveWindow",
            "the UIA name uses the Debug spelling the provider answers with"
        );

        // Position: the anchor label when uncustomized, custom coords when set.
        cfg.overlay.position_x = None;
        cfg.overlay.position_y = None;
        assert_eq!(
            setting_row_name(SettingId::Position, &cfg),
            "Expanded Position: top-center"
        );
        cfg.overlay.position_x = Some(120);
        cfg.overlay.position_y = Some(80);
        assert_eq!(
            setting_row_name(SettingId::Position, &cfg),
            "Expanded Position: Custom (120, 80)"
        );

        // List rows: the empty-list spellings and a populated list.
        cfg.behavior.media_sources = vec![];
        assert_eq!(setting_row_name(SettingId::AllowedApps, &cfg), "Allowed apps: All apps");
        cfg.behavior.media_sources = vec!["spotify".to_string()];
        assert_eq!(setting_row_name(SettingId::AllowedApps, &cfg), "Allowed apps: spotify");
        cfg.behavior.auto_compact_sources = vec![];
        assert_eq!(
            setting_row_name(SettingId::AutoCompactApps, &cfg),
            "Auto-compact apps: None"
        );

        // Pin: None spelled out, or the pin name.
        cfg.behavior.pinned_source = None;
        assert_eq!(setting_row_name(SettingId::PinnedSource, &cfg), "Pinned source: None");
        cfg.behavior.pinned_source = Some("spotify".to_string());
        assert_eq!(
            setting_row_name(SettingId::PinnedSource, &cfg),
            "Pinned source: spotify"
        );
    }

    #[test]
    fn setting_sub_tag_round_trips_through_from_tag() {
        // The UIA focus handoff encodes a control into the message payload via
        // setting_sub_tag and decodes it with setting_sub_from_tag; every
        // variant (including the payload-carrying segments and anchors) must
        // survive the round trip.
        let variants = [
            SettingSub::None,
            SettingSub::Reset,
            SettingSub::Adjust,
            SettingSub::Open,
            SettingSub::Copy,
            SettingSub::OpenConfig,
            SettingSub::ReloadConfig,
            SettingSub::Seg(0),
            SettingSub::Seg(3),
            SettingSub::Anchor(0),
            SettingSub::Anchor(5),
        ];
        for variant in variants {
            assert_eq!(
                setting_sub_from_tag(setting_sub_tag(variant)),
                Some(variant),
                "tag round trip must preserve the control"
            );
        }
        // An unknown tag (a message from a newer build) is rejected as `None`
        // so a foreign message can never name a control that does not exist.
        assert_eq!(setting_sub_from_tag(0x7F), None);
    }

    /// Serializes the tests that touch the shared `SETTINGS_UI_SNAPSHOT`
    /// static: `concurrent_teardown_never_strands_a_provider_read` storms it
    /// from many threads, and `snapshot_generation_advances_with_each_store`
    /// asserts exact generation arithmetic — without the lock they race each
    /// other's interleaved writes on the same static.
    static SNAPSHOT_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn settings_snapshot_defaults_to_empty_when_nothing_is_stored() {
        // A provider asked before the UI thread ever built a snapshot (or after
        // teardown cleared it) must degrade to empty answers, never crash.
        // (The snapshot built by `snapshot_generation_advances_with_each_store`
        // is also empty, so the assertion holds regardless of test order.)
        let snapshot = current_settings_ui_snapshot();
        assert!(snapshot.children.is_empty());
        assert!(snapshot.focus.is_none());
    }

    #[test]
    fn snapshot_generation_advances_with_each_store() {
        let _serialize = SNAPSHOT_TEST_LOCK.lock().unwrap();
        // Each stored build gets a strictly newer generation, so a waiter
        // that captured the old one can tell a fresh build from a stale one.
        let first = Arc::new(SettingsUiSnapshot::default());
        store_settings_ui_snapshot(first);
        let before = snapshot_generation();
        assert!(before > 0, "the first store must land on a non-zero generation");
        store_settings_ui_snapshot(Arc::new(SettingsUiSnapshot::default()));
        assert!(snapshot_generation() > before, "each build must advance the generation");
        // The slot always holds the newest build, tagged with that generation.
        let (generation, stored) = snapshot_slot().expect("a snapshot is stored");
        assert_eq!(generation, snapshot_generation());
        assert!(stored.children.is_empty());
    }

    #[test]
    fn concurrent_teardown_never_strands_a_provider_read() {
        let _serialize = SNAPSHOT_TEST_LOCK.lock().unwrap();
        // Teardown (WM_NCDESTROY) clears the shared snapshot via
        // clear_uia_provider_state while provider threads read it. The
        // storm races a fresh build against the clear, and an injector
        // poisons the lock mid-read. Every reader must finish — recovering
        // the poison through the production read pattern — so a teardown
        // can never strand a provider read.
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_millis(400);

        // Readers: the provider threads. Half use the post-teardown read
        // (current_settings_ui_snapshot); half go through the full
        // settings_ui_snapshot entry, whose post to a gone window fails and
        // falls back to the same slot read.
        let mut readers = Vec::new();
        for use_full_path in [false, true] {
            for _ in 0..4 {
                readers.push(std::thread::spawn(move || {
                    let mut reads = 0usize;
                    while Instant::now() < deadline {
                        if use_full_path {
                            let _ = settings_ui_snapshot(HWND::default());
                        } else {
                            let _ = current_settings_ui_snapshot();
                        }
                        reads += 1;
                    }
                    reads
                }));
            }
        }

        // Builder: stores a fresh build like build_settings_ui_snapshot.
        let builder = std::thread::spawn(move || {
            while Instant::now() < deadline {
                store_settings_ui_snapshot(Arc::new(SettingsUiSnapshot::default()));
            }
        });

        // Teardown: clears the slot like WM_NCDESTROY.
        let teardown = std::thread::spawn(move || {
            while Instant::now() < deadline {
                crate::accessibility::clear_uia_provider_state(&SETTINGS_UI_SNAPSHOT);
            }
        });

        // Poison injector: panics while holding the lock mid-read, so every
        // later lock in the storm must go through the recovery path. Bounded
        // so the panic hook's stderr output stays small.
        let injector = std::thread::spawn(move || {
            let mut injections = 0u32;
            while injections < 8 && Instant::now() < deadline {
                let _ = std::panic::catch_unwind(|| {
                    let _guard = SETTINGS_UI_SNAPSHOT.lock().unwrap();
                    std::thread::sleep(Duration::from_millis(1));
                    panic!("injected poison while holding the snapshot lock");
                });
                injections += 1;
            }
            injections
        });

        // All threads must terminate: a reader stranded on the poisoned lock
        // would hang these joins forever.
        for handle in readers {
            let _ = handle.join().expect("a provider read must never be stranded");
        }
        builder.join().expect("the builder must finish");
        teardown.join().expect("the teardown must finish");
        injector.join().expect("the injector must finish");

        // Teardown wins in the end: one final clear empties the slot and the
        // production read degrades to the empty default.
        crate::accessibility::clear_uia_provider_state(&SETTINGS_UI_SNAPSHOT);
        assert!(snapshot_slot().is_none());
        assert!(current_settings_ui_snapshot().children.is_empty());
    }

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
            ..TrackInfo::default()
        }
    }

    #[test]
    fn settings_layout_is_a_pure_function_of_geometry_and_banner() {
        // The memoized `settings_items` wrapper keys on every input, so the
        // pure builder must depend only on (geometry, scroll, banner status):
        // identical inputs yield identical layouts, scroll shifts every rect
        // by exactly the offset without touching the extent, and the banner
        // is the only config-dependent item.
        fn first_row_top(layout: &SettingsLayout) -> i32 {
            layout
                .items
                .iter()
                .find_map(|item| match item {
                    SettingsItem::Row { rect, .. } => Some(rect.top),
                    _ => None,
                })
                .expect("the layout has rows")
        }
        let base = MainWindowState::build_settings_layout(100, 800, 16, 1.0, 0, None);
        let same = MainWindowState::build_settings_layout(100, 800, 16, 1.0, 0, None);
        let scrolled = MainWindowState::build_settings_layout(100, 800, 16, 1.0, 30, None);
        let banner = MainWindowState::build_settings_layout(100, 800, 16, 1.0, 0, Some(ConfigStatus::Conflict));
        assert_eq!(base.items.len(), same.items.len());
        assert_eq!(base.content_extent, same.content_extent);
        assert_eq!(
            first_row_top(&scrolled),
            first_row_top(&base) - 30,
            "scroll must shift every rect up by the offset"
        );
        assert_eq!(
            base.content_extent, scrolled.content_extent,
            "scroll must not affect the extent"
        );
        assert_eq!(
            banner.items.len(),
            base.items.len() + 1,
            "the banner is the only config-dependent item"
        );
        assert!(banner.content_extent > base.content_extent);
    }

    #[test]
    fn setting_runtime_ids_are_unique_stable_and_nonzero() {
        // Every (row, sub) pair a focus target can have maps to the same
        // non-zero id on every call, and distinct pairs never collide — UIA
        // clients key elements by runtime id across provider rebuilds.
        let subs = [
            SettingSub::None,
            SettingSub::Reset,
            SettingSub::Adjust,
            SettingSub::Open,
            SettingSub::Copy,
            SettingSub::OpenConfig,
            SettingSub::ReloadConfig,
            SettingSub::Seg(0),
            SettingSub::Seg(1),
            SettingSub::Seg(4),
            SettingSub::Anchor(0),
            SettingSub::Anchor(5),
            SettingSub::Anchor(8),
        ];
        let mut seen = std::collections::HashSet::new();
        for row in 0usize..32 {
            for sub in &subs {
                let id = setting_runtime_id(row, *sub);
                assert_ne!(id, 0, "runtime id must never be zero");
                assert_eq!(id, setting_runtime_id(row, *sub), "runtime id must be stable");
                assert!(seen.insert(id), "duplicate runtime id {id} for row {row}");
            }
        }
    }

    #[test]
    fn clamp_settings_scroll_stays_within_the_reachable_range() {
        // Content shorter than the viewport: no scrolling is possible.
        assert_eq!(clamp_settings_scroll(0, 400, 900), 0);
        assert_eq!(clamp_settings_scroll(500, 400, 900), 0);
        assert_eq!(clamp_settings_scroll(-200, 400, 900), 0);
        // Content taller than the viewport: offset is clamped to the bottom
        // (document bottom flush with viewport bottom) and never negative.
        assert_eq!(clamp_settings_scroll(0, 1200, 900), 0);
        assert_eq!(clamp_settings_scroll(100, 1200, 900), 100);
        assert_eq!(clamp_settings_scroll(5000, 1200, 900), 300);
        assert_eq!(clamp_settings_scroll(-50, 1200, 900), 0);
    }

    #[test]
    fn settings_layout_offsets_every_rect_by_the_scroll_and_reports_extent() {
        let state = MainWindowState::new(
            Arc::new(RwLock::new(Config::default())),
            EventQueue::default(),
            HWND::default(),
            HINSTANCE::default(),
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(1);
                tx
            },
            Arc::new(Mutex::new(ControlMailbox::default())),
        );
        let scale = 1.0;
        let client_w = 1000;
        let pad = (PAD * scale) as i32;
        let sidebar_w = (SIDEBAR_W * scale) as i32;
        let none = state.settings_items(sidebar_w, client_w, pad, scale, 0);
        let scrolled = state.settings_items(sidebar_w, client_w, pad, scale, 120);
        assert!(none.content_extent > 0);
        // Every row shifts up by exactly the scroll offset; geometry otherwise
        // is identical.
        let rows_none: Vec<_> = none
            .items
            .iter()
            .filter_map(|i| match i {
                SettingsItem::Row { rect, .. } => Some(rect.top),
                _ => None,
            })
            .collect();
        let rows_scrolled: Vec<_> = scrolled
            .items
            .iter()
            .filter_map(|i| match i {
                SettingsItem::Row { rect, .. } => Some(rect.top),
                _ => None,
            })
            .collect();
        assert_eq!(rows_none.len(), rows_scrolled.len());
        for (a, b) in rows_none.iter().zip(&rows_scrolled) {
            assert_eq!(*b, *a - 120);
        }
    }

    #[test]
    fn every_settings_focus_target_can_scroll_into_view() {
        // At every accepted DPI scale and the small/large client heights in the
        // tested viewport grid (768/900/1080 tall at 100/150/200% scale),
        // recentering on a focus target and clamping keeps that target inside
        // the visible band — so keyboard navigation can always bring a control
        // on screen.
        let state = MainWindowState::new(
            Arc::new(RwLock::new(Config::default())),
            EventQueue::default(),
            HWND::default(),
            HINSTANCE::default(),
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(1);
                tx
            },
            Arc::new(Mutex::new(ControlMailbox::default())),
        );
        let client_w = 1000;
        for scale in [1.0f32, 1.5, 2.0] {
            let pad = (PAD * scale) as i32;
            let sidebar_w = (SIDEBAR_W * scale).round() as i32;
            let extent = state.settings_content_extent(sidebar_w, client_w, pad, scale);
            let targets = state.settings_focus_targets(sidebar_w, client_w, pad, scale);
            assert!(!targets.is_empty(), "focus targets must exist at scale {scale}");
            for client_h in [768i32, 900, 1080] {
                for t in &targets {
                    let scroll = clamp_settings_scroll(t.cy - client_h / 2, extent, client_h);
                    let on_screen = t.cy - scroll;
                    assert!(
                        on_screen >= 0 && on_screen < client_h,
                        "target at cy={} not reachable at scale {scale}, client_h {client_h} (extent {extent})",
                        t.cy
                    );
                }
            }
        }
    }

    #[test]
    fn settings_focus_rects_are_exact_non_overlapping_and_lockstep_with_hit_testing() {
        // The UIA bounds are built from the focus rects, so these are the
        // Acceptance: at every DPI scale and narrow/wide client
        // widths, every focus rect is the exact clickable target (its center
        // hit-tests back to itself), no two rects overlap (UIA hit-testing
        // can never resolve to the wrong control), and a point just outside a
        // sub-control's edge never resolves to that control.
        let state = MainWindowState::new(
            Arc::new(RwLock::new(Config::default())),
            EventQueue::default(),
            HWND::default(),
            HINSTANCE::default(),
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(1);
                tx
            },
            Arc::new(Mutex::new(ControlMailbox::default())),
        );
        let overlap = |a: &RECT, b: &RECT| a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
        for scale in [1.0f32, 1.5, 2.0] {
            for client_w in [800i32, 1600] {
                let pad = (PAD * scale) as i32;
                let sidebar_w = (SIDEBAR_W * scale).round() as i32;
                let targets = state.settings_focus_targets(sidebar_w, client_w, pad, scale);
                assert!(!targets.is_empty(), "focus targets must exist at scale {scale}");
                for t in &targets {
                    assert!(
                        t.rect.right > t.rect.left && t.rect.bottom > t.rect.top,
                        "degenerate focus rect {:?} at scale {scale}, width {client_w}",
                        t.rect
                    );
                    assert_eq!(
                        (t.cx, t.cy),
                        ((t.rect.left + t.rect.right) / 2, (t.rect.top + t.rect.bottom) / 2),
                        "the click point must be the exact rect's center"
                    );
                    assert_eq!(
                        state.settings_hover_at(t.cx, t.cy, sidebar_w, client_w, pad, scale),
                        Some((t.row_index, t.sub)),
                        "the rect's center must hit-test back to the same control"
                    );
                }
                for (i, a) in targets.iter().enumerate() {
                    for b in targets.iter().skip(i + 1) {
                        assert!(
                            !overlap(&a.rect, &b.rect),
                            "overlapping focus rects at scale {scale}, width {client_w}: {:?} vs {:?}",
                            a.rect,
                            b.rect
                        );
                    }
                }
                // A point just outside a sub-control's right edge must never
                // resolve back to that control (hit-testing uses x < right,
                // so the rects are half-open — the painted edge belongs to
                // the control, the next pixel does not). Whole-row controls
                // are excluded: their rect is the row's control band, and a
                // point outside it is still inside the row, which
                // `settings_hover_at` reports as the same (row, None).
                for t in targets.iter().filter(|t| t.sub != SettingSub::None) {
                    let outside = (t.rect.right, (t.rect.top + t.rect.bottom) / 2);
                    let hit = state.settings_hover_at(outside.0, outside.1, sidebar_w, client_w, pad, scale);
                    assert_ne!(
                        hit,
                        Some((t.row_index, t.sub)),
                        "a point just outside {:?} must not hit it",
                        t.rect
                    );
                }
            }
        }
    }

    #[test]
    fn the_real_window_proc_survives_a_benign_message_without_a_window() {
        // Drive the actual ABI entry point with a no-op message on a
        // null window — the state pointer is null (guarded), so the body
        // falls through to DefWindowProcW. This pins the wrapper/body split:
        // the extern fn must stay the thin guarded shim.
        let result = unsafe { window_proc(HWND::default(), WM_NULL, WPARAM(0), LPARAM(0)) };
        assert_eq!(result.0, 0);
    }

    #[test]
    fn settings_state_text_reaches_the_aa_floor_and_the_focus_outline_reaches_3_to_1() {
        // The raw faint gray managed only ~4.0:1 against the surface;
        // the effective color set lifts it through the shared contrast
        // helper to the 4.5:1 AA floor, and the dedicated focus outline
        // clears the 3:1 non-text benchmark — both against the surface AND
        // the hover fill the outline can sit on.
        let colors = settings_colors_for(&crate::winutil::SystemPreferences::DEFAULT);
        let ratio = |a: [u8; 4], b: [u8; 4]| crate::overlay::contrast_ratio([a[0], a[1], a[2]], [b[0], b[1], b[2]]);
        assert!(
            ratio(colors.faint, colors.surface) >= 4.5,
            "faint {} vs surface",
            ratio(colors.faint, colors.surface)
        );
        assert!(
            ratio(colors.muted, colors.surface) >= 4.5,
            "muted {} vs surface",
            ratio(colors.muted, colors.surface)
        );
        assert!(
            ratio(colors.warn, colors.surface) >= 4.5,
            "warn {} vs surface",
            ratio(colors.warn, colors.surface)
        );
        assert!(ratio(colors.focus, colors.surface) >= 3.0);
        assert!(ratio(colors.focus, colors.hover) >= 3.0);
        // The raw constant documents why the lift exists.
        assert!(ratio(SETTINGS_FAINT, SETTINGS_SURFACE) < 4.5);
    }

    #[test]
    fn high_contrast_preferences_switch_the_settings_colors_to_system_values() {
        // A high-contrast preference replaces the fixed dark theme
        // with the live system window surface and re-derives every checked
        // color against it (the ratios still hold by construction).
        let mut prefs = crate::winutil::SystemPreferences::DEFAULT;
        prefs.high_contrast = true;
        let colors = settings_colors_for(&prefs);
        assert_eq!(colors.surface, crate::winutil::system_window_color());
        assert_ne!(colors.surface, SETTINGS_SURFACE);
        let ratio = |a: [u8; 4], b: [u8; 4]| crate::overlay::contrast_ratio([a[0], a[1], a[2]], [b[0], b[1], b[2]]);
        assert!(ratio(colors.faint, colors.surface) >= 4.5);
        assert!(ratio(colors.muted, colors.surface) >= 4.5);
        assert!(ratio(colors.focus, colors.surface) >= 3.0);
    }

    #[test]
    fn setting_labels_are_unique_and_values_non_empty() {
        // The UIA provider derives element names from label + value; a
        // duplicate or empty name would make controls indistinguishable to
        // Narrator.
        let cfg = Config::default();
        let ids = [
            SettingId::Notifications,
            SettingId::Duration,
            SettingId::StartOnLogin,
            SettingId::CloseToTray,
            SettingId::AllowedApps,
            SettingId::Layout,
            SettingId::Position,
            SettingId::SeparateCompact,
            SettingId::CompactPosition,
            SettingId::DismissOnHover,
            SettingId::ExpandCompactOnHover,
            SettingId::HideForAutoCompactSources,
            SettingId::FadePersistentPill,
            SettingId::PinnedSource,
            SettingId::Monitor,
            SettingId::ShowSample,
            SettingId::CopyLogs,
            SettingId::OpenConfig,
            SettingId::AutoCompactApps,
        ];
        let mut labels = std::collections::HashSet::new();
        for id in ids {
            assert!(!setting_label(id).is_empty());
            assert!(
                labels.insert(setting_label(id)),
                "duplicate label {}",
                setting_label(id)
            );
            assert!(
                !setting_value(id, &cfg).is_empty(),
                "empty value for {}",
                setting_label(id)
            );
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
    fn resolve_track_state_prefers_snapshot_then_source_cache() {
        // A new activity derives its state from the authoritative snapshot,
        // never by defaulting to Playing (which would mask a pause/stop that
        // rode the TrackChanged because the worker suppressed its paired state
        // event in the same batch).
        let mk = |state: Option<PlaybackState>| TrackInfo {
            source_app: "spotify".to_string(),
            playback_state: state,
            ..TrackInfo::default()
        };
        let mut cache: HashMap<String, PlaybackState> = HashMap::new();
        cache.insert("spotify".to_string(), PlaybackState::Paused);
        // Snapshot wins over the remembered source state.
        assert_eq!(
            MainWindowState::resolve_track_state(&mk(Some(PlaybackState::Stopped)), &cache),
            PlaybackState::Stopped
        );
        // A None snapshot falls through to the source cache.
        assert_eq!(
            MainWindowState::resolve_track_state(&mk(None), &cache),
            PlaybackState::Paused
        );
        // A None snapshot with no cache falls back to Playing.
        let empty: HashMap<String, PlaybackState> = HashMap::new();
        assert_eq!(
            MainWindowState::resolve_track_state(&mk(None), &empty),
            PlaybackState::Playing
        );
    }

    #[test]
    fn accent_from_art_uses_the_album_palette_and_falls_back() {
        let fallback = [240, 110, 155, 255];
        // No artwork, no worker palette: the configured accent stands in for
        // both.
        assert_eq!(accent_from_art(None, None, fallback), (fallback, fallback));
        // Truncated/garbage bytes (not pixel-aligned): same fallback.
        assert_eq!(
            accent_from_art(Some(&[0, 0, 255]), None, fallback),
            (fallback, fallback)
        );
        // A solid white cover (premultiplied BGRA) yields a palette: the
        // primary and secondary leave the pink fallback behind, and the
        // monochrome palette keeps both equal.
        let white: Vec<u8> = vec![255u8; 8 * 8 * 4];
        let (primary, secondary) = accent_from_art(Some(&white), None, fallback);
        assert_ne!(primary, fallback, "a cover must recolor the accent");
        assert_ne!(secondary, fallback, "a cover must recolor the secondary");
        assert_eq!(primary, secondary, "monochrome art keeps primary == secondary");
        // The worker's identity-stable palette wins over a re-derivation: the
        // primary still passes through the contrast guard against the
        // settings surface, the secondary passes untouched.
        let explicit = crate::palette::Palette {
            primary: [0x12, 0x34, 0x56, 0xFF],
            secondary: [0x65, 0x43, 0x21, 0xFF],
        };
        assert_eq!(
            accent_from_art(None, Some(explicit), fallback),
            (
                crate::overlay::ensure_contrast(explicit.primary, SETTINGS_SURFACE, crate::overlay::TEXT_CONTRAST_AA),
                explicit.secondary,
            )
        );
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

    #[test]
    fn redundant_state_rows_mirror_the_overlay_suppression() {
        // The predicate decides row highlighting: a state that repeats what
        // the activity already displays is exactly what the overlay
        // suppresses, so it records grey; any real flip reaches the pill and
        // highlights.
        let (playing, paused, stopped) = (PlaybackState::Playing, PlaybackState::Paused, PlaybackState::Stopped);
        assert!(redundant_state_row(playing, playing));
        assert!(redundant_state_row(paused, paused));
        assert!(redundant_state_row(stopped, stopped));
        assert!(!redundant_state_row(paused, playing));
        assert!(!redundant_state_row(playing, paused));
        assert!(!redundant_state_row(stopped, playing));
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

    #[test]
    fn history_top_after_insert_follows_new_rows_at_top_and_holds_position() {
        // At the top (header or first data row) the newest row stays in view.
        assert_eq!(MainWindowState::history_top_after_insert(0, 10), 0);
        assert_eq!(MainWindowState::history_top_after_insert(1, 10), 1);
        // Reading lower down: the row under the cursor shifts down by one, not
        // to the newest row.
        assert_eq!(MainWindowState::history_top_after_insert(2, 10), 3);
        assert_eq!(MainWindowState::history_top_after_insert(5, 10), 6);
        // A reader at the very bottom clamps to the post-insert last index.
        assert_eq!(MainWindowState::history_top_after_insert(9, 10), 10);
        // An empty listbox (no rows yet) keeps the top at 0.
        assert_eq!(MainWindowState::history_top_after_insert(0, 0), 0);
    }

    #[test]
    fn tooltip_text_buffer_holds_the_full_string_and_is_nul_terminated() {
        let mut buffer = Vec::new();
        // A 256-char details string must not be truncated by the built-in
        // szText (bounded at 80 u16) — the window-owned buffer keeps it whole.
        let long = "x".repeat(256);
        let ptr = MainWindowState::tooltip_text_buffer(&mut buffer, &long);
        assert!(!ptr.is_null());
        assert_eq!(buffer.len(), 257, "256 chars plus the trailing NUL");
        assert_eq!(buffer[256], 0);
        assert_eq!(String::from_utf16_lossy(&buffer[..256]), long);
        // A later, shorter request reuses the same buffer in place.
        let short = "y".repeat(40);
        let _ = MainWindowState::tooltip_text_buffer(&mut buffer, &short);
        assert_eq!(buffer.len(), 41);
        assert_eq!(String::from_utf16_lossy(&buffer[..40]), short);
    }
}
