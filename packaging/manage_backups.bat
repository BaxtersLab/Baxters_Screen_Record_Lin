@echo off
REM BSR Backup Management Script
REM Lists, cleans, and manages backup directories

echo BSR Backup Management
echo =====================
echo.

if "%1"=="list" goto list_backups
if "%1"=="clean" goto clean_backups
if "%1"=="verify" goto verify_backups
if "%1"=="compress" goto compress_backups

echo Usage: manage_backups.bat ^<command^>
echo.
echo Commands:
echo   list          - List all backups with sizes and dates
echo   clean ^<days^>  - Remove backups older than specified days
echo   verify        - Verify all backups integrity
echo   compress      - Compress all backups to ZIP files
echo.
exit /b 1

:list_backups
echo Available backups:
echo.
if not exist "backup" (
    echo No backup directory found.
    goto end
)

powershell -Command "
Get-ChildItem -Path 'backup' -Directory | Sort-Object LastWriteTime -Descending | ForEach-Object {
    $size = (Get-ChildItem -Path $_.FullName -Recurse -File | Measure-Object -Property Length -Sum).Sum
    $sizeMB = [math]::Round($size / 1MB, 2)
    '{0,-20} {1,-10} {2,8} MB' -f $_.Name, $_.LastWriteTime.ToString('yyyy-MM-dd'), $sizeMB
}
"
goto end

:clean_backups
if "%2"=="" (
    echo Usage: manage_backups.bat clean ^<days^>
    echo Example: manage_backups.bat clean 30
    exit /b 1
)

set DAYS=%2
echo Removing backups older than %DAYS% days...
echo.

powershell -Command "
$cutoff = (Get-Date).AddDays(-%DAYS%)
Get-ChildItem -Path 'backup' -Directory | Where-Object { $_.LastWriteTime -lt $cutoff } | ForEach-Object {
    Write-Host 'Removing:' $_.Name
    Remove-Item $_.FullName -Recurse -Force
}
"
echo Cleanup complete.
goto end

:verify_backups
echo Verifying all backups...
echo.

if not exist "backup" (
    echo No backup directory found.
    goto end
)

for /d %%d in ("backup\*") do (
    echo Verifying %%~nd...
    call packaging\verify_backup.bat "%%d"
    echo.
)
echo All backups verified.
goto end

:compress_backups
echo Compressing all backups...
echo.

if not exist "backup" (
    echo No backup directory found.
    goto end
)

for /d %%d in ("backup\*") do (
    if not exist "%%d.zip" (
        echo Compressing %%~nd...
        powershell -Command "Compress-Archive -Path '%%d' -DestinationPath '%%d.zip'"
        echo Compressed: %%~nd.zip
    ) else (
        echo Already compressed: %%~nd.zip
    )
)
echo Compression complete.
goto end

:end