@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"
echo 正在编译接收器（cargo build --release）...
echo 需要 Rust 1.85 或更新版本（图形界面依赖 eframe）。
where cargo >nul 2>nul
if errorlevel 1 (
    if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
        set "CARGO=%USERPROFILE%\.cargo\bin\cargo.exe"
    ) else (
        echo 找不到 cargo，请先安装 Rust: https://rustup.rs/
        pause
        exit /b 1
    )
) else (
    set "CARGO=cargo"
)
"%CARGO%" build --release
if errorlevel 1 (
    echo.
    echo 编译失败！
    pause
    exit /b 1
)
copy /Y "target\release\HybridFileXfer-Receiver.exe" "HybridFileXfer-Receiver.exe" >nul
copy /Y "target\release\HybridFileXfer-Receiver-cli.exe" "HybridFileXfer-Receiver-cli.exe" >nul
echo.
echo 编译完成：
echo   图形界面：%~dp0HybridFileXfer-Receiver.exe
echo   命令行  ：%~dp0HybridFileXfer-Receiver-cli.exe
pause
