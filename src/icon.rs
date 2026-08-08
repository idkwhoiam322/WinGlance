use log::warn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::Duration;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits,
    HBITMAP, HDC,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, IBindCtx};
use windows::Win32::UI::Shell::{IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY};
use windows::core::{Interface, PCWSTR};

/// Time budget for one app-icon extraction. The shell calls
/// (`SHCreateItemFromParsingName` + `IShellItemImageFactory::GetImage`) can
/// block indefinitely on a broken shell extension; running them inline on the
/// SMTC worker would stall the whole listener until the supervisor's watchdog
/// restarts it. Extraction runs on a single persistent worker thread and a
/// call is abandoned past this budget. A call that *exceeds* the budget trips
/// the circuit breaker (see `ICON_WORKER_TRIPPED`): the worker is presumed
/// stuck in a hung shell call, and every later request would only pile into
/// the queue and time out, so submissions stop until the app restarts.
const ICON_EXTRACT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Circuit breaker: once a job's budget expires, the worker may be occupied
/// by a hung shell call indefinitely. Every later request would wait the full
/// timeout and then fail, so the breaker stops further submissions (the SMTC
/// worker keeps processing media events; icons simply stay missing for the
/// session). Reset only by restarting the app.
static ICON_WORKER_TRIPPED: AtomicBool = AtomicBool::new(false);

/// Cap of the icon worker's job queue. Requests beyond it are dropped with a
/// log line (the caller shows the pill without an icon); the SMTC worker
/// never blocks on a full icon queue.
const ICON_QUEUE_CAP: usize = 16;

/// One icon-extraction request. The caller waits on `reply` for the result
/// (up to `ICON_EXTRACT_TIMEOUT`); when the worker is stuck in a hung shell
/// call, the caller's timeout drops the receiver and the worker's later
/// send is a harmless no-op.
struct IconJob {
    aumid: String,
    size: usize,
    reply: mpsc::Sender<Option<Vec<u8>>>,
}

fn hbitmap_to_bgra_premul(hdc: HDC, bitmap: HBITMAP, size: usize) -> Option<Vec<u8>> {
    let total_bytes = size * size * 4;
    let mut buf = vec![0u8; total_bytes];

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let result = unsafe {
        GetDIBits(
            hdc,
            bitmap,
            0,
            size as u32,
            Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    if result == 0 {
        return None;
    }

    let mut pm = Vec::with_capacity(total_bytes);
    for px in buf.chunks_exact(4) {
        let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
        let a = a as f32 / 255.0;
        pm.push((b as f32 * a).round() as u8);
        pm.push((g as f32 * a).round() as u8);
        pm.push((r as f32 * a).round() as u8);
        pm.push(px[3]);
    }
    Some(pm)
}

fn extract_from_factory(factory: &IShellItemImageFactory, size: usize) -> Option<Vec<u8>> {
    let size_pt = windows::Win32::Foundation::SIZE {
        cx: size as i32,
        cy: size as i32,
    };
    let hbitmap = unsafe { factory.GetImage(size_pt, SIIGBF_ICONONLY).ok() }?;
    let hdc = unsafe { CreateCompatibleDC(None) };
    if hdc.0.is_null() {
        unsafe {
            let _ = DeleteObject(hbitmap);
        }
        return None;
    }
    // GetDIBits requires the bitmap NOT to be selected into a device context
    // (Microsoft's documented contract); the DC merely supplies the format.
    let result = hbitmap_to_bgra_premul(hdc, hbitmap, size);
    unsafe {
        let _ = DeleteObject(hbitmap);
        let _ = DeleteDC(hdc);
    }
    result
}

fn try_shell_item(item: &IShellItem, size: usize) -> Option<Vec<u8>> {
    let factory: IShellItemImageFactory = item.cast().ok()?;
    extract_from_factory(&factory, size)
}

fn try_parsing_name(path: &str, size: usize) -> Option<Vec<u8>> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let pcwstr = PCWSTR(wide.as_ptr());
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(pcwstr, Option::<&IBindCtx>::None).ok() }?;
    let result = try_shell_item(&item, size);
    // The generated `IShellItem` wraps an `IUnknown` field that releases the
    // owned reference exactly once on drop — no manual Release is needed (and
    // an extra one would be a double-release).
    drop(item);
    result
}

