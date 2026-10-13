//! Launch maintenance from the installed offline payload without holding files
//! that the transaction may replace or remove. No registry command is executed.
use crate::{
    execution_files::{self, PinnedDirectory, PinnedFile},
    resources,
};
use anyhow::{Context, Result, ensure};
use avelin_core::{Manifest, RECEIPT_NAME, Receipt};
use std::path::Path;

pub struct PreparedMaintenance {
    pub runtime: PinnedFile,
    pub backend: PinnedFile,
    pub manifest: PinnedFile,
    pub directory: PinnedDirectory,
}

fn bounded_file(path: &Path) -> Result<(PinnedFile, Vec<u8>)> {
    let file = PinnedFile::open(path)?;
    ensure!(
        std::fs::metadata(path)?.len() <= 32 * 1024 * 1024,
        "installed maintenance metadata exceeds its supported size"
    );
    let bytes = file.read_bytes()?;
    Ok((file, bytes))
}

pub fn prepare(root: &Path, user_sid: &str) -> Result<PreparedMaintenance> {
    prepare_inputs(root, user_sid).context(
        "installed maintenance inputs are unavailable or inconsistent; use the original Setup and retained session to recover or repair",
    )
}

fn prepare_inputs(root: &Path, user_sid: &str) -> Result<PreparedMaintenance> {
    let root = std::path::absolute(root)?;
    avelin_core::install_root(&root)?;
    let _root_pin = PinnedDirectory::open(&root)?;
    let (_manifest_pin, manifest_bytes) = bounded_file(&root.join(".avelin/manifest.json"))
        .context("installed manifest is unavailable; use the original Setup to repair")?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    manifest.validate()?;
    let (_receipt_pin, receipt_bytes) = bounded_file(&root.join(RECEIPT_NAME))
        .context("installed receipt is unavailable; retain task evidence for recovery")?;
    let receipt: Receipt = serde_json::from_slice(&receipt_bytes)?;
    resources::validate_receipt(&receipt, &manifest, &root, user_sid)?;
    let backend = PinnedFile::open(&root.join("uninstall.exe"))
        .context("installed offline payload is unavailable; use the original Setup to repair")?;
    ensure!(
        receipt.uninstaller_sha256.as_deref() == Some(backend.sha256()?.as_str()),
        "installed offline payload was changed; use the original Setup to repair"
    );
    resources::validate_backend(&backend, &manifest)?;
    let runtime = PinnedFile::open(&root.join(".avelin/avelin-app.exe")).context(
        "installed maintenance runtime is unavailable; use the original Setup to repair",
    )?;
    ensure!(
        manifest.runtime_sha256.as_deref() == Some(runtime.sha256()?.as_str()),
        "installed maintenance runtime was changed; use the original Setup to repair"
    );

    // Create every copy while the verified sources and their ancestor directories
    // are pinned. Return only private-copy handles; source handles close here.
    let directory = execution_files::create_private_session(
        &std::env::temp_dir().join("AvelinM0"),
        "maintenance-bootstrap",
    )?;
    let runtime = execution_files::copy_file(&directory, "maintenance-coordinator.exe", &runtime)?;
    let backend = execution_files::copy_file(&directory, "maintenance-worker.exe", &backend)?;
    let manifest = execution_files::create_file(&directory, "manifest.json", &manifest_bytes)?;
    Ok(PreparedMaintenance {
        runtime,
        backend,
        manifest,
        directory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    struct Fixture {
        root: PathBuf,
        manifest: Manifest,
        receipt: Receipt,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "avelin-installed-maintenance-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir(&root).unwrap();
            fs::create_dir(root.join(".avelin")).unwrap();
            let mut manifest: Manifest =
                serde_json::from_str(include_str!("../../../docs/schemas/examples/manifest.json"))
                    .unwrap();
            manifest.runtime_sha256 = Some(avelin_core::hash_bytes(b"runtime"));
            fs::write(root.join(".avelin/avelin-app.exe"), b"runtime").unwrap();
            let mut payload = avelin_core::native_payload::MAGIC.to_vec();
            payload.extend(1u32.to_le_bytes());
            payload.extend((manifest.files.len() as u32).to_le_bytes());
            payload.extend(7u64.to_le_bytes());
            payload.extend(b"runtime");
            for file in &manifest.files {
                assert_eq!(file.sha256, avelin_core::hash_bytes(b"abc"));
                payload.extend(3u64.to_le_bytes());
                payload.extend(b"abc");
            }
            fs::write(root.join("worker.stub"), b"inert worker fixture").unwrap();
            fs::write(root.join("payload.bin"), payload).unwrap();
            avelin_core::assemble(
                &root.join("worker.stub"),
                &root.join("payload.bin"),
                &manifest,
                &root.join("uninstall.exe"),
            )
            .unwrap();
            fs::write(
                root.join(".avelin/manifest.json"),
                serde_json::to_vec_pretty(&manifest).unwrap(),
            )
            .unwrap();
            let mut receipt: Receipt =
                serde_json::from_str(include_str!("../../../docs/schemas/examples/receipt.json"))
                    .unwrap();
            receipt.install_dir = root.to_string_lossy().into_owned();
            receipt.manifest_sha256 =
                avelin_core::hash_bytes(&serde_json::to_vec(&manifest).unwrap());
            receipt.uninstaller_sha256 =
                Some(avelin_core::hash_file(&root.join("uninstall.exe")).unwrap());
            fs::write(
                root.join(RECEIPT_NAME),
                serde_json::to_vec_pretty(&receipt).unwrap(),
            )
            .unwrap();
            Self {
                root,
                manifest,
                receipt,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            assert_eq!(self.root.parent(), Some(std::env::temp_dir().as_path()));
            assert!(
                self.root
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("avelin-installed-maintenance-")
            );
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn installed_maintenance_uses_verified_private_copies_and_releases_sources() {
        let fixture = Fixture::new();
        let prepared = prepare(&fixture.root, &fixture.receipt.user_sid).unwrap();
        assert!(!prepared.directory.path().starts_with(&fixture.root));
        assert_eq!(
            Some(prepared.runtime.sha256().unwrap().as_str()),
            fixture.manifest.runtime_sha256.as_deref()
        );
        assert_eq!(
            Some(prepared.backend.sha256().unwrap().as_str()),
            fixture.receipt.uninstaller_sha256.as_deref()
        );
        resources::validate_backend(&prepared.backend, &fixture.manifest).unwrap();
        // A repair may replace these immediately; returning source pins would
        // make these writes fail with sharing violations on Windows.
        fs::write(
            fixture.root.join(".avelin/avelin-app.exe"),
            b"changed after bootstrap",
        )
        .unwrap();
        fs::write(
            fixture.root.join("uninstall.exe"),
            b"changed after bootstrap",
        )
        .unwrap();
        fs::write(
            fixture.root.join(".avelin/manifest.json"),
            b"changed after bootstrap",
        )
        .unwrap();
        fs::write(fixture.root.join(RECEIPT_NAME), b"changed after bootstrap").unwrap();
        let directory = prepared.directory.path().to_owned();
        drop(prepared);
        for name in [
            "maintenance-coordinator.exe",
            "maintenance-worker.exe",
            "manifest.json",
        ] {
            fs::remove_file(directory.join(name)).unwrap();
        }
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn installed_maintenance_refuses_changed_runtime_payload_and_foreign_receipt() {
        let fixture = Fixture::new();
        assert!(prepare(&fixture.root, "S-1-5-21-200-200-200-1001").is_err());
        fs::write(
            fixture.root.join(".avelin/avelin-app.exe"),
            b"changed runtime",
        )
        .unwrap();
        assert!(prepare(&fixture.root, &fixture.receipt.user_sid).is_err());
        fs::write(fixture.root.join(".avelin/avelin-app.exe"), b"runtime").unwrap();
        fs::write(fixture.root.join("uninstall.exe"), b"changed worker").unwrap();
        assert!(prepare(&fixture.root, &fixture.receipt.user_sid).is_err());
    }
}
