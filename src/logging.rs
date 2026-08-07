use chrono::Utc;
use log::{LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// The live log is truncated when it exceeds this many bytes: the file is
/// only cleared at startup, and a churn-heavy session can otherwise write
/// tens of MB of Debug lines to disk.
const LIVE_LOG_CAP: u64 = 1024 * 1024;

pub fn init_logging(logs_dir: &Path) {
    let _ = fs::create_dir_all(logs_dir);
    let live_path = logs_dir.join("log-Live.log");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&live_path);
    let files = match file {
        Ok(file) => Some(LogFiles { live: file, written: 0 }),
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
    /// Bytes written since the last truncation.
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
            Utc::now().to_rfc3339(),
            record.level(),
            record.target(),
            record.args()
        );
        if let Ok(mut files) = self.files.lock()
            && let Some(files) = files.as_mut()
        {
            let _ = files.live.write_all(line.as_bytes());
            let _ = files.live.flush();
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
