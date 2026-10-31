@echo off
chcp 65001 >nul
cd /d "%~dp0"
if not exist "HybridFileXfer-Receiver.exe" (
    echo 还没有编译，请先运行 build.bat
    pause
    exit /b 1
)
start "" "%~dp0HybridFileXfer-Receiver.exe" %*
