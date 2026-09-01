use chrono::Local;
use log::{LevelFilter, Log, Metadata, Record};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Maximum live-log size. Plain launches may truncate the live log once at
/// startup; after startup the file is append-only and logging simply stops at
/// this cap. Earlier diagnostics are never erased to make room for later ones.
/// The in-app restart path preserves the file and continues using its remaining
/// capacity.
const LIVE_LOG_CAP: u64 = 1024 * 1024;

/// Opens the live log for a launch. With `preserve` set (the in-app "Restart
/// app" reload path) an existing file is appended to instead of truncated, so
/// the previous session's lines survive, and a separator line marks the
/// restart boundary. Returns the file and the byte count the size cap must
/// count from: the file's length at open, so the cap bounds the total file
/// and repeated restarts with short sessions cannot grow it without bound.
///
/// The open is the verified-write transaction (`winutil::open_verified_file`):
/// the parent directory is pinned and identity-verified THROUGH the open, the
/// final component is opened with `FILE_FLAG_OPEN_REPARSE_POINT` and rejected
/// when it is a link, and the handle's final path must equal the expected
/// path before the file is truncated or appended — so a pre-created
/// `log-Live.log` symlink, a junction swapped into the logs dir, or a parent
/// swap racing the open can never create, truncate, or append outside the
/// logs directory.
fn open_live_log(live_path: &Path, preserve: bool) -> std::io::Result<(File, u64)> {
    let mut file = crate::winutil::open_verified_file(live_path, /*truncate=*/ !preserve)?;
    if preserve {
        let boundary = format!(
            "===== restarted via the Settings 'Restart app' action at {} =====\n",
            Local::now().to_rfc3339()
        );
        let existing = file.metadata()?.len();
        if live_log_can_append(existing, boundary.len()) {
            // Append only when the complete boundary fits. A partial marker is
            // worse than no marker, and the cap never authorizes overwriting
            // older diagnostics.
            file.seek(SeekFrom::End(0))?;
            file.write_all(boundary.as_bytes())?;
            let _ = file.sync_all();
        }
    }
    let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    Ok((file, written))
}

pub fn init_logging(logs_dir: &Path, preserve: bool) {
    let live_path = logs_dir.join("log-Live.log");
    let files = match open_live_log(&live_path, preserve) {
        Ok((file, written)) => Some(LogFiles { live: file, written }),
        Err(error) => {
            // The whole run is about to go without diagnostics: the
            // eprintln reaches no one in a release build (no console), so
            // record the failure in crash.log — the one channel that does
            // not depend on the live log existing. One bounded line; if
            // even that fails (same environmental cause), it is silently
            // dropped, which changes nothing.
            crate::crash_log_append(
                format!("live log could not be opened ({error}); this run writes no log-Live.log\n").as_bytes(),
            );
            eprintln!("log file open failed ({live_path:?}): {error}");
            None
        }
    };

    let logger = FileLogger {
        files: Mutex::new(files),
        write_failure_last: Mutex::new(None),
    };
    if LOGGER.set(logger).is_ok() && log::set_logger(LOGGER.get().expect("logger initialized")).is_ok() {
        // Debug level: session churn, dedup skips and suppressed states are all
        // logged, which is what makes "why did/didn't a notification fire"
        // answerable from the log file.
        log::set_max_level(LevelFilter::Debug);
    }
    log::info!("logging initialized | live log: {live_path:?}");
}

/// The process-wide logger instance. Held at module level so the restart
/// handoff can reseat the live-log cursor without going through the `log`
/// facade.
static LOGGER: OnceLock<FileLogger> = OnceLock::new();

/// Last time each tagged log line was allowed through, keyed by a
/// caller-chosen static tag. Bounded: when the map reaches its cap, entries
/// older than any caller's interval are dropped first (a steady stream of
/// distinct tags cycles; a flood of one tag reuses its own entry).
static LAST_LOGGED: Mutex<Option<HashMap<&'static str, Instant>>> = Mutex::new(None);
const THROTTLED_KEY_CAP: usize = 64;

/// Returns whether the line tagged `key` may log now: the first call always
/// true, then at most once per `interval`. For failure paths that can fire
/// at animation-tick rate (a persistent render or blit failure would
/// otherwise drown everything else in log-Live.log). The tag classifies,
/// not the message — two different errors sharing a key suppress each other
/// for the interval, which is the accepted trade for bounded output.
pub(crate) fn should_log(key: &'static str, interval: Duration) -> bool {
    let mut guard = LAST_LOGGED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    match map.get(key) {
        Some(last) if now.duration_since(*last) < interval => false,
        _ => {
            if map.len() >= THROTTLED_KEY_CAP {
                map.retain(|_, last| now.duration_since(*last) >= interval);
                if map.len() >= THROTTLED_KEY_CAP {
                    // Pathological key churn: drop everything and start over.
                    map.clear();
                }
            }
            map.insert(key, now);
            true
        }
    }
}

