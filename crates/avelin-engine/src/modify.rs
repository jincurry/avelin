//! Component changes preserve the observed state of untouched payload files.
use crate::{
    ModifyFileState, ModifyReport, ModifySnapshot, system,
    transaction::{Resource, confirm, digest, key, read},
};
use anyhow::{Context, Result, ensure};
use avelin_core::{InstallPlan, Manifest, Operation, RECEIPT_NAME, Receipt, hash_bytes};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

pub(crate) fn capture(
    manifest: &Manifest,
    receipt: &Receipt,
    target: &Path,
) -> Result<ModifySnapshot> {
    receipt.validate_manifest(manifest)?;
    avelin_core::install_root(target)?;
    ensure!(
        key(Path::new(&receipt.install_dir)) == key(target),
        "snapshot destination differs from receipt"
    );
    let mut files = Vec::new();
    for file in &receipt.files {
        let path = target.join(&file.path);
        avelin_core::reject_reparse(&path)?;
        let (size, sha256) = if path.try_exists()? {
            (Some(fs::metadata(&path)?.len()), Some(digest(&path)?))
        } else {
            (None, None)
        };
        files.push(ModifyFileState {
            path: file.path.clone(),
            size,
            sha256,
        });
    }
    Ok(ModifySnapshot {
        schema_version: 1,
        files,
    })
}

pub(crate) fn validate_snapshot(snapshot: &ModifySnapshot, previous: &Receipt) -> Result<()> {
    ensure!(
        snapshot.schema_version == 1 && snapshot.files.len() == previous.files.len(),
        "invalid component state snapshot"
    );
    for (state, file) in snapshot.files.iter().zip(&previous.files) {
        ensure!(
            state.path == file.path,
            "component snapshot file order or ownership changed"
        );
        ensure!(
            matches!((&state.sha256, state.size), (None, None))
                || state
                    .sha256
                    .as_ref()
                    .is_some_and(|h| h.len() == 64 && h.bytes().all(|v| v.is_ascii_hexdigit()))
                    && state.size.is_some(),
            "invalid component snapshot resource state"
        );
    }
    Ok(())
}

pub(crate) fn confirm_state(root: &Path, state: &ModifyFileState) -> Result<()> {
    let path = root.join(&state.path);
    avelin_core::reject_reparse(&path)?;
    if let Some(hash) = &state.sha256 {
        confirm(&path, hash)?;
        ensure!(
            Some(fs::metadata(&path)?.len()) == state.size,
            "component resource size changed: {}",
            path.display()
        );
    } else {
        ensure!(
            !path.try_exists()?,
            "previously missing component resource appeared: {}",
            path.display()
        );
    }
    Ok(())
}

pub(crate) fn load_snapshot(
    plan: &InstallPlan,
    previous: &Receipt,
    session: &Path,
) -> Result<ModifySnapshot> {
    let upgrade = plan
        .upgrade
        .as_ref()
        .context("component change plan missing")?;
    let bytes = read(&session.join("modify-snapshot.json"))?;
    ensure!(
        upgrade.modify_snapshot_sha256.as_deref() == Some(hash_bytes(&bytes).as_str()),
        "component snapshot differs from frozen plan"
    );
    let snapshot: ModifySnapshot = serde_json::from_slice(&bytes)?;
    validate_snapshot(&snapshot, previous)?;
    Ok(snapshot)
}

pub(crate) fn classify(
    snapshot: &ModifySnapshot,
    previous: &Receipt,
    next: &Receipt,
    root: &Path,
) -> Result<(Vec<Resource>, Vec<Resource>, Vec<PathBuf>)> {
    validate_snapshot(snapshot, previous)?;
    let next_paths: BTreeSet<_> = next.files.iter().map(|f| key(Path::new(&f.path))).collect();
    let mut removed = Vec::new();
    let mut preserved = Vec::new();
    let mut absent = Vec::new();
    for (state, file) in snapshot.files.iter().zip(&previous.files) {
        let path = root.join(&state.path);
        if let Some(hash) = &state.sha256 {
            let resource = Resource {
                path,
                size: state.size.context("snapshot size missing")?,
                sha256: hash.clone(),
            };
            if !next_paths.contains(&key(Path::new(&file.path))) && hash == &file.sha256 {
                removed.push(resource);
            } else {
                preserved.push(resource);
            }
        } else {
            absent.push(path);
        }
    }
    Ok((removed, preserved, absent))
}

