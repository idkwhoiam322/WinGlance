//! Morph springs, hover decisions and pill geometry.

use super::{BOUNCE_OVER, BOUNCE_UNDER, COLLAPSE_TROUGH, EXPAND_SPRING_PEAK, LEAVE_DEBOUNCE, MORPH_LAG, ROW_HEIGHT};
use crate::config::Config;
use crate::events::MediaEvent;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::RECT;

/// Per-axis morph progress, 0 = compact, 1 = expanded. The width axis is
/// the leader and the height axis chases it with `MORPH_LAG` of delay, so
/// the card widens before it grows tall — the axis-led island motion
/// iOS/ColorOS use. Each axis runs the same spring curve; the height axis
/// is the width curve delayed and compressed into the rest of the leg (see
/// `lag_progress`), so it always trails the width and pins at its endpoint
/// exactly when the leg ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct MorphProgress {
    pub(super) width: f32,
    pub(super) height: f32,
}

/// Which way the in-place hover morph is going.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MorphDirection {
    Expand,
    Collapse,
}

/// The in-place compact→expanded hover morph (see `expand_compact_on_hover`).
/// Render sub-state only: the pill's `layout` stays Compact and its position
/// stays the compact anchor for the whole morph, so the pill grows in place
/// instead of jumping to the expanded position. `Phase` stays `Shown`; the
/// size lerp is the animation, with the expanded draw clipped to the growing
/// window. `done` pins the pill at the expanded size until the cursor leaves.
pub(super) struct HoverExpand {
    /// When the current leg (expand or collapse) started.
    pub(super) start: Instant,
    /// Which way the leg goes; a leave mid-expand flips it to `Collapse`.
    pub(super) direction: MorphDirection,
    /// Size progress at `start`: 0 at a fresh expand, the current progress at
    /// a reversal, so the collapse leg continues from the size it reversed at.
    pub(super) from: f32,
    /// The collapse leg's initial velocity (progress per collapse leg),
    /// seeded from the expand leg's velocity at the reversal (see
    /// `reversal_seed`), so the return continues the running motion instead
    /// of kinking to a fresh ease. 0 for a fresh expand and for a release
    /// from the pinned-expanded state.
    pub(super) velocity: f32,
    /// The expand leg reached its end; the pill renders expanded until the
    /// cursor leaves (which clears the state) or the pill dismisses.
    pub(super) done: bool,
}

/// The per-tick hover input snapshot, window-free so the decision below is
/// unit-testable. `tick` samples the real cursor over the real pill and
/// maps the state into this.
#[derive(Clone, Copy)]
pub(super) struct HoverTick {
    pub(super) cursor_over: bool,
    /// A morph leg is in flight.
    pub(super) morphing: bool,
    /// The in-flight leg is the expand leg (vs. a collapse leg).
    pub(super) morph_expanding: bool,
    /// The one-way hover dismiss is already armed.
    pub(super) dismiss_armed: bool,
}

/// What the tick does about the hover this tick. Pure — the caller applies
/// the step to its state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum HoverStep {
    /// Start the in-place expand morph; the dismissal clock resets to the
    /// full duration.
    StartExpand,
    /// Arm the one-way 500 ms hover dismiss.
    ArmDismiss,
    /// Reverse the expand leg (the cursor left): mid-morph it turns around
    /// from the current progress, after the expansion finished it releases
    /// from the pinned-expanded state — both run the collapse leg back to
    /// compact.
    ReverseMorph,
    /// Nothing to do.
    None,
}

