#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod config;
mod events;
mod logging;
mod main_window;
mod overlay;
mod positioner;
mod smtc;

use anyhow::Result;
use log::{error, info, warn};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW, TranslateMessage,
};

use crate::events::{MEDIA_EVENT_MSG, MediaEvent};
use crate::overlay::EventQueue;

fn main() -> Result<()> {
    let config = config::Config::load()?;
    logging::init_logging(&config.logs_dir());
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
    let listener_config = config.clone();
    thread::Builder::new().name("notch-smtc".to_string()).spawn(move || {
        if let Err(error) = smtc::SmtcListener::new(event_tx, listener_config).run() {
            error!("SMTC listener stopped: {error:#}");
        }
    })?;

    let main_queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
    let overlay_queue: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
    let overlay_hwnd = overlay::create_window(config.clone(), overlay_queue.clone())?;
    let main_hwnd = main_window::create_window(config, main_queue.clone(), overlay_hwnd)?;

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
