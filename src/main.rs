#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod accessibility;
mod autostart;
mod config;
mod duration_dialog;
mod events;
mod gdi;
mod icon;
mod logging;
mod main_window;
mod overlay;
mod palette;
mod positioner;
mod process_picker;
mod smtc;
mod winapi;
mod winutil;

use crate::config::Config;
use crate::winapi::{create_file, post_message};
use anyhow::Result;
use log::{debug, error, info, warn};
use std::collections::VecDeque;
use std::env;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0,
    WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Storage::FileSystem::{
    FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_BEGIN, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA, GetFileSize, OPEN_ALWAYS, SetEndOfFile, SetFilePointer,
    WriteFile,
};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS, RtlCaptureStackBackTrace,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcessId, OpenEventW, ReleaseMutex, SetEvent,
    WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, DispatchMessageW, GetMessageW, TranslateMessage};
use windows::core::PCWSTR;

use crate::events::{MEDIA_EVENT_MSG, MediaEvent, artwork_bytes};
use crate::overlay::EventQueue;
use crate::winutil::wide;

static CRASH_LOG_PATH: OnceLock<Vec<u16>> = OnceLock::new();

/// Writes literal bytes into the stack buffer, truncating if the buffer is full.
/// Returns the new write position.
fn crash_write_str(buf: &mut [u8], pos: usize, s: &[u8]) -> usize {
    let end = (pos + s.len()).min(buf.len());
    buf[pos..end].copy_from_slice(&s[..end - pos]);
    end
}

/// Writes "0x" followed by exactly 16 lowercase hex digits.
fn crash_write_hex16(buf: &mut [u8], pos: usize, value: usize) -> usize {
    let p = crash_write_str(buf, pos, b"0x");
    let mut digits = [0u8; 16];
    let mut v = value;
    for i in (0..16).rev() {
        let nibble = (v & 0xF) as u8;
        digits[i] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + (nibble - 10)
        };
        v >>= 4;
    }
    crash_write_str(buf, p, &digits)
}

/// Writes "0x" followed by the minimum hex digits needed (no leading zeros).
fn crash_write_hex_any(buf: &mut [u8], pos: usize, value: usize) -> usize {
    let p = crash_write_str(buf, pos, b"0x");
    if value == 0 {
        return crash_write_str(buf, p, b"0");
    }
    let mut digits = [0u8; 16];
    let mut v = value;
    let mut len = 0usize;
    while v > 0 {
        let nibble = (v & 0xF) as u8;
        digits[15 - len] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + (nibble - 10)
        };
        v >>= 4;
        len += 1;
    }
    crash_write_str(buf, p, &digits[16 - len..])
}

/// Writes an unsigned integer in decimal (no leading zeros).
fn crash_write_dec(buf: &mut [u8], pos: usize, value: usize) -> usize {
    if value == 0 {
        return crash_write_str(buf, pos, b"0");
    }
    let mut digits = [0u8; 20];
    let mut v = value;
    let mut len = 0usize;
    while v > 0 {
        digits[19 - len] = b'0' + (v % 10) as u8;
        v /= 10;
        len += 1;
    }
    crash_write_str(buf, pos, &digits[20 - len..])
}

/// Diagnostic vectored exception handler: on an access violation it appends the
/// faulting instruction address and a raw backtrace to crash.log using only
/// stack-allocated buffers and raw Win32 file APIs — no `std::fs`, no `String`,
/// no `format!`, no heap allocation — then lets Windows continue with default
/// crash handling.  This is safe even when the access violation is a symptom of
/// heap corruption, because the handler never touches the allocator.
unsafe extern "system" fn crash_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    if info.is_null() {
        return 0;
    }
    let record_ptr = unsafe { (*info).ExceptionRecord };
    if record_ptr.is_null() {
        return 0;
    }
    let record = unsafe { &*record_ptr };
    if record.ExceptionCode.0 != 0xC000_0005u32 as i32 {
        return 0;
    }
    let addr = if record.NumberParameters >= 2 {
        record.ExceptionInformation[1]
    } else {
        0
    };
    let ip = record.ExceptionAddress as usize;
    let base = unsafe { GetModuleHandleW(None) }.map(|h| h.0 as usize).unwrap_or(0);
    let mut frames = [0usize; 24];
    let mut raw: [*mut c_void; 24] = [std::ptr::null_mut(); 24];
    let count = unsafe { RtlCaptureStackBackTrace(0, &mut raw, None) } as usize;
    for (i, r) in raw.iter().take(count).enumerate() {
        frames[i] = *r as usize;
    }

    // Build the entire crash log in a stack-allocated buffer.  No String,
    // no format!, no heap allocation — safe under heap corruption.
    let mut buf = [0u8; 2048];
    let mut pos = 0usize;
    pos = crash_write_str(&mut buf, pos, b"CRASH access violation\n");
    pos = crash_write_str(&mut buf, pos, b"  ip    = ");
    pos = crash_write_hex16(&mut buf, pos, ip);
    pos = crash_write_str(&mut buf, pos, b" (rva ");
    pos = crash_write_hex_any(&mut buf, pos, ip.wrapping_sub(base));
    pos = crash_write_str(&mut buf, pos, b")\n");
    pos = crash_write_str(&mut buf, pos, b"  addr  = ");
    pos = crash_write_hex16(&mut buf, pos, addr);
    pos = crash_write_str(&mut buf, pos, b"\n");
    pos = crash_write_str(&mut buf, pos, b"  base  = ");
    pos = crash_write_hex16(&mut buf, pos, base);
    pos = crash_write_str(&mut buf, pos, b"\n");
    for (i, f) in frames.iter().take(count).enumerate() {
        pos = crash_write_str(&mut buf, pos, b"  frame[");
        pos = crash_write_dec(&mut buf, pos, i);
        pos = crash_write_str(&mut buf, pos, b"] = ");
        pos = crash_write_hex16(&mut buf, pos, *f);
        pos = crash_write_str(&mut buf, pos, b" (rva ");
        pos = crash_write_hex_any(&mut buf, pos, f.wrapping_sub(base));
        pos = crash_write_str(&mut buf, pos, b")\n");
    }

    // Write via raw Win32 APIs: CreateFileW (append), WriteFile, CloseHandle.
    // The path was pre-computed as a null-terminated UTF-16 string at install
    // time, so no allocation happens here.
    if let Some(path) = CRASH_LOG_PATH.get() {
        let handle = unsafe {
            create_file(
                PCWSTR(path.as_ptr()),
                FILE_APPEND_DATA.0 | FILE_WRITE_DATA.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_ALWAYS,
                // FILE_FLAG_OPEN_REPARSE_POINT: the entry is opened without
                // following a pre-created crash.log symlink, so a hostile link
                // can redirect the crash write to nothing (the link entry
                // itself gets appended to or fails) — never to an
                // attacker-chosen target. This handler cannot verify the
                // surrounding path (allocation-free); the logs dir itself is
                // verified at startup by init_logging.
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                HANDLE::default(),
            )
        };
        if let Ok(handle) = handle {
            // Keep crash.log bounded even on the allocation-free handler path:
            // a crash loop must not grow it without limit.
            unsafe {
                if GetFileSize(handle, None) > CRASH_LOG_CAP as u32 {
                    let _ = SetFilePointer(handle, 0, None, FILE_BEGIN);
                    let _ = SetEndOfFile(handle);
                }
            }
            let mut written: u32 = 0;
            let _ = unsafe { WriteFile(handle, Some(&buf[..pos]), Some(&mut written as *mut _), None) };
            let _ = unsafe { CloseHandle(handle) };
        }
    }
    0 // EXCEPTION_CONTINUE_SEARCH
}

