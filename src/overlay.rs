use crate::config::{Config, HorizontalPosition, VerticalPosition};
use crate::events::{MEDIA_EVENT_MSG, MediaEvent, PlaybackState, TOGGLE_MSG, TrackInfo};
use anyhow::{Context, Result};
use image::imageops::FilterType;
use log::{debug, error};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC,
    CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_END_ELLIPSIS, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, ETO_CLIPPED, ExtTextOutW, FF_DONTCARE,
    GetMonitorInfoW, GetTextExtentPoint32W, GetTextMetricsW, HBRUSH, HDC, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow, OUT_DEFAULT_PRECIS, SelectObject, SetBkMode,
    SetTextColor, TEXTMETRICW, TRANSPARENT, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, GWLP_USERDATA, GetForegroundWindow, GetWindowLongPtrW,
    HTTRANSPARENT, HWND_TOPMOST, IDC_ARROW, KillTimer, LoadCursorW, MA_NOACTIVATE, RegisterClassExW, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, ULW_ALPHA, WM_DESTROY, WM_MOUSEACTIVATE, WM_NCCREATE,
    WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASS_STYLES, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

const TIMER_DEBOUNCE: usize = 1;
const TIMER_ANIMATION: usize = 2;
const LIGHT_DURATION: Duration = Duration::from_millis(120);

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
const MARQUEE_SPEED: f32 = 30.0;

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
        }
    }

    fn reset_scroll(&mut self) {
        let now = Instant::now();
        for line in &mut self.scroll {
            line.offset = 0.0;
            line.started_at = Some(now);
        }
    }

    fn receive_events(&mut self) {
        if let Ok(mut queue) = self.queue.lock() {
            while let Some(event) = queue.pop_front() {
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
                    MediaEvent::TrackChanged(_) => {}
                }
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
        if !self.enabled {
            return;
        }
        self.content = Some(event);
        self.reset_scroll();
        let now = Instant::now();
        self.dismiss_at = Some(now + Duration::from_millis(self.config.overlay.duration_ms.max(500)));
        self.phase = if full_animation {
            Phase::Expanding(now)
        } else {
            Phase::Light(now)
        };
        unsafe {
            let _ = SetTimer(self.hwnd, TIMER_ANIMATION, 16, None);
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        self.render();
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.dismiss_at.is_some_and(|deadline| deadline <= now)
            && !matches!(self.phase, Phase::Collapsing(_) | Phase::Hidden)
        {
            self.phase = Phase::Collapsing(now);
        }

        match self.phase {
            Phase::Expanding(start) if start.elapsed() >= animation_duration(&self.config) => {
                self.phase = Phase::Shown;
            }
            Phase::Light(start) if start.elapsed() >= LIGHT_DURATION => {
                self.phase = Phase::Shown;
            }
            Phase::Collapsing(start) if start.elapsed() >= animation_duration(&self.config) => {
                self.hide();
                return;
            }
            _ => {}
        }

        // Advance marquee offsets (driven by this same 16ms tick, entirely
        // independent of the dismiss countdown).
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let per_tick = MARQUEE_SPEED * scale * (0.016);
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
        let Some(content) = self.content.as_ref() else {
            return;
        };
        let (alpha, shape) = self.frame();
        let dpi = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let (logical_width, logical_height) = match content {
            MediaEvent::TrackChanged(_) => track_content_size(&self.config),
            MediaEvent::PlaybackStateChanged(_) => (240.0, 80.0),
        };
        let width = (logical_width * dpi * shape).round().max(1.0) as i32;
        let height = (logical_height * dpi * shape).round().max(1.0) as i32;
        let Some(position) = self.position(width, height) else {
            return;
        };
        if let Err(error) = render_layered(self, content, width, height, dpi * shape, alpha, position) {
            error!("rendering overlay: {error:#}");
        }
    }

    fn frame(&self) -> (u8, f32) {
        match self.phase {
            Phase::Hidden => (0, 0.55),
            Phase::Expanding(start) => {
                let progress = ease_out(start.elapsed().as_secs_f32() / animation_duration(&self.config).as_secs_f32());
                ((progress * 255.0) as u8, 0.55 + progress * 0.45)
            }
            Phase::Light(start) => {
                let progress = ease_out(start.elapsed().as_secs_f32() / LIGHT_DURATION.as_secs_f32());
                ((progress * 255.0) as u8, 1.0)
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
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_ANIMATION);
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
            MediaEvent::TrackChanged(_) => track_content_size(&self.config),
            MediaEvent::PlaybackStateChanged(_) => (240.0, 80.0),
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
        unsafe {
            let _ = SetTimer(self.hwnd, TIMER_ANIMATION, 16, None);
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

/// Logical (96-DPI) size of a track-changed pill, sized for up to four text
/// rows (title, subtitle, meta line, source app). Single source of truth used
/// by both `render()` and `content_size()` so they cannot drift.
fn track_content_size(config: &Config) -> (f32, f32) {
    let appearance = &config.appearance;
    let fs_artist = appearance.font_size_artist;
    let text_h =
        appearance.font_size_title * 1.35 + fs_artist * 1.35 + fs_artist * 0.85 * 1.35 + fs_artist * 0.75 * 1.35;
    let height = (appearance.art_size as f32 + 2.0 * appearance.padding).max(text_h + 2.0 * appearance.padding + 8.0);
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

fn render_layered(
    state: &OverlayState,
    content: &MediaEvent,
    width: i32,
    height: i32,
    scale: f32,
    alpha: u8,
    position: POINT,
) -> Result<()> {
    let pixels = draw_pixels(state, content, width as usize, height as usize, scale)?;
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
    let hdc = unsafe { CreateCompatibleDC(None) };
    if hdc.0.is_null() {
        anyhow::bail!("CreateCompatibleDC failed");
    }
    let mut bits: *mut c_void = null_mut();
    let bitmap = unsafe { CreateDIBSection(hdc, &bitmap_info, DIB_RGB_COLORS, &mut bits, None, 0) }?;
    if bits.is_null() {
        unsafe {
            let _ = DeleteDC(hdc);
        }
        anyhow::bail!("CreateDIBSection returned no pixel buffer");
    }
    unsafe {
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast(), pixels.len());
    }
    let old_bitmap = unsafe { SelectObject(hdc, bitmap) };
    draw_text(state, hdc, content, width, height, scale);

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
        SelectObject(hdc, old_bitmap);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(hdc);
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

fn draw_pixels(state: &OverlayState, content: &MediaEvent, width: usize, height: usize, scale: f32) -> Result<Vec<u8>> {
    let mut pixels = vec![0u8; width * height * 4];
    let radius = state.config.appearance.corner_radius * scale;
    let background = state.config.appearance.background_color;
    for y in 0..height {
        for x in 0..width {
            if inside_round_rect(x as f32, y as f32, width as f32, height as f32, radius) {
                set_pixel(&mut pixels, width, x, y, background);
            }
        }
    }

    match content {
        MediaEvent::TrackChanged(track) => {
            let padding = (state.config.appearance.padding * scale).round() as usize;
            let art_size = (state.config.appearance.art_size as f32 * scale).round() as usize;
            let art_x = padding;
            let art_y = height.saturating_sub(art_size) / 2;
            if let Some(artwork) = &track.artwork {
                if let Some(decoded) = decode_artwork(artwork, art_size) {
                    for y in 0..art_size {
                        for x in 0..art_size {
                            let source = (y * art_size + x) * 4;
                            let rgba = [
                                decoded[source],
                                decoded[source + 1],
                                decoded[source + 2],
                                decoded[source + 3],
                            ];
                            set_pixel(&mut pixels, width, art_x + x, art_y + y, rgba);
                        }
                    }
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
        MediaEvent::PlaybackStateChanged(_) => {
            let accent = state.config.appearance.accent_color;
            let size = (16.0 * scale).round() as usize;
            let x = (12.0 * scale).round() as usize;
            let y = height.saturating_sub(size) / 2;
            draw_placeholder(&mut pixels, width, x, y, size, accent);
        }
    }
    Ok(pixels)
}

fn draw_text(state: &OverlayState, hdc: HDC, content: &MediaEvent, width: i32, height: i32, scale: f32) {
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
    }
    match content {
        MediaEvent::TrackChanged(track) => {
            let appearance = &state.config.appearance;
            let padding = (appearance.padding * scale) as i32;
            let art = (appearance.art_size as f32 * scale) as i32;
            let left = padding + art + (12.0 * scale) as i32;
            let right = width - padding;

            // Font-driven row heights: bands are sized from the actual fonts, so
            // rows can never overlap at any pill size (including mid-animation).
            let fs_title = appearance.font_size_title * scale;
            let fs_artist = appearance.font_size_artist * scale;
            let fs_meta = fs_artist * 0.85;
            let fs_app = fs_artist * 0.75;
            let rows: [(f32, f32); 4] = [
                (fs_title * 1.35, fs_title),
                (fs_artist * 1.35, fs_artist),
                (fs_meta * 1.35, fs_meta),
                (fs_app * 1.35, fs_app),
            ];
            // Only rows that will actually be drawn participate in the band
            // split, so title/artist expand to fill the pill when the meta or
            // source-app line is absent.
            let meta = track.meta_line(true);
            let active: [bool; 4] = [true, true, !meta.is_empty(), !track.source_app.trim().is_empty()];
            let total: f32 = rows
                .iter()
                .zip(active)
                .filter(|(_, active)| *active)
                .map(|((h, _), _)| *h)
                .sum();
            let mut y = 0.0f32;
            let mut next_band = |i: usize| -> RECT {
                let band_h = if active[i] {
                    rows[i].0 / total * height as f32
                } else {
                    0.0
                };
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
            draw_marquee_line(
                hdc,
                &track.title,
                &title_rect,
                rows[0].1 as i32,
                appearance.text_color,
                true,
                &state.scroll[0],
            );

            let subtitle = if track.artist.trim().is_empty() {
                &track.source_app
            } else {
                &track.artist
            };
            let artist_rect = next_band(1);
            draw_marquee_line(
                hdc,
                subtitle,
                &artist_rect,
                rows[1].1 as i32,
                [0xCC, 0xCC, 0xCC, 0xFF],
                false,
                &state.scroll[1],
            );

            if active[2] {
                let meta_rect = next_band(2);
                draw_marquee_line(
                    hdc,
                    &meta,
                    &meta_rect,
                    rows[2].1 as i32,
                    [0x99, 0x99, 0x99, 0xFF],
                    false,
                    &state.scroll[2],
                );
            }
            if active[3] {
                let app_rect = next_band(3);
                draw_marquee_line(
                    hdc,
                    &track.source_app,
                    &app_rect,
                    rows[3].1 as i32,
                    [0x77, 0x77, 0x77, 0xFF],
                    false,
                    &state.scroll[3],
                );
            }
        }
        MediaEvent::PlaybackStateChanged(playback) => {
            let label = match playback {
                PlaybackState::Playing => "Playing",
                PlaybackState::Paused => "Paused",
                PlaybackState::Stopped => "Stopped",
            };
            let mut state_rect = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: (height as f32 * 0.35) as i32,
            };
            draw_string(
                hdc,
                label,
                &mut state_rect,
                (state.config.appearance.font_size_title * scale) as i32,
                state.config.appearance.accent_color,
                true,
                true,
            );
            if let Some(track) = &state.last_track {
                let mut title_rect = RECT {
                    left: 0,
                    top: (height as f32 * 0.35) as i32,
                    right: width,
                    bottom: (height as f32 * 0.65) as i32,
                };
                draw_string(
                    hdc,
                    &track.title,
                    &mut title_rect,
                    (state.config.appearance.font_size_artist * scale) as i32,
                    state.config.appearance.text_color,
                    true,
                    true,
                );
                if !track.artist.trim().is_empty() {
                    let mut artist_rect = RECT {
                        left: 0,
                        top: (height as f32 * 0.65) as i32,
                        right: width,
                        bottom: height,
                    };
                    draw_string(
                        hdc,
                        &track.artist,
                        &mut artist_rect,
                        ((state.config.appearance.font_size_artist * 0.85) as i32).max(1),
                        [0xCC, 0xCC, 0xCC, 0xFF],
                        false,
                        true,
                    );
                }
            }
        }
    }
}

/// Draws one pill text line. Text that fits is drawn statically (left-aligned);
/// overflowing text scrolls horizontally (marquee) using the line's scroll state.
/// Auto-dismiss timing is never involved — scroll is pure per-frame visual state.
fn draw_marquee_line(
    hdc: HDC,
    value: &str,
    rect: &RECT,
    font_height: i32,
    color: [u8; 4],
    bold: bool,
    scroll: &LineScroll,
) {
    let text = value.encode_utf16().collect::<Vec<_>>();
    let font_name = wide("Segoe UI");
    let font = unsafe {
        CreateFontW(
            -font_height.max(1),
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

        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &text, &mut size);
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(hdc, &mut tm);
        let line_h = tm.tmHeight;
        let y = rect.top + ((rect.bottom - rect.top - line_h) / 2).max(0);
        let avail = (rect.right - rect.left).max(1);
        let count = text.len() as u32;

        if size.cx <= avail {
            let _ = ExtTextOutW(
                hdc,
                rect.left,
                y,
                ETO_CLIPPED,
                Some(rect),
                PCWSTR(text.as_ptr()),
                count,
                None,
            );
        } else {
            // Continuous loop: draw the text at a shifting offset plus a second
            // copy after a gap, clipped to the row rect (ETO_CLIPPED).
            let total = size.cx + MARQUEE_GAP as i32;
            let hold_elapsed = scroll.started_at.map(|t| t.elapsed()).unwrap_or_default();
            let offset = if hold_elapsed < MARQUEE_HOLD {
                0.0
            } else {
                scroll.offset
            };
            let off = (offset % total as f32) as i32;
            let x1 = rect.left - off;
            let _ = ExtTextOutW(hdc, x1, y, ETO_CLIPPED, Some(rect), PCWSTR(text.as_ptr()), count, None);
            let x2 = x1 + total;
            if x2 < rect.right {
                let _ = ExtTextOutW(hdc, x2, y, ETO_CLIPPED, Some(rect), PCWSTR(text.as_ptr()), count, None);
            }
        }
        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
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

fn draw_placeholder(pixels: &mut [u8], width: usize, x: usize, y: usize, size: usize, color: [u8; 4]) {
    for py in y..y.saturating_add(size) {
        for px in x..x.saturating_add(size) {
            let center_x = x + size / 2;
            let center_y = y + size / 2;
            let dx = px as isize - center_x as isize;
            let dy = py as isize - center_y as isize;
            if dx * dx + dy * dy <= (size as isize / 2).pow(2) {
                set_pixel(pixels, width, px, py, color);
            }
        }
    }
}

fn set_pixel(pixels: &mut [u8], width: usize, x: usize, y: usize, color: [u8; 4]) {
    if x >= width || y >= pixels.len() / width / 4 {
        return;
    }
    let offset = (y * width + x) * 4;
    let alpha = color[3] as u32;
    pixels[offset] = (color[2] as u32 * alpha / 255) as u8;
    pixels[offset + 1] = (color[1] as u32 * alpha / 255) as u8;
    pixels[offset + 2] = (color[0] as u32 * alpha / 255) as u8;
    pixels[offset + 3] = color[3];
}

fn inside_round_rect(x: f32, y: f32, width: f32, height: f32, radius: f32) -> bool {
    let radius = radius.min(width / 2.0).min(height / 2.0);
    let dx = if x < radius {
        radius - x
    } else if x >= width - radius {
        x - (width - radius)
    } else {
        0.0
    };
    let dy = if y < radius {
        radius - y
    } else if y >= height - radius {
        y - (height - radius)
    } else {
        0.0
    };
    dx == 0.0 || dy == 0.0 || dx * dx + dy * dy <= radius * radius
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
        WM_TIMER if wparam.0 == TIMER_ANIMATION => {
            if !state_ptr.is_null() {
                (*state_ptr).tick();
            }
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
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
    fn track_content_size_fits_four_text_rows() {
        let config = Config::default();
        let (width, height) = track_content_size(&config);
        assert_eq!(width, config.overlay.max_width as f32);
        // Height must clear the sum of the four font-driven row heights plus
        // padding, so no row gets clipped.
        let fs = config.appearance.font_size_artist;
        let text_h = config.appearance.font_size_title * 1.35 + fs * 1.35 + fs * 0.85 * 1.35 + fs * 0.75 * 1.35;
        let needed = text_h + 2.0 * config.appearance.padding + 8.0;
        assert!(height >= needed);
    }
}
