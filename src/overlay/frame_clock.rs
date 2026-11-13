//! The overlay's animation frame clock.
//!
//! The pill was driven by a `CreateTimerQueueTimer` at an integer-millisecond
//! period. The system timer queue is serviced at the platform clock tick
//! (15.625 ms on a default Windows configuration), so a requested 16 ms period
//! actually delivered 13.96–30.24 ms intervals on the development machine: the
//! moving edge advances by uneven distances and periodically skips a frame.
//! Two rounding losses compound the problem: 60 Hz is 16.666… ms, not 16 ms,
//! and the queue cannot express a period finer than one millisecond.
//!
//! This clock waits on a high-resolution waitable timer
//! (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`, Windows 10 1803+) on a dedicated
//! thread and re-arms it against an absolute schedule, so the requested period
//! is honoured without drift accumulation. The thread only posts the existing
//! `TIMER_ANIMATION_MSG`; every piece of window state stays on the UI thread,
//! exactly as the timer-queue callback did. It is a kernel wait, not a
//! busy-wait, and it does not change any global timer-resolution setting.

use crate::winapi::post_message;
use log::error;
use std::ffi::c_void;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_EVENT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE, SetEvent, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForMultipleObjects,
};
use windows::core::PCWSTR;

use super::TIMER_ANIMATION_MSG;

/// Shortest and longest animation frame period. The fast end keeps a
/// pathological refresh report from spinning the UI thread; the slow end
/// keeps a nonsense display rate from stalling the dismiss path. The clock
/// itself also drives the coarser static (250 ms) and hidden-watchdog (1 s)
/// cadences, so only the animation period is capped.
const MIN_PERIOD: Duration = Duration::from_millis(1);
const MAX_PERIOD: Duration = Duration::from_millis(100);

/// The animation tick period for a display frame period and the configured
/// `max_tick_hz` cap.
///
/// The cap's period is computed in nanoseconds rather than whole
/// milliseconds: 60 Hz is 16.666… ms, and truncating it to 16 ms makes the
/// pill produce 62.5 updates/s against a 60 Hz composition — a beat that
/// reads as periodic judder even when every timer callback lands on time. A
/// display slower than the cap keeps its own period; a faster one is capped.
pub(super) fn frame_period(display: Duration, max_tick_hz: Option<u32>) -> Duration {
    let hz = max_tick_hz.unwrap_or(60).clamp(60, 1000);
    let cap = Duration::from_nanos(1_000_000_000 / hz as u64).max(MIN_PERIOD);
    display.max(cap).clamp(MIN_PERIOD, MAX_PERIOD)
}

/// Whether a re-sampled animation period differs enough to justify rebuilding
/// the clock. DWM's live compose rate can wobble by microseconds on a VRR
/// display; tearing down and respawning the clock thread for a sub-percent
/// change would cost a frame for no visible benefit.
pub(super) fn period_change_is_significant(current: Duration, next: Duration) -> bool {
    let delta = current.max(next) - current.min(next);
    delta.as_nanos() * 100 >= current.as_nanos().max(1)
}

/// A high-resolution animation clock posting `TIMER_ANIMATION_MSG` once per
/// period. Dropping it wakes and joins the timer thread, so no tick can be
/// posted after the owning window starts tearing down.
pub(super) struct FrameClock {
    stop: HANDLE,
    thread: Option<JoinHandle<()>>,
}

impl FrameClock {
    /// Starts the clock. `None` when the OS cannot create a waitable timer or
    /// the thread cannot be spawned; the caller falls back to `SetTimer`.
    ///
    /// Both kernel objects are created here, before the thread exists: a
    /// failure inside the thread would otherwise leave the caller holding a
    /// live-looking clock with no thread behind it, freezing the pill.
    pub(super) fn start(hwnd: HWND, period: Duration) -> Option<Self> {
        // An auto-reset event that is signalled once, on drop.
        let stop = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.ok()?;
        let Some(timer) = create_timer() else {
            unsafe {
                let _ = CloseHandle(stop);
            }
            return None;
        };
        let stop_raw = stop.0 as isize;
        let timer_raw = timer.0 as isize;
        let hwnd_raw = hwnd.0 as isize;
        // A sub-millisecond wait would spin the thread with no visual gain.
        let period = period.max(MIN_PERIOD);
        let spawned = std::thread::Builder::new()
            .name("winglance-frame-clock".into())
            .spawn(move || run(hwnd_raw, stop_raw, timer_raw, period));
        match spawned {
            Ok(thread) => Some(Self {
                stop,
                thread: Some(thread),
            }),
            Err(_) => {
                unsafe {
                    let _ = CloseHandle(timer);
                    let _ = CloseHandle(stop);
                }
                None
            }
        }
    }
}