/// Decides the tick's hover handling from the pure snapshot. Hovering follows
/// the pill's *effective* layout (see `expanded`): an Expanded-layout pill
/// arms the one-way 500 ms dismiss only while `dismiss_on_hover` is enabled,
/// and a Compact-layout pill expands on hover while `expand_compact_on_hover`
/// is enabled (falling back to the Expanded rules otherwise) — the first
/// hover of a showing expands, and with `dismiss_on_hover` enabled later
/// hovers dismiss instead (the second hover dismisses), while without it
/// every hover re-expands. The compact→expanded morph itself is an
/// interaction: while it is in flight or pinned, hovering never arms
/// anything — the expanded state is held (see `held` in `tick`), so it is
/// never dismissed mid-read.
pub(super) fn hover_step(
    hover: HoverTick,
    dismiss_on_hover: bool,
    expand_compact_on_hover: bool,
    expanded_once: bool,
    expanded: bool,
) -> HoverStep {
    if hover.morphing {
        // A collapse leg always runs to completion on its own.
        if !hover.morph_expanding {
            return HoverStep::None;
        }
        if hover.cursor_over {
            return HoverStep::None;
        }
        // Leaving — mid-morph or after the pin — always runs the collapse
        // leg back to compact: the release from the pinned state passes
        // `from = 1.0, velocity ≈ 0`, so it settles without a bounce.
        return HoverStep::ReverseMorph;
    }
    if !hover.cursor_over {
        return HoverStep::None;
    }
    if expanded {
        // A laid-out expanded pill: hover arms the dismiss only while
        // dismiss-on-hover is enabled. The morph-origin expanded state never
        // reaches this arm — the morphing branch above holds it.
        if dismiss_on_hover && !hover.dismiss_armed {
            HoverStep::ArmDismiss
        } else {
            HoverStep::None
        }
    } else if expand_compact_on_hover && (!dismiss_on_hover || !expanded_once) {
        // The first hover over a Compact pill expands it; with
        // dismiss-on-hover enabled a later hover dismisses instead (the
        // second hover), while without it every hover re-expands.
        HoverStep::StartExpand
    } else if dismiss_on_hover && !hover.dismiss_armed {
        HoverStep::ArmDismiss
    } else {
        HoverStep::None
    }
}

