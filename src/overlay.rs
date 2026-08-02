use crate::config::{Config, HorizontalPosition, VerticalPosition};
use crate::events::{MEDIA_EVENT_MSG, MediaEvent, PlaybackState, TOGGLE_MSG, TrackInfo};
use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
use image::imageops::FilterType;
use log::{debug, error};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::BOOLEAN;
use windows::Win32::Foundation::{COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC,
    CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_END_ELLIPSIS, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, FF_DONTCARE, GetMonitorInfoW, HBITMAP, HBRUSH, HDC,
    HGDIOBJ, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow, OUT_DEFAULT_PRECIS,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateTimerQueueTimer, DeleteTimerQueueTimer, WT_EXECUTEDEFAULT};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, GWLP_USERDATA, GetForegroundWindow, GetWindowLongPtrW,
    HTTRANSPARENT, HWND_TOPMOST, IDC_ARROW, IsWindowVisible, KillTimer, LoadCursorW, MA_NOACTIVATE, RegisterClassExW,
    SW_HIDE, SW_SHOWNOACTIVATE, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW, SendMessageW, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, ULW_ALPHA, WM_APP, WM_DESTROY,
    WM_MOUSEACTIVATE, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASS_STYLES, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

const TIMER_DEBOUNCE: usize = 1;
const LIGHT_DURATION: Duration = Duration::from_millis(120);

/// Posted by the high-resolution animation timer to drive pill frames.
const TIMER_ANIMATION_MSG: u32 = WM_APP + 6;
/// Animation timer period in ms. On a 300Hz display a full frame is ~3.3ms;
/// 4ms targets that without flooding slower machines (the effective frame
/// rate self-throttles to the UI thread's render speed).
const ANIM_TICK_MS: u32 = 4;

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
/// to the UI thread; SendMessage blocks the timer thread until the frame is
/// rendered, so the effective frame rate follows the UI thread's speed.
unsafe extern "system" fn animation_timer_proc(parameter: *mut c_void, _fired: BOOLEAN) {
    let hwnd = HWND(parameter);
    unsafe {
        let _ = SendMessageW(hwnd, TIMER_ANIMATION_MSG, WPARAM(0), LPARAM(0));
    }
}
pub(crate) type EventQueue = Arc<Mutex<VecDeque<MediaEvent>>>;

enum Phase {
    Hidden,
    Expanding(Instant),
    Light(Instant),
    Shown,
    Collapsing(Instant),
}

#[derive(Default)]
struct PendingEvents {
    track: Option<TrackInfo>,
    playback: Option<PlaybackState>,
    /// True when the pending track matches the currently shown track (same
    /// title+artist) and only metadata changed — e.g. album/artwork arriving a
    /// moment after the initial notification. Such refreshes update the pill in
    /// place instead of showing a brand-new notification.
    track_update: bool,
}

/// Per-line marquee state for the pill's text rows. The offset advances on the
/// 16ms animation tick; a short hold before the first movement reads better.
#[derive(Default, Clone, Copy)]
struct LineScroll {
    offset: f32,
    started_at: Option<Instant>,
}

/// Hold time before an overflowing line starts scrolling.
const MARQUEE_HOLD: Duration = Duration::from_millis(600);
/// Horizontal gap between the end of the text and its repeated copy.
const MARQUEE_GAP: f32 = 24.0;
/// Scroll speed in logical px per second.
const MARQUEE_SPEED: f32 = 40.0;
/// Band height per text row as a multiple of the row's font size. Matches the
/// font's natural line height (ascent + descent ≈ 1.33x for Segoe UI), so rows
/// pack tightly without clipping.
const ROW_HEIGHT: f32 = 1.35;

/// Regular and bold faces loaded once from the system font directory. Segoe UI
/// is the preferred family, with Tahoma and Arial as fallbacks.
struct FontSet {
    regular: Font,
    bold: Font,
}

static TEXT_FONTS: OnceLock<Result<FontSet, String>> = OnceLock::new();
/// Set once when the first text render failed to load fonts, so the error is
/// logged exactly once instead of every frame.
static FONT_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);

fn load_font(path: &str) -> Option<Font> {
    let bytes = std::fs::read(path).ok()?;
    Font::from_bytes(bytes, FontSettings::default()).ok()
}

fn font_set() -> &'static Result<FontSet, String> {
    TEXT_FONTS.get_or_init(|| {
        const CANDIDATES: [(&str, &str); 3] = [
            (r"C:\Windows\Fonts\segoeui.ttf", r"C:\Windows\Fonts\segoeuib.ttf"),
            (r"C:\Windows\Fonts\tahoma.ttf", r"C:\Windows\Fonts\tahomabd.ttf"),
            (r"C:\Windows\Fonts\arial.ttf", r"C:\Windows\Fonts\arialbd.ttf"),
        ];
        for (regular, bold) in CANDIDATES {
            if let (Some(regular), Some(bold)) = (load_font(regular), load_font(bold)) {
                return Ok(FontSet { regular, bold });
            }
        }
        Err("no usable system font found (tried Segoe UI, Tahoma, Arial)".to_string())
    })
}

