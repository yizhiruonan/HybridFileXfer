@echo off
chcp 65001 >nul
setlocal
REM 编译 Android 客户端（需要 JDK 17 + Android SDK/NDK）
REM 依赖：platforms;android-34  build-tools;34.0.0  ndk;25.1.8937393  cmake;3.22.1
REM 可以在下面直接改成本机的路径，或者设置好 JAVA_HOME / ANDROID_HOME 环境变量后删掉这两行
if "%JAVA_HOME%"=="" set JAVA_HOME=C:\Users\nil\Desktop\HybridFileXfer-PC\tools\jdk-17
if "%ANDROID_HOME%"=="" set ANDROID_HOME=C:\Users\nil\Desktop\HybridFileXfer-PC\tools\android-sdk

cd /d "%~dp0..\HybridFileXfer-Android"
call gradlew.bat --no-daemon assembleDebug
if errorlevel 1 (
    echo.
    echo 编译失败
    pause
    exit /b 1
)
echo.
echo 编译完成：%~dp0..\HybridFileXfer-Android\app\build\outputs\apk\debug\app-debug.apk
pause
