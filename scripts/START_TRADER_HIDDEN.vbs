' START_TRADER_HIDDEN.vbs — Runs the silent trader startup without a visible console window.
' Used by Windows Task Scheduler on login/restart.
'
' The .bat file must be copied to a local Windows path (not WSL UNC):
'   copy scripts\START_TRADER_SILENT.bat C:\Users\andyd\ai-workspace\
'
' This VBS wrapper exists because Task Scheduler cannot hide bat windows natively.
Set WshShell = CreateObject("WScript.Shell")
WshShell.Run chr(34) & "C:\Users\andyd\ai-workspace\START_TRADER_SILENT.bat" & chr(34), 0
Set WshShell = Nothing