pub(crate) fn verify_result(
    manifest: &Manifest,
    target: &Path,
    user_sid: &str,
    session: &Path,
) -> Result<ModifyReport> {
    avelin_core::scope::validate_session(manifest.install_scope, session)?;
    avelin_core::scope::require_elevation(manifest.install_scope)?;
    avelin_core::scope::validate_target(manifest.install_scope, target)?;
    let plan: InstallPlan = serde_json::from_slice(&read(&session.join("install-plan.json"))?)?;
    let previous_bytes = read(&session.join("previous-receipt.json"))?;
    let previous: Receipt = serde_json::from_slice(&previous_bytes)?;
    plan.validate_modify(manifest, &previous, &hash_bytes(&previous_bytes))?;
    ensure!(
        plan.operation == Operation::Modify
            && plan.user_sid == user_sid
            && key(&plan.install_dir) == key(target),
        "component result belongs to another operation or installation"
    );
    let snapshot = load_snapshot(&plan, &previous, session)?;
    let candidate: Receipt =
        serde_json::from_slice(&read(&session.join("receipt-candidate.json"))?)?;
    candidate.validate_manifest(manifest)?;
    ensure!(
        candidate.task_id == plan.task_id
            && candidate.task_id != previous.task_id
            && candidate.commit_id != previous.commit_id
            && candidate.installation_id == previous.installation_id
            && candidate.previous_commit_id.as_deref() == Some(previous.commit_id.as_str())
            && candidate.user_sid == user_sid
            && key(Path::new(&candidate.install_dir)) == key(target)
            && candidate.shortcuts == previous.shortcuts,
        "component change candidate does not preserve the installation and commit chain"
    );
    let installed: Receipt = serde_json::from_slice(&read(&target.join(RECEIPT_NAME))?)?;
    let mut expected = candidate.clone();
    expected.uninstaller_sha256 = previous.uninstaller_sha256.clone();
    expected.shortcuts = previous.shortcuts.clone();
    ensure!(
        serde_json::to_value(&installed)? == serde_json::to_value(&expected)?,
        "component change receipt differs from frozen intent"
    );
    ensure!(
        installed.selected_components == plan.selected_components,
        "component result selection differs from plan"
    );
    let previous_paths: BTreeSet<_> = previous
        .files
        .iter()
        .map(|f| key(Path::new(&f.path)))
        .collect();
    let next_paths: BTreeSet<_> = installed
        .files
        .iter()
        .map(|f| key(Path::new(&f.path)))
        .collect();
    let mut report = ModifyReport {
        schema_version: 1,
        task_id: plan.task_id.clone(),
        selected_components: installed.selection(manifest)?,
        added: vec![],
        removed: vec![],
        preserved: vec![],
        missing: vec![],
    };
    for file in &installed.files {
        if !previous_paths.contains(&key(Path::new(&file.path))) {
            let path = target.join(&file.path);
            confirm(&path, &file.sha256)?;
            ensure!(
                fs::metadata(&path)?.len() == file.size,
                "added component resource size differs"
            );
            report.added.push(file.path.clone());
        }
    }
    for (state, file) in snapshot.files.iter().zip(&previous.files) {
        if !next_paths.contains(&key(Path::new(&file.path)))
            && state.sha256.as_deref() == Some(file.sha256.as_str())
        {
            ensure!(
                !target.join(&file.path).try_exists()?,
                "removed component resource still exists"
            );
            report.removed.push(file.path.clone());
        } else {
            confirm_state(target, state)?;
            if state.sha256.is_none() {
                if next_paths.contains(&key(Path::new(&file.path))) {
                    report.missing.push(file.path.clone());
                } else {
                    report.removed.push(file.path.clone());
                }
            } else if state.sha256.as_deref() != Some(file.sha256.as_str()) {
                report.preserved.push(file.path.clone());
            }
        }
    }
    confirm(
        &target.join("uninstall.exe"),
        previous
            .uninstaller_sha256
            .as_deref()
            .context("original backend digest missing")?,
    )?;
    confirm(
        &target.join(".avelin/avelin-app.exe"),
        manifest
            .runtime_sha256
            .as_deref()
            .context("runtime digest missing")?,
    )?;
    let old_manifest = read(&session.join("previous-manifest.json"))?;
    confirm(
        &target.join(".avelin/manifest.json"),
        &hash_bytes(&old_manifest),
    )?;
    for shortcut in &installed.shortcuts {
        confirm(Path::new(&shortcut.path), &shortcut.sha256)?;
    }
    system::registration_for_product(manifest, target)?
        .context("component change registration missing")?;
    Ok(report)
}