fn install_crash_handler(logs_dir: &Path) {
    // Pre-build the crash.log path as a null-terminated UTF-16 string so the
    // exception handler can pass it to CreateFileW without any heap allocation
    // during a crash.
    let full_path = logs_dir.join("crash.log");
    let wide: Vec<u16> = full_path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let _ = CRASH_LOG_PATH.set(wide);
    unsafe {
        AddVectoredExceptionHandler(1, Some(crash_handler));
    }
}

/// Writes Rust panics to crash.log. A panic in a window-proc unwinds across
/// the extern "C" boundary, which aborts the process silently (no access
/// violation, so the vectored handler never fires) — without this hook a
/// panic looks like the app "stopped running randomly". The file is capped so
/// a crash loop cannot grow it without bound.
fn install_panic_hook(logs_dir: &Path) {
    let dir = logs_dir.to_path_buf();
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown payload".to_string()
        };
        let location = info.location().map(|l| l.to_string()).unwrap_or_default();
        let message = format!("PANIC {payload} at {location}\n");
        let path = dir.join("crash.log");
        // Verified append — the parent's identity is checked and the
        // final component is opened without following a pre-created link. The
        // cap truncates the file when a crash loop would otherwise grow it
        // without bound.
        let _ = crate::winutil::append_verified_bounded(&path, message.as_bytes(), CRASH_LOG_CAP);
    }));
}

/// Upper bound on `crash.log` before the next panic truncates it.
const CRASH_LOG_CAP: u64 = 1024 * 1024;

/// Bounded waits for the restart handoff protocol. The old process gives the
/// successor at most `RESTART_READY_TIMEOUT` to signal ready, and the
/// successor (child) waits at most `RESTART_READY_WAIT` on the mutex the old
/// process releases on success.
const RESTART_READY_TIMEOUT: Duration = Duration::from_secs(10);
const RESTART_READY_WAIT: Duration = Duration::from_secs(15);

/// Owns the single-instance mutex handle for the process lifetime. The handle
/// is closed exactly once: either by `release` during a successful restart
/// handoff, or by the OS when the process ends (the guard is held in a
/// `static`, which is never dropped by the runtime, so `Drop` covers the
/// take-then-fail windows inside `relaunch_self` and nothing else). Stored
/// as a raw `isize` because `HANDLE` is neither `Send` nor `Sync`; the guard
/// is only ever touched on the main/UI thread, and the numeric value alone is
/// shareable through the `SINGLETON_GUARD` slot the restart path reads.
struct SingletonGuard {
    raw: isize,
}

impl SingletonGuard {
    fn new(handle: HANDLE) -> Self {
        Self { raw: handle.0 as isize }
    }

    /// Releases the mutex and closes the handle. Once released, the guard
    /// holds nothing: a later `Drop` is a no-op, so the handle cannot be
    /// released or closed twice.
    fn release(&mut self) {
        let handle = HANDLE(self.raw as *mut c_void);
        unsafe {
            let _ = ReleaseMutex(handle);
            let _ = CloseHandle(handle);
        }
        self.raw = 0;
    }
}

impl Drop for SingletonGuard {
    fn drop(&mut self) {
        if self.raw != 0 {
            self.release();
        }
    }
}

/// The singleton guard's raw handle value, mirrored so the UI-thread restart
/// path (`relaunch_self`) can hand the mutex to the successor process. The
/// value is taken out of the slot for the handoff and put back on any
/// failure, so `main` keeps exactly one live guard at all times.
static SINGLETON_GUARD: Mutex<Option<SingletonGuard>> = Mutex::new(None);

/// Whether a snapshot entry is a running instance of this app: the
/// executable name matches `WinGlance.exe` case-insensitively (NUL-padded as
/// Toolhelp reports it) and the pid is not this process's own. Pure, so the
/// duplicate-vs-squat classification is unit-testable without a process
/// snapshot.
fn is_our_instance(units: &[u16], pid: u32, our_pid: u32) -> bool {
    pid != our_pid
        && String::from_utf16_lossy(units)
            .trim_end_matches('\0')
            .eq_ignore_ascii_case("WinGlance.exe")
}

/// Whether another `WinGlance.exe` process is currently running in this
/// session, sampled from one Toolhelp snapshot and excluding this process.
/// Used only to classify a live-held single-instance mutex (see
/// `acquire_singleton`): a running instance means the duplicate launch is a
/// legitimate double-launch and may exit silently, while no running instance
/// means the mutex name is likely squatted by a foreign process, which is
/// worth reporting instead of failing without a trace. Diagnostic only — the
/// probe is not a security boundary, and a same-session adversary can evade
/// or spoof it (they can already stop or interfere with the app).
fn winglance_instance_running() -> bool {
    let our_pid = unsafe { GetCurrentProcessId() };
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
        };
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut running = false;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                if is_our_instance(&entry.szExeFile, entry.th32ProcessID, our_pid) {
                    running = true;
                    break;
                }
                if !Process32NextW(snapshot, &mut entry).is_ok() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        running
    }
}

/// Re-samples `winglance_instance_running` a few times so a just-started
/// legitimate instance — already past mutex creation but not yet visible in a
/// snapshot — is not mistaken for a squatter. Returns the last sample.
fn winglance_instance_running_retried() -> bool {
    for _ in 0..3 {
        if winglance_instance_running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(75));
    }
    false
}

/// Appends a diagnostic line to `crash.log` — the only log available before
/// `logging::init_logging` runs. Bounded like the panic hook so a launch
/// loop cannot grow the file without limit.
fn append_crash_log_line(message: &[u8]) {
    if let Some(dir) = config::Config::data_dir().ok().map(|d| d.join("logs")) {
        let _ = crate::winutil::append_verified_bounded(&dir.join("crash.log"), message, CRASH_LOG_CAP);
    }
}

/// Records a suspected single-instance squat to `crash.log`. The mutex is
/// live-held by a process that is not a running WinGlance instance, so this
/// launch exits with no feedback anywhere; without this line the app would
/// silently refuse to start.
fn report_suspected_squat() {
    append_crash_log_line(
        b"suspected single-instance mutex squat: 'WinGlanceSingleInstance' is held by a live process that is not a running WinGlance instance; this launch exits without starting\n",
    );
}

