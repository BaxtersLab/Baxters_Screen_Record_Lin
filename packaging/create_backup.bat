@echo off
REM BSR Backup Creation Script
REM Creates a timestamped, self-contained backup of the entire project

echo BSR Backup Creation Tool
echo ========================
echo.

REM Generate timestamp
for /f "tokens=2 delims==" %%i in ('wmic os get localdatetime /value') do set datetime=%%i
set TIMESTAMP=%datetime:~0,4%-%datetime:~4,2%-%datetime:~6,2%_%datetime:~8,2%-%datetime:~10,2%-%datetime:~12,2%

echo Backup timestamp: %TIMESTAMP%
echo.

REM Create backup directory structure
set BACKUP_DIR=backup\%TIMESTAMP%
echo Creating backup directory: %BACKUP_DIR%

if not exist "backup" mkdir "backup"
mkdir "%BACKUP_DIR%"
mkdir "%BACKUP_DIR%\source"
mkdir "%BACKUP_DIR%\dist"
mkdir "%BACKUP_DIR%\installer"
mkdir "%BACKUP_DIR%\ffmpeg"
mkdir "%BACKUP_DIR%\config"
mkdir "%BACKUP_DIR%\logs"
mkdir "%BACKUP_DIR%\reports"
mkdir "%BACKUP_DIR%\metadata"

echo Directory structure created.
echo.

REM Copy source code (excluding build artifacts and unwanted directories)
echo Copying source code...
xcopy "crates" "%BACKUP_DIR%\source\crates\" /E /I /H /Y /EXCLUDE:packaging\backup_exclude.txt >nul 2>&1
xcopy "*.toml" "%BACKUP_DIR%\source\" /Y >nul 2>&1
xcopy "*.md" "%BACKUP_DIR%\source\" /Y >nul 2>&1
xcopy "LICENSE" "%BACKUP_DIR%\source\" >nul 2>&1
xcopy "THIRD_PARTY_LICENSES" "%BACKUP_DIR%\source\" >nul 2>&1
xcopy "generate_report.rs" "%BACKUP_DIR%\source\" >nul 2>&1
xcopy "tests" "%BACKUP_DIR%\source\tests\" /E /I /H /Y >nul 2>&1
xcopy "packaging" "%BACKUP_DIR%\source\packaging\" /E /I /H /Y >nul 2>&1
xcopy "scripts" "%BACKUP_DIR%\source\scripts\" /E /I /H /Y >nul 2>&1

echo Source code copied.
echo.

REM Copy distribution artifacts
echo Copying distribution artifacts...
if exist "dist" (
    xcopy "dist" "%BACKUP_DIR%\dist\" /E /I /H /Y >nul 2>&1
    echo Distribution artifacts copied.
) else (
    echo No dist directory found.
)

REM Copy installer artifacts
if exist "installer" (
    xcopy "installer" "%BACKUP_DIR%\installer\" /E /I /H /Y >nul 2>&1
    echo Installer artifacts copied.
) else (
    echo No installer directory found.
)

REM Copy FFmpeg bundles
if exist "vcpkg\installed\x64-windows\bin" (
    xcopy "vcpkg\installed\x64-windows\bin\av*.dll" "%BACKUP_DIR%\ffmpeg\" /Y >nul 2>&1
    xcopy "vcpkg\installed\x64-windows\bin\sw*.dll" "%BACKUP_DIR%\ffmpeg\" /Y >nul 2>&1
    echo FFmpeg DLLs copied from vcpkg.
) else if exist "dist\ffmpeg" (
    xcopy "dist\ffmpeg" "%BACKUP_DIR%\ffmpeg\" /E /I /H /Y >nul 2>&1
    echo FFmpeg DLLs copied from dist.
) else (
    echo No FFmpeg DLLs found.
)

REM Copy configuration files
if exist "dist\config" (
    xcopy "dist\config" "%BACKUP_DIR%\config\" /E /I /H /Y >nul 2>&1
    echo Configuration files copied.
) else (
    echo No config directory found.
)

REM Copy logs
if exist "dist\logs" (
    xcopy "dist\logs" "%BACKUP_DIR%\logs\" /E /I /H /Y >nul 2>&1
    echo Log files copied.
) else (
    echo No logs directory found.
)

REM Copy reports
if exist "readiness_report.txt" (
    copy "readiness_report.txt" "%BACKUP_DIR%\reports\" >nul 2>&1
    echo Readiness report copied.
)
if exist "tests" (
    xcopy "tests\*.txt" "%BACKUP_DIR%\reports\" /Y >nul 2>&1
    echo Test reports copied.
)

echo.
echo Generating metadata...

REM Generate version info
if exist "dist\version.txt" (
    copy "dist\version.txt" "%BACKUP_DIR%\metadata\" >nul 2>&1
) else (
    echo unknown > "%BACKUP_DIR%\metadata\version.txt"
)

REM Generate commit hash
git rev-parse HEAD > "%BACKUP_DIR%\metadata\commit_hash.txt" 2>nul
if %ERRORLEVEL% neq 0 (
    echo unknown > "%BACKUP_DIR%\metadata\commit_hash.txt"
)

REM Generate build info
echo Build Timestamp: %TIMESTAMP% > "%BACKUP_DIR%\metadata\build_info.txt"
echo Operating System: Windows >> "%BACKUP_DIR%\metadata\build_info.txt"
echo Rust Version: >> "%BACKUP_DIR%\metadata\build_info.txt"
rustc --version >> "%BACKUP_DIR%\metadata\build_info.txt" 2>nul
if %ERRORLEVEL% neq 0 (
    echo Rust not found >> "%BACKUP_DIR%\metadata\build_info.txt"
)
echo Cargo Version: >> "%BACKUP_DIR%\metadata\build_info.txt"
cargo --version >> "%BACKUP_DIR%\metadata\build_info.txt" 2>nul
if %ERRORLEVEL% neq 0 (
    echo Cargo not found >> "%BACKUP_DIR%\metadata\build_info.txt"
)

echo Metadata generated.
echo.

REM Generate integrity checks
echo Generating integrity checks...
powershell -Command "Get-ChildItem -Path '%BACKUP_DIR%' -Recurse -File | Measure-Object | Select-Object -ExpandProperty Count" > "%BACKUP_DIR%\metadata\file_count.txt"

REM Generate SHA256 hashes for executables and important files
if exist "%BACKUP_DIR%\dist\bin\bsr-ui.exe" (
    powershell -Command "Get-FileHash -Path '%BACKUP_DIR%\dist\bin\bsr-ui.exe' -Algorithm SHA256 | Select-Object -ExpandProperty Hash" > "%BACKUP_DIR%\metadata\bsr-ui.sha256"
)

if exist "%BACKUP_DIR%\installer\*.exe" (
    for %%f in ("%BACKUP_DIR%\installer\*.exe") do (
        powershell -Command "Get-FileHash -Path '%%f' -Algorithm SHA256 | Select-Object -ExpandProperty Hash" > "%BACKUP_DIR%\metadata\%%~nf.sha256"
    )
)

echo Integrity checks completed.
echo.

REM Optional compression
echo Backup created successfully!
echo Location: %BACKUP_DIR%
echo.
echo File count: 
type "%BACKUP_DIR%\metadata\file_count.txt" 2>nul
echo.
echo To compress this backup (optional):
echo powershell "Compress-Archive -Path '%BACKUP_DIR%' -DestinationPath '%BACKUP_DIR%.zip'"
echo.
echo Backup complete! 🎉