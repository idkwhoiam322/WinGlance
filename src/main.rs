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
use crate::winapi::post_message;
use anyhow::Result;
use chrono::Local;
use log::{debug, error, info, warn};
use std::collections::VecDeque;
use std::env;
use std::ffi::c_void;
use std::os::windows::io::IntoRawHandle;
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, HWND, LPARAM, LocalFree, WAIT_ABANDONED,
    WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SE_KERNEL_OBJECT, SetSecurityInfo,
};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorSacl, IsValidSecurityDescriptor,
    LABEL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
};
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, WriteFile};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS, RtlCaptureStackBackTrace,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcessId, OpenEventW, ReleaseMutex, SetEvent,
    WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW, RegisterWindowMessageW, TranslateMessage,
};
use windows::core::{BOOL, PCWSTR};

use crate::events::{MEDIA_EVENT_MSG, MediaEvent, RESTART_RESULT_MSG};
use crate::overlay::EventQueue;
use crate::winutil::wide;

/// The verified `crash.log` handle retained for the exception handler's
/// lifetime. Opened at startup under the pinned-verified discipline (parent
/// pinned through the open, final component not followed, identity validated
/// before any write), so the allocation-free handler can append through this
/// handle without any open — a reparse swap of any path component after
/// startup can never redirect a crash write, because the handle names the
/// object that was verified. Stored as a raw `usize` because `HANDLE` is
/// neither `Send` nor `Sync`; zero means the open failed and the handler
/// silently skips the crash log. The OS closes the handle at process exit;
/// it is deliberately never closed in the handler (which must stay
/// allocation- and teardown-free).
static CRASH_LOG_HANDLE: AtomicUsize = AtomicUsize::new(0);

/// Upper bound on crash.log growth. The file is diagnostic forensics, not
/// user data: past this size a crash loop stops appending, so a broken
/// install that crashes at startup cannot fill the disk. The earliest
/// records survive, which is where a loop's signature lives. Both writers —
/// the allocation-free vectored handler and the panic hook — append through
/// the one shared accounting path (`crash_log_write_retained`), so the cap and
/// the byte count can never desync between them.
pub(crate) const CRASH_LOG_CAP: u64 = 8 * 1024 * 1024;

/// Bytes already in crash.log as accounted by this process: seeded from the
/// file's length when the verified handle is installed, advanced by every
/// writer through `crash_log_write_retained`. Only touched before/during a
/// crash path, so plain loads and stores are sufficient.
static CRASH_LOG_BYTES: AtomicU64 = AtomicU64::new(0);

/// The single accounting path every retained-handle crash write goes
/// through. Appends `data` at EOF of the verified retained handle and
/// advances the shared counter; once the counter reaches `CRASH_LOG_CAP`
/// appends stop (earliest records survive a crash loop). Returns false only
/// when no handle was retained at startup — callers without a handle fall
/// back to their own open-append path. Allocation-free, so the vectored
/// exception handler can call it under heap corruption. The handle is
/// append-only (`FILE_APPEND_DATA` without `FILE_WRITE_DATA`) so the OS
/// appends atomically without a `SetFilePointer` dance.
fn crash_log_write_retained(data: &[u8]) -> bool {
    let raw = CRASH_LOG_HANDLE.load(Ordering::SeqCst);
    if raw == 0 {
        return false;
    }
    // Reserve bytes against the cap before writing: compare-exchange reserves
    // the exact slice that will be written, so two concurrent writers cannot
    // both pass a stale `load >= CAP` check and exceed the cap. The reserve
    // is the truncated slice that fits within the remaining budget; the write
    // then appends exactly that slice via the atomic-append handle.
    let mut cur = CRASH_LOG_BYTES.load(Ordering::SeqCst);
    loop {
        if cur >= CRASH_LOG_CAP {
            return true;
        }
        let remaining = (CRASH_LOG_CAP - cur) as usize;
        let to_write_len = data.len().min(remaining);
        if to_write_len == 0 {
            return true;
        }
        let to_write = &data[..to_write_len];
        match CRASH_LOG_BYTES.compare_exchange(cur, cur + to_write_len as u64, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => {
                let handle = HANDLE(raw as *mut c_void);
                unsafe {
                    let mut written: u32 = 0;
                    if WriteFile(handle, Some(to_write), Some(&mut written as *mut _), None).is_ok() && written > 0 {
                        let _ = FlushFileBuffers(handle);
                        // If the OS wrote short (partial), reclaim the over-reserved tail
                        // so the counter matches the file length; over-reserve is
                        // conservative, under-reserve never exceeds the cap.
                        if (written as usize) < to_write_len {
                            let over = (to_write_len - written as usize) as u64;
                            CRASH_LOG_BYTES.fetch_sub(over, Ordering::SeqCst);
                        }
                    } else if to_write_len > 0 {
                        // Write failed after reservation: roll back the reservation
                        // so the next record can use the budget.
                        CRASH_LOG_BYTES.fetch_sub(to_write_len as u64, Ordering::SeqCst);
                    }
                }
                return true;
            }
            Err(actual) => cur = actual,
        }
    }
}

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
    // no format!, no heap allocation — safe under heap corruption. The
    // timestamp is uptime-milliseconds (GetTickCount64): allocation-free,
    // and enough to order records against each other and against a
    // panic-hook record's wall clock within one session.
    let uptime_ms = unsafe { GetTickCount64() };
    let mut buf = [0u8; 2048];
    let mut pos = 0usize;
    pos = crash_write_str(&mut buf, pos, b"CRASH access violation\n");
    pos = crash_write_str(&mut buf, pos, b"  uptime_ms = ");
    pos = crash_write_dec(&mut buf, pos, uptime_ms as usize);
    pos = crash_write_str(&mut buf, pos, b"\n");
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

    // Append through the shared accounting path: the verified handle was
    // retained at install time (no open, no close, no allocation here) and
    // `crash_log_write_retained` owns the cap and the byte counter for every
    // writer, so the panic hook can never desync it.
    crash_log_write_retained(&buf[..pos]);
    0 // EXCEPTION_CONTINUE_SEARCH
}

fn install_crash_handler(logs_dir: &Path) {
    // Pre-open crash.log under the verified-write discipline and retain the
    // handle: the allocation-free exception handler appends through it, so a
    // reparse swap of any path component after startup can never redirect a
    // crash write (the object was opened under the pinned parent and
    // identity-validated before the open returned). An empty crash.log may
    // appear at startup — that is the point: it pins the verified object. If
    // the open fails the handler degrades to skipping the crash log, exactly
    // as it did when the per-crash open failed. Append-only so NT appends
    // atomically without a seek.
    if let Ok(file) = crate::winutil::open_verified_file_append(&logs_dir.join("crash.log")) {
        // Seed the byte counter with the file's existing length, so the cap
        // bounds the whole file and not just this run's additions.
        let existing = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        CRASH_LOG_BYTES.store(existing.min(CRASH_LOG_CAP), Ordering::SeqCst);
        CRASH_LOG_HANDLE.store(file.into_raw_handle() as usize, Ordering::SeqCst);
    }
    unsafe {
        AddVectoredExceptionHandler(1, Some(crash_handler));
    }
}

