@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"
echo 正在编译接收器（cargo build --release）...
echo 需要 Rust 1.85 或更新版本：Cargo.lock 里的依赖（hashbrown / icu_* 等）已使用
echo   edition 2024，1.83 及更早的 cargo 会直接报 "feature `edition2024` is required"，
echo   连依赖都下载不下来。实测 1.95 可编。
rem 版本自检：这台机器上同时装了几套 Rust（PATH 里排前面的可能是 1.83），
rem 1.83 编不了这个工程，但报出来的是「解析 Cargo.lock 失败」，很容易被当成代码问题。
rem 先解析出实际要用的 cargo / rustc 并打印出来：编不过时第一眼看的就是这两行。
call :resolve_tool cargo CARGO CARGO_VER
if errorlevel 1 exit /b 1
call :resolve_tool rustc RUSTC RUSTC_VER
if errorlevel 1 exit /b 1
echo 工具链（本次实际使用）：
echo   cargo: %CARGO%  -  %CARGO_VER%
echo   rustc: %RUSTC%  -  %RUSTC_VER%
call :need_modern cargo "%CARGO_VER%"
if errorlevel 1 exit /b 1
call :need_modern rustc "%RUSTC_VER%"
if errorlevel 1 exit /b 1
goto :do_build

rem ----------------------------------------------------------------
rem 找工具并解析版本：%1 工具名、%2 存路径的变量、%3 存版本的变量。
rem 解析不出就退 1 —— 宁可停在这里，也不要让「用错工具链」的报错出现在依赖里。
:resolve_tool
set "%~2="
set "%~3="
for /f "delims=" %%p in ('where %1 2^>nul') do if not defined %~2 set "%~2=%%p"
if not defined %~2 if exist "%USERPROFILE%\.cargo\bin\%1.exe" set "%~2=%USERPROFILE%\.cargo\bin\%1.exe"
if not defined %~2 (
    echo 找不到 %1，请先安装 Rust: https://rustup.rs/
    pause
    exit /b 1
)
for /f "tokens=2" %%v in ('"%~2" --version') do set "%~3=%%v"
if not defined %~3 (
    echo 探测不出 %1 的版本（"%~2" --version 没有输出可识别的版本号），已停止编译。
    echo 用错工具链时报错会出现在依赖解析里，看不出真因，所以这里必须先停下。
    pause
    exit /b 1
)
exit /b 0

rem 版本下限：%1 工具名、%2 版本号字符串。低于 1.85 或解析不出都给一句人话再停。
:need_modern
set "MAJ="
set "MIN="
for /f "tokens=1,2 delims=." %%a in ("%~2") do (
    set "MAJ=%%a"
    set "MIN=%%b"
)
if not defined MAJ goto :bad_version
if not defined MIN goto :bad_version
if %MAJ% LSS 1 goto :too_old
if %MAJ% EQU 1 if %MIN% LSS 85 goto :too_old
exit /b 0
:too_old
echo.
echo 当前 %1 版本是 %~2，这个工程需要 1.85 或更新版本（Cargo.lock 里的依赖用了 edition 2024）。
echo 请安装新版 Rust（https://rustup.rs/），或把新版放到 PATH 更前面再重试。
pause
exit /b 1
:bad_version
echo.
echo 解析不出 %1 的版本号「%~2」，已停止编译（拒绝在工具链不明的情况下继续）。
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
