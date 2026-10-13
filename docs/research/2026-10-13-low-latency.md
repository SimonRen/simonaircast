# Research: lower latency and fewer dropouts (2026-10-13)

Question: is there a "magic algorithm" that gets AirPlay to a HomePod near
real-time, as some products claim — and what would make simonaircast more stable?

## Bottom line

- There is no magic protocol trick. Every sender (TuneBlade's "real-time mode",
  WinAirCast's "real-time streaming", AirFlash, playfruit) does the same thing we
  do: shrink the playout delay towards the receiver's floor. The rest is marketing.
- The floor is the **receiver**, not the network or the PC. The HomePod reports
  `arrivalToRenderLatencyMs: 88`; TuneBlade's own docs list ~92 ms as the minimum
  buffer for AirPlay speakers. Our home Wi-Fi adds only a few ms (measured below).
- simonaircast is already at the aggressive end (~125 ms budget). Realistic
  remaining gain from the protocol path: **~20 ms** (to ~105 ms).
- The way products make it *feel* real-time is different: **delay the video** to
  match the audio (Airfoil's video player, the "Frame Sync" browser add-on). That
  gives perfect lip-sync at any latency and is the biggest perceived win.
- The biggest *stability* risk is Windows Wi-Fi **background scanning**: 50–400 ms
  stalls every few minutes — far larger than our ~12 ms margin at a 100 ms delay.

## Where our latency goes

| Stage | Time | Source |
|---|---|---|
| Windows audio engine → loopback capture | ~10 ms (one engine period) | WASAPI shared mode |
| PC buffer (capture ↔ send rhythm) | 25 ms target | `CaptureConfig::target_fill` |
| Wi-Fi PC → HomePod | ~1.5 ms one way (RTT median 3.2 ms) | measured, below |
| HomePod processing (decrypt, buffer, DSP, DAC) | 88 ms (inside the playout delay) | its SETUP reply |
| Playout delay we configure | 100 ms | `--delay-ms` (covers the 88 ms + margin) |
| **Total, PC sound → HomePod sound** | **~135 ms** (estimate, not yet measured acoustically) | |

Note the honest caveat: no project, including this one, publishes an *acoustic*
end-to-end measurement. AirFlash's "120 ms" is a target ("not measurements of
end-to-end acoustic latency"); playfruit measured ~450–500 ms by ear.

### Network measurement (this home, PC on Wi-Fi Intel AX201 → HomePod 书房)

600 ICMP pings at 10/s: 0 lost; RTT min 2.1, median 3.2, p95 6.7, p99 12.4,
max 43.6 ms (first packet, radio wake-up). No background-scan stall in that
60 s window — they occur every few minutes, so a longer log is needed.

## What others do

| Product | Approach | Claimed latency |
|---|---|---|
| TuneBlade | "Real-time mode" = smallest receiver buffer (docs: 5 ms Apple TV, 92 ms AirPlay speakers, 140–350 ms AirPort Express) + ~200 ms "system latency" | 5–350 ms + 100–300 ms |
| WinAirCast | "Real-time streaming — 0 ms extra latency"; Rust; MMCSS Pro Audio; ALAC default, PCM option | ~45 ms (vendor) |
| AirFlash (OSS) | WASAPI loopback, PCM/ALAC, encrypted RTP, PTP; selectable modes | "targets 120 ms" (not measured) |
| playfruit (OSS) | ALAC live channel, 250–300 ms prefill, retransmits, drift regulation; profiles 0.5/0.6/0.7 s | ~450–500 ms (by ear) |
| Airfoil | 2 s AirPlay latency; its video player captures the page and redraws it delayed by exactly the audio latency | perceived lip-sync |
| Frame Sync (Firefox) | holds video frames back by the measured audio delay, any site; per-device calibration | perceived lip-sync |

## Recommendations (ranked)

1. **Measure end-to-end acoustically** before tuning further. A `latency-test`
   command: play clicks through the PC output (which we capture), record them with a
   microphone near the HomePod (PC mic or phone), cross-correlate. Without this every
   number above is a budget, not a fact. *Effort: small–medium.*
2. **Lip-sync by delaying video (browser extension).** A small Chrome/Edge/Firefox
   extension that holds video frames back by simonaircast's current delay, which it
   reads from a localhost endpoint the tray exposes (like the Frame Sync fork does with
   TuneBlade). Makes YouTube/Netflix look perfectly in sync. DRM video may need the
   playback-rate fallback. *Effort: medium. Biggest perceived win.*
3. **Stop Wi-Fi background-scan stalls while streaming.** Call `WlanSetInterface` with
   `wlan_intf_opcode_media_streaming_mode = TRUE` and
   `wlan_intf_opcode_background_scan_enabled = FALSE` when a stream starts (no admin;
   re-apply after reconnect; reverts automatically on disconnect). Driver support
   varies — newer Intel cards (incl. AX201) may ignore it; their driver exposes
   "Global BG Scan Blocking" in some versions (not visible on this PC's driver). Also
   log stalls (send-lag spikes, retransmit bursts) so we can see if they happen.
   *Effort: small.*
4. **Monotonic NTP clock.** Our timing replies use wall-clock time; a Windows Time
   step (w32time) mid-stream desynchronises the receiver → glitch. Anchor NTP time to
   the wall clock once at start and advance it with the monotonic clock. *Small.*
5. **Adaptive safety margin.** Decode the HomePod's `/feedback` reply (55-byte plist
   we currently ignore) and track retransmit requests; if the receiver signals trouble,
   raise the playout delay a step (e.g. 100 → 150 ms) for the session, drop back later.
   This is the only "algorithm" worth the name: low latency when the air is clean,
   robustness when it isn't. *Medium; needs at-home data first.*
6. **QoS priority on Wi-Fi (installer).** A policy-based QoS rule marking our UDP as
   DSCP 46 (EF) so Wi-Fi uses the voice queue (WMM AC_VO) on both hops; requires the
   `HKLM\...\Tcpip\QoS\Do not use NLA = 1` switch on non-domain PCs and a reboot. Helps
   only when the network is busy (others streaming/downloading). *Small, admin-only,
   so it belongs in the installer.*
7. **Capture-driven sending (M6d).** Send packets as soon as audio arrives instead of
   on a fixed clock; PC buffer 25 ms → <8 ms, with the delay setting raised slightly to
   keep the margin. Net ~15–20 ms. *Medium; do after (1) so the gain is measurable.*
8. **Probe for a lower receiver floor.** The HomePod's `/info` lists a separate
   `lowLatencyAudioStream` format set (AAC-ELD bits — the screen-mirroring codec). An
   experiment: SETUP variants (formats/`audioMode`) and read `arrivalToRenderLatencyMs`
   from the reply. If any variant reports < 88 ms, it could be worth an encoder.
   *Small experiment (takes over the speaker briefly); unverified lead.*
9. **Long-term: a virtual audio device that reports its latency.** Players (Chrome,
   VLC) sync video to the audio device's clock; a simonaircast output device whose
   clock reflects what the HomePod is playing would give system-wide lip-sync with no
   extension. Requires a signed kernel driver (EV certificate + Microsoft attestation).
   *Large; out of scope for now.*

## Not worth it

- **Hardware encoding**: we send uncompressed PCM — zero encode time. Encoders add delay.
- **Buffered AAC (type 103) + PTP**: designed for multi-room/music, typically ~0.5 s.
- **Proactive packet duplication**: our measured loss is 0 %; retransmits suffice.

## Sources

- TuneBlade, [RealTime Streaming Mode](http://www.tuneblade.com/support/documentation/3.html), [FAQ](http://www.tuneblade.com/support/faqs.html)
- [AirFlash](https://github.com/Ding-Kyoma/AirFlash), [playfruit](https://github.com/bartleman/playfruit), [airplay2-rs AIRPLAY_2_SPEC.md](https://github.com/lmcgartland/airplay2-rs/blob/master/AIRPLAY_2_SPEC.md)
- WinAirCast: [Product Hunt](https://www.producthunt.com/p/winaircast/winaircast), [sspai review](https://sspai.com/post/113002)
- Lip-sync: [Frame Sync – AirPlay](https://addons.mozilla.org/en-US/firefox/addon/frame-sync-airplay/), [Airfoil Video Player 4](https://weblog.rogueamoeba.com/2010/12/17/whats-new-in-airfoil-video-player-4/)
- Wi-Fi scanning: [WLAN_INTF_OPCODE](https://learn.microsoft.com/en-us/windows/win32/api/wlanapi/ne-wlanapi-wlan_intf_opcode), [moonlight-qt #1368](https://github.com/moonlight-stream/moonlight-qt/issues/1368), [#1839](https://github.com/moonlight-stream/moonlight-qt/issues/1839), [Intel community](https://community.intel.com/t5/Wireless/Intel-Dual-Band-Wireless-AC-3160-lag-spikes-streaming-mode/td-p/363974)
- QoS: [QOS_TRAFFIC_TYPE](https://learn.microsoft.com/en-us/windows/win32/api/qos2/ne-qos2-qos_traffic_type), [Teams QoS](https://learn.microsoft.com/en-us/microsoftteams/qos-in-teams-clients), [Cisco: DSCP on Windows](https://cisco.com/c/de_de/support/docs/quality-of-service-qos/qos-configuration-monitoring/221868-enable-dscp-qos-tagging-on-windows-machi.pdf), [CableLabs: WMM impacts](https://www-res.cablelabs.com/wp-content/uploads/2024/04/10100006/Impacts-of-WMM-on-Wi-Fi.pdf)
- [IAudioClient::GetStreamLatency](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-getstreamlatency)
