@echo off
chcp 65001 >nul
cd /d "%~dp0"
echo 这个脚本会把"命令行版（无窗口）"加进开机启动。
echo 如果你想要图形界面版自启，请直接打开 HybridFileXfer-Receiver.exe 勾选"开机自动启动"。
echo.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0autostart.ps1" install
pause
