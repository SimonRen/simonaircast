use std::path::PathBuf;
use std::time::Duration;

use airplay::discovery::features;
use airplay::rtsp::{Connection, Protocol, Request};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use simonaircast::net::{discover, resolve};
use simonaircast::runtime::HighResTimer;
use simonaircast::{engine, sources};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List AirPlay 2 receivers on the local network.
    List {
        /// Seconds to browse for.
        #[arg(long, default_value_t = 3)]
        timeout: u64,
    },
    /// Pair with a receiver (transient, no PIN) and query it over the encrypted channel.
    Pair {
        /// Receiver name (as shown by `list`) or IP address.
        #[arg(long)]
        to: String,
    },
    /// Open a playback session (no audio yet): setup, RECORD, hold, TEARDOWN.
    /// Takes over the receiver, interrupting whatever it is playing.
    Session {
        /// Receiver name (as shown by `list`) or IP address.
        #[arg(long)]
        to: String,
        /// Seconds to hold the session open.
        #[arg(long, default_value_t = 15)]
        seconds: u64,
    },
    /// Play a WAV file (16-bit, 44.1 kHz) or a beep-per-second test signal.
    Play {
        /// Receiver name (as shown by `list`) or IP address.
        #[arg(long)]
        to: String,
        /// WAV file to play.
        #[arg(required_unless_present = "beeps")]
        file: Option<PathBuf>,
        /// Instead of a file, play a short beep every second for this many seconds.
        #[arg(long, conflicts_with = "file")]
        beeps: Option<u64>,
        /// Volume in dB (0 = max, -30 = min). Default: leave the receiver's volume.
        #[arg(long, allow_hyphen_values = true)]
        volume: Option<f64>,
        /// Playout delay in ms: how far ahead of its play time each packet is
        /// sent. Must cover the receiver's processing (HomePod: ~88 ms) plus
        /// Wi-Fi jitter and time to resend a lost packet.
        #[arg(long, default_value_t = 250)]
        delay_ms: u64,
    },
    /// Check setup: settings, logs, autostart, firewall, audio device, receivers.
    Doctor,
    /// Create the firewall rules for this install (run elevated; the installer does this).
    #[command(hide = true)]
    SetupFirewall,
    /// Remove our firewall rules (run elevated; the uninstaller does this).
    #[command(hide = true)]
    RemoveFirewall,
    /// Capture this PC's audio locally (no receiver) and report rate and level.
    CaptureCheck {
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
    /// Stream whatever this PC is playing (default output device) until Ctrl+C.
    Live {
        /// Receiver name (as shown by `list`) or IP address.
        #[arg(long)]
        to: String,
        /// Leave the receiver's volume alone instead of following the Windows volume.
        #[arg(long)]
        no_volume_sync: bool,
        /// PC-side buffer in milliseconds (absorbs capture/send jitter).
        #[arg(long, default_value_t = 25)]
        buffer_ms: u64,
        /// Playout delay in ms: how far ahead of its play time each packet is
        /// sent. Must cover the receiver's processing (HomePod: ~88 ms) plus
        /// Wi-Fi jitter and time to resend a lost packet. Lower is more
        /// real-time; raise it if you hear dropouts. 100 ms verified clean on
        /// HomePod over home Wi-Fi.
        #[arg(long, default_value_t = 100)]
        delay_ms: u64,
        #[command(flatten)]
        advanced: Advanced,
    },
    /// Measure the real delay with a microphone: plays clicks on this PC
    /// (streamed to the speaker as usual) and times when the mic hears them.
    /// Takes over the receiver for each run.
    LatencyTest {
        /// Receiver name (as shown by `list`) or IP address.
        #[arg(long, required_unless_present = "analyze")]
        to: Option<String>,
        /// Analyze a recording made on a phone (WAV) instead of using the PC
        /// mic: the gap between PC-speaker and AirPlay clicks.
        #[arg(long, conflicts_with_all = ["to", "sweep"])]
        analyze: Option<PathBuf>,
        /// No PC mic: just play the clicks so a phone can record them.
        #[arg(long, conflicts_with = "analyze")]
        no_mic: bool,
        #[arg(long, default_value_t = 12)]
        clicks: usize,
        /// Time between clicks; must exceed the delay being measured.
        #[arg(long, default_value_t = 1500)]
        period_ms: u64,
        #[arg(long, default_value_t = 25)]
        buffer_ms: u64,
        /// Playout delay (see `live`).
        #[arg(long, default_value_t = 100)]
        delay_ms: u64,
        #[command(flatten)]
        advanced: Advanced,
        /// Measure every combination of latencyMin 250/100/50 ms and delay
        /// 100/50/0 ms, one session each.
        #[arg(long)]
        sweep: bool,
    },
}

