//! Ownership decisions for the coordinator. The Rust worker mutates managed resources.
use crate::execution_files::PinnedFile;
use anyhow::{Context, Result, ensure};
use avelin_core::{
    Manifest, RECEIPT_NAME, Receipt, ShortcutKind, ShortcutRecord, hash_bytes, reject_reparse,
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

pub fn same_path(left: &Path, right: &Path) -> bool {
    avelin_core::same_windows_name(
        &left.to_string_lossy().replace('/', "\\"),
        &right.to_string_lossy().replace('/', "\\"),
    )
}

/// Validate the pinned worker before any process launch or installed-state mutation.
pub fn validate_backend(backend: &PinnedFile, manifest: &Manifest) -> Result<()> {
    let (payload, embedded_manifest) = avelin_core::embedded(backend.path())?.context(
        "only a packaged Rust worker is supported; rebuild with the current developer kit",
    )?;
    avelin_core::native_payload::NativePayload::parse(&payload, &embedded_manifest)?;
    ensure!(
        hash_bytes(&serde_json::to_vec(&embedded_manifest)?)
            == hash_bytes(&serde_json::to_vec(manifest)?),
        "Rust worker manifest differs from the requested task"
    );
    Ok(())
}

pub fn shortcut_path(
    scope: avelin_core::InstallScope,
    app_id: &str,
    kind: ShortcutKind,
) -> Result<PathBuf> {
    avelin_core::safe_id(app_id)?;
    Ok(avelin_core::scope::shortcut_folder(scope, kind)?.join(format!("{app_id}.lnk")))
}

pub fn planned_shortcuts(
    manifest: &Manifest,
    target: &Path,
    desktop: bool,
    start_menu: bool,
) -> Result<Vec<ShortcutRecord>> {
    let mut shortcuts = Vec::new();
    for (kind, selected) in [
        (ShortcutKind::Desktop, desktop),
        (ShortcutKind::StartMenu, start_menu),
    ] {
        if selected {
            let path = shortcut_path(manifest.install_scope, &manifest.app_id, kind)?;
            reject_reparse(&path)?;
            ensure!(
                !path.exists(),
                "shortcut destination already exists: {}",
                path.display()
            );
            shortcuts.push(ShortcutRecord {
                kind,
                path: path.to_string_lossy().into_owned(),
                target: target.join(&manifest.entry).to_string_lossy().into_owned(),
                sha256: String::new(),
            });
        }
    }
    Ok(shortcuts)
}

pub fn validate_receipt(
    receipt: &Receipt,
    manifest: &Manifest,
    target: &Path,
    user_sid: &str,
) -> Result<()> {
    receipt.validate_manifest(manifest)?;
    ensure!(
        receipt.app_id == manifest.app_id
            && receipt.app_version == manifest.app_version
            && receipt.install_scope == manifest.install_scope
            && receipt.manifest_sha256 == hash_bytes(&serde_json::to_vec(manifest)?)
            && same_path(Path::new(&receipt.install_dir), target),
        "receipt identity, ownership or file inventory mismatch"
    );
    avelin_core::scope::validate_owned_installation(
        manifest.install_scope,
        target,
        &receipt.user_sid,
        user_sid,
    )?;
    let mut seen = BTreeSet::new();
    for shortcut in &receipt.shortcuts {
        ensure!(
            seen.insert(shortcut.kind),
            "duplicate shortcut ownership record"
        );
        ensure!(
            same_path(
                Path::new(&shortcut.path),
                &shortcut_path(manifest.install_scope, &manifest.app_id, shortcut.kind)?
            ) && same_path(Path::new(&shortcut.target), &target.join(&manifest.entry))
                && shortcut.sha256.len() == 64
                && shortcut.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "shortcut ownership record is outside this product's scoped destinations"
        );
    }
    Ok(())
}

pub fn read_receipt(manifest: &Manifest, target: &Path, user_sid: &str) -> Result<Receipt> {
    Ok(read_receipt_with_hash(manifest, target, user_sid)?.0)
}

pub fn read_receipt_with_hash(
    manifest: &Manifest,
    target: &Path,
    user_sid: &str,
) -> Result<(Receipt, String)> {
    let input =
        PinnedFile::open(&target.join(RECEIPT_NAME)).context("no readable installation receipt")?;
    let receipt = serde_json::from_slice(&input.read_bytes()?)?;
    validate_receipt(&receipt, manifest, target, user_sid)?;
    Ok((receipt, input.sha256()?))
}

/// A fresh read may confirm ownership, but may never replace its expected hash.
pub fn confirm_owned_hash(path: &Path, expected: &str) -> Result<String> {
    let file = PinnedFile::open(path)?;
    ensure!(
        file.sha256()?.eq_ignore_ascii_case(expected),
        "owned resource changed after verification: {}",
        path.display()
    );
    Ok(expected.to_ascii_lowercase())
}

/// A nonempty directory is usable only when every owned destination is absent.
/// Existing unrelated files/directories are neither adopted nor deleted.
pub fn validate_install_destination(target: &Path, manifest: &Manifest) -> Result<()> {
    validate_selected_destination(target, &manifest.files)
}

pub fn validate_selected_destination(
    target: &Path,
    files: &[avelin_core::FileResource],
) -> Result<()> {
    avelin_core::install_root(target)?;
    ensure!(
        !target.exists() || target.is_dir(),
        "installation target is not a directory"
    );
    for relative in [RECEIPT_NAME, "uninstall.exe", ".avelin"]
        .into_iter()
        .chain(files.iter().map(|file| file.path.as_str()))
    {
        let path = target.join(relative);
        reject_reparse(&path)?;
        ensure!(
            !path.exists(),
            "installation destination conflicts with an existing resource: {}",
            path.display()
        );
        for ancestor in path.ancestors().skip(1).take_while(|path| *path != target) {
            ensure!(
                !ancestor.exists() || ancestor.is_dir(),
                "installation parent conflicts with an existing file: {}",
                ancestor.display()
            );
        }
    }
    Ok(())
}

/// Remaining changed files are user-owned. A matching managed file must be gone.
pub fn verify_removed_or_preserved(path: &Path, expected: &str) -> Result<()> {
    reject_reparse(path)?;
    if !path.try_exists()? {
        return Ok(());
    }
    let file = PinnedFile::open(path)?;
    ensure!(
        !file.sha256()?.eq_ignore_ascii_case(expected),
        "unchanged owned file remains: {}",
        path.display()
    );
    Ok(())
}

pub fn verified_shortcuts(
    planned: &[ShortcutRecord],
    evidence: &Path,
) -> Result<Vec<ShortcutRecord>> {
    let mut hashes = std::collections::BTreeMap::new();
    if evidence.exists() {
        let bytes = PinnedFile::open(evidence)?.read_bytes()?;
        ensure!(bytes.len() <= 8192, "shortcut evidence exceeds its limit");
        let text = if bytes.starts_with(&[0xff, 0xfe]) {
            ensure!(bytes.len() % 2 == 0, "invalid Unicode shortcut evidence");
            String::from_utf16(
                &bytes[2..]
                    .chunks_exact(2)
                    .map(|s| u16::from_le_bytes([s[0], s[1]]))
                    .collect::<Vec<_>>(),
            )?
        } else {
            String::from_utf8(bytes)?
        };
        let mut section = false;
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            if line == "[shortcuts]" {
                ensure!(!section, "duplicate shortcut evidence section");
                section = true;
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .context("invalid shortcut evidence line")?;
            ensure!(
                section
                    && matches!(key, "desktop_sha256" | "start_menu_sha256")
                    && value.len() == 64
                    && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && hashes.insert(key.to_owned(), value.to_owned()).is_none(),
                "invalid or repeated shortcut evidence"
            );
        }
    }
    ensure!(
        hashes.len() == planned.len(),
        "shortcut creation evidence does not match selected options"
    );
    planned
        .iter()
        .map(|shortcut| {
            let key = match shortcut.kind {
                ShortcutKind::Desktop => "desktop_sha256",
                ShortcutKind::StartMenu => "start_menu_sha256",
            };
            let expected = hashes
                .get(key)
                .context("selected shortcut has no creation evidence")?;
            let file = PinnedFile::open(Path::new(&shortcut.path))?;
            ensure!(
                file.sha256()?.eq_ignore_ascii_case(expected),
                "shortcut changed after creation"
            );
            Ok(ShortcutRecord {
                sha256: expected.to_ascii_lowercase(),
                ..shortcut.clone()
            })
        })
        .collect()
}

#[cfg(any(feature = "gui", test))]
pub fn verified_entry(
    manifest: &Manifest,
    target: &Path,
    user_sid: &str,
    task_id: &str,
) -> Result<PinnedFile> {
    manifest.validate()?;
    avelin_core::install_root(target)?;
    let receipt = read_receipt(manifest, target, user_sid)?;
    ensure!(
        receipt.task_id == task_id,
        "installed application belongs to a different completed task"
    );
    let entry = manifest
        .files
        .iter()
        .find(|file| file.path == manifest.entry)
        .context("manifest entry is missing")?;
    let file = PinnedFile::open(&target.join(&entry.path))?;
    ensure!(
        file.sha256()?.eq_ignore_ascii_case(&entry.sha256),
        "installed entry was modified"
    );
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "avelin-m2-resource-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn manifest(&self) -> Manifest {
            let mut manifest: Manifest =
                serde_json::from_str(include_str!("../../../docs/schemas/examples/manifest.json"))
                    .unwrap();
            manifest.files[0].size = 3;
            manifest.files[0].sha256 = hash_bytes(b"abc");
            manifest
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            assert!(
                self.0.starts_with(std::env::temp_dir())
                    && self
                        .0
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("avelin-m2-resource-")
            );
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn backend_validation_requires_a_complete_rust_package_bound_to_manifest() {
        let fixture = Fixture::new();
        let mut manifest = fixture.manifest();
        manifest.runtime_sha256 = Some(hash_bytes(b"runtime"));
        let worker = fixture.0.join("worker.exe");
        fs::write(&worker, b"inert executable without a native package").unwrap();
        let input = PinnedFile::open(&worker).unwrap();
        assert!(validate_backend(&input, &manifest).is_err());
        drop(input);

        let mut payload = avelin_core::native_payload::MAGIC.to_vec();
        payload.extend(1u32.to_le_bytes());
        payload.extend((manifest.files.len() as u32).to_le_bytes());
        payload.extend(7u64.to_le_bytes());
        payload.extend(b"runtime");
        for file in &mut manifest.files {
            file.size = 3;
            file.sha256 = hash_bytes(b"abc");
            payload.extend(3u64.to_le_bytes());
            payload.extend(b"abc");
        }
        let packed_payload = fixture.0.join("payload.bin");
        fs::write(&packed_payload, &payload).unwrap();
        let packaged = fixture.0.join("packaged.exe");
        avelin_core::assemble(&worker, &packed_payload, &manifest, &packaged).unwrap();
        let input = PinnedFile::open(&packaged).unwrap();
        validate_backend(&input, &manifest).unwrap();
        let mut mismatch = manifest.clone();
        mismatch.app_version = "99.0.0".into();
        assert!(validate_backend(&input, &mismatch).is_err());
        drop(input);

        payload[0] ^= 1;
        fs::write(&packed_payload, payload).unwrap();
        let invalid = fixture.0.join("invalid.exe");
        avelin_core::assemble(&worker, &packed_payload, &manifest, &invalid).unwrap();
        let input = PinnedFile::open(&invalid).unwrap();
        assert!(validate_backend(&input, &manifest).is_err());
    }

    #[test]
    fn unknown_user_files_survive_preflight_but_owned_conflicts_are_refused() {
        let fixture = Fixture::new();
        let manifest = fixture.manifest();
        fs::write(fixture.0.join("user-kept.txt"), b"user data").unwrap();
        validate_install_destination(&fixture.0, &manifest).unwrap();
        fs::write(fixture.0.join(&manifest.entry), b"modified existing entry").unwrap();
        assert!(validate_install_destination(&fixture.0, &manifest).is_err());
        fs::remove_file(fixture.0.join(&manifest.entry)).unwrap();
        fs::create_dir(fixture.0.join(".avelin")).unwrap();
        assert!(validate_install_destination(&fixture.0, &manifest).is_err());
        assert_eq!(
            fs::read(fixture.0.join("user-kept.txt")).unwrap(),
            b"user data"
        );
    }

    #[test]
    fn verification_accepts_changed_and_missing_files_but_rejects_unchanged_leftovers() {
        let fixture = Fixture::new();
        let path = fixture.0.join("payload.exe");
        let original = hash_bytes(b"original");
        verify_removed_or_preserved(&path, &original).unwrap();
        fs::write(&path, b"original").unwrap();
        assert!(verify_removed_or_preserved(&path, &original).is_err());
        fs::write(&path, b"user changed this file").unwrap();
        verify_removed_or_preserved(&path, &original).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"user changed this file");
    }

    #[test]
    fn fresh_internal_hash_check_never_adopts_replaced_bytes() {
        let fixture = Fixture::new();
        let path = fixture.0.join("runtime.exe");
        fs::write(&path, b"verified runtime").unwrap();
        let expected = hash_bytes(b"verified runtime");
        assert_eq!(confirm_owned_hash(&path, &expected).unwrap(), expected);
        fs::write(&path, b"replaced runtime").unwrap();
        assert!(confirm_owned_hash(&path, &expected).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"replaced runtime");
    }

    #[test]
    fn shortcut_ownership_requires_the_creation_hash_not_just_the_final_path() {
        let fixture = Fixture::new();
        let path = fixture.0.join("owned.lnk");
        let evidence = fixture.0.join("shortcuts.ini");
        fs::write(&path, b"created link").unwrap();
        let planned = [ShortcutRecord {
            kind: ShortcutKind::Desktop,
            path: path.to_string_lossy().into_owned(),
            target: fixture.0.join("Example.exe").to_string_lossy().into_owned(),
            sha256: String::new(),
        }];
        assert!(verified_shortcuts(&planned, &evidence).is_err());
        let text = format!(
            "[shortcuts]\r\ndesktop_sha256={}\r\n",
            hash_bytes(b"created link")
        );
        let bytes: Vec<u8> = [0xff, 0xfe]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        fs::write(&evidence, bytes).unwrap();
        assert_eq!(
            verified_shortcuts(&planned, &evidence).unwrap()[0].sha256,
            hash_bytes(b"created link")
        );
        assert!(verified_shortcuts(&[], &evidence).is_err());
        fs::write(&path, b"user replaced link").unwrap();
        assert!(verified_shortcuts(&planned, &evidence).is_err());
    }

    #[test]
    fn launch_validation_binds_receipt_task_inventory_and_current_entry_bytes() {
        let fixture = Fixture::new();
        let manifest = fixture.manifest();
        fs::write(fixture.0.join(&manifest.entry), b"abc").unwrap();
        let mut receipt = Receipt {
            install_scope: manifest.install_scope,
            selected_components: vec![],
            schema_version: 1,
            app_id: manifest.app_id.clone(),
            app_version: manifest.app_version.clone(),
            installation_id: "install".into(),
            task_id: "completed-task".into(),
            commit_id: "commit".into(),
            install_dir: fixture.0.to_string_lossy().into_owned(),
            manifest_sha256: hash_bytes(&serde_json::to_vec(&manifest).unwrap()),
            uninstaller_sha256: None,
            user_sid: "S-1-5-21-123".into(),
            files: manifest.files.clone(),
            shortcuts: Vec::new(),
            previous_commit_id: None,
        };
        let save = |receipt: &Receipt| {
            fs::write(
                fixture.0.join(RECEIPT_NAME),
                serde_json::to_vec(receipt).unwrap(),
            )
            .unwrap()
        };
        save(&receipt);
        drop(verified_entry(&manifest, &fixture.0, &receipt.user_sid, "completed-task").unwrap());
        assert!(verified_entry(&manifest, &fixture.0, &receipt.user_sid, "other-task").is_err());
        receipt.files[0].path = "unowned.txt".into();
        save(&receipt);
        assert!(
            verified_entry(&manifest, &fixture.0, &receipt.user_sid, "completed-task").is_err()
        );
        receipt.files = manifest.files.clone();
        save(&receipt);
        fs::write(fixture.0.join(&manifest.entry), b"changed").unwrap();
        assert!(
            verified_entry(&manifest, &fixture.0, &receipt.user_sid, "completed-task").is_err()
        );
    }
}
