//! Persistent app settings: `%APPDATA%\simonaircast\settings.json`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Receiver to stream to on launch (the last one the user picked).
    pub speaker: Option<String>,
    pub delay_ms: u64,
    pub buffer_ms: u64,
    pub volume_sync: bool,
    /// Advanced (no menu item): `latencyMin` sent in SETUP, see engine::Settings.
    pub latency_min_ms: u64,
    /// Advanced (no menu item): send uncompressed ALAC frames instead of PCM.
    pub alac: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        let engine = engine::Settings::default();
        Self {
            speaker: None,
            delay_ms: engine.delay.as_millis() as u64,
            buffer_ms: engine.buffer.as_millis() as u64,
            volume_sync: engine.volume_sync,
            latency_min_ms: engine.latency_min.as_millis() as u64,
            alac: engine.codec == airplay::stream::Codec::AlacUncompressed,
        }
    }
}

impl AppSettings {
    /// `%APPDATA%\simonaircast` (falls back to the current directory).
    pub fn dir() -> PathBuf {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("simonaircast")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("settings.json")
    }

    /// Loads settings; a missing or unreadable file yields defaults.
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "ignoring invalid settings file");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Writes atomically (temp file + rename) so a crash can't truncate it.
    pub fn save(&self) -> Result<()> {
        let dir = Self::dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = dir.join("settings.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, Self::path())?;
        Ok(())
    }

    pub fn engine(&self) -> engine::Settings {
        engine::Settings {
            delay: Duration::from_millis(self.delay_ms),
            buffer: Duration::from_millis(self.buffer_ms),
            volume_sync: self.volume_sync,
            codec: if self.alac {
                airplay::stream::Codec::AlacUncompressed
            } else {
                airplay::stream::Codec::Pcm
            },
            latency_min: Duration::from_millis(self.latency_min_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let s: AppSettings = serde_json::from_str(r#"{"speaker":"书房"}"#).unwrap();
        assert_eq!(s.speaker.as_deref(), Some("书房"));
        assert_eq!(s.delay_ms, 100);
        assert!(s.volume_sync);
    }

    #[test]
    fn round_trips_through_json() {
        let s = AppSettings {
            speaker: Some("客厅".into()),
            delay_ms: 150,
            ..AppSettings::default()
        };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
