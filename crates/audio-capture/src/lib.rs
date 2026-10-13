//! System audio capture for streaming: WASAPI loopback of the default output
//! device, converted to 44.1 kHz stereo with a drift-correcting resampler.
//!
//! Two Windows behaviours shape the design:
//! - Loopback delivers no data while nothing is playing. We render silence on
//!   the same device, so capture runs continuously on the device clock.
//! - The AirPlay stream is paced by the system clock; the audio device runs on
//!   its own crystal, typically tens of ppm off. A slow PI controller nudges the
//!   resample ratio to hold the buffer at its target fill instead of letting it
//!   drain (dropouts) or grow (creeping latency).

pub mod priority;
#[cfg(windows)]
pub mod probe;
pub mod volume;
#[cfg(windows)]
mod wasapi_loopback;

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub const OUTPUT_RATE: u32 = 44_100;
pub const OUTPUT_CHANNELS: usize = 2;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("audio device error: {0}")]
    Device(String),
    #[error("unsupported device format: {0}")]
    Format(String),
    #[error("the default output device changed")]
    DeviceChanged,
    /// Windows has no default playback device: speakers/monitor unplugged or
    /// asleep (HDMI audio vanishes with the display), or a Remote Desktop
    /// session without audio redirection.
    #[error(
        "no audio output device (is the monitor/speaker on? Remote Desktop sessions have none)"
    )]
    NoOutputDevice,
    #[error("system audio capture is only implemented on Windows")]
    Unsupported,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Buffer level the drift controller holds. Must cover the sender's burst
    /// size (two 8 ms packets per ~15.6 ms Windows timer tick) with margin.
    pub target_fill: Duration,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            target_fill: Duration::from_millis(60),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub format: String,
}

#[derive(Debug, Clone, Default)]
pub struct CaptureStats {
    /// Current resample-ratio correction in parts per million.
    pub correction_ppm: f64,
    pub buffered: Duration,
    /// Times the reader found too little audio and padded with silence.
    pub underruns: u64,
    /// Samples dropped because the buffer was full (reader stalled).
    pub overflows: u64,
    /// Glitches reported by Windows in the captured stream.
    pub discontinuities: u64,
}

/// State shared between the capture thread and the reader.
#[derive(Default)]
struct Shared {
    correction_ppb: AtomicI64,
    overflows: AtomicU64,
    discontinuities: AtomicU64,
    error: Mutex<Option<String>>,
    /// Set by the reader when playback starts; the capture thread then
    /// restarts the drift controller from a clean state.
    reset_controller: AtomicBool,
}

pub struct Capture {
    consumer: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    info: DeviceInfo,
    target_frames: usize,
    started: bool,
    underruns: u64,
}

