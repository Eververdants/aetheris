# Measures aetheris-service idle footprint (WorkingSet + CPU) over 60s.
# Run elevated:  powershell -ExecutionPolicy Bypass -File scripts/measure-footprint.ps1
param([int]$Seconds = 60, [string]$Config = "aetheris.toml")

$release = Join-Path (Get-Location) "target\release\aetheris-service.exe"
if (-not (Test-Path $release)) { Write-Error "build release first: cargo build --release"; exit 1 }

$p = Start-Process -FilePath $release -ArgumentList "--config", $Config -PassThru
Start-Sleep -Seconds 2
if ($p.HasExited) { Write-Error "service exited early: $($p.ExitCode)"; exit 1 }

$samples = @()
$prevCpu = (Get-Process -Id $p.Id).TotalProcessorTime
$prevT = Get-Date
$samplesCpu = @()
$end = (Get-Date).AddSeconds($Seconds)
while ((Get-Date) -lt $end) {
    Start-Sleep -Milliseconds 2000
    $proc = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
    if (-not $proc) { break }
    $samples += $proc.WorkingSet64
    $cpu = $proc.TotalProcessorTime
    $t = Get-Date
    $dt = ($t - $prevT).TotalSeconds
    $dcpu = ($cpu - $prevCpu).TotalSeconds
    $prevCpu = $cpu; $prevT = $t
    if ($dt -gt 0) { $samplesCpu += ($dcpu / $dt * 100) }
}

Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue

$wsMin = ($samples | Measure-Object -Minimum).Minimum / 1MB
$wsMax = ($samples | Measure-Object -Maximum).Maximum / 1MB
$wsAvg = ($samples | Measure-Object -Average).Average / 1MB
$cpuMax = if ($samplesCpu.Count) { ($samplesCpu | Measure-Object -Maximum).Maximum } else { 0 }
$cpuAvg = if ($samplesCpu.Count) { ($samplesCpu | Measure-Object -Average).Average } else { 0 }

# Adaptation note (task 7): a sample-count line is appended so a measurement
# truncated by an early service exit is visible instead of silently
# under-reporting (expected ~Seconds/2 samples at a 2s interval).
$report = @"
# aetheris v1 acceptance measurement
Date: $(Get-Date -Format o)
Duration: ${Seconds}s idle
Memory (WorkingSet64): min=$('{0:F2}' -f $wsMin)MB avg=$('{0:F2}' -f $wsAvg)MB max=$('{0:F2}' -f $wsMax)MB
CPU: avg=$('{0:F3}' -f $cpuAvg)% max=$('{0:F3}' -f $cpuMax)%
Samples: $($samples.Count) mem / $($samplesCpu.Count) cpu (expected ~$([math]::Floor($Seconds/2)))
Targets: mem<=5MB avg, CPU<0.1% avg
"@

$out = Join-Path (Get-Location) "docs\acceptance-v1.md"
Set-Content -Path $out -Value $report -Encoding UTF8
Write-Host $report
Write-Host "wrote $out"
