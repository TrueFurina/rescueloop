$ErrorActionPreference = "Stop"
$Repository = if ($env:RESCUELOOP_REPOSITORY) { $env:RESCUELOOP_REPOSITORY } else { "ostapondo/rescueloop" }
$Version = if ($env:RESCUELOOP_VERSION) { $env:RESCUELOOP_VERSION } else { "latest" }
$InstallDir = if ($env:RESCUELOOP_INSTALL_DIR) { $env:RESCUELOOP_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "RescueLoop\bin" }
$Asset = "rescueloop-windows-x86_64.zip"
$Base = if ($Version -eq "latest") { "https://github.com/$Repository/releases/latest/download" } else { "https://github.com/$Repository/releases/download/$Version" }
$Temp = Join-Path ([System.IO.Path]::GetTempPath()) ("rescueloop-" + [guid]::NewGuid())

try {
    New-Item -ItemType Directory -Force -Path $Temp | Out-Null
    Invoke-WebRequest -UseBasicParsing "$Base/$Asset" -OutFile (Join-Path $Temp $Asset)
    Invoke-WebRequest -UseBasicParsing "$Base/SHA256SUMS" -OutFile (Join-Path $Temp "SHA256SUMS")
    $Line = Get-Content (Join-Path $Temp "SHA256SUMS") | Where-Object { $_ -match "\s+$([regex]::Escape($Asset))$" } | Select-Object -First 1
    if (-not $Line) { throw "Checksum for $Asset is missing." }
    $Expected = ($Line -split "\s+")[0].ToLowerInvariant()
    $Actual = (Get-FileHash -Algorithm SHA256 (Join-Path $Temp $Asset)).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) { throw "Checksum verification failed." }
    Expand-Archive (Join-Path $Temp $Asset) -DestinationPath $Temp -Force
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item (Join-Path $Temp "rescueloop.exe") (Join-Path $InstallDir "rescueloop.exe") -Force
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($UserPath -split ';') -notcontains $InstallDir) {
        [Environment]::SetEnvironmentVariable("Path", "$InstallDir;$UserPath", "User")
    }
    Write-Host "Installed RescueLoop to $InstallDir\rescueloop.exe"
    Write-Host "Open a new terminal, then run: rescueloop setup"
}
finally {
    if (Test-Path $Temp) { Remove-Item -Recurse -Force $Temp }
}
