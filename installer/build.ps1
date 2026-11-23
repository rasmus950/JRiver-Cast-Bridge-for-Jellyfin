# Build the jriver-cast-bridge installer:
#   powershell -ExecutionPolicy Bypass -File installer\build.ps1
# Output: target\installer\jriver-cast-bridge-setup-<version>.exe
# Needs Rust (rustup) and Inno Setup 6 (winget install JRSoftware.InnoSetup).

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
  $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

  cargo build --release -p jriver-cast-bridge --bins
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

  $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
  $version = ($metadata.packages | Where-Object name -eq 'jriver-cast-bridge').version
  if (-not $version) { throw "can't read the jriver-cast-bridge version" }

  $iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
  ) | Where-Object { Test-Path $_ } | Select-Object -First 1
  if (-not $iscc) { throw "Inno Setup 6 not found; winget install JRSoftware.InnoSetup" }

  & $iscc "/DAppVersion=$version" "$PSScriptRoot\jriver-cast-bridge.iss"
  if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
  Write-Host "installer: target\installer\jriver-cast-bridge-setup-$version.exe"
}
finally {
  Pop-Location
}