/// Acquires the single-instance mutex for the process lifetime. Returns the
/// guard while the caller holds it, or `None` when another instance already
/// owns the mutex. On the restart-handoff path (`restart_nonce` is `Some`),
/// the successor waits on the mutex the old process releases after the ready
/// handshake instead of treating the live owner as a duplicate.
fn acquire_singleton(restart_nonce: Option<&str>) -> anyhow::Result<Option<SingletonGuard>> {
    unsafe {
        let name = wide("WinGlanceSingleInstance");
        // CreateMutexW both creates (fresh) and opens (existing) the named
        // mutex. Opening can fail with ACCESS_DENIED when the name is held by
        // a process whose DACL denies us (a higher-integrity squatter):
        // annotate that instead of surfacing a bare OS error.
        let handle = CreateMutexW(None, true, PCWSTR(name.as_ptr())).map_err(|error| {
            anyhow::anyhow!(
                "creating/opening the single-instance mutex failed: {error} (another process may already hold the name with restrictive permissions)"
            )
        })?;
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return Ok(Some(SingletonGuard::new(handle)));
        }
        // The mutex already exists, so either a live instance owns it or the
        // previous instance died without releasing it (crash or kill), which
        // leaves the mutex abandoned.
        if let Some(nonce) = restart_nonce {
            // A handoff only ever carries a well-formed random nonce; any
            // other argument is not a handoff and falls through to the plain
            // conflict path below.
            if nonce_shape_ok(nonce) {
                // Restart handoff: the old process keeps ownership until
                // we signal ready — it releases the mutex and exits only after.
                // Signal ready first (so the old process stops waiting), then wait
                // (bounded) for the mutex it is about to release. If the event is
                // gone, the old process died before the handoff: fall through to
                // the plain conflict path, whose abandoned-mutex takeover covers
                // a crashed predecessor.
                let event_name = wide(&format!("WinGlanceRestartReady-{nonce}"));
                match OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(event_name.as_ptr())) {
                    Ok(ready) => {
                        // Only proceed when the signal landed: a failed SetEvent
                        // means the old process closed the event mid-handoff, so
                        // fall through to the plain conflict path, whose
                        // abandoned-mutex takeover covers a crashed predecessor.
                        if SetEvent(ready).is_ok() {
                            let _ = CloseHandle(ready);
                            return match WaitForSingleObject(handle, RESTART_READY_WAIT.as_millis() as u32) {
                                WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Some(SingletonGuard::new(handle))),
                                WAIT_TIMEOUT => {
                                    let _ = CloseHandle(handle);
                                    // The old process either kept the guard (its own
                                    // ready wait failed, so it is still the live
                                    // owner) or a concurrent launch acquired the
                                    // mutex while it was briefly unowned between the
                                    // old process's release and this wait. Both
                                    // outcomes leave the singleton with exactly one
                                    // owner and make this launch the duplicate; the
                                    // crash.log line makes a stolen handoff
                                    // diagnosable when the old process is already
                                    // gone. Logging is not initialized yet, so
                                    // crash.log it is.
                                    append_crash_log_line(
                                        b"restart handoff timed out waiting for the single-instance mutex; the old process either kept the singleton or a concurrent launch acquired it; this launch exits without starting\n",
                                    );
                                    Ok(None)
                                }
                                WAIT_FAILED => {
                                    let _ = CloseHandle(handle);
                                    Ok(None)
                                }
                                _ => {
                                    let _ = CloseHandle(handle);
                                    anyhow::bail!("unexpected wait result on the single-instance mutex")
                                }
                            };
                        }
                        let _ = CloseHandle(ready);
                    }
                    Err(_) => {
                        // The old process died before the handoff; fall through to
                        // the plain conflict path, whose abandoned-mutex takeover
                        // covers a crashed predecessor.
                    }
                }
            }
        }
        // A zero-timeout wait tells the cases apart: an abandoned mutex grants
        // ownership immediately, a live owner returns WAIT_TIMEOUT. Without
        // this, the first relaunch after a crash would exit, requiring a
        // second launch.
        match WaitForSingleObject(handle, 0) {
            WAIT_ABANDONED | WAIT_OBJECT_0 => Ok(Some(SingletonGuard::new(handle))),
            WAIT_TIMEOUT => {
                let _ = CloseHandle(handle);
                // A live (non-abandoned) owner: either a legitimate running
                // instance (a double-launch, which exits silently) or a
                // foreign process squatting the name (a same-session denial
                // of service). The probe tells the two apart well enough to
                // report the second — without it, a squatted name makes the
                // app fail to start with no diagnostic anywhere.
                if !winglance_instance_running_retried() {
                    report_suspected_squat();
                }
                Ok(None)
            }
            WAIT_FAILED => {
                let _ = CloseHandle(handle);
                anyhow::bail!(
                    "WaitForSingleObject failed on the single-instance mutex (the name may be held by a higher-integrity process)"
                );
            }
            _ => {
                let _ = CloseHandle(handle);
                anyhow::bail!("unexpected wait result on the single-instance mutex");
            }
        }
    }
}

/// Scans the process arguments for the restart-handoff nonce. Every other
/// argument (the `--reload-config` marker, the icon-worker probe) is left
/// alone; the handoff path only needs the value following the flag.
fn restart_nonce_arg(args: &[String]) -> Option<String> {
    args.iter()
        .position(|arg| arg == "--winglance-restart-nonce")
        .and_then(|index| args.get(index + 1).cloned())
}

/// Whether a nonce has the shape this app produces: exactly 32 hex digits.
/// Anything else is not a handoff: a foreign or corrupted argument must never
/// make the child open a named event it cannot own. (Kernel object names are
/// case-insensitive, so both cases of hex are accepted.)
fn nonce_shape_ok(nonce: &str) -> bool {
    nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A fresh 128-bit (32 hex digit) nonce for the restart handoff. The ready
/// event's name is derived from it, so a process that did not see this
/// command line cannot predict or race the event before the successor
/// signals it. Failure to obtain randomness aborts the restart and keeps the
/// current instance running: a restart with a guessable nonce would hand the
/// singleton to whoever signals the event first.
fn random_restart_nonce() -> Option<String> {
    let mut bytes = [0u8; 16];
    unsafe {
        let status = BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG);
        if !status.is_ok() {
            error!("restart: BCryptGenRandom failed: {status:?}");
            return None;
        }
    }
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Restarts the app in place so it reloads `config.toml` from disk. Unlike the
/// old release-then-spawn sequence, the single-instance mutex stays owned
/// until the successor signals it is ready, then the old process releases the
/// mutex and exits. Any spawn or ready failure leaves the guard — and this
/// instance — intact and running, so a failed handoff never leaves two
/// instances able to run or this one unprotected. The handoff nonce is a
/// fresh 128-bit random value, so the ready event's name cannot be predicted
/// — or signaled — before the successor itself signals it; and the guard is
/// released only while the successor is verifiably alive, so a stale signal
/// can never drop the singleton into an unowned gap. Nothing under
/// `%APPDATA%\WinGlance\WinGlance\data\` is deleted, so any on-disk cache
/// survives and the live log is preserved: the reloaded process appends to it
/// and marks the boundary instead of truncating it. Only in-memory caches
/// (icon/track/period) are lost, as they are on any restart.
pub fn relaunch_self() {
    // Take the guard out of the shared slot; every failure path puts it back.
    let Some(mut guard) = SINGLETON_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    else {
        error!("restart: the single-instance guard is not held; refusing to restart");
        return;
    };
    let Ok(exe) = env::current_exe() else {
        error!("restart: resolving the current executable path failed; keeping this instance running");
        *SINGLETON_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guard);
        return;
    };
    let Some(nonce) = random_restart_nonce() else {
        error!("restart: generating the handoff nonce failed; keeping this instance running");
        *SINGLETON_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guard);
        return;
    };
    // The ready event must exist before the child starts so the child can
    // open it by name; auto-reset (bManualReset = false) so the single wait
    // consumes the signal.
    let event_name = wide(&format!("WinGlanceRestartReady-{nonce}"));
    let ready = match unsafe { CreateEventW(None, false, false, PCWSTR(event_name.as_ptr())) } {
        Ok(event) => {
            // A fresh 128-bit nonce must yield a fresh event. An already-
            // existing name means the object was pre-created by something
            // that saw this command line: never wait on an event this
            // process did not create, or a forged signal could release the
            // singleton while the successor is not yet waiting.
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                error!("restart: the ready event already exists; keeping this instance running");
                unsafe {
                    let _ = CloseHandle(event);
                }
                *SINGLETON_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guard);
                return;
            }
            event
        }
        Err(error) => {
            error!("restart: creating the ready event failed: {error}; keeping this instance running");
            *SINGLETON_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guard);
            return;
        }
    };
    let spawned = process::Command::new(&exe)
        .arg("--reload-config")
        .arg("--winglance-restart-nonce")
        .arg(&nonce)
        .spawn();
    let Ok(mut child) = spawned else {
        error!("restart: launching the new process failed; keeping this instance running");
        unsafe {
            let _ = CloseHandle(ready);
        }
        *SINGLETON_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guard);
        return;
    };
    // Wait for the successor to signal ready. On success the mutex is
    // released and this process exits; on timeout the successor never came
    // up, so the guard stays and this instance keeps running as the owner.
    match unsafe { WaitForSingleObject(ready, RESTART_READY_TIMEOUT.as_millis() as u32) } {
        WAIT_OBJECT_0 | WAIT_ABANDONED => {}
        WAIT_TIMEOUT | WAIT_FAILED => {
            error!("restart: the new process did not signal ready in time; keeping this instance running");
            unsafe {
                let _ = CloseHandle(ready);
            }
            *SINGLETON_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guard);
            return;
        }
        _ => {
            error!("restart: unexpected wait result on the ready event; keeping this instance running");
            unsafe {
                let _ = CloseHandle(ready);
            }
            *SINGLETON_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guard);
            return;
        }
    }
    // Handoff accepted by the wait, but a signal alone does not prove the
    // handoff completed: a stale or forged ready signal must never drop the
    // guard into a gap nothing can take. Release only while the successor is
    // verifiably alive (`try_wait` is non-blocking here; a running child
    // reports `Ok(None)`).
    if let Ok(Some(_)) = child.try_wait() {
        error!("restart: the new process exited before the handoff completed; keeping this instance running");
        unsafe {
            let _ = CloseHandle(ready);
        }
        *SINGLETON_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guard);
        return;
    }
    // Handoff accepted: the successor is alive and signaled. Release the
    // mutex and exit; the child's bounded wait acquires ownership. The child
    // keeps running, so it must not be waited on.
    guard.release();
    unsafe {
        let _ = CloseHandle(ready);
    }
    drop(child);
    process::exit(0);
}

