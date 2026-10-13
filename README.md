# simonaircast

Stream Windows system audio to AirPlay 2 speakers (HomePod) — about 125 ms behind
the PC, written in Rust.

Built because TuneBlade stopped working with HomePod software 27 (see
[`docs/findings/2026-10-13-tuneblade-homepod-os27.md`](docs/findings/2026-10-13-tuneblade-homepod-os27.md))
and WinAirCast lags intermittently.

## Install and use

Run `simonaircast-setup-<version>.exe` (build it with `.\tools\build-installer.ps1`;
output in `dist\`). It asks for administrator rights **once**, to install into
Program Files and set up the firewall (inbound UDP 50610–50611 from the local network
for the speaker's timing packets, plus rules for both exes so Windows never asks
"allow access?"). After that nothing prompts: the app runs as you, with no console
windows. Uninstall from Settings → Apps; it removes the firewall rules and asks
whether to keep your settings.

Silent install: `simonaircast-setup-<version>.exe /VERYSILENT /TASKS=autostart`.

Then use the tray icon (Start Menu → simonaircast):

- **Speakers** → pick one to start streaming; pick it again (or **Stop streaming**) to stop.
  The last speaker reconnects automatically next time.
- **Delay** → Real-time 100 ms (default) / Normal 150 / Buffered 250 — raise it if you hear dropouts.
- **Use Windows volume** → volume keys / slider control the speaker.
- **Start with Windows**, **Open log folder**, **Quit**.

Icon colour: grey idle · blue connecting · green streaming · amber reconnecting.

If the firewall rule ever goes missing, the menu shows **Repair firewall rule…**
(that is the only thing in the app that can ask for administrator rights, and only
when you click it).

## Status

| Stage | State |
|---|---|
| mDNS discovery | working |
| Transient pairing + encrypted control channel | working |
| Session setup, NTP timing, event channel, keepalive | working |
| Encrypted RTP audio, sync packets, retransmits | working |
| Live PC audio with drift correction, ~125 ms latency | working |
| Engine: reconnect, device changes, Windows volume sync | built, needs at-home check |
| Tray app, settings, autostart, firewall rule | built, needs at-home check |
| Installer (one UAC at install, none at runtime), exe icon + version info | working |

See [`docs/roadmap.md`](docs/roadmap.md) and [`docs/testing-at-home.md`](docs/testing-at-home.md).

## Layout

```
apps/simonaircast/     CLI (simonaircast.exe) + tray app (simonaircast-tray.exe) + shared lib:
                       engine (stream lifecycle), sources, settings, system (autostart,
                       firewall, single instance), tray, logging, runtime, net
crates/airplay/        AirPlay 2 sender: discovery, pairing, rtsp, timing, events, session, stream
crates/audio-capture/  WASAPI loopback capture, drift-corrected resampling, volume watcher, MMCSS
installer/             Inno Setup script (setup.exe: Program Files, firewall, Start Menu, uninstaller)
assets/                app icon (generated from icon.rs by tools/make_icon.py)
docs/                  roadmap, protocol references, investigation notes, test checklist
tools/                 build-installer, spikes, capture and analysis helpers
```

## CLI

```powershell
simonaircast doctor                          # check settings, firewall, audio device, receivers
simonaircast list                            # AirPlay receivers on the network
simonaircast live --to 书房                  # stream PC audio until Ctrl+C (same engine as the tray)
simonaircast live --to 书房 --delay-ms 150   # more margin if Wi-Fi is poor
simonaircast capture-check                   # is PC audio being captured?
simonaircast play --to 书房 song.wav         # 16-bit 44.1 kHz WAV
simonaircast play --to 书房 --beeps 600      # beep per second, sync test
simonaircast pair --to 书房                  # pairing + encrypted GET /info only
```

## Development

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
```
