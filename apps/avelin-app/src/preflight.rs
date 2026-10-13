//! Advisory, read-only usage checks before allocating an installation transaction.
use crate::{
    coordinator::{Args, Operation},
    execution_files::PinnedFile,
    resources,
};
use anyhow::{Context, Result, ensure};
use avelin_core::{
    Manifest, RECEIPT_NAME,
    resource_usage::{self, UsageReport},
};
use std::path::PathBuf;
use std::sync::Arc;

pub fn inspect_with_backend(
    args: &Args,
    manifest: &Manifest,
    backend: Arc<PinnedFile>,
) -> Result<(Arc<PinnedFile>, UsageReport)> {
    let installed_backend = args
        .install_dir
        .as_ref()
        .is_some_and(|root| resources::same_path(&root.join("uninstall.exe"), backend.path()));
    if !installed_backend || args.operation == Operation::Recover {
        return Ok((backend, inspect(args, manifest)?));
    }
    // Our read pin itself denies DELETE access. Release it only for this
    // read-only observation, then reopen and authenticate the exact bytes again
    // before a worker can use them. No managed resource is omitted from scanning.
    let expected = backend.sha256()?;
    let path = backend.path().to_owned();
    let owned = Arc::try_unwrap(backend)
        .map_err(|_| anyhow::anyhow!("backend has unexpected retained handles before preflight"))?;
    drop(owned);
    let inspection = inspect(args, manifest);
    let backend = Arc::new(PinnedFile::open(&path)?);
    ensure!(
        backend.sha256()? == expected,
        "backend changed during resource inspection; reopen the original installer"
    );
    Ok((backend, inspection?))
}

fn inspect(args: &Args, manifest: &Manifest) -> Result<UsageReport> {
    // Interrupted transactions have their own evidence and resource ownership
    // rules. Do not interpret their partial receipt as a committed installation.
    if args.operation == Operation::Recover {
        return Ok(UsageReport::default());
    }
    let target = args.install_dir.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default())
            .join(&manifest.directory_name)
    });
    avelin_core::install_root(&target)?;
    let sid = crate::transport::current_user_sid()?;
    let selection = crate::component_selection::resolve(
        manifest,
        &target,
        &sid,
        args.operation,
        args.components.as_deref(),
        false,
    )?;
    let selected = avelin_core::resolve_selection(manifest, &selection.selected)?;
    let mut paths: Vec<PathBuf> = selected
        .files
        .iter()
        .map(|file| target.join(&file.path))
        .collect();
    if args.operation != Operation::Install {
        let installed = if args.operation == Operation::Upgrade {
            let input = PinnedFile::open(&target.join(".avelin/manifest.json"))?;
            let prior: Manifest = serde_json::from_slice(&input.read_bytes()?)?;
            prior.validate()?;
            prior
        } else {
            manifest.clone()
        };
        let receipt = resources::read_receipt(&installed, &target, &sid)?;
        paths.extend(receipt.files.iter().map(|file| target.join(&file.path)));
        paths.extend(
            receipt
                .shortcuts
                .iter()
                .map(|link| PathBuf::from(&link.path)),
        );
        paths.extend([
            target.join(".avelin/avelin-app.exe"),
            target.join(".avelin/manifest.json"),
            target.join(RECEIPT_NAME),
            target.join("uninstall.exe"),
        ]);
    }
    if matches!(args.operation, Operation::Install | Operation::Upgrade) {
        for (kind, enabled) in [
            (
                avelin_core::ShortcutKind::Desktop,
                args.desktop_shortcut.unwrap_or(manifest.desktop_shortcut),
            ),
            (
                avelin_core::ShortcutKind::StartMenu,
                args.start_menu_shortcut
                    .unwrap_or(manifest.start_menu_shortcut),
            ),
        ] {
            if enabled {
                paths.push(resources::shortcut_path(&manifest.app_id, kind)?);
            }
        }
    }
    for path in &paths {
        avelin_core::reject_reparse(path)?;
    }
    let current = avelin_core::session::running_process(std::process::id())?
        .context("could not identify the preflight coordinator")?;
    resource_usage::inspect(&paths, &[current])
}

pub fn detail(report: &UsageReport, english: bool) -> String {
    let mut lines = vec![if english {
        "AVELIN_RESOURCES_IN_USE\nSave your work, close the listed applications, then check again."
            .into()
    } else {
        "AVELIN_RESOURCES_IN_USE\n请保存工作并关闭下列应用，再重新检查。".into()
    }];
    for process in report.processes.iter().take(12) {
        lines.push(format!(
            "{} (PID {}){}",
            process.name,
            process.pid,
            process
                .executable
                .as_ref()
                .map(|path| format!(" — {path}"))
                .unwrap_or_default()
        ));
    }
    for file in report.locked_files.iter().take(12) {
        lines.push(format!(
            "{}: {} (Windows {})",
            if english {
                "File in use"
            } else {
                "文件被占用"
            },
            file.path,
            file.win32_error
        ));
    }
    for issue in report.issues.iter().take(8) {
        lines.push(format!(
            "{}: {} — {}",
            if english {
                "Check unavailable"
            } else {
                "无法完成检查"
            },
            issue.scope,
            issue.message
        ));
    }
    if report.reboot_reasons != 0 {
        lines.push(format!(
            "{}: {}",
            if english {
                "Windows restart reasons"
            } else {
                "Windows 报告的重启原因"
            },
            report.reboot_reasons
        ));
    }
    if report.processes.len() > 12 || report.locked_files.len() > 12 || report.issues.len() > 8 {
        lines.push(
            if english {
                "Additional resources also need checking."
            } else {
                "还有其他资源需要检查。"
            }
            .into(),
        );
    }
    lines.join("\n")
}

pub fn save_blocked(args: &Args, report: &UsageReport, english: bool) -> Result<i32> {
    use std::io::Write;
    let directory = crate::execution_files::create_private_session(
        &std::env::temp_dir().join("AvelinM0"),
        "preflight",
    )?;
    let path = std::path::absolute(
        args.report
            .clone()
            .unwrap_or_else(|| directory.path().join("report.json")),
    )?;
    avelin_core::reject_reparse(&path)?;
    let value = serde_json::json!({"schema_version":1,"result":"NotStarted","coordinator_exit_code":3,
        "operation":avelin_core::Operation::from(args.operation),"install_dir":args.install_dir,
        "diagnostic":{"code":"AVELIN_RESOURCES_IN_USE"},"message":detail(report,english),
        "preflight":report,"report_path":path,"worker_started":false});
    // A blocked attempt never overwrites an earlier task's evidence.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(&serde_json::to_vec_pretty(&value)?)?;
    file.sync_all()?;
    println!("{value}");
    Ok(3)
}
