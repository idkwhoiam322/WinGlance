use chrono::Local;
use log::{LevelFilter, Log, Metadata, Record};
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// The live log is reset when it exceeds this many bytes counted against it:
/// a churn-heavy session can otherwise write tens of MB of Debug lines to
/// disk. On plain launches the count starts at zero (the file was truncated
/// on open); on the in-app "Restart app" path the file is preserved instead
/// of truncated and the count starts at the existing file length, so the cap
/// still bounds the total file on every launch path.
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
        // The boundary line is appended after the verified open, at the real
        // end of the preserved file (the open leaves the pointer at 0).
        file.seek(SeekFrom::End(0))?;
        file.write_all(
            format!(
                "===== restarted via the Settings 'Restart app' action at {} =====\n",
                Local::now().to_rfc3339()
            )
            .as_bytes(),
        )?;
    }
    let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    Ok((file, written))
}

pub fn init_logging(logs_dir: &Path, preserve: bool) {
    let _ = fs::create_dir_all(logs_dir);
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
            let _ = crate::winutil::append_verified_bounded(
                &logs_dir.join("crash.log"),
                format!("live log could not be opened ({error}); this run writes no log-Live.log\n").as_bytes(),
                crate::CRASH_LOG_CAP,
            );
            eprintln!("log file open failed ({live_path:?}): {error}");
            None
        }
    };

    let logger = FileLogger {
        files: Mutex::new(files),
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
    {
        let _ = files.live.seek(SeekFrom::End(0));
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
        if let Ok(mut files) = self.files.lock()
            && let Some(files) = files.as_mut()
        {
            // The cap counter advances only on a successful write:
            // phantom bytes from failed writes would trip the reset while
            // the disk still holds the old body. No per-line flush: the OS
            // page cache keeps the write durable across a process crash,
            // which is what the log is for. Flushing every Debug line would
            // stall the SMTC worker under churn; only a power loss can lose
            // the last few lines.
            match files.live.write_all(line.as_bytes()) {
                Ok(()) => files.written += line.len() as u64,
                Err(_) => return,
            }
            if files.written >= LIVE_LOG_CAP {
                // Start the log fresh instead of growing without bound; the
                // file is diagnostic scratch, not user data. If the truncate
                // itself fails (a full disk again), keep appending past the
                // stale cursor rather than resetting it: overwriting live
                // bytes with a wrong offset would garble what is still
                // readable.
                if files.live.set_len(0).is_ok() {
                    let _ = files.live.seek(SeekFrom::Start(0));
                    files.written = 0;
                }
            }
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
