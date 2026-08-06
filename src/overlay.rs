use crate::config::{Config, HorizontalPosition, VerticalPosition};
use crate::events::{MEDIA_EVENT_MSG, MediaEvent, PlaybackState, TOGGLE_MSG, TrackInfo};
use crate::palette::Palette;
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
use windows::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC,
    CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, DEVMODEW, DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER,
    DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, ENUM_CURRENT_SETTINGS,
    ETO_CLIPPED, EnumDisplaySettingsW, ExtTextOutW, FF_DONTCARE, GetMonitorInfoW, GetTextMetricsW, HBITMAP, HBRUSH,
    HDC, HFONT, HGDIOBJ, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MONITORINFOEXW,
    MonitorFromWindow, OUT_DEFAULT_PRECIS, SelectObject, SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT,
    ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateTimerQueueTimer, DeleteTimerQueueTimer, WT_EXECUTEDEFAULT};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, GWLP_USERDATA, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, HTTRANSPARENT, HWND_TOPMOST, IDC_ARROW, IsWindowVisible, KillTimer, LoadCursorW, MA_NOACTIVATE,
    RegisterClassExW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER,
    SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SendMessageW, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    ULW_ALPHA, WM_APP, WM_DESTROY, WM_MOUSEACTIVATE, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_TIMER,
    WNDCLASS_STYLES, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};
use windows::core::PCWSTR;

const TIMER_DEBOUNCE: usize = 1;
const LIGHT_DURATION: Duration = Duration::from_millis(120);
/// Remaining time left on the current pill when something newer wants the
/// screen: hovering over the pill or a queued update both cap the exit at
/// this, so the user never waits out the full duration to see a change.
const EARLY_EXIT_MS: u64 = 500;

/// Posted by the high-resolution animation timer to drive pill frames.
const TIMER_ANIMATION_MSG: u32 = WM_APP + 6;

