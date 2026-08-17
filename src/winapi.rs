//! Thin, version-stable facade over the raw Win32 API WinGlance calls.
//!
//! The `windows` 0.62 crate changed many Win32 signatures from 0.58:
//! handle/pointer parameters are `Option<T>` (a null handle is an explicit
//! `None`), GDI object handles need an explicit `.into()` to `HGDIOBJ`, and
//! `CreateFontW` gained typed charset / precision / quality newtypes. Call
//! sites go through this module, never through the `windows` crate directly,
//! so a future `windows` bump stays a body-only edit inside this one file.

use core::ffi::c_void;

use windows::Win32::Foundation::{
    COLORREF, GlobalFree, HANDLE, HGLOBAL, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BLENDFUNCTION, CreateDIBSection, CreateFontW, DIB_USAGE, DeleteObject, FONT_CHARSET,
    FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION, FONT_QUALITY, HBITMAP, HDC, HFONT, HGDIOBJ, InvalidateRect,
    SelectObject, ValidateRect,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_CREATION_DISPOSITION, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
};
use windows::Win32::System::DataExchange::SetClipboardData;
use windows::Win32::System::Registry::{HKEY, REG_VALUE_TYPE, RegSetValueExW};
use windows::Win32::System::Threading::DeleteTimerQueueTimer;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, WINEVENTPROC};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, HCURSOR, HMENU, IsWindow, KillTimer, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, MSG, MessageBoxW,
    PEEK_MESSAGE_REMOVE_TYPE, PeekMessageW, PostMessageW, SET_WINDOW_POS_FLAGS, SHOW_WINDOW_CMD, SendMessageW,
    SetCursor, SetTimer, SetWindowPos, TIMERPROC, TRACK_POPUP_MENU_FLAGS, TrackPopupMenu, UPDATE_LAYERED_WINDOW_FLAGS,
    UpdateLayeredWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::{PCWSTR, Result};

/// `SendMessageW` (blocking; the callee runs synchronously on this thread).
pub unsafe fn send_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { SendMessageW(hwnd, msg, Some(wparam), Some(lparam)) }
}

/// `PostMessageW` (non-blocking).
pub unsafe fn post_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Result<()> {
    unsafe { PostMessageW(Some(hwnd), msg, wparam, lparam) }
}

/// `PeekMessageW`; true when a message was retrieved.
pub unsafe fn peek_message(msg: *mut MSG, hwnd: HWND, min: u32, max: u32, remove: PEEK_MESSAGE_REMOVE_TYPE) -> bool {
    unsafe { PeekMessageW(msg, Some(hwnd), min, max, remove).as_bool() }
}

/// `SetFocus`; returns the window that previously had focus.
pub unsafe fn set_focus(hwnd: HWND) -> Result<HWND> {
    unsafe { SetFocus(Some(hwnd)) }
}

/// `SetCursor`; returns the previously selected cursor.
pub unsafe fn set_cursor(cursor: HCURSOR) -> HCURSOR {
    unsafe { SetCursor(Some(cursor)) }
}

/// `SetTimer`; returns the new timer id (0 on failure).
pub unsafe fn set_timer(hwnd: HWND, id: usize, ms: u32, proc: TIMERPROC) -> usize {
    unsafe { SetTimer(Some(hwnd), id, ms, proc) }
}

/// `KillTimer`.
pub unsafe fn kill_timer(hwnd: HWND, id: usize) -> Result<()> {
    unsafe { KillTimer(Some(hwnd), id) }
}

/// `SetWindowPos`. `after` is the insert-after window (ignored with `SWP_NOZORDER`).
pub unsafe fn set_window_pos(
    hwnd: HWND,
    after: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    flags: SET_WINDOW_POS_FLAGS,
) -> Result<()> {
    unsafe { SetWindowPos(hwnd, Some(after), x, y, w, h, flags) }
}

/// `InvalidateRect`; true when the invalidation was queued.
pub unsafe fn invalidate_rect(hwnd: HWND, rect: Option<*const RECT>, erase: bool) -> bool {
    unsafe { InvalidateRect(Some(hwnd), rect, erase).as_bool() }
}

/// `ValidateRect`; true when the update region was validated.
pub unsafe fn validate_rect(hwnd: HWND, rect: Option<*const RECT>) -> bool {
    unsafe { ValidateRect(Some(hwnd), rect).as_bool() }
}

/// `IsWindow`.
pub unsafe fn is_window(hwnd: HWND) -> bool {
    unsafe { IsWindow(Some(hwnd)).as_bool() }
}

/// `MessageBoxW`; returns the button the user chose.
pub unsafe fn message_box(hwnd: HWND, text: PCWSTR, caption: PCWSTR, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(Some(hwnd), text, caption, style) }
}

/// `TrackPopupMenu`; with `TPM_RETURNCMD` this is the selected command id.
pub unsafe fn track_popup_menu(
    menu: HMENU,
    flags: TRACK_POPUP_MENU_FLAGS,
    x: i32,
    y: i32,
    reserved: i32,
    hwnd: HWND,
    rect: Option<*const RECT>,
) -> i32 {
    unsafe { TrackPopupMenu(menu, flags, x, y, Some(reserved), hwnd, rect).0 }
}

