//! Resolve build tools without executing them. Explicit choices never fall back silently.
use anyhow::{Context, Result, bail, ensure};
use avelin_core::reject_reparse;
use std::path::{Path, PathBuf};

pub struct Resolved {
    pub runtime: PathBuf,
    pub worker: PathBuf,
}

fn resolve_with(bin: &Path, runtime: Option<PathBuf>, worker: Option<PathBuf>) -> Result<Resolved> {
    Ok(Resolved {
        runtime: select(
            runtime,
            vec![bin.join("avelin-app.exe")],
            "Avelin runtime",
            "keep avelin-app.exe next to avelin-cli.exe, or pass --runtime <avelin-app.exe>; repository users can build it with cargo build -p avelin-app --features gui",
        )?,
        worker: select(
            worker,
            vec![bin.join("avelin-engine.exe")],
            "Rust engine worker",
            "keep avelin-engine.exe next to avelin-cli.exe, or pass --rust-worker <avelin-engine.exe>; repository users can build it with cargo build -p avelin-engine",
        )?,
    })
}

pub fn resolve(runtime: Option<PathBuf>, worker: Option<PathBuf>) -> Result<Resolved> {
    let own = std::env::current_exe().context("locate avelin-cli executable")?;
    resolve_with(
        own.parent().context("CLI executable has no parent")?,
        runtime,
        worker,
    )
}

fn checked_file(path: &Path, component: &str, hint: &str) -> Result<PathBuf> {
    let absolute =
        std::path::absolute(path).with_context(|| format!("resolve {component} path"))?;
    reject_reparse(&absolute).with_context(|| format!("unsafe {component} path; {hint}"))?;
    ensure!(
        absolute.is_file(),
        "{component} not found at {}; {hint}",
        absolute.display()
    );
    Ok(absolute)
}

fn select(
    explicit: Option<PathBuf>,
    candidates: Vec<PathBuf>,
    component: &str,
    hint: &str,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return checked_file(&path, component, hint);
    }
    for path in &candidates {
        if path
            .try_exists()
            .with_context(|| format!("inspect {component} at {}", path.display()))?
        {
            return checked_file(path, component, hint);
        }
    }
    bail!(
        "{component} not found. Searched: {}. {hint}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("avelin-m4-components-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn file(&self, path: &str) -> PathBuf {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"resolution fixture; never executed").unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // The fixture owns exactly this generated tree; it cannot contain installer data.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn colocated_components_resolve_and_missing_explicit_worker_is_rejected() {
        let fixture = Fixture::new();
        let runtime = fixture.file("kit/avelin-app.exe");
        let worker = fixture.file("kit/avelin-engine.exe");
        let found = resolve_with(&fixture.0.join("kit"), None, None).unwrap();
        assert_eq!(found.runtime, runtime);
        assert_eq!(found.worker, worker);
        let error = resolve_with(
            &fixture.0.join("kit"),
            None,
            Some(fixture.0.join("missing-worker.exe")),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("--rust-worker") && error.contains("missing-worker.exe"));
    }

    #[test]
    fn explicit_runtime_and_worker_are_used_without_fallback() {
        let fixture = Fixture::new();
        fixture.file("kit/avelin-app.exe");
        fixture.file("kit/avelin-engine.exe");
        let runtime = fixture.file("selected/runtime.exe");
        let worker = fixture.file("selected/worker.exe");
        let found = resolve_with(
            &fixture.0.join("kit"),
            Some(runtime.clone()),
            Some(worker.clone()),
        )
        .unwrap();
        assert_eq!(found.runtime, runtime);
        assert_eq!(found.worker, worker);
        let error = resolve_with(
            &fixture.0.join("kit"),
            Some(fixture.0.join("missing-runtime.exe")),
            None,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("--runtime") && error.contains("missing-runtime.exe"));
    }
}