/// Samples the monitor's current refresh period in ms, so the animation timer
/// ticks once per presented frame on any display (60 Hz → 16 ms, 120 Hz → 8 ms,
/// 144 Hz → 7 ms, 240 Hz → 4 ms). The pill is positioned over the foreground
/// window's monitor (see `position()`), so the query targets that same monitor:
/// the primary display can run at a different rate on mixed-refresh setups.
/// Prefers DWM's live compose rate, which stays correct on variable-refresh-rate
/// monitors; falls back to the display mode's nominal frequency; last resort is
/// 16 ms (60 Hz).
fn refresh_period_ms() -> u32 {
    let foreground = unsafe { GetForegroundWindow() };
    let dwm_period = unsafe {
        let mut timing = std::mem::zeroed::<DWM_TIMING_INFO>();
        timing.cbSize = std::mem::size_of::<DWM_TIMING_INFO>() as u32;
        DwmGetCompositionTimingInfo(foreground, &mut timing)
            .ok()
            .and_then(|()| {
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
    // it hits the same monitor the pill will be shown on.
    let mode_period = unsafe {
        let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
        if monitor.0.is_null() {
            None
        } else {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool().then(|| {
                let mut devmode = std::mem::zeroed::<DEVMODEW>();
                devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
                EnumDisplaySettingsW(PCWSTR(info.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut devmode)
                    .as_bool()
                    .then(|| 1000u32.checked_div(devmode.dmDisplayFrequency as u32))
                    .flatten()
            })
        }
    };
    mode_period.flatten().unwrap_or(16).clamp(1, 100)
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
    /// When the cursor hovers over the pill, the dismiss deadline is
    /// shortened to 500ms. The arm is one-way: the pill dismisses 500ms
    /// after the hover is first detected even if the cursor leaves before
    /// then. The flag also stops the tick from re-arming (which would keep
    /// pushing the deadline forward while the cursor stays put).
    hover_dismiss_at: Option<Instant>,
    position: OverlayPos,
    /// Per-row marquee state for the four track lines (title/subtitle/meta/app).
    scroll: [LineScroll; 4],
    /// High-resolution timer driving the pill animation.
    anim_timer: HANDLE,
    /// Animation tick period in ms, matched to the monitor's refresh rate.
    /// Re-detected on every show; the timer is recreated only when it changes.
    tick_period: u32,
    /// Cached decoded artwork for the current track (RGBA8 at the full art
    /// size), so animation frames never re-decode the JPEG/PNG.
    decoded_art: Option<Vec<u8>>,
    /// The artwork bytes that produced `decoded_art`, so a cover change for
    /// the same song (same title+artist, different art) re-decodes instead of
    /// showing the stale image.
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
    /// Reusable UTF-16 scratch buffer for GDI text rendering, cleared and
    /// refilled on each text-line draw so the render tick performs no per-frame
    /// heap allocation for text encoding.
    scratch_utf16: Vec<u16>,
    /// Mutex-free font cache: HFONT handles keyed by (height, bold, quality),
    /// paired with the font's `tmHeight` text metric (a pure function of the
    /// same key, measured once so the render path never re-queries it per
    /// text row per frame). Used by the render path instead of the global
    /// `FONT_CACHE` so per-frame text rendering performs no cross-thread
    /// synchronization. Fonts are created with `CreateFontW` directly and
    /// deleted when the DPI changes or the window is destroyed.
    font_cache: HashMap<FontKey, (HFONT, i32)>,
    /// Last DPI value observed in render(), so the font cache can be flushed
    /// when the window moves to another monitor or system scaling changes.
    last_dpi: u32,
    /// Physical-pixel inset from the buffer edge to the pill body, computed
    /// each frame from `AURA_HALO_LOGICAL * dpi * shape`. The pill is
    /// drawn at `(aura_inset, aura_inset)` so the aura fills the outer ring.
    aura_inset: i32,
}

impl Drop for OverlayState {
    fn drop(&mut self) {
        self.flush_fonts();
    }
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
            hover_dismiss_at: None,
            position,
            scroll: [LineScroll::default(); 4],
            anim_timer: HANDLE::default(),
            tick_period: 16,
            decoded_art: None,
            decoded_art_source: None,
            palette: None,
            dib: None,
            frame_scratch: Vec::new(),
            last_tick: Instant::now(),
            last_reassert: None,
            current_source: None,
            track_cache: HashMap::new(),
            text_scratch: None,
            scratch_utf16: Vec::new(),
            font_cache: HashMap::new(),
            last_dpi: 0,
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

    /// Decodes (once per artwork) and caches the artwork bitmap at the full
    /// art size, so animation frames never re-decode the JPEG/PNG. Keyed by
    /// the artwork bytes themselves: the same song with a different cover
    /// re-decodes, while unchanged art (session recreation, re-render) is
    /// served from the cache. The palette is derived from the same decoded
    /// buffer (~0.1ms, only when a re-decode happens), so no separate
    /// full-resolution decode is ever needed for color extraction.
    fn ensure_art(&mut self, artwork: Option<&Arc<[u8]>>, base_size: usize) {
        let same_art = match (&self.decoded_art_source, artwork) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a.as_ref() == b.as_ref(),
            (None, None) => true,
            _ => false,
        };
        if self.decoded_art.is_none() || !same_art {
            self.decoded_art = artwork.and_then(|a| decode_artwork(a, base_size));
            self.decoded_art_source = artwork.cloned();
            self.palette = self.decoded_art.as_deref().and_then(crate::palette::palette_from_rgba);
        }
    }

    /// Returns the Segoe UI (ANTIALIASED_QUALITY) HFONT for the given pixel
    /// height and weight, creating it on first use and caching it locally on
    /// the state, together with the font's `tmHeight` text metric (used for
    /// vertical centering; measured once per key instead of on every text
    /// row of every frame). Unlike the global `cached_font`, this never takes
    /// a cross-thread Mutex, so it is safe to call on every render tick. The
    /// cache is flushed on DPI change and window destruction (see
    /// `flush_fonts`).
    fn font_for(&mut self, height: i32, bold: bool) -> (HFONT, i32) {
        let key = (height, bold, ANTIALIASED_QUALITY.0 as u32);
        if let Some((font, tm_height)) = self.font_cache.get(&key) {
            return (*font, *tm_height);
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
                ANTIALIASED_QUALITY.0 as u32,
                DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
                PCWSTR(font_name.as_ptr()),
            )
        };
        let mut tm_height = 0;
        if !font.0.is_null() {
            unsafe {
                let hdc = CreateCompatibleDC(None);
                if !hdc.0.is_null() {
                    let old_font = SelectObject(hdc, font);
                    let mut tm = TEXTMETRICW::default();
                    if GetTextMetricsW(hdc, &mut tm).as_bool() {
                        tm_height = tm.tmHeight;
                    }
                    SelectObject(hdc, old_font);
                    let _ = DeleteDC(hdc);
                }
            }
            self.font_cache.insert(key, (font, tm_height));
        }
        (font, tm_height)
    }

    /// Deletes all cached HFONT handles, releasing GDI resources. Called when
    /// the DPI changes (fonts become invalid at the new scale) and on window
    /// destruction.
    fn flush_fonts(&mut self) {
        for (_, (font, _)) in self.font_cache.drain() {
            unsafe {
                let _ = DeleteObject(font);
            }
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
                self.tick_period,
                self.tick_period,
                WT_EXECUTEDEFAULT,
            );
        }
        self.anim_timer = handle;
    }

    /// Re-samples the monitor's refresh period and recreates the animation
    /// timer when it changed (display switched, DPI changed, VRR kicked in).
    /// The tick cadence only affects how many frames the UI thread gets asked
    /// to paint; the easing is time-based, so motion is identical either way.
    fn sync_anim_timer(&mut self) {
        let period = refresh_period_ms();
        if period != self.tick_period {
            debug!(
                "animation tick {period}ms = {} Hz (refresh-rate matched)",
                1000 / period.max(1)
            );
            self.tick_period = period;
            self.delete_anim_timer();
        }
        self.ensure_anim_timer();
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
                    // matches do not refresh in place, and a different cover
                    // for the same title+artist (video vs audio version)
                    // queues a fresh pill rather than updating the old one.
                    let is_update = self.content.as_ref().is_some_and(
                        |content| matches!(content, MediaEvent::TrackChanged(shown) if shown.same_media(&track)),
                    );
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
    /// current animation phase, extends the visible time, and re-renders. The
    /// pill's size is constant — every row band is always reserved — so a
    /// refresh only changes the drawn rows, never the pill's dimensions.
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
        // speed is identical at any frame rate.
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
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
        let raw_dpi = unsafe { GetDpiForWindow(self.hwnd) };
        if raw_dpi != 0 && raw_dpi != self.last_dpi {
            self.last_dpi = raw_dpi;
            self.flush_fonts();
        }
        let dpi = raw_dpi.max(96) as f32 / 96.0;
        let (logical_width, logical_height) = content_size_of(&self.config, &content);
        let width = (logical_width * dpi * shape).round().max(1.0) as i32;
        let height = (logical_height * dpi * shape).round().max(1.0) as i32;
        self.aura_inset = (AURA_HALO_LOGICAL * dpi * shape).round() as i32;
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
        // The DIB is inflated by `aura_inset` on each side, but the PILL
        // (not the window) must be centered. Subtract the inset so the pill
        // lands where the user expects it.
        let inset = self.aura_inset;
        let x = if let Some(px) = self.position.x {
            (px as f32 * scale).round() as i32
        } else {
            match self.position.horizontal {
                // The DIB extends `inset` beyond the pill on each side, so
                // the glow reaches `margin` from the work-area edge — the
                // pill itself sits `margin + inset` in.
                HorizontalPosition::Left => work.left + margin + inset,
                HorizontalPosition::Center => work.left + (span_w - width) / 2 - inset,
                HorizontalPosition::Right => work.right - width - margin - inset,
            }
        };
        let y = if let Some(py) = self.position.y {
            (py as f32 * scale).round() as i32
        } else {
            match self.position.vertical {
                // The DIB extends `inset` beyond the pill on each side; shift
                // the window so the PILL body (not the aura) sits at the
                // configured margin from the work-area edge.
                VerticalPosition::Top => work.top + margin + inset,
                VerticalPosition::Bottom => work.bottom - height - margin - inset,
            }
        };
        // Clamp to the current work area so absolute overrides stay usable after a
        // resolution or monitor change.
        let x = x.clamp(work.left, (work.right - width).max(work.left));
        let y = y.clamp(work.top, (work.bottom - height).max(work.top));
        Some(POINT { x, y })
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
        self.content = None;
        self.dismiss_at = None;
        self.hover_dismiss_at = None;
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
        let (logical_width, logical_height) = content_size_of(&self.config, content);
        let width = (logical_width * dpi * shape).round().max(1.0) as i32;
        let height = (logical_height * dpi * shape).round().max(1.0) as i32;
        Some((width, height))
    }

    /// Shows a short-lived preview of the overlay at its current position, used by
    /// the tray "Show sample" command to preview placement without real media.
    /// Shows the most recent real track (and its palette/aura) so the preview
    /// looks like an actual notification; on a fresh start before any track
    /// has been seen it falls back to a track-change pill with sample data.
    fn show_sample(&mut self) {
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
fn content_size_of(config: &Config, content: &MediaEvent) -> (f32, f32) {
    match content {
        MediaEvent::TrackChanged(_) | MediaEvent::PlaybackStateChanged(_, _) => content_size(config),
        // Never shown (receive_events skips it); the .max(1.0) guards keep the
        // size sane if this dead arm is ever reached.
        MediaEvent::SessionRejected { .. } => (0.0, 0.0),
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

/// Creates the passive WinGlance overlay window. It owns no message loop: the caller
/// runs the loop and destroys the window at exit.
pub(crate) fn create_window(config: Config, queue: EventQueue) -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.context("getting the process module")?;
    let instance: HINSTANCE = module.into();
    let class_name = wide("WinGlanceOverlayWindow");
    register_window_class(instance, &class_name)?;

    let state = Box::new(OverlayState::new(config, queue));
    let state_ptr = Box::into_raw(state);
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
            // freed in WM_NCDESTROY. If CreateWindowExW fails after WM_NCCREATE
            // ran, the system tears the window down through WM_NCDESTROY first,
            // so freeing the box here would double-free it. Freeing here only
            // covers the WM_NCCREATE-never-ran case, which cannot happen
            // because the class was just registered above.
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
    // GDI call. The scratch buffer is grown but never shrunk across frames,
    // so after warm-up this performs no per-frame heap allocation, matching
    // the existing `text_scratch` buffer's pattern elsewhere in this file.
    let (hdc, _bitmap, bits) = dib_for(state, buf_w, buf_h)?;
    let alloc_w = state.dib.as_ref().map(|dib| dib.width).unwrap_or(buf_w) as usize;
    let alloc_h = state.dib.as_ref().map(|dib| dib.height).unwrap_or(buf_h) as usize;

    let needed = buf_w as usize * buf_h as usize * 4;
    let mut scratch = std::mem::take(&mut state.frame_scratch);
    if scratch.len() < needed {
        scratch.resize(needed, 0);
    } else {
        scratch[..needed].fill(0);
    }
    draw_pixels(
        state,
        &mut scratch[..needed],
        content,
        buf_w as usize,
        buf_h as usize,
        scale,
        art_base,
    )?;
    draw_text_pixels(state, &mut scratch[..needed], content, buf_w, scale);
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
        let _ = ShowWindow(state.hwnd, SW_SHOWNOACTIVATE);
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
    let (bound_w, bound_h) = backing_upper_bound(&state.config, state.last_dpi);
    let alloc_w = width.max(bound_w).max(1);
    let alloc_h = height.max(bound_h).max(1);
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: alloc_w,
            biHeight: -alloc_h,
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
    let bitmap = unsafe { CreateDIBSection(hdc, &info, DIB_RGB_COLORS, &mut bits, None, 0) }?;
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
    art_base: usize,
) -> Result<()> {
    let radius = state.config.appearance.corner_radius * scale;
    // Resolve the artwork that will be displayed and decode it (once per
    // unique cover) up front, so the aura palette below is ready and the
    // cover is never shown stale. Track pills carry the artwork directly;
    // state pills reuse the cached track's for the source.
    let artwork: Option<Arc<[u8]>> = match content {
        MediaEvent::TrackChanged(track) => track.artwork.clone(),
        MediaEvent::PlaybackStateChanged(_, source_app) => {
            if source_app.is_empty() {
                None
            } else {
                state.track_cache.get(source_app).and_then(|t| t.artwork.clone())
            }
        }
        MediaEvent::SessionRejected { .. } => None,
    };
    state.ensure_art(artwork.as_ref(), art_base);
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

    match content {
        MediaEvent::TrackChanged(_) => {
            let padding = (state.config.appearance.padding * scale).round() as usize;
            let art_size = (state.config.appearance.art_size as f32 * scale).round() as usize;
            // Must match the mask radius draw_art_scaled uses for the art
            // bitmap itself, not the pill's corner_radius — otherwise the
            // halo/rim are rounder than the art beneath them and visibly
            // don't hug its corners.
            let art_radius = art_size as f32 * 0.2;
            let art_x = inset + padding;
            let art_y = inset + pill_h.saturating_sub(art_size) / 2;
            // Album art halo: subtle accent glow behind the art square.
            if let Some(c) = state.palette.map(|p| p.primary) {
                let halo_pad = (1.5 * scale).round() as usize;
                let halo_size = art_size + halo_pad * 2;
                let halo_x = art_x.saturating_sub(halo_pad);
                let halo_y = art_y.saturating_sub(halo_pad);
                let halo_radius = art_radius + halo_pad as f32;
                for dy in 0..halo_size {
                    for dx in 0..halo_size {
                        let cov =
                            round_rect_coverage(dx as f32, dy as f32, halo_size as f32, halo_size as f32, halo_radius);
                        if cov > 0.0 {
                            let alpha = (c[3] as f32 * 0.75 * cov) as u32;
                            composite(pixels, width, halo_x + dx, halo_y + dy, [c[0], c[1], c[2]], alpha);
                        }
                    }
                }
            }
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
            // Glowing rim: thin 1.5px accent stroke around the album art.
            if let Some(c) = state.palette.map(|p| p.primary) {
                let stroke_w = (1.5 * scale).round().max(1.0);
                for dy in 0..art_size {
                    for dx in 0..art_size {
                        let d =
                            round_rect_signed_dist(dx as f32, dy as f32, art_size as f32, art_size as f32, art_radius);
                        if d.abs() < stroke_w {
                            let edge = 1.0 - d.abs() / stroke_w;
                            let alpha = (c[3] as f32 * 0.9 * edge) as u32;
                            composite(pixels, width, art_x + dx, art_y + dy, [c[0], c[1], c[2]], alpha);
                        }
                    }
                }
            }
        }
        MediaEvent::PlaybackStateChanged(_, _) => {
            // State pills reuse the cached track's artwork for the source that
            // produced the state change, so a pause/play pill still shows the
            // right cover. Falls back to the accent placeholder when nothing
            // has been cached for this source yet.
            let padding = (state.config.appearance.padding * scale).round() as usize;
            let art_size = (state.config.appearance.art_size as f32 * scale).round() as usize;
            let art_size = art_size.min(pill_h.saturating_sub(2 * padding));
            // Must match the mask radius draw_art_scaled uses for the art
            // bitmap itself, not the pill's corner_radius — otherwise the
            // halo/rim are rounder than the art beneath them and visibly
            // don't hug its corners.
            let art_radius = art_size as f32 * 0.2;
            let art_x = inset + padding;
            let art_y = inset + pill_h.saturating_sub(art_size) / 2;
            // Album art halo: subtle accent glow behind the art square.
            if let Some(c) = state.palette.map(|p| p.primary) {
                let halo_pad = (1.5 * scale).round() as usize;
                let halo_size = art_size + halo_pad * 2;
                let halo_x = art_x.saturating_sub(halo_pad);
                let halo_y = art_y.saturating_sub(halo_pad);
                let halo_radius = art_radius + halo_pad as f32;
                for dy in 0..halo_size {
                    for dx in 0..halo_size {
                        let cov =
                            round_rect_coverage(dx as f32, dy as f32, halo_size as f32, halo_size as f32, halo_radius);
                        if cov > 0.0 {
                            let alpha = (c[3] as f32 * 0.75 * cov) as u32;
                            composite(pixels, width, halo_x + dx, halo_y + dy, [c[0], c[1], c[2]], alpha);
                        }
                    }
                }
            }
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
            // Glowing rim: thin 1.5px accent stroke around the album art.
            if let Some(c) = state.palette.map(|p| p.primary) {
                let stroke_w = (1.5 * scale).round().max(1.0);
                for dy in 0..art_size {
                    for dx in 0..art_size {
                        let d =
                            round_rect_signed_dist(dx as f32, dy as f32, art_size as f32, art_size as f32, art_radius);
                        if d.abs() < stroke_w {
                            let edge = 1.0 - d.abs() / stroke_w;
                            let alpha = (c[3] as f32 * 0.9 * edge) as u32;
                            composite(pixels, width, art_x + dx, art_y + dy, [c[0], c[1], c[2]], alpha);
                        }
                    }
                }
            }
        }
        // Never rendered: SessionRejected is filtered out before enqueue.
        MediaEvent::SessionRejected { .. } => {}
    }
    Ok(())
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
    track: &TrackInfo,
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
    let (font_title, h_title) = state.font_for(rows[0].1 as i32, true);
    let (font_artist, h_artist) = state.font_for(rows[1].1 as i32, false);
    let (font_meta, h_meta) = state.font_for(rows[2].1 as i32, false);
    let (font_app, h_app) = state.font_for(rows[3].1 as i32, false);
    // Only rows that will actually be drawn take up vertical space: the rest
    // of the pill's constant height stays empty below the rows.
    let (meta_clock, meta) = track.meta_line_for_overlay(true);
    let artist_active = !track.artist.trim().is_empty();
    let active: [bool; 4] = [
        true,
        artist_active,
        !meta.is_empty(),
        !track.source_app.trim().is_empty(),
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
        &track.title,
        &title_narrow,
        font_title,
        h_title,
        text_color,
        false,
        Some(&mut state.scroll[0]),
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
            &track.artist,
            &artist_rect,
            font_artist,
            h_artist,
            muted_accent(accent),
            false,
            Some(&mut state.scroll[1]),
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
            &meta,
            meta_clock,
            font_meta,
            rows[2].1 as i32,
            h_meta,
            accent,
            accent,
            scale,
            Some(&mut state.scroll[2]),
        );
    }
    if active[3] {
        let app_rect = next_band(3);
        draw_source_app_row(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            pixels,
            width as usize,
            track,
            &app_rect,
            font_app,
            h_app,
            muted,
            scale,
            Some(&mut state.scroll[3]),
        );
    }
}

/// Draws the pill's text rows into the same premultiplied pixel buffer as the
/// shapes: glyph coverage from fontdue becomes alpha, so text alpha-composites
/// exactly like every other element (GDI text cannot do this on a layered
/// window — it never touches the alpha channel).
fn draw_text_pixels(state: &mut OverlayState, pixels: &mut [u8], content: &MediaEvent, width: i32, scale: f32) {
    match content {
        MediaEvent::TrackChanged(track) => {
            draw_pill_text_rows(state, pixels, width, scale, track, Some(PlaybackState::NowPlaying));
        }
        MediaEvent::PlaybackStateChanged(playback, source_app) => {
            // Cached track: render the shared layout with the state symbol
            // on the title row.
            let cached = if source_app.is_empty() {
                None
            } else {
                state.track_cache.get(source_app).cloned()
            };
            if let Some(track) = cached {
                draw_pill_text_rows(state, pixels, width, scale, &track, Some(*playback));
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
                let (font_title, h_title) = state.font_for(fs_title as i32, true);
                let (font_artist, h_artist) = state.font_for((fs_artist * 0.85) as i32, false);
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
                        None,
                    );
                }
            }
        }
        // Never rendered: SessionRejected is filtered out before enqueue.
        MediaEvent::SessionRejected { .. } => {}
    }
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
    marquee: Option<&mut LineScroll>,
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
        if let Some(scroll) = marquee {
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
            let was_scrolling = scroll.scrolling;
            scroll.scrolling = text_w > rw;
            if scroll.scrolling && !was_scrolling {
                debug!("marquee overflow | text_w={text_w} | draw_w={rw} | title={value}");
            }
            let hold_elapsed = scroll.started_at.map(|t| t.elapsed()).unwrap_or_default();
            if text_w <= rw {
                // Text fits: render once statically (no scrolling needed).
                let _ = DrawTextW(hdc, &mut *scratch_utf16, &mut local, flags);
            } else if hold_elapsed < MARQUEE_HOLD {
                // Overflow but still in the static hold: render with ellipsis so
                // the text is readable ("…") instead of hard-clipped at the edge.
                let _ = DrawTextW(hdc, &mut *scratch_utf16, &mut local, flags);
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
                    PCWSTR(scratch_utf16.as_ptr()),
                    scratch_utf16.len() as u32,
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
                        PCWSTR(scratch_utf16.as_ptr()),
                        scratch_utf16.len() as u32,
                        None,
                    );
                }
            }
        } else {
            let _ = DrawTextW(hdc, &mut *scratch_utf16, &mut local, flags);
        }
        SelectObject(hdc, old_font);
    }

    // Composite the glyph pixels. The scratch is white-on-black, so the RGB
    // channels are the glyph coverage; alpha is coverage scaled by the text
    // color's own alpha, and the color is premultiplied by alpha for
    // `composite_pm`. Drawing the final color via SetTextColor instead would
    // make GDI pre-dim the scratch, and reading that dimmed value as coverage
    // would render gray text at ~brightness² opacity.
    let sw = sw as usize;
    let rw = rw as usize;
    let rh = rh as usize;
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
                pixels,
                width,
                (rect.left + x as i32) as usize,
                (rect.top + y as i32) as usize,
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