fn main() -> Result<()> {
    // Record the thread that owns the windows before anything can create one:
    // the UIA provider helpers use this to tell whether a call already runs on
    // the UI thread (direct state access) or must be handed off by message.
    main_window::mark_ui_thread();
    // The single-instance guard must come before any side effects: logging
    // truncates the live log and config recovery touches the user's file, so a
    // duplicate launch must not get that far. A restart-handoff child carries
    // the nonce it was spawned with; every other launch does not.
    let restart_nonce = restart_nonce_arg(
        &env::args_os()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    );
    match acquire_singleton(restart_nonce.as_deref()) {
        Ok(Some(guard)) => {
            // Mirror the guard into the shared slot so the Settings/restart
            // path can hand the mutex to a successor. The slot is the sole
            // owner from here on; `relaunch_self` takes the guard out of it
            // for a handoff and puts it back if the handoff fails.
            *SINGLETON_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guard);
        }
        Ok(None) => {
            // Another instance holds the mutex; exit without touching its
            // log or config.
            return Ok(());
        }
        Err(error) => {
            // Fail closed: running without the singleton would let a second
            // instance truncate the live log or rewrite config while the
            // first is running. Logging is not initialized yet, so record the
            // failure in crash.log and exit. Verified append (parent
            // identity checked, no reparse follow).
            if let Some(dir) = config::Config::data_dir().ok().map(|d| d.join("logs")) {
                let _ = crate::winutil::append_verified(
                    &dir.join("crash.log"),
                    format!("could not acquire the single-instance mutex: {error:#}\n").as_bytes(),
                );
            }
            return Err(error);
        }
    };

    // Logging initializes before the config loads: a corrupted config.toml now
    // falls back to defaults, and that fallback must be diagnosable through
    // the log file. The reload marker must be scanned first, because on that
    // path the live log is preserved (appended to) instead of truncated.
    let reload_config = std::env::args_os().any(|arg| arg == "--reload-config");
    logging::init_logging(&config::Config::default().logs_dir(), reload_config);
    let config = config::Config::load()?;
    config.log_settings();
    install_crash_handler(&config.logs_dir());
    install_panic_hook(&config.logs_dir());

    info!("starting WinGlance");

    // Distinguish a user-requested reload (Settings "Restart app" button)
    // from a plain launch, tray start or autostart: the old process exits
    // right after spawning the new one, so the marker is passed as an
    // argument and recorded here in the new process's preserved log.
    if reload_config {
        info!("started via the Settings 'Restart app' action; applying the on-disk config");
    }

    if let Err(error) = autostart::apply(config.behavior.start_on_login) {
        warn!("start-on-login sync failed: {error:#}");
    }

    unsafe {
        if let Err(error) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            warn!("per-monitor DPI awareness unavailable: {error}");
        }
    }

    let (event_tx, event_rx) = mpsc::sync_channel::<Arc<MediaEvent>>(EVENT_CHANNEL_CAP);
    // Shared in-flight artwork byte counter for the event path (see
    // `smtc::MAX_IN_FLIGHT_ARTWORK_BYTES`): the SMTC worker adds the artwork
    // bytes of every event it queues and drops the payload when the budget is
    // exceeded; the forwarder frees the bytes as it pops, so the count tracks
    // the distinct artwork allocations held by the outbound queues. Shared
    // across worker restarts so events a replaced worker left queued stay
    // accounted.
    let in_flight_art: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let supervisor_art = in_flight_art.clone();
    let forwarder_art = in_flight_art.clone();
    // One-shot latch for the budget-drop tray warning: the worker sets it the
    // first time the in-flight artwork budget strips a cover payload, so the
    // user gets exactly one "the UI is not keeping up" note per app run, not
    // one per dropped cover. Shared across worker restarts like the counter.
    let budget_warned: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let supervisor_budget_warned = budget_warned.clone();
    // Dedicated one-shot status channel for the supervisor's permanent-failure
    // report, deliberately *not* the bounded event channel: under the very
    // overload that motivates the event cap, try_send into it could drop the
    // "notifications stopped" signal. Unbounded is safe here because the
    // supervisor sends at most one WorkerFailed ever (it gives up right
    // after), so the channel cannot grow.
    let (supervisor_tx, supervisor_rx) = mpsc::channel::<Arc<MediaEvent>>();
    let shared_config: std::sync::Arc<std::sync::RwLock<Config>> =
        std::sync::Arc::new(std::sync::RwLock::new(config.clone()));
    let listener_config = shared_config.clone();
    let heartbeat: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));
    let supervisor_heartbeat = heartbeat.clone();
    // Set once the message loop returns; the supervisor and the event
    // forwarder exit promptly so main can join them before destroying the
    // windows (a forwarder still posting after teardown would risk reaching a
    // reused HWND).
    let shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let supervisor_shutdown = shutdown.clone();
    let forwarder_shutdown = shutdown.clone();
    // Worker generation counter: each spawned worker gets the next value, so
    // a worker that stalled and was replaced stops emitting events and stops
    // updating the shared heartbeat the moment its successor takes over.
    let generation: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let supervisor_generation = generation.clone();
    // The one source of truth for "which source's pill the overlay is
    // showing": the overlay publishes to this cell on every content display,
    // and every SMTC worker reads it for the session-recreation suppression
    // gate. A worker must not attribute from its own emissions — an emitted
    // event can be queued or superseded on the overlay side — and the cell
    // survives worker restarts, so a session recreated after a restart still
    // compares against what the user actually sees.
    let now_showing: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let now_showing_supervisor = now_showing.clone();
    // Supervisor: runs the SMTC worker and restarts it when it stalls (a WinRT
    // call can hang under heavy session churn, which would otherwise silently
    // stop all events and pills). The hung worker thread is leaked; a fresh
    // worker with its own manager takes over. Threads get explicit smaller
    // stacks (Rust defaults to 2 MB reserve each) — the supervisor and the
    // event forwarder only sleep and forward, and the worker's WinRT calls
    // stay well under 1 MB.
    let supervisor_handle = thread::Builder::new()
        .name("WinGlance-smtc".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            // The supervisor reports a permanent worker failure to the main
            // window (one WorkerFailed event, on the dedicated unbounded
            // status channel) so the user sees it in the history and as a
            // tray note instead of the app silently stopping notifications.
            // Consecutive worker failures (stall or exit) back off: a
            // permanently broken SMTC worker must not restart (and re-log)
            // every few seconds forever, and each stalled restart leaks the
            // hung thread plus its COM registrations, so the leak rate needs
            // a bound. A worker that runs for two minutes resets the counter.
            let mut consecutive_restarts: u32 = 0;
            loop {
                if supervisor_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                if worker_budget_exhausted(consecutive_restarts) {
                    let reason = format!(
                        "SMTC worker failed {MAX_WORKER_RESTARTS} times in a row; restart WinGlance to restore media notifications"
                    );
                    error!("{reason}");
                    // Unbounded send: cannot be dropped by an overloaded
                    // event channel. Only fails if the app is already
                    // tearing down and the forwarder receiver is gone.
                    let _ = supervisor_tx.send(Arc::new(MediaEvent::WorkerFailed { reason }));
                    break;
                }
                let worker_heartbeat = supervisor_heartbeat.clone();
                let worker_generation = supervisor_generation.clone();
                let worker_shutdown = supervisor_shutdown.clone();
                let my_generation = supervisor_generation.fetch_add(1, Ordering::SeqCst) + 1;
                let event_tx_worker = event_tx.clone();
                let art_worker = supervisor_art.clone();
                let budget_warned_worker = supervisor_budget_warned.clone();
                let listener_config_worker = listener_config.clone();
                let now_showing_worker = now_showing_supervisor.clone();
                let worker_started = Instant::now();
                // A replacement worker starts with a fresh heartbeat:
                // the supervisor may still be reading the previous worker's
                // stale heartbeat at the first 1 s check, and the successor
                // must get the full stall window to reach its event loop.
                *supervisor_heartbeat
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
                // One-shot completion channel: the worker signals it
                // once `run` has fully returned — COM cleanup included — so a
                // shutdown can join a responsive worker within a bound instead
                // of detaching it blindly.
                let (done_tx, done_rx) = mpsc::channel::<()>();
                let worker = thread::Builder::new()
                    .name("WinGlance-smtc-worker".to_string())
                    .stack_size(1024 * 1024)
                    .spawn(move || {
                        let _ = smtc::SmtcListener::new(
                            event_tx_worker,
                            art_worker,
                            budget_warned_worker,
                            listener_config_worker,
                            worker_heartbeat,
                            worker_generation,
                            my_generation,
                            worker_shutdown,
                            now_showing_worker,
                        )
                        .run();
                        let _ = done_tx.send(());
                    });
                let Ok(worker) = worker else {
                    // A failed spawn consumes the same restart budget as a
                    // stall or an exit, with the same backoff: a persistently
                    // un-creatable worker reaches the cap and emits the
                    // one-shot WorkerFailed instead of retrying forever.
                    consecutive_restarts += 1;
                    let delay = worker_restart_delay(consecutive_restarts);
                    warn!("could not start the SMTC worker; retrying in {}s", delay.as_secs());
                    sleep_interruptible(delay, &supervisor_shutdown);
                    continue;
                };
                let exit = loop {
                    if supervisor_shutdown.load(Ordering::SeqCst) {
                        // Bounded shutdown join: a responsive worker
                        // finishes — COM cleanup included — within the grace
                        // and is joined; a stuck one is detached only after
                        // the grace expires.
                        match done_rx.recv_timeout(WORKER_SHUTDOWN_GRACE) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break WorkerExit::Joined,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                warn!(
                                    "SMTC worker did not stop within {}s; detaching it",
                                    WORKER_SHUTDOWN_GRACE.as_secs()
                                );
                                break WorkerExit::Detached;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1000));
                    if worker_started.elapsed() > Duration::from_secs(120) {
                        consecutive_restarts = 0;
                    }
                    let last = *supervisor_heartbeat
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if worker_is_stalled(worker_started.elapsed(), last.elapsed()) {
                        break WorkerExit::Stalled;
                    }
                    if worker.is_finished() {
                        break WorkerExit::Completed;
                    }
                };
                match exit {
                    WorkerExit::Stalled => {
                        // Do not join: the worker may be blocked inside COM forever.
                        consecutive_restarts += 1;
                        // A hard-stalled worker is leaked mid-call, so its `Drop`
                        // never runs and the mailbox-clear latch reset (see
                        // `clear_pending_output`) cannot fire: an undelivered
                        // budget warning sitting in the leaked mailbox would keep
                        // `budget_warned` set and lose the note for the rest of
                        // the app run — exactly the wedged-UI overload it exists
                        // to report. Reset the latch here. A budget strip is
                        // almost always caused by a full event channel (the
                        // in-flight byte counter only trips while the forwarder
                        // cannot drain), which is also the precondition for the
                        // warning to sit in the mailbox rather than the channel;
                        // the accepted residual is the rare delivered-then-stall
                        // ordering, where a later, separate wedged episode may
                        // re-warn once more — a duplicate tray note beats a
                        // permanently silent one.
                        if reset_budget_warning_on_stall(&supervisor_budget_warned) {
                            warn!("budget-warning latch reset | reason=stalled-worker-leak");
                        }
                        let delay = worker_restart_delay(consecutive_restarts);
                        error!("SMTC worker stalled; restarting it in {}s", delay.as_secs());
                        sleep_interruptible(delay, &supervisor_shutdown);
                        continue;
                    }
                    WorkerExit::Completed => {
                        match worker.join() {
                            Ok(()) => {}
                            Err(panic) => {
                                let payload = panic.downcast_ref::<&str>().copied().unwrap_or("unknown panic");
                                error!("SMTC worker panicked: {payload}");
                            }
                        }
                        // The worker exited on its own (an error or a panic): restart it.
                        consecutive_restarts += 1;
                        let delay = worker_restart_delay(consecutive_restarts);
                        warn!("SMTC worker exited; restarting it in {}s", delay.as_secs());
                        sleep_interruptible(delay, &supervisor_shutdown);
                    }
                    // Shutdown paths: the worker finished inside the grace
                    // (reaped here) or is a stuck detach left for the OS. In
                    // both cases the supervisor exits so main can join it and
                    // destroy the windows.
                    WorkerExit::Joined => {
                        let _ = worker.join();
                        break;
                    }
                    WorkerExit::Detached => break,
                }
            }
        })?;

    let main_queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
    let overlay_queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
    let main_wake: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let overlay_wake: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    // Sample the system accessibility preferences before any window exists:
    // the overlay's very first frame must already honor animation,
    // overlapped-content and high-contrast settings. Later changes
    // arrive through WM_SETTINGCHANGE.
    let prefs = winutil::refresh_system_preferences();
    debug!("sampled system preferences at startup: {prefs:?}");
    let overlay_hwnd = overlay::create_window(
        config.clone(),
        overlay_queue.clone(),
        overlay_wake.clone(),
        now_showing.clone(),
    )?;
    let main_hwnd = match main_window::create_window(
        shared_config.clone(),
        main_queue.clone(),
        overlay_hwnd,
        main_wake.clone(),
    ) {
        Ok(hwnd) => hwnd,
        Err(error) => {
            // The overlay was already created and armed (foreground hook,
            // animation timer, name cell, state box). Teardown must not
            // depend on process exit: destroy it now so its WM_NCDESTROY
            // runs the full teardown — hook unhook, timer delete, name-cell
            // null, box free — before the error propagates. No forwarder
            // exists yet, so nothing can post to the overlay after this
            // point.
            unsafe {
                let _ = DestroyWindow(overlay_hwnd);
            }
            return Err(error);
        }
    };

    let forwarder_handle = spawn_event_forwarder(
        main_hwnd,
        overlay_hwnd,
        main_queue,
        overlay_queue,
        main_wake,
        overlay_wake,
        event_rx,
        supervisor_rx,
        forwarder_shutdown,
        forwarder_art,
    );

    let message_result = message_loop();
    debug!("message loop exited; shutting down");

    // Stop the producers before destroying the windows: the forwarder must
    // not post to an HWND that teardown is about to free. The supervisor
    // exits within ~1s of the flag; a stalled worker is left for process
    // exit (it may be blocked inside COM and cannot be joined).
    shutdown.store(true, Ordering::SeqCst);
    let _ = forwarder_handle.join();
    let _ = supervisor_handle.join();

    unsafe {
        let _ = DestroyWindow(overlay_hwnd);
        let _ = DestroyWindow(main_hwnd);
    }
    message_result
}

