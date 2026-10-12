//! Draft plans for fresh install, controlled uninstall and an owned in-place upgrade.
//! A plan binds intent to a manifest; it does not assert free space, ownership or execution success.

use crate::{
    FileResource, Manifest, Receipt, SCHEMA_VERSION, hash_bytes, install_root, safe_id,
    same_windows_name,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Install,
    Uninstall,
    Upgrade,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
            Self::Upgrade => "upgrade",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutKind {
    Desktop,
    StartMenu,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ShortcutRecord {
    pub kind: ShortcutKind,
    pub path: String,
    pub target: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstallPlan {
    pub schema_version: u32,
    pub task_id: String,
    pub operation: Operation,
    pub app_id: String,
    pub app_version: String,
    pub install_dir: PathBuf,
    pub user_sid: String,
    pub manifest_sha256: String,
    pub payload_file_count: u64,
    pub payload_bytes: u64,
    /// Actual choices for this operation; package settings are defaults only.
    pub start_menu_shortcut: bool,
    #[serde(default)]
    pub desktop_shortcut: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<UpgradePlan>,
}

impl InstallPlan {
    pub fn new(
        manifest: &Manifest,
        operation: Operation,
        task_id: String,
        install_dir: PathBuf,
        user_sid: String,
    ) -> Result<Self> {
        let plan = Self {
            schema_version: SCHEMA_VERSION,
            task_id,
            operation,
            app_id: manifest.app_id.clone(),
            app_version: manifest.app_version.clone(),
            install_dir,
            user_sid,
            manifest_sha256: hash_bytes(&serde_json::to_vec(manifest)?),
            payload_file_count: manifest.files.len() as u64,
            payload_bytes: payload_bytes(manifest)?,
            start_menu_shortcut: manifest.start_menu_shortcut,
            desktop_shortcut: manifest.desktop_shortcut,
            upgrade: None,
        };
        plan.validate(manifest)?;
        Ok(plan)
    }

    /// Construct an upgrade bound to a caller-verified old receipt. The supplied
    /// hash is computed from the original receipt bytes, not its reserialization.
    pub fn new_upgrade(
        new: &Manifest,
        old: &Manifest,
        receipt: &Receipt,
        previous_receipt_sha256: &str,
        task_id: String,
        upgrade: UpgradePlan,
    ) -> Result<Self> {
        let mut plan = Self::new(
            new,
            Operation::Install,
            task_id,
            PathBuf::from(&receipt.install_dir),
            receipt.user_sid.clone(),
        )?;
        plan.operation = Operation::Upgrade;
        plan.desktop_shortcut = receipt
            .shortcuts
            .iter()
            .any(|link| link.kind == ShortcutKind::Desktop);
        plan.start_menu_shortcut = receipt
            .shortcuts
            .iter()
            .any(|link| link.kind == ShortcutKind::StartMenu);
        plan.upgrade = Some(upgrade);
        plan.validate_upgrade(new, old, receipt, previous_receipt_sha256)?;
        Ok(plan)
    }

    pub fn validate_upgrade(
        &self,
        new: &Manifest,
        old: &Manifest,
        receipt: &Receipt,
        previous_receipt_sha256: &str,
    ) -> Result<()> {
        self.validate(new)?;
        ensure!(self.operation == Operation::Upgrade, "not an upgrade plan");
        let upgrade = self.upgrade.as_ref().expect("validated upgrade field");
        upgrade.validate(new, old, receipt)?;
        ensure!(
            self.install_dir == PathBuf::from(&receipt.install_dir)
                && self.user_sid == receipt.user_sid
                && self.task_id != receipt.task_id
                && upgrade.previous_receipt_sha256 == previous_receipt_sha256,
            "upgrade plan does not match the previous receipt or fresh task"
        );
        Ok(())
    }

    /// Structure and manifest binding only. The coordinator still checks the current
    /// user, resource ownership, destination conflicts, locks and backend identity.
    pub fn validate(&self, manifest: &Manifest) -> Result<()> {
        manifest.validate()?;
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported plan schema"
        );
        safe_id(&self.task_id)?;
        ensure!(
            self.user_sid.starts_with("S-1-")
                && self.user_sid.len() <= 184
                && self.user_sid[4..]
                    .split('-')
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())),
            "invalid plan user SID"
        );
        install_root(&self.install_dir)?;
        ensure!(
            self.app_id == manifest.app_id
                && self.app_version == manifest.app_version
                && self.manifest_sha256 == hash_bytes(&serde_json::to_vec(manifest)?)
                && self.payload_file_count == manifest.files.len() as u64
                && self.payload_bytes == payload_bytes(manifest)?,
            "plan does not match manifest"
        );
        match (self.operation, &self.upgrade) {
            (Operation::Upgrade, Some(upgrade)) => {
                upgrade.validate_new(manifest)?;
                for private in [&upgrade.staging_dir, &upgrade.backup_dir] {
                    ensure!(
                        !paths_overlap(private, &self.install_dir),
                        "upgrade staging/backup must be outside the installation directory"
                    );
                }
            }
            (Operation::Upgrade, None) => {
                anyhow::bail!("upgrade requires the previous installation context")
            }
            (_, Some(_)) => anyhow::bail!("upgrade context is only valid for an upgrade"),
            (_, None) => {}
        }
        Ok(())
    }
}