impl Drop for FrameClock {
    fn drop(&mut self) {
        // Wakes the thread's wait; the join then guarantees it can no longer
        // post a tick before the stop handle is closed.
        unsafe {
            let _ = SetEvent(self.stop);
        }
        if let Some(thread) = self.thread.take() {
            // A panicked timer thread must not propagate into the UI thread;
            // the clock is gone either way and the caller can rebuild it.
            let _ = thread.join();
        }
        unsafe {
            let _ = CloseHandle(self.stop);
        }
    }
}

/// Creates the high-resolution timer, falling back to a plain waitable timer
/// when the OS rejects the flag (pre-1803). The fallback still uses a 100 ns
/// due time and the same absolute schedule, so it never accumulates drift,
/// though without the flag it is subject to the platform clock tick.
fn create_timer() -> Option<HANDLE> {
    unsafe {
        CreateWaitableTimerExW(
            None,
            PCWSTR::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )
        .ok()
        .or_else(|| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0).ok())
    }
}

/// The timer thread: wait for the next absolute deadline, post one tick, and
/// skip any deadlines the UI thread or the OS missed rather than bursting a
/// backlog (the animation is time-based and catches up on its own).
fn run(hwnd_raw: isize, stop_raw: isize, timer_raw: isize, period: Duration) {
    let hwnd = HWND(hwnd_raw as *mut c_void);
    let stop = HANDLE(stop_raw as *mut c_void);
    let timer = HANDLE(timer_raw as *mut c_void);
    let mut next = Instant::now() + period;
    loop {
        let wait = next.saturating_duration_since(Instant::now());
        // A relative waitable-timer due time is negative and expressed in
        // 100 ns units; round up so a wait never ends early.
        let due = -((wait.as_nanos().div_ceil(100).max(1)) as i64);
        if let Err(error) = unsafe { SetWaitableTimer(timer, &due, 0, None, None, false) } {
            error!("frame clock could not arm its timer: {error}");
            break;
        }
        let handles = [timer, stop];
        let wake = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        if wake != WAIT_OBJECT_0 {
            // The stop event is WAIT_OBJECT_0 + 1. Anything else is a real
            // wait failure; surface it rather than freezing the pill silently.
            if wake != WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
                error!("frame clock wait failed (status {})", wake.0);
            }
            break;
        }
        unsafe {
            let _ = post_message(hwnd, TIMER_ANIMATION_MSG, WPARAM(0), LPARAM(0));
        }
        next += period;
        // Sleep/resume or a modal loop can stall the UI thread past whole
        // periods; missing those deadlines is expected, not an error.
        let now = Instant::now();
        while next <= now {
            next += period;
        }
    }
    unsafe {
        let _ = CloseHandle(timer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame at `hz` as the exact truncated nanosecond period.
    fn period_at(hz: u32) -> Duration {
        Duration::from_nanos(1_000_000_000 / hz as u64)
    }

    #[test]
    fn frame_period_honors_the_display_and_the_cap_band() {
        // The cap must be the exact period, not a truncated millisecond
        // value: 16 ms would be 62.5 fps, and a 144 Hz truncation would beat
        // against the display.
        let period = frame_period(period_at(60), None);
        assert_eq!(period, Duration::from_nanos(16_666_666));
        assert!(
            (1.0 / period.as_secs_f64() - 60.0).abs() < 0.001,
            "the effective rate must be 60 Hz, got {}",
            1.0 / period.as_secs_f64()
        );
        assert_eq!(frame_period(period_at(300), Some(144)), Duration::from_nanos(6_944_444));

        // A display slower than the cap keeps its own period; a faster one
        // is capped; an absent cap means 60 Hz.
        assert_eq!(frame_period(period_at(30), Some(60)), period_at(30));
        assert_eq!(frame_period(period_at(300), Some(60)), period_at(60));
        assert_eq!(frame_period(period_at(144), Some(120)), period_at(120));
        assert_eq!(frame_period(period_at(300), None), period_at(60));

        // Degenerate periods clamp to the supported range.
        assert_eq!(frame_period(Duration::ZERO, Some(1000)), MIN_PERIOD);
        assert_eq!(frame_period(Duration::from_secs(5), Some(60)), MAX_PERIOD);

        // The cap is clamped to the supported band before dividing: below
        // the 60 Hz floor a user value would slow the animation under the
        // documented minimum; above 1000 the period would underflow.
        assert_eq!(frame_period(period_at(300), Some(1)), period_at(60));
        assert_eq!(frame_period(period_at(1000), Some(u32::MAX)), period_at(1000));
    }

    #[test]
    fn only_a_perceptible_period_change_rebuilds_the_clock() {
        let sixty = period_at(60);
        assert!(!period_change_is_significant(sixty, sixty));
        // A VRR wobble below one percent must not respawn the thread.
        assert!(!period_change_is_significant(sixty, sixty + Duration::from_micros(100)));
        assert!(!period_change_is_significant(sixty, sixty - Duration::from_micros(100)));
        // A real display-rate change must.
        assert!(period_change_is_significant(sixty, period_at(30)));
        assert!(period_change_is_significant(sixty, period_at(144)));
    }
}