/// Upper bound on consecutive SMTC worker restarts without a 2-minute
/// healthy run in between. Beyond it the supervisor gives up, so a broken
/// SMTC stack cannot leak one hung thread (plus its COM registrations) every
/// 90 seconds forever.
const MAX_WORKER_RESTARTS: u32 = 5;

/// Cap of the SMTC worker → forwarder event channel. The forwarder drains it
/// every 200ms, so in practice it never fills; the cap only matters when the
/// forwarder is wedged, and then it sheds new events at the source (with a
/// log) instead of growing without bound. Kept larger than the window-queue
/// cap so the window queues, not this channel, do the realistic overload
/// shedding with newest-wins semantics.
const EVENT_CHANNEL_CAP: usize = 1024;

/// Cap of each window's pending event queue (main window and overlay). The
/// forwarder pushes into these on every SMTC event; a window that is slow to
/// drain (long paint, message loop stall) must not accumulate events forever.
/// When the cap is exceeded the oldest buffered event is dropped in favor of
/// the newest, so the window eventually shows the latest media state.
const EVENT_QUEUE_CAP: usize = 256;

/// Sleeps in 200ms slices, returning early once `shutdown` is set, so the
/// supervisor can be joined promptly on exit.
fn sleep_interruptible(duration: Duration, shutdown: &AtomicBool) {
    let mut remaining = duration;
    while remaining > Duration::ZERO && !shutdown.load(Ordering::SeqCst) {
        let step = remaining.min(Duration::from_millis(200));
        std::thread::sleep(step);
        remaining -= step;
    }
}

