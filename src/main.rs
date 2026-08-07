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

use crate::config::Config;
use anyhow::Result;
use log::{error, info, warn};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_ALWAYS, WriteFile,
};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS, RtlCaptureStackBackTrace,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
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
                FILE_APPEND_DATA.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                HANDLE::default(),
            )
        };
        if let Ok(handle) = handle {
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
/// panic looks like the app "stopped running randomly".
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
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("crash.log"))
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(message.as_bytes())
            });
    }));
}

/// Acquires a named mutex owned by the process. When another instance already
/// holds it, CreateMutexW succeeds with ERROR_ALREADY_EXISTS, and the app
/// exits without touching the existing instance's windows.
fn is_already_running() -> bool {
    unsafe {
        let name = wide("WinGlanceSingleInstance");
        let _ = CreateMutexW(None, true, PCWSTR(name.as_ptr())).ok();
        GetLastError() == ERROR_ALREADY_EXISTS
    }
}

fn main() -> Result<()> {
    // Logging initializes before the config loads: a corrupted config.toml now
    // falls back to defaults, and that fallback must be diagnosable through
    // the log file.
    logging::init_logging(&config::Config::default().logs_dir());
    let config = config::Config::load()?;
    install_crash_handler(&config.logs_dir());
    install_panic_hook(&config.logs_dir());

    // Only one instance may run at a time; the mutex lives for the process
    // lifetime and is released automatically when the process exits.
    if is_already_running() {
        warn!("another instance of WinGlance is already running; exiting");
        return Ok(());
    }

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
    // Supervisor: runs the SMTC worker and restarts it when it stalls (a WinRT
    // call can hang under heavy session churn, which would otherwise silently
    // stop all events and pills). The hung worker thread is leaked; a fresh
    // worker with its own manager takes over. Threads get explicit smaller
    // stacks (Rust defaults to 2 MB reserve each) — the supervisor and the
    // event forwarder only sleep and forward, and the worker's WinRT calls
    // stay well under 1 MB.
    thread::Builder::new()
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
                let worker_heartbeat = supervisor_heartbeat.clone();
                let event_tx_worker = event_tx.clone();
                let listener_config_worker = listener_config.clone();
                let worker_started = Instant::now();
                let worker = thread::Builder::new()
                    .name("WinGlance-smtc-worker".to_string())
                    .stack_size(1024 * 1024)
                    .spawn(move || {
                        let _ =
                            smtc::SmtcListener::new(event_tx_worker, listener_config_worker, worker_heartbeat).run();
                    });
                let Ok(worker) = worker else {
                    warn!("could not start the SMTC worker; retrying in 5s");
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                };
                let mut stalled = false;
                while !worker.is_finished() {
                    std::thread::sleep(Duration::from_secs(2));
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
                if stalled {
                    // Do not join: the worker may be blocked inside COM forever.
                    consecutive_restarts += 1;
                    let delay = worker_restart_delay(consecutive_restarts);
                    error!("SMTC worker stalled; restarting it in {}s", delay.as_secs());
                    std::thread::sleep(delay);
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
                std::thread::sleep(delay);
            }
        })?;

    let main_queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
    let overlay_queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
    let overlay_hwnd = overlay::create_window(config.clone(), overlay_queue.clone())?;
    let main_hwnd = main_window::create_window(shared_config.clone(), main_queue.clone(), overlay_hwnd)?;

    spawn_event_forwarder(main_hwnd, overlay_hwnd, main_queue, overlay_queue, event_rx);

    let message_result = message_loop();

    unsafe {
        let _ = DestroyWindow(overlay_hwnd);
        let _ = DestroyWindow(main_hwnd);
    }
    message_result
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
fn spawn_event_forwarder(
    main_hwnd: HWND,
    overlay_hwnd: HWND,
    main_queue: EventQueue,
    overlay_queue: EventQueue,
    receiver: mpsc::Receiver<MediaEvent>,
) {
    let main_raw = main_hwnd.0 as isize;
    let overlay_raw = overlay_hwnd.0 as isize;
    thread::Builder::new()
        .name("WinGlance-events".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            while let Ok(event) = receiver.recv() {
                if let Ok(mut queue) = main_queue.lock() {
                    queue.push_back(event.clone());
                }
                if let Ok(mut queue) = overlay_queue.lock() {
                    queue.push_back(event);
                }
                if unsafe { PostMessageW(HWND(main_raw as *mut c_void), MEDIA_EVENT_MSG, WPARAM(0), LPARAM(0)) }
                    .is_err()
                {
                    warn!("posting the media event to the main window failed; dropping its queue copy");
                    // The forwarder is the only pusher and the window drains
                    // under the same lock, so pop_back removes exactly the
                    // event posted above — or is a no-op when the window
                    // already drained it, meaning it was delivered after all.
                    if let Ok(mut queue) = main_queue.lock() {
                        queue.pop_back();
                    }
                }
                if unsafe { PostMessageW(HWND(overlay_raw as *mut c_void), MEDIA_EVENT_MSG, WPARAM(0), LPARAM(0)) }
                    .is_err()
                {
                    warn!("posting the media event to the overlay failed; dropping its queue copy");
                    if let Ok(mut queue) = overlay_queue.lock() {
                        queue.pop_back();
                    }
                }
            }
        })
        .expect("event forwarder thread should start");
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
