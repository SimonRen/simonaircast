//! The capture thread: WASAPI loopback + silent keep-alive render stream,
//! resampled to 44.1 kHz stereo and pushed into the ring buffer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use wasapi::{BufferFlags, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

use crate::priority::ProAudioPriority;
use crate::{DeviceInfo, DriftController, Error, OUTPUT_CHANNELS, OUTPUT_RATE, Result, Shared};

const POLL_INTERVAL: Duration = Duration::from_millis(2);
const CONTROL_INTERVAL: Duration = Duration::from_millis(100);
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// Resampler input chunk in device frames (5 ms at 48 kHz). Small chunks keep
/// the buffer level smooth, so the target fill (and latency) can stay low.
const CHUNK_FRAMES: usize = 240;
/// WASAPI buffer durations, in 100 ns units.
const CAPTURE_BUFFER_HNS: i64 = 2_000_000;
const RENDER_BUFFER_HNS: i64 = 400_000;

pub(crate) fn run(
    mut producer: rtrb::Producer<f32>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    target_frames: usize,
    info_tx: Sender<Result<DeviceInfo>>,
) {
    let mut info_tx = Some(info_tx);
    let _mmcss = ProAudioPriority::enter();
    let result = capture_loop(&mut producer, &shared, &stop, target_frames, &mut info_tx);
    if let Err(e) = result {
        tracing::warn!(error = %e, "loopback capture stopped");
        match info_tx.take() {
            Some(tx) => {
                let _ = tx.send(Err(e));
            }
            None => {
                if let Ok(mut slot) = shared.error.lock() {
                    *slot = Some(e.to_string());
                }
            }
        }
    }
}

fn dev<E: std::fmt::Display>(context: &'static str) -> impl FnOnce(E) -> Error {
    move |e| Error::Device(format!("{context}: {e}"))
}

