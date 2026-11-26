# Builds the release program and packs it with the README and licence into
# dist\Mestream-<version>-windows-x64.zip, ready to attach to a GitHub release.
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    # The Opus decoder is built with CMake. If it is not on the PATH, use the
    # copy that comes with Visual Studio.
    if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
        $found = if (Test-Path $vswhere) {
            & $vswhere -latest -products * -find '**/CMake/bin/cmake.exe' | Select-Object -First 1
        }
        if (-not $found) { throw 'CMake was not found: install it or add it to the PATH' }
        $env:PATH = "$(Split-Path -Parent $found);$env:PATH"
    }

    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'the release build failed' }

    $version = (Select-String -Path 'Cargo.toml' -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
    $name = "Mestream-$version-windows-x64"
    $stage = Join-Path $root "dist\$name"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force $stage | Out-Null

    Copy-Item 'target\release\mestream.exe', 'README.md', 'LICENSE' $stage
    $zip = Join-Path $root "dist\$name.zip"
    if (Test-Path $zip) { Remove-Item -Force $zip }
    Compress-Archive -Path "$stage\*" -DestinationPath $zip

    $hash = (Get-FileHash $zip -Algorithm SHA256).Hash
    "package: $zip"
    "sha256:  $hash"
}
finally {
    Pop-Location
}
