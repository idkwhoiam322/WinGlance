use crate::winapi::delete_object;
use crate::winutil::wide;
use log::{debug, warn};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::Duration;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, GetDIBits, HBITMAP, HDC,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, IBindCtx};
use windows::Win32::UI::Shell::{IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY};
use windows::core::{Interface, PCWSTR};

/// Time budget for one app-icon extraction. The shell calls
/// (`SHCreateItemFromParsingName` + `IShellItemImageFactory::GetImage`) can
/// block indefinitely on a broken shell extension; running them inline on the
/// SMTC worker would stall the whole listener until the supervisor's watchdog
/// restarts it. Extraction runs on a single persistent worker thread and a
/// call is abandoned past this budget. Consecutive expired budgets trip the
/// circuit breaker (see `ICON_WORKER_STRIKES`/`ICON_WORKER_TRIPPED`): the
/// worker is presumed stuck in a hung shell call, and every later request
/// would only pile into the queue and time out, so submissions stop until
/// the app restarts.
const ICON_EXTRACT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Circuit breaker: once jobs' budgets expire repeatedly in a row, the worker
/// may be occupied by a hung shell call indefinitely. Every later request
/// would wait the full timeout and then fail, so the breaker stops further
/// submissions (the SMTC worker keeps processing media events; icons simply
/// stay missing for the session). Reset only by restarting the app. A single
/// timeout does NOT trip it — one slow extraction (a cold disk, a network
/// shell extension that eventually answers) must not disable every icon for
/// the session; two consecutive timeouts, with no success in between, are
/// the hang signature.
const ICON_BREAKER_STRIKES: u32 = 2;
static ICON_WORKER_TRIPPED: AtomicBool = AtomicBool::new(false);
static ICON_WORKER_STRIKES: AtomicU32 = AtomicU32::new(0);

/// Cap of the icon worker's job queue. When a submission arrives at a full
/// queue, the OLDEST job is evicted (answered with no icon) in favor of the
/// newest: under churn, the app the user just foregrounded is the one whose
/// icon matters. The SMTC worker never blocks on a full icon queue.
const ICON_QUEUE_CAP: usize = 16;

/// One icon-extraction request. The caller waits on `reply` for the result
/// (up to `ICON_EXTRACT_TIMEOUT`); when the worker is stuck in a hung shell
/// call, the caller's timeout drops the receiver and the worker's later
/// send is a harmless no-op.
struct IconJob {
    aumid: String,
    size: usize,
    reply: mpsc::Sender<IconOutcome>,
}

/// The result of one icon job, with who answered it. Only a worker reply is
/// a liveness proof: an eviction answer is synthesized by another submitter
/// thread under the queue lock and says nothing about whether the worker can
/// still drain jobs, so the circuit breaker's strike counter ignores it.
struct IconOutcome {
    icon: Option<Vec<u8>>,
    from_worker: bool,
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
        pm.push(premultiply_channel(b, a));
        pm.push(premultiply_channel(g, a));
        pm.push(premultiply_channel(r, a));
        pm.push(a);
    }
    Some(pm)
}

/// Premultiplies one color channel by the alpha, rounding half-up: the
/// integer form of `round(channel * alpha / 255.0)`. The exhaustive
/// equivalence test pins the two forms to the same output on every
/// (channel, alpha) pair.
fn premultiply_channel(channel: u8, alpha: u8) -> u8 {
    ((channel as u32 * alpha as u32 + 127) / 255) as u8
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
            let _ = delete_object(hbitmap);
        }
        return None;
    }
    // GetDIBits requires the bitmap NOT to be selected into a device context
    // (Microsoft's documented contract); the DC merely supplies the format.
    let result = hbitmap_to_bgra_premul(hdc, hbitmap, size);
    unsafe {
        let _ = delete_object(hbitmap);
        let _ = DeleteDC(hdc);
    }
    result
}

fn try_shell_item(item: &IShellItem, size: usize) -> Option<Vec<u8>> {
    let factory: IShellItemImageFactory = item.cast().ok()?;
    extract_from_factory(&factory, size)
}

