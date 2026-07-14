@echo off
set "TARGET_DIR=.claw\sessions"
set "VSDEVCMD=H:\Program Files\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat"
call "%VSDEVCMD%" -arch=x64 -no_logo >nul

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
set "CLANG_BIN=H:\clang+llvm-22.1.2-x86_64-pc-windows-msvc\bin"
set "CC=%CLANG_BIN%\clang-cl.exe"
set "CXX=%CLANG_BIN%\clang-cl.exe"
set "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER=link.exe"
set "NASM_EXE=C:\Users\Incredible\AppData\Local\bin\NASM\nasm.exe"
if exist "%NASM_EXE%" (
    set "PATH=%PATH%;C:\Users\Incredible\AppData\Local\bin\NASM"
)
set "PERL_EXE=H:\strawberry-perl-5.42.2.1-64bit-portable\perl\bin\perl.exe"
if exist "%PERL_EXE%" (
    set "OPENSSL_SRC_PERL=%PERL_EXE%"
)
set ANTHROPIC_BASE_URL=http://127.0.0.1:1234
set ANTHROPIC_API_KEY=sk-ant-your-key-here
set DISABLE_TELEMETRY=1
set CLAW_WORKSPACE_POLICY=allow
set RUST_LOG=info
set CLAW_BIN=rust\target\release\claw.exe
"%CLAW_BIN%" %*
pause