impl Capture {
    /// Starts capturing the default output device. Audio accumulates until read.
    pub fn start(config: CaptureConfig) -> Result<Self> {
        #[cfg(windows)]
        {
            let target_frames =
                (config.target_fill.as_secs_f64() * f64::from(OUTPUT_RATE)) as usize;
            // Room for ~2 s so a stalled reader doesn't immediately overflow.
            let (producer, consumer) =
                rtrb::RingBuffer::new(OUTPUT_CHANNELS * OUTPUT_RATE as usize * 2);
            let shared = Arc::new(Shared::default());
            let stop = Arc::new(AtomicBool::new(false));
            let (info_tx, info_rx) = std::sync::mpsc::channel();

            let thread_shared = shared.clone();
            let thread_stop = stop.clone();
            let thread = std::thread::Builder::new()
                .name("loopback-capture".into())
                .spawn(move || {
                    wasapi_loopback::run(
                        producer,
                        thread_shared,
                        thread_stop,
                        target_frames,
                        info_tx,
                    )
                })
                .map_err(|e| Error::Device(format!("spawning capture thread: {e}")))?;

            let info = info_rx
                .recv()
                .map_err(|_| Error::Device("capture thread exited during setup".into()))??;
            Ok(Self {
                consumer,
                shared,
                stop,
                thread: Some(thread),
                info,
                target_frames,
                started: false,
                underruns: 0,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = config;
            Err(Error::Unsupported)
        }
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    pub fn buffered_frames(&self) -> usize {
        self.consumer.slots() / OUTPUT_CHANNELS
    }

    /// Fills `out` (interleaved stereo) completely. Missing audio becomes
    /// silence and counts as an underrun. Returns the frames that were real audio.
    ///
    /// The first call starts playback: audio buffered beyond the target is
    /// dropped (it would only add latency) and the drift controller restarts,
    /// so startup buffering isn't mistaken for clock drift.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        if !self.started {
            self.started = true;
            let excess = self.buffered_frames().saturating_sub(self.target_frames);
            for _ in 0..excess * OUTPUT_CHANNELS {
                let _ = self.consumer.pop();
            }
            self.shared.reset_controller.store(true, Ordering::Relaxed);
        }
        let available = self.consumer.slots().min(out.len()) / OUTPUT_CHANNELS * OUTPUT_CHANNELS;
        for sample in &mut out[..available] {
            *sample = self.consumer.pop().unwrap_or(0.0);
        }
        if available < out.len() {
            out[available..].fill(0.0);
            self.underruns += 1;
        }
        available / OUTPUT_CHANNELS
    }

    pub fn stats(&self) -> CaptureStats {
        CaptureStats {
            correction_ppm: self.shared.correction_ppb.load(Ordering::Relaxed) as f64 / 1000.0,
            buffered: Duration::from_secs_f64(
                self.buffered_frames() as f64 / f64::from(OUTPUT_RATE),
            ),
            underruns: self.underruns,
            overflows: self.shared.overflows.load(Ordering::Relaxed),
            discontinuities: self.shared.discontinuities.load(Ordering::Relaxed),
        }
    }

    /// Set if the capture thread stopped with an error (e.g. device unplugged).
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().ok()?.clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// PI controller mapping buffer-fill error to a resample-ratio correction.
///
/// The buffer integrates the correction: d(fill)/dt = G·ppm with
/// G = 44 100·10⁻⁶ frames/s per ppm. With PI gains the closed loop is second
/// order, ωn = √(G·KI) and ζ = G·KP / (2ωn). The gains give ωn = 0.03 rad/s
/// (settles in ~2–3 min) at ζ = 1, i.e. no overshoot. (A first version had
/// ζ ≈ 0.09 and swung between the clamps on real hardware.)
///
/// Corrections stay far below audibility: the ±1000 ppm clamp is ≈ 1.7 cents.
struct DriftController {
    target: f64,
    smoothed: Option<f64>,
    integral: f64,
}

impl DriftController {
    const G: f64 = OUTPUT_RATE as f64 * 1e-6;
    const OMEGA: f64 = 0.03;
    const KP: f64 = 2.0 * Self::OMEGA / Self::G;
    const KI: f64 = Self::OMEGA * Self::OMEGA / Self::G;
    const MAX_PPM: f64 = 1000.0;
    /// Per-update weight of the fill average (~2 s at 10 updates/s). Reads come
    /// in bursts, so the raw fill jitters by several hundred frames.
    const SMOOTHING: f64 = 0.05;

    fn new(target_frames: usize) -> Self {
        Self {
            target: target_frames as f64,
            smoothed: None,
            integral: 0.0,
        }
    }

    fn reset(&mut self) {
        self.smoothed = None;
        self.integral = 0.0;
    }

    /// Feeds one fill measurement taken `dt` seconds after the previous one;
    /// returns the correction in ppm (positive = produce more output).
    fn update(&mut self, fill_frames: usize, dt: f64) -> f64 {
        let fill = fill_frames as f64;
        let smoothed = match self.smoothed {
            Some(s) => s + Self::SMOOTHING * (fill - s),
            None => fill,
        };
        self.smoothed = Some(smoothed);
        let error = smoothed - self.target;

        // Anti-windup: only integrate while the output isn't saturated.
        let integral = self.integral + error * dt;
        let output = -(Self::KP * error + Self::KI * integral);
        if output.abs() <= Self::MAX_PPM {
            self.integral = integral;
        }
        output.clamp(-Self::MAX_PPM, Self::MAX_PPM)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: f64 = 2646.0; // 60 ms

    struct Run {
        /// Fill error at the end (frames).
        final_error: f64,
        /// Mean correction over the last 60 s (ppm); the instantaneous value
        /// carries tens of ppm of jitter from the bursty fill measurements.
        final_ppm: f64,
        /// Lowest fill seen (frames); must stay well above 0 (underrun).
        min_fill: f64,
        /// Largest error on the opposite side of the initial offset (frames).
        overshoot: f64,
    }

    /// Simulates a device clock `drift_ppm` fast against a reader at exactly
    /// 44.1 kHz, starting `offset` frames from target. The measured fill gets
    /// ±700 frames of sawtooth jitter, like bursty reads on Windows.
    fn simulate(drift_ppm: f64, offset: f64, seconds: f64) -> Run {
        let mut ctl = DriftController::new(TARGET as usize);
        let mut fill = TARGET + offset;
        let dt = 0.1;
        let mut ppm = 0.0;
        let mut min_fill = fill;
        let mut overshoot: f64 = 0.0;
        let steps = (seconds / dt) as usize;
        let tail = (60.0 / dt) as usize;
        let mut tail_sum = 0.0;
        for step in 0..steps {
            fill += f64::from(OUTPUT_RATE) * dt * (drift_ppm + ppm) / 1e6;
            min_fill = min_fill.min(fill);
            let error = fill - TARGET;
            if offset != 0.0 && error.signum() != offset.signum() {
                overshoot = overshoot.max(error.abs());
            }
            let jitter = ((step % 7) as f64 - 3.0) * 233.0;
            ppm = ctl.update((fill + jitter).max(0.0) as usize, dt);
            if step >= steps - tail {
                tail_sum += ppm;
            }
        }
        Run {
            final_error: fill - TARGET,
            final_ppm: tail_sum / tail as f64,
            min_fill,
            overshoot,
        }
    }

    #[test]
    fn cancels_typical_clock_drift_within_five_minutes() {
        for drift in [-80.0, -20.0, 20.0, 80.0] {
            let run = simulate(drift, 0.0, 300.0);
            assert!(
                run.final_error.abs() < 100.0,
                "drift {drift}: error {} frames",
                run.final_error
            );
            assert!(
                (run.final_ppm + drift).abs() < 15.0,
                "drift {drift}: settled at {} ppm",
                run.final_ppm
            );
            assert!(
                run.min_fill > TARGET / 2.0,
                "drift {drift}: fill dipped to {}",
                run.min_fill
            );
        }
    }

    /// The failure seen on real hardware: a startup surplus made the old gains
    /// swing between the clamps and drain the buffer to ~35 ms.
    #[test]
    fn recovers_from_startup_offset_without_overshoot() {
        for offset in [2205.0, -1000.0] {
            for drift in [-50.0, 0.0, 50.0] {
                let run = simulate(drift, offset, 300.0);
                assert!(
                    run.final_error.abs() < 100.0,
                    "offset {offset} drift {drift}: error {}",
                    run.final_error
                );
                assert!(
                    run.overshoot < 0.15 * offset.abs(),
                    "offset {offset} drift {drift}: overshoot {} frames",
                    run.overshoot
                );
                assert!(
                    run.min_fill > 1000.0,
                    "offset {offset} drift {drift}: fill dipped to {}",
                    run.min_fill
                );
            }
        }
    }

    #[test]
    fn gains_are_critically_damped() {
        let g = DriftController::G;
        let omega = (g * DriftController::KI).sqrt();
        let zeta = g * DriftController::KP / (2.0 * omega);
        assert!((zeta - 1.0).abs() < 1e-9, "zeta {zeta}");
    }

    #[test]
    fn controller_never_exceeds_clamp() {
        let mut ctl = DriftController::new(2646);
        for _ in 0..1000 {
            assert!(ctl.update(80_000, 0.1).abs() <= DriftController::MAX_PPM);
        }
    }
}
