//! Generates the NSIS-controlled upgrade transaction. The coordinator supplies
//! a validated old-resource inventory; only this script sequences deployment.
use avelin_core::Manifest;

pub(super) fn generate(manifest: &Manifest) -> String {
    let mut s = include_str!("../../../packaging/nsis/upgrade-header.nsh").to_owned();
    s.push_str(
        r#"
Function Upgrade
StrCpy $Written 0
StrCpy $UpgradeDeleted 0
StrCpy $UpgradeMarkerHash ""
StrCpy $UpgradeJournalFailed 0
StrCpy $UpgradeRegistryTouched 0
StrCpy $CleanupIncomplete 0
StrCpy $OwnRuntime ""
StrCpy $OwnManifest ""
StrCpy $OwnUninstaller ""
StrCpy $OwnReceipt ""
StrCpy $OwnDesktop ""
StrCpy $OwnStartMenu ""
StrCpy $UpgradeStageDesktop ""
StrCpy $UpgradeStageStartMenu ""
ReadINIStr $UpgradePlan "$SessionIni" "task" "upgrade_plan"
ReadINIStr $UpgradeStage "$SessionIni" "task" "upgrade_staging_dir"
ReadINIStr $UpgradeBackup "$SessionIni" "task" "upgrade_backup_dir"
ReadINIStr $UpgradeJournal "$SessionIni" "task" "upgrade_journal"
ReadINIStr $UpgradeMarkerCandidate "$SessionIni" "task" "upgrade_marker_candidate"
!insertmacro UpgradeJournal "begin" "$INSTDIR" "" "preparing"
!insertmacro UpgradeRequireJournal
ReadINIStr $UpgradeCount "$UpgradePlan" "upgrade" "old_count"
StrCmp $UpgradeCount "" upgrade_failed 0
IntCmp $UpgradeCount 4 0 upgrade_failed 0
IntCmp $UpgradeCount 50006 0 0 upgrade_failed
avelin_nsis_bridge::CreateOwnedDirectory "$UpgradeStage"
Pop $0
StrCmp $0 "created" 0 upgrade_failed
avelin_nsis_bridge::CreateOwnedDirectory "$UpgradeBackup"
Pop $0
StrCmp $0 "created" 0 upgrade_failed
SetOutPath "$UpgradeStage"
IfErrors upgrade_failed
avelin_nsis_bridge::Emit "preparing" "0" "__TOTAL_FILES__"
Pop $0
"#,
    );
    for i in 0..manifest.files.len() {
        s.push_str(&format!(
            r#"
ClearErrors
File "/oname=payload-{i:06}" "payload\{i:06}"
IfErrors upgrade_failed
ReadINIStr $ExpectedHash "$PLUGINSDIR\package.ini" "files" "hash{i}"
!insertmacro UpgradeHash "$UpgradeStage\payload-{i:06}" "$ExpectedHash"
avelin_nsis_bridge::Emit "extracting" "{i}" "__TOTAL_FILES__"
Pop $0
"#
        ));
    }
    s.push_str(r#"
ClearErrors
File "/oname=runtime.exe" "runtime.exe"
File "manifest.json"
WriteUninstaller "$UpgradeStage\uninstall.exe"
IfErrors upgrade_failed
ReadINIStr $ExpectedHash "$PLUGINSDIR\package.ini" "app" "runtime_sha256"
!insertmacro UpgradeHash "$UpgradeStage\runtime.exe" "$ExpectedHash"
StrCpy $UpgradeRuntimeHash $ExpectedHash
ReadINIStr $ExpectedHash "$PLUGINSDIR\package.ini" "app" "manifest_sha256"
!insertmacro UpgradeHash "$UpgradeStage\manifest.json" "$ExpectedHash"
StrCpy $UpgradeManifestHash $ExpectedHash
avelin_nsis_bridge::OwnedFileHash "$UpgradeStage\uninstall.exe"
Pop $UpgradeUninstallerHash
StrCpy $0 $UpgradeUninstallerHash 6
StrCmp $0 "error:" upgrade_failed 0
StrCmp $DesktopShortcut "" upgrade_stage_desktop_done 0
SetOutPath "$ShortcutWorkingDir"
ClearErrors
CreateShortCut "$UpgradeStage\desktop.lnk" "$ShortcutTarget"
IfErrors upgrade_failed
StrCpy $UpgradeStageDesktop "$UpgradeStage\desktop.lnk"
avelin_nsis_bridge::OwnedFileHash "$UpgradeStageDesktop"
Pop $UpgradeDesktopHash
StrCpy $0 $UpgradeDesktopHash 6
StrCmp $0 "error:" upgrade_failed 0
upgrade_stage_desktop_done:
StrCmp $StartMenuShortcut "" upgrade_stage_start_done 0
SetOutPath "$ShortcutWorkingDir"
ClearErrors
CreateShortCut "$UpgradeStage\start-menu.lnk" "$ShortcutTarget"
IfErrors upgrade_failed
StrCpy $UpgradeStageStartMenu "$UpgradeStage\start-menu.lnk"
avelin_nsis_bridge::OwnedFileHash "$UpgradeStageStartMenu"
Pop $UpgradeStartMenuHash
StrCpy $0 $UpgradeStartMenuHash 6
StrCmp $0 "error:" upgrade_failed 0
upgrade_stage_start_done:
SetOutPath "$UpgradeStage"
ReadINIStr $0 "$SessionIni" "task" "receipt_candidate"
avelin_nsis_bridge::PrepareUpgradeReceipt "$0" "$UpgradeStage\uninstall.exe" "$UpgradeStageDesktop" "$UpgradeStageStartMenu" "$UpgradeStage\receipt.json"
Pop $ExpectedHash
StrCpy $0 $ExpectedHash 6
StrCmp $0 "error:" upgrade_failed 0
StrLen $0 $ExpectedHash
StrCmp $0 64 0 upgrade_failed
StrCpy $UpgradeReceiptHash $ExpectedHash
!insertmacro UpgradeHash "$UpgradeStage\uninstall.exe" "$UpgradeUninstallerHash"
StrCmp $UpgradeStageDesktop "" upgrade_receipt_desktop_verified 0
!insertmacro UpgradeHash "$UpgradeStageDesktop" "$UpgradeDesktopHash"
upgrade_receipt_desktop_verified:
StrCmp $UpgradeStageStartMenu "" upgrade_receipt_start_verified 0
!insertmacro UpgradeHash "$UpgradeStageStartMenu" "$UpgradeStartMenuHash"
upgrade_receipt_start_verified:
!insertmacro UpgradeJournal "staged" "$UpgradeStage\receipt.json" "$ExpectedHash" "all_new_resources_ready"
!insertmacro UpgradeRequireJournal
StrCpy $UpgradeIndex 0
upgrade_backup_loop:
IntCmp $UpgradeIndex $UpgradeCount upgrade_backup_complete 0 upgrade_failed
ReadINIStr $RelPath "$UpgradePlan" "old" "path$UpgradeIndex"
ReadINIStr $ExpectedHash "$UpgradePlan" "old" "hash$UpgradeIndex"
!insertmacro UpgradeJournal "backup_intent" "$RelPath" "$ExpectedHash" "$UpgradeIndex"
!insertmacro UpgradeRequireJournal
avelin_nsis_bridge::PublishOwnedFile "$RelPath" "$UpgradeBackup\old-$UpgradeIndex.bin" "$ExpectedHash"
Pop $0
StrCmp $0 $ExpectedHash 0 upgrade_failed
!insertmacro UpgradeJournal "backed_up" "$RelPath" "$ExpectedHash" "$UpgradeIndex"
!insertmacro UpgradeRequireJournal
IntOp $UpgradeIndex $UpgradeIndex + 1
Goto upgrade_backup_loop
upgrade_backup_complete:
StrCpy $UpgradeIndex 0
upgrade_verify_backup_loop:
IntCmp $UpgradeIndex $UpgradeCount upgrade_backup_verified 0 upgrade_failed
ReadINIStr $RelPath "$UpgradePlan" "old" "path$UpgradeIndex"
ReadINIStr $ExpectedHash "$UpgradePlan" "old" "hash$UpgradeIndex"
!insertmacro UpgradeHash "$RelPath" "$ExpectedHash"
!insertmacro UpgradeHash "$UpgradeBackup\old-$UpgradeIndex.bin" "$ExpectedHash"
IntOp $UpgradeIndex $UpgradeIndex + 1
Goto upgrade_verify_backup_loop
upgrade_backup_verified:
"#);
    for (name, variable, kind) in registry_fields() {
        s.push_str(&format!("ClearErrors\nReadReg{kind} ${variable} HKCU \"$UninstallKey\" \"{name}\"\nIfErrors upgrade_failed\n!insertmacro UpgradeJournal \"registry_snapshot\" \"{name}\" \"\" \"${variable}\"\n!insertmacro UpgradeRequireJournal\n"));
    }
    s.push_str(r#"
avelin_nsis_bridge::PublishOwnedFile "$UpgradeMarkerCandidate" "$INSTDIR\.avelin\upgrade-state.json" ""
Pop $0
StrCpy $1 $0 6
StrCmp $1 "error:" upgrade_failed 0
StrLen $1 $0
StrCmp $1 64 0 upgrade_failed
StrCpy $UpgradeMarkerHash $0
!insertmacro UpgradeJournal "marker_created" "$INSTDIR\.avelin\upgrade-state.json" "$UpgradeMarkerHash" "active"
!insertmacro UpgradeRequireJournal
avelin_nsis_bridge::Emit "finalizing" "__TOTAL_FILES__" "__TOTAL_FILES__"
Pop $0
StrCpy $UpgradeIndex 0
upgrade_delete_loop:
IntCmp $UpgradeIndex $UpgradeCount upgrade_old_removed 0 upgrade_failed
ReadINIStr $RelPath "$UpgradePlan" "old" "path$UpgradeIndex"
ReadINIStr $ExpectedHash "$UpgradePlan" "old" "hash$UpgradeIndex"
!insertmacro UpgradeJournal "delete_intent" "$RelPath" "$ExpectedHash" "$UpgradeIndex"
!insertmacro UpgradeRequireJournal
avelin_nsis_bridge::DeleteOwnedFile "$RelPath" "$ExpectedHash"
Pop $1
StrCmp $1 "deleted" upgrade_deleted 0
StrCmp $1 "missing" upgrade_deleted upgrade_failed
upgrade_deleted:
IntOp $UpgradeDeleted $UpgradeDeleted + 1
!insertmacro UpgradeJournal "deleted" "$RelPath" "$ExpectedHash" "$1"
!insertmacro UpgradeRequireJournal
IntOp $UpgradeIndex $UpgradeIndex + 1
Goto upgrade_delete_loop
upgrade_old_removed:
"#);
    for i in 0..manifest.files.len() {
        s.push_str(&format!(r#"
ReadINIStr $RelPath "$PLUGINSDIR\package.ini" "files" "path{i}"
ReadINIStr $ParentPath "$PLUGINSDIR\package.ini" "files" "dir{i}"
ReadINIStr $ExpectedHash "$PLUGINSDIR\package.ini" "files" "hash{i}"
ClearErrors
CreateDirectory "$INSTDIR\$ParentPath"
IfErrors upgrade_failed
!insertmacro UpgradeJournal "publish_intent" "$INSTDIR\$RelPath" "$ExpectedHash" "payload"
!insertmacro UpgradeRequireJournal
avelin_nsis_bridge::PublishOwnedFile "$UpgradeStage\payload-{i:06}" "$INSTDIR\$RelPath" "$ExpectedHash"
Pop $0
StrCpy $1 $0 32
StrCmp $1 "error:publish:cleanup_incomplete" 0 +2
StrCpy $CleanupIncomplete 1
StrCmp $0 $ExpectedHash 0 upgrade_failed
IntOp $Written $Written + 1
!insertmacro UpgradeJournal "published" "$INSTDIR\$RelPath" "$ExpectedHash" "payload"
!insertmacro UpgradeRequireJournal
"#));
    }
    for (source, target, owner) in new_resources(false) {
        s.push_str(&publish_resource(source, target, owner));
    }
    s.push_str(
        r#"
StrCmp $DesktopShortcut "" upgrade_publish_desktop_done 0
"#,
    );
    s.push_str(&publish_resource(
        "desktop.lnk",
        "$DesktopShortcut",
        "OwnDesktop",
    ));
    s.push_str("WriteINIStr \"$ShortcutEvidence\" \"shortcuts\" \"desktop_sha256\" \"$OwnDesktop\"\nIfErrors upgrade_failed\nupgrade_publish_desktop_done:\nStrCmp $StartMenuShortcut \"\" upgrade_publish_start_done 0\n");
    s.push_str(&publish_resource(
        "start-menu.lnk",
        "$StartMenuShortcut",
        "OwnStartMenu",
    ));
    s.push_str("WriteINIStr \"$ShortcutEvidence\" \"shortcuts\" \"start_menu_sha256\" \"$OwnStartMenu\"\nIfErrors upgrade_failed\nupgrade_publish_start_done:\nStrCpy $UpgradeRegistryTouched 1\nClearErrors\n");
    s.push_str(&write_new_registry());
    s.push_str(r#"
IfErrors upgrade_failed
!insertmacro UpgradeJournal "registry_updated" "$UninstallKey" "" "known_fields_only"
!insertmacro UpgradeRequireJournal
!insertmacro UpgradeJournal "commit_intent" "$INSTDIR\.avelin-receipt.json" "" "publish_complete_receipt_last"
!insertmacro UpgradeRequireJournal
"#);
    s.push_str(&publish_resource(
        "receipt.json",
        r"$INSTDIR\.avelin-receipt.json",
        "OwnReceipt",
    ));
    for i in 0..manifest.files.len() {
        s.push_str(&format!("ReadINIStr $RelPath \"$PLUGINSDIR\\package.ini\" \"files\" \"path{i}\"\nReadINIStr $ExpectedHash \"$PLUGINSDIR\\package.ini\" \"files\" \"hash{i}\"\n!insertmacro UpgradeHash \"$INSTDIR\\$RelPath\" \"$ExpectedHash\"\n"));
    }
    for (_, target, owner) in new_resources(true) {
        s.push_str(&format!(
            "!insertmacro UpgradeHash \"{target}\" \"${owner}\"\n"
        ));
    }
    s.push_str("StrCmp $OwnDesktop \"\" upgrade_verify_desktop_done 0\n!insertmacro UpgradeHash \"$DesktopShortcut\" \"$OwnDesktop\"\nupgrade_verify_desktop_done:\nStrCmp $OwnStartMenu \"\" upgrade_verify_start_done 0\n!insertmacro UpgradeHash \"$StartMenuShortcut\" \"$OwnStartMenu\"\nupgrade_verify_start_done:\n");
    s.push_str(&verify_new_registry());
    s.push_str(
        r#"
ReadINIStr $UpgradeDirectoryCount "$UpgradePlan" "old_directories" "count"
StrCmp $UpgradeDirectoryCount "" upgrade_directories_done 0
StrCpy $UpgradeIndex 0
upgrade_directory_loop:
IntCmp $UpgradeIndex $UpgradeDirectoryCount upgrade_directories_done 0 upgrade_directories_done
ReadINIStr $RelPath "$UpgradePlan" "old_directories" "path$UpgradeIndex"
StrCmp $RelPath "" upgrade_directory_next 0
StrCmp $RelPath ".avelin" upgrade_directory_next 0
ClearErrors
RMDir "$INSTDIR\$RelPath"
StrCpy $1 "preserved"
IfErrors +2 0
StrCpy $1 "removed_empty_directory"
!insertmacro UpgradeJournal "directory_cleanup" "$INSTDIR\$RelPath" "" "$1"
upgrade_directory_next:
IntOp $UpgradeIndex $UpgradeIndex + 1
Goto upgrade_directory_loop
upgrade_directories_done:
"#,
    );
    s.push_str(
        r#"
!insertmacro UpgradeJournal "committed" "$INSTDIR\.avelin-receipt.json" "$OwnReceipt" "verified"
StrCmp $UpgradeJournalFailed 0 0 upgrade_unknown
avelin_nsis_bridge::DeleteOwnedFile "$INSTDIR\.avelin\upgrade-state.json" "$UpgradeMarkerHash"
Pop $0
StrCmp $0 "deleted" 0 upgrade_unknown
avelin_nsis_bridge::Finish "Succeeded" "0"
Pop $0
SetErrorLevel 0
Quit
upgrade_failed:
avelin_nsis_bridge::LastError
Pop $UpgradeFailure
StrCpy $0 $UpgradeFailure 32
StrCmp $0 "error:publish:cleanup_incomplete" 0 +2
StrCpy $CleanupIncomplete 1
!insertmacro UpgradeJournal "error" "$INSTDIR" "" "$UpgradeFailure"
StrCmp $UpgradeMarkerHash "" upgrade_failed_no_mutation 0
!insertmacro UpgradeJournal "rollback_begin" "$INSTDIR" "" "bounded_in_process_restore"
SetOutPath "$UpgradeStage"
"#,
    );
    for (_, target, owner) in new_resources(true).into_iter().rev().chain([
        ("", "$DesktopShortcut", "OwnDesktop"),
        ("", "$StartMenuShortcut", "OwnStartMenu"),
    ]) {
        s.push_str(&format!("StrCmp ${owner} \"\" upgrade_rollback_{owner}_done 0\n!insertmacro UpgradeRollbackDelete \"{target}\" \"${owner}\"\nupgrade_rollback_{owner}_done:\n"));
    }
    for i in (0..manifest.files.len()).rev() {
        s.push_str(&format!("IntCmp $Written {i} upgrade_rollback_payload{i}_done upgrade_rollback_payload{i}_done 0\nReadINIStr $RelPath \"$PLUGINSDIR\\package.ini\" \"files\" \"path{i}\"\nReadINIStr $ExpectedHash \"$PLUGINSDIR\\package.ini\" \"files\" \"hash{i}\"\n!insertmacro UpgradeRollbackDelete \"$INSTDIR\\$RelPath\" \"$ExpectedHash\"\nupgrade_rollback_payload{i}_done:\n"));
    }
    s.push_str(r#"
StrCpy $UpgradeIndex $UpgradeDeleted
upgrade_restore_loop:
IntCmp $UpgradeIndex 0 upgrade_restore_complete upgrade_restore_complete 0
IntOp $UpgradeIndex $UpgradeIndex - 1
ReadINIStr $RelPath "$UpgradePlan" "old" "path$UpgradeIndex"
ReadINIStr $ExpectedHash "$UpgradePlan" "old" "hash$UpgradeIndex"
avelin_nsis_bridge::PublishOwnedFile "$UpgradeBackup\old-$UpgradeIndex.bin" "$RelPath" "$ExpectedHash"
Pop $1
StrCmp $1 $ExpectedHash +2 0
StrCpy $CleanupIncomplete 1
!insertmacro UpgradeJournal "rollback_restore" "$RelPath" "$ExpectedHash" "$1"
Goto upgrade_restore_loop
upgrade_restore_complete:
StrCmp $UpgradeRegistryTouched 1 0 upgrade_restore_registry_done
ClearErrors
"#);
    for (name, variable, kind) in registry_fields() {
        s.push_str(&format!(
            "WriteReg{kind} HKCU \"$UninstallKey\" \"{name}\" ${variable}\n"
        ));
    }
    s.push_str("IfErrors 0 +2\nStrCpy $CleanupIncomplete 1\nupgrade_restore_registry_done:\n");
    for (name, variable, kind) in registry_fields() {
        s.push_str(&format!("ClearErrors\nReadReg{kind} $1 HKCU \"$UninstallKey\" \"{name}\"\nIfErrors 0 +2\nStrCpy $CleanupIncomplete 1\nStrCmp $1 ${variable} +2 0\nStrCpy $CleanupIncomplete 1\n"));
    }
    s.push_str(
        r#"
StrCpy $UpgradeIndex 0
upgrade_verify_restore_loop:
IntCmp $UpgradeIndex $UpgradeCount upgrade_verify_restore_done 0 upgrade_rollback_incomplete
ReadINIStr $RelPath "$UpgradePlan" "old" "path$UpgradeIndex"
ReadINIStr $ExpectedHash "$UpgradePlan" "old" "hash$UpgradeIndex"
avelin_nsis_bridge::OwnedFileHash "$RelPath"
Pop $0
StrCmp $0 $ExpectedHash +2 0
StrCpy $CleanupIncomplete 1
IntOp $UpgradeIndex $UpgradeIndex + 1
Goto upgrade_verify_restore_loop
upgrade_verify_restore_done:
StrCmp $CleanupIncomplete 0 0 upgrade_rollback_incomplete
StrCmp $UpgradeJournalFailed 0 0 upgrade_rollback_incomplete
!insertmacro UpgradeJournal "rolled_back" "$INSTDIR" "" "old_resources_and_registry_verified"
StrCmp $UpgradeJournalFailed 0 0 upgrade_unknown
avelin_nsis_bridge::DeleteOwnedFile "$INSTDIR\.avelin\upgrade-state.json" "$UpgradeMarkerHash"
Pop $0
StrCmp $0 "deleted" 0 upgrade_unknown
upgrade_failed_no_mutation:
avelin_nsis_bridge::Finish "Failed" "3"
Pop $0
SetErrorLevel 3
Quit
upgrade_rollback_incomplete:
!insertmacro UpgradeJournal "rollback_incomplete" "$INSTDIR" "" "preserve_marker_backup_and_staging"
upgrade_unknown:
avelin_nsis_bridge::Finish "Unknown" "4"
Pop $0
SetErrorLevel 4
Quit
FunctionEnd
"#,
    );
    s.replace("__TOTAL_FILES__", &manifest.files.len().to_string())
}

fn new_resources(receipt: bool) -> Vec<(&'static str, &'static str, &'static str)> {
    let mut resources = vec![
        (
            "runtime.exe",
            r"$INSTDIR\.avelin\avelin-app.exe",
            "OwnRuntime",
        ),
        (
            "manifest.json",
            r"$INSTDIR\.avelin\manifest.json",
            "OwnManifest",
        ),
        ("uninstall.exe", r"$INSTDIR\uninstall.exe", "OwnUninstaller"),
    ];
    if receipt {
        resources.push((
            "receipt.json",
            r"$INSTDIR\.avelin-receipt.json",
            "OwnReceipt",
        ));
    }
    resources
}

fn publish_resource(source: &str, target: &str, owner: &str) -> String {
    let expected = match source {
        "runtime.exe" => "UpgradeRuntimeHash",
        "manifest.json" => "UpgradeManifestHash",
        "uninstall.exe" => "UpgradeUninstallerHash",
        "receipt.json" => "UpgradeReceiptHash",
        "desktop.lnk" => "UpgradeDesktopHash",
        "start-menu.lnk" => "UpgradeStartMenuHash",
        _ => unreachable!("fixed staged resource"),
    };
    format!(
        "!insertmacro UpgradePublish \"$UpgradeStage\\{source}\" \"{target}\" \"${expected}\" ${owner}\n"
    )
}

fn registry_fields() -> [(&'static str, &'static str, &'static str); 7] {
    [
        ("DisplayName", "UpgradeOldName", "Str"),
        ("DisplayVersion", "UpgradeOldVersion", "Str"),
        ("InstallLocation", "UpgradeOldLocation", "Str"),
        ("UninstallString", "UpgradeOldNormal", "Str"),
        ("QuietUninstallString", "UpgradeOldQuiet", "Str"),
        ("NoModify", "UpgradeOldNoModify", "DWORD"),
        ("NoRepair", "UpgradeOldNoRepair", "DWORD"),
    ]
}

fn write_new_registry() -> String {
    r#"ReadINIStr $0 "$PLUGINSDIR\package.ini" "app" "name"
WriteRegStr HKCU "$UninstallKey" "DisplayName" "$0"
ReadINIStr $0 "$PLUGINSDIR\package.ini" "app" "version"
WriteRegStr HKCU "$UninstallKey" "DisplayVersion" "$0"
WriteRegStr HKCU "$UninstallKey" "InstallLocation" "$INSTDIR"
WriteRegStr HKCU "$UninstallKey" "UninstallString" '"$INSTDIR\.avelin\avelin-app.exe" --gui --operation uninstall --backend "$INSTDIR\uninstall.exe" --manifest "$INSTDIR\.avelin\manifest.json" --install-dir "$INSTDIR"'
WriteRegStr HKCU "$UninstallKey" "QuietUninstallString" '"$INSTDIR\.avelin\avelin-app.exe" --silent --operation uninstall --backend "$INSTDIR\uninstall.exe" --manifest "$INSTDIR\.avelin\manifest.json" --install-dir "$INSTDIR"'
WriteRegDWORD HKCU "$UninstallKey" "NoModify" 1
WriteRegDWORD HKCU "$UninstallKey" "NoRepair" 1
"#.into()
}

fn verify_new_registry() -> String {
    let mut s = String::new();
    for (name, expected) in [
        ("DisplayName", "$2"),
        ("DisplayVersion", "$3"),
        ("InstallLocation", "$INSTDIR"),
        ("UninstallString", "$4"),
        ("QuietUninstallString", "$5"),
    ] {
        if name == "DisplayName" {
            s.push_str(r#"ReadINIStr $2 "$PLUGINSDIR\package.ini" "app" "name"
ReadINIStr $3 "$PLUGINSDIR\package.ini" "app" "version"
StrCpy $4 '"$INSTDIR\.avelin\avelin-app.exe" --gui --operation uninstall --backend "$INSTDIR\uninstall.exe" --manifest "$INSTDIR\.avelin\manifest.json" --install-dir "$INSTDIR"'
StrCpy $5 '"$INSTDIR\.avelin\avelin-app.exe" --silent --operation uninstall --backend "$INSTDIR\uninstall.exe" --manifest "$INSTDIR\.avelin\manifest.json" --install-dir "$INSTDIR"'
"#);
        }
        s.push_str(&format!("ClearErrors\nReadRegStr $1 HKCU \"$UninstallKey\" \"{name}\"\nIfErrors upgrade_failed\nStrCmp $1 {expected} 0 upgrade_failed\n"));
    }
    for name in ["NoModify", "NoRepair"] {
        s.push_str(&format!("ClearErrors\nReadRegDWORD $1 HKCU \"$UninstallKey\" \"{name}\"\nIfErrors upgrade_failed\nStrCmp $1 1 0 upgrade_failed\n"));
    }
    s
}
