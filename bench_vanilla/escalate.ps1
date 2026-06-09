# Escalation: find vanilla's ceiling. 40,000 falling-block entities.
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
$proc.BeginOutputReadLine()
$proc.BeginErrorReadLine()

function Send([string]$cmd) { $proc.StandardInput.WriteLine($cmd); $proc.StandardInput.Flush() }
function WaitLog([string]$pattern, [int]$timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $log) { if (Select-String -Path $log -Pattern $pattern -Quiet) { return $true } }
        Start-Sleep -Milliseconds 500
    }
    return $false
}

if (-not (WaitLog 'Done \(' 300)) { Write-Host 'FAILED to start'; try { $proc.Kill() } catch {}; exit 1 }
Write-Host 'Server up. Preparing...'
Send 'forceload add -70 -70 70 70'
Start-Sleep -Seconds 3
Send 'fill -60 0 -60 60 0 60 sandstone'
Send 'fill -60 1 -60 60 5 60 air'
Start-Sleep -Seconds 4

Write-Host 'W3: 40,000 falling sand entities...'
Send 'debug start'
Start-Sleep -Milliseconds 300
Send 'fill -49 30 -49 50 31 50 sand'
Send 'fill -49 33 -49 50 34 50 sand'
Start-Sleep -Seconds 30
Send 'debug stop'
Start-Sleep -Seconds 2
Send 'stop'
if (-not $proc.WaitForExit(60000)) { try { $proc.Kill() } catch {} }

Write-Host ''
Write-Host '=== Vanilla escalation results ==='
Select-String -Path $log -Pattern 'Stopped tick profiling|Can.t keep up|Running .* behind' | ForEach-Object { $_.Line }
$dump = Get-ChildItem (Join-Path $dir 'debug') -Filter 'profile-results-*.txt' -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($dump) {
    Write-Host '--- profile dump header ---'
    Get-Content $dump.FullName -TotalCount 12
}
