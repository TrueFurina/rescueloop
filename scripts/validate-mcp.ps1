$ErrorActionPreference = "Stop"

$StateRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("rescueloop-mcp-" + [guid]::NewGuid())
try {
    New-Item -ItemType Directory -Force -Path $StateRoot | Out-Null
    & cargo build --quiet -p rescueloop
    if ($LASTEXITCODE -ne 0) { throw "RescueLoop build failed" }

    $Binary = (Resolve-Path "target/debug/rescueloop.exe").Path
    $IncidentDirectory = Join-Path $StateRoot ".rescueloop/incidents"
    $ConfigPath = Join-Path $StateRoot "inspector.json"
    @{
        mcpServers = @{
            rescueloop = @{
                type = "stdio"
                command = $Binary
                args = @("--incident-dir", $IncidentDirectory, "mcp")
            }
        }
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $ConfigPath -Encoding utf8

    $Inspector = @(
        "-y", "@modelcontextprotocol/inspector@2.4.0", "--cli",
        "--config", $ConfigPath, "--server", "rescueloop", "--format", "json"
    )
    & npx.cmd @Inspector --method tools/list --strict | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "MCP tool discovery failed" }
    & npx.cmd @Inspector --method tools/call --tool-name list_incidents --tool-arg limit=5 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "MCP list_incidents call failed" }

    Write-Host "Windows MCP Inspector validation passed."
}
finally {
    if (Test-Path -LiteralPath $StateRoot) {
        Remove-Item -LiteralPath $StateRoot -Recurse -Force
    }
}
