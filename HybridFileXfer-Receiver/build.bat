@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"
echo 正在编译接收器（cargo build --release）...
echo 需要 Rust 1.85 或更新版本：Cargo.lock 里的依赖（hashbrown / icu_* 等）已使用
echo   edition 2024，1.83 及更早的 cargo 会直接报 "feature `edition2024` is required"，
echo   连依赖都下载不下来。实测 1.95 可编。
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
rem 版本自检：这台机器上同时装了几套 Rust（PATH 里排前面的可能是 1.83），
rem 1.83 编不了这个工程，但报出来的是「解析 Cargo.lock 失败」，很容易被当成代码问题。
for /f "tokens=2" %%v in ('"%CARGO%" --version') do set "CARGO_VER=%%v"
for /f "tokens=1,2 delims=." %%a in ("%CARGO_VER%") do (
    if %%a LSS 1 goto :old_rust
    if %%a EQU 1 if %%b LSS 85 goto :old_rust
)
goto :do_build
:old_rust
echo.
echo 当前 cargo 版本是 %CARGO_VER%，这个工程需要 1.85 或更新版本。
echo 请安装新版 Rust（https://rustup.rs/），或把新版 cargo 放到 PATH 更前面。
pause
exit /b 1
:do_build
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
