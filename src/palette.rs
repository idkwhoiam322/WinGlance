use std::borrow::Cow;

/// Two dominant colors extracted from track artwork at decode time, used to
/// recolor UI accents (playback symbols, clock icon) and to drive the pill's
/// boundary aura gradient. Both colors pass one tier of the guard hierarchy
/// (vibrant → strict → relaxed → monochrome), so they read clearly against
/// the pill's dark text and dark or bright covers still yield a palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Palette {
    /// Primary accent color (RGBA, alpha 255).
    pub primary: [u8; 4],
    /// Secondary accent color (RGBA, alpha 255), at least 30° away from
    /// `primary` in HSL hue space. Falls back to `primary` when the artwork
    /// has no second qualifying color (monochrome covers).
    pub secondary: [u8; 4],
}

/// Guard thresholds for candidate colors: saturation must be high enough and
/// luminance in a mid range, so the color stands out from both black and
/// white pill elements.
const MIN_SATURATION: f32 = 0.25;
const MIN_LUMINANCE: f32 = 0.20;
const MAX_LUMINANCE: f32 = 0.85;
/// Guard for dark but colored artwork: a low saturation/luminance floor that
/// still excludes near-black. Only used when the Vibrant filter and the
/// basic guard find nothing, so dark-but-colorful covers (moody portraits)
/// still get a palette instead of the accent default.
const RELAXED_SATURATION: f32 = 0.10;
const RELAXED_MIN_LUMINANCE: f32 = 0.10;
/// Floor for the monochrome tier: above the relaxed floor so a dark neutral
/// primary still reads against the near-black pill background when reused as
/// a solid glyph color.
const MONOCHROME_MIN_LUMINANCE: f32 = 0.18;
/// Minimum circular distance in HSL hue (degrees) between the two picks.
const MIN_HUE_DISTANCE: f32 = 30.0;
/// 4 bits per channel: 4096 histogram buckets.
const CHANNEL_BITS: u32 = 4;
const BUCKET_COUNT: usize = 1 << (CHANNEL_BITS * 3);
/// Fixed grid for the palette's box-average pre-sample. The worker decodes
/// artwork at the fixed `events::ARTWORK_DECODE`² size, so the histogram input
/// is stable per cover; the grid additionally dampens the residual resampling
/// noise from sources that re-encode their thumbnail between reads, so a
/// near-tie between buckets cannot flip the dominant pick between pill shows.
const SAMPLE_GRID: usize = 16;

#[derive(Default, Clone, Copy)]
struct Bucket {
    count: u32,
    sum_r: u64,
    sum_g: u64,
    sum_b: u64,
}

