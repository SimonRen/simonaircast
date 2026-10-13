# TuneBlade vs HomePod OS 27 (2026-10-13)

Investigation that started this project. Devices: HomePod 书房 (`AudioAccessory1,1`),
Apple TV 4K 客厅 (`AppleTV14,1`), both `ov=27.0`, AirTunes `980.77.2`. Sender: TuneBlade 1.8.0 (2018).

## 1. User-Agent filtering
OS 27 receivers reject RTSP `OPTIONS` unless the User-Agent looks like Apple software.

| User-Agent | 书房 reply |
|---|---|
| `TuneBlade/1.0 (Windows)` | 403 Forbidden |
| (none) | 403 |
| `Music/1.0 (Windows)` | 403 |
| `iTunes/12.0.1 (Windows)` | 200 OK |
| `AirPlay/320.2 (Windows)` | 200 OK |

**Implication for us:** send an `AirPlay/<version>` User-Agent on every request.

## 2. Legacy (AirPlay 1 / RAOP) session is accepted but silent
With TuneBlade patched to send `iTunes/12.0.1`, the full legacy handshake succeeded
(OPTIONS → ANNOUNCE (ALAC, unencrypted) → SETUP → RECORD → SET_PARAMETER, all 200).
A 15 s packet capture showed audio RTP (~125 pkt/s, 300–720 B), sync packets and
NTP timing requests/replies all flowing — yet no sound.

SETUP reply: `server_port=…;control_port=…;timing_port=0`.

**Implication:** legacy RAOP is not a viable path on OS 27; implement real AirPlay 2
(transient pairing + encrypted control + encrypted audio).

## 3. Apple TV 客厅
Rejects even a plain `OPTIONS` with an `AirPlay/` User-Agent (403). Unresolved.

## 4. M0 spike: AirPlay 2 works — once timing gets through the firewall
pyatv (`tools/pyatv-spike.ps1`) streamed an audible tone to 书房:
transient pairing (`/pair-pin-start`, `/pair-setup`, `/pair-verify`) → SETUP
(`timingProtocol: NTP`, reply `eventPort`) → stream SETUP (`type 96`, `ct 1` PCM,
`audioFormat 0x800`, reply `arrivalToRenderLatencyMs 88`) → RECORD → RTP.

Before that it hung: **the HomePod does not answer the stream SETUP until NTP
timing has synced**, and it gives no error. The Windows firewall (Wi-Fi on the
Public profile) dropped the HomePod's timing requests (`pfirewall.log`: `DROP UDP
192.168.166.12 → <timing port>`).

- A program-based allow rule for the exact owning `python.exe` (verified via
  `Get-NetUDPEndpoint`) did **not** take effect — unexplained.
- A rule allowing inbound UDP from the receiver's IP fixed it immediately.

Timing request seen: `80 d2 00 07`, zero reference/receive times, send time
`0x83AA7E80.37AA017E` (NTP epoch + ~0.2 s, i.e. receiver monotonic clock), resent ~8×/s
while unanswered.

**Implications for us:**
- Our timing server must answer NTP requests (`0xD3` replies) from the first SETUP on.
- Ship/install a firewall rule; detect drops (no timing request seen within ~1 s → tell the user).
- Raw PCM is fine for realtime streams; ALAC/AAC optional.

## 5. Ruled out
Stale TuneBlade config, firewall, Tailscale, the router's fake-IP DNS (198.18.x),
Home-app access control (no password set; WinAirCast works).