/// Logical (96-DPI) size of a pill for the given content. Single source of
/// truth shared by `render()` and `content_size()` so they cannot drift.
/// `compact` selects the compact pill geometry (one title row, trailing app
/// icon and playback symbol) over the expanded four-row layout.
pub(super) fn content_size_of(config: &Config, content: &MediaEvent, compact: bool) -> (f32, f32) {
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

/// The whole-pill scale factor of the settle-bounce, as a pure function of
/// the leg's progress — there is no appended phase, so the bounce starts the
/// instant the size completes and there is never a still pause before it.
/// Exactly 1.0 whenever the spring is inside its endpoints (and again at the
/// pinned end), so the window and content hand off to the steady frame at
/// the final size. The expand rides the spring's own overshoot past 1.0,
/// normalized to peak at (1 + `BOUNCE_OVER`) when the spring peaks. The
/// compaction dips to (1 - `BOUNCE_UNDER`) at the spring's undershoot
/// trough and recovers straight to exactly 1.0 when the spring pins — the
/// shrink-below-minimum return, with no over-bounce past the final size.
pub(super) fn bounce_scale(progress: MorphProgress, direction: MorphDirection) -> f32 {
    match direction {
        MorphDirection::Expand => {
            let excess = ((progress.width - 1.0) / (EXPAND_SPRING_PEAK - 1.0)).clamp(0.0, 1.0);
            1.0 + BOUNCE_OVER * excess
        }
        MorphDirection::Collapse => {
            let dip = if progress.width < 0.0 {
                (progress.width / COLLAPSE_TROUGH).clamp(0.0, 1.0)
            } else {
                0.0
            };
            // The dip rides the spring's undershoot straight back to 1.0 —
            // the pill shrinks below the compact minimum and returns, with
            // no over-bounce past the steady size. Exactly 1.0 when the
            // spring pins, so there is no seam at the steady handoff.
            1.0 - BOUNCE_UNDER * dip
        }
    }
}

/// Duration of one hover-morph leg. The expand leg gets the full animation
/// duration — room for the spring to play out — while the collapse leg runs
/// shorter at 4/5: still a confident close, but long enough that the
/// undershoot tail (the settle-bounce) has time to read. Shared by the
/// completion check in `tick` and the progress curve, so a leg always
/// settles exactly when its animation is done. The settle-bounce is not
/// appended: it rides the spring's own overshoot/undershoot (see
/// `bounce_scale`), so the leg duration is the spring duration.
pub(super) fn morph_duration(config: &Config, direction: MorphDirection) -> Duration {
    match direction {
        MorphDirection::Expand => animation_duration(config),
        MorphDirection::Collapse => Duration::from_millis((animation_duration(config).as_millis() * 4 / 5) as u64),
    }
}

/// Current hover-morph progress, per axis (see `MorphProgress`): 0 =
/// compact, 1 = expanded. The expand leg runs the springy `spring_expand`
/// curve on the leading width axis — it may pass 1.0 mid-flight, which
/// `morph_size`'s geometry clamp contains, so the bounce reads as a quick
/// settle without the pill ever exceeding the expanded size — while the
/// height chases it with `MORPH_LAG` of delay. The collapse leg is the
/// mirrored release spring: the width starts from the progress it reversed
/// at, seeded with the expand leg's velocity there (see `reversal_seed`),
/// and the height continues the same motion delayed, so nothing kinks — the
/// pill may travel a little farther before turning — and both axes settle
/// exactly at compact.
pub(super) fn hover_progress(morph: &HoverExpand, config: &Config) -> MorphProgress {
    let total = morph_duration(config, morph.direction).as_secs_f32();
    let t = (morph.start.elapsed().as_secs_f32() / total).clamp(0.0, 1.0);
    match morph.direction {
        MorphDirection::Expand => MorphProgress {
            width: spring_expand(t),
            height: lagged_expand(&EXPAND_SPRING, t, MORPH_LAG),
        },
        MorphDirection::Collapse => MorphProgress {
            width: spring_collapse(t, morph.from, morph.velocity),
            height: lagged_collapse(t, MORPH_LAG, morph.from, morph.velocity),
        },
    }
}

/// The follower axis's local time in a lagged chase: the leader's curve,
/// delayed by `lag` and compressed into the remaining leg. The follower
/// therefore always trails the leader's curve value (its local time stays
/// behind) yet still reaches its own pinned endpoint exactly when the leg
/// ends — a plain time shift would leave the follower a hair short.
pub(super) fn lag_progress(t: f32, lag: f32) -> f32 {
    ((t - lag) / (1.0 - lag)).clamp(0.0, 1.0)
}

/// The follower axis of an expand: the leader's spring curve evaluated at
/// the delayed, compressed local time — the same curve, started `lag` into
/// the leg from rest, so the height begins growing a beat after the width
/// and never overtakes it.
pub(super) fn lagged_expand(spring: &Spring, t: f32, lag: f32) -> f32 {
    spring.value_at(lag_progress(t, lag), 0.0, 0.0)
}

/// The follower axis of a collapse: the mirrored release curve delayed by
/// `lag` and compressed into the remaining leg. The seed velocity is scaled
/// by (1 − lag) so the follower's physical (per-second) motion at its start
/// matches the leader's seed exactly — the height lingers, then continues
/// the collapse at the same speed the width began it, and both pin at
/// compact when the leg ends.
pub(super) fn lagged_collapse(t: f32, lag: f32, from: f32, velocity: f32) -> f32 {
    1.0 - COLLAPSE_SPRING.value_at(lag_progress(t, lag), 1.0 - from, -velocity * (1.0 - lag))
}

/// The reversal seed: the expand leg's progress and velocity at the reversal
/// moment, the velocity converted to collapse-leg units so the absolute
/// (per-second) motion is unchanged across the flip. The collapse leg then
/// continues that exact motion instead of kinking to a fresh ease. The clock
/// is passed in so the seed is a pure function of the state (callers tick at
/// a fixed `now`, and tests get exact determinism).
pub(super) fn reversal_seed(morph: &HoverExpand, config: &Config, now: Instant) -> (f32, f32) {
    let expand_leg = morph_duration(config, MorphDirection::Expand).as_secs_f32();
    let collapse_leg = morph_duration(config, MorphDirection::Collapse).as_secs_f32();
    let t = (now.duration_since(morph.start).as_secs_f32() / expand_leg).clamp(0.0, 1.0);
    let from = spring_expand(t);
    let velocity = EXPAND_SPRING.velocity_at(t, 0.0, 0.0) * collapse_leg / expand_leg;
    (from, velocity)
}

/// Whether a hover input counts as "over" this tick: the raw cursor state
/// plus the leave-debounce window that keeps boundary jitter from cancelling
/// a fresh morph the moment it starts.
pub(super) fn hover_engaged(cursor_over: bool, left_at: Option<Instant>, now: Instant) -> bool {
    cursor_over || left_at.is_some_and(|left| now.duration_since(left) < LEAVE_DEBOUNCE)
}

/// The logical pill size at a morph progress: each dimension lerps between
/// the compact and expanded sizes on its own axis's progress, clamped so the
/// eased spring can never overshoot past either endpoint (and so a reversal
/// continues from exactly the size it reversed at).
pub(super) fn morph_size(config: &Config, content: &MediaEvent, progress: MorphProgress) -> (f32, f32) {
    let (compact_w, compact_h) = content_size_of(config, content, true);
    let (expanded_w, expanded_h) = content_size_of(config, content, false);
    let width = compact_w + (expanded_w - compact_w) * progress.width.clamp(0.0, 1.0);
    let height = compact_h + (expanded_h - compact_h) * progress.height.clamp(0.0, 1.0);
    (
        width.clamp(compact_w.min(expanded_w), compact_w.max(expanded_w)),
        height.clamp(compact_h.min(expanded_h), compact_h.max(expanded_h)),
    )
}

/// The pill's corner radius during a morph: the compact and expanded radii
/// lerped by the leading (width) axis's progress, so the corner curvature
/// follows the silhouette the eye is tracking. Clamped between both
/// endpoints; the appended settle-bounce scales the rendered frame as a
/// whole (`render_layered`), so the corners ride it without this lerp
/// overshooting.
pub(super) fn morph_radius(compact_radius: f32, expanded_radius: f32, progress: MorphProgress) -> f32 {
    let p = progress.width.clamp(0.0, 1.0);
    compact_radius + (expanded_radius - compact_radius) * p
}

/// The compact-exclusive content's opacity during a morph — the inline app
/// icon — keyed to the shape progress, the LESS-advanced of the two axes
/// (see `draw_text_pixels`): it holds fully visible only very briefly, then
/// dissolves out over 0.05..0.20, so it is completely gone BEFORE the
/// expanded extra rows start arriving (0.25). The windows are deliberately
/// disjoint: the two exclusive element groups must never coexist, or the
/// compact icon would visibly sit beside the expanding app row.
pub(super) fn compact_alpha(shape_progress: f32) -> f32 {
    const HOLD_END: f32 = 0.05;
    const FADE_OUT_END: f32 = 0.20;
    1.0 - ease_out_quint(((shape_progress - HOLD_END) / (FADE_OUT_END - HOLD_END)).clamp(0.0, 1.0))
}

/// The expanded-exclusive content's opacity during a morph — the extra rows
/// (artist, meta, app) — keyed to the shape progress, the less-advanced of
/// the two axes: on expand that is the lagging height, so the rows arrive
/// only after the pill has grown tall enough to show them; on collapse it is
/// the leading width, so the rows leave as the pill narrows. The fade window
/// (0.25 to 0.60) starts only where `compact_alpha`'s has ended — the icon
/// is gone before the rows appear, so the two never blend. The shared
/// elements (title, symbol, art) never fade: they travel (`draw_morph_content`).
pub(super) fn expanded_alpha(shape_progress: f32) -> f32 {
    const FADE_IN_START: f32 = 0.25;
    const FADE_IN_END: f32 = 0.60;
    ease_out_quint(((shape_progress - FADE_IN_START) / (FADE_IN_END - FADE_IN_START)).clamp(0.0, 1.0))
}

/// Scales a color's alpha by `factor`, the per-pass opacity of the morph's
/// content fade: the content primitives all derive their final alpha
/// from the color's alpha channel, so dimming the color at the call site
/// fades the whole element (glyphs, symbols, placeholder art) without
/// touching the primitives.
pub(super) fn dim_color(color: [u8; 4], factor: f32) -> [u8; 4] {
    let factor = factor.clamp(0.0, 1.0);
    [color[0], color[1], color[2], (color[3] as f32 * factor).round() as u8]
}

/// A text row's opacity from the morph's reveal edge: the row's band ends at
/// `band_bottom` (buffer coords), the pill's animated bottom edge is at
/// `body_bottom`, and its rest position at `rest_body_bottom`. The row is
/// drawn only once the edge has passed its band bottom (so no text ever
/// renders outside the pill body), and fades in over the remaining sweep to
/// the rest position — full opacity exactly at rest, and the same window
/// fades the row back out as the edge returns. `band_bottom` is guaranteed
/// to sit strictly above `rest_body_bottom` by the pill's constant-height
/// layout (the `+ 8` slack in `content_size`); the guard keeps a band that
/// somehow reaches the rest edge fully visible instead of invisible.
pub(super) fn row_unveil_alpha(body_bottom: i32, rest_body_bottom: i32, band_bottom: i32) -> f32 {
    if band_bottom >= rest_body_bottom {
        return 1.0;
    }
    ((body_bottom - band_bottom) as f32 / (rest_body_bottom - band_bottom) as f32).clamp(0.0, 1.0)
}

/// Logical (96-DPI) size of a pill: the configured max width and a constant
/// height that always reserves all four row bands (title, artist, meta,
/// source). A missing row leaves empty space at the bottom instead of
/// shrinking the pill, so every pill — track change, state change, any
/// source — is exactly the same size. Single source of truth used by both
/// `render()` and `content_size()` so they cannot drift.
pub(super) fn content_size(config: &Config) -> (f32, f32) {
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
pub(super) struct CompactMetrics {
    /// Art tile side length.
    pub(super) art: f32,
    /// App icon side length.
    pub(super) icon: f32,
    /// Playback symbol box size.
    pub(super) symbol: f32,
}

pub(super) fn compact_metrics(config: &Config) -> CompactMetrics {
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
pub(super) fn compact_size(config: &Config) -> (f32, f32) {
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
pub(super) fn compact_title_viewport(config: &Config) -> (f32, f32) {
    let metrics = compact_metrics(config);
    let appearance = &config.appearance;
    let (pill_w, _) = compact_size(config);
    let left = appearance.padding + metrics.art + 12.0;
    let right = pill_w - appearance.padding - metrics.symbol - 16.0 - metrics.icon - 6.0;
    (left, right)
}

/// f32 lerp on an i32 edge, rounded to the pixel: `a + (b - a) * t`.
pub(super) fn lerp_edge(a: i32, b: i32, t: f32) -> i32 {
    (a as f32 + (b as f32 - a as f32) * t).round() as i32
}

/// The morph artwork tile: the side length lerps between the compact and
/// expanded art sizes on the shape progress while the tile stays vertically
/// centered in the current body. At either endpoint the frame's body has the
/// matching layout height, so the tile lands exactly on the steady compact
/// tile (shape 0) or the steady expanded tile (shape 1) at 1.0 DPI; in
/// between it grows and slides toward the body's center with the card.
/// `pill_h` is the current body height (animated window height minus the
/// aura insets).
pub(super) fn morph_art_tile(config: &Config, inset: i32, pill_h: i32, scale: f32, shape: f32) -> (i32, i32, i32) {
    let appearance = &config.appearance;
    let padding = (appearance.padding * scale).round() as i32;
    let compact = compact_metrics(config).art * scale;
    let expanded = appearance.art_size as f32 * scale;
    let size = (compact + (expanded - compact) * shape).round() as i32;
    (inset + padding, inset + (pill_h - size) / 2, size)
}

/// The art tile's edge gate during a morph: full opacity while the tile fits
/// inside the current body, fading proportionally to the cut only when the
/// body edge passes through it. The tile grows with the body, so in practice
/// the gate never bites — unlike `row_unveil_alpha`, it is not normalized
/// against the rest height, because the tile must not fade while it simply
/// waits for the body to finish growing.
pub(super) fn art_edge_gate(body_bottom: i32, art_y: i32, art_size: i32) -> f32 {
    ((body_bottom - art_y) as f32 / art_size as f32).clamp(0.0, 1.0)
}

/// The morph title band: the compact band (right of the small art,
/// vertically centered in the compact row) travels to the expanded title row
/// (right of the big art, top-packed and narrowed for the symbol slot),
/// each edge on its own axis's progress. The compact end is pinned to the
/// compact body, so the title stays put while the window grows and only
/// starts traveling as the progress advances; the expanded end matches the
/// steady expanded band exactly.
pub(super) fn morph_title_band(config: &Config, inset: i32, width: i32, scale: f32, progress: MorphProgress) -> RECT {
    let appearance = &config.appearance;
    let padding = (appearance.padding * scale).round() as i32;
    let row_h = (appearance.font_size_title * ROW_HEIGHT * scale).round() as i32;
    let art = (appearance.art_size as f32 * scale).round() as i32;
    let symbol = (compact_metrics(config).symbol * scale).round() as i32;
    let label_w = symbol + (16.0 * scale).round() as i32;
    let (vp_left, vp_right) = compact_title_viewport(config);
    let compact_h = (compact_size(config).1 * scale).round() as i32;
    let compact = RECT {
        left: inset + (vp_left * scale).round() as i32,
        top: inset + (compact_h - row_h) / 2,
        right: inset + (vp_right * scale).round() as i32,
        bottom: inset + (compact_h - row_h) / 2 + row_h,
    };
    let expanded = RECT {
        left: inset + padding + art + (12.0 * scale).round() as i32,
        top: inset + padding,
        right: width - inset - padding - label_w,
        bottom: inset + padding + row_h,
    };
    RECT {
        left: lerp_edge(compact.left, expanded.left, progress.width),
        top: lerp_edge(compact.top, expanded.top, progress.height),
        right: lerp_edge(compact.right, expanded.right, progress.width),
        bottom: lerp_edge(compact.bottom, expanded.bottom, progress.height),
    }
}

/// The morph playback symbol: the compact trailing-chain position (right of
/// the app icon, vertically centered in the compact row) travels to the
/// expanded title row's right slot — the same right edge the steady
/// expanded symbol uses (`title_rect.right`; the `label_w` narrowing applies
/// to the title text, not the symbol). Both layouts draw the same symbol
/// size, so only the position travels.
pub(super) fn morph_symbol_pos(
    config: &Config,
    inset: i32,
    width: i32,
    scale: f32,
    progress: MorphProgress,
) -> (i32, i32, f32) {
    let appearance = &config.appearance;
    let padding = (appearance.padding * scale).round() as i32;
    let symbol = (compact_metrics(config).symbol * scale).round() as i32;
    let compact_h = (compact_size(config).1 * scale).round() as i32;
    let (_, vp_right) = compact_title_viewport(config);
    let viewport_right = inset + (vp_right * scale).round() as i32;
    let gap = (6.0 * scale).round() as i32;
    let icon = (16.0 * scale).round() as i32;
    let symbol_gap = (16.0 * scale).round() as i32;
    let compact_right = viewport_right + gap + icon + symbol_gap + symbol;
    let compact_y = inset + (compact_h - symbol) / 2;
    let expanded_right = width - inset - padding;
    let expanded_y = inset + padding;
    (
        lerp_edge(compact_right, expanded_right, progress.width),
        lerp_edge(compact_y, expanded_y, progress.height),
        symbol as f32,
    )
}

/// The morph app icon's position: the compact trailing chain, pinned to the
/// compact body (the icon only exists in the compact layout and dissolves
/// out before the pill has grown much).
pub(super) fn morph_icon_pos(config: &Config, inset: i32, scale: f32) -> (i32, i32, i32) {
    let (_, vp_right) = compact_title_viewport(config);
    let viewport_right = inset + (vp_right * scale).round() as i32;
    let gap = (6.0 * scale).round() as i32;
    let icon = (16.0 * scale).round() as i32;
    let compact_h = (compact_size(config).1 * scale).round() as i32;
    let x = viewport_right + gap;
    let y = inset + (compact_h - icon) / 2;
    (x, y, icon)
}

pub(super) fn animation_duration(config: &Config) -> Duration {
    Duration::from_millis(config.overlay.animation_ms.clamp(100, 1000))
}

/// The exit leg's duration: shorter than the entrance — a confident close
/// (the same 4/5 ratio the hover collapse uses) that still gives the release
/// spring's undershoot tail room to play out.
pub(super) fn collapse_duration(config: &Config) -> Duration {
    Duration::from_millis((animation_duration(config).as_millis() * 4 / 5) as u64)
}

/// Quintic ease-out: a fast start with a long, soft settle. Used for opacity
/// ramps, where a punchy fade-in reads better than a slow cubic ramp.
pub(super) fn ease_out_quint(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    1.0 - (1.0 - value).powi(5)
}

/// A closed-form damped harmonic oscillator, the standard way to drive
/// springy UI motion (the CASpringAnimation-style solution used by React
/// Native's animation springs). The response is the unique solution of
/// y'' + 2ζΩy' + Ω²y = Ω² with initial position `from` and initial velocity
/// `velocity`, sampled at normalized leg time `t` (0..1 = one animation leg).
/// The free `velocity` (in progress-per-leg) lets an interrupting leg continue
/// a running motion seamlessly — the oscillator's solution is unique given
/// (position, velocity), so a resumed curve lands exactly where the
/// un-interrupted one would.
pub(super) struct Spring {
    /// Damping ratio: below 1 bounces (lower = more bounce), 1 is the
    /// fastest settle without a bounce, above 1 is overdamped.
    zeta: f32,
    /// Undamped angular frequency in radians per leg (omega * leg
    /// duration): how much of the oscillation/decay fits inside the leg.
    omega: f32,
}

impl Spring {
    /// The un-clamped response at `t` (may be called slightly outside 0..1
    /// for derivatives): the closed-form solution of y'' + 2ζΩy' + Ω²y = Ω²
    /// starting from `from` with `velocity`.
    pub(super) fn raw_value(&self, t: f32, from: f32, velocity: f32) -> f32 {
        let zeta = self.zeta;
        let w = self.omega;
        let a = from - 1.0;
        if zeta < 1.0 {
            // y = 1 + e^(-ζΩt)(A cos(ωd t) + B sin(ωd t)),
            // A = from - 1, B = (velocity + ζΩA) / ωd.
            let damped = (1.0 - zeta * zeta).sqrt();
            let b = (velocity + zeta * w * a) / (w * damped);
            let decay = (-zeta * w * t).exp();
            1.0 + decay * (a * (w * damped * t).cos() + b * (w * damped * t).sin())
        } else if zeta == 1.0 {
            // y = 1 + e^(-Ωt)(A + (velocity + ΩA)t).
            let b = velocity + w * a;
            let decay = (-w * t).exp();
            1.0 + decay * (a + b * t)
        } else {
            // y = 1 + e^(-ζΩt)(A cosh(ωd t) + B sinh(ωd t)).
            let damped = (zeta * zeta - 1.0).sqrt();
            let b = (velocity + zeta * w * a) / (w * damped);
            let decay = (-zeta * w * t).exp();
            1.0 + decay * (a * (w * damped * t).cosh() + b * (w * damped * t).sinh())
        }
    }

    /// The response at normalized `t`, pinned to the exact endpoint at the
    /// end of the leg: the resting state must render at exactly 1.0, never a
    /// hair short (the residual is below 1 % by construction, so the pin is
    /// invisible).
    pub(super) fn value_at(&self, t: f32, from: f32, velocity: f32) -> f32 {
        if t >= 1.0 {
            return 1.0;
        }
        self.raw_value(t.max(0.0), from, velocity)
    }

    /// The response's derivative with respect to normalized time at `t`
    /// (progress per leg), by central difference (h = 1e-4). The exact
    /// derivative has three messy branch cases, and the numeric one is
    /// accurate to ~1e-6 at these scales. The probe extrapolates a hair
    /// outside the leg at the exact endpoints, so the estimate stays
    /// second-order accurate there too; the un-pinned curve is analytic, so
    /// a 1e-4 excursion is numerically identical to the in-leg curve.
    pub(super) fn velocity_at(&self, t: f32, from: f32, velocity: f32) -> f32 {
        const H: f32 = 1e-4;
        let t = t.clamp(0.0, 1.0);
        (self.raw_value(t + H, from, velocity) - self.raw_value(t - H, from, velocity)) / (2.0 * H)
    }
}

/// The hover-expand spring: a firm attack (strong initial acceleration, so
/// the card starts growing immediately), a modest overshoot past 1.0, and an
/// exact 1.0 endpoint — the pinned expanded state must render at the true
/// expanded size. The mid-flight overshoot never reaches the geometry:
/// `morph_size` clamps the rendered rectangle, the clipping region, and the
/// hit-testing bounds to the Compact..Expanded interval. `ZETA` is the
/// damping ratio: 0.7 — the same as `ENTRANCE_GROW` — keeps both the
/// overshoot (~5 %) and, crucially, the undershoot after it (~0.2 %) small
/// enough that the clamp makes them invisible. The clamp hides values above
/// 1.0, but values below 1.0 pass straight through, so a bouncier spring
/// (ζ = 0.5 showed a ~5 % undershoot) visibly shrank the pill and regrew it
/// in the last stretch of the leg — the end-of-morph reversal. `HALF_CYCLES`
/// still fits 2.8 half-cycles into the leg (the overshoot peak around half
/// the leg, the residual decay below 1 % at the end).
pub(super) const EXPAND_SPRING: Spring = Spring {
    zeta: 0.7,
    omega: 2.8 * std::f32::consts::PI,
};

pub(super) fn spring_expand(t: f32) -> f32 {
    EXPAND_SPRING.value_at(t, 0.0, 0.0)
}

/// The entrance grow spring: the card opens from its compact shape into the
/// expanded layout with a soft iOS/ColorOS-style bounce. ζ=0.7 keeps the
/// overshoot at ~5 % — clearly bouncy, never a wobble — with the peak around
/// half the leg and the residual below 1 % at the end. Unlike the hover
/// morph, this overshoot is *shown* (see `grow_size`).
pub(super) const ENTRANCE_GROW: Spring = Spring {
    zeta: 0.7,
    omega: 2.8 * std::f32::consts::PI,
};

/// The collapse spring, shared by the hover return and the exit shrink: the
/// expand spring's shape family mirrored (see `spring_collapse`), with ζ=0.6
/// undershooting below compact by ~9.5 % of the remaining distance. The
/// undershoot spreads over the tail of the leg — `bounce_scale` renders it
/// as the whole-pill settle-bounce, and `morph_size` clamps it out of the
/// geometry — so the return lands with a slow, pronounced bounce instead of
/// a dead stop.
pub(super) const COLLAPSE_SPRING: Spring = Spring {
    zeta: 0.6,
    omega: 2.8 * std::f32::consts::PI,
};

/// The mirrored release curve: 1 − COLLAPSE_SPRING from `1 − from` with the
/// seed velocity negated (a positive expand velocity continues as a positive
/// remaining-progress velocity). Runs from exactly `from` down to exactly 0
/// (compact) at the leg end. A release from the pinned-expanded state passes
/// `from = 1.0, velocity = 0.0` (the earliest dismiss lands after the ≤500 ms
/// entrance, so no velocity seed is needed there).
pub(super) fn spring_collapse(t: f32, from: f32, velocity: f32) -> f32 {
    1.0 - COLLAPSE_SPRING.value_at(t, 1.0 - from, -velocity)
}
