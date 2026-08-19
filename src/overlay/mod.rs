//! The pill overlay: state, tick, event handling, hover interaction and the
//! window/timer glue. Rendering lives in `render`, morph springs and pill
//! geometry in `morph`, and display enumeration and fullscreen detection in
//! `fullscreen`; the `set_*` push functions and `create_window`/`show_sample`
//! are the crate-facing entry points, re-exported at the bottom of this
//! module's import block.

use crate::config::{Config, HorizontalPosition, LayoutMode, MonitorMode, VerticalPosition};
use crate::events::{
    MEDIA_EVENT_MSG, MediaEvent, PlaybackState, TOGGLE_MSG, TrackInfo, artwork_same, media_event_into_owned,
};
use crate::gdi::FontProvider;
use crate::palette::Palette;
use crate::winapi::{delete_object, kill_timer, post_message, select_object, set_timer, set_window_pos, validate_rect};
use crate::winutil::{StateClaim, release_window_state, set_window_state, wide, window_state};
use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteDC, HBITMAP, HDC, HFONT, HGDIOBJ};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateTimerQueueTimer, WT_EXECUTEDEFAULT};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, UiaReturnRawElementProvider, UiaRootObjectId, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, DefWindowProcW, EVENT_SYSTEM_FOREGROUND, GetCursorPos, GetForegroundWindow,
    GetWindowThreadProcessId, HTTRANSPARENT, HWND_TOPMOST, IsWindowVisible, MA_NOACTIVATE, MSG, PM_REMOVE, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    ShowWindow, WINEVENT_OUTOFCONTEXT, WM_APP, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_GETOBJECT,
    WM_MOUSEACTIVATE, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_TIMER, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

mod fullscreen;
mod morph;
mod render;

pub(crate) use fullscreen::{enumerate_displays_cached, invalidate_display_cache};
pub(crate) use render::{TEXT_CONTRAST_AA, ensure_contrast, pm_bgra_to_rgba};
// Tests outside this module assert contrast ratios through the shared
// helper; the binary itself never names it.
#[cfg(test)]
pub(crate) use render::contrast_ratio;

use fullscreen::{
    ForegroundVerdict, TargetMonitor, anchor_unchanged, decide_layout, effective_position_rect,
    foreground_fullscreens_target, foreground_monitor_index, log_target_once, monitor_dpi, placement,
    refresh_period_ms, resolve_target, window_is_fullscreen,
};
use morph::{
    ENTRANCE_GROW, HoverExpand, HoverStep, HoverTick, MorphDirection, MorphProgress, animation_duration, bounce_scale,
    collapse_duration, content_size_of, ease_out_quint, hover_engaged, hover_progress, hover_step, lagged_collapse,
    lagged_expand, morph_duration, morph_size, normalized_elapsed, reversal_seed, spring_collapse,
};
use render::{AURA_HALO_LOGICAL, pill_text_from_track, render_layered};

/// How long an in-place content swap dissolves the previous frame into the
/// new one (see `ContentFade`).
const CONTENT_FADE_DURATION: Duration = Duration::from_millis(200);

/// An in-place content cross-fade: the previous frame (the last rendered
/// pixels) blends into the new content's frames over `CONTENT_FADE_DURATION`,
/// so a track swap reads as a dissolve instead of a hard cut. Only valid
/// while the pill renders the same static frame size (Phase::Shown, no hover
/// morph, no bounce); `render_layered` ends the fade the moment any of those
/// change, and the next frame renders the new content plainly.
struct ContentFade {
    /// When the fade started.
    start: Instant,
    /// The previous frame's premultiplied BGRA pixels, tightly packed.
    from: Vec<u8>,
    /// The previous frame's dimensions.
    from_w: usize,
    from_h: usize,
}

const TIMER_DEBOUNCE: usize = 1;
/// Window-timer ID used only when the timer-queue fallback is active.
const ANIM_TIMER_ID: usize = 2;
/// One-shot window-timer ID that releases the frame pipeline buffers after
/// the pill has stayed hidden for `IDLE_BUFFER_RELEASE_MS` (see `hide`).
const IDLE_BUFFER_TIMER_ID: usize = 3;
const IDLE_BUFFER_RELEASE_MS: u32 = 30_000;
const LIGHT_DURATION: Duration = Duration::from_millis(120);
/// Remaining time left on the current pill when something newer wants the
/// screen: a queued update, or a hover that arms the dismiss-on-hover (a
/// laid-out expanded pill, or the second hover over a compact pill), caps
/// the exit at this, so the user never waits out the full duration to see
/// a change. A morph-origin expanded pill held under the cursor is exempt —
/// its dismissal is deferred while the hold lasts (see `held` in `tick`).
const EARLY_EXIT_MS: u64 = 500;
/// How long a cursor leave is ignored before it counts: boundary jitter
/// must not reverse a fresh hover morph the moment it starts.
const LEAVE_DEBOUNCE: Duration = Duration::from_millis(60);
/// Reversals from less than this progress drop the morph instead of running
/// a spring release (a seeded release would visibly balloon a pill that had
/// barely left compact).
const REVERSAL_MIN_PROGRESS: f32 = 0.05;
/// The follower axis's lag, as a fraction of the leg: the height axis
/// starts its motion this long after the width axis and compresses its
/// curve into the remaining leg, so the card widens before it grows tall.
/// 0.10–0.15 reads as a connected chase; beyond ~0.2 the height visibly
/// detaches from the width.
const MORPH_LAG: f32 = 0.12;

/// The whole-pill settle-bounce: after the size spring passes its endpoint,
/// the entire pill scales about its anchor past the final size and back
/// (expand: 1 -> 1.05 -> 1; compaction: 1 -> 0.95 -> 1), so the
/// bounce reads as one 1:1 card settling instead of per-element overshoots.
/// The compaction leg is the slow half: ζ=0.6's ~9.5 % undershoot spreads
/// over the tail of a leg that runs 4/5 of the entrance duration, so the
/// dip reads as a pronounced settle instead of a thud. The amplitudes are
/// the first tuning knobs if the bounce reads too weak or too wild.
const BOUNCE_OVER: f32 = 0.05;
/// Under-bounce amplitude (compaction only): the pill shrinks to
/// (1 - UNDER) of its final size at the spring's undershoot trough.
const BOUNCE_UNDER: f32 = 0.05;
/// The expand spring's peak progress (ζ = 0.7, 2.8π, from rest): the
/// bounce's excess is normalized against it, so the over-bounce peaks at
/// exactly `BOUNCE_OVER` when the spring peaks. Pinned by
/// `spring_expand_overshoots_then_settles_exactly`.
const EXPAND_SPRING_PEAK: f32 = 1.045988;
/// The collapse spring's undershoot below compact (ζ = 0.6, released from
/// rest): the compaction's dip is normalized against it, so the pill shrinks
/// to exactly (1 - `BOUNCE_UNDER`) at the trough. Pinned by
/// `spring_collapse_release_from_expanded_undershoots_once_then_pins_compact`.
const COLLAPSE_TROUGH: f32 = -0.094780;
/// Tick period while the pill is fully static (no animation, no marquee
/// scrolling). The dismiss countdown and hover polling do not need frame
/// rate; the refresh-rate timer is restored the moment the pill animates or
/// a line scrolls.
const STATIC_TICK_MS: u32 = 250;
/// During steady playback the OS reports position with latency (the value was
/// read a tick or two ago), so a live sample a little behind the displayed
/// position is jitter and must not snap the bar backward. A backward jump far
/// beyond this is a genuine backward seek or a new track starting at 0, and must
/// be adopted. Kept larger than any expected report latency (~2 s poll/event
/// cadence) and far below a track-length jump; the worker's `SEEK_DELTA_SECS`
/// covers seeks independently via a `TrackChanged` re-emit.
const PROGRESS_LATENCY_TOL_SECS: f64 = 3.0;
/// Duration of the PersistentCompact idle-fade ramp (full opacity → 25% idle).
const FADE_DURATION_MS: u64 = 300;

/// Posted by the high-resolution animation timer to drive pill frames.
pub(crate) const TIMER_ANIMATION_MSG: u32 = WM_APP + 6;
/// Posted to the overlay window by the `EVENT_SYSTEM_FOREGROUND` WinEvent hook
/// callback whenever the system foreground window changes. The callback only
/// posts this message; the real re-resolve happens on the UI thread in its
/// `FOREGROUND_CHANGE_MSG` handler, which reuses the existing
/// `sample_foreground` / `effective_work_area` / `reposition` / `render`
/// decisioning. This constant is overlay-internal: it lives next to
/// `TIMER_ANIMATION_MSG`, not in `events.rs` (the `WM_APP + 2` slot there is
/// already taken by the main window's `WM_TRAY` on main_window.rs).
pub(crate) const FOREGROUND_CHANGE_MSG: u32 = WM_APP + 4;

/// Reusable device context + DIB section for the pill's frames. The overlay
/// redraws every animation tick; recreating the DIB per frame is pure waste.
/// Freed automatically on drop (see `Drop` below).
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
unsafe extern "system" fn animation_timer_proc(parameter: *mut c_void, _fired: bool) {
    // A contained panic no-ops the tick; the next tick retries.
    crate::winutil::guarded_void("the animation timer callback", || {
        let hwnd = HWND(parameter);
        unsafe {
            let _ = post_message(hwnd, TIMER_ANIMATION_MSG, WPARAM(0), LPARAM(0));
        }
    });
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

/// What one animation frame looks like, per `frame()`.
struct FrameState {
    /// Window opacity 0..255.
    alpha: u8,
    /// The entrance-grow / exit-shrink progress: `Some` while the pill
    /// morphs between the compact and the expanded shape, `None` when it is
    /// rendered at its plain layout size. Never `Some` for a Compact-layout
    /// pill, which has nothing to grow into.
    morph: Option<MorphProgress>,
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
/// LRU cache: three entries bound the retained cover memory while covering
/// a realistic source mix (music + video + podcast). Retention is indefinite
/// once inserted — a source's last track stays available as a successor
/// while its playback state says it is still playing — so the cap alone
/// bounds the memory (per entry: pill text + one 256² decoded cover).
const TRACK_CACHE_CAP: usize = 3;

/// Bound on the per-source playback-state ledger (`OverlayState.source_state`).
/// Live sources are capped at the worker's admission limits (32 sources), so
/// the ledger never needs more than that for live entries; the headroom
/// covers settled/rejected sources still awaiting eviction. A Stopped entry
/// is inert (never a successor), so evicting a Stopped entry first is always
/// safe.
const LEDGER_STATE_CAP: usize = 64;

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

/// Identity of the fully-composed static "background" layer (everything that
/// does not move during a marquee scroll: aura, body, edge stroke, art tile,
/// progress bar, and the non-scrolling text rows). The marquee tick copies this
/// cached buffer and only re-composites the scrolling row(s), so any change to a
/// background-affecting input must change at least one field here or the pill
/// would render stale. Keep this exhaustive: every value `draw_pixels` /
/// `draw_text_pixels` read that can alter the background must be reflected, or a
/// config/art change will not invalidate the cache. Bumped separately from the
/// structural `content` identity via `content_rev` (palette/art travel with the
/// content, but a content swap also bumps `content_rev`, so both are covered).
#[derive(Clone, Debug, PartialEq)]
struct ChromeKey {
    content_rev: u64,
    compact: bool,
    dpi: u32,
    buf_w: usize,
    buf_h: usize,
    scale: f32,
    bar_w: Option<usize>,
    high_contrast: bool,
    palette: Option<([u8; 4], [u8; 4])>,
    background_color: [u8; 4],
    text_color: [u8; 4],
    accent_color: [u8; 4],
    art_size: f32,
    padding: f32,
    font_size_title: f32,
    font_size_artist: f32,
    corner_radius: f32,
    compact_corner_radius: f32,
    morph: Option<(f32, f32)>,
}

/// The retained static-background raster produced by a `RenderLayer::Background`
/// pass, reused by the marquee tick's `Foreground` pass. The pixels are tightly
/// packed at stride `buf_w * 4` (the same layout `draw_pixels` / `draw_text_pixels`
/// assume), so each marquee tick copies `pixels` over the scratch buffer and
/// composites the scrolling rows on top — no chrome or GDI re-rasterization.
struct ChromeCache {
    key: ChromeKey,
    pixels: Vec<u8>,
}

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

/// Releases a scratch DC + DIB the way every teardown must: unselect the
/// original bitmap, delete the DIB, then release the DC. Deleting a DC while
/// a bitmap is selected (or a bitmap while it is selected into a DC) is
/// undefined, so the order is the contract — shared by the `DibCache` and
/// `TextScratch` `Drop` impls instead of being re-written at each free site.
fn release_dib(hdc: HDC, bitmap: HBITMAP, old_bitmap: HGDIOBJ) {
    unsafe {
        let _ = select_object(hdc, old_bitmap);
        let _ = delete_object(bitmap);
        let _ = DeleteDC(hdc);
    }
}

impl Drop for DibCache {
    fn drop(&mut self) {
        release_dib(self.hdc, self.bitmap, self.old_bitmap);
    }
}

impl Drop for TextScratch {
    fn drop(&mut self) {
        release_dib(self.hdc, self.bitmap, self.old_bitmap);
    }
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
    /// The identity-stable palette the SMTC worker attached to the current
    /// track, if any. `palette` is derived from it (when present) instead of
    /// a fresh per-frame derivation, so a source that re-encodes its
    /// thumbnail between reads — different bytes, same cover — can never
    /// shift the pill's accent colors mid-session.
    content_palette: Option<Palette>,
    last_track: Option<TrackInfo>,
    phase: Phase,
    dismiss_at: Option<Instant>,
    /// When the cursor hovers over the pill, the dismiss deadline is
    /// shortened to 500ms. The arm is one-way: the pill dismisses 500ms
    /// after the hover is first detected even if the cursor leaves before
    /// then. The flag also stops the tick from re-arming (which would keep
    /// pushing the deadline forward while the cursor stays put).
    hover_dismiss_at: Option<Instant>,
    /// The in-place compact→expanded hover morph, while one is in flight or
    /// pinned (see `HoverExpand`). `Some` with `done` keeps rendering the
    /// expanded pill after the animation; `hide`, a fresh show, and a layout
    /// push clear it.
    hover_expand: Option<HoverExpand>,
    /// Whether the pill already expanded via hover during this showing. With
    /// `dismiss_on_hover` enabled, the first hover over the compact pill
    /// expands and later hovers dismiss instead (the second hover
    /// dismisses); while dismiss-on-hover is off the flag is ignored and
    /// every hover re-expands. Reset on every show, so each notification
    /// gets its own expansion.
    hover_expanded_once: bool,
    /// When the cursor left the pill, while the leave is still within the
    /// debounce window (see `LEAVE_DEBOUNCE`). A leave is only acted on once
    /// it has held for the window, so boundary jitter cannot cancel a morph
    /// the moment it starts; re-entering clears it.
    hover_leave_at: Option<Instant>,
    /// The in-place content cross-fade, while one is in flight (see
    /// `ContentFade`).
    content_fade: Option<ContentFade>,
    /// Dimensions of the last rendered frame — the cross-fade snapshots the
    /// frame buffer at exactly this size.
    last_frame_w: usize,
    last_frame_h: usize,
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
    /// Fullscreen verdict of the most recent `sample_foreground` call, used
    /// to detect a verdict change on an unchanged foreground window (a
    /// same-window fullscreen toggle fires no WinEvent hook). Updated on
    /// every sample, test verdicts included.
    last_fullscreen: Option<bool>,
    /// While the pill is auto-hidden with held content (fullscreen/listed
    /// foreground), a coarse 1 Hz timer keeps polling the foreground — see
    /// `tick_hidden_watchdog`. False while the pill is visible or hidden
    /// without a hold.
    hidden_watchdog: bool,
    /// Per-row marquee state for the four track lines (title/subtitle/meta/app).
    scroll: [LineScroll; 4],
    /// Per-row cached marquee rasters (parallel to `scroll`), see `MarqueeStrip`.
    marquee_strips: [Option<MarqueeStrip>; 4],
    /// High-resolution timer driving the pill animation.
    /// Animation timer from the timer queue; when creation fails, a plain
    /// window timer with `ANIM_TIMER_ID` drives the animation instead.
    anim_timer: HANDLE,
    anim_timer_fallback: bool,
    /// Animation tick period in ms, capped to `config.overlay.max_tick_hz`
    /// (default 60 Hz). Re-detected on every show; the timer is recreated only
    /// when it changes.
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
    /// Estimated live playback position (seconds), advanced each animation
    /// tick from `progress_anchor`. None when the source reports no position.
    estimated_position_secs: Option<f64>,
    /// Total duration (seconds) of the current track. None when not reported.
    progress_duration_secs: Option<u64>,
    /// Playback rate for position estimation. None when not reported.
    progress_rate: Option<f64>,
    /// (anchor instant, anchor position) the estimate integrates from.
    progress_anchor: Option<(Instant, f64)>,
    /// Whether the current content is playing (drives freeze/resume).
    progress_playing: bool,
    /// Last SMTC-reported position seen by `apply_progress`. Used to detect
    /// stale samples: when the OS has not advanced the position since the last
    /// read (apps that refresh SMTC position every few seconds, not every poll),
    /// the bar must keep interpolating instead of snapping back to the stale
    /// value. A genuinely fresh backward jump (seek / new track) is still adopted.
    last_progress_position_secs: Option<f64>,
    /// Bar fraction painted on the last frame, so a settled pill can skip a
    /// static-tick repaint when the bar did not move by at least a pixel.
    last_bar_fraction: Option<f32>,
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
    /// Retained static-background raster (chrome + non-scrolling text) for the
    /// current content, used to skip the expensive per-frame chrome re-render
    /// while a marquee line scrolls. `None` until the first background pass;
    /// invalidated by any `ChromeKey` change (see `chrome_cache_key`).
    chrome_cache: Option<ChromeCache>,
    /// Bumped whenever `content` is replaced or cleared, so the chrome cache is
    /// invalidated across content swaps. Palette/art travel with the content, so
    /// this also covers cover changes.
    content_rev: u64,
    /// The layer the current render pass should draw (set by `render_layered`
    /// before the text draw). Read by the text-drawing helpers so the marquee
    /// `Foreground` pass only re-composites the scrolling rows.
    render_layer: render::RenderLayer,
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
    /// Handle of the `EVENT_SYSTEM_FOREGROUND` WinEvent hook, installed once in
    /// `create_window` and unhooked in `WM_NCDESTROY`. `None` if the hook could
    /// not be installed (foreground repositioning then degrades to the 250 ms
    /// static tick; the overlay still functions).
    hook: Option<HWINEVENTHOOK>,
    /// Last effective anchor rectangle (`rcWork` or `rcMonitor`) resolved by
    /// `on_foreground_change`. Captures the last settled anchor so a foreground
    /// switch that did not actually move it (e.g. Alt-Tab between two normal
    /// apps on the same monitor) is skipped instead of issuing a redundant
    /// `SetWindowPos`. Maintained only on the foreground-change path: a stale
    /// value from another path (e.g. `WM_DISPLAYCHANGE`) can at most cause one
    /// extra move, never a misplacement, since every reposition recomputes the
    /// anchor from scratch.
    last_anchor_edge: Option<RECT>,
    /// Whether the persistent-compact pill is currently in the faded (idle)
    /// state. The alpha drops to the idle level (0.25 * 255 = 64) after the
    /// dismiss timeout. Reset on hover, track change, or playback change.
    persistent_faded: bool,
    /// `auto_compact_sources` list) is foreground. Saved here before
    /// `hide()` clears `content`, so `on_foreground_change` can restore it
    /// on the resume path without depending on the queue.
    held_content: Option<MediaEvent>,
    /// Shared cell the SMTC worker reads for its session-recreation gate
    /// (see `smtc::ListenerState::now_showing`): the source of the pill
    /// content most recently displayed. Set on every content display; never
    /// cleared on dismiss — the pill's last content is still what the user
    /// last saw, so suppressing a re-report of it stays correct. Attached
    /// by `create_window`; `None` in tests.
    now_showing: Option<Arc<Mutex<Option<String>>>>,
    /// Current track as the pill's accessible name, kept in a shared cell so
    /// the read-only UIA name provider (which UIA core may call from any
    /// thread, and which can outlive the window) never dereferences window
    /// state: the UI thread writes the pill text here on every content
    /// change (`resolve_pill_text`) and clears it on hide; the provider only
    /// ever reads this cell. `None` in tests (no provider is ever built for
    /// a test state).
    pill_name: Option<Arc<Mutex<Option<String>>>>,
    /// When true (PersistentCompact + hide_for_auto_compact_sources, foreground
    /// is fullscreen/listed), the pill collapses to fully hidden on its normal
    /// dismiss instead of fading to idle opacity. Set in `show_with_duration`
    /// and re-evaluated by `on_foreground_change` on every foreground switch.
    persistent_collapse_on_dismiss: bool,
    /// Cached result of `is_cursor_over_pill()` from the last animation tick,
    /// so `held_expanded()` (called from `receive_events` between ticks) can
    /// skip the display enumeration the cursor poll triggers. Updated every
    /// tick; stale by at most one tick period (250 ms static, ~16 ms animated).
    last_cursor_over_pill: bool,
    /// Source app of the last TrackChanged shown, used as the label fallback
    /// in state pills for current-session playback states so the pill always
    /// names the app that owns the media — never another app's last track.
    current_source: Option<String>,
    /// Per-source track cache: the last TrackChanged shown for each source app,
    /// so that a later PlaybackStateChanged for that source can render the
    /// correct track info instead of the most-recently-shown app's track, and
    /// a retired source can be succeeded by a source that is still playing.
    /// Entries hold the pill text and decoded cover (raw artwork stripped at
    /// insert — see `cache_track`). LRU-ordered and bounded by
    /// `TRACK_CACHE_CAP` alone: retention is indefinite once inserted, so a
    /// source that stops playing drops out only when the cap evicts it.
    track_cache: HashMap<String, TrackInfo>,
    /// Last known playback state per source app, so successor selection knows
    /// which cached tracks belong to sources that are *actually playing* (see
    /// `best_successor`). Fed by TrackChanged snapshots (only when the
    /// snapshot carries a state — a transitional `None` never downgrades a
    /// source), PlaybackStateChanged events, and retirement (a settled,
    /// allow-list-removed or churn-excluded source is marked Stopped and can
    /// never surface again until it emits a new event). Bounded by
    /// `LEDGER_STATE_CAP`, evicting Stopped entries first.
    source_state: HashMap<String, PlaybackState>,
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
    /// Test-only: forces `is_cursor_over_pill` to a fixed answer, so `tick()`
    /// can be driven deterministically without polling the real cursor.
    #[cfg(test)]
    test_cursor_over: Option<bool>,
    /// Test-only: when set, `sample_foreground` returns this verdict instead
    /// of polling the real foreground window, so layout/hide decisions can be
    /// tested deterministically.
    #[cfg(test)]
    test_fg_verdict: Option<ForegroundVerdict>,
    /// Test-only: counts `render()` entries, so a tick-level test can assert
    /// that the tick which starts a hover morph renders on that same tick.
    #[cfg(test)]
    render_count: u32,
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
        // A layout change redefines what the pill is mid-morph: drop any
        // in-flight hover expansion (and the content cross-fade, whose
        // snapshot no longer matches) so the render starts from the newly
        // applied layout.
        state.hover_expand = None;
        state.hover_expanded_once = false;
        state.content_fade = None;
        // Reset persistent-compact state: switching away from (or to) the
        // layout must not leave a stale faded/collapse flag that would
        // affect the next dismiss cycle.
        state.persistent_faded = false;
        state.persistent_collapse_on_dismiss = false;
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
            "overlay compact_position_separate set to {separate} ({})",
            if separate {
                "compact position: independent"
            } else {
                "compact position: follows expanded"
            }
        );
        if !state.preview_if_hidden() {
            state.render();
        }
    }
}

/// Pushes the dismiss-on-hover setting to the live overlay (which keeps its
/// own config snapshot). Nothing visual changes until the next hover tick,
/// so no re-render or preview is needed here.
pub(crate) fn set_dismiss_on_hover(hwnd: HWND, enabled: bool) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = window_state::<OverlayState>(hwnd);
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        state.config.overlay.dismiss_on_hover = enabled;
        info!("overlay dismiss_on_hover set to {enabled}");
    }
}

/// Pushes the expand-compact-on-hover setting to the live overlay (which
/// keeps its own config snapshot). Nothing visual changes until the next
/// hover tick, so no re-render or preview is needed here.
pub(crate) fn set_expand_compact_on_hover(hwnd: HWND, enabled: bool) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = window_state::<OverlayState>(hwnd);
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        state.config.overlay.expand_compact_on_hover = enabled;
        info!("overlay expand_compact_on_hover set to {enabled}");
    }
}

/// Pushes the persistent-pill idle-fade toggle to the live overlay (which
/// keeps its own config snapshot). A pill that is currently faded returns
/// to full opacity immediately, so the change is visible right away; a
/// hidden pill stays hidden (no preview — the setting only affects what
/// happens once the next dismiss deadline fires).
pub(crate) fn set_fade_persistent_pill(hwnd: HWND, enabled: bool) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = window_state::<OverlayState>(hwnd);
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        state.config.overlay.fade_persistent_pill = enabled;
        state.persistent_faded = false;
        info!("overlay fade_persistent_pill set to {enabled}");
        if !matches!(state.phase, Phase::Hidden | Phase::Collapsing(_)) {
            state.render();
        }
    }
}

/// Pushes the pinned-source pattern to the live overlay (which keeps its own
/// config snapshot). Trims the pattern and treats an empty value as no pin —
/// the same normalization `Config::normalize` applies to hand-edited configs,
/// so a pin cleared in the settings UI and a cleared hand-edited config agree.
/// Nothing changes visually until the next dismiss deadline: the pin only
/// decides what the persistent pill returns to after a dismiss
/// (`try_return_to_pinned`), so no re-render or preview is needed here.
pub(crate) fn set_pinned_source(hwnd: HWND, pin: Option<String>) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = window_state::<OverlayState>(hwnd);
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        let pin = pin.map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
        state.config.behavior.pinned_source = pin;
        info!("overlay pinned_source set to {:?}", state.config.behavior.pinned_source);
    }
}

/// Pushes the hide-for-auto-compact-sources setting to the live overlay.
/// The next foreground change evaluates the new value.
pub(crate) fn set_hide_for_auto_compact_sources(hwnd: HWND, enabled: bool) {
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let state_ptr = window_state::<OverlayState>(hwnd);
        if state_ptr.is_null() {
            return;
        }
        let state = &mut *state_ptr;
        state.config.behavior.hide_for_auto_compact_sources = enabled;
        info!("overlay hide_for_auto_compact_sources set to {enabled}");
    }
}

/// Whether a source app matches the pinned-source pattern, using the same
/// identity rules as the media-sources allow-list (`normalize_for_match`):
/// the pattern or the source contains the other, so a stored "Spotify.exe"
/// matches a session label "spotify" and vice versa (the same bidirectional
/// rule the picker's pre-check uses, so a row the picker shows as checked
/// always matches at runtime). An empty pattern never matches. Shared with
/// the process picker, which uses it to restrict the pinned-source row set
/// to the user's allowed sources.
pub(crate) fn source_matches_pin(source: &str, pin: &str) -> bool {
    let nsource = crate::smtc::normalize_for_match(source);
    let npin = crate::smtc::normalize_for_match(pin);
    // Both sides must be non-empty: the bidirectional contains rule would
    // otherwise let an empty source or pin match everything.
    !nsource.is_empty() && !npin.is_empty() && (nsource.contains(&npin) || npin.contains(&nsource))
}

/// Whether the pill's fonts must be rebuilt because the resolved target
/// monitor's DPI differs from the DPI the current `FontProvider` was built
/// for. On a mismatch every size derived from the fonts (layout, DIB, hitbox)
/// is stale for the target, so a plain move would leave a wrong-size pill.
/// `fonts_dpi` is 0 before the first render (see `OverlayState::new`), which
/// also counts as "must render".
fn needs_font_rebuild(fonts_dpi: u32, target_dpi: u32) -> bool {
    fonts_dpi != target_dpi
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
            content_palette: None,
            last_track: None,
            phase: Phase::Hidden,
            dismiss_at: None,
            hover_dismiss_at: None,
            hover_expand: None,
            hover_expanded_once: false,
            hover_leave_at: None,
            persistent_faded: false,
            persistent_collapse_on_dismiss: false,
            content_fade: None,
            last_frame_w: 0,
            last_frame_h: 0,
            position,
            compact_position,
            // Every show path re-resolves the layout before the first frame
            // (see `show_with_duration`), so this initial value is only a
            // placeholder until then.
            layout: LayoutMode::Expanded,
            layout_fg: None,
            layout_fg_exe: None,
            last_geometry_check: None,
            last_fullscreen: None,
            hidden_watchdog: false,
            scroll: [LineScroll::default(); 4],
            marquee_strips: [None, None, None, None],
            anim_timer: HANDLE::default(),
            anim_timer_fallback: false,
            tick_period: 16,
            decoded_art: None,
            decoded_art_source: None,
            palette: None,
            estimated_position_secs: None,
            progress_duration_secs: None,
            progress_rate: None,
            progress_anchor: None,
            progress_playing: false,
            last_progress_position_secs: None,
            last_bar_fraction: None,
            dib: None,
            frame_scratch: Vec::new(),
            chrome_cache: None,
            content_rev: 0,
            render_layer: render::RenderLayer::Full,
            last_tick: Instant::now(),
            last_reassert: None,
            period_cache: None,
            wake: Arc::new(AtomicBool::new(false)),
            hook: None,
            last_anchor_edge: None,
            held_content: None,
            now_showing: None,
            pill_name: None,
            last_cursor_over_pill: false,
            current_source: None,
            track_cache: HashMap::new(),
            source_state: HashMap::new(),
            track_cache_order: VecDeque::new(),
            pill_text: None,
            text_scratch: None,
            scratch_utf16: Vec::new(),
            fonts: FontProvider::new(0),
            aura_inset: 0,
            #[cfg(test)]
            test_cursor_over: None,
            #[cfg(test)]
            test_fg_verdict: None,
            #[cfg(test)]
            render_count: 0,
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
        // The worker's identity-stable palette wins when present: a re-encoded
        // thumbnail re-converts the buffer (different bytes, same cover) but
        // must not shift the accent colors. Without one (state pills, artless
        // tracks) the palette is derived from the converted buffer as before.
        self.palette = self
            .content_palette
            .or_else(|| self.decoded_art.as_deref().and_then(crate::palette::palette_from_rgba));
    }

    /// The key identifying one static-background raster: every input that can
    /// change the geometry or the non-scrolling text rows. A marquee tick whose
    /// computed key matches the cached `ChromeCache` can skip the chrome and
    /// the static-text GDI entirely and only re-composite the scrolling rows.
    /// `content_rev` carries the structural content identity (title/artist/etc.
    /// are not hashed here); palette and art travel with the content, so they
    /// are covered by both `content_rev` and the `palette` tuple.
    fn chrome_cache_key(
        &self,
        buf_w: usize,
        buf_h: usize,
        dpi: u32,
        scale: f32,
        compact: bool,
        morph: Option<MorphProgress>,
    ) -> ChromeKey {
        let a = &self.config.appearance;
        // The bar width must use the exact draw formula (`render::bar_pixel_w`,
        // shared with `draw_pixels`): the key has to change on the pixel step
        // the draw takes and not before, or the cache drifts silently either
        // way.
        let pill_w = buf_w.saturating_sub(self.aura_inset as usize * 2);
        ChromeKey {
            content_rev: self.content_rev,
            compact,
            dpi,
            buf_w,
            buf_h,
            scale,
            bar_w: render::bar_pixel_w(self.estimated_position_secs, self.progress_duration_secs, pill_w),
            high_contrast: crate::winutil::system_preferences().high_contrast,
            palette: self.palette.map(|p| (p.primary, p.secondary)),
            background_color: a.background_color,
            text_color: a.text_color,
            accent_color: a.accent_color,
            art_size: a.art_size as f32,
            padding: a.padding,
            font_size_title: a.font_size_title,
            font_size_artist: a.font_size_artist,
            corner_radius: a.corner_radius,
            compact_corner_radius: a.compact_corner_radius,
            morph: morph.map(|m| (m.width, m.height)),
        }
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
        if unsafe { set_timer(self.hwnd, ANIM_TIMER_ID, self.tick_period, None) } != 0 {
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
        let animating = !matches!(self.phase, Phase::Shown)
            || self.hover_expand.is_some()
            || self.content_fade.is_some()
            || self.persistent_fade_active();
        let marquee_active = self.scroll.iter().any(|line| line.scrolling);
        let now = Instant::now();
        let raw = if animating || marquee_active {
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
        let period = if animating || marquee_active {
            // `max_tick_hz` caps the repaint rate; the raw monitor period is
            // raised to at least the cap's period so, e.g., a 144 Hz display
            // still animates at most at the configured Hz. Motion is
            // time-based (driven by `dt`), so only the frame count changes.
            let hz = self.config.overlay.max_tick_hz.unwrap_or(60).clamp(60, 1000);
            let cap_ms = (1000u32 / hz).max(1);
            raw.max(cap_ms).clamp(1, 100)
        } else {
            raw
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

    /// Persistent-compact pill currently active: showing, or auto-hidden
    /// with held content. Must be computed per event — a batch can cross
    /// the first-show boundary mid-batch (the first event shows the pill,
    /// changing the phase), so a snapshot taken before the loop would go
    /// stale and re-queue events behind a pill that never collapses.
    fn persistent_active(&self) -> bool {
        self.config.overlay.layout == LayoutMode::PersistentCompact
            && (!matches!(self.phase, Phase::Hidden) || self.persistent_auto_hidden())
    }

    /// PersistentCompact auto-hide state: hidden for a fullscreen/listed
    /// foreground, with content held for the resume. In this state the pill
    /// is still active — events must update the held content in place rather
    /// than queue behind it, or the resume would re-show a stale track.
    fn persistent_auto_hidden(&self) -> bool {
        self.config.overlay.layout == LayoutMode::PersistentCompact
            && matches!(self.phase, Phase::Hidden)
            && self.held_content.is_some()
    }

    /// PersistentCompact before the first show of this run: hidden with
    /// nothing held. The batch's first event must show directly — a queued
    /// event would strand forever, because the pill never collapses and
    /// show_next only drains while Hidden.
    fn persistent_pre_first_show(&self) -> bool {
        self.config.overlay.layout == LayoutMode::PersistentCompact
            && matches!(self.phase, Phase::Hidden)
            && self.held_content.is_none()
    }

    /// Whether the PersistentCompact idle-fade ramp is currently in progress.
    /// Returns true only when the pill has already entered the faded (idle)
    /// state, is not in the collapse-on-dismiss mode (fullscreen/listed
    /// foreground), and the fade ramp hasn't elapsed yet.
    fn persistent_fade_active(&self) -> bool {
        self.config.overlay.layout == LayoutMode::PersistentCompact
            && self.config.overlay.fade_persistent_pill
            && self.persistent_faded
            && !self.persistent_collapse_on_dismiss
            && self
                .dismiss_at
                .is_some_and(|d| d.elapsed() < Duration::from_millis(FADE_DURATION_MS))
    }

    fn delete_anim_timer(&mut self) {
        if !self.anim_timer.0.is_null() {
            // Wait for any in-flight callback to complete
            // (INVALID_HANDLE_VALUE): the callback is a single non-blocking
            // PostMessageW, so the wait cannot deadlock, and no stale tick
            // message can be posted to a window that is being torn down.
            unsafe {
                let _ = crate::winapi::delete_timer_queue_timer(None, self.anim_timer, Some(INVALID_HANDLE_VALUE));
            }
            self.anim_timer = HANDLE::default();
        }
        if self.anim_timer_fallback {
            unsafe {
                let _ = kill_timer(self.hwnd, ANIM_TIMER_ID);
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
        // Persistent-compact never collapses to Hidden on its own, so its
        // pending queue (drained only while hidden) would hold events
        // forever — nothing would ever show them. While such a pill is
        // active — showing, or auto-hidden with held content — any event,
        // same or cross-source, updates it in place, and the first event
        // of a run shows directly. The queue remains for the notification
        // layouts, where pills still collapse and drain it.
        // The queue carries Arc<MediaEvent> so the fan-out to both windows
        // never copies the event; recover the owned event here (zero-copy
        // when this window is the last holder, a clone otherwise).
        for event in batch.into_iter().map(media_event_into_owned) {
            // Playback-state ledger: remember the last known state per source
            // so successor selection only ever announces sources that are
            // actually playing (see `best_successor`). Updated even while
            // notifications are disabled — the ledger is liveness truth, not
            // display policy. A TrackChanged whose snapshot carries no state
            // (transitional read) leaves the existing entry untouched rather
            // than downgrading a playing source.
            match &event {
                MediaEvent::TrackChanged(track) => {
                    if let Some(state) = track.playback_state {
                        self.remember_source_state(&track.source_app, state);
                    }
                }
                MediaEvent::PlaybackStateChanged(state, source) => {
                    self.remember_source_state(source, *state);
                }
                _ => {}
            }
            // A source retired from the allow-list must drop its content
            // even while notifications are disabled: hygiene for the next
            // show, not a notification.
            if let MediaEvent::SessionRejected { source_app, .. } = &event {
                self.retire_source(source_app);
                continue;
            }
            // A source whose sessions all settled is gone for good. Its
            // terminal Stopped (if any) was already delivered moments earlier
            // in the same sync; this event is the cleanup that must land even
            // while notifications are disabled — otherwise the fast-path
            // restore on a later re-enable would surface the source's stale
            // last track (the worker-side caches are pruned at settle, so
            // nothing corrects it from below).
            if let MediaEvent::SourceGone { source_app } = &event {
                self.retire_source_gone(source_app);
                continue;
            }
            if !self.enabled {
                // Notifications off: the ledger above stays live, and so does
                // the track cache — the re-enable fast path restores the
                // pinned source's *current* track, not the one cached before
                // the disable. Same write discipline as the display paths
                // (`cache_track` dedups by source and enforces the cap); only
                // the cache is touched, nothing is shown or queued.
                if let MediaEvent::TrackChanged(track) = &event {
                    self.cache_track(track);
                }
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
                    // While a pill is up, a newer TrackChanged from the same
                    // source swaps the content in place instead of enqueueing
                    // behind the visible one — otherwise rapid skip-next/prev
                    // leaves the pill on the oldest queued track, showing a
                    // stale title/art/duration while a different track plays.
                    // Cross-source changes still queue.
                    let same_source_shown = !matches!(self.phase, Phase::Hidden)
                        && self.content.as_ref().is_some_and(|content| match content {
                            MediaEvent::TrackChanged(shown) => shown.source_app == track.source_app,
                            MediaEvent::PlaybackStateChanged(_, source) => source == &track.source_app,
                            _ => false,
                        });
                    if is_update {
                        self.current_source = Some(track.source_app.clone());
                        self.last_track = Some(track.clone());
                        self.cache_track(&track);
                        self.update_content(MediaEvent::TrackChanged(track), update_min_duration(&self.config));
                    } else if self.held_expanded() || same_source_shown || self.persistent_active() {
                        // A new track while the cursor holds an expanded pill, or
                        // a newer track from the same source arriving while any
                        // pill is up, swaps the content in place instead of
                        // queueing behind the visible one. Full duration from the
                        // swap, so leaving later gives the new content its normal
                        // time; update_content revives a collapsing pill so the
                        // latest track always reads on screen.
                        self.current_source = Some(track.source_app.clone());
                        self.last_track = Some(track.clone());
                        self.cache_track(&track);
                        let full = Duration::from_millis(self.config.overlay.duration_ms.max(500));
                        self.update_content(MediaEvent::TrackChanged(track), full);
                    } else if self.persistent_pre_first_show() {
                        // Persistent-compact before its first show of the
                        // run: show this event directly. A queued event
                        // would strand — the pill never collapses, so
                        // show_next could never drain it. Later events in
                        // the same batch take the persistent_active path.
                        self.current_source = Some(track.source_app.clone());
                        self.last_track = Some(track.clone());
                        self.cache_track(&track);
                        self.show(MediaEvent::TrackChanged(track), true);
                    } else {
                        self.enqueue(MediaEvent::TrackChanged(track));
                    }
                }
                MediaEvent::ProgressChanged {
                    position_secs,
                    duration_secs,
                    playback_rate,
                    source_app,
                } => {
                    // A live position update only re-anchors the progress bar; it
                    // never announces a pill or changes the active content (which
                    // stays the last TrackChanged/PlaybackStateChanged shown).
                    // Apply it only when it belongs to the content on screen: the
                    // worker pushes a timeline refresh for every tracked session
                    // every ~2s, so without this gate a different source's
                    // advancing position would drive the seekbar under this
                    // source's pill (e.g. a paused YouTube Music card while a
                    // Brave playback runs in the background).
                    let matches_shown = self.content.as_ref().is_some_and(|content| match content {
                        MediaEvent::TrackChanged(shown) => shown.source_app == source_app,
                        MediaEvent::PlaybackStateChanged(_, source) => source == &source_app,
                        _ => false,
                    });
                    if matches_shown {
                        self.apply_progress(position_secs, duration_secs, playback_rate);
                        // The static tick that drives a settled pill does not repaint
                        // (see `tick`'s render gate), so a live position update — and a
                        // seek it re-anchors — would otherwise stay stale on screen
                        // until the next content-driven render. Paint the re-based bar
                        // right away while the pill is up; skip it when hidden so a
                        // dismissed pill never gets dragged back to life by a late event.
                        if !matches!(self.phase, Phase::Hidden) {
                            self.render();
                        }
                    }
                }
                MediaEvent::PlaybackStateChanged(state, source_app)
                    if self.config.behavior.enable_playback_state_change =>
                {
                    // Persistent-compact: a Stopped that does not belong to
                    // the source the pill is showing (or holding while
                    // auto-hidden) is dropped. The in-place swap below would
                    // otherwise put a dead ⏹ pill on screen whenever a
                    // background source closes — and with nothing showing, a
                    // terminal Stopped has nothing to retire, so it must not
                    // flash a pill either.
                    let shows_source = self
                        .content
                        .as_ref()
                        .or(self.held_content.as_ref())
                        .and_then(|content| match content {
                            MediaEvent::TrackChanged(track) => Some(track.source_app.as_str()),
                            MediaEvent::PlaybackStateChanged(_, source) => Some(source.as_str()),
                            _ => None,
                        });
                    if matches!(state, PlaybackState::Stopped)
                        && self.config.overlay.layout == LayoutMode::PersistentCompact
                        && shows_source != Some(source_app.as_str())
                    {
                        debug!(
                            "playback state pill dropped | reason=cross-source Stopped (persistent pill shows another source) | source={source_app}"
                        );
                        continue;
                    }
                    // Suppress a redundant PlaybackStateChanged pill when:
                    //  - A TrackChanged for the same source is in this batch
                    //    (see the pre-scan above: the state pill would render the
                    //    source's previously cached track) or already queued (a
                    //    TrackChanged pill is about to show; a redundant
                    //    PlaybackStateChanged would flash the same info).
                    //  - The pill on screen is this source's track pill AND the
                    //    state is Playing: the track pill already carries the
                    //    music-note "now playing" symbol, so re-announcing
                    //    Playing adds nothing.
                    //  - It is Playing from a source whose track was recently
                    //    shown but whose track pill has already dismissed (the
                    //    `replaying` guard below; prevents the "replaying" pill
                    //    after session recreation, or when a browser video
                    //    triggers YTM to re-report "Playing").
                    // Paused/Stopped from a source whose track pill is currently
                    // shown do NOT get dropped: they refresh that pill in place
                    // (symbol ♪ -> ⏸/⏹) so a pause right after a track change is
                    // still surfaced. Paused/Stopped from a not-currently-shown
                    // source pass through as a fresh pill.
                    let track_wins = track_sources.iter().any(|s| s == &source_app)
                        || self
                            .pending
                            .iter()
                            .any(|e| matches!(e, MediaEvent::TrackChanged(t) if t.source_app == source_app));
                    let track_pill_shown = matches!(
                        self.content.as_ref(),
                        Some(MediaEvent::TrackChanged(t)) if t.source_app == source_app
                    );
                    if track_wins {
                        // A TrackChanged for this source is in this batch or
                        // already queued behind the current pill: the upcoming
                        // track announcement supersedes the state, so suppress
                        // it (it would flash the source's previously cached
                        // track before the track pill shows).
                        debug!(
                            "playback state pill suppressed | reason=track wins for same source | source={source_app}"
                        );
                        continue;
                    }
                    if track_pill_shown && matches!(state, PlaybackState::Playing) {
                        // The track pill already carries the music-note "now playing"
                        // symbol, so a Playing re-announcement for the same source
                        // adds nothing the pill does not already show.
                        debug!(
                            "playback state pill suppressed | reason=now-playing re-announced | source={source_app}"
                        );
                        continue;
                    }
                    if track_pill_shown {
                        // A genuine Paused/Stopped while the track announcement is
                        // still on screen must not be lost to the dismiss timer:
                        // refresh the current pill in place instead. The cached
                        // track for this source supplies the title/artist; only
                        // the symbol flips (♪ -> ⏸/⏹) and the dismiss clock
                        // resets to the full configured duration from this change.
                        let event = MediaEvent::PlaybackStateChanged(state, source_app);
                        let full = Duration::from_millis(self.config.overlay.duration_ms.max(500));
                        self.update_content(event, full);
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
                    if self.persistent_active() {
                        // A cross-source state while a persistent pill is up:
                        // the queue would never drain (the pill fades to idle,
                        // it never hides), so swap the state in place exactly
                        // like the same-source state toggle above.
                        let event = MediaEvent::PlaybackStateChanged(state, source_app);
                        let full = Duration::from_millis(self.config.overlay.duration_ms.max(500));
                        self.update_content(event, full);
                        continue;
                    }
                    if self.persistent_pre_first_show() {
                        // Mirror the track branch: the first state event of
                        // the run shows directly instead of queueing.
                        self.show(MediaEvent::PlaybackStateChanged(state, source_app), false);
                        continue;
                    }
                    self.enqueue(MediaEvent::PlaybackStateChanged(state, source_app));
                }
                MediaEvent::TrackChanged(_) | MediaEvent::PlaybackStateChanged(_, _) => {}
                // Rejected sessions, settled sources, worker failures and the
                // budget-drop warning are history/tray-only: never shown as a
                // pill.
                MediaEvent::SessionRejected { .. }
                | MediaEvent::SourceGone { .. }
                | MediaEvent::WorkerFailed { .. }
                | MediaEvent::ArtworkBudgetExceeded => {}
            }
        }
        if !self.pending.is_empty() {
            // A newer notification is waiting: the pill on screen is never
            // pulled early — the queue advances when it collapses. The tick
            // caps the remaining time (EARLY_EXIT_MS) once the pill is no
            // longer held, so the queued notification shows promptly.
            unsafe {
                let _ = kill_timer(self.hwnd, TIMER_DEBOUNCE);
                set_timer(
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

    /// Caches the current track for a source, moving the source to the back
    /// of the recency order and evicting the oldest entry when the cap is
    /// exceeded. Written by the display paths for the track they show, and —
    /// while notifications are disabled — by `receive_events` for every
    /// incoming `TrackChanged`, so the re-enable restore serves the current
    /// track rather than the pre-disable one. A state pill for an evicted
    /// source falls back to the source-name layout — the accepted degradation
    /// for a source that has not played in a long time.
    fn cache_track(&mut self, track: &TrackInfo) {
        let source = track.source_app.clone();
        // Move the source to the back of the recency order. The keys are
        // unique, so a single retain pass (instead of position + remove)
        // dedups in one traversal; it also self-heals a hypothetical
        // duplicate marker.
        self.track_cache_order.retain(|s| *s != source);
        self.track_cache_order.push_back(source.clone());
        // Insert first so the cap sweep below sees the fresh entry: a
        // brand-new source must never look like an eviction candidate.
        let mut cached = track.clone();
        // The cache only ever serves pill text and the decoded cover; nothing
        // reads the raw artwork bytes from it. Stripping them keeps the raw
        // cover (typically 50-500 KB) from being retained per source after
        // that source stops playing.
        cached.artwork = None;
        self.track_cache.insert(source, cached);
        // Lazy cap sweep: evict the oldest entries until the cap is met. Only
        // runs here (on insert). Entries are otherwise retained indefinitely
        // — a state pill is never robbed of a cached track by a timeout.
        while self.track_cache_order.len() > TRACK_CACHE_CAP {
            let front = self
                .track_cache_order
                .pop_front()
                .expect("the recency order stays in lockstep with the cache");
            self.track_cache.remove(&front);
        }
    }

    /// Records a source's last known playback state, evicting a Stopped
    /// entry first when the ledger cap is exceeded. Stopped entries are inert
    /// (never successors), so evicting one never drops live state; if the
    /// ledger somehow exceeds the cap with no Stopped entry (the worker's
    /// admission caps keep live sources far below `LEDGER_STATE_CAP`), any
    /// entry is evicted as a defense.
    fn remember_source_state(&mut self, source: &str, state: PlaybackState) {
        self.source_state.insert(source.to_owned(), state);
        if self.source_state.len() <= LEDGER_STATE_CAP {
            return;
        }
        let evict = self
            .source_state
            .iter()
            .find(|(_, s)| **s == PlaybackState::Stopped)
            .map(|(key, _)| key.clone())
            .or_else(|| self.source_state.keys().next().cloned())
            .expect("the ledger is non-empty past the cap");
        self.source_state.remove(&evict);
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
            MediaEvent::SessionRejected { .. }
            | MediaEvent::SourceGone { .. }
            | MediaEvent::WorkerFailed { .. }
            | MediaEvent::ArtworkBudgetExceeded
            | MediaEvent::ProgressChanged { .. } => {}
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
            MediaEvent::WorkerFailed { .. } | MediaEvent::ProgressChanged { .. } => {
                debug!("worker-failed event reached the pill queue; ignoring");
            }
            MediaEvent::SourceGone { .. } => {
                debug!("source-gone event reached the pill queue; ignoring");
            }
            MediaEvent::ArtworkBudgetExceeded => {
                debug!("artwork-budget event reached the pill queue; ignoring");
            }
        }
    }
    fn flush_pending(&mut self) {
        unsafe {
            let _ = kill_timer(self.hwnd, TIMER_DEBOUNCE);
        }
        // While a pill is on screen the queue waits: the next event shows when
        // the current one collapses, so notifications never clobber each other.
        if !matches!(self.phase, Phase::Hidden) {
            return;
        }
        self.show_next();
    }

    /// The source an event belongs to, for content-ownership checks. None for
    /// events that never render (rejected sessions, worker failures, progress
    /// updates).
    fn event_source(event: &MediaEvent) -> Option<&str> {
        match event {
            MediaEvent::TrackChanged(track) => Some(&track.source_app),
            MediaEvent::PlaybackStateChanged(_, source) => Some(source),
            _ => None,
        }
    }

    /// The most recent cached track from a source other than `excluded` that
    /// is *actually playing* (last known playback state Playing). A source
    /// whose state is Paused, Stopped, or unknown is never a successor:
    /// swapping the pill to it would announce "now playing" content that is
    /// not playing. Returns None when no playing source has a cached track.
    fn best_successor(&self, excluded: &str) -> Option<TrackInfo> {
        self.track_cache_order.iter().rev().find_map(|source| {
            if source.as_str() == excluded {
                return None;
            }
            self.track_cache
                .get(source)
                .filter(|_| self.source_state.get(source.as_str()) == Some(&PlaybackState::Playing))
                .cloned()
        })
    }

    /// The pinned source's most recently cached track, but only while the pin
    /// is *actually playing* — the "swap only to sources still playing"
    /// discipline (`best_successor`). Recency order keeps the choice
    /// deterministic when several cached sources match a broad pin, and a
    /// paused/stopped match never blocks a playing one — iterating the cache
    /// map directly would let the arbitrary HashMap order decide, and could
    /// skip the pin even though its app is playing. Returns None when no pin
    /// is configured, or the pinned source is paused/stopped/unknown or has
    /// no cached track.
    fn pinned_track(&self) -> Option<TrackInfo> {
        let pin = self.config.behavior.pinned_source.as_deref()?;
        self.track_cache_order.iter().rev().find_map(|source| {
            if !source_matches_pin(source, pin) {
                return None;
            }
            self.track_cache
                .get(source)
                .filter(|_| self.source_state.get(source.as_str()) == Some(&PlaybackState::Playing))
                .cloned()
        })
    }

    /// Preferred-source pinning: when the persistent pill's dismiss deadline
    /// fires while a non-pinned source is showing, swap the pill to the pinned
    /// source's cached track instead of settling into the idle fade — the
    /// pill's resting state is its pinned source. Other sources' events still
    /// show (nothing is filtered; the worker keeps emitting every source); the
    /// return only decides what the pill *rests* on after a dismiss.
    /// The pinned source must be *actually playing* — the "swap only to
    /// sources still playing" discipline, `best_successor` — and must have a
    /// cached track: a paused/stopped pin never resurrects stale content, and
    /// when the pin's session closes (`retire_source_gone` marks it Stopped)
    /// the return stops on its own. Returns true when the pill was swapped;
    /// the caller then skips the fade/collapse decisions and the fresh
    /// deadline (set by `update_content`) runs a full duration before the
    /// next idle fade.
    fn try_return_to_pinned(&mut self) -> bool {
        let Some(pin) = self.config.behavior.pinned_source.as_deref() else {
            return false;
        };
        // The pill already rests on the pinned source (its track, or a state
        // pill for it): nothing to return to.
        let shown_source = self.content.as_ref().and_then(Self::event_source);
        if shown_source.is_some_and(|source| source_matches_pin(source, pin)) {
            return false;
        }
        let Some(track) = self.pinned_track() else {
            return false;
        };
        debug!("persistent pill returning to pinned source {}", track.source_app);
        self.current_source = Some(track.source_app.clone());
        self.last_track = Some(track.clone());
        self.cache_track(&track);
        let full = Duration::from_millis(self.config.overlay.duration_ms.max(500));
        self.update_content(MediaEvent::TrackChanged(track), full);
        true
    }

    /// A source is no longer allowed (removed from `media_sources`, or its
    /// session tripped the churn cool-down). Its content must not stay on the
    /// pill, in the persistent resume hold, behind the settings sample pill,
    /// or queued for a later notification: swap every holding site to the most
    /// recent cached track from a source that is still playing, and hide the
    /// pill when nothing playing remains. Runs regardless of the notifications
    /// toggle — a disabled overlay must not resurrect a retired source's
    /// content at the next show.
    fn retire_source(&mut self, retired: &str) {
        // The retired source must never be a successor again: mark it Stopped
        // in the ledger before the early return below, so a later successor
        // lookup (for another source) cannot surface its cached track.
        self.remember_source_state(retired, PlaybackState::Stopped);
        // The retired source must never surface again from the track cache:
        // drop its entry and recency marker before the early return below,
        // so a later successor lookup or re-show cannot resurrect it. The
        // entry (with its decoded cover) otherwise lingers for the full TTL,
        // wasting one of the few cache slots.
        self.track_cache.remove(retired);
        self.track_cache_order.retain(|source| source != retired);
        // Queued notifications from the retired source can never show now;
        // drop them before hide() -> show_next() could re-show one.
        self.pending.retain(|event| Self::event_source(event) != Some(retired));
        let content_is_retired = self
            .content
            .as_ref()
            .is_some_and(|event| Self::event_source(event) == Some(retired));
        let held_is_retired = self
            .held_content
            .as_ref()
            .is_some_and(|event| Self::event_source(event) == Some(retired));
        let last_is_retired = self
            .last_track
            .as_ref()
            .is_some_and(|track| track.source_app == retired);
        if !content_is_retired && !held_is_retired && !last_is_retired {
            return;
        }
        let successor = self.best_successor(retired);
        if content_is_retired {
            if let Some(track) = &successor {
                debug!("retired source {retired}: swapping the pill to the most recent playing source");
                let full = Duration::from_millis(self.config.overlay.duration_ms.max(500));
                self.current_source = Some(track.source_app.clone());
                self.last_track = Some(track.clone());
                self.cache_track(track);
                self.update_content(MediaEvent::TrackChanged(track.clone()), full);
            } else {
                debug!("retired source {retired}: hiding the pill (no playing source remains)");
                self.held_content = None;
                self.last_track = None;
                self.hide();
                return;
            }
        }
        if held_is_retired {
            self.held_content = successor.as_ref().map(|track| MediaEvent::TrackChanged(track.clone()));
        }
        if self
            .last_track
            .as_ref()
            .is_some_and(|track| track.source_app == retired)
        {
            self.last_track = successor.clone();
        }
    }

    /// A source's sessions all settled (absent from the snapshot past the
    /// worker's terminal-Stop grace): the source stopped or its app quit for
    /// real, so everything the fast-path could restore from it is stale.
    /// Unlike `retire_source` (allow-list removal / churn cool-down), the
    /// source may legitimately return, so its `track_cache` entry and the
    /// now-showing cell are left to their normal lifetimes — only the
    /// restoreable standby dies: queued notifications, the resume hold and
    /// `last_track` swap to the most recent *playing* source or are cleared,
    /// and a pill that is showing the gone source's **track** is retired so
    /// it cannot linger as a stale "now playing".
    /// A `PlaybackStateChanged` pill for the gone source is deliberately left
    /// alone: the settle's terminal Stopped was emitted immediately before
    /// this event in the same sync, so a state pill on screen is the
    /// tombstone itself — hiding it early would cut the deliberate dismissal
    /// UX. Runs regardless of the notifications toggle — a disabled overlay
    /// must not restore the gone source's content at the next show.
    fn retire_source_gone(&mut self, gone: &str) {
        // The settled source must never be a successor again: mark it Stopped
        // unconditionally (before the content checks), so a later SourceGone
        // for another source cannot surface its cached track. A returning
        // source re-inserts its live state with its next event.
        self.remember_source_state(gone, PlaybackState::Stopped);
        // Queued notifications from the gone source can never show now; drop
        // them before hide() -> show_next() could re-show one.
        self.pending.retain(|event| Self::event_source(event) != Some(gone));
        let content_is_gone_track = matches!(
            self.content.as_ref(),
            Some(MediaEvent::TrackChanged(track)) if track.source_app == gone
        );
        let held_is_gone_track = matches!(
            self.held_content.as_ref(),
            Some(MediaEvent::TrackChanged(track)) if track.source_app == gone
        );
        let last_is_gone = self.last_track.as_ref().is_some_and(|track| track.source_app == gone);
        if !content_is_gone_track && !held_is_gone_track && !last_is_gone {
            return;
        }
        let successor = self.best_successor(gone);
        if content_is_gone_track {
            if let Some(track) = &successor {
                debug!("settled source {gone}: swapping the pill to the most recent playing source");
                let full = Duration::from_millis(self.config.overlay.duration_ms.max(500));
                self.current_source = Some(track.source_app.clone());
                self.last_track = Some(track.clone());
                self.cache_track(track);
                self.update_content(MediaEvent::TrackChanged(track.clone()), full);
            } else {
                debug!("settled source {gone}: hiding the pill (no playing source remains)");
                self.held_content = None;
                self.last_track = None;
                self.hide();
                return;
            }
        }
        if held_is_gone_track {
            self.held_content = successor.as_ref().map(|track| MediaEvent::TrackChanged(track.clone()));
        }
        if last_is_gone {
            self.last_track = successor.clone();
        }
    }
    /// Refreshes the shown content in place: keeps the current animation
    /// phase, extends the dismiss deadline to at least `now + min_visible`
    /// (a metadata refresh grants a short extension, a real content change —
    /// a state flip — grants the full configured duration again), and
    /// re-renders. The pill's size is constant — every row band is always
    /// reserved — so a refresh only changes the drawn rows, never the pill's
    /// dimensions.
    /// Re-bases the progress estimate from a `TrackChanged` event. Called on
    /// every path a track becomes active content (`update_content` and
    /// `show_with_duration`), since the latter does not funnel through the
    /// former. A state pill never touches progress — freeze/resume is handled
    /// in `tick`.
    fn apply_track_progress(&mut self, track: &TrackInfo) {
        self.progress_duration_secs = track.duration_secs;
        self.progress_rate = track.playback_rate;
        // Reset the stale-sample baseline: a new track's first ProgressChanged
        // must not be compared against the previous track's last sample.
        self.last_progress_position_secs = None;
        self.estimated_position_secs = track.position_secs;
        // A track with no reported position gets no anchor: anchoring at a
        // fabricated 0.0 would make the tick crawl the bar from zero (and
        // anchoring at an old position would keep a stale estimate). With no
        // anchor the bar simply holds.
        self.progress_anchor = track
            .position_secs
            .map(|pos| (track.position_updated_at.unwrap_or_else(Instant::now), pos));
        self.progress_playing = true;
    }

    /// Re-bases the progress estimate from a live `ProgressChanged` update
    /// (pushed on every timeline refresh). Unlike `apply_track_progress` it does
    /// not touch `progress_playing` — that is derived from the active content
    /// each tick — so a position update never changes whether the bar is
    /// advancing. Re-anchoring to the freshly read position means the bar tracks
    /// seeks immediately instead of relying on the seek re-emit.
    /// Reconciles the live sample against the interpolated estimate instead of
    /// hard-snapping: adopt the sample when it is at/ahead of the display; when it
    /// is a little behind (OS report latency, within `PROGRESS_LATENCY_TOL_SECS`)
    /// keep the displayed value and only forward the anchor time, so the bar stays
    /// monotonic; when it is far behind (backward seek or a new track) adopt it so
    /// the bar reflects the real position. The worker's `SEEK_DELTA_SECS` covers
    /// seeks independently via a `TrackChanged` re-emit.
    fn apply_progress(&mut self, position_secs: Option<f64>, duration_secs: Option<u64>, rate: Option<f64>) {
        self.progress_duration_secs = duration_secs;
        self.progress_rate = rate;
        let Some(pos) = position_secs else {
            // A source that stops reporting a position (or reports none) must
            // not leave the old estimate or its interpolating anchor behind:
            // the bar would otherwise keep crawling from a stale base. Clear
            // it.
            self.estimated_position_secs = None;
            self.progress_anchor = None;
            self.last_progress_position_secs = None;
            return;
        };
        // Detect stale SMTC samples: many media apps refresh the OS timeline
        // position every few seconds rather than on every poll, so consecutive
        // reads can return the same value while the bar keeps interpolating ahead.
        // Snapping to an unchanged (stale) value every 3-4s makes the bar jerk
        // backward on every poll that didn't advance. When the position is
        // fresh — moved since the last read — the normal reconciliation logic
        // applies: a small backward jitter is absorbed (monotonic bar), a large
        // backward jump (seek / new track at 0) is adopted.
        let stale = self.last_progress_position_secs == Some(pos);
        let base = match self.estimated_position_secs {
            Some(cur) if pos >= cur => pos,
            Some(cur) if !stale && pos >= cur - PROGRESS_LATENCY_TOL_SECS => cur,
            Some(cur) if stale => cur,
            _ => pos,
        };
        self.estimated_position_secs = Some(base);
        // Only re-anchor on a fresh sample. A stale sample must not reset the
        // anchor instant, which would freeze the bar at the stale position.
        if !stale {
            self.progress_anchor = Some((Instant::now(), base));
        }
        self.last_progress_position_secs = Some(pos);
    }

    /// Integrates the live playback position from an anchor: position at the
    /// anchor plus elapsed seconds times rate, never negative.
    fn estimate_position(base: f64, rate: f64, elapsed: f64) -> f64 {
        (base + elapsed * rate).max(0.0)
    }

    /// Publishes the source of the content just displayed into the shared
    /// now-showing cell: the SMTC worker's session-recreation gate suppresses
    /// a same-source re-report only while the pill actually shows that
    /// source, and this overlay alone knows what that is. Every content
    /// display funnels through `update_content` or `show_with_duration`, so
    /// both publish here; a dismiss or collapse never clears the cell (see
    /// `now_showing`).
    fn publish_now_showing(&self) {
        let Some(cell) = &self.now_showing else {
            return;
        };
        let source = match &self.content {
            Some(MediaEvent::TrackChanged(track)) => Some(track.source_app.clone()),
            Some(MediaEvent::PlaybackStateChanged(_, source)) if !source.is_empty() => Some(source.clone()),
            _ => None,
        };
        if let Some(source) = source {
            *cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(source);
        }
    }

    fn update_content(&mut self, event: MediaEvent, min_visible: Duration) {
        // PersistentCompact auto-hide: a meaningful update that arrives while the
        // pill is already hidden for a fullscreen/listed foreground must surface
        // briefly like a compact notification (full configured duration, then
        // collapse back into the auto-hidden hold) — otherwise track and state
        // changes vanish into the held content and never show over fullscreen or
        // whitelisted apps. `show_with_duration` re-samples the foreground,
        // saves the latest content for the resume, and — over a still-fullscreen
        // foreground — flags the dismiss path to collapse into a full hide; the
        // collapse's `hide()` re-arms the watchdog and keeps the newest
        // held_content, so the next foreground clear resumes the latest track.
        if self.config.overlay.layout == LayoutMode::PersistentCompact && self.persistent_auto_hidden() {
            let full = matches!(event, MediaEvent::TrackChanged(_));
            self.show(event, full);
            return;
        }
        // An in-place refresh is a meaningful pill update too: re-resolve
        // the layout so a foreground change since the pill appeared takes
        // effect with the update rather than on the next static tick.
        self.refresh_layout();
        // A swap on a fully-static pill dissolves the previous frame into
        // the new content (see `ContentFade`) — the snapshot is the last
        // rendered frame, and the animation timer keeps the dissolve
        // ticking. Any animated state (entrance, collapse, hover morph)
        // swaps instantly instead — and so does every swap while system
        // preferences disable animation.
        if matches!(self.phase, Phase::Shown)
            && self.hover_expand.is_none()
            && !self.frame_scratch.is_empty()
            && crate::winutil::animations_enabled()
        {
            let from = std::mem::take(&mut self.frame_scratch);
            self.content_fade = Some(ContentFade {
                start: Instant::now(),
                from,
                from_w: self.last_frame_w,
                from_h: self.last_frame_h,
            });
            self.sync_anim_timer();
        } else {
            self.content_fade = None;
        }
        if let MediaEvent::TrackChanged(ref track) = event {
            self.apply_track_progress(track);
        }
        // A pill currently shown over a fullscreen/listed foreground is flagged
        // for collapse-on-dismiss; keep held_content in sync so the resume (when
        // the foreground clears) restores the latest track, not a stale one. The
        // auto-hidden case is handled above — it shows briefly instead of
        // swapping in place while the pill stays hidden.
        if self.persistent_collapse_on_dismiss {
            self.held_content = Some(event.clone());
        }
        self.content_rev += 1;
        self.content = Some(event);
        self.publish_now_showing();
        self.content_palette = match &self.content {
            Some(MediaEvent::TrackChanged(track)) => track.palette,
            _ => None,
        };
        self.resolve_pill_text();
        self.reset_scroll();
        // Persistent-compact: a content refresh restores full opacity and
        // restarts the fade timer.
        if self.config.overlay.layout == LayoutMode::PersistentCompact {
            self.persistent_faded = false;
        }
        if let Some(deadline) = self.dismiss_at {
            self.dismiss_at = Some(deadline.max(Instant::now() + min_visible));
        }
        // A refresh that lands during the collapse (e.g. artwork arriving as
        // the pill fades) would otherwise be cut short: the collapse keeps its
        // original start time and hides the pill when its animation finishes,
        // ignoring the extended deadline. Revive it with a fresh entrance so it
        // grows back smoothly to full visibility for the extended time, instead
        // of snapping to the full size on the next frame.
        if matches!(self.phase, Phase::Collapsing(_)) {
            self.phase = Phase::Expanding(Instant::now());
        }
        self.render();
    }

    /// Builds the render pieces for the pill content once, when the content
    /// changes. The state-pill path resolves the cached track here too, so
    /// animation frames draw from `pill_text` without a per-frame TrackInfo
    /// clone or meta-line rebuild. `None` for a state pill whose source has
    /// no cached track: the caller falls back to the source-name layout.
    /// The resolved text is also mirrored into the shared accessible-name
    /// cell, so the read-only UIA name provider (callable from any thread)
    /// always reflects what the pill is showing; a genuine track change
    /// additionally raises the UIA name property-changed event so a screen
    /// reader tracking the pill announces the new track.
    fn resolve_pill_text(&mut self) {
        self.pill_text = match &self.content {
            Some(MediaEvent::TrackChanged(track)) => Some(pill_text_from_track(track)),
            Some(MediaEvent::PlaybackStateChanged(_, source)) if !source.is_empty() => {
                self.track_cache.get(source).map(pill_text_from_track)
            }
            _ => None,
        };
        // The accessible name is the pill's own text: title — artist
        // (source), with empty parts dropped, so a screen reader announces
        // exactly what the pill shows. A state pill with no cached track
        // (the pill then renders the source-name layout) falls back to
        // naming the source app.
        let name = self.pill_text.as_ref().map(|text| {
            let mut parts = Vec::new();
            if !text.title.trim().is_empty() {
                parts.push(text.title.trim().to_string());
            }
            if !text.artist.trim().is_empty() {
                parts.push(text.artist.trim().to_string());
            }
            let joined = parts.join(" — ");
            if joined.is_empty() {
                text.source_app.clone()
            } else if !text.source_app.trim().is_empty() {
                format!("{joined} ({})", text.source_app.trim())
            } else {
                joined
            }
        });
        // The state-pill source-name fallback: no cached track, so the pill
        // text is None but the pill still names its source app on screen.
        let name = name.or_else(|| match &self.content {
            Some(MediaEvent::PlaybackStateChanged(_, source)) if !source.trim().is_empty() => {
                Some(source.trim().to_string())
            }
            _ => None,
        });
        if let Some(cell) = &self.pill_name {
            // Mirror the resolved name into the shared cell, then raise the
            // UIA name property-changed event on a genuine track change, so a
            // screen reader tracking the pill announces the new track. The
            // cell always reflects the current name (re-queries stay correct);
            // only the announcement is gated — a play/pause transition that
            // alters the name must stay silent. The raise is best-effort and
            // passive — announcement only, never focus or activation (see
            // accessibility::raise_pill_name_changed).
            let mut guard = cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let old = guard.take();
            let changed = old != name;
            *guard = name.clone();
            drop(guard);
            if changed && Self::announces_pill_name_change(&self.content) {
                crate::accessibility::raise_pill_name_changed(self.hwnd, cell, old, name);
            }
        }
    }

    /// Whether a name change on the current content should be announced. Only
    /// a genuine track change announces: a play/pause transition can alter the
    /// resolved name too (the state-pill source-name fallback when the source
    /// has no cached track) but must stay silent — announcements are for new
    /// tracks, not state flips.
    fn announces_pill_name_change(content: &Option<MediaEvent>) -> bool {
        matches!(content, Some(MediaEvent::TrackChanged(_)))
    }

    /// Drops the overlay's reference to the shared accessible-name cell and
    /// clears the cell's contents through the shared UIA teardown contract
    /// (`accessibility::clear_uia_provider_state`, the same helper the main
    /// window's settings-snapshot drop uses), so a UIA name provider that
    /// outlives the window (UIA core holds a reference across the last
    /// release) reads an empty name instead of the last track. Called from
    /// WM_NCDESTROY — the overlay analog of the main window's snapshot drop.
    /// The contents are cleared, not just the Arc dropped: the provider
    /// holds its own clone, so dropping ours alone would leave it reading
    /// the last track name. Clearing via the shared helper also recovers a
    /// poisoned lock, which the previous `if let Ok` skip did not.
    fn null_pill_name_cell(&mut self) {
        if let Some(cell) = self.pill_name.take() {
            crate::accessibility::clear_uia_provider_state(&cell);
        }
    }

    fn show(&mut self, event: MediaEvent, full_animation: bool) {
        self.show_with_duration(event, full_animation, self.config.overlay.duration_ms.max(500));
    }

    fn show_with_duration(&mut self, event: MediaEvent, full_animation: bool, duration_ms: u64) {
        if !self.enabled {
            return;
        }
        // Any show ends the auto-hide hold: the watchdog (armed by hide()
        // when content was held) must not poll while a pill is up, and the
        // next hide re-arms it from scratch.
        self.hidden_watchdog = false;
        // A show is a meaningful pill-update boundary: re-resolve the Auto
        // layout from the current foreground before the frame geometry is
        // computed, so a pill that appears over a fullscreen game (or over a
        // listed app) is compact from its very first frame.
        self.refresh_layout();
        // PersistentCompact auto-hide: if the foreground is fullscreen/listed
        // and hide_for_auto_compact_sources is on, flag the dismiss path so the
        // pill collapses (full hide) on its normal dismiss instead of fading to
        // idle opacity. The pill still shows for its duration first — this only
        // changes what happens when dismiss_at fires.
        if self.config.overlay.layout == LayoutMode::PersistentCompact
            && self.config.behavior.hide_for_auto_compact_sources
        {
            let verdict = self.sample_foreground();
            self.persistent_collapse_on_dismiss =
                verdict.fullscreen || fullscreen::auto_source_matches(&self.config, verdict.exe.as_deref());
            // Save the content so on_foreground_change can resume it when the
            // user returns from the fullscreen/listed app — the pill will
            // collapse on its own via the tick, not from this call site.
            if self.persistent_collapse_on_dismiss {
                self.held_content = Some(event.clone());
            }
        }
        // A fresh pill invalidates the idle-release deadline: the frame
        // buffers are about to be reused.
        unsafe {
            let _ = kill_timer(self.hwnd, IDLE_BUFFER_TIMER_ID);
        }
        if let MediaEvent::TrackChanged(ref track) = event {
            self.apply_track_progress(track);
        }
        self.content_rev += 1;
        self.content = Some(event);
        self.publish_now_showing();
        self.content_palette = match &self.content {
            Some(MediaEvent::TrackChanged(track)) => track.palette,
            _ => None,
        };
        self.resolve_pill_text();
        self.reset_scroll();
        let now = Instant::now();
        self.dismiss_at = Some(now + Duration::from_millis(duration_ms));
        // A fresh pill must not inherit hover state from the previous one:
        // re-arm hover-dismiss only if the cursor is still over the new pill,
        // and grant the new notification its own first expansion.
        self.hover_dismiss_at = None;
        self.hover_expand = None;
        self.hover_expanded_once = false;
        self.hover_leave_at = None;
        self.persistent_faded = false;
        self.content_fade = None;
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
        #[cfg(test)]
        {
            if let Some(verdict) = &self.test_fg_verdict {
                self.last_fullscreen = Some(verdict.fullscreen);
                return verdict.clone();
            }
        }
        let foreground = unsafe { GetForegroundWindow() };
        let fullscreen = window_is_fullscreen(foreground, self.hwnd);
        self.last_fullscreen = Some(fullscreen);
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

    /// The static-tick re-check: reacts to a foreground change within one
    /// static tick (250 ms) even when no media event arrives (e.g. an
    /// alt-tab into a fullscreen game while the pill is up). The full
    /// decision (process enumeration) runs only when the foreground HWND
    /// changed; an unchanged window gets its fullscreen geometry re-checked
    /// at most once per second — a same-window resize (fullscreen toggle)
    /// cannot matter more often than that. Returns (whether the layout
    /// flipped, whether the fullscreen verdict changed), so the caller can
    /// force a re-render (Auto) or re-run the auto-hide decision
    /// (PersistentCompact).
    fn tick_layout_check(&mut self) -> (bool, bool) {
        let now = Instant::now();
        let foreground = unsafe { GetForegroundWindow() };
        let hwnd_changed = self.layout_fg != Some(foreground);
        let geometry_due = hwnd_changed
            || self
                .last_geometry_check
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if !geometry_due {
            return (false, false);
        }
        self.last_geometry_check = Some(now);
        let before_layout = self.layout;
        let before_fullscreen = self.last_fullscreen;
        self.refresh_layout();
        (self.layout != before_layout, self.last_fullscreen != before_fullscreen)
    }

    /// 1 Hz foreground re-check while the pill is auto-hidden with held
    /// content (see `hide`). A same-window fullscreen-exit (F11 in a browser,
    /// Alt+Enter in a game) leaves the foreground HWND unchanged, so
    /// `EVENT_SYSTEM_FOREGROUND` never fires and `on_foreground_change`
    /// would not run; this poll routes a verdict change through it so the
    /// held pill resumes. Disarms itself when the auto-hide is no longer
    /// applicable (layout/config changed while hidden).
    fn tick_hidden_watchdog(&mut self) {
        if !self.enabled
            || !(self.config.overlay.layout == LayoutMode::PersistentCompact
                && self.config.behavior.hide_for_auto_compact_sources)
        {
            self.hidden_watchdog = false;
            self.delete_anim_timer();
            return;
        }
        let now = Instant::now();
        if self
            .last_geometry_check
            .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.last_geometry_check = Some(now);
        let before = self.last_fullscreen;
        self.sample_foreground();
        if self.last_fullscreen != before {
            self.on_foreground_change();
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        // A tick can be delivered after the pill was hidden (one was already
        // queued when hide() ran). The hidden phase must not re-arm the
        // refresh-rate timer or do any per-tick work. The single exception
        // is the auto-hide watchdog: those ticks are deliberately armed by
        // hide() and only poll the foreground at 1 Hz.
        if matches!(self.phase, Phase::Hidden) {
            self.last_tick = now;
            if self.hidden_watchdog {
                self.tick_hidden_watchdog();
            }
            return;
        }
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        // Progress estimate: advance the live position from the anchor while
        // playing; freeze it while paused/stopped and re-anchor on resume so
        // the bar never crawls or jumps forward.
        let playing = match &self.content {
            // The TrackChanged snapshot carries the authoritative playback
            // state (see `TrackInfo.playback_state`): a paused/stopped pill
            // must not crawl its bar. A None state (pre-carriage sessions,
            // spurious-recreation snapshots) keeps the historical behavior
            // of treating a track pill as playing.
            Some(MediaEvent::TrackChanged(track)) => track.playback_state.is_none_or(|s| s == PlaybackState::Playing),
            Some(MediaEvent::PlaybackStateChanged(s, _)) => *s == PlaybackState::Playing,
            _ => false,
        };
        // A Stopped-state pill is a tombstone: the source behind it is done,
        // so persistent-compact must not let it linger at idle opacity — it
        // collapses and hides at its dismiss deadline (see below).
        let stopped_shown = matches!(
            &self.content,
            Some(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, _))
        );
        if playing && !self.progress_playing {
            // Resuming after a pause/stop: re-anchor so elapsed restarts from
            // the frozen position instead of the original anchor.
            if let Some(est) = self.estimated_position_secs {
                self.progress_anchor = Some((Instant::now(), est));
            }
        }
        self.progress_playing = playing;
        if playing && let (Some((at, base)), Some(rate)) = (self.progress_anchor, self.progress_rate) {
            self.estimated_position_secs = Some(Self::estimate_position(base, rate, at.elapsed().as_secs_f64()));
        }
        // A settled pill (Phase::Shown, nothing animating) does not repaint on the
        // static tick — see the render gate below — so a live position advance
        // would otherwise stay painted at the last `ProgressChanged` sample until
        // the next content event, freezing the bar for up to ~1 s on slow
        // samplers. Crawl it here instead, but only when it would actually move by
        // at least a pixel: at 1x on a long song the per-tick advance is
        // sub-pixel, so repainting those frames would burn whole-pill rasterizes
        // for identical pixels. `last_frame_w` is the painted content width, set
        // each render — used as the bar's pixel span for the 1px threshold.
        let bar_moved = if playing
            && self.progress_rate.is_some()
            && self.progress_anchor.is_some()
            && let Some(est) = self.estimated_position_secs
            && let Some(duration) = self.progress_duration_secs
            && duration > 0
            && self.last_frame_w > 0
        {
            let fraction = (est / duration as f64).clamp(0.0, 1.0) as f32;
            let threshold = (1.0 / self.last_frame_w as f32).max(1e-4);
            let moved = self
                .last_bar_fraction
                .is_none_or(|prev| (fraction - prev).abs() >= threshold);
            self.last_bar_fraction = Some(fraction);
            moved
        } else {
            self.last_bar_fraction = None;
            false
        };
        // When not playing, estimated_position_secs is left frozen.
        // A layered popup can be hidden by fullscreen transitions or external
        // ShowWindow calls; re-assert visibility and topmost z-order while a
        // pill should be up. While the pill is fully shown this is throttled
        // to 1 Hz — the window state cannot meaningfully change every 4 ms.
        let animating =
            !matches!(self.phase, Phase::Shown) || self.hover_expand.is_some() || self.content_fade.is_some();
        if !matches!(self.phase, Phase::Hidden)
            && (animating || self.last_reassert.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)))
        {
            self.last_reassert = Some(now);
            unsafe {
                if !IsWindowVisible(self.hwnd).as_bool() {
                    let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                }
                if let Err(error) = set_window_pos(
                    self.hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                ) {
                    debug!("pill set_window_pos(topmost) failed: {error}");
                }
            }
        }
        // Hover handling. The rules follow the pill's *effective* layout
        // (see `hover_step`): an Expanded-layout pill arms the one-way 500ms
        // hover-dismiss only while `dismiss_on_hover` is enabled — the
        // countdown is never deferred for the cursor — and a Compact-layout
        // pill expands on hover while `expand_compact_on_hover` is enabled,
        // falling back to the Expanded rules otherwise. The compact→expanded
        // morph itself is an interaction: while it is in flight or pinned,
        // hovering never arms anything, and the `held` gate below defers the
        // countdown, so the expanded state is never dismissed mid-read.
        // Leaving the morph collapses it back to compact and resets the
        // countdown to the full duration; every re-entry re-expands and
        // resets again.
        //
        // Cursor state with the leave debounce: a leave is only trusted
        // after the cursor has stayed away for the debounce window, so
        // boundary jitter cannot reverse a fresh morph the moment it
        // starts. Re-entering (or never leaving) keeps the pill engaged.
        // Computed here, outside the phase guard, so the `held` gate below
        // can use it.
        let cursor_over = self.is_cursor_over_pill();
        self.last_cursor_over_pill = cursor_over;
        if cursor_over {
            self.hover_leave_at = None;
            // Persistent-compact: hovering the pill restores full opacity and
            // restarts the fade timer for when the cursor leaves.
            if self.config.overlay.layout == LayoutMode::PersistentCompact && self.persistent_faded {
                self.persistent_faded = false;
                self.dismiss_at = Some(now + Duration::from_millis(self.config.overlay.duration_ms.max(500)));
                debug!("persistent pill restored on hover");
            }
        } else if self.hover_leave_at.is_none() {
            self.hover_leave_at = Some(now);
        }
        let engaged = hover_engaged(cursor_over, self.hover_leave_at, now);
        // Only the morph-origin expanded state is held — it is an
        // interaction, so its countdown is deferred while the cursor stays
        // on it. The hold is stateless math over the cursor inputs — no flag
        // to clear — so the instant it drops (leave past the debounce) the
        // dismissal applies again with whatever deadline the pill has.
        // Queued notifications wait with the hold (their EARLY_EXIT cap is
        // suppressed below) and updates route in place (see `held_expanded`
        // in `receive_events`). A laid-out expanded pill is never held.
        let held = engaged && self.hover_expand.is_some();
        if !matches!(self.phase, Phase::Hidden) {
            // A morph leg completes first so the same tick sees `done`. The
            // leg duration is per-direction (the collapse leg is shorter).
            if let Some(morph) = &mut self.hover_expand
                && morph.start.elapsed() >= morph_duration(&self.config, morph.direction)
            {
                match morph.direction {
                    MorphDirection::Expand if !morph.done => {
                        morph.done = true;
                        self.hover_expanded_once = true;
                        debug!("pill hover expand complete");
                    }
                    MorphDirection::Collapse => {
                        self.hover_expand = None;
                        debug!("pill hover collapse complete");
                    }
                    _ => {}
                }
            }
            // The morph decisions only apply to a fully-shown pill: a hover
            // during the entrance/collapse animation keeps the plain
            // Expanded-rule arming below.
            if matches!(self.phase, Phase::Shown) {
                let step = hover_step(
                    HoverTick {
                        cursor_over: engaged,
                        morphing: self.hover_expand.is_some(),
                        morph_expanding: matches!(&self.hover_expand, Some(m) if m.direction == MorphDirection::Expand),
                        dismiss_armed: self.hover_dismiss_at.is_some(),
                    },
                    // Persistent-compact: the expanded pill never dismisses on
                    // hover — it collapses back to compact and fades after the
                    // timeout. Override dismiss_on_hover to false so the hover
                    // machine never arms a dismiss.
                    if self.config.overlay.layout == LayoutMode::PersistentCompact {
                        false
                    } else {
                        self.config.overlay.dismiss_on_hover
                    },
                    self.config.overlay.expand_compact_on_hover,
                    self.hover_expanded_once,
                    self.layout == LayoutMode::Expanded,
                );
                match step {
                    HoverStep::StartExpand => {
                        self.hover_expand = Some(HoverExpand {
                            start: now,
                            direction: MorphDirection::Expand,
                            from: 0.0,
                            velocity: 0.0,
                            done: false,
                        });
                        // The morph replaces the hover-to-dismiss deadline
                        // with the full configured duration: the user is
                        // reading the expanded pill, so it must not be cut
                        // short at 500ms. Every re-entry resets it again.
                        self.dismiss_at = Some(now + Duration::from_millis(self.config.overlay.duration_ms.max(500)));
                        debug!("pill hover expand started");
                    }
                    HoverStep::ArmDismiss => {
                        self.hover_dismiss_at = Some(now);
                        // The arm caps the remaining time at 500ms; it must
                        // never extend an already-sooner deadline (e.g. an
                        // earlier hover arm or the queued-notification cap).
                        let early = now + Duration::from_millis(EARLY_EXIT_MS);
                        self.dismiss_at = Some(self.dismiss_at.map_or(early, |d| d.min(early)));
                        debug!("pill hover-dismiss armed");
                    }
                    HoverStep::ReverseMorph => {
                        let (from, velocity) = self
                            .hover_expand
                            .as_ref()
                            .map(|morph| reversal_seed(morph, &self.config, now))
                            .unwrap_or((0.0, 0.0));
                        // Leaving the interaction resets the countdown: the
                        // collapsed pill gets its full duration again.
                        self.dismiss_at = Some(now + Duration::from_millis(self.config.overlay.duration_ms.max(500)));
                        if from < REVERSAL_MIN_PROGRESS {
                            // Reversed from (nearly) compact: drop the morph
                            // instead of running a spring release — a seeded
                            // release would balloon the pill for a reversal
                            // it barely left.
                            self.hover_expand = None;
                            debug!("pill hover morph cancelled near compact");
                        } else {
                            self.hover_expand = Some(HoverExpand {
                                start: now,
                                direction: MorphDirection::Collapse,
                                from,
                                velocity,
                                done: false,
                            });
                            debug!("pill hover morph reversed");
                        }
                    }
                    HoverStep::None => {}
                }
            } else if cursor_over && self.hover_dismiss_at.is_none() && self.config.overlay.dismiss_on_hover {
                // Hover during the entrance/collapse animation: only an
                // Expanded-layout pill arms (Compact pills only ever expand
                // on hover, which the Shown path handles).
                if self.layout == LayoutMode::Expanded {
                    self.hover_dismiss_at = Some(now);
                    self.dismiss_at = Some(now + Duration::from_millis(EARLY_EXIT_MS));
                    debug!("pill hover-dismiss armed");
                }
            }
        }
        // A newer notification is waiting: while the pill is held under the
        // cursor the queue waits (the user is reading it), but once the hold
        // ends the current pill's remaining time is capped so the queued
        // notification shows promptly. min() never extends an already-sooner
        // deadline (e.g. hover-dismiss). (This used to run in
        // `receive_events`; it lives here so the hold can suppress it — the
        // tick is the only place that knows the cursor state.)
        if !held && !self.pending.is_empty() && !matches!(self.phase, Phase::Hidden | Phase::Collapsing(_)) {
            let early = now + Duration::from_millis(EARLY_EXIT_MS);
            self.dismiss_at = Some(self.dismiss_at.map_or(early, |d| d.min(early)));
        }
        // Persistent-compact: when the cursor leaves the pill, restart the
        // fade timer so the pill fades from full opacity after another
        // duration_ms idle period.
        if self.config.overlay.layout == LayoutMode::PersistentCompact
            && !cursor_over
            && self.hover_leave_at == Some(now)
        {
            self.dismiss_at = Some(now + Duration::from_millis(self.config.overlay.duration_ms.max(500)));
            self.persistent_faded = false;
        }
        // Persistent-compact: when dismiss_at fires, fade to idle opacity
        // instead of collapsing. Skip the normal dismiss path entirely.
        // Exception: when persistent_collapse_on_dismiss is set (foreground is
        // fullscreen/listed), skip the idle fade and fall through to the
        // normal collapse path so the pill hides instead of lingering at 0.25.
        // With the idle fade off, the deadline is a no-op: the pill stays at
        // full opacity whether playing or paused — only a Stopped-state pill
        // (tombstone: source is done) collapses into a full hide below.
        if self.config.overlay.layout == LayoutMode::PersistentCompact && !self.persistent_collapse_on_dismiss {
            // Preferred-source pinning: when the dismiss deadline fires while a
            // non-pinned source is showing, return to the pinned source's
            // cached track instead of settling into the idle fade (or
            // collapsing a Stopped tombstone) — the pill's resting state is
            // its pinned source. The swap runs `update_content`, which
            // restores full opacity and restarts the dismiss timer, so the
            // pill shows the pinned track for a full duration before the next
            // idle fade. Skipped whenever the pill must collapse into a hide
            // instead (collapse-on-dismiss is handled by the outer guard), is
            // being read (cursor over), or the pinned source is not playing.
            if !cursor_over
                && self.dismiss_at.is_some_and(|deadline| deadline <= now)
                && matches!(self.phase, Phase::Shown)
                && self.try_return_to_pinned()
            {
                debug!("persistent pill returned to the pinned source");
            } else if stopped_shown {
                // A Stopped-state pill must not linger at idle opacity: its
                // source is done, so the deadline collapses it into a full
                // hide. One exception: when the tombstone belongs to the
                // pinned source and another source is still playing, the pill
                // settles onto that source's most recent track instead of
                // going dark — the pin's session closing must not leave the
                // persistent pill hiding while other media is audible. The
                // tombstone has already held its full duration by this
                // deadline, so the deliberate "source stopped" announcement
                // is preserved; this is the "swap only to sources still
                // playing" discipline (`best_successor`) applied to the pin's
                // own retirement. With no playing successor the tombstone
                // still collapses into the full hide.
                if !cursor_over
                    && self.dismiss_at.is_some_and(|deadline| deadline <= now)
                    && matches!(self.phase, Phase::Shown)
                {
                    let tombstone_source = match &self.content {
                        Some(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, source)) => Some(source.as_str()),
                        _ => None,
                    };
                    let pinned_tombstone = tombstone_source.is_some_and(|source| {
                        self.config
                            .behavior
                            .pinned_source
                            .as_deref()
                            .is_some_and(|pin| source_matches_pin(source, pin))
                    });
                    let successor = if pinned_tombstone {
                        tombstone_source.and_then(|gone| self.best_successor(gone))
                    } else {
                        None
                    };
                    if let Some(track) = successor {
                        debug!(
                            "persistent pill settling on {} after the pinned source stopped",
                            track.source_app
                        );
                        let full = Duration::from_millis(self.config.overlay.duration_ms.max(500));
                        self.current_source = Some(track.source_app.clone());
                        self.last_track = Some(track.clone());
                        self.cache_track(&track);
                        self.update_content(MediaEvent::TrackChanged(track), full);
                    } else {
                        self.phase = Phase::Collapsing(now);
                        self.hover_expand = None;
                        debug!("persistent pill hidden (stopped)");
                    }
                }
            } else if self.config.overlay.fade_persistent_pill
                && !self.persistent_faded
                && !cursor_over
                && self.dismiss_at.is_some_and(|deadline| deadline <= now)
                && matches!(self.phase, Phase::Shown)
            {
                self.persistent_faded = true;
                debug!("persistent pill faded to idle opacity");
            }
            // fade_persistent_pill = false and !stopped_shown: the deadline is a
            // no-op — the pill stays at full opacity in the Shown phase.
        } else if !held
            && self.dismiss_at.is_some_and(|deadline| deadline <= now)
            && !matches!(self.phase, Phase::Collapsing(_) | Phase::Hidden)
            && !matches!(&self.hover_expand, Some(m) if m.direction == MorphDirection::Collapse)
        {
            self.phase = Phase::Collapsing(now);
            // An in-flight hover *expand* leg loses to the dismiss: the pill
            // collapses from the plain compact shape instead of freezing at
            // whatever size the morph had reached (e.g. a queued update caps
            // the remaining time). An in-flight hover *collapse* leg is
            // allowed to finish first — it is already on its way back, and
            // cutting it short mid-shrink would snap the pill — so its
            // dismissal fires the tick after the leg completes.
            self.hover_expand = None;
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
            Phase::Collapsing(start) if start.elapsed() >= collapse_duration(&self.config) => {
                // Persistent-compact: the collapse animation shrinks the pill
                // back to compact size, but the pill stays visible (fades to
                // idle opacity instead of hiding) — unless
                // fullscreen/listed), in which case the pill fully hides.
                // A Stopped-state pill (tombstone: source is done) also hides
                // here regardless of the fade setting — nothing can revive it.
                if self.config.overlay.layout == LayoutMode::PersistentCompact && !self.persistent_collapse_on_dismiss {
                    if stopped_shown {
                        self.hide();
                        return;
                    }
                    self.phase = Phase::Shown;
                    self.persistent_faded = false;
                    self.dismiss_at = Some(now + Duration::from_millis(self.config.overlay.duration_ms.max(500)));
                    debug!("persistent pill collapsed to compact, restarting fade timer");
                } else {
                    self.hide();
                    return;
                }
            }
            _ => {}
        }

        // Advance marquee offsets (driven by this same tick, entirely
        // independent of the dismiss countdown). Time-based so the scroll
        // speed is identical at any frame rate. With animations disabled the
        // offsets never advance: overflowing lines render statically
        // . The DPI scale is queried only while a line is actually
        // scrolling: a static pill repaints nothing, so its coarse tick must
        // not pay for a per-tick DPI call.
        let marquee_active = self.scroll.iter().any(|line| line.scrolling);
        let scale = if marquee_active {
            self.fonts.dpi().max(96) as f32 / 96.0
        } else {
            1.0
        };
        let per_tick = MARQUEE_SPEED * scale * dt;
        if crate::winutil::animations_enabled() {
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
        }
        // Foreground re-check: a foreground change flips the Auto pill between
        // layouts within one static tick even when no media event arrives
        // (an alt-tab into a fullscreen game mid-pill). Persistent-compact
        // re-checks too: a same-window fullscreen toggle (F11 in a browser,
        // Alt+Enter in a game) leaves the foreground HWND unchanged, so the
        // WinEvent hook never fires, and the verdict comparison keeps the
        // auto-hide decision honest without the hook. Only the static tick
        // runs it; animation frames skip it, so a flip never lands mid-
        // expand/collapse. A flipped layout forces a render (the pill's
        // size, content layout and placement all change with it).
        let mode = self.config.overlay.layout;
        let (layout_flipped, fullscreen_changed) = if !animating
            && (mode == LayoutMode::Auto
                || (mode == LayoutMode::PersistentCompact && self.config.behavior.hide_for_auto_compact_sources))
        {
            self.tick_layout_check()
        } else {
            (false, false)
        };
        // Persistent-compact: route a fullscreen-verdict change through the
        // foreground-change handler so the pill auto-hides the moment a
        // same-window fullscreen toggle lands (the foreground HWND never
        // changed, so the event hook cannot have reported it).
        if fullscreen_changed && mode == LayoutMode::PersistentCompact {
            self.on_foreground_change();
            // The auto-hide hid the pill mid-tick; the trailing
            // sync_anim_timer would treat Hidden as animating and recreate
            // the timer at refresh rate, clobbering the watchdog's coarse
            // 1 s cadence. Mirror the collapse-finish precedent: stop here.
            if matches!(self.phase, Phase::Hidden) {
                return;
            }
        }
        // The render gate must see the hover state as it stands AFTER this
        // tick's hover-detection code ran: `animating` was computed at the
        // top of the tick, before the cursor poll, so on the tick that
        // starts a hover morph it still reflects the pre-morph state and
        // would skip the morph's first frame — the first rendered frame
        // would then sample the spring already into the leg (the
        // tick-cadence gap). The phase half of the condition stays on the
        // top-of-tick value, so a phase transition in this same tick still
        // renders its rest frame, exactly as before.
        if layout_flipped
            || animating
            || self.hover_expand.is_some()
            || marquee_active
            || bar_moved
            || self.persistent_fade_active()
        {
            self.render();
        }
        // Re-sync the timer to the phase: a static pill drops to the coarse
        // tick, a phase transition or a marquee line starting restores the
        // refresh-rate cadence.
        self.sync_anim_timer();
    }

    fn render(&mut self) {
        #[cfg(test)]
        {
            self.render_count += 1;
        }
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
        let frame = self.frame();
        let raw_dpi = monitor_dpi(target.handle);
        if needs_font_rebuild(self.fonts.dpi(), raw_dpi) {
            self.fonts = FontProvider::new(raw_dpi);
        }
        let dpi = raw_dpi as f32 / 96.0;
        let compact = self.layout == LayoutMode::Compact;
        // One morph per frame, resolved by construction: the hover leg and
        // the entrance/exit grow are mutually exclusive — the hover morph is
        // only ever started while the pill is fully shown (`Phase::Shown`,
        // see the hover step in `tick`), where `frame.morph` is None, and it
        // is cleared the moment the pill leaves Shown (dismiss, collapse,
        // hide). Prefer the hover leg when both look present, the same rule
        // the sizing below uses, so the progress that sizes the window is
        // always the one that renders it. Without this the hover leg's
        // progress was computed for sizing and then discarded: the render
        // got `frame.morph` (None during Shown), so the corner radius
        // snapped to the expanded value and the interpolated art tile never
        // rendered at its morph position on the very first hover frame.
        let morph = if let Some(hover) = &self.hover_expand {
            debug_assert!(
                frame.morph.is_none(),
                "the hover morph must not overlap the entrance/exit grow"
            );
            Some(hover_progress(hover, &self.config))
        } else {
            frame.morph
        };
        // A hover morph lerps the window size between the compact and the
        // expanded pill, drawing the expanded content into the growing
        // window (a clip-reveal); the anchor stays the compact position, so
        // the pill grows in place. The entrance/exit grow uses the same
        // reveal. Every other frame uses the plain size of the applied
        // layout.
        let (logical_width, logical_height, morph_progress) = if let Some(hover) = &self.hover_expand {
            let progress = hover_progress(hover, &self.config);
            let size = morph_size(&self.config, &content, progress);
            (size.0, size.1, progress)
        } else if let Some(progress) = frame.morph {
            let size = morph_size(&self.config, &content, progress);
            (size.0, size.1, progress)
        } else {
            let size = content_size_of(&self.config, &content, compact);
            (
                size.0,
                size.1,
                MorphProgress {
                    width: 0.0,
                    height: 0.0,
                },
            )
        };
        // The settle-bounce: while the size spring passes its endpoint, the
        // whole pill scales about its anchor past the final size and back
        // (see `bounce_scale`) — the bounce rides the spring itself, so it
        // starts the instant the size completes, with no still pause. The
        // scale multiplies the size (window and hitbox alike) and is applied
        // to the rendered frame in `render_layered`.
        let direction = if let Some(hover) = &self.hover_expand {
            hover.direction
        } else if matches!(self.phase, Phase::Collapsing(_)) {
            MorphDirection::Collapse
        } else {
            MorphDirection::Expand
        };
        let scale_factor = bounce_scale(morph_progress, direction);
        let logical_width = logical_width * scale_factor;
        let logical_height = logical_height * scale_factor;
        let width = (logical_width * dpi).round().max(1.0) as i32;
        let height = (logical_height * dpi).round().max(1.0) as i32;
        self.aura_inset = (AURA_HALO_LOGICAL * dpi).round() as i32;
        // A genuine fullscreen foreground window on the target monitor collapses
        // the work area to the full `rcMonitor`; otherwise the pill anchors
        // against the selected monitor's `rcWork` (taskbar- and app-bar-aware).
        let edge = self.effective_work_area(&target);
        let position = placement(edge, width, height, self.active_pos(), self.aura_inset, dpi);
        let result = render_layered(
            self,
            &content,
            width,
            height,
            dpi,
            frame.alpha,
            position,
            // A morphing frame always renders the expanded pass; `compact`
            // and `morph` can never disagree about whether a morph is in
            // flight (`compact` is derived from the same `morph`).
            compact && morph.is_none(),
            morph,
            scale_factor,
        );
        self.content = Some(content);
        if let Err(error) = result {
            error!("rendering overlay: {error:#}");
        }
    }

    fn frame(&self) -> FrameState {
        match self.phase {
            Phase::Hidden => FrameState { alpha: 0, morph: None },
            Phase::Expanding(start) => {
                let t = normalized_elapsed(&start, animation_duration(&self.config));
                // The live-pill reveal: the pill appears solid immediately
                // (opacity lands within the first ~15 % of the leg — the
                // geometry, not the fade, is the animation) and grows in
                // place from its compact shape on the bouncy grow spring,
                // the width axis leading and the height chasing (see
                // `MorphProgress`). A compact layout pill has nothing to
                // grow into and only appears.
                let grow = ENTRANCE_GROW.value_at(t, 0.0, 0.0);
                let alpha_t = (t / 0.15).min(1.0);
                FrameState {
                    alpha: (ease_out_quint(alpha_t) * 255.0) as u8,
                    morph: if self.effective_compact() {
                        None
                    } else {
                        Some(MorphProgress {
                            width: grow,
                            height: lagged_expand(&ENTRANCE_GROW, t, MORPH_LAG),
                        })
                    },
                }
            }
            Phase::Light(start) => {
                // A quick opacity reveal; with animations disabled the pill is
                // simply solid.
                let progress = if crate::winutil::animations_enabled() {
                    ease_out_quint(start.elapsed().as_secs_f32() / LIGHT_DURATION.as_secs_f32())
                } else {
                    1.0
                };
                FrameState {
                    alpha: (64.0 + progress * 191.0) as u8,
                    morph: None,
                }
            }
            Phase::Shown => {
                if self.config.overlay.layout == LayoutMode::PersistentCompact && self.persistent_faded {
                    // Persistent-compact idle fade: ramp from full to idle
                    // opacity over 300 ms once the dismiss timeout fires;
                    // with animations disabled the idle level applies at once.
                    let idle = 64.0_f32; // 0.25 * 255
                    let fade_start = self.dismiss_at.unwrap_or_else(Instant::now);
                    let t = if crate::winutil::animations_enabled() {
                        (fade_start.elapsed().as_secs_f32() / 0.3).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    let alpha = 255.0 - (255.0 - idle) * t;
                    FrameState {
                        alpha: alpha as u8,
                        morph: None,
                    }
                } else {
                    FrameState {
                        alpha: 255,
                        morph: None,
                    }
                }
            }
            Phase::Collapsing(start) => {
                let t = normalized_elapsed(&start, collapse_duration(&self.config));
                // The exit runs the reveal backwards: the pill springs closed
                // to its compact shape on the mirrored release curve — the
                // width collapsing first, the height lingering behind it —
                // and fades in the last stretch: a cubic fade across the
                // final 75 % of the leg, so the exit stays readable while it
                // closes and never disappears early (the undershoot bounce
                // still shows through the tail of the fade). A compact
                // layout pill just fades out.
                let shrink = spring_collapse(t, 1.0, 0.0);
                let fade_t = (t / 0.75).min(1.0);
                FrameState {
                    alpha: (255.0 * (1.0 - fade_t.powi(3))) as u8,
                    morph: if self.effective_compact() {
                        None
                    } else {
                        Some(MorphProgress {
                            width: shrink,
                            height: lagged_collapse(t, MORPH_LAG, 1.0, 0.0),
                        })
                    },
                }
            }
        }
    }

    /// Re-resolves the target display for the configured monitor mode. Fresh
    /// on every call — the system monitor enumeration is cheap, and handles
    /// are never cached, so a hot-plugged or reordered display takes effect
    /// on the very next frame.
    fn target(&self) -> Option<TargetMonitor> {
        let displays = enumerate_displays_cached();
        let foreground_nearest = foreground_monitor_index(&displays);
        let index = resolve_target(self.position.monitor, &displays, foreground_nearest)?;
        let display = &displays[index];
        let target = TargetMonitor {
            handle: display.handle,
            work: display.work,
            monitor: display.monitor,
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

    /// The rectangle `placement` anchors against for the resolved target
    /// monitor: the target monitor's `rcMonitor` when a genuine fullscreen
    /// foreground window occupies the target monitor (no work-area inset to
    /// respect), otherwise the taskar-/app-bar-aware `rcWork`. A stale work-area
    /// gap is never retained after a fullscreen transition, because Windows is
    /// not assumed to align `rcWork` with `rcMonitor`.
    fn effective_work_area(&self, target: &TargetMonitor) -> RECT {
        let fullscreen = foreground_fullscreens_target(target, self.hwnd);
        effective_position_rect(target.monitor, target.work, fullscreen)
    }

    /// The pill's screen top-left for a `width`×`height` window on the
    /// currently resolved target display, or `None` when no display is
    /// available.
    fn position(&self, width: i32, height: i32) -> Option<POINT> {
        let target = self.target()?;
        let scale = monitor_dpi(target.handle) as f32 / 96.0;
        let edge = self.effective_work_area(&target);
        Some(placement(
            edge,
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
        #[cfg(test)]
        if let Some(over) = self.test_cursor_over {
            return over;
        }
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

    /// Whether a morph-origin expanded pill is currently held by the cursor
    /// (the deferred dismissal). Same inputs as the tick's hold decision, so
    /// an event that lands between ticks routes the same way the next tick
    /// resolves it. Only the compact→expanded hover morph is an interaction
    /// and gets held — a laid-out expanded pill is never held.
    fn held_expanded(&self) -> bool {
        let engaged = hover_engaged(self.last_cursor_over_pill, self.hover_leave_at, Instant::now());
        engaged && self.hover_expand.is_some()
    }

    /// Moves the live overlay window to its resolved position without a full
    /// redraw. When the resolved target's DPI differs from the DPI the current
    /// fonts were built for, a plain move would keep a stale-size pill and
    /// hitbox — rerender instead, which rebuilds the fonts and sizes and
    /// places the window, all against the resolved target.
    fn reposition(&mut self) {
        if matches!(self.phase, Phase::Hidden) {
            return;
        }
        let Some(target) = self.target() else {
            return;
        };
        if needs_font_rebuild(self.fonts.dpi(), monitor_dpi(target.handle)) {
            self.render();
            return;
        }
        let Some((width, height)) = self.content_size() else {
            return;
        };
        let Some(point) = self.position(width, height) else {
            return;
        };
        unsafe {
            if let Err(error) = set_window_pos(
                self.hwnd,
                HWND_TOPMOST,
                point.x,
                point.y,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOOWNERZORDER,
            ) {
                debug!("set_window_pos(reposition) failed: {error}");
            }
        }
    }

    /// Reacts to a foreground-window change, posted from the
    /// `EVENT_SYSTEM_FOREGROUND` hook via `FOREGROUND_CHANGE_MSG`. Re-resolves
    /// the Auto layout and the effective anchor rectangle (rcWork vs rcMonitor)
    /// for the selected target monitor and repositions immediately — instead of
    /// waiting for the next media event or the 250 ms static tick. Layout is
    /// re-evaluated independently of position (so an Explicit Compact/Expanded
    /// pill still moves when the fullscreen inset changes), and the move is
    /// skipped when nothing actually changed since the last resolve (e.g.
    /// Alt-Tab between two normal apps on the same monitor). All foreground and
    /// fullscreen verdicts are recomputed here through the existing
    /// `sample_foreground` / `foreground_fullscreens_target` decisioning, so
    /// this is the only trigger that consults them — no second detection path.
    fn on_foreground_change(&mut self) {
        // While the pill is hidden there is nothing to anchor; bail before any
        // display/monitor enumeration so a foreground switch with no pill up
        // only pays for the posted-message round-trip. Exception:
        // persistent-compact mode may need to resume from an auto-hide.
        let is_persistent = self.config.overlay.layout == LayoutMode::PersistentCompact;
        let was_auto_hidden = is_persistent && matches!(self.phase, Phase::Hidden);
        if matches!(self.phase, Phase::Hidden) && !was_auto_hidden {
            return;
        }
        // Persistent-compact auto-hide: hide the pill while a fullscreen or
        // listed `auto_compact_sources` app is the foreground window and the
        // toggle is enabled. When the foreground clears, resume the held
        // content (saved before hide() cleared it).
        if is_persistent && self.config.behavior.hide_for_auto_compact_sources {
            let verdict = self.sample_foreground();
            let should_hide =
                verdict.fullscreen || fullscreen::auto_source_matches(&self.config, verdict.exe.as_deref());
            // Track whether dismiss should collapse (not fade to idle). Cleared
            // when the foreground is not fullscreen/listed so the pill fades
            // normally after the user returns to a non-fullscreen app.
            self.persistent_collapse_on_dismiss = should_hide;
            if should_hide && !was_auto_hidden {
                // Save content before hide() clears it, so resume can restore it.
                // hide() calls show_next(), which may show a pending event;
                // show_with_duration will overwrite held_content for that event,
                // which is correct — the pending event's pill is the one that
                // was shown and collapsed. If no pending event, the save survives.
                self.held_content = self.content.clone();
                self.hide();
                return;
            }
            if was_auto_hidden && !should_hide {
                // Resume: re-show the content that was saved before hide().
                if let Some(event) = self.held_content.take() {
                    let full_animation = matches!(event, MediaEvent::TrackChanged(_));
                    self.show(event, full_animation);
                }
                return;
            }
        }
        let before_layout = self.layout;
        self.refresh_layout();
        let layout_flipped = self.layout != before_layout;
        // Re-resolve the target (per-frame enumeration, never cached) and the
        // current rcWork/rcMonitor anchor for the SELECTED monitor. The
        // fullscreen verdict comes from `effective_work_area` ->
        // `foreground_fullscreens_target`, the single source of truth.
        let Some(target) = self.target() else {
            // No display available: return without touching `last_anchor_edge`
            // so a transient no-display state cannot poison the §11 skip guard.
            return;
        };
        let edge = self.effective_work_area(&target);
        // Skip the redundant reposition/render when the foreground switch did
        // not move the anchor and did not flip the Auto layout (e.g. Alt-Tab
        // between two normal apps on the same monitor). The first resolve
        // (`last_anchor_edge == None`) always proceeds. The resolved `edge` is
        // authoritative (recomputed from scratch here), so a stale cached value
        // can at most cause one extra move, never a misplacement.
        if anchor_unchanged(self.last_anchor_edge, edge, layout_flipped) {
            return;
        }
        self.last_anchor_edge = Some(edge);
        if layout_flipped {
            // Compact<->Expanded changed the pill size and content layout: a
            // full re-render re-blits at the new dimensions and re-applies the
            // position. The animation phase is preserved by render() (it reads
            // self.phase via frame()), so a foreground switch never restarts an
            // in-flight expand/collapse/hover morph.
            self.render();
        } else {
            // Layout unchanged but the rcWork<->_rcMonitor anchor may have
            // moved (or didn't), so re-resolve the position and move the window
            // without a re-blitting.
            self.reposition();
        }
    }

    fn hide(&mut self) {
        debug!("pill hidden");
        self.content_rev += 1;
        self.content = None;
        self.dismiss_at = None;
        self.hover_dismiss_at = None;
        self.hover_expand = None;
        self.hover_expanded_once = false;
        self.hover_leave_at = None;
        self.content_fade = None;
        self.persistent_collapse_on_dismiss = false;
        self.phase = Phase::Hidden;
        // Release the per-show render state: the next show re-converts the
        // artwork and rebuilds the marquee rasters from the cached track, so
        // an idle pill holds no decoded cover or raster buffers. The
        // size-reuse buffers (`dib`, `frame_scratch`) and the caches
        // (`track_cache`, fonts) stay.
        self.decoded_art = None;
        self.decoded_art_source = None;
        self.palette = None;
        self.content_palette = None;
        self.marquee_strips = [None, None, None, None];
        self.pill_text = None;
        // Nothing is on screen anymore, so the accessible name must go quiet
        // too: a screen reader that re-queries the hidden pill (which still
        // exists as an HWND) must not announce a track that is no longer
        // displayed.
        if let Some(cell) = &self.pill_name
            && let Ok(mut guard) = cell.lock()
        {
            *guard = None;
        }
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
            if let Err(error) = set_window_pos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_HIDEWINDOW | SWP_NOACTIVATE | SWP_NOZORDER,
            ) {
                debug!("set_window_pos(hide) failed: {error}");
            }
        }
        // The size-reuse buffers (`dib`, `frame_scratch`) only pay off for
        // closely-spaced pills; when the pill stays hidden, schedule their
        // release so a long-idle process holds no frame DIBs. Every show path
        // kills the timer, so this only fires if no fresh pill appears within
        // the deadline; the buffers are rebuilt lazily on the next show.
        unsafe {
            let _ = set_timer(self.hwnd, IDLE_BUFFER_TIMER_ID, IDLE_BUFFER_RELEASE_MS, None);
        }
        // Advance the queue: the next pending notification shows as a fresh
        // pill. show() checks `enabled`, so a toggle-off collapse stays hidden.
        self.show_next();
        // Auto-hide watchdog: while the pill stays hidden with held content
        // (PersistentCompact auto-hide for a fullscreen/listed foreground),
        // keep a coarse 1 s timer polling the foreground. A same-window
        // fullscreen-exit (F11 / Alt+Enter) never fires EVENT_SYSTEM_FOREGROUND,
        // so without it the held pill would stay hidden until the next media
        // event or foreground change. Only the held state arms it (and only
        // while notifications are enabled — a watchdog that cannot show
        // anything must not poll); any other hide leaves no timer running
        // (deleted above; hidden ticks no-op).
        self.hidden_watchdog = self.enabled
            && self.config.overlay.layout == LayoutMode::PersistentCompact
            && self.config.behavior.hide_for_auto_compact_sources
            && self.held_content.is_some()
            && matches!(self.phase, Phase::Hidden);
        if self.hidden_watchdog {
            self.tick_period = 1000;
            self.ensure_anim_timer();
        }
    }

    /// Releases the size-reuse buffers after the pill has been hidden for a
    /// long stretch (fired by `IDLE_BUFFER_TIMER_ID`). The next show rebuilds
    /// them lazily (`dib_for`, `clear_frame_scratch`, and the text scratch
    /// creation), so the cost of a release is one CreateDIBSection round on
    /// the next pill, not on the release itself.
    fn release_idle_buffers(&mut self) {
        // Dropping the buffers unselects the bitmaps and frees the DIBs and
        // DCs via the `Drop` impls.
        self.text_scratch = None;
        self.dib = None;
        self.frame_scratch = Vec::new();
        // The retained chrome raster too: a hidden pill would otherwise keep
        // the last background cached for the rest of the session. The next
        // show always rebuilds it (a hide bumps `content_rev`, so the key
        // never matches), which is already the cost of showing anyway.
        self.chrome_cache = None;
        debug!("released idle overlay buffers");
    }

    fn toggle_enabled(&mut self) {
        self.enabled = !self.enabled;
        info!("notifications {}", if self.enabled { "enabled" } else { "disabled" });
        if !self.enabled {
            self.pending.clear();
            self.hide();
        } else {
            // Re-enabling: surface the last shown track immediately. The worker
            // re-reads the current session within ~2s (its poll detects the
            // config write) and re-emits the live track, which then refreshes
            // this pill in place via the same_media / same_source_shown dedup.
            // This fast-path closes the ordering edge case in which a queued
            // MEDIA_EVENT_MSG is drained and dropped by `!self.enabled` before
            // the toggle flips it; the worker's re-emit then lands on the
            // now-enabled overlay and corrects or refreshes the pill.
            // Preferred-source pinning: while the pin is actually playing, the
            // pill's resting state is its pinned source, so the restore
            // prefers the pinned track over whatever happened to be showing
            // when notifications were disabled (the same "swap only to
            // sources still playing" gate as the dismiss-deadline return, so
            // a paused/stopped pin never gets restored). The worker's re-emit
            // then corrects the cached track if the pin changed songs while
            // disabled.
            // Otherwise restore the most recent cached track that is
            // *actually playing* instead of the pre-disable last-shown track:
            // the cache is kept fresh while notifications are disabled, so a
            // song change on any playing source is surfaced on re-enable (the
            // same "swap only to sources still playing" discipline; the
            // worker's ~2s re-emit then refreshes artwork in place). This
            // applies both with no pin and when the pin exists but cannot be
            // restored (paused/stopped/unknown): `best_successor("")`
            // excludes no source, and the playing filter can never return the
            // pin's own track in the paused case — a playing pin would have
            // been served by `pinned_track` above.
            if let Some(track) = self.pinned_track() {
                self.show(MediaEvent::TrackChanged(track), true);
            } else if let Some(track) = self.best_successor("") {
                self.show(MediaEvent::TrackChanged(track), true);
            } else if let Some(held) = self.held_content.take() {
                self.show(held, true);
            } else if let Some(track) = self.last_track.clone() {
                self.show(MediaEvent::TrackChanged(track), true);
            }
            // If none is available, the worker's re-show read surfaces the
            // current track through the normal receive_events path.
        }
    }

    /// Current (scaled) pixel size of the shown content, or `None` while
    /// hidden. Sized at the resolved target monitor's DPI — the window's own
    /// DPI can lag the target right after a monitor switch, which would keep
    /// a stale hitbox. `render()` and `position()` scale with this same value.
    fn content_size(&self) -> Option<(i32, i32)> {
        let target = self.target()?;
        self.content_size_at(monitor_dpi(target.handle) as f32 / 96.0)
    }

    /// The shown content's pixel size at an explicit scale (logical × `dpi`),
    /// tracking the active morph and the settle bounce so the hitbox matches
    /// the visible pill. Pure geometry — the caller resolves the DPI.
    fn content_size_at(&self, dpi: f32) -> Option<(i32, i32)> {
        let content = self.content.as_ref()?;
        let frame = self.frame();
        // The hitbox must track the morph, or the cursor would stop being
        // "over" the pill the moment it outgrows the compact size.
        let (logical_width, logical_height, morph_progress) = if let Some(morph) = &self.hover_expand {
            let progress = hover_progress(morph, &self.config);
            let size = morph_size(&self.config, content, progress);
            (size.0, size.1, progress)
        } else if let Some(progress) = frame.morph {
            let size = morph_size(&self.config, content, progress);
            (size.0, size.1, progress)
        } else {
            let size = content_size_of(&self.config, content, self.layout == LayoutMode::Compact);
            (
                size.0,
                size.1,
                MorphProgress {
                    width: 0.0,
                    height: 0.0,
                },
            )
        };
        // The settle-bounce scales the size too, so the hitbox matches the
        // visible pill (see `bounce_scale`).
        let direction = if let Some(morph) = &self.hover_expand {
            morph.direction
        } else if matches!(self.phase, Phase::Collapsing(_)) {
            MorphDirection::Collapse
        } else {
            MorphDirection::Expand
        };
        let scale_factor = bounce_scale(morph_progress, direction);
        let logical_width = logical_width * scale_factor;
        let logical_height = logical_height * scale_factor;
        let width = (logical_width * dpi).round().max(1.0) as i32;
        let height = (logical_height * dpi).round().max(1.0) as i32;
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
            let _ = kill_timer(self.hwnd, IDLE_BUFFER_TIMER_ID);
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
        self.content_rev += 1;
        self.content = Some(content);
        self.resolve_pill_text();
        self.reset_scroll();
        let now = Instant::now();
        self.dismiss_at = Some(now + sample_duration(&self.config));
        // A fresh pill must not inherit hover state from the previous one
        // (same reset `show_with_duration` and `hide` perform): a stale
        // `hover_expand` would render the sample already mid-morph or fully
        // expanded with the cursor nowhere near it, and seed the sample's
        // collapse from a velocity that belonged to a different hover.
        self.hover_dismiss_at = None;
        self.hover_expand = None;
        self.hover_expanded_once = false;
        self.hover_leave_at = None;
        self.content_fade = None;
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
/// caller. See `winutil::StateClaim` for the shared mechanics.
static OVERLAY_STATE_CLAIMED: StateClaim = StateClaim::new();
/// The overlay window handle the `EVENT_SYSTEM_FOREGROUND` hook callback
/// forwards its message to. Written once in `create_window` (after the overlay
/// window succeeds) and cleared in `WM_NCDESTROY`; a racing callback that fires
/// during/after teardown reads `0` and no-ops (`PostMessageW` to `HWND(0)` is
/// harmlessly ignored by the system). Relaxed reads are enough (a stale `0`
/// only yields a missed, harmless no-op); `SeqCst` on the store matches
/// `OVERLAY_STATE_CLAIMED`.
static OVERLAY_FG_HWND: AtomicU64 = AtomicU64::new(0);

/// Creates the passive WinGlance overlay window. It owns no message loop: the caller
/// runs the loop and destroys the window at exit.
pub(crate) fn create_window(
    config: Config,
    queue: EventQueue,
    wake: Arc<AtomicBool>,
    now_showing: Arc<Mutex<Option<String>>>,
) -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.context("getting the process module")?;
    let instance: HINSTANCE = module.into();
    let class_name = wide("WinGlanceOverlayWindow");
    register_window_class(instance, &class_name)?;

    let mut state = Box::new(OverlayState::new(config, queue));
    state.wake = wake;
    state.now_showing = Some(now_showing);
    // The accessible-name cell is attached here so `resolve_pill_text` (the
    // single content-change choke point) can always write it; the UIA
    // provider built in `WM_GETOBJECT` reads the same cell.
    state.pill_name = Some(Arc::new(Mutex::new(None)));
    let state_ptr = Box::into_raw(state);
    OVERLAY_STATE_CLAIMED.reset();
    let hwnd = unsafe {
        crate::winapi::create_window(
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
        Ok(hwnd) => {
            // WM_NCCREATE ran synchronously inside CreateWindowExW and already
            // claimed `state_ptr` (OVERLAY_STATE_CLAIMED) and set state.hwnd.
            // Publish the handle to the foreground hook callback and arm the
            // EVENT_SYSTEM_FOREGROUND hook. Best-effort: if the hook cannot be
            // installed, foreground repositioning falls back to the 250 ms
            // static tick (`tick_layout_check`) — never block startup over it.
            OVERLAY_FG_HWND.store(hwnd.0 as u64, Ordering::SeqCst);
            let hook = unsafe {
                crate::winapi::set_win_event_hook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_FOREGROUND,
                    None,
                    Some(foreground_hook_cb),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            if hook.0.is_null() {
                warn!(
                    "SetWinEventHook(EVENT_SYSTEM_FOREGROUND) failed; foreground repositioning falls back to the 250 ms static tick"
                );
            } else {
                unsafe {
                    (*state_ptr).hook = Some(hook);
                }
            }
            Ok(hwnd)
        }
        Err(error) => {
            // The state box is owned by the window from WM_NCCREATE onward and
            // freed in WM_NCDESTROY. WM_NCCREATE flips OVERLAY_STATE_CLAIMED
            // when it takes the box; if it never ran (a creation failure before
            // the window object existed), the box still belongs to us and must
            // be freed here — otherwise it leaks. When WM_NCCREATE did run,
            // the system tears the window down through WM_NCDESTROY first, so
            // freeing the box here would double-free it.
            if let Some(state) = OVERLAY_STATE_CLAIMED.take_unclaimed(state_ptr) {
                drop(state);
            }
            Err(error.into())
        }
    }
}

/// WinEvent callback for `EVENT_SYSTEM_FOREGROUND`. Runs on the calling (UI)
/// thread under `WINEVENT_OUTOFCONTEXT` (the system delivers it there; it is
/// never injected into another process), so it must not touch `OverlayState` or
/// perform Win32 work itself. It only forwards a lightweight message to the
/// overlay window and returns — the real re-resolve happens later in the
/// `FOREGROUND_CHANGE_MSG` handler on the UI thread.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn foreground_hook_cb(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _dw_event_thread: u32,
    _dw_ms_event_time: u32,
) {
    // A contained panic no-ops the hook; the next foreground event
    // retries.
    crate::winutil::guarded_void("the foreground WinEvent hook", || {
        let target = OVERLAY_FG_HWND.load(Ordering::Relaxed);
        if target != 0 {
            // Post only — never SendMessageW (would block the system foreground
            // dispatch). A null hwnd (teardown race) is harmless: PostMessageW to
            // an invalid window simply returns FALSE, which we discard.
            let _ = post_message(HWND(target as *mut c_void), FOREGROUND_CHANGE_MSG, WPARAM(0), LPARAM(0));
        }
    });
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

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // The body is panic-contained; a panic logs, posts quit (normal
    // teardown) and answers with DefWindowProcW instead of unwinding across
    // the ABI.
    crate::winutil::guarded_wndproc(
        hwnd,
        message,
        wparam,
        lparam,
        "the overlay window procedure",
        || unsafe { window_proc_body(hwnd, message, wparam, lparam) },
    )
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn window_proc_body(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            let state = (*create).lpCreateParams as *mut OverlayState;
            if !state.is_null() {
                set_window_state(hwnd, state);
                (*state).hwnd = hwnd;
                OVERLAY_STATE_CLAIMED.claim();
            }
        }
    }

    let state_ptr = window_state::<OverlayState>(hwnd);
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_GETOBJECT => {
            // UI Automation asks for a provider with lParam == UiaRootObjectId;
            // MSAA OBJID_* queries keep the DefWindowProcW answer. The pill
            // gets a read-only name provider only (see accessibility.rs): it
            // exposes the current track as the accessible name and nothing
            // else — no patterns, no focus, no clicks, preserving the pill's
            // passive architecture. The provider reads a shared name cell the
            // UI thread updates on content changes, so no window state is
            // dereferenced off the UI thread.
            if lparam.0 == UiaRootObjectId as isize
                && !state_ptr.is_null()
                && let Some(cell) = &(*state_ptr).pill_name
            {
                let provider = crate::accessibility::pill_name_provider(hwnd, std::sync::Arc::clone(cell));
                return UiaReturnRawElementProvider(hwnd, wparam, lparam, &provider);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_PAINT => {
            let _ = validate_rect(hwnd, None);
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
            // changed). Invalidate the display snapshot FIRST: a monitor that
            // was just removed or reordered must not resolve against the
            // up-to-1-second-old cache, or the pill could be placed onto a
            // stale handle. The per-frame target resolution picks the new
            // layout up on its own; here, a visible pill is moved onto the
            // re-resolved target immediately instead of waiting for the next
            // tick (reposition rerenders when the new target's DPI differs),
            // and the refresh-rate cache is dropped so the animation timer
            // re-samples the target's rate.
            invalidate_display_cache();
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                state.period_cache = None;
                if !matches!(state.phase, Phase::Hidden) {
                    state.reposition();
                }
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // The window was moved onto a display with a different DPI. This
            // window (not the system) owns its placement: ignore the suggested
            // rect and reposition at the resolved target instead — reposition
            // rerenders when the target's DPI differs from the fonts' DPI, so
            // the pill re-sizes and re-places at the new DPI with no stale
            // size or hitbox. While hidden there is nothing to move (the next
            // show renders against the then-current target).
            if !state_ptr.is_null() && !matches!((*state_ptr).phase, Phase::Hidden) {
                let state = &mut *state_ptr;
                state.reposition();
            }
            LRESULT(0)
        }
        FOREGROUND_CHANGE_MSG => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                // Collapse a burst of foreground switches (Alt-Tab chains, focus
                // races, a fullscreen-source burst) into a single resolve: only
                // the latest settled foreground matters. The hook callback posts
                // one message per event, so drain the queued siblings before
                // acting — the same coalescence the animation tick uses.
                let mut pending = MSG::default();
                while crate::winapi::peek_message(
                    &mut pending,
                    hwnd,
                    FOREGROUND_CHANGE_MSG,
                    FOREGROUND_CHANGE_MSG,
                    PM_REMOVE,
                ) {}
                state.on_foreground_change();
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
                while crate::winapi::peek_message(&mut msg, hwnd, TIMER_ANIMATION_MSG, TIMER_ANIMATION_MSG, PM_REMOVE) {
                }
            }
            if !state_ptr.is_null() {
                (*state_ptr).tick();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // Disconnect the UIA provider while the window and its state still
            // exist — the same defensive detach the main window applies.
            crate::accessibility::detach_hwnd_provider(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                // Tear down the foreground hook before releasing the state box:
                // a racing callback after this point must see a null overlay
                // handle (it no-ops), and the hook handle is freed here — so
                // unhook, then clear the routing static, then drop the box.
                if let Some(hook) = state.hook.take() {
                    let unhooked = unsafe { UnhookWinEvent(hook) };
                    if !unhooked.as_bool() {
                        debug!("UnhookWinEvent failed");
                    }
                }
                OVERLAY_FG_HWND.store(0, Ordering::SeqCst);
                state.delete_anim_timer();
                // Null the shared accessible-name cell last: a provider that
                // outlives the window must read an empty name, not the last
                // track (see `null_pill_name_cell`). Placed after the hook
                // and timer teardown so no racing callback can repopulate it
                // before the box is released.
                state.null_pill_name_cell();
                // Dropping the scratch buffers unselects the bitmaps and
                // frees the DIBs and DCs via the `Drop` impls; the state box
                // is released right after — slot clear first, box second, via
                // the shared helper (the canonical order every window applies).
                state.text_scratch = None;
                state.dib = None;
                release_window_state(hwnd, state_ptr);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Test-only imports: the bin compilation must not see these (the moved
    // modules and the windows APIs below are used only by the tests).
    use super::fullscreen::{DisplayInfo, auto_source_matches, rect_covers_monitor};
    use super::morph::{
        EXPAND_SPRING, animation_duration_with, art_edge_gate, compact_alpha, compact_metrics, compact_size,
        compact_title_viewport, content_size, dim_color, expanded_alpha, lag_progress, morph_art_tile, morph_icon_pos,
        morph_radius, morph_symbol_pos, morph_title_band, row_unveil_alpha, spring_expand,
    };
    use super::render::{
        FILL_TINT_WEIGHT, RenderLayer, blend_frames, blit_packed_rows, circle_coverage, clear_frame_scratch,
        clock_icon_coverage, composite_marquee_strip, contrast_ratio, draw_aura, draw_clock_icon_pixels,
        draw_compact_pill, draw_icon_scaled, draw_pixels, draw_symbol_pixels, draw_text_line_pixels, draw_text_pixels,
        edge_fade_factor, muted_accent, playback_state_for_track, round_rect_coverage, round_rect_coverage_fast,
        round_rect_coverage_supersampled, rounded_triangle_coverage, scale_frame_about, shrink_frame_scratch,
        tinted_fill,
    };
    use crate::events::{ARTWORK_DECODE, PlaybackType};
    use std::ptr::null_mut;
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::{
        ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE, OUT_DEFAULT_PRECIS,
    };
    use windows::Win32::Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
        DrawTextW, HMONITOR, SetBkMode, SetTextColor, TRANSPARENT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DestroyWindow, DispatchMessageW, GetMessageW, TranslateMessage, WINDOW_EX_STYLE, WM_NULL,
    };

    #[test]
    fn needs_font_rebuild_is_true_only_when_the_target_dpi_differs() {
        // Same DPI on both sides: the pill's fonts already match the target.
        assert!(!needs_font_rebuild(96, 96));
        assert!(!needs_font_rebuild(144, 144));
        // A mismatch — monitor switch, WM_DISPLAYCHANGE, or the window moved
        // onto a different-DPI display — must force a rerender so the pill is
        // sized for the target, never for its stale fonts.
        assert!(needs_font_rebuild(96, 144));
        assert!(needs_font_rebuild(144, 96));
        // A font provider that was never built (dpi 0, see `OverlayState::new`)
        // must also rerender on the first non-hidden frame.
        assert!(needs_font_rebuild(0, 96));
    }

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
    /// BGRA at the fixed `ARTWORK_DECODE`² size (the overlay only reads the
    /// side from the buffer length).
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
    fn ensure_art_prefers_the_worker_palette_over_recomputation() {
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        let red = pm_art([200, 40, 40]);
        let blue = pm_art([40, 40, 200]);
        // Without a track palette the palette derives from the converted art.
        state.ensure_art(Some(&red));
        let derived_red = state.palette.expect("a solid cover must yield a palette");
        // A worker palette wins and is immune to byte changes: a re-encoded
        // thumbnail converts a new buffer, but the accent must not shift.
        let worker_palette = Palette {
            primary: [0x11, 0x22, 0x33, 0xFF],
            secondary: [0x44, 0x55, 0x66, 0xFF],
        };
        state.content_palette = Some(worker_palette);
        state.ensure_art(Some(&blue));
        assert_eq!(state.palette, Some(worker_palette));
        assert_ne!(
            state.palette,
            Some(derived_red),
            "the worker palette must replace a previously derived one"
        );
        // Same art bytes with the worker palette: the early return keeps
        // both the buffer and the palette stable.
        let before = state.decoded_art.clone();
        state.ensure_art(Some(&blue));
        assert_eq!(state.decoded_art, before);
        assert_eq!(state.palette, Some(worker_palette));
        // A state pill (no track palette) with NEW art derives again.
        state.content_palette = None;
        let yellow = pm_art([240, 220, 40]);
        state.ensure_art(Some(&yellow));
        assert_ne!(state.palette, Some(worker_palette));
        assert!(state.palette.is_some());
    }

    #[test]
    fn draw_icon_scaled_rejects_a_short_icon_buffer() {
        let mut pixels = vec![0u8; 40 * 40 * 4];
        // An icon shorter than icon_size^2 * 4 must be a no-op, not an
        // out-of-bounds read.
        draw_icon_scaled(&mut pixels, 40, &[0u8; 10], 24, 0, 0, 24, 1.0);
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
    fn accessible_name_cell_tracks_the_resolved_pill_text_and_clears_on_hide() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        let cell = Arc::new(Mutex::new(None));
        state.pill_name = Some(cell.clone());

        // A track pill names the title — artist (source).
        state.content = Some(MediaEvent::TrackChanged(track_for(
            "spotify",
            "Love Me Not",
            "Ravyn Lenae",
        )));
        state.resolve_pill_text();
        assert_eq!(
            *cell.lock().unwrap(),
            Some("Love Me Not — Ravyn Lenae (spotify)".to_string())
        );

        // A state pill names the cached track of its source; without a cached
        // track it falls back to the source app alone.
        state.cache_track(&track_for("youtube-music", "Nights", "Frank Ocean"));
        state.content = Some(MediaEvent::PlaybackStateChanged(
            PlaybackState::Paused,
            "youtube-music".into(),
        ));
        state.resolve_pill_text();
        assert_eq!(
            *cell.lock().unwrap(),
            Some("Nights — Frank Ocean (youtube-music)".to_string())
        );

        state.content = Some(MediaEvent::PlaybackStateChanged(
            PlaybackState::Paused,
            "unknown-source".into(),
        ));
        state.resolve_pill_text();
        assert_eq!(
            *cell.lock().unwrap(),
            Some("unknown-source".to_string()),
            "a state pill with no cached track names the source app"
        );

        // Hiding the pill goes quiet: the name must not linger for a hidden
        // window.
        state.hide();
        assert_eq!(*cell.lock().unwrap(), None);
    }

    #[test]
    fn accessible_name_cell_builds_every_part_combination() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        let cell = Arc::new(Mutex::new(None));
        state.pill_name = Some(cell.clone());

        // Title only: the empty artist part is dropped, the source is
        // parenthesized.
        state.content = Some(MediaEvent::TrackChanged(track_for("spotify", "Love Me Not", "")));
        state.resolve_pill_text();
        assert_eq!(*cell.lock().unwrap(), Some("Love Me Not (spotify)".to_string()));

        // No title and no artist: the name degrades to the source app alone.
        state.content = Some(MediaEvent::TrackChanged(track_for("spotify", "", "")));
        state.resolve_pill_text();
        assert_eq!(*cell.lock().unwrap(), Some("spotify".to_string()));

        // No source: the joined title — artist stands alone.
        state.content = Some(MediaEvent::TrackChanged(track_for("", "Love Me Not", "Ravyn Lenae")));
        state.resolve_pill_text();
        assert_eq!(*cell.lock().unwrap(), Some("Love Me Not — Ravyn Lenae".to_string()));
    }

    #[test]
    fn only_genuine_track_changes_announce_the_pill_name() {
        let track = MediaEvent::TrackChanged(track_for("spotify", "Love Me Not", "Ravyn Lenae"));
        let paused = MediaEvent::PlaybackStateChanged(PlaybackState::Paused, "spotify".into());
        assert!(OverlayState::announces_pill_name_change(&Some(track)));
        assert!(
            !OverlayState::announces_pill_name_change(&Some(paused)),
            "a play/pause transition never announces, even when it changes the name"
        );
        assert!(!OverlayState::announces_pill_name_change(&None));
    }

    #[test]
    fn nulling_the_name_cell_makes_a_client_held_clone_read_empty() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        // The "client" is a provider handed to UIA core: it holds its own
        // clone of the cell and outlives the window.
        let client = Arc::new(Mutex::new(None));
        state.pill_name = Some(client.clone());
        state.content = Some(MediaEvent::TrackChanged(track_for(
            "spotify",
            "Love Me Not",
            "Ravyn Lenae",
        )));
        state.resolve_pill_text();
        assert_eq!(
            *client.lock().unwrap(),
            Some("Love Me Not — Ravyn Lenae (spotify)".to_string())
        );

        // Teardown (WM_NCDESTROY): the overlay drops its reference and
        // clears the contents — the client's clone stays alive but must now
        // read empty, not the last track name.
        state.null_pill_name_cell();
        assert!(state.pill_name.is_none(), "the overlay no longer holds the cell");
        assert_eq!(
            *client.lock().unwrap(),
            None,
            "a client-held provider must read empty after teardown"
        );
    }

    #[test]
    fn retire_source_purges_the_retired_source_from_the_track_cache() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.cache_track(&track_for("alpha", "Song A", "Artist"));
        state.cache_track(&track_for("zeta", "Song Z", "Artist"));
        assert!(state.track_cache.contains_key("alpha"));
        // Retiring a source that is not on screen must still drop its cache
        // entry: the purge runs before the early return.
        state.retire_source("alpha");
        assert!(!state.track_cache.contains_key("alpha"));
        assert!(!state.track_cache_order.iter().any(|s| s == "alpha"));
        assert!(state.track_cache.contains_key("zeta"));
        assert!(state.track_cache_order.iter().any(|s| s == "zeta"));
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
    fn track_cache_retains_idle_entries_until_the_cap_evicts_them() {
        // Retention is indefinite once an entry is inserted: only the cap
        // evicts. An idle (never-refreshed) entry survives inserts below the
        // cap — the successor rule's playback-state gate, not a timeout, is
        // what keeps a stopped source's track from ever being announced.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state
            .track_cache
            .insert("stale".into(), track_for("stale", "Old", "Song"));
        state.track_cache_order.push_back("stale".into());
        state.cache_track(&track_for("fresh", "New", "Song"));
        assert!(
            state.track_cache.contains_key("stale"),
            "an idle entry must be retained below the cap"
        );
        assert!(state.track_cache.contains_key("fresh"));
        assert_eq!(state.track_cache.len(), 2);
        // ... and the cap still evicts the oldest entries once exceeded.
        for i in 0..TRACK_CACHE_CAP {
            state.cache_track(&track_for(&format!("more-{i}"), "Song", "Artist"));
        }
        assert_eq!(state.track_cache.len(), TRACK_CACHE_CAP);
        assert!(
            !state.track_cache.contains_key("stale"),
            "the oldest entries must be evicted at the cap"
        );
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
            track_for("youtube-music", "Old Song", "Old Artist"),
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
        state
            .track_cache
            .insert("youtube-music".into(), track_for("youtube-music", "Song", "Artist"));

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
            "a Playing re-announcement adds nothing while the source's track pill is still visible"
        );
        assert!(
            matches!(state.content.as_ref(), Some(MediaEvent::TrackChanged(_))),
            "the track pill must stay on screen (note symbol) for a redundant Playing"
        );
    }

    #[test]
    fn same_source_pause_or_stop_updates_the_track_pill_in_place() {
        // Regression: a genuine Paused/Stopped that arrives while the source's
        // TrackChanged pill is still on screen used to be dropped wholesale, so
        // a pause right after a track change never showed — the event was lost
        // to the dismiss timer. It now refreshes the shown pill in place
        // (♪ -> ⏸/⏹) using the cached track's text.
        for state_value in [PlaybackState::Paused, PlaybackState::Stopped] {
            let mut state = OverlayState::new(Config::default(), EventQueue::default());
            let track = track_for("youtube-music", "Song", "Artist");
            // The track pill caches its track so the in-place state pill can
            // resolve the title/artist without a new TrackChanged.
            state.track_cache.insert("youtube-music".into(), track.clone());
            state.content = Some(MediaEvent::TrackChanged(track));
            state.phase = Phase::Shown;
            state.dismiss_at = Some(Instant::now() + Duration::from_secs(3));

            state
                .queue
                .lock()
                .unwrap()
                .push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                    state_value,
                    "youtube-music".into(),
                )));
            state.receive_events();

            assert!(
                state.pending.is_empty(),
                "a same-source state flip must not queue a second pill: {:?}",
                state_value
            );
            assert!(
                matches!(
                    state.content.as_ref(),
                    Some(MediaEvent::PlaybackStateChanged(s, source))
                        if *s == state_value && source == "youtube-music"
                ),
                "the track pill must refresh in place to {:?}, got {:?}",
                state_value,
                state.content.as_ref().map(|e| format!("{e:?}")).unwrap_or_default()
            );
            let remaining = state.dismiss_at.unwrap().saturating_duration_since(Instant::now());
            assert!(
                remaining >= Duration::from_millis(2900),
                "the refreshed pill must keep the full duration from this change, got {remaining:?} for {state_value:?}"
            );
        }
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
            matches!(state.phase, Phase::Expanding(_)),
            "the toggle must rescue the collapsing pill by resuming the entrance, not snapping to shown"
        );
    }

    #[test]
    fn displayed_content_publishes_its_source_to_the_shared_cell() {
        // The SMTC worker's session-recreation gate must compare against the
        // source the pill actually shows. Every display path — an in-place
        // swap and a fresh show — publishes the content's source into the
        // shared cell the worker reads.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        let cell: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        state.now_showing = Some(cell.clone());

        state.update_content(
            MediaEvent::TrackChanged(track_for("youtube-music", "Song", "Artist")),
            Duration::from_millis(100),
        );
        assert_eq!(
            *cell.lock().unwrap(),
            Some("youtube-music".to_string()),
            "an in-place content swap must publish its source"
        );

        state.show_with_duration(
            MediaEvent::PlaybackStateChanged(PlaybackState::Paused, "Brave".into()),
            false,
            3000,
        );
        assert_eq!(
            *cell.lock().unwrap(),
            Some("Brave".to_string()),
            "a fresh show must publish its source"
        );
    }

    #[test]
    fn persistent_batch_before_first_show_ends_on_the_latest_track() {
        // Startup burst: two cross-source tracks arrive before the first
        // pill ever shows. The first must show directly; the second must
        // swap in place — queueing either would strand it forever.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.config.overlay.layout = LayoutMode::PersistentCompact;
        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "Brave", "Song A", "Artist",
            ))));
        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "youtube-music",
                "Song B",
                "Artist",
            ))));
        state.receive_events();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t))
                    if t.source_app == "youtube-music" && t.title == "Song B"
            ),
            "the batch must end on the latest track"
        );
        assert!(state.pending.is_empty());
        assert!(!matches!(state.phase, Phase::Hidden));
    }

    #[test]
    fn persistent_state_event_before_first_show_shows_directly() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.config.overlay.layout = LayoutMode::PersistentCompact;
        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Paused,
                "youtube-music".into(),
            )));
        state.receive_events();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::PlaybackStateChanged(PlaybackState::Paused, s)) if s == "youtube-music"
            ),
            "the first state event of a persistent run must show, not queue"
        );
        assert!(state.pending.is_empty());
        assert!(!matches!(state.phase, Phase::Hidden));
    }

    #[test]
    fn persistent_auto_hidden_surfaces_updates_briefly_over_fullscreen() {
        // Auto-hidden (fullscreen/listed foreground with hide_for_auto_compact_sources
        // on): the pill is hidden but still active. A meaningful update must surface
        // briefly like a compact notification — full configured duration, then
        // collapse back into the auto-hidden hold — so track changes no longer vanish
        // over fullscreen/listed apps; the resume must still re-show the latest track
        // when the foreground clears.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.config.overlay.layout = LayoutMode::PersistentCompact;
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: true,
        });
        state.test_cursor_over = Some(false);
        // Auto-hide hold from a prior track.
        state.held_content = Some(MediaEvent::TrackChanged(track_for("Brave", "Song A", "Artist")));
        state.phase = Phase::Hidden;
        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "youtube-music",
                "Song B",
                "Artist",
            ))));
        state.receive_events();

        // The update must surface the pill, not vanish into the held content.
        assert!(
            !matches!(state.phase, Phase::Hidden),
            "an auto-hidden persistent pill must surface the update, not stay hidden"
        );
        assert!(
            state.content.as_ref().is_some_and(
                |c| matches!(c, MediaEvent::TrackChanged(t) if t.source_app == "youtube-music" && t.title == "Song B")
            ),
            "the latest track must be on screen"
        );
        assert!(
            state.dismiss_at.is_some(),
            "the surfaced update must have a dismiss deadline"
        );
        assert!(
            state.persistent_collapse_on_dismiss,
            "dismiss must collapse into a hide over the fullscreen foreground"
        );
        assert!(state.pending.is_empty());
        // The resume hold tracks the latest update so the foreground-clear resume
        // re-shows it, not the stale prior track.
        assert!(
            matches!(
                state.held_content.as_ref(),
                Some(MediaEvent::TrackChanged(t)) if t.title == "Song B"
            ),
            "held content must track the latest update for resume"
        );

        // At the deadline the temporary pill takes the collapse-to-hide route
        // instead of the idle fade, so it does not linger at dimmed opacity over
        // the fullscreen app; the auto-hidden state is restored with the newest
        // held content for the resume.
        state.dismiss_at = Some(Instant::now() - Duration::from_secs(1));
        state.last_fullscreen = Some(true);
        // Seed the leave timer into the past so the "cursor leaves, restart the
        // fade timer" branch does not reset dismiss_at to the future this tick
        // (the same gotcha the persistent-idle-fade test documents).
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));
        state.tick();
        assert!(
            !state.persistent_faded,
            "the idle fade must not engage on the collapse-to-hide path"
        );
        assert!(
            matches!(state.phase, Phase::Collapsing(_) | Phase::Hidden),
            "the temporary pill must collapse (or finish collapsing) rather than linger"
        );
    }

    fn brave_track(title: &str) -> TrackInfo {
        track_for("Brave", title, "Brave Artist")
    }

    fn ytm_track(title: &str) -> TrackInfo {
        track_for("youtube-music", title, "YTM Artist")
    }

    fn reject(source: &str) -> MediaEvent {
        MediaEvent::SessionRejected {
            source_app: source.into(),
            title: String::new(),
            artist: String::new(),
            state: PlaybackState::Paused,
            accepted: false,
        }
    }

    #[test]
    fn session_rejected_swaps_shown_content_to_the_latest_valid_track() {
        // Brave is on screen; YTM's track was shown earlier this run, is
        // cached, and YTM is still playing. Retiring Brave must swap the pill
        // to YTM's track in place, not leave stale Brave content behind.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.cache_track(&ytm_track("Last YTM Track"));
        state
            .source_state
            .insert("youtube-music".into(), PlaybackState::Playing);
        state.content = Some(MediaEvent::TrackChanged(brave_track("Brave Song")));
        state.current_source = Some("Brave".into());
        state.last_track = Some(brave_track("Brave Song"));
        state.phase = Phase::Shown;
        state.queue.lock().unwrap().push_back(Arc::new(reject("Brave")));
        state.receive_events();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t))
                    if t.source_app == "youtube-music" && t.title == "Last YTM Track"
            ),
            "the pill must swap to the most recent valid cached track"
        );
        assert_eq!(state.current_source.as_deref(), Some("youtube-music"));
        assert_eq!(
            state.last_track.as_ref().map(|t| t.source_app.as_str()),
            Some("youtube-music"),
            "the sample pill must not render the retired source"
        );
        assert!(!matches!(state.phase, Phase::Hidden));
    }

    #[test]
    fn session_rejected_hides_the_pill_when_nothing_valid_remains() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.content = Some(MediaEvent::TrackChanged(brave_track("Brave Song")));
        state.current_source = Some("Brave".into());
        state.last_track = Some(brave_track("Brave Song"));
        state.phase = Phase::Shown;
        state.queue.lock().unwrap().push_back(Arc::new(reject("Brave")));
        state.receive_events();

        assert!(state.content.is_none());
        assert!(state.last_track.is_none());
        assert!(matches!(state.phase, Phase::Hidden));
    }

    #[test]
    fn session_rejected_swaps_held_content_while_auto_hidden() {
        // PersistentCompact auto-hide: the resume hold is Brave's content.
        // Retiring Brave must swap the hold so the resume re-shows YTM, not
        // the retired source.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.config.overlay.layout = LayoutMode::PersistentCompact;
        state.phase = Phase::Hidden;
        state.held_content = Some(MediaEvent::TrackChanged(brave_track("Brave Song")));
        state.last_track = Some(brave_track("Brave Song"));
        state.cache_track(&ytm_track("Last YTM Track"));
        state
            .source_state
            .insert("youtube-music".into(), PlaybackState::Playing);
        state.queue.lock().unwrap().push_back(Arc::new(reject("Brave")));
        state.receive_events();

        assert!(
            matches!(
                state.held_content.as_ref(),
                Some(MediaEvent::TrackChanged(t))
                    if t.source_app == "youtube-music" && t.title == "Last YTM Track"
            ),
            "the resume hold must swap to the latest valid track"
        );
        assert_eq!(
            state.last_track.as_ref().map(|t| t.source_app.as_str()),
            Some("youtube-music")
        );
        assert!(state.content.is_none());
        assert!(matches!(state.phase, Phase::Hidden));
    }

    #[test]
    fn session_rejected_drops_queued_notifications_from_the_retired_source() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state
            .pending
            .push_back(MediaEvent::TrackChanged(brave_track("Queued Brave")));
        state
            .pending
            .push_back(MediaEvent::TrackChanged(ytm_track("Queued YTM")));
        state.queue.lock().unwrap().push_back(Arc::new(reject("Brave")));
        state.receive_events();

        assert_eq!(state.pending.len(), 1, "only the queued Brave pill must drop");
        assert!(matches!(
            state.pending.front(),
            Some(MediaEvent::TrackChanged(t)) if t.source_app == "youtube-music"
        ));
    }

    #[test]
    fn session_rejected_swaps_last_track_for_the_sample_pill() {
        // Hidden with no content: the settings sample pill renders last_track.
        // Retiring the source behind it must point the sample at the most
        // recent valid track instead of the retired one.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.phase = Phase::Hidden;
        state.last_track = Some(brave_track("Brave Song"));
        state.cache_track(&ytm_track("Last YTM Track"));
        state
            .source_state
            .insert("youtube-music".into(), PlaybackState::Playing);
        state.queue.lock().unwrap().push_back(Arc::new(reject("Brave")));
        state.receive_events();

        assert_eq!(
            state.last_track.as_ref().map(|t| t.source_app.as_str()),
            Some("youtube-music"),
            "the sample pill must never render a retired source"
        );
        assert!(state.content.is_none());
    }

    #[test]
    fn session_rejected_swaps_content_even_while_notifications_disabled() {
        // The user's reported case: notifications were off, so nothing ever
        // updated the held Brave content. The retirement is content hygiene,
        // not a notification — it must run while the toggle is off so the
        // next show (or sample pill) never resurrects the retired source.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.enabled = false;
        state.cache_track(&ytm_track("Last YTM Track"));
        state
            .source_state
            .insert("youtube-music".into(), PlaybackState::Playing);
        state.content = Some(MediaEvent::TrackChanged(brave_track("Brave Song")));
        state.last_track = Some(brave_track("Brave Song"));
        state.phase = Phase::Shown;
        state.queue.lock().unwrap().push_back(Arc::new(reject("Brave")));
        state.receive_events();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "youtube-music"
            ),
            "the swap must apply even while notifications are disabled"
        );
        assert_eq!(
            state.last_track.as_ref().map(|t| t.source_app.as_str()),
            Some("youtube-music")
        );
    }

    #[test]
    fn session_rejected_for_an_unshown_source_leaves_the_pill_alone() {
        // A source that owns nothing on screen, in the hold, or behind the
        // sample must not disturb the shown content.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.cache_track(&ytm_track("Last YTM Track"));
        state.content = Some(MediaEvent::TrackChanged(ytm_track("Last YTM Track")));
        state.current_source = Some("youtube-music".into());
        state.last_track = Some(ytm_track("Last YTM Track"));
        state.phase = Phase::Shown;
        state.queue.lock().unwrap().push_back(Arc::new(reject("Spotify")));
        state.receive_events();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "youtube-music"
            ),
            "an unrelated rejection must not touch the shown content"
        );
        assert!(matches!(state.phase, Phase::Shown));
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
    fn persistent_layout_swaps_cross_source_track_in_place() {
        // A persistent-compact pill never collapses to Hidden, so the pending
        // queue (drained only while hidden) would hold a cross-source track
        // forever — a source switch must swap the pill in place instead.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.config.overlay.layout = LayoutMode::PersistentCompact;
        state.content = Some(MediaEvent::TrackChanged(track_for("youtube-music", "Song A", "Artist")));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() + Duration::from_millis(3000));

        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "Brave", "Song B", "Artist",
            ))));
        state.receive_events();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "Brave" && t.title == "Song B"
            ),
            "the cross-source track must become the shown content"
        );
        assert!(
            state.pending.is_empty(),
            "nothing may queue behind a persistent pill that never collapses"
        );
    }

    #[test]
    fn persistent_layout_swaps_cross_source_state_in_place() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.config.overlay.layout = LayoutMode::PersistentCompact;
        state.content = Some(MediaEvent::TrackChanged(track_for("youtube-music", "Song A", "Artist")));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() + Duration::from_millis(3000));

        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Paused,
                "Brave".into(),
            )));
        state.receive_events();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::PlaybackStateChanged(PlaybackState::Paused, source))
                    if source == "Brave"
            ),
            "the cross-source state must become the shown content"
        );
        assert!(state.pending.is_empty());
    }

    #[test]
    fn persistent_layout_event_before_first_show_shows_directly() {
        // Hidden with nothing held (pre-first-show): the batch's first event
        // must show directly — queueing it would strand it, because the
        // persistent pill never collapses and show_next would never drain.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.config.overlay.layout = LayoutMode::PersistentCompact;
        state.phase = Phase::Hidden;

        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "Brave", "Song B", "Artist",
            ))));
        state.receive_events();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "Brave" && t.title == "Song B"
            ),
            "the first event of a persistent run must show, not queue"
        );
        assert!(state.pending.is_empty());
        assert!(!matches!(state.phase, Phase::Hidden));
    }

    #[test]
    fn non_persistent_layout_still_queues_cross_source_track() {
        // Notification layouts collapse and hide, so their queue drains; a
        // cross-source track must keep queueing there (regression guard).
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.content = Some(MediaEvent::TrackChanged(track_for("youtube-music", "Song A", "Artist")));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() + Duration::from_millis(3000));

        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "Brave", "Song B", "Artist",
            ))));
        state.receive_events();

        assert_eq!(
            state.pending.len(),
            1,
            "notification layouts keep queueing cross-source tracks"
        );
        assert!(matches!(
            state.content.as_ref(),
            Some(MediaEvent::TrackChanged(t)) if t.source_app == "youtube-music"
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
    fn decide_layout_persistent_compact_always_uses_compact_geometry() {
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let verdict = ForegroundVerdict {
            exe: None,
            fullscreen: false,
        };
        assert_eq!(decide_layout(&config, &verdict), LayoutMode::Compact);
        let fullscreen = ForegroundVerdict {
            exe: None,
            fullscreen: true,
        };
        assert_eq!(decide_layout(&config, &fullscreen), LayoutMode::Compact);
    }

    #[test]
    fn hide_for_auto_compact_hides_persistent_pill_for_fullscreen() {
        // When layout = "persistent-compact" and hide_for_auto_compact_sources
        // is on, switching to a fullscreen foreground must hide the pill.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: true,
        });
        state.content = Some(MediaEvent::TrackChanged(track_for("spotify", "Song", "Artist")));
        state.phase = Phase::Shown;

        state.on_foreground_change();

        assert!(
            matches!(state.phase, Phase::Hidden),
            "the persistent pill must hide when the foreground goes fullscreen"
        );
        assert!(
            state.content.is_none(),
            "content is cleared on hide, but held_content must preserve it"
        );
    }

    #[test]
    fn hide_for_auto_compact_resumes_persistent_pill_when_foreground_clears() {
        // After hiding, switching back to a non-fullscreen foreground must
        // restore the held content as a fresh pill.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        let track = track_for("spotify", "Song", "Artist");

        // Simulate a showing pill over a fullscreen foreground, then hide it.
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: true,
        });
        state.content = Some(MediaEvent::TrackChanged(track.clone()));
        state.phase = Phase::Shown;
        state.on_foreground_change();
        assert!(matches!(state.phase, Phase::Hidden), "pill must hide for fullscreen");

        // Frontend clears: resume the held content.
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.on_foreground_change();

        assert!(
            !matches!(state.phase, Phase::Hidden),
            "the persistent pill must resume when the foreground clears"
        );
        assert!(state.content.is_some(), "the resume path must restore the held content");
        assert!(state.held_content.is_none(), "held_content must be consumed on resume");
    }

    #[test]
    fn same_window_fullscreen_toggle_hides_persistent_pill_on_tick() {
        // A same-window fullscreen toggle (F11 in a browser, Alt+Enter in a
        // game) fires no EVENT_SYSTEM_FOREGROUND — the foreground HWND never
        // changes. The static tick re-check must detect the verdict flip and
        // auto-hide the pill through on_foreground_change.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.last_fullscreen = Some(false);
        let track = track_for("spotify", "Song", "Artist");
        state.content = Some(MediaEvent::TrackChanged(track));
        state.phase = Phase::Shown;
        // Keep the dismiss countdown in the future so the pill cannot fade or
        // collapse independently of the fullscreen transition.
        state.dismiss_at = Some(Instant::now() + Duration::from_secs(3600));

        // The window goes fullscreen without changing identity.
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: true,
        });
        state.tick();

        assert!(
            matches!(state.phase, Phase::Hidden),
            "the pill must auto-hide on the tick"
        );
        assert!(state.content.is_none(), "content is cleared on hide");
        assert!(state.held_content.is_some(), "the content must be held for resume");
        assert!(
            state.hidden_watchdog,
            "the hidden-hold state must arm the foreground watchdog"
        );
        assert_eq!(
            state.tick_period, 1000,
            "the watchdog must keep its coarse 1 s cadence — the trailing \
             sync_anim_timer must not recreate the timer at refresh rate"
        );
    }

    #[test]
    fn hidden_watchdog_resumes_persistent_pill_on_same_window_fullscreen_exit() {
        // Once auto-hidden, only a foreground change used to resume the pill.
        // A same-window fullscreen-exit fires no event either, so the 1 Hz
        // hidden watchdog must detect the verdict flip and resume the held
        // content through on_foreground_change.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: true,
        });
        let track = track_for("spotify", "Song", "Artist");
        state.content = Some(MediaEvent::TrackChanged(track));
        state.phase = Phase::Shown;
        state.on_foreground_change();
        assert!(
            matches!(state.phase, Phase::Hidden),
            "pill must auto-hide for fullscreen"
        );
        assert!(state.hidden_watchdog, "auto-hide must arm the watchdog");

        // The same window exits fullscreen; no foreground change occurs.
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.tick();

        assert!(
            !matches!(state.phase, Phase::Hidden),
            "the watchdog must resume the held pill"
        );
        assert!(state.content.is_some(), "the resumed pill must restore the content");
        assert!(state.held_content.is_none(), "held_content must be consumed on resume");
        assert!(
            !state.hidden_watchdog,
            "the resumed pill must not keep the watchdog armed"
        );
    }

    #[test]
    fn hidden_watchdog_ignores_unchanged_verdict() {
        // The watchdog must not disturb the hidden-hold state while the
        // foreground stays fullscreen (a verdict flip is the only trigger).
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: true,
        });
        let track = track_for("spotify", "Song", "Artist");
        state.content = Some(MediaEvent::TrackChanged(track));
        state.phase = Phase::Shown;
        state.on_foreground_change();
        assert!(
            matches!(state.phase, Phase::Hidden),
            "pill must auto-hide for fullscreen"
        );

        state.tick();
        state.tick();

        assert!(
            matches!(state.phase, Phase::Hidden),
            "an unchanged verdict must keep the pill hidden"
        );
        assert!(
            state.hidden_watchdog,
            "the watchdog must stay armed while the hold is in place"
        );
    }

    #[test]
    fn hidden_watchdog_disarms_when_auto_hide_no_longer_applies() {
        // Layout/config can change while the pill is hidden (settings pane).
        // The watchdog must disarm itself instead of polling pointlessly; the
        // held content is left alone — a later, properly-armed hide resumes it.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: true,
        });
        let track = track_for("spotify", "Song", "Artist");
        state.content = Some(MediaEvent::TrackChanged(track));
        state.phase = Phase::Shown;
        state.on_foreground_change();
        assert!(state.hidden_watchdog, "auto-hide must arm the watchdog");

        state.config.overlay.layout = LayoutMode::Expanded;
        state.tick();

        assert!(
            !state.hidden_watchdog,
            "the watchdog must disarm when the auto-hide no longer applies"
        );
        assert!(matches!(state.phase, Phase::Hidden), "the pill stays hidden");
    }

    #[test]
    fn hidden_watchdog_not_armed_while_notifications_disabled() {
        // Disabling notifications while the pill is auto-hidden must not arm
        // (or keep) a watchdog poll: it could never show anything, and a
        // verdict flip while disabled would consume the held content into a
        // no-op show.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: true,
        });
        let track = track_for("spotify", "Song", "Artist");
        state.content = Some(MediaEvent::TrackChanged(track));
        state.phase = Phase::Shown;
        state.on_foreground_change();
        assert!(state.hidden_watchdog, "auto-hide must arm the watchdog");

        state.toggle_enabled();

        assert!(
            !state.hidden_watchdog,
            "disabling notifications must disarm the watchdog"
        );
        assert!(matches!(state.phase, Phase::Hidden), "the pill stays hidden");
    }

    #[test]
    fn show_flags_persistent_pill_for_fullscreen_foreground() {
        // A new pill arriving while a fullscreen app is the foreground must
        // show (in compact mode) but be flagged for collapse on dismiss —
        // it should hide instead of lingering at idle opacity.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: true,
        });
        let track = track_for("spotify", "Song", "Artist");

        state.show(MediaEvent::TrackChanged(track), true);

        assert!(
            !matches!(state.phase, Phase::Hidden),
            "the pill must show (not suppress) for a fullscreen foreground"
        );
        assert!(
            state.persistent_collapse_on_dismiss,
            "the pill must be flagged to collapse (not fade to idle) on dismiss"
        );
        assert!(state.held_content.is_some(), "the content must be saved for resume");
    }

    #[test]
    fn re_enable_restores_the_last_track_as_a_fast_path() {
        // Disabling hides the pill (clearing `content` but not `last_track`);
        // re-enabling must surface the last shown track immediately, so a
        // MEDIA_EVENT_MSG drained ahead of TOGGLE_MSG cannot strand the pill
        // hidden. The worker's forced re-show (within ~2s) then refreshes it
        // in place via the same_media / same_source_shown dedup.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        let track = track_for("spotify", "Song", "Artist");
        state.last_track = Some(track);

        // Simulate toggle-off (notifications disabled, pill hidden).
        state.enabled = false;
        state.phase = Phase::Hidden;
        assert!(state.content.is_none());

        // Re-enable -> the else-branch restores last_track at once.
        state.toggle_enabled();

        assert!(state.enabled, "notifications must be re-enabled");
        assert!(
            !matches!(state.phase, Phase::Hidden),
            "the pill must show immediately on re-enable, not wait for the worker"
        );
        assert!(
            matches!(&state.content, Some(MediaEvent::TrackChanged(t)) if t.source_app == "spotify" && t.title == "Song"),
            "last_track must be restored as the shown content"
        );
        assert!(
            state.last_track.is_some(),
            "last_track is cloned (not consumed) so the worker can refresh it in place"
        );
    }

    /// Serializes the wndproc tests that create a real window: the overlay
    /// class registration is guarded by a process-wide OnceLock whose guard
    /// check and RegisterClassExW are not atomic, so two parallel tests would
    /// race the registration (the loser fails with "Class already exists").
    /// The lock restores the single-registration semantics production relies
    /// on; production itself never races because the window is created once,
    /// on the UI thread, at startup.
    static WNDPROC_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Creates a hidden overlay window wired to the production wndproc and
    /// class: the state box lives in the window's GWLP_USERDATA slot (exactly
    /// what WM_NCCREATE sets in production), so dispatching posted messages
    /// through it exercises the real message path. The baseline state is a
    /// persistent-compact overlay with `last_track` set, notifications
    /// disabled, and a test foreground verdict. Returns `(hwnd, state_ptr,
    /// queue)`; the caller pushes events into `queue`, pumps messages, and
    /// finishes with `destroy_wndproc_overlay`.
    fn spawn_wndproc_overlay() -> (HWND, *mut OverlayState, EventQueue) {
        let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }.expect("process module").into();
        register_window_class(instance, &wide("WinGlanceOverlayWindow")).expect("the overlay class registers");

        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
        let mut state = Box::new(OverlayState::new(config, queue.clone()));
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.last_track = Some(track_for("spotify", "Song", "Artist"));
        state.enabled = false;
        state.phase = Phase::Hidden;
        let state_ptr = Box::into_raw(state);

        let hwnd = unsafe {
            crate::winapi::create_window(
                WINDOW_EX_STYLE(0),
                PCWSTR(wide("WinGlanceOverlayWindow").as_ptr()),
                PCWSTR(wide("WinGlance test").as_ptr()),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                instance,
                None,
            )
        }
        .expect("the test overlay window must be created");
        set_window_state(hwnd, state_ptr);
        (hwnd, state_ptr, queue)
    }

    /// Posts `messages` to `hwnd` in order, followed by a WM_NULL sentinel,
    /// and pumps them through a GetMessageW loop (the same FIFO drain
    /// production's message_loop performs). The sentinel is retrieved only
    /// after everything posted ahead of it was dispatched, so its arrival
    /// marks the drain complete without ever blocking.
    fn pump_posted_messages(hwnd: HWND, messages: &[u32]) {
        for &message in messages {
            unsafe { crate::winapi::post_message(hwnd, message, WPARAM(0), LPARAM(0)) }.expect("the message must post");
        }
        unsafe { crate::winapi::post_message(hwnd, WM_NULL, WPARAM(0), LPARAM(0)) }.expect("the sentinel must post");
        let mut msg = MSG::default();
        loop {
            let result = unsafe { GetMessageW(&mut msg, Some(hwnd), 0, 0) };
            assert!(result.0 > 0, "GetMessageW must retrieve the posted messages");
            if msg.message == WM_NULL {
                break;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Destroys the window through the production teardown: WM_NCDESTROY
    /// clears the routing slot *before* releasing the state box (the same
    /// order the main window applies), so the e2e tests exercise the real
    /// teardown instead of bypassing it. The box must therefore not be
    /// freed here — the handler owns it once it resolves it from the slot
    /// the test installed; freeing it here too would double-free.
    ///
    /// The slot-vs-box *order* is not observable through this e2e path: the
    /// window is mid-destroy during WM_NCDESTROY, so nothing reads the slot
    /// between the two operations, and the name-cell null precedes the
    /// release in either order — a mutation reversing them passes every e2e
    /// test unchanged. The order is pinned where it is observable, at the
    /// box's own drop, by `release_window_state_clears_the_slot_before_dropping_the_box`
    /// in winutil.rs.
    fn destroy_wndproc_overlay(hwnd: HWND, _state_ptr: *mut OverlayState) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn toggle_via_the_wndproc_survives_a_medial_event_drained_while_disabled() {
        // End-to-end through the real message path, not the method call: a
        // MEDIA_EVENT_MSG queued while notifications are disabled drains
        // first (the event is only cached, nothing is shown), then TOGGLE_MSG
        // re-enables and the fast-path restore must still surface the pill.
        let _serialize = WNDPROC_TEST_LOCK.lock().unwrap();
        let (hwnd, state_ptr, queue) = spawn_wndproc_overlay();
        queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "spotify",
                "New Song",
                "New Artist",
            ))));
        pump_posted_messages(hwnd, &[MEDIA_EVENT_MSG, TOGGLE_MSG]);

        let state = unsafe { &mut *state_ptr };
        assert!(state.enabled, "notifications must be re-enabled");
        assert!(
            state.track_cache.get("spotify").is_some_and(|t| t.title == "New Song"),
            "the disabled drain must still cache the track for the restore"
        );
        assert!(
            !matches!(state.phase, Phase::Hidden),
            "the pill must show immediately, not wait for the worker's re-emit"
        );
        assert!(
            matches!(&state.content, Some(MediaEvent::TrackChanged(t)) if t.source_app == "spotify" && t.title == "Song"),
            "the drained MEDIA_EVENT_MSG must not strand the pill: the fast-path restore surfaces the last shown track"
        );
        destroy_wndproc_overlay(hwnd, state_ptr);
    }

    #[test]
    fn toggle_via_the_wndproc_reaches_the_newer_track_when_the_toggle_lands_first() {
        // The opposite FIFO outcome, pinned through the same real queue: with
        // TOGGLE_MSG posted first, the fast-path restore surfaces the last
        // track, and the MEDIA_EVENT_MSG that follows lands on the now-
        // enabled pill, swapping it in place to the newer track (same-source
        // update). Together the two tests pin both arrival orders the
        // production comments rely on.
        let _serialize = WNDPROC_TEST_LOCK.lock().unwrap();
        let (hwnd, state_ptr, queue) = spawn_wndproc_overlay();
        queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "spotify",
                "New Song",
                "New Artist",
            ))));
        pump_posted_messages(hwnd, &[TOGGLE_MSG, MEDIA_EVENT_MSG]);

        let state = unsafe { &mut *state_ptr };
        assert!(state.enabled, "notifications must be re-enabled");
        assert!(
            !matches!(state.phase, Phase::Hidden),
            "the pill must be showing after the toggle and the media event"
        );
        assert!(
            matches!(&state.content, Some(MediaEvent::TrackChanged(t)) if t.source_app == "spotify" && t.title == "New Song"),
            "the media event must update the enabled pill in place to the newer track"
        );
        assert!(
            state.track_cache.get("spotify").is_some_and(|t| t.title == "New Song"),
            "the enabled drain must cache the newer track too"
        );
        destroy_wndproc_overlay(hwnd, state_ptr);
    }

    #[test]
    fn teardown_via_the_wndproc_leaves_a_client_held_name_cell_empty() {
        // End-to-end through the real message path: a MEDIA_EVENT_MSG is
        // pumped into the enabled pill (the production wndproc writes the
        // shared name cell via resolve_pill_text), then DestroyWindow drives
        // WM_DESTROY/WM_NCDESTROY synchronously through the production
        // handler — the provider detach, the name-cell null, and the box
        // release. The client-held clone (a provider handed to UIA core)
        // must read empty after the real teardown, not the last track name.
        let _serialize = WNDPROC_TEST_LOCK.lock().unwrap();
        let (hwnd, state_ptr, queue) = spawn_wndproc_overlay();
        let client = Arc::new(Mutex::new(None));
        unsafe {
            (*state_ptr).pill_name = Some(client.clone());
            (*state_ptr).enabled = true;
        }
        queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "spotify",
                "Love Me Not",
                "Ravyn Lenae",
            ))));
        pump_posted_messages(hwnd, &[MEDIA_EVENT_MSG]);

        // The real path wrote the cell: the client's clone mirrors the pill.
        assert_eq!(
            *client.lock().unwrap(),
            Some("Love Me Not — Ravyn Lenae (spotify)".to_string())
        );

        // DestroyWindow sends WM_DESTROY then WM_NCDESTROY through the
        // production wndproc; the handler frees the state box, so state_ptr
        // must not be touched after this point.
        destroy_wndproc_overlay(hwnd, state_ptr);
        assert_eq!(
            *client.lock().unwrap(),
            None,
            "a client-held provider must read empty after the real teardown"
        );
    }

    #[test]
    fn re_enable_restores_the_pinned_source_when_playing() {
        // Notifications were disabled while an arbitrary source (Brave) was
        // the last shown track; the pinned source (Spotify) is still playing.
        // Re-enabling must restore the pinned track — the pill's resting
        // identity — instead of the last arbitrary one.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&track_for("spotify", "Pinned Song", "Pinned Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Playing);
        state.last_track = Some(brave_track("Brave Song"));

        state.enabled = false;
        state.phase = Phase::Hidden;
        assert!(state.content.is_none());

        state.toggle_enabled();

        assert!(state.enabled, "notifications must be re-enabled");
        assert!(
            matches!(
                &state.content,
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "spotify" && t.title == "Pinned Song"
            ),
            "re-enable must restore the pinned source's track, not the last arbitrary one"
        );
    }

    #[test]
    fn re_enable_prefers_the_pinned_track_over_held_content() {
        // The same preference holds when the fast-path standby is the resume
        // hold rather than `last_track`: a playing pin wins over the held
        // content saved before the pill hid.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&track_for("spotify", "Pinned Song", "Pinned Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Playing);
        state.held_content = Some(MediaEvent::TrackChanged(brave_track("Brave Song")));

        state.enabled = false;
        state.phase = Phase::Hidden;

        state.toggle_enabled();

        assert!(
            matches!(
                &state.content,
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "spotify" && t.title == "Pinned Song"
            ),
            "a playing pin must win over the held content on re-enable"
        );
    }

    #[test]
    fn re_enable_does_not_restore_a_paused_pinned_source() {
        // The pinned source paused while notifications were disabled: the
        // "swap only to sources still playing" gate must keep the restore on
        // the last shown track instead of resurrecting the paused pin.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&track_for("spotify", "Pinned Song", "Pinned Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Paused);
        state.last_track = Some(brave_track("Brave Song"));

        state.enabled = false;
        state.phase = Phase::Hidden;

        state.toggle_enabled();

        assert!(
            matches!(
                &state.content,
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "Brave" && t.title == "Brave Song"
            ),
            "a paused pin must not be restored; the last shown track wins"
        );
    }

    #[test]
    fn track_cache_stays_fresh_while_notifications_are_disabled() {
        // Disabling must not freeze the track cache: a song change on the
        // pinned source while notifications are off is still cached, so the
        // re-enable fast path restores the exact current track rather than
        // the pre-disable one (the worker's ~2s re-emit would eventually
        // correct it, but the fast path should not need to wait on it).
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
        let mut state = OverlayState::new(config, queue.clone());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        // Cache the pre-disable track; the pin is still playing.
        state.cache_track(&track_for("spotify", "Old Song", "Old Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Playing);
        state.enabled = false;
        state.phase = Phase::Hidden;
        assert!(state.content.is_none());

        // The pinned source changes songs while notifications are disabled.
        queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "spotify",
                "New Song",
                "New Artist",
            ))));
        state.receive_events();

        assert_eq!(
            state.pending.len(),
            0,
            "no pill may be queued while notifications are disabled"
        );
        assert!(
            state.content.is_none(),
            "no pill may be shown while notifications are disabled"
        );
        assert!(
            matches!(state.track_cache.get("spotify"), Some(t) if t.title == "New Song"),
            "the track cache must be refreshed while notifications are disabled"
        );

        state.toggle_enabled();

        assert!(
            matches!(
                &state.content,
                Some(MediaEvent::TrackChanged(t)) if t.title == "New Song"
            ),
            "re-enable must restore the exact current pinned track, not the pre-disable one"
        );
    }

    #[test]
    fn re_enable_without_a_pin_restores_the_most_recent_playing_cached_track() {
        // No pin: the re-enable fast path must surface the most recent cached
        // track that is *actually playing* — the cache is kept fresh while
        // notifications are disabled — instead of the pre-disable last-shown
        // track. The most recent playing source wins over an older playing
        // one and over the stale last-shown track.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        // Older playing source first, then the newer one (recency order).
        state.cache_track(&track_for("brave", "Brave Song", "Brave Artist"));
        state.source_state.insert("brave".into(), PlaybackState::Playing);
        state.cache_track(&track_for("spotify", "New Song", "New Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Playing);
        // The pre-disable last-shown track is a third, unrelated source.
        state.last_track = Some(track_for("vlc", "VLC Song", "VLC Artist"));

        state.enabled = false;
        state.phase = Phase::Hidden;
        state.toggle_enabled();

        assert!(
            matches!(
                &state.content,
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "spotify" && t.title == "New Song"
            ),
            "the most recent playing cached track must win over the older one and the last-shown track"
        );
    }

    #[test]
    fn re_enable_without_a_pin_falls_back_to_the_last_track_when_nothing_cached_is_playing() {
        // The "swap only to sources still playing" gate applies to the
        // no-pin search too: a cached source that is paused or stopped is not
        // restored, so the pre-disable last-shown track remains the fallback.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&track_for("spotify", "Paused Song", "Paused Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Paused);
        state.last_track = Some(track_for("brave", "Brave Song", "Brave Artist"));

        state.enabled = false;
        state.phase = Phase::Hidden;
        state.toggle_enabled();

        assert!(
            matches!(
                &state.content,
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "brave" && t.title == "Brave Song"
            ),
            "no playing cached source: the last shown track wins"
        );
    }

    #[test]
    fn re_enable_without_a_pin_prefers_a_playing_cached_track_over_held_content() {
        // The same preference applies when the fast-path standby is the
        // resume hold: a playing cached track is live state, the held content
        // is a pre-disable snapshot, so the live track wins.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&track_for("spotify", "New Song", "New Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Playing);
        state.held_content = Some(MediaEvent::TrackChanged(track_for(
            "brave",
            "Brave Song",
            "Brave Artist",
        )));

        state.enabled = false;
        state.phase = Phase::Hidden;
        state.toggle_enabled();

        assert!(
            matches!(
                &state.content,
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "spotify" && t.title == "New Song"
            ),
            "a playing cached track must win over the held pre-disable content"
        );
    }

    #[test]
    fn re_enable_with_a_paused_pin_prefers_a_playing_non_pin_source() {
        // The pinned source paused while notifications were disabled: the pin
        // itself is not restored (the "swap only to sources still playing"
        // gate), but the playing-cache search is not gated on "no pin" — a
        // live non-pin source with a cached track wins over the stale
        // last-shown track. The playing filter guarantees the paused pin's
        // own track cannot be the search result.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        // The paused pin is the older cache entry; a live non-pin source is
        // the newer one (recency order).
        state.cache_track(&track_for("spotify", "Pinned Song", "Pinned Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Paused);
        state.cache_track(&track_for("brave", "Brave Song", "Brave Artist"));
        state.source_state.insert("brave".into(), PlaybackState::Playing);
        state.last_track = Some(track_for("vlc", "VLC Song", "VLC Artist"));

        state.enabled = false;
        state.phase = Phase::Hidden;
        state.toggle_enabled();

        assert!(
            matches!(
                &state.content,
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "brave" && t.title == "Brave Song"
            ),
            "a paused pin must not be restored; the most recent live non-pin source wins over the stale last-shown track"
        );
    }

    #[test]
    fn source_gone_clears_the_fast_path_standby_while_disabled() {
        // Post-settle re-enable regression: a source whose sessions settled
        // while notifications were off left track/state pills and a playback
        // tombstone queued or held; SourceGone must clean every restoreable
        // site even though the overlay is disabled, so the fast-path on a
        // later re-enable has nothing stale to surface.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.enabled = false;
        state.phase = Phase::Hidden;
        let track = track_for("spotify", "Song", "Artist");
        state.content = Some(MediaEvent::TrackChanged(track.clone()));
        // The resume hold and the fast-path restore source, plus a queued
        // notification from the same source.
        state.held_content = Some(MediaEvent::TrackChanged(track.clone()));
        state.last_track = Some(track.clone());
        state.pending.push_back(MediaEvent::TrackChanged(track));

        state.queue.lock().unwrap().push_back(Arc::new(MediaEvent::SourceGone {
            source_app: "spotify".into(),
        }));
        state.receive_events();

        assert!(
            state.pending.is_empty(),
            "queued events of the gone source must be dropped"
        );
        assert!(state.held_content.is_none(), "the resume hold must be cleared");
        assert!(
            state.last_track.is_none(),
            "the fast-path restore source must be cleared"
        );
        assert!(state.content.is_none(), "the stale track pill must be hidden");

        // Re-enable: the fast-path finds nothing to restore, so the pill
        // stays hidden instead of resurrecting the settled source's track.
        state.toggle_enabled();
        assert!(state.enabled);
        assert!(state.content.is_none(), "no stale track may be restored on re-enable");
        assert!(matches!(state.phase, Phase::Hidden), "the pill must stay hidden");
    }

    #[test]
    fn source_gone_retires_a_shown_stale_track_but_keeps_the_tombstone() {
        // A TrackChanged pillow for the gone source is stale "now playing"
        // content: SourceGone must retire it. The Stopped tombstone is the
        // retirement itself, so it stays on screen to finish its dismissal
        // UX — only the restoreable standby dies around it.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.phase = Phase::Shown;
        let track = track_for("spotify", "Song", "Artist");
        state.content = Some(MediaEvent::TrackChanged(track.clone()));
        state.last_track = Some(track);
        state.queue.lock().unwrap().push_back(Arc::new(MediaEvent::SourceGone {
            source_app: "spotify".into(),
        }));
        state.receive_events();
        assert!(state.content.is_none(), "the stale track pill must be dismissed");
        assert!(state.last_track.is_none(), "the standby must die with its source");
        assert!(matches!(state.phase, Phase::Hidden), "nothing valid remains to show");

        // Same source, but the shown content is the Stopped tombstone: it is
        // left in place, while the standby is still cleaned.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.phase = Phase::Shown;
        state.content = Some(MediaEvent::PlaybackStateChanged(
            PlaybackState::Stopped,
            "spotify".into(),
        ));
        state.last_track = Some(track_for("spotify", "Song", "Artist"));
        state.queue.lock().unwrap().push_back(Arc::new(MediaEvent::SourceGone {
            source_app: "spotify".into(),
        }));
        state.receive_events();
        assert!(
            matches!(&state.content, Some(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, s)) if s == "spotify"),
            "the Stopped tombstone must survive SourceGone"
        );
        assert!(
            state.last_track.is_none(),
            "the standby must die even under a tombstone"
        );
    }

    #[test]
    fn source_gone_swaps_the_standby_to_the_most_recent_valid_source() {
        // A pill (and its standby sites) showing the gone source swaps to the
        // newest cached track from a source that is still playing — same
        // succession rule as retire_source.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.phase = Phase::Shown;
        let gone = track_for("spotify", "Song", "Artist");
        let survivor = track_for("brave", "Survivor", "Artist");
        state.cache_track(&gone);
        state.cache_track(&survivor);
        state.source_state.insert("brave".into(), PlaybackState::Playing);
        state.content = Some(MediaEvent::TrackChanged(gone.clone()));
        state.held_content = Some(MediaEvent::TrackChanged(gone.clone()));
        state.last_track = Some(gone);

        state.queue.lock().unwrap().push_back(Arc::new(MediaEvent::SourceGone {
            source_app: "spotify".into(),
        }));
        state.receive_events();

        assert!(
            matches!(&state.content, Some(MediaEvent::TrackChanged(t)) if t.source_app == "brave" && t.title == "Survivor"),
            "the pill must swap to the most recent valid source"
        );
        assert!(
            matches!(&state.last_track, Some(t) if t.source_app == "brave"),
            "the fast-path restore source must swap to the survivor"
        );
        assert!(
            matches!(&state.held_content, Some(MediaEvent::TrackChanged(t)) if t.source_app == "brave"),
            "the resume hold must swap to the survivor"
        );
    }

    #[test]
    fn source_gone_hides_when_the_only_survivor_is_not_playing() {
        // The successor rule only announces sources that are actually playing:
        // a paused (or stopped) survivor must not surface its cached track as
        // "now playing" when the shown source settles.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.phase = Phase::Shown;
        let gone = track_for("spotify", "Song", "Artist");
        let paused = track_for("brave", "Paused Survivor", "Artist");
        state.cache_track(&gone);
        state.cache_track(&paused);
        state.source_state.insert("brave".into(), PlaybackState::Paused);
        state.content = Some(MediaEvent::TrackChanged(gone.clone()));
        state.last_track = Some(gone);

        state.queue.lock().unwrap().push_back(Arc::new(MediaEvent::SourceGone {
            source_app: "spotify".into(),
        }));
        state.receive_events();

        assert!(
            state.content.is_none(),
            "a paused survivor must not be announced as now playing"
        );
        assert!(state.last_track.is_none());
        assert!(matches!(state.phase, Phase::Hidden));
    }

    #[test]
    fn source_gone_prefers_a_playing_source_over_a_newer_paused_one() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.phase = Phase::Shown;
        let gone = track_for("spotify", "Song", "Artist");
        let playing = track_for("yt", "Playing Track", "Artist");
        let paused = track_for("brave", "Paused Track", "Artist");
        state.cache_track(&gone);
        state.cache_track(&playing);
        state.cache_track(&paused); // newest entry, but paused
        state.source_state.insert("yt".into(), PlaybackState::Playing);
        state.source_state.insert("brave".into(), PlaybackState::Paused);
        state.content = Some(MediaEvent::TrackChanged(gone));
        state.last_track = Some(track_for("spotify", "Song", "Artist"));

        state.queue.lock().unwrap().push_back(Arc::new(MediaEvent::SourceGone {
            source_app: "spotify".into(),
        }));
        state.receive_events();

        assert!(
            matches!(&state.content, Some(MediaEvent::TrackChanged(t)) if t.source_app == "yt" && t.title == "Playing Track"),
            "the playing source must win over a newer paused one"
        );
    }

    #[test]
    fn settled_sources_are_marked_stopped_and_cannot_succeed_each_other() {
        // Two sources settle in one batch: after the first swap, the second
        // SourceGone must not re-announce the first source's track — each
        // settle marks its source Stopped in the ledger, converging to a hide.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.phase = Phase::Shown;
        let spotify = track_for("spotify", "Song", "Artist");
        let brave = track_for("brave", "Song", "Artist");
        state.cache_track(&spotify);
        state.cache_track(&brave);
        state.source_state.insert("spotify".into(), PlaybackState::Playing);
        state.source_state.insert("brave".into(), PlaybackState::Playing);
        state.content = Some(MediaEvent::TrackChanged(spotify));
        state.last_track = Some(track_for("spotify", "Song", "Artist"));

        state.queue.lock().unwrap().push_back(Arc::new(MediaEvent::SourceGone {
            source_app: "spotify".into(),
        }));
        state.queue.lock().unwrap().push_back(Arc::new(MediaEvent::SourceGone {
            source_app: "brave".into(),
        }));
        state.receive_events();

        assert!(
            state.content.is_none(),
            "with both sources settled there is nothing playing to announce"
        );
        assert!(matches!(state.phase, Phase::Hidden));
    }

    #[test]
    fn source_state_tracks_playback_from_events() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Paused,
                "yt".into(),
            )));
        let mut playing = track_for("spotify", "Song", "Artist");
        playing.playback_state = Some(PlaybackState::Playing);
        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(playing)));
        state.receive_events();
        assert_eq!(state.source_state.get("yt"), Some(&PlaybackState::Paused));
        assert_eq!(state.source_state.get("spotify"), Some(&PlaybackState::Playing));

        // A TrackChanged whose snapshot carries no state (transitional read)
        // must not downgrade a known state.
        let mut transitional = track_for("spotify", "Song", "Artist");
        transitional.playback_state = None;
        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(transitional)));
        state.receive_events();
        assert_eq!(
            state.source_state.get("spotify"),
            Some(&PlaybackState::Playing),
            "a transitional snapshot must not erase a playing state"
        );
    }

    #[test]
    fn source_state_evicts_a_stopped_entry_at_the_ledger_cap() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        for i in 0..(LEDGER_STATE_CAP - 1) {
            state.remember_source_state(&format!("live-{i}"), PlaybackState::Playing);
        }
        state.remember_source_state("stopped-old", PlaybackState::Stopped);
        // Overflow evicts the inert Stopped entry; live sources survive.
        state.remember_source_state("live-last", PlaybackState::Playing);
        assert_eq!(state.source_state.len(), LEDGER_STATE_CAP);
        assert!(
            !state.source_state.contains_key("stopped-old"),
            "a Stopped entry must be evicted first"
        );
        assert!(state.source_state.contains_key("live-last"));
        assert!(state.source_state.contains_key("live-0"));
    }

    #[test]
    fn source_gone_of_an_unrelated_source_is_a_noop() {
        // Hygiene must be scoped: another source settling (its own stopped
        // tombstone may be next in the batch) must not touch this pill or its
        // standby.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.phase = Phase::Shown;
        let track = track_for("spotify", "Song", "Artist");
        state.content = Some(MediaEvent::TrackChanged(track.clone()));
        state.last_track = Some(track);

        state.queue.lock().unwrap().push_back(Arc::new(MediaEvent::SourceGone {
            source_app: "brave".into(),
        }));
        state.receive_events();

        assert!(
            matches!(&state.content, Some(MediaEvent::TrackChanged(t)) if t.source_app == "spotify"),
            "an unrelated SourceGone must not retire the shown pill"
        );
        assert!(
            state.last_track.is_some(),
            "an unrelated SourceGone must not clear the standby"
        );
    }

    #[test]
    fn persistent_idle_fade_renders_on_dismiss() {
        // Regression: the idle-fade ramp (persistent_faded) must trigger a
        // render on the static tick — otherwise the pill stays painted at full
        // opacity and the 0.25 alpha is never applied (paused media, short
        // titles where bar/marquee don't drive a repaint).
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        // Stub the real foreground: the first tick would otherwise route
        // through on_foreground_change and, when the terminal's window is
        // fullscreen, hide the pill before the fade logic runs.
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        let track = track_for("spotify", "Song", "Artist");
        state.content = Some(MediaEvent::TrackChanged(track));
        state.phase = Phase::Shown;
        // dismiss_at just in the past, within the 300ms fade window.
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.persistent_faded = false;
        state.progress_playing = false; // paused: bar_moved won't drive a render
        // hover_leave_at must be in the past so the "cursor leaves restart
        // timer" branch doesn't reset dismiss_at to the future.
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        let before = state.render_count;
        state.tick();

        assert!(state.persistent_faded, "dismiss_at firing must set persistent_faded");
        assert!(
            state.render_count > before,
            "the idle-fade must be painted, not just flagged"
        );
    }

    #[test]
    fn fade_disabled_persistent_pill_stays_bright_while_playing() {
        // fade_persistent_pill = false + media playing: the dismiss deadline
        // is a no-op — the pill stays at full opacity in the Shown phase,
        // never fading and never collapsing.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.overlay.fade_persistent_pill = false;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        // Stub the real foreground: the first tick would otherwise route
        // through on_foreground_change and, when the terminal's window is
        // fullscreen, hide the pill before the fade logic runs.
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.content = Some(MediaEvent::TrackChanged(track_for("spotify", "Song", "Artist")));
        state.phase = Phase::Shown;
        // dismiss_at just in the past; hover_leave_at also in the past so the
        // cursor-leave branch doesn't push the deadline back to the future.
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(
            matches!(state.phase, Phase::Shown),
            "a playing persistent pill must not collapse with the fade off"
        );
        assert!(!state.persistent_faded, "the idle fade must never arm");
    }

    #[test]
    fn fade_disabled_persistent_pill_stays_bright_while_paused() {
        // fade_persistent_pill = false + paused: the dismiss deadline is a no-op
        // — the pill stays at full opacity in the Shown phase, not collapsing.
        // Paused means the source is still alive and the user may resume; only
        // a Stopped state (tombstone) should hide the pill.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.overlay.fade_persistent_pill = false;
        let mut state = OverlayState::new(config.clone(), EventQueue::default());
        state.test_cursor_over = Some(false);
        // Stub the real foreground: the first tick would otherwise route
        // through on_foreground_change and, when the terminal's window is
        // fullscreen, hide the pill before the dismiss logic runs.
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.content = Some(MediaEvent::PlaybackStateChanged(
            PlaybackState::Paused,
            "spotify".into(),
        ));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(
            matches!(state.phase, Phase::Shown),
            "a paused persistent pill must not collapse with the fade off"
        );
        assert!(!state.persistent_faded, "the idle fade must never arm");
    }

    #[test]
    fn stopped_persistent_pill_hides_at_deadline_even_with_fade_enabled() {
        // fade_persistent_pill = true + a Stopped-state pill: the deadline
        // must collapse the tombstone into a full hide — lingering at idle
        // opacity would keep a dead ⏹ pill on screen forever.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.overlay.fade_persistent_pill = true;
        let mut state = OverlayState::new(config.clone(), EventQueue::default());
        state.test_cursor_over = Some(false);
        // Stub the real foreground: the first tick would otherwise route
        // through on_foreground_change and, when the terminal's window is
        // fullscreen, hide the pill before the collapse logic runs.
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.content = Some(MediaEvent::PlaybackStateChanged(
            PlaybackState::Stopped,
            "youtube-music".into(),
        ));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();
        assert!(
            matches!(state.phase, Phase::Collapsing(_)),
            "a stopped persistent pill must collapse at its deadline even with the fade enabled"
        );

        // Run the collapse past its animation length: it must complete into a
        // hide, not back into a bright shown pill at idle opacity.
        state.phase = Phase::Collapsing(Instant::now() - collapse_duration(&state.config) - Duration::from_millis(1));
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.tick();
        assert!(
            matches!(state.phase, Phase::Hidden),
            "the completed collapse must hide the stopped pill"
        );
    }

    #[test]
    fn source_matches_pin_uses_the_media_sources_identity_rules() {
        // The pin matches with the same normalization as the media-sources
        // allow-list, bidirectionally: a picker-stored "Spotify.exe" matches
        // the session label "spotify" and vice versa. Empty sides never match.
        assert!(source_matches_pin("spotify", "Spotify"));
        assert!(source_matches_pin("Spotify.exe", "spotify"));
        assert!(source_matches_pin("spotify", "Spotify.exe"));
        assert!(source_matches_pin("youtube-music", "youtube music"));
        assert!(!source_matches_pin("brave", "Spotify"));
        assert!(!source_matches_pin("spotify", ""));
        assert!(!source_matches_pin("", "spotify"));
        assert!(!source_matches_pin("", ""));
    }

    #[test]
    fn persistent_pill_returns_to_the_pinned_source_at_the_dismiss_deadline() {
        // Preferred-source pinning: the pill shows Brave's track; at the
        // dismiss deadline it returns to the pinned source's (Spotify, still
        // playing) cached track instead of fading to idle on the non-pinned
        // content — the pill's resting state is its pinned source.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("Spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&track_for("spotify", "Pinned Song", "Pinned Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Playing);
        state.content = Some(MediaEvent::TrackChanged(track_for(
            "brave",
            "Other Song",
            "Other Artist",
        )));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "spotify" && t.title == "Pinned Song"
            ),
            "the pill must return to the pinned source's track at the deadline"
        );
        assert!(!state.persistent_faded, "the return must restart full opacity");
        assert!(
            state.dismiss_at.is_some_and(|d| d > Instant::now()),
            "the returned pill must run a fresh dismiss timer"
        );
    }

    #[test]
    fn pinned_return_prefers_the_newest_playing_match() {
        // A broad pin ("spotify") matches two cached sources — the app and a
        // helper — and only the app is playing. The return must pick the
        // most recently cached *playing* match deterministically (recency
        // order), not an arbitrary cache entry that would skip the return.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&track_for("spotifyhelper", "Helper Song", "Artist"));
        state.cache_track(&track_for("spotify", "Pinned Song", "Artist"));
        state.source_state.insert("spotifyhelper".into(), PlaybackState::Paused);
        state.source_state.insert("spotify".into(), PlaybackState::Playing);
        state.content = Some(MediaEvent::TrackChanged(track_for("brave", "Other", "Other")));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "spotify" && t.title == "Pinned Song"
            ),
            "the return must pick the newest cached source that is actually playing"
        );
    }

    #[test]
    fn persistent_pill_does_not_return_to_a_paused_pinned_source() {
        // The pinned source paused: the "swap only to sources still playing"
        // discipline must keep the pill on what is actually audible — the
        // normal idle fade applies instead of resurrecting the paused pin.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&track_for("spotify", "Pinned Song", "Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Paused);
        state.content = Some(MediaEvent::TrackChanged(track_for("brave", "Other", "Other")));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(
            state.persistent_faded,
            "the deadline must fade normally when the pinned source is paused"
        );
        assert!(
            matches!(state.content.as_ref(), Some(MediaEvent::TrackChanged(t)) if t.source_app == "brave"),
            "the non-playing pin must not displace the pill"
        );
    }

    #[test]
    fn persistent_pill_does_not_return_when_the_pinned_source_has_no_cached_track() {
        // The pin is configured but its source never emitted a track this
        // session: there is nothing to return to, so the normal fade applies.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.content = Some(MediaEvent::TrackChanged(track_for("brave", "Other", "Other")));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(state.persistent_faded, "no cached track: fade normally");
    }

    #[test]
    fn persistent_pill_fades_normally_without_a_pin() {
        // No pinned_source configured: the deadline behaves exactly as before
        // — fade to idle, no content swap.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&track_for("spotify", "Pinned Song", "Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Playing);
        state.content = Some(MediaEvent::TrackChanged(track_for("brave", "Other", "Other")));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(
            state.persistent_faded,
            "without a pin the deadline must fade, not swap content"
        );
        assert!(matches!(state.content.as_ref(), Some(MediaEvent::TrackChanged(t)) if t.source_app == "brave"));
    }

    #[test]
    fn pinned_return_yields_to_the_collapse_on_dismiss_path() {
        // hide_for_auto_compact_sources over a fullscreen/listed foreground
        // flags the pill for collapse-on-dismiss: the deliberate hide takes
        // precedence over returning to the pinned source.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("Spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&track_for("spotify", "Pinned Song", "Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Playing);
        state.content = Some(MediaEvent::TrackChanged(track_for("brave", "Other", "Other")));
        state.phase = Phase::Shown;
        state.persistent_collapse_on_dismiss = true;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(
            matches!(state.phase, Phase::Collapsing(_)),
            "collapse-on-dismiss must hide the pill, not return to the pin"
        );
        assert!(
            matches!(state.content.as_ref(), Some(MediaEvent::TrackChanged(t)) if t.source_app == "brave"),
            "the collapse path must not swap content"
        );
    }

    #[test]
    fn stopped_tombstone_from_another_source_returns_to_the_pinned_source() {
        // A Stopped-state pill (tombstone) from a non-pinned source: with the
        // pinned source still playing, the deadline returns to the pin instead
        // of collapsing into a full hide — the playing pin's track is what
        // should rest on screen.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&track_for("spotify", "Pinned Song", "Artist"));
        state.source_state.insert("spotify".into(), PlaybackState::Playing);
        state.content = Some(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "brave".into()));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "spotify"
            ),
            "the tombstone must yield to the pinned source's playing track"
        );
        assert!(
            matches!(state.phase, Phase::Shown),
            "the returned pill stays shown at full opacity"
        );
    }

    #[test]
    fn pinned_source_tombstone_settles_on_the_playing_successor() {
        // The pinned source's session closes while its tombstone is showing
        // and another source (Brave) is still playing: after the tombstone's
        // full duration the pill settles onto Brave's track instead of
        // hiding — the pin's retirement must not leave the persistent pill
        // dark while other media is audible (the "swap only to sources still
        // playing" discipline, `best_successor`).
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&brave_track("Brave Song"));
        state.source_state.insert("Brave".into(), PlaybackState::Playing);
        state.source_state.insert("spotify".into(), PlaybackState::Stopped);
        state.content = Some(MediaEvent::PlaybackStateChanged(
            PlaybackState::Stopped,
            "spotify".into(),
        ));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "Brave"
            ),
            "the pin's tombstone must settle onto the most recent playing source"
        );
        assert!(matches!(state.phase, Phase::Shown), "the successor track stays shown");
        assert!(!state.persistent_faded, "the swap must restart full opacity");
        assert!(
            state.dismiss_at.is_some_and(|d| d > Instant::now()),
            "the settled pill must run a fresh dismiss timer"
        );
    }

    #[test]
    fn pinned_source_tombstone_hides_when_nothing_else_is_playing() {
        // The pinned source stops and no other source is playing: there is
        // no truthful content to settle onto, so the tombstone collapses
        // into the full hide exactly as before.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.source_state.insert("spotify".into(), PlaybackState::Stopped);
        state.content = Some(MediaEvent::PlaybackStateChanged(
            PlaybackState::Stopped,
            "spotify".into(),
        ));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(
            matches!(state.phase, Phase::Collapsing(_)),
            "no playing successor: the pin's tombstone still collapses"
        );
    }

    #[test]
    fn non_pinned_tombstone_still_hides_with_a_playing_source_present() {
        // Scoped to the pinned source: a tombstone for a *non-pinned*
        // source (here Brave stops while its track shows, with YouTube Music
        // still playing) keeps the deliberate hide — the successor fallback
        // is the pin's retirement rule, not a change to general tombstone
        // semantics.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.behavior.pinned_source = Some("spotify".into());
        let mut state = OverlayState::new(config, EventQueue::default());
        state.test_cursor_over = Some(false);
        state.test_fg_verdict = Some(ForegroundVerdict {
            exe: None,
            fullscreen: false,
        });
        state.cache_track(&ytm_track("YTM Song"));
        state
            .source_state
            .insert("youtube-music".into(), PlaybackState::Playing);
        state.source_state.insert("brave".into(), PlaybackState::Stopped);
        state.content = Some(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, "brave".into()));
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));

        state.tick();

        assert!(
            matches!(state.phase, Phase::Collapsing(_)),
            "a non-pinned tombstone still hides at its deadline"
        );
    }

    #[test]
    fn cross_source_stopped_does_not_displace_a_persistent_pill() {
        // The persistent pill shows Brave's track; YouTube Music closes in
        // the background. Its terminal Stopped must not swap the pill to a
        // dead ⏹ state — the in-place swap is reserved for the shown source.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.content = Some(MediaEvent::TrackChanged(brave_track("Brave Song")));
        state.current_source = Some("Brave".into());
        state.phase = Phase::Shown;
        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Stopped,
                "youtube-music".into(),
            )));
        state.receive_events();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t)) if t.source_app == "Brave"
            ),
            "the shown pill must survive a cross-source Stopped"
        );
    }

    #[test]
    fn same_source_stopped_refreshes_a_persistent_pill_then_it_hides() {
        // The source behind the shown track quits: the Stopped must swap the
        // pill in place (⏹ over the cached track), then the new dismiss
        // deadline collapses it into a full hide instead of the idle fade.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::PersistentCompact;
        config.overlay.fade_persistent_pill = true;
        let mut state = OverlayState::new(config.clone(), EventQueue::default());
        state.cache_track(&ytm_track("Last YTM Track"));
        state.content = Some(MediaEvent::TrackChanged(ytm_track("Last YTM Track")));
        state.phase = Phase::Shown;
        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Stopped,
                "youtube-music".into(),
            )));
        state.receive_events();

        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::PlaybackStateChanged(PlaybackState::Stopped, s)) if s == "youtube-music"
            ),
            "a same-source Stopped must refresh the shown pill in place"
        );

        state.test_cursor_over = Some(false);
        state.phase = Phase::Shown;
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));
        state.tick();
        assert!(
            matches!(state.phase, Phase::Collapsing(_)),
            "the refreshed Stopped pill must collapse at its deadline"
        );

        state.phase = Phase::Collapsing(Instant::now() - collapse_duration(&state.config) - Duration::from_millis(1));
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(10));
        state.tick();
        assert!(
            matches!(state.phase, Phase::Hidden),
            "the completed collapse must hide the stopped pill"
        );
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
        assert!(auto_source_matches(
            &config_with_sources(&config, ["Plex"]),
            Some("Plex.exe")
        ));
        assert!(auto_source_matches(
            &config_with_sources(&config, ["firefox"]),
            Some("firefox.exe")
        ));
        assert!(auto_source_matches(
            &config_with_sources(&config, ["Roblox"]),
            Some("Roblox.exe")
        ));
        assert!(auto_source_matches(
            &config_with_sources(&config, ["code"]),
            Some("Code.exe")
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
            let bitmap =
                crate::winapi::create_dib_section(Some(hdc), &info, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            assert!(!bits.is_null());
            let old = select_object(hdc, bitmap);
            // Pre-fill the DIB with an opaque black background like the pill.
            std::ptr::write_bytes(bits.cast::<u8>(), 0, 200 * 40 * 4);
            for i in 0..(200 * 40) {
                (bits.cast::<u8>()).add(i * 4 + 3).write(255);
            }
            let font_name = wide("Segoe UI");
            let font = crate::winapi::create_font(
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
            let old_font = select_object(hdc, font);
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
            select_object(hdc, old_font);
            let _ = delete_object(font);
            select_object(hdc, old);
            let _ = delete_object(bitmap);
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
            1.0,
            None,
            RenderLayer::Full,
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
            1.0,
            None,
            RenderLayer::Full,
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
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
            RenderLayer::Full,
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
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
            RenderLayer::Full,
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
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
            RenderLayer::Full,
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
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
            RenderLayer::Full,
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
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
            RenderLayer::Full,
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
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
            RenderLayer::Full,
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
            1.0,
            None,
            RenderLayer::Full,
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
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
            RenderLayer::Full,
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
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
            RenderLayer::Full,
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
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
            RenderLayer::Full,
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
            1.0,
            Some(MarqueeCtx {
                scroll: &mut scroll,
                strip: &mut strip,
            }),
            RenderLayer::Full,
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
            1.0,
            false,
            None,
            // The buffer is the pill at its rest size: the body's bottom
            // edges coincide, so every row is fully unveiled.
            76,
            76,
        );
        let lit = pixels.chunks(4).filter(|p| p[3] > 0).count();
        assert!(lit > 500, "expected text + art pixels, got {lit}");
    }

    #[test]
    fn compact_content_is_centered_in_the_scaled_body() {
        // Regression for the DPI-centering bug: draw_compact_pill centered
        // its content against the LOGICAL compact height while every element
        // was DPI-scaled, so at 150 % the content sat ~15 px high in the
        // body, and the hover morph's shape-0 frame (centered in the scaled
        // body) jumped at the morph boundary. The art tile's top edge must
        // sit at (scaled_body_h - scaled_art)/2.
        let config = Config::default();
        let scale = 1.5;
        let (compact_w, compact_h) = compact_size(&config);
        let buf_w = (compact_w * scale).round() as usize;
        let buf_h = (compact_h * scale).round() as usize;
        let mut state = OverlayState::new(config.clone(), EventQueue::default());
        let art_side = (compact_metrics(&config).art * scale).round() as usize;
        // Solid opaque red artwork (RGBA), sized to the scaled tile.
        let mut art = vec![255u8; art_side * art_side * 4];
        for px in art.chunks_mut(4) {
            px[1] = 0;
            px[2] = 0;
        }
        state.decoded_art = Some(art);
        let content = MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        });
        let mut pixels = vec![0u8; buf_w * buf_h * 4];
        draw_compact_pill(&mut state, &mut pixels, &content, buf_w as i32, scale, 1.0);
        let padding = (config.appearance.padding * scale).round() as usize;
        let art_x = state.aura_inset as usize + padding;
        // The tile's topmost opaque row within its horizontal extent (skip
        // the corner-mask columns, which round the corners transparent).
        let art_y =
            (0..buf_h).find(|&y| (art_x + 2..art_x + art_side - 2).any(|x| pixels[(y * buf_w + x) * 4 + 3] > 0));
        let expected = (buf_h - art_side) / 2;
        assert_eq!(
            art_y,
            Some(expected),
            "the art tile must be centered in the scaled body"
        );
    }

    #[test]
    fn blend_frames_lerps_premultiplied_pixels() {
        // The cross-fade's frame composition: all four bytes (the alpha
        // channel included) lerp from the old frame to the new one.
        let to = vec![200u8, 100, 50, 255, 10, 20, 30, 128];
        let from = vec![0u8, 0, 0, 0, 100, 100, 100, 255];
        let mut at0 = to.clone();
        blend_frames(&mut at0, &from, 0.0);
        assert_eq!(at0, from, "weight 0 must show the old frame");
        let mut at1 = to.clone();
        blend_frames(&mut at1, &from, 1.0);
        assert_eq!(at1, to, "weight 1 must show the new frame unchanged");
        let mut half = to.clone();
        blend_frames(&mut half, &from, 0.5);
        assert_eq!(
            half,
            vec![100, 50, 25, 128, 55, 60, 65, 192],
            "weight 0.5 must blend both frames"
        );
    }

    #[test]
    fn update_content_cross_fades_in_place_while_shown() {
        // A track swap on a fully-shown pill snapshots the previous frame at
        // the last rendered size and dissolves into the new content; the
        // first blended frame still shows the old content, and the fade
        // clears once its duration expires (or when the pill is animating).
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.layout = LayoutMode::Expanded;
        state.phase = Phase::Shown;
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        }));
        state.render();
        let (w, h) = (state.last_frame_w, state.last_frame_h);
        assert!(w > 0 && h > 0, "the render must record its frame size");
        assert!(!state.frame_scratch.is_empty());
        state.update_content(
            MediaEvent::TrackChanged(TrackInfo {
                source_app: "youtube-music".into(),
                title: "Next Song".into(),
                ..TrackInfo::default()
            }),
            Duration::from_secs(5),
        );
        let fade = state.content_fade.as_ref().expect("a shown pill must fade the swap");
        assert_eq!(
            (fade.from_w, fade.from_h),
            (w, h),
            "the snapshot must match the rendered size"
        );
        assert_eq!(fade.from.len(), w * h * 4);
        // The fade clears once its duration expires.
        state.content_fade.as_mut().unwrap().start = Instant::now() - CONTENT_FADE_DURATION;
        state.render();
        assert!(state.content_fade.is_none(), "the expired fade must clear");
        // An animating pill (entrance) swaps instantly instead.
        state.phase = Phase::Expanding(Instant::now());
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        }));
        state.render();
        state.update_content(
            MediaEvent::TrackChanged(TrackInfo {
                source_app: "youtube-music".into(),
                title: "Next Song".into(),
                ..TrackInfo::default()
            }),
            Duration::from_secs(5),
        );
        assert!(state.content_fade.is_none(), "an animating pill must not fade the swap");
    }

    #[test]
    fn compact_icon_leaves_no_trace_at_the_morph_end() {
        // The morph model: the compact-mode icon (inline, after the
        // title, next to the playback symbol) must dissolve with the compact
        // content and be completely gone by the time the morph reaches the
        // expanded state — the expanded layout's icon lives only in the app
        // row. Renders the morph-end frame twice, with and without an app
        // icon, and asserts the buffers are pixel-identical: the icon
        // contributes zero pixels to the expanded frame. (The compact slot
        // region legitimately holds the expanded title's glyphs, so
        // equality — not emptiness — is the contract.)
        let config = Config::default();
        let mut state = OverlayState::new(config, EventQueue::default());
        // Synthetic 24x24 icon, solid blue as BGRA (the pixel convention the
        // icon pipeline consumes) — which reads as a pure-red square
        // (255, 0, 0) in the RGBA buffer, detected below by that signature:
        // zero green and blue at any alpha. No text or symbol color can
        // match it — the app-name text is accent-tinted (the default accent
        // is red-dominant but never pure red), so a red-dominance heuristic
        // would count its glyphs as icon pixels.
        let mut icon = vec![0u8; 24 * 24 * 4];
        for px in icon.chunks_mut(4) {
            px.copy_from_slice(&[0, 0, 255, 255]);
        }
        let track_with_icon = TrackInfo {
            title: "Everything, Everywhere".into(),
            artist: "John Muirhead".into(),
            source_app: "Spotify".into(),
            app_icon: Some(Arc::<[u8]>::from(icon)),
            ..TrackInfo::default()
        };
        let content_with_icon = MediaEvent::TrackChanged(track_with_icon);
        let track_without = TrackInfo {
            title: "Everything, Everywhere".into(),
            artist: "John Muirhead".into(),
            source_app: "Spotify".into(),
            ..TrackInfo::default()
        };
        let content_without = MediaEvent::TrackChanged(track_without);
        let (_, expanded_h) = content_size_of(&state.config, &content_with_icon, false);
        let height = expanded_h as i32;
        let buf_w = 400usize;
        let buf_h = 120usize;
        let mut pixels = vec![0u8; buf_w * buf_h * 4];
        let icon_lit = |pixels: &[u8], x0: usize, y0: usize, x1: usize, y1: usize| {
            (y0..y1)
                .flat_map(|y| (x0..x1).map(move |x| (y, x)))
                .filter(|&(y, x)| {
                    let p = &pixels[(y * buf_w + x) * 4..(y * buf_w + x) * 4 + 4];
                    p[3] > 0 && p[1] == 0 && p[0] == 0
                })
                .count()
        };
        // Morph start: the compact icon is present at the compact slot
        // (235, 17), 16 px, at the default config and scale 1.0.
        draw_text_pixels(
            &mut state,
            &mut pixels,
            &content_with_icon,
            buf_w as i32,
            1.0,
            false,
            Some(MorphProgress {
                width: 0.0,
                height: 0.0,
            }),
            height,
            height,
        );
        assert!(
            icon_lit(&pixels, 235, 17, 251, 33) > 0,
            "the compact icon must show at the morph start"
        );
        // Morph end: the icon must leave no trace — the expanded frame is
        // identical with and without it.
        let mut with_icon = vec![0u8; buf_w * buf_h * 4];
        draw_text_pixels(
            &mut state,
            &mut with_icon,
            &content_with_icon,
            buf_w as i32,
            1.0,
            false,
            Some(MorphProgress {
                width: 1.0,
                height: 1.0,
            }),
            height,
            height,
        );
        let mut without_icon = vec![0u8; buf_w * buf_h * 4];
        // Reset the cached pill text: the first render stored the with-icon
        // pill in `state.pill_text`, and the morph pass consumes that cache
        // before rebuilding — without the reset this render would silently
        // redraw the with-icon content and the comparison would be vacuous.
        state.pill_text = None;
        draw_text_pixels(
            &mut state,
            &mut without_icon,
            &content_without,
            buf_w as i32,
            1.0,
            false,
            Some(MorphProgress {
                width: 1.0,
                height: 1.0,
            }),
            height,
            height,
        );
        // The compact slot region (where the compact icon sits at the morph
        // start) must be pixel-identical with and without the icon. The
        // buffers differ legally elsewhere: the expanded layout draws its own
        // app-row icon and shifts the row-3 text right by its width.
        let region = |buf: &[u8], x0: usize, y0: usize, x1: usize, y1: usize| -> Vec<u8> {
            (y0..y1)
                .flat_map(|y| (x0..x1).map(move |x| (y * buf_w + x) * 4))
                .flat_map(|i| buf[i..i + 4].to_vec())
                .collect()
        };
        assert_eq!(
            region(&with_icon, 235, 17, 251, 34),
            region(&without_icon, 235, 17, 251, 34),
            "the compact slot must be byte-identical with and without the icon at the morph end"
        );
        // The expanded app-row slot is the icon's only legal home: present
        // with the icon, absent without.
        assert!(
            icon_lit(&with_icon, 75, 50, 92, 70) > 0,
            "the expanded app-row must show the icon"
        );
        assert_eq!(
            icon_lit(&without_icon, 75, 50, 92, 70),
            0,
            "the expanded app-row must not show the icon without one"
        );
        // Mid-fade: partially faded, still present.
        pixels.fill(0);
        state.pill_text = None;
        draw_text_pixels(
            &mut state,
            &mut pixels,
            &content_with_icon,
            buf_w as i32,
            1.0,
            false,
            Some(MorphProgress {
                width: 0.10,
                height: 0.10,
            }),
            height,
            height,
        );
        assert!(
            icon_lit(&pixels, 235, 17, 251, 33) > 0,
            "the compact icon must be mid-fade during the transition"
        );
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
            1.0,
            None,
            RenderLayer::Full,
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
    fn show_sample_clears_stale_hover_state() {
        // Regression: show_sample reset hover_dismiss_at/hover_leave_at but
        // not hover_expand — the one "new pill" entry point that didn't. A
        // sample shown right after a hover morph (the collapse leg can still
        // be in flight when the user opens Settings and hits "Show sample")
        // would inherit the real pill's expansion: mid-morph or fully
        // expanded with the cursor nowhere near it, and a bogus collapse
        // seeded from another hover's velocity.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::Compact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.hover_expand = Some(HoverExpand {
            start: Instant::now() - Duration::from_millis(50),
            direction: MorphDirection::Expand,
            from: 0.5,
            velocity: 2.0,
            done: false,
        });

        state.show_sample();

        assert!(
            state.hover_expand.is_none(),
            "the sample must not inherit an in-flight hover morph"
        );
        assert!(
            matches!(state.phase, Phase::Light(_)),
            "the sample must take the light-up phase"
        );
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
        // The pill is not held (no morph-origin expansion), so the tick
        // applies the cap — the cursor is irrelevant for a laid-out pill.
        state.test_cursor_over = Some(false);

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
        // The cap lives in `tick` (so a morph-origin hold can suppress it);
        // a laid-out pill is never held, so the very next tick caps it.
        state.tick();

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
        state.test_cursor_over = Some(false);

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
        // Same as the sibling test: the cap applies on the first tick and
        // must not extend the earlier deadline.
        state.tick();

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
            PlaybackType::Unknown,
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
            PlaybackType::Unknown,
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
    fn disabled_animation_completes_a_leg_on_its_first_frame() {
        // While system preferences disallow motion, every leg derived
        // from the base animation duration is zero-length, and a zero-length
        // leg is already complete — the first frame renders the settled
        // state, so states switch immediately instead of animating.
        let mut config = Config::default();
        config.overlay.animation_ms = 500;
        assert_eq!(animation_duration_with(&config, true), Duration::from_millis(500));
        assert_eq!(animation_duration_with(&config, false), Duration::ZERO);
        assert_eq!(morph::normalized_elapsed(&Instant::now(), Duration::ZERO), 1.0);
        // The springs evaluate a completed leg at the pinned endpoint (1.0),
        // so the completed first frame is the steady state.
        assert!((ENTRANCE_GROW.value_at(1.0, 0.0, 0.0) - 1.0).abs() < 1e-6);
        assert!((spring_expand(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn entrance_grow_spring_overshoots_then_settles() {
        // The entrance grow spring runs from exactly 0 (compact) to exactly
        // 1 (expanded)...
        assert!(ENTRANCE_GROW.value_at(0.0, 0.0, 0.0).abs() < 1e-6);
        assert!((ENTRANCE_GROW.value_at(1.0, 0.0, 0.0) - 1.0).abs() < 1e-6);
        // ...with a modest mid-curve overshoot: clearly bouncy (the iOS/ColorOS
        // live-pill settle), never a wobble.
        let peak = (0..=100)
            .map(|i| ENTRANCE_GROW.raw_value(i as f32 / 100.0, 0.0, 0.0))
            .fold(0.0_f32, f32::max);
        assert!(
            peak > 1.02 && peak < 1.1,
            "entrance grow overshoot out of range: {peak}"
        );
        // And no wild undershoot below the compact floor at any sample point.
        for i in 0..=100 {
            let v = ENTRANCE_GROW.raw_value(i as f32 / 100.0, 0.0, 0.0);
            assert!(v >= -1e-6, "entrance grow undershoots the floor: {v}");
        }
    }

    #[test]
    fn lag_progress_holds_then_catches_up_and_pins() {
        // The follower's local time is 0 (holding at its start state) for
        // the first `lag` of the leg, then compresses the leader's curve
        // into the remaining leg: strictly increasing, reaching exactly 1.0
        // at the leg end so the follower's own spring pin lands precisely.
        let lag = MORPH_LAG;
        assert_eq!(lag_progress(0.0, lag), 0.0);
        assert_eq!(lag_progress(lag, lag), 0.0);
        assert_eq!(lag_progress(1.0, lag), 1.0);
        let mut last = 0.0;
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let v = lag_progress(t, lag);
            assert!(
                v >= last - 1e-6,
                "lagged time must be non-decreasing at {t}: {v} < {last}"
            );
            last = v;
        }
        // Out-of-range inputs clamp; lag 0 degenerates to the identity.
        assert_eq!(lag_progress(2.0, lag), 1.0);
        assert_eq!(lag_progress(-1.0, lag), 0.0);
        assert_eq!(lag_progress(0.3, 0.0), 0.3);
    }

    #[test]
    fn playback_glyph_matches_the_snapshot_state_across_layouts() {
        // The pill's glyph is resolved from the TrackChanged snapshot's
        // own playback_state, not a hardcoded NowPlaying. All three layout
        // paths share `playback_state_for_track`, so a single assertion on the
        // resolved state covers compact, expanded, and expanded-text.
        let playing = TrackInfo {
            playback_state: Some(PlaybackState::Playing),
            ..TrackInfo::default()
        };
        let paused = TrackInfo {
            playback_state: Some(PlaybackState::Paused),
            ..TrackInfo::default()
        };
        let stopped = TrackInfo {
            playback_state: Some(PlaybackState::Stopped),
            ..TrackInfo::default()
        };
        let transitional = TrackInfo::default();
        assert_eq!(playback_state_for_track(&playing), PlaybackState::Playing);
        assert_eq!(playback_state_for_track(&paused), PlaybackState::Paused);
        assert_eq!(playback_state_for_track(&stopped), PlaybackState::Stopped);
        // A source that reported no terminal/transitional state falls back to
        // the default now-playing symbol — never to Playing, which would mask a
        // genuine pause that arrived without a TrackChanged in the batch.
        assert_eq!(playback_state_for_track(&transitional), PlaybackState::NowPlaying);
    }

    #[test]
    fn lagged_expand_trails_the_leader_and_pins_exactly() {
        // The height follower is the leader's curve delayed and compressed
        // into the rest of the leg: it holds at compact through the lag,
        // trails the width on the way up, reaches the same overshoot peak a
        // beat later, and pins at exactly expanded at the leg end — a plain
        // time shift would leave it a hair short, popping when the leg
        // completes.
        assert_eq!(lagged_expand(&EXPAND_SPRING, 0.0, MORPH_LAG), 0.0);
        assert_eq!(lagged_expand(&EXPAND_SPRING, MORPH_LAG, MORPH_LAG), 0.0);
        assert_eq!(lagged_expand(&EXPAND_SPRING, 1.0, MORPH_LAG), 1.0);
        let samples: Vec<(f32, f32, f32)> = (0..=400)
            .map(|i| {
                let t = i as f32 / 400.0;
                (
                    t,
                    EXPAND_SPRING.value_at(t, 0.0, 0.0),
                    lagged_expand(&EXPAND_SPRING, t, MORPH_LAG),
                )
            })
            .collect();
        // The leader's ascent ends at its overshoot peak; until then the
        // follower never gets ahead of it.
        let leader_peak_i = samples
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
            .unwrap();
        for (i, &(t, leader, follower)) in samples.iter().enumerate() {
            if i <= leader_peak_i {
                assert!(
                    follower <= leader + 1e-4,
                    "the follower must trail on the way up at t={t}: {follower} > {leader}"
                );
            }
        }
        // The follower's own peak comes after the leader's (the chase is
        // visible), and never exceeds it — it is the same curve, later.
        let follower_peak_i = samples
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.2.total_cmp(&b.2))
            .map(|(i, _)| i)
            .unwrap();
        assert!(follower_peak_i > leader_peak_i, "the height must peak after the width");
        assert!(
            samples[follower_peak_i].2 <= samples[leader_peak_i].1 + 1e-4,
            "the follower must never overshoot further than the leader"
        );
        // Never negative at any point of the leg.
        for &(_, _, follower) in &samples {
            assert!(follower >= 0.0, "the follower must never go negative");
        }
    }

    #[test]
    fn lagged_collapse_holds_then_continues_the_seed_velocity_and_pins() {
        // The height follower of a collapse holds at the reversed progress
        // during the lag, then continues the collapse at exactly the
        // leader's seed velocity (the seed is scaled by 1 − lag to convert
        // to compressed local time, and the leg time derivative then lands
        // back on the physical per-leg velocity), and pins at compact at
        // the leg end. Like the un-lagged `spring_collapse`, the seeded
        // case briefly continues the expand motion (no kink) before the
        // spring turns it around.
        let from = 0.6;
        let velocity = 2.5;
        let h = 1e-3;
        // Held flat during the lag.
        assert!((lagged_collapse(0.0, MORPH_LAG, from, velocity) - from).abs() < 1e-6);
        assert!((lagged_collapse(MORPH_LAG, MORPH_LAG, from, velocity) - from).abs() < 1e-6);
        // At the lag's end the follower starts at the leader's exact seed
        // velocity (physical, per-leader-leg units).
        let t0 = MORPH_LAG + h;
        let slope = (lagged_collapse(t0, MORPH_LAG, from, velocity) - from) / h;
        assert!(
            (slope - velocity).abs() < 1e-1,
            "the follower must start at the seeded velocity, got {slope}"
        );
        // Pins at compact at the leg end, and stays pinned past it.
        assert_eq!(lagged_collapse(1.0, MORPH_LAG, from, velocity), 0.0);
        assert_eq!(lagged_collapse(2.0, MORPH_LAG, from, velocity), 0.0);
    }

    #[test]
    fn lagged_collapse_release_trails_the_leader() {
        // The release case (leave after the expansion pinned, and the exit
        // shrink): from rest the follower holds at expanded during the lag,
        // then lingers above the width while both descend to compact — the
        // height visibly stays behind the collapsing width. Once the leader
        // enters the undershoot below compact, the delayed follower trails
        // deeper into the dip and recovers later, so the trailing assertion
        // applies only to the descent; both stay within the pinned trough
        // and pin at compact.
        let mut min_leader = f32::INFINITY;
        let mut min_follower = f32::INFINITY;
        let mut descending = true;
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let leader = spring_collapse(t, 1.0, 0.0);
            let follower = lagged_collapse(t, MORPH_LAG, 1.0, 0.0);
            if leader < 0.0 {
                descending = false;
            }
            if descending {
                assert!(
                    follower >= leader - 5e-3,
                    "the follower must trail (stay above) the leader at t={t}: {follower} < {leader}"
                );
            }
            min_leader = min_leader.min(leader);
            min_follower = min_follower.min(follower);
        }
        assert!(
            (min_leader - COLLAPSE_TROUGH).abs() < 1e-3,
            "the leader must dip to the pinned trough, got {min_leader}"
        );
        assert!(
            (min_follower - COLLAPSE_TROUGH).abs() < 1e-3,
            "the follower must dip to the same trough, got {min_follower}"
        );
        assert_eq!(lagged_collapse(1.0, MORPH_LAG, 1.0, 0.0), 0.0);
    }

    #[test]
    fn morph_radius_lerps_between_the_two_radii() {
        // The radius morphs continuously between the compact and expanded
        // radii on the leading (width) axis, clamped so the eased spring can
        // never render the shape over-rounded or pinched. The settle-bounce
        // scales the composed frame as a whole (see `scale_frame_about`), so
        // the corners ride it without this lerp overshooting.
        let (compact_r, expanded_r) = (8.0, 16.0);
        assert_eq!(
            morph_radius(
                compact_r,
                expanded_r,
                MorphProgress {
                    width: 0.0,
                    height: 0.0
                }
            ),
            compact_r
        );
        assert_eq!(
            morph_radius(
                compact_r,
                expanded_r,
                MorphProgress {
                    width: 1.0,
                    height: 1.0
                }
            ),
            expanded_r
        );
        let mid = morph_radius(
            compact_r,
            expanded_r,
            MorphProgress {
                width: 0.5,
                height: 0.5,
            },
        );
        assert!(
            (mid - 12.0).abs() < 1e-5,
            "mid-morph radius must be the midpoint, got {mid}"
        );
        // Overshoot and lagged-height states clamp to the width interval.
        let over = morph_radius(
            compact_r,
            expanded_r,
            MorphProgress {
                width: 1.3,
                height: 0.4,
            },
        );
        assert_eq!(over, expanded_r);
        let under = morph_radius(
            compact_r,
            expanded_r,
            MorphProgress {
                width: -0.2,
                height: 0.9,
            },
        );
        assert_eq!(under, compact_r);
    }

    #[test]
    fn hover_morph_radius_is_exactly_continuous_with_the_discrete_layout_radii() {
        // Regression pin for the hover-morph wiring (see `render`): the
        // hover leg's progress must reach the render, and the morphing
        // radius it drives must equal the discrete layout radii bit-for-bit
        // at the leg endpoints — otherwise the first/last hover frame snaps
        // the corners. Both paths run through the real production chain:
        // `hover_progress` -> `morph_radius` -> `effective_corner_radius`.
        let config = Config::default();
        let (compact_r, expanded_r) = (config.appearance.compact_corner_radius, config.appearance.corner_radius);
        assert_eq!(
            config.appearance.effective_corner_radius(true),
            compact_r,
            "precondition: the effective compact radius is the compact radius"
        );
        // The exit seam: a finished collapse leg pins both axes at exactly
        // 0.0 (the springs pin at the leg end), so the frame that hands off
        // to the steady compact render draws the compact radius exactly.
        let finished_collapse = HoverExpand {
            start: Instant::now() - morph_duration(&config, MorphDirection::Collapse),
            direction: MorphDirection::Collapse,
            from: 1.0,
            velocity: 0.0,
            done: true,
        };
        let progress = hover_progress(&finished_collapse, &config);
        assert_eq!(
            progress,
            MorphProgress {
                width: 0.0,
                height: 0.0
            },
            "a finished collapse must land exactly on compact"
        );
        assert_eq!(
            morph_radius(compact_r, expanded_r, progress),
            config.appearance.effective_corner_radius(true),
            "the 0 endpoint must equal the compact layout's discrete radius exactly"
        );
        // The entry seam, far end: a finished expand leg pins at exactly
        // 1.0, matching the expanded discrete radius exactly.
        let finished_expand = HoverExpand {
            start: Instant::now() - morph_duration(&config, MorphDirection::Expand),
            direction: MorphDirection::Expand,
            from: 0.0,
            velocity: 0.0,
            done: true,
        };
        let progress = hover_progress(&finished_expand, &config);
        assert_eq!(
            progress,
            MorphProgress {
                width: 1.0,
                height: 1.0
            },
            "a finished expand must land exactly on expanded"
        );
        assert_eq!(
            morph_radius(compact_r, expanded_r, progress),
            config.appearance.effective_corner_radius(false),
            "the 1 endpoint must equal the expanded layout's discrete radius exactly"
        );
        // The entry seam, near end: a leg that has just begun drifts from
        // exactly 0.0 only by the wall-clock dust between `Instant::now()`
        // and the progress evaluation (the curve itself starts at exactly
        // 0.0), so the radius stays within a sub-pixel of the compact
        // radius — versus the full 14 px snap to the expanded radius the
        // unwired render produced on the first hover frame.
        let just_started = HoverExpand {
            start: Instant::now(),
            direction: MorphDirection::Expand,
            from: 0.0,
            velocity: 0.0,
            done: false,
        };
        let progress = hover_progress(&just_started, &config);
        let radius = morph_radius(compact_r, expanded_r, progress);
        assert!(
            (radius - compact_r).abs() < 5e-3,
            "a fresh leg must render within sub-pixel of the compact radius, got {radius}"
        );
        assert!(
            (radius - expanded_r).abs() > 1.0,
            "a fresh leg must not snap toward the expanded radius, got {radius}"
        );
    }

    #[test]
    fn morph_exclusive_elements_never_coexist() {
        // The shared elements (title, playback symbol, art) never fade during
        // a morph — they travel (see `draw_morph_content`). Only the
        // layout-exclusive elements fade, both keyed to the shape progress —
        // the less-advanced axis, min(width, height). The fade windows
        // (compact: 0.05..0.20, expanded: 0.25..0.60) are deliberately
        // DISJOINT: the compact app icon must be completely gone before the
        // expanded extra rows start arriving, or the icon would visibly sit
        // beside the arriving app row. The assertion allows float dust at
        // the window boundary — at no point can both exclusive groups be
        // meaningfully visible.
        assert_eq!(compact_alpha(0.05), 1.0);
        assert_eq!(compact_alpha(0.20), 0.0);
        assert_eq!(expanded_alpha(0.25), 0.0);
        assert_eq!(expanded_alpha(0.60), 1.0);
        for i in 0..=400 {
            let t = i as f32 / 400.0;
            let width = EXPAND_SPRING.value_at(t, 0.0, 0.0);
            let height = lagged_expand(&EXPAND_SPRING, t, MORPH_LAG);
            let shape = width.min(height);
            let (compact, expanded) = (compact_alpha(shape), expanded_alpha(shape));
            assert!(
                compact <= 0.01 || expanded <= 0.01,
                "the exclusive elements must never overlap at t={t}: compact={compact} expanded={expanded}"
            );
        }
        // The release direction (collapse) keeps the same disjointness, with
        // the leading width as the limiting axis: the expanded rows are gone
        // before the compact icon starts fading back in.
        for i in 0..=400 {
            let t = i as f32 / 400.0;
            let width = spring_collapse(t, 1.0, 0.0);
            let height = lagged_collapse(t, MORPH_LAG, 1.0, 0.0);
            let shape = width.min(height);
            let (compact, expanded) = (compact_alpha(shape), expanded_alpha(shape));
            assert!(
                compact <= 0.01 || expanded <= 0.01,
                "the exclusive elements must never overlap on collapse at t={t}: compact={compact} expanded={expanded}"
            );
        }
    }

    #[test]
    fn morph_art_tile_lands_exactly_on_the_steady_tiles_at_both_ends() {
        // The interpolated tile must be pixel-identical to the compact tile
        // at shape 0 (rendered in the compact body) and the expanded tile at
        // shape 1 (in the expanded body) — the morph hands off without a
        // jump.
        let config = Config::default();
        let inset = 0;
        let scale = 1.0;
        let compact_h = (compact_size(&config).1 * scale).round() as i32;
        let expanded_h = (content_size(&config).1 * scale).round() as i32;
        let metrics = compact_metrics(&config);
        let appearance = &config.appearance;
        let padding = (appearance.padding * scale).round() as i32;
        let (x0, y0, s0) = morph_art_tile(&config, inset, compact_h, scale, 0.0);
        assert_eq!(s0, (metrics.art * scale).round() as i32, "shape-0 size");
        assert_eq!(y0, inset + (compact_h - s0) / 2, "shape-0 y");
        assert_eq!(x0, inset + padding, "shape-0 x");
        let (x1, y1, s1) = morph_art_tile(&config, inset, expanded_h, scale, 1.0);
        assert_eq!(s1, (appearance.art_size as f32 * scale).round() as i32, "shape-1 size");
        assert_eq!(y1, inset + (expanded_h - s1) / 2, "shape-1 y");
        assert_eq!(x1, inset + padding, "shape-1 x");
    }

    #[test]
    fn morph_title_band_lands_exactly_on_the_layout_bands_at_both_ends() {
        // Same contract as the art tile: the traveling title band is exactly
        // the compact band at shape 0 and the expanded title row at shape 1.
        let config = Config::default();
        let inset = 0;
        let scale = 1.0;
        let width = (content_size(&config).0 * scale).round() as i32;
        let appearance = &config.appearance;
        let padding = (appearance.padding * scale).round() as i32;
        let row_h = (appearance.font_size_title * ROW_HEIGHT * scale).round() as i32;
        let art = (appearance.art_size as f32 * scale).round() as i32;
        let symbol = (compact_metrics(&config).symbol * scale).round() as i32;
        let label_w = symbol + (16.0 * scale).round() as i32;
        let compact_h = (compact_size(&config).1 * scale).round() as i32;
        let (vp_left, vp_right) = compact_title_viewport(&config);
        let at0 = morph_title_band(
            &config,
            inset,
            width,
            scale,
            MorphProgress {
                width: 0.0,
                height: 0.0,
            },
        );
        assert_eq!(at0.left, inset + (vp_left * scale).round() as i32, "shape-0 left");
        assert_eq!(at0.right, inset + (vp_right * scale).round() as i32, "shape-0 right");
        assert_eq!(at0.top, inset + (compact_h - row_h) / 2, "shape-0 top");
        assert_eq!(at0.bottom, at0.top + row_h, "shape-0 bottom");
        let at1 = morph_title_band(
            &config,
            inset,
            width,
            scale,
            MorphProgress {
                width: 1.0,
                height: 1.0,
            },
        );
        assert_eq!(
            at1.left,
            inset + padding + art + (12.0 * scale).round() as i32,
            "shape-1 left"
        );
        assert_eq!(at1.top, inset + padding, "shape-1 top");
        assert_eq!(at1.right, width - inset - padding - label_w, "shape-1 right");
        assert_eq!(at1.bottom, at1.top + row_h, "shape-1 bottom");
    }

    #[test]
    fn morph_symbol_pos_lands_exactly_on_the_layout_positions_at_both_ends() {
        // The traveling symbol: the compact trailing chain at shape 0, the
        // expanded title row's right slot at shape 1, same size throughout.
        let config = Config::default();
        let inset = 0;
        let scale = 1.0;
        let width = (content_size(&config).0 * scale).round() as i32;
        let appearance = &config.appearance;
        let padding = (appearance.padding * scale).round() as i32;
        let symbol = (compact_metrics(&config).symbol * scale).round() as i32;
        let compact_h = (compact_size(&config).1 * scale).round() as i32;
        let (_, vp_right) = compact_title_viewport(&config);
        let viewport_right = inset + (vp_right * scale).round() as i32;
        let gap = (6.0 * scale).round() as i32;
        let icon = (16.0 * scale).round() as i32;
        let symbol_gap = (16.0 * scale).round() as i32;
        let at0 = morph_symbol_pos(
            &config,
            inset,
            width,
            scale,
            MorphProgress {
                width: 0.0,
                height: 0.0,
            },
        );
        assert_eq!(
            at0.0,
            viewport_right + gap + icon + symbol_gap + symbol,
            "shape-0 right"
        );
        assert_eq!(at0.1, inset + (compact_h - symbol) / 2, "shape-0 y");
        assert_eq!(at0.2, symbol as f32, "shape-0 size");
        let at1 = morph_symbol_pos(
            &config,
            inset,
            width,
            scale,
            MorphProgress {
                width: 1.0,
                height: 1.0,
            },
        );
        assert_eq!(at1.0, width - inset - padding, "shape-1 right");
        assert_eq!(at1.1, inset + padding, "shape-1 y");
        assert_eq!(at1.2, symbol as f32, "shape-1 size");
    }

    #[test]
    fn art_edge_gate_stays_full_while_the_tile_fits_and_fades_with_the_cut() {
        // The morph art must render at full opacity the moment it fits the
        // current body (no fade while the body finishes growing), and fade
        // only proportionally to the edge cutting through it.
        let art_y = 20;
        let art_size = 40;
        assert_eq!(art_edge_gate(art_y + art_size, art_y, art_size), 1.0, "fits exactly");
        assert_eq!(
            art_edge_gate(art_y + art_size + 50, art_y, art_size),
            1.0,
            "fits with room"
        );
        assert_eq!(art_edge_gate(art_y + art_size / 2, art_y, art_size), 0.5, "half cut");
        assert_eq!(art_edge_gate(art_y, art_y, art_size), 0.0, "edge at the tile top");
        assert_eq!(
            art_edge_gate(art_y - 10, art_y, art_size),
            0.0,
            "tile fully below the edge"
        );
    }

    #[test]
    fn morph_icon_pos_matches_the_compact_trailing_chain() {
        // The app icon stays at its compact position while it dissolves out.
        let config = Config::default();
        let inset = 0;
        let scale = 1.0;
        let (_, vp_right) = compact_title_viewport(&config);
        let viewport_right = inset + (vp_right * scale).round() as i32;
        let gap = (6.0 * scale).round() as i32;
        let icon = (16.0 * scale).round() as i32;
        let compact_h = (compact_size(&config).1 * scale).round() as i32;
        let (x, y, size) = morph_icon_pos(&config, inset, scale);
        assert_eq!(x, viewport_right + gap);
        assert_eq!(y, inset + (compact_h - icon) / 2);
        assert_eq!(size, icon);
    }

    #[test]
    fn morph_title_band_travels_monotonically_between_the_layouts() {
        // As the progress advances the title's edges move from the compact
        // band to the expanded band without reversing — the travel must read
        // as one continuous move, never a back-and-forth.
        let config = Config::default();
        let inset = 0;
        let scale = 1.0;
        let width = (content_size(&config).0 * scale).round() as i32;
        let mut last: Option<RECT> = None;
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let band = morph_title_band(&config, inset, width, scale, MorphProgress { width: t, height: t });
            if let Some(prev) = last {
                assert!(band.left >= prev.left, "left must not move left at t={t}");
                assert!(band.right >= prev.right, "right must not move left at t={t}");
                assert!(band.top <= prev.top, "top must not move down at t={t}");
                assert!(band.bottom <= prev.bottom, "bottom must not move down at t={t}");
            }
            last = Some(band);
        }
    }

    #[test]
    fn morph_art_stays_inside_the_body_across_the_leg() {
        // Sampled over the real expand leg, the interpolated tile must fit
        // inside the growing body, so the edge gate never clips it mid-morph
        // — the artwork renders at full opacity for the whole leg, with no
        // fade while the body finishes growing.
        let config = Config::default();
        let content = MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        });
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let width = EXPAND_SPRING.value_at(t, 0.0, 0.0);
            let height = lagged_expand(&EXPAND_SPRING, t, MORPH_LAG);
            let shape = width.min(height);
            let (_, compact_h) = content_size_of(&config, &content, true);
            let (_, expanded_h) = content_size_of(&config, &content, false);
            let pill_h = (compact_h + (expanded_h - compact_h) * height).round() as i32;
            let (_, y, size) = morph_art_tile(&config, 0, pill_h, 1.0, shape);
            assert!(size <= pill_h, "art must fit the body at t={t}: {size} > {pill_h}");
            assert!(y + size <= pill_h, "art must stay inside the body at t={t}");
            assert_eq!(
                art_edge_gate(pill_h, y, size),
                1.0,
                "the edge gate must not fade the art at t={t}"
            );
        }
    }

    #[test]
    fn bounce_scale_pins_at_one_and_hits_the_configured_amplitudes() {
        // The whole-pill settle-bounce, driven by the spring progress
        // itself: exactly 1.0 whenever the spring is inside its endpoints
        // (and at the pinned end), an expand that overshoots once to
        // 1 + BOUNCE_OVER at the spring's peak, and a compaction that dips
        // to 1 - BOUNCE_UNDER at the undershoot trough and recovers
        // straight to 1.0 at the pin — the shrink-below-minimum return.
        let at = |width: f32| MorphProgress { width, height: 0.0 };
        // Expand: no scale inside the endpoints, exactly 1 + OVER at the
        // spring's peak, clamped beyond it.
        assert_eq!(bounce_scale(at(0.0), MorphDirection::Expand), 1.0);
        assert_eq!(bounce_scale(at(0.5), MorphDirection::Expand), 1.0);
        assert_eq!(bounce_scale(at(1.0), MorphDirection::Expand), 1.0);
        assert!(
            (bounce_scale(at(EXPAND_SPRING_PEAK), MorphDirection::Expand) - (1.0 + BOUNCE_OVER)).abs() < 1e-5,
            "the expand must peak at 1 + BOUNCE_OVER, got {}",
            bounce_scale(at(EXPAND_SPRING_PEAK), MorphDirection::Expand)
        );
        assert_eq!(bounce_scale(at(1.3), MorphDirection::Expand), 1.0 + BOUNCE_OVER);
        // Compaction: exactly 1.0 at the zero crossings (the pill reaches
        // compact and the pin lands at compact), 1 - UNDER at the trough,
        // and a straight recovery (1 - UNDER/2) mid-way back — no over-bounce.
        assert_eq!(bounce_scale(at(0.6), MorphDirection::Collapse), 1.0);
        assert_eq!(bounce_scale(at(0.0), MorphDirection::Collapse), 1.0);
        assert!(
            (bounce_scale(at(COLLAPSE_TROUGH), MorphDirection::Collapse) - (1.0 - BOUNCE_UNDER)).abs() < 1e-5,
            "the compaction must dip to 1 - BOUNCE_UNDER, got {}",
            bounce_scale(at(COLLAPSE_TROUGH), MorphDirection::Collapse)
        );
        assert!(
            (bounce_scale(at(COLLAPSE_TROUGH / 2.0), MorphDirection::Collapse) - (1.0 - BOUNCE_UNDER / 2.0)).abs()
                < 1e-5,
            "the compaction must recover to 1 - BOUNCE_UNDER/2 mid-recovery, got {}",
            bounce_scale(at(COLLAPSE_TROUGH / 2.0), MorphDirection::Collapse)
        );
        // The scale is a pure function of the progress: a below-trough dip
        // clamps at the configured minimum rather than overshooting it.
        assert_eq!(
            bounce_scale(at(COLLAPSE_TROUGH * 2.0), MorphDirection::Collapse),
            1.0 - BOUNCE_UNDER
        );
    }

    #[test]
    fn scale_frame_about_is_a_uniform_scale_about_the_content_corner() {
        // A 4x4 src with one solid premultiplied pixel; scaling by 0.5 about
        // the content corner (inset 0) must map the src's center pixel (2, 2)
        // to the dst's center pixel (1, 1) — the shrink pulls everything toward
        // the top-left corner — with every other dst pixel transparent. The
        // function has no anchor parameter (the on-screen anchor is produced by
        // `placement()` repositioning the window), so the result must be
        // identical for any hypothetical anchor value.
        let src_w = 4usize;
        let src_h = 4usize;
        let mut src = vec![0u8; src_w * src_h * 4];
        // Solid red, fully opaque, at the src's center pixel (2, 2).
        let p = (2 * src_w + 2) * 4;
        src[p..p + 4].copy_from_slice(&[0, 0, 255, 255]);
        let dst_w = 2usize;
        let dst_h = 2usize;
        let mut dst = vec![0u8; dst_w * dst_h * 4];
        scale_frame_about(&mut dst, dst_w * 4, dst_w, dst_h, &src, src_w * 4, src_w, src_h, 0, 0.5);
        // The dst's center pixel (1, 1) samples the src's center exactly.
        let q = (dst_w + 1) * 4;
        assert_eq!(
            &dst[q..q + 4],
            &[0, 0, 255, 255],
            "the scaled pixel must land at the dst center"
        );
        // The dst's top-left pixel samples the src's top-left area, which is
        // transparent.
        assert_eq!(&dst[0..4], &[0, 0, 0, 0], "the rest of the dst stays transparent");
    }

    #[test]
    fn dim_color_scales_only_the_alpha_channel() {
        assert_eq!(dim_color([10, 20, 30, 255], 1.0), [10, 20, 30, 255]);
        assert_eq!(dim_color([10, 20, 30, 255], 0.5), [10, 20, 30, 128]);
        assert_eq!(dim_color([10, 20, 30, 255], 0.0), [10, 20, 30, 0]);
        // Out-of-range factors clamp.
        assert_eq!(dim_color([1, 2, 3, 128], 2.0), [1, 2, 3, 128]);
    }

    #[test]
    fn contrast_ratio_matches_wcag_reference_values() {
        // Sanity anchors: black vs white is 21:1, identical colors 1:1.
        assert!((contrast_ratio([0, 0, 0], [255, 255, 255]) - 21.0).abs() < 1e-3);
        assert_eq!(contrast_ratio([80, 90, 100], [80, 90, 100]), 1.0);
    }

    #[test]
    fn ensure_contrast_leaves_passing_colors_unchanged() {
        // White title text on the default near-black fill passes already:
        // the identity fast path must keep the exact color (bit-for-bit,
        // including the alpha channel).
        assert_eq!(
            ensure_contrast([255, 255, 255, 255], [0x12, 0x14, 0x1C, 0xEB], TEXT_CONTRAST_AA),
            [255, 255, 255, 255]
        );
        assert_eq!(
            ensure_contrast([200, 200, 200, 128], [18, 20, 28, 235], TEXT_CONTRAST_AA),
            [200, 200, 200, 128]
        );
    }

    #[test]
    fn ensure_contrast_darkens_on_a_light_background() {
        // On a light fill (a user-configured light background), brightening
        // toward white would push contrast the wrong way; the correction
        // must darken toward black and still reach the AA target.
        let bg = [230, 230, 230, 255];
        let white = [255, 255, 255, 255];
        assert!(
            contrast_ratio([white[0], white[1], white[2]], [bg[0], bg[1], bg[2]]) < TEXT_CONTRAST_AA,
            "precondition: white on the light fill must fail AA"
        );
        let fixed = ensure_contrast(white, bg, TEXT_CONTRAST_AA);
        assert!(
            contrast_ratio([fixed[0], fixed[1], fixed[2]], [bg[0], bg[1], bg[2]]) >= TEXT_CONTRAST_AA,
            "the corrected color must reach the AA target"
        );
        assert!(fixed[0] < white[0], "the correction must darken, not brighten");
        // A dark background keeps the old brightening behavior.
        let dark = [0x1B, 0x1B, 0x1B, 255];
        let dark_text = [60, 60, 60, 255];
        let lifted = ensure_contrast(dark_text, dark, TEXT_CONTRAST_AA);
        assert!(
            lifted[0] > dark_text[0],
            "on a dark fill the correction still brightens"
        );
    }

    #[test]
    fn ensure_contrast_brightens_the_reviewed_worst_case_colors() {
        // The concrete measured failure: a strict-guard-accepted
        // palette primary (60, 45, 84) tints the fill to ~(25, 24, 37) and
        // gives an artist-row text of ~(128, 118, 144) — 4.10:1 — while the
        // raw primary as meta-row text lands at 1.40:1 against the same
        // fill. Both must be lifted to the AA target by the composite-time
        // check.
        let bg = pill_fill_bg_test_bg(60, 45, 84);
        let artist = muted_accent([60, 45, 84, 255]);
        assert!(
            contrast_ratio([artist[0], artist[1], artist[2]], [bg[0], bg[1], bg[2]]) < TEXT_CONTRAST_AA,
            "precondition: the lifted artist color must start below the target"
        );
        let lifted_artist = ensure_contrast(artist, bg, TEXT_CONTRAST_AA);
        assert!(
            contrast_ratio(
                [lifted_artist[0], lifted_artist[1], lifted_artist[2]],
                [bg[0], bg[1], bg[2]]
            ) >= TEXT_CONTRAST_AA,
            "the artist color must be lifted to the AA target"
        );
        assert!(
            contrast_ratio([60, 45, 84], [bg[0], bg[1], bg[2]]) < TEXT_CONTRAST_AA,
            "precondition: the raw primary must start below the target"
        );
        let lifted_meta = ensure_contrast([60, 45, 84, 255], bg, TEXT_CONTRAST_AA);
        assert!(
            contrast_ratio([lifted_meta[0], lifted_meta[1], lifted_meta[2]], [bg[0], bg[1], bg[2]]) >= TEXT_CONTRAST_AA,
            "the meta-row color must be lifted to the AA target"
        );
        // The lift brightens rather than darkens, and keeps the alpha.
        assert!(lifted_meta[0] >= 60 && lifted_meta[1] >= 45 && lifted_meta[2] >= 84);
        assert_eq!(lifted_meta[3], 255);
    }

    /// The tinted fill `pill_fill_bg` produces for a palette primary, built
    /// through the same tinting math (used to keep the contrast tests
    /// independent of `OverlayState` construction).
    fn pill_fill_bg_test_bg(primary_r: u8, primary_g: u8, primary_b: u8) -> [u8; 4] {
        tinted_fill(
            [0x12, 0x14, 0x1C, 0xEB],
            [primary_r, primary_g, primary_b, 255],
            FILL_TINT_WEIGHT,
        )
    }

    #[test]
    fn row_unveil_is_full_at_rest_and_invisible_below_the_edge() {
        // Rest body bottom 200, band bottom 160 (the row's final position
        // inside the pill): at rest every row draws at full opacity.
        assert_eq!(row_unveil_alpha(200, 200, 160), 1.0);
        // While the edge is still above (or at) the band bottom the row must
        // not draw at all: its band is not yet covered by the pill body.
        assert_eq!(row_unveil_alpha(160, 200, 160), 0.0);
        assert_eq!(row_unveil_alpha(120, 200, 160), 0.0);
    }

    #[test]
    fn row_unveil_fades_with_the_sweep_of_the_edge() {
        // The row fades in over the edge's travel from band bottom to rest
        // bottom — halfway through that travel it is at half opacity — and
        // the same window fades it back out on the way down (collapse).
        let rest = 200;
        let band = 160;
        assert!((row_unveil_alpha(180, rest, band) - 0.5).abs() < 1e-5);
        assert!((row_unveil_alpha(170, rest, band) - 0.25).abs() < 1e-5);
        // Spring overshoot pushes the edge past the rest bottom: clamped.
        assert_eq!(row_unveil_alpha(210, rest, band), 1.0);
    }

    #[test]
    fn row_unveil_never_dims_a_band_at_the_rest_edge() {
        // A band whose bottom reaches the rest edge is impossible with the
        // constant-height pill layout (its `+ 8` slack), but must read as
        // fully revealed rather than invisible if it ever occurs.
        assert_eq!(row_unveil_alpha(200, 200, 200), 1.0);
        // Any band bottom at or below the rest edge is drawable at rest.
        assert_eq!(row_unveil_alpha(200, 200, 205), 1.0);
    }

    #[test]
    fn endpoint_pin_does_not_fake_a_velocity_spike() {
        // The pin forces the exact 1.0 endpoint; the numeric derivative just
        // before it must stay small and finite, or a leg crossing the pinned
        // boundary would feel like a lurch. (The velocity probe is the same
        // one the hover-collapse continuation seeds its spring with.)
        let v = ENTRANCE_GROW.velocity_at(1.0 - 1e-3, 0.0, 0.0);
        assert!(v.abs() < 10.0, "velocity spike at the pin: {v}");
        // And a fresh leg's starting velocity must be exactly the initial
        // condition it was seeded with: no curve may self-accelerate at t=0.
        let v0 = EXPAND_SPRING.velocity_at(0.0, 0.3, 0.0);
        assert!(v0.abs() < 1e-4, "self-acceleration at t=0: {v0}");
    }

    #[test]
    fn spring_collapse_continues_the_seed_velocity_and_pins_compact() {
        // The mirrored spring starts exactly at `from` with the seed
        // velocity, so a reversal continues the expand motion without a
        // kink; the leg end pins exactly at compact.
        let from = 0.6;
        let velocity = 2.5;
        let h = 1e-3;
        let start = spring_collapse(0.0, from, velocity);
        assert!(
            (start - from).abs() < 1e-6,
            "the collapsed leg must start at the reversed progress"
        );
        let slope = (spring_collapse(h, from, velocity) - start) / h;
        assert!(
            (slope - velocity).abs() < 1e-1,
            "the collapsed leg must start at the seeded velocity, got {slope}"
        );
        assert_eq!(spring_collapse(1.0, from, velocity), 0.0, "leg end must pin at compact");
        assert_eq!(spring_collapse(2.0, from, velocity), 0.0, "past the leg stays pinned");
    }

    #[test]
    fn spring_collapse_release_from_expanded_undershoots_once_then_pins_compact() {
        // The release case (cursor leaves the pinned-expanded pill, and the
        // exit shrink): from rest the mirrored spring returns to compact,
        // undershooting below it to the pinned `COLLAPSE_TROUGH` (ζ=0.6's
        // ~9.5 % step undershoot — the tail that drives the slow
        // settle-bounce), then recovering to pin exactly at 0 at the leg
        // end. The recovery may overshoot a hair above compact (the raw
        // spring's second oscillation, ~0.9 %), which `morph_size`'s clamp
        // keeps out of the geometry and `bounce_scale` ignores.
        let mut min = f32::INFINITY;
        let mut min_i = 0usize;
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let p = spring_collapse(t, 1.0, 0.0);
            if p < min {
                min = p;
                min_i = i;
            }
        }
        assert!(
            (min - COLLAPSE_TROUGH).abs() < 1e-3,
            "release must dip to the pinned trough, got {min}"
        );
        let mut max_after = f32::NEG_INFINITY;
        for i in min_i..=200 {
            let t = i as f32 / 200.0;
            max_after = max_after.max(spring_collapse(t, 1.0, 0.0));
        }
        assert!(
            max_after <= 0.01,
            "the recovery must overshoot compact only by a hair, got {max_after}"
        );
        assert_eq!(
            spring_collapse(1.0, 1.0, 0.0),
            0.0,
            "release must pin exactly at compact"
        );
        assert_eq!(spring_collapse(2.0, 1.0, 0.0), 0.0, "past the leg stays pinned");
    }

    #[test]
    fn reversal_seed_continues_the_expand_velocity() {
        // The reversal seed is the expand curve's value and velocity at the
        // reversal moment, the velocity converted to collapse-leg units so
        // the absolute (per-second) motion is unchanged across the flip.
        let config = Config::default();
        let expand_leg = morph_duration(&config, MorphDirection::Expand);
        let collapse_leg = morph_duration(&config, MorphDirection::Collapse);
        let mid = expand_leg.as_millis() as u64 / 2;
        let morph = HoverExpand {
            start: Instant::now() - Duration::from_millis(mid),
            direction: MorphDirection::Expand,
            from: 0.0,
            velocity: 0.0,
            done: false,
        };
        // Measure both sides against one fixed clock instant: the seed must
        // not depend on when the test process happens to schedule.
        let now = morph.start + Duration::from_millis(mid);
        let t = now.duration_since(morph.start).as_secs_f32() / expand_leg.as_secs_f32();
        let expected_velocity =
            EXPAND_SPRING.velocity_at(t, 0.0, 0.0) * collapse_leg.as_millis() as f32 / expand_leg.as_millis() as f32;
        let (from, velocity) = reversal_seed(&morph, &config, now);
        assert!(
            (from - spring_expand(t)).abs() < 1e-5,
            "the seed must carry the expand progress at the reversal"
        );
        assert!(
            (velocity - expected_velocity).abs() < 1e-5,
            "the seed must convert the velocity to collapse-leg units"
        );
    }

    #[test]
    fn hover_engaged_holds_a_leave_through_the_debounce_window() {
        // A leave counts as engaged during the debounce window, so boundary
        // jitter cannot cancel a morph; it stops counting once the window
        // expires — and a leave with the cursor back over never counts.
        let now = Instant::now();
        assert!(hover_engaged(true, None, now), "over is always engaged");
        assert!(
            hover_engaged(false, Some(now - Duration::from_millis(30)), now),
            "a fresh leave stays engaged through the debounce"
        );
        assert!(
            !hover_engaged(false, Some(now - Duration::from_millis(61)), now),
            "an expired leave is not engaged"
        );
        assert!(
            hover_engaged(true, Some(now - Duration::from_millis(61)), now),
            "the cursor over wins over a stale leave"
        );
    }

    #[test]
    fn hover_step_arms_dismiss_only_with_dismiss_on_hover() {
        let idle = HoverTick {
            cursor_over: false,
            morphing: false,
            morph_expanding: false,
            dismiss_armed: false,
        };
        // No hover, no morph: nothing to do.
        assert_eq!(hover_step(idle, true, true, false, false), HoverStep::None);
        // First hover over a laid-out expanded pill arms the dismiss.
        let over = HoverTick {
            cursor_over: true,
            ..idle
        };
        assert_eq!(hover_step(over, true, true, false, true), HoverStep::ArmDismiss);
        // The arm is one-way: an already-armed tick does nothing, so the
        // 500ms deadline keeps counting down while the cursor stays put.
        assert_eq!(
            hover_step(
                HoverTick {
                    cursor_over: true,
                    dismiss_armed: true,
                    ..idle
                },
                true,
                true,
                false,
                true
            ),
            HoverStep::None
        );
        // Without dismiss-on-hover the laid-out expanded pill is untouched.
        assert_eq!(hover_step(over, false, true, false, true), HoverStep::None);
        // A Compact pill with expand-off behaves exactly like an Expanded
        // one: the dismiss arms only while dismiss-on-hover is enabled.
        assert_eq!(hover_step(over, true, false, false, false), HoverStep::ArmDismiss);
        assert_eq!(hover_step(over, false, false, false, false), HoverStep::None);
    }

    #[test]
    fn hover_step_first_hover_expands_then_dismisses_with_dismiss_on_hover() {
        let idle = HoverTick {
            cursor_over: true,
            morphing: false,
            morph_expanding: false,
            dismiss_armed: false,
        };
        // First hover over a Compact pill starts the morph — regardless of
        // the dismiss toggle.
        assert_eq!(hover_step(idle, true, true, false, false), HoverStep::StartExpand);
        assert_eq!(hover_step(idle, false, true, false, false), HoverStep::StartExpand);
        // With dismiss-on-hover the expansion has been used: the next hover
        // over the compact pill dismisses (the second hover dismisses).
        assert_eq!(hover_step(idle, true, true, true, false), HoverStep::ArmDismiss);
        // Without dismiss-on-hover every hover re-expands instead.
        assert_eq!(hover_step(idle, false, true, true, false), HoverStep::StartExpand);
        // No cursor, no morph: nothing to do.
        assert_eq!(
            hover_step(
                HoverTick {
                    cursor_over: false,
                    ..idle
                },
                true,
                true,
                false,
                false
            ),
            HoverStep::None
        );
    }

    #[test]
    fn hover_step_morph_legs_reverse_on_leave() {
        let base = HoverTick {
            cursor_over: false,
            morphing: true,
            morph_expanding: true,
            dismiss_armed: false,
        };
        // Leaving mid-expand reverses the morph; leaving after the expansion
        // finished behaves identically — the release from the pinned state
        // (from = 1.0, velocity ≈ 0) runs the same collapse leg back to
        // compact instead of snapping.
        assert_eq!(hover_step(base, true, true, false, false), HoverStep::ReverseMorph);
        // Staying over the pill mid-expand keeps the morph running — the
        // morph-origin expanded state is an interaction, never armed.
        assert_eq!(
            hover_step(
                HoverTick {
                    cursor_over: true,
                    ..base
                },
                true,
                true,
                false,
                false
            ),
            HoverStep::None
        );
        // A collapse leg always runs to completion, cursor or not.
        let collapsing = HoverTick {
            morph_expanding: false,
            ..base
        };
        assert_eq!(hover_step(collapsing, true, true, false, false), HoverStep::None);
        assert_eq!(
            hover_step(
                HoverTick {
                    cursor_over: true,
                    ..collapsing
                },
                true,
                true,
                false,
                false
            ),
            HoverStep::None
        );
    }

    #[test]
    fn hover_step_never_arms_the_morph_expanded_state() {
        // The compact→expanded morph is an interaction: while it is in
        // flight or pinned, hovering never arms anything — only the laid-out
        // expanded pill arms (and only with dismiss-on-hover).
        let over = HoverTick {
            cursor_over: true,
            morphing: true,
            morph_expanding: true,
            dismiss_armed: false,
        };
        // Pinned/in-flight morph under the cursor: nothing, regardless of
        // the toggles.
        assert_eq!(hover_step(over, true, true, false, false), HoverStep::None);
        assert_eq!(hover_step(over, true, false, false, false), HoverStep::None);
        assert_eq!(hover_step(over, false, true, false, false), HoverStep::None);
        // A laid-out expanded pill (no morph) arms with dismiss-on-hover...
        let laid_out = HoverTick {
            morphing: false,
            ..over
        };
        assert_eq!(hover_step(laid_out, true, true, false, true), HoverStep::ArmDismiss);
        // ...and is untouched without it.
        assert_eq!(hover_step(laid_out, false, true, false, true), HoverStep::None);
    }

    #[test]
    fn laid_out_expanded_pill_is_never_held_or_deferred() {
        // An expanded pill has no hover interaction: the countdown is never
        // deferred for the cursor. With dismiss-on-hover (the default) the
        // first hover tick arms the one-way 500ms dismiss — even under the
        // cursor; without it, hovering changes nothing and an expired
        // deadline dismisses under the cursor either way.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::Expanded;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.layout = LayoutMode::Expanded;
        state.phase = Phase::Shown;
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        }));
        state.dismiss_at = Some(Instant::now() + Duration::from_secs(10));
        state.test_cursor_over = Some(true);
        state.tick();
        let remaining = state.dismiss_at.unwrap().saturating_duration_since(Instant::now());
        assert!(
            remaining <= Duration::from_millis(EARLY_EXIT_MS + 50),
            "dismiss-on-hover must cap the laid-out expanded pill near EARLY_EXIT_MS, got {remaining:?}"
        );
        assert!(matches!(state.phase, Phase::Shown));
        // The arm is one-way: staying over the pill does not re-arm.
        state.tick();
        let remaining = state.dismiss_at.unwrap().saturating_duration_since(Instant::now());
        assert!(
            remaining <= Duration::from_millis(EARLY_EXIT_MS + 50),
            "the one-way arm must not push the deadline while the cursor stays, got {remaining:?}"
        );
        // A deadline that expires under the cursor dismisses the laid-out
        // expanded pill: no hold defers it.
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(50));
        state.tick();
        assert!(
            matches!(state.phase, Phase::Collapsing(_)),
            "an expanded pill must dismiss under the cursor, no hold"
        );
    }

    #[test]
    fn laid_out_expanded_pill_ignores_hover_without_dismiss_on_hover() {
        // dismiss_on_hover off: hovering an expanded pill changes nothing —
        // no arm, no reset, no deferral; the countdown runs and dismisses.
        let mut config = Config::default();
        config.overlay.dismiss_on_hover = false;
        config.overlay.layout = LayoutMode::Expanded;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.layout = LayoutMode::Expanded;
        state.phase = Phase::Shown;
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        }));
        state.dismiss_at = Some(Instant::now() + Duration::from_secs(10));
        state.test_cursor_over = Some(true);
        state.tick();
        let remaining = state.dismiss_at.unwrap().saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::from_secs(9),
            "without dismiss-on-hover the hover must leave the deadline alone, got {remaining:?}"
        );
        // The expired deadline dismisses even under the cursor.
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(50));
        state.tick();
        assert!(
            matches!(state.phase, Phase::Collapsing(_)),
            "the countdown must dismiss under the cursor, no deferral"
        );
    }

    #[test]
    fn held_hover_pinned_pill_collapses_on_leave_and_resets() {
        // A hover-pinned expanded pill is an interaction: held past its
        // deadline it survives the tick, and on leave it runs its collapse
        // leg back to compact while the countdown resets to the full
        // duration — the collapsed pill keeps its normal time instead of
        // dismissing on the expired deadline.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::Compact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.phase = Phase::Shown;
        state.layout = LayoutMode::Compact;
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            source_app: "Spotify".into(),
            ..TrackInfo::default()
        }));
        // The hover already pinned the pill; the countdown ran out long ago
        // while the cursor stayed.
        let expand_leg = morph_duration(&state.config, MorphDirection::Expand);
        state.hover_expand = Some(HoverExpand {
            start: Instant::now() - expand_leg,
            direction: MorphDirection::Expand,
            from: 1.0,
            velocity: 0.0,
            done: true,
        });
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(100));
        state.test_cursor_over = Some(true);
        state.tick();
        assert!(
            matches!(state.phase, Phase::Shown) && state.hover_expand.is_some(),
            "a pinned pill must survive its deadline while the cursor stays"
        );
        // Leave: the debounce window keeps the hold for one tick...
        state.test_cursor_over = Some(false);
        state.tick();
        assert!(matches!(state.phase, Phase::Shown));
        // ...then the hold drops and leaving runs the collapse leg while the
        // countdown resets to the full duration.
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));
        state.tick();
        assert!(
            matches!(&state.hover_expand, Some(m) if m.direction == MorphDirection::Collapse),
            "leaving a pinned pill must run the collapse leg"
        );
        assert!(
            matches!(state.phase, Phase::Shown),
            "the dismissal must wait for the in-flight collapse leg"
        );
        let full = Duration::from_millis(state.config.overlay.duration_ms.max(500));
        let remaining = state.dismiss_at.unwrap().saturating_duration_since(Instant::now());
        assert!(
            remaining >= full - Duration::from_millis(100) && remaining <= full + Duration::from_millis(100),
            "leaving the interaction must reset the countdown to the full duration, got {remaining:?} (full {full:?})"
        );
        // Fast-forward the collapse leg: the fresh deadline means the pill
        // stays shown as compact instead of dismissing on the old deadline.
        state.hover_expand = Some(HoverExpand {
            start: Instant::now() - Duration::from_millis(2000),
            direction: MorphDirection::Collapse,
            from: 1.0,
            velocity: 0.0,
            done: false,
        });
        state.tick();
        assert!(state.hover_expand.is_none(), "the collapse leg must complete");
        assert!(
            matches!(state.phase, Phase::Shown),
            "the reset deadline must keep the collapsed pill shown"
        );
    }

    #[test]
    fn compact_second_hover_dismisses_with_dismiss_on_hover() {
        // The end-to-end compact flow with dismiss-on-hover (the default):
        // hover 1 expands (+ reset), leaving collapses (+ reset), and hover
        // 2 over the compact pill again dismisses instead of re-expanding.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::Compact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.phase = Phase::Shown;
        state.layout = LayoutMode::Compact;
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            source_app: "Spotify".into(),
            ..TrackInfo::default()
        }));
        // Hover 1: the expand starts.
        state.test_cursor_over = Some(true);
        state.tick();
        assert!(
            matches!(&state.hover_expand, Some(m) if m.direction == MorphDirection::Expand),
            "hover 1 must start the expand"
        );
        // Complete the expand leg: the expansion is now "used" for this
        // showing.
        let expand_leg = morph_duration(&state.config, MorphDirection::Expand);
        state.hover_expand.as_mut().unwrap().start = Instant::now() - expand_leg;
        state.tick();
        assert!(state.hover_expanded_once, "the completed expansion must be recorded");
        // Leave: the pill runs the collapse leg back to compact.
        state.test_cursor_over = Some(false);
        state.hover_leave_at = Some(Instant::now() - Duration::from_millis(100));
        state.tick();
        assert!(
            matches!(&state.hover_expand, Some(m) if m.direction == MorphDirection::Collapse),
            "leaving must run the collapse leg"
        );
        // Fast-forward the collapse leg so the pill is compact again.
        state.hover_expand = Some(HoverExpand {
            start: Instant::now() - Duration::from_millis(2000),
            direction: MorphDirection::Collapse,
            from: 1.0,
            velocity: 0.0,
            done: false,
        });
        state.tick();
        assert!(state.hover_expand.is_none(), "the collapse leg must complete");
        // Hover 2: the second hover dismisses — no new expand, 500ms arm.
        state.test_cursor_over = Some(true);
        state.tick();
        assert!(
            state.hover_expand.is_none(),
            "hover 2 must not re-expand with dismiss-on-hover"
        );
        let remaining = state.dismiss_at.unwrap().saturating_duration_since(Instant::now());
        assert!(
            remaining <= Duration::from_millis(EARLY_EXIT_MS + 50),
            "hover 2 must arm the 500ms dismiss, got {remaining:?}"
        );
    }

    #[test]
    fn track_update_while_held_swaps_in_place_and_stays_expanded() {
        // A new track (different media — normally queued for the next pill)
        // that arrives while the cursor holds a hover-expanded pill swaps
        // the content in place instead: queueing would tear the pill out
        // from under the cursor. The full duration is counted from the swap,
        // so leaving later gives the new content its normal time.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::Compact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            title: "First Song".into(),
            ..TrackInfo::default()
        }));
        state.phase = Phase::Shown;
        state.layout = LayoutMode::Compact;
        // The cursor holds a hover-pinned expanded pill whose countdown has
        // run out (the typical held state when an update lands).
        let expand_leg = morph_duration(&state.config, MorphDirection::Expand);
        state.hover_expand = Some(HoverExpand {
            start: Instant::now() - expand_leg,
            direction: MorphDirection::Expand,
            from: 1.0,
            velocity: 0.0,
            done: true,
        });
        state.dismiss_at = Some(Instant::now() - Duration::from_millis(100));
        state.test_cursor_over = Some(true);
        state.last_cursor_over_pill = true;

        state
            .queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(TrackInfo {
                source_app: "youtube-music".into(),
                title: "Next Song".into(),
                ..TrackInfo::default()
            })));
        state.receive_events();

        assert!(
            state.pending.is_empty(),
            "a held pill must not queue the update behind itself"
        );
        assert!(
            matches!(
                state.content.as_ref(),
                Some(MediaEvent::TrackChanged(t)) if t.title == "Next Song"
            ),
            "the swap must replace the content in place"
        );
        assert!(state.hover_expand.is_some(), "the pill must stay expanded while held");
        let full = Duration::from_millis(state.config.overlay.duration_ms.max(500));
        let remaining = state.dismiss_at.unwrap().saturating_duration_since(Instant::now());
        assert!(
            remaining >= full - Duration::from_millis(100) && remaining <= full + Duration::from_millis(100),
            "the swap must grant the full duration from the update, got {remaining:?} (full {full:?})"
        );
    }

    #[test]
    fn tick_that_starts_the_hover_morph_renders_on_that_same_tick() {
        // Regression for the tick-cadence gap: `animating` is computed at the
        // top of `tick` before the hover-detection code runs, so the tick
        // that sets `hover_expand` from None to Some must not use the stale
        // value in its render gate — the morph's first frame would be
        // skipped and the first rendered frame would sample the spring
        // already into the leg, reading as a jump from the still-compact
        // pill. Exercises `tick()` itself (with a fixed cursor), since every
        // isolated morph-math test passes regardless of this ordering bug.
        let mut config = Config::default();
        config.overlay.layout = LayoutMode::Compact;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.phase = Phase::Shown;
        state.layout = LayoutMode::Compact;
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            title: "Everything, Everywhere".into(),
            artist: "John Muirhead".into(),
            source_app: "Spotify".into(),
            ..TrackInfo::default()
        }));
        state.test_cursor_over = Some(true);
        let before = state.render_count;
        state.tick();
        assert!(
            state
                .hover_expand
                .as_ref()
                .is_some_and(|m| m.direction == MorphDirection::Expand),
            "precondition: this tick must start the hover morph"
        );
        assert_eq!(
            state.render_count,
            before + 1,
            "the tick that starts the morph must render its first frame"
        );
    }

    #[test]
    fn paused_track_changed_pill_does_not_crawl_the_progress_bar() {
        // A TrackChanged whose snapshot says Paused must freeze the bar: the
        // pill shows a paused track, not a playing one, so the tick must not
        // advance the estimate or flag the progress as playing.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.phase = Phase::Shown;
        let mut paused_track = track_for("spotify", "Pause Song", "Artist");
        paused_track.playback_state = Some(PlaybackState::Paused);
        state.content = Some(MediaEvent::TrackChanged(paused_track));
        state.progress_rate = Some(1.0);
        state.progress_anchor = Some((Instant::now(), 10.0));
        state.progress_duration_secs = Some(120);
        state.estimated_position_secs = Some(10.0);
        state.tick();
        assert!(
            !state.progress_playing,
            "a paused TrackChanged pill must not be treated as playing"
        );
        assert_eq!(
            state.estimated_position_secs,
            Some(10.0),
            "the bar must stay frozen for a paused track pill"
        );

        // A playing snapshot crawls as usual.
        let mut playing_track = track_for("spotify", "Play Song", "Artist");
        playing_track.playback_state = Some(PlaybackState::Playing);
        state.content = Some(MediaEvent::TrackChanged(playing_track));
        state.tick();
        assert!(state.progress_playing, "a playing TrackChanged pill must crawl");

        // A stateless snapshot (pre-carriage sessions, spurious recreation)
        // keeps the historical behavior: a track pill plays.
        state.content = Some(MediaEvent::TrackChanged(track_for("spotify", "No State", "Artist")));
        state.tick();
        assert!(
            state.progress_playing,
            "a stateless TrackChanged pill keeps the historical playing behavior"
        );
    }

    #[test]
    fn chrome_cache_key_tracks_the_bar_by_visible_pixels() {
        // The cached background is reused while a marquee scrolls, so the key
        // must change exactly when the drawn bar changes: the draw quantizes
        // the bar to integer pixels (`(pill_w * fraction).round()`), so a
        // position that stays inside the same pixel step keeps the key (and
        // the cache) and a step (or the bar appearing/disappearing) rebuilds.
        // Keying on the raw position would invalidate every playing tick and
        // never reuse the cache; keying on the paused scheduling fraction
        // would hide paused seeks behind a stale cached bar.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.progress_duration_secs = Some(120);
        state.estimated_position_secs = Some(60.0);
        // 300 px pill at scale 1.0: one bar pixel = 0.4 s.
        let at_half = state.chrome_cache_key(300, 100, 96, 1.0, false, None);
        state.estimated_position_secs = Some(60.1);
        assert_eq!(
            state.chrome_cache_key(300, 100, 96, 1.0, false, None),
            at_half,
            "sub-pixel movement must keep the cached background"
        );
        state.estimated_position_secs = Some(62.5);
        assert_ne!(
            state.chrome_cache_key(300, 100, 96, 1.0, false, None),
            at_half,
            "a visible bar step must rebuild the background"
        );
        state.estimated_position_secs = None;
        let barless = state.chrome_cache_key(300, 100, 96, 1.0, false, None);
        assert_ne!(barless, at_half, "a disappearing bar must invalidate the cache");
        state.progress_duration_secs = None;
        assert_eq!(
            state.chrome_cache_key(300, 100, 96, 1.0, false, None),
            barless,
            "no position and no duration both draw no bar"
        );
    }

    #[test]
    fn layered_background_plus_foreground_equals_full_render() {
        // The marquee fast path composites a cached `Background` raster with a
        // `Foreground` pass that draws only the scrolling rows. The union of
        // the two passes must be pixel-identical to the single `Full` pass, or
        // a scrolling pill would show a frozen, missing, or doubled row
        // between cache rebuilds. This pins the layer gating of every element
        // (scrolling rows, static rows, the play symbol, art tile, progress
        // bar): anything added to a draw without a matching layer guard makes
        // the buffers diverge here.
        let config = Config::default();
        let content = MediaEvent::TrackChanged(TrackInfo {
            source_app: "spotify".into(),
            title: "A deliberately very long title that cannot fit into the visible band".into(),
            artist: "The Artist".into(),
            album: "The Album".into(),
            playback_state: Some(PlaybackState::Playing),
            ..TrackInfo::default()
        });
        let inset = OverlayState::new(Config::default(), EventQueue::default()).aura_inset;
        let (pill_w, pill_h) = content_size_of(&config, &content, false);
        let buf_w = (pill_w.round() as i32 + inset * 2) as usize;
        let buf_h = (pill_h.round() as i32 + inset * 2) as usize;
        let body_bottom = inset + pill_h.round() as i32;
        let needed = buf_w * buf_h * 4;

        let mut full = OverlayState::new(Config::default(), EventQueue::default());
        let mut split = OverlayState::new(Config::default(), EventQueue::default());
        for state in [&mut full, &mut split] {
            // A live bar: painted by the background pass and keyed separately,
            // so the equivalence must hold with it drawn too.
            state.progress_duration_secs = Some(120);
            state.estimated_position_secs = Some(47.3);
        }
        let mut full_buf = vec![0u8; needed];
        let mut split_buf = vec![0u8; needed];

        // Single-pass reference render.
        full.render_layer = RenderLayer::Full;
        draw_pixels(
            &mut full,
            &mut full_buf,
            &content,
            buf_w,
            buf_h,
            1.0,
            false,
            None,
            body_bottom,
        )
        .unwrap();
        draw_text_pixels(
            &mut full,
            &mut full_buf,
            &content,
            buf_w as i32,
            1.0,
            false,
            None,
            body_bottom,
            body_bottom,
        );

        // The production two-pass sequence: background, then foreground.
        split.render_layer = RenderLayer::Background;
        draw_pixels(
            &mut split,
            &mut split_buf,
            &content,
            buf_w,
            buf_h,
            1.0,
            false,
            None,
            body_bottom,
        )
        .unwrap();
        draw_text_pixels(
            &mut split,
            &mut split_buf,
            &content,
            buf_w as i32,
            1.0,
            false,
            None,
            body_bottom,
            body_bottom,
        );
        split.render_layer = RenderLayer::Foreground;
        draw_text_pixels(
            &mut split,
            &mut split_buf,
            &content,
            buf_w as i32,
            1.0,
            false,
            None,
            body_bottom,
            body_bottom,
        );

        assert_eq!(
            full_buf, split_buf,
            "the Background + Foreground passes must composite exactly like a Full render"
        );
    }

    #[test]
    fn morph_size_lerps_between_endpoints_and_never_overshoots() {
        let config = Config::default();
        let content = MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            title: "A reasonably long title".into(),
            ..TrackInfo::default()
        });
        let (compact_w, compact_h) = content_size_of(&config, &content, true);
        let (expanded_w, expanded_h) = content_size_of(&config, &content, false);
        assert!(
            compact_w < expanded_w || compact_h < expanded_h,
            "the morph needs a real size difference"
        );
        // Endpoints map exactly onto the plain sizes.
        assert_eq!(
            morph_size(
                &config,
                &content,
                MorphProgress {
                    width: 0.0,
                    height: 0.0
                }
            ),
            (compact_w, compact_h)
        );
        assert_eq!(
            morph_size(
                &config,
                &content,
                MorphProgress {
                    width: 1.0,
                    height: 1.0
                }
            ),
            (expanded_w, expanded_h)
        );
        // Every progress (including an eased overshoot past 1.0 and negative
        // inputs) stays clamped between the endpoints per dimension.
        for progress in [-0.5, 0.0, 0.25, 0.5, 0.75, 1.0, 1.5] {
            let (width, height) = morph_size(
                &config,
                &content,
                MorphProgress {
                    width: progress,
                    height: progress,
                },
            );
            assert!(
                width >= compact_w.min(expanded_w) && width <= compact_w.max(expanded_w),
                "width out of range at {progress}: {width}"
            );
            assert!(
                height >= compact_h.min(expanded_h) && height <= compact_h.max(expanded_h),
                "height out of range at {progress}: {height}"
            );
        }
        // Per-axis independence: the height axis trails the width through the
        // morph (the lag), so the geometry reflects each axis's own progress.
        let leading = morph_size(
            &config,
            &content,
            MorphProgress {
                width: 0.9,
                height: 0.3,
            },
        );
        assert!(
            leading.0 > leading.1,
            "width must lead the height through the morph, got {leading:?}"
        );
        // The spring's mid-flight overshoot (the easing peaks well past 1.0)
        // must never leak into the geometry: the rendered rectangle, the
        // clipping region, and the hit-testing bounds all stay inside the
        // Compact..Expanded interval at every sampled frame.
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let progress = MorphProgress {
                width: spring_expand(t),
                height: lagged_expand(&EXPAND_SPRING, t, MORPH_LAG),
            };
            let (width, height) = morph_size(&config, &content, progress);
            assert!(
                width >= compact_w.min(expanded_w) && width <= compact_w.max(expanded_w),
                "spring-driven width out of range at frame {i}: {width}"
            );
            assert!(
                height >= compact_h.min(expanded_h) && height <= compact_h.max(expanded_h),
                "spring-driven height out of range at frame {i}: {height}"
            );
        }
    }

    #[test]
    fn spring_expand_overshoots_then_settles_exactly() {
        // Starts at compact, and the settle endpoint is exact: the pinned
        // expanded state must render at the true expanded size, not a hair
        // short (a sub-pixel shortfall would clip the expanded content).
        assert!((spring_expand(0.0) - 0.0).abs() < 1e-4);
        assert_eq!(spring_expand(1.0), 1.0);
        assert_eq!(
            spring_expand(2.0),
            1.0,
            "out-of-range input clamps to the settle endpoint"
        );
        // The spring overshoots past 1.0 mid-flight — the geometry clamp in
        // `morph_size` contains that — with a controlled amplitude (ζ = 0.7,
        // the same damping the entrance spring uses).
        let samples: Vec<f32> = (0..=200).map(|i| spring_expand(i as f32 / 200.0)).collect();
        let peak = samples.iter().cloned().fold(0.0_f32, f32::max);
        assert!(
            (1.03..1.06).contains(&peak),
            "spring overshoot must be visible but controlled, got {peak}"
        );
        // The undershoot after the peak is what the clamp cannot hide: values
        // below 1.0 pass straight through `morph_size`, so a large undershoot
        // would visibly shrink the pill and regrow it in the last stretch of
        // the leg (the end-of-morph reversal). ζ = 0.7 keeps it sub-pixel —
        // the regression pin for the spring's damping choice. Measured from
        // the peak onward (the pre-peak climb starts at 0 by design).
        let peak_i = samples.iter().position(|v| *v == peak).unwrap_or(0);
        let trough = samples[peak_i..].iter().cloned().fold(f32::INFINITY, f32::min);
        assert!(
            trough >= 0.995,
            "the undershoot must stay invisible to the clamp, got {trough}"
        );
        // Never negative, never wild.
        for v in &samples {
            assert!((0.0..=1.3).contains(v), "spring out of range: {v}");
        }
        // The curve crosses 1.0 exactly twice: up into the overshoot and down
        // into the settle. One visible bounce, then a stable expanded state —
        // no repeated wobble (the next oscillation would land past the leg).
        let crossings = samples.windows(2).filter(|w| (w[0] - 1.0) * (w[1] - 1.0) < 0.0).count();
        assert_eq!(crossings, 2, "the spring must overshoot once and settle once");
        // The attack is fast: well past half size within the first third.
        assert!(spring_expand(1.0 / 3.0) > 0.6, "the spring must feel responsive");
    }

    #[test]
    fn collapse_leg_is_shorter_and_settles_with_a_controlled_undershoot() {
        let config = Config::default();
        let expand = morph_duration(&config, MorphDirection::Expand);
        let collapse = morph_duration(&config, MorphDirection::Collapse);
        assert!(
            collapse < expand,
            "collapse must be faster than expand: {collapse:?} vs {expand:?}"
        );
        // Sampled over its whole leg, the collapse runs back to compact on
        // both axes: never rising above the progress it reversed from,
        // dipping once below compact to the pinned trough (ζ=0.6's
        // undershoot, scaled by the remaining distance `from` — the tail the
        // settle-bounce renders), and pinning exactly at compact at the leg
        // end. After its own trough each axis recovers, overshooting a hair
        // above compact at most (the raw spring's second oscillation), which
        // the geometry clamp hides. The height axis holds at `from` through
        // the lag before it starts moving.
        let from = 0.6;
        let expected_trough = COLLAPSE_TROUGH * from;
        let mut samples = Vec::with_capacity(201);
        for i in 0..=200 {
            let elapsed = Duration::from_millis((collapse.as_millis() as u64 * i / 200).max(1));
            let morph = HoverExpand {
                start: Instant::now() - elapsed,
                direction: MorphDirection::Collapse,
                from,
                velocity: 0.0,
                done: false,
            };
            let progress = hover_progress(&morph, &config);
            assert!(
                progress.width <= from + 1e-3,
                "collapse must not rise above its start: width {0} at step {i}",
                progress.width
            );
            assert!(
                progress.height <= from + 1e-3,
                "collapse must not rise above its start: height {0} at step {i}",
                progress.height
            );
            samples.push((progress.width, progress.height));
        }
        let min_width = samples.iter().map(|(w, _)| *w).fold(f32::INFINITY, f32::min);
        let min_height = samples.iter().map(|(_, h)| *h).fold(f32::INFINITY, f32::min);
        assert!(
            (min_width - expected_trough).abs() < 1e-3,
            "collapse must dip to the pinned trough, got {min_width}"
        );
        assert!(
            (min_height - expected_trough).abs() < 1e-3,
            "the lagged height must dip to the same trough, got {min_height}"
        );
        let min_w_i = samples.iter().position(|(w, _)| *w == min_width).unwrap();
        let min_h_i = samples.iter().position(|(_, h)| *h == min_height).unwrap();
        // After its own trough each axis recovers; the recovery may
        // overshoot compact by only a hair.
        let max_width = samples[min_w_i..]
            .iter()
            .map(|(w, _)| *w)
            .fold(f32::NEG_INFINITY, f32::max);
        let max_height = samples[min_h_i..]
            .iter()
            .map(|(_, h)| *h)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            max_width <= 0.01 && max_height <= 0.01,
            "the recovery must overshoot compact only by a hair: {max_width} / {max_height}"
        );
        let done = HoverExpand {
            start: Instant::now() - collapse,
            direction: MorphDirection::Collapse,
            from,
            velocity: 0.0,
            done: false,
        };
        let progress = hover_progress(&done, &config);
        assert_eq!(progress.width, 0.0, "collapse must reach compact");
        assert_eq!(progress.height, 0.0, "collapse must reach compact");
    }

    #[test]
    fn expand_leg_is_more_expressive_than_collapse() {
        // The asymmetry that makes expansion feel like a reveal and collapse
        // like a close: expand springs (overshoots its endpoint), collapse
        // undershoots compact only in its tail (the settle-bounce); and the
        // collapse leg itself is shorter.
        let config = Config::default();
        assert!(morph_duration(&config, MorphDirection::Collapse) < morph_duration(&config, MorphDirection::Expand));
        // Mid-flight, the expand spring travels past its endpoint...
        let expand_mid = HoverExpand {
            start: Instant::now()
                - Duration::from_millis(morph_duration(&config, MorphDirection::Expand).as_millis() as u64 * 2 / 5),
            direction: MorphDirection::Expand,
            from: 0.0,
            velocity: 0.0,
            done: false,
        };
        let expand_progress = hover_progress(&expand_mid, &config);
        assert!(
            expand_progress.width > 1.0,
            "expand must overshoot mid-flight, got {}",
            expand_progress.width
        );
        assert!(
            expand_progress.height > 0.0,
            "the lagged height must be mid-chase, got {}",
            expand_progress.height
        );
        assert!(
            expand_progress.height < expand_progress.width,
            "the height axis must trail the width axis, got {expand_progress:?}"
        );
        // ...while the collapse at its own midpoint has only closed toward
        // compact, dipping into the undershoot band (the settle-bounce) but
        // never past the pinned trough.
        let collapse_mid = HoverExpand {
            start: Instant::now()
                - Duration::from_millis(morph_duration(&config, MorphDirection::Collapse).as_millis() as u64 / 2),
            direction: MorphDirection::Collapse,
            from: 0.75,
            velocity: 0.0,
            done: false,
        };
        let collapse_progress = hover_progress(&collapse_mid, &config);
        assert!(
            collapse_progress.width >= COLLAPSE_TROUGH * 0.75 - 1e-3 && collapse_progress.width < 0.75,
            "collapse must be inside its return path, got {}",
            collapse_progress.width
        );
    }

    #[test]
    fn morph_anchor_stays_fixed_while_the_pill_grows() {
        // The morph must feel like the card is unfolding in place: the
        // anchored edge(s) of the Compact pill do not move as the size
        // lerps to the expanded geometry (placement re-anchors every frame
        // from the current size).
        let config = Config::default();
        let content = MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            title: "A reasonably long title".into(),
            ..TrackInfo::default()
        });
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let (compact_w, compact_h) = morph_size(
            &config,
            &content,
            MorphProgress {
                width: 0.0,
                height: 0.0,
            },
        );
        let (expanded_w, expanded_h) = morph_size(
            &config,
            &content,
            MorphProgress {
                width: 1.0,
                height: 1.0,
            },
        );
        assert!(
            compact_w < expanded_w && compact_h < expanded_h,
            "the morph needs a real size difference"
        );
        // Bottom-right anchor: the right and bottom edges of the pill body
        // stay pinned while it grows left/up.
        let bottom_right = OverlayPos {
            vertical: VerticalPosition::Bottom,
            horizontal: HorizontalPosition::Right,
            ..anchor_pos(None, None)
        };
        let compact_pt = placement(work, compact_w as i32, compact_h as i32, &bottom_right, 0, 1.0);
        let expanded_pt = placement(work, expanded_w as i32, expanded_h as i32, &bottom_right, 0, 1.0);
        assert_eq!(
            compact_pt.x + compact_w as i32,
            expanded_pt.x + expanded_w as i32,
            "the right edge must stay anchored while the pill grows"
        );
        assert_eq!(
            compact_pt.y + compact_h as i32,
            expanded_pt.y + expanded_h as i32,
            "the bottom edge must stay anchored while the pill grows"
        );
        // Top-center anchor: the horizontal center of the pill body stays
        // pinned while it grows outward to both sides. (Center placement
        // halves the span with integer division, so the pixel center may
        // jitter by 1 px between sizes of different parity.)
        let top_center = OverlayPos {
            vertical: VerticalPosition::Top,
            horizontal: HorizontalPosition::Center,
            ..anchor_pos(None, None)
        };
        let compact_pt = placement(work, compact_w as i32, compact_h as i32, &top_center, 0, 1.0);
        let expanded_pt = placement(work, expanded_w as i32, expanded_h as i32, &top_center, 0, 1.0);
        let center_drift = (compact_pt.x + compact_w as i32 / 2 - (expanded_pt.x + expanded_w as i32 / 2)).abs();
        assert!(
            center_drift <= 1,
            "the horizontal center must stay anchored, drifted {center_drift}px"
        );
        assert_eq!(compact_pt.y, expanded_pt.y, "the top edge must stay anchored");
    }

    #[test]
    fn hover_progress_legs_bounded_and_continuous() {
        let config = Config::default();
        // An expand leg just started is at compact...
        let expand = HoverExpand {
            start: Instant::now(),
            direction: MorphDirection::Expand,
            from: 0.0,
            velocity: 0.0,
            done: false,
        };
        let expand = hover_progress(&expand, &config);
        assert!(
            expand.width.abs() < 1e-3 && expand.height.abs() < 1e-3,
            "a fresh expand must be at compact, got {expand:?}"
        );
        // ...and a finished one is at exactly expanded (the spring settles
        // to an exact endpoint, and the lagged follower catches up and pins).
        let finished = HoverExpand {
            start: Instant::now() - morph_duration(&config, MorphDirection::Expand),
            direction: MorphDirection::Expand,
            from: 0.0,
            velocity: 0.0,
            done: true,
        };
        assert_eq!(
            hover_progress(&finished, &config),
            MorphProgress {
                width: 1.0,
                height: 1.0
            }
        );
        // A collapse leg starts from the progress it reversed at and settles
        // down to compact over its own (shorter) leg duration, never jumping
        // back up to expanded. The height axis holds at `from` through the
        // lag while the width axis is already moving.
        let collapse = HoverExpand {
            start: Instant::now(),
            direction: MorphDirection::Collapse,
            from: 0.6,
            velocity: 0.0,
            done: false,
        };
        let at_start = hover_progress(&collapse, &config);
        assert!(
            (at_start.width - 0.6).abs() < 1e-3 && (at_start.height - 0.6).abs() < 1e-3,
            "a fresh collapse must start at the reversal point, got {at_start:?}"
        );
        let collapse_done = HoverExpand {
            start: Instant::now() - morph_duration(&config, MorphDirection::Collapse),
            direction: MorphDirection::Collapse,
            from: 0.6,
            velocity: 0.0,
            done: false,
        };
        assert_eq!(
            hover_progress(&collapse_done, &config),
            MorphProgress {
                width: 0.0,
                height: 0.0
            }
        );
    }

    #[test]
    fn starting_the_morph_keeps_the_layout_compact() {
        // Invariant: the hover morph is render sub-state. Starting it must
        // never touch `layout` (or the effective compact decision), so the
        // pill keeps its compact anchor position while it grows.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.layout = LayoutMode::Compact;
        state.phase = Phase::Shown;
        state.content = Some(MediaEvent::TrackChanged(TrackInfo {
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        }));
        state.hover_expand = Some(HoverExpand {
            start: Instant::now(),
            direction: MorphDirection::Expand,
            from: 0.0,
            velocity: 0.0,
            done: false,
        });
        assert_eq!(
            state.layout,
            LayoutMode::Compact,
            "the morph must not change the layout"
        );
        assert!(
            state.effective_compact(),
            "the pill must stay effectively compact while morphing"
        );
        // The morph size is available for the hitbox and the render.
        let (width, height) = state.content_size_at(1.0).unwrap();
        assert!(width > 0 && height > 0);
    }

    #[test]
    fn expanding_alpha_reaches_full_before_the_end() {
        let mut config = Config::default();
        config.overlay.animation_ms = 200;
        let mut state = OverlayState::new(config, EventQueue::default());
        state.phase = Phase::Expanding(Instant::now() - Duration::from_millis(100));
        // At half the duration the grow is still mid-flight (overshooting, so
        // not settled at 1.0), but alpha must already be at full strength
        // (decoupled opacity).
        let frame = state.frame();
        assert_eq!(frame.alpha, 255);
        let grow = frame.morph.expect("an expanded pill must grow in");
        assert!(
            (grow.width - 1.0).abs() > 1e-3,
            "grow should not be settled at t=0.5, got {}",
            grow.width
        );
        assert!(
            (grow.height - 1.0).abs() > 1e-3,
            "the lagged height must still be mid-chase at t=0.5, got {}",
            grow.height
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
            PlaybackType::Unknown,
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
            PlaybackType::Unknown,
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
    fn now_playing_video_draws_a_connected_player_box() {
        let mut pixels = vec![0u8; 256 * 256 * 4];
        // Icon box: 192px at left edge 32, vertically centered (y 32..224).
        // Frame is 0.72S wide (138px → x 59..197) and 0.48S tall (92px →
        // y 82..174); rails are 0.055S (11px) thick; the play triangle is
        // 0.22S×0.26S (x ≈112..154, y ≈103..153), shifted right 0.025S.
        draw_symbol_pixels(
            &mut pixels,
            256,
            224,
            32,
            192.0,
            PlaybackState::NowPlaying,
            PlaybackType::Video,
            [255, 255, 255, 255],
        );
        let lit = |x: i32, y: i32| pixels[((y * 256 + x) * 4 + 3) as usize] > 0;
        // Rail midpoints: each of the four sides must be solid.
        assert!(lit(59, 128), "left rail must be drawn");
        assert!(lit(191, 128), "right rail must be drawn");
        assert!(lit(128, 82), "top rail must be drawn");
        assert!(lit(128, 168), "bottom rail must be drawn");
        // Corners: the rail overlap region (x 59..70 / y 82..93 and the
        // mirror) must be solid — the frame reads connected, not notched.
        for (cx, cy) in [(64, 87), (192, 87), (64, 169), (192, 169)] {
            assert!(lit(cx, cy), "corner ({cx},{cy}) must be solid");
        }
        // Interior between the rails and the triangle must be hollow.
        for (cx, cy) in [(80, 128), (160, 128)] {
            assert!(!lit(cx, cy), "interior ({cx},{cy}) must be hollow");
        }
        // The play triangle interior must be solid.
        assert!(lit(130, 120), "play triangle must be solid");
    }

    #[test]
    fn video_icon_rails_end_on_one_shared_bottom_edge() {
        // Regression: the frame rails used to round their own (x,w)/(y,h)
        // pairs, so at smaller glyph sizes the vertical rails could extend
        // one pixel BELOW the bottom rail's edge (e.g. size 24: vertical
        // bottom = round(0.48·24) = 12 vs horizontal bottom =
        // round(10.2) + round(1.32) = 11). The six frame edges are rounded
        // once and every rail derives from them, so all four rails end on
        // the same rows.
        let mut pixels = vec![0u8; 256 * 256 * 4];
        let size = 64.0_f32;
        draw_symbol_pixels(
            &mut pixels,
            256,
            224,
            32,
            size,
            PlaybackState::NowPlaying,
            PlaybackType::Video,
            [255, 255, 255, 255],
        );
        let fw = 0.72 * size;
        let fh = 0.48 * size;
        let thick = 0.055 * size;
        let left = (224.0 - size) + (size - fw) / 2.0;
        let top = 32.0 + size * 0.5 - fh / 2.0;
        let l = left.round() as i32;
        let r = (left + fw).round() as i32;
        let b = (top + fh).round() as i32;
        let th = thick.round() as i32;
        let bottommost = |x: i32| (0..=b).rev().find(|&y| pixels[((y * 256 + x) * 4 + 3) as usize] > 0);
        // The left/right rails' centerlines and the bottom rail's straight
        // span all end on the same row (b-1); nothing paints at row b or
        // below anywhere across the frame width.
        for x in [l + th / 2, r - th / 2, l + (r - l) / 2] {
            assert_eq!(
                bottommost(x),
                Some(b - 1),
                "rail at column {x} must end at row {}",
                b - 1
            );
        }
        for x in l..r {
            assert_eq!(
                pixels[((b * 256 + x) * 4 + 3) as usize],
                0,
                "nothing may paint below the bottom edge at column {x}"
            );
        }
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
            // Fake displays have no taskbars/app bars, so rcMonitor == rcWork.
            monitor: RECT {
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

    #[test]
    fn placement_custom_overrides_land_the_pill_body_at_the_coordinate() {
        // Custom `position_x`/`position_y` are pill-body coordinates: the aura
        // inset is subtracted so the body (not the window) top-left lands at the
        // configured spot, exactly like the anchor arms.
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let custom = OverlayPos {
            x: Some(200),
            y: Some(300),
            ..anchor_pos(None, None)
        };
        // inset 10 at 1.0 scale: the window sits `inset` above/left of the body.
        let point = placement(work, 400, 100, &custom, 10, 1.0);
        assert_eq!(
            point,
            POINT { x: 190, y: 290 },
            "the window top-left is the body coordinate minus the aura inset"
        );
        // At a non-1.0 scale the override scales first, then the inset subtracts.
        let point = placement(work, 400, 100, &custom, 15, 1.5);
        assert_eq!(
            point,
            POINT {
                x: (200f32 * 1.5).round() as i32 - 15,
                y: (300f32 * 1.5).round() as i32 - 15
            },
            "scale applies to the override, then the inset is subtracted"
        );
    }

    #[test]
    fn placement_with_no_work_area_inset_uses_the_full_monitor() {
        // Fullscreen or no taskbar: Windows reports rcWork == rcMonitor, so the
        // pill sits at the configured margin from the physical monitor edges —
        // never retaining a stale taskbar-sized gap.
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let bottom = anchor_pos(None, None);
        assert_eq!(
            placement(work, 400, 100, &bottom, 0, 1.0),
            POINT {
                x: (1920 - 400) / 2,
                y: 1080 - 100 - 8
            },
            "bottom anchor uses the full-monitor bottom when there is no inset"
        );
        let top = OverlayPos {
            vertical: VerticalPosition::Top,
            ..anchor_pos(None, None)
        };
        assert_eq!(
            placement(work, 400, 100, &top, 0, 1.0),
            POINT {
                x: (1920 - 400) / 2,
                y: 8
            }
        );
    }

    #[test]
    fn placement_top_inset_anchors_against_the_work_area_top() {
        // Taskbar along the top edge: rcWork.top > rcMonitor.top. The top anchor
        // must read the work-area top, not the physical monitor top.
        let work = RECT {
            left: 0,
            top: 40,
            right: 1920,
            bottom: 1080,
        };
        let top_left = OverlayPos {
            vertical: VerticalPosition::Top,
            horizontal: HorizontalPosition::Left,
            ..anchor_pos(None, None)
        };
        let point = placement(work, 400, 100, &top_left, 0, 1.0);
        assert_eq!(
            point,
            POINT { x: 8, y: 48 },
            "top-left pill sits margin from the work-area top-left"
        );
        // Anchoring on the physical monitor top (0) would land the pill at y = 8.
        assert_ne!(point.y, 8);
    }

    #[test]
    fn placement_top_inset_sits_the_pill_body_margin_from_the_work_edge() {
        // With a non-zero aura (`inset`), the top anchor must place the PILL
        // BODY (window.y + inset) at exactly `margin` from the work-area top —
        // not `margin + inset`. The old `+ inset` sign shifted the pill down by
        // a double inset (pill body sat at margin + 2*inset). A full-monitor work
        // rect (no taskbar) isolates the sign from the fullscreen/aura logic.
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let top = OverlayPos {
            vertical: VerticalPosition::Top,
            ..anchor_pos(None, None)
        };
        let inset = 6;
        let point = placement(work, 400, 100, &top, inset, 1.0);
        // window.y = work.top + margin - inset = 0 + 8 - 6 = 2. The buggy `+ inset`
        // sign would yield y = 14 here.
        assert_eq!(
            point,
            POINT {
                x: (1920 - 400) / 2 - inset,
                y: 8 - inset
            },
            "top anchor window.y = work.top + margin - inset"
        );
        // Pill body top = window.y + inset = margin below the work-area top,
        // matching the Bottom arm (vertical symmetry at this inset).
        let pill_top = point.y + inset;
        assert_eq!(
            pill_top - work.top,
            8,
            "pill body sits exactly `margin` (8) from the work-area top edge"
        );
        let bottom = anchor_pos(None, None);
        let bpoint = placement(work, 400, 100, &bottom, inset, 1.0);
        let pill_bottom = bpoint.y + 100 + inset;
        assert_eq!(
            work.bottom - pill_bottom,
            8,
            "pill body sits exactly `margin` (8) from the work-area bottom edge"
        );
    }

    #[test]
    fn placement_left_inset_sits_the_pill_body_margin_from_the_work_edge() {
        // Mirror of the Top-arm regression, for the left edge: with a non-zero
        // aura (`inset`), the Left anchor must place the PILL BODY (window.x +
        // inset) at exactly `margin` from the work-area left — not `margin + inset`.
        // The old `+ inset` sign (identical to the former Top bug) shifted the pill
        // right by a double inset. A full-monitor work rect (no taskbar) isolates
        // the sign. (vertical defaults to Bottom via anchor_pos, the same
        // convention `placement_left_inset_anchors_against_the_work_area_left`
        // uses; only the horizontal Left arm is under test here.)
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let left = OverlayPos {
            horizontal: HorizontalPosition::Left,
            ..anchor_pos(None, None)
        };
        let inset = 6;
        let point = placement(work, 400, 100, &left, inset, 1.0);
        // window.x = work.left + margin - inset = 0 + 8 - 6 = 2. The buggy `+ inset`
        // sign would yield x = 14 here.
        assert_eq!(
            point,
            POINT {
                x: 8 - inset,
                y: 1080 - 100 - 8 - inset
            },
            "left anchor window.x = work.left + margin - inset"
        );
        // Pill body left = window.x + inset = margin from the work-area left,
        // matching the Right arm (horizontal symmetry at this inset).
        let pill_left = point.x + inset;
        assert_eq!(
            pill_left - work.left,
            8,
            "pill body sits exactly `margin` (8) from the work-area left edge"
        );
        let right = OverlayPos {
            horizontal: HorizontalPosition::Right,
            ..anchor_pos(None, None)
        };
        let rpoint = placement(work, 400, 100, &right, inset, 1.0);
        let pill_right = rpoint.x + 400 + inset;
        assert_eq!(
            work.right - pill_right,
            8,
            "pill body sits exactly `margin` (8) from the work-area right edge"
        );
    }

    #[test]
    fn placement_left_inset_anchors_against_the_work_area_left() {
        // Taskbar along the left edge: rcWork.left > rcMonitor.left.
        let work = RECT {
            left: 80,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let left = OverlayPos {
            horizontal: HorizontalPosition::Left,
            ..anchor_pos(None, None)
        };
        let point = placement(work, 400, 100, &left, 0, 1.0);
        assert_eq!(
            point,
            POINT {
                x: 88,
                y: 1080 - 100 - 8
            },
            "left pill sits margin from the work-area left"
        );
        // Anchoring on the physical monitor left (0) would land the pill at x = 8.
        assert_ne!(point.x, 8);
    }

    #[test]
    fn placement_right_inset_anchors_against_the_work_area_right() {
        // Taskbar along the right edge: rcWork.right < rcMonitor.right.
        let work = RECT {
            left: 0,
            top: 0,
            right: 1840,
            bottom: 1080,
        };
        let right = OverlayPos {
            horizontal: HorizontalPosition::Right,
            ..anchor_pos(None, None)
        };
        let point = placement(work, 400, 100, &right, 0, 1.0);
        assert_eq!(
            point,
            POINT {
                x: 1840 - 400 - 8,
                y: 1080 - 100 - 8
            }
        );
        // Anchoring on the physical monitor right (1920) would land the pill at x = 1512.
        assert_ne!(point.x, 1920 - 400 - 8);
    }

    #[test]
    fn placement_bottom_inset_anchors_against_the_work_area_bottom() {
        // Taskbar along the bottom edge: rcWork.bottom < rcMonitor.bottom.
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let bottom = anchor_pos(None, None);
        let point = placement(work, 400, 100, &bottom, 0, 1.0);
        assert_eq!(
            point,
            POINT {
                x: 760,
                y: 1040 - 100 - 8
            }
        );
        // Anchoring on the physical monitor bottom (1080) would land the pill at y = 972.
        assert_ne!(point.y, 1080 - 100 - 8);
    }

    #[test]
    fn placement_respects_all_four_work_area_insets_at_once() {
        // Synthetic geometry with an inset on every edge: each anchor reads its
        // own work-area boundary, never a physical-monitor boundary.
        let work = RECT {
            left: 80,
            top: 40,
            right: 1840,
            bottom: 1040,
        };
        let span_w = work.right - work.left;
        let top_left = OverlayPos {
            vertical: VerticalPosition::Top,
            horizontal: HorizontalPosition::Left,
            ..anchor_pos(None, None)
        };
        let bottom_right = OverlayPos {
            vertical: VerticalPosition::Bottom,
            horizontal: HorizontalPosition::Right,
            ..anchor_pos(None, None)
        };
        let top_center = OverlayPos {
            vertical: VerticalPosition::Top,
            horizontal: HorizontalPosition::Center,
            ..anchor_pos(None, None)
        };
        assert_eq!(placement(work, 400, 100, &top_left, 0, 1.0), POINT { x: 88, y: 48 });
        assert_eq!(
            placement(work, 400, 100, &bottom_right, 0, 1.0),
            POINT {
                x: 1840 - 400 - 8,
                y: 1040 - 100 - 8
            },
        );
        assert_eq!(
            placement(work, 400, 100, &top_center, 0, 1.0),
            POINT {
                x: 80 + (span_w - 400) / 2,
                y: 48
            },
        );
    }

    #[test]
    fn placement_margin_runs_from_the_work_area_edge_exactly_once() {
        // Doubling the configured margin shifts the pill by exactly the extra
        // pixels, edge-relative — the margin is never compounded with a separate
        // edge offset (section 17: no double application).
        let work = RECT {
            left: 80,
            top: 40,
            right: 1840,
            bottom: 1040,
        };
        let near = OverlayPos {
            vertical: VerticalPosition::Top,
            horizontal: HorizontalPosition::Left,
            margin: 8,
            ..anchor_pos(None, None)
        };
        let far = OverlayPos {
            vertical: VerticalPosition::Top,
            horizontal: HorizontalPosition::Left,
            margin: 28,
            ..anchor_pos(None, None)
        };
        let near_pt = placement(work, 400, 100, &near, 0, 1.0);
        let far_pt = placement(work, 400, 100, &far, 0, 1.0);
        assert_eq!(
            (far_pt.x - near_pt.x, far_pt.y - near_pt.y),
            (20, 20),
            "the margin is measured from the work-area edge, applied exactly once"
        );
    }

    #[test]
    fn compact_layout_shares_the_work_area_aware_placement() {
        // Compact and Expanded never pick a different edge to anchor to: both
        // flow through the same `placement` against the resolved work area, so a
        // compact pill rests on the same work-area edge as its expanded twin.
        let work = RECT {
            left: 80,
            top: 40,
            right: 1840,
            bottom: 1040,
        };
        let expanded = anchor_pos(None, None); // Bottom/Center, margin 8
        let compact = OverlayPos {
            vertical: VerticalPosition::Bottom,
            horizontal: HorizontalPosition::Left,
            margin: 4,
            ..anchor_pos(None, None)
        };
        let expanded_pt = placement(work, 600, 120, &expanded, 0, 1.0);
        let compact_pt = placement(work, 400, 100, &compact, 0, 1.0);
        assert_eq!(
            expanded_pt.y + 120,
            work.bottom - 8,
            "expanded pill bottom rests on the work-area bottom"
        );
        assert_eq!(
            compact_pt.y + 100,
            work.bottom - 4,
            "compact pill bottom rests on the work-area bottom too"
        );
    }

    #[test]
    fn resolve_target_uses_the_selected_monitor_work_area_not_the_primary() {
        // Two monitors with distinct, asymmetric work areas: the primary carries
        // a bottom taskbar, the secondary a left taskbar and a negative virtual-
        // screen origin. The resolved target must surface the *selected*
        // monitor's work area, never the primary's.
        // The handles are arbitrary (Primary/Index selection never compares
        // handles), so reuse the existing `fake_display` helper rather than
        // casting integer literals to raw pointers.
        let mut primary = fake_display(1, true);
        primary.work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let mut secondary = fake_display(2, false);
        secondary.work = RECT {
            left: -320,
            top: 0,
            right: 1840,
            bottom: 1080,
        };
        let displays = vec![primary, secondary];
        // Primary mode resolves the primary display and its work area.
        let primary_index = resolve_target(MonitorMode::Primary, &displays, None).unwrap();
        assert_eq!(primary_index, 0);
        assert_eq!(
            displays[primary_index].work,
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040
            },
        );
        // Explicit index resolves THAT display's work area (the negative-origin one).
        let secondary_index = resolve_target(MonitorMode::Index(1), &displays, None).unwrap();
        assert_eq!(secondary_index, 1);
        assert_eq!(
            displays[secondary_index].work,
            RECT {
                left: -320,
                top: 0,
                right: 1840,
                bottom: 1080
            },
        );
        // The chosen monitor's work area is the secondary's, not the primary's.
        assert_ne!(displays[secondary_index].work, displays[primary_index].work);
    }

    #[test]
    fn rect_covers_monitor_detects_genuine_fullscreen_only() {
        // A monitor's full physical bounds.
        let monitor_rc = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        // A maximized window covers the work area (inset here by a 40px bottom
        // taskbar) but NOT the full monitor, so it is not fullscreen-on-target.
        let maximized = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        assert!(
            !rect_covers_monitor(&maximized, &monitor_rc),
            "maximized covers rcWork, not rcMonitor"
        );
        // A genuine fullscreen window covers the full monitor.
        assert!(
            rect_covers_monitor(&monitor_rc, &monitor_rc),
            "fullscreen covers rcMonitor"
        );
        // A normal (smaller) window does not.
        let normal = RECT {
            left: 100,
            top: 100,
            right: 900,
            bottom: 700,
        };
        assert!(!rect_covers_monitor(&normal, &monitor_rc));
        // An empty/ambiguous rect does not.
        let empty = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        assert!(!rect_covers_monitor(&empty, &monitor_rc));
    }

    #[test]
    fn rect_covers_monitor_is_relative_to_the_target_monitor() {
        // Foreground is fullscreen on monitor A (covers A entirely) but the
        // target is monitor B: it must not read as fullscreen-on-target.
        let monitor_a = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let monitor_b = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        assert!(
            rect_covers_monitor(&monitor_a, &monitor_a),
            "foreground is fullscreen on A"
        );
        assert!(
            !rect_covers_monitor(&monitor_a, &monitor_b),
            "fullscreen on another monitor does not override the target work area"
        );
    }

    #[test]
    fn effective_position_rect_collapses_to_rcmonitor_only_when_fullscreen() {
        let monitor_rc = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let work_rc = RECT {
            left: 0,
            top: 40,
            right: 1920,
            bottom: 1080,
        }; // top taskbar
        assert_eq!(
            effective_position_rect(monitor_rc, work_rc, false),
            work_rc,
            "no fullscreen -> work area"
        );
        assert_eq!(
            effective_position_rect(monitor_rc, work_rc, true),
            monitor_rc,
            "fullscreen -> physical monitor"
        );
    }

    #[test]
    fn fullscreen_positioning_uses_the_physical_edge_with_no_stale_taskbar_gap() {
        // A top taskbar insets rcWork.top, but a genuine fullscreen foreground
        // on the target monitor collapses the effective rect to rcMonitor, so a
        // top anchor drops to the physical edge instead of leaving the taskbar
        // gap. Non-fullscreen keeps the work-area inset.
        let monitor_rc = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let work_rc = RECT {
            left: 0,
            top: 40,
            right: 1920,
            bottom: 1080,
        };
        let top_left = OverlayPos {
            vertical: VerticalPosition::Top,
            horizontal: HorizontalPosition::Left,
            ..anchor_pos(None, None)
        };
        let fullscreen_pt = placement(
            effective_position_rect(monitor_rc, work_rc, true),
            400,
            100,
            &top_left,
            0,
            1.0,
        );
        assert_eq!(
            fullscreen_pt,
            POINT { x: 8, y: 8 },
            "fullscreen drops the work-area top inset"
        );
        let normal_pt = placement(
            effective_position_rect(monitor_rc, work_rc, false),
            400,
            100,
            &top_left,
            0,
            1.0,
        );
        assert_eq!(
            normal_pt,
            POINT { x: 8, y: 48 },
            "non-fullscreen respects the work-area top inset"
        );
    }

    #[test]
    fn anchor_unchanged_skips_when_nothing_moved_and_renders_on_flip_or_change() {
        let edge = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        // First resolve has no previous anchor: never skip.
        assert!(!anchor_unchanged(None, edge, false));
        // Same anchor, no layout flip: skip (Alt-Tab between two normal apps).
        assert!(anchor_unchanged(Some(edge), edge, false));
        // Same anchor but Auto layout flipped: do not skip (size/contents
        // changed, e.g. fullscreen->normal flipping Auto Compact->Expanded).
        assert!(!anchor_unchanged(Some(edge), edge, true));
        // Anchor moved (rcWork<->rcMonitor, e.g. fullscreen enter/leave on the
        // target monitor): do not skip even with no layout flip.
        let moved = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(!anchor_unchanged(Some(edge), moved, false));
    }

    #[test]
    fn estimate_position_advances_and_clamps() {
        // base + elapsed * rate, never negative.
        assert_eq!(OverlayState::estimate_position(60.0, 1.0, 5.0), 65.0);
        assert_eq!(OverlayState::estimate_position(0.0, 2.0, 3.0), 6.0);
        assert_eq!(OverlayState::estimate_position(2.0, 1.0, -5.0), 0.0);
    }

    #[test]
    fn progress_changed_rebases_the_bar_without_becoming_content() {
        let config = Config::default();
        let queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
        let mut state = OverlayState::new(config, queue.clone());
        // Seed the bar with a track at position 10/120s.
        let track = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            source_app: "spotify".into(),
            duration_secs: Some(120),
            position_secs: Some(10.0),
            playback_rate: Some(1.0),
            ..TrackInfo::default()
        };
        state.content = Some(MediaEvent::TrackChanged(track.clone()));
        state.apply_track_progress(&track);
        assert_eq!(state.estimated_position_secs, Some(10.0));

        // A seek to 90s arrives as a ProgressChanged (no TrackChanged,
        // because the song identity is unchanged): the bar must jump now,
        // not wait for a content re-emit.
        queue.lock().unwrap().push_back(Arc::new(MediaEvent::ProgressChanged {
            source_app: "spotify".into(),
            position_secs: Some(90.0),
            duration_secs: Some(120),
            playback_rate: Some(1.0),
        }));
        state.receive_events();

        // The position updated to the seeked point...
        assert_eq!(state.estimated_position_secs, Some(90.0));
        // ...duration was carried through...
        assert_eq!(state.progress_duration_secs, Some(120));
        // ...and content is still the original TrackChanged (ProgressChanged
        // is a data update, never a notification).
        assert!(matches!(state.content, Some(MediaEvent::TrackChanged(t)) if t.title == "Song"));
        assert_eq!(state.pending.len(), 0, "no pill queued for a progress update");
    }

    #[test]
    fn progress_from_foreign_source_is_ignored() {
        let config = Config::default();
        let queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
        let mut state = OverlayState::new(config, queue.clone());
        // Seed the bar with a spotify track at position 10/120s.
        let track = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            source_app: "spotify".into(),
            duration_secs: Some(120),
            position_secs: Some(10.0),
            playback_rate: Some(1.0),
            ..TrackInfo::default()
        };
        state.content = Some(MediaEvent::TrackChanged(track.clone()));
        state.apply_track_progress(&track);
        assert_eq!(state.estimated_position_secs, Some(10.0));

        // A Brave timeline refresh must not re-anchor the bar under spotify's
        // pill: each source pushes a progress update every ~2s, so without the
        // source gate the seekbar would follow whatever session is advancing.
        queue.lock().unwrap().push_back(Arc::new(MediaEvent::ProgressChanged {
            source_app: "brave".into(),
            position_secs: Some(90.0),
            duration_secs: Some(300),
            playback_rate: Some(1.0),
        }));
        state.receive_events();

        // The foreign sample was dropped entirely: position and duration are
        // untouched, and nothing was queued.
        assert_eq!(state.estimated_position_secs, Some(10.0));
        assert_eq!(state.progress_duration_secs, Some(120));
        assert_eq!(state.pending.len(), 0, "no pill queued for a progress update");
    }

    #[test]
    fn progress_changed_does_not_snap_the_bar_backward() {
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        // The bar has been interpolating forward from its last anchor and now
        // displays 50.0s.
        state.estimated_position_secs = Some(50.0);
        state.progress_anchor = Some((Instant::now(), 50.0));
        state.progress_playing = true;
        state.progress_rate = Some(1.0);
        state.progress_duration_secs = Some(120);

        // A sample behind the display but within PROGRESS_LATENCY_TOL_SECS must
        // NOT snap the bar backward — that jitter is what made it oscillate.
        // 49.5s is within tolerance of 50.0s, so the displayed position is kept
        // and last_progress_position_secs is updated (the sample is fresh).
        state.apply_progress(Some(49.5), Some(120), Some(1.0));
        assert_eq!(
            state.estimated_position_secs,
            Some(50.0),
            "latency jitter must not snap the bar backward"
        );

        // A large backward jump is a new track starting near 0 (or a backward seek):
        // it must be adopted so the bar reflects the real track instead of sitting
        // at the old track's position.
        state.apply_progress(Some(0.0), Some(120), Some(1.0));
        assert_eq!(
            state.estimated_position_secs,
            Some(0.0),
            "a track change must be adopted, not kept at the old position"
        );

        // A forward sample is always adopted (forward seek or catch-up).
        state.apply_progress(Some(52.0), Some(120), Some(1.0));
        assert_eq!(state.estimated_position_secs, Some(52.0));
    }

    #[test]
    fn progress_changed_skips_stale_sm_sample() {
        // Apps that refresh SMTC position every few seconds can return the same
        // value on consecutive polls. The bar must keep interpolating instead of
        // snapping backward to the stale value (which exceeds the 3 s tolerance
        // after ~4 s of playback at 1x).
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        // Bar has interpolated to 14.0s (4s ahead of the last fresh SMTC sample).
        state.estimated_position_secs = Some(14.0);
        state.progress_anchor = Some((Instant::now(), 14.0));
        state.progress_playing = true;
        state.progress_rate = Some(1.0);
        state.progress_duration_secs = Some(120);
        state.last_progress_position_secs = Some(10.0);

        // Same stale position arrives again: must NOT snap backward.
        state.apply_progress(Some(10.0), Some(120), Some(1.0));
        assert_eq!(state.estimated_position_secs, Some(14.0));
        assert_eq!(
            state.progress_anchor.unwrap().1,
            14.0,
            "stale sample must not re-anchor to the stale position"
        );

        // A fresh sample beyond tolerance is still adopted (genuine seek/new track),
        // even though it is behind the displayed position.
        state.apply_progress(Some(5.0), Some(120), Some(1.0));
        assert_eq!(state.estimated_position_secs, Some(5.0));
    }

    #[test]
    fn absent_progress_position_clears_the_stale_estimate() {
        // A source that stops reporting a position must not leave the old
        // estimate or its interpolating anchor behind: the bar would otherwise
        // keep crawling from a stale base.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        state.estimated_position_secs = Some(50.0);
        state.progress_anchor = Some((Instant::now(), 50.0));
        state.last_progress_position_secs = Some(50.0);
        state.progress_rate = Some(1.0);
        state.progress_duration_secs = Some(120);

        state.apply_progress(None, Some(120), Some(1.0));
        assert_eq!(state.estimated_position_secs, None);
        assert_eq!(state.progress_anchor, None);
        assert_eq!(state.last_progress_position_secs, None);
    }

    #[test]
    fn track_without_position_sets_no_anchor() {
        // A track arriving without a reported position must not anchor at a
        // fabricated 0.0: the tick would crawl the bar from the start of the
        // song on data we never received.
        let mut state = OverlayState::new(Config::default(), EventQueue::default());
        let track = TrackInfo {
            title: "Song".into(),
            artist: "Artist".into(),
            source_app: "spotify".into(),
            duration_secs: Some(120),
            position_secs: None,
            ..TrackInfo::default()
        };
        state.apply_track_progress(&track);
        assert_eq!(state.estimated_position_secs, None);
        assert_eq!(
            state.progress_anchor, None,
            "no position, no anchor: nothing to interpolate from"
        );
    }

    #[test]
    fn newer_same_source_track_swaps_in_place_while_showing() {
        let config = Config::default();
        let queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
        let mut state = OverlayState::new(config, queue.clone());
        state.layout = LayoutMode::Expanded;
        state.phase = Phase::Shown;
        // Pill is up showing Apologize (Spotify).
        state.content = Some(MediaEvent::TrackChanged(track_for("spotify", "Apologize", "Coldplay")));
        state.render();

        // Skip to Payphone on the SAME source while the pill is visible.
        queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "spotify", "Payphone", "Maroon 5",
            ))));
        state.receive_events();

        // The newer same-source track swaps the visible content in place instead
        // of queueing behind it, so the pill shows the current track, not Apologize.
        assert!(
            matches!(state.content, Some(MediaEvent::TrackChanged(t)) if t.title == "Payphone"),
            "must show Payphone, the current track"
        );
        assert!(
            state.pending.is_empty(),
            "a same-source swap must not enqueue behind the visible pill"
        );
    }

    #[test]
    fn cross_source_track_still_enqueues_behind_the_visible_pill() {
        let config = Config::default();
        let queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
        let mut state = OverlayState::new(config, queue.clone());
        state.layout = LayoutMode::Expanded;
        state.phase = Phase::Shown;
        state.content = Some(MediaEvent::TrackChanged(track_for("spotify", "Apologize", "Coldplay")));
        state.render();

        // Skip to a different source (YouTube Music) while the pill is up.
        queue
            .lock()
            .unwrap()
            .push_back(Arc::new(MediaEvent::TrackChanged(track_for(
                "youtube-music",
                "Payphone",
                "Maroon 5",
            ))));
        state.receive_events();

        // Cross-source still queues; the visible pill keeps showing Apologize.
        assert!(
            matches!(state.content, Some(MediaEvent::TrackChanged(t)) if t.title == "Apologize"),
            "cross-source must not swap the visible pill in place"
        );
        assert_eq!(state.pending.len(), 1, "cross-source track must be enqueued");
    }
}
