@echo off
call "C:\Users\11307\Develop\VSBuildTools\Common7\Tools\VsDevCmd.bat" -startdir=none -arch=arm64 -host_arch=arm64 >nul
cargo %*
