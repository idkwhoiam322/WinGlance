#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
mod winutil;

use crate::config::Config;
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
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_BEGIN, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_DATA, GetFileSize, OPEN_ALWAYS, SetEndOfFile, SetFilePointer, WriteFile,
};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS, RtlCaptureStackBackTrace,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW, TranslateMessage,
};
use windows::core::PCWSTR;

use crate::events::{MEDIA_EVENT_MSG, MediaEvent};
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
            CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_APPEND_DATA.0 | FILE_WRITE_DATA.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
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
        let _ = std::fs::create_dir_all(&dir);
        if std::fs::metadata(&path)
            .map(|m| m.len() > CRASH_LOG_CAP)
            .unwrap_or(false)
        {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path);
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(message.as_bytes())
            });
    }));
}

/// Upper bound on `crash.log` before the next panic truncates it.
const CRASH_LOG_CAP: u64 = 1024 * 1024;

/// Holds the raw value of the single-instance mutex handle for the lifetime of
/// the process so the "Restart app" button can release it before spawning
/// the next instance. A relaunch must free it first, otherwise the freshly
/// launched process would see a live owner (this one) and exit immediately.
/// Stored as `isize` because `HANDLE` is neither `Send` nor `Sync` and cannot
/// live in a `static` directly.
static SINGLETON_HANDLE: OnceLock<isize> = OnceLock::new();

/// Acquires the single-instance mutex for the process lifetime. Returns the
/// handle while the caller holds it, or `None` when another instance already
/// owns the mutex. The handle must be kept alive until process exit; releasing
/// it would allow a second instance to start.
fn acquire_singleton() -> anyhow::Result<Option<HANDLE>> {
    unsafe {
        let name = wide("WinGlanceSingleInstance");
        let handle = CreateMutexW(None, true, PCWSTR(name.as_ptr()))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // The mutex already exists, so either a live instance owns it or
            // the previous instance died without releasing it (crash or kill),
            // which leaves the mutex abandoned. A zero-timeout wait tells the
            // cases apart: an abandoned mutex grants ownership immediately,
            // a live owner returns WAIT_TIMEOUT. Without this, the first
            // relaunch after a crash would exit, requiring a second launch.
            match WaitForSingleObject(handle, 0) {
                WAIT_ABANDONED | WAIT_OBJECT_0 => {
                    let _ = SINGLETON_HANDLE.set(handle.0 as isize);
                    Ok(Some(handle))
                }
                WAIT_TIMEOUT => {
                    let _ = CloseHandle(handle);
                    Ok(None)
                }
                WAIT_FAILED => {
                    let _ = CloseHandle(handle);
                    anyhow::bail!("WaitForSingleObject failed on the single-instance mutex");
                }
                _ => {
                    let _ = CloseHandle(handle);
                    anyhow::bail!("unexpected wait result on the single-instance mutex");
                }
            }
        } else {
            let _ = SINGLETON_HANDLE.set(handle.0 as isize);
            Ok(Some(handle))
        }
    }
}

/// Restarts the app in place so it reloads `config.toml` from disk. The
/// single-instance mutex is released first (see `SINGLETON_HANDLE`), then the
/// current executable is launched with the `--reload-config` marker (so the
/// new instance can record the reload in its own log) and this process exits.
/// Nothing under `%APPDATA%\WinGlance\WinGlance\data\` is deleted, so any
/// on-disk cache survives and the live log is preserved: the reloaded
/// process appends to it and marks the boundary instead of truncating it.
/// Only in-memory caches (icon/track/period) are lost, as they are on any
/// restart. If the new process cannot be launched, this instance keeps
/// running rather than disappearing.
pub fn relaunch_self() {
    if let Some(raw) = SINGLETON_HANDLE.get() {
        let handle = HANDLE(*raw as *mut c_void);
        unsafe {
            let _ = ReleaseMutex(handle);
            let _ = CloseHandle(handle);
        }
    }
    match env::current_exe() {
        Ok(exe) => {
            if process::Command::new(exe).arg("--reload-config").spawn().is_ok() {
                process::exit(0);
            }
            error!("reload config: launching the new process failed; keeping this instance running");
        }
        Err(error) => {
            error!("reload config: resolving the current executable path failed: {error:#}");
        }
    }
}

