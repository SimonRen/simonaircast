//! Shared core of the `simonaircast` CLI and the `simonaircast-tray` app.

pub mod engine;
pub mod icon;
pub mod latency;
pub mod logging;
pub mod net;
pub mod runtime;
pub mod settings;
pub mod sources;
pub mod system;
#[cfg(windows)]
pub mod tray;
