use crate::winapi::{create_font, delete_object, select_object};
use crate::winutil::wide;
use std::cell::RefCell;
use std::collections::HashMap;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER,
    DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DrawTextW, FF_DONTCARE, GetTextMetricsW, HDC, HFONT,
    OUT_DEFAULT_PRECIS, SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT,
};
use windows::core::PCWSTR;

/// Cache key: DPI, pixel height, and boldness. Quality is a constant
/// (`ANTIALIASED_QUALITY`), so it is not part of the key.
type FontKey = (u32, i32, bool);

/// Per-window font cache, DPI-scoped.
///
/// WinGlance draws text in two windows: the SMTC pill overlay and the main
/// settings/history window. Both previously reached into a single process-wide
/// `FONT_CACHE` (`static Mutex<HashMap>`): that cache was only drained when the
/// *overlay* observed a DPI change on `render()`, so moving the settings window
/// to another monitor never invalidated its fonts (stale sizes, S1), and the
/// cache was never drained at process exit (handle leak, S2).
///
/// `FontProvider` is owned per window state instead. The overlay swaps in a new
/// provider on DPI change from `render()`, and the main window does the same in
/// `on_dpi_changed` (`WM_DPICHANGED`). Replacing the provider drops the old one,
/// whose `Drop` deletes the stale HFONTs for that DPI.
///
/// The cache is interior-mutable only to preserve the `&self` paint-helper API:
/// each provider has exactly one UI-thread owner, so a mutex would add lock/
/// poison machinery without protecting any cross-thread access. `RefCell`
/// makes that single-thread ownership explicit and keeps the change local.
pub(crate) struct FontProvider {
    dpi: u32,
    cache: RefCell<HashMap<FontKey, (HFONT, i32)>>,
}

impl FontProvider {
    pub(crate) fn new(dpi: u32) -> Self {
        Self {
            dpi,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Returns the cached Segoe UI (`ANTIALIASED_QUALITY`) HFONT for the given
    /// pixel height and boldness, creating and caching it on first use. The
    /// paired `tmHeight` text metric (a pure function of the key) is returned
    /// alongside so callers never re-query it per text row. Handles stay valid
    /// until the provider is replaced on a DPI change, whose `Drop` frees them.
    pub(crate) fn font_for(&self, height: i32, bold: bool) -> (HFONT, i32) {
        let key = (self.dpi, height, bold);
        let mut guard = self.cache.borrow_mut();
        if let Some((font, tm_height)) = guard.get(&key) {
            return (*font, *tm_height);
        }
        let font_name = wide("Segoe UI");
        let font = unsafe {
            create_font(
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
                if !hdc.is_invalid() {
                    let _dc = crate::winutil::DcGuard(hdc);
                    let old_font = select_object(hdc, font);
                    let mut tm = TEXTMETRICW::default();
                    if GetTextMetricsW(hdc, &mut tm).as_bool() {
                        tm_height = tm.tmHeight;
                    }
                    select_object(hdc, old_font);
                }
            }
            guard.insert(key, (font, tm_height));
        }
        (font, tm_height)
    }

    pub(crate) fn dpi(&self) -> u32 {
        self.dpi
    }

    /// Deletes every cached HFONT, releasing GDI resources. Called
    /// explicitly when the DPI changes (via provider replacement) and in `Drop`
    /// at window destruction.
    fn flush(&self) {
        let mut guard = self.cache.borrow_mut();
        for (_, (font, _)) in guard.drain() {
            unsafe {
                let _ = delete_object(font);
            }
        }
    }
}

impl Drop for FontProvider {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Draws `value` centered (or left-aligned) into `rect` at `height` pixels, in
/// `color` (straight RGBA), using the cached font from `fonts`. Empty strings
/// are skipped: an empty UTF-16 buffer is the dangling sentinel `0x2`, which
/// `DrawTextW` would dereference.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_string(
    fonts: &FontProvider,
    hdc: HDC,
    value: &str,
    rect: &mut RECT,
    height: i32,
    color: [u8; 4],
    bold: bool,
    centered: bool,
) {
    if value.is_empty() {
        return;
    }
    // ClearType subpixel rendering is incorrect on layered windows; grayscale
    // antialiasing keeps the pill text crisp.
    let (font, _) = fonts.font_for(height, bold);
    if font.0.is_null() {
        return;
    }
    // Reused UTF-16 scratch: every settings/history paint row funnels through
    // this one function on the UI thread, so a fresh Vec per call is pure
    // allocation churn. The thread-local is race-free because only the UI
    // thread paints.
    thread_local! {
        static TEXT_UTF16: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
    }
    TEXT_UTF16.with(|cell| {
        let mut text = cell.borrow_mut();
        text.clear();
        text.extend(value.encode_utf16());
        let old_font = unsafe { select_object(hdc, font) };
        let color = COLORREF(color[0] as u32 | (color[1] as u32) << 8 | (color[2] as u32) << 16);
        unsafe {
            SetTextColor(hdc, color);
            SetBkMode(hdc, TRANSPARENT);
            let mut flags = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX;
            if centered {
                flags |= DT_CENTER;
            }
            let _ = DrawTextW(hdc, &mut text, rect, flags);
            select_object(hdc, old_font);
        }
    });
}
