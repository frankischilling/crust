Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Resolve-Path (Join-Path $scriptDir "..")
Push-Location $repoRoot

try {
    Write-Host "[1/4] Building crust release binary..."
    cargo build -p crust --release

    $exePath = Join-Path $repoRoot "target\release\crust.exe"
    if (-not (Test-Path $exePath)) {
        throw "Release binary not found at $exePath"
    }

    $metadata = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
    $pkg = $metadata.packages | Where-Object { $_.name -eq "crust" } | Select-Object -First 1
    if (-not $pkg) {
        throw "Could not resolve package version for 'crust' from cargo metadata"
    }
    $version = $pkg.version
    $zipName = "crust-v$version-windows-x64.zip"

    $distRoot = Join-Path $repoRoot "dist\windows"
    $stagingDir = Join-Path $distRoot "crust-v$version-windows-x64"

    if (Test-Path $stagingDir) {
        Remove-Item -Recurse -Force $stagingDir
    }
    New-Item -ItemType Directory -Path $stagingDir -Force | Out-Null

    Write-Host "[2/4] Copying artifacts..."
    Copy-Item $exePath (Join-Path $stagingDir "crust.exe") -Force
    Copy-Item (Join-Path $repoRoot "README.md") (Join-Path $stagingDir "README.md") -Force
    Copy-Item (Join-Path $repoRoot "LICENSE") (Join-Path $stagingDir "LICENSE") -Force

    $zipPath = Join-Path $distRoot $zipName
    if (Test-Path $zipPath) {
        Remove-Item -Force $zipPath
    }

    Write-Host "[3/4] Creating zip package..."
    Compress-Archive -Path (Join-Path $stagingDir "*") -DestinationPath $zipPath -Force

    Write-Host "[4/4] Done"
    Write-Host "Binary: $exePath"
    Write-Host "Package: $zipPath"
}
finally {
    Pop-Location
}
