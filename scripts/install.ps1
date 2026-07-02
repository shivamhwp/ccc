# Install the latest ccc release binary on Windows.
#   irm https://raw.githubusercontent.com/shivamhwp/ccc/main/scripts/install.ps1 | iex
$ErrorActionPreference = "Stop"

$repo = "shivamhwp/ccc"
$binDir = if ($env:CCC_INSTALL_DIR) { $env:CCC_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\ccc" }

$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne "AMD64") {
    Write-Error "unsupported Windows arch: $arch (build from source with cargo)"
}
$target = "x86_64-pc-windows-msvc"

$tag = $env:CCC_VERSION
if (-not $tag) {
    $tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}
if (-not $tag) { Write-Error "could not determine latest release tag" }

$asset = "ccc-$target.zip"
$url = "https://github.com/$repo/releases/download/$tag/$asset"
Write-Host "Downloading $asset ($tag)..."

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("ccc-install-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $zip = Join-Path $tmp $asset
    Invoke-WebRequest -Uri $url -OutFile $zip

    # Verify checksum if published alongside.
    try {
        $expected = (Invoke-WebRequest -Uri "$url.sha256" -UseBasicParsing).Content
        if ($expected -is [byte[]]) { $expected = [System.Text.Encoding]::ASCII.GetString($expected) }
        $expected = ($expected -split '\s+')[0].Trim()
        $actual = (Get-FileHash $zip -Algorithm SHA256).Hash
        if ($expected -and ($actual -ine $expected)) {
            Write-Error "checksum verification failed (expected $expected, got $actual)"
        }
        Write-Host "checksum ok"
    } catch [System.Net.WebException] {
        # No checksum published; continue.
    }

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    Copy-Item (Join-Path $tmp "ccc-$target\ccc.exe") (Join-Path $binDir "ccc.exe") -Force
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Write-Host "Installed ccc to $binDir\ccc.exe"

# Add to the user PATH if missing (takes effect in new terminals).
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ';') -notcontains $binDir) {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$binDir", "User")
    Write-Host "Added $binDir to your user PATH (open a new terminal to pick it up)."
}
Write-Host "Next: ccc setup"
