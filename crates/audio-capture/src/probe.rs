//! Latency probe: plays short clicks on the default output device while
//! recording the default microphone, both timed on the same clock (QPC).
//!
//! The clicks reach the speaker the app is streaming to through loopback
//! capture, so the recording holds each click twice when the PC's own
//! speakers are audible (PC first, then the AirPlay speaker), or once when
//! they aren't. The analysis lives with the caller; this module only records.

use std::f32::consts::PI;
use std::time::{Duration, Instant};

use wasapi::{AudioCaptureClient, AudioClient, DeviceEnumerator, Direction, StreamMode};

use crate::priority::ProAudioPriority;
use crate::wasapi_loopback::{Decoder, dev};
use crate::{Error, Result};

const POLL_INTERVAL: Duration = Duration::from_millis(2);
const BUFFER_HNS: i64 = 1_000_000;
/// Silence before the first click, so both streams have settled.
const LEAD_IN: Duration = Duration::from_millis(800);
/// A click: 4 ms, 2.5 kHz burst under a Hann window. Short enough to time
/// to a millisecond, tonal enough to survive a HomePod's processing.
const CLICK_MS: f32 = 4.0;
const CLICK_HZ: f32 = 2_500.0;

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub clicks: usize,
    /// Time between clicks; must exceed the latency being measured.
    pub period: Duration,
    /// Click amplitude (0..1) before the Windows volume.
    pub level: f32,
    /// Record the default microphone. Off: only play the clicks (for
    /// recording on a phone instead).
    pub record: bool,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            clicks: 12,
            period: Duration::from_millis(1500),
            level: 0.7,
            record: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProbeRecording {
    pub mic_name: Option<String>,
    pub output_name: String,
    /// Mono microphone samples (first channel); empty without a microphone.
    pub rate: u32,
    pub samples: Vec<f32>,
    /// When each click left the PC's output device, in seconds from
    /// `samples[0]` (or from the start of playback without a microphone).
    pub clicks: Vec<f64>,
}

struct Mic {
    name: String,
    client: AudioClient,
    capture: AudioCaptureClient,
    codec: Decoder,
    rate: u32,
    channels: usize,
    align: usize,
    raw: Vec<u8>,
    stereo: Vec<f32>,
    samples: Vec<f32>,
    /// QPC (100 ns) of the first recorded frame.
    start_hns: Option<u64>,
}

impl Mic {
    fn open(enumerator: &DeviceEnumerator) -> Result<Self> {
        let device = enumerator
            .get_default_device(&Direction::Capture)
            .map_err(|_| Error::Device("no microphone (default recording device)".into()))?;
        let mut client = device.get_iaudioclient().map_err(dev("mic client"))?;
        let format = client.get_mixformat().map_err(dev("mic format"))?;
        let codec = Decoder::for_format(&format)?;
        client
            .initialize_client(
                &format,
                &Direction::Capture,
                &StreamMode::PollingShared {
                    autoconvert: false,
                    buffer_duration_hns: BUFFER_HNS,
                },
            )
            .map_err(dev("initialize microphone"))?;
        let capture = client
            .get_audiocaptureclient()
            .map_err(dev("mic capture client"))?;
        let align = format.get_blockalign() as usize;
        let raw = vec![0u8; client.get_buffer_size().map_err(dev("mic buffer"))? as usize * align];
        Ok(Self {
            name: device.get_friendlyname().unwrap_or_else(|_| "?".into()),
            client,
            capture,
            codec,
            rate: format.get_samplespersec(),
            channels: format.get_nchannels() as usize,
            align,
            raw,
            stereo: Vec::new(),
            samples: Vec::new(),
            start_hns: None,
        })
    }

    fn drain(&mut self) -> Result<()> {
        while self
            .capture
            .get_next_packet_size()
            .map_err(dev("mic packet size"))?
            .unwrap_or(0)
            > 0
        {
            let (frames, info) = self
                .capture
                .read_from_device(&mut self.raw)
                .map_err(dev("mic read"))?;
            let frames = frames as usize;
            self.start_hns.get_or_insert(info.timestamp);
            self.stereo.clear();
            self.stereo.resize(frames * 2, 0.0);
            if !info.flags.silent {
                self.codec.to_stereo(
                    &self.raw[..frames * self.align],
                    self.channels,
                    &mut self.stereo,
                );
            }
            self.samples.extend(self.stereo.iter().step_by(2));
        }
        Ok(())
    }
}

