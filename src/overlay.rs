use crate::config::{Config, OverlayPosition};
use crate::events::{MediaEvent, PlaybackState, TrackInfo};
use anyhow::{Context, Result};
use image::imageops::FilterType;
use log::{error, warn};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::{Arc, Mutex, mpsc::Receiver};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleDC,
    CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_END_ELLIPSIS, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, FF_DONTCARE, GetMonitorInfoW, HBRUSH, HDC,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow, OUT_DEFAULT_PRECIS,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONUP;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetCursorPos, GetForegroundWindow, GetMessageW, GetWindowLongPtrW, HTTRANSPARENT, HWND_TOPMOST,
    IDC_ARROW, IDI_APPLICATION, KillTimer, LoadCursorW, LoadIconW, MA_NOACTIVATE, MF_SEPARATOR, MF_STRING,
    PostMessageW, PostQuitMessage, RegisterClassExW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_HIDEWINDOW, SWP_NOACTIVATE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, ULW_ALPHA, WM_APP, WM_DESTROY, WM_MOUSEACTIVATE, WM_NCCREATE,
    WM_NCDESTROY, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASS_STYLES, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

const WM_MEDIA_EVENT: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;
const TIMER_DEBOUNCE: usize = 1;
const TIMER_ANIMATION: usize = 2;
const TRAY_ID: u32 = 1;
const TRAY_TOGGLE_ID: usize = 1001;
const TRAY_QUIT_ID: usize = 1002;
const LIGHT_DURATION: Duration = Duration::from_millis(120);

type EventQueue = Arc<Mutex<VecDeque<MediaEvent>>>;

enum Phase {
    Hidden,
    Expanding(Instant),
    Light(Instant),
    Shown,
    Collapsing(Instant),
}

#[derive(Default)]
struct PendingEvents {
    track: Option<TrackInfo>,
    playback: Option<PlaybackState>,
}

struct OverlayState {
    hwnd: HWND,
    config: Config,
    queue: EventQueue,
    pending: PendingEvents,
    enabled: bool,
    content: Option<MediaEvent>,
    phase: Phase,
    dismiss_at: Option<Instant>,
}

pub fn run(config: Config, event_rx: Receiver<MediaEvent>) -> Result<()> {
    unsafe {
        if let Err(error) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            warn!("per-monitor DPI awareness unavailable: {error}");
        }
    }

    let module = unsafe { GetModuleHandleW(None) }.context("getting the process module")?;
    let instance: HINSTANCE = module.into();
    let class_name = wide("NotchOverlayWindow");
    register_window_class(instance, &class_name)?;

    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let state = Box::new(OverlayState::new(config.clone(), queue.clone()));
    let state_ptr = Box::into_raw(state);
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("Notch").as_ptr()),
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
    let hwnd = match hwnd {
        Ok(hwnd) => hwnd,
        Err(error) => {
            unsafe { drop(Box::from_raw(state_ptr)) };
            return Err(error.into());
        }
    };

    if let Err(error) = install_tray_icon(hwnd) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return Err(error);
    }
    spawn_event_forwarder(hwnd, queue, event_rx);
    let message_result = message_loop();
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    remove_tray_icon(hwnd);
    message_result
}

impl OverlayState {
    fn new(config: Config, queue: EventQueue) -> Self {
        Self {
            hwnd: HWND::default(),
            config,
            queue,
            pending: PendingEvents::default(),
            enabled: true,
            content: None,
            phase: Phase::Hidden,
            dismiss_at: None,
        }
    }

