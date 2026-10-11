@echo off
chcp 65001 >nul
REM Agent 驾驶舱启动脚本：优先 bin 里的正式版，其次编译产物，最后回退 Python 旧版
set "BIN=%~dp0bin\AgentCockpit.exe"
set "REL=%~dp0src-tauri\target\release\agent-cockpit.exe"
if exist "%BIN%" (
  start "" "%BIN%"
  exit /b
)
if exist "%REL%" (
  start "" "%REL%"
  exit /b
)
echo 未找到原生程序，回退 Python 版（需 pip install -r legacy-python\requirements.txt）
start "" pythonw "%~dp0legacy-python\agent_cockpit.py"
