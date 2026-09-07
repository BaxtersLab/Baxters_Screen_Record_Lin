# BSR Packaging Guide

Packaging infrastructure for Baxter's Screen Record (BSR).

## Build + package (the supported path)

`packaging\package_portable.ps1` produces a **self-contained, double-clickable**
folder: `bsr-ui.exe` with the required FFmpeg shared DLLs placed **beside it**.
Windows resolves DLLs from the exe's own directory first, so the bundle runs with
no PATH setup, no vcpkg, and no installer.

```powershell
powershell -ExecutionPolicy Bypass -File packaging\package_portable.ps1
```

Optional overrides:

- `-FfmpegDir <dir with bin\ include\ lib\>` — a **shared/dev** FFmpeg build
  (default: the dev build documented in `BSR_HANDOFF.md`).
- `-LibclangPath <dir with libclang.dll>` — for bindgen (default: `C:\Program Files\LLVM\bin`).
- `-OutDir <folder>` — output location (default: `<repo>\dist\BSR`).

### What the bundle contains

```
dist\BSR\
├── bsr-ui.exe            # Main executable
├── avcodec-*.dll         # ┐
├── avformat-*.dll        # │ the five FFmpeg runtime DLLs, BESIDE the exe
├── avutil-*.dll          # │ (missing any ⇒ STATUS_DLL_NOT_FOUND 0xC0000135)
├── swscale-*.dll         # │
├── swresample-*.dll      # ┘
├── README.md             # (if present at repo root)
├── LICENSE               # (if present)
├── THIRD_PARTY_LICENSES  # (if present; FFmpeg is LGPL)
└── version.txt           # short git SHA of the build
```

Only these five DLLs are needed: `bsr-encode`/`bsr-mux` set
`default-features = false` on `ffmpeg-next`, dropping the unused `avdevice` +
`avfilter` (the latter alone is ~119 MB), which halved the bundle to ~133 MB. The
DLL closure was confirmed via `dumpbin /dependents` and a clean-PATH launch test.
Non-FFmpeg imports (VCRUNTIME140, api-ms-win-crt-*, dxgi/d3d11/opengl32) live in
System32 and need no bundling.

## Build environment (required for a from-source build)

BSR links FFmpeg via `ffmpeg-next` (which runs bindgen), so a compile needs a
**shared/dev** FFmpeg build (headers + import libs + DLLs) and libclang. The exact
setup — `FFMPEG_DIR`, `LIBCLANG_PATH`, and the runtime-DLL PATH caveat — is
documented in **`BSR_HANDOFF.md`** at the repo root. The winget `Gyan.FFmpeg`
package is a *static* build and cannot satisfy the link.

## Runtime requirements

- Windows 10 or later (required for DXGI Desktop Duplication capture).
- No admin rights for normal operation.
- Write access to the configured output directory.

## Configuration

`bsr-config-template.toml` in this directory is a reference for the available
settings. The application auto-creates a default config on first run if none is
found; edit the created file to change output directory, bitrate, fps, etc.

## Backup System

BSR includes local backup tooling for project snapshots (independent of the build
above).

- `packaging\create_backup.bat` — timestamped project snapshot (source + any
  `dist\`/`installer\` artifacts + metadata + integrity checks).
- `packaging\manage_backups.bat` — `list` / `clean <days>` / `verify` / `compress`.
- `packaging\verify_backup.bat <backup\dir>` — integrity check of a snapshot.

## Installer (future work)

There is intentionally **no installer script** in this tree. The former
Inno Setup chain (`build_release.bat` → `bsr.iss`) was removed: it depended on a
vcpkg layout that doesn't exist here, placed the DLLs in a separate `ffmpeg\`
folder the exe cannot load from, referenced a missing icon, and its
`build_release.bat` wrote a *text file* named `bsr-ui.exe` as a "mock" when FFmpeg
was absent — an actively misleading artifact.

If a signed installer is wanted later, build it **from the verified portable
bundle** (`dist\BSR\`, produced above): install the exe and the five DLLs together
into the same directory, add a Start-menu/desktop shortcut and an uninstaller, and
verify the installed app records on a clean machine before shipping.