/// Creates the pill's Segoe UI font with grayscale antialiasing, cached across
/// frames. ClearType subpixel rendering is unusable here: it paints colored
/// fringes into the scratch DIB, and the text path derives glyph coverage from
/// the RGB channels, so gray AA keeps the mask clean (see `draw_string` for
/// the same call on the layered-window path).
#[allow(dead_code)]
fn create_pill_font(height: i32, bold: bool) -> HFONT {
    cached_font(height, bold, ANTIALIASED_QUALITY.0 as u32)
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
    if dest_size == 0 || icon_size == 0 || icon.is_empty() {
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
    track: &TrackInfo,
    rect: &RECT,
    font: HFONT,
    tm_height: i32,
    color: [u8; 4],
    scale: f32,
    marquee: Option<&mut LineScroll>,
) {
    if let Some(icon) = track.app_icon.as_deref() {
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
            &track.source_app,
            &text_rect,
            font,
            tm_height,
            color,
            false,
            marquee,
        );
    } else {
        draw_text_line_pixels(
            text_scratch,
            scratch_utf16,
            pixels,
            width,
            &track.source_app,
            rect,
            font,
            tm_height,
            color,
            false,
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
            if !state.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                (*state).hwnd = hwnd;
            }
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
                state.flush_fonts();
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
        let (width, height) = content_size_of(&config, &MediaEvent::TrackChanged(full));
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
        let (_, compact) = content_size_of(&config, &MediaEvent::TrackChanged(minimal));
        assert_eq!(compact, height, "missing rows must not shrink the pill");
        // State pills share the same constant height.
        let (_, state_h) = content_size_of(
            &config,
            &MediaEvent::PlaybackStateChanged(PlaybackState::Paused, "App".into()),
        );
        assert_eq!(state_h, height, "state pills must match the track pill height");
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
        let (font, h) = state.font_for(12, false);
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
        let (font, h) = state.font_for(48, false);
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
        let (font, h) = state.font_for(12, false);
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
            Some(&mut LineScroll::default()),
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
        let (font, h) = state.font_for(12, false);
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
            Some(&mut scroll),
        );
        assert!(
            scroll.scrolling,
            "a title wider than the visible band must be marked as scrolling"
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
        let (font, h) = state.font_for(12, false);
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
            .push_back(MediaEvent::TrackChanged(TrackInfo {
                source_app: "spotify".into(),
                title: "Next Song".into(),
                ..TrackInfo::default()
            }));
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
            .push_back(MediaEvent::TrackChanged(TrackInfo {
                source_app: "spotify".into(),
                title: "Next Song".into(),
                ..TrackInfo::default()
            }));
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
}
