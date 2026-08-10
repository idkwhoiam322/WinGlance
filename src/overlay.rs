use crate::config::{Config, HorizontalPosition, LayoutMode, MonitorMode, VerticalPosition};
use crate::events::{
    MEDIA_EVENT_MSG, MediaEvent, PlaybackState, TOGGLE_MSG, TrackInfo, artwork_same, media_event_into_owned,
};
use crate::gdi::FontProvider;
use crate::palette::Palette;
use crate::winutil::{clear_window_state, set_window_state, wide, window_state};
use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::BOOLEAN;
use windows::Win32::Foundation::{
    BOOL, COLORREF, HANDLE, HINSTANCE, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC, CreateDIBSection, DEVMODEW, DIB_RGB_COLORS,
    DT_CALCRECT, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW,
    ENUM_CURRENT_SETTINGS, ETO_CLIPPED, EnumDisplayMonitors, EnumDisplaySettingsW, ExtTextOutW, GdiFlush,
    GetMonitorInfoW, HBITMAP, HDC, HFONT, HGDIOBJ, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW,
    MonitorFromWindow, SelectObject, SetBkMode, SetTextColor, TRANSPARENT, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateTimerQueueTimer, DeleteTimerQueueTimer, WT_EXECUTEDEFAULT};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, GWL_EXSTYLE, GetClassNameW, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, HTTRANSPARENT, HWND_TOPMOST, IsIconic, IsWindowVisible,
    KillTimer, MA_NOACTIVATE, MONITORINFOF_PRIMARY, MSG, PM_REMOVE, PeekMessageW, PostMessageW, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW, SetTimer, SetWindowPos, ShowWindow, ULW_ALPHA, WM_APP, WM_DESTROY, WM_DISPLAYCHANGE,
    WM_MOUSEACTIVATE, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_TIMER, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

const TIMER_DEBOUNCE: usize = 1;
/// Window-timer ID used only when the timer-queue fallback is active.
const ANIM_TIMER_ID: usize = 2;
/// One-shot window-timer ID that releases the frame pipeline buffers after
/// the pill has stayed hidden for `IDLE_BUFFER_RELEASE_MS` (see `hide`).
const IDLE_BUFFER_TIMER_ID: usize = 3;
const IDLE_BUFFER_RELEASE_MS: u32 = 30_000;
const LIGHT_DURATION: Duration = Duration::from_millis(120);
/// Remaining time left on the current pill when something newer wants the
/// screen: hovering over the pill or a queued update both cap the exit at
/// this, so the user never waits out the full duration to see a change.
const EARLY_EXIT_MS: u64 = 500;
/// Tick period while the pill is fully static (no animation, no marquee
/// scrolling). The dismiss countdown and hover polling do not need frame
/// rate; the refresh-rate timer is restored the moment the pill animates or
/// a line scrolls.
const STATIC_TICK_MS: u32 = 250;

/// Posted by the high-resolution animation timer to drive pill frames.
const TIMER_ANIMATION_MSG: u32 = WM_APP + 6;

/// Samples the monitor's current refresh period in ms, so the animation timer
/// ticks once per presented frame on any display (60 Hz → 16 ms, 120 Hz → 8 ms,
/// 144 Hz → 7 ms, 240 Hz → 4 ms). The pill can target a display other than the
/// foreground window's (see `MonitorMode`), so when a target was resolved the
/// DWM query runs against the overlay window itself: DWM reports the compose
/// rate of the monitor the queried window is on, and while animating the
/// overlay sits on the target display. Prefers DWM's live compose rate, which
/// stays correct on variable-refresh-rate monitors; falls back to the display
/// mode's nominal frequency of the target (resolved by device name, so no
/// window is needed); last resort is 16 ms (60 Hz).
fn refresh_period_ms(target: Option<&TargetMonitor>, overlay_hwnd: HWND) -> u32 {
    let dwm_hwnd = match target {
        Some(_) => overlay_hwnd,
        None => unsafe { GetForegroundWindow() },
    };
    let dwm_period = unsafe {
        let mut timing = std::mem::zeroed::<DWM_TIMING_INFO>();
        timing.cbSize = std::mem::size_of::<DWM_TIMING_INFO>() as u32;
        DwmGetCompositionTimingInfo(dwm_hwnd, &mut timing).ok().and_then(|()| {
            let ratio = timing.rateRefresh;
            // Refresh rate = numerator / denominator (Hz); 0/0 means DWM
            // did not report a rate (e.g. composition paused).
            if ratio.uiNumerator != 0 && ratio.uiDenominator != 0 {
                Some(1000 * ratio.uiDenominator / ratio.uiNumerator)
            } else {
                None
            }
        })
    };
    if let Some(period) = dwm_period {
        return period.clamp(1, 100);
    }
    // Fallback: the monitor's nominal frequency, resolved by device name so
    // it hits the same display the pill will be shown on.
    let mode_period = match target {
        Some(target) => monitor_frequency_ms(target.handle),
        None => {
            let foreground = unsafe { GetForegroundWindow() };
            let monitor = unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) };
            if monitor.0.is_null() {
                None
            } else {
                monitor_frequency_ms(monitor)
            }
        }
    };
    mode_period.unwrap_or(16).clamp(1, 100)
}

/// The display's nominal frame period in ms from its current display mode;
/// `None` when the mode cannot be read.
fn monitor_frequency_ms(monitor: HMONITOR) -> Option<u32> {
    if monitor.0.is_null() {
        return None;
    }
    let mut info = MONITORINFOEXW::default();
    // cbSize must cover the extended structure (with szDevice), or Windows
    // may not populate the device name and the refresh-rate fallback below
    // silently degrades to 16 ms.
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if !unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
        return None;
    }
    let mut devmode = unsafe { std::mem::zeroed::<DEVMODEW>() };
    devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    let read =
        unsafe { EnumDisplaySettingsW(PCWSTR(info.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut devmode).as_bool() };
    if !read {
        return None;
    }
    1000u32.checked_div(devmode.dmDisplayFrequency as u32)
}

/// A snapshot of one active display, in `EnumDisplayMonitors` order — the
/// same order Windows uses in display settings. Re-enumerated on every use
/// (handles are never cached), so a hot-plugged or reordered display is
/// picked up immediately.
pub(crate) struct DisplayInfo {
    pub handle: HMONITOR,
    /// The display's work area (excludes taskbars and app bars) in virtual
    /// screen coordinates.
    pub work: RECT,
    /// Whether Windows flags this as the primary display.
    pub primary: bool,
    /// The device name (`\\.\DISPLAY1`), as reported by the system.
    pub name: String,
}

/// Enumerates every active display. Returns an empty vec only when the system
/// currently reports no display (for example, a locked or disconnected
/// session).
pub(crate) fn enumerate_displays() -> Vec<DisplayInfo> {
    let mut displays: Vec<DisplayInfo> = Vec::new();
    unsafe extern "system" fn collect(monitor: HMONITOR, _hdc: HDC, _rect: *mut RECT, data: LPARAM) -> BOOL {
        let displays = unsafe { &mut *(data.0 as *mut Vec<DisplayInfo>) };
        let mut info = MONITORINFOEXW::default();
        // cbSize must cover the extended structure (with szDevice) or the
        // device name below is not populated.
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
            // Keep enumerating: one failed read must not drop the rest.
            return true.into();
        }
        let name_len = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        displays.push(DisplayInfo {
            handle: monitor,
            work: info.monitorInfo.rcWork,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            name: String::from_utf16_lossy(&info.szDevice[..name_len]),
        });
        true.into()
    }
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut displays as *mut Vec<DisplayInfo> as isize),
        );
    }
    displays
}

/// Index of the monitor the foreground window is on, among `displays`.
/// `None` when the foreground monitor cannot be determined (no foreground
/// window, or it does not match the current snapshot).
fn foreground_monitor_index(displays: &[DisplayInfo]) -> Option<usize> {
    let foreground = unsafe { GetForegroundWindow() };
    let monitor = unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) };
    if monitor.0.is_null() {
        return None;
    }
    displays.iter().position(|display| display.handle == monitor)
}

/// Resolves which display the pill targets, per `mode`, against the current
/// display snapshot:
///
/// - `ActiveWindow` — the monitor of the foreground window (given as its index
///   in `displays`); when that cannot be determined, the primary display.
/// - `Primary` — the display Windows flags as primary; when none is flagged
///   (should not happen), the first enumerated.
/// - `Index(n)` — the nth enumerated display; when out of range (the display
///   was unplugged or reordered after the config was saved), the primary
///   display. The configured index is preserved — nothing here mutates the
///   config — so the setting reapplies automatically when the display
///   comes back.
///
/// Returns `None` only when no display exists at all.
pub(crate) fn resolve_target(
    mode: MonitorMode,
    displays: &[DisplayInfo],
    foreground_nearest: Option<usize>,
) -> Option<usize> {
    if displays.is_empty() {
        return None;
    }
    let primary = displays.iter().position(|display| display.primary).unwrap_or(0);
    match mode {
        MonitorMode::ActiveWindow => Some(
            foreground_nearest
                .filter(|index| *index < displays.len())
                .unwrap_or(primary),
        ),
        MonitorMode::Primary => Some(primary),
        MonitorMode::Index(index) => {
            let in_range = (index as usize) < displays.len();
            if in_range {
                Some(index as usize)
            } else {
                warn_index_fallback(index);
                Some(primary)
            }
        }
    }
}

/// Warns about a configured-but-unattached display index, at most once per
/// unique index every `INDEX_WARN_INTERVAL` — the pill re-resolves its target
/// every frame, so an unthrottled warn would flood the log while a display is
/// gone for a long time.
const INDEX_WARN_INTERVAL: Duration = Duration::from_secs(10);
static LAST_INDEX_WARN: Mutex<Option<(u32, Instant)>> = Mutex::new(None);

fn warn_index_fallback(index: u32) {
    let mut last = LAST_INDEX_WARN.lock().unwrap();
    let now = Instant::now();
    let due = match *last {
        Some((last_index, at)) => last_index != index || now.duration_since(at) >= INDEX_WARN_INTERVAL,
        None => true,
    };
    if due {
        warn!("configured display index {index} is not attached; using the primary display (config untouched)");
        *last = Some((index, now));
    }
}

/// Logs the resolved target at most once every `TARGET_LOG_INTERVAL` per
/// target, so a visible pill (which re-resolves its target every frame) does
/// not log one line per animation frame. The throttled line is the log-based
/// answer to "which display is the pill on, and why".
const TARGET_LOG_INTERVAL: Duration = Duration::from_secs(5);
static LAST_TARGET_LOG: Mutex<Option<(usize, Instant)>> = Mutex::new(None);

fn log_target_once(target: &TargetMonitor, name: &str) {
    let mut last = LAST_TARGET_LOG.lock().unwrap();
    let now = Instant::now();
    let due = match *last {
        Some((last_index, at)) => last_index != target.index || now.duration_since(at) >= TARGET_LOG_INTERVAL,
        None => true,
    };
    if due {
        debug!(
            "overlay target: Display {} ({}){}",
            target.index + 1,
            name,
            if target.primary { ", primary" } else { "" }
        );
        *last = Some((target.index, now));
    }
}

/// A resolved placement target display. Re-derived from a fresh enumeration
/// on every use (see `enumerate_displays`), so a hot-plugged or reordered
/// display is picked up immediately.
pub(crate) struct TargetMonitor {
    pub handle: HMONITOR,
    /// The display's work area in virtual screen coordinates — the pill's
    /// anchors and clamping operate on this.
    pub work: RECT,
    /// Zero-based position in the enumeration.
    pub index: usize,
    /// Whether this is the display Windows flags as primary.
    pub primary: bool,
}

/// Effective DPI of a display. `GetDpiForMonitor(MDT_EFFECTIVE_DPI)` reports
/// the DPI Windows scales that display at; this process is per-monitor-v2
/// aware (see `main.rs`), so the value matches what a window on that display
/// gets. Falls back to 96 (100 %) when the API fails.
pub(crate) fn monitor_dpi(handle: HMONITOR) -> u32 {
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;
    unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
        .map(|_| dpi_x.max(dpi_y).max(96))
        .unwrap_or(96)
}

/// The pill's screen top-left for a `width`×`height` window on the target
/// display's work area, from the config-derived placement. Pure math — the
/// historical behavior of `OverlayState::position()` with the foreground
/// monitor's work area, now parameterized by the resolved target.
fn placement(work: RECT, width: i32, height: i32, position: &OverlayPos, inset: i32, scale: f32) -> POINT {
    let margin = (position.margin as f32 * scale).round() as i32;
    let span_w = work.right - work.left;
    // The DIB is inflated by `aura_inset` on each side, but the PILL (not the
    // window) must be centered/anchored. Subtract the inset so the pill lands
    // where the user expects it.
    let x = if let Some(px) = position.x {
        (px as f32 * scale).round() as i32
    } else {
        match position.horizontal {
            HorizontalPosition::Left => work.left + margin + inset,
            HorizontalPosition::Center => work.left + (span_w - width) / 2 - inset,
            HorizontalPosition::Right => work.right - width - margin - inset,
        }
    };
    let y = if let Some(py) = position.y {
        (py as f32 * scale).round() as i32
    } else {
        match position.vertical {
            // The DIB extends `inset` beyond the pill on each side; shift the
            // window so the PILL body (not the aura) sits at the configured
            // margin from the work-area edge.
            VerticalPosition::Top => work.top + margin + inset,
            VerticalPosition::Bottom => work.bottom - height - margin - inset,
        }
    };
    // Clamp to the current work area so absolute overrides stay usable after a
    // resolution or monitor change.
    let x = x.clamp(work.left, (work.right - width).max(work.left));
    let y = y.clamp(work.top, (work.bottom - height).max(work.top));
    POINT { x, y }
}

/// The sampled foreground state a layout decision is based on. Produced by
/// `OverlayState::sample_foreground` (the only place Win32 is queried), so
/// `decide_layout` stays pure and unit-testable.
struct ForegroundVerdict {
    /// The foreground window's executable name (with extension, as the
    /// process table reports it), when it could be read.
    exe: Option<String>,
    /// Whether the foreground window is a fullscreen app covering its
    /// monitor's entire screen.
    fullscreen: bool,
}

/// Whether a foreground window counts as a fullscreen app for Auto layout.
/// Conservative on purpose: the window must be visible, not minimized, not a
/// tool window, not a desktop/shell/taskbar surface, not this overlay's own
/// window, and its window rect must cover the entire monitor — not merely
/// the work area, so a maximized window stays Expanded. Anything ambiguous
/// resolves to `false` (Expanded).
fn window_is_fullscreen(hwnd: HWND, overlay: HWND) -> bool {
    if hwnd.0.is_null() || hwnd == overlay {
        return false;
    }
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return false;
        }
        // Desktop and shell surfaces are windows too, but never fullscreen apps.
        let mut class = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut class);
        if len > 0 {
            let name = String::from_utf16_lossy(&class[..len as usize]);
            if matches!(
                name.as_str(),
                "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
            ) {
                return false;
            }
        }
        // Transient tool windows (flyouts, popups) never count as fullscreen.
        if (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return false;
        }
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.0.is_null() {
            return false;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return false;
        }
        // A maximized window covers the work area, which excludes the
        // taskbar band; only a real fullscreen window covers rcMonitor.
        const TOLERANCE: i32 = 2;
        let m = info.rcMonitor;
        rect.left <= m.left + TOLERANCE
            && rect.top <= m.top + TOLERANCE
            && rect.right >= m.right - TOLERANCE
            && rect.bottom >= m.bottom - TOLERANCE
    }
}

/// Whether the foreground app's executable name matches the Auto-compact
/// source list. Mirrors the `media_sources` matching convention (normalized
/// case-insensitive substring; word-boundary characters stripped), with the
/// process-picker's `.exe`-stripping applied to the name. Unlike
/// `media_sources`, an empty list allows nothing: Auto-compact is opt-in
/// per app, so an unlisted foreground never compacts.
fn auto_source_matches(config: &Config, exe_name: Option<&str>) -> bool {
    let Some(exe) = exe_name else {
        return false;
    };
    let patterns = &config.behavior.auto_compact_sources;
    if patterns.is_empty() {
        return false;
    }
    let exe = exe.trim_end_matches(".exe").trim_end_matches(".EXE");
    let n_exe = crate::smtc::normalize_for_match(exe);
    patterns
        .iter()
        .any(|pattern| n_exe.contains(&crate::smtc::normalize_for_match(pattern)))
}

/// Decides the effective pill layout from the configured mode and the
/// foreground verdict. Pure: the caller samples the foreground and feeds the
/// verdict in, so every branch is unit-testable without Win32. Auto compacts
/// when the foreground app is fullscreen or on the `auto_compact_sources`
/// list; everything else stays Expanded.
fn decide_layout(config: &Config, verdict: &ForegroundVerdict) -> LayoutMode {
    match config.overlay.layout {
        LayoutMode::Expanded => LayoutMode::Expanded,
        LayoutMode::Compact => LayoutMode::Compact,
        LayoutMode::Auto => {
            if verdict.fullscreen || auto_source_matches(config, verdict.exe.as_deref()) {
                LayoutMode::Compact
            } else {
                LayoutMode::Expanded
            }
        }
    }
}

/// Reusable device context + DIB section for the pill's frames. The overlay
/// redraws every animation tick; recreating the DIB per frame is pure waste.
struct DibCache {
    hdc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    bits: *mut c_void,
    width: i32,
    height: i32,
}

/// Animation tick driver. Fires from the timer queue and dispatches the tick
/// to the UI thread. `PostMessageW` (not `SendMessageW`) keeps the callback
/// non-blocking, so the timer can be deleted with a completion wait at
/// teardown without deadlocking (a blocking callback would wait on the very
/// thread that is deleting the timer).
unsafe extern "system" fn animation_timer_proc(parameter: *mut c_void, _fired: BOOLEAN) {
    let hwnd = HWND(parameter);
    unsafe {
        let _ = PostMessageW(hwnd, TIMER_ANIMATION_MSG, WPARAM(0), LPARAM(0));
    }
}

/// Incoming event transport shared with the main window: an event is
/// allocated once by the SMTC worker, the forwarder hands the same `Arc` to
/// both window queues (a refcount bump per window), and each drain recovers
/// the owned event with `media_event_into_owned` — zero-copy when this window
/// is the last holder, a single clone while the other window still holds it.
pub(crate) type EventQueue = Arc<Mutex<VecDeque<Arc<MediaEvent>>>>;

enum Phase {
    Hidden,
    Expanding(Instant),
    Light(Instant),
    Shown,
    Collapsing(Instant),
}

/// Upper bound on the pending notification queue. At the cap the oldest
/// unshown queued event is dropped in favor of the incoming one; the pill
/// currently on screen is never pulled. Four distinct real notifications
/// colliding within milliseconds is already an edge case, so the cap is not
/// worth tuning.
const PENDING_CAP: usize = 4;

/// Upper bound on the per-source track cache. Each entry holds the last
/// track's decoded cover (up to 256 KB at the cap; typically 64 KB at 100 %
/// DPI) plus the pill text, and the worker evicts its own source-level
/// caches when sessions close — the overlay has no session knowledge, so an
/// LRU + idle timeout keeps long-idle sources from accumulating covers that
/// will never be shown again. Three entries bound the retained cover memory
/// while covering a realistic source mix (music + video + podcast).
const TRACK_CACHE_CAP: usize = 3;

/// Entries untouched for this long are evicted at the next insert. Eviction
/// is lazy (sweep inside `cache_track`), so an expired entry remains
/// readable until new data arrives: a state pill is never robbed of the
/// cached track by the timeout alone. Time-bounds what the cap cannot: a
/// source that played once and never returns still drops out after this.
const TRACK_CACHE_TTL: Duration = Duration::from_secs(600);

/// Per-line marquee state for the pill's text rows. The offset advances on the
/// 16ms animation tick; a short hold before the first movement reads better.
#[derive(Default, Clone, Copy)]
struct LineScroll {
    offset: f32,
    started_at: Option<Instant>,
    /// Whether the last rendered frame overflowed this line (text wider than
    /// its band). The animation tick only repaints a fully-shown pill while at
    /// least one line is scrolling; static text needs no per-frame redraw.
    scrolling: bool,
}

/// Bundles the scroll state of one line with the cached raster of that line,
/// so the scrolling draw can pass both to the text rasterizer together.
struct MarqueeCtx<'a> {
    scroll: &'a mut LineScroll,
    strip: &'a mut Option<MarqueeStrip>,
}

/// The overflowing line rasterized once at its natural width, premultiplied
/// with the row's color. Every overflow frame — the pre-scroll hold included
/// — samples the visible window from this strip (two contiguous runs)
/// instead of re-running GDI text rendering (ExtTextOutW + GdiFlush) at
/// animation cadence. Rasterization occurs on a cache miss: the strip is
/// rebuilt when content, size, font, or color changes, and a cache hit keeps
/// every later frame a pure composite.
struct MarqueeStrip {
    value: String,
    rw: i32,
    rh: i32,
    font: HFONT,
    font_height: i32,
    color: [u8; 4],
    centered: bool,
    text_w: i32,
    pixels: Vec<u8>,
}

/// Hold time before an overflowing line starts scrolling.
const MARQUEE_HOLD: Duration = Duration::from_millis(600);
/// Horizontal gap between the end of the text and its repeated copy.
const MARQUEE_GAP: f32 = 24.0;
/// Scroll speed in logical px per second.
const MARQUEE_SPEED: f32 = 40.0;
/// Width of the horizontal alpha fade at each visible edge of a scrolling
/// marquee line, in logical px (scaled by the render scale at draw time).
/// During the pre-scroll hold only the trailing edge fades.
const MARQUEE_FADE: f32 = 12.0;
/// Band height per text row as a multiple of the row's font size. Matches the
/// font's natural line height (ascent + descent ≈ 1.33x for Segoe UI), so rows
/// pack tightly without clipping.
const ROW_HEIGHT: f32 = 1.35;

/// Scratch device context + DIB used to render pill text with Windows' own
/// GDI text engine (ClearType, proper hinting), then composite the glyph
/// coverage into the pill's premultiplied buffer. GDI writes alpha 0 for text
/// into 32bpp DIBs, so the RGB of each glyph pixel (text color × coverage)
/// supplies the coverage.
struct TextScratch {
    hdc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    bits: *mut c_void,
    width: i32,
    height: i32,
}

/// Pre-rendered text pieces of the pill currently on screen, built once per
/// content change so animation frames neither rebuild the meta line nor
/// clone the cached TrackInfo. The draw path takes it out of the state and
/// puts it back, keeping the `state` borrow exclusive.
struct PillText {
    title: String,
    artist: String,
    source_app: String,
    app_icon: Option<Arc<[u8]>>,
    meta_clock: bool,
    meta: String,
}

struct OverlayState {
    hwnd: HWND,
    config: Config,
    queue: EventQueue,
    /// Notifications waiting to be shown, in arrival order. Distinct events
    /// from different sources show one after another instead of clobbering
    /// each other; the pill on screen is never replaced early. A newer event
    /// for a source already waiting supersedes the older one, so a burst of
    /// same-source events (play/pause spam) collapses to the latest.
    pending: VecDeque<MediaEvent>,
    enabled: bool,
    content: Option<MediaEvent>,
    last_track: Option<TrackInfo>,
    phase: Phase,
    dismiss_at: Option<Instant>,
    /// When the cursor hovers over the pill, the dismiss deadline is
    /// shortened to 500ms. The arm is one-way: the pill dismisses 500ms
    /// after the hover is first detected even if the cursor leaves before
    /// then. The flag also stops the tick from re-arming (which would keep
    /// pushing the deadline forward while the cursor stays put).
    hover_dismiss_at: Option<Instant>,
    position: OverlayPos,
    /// The compact pill's resolved placement (independent of `position` only
    /// while `compact_position_separate` is on; see `active_pos`).
    compact_position: OverlayPos,
    /// The layout actually applied to the current pill. `Auto` is already
    /// resolved to Expanded/Compact from the foreground (see
    /// `refresh_layout`/`tick_layout_check`), so every consumer just reads
    /// this.
    layout: LayoutMode,
    /// Foreground HWND the cached executable identity (`layout_fg_exe`)
    /// belongs to. The process table is only re-enumerated when this
    /// changes; a static foreground is served from the cache.
    layout_fg: Option<HWND>,
    /// Cached executable name of the foreground window, keyed by
    /// `layout_fg`. `None` when the process could not be read (missing or
    /// elevated) — callers treat that as "no Auto-compact source match".
    layout_fg_exe: Option<String>,
    /// Last time the fullscreen geometry of an unchanged foreground window
    /// was re-checked. The same-window re-check is throttled to 1 Hz (the
    /// geometry can only change on a window resize, never at 4 Hz).
    last_geometry_check: Option<Instant>,
    /// Per-row marquee state for the four track lines (title/subtitle/meta/app).
    scroll: [LineScroll; 4],
    /// Per-row cached marquee rasters (parallel to `scroll`), see `MarqueeStrip`.
    marquee_strips: [Option<MarqueeStrip>; 4],
    /// High-resolution timer driving the pill animation.
    /// Animation timer from the timer queue; when creation fails, a plain
    /// window timer with `ANIM_TIMER_ID` drives the animation instead.
    anim_timer: HANDLE,
    anim_timer_fallback: bool,
    /// Animation tick period in ms, matched to the monitor's refresh rate.
    /// Re-detected on every show; the timer is recreated only when it changes.
    tick_period: u32,
    /// Cached decoded artwork for the current track (RGBA8 at the full art
    /// size), so animation frames never re-decode or re-convert the cover.
    decoded_art: Option<Vec<u8>>,
    /// The worker's decoded pixels (premultiplied BGRA) that produced
    /// `decoded_art`, so a cover change for the same song (same title+artist,
    /// different art) re-converts instead of showing the stale image. Also
    /// records failed decodes (None source key) so a corrupt cover is
    /// attempted once instead of on every animation frame.
    decoded_art_source: Option<Arc<[u8]>>,
    /// Dominant colors derived from `decoded_art` (recomputed only when the
    /// artwork re-decodes): the aura gradient and the accent recoloring read
    /// from here, so they always match the cover that is actually displayed.
    palette: Option<Palette>,
    /// Cached DIB (DC + bitmap) reused across frames of the same size.
    dib: Option<DibCache>,
    /// Tightly-packed per-frame scratch buffer (stride == the requested
    /// frame width), reused and grown but never shrunk across frames. The
    /// real DIB backing buffer (`dib`) is allocated to a generous upper
    /// bound and reused across animation frames, so its scanline stride
    /// does not match the requested per-frame size; `draw_pixels` and
    /// `draw_text_pixels` render into this buffer instead, at the stride
    /// they have always assumed, and `render_layered` blits the result into
    /// the real DIB at its real stride right before the GDI call. See
    /// `render_layered` for why: drawing straight into the oversized DIB at
    /// the requested width as its stride was tried once and produced a
    /// torn image, because the two strides only match when the pill is at
    /// its fully expanded size.
    frame_scratch: Vec<u8>,
    /// Timestamp of the previous animation tick, for time-based marquee
    /// scrolling.
    last_tick: Instant,
    /// Last time the topmost z-order was re-asserted. While the pill is fully
    /// shown (static), the re-assert is throttled to 1 Hz instead of running
    /// on every 4 ms tick.
    last_reassert: Option<Instant>,
    /// Cached monitor refresh period (ms), re-sampled at most once per
    /// second. `sync_anim_timer` runs on every animation tick; the underlying
    /// DWM/display-mode queries are far more expensive than the tick itself.
    period_cache: Option<(Instant, u32)>,
    /// Wake flag for the event queue: `true` while a `MEDIA_EVENT_MSG` is in
    /// flight. The forwarder and this window only post when the flag was
    /// clear, so an event burst collapses into one wake message per drain.
    wake: Arc<AtomicBool>,
    /// Source app of the last TrackChanged shown, used as the label fallback
    /// in state pills for current-session playback states so the pill always
    /// names the app that owns the media — never another app's last track.
    current_source: Option<String>,
    /// Per-source track cache: the last TrackChanged shown for each source app,
    /// so that a later PlaybackStateChanged for that source can render the
    /// correct track info instead of the most-recently-shown app's track.
    /// Entries hold the pill text and decoded cover (raw artwork stripped at
    /// insert — see `cache_track`). LRU-ordered and time-bounded (see
    /// `TRACK_CACHE_CAP`/`TRACK_CACHE_TTL`), so a source that stops playing
    /// eventually drops out instead of holding its cover forever. The
    /// `Instant` is the last insert time.
    track_cache: HashMap<String, (TrackInfo, Instant)>,
    /// Recency order of `track_cache` keys (front = oldest). Kept in sync by
    /// `cache_track`.
    track_cache_order: VecDeque<String>,
    /// Pre-rendered text pieces of the pill currently on screen, resolved once
    /// per content change (see `resolve_pill_text`).
    pill_text: Option<PillText>,
    /// Scratch DC + DIB for GDI text rendering (cached across frames).
    text_scratch: Option<TextScratch>,
    /// Reusable UTF-16 scratch buffer for GDI text rendering, cleared and
    /// refilled on each text-line draw so the render tick performs no per-frame
    /// heap allocation for text encoding.
    scratch_utf16: Vec<u16>,
    /// Per-window font cache (DPI-scoped). `FontProvider::font_for` returns a
    /// cached Segoe UI HFONT for a pixel height and boldness, creating it once;
    /// the cache is swapped for a fresh provider on DPI change, whose `Drop`
    /// deletes the old HFONTs. Owned by the overlay state and touched only from
    /// this thread, so its inner lock is uncontended.
    fonts: FontProvider,
    /// Physical-pixel inset from the buffer edge to the pill body, computed
    /// each frame from `AURA_HALO_LOGICAL * dpi * shape`. The pill is
    /// drawn at `(aura_inset, aura_inset)` so the aura fills the outer ring.
    aura_inset: i32,
}

/// Resolved placement for the WinGlance pill, pulled from [overlay] config. `x`/`y`
/// are absolute overrides (96-DPI logical pixels) that take precedence over the
/// vertical/horizontal anchors when `Some`; the pill snaps back to the anchor when
/// they are cleared.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OverlayPos {
    vertical: VerticalPosition,
    horizontal: HorizontalPosition,
    margin: i32,
    x: Option<i32>,
    y: Option<i32>,
    monitor: MonitorMode,
}

