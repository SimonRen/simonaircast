//! File logging for the tray app (it has no console):
//! `%LOCALAPPDATA%\simonaircast\logs\simonaircast.log`, rotated at 5 MB.

use std::path::PathBuf;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub fn log_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("simonaircast")
        .join("logs")
}

/// Installs a global subscriber writing to the log file. `RUST_LOG` overrides
/// the default level. Returns the log file path, or `None` if it can't be opened.
pub fn init_file() -> Option<PathBuf> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("simonaircast.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, dir.join("simonaircast.old.log"));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .init();
    Some(path)
}
