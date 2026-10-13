// No console window: this is the tray app. Logs go to
// %LOCALAPPDATA%\simonaircast\logs\simonaircast.log.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    if let Err(e) = simonaircast::tray::run() {
        tracing::error!(error = format!("{e:#}"), "tray app failed");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    eprintln!("simonaircast-tray only runs on Windows; use the `simonaircast` CLI.");
}
