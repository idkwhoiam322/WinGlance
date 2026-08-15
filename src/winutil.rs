use log::{debug, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, DeleteObject, GetSysColor, HBRUSH, HGDIOBJ};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, HCURSOR, IDC_ARROW, LoadCursorW, RegisterClassExW,
    SPI_GETCLIENTAREAANIMATION, SPI_GETDISABLEOVERLAPPEDCONTENT, SPI_GETFOCUSBORDERWIDTH, SPI_GETHIGHCONTRAST,
    SPI_GETMESSAGEDURATION, SYSTEM_PARAMETERS_INFO_ACTION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowLongPtrW,
    SystemParametersInfoW, WNDCLASS_STYLES, WNDCLASSEXW, WNDPROC,
};
use windows::core::{PCWSTR, PWSTR};

/// Registers a window class exactly once per window type (guarded by `guard`).
/// The shared boilerplate lives here: arrow cursor, default style and extra
/// bytes, the `RegisterClassExW` call, and the failure path. `background` is
/// called lazily (only when the class is not yet registered) and must return
/// an owned brush the helper deletes if registration fails — stock or null
/// brushes must return `None`, since stock objects must never be deleted.
/// Logs `description` on failure.
pub(crate) fn register_class_once(
    guard: &'static OnceLock<()>,
    instance: HINSTANCE,
    class_name: &[u16],
    window_proc: WNDPROC,
    background: impl FnOnce() -> Option<HBRUSH>,
    description: &str,
) -> Result<(), windows::core::Error> {
    if guard.get().is_some() {
        return Ok(());
    }
    // A null class cursor falls back to the default arrow, so a failed load
    // must never panic window registration.
    let cursor = match unsafe { LoadCursorW(None, IDC_ARROW) } {
        Ok(cursor) => cursor,
        Err(error) => {
            warn!("LoadCursorW(IDC_ARROW) failed: {error}; the class will use the default cursor");
            HCURSOR::default()
        }
    };
    let background = background();
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: window_proc,
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hbrBackground: background.unwrap_or_default(),
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        if let Some(brush) = background {
            let _ = unsafe { DeleteObject(HGDIOBJ(brush.0)) };
        }
        warn!("RegisterClassExW failed for {description}");
        return Err(windows::core::Error::from_win32());
    }
    let _ = guard.set(());
    Ok(())
}

/// Stores a window's per-instance state pointer in its GWLP_USERDATA slot.
/// The pointer is a leaked box owned by the window, freed in WM_NCDESTROY.
pub(crate) fn set_window_state<T>(hwnd: HWND, state: *mut T) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    }
}

/// Clears a window's GWLP_USERDATA slot after WM_NCDESTROY freed the state
/// box, so a stale pointer is never read from a reused window handle.
pub(crate) fn clear_window_state(hwnd: HWND) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }
}

/// Reads a window's per-instance state pointer from GWLP_USERDATA. Returns
/// null when the slot is empty or was cleared.
pub(crate) fn window_state<T>(hwnd: HWND) -> *mut T {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut T }
}

/// System accessibility/appearance preferences, sampled at startup
/// and re-sampled on every `WM_SETTINGCHANGE`. A `Copy`-sized snapshot behind
/// a lock, so any thread reads the current values cheaply; an SPI query that
/// fails keeps the documented default rather than failing the sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SystemPreferences {
    /// `SPI_GETCLIENTAREAANIMATION`: the user allows animated client-area
    /// content. `false` disables every WinGlance motion.
    pub client_area_animation: bool,
    /// `SPI_GETDISABLEOVERLAPPEDCONTENT`: the user asked to minimize
    /// translucent/overlapped decoration.
    pub disable_overlapped_content: bool,
    /// `SPI_GETHIGHCONTRAST` with `HCF_HIGHCONTRASTON`: a high-contrast
    /// theme is active.
    pub high_contrast: bool,
    /// `SPI_GETMESSAGEDURATION`: how long the user wants transient messages
    /// on screen, in milliseconds.
    pub message_duration_ms: u32,
    /// `SPI_GETFOCUSBORDERWIDTH`/`-HEIGHT`: the focus indication thickness
    /// the user chose, in pixels (1 when the query fails).
    pub focus_border_px: u32,
}