fn try_parsing_name(path: &str, size: usize) -> Option<Vec<u8>> {
    let wide_path = wide(path);
    let pcwstr = PCWSTR(wide_path.as_ptr());
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

/// Bounded escaped preview for untrusted strings in log output, via the
/// shared `winutil::log_preview` helper (with the omission note appended), so
/// the log line is independent of the raw input length.
const ICON_LOG_PREVIEW: usize = 128;

fn log_preview(value: &str) -> String {
    let (preview, omitted) = crate::winutil::log_preview(value, ICON_LOG_PREVIEW);
    if omitted > 0 {
        let mut out = preview;
        use std::fmt::Write;
        let _ = write!(out, " (+{omitted} omitted)");
        out
    } else {
        preview
    }
}

/// Accepts only AUMIDs that match the Windows app-user-model grammar: 1-128
/// ASCII characters from the legal shell-identifier set. Everything else —
/// UNC/device/drive paths, URLs, traversal, control characters, whitespace —
/// is rejected so a hostile `source_app` can never become an arbitrary
/// `SHCreateItemFromParsingName` argument. `shell:AppsFolder\` is the
/// only parsing-name form a validated AUMID is ever combined with.
fn valid_aumid(value: &str) -> bool {
    let len = value.chars().count();
    (1..=128).contains(&len)
        && value.is_ascii()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'!'))
}

/// The shared icon job queue: bounded, drop-oldest on submission. The worker
/// waits on the condition variable; a submit that finds the queue full
/// evicts the oldest job (answering its caller immediately with no icon) so
/// the newest request survives.
struct IconQueue {
    jobs: Mutex<VecDeque<IconJob>>,
    signal: Condvar,
}

fn icon_queue() -> &'static IconQueue {
    static QUEUE: OnceLock<IconQueue> = OnceLock::new();
    QUEUE.get_or_init(|| IconQueue {
        jobs: Mutex::new(VecDeque::new()),
        signal: Condvar::new(),
    })
}

/// Starts the single icon worker thread lazily on first use. All extraction
/// in the process funnels through this one thread. A failed
/// spawn caches `None` so it is only attempted (and logged) once.
fn icon_worker_started() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let queue = icon_queue();
        match thread::Builder::new()
            .name("WinGlance-icon".to_string())
            .stack_size(2 * 1024 * 1024)
            .spawn(move || icon_worker(queue))
        {
            Ok(_) => true,
            Err(error) => {
                warn!("could not start the icon-extraction worker: {error}");
                false
            }
        }
    })
}

/// The icon worker's main loop: one COM apartment for the thread's whole
/// lifetime (initialized once — the thread itself lives for the process, so
/// the OS reclaims the apartment at exit), one job at a time. A panic inside
/// a shell call must not take down the permanent worker: it is caught,
/// logged, and the job answered with no icon so the caller can continue.
fn icon_worker(queue: &IconQueue) {
    // A fresh thread always gets a fresh apartment; the result is still
    // checked so a failed init degrades to a no-icon worker instead of a
    // crashing one.
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    if !initialized {
        warn!("icon worker could not initialize COM; no app icons will be extracted");
    }
    loop {
        let job = {
            let mut jobs = queue.jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                match jobs.pop_front() {
                    Some(job) => break job,
                    None => {
                        jobs = queue.signal.wait(jobs).unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                }
            }
        };
        let result = std::panic::catch_unwind(|| {
            if !initialized {
                return None;
            }
            // An untrusted AUMID never reaches Shell parsing. Invalid
            // IDs are skipped before any `SHCreateItemFromParsingName` call,
            // so a malformed ID costs no worker time and no raw parsing-name
            // fallback exists (a path-shaped AUMID is simply not trusted).
            if !valid_aumid(&job.aumid) {
                debug!(
                    "app-icon skipped | reason=invalid-aumid | aumid={}",
                    log_preview(&job.aumid)
                );
                return None;
            }
            extract_from_aumid(&job.aumid, job.size)
        });
        match result {
            Ok(pixels) => {
                let _ = job.reply.send(IconOutcome {
                    icon: pixels,
                    from_worker: true,
                });
            }
            Err(_) => {
                warn!(
                    "app-icon extraction panicked for {}; continuing",
                    log_preview(&job.aumid)
                );
                let _ = job.reply.send(IconOutcome {
                    icon: None,
                    from_worker: true,
                });
            }
        }
    }
}

