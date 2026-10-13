# At-home checklist (M6)

Everything below was built and unit-tested remotely, but needs ears and eyes on the
real PC + HomePod. Do it at the PC itself, not over Remote Desktop (an RDP session
has no audio output device).

Start: **turn the Windows volume down to ~20 %** — with "Use Windows volume" the
HomePod follows the Windows level, and the stream is now full-scale (Windows'
attenuation is undone in the captured audio), so it will be louder than before.

## 1. Start
Already installed with the setup program (`C:\Program Files\simonaircast`, firewall
rules in place) — starting it should not show any prompt or console window.
- [ ] Start Menu → simonaircast — tray icon appears (grey speaker); menu shows
      "Firewall: allowed". No UAC, no "allow access?" dialog, no terminal.
- [ ] `& "$env:ProgramFiles\simonaircast\simonaircast.exe" doctor` shows
      `[ok] firewall rule` and `[ok] capture "DELL U2725QE …"`.
- [ ] Task Manager shows "simonaircast" with the blue speaker icon.

## 2. Streaming
- [ ] Menu → Speakers → 书房: icon turns blue, then green; status
      "● Streaming to 书房 (125 ms)". Play YouTube: sound on the HomePod.
- [ ] Lip-sync feels like the earlier 100 ms test.
- [ ] Move the Windows volume slider / volume keys: HomePod follows within ~0.2 s.
      Mute in Windows → HomePod mutes.
- [ ] Delay → Normal (150 ms): brief gap, then streaming again with "(175 ms)".

## 3. Resilience
- [ ] Start AirPlay to 书房 from the iPhone: simonaircast goes grey with
      "书房 is playing from another device" and does **not** grab it back.
- [ ] Switch the Windows output (e.g. to Realtek or headphones) while streaming:
      it reconnects on the new device within a few seconds.
- [ ] Turn the HomePod off/on (or pull its power): icon goes amber
      ("reconnecting in N s") and recovers when it's back.
- [ ] Speakers → 客厅 (Apple TV): stops with "refused the connection; check its AirPlay
      access settings" instead of retrying forever.

## 4. Lifecycle
- [ ] Quit, start again: it reconnects to 书房 by itself.
- [ ] "Start with Windows" → reboot/sign out: tray starts and reconnects, no prompts.
- [ ] Settings → Apps → simonaircast → Uninstall works (then reinstall from `dist\`).
- [ ] Logs: menu → Open log folder → `simonaircast.log` has the session history.

## 5. If it connects but never plays
The development firewall rules are gone, so step 2 is also the first real test of the
port rule (UDP 50610–50611 from the local network). If the icon stays blue/amber and
the log (menu → Open log folder) says "no timing request reached us", the rule isn't
taking effect. Temporary workaround while we look at it (admin PowerShell):

```powershell
New-NetFirewallRule -DisplayName 'simonaircast temp - HomePod UDP in' -Direction Inbound -Protocol UDP -RemoteAddress 192.168.166.12 -Action Allow
```

## 6. Measure the real delay (M7)
Quit the tray app first (it would fight for the speaker). Needs a microphone on the PC
(webcam, headset or laptop mic) set as the default recording device, and audible PC
speakers (the Dell monitor) so the gap to the HomePod can be measured.

```powershell
$exe = "C:\work\simonaircast\target\release\simonaircast.exe"   # until the next installer
& $exe latency-test --to 书房              # current defaults, 12 clicks
& $exe latency-test --to 书房 --sweep      # 9 runs: latencyMin 250/100/50 × delay 100/50/0
```

- [ ] Each run prints "PC speakers", "AirPlay speaker" and **"speaker behind PC"** —
      the last one is what you notice watching a video. Mic recordings are saved in the
      log folder (`latency-*.wav`).
- [ ] Note which combinations stay clean (no clicks missing, `skipped 0`) and which is
      lowest. Listen to music for a minute at the best one: `live --to 书房
      --latency-min-ms <x> --delay-ms <y>`.
- [ ] Delay 0 probably drops audio — that's expected; it tells us whether the HomePod
      buffers `latencyMin` on its own.

No PC mic? Record with a phone halfway between monitor and HomePod, run
`latency-test --to 书房 --no-mic`, then copy the recording as WAV and run
`latency-test --analyze recording.wav`.

## Known limits
- HDMI audio (the Dell monitor) disappears when the monitor sleeps or is off; streaming
  pauses and resumes when it's back. Making the Realtek output the default avoids that.
- With "Use Windows volume", muting Windows mutes the HomePod; to silence only the
  monitor speakers, use the monitor's own buttons.