impl SystemPreferences {
    /// The safe defaults every failed query falls back to: animation and
    /// color allowed, no overlapped-content restriction, Windows' 5 s toast
    /// default, a 1 px focus border.
    pub(crate) const DEFAULT: SystemPreferences = SystemPreferences {
        client_area_animation: true,
        disable_overlapped_content: false,
        high_contrast: false,
        message_duration_ms: 5_000,
        focus_border_px: 1,
    };

    /// Motion is allowed only when the user has not turned client-area
    /// animation off and has not asked for overlapped/translucent content to
    /// be minimized; either preference alone makes every animation
    /// immediate/static.
    pub(crate) fn animations_enabled(&self) -> bool {
        self.client_area_animation && !self.disable_overlapped_content
    }

    /// Queries the live system preferences. Every query failure keeps the
    /// corresponding default and logs at debug level: preference sampling
    /// must never be a startup failure.
    pub(crate) fn sample() -> Self {
        let mut prefs = Self::DEFAULT;
        unsafe {
            query_bool(SPI_GETCLIENTAREAANIMATION, &mut prefs.client_area_animation);
            query_bool(SPI_GETDISABLEOVERLAPPEDCONTENT, &mut prefs.disable_overlapped_content);
            let mut high_contrast = HIGHCONTRASTW {
                cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
                dwFlags: HCF_HIGHCONTRASTON,
                lpszDefaultScheme: PWSTR::null(),
            };
            if let Ok(()) = SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                0,
                Some(&mut high_contrast as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            ) {
                prefs.high_contrast = (high_contrast.dwFlags & HCF_HIGHCONTRASTON).0 != 0;
            } else {
                debug!("SPI_GETHIGHCONTRAST failed; keeping the default");
            }
            let mut duration: u32 = 0;
            // SPI_GETMESSAGEDURATION reports whole seconds (the live system
            // answers 5 for Windows' default "5 seconds" toast duration).
            if let Ok(()) = SystemParametersInfoW(
                SPI_GETMESSAGEDURATION,
                0,
                Some(&mut duration as *mut u32 as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            ) && duration > 0
            {
                prefs.message_duration_ms = duration.saturating_mul(1000);
            }
            let mut border: u32 = 0;
            if let Ok(()) = SystemParametersInfoW(
                SPI_GETFOCUSBORDERWIDTH,
                0,
                Some(&mut border as *mut u32 as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            ) && border > 0
            {
                prefs.focus_border_px = border;
            }
        }
        prefs
    }
}

/// Reads a boolean SPI into `out` on success; a failure keeps the default.
unsafe fn query_bool(action: SYSTEM_PARAMETERS_INFO_ACTION, out: &mut bool) {
    let mut value = BOOL(0);
    let queried = unsafe {
        SystemParametersInfoW(
            action,
            0,
            Some(&mut value as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if queried.is_ok() {
        *out = value.as_bool();
    } else {
        debug!("SystemParametersInfoW({action:?}) failed; keeping the default");
    }
}

static SYSTEM_PREFERENCES: RwLock<SystemPreferences> = RwLock::new(SystemPreferences::DEFAULT);

/// Re-samples the system preferences, stores them, and returns the snapshot.
/// Called at startup and from `WM_SETTINGCHANGE`.
pub(crate) fn refresh_system_preferences() -> SystemPreferences {
    let sampled = SystemPreferences::sample();
    match SYSTEM_PREFERENCES.write() {
        Ok(mut slot) => *slot = sampled,
        Err(poisoned) => *poisoned.into_inner() = sampled,
    }
    sampled
}

/// The most recently sampled system preferences.
pub(crate) fn system_preferences() -> SystemPreferences {
    match SYSTEM_PREFERENCES.read() {
        Ok(slot) => *slot,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// Whether motion is currently allowed (client-area animation on, overlapped
/// content not minimized).
pub(crate) fn animations_enabled() -> bool {
    system_preferences().animations_enabled()
}

/// The current system window color as an opaque RGBA array (high-contrast
/// themes repaint their surfaces through this).
pub(crate) fn system_window_color() -> [u8; 4] {
    let color = unsafe { GetSysColor(COLOR_WINDOW) };
    [
        (color & 0xFF) as u8,
        ((color >> 8) & 0xFF) as u8,
        ((color >> 16) & 0xFF) as u8,
        0xFF,
    ]
}

/// Tracks whether a window's WM_NCCREATE took ownership of the state box
/// passed through `lpCreateParams`, so the box's creator can tell, after a
/// failed `CreateWindowExW`, whether the window claimed it (and frees it in
/// WM_NCDESTROY) or whether it never materialized and the creator must free
/// it. Window creation is single-threaded on the UI thread, so a plain
/// atomic flag is race-free.
pub(crate) struct StateClaim {
    claimed: AtomicBool,
}

impl StateClaim {
    pub(crate) const fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
        }
    }

    /// Arms the flag before `CreateWindowExW`; `WM_NCCREATE` flips it via
    /// `claim` when the window object materializes.
    pub(crate) fn reset(&self) {
        self.claimed.store(false, Ordering::SeqCst);
    }

    /// Marks the state box as taken by the window. WM_NCCREATE calls this
    /// after storing `lpCreateParams` in GWLP_USERDATA.
    pub(crate) fn claim(&self) {
        self.claimed.store(true, Ordering::SeqCst);
    }

    /// Returns the state box to the caller when WM_NCCREATE never ran (a
    /// creation failure before the window object existed). `None` when the
    /// window owns the box — freeing it there would double-free, because the
    /// system tears the window down through WM_NCDESTROY first.
    pub(crate) fn take_unclaimed<T>(&self, state_ptr: *mut T) -> Option<Box<T>> {
        if self.claimed.load(Ordering::SeqCst) {
            None
        } else {
            // SAFETY: the caller created the box with `Box::into_raw` and
            // has not freed it; an unclaimed box is still owned by the
            // caller, so taking it back is sound.
            Some(unsafe { Box::from_raw(state_ptr) })
        }
    }
}

/// Closes a window whose handle is registered in a `OnceLock<Mutex<T>>`
/// slot, e.g. the positioner or the process picker. `extract` pulls the
/// window handle out of the slot's value (returning `None` when no window is
/// open). The handle is copied out and the guard released before
/// `DestroyWindow`: the destruction messages (WM_DESTROY/WM_NCDESTROY) lock
/// the same slot again, and holding the mutex across `DestroyWindow` would
/// deadlock the UI thread.
pub(crate) fn close_registered<T>(slot: &OnceLock<Mutex<T>>, extract: impl FnOnce(&T) -> Option<HWND>) {
    let Some(m) = slot.get() else {
        return;
    };
    let hwnd = {
        let Ok(guard) = m.lock() else {
            return;
        };
        extract(&*guard)
    };
    if let Some(hwnd) = hwnd {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// UTF-16-encodes `value` with a trailing NUL terminator suitable for the
/// `PCWSTR` Win32 APIs. Single source of truth — used by `overlay`,
/// `main_window`, `positioner`, `autostart`, `process_picker`, and `main`.
pub(crate) fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

// ────────────────────────────────────────────────────────────────────────────
// Verified app-data writes (reparse-safe: the data root and temp file are identity-checked)
//
// The threat model: path-based opens follow reparse points. A junction swapped
// into the data/log directory, or a symlink pre-created at a fixed temp name,
// can redirect a write to a location the user did not approve (confused
// deputy). Every write in the app therefore lands either verified or not at
// all:
//   - the target's parent is opened WITHOUT `FILE_SHARE_DELETE`, pinning it so
//     it cannot be renamed or removed for the duration of the operation, and
//     with `FILE_FLAG_OPEN_REPARSE_POINT` so the opened object's own reparse
//     attribute is visible;
//   - the opened parent is rejected if it IS a reparse point, is not a
//     directory, or its canonical final handle path does not equal the
//     caller's expected path (this comparison also rejects any junction in an
//     intermediate component — the final path would resolve to the link
//     target, not the expected path);
//   - temp files use randomized `CREATE_NEW` names with
//     `FILE_FLAG_OPEN_REPARSE_POINT`, so a pre-created link at that name can
//     never be followed (the create fails instead);
//   - the commit is a rename of the temp onto the target name through
//     `SetFileInformationByHandle(FileRenameInfo)` with `ReplaceIfExists`,
//     which exchanges the directory entry atomically without following the
//     target's own reparse point. Windows rejects the root-relative rename
//     forms (`FileRenameInfoEx`/`FileRenameInfo` with `RootDirectory` set
//     return ERROR_INVALID_PARAMETER), so the documented full `\\?\` path
//     form is used; the parent held pinned for the transaction makes the path
//     un-redirectable while the commit runs. The parent directory handle is
//     then flushed (opened with `FILE_WRITE_DATA`/`FILE_APPEND_DATA`
//     directory-equivalents so the flush is permitted) for the rename's
//     write-through durability);
//   - on any pre-commit failure the temp is deleted via its handle and the
//     error is returned; callers log it and never fall back to a plain
//     relative path.
// ────────────────────────────────────────────────────────────────────────────

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::RawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
    FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FlushFileBuffers, GetFileInformationByHandle, GetFinalPathNameByHandleW,
    OPEN_ALWAYS, OPEN_EXISTING, SetFileInformationByHandle, WriteFile,
};

/// `FILE_DELETE_CHILD` (0x0040); `windows` 0.58 does not export it. Needed on
/// the pinned parent so a rename-with-replace may exchange a child entry.
const FILE_DELETE_CHILD: u32 = 0x0000_0040;

/// The Win32 DELETE access right (0x0001_0000); `windows` 0.58 does not export
/// it. Needed so the temp's handle can also delete it (disposition delete).
const DELETE_ACCESS: u32 = 0x0001_0000;
/// `FILE_DISPOSITION_FLAG_DELETE` from winnt.h; not exported by `windows`
/// 0.58.
const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x0000_0001;
/// `SetFileInformationByHandle` disposition-ex information class from winnt.h.
/// The `windows` crate exports the `FileDispositionInfoEx` *value* but not
/// the struct definition it pairs with, so both live here (documented, stable
/// ABI: 21 = disposition-ex).
const DISPOSITION_INFO_EX_CLASS: i32 = 21;

/// `FileDispositionInfoEx` (winnt.h).
#[repr(C)]
#[derive(Clone, Copy)]
struct FileDispositionInfoEx {
    flags: u32,
}

/// Converts a `windows`-crate error into a `std::io::Error`, extracting the
/// Win32 code (facility 7) so callers see the underlying OS error.
fn to_io(error: windows::core::Error) -> io::Error {
    let code = error.code().0 as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        io::Error::other(error)
    }
}

/// True when the Win32 error carries the given Win32 code.
fn is_win32_code(error: &windows::core::Error, code: u32) -> bool {
    let raw = error.code().0 as u32;
    raw & 0xFFFF_0000 == 0x8007_0000 && raw & 0xFFFF == code
}

/// ASCII-insensitive comparison of two paths on their UTF-16 forms, so a
/// `\\?\C:\...` final handle path compares equal to the caller's expected
/// path regardless of casing.
pub(crate) fn paths_equal(a: &Path, b: &Path) -> bool {
    let wa = a.as_os_str().encode_wide().collect::<Vec<_>>();
    let wb = b.as_os_str().encode_wide().collect::<Vec<_>>();
    fn fold(unit: u16) -> u16 {
        if (0x41..=0x5A).contains(&unit) {
            unit + 0x20
        } else {
            unit
        }
    }
    wa.len() == wb.len() && wa.iter().zip(wb.iter()).all(|(x, y)| fold(*x) == fold(*y))
}

/// The `\\?\` extended-length form of `path`, the form
/// `GetFinalPathNameByHandleW` yields, so the two are directly comparable.
pub(crate) fn extended_path(path: &Path) -> PathBuf {
    let raw = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let already_extended = raw.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]);
    if already_extended {
        path.to_path_buf()
    } else {
        let mut prefixed = Vec::with_capacity(raw.len() + 4);
        prefixed.extend_from_slice(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]);
        prefixed.extend_from_slice(&raw);
        PathBuf::from(OsString::from_wide(&prefixed))
    }
}

/// Canonical final path (`\\?\C:\...`) of an open handle, via
/// `GetFinalPathNameByHandleW`. The call describes the opened object itself —
/// it never re-resolves the path through the filesystem — so a handle opened
/// with `FILE_FLAG_OPEN_REPARSE_POINT` reports the link's own path, and a
/// handle opened normally reports the resolved target.
fn final_path_of_raw(handle: HANDLE) -> io::Result<PathBuf> {
    let mut capacity = 256u32;
    loop {
        let mut buf = vec![0u16; capacity as usize];
        let len = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                &mut buf,
                windows::Win32::Storage::FileSystem::GETFINALPATHNAMEBYHANDLE_FLAGS(0),
            )
        };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if (len as usize) < buf.len() {
            let mut units = &buf[..len as usize];
            if units.last() == Some(&0) {
                units = &units[..units.len() - 1];
            }
            return Ok(PathBuf::from(OsString::from_wide(units)));
        }
        capacity = len + 1;
    }
}

