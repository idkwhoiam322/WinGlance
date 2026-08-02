use crate::autostart;
use crate::config::{Config, HorizontalPosition, VerticalPosition};
use crate::events::{MEDIA_EVENT_MSG, MediaEvent, PlaybackState, TOGGLE_MSG, TrackInfo};
use crate::overlay::{EventQueue, OverlayPos, decode_artwork, draw_string, set_position, show_sample, wide};
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use log::error;
use std::collections::VecDeque;
use std::ffi::c_void;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush,
    DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DeleteObject, EndPaint, FF_DONTCARE, FillRect, GetStockObject,
    HBRUSH, HDC, HFONT, InvalidateRect, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, SetBkColor, SetTextColor,
    StretchDIBits,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GWLP_USERDATA, GetClientRect, GetCursorPos, GetWindowLongPtrW, HMENU, IDC_ARROW, IDI_APPLICATION, LB_ADDSTRING,
    LB_DELETESTRING, LB_GETCOUNT, LB_SETITEMHEIGHT, LB_SETTOPINDEX, LBS_HASSTRINGS, LBS_NOINTEGRALHEIGHT,
    LBS_OWNERDRAWFIXED, LoadCursorW, LoadIconW, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, PostMessageW,
    PostQuitMessage, RegisterClassExW, SW_HIDE, SW_SHOWMAXIMIZED, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_CREATE, WM_CTLCOLORLISTBOX, WM_DESTROY, WM_LBUTTONDBLCLK,
    WM_LBUTTONDOWN, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONUP, WM_SETFONT, WM_SIZE, WNDCLASS_STYLES,
    WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN, WS_OVERLAPPEDWINDOW, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::PCWSTR;

const WM_TRAY: u32 = WM_APP + 2;
const TRAY_ID: u32 = 1;
const MENU_OPEN_ID: usize = 1001;
const MENU_NOTIFY_ID: usize = 1002;
const MENU_AUTOSTART_ID: usize = 1003;
const MENU_CLOSE_TRAY_ID: usize = 1004;
const MENU_QUIT_ID: usize = 1006;
const MENU_POSITION_TOP_LEFT: usize = 1007;
const MENU_POSITION_TOP_CENTER: usize = 1008;
const MENU_POSITION_TOP_RIGHT: usize = 1009;
const MENU_POSITION_BOTTOM_LEFT: usize = 1010;
const MENU_POSITION_BOTTOM_CENTER: usize = 1011;
const MENU_POSITION_BOTTOM_RIGHT: usize = 1012;
const MENU_POSITION_CUSTOM: usize = 1013;
const MENU_POSITION_SAMPLE: usize = 1014;
const MENU_POSITION_RESET: usize = 1015;
const MENU_DURATION_2S: usize = 1017;
const MENU_DURATION_3S: usize = 1018;
const MENU_DURATION_5S: usize = 1019;
const MENU_DURATION_10S: usize = 1020;
const LISTBOX_ID: usize = 2;
const HISTORY_CAP: usize = 500;

const fn colorref(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

// Logical panel geometry (scaled by DPI at paint/layout time).
const PAD: f32 = 16.0;
const HEADER_H: f32 = 22.0;
const ART_Y: f32 = 36.0;
const ART_SIZE: f32 = 96.0;
const SEP_GAP: f32 = 20.0;
const HIST_GAP: f32 = 14.0;
const HIST_H: f32 = 18.0;
const LIST_GAP: f32 = 8.0;
const BOTTOM_GAP: f32 = 16.0;

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct HistoryEntry {
    at: DateTime<Local>,
    track: TrackInfo,
    state: PlaybackState,
}

struct History {
    entries: VecDeque<HistoryEntry>,
    cap: usize,
}

impl History {
    fn new(cap: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            cap,
        }
    }

    fn push(&mut self, entry: HistoryEntry) {
        self.entries.push_back(entry);
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter()
    }
}

struct CurrentActivity {
    track: TrackInfo,
    state: PlaybackState,
    art: Option<Vec<u8>>,
}

struct MainWindowState {
    hwnd: HWND,
    instance: HINSTANCE,
    config: Config,
    queue: EventQueue,
    overlay_hwnd: HWND,
    listbox: HWND,
    current: Option<CurrentActivity>,
    history: History,
    listbox_font: HFONT,
    gray_brush: HBRUSH,
    accent_brush: HBRUSH,
    notifications_enabled: bool,
}