/// Restart delay after `consecutive` SMTC worker failures: quick retries at
/// first, then a slow cadence so a permanently broken worker does not restart
/// (and leak a thread plus its COM registrations) every few seconds forever.
fn worker_restart_delay(consecutive: u32) -> Duration {
    if consecutive >= 3 {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(5)
    }
}

/// Whether the supervisor must give up permanently. Every failure kind —
/// spawn, exit, and stall — increments the same `consecutive_restarts`
/// counter, so the budget is exhausted at the same point no matter how the
/// worker failed; the loop-top check then emits the one-shot WorkerFailed
/// and stops restarting.
fn worker_budget_exhausted(consecutive_restarts: u32) -> bool {
    consecutive_restarts >= MAX_WORKER_RESTARTS
}

/// The supervisor's stall-branch latch reset: a hard-stalled worker is
/// leaked mid-call, so its `Drop` (and the mailbox-clear latch reset in
/// `clear_pending_output`) never runs — an undelivered budget warning in the
/// leaked mailbox would keep `budget_warned` set and lose the note for the
/// rest of the app run. Returns whether the latch was set (a stall with no
/// outstanding warning stays silent) and clears it, so the next budget strip
/// can warn again. Extracted as a pure helper so the swap is testable
/// without a genuinely stalled COM worker.
fn reset_budget_warning_on_stall(latch: &AtomicBool) -> bool {
    latch.swap(false, Ordering::Relaxed)
}

/// Stall threshold for the SMTC worker: both the time since spawn and the
/// time since the last heartbeat must exceed this before the supervisor
/// declares a stall and restarts the worker.
const WORKER_STALL_THRESHOLD: Duration = Duration::from_secs(30);

/// How long shutdown waits for a responsive worker to finish (COM cleanup
/// included) before detaching it as demonstrably stuck.
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Whether the SMTC worker is stalled. Both the spawn age and the heartbeat
/// age must exceed the threshold: a freshly spawned replacement
/// always gets the full startup window even while the supervisor is still
/// reading the previous worker's stale heartbeat, and a worker that ran past
/// its spawn age but keeps beating is alive.
fn worker_is_stalled(worker_age: Duration, heartbeat_age: Duration) -> bool {
    worker_age > WORKER_STALL_THRESHOLD && heartbeat_age > WORKER_STALL_THRESHOLD
}

/// How a worker ended, as seen by the supervisor watchdog.
enum WorkerExit {
    /// Restart: the worker stopped beating long enough (see `worker_is_stalled`).
    Stalled,
    /// Restart: the worker ended on its own and is joinable.
    Completed,
    /// Shutdown: the worker finished inside the grace and was joined.
    Joined,
    /// Shutdown: the worker stayed stuck past the grace and was detached.
    Detached,
}

/// Whether an event must be delivered to the overlay queue. Worker failures
/// and the budget-drop tray warning are history/tray-only: the overlay never
/// renders them, so they must not wake the pill or occupy its queue. Every
/// other event — including `SessionRejected`, which drives the overlay's
/// retire logic for sources that leave the allow-list — is forwarded even
/// though rejections are never rendered as pills.
fn overlay_bound(event: &MediaEvent) -> bool {
    !matches!(
        event,
        MediaEvent::WorkerFailed { .. } | MediaEvent::ArtworkBudgetExceeded
    )
}

/// Drains SMTC events into each window's queue and wakes both windows.
/// Returns the thread handle so main can join it before destroying the
/// windows. The loop exits within ~200ms of `shutdown` being set. At most one
/// wake message per window is in flight at a time: the `wake` flags make an
/// event burst collapse into a single `MEDIA_EVENT_MSG` per drain.
#[allow(clippy::too_many_arguments)]
fn spawn_event_forwarder(
    main_hwnd: HWND,
    overlay_hwnd: HWND,
    main_queue: EventQueue,
    overlay_queue: EventQueue,
    main_wake: Arc<AtomicBool>,
    overlay_wake: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Arc<MediaEvent>>,
    supervisor_rx: mpsc::Receiver<Arc<MediaEvent>>,
    shutdown: Arc<AtomicBool>,
    in_flight_art: Arc<AtomicU64>,
) -> std::thread::JoinHandle<()> {
    // HWND is not Send; the raw handle value is all the forwarder needs to
    // post with.
    let main_raw = main_hwnd.0 as isize;
    let overlay_raw = overlay_hwnd.0 as isize;
    thread::Builder::new()
        .name("WinGlance-events".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                // One-shot status events from the supervisor (at most one
                // WorkerFailed per session, then it gives up). History-only:
                // never wake the pill or occupy its queue.
                while let Ok(event) = supervisor_rx.try_recv() {
                    push_and_wake(
                        &main_queue,
                        &main_wake,
                        event,
                        HWND(main_raw as *mut c_void),
                        "main window",
                    );
                }
                let event = match receiver.recv_timeout(Duration::from_millis(200)) {
                    Ok(event) => {
                        // The event left the worker's outbound queue: its
                        // artwork bytes are no longer in flight here. The
                        // window queues share the same `Arc` allocations and
                        // are separately count-capped, so freeing at the pop
                        // keeps the counter at the distinct live allocations.
                        in_flight_art.fetch_sub(artwork_bytes(&event), Ordering::Relaxed);
                        event
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                // One allocation per event, shared by both window queues via
                // Arc clones; the windows recover the owned event on drain.
                push_and_wake(
                    &main_queue,
                    &main_wake,
                    event.clone(),
                    HWND(main_raw as *mut c_void),
                    "main window",
                );
                // Worker failures are history-only: the overlay never renders
                // them, so they must not wake the pill or occupy its queue.
                // Rejected sessions do reach the overlay: its retire logic
                // drops a retired source's content from the pill, even though
                // the rejection itself is never rendered.
                if overlay_bound(&event) {
                    push_and_wake(
                        &overlay_queue,
                        &overlay_wake,
                        event,
                        HWND(overlay_raw as *mut c_void),
                        "overlay",
                    );
                }
            }
        })
        .expect("event forwarder thread should start")
}

/// Applies the window-queue cap after a push: when the queue holds more than
/// `EVENT_QUEUE_CAP` events, the oldest is dropped in favor of the newest.
fn enforce_queue_cap(queue: &mut VecDeque<Arc<MediaEvent>>, name: &str) {
    if queue.len() > EVENT_QUEUE_CAP {
        warn!("the {name} event queue exceeded its cap of {EVENT_QUEUE_CAP}; dropping the oldest buffered event");
        queue.pop_front();
    }
}

