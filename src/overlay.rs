use crate::config::{Config, HorizontalPosition, VerticalPosition};
use crate::events::{MEDIA_EVENT_MSG, MediaEvent, PlaybackState, TOGGLE_MSG, TrackInfo};
use anyhow::{Context, Result};
use image::imageops::FilterType;
use log::{debug, error};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::BOOLEAN;
use windows::Win32::Foundation::{COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CALCRECT,
    DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, ETO_CLIPPED,
    ExtTextOutW, FF_DONTCARE, GetMonitorInfoW, GetTextMetricsW, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow, OUT_DEFAULT_PRECIS,
    SelectObject, SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateTimerQueueTimer, DeleteTimerQueueTimer, WT_EXECUTEDEFAULT};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, GWLP_USERDATA, GetForegroundWindow, GetWindowLongPtrW,
    HTTRANSPARENT, HWND_TOPMOST, IDC_ARROW, IsWindowVisible, KillTimer, LoadCursorW, MA_NOACTIVATE, RegisterClassExW,
    SW_HIDE, SW_SHOWNOACTIVATE, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SendMessageW, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, ULW_ALPHA,
    WM_APP, WM_DESTROY, WM_MOUSEACTIVATE, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASS_STYLES,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
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

/// Upper bound on the pending notification queue. At the cap the oldest
/// unshown queued event is dropped in favor of the incoming one; the pill
/// currently on screen is never pulled. Four distinct real notifications
/// colliding within milliseconds is already an edge case, so the cap is not
/// worth tuning.
const PENDING_CAP: usize = 4;

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

