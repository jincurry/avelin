//! Upgrade preflight and evidence. NSIS owns deployment, backup and restoration.
use crate::{execution_files::PinnedFile, resources};
use anyhow::{Context, Result, ensure};
use avelin_core::{Manifest, RECEIPT_NAME, Receipt, ShortcutKind, ShortcutRecord, reject_reparse};
use serde::Serialize;
use serde_json::Value;
use std::{
    ffi::c_void,
    path::{Path, PathBuf},
    ptr,
};

pub const MARKER: &str = ".avelin/upgrade-state.json";

#[derive(Debug, Clone, Serialize)]
pub struct Registration {
    pub display_name: String,
    pub display_version: String,
    pub install_location: String,
    pub uninstall_string: String,
    pub quiet_uninstall_string: String,
    pub no_modify: u32,
    pub no_repair: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct OwnedResource {
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

pub struct PreviousInstallation {
    pub manifest: Manifest,
    pub manifest_file_sha256: String,
    pub receipt: Receipt,
    pub receipt_sha256: String,
    pub registration: Registration,
    pub backup: Vec<OwnedResource>,
    pub preserved: Vec<OwnedResource>,
    pub shortcuts: Vec<ShortcutRecord>,
    pub previously_absent: Vec<PathBuf>,
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(
        key: *mut c_void,
        name: *const u16,
        options: u32,
        access: u32,
        output: *mut *mut c_void,
    ) -> i32;
    fn RegQueryValueExW(
        key: *mut c_void,
        name: *const u16,
        reserved: *mut u32,
        kind: *mut u32,
        data: *mut u8,
        length: *mut u32,
    ) -> i32;
    fn RegCloseKey(key: *mut c_void) -> i32;
}

struct Key(*mut c_void);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn registry_value(key: &Key, name: &str) -> Result<(u32, Vec<u8>)> {
    let name = wide(name);
    let (mut kind, mut size) = (0, 0);
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null_mut(),
            &mut kind,
            ptr::null_mut(),
            &mut size,
        )
    };
    ensure!(
        status == 0 && size <= 65536,
        "cannot read installed product registration ({status})"
    );
    let mut bytes = vec![0u8; size as usize];
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null_mut(),
            &mut kind,
            bytes.as_mut_ptr(),
            &mut size,
        )
    };
    ensure!(
        status == 0 && size as usize <= bytes.len(),
        "registration changed during inspection ({status})"
    );
    bytes.truncate(size as usize);
    Ok((kind, bytes))
}

fn registry_string(key: &Key, name: &str) -> Result<String> {
    let (kind, bytes) = registry_value(key, name)?;
    ensure!(
        kind == 1 && bytes.len() >= 2 && bytes.len() % 2 == 0,
        "invalid string registration {name}"
    );
    let mut units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    ensure!(
        units.pop() == Some(0) && !units.contains(&0),
        "invalid terminated registration {name}"
    );
    let text = String::from_utf16(&units)?;
    ensure!(
        !text.chars().any(char::is_control),
        "control character in registration {name}"
    );
    Ok(text)
}

fn registry_dword(key: &Key, name: &str) -> Result<u32> {
    let (kind, bytes) = registry_value(key, name)?;
    ensure!(
        kind == 4 && bytes.len() == 4,
        "invalid DWORD registration {name}"
    );
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

pub fn registration(app_id: &str) -> Result<Option<Registration>> {
    avelin_core::safe_id(app_id)?;
    let name = wide(&format!(
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.{app_id}"
    ));
    let mut raw = ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            0x80000001u32 as i32 as isize as *mut c_void,
            name.as_ptr(),
            0,
            0x0001 | 0x0100,
            &mut raw,
        )
    };
    if status == 2 {
        return Ok(None);
    }
    ensure!(
        status == 0,
        "cannot establish installed product identity ({status})"
    );
    let key = Key(raw);
    Ok(Some(Registration {
        display_name: registry_string(&key, "DisplayName")?,
        display_version: registry_string(&key, "DisplayVersion")?,
        install_location: registry_string(&key, "InstallLocation")?,
        uninstall_string: registry_string(&key, "UninstallString")?,
        quiet_uninstall_string: registry_string(&key, "QuietUninstallString")?,
        no_modify: registry_dword(&key, "NoModify")?,
        no_repair: registry_dword(&key, "NoRepair")?,
    }))
}