/// Clears a window's pending-event queue and its wake flag, logging how many
/// events were dropped. Used when the window cannot be poked (a failed
/// post): leaving the queue populated without a wake message in flight
/// would strand those events until some unrelated future event reposts.
fn clear_and_account(queue: &EventQueue, wake: &AtomicBool, name: &str) {
    wake.store(false, Ordering::SeqCst);
    let dropped = queue
        .lock()
        .map(|mut q| {
            let count = q.len();
            q.clear();
            count
        })
        .unwrap_or(0);
    if dropped > 0 {
        warn!("dropped {dropped} queued events for the {name} after a failed wake-up post");
    }
}

/// Pushes one event into a window's queue and posts `MEDIA_EVENT_MSG` only
/// when no wake message is already in flight (`wake` was clear). On a failed
/// post the whole pending queue is dropped and the flag cleared: the window
/// cannot be poked, so nothing may stay queued without a wake message or a
/// retry. On a poisoned queue the event is dropped and the wake flag is left
/// untouched.
fn push_and_wake(queue: &EventQueue, wake: &AtomicBool, event: Arc<MediaEvent>, hwnd: HWND, name: &str) {
    // A poisoned queue is unusable, so the event cannot be delivered. Do not
    // arm the wake flag: the window would drain nothing and the flag would
    // stay set until the next successful push.
    let Ok(mut q) = queue.lock() else {
        warn!("the {name} event queue is poisoned; dropping the event");
        return;
    };
    q.push_back(event);
    enforce_queue_cap(&mut q, name);
    if !wake.swap(true, Ordering::SeqCst)
        && unsafe { post_message(hwnd, MEDIA_EVENT_MSG, WPARAM(0), LPARAM(0)) }.is_err()
    {
        drop(q);
        clear_and_account(queue, wake, name);
    }
}

