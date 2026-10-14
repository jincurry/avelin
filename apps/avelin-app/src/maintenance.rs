//! Read-only setup discovery. Every mutation remains in the authenticated worker.
use crate::{execution_files::PinnedFile, resources, upgrade};
use anyhow::{Context, Result, ensure};
use avelin_core::{Manifest, RECEIPT_NAME, Receipt, hash_bytes, reject_reparse};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub type Finding = (String, String);

fn read_json(path: &Path, maximum: u64) -> Result<(Vec<u8>, Value)> {
    let pin = PinnedFile::open(path)?;
    ensure!(
        std::fs::metadata(path)?.len() <= maximum,
        "maintenance evidence is too large"
    );
    let bytes = pin.read_bytes()?;
    let value = serde_json::from_slice(&bytes)?;
    Ok((bytes, value))
}

/// A marker supplies only a candidate evidence location, never an executable.
/// The engine subsequently verifies task, product, user, paths and process identities.
pub fn pending_session(manifest: &Manifest, target: &Path) -> Result<Option<PathBuf>> {
    let marker = target.join(upgrade::MARKER);
    reject_reparse(&marker)?;
    if !marker.try_exists()? {
        return Ok(None);
    }
    let (_, value) = read_json(&marker, 128 * 1024)?;
    ensure!(
        value["schema_version"] == 2
            && value["engine"] == "rust"
            && value["app_id"] == manifest.app_id,
        "unfinished transaction does not belong to this Rust package"
    );
    let session = PathBuf::from(
        value["session_dir"]
            .as_str()
            .context("transaction session missing")?,
    );
    ensure!(
        session.is_absolute(),
        "transaction session must be absolute"
    );
    reject_reparse(&session)?;
    Ok(Some(session))
}

/// The retained coordinator lock also covers a task interrupted before it could
/// create the target marker. Treat its session as untrusted discovery data.
pub fn pending_session_from_lock(manifest: &Manifest, sid: &str) -> Result<Option<PathBuf>> {
    let path =
        avelin_core::scope::product_lock_path(manifest.install_scope, &manifest.app_id, sid)?;
    reject_reparse(&path)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    let pin = PinnedFile::open(&path)?;
    ensure!(
        std::fs::metadata(&path)?.len() <= 128 * 1024,
        "retained lock is too large"
    );
    session_hint(&String::from_utf8(pin.read_bytes()?)?)
}

fn session_hint(lock: &str) -> Result<Option<PathBuf>> {
    let mut lines = lock.lines();
    lines
        .next()
        .context("empty product lock")?
        .trim()
        .parse::<u32>()
        .context("invalid product lock owner")?;
    let sessions: Vec<_> = lines
        .filter_map(|line| line.strip_prefix("session="))
        .collect();
    ensure!(sessions.len() <= 1, "ambiguous retained task session");
    sessions
        .first()
        .map(|value| {
            let path = PathBuf::from(value);
            ensure!(path.is_absolute(), "retained session must be absolute");
            reject_reparse(&path)?;
            Ok(path)
        })
        .transpose()
}

pub fn recovery_target(manifest: &Manifest, session: &Path) -> Result<PathBuf> {
    let (_, value) = read_json(&session.join("session.json"), 128 * 1024)?;
    let record: avelin_core::session::SessionRecord = serde_json::from_value(value)?;
    ensure!(
        record.schema_version == 1 && record.app_id == manifest.app_id,
        "recovery session belongs to a different product"
    );
    avelin_core::install_root(&record.install_dir)?;
    Ok(record.install_dir)
}

pub fn matching_installed_manifest(manifest: &Manifest, target: &Path) -> Result<()> {
    let path = target.join(".avelin/manifest.json");
    reject_reparse(&path)?;
    // A complete receipt still binds the original package when this file is
    // missing. Existing different metadata never receives an inferred repair.
    if !path.try_exists()? {
        return Ok(());
    }
    let (_, value) = read_json(&path, 32 * 1024 * 1024)?;
    let installed: Manifest = serde_json::from_value(value)?;
    installed.validate()?;
    ensure!(
        hash_bytes(&serde_json::to_vec(&installed)?) == hash_bytes(&serde_json::to_vec(manifest)?),
        "repair requires the original matching package; installed metadata or package differs"
    );
    Ok(())
}

