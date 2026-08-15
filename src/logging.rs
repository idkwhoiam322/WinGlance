use chrono::Local;
use log::{LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::windows::io::AsRawHandle;
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
/// The parent directory is pinned and identity-verified before the
/// open (a junction swapped into the logs dir rejects the launch), and the
/// opened file's final handle path must match the expected path, so a
/// pre-created `log-Live.log` symlink cannot redirect the session's writes.
fn open_live_log(live_path: &Path, preserve: bool) -> std::io::Result<(File, u64)> {
    if let Some(parent) = live_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
        // Held only for the open: the file handle is the anchor from here on.
        let _guard = crate::winutil::open_pinned_parent(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if preserve {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut file = options.open(live_path)?;
    let final_path = crate::winutil::final_path_of(file.as_raw_handle())?;
    if !crate::winutil::paths_equal(&final_path, &crate::winutil::extended_path(live_path)) {
        return Err(std::io::Error::other(format!(
            "live log resolved outside the expected path ({} vs {})",
            final_path.display(),
            live_path.display()
        )));
    }
    if preserve {
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
            eprintln!("log file open failed ({live_path:?}): {error}");
            None
        }
    };

    let logger = FileLogger {
        files: Mutex::new(files),
    };
    static LOGGER: OnceLock<FileLogger> = OnceLock::new();
    if LOGGER.set(logger).is_ok() && log::set_logger(LOGGER.get().expect("logger initialized")).is_ok() {
        // Debug level: session churn, dedup skips and suppressed states are all
        // logged, which is what makes "why did/didn't a notification fire"
        // answerable from the log file.
        log::set_max_level(LevelFilter::Debug);
    }
    log::info!("logging initialized | live log: {live_path:?}");
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
            let _ = files.live.write_all(line.as_bytes());
            // No per-line flush: the OS page cache keeps the write durable
            // across a process crash, which is what the log is for. Flushing
            // every Debug line would stall the SMTC worker under churn; only
            // a power loss can lose the last few lines.
            files.written += line.len() as u64;
            if files.written >= LIVE_LOG_CAP {
                // Start the log fresh instead of growing without bound; the
                // file is diagnostic scratch, not user data.
                let _ = files.live.set_len(0);
                let _ = files.live.seek(SeekFrom::Start(0));
                files.written = 0;
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
