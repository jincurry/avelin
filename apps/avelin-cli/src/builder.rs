use anyhow::{Context, Result, ensure};
#[path = "upgrade_builder.rs"]
mod upgrade;
use avelin_core::{
    FileResource, Manifest, SCHEMA_VERSION, hash_file, reject_reparse, relative_path,
};
use serde::Deserialize;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    schema_version: u32,
    app: App,
    payload: Payload,
    install: Install,
    #[serde(default)]
    ui: Option<crate::presentation_config::UiConfig>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct App {
    id: String,
    name: String,
    version: String,
    entry: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    directory: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Install {
    directory_name: String,
    #[serde(default)]
    desktop_shortcut: bool,
    #[serde(default)]
    start_menu_shortcut: bool,
}

fn inventory(base: &Path, at: &Path, files: &mut Vec<FileResource>) -> Result<()> {
    reject_reparse(at)?;
    for entry in fs::read_dir(at)? {
        let path = entry?.path();
        reject_reparse(&path)?;
        if path.is_dir() {
            inventory(base, &path, files)?;
        } else if path.is_file() {
            let rel = relative_path(
                path.strip_prefix(base)?
                    .to_str()
                    .context("non-Unicode filename")?,
            )?;
            files.push(FileResource {
                path: rel,
                size: fs::metadata(&path)?.len(),
                sha256: hash_file(&path)?,
            });
        } else {
            anyhow::bail!("unsupported payload entry: {}", path.display());
        }
    }
    Ok(())
}
pub fn load(config_path: &Path) -> Result<(PathBuf, Manifest)> {
    let config_path = std::path::absolute(config_path).context("resolve configuration path")?;
    reject_reparse(&config_path)?;
    let text = fs::read_to_string(&config_path).with_context(|| {
        format!(
            "read configuration {}; pass --config <existing avelin.toml> or create one with avelin-cli init --help",
            config_path.display()
        )
    })?;
    load_text(config_path.parent().context("config parent")?, &text)
}

/// `init` validates serialized TOML through exactly the same path as `check`.
pub(crate) fn load_text(config_directory: &Path, text: &str) -> Result<(PathBuf, Manifest)> {
    let cfg: Config = toml::from_str(text)
        .context("parse avelin.toml; check section names and TOML string quoting")?;
    ensure!(
        cfg.schema_version == SCHEMA_VERSION,
        "unsupported config schema"
    );
    let source = config_directory.join(cfg.payload.directory);
    reject_reparse(&std::path::absolute(&source)?)?;
    ensure!(
        source.is_dir(),
        "payload directory is missing or is not a directory: {}; set [payload].directory to existing application files (relative to avelin.toml or absolute)",
        source.display()
    );
    let source = fs::canonicalize(&source)
        .with_context(|| format!("read payload directory {}", source.display()))?;
    let mut files = Vec::new();
    inventory(&source, &source, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        app_id: cfg.app.id,
        app_name: cfg.app.name,
        app_version: cfg.app.version,
        entry: relative_path(&cfg.app.entry)?,
        directory_name: cfg.install.directory_name,
        desktop_shortcut: cfg.install.desktop_shortcut,
        start_menu_shortcut: cfg.install.start_menu_shortcut,
        files,
        runtime_sha256: None,
        presentation: cfg
            .ui
            .as_ref()
            .map(|ui| ui.load(config_directory))
            .transpose()?,
    };
    manifest.validate().context(
        "validate app/payload configuration; [app].entry must name an existing payload file",
    )?;
    ensure!(
        !manifest.app_name.contains(['\"', '[', ']', '=']),
        "M0 INI display name excludes quotes/brackets/equals"
    );
    Ok((source, manifest))
}

fn write_ini(path: &Path, value: &str) -> Result<()> {
    let mut bytes = vec![0xff, 0xfe];
    for c in value.encode_utf16() {
        bytes.extend(c.to_le_bytes());
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn create_output(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create new build output {}", path.display()))
}

fn copy_new_file(source: &Path, destination: &Path) -> Result<()> {
    let mut source = File::open(source)?;
    let mut output = create_output(destination)?;
    std::io::copy(&mut source, &mut output)?;
    output.sync_all()?;
    Ok(())
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = create_output(path)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

pub fn build(
    config: &Path,
    output: &Path,
    runtime: Option<PathBuf>,
    bridge: Option<PathBuf>,
    nsis: Option<PathBuf>,
) -> Result<()> {
    let (source, mut manifest) = load(config)?;
    let crate::components::Resolved {
        runtime,
        bridge,
        nsis,
    } = crate::components::resolve(runtime, bridge, nsis)?;
    manifest.runtime_sha256 = Some(hash_file(&runtime)?);
    manifest.validate()?;
    let output = std::path::absolute(output)?;
    let parent = output.parent().context("output parent")?;
    fs::create_dir_all(parent)?;
    reject_reparse(parent)?;
    ensure!(
        !output.exists(),
        "output already exists; choose a new output path"
    );
    let stage = parent.join(format!("avelin-build-{}", uuid::Uuid::new_v4().simple()));
    fs::create_dir(&stage)?;
    fs::create_dir(stage.join("payload"))?;
    fs::copy(&bridge, stage.join("avelin_nsis_bridge.dll"))?;
    let staged_runtime = stage.join("runtime.exe");
    copy_new_file(&runtime, &staged_runtime)?;
    ensure!(
        manifest.runtime_sha256.as_deref() == Some(hash_file(&staged_runtime)?.as_str()),
        "runtime changed during staging"
    );
    fs::write(
        stage.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let mut ini = format!(
        "[app]\r\nid={}\r\nname={}\r\nversion={}\r\nentry={}\r\nruntime_sha256={}\r\nmanifest_sha256={}\r\n[files]\r\n",
        manifest.app_id,
        manifest.app_name,
        manifest.app_version,
        manifest.entry,
        manifest.runtime_sha256.as_deref().unwrap(),
        hash_file(&stage.join("manifest.json"))?
    );
    for (i, item) in manifest.files.iter().enumerate() {
        fs::copy(
            source.join(&item.path),
            stage.join(format!("payload/{i:06}")),
        )?;
        ensure!(
            hash_file(&stage.join(format!("payload/{i:06}")))? == item.sha256,
            "payload changed during staging"
        );
        ini.push_str(&format!("path{i}={}\r\n", item.path));
        ini.push_str(&format!("hash{i}={}\r\n", item.sha256));
        let parent = Path::new(&item.path).parent().unwrap_or(Path::new(""));
        ini.push_str(&format!("dir{i}={}\r\n", parent.display()));
    }
    ini.push_str("[directories]\r\n");
    for (i, directory) in directories(&manifest).iter().enumerate() {
        ini.push_str(&format!("dir{i}={directory}\r\n"));
    }
    write_ini(&stage.join("package.ini"), &ini)?;
    let script = generate(&manifest);
    fs::write(stage.join("installer.nsi"), script)?;
    let compilation_started = std::time::Instant::now();
    let status = Command::new(&nsis)
        .current_dir(&stage)
        .args(["/NOCONFIG", "/WX", "/V2", "installer.nsi"])
        .status().with_context(|| format!("start NSIS compiler {}; keep the complete NSIS tools directory together or pass --nsis <makensis.exe>", nsis.display()))?;
    let makensis_wall_ms = compilation_started.elapsed().as_millis();
    ensure!(
        status.success(),
        "NSIS build failed; inspect {}. Use a complete NSIS 3 Unicode distribution (including Bin, Include, Stubs and Plugins), or pass --nsis <makensis.exe>",
        stage.display()
    );
    let backend = output.with_extension("backend.exe");
    let metadata = output.with_extension("manifest.json");
    ensure!(
        !backend.exists() && !metadata.exists(),
        "sidecar output already exists"
    );
    ensure!(
        manifest.runtime_sha256.as_deref() == Some(hash_file(&staged_runtime)?.as_str()),
        "staged runtime changed during compilation"
    );
    // The earlier existence checks are useful diagnostics, not exclusive
    // reservations. Final writes must also refuse an output created meanwhile.
    copy_new_file(&stage.join("backend.exe"), &backend)?;
    write_new_file(&metadata, &serde_json::to_vec_pretty(&manifest)?)?;
    avelin_core::assemble(&staged_runtime, &backend, &manifest, &output)?;
    println!(
        "{}",
        serde_json::json!({"setup":output,"backend":backend,"manifest":metadata,"stage":stage,"signed":false,"files":manifest.files.len(),"makensis_wall_ms":makensis_wall_ms,"runtime_path":runtime,"bridge_path":bridge,"nsis_path":nsis,"tool_version":env!("CARGO_PKG_VERSION")})
    );
    Ok(())
}

fn generate(m: &Manifest) -> String {
    let mut s = include_str!("../../../packaging/nsis/m0-header.nsh").to_owned();
    s.push_str(&upgrade::generate(m));
    // ReadINIStr sets a sticky error for the absent optional upgrade key.
    // Only the following filesystem operation may determine its IfErrors branch.
    s.push_str("\nSection \"Install\"\nCall Initialize\nReadINIStr $0 \"$SessionIni\" \"task\" \"upgrade\"\nStrCmp $0 \"1\" 0 +2\nCall Upgrade\nStrCpy $Written 0\nStrCpy $OwnUninstaller \"\"\nStrCpy $OwnReceipt \"\"\nStrCpy $OwnRegistry 0\nStrCpy $OwnRuntime \"\"\nStrCpy $OwnManifest \"\"\nStrCpy $OwnDesktop \"\"\nStrCpy $OwnStartMenu \"\"\nStrCpy $CleanupIncomplete 0\nStrCpy $OwnInternalDirectory 0\nClearErrors\nCreateDirectory \"$INSTDIR\"\nIfErrors failed\nSetOutPath \"$PLUGINSDIR\"\nIfErrors failed\n");
    s.push_str(&emit("preparing", 0, m.files.len()));
    let mut created_parents = std::collections::BTreeSet::new();
    for (i, item) in m.files.iter().enumerate() {
        s.push_str(&format!(
            r#"
Call Checkpoint
StrCmp $StopReason "cancel" cancelled
StrCmp $StopReason "disconnected" disconnected
ReadINIStr $RelPath "$PLUGINSDIR\package.ini" "files" "path{i}"
ReadINIStr $ExpectedHash "$PLUGINSDIR\package.ini" "files" "hash{i}"
IfFileExists "$INSTDIR\$RelPath" failed 0
ClearErrors
"#
        ));
        let parent = Path::new(&item.path).parent().unwrap_or(Path::new(""));
        // The root is already created above. Create each payload parent on its
        // first use, keeping paths in the INI rather than interpolating metadata
        // into NSIS source. Publication pins the parent and refuses existing
        // final files, even if they appear after this early diagnostic check.
        if !parent.as_os_str().is_empty() && created_parents.insert(parent) {
            s.push_str(&format!(
                r#"ReadINIStr $ParentPath "$PLUGINSDIR\package.ini" "files" "dir{i}"
CreateDirectory "$INSTDIR\$ParentPath"
IfErrors failed
"#
            ));
        }
        s.push_str(&format!(
            r#"
avelin_nsis_bridge::Emit "extracting" "{i}" "{total}"
Pop $0
File "/oname=payload-{i:06}" "payload\{i:06}"
IfErrors failed
avelin_nsis_bridge::PublishOwnedFile "$PLUGINSDIR\payload-{i:06}" "$INSTDIR\$RelPath" "$ExpectedHash"
Pop $0
StrCmp $0 $ExpectedHash 0 failed
IntOp $Written $Written + 1
Delete "$PLUGINSDIR\payload-{i:06}"
StrCmp $Written $FaultAfter failed
"#,
            total = m.files.len()
        ));
        s.push_str(&emit("running", i + 1, m.files.len()));
        s.push_str(&format!("StrCmp $Written $CancelAfter 0 no_barrier{i}\navelin_nsis_bridge::WaitForCancel \"5000\"\nPop $0\nStrCmp $0 \"cancel\" cancelled\nStrCmp $0 \"disconnected\" disconnected\nGoto failed\nno_barrier{i}:\n"));
        s.push_str(&format!(
            "StrCmp $StepDelay \"0\" no_delay{i}\nSleep $StepDelay\nno_delay{i}:\n"
        ));
    }
    s.push_str("Call Checkpoint\nStrCmp $StopReason \"cancel\" cancelled\nStrCmp $StopReason \"disconnected\" disconnected\n");
    s.push_str(&emit("finalizing", m.files.len(), m.files.len()));
    s.push_str(r#"
ClearErrors
IfFileExists "$INSTDIR\uninstall.exe" failed 0
WriteUninstaller "$PLUGINSDIR\uninstall.exe"
IfErrors failed
avelin_nsis_bridge::PublishOwnedFile "$PLUGINSDIR\uninstall.exe" "$INSTDIR\uninstall.exe" ""
Pop $0
StrCpy $1 $0 6
StrCmp $1 "error:" failed 0
StrLen $1 $0
StrCmp $1 64 0 failed
StrCpy $OwnUninstaller $0
IfFileExists "$INSTDIR\.avelin" failed 0
avelin_nsis_bridge::CreateOwnedDirectory "$INSTDIR\.avelin"
Pop $0
StrCmp $0 "created" 0 failed
StrCpy $OwnInternalDirectory 1
File "/oname=runtime.exe" "runtime.exe"
IfErrors failed
ReadINIStr $ExpectedHash "$PLUGINSDIR\package.ini" "app" "runtime_sha256"
avelin_nsis_bridge::PublishOwnedFile "$PLUGINSDIR\runtime.exe" "$INSTDIR\.avelin\avelin-app.exe" "$ExpectedHash"
Pop $0
StrCmp $0 $ExpectedHash 0 failed
StrCpy $OwnRuntime $0
File "manifest.json"
IfErrors failed
ReadINIStr $ExpectedHash "$PLUGINSDIR\package.ini" "app" "manifest_sha256"
avelin_nsis_bridge::PublishOwnedFile "$PLUGINSDIR\manifest.json" "$INSTDIR\.avelin\manifest.json" "$ExpectedHash"
Pop $0
StrCmp $0 $ExpectedHash 0 failed
StrCpy $OwnManifest $0
StrCmp $DesktopShortcut "" desktop_created 0
SetOutPath "$ShortcutWorkingDir"
ClearErrors
CreateShortCut "$PLUGINSDIR\desktop.lnk" "$ShortcutTarget"
IfErrors failed
SetOutPath "$PLUGINSDIR"
avelin_nsis_bridge::PublishOwnedFile "$PLUGINSDIR\desktop.lnk" "$DesktopShortcut" ""
Pop $0
StrCpy $1 $0 6
StrCmp $1 "error:" failed 0
StrLen $1 $0
StrCmp $1 64 0 failed
StrCpy $OwnDesktop $0
WriteINIStr "$ShortcutEvidence" "shortcuts" "desktop_sha256" "$OwnDesktop"
IfErrors failed
desktop_created:
StrCmp $StartMenuShortcut "" start_menu_created 0
SetOutPath "$ShortcutWorkingDir"
ClearErrors
CreateShortCut "$PLUGINSDIR\start-menu.lnk" "$ShortcutTarget"
IfErrors failed
SetOutPath "$PLUGINSDIR"
avelin_nsis_bridge::PublishOwnedFile "$PLUGINSDIR\start-menu.lnk" "$StartMenuShortcut" ""
Pop $0
StrCpy $1 $0 6
StrCmp $1 "error:" failed 0
StrLen $1 $0
StrCmp $1 64 0 failed
StrCpy $OwnStartMenu $0
WriteINIStr "$ShortcutEvidence" "shortcuts" "start_menu_sha256" "$OwnStartMenu"
IfErrors failed
start_menu_created:
ClearErrors
StrCpy $OwnRegistry 1
ReadINIStr $0 "$PLUGINSDIR\package.ini" "app" "name"
WriteRegStr HKCU "$UninstallKey" "DisplayName" "$0"
ReadINIStr $0 "$PLUGINSDIR\package.ini" "app" "version"
WriteRegStr HKCU "$UninstallKey" "DisplayVersion" "$0"
WriteRegStr HKCU "$UninstallKey" "InstallLocation" "$INSTDIR"
WriteRegStr HKCU "$UninstallKey" "UninstallString" '"$INSTDIR\.avelin\avelin-app.exe" --gui --operation uninstall --backend "$INSTDIR\uninstall.exe" --manifest "$INSTDIR\.avelin\manifest.json" --install-dir "$INSTDIR"'
WriteRegStr HKCU "$UninstallKey" "QuietUninstallString" '"$INSTDIR\.avelin\avelin-app.exe" --silent --operation uninstall --backend "$INSTDIR\uninstall.exe" --manifest "$INSTDIR\.avelin\manifest.json" --install-dir "$INSTDIR"'
WriteRegDWORD HKCU "$UninstallKey" "NoModify" 1
WriteRegDWORD HKCU "$UninstallKey" "NoRepair" 1
IfErrors failed
ReadINIStr $0 "$SessionIni" "task" "receipt_candidate"
IfFileExists "$INSTDIR\.avelin-receipt.json" failed 0
avelin_nsis_bridge::PublishOwnedFile "$0" "$INSTDIR\.avelin-receipt.json" ""
Pop $0
StrCpy $1 $0 6
StrCmp $1 "error:" failed 0
StrLen $1 $0
StrCmp $1 64 0 failed
StrCpy $OwnReceipt $0
avelin_nsis_bridge::Finish "Succeeded" "0"
Pop $0
SetErrorLevel 0
Quit
cancelled:
avelin_nsis_bridge::Emit "cancelling" "$Written" "__TOTAL_FILES__"
Pop $0
StrCpy $Result "Cancelled"
StrCpy $ExitCode 2
Goto cleanup
disconnected:
StrCpy $Result "Unknown"
StrCpy $ExitCode 4
Goto cleanup
failed:
StrCpy $Result "Failed"
StrCpy $ExitCode 3
cleanup:
SetOutPath "$PLUGINSDIR"
ClearErrors
StrCmp $OwnRegistry 1 0 +2
DeleteRegKey HKCU "$UninstallKey"
"#);
    for (label, path, variable) in [
        ("receipt", r"$INSTDIR\.avelin-receipt.json", "OwnReceipt"),
        ("uninstaller", r"$INSTDIR\uninstall.exe", "OwnUninstaller"),
        ("runtime", r"$INSTDIR\.avelin\avelin-app.exe", "OwnRuntime"),
        ("manifest", r"$INSTDIR\.avelin\manifest.json", "OwnManifest"),
        ("desktop", "$DesktopShortcut", "OwnDesktop"),
        ("start_menu", "$StartMenuShortcut", "OwnStartMenu"),
    ] {
        s.push_str(&format!(
            "StrCmp ${variable} \"\" rollback_{label}_done 0\n"
        ));
        s.push_str(&delete_owned(
            path,
            &format!("${variable}"),
            &format!("rollback_{label}"),
            true,
        ));
    }
    s.push_str("StrCmp $OwnInternalDirectory 1 0 +2\nRMDir \"$INSTDIR\\.avelin\"\n");
    for i in (0..m.files.len()).rev() {
        s.push_str(&format!("IntCmp $Written {i} rollback_file{i}_done rollback_file{i}_done 0\nReadINIStr $RelPath \"$PLUGINSDIR\\package.ini\" \"files\" \"path{i}\"\nReadINIStr $ExpectedHash \"$PLUGINSDIR\\package.ini\" \"files\" \"hash{i}\"\n"));
        s.push_str(&delete_owned(
            r"$INSTDIR\$RelPath",
            "$ExpectedHash",
            &format!("rollback_file{i}"),
            true,
        ));
    }
    s.push_str(&remove_dirs(m, false));
    s.push_str(
        r#"
RMDir "$INSTDIR"
StrCmp $CleanupIncomplete 0 cleanup_complete 0
StrCpy $Result "Failed"
StrCpy $ExitCode 3
cleanup_complete:
avelin_nsis_bridge::Emit "cleanup_completed" "$Written" "__TOTAL_FILES__"
Pop $0
avelin_nsis_bridge::Finish "$Result" "$ExitCode"
Pop $0
SetErrorLevel $ExitCode
Quit
SectionEnd

Section "Uninstall"
Call un.Initialize
"#,
    );
    s.push_str(&emit("running", 0, m.files.len()));
    for i in (0..m.files.len()).rev() {
        s.push_str(&format!("ReadINIStr $RelPath \"$PLUGINSDIR\\package.ini\" \"files\" \"path{i}\"\nReadINIStr $ExpectedHash \"$PLUGINSDIR\\package.ini\" \"files\" \"hash{i}\"\n"));
        s.push_str(&delete_owned(
            r"$INSTDIR\$RelPath",
            "$ExpectedHash",
            &format!("un_file{i}"),
            false,
        ));
        s.push_str(&emit("running", m.files.len() - i, m.files.len()));
    }
    s.push_str(&emit("finalizing", m.files.len(), m.files.len()));
    for (label, path, key) in [
        ("desktop", "$DesktopShortcut", "desktop_shortcut_sha256"),
        (
            "start_menu",
            "$StartMenuShortcut",
            "start_menu_shortcut_sha256",
        ),
    ] {
        s.push_str(&format!("StrCmp {path} \"\" un_{label}_done 0\nReadINIStr $ExpectedHash \"$SessionIni\" \"task\" \"{key}\"\n"));
        s.push_str(&delete_owned(
            path,
            "$ExpectedHash",
            &format!("un_{label}"),
            false,
        ));
    }
    for (label, path, key) in [
        (
            "receipt",
            r"$INSTDIR\.avelin-receipt.json",
            "receipt_sha256",
        ),
        (
            "uninstaller",
            r"$INSTDIR\uninstall.exe",
            "uninstaller_sha256",
        ),
        (
            "runtime",
            r"$INSTDIR\.avelin\avelin-app.exe",
            "runtime_sha256",
        ),
        (
            "manifest",
            r"$INSTDIR\.avelin\manifest.json",
            "manifest_sha256",
        ),
    ] {
        s.push_str(&format!(
            "ReadINIStr $ExpectedHash \"$SessionIni\" \"task\" \"{key}\"\n"
        ));
        s.push_str(&delete_owned(
            path,
            "$ExpectedHash",
            &format!("un_{label}"),
            false,
        ));
    }
    s.push_str("ClearErrors\nDeleteRegKey HKCU \"$UninstallKey\"\nIfErrors un_failed\nRMDir \"$INSTDIR\\.avelin\"\n");
    s.push_str(&remove_dirs(m, true));
    s.push_str(
        r#"
RMDir "$INSTDIR"
avelin_nsis_bridge::Finish "Succeeded" "0"
Pop $0
SetErrorLevel 0
Quit
un_failed:
avelin_nsis_bridge::Finish "Failed" "3"
Pop $0
SetErrorLevel 3
Quit
SectionEnd
"#,
    );
    s.replace("__TOTAL_FILES__", &m.files.len().to_string())
}
fn delete_owned(path: &str, hash: &str, label: &str, rollback: bool) -> String {
    // A changed file is retained during uninstall. Rollback reports incomplete
    // cleanup rather than claiming it restored a target with modified bytes.
    let preserved = if rollback {
        "".to_owned()
    } else {
        format!("StrCmp $0 \"preserved\" {label}_done 0\n")
    };
    let error = if rollback {
        "StrCpy $CleanupIncomplete 1"
    } else {
        "Goto un_failed"
    };
    format!(
        "avelin_nsis_bridge::DeleteOwnedFile \"{path}\" \"{hash}\"\nPop $0\nStrCmp $0 \"deleted\" {label}_done 0\nStrCmp $0 \"missing\" {label}_done 0\n{preserved}{error}\n{label}_done:\n"
    )
}
fn emit(phase: &str, n: usize, total: usize) -> String {
    format!("avelin_nsis_bridge::Emit \"{phase}\" \"{n}\" \"{total}\"\nPop $0\n")
}
fn directories(m: &Manifest) -> Vec<String> {
    let mut dirs = std::collections::BTreeSet::new();
    for file in &m.files {
        let mut rest = file.path.as_str();
        while let Some((parent, _)) = rest.rsplit_once('\\') {
            dirs.insert(parent.to_owned());
            rest = parent;
        }
    }
    let mut dirs: Vec<_> = dirs.into_iter().collect();
    dirs.sort_by_key(|path| (std::cmp::Reverse(path.matches('\\').count()), path.clone()));
    dirs
}
fn remove_dirs(m: &Manifest, _uninstall: bool) -> String {
    // RMDir is deliberately non-recursive; unknown contents preserve the directory.
    directories(m).iter().enumerate().map(|(i,_)|format!("ReadINIStr $ParentPath \"$PLUGINSDIR\\package.ini\" \"directories\" \"dir{i}\"\nRMDir \"$INSTDIR\\$ParentPath\"\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OutputFixture(PathBuf);
    impl OutputFixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "avelin-build-output-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir(&directory).unwrap();
            Self(directory)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for OutputFixture {
        fn drop(&mut self) {
            // Only remove the exact files created by this isolated fixture.
            for name in ["source.bin", "output.bin", "copied.bin", "written.json"] {
                let _ = fs::remove_file(self.path(name));
            }
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn sidecar_writers_preserve_outputs_created_before_exclusive_open() {
        let fixture = OutputFixture::new();
        let source = fixture.path("source.bin");
        let output = fixture.path("output.bin");
        fs::write(&source, b"new build bytes").unwrap();
        fs::write(&output, b"existing output must survive").unwrap();
        assert!(copy_new_file(&source, &output).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"existing output must survive");
        assert!(write_new_file(&output, b"replacement metadata").is_err());
        assert_eq!(fs::read(&output).unwrap(), b"existing output must survive");
    }

    #[test]
    fn sidecar_writers_create_new_outputs_with_exact_contents() {
        let fixture = OutputFixture::new();
        let source = fixture.path("source.bin");
        fs::write(&source, b"backend bytes").unwrap();
        copy_new_file(&source, &fixture.path("copied.bin")).unwrap();
        write_new_file(&fixture.path("written.json"), br#"{"schema_version":1}"#).unwrap();
        assert_eq!(
            fs::read(fixture.path("copied.bin")).unwrap(),
            b"backend bytes"
        );
        assert_eq!(
            fs::read(fixture.path("written.json")).unwrap(),
            br#"{"schema_version":1}"#
        );
    }

    fn template_manifest() -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION,
            app_id: "dev.avelin.template-fixture".into(),
            app_name: "Literal ${APP_NAME}".into(),
            app_version: "0.1.0".into(),
            entry: "literal-entry.exe".into(),
            directory_name: "LiteralDirectory".into(),
            desktop_shortcut: false,
            start_menu_shortcut: false,
            files: ["literal-entry.exe", r"中文\${FILE_NAME}.txt", "another.txt"]
                .into_iter()
                .map(|path| FileResource {
                    path: path.into(),
                    size: 1,
                    sha256: "a".repeat(64),
                })
                .collect(),
            runtime_sha256: Some("b".repeat(64)),
            presentation: None,
        }
    }

    #[test]
    fn install_loop_caches_options_and_preserves_fault_controls() {
        let manifest = template_manifest();
        let script = generate(&manifest);
        let (initialization, install) = script.split_once("Section \"Install\"\n").unwrap();
        let (file_loop, _) = install
            .split_once("avelin_nsis_bridge::Emit \"finalizing\"")
            .unwrap();
        for (variable, key) in [
            ("FaultAfter", "fault_after_files"),
            ("CancelAfter", "cancel_after_files"),
            ("StepDelay", "step_delay_ms"),
        ] {
            let read = format!("ReadINIStr ${variable} \"$SessionIni\" \"task\" \"{key}\"");
            assert_eq!(initialization.matches(&read).count(), 1);
            assert!(!file_loop.contains(&format!("\"{key}\"")));
        }
        assert_eq!(file_loop.matches("SetOutPath \"$PLUGINSDIR\"").count(), 1);
        assert!(!file_loop.contains("SetOutPath \"$INSTDIR\""));
        assert_eq!(
            file_loop
                .matches("StrCmp $Written $FaultAfter failed")
                .count(),
            manifest.files.len()
        );
        assert_eq!(
            file_loop
                .matches("avelin_nsis_bridge::WaitForCancel \"5000\"")
                .count(),
            manifest.files.len()
        );
        for i in 0..manifest.files.len() {
            assert!(file_loop.contains(&format!("StrCmp $Written $CancelAfter 0 no_barrier{i}")));
            assert!(file_loop.contains(&format!(
                "StrCmp $StepDelay \"0\" no_delay{i}\nSleep $StepDelay\nno_delay{i}:"
            )));
        }
        assert!(!file_loop.contains("Sleep $0"));
    }

    #[test]
    fn source_metadata_remains_data_instead_of_nsis_source() {
        let manifest = template_manifest();
        manifest.validate().unwrap();
        let script = generate(&manifest);
        for literal in [&manifest.app_id, &manifest.app_name, &manifest.entry]
            .into_iter()
            .chain(manifest.files.iter().map(|file| &file.path))
        {
            assert!(
                !script.contains(literal),
                "metadata leaked into NSIS source"
            );
        }
        assert!(script.contains("File \"/oname=payload-000000\" \"payload\\000000\""));
        assert!(
            script.contains("ReadINIStr $RelPath \"$PLUGINSDIR\\package.ini\" \"files\" \"path1\"")
        );
    }

    #[test]
    fn real_shortcuts_and_uninstall_modes_use_controlled_task_data() {
        let mut manifest = template_manifest();
        manifest.desktop_shortcut = true;
        manifest.start_menu_shortcut = true;
        let script = generate(&manifest);
        for key in [
            "desktop_shortcut_path",
            "start_menu_shortcut_path",
            "shortcut_target",
            "shortcut_working_dir",
            "shortcut_evidence",
        ] {
            assert!(script.contains(&format!("\"task\" \"{key}\"")));
        }
        assert!(script.contains("CreateShortCut \"$PLUGINSDIR\\desktop.lnk\" \"$ShortcutTarget\""));
        assert!(
            script.contains("CreateShortCut \"$PLUGINSDIR\\start-menu.lnk\" \"$ShortcutTarget\"")
        );
        assert!(script.contains("--gui --operation uninstall"));
        assert!(script.contains("--silent --operation uninstall"));
        // Only the private extraction directory uses plain Delete. Every
        // installed file and external link goes through same-handle checking.
        for instruction in script.lines().map(str::trim_start).filter(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case("Delete"))
        }) {
            for target in ["$INSTDIR", "$DesktopShortcut", "$StartMenuShortcut"] {
                assert!(
                    !instruction.contains(target),
                    "unconditional installed-resource delete: {instruction}"
                );
            }
        }
        assert!(script.contains("SetOverwrite off"));
        assert!(!script.contains("RMDir /r"));
        let (_, uninstall) = script.split_once("Section \"Uninstall\"").unwrap();
        assert!(uninstall.contains("StrCmp $0 \"preserved\" un_file0_done 0"));
        assert!(uninstall.contains("\"task\" \"desktop_shortcut_sha256\""));
        assert!(uninstall.contains("\"task\" \"start_menu_shortcut_sha256\""));
    }

    #[test]
    fn upgrade_stages_and_backs_up_before_deleting_then_commits_receipt_last() {
        let script = generate(&template_manifest());
        let upgrade = script
            .split_once("Function Upgrade\n")
            .unwrap()
            .1
            .split_once("FunctionEnd")
            .unwrap()
            .0;
        let staged = upgrade.find("\"staged\"").unwrap();
        let backup = upgrade.find("upgrade_backup_verified:").unwrap();
        let marker = upgrade.find("\"marker_created\"").unwrap();
        let delete = upgrade.find("\"delete_intent\"").unwrap();
        let receipt = upgrade
            .find("\"$UpgradeStage\\receipt.json\" \"$INSTDIR\\.avelin-receipt.json\"")
            .unwrap();
        let commit = upgrade.find("\"committed\"").unwrap();
        assert!(
            staged < backup
                && backup < marker
                && marker < delete
                && delete < receipt
                && receipt < commit
        );
        assert!(!upgrade.contains("DeleteRegKey"));
        assert!(!upgrade.contains("RMDir /r"));
        assert!(upgrade.contains("StrCpy $UpgradeIndex $UpgradeDeleted"));
        assert!(upgrade.contains("\"rollback_incomplete\""));
        assert!(upgrade.contains("Finish \"Unknown\" \"4\""));
    }

    #[test]
    fn shared_parent_is_created_only_on_first_use_without_weakening_file_guards() {
        let mut manifest = template_manifest();
        manifest.files = [
            "literal-entry.exe",
            r"shared\first.txt",
            r"shared\second.txt",
            r"shared\nested\third.txt",
            r"shared\fourth.txt",
        ]
        .into_iter()
        .map(|path| FileResource {
            path: path.into(),
            size: 1,
            sha256: "a".repeat(64),
        })
        .collect();
        manifest.validate().unwrap();
        let script = generate(&manifest);
        let (_, install) = script.split_once("Section \"Install\"\n").unwrap();
        let (file_loop, _) = install
            .split_once("avelin_nsis_bridge::Emit \"finalizing\"")
            .unwrap();
        for i in [1, 3] {
            let read =
                format!("ReadINIStr $ParentPath \"$PLUGINSDIR\\package.ini\" \"files\" \"dir{i}\"");
            let file = format!("File \"/oname=payload-{i:06}\" \"payload\\{i:06}\"");
            assert!(file_loop.find(&read).unwrap() < file_loop.find(&file).unwrap());
        }
        for i in [0, 2, 4] {
            assert!(!file_loop.contains(&format!("\"files\" \"dir{i}\"")));
        }
        assert_eq!(
            file_loop
                .matches("CreateDirectory \"$INSTDIR\\$ParentPath\"")
                .count(),
            2
        );
        assert_eq!(
            file_loop
                .matches("IfFileExists \"$INSTDIR\\$RelPath\" failed 0")
                .count(),
            manifest.files.len()
        );
        assert_eq!(
            file_loop.matches("Call Checkpoint").count(),
            manifest.files.len() + 1
        );
        for i in 0..manifest.files.len() {
            assert!(file_loop.contains(&format!(
                "File \"/oname=payload-{i:06}\" \"payload\\{i:06}\"\nIfErrors failed"
            )));
        }
        assert!(!script.contains("RMDir /r"));
    }
}