impl OverlayPos {
    pub(crate) fn from_config(config: &Config) -> Self {
        Self {
            vertical: config.overlay.vertical,
            horizontal: config.overlay.horizontal,
            margin: config.overlay.margin,
            x: config.overlay.position_x,
            y: config.overlay.position_y,
            monitor: config.overlay.monitor,
        }
    }

    /// Resolves the compact pill's placement from config through the shared
    /// effective rule (`compact_effective`): while `compact_position_separate`
    /// is off this is exactly the expanded position, so the overlay and the
    /// settings UI can never disagree about where a compact pill sits.
    pub(crate) fn compact_from_config(config: &Config) -> Self {
        let compact = config.overlay.compact_effective();
        Self {
            vertical: compact.vertical,
            horizontal: compact.horizontal,
            margin: compact.margin,
            x: compact.x,
            y: compact.y,
            monitor: compact.monitor,
        }
    }
}

/// Updates the live overlay's placement from the resolved expanded and
/// compact positions.
pub(crate) fn set_positions(hwnd: HWND, pos: OverlayPos, compact_pos: OverlayPos) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = window_state::<OverlayState>(hwnd);
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        state.position = pos;
        state.compact_position = compact_pos;
        debug!(
            "overlay position applied: vertical={:?} horizontal={:?} x={:?} y={:?} monitor={:?} | compact vertical={:?} horizontal={:?} x={:?} y={:?} monitor={:?}",
            pos.vertical,
            pos.horizontal,
            pos.x,
            pos.y,
            pos.monitor,
            compact_pos.vertical,
            compact_pos.horizontal,
            compact_pos.x,
            compact_pos.y,
            compact_pos.monitor
        );
        if !state.preview_if_hidden() {
            state.reposition();
        }
    }
}

/// Updates the live overlay's notification duration. The overlay keeps its own
/// config snapshot, so settings changes must be pushed here to take effect.
pub(crate) fn set_duration(hwnd: HWND, duration_ms: u64) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = window_state::<OverlayState>(hwnd);
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        state.config.overlay.duration_ms = duration_ms.clamp(500, 60_000);
        info!("overlay duration set to {} ms", state.config.overlay.duration_ms);
    }
}

/// Pushes a layout-mode change to the live overlay (which keeps its own config
/// snapshot): the mode is stored and re-resolved from the current foreground,
/// so a visible pill flips between Expanded and Compact immediately — with its
/// size, content layout and placement recomputed — while a hidden pill shows a
/// short sample so the new mode is previewable (same behavior position changes
/// use).
pub(crate) fn set_layout(hwnd: HWND, mode: LayoutMode) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = window_state::<OverlayState>(hwnd);
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        state.config.overlay.layout = mode;
        // A hidden pill's sample re-resolves the layout from the foreground
        // itself (show_sample → refresh_layout), so only the visible path
        // refreshes here.
        if !state.preview_if_hidden() {
            state.refresh_layout();
            state.render();
        }
        info!("overlay layout mode set: {mode:?} (resolved: {:?})", state.layout);
    }
}

/// Pushes the compact-position separation flag to the live overlay, so the
/// pill's placement follows `compact_effective` immediately (the positions
/// themselves travel through `set_positions`).
pub(crate) fn set_compact_separate(hwnd: HWND, separate: bool) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = window_state::<OverlayState>(hwnd);
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        state.config.overlay.compact_position_separate = separate;
        info!(
            "overlay compact_position_separate set to {separate} (display: {})",
            if separate { "OFF" } else { "ON" }
        );
        if !state.preview_if_hidden() {
            state.render();
        }
    }
}

impl OverlayState {
    fn new(config: Config, queue: EventQueue) -> Self {
        let position = OverlayPos::from_config(&config);
        let compact_position = OverlayPos::compact_from_config(&config);
        let enabled = config.behavior.notifications_enabled;
        Self {
            hwnd: HWND::default(),
            config,
            queue,
            pending: VecDeque::new(),
            enabled,
            content: None,
            last_track: None,
            phase: Phase::Hidden,
            dismiss_at: None,
            hover_dismiss_at: None,
            position,
            compact_position,
            // Every show path re-resolves the layout before the first frame
            // (see `show_with_duration`), so this initial value is only a
            // placeholder until then.
            layout: LayoutMode::Expanded,
            layout_fg: None,
            layout_fg_exe: None,
            last_geometry_check: None,
            scroll: [LineScroll::default(); 4],
            marquee_strips: [None, None, None, None],
            anim_timer: HANDLE::default(),
            anim_timer_fallback: false,
            tick_period: 16,
            decoded_art: None,
            decoded_art_source: None,
            palette: None,
            dib: None,
            frame_scratch: Vec::new(),
            last_tick: Instant::now(),
            last_reassert: None,
            period_cache: None,
            wake: Arc::new(AtomicBool::new(false)),
            current_source: None,
            track_cache: HashMap::new(),
            track_cache_order: VecDeque::new(),
            pill_text: None,
            text_scratch: None,
            scratch_utf16: Vec::new(),
            fonts: FontProvider::new(0),
            aura_inset: 0,
        }
    }

    fn reset_scroll(&mut self) {
        let now = Instant::now();
        for line in &mut self.scroll {
            line.offset = 0.0;
            line.started_at = Some(now);
            // The overflow flag is recomputed on the next render.
            line.scrolling = false;
        }
    }

    /// Converts (once per artwork) the worker's premultiplied BGRA decode into
    /// the overlay's straight RGBA buffer at the full art size, so animation
    /// frames never re-decode or re-convert the JPEG/PNG. Keyed by the decoded
    /// pixels: a different cover (new Arc/bytes) re-converts, unchanged art
    /// (session recreation, re-render) is served from the cache. A failed
    /// decode is cached too — the source is recorded even when the buffer
    /// stays `None` — so a corrupt cover is attempted once instead of on every
    /// animation frame. The palette is derived from the same converted buffer
    /// (~0.1ms, only when a conversion happens), so no separate
    /// full-resolution decode is ever needed for color extraction.
    fn ensure_art(&mut self, decoded: Option<&Arc<[u8]>>) {
        let same_art = artwork_same(self.decoded_art_source.as_deref(), decoded.map(|d| d.as_ref()));
        if same_art {
            return;
        }
        self.decoded_art = decoded.and_then(|arc| pm_bgra_to_rgba(arc));
        self.decoded_art_source = decoded.cloned();
        self.palette = self.decoded_art.as_deref().and_then(crate::palette::palette_from_rgba);
    }

    fn ensure_anim_timer(&mut self) {
        if !self.anim_timer.0.is_null() || self.anim_timer_fallback {
            return;
        }
        let mut handle = HANDLE::default();
        let created = unsafe {
            CreateTimerQueueTimer(
                &mut handle,
                None,
                Some(animation_timer_proc),
                Some(self.hwnd.0 as *const c_void),
                self.tick_period,
                self.tick_period,
                WT_EXECUTEDEFAULT,
            )
            .is_ok()
        };
        if created {
            self.anim_timer = handle;
            return;
        }
        // Rare (handle exhaustion or a low-resource condition): without a
        // timer the pill freezes at its first frame and never dismisses.
        // Fall back to a plain window timer that drives the same tick.
        error!("CreateTimerQueueTimer failed; falling back to SetTimer");
        if unsafe { SetTimer(self.hwnd, ANIM_TIMER_ID, self.tick_period, None) } != 0 {
            self.anim_timer_fallback = true;
        }
    }

    /// Re-samples the monitor's refresh period and recreates the animation
    /// timer when it changed (display switched, DPI changed, VRR kicked in).
    /// The tick cadence only affects how many frames the UI thread gets asked
    /// to paint; the easing is time-based, so motion is identical either way.
    /// While the pill is fully static (no animation, no marquee line) the
    /// timer drops to `STATIC_TICK_MS`: the dismiss countdown and hover
    /// polling do not need frame rate, so a shown pill stops waking the UI
    /// thread at monitor refresh rate.
    fn sync_anim_timer(&mut self) {
        let animating = !matches!(self.phase, Phase::Shown);
        let marquee_active = self.scroll.iter().any(|line| line.scrolling);
        let now = Instant::now();
        let period = if animating || marquee_active {
            // The monitor queries behind `refresh_period_ms` (DWM timing,
            // display-mode enumeration) are not free to run every tick; a
            // 1-second cache is far fresher than any real rate change. The
            // target is re-resolved only when a fresh period is actually
            // needed.
            match self.period_cache {
                Some((cached_at, cached)) if now.duration_since(cached_at) < Duration::from_secs(1) => cached,
                _ => {
                    let fresh = refresh_period_ms(self.target().as_ref(), self.hwnd);
                    self.period_cache = Some((now, fresh));
                    fresh
                }
            }
        } else {
            STATIC_TICK_MS
        };
        if period != self.tick_period {
            debug!(
                "animation tick {period}ms = {} Hz ({})",
                1000 / period.max(1),
                if animating || marquee_active {
                    "refresh-rate matched"
                } else {
                    "static"
                }
            );
            self.tick_period = period;
            self.delete_anim_timer();
        }
        self.ensure_anim_timer();
    }

    fn delete_anim_timer(&mut self) {
        if !self.anim_timer.0.is_null() {
            // Wait for any in-flight callback to complete
            // (INVALID_HANDLE_VALUE): the callback is a single non-blocking
            // PostMessageW, so the wait cannot deadlock, and no stale tick
            // message can be posted to a window that is being torn down.
            unsafe {
                let _ = DeleteTimerQueueTimer(None, self.anim_timer, INVALID_HANDLE_VALUE);
            }
            self.anim_timer = HANDLE::default();
        }
        if self.anim_timer_fallback {
            unsafe {
                let _ = KillTimer(self.hwnd, ANIM_TIMER_ID);
            }
            self.anim_timer_fallback = false;
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
        // A PlaybackStateChanged that races a TrackChanged for the same
        // source in the same batch is redundant: the state pill would render
        // the source's *previously cached* track, and the track pill that
        // follows in the same batch carries the change. The worker emits
        // state-then-track per read, so the pairing is always ordered.
        let track_sources: Vec<String> = batch
            .iter()
            .filter_map(|e| match e.as_ref() {
                MediaEvent::TrackChanged(t) => Some(t.source_app.clone()),
                _ => None,
            })
            .collect();
        // The queue carries Arc<MediaEvent> so the fan-out to both windows
        // never copies the event; recover the owned event here (zero-copy
        // when this window is the last holder, a clone otherwise).
        for event in batch.into_iter().map(media_event_into_owned) {
            if !self.enabled {
                continue;
            }
            match event {
                MediaEvent::TrackChanged(track) if self.config.behavior.enable_track_change => {
                    // A metadata refresh for the track currently on screen
                    // (SMTC fills artwork/album progressively, a moment after
                    // the title) updates the pill in place instead of queueing
                    // a second notification for the same song. Cross-source
                    // matches do not refresh in place, and a different cover
                    // for the same title+artist (video vs audio version)
                    // queues a fresh pill rather than updating the old one.
                    let is_update = self.content.as_ref().is_some_and(
                        |content| matches!(content, MediaEvent::TrackChanged(shown) if shown.same_media(&track)),
                    );
                    if is_update {
                        self.current_source = Some(track.source_app.clone());
                        self.last_track = Some(track.clone());
                        self.cache_track(&track);
                        self.update_content(MediaEvent::TrackChanged(track), update_min_duration(&self.config));
                    } else {
                        self.enqueue(MediaEvent::TrackChanged(track));
                    }
                }
                MediaEvent::PlaybackStateChanged(state, source_app)
                    if self.config.behavior.enable_playback_state_change =>
                {
                    // Suppress a PlaybackStateChanged pill when:
                    //  - A TrackChanged for the same source is in this batch
                    //    (see the pre-scan above: the state pill would render
                    //    the source's previously cached track) or already
                    //    queued (a TrackChanged pill is about to show; a
                    //    redundant PlaybackStateChanged would flash the same
                    //    info).
                    //  - The pill on screen is the source's track pill: the
                    //    song announcement is still visible, so the state flip
                    //    adds nothing.
                    //  - It is Playing AND the same source's TrackChanged was
                    //    recently shown (prevents the "replaying" pill after
                    //    session recreation, or when a browser video triggers
                    //    YTM to re-report "Playing").
                    // Paused/Stopped pass through when they are a new state
                    // from a source that is NOT currently shown.
                    let track_wins = track_sources.iter().any(|s| s == &source_app)
                        || self
                            .pending
                            .iter()
                            .any(|e| matches!(e, MediaEvent::TrackChanged(t) if t.source_app == source_app));
                    let track_pill_shown = matches!(
                        self.content.as_ref(),
                        Some(MediaEvent::TrackChanged(t)) if t.source_app == source_app
                    );
                    if track_wins || track_pill_shown {
                        debug!(
                            "playback state pill suppressed | reason=track shown for same source | source={source_app}"
                        );
                        continue;
                    }
                    // A new state from a source whose state pill is on screen
                    // updates it in place (play/pause spam): the pill shows
                    // the latest state and gets the full duration again from
                    // the last change, instead of queueing one pill per
                    // toggle.
                    let state_pill_shown = matches!(
                        self.content.as_ref(),
                        Some(MediaEvent::PlaybackStateChanged(_, shown_source)) if shown_source == &source_app
                    );
                    if state_pill_shown {
                        let event = MediaEvent::PlaybackStateChanged(state, source_app);
                        let full = Duration::from_millis(self.config.overlay.duration_ms.max(500));
                        self.update_content(event, full);
                        continue;
                    }
                    let replaying = matches!(state, PlaybackState::Playing)
                        && self.current_source.as_deref() == Some(source_app.as_str());
                    if replaying {
                        debug!("playback state pill suppressed | reason=replaying same source | source={source_app}");
                        continue;
                    }
                    self.enqueue(MediaEvent::PlaybackStateChanged(state, source_app));
                }
                MediaEvent::TrackChanged(_) | MediaEvent::PlaybackStateChanged(_, _) => {}
                // Rejected sessions and worker failures are history-only:
                // never shown as a pill.
                MediaEvent::SessionRejected { .. } | MediaEvent::WorkerFailed { .. } => {}
            }
        }
        if !self.pending.is_empty() {
            // A newer notification is waiting: don't make the user wait out
            // the current pill's full duration (2-3s+ on a pause/play). Cap
            // the remaining time at EARLY_EXIT_MS so the queued update shows
            // promptly. min() never extends an already-sooner deadline
            // (e.g. hover-dismiss).
            if !matches!(self.phase, Phase::Hidden | Phase::Collapsing(_)) {
                let early = Instant::now() + Duration::from_millis(EARLY_EXIT_MS);
                self.dismiss_at = Some(self.dismiss_at.map_or(early, |d| d.min(early)));
            }
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_DEBOUNCE);
                SetTimer(
                    self.hwnd,
                    TIMER_DEBOUNCE,
                    self.config.behavior.debounce_ms.clamp(150, 250) as u32,
                    None,
                );
            }
        }
        // Events that arrived while we were draining need a wake-up: re-arm
        // and post only if no wake message is already in flight. A failed
        // post drops the pending batch (and accounts for it) instead of
        // stranding events without a wake.
        crate::repost_if_pending(&self.queue, &self.wake, self.hwnd, "overlay");
    }

    /// Caches the last shown track for a source, moving the source to the
    /// back of the recency order and evicting the oldest entry when the cap
    /// is exceeded. A state pill for an evicted source falls back to the
    /// source-name layout — the accepted degradation for a source that has
    /// not played in a long time.
    fn cache_track(&mut self, track: &TrackInfo) {
        let source = track.source_app.clone();
        if let Some(pos) = self.track_cache_order.iter().position(|s| *s == source) {
            self.track_cache_order.remove(pos);
        }
        self.track_cache_order.push_back(source.clone());
        let now = Instant::now();
        // Insert first so the sweep below sees the fresh entry: a brand-new
        // source must never look like an expired cache miss.
        let mut cached = track.clone();
        // The cache only ever serves pill text and the decoded cover; nothing
        // reads the raw artwork bytes from it. Stripping them keeps the raw
        // cover (typically 50-500 KB) from being retained per source after
        // that source stops playing.
        cached.artwork = None;
        self.track_cache.insert(source, (cached, now));
        // Lazy sweep: expire idle entries first, then enforce the cap. Only
        // runs here (on insert), so an expired entry is still readable by a
        // pill that arrives before the next insert — the timeout never
        // degrades a pill on its own.
        while let Some(front) = self.track_cache_order.front().cloned() {
            let expired = self
                .track_cache
                .get(&front)
                .is_none_or(|(_, last_used)| now.duration_since(*last_used) > TRACK_CACHE_TTL);
            if self.track_cache_order.len() <= TRACK_CACHE_CAP && !expired {
                break;
            }
            self.track_cache_order.pop_front();
            self.track_cache.remove(&front);
        }
    }

    /// Adds a notification to the pending queue. At the cap, the oldest unshown
    /// queued event is dropped in favor of the incoming one; the pill currently
    /// on screen is never pulled. A newer event for a source already waiting
    /// supersedes the older one — the queue holds at most one event per source —
    /// so a burst of same-source events (play/pause spam, fast skipping)
    /// collapses to the latest. A metadata refresh for a track already waiting
    /// in the queue merges into it instead of showing the song twice.
    fn enqueue(&mut self, event: MediaEvent) {
        match &event {
            // A newer track from the same source supersedes the queued one:
            // skipping songs quickly shows only the last one. A metadata
            // refresh for the same media (artwork or album arriving late)
            // merges into that entry instead of queueing a duplicate; the
            // merge follows the same `same_media` identity rule as the
            // shown-pill update path, so a cover swap for the same
            // title+artist (video vs audio version) replaces the queued pill
            // with the latest version rather than keeping the stale cover.
            MediaEvent::TrackChanged(incoming) => {
                for queued in self.pending.iter_mut() {
                    if let MediaEvent::TrackChanged(queued) = queued
                        && queued.source_app == incoming.source_app
                    {
                        if queued.same_media(incoming) {
                            // Late metadata for the same song merges into the
                            // queued pill instead of queueing a duplicate —
                            // and merges *every* displayed field, not just
                            // album/artwork (a refresh can carry a later
                            // duration, genre, subtitle, or icon).
                            queued.merge_late_metadata(incoming);
                        } else {
                            *queued = incoming.clone();
                        }
                        return;
                    }
                }
            }
            // A newer playback state from the same source supersedes the
            // queued one: play/pause spam shows only the final state.
            MediaEvent::PlaybackStateChanged(_, source_app) => {
                for queued in self.pending.iter_mut() {
                    if let MediaEvent::PlaybackStateChanged(_, queued_source) = queued
                        && queued_source == source_app
                    {
                        *queued = event.clone();
                        return;
                    }
                }
            }
            // Never queued (receive_events skips it); defensive for
            // exhaustiveness.
            MediaEvent::SessionRejected { .. } | MediaEvent::WorkerFailed { .. } => {}
        }
        if self.pending.len() >= PENDING_CAP {
            self.pending.pop_front();
        }
        self.pending.push_back(event);
    }

    /// Shows the front of the pending queue as a fresh notification. Called by
    /// the debounce flush while the pill is hidden, and when the current pill
    /// finishes collapsing, so queued notifications show one after another.
    fn show_next(&mut self) {
        let Some(event) = self.pending.pop_front() else {
            return;
        };
        match event {
            MediaEvent::TrackChanged(track) => {
                self.current_source = Some(track.source_app.clone());
                self.last_track = Some(track.clone());
                self.cache_track(&track);
                self.show(MediaEvent::TrackChanged(track), true);
            }
            MediaEvent::PlaybackStateChanged(state, source_app) => {
                self.show(MediaEvent::PlaybackStateChanged(state, source_app), false);
            }
            // Never queued (receive_events skips it); defensive for
            // exhaustiveness.
            MediaEvent::SessionRejected { .. } => {
                debug!("session rejected event reached the pill queue; ignoring");
            }
            MediaEvent::WorkerFailed { .. } => {
                debug!("worker-failed event reached the pill queue; ignoring");
            }
        }
    }

    fn flush_pending(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_DEBOUNCE);
        }
        // While a pill is on screen the queue waits: the next event shows when
        // the current one collapses, so notifications never clobber each other.
        if !matches!(self.phase, Phase::Hidden) {
            return;
        }
        self.show_next();
    }

    /// Refreshes the shown content in place: keeps the current animation
    /// phase, extends the dismiss deadline to at least `now + min_visible`
    /// (a metadata refresh grants a short extension, a real content change —
    /// a state flip — grants the full configured duration again), and
    /// re-renders. The pill's size is constant — every row band is always
    /// reserved — so a refresh only changes the drawn rows, never the pill's
    /// dimensions.
    fn update_content(&mut self, event: MediaEvent, min_visible: Duration) {
        // An in-place refresh is a meaningful pill update too: re-resolve
        // the layout so a foreground change since the pill appeared takes
        // effect with the update rather than on the next static tick.
        self.refresh_layout();
        self.content = Some(event);
        self.resolve_pill_text();
        self.reset_scroll();
        if let Some(deadline) = self.dismiss_at {
            self.dismiss_at = Some(deadline.max(Instant::now() + min_visible));
        }
        // A refresh that lands during the collapse (e.g. artwork arriving as
        // the pill fades) would otherwise be cut short: the collapse keeps its
        // original start time and hides the pill when its animation finishes,
        // ignoring the extended deadline. Bring it back to full visibility for
        // the extended time instead.
        if matches!(self.phase, Phase::Collapsing(_)) {
            self.phase = Phase::Shown;
        }
        self.render();
    }

    /// Builds the render pieces for the pill content once, when the content
    /// changes. The state-pill path resolves the cached track here too, so
    /// animation frames draw from `pill_text` without a per-frame TrackInfo
    /// clone or meta-line rebuild. `None` for a state pill whose source has
    /// no cached track: the caller falls back to the source-name layout.
    fn resolve_pill_text(&mut self) {
        self.pill_text = match &self.content {
            Some(MediaEvent::TrackChanged(track)) => Some(pill_text_from_track(track)),
            Some(MediaEvent::PlaybackStateChanged(_, source)) if !source.is_empty() => self
                .track_cache
                .get(source)
                .map(|(track, _)| pill_text_from_track(track)),
            _ => None,
        };
    }

    fn show(&mut self, event: MediaEvent, full_animation: bool) {
        self.show_with_duration(event, full_animation, self.config.overlay.duration_ms.max(500));
    }

    fn show_with_duration(&mut self, event: MediaEvent, full_animation: bool, duration_ms: u64) {
        if !self.enabled {
            return;
        }
        // A show is a meaningful pill-update boundary: re-resolve the Auto
        // layout from the current foreground before the frame geometry is
        // computed, so a pill that appears over a fullscreen game (or over a
        // listed app) is compact from its very first frame.
        self.refresh_layout();
        // A fresh pill invalidates the idle-release deadline: the frame
        // buffers are about to be reused.
        unsafe {
            let _ = KillTimer(self.hwnd, IDLE_BUFFER_TIMER_ID);
        }
        self.content = Some(event);
        self.resolve_pill_text();
        self.reset_scroll();
        let now = Instant::now();
        self.dismiss_at = Some(now + Duration::from_millis(duration_ms));
        // A fresh pill must not inherit hover state from the previous one:
        // re-arm hover-dismiss only if the cursor is still over the new pill.
        self.hover_dismiss_at = None;
        self.phase = if full_animation {
            Phase::Expanding(now)
        } else {
            Phase::Light(now)
        };
        self.sync_anim_timer();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        // Log the foreground window so we can tell what was in front when the
        // pill fired (an exclusive-fullscreen game hides topmost windows).
        let foreground = unsafe { GetForegroundWindow() };
        debug!(
            "pill shown | duration_ms={duration_ms} | fg=0x{:x}",
            foreground.0 as usize
        );
        self.render();
    }

    /// Re-resolves the effective layout from the current foreground. Runs at
    /// every show boundary (a fresh decision per meaningful pill update) and
    /// from the static-tick re-check. The process table is re-enumerated
    /// only when the foreground HWND changed since the last decision; the
    /// fullscreen geometry of an unchanged window is re-read on every call
    /// (cheap window/monitor queries).
    fn refresh_layout(&mut self) {
        let verdict = self.sample_foreground();
        let decided = decide_layout(&self.config, &verdict);
        if decided != self.layout {
            debug!(
                "overlay layout: {:?} (mode={:?} fullscreen={} source={:?})",
                decided, self.config.overlay.layout, verdict.fullscreen, verdict.exe
            );
            self.layout = decided;
        }
    }

    /// Samples the foreground window once: the fullscreen verdict is always
    /// recomputed (cheap window/monitor reads), while the executable
    /// identity is cached with the foreground HWND, so the process table is
    /// enumerated only when the foreground window actually changed.
    fn sample_foreground(&mut self) -> ForegroundVerdict {
        let foreground = unsafe { GetForegroundWindow() };
        let fullscreen = window_is_fullscreen(foreground, self.hwnd);
        let exe = if self.layout_fg == Some(foreground) {
            self.layout_fg_exe.clone()
        } else {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(foreground, Some(&mut pid)) };
            let exe = if pid == 0 {
                None
            } else {
                crate::process_picker::exe_name_for_pid(pid)
            };
            self.layout_fg = Some(foreground);
            self.layout_fg_exe = exe.clone();
            exe
        };
        ForegroundVerdict { exe, fullscreen }
    }

    /// The static-tick re-check for Auto layout: reacts to a foreground
    /// change within one static tick (250 ms) even when no media event
    /// arrives (e.g. an alt-tab into a fullscreen game while the pill is
    /// up). The full decision (process enumeration) runs only when the
    /// foreground HWND changed; an unchanged window gets its fullscreen
    /// geometry re-checked at most once per second — a same-window resize
    /// (fullscreen toggle) cannot matter more often than that. Returns
    /// whether the layout flipped, so the caller can force a re-render.
    fn tick_layout_check(&mut self) -> bool {
        let now = Instant::now();
        let foreground = unsafe { GetForegroundWindow() };
        let hwnd_changed = self.layout_fg != Some(foreground);
        let geometry_due = hwnd_changed
            || self
                .last_geometry_check
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if !geometry_due {
            return false;
        }
        self.last_geometry_check = Some(now);
        let before = self.layout;
        self.refresh_layout();
        self.layout != before
    }

    fn tick(&mut self) {
        let now = Instant::now();
        // A tick can be delivered after the pill was hidden (one was already
        // queued when hide() ran). The hidden phase must not re-arm the
        // refresh-rate timer or do any per-tick work.
        if matches!(self.phase, Phase::Hidden) {
            self.last_tick = now;
            return;
        }
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        // A layered popup can be hidden by fullscreen transitions or external
        // ShowWindow calls; re-assert visibility and topmost z-order while a
        // pill should be up. While the pill is fully shown this is throttled
        // to 1 Hz — the window state cannot meaningfully change every 4 ms.
        let animating = !matches!(self.phase, Phase::Shown);
        if !matches!(self.phase, Phase::Hidden)
            && (animating || self.last_reassert.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)))
        {
            self.last_reassert = Some(now);
            unsafe {
                if !IsWindowVisible(self.hwnd).as_bool() {
                    let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                }
                if let Err(error) = SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                ) {
                    debug!("pill SetWindowPos(topmost) failed: {error}");
                }
            }
        }
        // Hover-to-dismiss: the first tick that finds the cursor over the
        // pill caps the remaining time at 500ms. One-way: leaving the pill
        // before that does not cancel the early dismissal.
        if !matches!(self.phase, Phase::Hidden) {
            if self.is_cursor_over_pill() && self.hover_dismiss_at.is_none() {
                self.hover_dismiss_at = Some(now);
                self.dismiss_at = Some(now + Duration::from_millis(EARLY_EXIT_MS));
                debug!("pill hover-dismiss armed");
            }
        } else {
            self.hover_dismiss_at = None;
            self.dismiss_at = None;
        }
        if self.dismiss_at.is_some_and(|deadline| deadline <= now)
            && !matches!(self.phase, Phase::Collapsing(_) | Phase::Hidden)
        {
            self.phase = Phase::Collapsing(now);
        }

        match self.phase {
            Phase::Expanding(start) if start.elapsed() >= animation_duration(&self.config) => {
                self.phase = Phase::Shown;
                debug!("pill phase -> shown");
            }
            Phase::Light(start) if start.elapsed() >= LIGHT_DURATION => {
                self.phase = Phase::Shown;
                debug!("pill phase -> shown");
            }
            Phase::Collapsing(start) if start.elapsed() >= animation_duration(&self.config) => {
                self.hide();
                return;
            }
            _ => {}
        }

        // Advance marquee offsets (driven by this same tick, entirely
        // independent of the dismiss countdown). Time-based so the scroll
        // speed is identical at any frame rate. The DPI scale is queried
        // only while a line is actually scrolling: a static pill repaints
        // nothing, so its coarse tick must not pay for a per-tick DPI call.
        let marquee_active = self.scroll.iter().any(|line| line.scrolling);
        let scale = if marquee_active {
            let dpi = unsafe { GetDpiForWindow(self.hwnd) };
            dpi.max(96) as f32 / 96.0
        } else {
            1.0
        };
        let per_tick = MARQUEE_SPEED * scale * dt;
        for line in &mut self.scroll {
            if let Some(started) = line.started_at
                && started.elapsed() >= MARQUEE_HOLD
            {
                if line.offset == 0.0 {
                    debug!("marquee scroll started | offset advancing");
                }
                line.offset += per_tick;
            }
        }
        // Auto layout re-check: a foreground change flips the pill between
        // layouts within one static tick even when no media event arrives
        // (an alt-tab into a fullscreen game mid-pill). Only the static tick
        // runs it; animation frames skip it, so a flip never lands mid-
        // expand/collapse. A flipped layout forces a render (the pill's
        // size, content layout and placement all change with it).
        let layout_flipped = self.config.overlay.layout == LayoutMode::Auto && !animating && self.tick_layout_check();
        // A fully-shown pill is static unless a marquee line is actually
        // overflowing: skip the render (and its UpdateLayeredWindow) entirely
        // when nothing changed. The animation phases still repaint every tick.
        if layout_flipped || animating || marquee_active {
            self.render();
        }
        // Re-sync the timer to the phase: a static pill drops to the coarse
        // tick, a phase transition or a marquee line starting restores the
        // refresh-rate cadence.
        self.sync_anim_timer();
    }

    fn render(&mut self) {
        let Some(content) = self.content.take() else {
            return;
        };
        // Resolve the target display first: both the DPI and the work area
        // must come from the display the pill is about to appear on, so the
        // first frame after a display switch is already correct.
        let Some(target) = self.target() else {
            self.content = Some(content);
            return;
        };
        let (alpha, shape) = self.frame();
        let raw_dpi = monitor_dpi(target.handle);
        if raw_dpi != self.fonts.dpi() {
            self.fonts = FontProvider::new(raw_dpi);
        }
        let dpi = raw_dpi as f32 / 96.0;
        let compact = self.layout == LayoutMode::Compact;
        let (logical_width, logical_height) = content_size_of(&self.config, &content, compact);
        let width = (logical_width * dpi * shape).round().max(1.0) as i32;
        let height = (logical_height * dpi * shape).round().max(1.0) as i32;
        self.aura_inset = (AURA_HALO_LOGICAL * dpi * shape).round() as i32;
        let position = placement(target.work, width, height, self.active_pos(), self.aura_inset, dpi);
        let result = render_layered(self, &content, width, height, dpi * shape, alpha, position, compact);
        self.content = Some(content);
        if let Err(error) = result {
            error!("rendering overlay: {error:#}");
        }
    }

    fn frame(&self) -> (u8, f32) {
        // Animation frames start visibly (never alpha 0): the pill must be
        // seen from the first render even if a tick is delayed, and the fade
        // from ~25% reads just as smoothly.
        match self.phase {
            Phase::Hidden => (0, 0.55),
            Phase::Expanding(start) => {
                let total_dur = animation_duration(&self.config).as_secs_f32();
                let t = (start.elapsed().as_secs_f32() / total_dur).clamp(0.0, 1.0);
                // Opacity lands by ~35% of the animation so the pill reads as
                // solid while it is still growing; scale carries the spring.
                let alpha_t = (t / 0.35).min(1.0);
                let alpha = (64.0 + ease_out_quint(alpha_t) * 191.0) as u8;
                let shape = 0.55 + ease_out_back(t) * 0.45;
                (alpha, shape)
            }
            Phase::Light(start) => {
                let progress = ease_out_quint(start.elapsed().as_secs_f32() / LIGHT_DURATION.as_secs_f32());
                ((64.0 + progress * 191.0) as u8, 1.0)
            }
            Phase::Shown => (255, 1.0),
            Phase::Collapsing(start) => {
                let progress =
                    ease_out_quint(start.elapsed().as_secs_f32() / animation_duration(&self.config).as_secs_f32());
                (((1.0 - progress) * 255.0) as u8, 1.0 - progress * 0.45)
            }
        }
    }

    /// Re-resolves the target display for the configured monitor mode. Fresh
    /// on every call — the system monitor enumeration is cheap, and handles
    /// are never cached, so a hot-plugged or reordered display takes effect
    /// on the very next frame.
    fn target(&self) -> Option<TargetMonitor> {
        let displays = enumerate_displays();
        let foreground_nearest = foreground_monitor_index(&displays);
        let index = resolve_target(self.position.monitor, &displays, foreground_nearest)?;
        let display = &displays[index];
        let target = TargetMonitor {
            handle: display.handle,
            work: display.work,
            index,
            primary: display.primary,
        };
        log_target_once(&target, &display.name);
        Some(target)
    }

    /// Whether the current pill is compact: the applied layout is Compact.
    fn effective_compact(&self) -> bool {
        self.layout == LayoutMode::Compact
    }

    /// The position governing the current pill. A compact pill uses the
    /// independent compact position only while `compact_position_separate` is
    /// set; otherwise it sits exactly where the expanded pill would — the
    /// same shared rule (`compact_effective`) the settings UI highlights,
    /// so the preview and the pill can never disagree.
    fn active_pos(&self) -> &OverlayPos {
        if self.effective_compact() && self.config.overlay.compact_position_separate {
            &self.compact_position
        } else {
            &self.position
        }
    }

    /// The pill's screen top-left for a `width`×`height` window on the
    /// currently resolved target display, or `None` when no display is
    /// available.
    fn position(&self, width: i32, height: i32) -> Option<POINT> {
        let target = self.target()?;
        let scale = monitor_dpi(target.handle) as f32 / 96.0;
        Some(placement(
            target.work,
            width,
            height,
            self.active_pos(),
            self.aura_inset,
            scale,
        ))
    }

    /// Whether the cursor currently sits over the pill body (not the aura
    /// ring). The overlay window is `WS_EX_TRANSPARENT`, so it receives no
    /// mouse messages; the cursor is polled instead on the animation tick.
    fn is_cursor_over_pill(&self) -> bool {
        let mut pt = POINT::default();
        if unsafe { GetCursorPos(&mut pt) }.is_err() {
            return false;
        }
        let Some((width, height)) = self.content_size() else {
            return false;
        };
        let Some(pos) = self.position(width, height) else {
            return false;
        };
        let inset = self.aura_inset;
        pt.x >= pos.x + inset
            && pt.x <= pos.x + width + inset
            && pt.y >= pos.y + inset
            && pt.y <= pos.y + height + inset
    }

    /// Moves the live overlay window to its resolved position without a full redraw.
    fn reposition(&mut self) {
        if matches!(self.phase, Phase::Hidden) {
            return;
        }
        let Some((width, height)) = self.content_size() else {
            return;
        };
        let Some(point) = self.position(width, height) else {
            return;
        };
        unsafe {
            if let Err(error) = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                point.x,
                point.y,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOOWNERZORDER,
            ) {
                debug!("SetWindowPos(reposition) failed: {error}");
            }
        }
    }

    fn hide(&mut self) {
        debug!("pill hidden");
        self.content = None;
        self.dismiss_at = None;
        self.hover_dismiss_at = None;
        self.phase = Phase::Hidden;
        // Release the per-show render state: the next show re-converts the
        // artwork and rebuilds the marquee rasters from the cached track, so
        // an idle pill holds no decoded cover or raster buffers. The
        // size-reuse buffers (`dib`, `frame_scratch`) and the caches
        // (`track_cache`, fonts) stay.
        self.decoded_art = None;
        self.decoded_art_source = None;
        self.palette = None;
        self.marquee_strips = [None, None, None, None];
        self.pill_text = None;
        // Clear the last-shown source label so a subsequent PlaybackStateChanged
        // from the same source is no longer treated as redundant with a track
        // pill that has already collapsed. The label is re-set in show_next()
        // if a fresh TrackChanged is queued.
        self.current_source = None;
        self.delete_anim_timer();
        unsafe {
            // Do NOT kill the debounce timer here: an event that arrived while
            // the pill was collapsing still has a pending debounce, and killing
            // it here silently drops that event (a pill that never shows).
            // toggle_enabled clears the pending events explicitly instead.
            if !ShowWindow(self.hwnd, SW_HIDE).as_bool() {
                debug!("ShowWindow(SW_HIDE) failed");
            }
            if let Err(error) = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_HIDEWINDOW | SWP_NOACTIVATE | SWP_NOZORDER,
            ) {
                debug!("SetWindowPos(hide) failed: {error}");
            }
        }
        // The size-reuse buffers (`dib`, `frame_scratch`) only pay off for
        // closely-spaced pills; when the pill stays hidden, schedule their
        // release so a long-idle process holds no frame DIBs. Every show path
        // kills the timer, so this only fires if no fresh pill appears within
        // the deadline; the buffers are rebuilt lazily on the next show.
        unsafe {
            let _ = SetTimer(self.hwnd, IDLE_BUFFER_TIMER_ID, IDLE_BUFFER_RELEASE_MS, None);
        }
        // Advance the queue: the next pending notification shows as a fresh
        // pill. show() checks `enabled`, so a toggle-off collapse stays hidden.
        self.show_next();
    }

    /// Releases the size-reuse buffers after the pill has been hidden for a
    /// long stretch (fired by `IDLE_BUFFER_TIMER_ID`). The next show rebuilds
    /// them lazily (`dib_for`, `clear_frame_scratch`, and the text scratch
    /// creation), so the cost of a release is one CreateDIBSection round on
    /// the next pill, not on the release itself.
    fn release_idle_buffers(&mut self) {
        if let Some(scratch) = self.text_scratch.take() {
            unsafe {
                let _ = SelectObject(scratch.hdc, scratch.old_bitmap);
                let _ = DeleteObject(scratch.bitmap);
                let _ = DeleteDC(scratch.hdc);
            }
        }
        if let Some(dib) = self.dib.take() {
            unsafe {
                let _ = SelectObject(dib.hdc, dib.old_bitmap);
                let _ = DeleteObject(dib.bitmap);
                let _ = DeleteDC(dib.hdc);
            }
        }
        self.frame_scratch = Vec::new();
        debug!("released idle overlay buffers");
    }

    fn toggle_enabled(&mut self) {
        self.enabled = !self.enabled;
        info!("notifications {}", if self.enabled { "enabled" } else { "disabled" });
        if !self.enabled {
            self.pending.clear();
            self.hide();
        }
    }

    /// Current (scaled) pixel size of the shown content, or `None` while hidden.
    fn content_size(&self) -> Option<(i32, i32)> {
        let content = self.content.as_ref()?;
        let (_, shape) = self.frame();
        let dpi = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let (logical_width, logical_height) =
            content_size_of(&self.config, content, self.layout == LayoutMode::Compact);
        let width = (logical_width * dpi * shape).round().max(1.0) as i32;
        let height = (logical_height * dpi * shape).round().max(1.0) as i32;
        Some((width, height))
    }

    /// Shows a short sample when the pill is currently hidden, so a settings
    /// change that affects what the pill looks like or where it sits is
    /// previewable even while nothing is playing. Returns whether a sample was
    /// shown; the caller then knows the visible-pill path is not needed.
    /// Shared by the position/layout/separation push functions instead of
    /// duplicating the phase check at each site.
    fn preview_if_hidden(&mut self) -> bool {
        if matches!(self.phase, Phase::Hidden) {
            self.show_sample();
            true
        } else {
            false
        }
    }

    /// Shows a short-lived preview of the overlay at its current position, used by
    /// the tray "Show sample" command to preview placement without real media.
    /// Shows the most recent real track (and its palette/aura) so the preview
    /// looks like an actual notification; on a fresh start before any track
    /// has been seen it falls back to a track-change pill with sample data.
    fn show_sample(&mut self) {
        debug!("sample pill shown");
        // A sample pill is a show: cancel the idle-release deadline so the
        // buffers survive the preview.
        unsafe {
            let _ = KillTimer(self.hwnd, IDLE_BUFFER_TIMER_ID);
        }
        // The sample follows the same layout decision as a real pill (the
        // settings preview must show what a real notification would show).
        self.refresh_layout();
        let content = self.last_track.clone().map_or_else(
            || {
                let track = TrackInfo {
                    title: "Sample Track".into(),
                    artist: "Sample Artist".into(),
                    album: "Sample Album".into(),
                    source_app: "Example Player".into(),
                    duration_secs: Some(3 * 60 + 45),
                    ..Default::default()
                };
                MediaEvent::TrackChanged(track)
            },
            MediaEvent::TrackChanged,
        );
        self.content = Some(content);
        self.resolve_pill_text();
        self.reset_scroll();
        let now = Instant::now();
        self.dismiss_at = Some(now + sample_duration(&self.config));
        self.hover_dismiss_at = None;
        self.phase = Phase::Light(now);
        self.sync_anim_timer();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        self.render();
    }
}

