//! Optional Windows 11 Composition glass for the pill body.
//!
//! The existing layered HWND remains the foreground/content renderer. This
//! companion HWND sits directly behind it and hosts a Windows.UI.Composition
//! visual tree: live backdrop -> Gaussian blur -> restrained color tint. The
//! foreground renderer keeps artwork, text, progress, edge light and the
//! aura/comet, so the aura can bleed outside the clipped glass silhouette.
//! The companion is created only when the user enables the feature, never
//! receives input or activation, and fails closed to the existing solid pill.

use crate::winapi::set_window_pos;
use crate::winutil::{register_class_once, system_preferences, wide};
use log::{debug, warn};
use std::mem::size_of;
use std::sync::OnceLock;
use windows::Foundation::Numerics::Vector2;
use windows::Foundation::{IPropertyValue, PropertyValue};
use windows::Graphics::Effects::{
    IGraphicsEffect, IGraphicsEffect_Impl, IGraphicsEffectSource, IGraphicsEffectSource_Impl,
};
use windows::System::DispatcherQueueController;
use windows::UI::Color;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{
    CompositionColorBrush, CompositionEffectSourceParameter, CompositionRoundedRectangleGeometry, Compositor,
    ContainerVisual, SpriteVisual,
};
use windows::Win32::Foundation::{E_INVALIDARG, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::System::WinRT::Graphics::Direct2D::{
    GRAPHICS_EFFECT_PROPERTY_MAPPING, GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT, IGraphicsEffectD2D1Interop,
    IGraphicsEffectD2D1Interop_Impl,
};
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT, DispatcherQueueOptions,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, DefWindowProcW, DestroyWindow, HTTRANSPARENT, MA_NOACTIVATE, SW_HIDE, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, ShowWindow, WM_MOUSEACTIVATE, WM_NCCREATE, WM_NCHITTEST, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{Error, GUID, HSTRING, Interface, PCWSTR, Result as WinResult};

static BACKDROP_CLASS_REGISTERED: OnceLock<()> = OnceLock::new();
const BLUR_AMOUNT: f32 = 24.0;
const TINT_ALPHA: u8 = 34;

#[windows::core::implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
struct GaussianBlurEffect {
    source: IGraphicsEffectSource,
    blur_amount: f32,
}

impl IGraphicsEffectSource_Impl for GaussianBlurEffect_Impl {}

impl IGraphicsEffect_Impl for GaussianBlurEffect_Impl {
    fn Name(&self) -> WinResult<HSTRING> {
        Ok(HSTRING::from("WinGlanceGaussianBlur"))
    }

    fn SetName(&self, _name: &HSTRING) -> WinResult<()> {
        Ok(())
    }
}

impl IGraphicsEffectD2D1Interop_Impl for GaussianBlurEffect_Impl {
    fn GetEffectId(&self) -> WinResult<GUID> {
        Ok(GUID::from_u128(0x1feb6d69_2fe6_4ac9_8c58_1d7f93e7a6a5))
    }

    fn GetNamedPropertyMapping(
        &self,
        _name: &PCWSTR,
        index: *mut u32,
        mapping: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
    ) -> WinResult<()> {
        if index.is_null() || mapping.is_null() {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        // BlurAmount is the only property WinGlance ever exposes to the
        // composition factory. The graph itself is immutable after creation.
        unsafe {
            *index = 0;
            *mapping = GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT;
        }
        Ok(())
    }

    fn GetPropertyCount(&self) -> WinResult<u32> {
        Ok(3)
    }

    fn GetProperty(&self, index: u32) -> WinResult<IPropertyValue> {
        let value = match index {
            // D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION
            0 => PropertyValue::CreateSingle(self.blur_amount)?,
            // D2D1_GAUSSIANBLUR_PROP_OPTIMIZATION = BALANCED
            1 => PropertyValue::CreateUInt32(1)?,
            // D2D1_GAUSSIANBLUR_PROP_BORDER_MODE = HARD, preventing a dark
            // transparent fringe around the rounded clip.
            2 => PropertyValue::CreateUInt32(1)?,
            _ => return Err(Error::from_hresult(E_INVALIDARG)),
        };
        value.cast()
    }

    fn GetSource(&self, index: u32) -> WinResult<IGraphicsEffectSource> {
        if index == 0 {
            Ok(self.source.clone())
        } else {
            Err(Error::from_hresult(E_INVALIDARG))
        }
    }

    fn GetSourceCount(&self) -> WinResult<u32> {
        Ok(1)
    }
}

struct CompositionGlass {
    _queue: Option<DispatcherQueueController>,
    _compositor: Compositor,
    _target: DesktopWindowTarget,
    root: ContainerVisual,
    blur_visual: SpriteVisual,
    tint_visual: SpriteVisual,
    tint_brush: CompositionColorBrush,
    clip_geometry: CompositionRoundedRectangleGeometry,
    geometry: Option<(i32, i32, u32)>,
    tint: [u8; 3],
    opacity: u8,
}

fn create_dispatcher_queue() -> Option<DispatcherQueueController> {
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

impl CompositionGlass {
    fn new(hwnd: HWND) -> WinResult<Self> {
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

    fn sync(&mut self, w: i32, h: i32, radius: f32, tint: [u8; 3], opacity: u8) -> WinResult<()> {
        let radius = radius.clamp(0.0, w.min(h).max(0) as f32 * 0.5);
        let radius_key = radius.to_bits();
        if self.geometry != Some((w, h, radius_key)) {
            let size = Vector2 {
                X: w.max(1) as f32,
                Y: h.max(1) as f32,
            };
            let corner = Vector2 { X: radius, Y: radius };
            self.root.SetSize(size)?;
            self.blur_visual.SetSize(size)?;
            self.tint_visual.SetSize(size)?;
            self.clip_geometry.SetSize(size)?;
            self.clip_geometry.SetCornerRadius(corner)?;
            self.geometry = Some((w, h, radius_key));
        }
        if self.tint != tint {
            self.tint_brush.SetColor(glass_color(tint))?;
            self.tint = tint;
        }
        if self.opacity != opacity {
            self.root.SetOpacity(opacity as f32 / 255.0)?;
            self.opacity = opacity;
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Backdrop {
    hwnd: HWND,
    composition: Option<CompositionGlass>,
    unavailable: bool,
    enabled: bool,
}

impl Backdrop {
    /// Ensures a usable Composition backdrop exists when the user request and
    /// system accessibility preferences permit translucent decoration.
    pub(super) fn prepare(&mut self, requested: bool) -> bool {
        let prefs = system_preferences();
        if !requested || prefs.high_contrast || prefs.disable_overlapped_content {
            self.hide();
            return false;
        }
        if !self.hwnd.0.is_null() && self.composition.is_some() {
            self.enabled = true;
            return true;
        }
        if self.unavailable {
            self.enabled = false;
            return false;
        }
        match create_backdrop_window().and_then(|hwnd| match CompositionGlass::new(hwnd) {
            Ok(composition) => Ok((hwnd, composition)),
            Err(error) => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                Err(error)
            }
        }) {
            Ok((hwnd, composition)) => {
                self.hwnd = hwnd;
                self.composition = Some(composition);
                self.enabled = true;
                debug!("Windows 11 Composition glass initialized");
                true
            }
            Err(error) => {
                self.unavailable = true;
                self.enabled = false;
                warn!("Windows 11 Composition glass unavailable; keeping solid pill: {error}");
                false
            }
        }
    }

    pub(super) fn active(&self) -> bool {
        self.enabled && !self.hwnd.0.is_null() && self.composition.is_some()
    }

    /// Places and updates the body-only material directly below the layered
    /// overlay. x/y/w/h exclude the aura inset so the existing glow and comet
    /// remain free to bleed outside the clipped glass boundary.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn sync(
        &mut self,
        overlay: HWND,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        radius: f32,
        tint: [u8; 3],
        opacity: u8,
    ) {
        if !self.active() || w <= 0 || h <= 0 {
            return;
        }
        let visual_result = self
            .composition
            .as_mut()
            .expect("active backdrop has composition")
            .sync(w, h, radius, tint, opacity);
        if let Err(error) = visual_result {
            warn!("Composition glass update failed; reverting to solid pill: {error}");
            self.unavailable = true;
            self.hide();
            return;
        }
        unsafe {
            let _ = set_window_pos(self.hwnd, overlay, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
    }

    pub(super) fn hide(&mut self) {
        self.enabled = false;
        if !self.hwnd.0.is_null() {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }
}

impl Drop for Backdrop {
    fn drop(&mut self) {
        self.composition = None;
        if !self.hwnd.0.is_null() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
            self.hwnd = HWND::default();
        }
    }
}

fn glass_color(rgb: [u8; 3]) -> Color {
    Color {
        A: TINT_ALPHA,
        R: rgb[0],
        G: rgb[1],
        B: rgb[2],
    }
}

fn create_backdrop_window() -> WinResult<HWND> {
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class_name = wide("WinGlanceCompositionGlass");
    register_class_once(
        &BACKDROP_CLASS_REGISTERED,
        instance,
        &class_name,
        Some(backdrop_wndproc),
        || None,
        "the Composition glass window",
    )?;

    unsafe {
        crate::winapi::create_window(
            WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("WinGlance glass").as_ptr()),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            instance,
            None,
        )
    }
}

unsafe extern "system" fn backdrop_wndproc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCCREATE => {
            let _ = lparam.0 as *const CREATESTRUCTW;
            LRESULT(1)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