/// Creates the main window: a maximized tracker with current activity,
/// per-session history, and a tray icon. The caller runs the message loop.
pub fn create_window(config: Config, queue: EventQueue, overlay_hwnd: HWND) -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.context("getting the process module")?;
    let instance: HINSTANCE = module.into();
    let class_name = wide("NotchMainWindow");
    register_main_class(instance, &class_name)?;

    let state = Box::new(MainWindowState::new(config.clone(), queue, overlay_hwnd, instance));
    let state_ptr = Box::into_raw(state);
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("Notch").as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            0,
            0,
            800,
            600,
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

    unsafe {
        if config.behavior.start_in_tray {
            let _ = ShowWindow(hwnd, SW_HIDE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
        }
    }
    if let Err(error) = install_tray_icon(hwnd) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return Err(error);
    }
    Ok(hwnd)
}

impl MainWindowState {
    fn new(config: Config, queue: EventQueue, overlay_hwnd: HWND, instance: HINSTANCE) -> Self {
        Self {
            hwnd: HWND::default(),
            instance,
            config,
            queue,
            overlay_hwnd,
            listbox: HWND::default(),
            current: None,
            history: History::new(HISTORY_CAP),
            listbox_font: HFONT::default(),
            gray_brush: HBRUSH::default(),
            accent_brush: HBRUSH::default(),
            notifications_enabled: true,
        }
    }

    fn create_children(&mut self) {
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let font_name = wide("Segoe UI");
        self.listbox_font = unsafe {
            CreateFontW(
                -((13.0 * scale).round() as i32).max(1),
                0,
                0,
                0,
                400,
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
        self.gray_brush = unsafe { CreateSolidBrush(colorref(0x1E, 0x1E, 0x1E)) };
        let accent = self.config.appearance.accent_color;
        self.accent_brush = unsafe { CreateSolidBrush(colorref(accent[0], accent[1], accent[2])) };

        self.listbox = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                PCWSTR(wide("LISTBOX").as_ptr()),
                PCWSTR::null(),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_VSCROLL
                    | WINDOW_STYLE(LBS_OWNERDRAWFIXED as u32 | LBS_HASSTRINGS as u32 | LBS_NOINTEGRALHEIGHT as u32),
                0,
                0,
                0,
                0,
                self.hwnd,
                HMENU(LISTBOX_ID as *mut c_void),
                self.instance,
                None,
            )
        }
        .unwrap_or_default();
        if !self.listbox.0.is_null() {
            unsafe {
                let scale = GetDpiForWindow(self.hwnd).max(96) as f32 / 96.0;
                let item_h = (18.0 * scale).round() as i32;
                let _ = SendMessageW(self.listbox, LB_SETITEMHEIGHT, WPARAM(0), LPARAM(item_h as isize));
                let _ = SendMessageW(
                    self.listbox,
                    WM_SETFONT,
                    WPARAM(self.listbox_font.0 as usize),
                    LPARAM(1),
                );
                let header = wide("TIME     EVENT");
                let _ = SendMessageW(self.listbox, LB_ADDSTRING, WPARAM(0), LPARAM(header.as_ptr() as isize));
            }
            self.layout();
        }
    }

    fn receive_events(&mut self) {
        let mut batch = Vec::new();
        if let Ok(mut queue) = self.queue.lock() {
            while let Some(event) = queue.pop_front() {
                batch.push(event);
            }
        }
        for event in batch {
            match event {
                MediaEvent::TrackChanged(track) => self.add_track(track),
                MediaEvent::PlaybackStateChanged(state) => {
                    if let Some(current) = &mut self.current {
                        current.state = state;
                        self.add_state_change(state);
                        self.invalidate();
                    }
                }
            }
        }
    }

