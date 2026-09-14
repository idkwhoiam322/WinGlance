//! Optional Windows 11 system backdrop for the pill body.
//!
//! The existing layered HWND remains the foreground/content renderer. This
//! companion HWND sits directly behind it and asks DWM for the transient
//! system backdrop (Desktop Acrylic on Windows 11 22H2+). It is created only
//! when the user enables the feature, never receives input or activation, and
//! fails closed: unsupported systems keep the existing solid pill.

use crate::winapi::set_window_pos;
use crate::winutil::{register_class_once, system_preferences, wide};
use log::{debug, warn};
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::OnceLock;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DwmSetWindowAttribute,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, DefWindowProcW, DestroyWindow, HTTRANSPARENT, MA_NOACTIVATE, SW_HIDE, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, ShowWindow, WM_MOUSEACTIVATE, WM_NCCREATE, WM_NCHITTEST, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

static BACKDROP_CLASS_REGISTERED: OnceLock<()> = OnceLock::new();

#[derive(Default)]
pub(super) struct Backdrop {
    hwnd: HWND,
    unavailable: bool,
    enabled: bool,
}

impl Backdrop {
    /// Ensures that a usable backdrop window exists when the user request and
    /// system accessibility preferences permit translucent decoration.
    /// Returns true only when the renderer may safely lower the solid fill's
    /// alpha because a DWM backdrop is available underneath it.
    pub(super) fn prepare(&mut self, requested: bool) -> bool {
        let prefs = system_preferences();
        if !requested || prefs.high_contrast || prefs.disable_overlapped_content {
            self.hide();
            return false;
        }
        if !self.hwnd.0.is_null() {
            self.enabled = true;
            return true;
        }
        if self.unavailable {
            self.enabled = false;
            return false;
        }
        match create_backdrop_window() {
            Ok(hwnd) => {
                self.hwnd = hwnd;
                self.enabled = true;
                debug!("Windows 11 acrylic backdrop initialized");
                true
            }
            Err(error) => {
                self.unavailable = true;
                self.enabled = false;
                warn!("Windows 11 acrylic backdrop unavailable; keeping solid pill: {error}");
                false
            }
        }
    }

    pub(super) fn active(&self) -> bool {
        self.enabled && !self.hwnd.0.is_null()
    }

    /// Places the body-only backdrop directly below the layered overlay.
    /// `x/y/w/h` exclude the aura inset so the glow can continue to bleed
    /// beyond the glass boundary while the material stays clipped to the pill.
    pub(super) fn sync(&mut self, overlay: HWND, x: i32, y: i32, w: i32, h: i32) {
        if !self.active() || w <= 0 || h <= 0 {
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
        if !self.hwnd.0.is_null() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
            self.hwnd = HWND::default();
        }
    }
}

fn create_backdrop_window() -> Result<HWND, windows::core::Error> {
    let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class_name = wide("WinGlanceAcrylicBackdrop");
    register_class_once(
        &BACKDROP_CLASS_REGISTERED,
        instance,
        &class_name,
        Some(backdrop_wndproc),
        || None,
        "the acrylic backdrop window",
    )?;

    let hwnd = unsafe {
        crate::winapi::create_window(
            WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("WinGlance backdrop").as_ptr()),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            instance,
            None,
        )?
    };

    let backdrop = DWMSBT_TRANSIENTWINDOW;
    let corner = DWMWCP_ROUND;
    let backdrop_result = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const _ as *const c_void,
            size_of_val_u32(&backdrop),
        )
    };
    if let Err(error) = backdrop_result {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return Err(error);
    }
    let corner_result = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            size_of_val_u32(&corner),
        )
    };
    if let Err(error) = corner_result {
        debug!("DwmSetWindowAttribute(WINDOW_CORNER_PREFERENCE) failed: {error}");
    }
    Ok(hwnd)
}

fn size_of_val_u32<T>(_: &T) -> u32 {
    size_of::<T>() as u32
}

unsafe extern "system" fn backdrop_wndproc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCCREATE => {
            // The class has no per-instance state; accept creation explicitly.
            let _ = lparam.0 as *const CREATESTRUCTW;
            LRESULT(1)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}