/// How long the "Show sample" preview stays visible: the configured duration,
/// clamped to the same floor used for real notifications.
fn sample_duration(config: &Config) -> Duration {
    Duration::from_millis(config.overlay.duration_ms.max(500))
}

/// Minimum extra visible time granted to a metadata refresh: capped at the
/// configured duration so a short setting is never silently extended.
fn update_min_duration(config: &Config) -> Duration {
    Duration::from_millis(config.overlay.duration_ms.min(1500))
}

/// Logical (96-DPI) size of a pill for the given content. Single source of
/// truth shared by `render()` and `content_size()` so they cannot drift.
/// `compact` selects the compact pill geometry (one title row, trailing app
/// icon and playback symbol) over the expanded four-row layout.
fn content_size_of(config: &Config, content: &MediaEvent, compact: bool) -> (f32, f32) {
    match content {
        MediaEvent::TrackChanged(_) | MediaEvent::PlaybackStateChanged(_, _) => {
            if compact {
                compact_size(config)
            } else {
                content_size(config)
            }
        }
        // Never shown (receive_events skips it); the .max(1.0) guards keep the
        // size sane if this dead arm is ever reached.
        MediaEvent::SessionRejected { .. } | MediaEvent::WorkerFailed { .. } => (0.0, 0.0),
    }
}

/// Logical (96-DPI) size of a pill: the configured max width and a constant
/// height that always reserves all four row bands (title, artist, meta,
/// source). A missing row leaves empty space at the bottom instead of
/// shrinking the pill, so every pill — track change, state change, any
/// source — is exactly the same size. Single source of truth used by both
/// `render()` and `content_size()` so they cannot drift.
fn content_size(config: &Config) -> (f32, f32) {
    let appearance = &config.appearance;
    let fs_artist = appearance.font_size_artist;
    let rows: [f32; 4] = [
        appearance.font_size_title * ROW_HEIGHT,
        fs_artist * ROW_HEIGHT,
        fs_artist * 0.85 * ROW_HEIGHT,
        fs_artist * 0.85 * ROW_HEIGHT,
    ];
    let text_h: f32 = rows.iter().sum();
    let height = (appearance.art_size as f32 + 2.0 * appearance.padding).max(text_h + 2.0 * appearance.padding + 8.0);
    (config.overlay.max_width.max(180) as f32, height)
}

/// Derived per-element metrics of the compact pill (logical px). Single
/// source of truth shared by `compact_size` (window sizing) and the compact
/// draw path (element placement), so the title viewport can never drift
/// from the pill width. Each element reuses an expanded-pill convention:
/// the art tile fits the title row band (the state pill's art clamp), the
/// app icon is the 16 px base the app row uses, and the playback symbol is
/// the title font × 1.5 capped at the row height the expanded title row
/// uses.
struct CompactMetrics {
    /// Art tile side length.
    art: f32,
    /// App icon side length.
    icon: f32,
    /// Playback symbol box size.
    symbol: f32,
}

fn compact_metrics(config: &Config) -> CompactMetrics {
    let appearance = &config.appearance;
    let row_h = appearance.font_size_title * ROW_HEIGHT;
    CompactMetrics {
        art: (appearance.art_size as f32).min(row_h).max(1.0),
        icon: 16.0,
        symbol: (appearance.font_size_title * 1.5).min(row_h).max(1.0),
    }
}

/// Logical (96-DPI) size of the compact pill: one title row high, and wide
/// enough for `[ART] [TITLE] [APP ICON] [▶]`, with the title band taking
/// half the configured max width (floored at the 180 px minimum pill
/// width). The total is capped at max_width, so a compact pill is never
/// wider than the expanded one; when the cap bites, the title viewport
/// (derived from the same metrics) simply shrinks and the title marquees.
fn compact_size(config: &Config) -> (f32, f32) {
    let appearance = &config.appearance;
    let metrics = compact_metrics(config);
    let max_w = config.overlay.max_width.max(180) as f32;
    let title = (max_w * 0.5).clamp(180.0, (max_w - 160.0).max(180.0));
    let width = (2.0 * appearance.padding + metrics.art + 12.0 + title + 6.0 + metrics.icon + 16.0 + metrics.symbol)
        .min(max_w)
        .max(1.0);
    let height = (appearance.font_size_title * ROW_HEIGHT + 2.0 * appearance.padding).max(1.0);
    (width, height)
}

/// Horizontal extents of the compact pill's title viewport (logical px,
/// relative to the pill body): everything between the art tile and the
/// trailing app icon. The icon, its gap and the playback symbol are all
/// excluded, so marquee text and the edge fade can never render under them.
fn compact_title_viewport(config: &Config) -> (f32, f32) {
    let metrics = compact_metrics(config);
    let appearance = &config.appearance;
    let (pill_w, _) = compact_size(config);
    let left = appearance.padding + metrics.art + 12.0;
    let right = pill_w - appearance.padding - metrics.symbol - 16.0 - metrics.icon - 6.0;
    (left, right)
}

/// Forces the live overlay at `hwnd` to preview its current placement.
pub(crate) fn show_sample(hwnd: HWND) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = window_state::<OverlayState>(hwnd);
        if !state_ptr.is_null() {
            (*state_ptr).show_sample();
        }
    }
}

