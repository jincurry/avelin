use anyhow::{Context, Result, ensure};
use avelin_core::{InstallPlan, Manifest, Operation, Receipt, ShortcutKind, ShortcutRecord};
use avelin_engine::EngineRequest;
use serde_json::json;
use std::{collections::BTreeMap, path::Path, time::Duration};

// Reuse the same pinned Windows handles and authenticated protocol as the UI.
#[allow(dead_code)]
#[path = "../../../apps/avelin-app/src/execution_files.rs"]
mod execution_files;
#[path = "worker_pipe.rs"]
mod pipe;
#[allow(dead_code)]
#[path = "../../../apps/avelin-app/src/transport.rs"]
mod transport;

pub fn entry() -> i32 {
    match std::panic::catch_unwind(run) {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => {
            eprintln!("AVELIN_ENGINE_START: {error:#}");
            3
        }
        Err(_) => {
            eprintln!("AVELIN_ENGINE_PANIC: execution was interrupted; retain task evidence");
            4
        }
    }
}

fn read_pinned(path: &Path, maximum: u64) -> Result<(execution_files::PinnedFile, Vec<u8>)> {
    let file = execution_files::PinnedFile::open(path)?;
    ensure!(
        std::fs::metadata(path)?.len() <= maximum,
        "task input exceeds its size limit"
    );
    let bytes = file.read_bytes()?;
    Ok((file, bytes))
}

fn run() -> Result<i32> {
    execution_files::initialize_dll_search()?;
    let Some(configuration) = std::env::var_os("AVELIN_SESSION_INI") else {
        return launch_installed_uninstaller();
    };
    let configuration = std::path::absolute(configuration)?;
    ensure!(
        configuration
            .file_name()
            .is_some_and(|name| name == "task.ini"),
        "unexpected task configuration name"
    );
    let session = execution_files::PinnedDirectory::open(
        configuration.parent().context("session directory")?,
    )?;
    let (_config_pin, ini_bytes) = read_pinned(&configuration, 128 * 1024)?;
    let ini = TaskIni::parse(&ini_bytes)?;
    let (_plan_pin, plan_bytes) =
        read_pinned(&session.path().join("install-plan.json"), 32 * 1024 * 1024)?;
    let plan: InstallPlan = serde_json::from_slice(&plan_bytes)?;
    ensure!(
        ini.required("task_id")? == plan.task_id,
        "task identity mismatch"
    );
    ensure!(
        same_path(Path::new(ini.required("install_dir")?), &plan.install_dir),
        "task destination mismatch"
    );
    ensure!(
        same_path(
            Path::new(ini.required("receipt_candidate")?),
            &session.path().join("receipt-candidate.json")
        ),
        "receipt candidate is outside the session"
    );
    ensure!(
        same_path(
            Path::new(ini.required("shortcut_evidence")?),
            &session.path().join("shortcuts.ini")
        ),
        "shortcut evidence is outside the session"
    );
    let (_receipt_pin, receipt_bytes) = read_pinned(
        &session.path().join("receipt-candidate.json"),
        32 * 1024 * 1024,
    )?;
    let receipt: Receipt = serde_json::from_slice(&receipt_bytes)?;
    let executable = std::env::current_exe()?;
    let _worker_pin = execution_files::PinnedFile::open(&executable)?;

    // Connect before hashing a potentially large payload, so the dedicated I/O
    // thread can keep the coordinator alive throughout package validation.
    let connection = transport::Connection::connect(
        ini.required("pipe_name")?,
        ini.required("token")?,
        &plan.task_id,
        Duration::from_secs(10),
    )?;
    let coordinator_pid = connection.peer_pid()?;
    let mut observer = pipe::Observer::new(connection)?;
    let result = (|| -> Result<avelin_engine::EngineOutcome> {
        verify_session(session.path(), &plan, coordinator_pid)?;
        let (bytes, manifest) =
            avelin_core::embedded(&executable)?.context("native worker has no package")?;
        plan.validate(&manifest)?;
        validate_candidate(&plan, &manifest, &receipt)?;
        let payload = avelin_core::native_payload::NativePayload::parse(&bytes, &manifest)?;
        let shortcuts = task_shortcuts(&ini, &plan, &manifest)?;
        let request = EngineRequest {
            manifest: &manifest,
            plan: &plan,
            receipt: &receipt,
            session_dir: session.path(),
            worker_exe: &executable,
            payload: Some(&payload),
            shortcuts,
            fault_after_files: ini.optional_count("fault_after_files")?,
            cancel_after_files: ini.optional_count("cancel_after_files")?,
            step_delay_ms: ini.number("step_delay_ms")?,
            fault_point: ini.optional("engine_fault"),
            pause_point: ini.optional("engine_pause"),
            recovery_session: ini.optional("recovery_session").map(Path::new),
            recovery_apply: ini.optional("recovery_apply") == Some("1"),
            repair_overwrite: ini.optional("repair_overwrite") == Some("1"),
        };
        avelin_engine::execute(request, &mut observer)
    })();
    // Only errors returned before managed mutations are confirmed failures.
    // Errors saving or delivering a completed transaction must remain Unknown.
    let outcome = result.unwrap_or_else(|error| avelin_engine::EngineOutcome {
        exit_code: 3,
        detail: format!("{error:#}"),
    });
    if !matches!(outcome.exit_code, 0 | 2 | 3 | 4) {
        let _ = observer.finish(4, "native engine returned an unsupported outcome");
        return Ok(4);
    }
    if let Err(error) = save_result(&session, outcome.exit_code, &outcome.detail) {
        let _ = observer.finish(
            4,
            &format!("cannot persist native engine result: {error:#}"),
        );
        return Ok(4);
    }
    if observer.finish(outcome.exit_code, &outcome.detail).is_err() {
        return Ok(4);
    }
    Ok(outcome.exit_code)
}