/// Writes Rust panics to crash.log. A panic in a window-proc unwinds across
/// the extern "C" boundary, which aborts the process silently (no access
/// violation, so the vectored handler never fires) — without this hook a
/// panic looks like the app "stopped running randomly". The file is appended
/// to under the shared `CRASH_LOG_CAP`, so a panic loop cannot grow it
/// without bound.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(move |info| {
        let raw_payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown payload".to_string()
        };
        // Bound the payload: a hostile SMTC title (up to MAX_META_CHARS or a
        // 100 KiB injected string that reached a panic! site) must not make
        // this single crash.log line unbounded. Truncate to 512 chars with an
        // omission note so the loop signature survives.
        let payload = if raw_payload.chars().count() > 512 {
            let keep: String = raw_payload.chars().take(512).collect();
            let omitted = raw_payload.chars().count() - 512;
            format!("{keep}…[truncated {omitted} chars omitted]")
        } else {
            raw_payload
        };
        let location = info.location().map(|l| l.to_string()).unwrap_or_default();
        // Wall-clock timestamp so a record correlates with the live log's
        // sessions (the vectored handler records uptime-ms instead — it
        // must stay allocation-free).
        let message = format!("PANIC {} {payload} at {location}\n", Local::now().to_rfc3339());
        // One shared accounting path with the vectored handler: the
        // retained handle advances the shared cap counter, so the two
        // writers can never desync (see `crash_log_append`).
        crash_log_append(message.as_bytes());
    }));
}

/// Bounded waits for the restart handoff protocol. The old process gives the
/// successor at most `RESTART_READY_TIMEOUT` to signal ready, and the
/// successor (child) waits at most `RESTART_READY_WAIT` on the mutex the old
/// process releases on success.
const RESTART_READY_TIMEOUT: Duration = Duration::from_secs(10);
const RESTART_READY_WAIT: Duration = Duration::from_secs(15);

/// How long after the first aliveness sample the old process re-samples the
/// successor before releasing the singleton. A successor that dies right
/// after signaling ready — its own fallible startup is still ahead of it —
/// is caught by the second sample instead of leaving zero instances running.
const RESTART_REVERIFY_DELAY: Duration = Duration::from_millis(500);

/// Owns the single-instance mutex handle for the process lifetime. The handle
/// is closed exactly once: either by `release` during a successful restart
/// handoff, or by the OS when the process ends (the guard is held in a
/// `static`, which is never dropped by the runtime, so `Drop` covers the
/// take-then-fail windows inside the restart helper and nothing else). Stored
/// as a raw `isize` because `HANDLE` is neither `Send` nor `Sync`; moving the
/// numeric handle owner into the one restart helper is safe because no other
/// thread touches that guard while it is absent from `SINGLETON_GUARD`.
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

/// The singleton guard's raw handle value. The UI initiates restart work,
/// then the dedicated handoff helper takes this guard out of the slot and owns
/// it across the bounded ready wait/re-verification. Every failure puts it back
/// before posting `RESTART_RESULT_MSG`, so `main` keeps exactly one live guard
/// at all times and the UI thread never blocks on the handoff protocol.
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
        let _guard = match crate::winutil::HandleGuard::new(snapshot) {
            Some(g) => g,
            None => return false,
        };
        let snapshot = _guard.get();
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
        running
    }
}

/// Second tier of the tray-stranding recovery: a duplicate launch asks the
/// running instance to show its tracking window before the duplicate exits,
/// so a user whose tray icon is gone still has a way in (and, from the
/// window, a way to quit). Registered message + PostMessage, fire and
/// forget — a hung instance must not block the duplicate's exit.
fn ping_running_instance_to_show() {
    unsafe {
        let class = wide(crate::main_window::MAIN_WINDOW_CLASS);
        let Ok(hwnd) = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) else {
            return;
        };
        let name = wide(crate::main_window::SHOW_YOURSELF_MSG);
        let message = RegisterWindowMessageW(PCWSTR(name.as_ptr()));
        if message != 0 {
            let _ = crate::winapi::post_message(hwnd, message, WPARAM(0), LPARAM(0));
        }
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

/// The single accounting path for every non-vectored crash.log write. The
/// retained verified handle is preferred: it advances the shared byte
/// counter, so its cap-stop and the vectored handler's accounting can never
/// desync (a counter stranded at the cap would silently drop every later
/// crash record while reporting success). Only when the startup install
/// failed — no retained handle — does this fall back to an open-append
/// bounded write; in that mode no counter is active, so there is nothing to
/// strand. Two concurrent writers can still interleave records (seek-write
/// is not atomic and the counter can lose an increment) — accepted:
/// records are diagnostics, and a crash-time lock is not viable under heap
/// corruption.
pub(crate) fn crash_log_append(message: &[u8]) {
    if crash_log_write_retained(message) {
        return;
    }
    if let Ok(dir) = config::Config::ensure_logs_dir() {
        let _ = crate::winutil::append_verified_bounded(&dir.join("crash.log"), message, CRASH_LOG_CAP);
    }
}

/// Records a suspected single-instance squat to `crash.log`. The mutex is
/// live-held by a process that is not a running WinGlance instance, so this
/// launch exits with no feedback anywhere; without this line the app would
/// silently refuse to start.
fn report_suspected_squat() {
    crash_log_append(
        b"suspected single-instance mutex squat: 'WinGlanceSingleInstance' is held by a live process that is not a running WinGlance instance; this launch exits without starting\n",
    );
}

/// Security descriptor string for the named singleton objects (the
/// single-instance mutex and the restart-ready event). The DACL grants full
/// control only to SYSTEM, Administrators, and the object owner (the current
/// user); the mandatory label pins the objects at Medium integrity with
/// no-write-up / no-execute-up, so a lower-integrity process in the same
/// session cannot open these names for modify (release/signal) or wait
/// (SYNCHRONIZE falls under the mandatory execute policy) — the named-object
/// denial-of-service vectors for the singleton handshake. The objects are
/// session-local (no `Global\` prefix), so other users cannot reach them by
/// name at all; the DACL is the belt-and-braces for that boundary.
const SINGLETON_OBJECT_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)S:(ML;;NWNX;;;ME)";

/// Owns the `SECURITY_DESCRIPTOR` built from `SINGLETON_OBJECT_SDDL` and
/// hands out the `SECURITY_ATTRIBUTES` pointing at it. The descriptor is a
/// `LocalAlloc` allocation from the SDDL converter that must outlive the
/// attributes, so both travel together; `Drop` frees it.
struct SingletonSecurity(*mut std::ffi::c_void);