/// `final_path_of_raw` for any Rust handle (e.g. a `std::fs::File`).
pub(crate) fn final_path_of(raw: RawHandle) -> io::Result<PathBuf> {
    final_path_of_raw(HANDLE(raw))
}

/// A pinned, verified directory handle. While the guard lives the directory
/// cannot be renamed or removed (opened without `FILE_SHARE_DELETE`) and the
/// opened object is a plain directory (no reparse attribute).
pub(crate) struct DirGuard {
    handle: HANDLE,
}

impl Drop for DirGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Opens `dir`, pinned and verified, as the root for a write transaction.
/// Rejects: a missing directory (caller creates it first), a reparse point,
/// a non-directory, or a final handle path that differs from the expected
/// path (which also flags junctions in intermediate components).
pub(crate) fn open_pinned_parent(dir: &Path) -> io::Result<DirGuard> {
    let desired = (FILE_LIST_DIRECTORY
        | FILE_READ_ATTRIBUTES
        | FILE_WRITE_ATTRIBUTES
        | FILE_TRAVERSE
        | FILE_WRITE_DATA // = FILE_ADD_FILE: lets the flush write directory entries through
        | FILE_APPEND_DATA // = FILE_ADD_SUBDIRECTORY
        | windows::Win32::Storage::FileSystem::FILE_ACCESS_RIGHTS(FILE_DELETE_CHILD))
    .0;
    let wide = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            desired,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(to_io)?;

    let reject = |message: &str| {
        unsafe {
            let _ = CloseHandle(handle);
        }
        Err(io::Error::other(format!("{message} ({})", dir.display())))
    };

    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if let Err(error) = unsafe { GetFileInformationByHandle(handle, &mut info) } {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(to_io(error));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return reject("refusing to write through a reparse point");
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        return reject("write root is not a directory");
    }
    let final_path = final_path_of_raw(handle)?;
    if !paths_equal(&final_path, &extended_path(dir)) {
        return reject(&format!(
            "write root final path does not match the expected path (resolved to {})",
            final_path.display()
        ));
    }
    Ok(DirGuard { handle })
}