/// Set when this window's WM_NCCREATE claims the state box handed over in
/// `lpCreateParams`, so a failed CreateWindowExW can tell whether the box was
/// taken by the system (and freed in WM_NCDESTROY) or still belongs to the
/// caller. Window creation is single-threaded on the UI thread, so a plain
/// atomic flag per window class is race-free.
static OVERLAY_STATE_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Creates the passive WinGlance overlay window. It owns no message loop: the caller
/// runs the loop and destroys the window at exit.
pub(crate) fn create_window(config: Config, queue: EventQueue, wake: Arc<AtomicBool>) -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.context("getting the process module")?;
    let instance: HINSTANCE = module.into();
    let class_name = wide("WinGlanceOverlayWindow");
    register_window_class(instance, &class_name)?;

    let mut state = Box::new(OverlayState::new(config, queue));
    state.wake = wake;
    let state_ptr = Box::into_raw(state);
    OVERLAY_STATE_CLAIMED.store(false, Ordering::SeqCst);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("WinGlance").as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            Some(state_ptr.cast()),
        )
    };
    match hwnd {
        Ok(hwnd) => Ok(hwnd),
        Err(error) => {
            // The state box is owned by the window from WM_NCCREATE onward and
            // freed in WM_NCDESTROY. WM_NCCREATE flips OVERLAY_STATE_CLAIMED
            // when it takes the box; if it never ran (a creation failure before
            // the window object existed), the box still belongs to us and must
            // be freed here — otherwise it leaks. When WM_NCCREATE did run,
            // the system tears the window down through WM_NCDESTROY first, so
            // freeing the box here would double-free it.
            if !OVERLAY_STATE_CLAIMED.load(Ordering::SeqCst) {
                unsafe {
                    drop(Box::from_raw(state_ptr));
                }
            }
            Err(error.into())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_layered(
    state: &mut OverlayState,
    content: &MediaEvent,
    width: i32,
    height: i32,
    scale: f32,
    alpha: u8,
    position: POINT,
    compact: bool,
) -> Result<()> {
    let inset = state.aura_inset;
    let buf_w = (width + inset * 2).max(1);
    let buf_h = (height + inset * 2).max(1);
    // The DIB backing buffer may be larger than the requested frame (dib_for
    // allocates to a generous upper bound and reuses it across animation
    // frames instead of recreating it every tick). Its real scanline stride
    // is therefore `alloc_w`, which only equals `buf_w` when the pill is at
    // its fully expanded size. Rendering straight into it at a `buf_w`
    // stride was tried once and tore the image (every row past the first
    // landed at the wrong offset). To avoid threading a second stride
    // parameter through every pixel-writing function, render into a
    // tightly-packed scratch buffer at the *requested* size instead — the
    // stride `draw_pixels`/`draw_text_pixels` have always assumed — and
    // blit the result into the real DIB at its real stride right before the
    // GDI call. The scratch buffer is grown across frames (and shrunk back
    // below only when an oversized frame inflates it), so after warm-up this
    // performs no per-frame heap allocation, matching the existing
    // `text_scratch` buffer's pattern elsewhere in this file.
    let (hdc, _bitmap, bits) = dib_for(state, buf_w, buf_h)?;
    let alloc_w = state.dib.as_ref().map(|dib| dib.width).unwrap_or(buf_w) as usize;
    let alloc_h = state.dib.as_ref().map(|dib| dib.height).unwrap_or(buf_h) as usize;

    let needed = buf_w as usize * buf_h as usize * 4;
    let mut scratch = std::mem::take(&mut state.frame_scratch);
    clear_frame_scratch(&mut scratch, needed);
    draw_pixels(
        state,
        &mut scratch[..needed],
        content,
        buf_w as usize,
        buf_h as usize,
        scale,
        compact,
    )?;
    draw_text_pixels(state, &mut scratch[..needed], content, buf_w, buf_h, scale, compact);
    // A single oversized metadata string (huge title/album) can inflate the
    // retained UTF-16 scratch far beyond any real row; shrink it back so the
    // capacity does not stay bloated for the rest of the run.
    if state.scratch_utf16.capacity() > 8192 {
        state.scratch_utf16.shrink_to(4096);
    }
    // A single oversized frame (wide max_width on a high-DPI monitor) can
    // inflate the packed frame scratch the same way; shrink it back so the
    // capacity does not stay bloated for the rest of the run.
    shrink_frame_scratch(&mut scratch, needed);
    state.frame_scratch = scratch;

    // Blit the packed frame into the real DIB, row by row, at the DIB's real
    // stride. `dib_for` guarantees `alloc_w >= buf_w` and `alloc_h >= buf_h`,
    // so `dib_len` stays within the buffer's real allocated capacity
    // (`alloc_w * alloc_h * 4`).
    let dib_len = alloc_w * buf_h as usize * 4;
    let dib_slice = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), dib_len.min(alloc_w * alloc_h * 4)) };
    blit_packed_rows(
        dib_slice,
        alloc_w * 4,
        &state.frame_scratch,
        buf_w as usize * 4,
        buf_h as usize,
    );

    let size = SIZE { cx: buf_w, cy: buf_h };
    let source = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: 0,
        BlendFlags: 0,
        SourceConstantAlpha: alpha,
        AlphaFormat: 1,
    };
    let result = unsafe {
        windows::Win32::UI::WindowsAndMessaging::UpdateLayeredWindow(
            state.hwnd,
            None,
            Some(&position),
            Some(&size),
            hdc,
            Some(&source),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    };
    unsafe {
        let _ = SetWindowPos(
            state.hwnd,
            HWND_TOPMOST,
            position.x,
            position.y,
            buf_w,
            buf_h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
    result.context("UpdateLayeredWindow")
}

/// Grows the reusable frame buffer when needed and clears the entire region
/// that this frame will present. `Vec::resize` preserves existing elements, so
/// clearing only in the no-growth branch leaves old animation pixels behind
/// while the pill expands.
fn clear_frame_scratch(scratch: &mut Vec<u8>, needed: usize) {
    if scratch.len() < needed {
        scratch.resize(needed, 0);
    }
    scratch[..needed].fill(0);
}

/// Shrinks the packed frame scratch back when an oversized frame has inflated
/// it far beyond any real pill size. The buffer is grown on demand across
/// frames (no per-frame allocation after warm-up); this releases capacity only
/// when the needed size has dropped to half the allocated capacity, so the
/// normal expand/collapse animation never reallocates. Pure and GDI-free so it
/// can be unit tested directly.
fn shrink_frame_scratch(scratch: &mut Vec<u8>, needed: usize) {
    if scratch.capacity() > needed * 2 {
        scratch.shrink_to(needed);
    }
}

/// Copies `rows` rows of `row_bytes` each from a tightly-packed `src` buffer
/// into `dst`, which uses a real stride of `dst_stride_bytes` per row
/// (`dst_stride_bytes >= row_bytes`; equal when the destination has no extra
/// padding). Used to blit the packed per-frame scratch buffer into the
/// oversized, reused DIB backing buffer, whose real scanline stride does not
/// match the requested frame size during most of the expand/collapse
/// animation. Pure and GDI-free so it can be unit tested directly.
fn blit_packed_rows(dst: &mut [u8], dst_stride_bytes: usize, src: &[u8], row_bytes: usize, rows: usize) {
    debug_assert!(row_bytes <= dst_stride_bytes);
    debug_assert!(src.len() >= row_bytes * rows);
    if rows == 0 || row_bytes == 0 {
        return;
    }
    debug_assert!(dst.len() >= dst_stride_bytes * (rows - 1) + row_bytes);
    for row in 0..rows {
        let src_off = row * row_bytes;
        let dst_off = row * dst_stride_bytes;
        dst[dst_off..dst_off + row_bytes].copy_from_slice(&src[src_off..src_off + row_bytes]);
    }
}

/// Generous upper bound on the DIB backing buffer for the current config:
/// the pill's logical size never exceeds `max_width` wide and the fitted
/// height for the largest allowed art/font rows (both from
/// `content_size_of`), inflated by the aura halo extent on every side, the
/// ~3% ease-out-back shape overshoot mid-expand, and rounding. Allocating to
/// this bound means animation frames reuse the buffer instead of recreating
/// it every tick; a request that still exceeds it (e.g. config changed
/// mid-run) just recreates once — the bound is an efficiency knob, never a
/// correctness constraint.
fn backing_upper_bound(config: &Config, dpi: u32) -> (i32, i32) {
    let dpi = dpi.max(96) as f32 / 96.0;
    let appearance = &config.appearance;
    let max_w = config.overlay.max_width.max(180) as f32;
    let max_text_h = 4.0 * appearance.font_size_title.max(appearance.font_size_artist) * ROW_HEIGHT;
    let max_h =
        (appearance.art_size as f32 + 2.0 * appearance.padding).max(max_text_h + 2.0 * appearance.padding + 8.0);
    let aura_px = AURA_HALO_LOGICAL;
    let scale = dpi * 1.1;
    (
        ((max_w + 2.0 * aura_px) * scale).ceil() as i32,
        ((max_h + 2.0 * aura_px) * scale).ceil() as i32,
    )
}

/// Creates a compatible DC with a top-down 32-bit DIB of the given size,
/// releasing the DC when the DIB cannot be created. Callers select the bitmap
/// into the DC and own both handles.
fn create_dc_with_dib(width: i32, height: i32) -> Result<(HDC, HBITMAP, *mut c_void)> {
    let hdc = unsafe { CreateCompatibleDC(None) };
    if hdc.0.is_null() {
        anyhow::bail!("CreateCompatibleDC failed");
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = null_mut();
    let bitmap = match unsafe { CreateDIBSection(hdc, &info, DIB_RGB_COLORS, &mut bits, None, 0) } {
        Ok(bitmap) => bitmap,
        Err(error) => {
            unsafe {
                let _ = DeleteDC(hdc);
            }
            return Err(error.into());
        }
    };
    if bits.is_null() {
        unsafe {
            let _ = DeleteDC(hdc);
        }
        anyhow::bail!("CreateDIBSection returned no pixel buffer");
    }
    Ok((hdc, bitmap, bits))
}

/// Returns the cached DIB for the given size, creating (or replacing) it when
/// the cache is too small. The backing buffer is allocated to the generous
/// config bound, so during expand/collapse the requested size changes every
/// frame but the buffer is created once and reused for the rest of the
/// process's life (per DPI/config). The DIB stays alive across frames and is
/// released at window destruction. The returned buffer's *real* scanline
/// stride is `state.dib`'s cached `width`, which may be larger than the
/// requested `width` — callers must not draw into it directly at the
/// requested width as the stride (see `render_layered`, which renders into a
/// packed scratch buffer and blits into this one via `blit_packed_rows`
/// instead).
fn dib_for(state: &mut OverlayState, width: i32, height: i32) -> Result<(HDC, HBITMAP, *mut c_void)> {
    if let Some(dib) = &state.dib {
        if dib.width >= width && dib.height >= height {
            return Ok((dib.hdc, dib.bitmap, dib.bits));
        }
        unsafe {
            let _ = SelectObject(dib.hdc, dib.old_bitmap);
            let _ = DeleteObject(dib.bitmap);
            let _ = DeleteDC(dib.hdc);
        }
        state.dib = None;
    }
    let (bound_w, bound_h) = backing_upper_bound(&state.config, state.fonts.dpi());
    let alloc_w = width.max(bound_w).max(1);
    let alloc_h = height.max(bound_h).max(1);
    let (hdc, bitmap, bits) = create_dc_with_dib(alloc_w, alloc_h)?;
    let old_bitmap = unsafe { SelectObject(hdc, bitmap) };
    state.dib = Some(DibCache {
        hdc,
        bitmap,
        old_bitmap,
        bits,
        width: alloc_w,
        height: alloc_h,
    });
    Ok((hdc, bitmap, bits))
}

fn draw_pixels(
    state: &mut OverlayState,
    pixels: &mut [u8],
    content: &MediaEvent,
    width: usize,
    height: usize,
    scale: f32,
    compact: bool,
) -> Result<()> {
    let radius = state.config.appearance.corner_radius * scale;
    // Resolve the artwork that will be displayed and convert it (once per
    // unique cover) up front, so the aura palette below is ready and the
    // cover is never shown stale. Track pills carry the worker's decode
    // directly; state pills reuse the cached track's for the source.
    let decoded: Option<Arc<[u8]>> = match content {
        MediaEvent::TrackChanged(track) => track.decoded_art.clone(),
        MediaEvent::PlaybackStateChanged(_, source_app) => {
            if source_app.is_empty() {
                None
            } else {
                state
                    .track_cache
                    .get(source_app)
                    .and_then(|(t, _)| t.decoded_art.clone())
            }
        }
        MediaEvent::SessionRejected { .. } | MediaEvent::WorkerFailed { .. } => None,
    };
    state.ensure_art(decoded.as_ref());
    let inset = state.aura_inset as usize;
    let pill_w = width.saturating_sub(inset * 2);
    let pill_h = height.saturating_sub(inset * 2);
    // Aura: painted first (underneath the pill body) in the full buffer,
    // fading outside the pill boundary. Uses the decoded artwork's palette
    // when available; otherwise falls back to the config accent so even
    // palette-less pills (e.g. the sample) glow.
    let aura_palette = state.palette.unwrap_or(Palette {
        primary: state.config.appearance.accent_color,
        secondary: state.config.appearance.accent_color,
    });
    // The pill fill picks up a hint of the cover's hue when a palette is
    // available; palette-less pills (e.g. the sample) keep the configured
    // fill exactly.
    let effective_bg = if state.palette.is_some() {
        tinted_fill(
            state.config.appearance.background_color,
            aura_palette.primary,
            FILL_TINT_WEIGHT,
        )
    } else {
        state.config.appearance.background_color
    };
    draw_aura(
        pixels,
        width,
        height,
        aura_palette,
        inset,
        pill_w,
        pill_h,
        radius,
        scale,
    );

    // Pill body: filled rounded rect inset from the DIB edges, leaving the
    // outer ring transparent for the aura glow. Rendered on top of the aura
    // so the smooth supersampled edge blends with the glow beneath it. The
    // loop spans the full `0..width` / `0..height` range so the exterior
    // anti-aliasing pixels (which carry the supersampled blend at the rounded
    // corners and right edge) are not truncated by `inset + pill_w`.
    for y in 0..height {
        for x in 0..width {
            let coverage = round_rect_coverage_supersampled(
                (x as i32 - inset as i32) as f32,
                (y as i32 - inset as i32) as f32,
                pill_w as f32,
                pill_h as f32,
                radius,
            );
            if coverage > 0.0 {
                let alpha = (effective_bg[3] as f32 * coverage) as u32;
                composite(
                    pixels,
                    width,
                    x,
                    y,
                    [effective_bg[0], effective_bg[1], effective_bg[2]],
                    alpha,
                );
            }
        }
    }

    // Directional edge highlight: white stroke on the pill's own boundary,
    // brighter along the top-left than the bottom-right.
    draw_edge_stroke(pixels, width, inset, pill_w, pill_h, radius, scale);

    // The compact pill draws its own smaller art tile (plus the title row
    // and the trailing icon/symbol) in `draw_compact_pill`; drawing it here
    // as well would composite the halo, the cover and the rim twice. The
    // expanded pills draw the art tile at the configured art size.
    if !compact {
        match content {
            MediaEvent::TrackChanged(_) => {
                let padding = (state.config.appearance.padding * scale).round() as usize;
                let art_size = (state.config.appearance.art_size as f32 * scale).round() as usize;
                let art_radius = art_size as f32 * 0.2;
                let art_x = inset + padding;
                let art_y = inset + pill_h.saturating_sub(art_size) / 2;
                draw_art_tile(
                    pixels,
                    width,
                    state.palette,
                    state.config.appearance.accent_color,
                    art_x,
                    art_y,
                    art_size,
                    art_radius,
                    state.decoded_art.as_deref(),
                    scale,
                );
            }
            MediaEvent::PlaybackStateChanged(_, _) => {
                // State pills reuse the cached track's artwork for the source that
                // produced the state change, so a pause/play pill still shows the
                // right cover. Falls back to the accent placeholder when nothing
                // has been cached for this source yet. The art size is clamped to
                // the pill body: the state-pill layout reserves no extra rows.
                let padding = (state.config.appearance.padding * scale).round() as usize;
                let art_size = (state.config.appearance.art_size as f32 * scale).round() as usize;
                let art_size = art_size.min(pill_h.saturating_sub(2 * padding));
                let art_radius = art_size as f32 * 0.2;
                let art_x = inset + padding;
                let art_y = inset + pill_h.saturating_sub(art_size) / 2;
                draw_art_tile(
                    pixels,
                    width,
                    state.palette,
                    state.config.appearance.accent_color,
                    art_x,
                    art_y,
                    art_size,
                    art_radius,
                    state.decoded_art.as_deref(),
                    scale,
                );
            }
            // Never rendered: SessionRejected is filtered out before enqueue.
            MediaEvent::SessionRejected { .. } | MediaEvent::WorkerFailed { .. } => {}
        }
    }
    Ok(())
}

/// Draws the art tile at (art_x, art_y): the accent halo behind the square,
/// the cover (or the accent placeholder when no art decoded) and the glowing
/// rim. Shared by the track- and state-pill arms, which differ only in the
/// art-size clamp the caller applies. The mask radius must match the one
/// `draw_art_scaled` uses for the art bitmap itself, not the pill's
/// `corner_radius` — otherwise the halo/rim are rounder than the art beneath
/// them and visibly don't hug its corners.
#[allow(clippy::too_many_arguments)]
fn draw_art_tile(
    pixels: &mut [u8],
    width: usize,
    palette: Option<Palette>,
    accent: [u8; 4],
    art_x: usize,
    art_y: usize,
    art_size: usize,
    art_radius: f32,
    decoded_art: Option<&[u8]>,
    scale: f32,
) {
    // Album art halo: subtle accent glow behind the art square.
    if let Some(c) = palette.map(|p| p.primary) {
        let halo_pad = (1.5 * scale).round() as usize;
        let halo_size = art_size + halo_pad * 2;
        let halo_x = art_x.saturating_sub(halo_pad);
        let halo_y = art_y.saturating_sub(halo_pad);
        let halo_radius = art_radius + halo_pad as f32;
        for dy in 0..halo_size {
            for dx in 0..halo_size {
                let cov = round_rect_coverage(dx as f32, dy as f32, halo_size as f32, halo_size as f32, halo_radius);
                if cov > 0.0 {
                    let alpha = (c[3] as f32 * 0.75 * cov) as u32;
                    composite(pixels, width, halo_x + dx, halo_y + dy, [c[0], c[1], c[2]], alpha);
                }
            }
        }
    }
    if let Some(art) = decoded_art {
        draw_art_scaled(pixels, width, art, art_x, art_y, art_size, accent);
    } else {
        draw_placeholder(pixels, width, art_x, art_y, art_size, accent);
    }
    // Glowing rim: thin 1.5px accent stroke around the album art.
    if let Some(c) = palette.map(|p| p.primary) {
        let stroke_w = (1.5 * scale).round().max(1.0);
        for dy in 0..art_size {
            for dx in 0..art_size {
                let d = round_rect_signed_dist(dx as f32, dy as f32, art_size as f32, art_size as f32, art_radius);
                if d.abs() < stroke_w {
                    let edge = 1.0 - d.abs() / stroke_w;
                    let alpha = (c[3] as f32 * 0.9 * edge) as u32;
                    composite(pixels, width, art_x + dx, art_y + dy, [c[0], c[1], c[2]], alpha);
                }
            }
        }
    }
}

/// Directional edge highlight traced on the pill's own boundary — a
/// supersampled coverage ring (outer rounded-rect minus the same shape
/// inset by the stroke width), at low alpha and biased brighter along
/// the top-left than the bottom-right, to read as light catching a
/// physical cut edge rather than a flat outline. Purely a boundary
/// definition line; the aura glow (drawn earlier, underneath) is what
/// carries color outside it.
fn draw_edge_stroke(
    pixels: &mut [u8],
    width: usize,
    inset: usize,
    pill_w: usize,
    pill_h: usize,
    radius: f32,
    scale: f32,
) {
    const STROKE_COLOR: [u8; 3] = [255, 255, 255];
    const PEAK_ALPHA: f32 = 90.0;
    const MIN_ALPHA: f32 = 30.0;
    let stroke_w = (1.25 * scale).round().max(1.0);
    // Ring coverage = outer rounded-rect coverage minus the same shape
    // inset by stroke_w, both supersampled — the same technique the pill
    // fill uses (round_rect_coverage_supersampled), reused here so the
    // stroke gets correct anti-aliasing at the diagonal corners instead
    // of the single-sample banding the old d-based edge ramp produced.
    let inner_w = (pill_w as f32 - 2.0 * stroke_w).max(0.0);
    let inner_h = (pill_h as f32 - 2.0 * stroke_w).max(0.0);
    let inner_radius = (radius - stroke_w).max(0.0);
    for y in 0..pill_h {
        for x in 0..pill_w {
            let px = x as f32;
            let py = y as f32;
            let outer = round_rect_coverage_supersampled(px, py, pill_w as f32, pill_h as f32, radius);
            if outer <= 0.0 {
                continue;
            }
            let inner = round_rect_coverage_supersampled(px - stroke_w, py - stroke_w, inner_w, inner_h, inner_radius);
            let coverage = (outer - inner).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            // Diagonal light bias: brightest at top-left (0,0), dimmest
            // at bottom-right (pill_w, pill_h), normalized to [0, 1].
            let t = ((x as f32 / pill_w.max(1) as f32) + (y as f32 / pill_h.max(1) as f32)) * 0.5;
            let peak = PEAK_ALPHA - (PEAK_ALPHA - MIN_ALPHA) * t;
            let alpha = (peak * coverage).round() as u32;
            composite(pixels, width, inset + x, inset + y, STROKE_COLOR, alpha);
        }
    }
}

/// Draws the cached artwork bitmap into the tile region, bilinear-scaled from
/// the cached base size to the current (animation-scaled) size, with the
/// rounded-corner mask. Falls back to the accent placeholder on decode errors.
#[allow(clippy::too_many_arguments)]
fn draw_art_scaled(pixels: &mut [u8], width: usize, art: &[u8], x: usize, y: usize, size: usize, accent: [u8; 4]) {
    let base = (art.len() / 4) as f64;
    let base = base.sqrt() as usize;
    if size == 0 || base == 0 || base * base * 4 != art.len() {
        draw_placeholder(pixels, width, x, y, size, accent);
        return;
    }
    let radius = size as f32 * 0.2;
    for dy in 0..size {
        for dx in 0..size {
            let coverage = round_rect_coverage(dx as f32, dy as f32, size as f32, size as f32, radius);
            if coverage <= 0.0 {
                continue;
            }
            let sx = (dx as f32 + 0.5) * base as f32 / size as f32 - 0.5;
            let sy = (dy as f32 + 0.5) * base as f32 / size as f32 - 0.5;
            let x0 = sx.max(0.0) as usize;
            let y0 = sy.max(0.0) as usize;
            let x1 = (x0 + 1).min(base - 1);
            let y1 = (y0 + 1).min(base - 1);
            let fx = (sx - x0 as f32).clamp(0.0, 1.0);
            let fy = (sy - y0 as f32).clamp(0.0, 1.0);
            let p00 = (y0 * base + x0) * 4;
            let p10 = (y0 * base + x1) * 4;
            let p01 = (y1 * base + x0) * 4;
            let p11 = (y1 * base + x1) * 4;
            let r = lerp(lerp(art[p00], art[p10], fx), lerp(art[p01], art[p11], fx), fy);
            let g = lerp(
                lerp(art[p00 + 1], art[p10 + 1], fx),
                lerp(art[p01 + 1], art[p11 + 1], fx),
                fy,
            );
            let b = lerp(
                lerp(art[p00 + 2], art[p10 + 2], fx),
                lerp(art[p01 + 2], art[p11 + 2], fx),
                fy,
            );
            let a = lerp(
                lerp(art[p00 + 3], art[p10 + 3], fx),
                lerp(art[p01 + 3], art[p11 + 3], fx),
                fy,
            );
            let alpha = (a as f32 * coverage) as u32;
            composite(pixels, width, x + dx, y + dy, [r, g, b], alpha);
        }
    }
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

/// Draws a playback-state symbol (play ▶ / pause ‖ / stop ■ / music note ♪)
/// as custom anti-aliased vector shapes directly into the pixel buffer,
/// replacing the old GDI text glyphs. The symbol box is `size`×`size` pixels
/// (size = font height); the symbols are ~0.88×S tall, vertically centered in
/// the box. Pause bars are 0.22×S wide with a 0.16×S gap; play is a triangle
/// 0.60×S wide of the same height whose corners are rounded at the pause
/// bars' radius; pause and stop use rounded corners with radius 0.2×S
/// (clamped to half the bar width — capsule ends for the bars, matching the
/// artwork tile's `size * 0.2` rounding convention and the pill's soft look).
/// The symbol is positioned with its right edge at `right` and vertically
/// centered in its row band.
#[allow(clippy::too_many_arguments)]
fn draw_symbol_pixels(
    pixels: &mut [u8],
    width: usize,
    right: i32,
    y: i32,
    size: f32,
    playback: PlaybackState,
    color: [u8; 4],
) {
    let bar_w = 0.20 * size;
    let radius = (0.20 * size).min(bar_w / 2.0).max(0.0);
    let box_left = (right as f32 - size).round() as i32;
    // The symbols are ~0.88×S tall; center them in the S×S box.
    let v_center = y as f32 + size * 0.5;
    match playback {
        PlaybackState::Playing => {
            // Larger Triangle (▶) — synced to 0.88
            let icon_h = 0.88 * size;
            let tri_w = 0.60 * size;

            let left = box_left as f32 + (size - tri_w) * 0.5 + (tri_w * 0.05);
            let top = v_center - icon_h / 2.0;

            draw_triangle_filled(
                pixels,
                width,
                (left as i32, top as i32),
                ((left + tri_w) as i32, (top + icon_h / 2.0) as i32),
                (left as i32, (top + icon_h) as i32),
                radius,
                color,
            );
        }
        PlaybackState::Paused => {
            // Larger Rounded Bars (❚❚) — synced to 0.88
            let icon_h = 0.88 * size;
            let bar_w = (0.22 * size).round().max(2.0);
            let gap = (0.16 * size).round().max(2.0);

            let total = bar_w * 2.0 + gap;
            let origin = box_left as f32 + (size - total) * 0.5;

            for offset in [0.0, bar_w + gap] {
                draw_rounded_rect_filled(
                    pixels,
                    width,
                    (origin + offset) as i32,
                    (v_center - icon_h / 2.0) as i32,
                    bar_w as i32,
                    icon_h as i32,
                    radius,
                    color,
                );
            }
        }
        PlaybackState::Stopped => {
            // Larger Stop Square (◼) — scaled to 82% of 0.88 height for optical weight
            let icon_h = 0.88 * size;
            let sq = (icon_h * 0.82).round();
            let left = box_left as f32 + (size - sq) * 0.5;
            let top = v_center - sq / 2.0;

            draw_rounded_rect_filled(
                pixels,
                width,
                left as i32,
                top as i32,
                sq as i32,
                sq as i32,
                radius,
                color,
            );
        }
        PlaybackState::NowPlaying => {
            // Eighth note (♪) — synced to 0.88
            let note_h = 0.88 * size;
            let head_d = 0.40 * size;
            let stem_w = (0.14 * size).round().max(2.0);

            let head_x = box_left as f32 + 0.20 * size;
            let head_y = v_center + (note_h / 2.0) - head_d;

            draw_rounded_rect_filled(
                pixels,
                width,
                head_x.round() as i32,
                head_y.round() as i32,
                head_d.round() as i32,
                head_d.round() as i32,
                head_d / 2.0,
                color,
            );

            let stem_x = head_x + head_d - stem_w;
            let stem_top = v_center - (note_h / 2.0);
            let stem_h = (head_y + head_d * 0.5) - stem_top;

            draw_rounded_rect_filled(
                pixels,
                width,
                stem_x.round() as i32,
                stem_top.round() as i32,
                stem_w.round() as i32,
                stem_h.round() as i32,
                stem_w / 2.0,
                color,
            );

            let flag_w = 0.32 * size;
            let flag_h = 0.26 * size;

            draw_rounded_rect_filled(
                pixels,
                width,
                stem_x.round() as i32,
                stem_top.round() as i32,
                flag_w.round() as i32,
                flag_h.round() as i32,
                stem_w / 2.0,
                color,
            );
        }
    }
}

/// Fills a rounded rectangle into the pixel buffer using `round_rect_coverage`.
#[allow(clippy::too_many_arguments)]
fn draw_rounded_rect_filled(
    pixels: &mut [u8],
    width: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    radius: f32,
    color: [u8; 4],
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let r = radius.min(w as f32 / 2.0).min(h as f32 / 2.0);
    for dy in 0..h {
        for dx in 0..w {
            let cov = round_rect_coverage(dx as f32, dy as f32, w as f32, h as f32, r);
            if cov > 0.0 {
                let alpha = (color[3] as f32 * cov) as u32;
                composite(
                    pixels,
                    width,
                    (x + dx) as usize,
                    (y + dy) as usize,
                    [color[0], color[1], color[2]],
                    alpha,
                );
            }
        }
    }
}

/// Fills a triangle (given three pixel corners) with corners rounded to the
/// given radius into the pixel buffer, anti-aliased via signed-distance
/// coverage. Used only for the play symbol; the radius matches the pause
/// bars' capsule-end radius so all three symbols share the same rounding.
fn draw_triangle_filled(
    pixels: &mut [u8],
    width: usize,
    (ax, ay): (i32, i32),
    (bx, by): (i32, i32),
    (cx, cy): (i32, i32),
    radius: f32,
    color: [u8; 4],
) {
    let min_x = ax.min(bx).min(cx);
    let max_x = ax.max(bx).max(cx);
    let min_y = ay.min(by).min(cy);
    let max_y = ay.max(by).max(cy);
    for py in min_y..=max_y {
        for px in min_x..=max_x {
            let cov = rounded_triangle_coverage(
                px as f32, py as f32, ax as f32, ay as f32, bx as f32, by as f32, cx as f32, cy as f32, radius,
            );
            if cov > 0.0 {
                let alpha = (color[3] as f32 * cov) as u32;
                composite(
                    pixels,
                    width,
                    px as usize,
                    py as usize,
                    [color[0], color[1], color[2]],
                    alpha,
                );
            }
        }
    }
}

/// Signed distance from a point to the line through (a, b): positive on the
/// left side, which is the interior side for a counter-clockwise triangle.
fn edge_signed_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let ex = bx - ax;
    let ey = by - ay;
    let len = ex.hypot(ey);
    if len <= 0.0 {
        return f32::INFINITY;
    }
    (ex * (py - ay) - ey * (px - ax)) / len
}

/// Distance from a point to the closest point of a line segment.
fn point_segment_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let ex = bx - ax;
    let ey = by - ay;
    let len2 = ex * ex + ey * ey;
    let t = if len2 <= 0.0 {
        0.0
    } else {
        (((px - ax) * ex + (py - ay) * ey) / len2).clamp(0.0, 1.0)
    };
    let qx = ax + t * ex;
    let qy = ay + t * ey;
    (px - qx).hypot(py - qy)
}

/// The vertex of the triangle eroded by `radius` at corner (ax, ay): the
/// intersection of the two lines parallel to the adjacent edges, each inset
/// by the perpendicular `radius` toward the interior.
fn inset_vertex(ax: f32, ay: f32, bx: f32, by: f32, cx: f32, cy: f32, radius: f32) -> (f32, f32) {
    // Inward unit normals (left of the counter-clockwise edge direction).
    let (e1x, e1y) = (bx - ax, by - ay);
    let l1 = e1x.hypot(e1y);
    let (e2x, e2y) = (ax - cx, ay - cy);
    let l2 = e2x.hypot(e2y);
    if l1 <= 0.0 || l2 <= 0.0 {
        return (ax, ay);
    }
    let (n1x, n1y) = (-e1y / l1, e1x / l1);
    let (n2x, n2y) = (-e2y / l2, e2x / l2);
    let det = n1x * n2y - n1y * n2x;
    if det.abs() <= 1e-6 {
        return (ax, ay);
    }
    let vx = radius * (n2y - n1y) / det;
    let vy = radius * (n1x - n2x) / det;
    (ax + vx, ay + vy)
}

/// Anti-aliased coverage of a triangle with corners rounded to `radius`
/// (radius 0 = sharp triangle). The rounded triangle is the original eroded
/// by `radius` (each edge inset perpendicularly) dilated back by the same
/// radius: a pixel is covered when it is within `radius` of the eroded core,
/// which cuts the corners into arcs while keeping the flat edges on the
/// original edge lines.
#[allow(clippy::too_many_arguments)]
fn rounded_triangle_coverage(
    px: f32,
    py: f32,
    ax: f32,
    ay: f32,
    bx: f32,
    by: f32,
    cx: f32,
    cy: f32,
    radius: f32,
) -> f32 {
    let signed_dist = if radius <= 0.0 {
        // Sharp triangle: minimum signed distance to the three edges.
        edge_signed_dist(px, py, ax, ay, bx, by)
            .min(edge_signed_dist(px, py, bx, by, cx, cy))
            .min(edge_signed_dist(px, py, cx, cy, ax, ay))
    } else {
        let (ax2, ay2) = inset_vertex(ax, ay, bx, by, cx, cy, radius);
        let (bx2, by2) = inset_vertex(bx, by, cx, cy, ax, ay, radius);
        let (cx2, cy2) = inset_vertex(cx, cy, ax, ay, bx, by, radius);
        let inside_core = edge_signed_dist(px, py, ax2, ay2, bx2, by2) >= 0.0
            && edge_signed_dist(px, py, bx2, by2, cx2, cy2) >= 0.0
            && edge_signed_dist(px, py, cx2, cy2, ax2, ay2) >= 0.0;
        let dist = if inside_core {
            0.0
        } else {
            point_segment_dist(px, py, ax2, ay2, bx2, by2)
                .min(point_segment_dist(px, py, bx2, by2, cx2, cy2))
                .min(point_segment_dist(px, py, cx2, cy2, ax2, ay2))
        };
        radius - dist
    };
    let t = (signed_dist / 1.5 + 0.5).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Weight for blending the track palette's primary color into the pill
/// fill. Kept low so the fill stays neutral-legible and only picks up a
/// subtle hint of the cover's hue — not a solid color wash.
const FILL_TINT_WEIGHT: f32 = 0.16;

/// Blends `accent` into `base` at `weight`, keeping base's own alpha.
/// Used to give the pill fill a subtle per-track hue instead of a fixed
/// neutral fill.
fn tinted_fill(base: [u8; 4], accent: [u8; 4], weight: f32) -> [u8; 4] {
    let mix = |b: u8, a: u8| -> u8 { (b as f32 * (1.0 - weight) + a as f32 * weight).round() as u8 };
    [
        mix(base[0], accent[0]),
        mix(base[1], accent[1]),
        mix(base[2], accent[2]),
        base[3],
    ]
}

/// A softened version of the accent color: lifts each channel towards white
/// by 35%, producing a vibrant pastel rather than a muddy gray. Used for
/// the artist and app-name rows so they complement the full accent without
/// competing with it.
fn muted_accent(primary: [u8; 4]) -> [u8; 4] {
    let lift = |c: u8| -> u8 {
        let float = c as f32;
        (float + (255.0 - float) * 0.35).clamp(0.0, 255.0) as u8
    };
    [lift(primary[0]), lift(primary[1]), lift(primary[2]), 255]
}

/// Draws the shared pill text layout used by every notification: title,
/// artist, meta and source-app rows, fitted to the rows that are actually
/// present. When `playback` is `Some`, the title row reserves space on its
/// right for the play/pause/stop symbol; track-change pills pass `None` and
/// use the full width. Every row marquee-scrolls when it overflows.
#[allow(clippy::too_many_arguments)]
fn draw_pill_text_rows(
    state: &mut OverlayState,
    pixels: &mut [u8],
    width: i32,
    scale: f32,
    pill: &PillText,
    playback: Option<PlaybackState>,
) {
    let inset = state.aura_inset;
    let appearance = &state.config.appearance;
    // Accent color: the displayed artwork's primary palette color when
    // available (gives the pill per-track theming), falling back to the
    // configured accent.
    let accent = state.palette.map(|p| p.primary).unwrap_or(appearance.accent_color);
    let muted = state
        .palette
        .map(|p| muted_accent(p.primary))
        .unwrap_or([0x77, 0x77, 0x77, 0xFF]);
    let padding = (appearance.padding * scale) as i32;
    let art = (appearance.art_size as f32 * scale) as i32;
    let left = inset + padding + art + (12.0 * scale) as i32;
    let right = width - inset - padding;

    // Font-driven row heights: bands are sized from the actual fonts, so
    // rows can never overlap at any pill size (including mid-animation).
    // Rows pack at the top of the pill; the height is constant, so a missing
    // row leaves its band empty below the drawn rows.
    let fs_title = appearance.font_size_title * scale;
    let fs_artist = appearance.font_size_artist * scale;
    let fs_meta = fs_artist * 0.85;
    let fs_app = fs_artist * 0.85;
    let rows: [(f32, f32); 4] = [
        (fs_title * ROW_HEIGHT, fs_title),
        (fs_artist * ROW_HEIGHT, fs_artist),
        (fs_meta * ROW_HEIGHT, fs_meta),
        (fs_app * ROW_HEIGHT, fs_app),
    ];
    let text_color = appearance.text_color;
    let pad = appearance.padding;
    let (font_title, h_title) = state.fonts.font_for(rows[0].1 as i32, true);
    let (font_artist, h_artist) = state.fonts.font_for(rows[1].1 as i32, false);
    let (font_meta, h_meta) = state.fonts.font_for(rows[2].1 as i32, false);
    let (font_app, h_app) = state.fonts.font_for(rows[3].1 as i32, false);
    // Only rows that will actually be drawn take up vertical space: the rest
    // of the pill's constant height stays empty below the rows.
    let artist_active = !pill.artist.trim().is_empty();
    let active: [bool; 4] = [
        true,
        artist_active,
        !pill.meta.is_empty(),
        !pill.source_app.trim().is_empty(),
    ];
    let text_top = inset as f32 + pad * scale;
    let mut y = text_top;
    let mut next_band = |i: usize| -> RECT {
        let band_h = if active[i] { rows[i].0 } else { 0.0 };
        let r = RECT {
            left,
            top: y as i32,
            right,
            bottom: (y + band_h) as i32,
        };
        y += band_h;
        r
    };

    // The symbol box is ~1.5× the title font, capped at the title row's own
    // height so it never overflows the band. The width reserved on the right
    // of the title row follows the actual symbol size.
    let symbol_size = (fs_title * 1.5).min(fs_title * ROW_HEIGHT);
    let label_w = (symbol_size + 16.0 * scale) as i32;

    let title_rect = next_band(0);
    let title_narrow = if playback.is_some() {
        RECT {
            left: title_rect.left,
            top: title_rect.top,
            right: title_rect.right - label_w,
            bottom: title_rect.bottom,
        }
    } else {
        title_rect
    };
    draw_text_line_pixels(
        &mut state.text_scratch,
        &mut state.scratch_utf16,
        pixels,
        width as usize,
        &pill.title,
        &title_narrow,
        font_title,
        h_title,
        text_color,
        false,
        scale,
        Some(MarqueeCtx {
            scroll: &mut state.scroll[0],
            strip: &mut state.marquee_strips[0],
        }),
    );
    if let Some(playback) = playback {
        draw_symbol_pixels(
            pixels,
            width as usize,
            title_rect.right,
            title_rect.top,
            symbol_size,
            playback,
            accent,
        );
    }

    let artist_rect = next_band(1);
    if artist_active {
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            pixels,
            width as usize,
            &pill.artist,
            &artist_rect,
            font_artist,
            h_artist,
            muted_accent(accent),
            false,
            scale,
            Some(MarqueeCtx {
                scroll: &mut state.scroll[1],
                strip: &mut state.marquee_strips[1],
            }),
        );
    }

    if active[2] {
        let meta_rect = next_band(2);
        draw_meta_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            pixels,
            width,
            &meta_rect,
            &pill.meta,
            pill.meta_clock,
            font_meta,
            rows[2].1 as i32,
            h_meta,
            accent,
            accent,
            scale,
            Some(MarqueeCtx {
                scroll: &mut state.scroll[2],
                strip: &mut state.marquee_strips[2],
            }),
        );
    }
    if active[3] {
        let app_rect = next_band(3);
        draw_source_app_row(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            pixels,
            width as usize,
            &pill.source_app,
            pill.app_icon.as_ref(),
            &app_rect,
            font_app,
            h_app,
            muted,
            scale,
            Some(MarqueeCtx {
                scroll: &mut state.scroll[3],
                strip: &mut state.marquee_strips[3],
            }),
        );
    }
}

/// Builds the render pieces for a track, computing the meta line once.
fn pill_text_from_track(track: &TrackInfo) -> PillText {
    let (meta_clock, meta) = track.meta_line_for_overlay(true);
    PillText {
        title: track.title.clone(),
        artist: track.artist.clone(),
        source_app: track.source_app.clone(),
        app_icon: track.app_icon.clone(),
        meta_clock,
        meta,
    }
}

/// Draws the pill's text rows into the same premultiplied pixel buffer as the
/// shapes: glyph coverage from fontdue becomes alpha, so text alpha-composites
/// exactly like every other element (GDI text cannot do this on a layered
/// window — it never touches the alpha channel). The compact layout draws a
/// single title row via `draw_compact_pill` instead.
#[allow(clippy::too_many_arguments)]
fn draw_text_pixels(
    state: &mut OverlayState,
    pixels: &mut [u8],
    content: &MediaEvent,
    width: i32,
    height: i32,
    scale: f32,
    compact: bool,
) {
    if compact {
        draw_compact_pill(state, pixels, content, width, height, scale);
        return;
    }
    match content {
        MediaEvent::TrackChanged(track) => {
            // The pill pieces were resolved when the content changed (see
            // `resolve_pill_text`); take them out so drawing can borrow
            // `state` mutably, then put them back for the next frame. The
            // on-demand fallback keeps direct draw calls self-sufficient.
            let pill = state.pill_text.take().unwrap_or_else(|| pill_text_from_track(track));
            draw_pill_text_rows(state, pixels, width, scale, &pill, Some(PlaybackState::NowPlaying));
            state.pill_text = Some(pill);
        }
        MediaEvent::PlaybackStateChanged(playback, source_app) => {
            let pill = state.pill_text.take().or_else(|| {
                if source_app.is_empty() {
                    None
                } else {
                    state.track_cache.get(source_app).map(|(t, _)| pill_text_from_track(t))
                }
            });
            if let Some(pill) = pill {
                draw_pill_text_rows(state, pixels, width, scale, &pill, Some(*playback));
                state.pill_text = Some(pill);
            } else {
                // No cached track (the state change arrived before the first
                // TrackChanged): fall back to the source name with an
                // "Unknown" artist row.
                let appearance = &state.config.appearance;
                let inset = state.aura_inset;
                let padding = (appearance.padding * scale) as i32;
                let art = (appearance.art_size as f32 * scale) as i32;
                let left = inset + padding + art + (12.0 * scale) as i32;
                let right = width - inset - padding;
                let fs_title = appearance.font_size_title * scale;
                let fs_artist = appearance.font_size_artist * scale;
                let text_color = appearance.text_color;
                let accent_color = appearance.accent_color;
                let pad = appearance.padding;
                let (font_title, h_title) = state.fonts.font_for(fs_title as i32, true);
                let (font_artist, h_artist) = state.fonts.font_for((fs_artist * 0.85) as i32, false);
                let symbol_size = (fs_title * 1.5).min(fs_title * ROW_HEIGHT);
                let label_w = (symbol_size + 16.0 * scale) as i32;
                let mut y = inset as f32 + pad * scale;
                let mut next_band = |h: f32| -> RECT {
                    let r = RECT {
                        left,
                        top: y as i32,
                        right,
                        bottom: (y + h) as i32,
                    };
                    y += h;
                    r
                };

                let fallback_name = if !source_app.is_empty() {
                    Some(source_app.as_str())
                } else {
                    state.current_source.as_deref()
                };
                if let Some(name) = fallback_name {
                    let title_rect = next_band(fs_title * ROW_HEIGHT);
                    let title_narrow = RECT {
                        left: title_rect.left,
                        top: title_rect.top,
                        right: title_rect.right - label_w,
                        bottom: title_rect.bottom,
                    };
                    draw_text_line_pixels(
                        &mut state.text_scratch,
                        &mut state.scratch_utf16,
                        pixels,
                        width as usize,
                        name,
                        &title_narrow,
                        font_title,
                        h_title,
                        text_color,
                        false,
                        scale,
                        None,
                    );
                    draw_symbol_pixels(
                        pixels,
                        width as usize,
                        title_rect.right,
                        title_rect.top,
                        symbol_size,
                        *playback,
                        accent_color,
                    );
                    let artist_rect = next_band(fs_artist * 0.85 * ROW_HEIGHT);
                    draw_text_line_pixels(
                        &mut state.text_scratch,
                        &mut state.scratch_utf16,
                        pixels,
                        width as usize,
                        "Unknown",
                        &artist_rect,
                        font_artist,
                        h_artist,
                        [0xCC, 0xCC, 0xCC, 0xFF],
                        false,
                        scale,
                        None,
                    );
                }
            }
        }
        // Never rendered: SessionRejected is filtered out before enqueue.
        MediaEvent::SessionRejected { .. } | MediaEvent::WorkerFailed { .. } => {}
    }
}

/// Draws the compact pill's content: `[ART] [TITLE] [APP ICON] [▶]`. The art
/// tile is drawn here (not in `draw_pixels`, which sizes the expanded art),
/// the title occupies exactly `compact_title_viewport` — so marquee text and
/// its edge fade can never render under the app icon or the playback symbol
/// — and the trailing icon and symbol reuse the shared app-icon and
/// playback-symbol drawing. The take/put-back of the resolved pill text
/// mirrors `draw_pill_text_rows`.
#[allow(clippy::too_many_arguments)]
fn draw_compact_pill(
    state: &mut OverlayState,
    pixels: &mut [u8],
    content: &MediaEvent,
    width: i32,
    height: i32,
    scale: f32,
) {
    let inset = state.aura_inset;
    let appearance = &state.config.appearance;
    let accent = state.palette.map(|p| p.primary).unwrap_or(appearance.accent_color);
    let metrics = compact_metrics(&state.config);
    let padding = (appearance.padding * scale).round() as i32;
    let pill_h = height - inset * 2;
    let (title_vp_left, title_vp_right) = compact_title_viewport(&state.config);

    // Art tile: left-aligned like the expanded pill (inset + padding),
    // vertically centered on the row. This is the only place the compact
    // art is drawn — `draw_pixels` skips its art arms in compact mode, so
    // the halo, cover and rim composite exactly once. The placeholder is
    // drawn here too when no cover is available.
    let art_size = (metrics.art * scale).round() as i32;
    let art_x = inset + padding;
    let art_y = inset + (pill_h - art_size) / 2;
    draw_art_tile(
        pixels,
        width as usize,
        state.palette,
        appearance.accent_color,
        art_x as usize,
        art_y as usize,
        art_size as usize,
        art_size as f32 * 0.2,
        state.decoded_art.as_deref(),
        scale,
    );

    // Title row band: the title font's own row height, vertically centered
    // in the pill.
    let fs_title = appearance.font_size_title * scale;
    let row_h = (fs_title * ROW_HEIGHT).round() as i32;
    let band_top = inset + (pill_h - row_h) / 2;
    let (font_title, h_title) = state.fonts.font_for(fs_title as i32, true);
    let title_rect = RECT {
        left: inset + (title_vp_left * scale).round() as i32,
        top: band_top,
        right: inset + (title_vp_right * scale).round() as i32,
        bottom: band_top + row_h,
    };

    let (title, app_icon, playback) = match content {
        MediaEvent::TrackChanged(track) => {
            let pill = state.pill_text.take().unwrap_or_else(|| pill_text_from_track(track));
            let (title, app_icon) = (pill.title.clone(), pill.app_icon.clone());
            state.pill_text = Some(pill);
            (title, app_icon, PlaybackState::NowPlaying)
        }
        MediaEvent::PlaybackStateChanged(playback, source_app) => {
            let pill = state.pill_text.take().or_else(|| {
                if source_app.is_empty() {
                    None
                } else {
                    state.track_cache.get(source_app).map(|(t, _)| pill_text_from_track(t))
                }
            });
            match pill {
                Some(pill) => {
                    let (title, app_icon) = (pill.title.clone(), pill.app_icon.clone());
                    state.pill_text = Some(pill);
                    (title, app_icon, *playback)
                }
                // No cached track (the state change arrived before the first
                // TrackChanged): the source name stands in for the title, and
                // no app icon is available.
                None => {
                    let name = if !source_app.is_empty() {
                        source_app.clone()
                    } else {
                        state.current_source.clone().unwrap_or_default()
                    };
                    (name, None, *playback)
                }
            }
        }
        // Never rendered: SessionRejected is filtered out before enqueue.
        MediaEvent::SessionRejected { .. } | MediaEvent::WorkerFailed { .. } => return,
    };

    draw_text_line_pixels(
        &mut state.text_scratch,
        &mut state.scratch_utf16,
        pixels,
        width as usize,
        &title,
        &title_rect,
        font_title,
        h_title,
        appearance.text_color,
        false,
        scale,
        Some(MarqueeCtx {
            scroll: &mut state.scroll[0],
            strip: &mut state.marquee_strips[0],
        }),
    );

    // Trailing elements, from the title viewport's right edge outward:
    // 6 px gap, app icon, 16 px gap (the expanded symbol gap), playback
    // symbol. The chain mirrors `compact_title_viewport`, so the viewport
    // and the elements can never overlap.
    let icon_size = (metrics.icon * scale).round() as i32;
    let gap = (6.0 * scale).round() as i32;
    let symbol_gap = (16.0 * scale).round() as i32;
    let symbol = (metrics.symbol * scale).round() as i32;
    let viewport_right = inset + (title_vp_right * scale).round() as i32;
    let icon_x = viewport_right + gap;
    let icon_y = inset + (pill_h - icon_size) / 2;
    if let Some(icon) = app_icon {
        draw_icon_scaled(
            pixels,
            width as usize,
            &icon,
            24,
            icon_x as usize,
            icon_y as usize,
            icon_size as usize,
        );
    }
    let symbol_right = icon_x + icon_size + symbol_gap + symbol;
    let symbol_y = inset + (pill_h - symbol) / 2;
    draw_symbol_pixels(
        pixels,
        width as usize,
        symbol_right,
        symbol_y,
        symbol as f32,
        playback,
        accent,
    );
}

/// Draws the meta row of a track pill: when it carries a duration (`clock`),
/// a vector clock icon is pinned to the left edge of the band and the text
/// (`meta`, already stripped of the stopwatch glyph by the caller) is drawn
/// to its right; otherwise the line renders as plain text. The clock icon
/// uses `accent` (the palette primary) while the text keeps `color`. When
/// the line overflows and marquees, the icon stays anchored and the text
/// scrolls in its offset box.
#[allow(clippy::too_many_arguments)]
fn draw_meta_line_pixels(
    text_scratch: &mut Option<TextScratch>,
    scratch_utf16: &mut Vec<u16>,
    pixels: &mut [u8],
    width: i32,
    rect: &RECT,
    meta: &str,
    clock: bool,
    font: HFONT,
    font_height: i32,
    tm_height: i32,
    color: [u8; 4],
    accent: [u8; 4],
    scale: f32,
    marquee: Option<MarqueeCtx<'_>>,
) {
    if !clock {
        draw_text_line_pixels(
            text_scratch,
            scratch_utf16,
            pixels,
            width as usize,
            meta,
            rect,
            font,
            tm_height,
            color,
            false,
            scale,
            marquee,
        );
        return;
    }
    let icon_size = font_height as f32;
    let icon_h = icon_size.round() as i32;
    let gap = (4.0 * scale) as i32;
    let icon_top = rect.top + (rect.bottom - rect.top - icon_h) / 2;
    draw_clock_icon_pixels(pixels, width as usize, rect.left, icon_top, icon_size, accent);
    let text_rect = RECT {
        left: rect.left + icon_h + gap,
        ..*rect
    };
    draw_text_line_pixels(
        text_scratch,
        scratch_utf16,
        pixels,
        width as usize,
        meta,
        &text_rect,
        font,
        tm_height,
        color,
        false,
        scale,
        marquee,
    );
}

/// Draws one pill text line into the pixel buffer using Windows' own GDI text
/// engine (grayscale antialiasing, proper hinting). Text is rendered in white
/// into a scratch DIB; GDI writes alpha 0 for text into 32bpp DIBs, so each
/// glyph pixel's RGB (white × coverage) supplies the coverage, which is
/// combined with the requested color at composite time. Drawing the final
/// color instead would pre-dim the scratch, and reading that dimmed value as
/// coverage would render gray text at ~brightness² opacity.
#[allow(clippy::too_many_arguments)]
fn draw_text_line_pixels(
    text_scratch: &mut Option<TextScratch>,
    scratch_utf16: &mut Vec<u16>,
    pixels: &mut [u8],
    width: usize,
    value: &str,
    rect: &RECT,
    font: HFONT,
    font_height: i32,
    color: [u8; 4],
    centered: bool,
    scale: f32,
    marquee: Option<MarqueeCtx<'_>>,
) {
    if value.is_empty() || rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    let rw = rect.right - rect.left;
    let rh = rect.bottom - rect.top;
    let Ok((hdc, bits, sw, sh)) = text_scratch_for(text_scratch, rw, rh) else {
        return;
    };
    // The scratch DIB is reused across rows (it grows but never shrinks), so a
    // narrower row reuses a wider buffer. Zeroing only `rw * rh * 4` contiguous
    // bytes leaves stale pixels from a previous wider row in the scratch's full
    // stride (sw * 4 per row); they ghost through as stray colored dots. Clear
    // the entire scratch buffer so every pixel read during compositing is clean.
    unsafe {
        std::ptr::write_bytes(bits.cast::<u8>(), 0, (sw * sh * 4) as usize);
    }
    if font.0.is_null() {
        return;
    }
    scratch_utf16.clear();
    scratch_utf16.extend(value.encode_utf16());
    unsafe {
        let old_font = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT);
        // Draw in pure white so the scratch RGB channels hold exactly the glyph
        // coverage (gray antialiasing keeps R == G == B); the requested text
        // color is applied when compositing below.
        SetTextColor(hdc, COLORREF(0x00FFFFFF));
        // Row-local drawing: the scratch starts at the row's top-left, so the
        // clip rect is (0, 0, rw, rh) and the text y is centered like the
        // static path. `font_height` is the font's tmHeight, cached with the
        // font instead of re-read per row per frame.
        let y = ((rh - font_height) / 2).max(0);
        let mut flags = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX;
        if centered {
            flags |= DT_CENTER;
        }
        let mut local = RECT {
            left: 0,
            top: 0,
            right: rw,
            bottom: rh,
        };
        if let Some(ctx) = marquee {
            let mut measured = RECT::default();
            let _ = DrawTextW(
                hdc,
                &mut *scratch_utf16,
                &mut measured,
                DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT,
            );
            let text_w = measured.right - measured.left;
            // Whether this line overflows its visible band: while a
            // fully-shown pill has no overflowing line, the animation tick
            // skips repainting. The threshold is the draw rect itself (the
            // symbol- or icon-narrowed width) — text that is cut off by the
            // badge must scroll rather than sit truncated.
            let was_scrolling = ctx.scroll.scrolling;
            ctx.scroll.scrolling = text_w > rw;
            if ctx.scroll.scrolling && !was_scrolling {
                debug!("marquee overflow | text_w={text_w} | draw_w={rw} | title={value}");
            }
            let hold_elapsed = ctx.scroll.started_at.map(|t| t.elapsed()).unwrap_or_default();
            // Edge-fade width in the rendering coordinate space (the same
            // scale the row rects live in), 12 logical px per side.
            let fade_w = MARQUEE_FADE * scale;
            if text_w <= rw {
                // Text fits: render once statically (no scrolling needed).
                let _ = DrawTextW(hdc, &mut *scratch_utf16, &mut local, flags);
            } else {
                // Overflowing line, served from the cached strip.
                // Rasterization occurs on a marquee-strip cache miss (a
                // content, size, font, or color change); the cached strip is
                // reused during the initial hold and subsequent scrolling, so
                // GDI text rendering (ExtTextOutW) never runs at animation
                // cadence. The tick keeps the offset at 0 through the hold,
                // so compositing at that offset shows the complete,
                // unellipsized title stationary — the viewport clips and
                // fades the overflowing tail. When the hold elapses, the same
                // strip starts sliding. Returns early because the strip
                // composite below replaces the general glyph composite at the
                // end of this function.
                let total = text_w + MARQUEE_GAP as i32;
                build_marquee_strip(
                    ctx.strip,
                    text_scratch,
                    scratch_utf16,
                    value,
                    rw,
                    rh,
                    font,
                    font_height,
                    color,
                    centered,
                    y,
                    text_w,
                );
                if let Some(strip) = ctx.strip.as_ref() {
                    let off = (ctx.scroll.offset % total as f32) as i32;
                    // Edge fade relative to the visible band: during the hold
                    // only the trailing edge fades — nothing exits the left
                    // edge, and the text head sits at the band boundary where
                    // it must stay readable. Once the line scrolls, text
                    // exits the left edge and enters at the right, so both
                    // edges fade.
                    let (fade_left, fade_right) = if hold_elapsed < MARQUEE_HOLD {
                        (0.0, fade_w)
                    } else {
                        (fade_w, fade_w)
                    };
                    composite_marquee_strip(pixels, width, rect, strip, off, total, fade_left, fade_right);
                }
                return;
            }
        } else {
            let _ = DrawTextW(hdc, &mut *scratch_utf16, &mut local, flags);
        }
        SelectObject(hdc, old_font);
    }

    // CreateDIBSection's documented contract: GDI must finish any drawing
    // into the DIB before the application reads the bit values directly.
    unsafe {
        let _ = GdiFlush();
    }

    // Composite the glyph pixels. The scratch is white-on-black, so the RGB
    // channels are the glyph coverage; alpha is coverage scaled by the text
    // color's own alpha, and the color is premultiplied by alpha for
    // `composite_pm`. Drawing the final color via SetTextColor instead would
    // make GDI pre-dim the scratch, and reading that dimmed value as coverage
    // would render gray text at ~brightness² opacity. The edge mask never
    // applies here: only the marquee strip composite (above) fades, relative
    // to the visible band.
    composite_glyphs(
        pixels,
        width,
        rect.left,
        rect.top,
        bits,
        sw as usize,
        rw as usize,
        rh as usize,
        color,
    );
}