fn main() -> Result<()> {
    // The single-instance guard must come before any side effects: logging
    // truncates the live log and config recovery touches the user's file, so a
    // duplicate launch must not get that far.
    let _singleton = match acquire_singleton() {
        Ok(Some(handle)) => Some(handle),
        Ok(None) => {
            // Another instance holds the mutex; exit without touching its
            // log or config.
            return Ok(());
        }
        Err(error) => {
            // Fail closed: running without the singleton would let a second
            // instance truncate the live log or rewrite config while the
            // first is running. Logging is not initialized yet, so record the
            // failure in crash.log and exit.
            if let Some(dir) = config::Config::data_dir().ok().map(|d| d.join("logs")) {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("crash.log"))
                    .and_then(|mut f| {
                        use std::io::Write;
                        f.write_all(format!("could not acquire the single-instance mutex: {error:#}\n").as_bytes())
                    });
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
                let listener_config_worker = listener_config.clone();
                let now_showing_worker = now_showing_supervisor.clone();
                let worker_started = Instant::now();
                let worker = thread::Builder::new()
                    .name("WinGlance-smtc-worker".to_string())
                    .stack_size(1024 * 1024)
                    .spawn(move || {
                        let _ = smtc::SmtcListener::new(
                            event_tx_worker,
                            listener_config_worker,
                            worker_heartbeat,
                            worker_generation,
                            my_generation,
                            worker_shutdown,
                            now_showing_worker,
                        )
                        .run();
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
                let mut stalled = false;
                while !worker.is_finished() {
                    if supervisor_shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1000));
                    if worker_started.elapsed() > Duration::from_secs(120) {
                        consecutive_restarts = 0;
                    }
                    let last = *supervisor_heartbeat
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if last.elapsed() > Duration::from_secs(30) {
                        stalled = true;
                        break;
                    }
                }
                if supervisor_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                if stalled {
                    // Do not join: the worker may be blocked inside COM forever.
                    consecutive_restarts += 1;
                    let delay = worker_restart_delay(consecutive_restarts);
                    error!("SMTC worker stalled; restarting it in {}s", delay.as_secs());
                    sleep_interruptible(delay, &supervisor_shutdown);
                    continue;
                }
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
        })?;

    let main_queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
    let overlay_queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
    let main_wake: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let overlay_wake: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let overlay_hwnd = overlay::create_window(
        config.clone(),
        overlay_queue.clone(),
        overlay_wake.clone(),
        now_showing.clone(),
    )?;
    let main_hwnd = main_window::create_window(
        shared_config.clone(),
        main_queue.clone(),
        overlay_hwnd,
        main_wake.clone(),
    )?;

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

/// Whether an event must be delivered to the overlay queue. Worker failures
/// are history-only: the overlay never renders them, so they must not wake
/// the pill or occupy its queue. Every other event — including
/// `SessionRejected`, which drives the overlay's retire logic for sources
/// that leave the allow-list — is forwarded even though rejections are never
/// rendered as pills.
fn overlay_bound(event: &MediaEvent) -> bool {
    !matches!(event, MediaEvent::WorkerFailed { .. })
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
                    Ok(event) => event,
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
        && unsafe { PostMessageW(hwnd, MEDIA_EVENT_MSG, WPARAM(0), LPARAM(0)) }.is_err()
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
        && unsafe { PostMessageW(hwnd, MEDIA_EVENT_MSG, WPARAM(0), LPARAM(0)) }.is_err()
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
        // history-only and must not wake the pill or occupy its queue.
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
        let changed = MediaEvent::PlaybackStateChanged(PlaybackState::Playing, "Brave".into());
        assert!(overlay_bound(&changed), "playback events must reach the overlay");
    }
}
