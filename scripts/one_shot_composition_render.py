from pathlib import Path
import re


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label} count={count}")
    return text.replace(old, new, 1)


# Render integration/tuning.
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
text = replace_once(text, old, new, "content_h anchor")

old = '''    state
        .backdrop
        .sync(state.hwnd, position.x + inset, position.y + inset, width, height);
    // Re-assert topmost'''
new = '''    // Re-assert topmost'''
text = replace_once(text, old, new, "late backdrop sync")

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
text = replace_once(text, old, new, "edge call")

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
text = replace_once(text, old, new, "glass fill")

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
text = replace_once(text, old, new, "edge signature")

old = '''                let alpha = (peak * coverage).round() as u32;'''
new = '''                let alpha = (peak * coverage * alpha_scale).round() as u32;'''
text = replace_once(text, old, new, "edge alpha")
p.write_text(text, encoding="utf-8")


# Split Composition construction into small helpers. rust-code-analysis counts
# each fallible WinRT call as a decision point; keeping one large constructor
# would violate the repository's existing cyclomatic ratchet even though the
# operations themselves are a straight-line setup sequence.
p = Path("src/overlay/backdrop.rs")
text = p.read_text(encoding="utf-8")
helpers = r'''fn create_dispatcher_queue() -> Option<DispatcherQueueController> {
    let options = DispatcherQueueOptions {
        dwSize: size_of::<DispatcherQueueOptions>() as u32,
        threadType: DQTYPE_THREAD_CURRENT,
        apartmentType: DQTAT_COM_NONE,
    };
    // A queue may already belong to this UI thread. In that case creation
    // fails, but the existing queue is sufficient for the compositor.
    unsafe { CreateDispatcherQueueController(options) }.ok()
}

fn create_composition_target(
    hwnd: HWND,
) -> WinResult<(Option<DispatcherQueueController>, Compositor, DesktopWindowTarget)> {
    let queue = create_dispatcher_queue();
    let compositor = Compositor::new()?;
    let interop: ICompositorDesktopInterop = compositor.cast()?;
    let target = unsafe { interop.CreateDesktopWindowTarget(hwnd, true)? };
    Ok((queue, compositor, target))
}

fn configure_blur_visual(compositor: &Compositor, blur_visual: &SpriteVisual) -> WinResult<()> {
    let parameter_name = HSTRING::from("backdrop");
    let parameter = CompositionEffectSourceParameter::Create(&parameter_name)?;
    let effect_source: IGraphicsEffectSource = parameter.cast()?;
    let effect: IGraphicsEffect = GaussianBlurEffect {
        source: effect_source,
        blur_amount: BLUR_AMOUNT,
    }
    .into();
    let factory = compositor.CreateEffectFactory(&effect)?;
    let effect_brush = factory.CreateBrush()?;
    // HostBackdrop samples behind the HWND before this window is drawn. The
    // ordinary Backdrop brush only samples visuals already inside the target,
    // which is not the desktop/game content this companion window needs.
    let backdrop_brush = compositor.CreateHostBackdropBrush()?;
    effect_brush.SetSourceParameter(&parameter_name, &backdrop_brush)?;
    blur_visual.SetBrush(&effect_brush)?;
    Ok(())
}

fn create_glass_visuals(
    compositor: &Compositor,
    target: &DesktopWindowTarget,
    tint: [u8; 3],
) -> WinResult<(
    ContainerVisual,
    SpriteVisual,
    SpriteVisual,
    CompositionColorBrush,
    CompositionRoundedRectangleGeometry,
)> {
    let root = compositor.CreateContainerVisual()?;
    let blur_visual = compositor.CreateSpriteVisual()?;
    let tint_visual = compositor.CreateSpriteVisual()?;
    configure_blur_visual(compositor, &blur_visual)?;

    let tint_brush = compositor.CreateColorBrushWithColor(glass_color(tint))?;
    tint_visual.SetBrush(&tint_brush)?;

    let clip_geometry = compositor.CreateRoundedRectangleGeometry()?;
    let clip = compositor.CreateGeometricClipWithGeometry(&clip_geometry)?;
    root.SetClip(&clip)?;

    let children = root.Children()?;
    children.InsertAtBottom(&blur_visual)?;
    children.InsertAtTop(&tint_visual)?;
    target.SetRoot(&root)?;
    Ok((root, blur_visual, tint_visual, tint_brush, clip_geometry))
}

'''
anchor = "impl CompositionGlass {\n"
text = replace_once(text, anchor, helpers + anchor, "CompositionGlass impl anchor")
pattern = re.compile(r'''    fn new\(hwnd: HWND\) -> WinResult<Self> \{.*?\n    \}\n\n(?=    fn sync\()''', re.S)
replacement = r'''    fn new(hwnd: HWND) -> WinResult<Self> {
        let (queue, compositor, target) = create_composition_target(hwnd)?;
        let tint = [48, 48, 52];
        let (root, blur_visual, tint_visual, tint_brush, clip_geometry) =
            create_glass_visuals(&compositor, &target, tint)?;
        Ok(Self {
            _queue: queue,
            _compositor: compositor,
            _target: target,
            root,
            blur_visual,
            tint_visual,
            tint_brush,
            clip_geometry,
            geometry: None,
            tint,
            opacity: 255,
        })
    }

'''
text, count = pattern.subn(replacement, text, count=1)
if count != 1:
    raise SystemExit(f"CompositionGlass::new replacement count={count}")
p.write_text(text, encoding="utf-8")

print("composition render tuning and quality refactor applied")