fn save_result(session: &execution_files::PinnedDirectory, code: i32, detail: &str) -> Result<()> {
    let detail: String = detail.chars().take(4096).collect();
    execution_files::create_file(
        session,
        "engine-result.json",
        &serde_json::to_vec_pretty(&json!({
            "schema_version":1,"engine":"rust","transaction_outcome_code":code,
            "scope":"transaction result before IPC delivery and process exit","detail":detail
        }))?,
    )?;
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    avelin_core::same_windows_name(
        &left.to_string_lossy().replace('/', "\\"),
        &right.to_string_lossy().replace('/', "\\"),
    )
}

/// Double-clicking the installed worker opens the same branded confirmation UI
/// as Windows Settings. The bootstrap exits before the resident runtime proceeds.
fn launch_installed_uninstaller() -> Result<i32> {
    use std::os::windows::process::CommandExt;
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        arguments.is_empty()
            || (arguments.len() == 1 && (arguments[0] == "--silent" || arguments[0] == "/S")),
        "the installed uninstaller accepts only --silent or /S"
    );
    let executable = std::env::current_exe()?;
    ensure!(
        executable
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| avelin_core::same_windows_name(name, "uninstall.exe")),
        "use avelin-cli build to package this internal worker"
    );
    let root = executable.parent().context("installed worker parent")?;
    avelin_core::install_root(root)?;
    let _directory = execution_files::PinnedDirectory::open(root)?;
    let worker = execution_files::PinnedFile::open(&executable)?;
    let (_payload, manifest) =
        avelin_core::embedded(worker.path())?.context("installed worker package missing")?;
    let (_receipt_pin, receipt_bytes) =
        read_pinned(&root.join(avelin_core::RECEIPT_NAME), 32 * 1024 * 1024)?;
    let receipt: Receipt = serde_json::from_slice(&receipt_bytes)?;
    let plan = InstallPlan::new(
        &manifest,
        Operation::Uninstall,
        uuid::Uuid::new_v4().to_string(),
        root.to_path_buf(),
        transport::current_user_sid()?,
    )?;
    validate_candidate(&plan, &manifest, &receipt)?;
    ensure!(
        receipt.uninstaller_sha256.as_deref() == Some(worker.sha256()?.as_str()),
        "installed uninstaller was changed"
    );
    let manifest_path = root.join(".avelin").join("manifest.json");
    let (_manifest_pin, bytes) = read_pinned(&manifest_path, 32 * 1024 * 1024)?;
    let installed: Manifest = serde_json::from_slice(&bytes)?;
    installed.validate()?;
    ensure!(
        avelin_core::hash_bytes(&serde_json::to_vec(&installed)?) == plan.manifest_sha256,
        "installed manifest was changed"
    );
    let runtime_path = root.join(".avelin").join("avelin-app.exe");
    let runtime = execution_files::PinnedFile::open(&runtime_path)?;
    ensure!(
        manifest.runtime_sha256.as_deref() == Some(runtime.sha256()?.as_str()),
        "installed runtime was changed"
    );
    std::process::Command::new(runtime.path())
        .arg(if arguments.is_empty() {
            "--gui"
        } else {
            "--silent"
        })
        .args(["--operation", "uninstall", "--backend"])
        .arg(worker.path())
        .arg("--manifest")
        .arg(manifest_path)
        .arg("--install-dir")
        .arg(root)
        .arg("--wait-for-pid")
        .arg(std::process::id().to_string())
        .creation_flags(0x08000000)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("start uninstall confirmation")?;
    Ok(0)
}

