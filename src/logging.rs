use chrono::Utc;
use log::{LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const LIVE_LOG_NAME: &str = "log-Live.log";

pub fn init_logging(logs_dir: &Path, keep: u32) {
    let keep = keep.max(1);
    let _ = fs::create_dir_all(logs_dir);
    cleanup_old_logs(logs_dir, keep);

    let numbered_path = logs_dir.join(format!("log-{}.log", next_log_number(logs_dir)));
    let live_path = logs_dir.join(LIVE_LOG_NAME);
    let numbered = OpenOptions::new().create(true).append(true).open(&numbered_path);
    let live = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&live_path);

    let files = match (numbered, live) {
        (Ok(numbered), Ok(live)) => Some(LogFiles {
            numbered,
            live: Some(live),
        }),
        (Ok(numbered), Err(error)) => {
            eprintln!("live log open failed ({live_path:?}): {error}");
            Some(LogFiles { numbered, live: None })
        }
        (Err(error), _) => {
            eprintln!("log file open failed ({numbered_path:?}): {error}");
            None
        }
    };

    let logger = FileLogger {
        files: Mutex::new(files),
    };
    static LOGGER: OnceLock<FileLogger> = OnceLock::new();
    if LOGGER.set(logger).is_ok() && log::set_logger(LOGGER.get().expect("logger initialized")).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
    log::info!(
        "logging initialized | numbered: {:?} | live: {:?}",
        numbered_path,
        live_path
    );
}

struct LogFiles {
    numbered: File,
    live: Option<File>,
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
            let _ = files.numbered.write_all(line.as_bytes());
            if let Some(live) = files.live.as_mut() {
                let _ = live.write_all(line.as_bytes());
            }
            let _ = files.numbered.flush();
            if let Some(live) = files.live.as_mut() {
                let _ = live.flush();
            }
        }
        eprint!("{line}");
    }

    fn flush(&self) {}
}

fn list_log_files(logs_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(logs_dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("log-") && name.ends_with(".log") && name != LIVE_LOG_NAME)
        })
        .collect();
    files.sort();
    files
}

fn next_log_number(logs_dir: &Path) -> u32 {
    list_log_files(logs_dir)
        .iter()
        .filter_map(|path| path.file_stem()?.to_str()?.strip_prefix("log-")?.parse().ok())
        .max()
        .unwrap_or(0)
        + 1
}

fn cleanup_old_logs(logs_dir: &Path, keep: u32) {
    let files = list_log_files(logs_dir);
    let excess = files.len().saturating_sub(keep as usize);
    for path in files.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
}
