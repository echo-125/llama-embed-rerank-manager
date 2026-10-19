@echo off
rem exe, config.json 与模型文件位于上一级目录
cd /d "%~dp0.."
start "" "%~dp0..\llama-rag-gui.exe"
exit
