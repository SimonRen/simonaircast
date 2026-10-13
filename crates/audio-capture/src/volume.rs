//! Watches the Windows volume of the default output device.
//!
//! Polls (cheap) instead of registering a COM callback: a dedicated thread reads
//! the master volume, mute and device id a few times a second and reports
//! changes. A changed device id also tells the caller to restart capture.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, PartialEq)]
pub struct OutputVolume {
    /// Slider position, 0.0–1.0.
    pub level: f32,
    /// Attenuation the endpoint applies at this position, in dB (e.g. 55 % is
    /// about -9.1 dB on the Dell HDMI endpoint). Loopback audio is scaled by this.
    pub level_db: f32,
    pub muted: bool,
    /// True if the device applies volume in hardware, i.e. after loopback
    /// capture. Then captured audio is not attenuated and needs no compensation.
    pub hardware: bool,
    /// Endpoint id of the default output device.
    pub device_id: String,
}

pub struct VolumeWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VolumeWatcher {
    /// Calls `on_change` with the current state right away, then whenever it changes.
    pub fn start(on_change: impl FnMut(OutputVolume) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("volume-watcher".into())
            .spawn(move || run(on_change, &thread_stop))
            .ok();
        Self { stop, thread }
    }
}

impl Drop for VolumeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
fn run(mut on_change: impl FnMut(OutputVolume), stop: &AtomicBool) {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::System::Com::CLSCTX_ALL;

    if wasapi::initialize_mta().is_err() {
        tracing::warn!("volume watcher: COM init failed");
        return;
    }
    let mut last: Option<OutputVolume> = None;
    let mut endpoint: Option<(String, IAudioEndpointVolume)> = None;

    while !stop.load(Ordering::Relaxed) {
        let current = (|| -> Option<OutputVolume> {
            let device = wasapi::DeviceEnumerator::new()
                .ok()?
                .get_default_device(&wasapi::Direction::Render)
                .ok()?;
            let id = device.get_id().ok()?;
            if endpoint.as_ref().is_none_or(|(known, _)| *known != id) {
                // SAFETY: activating a documented endpoint interface on an MTA thread.
                let volume: IAudioEndpointVolume =
                    unsafe { device.as_immdevice().Activate(CLSCTX_ALL, None) }.ok()?;
                endpoint = Some((id.clone(), volume));
            }
            let (_, volume) = endpoint.as_ref()?;
            // SAFETY: plain getters on a live interface, on the thread that created it.
            unsafe {
                Some(OutputVolume {
                    level: volume.GetMasterVolumeLevelScalar().ok()?,
                    level_db: volume.GetMasterVolumeLevel().ok()?,
                    muted: volume.GetMute().ok()?.as_bool(),
                    hardware: volume.QueryHardwareSupport().ok()? & 0x1 != 0,
                    device_id: id,
                })
            }
        })();

        if let Some(current) = current
            && last.as_ref() != Some(&current)
        {
            on_change(current.clone());
            last = Some(current);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(not(windows))]
fn run(_on_change: impl FnMut(OutputVolume), _stop: &AtomicBool) {}

/// Maps the Windows slider (0–1) to AirPlay volume: -30 dB (min) … 0 dB (max),
/// -144 for mute — the same linear slider mapping iOS uses.
pub fn airplay_volume_db(volume: &OutputVolume) -> f64 {
    if volume.muted || volume.level <= 0.001 {
        -144.0
    } else {
        -30.0 * (1.0 - f64::from(volume.level.min(1.0)))
    }
}

/// Gain that undoes the Windows software volume in the captured signal, so
/// the receiver gets full-scale audio and applies the volume itself.
pub fn compensation_gain(volume: &OutputVolume) -> f32 {
    if volume.hardware || volume.muted || volume.level <= 0.001 {
        1.0
    } else {
        // Loopback audio is attenuated by the endpoint's dB value, not by the
        // slider scalar: at 55 % (-9.1 dB) capture measured ~9 dB below the
        // source. Undo that, capped at +40 dB.
        10f32.powf(-volume.level_db.min(0.0) / 20.0).min(100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(level: f32, muted: bool, hardware: bool) -> OutputVolume {
        OutputVolume {
            level,
            level_db: -20.0,
            muted,
            hardware,
            device_id: "x".into(),
        }
    }

    #[test]
    fn maps_slider_to_airplay_range() {
        assert_eq!(airplay_volume_db(&vol(1.0, false, false)), 0.0);
        assert_eq!(airplay_volume_db(&vol(0.5, false, false)), -15.0);
        assert_eq!(airplay_volume_db(&vol(0.0, false, false)), -144.0);
        assert_eq!(airplay_volume_db(&vol(0.8, true, false)), -144.0);
    }

    #[test]
    fn compensates_software_volume_only() {
        // -20 dB attenuation -> x10 gain.
        assert!((compensation_gain(&vol(0.5, false, false)) - 10.0).abs() < 1e-4);
        assert_eq!(compensation_gain(&vol(0.5, false, true)), 1.0);
        assert_eq!(compensation_gain(&vol(0.001, false, false)), 1.0);
        let quiet = OutputVolume {
            level_db: -60.0,
            ..vol(0.01, false, false)
        };
        assert_eq!(compensation_gain(&quiet), 100.0, "capped at +40 dB");
    }
}