fn capture_loop(
    producer: &mut rtrb::Producer<f32>,
    shared: &Shared,
    stop: &AtomicBool,
    target_frames: usize,
    info_tx: &mut Option<Sender<Result<DeviceInfo>>>,
) -> Result<()> {
    wasapi::initialize_mta().ok().map_err(dev("COM init"))?;
    let device = DeviceEnumerator::new()
        .map_err(dev("device enumerator"))?
        .get_default_device(&Direction::Render)
        .map_err(|e| {
            tracing::debug!(error = %e, "no default render device");
            Error::NoOutputDevice
        })?;
    let name = device.get_friendlyname().unwrap_or_else(|_| "?".into());
    let device_id = device.get_id().map_err(dev("device id"))?;

    // Keep-alive: render silence so loopback produces data on the device clock.
    let mut render = device.get_iaudioclient().map_err(dev("render client"))?;
    let mix = render.get_mixformat().map_err(dev("mix format"))?;
    let decode = Decoder::for_format(&mix)?;
    let rate = mix.get_samplespersec();
    let channels = mix.get_nchannels();
    let block_align = mix.get_blockalign() as usize;

    render
        .initialize_client(
            &mix,
            &Direction::Render,
            &StreamMode::PollingShared {
                autoconvert: false,
                buffer_duration_hns: RENDER_BUFFER_HNS,
            },
        )
        .map_err(dev("initialize keep-alive render stream"))?;
    let render_client = render
        .get_audiorenderclient()
        .map_err(dev("render client"))?;
    let silence =
        vec![0u8; render.get_buffer_size().map_err(dev("render buffer"))? as usize * block_align];

    let mut capture = device.get_iaudioclient().map_err(dev("capture client"))?;
    capture
        .initialize_client(
            &mix,
            &Direction::Capture,
            &StreamMode::PollingShared {
                autoconvert: false,
                buffer_duration_hns: CAPTURE_BUFFER_HNS,
            },
        )
        .map_err(dev("initialize loopback capture"))?;
    let capture_client = capture
        .get_audiocaptureclient()
        .map_err(dev("capture client"))?;
    let mut raw =
        vec![0u8; capture.get_buffer_size().map_err(dev("capture buffer"))? as usize * block_align];

    let params = SincInterpolationParameters::new(128, WindowFunction::Blackman2)
        .oversampling_factor(256)
        .interpolation(SincInterpolationType::Quadratic);
    let mut resampler = Async::<f32>::new_sinc(
        f64::from(OUTPUT_RATE) / f64::from(rate),
        1.01,
        &params,
        CHUNK_FRAMES,
        OUTPUT_CHANNELS,
        FixedAsync::Input,
    )
    .map_err(dev("resampler"))?;
    let mut resampled = vec![0f32; resampler.output_frames_max() * OUTPUT_CHANNELS];
    let mut pending: Vec<f32> = Vec::with_capacity(CHUNK_FRAMES * OUTPUT_CHANNELS * 4);

    render
        .start_stream()
        .map_err(dev("start keep-alive stream"))?;
    capture
        .start_stream()
        .map_err(dev("start loopback capture"))?;
    let info = DeviceInfo {
        name,
        sample_rate: rate,
        channels,
        format: decode.describe(),
    };
    tracing::debug!(?info, "loopback capture running");
    if let Some(tx) = info_tx.take() {
        let _ = tx.send(Ok(info));
    }

    let mut controller = DriftController::new(target_frames);
    let mut last_control = Instant::now();
    let mut last_device_check = Instant::now();
    let enumerator = DeviceEnumerator::new().map_err(dev("device enumerator"))?;
    let capacity = producer.slots();

    while !stop.load(Ordering::Relaxed) {
        let space = render
            .get_available_space_in_frames()
            .map_err(dev("keep-alive space"))? as usize;
        if space > 0 {
            render_client
                .write_to_device(
                    space,
                    &silence[..space * block_align],
                    Some(BufferFlags {
                        data_discontinuity: false,
                        silent: true,
                        timestamp_error: false,
                    }),
                )
                .map_err(dev("keep-alive write"))?;
        }

        while capture_client
            .get_next_packet_size()
            .map_err(dev("packet size"))?
            .unwrap_or(0)
            > 0
        {
            let (frames, info) = capture_client
                .read_from_device(&mut raw)
                .map_err(dev("loopback read"))?;
            let frames = frames as usize;
            if info.flags.data_discontinuity {
                shared.discontinuities.fetch_add(1, Ordering::Relaxed);
            }
            let start = pending.len();
            pending.resize(start + frames * OUTPUT_CHANNELS, 0.0);
            if !info.flags.silent {
                decode.to_stereo(
                    &raw[..frames * block_align],
                    channels as usize,
                    &mut pending[start..],
                );
            }
        }

        let mut consumed = 0;
        while pending.len() - consumed >= CHUNK_FRAMES * OUTPUT_CHANNELS {
            let input = InterleavedSlice::new(&pending[consumed..], OUTPUT_CHANNELS, CHUNK_FRAMES)
                .map_err(dev("resampler input"))?;
            let out_frames = resampled.len() / OUTPUT_CHANNELS;
            let mut output = InterleavedSlice::new_mut(&mut resampled, OUTPUT_CHANNELS, out_frames)
                .map_err(dev("resampler output"))?;
            let (used, produced) = resampler
                .process_into_buffer(&input, &mut output, None)
                .map_err(dev("resample"))?;
            consumed += used * OUTPUT_CHANNELS;
            for &sample in &resampled[..produced * OUTPUT_CHANNELS] {
                if producer.push(sample).is_err() {
                    shared.overflows.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        pending.drain(..consumed);

        // Windows keeps a loopback stream on the old device after the user
        // switches output (e.g. to headphones); stop so the caller restarts.
        if last_device_check.elapsed() >= DEVICE_CHECK_INTERVAL {
            last_device_check = Instant::now();
            let current = enumerator
                .get_default_device(&Direction::Render)
                .and_then(|d| d.get_id())
                .ok();
            if current.as_deref() != Some(device_id.as_str()) {
                return Err(Error::DeviceChanged);
            }
        }

        if last_control.elapsed() >= CONTROL_INTERVAL {
            let dt = last_control.elapsed().as_secs_f64();
            last_control = Instant::now();
            if shared.reset_controller.swap(false, Ordering::Relaxed) {
                controller.reset();
            }
            let fill = (capacity - producer.slots()) / OUTPUT_CHANNELS;
            let ppm = controller.update(fill, dt);
            if let Some(adjustable) = resampler.as_adjustable() {
                adjustable
                    .set_resample_ratio_relative(1.0 + ppm / 1e6, true)
                    .map_err(dev("adjust ratio"))?;
            }
            shared
                .correction_ppb
                .store((ppm * 1000.0) as i64, Ordering::Relaxed);
        }

        std::thread::sleep(POLL_INTERVAL);
    }

    let _ = capture.stop_stream();
    let _ = render.stop_stream();
    Ok(())
}

/// Converts the device's interleaved samples to stereo f32.
enum Decoder {
    F32,
    I16,
    I32,
}

impl Decoder {
    fn for_format(format: &WaveFormat) -> Result<Self> {
        let subformat = format
            .get_subformat()
            .map_err(|e| Error::Format(e.to_string()))?;
        match (subformat, format.get_bitspersample()) {
            (SampleType::Float, 32) => Ok(Self::F32),
            (SampleType::Int, 16) => Ok(Self::I16),
            (SampleType::Int, 32) => Ok(Self::I32),
            (other, bits) => Err(Error::Format(format!("{other:?} {bits}-bit"))),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::F32 => "float32",
            Self::I16 => "int16",
            Self::I32 => "int32",
        }
        .into()
    }

    fn sample(&self, bytes: &[u8]) -> f32 {
        match self {
            Self::F32 => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            Self::I16 => f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32_768.0,
            Self::I32 => {
                i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32
                    / 2_147_483_648.0
            }
        }
    }

    fn width(&self) -> usize {
        match self {
            Self::I16 => 2,
            Self::F32 | Self::I32 => 4,
        }
    }

    /// Takes the first two channels (front left/right); mono is duplicated.
    fn to_stereo(&self, raw: &[u8], channels: usize, out: &mut [f32]) {
        let w = self.width();
        let (out, _) = out.as_chunks_mut::<OUTPUT_CHANNELS>();
        for (frame, dst) in raw.chunks_exact(channels * w).zip(out) {
            let left = self.sample(&frame[..w]);
            let right = if channels > 1 {
                self.sample(&frame[w..2 * w])
            } else {
                left
            };
            dst[0] = left;
            dst[1] = right;
        }
    }
}
