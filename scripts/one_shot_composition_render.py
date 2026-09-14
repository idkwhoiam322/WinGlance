from pathlib import Path

p = Path("src/overlay/render.rs")
text = p.read_text(encoding="utf-8")

old = '''    let content_h = if scale_factor == 1.0 {
        height
    } else {
        (height as f32 / scale_factor).round().max(1.0) as i32
    };
    let content_buf_w = (content_w + inset * 2).max(1);
'''
new = '''    let content_h = if scale_factor == 1.0 {
        height
    } else {
        (height as f32 / scale_factor).round().max(1.0) as i32
    };
    // Keep the custom Composition material in exact lockstep with the body,
    // including morph/bounce radius, palette tint and the foreground fade.
    // Sync happens before rasterization so a Composition failure can disable
    // the material immediately and this same frame falls back to the normal
    // solid renderer rather than flashing an under-filled pill.
    let glass_radius = frame_radius(&state.config, scale, compact, morph) * scale_factor;
    let glass_fill = pill_fill_bg(state);
    state.backdrop.sync(
        state.hwnd,
        position.x + inset,
        position.y + inset,
        width,
        height,
        glass_radius,
        [glass_fill[0], glass_fill[1], glass_fill[2]],
        alpha,
    );
    let content_buf_w = (content_w + inset * 2).max(1);
'''
if text.count(old) != 1:
    raise SystemExit(f"content_h anchor count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''    if state.backdrop.active() {
        state
            .backdrop
            .sync(state.hwnd, position.x + inset, position.y + inset, width, height);
    }
    // Re-assert topmost'''
new = '''    // Re-assert topmost'''
if text.count(old) != 1:
    raise SystemExit(f"late backdrop sync count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''    draw_edge_stroke(pixels, width, inset, pill_w, pill_h, radius, scale);'''
new = '''    draw_edge_stroke(
        pixels,
        width,
        inset,
        pill_w,
        pill_h,
        radius,
        scale,
        material_edge_strength(state),
    );'''
if text.count(old) != 1:
    raise SystemExit(f"edge call count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''    if state.backdrop.active() {
        // Keep enough tint for text contrast while allowing the system
        // material to read through. The final alpha is also multiplied by
        // the normal frame/persistent-fade alpha later in the pipeline.
        fill[3] = fill[3].min(176);
    }
'''
new = '''    if state.backdrop.active() {
        // Composition owns the material. This foreground layer contributes
        // only a faint palette-aware wash; a mostly-opaque fill would bury
        // the live blur and recreate the grey-card look glass mode replaces.
        fill[3] = fill[3].min(36);
    }
'''
if text.count(old) != 1:
    raise SystemExit(f"glass fill count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''pub(super) fn draw_edge_stroke(
    pixels: &mut [u8],
    width: usize,
    inset: usize,
    pill_w: usize,
    pill_h: usize,
    radius: f32,
    scale: f32,
) {'''
new = '''fn material_edge_strength(state: &OverlayState) -> f32 {
    if state.backdrop.active() { 0.48 } else { 1.0 }
}

pub(super) fn draw_edge_stroke(
    pixels: &mut [u8],
    width: usize,
    inset: usize,
    pill_w: usize,
    pill_h: usize,
    radius: f32,
    scale: f32,
    alpha_scale: f32,
) {'''
if text.count(old) != 1:
    raise SystemExit(f"edge signature count={text.count(old)}")
text = text.replace(old, new, 1)

old = '''                let alpha = (peak * coverage).round() as u32;'''
new = '''                let alpha = (peak * coverage * alpha_scale).round() as u32;'''
if text.count(old) != 1:
    raise SystemExit(f"edge alpha count={text.count(old)}")
text = text.replace(old, new, 1)

p.write_text(text, encoding="utf-8")
print("composition render tuning applied")
