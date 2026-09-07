# Baxter's Screen Record — portable bundle packager
#
# Produces a self-contained, DOUBLE-CLICKABLE folder: bsr-ui.exe with the required
# FFmpeg shared DLLs placed BESIDE it (Windows resolves DLLs from the exe's own
# directory first), so it runs with no PATH setup, no vcpkg, and no installer.
#
# Usage (from anywhere):
#   powershell -ExecutionPolicy Bypass -File packaging\package_portable.ps1
# Optional overrides:
#   -FfmpegDir <dir with bin\ include\ lib\>   (default: the dev shared build)
#   -OutDir    <output folder>                 (default: <repo>\dist\BSR)

[CmdletBinding()]
param(
    [string]$FfmpegDir    = $env:FFMPEG_DIR,
    [string]$LibclangPath = $env:LIBCLANG_PATH,
    [string]$OutDir
)
$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = Split-Path -Parent $ScriptDir

# Dev-toolchain defaults (override via -FfmpegDir / env). See BSR_HANDOFF.md.
if (-not $FfmpegDir)    { $FfmpegDir    = 'C:\Users\user\ffmpeg-dev\ffmpeg-8.1.2-full_build-shared' }
if (-not $LibclangPath) { $LibclangPath = 'C:\Program Files\LLVM\bin' }
if (-not $OutDir)       { $OutDir       = Join-Path $RepoRoot 'dist\BSR' }

$FfmpegBin = Join-Path $FfmpegDir 'bin'
if (-not (Test-Path $FfmpegBin)) {
    throw "FFmpeg shared 'bin' not found: $FfmpegBin  (pass -FfmpegDir <ffmpeg-*-full_build-shared>)"
}

# Runtime DLL closure for bsr-ui, confirmed via `dumpbin /dependents` + a clean-PATH
# launch test. bsr-encode/bsr-mux set default-features=false on ffmpeg-next, dropping
# avdevice + avfilter (unused), so the exe imports avcodec/avformat/avutil/swscale
# directly and swresample transitively (avcodec) — five DLLs. Missing any ⇒
# STATUS_DLL_NOT_FOUND (0xC0000135). Non-FFmpeg imports (VCRUNTIME140,
# api-ms-win-crt-*, dxgi/d3d11/opengl32) live in System32 and need no bundling.
$DllPatterns = @(
    'avcodec-*.dll', 'avformat-*.dll', 'avutil-*.dll', 'swscale-*.dll', 'swresample-*.dll'
)

Write-Host "Building release (bsr-ui)..." -ForegroundColor Cyan
$env:FFMPEG_DIR    = $FfmpegDir
$env:LIBCLANG_PATH = $LibclangPath
Push-Location $RepoRoot
try {
    & cargo build --release -p bsr-ui
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed ($LASTEXITCODE)" }
    $targetDir = (& cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory
} finally {
    Pop-Location
}

$Exe = Join-Path $targetDir 'release\bsr-ui.exe'
if (-not (Test-Path $Exe)) { throw "built exe not found: $Exe" }

# Assemble the bundle (flat: exe + DLLs together == double-clickable).
if (Test-Path $OutDir) { Remove-Item $OutDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Copy-Item $Exe -Destination $OutDir
foreach ($pat in $DllPatterns) {
    $dll = Get-ChildItem -Path (Join-Path $FfmpegBin $pat) -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $dll) { throw "required FFmpeg DLL not found for pattern '$pat' in $FfmpegBin" }
    Copy-Item $dll.FullName -Destination $OutDir
}

foreach ($doc in @('README.md', 'LICENSE', 'THIRD_PARTY_LICENSES')) {
    $p = Join-Path $RepoRoot $doc
    if (Test-Path $p) { Copy-Item $p -Destination $OutDir }
}

$sha = (& git -C $RepoRoot rev-parse --short HEAD 2>$null)
if (-not $sha) { $sha = 'unknown' }
Set-Content -Path (Join-Path $OutDir 'version.txt') -Value $sha -Encoding ascii

Write-Host ""
Write-Host "Portable bundle ready: $OutDir" -ForegroundColor Green
Get-ChildItem $OutDir |
    Select-Object Name, @{ n = 'Size'; e = { '{0:N1} MB' -f ($_.Length / 1MB) } } |
    Format-Table -AutoSize
Write-Host "Double-click bsr-ui.exe in that folder to run (no PATH/FFmpeg setup needed)."