pub fn installed_version(target: &Path) -> Result<String> {
    if target.join(".avelin/manifest.json").try_exists()? {
        return upgrade::previous_version(target);
    }
    let (_, value) = read_json(&target.join(RECEIPT_NAME), 32 * 1024 * 1024)?;
    let receipt: Receipt = serde_json::from_value(value)?;
    Ok(receipt.app_version)
}

pub struct RepairInputs {
    pub receipt: Receipt,
    pub receipt_bytes: Vec<u8>,
    pub findings: Vec<Finding>,
}

pub fn repair_inputs(manifest: &Manifest, target: &Path, sid: &str) -> Result<RepairInputs> {
    repair_inputs_impl(manifest, target, sid, false)
}

/// Locked payloads can still reach the confirmation page and its retry flow.
/// Ownership metadata and all other errors remain strict; execution rechecks.
pub fn repair_preview(manifest: &Manifest, target: &Path, sid: &str) -> Result<RepairInputs> {
    repair_inputs_impl(manifest, target, sid, true)
}

fn repair_inputs_impl(
    manifest: &Manifest,
    target: &Path,
    sid: &str,
    allow_busy: bool,
) -> Result<RepairInputs> {
    avelin_core::install_root(target)?;
    upgrade::ensure_no_pending(target)?;
    matching_installed_manifest(manifest, target)?;
    let (receipt_bytes, value) = read_json(&target.join(RECEIPT_NAME), 32 * 1024 * 1024)?;
    let receipt: Receipt = serde_json::from_value(value)?;
    resources::validate_receipt(&receipt, manifest, target, sid)?;
    for id in [
        &receipt.installation_id,
        &receipt.task_id,
        &receipt.commit_id,
    ] {
        avelin_core::safe_id(id)?;
    }
    let mut expected: Vec<_> = receipt
        .files
        .iter()
        .map(|file| (target.join(&file.path), file.sha256.clone()))
        .collect();
    expected.push((
        target.join(".avelin/avelin-app.exe"),
        manifest
            .runtime_sha256
            .clone()
            .context("runtime integrity missing")?,
    ));
    expected.push((
        target.join("uninstall.exe"),
        receipt
            .uninstaller_sha256
            .clone()
            .context("uninstaller integrity missing")?,
    ));
    expected.push((
        target.join(".avelin/manifest.json"),
        hash_bytes(&serde_json::to_vec_pretty(manifest)?),
    ));
    expected.extend(
        receipt
            .shortcuts
            .iter()
            .map(|link| (PathBuf::from(&link.path), link.sha256.clone())),
    );
    let mut findings = Vec::new();
    for (path, expected_hash) in expected {
        reject_reparse(&path)?;
        let display = path.display();
        if !path.try_exists()? {
            findings.push((format!("缺失：{display}"), format!("Missing: {display}")));
        } else {
            match PinnedFile::open(&path).and_then(|file| file.sha256()) {
                Ok(hash) if hash != expected_hash => findings.push((
                    format!("已修改，默认保留：{display}"),
                    format!("Changed; preserved by default: {display}"),
                )),
                Ok(_) => {}
                Err(error)
                    if allow_busy
                        && error.chain().any(|cause| {
                            cause
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|io| matches!(io.raw_os_error(), Some(32 | 33)))
                        }) =>
                {
                    findings.push((
                        format!("文件被占用，开始前需要重新检查：{display}"),
                        format!("File in use; check again before starting: {display}"),
                    ))
                }
                Err(error) => return Err(error),
            }
        }
    }
    match upgrade::registration(manifest.install_scope, &manifest.app_id)? {
        Some(registration) => {
            upgrade::verify_registration(&registration, manifest, target)
                .context("existing application registration differs; refusing to overwrite it")?;
        }
        None => findings.push((
            "缺失：Windows 应用登记".into(),
            "Missing: Windows application registration".into(),
        )),
    }
    if findings.is_empty() {
        findings.push((
            "受管资源检查通过，无缺失或修改项。".into(),
            "Managed resources checked; no missing or changed items.".into(),
        ));
    }
    Ok(RepairInputs {
        receipt,
        receipt_bytes,
        findings,
    })
}

