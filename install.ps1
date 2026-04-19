# mlink installer for Windows
# Usage: irm https://raw.githubusercontent.com/zhuqingyv/mlink/main/install.ps1 | iex

$ErrorActionPreference = 'Stop'

$Repo      = 'zhuqingyv/mlink'
$BinName   = 'mlink.exe'
$InstallDir = if ($env:MLINK_INSTALL_DIR) { $env:MLINK_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'mlink' }

function Write-Info($msg)  { Write-Host "info: $msg" }
function Write-Ok($msg)    { Write-Host "✓ $msg" -ForegroundColor Green }
function Write-Warn($msg)  { Write-Host "warn: $msg" -ForegroundColor Yellow }
function Fail($msg)        { Write-Host "error: $msg" -ForegroundColor Red; exit 1 }

# ---- detect arch ----
$arch = (Get-CimInstance Win32_Processor -ErrorAction SilentlyContinue | Select-Object -First 1).Architecture
# 9 = x64, 12 = ARM64. Fall back to env var if CIM unavailable.
switch ($arch) {
    9       { $Target = 'x86_64-pc-windows-msvc' }
    12      { $Target = 'aarch64-pc-windows-msvc' }
    default {
        switch ($env:PROCESSOR_ARCHITECTURE) {
            'AMD64' { $Target = 'x86_64-pc-windows-msvc' }
            'ARM64' { $Target = 'aarch64-pc-windows-msvc' }
            default { Fail "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
        }
    }
}
Write-Info "detected platform: $Target"

# ---- fetch latest tag ----
Write-Info 'fetching latest release tag...'
try {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -UseBasicParsing
    $Tag = $release.tag_name
} catch {
    Fail "could not fetch latest release: $_"
}
if (-not $Tag) { Fail 'could not resolve latest release tag' }
Write-Ok "latest release: $Tag"

# ---- download ----
$Archive    = "mlink-$Tag-$Target.zip"
$BaseUrl    = "https://github.com/$Repo/releases/download/$Tag"
$ZipUrl     = "$BaseUrl/$Archive"
$ShaUrl     = "$ZipUrl.sha256"

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("mlink-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Tmp -Force | Out-Null

try {
    Write-Info "downloading $ZipUrl"
    $ZipPath = Join-Path $Tmp $Archive
    Invoke-WebRequest -Uri $ZipUrl -OutFile $ZipPath -UseBasicParsing

    # ---- verify checksum ----
    try {
        $ShaPath = "$ZipPath.sha256"
        Invoke-WebRequest -Uri $ShaUrl -OutFile $ShaPath -UseBasicParsing -ErrorAction Stop
        $expected = (Get-Content $ShaPath -Raw).Trim().Split()[0].ToLower()
        $actual   = (Get-FileHash -Path $ZipPath -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) {
            Fail "checksum mismatch: expected $expected, got $actual"
        }
        Write-Ok 'checksum verified'
    } catch {
        Write-Warn "checksum verification skipped: $($_.Exception.Message)"
    }

    # ---- extract ----
    Write-Info 'extracting...'
    $ExtractDir = Join-Path $Tmp 'extracted'
    Expand-Archive -Path $ZipPath -DestinationPath $ExtractDir -Force

    $binary = Get-ChildItem -Path $ExtractDir -Recurse -Filter $BinName -File | Select-Object -First 1
    if (-not $binary) { Fail "could not find $BinName inside archive" }

    # ---- install ----
    if (-not (Test-Path $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }
    $Dest = Join-Path $InstallDir $BinName
    Copy-Item -Path $binary.FullName -Destination $Dest -Force
    Write-Ok "installed to $Dest"

    # ---- PATH ----
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $userPath) { $userPath = '' }
    $pathEntries = $userPath.Split(';') | Where-Object { $_ -ne '' }
    $already = $pathEntries | Where-Object { $_.TrimEnd('\') -ieq $InstallDir.TrimEnd('\') }

    if (-not $already) {
        $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Ok "added $InstallDir to user PATH"
        Write-Warn 'open a new terminal for PATH changes to take effect'
    }

    # make it usable in the current session too
    if (-not ($env:Path -split ';' | Where-Object { $_.TrimEnd('\') -ieq $InstallDir.TrimEnd('\') })) {
        $env:Path = "$env:Path;$InstallDir"
    }

    # ---- version ----
    try {
        $ver = & $Dest --version 2>$null
        if ($ver) { Write-Ok $ver }
    } catch { }

    Write-Host ''
    Write-Host 'install complete.' -ForegroundColor Green
    Write-Host 'run ' -NoNewline; Write-Host 'mlink --help' -ForegroundColor Cyan -NoNewline; Write-Host ' to get started.'
}
finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
