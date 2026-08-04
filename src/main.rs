#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod config;
mod events;
mod logging;
mod main_window;
mod overlay;
mod positioner;
mod process_picker;
mod smtc;

use crate::config::Config;
use anyhow::Result;
use log::{error, info, warn};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, WPARAM};
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

static CRASH_LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Diagnostic vectored exception handler: on an access violation it appends the
/// faulting instruction address and a raw backtrace to crash.log (no locks, no
/// allocations that can deadlock), then lets Windows continue with default crash
/// handling.
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
    let mut out = String::from("CRASH access violation\n");
    out += &format!("  ip    = 0x{ip:016x} (rva 0x{:x})\n", ip.wrapping_sub(base));
    out += &format!("  addr  = 0x{addr:016x}\n");
    out += &format!("  base  = 0x{base:016x}\n");
    for (i, f) in frames.iter().take(count as usize).enumerate() {
        out += &format!("  frame[{i}] = 0x{f:016x} (rva 0x{:x})\n", f.wrapping_sub(base));
    }
    if let Some(dir) = CRASH_LOG_DIR.get() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("crash.log"))
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(out.as_bytes())
            });
    }
    0 // EXCEPTION_CONTINUE_SEARCH
}

fn install_crash_handler(logs_dir: &Path) {
    let _ = CRASH_LOG_DIR.set(logs_dir.to_path_buf());
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
        let name = wide("NotchSingleInstance");
        let _ = CreateMutexW(None, true, PCWSTR(name.as_ptr())).ok();
        GetLastError() == ERROR_ALREADY_EXISTS
    }
}

fn main() -> Result<()> {
    let config = config::Config::load()?;
    logging::init_logging(&config.logs_dir());
    install_crash_handler(&config.logs_dir());
    install_panic_hook(&config.logs_dir());

    // Only one instance may run at a time; the mutex lives for the process
    // lifetime and is released automatically when the process exits.
    if is_already_running() {
        warn!("another instance of Notch is already running; exiting");
        return Ok(());
    }

    info!("starting Notch");

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
        .name("notch-smtc".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            loop {
                let worker_heartbeat = supervisor_heartbeat.clone();
                let event_tx_worker = event_tx.clone();
                let listener_config_worker = listener_config.clone();
                let worker = thread::Builder::new()
                    .name("notch-smtc-worker".to_string())
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
                    let last = *supervisor_heartbeat.lock().unwrap();
                    if last.elapsed() > Duration::from_secs(30) {
                        stalled = true;
                        break;
                    }
                }
                if stalled {
                    // Do not join: the worker may be blocked inside COM forever.
                    error!("SMTC worker stalled; restarting it");
                    std::thread::sleep(Duration::from_secs(5));
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
                warn!("SMTC worker exited; restarting it");
                std::thread::sleep(Duration::from_secs(5));
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
        .name("notch-events".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            while let Ok(event) = receiver.recv() {
                let mut posted = true;
                if let Ok(mut queue) = main_queue.lock() {
                    queue.push_back(event.clone());
                }
                if let Ok(mut queue) = overlay_queue.lock() {
                    queue.push_back(event);
                }
                if unsafe { PostMessageW(HWND(main_raw as *mut c_void), MEDIA_EVENT_MSG, WPARAM(0), LPARAM(0)) }
                    .is_err()
                {
                    posted = false;
                }
                if unsafe { PostMessageW(HWND(overlay_raw as *mut c_void), MEDIA_EVENT_MSG, WPARAM(0), LPARAM(0)) }
                    .is_err()
                {
                    posted = false;
                }
                if !posted {
                    break;
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