/// Rasterizes the scrolling line once at its natural width and caches it,
/// premultiplied with the row's color. A cache hit (same text, rect, font,
/// color) is a no-op; a miss re-runs the GDI text draw into the scratch —
/// which may grow from the row's width to the text's width — and premultiplies
/// the coverage into the strip. On any GDI failure the strip is dropped so a
/// stale raster can never be shown for different content.
#[allow(clippy::too_many_arguments)]
fn build_marquee_strip(
    strip: &mut Option<MarqueeStrip>,
    text_scratch: &mut Option<TextScratch>,
    scratch_utf16: &mut Vec<u16>,
    value: &str,
    rw: i32,
    rh: i32,
    font: HFONT,
    font_height: i32,
    color: [u8; 4],
    centered: bool,
    y: i32,
    text_w: i32,
) {
    let cache_hit = matches!(
        strip,
        Some(cached)
            if cached.value == value
                && cached.rw == rw
                && cached.rh == rh
                && cached.font.0 == font.0
                && cached.font_height == font_height
                && cached.color == color
                && cached.centered == centered
                && cached.text_w == text_w
    );
    if cache_hit {
        return;
    }
    let Ok((hdc, bits, sw, sh)) = text_scratch_for(text_scratch, text_w, rh) else {
        *strip = None;
        return;
    };
    // The scratch DIB may be wider than the visible band after this grow; the
    // full buffer must be clean because the strip build reads every pixel of
    // it below (stale pixels from a previous wider row would composite in).
    unsafe {
        std::ptr::write_bytes(bits.cast::<u8>(), 0, (sw * sh * 4) as usize);
    }
    if font.0.is_null() {
        *strip = None;
        return;
    }
    scratch_utf16.clear();
    scratch_utf16.extend(value.encode_utf16());
    unsafe {
        let old_font = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT);
        // Draw in pure white so the scratch RGB channels hold exactly the glyph
        // coverage; the requested text color is applied when premultiplying.
        SetTextColor(hdc, COLORREF(0x00FFFFFF));
        let clip = RECT {
            left: 0,
            top: 0,
            right: text_w,
            bottom: rh,
        };
        let _ = ExtTextOutW(
            hdc,
            0,
            y,
            ETO_CLIPPED,
            Some(&clip),
            PCWSTR(scratch_utf16.as_ptr()),
            scratch_utf16.len() as u32,
            None,
        );
        // CreateDIBSection's documented contract: GDI must finish any drawing
        // into the DIB before the application reads the bit values directly.
        let _ = GdiFlush();
        let _ = SelectObject(hdc, old_font);
    }
    let mut pixels = vec![0u8; text_w as usize * rh as usize * 4];
    // No edge mask: the strip keeps the full raster, and the fade is applied
    // relative to the visible band at composite time.
    composite_glyphs(
        &mut pixels,
        text_w as usize,
        0,
        0,
        bits,
        sw as usize,
        text_w as usize,
        rh as usize,
        color,
    );
    *strip = Some(MarqueeStrip {
        value: value.to_owned(),
        rw,
        rh,
        font,
        font_height,
        color,
        centered,
        text_w,
        pixels,
    });
}

/// Samples the visible window of the scrolling marquee from the cached strip
/// and composites it into the frame, replicating the old two-copy GDI draw:
/// copy 1 of the loop covers [x1, x1+text_w), copy 2 covers
/// [x1+total, x1+total+text_w) with x1 = -off. Pixels between the copies are
/// background and stay untouched. The strip holds premultiplied pixels, so
/// the composite is the same source-over math as `composite_pm`. `fade_left`
/// and `fade_right` are the horizontal edge-fade widths in pixels; 0 disables
/// that edge's mask (the pre-scroll hold fades only the trailing edge).
#[allow(clippy::too_many_arguments)]
fn composite_marquee_strip(
    pixels: &mut [u8],
    width: usize,
    rect: &RECT,
    strip: &MarqueeStrip,
    off: i32,
    total: i32,
    fade_left: f32,
    fade_right: f32,
) {
    let rw = (rect.right - rect.left) as usize;
    let rh = (rect.bottom - rect.top) as usize;
    let tw = strip.text_w as usize;
    let x1 = -off;
    let x1_end = x1 + strip.text_w;
    let x2 = x1 + total;
    let x2_end = x2 + strip.text_w;
    for dy in 0..rh {
        let src_row = &strip.pixels[dy * tw * 4..(dy + 1) * tw * 4];
        let dst_row = &mut pixels[((rect.top as usize + dy) * width + rect.left as usize) * 4..];
        for x in 0..rw as i32 {
            let sx = if x >= x1 && x < x1_end {
                x - x1
            } else if x >= x2 && x < x2_end {
                x - x2
            } else {
                continue;
            };
            let sp = sx as usize * 4;
            let alpha = src_row[sp + 3] as u32;
            if alpha == 0 {
                continue;
            }
            // The strip is premultiplied, so the fade must scale the
            // premultiplied RGB together with the alpha, or a fading glyph
            // would keep its color while its coverage falls. The mask is
            // relative to the visible row `[rect.left, rect.right)`.
            let fade = edge_fade_factor(
                (rect.left + x) as f32,
                rect.left as f32,
                rect.right as f32,
                fade_left,
                fade_right,
            );
            let alpha = ((alpha as f32) * fade).round() as u32;
            if alpha == 0 {
                continue;
            }
            let src_r = (src_row[sp] as f32 * fade).round() as u32;
            let src_g = (src_row[sp + 1] as f32 * fade).round() as u32;
            let src_b = (src_row[sp + 2] as f32 * fade).round() as u32;
            let inv = 255 - alpha;
            let dp = x as usize * 4;
            dst_row[dp] = (src_r + dst_row[dp] as u32 * inv / 255) as u8;
            dst_row[dp + 1] = (src_g + dst_row[dp + 1] as u32 * inv / 255) as u8;
            dst_row[dp + 2] = (src_b + dst_row[dp + 2] as u32 * inv / 255) as u8;
            dst_row[dp + 3] = (alpha + dst_row[dp + 3] as u32 * inv / 255) as u8;
        }
    }
}