    fn receive_events(&mut self) {
        if let Ok(mut queue) = self.queue.lock() {
            while let Some(event) = queue.pop_front() {
                if !self.enabled {
                    continue;
                }
                match event {
                    MediaEvent::TrackChanged(track) if self.config.behavior.enable_track_change => {
                        self.pending.track = Some(track)
                    }
                    MediaEvent::PlaybackStateChanged(state) => self.pending.playback = Some(state),
                    MediaEvent::TrackChanged(_) => {}
                }
            }
        }
        if self.pending.track.is_some() || self.pending.playback.is_some() {
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_DEBOUNCE);
                SetTimer(
                    self.hwnd,
                    TIMER_DEBOUNCE,
                    self.config.behavior.debounce_ms.clamp(150, 250) as u32,
                    None,
                );
            }
        }
    }

    fn flush_pending(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_DEBOUNCE);
        }
        let pending = std::mem::take(&mut self.pending);
        if let Some(track) = pending.track {
            self.show(MediaEvent::TrackChanged(track), true);
        } else if let Some(playback) = pending.playback
            && self.config.behavior.enable_playback_state_change
        {
            self.show(MediaEvent::PlaybackStateChanged(playback), false);
        }
    }

    fn show(&mut self, event: MediaEvent, full_animation: bool) {
        if !self.enabled {
            return;
        }
        self.content = Some(event);
        let now = Instant::now();
        self.dismiss_at = Some(now + Duration::from_millis(self.config.overlay.duration_ms.max(500)));
        self.phase = if full_animation {
            Phase::Expanding(now)
        } else {
            Phase::Light(now)
        };
        unsafe {
            let _ = SetTimer(self.hwnd, TIMER_ANIMATION, 16, None);
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        self.render();
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.dismiss_at.is_some_and(|deadline| deadline <= now)
            && !matches!(self.phase, Phase::Collapsing(_) | Phase::Hidden)
        {
            self.phase = Phase::Collapsing(now);
        }

        match self.phase {
            Phase::Expanding(start) if start.elapsed() >= animation_duration(&self.config) => {
                self.phase = Phase::Shown;
            }
            Phase::Light(start) if start.elapsed() >= LIGHT_DURATION => {
                self.phase = Phase::Shown;
            }
            Phase::Collapsing(start) if start.elapsed() >= animation_duration(&self.config) => {
                self.hide();
                return;
            }
            _ => {}
        }
        self.render();
    }

    fn render(&mut self) {
        let Some(content) = self.content.as_ref() else {
            return;
        };
        let (alpha, shape) = self.frame();
        let dpi = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let (logical_width, logical_height) = match content {
            MediaEvent::TrackChanged(_) => (
                self.config.overlay.max_width.max(180) as f32,
                (self.config.appearance.art_size as f32 + 2.0 * self.config.appearance.padding + 4.0).max(40.0),
            ),
            MediaEvent::PlaybackStateChanged(_) => (120.0, 44.0),
        };
        let width = (logical_width * dpi * shape).round().max(1.0) as i32;
        let height = (logical_height * dpi * shape).round().max(1.0) as i32;
        let Some(position) = self.position(width, height) else {
            return;
        };
        if let Err(error) = render_layered(self, content, width, height, dpi * shape, alpha, position) {
            error!("rendering overlay: {error:#}");
        }
    }

    fn frame(&self) -> (u8, f32) {
        match self.phase {
            Phase::Hidden => (0, 0.55),
            Phase::Expanding(start) => {
                let progress = ease_out(start.elapsed().as_secs_f32() / animation_duration(&self.config).as_secs_f32());
                ((progress * 255.0) as u8, 0.55 + progress * 0.45)
            }
            Phase::Light(start) => {
                let progress = ease_out(start.elapsed().as_secs_f32() / LIGHT_DURATION.as_secs_f32());
                ((progress * 255.0) as u8, 1.0)
            }
            Phase::Shown => (255, 1.0),
            Phase::Collapsing(start) => {
                let progress = ease_out(start.elapsed().as_secs_f32() / animation_duration(&self.config).as_secs_f32());
                (((1.0 - progress) * 255.0) as u8, 1.0 - progress * 0.45)
            }
        }
    }

    fn position(&self, width: i32, _height: i32) -> Option<POINT> {
        let foreground = unsafe { GetForegroundWindow() };
        let monitor = unsafe {
            let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
            if monitor.0.is_null() {
                MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY)
            } else {
                monitor
            }
        };
        if monitor.0.is_null() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            return None;
        }
        let work = info.rcWork;
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let margin = (self.config.overlay.margin_top as f32 * scale).round() as i32;
        let x = match self.config.overlay.position {
            OverlayPosition::TopCenter => work.left + (work.right - work.left - width) / 2,
            OverlayPosition::TopRight => work.right - width - margin,
            OverlayPosition::TopLeft => work.left + margin,
        };
        Some(POINT {
            x,
            y: work.top + margin,
        })
    }

    fn hide(&mut self) {
        self.content = None;
        self.dismiss_at = None;
        self.phase = Phase::Hidden;
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_ANIMATION);
            let _ = KillTimer(self.hwnd, TIMER_DEBOUNCE);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_HIDEWINDOW | SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
    }

    fn toggle_enabled(&mut self) {
        self.enabled = !self.enabled;
        if !self.enabled {
            self.pending = PendingEvents::default();
            self.hide();
        }
    }
}

