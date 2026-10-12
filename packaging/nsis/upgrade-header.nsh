Var UpgradePlan
Var UpgradeStage
Var UpgradeBackup
Var UpgradeJournal
Var UpgradeMarkerCandidate
Var UpgradeMarkerHash
Var UpgradeCount
Var UpgradeDirectoryCount
Var UpgradeIndex
Var UpgradeDeleted
Var UpgradeJournalFailed
Var UpgradeRegistryTouched
Var UpgradeStageDesktop
Var UpgradeStageStartMenu
Var UpgradeFailure
Var UpgradeRuntimeHash
Var UpgradeManifestHash
Var UpgradeUninstallerHash
Var UpgradeReceiptHash
Var UpgradeDesktopHash
Var UpgradeStartMenuHash
Var UpgradeOldName
Var UpgradeOldVersion
Var UpgradeOldLocation
Var UpgradeOldNormal
Var UpgradeOldQuiet
Var UpgradeOldNoModify
Var UpgradeOldNoRepair

!macro UpgradeJournal Event Path Hash Result
avelin_nsis_bridge::AppendUpgradeJournal "$UpgradeJournal" "$TaskId" "${Event}" "${Path}" "${Hash}" "${Result}"
Pop $0
StrCmp $0 "OK" +2 0
StrCpy $UpgradeJournalFailed 1
!macroend

!macro UpgradeRequireJournal
StrCmp $UpgradeJournalFailed 0 0 upgrade_failed
!macroend

!macro UpgradeHash Path Expected
avelin_nsis_bridge::OwnedFileHash "${Path}"
Pop $0
StrCmp $0 "${Expected}" 0 upgrade_failed
!macroend

!macro UpgradePublish Source Target Hash Owner
!insertmacro UpgradeJournal "publish_intent" "${Target}" "${Hash}" ""
!insertmacro UpgradeRequireJournal
avelin_nsis_bridge::PublishOwnedFile "${Source}" "${Target}" "${Hash}"
Pop $0
StrCpy $1 $0 32
StrCmp $1 "error:publish:cleanup_incomplete" 0 +2
StrCpy $CleanupIncomplete 1
StrCmp $0 "${Hash}" 0 upgrade_failed
StrCpy ${Owner} $0
!insertmacro UpgradeJournal "published" "${Target}" "${Hash}" "created"
!insertmacro UpgradeRequireJournal
!macroend

!macro UpgradeRollbackDelete Path Hash
avelin_nsis_bridge::DeleteOwnedFile "${Path}" "${Hash}"
Pop $1
!insertmacro UpgradeJournal "rollback_delete" "${Path}" "${Hash}" "$1"
StrCmp $1 "deleted" +3 0
StrCmp $1 "missing" +2 0
StrCpy $CleanupIncomplete 1
!macroend
