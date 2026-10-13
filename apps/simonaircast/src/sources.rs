//! Audio sources: WAV files, a built-in test signal, and live system audio.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use airplay::stream::{
    CHANNELS, FRAMES_PER_PACKET, PcmPacket, PcmSource, SAMPLE_RATE, SAMPLES_PER_PACKET,
};
use anyhow::{Context, Result, bail};
use audio_capture::{Capture, CaptureStats};

/// What this PC is playing, via loopback capture. Never ends on its own; stops
/// (returns `false`) only if the capture device fails.
pub struct LiveSource {
    capture: Capture,
    scratch: Vec<f32>,
    packets: u64,
    /// Refreshed about once a second for progress reporting.
    pub stats: Arc<Mutex<CaptureStats>>,
    /// Linear gain (f32 bits) applied before conversion; used to undo the
    /// Windows volume when the receiver's volume follows it instead.
    pub gain: Arc<AtomicU32>,
}

impl LiveSource {
    pub fn new(capture: Capture) -> Self {
        Self {
            stats: Arc::new(Mutex::new(capture.stats())),
            capture,
            scratch: vec![0.0; SAMPLES_PER_PACKET],
            packets: 0,
            gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
        }
    }

    pub fn error(&self) -> Option<String> {
        self.capture.error()
    }
}

impl PcmSource for LiveSource {
    fn fill(&mut self, packet: &mut PcmPacket) -> bool {
        if self.capture.error().is_some() {
            return false;
        }
        self.capture.read(&mut self.scratch);
        let gain = f32::from_bits(self.gain.load(Ordering::Relaxed));
        for (dst, &src) in packet.iter_mut().zip(&self.scratch) {
            *dst = ((src * gain).clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        }
        self.packets += 1;
        if self.packets.is_multiple_of(125)
            && let Ok(mut stats) = self.stats.lock()
        {
            *stats = self.capture.stats();
        }
        true
    }
}

/// 16-bit PCM WAV at 44.1 kHz, mono or stereo (mono is duplicated to both channels).
pub struct WavSource {
    samples: hound::WavIntoSamples<std::io::BufReader<std::fs::File>, i16>,
    channels: usize,
    pub duration_secs: f64,
}

impl WavSource {
    pub fn open(path: &Path) -> Result<Self> {
        let reader =
            hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
        let spec = reader.spec();
        if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
            bail!("{}: only 16-bit integer PCM is supported", path.display());
        }
        if spec.sample_rate != SAMPLE_RATE {
            bail!(
                "{}: sample rate is {} Hz, need {SAMPLE_RATE} Hz (resampling arrives with live capture)",
                path.display(),
                spec.sample_rate
            );
        }
        if !(1..=2).contains(&spec.channels) {
            bail!(
                "{}: {} channels, need mono or stereo",
                path.display(),
                spec.channels
            );
        }
        Ok(Self {
            duration_secs: f64::from(reader.duration()) / f64::from(SAMPLE_RATE),
            channels: spec.channels.into(),
            samples: reader.into_samples(),
        })
    }
}

impl PcmSource for WavSource {
    fn fill(&mut self, packet: &mut PcmPacket) -> bool {
        let mut frames = 0;
        while frames < FRAMES_PER_PACKET {
            let Some(Ok(left)) = self.samples.next() else {
                break;
            };
            let right = if self.channels == 2 {
                self.samples.next().and_then(Result::ok).unwrap_or(left)
            } else {
                left
            };
            packet[frames * CHANNELS] = left;
            packet[frames * CHANNELS + 1] = right;
            frames += 1;
        }
        packet[frames * CHANNELS..].fill(0);
        frames > 0
    }
}

/// A short 880 Hz beep at the start of every second. Gaps, clicks or drift
/// against a clock are easy to hear, which makes it a good long-run sync test.
pub struct Beeps {
    frame: u64,
    total_frames: u64,
}

impl Beeps {
    pub fn new(seconds: u64) -> Self {
        Self {
            frame: 0,
            total_frames: seconds * u64::from(SAMPLE_RATE),
        }
    }
}

impl PcmSource for Beeps {
    fn fill(&mut self, packet: &mut PcmPacket) -> bool {
        if self.frame >= self.total_frames {
            return false;
        }
        let rate = u64::from(SAMPLE_RATE);
        for i in 0..FRAMES_PER_PACKET {
            let n = self.frame + i as u64;
            let in_second = n % rate;
            let sample = if n < self.total_frames && in_second < rate / 10 {
                let t = in_second as f64 / rate as f64;
                // -20 dBFS sine with a 5 ms fade in/out to avoid clicks.
                let fade = (t / 0.005).min((0.1 - t) / 0.005).clamp(0.0, 1.0);
                (3277.0 * fade * (2.0 * std::f64::consts::PI * 880.0 * t).sin()) as i16
            } else {
                0
            };
            packet[i * CHANNELS] = sample;
            packet[i * CHANNELS + 1] = sample;
        }
        self.frame += FRAMES_PER_PACKET as u64;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airplay::stream::SAMPLES_PER_PACKET;

    #[test]
    fn beeps_cover_requested_length_then_stop() {
        let mut beeps = Beeps::new(1);
        let mut packet = [0i16; SAMPLES_PER_PACKET];
        let mut packets = 0;
        let mut loud = 0;
        while beeps.fill(&mut packet) {
            packets += 1;
            loud += packet.iter().filter(|s| s.abs() > 1000).count();
        }
        assert_eq!(packets, 44_100_usize.div_ceil(FRAMES_PER_PACKET));
        assert!(loud > 0);
    }

    #[test]
    fn mono_wav_is_duplicated_and_last_packet_padded() {
        let path = std::env::temp_dir().join("simonaircast-test-mono.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..400i16 {
            w.write_sample(i).unwrap();
        }
        w.finalize().unwrap();

        let mut src = WavSource::open(&path).unwrap();
        let mut packet = [0i16; SAMPLES_PER_PACKET];
        assert!(src.fill(&mut packet));
        assert_eq!(&packet[..4], &[0, 0, 1, 1]);
        assert!(src.fill(&mut packet));
        assert_eq!(&packet[..2], &[352, 352]);
        assert_eq!(packet[2 * 48], 0, "padding after the 48 remaining frames");
        assert!(!src.fill(&mut packet));
    }
}
