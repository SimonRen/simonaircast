//! Background streaming service: owns the session + capture lifecycle so the
//! UI only sends commands and renders state.
//!
//! - Reconnects with backoff after network errors, device changes or a
//!   receiver that stopped answering.
//! - Does *not* reconnect when the receiver ended the session itself (another
//!   device took over); fighting the user's phone for the speaker is worse.
//! - Optionally lets the Windows volume drive the receiver's volume.

use std::time::{Duration, Instant};

use airplay::session::{PlayStats, Session};
use anyhow::{Context, Result, anyhow, bail};
use audio_capture::volume::{OutputVolume, VolumeWatcher, airplay_volume_db, compensation_gain};
use audio_capture::{Capture, CaptureConfig, CaptureStats};
use tokio::sync::{mpsc, watch};

use crate::sources::LiveSource;

const SENDER_NAME: &str = "simonaircast";
/// A stream that ran this long before failing counts as healthy: retry from
/// the shortest backoff again.
const HEALTHY_RUN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Playout delay (see `live --delay-ms`).
    pub delay: Duration,
    /// PC-side capture buffer.
    pub buffer: Duration,
    /// Receiver volume follows the Windows volume.
    pub volume_sync: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            delay: Duration::from_millis(100),
            buffer: Duration::from_millis(25),
            volume_sync: true,
        }
    }
}

#[derive(Debug)]
pub enum Command {
    /// Stream to the receiver with this name (replaces any current stream).
    Start(String),
    Stop,
    Update(Settings),
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Idle {
        note: Option<String>,
    },
    Connecting {
        speaker: String,
    },
    Streaming {
        speaker: String,
        delay_ms: u64,
    },
    Retrying {
        speaker: String,
        reason: String,
        retry_in: Duration,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub capture: CaptureStats,
    pub play: PlayStats,
}

pub struct Engine {
    commands: mpsc::UnboundedSender<Command>,
    pub state: watch::Receiver<State>,
    pub stats: watch::Receiver<Stats>,
}

impl Engine {
    /// Spawns the engine on the current tokio runtime.
    pub fn spawn(settings: Settings) -> Self {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(State::Idle { note: None });
        let (stats_tx, stats) = watch::channel(Stats::default());
        let (volume_tx, volume_rx) = watch::channel(None);
        let watcher = VolumeWatcher::start(move |v| {
            let _ = volume_tx.send(Some(v));
        });
        tokio::spawn(run(
            settings, command_rx, state_tx, stats_tx, volume_rx, watcher,
        ));
        Self {
            commands,
            state,
            stats,
        }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

async fn run(
    mut settings: Settings,
    mut commands: mpsc::UnboundedReceiver<Command>,
    state: watch::Sender<State>,
    stats: watch::Sender<Stats>,
    mut volume: watch::Receiver<Option<OutputVolume>>,
    _watcher: VolumeWatcher,
) {
    let mut speaker: Option<String> = None;
    let mut failures = 0u32;

    loop {
        let Some(name) = speaker.clone() else {
            match commands.recv().await {
                Some(cmd) => apply(cmd, &mut speaker, &mut settings),
                None => return,
            }
            continue;
        };

        state.send_replace(State::Connecting {
            speaker: name.clone(),
        });
        let started = Instant::now();
        let error = match stream(&name, &settings, &state, &stats, &mut volume, &mut commands).await
        {
            Ok(Interrupted::By(cmd)) => {
                // A command changed what we should do; the stream was torn down
                // cleanly, act on it right away.
                apply(cmd, &mut speaker, &mut settings);
                failures = 0;
                if speaker.is_none() {
                    state.send_replace(State::Idle { note: None });
                }
                continue;
            }
            Ok(Interrupted::ChannelClosed) => return,
            Ok(Interrupted::SourceEnded) => anyhow!("stream ended"),
            Err(e) => e,
        };

        if let Some(note) = permanent_failure(&name, &error) {
            tracing::info!(speaker = %name, error = format!("{error:#}"), "not reconnecting");
            speaker = None;
            state.send_replace(State::Idle { note: Some(note) });
            continue;
        }

        if started.elapsed() >= HEALTHY_RUN {
            failures = 0;
        }
        failures += 1;
        let wait = backoff(failures);
        tracing::warn!(speaker = %name, error = format!("{error:#}"), ?wait, "stream failed; retrying");
        state.send_replace(State::Retrying {
            speaker: name,
            reason: format!("{error:#}"),
            retry_in: wait,
        });
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            cmd = commands.recv() => match cmd {
                Some(cmd) => { apply(cmd, &mut speaker, &mut settings); failures = 0; }
                None => return,
            },
        }
        if speaker.is_none() {
            state.send_replace(State::Idle { note: None });
        }
    }
}

/// Errors where retrying can't help (or would be rude): returns the note to
/// show instead. Everything else is treated as transient.
fn permanent_failure(name: &str, error: &anyhow::Error) -> Option<String> {
    match error.downcast_ref::<airplay::Error>()? {
        // Another device took over; reconnecting would fight it.
        airplay::Error::SessionEnded => Some(format!("{name} is playing from another device")),
        // 470 Connection Authorization Required / 403 Forbidden: the receiver
        // needs a PIN/HomeKit pairing or restricts AirPlay access (seen on
        // Apple TV 客厅). Its AirPlay settings have to change first.
        airplay::Error::Status {
            status: 403 | 470, ..
        } => Some(format!(
            "{name} refused the connection; check its AirPlay access settings"
        )),
        _ => None,
    }
}

fn apply(command: Command, speaker: &mut Option<String>, settings: &mut Settings) {
    match command {
        Command::Start(name) => *speaker = Some(name),
        Command::Stop => *speaker = None,
        Command::Update(new) => *settings = new,
    }
}

fn backoff(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 | 1 => 1,
        2 => 2,
        3 => 5,
        _ => 10,
    })
}