/// Moves the live log's write cursor to EOF: called by the restart
/// handoff right before the old process releases the singleton. The
/// successor has already appended its boundary line to the preserved file;
/// any late write from this process's remaining threads must append after
/// it — this handle's cursor predates those bytes and would otherwise
/// overwrite them. Best-effort: a failed seek leaves the pre-restart
/// behavior (a microsecond-window overlap that is diagnostics-only).
pub fn reseat_live_log_to_eof() {
    if let Some(logger) = LOGGER.get()
        && let Ok(mut files) = logger.files.lock()
        && let Some(files) = files.as_mut()
        && files.live.seek(SeekFrom::End(0)).is_ok()
        && let Ok(meta) = files.live.metadata()
    {
        files.written = meta.len();
    }
}

struct LogFiles {
    live: File,
    /// Bytes counted toward the size cap: the file's length at startup
    /// (zero on truncating launches, the preserved length on reloads) plus
    /// every line written since.
    written: u64,
}

struct FileLogger {
    files: Mutex<Option<LogFiles>>,
    /// Rate-limits the write-failure fallback: when the live log cannot be
    /// written, one crash.log line per window instead of one per log call.
    /// `None` until the first failure.
    write_failure_last: Mutex<Option<Instant>>,
}

/// How often the write-failure fallback reports to crash.log.
const WRITE_FAILURE_REPORT_INTERVAL: Duration = Duration::from_secs(30);

fn live_log_can_append(written: u64, bytes: usize) -> bool {
    u64::try_from(bytes).is_ok_and(|bytes| bytes <= LIVE_LOG_CAP.saturating_sub(written))
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} [{:<5}] {}: {}\n",
            Local::now().to_rfc3339(),
            record.level(),
            record.target(),
            record.args()
        );
        match self.files.lock() {
            Ok(mut guard) => match guard.as_mut() {
                Some(files) => {
                    // The cap is append-only after startup: once a complete
                    // line no longer fits, drop it rather than truncating or
                    // overwriting earlier diagnostics. The counter advances only
                    // after a successful write, so failed writes never consume
                    // capacity that still exists on disk.
                    if live_log_can_append(files.written, line.len()) {
                        if let Err(error) = files.live.write_all(line.as_bytes()) {
                            self.report_write_failure(&error.to_string());
                        } else {
                            files.written += line.len() as u64;
                        }
                    }
                }
                // The live log never opened (the logs directory was unwritable
                // at startup): the whole run is blind. Surface that through
                // crash.log at the same rate the write-failure path reports,
                // instead of dropping every line in total silence.
                None => self.report_write_failure("the live log is not open"),
            },
            // A poisoned lock means a panic while holding it: the line is
            // diagnostics-only and is dropped, but the loss stays visible.
            Err(_) => self.report_write_failure("the live-log lock is poisoned"),
        }
        // Echo to the console in debug builds only: the packaged exe is
        // `windows_subsystem = "windows"` with no console, so every eprint!
        // in release is a wasted WriteFile against a handle nothing reads.
        if cfg!(debug_assertions) {
            eprint!("{line}");
        }
    }

    fn flush(&self) {}
}