/// Horizontal alpha mask for overflowing marquee text: full opacity across
/// the interior of the visible row, ramping linearly to zero across
/// `fade_left` pixels from the left boundary and `fade_right` pixels from
/// the right. `x`, `left` and `right` share one coordinate space (the visible
/// row rect, `[left, right)`). A non-positive edge width disables that
/// edge's ramp, so the pre-scroll hold can fade only its trailing edge while
/// the text head stays at full opacity. When the fade zones overlap, the
/// stronger ramp wins, so a pixel near both boundaries is attenuated once,
/// never twice. A degenerate rect disables the mask (factor stays 1.0).
fn edge_fade_factor(x: f32, left: f32, right: f32, fade_left: f32, fade_right: f32) -> f32 {
    if right <= left {
        return 1.0;
    }
    let left_t = if fade_left > 0.0 {
        ((x - left) / fade_left).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let right_t = if fade_right > 0.0 {
        ((right - x) / fade_right).clamp(0.0, 1.0)
    } else {
        1.0
    };
    left_t.min(right_t)
}

/// Premultiplies the glyph coverage in the scratch DIB (white-on-black, stride
/// `sw` pixels per row) into `dest` at (left, top) with `color`, skipping
/// fully transparent pixels. Shared by the per-frame text composite and the
/// marquee-strip build. The edge mask is never applied here: the strip keeps
/// the full raster so the fade can be re-evaluated relative to the visible
/// band at composite time.
#[allow(clippy::too_many_arguments)]
fn composite_glyphs(
    dest: &mut [u8],
    dest_width: usize,
    left: i32,
    top: i32,
    bits: *mut c_void,
    sw: usize,
    rw: usize,
    rh: usize,
    color: [u8; 4],
) {
    for y in 0..rh {
        for x in 0..rw {
            let p = unsafe { bits.cast::<u8>().add((y * sw + x) * 4) };
            let b = unsafe { *p as u32 };
            let g = unsafe { *p.add(1) as u32 };
            let r = unsafe { *p.add(2) as u32 };
            let cov = r.max(g).max(b);
            if cov == 0 {
                continue;
            }
            let alpha = cov * color[3] as u32 / 255;
            if alpha == 0 {
                continue;
            }
            composite_pm(
                dest,
                dest_width,
                (left + x as i32) as usize,
                (top + y as i32) as usize,
                [
                    (color[0] as u32 * alpha / 255) as u8,
                    (color[1] as u32 * alpha / 255) as u8,
                    (color[2] as u32 * alpha / 255) as u8,
                ],
                alpha,
            );
        }
    }
}

/// Returns the scratch DC + DIB for GDI text, growing it when a larger text
/// row arrives. The DIB is kept across frames and released at window
/// destruction.
fn text_scratch_for(
    scratch: &mut Option<TextScratch>,
    width: i32,
    height: i32,
) -> Result<(HDC, *mut c_void, i32, i32)> {
    if let Some(cached) = scratch {
        if cached.width >= width && cached.height >= height {
            return Ok((cached.hdc, cached.bits, cached.width, cached.height));
        }
        unsafe {
            let _ = SelectObject(cached.hdc, cached.old_bitmap);
            let _ = DeleteObject(cached.bitmap);
            let _ = DeleteDC(cached.hdc);
        }
        *scratch = None;
    }
    let width = width.max(1);
    let height = height.max(1);
    let (hdc, bitmap, bits) = create_dc_with_dib(width, height)?;
    let old_bitmap = unsafe { SelectObject(hdc, bitmap) };
    *scratch = Some(TextScratch {
        hdc,
        bitmap,
        old_bitmap,
        bits,
        width,
        height,
    });
    Ok((hdc, bits, width, height))
}

/// Source-over composite of a premultiplied source (rgb already multiplied by
/// alpha) onto the premultiplied pill buffer.
fn composite_pm(pixels: &mut [u8], width: usize, x: usize, y: usize, rgb: [u8; 3], alpha: u32) {
    if x >= width || y >= pixels.len() / width / 4 {
        return;
    }
    let offset = (y * width + x) * 4;
    let alpha = alpha.min(255);
    let inv = 255 - alpha;
    pixels[offset] = (rgb[2] as u32 + pixels[offset] as u32 * inv / 255) as u8;
    pixels[offset + 1] = (rgb[1] as u32 + pixels[offset + 1] as u32 * inv / 255) as u8;
    pixels[offset + 2] = (rgb[0] as u32 + pixels[offset + 2] as u32 * inv / 255) as u8;
    pixels[offset + 3] = (alpha + pixels[offset + 3] as u32 * inv / 255) as u8;
}

/// Converts the worker's premultiplied BGRA artwork (square, adaptive
/// per-DPI size) into the straight RGBA buffer the overlay composites and
/// palettizes from.
/// Runs once per cover change, keyed by the decoded pixels in `ensure_art`.
/// The result is always a perfect square; `draw_art_scaled` derives the side
/// from the buffer length.
fn pm_bgra_to_rgba(pm: &[u8]) -> Option<Vec<u8>> {
    let mut rgba = Vec::with_capacity(pm.len());
    for px in pm.chunks_exact(4) {
        let (b, g, r, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        if a == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        // Un-premultiply: straight channel = premultiplied × 255 / alpha.
        rgba.push((r * 255 / a) as u8);
        rgba.push((g * 255 / a) as u8);
        rgba.push((b * 255 / a) as u8);
        rgba.push(a as u8);
    }
    Some(rgba)
}

/// Source-over composite of a premultiplied source (rgb, alpha) onto the
/// buffer. The buffer holds premultiplied BGRA, exactly what
/// UpdateLayeredWindow(ULW_ALPHA) consumes, so every shape and glyph goes
/// through this single alpha-correct path.
fn composite(pixels: &mut [u8], width: usize, x: usize, y: usize, rgb: [u8; 3], alpha: u32) {
    if x >= width || y >= pixels.len() / width / 4 {
        return;
    }
    let offset = (y * width + x) * 4;
    let alpha = alpha.min(255);
    let inv = 255 - alpha;
    pixels[offset] = ((rgb[2] as u32 * alpha + pixels[offset] as u32 * inv) / 255) as u8;
    pixels[offset + 1] = ((rgb[1] as u32 * alpha + pixels[offset + 1] as u32 * inv) / 255) as u8;
    pixels[offset + 2] = ((rgb[0] as u32 * alpha + pixels[offset + 2] as u32 * inv) / 255) as u8;
    pixels[offset + 3] = (alpha + pixels[offset + 3] as u32 * inv / 255) as u8;
}

/// Bilinearly scales a premultiplied BGRA icon and composites it into the
/// pixel buffer at (x, y) in pixel-space. The source `icon` has `icon_size`
/// pixels per side; the destination renders at `dest_size` pixels per side.
fn draw_icon_scaled(
    pixels: &mut [u8],
    width: usize,
    icon: &[u8],
    icon_size: usize,
    x: usize,
    y: usize,
    dest_size: usize,
) {
    if dest_size == 0 || icon_size == 0 || icon.len() < icon_size * icon_size * 4 {
        return;
    }
    let src_stride = icon_size * 4;
    for dy in 0..dest_size {
        for dx in 0..dest_size {
            let sx = (dx as f32 + 0.5) * icon_size as f32 / dest_size as f32 - 0.5;
            let sy = (dy as f32 + 0.5) * icon_size as f32 / dest_size as f32 - 0.5;
            let x0 = sx.max(0.0) as usize;
            let y0 = sy.max(0.0) as usize;
            let x1 = (x0 + 1).min(icon_size - 1);
            let y1 = (y0 + 1).min(icon_size - 1);
            let fx = (sx - x0 as f32).clamp(0.0, 1.0);
            let fy = (sy - y0 as f32).clamp(0.0, 1.0);
            let p00 = y0 * src_stride + x0 * 4;
            let p10 = y0 * src_stride + x1 * 4;
            let p01 = y1 * src_stride + x0 * 4;
            let p11 = y1 * src_stride + x1 * 4;
            let b = lerp(lerp(icon[p00], icon[p10], fx), lerp(icon[p01], icon[p11], fx), fy);
            let g = lerp(
                lerp(icon[p00 + 1], icon[p10 + 1], fx),
                lerp(icon[p01 + 1], icon[p11 + 1], fx),
                fy,
            );
            let r = lerp(
                lerp(icon[p00 + 2], icon[p10 + 2], fx),
                lerp(icon[p01 + 2], icon[p11 + 2], fx),
                fy,
            );
            let a = lerp(
                lerp(icon[p00 + 3], icon[p10 + 3], fx),
                lerp(icon[p01 + 3], icon[p11 + 3], fx),
                fy,
            );
            if a > 0 {
                composite_pm(pixels, width, x + dx, y + dy, [r, g, b], a as u32);
            }
        }
    }
}

/// Draws the source-app row: the app icon (when the track carries one) at
/// 16px base, DPI-scaled and capped at the row band, followed by the app-name
/// text. The text glyphs sit centered in the band, so the icon is centered on
/// the same midpoint to line up with them. Without an icon the text renders
/// at the band's left edge, as before the icon was added.
#[allow(clippy::too_many_arguments)]
fn draw_source_app_row(
    text_scratch: &mut Option<TextScratch>,
    scratch_utf16: &mut Vec<u16>,
    pixels: &mut [u8],
    width: usize,
    source_app: &str,
    app_icon: Option<&Arc<[u8]>>,
    rect: &RECT,
    font: HFONT,
    tm_height: i32,
    color: [u8; 4],
    scale: f32,
    marquee: Option<MarqueeCtx<'_>>,
) {
    if let Some(icon) = app_icon {
        // The source bitmap is always 24x24; the destination size is the
        // 16px base scaled for DPI, clamped so it never overflows the band.
        let band_h = (rect.bottom - rect.top) as usize;
        let icon_size = ((16.0 * scale).round() as usize).min(band_h);
        let icon_x = rect.left as usize;
        let icon_y = rect.top as usize + (band_h - icon_size) / 2;
        draw_icon_scaled(pixels, width, icon, 24, icon_x, icon_y, icon_size);
        let text_rect = RECT {
            left: rect.left + icon_size as i32 + 6,
            ..*rect
        };
        draw_text_line_pixels(
            text_scratch,
            scratch_utf16,
            pixels,
            width,
            source_app,
            &text_rect,
            font,
            tm_height,
            color,
            false,
            scale,
            marquee,
        );
    } else {
        draw_text_line_pixels(
            text_scratch,
            scratch_utf16,
            pixels,
            width,
            source_app,
            rect,
            font,
            tm_height,
            color,
            false,
            scale,
            marquee,
        );
    }
}
/// Signed distance to a rounded rectangle's boundary at pixel (x, y),
/// negative inside the shape. Used for the pill's outer shape, the
/// placeholder art and the album-artwork corner mask.
fn round_rect_signed_dist(x: f32, y: f32, width: f32, height: f32, radius: f32) -> f32 {
    let radius = radius.min(width / 2.0).min(height / 2.0);
    let qx = ((x + 0.5) - width / 2.0).abs() - (width / 2.0 - radius);
    let qy = ((y + 0.5) - height / 2.0).abs() - (height / 2.0 - radius);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius
}

/// Anti-aliased coverage (0..=1) of a rounded rectangle at pixel (x, y):
/// signed distance to the boundary smoothed over a 1.5 px band via
/// Hermite interpolation. Used for the pill's outer shape, the placeholder
/// art and the album-artwork corner mask.
fn round_rect_coverage(x: f32, y: f32, width: f32, height: f32, radius: f32) -> f32 {
    let dist = round_rect_signed_dist(x, y, width, height, radius);
    let t = ((0.75 - dist) / 1.5).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Returns the exact supersampled coverage for (x, y) when it can be proven
/// without sampling, `None` otherwise. Interior: a pixel whose center is at
/// least `max(radius, 0.75) + 0.35` from every edge is fully covered — every
/// supersample sits in the straight-edge band of the SDF (clearing the corner
/// squares by 0.35) and at least 0.75px inside it, so all four samples read
/// coverage exactly 1.0. Exterior: a pixel whose center is at least 1.1px
/// beyond any bounding-box edge has every supersample at least 0.75px outside
/// the shape (the box contains the shape, and the corner arcs only pull the
/// boundary inward), so all four samples read exactly 0.0. The bounds are
/// deliberately conservative: a wrong guess here would be a visible hard edge
/// or a thin unlit ring.
fn round_rect_coverage_fast(x: f32, y: f32, width: f32, height: f32, radius: f32) -> Option<f32> {
    let radius = radius.min(width / 2.0).min(height / 2.0);
    let cx = x + 0.5;
    let cy = y + 0.5;
    let inset = radius.max(0.75) + 0.35;
    if cx >= inset && cx <= width - inset && cy >= inset && cy <= height - inset {
        return Some(1.0);
    }
    if cx <= -1.1 || cx >= width + 1.1 || cy <= -1.1 || cy >= height + 1.1 {
        return Some(0.0);
    }
    None
}

/// 2×2 subpixel supersampled coverage of a rounded rectangle. Replaces the
/// single-sample `round_rect_coverage` for the pill body to smooth the curved
/// corners and straight edges, reducing stair-stepping on the anti-aliased
/// boundary. `round_rect_coverage` treats its argument as a pixel corner (it
/// adds 0.5 internally for the pixel centre), so offsets of ±0.35 land on the
/// four sub-pixel sample points at 0.15 and 0.85 within the pixel — wide
/// enough to fully span the 1.5 px anti-alias band for the black pill edge.
/// Pixels provably inside or outside the shape short-circuit through
/// `round_rect_coverage_fast`, which returns bit-identical results to the
/// full four-sample evaluation.
fn round_rect_coverage_supersampled(x: f32, y: f32, width: f32, height: f32, radius: f32) -> f32 {
    if let Some(coverage) = round_rect_coverage_fast(x, y, width, height, radius) {
        return coverage;
    }
    let cov = |dx: f32, dy: f32| round_rect_coverage(x + dx, y + dy, width, height, radius);
    (cov(-0.35, -0.35) + cov(0.35, -0.35) + cov(-0.35, 0.35) + cov(0.35, 0.35)) * 0.25
}

/// Soft multi-color glow around the pill's boundary. The DIB is inflated by
/// `AURA_HALO_LOGICAL` (scaled by DPI × shape) on every side so the halo can
/// extend outside the pill into the desktop background.
const AURA_MARGIN_LOGICAL: f32 = 10.0;
/// Outer extent of the synthetic aura glow, in logical px per side. The
/// falloff curve (see `AURA_DECAY`) is normalized by `AURA_MARGIN_LOGICAL`,
/// so the glow's shape is independent of where it ends: shrinking the halo
/// truncates the faint outer tail instead of re-shaping the visible part.
const AURA_HALO_LOGICAL: f32 = 6.0;
/// Peak opacity of the outer aura ring, at the pill boundary. Capped at ~140
/// so the glow stays soft beneath the pill body's supersampled edge instead
/// of producing a hard 0→255 step at the boundary.
const AURA_PEAK_ALPHA: f32 = 140.0;
/// Exponential decay constant. The falloff is exp(-AURA_DECAY * d /
/// (AURA_MARGIN_LOGICAL * scale)) per physical px, so the curve's per-px
/// rate is fixed by these two constants and does not change when the halo
/// extent shrinks.
const AURA_DECAY: f32 = 3.0;

#[allow(clippy::too_many_arguments)]
fn draw_aura(
    pixels: &mut [u8],
    buf_w: usize,
    buf_h: usize,
    palette: Palette,
    inset: usize,
    pill_w: usize,
    pill_h: usize,
    radius: f32,
    scale: f32,
) {
    let c1 = palette.primary;
    let c2 = palette.secondary;
    let margin = (AURA_HALO_LOGICAL * scale).round().max(1.0) as usize;

    for y in 0..buf_h {
        for x in 0..buf_w {
            // Pixels farther than the margin from the pill's bounding box are
            // certainly farther than the margin from the rounded pill itself
            // (the box contains the pill), so they can never contribute —
            // skip before evaluating the signed distance.
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let box_left = inset as f32;
            let box_right = box_left + pill_w as f32;
            let box_top = inset as f32;
            let box_bottom = box_top + pill_h as f32;
            let margin_f = margin as f32;
            if px < box_left - margin_f
                || px > box_right + margin_f
                || py < box_top - margin_f
                || py > box_bottom + margin_f
            {
                continue;
            }
            let d = round_rect_signed_dist(
                (x as f32) - inset as f32,
                (y as f32) - inset as f32,
                pill_w as f32,
                pill_h as f32,
                radius,
            );

            // Smooth inner anti-aliased transition at the pill boundary,
            // replacing the hard `d <= 0` cutoff that produced an abrupt
            // 0→peak alpha jump. `inner_aa` ramps from 0 (deep inside the pill)
            // to 1 (at the boundary) over a ~1.5 px band, so the supersampled
            // pill edge blends smoothly with the glow beneath it instead of
            // hard-clipping the aura ring.
            let inner_aa = if d < 0.0 {
                let t = ((d + 1.5) / 1.5).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t)
            } else {
                1.0
            };

            if inner_aa <= 0.0 || d > margin as f32 {
                continue;
            }

            // Horizontal color transition from primary (left) to secondary (right)
            let t = ((x as f32 - inset as f32) / pill_w as f32).clamp(0.0, 1.0);
            let rgb = [
                (c1[0] as f32 * (1.0 - t) + c2[0] as f32 * t).round() as u8,
                (c1[1] as f32 * (1.0 - t) + c2[1] as f32 * t).round() as u8,
                (c1[2] as f32 * (1.0 - t) + c2[2] as f32 * t).round() as u8,
            ];

            // Exponential outer decay at a fixed per-logical-px rate (DPI and
            // the expand/collapse shape are folded into `scale`). The margin
            // guard above truncates the halo at its extent; the last px
            // ramps linearly to 0 so the glow ends smoothly mid-curve
            // instead of hitting a hard edge.
            let falloff = (-d * AURA_DECAY / AURA_MARGIN_LOGICAL / scale).exp();
            let edge = (margin as f32 - d).clamp(0.0, 1.0);
            let alpha = (AURA_PEAK_ALPHA * inner_aa * falloff * edge)
                .round()
                .min(AURA_PEAK_ALPHA) as u32;

            if alpha > 0 {
                composite(pixels, buf_w, x, y, rgb, alpha);
            }
        }
    }
}

/// Anti-aliased coverage of a circle of the given pixel size, sampled at the
/// pixel at (x, y) relative to the circle's top-left corner.
fn circle_coverage(x: f32, y: f32, size: f32) -> f32 {
    let radius = size / 2.0;
    let dist = (x + 0.5 - radius).hypot(y + 0.5 - radius) - radius;
    let t = (0.5 - dist / 1.5).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Anti-aliased coverage of a clock icon inside a `size`×`size` box at the
/// pixel at (x, y) relative to the box's top-left corner: a thin ring with an
/// hour hand pointing at 12 and a minute hand pointing at 3, both meeting at
/// the center. Stroked like the pill's other shapes, so at small sizes it
/// stays crisp instead of mushing like the ⏱ emoji routed through GDI text.
fn clock_icon_coverage(x: f32, y: f32, size: f32) -> f32 {
    let center = size / 2.0;
    let px = x + 0.5 - center;
    let py = y + 0.5 - center;
    let dist = px.hypot(py);

    // Ring: signed distance from the ring's centerline, negative inside the
    // stroke band. The hole inside the ring stays uncovered, so the pill's
    // background shows through like a real clock face.
    let ring_r = size * 0.36;
    let band = size * 0.055;
    let d_ring = (dist - ring_r).abs() - band;
    let t_ring = (0.5 - d_ring / 1.5).clamp(0.0, 1.0);
    let ring = t_ring * t_ring * (3.0 - 2.0 * t_ring);

    // Hands: thin stroked segments from the center outward, anti-aliased via
    // distance to the segment.
    let hand_w = size * 0.05;
    let hour = point_segment_dist(px, py, 0.0, 0.0, 0.0, -ring_r * 0.55);
    let minute = point_segment_dist(px, py, 0.0, 0.0, ring_r * 0.78, 0.0);
    let d_hand = hour.min(minute) - hand_w;
    let t_hand = (0.5 - d_hand / 1.5).clamp(0.0, 1.0);
    let hands = t_hand * t_hand * (3.0 - 2.0 * t_hand);

    ring.max(hands)
}

/// Draws a vector clock icon into the premultiplied pixel buffer, sized to
/// `size` pixels at (`x`, `y`) with its top-left corner at that point.
/// Procedural like the play/pause/stop symbols, so it renders identically on
/// every Windows version with no font fallback involved.
fn draw_clock_icon_pixels(pixels: &mut [u8], width: usize, x: i32, y: i32, size: f32, color: [u8; 4]) {
    if size <= 0.0 {
        return;
    }
    for dy in 0..size as i32 {
        for dx in 0..size as i32 {
            let cov = clock_icon_coverage(dx as f32, dy as f32, size);
            if cov > 0.0 {
                let px = x + dx;
                let py = y + dy;
                if px >= 0 && py >= 0 {
                    let alpha = (color[3] as f32 * cov) as u32;
                    composite(
                        pixels,
                        width,
                        px as usize,
                        py as usize,
                        [color[0], color[1], color[2]],
                        alpha,
                    );
                }
            }
        }
    }
}

fn draw_placeholder(pixels: &mut [u8], width: usize, x: usize, y: usize, size: usize, color: [u8; 4]) {
    for py in y..y.saturating_add(size) {
        for px in x..x.saturating_add(size) {
            let coverage = circle_coverage((px - x) as f32, (py - y) as f32, size as f32);
            if coverage > 0.0 {
                let alpha = (color[3] as f32 * coverage) as u32;
                composite(pixels, width, px, py, [color[0], color[1], color[2]], alpha);
            }
        }
    }
}

fn register_window_class(instance: HINSTANCE, class_name: &[u16]) -> Result<()> {
    Ok(crate::winutil::register_class_once(
        &REGISTERED,
        instance,
        class_name,
        Some(window_proc),
        || None,
        "the overlay window",
    )?)
}

static REGISTERED: OnceLock<()> = OnceLock::new();

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            let state = (*create).lpCreateParams as *mut OverlayState;
            if !state.is_null() {
                set_window_state(hwnd, state);
                (*state).hwnd = hwnd;
                OVERLAY_STATE_CLAIMED.store(true, Ordering::SeqCst);
            }
        }
    }

    let state_ptr = window_state::<OverlayState>(hwnd);
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            let _ = ValidateRect(hwnd, None);
            LRESULT(0)
        }
        MEDIA_EVENT_MSG => {
            if !state_ptr.is_null() {
                (*state_ptr).receive_events();
            }
            LRESULT(0)
        }
        TOGGLE_MSG => {
            if !state_ptr.is_null() {
                (*state_ptr).toggle_enabled();
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_DEBOUNCE => {
            if !state_ptr.is_null() {
                (*state_ptr).flush_pending();
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == ANIM_TIMER_ID => {
            if !state_ptr.is_null() {
                (*state_ptr).tick();
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == IDLE_BUFFER_TIMER_ID => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                // Every show path kills this timer, so firing with a visible
                // pill would be a logic error elsewhere; the check keeps the
                // release from ever racing a render.
                if state.content.is_none() {
                    state.release_idle_buffers();
                }
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            // A display was added, removed, or reordered (or its resolution
            // changed). The per-frame target resolution picks the new layout
            // up on its own; here, a visible pill is moved onto the re-resolved
            // target immediately instead of waiting for the next tick, and the
            // refresh-rate cache is dropped so the animation timer re-samples
            // the target's rate.
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                state.period_cache = None;
                if !matches!(state.phase, Phase::Hidden) {
                    state.reposition();
                }
            }
            LRESULT(0)
        }
        TIMER_ANIMATION_MSG => {
            // The timer-queue callback posts one tick message per period;
            // when the UI thread stalls, those accumulate. Drain the queue so
            // one dispatch consumes every queued tick (the animation is
            // time-based, so a dropped tick changes nothing and the backlog
            // cannot pile up).
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, hwnd, TIMER_ANIMATION_MSG, TIMER_ANIMATION_MSG, PM_REMOVE).as_bool() {}
            }
            if !state_ptr.is_null() {
                (*state_ptr).tick();
            }
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        WM_NCDESTROY => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                state.delete_anim_timer();
                if let Some(scratch) = state.text_scratch.take() {
                    unsafe {
                        let _ = SelectObject(scratch.hdc, scratch.old_bitmap);
                        let _ = DeleteObject(scratch.bitmap);
                        let _ = DeleteDC(scratch.hdc);
                    }
                }
                if let Some(dib) = state.dib.take() {
                    unsafe {
                        let _ = SelectObject(dib.hdc, dib.old_bitmap);
                        let _ = DeleteObject(dib.bitmap);
                        let _ = DeleteDC(dib.hdc);
                    }
                }
                drop(Box::from_raw(state_ptr));
            }
            clear_window_state(hwnd);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn animation_duration(config: &Config) -> Duration {
    Duration::from_millis(config.overlay.animation_ms.clamp(100, 500))
}

/// Quintic ease-out: a fast start with a long, soft settle. Used for opacity
/// (and collapse), where a punchy fade-in reads better than a slow cubic ramp.
fn ease_out_quint(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    1.0 - (1.0 - value).powi(5)
}

/// Cubic ease-out-back with a subtle spring overshoot (~8% past 1.0), the
/// standard "physical snap" curve for expanding UI elements. The overshoot is
/// clamped modest so the pill never visibly exceeds its final size.
fn ease_out_back(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    let c1 = 1.40;
    let c3 = c1 + 1.0;
    1.0 + c3 * (value - 1.0).powi(3) + c1 * (value - 1.0).powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ARTWORK_DECODE;
    use windows::Win32::Graphics::Gdi::{
        ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE,
        OUT_DEFAULT_PRECIS,
    };

    #[test]
    fn frame_scratch_is_cleared_when_it_grows() {
        let mut scratch = vec![0xA5; 4];

        clear_frame_scratch(&mut scratch, 8);

        assert_eq!(scratch, vec![0; 8]);
    }

    #[test]
    fn frame_scratch_is_shrunk_back_only_when_it_drops_to_half_capacity() {
        // Models production: `clear_frame_scratch` keeps len == needed while
        // an oversized frame has inflated the capacity far beyond it.
        let mut scratch = Vec::with_capacity(512 * 1024);
        scratch.resize(128 * 1024, 0);

        shrink_frame_scratch(&mut scratch, 128 * 1024);

        assert_eq!(scratch.capacity(), 128 * 1024);

        // A mild reduction keeps the capacity (hysteresis), so the normal
        // expand/collapse animation never reallocates.
        let mut mild = Vec::with_capacity(512 * 1024);
        mild.resize(300 * 1024, 0);
        shrink_frame_scratch(&mut mild, 300 * 1024);
        assert_eq!(mild.capacity(), 512 * 1024);
    }

    /// This is the test that would have caught the original stride bug: it
    /// constructs a destination buffer *larger* than the packed source (an
    /// oversized DIB, standing in for the reused animation-frame backing
    /// buffer) and asserts every row lands at its real stride, not the
    /// source's packed width.
    #[test]
    fn blit_packed_rows_respects_the_larger_destination_stride() {
        let row_bytes = 3 * 4; // 3 pixels wide, BGRA
        let rows = 4;
        let dst_stride_bytes = 10 * 4; // destination is much wider (10px) per row

        // Distinct byte pattern per row so a stride mismatch is unmistakable.
        let mut src = vec![0u8; row_bytes * rows];
        for row in 0..rows {
            for b in 0..row_bytes {
                src[row * row_bytes + b] = (row * 10 + b) as u8;
            }
        }

        let mut dst = vec![0xAAu8; dst_stride_bytes * rows]; // 0xAA marks untouched bytes
        blit_packed_rows(&mut dst, dst_stride_bytes, &src, row_bytes, rows);

        for row in 0..rows {
            let dst_row = &dst[row * dst_stride_bytes..row * dst_stride_bytes + dst_stride_bytes];
            let src_row = &src[row * row_bytes..row * row_bytes + row_bytes];
            // The row's own data lands at the start of its (wider) destination row.
            assert_eq!(
                &dst_row[..row_bytes],
                src_row,
                "row {row} landed at the wrong offset — this is the stride bug"
            );
            // The padding past the packed row width is untouched, proving the
            // copy did not run past the packed row into the next one (which
            // would happen if the packed stride were used instead of the
            // real one).
            assert!(
                dst_row[row_bytes..].iter().all(|&b| b == 0xAA),
                "row {row} overwrote destination padding past its packed width"
            );
        }
    }

    #[test]
    fn blit_packed_rows_is_a_no_op_for_zero_rows_or_width() {
        let mut dst = vec![0xAAu8; 40];
        blit_packed_rows(&mut dst, 10, &[], 4, 0);
        assert!(dst.iter().all(|&b| b == 0xAA));
        let mut dst2 = vec![0xAAu8; 40];
        blit_packed_rows(&mut dst2, 10, &[1, 2, 3], 0, 3);
        assert!(dst2.iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn blit_packed_rows_is_identity_when_strides_match() {
        // When dst_stride_bytes == row_bytes (the pre-oversized-DIB case,
        // i.e. the pill at its fully expanded size), the blit degenerates to
        // a straight contiguous copy.
        let src: Vec<u8> = (0..24u8).collect();
        let mut dst = vec![0u8; 24];
        blit_packed_rows(&mut dst, 6, &src, 6, 4);
        assert_eq!(dst, src);
    }

    /// A solid-color artwork buffer in the worker's format: premultiplied
    /// BGRA at `ARTWORK_DECODE`² (the cap the worker's adaptive decode
    /// targets; the overlay only reads the side from the buffer length).
    fn pm_art(color: [u8; 3]) -> Arc<[u8]> {
        let (b, g, r) = (color[2], color[1], color[0]);
        let px = [b, g, r, 255];
        Arc::from(px.repeat(ARTWORK_DECODE as usize * ARTWORK_DECODE as usize))
    }

    #[test]
    fn artwork_cache_is_keyed_by_decoded_pixels() {
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let red = pm_art([200, 40, 40]);
        let blue = pm_art([40, 40, 200]);

        state.ensure_art(Some(&red));
        assert_eq!(
            state.decoded_art.as_ref().map(Vec::len),
            Some(ARTWORK_DECODE as usize * ARTWORK_DECODE as usize * 4)
        );
        let first = state.decoded_art.clone();

        // Same pixels (equal bytes, different Arc): served from the cache,
        // not converted again.
        state.ensure_art(Some(&red));
        assert_eq!(state.decoded_art, first);

        // New pixels: converted again.
        state.ensure_art(Some(&blue));
        assert_ne!(state.decoded_art, first);

        // A simulated eviction (empty buffer) with an unchanged key is a
        // no-op: a failed decode must not be re-attempted on every frame.
        state.decoded_art = None;
        state.ensure_art(Some(&blue));
        assert!(state.decoded_art.is_none(), "same key must not re-convert");

        // A missing decode (corrupt art) is cached as a failure: the same
        // `None` key again is a no-op and never re-attempts conversion.
        state.ensure_art(None);
        state.decoded_art = Some(vec![0u8; 4]);
        state.ensure_art(None);
        assert_eq!(
            state.decoded_art.as_deref(),
            Some(&[0u8, 0, 0, 0][..]),
            "None key must be a no-op"
        );

        // A new cover after a recorded failure still converts.
        state.ensure_art(Some(&blue));
        assert_ne!(state.decoded_art.as_ref().map(Vec::len), Some(4));
    }

    #[test]
    fn draw_icon_scaled_rejects_a_short_icon_buffer() {
        let mut pixels = vec![0u8; 40 * 40 * 4];
        // An icon shorter than icon_size^2 * 4 must be a no-op, not an
        // out-of-bounds read.
        draw_icon_scaled(&mut pixels, 40, &[0u8; 10], 24, 0, 0, 24);
        assert!(pixels.iter().all(|&b| b == 0));
    }

    fn track_for(source: &str, title: &str, artist: &str) -> TrackInfo {
        TrackInfo {
            title: title.into(),
            artist: artist.into(),
            source_app: source.into(),
            ..TrackInfo::default()
        }
    }

    #[test]
    fn track_cache_evicts_the_oldest_source_at_the_cap() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        for i in 0..(TRACK_CACHE_CAP + 2) {
            state.cache_track(&track_for(&format!("source-{i}"), "Song", "Artist"));
        }
        assert_eq!(state.track_cache.len(), TRACK_CACHE_CAP);
        assert!(
            !state.track_cache.contains_key("source-0"),
            "the two oldest sources must be evicted"
        );
        assert!(
            state
                .track_cache
                .contains_key(&format!("source-{}", TRACK_CACHE_CAP + 1)),
            "the newest source stays"
        );
        // Re-caching an existing source refreshes its recency: it survives
        // the evictions that follow.
        state.cache_track(&track_for("source-1", "Song", "Artist"));
        state.cache_track(&track_for("source-new", "Song", "Artist"));
        assert!(
            state.track_cache.contains_key("source-1"),
            "a re-cached source must not be evicted"
        );
        assert_eq!(state.track_cache.len(), TRACK_CACHE_CAP);
    }

    #[test]
    fn track_cache_expires_sources_idle_beyond_the_ttl() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.track_cache.insert(
            "stale".into(),
            (
                track_for("stale", "Old", "Song"),
                Instant::now() - TRACK_CACHE_TTL - Duration::from_secs(1),
            ),
        );
        state.track_cache_order.push_back("stale".into());
        // Lazy eviction: nothing sweeps between inserts, so an expired entry
        // stays readable for a state pill that arrives before the next one.
        assert!(
            state.track_cache.contains_key("stale"),
            "expired entries are readable until the next insert"
        );
        state.cache_track(&track_for("fresh", "New", "Song"));
        assert!(
            !state.track_cache.contains_key("stale"),
            "an entry idle past the TTL must be evicted at the next insert"
        );
        assert!(state.track_cache.contains_key("fresh"));
        assert_eq!(state.track_cache.len(), 1);
    }

    #[test]
    fn state_pill_suppressed_when_track_change_follows_in_the_same_batch() {
        let config = Config::default();
        let queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
        let mut state = OverlayState::new(config, queue.clone());
        // Prime the source's track cache with the previous song: without the
        // suppression the state pill would render it stale.
        state.track_cache.insert(
            "youtube-music".into(),
            (track_for("youtube-music", "Old Song", "Old Artist"), Instant::now()),
        );

        queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Playing,
                "youtube-music".into(),
            )));
        queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "youtube-music",
                "New Song",
                "New Artist",
            ))));
        state.receive_events();

        assert_eq!(state.pending.len(), 1, "only the track pill should be queued");
        assert!(
            matches!(state.pending.front(), Some(MediaEvent::TrackChanged(t)) if t.title == "New Song"),
            "the queued event must be the new track"
        );
    }

    #[test]
    fn state_pill_still_shown_without_a_track_change_in_the_batch() {
        let config = Config::default();
        let queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
        let mut state = OverlayState::new(config, queue.clone());
        state.track_cache.insert(
            "youtube-music".into(),
            (track_for("youtube-music", "Song", "Artist"), Instant::now()),
        );

        queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Paused,
                "youtube-music".into(),
            )));
        state.receive_events();

        assert_eq!(state.pending.len(), 1, "a state change alone still queues a pill");
        assert!(matches!(
            state.pending.front(),
            Some(MediaEvent::PlaybackStateChanged(PlaybackState::Paused, source)) if source == "youtube-music"
        ));
    }

    #[test]
    fn same_source_state_toggle_updates_the_shown_state_pill_in_place() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.content = Some(MediaEvent::PlaybackStateChanged(
            PlaybackState::Paused,
            "youtube-music".into(),
        ));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() + Duration::from_millis(1500));

        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Playing,
                "youtube-music".into(),
            )));
        state.receive_events();

        assert!(
            state.pending.is_empty(),
            "a same-source toggle must not queue a second pill"
        );
        assert!(matches!(
            state.content.as_ref(),
            Some(MediaEvent::PlaybackStateChanged(PlaybackState::Playing, source))
                if source == "youtube-music"
        ));
        let remaining = state.dismiss_at.unwrap().saturating_duration_since(Instant::now());
        assert!(
            remaining >= Duration::from_millis(2900),
            "the pill must get the full configured duration again, got {remaining:?}"
        );
    }

    #[test]
    fn play_pause_spam_collapses_to_the_latest_state_on_the_shown_pill() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.content = Some(MediaEvent::PlaybackStateChanged(
            PlaybackState::Paused,
            "youtube-music".into(),
        ));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() + Duration::from_millis(3000));

        {
            let mut queue = state.queue.lock().unwrap();
            queue.push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Playing,
                "youtube-music".into(),
            )));
            queue.push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Paused,
                "youtube-music".into(),
            )));
            queue.push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Playing,
                "youtube-music".into(),
            )));
        }
        state.receive_events();

        assert!(
            state.pending.is_empty(),
            "the whole burst must update in place, not queue"
        );
        assert!(matches!(
            state.content.as_ref(),
            Some(MediaEvent::PlaybackStateChanged(PlaybackState::Playing, source))
                if source == "youtube-music"
        ));
    }

    #[test]
    fn same_source_state_event_suppressed_while_the_track_pill_is_shown() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.content = Some(MediaEvent::TrackChanged(track_for("youtube-music", "Song", "Artist")));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() + Duration::from_secs(3));

        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Playing,
                "youtube-music".into(),
            )));
        state.receive_events();

        assert!(
            state.pending.is_empty(),
            "a state flip adds nothing while the source's track pill is still visible"
        );
        assert!(matches!(state.content.as_ref(), Some(MediaEvent::TrackChanged(_))));
    }

    #[test]
    fn same_source_state_toggle_during_collapse_brings_the_pill_back() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.content = Some(MediaEvent::PlaybackStateChanged(
            PlaybackState::Paused,
            "youtube-music".into(),
        ));
        state.phase = Phase::Collapsing(Instant::now() - Duration::from_millis(10));
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));

        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Playing,
                "youtube-music".into(),
            )));
        state.receive_events();

        assert!(
            matches!(state.phase, Phase::Shown),
            "the toggle must rescue the collapsing pill"
        );
    }

    #[test]
    fn enqueue_keeps_only_the_newest_track_for_a_source() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.enqueue(MediaEvent::TrackChanged(track_for("youtube-music", "Song A", "Artist")));
        state.enqueue(MediaEvent::TrackChanged(track_for("youtube-music", "Song B", "Artist")));
        assert_eq!(state.pending.len(), 1, "a newer track must supersede the queued one");
        assert!(matches!(
            state.pending.front(),
            Some(MediaEvent::TrackChanged(t)) if t.title == "Song B"
        ));
    }

    #[test]
    fn enqueue_merges_metadata_into_the_queued_track_for_same_media() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.enqueue(MediaEvent::TrackChanged(track_for("youtube-music", "Song", "Artist")));
        let mut refreshed = track_for("youtube-music", "Song", "Artist");
        refreshed.album = "Album".into();
        refreshed.album_artist = "Album Artist".into();
        refreshed.subtitle = "Episode 7".into();
        refreshed.duration_secs = Some(214);
        refreshed.track_number = Some(3);
        refreshed.track_count = Some(12);
        refreshed.genre = Some("Synthwave".into());
        refreshed.artwork = Some(Arc::new([1u8, 2, 3]));
        refreshed.decoded_art = Some(Arc::new([4u8, 5, 6, 7]));
        refreshed.app_icon = Some(Arc::new([8u8, 9, 10, 11]));
        state.enqueue(MediaEvent::TrackChanged(refreshed));
        assert_eq!(state.pending.len(), 1, "a same-media refresh must merge, not queue");
        let queued = match state.pending.front() {
            Some(MediaEvent::TrackChanged(track)) => track,
            other => panic!("expected a queued track, got {other:?}"),
        };
        // Every displayed field the refresh carried lands on the queued pill,
        // not just album/artwork.
        assert_eq!(queued.album, "Album");
        assert_eq!(queued.album_artist, "Album Artist");
        assert_eq!(queued.subtitle, "Episode 7");
        assert_eq!(queued.duration_secs, Some(214));
        assert_eq!(queued.track_number, Some(3));
        assert_eq!(queued.track_count, Some(12));
        assert_eq!(queued.genre.as_deref(), Some("Synthwave"));
        assert!(queued.artwork.is_some());
        assert!(queued.decoded_art.is_some());
        assert!(queued.app_icon.is_some());
    }

    #[test]
    fn enqueue_merge_keeps_the_queued_cover_on_a_no_art_refresh() {
        // SMTC reads artwork only on some passes: a refresh without art must
        // not clobber the already-queued cover (raw bytes, its decode, and
        // the cached app icon all survive).
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        let mut first = track_for("youtube-music", "Song", "Artist");
        first.artwork = Some(Arc::new([1u8, 2, 3]));
        first.decoded_art = Some(Arc::new([4u8, 5, 6, 7]));
        first.app_icon = Some(Arc::new([8u8, 9, 10, 11]));
        state.enqueue(MediaEvent::TrackChanged(first));
        let mut no_art = track_for("youtube-music", "Song", "Artist");
        no_art.duration_secs = Some(180);
        state.enqueue(MediaEvent::TrackChanged(no_art));
        let queued = match state.pending.front() {
            Some(MediaEvent::TrackChanged(track)) => track,
            other => panic!("expected a queued track, got {other:?}"),
        };
        // The duration still merges...
        assert_eq!(queued.duration_secs, Some(180));
        // ...but the cover and icon survive the art-less refresh.
        assert!(queued.artwork.is_some());
        assert!(queued.decoded_art.is_some());
        assert!(queued.app_icon.is_some());
    }

    #[test]
    fn enqueue_keeps_only_the_newest_state_for_a_source() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.enqueue(MediaEvent::PlaybackStateChanged(
            PlaybackState::Paused,
            "youtube-music".into(),
        ));
        state.enqueue(MediaEvent::PlaybackStateChanged(
            PlaybackState::Playing,
            "youtube-music".into(),
        ));
        state.enqueue(MediaEvent::PlaybackStateChanged(
            PlaybackState::Paused,
            "youtube-music".into(),
        ));
        assert_eq!(state.pending.len(), 1, "a newer state must supersede the queued one");
        assert!(matches!(
            state.pending.front(),
            Some(MediaEvent::PlaybackStateChanged(PlaybackState::Paused, source))
                if source == "youtube-music"
        ));
    }

    #[test]
    fn enqueue_keeps_events_from_distinct_sources() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.enqueue(MediaEvent::TrackChanged(track_for("youtube-music", "Song A", "Artist")));
        state.enqueue(MediaEvent::TrackChanged(track_for("spotify", "Song B", "Artist")));
        state.enqueue(MediaEvent::TrackChanged(track_for("youtube-music", "Song C", "Artist")));
        assert_eq!(state.pending.len(), 2, "distinct sources still queue separately");
        assert!(matches!(
            state.pending.front(),
            Some(MediaEvent::TrackChanged(t)) if t.title == "Song C"
        ));
        assert!(matches!(
            state.pending.back(),
            Some(MediaEvent::TrackChanged(t)) if t.title == "Song B"
        ));
    }

    #[test]
    fn sample_duration_scales_with_config() {
        let mut config = Config::default();
        config.overlay.duration_ms = 10_000;
        assert_eq!(sample_duration(&config), Duration::from_millis(10_000));

        config.overlay.duration_ms = 200;
        assert_eq!(sample_duration(&config), Duration::from_millis(500));
    }

    #[test]
    fn update_extension_is_capped_at_configured_duration() {
        let mut config = Config::default();
        config.overlay.duration_ms = 800;
        assert_eq!(update_min_duration(&config), Duration::from_millis(800));

        config.overlay.duration_ms = 5000;
        assert_eq!(update_min_duration(&config), Duration::from_millis(1500));
    }

    #[test]
    fn pill_height_is_constant_across_sparse_and_full_tracks() {
        let config = Config::default();
        // Full track: all four rows (title, artist, meta, source) drawn.
        let full = TrackInfo {
            title: "Title".into(),
            artist: "Artist".into(),
            source_app: "App".into(),
            duration_secs: Some(225),
            ..TrackInfo::default()
        };
        // Sparse track: only the title row would be drawn.
        let minimal = TrackInfo {
            title: "Title".into(),
            ..TrackInfo::default()
        };
        let (width, height) = content_size_of(&config, &MediaEvent::TrackChanged(full), false);
        assert_eq!(width, config.overlay.max_width as f32);
        // Height must clear the sum of all four row bands plus padding, so
        // no row gets clipped.
        let fs = config.appearance.font_size_artist;
        let text_h = config.appearance.font_size_title * ROW_HEIGHT
            + fs * ROW_HEIGHT
            + fs * 0.85 * ROW_HEIGHT
            + fs * 0.85 * ROW_HEIGHT;
        let needed = text_h + 2.0 * config.appearance.padding + 8.0;
        assert!(height >= needed);
        // A sparse track must not shrink the pill: same size, empty space
        // below the drawn rows instead.
        let (_, compact) = content_size_of(&config, &MediaEvent::TrackChanged(minimal), false);
        assert_eq!(compact, height, "missing rows must not shrink the pill");
        // State pills share the same constant height.
        let (_, state_h) = content_size_of(
            &config,
            &MediaEvent::PlaybackStateChanged(PlaybackState::Paused, "App".into()),
            false,
        );
        assert_eq!(state_h, height, "state pills must match the track pill height");
    }

    #[test]
    fn compact_metrics_fit_the_title_row_band() {
        let config = Config::default();
        let metrics = compact_metrics(&config);
        let row_h = config.appearance.font_size_title * ROW_HEIGHT;
        // The art tile and the playback symbol must fit the single title
        // row band; the app icon is the app row's 16 px base.
        assert!(metrics.art <= row_h, "art {} exceeds the row band {row_h}", metrics.art);
        assert!(
            metrics.symbol <= row_h,
            "symbol {} exceeds the row band {row_h}",
            metrics.symbol
        );
        assert_eq!(metrics.icon, 16.0);
        // A huge configured art size must still yield a compact tile.
        let mut big = Config::default();
        big.appearance.art_size = 96;
        assert_eq!(compact_metrics(&big).art, row_h);
    }

    #[test]
    fn compact_size_is_a_single_row_and_smaller_than_expanded() {
        let config = Config::default();
        let (expanded_w, expanded_h) = content_size(&config);
        let (width, height) = compact_size(&config);
        assert!(
            width < expanded_w,
            "compact width {width} must stay below the expanded {expanded_w}"
        );
        assert!(
            height < expanded_h,
            "compact height {height} must stay below the expanded {expanded_h}"
        );
        // One title row + padding, nothing else.
        let row_h = config.appearance.font_size_title * ROW_HEIGHT;
        assert_eq!(height, row_h + 2.0 * config.appearance.padding);
        // The compact pill never exceeds max_width (the expanded pill's cap).
        assert!(width <= config.overlay.max_width as f32);
    }

    #[test]
    fn compact_size_never_exceeds_max_width_at_any_config() {
        // A tiny max_width caps the compact pill at the configured maximum
        // rather than overflowing, and never collapses below the 180 px
        // minimum pill width.
        let mut tiny = Config::default();
        tiny.overlay.max_width = 200;
        let (width, _) = compact_size(&tiny);
        assert!(
            width <= 200.0,
            "compact width {width} must never exceed the configured max_width"
        );
        assert!(
            width >= 180.0,
            "compact width {width} must never collapse below the min pill width"
        );
        // A very wide max_width still caps the compact pill at max_width.
        let mut wide = Config::default();
        wide.overlay.max_width = 800;
        let (width, _) = compact_size(&wide);
        assert!(width <= 800.0);
        assert!(width < 800.0, "a compact pill must not fill the full max width");
    }

    #[test]
    fn compact_title_viewport_reserves_the_trailing_elements() {
        let config = Config::default();
        let metrics = compact_metrics(&config);
        let (pill_w, _) = compact_size(&config);
        let (left, right) = compact_title_viewport(&config);
        // The viewport sits between the art tile (with its 12 px gap) and
        // the trailing chain: 6 px gap, app icon, 16 px gap, playback
        // symbol, padding — the marquee band can never reach them.
        let expected_right = pill_w - config.appearance.padding - metrics.symbol - 16.0 - metrics.icon - 6.0;
        assert_eq!(right, expected_right, "the viewport must end before the icon chain");
        assert_eq!(left, config.appearance.padding + metrics.art + 12.0);
        assert!(
            right > left,
            "the viewport must have positive width (got {left}..{right})"
        );
    }

    #[test]
    fn decide_layout_applies_the_configured_mode_directly() {
        let config = Config::default();
        let verdict = ForegroundVerdict {
            exe: Some("Spotify.exe".into()),
            fullscreen: true,
        };
        let mut explicit = config.clone();
        explicit.overlay.layout = LayoutMode::Expanded;
        assert_eq!(decide_layout(&explicit, &verdict), LayoutMode::Expanded);
        explicit.overlay.layout = LayoutMode::Compact;
        assert_eq!(decide_layout(&explicit, &verdict), LayoutMode::Compact);
    }

    #[test]
    fn decide_layout_auto_compacts_for_fullscreen() {
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::Auto;
        let fullscreen = ForegroundVerdict {
            exe: None,
            fullscreen: true,
        };
        assert_eq!(decide_layout(&config, &fullscreen), LayoutMode::Compact);
        let windowed = ForegroundVerdict {
            exe: None,
            fullscreen: false,
        };
        assert_eq!(decide_layout(&config, &windowed), LayoutMode::Expanded);
    }

    #[test]
    fn decide_layout_auto_compacts_only_for_listed_sources() {
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::Auto;
        config.behavior.auto_compact_sources = vec!["spotify".into()];
        let listed = ForegroundVerdict {
            exe: Some("Spotify.exe".into()),
            fullscreen: false,
        };
        assert_eq!(decide_layout(&config, &listed), LayoutMode::Compact);
        let unlisted = ForegroundVerdict {
            exe: Some("chrome.exe".into()),
            fullscreen: false,
        };
        assert_eq!(decide_layout(&config, &unlisted), LayoutMode::Expanded);
        // An empty list means Auto-compact is off: nothing compacts.
        config.behavior.auto_compact_sources.clear();
        assert_eq!(decide_layout(&config, &listed), LayoutMode::Expanded);
    }

    #[test]
    fn auto_source_matches_strips_the_exe_extension_and_normalizes() {
        let config = Config::default();
        // Mirrors the media_sources convention: normalized case-insensitive
        // substring, with the picker's .exe-stripping applied to the name.
        assert!(auto_source_matches(
            &config_with_sources(&config, ["youtube-music"]),
            Some("YouTube.Music.exe")
        ));
        assert!(auto_source_matches(
            &config_with_sources(&config, ["spotify"]),
            Some("Spotify.exe")
        ));
        // Case and word boundaries in the pattern are irrelevant.
        assert!(auto_source_matches(
            &config_with_sources(&config, ["YouTube Music"]),
            Some("youtube-music.exe")
        ));
        assert!(!auto_source_matches(
            &config_with_sources(&config, ["spotify"]),
            Some("chrome.exe")
        ));
        // An empty list allows nothing (opt-in), and a missing executable
        // identity (elevated process, dead pid) never matches.
        assert!(!auto_source_matches(&config, Some("spotify.exe")));
        assert!(!auto_source_matches(&config_with_sources(&config, ["spotify"]), None));
    }

    fn config_with_sources<const N: usize>(base: &Config, sources: [&str; N]) -> Config {
        let mut config = base.clone();
        config.behavior.auto_compact_sources = sources.iter().map(|s| s.to_string()).collect();
        config
    }

    #[test]
    fn gdi_text_writes_visible_alpha_into_a_dib() {
        // Probes how GDI text writes the alpha channel of a 32bpp top-down
        // DIB: if glyph pixels get alpha 0, the pill's text path needs an
        // alpha fix-up; if they keep/use alpha 255, GDI can draw directly.
        unsafe {
            let hdc = CreateCompatibleDC(None);
            assert!(!hdc.0.is_null());
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 200,
                    biHeight: -40,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = null_mut();
            let bitmap = CreateDIBSection(hdc, &info, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            assert!(!bits.is_null());
            let old = SelectObject(hdc, bitmap);
            // Pre-fill the DIB with an opaque black background like the pill.
            std::ptr::write_bytes(bits.cast::<u8>(), 0, 200 * 40 * 4);
            for i in 0..(200 * 40) {
                (bits.cast::<u8>()).add(i * 4 + 3).write(255);
            }
            let font_name = wide("Segoe UI");
            let font = CreateFontW(
                -16,
                0,
                0,
                0,
                600,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                ANTIALIASED_QUALITY.0 as u32,
                DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
                PCWSTR(font_name.as_ptr()),
            );
            let old_font = SelectObject(hdc, font);
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(0x00FFFFFF));
            let mut text = wide("Hello");
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 200,
                bottom: 40,
            };
            let _ = DrawTextW(hdc, &mut text, &mut rect, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
            // GDI writes alpha 0 for text into 32bpp DIBs; the coverage lives
            // in the RGB channels (text color × coverage). The pill's text
            // path derives alpha from max(R,G,B) and composites premultiplied.
            let mut rgb_lit = 0u32;
            let mut lit_alpha = 0u32;
            for i in 0..(200 * 40) {
                let p = bits.cast::<u8>().add(i * 4);
                let a = *p.add(3) as u32;
                let b = *p as u32;
                if b > 0 {
                    rgb_lit += 1;
                    lit_alpha = a;
                }
            }
            SelectObject(hdc, old_font);
            let _ = DeleteObject(font);
            SelectObject(hdc, old);
            let _ = DeleteObject(bitmap);
            let _ = DeleteDC(hdc);
            assert!(
                rgb_lit > 50,
                "expected visible GDI text pixels in the DIB, got {rgb_lit}"
            );
            assert_eq!(lit_alpha, 0, "GDI text alpha must be 0 (coverage comes from RGB)");
        }
    }

    #[test]
    fn round_rect_coverage_is_solid_inside_and_smooth_at_the_arc() {
        // Center pixel: fully covered.
        assert_eq!(round_rect_coverage(50.0, 20.0, 100.0, 40.0, 16.0), 1.0);
        // Far corner outside the arc: fully transparent.
        assert_eq!(round_rect_coverage(0.0, 0.0, 100.0, 40.0, 16.0), 0.0);
        // On the corner arc (distance radius from its center): half coverage.
        let edge = round_rect_coverage(4.19, 4.19, 100.0, 40.0, 16.0);
        assert!((edge - 0.5).abs() < 0.2, "expected ~0.5 on the arc, got {edge}");
        // Straight edge mid-pill: solid.
        assert_eq!(round_rect_coverage(0.5, 20.0, 100.0, 40.0, 16.0), 1.0);
    }

    #[test]
    fn supersampled_fast_path_is_equivalent_to_full_sampling() {
        // The interior/exterior shortcut must return bit-identical values to
        // the full four-sample evaluation on every pixel it claims, across
        // typical, clamped, and degenerate radii.
        let cases: [(f32, f32, f32); 5] = [
            (100.0, 40.0, 16.0),
            (340.0, 110.0, 26.0),
            (48.0, 48.0, 9.6),
            (40.0, 40.0, 40.0), // radius clamped to half the size
            (100.0, 40.0, 0.1), // degenerate near-zero radius
        ];
        let full = |x: f32, y: f32, w: f32, h: f32, r: f32| {
            let cov = |dx: f32, dy: f32| round_rect_coverage(x + dx, y + dy, w, h, r);
            (cov(-0.35, -0.35) + cov(0.35, -0.35) + cov(-0.35, 0.35) + cov(0.35, 0.35)) * 0.25
        };
        for (w, h, r) in cases {
            for y in -8..(h as i32 + 8) {
                for x in -8..(w as i32 + 8) {
                    let xf = x as f32;
                    let yf = y as f32;
                    let expected = full(xf, yf, w, h, r);
                    let actual = round_rect_coverage_supersampled(xf, yf, w, h, r);
                    assert_eq!(
                        actual, expected,
                        "shortcut differs from full sampling at ({x}, {y}) in {w}x{h} r={r}"
                    );
                }
            }
        }
    }

    #[test]
    fn supersampled_fast_path_claims_solid_interior_and_void_exterior() {
        // The shortcut must claim exactly 1.0 for a pixel well inside the
        // rect and exactly 0.0 for a pixel well outside it.
        assert_eq!(round_rect_coverage_fast(30.0, 20.0, 100.0, 40.0, 16.0), Some(1.0));
        assert_eq!(
            round_rect_coverage_fast(30.0, 5.0, 100.0, 40.0, 16.0),
            None,
            "pixels near the straight edge still need sampling"
        );
        assert_eq!(round_rect_coverage_fast(-5.0, 20.0, 100.0, 40.0, 16.0), Some(0.0));
        assert_eq!(round_rect_coverage_fast(105.0, 45.0, 100.0, 40.0, 16.0), Some(0.0));
        assert_eq!(round_rect_coverage_fast(101.0, 20.0, 100.0, 40.0, 16.0), Some(0.0));
        assert_eq!(
            round_rect_coverage_fast(100.0, 20.0, 100.0, 40.0, 16.0),
            None,
            "the edge pixel itself carries the anti-aliased sliver"
        );
    }

    #[test]
    fn aura_glow_appears_outside_the_pill() {
        // Buffer larger than the pill so the outer ring has room for the glow.
        let buf = 50;
        let mut pixels = vec![0u8; buf * buf * 4];
        let palette = Palette {
            primary: [255, 0, 0, 255],
            secondary: [0, 0, 255, 255],
        };
        let inset = 12usize;
        let pill_w = buf - inset * 2;
        let pill_h = buf - inset * 2;
        draw_aura(&mut pixels, buf, buf, palette, inset, pill_w, pill_h, 8.0, 1.0);
        let alpha_at = |x: usize, y: usize| pixels[(y * buf + x) * 4 + 3];
        // Just outside the pill boundary (d ≈ 1.5): visible.
        let near = alpha_at(inset + pill_w + 1, inset + pill_h / 2);
        assert!(near > 0, "outer glow must be visible just outside the pill");
        // Farther out (d ≈ 5.5, inside the 6px halo): still visible but weaker.
        let far = alpha_at(inset + pill_w + 5, inset + pill_h / 2);
        assert!(far > 0, "glow must extend beyond the pill edge");
        assert!(near > far, "glow must fade with distance from the pill");
        // Beyond the halo extent: nothing.
        let beyond = alpha_at(inset + pill_w + 7, inset + pill_h / 2);
        assert_eq!(beyond, 0, "no glow beyond the halo extent");
        // Inside the pill: no aura (covered by body fill).
        let inside = alpha_at(inset + 2, inset + pill_h / 2);
        assert_eq!(inside, 0, "inside the pill there must be no aura");
    }

    #[test]
    fn fill_tint_mixes_toward_the_accent_and_keeps_alpha() {
        let base = [160, 160, 180, 38];
        let accent = [255, 0, 0, 255];
        assert_eq!(
            tinted_fill(base, accent, 0.0),
            base,
            "zero weight leaves the base untouched"
        );
        let full = tinted_fill(base, accent, 1.0);
        assert_eq!([full[0], full[1], full[2]], [accent[0], accent[1], accent[2]]);
        let half = tinted_fill(base, accent, 0.5);
        assert_eq!(half, [208, 80, 90, 38], "weight 0.5 lands at the channel midpoint");
        assert_eq!(half[3], base[3], "alpha must be preserved");
    }

    #[test]
    fn circle_coverage_is_smooth() {
        assert_eq!(circle_coverage(16.0, 16.0, 32.0), 1.0);
        assert_eq!(circle_coverage(0.0, 0.0, 32.0), 0.0);
        // On the circle boundary (radius from the center): half coverage.
        let edge = circle_coverage(4.2, 4.2, 32.0);
        assert!((edge - 0.5).abs() < 0.2, "expected ~0.5 on the boundary, got {edge}");
    }

    #[test]
    fn clock_icon_coverage_has_ring_hands_and_an_open_face() {
        let size = 12.0;
        // Ring at 3 o'clock (pixel center on the ring centerline).
        assert!(
            clock_icon_coverage(10.0, 6.0, size) > 0.8,
            "the ring must cover its stroke, got {}",
            clock_icon_coverage(10.0, 6.0, size)
        );
        // Hour hand (12 o'clock) and minute hand (3 o'clock), both near solid.
        assert!(clock_icon_coverage(6.0, 4.0, size) > 0.5, "hour hand must be drawn");
        assert!(clock_icon_coverage(9.0, 6.0, size) > 0.5, "minute hand must be drawn");
        // The face is open: the center of the box is inside the ring and
        // uncovered, and the corners are far outside everything.
        assert!(
            clock_icon_coverage(6.0, 6.0, size) < 1.0,
            "the center must not be a solid filled disk"
        );
        assert_eq!(clock_icon_coverage(0.0, 0.0, size), 0.0, "corners stay empty");
        let outside = clock_icon_coverage(0.0, 11.0, size);
        assert!(outside < 0.9, "the icon must stay inside its box, got {outside}");
    }

    #[test]
    fn clock_icon_renders_into_the_buffer() {
        let mut pixels = vec![0u8; 40 * 40 * 4];
        draw_clock_icon_pixels(&mut pixels, 40, 10, 10, 12.0, [153, 153, 153, 255]);
        let lit = pixels.chunks(4).filter(|p| p[3] > 0).count();
        assert!(lit > 30, "expected a visible clock icon, got {lit} lit pixels");
        // The composite writes the icon's color, strongest where coverage is
        // fullest, and respects the box: nothing outside its 12×12 footprint
        // draws.
        let (_, px) = pixels
            .chunks(4)
            .enumerate()
            .filter(|(_, p)| p[3] > 0)
            .max_by_key(|(_, p)| p[3])
            .unwrap();
        assert!(px[3] >= 120, "icon requires a near-solid stroke, got alpha {}", px[3]);
        assert!(px[0] >= 70, "stroke must carry the icon color, got {px:?}");
        assert_eq!(px[0], px[2], "gray icon color must stay neutral, got {px:?}");
        let outside = pixels
            .chunks(4)
            .enumerate()
            .any(|(i, p)| p[3] > 0 && !(i % 40 >= 10 && i % 40 < 22 && i / 40 >= 10 && i / 40 < 22));
        assert!(!outside, "clock icon must stay inside its box");
    }

    #[test]
    fn text_line_rasterizes_into_pixels() {
        let mut pixels = vec![0u8; 200 * 40 * 4];
        let rect = RECT {
            left: 0,
            top: 0,
            right: 200,
            bottom: 40,
        };
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let (font, h) = state.fonts.font_for(12, false);
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut pixels,
            200,
            "Hello World",
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            None,
        );
        let lit = pixels.chunks(4).filter(|p| p[3] > 0).count();
        assert!(lit > 100, "expected glyph pixels in the buffer, got {lit}");
    }

    #[test]
    fn gray_glyph_interiors_carry_full_alpha() {
        // Regression guard for the double-attenuation bug: when the text color
        // was set on the scratch DC, GDI pre-dimmed the pixels (coverage ×
        // color), and reading that dimmed value back as coverage made a #808080
        // glyph render at ~50% opacity instead of solid. Drawing white and
        // applying the color while compositing keeps the glyph interior at full
        // alpha regardless of brightness.
        let mut pixels = vec![0u8; 200 * 80 * 4];
        let rect = RECT {
            left: 0,
            top: 0,
            right: 200,
            bottom: 80,
        };
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let (font, h) = state.fonts.font_for(48, false);
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut pixels,
            200,
            "MMMMMM",
            &rect,
            font,
            h,
            [0x80, 0x80, 0x80, 0xFF],
            false,
            1.0,
            None,
        );
        // Find the highest alpha in the buffer: the interior of the glyphs.
        let max_alpha = pixels.chunks(4).map(|p| p[3]).max().unwrap_or(0);
        assert!(
            max_alpha >= 240,
            "gray glyph interiors must be nearly opaque, got max alpha {max_alpha}"
        );
        // The interior pixel must also be the requested gray, not the color
        // scaled down by the coverage mask under it.
        let (px, py, pr, pb) = pixels
            .chunks(4)
            .enumerate()
            .find(|(_, p)| p[3] == max_alpha)
            .map(|(i, p)| (i % 200, i / 200, p[0], p[2]))
            .expect("gray glyph must have an interior pixel");
        assert!(
            (pb as i32 - 0x80).abs() <= 8
                && (pixels[(py * 200 + px) * 4 + 1] as i32 - 0x80).abs() <= 8
                && (pr as i32 - 0x80).abs() <= 8,
            "interior should render the requested gray, got [{pb}, {}, {pr}]",
            pixels[(py * 200 + px) * 4 + 1]
        );
    }

    #[test]
    fn text_line_renders_with_marquee_state() {
        let mut pixels = vec![0u8; 200 * 40 * 4];
        let rect = RECT {
            left: 0,
            top: 0,
            right: 200,
            bottom: 40,
        };
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let mut scroll = LineScroll::default();
        let mut strip = None;
        let (font, h) = state.fonts.font_for(12, false);
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut pixels,
            200,
            "Hello World",
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
        );
        let lit = pixels.chunks(4).filter(|p| p[3] > 0).count();
        assert!(lit > 100, "expected glyph pixels with marquee state, got {lit}");
    }

    #[test]
    fn marquee_flag_triggers_when_text_overflows_the_narrowed_rect() {
        // Regression: text cut off by the symbol-narrowed draw rect must
        // mark the line as scrolling — it must not sit truncated forever.
        let mut pixels = vec![0u8; 200 * 40 * 4];
        let rect = RECT {
            left: 0,
            top: 0,
            right: 80, // narrow, like a title row next to the symbol
            bottom: 40,
        };
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let mut scroll = LineScroll::default();
        let mut strip = None;
        let (font, h) = state.fonts.font_for(12, false);
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut pixels,
            200,
            "Feel It (Official Music Video)",
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
        );
        assert!(
            scroll.scrolling,
            "a title wider than the visible band must be marked as scrolling"
        );
    }

    #[test]
    fn marquee_strip_is_built_once_and_reused_across_ticks() {
        // The scrolling branch must rasterize the overflowing line into the
        // strip on the first tick, then serve later ticks from the cache: the
        // strip pixels must not change when nothing that affects the raster
        // changed (this is the property that removes ExtTextOutW from the
        // per-tick path).
        let mut pixels = vec![0u8; 200 * 40 * 4];
        let rect = RECT {
            left: 0,
            top: 0,
            right: 80, // narrow, forces overflow
            bottom: 40,
        };
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let mut scroll = LineScroll {
            // Past the static hold so the draw takes the scrolling branch.
            started_at: Some(Instant::now() - MARQUEE_HOLD - Duration::from_millis(100)),
            ..LineScroll::default()
        };
        let mut strip = None;
        let (font, h) = state.fonts.font_for(12, false);
        let value = "Feel It (Official Music Video)";
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut pixels,
            200,
            value,
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
        );
        let built = strip.as_ref().expect("scrolling line must build a strip");
        assert!(built.text_w > 80, "strip must carry the full text width");
        assert_eq!(built.rw, 80, "strip must remember the visible band width");
        let first_pixels = built.pixels.clone();

        // Second tick with identical content/size/font/color: cache hit.
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut pixels,
            200,
            value,
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
        );
        assert_eq!(
            strip.as_ref().unwrap().pixels,
            first_pixels,
            "unchanged content must not re-rasterize the strip"
        );
    }

    #[test]
    fn marquee_strip_rebuilds_when_the_content_changes() {
        // The strip is keyed by its raster inputs; a new value must invalidate
        // it so the old glyphs never scroll for the new text.
        let mut pixels = vec![0u8; 200 * 40 * 4];
        let rect = RECT {
            left: 0,
            top: 0,
            right: 80,
            bottom: 40,
        };
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let mut scroll = LineScroll {
            started_at: Some(Instant::now() - MARQUEE_HOLD - Duration::from_millis(100)),
            ..LineScroll::default()
        };
        let mut strip = None;
        let (font, h) = state.fonts.font_for(12, false);
        let first = "Feel It (Official Music Video)";
        let second = "A Different Song Name That Also Overflows";
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut pixels,
            200,
            first,
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
        );
        assert_eq!(strip.as_ref().unwrap().value, first);
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut pixels,
            200,
            second,
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
        );
        assert_eq!(
            strip.as_ref().unwrap().value,
            second,
            "a new text value must rebuild the strip"
        );
    }

    #[test]
    fn marquee_strip_composites_both_copies_inside_the_window() {
        // The visible window of a scrolling line is two contiguous runs of the
        // strip (the tail from -off and the head after one total period).
        // Everything between the copies stays background.
        let strip = MarqueeStrip {
            value: "x".into(),
            rw: 40,
            rh: 20,
            font: HFONT(std::ptr::null_mut()),
            font_height: 10,
            color: [0, 0, 255, 255],
            centered: false,
            text_w: 10,
            pixels: [255u8, 0, 0, 255].repeat(10 * 20), // solid premultiplied blue (BGRA)
        };
        let mut pixels = vec![0u8; 40 * 20 * 4];
        let rect = RECT {
            left: 0,
            top: 0,
            right: 40,
            bottom: 20,
        };
        composite_marquee_strip(&mut pixels, 40, &rect, &strip, 2, 30, 0.0, 0.0);
        for (i, p) in pixels.chunks(4).enumerate() {
            let x = i % 40;
            let in_tail = (0..8).contains(&x); // x1 = -2, x1 + text_w = 8
            let in_head = (28..38).contains(&x); // x2 = 28, x2 + text_w = 38
            if in_tail || in_head {
                assert_eq!(p, &[255, 0, 0, 255], "strip pixels must land at x={x} (row {})", i / 40);
            } else {
                assert_eq!(p, &[0, 0, 0, 0], "background must stay clear at x={x}");
            }
        }
    }

    #[test]
    fn edge_fade_factor_ramps_to_zero_at_both_edges() {
        // The mask must read 0 at each visible boundary, 1 across the
        // interior, and linear in between; a zero fade width or a degenerate
        // rect must disable it entirely. The two edge widths are independent:
        // disabling one edge leaves that boundary at full opacity.
        let (left, right, fade_w) = (10.0, 110.0, 12.0);
        let at = |x: f32| edge_fade_factor(x, left, right, fade_w, fade_w);
        assert_eq!(at(left), 0.0, "left boundary must be fully faded");
        assert!(
            (at(left + fade_w / 2.0) - 0.5).abs() < 1e-6,
            "halfway through the left fade must read ~0.5"
        );
        assert_eq!(at((left + right) / 2.0), 1.0, "the interior must stay at full opacity");
        assert!(
            (at(right - fade_w / 2.0) - 0.5).abs() < 1e-6,
            "halfway through the right fade must read ~0.5"
        );
        assert_eq!(at(right), 0.0, "right boundary must be fully faded");
        assert_eq!(
            edge_fade_factor(50.0, 0.0, 100.0, 0.0, 0.0),
            1.0,
            "a zero fade width must disable the mask"
        );
        assert_eq!(
            edge_fade_factor(50.0, 100.0, 50.0, 12.0, 12.0),
            1.0,
            "a degenerate rect must disable the mask"
        );
        // Trailing-only hold fade: a disabled left edge keeps the left
        // boundary at full opacity while the right edge still ramps (and the
        // mirror case for a disabled right edge).
        assert_eq!(
            edge_fade_factor(left, left, right, 0.0, fade_w),
            1.0,
            "a disabled left edge must keep the left boundary at full opacity"
        );
        assert_eq!(
            edge_fade_factor(right, left, right, 0.0, fade_w),
            0.0,
            "the right edge must still fade when only the left is disabled"
        );
        assert_eq!(
            edge_fade_factor(left, left, right, fade_w, 0.0),
            0.0,
            "the left edge must still fade when only the right is disabled"
        );
        assert_eq!(
            edge_fade_factor(right, left, right, fade_w, 0.0),
            1.0,
            "a disabled right edge must keep the right boundary at full opacity"
        );
    }

    #[test]
    fn non_overflowing_marquee_line_renders_identically_to_static_text() {
        // The edge mask must not touch text that fits its band: a fitting line
        // drawn with marquee state must be byte-identical to the same line
        // drawn without it, and must not be flagged as scrolling.
        let rect = RECT {
            left: 0,
            top: 0,
            right: 200,
            bottom: 40,
        };
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let (font, h) = state.fonts.font_for(12, false);
        let mut static_pixels = vec![0u8; 200 * 40 * 4];
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut static_pixels,
            200,
            "Hello",
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            None,
        );
        let mut marquee_pixels = vec![0u8; 200 * 40 * 4];
        let mut scroll = LineScroll::default();
        let mut strip = None;
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut marquee_pixels,
            200,
            "Hello",
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
        );
        assert!(!scroll.scrolling, "fitting text must not be marked as scrolling");
        assert!(strip.is_none(), "fitting text must not build a strip");
        assert_eq!(
            marquee_pixels, static_pixels,
            "non-overflowing marquee text must render exactly like static text"
        );
    }

    #[test]
    fn overflowing_hold_shows_full_text_and_stays_stationary() {
        // One LineScroll and one Option<MarqueeStrip> serve both the hold and
        // the scroll: the strip is built on the first overflow frame, and the
        // same cached raster must be reused through the hold and into the
        // scroll, with the offset held at 0 the whole time. The hold fades
        // only the trailing edge (the text head sits at the band boundary and
        // is not clipped); once the hold elapses, both edges fade.
        let rect = RECT {
            left: 0,
            top: 0,
            right: 80, // narrow, forces overflow
            bottom: 40,
        };
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let (font, h) = state.fonts.font_for(12, false);
        let value = "Feel It (Official Music Video)";

        let mut scroll = LineScroll {
            started_at: Some(Instant::now()),
            ..LineScroll::default()
        };
        let mut strip = None;

        // --- Hold phase: full text at offset 0 ---
        let mut hold_pixels = vec![0u8; 200 * 40 * 4];
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut hold_pixels,
            200,
            value,
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
        );
        assert!(scroll.scrolling, "overflow must be detected during the hold");
        assert_eq!(scroll.offset, 0.0, "the hold must not advance the scroll offset");

        // A second hold frame must be served from the same cached strip: the
        // strip pixels must not change (no fresh GDI rasterization on later
        // hold frames) and the frame must be pixel-identical.
        let strip_before = strip
            .as_ref()
            .expect("the hold must build the cached strip")
            .pixels
            .clone();
        let mut hold2_pixels = vec![0u8; 200 * 40 * 4];
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut hold2_pixels,
            200,
            value,
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
        );
        assert_eq!(
            strip.as_ref().unwrap().pixels,
            strip_before,
            "a second hold frame must reuse the cached strip without re-rasterizing"
        );
        assert_eq!(
            hold2_pixels, hold_pixels,
            "a second hold frame must render identically to the first"
        );
        assert_eq!(scroll.offset, 0.0, "the offset must stay at 0 throughout the hold");

        let built = strip.as_ref().expect("the hold must build the cached strip");
        assert!(
            built.text_w > 80,
            "the strip must carry the full natural text width, not an ellipsized band"
        );
        assert!(
            state.text_scratch.as_ref().unwrap().width > 80,
            "the strip build must rasterize the full natural text width"
        );
        // The raster must contain the title's tail beyond the visible band:
        // an ellipsized draw would end at the band width and leave the
        // strip's outer columns empty, while the full title clips there.
        let tail_lit = (0..built.rh as usize).any(|r| {
            let row = r * built.text_w as usize * 4;
            built.pixels[row + rect.right as usize * 4..row + built.text_w as usize * 4]
                .chunks(4)
                .any(|p| p[3] > 0)
        });
        assert!(
            tail_lit,
            "the hold raster must contain the title's tail, not an ellipsis"
        );
        // Owned copies so the comparisons below can run while `strip` is
        // borrowed mutably for the scrolling draw.
        let strip_pixels = built.pixels.clone();
        let tw = built.text_w as usize;

        // Leading edge stays unfaded during the hold: every pixel of the hold
        // frame inside the left fade zone is byte-identical to the cached
        // strip (fade_left is disabled, so the mask never touches it).
        for r in 0..rect.bottom as usize {
            for x in 0..MARQUEE_FADE as usize {
                let s = &strip_pixels[(r * tw + x) * 4..(r * tw + x) * 4 + 4];
                let f = &hold_pixels[(r * 200 + x) * 4..(r * 200 + x) * 4 + 4];
                assert_eq!(s, f, "the hold must not fade the leading edge at row {r}, column {x}");
            }
        }

        // The trailing edge must fade during the hold: the tail crossing the
        // band's right boundary is attenuated against the cached, unfaded
        // strip (scanning every row over the columns strictly inside the
        // 12px fade zone, where the factor is always < 1).
        let zone_from = (rect.right - MARQUEE_FADE as i32 + 1) as usize;
        let strip_max = (0..rect.bottom as usize)
            .flat_map(|r| {
                let row = r * tw * 4;
                strip_pixels[row + zone_from * 4..row + rect.right as usize * 4]
                    .chunks(4)
                    .map(|p| p[3])
            })
            .max()
            .unwrap_or(0);
        let frame_max = (0..rect.bottom as usize)
            .flat_map(|r| {
                let row = r * 200 * 4;
                hold_pixels[row + zone_from * 4..row + rect.right as usize * 4]
                    .chunks(4)
                    .map(|p| p[3])
            })
            .max()
            .unwrap_or(0);
        assert!(
            strip_max > frame_max,
            "the hold must fade the tail at the right boundary (strip {strip_max} vs frame {frame_max})"
        );

        // --- Scroll phase: the hold elapses, the same strip scrolls ---
        scroll.started_at = Some(Instant::now() - MARQUEE_HOLD - Duration::from_millis(100));
        // The offset is still 0 (the tick only advances it after the hold):
        // the first scrolling frame shows the same window, now with both edge
        // fades active.
        let mut scroll_pixels = vec![0u8; 200 * 40 * 4];
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut scroll_pixels,
            200,
            value,
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
        );
        assert_eq!(
            strip.as_ref().unwrap().pixels,
            strip_pixels,
            "the scrolling draw must reuse the same cached strip built during the hold"
        );

        // Both edges fade once scrolling begins: every visible column of the
        // scrolling frame must match the same strip column scaled by the
        // linear edge mask — x / fade_w from the left boundary, (rw - x) /
        // fade_w from the right, 1.0 across the interior. The strip holds
        // premultiplied white text (RGB == alpha), so the expected pixel is
        // the masked alpha in every channel.
        let rw_f = rect.right as f32;
        for r in 0..rect.bottom as usize {
            for x in 0..rect.right as usize {
                let fade = ((x as f32) / MARQUEE_FADE)
                    .min((rw_f - x as f32) / MARQUEE_FADE)
                    .clamp(0.0, 1.0);
                let sa = strip_pixels[(r * tw + x) * 4 + 3] as f32;
                let a = (sa * fade).round() as u8;
                let f = &scroll_pixels[(r * 200 + x) * 4..(r * 200 + x) * 4 + 4];
                assert_eq!(
                    f,
                    &[a, a, a, a],
                    "the scrolling frame must match strip x fade at row {r}, column {x}"
                );
            }
        }
    }

    #[test]
    fn marquee_strip_composite_fades_the_visible_edges() {
        // The scrolling path must fade glyphs near both visible boundaries
        // while the row interior keeps full opacity. The strip holds
        // premultiplied pixels, so the fade must scale RGB with alpha: a
        // faded pixel keeps its hue while its coverage falls.
        let strip = MarqueeStrip {
            value: "x".into(),
            rw: 40,
            rh: 20,
            font: HFONT(std::ptr::null_mut()),
            font_height: 10,
            color: [0, 0, 255, 255],
            centered: false,
            text_w: 40,
            pixels: [255u8, 0, 0, 255].repeat(40 * 20), // solid premultiplied blue (BGRA)
        };
        let mut pixels = vec![0u8; 40 * 20 * 4];
        let rect = RECT {
            left: 0,
            top: 0,
            right: 40,
            bottom: 20,
        };
        composite_marquee_strip(&mut pixels, 40, &rect, &strip, 2, 70, MARQUEE_FADE, MARQUEE_FADE);
        let px = |x: usize| -> [u8; 4] {
            let p = &pixels[x * 4..x * 4 + 4];
            [p[0], p[1], p[2], p[3]]
        };
        assert_eq!(px(0), [0, 0, 0, 0], "the left boundary must be fully faded");
        let half = px(6); // halfway through the 12px left fade
        assert!(
            (half[3] as f32 - 127.5).abs() <= 2.0,
            "the left fade must halve the alpha at its midpoint, got {:?}",
            half
        );
        assert!(
            (half[0] as i32 - half[3] as i32).abs() <= 2,
            "premultiplied RGB must fade with the alpha, got {:?}",
            half
        );
        assert_eq!(px(12), [255, 0, 0, 255], "the left fade boundary must be full opacity");
        assert_eq!(px(20), [255, 0, 0, 255], "the row center must keep its original alpha");
        let tail = px(31); // three quarters through the right fade: 255 * 0.75
        assert!(
            (tail[3] as f32 - 191.0).abs() <= 2.0,
            "the right fade must attenuate toward the edge, got {:?}",
            tail
        );
        let edge = px(37); // 3px before the right boundary: 255 * 0.25
        assert!(
            (edge[3] as f32 - 64.0).abs() <= 2.0,
            "the right fade must approach zero at the boundary, got {:?}",
            edge
        );
        // Same attenuation on a middle row: the mask must be uniform down the
        // row, not just at the first scanline.
        let mid = &pixels[10 * 40 * 4 + 6 * 4..10 * 40 * 4 + 6 * 4 + 4];
        assert!(
            (mid[3] as i32 - half[3] as i32).abs() <= 1,
            "every row must fade identically, got {:?} vs {:?}",
            mid,
            half
        );
    }

    #[test]
    fn track_pill_renders_text() {
        let mut pixels = vec![0u8; 240 * 76 * 4];
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let track = TrackInfo {
            title: "Everything, Everywhere".into(),
            artist: "John Muirhead".into(),
            ..TrackInfo::default()
        };
        draw_text_pixels(
            &mut state,
            &mut pixels,
            &MediaEvent::TrackChanged(track),
            240,
            76,
            1.0,
            false,
        );
        let lit = pixels.chunks(4).filter(|p| p[3] > 0).count();
        assert!(lit > 500, "expected text + art pixels, got {lit}");
    }

    #[test]
    fn text_sits_inside_its_row_band() {
        // Regression guard for the glyph vertical placement: with a 40px-tall
        // row, glyph pixels must land in the upper two thirds, not below the
        // baseline-clipped region (a sign error once pushed them out of the
        // row entirely).
        let mut pixels = vec![0u8; 200 * 40 * 4];
        let rect = RECT {
            left: 0,
            top: 0,
            right: 200,
            bottom: 40,
        };
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let (font, h) = state.fonts.font_for(12, false);
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            &mut pixels,
            200,
            "Hello",
            &rect,
            font,
            h,
            [255, 255, 255, 255],
            false,
            1.0,
            None,
        );
        let upper = pixels
            .chunks(4)
            .enumerate()
            .filter(|(i, p)| p[3] > 0 && *i / 200 < 27)
            .count();
        let lower = pixels
            .chunks(4)
            .enumerate()
            .filter(|(i, p)| p[3] > 0 && *i / 200 >= 27)
            .count();
        assert!(upper > 50, "expected glyphs in the upper part of the row, got {upper}");
        assert!(
            lower < upper,
            "glyphs must not sit at the bottom of the row (lower={lower})"
        );
    }

    #[test]
    fn current_source_is_cleared_when_the_pill_hides() {
        // Regression: current_source was set by show_next() when a TrackChanged
        // pill was shown and never cleared, so ALL subsequent PlaybackStateChanged
        // pills from the same source were permanently suppressed — the user saw
        // Paused pills but no Playing pills after the first track notification.
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());

        // Simulate the state show_next() leaves behind after a TrackChanged.
        state.current_source = Some("youtube-music".to_string());
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".to_string(),
            ..TrackInfo::default()
        }));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() + Duration::from_secs(1));

        state.hide();

        assert!(
            state.current_source.is_none(),
            "current_source must clear when the pill collapses"
        );
        assert!(matches!(state.phase, Phase::Hidden));
    }

    #[test]
    fn preview_if_hidden_shows_a_sample_only_while_hidden() {
        // Settings pushes (position/layout/separation) preview a hidden pill
        // instead of silently deferring to the next show; a visible pill must
        // be left alone so the caller repaints it in place.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        assert!(matches!(state.phase, Phase::Hidden));
        assert!(state.preview_if_hidden(), "a hidden pill must show a sample");
        assert!(
            matches!(state.phase, Phase::Light(_)),
            "the sample must take the light-up phase"
        );

        state.phase = Phase::Shown;
        assert!(
            !state.preview_if_hidden(),
            "a visible pill must not be replaced by a sample"
        );
        assert!(matches!(state.phase, Phase::Shown));
    }

    #[test]
    fn queued_update_caps_the_current_pill_remaining_time() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        // A pill is fully shown with a long remaining duration.
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        }));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() + Duration::from_secs(5));

        // A newer event arrives from another source (not an in-place update).
        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(TrackInfo {
                source_app: "spotify".into(),
                title: "Next Song".into(),
                ..TrackInfo::default()
            })));
        state.receive_events();

        assert!(!state.pending.is_empty(), "the update must be queued for the next pill");
        let remaining = state.dismiss_at.unwrap().saturating_duration_since(Instant::now());
        assert!(
            remaining <= Duration::from_millis(EARLY_EXIT_MS + 50),
            "remaining time must be capped near EARLY_EXIT_MS, got {remaining:?}"
        );
    }

    #[test]
    fn queued_update_never_extends_an_earlier_deadline() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        }));
        state.phase = Phase::Shown;
        // Already sooner than EARLY_EXIT_MS (e.g. hover-dismiss armed).
        let earlier = Instant::now() + Duration::from_millis(200);
        state.dismiss_at = Some(earlier);

        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(TrackInfo {
                source_app: "spotify".into(),
                title: "Next Song".into(),
                ..TrackInfo::default()
            })));
        state.receive_events();

        let remaining = state.dismiss_at.unwrap().saturating_duration_since(Instant::now());
        assert!(
            remaining <= Duration::from_millis(250),
            "an earlier deadline must not be extended, got {remaining:?}"
        );
    }

    #[test]
    fn pause_symbol_draws_two_separate_bars() {
        let mut pixels = vec![0u8; 100 * 100 * 4];
        let size = 40.0;
        // Right-aligned at x=100, y=30 — a 40px box, bars centered in it.
        draw_symbol_pixels(
            &mut pixels,
            100,
            100,
            30,
            size,
            PlaybackState::Paused,
            [255, 255, 255, 255],
        );
        let lit = pixels.chunks(4).filter(|p| p[3] > 0).count();
        assert!(lit > 0, "pause symbol must draw pixels");
        // Two bars: scan the vertical midline of the box for two separate
        // lit clusters (the gap between bars must be dark).
        let mid_y = 50; // vertically centered in the 30..70 band
        let mut clusters = 0;
        let mut in_bar = false;
        for x in 0..100 {
            let alpha = pixels[(mid_y * 100 + x) * 4 + 3];
            if alpha > 0 && !in_bar {
                clusters += 1;
                in_bar = true;
            } else if alpha == 0 {
                in_bar = false;
            }
        }
        assert_eq!(
            clusters, 2,
            "expected two bars with a gap, got {clusters} lit cluster(s)"
        );
    }

    #[test]
    fn play_symbol_draws_a_triangle() {
        let mut pixels = vec![0u8; 100 * 100 * 4];
        draw_symbol_pixels(
            &mut pixels,
            100,
            100,
            30,
            40.0,
            PlaybackState::Playing,
            [255, 255, 255, 255],
        );
        let lit = pixels.chunks(4).filter(|p| p[3] > 0).count();
        assert!(lit > 0, "play symbol must draw pixels");
    }

    #[test]
    fn ease_out_quint_is_monotonic_and_clamped() {
        assert_eq!(ease_out_quint(0.0), 0.0);
        assert_eq!(ease_out_quint(1.0), 1.0);
        // Out-of-range inputs clamp, never panic or overshoot.
        assert_eq!(ease_out_quint(-1.0), 0.0);
        assert_eq!(ease_out_quint(2.0), 1.0);
        let mut last = 0.0;
        for i in 0..=100 {
            let v = ease_out_quint(i as f32 / 100.0);
            assert!(v >= last - 1e-6, "quint must be non-decreasing");
            last = v;
        }
    }

    #[test]
    fn ease_out_back_overshoots_then_settles() {
        // Floating point keeps the t=0 endpoint at ~-1e-7; anything that tiny
        // is visually identical to 0 and harmless (render clamps sizes).
        assert!(ease_out_back(0.0).abs() < 1e-6);
        assert!((ease_out_back(1.0) - 1.0).abs() < 1e-6);
        // The spring peaks above 1.0 in the middle of the curve...
        let peak = (0..=100)
            .map(|i| ease_out_back(i as f32 / 100.0))
            .fold(0.0_f32, f32::max);
        assert!(peak > 1.0 && peak < 1.2, "spring overshoot out of range: {peak}");
        // ...and never dips below the start or above the sanity bound.
        for i in 0..=100 {
            let v = ease_out_back(i as f32 / 100.0);
            assert!((-1e-6..=1.2).contains(&v), "ease_out_back out of range: {v}");
        }
    }

    #[test]
    fn expanding_alpha_reaches_full_before_the_end() {
        let mut config = Config::default();
        config.overlay.animation_ms = 200;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.phase = Phase::Expanding(Instant::now() - Duration::from_millis(100));
        // At half the duration the scale is still mid-flight (overshooting, so
        // not settled at 1.0), but alpha must already be at full strength
        // (decoupled opacity).
        let (alpha, shape) = state.frame();
        assert_eq!(alpha, 255);
        assert!(
            (shape - 1.0).abs() > 1e-3,
            "scale should not be settled at t=0.5, got {shape}"
        );
    }

    #[test]
    fn stop_symbol_draws_a_square() {
        let mut pixels = vec![0u8; 100 * 100 * 4];
        draw_symbol_pixels(
            &mut pixels,
            100,
            100,
            30,
            40.0,
            PlaybackState::Stopped,
            [255, 255, 255, 255],
        );
        let lit = pixels.chunks(4).filter(|p| p[3] > 0).count();
        assert!(lit > 0, "stop symbol must draw pixels");
    }

    #[test]
    fn symbol_is_right_aligned_within_its_box() {
        let mut pixels = vec![0u8; 200 * 80 * 4];
        // Symbol box right edge at x=200, size 32 → box spans 168..200.
        let size = 32.0_f32;
        draw_symbol_pixels(
            &mut pixels,
            200,
            200,
            24,
            size,
            PlaybackState::Paused,
            [255, 255, 255, 255],
        );
        // Find the rightmost lit pixel in the vertical center band.
        let mut rightmost_lit = -1_i32;
        for x in 0..200 {
            for y in 30..70 {
                if pixels[(y * 200 + x) * 4 + 3] > 0 {
                    rightmost_lit = rightmost_lit.max(x as i32);
                }
            }
        }
        assert!(rightmost_lit >= 0, "symbol must have lit pixels");
        // Rightmost lit pixel should be inside the right half of the box
        // (i.e. within 100px..200px), confirming the symbol sits in the
        // right-aligned box, not on the left.
        assert!(
            rightmost_lit >= 168,
            "rightmost lit pixel at {rightmost_lit} should be inside the right-aligned box (>=168)"
        );
    }

    #[test]
    fn rounded_triangle_coverage_matches_sharp_triangle_without_radius() {
        // Triangle (0,0), (10,0), (0,10) at radius 0: solid inside, empty outside.
        let cov = |x: f32, y: f32, r: f32| rounded_triangle_coverage(x, y, 0.0, 0.0, 10.0, 0.0, 0.0, 10.0, r);
        assert_eq!(cov(3.0, 3.0, 0.0), 1.0);
        assert_eq!(cov(1.0, 7.0, 0.0), 1.0);
        assert_eq!(cov(8.0, 8.0, 0.0), 0.0);
        assert_eq!(cov(-1.0, 5.0, 0.0), 0.0);
        // On an edge: half coverage.
        let edge = cov(0.0, 5.0, 0.0);
        assert!((edge - 0.5).abs() < 0.2, "expected ~0.5 on the edge, got {edge}");
    }

    #[test]
    fn rounded_triangle_coverage_cuts_the_corners() {
        let cov = |x: f32, y: f32, r: f32| rounded_triangle_coverage(x, y, 0.0, 0.0, 10.0, 0.0, 0.0, 10.0, r);
        // The sharp corner (0,0) is cut away by the radius-2 arc.
        assert_eq!(cov(0.0, 0.0, 2.0), 0.0);
        // The core vertex (the arc's center) is fully covered.
        assert_eq!(cov(2.0, 2.0, 2.0), 1.0);
        // Deep interior stays solid.
        assert_eq!(cov(4.0, 4.0, 2.0), 1.0);
        // The arc passes through (2-√2, 2-√2) ≈ (0.586, 0.586): half coverage.
        let arc = cov(0.586, 0.586, 2.0);
        assert!((arc - 0.5).abs() < 0.2, "expected ~0.5 on the arc, got {arc}");
        // The flat edge stays on the original edge: (5,5) lies on x+y=10.
        let flat = cov(5.0, 5.0, 2.0);
        assert!((flat - 0.5).abs() < 0.2, "expected ~0.5 on the flat edge, got {flat}");
    }

    fn fake_display(handle: usize, primary: bool) -> DisplayInfo {
        DisplayInfo {
            handle: HMONITOR(handle as *mut c_void),
            work: RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            primary,
            name: format!(r"\\.\DISPLAY{handle}"),
        }
    }

    #[test]
    fn resolve_target_picks_the_foreground_monitor_for_active_window() {
        let displays = vec![fake_display(1, true), fake_display(2, false)];
        // The foreground window sits on the second display.
        assert_eq!(
            resolve_target(MonitorMode::ActiveWindow, &displays, Some(1)),
            Some(1),
            "ActiveWindow must target the foreground window's monitor"
        );
    }

    #[test]
    fn resolve_target_falls_back_to_primary_without_a_foreground_monitor() {
        let displays = vec![fake_display(1, true), fake_display(2, false)];
        // No foreground window (or its monitor is not in the snapshot).
        assert_eq!(
            resolve_target(MonitorMode::ActiveWindow, &displays, None),
            Some(0),
            "ActiveWindow without a foreground monitor must fall back to primary"
        );
        // A stale foreground index (defensive: the lookup never yields one)
        // must not produce an out-of-bounds pick.
        assert_eq!(
            resolve_target(MonitorMode::ActiveWindow, &displays, Some(9)),
            Some(0),
            "an out-of-range foreground index must fall back to primary"
        );
    }

    #[test]
    fn resolve_target_uses_the_primary_flag_for_primary() {
        // The primary display is not necessarily the first enumerated.
        let displays = vec![fake_display(1, false), fake_display(2, true)];
        assert_eq!(resolve_target(MonitorMode::Primary, &displays, Some(0)), Some(1));
        // No display flagged primary (should not happen): first enumerated wins.
        let unmarked = vec![fake_display(1, false), fake_display(2, false)];
        assert_eq!(resolve_target(MonitorMode::Primary, &unmarked, None), Some(0));
    }

    #[test]
    fn resolve_target_maps_an_index_onto_the_enumeration_order() {
        let displays = vec![fake_display(1, true), fake_display(2, false), fake_display(3, false)];
        assert_eq!(resolve_target(MonitorMode::Index(0), &displays, None), Some(0));
        assert_eq!(resolve_target(MonitorMode::Index(2), &displays, None), Some(2));
        // The foreground monitor is irrelevant to an explicit index.
        assert_eq!(resolve_target(MonitorMode::Index(1), &displays, Some(2)), Some(1));
    }

    #[test]
    fn resolve_target_falls_back_to_primary_for_an_out_of_range_index() {
        let displays = vec![fake_display(1, true), fake_display(2, false)];
        assert_eq!(
            resolve_target(MonitorMode::Index(2), &displays, None),
            Some(0),
            "an unattached index must resolve to primary, never to a missing display"
        );
        assert_eq!(resolve_target(MonitorMode::Index(7), &displays, None), Some(0));
    }

    #[test]
    fn resolve_target_returns_none_without_any_display() {
        assert_eq!(resolve_target(MonitorMode::ActiveWindow, &[], None), None);
        assert_eq!(resolve_target(MonitorMode::Primary, &[], None), None);
        assert_eq!(resolve_target(MonitorMode::Index(0), &[], None), None);
    }

    fn anchor_pos(x: Option<i32>, y: Option<i32>) -> OverlayPos {
        OverlayPos {
            vertical: VerticalPosition::Bottom,
            horizontal: HorizontalPosition::Center,
            margin: 8,
            x,
            y,
            monitor: MonitorMode::ActiveWindow,
        }
    }

    #[test]
    fn placement_anchors_against_the_target_work_area() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040, // 1080 minus a 40px taskbar
        };
        let pos = anchor_pos(None, None);
        // Center anchor with the DIB inset subtracted; margin scaled by DPI.
        let point = placement(work, 400, 100, &pos, 0, 1.0);
        assert_eq!(point, POINT { x: 760, y: 932 }, "centered, 8px from the bottom edge");
        let point = placement(work, 400, 100, &pos, 10, 1.0);
        assert_eq!(
            point,
            POINT { x: 750, y: 922 },
            "the inset shifts the window, not the pill"
        );
        // 150 % DPI scales the margin.
        let point = placement(work, 600, 150, &pos, 10, 1.5);
        assert_eq!(point, POINT { x: 650, y: 868 }, "margin scales with DPI");
        // Left/top anchors.
        let left_top = OverlayPos {
            vertical: VerticalPosition::Top,
            horizontal: HorizontalPosition::Left,
            ..anchor_pos(None, None)
        };
        let point = placement(work, 400, 100, &left_top, 0, 1.0);
        assert_eq!(point, POINT { x: 8, y: 8 });
        // Right anchor.
        let right = OverlayPos {
            vertical: VerticalPosition::Bottom,
            horizontal: HorizontalPosition::Right,
            ..anchor_pos(None, None)
        };
        let point = placement(work, 400, 100, &right, 0, 1.0);
        assert_eq!(point, POINT { x: 1512, y: 932 });
    }

    #[test]
    fn placement_honors_absolute_overrides_and_clamps_them() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let custom = anchor_pos(Some(100), Some(200));
        let point = placement(work, 400, 100, &custom, 0, 1.0);
        assert_eq!(point, POINT { x: 100, y: 200 }, "absolute overrides win over anchors");
        // A far-off override is clamped back into the work area.
        let huge = anchor_pos(Some(10_000), Some(10_000));
        let point = placement(work, 400, 100, &huge, 0, 1.0);
        assert_eq!(
            point,
            POINT { x: 1520, y: 940 },
            "clamped to the work-area bottom-right"
        );
        // Absolute overrides are virtual-screen coordinates: a value beyond
        // the target's work area is clamped back into it (the pill on a
        // monitor left of the origin still lands on that monitor).
        let left_monitor = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1040,
        };
        let point = placement(left_monitor, 400, 100, &custom, 0, 1.0);
        assert_eq!(point, POINT { x: -400, y: 200 }, "clamped to the target work area");
    }
}