pub fn ensure_no_pending(target: &Path) -> Result<()> {
    let marker = target.join(MARKER);
    reject_reparse(&marker)?;
    ensure!(
        !marker.exists(),
        "unfinished upgrade requires inspection; no new operation was started: {} (read the recorded session with avelin-cli diagnose)",
        marker.display()
    );
    Ok(())
}

pub fn previous_version(target: &Path) -> Result<String> {
    let file = PinnedFile::open(&target.join(".avelin/manifest.json"))?;
    let manifest: Manifest = serde_json::from_slice(&file.read_bytes()?)?;
    manifest.validate()?;
    Ok(manifest.app_version)
}

fn command(target: &Path, silent: bool) -> String {
    format!(
        "\"{}\" --{} --operation uninstall --backend \"{}\" --manifest \"{}\" --install-dir \"{}\"",
        target.join(".avelin").join("avelin-app.exe").display(),
        if silent { "silent" } else { "gui" },
        target.join("uninstall.exe").display(),
        target.join(".avelin").join("manifest.json").display(),
        target.display()
    )
}

pub fn verify_registration(value: &Registration, manifest: &Manifest, target: &Path) -> Result<()> {
    ensure!(
        resources::same_path(Path::new(&value.install_location), target)
            && value.display_name == manifest.app_name
            && value.display_version == manifest.app_version
            && value.uninstall_string == command(target, false)
            && value.quiet_uninstall_string == command(target, true)
            && value.no_modify == 1
            && value.no_repair == 1,
        "installation registration does not match the recorded product, version, directory or uninstall commands"
    );
    Ok(())
}

fn observe(path: &Path) -> Result<Option<OwnedResource>> {
    reject_reparse(path)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    let input = PinnedFile::open(path)?;
    Ok(Some(OwnedResource {
        path: path.into(),
        sha256: input.sha256()?,
        size: std::fs::metadata(path)?.len(),
    }))
}

fn same_relative(left: &str, right: &str) -> bool {
    avelin_core::same_windows_name(&left.replace('/', "\\"), &right.replace('/', "\\"))
}

/// Refuse new destinations whose parent is a file. File/directory shape changes
/// are deliberately rejected before any resource mutation.
fn check_destination(path: &Path, target: &Path) -> Result<()> {
    reject_reparse(path)?;
    ensure!(
        path.to_string_lossy().encode_utf16().count() < 240,
        "upgrade destination exceeds the supported path limit: {}",
        path.display()
    );
    for ancestor in path.ancestors().skip(1).take_while(|p| *p != target) {
        ensure!(
            !ancestor.exists() || ancestor.is_dir(),
            "upgrade parent is an existing file: {}",
            ancestor.display()
        );
    }
    Ok(())
}

