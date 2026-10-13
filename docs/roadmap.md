# Roadmap

Each milestone ends with something audible or observable on the real HomePod (书房).

## M0 — Prove the protocol path (spike, no Rust) ✅
Audible on 书房 (2026-10-13). Key lesson: firewall must pass the HomePod's NTP timing requests.

Stream a test WAV to 书房 with pyatv (`tools/pyatv-spike.ps1`).
Confirms AirPlay 2 audio works against OS 27 from this PC before we invest.
Capture the session with `tools/capture-homepod.ps1` as a reference trace.

## M1 — Discovery ✅
`simonaircast list`: `_airplay._tcp` browse, TXT decoding, feature bits.

## M2 — Transient pairing ✅
Transient pair-setup M1–M4 only (SRP-6a, PIN 3939, TLV8) — no pair-verify; the SRP
session key seeds HKDF-SHA512 control keys. `simonaircast pair --to 书房` pairs and
gets an encrypted `GET /info` plist; 10/10 runs OK (2026-10-13).

## M3 — Session setup ✅
`simonaircast session --to 书房`: session SETUP (NTP timing) → event channel → stream
SETUP (type 96, PCM) → feedback every 2 s → RECORD/FLUSH → volume → TEARDOWN, all 200
on HomePod OS 27 (2026-10-13). Timing answered throughout; `updateInfo` events handled.

## M4 — Play a file ✅
Realtime PCM RTP packets (ChaCha20-Poly1305), sync packets every second, retransmits
from a 1000-packet backlog. 2026-10-13 on 书房: ~6 min beep test steady by ear
(43,848 packets, worst lag 19 ms, 0 retransmit requests) and a 6½ min piano recording
(`captures/samples`, via `tools/to_wav.py`) played clean.

## M5 — Live system audio ✅
`simonaircast live --to 书房`: WASAPI loopback (+ silent keep-alive stream) → rubato
resample to 44.1 kHz with a critically damped drift controller → 25 ms PC buffer →
raw PCM RTP with a 100 ms playout delay. ~125 ms behind the PC (was ~2 s with
pyatv's 1.5 s offset). 2026-10-13: 250/150/100 ms runs clean by ear and stats
(0 underruns, 0 retransmits, 3 ms send lag with 1 ms timer resolution).

Latency floor: the HomePod reports ~88 ms internal processing
(`arrivalToRenderLatencyMs`), so ~100 ms total is near the practical minimum.
Notes: loopback is post-Windows-volume (Windows volume scales the HomePod level;
muting Windows silences it). Startup dips the PC buffer to ~11 ms before settling.

## M6 — Usable app (built; at-home verification pending — docs/testing-at-home.md)
Decided 2026-10-13: tray icon + menu (no settings window yet); Windows volume drives the
HomePod volume.

- **M6a Engine** — background service owning the stream: auto-reconnect with backoff
  (receiver drop, Wi-Fi blip), restart on default-audio-device change, Windows-volume
  sync (HomePod volume follows the slider; captured audio gain-compensated unless the
  device has hardware volume), state + stats published for the UI. `live` CLI runs on it.
- **M6b Tray** — tray icon + menu: status line, speaker list (live discovery),
  delay preset (Real-time 100 / Normal 150 / Buffered 250 ms), "Use Windows volume",
  "Start with Windows", Quit.
- **M6c Install** — settings in `%APPDATA%\simonaircast\settings.json`, autostart via
  `HKCU\...\Run`, one-time firewall rule for the receivers' timing/control UDP.
- **M6e Packaging** ✅ — Inno Setup installer: one UAC at install (Program Files,
  firewall port rule + per-exe rules so Windows never shows "allow access?"), Start
  Menu, optional autostart, uninstaller removes rules and offers to delete user data;
  upgrades stop the running tray and remove the old per-user dev copy. Exes carry an
  icon, version info and an asInvoker/PerMonitorV2 manifest. Runtime never elevates
  on its own; the firewall check uses the COM API (no helper process). Verified
  2026-10-13: silent install → uninstall → reinstall, all artefacts checked.
  Not done: code signing (UAC shows "Unknown publisher"; downloaded copies would hit
  SmartScreen) — needs a certificate.
- **M6d Latency polish** — *deferred.* Capture-driven sending would remove the 25 ms
  PC buffer, but the jitter it absorbs (10 ms WASAPI capture periods + 8 ms packets)
  then has to come out of the playout margin instead (100 ms delay vs the HomePod's
  88 ms processing), so the delay setting would need to rise to ~110 ms. Net gain
  ≈ 10 ms (125 → ~115 ms) for real dropout risk. Revisit only if lip-sync still bothers.

Status 2026-10-13 (remote, no audio tests): M6a–M6c implemented, unit-tested, tray
smoke-tested in a scratch profile (starts, discovers 3 receivers, single instance),
installed to `%LOCALAPPDATA%\Programs\simonaircast` without launching.

## Open questions
- Why a program-based firewall allow rule for python.exe did not apply (IP-based rule worked). Matters for how we install our own rule.
- NTP works for realtime audio on OS 27; PTP only needed if we move to buffered audio (type 103).
- Apple TV 客厅 rejects plain OPTIONS with 403 and transient `/pair-setup` with 470
  (Connection Authorization Required). Likely its "Allow Access" setting; otherwise needs
  full HomeKit pair-setup with a PIN + pair-verify.
