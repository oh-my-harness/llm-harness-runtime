param(
    [ValidateSet("debug", "release")]
    [string]$Configuration = "release"
)

$ErrorActionPreference = "Stop"
$tauriCliVersion = "2.11.4"

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$shellRoot = Join-Path $repositoryRoot "crates\llm-harness-runtime-app-shell"
Set-Location $shellRoot

cargo build --$Configuration -p llm-harness-runtime-app-shell

$hostLine = (& rustc -vV) | Where-Object { $_ -like "host:*" } | Select-Object -First 1
if (-not $hostLine) {
    throw "Could not determine Rust host target"
}
$targetTriple = $hostLine.Substring("host:".Length).Trim()
if ($targetTriple -notlike "*-windows-*") {
    throw "This packaging script currently supports Windows targets; detected $targetTriple"
}

$coreBinary = Join-Path $repositoryRoot "target\$Configuration\llm-harness-app.exe"
if (-not (Test-Path -LiteralPath $coreBinary)) {
    throw "Core binary was not built: $coreBinary"
}

$sidecarDirectory = Join-Path $shellRoot "binaries"
New-Item -ItemType Directory -Force -Path $sidecarDirectory | Out-Null
$sidecarBinary = Join-Path $sidecarDirectory "llm-harness-app-$targetTriple.exe"
Copy-Item -LiteralPath $coreBinary -Destination $sidecarBinary -Force

npx --yes "@tauri-apps/cli@$tauriCliVersion" build --config tauri.bundle.json -- --no-default-features
if ($LASTEXITCODE -ne 0) {
    throw "Tauri bundle build failed with exit code $LASTEXITCODE"
}

$bundleRoot = Join-Path $repositoryRoot "target\$Configuration\bundle"
if (Test-Path -LiteralPath $bundleRoot) {
    Write-Host "Runtime app shell bundles:"
    Get-ChildItem -LiteralPath $bundleRoot -Recurse -File |
        Where-Object { $_.Extension -in ".msi", ".exe" } |
        ForEach-Object { Write-Host $_.FullName }
} else {
    Write-Warning "No bundle directory was produced at $bundleRoot"
}