pub fn prepare(
    manifest: &Manifest,
    target: &Path,
    sid: &str,
    desktop: bool,
    start_menu: bool,
) -> Result<PreviousInstallation> {
    manifest.validate()?;
    avelin_core::install_root(target)?;
    ensure_no_pending(target)?;
    let input = PinnedFile::open(&target.join(".avelin/manifest.json"))
        .context("upgrade requires the existing installed manifest")?;
    let old: Manifest = serde_json::from_slice(&input.read_bytes()?)?;
    old.validate()?;
    let manifest_file_sha256 = input.sha256()?;
    let (receipt, receipt_sha256) = resources::read_receipt_with_hash(&old, target, sid)?;
    ensure!(
        manifest.app_id == old.app_id,
        "upgrade product identity conflicts with the existing installation"
    );
    // Semantic precedence deliberately ignores build metadata.
    avelin_core::model::ensure_newer_version(&old.app_version, &manifest.app_version)?;
    let registration =
        registration(&old.app_id)?.context("upgrade requires the existing product registration")?;
    verify_registration(&registration, &old, target)?;
    let mut backup = Vec::new();
    let mut preserved = Vec::new();
    let mut previously_absent = Vec::new();
    for file in &old.files {
        let path = target.join(&file.path);
        if let Some(current) = observe(&path)? {
            if current.sha256 == file.sha256 {
                backup.push(current);
            } else {
                ensure!(
                    !manifest
                        .files
                        .iter()
                        .any(|new| same_relative(&new.path, &file.path)),
                    "upgrade would overwrite a modified installed file: {}",
                    path.display()
                );
                preserved.push(current);
            }
        } else {
            previously_absent.push(path);
        }
    }
    for file in &manifest.files {
        let path = target.join(&file.path);
        check_destination(&path, target)?;
        if path.try_exists()? {
            ensure!(
                backup
                    .iter()
                    .any(|item| resources::same_path(&item.path, &path)),
                "upgrade destination conflicts with an unknown or modified resource: {}",
                path.display()
            );
        } else {
            previously_absent.push(path);
        }
    }
    for (relative, expected) in [
        (RECEIPT_NAME, receipt_sha256.as_str()),
        (".avelin/manifest.json", manifest_file_sha256.as_str()),
        (
            ".avelin/avelin-app.exe",
            old.runtime_sha256
                .as_deref()
                .context("old runtime hash missing")?,
        ),
        (
            "uninstall.exe",
            receipt
                .uninstaller_sha256
                .as_deref()
                .context("old uninstaller hash missing; use its original runtime")?,
        ),
    ] {
        let path = target.join(relative);
        let item = observe(&path)?
            .with_context(|| format!("missing installed component: {}", path.display()))?;
        ensure!(
            item.sha256.eq_ignore_ascii_case(expected),
            "installed component changed: {}",
            path.display()
        );
        backup.push(item);
    }
    let mut shortcuts = Vec::new();
    for (kind, selected) in [
        (ShortcutKind::Desktop, desktop),
        (ShortcutKind::StartMenu, start_menu),
    ] {
        let path = resources::shortcut_path(&manifest.app_id, kind)?;
        let old_record = receipt.shortcuts.iter().find(|item| item.kind == kind);
        if let Some(current) = observe(&path)? {
            match old_record {
                Some(record) if current.sha256.eq_ignore_ascii_case(&record.sha256) => {
                    backup.push(current)
                }
                _ => {
                    ensure!(
                        !selected,
                        "upgrade shortcut destination is unknown or modified: {}",
                        path.display()
                    );
                    if old_record.is_some() {
                        preserved.push(current);
                    }
                }
            }
        } else if selected {
            previously_absent.push(path.clone());
        }
        if selected {
            shortcuts.push(ShortcutRecord {
                kind,
                path: path.to_string_lossy().into_owned(),
                target: target.join(&manifest.entry).to_string_lossy().into_owned(),
                sha256: String::new(),
            });
        }
    }
    Ok(PreviousInstallation {
        manifest: old,
        manifest_file_sha256,
        receipt,
        receipt_sha256,
        registration,
        backup,
        preserved,
        shortcuts,
        previously_absent,
    })
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetDiskFreeSpaceExW(
        path: *const u16,
        available: *mut u64,
        total: *mut u64,
        free: *mut u64,
    ) -> i32;
}