/// Experimental stream parameters.
#[derive(clap::Args, Clone, Copy)]
struct Advanced {
    /// `latencyMin` asked of the receiver in SETUP, in ms. Receivers may add
    /// buffering of their own up to this; lower can cut real delay.
    #[arg(long, default_value_t = 250)]
    latency_min_ms: u64,
    /// Send uncompressed ALAC frames instead of raw PCM (for receivers that
    /// refuse PCM).
    #[arg(long)]
    alac: bool,
}

impl Advanced {
    fn codec(self) -> airplay::stream::Codec {
        if self.alac {
            airplay::stream::Codec::AlacUncompressed
        } else {
            airplay::stream::Codec::Pcm
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    simonaircast::runtime::build()?.block_on(run(Cli::parse()))
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::List { timeout } => list(Duration::from_secs(timeout)).await,
        Command::Pair { to } => pair(&to).await,
        Command::Session { to, seconds } => session(&to, Duration::from_secs(seconds)).await,
        Command::CaptureCheck { seconds } => capture_check(seconds).await,
        Command::Doctor => doctor().await,
        Command::SetupFirewall => {
            let exe = std::env::current_exe().context("locating simonaircast.exe")?;
            let dir = exe.parent().context("install folder")?;
            simonaircast::system::setup_firewall(dir)?;
            println!("firewall rules created for {}", dir.display());
            Ok(())
        }
        Command::RemoveFirewall => {
            simonaircast::system::remove_firewall();
            println!("firewall rules removed");
            Ok(())
        }
        Command::Live {
            to,
            no_volume_sync,
            buffer_ms,
            delay_ms,
            advanced,
        } => {
            live(
                &to,
                engine::Settings {
                    delay: Duration::from_millis(delay_ms),
                    buffer: Duration::from_millis(buffer_ms),
                    volume_sync: !no_volume_sync,
                    codec: advanced.codec(),
                    latency_min: Duration::from_millis(advanced.latency_min_ms),
                },
            )
            .await
        }
        Command::LatencyTest {
            to,
            analyze,
            no_mic,
            clicks,
            period_ms,
            buffer_ms,
            delay_ms,
            advanced,
            sweep,
        } => {
            let period = Duration::from_millis(period_ms);
            if let Some(path) = analyze {
                return analyze_recording(&path, period);
            }
            let base = engine::Settings {
                delay: Duration::from_millis(delay_ms),
                buffer: Duration::from_millis(buffer_ms),
                volume_sync: true,
                codec: advanced.codec(),
                latency_min: Duration::from_millis(advanced.latency_min_ms),
            };
            let runs: Vec<engine::Settings> = if sweep {
                [250, 100, 50]
                    .into_iter()
                    .flat_map(|min| {
                        let base = &base;
                        [100, 50, 0].into_iter().map(move |delay| engine::Settings {
                            delay: Duration::from_millis(delay),
                            latency_min: Duration::from_millis(min),
                            ..base.clone()
                        })
                    })
                    .collect()
            } else {
                vec![base]
            };
            let probe = audio_capture::probe::ProbeConfig {
                clicks,
                period,
                record: !no_mic,
                ..Default::default()
            };
            latency_test(&to.context("--to is required")?, &runs, &probe).await
        }
        Command::Play {
            to,
            file,
            beeps,
            volume,
            delay_ms,
        } => play(&to, file, beeps, volume, Duration::from_millis(delay_ms)).await,
    }
}

async fn play(
    target: &str,
    file: Option<PathBuf>,
    beeps: Option<u64>,
    volume: Option<f64>,
    latency: Duration,
) -> Result<()> {
    let (mut source, length): (Box<dyn airplay::stream::PcmSource>, f64) = match (file, beeps) {
        (Some(path), _) => {
            let wav = sources::WavSource::open(&path)?;
            let secs = wav.duration_secs;
            (Box::new(wav), secs)
        }
        (None, Some(secs)) => (Box::new(sources::Beeps::new(secs)), secs as f64),
        (None, None) => unreachable!("clap requires a file or --beeps"),
    };

    let _timer = HighResTimer::enter();
    let addr = resolve(target).await?;
    let session = airplay::session::Session::connect(addr, "simonaircast")
        .await
        .context("setting up session")?;
    println!(
        "playing {length:.0} s to {addr} (receiver latency {} ms); Ctrl+C to stop",
        session
            .stream()
            .latency_ms
            .map_or("?".into(), |l| l.to_string())
    );
    if let Some(db) = volume {
        session.set_volume(db).await.context("setting volume")?;
    }

    let mut last_report = 0;
    let result = tokio::select! {
        r = session.play(source.as_mut(), latency, |s| {
            let secs = s.elapsed.as_secs();
            if secs >= last_report + 10 {
                last_report = secs;
                println!(
                    "  {secs:>4}s  packets {}  worst lag {} ms  >100ms late {}  retransmit requests {} (resent {})",
                    s.packets_sent,
                    s.max_lag.as_millis(),
                    s.late_packets,
                    s.retransmit_requests,
                    s.packets_resent
                );
            }
        }) => Some(r),
        _ = tokio::signal::ctrl_c() => None,
    };
    let timing = session.timing_requests_answered();
    session.teardown().await.context("TEARDOWN")?;

    match result {
        Some(stats) => {
            let s = stats.context("streaming")?;
            println!(
                "done in {:.1} s: {} packets, worst lag {} ms, {} >100ms late, {} retransmit requests ({} resent), {timing} timing replies",
                s.elapsed.as_secs_f64(),
                s.packets_sent,
                s.max_lag.as_millis(),
                s.late_packets,
                s.retransmit_requests,
                s.packets_resent
            );
        }
        None => println!("stopped"),
    }
    Ok(())
}

async fn doctor() -> Result<()> {
    use simonaircast::settings::AppSettings;
    use simonaircast::system;

    let mark = |ok: bool| if ok { "ok  " } else { "MISS" };
    println!("settings   {}", AppSettings::path().display());
    let settings = AppSettings::load();
    println!(
        "           speaker {:?}, delay {} ms, buffer {} ms, Windows volume sync {}",
        settings.speaker, settings.delay_ms, settings.buffer_ms, settings.volume_sync
    );
    println!("logs       {}", simonaircast::logging::log_dir().display());
    println!("[{}] start with Windows", mark(system::autostart_enabled()));
    let rule = tokio::task::spawn_blocking(system::firewall_rule_exists).await?;
    println!(
        "[{}] firewall rule \"{}\" (UDP {}-{} from local subnet)",
        mark(rule),
        system::FIREWALL_RULE,
        system::TIMING_PORT,
        system::CONTROL_PORT
    );

    match audio_capture::Capture::start(audio_capture::CaptureConfig::default()) {
        Ok(capture) => {
            let i = capture.info();
            println!(
                "[ok  ] capture \"{}\" ({} Hz, {} ch, {})",
                i.name, i.sample_rate, i.channels, i.format
            );
        }
        Err(e) => println!("[MISS] capture: {e}"),
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let watcher = audio_capture::volume::VolumeWatcher::start(move |v| {
        let _ = tx.send(v);
    });
    if let Ok(v) = rx.recv_timeout(Duration::from_secs(2)) {
        println!(
            "[ok  ] Windows volume {:.0}% ({:.1} dB){}{} -> receiver {:.1} dB, gain x{:.2}",
            v.level * 100.0,
            v.level_db,
            if v.muted { ", muted" } else { "" },
            if v.hardware { ", hardware volume" } else { "" },
            audio_capture::volume::airplay_volume_db(&v),
            audio_capture::volume::compensation_gain(&v)
        );
    }
    drop(watcher);

    let devices = discover(Duration::from_secs(3)).await?;
    println!(
        "receivers  {}",
        if devices.is_empty() { "none found" } else { "" }
    );
    for d in &devices {
        let addr = d
            .addresses
            .iter()
            .find(|ip| ip.is_ipv4())
            .map(ToString::to_string);
        let usable = d.has_feature(features::TRANSIENT_PAIRING);
        println!(
            "  [{}] {} [{}] {}",
            if usable { "ok  " } else { "??  " },
            d.name,
            d.model,
            addr.unwrap_or_default()
        );
    }
    Ok(())
}

async fn capture_check(seconds: u64) -> Result<()> {
    let mut capture = audio_capture::Capture::start(audio_capture::CaptureConfig::default())
        .context("starting system audio capture")?;
    let info = capture.info().clone();
    println!(
        "capturing \"{}\" ({} Hz, {} ch, {})",
        info.name, info.sample_rate, info.channels, info.format
    );
    let mut buf = vec![0f32; 2 * audio_capture::OUTPUT_RATE as usize];
    for _ in 0..seconds {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let frames = capture.buffered_frames().min(buf.len() / 2);
        let got = capture.read(&mut buf[..frames * 2]);
        let peak = buf[..got * 2].iter().fold(0f32, |m, s| m.max(s.abs()));
        let db = if peak > 0.0 {
            20.0 * peak.log10()
        } else {
            f32::NEG_INFINITY
        };
        println!("  {got:>6} frames/s at 44.1 kHz, peak {db:>6.1} dBFS");
        if let Some(e) = capture.error() {
            bail!("capture failed: {e}");
        }
    }
    Ok(())
}

/// `live` runs on the same engine as the tray app and prints its state.
async fn live(target: &str, settings: engine::Settings) -> Result<()> {
    use engine::{Command, Engine, State};

    let _timer = HighResTimer::enter();
    let engine = Engine::spawn(settings);
    engine.send(Command::Start(target.to_string()));
    let mut state = engine.state.clone();
    let mut stats = engine.stats.clone();
    let mut last_report = 0;
    println!("Ctrl+C to stop");

    loop {
        tokio::select! {
            changed = state.changed() => {
                changed?;
                match state.borrow_and_update().clone() {
                    State::Idle { note } => {
                        println!("idle{}", note.map(|n| format!(": {n}")).unwrap_or_default());
                        return Ok(());
                    }
                    State::Connecting { speaker } => println!("connecting to {speaker} ..."),
                    State::Streaming { speaker, delay_ms } => {
                        println!("streaming to {speaker}, about {delay_ms} ms behind the PC");
                        last_report = 0;
                    }
                    State::Retrying { speaker, reason, retry_in } => {
                        println!("{speaker}: {reason}; retrying in {} s", retry_in.as_secs());
                    }
                }
            }
            changed = stats.changed() => {
                changed?;
                let s = stats.borrow_and_update().clone();
                let secs = s.play.elapsed.as_secs();
                if secs >= last_report + 10 {
                    last_report = secs;
                    println!(
                        "  {secs:>5}s  buffer {:>3} ms  drift corr {:+6.1} ppm  underruns {}  lag {} ms  skipped {}  retransmits {} (expired {})",
                        s.capture.buffered.as_millis(),
                        s.capture.correction_ppm,
                        s.capture.underruns,
                        s.play.max_lag.as_millis(),
                        s.play.skipped_packets,
                        s.play.retransmit_requests,
                        s.play.expired_retransmits
                    );
                }
            }
            _ = tokio::signal::ctrl_c() => {
                engine.send(Command::Stop);
                // Give the engine a moment to tear the session down cleanly.
                let _ = tokio::time::timeout(
                    Duration::from_secs(3),
                    state.wait_for(|s| matches!(s, State::Idle { .. })),
                )
                .await;
                println!("stopped");
                return Ok(());
            }
        }
    }
}

const PHONE_STEPS: &str = "\
Without a PC microphone, record on a phone instead:
  1. Make sure the PC's own speakers (or monitor speakers) are audible, at a
     moderate volume, and put the phone halfway between them and the speaker.
  2. Start a voice recording, then run:  simonaircast latency-test --to <speaker> --no-mic
  3. Copy the recording to the PC as WAV (convert m4a with any converter) and run:
       simonaircast latency-test --analyze recording.wav";

/// One session per settings combination: stream as usual, play clicks,
/// time them on the mic.
async fn latency_test(
    target: &str,
    runs: &[engine::Settings],
    probe: &audio_capture::probe::ProbeConfig,
) -> Result<()> {
    use engine::{Command, Engine, State};
    use simonaircast::latency;

    let _timer = HighResTimer::enter();
    let period = probe.period.as_secs_f64();
    let mut results = Vec::new();
    for settings in runs {
        let label = format!(
            "latencyMin {:>3} ms, delay {:>3} ms{}",
            settings.latency_min.as_millis(),
            settings.delay.as_millis(),
            if settings.codec == airplay::stream::Codec::Pcm {
                ""
            } else {
                ", ALAC"
            }
        );
        println!("== {label}");
        let engine = Engine::spawn(settings.clone());
        engine.send(Command::Start(target.to_string()));
        let mut state = engine.state.clone();
        let up = tokio::time::timeout(
            Duration::from_secs(20),
            state.wait_for(|s| matches!(s, State::Streaming { .. } | State::Idle { .. })),
        )
        .await
        .map(|r| r.map(|s| s.clone()));
        match up {
            Ok(Ok(State::Streaming { .. })) => {}
            Ok(Ok(State::Idle { note })) => bail!("not streaming: {}", note.unwrap_or_default()),
            _ => {
                engine.send(Command::Stop);
                bail!("could not start streaming to {target} within 20 s");
            }
        }
        // Let the drift controller and the receiver's buffer settle.
        tokio::time::sleep(Duration::from_secs(2)).await;

        let config = probe.clone();
        let recorded =
            tokio::task::spawn_blocking(move || audio_capture::probe::record_clicks(&config))
                .await?;
        let stats = engine.stats.borrow().clone();
        engine.send(Command::Stop);
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            state.wait_for(|s| matches!(s, State::Idle { .. })),
        )
        .await;

        let rec = match recorded {
            Ok(rec) => rec,
            Err(e) => {
                if probe.record {
                    eprintln!("{PHONE_STEPS}");
                }
                bail!("probe failed: {e}");
            }
        };
        if rec.mic_name.is_none() {
            println!(
                "   played {} clicks on {}",
                rec.clicks.len(),
                rec.output_name
            );
            continue;
        }
        let wav = save_recording(&rec, results.len())?;
        let m = latency::measure(
            &latency::onsets(&rec.samples, rec.rate),
            &rec.clicks,
            period,
        );
        println!(
            "   mic \"{}\", output \"{}\", saved {}",
            rec.mic_name.as_deref().unwrap_or("?"),
            rec.output_name,
            wav.display()
        );
        println!(
            "   stream: skipped {}, underruns {}, retransmits {} (expired {})",
            stats.play.skipped_packets,
            stats.capture.underruns,
            stats.play.retransmit_requests,
            stats.play.expired_retransmits
        );
        let show = |s: Option<latency::Summary>| s.map_or("not heard".into(), |s| s.to_string());
        println!("   PC speakers       {}", show(m.pc));
        println!("   AirPlay speaker   {}", show(m.airplay));
        println!("   speaker behind PC {}", show(m.gap));
        if m.airplay.is_none() {
            println!("   (no AirPlay clicks heard: is the mic near the speaker, volume up?)");
        }
        results.push((label, m));
    }

    if results.len() > 1 {
        println!("\nsummary (median click -> AirPlay speaker, and behind the PC speakers):");
        for (label, m) in &results {
            let ms = |s: Option<latency::Summary>| {
                s.map_or("   -".into(), |s| format!("{:>4.0}", s.median_ms))
            };
            println!(
                "  {label}   speaker {} ms   behind PC {} ms",
                ms(m.airplay),
                ms(m.gap)
            );
        }
    }
    if !probe.record {
        println!(
            "\nnow stop the phone recording and run: simonaircast latency-test --analyze <file.wav>"
        );
    }
    Ok(())
}

fn save_recording(rec: &audio_capture::probe::ProbeRecording, run: usize) -> Result<PathBuf> {
    let dir = simonaircast::logging::log_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = dir.join(format!("latency-{stamp}-{run}.wav"));
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rec.rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&path, spec)?;
    for &s in &rec.samples {
        w.write_sample(s)?;
    }
    w.finalize()?;
    Ok(path)
}

