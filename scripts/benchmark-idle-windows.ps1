param(
    [int]$DurationSeconds = 1800,
    [string]$Binary = "target/release/rescueloop.exe"
)

$ErrorActionPreference = "Stop"
$MaxCpu = if ($env:RESCUELOOP_MAX_CPU) { [double]$env:RESCUELOOP_MAX_CPU } else { 1.0 }
$MaxRssMiB = if ($env:RESCUELOOP_MAX_RSS_MIB) { [double]$env:RESCUELOOP_MAX_RSS_MIB } else { 30.0 }
$State = Join-Path ([System.IO.Path]::GetTempPath()) ("rescueloop-perf-" + [guid]::NewGuid())
$Stdout = Join-Path $State "watch.out"
$Stderr = Join-Path $State "watch.err"
$Process = $null

function Get-ProcessTree([int]$RootId) {
    $Rows = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId)
    $Ids = [System.Collections.Generic.HashSet[int]]::new()
    [void]$Ids.Add($RootId)
    $Changed = $true
    while ($Changed) {
        $Changed = $false
        foreach ($Row in $Rows) {
            if ($Ids.Contains([int]$Row.ParentProcessId) -and $Ids.Add([int]$Row.ProcessId)) {
                $Changed = $true
            }
        }
    }
    @($Ids | ForEach-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue })
}

try {
    New-Item -ItemType Directory -Force -Path $State | Out-Null
    $PreviousRustLog = $env:RUST_LOG
    $env:RUST_LOG = "info"
    $Process = Start-Process -FilePath $Binary `
        -ArgumentList @("--incident-dir", (Join-Path $State "incidents"), "watch") `
        -RedirectStandardOutput $Stdout -RedirectStandardError $Stderr -PassThru -WindowStyle Hidden
    if ($null -eq $PreviousRustLog) { Remove-Item Env:RUST_LOG } else { $env:RUST_LOG = $PreviousRustLog }

    $Ready = $false
    for ($Attempt = 0; $Attempt -lt 100; $Attempt++) {
        if ($Process.HasExited) { break }
        if ((Test-Path $Stdout) -and (Select-String -Path $Stdout -SimpleMatch "Status: READY" -Quiet)) {
            $Ready = $true
            break
        }
        Start-Sleep -Milliseconds 100
    }
    if (-not $Ready) { throw "watcher did not become ready: $(Get-Content $Stderr -Raw)" }

    $PreviousCpu = @{}
    foreach ($Member in (Get-ProcessTree $Process.Id)) {
        $PreviousCpu[$Member.Id] = $Member.TotalProcessorTime.TotalMilliseconds
    }
    $CpuSum = 0.0
    $PeakCpu = 0.0
    $PeakRssMiB = 0.0
    for ($Sample = 0; $Sample -lt $DurationSeconds; $Sample++) {
        $Interval = [System.Diagnostics.Stopwatch]::StartNew()
        Start-Sleep -Seconds 1
        if ($Process.HasExited) { throw "watcher exited during benchmark" }
        $CurrentCpu = @{}
        $DeltaCpuMs = 0.0
        $RssBytes = 0L
        foreach ($Member in (Get-ProcessTree $Process.Id)) {
            $CurrentMs = $Member.TotalProcessorTime.TotalMilliseconds
            $CurrentCpu[$Member.Id] = $CurrentMs
            $DeltaCpuMs += $CurrentMs - $(if ($PreviousCpu.ContainsKey($Member.Id)) { $PreviousCpu[$Member.Id] } else { 0 })
            $RssBytes += $Member.WorkingSet64
        }
        $PreviousCpu = $CurrentCpu
        $Cpu = ($DeltaCpuMs / $Interval.Elapsed.TotalMilliseconds) * 100.0
        $RssMiB = $RssBytes / 1MB
        $CpuSum += $Cpu
        if ($Cpu -gt $PeakCpu) { $PeakCpu = $Cpu }
        if ($RssMiB -gt $PeakRssMiB) { $PeakRssMiB = $RssMiB }
    }
    $AverageCpu = $CpuSum / $DurationSeconds
    Write-Host ("samples={0} avg_cpu={1:F3}% max_cpu={2:F3}% peak_rss={3:F2}MiB" -f `
        $DurationSeconds, $AverageCpu, $PeakCpu, $PeakRssMiB)
    if ($AverageCpu -ge $MaxCpu) { throw "average CPU budget exceeded" }
    if ($PeakRssMiB -ge $MaxRssMiB) { throw "peak RSS budget exceeded" }
}
finally {
    if ($null -ne $Process -and -not $Process.HasExited) {
        & taskkill.exe /PID $Process.Id /T /F 2>$null | Out-Null
        $Process.WaitForExit()
    }
    if (Test-Path $State) { Remove-Item -Recurse -Force $State }
}
