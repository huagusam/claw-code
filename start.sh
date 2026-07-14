#!/usr/bin/env bash
# start.sh - Bash version of start.bat for MSYS2/WSL terminal

set -e

# Session directory
export TARGET_DIR="C:/Users/Incredible/Code/claw-code/.claw/sessions"

# MSVC Environment
export VCINSTALLDIR="H:/Program Files/Microsoft Visual Studio/2022/Community/VC/Tools/MSVC/14.44.35207"
export WINSDK_BASE="H:/Program Files (x86)/Windows Kits/10"
export WINSDK_VER="10.0.26100.0"

# Library paths
export LIB="$VCINSTALLDIR/lib/x64"
export LIB="$LIB;$WINSDK_BASE/Lib/$WINSDK_VER/um/x64"
export LIB="$LIB;$WINSDK_BASE/Lib/$WINSDK_VER/ucrt/x64"

# Include paths
export INCLUDE="$VCINSTALLDIR/include"
export INCLUDE="$INCLUDE;$WINSDK_BASE/include/$WINSDK_VER/ucrt"
export INCLUDE="$INCLUDE;$WINSDK_BASE/include/$WINSDK_VER/um"
export INCLUDE="$INCLUDE;$WINSDK_BASE/include/$WINSDK_VER/shared"

# PATH additions
export PATH="$PATH:$WINSDK_BASE/bin/$WINSDK_VER/x64"

# Clang compiler
export CLANG_BIN="H:/clang+llvm-22.1.2-x86_64-pc-windows-msvc/bin"
export CC="$CLANG_BIN/clang-cl.exe"
export CXX="$CLANG_BIN/clang-cl.exe"
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER="link.exe"

# NASM (optional)
NASM_EXE="C:/Users/Incredible/AppData/Local/bin/NASM/nasm.exe"
if [ -f "$NASM_EXE" ]; then
    export PATH="$PATH:C:/Users/Incredible/AppData/Local/bin/NASM"
fi

# Perl (optional)
PERL_EXE="H:/strawberry-perl-5.42.2.1-64bit-portable/perl/bin/perl.exe"
if [ -f "$PERL_EXE" ]; then
    export OPENSSL_SRC_PERL="$PERL_EXE"
fi

# API configuration
export ANTHROPIC_BASE_URL="http://127.0.0.1:1234"
export ANTHROPIC_API_KEY="sk-ant-your-key-here"

# Shell configuration
export CLAW_WORKSPACE_POLICY=allow

# Binary path
CLAW_BIN="rust/target/release/claw.exe"

# Run
"$CLAW_BIN" "$@"
