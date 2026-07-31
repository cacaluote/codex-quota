use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_LOG_BYTES: u64 = 1024 * 1024;

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

struct Logger {
    path: PathBuf,
    file: Option<File>,
}

pub fn init(directory: &Path) {
    let path = directory.join("codex-quota.log");
    let logger = Logger::open(path);
    let _ = LOGGER.set(Mutex::new(logger));
}

pub fn log(message: &str) {
    let Some(logger) = LOGGER.get() else {
        return;
    };
    let Ok(mut logger) = logger.lock() else {
        return;
    };
    logger.write(message);
}

impl Logger {
    fn open(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        Self { path, file }
    }

    fn write(&mut self, message: &str) {
        if self
            .file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .is_some_and(|metadata| metadata.len() >= MAX_LOG_BYTES)
        {
            self.rotate();
        }

        let timestamp = local_timestamp();
        if let Some(file) = self.file.as_mut() {
            let _ = writeln!(file, "[{timestamp}] {message}");
            let _ = file.flush();
        }
    }

    fn rotate(&mut self) {
        self.file = None;
        let old_path = self.path.with_extension("log.old");
        let _ = fs::remove_file(&old_path);
        let _ = fs::rename(&self.path, old_path);
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
    }
}

fn local_timestamp() -> String {
    // SAFETY: GetLocalTime has no caller-side preconditions and returns SYSTEMTIME by value.
    let time = unsafe { GetLocalTime() };
    format_system_time(&time)
}

fn format_system_time(time: &SYSTEMTIME) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_time_is_formatted_as_readable_local_timestamp() {
        let time = SYSTEMTIME {
            wYear: 2026,
            wMonth: 7,
            wDay: 31,
            wHour: 14,
            wMinute: 9,
            wSecond: 22,
            ..Default::default()
        };

        assert_eq!(format_system_time(&time), "2026-07-31 14:09:22");
    }
}
