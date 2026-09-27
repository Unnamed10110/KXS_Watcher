@echo off
rem Release build of KXS Watcher.
rem   build.bat        cargo build --release; the exe is copied to target\KXS-Watcher\
rem   build.bat msi    the same, then the MSI installer (build_msi.ps1) in the same folder
setlocal
cd /d "%~dp0"

where cargo >nul 2>nul || (
  echo cargo not found. Install Rust from https://rustup.rs
  exit /b 1
)

cargo build --release || exit /b 1

rem Build output lives outside OneDrive (.cargo\config.toml); ask cargo where.
for /f "delims=" %%d in ('powershell -NoProfile -Command "(cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory"') do set "TARGET=%%d"
if not exist "%TARGET%\release\kxs-watcher.exe" (
  echo Release exe not found in %TARGET%\release
  exit /b 1
)
if not exist "target\KXS-Watcher" mkdir "target\KXS-Watcher"
copy /y "%TARGET%\release\kxs-watcher.exe" "target\KXS-Watcher\kxs-watcher.exe" >nul || exit /b 1
echo.
echo Release exe: %CD%\target\KXS-Watcher\kxs-watcher.exe

if /i "%~1"=="msi" (
  powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build_msi.ps1" -SkipBuild || exit /b 1
)
endlocal