fn analyze_recording(path: &std::path::Path, period: Duration) -> Result<()> {
    use simonaircast::latency;

    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let all: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<Result<_, _>>()?
        }
    };
    let mono: Vec<f32> = all.iter().step_by(channels).copied().collect();
    let found = latency::onsets(&mono, spec.sample_rate);
    println!(
        "{}: {:.1} s, {} clicks heard",
        path.display(),
        mono.len() as f64 / f64::from(spec.sample_rate),
        found.len()
    );
    match latency::measure_pairs(&found, period.as_secs_f64()) {
        Some(gap) => println!("AirPlay speaker behind the PC speakers: {gap}"),
        None => println!(
            "no click pairs found: both the PC speakers and the AirPlay speaker must be audible"
        ),
    }
    Ok(())
}

async fn session(target: &str, hold: Duration) -> Result<()> {
    let addr = resolve(target).await?;
    let session = airplay::session::Session::connect(addr, "simonaircast")
        .await
        .context("setting up session")?;
    let stream = session.stream();
    println!(
        "session up: data port {}, control port {}, latency {} ms, timing requests answered {}",
        stream.data_port,
        stream.control_port,
        stream.latency_ms.map_or("?".into(), |l| l.to_string()),
        session.timing_requests_answered()
    );

    // Arbitrary start position; it becomes meaningful once audio is sent (M4).
    session.record(0, 0).await.context("RECORD/FLUSH")?;
    println!("RECORD + FLUSH OK");

    // Re-apply the receiver's current volume so the test changes nothing audible.
    if let Some(volume) = session
        .info()
        .get("initialVolume")
        .and_then(|v| v.as_real())
    {
        session
            .set_volume(volume)
            .await
            .context("SET_PARAMETER volume")?;
        println!("volume set to {volume:.1} dB (receiver's current level)");
    }

    let start = std::time::Instant::now();
    while start.elapsed() < hold {
        tokio::time::sleep(Duration::from_secs(5).min(hold - start.elapsed())).await;
        println!(
            "  {:>2}s: timing answered {}, events {}",
            start.elapsed().as_secs(),
            session.timing_requests_answered(),
            session.events_received()
        );
    }

    session.teardown().await.context("TEARDOWN")?;
    println!("TEARDOWN OK");
    Ok(())
}