impl FileLogger {
    /// Rate-limited crash.log report for a live-log write that could not
    /// land (the file write failed, or the file never opened): one line per
    /// window instead of one per log call, so a persistent disk-full
    /// condition cannot spam crash.log past its own cap. Routed through the
    /// one shared crash-log accounting path (`main::crash_log_append`), so
    /// this writer can never strand the vectored handler's cap counter.
    fn report_write_failure(&self, detail: &str) {
        let report = {
            let mut last = self
                .write_failure_last
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let due = last.is_none_or(|t| t.elapsed() >= WRITE_FAILURE_REPORT_INTERVAL);
            if due {
                *last = Some(Instant::now());
            }
            due
        };
        if report {
            crate::crash_log_append(
                format!("live-log write failed ({detail}); log lines are being dropped\n").as_bytes(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A uniquely-named temporary directory removed on drop, so log tests can
    /// exercise the real filesystem without touching %APPDATA%.
    struct TestDir {
        dir: PathBuf,
    }

    impl TestDir {
        fn new(tag: &str) -> Self {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("winglance-log-{tag}-{}-{stamp}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn plain_launch_truncates_and_counts_from_zero() {
        let guard = TestDir::new("plain-launch");
        let live_path = guard.dir.join("log-Live.log");
        std::fs::write(&live_path, "previous session content\n").unwrap();

        let (file, written) = open_live_log(&live_path, false).unwrap();
        assert_eq!(written, 0, "the cap must count this process's writes only");
        assert_eq!(file.metadata().unwrap().len(), 0, "the file must be truncated");
        // Drop the handle before the guard removes the directory.
        drop(file);
    }

    #[test]
    fn live_log_open_rejects_a_final_component_link_and_leaves_the_target() {
        // A pre-created log-Live.log link must be refused before any
        // truncate/append: the launch fails and the external target stays
        // byte-identical (the final component carries the reparse attribute
        // and is rejected outright by the verified open).
        let guard = TestDir::new("live-log-link");
        let real = guard.dir.join("victim.log");
        let original = b"EXTERNAL TARGET DATA";
        std::fs::write(&real, original).unwrap();
        let live_path = guard.dir.join("log-Live.log");
        if std::os::windows::fs::symlink_file(&real, &live_path).is_err() {
            return;
        }

        assert!(
            open_live_log(&live_path, false).is_err(),
            "a link at the live log name must be rejected"
        );
        assert!(
            open_live_log(&live_path, true).is_err(),
            "even the preserve path must reject a link"
        );
        assert_eq!(
            std::fs::read(&real).unwrap(),
            original,
            "the external target must stay byte-identical"
        );
        assert!(
            std::fs::symlink_metadata(&live_path).unwrap().file_type().is_symlink(),
            "the link entry itself must not be truncated or replaced"
        );
    }

    #[test]
    fn live_log_open_rejects_a_parent_junction_and_creates_nothing() {
        // A junction swapped into the logs directory must reject the launch
        // before any file is created or truncated inside the link target.
        let guard = TestDir::new("live-log-junction");
        let real = guard.dir.join("real");
        std::fs::create_dir_all(real.join("logs")).unwrap();
        let evil = guard.dir.join("evil");
        if std::os::windows::fs::symlink_dir(&real, &evil).is_err() {
            return;
        }
        let live_path = evil.join("logs").join("log-Live.log");

        assert!(open_live_log(&live_path, false).is_err());
        assert!(
            !real.join("logs").join("log-Live.log").exists(),
            "no log file may be created through the junction"
        );
    }

    #[test]
    fn live_log_cap_is_monotonic_and_never_requires_overwrite() {
        assert!(live_log_can_append(0, LIVE_LOG_CAP as usize));
        assert!(!live_log_can_append(1, LIVE_LOG_CAP as usize));
        assert!(live_log_can_append(LIVE_LOG_CAP - 4, 4));
        assert!(!live_log_can_append(LIVE_LOG_CAP - 4, 5));
        assert!(!live_log_can_append(LIVE_LOG_CAP, 1));
    }

    #[test]
    fn preserved_log_without_room_for_boundary_stays_byte_identical() {
        let guard = TestDir::new("reload-full");
        let live_path = guard.dir.join("log-Live.log");
        let original = vec![b'x'; LIVE_LOG_CAP as usize - 1];
        std::fs::write(&live_path, &original).unwrap();
        let (file, written) = open_live_log(&live_path, true).unwrap();
        assert_eq!(written, original.len() as u64);
        drop(file);
        assert_eq!(std::fs::read(&live_path).unwrap(), original);
    }

    #[test]
    fn reload_keeps_prior_content_seeds_the_cap_and_marks_the_boundary() {
        let guard = TestDir::new("reload-keeps");
        let live_path = guard.dir.join("log-Live.log");
        let prior = "first session line\n";
        std::fs::write(&live_path, prior).unwrap();

        let (file, written) = open_live_log(&live_path, true).unwrap();
        assert_eq!(
            written,
            file.metadata().unwrap().len(),
            "the cap must count the preserved bytes, so the file stays within the cap total"
        );
        let content = std::fs::read_to_string(&live_path).unwrap();
        assert!(content.starts_with(prior), "the previous session must survive");
        assert!(
            content.contains("restarted via the Settings 'Restart app' action"),
            "the restart boundary must be visible in the preserved log:\n{content}"
        );
        drop(file);
    }
}
