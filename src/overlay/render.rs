//! Pill rendering: frame composition, text rasterization and vector primitives.

use super::morph::{
    MorphProgress, art_edge_gate, compact_alpha, compact_metrics, compact_size, compact_title_viewport,
    content_size_of, dim_color, expanded_alpha, morph_art_tile, morph_icon_pos, morph_radius, morph_symbol_pos,
    morph_title_band, row_unveil_alpha,
};
use super::{
    CONTENT_FADE_DURATION, ContentFade, DibCache, MARQUEE_FADE, MARQUEE_GAP, MARQUEE_HOLD, MarqueeCtx, MarqueeStrip,
    OverlayState, PillText, ROW_HEIGHT, TextScratch,
};
use crate::config::{AppearanceConfig, Config};
use crate::events::{MediaEvent, PlaybackState, PlaybackType, TrackInfo};
use crate::palette::Palette;
use anyhow::{Context, Result};
use log::debug;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::Arc;
use windows::Win32::Foundation::{COLORREF, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, COLOR_HIGHLIGHT, COLOR_WINDOWTEXT, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DT_CALCRECT, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC,
    DrawTextW, ETO_CLIPPED, ExtTextOutW, GdiFlush, GetSysColor, HBITMAP, HDC, HFONT, HGDIOBJ, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{HWND_TOPMOST, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetWindowPos, ULW_ALPHA};
use windows::core::PCWSTR;

/// The cross-fade's blend weight from the fade's elapsed time: a smoothstep
/// (a symmetric ease reads best for a dissolve), pinned at 1.0 past the
/// duration.
pub(super) fn fade_progress(fade: &ContentFade) -> f32 {
    let t = (fade.start.elapsed().as_secs_f32() / CONTENT_FADE_DURATION.as_secs_f32()).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Premultiplied per-pixel blend of `from` into `to` at `weight` (0.0 =
/// from, 1.0 = to): the cross-fade's frame composition. The frames are
/// tightly packed BGRA with the alpha channel included, so all four bytes
/// lerp.
pub(super) fn blend_frames(to: &mut [u8], from: &[u8], weight: f32) {
    for (dst, src) in to.iter_mut().zip(from.iter()) {
        *dst = (*dst as f32 * weight + *src as f32 * (1.0 - weight)).round() as u8;
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_layered(
    state: &mut OverlayState,
    content: &MediaEvent,
    width: i32,
    height: i32,
    scale: f32,
    alpha: u8,
    position: POINT,
    compact: bool,
    morph: Option<MorphProgress>,
    scale_factor: f32,
) -> Result<()> {
    let inset = state.aura_inset;
    let buf_w = (width + inset * 2).max(1);
    let buf_h = (height + inset * 2).max(1);
    // The settle-bounce renders the final layout at its true size and scales
    // the composed frame about the anchor into the window-sized buffer (see
    // `scale_frame_about`); the content's own pill size is the window size
    // divided by the scale factor. Outside the bounce (`scale_factor == 1`)
    // the content size is the window size, as always.
    let content_w = if scale_factor == 1.0 {
        width
    } else {
        (width as f32 / scale_factor).round().max(1.0) as i32
    };
    let content_h = if scale_factor == 1.0 {
        height
    } else {
        (height as f32 / scale_factor).round().max(1.0) as i32
    };
    let content_buf_w = (content_w + inset * 2).max(1);
    let content_buf_h = (content_h + inset * 2).max(1);
    // Every morph resolves to the expanded pill, so its final body bottom is
    // the rest edge the text rows unveil against (see `row_unveil_alpha`).
    let rest_pill_h = (content_size_of(&state.config, content, false).1 * scale)
        .round()
        .max(1.0) as i32;
    // The pill body's current and final bottom edges in buffer coordinates:
    // rows are laid out at their final positions, so anything below the
    // current edge would render outside the still-growing body.
    let body_bottom = inset + content_h;
    let rest_body_bottom = inset + rest_pill_h;
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
    // blit (or scale, during the bounce) the result into the real DIB at its
    // real stride right before the GDI call. The scratch buffer is grown
    // across frames (and shrunk back below only when an oversized frame
    // inflates it), so after warm-up this performs no per-frame heap
    // allocation, matching the existing `text_scratch` buffer's pattern
    // elsewhere in this file.
    let (hdc, _bitmap, bits) = dib_for(state, buf_w, buf_h)?;
    let alloc_w = state.dib.as_ref().map(|dib| dib.width).unwrap_or(buf_w) as usize;
    let alloc_h = state.dib.as_ref().map(|dib| dib.height).unwrap_or(buf_h) as usize;

    let needed = content_buf_w as usize * content_buf_h as usize * 4;
    let mut scratch = std::mem::take(&mut state.frame_scratch);
    clear_frame_scratch(&mut scratch, needed);
    draw_pixels(
        state,
        &mut scratch[..needed],
        content,
        content_buf_w as usize,
        content_buf_h as usize,
        scale,
        compact,
        morph,
        body_bottom,
    )?;
    draw_text_pixels(
        state,
        &mut scratch[..needed],
        content,
        content_buf_w,
        scale,
        compact,
        morph,
        body_bottom,
        rest_body_bottom,
    );
    // Record the composed frame's dimensions, so the next in-place content
    // swap can snapshot it for its cross-fade (see `update_content`).
    state.last_frame_w = content_buf_w as usize;
    state.last_frame_h = content_buf_h as usize;
    // The in-place content cross-fade: blend the previous frame into this
    // one. Valid only while both frames are the same static size (Phase
    // Shown with no hover morph or bounce); any change ends the fade here,
    // and this frame renders the new content plainly.
    if let Some(fade) = &mut state.content_fade {
        let progress = fade_progress(fade);
        if progress >= 1.0 || fade.from_w != content_buf_w as usize || fade.from_h != content_buf_h as usize {
            state.content_fade = None;
        } else {
            blend_frames(&mut scratch[..needed], &fade.from, progress);
        }
    }
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
    if scale_factor == 1.0 {
        blit_packed_rows(
            dib_slice,
            alloc_w * 4,
            &state.frame_scratch,
            content_buf_w as usize * 4,
            content_buf_h as usize,
        );
    } else {
        // The settle-bounce: scale the composed final-layout frame into the
        // window-sized DIB region, so the whole pill scales 1:1. The on-screen
        // anchor is produced by `placement()` repositioning the window as the
        // size changes — not by the resample pivot here.
        scale_frame_about(
            dib_slice,
            alloc_w * 4,
            buf_w as usize,
            buf_h as usize,
            &state.frame_scratch,
            content_buf_w as usize * 4,
            content_buf_w as usize,
            content_buf_h as usize,
            inset as usize,
            scale_factor,
        );
    }

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
pub(super) fn clear_frame_scratch(scratch: &mut Vec<u8>, needed: usize) {
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
pub(super) fn shrink_frame_scratch(scratch: &mut Vec<u8>, needed: usize) {
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
pub(super) fn blit_packed_rows(dst: &mut [u8], dst_stride_bytes: usize, src: &[u8], row_bytes: usize, rows: usize) {
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

/// Uniformly scales the composed frame into the window-sized DIB region, so the
/// whole pill — body, aura, rows, art, icon — grows and shrinks 1:1. The pivot
/// is the content's top-left corner (`inset`, `inset`): the resample is
/// scale-about-corner and the on-screen anchor is produced entirely by
/// `placement()` repositioning the window top-left as the size changes
/// (see `fullscreen.rs::placement`). Pure and GDI-free so it can be unit
/// tested directly.
#[allow(clippy::too_many_arguments)]
pub(super) fn scale_frame_about(
    dst: &mut [u8],
    dst_stride: usize,
    dst_w: usize,
    dst_h: usize,
    src: &[u8],
    src_stride: usize,
    src_w: usize,
    src_h: usize,
    inset: usize,
    scale: f32,
) {
    let inset_f = inset as f32;
    for y in 0..dst_h {
        let sy = inset_f + (y as f32 - inset_f) / scale;
        for x in 0..dst_w {
            let sx = inset_f + (x as f32 - inset_f) / scale;
            let off = y * dst_stride + x * 4;
            if sx < 0.0 || sy < 0.0 || sx >= src_w as f32 || sy >= src_h as f32 {
                dst[off..off + 4].fill(0);
                continue;
            }
            let x0 = sx as usize;
            let y0 = sy as usize;
            let x1 = (x0 + 1).min(src_w - 1);
            let y1 = (y0 + 1).min(src_h - 1);
            let fx = sx - x0 as f32;
            let fy = sy - y0 as f32;
            let p00 = y0 * src_stride + x0 * 4;
            let p10 = y0 * src_stride + x1 * 4;
            let p01 = y1 * src_stride + x0 * 4;
            let p11 = y1 * src_stride + x1 * 4;
            let blend = |a: u8, b: u8, f: f32| (a as f32 + (b as f32 - a as f32) * f).round() as u8;
            let b = blend(blend(src[p00], src[p10], fx), blend(src[p01], src[p11], fx), fy);
            let g = blend(
                blend(src[p00 + 1], src[p10 + 1], fx),
                blend(src[p01 + 1], src[p11 + 1], fx),
                fy,
            );
            let r = blend(
                blend(src[p00 + 2], src[p10 + 2], fx),
                blend(src[p01 + 2], src[p11 + 2], fx),
                fy,
            );
            let a = blend(
                blend(src[p00 + 3], src[p10 + 3], fx),
                blend(src[p01 + 3], src[p11 + 3], fx),
                fy,
            );
            dst[off] = b;
            dst[off + 1] = g;
            dst[off + 2] = r;
            dst[off + 3] = a;
        }
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
pub(super) fn backing_upper_bound(config: &Config, dpi: u32) -> (i32, i32) {
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
pub(super) fn create_dc_with_dib(width: i32, height: i32) -> Result<(HDC, HBITMAP, *mut c_void)> {
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
pub(super) fn dib_for(state: &mut OverlayState, width: i32, height: i32) -> Result<(HDC, HBITMAP, *mut c_void)> {
    if let Some(dib) = &state.dib
        && dib.width >= width
        && dib.height >= height
    {
        return Ok((dib.hdc, dib.bitmap, dib.bits));
    }
    // Too small or absent: dropping the old cache unselects its bitmap and
    // frees the DIB (see `Drop for DibCache`); a fresh buffer is created
    // below. The backing allocation is reused for the rest of the process's
    // life, so resizing happens at most once per DPI/config.
    state.dib = None;
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

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_pixels(
    state: &mut OverlayState,
    pixels: &mut [u8],
    content: &MediaEvent,
    width: usize,
    height: usize,
    scale: f32,
    compact: bool,
    morph: Option<MorphProgress>,
    body_bottom: i32,
) -> Result<()> {
    // One radius per frame. A morph lerps the radius continuously between
    // the compact and the expanded radius (see `morph_radius`), so the
    // corner curvature follows the silhouette while the pill changes shape;
    // every other frame uses the radius of the effective layout (`compact`
    // is the already-resolved layout: Auto has been decided into Expanded or
    // Compact before rendering, so Auto automatically follows whatever is
    // drawn). The same value feeds the aura, the pill body and the edge
    // stroke, keeping the shadow, fill, border and clipped shape aligned.
    // Oversized values are safe: every rounded-rect primitive clamps the
    // radius to half the smaller pill dimension.
    let radius = match morph {
        Some(progress) => {
            morph_radius(
                state.config.appearance.effective_corner_radius(true),
                state.config.appearance.effective_corner_radius(false),
                progress,
            ) * scale
        }
        None => state.config.appearance.effective_corner_radius(compact) * scale,
    };
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
                state.track_cache.get(source_app).and_then(|t| t.decoded_art.clone())
            }
        }
        MediaEvent::SessionRejected { .. }
        | MediaEvent::SourceGone { .. }
        | MediaEvent::WorkerFailed { .. }
        | MediaEvent::ArtworkBudgetExceeded
        | MediaEvent::ProgressChanged { .. } => None,
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
    // fill exactly. Single source shared with the text-row contrast checks
    // (`pill_fill_bg`), so the checks always measure the painted backdrop.
    let effective_bg = pill_fill_bg(state);
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

    // Progress bar: a thin accent fill at the pill's bottom edge, masked to
    // the rounded body so it never paints into the transparent aura ring at
    // the corners. Present only when the source reports both a position and a
    // non-zero duration.
    if let (Some(position), Some(duration)) = (state.estimated_position_secs, state.progress_duration_secs)
        && duration > 0
    {
        let position = position.clamp(0.0, duration as f64);
        let fraction = (position / duration as f64).clamp(0.0, 1.0) as f32;
        let bar_h = (2.0 * scale).round().max(1.0) as usize;
        let bar_y = inset + pill_h.saturating_sub(bar_h);
        let bar_w = (pill_w as f32 * fraction).round() as usize;
        let bar_color = aura_palette.primary;
        let bar_alpha = (bar_color[3] as f32 * 0.8) as u32;
        for y in bar_y..(bar_y + bar_h).min(height) {
            for x in inset..(inset + bar_w).min(width) {
                let cov = round_rect_coverage_supersampled(
                    (x as i32 - inset as i32) as f32,
                    (y as i32 - inset as i32) as f32,
                    pill_w as f32,
                    pill_h as f32,
                    radius,
                );
                if cov > 0.0 {
                    composite(
                        pixels,
                        width,
                        x,
                        y,
                        [bar_color[0], bar_color[1], bar_color[2]],
                        bar_alpha,
                    );
                }
            }
        }
    }

    // Directional edge highlight: white stroke on the pill's own boundary,
    // brighter along the top-left than the bottom-right.
    draw_edge_stroke(pixels, width, inset, pill_w, pill_h, radius, scale);

    // The compact pill draws its own smaller art tile (plus the title row
    // and the trailing icon/symbol) in `draw_compact_pill`; drawing it here
    // as well would composite the halo, the cover and the rim twice. The
    // expanded pills draw the art tile at the configured art size. During a
    // morph the two tiles merge into one interpolated tile (`morph_art_tile`):
    // the side length lerps between the compact and expanded sizes while the
    // tile stays centered in the growing body — the art grows in place
    // instead of two tiles swapping, and only the defensive edge gate
    // (`art_edge_gate`) can dim it.
    if !compact {
        let padding = (state.config.appearance.padding * scale).round() as usize;
        let art_size = (state.config.appearance.art_size as f32 * scale).round() as usize;
        let (art_size, art_y, content_alpha) = match (content, morph) {
            (MediaEvent::TrackChanged(_), Some(progress)) => {
                let shape = progress.width.min(progress.height);
                let (_, art_y, art_size) = morph_art_tile(&state.config, inset as i32, pill_h as i32, scale, shape);
                // The tile grows with the body and stays inside it, so it
                // renders at full opacity for the whole leg; the gate only
                // fades it in the extreme case where the shrinking body edge
                // would cut through it.
                let unveil = art_edge_gate(body_bottom, art_y, art_size);
                (art_size as usize, art_y as usize, unveil)
            }
            (MediaEvent::TrackChanged(_), None) => (art_size, inset + pill_h.saturating_sub(art_size) / 2, 1.0),
            (MediaEvent::PlaybackStateChanged(_, _), Some(progress)) => {
                // State pills reuse the cached track's artwork for the source
                // that produced the state change, so a pause/play pill still
                // shows the right cover. The art size is clamped to the pill
                // body: the state-pill layout reserves no extra rows.
                let shape = progress.width.min(progress.height);
                let (_, _, morph_size) = morph_art_tile(&state.config, inset as i32, pill_h as i32, scale, shape);
                let art_size = morph_size.min(pill_h as i32 - 2 * padding as i32);
                let art_y = inset as i32 + (pill_h as i32 - art_size) / 2;
                let unveil = art_edge_gate(body_bottom, art_y, art_size);
                (art_size as usize, art_y as usize, unveil)
            }
            (MediaEvent::PlaybackStateChanged(_, _), None) => {
                let art_size = art_size.min(pill_h.saturating_sub(2 * padding));
                (art_size, inset + pill_h.saturating_sub(art_size) / 2, 1.0)
            }
            // Never rendered: SessionRejected is filtered out before enqueue.
            (
                MediaEvent::SessionRejected { .. }
                | MediaEvent::SourceGone { .. }
                | MediaEvent::WorkerFailed { .. }
                | MediaEvent::ArtworkBudgetExceeded
                | MediaEvent::ProgressChanged { .. },
                _,
            ) => return Ok(()),
        };
        if content_alpha > 0.0 {
            let art_radius = art_size as f32 * 0.2;
            let art_x = inset + padding;
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
                content_alpha,
            );
        }
    }
    Ok(())
}

/// Draws the art tile at (art_x, art_y): the accent halo behind the square,
/// the cover (or the accent placeholder when no art decoded) and the glowing
/// rim. Shared by the track- and state-pill arms, which differ only in the
/// art-size clamp the caller applies. The mask radius must match the one
/// `draw_art_scaled` uses for the art bitmap itself, not the pill's
/// `corner_radius` (either pill radius — this runs on the expanded-only
/// path) — otherwise the halo/rim are rounder than the art beneath
/// them and visibly don't hug its corners.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_art_tile(
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
    content_alpha: f32,
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
                    let alpha = (c[3] as f32 * 0.75 * content_alpha * cov) as u32;
                    composite(pixels, width, halo_x + dx, halo_y + dy, [c[0], c[1], c[2]], alpha);
                }
            }
        }
    }
    if let Some(art) = decoded_art {
        draw_art_scaled(pixels, width, art, art_x, art_y, art_size, accent, content_alpha);
    } else {
        draw_placeholder(pixels, width, art_x, art_y, art_size, dim_color(accent, content_alpha));
    }
    // Glowing rim: thin 1.5px accent stroke around the album art.
    if let Some(c) = palette.map(|p| p.primary) {
        let stroke_w = (1.5 * scale).round().max(1.0);
        for dy in 0..art_size {
            for dx in 0..art_size {
                let d = round_rect_signed_dist(dx as f32, dy as f32, art_size as f32, art_size as f32, art_radius);
                if d.abs() < stroke_w {
                    let edge = 1.0 - d.abs() / stroke_w;
                    let alpha = (c[3] as f32 * 0.9 * content_alpha * edge) as u32;
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
pub(super) fn draw_edge_stroke(
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
pub(super) fn draw_art_scaled(
    pixels: &mut [u8],
    width: usize,
    art: &[u8],
    x: usize,
    y: usize,
    size: usize,
    accent: [u8; 4],
    content_alpha: f32,
) {
    let base = (art.len() / 4) as f64;
    let base = base.sqrt() as usize;
    if size == 0 || base == 0 || base * base * 4 != art.len() {
        draw_placeholder(pixels, width, x, y, size, dim_color(accent, content_alpha));
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
            let alpha = (a as f32 * content_alpha * coverage) as u32;
            composite(pixels, width, x + dx, y + dy, [r, g, b], alpha);
        }
    }
}

pub(super) fn lerp(a: u8, b: u8, t: f32) -> u8 {
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
pub(super) fn draw_symbol_pixels(
    pixels: &mut [u8],
    width: usize,
    right: i32,
    y: i32,
    size: f32,
    playback: PlaybackState,
    playback_type: PlaybackType,
    color: [u8; 4],
) {
    let radius = (0.10 * size).max(0.0);
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
        PlaybackState::NowPlaying if playback_type == PlaybackType::Video => {
            draw_video_icon(pixels, width, box_left as f32, v_center, size, color);
        }
        PlaybackState::NowPlaying if playback_type == PlaybackType::Image => {
            draw_image_icon(pixels, width, box_left as f32, v_center, size, color);
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

/// Resolves the glyph for a `TrackChanged` snapshot: the playback state
/// reported by the source in the same `GetPlaybackInfo` read, or the default
/// `NowPlaying` symbol when the source did not report one (transitional or
/// unknown statuses). All three layout paths share this so the glyph can never
/// drift from the snapshot that carries it.
pub(super) fn playback_state_for_track(track: &TrackInfo) -> PlaybackState {
    track.playback_state.unwrap_or(PlaybackState::NowPlaying)
}

/// Draws the video-player glyph shown on track-change pills whose source
/// reported `Video`: a hollow rounded box with an optically centered play
/// triangle. The frame is 0.72S × 0.48S; four capsule bars (0.055S thick,
/// radius half the thickness) are laid as full-width top/bottom and
/// full-height left/right rails, overlapping so the corners connect solidly
/// instead of reading as four separate lines. The frame's six edges are
/// rounded once and every rail derives from them, so the rails can never
/// disagree by a pixel (independent (x,w)/(y,h) rounding would let the
/// vertical rails extend one row past the horizontal ones at small sizes).
/// The triangle is 0.22S × 0.26S with 0.03S corner radius, shifted 0.025S
/// right of the box center for optical balance. Every shard is CCW:
/// `rounded_triangle_coverage` treats only counter-clockwise triangles as
/// inside.
fn draw_video_icon(pixels: &mut [u8], width: usize, x: f32, y: f32, size: f32, color: [u8; 4]) {
    let fw = 0.72 * size;
    let fh = 0.48 * size;
    let thick = 0.055 * size;
    let left = x + (size - fw) / 2.0;
    let top = y - fh / 2.0;
    // Shared, once-rounded frame edges; the corner radius is capped inside
    // the fill helper.
    let l = left.round() as i32;
    let t = top.round() as i32;
    let r = (left + fw).round() as i32;
    let b = (top + fh).round() as i32;
    let th = thick.round() as i32;

    // Top and bottom rails run the full frame width; left and right rails
    // run the full frame height over them, so the corners overlap into a
    // solid capsule instead of a notched joint.
    for by in [t, b - th] {
        draw_rounded_rect_filled(pixels, width, l, by, r - l, th, thick / 2.0, color);
    }
    for bx in [l, r - th] {
        draw_rounded_rect_filled(pixels, width, bx, t, th, b - t, thick / 2.0, color);
    }

    let tri_w = 0.22 * size;
    let tri_h = 0.26 * size;
    let tx = left + fw / 2.0 + 0.025 * size - tri_w / 2.0;
    let ty = y - tri_h / 2.0;
    draw_triangle_filled(
        pixels,
        width,
        (tx.round() as i32, ty.round() as i32),
        ((tx + tri_w).round() as i32, (ty + tri_h / 2.0).round() as i32),
        (tx.round() as i32, (ty + tri_h).round() as i32),
        0.03 * size,
        color,
    );
}

/// Draws the image glyph for `Image`-typed tracks: a landscape icon — a
/// rounded frame (0.66S × 0.50S, rails 0.055S thick) with a sun disc and a
/// mountain triangle. The frame's six edges are rounded once and shared by
/// every rail (same discipline as `draw_video_icon`), and the horizontal
/// rails' own rounded caps form the corners at the same radius as the video
/// frame. The mountain is CCW. This glyph draws only for an `Image` type,
/// which the worker currently suppresses — so it never renders in
/// production.
fn draw_image_icon(pixels: &mut [u8], width: usize, x: f32, y: f32, size: f32, color: [u8; 4]) {
    let fw = 0.66 * size;
    let fh = 0.50 * size;
    let thick = 0.055 * size;
    let left = x + (size - fw) / 2.0;
    let top = y - fh / 2.0;
    // Shared, once-rounded frame edges (see `draw_video_icon`).
    let l = left.round() as i32;
    let t = top.round() as i32;
    let r = (left + fw).round() as i32;
    let b = (top + fh).round() as i32;
    let th = thick.round() as i32;

    // Full-width top/bottom rails; the side rails butt between them, so the
    // horizontal rails' rounded caps round the four corners on their own.
    draw_rounded_rect_filled(pixels, width, l, t, r - l, th, thick / 2.0, color);
    draw_rounded_rect_filled(pixels, width, l, b - th, r - l, th, thick / 2.0, color);
    draw_rounded_rect_filled(pixels, width, l, t + th, th, b - t - 2 * th, thick / 2.0, color);
    draw_rounded_rect_filled(pixels, width, r - th, t + th, th, b - t - 2 * th, thick / 2.0, color);

    // Sun: a disc (a rounded square at radius d/2) tucked into the top-left.
    let sun_d = 0.14 * size;
    let sun_x = left + thick + 0.05 * size;
    let sun_y = top + thick + 0.05 * size;
    draw_rounded_rect_filled(
        pixels,
        width,
        sun_x.round() as i32,
        sun_y.round() as i32,
        sun_d.round() as i32,
        sun_d.round() as i32,
        sun_d / 2.0,
        color,
    );

    // Mountain: CCW triangle from the bottom corners to a centered peak.
    draw_triangle_filled(
        pixels,
        width,
        (
            (left + thick + 0.03 * size).round() as i32,
            (top + fh - thick).round() as i32,
        ),
        (
            (left + fw * 0.5).round() as i32,
            (top + thick + 0.06 * size).round() as i32,
        ),
        (
            (left + fw - thick - 0.03 * size).round() as i32,
            (top + fh - thick).round() as i32,
        ),
        0.06 * size,
        color,
    );
}

/// Fills a rounded rectangle into the pixel buffer using `round_rect_coverage`.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_rounded_rect_filled(
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
pub(super) fn draw_triangle_filled(
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
pub(super) fn edge_signed_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let ex = bx - ax;
    let ey = by - ay;
    let len = ex.hypot(ey);
    if len <= 0.0 {
        return f32::INFINITY;
    }
    (ex * (py - ay) - ey * (px - ax)) / len
}

/// Distance from a point to the closest point of a line segment.
pub(super) fn point_segment_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
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
pub(super) fn inset_vertex(ax: f32, ay: f32, bx: f32, by: f32, cx: f32, cy: f32, radius: f32) -> (f32, f32) {
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
pub(super) fn rounded_triangle_coverage(
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
pub(super) const FILL_TINT_WEIGHT: f32 = 0.16;

/// Blends `accent` into `base` at `weight`, keeping base's own alpha.
/// Used to give the pill fill a subtle per-track hue instead of a fixed
/// neutral fill.
pub(super) fn tinted_fill(base: [u8; 4], accent: [u8; 4], weight: f32) -> [u8; 4] {
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
pub(super) fn muted_accent(primary: [u8; 4]) -> [u8; 4] {
    let lift = |c: u8| -> u8 {
        let float = c as f32;
        (float + (255.0 - float) * 0.35).clamp(0.0, 255.0) as u8
    };
    [lift(primary[0]), lift(primary[1]), lift(primary[2]), 255]
}

/// WCAG 2.x AA contrast target for the palette-derived text rows (normal-
/// sized text on the pill body). The meta row's clock icon shares the row's
/// color and inherits the same floor.
pub(crate) const TEXT_CONTRAST_AA: f32 = 4.5;

/// The pill body fill actually painted behind the text rows: the configured
/// background blended 16% toward the artwork's palette primary when a
/// palette is available, otherwise the configured background exactly. Both
/// the drawing (`render`) and the text contrast checks consume this single
/// source, so the check cannot drift from what is actually painted.
/// A high-contrast theme replaces all of it with the opaque system window
/// color: no palette tint, no translucency.
pub(super) fn pill_fill_bg(state: &OverlayState) -> [u8; 4] {
    if crate::winutil::system_preferences().high_contrast {
        return crate::winutil::system_window_color();
    }
    match state.palette {
        Some(palette) => tinted_fill(
            state.config.appearance.background_color,
            palette.primary,
            FILL_TINT_WEIGHT,
        ),
        None => state.config.appearance.background_color,
    }
}

/// The effective pill text color: the configured color normally, the system
/// window-text color under a high-contrast theme.
pub(super) fn pill_text_color(appearance: &AppearanceConfig) -> [u8; 4] {
    if crate::winutil::system_preferences().high_contrast {
        let text = unsafe { GetSysColor(COLOR_WINDOWTEXT) };
        return [
            (text & 0xFF) as u8,
            ((text >> 8) & 0xFF) as u8,
            ((text >> 16) & 0xFF) as u8,
            0xFF,
        ];
    }
    appearance.text_color
}

/// The palette fallback/accent base for symbols and secondary rows: the
/// artwork palette's primary normally, the system highlight color under a
/// high-contrast theme (the palette is disabled there).
pub(super) fn pill_accent_base(state: &OverlayState, appearance: &AppearanceConfig) -> [u8; 4] {
    if crate::winutil::system_preferences().high_contrast {
        let highlight = unsafe { GetSysColor(COLOR_HIGHLIGHT) };
        return [
            (highlight & 0xFF) as u8,
            ((highlight >> 8) & 0xFF) as u8,
            ((highlight >> 16) & 0xFF) as u8,
            0xFF,
        ];
    }
    state.palette.map(|p| p.primary).unwrap_or(appearance.accent_color)
}

/// Relative luminance of an sRGB color, linearized per the WCAG 2.x formula
/// (gamma-encoded channel values are not weights). The palette guard's own
/// luminance is a gamma-encoded approximation for candidate selection; the
/// contrast guarantee uses the exact formula.
pub(super) fn relative_luminance([r, g, b]: [u8; 3]) -> f32 {
    let linearize = |channel: u8| {
        let s = channel as f32 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linearize(r) + 0.7152 * linearize(g) + 0.0722 * linearize(b)
}

/// WCAG 2.x contrast ratio between two opaque colors, in 1..=21.
pub(crate) fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f32 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Brightens `text` toward white until its contrast against `bg` reaches
/// `target`, or returns it unchanged when it already passes. The palette
/// guard validates candidate colors against the pill's two fixed colors
/// only; at render time the same primary does double duty — the fill blends
/// 16% toward it (`pill_fill_bg`) while the text rows draw in the raw or
/// 35%-lifted primary — so a guard-accepted color can still land at 1.4:1
/// (raw primary) or ~4:1 (lifted) against the tinted fill. The check must
/// therefore run here, against the actual backdrop. Blending toward white
/// strictly raises luminance, so a bisection in the blend weight finds the
/// smallest lift that passes. Even pure white is returned as the best
/// effort when it cannot pass (a near-white fill).
pub(crate) fn ensure_contrast(text: [u8; 4], bg: [u8; 4], target: f32) -> [u8; 4] {
    let text_rgb = [text[0], text[1], text[2]];
    let bg_rgb = [bg[0], bg[1], bg[2]];
    if contrast_ratio(text_rgb, bg_rgb) >= target {
        return text;
    }
    // Blend toward whichever of pure white or pure black gives the higher
    // contrast against this background. On the default dark fills that is
    // white (brighten); on a user-configured light fill it is black
    // (darken) — brightening toward white there would push contrast the
    // wrong way and can make it worse than doing nothing.
    let endpoint = if contrast_ratio([255, 255, 255], bg_rgb) >= contrast_ratio([0, 0, 0], bg_rgb) {
        255.0
    } else {
        0.0
    };
    let blend = |w: f32| -> [u8; 3] {
        [
            (text[0] as f32 + (endpoint - text[0] as f32) * w).round() as u8,
            (text[1] as f32 + (endpoint - text[1] as f32) * w).round() as u8,
            (text[2] as f32 + (endpoint - text[2] as f32) * w).round() as u8,
        ]
    };
    // Bisection on the blend weight, keeping `hi` the smallest weight seen
    // so far that passes and `lo` the largest that fails; the initial
    // `hi = 1.0` is only actually passing when the pure endpoint passes, so
    // when even it cannot reach the target (a fill that contrasts poorly
    // with both endpoints) the endpoint is the best effort. Blending toward
    // an endpoint strictly moves luminance toward it, which makes the
    // pass/fail boundary monotonic and the bisection exact.
    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if contrast_ratio(blend(mid), bg_rgb) >= target {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let lifted = blend(hi);
    [lifted[0], lifted[1], lifted[2], text[3]]
}

/// Draws the shared pill text layout used by every notification: title,
/// artist, meta and source-app rows, fitted to the rows that are actually
/// present. When `playback` is `Some`, the title row reserves space on its
/// right for the play/pause/stop symbol; track-change pills pass `None` and
/// use the full width. Every row marquee-scrolls when it overflows.
///
/// `body_bottom`/`rest_body_bottom` are the current and final (expanded)
/// pill body bottom edges in buffer coordinates. Rows are laid out at their
/// final positions, so while a morph grows the pill each row below the
/// current edge would render outside the body; `row_unveil_alpha` gates
/// each row to the edge instead — nothing draws outside the pill, and every
/// row fades in/out with the sweep of the growing/shrinking bottom edge
/// (see `draw_text_pixels` for how this interacts with the morph).
/// `skip_title` keeps the title band's height but skips the title row and
/// its symbol slot — the morph's traveling title replaces them — so the
/// remaining rows sit at their steady positions.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_pill_text_rows(
    state: &mut OverlayState,
    pixels: &mut [u8],
    width: i32,
    scale: f32,
    pill: &PillText,
    playback: Option<PlaybackState>,
    playback_type: PlaybackType,
    content_alpha: f32,
    body_bottom: i32,
    rest_body_bottom: i32,
    skip_title: bool,
) {
    let inset = state.aura_inset;
    let appearance = &state.config.appearance;
    // Accent color: the displayed artwork's primary palette color when
    // available (gives the pill per-track theming), falling back to the
    // configured accent. Every color is dimmed by the cross-fade's
    // per-pass opacity, so the whole content fades as one.
    //
    // The palette-derived text colors are re-checked against the actual
    // painted fill (`pill_fill_bg`) at composite time, before dimming:
    // the same primary both tints the fill 16% toward it and colors these
    // rows, so the two move together and the palette guard's check against
    // the untinted fixed colors cannot see the real pair. `ensure_contrast`
    // brightens only the colors that fail the AA target — passing palettes
    // render exactly as before. The muted tier ("muted_accent", used by the
    // artist and app-name rows) and the meta row share `accent_base`, so a
    // palette-less pill tracks the user's accent in both tiers alike.
    let accent_base = pill_accent_base(state, appearance);
    let fill = pill_fill_bg(state);
    let accent = dim_color(ensure_contrast(accent_base, fill, TEXT_CONTRAST_AA), content_alpha);
    let muted = dim_color(
        ensure_contrast(muted_accent(accent_base), fill, TEXT_CONTRAST_AA),
        content_alpha,
    );
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
    let pad = appearance.padding;
    let (font_title, h_title) = state.fonts.font_for(rows[0].1 as i32, true);
    let (font_artist, h_artist) = state.fonts.font_for(rows[1].1 as i32, false);
    let (font_meta, h_meta) = state.fonts.font_for(rows[2].1 as i32, false);
    let (font_app, h_app) = state.fonts.font_for(rows[3].1 as i32, false);
    // Only rows that will actually be drawn take up vertical space: the rest
    // of the pill's constant height stays empty below the rows.
    let artist_display = if pill.artist.trim().is_empty() {
        "Unknown Artist"
    } else {
        pill.artist.as_str()
    };
    let active: [bool; 4] = [true, true, !pill.meta.is_empty(), !pill.source_app.trim().is_empty()];
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
    if !skip_title {
        let unveil = row_unveil_alpha(body_bottom, rest_body_bottom, title_rect.bottom);
        if unveil > 0.0 {
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
                dim_color(pill_text_color(appearance), content_alpha * unveil),
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
                    playback_type,
                    dim_color(accent, unveil),
                );
            }
        }
    }

    let artist_rect = next_band(1);
    {
        let unveil = row_unveil_alpha(body_bottom, rest_body_bottom, artist_rect.bottom);
        if unveil > 0.0 {
            draw_text_line_pixels(
                &mut state.text_scratch,
                &mut state.scratch_utf16,
                pixels,
                width as usize,
                artist_display,
                &artist_rect,
                font_artist,
                h_artist,
                dim_color(muted, unveil),
                scale,
                Some(MarqueeCtx {
                    scroll: &mut state.scroll[1],
                    strip: &mut state.marquee_strips[1],
                }),
            );
        }
    }

    if active[2] {
        let meta_rect = next_band(2);
        let unveil = row_unveil_alpha(body_bottom, rest_body_bottom, meta_rect.bottom);
        if unveil > 0.0 {
            // `accent` here already carries the pass opacity; unveil is the
            // row's share of it, so the clock icon and meta text fade with
            // the row instead of popping at the edge.
            let row_accent = dim_color(accent, unveil);
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
                row_accent,
                row_accent,
                scale,
                Some(MarqueeCtx {
                    scroll: &mut state.scroll[2],
                    strip: &mut state.marquee_strips[2],
                }),
            );
        }
    }
    if active[3] {
        let app_rect = next_band(3);
        let unveil = row_unveil_alpha(body_bottom, rest_body_bottom, app_rect.bottom);
        if unveil > 0.0 {
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
                dim_color(muted, unveil),
                scale,
                content_alpha * unveil,
                Some(MarqueeCtx {
                    scroll: &mut state.scroll[3],
                    strip: &mut state.marquee_strips[3],
                }),
            );
        }
    }
}

/// Builds the render pieces for a track, computing the meta line once.
pub(super) fn pill_text_from_track(track: &TrackInfo) -> PillText {
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
/// window — it never touches the alpha channel). While a morph is in flight
/// `draw_morph_content` takes over: the shared elements (title, playback
/// symbol) travel between the layouts and only the layout-exclusive elements
/// fade — the compact app icon out, the expanded extra rows in — both keyed
/// to the SHAPE progress, the less-advanced of the two axes
/// `min(width, height)`. On expand that is the lagging height, so the icon
/// holds until the pill grows tall, then dissolves out while the extra rows
/// arrive as the height rises. On collapse it is the leading width, so the
/// extra rows leave as the pill narrows, before the icon fades back in. The
/// two fade windows (see `compact_alpha` / `expanded_alpha`) are disjoint, so
/// the exclusive elements never coexist mid-morph.
/// `body_bottom`/`rest_body_bottom` (the pill body's current and final bottom
/// edges) additionally gate every expanded row to the animated edge via
/// `row_unveil_alpha`: a row is not drawn until the edge has passed its band,
/// so text can never render outside the growing/shrinking body.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_text_pixels(
    state: &mut OverlayState,
    pixels: &mut [u8],
    content: &MediaEvent,
    width: i32,
    scale: f32,
    compact: bool,
    morph: Option<MorphProgress>,
    body_bottom: i32,
    rest_body_bottom: i32,
) {
    if let Some(progress) = morph {
        draw_morph_content(
            state,
            pixels,
            content,
            width,
            scale,
            progress,
            body_bottom,
            rest_body_bottom,
        );
    } else if compact {
        draw_compact_pill(state, pixels, content, width, scale, 1.0);
    } else {
        draw_expanded_pill_text(
            state,
            pixels,
            content,
            width,
            scale,
            1.0,
            body_bottom,
            rest_body_bottom,
            false,
        );
    }
}

/// Draws the expanded layout's text rows (and the state-pill fallback) into
/// the pixel buffer, at `content_alpha` (1.0 when no morph is running). The
/// alpha multiplies every drawn color, so the whole content fades together
/// as one pass of the morph's fade. `body_bottom`/`rest_body_bottom`
/// (see `draw_pill_text_rows`) gate each row to the pill's animated bottom
/// edge, so no text renders outside the body while it grows or shrinks.
/// `skip_title` drops the title row and its symbol slot from the draw (the
/// morph's traveling title replaces them) while keeping the band's height,
/// so the remaining rows sit at their steady positions.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_expanded_pill_text(
    state: &mut OverlayState,
    pixels: &mut [u8],
    content: &MediaEvent,
    width: i32,
    scale: f32,
    content_alpha: f32,
    body_bottom: i32,
    rest_body_bottom: i32,
    skip_title: bool,
) {
    match content {
        MediaEvent::TrackChanged(track) => {
            // The pill pieces were resolved when the content changed (see
            // `resolve_pill_text`); take them out so drawing can borrow
            // `state` mutably, then put them back for the next frame. The
            // on-demand fallback keeps direct draw calls self-sufficient.
            let pill = state.pill_text.take().unwrap_or_else(|| pill_text_from_track(track));
            draw_pill_text_rows(
                state,
                pixels,
                width,
                scale,
                &pill,
                Some(playback_state_for_track(track)),
                track.playback_type,
                content_alpha,
                body_bottom,
                rest_body_bottom,
                skip_title,
            );
            state.pill_text = Some(pill);
        }
        MediaEvent::PlaybackStateChanged(playback, source_app) => {
            let pill = state.pill_text.take().or_else(|| {
                if source_app.is_empty() {
                    None
                } else {
                    state.track_cache.get(source_app).map(pill_text_from_track)
                }
            });
            if let Some(pill) = pill {
                draw_pill_text_rows(
                    state,
                    pixels,
                    width,
                    scale,
                    &pill,
                    Some(*playback),
                    PlaybackType::Unknown,
                    content_alpha,
                    body_bottom,
                    rest_body_bottom,
                    skip_title,
                );
                state.pill_text = Some(pill);
            } else {
                // No cached track (the state change arrived before the first
                // TrackChanged): fall back to the source name with an
                // "Unknown Artist" artist row.
                let appearance = &state.config.appearance;
                let inset = state.aura_inset;
                let padding = (appearance.padding * scale) as i32;
                let art = (appearance.art_size as f32 * scale) as i32;
                let left = inset + padding + art + (12.0 * scale) as i32;
                let right = width - inset - padding;
                let fs_title = appearance.font_size_title * scale;
                let fs_artist = appearance.font_size_artist * scale;
                let text_color = dim_color(pill_text_color(appearance), content_alpha);
                let accent_color = dim_color(appearance.accent_color, content_alpha);
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
                    if !skip_title {
                        let unveil = row_unveil_alpha(body_bottom, rest_body_bottom, title_rect.bottom);
                        if unveil > 0.0 {
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
                                dim_color(text_color, unveil),
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
                                PlaybackType::Unknown,
                                dim_color(accent_color, unveil),
                            );
                        }
                    }
                    let artist_rect = next_band(fs_artist * 0.85 * ROW_HEIGHT);
                    let unveil = row_unveil_alpha(body_bottom, rest_body_bottom, artist_rect.bottom);
                    if unveil > 0.0 {
                        draw_text_line_pixels(
                            &mut state.text_scratch,
                            &mut state.scratch_utf16,
                            pixels,
                            width as usize,
                            "Unknown Artist",
                            &artist_rect,
                            font_artist,
                            h_artist,
                            dim_color([0xCC, 0xCC, 0xCC, 0xFF], content_alpha * unveil),
                            scale,
                            None,
                        );
                    }
                }
            }
        }
        // Never rendered: SessionRejected is filtered out before enqueue.
        MediaEvent::SessionRejected { .. }
        | MediaEvent::SourceGone { .. }
        | MediaEvent::WorkerFailed { .. }
        | MediaEvent::ArtworkBudgetExceeded
        | MediaEvent::ProgressChanged { .. } => {}
    }
}

/// Draws the compact pill's content: `[ART] [TITLE] [APP ICON] [▶]`. The art
/// tile is drawn here (not in `draw_pixels`, which sizes the expanded art),
/// the title occupies exactly `compact_title_viewport` — so marquee text and
/// its edge fade can never render under the app icon or the playback symbol
/// — and the trailing icon and symbol reuse the shared app-icon and
/// playback-symbol drawing. The take/put-back of the resolved pill text
/// mirrors `draw_pill_text_rows`. `content_alpha` is 1.0: a morph frame
/// never calls this — `draw_morph_content` draws the traveling compact
/// elements itself, so the compact layout renders only at rest.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_compact_pill(
    state: &mut OverlayState,
    pixels: &mut [u8],
    content: &MediaEvent,
    width: i32,
    scale: f32,
    content_alpha: f32,
) {
    let inset = state.aura_inset;
    let appearance = &state.config.appearance;
    // The playback symbol's color, guarded exactly like the expanded layout
    // and the morph's traveling symbol (see `draw_pill_text_rows`): the raw
    // palette primary can be too dark against the fill, and the compact and
    // expanded pills must render the same accent, or the symbol visibly
    // changes color when the pill morphs between the two layouts.
    let accent = ensure_contrast(
        state.palette.map(|p| p.primary).unwrap_or(appearance.accent_color),
        pill_fill_bg(state),
        TEXT_CONTRAST_AA,
    );
    let metrics = compact_metrics(&state.config);
    let padding = (appearance.padding * scale).round() as i32;
    // The compact body height, DPI-scaled like every element size below —
    // centering against the logical height would sit the content high in
    // the body at scaled DPI and disagree with the hover morph's shape-0
    // frame (which centers in the scaled body), jumping at the boundary.
    let pill_h = (compact_size(&state.config).1 * scale).round() as i32;
    let (title_vp_left, title_vp_right) = compact_title_viewport(&state.config);

    // Art tile: left-aligned like the expanded pill (inset + padding),
    // vertically centered on the row. This is the only place the compact
    // art is drawn — `draw_pixels` skips its art arms in compact mode, so
    // the halo, cover and rim composite exactly once. The placeholder is
    // drawn here too when no cover is available. During a morph this tile
    // fades out with the compact content while the expanded tile fades in
    // (in `draw_pixels`), so the two cross-fade like every other element.
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
        content_alpha,
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

    let (title, app_icon, playback, playback_type) = match content {
        MediaEvent::TrackChanged(track) => {
            let pill = state.pill_text.take().unwrap_or_else(|| pill_text_from_track(track));
            let (title, app_icon) = (pill.title.clone(), pill.app_icon.clone());
            state.pill_text = Some(pill);
            (title, app_icon, playback_state_for_track(track), track.playback_type)
        }
        MediaEvent::PlaybackStateChanged(playback, source_app) => {
            let pill = state.pill_text.take().or_else(|| {
                if source_app.is_empty() {
                    None
                } else {
                    state.track_cache.get(source_app).map(pill_text_from_track)
                }
            });
            match pill {
                Some(pill) => {
                    let (title, app_icon) = (pill.title.clone(), pill.app_icon.clone());
                    state.pill_text = Some(pill);
                    (title, app_icon, *playback, PlaybackType::Unknown)
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
                    (name, None, *playback, PlaybackType::Unknown)
                }
            }
        }
        // Never rendered: SessionRejected is filtered out before enqueue.
        MediaEvent::SessionRejected { .. }
        | MediaEvent::SourceGone { .. }
        | MediaEvent::WorkerFailed { .. }
        | MediaEvent::ArtworkBudgetExceeded
        | MediaEvent::ProgressChanged { .. } => {
            return;
        }
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
        dim_color(pill_text_color(appearance), content_alpha),
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
            content_alpha,
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
        playback_type,
        accent,
    );
}

/// Draws one morph frame's content: the shared elements — the title and the
/// playback symbol (the artwork travels in `draw_pixels` via
/// `morph_art_tile`) — move from their compact positions to their expanded
/// positions on each axis's own progress, while the layout-exclusive
/// elements fade: the compact app icon dissolves out with `compact_alpha`
/// (0.05..0.20 of shape progress) and the expanded extra rows (artist, meta,
/// app) sweep in with `expanded_alpha` (0.25..0.60), edge-unveiled. The
/// shared elements never fade, so the morph reads as the card unfolding in
/// place instead of two layouts swapping.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_morph_content(
    state: &mut OverlayState,
    pixels: &mut [u8],
    content: &MediaEvent,
    width: i32,
    scale: f32,
    progress: MorphProgress,
    body_bottom: i32,
    rest_body_bottom: i32,
) {
    let shape = progress.width.min(progress.height);
    let inset = state.aura_inset;
    let appearance = &state.config.appearance;
    let compact_opacity = compact_alpha(shape);
    let expanded_opacity = expanded_alpha(shape);

    // The pieces the traveling elements share, resolved once (mirrors
    // `draw_compact_pill` and `draw_expanded_pill_text`).
    let (title, app_icon, playback, playback_type) = match content {
        MediaEvent::TrackChanged(track) => {
            let pill = state.pill_text.take().unwrap_or_else(|| pill_text_from_track(track));
            let (title, app_icon) = (pill.title.clone(), pill.app_icon.clone());
            state.pill_text = Some(pill);
            (title, app_icon, playback_state_for_track(track), track.playback_type)
        }
        MediaEvent::PlaybackStateChanged(playback, source_app) => {
            let pill = state.pill_text.take().or_else(|| {
                if source_app.is_empty() {
                    None
                } else {
                    state.track_cache.get(source_app).map(pill_text_from_track)
                }
            });
            match pill {
                Some(pill) => {
                    let (title, app_icon) = (pill.title.clone(), pill.app_icon.clone());
                    state.pill_text = Some(pill);
                    (title, app_icon, *playback, PlaybackType::Unknown)
                }
                // No cached track (the state change arrived before the first
                // TrackChanged): the source name stands in for the title.
                None => {
                    let name = if !source_app.is_empty() {
                        source_app.clone()
                    } else {
                        state.current_source.clone().unwrap_or_default()
                    };
                    (name, None, *playback, PlaybackType::Unknown)
                }
            }
        }
        // Never rendered: SessionRejected is filtered out before enqueue.
        MediaEvent::SessionRejected { .. }
        | MediaEvent::SourceGone { .. }
        | MediaEvent::WorkerFailed { .. }
        | MediaEvent::ArtworkBudgetExceeded
        | MediaEvent::ProgressChanged { .. } => {
            return;
        }
    };

    // App icon (compact-only): stays at its compact position and dissolves
    // out; it is gone by 0.20, before the expanded app row starts arriving
    // at 0.25, so the two never coexist.
    match app_icon {
        Some(icon) if compact_opacity > 0.0 => {
            let (icon_x, icon_y, icon_size) = morph_icon_pos(&state.config, inset, scale);
            draw_icon_scaled(
                pixels,
                width as usize,
                &icon,
                24,
                icon_x as usize,
                icon_y as usize,
                icon_size as usize,
                compact_opacity,
            );
        }
        _ => {}
    }

    // The traveling title: one instance, moving from the compact band to
    // the expanded title row. The marquee state rides along in the same
    // slot both layouts use.
    if !title.is_empty() {
        let band = morph_title_band(&state.config, inset, width, scale, progress);
        let fs_title = appearance.font_size_title * scale;
        let (font_title, h_title) = state.fonts.font_for(fs_title as i32, true);
        draw_text_line_pixels(
            &mut state.text_scratch,
            &mut state.scratch_utf16,
            pixels,
            width as usize,
            &title,
            &band,
            font_title,
            h_title,
            pill_text_color(appearance),
            scale,
            Some(MarqueeCtx {
                scroll: &mut state.scroll[0],
                strip: &mut state.marquee_strips[0],
            }),
        );
    }

    // The traveling playback symbol: from the compact trailing chain to the
    // expanded title row's right slot. Both layouts draw the same size; the
    // color matches the expanded steady state (contrast-checked).
    let accent_base = pill_accent_base(state, appearance);
    let accent = ensure_contrast(accent_base, pill_fill_bg(state), TEXT_CONTRAST_AA);
    let (symbol_right, symbol_y, symbol_size) = morph_symbol_pos(&state.config, inset, width, scale, progress);
    draw_symbol_pixels(
        pixels,
        width as usize,
        symbol_right,
        symbol_y,
        symbol_size,
        playback,
        playback_type,
        accent,
    );

    // The expanded extra rows (artist, meta, app): fade in with the expanded
    // window and sweep in behind the body edge. The title band keeps its
    // height so the rows sit at their steady positions, but the row itself
    // is not drawn here — the title above is the same element traveling.
    draw_expanded_pill_text(
        state,
        pixels,
        content,
        width,
        scale,
        expanded_opacity,
        body_bottom,
        rest_body_bottom,
        true,
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
pub(super) fn draw_meta_line_pixels(
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
pub(super) fn draw_text_line_pixels(
    text_scratch: &mut Option<TextScratch>,
    scratch_utf16: &mut Vec<u16>,
    pixels: &mut [u8],
    width: usize,
    value: &str,
    rect: &RECT,
    font: HFONT,
    font_height: i32,
    color: [u8; 4],
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
        // Guard restores the previous selection on every exit path. The
        // overflow branch below restores explicitly BEFORE the strip build:
        // `build_marquee_strip` may replace/drop this scratch DC, and any
        // restore after that would target the wrong DC. Leaving our
        // font current in the live scratch would also make the next frame
        // treat it as its `old_font` — a stale handle once the DPI-scoped
        // provider drops it.
        let mut font_guard = SelectedObjectGuard::new(hdc, old_font);
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
        let flags = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX;
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
            // badge must scroll rather than sit truncated. With animations
            // disabled the marquee never runs: overflowing lines render
            // statically, end-ellipsized.
            let motion = crate::winutil::animations_enabled();
            let was_scrolling = ctx.scroll.scrolling;
            ctx.scroll.scrolling = text_w > rw && motion;
            if ctx.scroll.scrolling && !was_scrolling {
                debug!("marquee overflow | text_w={text_w} | draw_w={rw} | title={value}");
            }
            let hold_elapsed = ctx.scroll.started_at.map(|t| t.elapsed()).unwrap_or_default();
            // Edge-fade width in the rendering coordinate space (the same
            // scale the row rects live in), 12 logical px per side.
            let fade_w = MARQUEE_FADE * scale;
            if text_w <= rw || !motion {
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
                // Unselect our font before the strip build may replace or
                // drop this scratch DC; the guard becomes a no-op.
                font_guard.restore();
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
        // `font_guard` restores the previous selection here on the static
        // paths; the overflow branch already restored and the Drop is a no-op.
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
pub(super) fn build_marquee_strip(
    strip: &mut Option<MarqueeStrip>,
    text_scratch: &mut Option<TextScratch>,
    scratch_utf16: &mut Vec<u16>,
    value: &str,
    rw: i32,
    rh: i32,
    font: HFONT,
    font_height: i32,
    color: [u8; 4],
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
        // Same structural restore as the draw path: the strip's DC must never
        // keep a live font selected across returns.
        let _font_guard = SelectedObjectGuard::new(hdc, old_font);
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
pub(super) fn composite_marquee_strip(
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
pub(super) fn edge_fade_factor(x: f32, left: f32, right: f32, fade_left: f32, fade_right: f32) -> f32 {
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
pub(super) fn composite_glyphs(
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

/// RAII restore of a previously-selected GDI object into its DC. Restores in
/// `Drop` (or on an explicit `restore`), so a font selection can never stay
/// current in a long-lived scratch DC across frames: the next
/// frame's `SelectObject` would read that font as its `old_font`, and if a
/// DPI swap has deleted it in the meantime, the restore would hand the DC a
/// dangling handle. Callers that hand the DC to code which may replace it
/// (`text_scratch_for` drops the scratch on growth) call `restore` first:
/// afterwards the Drop is a no-op, because restoring against a replaced DC
/// would select into the wrong object.
struct SelectedObjectGuard {
    hdc: HDC,
    previous: HGDIOBJ,
    restored: bool,
}

impl SelectedObjectGuard {
    fn new(hdc: HDC, previous: HGDIOBJ) -> Self {
        Self {
            hdc,
            previous,
            restored: false,
        }
    }

    fn restore(&mut self) {
        if !self.restored {
            unsafe {
                let _ = SelectObject(self.hdc, self.previous);
            }
            self.restored = true;
        }
    }
}

impl Drop for SelectedObjectGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Returns the scratch DC + DIB for GDI text, growing it when a larger text
/// row arrives. The DIB is kept across frames and released at window
/// destruction.
pub(super) fn text_scratch_for(
    scratch: &mut Option<TextScratch>,
    width: i32,
    height: i32,
) -> Result<(HDC, *mut c_void, i32, i32)> {
    if let Some(cached) = scratch
        && cached.width >= width
        && cached.height >= height
    {
        return Ok((cached.hdc, cached.bits, cached.width, cached.height));
    }
    // Too small or absent: dropping the old scratch unselects its bitmap and
    // frees the DIB (see `Drop for TextScratch`); a fresh buffer is created
    // below.
    *scratch = None;
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
pub(super) fn composite_pm(pixels: &mut [u8], width: usize, x: usize, y: usize, rgb: [u8; 3], alpha: u32) {
    if width == 0 || x >= width || y >= pixels.len() / width / 4 {
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

/// Converts the worker's premultiplied BGRA artwork (square, fixed
/// `ARTWORK_DECODE` size) into the straight RGBA buffer the overlay composites
/// and palettizes from. Shared with the main window, which derives its accent
/// from the same decode.
/// Runs once per cover change, keyed by the decoded pixels in `ensure_art`.
/// The result is always a perfect square; `draw_art_scaled` derives the side
/// from the buffer length.
pub(crate) fn pm_bgra_to_rgba(pm: &[u8]) -> Option<Vec<u8>> {
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
pub(super) fn composite(pixels: &mut [u8], width: usize, x: usize, y: usize, rgb: [u8; 3], alpha: u32) {
    if width == 0 || x >= width || y >= pixels.len() / width / 4 {
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
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_icon_scaled(
    pixels: &mut [u8],
    width: usize,
    icon: &[u8],
    icon_size: usize,
    x: usize,
    y: usize,
    dest_size: usize,
    content_alpha: f32,
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
                let alpha = (a as f32 * content_alpha) as u32;
                if alpha > 0 {
                    // Premultiply like the glyph composite: `composite_pm`
                    // blends src + dst*(1 - src_a), so an unpremultiplied
                    // color would bloom at full strength during fades and, at
                    // alpha 0, ADD a full-color ghost with zero alpha into
                    // the buffer. Later glyphs blend over that ghost, which
                    // left a smudge of icon color on the expanded title at
                    // the morph end (the compact icon never truly vanished).
                    composite_pm(
                        pixels,
                        width,
                        x + dx,
                        y + dy,
                        [
                            (r as u32 * alpha / 255) as u8,
                            (g as u32 * alpha / 255) as u8,
                            (b as u32 * alpha / 255) as u8,
                        ],
                        alpha,
                    );
                }
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
pub(super) fn draw_source_app_row(
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
    content_alpha: f32,
    marquee: Option<MarqueeCtx<'_>>,
) {
    if let Some(icon) = app_icon {
        // The source bitmap is always 24x24; the destination size is the
        // 16px base scaled for DPI, clamped so it never overflows the band.
        let band_h = (rect.bottom - rect.top) as usize;
        let icon_size = ((16.0 * scale).round() as usize).min(band_h);
        let icon_x = rect.left as usize;
        let icon_y = rect.top as usize + (band_h - icon_size) / 2;
        draw_icon_scaled(pixels, width, icon, 24, icon_x, icon_y, icon_size, content_alpha);
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
            scale,
            marquee,
        );
    }
}
/// Signed distance to a rounded rectangle's boundary at pixel (x, y),
/// negative inside the shape. Used for the pill's outer shape, the
/// placeholder art and the album-artwork corner mask.
pub(super) fn round_rect_signed_dist(x: f32, y: f32, width: f32, height: f32, radius: f32) -> f32 {
    let radius = radius.min(width / 2.0).min(height / 2.0);
    let qx = ((x + 0.5) - width / 2.0).abs() - (width / 2.0 - radius);
    let qy = ((y + 0.5) - height / 2.0).abs() - (height / 2.0 - radius);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius
}

/// Anti-aliased coverage (0..=1) of a rounded rectangle at pixel (x, y):
/// signed distance to the boundary smoothed over a 1.5 px band via
/// Hermite interpolation. Used for the pill's outer shape, the placeholder
/// art and the album-artwork corner mask.
pub(super) fn round_rect_coverage(x: f32, y: f32, width: f32, height: f32, radius: f32) -> f32 {
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
pub(super) fn round_rect_coverage_fast(x: f32, y: f32, width: f32, height: f32, radius: f32) -> Option<f32> {
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
pub(super) fn round_rect_coverage_supersampled(x: f32, y: f32, width: f32, height: f32, radius: f32) -> f32 {
    if let Some(coverage) = round_rect_coverage_fast(x, y, width, height, radius) {
        return coverage;
    }
    let cov = |dx: f32, dy: f32| round_rect_coverage(x + dx, y + dy, width, height, radius);
    (cov(-0.35, -0.35) + cov(0.35, -0.35) + cov(-0.35, 0.35) + cov(0.35, 0.35)) * 0.25
}

/// Soft multi-color glow around the pill's boundary. The DIB is inflated by
/// `AURA_HALO_LOGICAL` (scaled by DPI × shape) on every side so the halo can
/// extend outside the pill into the desktop background.
pub(super) const AURA_MARGIN_LOGICAL: f32 = 10.0;
/// Outer extent of the synthetic aura glow, in logical px per side. The
/// falloff curve (see `AURA_DECAY`) is normalized by `AURA_MARGIN_LOGICAL`,
/// so the glow's shape is independent of where it ends: shrinking the halo
/// truncates the faint outer tail instead of re-shaping the visible part.
pub(super) const AURA_HALO_LOGICAL: f32 = 6.0;
/// Peak opacity of the outer aura ring, at the pill boundary. Capped at ~140
/// so the glow stays soft beneath the pill body's supersampled edge instead
/// of producing a hard 0→255 step at the boundary.
pub(super) const AURA_PEAK_ALPHA: f32 = 140.0;
/// Exponential decay constant. The falloff is exp(-AURA_DECAY * d /
/// (AURA_MARGIN_LOGICAL * scale)) per physical px, so the curve's per-px
/// rate is fixed by these two constants and does not change when the halo
/// extent shrinks.
pub(super) const AURA_DECAY: f32 = 3.0;

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_aura(
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
pub(super) fn circle_coverage(x: f32, y: f32, size: f32) -> f32 {
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
pub(super) fn clock_icon_coverage(x: f32, y: f32, size: f32) -> f32 {
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
pub(super) fn draw_clock_icon_pixels(pixels: &mut [u8], width: usize, x: i32, y: i32, size: f32, color: [u8; 4]) {
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

pub(super) fn draw_placeholder(pixels: &mut [u8], width: usize, x: usize, y: usize, size: usize, color: [u8; 4]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::winutil::wide;
    use windows::Win32::Foundation::FALSE;
    use windows::Win32::Graphics::Gdi::{
        ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, DeleteObject,
        FF_DONTCARE, GetCurrentObject, OBJ_FONT, OUT_DEFAULT_PRECIS,
    };

    /// A real Segoe UI HFONT sized like the pill's drawing fonts.
    unsafe fn test_font() -> HFONT {
        let name = wide("Segoe UI");
        unsafe {
            CreateFontW(
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
                PCWSTR(name.as_ptr()),
            )
        }
    }

    #[test]
    fn selected_object_guard_restores_the_prior_selection_on_drop() {
        // The overflow branch used to return without restoring its
        // selection, so the live draw font stayed current in the persistent
        // scratch DC. The next frame's `SelectObject` then read that font as
        // its `old_font`, and a DPI swap could delete the very font the DC
        // still named. The guard must put the prior object back: the DC's
        // current font after the guard is the pre-select one, and the
        // swapped-in font is unselected and deletable.
        unsafe {
            let hdc = CreateCompatibleDC(None);
            assert!(!hdc.0.is_null());
            let font = test_font();
            let before = GetCurrentObject(hdc, OBJ_FONT);
            let old_font = SelectObject(hdc, font);
            assert_ne!(
                GetCurrentObject(hdc, OBJ_FONT).0,
                before.0,
                "the font must be current while selected"
            );
            {
                let _guard = SelectedObjectGuard::new(hdc, old_font);
            }
            assert_eq!(
                GetCurrentObject(hdc, OBJ_FONT).0,
                before.0,
                "the guard must restore the prior selection on drop"
            );
            assert!(DeleteObject(font) != FALSE, "an unselected font must delete cleanly");
            let _ = DeleteDC(hdc);
        }
    }

    #[test]
    fn overflow_branch_restores_before_the_scratch_replacement() {
        // The marquee overflow path must unselect its font BEFORE the strip
        // rebuild: `text_scratch_for` drops the scratch (and its DC) when the
        // strip needs a wider buffer, and a restore after that would select
        // into a replaced DC. The guard's restored flag pins the
        // ordering — no GDI handle-table allocation luck involved — and the
        // font must never be left selected somewhere undeletable.
        unsafe {
            let font = test_font();
            let mut scratch: Option<TextScratch> = None;
            let (hdc, _, _, _) = text_scratch_for(&mut scratch, 32, 16).unwrap();
            let old_font = SelectObject(hdc, font);
            let mut guard = SelectedObjectGuard::new(hdc, old_font);
            guard.restore();
            assert!(
                guard.restored,
                "the overflow branch must restore before the strip rebuild"
            );
            // The strip rebuild needs a text_w-wide scratch, which grows the
            // scratch and replaces its DC.
            let (hdc2, _, _, _) = text_scratch_for(&mut scratch, 320, 16).unwrap();
            assert_eq!(
                scratch.as_ref().unwrap().width,
                320,
                "the scratch must have been replaced with the wider buffer"
            );
            // The guard is a no-op from here (already restored), so the Drop
            // never touches the replaced DC.
            drop(guard);
            assert!(
                GetCurrentObject(hdc2, OBJ_FONT).0 != font.0,
                "the fresh strip DC must not carry our font"
            );
            assert!(DeleteObject(font) != FALSE, "the font must delete cleanly");
        }
    }

    #[test]
    fn repeated_font_swaps_leave_no_selection_stuck_in_the_scratch() {
        // A hundred DPI-style swaps against ONE persistent scratch DC (like
        // the real one): each generation restores the baseline selection and
        // deletes its font. A frame that skipped its restore would leave the
        // generation's font current in the long-lived DC, so the next frame's
        // `SelectObject` would read it as `old_font` — a stale handle once the
        // provider drops it.
        unsafe {
            let hdc = CreateCompatibleDC(None);
            assert!(!hdc.0.is_null());
            let baseline = GetCurrentObject(hdc, OBJ_FONT);
            for i in 0..100 {
                let font = test_font();
                let old_font = SelectObject(hdc, font);
                {
                    let _guard = SelectedObjectGuard::new(hdc, old_font);
                }
                assert_eq!(
                    GetCurrentObject(hdc, OBJ_FONT).0,
                    baseline.0,
                    "generation {i} left its font current in the scratch DC"
                );
                assert!(DeleteObject(font) != FALSE, "generation {i}'s font failed to delete");
            }
            let _ = DeleteDC(hdc);
        }
    }
}
