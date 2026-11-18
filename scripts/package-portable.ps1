$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

$CargoCmd = Get-Command cargo -ErrorAction SilentlyContinue
if ($CargoCmd) {
    $CargoExe = $CargoCmd.Source
} else {
    $CargoExe = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
}

if (-not (Test-Path $CargoExe)) {
    throw "cargo executable not found in PATH or at $CargoExe"
}

& $CargoExe dist
exit $LASTEXITCODE