fn verify_session(session: &Path, plan: &InstallPlan, coordinator_pid: u32) -> Result<()> {
    use avelin_core::session::{SessionRecord, running_process};
    let (_pin, bytes) = read_pinned(&session.join("session.json"), 128 * 1024)?;
    let record: SessionRecord = serde_json::from_slice(&bytes)?;
    ensure!(
        record.schema_version == 1
            && record.task_id == plan.task_id
            && record.app_id == plan.app_id
            && record.operation == plan.operation.as_str()
            && record.manifest_sha256 == plan.manifest_sha256
            && same_path(&record.install_dir, &plan.install_dir),
        "session identity differs from the task plan"
    );
    ensure!(
        record.coordinator.pid == coordinator_pid && record.backend.pid == std::process::id(),
        "pipe peer or worker identity mismatch"
    );
    ensure!(
        running_process(coordinator_pid)?.as_ref() == Some(&record.coordinator)
            && running_process(std::process::id())?.as_ref() == Some(&record.backend),
        "task process identity is stale"
    );
    ensure!(
        plan.user_sid == transport::current_user_sid()?,
        "task belongs to another Windows user"
    );
    Ok(())
}

fn validate_candidate(plan: &InstallPlan, manifest: &Manifest, receipt: &Receipt) -> Result<()> {
    ensure!(
        receipt.schema_version == avelin_core::SCHEMA_VERSION
            && receipt.app_id == plan.app_id
            && receipt.app_version == plan.app_version
            && receipt.user_sid == plan.user_sid
            && receipt.manifest_sha256 == plan.manifest_sha256
            && same_path(Path::new(&receipt.install_dir), &plan.install_dir)
            && receipt.files == manifest.files,
        "receipt candidate differs from task intent"
    );
    avelin_core::safe_id(&receipt.installation_id)?;
    avelin_core::safe_id(&receipt.commit_id)?;
    if plan.operation != Operation::Uninstall {
        ensure!(
            receipt.task_id == plan.task_id,
            "candidate is from another task"
        );
    }
    Ok(())
}