/// Why a stream attempt ended without an error.
enum Interrupted {
    /// A command arrived; the stream was torn down cleanly first.
    By(Command),
    /// The engine handle was dropped.
    ChannelClosed,
    /// The audio source ran out (not expected for live capture).
    SourceEnded,
}

/// One streaming attempt: connect, capture, play until something fails or a
/// command arrives.
async fn stream(
    name: &str,
    settings: &Settings,
    state: &watch::Sender<State>,
    stats: &watch::Sender<Stats>,
    volume: &mut watch::Receiver<Option<OutputVolume>>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> Result<Interrupted> {
    // Capture first: if there's no audio device (monitor asleep, Remote
    // Desktop) we fail without ever taking the speaker away from whoever is
    // using it. Audio that piles up while connecting is trimmed to the target
    // fill on the first read.
    let mut capture = Capture::start(CaptureConfig {
        target_fill: settings.buffer,
    })
    .context("starting system audio capture")?;

    let connect = async {
        let addr = crate::net::resolve(name).await?;
        Session::connect_with_ports(addr, SENDER_NAME, crate::system::local_ports())
            .await
            .with_context(|| format!("connecting to {name}"))
    };
    let session = tokio::select! {
        s = connect => s?,
        cmd = commands.recv() => return Ok(cmd.map_or(Interrupted::ChannelClosed, Interrupted::By)),
    };
    wait_for_prefill(&mut capture, settings.buffer).await?;

    let mut source = LiveSource::new(capture);
    let gain = source.gain.clone();
    let capture_stats = source.stats.clone();

    let follow_volume = async {
        if !settings.volume_sync {
            return std::future::pending::<Result<()>>().await;
        }
        loop {
            let current = volume.borrow_and_update().clone();
            if let Some(v) = current {
                session
                    .set_volume(airplay_volume_db(&v))
                    .await
                    .context("setting receiver volume")?;
                gain.store(
                    compensation_gain(&v).to_bits(),
                    std::sync::atomic::Ordering::Relaxed,
                );
                tracing::debug!(level = v.level, db = airplay_volume_db(&v), "volume synced");
            }
            volume.changed().await.context("volume watcher stopped")?;
        }
    };

    state.send_replace(State::Streaming {
        speaker: name.to_string(),
        delay_ms: (settings.delay + settings.buffer).as_millis() as u64,
    });
    let play = session.play(&mut source, settings.delay, |p| {
        let capture = capture_stats.lock().map(|c| c.clone()).unwrap_or_default();
        stats.send_replace(Stats {
            capture,
            play: p.clone(),
        });
    });

    let result = tokio::select! {
        r = play => r.map(|_| Interrupted::SourceEnded).map_err(anyhow::Error::from),
        r = follow_volume => r.map(|()| Interrupted::SourceEnded),
        cmd = commands.recv() => Ok(cmd.map_or(Interrupted::ChannelClosed, Interrupted::By)),
    };
    let capture_error = source.error();
    drop(source);
    // Best effort: the receiver may already be gone.
    let _ = session.teardown().await;

    if let Some(e) = capture_error {
        bail!("audio capture stopped: {e}");
    }
    result
}

/// `&mut` because `Capture` is `Send` but not `Sync` (lock-free SPSC reader).
async fn wait_for_prefill(capture: &mut Capture, buffer: Duration) -> Result<()> {
    let target = (buffer.as_secs_f64() * f64::from(audio_capture::OUTPUT_RATE)) as usize;
    let deadline = Instant::now() + Duration::from_secs(2);
    while capture.buffered_frames() < target {
        if let Some(e) = capture.error() {
            bail!("audio capture failed: {e}");
        }
        if Instant::now() > deadline {
            bail!("no audio arrived from the capture device within 2 s");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_and_takeovers_are_permanent_network_errors_are_not() {
        let refused = anyhow::Error::from(airplay::Error::Status {
            request: "POST /pair-setup".into(),
            status: 470,
        })
        .context("connecting to 客厅");
        assert!(
            permanent_failure("客厅", &refused)
                .unwrap()
                .contains("refused")
        );

        let taken = anyhow::Error::from(airplay::Error::SessionEnded);
        assert!(
            permanent_failure("书房", &taken)
                .unwrap()
                .contains("another device")
        );

        let unreachable = anyhow::Error::from(airplay::Error::ReceiverUnreachable);
        assert!(permanent_failure("书房", &unreachable).is_none());
        assert!(permanent_failure("书房", &anyhow::anyhow!("no receiver named")).is_none());
    }

    #[test]
    fn backoff_grows_then_caps() {
        let waits: Vec<u64> = (1..=6).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(waits, [1, 2, 5, 10, 10, 10]);
    }

    #[test]
    fn commands_update_target_and_settings() {
        let mut speaker = None;
        let mut settings = Settings::default();
        apply(Command::Start("书房".into()), &mut speaker, &mut settings);
        assert_eq!(speaker.as_deref(), Some("书房"));
        let changed = Settings {
            delay: Duration::from_millis(150),
            ..Settings::default()
        };
        apply(
            Command::Update(changed.clone()),
            &mut speaker,
            &mut settings,
        );
        assert_eq!(settings, changed);
        apply(Command::Stop, &mut speaker, &mut settings);
        assert!(speaker.is_none());
    }
}
