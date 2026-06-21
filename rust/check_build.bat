@echo off
call "C:\Users\Incredible\openspace\.opencode\CompilePreSet.bat"
cd /d "c:\Users\Incredible\Code\claw-code\rust"
cargo check -p rusty-claude-cli 2>&1