pub fn check_space(
    target: &Path,
    session: &Path,
    target_bytes: u64,
    temporary_bytes: u64,
) -> Result<()> {
    fn available(path: &Path) -> Result<u64> {
        let (mut free, mut total, mut total_free) = (0, 0, 0);
        ensure!(
            unsafe {
                GetDiskFreeSpaceExW(
                    wide(&path.to_string_lossy()).as_ptr(),
                    &mut free,
                    &mut total,
                    &mut total_free,
                )
            } != 0,
            "cannot determine space available for upgrade: {}",
            std::io::Error::last_os_error()
        );
        Ok(free)
    }
    let sum = target_bytes
        .checked_add(temporary_bytes)
        .context("upgrade space requirement overflow")?;
    let same_drive =
        target.to_string_lossy()[..2].eq_ignore_ascii_case(&session.to_string_lossy()[..2]);
    ensure!(
        available(target)? >= if same_drive { sum } else { target_bytes },
        "insufficient available space on the upgrade target volume"
    );
    ensure!(
        available(session)? >= if same_drive { sum } else { temporary_bytes },
        "insufficient available space on the upgrade temporary volume"
    );
    Ok(())
}

pub fn verify_preserved(previous: &PreviousInstallation) -> Result<()> {
    for item in &previous.preserved {
        let current =
            observe(&item.path)?.context("preserved user resource is missing after upgrade")?;
        ensure!(
            current.sha256 == item.sha256 && current.size == item.size,
            "preserved resource changed: {}",
            item.path.display()
        );
    }
    Ok(())
}

pub fn verify_removed(
    previous: &PreviousInstallation,
    manifest: &Manifest,
    target: &Path,
) -> Result<()> {
    for old in &previous.manifest.files {
        if manifest
            .files
            .iter()
            .any(|new| same_relative(&new.path, &old.path))
        {
            continue;
        }
        let path = target.join(&old.path);
        if previous
            .preserved
            .iter()
            .any(|item| resources::same_path(&item.path, &path))
        {
            continue;
        }
        ensure!(
            !path.try_exists()?,
            "obsolete owned file remains after upgrade: {}",
            path.display()
        );
    }
    for shortcut in &previous.receipt.shortcuts {
        let path = Path::new(&shortcut.path);
        if previous
            .shortcuts
            .iter()
            .any(|item| item.kind == shortcut.kind)
            || previous
                .preserved
                .iter()
                .any(|item| resources::same_path(&item.path, path))
        {
            continue;
        }
        ensure!(
            !path.try_exists()?,
            "obsolete owned shortcut remains after upgrade: {}",
            path.display()
        );
    }
    verify_preserved(previous)
}

pub fn verify_rollback(previous: &PreviousInstallation, target: &Path) -> Result<()> {
    for item in &previous.backup {
        resources::confirm_owned_hash(&item.path, &item.sha256)?;
    }
    for path in &previous.previously_absent {
        ensure!(
            !path.try_exists()?,
            "upgrade rollback left a newly created resource: {}",
            path.display()
        );
    }
    verify_preserved(previous)?;
    verify_registration(
        &registration(&previous.manifest.app_id)?
            .context("old registration missing after failed upgrade")?,
        &previous.manifest,
        target,
    )?;
    ensure_no_pending(target)
}