fn payload_bytes(manifest: &Manifest) -> Result<u64> {
    manifest.files.iter().try_fold(0u64, |total, file| {
        total
            .checked_add(file.size)
            .ok_or_else(|| anyhow::anyhow!("payload byte count overflow"))
    })
}

/// SemVer precedence deliberately ignores build metadata. A rebuild of the same
/// version is not an upgrade, and prereleases follow SemVer ordering.
pub fn ensure_newer_version(old: &str, new: &str) -> Result<()> {
    let mut old = semver::Version::parse(old)?;
    let mut new = semver::Version::parse(new)?;
    old.build = semver::BuildMetadata::EMPTY;
    new.build = semver::BuildMetadata::EMPTY;
    ensure!(
        new > old,
        "upgrade version must be newer than the installed version"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileReplacement {
    pub before: FileResource,
    pub after: FileResource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpgradeDiff {
    pub added: Vec<FileResource>,
    pub replaced: Vec<FileReplacement>,
    pub removed: Vec<FileResource>,
    pub unchanged: Vec<FileResource>,
}

impl UpgradeDiff {
    /// Canonical Windows-name order. Ambiguous case-only renames and file/directory
    /// shape changes are rejected before the backend touches an existing install.
    pub fn between(old: &Manifest, new: &Manifest) -> Result<Self> {
        use std::collections::BTreeMap;
        old.validate()?;
        new.validate()?;
        let old_files: BTreeMap<_, _> = old
            .files
            .iter()
            .map(|file| (WindowsPath(file.path.clone()), file))
            .collect();
        let new_files: BTreeMap<_, _> = new
            .files
            .iter()
            .map(|file| (WindowsPath(file.path.clone()), file))
            .collect();
        for (files, other) in [(&old_files, &new_files), (&new_files, &old_files)] {
            for path in files.keys() {
                for (position, _) in path.0.match_indices('\\') {
                    ensure!(
                        !other.contains_key(&WindowsPath(path.0[..position].into())),
                        "upgrade file/directory shape change is unsupported: {}",
                        path.0
                    );
                }
            }
        }
        let mut diff = Self {
            added: vec![],
            replaced: vec![],
            removed: vec![],
            unchanged: vec![],
        };
        for (path, after) in &new_files {
            match old_files.get(path) {
                Some(before) => {
                    ensure!(
                        before.path == after.path,
                        "upgrade path spelling change is unsupported: {} -> {}",
                        before.path,
                        after.path
                    );
                    if before == after {
                        diff.unchanged.push((*after).clone());
                    } else {
                        diff.replaced.push(FileReplacement {
                            before: (*before).clone(),
                            after: (*after).clone(),
                        });
                    }
                }
                None => diff.added.push((*after).clone()),
            }
        }
        for (path, before) in &old_files {
            if !new_files.contains_key(path) {
                diff.removed.push((*before).clone());
            }
        }
        Ok(diff)
    }

    fn previous_files(&self) -> Vec<FileResource> {
        self.removed
            .iter()
            .chain(self.unchanged.iter())
            .chain(self.replaced.iter().map(|replacement| &replacement.before))
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpgradePlan {
    pub previous_version: String,
    pub previous_manifest_sha256: String,
    /// SHA-256 of the exact previous receipt bytes, including its original formatting.
    pub previous_receipt_sha256: String,
    pub previous_task_id: String,
    pub previous_commit_id: String,
    pub installation_id: String,
    pub diff: UpgradeDiff,
    pub staging_dir: PathBuf,
    pub backup_dir: PathBuf,
    /// Conservative staging + old payload backup + new deployment estimate.
    /// The executor adds metadata/runtime overhead and checks actual free space.
    pub required_space_bytes: u64,
}

impl UpgradePlan {
    pub fn validate(&self, new: &Manifest, old: &Manifest, receipt: &Receipt) -> Result<()> {
        self.validate_new(new)?;
        old.validate()?;
        ensure!(
            new.app_id == old.app_id && receipt.app_id == old.app_id,
            "upgrade application identity mismatch"
        );
        ensure!(
            receipt.schema_version == SCHEMA_VERSION
                && receipt.app_version == old.app_version
                && receipt.manifest_sha256 == hash_bytes(&serde_json::to_vec(old)?)
                && receipt.files == old.files,
            "previous receipt does not match the installed manifest"
        );
        ensure!(
            self.previous_version == old.app_version
                && self.previous_manifest_sha256 == receipt.manifest_sha256
                && self.previous_task_id == receipt.task_id
                && self.previous_commit_id == receipt.commit_id
                && self.installation_id == receipt.installation_id
                && self.diff == UpgradeDiff::between(old, new)?,
            "upgrade context does not match the previous installation"
        );
        Ok(())
    }

    fn validate_new(&self, new: &Manifest) -> Result<()> {
        new.validate()?;
        ensure_newer_version(&self.previous_version, &new.app_version)?;
        for id in [
            &self.previous_task_id,
            &self.previous_commit_id,
            &self.installation_id,
        ] {
            safe_id(id)?;
        }
        for hash in [
            &self.previous_manifest_sha256,
            &self.previous_receipt_sha256,
        ] {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid previous SHA-256"
            );
        }
        for path in [&self.staging_dir, &self.backup_dir] {
            install_root(path)?;
        }
        ensure!(
            !paths_overlap(&self.staging_dir, &self.backup_dir),
            "upgrade staging and backup overlap"
        );
        // Reconstruct only the old resource set to validate an exhaustive, disjoint
        // diff against the new manifest. Actual old identity is checked by validate.
        let mut previous = new.clone();
        previous.app_version = self.previous_version.clone();
        previous.files = self.diff.previous_files();
        previous.entry = previous
            .files
            .first()
            .map(|file| file.path.clone())
            .unwrap_or_default();
        previous.presentation = None;
        ensure!(
            self.diff == UpgradeDiff::between(&previous, new)?,
            "upgrade diff does not match new payload"
        );
        let minimum = payload_bytes(new)?
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(payload_bytes(&previous).ok()?))
            .ok_or_else(|| anyhow::anyhow!("upgrade space estimate overflow"))?;
        ensure!(
            self.required_space_bytes >= minimum,
            "upgrade space estimate is below staging/backup/deployment payload size"
        );
        Ok(())
    }
}

fn paths_overlap(left: &std::path::Path, right: &std::path::Path) -> bool {
    fn ancestor(left: &std::path::Path, right: &std::path::Path) -> bool {
        let mut right = right.components();
        left.components().all(|part| {
            right.next().is_some_and(|other| {
                same_windows_name(
                    &part.as_os_str().to_string_lossy(),
                    &other.as_os_str().to_string_lossy(),
                )
            })
        })
    }
    ancestor(left, right) || ancestor(right, left)
}

#[derive(Eq)]
struct WindowsPath(String);
impl PartialEq for WindowsPath {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl PartialOrd for WindowsPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for WindowsPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        #[cfg(windows)]
        {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn CompareStringOrdinal(
                    a: *const u16,
                    alen: i32,
                    b: *const u16,
                    blen: i32,
                    ignore: i32,
                ) -> i32;
            }
            let a: Vec<u16> = self.0.encode_utf16().collect();
            let b: Vec<u16> = other.0.encode_utf16().collect();
            match unsafe {
                CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1)
            } {
                1 => std::cmp::Ordering::Less,
                2 => std::cmp::Ordering::Equal,
                3 => std::cmp::Ordering::Greater,
                _ => unreachable!("validated bounded Windows path comparison"),
            }
        }
        #[cfg(not(windows))]
        {
            self.0.to_uppercase().cmp(&other.0.to_uppercase())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        serde_json::from_str(include_str!("../../../docs/schemas/examples/manifest.json")).unwrap()
    }

    fn upgrade_fixture() -> (Manifest, Manifest, Receipt, UpgradePlan) {
        let old = manifest();
        let mut new = old.clone();
        new.app_version = "0.2.0".into();
        new.files[0].sha256 = "b".repeat(64);
        let receipt: Receipt =
            serde_json::from_str(include_str!("../../../docs/schemas/examples/receipt.json"))
                .unwrap();
        let upgrade = UpgradePlan {
            previous_version: old.app_version.clone(),
            previous_manifest_sha256: receipt.manifest_sha256.clone(),
            previous_receipt_sha256: hash_bytes(include_bytes!(
                "../../../docs/schemas/examples/receipt.json"
            )),
            previous_task_id: receipt.task_id.clone(),
            previous_commit_id: receipt.commit_id.clone(),
            installation_id: receipt.installation_id.clone(),
            diff: UpgradeDiff::between(&old, &new).unwrap(),
            staging_dir: std::env::temp_dir().join("AvelinM3Private").join("staging"),
            backup_dir: std::env::temp_dir().join("AvelinM3Private").join("backup"),
            required_space_bytes: 1024 * 1024,
        };
        (old, new, receipt, upgrade)
    }

    #[test]
    fn upgrade_semver_rejects_same_precedence_and_downgrade() {
        for (old, new) in [
            ("1.2.3", "1.2.3"),
            ("1.2.3+a", "1.2.3+b"),
            ("1.2.3", "1.2.2"),
            ("1.2.3", "1.2.3-rc.1"),
            ("1.2.3-rc.2", "1.2.3-rc.1"),
            ("1.2", "1.3.0"),
            ("1.2.3", "v2.0.0"),
        ] {
            assert!(ensure_newer_version(old, new).is_err(), "{old} -> {new}");
        }
        for (old, new) in [
            ("1.2.3", "1.2.4"),
            ("1.2.3-rc.1", "1.2.3"),
            ("1.2.3-beta.9", "1.2.3-beta.10"),
        ] {
            ensure_newer_version(old, new).unwrap();
        }
    }

    #[test]
    fn upgrade_diff_is_exhaustive_canonical_and_preserves_old_and_new_metadata() {
        let mut old = manifest();
        let resource = |path: &str, size, hash: &str| FileResource {
            path: path.into(),
            size,
            sha256: hash.repeat(64),
        };
        old.files = vec![
            resource("shared.txt", 1, "a"),
            resource("old.exe", 2, "b"),
            resource("unchanged.txt", 3, "c"),
        ];
        old.entry = "old.exe".into();
        let mut new = old.clone();
        new.app_version = "0.2.0".into();
        new.entry = "bin\\new.exe".into();
        new.files = vec![
            resource("unchanged.txt", 3, "c"),
            resource("bin\\new.exe", 4, "d"),
            resource("shared.txt", 5, "e"),
        ];
        let diff = UpgradeDiff::between(&old, &new).unwrap();
        assert_eq!(diff.added, vec![new.files[1].clone()]);
        assert_eq!(diff.removed, vec![old.files[1].clone()]);
        assert_eq!(diff.unchanged, vec![old.files[2].clone()]);
        assert_eq!(
            diff.replaced,
            vec![FileReplacement {
                before: old.files[0].clone(),
                after: new.files[2].clone()
            }]
        );
        old.files.reverse();
        new.files.reverse();
        assert_eq!(diff, UpgradeDiff::between(&old, &new).unwrap());
    }

    #[test]
    fn upgrade_rejects_ambiguous_case_and_file_directory_replacements() {
        let mut old = manifest();
        old.files[0].path = "Data".into();
        old.entry = "Data".into();
        let mut new = old.clone();
        new.files[0].path = "data".into();
        new.entry = "data".into();
        assert!(UpgradeDiff::between(&old, &new).is_err());
        new.files[0].path = "Data\\child".into();
        new.entry = new.files[0].path.clone();
        assert!(UpgradeDiff::between(&old, &new).is_err());
        assert!(UpgradeDiff::between(&new, &old).is_err());
    }

    #[test]
    fn upgrade_plan_binds_exact_old_receipt_and_retains_legacy_shape() {
        let (old, new, mut receipt, upgrade) = upgrade_fixture();
        receipt.install_dir = std::env::temp_dir()
            .join("AvelinM3Installed")
            .to_string_lossy()
            .into_owned();
        let hash = upgrade.previous_receipt_sha256.clone();
        let plan =
            InstallPlan::new_upgrade(&new, &old, &receipt, &hash, "new-task".into(), upgrade)
                .unwrap();
        assert_eq!(plan.operation, Operation::Upgrade);
        assert_eq!(plan.install_dir, PathBuf::from(&receipt.install_dir));
        assert_eq!(plan.user_sid, receipt.user_sid);
        let roundtrip: InstallPlan =
            serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
        roundtrip
            .validate_upgrade(&new, &old, &receipt, &hash)
            .unwrap();
        assert!(
            roundtrip
                .validate_upgrade(&new, &old, &receipt, &"0".repeat(64))
                .is_err()
        );
        let mut forged = plan.clone();
        forged.install_dir = std::env::temp_dir().join("AnotherInstall");
        assert!(
            forged
                .validate_upgrade(&new, &old, &receipt, &hash)
                .is_err()
        );
        forged = plan.clone();
        forged.task_id = receipt.task_id.clone();
        assert!(
            forged
                .validate_upgrade(&new, &old, &receipt, &hash)
                .is_err()
        );
        forged = plan.clone();
        forged.operation = Operation::Install;
        assert!(forged.validate(&new).is_err());
        assert!(
            InstallPlan::new(
                &new,
                Operation::Upgrade,
                "task".into(),
                plan.install_dir,
                receipt.user_sid.clone()
            )
            .is_err()
        );
        assert!(receipt.previous_commit_id.is_none());
        assert!(
            serde_json::to_value(&receipt)
                .unwrap()
                .get("previous_commit_id")
                .is_none()
        );
    }

    #[test]
    fn upgrade_plan_rejects_forged_identity_diff_and_unsafe_workspaces() {
        let (old, new, receipt, upgrade) = upgrade_fixture();
        upgrade.validate(&new, &old, &receipt).unwrap();
        let mut other_app = new.clone();
        other_app.app_id = "other.app".into();
        assert!(upgrade.validate(&other_app, &old, &receipt).is_err());
        for field in [
            "previous_version",
            "previous_manifest_sha256",
            "previous_task_id",
            "previous_commit_id",
            "installation_id",
        ] {
            let mut value = serde_json::to_value(&upgrade).unwrap();
            value[field] = serde_json::json!(if field.contains("sha256") {
                "c".repeat(64)
            } else {
                "changed".into()
            });
            let forged: UpgradePlan = serde_json::from_value(value).unwrap();
            assert!(forged.validate(&new, &old, &receipt).is_err(), "{field}");
        }
        let mut forged = upgrade.clone();
        forged.diff.replaced.clear();
        assert!(forged.validate(&new, &old, &receipt).is_err());
        forged = upgrade.clone();
        forged.required_space_bytes = 0;
        assert!(forged.validate(&new, &old, &receipt).is_err());
        forged = upgrade.clone();
        forged.backup_dir = forged.staging_dir.join("nested");
        assert!(forged.validate(&new, &old, &receipt).is_err());
        forged = upgrade.clone();
        forged.staging_dir = PathBuf::from("relative");
        assert!(forged.validate(&new, &old, &receipt).is_err());
        let mut receipt = receipt;
        receipt.install_dir = upgrade
            .staging_dir
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            InstallPlan::new_upgrade(
                &new,
                &old,
                &receipt,
                &upgrade.previous_receipt_sha256,
                "new-task".into(),
                upgrade.clone()
            )
            .is_err()
        );
    }

    fn plan(manifest: &Manifest) -> InstallPlan {
        InstallPlan::new(
            manifest,
            Operation::Install,
            "m1-plan-test".into(),
            std::env::temp_dir().join("AvelinM1PlanExample"),
            "S-1-5-21-100-100-100-1001".into(),
        )
        .unwrap()
    }

    #[test]
    fn plan_binds_supported_intent_and_manifest_without_mutating_old_serialization() {
        let manifest = manifest();
        let value = serde_json::to_value(&manifest).unwrap();
        assert!(value.get("presentation").is_none());
        let original = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../docs/schemas/examples/manifest.json"
        ))
        .unwrap();
        assert_eq!(value, original);
        assert_eq!(
            hash_bytes(&serde_json::to_vec(&manifest).unwrap()),
            "18921fcf49f28924a1802434b8b171ebc0561213e9e77596bec89de10da78366"
        );
        let plan = plan(&manifest);
        let decoded: InstallPlan =
            serde_json::from_slice(&serde_json::to_vec(&plan).unwrap()).unwrap();
        assert_eq!(decoded, plan);
        decoded.validate(&manifest).unwrap();
        let mut forged = decoded.clone();
        forged.payload_bytes += 1;
        assert!(forged.validate(&manifest).is_err());
        let mut changed = manifest.clone();
        changed.app_version = "0.2.0".into();
        assert!(decoded.validate(&changed).is_err());
    }

    #[test]
    fn documented_models_preserve_manifest_binding_and_legacy_receipt_shape() {
        let manifest = manifest();
        let documented: InstallPlan = serde_json::from_str(include_str!(
            "../../../docs/schemas/examples/install-plan.json"
        ))
        .unwrap();
        assert_eq!(
            documented.manifest_sha256,
            hash_bytes(&serde_json::to_vec(&manifest).unwrap())
        );
        assert_eq!(documented.payload_file_count, manifest.files.len() as u64);
        assert_eq!(documented.payload_bytes, payload_bytes(&manifest).unwrap());
        let receipt: crate::Receipt =
            serde_json::from_str(include_str!("../../../docs/schemas/examples/receipt.json"))
                .unwrap();
        assert_eq!(receipt.task_id, documented.task_id);
        assert_eq!(receipt.manifest_sha256, documented.manifest_sha256);
        let original: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/schemas/examples/receipt.json"))
                .unwrap();
        assert_eq!(serde_json::to_value(&receipt).unwrap(), original);
    }

    #[test]
    fn plan_rejects_unsupported_fields_identity_and_overflow() {
        let manifest = manifest();
        let mut value = serde_json::to_value(plan(&manifest)).unwrap();
        value["upgrade_from"] = serde_json::json!("0.0.1");
        assert!(serde_json::from_value::<InstallPlan>(value).is_err());
        let mut invalid = plan(&manifest);
        invalid.user_sid = "someone".into();
        assert!(invalid.validate(&manifest).is_err());
        invalid = plan(&manifest);
        invalid.schema_version += 1;
        assert!(invalid.validate(&manifest).is_err());
        let mut large = manifest.clone();
        large.files[0].size = u64::MAX;
        let mut second = large.files[0].clone();
        second.path = "second.exe".into();
        second.size = 1;
        large.files.push(second);
        assert!(payload_bytes(&large).is_err());
    }

    #[test]
    fn package_shortcut_defaults_and_actual_choices_are_independent() {
        let mut manifest = manifest();
        assert!(!manifest.desktop_shortcut);
        assert!(
            serde_json::to_value(&manifest)
                .unwrap()
                .get("desktop_shortcut")
                .is_none()
        );
        manifest.desktop_shortcut = true;
        manifest.start_menu_shortcut = true;
        let mut plan = plan(&manifest);
        assert!(plan.desktop_shortcut && plan.start_menu_shortcut);
        plan.desktop_shortcut = false;
        plan.start_menu_shortcut = false;
        plan.validate(&manifest).unwrap();
        assert!(
            serde_json::to_value(&manifest).unwrap()["desktop_shortcut"]
                .as_bool()
                .unwrap()
        );
        let mut legacy = serde_json::to_value(&manifest).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("start_menu_shortcut");
        assert!(
            !serde_json::from_value::<Manifest>(legacy)
                .unwrap()
                .start_menu_shortcut
        );
    }
}