struct OverlayState {
    hwnd: HWND,
    config: Config,
    queue: EventQueue,
    /// Notifications waiting to be shown, in arrival order. Distinct events
    /// from different sources show one after another instead of clobbering
    /// each other; the pill on screen is never replaced early.
    pending: VecDeque<MediaEvent>,
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
    /// Last time the topmost z-order was re-asserted. While the pill is fully
    /// shown (static), the re-assert is throttled to 1 Hz instead of running
    /// on every 4 ms tick.
    last_reassert: Option<Instant>,
    /// Source app of the last TrackChanged shown, used as the label fallback
    /// in state pills for current-session playback states so the pill always
    /// names the app that owns the media — never another app's last track.
    current_source: Option<String>,
    /// Per-source track cache: the last TrackChanged shown for each source app,
    /// so that a later PlaybackStateChanged for that source can render the
    /// correct track info instead of the most-recently-shown app's track.
    track_cache: HashMap<String, TrackInfo>,
    /// Scratch DC + DIB for GDI text rendering (cached across frames).
    text_scratch: Option<TextScratch>,
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
            pending: VecDeque::new(),
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
            last_reassert: None,
            current_source: None,
            track_cache: HashMap::new(),
            text_scratch: None,
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
                    // A metadata refresh for the track currently on screen
                    // (SMTC fills artwork/album progressively, a moment after
                    // the title) updates the pill in place instead of queueing
                    // a second notification for the same song. Cross-source
                    // matches do not refresh in place: both sources notify
                    // independently.
                    let is_update = self.content.as_ref().is_some_and(|content| {
                        matches!(content, MediaEvent::TrackChanged(shown)
                            if shown.title == track.title
                                && shown.artist == track.artist
                                && shown.source_app == track.source_app)
                    });
                    if is_update {
                        self.current_source = Some(track.source_app.clone());
                        self.last_track = Some(track.clone());
                        self.track_cache.insert(track.source_app.clone(), track.clone());
                        self.update_content(MediaEvent::TrackChanged(track));
                    } else {
                        self.enqueue(MediaEvent::TrackChanged(track));
                    }
                }
                MediaEvent::PlaybackStateChanged(state, source_app)
                    if self.config.behavior.enable_playback_state_change =>
                {
                    // Suppress a PlaybackStateChanged pill when:
                    //  - It is Playing AND the same source's TrackChanged was
                    //    recently shown (prevents the "replaying" pill after
                    //    session recreation, or when a browser video triggers
                    //    YTM to re-report "Playing").
                    //  - A TrackChanged for the same source is queued (a
                    //    TrackChanged pill is about to show; a redundant
                    //    PlaybackStateChanged would flash the same info).
                    // Paused/Stopped pass through when they are a new state
                    // from a source that is NOT currently shown.
                    let is_redundant = (matches!(state, PlaybackState::Playing)
                        && self.current_source.as_deref() == Some(source_app.as_str()))
                        || self
                            .pending
                            .iter()
                            .any(|e| matches!(e, MediaEvent::TrackChanged(t) if t.source_app == source_app));
                    if is_redundant {
                        debug!(
                            "playback state pill suppressed | reason=track shown for same source | source={source_app}"
                        );
                        continue;
                    }
                    let event = MediaEvent::PlaybackStateChanged(state, source_app);
                    self.enqueue(event);
                }
                MediaEvent::TrackChanged(_) | MediaEvent::PlaybackStateChanged(_, _) => {}
                // Rejected sessions are history-only: never shown as a pill.
                MediaEvent::SessionRejected { .. } => {}
            }
        }
        if !self.pending.is_empty() {
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

    /// Adds a notification to the pending queue. At the cap, the oldest unshown
    /// queued event is dropped in favor of the incoming one; the pill currently
    /// on screen is never pulled. A metadata refresh for a track already
    /// waiting in the queue replaces it instead of showing the song twice.
    fn enqueue(&mut self, event: MediaEvent) {
        // A metadata refresh for a track already waiting in the queue (artwork
        // or album arriving late) merges into that entry instead of queueing a
        // duplicate. Checking only the back of the queue is not enough: other
        // sources' events can interleave between the track and its refresh.
        if let MediaEvent::TrackChanged(incoming) = &event {
            for queued in self.pending.iter_mut() {
                if let MediaEvent::TrackChanged(queued) = queued
                    && queued.title == incoming.title
                    && queued.artist == incoming.artist
                    && queued.source_app == incoming.source_app
                {
                    if !incoming.album.trim().is_empty() {
                        queued.album = incoming.album.clone();
                    }
                    if incoming.artwork.is_some() {
                        queued.artwork = incoming.artwork.clone();
                    }
                    return;
                }
            }
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
                self.track_cache.insert(track.source_app.clone(), track.clone());
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

    /// Refreshes the shown content in place (metadata-only change): keeps the
    /// current animation phase, extends the visible time, and re-renders.
    fn update_content(&mut self, event: MediaEvent) {
        self.content = Some(event);
        self.reset_scroll();
        if let Some(deadline) = self.dismiss_at {
            self.dismiss_at = Some(deadline.max(Instant::now() + update_min_duration(&self.config)));
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
        // Log the foreground window so we can tell what was in front when the
        // pill fired (an exclusive-fullscreen game hides topmost windows).
        let foreground = unsafe { GetForegroundWindow() };
        debug!(
            "pill shown | duration_ms={duration_ms} | fg=0x{:x}",
            foreground.0 as usize
        );
        self.render();
    }

    fn tick(&mut self) {
        let now = Instant::now();
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
        // A fully-shown pill is static unless a marquee line is actually
        // overflowing: skip the render (and its UpdateLayeredWindow) entirely
        // when nothing changed. The animation phases still repaint every tick.
        let marquee_active = self.scroll.iter().any(|line| line.scrolling);
        if animating || marquee_active {
            self.render();
        }
    }

    fn render(&mut self) {
        let Some(content) = self.content.take() else {
            return;
        };
        let (alpha, shape) = self.frame();
        let dpi = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let (logical_width, logical_height) = match &content {
            MediaEvent::TrackChanged(track) => track_content_size(&self.config, track),
            MediaEvent::PlaybackStateChanged(_, _) => state_content_size(&self.config, self.last_track.as_ref()),
            // Never shown (receive_events skips it); the .max(1.0) guards
            // below keep the size sane if this dead arm is ever reached.
            MediaEvent::SessionRejected { .. } => (0.0, 0.0),
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
        // Advance the queue: the next pending notification shows as a fresh
        // pill. show() checks `enabled`, so a toggle-off collapse stays hidden.
        self.show_next();
    }

    fn toggle_enabled(&mut self) {
        self.enabled = !self.enabled;
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
        let (logical_width, logical_height) = match content {
            MediaEvent::TrackChanged(track) => track_content_size(&self.config, track),
            MediaEvent::PlaybackStateChanged(_, _) => state_content_size(&self.config, self.last_track.as_ref()),
            MediaEvent::SessionRejected { .. } => (0.0, 0.0),
        };
        let width = (logical_width * dpi * shape).round().max(1.0) as i32;
        let height = (logical_height * dpi * shape).round().max(1.0) as i32;
        Some((width, height))
    }

    /// Shows a short-lived preview of the overlay at its current position, used by
    /// the tray "Show sample" command to preview placement without real media.
    /// Uses the track-change pill with sample data so the preview exercises the
    /// exact render path real notifications use (an empty-source state pill would
    /// fall through to the fallback branch and look unlike any real pill).
    fn show_sample(&mut self) {
        let track = TrackInfo {
            title: "Sample Track".into(),
            artist: "Sample Artist".into(),
            album: "Sample Album".into(),
            source_app: "Example Player".into(),
            duration_secs: Some(3 * 60 + 45),
            ..Default::default()
        };
        self.content = Some(MediaEvent::TrackChanged(track));
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
    let active = [
        true,
        !track.artist.trim().is_empty(),
        !meta.is_empty(),
        !track.source_app.trim().is_empty(),
    ];
    let text_h: f32 = rows.iter().zip(active).filter(|(_, a)| *a).map(|(h, _)| *h).sum();
    let height = (appearance.art_size as f32 + 2.0 * appearance.padding).max(text_h + 2.0 * appearance.padding + 8.0);
    (config.overlay.max_width.max(180) as f32, height)
}

/// Logical size of a playback-state pill: the label plus the current track's
/// title/artist rows when one is known, again fitted to the drawn rows.
fn state_content_size(config: &Config, last_track: Option<&TrackInfo>) -> (f32, f32) {
    let appearance = &config.appearance;
    // The title row is always present (its right side holds the ▶/‖/■ symbol);
    // artist, meta and source rows are conditional, matching the TrackChanged
    // pill's row structure.
    let mut text_h = appearance.font_size_title * ROW_HEIGHT;
    if let Some(track) = last_track {
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
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
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
    // Draw straight into the cached DIB: no per-frame pixel Vec allocation and
    // no copy. The DIB is zeroed first so the transparent corners of the
    // rounded pill do not accumulate stale pixels from the previous frame.
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
    let pixel_count = width as usize * height as usize * 4;
    unsafe {
        std::ptr::write_bytes(bits.cast::<u8>(), 0, pixel_count);
    }
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), pixel_count) };
    draw_pixels(state, pixels, content, width as usize, height as usize, scale, art_base)?;
    draw_text_pixels(state, pixels, content, width, scale);

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
    pixels: &mut [u8],
    content: &MediaEvent,
    width: usize,
    height: usize,
    scale: f32,
    art_base: usize,
) -> Result<()> {
    let radius = state.config.appearance.corner_radius * scale;
    let background = state.config.appearance.background_color;
    for y in 0..height {
        for x in 0..width {
            let coverage = round_rect_coverage(x as f32, y as f32, width as f32, height as f32, radius);
            if coverage > 0.0 {
                let alpha = (background[3] as f32 * coverage) as u32;
                composite(
                    pixels,
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
        MediaEvent::TrackChanged(track) => {
            let padding = (state.config.appearance.padding * scale).round() as usize;
            let art_size = (state.config.appearance.art_size as f32 * scale).round() as usize;
            let art_x = padding;
            let art_y = height.saturating_sub(art_size) / 2;
            state.ensure_art(track, art_base);
            if let Some(art) = state.decoded_art.as_deref() {
                draw_art_scaled(
                    pixels,
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
                    pixels,
                    width,
                    art_x,
                    art_y,
                    art_size,
                    state.config.appearance.accent_color,
                );
            }
        }
        MediaEvent::PlaybackStateChanged(_, source_app) => {
            // State pills reuse the cached track's artwork for the source that
            // produced the state change, so a pause/play pill still shows the
            // right cover. Falls back to the accent placeholder when nothing
            // has been cached for this source yet.
            if !source_app.is_empty()
                && let Some(track) = state.track_cache.get(source_app).cloned()
            {
                state.ensure_art(&track, art_base);
            }
            let padding = (state.config.appearance.padding * scale).round() as usize;
            let art_size = (state.config.appearance.art_size as f32 * scale).round() as usize;
            let art_size = art_size.min(height.saturating_sub(2 * padding));
            let art_x = padding;
            let art_y = height.saturating_sub(art_size) / 2;
            if let Some(art) = state.decoded_art.as_deref() {
                draw_art_scaled(
                    pixels,
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
                    pixels,
                    width,
                    art_x,
                    art_y,
                    art_size,
                    state.config.appearance.accent_color,
                );
            }
        }
        // Never rendered: SessionRejected is filtered out before enqueue.
        MediaEvent::SessionRejected { .. } => {}
    }
    Ok(())
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

/// Draws a playback-state symbol (play ▶ / pause ‖ / stop ■) as custom
/// anti-aliased vector shapes directly into the pixel buffer, replacing the
/// old GDI text glyphs. The symbol box is `size`×`size` pixels (size = font
/// height); bars are 0.20×S wide × 0.62×S tall with a 0.22×S gap. Play is a
/// triangle of the same height whose corners are rounded at the pause bars'
/// radius; pause and stop use rounded corners with radius 0.2×S
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
    let bar_h = 0.62 * size;
    let gap = 0.22 * size;
    let radius = (0.20 * size).min(bar_w / 2.0).max(0.0);
    let box_left = (right as f32 - size).round() as i32;
    let v_center = y as f32 + (bar_h / 2.0);
    match playback {
        PlaybackState::Playing => {
            // Triangle, height ≈ bar_h, point on the left. The width matches
            // the pause symbol's total width (bars + gap ≈ 0.62×S) so the
            // play glyph carries the same visual weight as the pause bars;
            // its corners use the pause bars' rounding radius.
            let tri_w = 0.50 * size;
            let tri_h = bar_h;
            let left = box_left as f32 + (size - tri_w) * 0.5;
            let top = v_center - tri_h / 2.0;
            draw_triangle_filled(
                pixels,
                width,
                (left as i32, top as i32),
                (left as i32 + tri_w as i32, (top + tri_h / 2.0) as i32),
                (left as i32, (top + tri_h) as i32),
                radius,
                color,
            );
        }
        PlaybackState::Paused => {
            // Two rounded bars, centered horizontally in the box.
            let total = bar_w * 2.0 + gap;
            let origin = box_left as f32 + (size - total) * 0.5;
            for offset in [0.0, bar_w + gap] {
                draw_rounded_rect_filled(
                    pixels,
                    width,
                    (origin + offset) as i32,
                    (v_center - bar_h / 2.0) as i32,
                    bar_w as i32,
                    bar_h as i32,
                    radius,
                    color,
                );
            }
        }
        PlaybackState::Stopped => {
            // Rounded square, same height as the bars.
            let sq = bar_h;
            let left = box_left as f32 + (size - sq) * 0.5;
            draw_rounded_rect_filled(
                pixels,
                width,
                left as i32,
                (v_center - sq / 2.0) as i32,
                sq as i32,
                sq as i32,
                radius,
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

/// Draws the pill's text rows into the same premultiplied pixel buffer as the
/// shapes: glyph coverage from fontdue becomes alpha, so text alpha-composites
/// exactly like every other element (GDI text cannot do this on a layered
/// window — it never touches the alpha channel).
fn draw_text_pixels(state: &mut OverlayState, pixels: &mut [u8], content: &MediaEvent, width: i32, scale: f32) {
    match content {
        MediaEvent::TrackChanged(track) => {
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
            // Only rows that will actually be drawn participate, so title expands
            // to fill the pill when the artist, meta, or source-app line is absent.
            let meta = track.meta_line(true);
            let artist_active = !track.artist.trim().is_empty();
            let active: [bool; 4] = [
                true,
                artist_active,
                !meta.is_empty(),
                !track.source_app.trim().is_empty(),
            ];
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
                &mut state.text_scratch,
                pixels,
                width as usize,
                &track.title,
                &title_rect,
                rows[0].1 as i32,
                appearance.text_color,
                true,
                false,
                Some(&mut state.scroll[0]),
            );

            let artist_rect = next_band(1);
            if artist_active {
                draw_text_line_pixels(
                    &mut state.text_scratch,
                    pixels,
                    width as usize,
                    &track.artist,
                    &artist_rect,
                    rows[1].1 as i32,
                    [0xCC, 0xCC, 0xCC, 0xFF],
                    false,
                    false,
                    Some(&mut state.scroll[1]),
                );
            }

            if active[2] {
                let meta_rect = next_band(2);
                draw_text_line_pixels(
                    &mut state.text_scratch,
                    pixels,
                    width as usize,
                    &meta,
                    &meta_rect,
                    rows[2].1 as i32,
                    [0x99, 0x99, 0x99, 0xFF],
                    false,
                    false,
                    Some(&mut state.scroll[2]),
                );
            }
            if active[3] {
                let app_rect = next_band(3);
                draw_text_line_pixels(
                    &mut state.text_scratch,
                    pixels,
                    width as usize,
                    &track.source_app,
                    &app_rect,
                    rows[3].1 as i32,
                    [0x77, 0x77, 0x77, 0xFF],
                    false,
                    false,
                    Some(&mut state.scroll[3]),
                );
            }
        }
        MediaEvent::PlaybackStateChanged(playback, source_app) => {
            let appearance = &state.config.appearance;
            let padding = (appearance.padding * scale) as i32;
            let art = (appearance.art_size as f32 * scale) as i32;
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

            // Width reserved on the right of the title row for the playback
            // state symbol (▶/‖/■), so it reads like a badge rather than a
            // separate centered row — matching the TrackChanged pill layout.
            let label_w = (80.0 * scale) as i32;

            // Cached track: title row carries the symbol on the right.
            let cached = if source_app.is_empty() {
                None
            } else {
                state.track_cache.get(source_app).cloned()
            };

            if let Some(track) = cached {
                let title_rect = next_band(fs_title * ROW_HEIGHT);
                let title_narrow = RECT {
                    left: title_rect.left,
                    top: title_rect.top,
                    right: title_rect.right - label_w,
                    bottom: title_rect.bottom,
                };
                draw_text_line_pixels(
                    &mut state.text_scratch,
                    pixels,
                    width as usize,
                    &track.title,
                    &title_narrow,
                    fs_title as i32,
                    appearance.text_color,
                    true,
                    false,
                    None,
                );
                draw_symbol_pixels(
                    pixels,
                    width as usize,
                    title_rect.right,
                    title_rect.top,
                    fs_title,
                    *playback,
                    appearance.accent_color,
                );
                if !track.artist.trim().is_empty() {
                    let artist_rect = next_band(fs_artist * 0.85 * ROW_HEIGHT);
                    draw_text_line_pixels(
                        &mut state.text_scratch,
                        pixels,
                        width as usize,
                        &track.artist,
                        &artist_rect,
                        (fs_artist * 0.85) as i32,
                        [0xCC, 0xCC, 0xCC, 0xFF],
                        false,
                        false,
                        None,
                    );
                }
                let meta = track.meta_line(true);
                if !meta.is_empty() {
                    let meta_rect = next_band(fs_artist * 0.85 * ROW_HEIGHT);
                    draw_text_line_pixels(
                        &mut state.text_scratch,
                        pixels,
                        width as usize,
                        &meta,
                        &meta_rect,
                        (fs_artist * 0.85) as i32,
                        [0x99, 0x99, 0x99, 0xFF],
                        false,
                        false,
                        None,
                    );
                }
                if !track.source_app.trim().is_empty() {
                    let source_rect = next_band(fs_artist * 0.85 * ROW_HEIGHT);
                    draw_text_line_pixels(
                        &mut state.text_scratch,
                        pixels,
                        width as usize,
                        &track.source_app,
                        &source_rect,
                        (fs_artist * 0.85) as i32,
                        [0x77, 0x77, 0x77, 0xFF],
                        false,
                        false,
                        None,
                    );
                }
            } else {
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
                        pixels,
                        width as usize,
                        name,
                        &title_narrow,
                        fs_title as i32,
                        appearance.text_color,
                        true,
                        false,
                        None,
                    );
                    draw_symbol_pixels(
                        pixels,
                        width as usize,
                        title_rect.right,
                        title_rect.top,
                        fs_title,
                        *playback,
                        appearance.accent_color,
                    );
                    let artist_rect = next_band(fs_artist * 0.85 * ROW_HEIGHT);
                    draw_text_line_pixels(
                        &mut state.text_scratch,
                        pixels,
                        width as usize,
                        "Unknown",
                        &artist_rect,
                        (fs_artist * 0.85) as i32,
                        [0xCC, 0xCC, 0xCC, 0xFF],
                        false,
                        false,
                        None,
                    );
                }
            }
        }
        // Never rendered: SessionRejected is filtered out before enqueue.
        MediaEvent::SessionRejected { .. } => {}
    }
}

/// Draws one pill text line into the pixel buffer using Windows' own GDI text
/// engine (ClearType subpixel rendering, proper hinting). Text is rendered
/// into a scratch DIB; GDI writes alpha 0 for text into 32bpp DIBs, so each
/// glyph pixel's RGB (text color × coverage) supplies the coverage, which is
/// then composited into the pill's premultiplied buffer.
#[allow(clippy::too_many_arguments)]
fn draw_text_line_pixels(
    text_scratch: &mut Option<TextScratch>,
    pixels: &mut [u8],
    width: usize,
    value: &str,
    rect: &RECT,
    font_height: i32,
    color: [u8; 4],
    bold: bool,
    centered: bool,
    marquee: Option<&mut LineScroll>,
) {
    if value.is_empty() || rect.right <= rect.left || rect.bottom <= rect.top {
        return;
    }
    let rw = rect.right - rect.left;
    let rh = rect.bottom - rect.top;
    let Ok((hdc, bits, sw, sh)) = text_scratch_for(text_scratch, rw, rh) else {
        return;
    };
    unsafe {
        std::ptr::write_bytes(bits.cast::<u8>(), 0, (sw * sh * 4) as usize);
    }
    let font = create_pill_font(font_height, bold);
    if font.0.is_null() {
        return;
    }
    let mut text = wide(value);
    unsafe {
        let old_font = SelectObject(hdc, font);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(
            hdc,
            COLORREF(color[0] as u32 | (color[1] as u32) << 8 | (color[2] as u32) << 16),
        );
        // Row-local drawing: the scratch starts at the row's top-left, so the
        // clip rect is (0, 0, rw, rh) and the text y is centered like the
        // static path.
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(hdc, &mut tm);
        let y = ((rh - tm.tmHeight) / 2).max(0);
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
        if let Some(scroll) = marquee {
            let mut measured = RECT::default();
            let mut measure_text = text.clone();
            let _ = DrawTextW(
                hdc,
                &mut measure_text,
                &mut measured,
                DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT,
            );
            let text_w = measured.right - measured.left;
            // Whether this line overflows its band: while a fully-shown pill
            // has no overflowing line, the animation tick skips repainting.
            scroll.scrolling = text_w > rw;
            let hold_elapsed = scroll.started_at.map(|t| t.elapsed()).unwrap_or_default();
            if text_w <= rw {
                // Text fits: render once statically (no scrolling needed).
                let _ = DrawTextW(hdc, &mut text, &mut local, flags);
            } else if hold_elapsed < MARQUEE_HOLD {
                // Overflow but still in the static hold: render with ellipsis so
                // the text is readable ("…") instead of hard-clipped at the edge.
                let _ = DrawTextW(hdc, &mut text, &mut local, flags);
            } else {
                // Scrolling active: draw two copies offset by the marquee delta.
                let total = text_w + MARQUEE_GAP as i32;
                let off = (scroll.offset % total as f32) as i32;
                let clip = RECT {
                    left: 0,
                    top: 0,
                    right: rw,
                    bottom: rh,
                };
                let x1 = -off;
                let _ = ExtTextOutW(
                    hdc,
                    x1,
                    y,
                    ETO_CLIPPED,
                    Some(&clip),
                    PCWSTR(text.as_ptr()),
                    text.len() as u32,
                    None,
                );
                let x2 = x1 + total;
                if x2 < rw {
                    let _ = ExtTextOutW(
                        hdc,
                        x2,
                        y,
                        ETO_CLIPPED,
                        Some(&clip),
                        PCWSTR(text.as_ptr()),
                        text.len() as u32,
                        None,
                    );
                }
            }
        } else {
            let _ = DrawTextW(hdc, &mut text, &mut local, flags);
        }
        SelectObject(hdc, old_font);
    }

    // Composite the glyph pixels. Coverage = max(R,G,B); the scratch RGB is
    // the text color premultiplied by that coverage (all pill text colors are
    // fully opaque).
    let sw = sw as usize;
    let sh = sh as usize;
    for y in 0..sh {
        for x in 0..sw {
            let p = unsafe { bits.cast::<u8>().add((y * sw + x) * 4) };
            let b = unsafe { *p as u32 };
            let g = unsafe { *p.add(1) as u32 };
            let r = unsafe { *p.add(2) as u32 };
            let cov = r.max(g).max(b);
            if cov == 0 {
                continue;
            }
            let alpha = cov * color[3] as u32 / 255;
            composite_pm(
                pixels,
                width,
                (rect.left + x as i32) as usize,
                (rect.top + y as i32) as usize,
                [r as u8, g as u8, b as u8],
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
    let bitmap = unsafe { CreateDIBSection(hdc, &info, DIB_RGB_COLORS, &mut bits, None, 0) }?;
    if bits.is_null() {
        unsafe {
            let _ = DeleteDC(hdc);
        }
        anyhow::bail!("CreateDIBSection returned no pixel buffer");
    }
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

/// Cache key: (font height, bold, GDI quality constant).
type FontKey = (i32, bool, u32);

/// Process-wide cache of created HFONTs, keyed by (height, bold, quality).
/// Fonts are pure GDI objects with a tiny key set (a few sizes × 2 weights × 2
/// qualities), so caching them for the process lifetime replaces thousands of
/// CreateFontW/DeleteObject pairs per second with hash lookups. Handles are
/// stored as `usize`: HFONT is a raw pointer and not Send, but GDI font
/// handles are process-global and every use here is on the UI thread.
static FONT_CACHE: OnceLock<Mutex<HashMap<FontKey, usize>>> = OnceLock::new();

/// Returns the cached Segoe UI font for (height, bold, quality), creating it
/// on first use. The returned handle must never be deleted (it stays valid
/// until process exit).
pub(crate) fn cached_font(height: i32, bold: bool, quality: u32) -> HFONT {
    let cache = FONT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        if let Some(font) = guard.get(&(height, bold, quality)) {
            return HFONT(*font as *mut std::ffi::c_void);
        }
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
                quality,
                DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
                PCWSTR(font_name.as_ptr()),
            )
        };
        if !font.0.is_null() {
            guard.insert((height, bold, quality), font.0 as usize);
        }
        font
    } else {
        HFONT::default()
    }
}

/// Creates the pill's Segoe UI font with ClearType subpixel rendering,
/// cached across frames.
fn create_pill_font(height: i32, bold: bool) -> HFONT {
    cached_font(height, bold, CLEARTYPE_QUALITY.0 as u32)
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
    // ClearType subpixel rendering is incorrect on layered windows; grayscale
    // antialiasing keeps the pill text crisp.
    let font = cached_font(height, bold, ANTIALIASED_QUALITY.0 as u32);
    if font.0.is_null() {
        return;
    }
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
    }
}

/// Artwork only ever displays at ~200px, so refusing anything larger than
/// this defeats decompression bombs (a header can claim huge dimensions
/// while the compressed payload is tiny) without affecting real album art.
const ART_MAX_DIM: u32 = 4096;

/// Decodes artwork bytes with a hard cap on source dimensions. The `image`
/// crate's dimension limits are strict, so an oversized image fails here
/// instead of allocating a huge buffer.
fn decode_limited(data: &[u8]) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(ART_MAX_DIM);
    limits.max_image_height = Some(ART_MAX_DIM);
    reader.limits(limits);
    reader.decode().ok()
}

pub(crate) fn decode_artwork(data: &[u8], size: usize) -> Option<Vec<u8>> {
    let image = decode_limited(data)?.to_rgba8();
    let image = image::imageops::resize(&image, size as u32, size as u32, FilterType::Triangle);
    Some(image.into_raw())
}

/// Decodes artwork directly into the premultiplied BGRA layout that
/// StretchDIBits consumes (top-down 32bpp DIB), so the main window can draw
/// the cached bitmap with a single blit instead of re-converting per paint.
pub(crate) fn decode_artwork_pm(data: &[u8], size: usize) -> Option<Vec<u8>> {
    let image = decode_limited(data)?.to_rgba8();
    let image = image::imageops::resize(&image, size as u32, size as u32, FilterType::Triangle);
    let raw = image.into_raw();
    let mut pm = Vec::with_capacity(raw.len());
    for px in raw.chunks_exact(4) {
        let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        pm.push((b * a / 255) as u8);
        pm.push((g * a / 255) as u8);
        pm.push((r * a / 255) as u8);
        pm.push(a as u8);
    }
    Some(pm)
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
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
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
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        draw_text_line_pixels(
            &mut state.text_scratch,
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
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut pixels,
            200,
            "Hello World",
            &rect,
            12,
            [255, 255, 255, 255],
            false,
            false,
            Some(&mut LineScroll::default()),
        );
        let lit = pixels.chunks(4).filter(|p| p[3] > 0).count();
        assert!(lit > 100, "expected glyph pixels with marquee state, got {lit}");
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
        draw_text_pixels(&mut state, &mut pixels, &MediaEvent::TrackChanged(track), 240, 1.0);
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
        draw_text_line_pixels(
            &mut state.text_scratch,
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
}