fn extract_from_aumid(aumid: &str, size: usize) -> Option<Vec<u8>> {
    let apps_path = format!("shell:AppsFolder\\{}", aumid);
    try_parsing_name(&apps_path, size)
}

/// The single icon worker's job sender, started lazily on first use. All
/// extraction in the process funnels through this one thread. A failed
/// spawn caches `None` so it is only attempted (and logged) once.
fn icon_sender() -> Option<mpsc::SyncSender<IconJob>> {
    static SENDER: OnceLock<Option<mpsc::SyncSender<IconJob>>> = OnceLock::new();
    SENDER
        .get_or_init(|| {
            let (job_tx, job_rx) = mpsc::sync_channel::<IconJob>(ICON_QUEUE_CAP);
            match thread::Builder::new()
                .name("WinGlance-icon".to_string())
                .stack_size(512 * 1024)
                .spawn(move || icon_worker(job_rx))
            {
                Ok(_) => Some(job_tx),
                Err(error) => {
                    warn!("could not start the icon-extraction worker: {error}");
                    None
                }
            }
        })
        .clone()
}

/// The icon worker's main loop: one COM apartment for the thread's whole
/// lifetime (initialized once, uninitialized once on exit — never per
/// request), one job at a time. A panic inside a shell call must not take
/// down the permanent worker: it is caught, logged, and the job answered
/// with no icon so the caller can continue.
fn icon_worker(job_rx: mpsc::Receiver<IconJob>) {
    // A fresh thread always gets a fresh apartment; the result is still
    // checked so the single CoUninitialize below is only paired with a
    // successful init.
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    if !initialized {
        warn!("icon worker could not initialize COM; no app icons will be extracted");
    }
    loop {
        let Ok(job) = job_rx.recv() else {
            break; // every sender is gone; nothing more can be requested
        };
        let result = std::panic::catch_unwind(|| {
            if !initialized {
                return None;
            }
            if let Some(pixels) = extract_from_aumid(&job.aumid, job.size) {
                Some(pixels)
            } else if job.aumid.contains('\\') || job.aumid.contains("/.") {
                try_parsing_name(&job.aumid, job.size)
            } else {
                None
            }
        });
        match result {
            Ok(pixels) => {
                let _ = job.reply.send(pixels);
            }
            Err(_) => {
                warn!("app-icon extraction panicked for {}; continuing", job.aumid);
                let _ = job.reply.send(None);
            }
        }
    }
    if initialized {
        unsafe { CoUninitialize() };
    }
}

pub(crate) fn extract_app_icon(aumid: &str, target_size: usize) -> Option<Vec<u8>> {
    // A tripped breaker means the worker is (likely) stuck in a hung shell
    // call: skip submitting — the job would only time out anyway, and the
    // queue must not pile up behind a worker that cannot drain it.
    if ICON_WORKER_TRIPPED.load(Ordering::SeqCst) {
        return None;
    }
    let size = target_size.clamp(8, 256);
    let Some(sender) = icon_sender() else {
        warn!("could not start the icon-extraction worker");
        return None;
    };
    let (reply_tx, reply_rx) = mpsc::channel();
    let job = IconJob {
        aumid: aumid.to_string(),
        size,
        reply: reply_tx,
    };
    if sender.try_send(job).is_err() {
        warn!("icon-extraction queue is full; skipping the icon for {aumid}");
        return None;
    }
    match reply_rx.recv_timeout(ICON_EXTRACT_TIMEOUT) {
        Ok(result) => result,
        Err(_) => {
            if ICON_WORKER_TRIPPED
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                warn!(
                    "app-icon extraction timed out for {aumid}; the worker may be hung — no further icons will be requested this session"
                );
            }
            None
        }
    }
}
