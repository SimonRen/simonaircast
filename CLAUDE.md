# simonaircast

AirPlay 2 sender for Windows in Rust: capture system audio, stream to HomePods.

## Commands
- Build/lint/test: `cargo fmt --all; cargo clippy --workspace --all-targets; cargo test --workspace`
- Installer: `.\tools\build-installer.ps1` → `dist\simonaircast-setup-<ver>.exe` (needs `scoop install extras/inno-setup`). Version comes from the workspace `Cargo.toml`.
- Icon: drawn in `apps/simonaircast/src/icon.rs`; regenerate `assets/simonaircast.ico` with `tools/make_icon.py` (see its docstring).
- Diagnose: `simonaircast doctor`; discover: `simonaircast list`; stream: `simonaircast live --to 书房`

## Layout
- `crates/airplay` — protocol library, one module per stage: discovery → pairing → rtsp → timing/events → session → stream. Keep it free of Windows-specific code.
- `crates/audio-capture` — WASAPI loopback, resampling + drift controller, volume watcher, MMCSS priority. Windows-only code lives here.
- `apps/simonaircast` — lib (engine, sources, settings, system, tray, logging, runtime, net) + two bins: `simonaircast` (CLI) and `simonaircast-tray` (windows subsystem). `live` and the tray share `engine`.

## Conventions
- Library errors: `thiserror` enums. App: `anyhow`. Engine decides retry vs stop via `permanent_failure`.
- Logging via `tracing`; protocol messages at `debug`, raw bytes at `trace`. Tray logs to `%LOCALAPPDATA%\simonaircast\logs`.
- Protocol facts come from reverse-engineering docs (`docs/references.md`) and our captures (`docs/findings/`); cite the source in a comment when encoding a non-obvious constant.
- Pin protocol behaviour with unit tests using real captured values (see `discovery.rs`, `timing.rs` tests).
- Crypto is cross-checked against pyatv/srptools: regenerate `*_vectors.rs` with `tools/srp_vectors.py` and `tools/hap_vectors.py` (via `uv run --no-project --python 3.12 --with <pkg>`); never hand-edit them.

## Gotchas learned the hard way
- HomePod OS 27 rejects non-Apple User-Agents (403) and silently hangs the stream SETUP until its NTP timing requests are answered — a firewall dropping them looks like a hang.
- Windows firewall program-based rules didn't apply for our dev python; we use a port rule (UDP 50610–50611, fixed ports in `system.rs`). Program rules exist only to suppress the "allow access?" dialog.
- Only the installer elevates (it runs `simonaircast.exe setup-firewall`). The app must never trigger UAC or show a console on its own; spawn helpers with `CREATE_NO_WINDOW`.
- Sending threads need MMCSS "Pro Audio" priority and 1 ms timer resolution, or CPU load causes dropouts / bursty packets.
- Loopback capture is post-Windows-volume; volume sync compensates by the endpoint's dB level (not the slider scalar).
- Don't take over the HomePod (session/live/play) without asking — it interrupts whatever is playing at home.
- The engine starts capture *before* connecting, so a missing audio device never grabs the speaker; `Session` cleans up in `Drop` (abort keepalive + best-effort TEARDOWN) — an early `?` once leaked a keepalive task per retry.

## Test environment
- Receivers (IPs are DHCP — use `list`): 书房 = HomePod `AudioAccessory1,1`, 客厅 = Apple TV 4K `AppleTV14,1` (refuses with 470), both OS 27.0.
- Packet capture needs admin: `tools/capture-homepod.ps1`. Over Remote Desktop there is no audio output device.
