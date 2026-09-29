@echo off
if exist "%~dp0dist\AgentCockpit.exe" (
  start "" "%~dp0dist\AgentCockpit.exe"
) else (
  start "" pythonw "%~dp0agent_cockpit.py"
)