    fn add_state_change(&mut self, state: PlaybackState) {
        let track = self.current.as_ref().map(|c| c.track.clone()).unwrap_or(TrackInfo {
            title: "Unknown".into(),
            artist: String::new(),
            album: String::new(),
            artwork: None,
            source_app: "Media".into(),
        });
        let before = self.history.len();
        self.history.push(HistoryEntry {
            at: Local::now(),
            track: track.clone(),
            state,
        });
        if self.history.len() <= before && before > 0 {
            let _ = unsafe { SendMessageW(self.listbox, LB_DELETESTRING, WPARAM(0), LPARAM(0)) };
        }
        let row = history_row(&track, Local::now(), state);
        let row = wide(&row);
        if !self.listbox.0.is_null() {
            unsafe {
                let _ = SendMessageW(self.listbox, LB_ADDSTRING, WPARAM(0), LPARAM(row.as_ptr() as isize));
                let count = SendMessageW(self.listbox, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0 as usize;
                if count > 0 {
                    let _ = SendMessageW(self.listbox, LB_SETTOPINDEX, WPARAM(count - 1), LPARAM(0));
                }
            }
        }
    }

    fn add_track(&mut self, track: TrackInfo) {
        let state = self.current.as_ref().map(|c| c.state).unwrap_or(PlaybackState::Playing);
        let before = self.history.len();
        self.history.push(HistoryEntry {
            at: Local::now(),
            track: track.clone(),
            state,
        });
        if self.history.len() <= before && before > 0 {
            let _ = unsafe { SendMessageW(self.listbox, LB_DELETESTRING, WPARAM(0), LPARAM(0)) };
        }
        let row = history_row(&track, Local::now(), state);
        let row = wide(&row);
        if !self.listbox.0.is_null() {
            unsafe {
                let _ = SendMessageW(self.listbox, LB_ADDSTRING, WPARAM(0), LPARAM(row.as_ptr() as isize));
                let count = SendMessageW(self.listbox, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0 as usize;
                if count > 0 {
                    let _ = SendMessageW(self.listbox, LB_SETTOPINDEX, WPARAM(count - 1), LPARAM(0));
                }
            }
        }
        self.current = Some(CurrentActivity {
            track,
            state: self
                .current
                .as_ref()
                .map(|current| current.state)
                .unwrap_or(PlaybackState::Playing),
            art: None,
        });
        self.invalidate();
    }

    fn paint(&mut self) {
        let mut paint = PAINTSTRUCT::default();
        let hdc = unsafe { BeginPaint(self.hwnd, &mut paint) };
        if hdc.0.is_null() {
            return;
        }
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let (client_w, client_h) = client_size(self.hwnd);
        let whole = RECT {
            left: 0,
            top: 0,
            right: client_w,
            bottom: client_h,
        };
        let pad = (PAD * scale) as i32;
        unsafe {
            let black = CreateSolidBrush(COLORREF(0));
            let _ = FillRect(hdc, &whole, black);
            let _ = DeleteObject(black);
        }

        let mut header_rect = RECT {
            left: pad,
            top: pad,
            right: client_w - pad,
            bottom: pad + (HEADER_H * scale) as i32,
        };
        draw_string(
            hdc,
            "NOW PLAYING",
            &mut header_rect,
            (11.0 * scale) as i32,
            self.config.appearance.accent_color,
            true,
            false,
        );

        let art = (ART_SIZE * scale).round() as i32;
        let art_x = pad;
        let art_y = (ART_Y * scale) as i32;
        let text_left = art_x + art + (12.0 * scale) as i32;
        let text_right = client_w - pad;

        if let Some(current) = &mut self.current {
            if current.art.is_none() {
                current.art = current
                    .track
                    .artwork
                    .as_deref()
                    .and_then(|data| decode_artwork(data, (ART_SIZE * scale).round() as usize));
            }
            if let Some(art_pixels) = current.art.as_deref() {
                draw_art(hdc, art_pixels, art, art_x, art_y);
            } else {
                let art_rect = RECT {
                    left: art_x,
                    top: art_y,
                    right: art_x + art,
                    bottom: art_y + art,
                };
                unsafe {
                    let _ = FillRect(hdc, &art_rect, self.accent_brush);
                }
            }
            let state_label = match current.state {
                PlaybackState::Playing => "Playing",
                PlaybackState::Paused => "Paused",
                PlaybackState::Stopped => "Stopped",
            };
            let state_color = if current.state == PlaybackState::Playing {
                self.config.appearance.accent_color
            } else {
                [0xBB, 0xBB, 0xBB, 0xFF]
            };
            let mut state_rect = RECT {
                left: text_left,
                top: art_y,
                right: text_right,
                bottom: art_y + (18.0 * scale) as i32,
            };
            draw_string(
                hdc,
                state_label,
                &mut state_rect,
                (11.0 * scale) as i32,
                state_color,
                true,
                false,
            );

            let mut title_rect = RECT {
                left: text_left,
                top: art_y + (22.0 * scale) as i32,
                right: text_right,
                bottom: art_y + (48.0 * scale) as i32,
            };
            draw_string(
                hdc,
                &current.track.title,
                &mut title_rect,
                (19.0 * scale) as i32,
                self.config.appearance.text_color,
                true,
                false,
            );
            let subtitle = if current.track.artist.trim().is_empty() {
                &current.track.source_app
            } else {
                &current.track.artist
            };
            let mut artist_rect = RECT {
                left: text_left,
                top: art_y + (48.0 * scale) as i32,
                right: text_right,
                bottom: art_y + (72.0 * scale) as i32,
            };
            draw_string(
                hdc,
                subtitle,
                &mut artist_rect,
                (14.0 * scale) as i32,
                [0xCC, 0xCC, 0xCC, 0xFF],
                false,
                false,
            );
            if !current.track.album.trim().is_empty() {
                let mut album_rect = RECT {
                    left: text_left,
                    top: art_y + (70.0 * scale) as i32,
                    right: text_right,
                    bottom: art_y + (86.0 * scale) as i32,
                };
                draw_string(
                    hdc,
                    &current.track.album,
                    &mut album_rect,
                    (12.0 * scale) as i32,
                    [0x99, 0x99, 0x99, 0xFF],
                    false,
                    false,
                );
            }
            if !current.track.source_app.trim().is_empty() {
                let mut app_rect = RECT {
                    left: text_left,
                    top: art_y + (86.0 * scale) as i32,
                    right: text_right,
                    bottom: art_y + (100.0 * scale) as i32,
                };
                draw_string(
                    hdc,
                    &current.track.source_app,
                    &mut app_rect,
                    (10.0 * scale) as i32,
                    [0x77, 0x77, 0x77, 0xFF],
                    false,
                    false,
                );
            }
        } else {
            let mut empty_rect = RECT {
                left: text_left,
                top: art_y + (16.0 * scale) as i32,
                right: text_right,
                bottom: art_y + art,
            };
            draw_string(
                hdc,
                "No media playing",
                &mut empty_rect,
                (15.0 * scale) as i32,
                [0x99, 0x99, 0x99, 0xFF],
                false,
                false,
            );
        }

        let sep_y = art_y + art + (SEP_GAP * scale) as i32;
        let separator = RECT {
            left: 0,
            top: sep_y,
            right: client_w,
            bottom: sep_y + 1,
        };
        unsafe {
            let _ = FillRect(hdc, &separator, self.gray_brush);
        }

        let mut history_rect = RECT {
            left: pad,
            top: sep_y + (HIST_GAP * scale) as i32,
            right: client_w - pad,
            bottom: sep_y + ((HIST_GAP + HIST_H) * scale) as i32,
        };
        draw_string(
            hdc,
            "SESSION HISTORY",
            &mut history_rect,
            (11.0 * scale) as i32,
            [0x99, 0x99, 0x99, 0xFF],
            true,
            false,
        );

        let pos_y = history_rect.bottom + (4.0 * scale) as i32;
        let pos_label = if self.config.overlay.position_x.is_some() {
            format!(
                "Position: custom ({}, {})",
                self.config.overlay.position_x.unwrap_or(0),
                self.config.overlay.position_y.unwrap_or(0)
            )
        } else {
            format!(
                "Position: {}-{}",
                match self.config.overlay.vertical {
                    VerticalPosition::Top => "top",
                    VerticalPosition::Bottom => "bottom",
                },
                match self.config.overlay.horizontal {
                    HorizontalPosition::Left => "left",
                    HorizontalPosition::Center => "center",
                    HorizontalPosition::Right => "right",
                }
            )
        };
        let mut pos_rect = RECT {
            left: pad,
            top: pos_y,
            right: client_w - pad,
            bottom: pos_y + (16.0 * scale) as i32,
        };
        draw_string(
            hdc,
            &pos_label,
            &mut pos_rect,
            (10.0 * scale) as i32,
            [0x66, 0x66, 0x66, 0xFF],
            false,
            false,
        );

        unsafe {
            let _ = EndPaint(self.hwnd, &paint);
        }
    }

    fn layout(&self) {
        if self.listbox.0.is_null() {
            return;
        }
        let scale = unsafe { GetDpiForWindow(self.hwnd).max(96) } as f32 / 96.0;
        let (client_w, client_h) = client_size(self.hwnd);
        let pad = (PAD * scale) as i32;
        let top = ((ART_Y + ART_SIZE + SEP_GAP + HIST_GAP + HIST_H + LIST_GAP) * scale).round() as i32;
        let bottom_gap = (BOTTOM_GAP * scale).round() as i32;
        unsafe {
            let _ = SetWindowPos(
                self.listbox,
                HWND::default(),
                pad,
                top,
                (client_w - 2 * pad).max(0),
                (client_h - top - bottom_gap).max(0),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    fn on_close(&self) {
        unsafe {
            if self.config.behavior.close_to_tray {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            } else {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    fn on_destroy(&mut self) {
        remove_tray_icon(self.hwnd);
        unsafe {
            if !self.listbox_font.0.is_null() {
                let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(self.listbox_font.0));
            }
            if !self.gray_brush.0.is_null() {
                let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(self.gray_brush.0));
            }
            if !self.accent_brush.0.is_null() {
                let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(self.accent_brush.0));
            }
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    fn show_window(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWMAXIMIZED);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    /// Pins the overlay to a vertical/horizontal anchor: clears any absolute
    /// override, persists the choice, and nudges the live overlay into place.
    fn apply_anchor(&mut self, vertical: VerticalPosition, horizontal: HorizontalPosition) {
        self.config.overlay.vertical = vertical;
        self.config.overlay.horizontal = horizontal;
        self.config.overlay.position_x = None;
        self.config.overlay.position_y = None;
        let _ = self.config.save();
        set_position(self.overlay_hwnd, OverlayPos::from_config(&self.config));
    }
}

fn history_row(track: &TrackInfo, at: DateTime<Local>, state: PlaybackState) -> String {
    let artist = if track.artist.trim().is_empty() {
        &track.source_app
    } else {
        &track.artist
    };
    let status = match state {
        PlaybackState::Playing => "▶",
        PlaybackState::Paused => "‖",
        PlaybackState::Stopped => "■",
    };
    format!("{}  {}  {} — {}", at.format("%H:%M:%S"), status, track.title, artist)
}

fn draw_art(hdc: HDC, rgba: &[u8], px: i32, x: i32, y: i32) {
    let mut bgra = Vec::with_capacity(rgba.len());
    for chunk in rgba.chunks_exact(4) {
        let [r, g, b, a] = [chunk[0], chunk[1], chunk[2], chunk[3]];
        bgra.extend([
            (b as u32 * a as u32 / 255) as u8,
            (g as u32 * a as u32 / 255) as u8,
            (r as u32 * a as u32 / 255) as u8,
            a,
        ]);
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: px,
            biHeight: -px,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let drawn = unsafe {
        StretchDIBits(
            hdc,
            x,
            y,
            px,
            px,
            0,
            0,
            px,
            px,
            Some(bgra.as_ptr().cast()),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        )
    };
    if drawn == 0 {
        error!("StretchDIBits failed while drawing artwork");
    }
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rect);
    }
    (rect.right, rect.bottom)
}

fn register_main_class(instance: HINSTANCE, class_name: &[u16]) -> Result<()> {
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
        hbrBackground: HBRUSH(unsafe { GetStockObject(windows::Win32::Graphics::Gdi::BLACK_BRUSH) }.0),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: Default::default(),
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        anyhow::bail!("RegisterClassExW failed for main window");
    }
    Ok(())
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

fn show_tray_menu(state: &mut MainWindowState) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let open_flags = MF_STRING;
    let mut notify_flags = MF_STRING;
    if state.notifications_enabled {
        notify_flags |= MF_CHECKED;
    }
    let mut autostart_flags = MF_STRING;
    if state.config.behavior.start_on_login {
        autostart_flags |= MF_CHECKED;
    }
    let mut close_tray_flags = MF_STRING;
    if state.config.behavior.close_to_tray {
        close_tray_flags |= MF_CHECKED;
    }
    unsafe {
        let _ = AppendMenuW(menu, open_flags, MENU_OPEN_ID, PCWSTR(wide("Open Notch").as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            notify_flags,
            MENU_NOTIFY_ID,
            PCWSTR(wide("Toggle notifications").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            autostart_flags,
            MENU_AUTOSTART_ID,
            PCWSTR(wide("Start with Windows").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            close_tray_flags,
            MENU_CLOSE_TRAY_ID,
            PCWSTR(wide("Close window to tray").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_TOP_LEFT,
            PCWSTR(wide("Position: top-left").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_TOP_CENTER,
            PCWSTR(wide("Position: top-center").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_TOP_RIGHT,
            PCWSTR(wide("Position: top-right").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_BOTTOM_LEFT,
            PCWSTR(wide("Position: bottom-left").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_BOTTOM_CENTER,
            PCWSTR(wide("Position: bottom-center").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_BOTTOM_RIGHT,
            PCWSTR(wide("Position: bottom-right").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_CUSTOM,
            PCWSTR(wide("Adjust position…").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_SAMPLE,
            PCWSTR(wide("Show sample").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_POSITION_RESET,
            PCWSTR(wide("Reset position").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        // Duration submenu
        let Ok(duration_menu) = CreatePopupMenu() else {
            let _ = DestroyMenu(menu);
            return;
        };
        let current_secs = state.config.overlay.duration_ms / 1000;
        let dur_2s_flags = if current_secs == 2 {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let dur_3s_flags = if current_secs == 3 {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let dur_5s_flags = if current_secs == 5 {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let dur_10s_flags = if current_secs == 10 {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let _ = AppendMenuW(
            duration_menu,
            dur_2s_flags,
            MENU_DURATION_2S,
            PCWSTR(wide("2 seconds").as_ptr()),
        );
        let _ = AppendMenuW(
            duration_menu,
            dur_3s_flags,
            MENU_DURATION_3S,
            PCWSTR(wide("3 seconds").as_ptr()),
        );
        let _ = AppendMenuW(
            duration_menu,
            dur_5s_flags,
            MENU_DURATION_5S,
            PCWSTR(wide("5 seconds").as_ptr()),
        );
        let _ = AppendMenuW(
            duration_menu,
            dur_10s_flags,
            MENU_DURATION_10S,
            PCWSTR(wide("10 seconds").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            duration_menu.0 as usize,
            PCWSTR(wide("Duration").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT_ID, PCWSTR(wide("Quit").as_ptr()));

        let mut point = POINT::default();
        if GetCursorPos(&mut point).is_ok() {
            let command = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                point.x,
                point.y,
                0,
                state.hwnd,
                None,
            )
            .0 as usize;
            match command {
                MENU_OPEN_ID => state.show_window(),
                MENU_NOTIFY_ID => {
                    state.notifications_enabled = !state.notifications_enabled;
                    let _ = PostMessageW(state.overlay_hwnd, TOGGLE_MSG, WPARAM(0), LPARAM(0));
                }
                MENU_AUTOSTART_ID => {
                    state.config.behavior.start_on_login = !state.config.behavior.start_on_login;
                    let _ = state.config.save();
                    if let Err(error) = autostart::apply(state.config.behavior.start_on_login) {
                        error!("start-on-login update failed: {error:#}");
                    }
                }
                MENU_CLOSE_TRAY_ID => {
                    state.config.behavior.close_to_tray = !state.config.behavior.close_to_tray;
                    let _ = state.config.save();
                }
                MENU_QUIT_ID => {
                    let _ = DestroyWindow(state.hwnd);
                }
                MENU_POSITION_TOP_LEFT => state.apply_anchor(VerticalPosition::Top, HorizontalPosition::Left),
                MENU_POSITION_TOP_CENTER => state.apply_anchor(VerticalPosition::Top, HorizontalPosition::Center),
                MENU_POSITION_TOP_RIGHT => state.apply_anchor(VerticalPosition::Top, HorizontalPosition::Right),
                MENU_POSITION_BOTTOM_LEFT => state.apply_anchor(VerticalPosition::Bottom, HorizontalPosition::Left),
                MENU_POSITION_BOTTOM_CENTER => state.apply_anchor(VerticalPosition::Bottom, HorizontalPosition::Center),
                MENU_POSITION_BOTTOM_RIGHT => state.apply_anchor(VerticalPosition::Bottom, HorizontalPosition::Right),
                MENU_POSITION_CUSTOM => {
                    let _ = crate::positioner::open(state.hwnd, state.overlay_hwnd);
                }
                MENU_POSITION_SAMPLE => {
                    show_sample(state.overlay_hwnd);
                }
                MENU_POSITION_RESET => {
                    state.config.overlay.vertical = VerticalPosition::Top;
                    state.config.overlay.horizontal = HorizontalPosition::Center;
                    state.config.overlay.position_x = None;
                    state.config.overlay.position_y = None;
                    let _ = state.config.save();
                    set_position(state.overlay_hwnd, OverlayPos::from_config(&state.config));
                }
                MENU_DURATION_2S => {
                    state.config.overlay.duration_ms = 2000;
                    let _ = state.config.save();
                }
                MENU_DURATION_3S => {
                    state.config.overlay.duration_ms = 3000;
                    let _ = state.config.save();
                }
                MENU_DURATION_5S => {
                    state.config.overlay.duration_ms = 5000;
                    let _ = state.config.save();
                }
                MENU_DURATION_10S => {
                    state.config.overlay.duration_ms = 10000;
                    let _ = state.config.save();
                }
                _ => {}
            }
        }
        let _ = DestroyMenu(menu);
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            let state = (*create).lpCreateParams as *mut MainWindowState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            (*state).hwnd = hwnd;
        }
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;
    match message {
        WM_CREATE => {
            if !state_ptr.is_null() {
                (*state_ptr).create_children();
            }
            LRESULT(0)
        }
        WM_PAINT => {
            if !state_ptr.is_null() {
                (*state_ptr).paint();
            }
            LRESULT(0)
        }
        WM_SIZE => {
            if !state_ptr.is_null() {
                (*state_ptr).layout();
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if !state_ptr.is_null() {
                let state = &*state_ptr;
                let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
                let y = (lparam.0 >> 16) as i32;
                let pos_y =
                    ((ART_Y + ART_SIZE + SEP_GAP + HIST_GAP + HIST_H) * scale).round() as i32 + (4.0 * scale) as i32;
                let pos_bottom = pos_y + (16.0 * scale) as i32;
                if y >= pos_y && y <= pos_bottom {
                    let _ = crate::positioner::open(hwnd, state.overlay_hwnd);
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLORLISTBOX => {
            let hdc = HDC(wparam.0 as *mut c_void);
            SetTextColor(hdc, colorref(0xE6, 0xE6, 0xE6));
            SetBkColor(hdc, colorref(0, 0, 0));
            LRESULT(GetStockObject(windows::Win32::Graphics::Gdi::BLACK_BRUSH).0 as isize)
        }
        MEDIA_EVENT_MSG => {
            if !state_ptr.is_null() {
                (*state_ptr).receive_events();
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if !state_ptr.is_null() {
                (*state_ptr).on_close();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if !state_ptr.is_null() {
                (*state_ptr).on_destroy();
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_TRAY => {
            let event = lparam.0 as u32;
            if event == WM_RBUTTONUP && !state_ptr.is_null() {
                show_tray_menu(&mut *state_ptr);
            } else if event == WM_LBUTTONDBLCLK && !state_ptr.is_null() {
                (*state_ptr).show_window();
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str) -> TrackInfo {
        TrackInfo {
            title: title.into(),
            artist: "The Artist".into(),
            album: "The Album".into(),
            artwork: None,
            source_app: "Spotify".into(),
        }
    }

    #[test]
    fn history_keeps_cap_and_order() {
        let mut history = History::new(3);
        for index in 0..5 {
            history.push(HistoryEntry {
                at: Local::now(),
                track: track(&format!("Track {index}")),
                state: PlaybackState::Playing,
            });
        }
        assert_eq!(history.len(), 3);
        let titles: Vec<_> = history.iter().map(|entry| entry.track.title.as_str()).collect();
        assert_eq!(titles, ["Track 2", "Track 3", "Track 4"]);
    }

    #[test]
    fn row_falls_back_to_source_app_when_artist_is_blank() {
        let mut blank = track("Song");
        blank.artist = "   ".into();
        let row = history_row(&blank, Local::now(), PlaybackState::Playing);
        assert!(row.contains("Song"));
        assert!(row.contains("Spotify"));

        let titled = track("Song");
        let row = history_row(&titled, Local::now(), PlaybackState::Paused);
        assert!(row.contains("The Artist"));
    }
}
