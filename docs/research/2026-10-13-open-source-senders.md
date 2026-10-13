# Research: what the active open-source AirPlay 2 senders do (2026-10-13)

Studied read-only (nothing built or run), shallow clones in `C:\work\research`:

| Project | Lang | Licence | Latency it runs | Notes |
|---|---|---|---|---|
| [AirFlash](https://github.com/Ding-Kyoma/AirFlash) | Rust engine + C# UI | GPLv3+ / commercial | 120 / 200 / 500 ms modes | PCM default, real ALAC optional, PTP default, stereo pairs |
| [playfruit](https://github.com/bartleman/playfruit) | Rust (vendored airplay2-rs) | GPLv3+ | 0.5–0.7 s end to end | ALAC, NTP, retransmit-rate health warning |
| [DeetsAirplay](https://github.com/deets-137/DeetsAirplay) | Rust + Tauri | MIT declared in one Cargo.toml only | ≥ 250 ms (+ auto from RTT) | uncompressed-ALAC frames, Siri/touch commands, volume read-back |
| [airplay2-rs](https://github.com/lmcgartland/airplay2-rs) | Rust | GPLv3+ | 200 ms default render delay | ALAC/AAC, PTP + multi-room, JITTER.md field notes |
| [airplay2-sender-cpp](https://github.com/akustikrausch/airplay2-sender-cpp) | C++ | Apache-2.0 | 1.5 s (pyatv) | uncompressed ALAC, tested mostly on Apple TV |
| [OwnTone](https://github.com/owntone/owntone-server) | C | GPLv2+ (pair_ap, libairptp: MIT) | 2.25 s default ("500 ms usually works") | ALAC via ffmpeg, NTP + PTP, multi-room |

simonaircast is MIT: **ideas only, no code copied** from any of them.

## 1. The open question: what sets the HomePod's playout delay?

There are two knobs: `latencyMin` in the stream SETUP, and a delay baked into the
sync packets' RTP↔NTP mapping. The projects contradict each other:

| Source | Claim |
|---|---|
| AirFlash (acoustic probes, `rtp.rs:166-167`) | Receiver applies `latencyMin`; also shifting the sync mapping **doubles** the delay. Sends latencyMin = latencyMax = target, no sync shift. |
| OwnTone (`rtp_common.c:318-328`) | Sync-packet latency fields "don't seem to be used"; latency is communicated via `latencyMin`. |
| playfruit | Only `latencyMin` (250–500 ms); no sync shift. Below ~350 ms "robotizes" on Wi-Fi. |
| DeetsAirplay | Shift = latencyMin = 250 ms; measured ~335 ms total (= 250 + 85 ms HomePod render). Claims < 250 ms drops. |
| airplay2-rs JITTER.md | HomePod **ignores** latencyMin (used 70 ms); the sync offset is the only lever. |
| **simonaircast today** | Sync shift 100 ms **and** latencyMin 11025 (250 ms, inherited from pyatv). |

Our own evidence: pyatv's 1.5 s shift + 250 ms latencyMin felt like "about 2 s";
each later shift reduction (250 → 150 → 100 ms) was reported as better; 100 ms ran
clean. If `latencyMin` is applied too (additive, or as a floor), our real delay is
**~350–470 ms, not ~125 ms** — and lowering `latencyMin` could save up to ~250 ms.
This is the single largest open lever, and it can only be settled by measuring.

**Plan:** a microphone latency test (AirFlash does mic capture with QPC timestamps
and onset correlation), then a matrix on the real HomePod:
latencyMin ∈ {11025, 4410, 2205} × sync shift ∈ {100 ms, 0}, listening for dropouts.

## 2. Other findings, grouped

**Codec.** DeetsAirplay and the C++ sender claim realtime type 96 is always decoded
as ALAC and `ct`/`audioFormat` are ignored. Disproved for our HomePod (AudioAccessory1,1,
AirTunes 980.77.2): raw PCM (ct=1, 0x800) played clean. AirFlash also defaults to PCM.
Cheap hedge: an uncompressed-ALAC framing option (≈20 lines of bit packing) as fallback.

**Event channel replies.** Two sources (C++ README, DeetsAirplay) say replies must be a
bare `200 OK` with only `Server` and `CSeq`; adding `Content-Length: 0` or
`Audio-Latency: 0` (what pyatv and we send) caused "connected but silent" or a corrupted
timeline on some receivers. Ours works today, but `Audio-Latency: 0` may itself be read
as a latency hint. → switch to the bare form.

**Clock.** OwnTone uses CLOCK_MONOTONIC for timing replies, sync and the player;
AirFlash anchors one wall-clock sample at start and advances with `Instant`. We use the
wall clock for timing replies → a Windows time step mid-stream can desync. → adopt.

**Liveness and recovery.**
- playfruit: if `/feedback` fails 3× but NTP timing requests are still arriving, ride out
  the stall instead of reconnecting; zero timing requests after 10 s → firewall warning.
- AirFlash: tiered `/feedback` health (warn at 4 s, fail at 12 s; tolerate 3× 5xx;
  401/403 non-retryable, 454 retryable); reset the retry counter after 10 s healthy.
- AirFlash: if the sender falls > 60 ms behind, **skip** the missed slots (advance
  seq/RTP, force a sync) instead of bursting; drop stale capture.
- AirFlash: retransmits are deadline-aware (don't resend after the packet's play time),
  budgeted per tick, and new media goes out before retransmits.
- OwnTone: HomePods "disconnect for reasons that are not clear" — auto-reconnect after
  5 s is on by default for HomePod; re-arm the 0x90 sync marker after every FLUSH.
- playfruit: discard capture queued during an outage; re-baseline after a > 1 s
  monotonic jump (PC sleep).

**UX.**
- DeetsAirplay: Siri / touch-surface volume changes produce no traffic; poll
  `GET_PARAMETER volume` every 2 s (quiet window after a local change). We could mirror
  it back into the Windows volume.
- DeetsAirplay / OwnTone: HomePod buttons and Siri send `sendMediaRemoteCommand`
  (play, paus, nitm, pitm) on the event channel → map to Windows media keys.
- AirFlash / playfruit: optional "HomePod only" mode that mutes the PC endpoint and
  verifies capture still works (loopback is post-mute on some drivers, pre-mute on others).
- C++ sender: push an explicit volume at stream start so a receiver sitting muted
  doesn't look like "connected but silent".

**Network.** None of them uses `WlanSetInterface` (media streaming mode / background scan)
or Windows QoS; playfruit and airplay2-rs mark DSCP EF only on Unix. Those remain our
own ideas from the earlier research.

**Multi-room / stereo pairs.** Needs PTP (BMCA, SETPEERS, timingPeerInfo) — AirFlash,
airplay2-rs and OwnTone implement it; OwnTone's `libairptp` is MIT. Separate project.

**Where we're already ahead.** PI drift controller (others: P-only, sample drop/dup, or
none), 64-bit nonce counter (airplay2-rs repeats nonces every 65,536 packets), Windows
volume sync with gain compensation, takeover handling, installer with firewall rules,
MMCSS on all sender threads, silent keep-alive render stream (DeetsAirplay does the same).