fn render_layered(
    state: &OverlayState,
    content: &MediaEvent,
    width: i32,
    height: i32,
    scale: f32,
    alpha: u8,
    position: POINT,
) -> Result<()> {
    let pixels = draw_pixels(state, content, width as usize, height as usize, scale)?;
    let bitmap_info = BITMAPINFO {
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
    let hdc = unsafe { CreateCompatibleDC(None) };
    if hdc.0.is_null() {
        anyhow::bail!("CreateCompatibleDC failed");
    }
    let mut bits: *mut c_void = null_mut();
    let bitmap = unsafe { CreateDIBSection(hdc, &bitmap_info, DIB_RGB_COLORS, &mut bits, None, 0) }?;
    if bits.is_null() {
        unsafe {
            let _ = DeleteDC(hdc);
        }
        anyhow::bail!("CreateDIBSection returned no pixel buffer");
    }
    unsafe {
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast(), pixels.len());
    }
    let old_bitmap = unsafe { SelectObject(hdc, bitmap) };
    draw_text(state, hdc, content, width, height, scale);

    let size = SIZE { cx: width, cy: height };
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
        SelectObject(hdc, old_bitmap);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(hdc);
        let _ = ShowWindow(state.hwnd, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(
            state.hwnd,
            HWND_TOPMOST,
            position.x,
            position.y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
    result.context("UpdateLayeredWindow")
}

fn draw_pixels(state: &OverlayState, content: &MediaEvent, width: usize, height: usize, scale: f32) -> Result<Vec<u8>> {
    let mut pixels = vec![0u8; width * height * 4];
    let radius = state.config.appearance.corner_radius * scale;
    let background = state.config.appearance.background_color;
    for y in 0..height {
        for x in 0..width {
            if inside_round_rect(x as f32, y as f32, width as f32, height as f32, radius) {
                set_pixel(&mut pixels, width, x, y, background);
            }
        }
    }

    match content {
        MediaEvent::TrackChanged(track) => {
            let padding = (state.config.appearance.padding * scale).round() as usize;
            let art_size = (state.config.appearance.art_size as f32 * scale).round() as usize;
            let art_x = padding;
            let art_y = height.saturating_sub(art_size) / 2;
            if let Some(artwork) = &track.artwork {
                if let Some(decoded) = decode_artwork(artwork, art_size) {
                    for y in 0..art_size {
                        for x in 0..art_size {
                            let source = (y * art_size + x) * 4;
                            let rgba = [
                                decoded[source],
                                decoded[source + 1],
                                decoded[source + 2],
                                decoded[source + 3],
                            ];
                            set_pixel(&mut pixels, width, art_x + x, art_y + y, rgba);
                        }
                    }
                } else {
                    draw_placeholder(
                        &mut pixels,
                        width,
                        art_x,
                        art_y,
                        art_size,
                        state.config.appearance.accent_color,
                    );
                }
            } else {
                draw_placeholder(
                    &mut pixels,
                    width,
                    art_x,
                    art_y,
                    art_size,
                    state.config.appearance.accent_color,
                );
            }
        }
        MediaEvent::PlaybackStateChanged(_) => {
            let accent = state.config.appearance.accent_color;
            let size = (16.0 * scale).round() as usize;
            let x = (12.0 * scale).round() as usize;
            let y = height.saturating_sub(size) / 2;
            draw_placeholder(&mut pixels, width, x, y, size, accent);
        }
    }
    Ok(pixels)
}

fn draw_text(state: &OverlayState, hdc: HDC, content: &MediaEvent, width: i32, height: i32, scale: f32) {
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
    }
    match content {
        MediaEvent::TrackChanged(track) => {
            let padding = (state.config.appearance.padding * scale) as i32;
            let art = (state.config.appearance.art_size as f32 * scale) as i32;
            let left = padding + art + (12.0 * scale) as i32;
            let mut title_rect = RECT {
                left,
                top: (height as f32 * 0.20) as i32,
                right: width - padding,
                bottom: (height as f32 * 0.56) as i32,
            };
            let mut artist_rect = RECT {
                left,
                top: (height as f32 * 0.56) as i32,
                right: width - padding,
                bottom: height - (height as f32 * 0.12) as i32,
            };
            draw_string(
                hdc,
                &track.title,
                &mut title_rect,
                (state.config.appearance.font_size_title * scale) as i32,
                state.config.appearance.text_color,
                true,
                false,
            );
            let subtitle = if track.artist.trim().is_empty() {
                &track.source_app
            } else {
                &track.artist
            };
            draw_string(
                hdc,
                subtitle,
                &mut artist_rect,
                (state.config.appearance.font_size_artist * scale) as i32,
                [0xCC, 0xCC, 0xCC, 0xFF],
                false,
                false,
            );
        }
        MediaEvent::PlaybackStateChanged(playback) => {
            let label = match playback {
                PlaybackState::Playing => ">  Playing",
                PlaybackState::Paused => "||  Paused",
                PlaybackState::Stopped => "[]  Stopped",
            };
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            draw_string(
                hdc,
                label,
                &mut rect,
                (state.config.appearance.font_size_title * scale) as i32,
                state.config.appearance.text_color,
                true,
                true,
            );
        }
    }
}

fn draw_string(hdc: HDC, value: &str, rect: &mut RECT, height: i32, color: [u8; 4], bold: bool, centered: bool) {
    let mut text = value.encode_utf16().collect::<Vec<_>>();
    let font_name = wide("Segoe UI");
    let font = unsafe {
        CreateFontW(
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
            CLEARTYPE_QUALITY.0 as u32,
            DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
            PCWSTR(font_name.as_ptr()),
        )
    };
    let old_font = unsafe { SelectObject(hdc, font) };
    let color = COLORREF(color[0] as u32 | (color[1] as u32) << 8 | (color[2] as u32) << 16);
    unsafe {
        SetTextColor(hdc, color);
        let mut flags = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX;
        if centered {
            flags |= windows::Win32::Graphics::Gdi::DT_CENTER;
        }
        let _ = DrawTextW(hdc, &mut text, rect, flags);
        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

fn decode_artwork(data: &[u8], size: usize) -> Option<Vec<u8>> {
    let image = image::load_from_memory(data).ok()?.to_rgba8();
    let image = image::imageops::resize(&image, size as u32, size as u32, FilterType::Triangle);
    Some(image.into_raw())
}

fn draw_placeholder(pixels: &mut [u8], width: usize, x: usize, y: usize, size: usize, color: [u8; 4]) {
    for py in y..y.saturating_add(size) {
        for px in x..x.saturating_add(size) {
            let center_x = x + size / 2;
            let center_y = y + size / 2;
            let dx = px as isize - center_x as isize;
            let dy = py as isize - center_y as isize;
            if dx * dx + dy * dy <= (size as isize / 2).pow(2) {
                set_pixel(pixels, width, px, py, color);
            }
        }
    }
}

fn set_pixel(pixels: &mut [u8], width: usize, x: usize, y: usize, color: [u8; 4]) {
    if x >= width || y >= pixels.len() / width / 4 {
        return;
    }
    let offset = (y * width + x) * 4;
    let alpha = color[3] as u32;
    pixels[offset] = (color[2] as u32 * alpha / 255) as u8;
    pixels[offset + 1] = (color[1] as u32 * alpha / 255) as u8;
    pixels[offset + 2] = (color[0] as u32 * alpha / 255) as u8;
    pixels[offset + 3] = color[3];
}

fn inside_round_rect(x: f32, y: f32, width: f32, height: f32, radius: f32) -> bool {
    let radius = radius.min(width / 2.0).min(height / 2.0);
    let dx = if x < radius {
        radius - x
    } else if x >= width - radius {
        x - (width - radius)
    } else {
        0.0
    };
    let dy = if y < radius {
        radius - y
    } else if y >= height - radius {
        y - (height - radius)
    } else {
        0.0
    };
    dx == 0.0 || dy == 0.0 || dx * dx + dy * dy <= radius * radius
}

fn register_window_class(instance: HINSTANCE, class_name: &[u16]) -> Result<()> {
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: Default::default(),
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        anyhow::bail!("RegisterClassExW failed");
    }
    Ok(())
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            let state = (*create).lpCreateParams as *mut OverlayState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            (*state).hwnd = hwnd;
        }
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            let _ = ValidateRect(hwnd, None);
            LRESULT(0)
        }
        WM_MEDIA_EVENT => {
            if !state_ptr.is_null() {
                (*state_ptr).receive_events();
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_DEBOUNCE => {
            if !state_ptr.is_null() {
                (*state_ptr).flush_pending();
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_ANIMATION => {
            if !state_ptr.is_null() {
                (*state_ptr).tick();
            }
            LRESULT(0)
        }
        WM_TRAY => {
            if lparam.0 as u32 == WM_RBUTTONUP && !state_ptr.is_null() {
                show_tray_menu(hwnd, &mut *state_ptr);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !state_ptr.is_null() {
                drop(Box::from_raw(state_ptr));
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn spawn_event_forwarder(hwnd: HWND, queue: EventQueue, receiver: Receiver<MediaEvent>) {
    let raw_hwnd = hwnd.0 as isize;
    thread::Builder::new()
        .name("notch-events".to_string())
        .spawn(move || {
            while let Ok(event) = receiver.recv() {
                if let Ok(mut events) = queue.lock() {
                    events.push_back(event);
                }
                let hwnd = HWND(raw_hwnd as *mut c_void);
                if unsafe { PostMessageW(hwnd, WM_MEDIA_EVENT, WPARAM(0), LPARAM(0)) }.is_err() {
                    break;
                }
            }
        })
        .expect("event forwarder thread should start");
}

fn install_tray_icon(hwnd: HWND) -> Result<()> {
    let data = tray_data(hwnd)?;
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        anyhow::bail!("Shell_NotifyIconW(NIM_ADD) failed");
    }
    Ok(())
}

fn remove_tray_icon(hwnd: HWND) {
    if let Ok(data) = tray_data(hwnd) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }
}

fn tray_data(hwnd: HWND) -> Result<NOTIFYICONDATAW> {
    let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }?;
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    let tip = wide("Notch media overlay");
    let count = tip.len().min(data.szTip.len());
    data.szTip[..count].copy_from_slice(&tip[..count]);
    Ok(data)
}

fn show_tray_menu(hwnd: HWND, state: &mut OverlayState) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let toggle = if state.enabled {
        "Disable notifications"
    } else {
        "Enable notifications"
    };
    let toggle_text = wide(toggle);
    let quit_text = wide("Quit Notch");
    unsafe {
        let _ = AppendMenuW(menu, MF_STRING, TRAY_TOGGLE_ID, PCWSTR(toggle_text.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, TRAY_QUIT_ID, PCWSTR(quit_text.as_ptr()));
        let mut point = POINT::default();
        if GetCursorPos(&mut point).is_ok() {
            let command = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                point.x,
                point.y,
                0,
                hwnd,
                None,
            )
            .0 as usize;
            match command {
                TRAY_TOGGLE_ID => state.toggle_enabled(),
                TRAY_QUIT_ID => {
                    remove_tray_icon(hwnd);
                    PostQuitMessage(0);
                }
                _ => {}
            }
        }
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyMenu(menu);
    }
}

fn message_loop() -> Result<()> {
    let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == -1 {
            anyhow::bail!("GetMessageW failed");
        }
        if result.0 == 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

fn animation_duration(config: &Config) -> Duration {
    Duration::from_millis(config.overlay.animation_ms.clamp(100, 500))
}

fn ease_out(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    1.0 - (1.0 - value).powi(3)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
