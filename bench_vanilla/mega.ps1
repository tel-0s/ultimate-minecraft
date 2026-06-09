# Ceiling hunt: W5 = 14,641-source water sheet dump; W4 = 160,000 falling
# sand entities. Tight debug windows + profiler dump headers for MSPT.
$ErrorActionPreference = 'Stop'
$dir = $PSScriptRoot
$log = Join-Path $dir 'logs\latest.log'
if (Test-Path $log) { Remove-Item $log -Force }

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = 'java'
$psi.Arguments = '-Xmx6G -jar server.jar nogui'
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
Start-Sleep -Seconds 3

Write-Host 'W5: 14,641-source water sheet dumped from y=8...'
Send 'debug start'
Start-Sleep -Milliseconds 300
Send 'fill -60 8 -60 60 8 60 water'
Start-Sleep -Seconds 10
Send 'debug stop'
Start-Sleep -Seconds 2
# Clear all water (sources at y=8, flow y=1..7).
Send 'fill -60 8 -60 60 8 60 air'
for ($y = 1; $y -le 7; $y++) { Send "fill -60 $y -60 60 $y 60 air" }
Start-Sleep -Seconds 6

Write-Host 'W4: 160,000 falling sand entities...'
Send 'debug start'
Start-Sleep -Milliseconds 300
$layers = @(30, 33, 36, 39, 42, 45, 48, 51)
foreach ($y in $layers) {
    $y2 = $y + 1
    Send "fill -49 $y -49 50 $y2 50 sand"
}
Start-Sleep -Seconds 12
Send 'debug stop'
Start-Sleep -Seconds 2
Send 'stop'
if (-not $proc.WaitForExit(90000)) { try { $proc.Kill() } catch {} }

Write-Host ''
Write-Host '=== Vanilla mega results ==='
Select-String -Path $log -Pattern 'Stopped tick profiling|Saved profiling|Can.t keep up|Running .* behind' |
    ForEach-Object { $_.Line }
Write-Host ''
Get-ChildItem (Join-Path $dir 'debug') -Filter '*.txt' -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime | ForEach-Object {
        Write-Host ('--- ' + $_.Name + ' ---')
        Get-Content $_.FullName -TotalCount 10
    }