/// `UpdateLayeredWindow` (alpha-composited blit of the pill).
#[allow(clippy::too_many_arguments)]
pub unsafe fn update_layered_window(
    hwnd: HWND,
    dst_dc: Option<HDC>,
    dst_pos: Option<*const POINT>,
    size: Option<*const SIZE>,
    src_dc: HDC,
    src_pos: Option<*const POINT>,
    key: COLORREF,
    blend: Option<*const BLENDFUNCTION>,
    flags: UPDATE_LAYERED_WINDOW_FLAGS,
) -> Result<()> {
    unsafe { UpdateLayeredWindow(hwnd, dst_dc, dst_pos, size, Some(src_dc), src_pos, key, blend, flags) }
}

/// `ShellExecuteW`; the value is the HINSTANCE cast to `isize` (<= 32 = error).
pub unsafe fn shell_execute(
    hwnd: HWND,
    operation: PCWSTR,
    file: PCWSTR,
    params: Option<PCWSTR>,
    directory: Option<PCWSTR>,
    show: SHOW_WINDOW_CMD,
) -> isize {
    unsafe { ShellExecuteW(Some(hwnd), operation, file, params.as_ref(), directory.as_ref(), show).0 as isize }
}

/// `SetClipboardData`; transfers ownership of `mem` on success.
pub unsafe fn set_clipboard_data(format: u32, mem: HANDLE) -> Result<HANDLE> {
    unsafe { SetClipboardData(format, Some(mem)) }
}

/// `SetWinEventHook` (out-of-context foreground event hook).
pub unsafe fn set_win_event_hook(
    min: u32,
    max: u32,
    module: Option<HMODULE>,
    proc: WINEVENTPROC,
    process: u32,
    thread: u32,
    flags: u32,
) -> HWINEVENTHOOK {
    unsafe { SetWinEventHook(min, max, module, proc, process, thread, flags) }
}

/// `DeleteTimerQueueTimer` (waits for in-flight callbacks when `completion` is set).
pub unsafe fn delete_timer_queue_timer(queue: Option<HANDLE>, timer: HANDLE, completion: Option<HANDLE>) -> Result<()> {
    unsafe { DeleteTimerQueueTimer(queue, timer, completion) }
}

/// `CreateFileW`.
pub unsafe fn create_file(
    name: PCWSTR,
    access: u32,
    share: FILE_SHARE_MODE,
    security: Option<*const SECURITY_ATTRIBUTES>,
    disposition: FILE_CREATION_DISPOSITION,
    flags: FILE_FLAGS_AND_ATTRIBUTES,
    template_file: HANDLE,
) -> Result<HANDLE> {
    unsafe { CreateFileW(name, access, share, security, disposition, flags, Some(template_file)) }
}

/// `RegSetValueExW`; returns the `WIN32_ERROR` (0 = success).
pub unsafe fn reg_set_value(
    key: HKEY,
    name: PCWSTR,
    ty: REG_VALUE_TYPE,
    data: Option<&[u8]>,
) -> windows::Win32::Foundation::WIN32_ERROR {
    unsafe { RegSetValueExW(key, name, Some(0), ty, data) }
}

/// `GlobalFree`; returns the handle only when the free failed.
pub unsafe fn global_free(mem: HGLOBAL) -> Result<HGLOBAL> {
    unsafe { GlobalFree(Some(mem)) }
}

/// `CreateDIBSection`; `dc` and `section` may be `None` for a device- and
/// file-independent bitmap.
pub unsafe fn create_dib_section(
    dc: Option<HDC>,
    info: *const BITMAPINFO,
    usage: DIB_USAGE,
    bits: *mut *mut c_void,
    section: Option<HANDLE>,
    offset: u32,
) -> Result<HBITMAP> {
    unsafe { CreateDIBSection(dc, info, usage, bits, section, offset) }
}

/// `CreateWindowExW`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn create_window(
    ex_style: WINDOW_EX_STYLE,
    class: PCWSTR,
    name: PCWSTR,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    parent: Option<HWND>,
    menu: Option<HMENU>,
    instance: HINSTANCE,
    param: Option<*const c_void>,
) -> Result<HWND> {
    unsafe {
        CreateWindowExW(
            ex_style,
            class,
            name,
            style,
            x,
            y,
            w,
            h,
            parent,
            menu,
            Some(instance),
            param,
        )
    }
}

/// `SelectObject`; returns the previously selected object.
pub unsafe fn select_object(dc: HDC, obj: impl Into<HGDIOBJ>) -> HGDIOBJ {
    unsafe { SelectObject(dc, obj.into()) }
}

/// `DeleteObject`; true when the object was deleted.
pub unsafe fn delete_object(obj: impl Into<HGDIOBJ>) -> bool {
    unsafe { DeleteObject(obj.into()).as_bool() }
}

/// `CreateFontW`; charset/precision/quality are raw `u32` here so the call
/// sites are unaffected by the 0.62 typed-newtype change.
#[allow(clippy::too_many_arguments)]
pub unsafe fn create_font(
    height: i32,
    width: i32,
    escapement: i32,
    orientation: i32,
    weight: i32,
    italic: u32,
    underline: u32,
    strikeout: u32,
    charset: u32,
    out_precision: u32,
    clip_precision: u32,
    quality: u32,
    pitch_family: u32,
    name: PCWSTR,
) -> HFONT {
    unsafe {
        CreateFontW(
            height,
            width,
            escapement,
            orientation,
            weight,
            italic,
            underline,
            strikeout,
            FONT_CHARSET(charset as u8),
            FONT_OUTPUT_PRECISION(out_precision as u8),
            FONT_CLIP_PRECISION(clip_precision as u8),
            FONT_QUALITY(quality as u8),
            pitch_family,
            name,
        )
    }
}
