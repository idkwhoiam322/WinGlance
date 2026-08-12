//! Display enumeration, target resolution and fullscreen detection.

use super::OverlayPos;
use crate::config::{Config, HorizontalPosition, LayoutMode, MonitorMode, VerticalPosition};
use log::{debug, warn};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, HDC, HMONITOR,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetClassNameW, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, IsIconic, IsWindowVisible,
    MONITORINFOF_PRIMARY, WS_EX_TOOLWINDOW,
};
use windows::core::PCWSTR;

/// Samples the monitor's current refresh period in ms, so the animation timer
/// can tick once per presented frame on any display (60 Hz → 16 ms, 120 Hz → 8 ms,
/// 144 Hz → 7 ms, 240 Hz → 4 ms). The pill can target a display other than the
/// foreground window's (see `MonitorMode`), so when a target was resolved the
/// DWM query runs against the overlay window itself: DWM reports the compose
/// rate of the monitor the queried window is on, and while animating the
/// overlay sits on the target display. Prefers DWM's live compose rate, which
/// stays correct on variable-refresh-rate monitors; falls back to the display
/// mode's nominal frequency of the target (resolved by device name, so no
/// window is needed); last resort is 16 ms (60 Hz).
///
/// This returns the raw monitor period only. `sync_anim_timer` caps it to
/// `config.overlay.max_tick_hz` (default 60 Hz) before arming the timer.
pub(super) fn refresh_period_ms(target: Option<&TargetMonitor>, overlay_hwnd: HWND) -> u32 {
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
pub(super) fn monitor_frequency_ms(monitor: HMONITOR) -> Option<u32> {
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
    /// The display's physical bounds (`rcMonitor`) in virtual screen
    /// coordinates. Used when a genuine fullscreen foreground window occupies
    /// this monitor: the work area is collapsed to the full monitor so the
    /// pill does not keep a stale work-area (taskbar) gap.
    pub monitor: RECT,
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
            monitor: info.monitorInfo.rcMonitor,
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
pub(super) fn foreground_monitor_index(displays: &[DisplayInfo]) -> Option<usize> {
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
pub(super) const INDEX_WARN_INTERVAL: Duration = Duration::from_secs(10);
pub(super) static LAST_INDEX_WARN: Mutex<Option<(u32, Instant)>> = Mutex::new(None);

pub(super) fn warn_index_fallback(index: u32) {
    // Poison-tolerant: a panicking holder must not take down the pill thread
    // just to suppress a log throttle; losing the throttle at worst floods
    // the log at INDEX_WARN_INTERVAL cadence.
    let mut last = LAST_INDEX_WARN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
pub(super) const TARGET_LOG_INTERVAL: Duration = Duration::from_secs(5);
pub(super) static LAST_TARGET_LOG: Mutex<Option<(usize, Instant)>> = Mutex::new(None);

pub(super) fn log_target_once(target: &TargetMonitor, name: &str) {
    // Poison-tolerant for the same reason as warn_index_fallback: the
    // throttle guards log volume only, and must not abort the pill thread
    // when a panicking holder poisoned the mutex.
    let mut last = LAST_TARGET_LOG.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
    /// The display's physical bounds (`rcMonitor`) in virtual screen
    /// coordinates; the fallback edge rectangle when a fullscreen foreground
    /// window occupies this monitor.
    pub monitor: RECT,
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
pub(super) fn placement(work: RECT, width: i32, height: i32, position: &OverlayPos, inset: i32, scale: f32) -> POINT {
    let margin = (position.margin as f32 * scale).round() as i32;
    let span_w = work.right - work.left;
    // The DIB is inflated by `aura_inset` on each side, but the PILL (not the
    // window) must be centered/anchored. Subtract the inset so the pill lands
    // where the user expects it.
    let x = if let Some(px) = position.x {
        (px as f32 * scale).round() as i32
    } else {
        match position.horizontal {
            HorizontalPosition::Left => work.left + margin - inset,
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
            VerticalPosition::Top => work.top + margin - inset,
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
pub(super) struct ForegroundVerdict {
    /// The foreground window's executable name (with extension, as the
    /// process table reports it), when it could be read.
    pub(super) exe: Option<String>,
    /// Whether the foreground window is a fullscreen app covering its
    /// monitor's entire screen.
    pub(super) fullscreen: bool,
}

/// Whether a foreground window counts as a fullscreen app for Auto layout.
/// Conservative on purpose: the window must be visible, not minimized, not a
/// tool window, not a desktop/shell/taskbar surface, not this overlay's own
/// window, and its window rect must cover the entire monitor — not merely
/// the work area, so a maximized window stays Expanded. Anything ambiguous
/// resolves to `false` (Expanded). Tests the window against its *own* monitor's
/// `rcMonitor` (the historical behavior), preserving Auto-Compact unchanged.
pub(super) fn window_is_fullscreen(hwnd: HWND, overlay: HWND) -> bool {
    let Some(rect) = fullscreen_candidate_rect(hwnd, overlay) else {
        return false;
    };
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.0.is_null() {
        return false;
    }
    let Some(rc) = monitor_rc_monitor(monitor) else {
        return false;
    };
    rect_covers_monitor(&rect, &rc)
}

/// Returns the window's rect when it clears the fullscreen-candidate guards
/// (non-null, not the overlay, visible, not minimized, not a shell or transient
/// tool surface); `None` otherwise. Shared by `window_is_fullscreen` and the
/// work-area positioning check so the guard set is never duplicated.
pub(super) fn fullscreen_candidate_rect(hwnd: HWND, overlay: HWND) -> Option<RECT> {
    if hwnd.0.is_null() || hwnd == overlay {
        return None;
    }
    unsafe {
        // A visible, non-minimized window is a prerequisite; an iconized or
        // hidden surface cannot be a fullscreen app.
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
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
                return None;
            }
        }
        // Transient tool windows (flyouts, popups) never count as fullscreen.
        if (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW.0 != 0 {
            return None;
        }
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        Some(rect)
    }
}

/// Tolerance shared by the fullscreen geometry checks: a window within this many
/// physical pixels of the monitor edge counts as covering it.
pub(super) const FULLSCREEN_TOLERANCE: i32 = 2;

/// Pure: does `window_rect` cover `monitor_rc_monitor` within
/// `FULLSCREEN_TOLERANCE`? A maximized window reaches only `rcWork` (the area
/// inside the taskbar band) and so never covers `rcMonitor`; only a genuine
/// fullscreen window does.
pub(super) fn rect_covers_monitor(window_rect: &RECT, monitor_rc_monitor: &RECT) -> bool {
    window_rect.left <= monitor_rc_monitor.left + FULLSCREEN_TOLERANCE
        && window_rect.top <= monitor_rc_monitor.top + FULLSCREEN_TOLERANCE
        && window_rect.right >= monitor_rc_monitor.right - FULLSCREEN_TOLERANCE
        && window_rect.bottom >= monitor_rc_monitor.bottom - FULLSCREEN_TOLERANCE
}

/// The physical (`rcMonitor`) rect of `monitor`; `None` when it cannot be read.
pub(super) fn monitor_rc_monitor(monitor: HMONITOR) -> Option<RECT> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    Some(info.rcMonitor)
}

/// Whether a genuine fullscreen foreground window occupies the *selected target
/// monitor* — i.e. its rect covers the target monitor's `rcMonitor`. A window
/// fullscreen on a different display does not override the target's work area.
/// Uses the target's already-resolved `rcMonitor`, so no extra `GetMonitorInfoW`.
pub(super) fn foreground_fullscreens_target(target: &TargetMonitor, overlay: HWND) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    let Some(rect) = fullscreen_candidate_rect(foreground, overlay) else {
        return false;
    };
    rect_covers_monitor(&rect, &target.monitor)
}

/// The rectangle `placement` anchors against for the target monitor. A genuine
/// fullscreen foreground window on this monitor collapses the work area to the
/// full `rcMonitor` (no work-area inset to respect); otherwise the
/// taskbar-/app-bar-aware `rcWork` is used. Pure given its inputs, so the
/// fullscreen-vs-work-area decision is unit-testable.
pub(super) fn effective_position_rect(monitor_rc: RECT, work_rc: RECT, fullscreen: bool) -> RECT {
    if fullscreen { monitor_rc } else { work_rc }
}

/// Pure: did a foreground switch leave the pill's anchor and layout unchanged?
/// Given the last resolved anchor (`None` before the first resolve), the
/// freshly re-resolved anchor for the selected monitor, and whether the Auto
/// layout flipped — returns true when the caller can skip the reposition/render.
/// The `layout_flipped` escape hatch means a Compact<->Expanded toggle through
/// the same anchor still re-renders, since the pill's size and contents changed
/// even though the anchor did not. Kept pure so the §11 "skip when nothing
/// changed" rule is unit-testable without Win32.
pub(super) fn anchor_unchanged(last: Option<RECT>, edge: RECT, layout_flipped: bool) -> bool {
    match last {
        Some(prev) => prev == edge && !layout_flipped,
        None => false,
    }
}

/// Whether the foreground app's executable name matches the Auto-compact
/// source list. Mirrors the `media_sources` matching convention (normalized
/// case-insensitive substring; word-boundary characters stripped), with the
/// process-picker's `.exe`-stripping applied to the name. Unlike
/// `media_sources`, an empty list allows nothing: Auto-compact is opt-in
/// per app, so an unlisted foreground never compacts.
pub(super) fn auto_source_matches(config: &Config, exe_name: Option<&str>) -> bool {
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
pub(super) fn decide_layout(config: &Config, verdict: &ForegroundVerdict) -> LayoutMode {
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