struct OverlayState {
    hwnd: HWND,
    config: Config,
    queue: EventQueue,
    pending: PendingEvents,
    enabled: bool,
    content: Option<MediaEvent>,
    last_track: Option<TrackInfo>,
    phase: Phase,
    dismiss_at: Option<Instant>,
    position: OverlayPos,
    /// Per-row marquee state for the four track lines (title/subtitle/meta/app).
    scroll: [LineScroll; 4],
    /// High-resolution timer driving the pill animation.
    anim_timer: HANDLE,
    /// Cached decoded artwork for the current track (RGBA8 at the full art
    /// size), so animation frames never re-decode the JPEG/PNG.
    decoded_art: Option<Vec<u8>>,
    decoded_art_key: Option<(String, String)>,
    /// Cached DIB (DC + bitmap) reused across frames of the same size.
    dib: Option<DibCache>,
    /// Timestamp of the previous animation tick, for time-based marquee
    /// scrolling.
    last_tick: Instant,
}

/// Resolved placement for the notch pill, pulled from [overlay] config. `x`/`y`
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
}

impl OverlayPos {
    pub(crate) fn from_config(config: &Config) -> Self {
        Self {
            vertical: config.overlay.vertical,
            horizontal: config.overlay.horizontal,
            margin: config.overlay.margin,
            x: config.overlay.position_x,
            y: config.overlay.position_y,
        }
    }
}

/// Updates the live overlay's placement from a resolved position.
pub(crate) fn set_position(hwnd: HWND, pos: OverlayPos) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        state.position = pos;
        if matches!(state.phase, Phase::Hidden) {
            state.show_sample();
        } else {
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
        let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        state.config.overlay.duration_ms = duration_ms.clamp(500, 60_000);
    }
}

impl OverlayState {
    fn new(config: Config, queue: EventQueue) -> Self {
        let position = OverlayPos::from_config(&config);
        Self {
            hwnd: HWND::default(),
            config,
            queue,
            pending: PendingEvents::default(),
            enabled: true,
            content: None,
            last_track: None,
            phase: Phase::Hidden,
            dismiss_at: None,
            position,
            scroll: [LineScroll::default(); 4],
            anim_timer: HANDLE::default(),
            decoded_art: None,
            decoded_art_key: None,
            dib: None,
            last_tick: Instant::now(),
        }
    }

    fn reset_scroll(&mut self) {
        let now = Instant::now();
        for line in &mut self.scroll {
            line.offset = 0.0;
            line.started_at = Some(now);
        }
    }

    /// Decodes (once per track) and caches the artwork bitmap at the full art
    /// size, so animation frames never re-decode the JPEG/PNG.
    fn ensure_art(&mut self, track: &TrackInfo, base_size: usize) {
        let key = (track.title.clone(), track.artist.clone());
        if self.decoded_art_key.as_ref() != Some(&key) || self.decoded_art.is_none() {
            self.decoded_art = track.artwork.as_deref().and_then(|a| decode_artwork(a, base_size));
            self.decoded_art_key = Some(key);
        }
    }

    fn ensure_anim_timer(&mut self) {
        if !self.anim_timer.0.is_null() {
            return;
        }
        let mut handle = HANDLE::default();
        unsafe {
            let _ = CreateTimerQueueTimer(
                &mut handle,
                None,
                Some(animation_timer_proc),
                Some(self.hwnd.0 as *const c_void),
                ANIM_TICK_MS,
                ANIM_TICK_MS,
                WT_EXECUTEDEFAULT,
            );
        }
        self.anim_timer = handle;
    }

