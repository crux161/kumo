@echo off
setlocal

set "FORMAT=%~1"
if "%FORMAT%"=="" set "FORMAT=iso"

if /I not "%FORMAT%"=="iso" if /I not "%FORMAT%"=="img" (
    echo usage: build.amd64.bat [iso^|img]
    exit /b 2
)

where bash >nul 2>nul
if errorlevel 1 (
    echo error: bash is required to run the shared KUMO media builder
    exit /b 1
)

pushd "%~dp0"
bash ./build.sh amd64 "%FORMAT%"
set "RESULT=%ERRORLEVEL%"
popd
exit /b %RESULT%
