#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod config;
mod events;
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
use log::{error, info, warn};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
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
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW, TranslateMessage,
};
use windows::core::PCWSTR;

use crate::events::{MEDIA_EVENT_MSG, MediaEvent};
use crate::overlay::{EventQueue, wide};

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
    let record = unsafe { &*((*info).ExceptionRecord) };
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
                WAIT_ABANDONED | WAIT_OBJECT_0 => Ok(Some(handle)),
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
            Ok(Some(handle))
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
    // the log file.
    logging::init_logging(&config::Config::default().logs_dir());
    let config = config::Config::load()?;
    install_crash_handler(&config.logs_dir());
    install_panic_hook(&config.logs_dir());

    info!("starting WinGlance");

    if let Err(error) = autostart::apply(config.behavior.start_on_login) {
        warn!("start-on-login sync failed: {error:#}");
    }

    unsafe {
        if let Err(error) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            warn!("per-monitor DPI awareness unavailable: {error}");
        }
    }

    let (event_tx, event_rx) = mpsc::channel();
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
                if consecutive_restarts >= MAX_WORKER_RESTARTS {
                    error!(
                        "SMTC worker failed {MAX_WORKER_RESTARTS} times in a row; giving up until the process restarts"
                    );
                    break;
                }
                let worker_heartbeat = supervisor_heartbeat.clone();
                let worker_generation = supervisor_generation.clone();
                let worker_shutdown = supervisor_shutdown.clone();
                let my_generation = supervisor_generation.fetch_add(1, Ordering::SeqCst) + 1;
                let event_tx_worker = event_tx.clone();
                let listener_config_worker = listener_config.clone();
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
                        )
                        .run();
                    });
                let Ok(worker) = worker else {
                    warn!("could not start the SMTC worker; retrying in 5s");
                    sleep_interruptible(Duration::from_secs(5), &supervisor_shutdown);
                    continue;
                };
                let mut stalled = false;
                while !worker.is_finished() {
                    if supervisor_shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
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
    let overlay_hwnd = overlay::create_window(config.clone(), overlay_queue.clone(), overlay_wake.clone())?;
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
        forwarder_shutdown,
    );

    let message_result = message_loop();

    // Stop the producers before destroying the windows: the forwarder must
    // not post to an HWND that teardown is about to free. The supervisor
    // exits within ~200ms of the flag; a stalled worker is left for process
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
    receiver: mpsc::Receiver<MediaEvent>,
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
                let event = match receiver.recv_timeout(Duration::from_millis(200)) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                push_and_wake(
                    &main_queue,
                    &main_wake,
                    event.clone(),
                    HWND(main_raw as *mut c_void),
                    "main window",
                );
                // Rejected sessions are history-only: the overlay never renders
                // them, so they must not wake the pill or occupy its queue.
                if !matches!(event, MediaEvent::SessionRejected { .. }) {
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

/// Pushes one event into a window's queue and posts `MEDIA_EVENT_MSG` only
/// when no wake message is already in flight (`wake` was clear). On a failed
/// post the flag is cleared and the event removed, so the next push retries
/// instead of waiting on a message that never arrived. On a poisoned queue
/// the event is dropped and the wake flag is left untouched.
fn push_and_wake(queue: &EventQueue, wake: &AtomicBool, event: MediaEvent, hwnd: HWND, name: &str) {
    // A poisoned queue is unusable, so the event cannot be delivered. Do not
    // arm the wake flag: the window would drain nothing and the flag would
    // stay set until the next successful push.
    let Ok(mut q) = queue.lock() else {
        warn!("the {name} event queue is poisoned; dropping the event");
        return;
    };
    q.push_back(event);
    if !wake.swap(true, Ordering::SeqCst)
        && unsafe { PostMessageW(hwnd, MEDIA_EVENT_MSG, WPARAM(0), LPARAM(0)) }.is_err()
    {
        warn!("posting the media event to the {name} failed; dropping its queue copy");
        wake.store(false, Ordering::SeqCst);
        q.pop_back();
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
