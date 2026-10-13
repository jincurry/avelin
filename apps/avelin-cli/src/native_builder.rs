//! Assemble the native backend without a compiler, script or NSIS bridge.
use anyhow::{Context, Result, ensure};
use avelin_core::{Manifest, hash_file, native_payload::NativePayload, reject_reparse};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn build(
    config: &Path,
    output: &Path,
    runtime: Option<PathBuf>,
    worker: Option<PathBuf>,
) -> Result<()> {
    let (source, manifest) = crate::builder::load(config)?;
    let components = crate::components::resolve_native(runtime, worker)?;
    let report = build_resolved(&source, manifest, output, &components)?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn verify_hash(path: &Path, expected: &str, label: &str) -> Result<()> {
    ensure!(
        hash_file(path)? == expected,
        "{label} changed while building"
    );
    Ok(())
}

fn build_resolved(
    source: &Path,
    mut manifest: Manifest,
    output: &Path,
    components: &crate::components::NativeResolved,
) -> Result<serde_json::Value> {
    manifest.runtime_sha256 = Some(hash_file(&components.runtime)?);
    manifest.validate()?;
    let worker_hash = hash_file(&components.worker)?;
    ensure!(
        fs::metadata(&components.worker)?.len() > 0,
        "empty Rust worker"
    );
    let output = std::path::absolute(output)?;
    let backend = output.with_extension("backend.exe");
    let metadata = output.with_extension("manifest.json");
    let parent = output.parent().context("output parent")?;
    reject_reparse(parent)?;
    fs::create_dir_all(parent)?;
    reject_reparse(parent)?;
    for path in [&output, &backend, &metadata] {
        reject_reparse(path)?;
        ensure!(
            !path.try_exists()?,
            "build output already exists: {}",
            path.display()
        );
    }
    ensure!(
        output != backend && output != metadata && backend != metadata,
        "build output paths overlap"
    );
    let stage = parent.join(format!("avelin-build-{}", uuid::Uuid::new_v4().simple()));
    fs::create_dir(&stage)?;
    let staged_runtime = stage.join("runtime.exe");
    let staged_worker = stage.join("avelin-engine.exe");
    crate::builder::copy_new_file(&components.runtime, &staged_runtime)?;
    crate::builder::copy_new_file(&components.worker, &staged_worker)?;
    let runtime_hash = manifest.runtime_sha256.as_deref().unwrap();
    verify_hash(&staged_runtime, runtime_hash, "runtime")?;
    verify_hash(&staged_worker, &worker_hash, "Rust worker")?;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let manifest_hash = avelin_core::hash_bytes(&manifest_bytes);
    let staged_metadata = stage.join("manifest.json");
    crate::builder::write_new_file(&staged_metadata, &manifest_bytes)?;
    let payload = stage.join("payload.bin");
    avelin_core::native_payload::write_payload(source, &staged_runtime, &manifest, &payload)?;
    let payload_hash = hash_file(&payload)?;
    let staged_backend = stage.join("backend.exe");
    avelin_core::assemble(&staged_worker, &payload, &manifest, &staged_backend)?;
    verify_hash(&staged_worker, &worker_hash, "staged Rust worker")?;
    verify_hash(&payload, &payload_hash, "staged native payload")?;
    verify_hash(&staged_metadata, &manifest_hash, "staged manifest")?;
    let (embedded_payload, embedded_manifest) = avelin_core::embedded(&staged_backend)?
        .context("native backend has no embedded payload")?;
    ensure!(
        serde_json::to_vec(&embedded_manifest)? == serde_json::to_vec(&manifest)?,
        "native backend manifest changed"
    );
    NativePayload::parse(&embedded_payload, &manifest)?;
    drop(embedded_payload);
    let backend_hash = hash_file(&staged_backend)?;
    let staged_setup = stage.join("Setup.exe");
    avelin_core::assemble(&staged_runtime, &staged_backend, &manifest, &staged_setup)?;
    verify_hash(&staged_runtime, runtime_hash, "staged runtime")?;
    verify_hash(&staged_backend, &backend_hash, "staged native backend")?;
    let (embedded_backend, setup_manifest) =
        avelin_core::embedded(&staged_setup)?.context("native setup has no embedded backend")?;
    ensure!(
        avelin_core::hash_bytes(&embedded_backend) == backend_hash,
        "setup backend changed"
    );
    ensure!(
        serde_json::to_vec(&setup_manifest)? == serde_json::to_vec(&manifest)?,
        "setup manifest changed"
    );
    drop(embedded_backend);
    let setup_hash = hash_file(&staged_setup)?;
    // Publish only validated build products, using exclusive creates even when
    // another process creates an output after the diagnostic checks above.
    crate::builder::copy_new_file(&staged_backend, &backend)?;
    crate::builder::write_new_file(&metadata, &manifest_bytes)?;
    crate::builder::copy_new_file(&staged_setup, &output)?;
    verify_hash(&backend, &backend_hash, "published backend")?;
    verify_hash(&metadata, &manifest_hash, "published manifest")?;
    verify_hash(&output, &setup_hash, "published setup")?;
    Ok(serde_json::json!({
        "engine": "rust", "setup": output, "backend": backend, "manifest": metadata,
        "stage": stage, "signed": false, "files": manifest.files.len(),
        "runtime_path": components.runtime, "rust_worker_path": components.worker,
        "tool_version": env!("CARGO_PKG_VERSION"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use avelin_core::{FileResource, SCHEMA_VERSION, hash_bytes};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("avelin-native-build-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            fs::create_dir(root.join("payload")).unwrap();
            fs::write(root.join("payload/app.exe"), b"application bytes").unwrap();
            // Reader fixtures need no executable PE header and are never run.
            fs::write(root.join("runtime.exe"), b"runtime fixture bytes").unwrap();
            fs::write(root.join("worker.exe"), b"worker fixture bytes").unwrap();
            Self(root)
        }
        fn components(&self) -> crate::components::NativeResolved {
            crate::components::NativeResolved {
                runtime: self.0.join("runtime.exe"),
                worker: self.0.join("worker.exe"),
            }
        }
        fn manifest(&self) -> Manifest {
            Manifest {
                schema_version: SCHEMA_VERSION,
                app_id: "dev.avelin.native-build-test".into(),
                app_name: "Native Build".into(),
                app_version: "1.0.0".into(),
                entry: "app.exe".into(),
                directory_name: "NativeBuild".into(),
                start_menu_shortcut: false,
                desktop_shortcut: false,
                runtime_sha256: None,
                presentation: None,
                files: vec![FileResource {
                    path: "app.exe".into(),
                    size: 17,
                    sha256: hash_bytes(b"application bytes"),
                }],
            }
        }
        fn build(&self) -> Result<serde_json::Value> {
            build_resolved(
                &self.0.join("payload"),
                self.manifest(),
                &self.0.join("Setup.exe"),
                &self.components(),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // This UUID-named root is created exclusively by the fixture.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn builds_two_verified_layers_and_matching_manifest_without_nsis() {
        let fixture = Fixture::new();
        let report = fixture.build().unwrap();
        assert_eq!(report["engine"], "rust");
        assert!(report.get("nsis_path").is_none());
        let (backend, setup_manifest) = avelin_core::embedded(&fixture.0.join("Setup.exe"))
            .unwrap()
            .unwrap();
        assert_eq!(
            backend,
            fs::read(fixture.0.join("Setup.backend.exe")).unwrap()
        );
        let (bytes, backend_manifest) = avelin_core::embedded(&fixture.0.join("Setup.backend.exe"))
            .unwrap()
            .unwrap();
        let sidecar: Manifest =
            serde_json::from_slice(&fs::read(fixture.0.join("Setup.manifest.json")).unwrap())
                .unwrap();
        for candidate in [&setup_manifest, &backend_manifest] {
            assert_eq!(
                serde_json::to_value(candidate).unwrap(),
                serde_json::to_value(&sidecar).unwrap()
            );
        }
        let parsed = NativePayload::parse(&bytes, &sidecar).unwrap();
        assert_eq!(parsed.runtime, b"runtime fixture bytes");
        assert_eq!(parsed.files, [b"application bytes".as_slice()]);
    }

    #[test]
    fn refuses_each_existing_output_without_changing_it_or_creating_a_stage() {
        for name in ["Setup.exe", "Setup.backend.exe", "Setup.manifest.json"] {
            let fixture = Fixture::new();
            fs::write(fixture.0.join(name), b"preserve existing output").unwrap();
            assert!(fixture.build().is_err());
            assert_eq!(
                fs::read(fixture.0.join(name)).unwrap(),
                b"preserve existing output"
            );
            assert!(!fs::read_dir(&fixture.0).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("avelin-build-")
            }));
        }
    }

    #[test]
    fn changed_payload_never_produces_final_outputs() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("payload/app.exe"), b"different payload").unwrap();
        assert!(fixture.build().is_err());
        for name in ["Setup.exe", "Setup.backend.exe", "Setup.manifest.json"] {
            assert!(!fixture.0.join(name).exists());
        }
    }
}