/// Re-arms the wake flag and posts `MEDIA_EVENT_MSG` when events arrived
/// during a drain. On a failed post the pending events are dropped and
/// accounted for (see `clear_and_account`), so a window can never hold
/// events without a wake message in flight or a retry pending.
pub(crate) fn repost_if_pending(queue: &EventQueue, wake: &AtomicBool, hwnd: HWND, name: &str) {
    let more = queue.lock().map(|q| !q.is_empty()).unwrap_or(false);
    if more
        && !wake.swap(true, Ordering::SeqCst)
        && unsafe { post_message(hwnd, MEDIA_EVENT_MSG, WPARAM(0), LPARAM(0)) }.is_err()
    {
        clear_and_account(queue, wake, name);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{PlaybackState, media_event_into_owned};

    #[test]
    fn worker_restart_budget_is_shared_by_every_failure_kind() {
        // Spawn, exit, and stall failures all increment the one supervisor
        // counter, so the same cap decision applies to each: below the cap
        // the supervisor keeps retrying with the shared backoff, and at the
        // cap the loop emits WorkerFailed and stops.
        for count in 0..MAX_WORKER_RESTARTS {
            assert!(!worker_budget_exhausted(count), "failure {count} must still retry");
        }
        assert!(worker_budget_exhausted(MAX_WORKER_RESTARTS), "the cap is terminal");
        assert!(worker_budget_exhausted(MAX_WORKER_RESTARTS + 1));
        // The backoff is monotone: accumulating failures stretch the retry
        // delay up to the slow 60 s plateau.
        assert!(worker_restart_delay(1) < worker_restart_delay(3));
        assert_eq!(worker_restart_delay(3), worker_restart_delay(4));
    }

    #[test]
    fn stall_reset_reports_and_clears_a_set_budget_warning_latch() {
        // A hard-stalled worker may leave an undelivered budget warning in
        // its leaked mailbox; the supervisor's stall reset must report it
        // (so the WARN logs) and clear it (so the next strip can warn again).
        // A stall with no outstanding warning stays silent.
        let latch = AtomicBool::new(true);
        assert!(
            reset_budget_warning_on_stall(&latch),
            "a set latch is reported so the stall logs the reset"
        );
        assert!(
            !latch.load(Ordering::Relaxed),
            "and cleared, so the note can fire again after the stall"
        );
        assert!(
            !reset_budget_warning_on_stall(&latch),
            "a clear latch is silent — no warning was stranded"
        );
        assert!(!latch.load(Ordering::Relaxed));
    }

    #[test]
    fn worker_is_stalled_requires_both_ages_past_the_threshold() {
        // The stall verdict needs BOTH the spawn age and the heartbeat age
        // past the threshold: a replacement worker must always get
        // its full startup window even if the supervisor is still reading
        // the previous worker's stale heartbeat.
        let young = Duration::from_secs(1);
        let old = WORKER_STALL_THRESHOLD + Duration::from_secs(1);
        assert!(!worker_is_stalled(young, young), "a fresh worker is never stalled");
        assert!(
            !worker_is_stalled(old, young),
            "a fresh heartbeat keeps a freshly spawned worker alive"
        );
        assert!(
            !worker_is_stalled(young, old),
            "a worker past its spawn age that keeps beating is alive"
        );
        assert!(worker_is_stalled(old, old), "only a true 31 s stall is restarted");
    }

    #[test]
    fn restart_nonce_is_scanned_from_the_arguments() {
        // Plain launches carry no nonce; the handoff flag must pair with a
        // value, and the first occurrence wins.
        assert_eq!(restart_nonce_arg(&[]), None);
        assert_eq!(restart_nonce_arg(&["--reload-config".to_string()]), None);
        assert_eq!(
            restart_nonce_arg(&["--winglance-restart-nonce".to_string()]),
            None,
            "a flag without a value is not a handoff"
        );
        assert_eq!(
            restart_nonce_arg(&[
                "--reload-config".to_string(),
                "--winglance-restart-nonce".to_string(),
                "42-1".to_string(),
            ]),
            Some("42-1".to_string())
        );
        assert_eq!(
            restart_nonce_arg(&[
                "--winglance-restart-nonce".to_string(),
                "a-1".to_string(),
                "--winglance-restart-nonce".to_string(),
                "b-2".to_string(),
            ]),
            Some("a-1".to_string()),
            "the first value wins"
        );
    }

    #[test]
    fn handoff_nonce_is_random_hex() {
        let first = random_restart_nonce().expect("the RNG must succeed");
        let second = random_restart_nonce().expect("the RNG must succeed");
        assert_eq!(first.len(), 32, "a 128-bit nonce renders as 32 hex digits");
        assert!(nonce_shape_ok(&first));
        assert_ne!(first, second, "two handoffs must never share a nonce");
    }

    #[test]
    fn nonce_shape_rejects_every_non_handoff_argument() {
        assert!(nonce_shape_ok("0123456789abcdef0123456789abcdef"));
        // Kernel object names are case-insensitive, so upper-case hex is
        // accepted; only hex, and only exactly 32 digits.
        assert!(nonce_shape_ok("0123456789ABCDEF0123456789ABCDEF"));
        for bad in [
            "",
            "42-1",
            "a-1",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789abcdef0123456789abcdef ",
            "0123456789abcdef0123456789abcdeX",
        ] {
            assert!(!nonce_shape_ok(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn singleton_guard_release_frees_a_held_mutex() {
        // A real mutex object backs the guard. While the guard holds
        // it, a second handle opened in the same test sees the mutex owned;
        // after `release` the same handle acquires it immediately, and a
        // following `Drop` must be a no-op, so the handle is never released
        // or closed twice. The owner probe runs on another thread because the
        // owning thread could recursively acquire its own mutex and mask the
        // ownership state.
        let name = format!("WinGlanceTestSingleton-{}", process::id());
        unsafe {
            let name_wide = wide(&name);
            let first = CreateMutexW(None, true, PCWSTR(name_wide.as_ptr())).unwrap();
            let mut guard = SingletonGuard::new(first);
            let second = CreateMutexW(None, true, PCWSTR(name_wide.as_ptr())).unwrap();
            // `HANDLE` is not `Send`; probe through its numeric value.
            let second_raw = second.0 as usize;
            let wait_on_other_thread = || {
                let probe = move || WaitForSingleObject(HANDLE(second_raw as *mut c_void), 0);
                thread::spawn(probe).join().unwrap()
            };
            assert_eq!(
                wait_on_other_thread(),
                WAIT_TIMEOUT,
                "an unreleased guard owns the mutex"
            );
            guard.release();
            assert_eq!(
                wait_on_other_thread(),
                WAIT_OBJECT_0,
                "release hands the mutex back, so a later Drop is a no-op"
            );
            let _ = CloseHandle(second);
        }
    }

    #[test]
    fn window_queue_is_bounded_with_newest_wins() {
        // Push far more events than the cap: the queue must stay capped and
        // the newest event must survive, with the oldest evicted first.
        let mut queue = VecDeque::new();
        for i in 0..(EVENT_QUEUE_CAP + 50) {
            queue.push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Paused,
                format!("src-{i}"),
            )));
            enforce_queue_cap(&mut queue, "test");
        }
        assert_eq!(queue.len(), EVENT_QUEUE_CAP);
        match queue.front().map(|e| e.as_ref()) {
            Some(MediaEvent::PlaybackStateChanged(_, source)) => {
                assert_eq!(source, "src-50", "the oldest surviving event must be the 51st pushed");
            }
            _ => panic!("expected a PlaybackStateChanged at the front"),
        }
        match queue.back().map(|e| e.as_ref()) {
            Some(MediaEvent::PlaybackStateChanged(_, source)) => {
                assert_eq!(
                    source,
                    &format!("src-{}", EVENT_QUEUE_CAP + 49),
                    "the newest event must be kept"
                );
            }
            _ => panic!("expected a PlaybackStateChanged at the back"),
        }
    }

    #[test]
    fn window_queue_under_cap_keeps_every_event() {
        let mut queue = VecDeque::new();
        for i in 0..EVENT_QUEUE_CAP {
            queue.push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Playing,
                format!("src-{i}"),
            )));
            enforce_queue_cap(&mut queue, "test");
        }
        assert_eq!(queue.len(), EVENT_QUEUE_CAP);
        match queue.front().map(|e| e.as_ref()) {
            Some(MediaEvent::PlaybackStateChanged(_, source)) => assert_eq!(source, "src-0"),
            _ => panic!("expected the first event at the front"),
        }
    }

    #[test]
    fn post_failure_drops_the_pending_queue_and_clears_wake() {
        // PostMessageW to an invalid (non-null) window handle always fails,
        // injecting the failure the transport must survive: after each failed
        // post nothing may remain queued without a wake message in flight.
        // (A null handle would not do: PostMessageW treats it as "post to
        // the calling thread" and succeeds; so does the -1 sentinel, which
        // is why the bogus handle is isize::MAX, not usize::MAX.)
        let bogus_hwnd = HWND(isize::MAX as *mut c_void);
        let queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
        let wake = Arc::new(AtomicBool::new(false));
        push_and_wake(
            &queue,
            &wake,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Playing, "src-1".into())),
            bogus_hwnd,
            "test",
        );
        push_and_wake(
            &queue,
            &wake,
            Arc::new(MediaEvent::PlaybackStateChanged(PlaybackState::Paused, "src-2".into())),
            bogus_hwnd,
            "test",
        );
        assert!(
            queue.lock().unwrap().is_empty(),
            "a failed post must not leave events stranded"
        );
        assert!(
            !wake.load(Ordering::SeqCst),
            "a failed post must leave the wake flag clear"
        );
    }

    #[test]
    fn repost_failure_clears_events_that_arrived_during_a_drain() {
        // The drain-side repost path: events that arrived while the window
        // was draining must not stay queued when the wake-up post fails.
        let queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
        let wake = Arc::new(AtomicBool::new(false));
        {
            let mut q = queue.lock().unwrap();
            for i in 0..3 {
                q.push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                    PlaybackState::Playing,
                    format!("src-{i}"),
                )));
            }
        }
        repost_if_pending(&queue, &wake, HWND(isize::MAX as *mut c_void), "test");
        assert!(
            queue.lock().unwrap().is_empty(),
            "a failed repost must not strand pending events"
        );
        assert!(
            !wake.load(Ordering::SeqCst),
            "a failed repost must leave the wake flag clear"
        );
    }

    #[test]
    fn media_event_is_recovered_from_its_transport_arc() {
        // A sole reference unwraps zero-copy...
        let sole = Arc::new(MediaEvent::PlaybackStateChanged(
            PlaybackState::Playing,
            "spotify".into(),
        ));
        let owned = media_event_into_owned(sole);
        assert!(matches!(owned, MediaEvent::PlaybackStateChanged(PlaybackState::Playing, ref s) if s == "spotify"));
        // ...while a shared reference falls back to a clone of the same value.
        let shared = Arc::new(MediaEvent::PlaybackStateChanged(
            PlaybackState::Paused,
            "youtube-music".into(),
        ));
        let other_holder = shared.clone();
        let owned = media_event_into_owned(shared);
        assert!(
            matches!(owned, MediaEvent::PlaybackStateChanged(PlaybackState::Paused, ref s) if s == "youtube-music")
        );
        assert_eq!(Arc::strong_count(&other_holder), 1);
    }

    #[test]
    fn overlay_bound_forwards_rejections_but_not_worker_failures() {
        // SessionRejected drives the overlay's retire logic (content from a
        // source that left the allow-list must leave the pill), so it must
        // reach the overlay even though it is never rendered. WorkerFailed is
        // history-only and must not wake the pill or occupy its queue, and
        // the one-shot budget-drop warning is tray-only.
        let rejected = MediaEvent::SessionRejected {
            source_app: "Brave".into(),
            title: "t".into(),
            artist: "a".into(),
            state: PlaybackState::Paused,
            accepted: false,
        };
        assert!(overlay_bound(&rejected), "SessionRejected must reach the overlay");
        let failed = MediaEvent::WorkerFailed {
            reason: "worker died".into(),
        };
        assert!(!overlay_bound(&failed), "WorkerFailed is history-only");
        assert!(
            !overlay_bound(&MediaEvent::ArtworkBudgetExceeded),
            "the budget warning is tray-only and must not wake the pill"
        );
        let changed = MediaEvent::PlaybackStateChanged(PlaybackState::Playing, "Brave".into());
        assert!(overlay_bound(&changed), "playback events must reach the overlay");
    }

    #[test]
    fn is_our_instance_matches_only_another_win_glance_exe() {
        // Toolhelp reports the executable name as a fixed 260-unit NUL-padded
        // buffer; the classification must match the product name
        // case-insensitively, ignore padding, and never count this process
        // itself as "another instance".
        let padded = |s: &str| {
            let mut units: Vec<u16> = s.encode_utf16().collect();
            units.resize(260, 0);
            units
        };
        // Another pid running WinGlance.exe (any casing, padded) is an
        // instance.
        assert!(is_our_instance(&padded("WinGlance.EXE"), 42, 1));
        assert!(is_our_instance(&padded("winglance.exe"), 42, 1));
        // Our own pid never counts, even with the right name: the probe runs
        // in the duplicate process, which is itself a WinGlance.exe.
        assert!(!is_our_instance(&padded("WinGlance.exe"), 42, 42));
        // Foreign names, a missing extension, a trailing space, and empty
        // buffers are rejected.
        assert!(!is_our_instance(&padded("winthing.exe"), 42, 1));
        assert!(!is_our_instance(&padded("WinGlance"), 42, 1));
        assert!(!is_our_instance(&padded("WinGlance.exe "), 42, 1));
        assert!(!is_our_instance(&padded(""), 42, 1));
    }
}
