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
    & cargo run --quiet -p rescueloop -- service status
    if ($LASTEXITCODE -ne 0) { throw "service status failed" }
    Write-Host "Windows native E2E passed."
}
finally {
    if (Test-Path $Root) { Remove-Item -Recurse -Force $Root }
}