pub(crate) fn extract_app_icon(aumid: &str, target_size: usize) -> Option<Vec<u8>> {
    // A tripped breaker means the worker is (likely) stuck in a hung shell
    // call: skip submitting — the job would only time out anyway, and the
    // queue must not pile up behind a worker that cannot drain it.
    if ICON_WORKER_TRIPPED.load(Ordering::SeqCst) {
        return None;
    }
    if !icon_worker_started() {
        return None;
    }
    let size = target_size.clamp(8, 256);
    let (reply_tx, reply_rx) = mpsc::channel();
    let job = IconJob {
        aumid: aumid.to_string(),
        size,
        reply: reply_tx,
    };
    {
        let queue = icon_queue();
        let mut jobs = queue.jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Drop-oldest: under a burst the newest request (the app the user
        // just foregrounded) is the one worth keeping. The evicted job's
        // caller is answered immediately with no icon instead of waiting
        // out its timeout behind a queue that can never drain it. The
        // answer is marked as an eviction: it says nothing about worker
        // health, so the breaker's strike counter must ignore it.
        if jobs.len() >= ICON_QUEUE_CAP
            && let Some(evicted) = jobs.pop_front()
        {
            debug!(
                "icon-extraction queue full; dropped the oldest job for {}",
                log_preview(&evicted.aumid)
            );
            let _ = evicted.reply.send(IconOutcome {
                icon: None,
                from_worker: false,
            });
        }
        jobs.push_back(job);
        queue.signal.notify_one();
    }
    match reply_rx.recv_timeout(ICON_EXTRACT_TIMEOUT) {
        Ok(outcome) => {
            // A worker reply — icon or a healthy no-icon answer — is proof
            // the worker is alive: a single slow extraction must not
            // accumulate toward the breaker. An eviction answer proves
            // nothing (another submitter synthesized it while the worker
            // may be hung), so it neither resets nor adds a strike; the
            // caller simply goes without an icon.
            if outcome.from_worker {
                ICON_WORKER_STRIKES.store(0, Ordering::SeqCst);
            }
            outcome.icon
        }
        Err(_) => {
            let strikes = ICON_WORKER_STRIKES.fetch_add(1, Ordering::SeqCst) + 1;
            if strikes >= ICON_BREAKER_STRIKES
                && ICON_WORKER_TRIPPED
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                warn!(
                    "app-icon extraction timed out for {} ({strikes} consecutive timeouts); the worker may be hung — no further icons will be requested this session",
                    log_preview(aumid)
                );
            } else {
                warn!(
                    "app-icon extraction timed out for {} (timeout {strikes} of {ICON_BREAKER_STRIKES})",
                    log_preview(aumid)
                );
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_aumid_accepts_packaged_and_unpackaged_grammar() {
        // Packaged AUMIDs carry the package-family suffix plus the `!` splitter;
        // unpackaged apps use a plain dotted identifier. Both are 1-128 ASCII
        // shell-identifier characters and must pass the gate.
        for id in [
            "SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify",
            "Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic",
            "Obsidian.Obsidian",
            "Google.Chrome",
            "a",
            "a".repeat(128).as_str(),
        ] {
            assert!(valid_aumid(id), "{id:?} must be a valid AUMID");
        }
    }

    #[test]
    fn valid_aumid_rejects_paths_urls_and_untrusted_shape() {
        // None of these may ever reach a Shell parsing call: every one is a
        // parsing-name escape hatch, not an app identifier.
        for id in [
            r"\\server\share\app.exe",       // UNC
            r"C:\Program Files\App\App.exe", // drive-absolute
            r"\\.\pipe\name",                // device namespace
            "https://evil.example/x",        // URL/SSRF-ish
            r"..\..\secret",                 // traversal
            "Spot\u{0}ify.AB",               // control character
            "Google Chrome",                 // whitespace
            "",                              // empty
            "a".repeat(129).as_str(),        // over the 128-char cap
        ] {
            assert!(!valid_aumid(id), "{id:?} must be rejected");
        }
    }

    /// Deterministic xorshift64 for the fuzz sweeps below: the same seed
    /// always yields the same sequence, so a failure reproduces exactly and
    /// the tests never depend on ambient randomness.
    struct FuzzRng(u64);

    impl FuzzRng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn pick(&mut self, alphabet: &[char]) -> char {
            alphabet[(self.next() as usize) % alphabet.len()]
        }
    }

    #[test]
    fn valid_aumid_matches_the_grammar_oracle_on_fuzzed_inputs() {
        // Fuzz-style sweep against an independently written reference for the
        // same grammar (1-128 ASCII shell-identifier characters). Every
        // generated string — path delimiters, controls, whitespace,
        // non-ASCII, traversal shapes, and boundary lengths — must agree
        // with the oracle, so a boundary slip in either formulation cannot
        // hide. This is the security-critical property: the gate is exactly
        // the identifier grammar, and nothing else ever reaches a Shell
        // parsing-name.
        fn oracle(id: &str) -> bool {
            let len = id.chars().count();
            (1..=128).contains(&len)
                && id.is_ascii()
                && id
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-' | b'!'))
        }
        const HOSTILE: &[char] = &[
            // Allowed shell-identifier characters.
            'a', 'z', 'A', 'Z', '0', '9', '.', '_', '-', '!',
            // Every non-identifier ASCII shape an AUMID might smuggle.
            '/', '\\', ':', '?', '*', '"', '<', '>', '|', '#', '%', '~', '(', ')', ' ', '\t', '\n',
            // Controls and bidi commands.
            '\u{0}', '\u{1F}', '\u{7F}', '\u{85}', '\u{202E}', '\u{2066}',
            // Non-ASCII that looks identifier-like.
            'é', '你', '🎵', 'א', '\u{200D}', '\u{301}',
        ];
        let mut rng = FuzzRng(0xA1D5_0B17_0B17_0B17);
        for _ in 0..2000 {
            // Spans empty, in-range, and over-the-128-cap lengths.
            let len = (rng.next() % 200) as usize;
            let id: String = (0..len).map(|_| rng.pick(HOSTILE)).collect();
            assert_eq!(valid_aumid(&id), oracle(&id), "grammar oracle mismatch for {id:?}");
        }
    }

    #[test]
    fn valid_aumid_rejects_every_non_identifier_ascii_character() {
        // Exhaustive over the whole ASCII range, one character at a time:
        // exactly the alphanumerics plus . _ - ! may pass. Everything else —
        // punctuation, delimiters, whitespace, controls — must be rejected,
        // so no single-character escape hatch exists.
        let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'!');
        for b in 0u8..=127 {
            let s = (b as char).to_string();
            assert_eq!(
                valid_aumid(&s),
                allowed(b),
                "byte {b:#04x} ({:?}) misclassified",
                b as char
            );
        }
        // Non-ASCII characters are outside the grammar however identifier-like
        // they look: letters, bidi commands, emoji, ZWJ, combining marks.
        for c in ['é', 'א', '你', '🎵', '\u{202E}', '\u{2066}', '\u{200D}', '\u{301}'] {
            assert!(!valid_aumid(&c.to_string()), "{c:?} must be rejected");
        }
    }

    #[test]
    fn valid_aumid_enforces_the_length_bounds_under_fuzz() {
        // The 1..=128 bound holds under random lengths built purely from
        // allowed characters: shorter and in-range strings pass, empty and
        // anything past 128 fail, regardless of how the length was reached
        // (single char, packed suffix, exact cap).
        let mut rng = FuzzRng(0x5EED_0000_0000_5EED);
        const ALLOWED: &[char] = &['a', 'Z', '0', '.', '_', '-', '!', 'b', 'Y', '1', '9'];
        for _ in 0..2000 {
            let len = (rng.next() % 200) as usize;
            let id: String = (0..len).map(|_| rng.pick(ALLOWED)).collect();
            let expected = (1..=128).contains(&len);
            assert_eq!(valid_aumid(&id), expected, "length bound mismatch at {len} for {id:?}");
        }
    }

    #[test]
    fn premultiply_channel_matches_the_float_formula_exhaustively() {
        // Every (channel, alpha) pair must round exactly like the former
        // float path `(c * a / 255.0).round()`, so switching the icon
        // pipeline to integer math cannot shift a single pixel. The two
        // formulas agree because (c*a + 127)/255 and round(c*a/255) split
        // at the same remainder: c*a mod 255 <= 127 rounds down in both,
        // >= 128 rounds up in both.
        for channel in 0..=255u8 {
            for alpha in 0..=255u8 {
                let expected = (channel as f32 * alpha as f32 / 255.0).round() as u8;
                assert_eq!(
                    premultiply_channel(channel, alpha),
                    expected,
                    "channel {channel} alpha {alpha}"
                );
            }
        }
        // Endpoint sanity: opaque is the identity, transparent is black.
        assert_eq!(premultiply_channel(200, 255), 200);
        assert_eq!(premultiply_channel(200, 0), 0);
        assert_eq!(premultiply_channel(0, 128), 0);
    }
}