    fn delete_anim_timer(&mut self) {
        if !self.anim_timer.0.is_null() {
            // Do not wait for the callback (INVALID_HANDLE_VALUE would): the
            // callback blocks in SendMessageW to this very thread, so waiting
            // here deadlocks. The callback is a single SendMessageW and cannot
            // outlive the timer meaningfully; the timer simply stops firing.
            unsafe {
                let _ = DeleteTimerQueueTimer(None, self.anim_timer, None);
            }
            self.anim_timer = HANDLE::default();
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
            if !self.enabled {
                continue;
            }
            match event {
                MediaEvent::TrackChanged(track) if self.config.behavior.enable_track_change => {
                    let is_update = self
                        .last_track
                        .as_ref()
                        .is_some_and(|last| last.title == track.title && last.artist == track.artist);
                    self.pending.track = Some(track);
                    self.pending.track_update = is_update;
                }
                MediaEvent::PlaybackStateChanged(state) => self.pending.playback = Some(state),
                // A state change from a session that is not current: still show
                // the pill so no pause/play from any app is missed (the main
                // window records the source in its history).
                MediaEvent::HistoryPlaybackState(state, _) => self.pending.playback = Some(state),
                MediaEvent::TrackRestarted(track) if self.config.behavior.enable_track_change => {
                    // A restart (Prev/repeat) re-shows the pill briefly.
                    self.show_restart(track);
                }
                MediaEvent::TrackRestarted(_) => {}
                MediaEvent::TrackChanged(_) => {}
            }
        }
        if self.pending.track.is_some() || self.pending.playback.is_some() {
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
    }

    fn flush_pending(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_DEBOUNCE);
        }
        let pending = std::mem::take(&mut self.pending);
        if let Some(track) = pending.track {
            let is_update = pending.track_update;
            self.last_track = Some(track.clone());
            if is_update && self.content.is_some() {
                self.update_content(MediaEvent::TrackChanged(track));
            } else {
                self.show(MediaEvent::TrackChanged(track), true);
            }
        } else if let Some(playback) = pending.playback
            && self.config.behavior.enable_playback_state_change
        {
            self.show(MediaEvent::PlaybackStateChanged(playback), false);
        }
    }

    /// Refreshes the shown content in place (metadata-only change): keeps the
    /// current animation phase, extends the visible time, and re-renders.
    fn update_content(&mut self, event: MediaEvent) {
        self.content = Some(event);
        self.reset_scroll();
        if let Some(deadline) = self.dismiss_at {
            self.dismiss_at = Some(deadline.max(Instant::now() + update_min_duration(&self.config)));
        }
        self.render();
    }

    fn show(&mut self, event: MediaEvent, full_animation: bool) {
        self.show_with_duration(event, full_animation, self.config.overlay.duration_ms.max(500));
    }

    fn show_with_duration(&mut self, event: MediaEvent, full_animation: bool, duration_ms: u64) {
        if !self.enabled {
            return;
        }
        self.content = Some(event);
        self.reset_scroll();
        let now = Instant::now();
        self.dismiss_at = Some(now + Duration::from_millis(duration_ms));
        self.phase = if full_animation {
            Phase::Expanding(now)
        } else {
            Phase::Light(now)
        };
        self.ensure_anim_timer();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        debug!("pill shown | duration_ms={duration_ms}");
        self.render();
    }

    /// Re-shows the pill for a track restart (Prev button, or a looping track)
    /// with the shorter restart duration.
    fn show_restart(&mut self, track: TrackInfo) {
        self.last_track = Some(track.clone());
        let duration = self.config.overlay.restart_duration_ms.clamp(500, 2000);
        self.show_with_duration(MediaEvent::TrackChanged(track), true, duration);
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        // A layered popup can be hidden by fullscreen transitions or external
        // ShowWindow calls; re-assert visibility while a pill should be up.
        if !matches!(self.phase, Phase::Hidden) && !unsafe { IsWindowVisible(self.hwnd) }.as_bool() {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
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
        // speed is identical at any frame rate.
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let per_tick = MARQUEE_SPEED * scale * dt;
        for line in &mut self.scroll {
            if let Some(started) = line.started_at
                && started.elapsed() >= MARQUEE_HOLD
            {
                line.offset += per_tick;
            }
        }
        self.render();
    }

    fn render(&mut self) {
        let Some(content) = self.content.take() else {
            return;
        };
        let (alpha, shape) = self.frame();
        let dpi = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let (logical_width, logical_height) = match &content {
            MediaEvent::TrackChanged(track) | MediaEvent::TrackRestarted(track) => {
                track_content_size(&self.config, track)
            }
            MediaEvent::PlaybackStateChanged(_) | MediaEvent::HistoryPlaybackState(_, _) => {
                state_content_size(&self.config, self.last_track.as_ref())
            }
        };
        let width = (logical_width * dpi * shape).round().max(1.0) as i32;
        let height = (logical_height * dpi * shape).round().max(1.0) as i32;
        let Some(position) = self.position(width, height) else {
            self.content = Some(content);
            return;
        };
        let art_base = (self.config.appearance.art_size as f32 * dpi).round() as usize;
        let result = render_layered(self, &content, width, height, dpi * shape, art_base, alpha, position);
        self.content = Some(content);
        if let Err(error) = result {
            error!("rendering overlay: {error:#}");
        } else {
            debug!(
                "pill rendered | {width}x{height} at ({}, {}) | alpha={alpha}",
                position.x, position.y
            );
        }
    }

    fn frame(&self) -> (u8, f32) {
        // Animation frames start visibly (never alpha 0): the pill must be
        // seen from the first render even if a tick is delayed, and the fade
        // from ~25% reads just as smoothly.
        match self.phase {
            Phase::Hidden => (0, 0.55),
            Phase::Expanding(start) => {
                let progress = ease_out(start.elapsed().as_secs_f32() / animation_duration(&self.config).as_secs_f32());
                ((64.0 + progress * 191.0) as u8, 0.55 + progress * 0.45)
            }
            Phase::Light(start) => {
                let progress = ease_out(start.elapsed().as_secs_f32() / LIGHT_DURATION.as_secs_f32());
                ((64.0 + progress * 191.0) as u8, 1.0)
            }
            Phase::Shown => (255, 1.0),
            Phase::Collapsing(start) => {
                let progress = ease_out(start.elapsed().as_secs_f32() / animation_duration(&self.config).as_secs_f32());
                (((1.0 - progress) * 255.0) as u8, 1.0 - progress * 0.45)
            }
        }
    }

    fn position(&self, width: i32, height: i32) -> Option<POINT> {
        let foreground = unsafe { GetForegroundWindow() };
        let monitor = unsafe {
            let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
            if monitor.0.is_null() {
                MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY)
            } else {
                monitor
            }
        };
        if monitor.0.is_null() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            return None;
        }
        let work = info.rcWork;
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let margin = (self.position.margin as f32 * scale).round() as i32;
        let span_w = work.right - work.left;
        let x = if let Some(px) = self.position.x {
            (px as f32 * scale).round() as i32
        } else {
            match self.position.horizontal {
                HorizontalPosition::Left => work.left + margin,
                HorizontalPosition::Center => work.left + (span_w - width) / 2,
                HorizontalPosition::Right => work.right - width - margin,
            }
        };
        let y = if let Some(py) = self.position.y {
            (py as f32 * scale).round() as i32
        } else {
            match self.position.vertical {
                VerticalPosition::Top => work.top + margin,
                VerticalPosition::Bottom => work.bottom - height - margin,
            }
        };
        // Clamp to the current work area so absolute overrides stay usable after a
        // resolution or monitor change.
        let x = x.clamp(work.left, (work.right - width).max(work.left));
        let y = y.clamp(work.top, (work.bottom - height).max(work.top));
        Some(POINT { x, y })
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
        self.content = None;
        self.dismiss_at = None;
        self.phase = Phase::Hidden;
        self.delete_anim_timer();
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_DEBOUNCE);
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
    }

    fn toggle_enabled(&mut self) {
        self.enabled = !self.enabled;
        if !self.enabled {
            self.pending = PendingEvents::default();
            self.hide();
        }
    }

    /// Current (scaled) pixel size of the shown content, or `None` while hidden.
    fn content_size(&self) -> Option<(i32, i32)> {
        let content = self.content.as_ref()?;
        let (_, shape) = self.frame();
        let dpi = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let (logical_width, logical_height) = match content {
            MediaEvent::TrackChanged(track) | MediaEvent::TrackRestarted(track) => {
                track_content_size(&self.config, track)
            }
            MediaEvent::PlaybackStateChanged(_) | MediaEvent::HistoryPlaybackState(_, _) => {
                state_content_size(&self.config, self.last_track.as_ref())
            }
        };
        let width = (logical_width * dpi * shape).round().max(1.0) as i32;
        let height = (logical_height * dpi * shape).round().max(1.0) as i32;
        Some((width, height))
    }

    /// Shows a short-lived preview of the overlay at its current position, used by
    /// the tray "Show sample" command to preview placement without real media.
    fn show_sample(&mut self) {
        self.content = Some(MediaEvent::PlaybackStateChanged(PlaybackState::Playing));
        self.reset_scroll();
        let now = Instant::now();
        self.dismiss_at = Some(now + sample_duration(&self.config));
        self.phase = Phase::Light(now);
        self.ensure_anim_timer();
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

/// Logical (96-DPI) size of a track-changed pill. The height fits exactly the
/// rows that will actually be drawn (title, subtitle, plus the meta and
/// source-app rows when present), so the text fills the pill instead of
/// floating in expanded bands. Single source of truth used by both `render()`
/// and `content_size()` so they cannot drift.
fn track_content_size(config: &Config, track: &TrackInfo) -> (f32, f32) {
    let appearance = &config.appearance;
    let fs_artist = appearance.font_size_artist;
    let rows: [f32; 4] = [
        appearance.font_size_title * ROW_HEIGHT,
        fs_artist * ROW_HEIGHT,
        fs_artist * 0.85 * ROW_HEIGHT,
        fs_artist * 0.85 * ROW_HEIGHT,
    ];
    let meta = track.meta_line(true);
    let active = [true, true, !meta.is_empty(), !track.source_app.trim().is_empty()];
    let text_h: f32 = rows.iter().zip(active).filter(|(_, a)| *a).map(|(h, _)| *h).sum();
    let height = (appearance.art_size as f32 + 2.0 * appearance.padding).max(text_h + 2.0 * appearance.padding + 8.0);
    (config.overlay.max_width.max(180) as f32, height)
}

/// Logical size of a playback-state pill: the label plus the current track's
/// title/artist rows when one is known, again fitted to the drawn rows.
fn state_content_size(config: &Config, last_track: Option<&TrackInfo>) -> (f32, f32) {
    let appearance = &config.appearance;
    let mut text_h = appearance.font_size_title * ROW_HEIGHT;
    if let Some(track) = last_track {
        text_h += appearance.font_size_artist * ROW_HEIGHT;
        if !track.artist.trim().is_empty() {
            text_h += appearance.font_size_artist * 0.85 * ROW_HEIGHT;
        }
        if !track.meta_line(true).is_empty() {
            text_h += appearance.font_size_artist * 0.85 * ROW_HEIGHT;
        }
        if !track.source_app.trim().is_empty() {
            text_h += appearance.font_size_artist * 0.85 * ROW_HEIGHT;
        }
    }
    let height = text_h + 2.0 * appearance.padding + 8.0;
    (config.overlay.max_width.max(180) as f32, height)
}

/// Forces the live overlay at `hwnd` to preview its current placement.
pub(crate) fn show_sample(hwnd: HWND) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
        if !state_ptr.is_null() {
            (*state_ptr).show_sample();
        }
    }
}

