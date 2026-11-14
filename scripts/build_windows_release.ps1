Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-CargoBinaryPath {
    param([string[]]$Messages, [string]$PackageId, [string]$TargetName)

    $executable = $null
    foreach ($line in $Messages) {
        $artifact = $line | ConvertFrom-Json
        if ($artifact.reason -ne "compiler-artifact") {
            continue
        }
        if ($artifact.package_id -eq $PackageId -and
            $artifact.target.name -eq $TargetName -and
            $artifact.target.kind -contains "bin" -and
            $artifact.executable) {
            # Cargo reports fresh artifacts too, so an unchanged build is valid.
            $executable = [string]$artifact.executable
        }
    }
    if (-not $executable) {
        throw "Cargo did not report an executable for '$TargetName'"
    }
    return $executable
}

function Assert-WindowsX64Executable {
    param([string]$Path)

    $reader = [System.IO.BinaryReader]::new([System.IO.File]::OpenRead($Path))
    try {
        if ($reader.BaseStream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5A4D) {
            throw "Not a Windows PE executable: $Path"
        }
        $reader.BaseStream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
        if ($peOffset -lt 64 -or ([long]$peOffset + 6) -gt $reader.BaseStream.Length) {
            throw "Invalid Windows PE header: $Path"
        }
        $reader.BaseStream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne 0x8664) {
            throw "The Windows release package requires x64 executables: $Path"
        }
    }
    finally {
        $reader.Dispose()
    }
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Resolve-Path (Join-Path $scriptDir "..")
Push-Location $repoRoot

try {
    Write-Host "[1/5] Building crust + crust-webview release binaries..."
    $buildMessages = @(cargo build -p crust -p crust-webview-host --release --locked --message-format=json-render-diagnostics)
    if ($LASTEXITCODE -ne 0) {
        throw "Release build failed with exit code $LASTEXITCODE"
    }

    $metadataJson = cargo metadata --format-version 1 --no-deps --locked
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo metadata failed with exit code $LASTEXITCODE"
    }
    $metadata = $metadataJson | ConvertFrom-Json
    $pkg = $metadata.packages | Where-Object { $_.name -eq "crust" } | Select-Object -First 1
    if (-not $pkg) {
        throw "Could not resolve package version for 'crust' from cargo metadata"
    }
    $webviewPkg = $metadata.packages | Where-Object { $_.name -eq "crust-webview-host" } | Select-Object -First 1
    if (-not $webviewPkg) {
        throw "Could not resolve package 'crust-webview-host' from cargo metadata"
    }
    $exePath = Get-CargoBinaryPath -Messages $buildMessages -PackageId $pkg.id -TargetName "crust"
    $webviewPath = Get-CargoBinaryPath -Messages $buildMessages -PackageId $webviewPkg.id -TargetName "crust-webview"
    Assert-WindowsX64Executable -Path $exePath
    Assert-WindowsX64Executable -Path $webviewPath
    $version = $pkg.version
    $zipName = "crust-v$version-windows-x64.zip"

    $distRoot = Join-Path $repoRoot "dist\windows"
    $stagingDir = Join-Path $distRoot "crust-v$version-windows-x64"

    if (Test-Path $stagingDir) {
        Remove-Item -Recurse -Force $stagingDir
    }
    New-Item -ItemType Directory -Path $stagingDir -Force | Out-Null

    Write-Host "[2/5] Copying artifacts..."
    Copy-Item $exePath (Join-Path $stagingDir "crust.exe") -Force
    Copy-Item $webviewPath (Join-Path $stagingDir "crust-webview.exe") -Force
    Copy-Item (Join-Path $repoRoot "README.md") (Join-Path $stagingDir "README.md") -Force
    Copy-Item (Join-Path $repoRoot "LICENSE") (Join-Path $stagingDir "LICENSE") -Force

    $zipPath = Join-Path $distRoot $zipName
    if (Test-Path $zipPath) {
        Remove-Item -Force $zipPath
    }

    Write-Host "[3/5] Creating zip package..."
    Compress-Archive -Path (Join-Path $stagingDir "*") -DestinationPath $zipPath -Force

    Write-Host "[4/5] Writing SHA256 digest..."
    $digestPath = "$zipPath.sha256"
    $digest = (Get-FileHash -Algorithm SHA256 -LiteralPath $zipPath).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText($digestPath, "$digest  $zipName`n", [System.Text.Encoding]::ASCII)

    Write-Host "[5/5] Done"
    Write-Host "Binary: $exePath"
    Write-Host "Package: $zipPath"
    Write-Host "Digest: $digestPath"
}
finally {
    Pop-Location
}
