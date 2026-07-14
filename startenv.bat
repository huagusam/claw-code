@echo off
chcp 65001 >nul
set "TARGET_DIR=C:\Users\Incredible\Code\claw-code\.claw\sessions"
if not exist "%TARGET_DIR%" (
    echo 错误：目录不存在！
    pause
    exit /b 1
)
rd /s /q "%TARGET_DIR%"
md "%TARGET_DIR%"
REM ==============================================================================
REM Claw Code Local Runner (via LM Studio)
REM ==============================================================================
set "VSDEVCMD=H:\Program Files\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat"
if not exist "%VSDEVCMD%" goto ErrorVsDevCmd
echo [1/6] Loading MSVC environment...
call "%VSDEVCMD%" -arch=x64 -no_logo >nul
if errorlevel 1 goto ErrorVsDevCmdFail
echo         MSVC environment loaded.
goto Step2

:ErrorVsDevCmd
echo [FATAL] VsDevCmd.bat not found at: %VSDEVCMD%
pause
exit /b 1
:ErrorVsDevCmdFail
echo [FATAL] VsDevCmd.bat failed.
pause
exit /b 1

REM ==================================================
REM  STEP 2: Set Paths
REM ==================================================
:Step2
echo [2/6] Configuring LIB and INCLUDE paths...

set "VCINSTALLDIR=H:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC\14.44.35207"
set "WINSDK_BASE=H:\Program Files (x86)\Windows Kits\10"
set "WINSDK_VER=10.0.26100.0"

set "LIB=%VCINSTALLDIR%\lib\x64"
set "LIB=%LIB%;%WINSDK_BASE%\Lib\%WINSDK_VER%\um\x64"
set "LIB=%LIB%;%WINSDK_BASE%\Lib\%WINSDK_VER%\ucrt\x64"

set "INCLUDE=%VCINSTALLDIR%\include"
set "INCLUDE=%INCLUDE%;%WINSDK_BASE%\include\%WINSDK_VER%\ucrt"
set "INCLUDE=%INCLUDE%;%WINSDK_BASE%\include\%WINSDK_VER%\um"
set "INCLUDE=%INCLUDE%;%WINSDK_BASE%\include\%WINSDK_VER%\shared"

set "PATH=%PATH%;%WINSDK_BASE%\bin\%WINSDK_VER%\x64"

if not exist "%WINSDK_BASE%\Lib\%WINSDK_VER%\um\x64\kernel32.lib" (
    echo [FATAL] kernel32.lib missing.
    pause
    exit /b 1
)
echo         Paths configured.

REM ==================================================
REM  STEP 3: Configure Clang-CL
REM ==================================================
echo [3/6] Setting Clang-CL compiler...
set "CLANG_BIN=H:\clang+llvm-22.1.2-x86_64-pc-windows-msvc\bin"
if not exist "%CLANG_BIN%\clang-cl.exe" (
    echo [FATAL] clang-cl.exe not found at: %CLANG_BIN%
    pause
    exit /b 1
)
set "CC=%CLANG_BIN%\clang-cl.exe"
set "CXX=%CLANG_BIN%\clang-cl.exe"
set "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER=link.exe"
echo         CC=CXX=%CC%

REM ==================================================
REM  STEP 4: Optional Tools (Safe Parser Version)
REM ==================================================
echo [4/6] Checking optional tools...

REM Check NASM
set "NASM_EXE=C:\Users\Incredible\AppData\Local\bin\NASM\nasm.exe"
if exist "%NASM_EXE%" (
    set "PATH=%PATH%;C:\Users\Incredible\AppData\Local\bin\NASM"
    echo         NASM added.
)

REM Check Perl
set "PERL_EXE=H:\strawberry-perl-5.42.2.1-64bit-portable\perl\bin\perl.exe"
if exist "%PERL_EXE%" (
    set "OPENSSL_SRC_PERL=%PERL_EXE%"
    echo         Perl added.
)


REM ============================================================================
REM  MODE: Switch between LM Studio (local) or Anthropic API (cloud)
REM ============================================================================
REM Uncomment one of the following modes:

REM 3. [CRITICAL] Specify the model name
REM    Must match exactly with the Model ID loaded in LM Studio
REM    Replace with your actual loaded model name, e.g., llama-3-8b-instruct

REM 4. If Claw supports OpenAI adapter mode, try enabling it
REM    Some Rust clients need this flag to switch request formats
REM    (Currently handled by environment variables above)

REM 5. Set log level for debugging connection issues
set RUST_LOG=info

REM 6. Determine Claw binary path
REM    Depending on your build, it might be debug or release
set CLAW_BIN=rust\target\release\claw.exe

if not exist "%CLAW_BIN%" (
    echo [Error] Binary not found at %CLAW_BIN%
    echo Please check if you built with --release or --debug.
    pause
    exit /b 1
)

REM 7. Run Claw
"%CLAW_BIN%" %*
pause