/// Creates the passive notch overlay window. It owns no message loop: the caller
/// runs the loop and destroys the window at exit.
pub(crate) fn create_window(config: Config, queue: EventQueue) -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.context("getting the process module")?;
    let instance: HINSTANCE = module.into();
    let class_name = wide("NotchOverlayWindow");
    register_window_class(instance, &class_name)?;

    let state = Box::new(OverlayState::new(config, queue));
    let state_ptr = Box::into_raw(state);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("Notch").as_ptr()),
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
            unsafe { drop(Box::from_raw(state_ptr)) };
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
    art_base: usize,
    alpha: u8,
    position: POINT,
) -> Result<()> {
    let mut pixels = draw_pixels(state, content, width as usize, height as usize, scale, art_base)?;
    draw_text_pixels(&*state, &mut pixels, content, width, scale);
    let bitmap_info = BITMAPINFO {
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
    let (hdc, _bitmap, bits) = dib_for(state, &bitmap_info, width, height)?;
    unsafe {
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast(), pixels.len());
    }

    let size = SIZE { cx: width, cy: height };
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
        let _ = ShowWindow(state.hwnd, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(
            state.hwnd,
            HWND_TOPMOST,
            position.x,
            position.y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
    result.context("UpdateLayeredWindow")
}

/// Returns the cached DIB for the given size, creating (or replacing) it when
/// the size changed. The DIB stays alive across frames and is released at
/// window destruction.
fn dib_for(
    state: &mut OverlayState,
    info: &BITMAPINFO,
    width: i32,
    height: i32,
) -> Result<(HDC, HBITMAP, *mut c_void)> {
    if let Some(dib) = &state.dib {
        if dib.width == width && dib.height == height {
            return Ok((dib.hdc, dib.bitmap, dib.bits));
        }
        unsafe {
            let _ = SelectObject(dib.hdc, dib.old_bitmap);
            let _ = DeleteObject(dib.bitmap);
            let _ = DeleteDC(dib.hdc);
        }
        state.dib = None;
    }
    let hdc = unsafe { CreateCompatibleDC(None) };
    if hdc.0.is_null() {
        anyhow::bail!("CreateCompatibleDC failed");
    }
    let mut bits: *mut c_void = null_mut();
    let bitmap = unsafe { CreateDIBSection(hdc, info, DIB_RGB_COLORS, &mut bits, None, 0) }?;
    if bits.is_null() {
        unsafe {
            let _ = DeleteDC(hdc);
        }
        anyhow::bail!("CreateDIBSection returned no pixel buffer");
    }
    let old_bitmap = unsafe { SelectObject(hdc, bitmap) };
    state.dib = Some(DibCache {
        hdc,
        bitmap,
        old_bitmap,
        bits,
        width,
        height,
    });
    Ok((hdc, bitmap, bits))
}

fn draw_pixels(
    state: &mut OverlayState,
    content: &MediaEvent,
    width: usize,
    height: usize,
    scale: f32,
    art_base: usize,
) -> Result<Vec<u8>> {
    let mut pixels = vec![0u8; width * height * 4];
    let radius = state.config.appearance.corner_radius * scale;
    let background = state.config.appearance.background_color;
    for y in 0..height {
        for x in 0..width {
            let coverage = round_rect_coverage(x as f32, y as f32, width as f32, height as f32, radius);
            if coverage > 0.0 {
                let alpha = (background[3] as f32 * coverage) as u32;
                composite(
                    &mut pixels,
                    width,
                    x,
                    y,
                    [background[0], background[1], background[2]],
                    alpha,
                );
            }
        }
    }

    match content {
        MediaEvent::TrackChanged(track) | MediaEvent::TrackRestarted(track) => {
            let padding = (state.config.appearance.padding * scale).round() as usize;
            let art_size = (state.config.appearance.art_size as f32 * scale).round() as usize;
            let art_x = padding;
            let art_y = height.saturating_sub(art_size) / 2;
            state.ensure_art(track, art_base);
            if let Some(art) = state.decoded_art.as_deref() {
                draw_art_scaled(
                    &mut pixels,
                    width,
                    art,
                    art_base,
                    art_x,
                    art_y,
                    art_size,
                    state.config.appearance.accent_color,
                );
            } else {
                draw_placeholder(
                    &mut pixels,
                    width,
                    art_x,
                    art_y,
                    art_size,
                    state.config.appearance.accent_color,
                );
            }
        }
        MediaEvent::PlaybackStateChanged(_) | MediaEvent::HistoryPlaybackState(_, _) => {
            // The state pill reuses the current track's artwork and details so
            // a pause/play notification still shows what is playing (the cache
            // was populated when the track was shown; falls back to the accent
            // placeholder when nothing has been shown yet).
            let padding = (state.config.appearance.padding * scale).round() as usize;
            let art_size = (state.config.appearance.art_size as f32 * scale).round() as usize;
            let art_size = art_size.min(height.saturating_sub(2 * padding));
            let art_x = padding;
            let art_y = height.saturating_sub(art_size) / 2;
            if let Some(art) = state.decoded_art.as_deref() {
                draw_art_scaled(
                    &mut pixels,
                    width,
                    art,
                    art_base,
                    art_x,
                    art_y,
                    art_size,
                    state.config.appearance.accent_color,
                );
            } else {
                draw_placeholder(
                    &mut pixels,
                    width,
                    art_x,
                    art_y,
                    art_size,
                    state.config.appearance.accent_color,
                );
            }
        }
    }
    Ok(pixels)
}

/// Draws the cached artwork bitmap into the tile region, bilinear-scaled from
/// the cached base size to the current (animation-scaled) size, with the
/// rounded-corner mask. Falls back to the accent placeholder on decode errors.
#[allow(clippy::too_many_arguments)]
fn draw_art_scaled(
    pixels: &mut [u8],
    width: usize,
    art: &[u8],
    base: usize,
    x: usize,
    y: usize,
    size: usize,
    accent: [u8; 4],
) {
    if size == 0 || base == 0 || art.len() < base * base * 4 {
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

/// Draws the pill's text rows into the same premultiplied pixel buffer as the
/// shapes: glyph coverage from fontdue becomes alpha, so text alpha-composites
/// exactly like every other element (GDI text cannot do this on a layered
/// window — it never touches the alpha channel).
fn draw_text_pixels(state: &OverlayState, pixels: &mut [u8], content: &MediaEvent, width: i32, scale: f32) {
    match content {
        MediaEvent::TrackChanged(track) | MediaEvent::TrackRestarted(track) => {
            let appearance = &state.config.appearance;
            let padding = (appearance.padding * scale) as i32;
            let art = (appearance.art_size as f32 * scale) as i32;
            let left = padding + art + (12.0 * scale) as i32;
            let right = width - padding;

            // Font-driven row heights: bands are sized from the actual fonts, so
            // rows can never overlap at any pill size (including mid-animation)
            // and pack tightly — the pill is fitted to the drawn rows, so each
            // band keeps its natural line height instead of expanding.
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
            // Only rows that will actually be drawn participate, so title/artist
            // expand to fill the pill when the meta or source-app line is absent.
            let meta = track.meta_line(true);
            let active: [bool; 4] = [true, true, !meta.is_empty(), !track.source_app.trim().is_empty()];
            let text_top = appearance.padding * scale;
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

            let title_rect = next_band(0);
            draw_text_line_pixels(
                pixels,
                width as usize,
                &track.title,
                &title_rect,
                rows[0].1 as i32,
                appearance.text_color,
                true,
                false,
                Some(&state.scroll[0]),
            );

            let subtitle = if track.artist.trim().is_empty() {
                "Unknown"
            } else {
                &track.artist
            };
            let artist_rect = next_band(1);
            draw_text_line_pixels(
                pixels,
                width as usize,
                subtitle,
                &artist_rect,
                rows[1].1 as i32,
                [0xCC, 0xCC, 0xCC, 0xFF],
                false,
                false,
                Some(&state.scroll[1]),
            );

            if active[2] {
                let meta_rect = next_band(2);
                draw_text_line_pixels(
                    pixels,
                    width as usize,
                    &meta,
                    &meta_rect,
                    rows[2].1 as i32,
                    [0x99, 0x99, 0x99, 0xFF],
                    false,
                    false,
                    Some(&state.scroll[2]),
                );
            }
            if active[3] {
                let app_rect = next_band(3);
                draw_text_line_pixels(
                    pixels,
                    width as usize,
                    &track.source_app,
                    &app_rect,
                    rows[3].1 as i32,
                    [0x77, 0x77, 0x77, 0xFF],
                    false,
                    false,
                    Some(&state.scroll[3]),
                );
            }
        }
        MediaEvent::PlaybackStateChanged(playback) | MediaEvent::HistoryPlaybackState(playback, _) => {
            let label = match playback {
                PlaybackState::Playing => "Playing",
                PlaybackState::Paused => "Paused",
                PlaybackState::Stopped => "Stopped",
            };
            let appearance = &state.config.appearance;
            let padding = (appearance.padding * scale) as i32;
            let art = (appearance.art_size as f32 * scale) as i32;
            // Text starts after the artwork tile, like the track pill, so a
            // centered long line can never overlap the cover.
            let left = padding + art + (12.0 * scale) as i32;
            let right = width - padding;
            let fs_title = appearance.font_size_title * scale;
            let fs_artist = appearance.font_size_artist * scale;
            let text_top = appearance.padding * scale;
            let mut y = text_top;
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
            let label_rect = next_band(fs_title * ROW_HEIGHT);
            draw_text_line_pixels(
                pixels,
                width as usize,
                label,
                &label_rect,
                fs_title as i32,
                appearance.accent_color,
                true,
                true,
                None,
            );
            if let Some(track) = &state.last_track {
                let title_rect = next_band(fs_artist * ROW_HEIGHT);
                draw_text_line_pixels(
                    pixels,
                    width as usize,
                    &track.title,
                    &title_rect,
                    fs_artist as i32,
                    appearance.text_color,
                    true,
                    true,
                    None,
                );
                if !track.artist.trim().is_empty() {
                    let artist_rect = next_band(fs_artist * 0.85 * ROW_HEIGHT);
                    draw_text_line_pixels(
                        pixels,
                        width as usize,
                        &track.artist,
                        &artist_rect,
                        (fs_artist * 0.85) as i32,
                        [0xCC, 0xCC, 0xCC, 0xFF],
                        false,
                        true,
                        None,
                    );
                }
                let meta = track.meta_line(true);
                if !meta.is_empty() {
                    let meta_rect = next_band(fs_artist * 0.85 * ROW_HEIGHT);
                    draw_text_line_pixels(
                        pixels,
                        width as usize,
                        &meta,
                        &meta_rect,
                        (fs_artist * 0.85) as i32,
                        [0x99, 0x99, 0x99, 0xFF],
                        false,
                        true,
                        None,
                    );
                }
                if !track.source_app.trim().is_empty() {
                    let source_rect = next_band(fs_artist * 0.85 * ROW_HEIGHT);
                    draw_text_line_pixels(
                        pixels,
                        width as usize,
                        &track.source_app,
                        &source_rect,
                        (fs_artist * 0.85) as i32,
                        [0x77, 0x77, 0x77, 0xFF],
                        false,
                        true,
                        None,
                    );
                }
            }
        }
    }
}

/// Rasterizes one pill text line into the pixel buffer. Text that fits is
/// drawn statically (left-aligned); overflowing text scrolls horizontally
/// (marquee) using the line's scroll state; the non-scrolling form trims with
/// an ellipsis like DT_END_ELLIPSIS and honors the centered flag.
/// Auto-dismiss timing is never involved — scroll is pure per-frame visual state.
#[allow(clippy::too_many_arguments)]
fn draw_text_line_pixels(
    pixels: &mut [u8],
    width: usize,
    value: &str,
    rect: &RECT,
    font_height: i32,
    color: [u8; 4],
    bold: bool,
    centered: bool,
    marquee: Option<&LineScroll>,
) {
    if value.is_empty() || rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    let fonts = match font_set() {
        Ok(fonts) => fonts,
        Err(message) => {
            if !FONT_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                error!("text rendering unavailable: {message}");
            }
            return;
        }
    };
    let face = if bold { &fonts.bold } else { &fonts.regular };
    let px = font_height.max(1) as f32;
    let Some(line) = face.horizontal_line_metrics(px) else {
        return;
    };
    let line_h = line.ascent - line.descent;
    let rect_w = (rect.right - rect.left) as f32;
    let rect_h = (rect.bottom - rect.top) as f32;
    let baseline = rect.top as f32 + (rect_h - line_h) / 2.0 + line.ascent;

    let chars: Vec<char> = value.chars().collect();
    let advances: Vec<f32> = chars.iter().map(|c| face.metrics(*c, px).advance_width).collect();
    let total: f32 = advances.iter().sum();

    let mut draw_at = |start_x: f32, chars: &[char], advances: &[f32]| {
        draw_glyphs(pixels, width, rect, face, px, chars, advances, start_x, baseline, color);
    };

    if let Some(scroll) = marquee {
        if total <= rect_w {
            draw_at(rect.left as f32, &chars, &advances);
            return;
        }
        // Continuous loop: draw the text at a shifting offset plus a second
        // copy after a gap, clipped to the row rect.
        let span = total + MARQUEE_GAP;
        let hold_elapsed = scroll.started_at.map(|t| t.elapsed()).unwrap_or_default();
        let offset = if hold_elapsed < MARQUEE_HOLD {
            0.0
        } else {
            scroll.offset % span
        };
        let x1 = rect.left as f32 - offset;
        draw_at(x1, &chars, &advances);
        let x2 = x1 + span;
        if x2 < rect.right as f32 {
            draw_at(x2, &chars, &advances);
        }
        return;
    }

    // Static line: ellipsis-trim when the text does not fit.
    if total > rect_w {
        let ellipsis = '\u{2026}';
        let ellipsis_w = face.metrics(ellipsis, px).advance_width;
        let mut budget = rect_w - ellipsis_w;
        let mut end = 0;
        while end < chars.len() && budget >= advances[end] {
            budget -= advances[end];
            end += 1;
        }
        let mut shown: Vec<char> = chars[..end].to_vec();
        let mut shown_adv: Vec<f32> = advances[..end].to_vec();
        shown.push(ellipsis);
        shown_adv.push(ellipsis_w);
        let drawn: f32 = shown_adv.iter().sum();
        let start_x = if centered {
            rect.left as f32 + (rect_w - drawn) / 2.0
        } else {
            rect.left as f32
        };
        draw_at(start_x, &shown, &shown_adv);
        return;
    }
    let start_x = if centered {
        rect.left as f32 + (rect_w - total) / 2.0
    } else {
        rect.left as f32
    };
    draw_at(start_x, &chars, &advances);
}

/// Rasterizes glyphs with fontdue and composites their coverage as alpha.
#[allow(clippy::too_many_arguments)]
fn draw_glyphs(
    pixels: &mut [u8],
    width: usize,
    rect: &RECT,
    face: &Font,
    px: f32,
    chars: &[char],
    advances: &[f32],
    start_x: f32,
    baseline: f32,
    color: [u8; 4],
) {
    let mut pen = start_x;
    for (c, advance) in chars.iter().zip(advances) {
        let (metrics, coverage) = face.rasterize(*c, px);
        let origin_x = (pen + metrics.xmin as f32).round() as i32;
        // fontdue's ymin is the glyph's bottom edge in y-up coordinates
        // (negative below the baseline), so the bitmap's top row sits at
        // baseline - (ymin + height).
        let origin_y = (baseline - metrics.ymin as f32 - metrics.height as f32).round() as i32;
        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let x = origin_x + gx as i32;
                let y = origin_y + gy as i32;
                if x < rect.left || x >= rect.right || y < rect.top || y >= rect.bottom {
                    continue;
                }
                let coverage = coverage[gy * metrics.width + gx] as u32;
                if coverage == 0 {
                    continue;
                }
                let alpha = color[3] as u32 * coverage / 255;
                composite(
                    pixels,
                    width,
                    x as usize,
                    y as usize,
                    [color[0], color[1], color[2]],
                    alpha,
                );
            }
        }
        pen += advance;
    }
}

pub(crate) fn draw_string(
    hdc: HDC,
    value: &str,
    rect: &mut RECT,
    height: i32,
    color: [u8; 4],
    bold: bool,
    centered: bool,
) {
    // Drawing an empty string is a crash: for an empty Vec<u16> the buffer
    // pointer is the dangling sentinel 0x2, which DrawTextW dereferences.
    if value.is_empty() {
        return;
    }
    let mut text = value.encode_utf16().collect::<Vec<_>>();
    let font_name = wide("Segoe UI");
    let font = unsafe {
        CreateFontW(
            -height.max(1),
            0,
            0,
            0,
            if bold { 600 } else { 400 },
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            // ClearType subpixel rendering is incorrect on layered windows;
            // grayscale antialiasing keeps the pill text crisp.
            ANTIALIASED_QUALITY.0 as u32,
            DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
            PCWSTR(font_name.as_ptr()),
        )
    };
    let old_font = unsafe { SelectObject(hdc, font) };
    let color = COLORREF(color[0] as u32 | (color[1] as u32) << 8 | (color[2] as u32) << 16);
    unsafe {
        SetTextColor(hdc, color);
        SetBkMode(hdc, TRANSPARENT);
        let mut flags = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX;
        if centered {
            flags |= windows::Win32::Graphics::Gdi::DT_CENTER;
        }
        let _ = DrawTextW(hdc, &mut text, rect, flags);
        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

pub(crate) fn decode_artwork(data: &[u8], size: usize) -> Option<Vec<u8>> {
    let image = image::load_from_memory(data).ok()?.to_rgba8();
    let image = image::imageops::resize(&image, size as u32, size as u32, FilterType::Triangle);
    Some(image.into_raw())
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

/// Anti-aliased coverage (0..=1) of a rounded rectangle at pixel (x, y):
/// signed distance to the boundary smoothed over ~1.5 px. Used for the pill's
/// outer shape, the placeholder art and the album-artwork corner mask.
fn round_rect_coverage(x: f32, y: f32, width: f32, height: f32, radius: f32) -> f32 {
    let radius = radius.min(width / 2.0).min(height / 2.0);
    let qx = ((x + 0.5) - width / 2.0).abs() - (width / 2.0 - radius);
    let qy = ((y + 0.5) - height / 2.0).abs() - (height / 2.0 - radius);
    let dist = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
    let t = (0.5 - dist / 1.5).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Anti-aliased coverage of a circle of the given pixel size, sampled at the
/// pixel at (x, y) relative to the circle's top-left corner.
fn circle_coverage(x: f32, y: f32, size: f32) -> f32 {
    let radius = size / 2.0;
    let dist = (x + 0.5 - radius).hypot(y + 0.5 - radius) - radius;
    let t = (0.5 - dist / 1.5).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
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
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: Default::default(),
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        anyhow::bail!("RegisterClassExW failed");
    }
    Ok(())
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            let state = (*create).lpCreateParams as *mut OverlayState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            (*state).hwnd = hwnd;
        }
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
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
        TIMER_ANIMATION_MSG => {
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
                if let Some(dib) = state.dib.take() {
                    unsafe {
                        let _ = SelectObject(dib.hdc, dib.old_bitmap);
                        let _ = DeleteObject(dib.bitmap);
                        let _ = DeleteDC(dib.hdc);
                    }
                }
                drop(Box::from_raw(state_ptr));
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn animation_duration(config: &Config) -> Duration {
    Duration::from_millis(config.overlay.animation_ms.clamp(100, 500))
}

fn ease_out(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    1.0 - (1.0 - value).powi(3)
}

pub(crate) fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn track_content_size_fits_its_active_rows() {
        let config = Config::default();
        let track = TrackInfo {
            title: "Title".into(),
            artist: "Artist".into(),
            source_app: "App".into(),
            ..TrackInfo::default()
        };
        let (width, height) = track_content_size(&config, &track);
        assert_eq!(width, config.overlay.max_width as f32);
        // Height must clear the sum of the active row heights plus padding,
        // so no row gets clipped.
        let fs = config.appearance.font_size_artist;
        let meta = track.meta_line(true);
        let text_h = if meta.is_empty() {
            config.appearance.font_size_title * ROW_HEIGHT + fs * ROW_HEIGHT
        } else {
            config.appearance.font_size_title * ROW_HEIGHT + fs * ROW_HEIGHT + fs * 0.85 * ROW_HEIGHT
        } + fs * 0.85 * ROW_HEIGHT;
        let needed = text_h + 2.0 * config.appearance.padding + 8.0;
        assert!(height >= needed);
        // Without meta/source rows the pill is shorter, not bloated.
        let minimal = TrackInfo {
            title: "Title".into(),
            artist: "Artist".into(),
            ..TrackInfo::default()
        };
        let (_, compact) = track_content_size(&config, &minimal);
        assert!(compact < height, "fewer rows must yield a shorter pill");
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
    fn circle_coverage_is_smooth() {
        assert_eq!(circle_coverage(16.0, 16.0, 32.0), 1.0);
        assert_eq!(circle_coverage(0.0, 0.0, 32.0), 0.0);
        // On the circle boundary (radius from the center): half coverage.
        let edge = circle_coverage(4.2, 4.2, 32.0);
        assert!((edge - 0.5).abs() < 0.2, "expected ~0.5 on the boundary, got {edge}");
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
        draw_text_line_pixels(
            &mut pixels,
            200,
            "Hello World",
            &rect,
            12,
            [255, 255, 255, 255],
            false,
            false,
            None,
        );
        let lit = pixels.chunks(4).filter(|p| p[3] > 0).count();
        assert!(lit > 100, "expected glyph pixels in the buffer, got {lit}");
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
        draw_text_line_pixels(
            &mut pixels,
            200,
            "Hello World",
            &rect,
            12,
            [255, 255, 255, 255],
            false,
            false,
            Some(&LineScroll::default()),
        );
        let lit = pixels.chunks(4).filter(|p| p[3] > 0).count();
        assert!(lit > 100, "expected glyph pixels with marquee state, got {lit}");
    }

    #[test]
    fn track_pill_renders_text() {
        let mut pixels = vec![0u8; 240 * 76 * 4];
        let config = Config::default();
        let state = OverlayState::new(config, EventQueue::default());
        let track = TrackInfo {
            title: "Everything, Everywhere".into(),
            artist: "John Muirhead".into(),
            ..TrackInfo::default()
        };
        draw_text_pixels(&state, &mut pixels, &MediaEvent::TrackChanged(track), 240, 1.0);
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
        draw_text_line_pixels(
            &mut pixels,
            200,
            "Hello",
            &rect,
            12,
            [255, 255, 255, 255],
            false,
            false,
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
}
