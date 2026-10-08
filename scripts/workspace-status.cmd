@echo off
setlocal

if not defined STABLE_GIT_COMMIT goto from_git
echo STABLE_GIT_COMMIT %STABLE_GIT_COMMIT%
exit /b 0

:from_git
for /f "delims=" %%I in ('git rev-parse --verify HEAD 2^>nul') do set "BUILD_COMMIT=%%I"
if not defined BUILD_COMMIT goto from_github
echo STABLE_GIT_COMMIT %BUILD_COMMIT%
exit /b 0

:from_github
if not defined GITHUB_SHA goto unknown
echo STABLE_GIT_COMMIT %GITHUB_SHA%
exit /b 0

:unknown
echo STABLE_GIT_COMMIT unknown
