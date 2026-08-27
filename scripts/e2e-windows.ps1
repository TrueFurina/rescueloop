$ErrorActionPreference = "Stop"
$Root = Join-Path ([System.IO.Path]::GetTempPath()) ("rescueloop-e2e-" + [guid]::NewGuid())
try {
    New-Item -ItemType Directory -Force -Path $Root | Out-Null
    $Incidents = Join-Path $Root "incidents"
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents run cmd.exe /c exit 42
    if ($LASTEXITCODE -ne 0) { throw "supervised run command failed" }
    $Files = @(Get-ChildItem $Incidents -Filter *.json)
    if ($Files.Count -ne 1) { throw "expected exactly one incident, found $($Files.Count)" }
    $Incident = Get-Content $Files[0].FullName -Raw | ConvertFrom-Json
    if ($Incident.kind -ne "abnormal_exit") { throw "unexpected incident kind: $($Incident.kind)" }
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents sources list
    if ($LASTEXITCODE -ne 0) { throw "sources command failed" }
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents replay (Join-Path $Root "missing.json") 2>$null
    if ($LASTEXITCODE -eq 0) { throw "expected replay failure" }
    $LogFiles = @(Get-ChildItem (Join-Path $Root "logs") -Filter "rescueloop-*.jsonl")
    if ($LogFiles.Count -lt 1) { throw "expected operational log file" }
    $LogRecords = @(Get-Content $LogFiles[-1].FullName | ForEach-Object { $_ | ConvertFrom-Json })
    if (-not ($LogRecords | Where-Object { $_.fields.event -eq "runtime.failed" })) {
        throw "runtime.failed log event not found"
    }
    if ($LogRecords | Where-Object { -not $_.schema_version -or -not $_.run_id -or -not $_.correlation_id }) {
        throw "log context fields are incomplete"
    }
    $env:RESCUELOOP_TEST_PANIC = "1"
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents sources list 2>$null
    Remove-Item Env:RESCUELOOP_TEST_PANIC
    if ($LASTEXITCODE -eq 0) { throw "expected debug panic" }
    $Binary = Join-Path (Get-Location) "target/debug/rescueloop.exe"
    $Parallel = 1..8 | ForEach-Object {
        Start-Process -FilePath $Binary -ArgumentList @("--incident-dir", $Incidents, "sources", "list") -PassThru -WindowStyle Hidden
    }
    $Parallel | Wait-Process
    if ($Parallel | Where-Object { $_.ExitCode -ne 0 }) { throw "parallel logging process failed" }
    $LogRecords = @(& cargo run --quiet -p rescueloop -- --incident-dir $Incidents logs --lines 1000 --output json | ForEach-Object { $_ | ConvertFrom-Json })
    if (-not ($LogRecords | Where-Object { $_.fields.event -eq "runtime.panic" })) {
        throw "runtime.panic log event not found"
    }
    if (@($LogRecords.run_id | Sort-Object -Unique).Count -lt 3) {
        throw "expected distinct run IDs across process restarts"
    }
    $env:RESCUELOOP_TEST_ABORT_AFTER_OCCURRENCE = "1"
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents run cmd.exe /c exit 43 2>$null
    Remove-Item Env:RESCUELOOP_TEST_ABORT_AFTER_OCCURRENCE
    if ($LASTEXITCODE -eq 0) { throw "expected observation failpoint abort" }
    $Pending = @(Get-ChildItem (Join-Path $Root "observation-journal") -Filter *.json)
    if ($Pending.Count -ne 1) { throw "expected one pending observation transaction" }
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents run cmd.exe /c exit 43
    if ($LASTEXITCODE -ne 0) { throw "observation recovery command failed" }
    $Pending = @(Get-ChildItem (Join-Path $Root "observation-journal") -Filter *.json)
    if ($Pending.Count -ne 0) { throw "observation journal was not drained" }
    $Recovered = @(Get-ChildItem $Incidents -Filter *.json | ForEach-Object {
        Get-Content $_.FullName -Raw | ConvertFrom-Json
    } | Where-Object { $_.normalized_failure.code -eq "exit:43" })
    if ($Recovered.Count -ne 1 -or $Recovered[0].occurrence_count -ne 2) {
        throw "observation recovery was not idempotent"
    }
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents logs --verify --lines 0
    if ($LASTEXITCODE -ne 0) { throw "log integrity verification failed" }
    & cargo run --quiet -p rescueloop -- service status
    if ($LASTEXITCODE -ne 0) { throw "service status failed" }
    Write-Host "Windows native E2E passed."
}
finally {
    if (Test-Path $Root) { Remove-Item -Recurse -Force $Root }
}
