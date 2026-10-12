Unicode true
Name "Avelin M0 controlled backend"
OutFile "backend.exe"
RequestExecutionLevel user
SilentInstall silent
SilentUnInstall silent
AutoCloseWindow true
SetCompressor /SOLID lzma
SetOverwrite off
!addplugindir /x86-unicode "."
Var SessionIni
Var PipeName
Var Token
Var TaskId
Var RelPath
Var ParentPath
Var UninstallKey
Var Written
Var StopReason
Var Result
Var ExitCode
Var OwnUninstaller
Var OwnReceipt
Var OwnRegistry
Var OwnRuntime
Var OwnManifest
Var OwnDesktop
Var OwnStartMenu
Var DesktopShortcut
Var StartMenuShortcut
Var ShortcutTarget
Var ShortcutWorkingDir
Var ShortcutEvidence
Var ExpectedHash
Var CleanupIncomplete
Var OwnInternalDirectory
Var FaultAfter
Var CancelAfter
Var StepDelay

!macro InitializeFunction Prefix
Function ${Prefix}Initialize
SetShellVarContext current
SetRegView 64
ReadEnvStr $SessionIni "AVELIN_SESSION_INI"
StrCmp $SessionIni "" init_failed 0
ReadINIStr $INSTDIR "$SessionIni" "task" "install_dir"
ReadINIStr $PipeName "$SessionIni" "task" "pipe_name"
ReadINIStr $Token "$SessionIni" "task" "token"
ReadINIStr $TaskId "$SessionIni" "task" "task_id"
ReadINIStr $FaultAfter "$SessionIni" "task" "fault_after_files"
ReadINIStr $CancelAfter "$SessionIni" "task" "cancel_after_files"
ReadINIStr $StepDelay "$SessionIni" "task" "step_delay_ms"
ReadINIStr $DesktopShortcut "$SessionIni" "task" "desktop_shortcut_path"
ReadINIStr $StartMenuShortcut "$SessionIni" "task" "start_menu_shortcut_path"
ReadINIStr $ShortcutTarget "$SessionIni" "task" "shortcut_target"
ReadINIStr $ShortcutWorkingDir "$SessionIni" "task" "shortcut_working_dir"
ReadINIStr $ShortcutEvidence "$SessionIni" "task" "shortcut_evidence"
StrCmp $INSTDIR "" init_failed 0
InitPluginsDir
SetOutPath "$PLUGINSDIR"
File "package.ini"
ReadINIStr $0 "$PLUGINSDIR\package.ini" "app" "id"
StrCpy $UninstallKey "Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.$0"
avelin_nsis_bridge::Connect "$PipeName" "$Token" "$TaskId"
Pop $0
StrCmp $0 "OK" 0 init_failed
Return
init_failed:
SetErrorLevel 3
Quit
FunctionEnd
!macroend
!insertmacro InitializeFunction ""
!insertmacro InitializeFunction "un."

Function Checkpoint
avelin_nsis_bridge::CheckPoint
Pop $StopReason
FunctionEnd