pub fn recovery_findings(report: &avelin_engine::RecoveryInspection) -> Vec<Finding> {
    let decision = match report.decision.as_str() {
        "rollback" => (
            "操作尚未提交：将按记录恢复原状。",
            "The operation was not committed: restore its recorded previous state.",
        ),
        "confirm_committed" => (
            "操作已经提交：将核对已提交结果。",
            "The operation was committed: verify its committed result.",
        ),
        "already_recovered" => (
            "此任务已完成恢复：将重新核对记录。",
            "This task was recovered: verify its recorded outcome again.",
        ),
        _ => (
            "恢复计划需要进一步检查。",
            "Recovery requires further inspection.",
        ),
    };
    vec![(decision.0.into(), decision.1.into())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "avelin-maintenance-discovery-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
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
                        .starts_with("avelin-maintenance-discovery-")
            );
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn manifest() -> Manifest {
        serde_json::from_str(include_str!("../../../docs/schemas/examples/manifest.json")).unwrap()
    }

    #[test]
    fn schema_two_marker_discovers_only_the_expected_rust_product() {
        let fixture = Fixture::new();
        let manifest = manifest();
        assert!(pending_session(&manifest, &fixture.0).unwrap().is_none());
        fs::create_dir(fixture.0.join(".avelin")).unwrap();
        let path = fixture.0.join(upgrade::MARKER);
        let session = fixture.0.join("evidence");
        let mut marker = serde_json::json!({"schema_version":2,"engine":"rust","app_id":manifest.app_id,"session_dir":session});
        fs::write(&path, serde_json::to_vec(&marker).unwrap()).unwrap();
        assert_eq!(
            pending_session(&manifest, &fixture.0).unwrap(),
            Some(session)
        );
        for (field, invalid) in [
            ("schema_version", serde_json::json!(1)),
            ("app_id", serde_json::json!("other.product")),
            ("session_dir", serde_json::json!("relative")),
        ] {
            let original = marker[field].clone();
            marker[field] = invalid;
            let bytes = serde_json::to_vec(&marker).unwrap();
            fs::write(&path, &bytes).unwrap();
            assert!(pending_session(&manifest, &fixture.0).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            marker[field] = original;
        }
    }

    #[test]
    fn immutable_lock_hints_keep_pid_only_compatibility_and_reject_ambiguity() {
        assert_eq!(session_hint("123\n").unwrap(), None);
        assert_eq!(
            session_hint("123\nsession=C:\\Task Evidence\ninstall_dir=C:\\App\n").unwrap(),
            Some(PathBuf::from(r"C:\Task Evidence"))
        );
        assert!(session_hint("bad\nsession=C:\\Task\n").is_err());
        assert!(session_hint("123\nsession=relative\n").is_err());
        assert!(session_hint("123\nsession=C:\\First\nsession=C:\\Second\n").is_err());
    }

    #[test]
    fn missing_manifest_is_recoverable_but_different_existing_metadata_is_refused() {
        let fixture = Fixture::new();
        let manifest = manifest();
        matching_installed_manifest(&manifest, &fixture.0).unwrap();
        fs::create_dir(fixture.0.join(".avelin")).unwrap();
        let path = fixture.0.join(".avelin/manifest.json");
        fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        matching_installed_manifest(&manifest, &fixture.0).unwrap();
        let mut changed = manifest.clone();
        changed.app_version = "99.0.0".into();
        let bytes = serde_json::to_vec_pretty(&changed).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(matching_installed_manifest(&manifest, &fixture.0).is_err());
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}
