@echo off
REM 冻结为单文件 exe（需先 pip install pyinstaller）
python -m PyInstaller --onefile --noconsole --name AgentCockpit --collect-all pywebview --collect-submodules pystray agent_cockpit.py
echo 产物在 dist\AgentCockpit.exe