/// Quantizes raw RGBA pixels into a two-color palette: 4-bit-per-channel
/// histogram, candidates ranked by population × vibrancy, then evaluated
/// through a four-tier hierarchy — Vibrant target, strict guard, relaxed
/// guard (dark art), monochrome guard (B&W/high-key) — with the secondary
/// picked for ≥ 30° hue separation. The input is the overlay's already-decoded
/// artwork buffer, so no extra image decode is needed — computing the palette
/// here is ~0.1ms, done once per unique cover in `ensure_art`. Square buffers
/// are box-averaged onto a fixed 16×16 grid first (see `SAMPLE_GRID`), so the
/// pick is stable against resampling noise on top of the worker's fixed-size
/// decode.
pub(crate) fn palette_from_rgba(rgba: &[u8]) -> Option<Palette> {
    // Box-average square artwork onto the fixed grid before histogramming;
    // non-square or sub-grid inputs (tests, degenerate art) histogram as-is.
    let sampled: Cow<[u8]> = match box_downsample_square(rgba, SAMPLE_GRID) {
        Some(buf) => Cow::Owned(buf),
        None => Cow::Borrowed(rgba),
    };
    let shift = 8 - CHANNEL_BITS;
    // Heap-allocated: 4096 ~32-byte buckets (~128 KiB) would be a large single
    // stack frame on the UI thread's render path (under ensure_art), and the
    // worker threads already run on deliberately small stacks.
    let mut buckets = vec![Bucket::default(); BUCKET_COUNT];
    for px in sampled.chunks_exact(4) {
        let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        if a < 32 {
            continue;
        }
        let idx = (((r >> shift) << (CHANNEL_BITS * 2)) | ((g >> shift) << CHANNEL_BITS) | (b >> shift)) as usize;
        let bucket = &mut buckets[idx];
        bucket.count += 1;
        bucket.sum_r += r as u64;
        bucket.sum_g += g as u64;
        bucket.sum_b += b as u64;
    }
    let mut candidates: Vec<(u32, [u8; 4])> = buckets
        .iter()
        .filter(|b| b.count > 0)
        .map(|b| (b.count, bucket_mean(b)))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // Sort by population for iteration order; the final selection uses
    // target-based scoring, not a global population sort.
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let max_count = candidates[0].0 as f32;
    // The histogram is heap-allocated; trim the candidates heap so the
    // allocator gets clean pages back during candidate selection.
    candidates.shrink_to_fit();

    // AndroidX-style Vibrant target selection: hard-filter by L/S bounds,
    // then score by proximity to ideal vibrant target (L=0.5, S=1.0)
    // weighted with population. This ensures a vibrant pink beats a
    // dull-but-dominant blue regardless of pixel count.
    const VIBRANT_L_MIN: f32 = 0.3;
    const VIBRANT_L_MAX: f32 = 0.78;
    const VIBRANT_S_MIN: f32 = 0.25;
    const IDEAL_L: f32 = 0.5;
    const WEIGHT_L: f32 = 1.0;
    const WEIGHT_S: f32 = 0.6;
    const WEIGHT_POP: f32 = 0.5;

    let mut best_score = f32::MIN;
    let mut primary = None;
    for (count, c) in &candidates {
        let (_, s, l) = rgb_to_hsl(c[0], c[1], c[2]);
        if !(VIBRANT_L_MIN..=VIBRANT_L_MAX).contains(&l) || s < VIBRANT_S_MIN {
            continue;
        }
        let l_score = 1.0 - (l - IDEAL_L).abs();
        // Population share of *this* candidate, not the top bucket's share
        // of itself (which is always 1.0 and made WEIGHT_POP a no-op).
        let pop_score = *count as f32 / max_count;
        let score = l_score * WEIGHT_L + s * WEIGHT_S + pop_score * WEIGHT_POP;
        if score > best_score {
            best_score = score;
            primary = Some(*c);
        }
    }
    // Candidate evaluation hierarchy:
    //   1. Vibrant target scoring (bright, saturated colors)
    //   2. Strict guard (S ≥ 0.25, Y ∈ [0.20, 0.85])
    //   3. Relaxed guard (S ≥ 0.10, Y ∈ [0.10, 0.85]) — dark artwork
    //   4. Monochrome guard (Y ≥ 0.18, no S bound) — B&W and high-key covers
    // Only if every tier finds nothing does palette extraction fail and the
    // accent fallback take over.
    let primary = primary
        .or_else(|| candidates.iter().find(|(_, c)| passes_guard(*c)).map(|(_, c)| *c))
        .or_else(|| candidates.iter().find(|(_, c)| relaxed_guard(*c)).map(|(_, c)| *c))
        .or_else(|| candidates.iter().find(|(_, c)| monochrome_guard(*c)).map(|(_, c)| *c))?;
    let primary_hue = rgb_to_hsl(primary[0], primary[1], primary[2]).0;
    let secondary = candidates
        .iter()
        .find(|(_, c)| {
            passes_guard(*c) && hue_distance(primary_hue, rgb_to_hsl(c[0], c[1], c[2]).0) >= MIN_HUE_DISTANCE
        })
        .or_else(|| {
            candidates.iter().find(|(_, c)| {
                relaxed_guard(*c) && hue_distance(primary_hue, rgb_to_hsl(c[0], c[1], c[2]).0) >= MIN_HUE_DISTANCE
            })
        })
        .or_else(|| {
            candidates.iter().find(|(_, c)| {
                monochrome_guard(*c) && hue_distance(primary_hue, rgb_to_hsl(c[0], c[1], c[2]).0) >= MIN_HUE_DISTANCE
            })
        })
        .map(|(_, c)| *c)
        .unwrap_or(primary);
    Some(Palette { primary, secondary })
}

