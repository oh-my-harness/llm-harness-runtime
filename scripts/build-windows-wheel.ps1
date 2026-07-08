<# 
Build the llm-harness-py Windows x64 wheel and upload it to GitHub Release.

Usage:
  powershell -ExecutionPolicy Bypass -File ./scripts/build-windows-wheel.ps1 -Version 0.2.0
  powershell -ExecutionPolicy Bypass -File ./scripts/build-windows-wheel.ps1 -Version 0.2.0 -BuildOnly

Prerequisites:
  - Rust stable with the MSVC toolchain
  - Python 3.12 on PATH, or set $env:PYTHON to a Python executable
  - maturin on PATH, set $env:MATURIN, or install it into Python with `python -m pip install maturin`
  - gh authenticated with write access to oh-my-harness/llm-harness-py-wheels for uploads
#>

param(
    [Parameter(Mandatory = $true)]
    [string]$Version,

    [switch]$BuildOnly
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

$CrateDir = "crates/llm-harness-py"
$WheelDir = "target/wheels"
$WheelsRepo = "oh-my-harness/llm-harness-py-wheels"
$Tag = "v$Version"

function Require-Command {
    param([string]$Name)
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "$Name not found on PATH"
    }
}

function Wheel-Checksum-Line {
    param([System.IO.FileInfo]$Wheel)
    $Hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $Wheel.FullName).Hash.ToLowerInvariant()
    "$Hash  $($Wheel.Name)"
}

function Merge-Checksum-Lines {
    param(
        [string[]]$ExistingLines,
        [string[]]$NewLines
    )

    $ByName = [ordered]@{}
    foreach ($Line in @($ExistingLines + $NewLines)) {
        if ($Line -match "^[a-fA-F0-9]{64}\s+\*?(.+)$") {
            $Name = $Matches[1].Trim()
            $ByName[$Name] = $Line
        }
    }
    $ByName.Values
}

Require-Command "cargo"
Require-Command "rustc"
if (-not $BuildOnly) {
    Require-Command "gh"
}

if ($env:PYTHON) {
    $Python = $env:PYTHON
}
elseif (Get-Command "py" -ErrorAction SilentlyContinue) {
    $Python = (& py -3.12 -c "import sys; print(sys.executable)").Trim()
}
else {
    $Python = "python"
}
Require-Command $Python

$Maturin = if ($env:MATURIN) { $env:MATURIN } else { "maturin" }
$MaturinArgs = @()
if (-not (Get-Command $Maturin -ErrorAction SilentlyContinue)) {
    $Maturin = $Python
    $MaturinArgs = @("-m", "maturin")
}

try {
    & $Maturin @MaturinArgs --version | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "maturin exited with code $LASTEXITCODE"
    }
}
catch {
    throw "maturin not found. Install it with: $Python -m pip install maturin"
}

New-Item -ItemType Directory -Force -Path $WheelDir | Out-Null

Write-Host ""
Write-Host "Building Windows x64 abi3 wheel..."
& $Maturin @MaturinArgs build --release `
    --manifest-path "$CrateDir/Cargo.toml" `
    --interpreter $Python `
    --out $WheelDir
if ($LASTEXITCODE -ne 0) {
    throw "maturin build failed with code $LASTEXITCODE"
}

$WindowsWheels = @(Get-ChildItem -Path $WheelDir -Filter "*win_amd64.whl" | Sort-Object Name)
if ($WindowsWheels.Count -eq 0) {
    throw "No Windows x64 wheel found in $WheelDir"
}

Write-Host ""
Write-Host "Built Windows wheels:"
$WindowsWheels | ForEach-Object { Write-Host "  $($_.FullName)" }

Write-Host ""
Write-Host "Smoke testing Windows wheels..."
$SmokeVenv = Join-Path ([System.IO.Path]::GetTempPath()) ("llm-harness-py-smoke-" + [System.Guid]::NewGuid())
& $Python -m venv $SmokeVenv
$SmokePython = Join-Path $SmokeVenv "Scripts/python.exe"
$SmokePip = Join-Path $SmokeVenv "Scripts/pip.exe"

try {
    foreach ($Wheel in $WindowsWheels) {
        Write-Host "  Installing $($Wheel.Name)"
        & $SmokePip install --quiet $Wheel.FullName
        & $SmokePython -c "import llm_harness_py; print('    import ok')"
        & $SmokePip uninstall -y llm_harness_py --quiet
    }
}
finally {
    Remove-Item -LiteralPath $SmokeVenv -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "Generating checksums..."
$WindowsChecksumPath = Join-Path $WheelDir "checksums-windows.txt"
$CombinedChecksumPath = Join-Path $WheelDir "checksums.txt"
$WindowsChecksumLines = @($WindowsWheels | ForEach-Object { Wheel-Checksum-Line $_ })
$WindowsChecksumLines | Set-Content -Encoding ascii -Path $WindowsChecksumPath

$ExistingChecksumLines = @()
if (-not $BuildOnly) {
    $DownloadDir = Join-Path ([System.IO.Path]::GetTempPath()) ("llm-harness-checksums-" + [System.Guid]::NewGuid())
    New-Item -ItemType Directory -Force -Path $DownloadDir | Out-Null
    try {
        $releaseExists = $true
        & gh release view $Tag -R $WheelsRepo *> $null
        if ($LASTEXITCODE -ne 0) {
            $releaseExists = $false
        }

        if ($releaseExists) {
            & gh release download $Tag -R $WheelsRepo -p "checksums.txt" -D $DownloadDir *> $null
            $DownloadedChecksumPath = Join-Path $DownloadDir "checksums.txt"
            if (Test-Path $DownloadedChecksumPath) {
                $ExistingChecksumLines = @(Get-Content $DownloadedChecksumPath)
            }
        }
    }
    finally {
        Remove-Item -LiteralPath $DownloadDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$CombinedChecksumLines = @(Merge-Checksum-Lines -ExistingLines $ExistingChecksumLines -NewLines $WindowsChecksumLines)
$CombinedChecksumLines | Set-Content -Encoding ascii -Path $CombinedChecksumPath
Write-Host "  $WindowsChecksumPath"
Write-Host "  $CombinedChecksumPath"

if ($BuildOnly) {
    Write-Host ""
    Write-Host "Skipping upload (-BuildOnly). Wheels are in: $WheelDir"
    exit 0
}

Write-Host ""
Write-Host "Uploading Windows wheel to release $Tag in $WheelsRepo..."
$releaseExists = $true
& gh release view $Tag -R $WheelsRepo *> $null
if ($LASTEXITCODE -ne 0) {
    $releaseExists = $false
}

$Assets = @($WindowsWheels.FullName) + @($WindowsChecksumPath, $CombinedChecksumPath)
if ($releaseExists) {
    & gh release upload $Tag -R $WheelsRepo @Assets --clobber
    if ($LASTEXITCODE -ne 0) {
        throw "gh release upload failed with code $LASTEXITCODE. Check write access to $WheelsRepo."
    }
}
else {
    & gh release create $Tag -R $WheelsRepo `
        --title $Tag `
        --notes "Prebuilt PyO3 abi3 wheels for llm-harness-py (CPython 3.9+). Source code is in the private llm-harness-runtime repo; this repo distributes compiled binaries only." `
        @Assets
    if ($LASTEXITCODE -ne 0) {
        throw "gh release create failed with code $LASTEXITCODE. Check write access to $WheelsRepo."
    }
}

Write-Host ""
Write-Host "Done. Release: https://github.com/$WheelsRepo/releases/tag/$Tag"
