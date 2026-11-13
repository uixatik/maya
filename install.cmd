@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "& { & '%~dp0install.ps1' %* }"
endlocal