/// Box-averages a square RGBA buffer onto a `grid × grid` sample. Pixels below
/// the histogram's alpha gate are skipped per cell, so an all-transparent cell
/// emits no sample and opaque regions are not darkened by transparent ones.
/// Returns `None` when the input is not a perfect square or has fewer than
/// `grid` pixels per side, so the caller falls back to the raw buffer.
fn box_downsample_square(rgba: &[u8], grid: usize) -> Option<Vec<u8>> {
    if !rgba.len().is_multiple_of(4) {
        return None;
    }
    let n = (rgba.len() / 4) as u64;
    let side = n.isqrt() as usize;
    if (side as u64) * (side as u64) != n || side < grid {
        return None;
    }
    let cell = side / grid;
    let mut out = Vec::with_capacity(grid * grid * 4);
    for gy in 0..grid {
        let y0 = gy * cell;
        let y1 = (y0 + cell).min(side);
        for gx in 0..grid {
            let x0 = gx * cell;
            let x1 = (x0 + cell).min(side);
            let (mut sum_r, mut sum_g, mut sum_b, mut count) = (0u64, 0u64, 0u64, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let px = &rgba[(y * side + x) * 4..(y * side + x) * 4 + 4];
                    if px[3] < 32 {
                        continue;
                    }
                    sum_r += px[0] as u64;
                    sum_g += px[1] as u64;
                    sum_b += px[2] as u64;
                    count += 1;
                }
            }
            if count == 0 {
                continue;
            }
            out.extend_from_slice(&[
                (sum_r / count as u64) as u8,
                (sum_g / count as u64) as u8,
                (sum_b / count as u64) as u8,
                255,
            ]);
        }
    }
    Some(out)
}

fn bucket_mean(b: &Bucket) -> [u8; 4] {
    let n = b.count.max(1) as u64;
    [(b.sum_r / n) as u8, (b.sum_g / n) as u8, (b.sum_b / n) as u8, 255]
}

/// HSL saturation ≥ MIN_SATURATION and relative luminance within
/// [MIN_LUMINANCE, MAX_LUMINANCE] — the contrast guard against the pill's
/// near-black text and white-ish elements.
fn passes_guard(color: [u8; 4]) -> bool {
    let (_, s, _) = rgb_to_hsl(color[0], color[1], color[2]);
    let y = 0.2126 * color[0] as f32 / 255.0 + 0.7152 * color[1] as f32 / 255.0 + 0.0722 * color[2] as f32 / 255.0;
    s >= MIN_SATURATION && (MIN_LUMINANCE..=MAX_LUMINANCE).contains(&y)
}

/// HSL saturation ≥ RELAXED_SATURATION and relative luminance within
/// [RELAXED_MIN_LUMINANCE, MAX_LUMINANCE] — the last-resort guard for dark
/// artwork, so a moody cover yields a palette instead of the accent default.
fn relaxed_guard(color: [u8; 4]) -> bool {
    let (_, s, _) = rgb_to_hsl(color[0], color[1], color[2]);
    let y = 0.2126 * color[0] as f32 / 255.0 + 0.7152 * color[1] as f32 / 255.0 + 0.0722 * color[2] as f32 / 255.0;
    s >= RELAXED_SATURATION && (RELAXED_MIN_LUMINANCE..=MAX_LUMINANCE).contains(&y)
}

/// Final-tier guard for grayscale and high-key artwork: any pixel above the
/// monochrome luminance floor qualifies — no saturation constraint and no
/// upper luminance ceiling. Both the strict and relaxed guards reject
/// monochrome covers (S ≈ 0) and bright white backgrounds (Y > 0.85), so
/// this tier keeps them from failing palette extraction entirely. The floor
/// sits above the relaxed tier's so a dark neutral primary still reads as a
/// solid glyph against the near-black pill background.
fn monochrome_guard(color: [u8; 4]) -> bool {
    let y = 0.2126 * color[0] as f32 / 255.0 + 0.7152 * color[1] as f32 / 255.0 + 0.0722 * color[2] as f32 / 255.0;
    y >= MONOCHROME_MIN_LUMINANCE
}

fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (rf, gf, bf) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let l = (max + min) / 2.0;
    let d = max - min;
    let s = if d == 0.0 {
        0.0
    } else {
        d / (1.0 - (2.0 * l - 1.0).abs())
    };
    let h = if d == 0.0 {
        0.0
    } else if max == rf {
        60.0 * (((gf - bf) / d) % 6.0)
    } else if max == gf {
        60.0 * (((bf - rf) / d) + 2.0)
    } else {
        60.0 * (((rf - gf) / d) + 4.0)
    };
    (if h < 0.0 { h + 360.0 } else { h }, s, l)
}

