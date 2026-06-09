# Vanilla 1.21.11 benchmark driver: boots the server, runs two physics
# workloads under /debug profiling, prints the profiling summary lines.
# Workloads mirror examples/bench_vs_vanilla.rs on the causal engine:
#   W1: 441 water sources on a 6-block grid on a sandstone platform
#   W2: 10,000 sand blocks dropped 29 blocks (falling-block entities)

$ErrorActionPreference = 'Stop'
$dir = $PSScriptRoot
$log = Join-Path $dir 'logs\latest.log'
if (Test-Path $log) { Remove-Item $log -Force }

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = 'java'
$psi.Arguments = '-Xmx4G -jar server.jar nogui'
$psi.WorkingDirectory = $dir
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$proc = [System.Diagnostics.Process]::Start($psi)
# Drain (and discard) pipes so the server never blocks on logging; we
# read logs/latest.log instead.
$proc.BeginOutputReadLine()
$proc.BeginErrorReadLine()

function Send([string]$cmd) {
    $proc.StandardInput.WriteLine($cmd)
    $proc.StandardInput.Flush()
}

function WaitLog([string]$pattern, [int]$timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $log) {
            if (Select-String -Path $log -Pattern $pattern -Quiet) { return $true }
        }
        Start-Sleep -Milliseconds 500
    }
    return $false
}

Write-Host 'Waiting for vanilla server startup...'
if (-not (WaitLog 'Done \(' 300)) {
    Write-Host 'FAILED: server did not start in time'
    try { $proc.Kill() } catch {}
    exit 1
}
Write-Host 'Server up. Preparing arena...'

Send 'forceload add -70 -70 70 70'
Start-Sleep -Seconds 3
Send 'fill -60 0 -60 60 0 60 sandstone'
Start-Sleep -Seconds 3

# --- W1: water grid -------------------------------------------------------
Write-Host 'W1: 441 water sources...'
Send 'debug start'
Start-Sleep -Milliseconds 300
for ($i = 0; $i -le 20; $i++) {
    for ($j = 0; $j -le 20; $j++) {
        $x = -60 + $i * 6
        $z = -60 + $j * 6
        Send "setblock $x 1 $z water"
    }
}
Start-Sleep -Seconds 10
Send 'debug stop'
Start-Sleep -Seconds 2

# Clear the water layer for W2.
Send 'fill -60 1 -60 60 1 60 air'
Start-Sleep -Seconds 5

# --- W2: 10,000 falling sand ----------------------------------------------
Write-Host 'W2: 10,000 falling sand...'
Send 'debug start'
Start-Sleep -Milliseconds 300
Send 'fill -49 30 -49 50 30 50 sand'
Start-Sleep -Seconds 20
Send 'debug stop'
Start-Sleep -Seconds 2

Send 'stop'
if (-not $proc.WaitForExit(60000)) { try { $proc.Kill() } catch {} }

Write-Host ''
Write-Host '=== Vanilla results (from logs/latest.log) ==='
Select-String -Path $log -Pattern 'Stopped tick profiling|ticks per second|Can.t keep up|Running .* behind' |
    ForEach-Object { $_.Line }