pub fn verify_journal(path: &Path, task_id: &str, terminal: &str) -> Result<()> {
    use std::io::{BufRead, BufReader};
    reject_reparse(path)?;
    ensure!(
        std::fs::metadata(path)?.len() <= 512 * 1024 * 1024,
        "upgrade journal exceeds its supported size"
    );
    let mut last_terminal = None;
    let mut started = false;
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        ensure!(
            line.ends_with('\n'),
            "upgrade journal has a truncated final record"
        );
        ensure!(
            line.len() <= 65536,
            "upgrade journal record exceeds its bound"
        );
        let record: Value = serde_json::from_str(&line)?;
        ensure!(
            last_terminal.is_none(),
            "upgrade journal contains records after its terminal event"
        );
        ensure!(
            record["schema_version"] == 1 && record["task_id"] == task_id,
            "upgrade journal task identity mismatch"
        );
        let event = record["event"]
            .as_str()
            .context("upgrade journal event missing")?;
        if !started {
            ensure!(event == "begin", "upgrade journal has no beginning");
            started = true;
        }
        if matches!(event, "committed" | "rolled_back" | "rollback_incomplete") {
            if event == "committed" {
                ensure!(
                    record["result"] == "verified",
                    "upgrade commit was not verified"
                );
            }
            ensure!(
                last_terminal.is_none(),
                "upgrade journal has conflicting terminal records"
            );
            last_terminal = Some(event.to_owned());
        }
    }
    ensure!(
        started && last_terminal.as_deref() == Some(terminal),
        "upgrade journal does not confirm {terminal}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("avelin-m3-preflight-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Only the exact names created by this fixture; never recursive cleanup.
            for name in ["journal.jsonl", "parent-file", MARKER] {
                let _ = std::fs::remove_file(self.0.join(name));
            }
            let _ = std::fs::remove_dir(self.0.join(".avelin"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn pending_upgrade_blocks_new_operations_without_touching_evidence() {
        let fixture = Fixture::new();
        ensure_no_pending(&fixture.0).unwrap();
        std::fs::create_dir(fixture.0.join(".avelin")).unwrap();
        let path = fixture.0.join(MARKER);
        std::fs::write(&path, b"preserve partial upgrade evidence").unwrap();
        assert!(
            ensure_no_pending(&fixture.0)
                .unwrap_err()
                .to_string()
                .contains("unfinished upgrade")
        );
        assert_eq!(
            std::fs::read(path).unwrap(),
            b"preserve partial upgrade evidence"
        );
    }

    #[test]
    fn journal_requires_one_task_and_unambiguous_terminal_evidence() {
        let fixture = Fixture::new();
        let path = fixture.0.join("journal.jsonl");
        let record = |task: &str, event: &str| {
            format!(
                "{}\n",
                serde_json::json!({"schema_version":1,"task_id":task,"event":event,"result":"verified"})
            )
        };
        let good = record("task", "begin") + &record("task", "committed");
        std::fs::write(&path, good.trim_end_matches('\n')).unwrap();
        assert!(verify_journal(&path, "task", "committed").is_err());
        std::fs::write(&path, &good).unwrap();
        verify_journal(&path, "task", "committed").unwrap();
        assert!(verify_journal(&path, "other", "committed").is_err());
        assert!(verify_journal(&path, "task", "rolled_back").is_err());
        std::fs::write(&path, good + &record("task", "rolled_back")).unwrap();
        assert!(verify_journal(&path, "task", "committed").is_err());
        std::fs::write(
            &path,
            record("task", "begin") + &record("task", "committed") + &record("task", "published"),
        )
        .unwrap();
        assert!(verify_journal(&path, "task", "committed").is_err());
        std::fs::write(&path, record("task", "committed")).unwrap();
        assert!(verify_journal(&path, "task", "committed").is_err());
    }

    #[test]
    fn file_to_directory_shape_change_is_refused_before_mutation() {
        let fixture = Fixture::new();
        let parent = fixture.0.join("parent-file");
        std::fs::write(&parent, b"old application bytes").unwrap();
        assert!(check_destination(&parent.join("new.txt"), &fixture.0).is_err());
        assert_eq!(std::fs::read(parent).unwrap(), b"old application bytes");
    }

    #[test]
    fn registration_commands_match_the_nsis_native_path_spelling() {
        let target = Path::new(r"C:\Apps\Nimbus 笔记");
        assert_eq!(
            command(target, false),
            r#""C:\Apps\Nimbus 笔记\.avelin\avelin-app.exe" --gui --operation uninstall --backend "C:\Apps\Nimbus 笔记\uninstall.exe" --manifest "C:\Apps\Nimbus 笔记\.avelin\manifest.json" --install-dir "C:\Apps\Nimbus 笔记""#
        );
        assert!(command(target, true).contains(" --silent --operation uninstall "));
    }
}
