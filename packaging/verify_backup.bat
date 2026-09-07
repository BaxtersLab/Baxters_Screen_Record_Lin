@echo off
REM BSR Backup Verification Script
REM Verifies the integrity of a backup directory

if "%1"=="" (
    echo Usage: verify_backup.bat ^<backup_directory^>
    echo Example: verify_backup.bat backup\2026-03-15_14-30-00
    exit /b 1
)

set BACKUP_DIR=%1

echo BSR Backup Verification
echo =======================
echo Verifying: %BACKUP_DIR%
echo.

REM Check if backup directory exists
if not exist "%BACKUP_DIR%" (
    echo ERROR: Backup directory does not exist: %BACKUP_DIR%
    exit /b 1
)

echo Directory exists: ✓
echo.

REM Check required subdirectories
set REQUIRED_DIRS=source dist installer ffmpeg config logs reports metadata
for %%d in (%REQUIRED_DIRS%) do (
    if exist "%BACKUP_DIR%\%%d" (
        echo Directory %%d: ✓
    ) else (
        echo Directory %%d: MISSING
        set MISSING=1
    )
)

if defined MISSING (
    echo.
    echo ERROR: Some required directories are missing!
    exit /b 1
)

echo.
echo All required directories present: ✓
echo.

REM Check metadata files
set REQUIRED_METADATA=version.txt commit_hash.txt build_info.txt file_count.txt
for %%f in (%REQUIRED_METADATA%) do (
    if exist "%BACKUP_DIR%\metadata\%%f" (
        echo Metadata %%f: ✓
        echo   Content preview:
        type "%BACKUP_DIR%\metadata\%%f" | head -3
        echo.
    ) else (
        echo Metadata %%f: MISSING
        set MISSING=1
    )
)

if defined MISSING (
    echo.
    echo ERROR: Some required metadata files are missing!
    exit /b 1
)

echo All required metadata present: ✓
echo.

REM Check source code integrity
if exist "%BACKUP_DIR%\source\Cargo.toml" (
    echo Root Cargo.toml: ✓
) else (
    echo Root Cargo.toml: MISSING
    set MISSING=1
)

if exist "%BACKUP_DIR%\source\crates" (
    echo Crates directory: ✓
    dir /b "%BACKUP_DIR%\source\crates" | findstr /c:"bsr-" >nul
    if %ERRORLEVEL% equ 0 (
        echo BSR crates found: ✓
    ) else (
        echo BSR crates: MISSING
        set MISSING=1
    )
) else (
    echo Crates directory: MISSING
    set MISSING=1
)

if defined MISSING (
    echo.
    echo ERROR: Source code integrity check failed!
    exit /b 1
)

echo Source code integrity: ✓
echo.

REM Check file count consistency
if exist "%BACKUP_DIR%\metadata\file_count.txt" (
    for /f %%c in (%BACKUP_DIR%\metadata\file_count.txt) do set EXPECTED_COUNT=%%c
    for /f %%c in ('powershell -Command "Get-ChildItem -Path ''%BACKUP_DIR%'' -Recurse -File | Measure-Object | Select-Object -ExpandProperty Count"') do set ACTUAL_COUNT=%%c

    if "%EXPECTED_COUNT%"=="%ACTUAL_COUNT%" (
        echo File count consistency: ✓ (%ACTUAL_COUNT% files)
    ) else (
        echo File count mismatch: Expected %EXPECTED_COUNT%, got %ACTUAL_COUNT%
        set MISSING=1
    )
)

if defined MISSING (
    echo.
    echo ERROR: Backup verification failed!
    exit /b 1
)

echo.
echo Backup verification successful! ✅
echo.
echo Backup is complete and ready for archival.
echo.

REM Optional: Check SHA256 hashes if they exist
if exist "%BACKUP_DIR%\metadata\*.sha256" (
    echo SHA256 verification available for:
    dir /b "%BACKUP_DIR%\metadata\*.sha256"
    echo.
    echo To verify hashes manually, use:
    echo powershell "Get-FileHash -Path 'file.exe' -Algorithm SHA256"
)