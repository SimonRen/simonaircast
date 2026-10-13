# Capture traffic between this PC and one receiver with pktmon (needs admin; prompts UAC).
# Usage: .\tools\capture-homepod.ps1 -Address 192.168.166.12 [-Seconds 15]
# Output: captures\<timestamp>.etl and .pcapng (open in Wireshark).
param(
    [Parameter(Mandatory)][string]$Address,
    [int]$Seconds = 15
)
$dir = (New-Item -ItemType Directory -Force (Join-Path $PSScriptRoot '..\captures')).FullName
$name = Join-Path $dir (Get-Date -Format 'yyyyMMdd-HHmmss')

$script = @"
pktmon filter remove | Out-Null
pktmon filter add AIRPLAY -i $Address | Out-Null
pktmon start --capture --pkt-size 0 --file-name '$name.etl' | Out-Null
Start-Sleep $Seconds
pktmon stop | Out-Null
pktmon etl2pcap '$name.etl' --out '$name.pcapng' | Out-Null
pktmon filter remove | Out-Null
"@
Start-Process pwsh -Verb RunAs -Wait -ArgumentList '-NoProfile', '-Command', $script
Write-Host "Saved $name.pcapng"
