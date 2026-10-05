@echo off
chcp 65001 >nul
cd /d C:\Users\Admin\Desktop\xc

echo ============================================
echo   把 xcpd 推送到新仓库 xcpd-kamiban
echo ============================================
echo.

echo [1/3] 当前 remote:
git remote -v
echo.

echo [2/3] 把 origin 指向新仓库...
git remote set-url origin https://github.com/yinghan21/xcpd-kamiban.git
git remote -v
echo.

echo [3/3] 推送...
git push -u origin main
if errorlevel 1 (
    echo.
    echo !! 推送失败。如果提示 "rejected" 或 "fetch first"，
    echo !! 说明新仓库里有初始文件（比如 GitHub 自动生成的 README），
    echo !! 本地是完整历史可以安全覆盖，改用 --force：
    echo !!     git push -u origin main --force
)
echo.
pause