async fn list(timeout: Duration) -> Result<()> {
    let devices = discover(timeout).await?;
    if devices.is_empty() {
        println!("No AirPlay receivers found.");
        return Ok(());
    }

    for d in &devices {
        let addr = d
            .addresses
            .first()
            .map(ToString::to_string)
            .unwrap_or_default();
        let caps: Vec<&str> = [
            (features::TRANSIENT_PAIRING, "transient-pairing"),
            (features::HOMEKIT_PAIRING, "homekit-pairing"),
            (features::BUFFERED_AUDIO, "buffered-audio"),
            (features::PTP_CLOCK, "ptp"),
        ]
        .into_iter()
        .filter(|(bit, _)| d.has_feature(*bit))
        .map(|(_, label)| label)
        .collect();

        println!("{}  [{}]", d.name, d.model);
        println!("    {addr}:{}  os {}  id {}", d.port, d.os_version, d.id);
        println!("    features 0x{:016X}  {}", d.features, caps.join(", "));
    }
    Ok(())
}

async fn pair(target: &str) -> Result<()> {
    let addr = resolve(target).await?;
    let mut conn = Connection::connect(addr)
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    println!("connected to {addr}");

    airplay::pairing::pair_transient(&mut conn)
        .await
        .context("transient pairing")?;
    println!("paired (transient), control channel encrypted");

    let info = conn
        .request_ok(Request::new("GET", "/info", Protocol::Rtsp10))
        .await
        .context("encrypted GET /info")?
        .plist()?;
    let get = |key: &str| {
        info.as_dictionary()
            .and_then(|d| d.get(key))
            .and_then(|v| v.as_string())
            .unwrap_or("?")
            .to_string()
    };
    println!(
        "encrypted GET /info OK: {} [{}], build {}, AirTunes {}",
        get("name"),
        get("model"),
        get("osBuildVersion"),
        get("sourceVersion")
    );
    Ok(())
}
