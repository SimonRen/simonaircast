# M0 spike: stream a test tone to a receiver with pyatv (AirPlay 2 / RAOP).
# Usage: .\tools\pyatv-spike.ps1 -Address 192.168.166.12 [-Seconds 10]
# Requires uv (runs pyatv in an isolated environment, nothing installed globally).
param(
    [Parameter(Mandatory)][string]$Address,
    [int]$Seconds = 10,
    # Request ALAC instead of pyatv's default raw PCM (see pyatv_stream.py).
    [switch]$Alac
)
$ErrorActionPreference = 'Stop'

$dir = Join-Path $PSScriptRoot '..\captures'
New-Item -ItemType Directory -Force $dir | Out-Null
$wav = Join-Path $dir 'tone-440hz.wav'

# 44.1 kHz 16-bit stereo, 440 Hz sine at -12 dBFS.
$rate = 44100; $frames = $rate * $Seconds
$data = New-Object byte[] ($frames * 4)
for ($i = 0; $i -lt $frames; $i++) {
    $s = [int16](8000 * [Math]::Sin(2 * [Math]::PI * 440 * $i / $rate))
    $b = [BitConverter]::GetBytes($s)
    $data[$i * 4] = $b[0]; $data[$i * 4 + 1] = $b[1]; $data[$i * 4 + 2] = $b[0]; $data[$i * 4 + 3] = $b[1]
}
$fs = [IO.File]::Create($wav); $w = New-Object IO.BinaryWriter($fs)
$w.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $w.Write([int](36 + $data.Length))
$w.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt ')); $w.Write([int]16); $w.Write([int16]1); $w.Write([int16]2)
$w.Write([int]$rate); $w.Write([int]($rate * 4)); $w.Write([int16]4); $w.Write([int16]16)
$w.Write([Text.Encoding]::ASCII.GetBytes('data')); $w.Write([int]$data.Length); $w.Write($data); $w.Close()

Write-Host "Streaming $Seconds s tone to $Address ..."
# Python 3.12: pyatv calls asyncio.get_event_loop() in places, which fails on 3.14+.
uv run --no-project --python 3.12 --with pyatv (Join-Path $PSScriptRoot 'pyatv_stream.py') $Address $wav $(if ($Alac) { '--alac' })
