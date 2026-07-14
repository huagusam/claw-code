@echo off
chcp 65001 >nul

REM ============================================================================
REM  MODE: LM Studio (local llama.cpp) - OpenAI Compatible
REM ============================================================================
REM KEY: Use OPENAI_BASE_URL for LM Studio (enables /v1/chat/completions)
set OPENAI_BASE_URL=http://127.0.0.1:1234
set OPENAI_API_KEY=dummy
set ANTHROPIC_API_KEY=dummy

REM Model name - prefix with "openai/" to force OpenAI-compatible endpoint
set ANTHROPIC_MODEL=qwen

set CLAW_BIN=rust\target\release\claw.exe

if not exist "%CLAW_BIN%" (
    echo [Error] Binary not found at %CLAW_BIN%
    echo Run build.bat first.
    pause
    exit /b 1
)

echo ========================================
echo Starting Claw Code with LM Studio...
echo Base URL : %OPENAI_BASE_URL%
echo ========================================
echo.

"%CLAW_BIN%" %*
pause