fn hue_distance(h1: f32, h2: f32) -> f32 {
    let d = (h1 - h2).abs() % 360.0;
    d.min(360.0 - d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(color: [u8; 4], width: usize, height: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(width * height * 4);
        for _ in 0..width * height {
            buf.extend_from_slice(&color);
        }
        buf
    }

    fn assert_hue_distance_at_least(a: [u8; 4], b: [u8; 4], min: f32) {
        let ha = rgb_to_hsl(a[0], a[1], a[2]).0;
        let hb = rgb_to_hsl(b[0], b[1], b[2]).0;
        let d = hue_distance(ha, hb);
        assert!(d >= min, "hue distance {d} < {min}");
    }

    #[test]
    fn empty_input_returns_none() {
        assert!(palette_from_rgba(&[]).is_none());
    }

    #[test]
    fn pure_black_fails_the_luminance_guard() {
        assert!(palette_from_rgba(&solid([0, 0, 0, 255], 8, 4)).is_none());
    }

    #[test]
    fn pure_white_yields_a_monochrome_palette() {
        // High-key white: the strict and relaxed guards reject Y > 0.85 and
        // S = 0, but the monochrome tier accepts any non-black pixel.
        let palette = palette_from_rgba(&solid([255, 255, 255, 255], 8, 4));
        assert!(palette.is_some(), "high-key white art must get a palette");
    }

    #[test]
    fn gray_yields_a_monochrome_palette() {
        let palette = palette_from_rgba(&solid([128, 128, 128, 255], 8, 4));
        assert!(palette.is_some(), "gray art must get a palette");
    }

    #[test]
    fn monochrome_portrait_yields_white_or_gray_palette() {
        // High-key B&W cover: off-white background dominates, dark gray
        // details are a minority. The primary must reflect the bright
        // background rather than failing palette extraction.
        let mut buf = solid([240, 240, 240, 255], 6, 4);
        buf.extend_from_slice(&solid([40, 40, 40, 255], 2, 4));
        let palette = palette_from_rgba(&buf).expect("B&W cover must yield a palette");
        let (r, g, b) = (palette.primary[0], palette.primary[1], palette.primary[2]);
        assert!(
            r > 200 && g > 200 && b > 200,
            "primary should reflect the bright background, got [{r}, {g}, {b}]"
        );
    }

    #[test]
    fn pure_black_fails_all_guards() {
        // The final (monochrome) tier excludes near-black and dark neutrals;
        // a dark but saturated color (dark red, S≈1.0, Y=0.10) must still
        // get a palette via the relaxed tier.
        assert!(palette_from_rgba(&solid([0, 0, 0, 255], 8, 4)).is_none());
        assert!(palette_from_rgba(&solid([120, 0, 0, 255], 8, 4)).is_some());
    }

    #[test]
    fn dark_low_saturation_color_passes_the_relaxed_guard() {
        // S ≈ 0.12 (fails the basic guard's 0.25), Y ≈ 0.29 (passes):
        // a dark moody skin tone that must still yield a palette.
        let palette = palette_from_rgba(&solid([90, 70, 80, 255], 8, 4));
        assert!(palette.is_some(), "dark but non-uniform art must get a palette");
        assert!(!passes_guard([90, 70, 80, 255]));
        assert!(relaxed_guard([90, 70, 80, 255]));
    }

    #[test]
    fn very_low_saturation_dark_color_passes_the_monochrome_tier() {
        // S ≈ 0.06 (below the relaxed floor of 0.10), Y ≈ 0.29: the dark
        // portrait case falls through the strict and relaxed guards, but
        // the monochrome tier still yields a palette instead of the pink
        // accent default.
        let palette = palette_from_rgba(&solid([81, 72, 76, 255], 8, 4));
        assert!(palette.is_some(), "very low saturation dark art must get a palette");
        assert!(!relaxed_guard([81, 72, 76, 255]));
        assert!(monochrome_guard([81, 72, 76, 255]));
    }

    #[test]
    fn transparent_image_returns_none() {
        assert!(palette_from_rgba(&solid([0, 0, 0, 0], 8, 4)).is_none());
    }

    #[test]
    fn monochrome_secondary_falls_back_to_primary() {
        let palette = palette_from_rgba(&solid([220, 40, 40, 255], 8, 4)).unwrap();
        assert_eq!(palette.secondary, palette.primary);
        assert!(passes_guard(palette.primary));
    }

    #[test]
    fn dual_tone_picks_two_hue_separated_colors() {
        // Left half red, right half blue: equal population, opposite hues.
        let mut buf = solid([220, 50, 50, 255], 4, 4);
        buf.extend_from_slice(&solid([50, 50, 220, 255], 4, 4));
        let palette = palette_from_rgba(&buf).unwrap();
        assert!(passes_guard(palette.primary));
        assert!(passes_guard(palette.secondary));
        assert_hue_distance_at_least(palette.primary, palette.secondary, MIN_HUE_DISTANCE);
        assert_ne!(palette.primary, palette.secondary);
    }

    #[test]
    fn guard_skips_black_majority_for_a_vibrant_minority() {
        // 6 columns of black (fails luminance) vs 2 of red: the palette must
        // come from the red minority, not the black majority.
        let mut buf = solid([0, 0, 0, 255], 6, 4);
        buf.extend_from_slice(&solid([220, 40, 40, 255], 2, 4));
        let palette = palette_from_rgba(&buf).unwrap();
        let (hue, sat, _) = rgb_to_hsl(palette.primary[0], palette.primary[1], palette.primary[2]);
        assert!(sat >= MIN_SATURATION);
        assert!(
            !(30.0..=330.0).contains(&hue),
            "primary should be red-ish, got hue {hue}"
        );
    }

    #[test]
    fn decode_size_does_not_shift_the_palette() {
        // The worker decodes the same cover at a DPI-dependent size; the
        // fixed-grid pre-sample must make the palette size-independent.
        // 8×8 pattern: 6 columns of red, 2 of blue, nearest-neighbor upsized
        // to the real decode sizes (multiples of 64).
        let mut pattern = solid([220, 40, 40, 255], 6, 8);
        pattern.extend_from_slice(&solid([50, 50, 220, 255], 2, 8));
        let upscaled = |side: usize| -> Vec<u8> {
            let mut out = vec![0u8; side * side * 4];
            for y in 0..side {
                for x in 0..side {
                    let src = &pattern[((y * 8 / side) * 8 + (x * 8 / side)) * 4..][..4];
                    out[(y * side + x) * 4..(y * side + x) * 4 + 4].copy_from_slice(src);
                }
            }
            out
        };
        let palettes: Vec<_> = [64usize, 128, 192, 256]
            .iter()
            .map(|&s| palette_from_rgba(&upscaled(s)).expect("art must yield a palette"))
            .collect();
        for p in &palettes[1..] {
            assert_eq!(*p, palettes[0], "palette must not depend on the decode size");
        }
        // The red majority, not the blue minority, drives the primary.
        let (hue, sat, _) = rgb_to_hsl(palettes[0].primary[0], palettes[0].primary[1], palettes[0].primary[2]);
        assert!(sat >= MIN_SATURATION);
        assert!(
            !(30.0..=330.0).contains(&hue),
            "primary should be red-ish, got hue {hue}"
        );
    }

    #[test]
    fn white_square_uses_the_downscale_path() {
        // Square buffers (the real worker decodes) go through the fixed-grid
        // box average; a 64×64 white cover must still yield a monochrome
        // palette, matching the non-square high-key behavior.
        let palette = palette_from_rgba(&solid([255, 255, 255, 255], 64, 64));
        assert!(palette.is_some(), "white square art must get a palette");
    }

    #[test]
    fn box_downsample_skips_transparent_pixels() {
        // 4×4 buffer onto a 2×2 grid (cell = 2×2): one opaque red pixel per
        // cell corner (only cell (0,0) gets one), the rest transparent. The
        // box average must ignore transparent pixels — the cell mean is the
        // red pixel itself, not a darkened average — and fully transparent
        // cells must emit no sample.
        let mut buf = vec![0u8; 4 * 4 * 4]; // transparent
        buf[0..4].copy_from_slice(&[220, 40, 40, 255]); // row 0, col 0
        let out = box_downsample_square(&buf, 2).expect("4×4 must downscale to 2×2");
        assert_eq!(out, [220, 40, 40, 255], "transparent pixels must be skipped");
    }

    #[test]
    fn non_square_input_falls_back_to_the_raw_histogram() {
        // The real decodes are square, but non-square inputs (and everything
        // below the grid) must still histogram directly.
        let mut buf = solid([220, 40, 40, 255], 6, 4);
        buf.extend_from_slice(&solid([50, 50, 220, 255], 2, 4));
        let palette = palette_from_rgba(&buf).expect("non-square art must yield a palette");
        let (hue, _, _) = rgb_to_hsl(palette.primary[0], palette.primary[1], palette.primary[2]);
        assert!(
            !(30.0..=330.0).contains(&hue),
            "primary should be red-ish, got hue {hue}"
        );
    }
}
