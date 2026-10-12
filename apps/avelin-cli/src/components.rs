//! Resolve build tools without executing them. Explicit choices never fall back silently.
use anyhow::{Context, Result, bail, ensure};
use avelin_core::reject_reparse;
use std::path::{Path, PathBuf};

pub struct Resolved {
    pub runtime: PathBuf,
    pub bridge: PathBuf,
    pub nsis: PathBuf,
}

struct SearchPaths {
    bin: PathBuf,
    workspace: PathBuf,
    environment_nsis: Option<PathBuf>,
    system_nsis: Vec<PathBuf>,
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

fn resolve_with(
    paths: SearchPaths,
    runtime: Option<PathBuf>,
    bridge: Option<PathBuf>,
    nsis: Option<PathBuf>,
) -> Result<Resolved> {
    let runtime = select(
        runtime,
        vec![paths.bin.join("avelin-app.exe")],
        "Avelin runtime",
        "keep avelin-app.exe next to avelin-cli.exe, or pass --runtime <avelin-app.exe>; repository users can build it with cargo build -p avelin-app --features gui",
    )?;
    let profile = if paths.bin.file_name().is_some_and(|name| name == "release") {
        "release"
    } else {
        "debug"
    };
    let other_profile = if profile == "release" {
        "debug"
    } else {
        "release"
    };
    let bridge = select(
        bridge,
        vec![
            paths.bin.join("avelin_nsis_bridge.dll"),
            paths.workspace.join(format!(
                "target/i686-pc-windows-msvc/{profile}/avelin_nsis_bridge.dll"
            )),
            paths.workspace.join(format!(
                "target/i686-pc-windows-msvc/{other_profile}/avelin_nsis_bridge.dll"
            )),
        ],
        "x86 NSIS bridge",
        "keep avelin_nsis_bridge.dll next to avelin-cli.exe, or pass --bridge <x86 DLL>; repository users can build it with cargo build -p avelin-nsis-bridge --target i686-pc-windows-msvc",
    )?;
    let mut nsis_candidates = vec![paths.bin.join("tools/nsis/makensis.exe")];
    // Also accept the existing bin/current and bin layouts without searching arbitrary ancestors.
    if paths.bin.file_name().is_some_and(|name| name == "bin") {
        if let Some(root) = paths.bin.parent() {
            nsis_candidates.push(root.join("tools/nsis/makensis.exe"));
        }
    } else if paths
        .bin
        .file_name()
        .is_some_and(|name| name == "current" || name == "previous")
    {
        if let Some(bin) = paths
            .bin
            .parent()
            .filter(|p| p.file_name().is_some_and(|name| name == "bin"))
        {
            if let Some(root) = bin.parent() {
                nsis_candidates.push(root.join("tools/nsis/makensis.exe"));
            }
        }
    }
    nsis_candidates.extend(paths.system_nsis);
    let nsis = select(
        nsis.or(paths.environment_nsis),
        nsis_candidates,
        "NSIS 3 Unicode compiler",
        "pass --nsis <makensis.exe>, set NSIS_MAKENSIS, or restore the complete tools/nsis directory beside the CLI (including Bin, Include, Stubs and Plugins); an invalid explicit path/environment override is not silently replaced",
    )?;
    Ok(Resolved {
        runtime,
        bridge,
        nsis,
    })
}

pub fn resolve(
    runtime: Option<PathBuf>,
    bridge: Option<PathBuf>,
    nsis: Option<PathBuf>,
) -> Result<Resolved> {
    let own = std::env::current_exe().context("locate avelin-cli executable")?;
    let bin = own
        .parent()
        .context("CLI executable has no parent")?
        .to_path_buf();
    let mut system_nsis = Vec::new();
    for name in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(root) = std::env::var_os(name) {
            system_nsis.push(PathBuf::from(root).join("NSIS/makensis.exe"));
        }
    }
    system_nsis.push(PathBuf::from(r"C:\Program Files (x86)\NSIS\makensis.exe"));
    resolve_with(
        SearchPaths {
            bin,
            workspace: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
            environment_nsis: std::env::var_os("NSIS_MAKENSIS").map(PathBuf::from),
            system_nsis,
        },
        runtime,
        bridge,
        nsis,
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
        fn search(&self) -> SearchPaths {
            SearchPaths {
                bin: self.0.join("kit"),
                workspace: self.0.join("repo"),
                environment_nsis: None,
                system_nsis: vec![],
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // The fixture owns exactly this generated tree; it cannot contain installer data.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn relocated_kit_components_take_precedence_over_repository_and_system() {
        let fixture = Fixture::new();
        let runtime = fixture.file("kit/avelin-app.exe");
        let bridge = fixture.file("kit/avelin_nsis_bridge.dll");
        let nsis = fixture.file("kit/tools/nsis/makensis.exe");
        fixture.file("repo/target/i686-pc-windows-msvc/debug/avelin_nsis_bridge.dll");
        let mut paths = fixture.search();
        paths.system_nsis.push(fixture.file("system/makensis.exe"));
        let found = resolve_with(paths, None, None, None).unwrap();
        assert_eq!(found.runtime, runtime);
        assert_eq!(found.bridge, bridge);
        assert_eq!(found.nsis, nsis);
    }

    #[test]
    fn explicit_paths_and_environment_override_never_silently_fall_back() {
        let fixture = Fixture::new();
        fixture.file("kit/avelin-app.exe");
        fixture.file("kit/avelin_nsis_bridge.dll");
        fixture.file("kit/tools/nsis/makensis.exe");
        let runtime = fixture.file("explicit/runtime.exe");
        let bridge = fixture.file("explicit/bridge.dll");
        let nsis = fixture.file("explicit/compiler.exe");
        let mut paths = fixture.search();
        paths.environment_nsis = Some(fixture.0.join("missing-environment.exe"));
        let found = resolve_with(
            paths,
            Some(runtime.clone()),
            Some(bridge.clone()),
            Some(nsis.clone()),
        )
        .unwrap();
        assert_eq!(found.runtime, runtime);
        assert_eq!(found.bridge, bridge);
        assert_eq!(found.nsis, nsis);
        let mut paths = fixture.search();
        paths.environment_nsis = Some(fixture.0.join("missing-environment.exe"));
        let error = resolve_with(paths, None, None, None)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("missing-environment.exe") && error.contains("NSIS_MAKENSIS"));
        let error = resolve_with(
            fixture.search(),
            Some(fixture.0.join("missing-runtime.exe")),
            None,
            None,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("--runtime"));
    }

    #[test]
    fn repository_bridge_fallback_and_missing_dependency_hint_are_preserved() {
        let fixture = Fixture::new();
        fixture.file("kit/avelin-app.exe");
        let bridge = fixture.file("repo/target/i686-pc-windows-msvc/debug/avelin_nsis_bridge.dll");
        fixture.file("kit/tools/nsis/makensis.exe");
        assert_eq!(
            resolve_with(fixture.search(), None, None, None)
                .unwrap()
                .bridge,
            bridge
        );
        fs::remove_file(bridge).unwrap();
        let error = resolve_with(fixture.search(), None, None, None)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("--bridge") && error.contains("i686-pc-windows-msvc"));
    }
}