impl SingletonSecurity {
    /// Builds the descriptor from the constant SDDL. `None` only on a
    /// converter failure (a can't-happen for a constant string); the caller
    /// then proceeds with the default object security, and the crash.log
    /// line keeps that degradation visible.
    fn build() -> Option<SingletonSecurity> {
        let sddl = wide(SINGLETON_OBJECT_SDDL);
        let mut descriptor: PSECURITY_DESCRIPTOR = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), 1, &mut descriptor, None)
        }
        .ok()?;
        Some(SingletonSecurity(descriptor.0))
    }

    /// The attributes to pass to `CreateMutexW`/`CreateEventW`: a copy whose
    /// descriptor pointer stays valid while this wrapper is alive.
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: false.into(),
        }
    }

    fn descriptor(&self) -> PSECURITY_DESCRIPTOR {
        PSECURITY_DESCRIPTOR(self.0)
    }
}

impl Drop for SingletonSecurity {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0)));
        }
    }
}

/// Re-applies the singleton security descriptor to a freshly created named
/// object. The DACL is applied at creation through the attributes; this
/// re-apply is idempotent and additionally forces the mandatory label in,
/// because a creation-time SACL can be silently dropped on some paths. A
/// failure leaves the object with default security and records a crash.log
/// line — the only channel available in `acquire_singleton` — so an
/// unprotected launch is visible rather than silent.
fn harden_named_object(handle: HANDLE, security: &SingletonSecurity) {
    unsafe {
        let descriptor = security.descriptor();
        if !IsValidSecurityDescriptor(descriptor).as_bool() {
            crash_log_append(
                b"singleton hardening: the descriptor is invalid; the singleton objects run with default security\n",
            );
            return;
        }
        let mut dacl_present = BOOL(0);
        let mut dacl: *mut windows::Win32::Security::ACL = std::ptr::null_mut();
        let mut dacl_defaulted = BOOL(0);
        let mut sacl_present = BOOL(0);
        let mut sacl: *mut windows::Win32::Security::ACL = std::ptr::null_mut();
        let mut sacl_defaulted = BOOL(0);
        let dacl_ok = GetSecurityDescriptorDacl(descriptor, &mut dacl_present, &mut dacl, &mut dacl_defaulted);
        let sacl_ok = GetSecurityDescriptorSacl(descriptor, &mut sacl_present, &mut sacl, &mut sacl_defaulted);
        if dacl_ok.is_err() || sacl_ok.is_err() || !dacl_present.as_bool() || !sacl_present.as_bool() {
            crash_log_append(b"singleton hardening: reading the descriptor's DACL/label failed; the singleton objects run with default security\n");
            return;
        }
        let error = SetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            Some(sacl),
        );
        if !error.is_ok() {
            let message = format!(
                "singleton hardening: SetSecurityInfo failed (error {}); the singleton objects run with default security\n",
                error.0
            );
            crash_log_append(message.as_bytes());
        }
    }
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
        let security = SingletonSecurity::build();
        let attributes = security.as_ref().map(|s| s.attributes());
        let attributes_ptr = attributes.as_ref().map(|a| a as *const SECURITY_ATTRIBUTES);
        let handle = CreateMutexW(attributes_ptr, true, PCWSTR(name.as_ptr())).map_err(|error| {
            anyhow::anyhow!(
                "creating/opening the single-instance mutex failed: {error} (another process may already hold the name with restrictive permissions)"
            )
        })?;
        // ERROR_ALREADY_EXISTS is only meaningful immediately after the
        // create call; capture it before the hardening calls below can
        // overwrite it.
        let already_exists = GetLastError() == ERROR_ALREADY_EXISTS;
        // Hardening is applied to the object whenever this process holds a
        // handle, fresh or existing: an object created by an older, unhardened
        // instance is retro-fitted (object security is not handle-scoped).
        if let Some(security) = security.as_ref() {
            harden_named_object(handle, security);
        }
        if !already_exists {
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
                            // Wait for the old process to release the mutex.
                            // A WAIT_FAILED after we already signaled
                            // readiness is retried once with the full budget:
                            // giving up on a transient wait failure would
                            // strand the handoff with zero running instances
                            // (the old process releases on our signal).
                            let mut wait_failed_once = false;
                            return loop {
                                break match WaitForSingleObject(handle, RESTART_READY_WAIT.as_millis() as u32) {
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
                                        crash_log_append(
                                            b"restart handoff timed out waiting for the single-instance mutex; the old process either kept the singleton or a concurrent launch acquired it; this launch exits without starting\n",
                                        );
                                        Ok(None)
                                    }
                                    WAIT_FAILED if !wait_failed_once => {
                                        wait_failed_once = true;
                                        crash_log_append(
                                            b"restart handoff: waiting on the single-instance mutex failed; retrying once\n",
                                        );
                                        continue;
                                    }
                                    _ => {
                                        let _ = CloseHandle(handle);
                                        anyhow::bail!("unexpected wait result on the single-instance mutex")
                                    }
                                };
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
                if winglance_instance_running_retried() {
                    // A genuine running instance: the most likely reader of
                    // a duplicate launch is a user whose tray icon is
                    // missing (failed add, lost Explorer restart) trying to
                    // get the app back — ping it to show its window before
                    // this duplicate exits. No balloon from the duplicate
                    // itself: AGENTS.md forbids pop-ups on plain launch and
                    // the duplicate has no tray icon to balloon from; the
                    // ping + silent exit is the contract, squat vs duplicate
                    // distinction stays in crash.log.
                    ping_running_instance_to_show();
                } else {
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

/// Whether the handoff successor is still running at one sampling instant:
/// `try_wait` reports `Ok(None)` only for a live child. Extracted so the
/// re-verify decision is testable without spawning processes.
fn handoff_child_alive(child: &mut process::Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

/// Whether the old instance may release the singleton after re-verifying the
/// successor: every sample must have seen it alive. A single dead sample
/// aborts the handoff — releasing on a stale signal would leave zero running
/// instances (the guard is restored instead and the old instance continues).
fn handoff_survives_reverify(samples: [bool; 2]) -> bool {
    samples[0] && samples[1]
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
fn relaunch_with_guard(mut guard: SingletonGuard) {
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
    let security = SingletonSecurity::build();
    let attributes = security.as_ref().map(|s| s.attributes());
    let attributes_ptr = attributes.as_ref().map(|a| a as *const SECURITY_ATTRIBUTES);
    let ready = match unsafe { CreateEventW(attributes_ptr, false, false, PCWSTR(event_name.as_ptr())) } {
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
    if let Some(security) = security.as_ref() {
        harden_named_object(ready, security);
    }
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
    // guard into a gap nothing can take. Sample the successor's aliveness
    // twice — once now, once after a short delay — and release only if both
    // samples saw it alive (`try_wait` is non-blocking; a running child
    // reports `Ok(None)`). The second sample closes the window where a
    // successor dies immediately after signaling: its own fallible startup
    // is still ahead of it, and the old process exiting on a stale signal
    // would leave zero instances running.
    let mut samples = [handoff_child_alive(&mut child), false];
    std::thread::sleep(RESTART_REVERIFY_DELAY);
    samples[1] = handoff_child_alive(&mut child);
    if !handoff_survives_reverify(samples) {
        error!("restart: the new process exited before the handoff completed; keeping this instance running");
        unsafe {
            let _ = CloseHandle(ready);
        }
        *SINGLETON_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(guard);
        return;
    }
    // Handoff accepted: the successor stayed alive across both samples.
    // Release the mutex and exit; the child's bounded wait acquires
    // ownership. The child keeps running, so it must not be waited on.
    // First reseat this process's live-log cursor to EOF: the
    // successor appends its boundary line to the preserved file before we
    // exit, and any late write from our remaining threads must land after
    // it, not overwrite it from a stale offset.
    crate::logging::reseat_live_log_to_eof();
    guard.release();
    unsafe {
        let _ = CloseHandle(ready);
    }
    drop(child);
    process::exit(0);
}

/// Starts the restart handoff without blocking the UI thread. The helper is
/// created *before* the singleton guard leaves the shared slot, so a thread-
/// creation failure cannot accidentally drop/release the mutex. Once the
/// helper exists, the guard is transferred through a one-item channel; the
/// helper owns every blocking wait and either exits the process on success or
/// restores the guard and posts `RESTART_RESULT_MSG` on failure. The helper
/// never touches window state directly.
pub fn spawn_handoff_thread(hwnd: HWND) {
    // UI clicks are serialized, so an empty slot means a handoff is already
    // active (or teardown owns the guard). Do not create another waiter.
    if SINGLETON_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_none()
    {
        warn!("restart: handoff already in progress; ignoring duplicate request");
        return;
    }

    let (guard_tx, guard_rx) = mpsc::sync_channel::<SingletonGuard>(1);
    let hwnd_raw = hwnd.0 as isize;
    let worker = thread::Builder::new()
        .name("WinGlance-restart-handoff".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            let Ok(guard) = guard_rx.recv() else {
                return;
            };
            relaunch_with_guard(guard);
            // Success never returns (`process::exit`). A return means every
            // failure path restored the singleton guard; tell the UI thread
            // only that the attempt completed so it can resume normal state
            // ownership without this helper manipulating windows directly.
            let hwnd = HWND(hwnd_raw as *mut c_void);
            unsafe {
                let _ = post_message(hwnd, RESTART_RESULT_MSG, WPARAM(0), LPARAM(0));
            }
        });
    let Ok(worker) = worker else {
        error!("restart: could not create the handoff helper; keeping this instance running");
        return;
    };

    let guard = SINGLETON_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let Some(guard) = guard else {
        // Another request won the slot between the pre-check and transfer.
        // Dropping the sender lets this just-created helper exit immediately.
        drop(guard_tx);
        drop(worker);
        warn!("restart: handoff already in progress; ignoring duplicate request");
        return;
    };
    if let Err(error) = guard_tx.send(guard) {
        // The helper exited before accepting ownership. Recover the guard from
        // SendError rather than dropping it: dropping would release the mutex
        // and leave the live UI unprotected.
        *SINGLETON_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.0);
        error!("restart: handoff helper exited before accepting the singleton guard");
    }
    // Detached by design. On success it terminates the process; on failure it
    // restores the guard and posts the private completion message.
    drop(worker);
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
            // owner from here on; the handoff helper takes the guard out of it
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
            crash_log_append(format!("could not acquire the single-instance mutex: {error:#}\n").as_bytes());
            return Err(error);
        }
    };

    // Logging initializes before the config loads: a corrupted config.toml now
    // falls back to defaults, and that fallback must be diagnosable through
    // the log file. The reload marker must be scanned first, because on that
    // path the live log is preserved (appended to) instead of truncated.
    let reload_config = std::env::args_os().any(|arg| arg == "--reload-config");
    let logs_dir = config::Config::ensure_logs_dir()?;
    logging::init_logging(&logs_dir, reload_config);
    // Crash handlers install BEFORE Config::load: a failure during startup
    // must leave a crash.log record. The directory was created through the
    // verified WinGlance-owned descendant chain above; startup never scans or
    // removes pre-existing data-directory entries.
    install_crash_handler(&logs_dir);
    install_panic_hook();
    let config = match config::Config::load() {
        Ok(config) => config,
        Err(error) => {
            // A load failure is fatal (the data dir itself is unusable), and
            // the release build has no console to show it on: record the
            // reason in crash.log — the one channel independent of both the
            // live log and config.toml — before exiting. Best-effort
            // append; if even this fails the exit is as silent as before.
            crash_log_append(
                format!("config.toml could not be loaded; WinGlance cannot start: {error:#}\n").as_bytes(),
            );
            return Err(error);
        }
    };
    config.log_settings();

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
    // Shared downstream artwork byte counter (see
    // `smtc::MAX_IN_FLIGHT_ARTWORK_BYTES`): the SMTC worker reserves bytes
    // atomically before queue admission and attaches that reservation to the
    // TrackInfo's shared lifetime token. The final artwork-bearing clone — in
    // any worker queue, window queue, or UI state — releases it. Shared across
    // worker restarts so leaked/stalled generations remain honestly counted.
    let in_flight_art: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let supervisor_art = in_flight_art.clone();
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
    // Merged wake-up channel for the SMTC worker: WinRT wake-up signals
    // and the main window's best-effort control wake-up hints share one
    // bounded queue so the worker's single receive loop stays responsive
    // to both. Worker control commands themselves no longer travel over
    // this queue — they live in `control_mailbox` below, which never
    // drops. The channel is created here — not inside the worker — because
    // it must survive worker restarts: the replacement worker receives
    // from the same receiver, so a wake posted by the main window is never
    // lost to a restart.
    let (control_tx, control_rx) = mpsc::sync_channel::<smtc::Signal>(smtc::SIGNAL_QUEUE_CAP);
    // Latest-value mailbox for worker control commands (settings pushes).
    // Unlike the channel, a mailbox push can never be dropped by a
    // saturated queue, and the mailbox survives worker restarts: the
    // replacement worker drains what its predecessor left behind, so a
    // command posted just before a restart is still applied. The channel
    // carries only the paired best-effort wake-up hints (`ControlWake`).
    let control_mailbox: Arc<Mutex<smtc::ControlMailbox>> = Arc::new(Mutex::new(smtc::ControlMailbox::default()));
    let supervisor_control_tx = control_tx.clone();
    let main_window_control_tx = control_tx.clone();
    let supervisor_control_mailbox = control_mailbox.clone();
    let main_window_control_mailbox = control_mailbox.clone();
    let supervisor_control_rx: Arc<Mutex<mpsc::Receiver<smtc::Signal>>> = Arc::new(Mutex::new(control_rx));
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
    // Churn/wedged-read exclusions, shared across worker generations: a
    // replacement worker must not re-pay a fresh 10 s read for every source
    // its predecessor already excluded — the exclusion survives the
    // restart it exists to bound.
    let exclusions = smtc::shared_exclusions();
    let exclusions_supervisor = exclusions.clone();
    // Synchronous SMTC COM calls that have no WinRT async form run on one
    // reusable isolated helper per worker. The helper budget is shared across
    // generations: a call that never returns can leak its helper, but it can
    // never reset the accounting by forcing a worker restart.
    let sync_com_budget = smtc::sync_com_budget();
    let supervisor_sync_com_budget = sync_com_budget.clone();
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
            // Process-lifetime count of workers abandoned while blocked in
            // COM. Unlike the old rolling window this never resets after a
            // healthy stretch: leaked threads remain live until process exit,
            // so their accounting must live just as long.
            let mut leaked_workers: usize = 0;
            loop {
                if supervisor_shutdown.load(Ordering::Acquire) {
                    break;
                }
                if supervisor_sync_com_budget.breaker_open() {
                    let reason = "SMTC synchronous COM calls stopped responding repeatedly; media notifications are disabled until WinGlance restarts to bound leaked COM threads".to_string();
                    error!("{reason}");
                    let _ = supervisor_tx.send(Arc::new(MediaEvent::WorkerFailed { reason }));
                    break;
                }
                if leaked_worker_budget_exhausted(leaked_workers) {
                    let reason = format!(
                        "SMTC workers have hung {MAX_LEAKED_WORKERS} times during this WinGlance run; media notifications are disabled until WinGlance restarts to bound leaked COM threads"
                    );
                    error!("{reason}");
                    let _ = supervisor_tx.send(Arc::new(MediaEvent::WorkerFailed { reason }));
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
                let my_generation = {
                    // Bump under the mailbox lock so the verify-take in the
                    // worker's `drain_control` (same lock) is atomic with the
                    // restart: a superseded worker can never consume a control
                    // command meant for its successor.
                    let _guard = supervisor_control_mailbox
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    supervisor_generation.fetch_add(1, Ordering::SeqCst) + 1
                };
                let event_tx_worker = event_tx.clone();
                let art_worker = supervisor_art.clone();
                let budget_warned_worker = supervisor_budget_warned.clone();
                let listener_config_worker = listener_config.clone();
                let control_tx_worker = supervisor_control_tx.clone();
                let control_rx_worker = supervisor_control_rx.clone();
                let control_mailbox_worker = supervisor_control_mailbox.clone();
                let now_showing_worker = now_showing_supervisor.clone();
                let exclusions_worker = exclusions_supervisor.clone();
                let sync_com_budget_worker = supervisor_sync_com_budget.clone();
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
                // The worker's config snapshot, sampled once per spawn. The
                // worker never reads the shared config again: live changes
                // are pushed into the control mailbox, which survives
                // restarts (the replacement worker drains what its
                // predecessor left), and the seed keeps a brand-new worker
                // current even when the user never touches the settings
                // again.
                let listener_seed = {
                    let cfg = listener_config_worker
                        .read()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    smtc::ListenerSeed {
                        media_sources: cfg.behavior.media_sources.clone(),
                        debounce_ms: cfg.behavior.debounce_ms,
                    }
                };
                let worker = thread::Builder::new()
                    .name("WinGlance-smtc-worker".to_string())
                    .stack_size(1024 * 1024)
                    .spawn(move || {
                        let _ = smtc::SmtcListener::new(
                            event_tx_worker,
                            art_worker,
                            budget_warned_worker,
                            listener_seed,
                            worker_heartbeat,
                            worker_generation,
                            my_generation,
                            worker_shutdown,
                            now_showing_worker,
                            exclusions_worker,
                            sync_com_budget_worker,
                            control_tx_worker,
                            control_rx_worker,
                            control_mailbox_worker,
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
                    if supervisor_shutdown.load(Ordering::Acquire) {
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
                        // Every hard stall leaks the worker thread (stack +
                        // COM registrations). Count it for the whole process
                        // lifetime: a later healthy stretch cannot reclaim a
                        // thread that is still stuck, so it must not reclaim
                        // the budget either. The next loop-top check refuses
                        // to spawn another worker once the hard cap is reached.
                        leaked_workers = leaked_workers.saturating_add(1);
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
                                // Decode both payload shapes (a
                                // format!-based panic message is a String)
                                // so the log names the actual panic.
                                let payload = if let Some(s) = panic.downcast_ref::<&str>() {
                                    (*s).to_string()
                                } else if let Some(s) = panic.downcast_ref::<String>() {
                                    s.clone()
                                } else {
                                    "unknown panic".to_string()
                                };
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
    let overlay_hwnd = match overlay::create_window(
        config.clone(),
        overlay_queue.clone(),
        overlay_wake.clone(),
        now_showing.clone(),
    ) {
        Ok(hwnd) => hwnd,
        Err(error) => {
            // The supervisor is already running: tell it to stop spawning
            // workers before the error propagates, so a failed startup does
            // not keep replacing SMTC workers until process exit reaps
            // everything.
            shutdown.store(true, Ordering::Release);
            return Err(error);
        }
    };
    let main_hwnd = match main_window::create_window(
        shared_config.clone(),
        main_queue.clone(),
        overlay_hwnd,
        main_wake.clone(),
        main_window_control_tx,
        main_window_control_mailbox,
    ) {
        Ok(hwnd) => hwnd,
        Err(error) => {
            // The overlay was already created and armed (foreground hook,
            // animation timer, name cell, state box). Teardown must not
            // depend on process exit: destroy it now so its WM_NCDESTROY
            // runs the full teardown — hook unhook, timer delete, name-cell
            // null, box free — before the error propagates. No forwarder
            // exists yet, so nothing can post to the overlay after this
            // point. The supervisor is told to stop first (see above).
            shutdown.store(true, Ordering::Release);
            unsafe {
                let _ = DestroyWindow(overlay_hwnd);
            }
            return Err(error);
        }
    };

    // Clones of the queues stay behind for the shutdown-stranded count
    //: how many buffered events die with the process is part of the
    // post-mortem.
    let forwarder_handle = match spawn_event_forwarder(
        main_hwnd,
        overlay_hwnd,
        main_queue.clone(),
        overlay_queue.clone(),
        main_wake,
        overlay_wake,
        event_rx,
        supervisor_rx,
        forwarder_shutdown,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            // Both windows exist: the normal shutdown path cannot run (it
            // lives below), so stop the producers here — the supervisor
            // exits within ~1 s of the flag, and its senders drop with
            // main's return.
            shutdown.store(true, Ordering::Release);
            return Err(error);
        }
    };

    let message_result = message_loop();
    debug!("message loop exited; shutting down");

    // Stop the producers before destroying the windows: the forwarder must
    // not post to an HWND that teardown is about to free. The supervisor
    // exits within ~1s of the flag; a stalled worker is left for process
    // exit (it may be blocked inside COM and cannot be joined).
    shutdown.store(true, Ordering::Release);
    let _ = forwarder_handle.join();
    let _ = supervisor_handle.join();

    // Account for events that die with the process: a post-mortem
    // asking "what happened to the last events" needs to know they were
    // dropped at shutdown rather than silently lost mid-run.
    let stranded = main_queue.lock().map(|q| q.len()).unwrap_or(0) + overlay_queue.lock().map(|q| q.len()).unwrap_or(0);
    if stranded > 0 {
        debug!("{stranded} buffered event(s) dropped at shutdown (queues were not fully drained)");
    }

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

/// Hard process-lifetime cap on SMTC workers abandoned while blocked in COM.
/// A leaked thread is not reclaimed by a later healthy stretch, so neither is
/// its budget. After five stalls the supervisor enters the degraded state and
/// refuses to spawn a sixth worker until the user restarts WinGlance. This is
/// deliberately independent of `MAX_WORKER_RESTARTS`, whose consecutive count
/// may still reset after a healthy two-minute run.
const MAX_LEAKED_WORKERS: usize = 5;

/// Whether spawning another worker would exceed the process-lifetime leaked
/// worker budget. Pure so the no-sixth-worker invariant is unit-testable.
fn leaked_worker_budget_exhausted(leaked_workers: usize) -> bool {
    leaked_workers >= MAX_LEAKED_WORKERS
}

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
    while remaining > Duration::ZERO && !shutdown.load(Ordering::Acquire) {
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
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    // HWND is not Send; the raw handle value is all the forwarder needs to
    // post with.
    let main_raw = main_hwnd.0 as isize;
    let overlay_raw = overlay_hwnd.0 as isize;
    thread::Builder::new()
        .name("WinGlance-events".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            // Idle wait adaptation: with nothing arriving, the recv timeout
            // stretches from 200 ms up to 1 s so an always-running utility
            // does not wake the machine five times a second for two
            // container checks. Any event resets it; the shutdown flag is
            // polled on every wakeup, and the supervisor's own 1 s join
            // budget already dominates exit latency, so the longer wait
            // does not slow shutdown down.
            let mut quiet_cycles: u32 = 0;
            while !shutdown.load(Ordering::Acquire) {
                // One-shot status events from the supervisor (at most one
                // WorkerFailed per session, then it gives up). History-only:
                // never wake the pill or occupy its queue.
                while let Ok(event) = supervisor_rx.try_recv() {
                    quiet_cycles = 0;
                    push_and_wake(
                        &main_queue,
                        &main_wake,
                        event,
                        HWND(main_raw as *mut c_void),
                        "main window",
                    );
                }
                let event = match receiver.recv_timeout(Duration::from_millis(200 * (1 + quiet_cycles.min(4) as u64))) {
                    Ok(event) => {
                        quiet_cycles = 0;
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
        .map_err(anyhow::Error::from)
}

/// Whether an event should be preferred as a queue-overflow survivor:
/// one-shot signals whose loss is permanent for the whole run (the budget
/// warning fires once per app run; a lost `SourceGone` lets the overlay's
/// standby restore a dead source's track on re-enable; `WorkerFailed` is the
/// terminal report). Ordinary events are evicted before these; only an
/// all-protected overload may shed the oldest protected signal to preserve
/// the hard queue bound.
fn is_one_shot_signal(event: &MediaEvent) -> bool {
    matches!(
        event,
        MediaEvent::WorkerFailed { .. } | MediaEvent::ArtworkBudgetExceeded | MediaEvent::SourceGone { .. }
    )
}

/// Applies the window-queue hard cap after a push: when the queue holds more
/// than `EVENT_QUEUE_CAP` events, the oldest droppable event is dropped in
/// favor of the newest. One-shot signals are preferred as survivors because
/// losing them may hide terminal state, but the queue is never allowed to
/// exceed the hard cap: if every queued event is protected, the oldest
/// protected signal is shed as a last resort. A permanently stalled window
/// cannot be given lossless delivery and bounded memory at the same time;
/// newest state wins under that pathological overload.
fn enforce_queue_cap(queue: &mut VecDeque<Arc<MediaEvent>>, name: &str) {
    while queue.len() > EVENT_QUEUE_CAP {
        let victim = queue.iter().position(|event| !is_one_shot_signal(event)).unwrap_or(0);
        let protected = is_one_shot_signal(&queue[victim]);
        queue.remove(victim);
        if protected {
            warn!(
                "the {name} event queue reached its hard cap of {EVENT_QUEUE_CAP}; dropping the oldest protected one-shot signal"
            );
        } else {
            warn!("the {name} event queue exceeded its cap of {EVENT_QUEUE_CAP}; dropping a buffered event");
        }
    }
}

/// Clears a window's pending-event queue and its wake flag, logging how many
/// events were dropped. Used when the window cannot be poked (a failed
/// post): leaving the queue populated without a wake message in flight
/// would strand those events until some unrelated future event reposts.
fn clear_and_account(queue: &EventQueue, wake: &AtomicBool, name: &str) {
    wake.store(false, Ordering::Release);
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
    if let MediaEvent::TrackChanged(incoming) = event.as_ref()
        && let Some(index) = q.iter().position(|queued| {
            matches!(queued.as_ref(), MediaEvent::TrackChanged(existing) if existing.source_app == incoming.source_app)
        })
    {
        q.remove(index);
    }
    q.push_back(event);
    enforce_queue_cap(&mut q, name);
    if !wake.swap(true, Ordering::AcqRel)
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
        && !wake.swap(true, Ordering::AcqRel)
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
    // Test-only Win32 surface (production `main.rs` never touches these).
    use crate::events::{PlaybackState, media_event_into_owned};
    use windows::Win32::Foundation::GENERIC_ALL;
    use windows::Win32::Security::Authorization::GetSecurityInfo;
    use windows::Win32::Security::GetAce;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_ACCESS_RIGHTS};
    // ACE type constants: winnt.h ACCESS_ALLOWED_ACE_TYPE (0) and
    // SYSTEM_MANDATORY_LABEL_ACE_TYPE (0x11). The crate gates them behind a
    // feature this crate does not enable; these values are stable ABI.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const SYSTEM_MANDATORY_LABEL_ACE_TYPE: u8 = 0x11;

    #[test]
    fn crash_log_handle_is_retained_under_the_verified_discipline_at_install() {
        // Regression guard: install must open
        // crash.log under the verified-write discipline (pinned parent,
        // no-reparse-follow, identity-checked) and RETAIN the handle, so the
        // allocation-free handler can append without any open — a reparse
        // swap of any path component after startup can never redirect the
        // crash write. The vectored handler this installs is inert for the
        // rest of the test process (it always returns EXCEPTION_CONTINUE_SEARCH
        // and only fires on access violations, which no test raises).
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("winglance-crash-handle-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        install_crash_handler(&dir);

        let raw = CRASH_LOG_HANDLE.load(Ordering::SeqCst);
        assert_ne!(raw, 0, "the verified crash-log handle must be retained at install");
        let crash_log = dir.join("crash.log");
        assert!(
            crash_log.is_file(),
            "install must pre-open the verified crash.log ({} exists)",
            crash_log.display()
        );
        // The retained handle is a real append-capable handle: appending
        // through it lands in crash.log (NT ignores the file pointer for
        // append-only handles, so no SetFilePointer dance).
        let handle = HANDLE(raw as *mut c_void);
        unsafe {
            let mut written: u32 = 0;
            let _ = WriteFile(handle, Some(b"test crash record\n"), Some(&mut written as *mut _), None);
        }
        assert_eq!(
            std::fs::read(&crash_log).unwrap(),
            b"test crash record\n",
            "writes through the retained handle must land in the verified crash.log"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaked_worker_budget_is_process_lifetime_and_refuses_a_sixth_worker() {
        // Nothing ages out: five abandoned workers are still five live OS
        // threads no matter how much healthy time passed between them. The
        // first five workers may exist; once all five have wedged, the next
        // loop iteration must refuse to spawn worker six.
        for leaked in 0..MAX_LEAKED_WORKERS {
            assert!(
                !leaked_worker_budget_exhausted(leaked),
                "{leaked} leaked workers must remain below the hard cap"
            );
        }
        assert!(
            leaked_worker_budget_exhausted(MAX_LEAKED_WORKERS),
            "five leaked workers make a sixth spawn terminal"
        );
        assert!(leaked_worker_budget_exhausted(MAX_LEAKED_WORKERS + 1));
    }

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
    fn handoff_survives_reverify_requires_every_sample_alive() {
        // The old instance releases the singleton only when BOTH aliveness
        // samples saw the successor running: the first sample is the
        // pre-existing check, the second runs after a short delay to catch a
        // successor that dies immediately after signaling ready. One dead
        // sample aborts — releasing on it would leave zero instances.
        assert!(handoff_survives_reverify([true, true]));
        assert!(!handoff_survives_reverify([false, true]), "dead at first sample: abort");
        assert!(
            !handoff_survives_reverify([true, false]),
            "died before re-verify: abort"
        );
        assert!(!handoff_survives_reverify([false, false]));
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

    /// The mandatory-label SID of a Medium object: the
    /// `SECURITY_MANDATORY_LABEL_AUTHORITY` identifier authority
    /// ({0,0,0,0,0,16}) with the single subauthority
    /// `SECURITY_MANDATORY_MEDIUM_RID` (0x2000) — S-1-16-0x2000.
    const MEDIUM_LABEL_AUTHORITY: [u8; 6] = [0, 0, 0, 0, 0, 16];
    const MEDIUM_LABEL_RID: u32 = 0x2000;

    /// Reads the SID stored at an ACE's `SidStart` slot (offset 8 into an
    /// `ACCESS_ALLOWED_ACE` / `SYSTEM_MANDATORY_LABEL_ACE`: 4-byte header, 4-byte
    /// mask, then the SID). SIDs are variable-length and the crate's `SID`
    /// struct only carries one subauthority slot, so the authority and
    /// subauthorities are read at their byte offsets with unaligned reads.
    unsafe fn read_ace_sid(ace: *const std::ffi::c_void) -> ([u8; 6], Vec<u32>) {
        unsafe {
            // The ACE prefix is 4-byte header plus 4-byte mask; the SID
            // (revision, count, authority, subauthorities) starts at offset 8.
            let sid = (ace as *const u8).add(8);
            let count = *sid.add(1) as usize;
            let mut authority = [0u8; 6];
            authority.copy_from_slice(std::slice::from_raw_parts(sid.add(2), 6));
            let subauthorities = (0..count)
                .map(|index| std::ptr::read_unaligned(sid.add(8 + 4 * index) as *const u32))
                .collect();
            (authority, subauthorities)
        }
    }

    /// Reads an ACE's type byte and access mask — the fixed
    /// `ACE_HEADER`-plus-mask prefix shared by every ACE shape.
    unsafe fn read_ace_type_and_mask(ace: *const std::ffi::c_void) -> (u8, u32) {
        unsafe {
            let base = ace as *const u8;
            (*base, std::ptr::read_unaligned(base.add(4) as *const u32))
        }
    }

    #[test]
    fn singleton_security_descriptor_carries_the_intended_dacl_and_label() {
        // The constant SDDL must parse into a descriptor whose DACL grants
        // full control to exactly SYSTEM, Administrators, and the object
        // owner (owner rights), and whose mandatory label pins Medium
        // integrity. The kernel-applied counterpart is covered by the
        // next test; this one pins the descriptor contents themselves.
        let security = SingletonSecurity::build().expect("the constant SDDL must parse");
        unsafe {
            assert!(IsValidSecurityDescriptor(security.descriptor()).as_bool());
            let mut dacl_present = BOOL(0);
            let mut dacl: *mut windows::Win32::Security::ACL = std::ptr::null_mut();
            let mut dacl_defaulted = BOOL(0);
            assert!(
                GetSecurityDescriptorDacl(security.descriptor(), &mut dacl_present, &mut dacl, &mut dacl_defaulted)
                    .is_ok()
            );
            assert!(dacl_present.as_bool(), "the DACL must be present");
            let mut aces = Vec::new();
            for index in 0..8u32 {
                let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
                if GetAce(dacl, index, &mut ace).is_err() {
                    break;
                }
                aces.push(ace);
            }
            assert_eq!(aces.len(), 3, "exactly three DACL ACEs, got {}", aces.len());
            for (index, ace) in aces.iter().enumerate() {
                let (ace_type, mask) = read_ace_type_and_mask(*ace);
                assert_eq!(ace_type, ACCESS_ALLOWED_ACE_TYPE, "ACE {index} must be an allow");
                assert_eq!(mask, GENERIC_ALL.0, "ACE {index} must grant full control");
                let (authority, subauthorities) = read_ace_sid(*ace);
                let expected: ([u8; 6], Vec<u32>) = match index {
                    0 => ([0, 0, 0, 0, 0, 5], vec![18]),      // SYSTEM (S-1-5-18)
                    1 => ([0, 0, 0, 0, 0, 5], vec![32, 544]), // Administrators (S-1-5-32-544)
                    _ => ([0, 0, 0, 0, 0, 3], vec![4]),       // Owner rights (S-1-3-4)
                };
                assert_eq!((authority, subauthorities), expected, "ACE {index} trustee");
            }
            let mut sacl_present = BOOL(0);
            let mut sacl: *mut windows::Win32::Security::ACL = std::ptr::null_mut();
            let mut sacl_defaulted = BOOL(0);
            assert!(
                GetSecurityDescriptorSacl(security.descriptor(), &mut sacl_present, &mut sacl, &mut sacl_defaulted)
                    .is_ok()
            );
            assert!(sacl_present.as_bool(), "the mandatory label must be present");
            let mut label_ace: *mut std::ffi::c_void = std::ptr::null_mut();
            assert!(GetAce(sacl, 0, &mut label_ace).is_ok(), "the label ACE must exist");
            assert!(
                GetAce(sacl, 1, &mut std::ptr::null_mut()).is_err(),
                "exactly one label ACE"
            );
            let (ace_type, _) = read_ace_type_and_mask(label_ace);
            assert_eq!(ace_type, SYSTEM_MANDATORY_LABEL_ACE_TYPE, "the label ACE type");
            let (authority, subauthorities) = read_ace_sid(label_ace);
            assert_eq!(
                authority, MEDIUM_LABEL_AUTHORITY,
                "the label uses the mandatory authority"
            );
            assert_eq!(subauthorities, vec![MEDIUM_LABEL_RID], "the label must be Medium");
        }
    }

    #[test]
    fn hardened_singleton_objects_reach_the_kernel_and_stay_open_to_the_user() {
        // End-to-end: a named mutex created through the production attributes
        // must carry the restrictive DACL and the Medium label as seen by the
        // kernel (this also proves the label write via SetSecurityInfo works
        // without elevation), and the same user must still be able to open the
        // object — the successor process of a restart and the duplicate-launch
        // probe both open these objects by name.
        let security = SingletonSecurity::build().expect("the constant SDDL must parse");
        let attributes = security.attributes();
        let name = format!("WinGlanceTestHardened-{}", process::id());
        let name_wide = wide(&name);
        let mutex = unsafe { CreateMutexW(Some(&attributes), true, PCWSTR(name_wide.as_ptr())) }
            .expect("creating the test mutex");
        harden_named_object(mutex, &security);
        unsafe {
            // SYNCHRONIZE (0x0010_0000) is the standard right for waitable
            // handles; the crate only ships it typed to FILE_ACCESS_RIGHTS.
            let opened = OpenMutexW(
                SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000),
                false,
                PCWSTR(name_wide.as_ptr()),
            )
            .expect("the owning user must still open the hardened mutex");
            let _ = CloseHandle(opened);
            let mut descriptor: PSECURITY_DESCRIPTOR = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
            let error = GetSecurityInfo(
                mutex,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                Some(&mut descriptor),
            );
            assert_eq!(error.0, 0, "GetSecurityInfo must succeed on our own object");
            assert!(!descriptor.0.is_null());
            let mut dacl_present = BOOL(0);
            let mut dacl: *mut windows::Win32::Security::ACL = std::ptr::null_mut();
            let mut dacl_defaulted = BOOL(0);
            assert!(GetSecurityDescriptorDacl(descriptor, &mut dacl_present, &mut dacl, &mut dacl_defaulted).is_ok());
            let mut ace_count = 0;
            for index in 0..8u32 {
                if GetAce(dacl, index, &mut std::ptr::null_mut()).is_ok() {
                    ace_count += 1;
                } else {
                    break;
                }
            }
            assert_eq!(
                ace_count, 3,
                "the kernel-applied DACL must carry the three ACEs, got {ace_count}"
            );
            let mut sacl_present = BOOL(0);
            let mut sacl: *mut windows::Win32::Security::ACL = std::ptr::null_mut();
            let mut sacl_defaulted = BOOL(0);
            assert!(GetSecurityDescriptorSacl(descriptor, &mut sacl_present, &mut sacl, &mut sacl_defaulted).is_ok());
            assert!(sacl_present.as_bool(), "the kernel-applied object must carry the label");
            let mut label_ace: *mut std::ffi::c_void = std::ptr::null_mut();
            assert!(GetAce(sacl, 0, &mut label_ace).is_ok());
            let (_, subauthorities) = read_ace_sid(label_ace);
            assert_eq!(
                subauthorities,
                vec![MEDIUM_LABEL_RID],
                "the object must be labeled Medium"
            );
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
            let _ = CloseHandle(mutex);
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
    fn window_queue_coalesces_track_changed_to_newest_per_source() {
        let mut queue = VecDeque::new();
        let old = Arc::new(MediaEvent::TrackChanged(crate::events::TrackInfo {
            title: "old".into(),
            source_app: "spotify".into(),
            ..Default::default()
        }));
        let newest = Arc::new(MediaEvent::TrackChanged(crate::events::TrackInfo {
            title: "new".into(),
            source_app: "spotify".into(),
            ..Default::default()
        }));
        queue.push_back(old);
        if let MediaEvent::TrackChanged(incoming) = newest.as_ref()
            && let Some(index) = queue.iter().position(|queued| {
                matches!(queued.as_ref(), MediaEvent::TrackChanged(existing) if existing.source_app == incoming.source_app)
            })
        {
            queue.remove(index);
        }
        queue.push_back(newest);
        assert_eq!(queue.len(), 1);
        assert!(matches!(queue[0].as_ref(), MediaEvent::TrackChanged(track) if track.title == "new"));
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
    fn one_shot_signals_survive_queue_overflow() {
        // When the cap is exceeded, ordinary events are evicted but
        // the never-re-emitted signals (budget warning, source settle,
        // worker failure) survive — they cannot be re-emitted, and losing
        // them loses exactly the information the overload produced.
        let mut queue = VecDeque::new();
        for i in 0..EVENT_QUEUE_CAP {
            queue.push_back(Arc::new(MediaEvent::PlaybackStateChanged(
                PlaybackState::Playing,
                format!("src-{i}"),
            )));
        }
        queue.push_back(Arc::new(MediaEvent::ArtworkBudgetExceeded));
        queue.push_back(Arc::new(MediaEvent::SourceGone {
            source_app: "gone".into(),
        }));
        enforce_queue_cap(&mut queue, "test");
        assert_eq!(queue.len(), EVENT_QUEUE_CAP);
        assert!(
            queue
                .iter()
                .any(|e| matches!(e.as_ref(), MediaEvent::ArtworkBudgetExceeded)),
            "the budget warning must survive overflow"
        );
        assert!(
            queue
                .iter()
                .any(|e| matches!(e.as_ref(), MediaEvent::SourceGone { .. })),
            "the settle signal must survive overflow"
        );
        // If overload consists entirely of protected signals, they are still
        // preferred over ordinary victims, but the resource bound remains
        // absolute: the oldest protected signal is the last-resort victim.
        let mut protected = VecDeque::new();
        for _ in 0..EVENT_QUEUE_CAP + 5 {
            protected.push_back(Arc::new(MediaEvent::ArtworkBudgetExceeded));
            enforce_queue_cap(&mut protected, "test");
        }
        assert_eq!(protected.len(), EVENT_QUEUE_CAP);
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
            !wake.load(Ordering::Acquire),
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
            !wake.load(Ordering::Acquire),
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
    #[test]
    fn event_queue_cap_stays_hard_when_every_event_is_protected() {
        let mut queue = VecDeque::new();
        for index in 0..EVENT_QUEUE_CAP + 5 {
            queue.push_back(Arc::new(MediaEvent::SourceGone {
                source_app: format!("source-{index}"),
            }));
            enforce_queue_cap(&mut queue, "test");
        }
        assert_eq!(queue.len(), EVENT_QUEUE_CAP, "the window queue cap is absolute");
        match queue.front().map(Arc::as_ref) {
            Some(MediaEvent::SourceGone { source_app }) => {
                assert_eq!(
                    source_app, "source-5",
                    "oldest protected signals are the last-resort victims"
                )
            }
            other => panic!("expected SourceGone at the queue head, got {other:?}"),
        }
        match queue.back().map(Arc::as_ref) {
            Some(MediaEvent::SourceGone { source_app }) => {
                assert_eq!(source_app, &format!("source-{}", EVENT_QUEUE_CAP + 4));
            }
            other => panic!("expected newest SourceGone to survive, got {other:?}"),
        }
    }
}
