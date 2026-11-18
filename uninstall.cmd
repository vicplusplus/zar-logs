@echo off
rem Uninstall zar-logs: runs uninstall.ps1 from this folder. It asks before deleting anything.
setlocal
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0uninstall.ps1" %*
set RESULT=%ERRORLEVEL%
echo.
pause
rem exit code 10: the program was removed, so this file goes too
if "%RESULT%"=="10" (goto) 2>nul & del "%~f0"
