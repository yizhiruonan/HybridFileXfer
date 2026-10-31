@echo off
chcp 65001 >nul
setlocal
REM 编译 Android 客户端
REM 依赖：JDK 17、Android SDK Platform 37、NDK 25.1.8937393、CMake 3.22.1
REM 运行前请先设置好 JAVA_HOME 和 ANDROID_HOME（ANDROID_SDK_ROOT 也可以）
if "%JAVA_HOME%"=="" (
    echo [错误] 没有设置 JAVA_HOME，请先指向 JDK 17 的安装目录
    pause
    exit /b 1
)
if "%ANDROID_HOME%"=="" (
    if "%ANDROID_SDK_ROOT%"=="" (
        echo [错误] 没有设置 ANDROID_HOME，请先指向 Android SDK 的目录
        pause
        exit /b 1
    )
    set ANDROID_HOME=%ANDROID_SDK_ROOT%
)

cd /d "%~dp0..\HybridFileXfer-Android"
call gradlew.bat assembleDebug
if errorlevel 1 (
    echo.
    echo 编译失败
    pause
    exit /b 1
)
echo.
echo 编译完成：%~dp0..\HybridFileXfer-Android\app\build\outputs\apk\debug\app-debug.apk
pause
