@echo off
setlocal
cd /d "%~dp0"
"%~dp0orbisync-server.exe" web-admin %*
set "launch_exit=%errorlevel%"
if not "%launch_exit%"=="0" pause
exit /b %launch_exit%
