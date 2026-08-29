@echo off
cd /d "%~dp0"

echo ============================================
echo   Diablo IV Companion - One-Click Push
echo ============================================
echo.

set /p commit_msg="Commit message (Enter for default): "

if "%commit_msg%"=="" (
    set commit_msg=Update
)

echo.
echo [1/3] Staging all changes...
git add -A

echo.
echo [2/3] Committing...
git commit -m "%commit_msg%"

if %errorlevel% neq 0 (
    echo.
    echo Nothing to commit. Already up to date!
    goto :end
)

echo.
echo [3/3] Pushing to GitHub...
git push

if %errorlevel% equ 0 (
    echo.
    echo ============================================
    echo   Done! Pushed to GitHub.
    echo ============================================
) else (
    echo.
    echo Push failed. Check your credentials or network.
)

:end
pause