/// Randomized temp name: pid + sequence + subsecond clock, so no fixed name
/// can be pre-armed (CREATE_NEW + OPEN_REPARSE_POINT defeat a guessed link
/// anyway).
fn temp_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("wg-{:x}-{:x}-{:x}.tmp", std::process::id(), seq, nanos)
}

/// Deletes the temp through its own handle (disposition-delete) and closes
/// it. Only called on failure paths; best-effort.
fn delete_temp(handle: HANDLE) {
    let info = FileDispositionInfoEx {
        flags: FILE_DISPOSITION_FLAG_DELETE,
    };
    unsafe {
        let _ = SetFileInformationByHandle(
            handle,
            windows::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS(DISPOSITION_INFO_EX_CLASS),
            (&info as *const FileDispositionInfoEx).cast(),
            std::mem::size_of::<FileDispositionInfoEx>() as u32,
        );
        let _ = CloseHandle(handle);
    }
}

/// Atomically replaces `target` with `content` under the verified-write
/// discipline above. On any pre-commit failure the temp is deleted and `Err`
/// returns; the existing `target` entry is untouched. The write is durable
/// through the rename: the data is flushed, the directory entry is exchanged
/// relative to the held parent, and the parent directory handle is flushed
/// for the metadata change.
pub(crate) fn atomic_replace_file(target: &Path, content: &[u8]) -> io::Result<()> {
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"))?;
    let name_units = name.encode_wide().collect::<Vec<u16>>();
    if name_units.is_empty()
        || name_units.len() > 255
        || name_units.contains(&0)
        || name_units.contains(&(b'\\' as u16))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target name is not a single path component",
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no parent directory"))?;
    if !parent.exists() {
        std::fs::create_dir_all(parent)?;
    }
    let guard = open_pinned_parent(parent)?;

    for _ in 0..4 {
        let tmp_name = temp_name();
        let tmp_path = parent.join(&tmp_name);
        let wide = tmp_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let temp_handle = match unsafe {
            CreateFileW(
                windows::core::PCWSTR(wide.as_ptr()),
                FILE_GENERIC_WRITE.0 | DELETE_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        } {
            Ok(handle) => handle,
            Err(error) if is_win32_code(&error, 183) => continue, // ERROR_ALREADY_EXISTS: name collision, retry
            Err(error) => return Err(to_io(error)),
        };

        // Belt and braces: the temp we just created resolves exactly where the
        // pinned parent says it should. Unreachable in practice (the parent
        // cannot move and CREATE_NEW cannot follow a link), kept as a hard
        // invariant. The expected path is the temp's own entry inside the
        // pinned parent, not the final target name.
        let temp_expected = extended_path(&tmp_path);
        let fail = move |message: &str, handle: HANDLE| {
            delete_temp(handle);
            Err(io::Error::other(message))
        };
        let temp_final = match final_path_of_raw(temp_handle) {
            Ok(path) => path,
            Err(error) => {
                delete_temp(temp_handle);
                return Err(error);
            }
        };
        if !paths_equal(&temp_final, &temp_expected) {
            return fail(
                &format!(
                    "temp file resolved outside the verified parent ({} vs {})",
                    temp_final.display(),
                    temp_expected.display()
                ),
                temp_handle,
            );
        }

        if let Err(error) = unsafe { WriteFile(temp_handle, Some(content), None, None) } {
            delete_temp(temp_handle);
            return Err(to_io(error));
        }
        if let Err(error) = unsafe { FlushFileBuffers(temp_handle) } {
            delete_temp(temp_handle);
            return Err(to_io(error));
        }

        // Commit: rename the temp onto the target name. Windows rejects the
        // root-relative form of the rename info (classes 3 and 22 return
        // ERROR_INVALID_PARAMETER with `RootDirectory` set on this build), so
        // the documented full-path form is used: `FileRenameInfo` (class 3,
        // the layout the `tempfile` ecosystem relies on) with the target's
        // `\\?\`-extended path. Replacing the existing target entry (if any)
        // is atomic and never follows the target's own reparse point; the
        // parent was verified and is held pinned, so the path cannot be
        // redirected under the caller.
        let target_units = extended_path(target).as_os_str().encode_wide().collect::<Vec<u16>>();
        let mut raw = vec![0u8; 20 + target_units.len() * 2];
        raw[0] = 1; // ReplaceIfExists
        // raw[8..16] stays zero: RootDirectory = NULL (full path).
        raw[16..20].copy_from_slice(&(target_units.len() as u32 * 2).to_le_bytes());
        for (i, unit) in target_units.iter().enumerate() {
            let offset = 20 + i * 2;
            raw[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        if let Err(error) = unsafe {
            SetFileInformationByHandle(
                temp_handle,
                windows::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS(
                    windows::Win32::Storage::FileSystem::FileRenameInfo.0,
                ),
                raw.as_ptr().cast(),
                raw.len() as u32,
            )
        } {
            delete_temp(temp_handle);
            return Err(io::Error::other(format!("rename commit failed: {}", to_io(error))));
        }

        // Durability of the directory-entry change (the rename's write-through
        // intent): flush the parent directory handle.
        if let Err(error) = unsafe { FlushFileBuffers(guard.handle) } {
            unsafe {
                let _ = CloseHandle(temp_handle);
            }
            return Err(io::Error::other(format!("parent flush failed: {}", to_io(error))));
        }
        unsafe {
            let _ = CloseHandle(temp_handle);
        }
        return Ok(());
    }
    Err(io::Error::other("could not create a unique temp file after 4 attempts"))
}

/// Appends `data` to `path`, verifying the parent's identity and opening the
/// final component with `FILE_FLAG_OPEN_REPARSE_POINT` so a pre-created
/// symlink is never followed (an append goes to the link's own entry or
/// fails, never to the link target). Used by the crash.log writers, which
/// run where a full temp+rename transaction is not warranted.
pub(crate) fn append_verified(path: &Path, data: &[u8]) -> io::Result<()> {
    append_verified_bounded(path, data, u64::MAX)
}

/// `append_verified` with a size cap: when the file already exceeds `cap`,
/// it is truncated to zero before the append, so a crash loop cannot grow it
/// without bound (matches the cap applied on the allocation-free handler
/// path).
pub(crate) fn append_verified_bounded(path: &Path, data: &[u8], cap: u64) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
        let _guard = open_pinned_parent(parent)?;
    }
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            (FILE_APPEND_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(to_io)?;
    let result = (|| -> io::Result<()> {
        let final_path = final_path_of_raw(handle)?;
        if !paths_equal(&final_path, &extended_path(path)) {
            return Err(io::Error::other(format!(
                "append target final path does not match the expected path (resolved to {})",
                final_path.display()
            )));
        }
        if cap != u64::MAX {
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            unsafe { GetFileInformationByHandle(handle, &mut info) }.map_err(to_io)?;
            let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
            if size > cap {
                unsafe {
                    let _ = windows::Win32::Storage::FileSystem::SetFilePointer(
                        handle,
                        0,
                        None,
                        windows::Win32::Storage::FileSystem::FILE_BEGIN,
                    );
                    let _ = windows::Win32::Storage::FileSystem::SetEndOfFile(handle);
                }
            }
        }
        // With FILE_WRITE_DATA granted (needed for the truncation above) the
        // OS no longer writes at EOF automatically, so position the pointer
        // explicitly before every append.
        unsafe {
            let _ = windows::Win32::Storage::FileSystem::SetFilePointer(
                handle,
                0,
                None,
                windows::Win32::Storage::FileSystem::FILE_END,
            );
        }
        unsafe { WriteFile(handle, Some(data), None, None) }.map_err(to_io)
    })();
    unsafe {
        let _ = CloseHandle(handle);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn preference_motion_mapping_requires_animation_and_plain_content() {
        // Motion needs client-area animation allowed AND overlapped content
        // not minimized; either restriction alone makes animations
        // immediate/static when system preferences disallow motion.
        assert!(SystemPreferences::DEFAULT.animations_enabled());
        let base = SystemPreferences::DEFAULT;
        assert!(
            !SystemPreferences {
                client_area_animation: false,
                ..base
            }
            .animations_enabled()
        );
        assert!(
            !SystemPreferences {
                disable_overlapped_content: true,
                ..base
            }
            .animations_enabled()
        );
        assert!(
            !SystemPreferences {
                client_area_animation: false,
                disable_overlapped_content: true,
                ..base
            }
            .animations_enabled()
        );
    }

    #[test]
    fn sampled_preferences_stay_within_sane_bounds() {
        // Sampling runs in the live session: values outside these bounds
        // would mean a broken query result, not a user preference.
        let prefs = SystemPreferences::sample();
        assert!(prefs.message_duration_ms > 0 && prefs.message_duration_ms <= 600_000);
        assert!(prefs.focus_border_px >= 1 && prefs.focus_border_px <= 32);
    }

    #[test]
    fn refresh_returns_and_stores_the_same_snapshot() {
        let sampled = refresh_system_preferences();
        let read_back = system_preferences();
        assert_eq!(sampled, read_back);
        // Restore whatever the session actually reports so other tests (and
        // this one, re-run) observe live values, not this test's write.
        refresh_system_preferences();
    }

    /// A uniquely-named temporary directory removed on drop.
    struct TestDir {
        dir: PathBuf,
    }

    impl TestDir {
        fn new(tag: &str) -> Self {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("winglance-winutil-{tag}-{}-{stamp}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn sibling_names(dir: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn normal_replace_writes_content_and_leaves_no_temp() {
        let guard = TestDir::new("replace");
        let target = guard.dir.join("config.toml");
        std::fs::write(&target, b"old").unwrap();

        atomic_replace_file(&target, b"new content").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new content");
        assert_eq!(sibling_names(&guard.dir), vec!["config.toml"], "no temp may remain");
    }

    #[test]
    fn replace_creates_a_missing_parent_chain() {
        let guard = TestDir::new("deep");
        let target = guard.dir.join("a").join("b").join("config.toml");
        atomic_replace_file(&target, b"x").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"x");
    }

    #[test]
    fn replace_over_a_directory_fails_and_cleans_the_temp() {
        let guard = TestDir::new("over-dir");
        let target = guard.dir.join("config.toml");
        std::fs::create_dir(&target).unwrap();

        assert!(atomic_replace_file(&target, b"x").is_err());
        assert!(target.is_dir(), "the target entry must be left as-is");
        assert_eq!(sibling_names(&guard.dir), vec!["config.toml"], "no temp may remain");
    }

    #[test]
    fn replace_rejects_a_reparse_point_parent() {
        // Needs SeCreateSymbolicLinkPrivilege (Developer Mode / admin);
        // skipped when the OS refuses to create the link.
        let guard = TestDir::new("reparse-parent");
        let link = guard.dir.join("evil");
        let real = guard.dir.join("real");
        std::fs::create_dir_all(real.join("target")).unwrap();
        if std::os::windows::fs::symlink_dir(&real, &link).is_err() {
            return;
        }

        let target = link.join("config.toml");
        assert!(atomic_replace_file(&target, b"x").is_err());
        assert!(
            !real.join("target").join("config.toml").exists(),
            "nothing may be written through the link"
        );
    }

    #[test]
    fn replace_rejects_a_reparse_point_in_an_intermediate_component() {
        // `evil` is a directory link whose target is `real`: the write root
        // `evil\sub` resolves through the link, so its final handle path is
        // `real\sub` — the identity check must reject it even though the
        // opened component itself is an ordinary directory.
        let guard = TestDir::new("reparse-mid");
        let real = guard.dir.join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let evil = guard.dir.join("evil");
        if std::os::windows::fs::symlink_dir(&real, &evil).is_err() {
            return;
        }

        let target = evil.join("sub").join("config.toml");
        assert!(atomic_replace_file(&target, b"x").is_err());
        assert!(
            !real.join("sub").join("config.toml").exists(),
            "nothing may be written through the link"
        );
    }

    #[test]
    fn replace_replaces_a_target_symlink_without_following_it() {
        let guard = TestDir::new("target-link");
        let real = guard.dir.join("real-file");
        std::fs::write(&real, b"target").unwrap();
        let target = guard.dir.join("config.toml");
        if std::os::windows::fs::symlink_file(&real, &target).is_err() {
            return;
        }

        atomic_replace_file(&target, b"x").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"x", "the link entry must be replaced");
        assert_eq!(
            std::fs::read(&real).unwrap(),
            b"target",
            "the link target must never receive the write"
        );
    }

    #[test]
    fn append_verified_writes_and_obeys_the_cap() {
        let guard = TestDir::new("append");
        let path = guard.dir.join("crash.log");

        append_verified_bounded(&path, b"first\n", 100).unwrap();
        append_verified_bounded(&path, b"second\n", 100).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first\nsecond\n");

        // A file past the cap is truncated before the next append, so a crash
        // loop stays bounded: the file ends with only the latest line.
        append_verified_bounded(&path, b"third\n", 6).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"third\n");
    }

    #[test]
    fn append_verified_rejects_a_reparse_point_parent() {
        let guard = TestDir::new("append-reparse");
        let link = guard.dir.join("evil");
        let real = guard.dir.join("real");
        std::fs::create_dir_all(real.join("target")).unwrap();
        if std::os::windows::fs::symlink_dir(&real, &link).is_err() {
            return;
        }

        assert!(append_verified(&link.join("crash.log"), b"x").is_err());
        assert!(!real.join("target").join("crash.log").exists());
    }
}