fn task_shortcuts(
    ini: &TaskIni,
    plan: &InstallPlan,
    manifest: &Manifest,
) -> Result<Vec<ShortcutRecord>> {
    let target = plan.install_dir.join(&manifest.entry);
    ensure!(
        same_path(Path::new(ini.required("shortcut_target")?), &target)
            && same_path(
                Path::new(ini.required("shortcut_working_dir")?),
                &plan.install_dir
            ),
        "shortcut destination does not match the entry"
    );
    let mut shortcuts = Vec::new();
    for (kind, field, selected) in [
        (ShortcutKind::Desktop, "desktop", plan.desktop_shortcut),
        (
            ShortcutKind::StartMenu,
            "start_menu",
            plan.start_menu_shortcut,
        ),
    ] {
        let path = ini.optional(&format!("{field}_shortcut_path"));
        ensure!(
            path.is_some() == selected,
            "shortcut selection differs from plan"
        );
        if let Some(path) = path {
            shortcuts.push(ShortcutRecord {
                kind,
                path: path.into(),
                target: target.to_string_lossy().into_owned(),
                sha256: ini
                    .optional(&format!("{field}_shortcut_sha256"))
                    .unwrap_or_default()
                    .into(),
            });
        }
    }
    Ok(shortcuts)
}

struct TaskIni(BTreeMap<String, String>);
impl TaskIni {
    fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.starts_with(&[0xff, 0xfe]) && bytes.len().is_multiple_of(2),
            "task INI must be UTF-16LE"
        );
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|v| u16::from_le_bytes([v[0], v[1]]))
            .collect();
        let text = String::from_utf16(&units)?;
        ensure!(!text.contains('\0'), "NUL in task INI");
        let mut values = BTreeMap::new();
        let mut section = false;
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if line == "[task]" {
                ensure!(!section, "duplicate task section");
                section = true;
                continue;
            }
            ensure!(
                section && !line.starts_with('['),
                "unsupported task INI section"
            );
            let (key, value) = line
                .split_once('=')
                .context("task INI assignment missing")?;
            ensure!(
                !key.is_empty()
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit()),
                "invalid task INI key"
            );
            ensure!(
                values.insert(key.into(), value.into()).is_none(),
                "duplicate task INI key"
            );
        }
        ensure!(section, "task INI section missing");
        Ok(Self(values))
    }
    fn optional(&self, key: &str) -> Option<&str> {
        self.0
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }
    fn required(&self, key: &str) -> Result<&str> {
        self.optional(key)
            .with_context(|| format!("task field missing: {key}"))
    }
    fn number(&self, key: &str) -> Result<u64> {
        self.required(key)?
            .parse()
            .with_context(|| format!("task field is not a count: {key}"))
    }
    fn optional_count(&self, key: &str) -> Result<Option<u64>> {
        let n = self.number(key)?;
        Ok((n != u64::MAX).then_some(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ini(text: &str) -> Vec<u8> {
        [
            vec![0xff, 0xfe],
            text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        ]
        .concat()
    }
    #[test]
    fn task_ini_keeps_unicode_and_equals_in_values() {
        let task=TaskIni::parse(&ini("[task]\r\ninstall_dir=C:\\Apps\\笔记 = beta\r\nfault_after_files=18446744073709551615\r\n")).unwrap();
        assert_eq!(
            task.required("install_dir").unwrap(),
            "C:\\Apps\\笔记 = beta"
        );
        assert_eq!(task.optional_count("fault_after_files").unwrap(), None);
    }
    #[test]
    fn task_ini_rejects_duplicate_redirects_and_malformed_encoding() {
        for value in [
            "[task]\npath=a\npath=b\n",
            "[task]\n[other]\npath=b\n",
            "[task]\n[task]\n",
            "path=a\n",
            "[task]\npath=a\0b\n",
        ] {
            assert!(TaskIni::parse(&ini(value)).is_err());
        }
        assert!(TaskIni::parse(b"[task]\n").is_err());
        assert!(TaskIni::parse(&[0xff, 0xfe, 0x61]).is_err());
    }
}