/// Plays `config.clicks` clicks (and records the microphone) until the last
/// click plus one period. Blocking.
pub fn record_clicks(config: &ProbeConfig) -> Result<ProbeRecording> {
    let _mmcss = ProAudioPriority::enter();
    wasapi::initialize_mta().ok().map_err(dev("COM init"))?;
    let enumerator = DeviceEnumerator::new().map_err(dev("device enumerator"))?;
    let output = enumerator
        .get_default_device(&Direction::Render)
        .map_err(|_| Error::NoOutputDevice)?;
    let mut mic = if config.record {
        Some(Mic::open(&enumerator)?)
    } else {
        None
    };

    let mut render = output.get_iaudioclient().map_err(dev("render client"))?;
    let format = render.get_mixformat().map_err(dev("output format"))?;
    let codec = Decoder::for_format(&format)?;
    let rate = format.get_samplespersec();
    let channels = format.get_nchannels() as usize;
    let align = format.get_blockalign() as usize;
    render
        .initialize_client(
            &format,
            &Direction::Render,
            &StreamMode::PollingShared {
                autoconvert: false,
                buffer_duration_hns: BUFFER_HNS,
            },
        )
        .map_err(dev("initialize output"))?;
    let render_client = render
        .get_audiorenderclient()
        .map_err(dev("render client"))?;
    let clock = render.get_audioclock().map_err(dev("output clock"))?;
    let clock_freq = clock.get_frequency().map_err(dev("clock frequency"))? as f64;

    // Click schedule in output frames.
    let period = (config.period.as_secs_f64() * f64::from(rate)) as u64;
    let lead = (LEAD_IN.as_secs_f64() * f64::from(rate)) as u64;
    let click_len = (CLICK_MS / 1000.0 * rate as f32) as u64;
    let click_frames: Vec<u64> = (0..config.clicks as u64)
        .map(|k| lead + k * period)
        .collect();
    let end_frame = lead + config.clicks as u64 * period + period;
    let click = |n: u64| -> f32 {
        let Some(&start) = click_frames.iter().find(|&&s| n >= s && n < s + click_len) else {
            return 0.0;
        };
        let t = (n - start) as f32;
        let window = 0.5 - 0.5 * (2.0 * PI * t / click_len as f32).cos();
        config.level * window * (2.0 * PI * CLICK_HZ * t / rate as f32).sin()
    };

    let mut out_buf =
        vec![0u8; render.get_buffer_size().map_err(dev("output buffer"))? as usize * align];
    // (output frame position, QPC in 100 ns) pairs from the output clock.
    let mut clock_points: Vec<(f64, u64)> = Vec::new();
    let mut written: u64 = 0;
    let started = Instant::now();
    let timeout = LEAD_IN + config.period * (config.clicks as u32 + 3);

    if let Some(mic) = &mic {
        mic.client.start_stream().map_err(dev("start microphone"))?;
    }
    render.start_stream().map_err(dev("start output"))?;

    loop {
        let space = render
            .get_available_space_in_frames()
            .map_err(dev("output space"))? as usize;
        if space > 0 {
            let w = codec.width();
            for i in 0..space {
                let v = click(written + i as u64);
                for c in 0..channels {
                    let at = i * align + c * w;
                    codec.encode(v, &mut out_buf[at..at + w]);
                }
            }
            render_client
                .write_to_device(space, &out_buf[..space * align], None)
                .map_err(dev("output write"))?;
            written += space as u64;
        }

        if let Ok((pos, qpc)) = clock.get_position() {
            let frames = pos as f64 * f64::from(rate) / clock_freq;
            if frames > 0.0 && clock_points.last().is_none_or(|&(f, _)| frames > f) {
                clock_points.push((frames, qpc));
            }
        }
        if let Some(mic) = &mut mic {
            mic.drain()?;
        }

        let played = clock_points.last().map_or(0.0, |&(f, _)| f);
        if played >= end_frame as f64 {
            break;
        }
        if started.elapsed() > timeout {
            return Err(Error::Device("output device stopped playing".into()));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    let _ = render.stop_stream();
    if let Some(mic) = &mic {
        let _ = mic.client.stop_stream();
    }

    let origin_hns = match &mic {
        Some(m) => m
            .start_hns
            .ok_or_else(|| Error::Device("microphone produced no audio".into()))?,
        None => clock_points[0].1,
    };
    // Map each click frame to QPC with the latest clock reading at or before it.
    let clicks = click_frames
        .iter()
        .map(|&frame| {
            let f = frame as f64;
            let &(pf, qpc) = clock_points
                .iter()
                .rev()
                .find(|&&(pf, _)| pf <= f)
                .unwrap_or(&clock_points[0]);
            let at_hns = qpc as f64 + (f - pf) / f64::from(rate) * 1e7;
            (at_hns - origin_hns as f64) / 1e7
        })
        .collect();

    Ok(ProbeRecording {
        mic_name: mic.as_ref().map(|m| m.name.clone()),
        output_name: output.get_friendlyname().unwrap_or_else(|_| "?".into()),
        rate: mic.as_ref().map_or(rate, |m| m.rate),
        samples: mic.map(|m| m.samples).unwrap_or_default(),
        clicks,
    })
}
