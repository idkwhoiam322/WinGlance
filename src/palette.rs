use std::io::Cursor;

/// Two dominant colors extracted from track artwork at decode time, used to
/// recolor UI accents (playback symbols, clock icon) and to drive the pill's
/// boundary aura gradient. Both colors pass the saturation/luminance guard,
/// so they read clearly against the pill's dark text.
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
/// Minimum circular distance in HSL hue (degrees) between the two picks.
const MIN_HUE_DISTANCE: f32 = 30.0;
/// Artwork side length after downscale: 48×48 ≈ 2.3k samples, far more than
/// a two-color histogram needs while staying cheap on the worker thread.
const SAMPLE_SIZE: u32 = 48;
/// Rejects decompression bombs the same way the overlay's artwork decode
/// does: a header can claim huge dimensions with a tiny payload.
const MAX_DIM: u32 = 4096;
/// 4 bits per channel: 4096 histogram buckets.
const CHANNEL_BITS: u32 = 4;

#[derive(Default, Clone, Copy)]
struct Bucket {
    count: u32,
    sum_r: u64,
    sum_g: u64,
    sum_b: u64,
}

/// Decodes artwork bytes (guarded against oversized images) and returns the
/// two dominant, guard-passing colors. `None` when the artwork cannot be
/// decoded, is fully transparent, or yields no color that passes the guard.
/// Runs on the SMTC worker thread only — never during a render tick.
pub(crate) fn palette_from_artwork(data: &[u8]) -> Option<Palette> {
    let mut reader = image::ImageReader::new(Cursor::new(data)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    reader.limits(limits);
    let image = reader.decode().ok()?.to_rgba8();
    let image = image::imageops::resize(&image, SAMPLE_SIZE, SAMPLE_SIZE, image::imageops::FilterType::Triangle);
    palette_from_rgba(&image.into_raw())
}

/// Quantizes raw RGBA pixels into a two-color palette: 4-bit-per-channel
/// histogram, candidates ranked by pixel count, filtered by the
/// saturation/luminance guard, secondary picked for ≥ 30° hue separation.
fn palette_from_rgba(rgba: &[u8]) -> Option<Palette> {
    let shift = 8 - CHANNEL_BITS;
    let bucket_count = 1usize << (CHANNEL_BITS * 3);
    let mut buckets = vec![Bucket::default(); bucket_count];
    for px in rgba.chunks_exact(4) {
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
    // Let the allocator reclaim the histogram slab; shrink_to_fit trims the
    // candidates heap so the allocator gets clean pages back.
    drop(buckets);
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
    // Fallback: if the Vibrant filter excluded everything (e.g. monochrome
    // artwork), use the first candidate passing the guard.
    let primary = primary.or_else(|| candidates.iter().find(|(_, c)| passes_guard(*c)).map(|(_, c)| *c))?;
    let primary_hue = rgb_to_hsl(primary[0], primary[1], primary[2]).0;
    let secondary = candidates
        .iter()
        .find(|(_, c)| {
            passes_guard(*c) && hue_distance(primary_hue, rgb_to_hsl(c[0], c[1], c[2]).0) >= MIN_HUE_DISTANCE
        })
        .map(|(_, c)| *c)
        .unwrap_or(primary);
    Some(Palette { primary, secondary })
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
        assert!(palette_from_artwork(b"").is_none());
    }

    #[test]
    fn pure_black_fails_the_luminance_guard() {
        assert!(palette_from_rgba(&solid([0, 0, 0, 255], 8, 4)).is_none());
    }

    #[test]
    fn pure_white_fails_the_luminance_guard() {
        assert!(palette_from_rgba(&solid([255, 255, 255, 255], 8, 4)).is_none());
    }

    #[test]
    fn gray_fails_the_saturation_guard() {
        assert!(palette_from_rgba(&solid([128, 128, 128, 255], 8, 4)).is_none());
    }

    #[test]
    fn dark_color_fails_the_luminance_guard() {
        assert!(palette_from_rgba(&solid([60, 0, 0, 255], 8, 4)).is_none());
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
    fn decodes_encoded_artwork_bytes() {
        let mut img = image::RgbaImage::new(8, 4);
        for (x, _, px) in img.enumerate_pixels_mut() {
            *px = if x < 4 {
                image::Rgba([220, 50, 50, 255])
            } else {
                image::Rgba([50, 50, 220, 255])
            };
        }
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let palette = palette_from_artwork(&buf).unwrap();
        assert_hue_distance_at_least(palette.primary, palette.secondary, MIN_HUE_DISTANCE);
    }
}
