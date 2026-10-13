use crate::execution_files::{self, PinnedDirectory, PinnedFile};
use crate::resources;
use crate::transport;
#[path = "backend_events.rs"]
mod backend_events;
#[path = "task_diagnostics.rs"]
mod task_diagnostics;
use anyhow::{Context, Result, ensure};
use avelin_core::{
    InstallPlan, Manifest, RECEIPT_NAME, Receipt, SCHEMA_VERSION, ShortcutKind, ShortcutRecord,
    hash_bytes, hash_file, install_root, reject_reparse,
};
use backend_events::{
    BackendEvent, BackendEvents, Progress, ProgressPhase, TaskResult, observation_complete,
};
use clap::{Parser, ValueEnum};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum Operation {
    Install,
    Upgrade,
    Modify,
    Uninstall,
    Repair,
    Recover,
}
impl From<Operation> for avelin_core::Operation {
    fn from(operation: Operation) -> Self {
        match operation {
            Operation::Install => Self::Install,
            Operation::Upgrade => Self::Upgrade,
            Operation::Modify => Self::Modify,
            Operation::Uninstall => Self::Uninstall,
            Operation::Repair => Self::Repair,
            Operation::Recover => Self::Recover,
        }
    }
}
#[derive(Debug, Clone, Parser)]
#[command(
    version,
    about = "Avelin runtime: current-user installation, upgrade, repair, recovery and uninstall"
)]
pub struct Args {
    #[arg(long)]
    pub backend: Option<PathBuf>,
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    #[arg(long)]
    pub install_dir: Option<PathBuf>,
    #[arg(long)]
    pub report: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "install")]
    pub operation: Operation,
    /// Open maintenance using the verified, installed offline package.
    #[arg(long)]
    pub maintenance: bool,
    /// Complete final component IDs, for example core,tools. Defaults apply only to first install.
    #[arg(long)]
    pub components: Option<String>,
    #[arg(skip)]
    component_context: Option<crate::component_selection::ComponentContext>,
    /// Original interrupted task session; checked against this package and current user.
    #[arg(long)]
    pub recovery_session: Option<PathBuf>,
    /// Execute the checked recovery. Silent recovery otherwise performs a read-only check.
    #[arg(long)]
    pub recovery_apply: bool,
    /// Explicitly restore changed managed files from this package after backing them up.
    #[arg(long)]
    pub repair_overwrite: bool,
    #[arg(skip)]
    maintenance_details: Vec<crate::maintenance::Finding>,
    #[arg(long)]
    pub cancel_after_files: Option<u64>,
    /// Experiment timer measured from backend launch, with no artificial file barrier.
    #[arg(long, conflicts_with = "cancel_after_files")]
    pub cancel_after_ms: Option<u64>,
    #[arg(long)]
    pub fault_after_files: Option<u64>,
    /// Native-engine acceptance instrumentation; never shown as a product option.
    #[arg(long, hide = true, value_parser = ["after-staging", "before-registration", "after-registration", "before-commit", "after-commit", "rollback"])]
    pub engine_fault: Option<String>,
    /// Bounded pause for real-process interruption acceptance only.
    #[arg(long, hide = true, value_parser = ["prepared", "after-first-publish", "after-first-remove", "after-commit", "recovery-after-first-mutation"])]
    pub engine_pause: Option<String>,
    #[arg(long, default_value = "0")]
    pub step_delay_ms: u32,
    /// Bounded observation budget. Expiry reports Unknown and never kills the backend.
    #[arg(long, default_value = "600", value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub observation_timeout_secs: u64,
    /// Simulated UI only. An optional --manifest previews its validated presentation.
    #[arg(long)]
    pub ui_demo: bool,
    #[arg(long)]
    pub gui: bool,
    /// Run without opening a graphical window, using package defaults unless overridden.
    #[arg(long, conflicts_with_all = ["gui", "ui_demo"])]
    pub silent: bool,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub desktop_shortcut: Option<bool>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub start_menu_shortcut: Option<bool>,
    #[arg(long)]
    pub english: bool,
    #[arg(long, hide = true)]
    pub wait_for_pid: Option<u32>,
}

struct AppLock {
    _file: Option<File>,
    path: PathBuf,
    retain: bool,
}
impl AppLock {
    fn acquire(id: &str, sid: &str) -> Result<Self> {
        let base =
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is missing")?)
                .join("Avelin/M0/locks");
        reject_reparse(&base)?;
        fs::create_dir_all(&base)?;
        let path = base.join(format!("{id}-{}.lock", &hash_bytes(sid.as_bytes())[..16]));
        let mut file=OpenOptions::new().write(true).create_new(true).open(&path)
            .context("another task or an unresolved interrupted task holds the product lock; inspect logs before recovery")?;
        writeln!(file, "{}", std::process::id())?;
        Ok(Self {
            _file: Some(file),
            path,
            retain: false,
        })
    }

    fn seal(&mut self, session: &Path, target: &Path) -> Result<()> {
        let mut writer = self
            ._file
            .take()
            .context("product lock is already sealed")?;
        writeln!(
            writer,
            "session={}\ninstall_dir={}",
            session.display(),
            target.display()
        )?;
        writer.flush()?;
        writer.sync_all()?;
        // A write-capable handle conflicts with the worker's read-only hash
        // pin under Windows' symmetric sharing rules. Keep only a read handle
        // after recording the immutable discovery hints.
        drop(writer);
        reject_reparse(&self.path)?;
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ: deny writes and deletion.
            .open(&self.path)?;
        self._file = Some(reader);
        ensure!(
            fs::read(&self.path)?
                == format!(
                    "{}\nsession={}\ninstall_dir={}\n",
                    std::process::id(),
                    session.display(),
                    target.display()
                )
                .as_bytes(),
            "product lock changed while sealing its discovery hints"
        );
        Ok(())
    }
}
impl Drop for AppLock {
    fn drop(&mut self) {
        // The read pin denies deletion, including our own cleanup.
        drop(self._file.take());
        if !self.retain {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Once the Rust worker starts, errors cannot imply that no filesystem changes occurred.
#[derive(Debug)]
struct UnconfirmedExecution;
impl std::fmt::Display for UnconfirmedExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "backend execution result is Unknown; an error does not confirm cleanup"
        )
    }
}
impl std::error::Error for UnconfirmedExecution {}

/// Allocated task locations are diagnostic evidence, not proof of execution or
/// report persistence. Only `saved_report` records a successful report write.
#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
pub struct TaskLocations {
    pub task_id: String,
    pub session_dir: PathBuf,
    pub report_path: PathBuf,
    /// Set only after a task report was written successfully; it may be the
    /// Unknown fallback rather than the originally allocated report path.
    pub saved_report: Option<PathBuf>,
}

/// The UI outcome is independent of the coordinator's task result.
/// When the dispatcher joined successfully, `None` means no worker started.
/// A failed join provides no returned task code or proof of worker completion.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GuiFailureContext {
    pub task_exit_code: Option<i32>,
    pub dispatcher_join_failed: bool,
    pub ui_failure_kind: String,
    /// The caller's requested report path, not proof that a report was written.
    pub requested_report: Option<PathBuf>,
    pub task_locations: Option<TaskLocations>,
}
impl std::fmt::Display for GuiFailureContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.dispatcher_join_failed {
            return write!(
                f,
                "GUI {}; dispatcher join failed; coordinator task completion and exit code are unknown",
                self.ui_failure_kind
            );
        }
        match self.task_exit_code {
            Some(code) => write!(
                f,
                "GUI {}; coordinator task returned exit code {code}; the UI failure does not replace that task result",
                self.ui_failure_kind
            ),
            None => write!(
                f,
                "GUI {}; no coordinator task was started",
                self.ui_failure_kind
            ),
        }
    }
}
impl std::error::Error for GuiFailureContext {}

pub fn error_exit_code(error: &anyhow::Error) -> i32 {
    if error.is::<UnconfirmedExecution>() {
        4
    } else {
        3
    }
}

struct BackendGuard {
    child: std::process::Child,
    lock: AppLock,
    task: String,
    target: PathBuf,
    session: PathBuf,
    report_path: PathBuf,
    hooks: Option<UiHooks>,
    completed: bool,
    english: bool,
    fault_after_files: Option<u64>,
    last_progress: Option<Progress>,
    last_work_progress: Option<Progress>,
}
impl Drop for BackendGuard {
    fn drop(&mut self) {
        // Failure to query termination is not evidence that it is safe to retry.
        let observed_exit = self.child.try_wait().ok().flatten();
        let still_running = observed_exit.is_none();
        let exit_code = observed_exit.map(|status| status.code().unwrap_or(-1));
        self.lock.retain = still_running;
        // The retained lock is immutable evidence bound by the native recovery
        // record. Process/outcome updates belong in the task report, not here.
        if !self.completed {
            let message = if still_running {
                "coordinator ended before a verified outcome; backend may still be running; product lock retained; inspect the session before retrying"
            } else {
                "coordinator ended before a verified outcome; backend has exited but its changes and cleanup are unconfirmed"
            };
            let diagnostic = task_diagnostics::diagnostic(
                "Unknown",
                message,
                self.last_progress,
                self.last_work_progress,
                exit_code,
                self.fault_after_files,
            );
            let mut report = json!({"task_id":self.task,"result":"Unknown","exit_code":exit_code,"coordinator_exit_code":4,"message":message,
                "backend_pid":self.child.id(),"product_lock_retained":still_running,"install_dir":self.target,
                "session_dir":self.session,"report_path":self.report_path,"diagnostic":diagnostic,"signed":false});
            if let Some(parent) = self.report_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let bytes = serde_json::to_vec_pretty(&report).unwrap_or_default();
            let saved_report = match fs::write(&self.report_path, &bytes) {
                Ok(()) => Some(self.report_path.clone()),
                Err(error) => {
                    let fallback = self.session.join("unknown-result.json");
                    log::error!(
                        target: "avelin::coordinator",
                        "could not write Unknown report to {}: {error}; trying {}",
                        self.report_path.display(), fallback.display()
                    );
                    report["report_path"] = json!(fallback);
                    let fallback_bytes = serde_json::to_vec_pretty(&report).unwrap_or_default();
                    match fs::write(&fallback, &fallback_bytes) {
                        Ok(()) => Some(fallback),
                        Err(error) => {
                            log::error!(
                                target: "avelin::coordinator",
                                "could not write fallback Unknown report to {}: {error}", fallback.display()
                            );
                            None
                        }
                    }
                }
            };
            if let Some(path) = &saved_report {
                record_saved_report(&self.hooks, &path);
            }
            log::error!(
                target: "avelin::coordinator",
                "{message}; task {}; backend PID {}; session {}; requested report {}",
                self.task, self.child.id(), self.session.display(), self.report_path.display()
            );
            let _ = writeln!(
                std::io::stderr(),
                "Avelin: {message}; backend PID {}",
                self.child.id()
            );
            let detail = task_diagnostics::terminal_detail(
                message,
                diagnostic.as_ref(),
                saved_report.as_deref(),
                self.english,
            );
            notify_finished(&self.hooks, TaskResult::Unknown, &detail);
        }
        // Dropping Child closes handles only. Never force-kill a writing backend.
    }
}

pub fn requests_gui(args: &Args) -> bool {
    !args.silent
        && (args.gui
            || args.maintenance
            || args.ui_demo
            || (cfg!(feature = "gui")
                && (matches!(
                    args.operation,
                    Operation::Uninstall
                        | Operation::Repair
                        | Operation::Recover
                        | Operation::Modify
                ) || args.install_dir.is_none())))
}

fn validate_launch(args: &Args) -> Result<()> {
    if args.maintenance {
        ensure!(
            args.install_dir.is_some()
                && args.backend.is_none()
                && args.manifest.is_none()
                && !args.ui_demo
                && matches!(
                    args.operation,
                    Operation::Install | Operation::Repair | Operation::Modify | Operation::Recover
                ),
            "--maintenance requires --install-dir and uses only the installed package; install/repair/modify/recover modes only"
        );
        ensure!(
            !args.silent || args.operation != Operation::Install,
            "silent --maintenance requires an explicit repair, modify or recover operation"
        );
        ensure!(
            args.engine_fault.is_none()
                && args.engine_pause.is_none()
                && args.cancel_after_files.is_none()
                && args.cancel_after_ms.is_none()
                && args.fault_after_files.is_none()
                && args.desktop_shortcut.is_none()
                && args.start_menu_shortcut.is_none(),
            "installed maintenance does not accept fault injection or installation option overrides"
        );
    }
    ensure!(
        args.components.is_none()
            || matches!(
                args.operation,
                Operation::Install | Operation::Upgrade | Operation::Modify
            ),
        "--components is only valid for install, upgrade and modify"
    );
    if let Some(value) = &args.components {
        crate::component_selection::parse_explicit(value)?;
    }
    ensure!(
        args.operation != Operation::Modify || requests_gui(args) || args.components.is_some(),
        "silent modify requires an explicit --components final selection"
    );
    ensure!(
        args.operation == Operation::Recover
            || (args.recovery_session.is_none() && !args.recovery_apply),
        "recovery options require --operation recover"
    );
    ensure!(
        args.operation == Operation::Repair || !args.repair_overwrite,
        "--repair-overwrite requires --operation repair"
    );
    if matches!(
        args.operation,
        Operation::Repair | Operation::Recover | Operation::Modify
    ) {
        ensure!(
            !args.ui_demo
                && args.cancel_after_files.is_none()
                && args.cancel_after_ms.is_none()
                && args.fault_after_files.is_none()
                && args.desktop_shortcut.is_none()
                && args.start_menu_shortcut.is_none(),
            "maintenance uses fixed installed resources and does not accept demo, cancel or shortcut overrides"
        );
    }
    ensure!(
        args.engine_fault.is_none() || (!args.ui_demo && args.operation != Operation::Uninstall),
        "native fault injection is unavailable for demo or uninstall tasks"
    );
    ensure!(
        args.engine_pause.is_none()
            || (!args.ui_demo
                && (args.operation != Operation::Uninstall
                    || matches!(
                        args.engine_pause.as_deref(),
                        Some("after-first-remove" | "after-commit")
                    ))),
        "uninstall acceptance supports only after-first-remove and after-commit pauses; demo pauses are unavailable"
    );
    ensure!(
        !args.ui_demo || args.backend.is_none(),
        "--ui-demo accepts --manifest for a read-only preview, but cannot execute --backend"
    );
    if args.operation == Operation::Uninstall {
        ensure!(
            !args.ui_demo && args.install_dir.is_some(),
            "uninstall requires the installed directory and does not support UI demonstration"
        );
        ensure!(
            args.desktop_shortcut.is_none() && args.start_menu_shortcut.is_none(),
            "uninstall uses receipt shortcut ownership; install option overrides are unsupported"
        );
        ensure!(
            args.cancel_after_files.is_none()
                && args.cancel_after_ms.is_none()
                && args.fault_after_files.is_none(),
            "M0 uninstall has no cancellation checkpoints; cancellation and fault injection flags are unsupported"
        );
    }
    if args.operation == Operation::Upgrade {
        ensure!(
            !args.ui_demo
                && args.cancel_after_files.is_none()
                && args.cancel_after_ms.is_none()
                && args.fault_after_files.is_none(),
            "upgrade does not support UI demonstration, cancellation or fault-injection flags"
        );
    }
    if requests_gui(args) {
        ensure!(
            cfg!(feature = "gui"),
            "this runtime was built without GUI; use --install-dir <absolute-path> --report <report.json> for explicit CLI execution, or rebuild with --features gui"
        );
    } else {
        ensure!(
            args.install_dir.is_some() || args.silent,
            "headless runtime requires explicit --install-dir <absolute-path> (optionally --report <report.json>); an unpacked runtime also requires --backend <backend.exe> --manifest <manifest.json>; no installation was started"
        );
    }
    Ok(())
}

fn ini(directory: &PinnedDirectory, name: &str, value: &str) -> Result<PinnedFile> {
    let mut bytes = vec![0xff, 0xfe];
    for c in value.encode_utf16() {
        bytes.extend(c.to_le_bytes());
    }
    execution_files::create_file(directory, name, &bytes)
}

fn environment(args: &Args) -> Result<(Arc<PinnedFile>, Manifest)> {
    if let (Some(backend), Some(manifest)) = (&args.backend, &args.manifest) {
        // Resolve caller-supplied relative inputs once, then pin the resulting path.
        let input = PinnedFile::open(&std::path::absolute(manifest)?)?;
        let manifest: Manifest = serde_json::from_slice(&input.read_bytes()?)?;
        manifest.validate()?;
        let backend = Arc::new(PinnedFile::open(&std::path::absolute(backend)?)?);
        resources::validate_backend(&backend, &manifest)?;
        return Ok((backend, manifest));
    }
    ensure!(
        args.backend.is_none() && args.manifest.is_none(),
        "--backend and --manifest must be provided together"
    );
    // Keep the package and its ancestor paths immutable while the reader opens it.
    let package = PinnedFile::open(&std::env::current_exe()?)?;
    let (backend, manifest) = avelin_core::embedded(package.path())?
        .context("no embedded payload; pass --backend and --manifest or --ui-demo")?;
    let folder = session_root(args)?;
    let backend = execution_files::create_file(&folder, "backend.exe", &backend)?;
    resources::validate_backend(&backend, &manifest)?;
    Ok((Arc::new(backend), manifest))
}

#[cfg(feature = "gui")]
fn preview_manifest(path: Option<&Path>) -> Result<Option<Manifest>> {
    path.map(|path| {
        let input = PinnedFile::open(&std::path::absolute(path)?)?;
        let manifest: Manifest = serde_json::from_slice(&input.read_bytes()?)?;
        manifest.validate()?;
        Ok(manifest)
    })
    .transpose()
}

fn session_root(args: &Args) -> Result<PinnedDirectory> {
    let base = if let Some(report) = &args.report {
        std::path::absolute(report)?
            .parent()
            .context("report parent")?
            .to_path_buf()
    } else {
        std::env::temp_dir().join("AvelinM0")
    };
    execution_files::create_private_session(&base, "session")
}

pub fn entry(mut args: Args) -> Result<i32> {
    validate_launch(&args)?;
    if let Some(pid) = args.wait_for_pid {
        wait_for_parent(pid)?;
    }
    if args.maintenance {
        return launch_installed_maintenance(&args);
    }
    if args.operation == Operation::Uninstall {
        if let Some(root) = &args.install_dir {
            crate::upgrade::ensure_no_pending(root)?;
            let executable = std::env::current_exe()?;
            if resources::same_path(&executable, &root.join(".avelin/avelin-app.exe")) {
                // The resident frontend must exit before its installation can be removed.
                let (backend, manifest) = environment(&args)?;
                let receipt =
                    resources::read_receipt(&manifest, root, &transport::current_user_sid()?)?;
                validate_uninstaller(root, &backend, &receipt)?;
                let session = session_root(&args)?;
                let source = PinnedFile::open(&executable)?;
                ensure!(
                    manifest.runtime_sha256.as_deref() == Some(source.sha256()?.as_str()),
                    "resident frontend was modified"
                );
                let copy =
                    execution_files::copy_file(&session, "uninstall-coordinator.exe", &source)?;
                let mut child_args = vec![
                    "--operation".into(),
                    "uninstall".into(),
                    "--backend".into(),
                    backend.path().to_string_lossy().into_owned(),
                    "--manifest".into(),
                    std::path::absolute(
                        args.manifest
                            .as_ref()
                            .context("resident manifest required")?,
                    )?
                    .to_string_lossy()
                    .into_owned(),
                    "--install-dir".into(),
                    std::path::absolute(root)?.to_string_lossy().into_owned(),
                    "--wait-for-pid".into(),
                    std::process::id().to_string(),
                    "--observation-timeout-secs".into(),
                    args.observation_timeout_secs.to_string(),
                ];
                if let Some(report) = &args.report {
                    child_args.extend([
                        "--report".into(),
                        std::path::absolute(report)?.to_string_lossy().into_owned(),
                    ]);
                }
                if args.silent {
                    child_args.push("--silent".into());
                }
                if args.gui {
                    child_args.push("--gui".into());
                }
                if args.english {
                    child_args.push("--english".into());
                }
                use std::os::windows::process::CommandExt;
                Command::new(copy.path())
                    .args(child_args)
                    .current_dir(session.path())
                    .env("TEMP", session.path())
                    .env("TMP", session.path())
                    .creation_flags(0x08000000)
                    .spawn()?;
                return Ok(0);
            }
        }
    }
    if args.ui_demo {
        #[cfg(feature = "gui")]
        {
            let manifest = preview_manifest(args.manifest.as_deref())?;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                avelin_ui::run(ui_options(&args, manifest.as_ref())).map_err(anyhow::Error::from)
            }));
            // Demo never creates an installation worker, even if graphics panic.
            return merge_gui_outcome(None, args.report.clone(), None, outcome);
        }
        #[cfg(not(feature = "gui"))]
        anyhow::bail!("rebuild with --features gui to run the GPUI experiment");
    }
    let (backend, manifest) = environment(&args)?;
    // Discovery selects a confirmation page; it never applies recovery or repair.
    if args.operation != Operation::Uninstall {
        // An interrupted registration write can leave incomplete values. Find
        // retained task evidence before attempting ordinary registration parsing.
        if args.operation == Operation::Install && requests_gui(&args) {
            let probe_target = args.install_dir.clone().unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default())
                    .join(&manifest.directory_name)
            });
            let pending = match crate::maintenance::pending_session_from_lock(
                &manifest,
                &transport::current_user_sid()?,
            )? {
                Some(session) => Some(session),
                None => crate::maintenance::pending_session(&manifest, &probe_target)?,
            };
            if let Some(session) = pending {
                let original_target = crate::maintenance::recovery_target(&manifest, &session)?;
                if let Some(requested) = &args.install_dir {
                    ensure!(
                        resources::same_path(requested, &original_target),
                        "recovery cannot change the interrupted task directory"
                    );
                }
                args.operation = Operation::Recover;
                args.recovery_session = Some(session);
                args.install_dir = Some(original_target);
            }
        }
        if args.operation == Operation::Recover
            && args.recovery_session.is_none()
            && args.install_dir.is_none()
        {
            args.recovery_session = crate::maintenance::pending_session_from_lock(
                &manifest,
                &transport::current_user_sid()?,
            )?;
        }
        if args.operation == Operation::Recover && args.install_dir.is_none() {
            if let Some(session) = &args.recovery_session {
                args.install_dir = Some(crate::maintenance::recovery_target(&manifest, session)?);
            }
        }
        let registered = if args.operation == Operation::Recover {
            // Recovery verifies the original session and raw registry snapshots
            // inside the engine, including legitimately partial writes.
            None
        } else {
            crate::upgrade::registration(&manifest.app_id)?
        };
        let mut discovered_target = args
            .install_dir
            .clone()
            .or_else(|| {
                registered
                    .as_ref()
                    .map(|r| PathBuf::from(&r.install_location))
            })
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default())
                    .join(&manifest.directory_name)
            });
        if args.operation == Operation::Install && requests_gui(&args) {
            let pending = match crate::maintenance::pending_session(&manifest, &discovered_target)?
            {
                Some(session) => Some(session),
                None => crate::maintenance::pending_session_from_lock(
                    &manifest,
                    &transport::current_user_sid()?,
                )?,
            };
            if let Some(session) = pending {
                let original_target = crate::maintenance::recovery_target(&manifest, &session)?;
                if let Some(requested) = &args.install_dir {
                    ensure!(
                        resources::same_path(requested, &original_target),
                        "recovery cannot change the interrupted task directory"
                    );
                }
                discovered_target = original_target;
                args.operation = Operation::Recover;
                args.recovery_session = Some(session);
                args.install_dir = Some(discovered_target.clone());
            } else if discovered_target.join(RECEIPT_NAME).try_exists()? {
                let prior_version = crate::maintenance::installed_version(&discovered_target)?;
                if prior_version == manifest.app_version {
                    args.operation = Operation::Repair;
                    args.install_dir = Some(discovered_target.clone());
                } else if registered.is_some() {
                    args.operation = Operation::Upgrade;
                }
            } else if registered.is_some() {
                args.operation = Operation::Upgrade;
            }
        }
        if matches!(
            args.operation,
            Operation::Repair | Operation::Recover | Operation::Modify
        ) {
            if let Some(registration) = &registered {
                ensure!(
                    resources::same_path(
                        &discovered_target,
                        Path::new(&registration.install_location)
                    ),
                    "maintenance cannot change the registered installation directory"
                );
            } else {
                ensure!(
                    args.install_dir.is_some(),
                    "without registration, specify the original --install-dir for maintenance"
                );
            }
            args.install_dir = Some(discovered_target.clone());
            if matches!(args.operation, Operation::Repair | Operation::Modify) {
                let repair = crate::maintenance::repair_preview(
                    &manifest,
                    &discovered_target,
                    &transport::current_user_sid()?,
                )?;
                ensure!(
                    repair.receipt.uninstaller_sha256.as_deref()
                        == Some(backend.sha256()?.as_str()),
                    "repair requires the exact original Rust backend package"
                );
                args.maintenance_details = repair.findings;
            } else {
                let session = match args.recovery_session.take() {
                    Some(session) => std::path::absolute(session)?,
                    None => crate::maintenance::pending_session(&manifest, &discovered_target)?
                        .context("no interrupted Rust task found; pass --recovery-session")?,
                };
                let inspection = avelin_engine::inspect_recovery(
                    &manifest,
                    &discovered_target,
                    &session,
                    &transport::current_user_sid()?,
                )?;
                ensure!(inspection.verified, "recovery inspection was not verified");
                args.maintenance_details = crate::maintenance::recovery_findings(&inspection);
                args.recovery_session = Some(session);
            }
            validate_launch(&args)?;
        }
        if args.operation == Operation::Upgrade {
            let registration = registered.context("upgrade requires an installed product")?;
            let registered_target = PathBuf::from(&registration.install_location);
            if let Some(requested) = &args.install_dir {
                ensure!(
                    resources::same_path(requested, &registered_target),
                    "upgrade cannot change the registered installation directory"
                );
            }
            args.install_dir = Some(registered_target.clone());
            crate::upgrade::ensure_no_pending(&registered_target)?;
            if args.desktop_shortcut.is_none() || args.start_menu_shortcut.is_none() {
                let input = PinnedFile::open(&registered_target.join(".avelin/manifest.json"))?;
                let previous: Manifest = serde_json::from_slice(&input.read_bytes()?)?;
                previous.validate()?;
                let receipt = resources::read_receipt(
                    &previous,
                    &registered_target,
                    &transport::current_user_sid()?,
                )?;
                args.desktop_shortcut.get_or_insert(
                    receipt
                        .shortcuts
                        .iter()
                        .any(|s| s.kind == ShortcutKind::Desktop),
                );
                args.start_menu_shortcut.get_or_insert(
                    receipt
                        .shortcuts
                        .iter()
                        .any(|s| s.kind == ShortcutKind::StartMenu),
                );
            }
            validate_launch(&args)?;
        }
    }
    let selection_target = args.install_dir.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default())
            .join(&manifest.directory_name)
    });
    args.component_context = Some(crate::component_selection::resolve(
        &manifest,
        &selection_target,
        &transport::current_user_sid()?,
        args.operation,
        args.components.as_deref(),
        requests_gui(&args),
    )?);
    if requests_gui(&args) {
        #[cfg(feature = "gui")]
        {
            return run_gui(args, backend, manifest);
        }
        #[cfg(not(feature = "gui"))]
        anyhow::bail!(
            "this runtime was built without GUI; use the documented CLI flags or build --features gui"
        );
    }
    let (backend, usage) = crate::preflight::inspect_with_backend(&args, &manifest, backend)?;
    if !usage.is_clear() {
        return crate::preflight::save_blocked(&args, &usage, args.english);
    }
    run(args, backend, manifest, None)
}

fn launch_installed_maintenance(args: &Args) -> Result<i32> {
    use std::os::windows::process::CommandExt;
    let root = std::path::absolute(
        args.install_dir
            .as_ref()
            .context("maintenance directory required")?,
    )?;
    let resident = resources::same_path(
        &std::env::current_exe()?,
        &root.join(".avelin/avelin-app.exe"),
    );
    ensure!(
        !resident || !args.silent,
        "silent installed maintenance must be launched from an external runtime so its exit code can report the completed task; use the GUI for the resident maintenance entry"
    );
    let prepared = crate::installed_maintenance::prepare(&root, &transport::current_user_sid()?)?;
    let detach = requests_gui(args) || resident;
    let operation: avelin_core::Operation = args.operation.into();
    let mut command = Command::new(prepared.runtime.path());
    command
        .args([
            "--operation",
            serde_json::to_value(operation)?
                .as_str()
                .context("operation name")?,
        ])
        .arg("--backend")
        .arg(prepared.backend.path())
        .arg("--manifest")
        .arg(prepared.manifest.path())
        .arg("--install-dir")
        .arg(&root)
        .arg("--observation-timeout-secs")
        .arg(args.observation_timeout_secs.to_string())
        .current_dir(prepared.directory.path())
        .env("TEMP", prepared.directory.path())
        .env("TMP", prepared.directory.path())
        .creation_flags(0x08000000);
    command.arg(if args.silent { "--silent" } else { "--gui" });
    if detach {
        command
            .arg("--wait-for-pid")
            .arg(std::process::id().to_string());
    }
    if args.english {
        command.arg("--english");
    }
    if args.repair_overwrite {
        command.arg("--repair-overwrite");
    }
    if args.recovery_apply {
        command.arg("--recovery-apply");
    }
    if let Some(value) = &args.components {
        command.arg("--components").arg(value);
    }
    if let Some(value) = &args.recovery_session {
        command
            .arg("--recovery-session")
            .arg(std::path::absolute(value)?);
    }
    if let Some(value) = &args.report {
        command.arg("--report").arg(std::path::absolute(value)?);
    }
    let mut child = command
        .spawn()
        .context("could not start verified installed maintenance")?;
    if detach {
        Ok(0)
    } else {
        Ok(child.wait()?.code().unwrap_or(4))
    }
}

#[derive(Clone)]
pub struct UiHooks {
    #[cfg(feature = "gui")]
    snapshots: std::sync::mpsc::Sender<avelin_ui::UiSnapshot>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task_locations: SharedTaskLocations,
}

type SharedTaskLocations = std::sync::Arc<std::sync::Mutex<Option<TaskLocations>>>;

fn allocate_task_locations(
    task_id: String,
    session: PathBuf,
    requested_report: Option<&Path>,
    hooks: &Option<UiHooks>,
) -> Result<TaskLocations> {
    let session_dir = std::path::absolute(session)?;
    let report_path = std::path::absolute(
        requested_report
            .map(Path::to_path_buf)
            .unwrap_or_else(|| session_dir.join("report.json")),
    )?;
    let locations = TaskLocations {
        task_id,
        session_dir,
        report_path,
        saved_report: None,
    };
    if let Some(hooks) = hooks {
        // Diagnostic bookkeeping must not turn a poisoned lock into a new task
        // failure. There is a single worker, and these assignments do no I/O.
        *hooks
            .task_locations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(locations.clone());
    }
    Ok(locations)
}

fn record_saved_report(hooks: &Option<UiHooks>, path: &Path) {
    if let Some(hooks) = hooks {
        if let Some(locations) = hooks
            .task_locations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            locations.saved_report = Some(path.to_path_buf());
        }
    }
}

fn registry_open_result(status: i32) -> Result<bool> {
    match status {
        0 => Ok(true),
        2 => Ok(false), // ERROR_FILE_NOT_FOUND alone proves absence.
        other => Err(std::io::Error::from_raw_os_error(other))
            .context("cannot establish uninstall registration state"),
    }
}

fn registry_key_exists(subkey: &str) -> Result<bool> {
    use std::ffi::c_void;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(
            key: *mut c_void,
            subkey: *const u16,
            options: u32,
            access: u32,
            result: *mut *mut c_void,
        ) -> i32;
        fn RegCloseKey(key: *mut c_void) -> i32;
    }
    ensure!(!subkey.contains('\0'), "embedded NUL in registry path");
    let name: Vec<u16> = subkey.encode_utf16().chain(Some(0)).collect();
    let mut key = std::ptr::null_mut();
    // Match the worker's explicit 64-bit HKCU registry view.
    let status = unsafe {
        RegOpenKeyExW(
            0x80000001u32 as i32 as isize as *mut c_void,
            name.as_ptr(),
            0,
            0x0001 | 0x0100,
            &mut key,
        )
    };
    if status == 0 {
        unsafe {
            RegCloseKey(key);
        }
    }
    registry_open_result(status)
}

fn validate_uninstaller(target: &Path, backend: &PinnedFile, receipt: &Receipt) -> Result<()> {
    let expected = std::path::absolute(target.join("uninstall.exe"))?;
    ensure!(
        avelin_core::same_windows_name(
            &backend.path().to_string_lossy().replace('/', "\\"),
            &expected.to_string_lossy().replace('/', "\\"),
        ),
        "uninstall backend must be this installation's uninstall.exe"
    );
    let expected_hash = receipt.uninstaller_sha256.as_deref()
        .context("receipt has no uninstaller integrity record; use the original matching runtime for this older installation")?;
    ensure!(
        expected_hash.len() == 64 && expected_hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid uninstaller integrity record"
    );
    ensure!(
        backend.sha256()?.eq_ignore_ascii_case(expected_hash),
        "installed uninstaller was modified"
    );
    Ok(())
}

#[cfg(any(feature = "gui", test))]
fn task_locations_snapshot(locations: &SharedTaskLocations) -> Option<TaskLocations> {
    locations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn run(
    args: Args,
    backend: Arc<PinnedFile>,
    manifest: Manifest,
    hooks: Option<UiHooks>,
) -> Result<i32> {
    let target = args.install_dir.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default())
            .join(&manifest.directory_name)
    });
    install_root(&target)?;
    let user_sid = transport::current_user_sid()?;
    let mut product_lock = if args.operation == Operation::Recover {
        // Recovery validates the old task and claims the engine's product mutex.
        // Its new coordinator must not remove or overwrite the old task's lock.
        AppLock::acquire(
            &format!(
                "{}-recovery-{}",
                manifest.app_id,
                uuid::Uuid::new_v4().simple()
            ),
            &user_sid,
        )?
    } else {
        AppLock::acquire(&manifest.app_id, &user_sid)?
    };
    let registry_key = format!(
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.{}",
        manifest.app_id
    );
    let manifest_hash = hash_bytes(&serde_json::to_vec(&manifest)?);
    let selection = crate::component_selection::resolve(
        &manifest,
        &target,
        &user_sid,
        args.operation,
        args.components.as_deref(),
        false,
    )?;
    if let Some(expected) = args
        .component_context
        .as_ref()
        .and_then(|context| context.receipt_sha256.as_ref())
    {
        ensure!(
            selection.receipt_sha256.as_ref() == Some(expected),
            "installation changed since the component choices were displayed; reopen the installer"
        );
    }
    let selected_resources = avelin_core::resolve_selection(&manifest, &selection.selected)?;
    let recovery_preflight = if args.operation == Operation::Recover {
        Some(avelin_engine::inspect_recovery(
            &manifest,
            &target,
            args.recovery_session
                .as_deref()
                .context("recovery session required")?,
            &user_sid,
        )?)
    } else {
        crate::upgrade::ensure_no_pending(&target)?;
        None
    };
    let repair_inputs = if args.operation == Operation::Repair {
        let repair = crate::maintenance::repair_inputs(&manifest, &target, &user_sid)?;
        ensure!(
            repair.receipt.uninstaller_sha256.as_deref() == Some(backend.sha256()?.as_str()),
            "repair backend differs from the original installation"
        );
        Some(repair)
    } else {
        None
    };
    let previous = if matches!(args.operation, Operation::Upgrade | Operation::Modify) {
        let (desktop, start_menu) = if args.operation == Operation::Modify {
            let receipt = resources::read_receipt(&manifest, &target, &user_sid)?;
            ensure!(
                receipt.uninstaller_sha256.as_deref() == Some(backend.sha256()?.as_str()),
                "modify requires the exact original Rust backend package"
            );
            (
                receipt
                    .shortcuts
                    .iter()
                    .any(|s| s.kind == ShortcutKind::Desktop),
                receipt
                    .shortcuts
                    .iter()
                    .any(|s| s.kind == ShortcutKind::StartMenu),
            )
        } else {
            (
                args.desktop_shortcut.unwrap_or(manifest.desktop_shortcut),
                args.start_menu_shortcut
                    .unwrap_or(manifest.start_menu_shortcut),
            )
        };
        Some(crate::upgrade::prepare(
            &manifest,
            &target,
            &user_sid,
            desktop,
            start_menu,
            &selection.selected,
            args.operation == Operation::Modify,
        )?)
    } else {
        None
    };
    let mut internal_hashes = Vec::new();
    let existing_receipt = match args.operation {
        Operation::Install => {
            resources::validate_selected_destination(&target, &selected_resources.files)?;
            ensure!(
                !registry_key_exists(&registry_key)?,
                "this app already has an uninstall registration; use the explicit upgrade operation with a newer package"
            );
            None
        }
        Operation::Upgrade | Operation::Modify => None,
        Operation::Recover => None,
        Operation::Repair => Some(
            repair_inputs
                .as_ref()
                .expect("repair inputs checked")
                .receipt
                .clone(),
        ),
        Operation::Uninstall => {
            let (receipt, receipt_hash) =
                resources::read_receipt_with_hash(&manifest, &target, &user_sid)?;
            validate_uninstaller(&target, &backend, &receipt)?;
            let runtime = target.join(".avelin/avelin-app.exe");
            let runtime_hash = resources::confirm_owned_hash(
                &runtime,
                manifest
                    .runtime_sha256
                    .as_deref()
                    .context("manifest has no runtime integrity record")?,
            )?;
            let installed_manifest = PinnedFile::open(&target.join(".avelin/manifest.json"))?;
            let installed_metadata: Manifest =
                serde_json::from_slice(&installed_manifest.read_bytes()?)?;
            ensure!(
                hash_bytes(&serde_json::to_vec(&installed_metadata)?) == manifest_hash,
                "installed manifest does not match the package"
            );
            internal_hashes = vec![
                (RECEIPT_NAME, receipt_hash),
                (
                    "uninstall.exe",
                    receipt
                        .uninstaller_sha256
                        .clone()
                        .context("receipt has no uninstaller integrity record")?,
                ),
                (".avelin/avelin-app.exe", runtime_hash),
                (".avelin/manifest.json", installed_manifest.sha256()?),
            ];
            Some(receipt)
        }
    };
    let (desktop, start_menu) = if args.operation == Operation::Recover {
        (false, false)
    } else if let Some(receipt) = &existing_receipt {
        (
            receipt
                .shortcuts
                .iter()
                .any(|s| s.kind == ShortcutKind::Desktop),
            receipt
                .shortcuts
                .iter()
                .any(|s| s.kind == ShortcutKind::StartMenu),
        )
    } else if let Some(previous) = &previous {
        (
            previous
                .shortcuts
                .iter()
                .any(|s| s.kind == ShortcutKind::Desktop),
            previous
                .shortcuts
                .iter()
                .any(|s| s.kind == ShortcutKind::StartMenu),
        )
    } else {
        (
            args.desktop_shortcut.unwrap_or(manifest.desktop_shortcut),
            args.start_menu_shortcut
                .unwrap_or(manifest.start_menu_shortcut),
        )
    };
    let shortcuts = if let Some(previous) = &previous {
        previous.shortcuts.clone()
    } else if let Some(receipt) = &existing_receipt {
        receipt.shortcuts.clone()
    } else {
        resources::planned_shortcuts(&manifest, &target, desktop, start_menu)?
    };
    for resource in &selected_resources.files {
        let path = target.join(&resource.path);
        reject_reparse(&path)?;
        ensure!(
            path.to_string_lossy().encode_utf16().count() < 240,
            "M0 path limit exceeded"
        );
    }
    let execution_directory = session_root(&args)?;
    let locations = allocate_task_locations(
        uuid::Uuid::new_v4().to_string(),
        execution_directory.path().to_path_buf(),
        args.report.as_deref(),
        &hooks,
    )?;
    // Publish allocated locations before candidate/configuration writes can
    // fail. A location alone does not claim the report exists or the worker started.
    let task = locations.task_id;
    let session = locations.session_dir;
    let report_path = locations.report_path;
    if args.operation == Operation::Modify && selection.selected == selection.installed {
        let message = if args.english {
            "No component changes were requested."
        } else {
            "组件选择未变化，无需更改。"
        };
        let report = json!({"task_id":task,"operation":"modify","result":"Succeeded","coordinator_exit_code":0,
            "maintenance_status":"NoChanges","changed":false,"message":message,"install_dir":target,
            "session_dir":session,"selected_components":selection.selected,"selection_source":selection.source,
            "payload_file_count":selected_resources.payload_file_count,"payload_bytes":selected_resources.payload_bytes,"report_path":report_path});
        if let Some(parent) = report_path.parent() {
            fs::create_dir_all(parent)?;
        }
        reject_reparse(&report_path)?;
        fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
        record_saved_report(&hooks, &report_path);
        println!("{}", serde_json::to_string(&report)?);
        notify_finished(&hooks, TaskResult::Succeeded, message);
        return Ok(0);
    }
    ensure!(
        !session.to_string_lossy().contains(['\r', '\n', '\0']),
        "invalid task evidence path"
    );
    product_lock.seal(&session, &target)?;
    let _prior_receipt_guard = if let Some(repair) = &repair_inputs {
        resources::confirm_owned_hash(
            &target.join(RECEIPT_NAME),
            &hash_bytes(&repair.receipt_bytes),
        )?;
        Some(execution_files::create_file(
            &execution_directory,
            "prior-receipt.json",
            &repair.receipt_bytes,
        )?)
    } else {
        None
    };
    let modify_snapshot = if args.operation == Operation::Modify {
        let snapshot = avelin_engine::capture_modify_snapshot(
            &manifest,
            &previous
                .as_ref()
                .context("modify installation missing")?
                .receipt,
            &target,
        )?;
        Some(serde_json::to_vec_pretty(&snapshot)?)
    } else {
        None
    };
    let _modify_snapshot_guard = modify_snapshot
        .as_ref()
        .map(|bytes| {
            execution_files::create_file(&execution_directory, "modify-snapshot.json", bytes)
        })
        .transpose()?;
    let mut plan = if let Some(previous) = &previous {
        let payload_bytes = selected_resources
            .files
            .iter()
            .try_fold(0u64, |sum, item| {
                sum.checked_add(item.size)
                    .context("upgrade payload size overflow")
            })?;
        let old_bytes = previous.receipt.files.iter().try_fold(0u64, |sum, item| {
            sum.checked_add(item.size)
                .context("old payload size overflow")
        })?;
        let backup_bytes = previous.backup.iter().try_fold(0u64, |sum, item| {
            sum.checked_add(item.size)
                .context("upgrade backup size overflow")
        })?;
        let component_bound = fs::metadata(std::env::current_exe()?)?
            .len()
            .checked_add(fs::metadata(backend.path())?.len())
            .context("upgrade component size overflow")?;
        let target_bytes = payload_bytes
            .checked_add(component_bound)
            .context("upgrade target size overflow")?;
        let temporary_bytes = target_bytes
            .checked_add(backup_bytes.max(old_bytes))
            .and_then(|size| size.checked_add(component_bound))
            .and_then(|size| size.checked_add(16 * 1024 * 1024))
            .context("upgrade temporary size overflow")?;
        crate::upgrade::check_space(&target, &session, target_bytes, temporary_bytes)?;
        let upgrade = avelin_core::model::UpgradePlan {
            previous_version: previous.manifest.app_version.clone(),
            previous_manifest_sha256: previous.receipt.manifest_sha256.clone(),
            previous_receipt_sha256: previous.receipt_sha256.clone(),
            previous_task_id: previous.receipt.task_id.clone(),
            previous_commit_id: previous.receipt.commit_id.clone(),
            installation_id: previous.receipt.installation_id.clone(),
            diff: avelin_core::model::UpgradeDiff::between_files(
                &previous.receipt.files,
                &selected_resources.files,
            )?,
            selected_components: selection.selected.clone(),
            previous_selected_components: previous.receipt.selection(&previous.manifest)?,
            modify_snapshot_sha256: modify_snapshot.as_ref().map(|bytes| hash_bytes(bytes)),
            backend_sha256: (args.operation == Operation::Modify)
                .then(|| backend.sha256())
                .transpose()?,
            staging_dir: session.join("staging"),
            backup_dir: session.join("backup"),
            required_space_bytes: target_bytes
                .checked_add(temporary_bytes)
                .context("upgrade required size overflow")?,
        };
        if args.operation == Operation::Modify {
            InstallPlan::new_modify(
                &manifest,
                &previous.receipt,
                &previous.receipt_sha256,
                task.clone(),
                upgrade,
            )?
        } else {
            InstallPlan::new_upgrade_selected(
                &manifest,
                &previous.manifest,
                &previous.receipt,
                &previous.receipt_sha256,
                task.clone(),
                upgrade,
                selection.selected.clone(),
                selection.source,
            )?
        }
    } else {
        InstallPlan::new_selected(
            &manifest,
            args.operation.into(),
            task.clone(),
            target,
            user_sid,
            selection.selected.clone(),
            selection.source,
        )?
    };
    plan.desktop_shortcut = desktop;
    plan.start_menu_shortcut = start_menu;
    plan.validate(&manifest)?;
    if let Some(previous) = &previous {
        if args.operation == Operation::Modify {
            plan.validate_modify(&manifest, &previous.receipt, &previous.receipt_sha256)?;
        } else {
            plan.validate_upgrade(
                &manifest,
                &previous.manifest,
                &previous.receipt,
                &previous.receipt_sha256,
            )?;
        }
    }
    ensure!(
        plan.manifest_sha256 == manifest_hash,
        "plan manifest binding changed"
    );
    // Keep the validated task intent beside its diagnostics. The backend still
    // performs its existing checks; this record is not proof of execution.
    let _plan_guard = execution_files::create_file(
        &execution_directory,
        "install-plan.json",
        &serde_json::to_vec_pretty(&plan)?,
    )?;
    let target = plan.install_dir.clone();
    let token =
        uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string();
    let pipe = format!(r"\\.\pipe\avelin-m0-{}", uuid::Uuid::new_v4().simple());
    let candidate = session.join("receipt-candidate.json");
    let mut receipt = existing_receipt.unwrap_or_else(|| Receipt {
        schema_version: if manifest.schema_version == 1 {
            SCHEMA_VERSION
        } else {
            avelin_core::RECEIPT_SCHEMA_VERSION
        },
        app_id: plan.app_id.clone(),
        app_version: plan.app_version.clone(),
        installation_id: previous
            .as_ref()
            .map(|p| p.receipt.installation_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        task_id: plan.task_id.clone(),
        commit_id: uuid::Uuid::new_v4().to_string(),
        install_dir: target.to_string_lossy().into_owned(),
        manifest_sha256: plan.manifest_sha256.clone(),
        user_sid: plan.user_sid.clone(),
        files: selected_resources.files.clone(),
        selected_components: if manifest.schema_version == 1 {
            vec![]
        } else {
            selection.selected.clone()
        },
        uninstaller_sha256: None,
        shortcuts: if previous.is_some() {
            shortcuts.clone()
        } else {
            Vec::new()
        },
        previous_commit_id: previous.as_ref().map(|p| p.receipt.commit_id.clone()),
    });
    if args.operation == Operation::Repair {
        // Repaired shortcuts can have new bytes. Publish a new receipt commit
        // linked to the verified prior receipt instead of reusing its identity.
        receipt.previous_commit_id = Some(receipt.commit_id.clone());
        receipt.commit_id = uuid::Uuid::new_v4().to_string();
        receipt.task_id = task.clone();
        receipt.uninstaller_sha256 = None;
    }
    let _candidate_guard = execution_files::create_file(
        &execution_directory,
        "receipt-candidate.json",
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    let session_ini = session.join("task.ini");
    let mut config = format!(
        "[task]\r\ninstall_dir={}\r\npipe_name={}\r\ntoken={}\r\ntask_id={}\r\nreceipt_candidate={}\r\nfault_after_files={}\r\ncancel_after_files={}\r\nstep_delay_ms={}\r\n",
        target.display(),
        pipe,
        token,
        task,
        candidate.display(),
        args.fault_after_files.unwrap_or(u64::MAX),
        args.cancel_after_files.unwrap_or(u64::MAX),
        args.step_delay_ms
    );
    if let Some(point) = &args.engine_fault {
        config.push_str(&format!("engine_fault={point}\r\n"));
    }
    if let Some(point) = &args.engine_pause {
        config.push_str(&format!("engine_pause={point}\r\n"));
    }
    if let Some(original) = &args.recovery_session {
        ensure!(
            !original.to_string_lossy().contains(['\r', '\n', '\0']),
            "invalid recovery session path"
        );
        config.push_str(&format!(
            "recovery_session={}\r\nrecovery_apply={}\r\n",
            original.display(),
            u8::from(args.recovery_apply)
        ));
    }
    config.push_str(&format!(
        "repair_overwrite={}\r\n",
        u8::from(args.repair_overwrite)
    ));
    let shortcut_evidence = session.join("shortcuts.ini");
    ensure!(
        !shortcut_evidence.exists(),
        "shortcut evidence already exists"
    );
    for (kind, field) in [
        (ShortcutKind::Desktop, "desktop"),
        (ShortcutKind::StartMenu, "start_menu"),
    ] {
        let selected = shortcuts.iter().find(|shortcut| shortcut.kind == kind);
        config.push_str(&format!(
            "{field}_shortcut_path={}\r\n{field}_shortcut_sha256={}\r\n",
            selected.map(|s| s.path.as_str()).unwrap_or(""),
            selected.map(|s| s.sha256.as_str()).unwrap_or("")
        ));
    }
    config.push_str(&format!(
        "shortcut_target={}\r\nshortcut_working_dir={}\r\nshortcut_evidence={}\r\n",
        target.join(&manifest.entry).display(),
        target.display(),
        shortcut_evidence.display()
    ));
    for (relative, expected_hash) in internal_hashes {
        resources::confirm_owned_hash(&target.join(relative), &expected_hash)?;
    }
    let mut _upgrade_inputs = Vec::new();
    if let Some(previous) = &previous {
        for item in &previous.backup {
            resources::confirm_owned_hash(&item.path, &item.sha256)?;
        }
        let inventory = json!({"schema_version":1,"task_id":task,"backup":previous.backup,"preserved":previous.preserved,"registration":previous.registration});
        _upgrade_inputs.push(execution_files::create_file(
            &execution_directory,
            "upgrade-resources.json",
            &serde_json::to_vec_pretty(&inventory)?,
        )?);
        for (source, expected, name) in [
            (
                target.join(RECEIPT_NAME),
                previous.receipt_sha256.as_str(),
                "previous-receipt.json",
            ),
            (
                target.join(".avelin/manifest.json"),
                previous.manifest_file_sha256.as_str(),
                "previous-manifest.json",
            ),
        ] {
            let input = PinnedFile::open(&source)?;
            ensure!(
                input.sha256()? == expected,
                "upgrade input changed after preflight"
            );
            _upgrade_inputs.push(execution_files::create_file(
                &execution_directory,
                name,
                &input.read_bytes()?,
            )?);
        }
    }
    let _configuration_guard = ini(&execution_directory, "task.ini", &config)?;
    let server = transport::PipeServer::bind(&pipe, &token, &task)?;
    let runtime = if args.operation == Operation::Uninstall {
        // Copy the known installed uninstaller to the session, bypass its extra bootstrap child.
        let copy =
            execution_files::copy_file(&execution_directory, "uninstall-runner.exe", &backend)?;
        // The worker must be able to remove the installed source and its parents.
        drop(backend);
        Arc::new(copy)
    } else {
        backend
    };
    let mut command = Command::new(runtime.path());
    command
        .env("AVELIN_SESSION_INI", &session_ini)
        .env("TEMP", execution_directory.path())
        .env("TMP", execution_directory.path())
        .current_dir(execution_directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x08000000);
    ensure!(
        !hooks
            .as_ref()
            .is_some_and(|h| h.shutdown.load(std::sync::atomic::Ordering::Relaxed)),
        "UI closed before backend startup; no backend was started"
    );
    let started = Instant::now();
    let child = command.spawn().context("start installation backend")?;
    // No fallible work may occur between spawning the backend and installing its guard.
    let mut backend_guard = BackendGuard {
        child,
        lock: product_lock,
        task: task.clone(),
        target: target.clone(),
        session: session.clone(),
        report_path: report_path.clone(),
        hooks: hooks.clone(),
        completed: false,
        english: args.english,
        fault_after_files: args.fault_after_files,
        last_progress: None,
        last_work_progress: None,
    };
    let execution = (|| -> Result<i32> {
        use avelin_core::session::{SessionRecord, running_process};
        let identity = SessionRecord {
            schema_version: 1,
            task_id: task.clone(),
            app_id: manifest.app_id.clone(),
            operation: plan.operation.as_str().into(),
            install_dir: target.clone(),
            manifest_sha256: receipt.manifest_sha256.clone(),
            coordinator: running_process(std::process::id())?
                .context("coordinator identity missing")?,
            backend: running_process(backend_guard.child.id())?
                .context("backend exited before identity capture")?,
            product_lock: backend_guard.lock.path.clone(),
            report_path: report_path.clone(),
        };
        // Persist identity before acknowledging the pipe. It contains no session token.
        fs::write(
            session.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        fs::write(
            session.join("session.json"),
            serde_json::to_vec_pretty(&identity)?,
        )?;
        let mut events = Vec::<Value>::new();
        let mut backend_events = BackendEvents::default();
        let mut message = String::new();
        let mut backend_detail = None::<String>;
        let mut connection =
            match server.accept_from(Duration::from_secs(15), backend_guard.child.id()) {
                Ok(connection) => Some(connection),
                Err(e) => {
                    message = format!("backend handshake failed: {e}");
                    None
                }
            };
        let mut cancel_sent = false;
        let mut cancellation = json!({"requested_elapsed_ms":null,"acknowledged_elapsed_ms":null,
            "stopped_elapsed_ms":null,"cleanup_completed_elapsed_ms":null});
        let mut child_exit = None;
        let mut shutdown_at: Option<Instant> = None;
        let mut journal = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(session.join("events.jsonl"))?;
        loop {
            if hooks
                .as_ref()
                .is_some_and(|h| h.shutdown.load(std::sync::atomic::Ordering::Relaxed))
            {
                shutdown_at.get_or_insert_with(Instant::now);
            }
            if let Some(c) = &mut connection {
                let user_cancel = hooks
                    .as_ref()
                    .is_some_and(|h| h.cancel.load(std::sync::atomic::Ordering::Relaxed));
                if !cancel_sent
                    && args.operation == Operation::Install
                    && backend_events.accepts_commands()
                    && (user_cancel
                        || args.cancel_after_files == Some(0)
                        || args
                            .cancel_after_ms
                            .is_some_and(|ms| started.elapsed() >= Duration::from_millis(ms)))
                {
                    cancellation["requested_elapsed_ms"] = json!(started.elapsed().as_millis());
                    if let Err(error) = c.send_cancel() {
                        // A backend may already have queued its terminal and
                        // closed the pipe. Preserve the failed intent, then drain.
                        cancellation["request_error"] = json!(error.to_string());
                    }
                    cancel_sent = true;
                }
                match c.recv_timeout(Duration::from_millis(100)) {
                    Ok(mut event) => {
                        let observed = json!(started.elapsed().as_millis());
                        event["coordinator_received_elapsed_ms"] = observed.clone();
                        // Keep the original payload fields even if validation fails.
                        // Rejected events must not influence UI or cancellation metrics.
                        writeln!(journal, "{}", serde_json::to_string(&event)?)?;
                        journal.flush()?;
                        let accepted = backend_events.observe(&event);
                        events.push(event);
                        match accepted {
                            Some(BackendEvent::Finished(_)) => {
                                backend_detail = events
                                    .last()
                                    .and_then(|value| value["detail"].as_str())
                                    .map(|detail| detail.chars().take(2048).collect());
                            }
                            Some(BackendEvent::CancelReceived) => {
                                cancellation["acknowledged_elapsed_ms"] = observed;
                            }
                            Some(BackendEvent::Progress(progress)) => {
                                backend_guard.last_progress = Some(progress);
                                if !matches!(
                                    progress.phase,
                                    ProgressPhase::Cancelling | ProgressPhase::CleanupCompleted
                                ) {
                                    backend_guard.last_work_progress = Some(progress);
                                }
                                match progress.phase {
                                    ProgressPhase::Cancelling => {
                                        cancellation["stopped_elapsed_ms"] = observed;
                                    }
                                    ProgressPhase::CleanupCompleted => {
                                        cancellation["cleanup_completed_elapsed_ms"] = observed;
                                    }
                                    _ => {}
                                }
                                if progress.phase != ProgressPhase::CleanupCompleted {
                                    notify_progress(&hooks, progress);
                                }
                                if !cancel_sent
                                    && args
                                        .cancel_after_files
                                        .is_some_and(|n| progress.completed >= n)
                                    && progress.phase != ProgressPhase::Finalizing
                                {
                                    cancellation["requested_elapsed_ms"] =
                                        json!(started.elapsed().as_millis());
                                    if let Err(error) = c.send_cancel() {
                                        cancellation["request_error"] = json!(error.to_string());
                                    }
                                    cancel_sent = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => {
                        observe_pipe_failure(&mut backend_events, &e, c.pending_input_len());
                        connection = None;
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
            if let Some(status) = backend_guard.child.try_wait()? {
                child_exit = Some(status.code().unwrap_or(-1));
            }
            // A terminal followed by queued conflicting data must not be hidden
            // by a process that exited before the next receive iteration.
            if observation_complete(child_exit, connection.is_some()) {
                break;
            }
            if shutdown_at.is_some_and(|at| at.elapsed() > Duration::from_secs(5)) {
                message = "UI closed; backend did not finish during the shutdown observation budget; result unknown, process was not force-killed".into();
                break;
            }
            if started.elapsed() > Duration::from_secs(args.observation_timeout_secs) {
                message="backend did not exit within M0 observation budget; result unknown, process was not force-killed".into();
                break;
            }
        }
        let mut result = match backend_events.outcome(child_exit, connection.is_none()) {
            Ok(result) => result,
            Err(reason) => {
                message = if message.is_empty() {
                    reason
                } else {
                    format!("{message}; {reason}")
                };
                TaskResult::Unknown
            }
        };
        let mut repair_evidence = None;
        let mut recovery_evidence = None;
        let mut modify_evidence = None;
        if result == TaskResult::Succeeded {
            let verified = if args.operation == Operation::Recover {
                (|| -> Result<()> {
                    let input = PinnedFile::open(&session.join("recovery-report.json"))?;
                    let report: avelin_engine::RecoveryReport =
                        serde_json::from_slice(&input.read_bytes()?)?;
                    let preflight = recovery_preflight
                        .as_ref()
                        .context("recovery preflight missing")?;
                    ensure!(
                        report.schema_version == 1
                            && report.verified
                            && report.original_task_id == preflight.original_task_id
                            && report.applied == args.recovery_apply,
                        "recovery report differs from the checked task or requested action"
                    );
                    let verified = avelin_engine::inspect_recovery(
                        &manifest,
                        &target,
                        args.recovery_session
                            .as_deref()
                            .context("original recovery session missing")?,
                        &plan.user_sid,
                    )?;
                    verify_recovery_outcome(&report, &verified, args.recovery_apply)?;
                    message = if args.recovery_apply {
                        if args.english {
                            "Recovery completed and independently verified.".into()
                        } else {
                            "恢复已完成，并已独立核验。".into()
                        }
                    } else if args.english {
                        "Recovery check passed; no installation resources were changed. Use --recovery-apply to apply.".into()
                    } else {
                        "恢复检查通过，安装资源未改变；执行恢复需使用 --recovery-apply。".into()
                    };
                    recovery_evidence = Some(serde_json::to_value(report)?);
                    Ok(())
                })()
            } else if args.operation == Operation::Modify {
                (|| -> Result<()> {
                    let report = avelin_engine::verify_modify_result(
                        &manifest,
                        &target,
                        &plan.user_sid,
                        &session,
                    )?;
                    let committed = resources::read_receipt(&manifest, &target, &plan.user_sid)?;
                    ensure!(
                        committed.task_id == receipt.task_id
                            && committed.commit_id == receipt.commit_id
                            && committed.selected_components == receipt.selected_components
                            && committed.previous_commit_id == receipt.previous_commit_id,
                        "modified receipt differs from the frozen component selection or commit"
                    );
                    modify_evidence = Some(serde_json::to_value(report)?);
                    message = if args.english {
                        "Components changed. Existing changed or missing files were left as they were; see the report for details.".into()
                    } else {
                        "组件已更改，原有修改和缺失文件保持原状；详情见报告。".into()
                    };
                    Ok(())
                })()
            } else if args.operation == Operation::Repair {
                (|| -> Result<()> {
                    let input = PinnedFile::open(&session.join("repair-report.json"))?;
                    let report: avelin_engine::RepairReport =
                        serde_json::from_slice(&input.read_bytes()?)?;
                    let verified =
                        avelin_engine::verify_repair(&manifest, &target, &plan.user_sid)?;
                    let committed = resources::read_receipt(&manifest, &target, &plan.user_sid)?;
                    ensure!(
                        committed.task_id == receipt.task_id
                            && committed.commit_id == receipt.commit_id
                            && committed.installation_id == receipt.installation_id
                            && committed.previous_commit_id == receipt.previous_commit_id,
                        "repair receipt does not bind this task to its prior installation commit"
                    );
                    ensure!(
                        report.schema_version == 1
                            && report.task_id == task
                            && report.fully_repaired == verified.fully_repaired
                            && report
                                .preserved
                                .iter()
                                .collect::<std::collections::BTreeSet<_>>()
                                == verified
                                    .preserved
                                    .iter()
                                    .collect::<std::collections::BTreeSet<_>>(),
                        "repair outcome does not match independent resource verification"
                    );
                    message = if report.fully_repaired {
                        if args.english {
                            "Repair complete: all managed resources were verified.".into()
                        } else {
                            "完整修复：所有受管资源已核验。".into()
                        }
                    } else if args.english {
                        format!(
                            "Repair finished with {} modified resources preserved. These items were not repaired.",
                            report.preserved.len()
                        )
                    } else {
                        format!(
                            "修复流程完成，保留 {} 项已修改资源；这些项目未修复。",
                            report.preserved.len()
                        )
                    };
                    repair_evidence = Some(serde_json::to_value(report)?);
                    Ok(())
                })()
            } else if let Some(previous) = &previous {
                verify_upgrade_result(
                    &target,
                    &manifest,
                    &receipt,
                    previous,
                    &shortcut_evidence,
                    &session,
                )
            } else {
                verify_result(
                    &target,
                    &manifest,
                    &receipt,
                    args.operation,
                    &shortcuts,
                    &shortcut_evidence,
                )
            };
            if let Err(e) = verified {
                result = TaskResult::Unknown;
                message = format!("persistent evidence mismatch: {e:#}");
            }
        }
        if result == TaskResult::Cancelled {
            if selected_resources
                .files
                .iter()
                .any(|f| target.join(&f.path).exists())
                || target.join(RECEIPT_NAME).exists()
            {
                result = TaskResult::Failed;
                message = "cancel cleanup incomplete".into();
            }
        }
        if result == TaskResult::Failed
            && let Some(previous) = &previous
        {
            match crate::upgrade::verify_rollback(previous, &target) {
                Ok(()) => {
                    message = if args.operation == Operation::Modify {
                        if args.english {
                            "Component change failed; the original selection and resource state were independently verified unchanged or restored.".into()
                        } else {
                            "组件更改失败；已独立确认原组件选择和文件状态保持原状或已恢复。".into()
                        }
                    } else if args.english {
                        "Upgrade failed; the previous installation and preserved user resources were independently verified unchanged or restored. See the upgrade journal for details.".into()
                    } else {
                        "升级失败；已独立确认旧安装与保留的用户资源保持原状或已恢复。详情见升级日志。".into()
                    };
                }
                Err(error) => {
                    result = TaskResult::Unknown;
                    message = format!(
                        "maintenance rollback is not confirmed; preserve the session and backups: {error:#}"
                    );
                }
            }
        }
        let code = result.code();
        let diagnostic = task_diagnostics::diagnostic(
            result.as_str(),
            &message,
            backend_guard.last_progress,
            backend_guard.last_work_progress,
            child_exit,
            args.fault_after_files,
        )
        .map(|diagnostic| diagnostic.with_backend_detail(backend_detail.as_deref()));
        if message.trim().is_empty() {
            if let Some(diagnostic) = &diagnostic {
                message = diagnostic.reason.clone();
            }
        }
        let preserved_resources: Vec<Value> =
            if args.operation == Operation::Uninstall && result == TaskResult::Succeeded {
                receipt
                    .files
                    .iter()
                    .map(|file| target.join(&file.path))
                    .chain(
                        shortcuts
                            .iter()
                            .map(|shortcut| PathBuf::from(&shortcut.path)),
                    )
                    .filter(|path| path.exists())
                    .map(|path| json!({"path":path, "reason":"modified"}))
                    .collect()
            } else if result == TaskResult::Succeeded
                && let Some(previous) = &previous
            {
                previous
                    .preserved
                    .iter()
                    .map(|item| json!({"path":item.path,"reason":"modified_retired_resource"}))
                    .collect()
            } else {
                Vec::new()
            };
        if !preserved_resources.is_empty() && args.operation == Operation::Uninstall {
            message = if args.english {
                format!(
                    "Uninstalled. Preserved {} modified files or shortcuts.",
                    preserved_resources.len()
                )
            } else {
                format!(
                    "已卸载，保留了 {} 个修改过的文件或快捷方式。",
                    preserved_resources.len()
                )
            };
        }
        let upgrade_evidence = previous.as_ref().map(|previous| json!({"previous_version":previous.manifest.app_version,"new_version":manifest.app_version,
            "installation_id":receipt.installation_id,"previous_commit_id":previous.receipt.commit_id,"commit_id":receipt.commit_id,
            "journal":session.join("upgrade-journal.jsonl"),"backup":session.join("backup"),"marker":target.join(crate::upgrade::MARKER)}));
        let maintenance_status = if result == TaskResult::Succeeded {
            match args.operation {
                Operation::Modify => Some("ComponentsChanged"),
                Operation::Repair => Some(
                    if repair_evidence
                        .as_ref()
                        .is_some_and(|report| report["fully_repaired"] == true)
                    {
                        "RepairComplete"
                    } else {
                        "RepairPreservedConflicts"
                    },
                ),
                Operation::Recover => Some(if args.recovery_apply {
                    "RecoveryComplete"
                } else {
                    "RecoveryChecked"
                }),
                _ => None,
            }
        } else {
            None
        };
        let mut report = json!({"task_id":task,"operation":plan.operation,"result":result,"exit_code":child_exit,"coordinator_exit_code":code,"message":message,"backend_terminal_detail":backend_detail,"preserved_resources":preserved_resources,"upgrade":upgrade_evidence,
        "repair":repair_evidence,"recovery":recovery_evidence,"modify":modify_evidence,"maintenance_status":maintenance_status,
        "selected_components":plan.selected_components,"selection_source":plan.selection_source,"payload_file_count":plan.payload_file_count,"payload_bytes":plan.payload_bytes,
        "backend_pid":backend_guard.child.id(),"product_lock_retained":child_exit.is_none(),
        "elapsed_ms":started.elapsed().as_millis(),"observation_timeout_secs":args.observation_timeout_secs,"install_dir":target,"session_dir":session,"report_path":report_path,"diagnostic":diagnostic,"events":events,"cancellation":cancellation,"protocol_fault":backend_events.protocol_fault(),"signed":false});
        if args.operation == Operation::Recover {
            // This wrapper plan does not describe the interrupted transaction's
            // selection. Recovery evidence identifies the original frozen plan.
            let fields = report.as_object_mut().expect("report is an object");
            for field in [
                "selected_components",
                "selection_source",
                "payload_file_count",
                "payload_bytes",
            ] {
                fields.remove(field);
            }
        }
        let saved_report =
            persist_and_publish_report(&mut backend_guard, &report, &mut std::io::stdout().lock())?;
        if matches!(code, 3 | 4) {
            log::error!(
                target: "avelin::coordinator",
                "task {task} reported {result} (exit code {code}); session {}; report {}; {message}",
                session.display(), report_path.display()
            );
        }
        let detail = task_diagnostics::terminal_detail(
            &message,
            diagnostic.as_ref(),
            Some(&saved_report),
            args.english,
        );
        notify_finished(&hooks, result, &detail);
        Ok(code)
    })();
    execution.context(UnconfirmedExecution)
}

fn verify_recovery_outcome(
    report: &avelin_engine::RecoveryReport,
    verified: &avelin_engine::RecoveryInspection,
    apply: bool,
) -> Result<()> {
    ensure!(
        verified.verified && verified.original_task_id == report.original_task_id,
        "recovered state could not be independently verified"
    );
    ensure!(
        !apply || verified.decision == "already_recovered",
        "recovery reported success without independently verified completion evidence"
    );
    Ok(())
}

fn persist_and_publish_report(
    guard: &mut BackendGuard,
    report: &Value,
    output: &mut impl Write,
) -> Result<PathBuf> {
    if let Some(parent) = guard.report_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&guard.report_path, serde_json::to_vec_pretty(report)?)?;
    // Once the report is persisted, an output pipe error or panic must not make
    // Drop overwrite the verified task result with an Unknown report.
    guard.completed = true;
    record_saved_report(&guard.hooks, &guard.report_path);
    if let Err(error) = writeln!(output, "{report}") {
        log::error!(
            target: "avelin::coordinator",
            "could not write task report to stdout; persisted task result is unchanged: {error}"
        );
    }
    Ok(guard.report_path.clone())
}

fn verify_result(
    target: &Path,
    manifest: &Manifest,
    expected: &Receipt,
    operation: Operation,
    shortcuts: &[ShortcutRecord],
    shortcut_evidence: &Path,
) -> Result<()> {
    if operation == Operation::Install {
        let _target_guard = PinnedDirectory::open(target)?;
        let receipt_input = PinnedFile::open(&target.join(RECEIPT_NAME))?;
        let receipt: Receipt = serde_json::from_slice(&receipt_input.read_bytes()?)?;
        ensure!(
            receipt.schema_version == expected.schema_version
                && receipt.task_id == expected.task_id
                && receipt.installation_id == expected.installation_id
                && receipt.commit_id == expected.commit_id
                && receipt.manifest_sha256 == expected.manifest_sha256
                && receipt.app_id == expected.app_id
                && receipt.app_version == expected.app_version
                && receipt.install_dir == expected.install_dir
                && receipt.user_sid == expected.user_sid
                && receipt.files == expected.files
                && receipt.selected_components == expected.selected_components
                && receipt.previous_commit_id == expected.previous_commit_id,
            "receipt commit mismatch"
        );
        receipt.validate_manifest(manifest)?;
        for f in &receipt.files {
            ensure!(
                hash_file(&target.join(&f.path))? == f.sha256,
                "installed hash mismatch: {}",
                f.path
            );
        }
        let uninstaller = PinnedFile::open(&target.join("uninstall.exe"))
            .context("uninstaller missing or unsafe")?;
        ensure!(
            manifest.runtime_sha256.as_deref()
                == Some(hash_file(&target.join(".avelin/avelin-app.exe"))?.as_str()),
            "uninstall runtime mismatch"
        );
        let actual_uninstaller_hash = uninstaller.sha256()?;
        let actual_shortcuts = resources::verified_shortcuts(shortcuts, shortcut_evidence)?;
        ensure!(
            receipt.uninstaller_sha256.as_deref() == Some(actual_uninstaller_hash.as_str())
                && receipt.shortcuts == actual_shortcuts,
            "published receipt does not describe the actual uninstall resources"
        );
        let installed = PinnedFile::open(&target.join(".avelin/manifest.json"))?;
        let installed_manifest: Manifest = serde_json::from_slice(&installed.read_bytes()?)?;
        ensure!(
            hash_bytes(&serde_json::to_vec(&installed_manifest)?) == expected.manifest_sha256,
            "installed manifest does not describe the committed package"
        );
        crate::upgrade::verify_registration(
            &crate::upgrade::registration(&manifest.app_id)?
                .context("installed registration missing")?,
            manifest,
            target,
        )?;
        crate::upgrade::ensure_no_pending(target)?;
    } else {
        ensure!(
            !target.join(RECEIPT_NAME).exists()
                && !target.join("uninstall.exe").exists()
                && !target.join(".avelin/avelin-app.exe").exists()
                && !target.join(".avelin/manifest.json").exists(),
            "uninstall resources remain"
        );
        for f in &expected.files {
            resources::verify_removed_or_preserved(&target.join(&f.path), &f.sha256)?;
        }
        for shortcut in shortcuts {
            resources::verify_removed_or_preserved(Path::new(&shortcut.path), &shortcut.sha256)?;
        }
        ensure!(
            crate::upgrade::registration(&manifest.app_id)?.is_none(),
            "uninstall registration remains"
        );
        crate::upgrade::ensure_no_pending(target)?;
    }
    Ok(())
}

fn verify_upgrade_result(
    target: &Path,
    manifest: &Manifest,
    expected: &Receipt,
    previous: &crate::upgrade::PreviousInstallation,
    shortcut_evidence: &Path,
    session: &Path,
) -> Result<()> {
    let receipt = resources::read_receipt(manifest, target, &expected.user_sid)?;
    ensure!(
        receipt.task_id == expected.task_id
            && receipt.commit_id == expected.commit_id
            && receipt.installation_id == previous.receipt.installation_id
            && receipt.previous_commit_id.as_deref() == Some(previous.receipt.commit_id.as_str())
            && receipt.commit_id != previous.receipt.commit_id,
        "upgrade receipt does not bind the new commit to the previous installation"
    );
    ensure!(
        receipt.files == expected.files
            && receipt.selected_components == expected.selected_components,
        "upgrade receipt component selection differs from the plan"
    );
    for item in &receipt.files {
        resources::confirm_owned_hash(&target.join(&item.path), &item.sha256)?;
    }
    resources::confirm_owned_hash(
        &target.join(".avelin/avelin-app.exe"),
        manifest
            .runtime_sha256
            .as_deref()
            .context("new runtime hash missing")?,
    )?;
    resources::confirm_owned_hash(
        &target.join("uninstall.exe"),
        receipt
            .uninstaller_sha256
            .as_deref()
            .context("upgrade did not finalize the uninstaller hash")?,
    )?;
    let installed_manifest = PinnedFile::open(&target.join(".avelin/manifest.json"))?;
    let installed: Manifest = serde_json::from_slice(&installed_manifest.read_bytes()?)?;
    ensure!(
        hash_bytes(&serde_json::to_vec(&installed)?) == expected.manifest_sha256,
        "upgrade installed manifest mismatch"
    );
    ensure!(
        receipt.shortcuts == resources::verified_shortcuts(&previous.shortcuts, shortcut_evidence)?,
        "upgraded shortcut ownership differs from actual creation evidence"
    );
    for (index, item) in previous.backup.iter().enumerate() {
        resources::confirm_owned_hash(
            &session.join(format!("backup/old-{index}.bin")),
            &item.sha256,
        )?;
    }
    crate::upgrade::verify_removed(previous, &receipt.files, target)?;
    crate::upgrade::verify_registration(
        &crate::upgrade::registration(&manifest.app_id)?
            .context("upgraded registration missing")?,
        manifest,
        target,
    )?;
    crate::upgrade::ensure_no_pending(target)?;
    crate::upgrade::verify_journal(
        &session.join("upgrade-journal.jsonl"),
        &expected.task_id,
        "committed",
    )?;
    Ok(())
}

fn wait_for_parent(pid: u32) -> Result<()> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
        fn WaitForSingleObject(handle: *mut std::ffi::c_void, timeout: u32) -> u32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }
    ensure!(pid != std::process::id(), "cannot wait for self");
    unsafe {
        let handle = OpenProcess(0x00100000, 0, pid);
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(87) {
                return Ok(());
            }
            return Err(error).context("cannot inspect resident uninstall frontend");
        }
        let status = WaitForSingleObject(handle, 30_000);
        CloseHandle(handle);
        ensure!(status == 0, "resident uninstall frontend did not exit");
    }
    Ok(())
}

fn observe_pipe_failure(events: &mut BackendEvents, error: &std::io::Error, pending_bytes: usize) {
    if error.kind() == std::io::ErrorKind::InvalidData {
        events.record_fault(format!("invalid IPC framing or envelope: {error}"));
    } else if pending_bytes != 0 {
        events.record_fault(format!(
            "IPC ended with {pending_bytes} unconsumed frame bytes: {error}"
        ));
    } else if error.raw_os_error() != Some(109) {
        // ERROR_BROKEN_PIPE (109) is the ordinary Windows named-pipe EOF.
        // A synthetic BrokenPipe (heartbeat expiry), or another I/O failure,
        // is not evidence that the stream was completely consumed.
        events.record_fault(format!("IPC receive failed before a clean EOF: {error}"));
    }
}

fn notify_progress(_hooks: &Option<UiHooks>, _progress: Progress) {
    #[cfg(feature = "gui")]
    if let Some(h) = _hooks {
        if let Some(snapshot) = avelin_core::TaskSnapshot::progress(_progress) {
            let _ = h.snapshots.send(snapshot);
        }
    }
}
fn notify_finished(_hooks: &Option<UiHooks>, _result: TaskResult, _detail: &str) {
    #[cfg(feature = "gui")]
    if let Some(h) = _hooks {
        let _ = h
            .snapshots
            .send(avelin_core::TaskSnapshot::finished(_result, _detail));
    }
}

#[cfg(feature = "gui")]
fn ui_options(args: &Args, m: Option<&Manifest>) -> avelin_ui::UiOptions {
    let presentation = m
        .and_then(|manifest| manifest.presentation.clone())
        .unwrap_or_default();
    let locale = if args.english {
        avelin_core::presentation::Locale::English
    } else {
        presentation.resolve_locale(user_locale_tag().as_deref())
    };
    let dark = presentation.theme == avelin_core::presentation::ThemeMode::Dark;
    let context = args.component_context.as_ref();
    let components = m
        .map(|manifest| {
            manifest
                .components
                .iter()
                .map(|component| {
                    let text = |values: &std::collections::BTreeMap<String, String>| {
                        (
                            values
                                .get("zh-CN")
                                .cloned()
                                .unwrap_or_else(|| component.id.clone()),
                            values
                                .get("en-US")
                                .cloned()
                                .unwrap_or_else(|| component.id.clone()),
                        )
                    };
                    avelin_ui::UiComponent {
                        id: component.id.clone(),
                        name: text(&component.name),
                        description: text(&component.description),
                        size: manifest
                            .files
                            .iter()
                            .filter(|file| {
                                file.component_id.as_deref() == Some(component.id.as_str())
                            })
                            .map(|file| file.size)
                            .sum(),
                        required: component.required,
                        selected: context
                            .map_or(component.required || component.default_selected, |state| {
                                state.selected.contains(&component.id)
                            }),
                        installed: context
                            .is_some_and(|state| state.installed.contains(&component.id)),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    avelin_ui::UiOptions {
        components,
        maintenance_bytes: 0,
        allow_modify: matches!(args.operation, Operation::Repair | Operation::Modify)
            && m.is_some_and(|manifest| {
                manifest
                    .components
                    .iter()
                    .any(|component| !component.required)
            }),
        migration_details: context
            .map(|state| state.migration_details.clone())
            .unwrap_or_default(),
        migration_confirmation_required: context
            .is_some_and(|state| state.requires_migration_confirmation),
        operation: args.operation.into(),
        maintenance_details: args.maintenance_details.clone(),
        repair_overwrite: args.repair_overwrite,
        previous_version: if args.operation == Operation::Upgrade {
            args.install_dir
                .as_deref()
                .and_then(|target| crate::upgrade::previous_version(target).ok())
        } else {
            None
        },
        desktop_shortcut: args
            .desktop_shortcut
            .unwrap_or(m.is_some_and(|m| m.desktop_shortcut)),
        start_menu_shortcut: args
            .start_menu_shortcut
            .unwrap_or(m.is_some_and(|m| m.start_menu_shortcut)),
        app_name: m
            .map(|x| x.app_name.clone())
            .unwrap_or("Nimbus 笔记 · M0".into()),
        app_version: m.map(|x| x.app_version.clone()).unwrap_or("0.1.0".into()),
        install_dir: args.install_dir.clone().unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join(
                m.map(|m| m.directory_name.as_str())
                    .unwrap_or("AvelinM0Nimbus"),
            )
        }),
        locale,
        dark,
        presentation,
    }
}

#[cfg(feature = "gui")]
fn user_locale_tag() -> Option<String> {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetUserDefaultLocaleName(name: *mut u16, length: i32) -> i32;
        }
        let mut name = [0u16; 85];
        let length = unsafe { GetUserDefaultLocaleName(name.as_mut_ptr(), name.len() as i32) };
        if length > 1 {
            return String::from_utf16(&name[..length as usize - 1]).ok();
        }
    }
    None
}
/// Merge the UI result only after the coordinator has finished observing the Rust worker.
/// Keep the task outcome as context rather than replacing it with a UI error code.
#[cfg(any(feature = "gui", test))]
fn merge_gui_outcome(
    task_code: Option<i32>,
    requested_report: Option<PathBuf>,
    task_locations: Option<TaskLocations>,
    outcome: std::thread::Result<Result<()>>,
) -> Result<i32> {
    merge_gui_outcome_with_dispatcher_status(
        task_code,
        requested_report,
        task_locations,
        outcome,
        false,
    )
}

#[cfg(any(feature = "gui", test))]
fn merge_gui_outcome_with_dispatcher_status(
    task_code: Option<i32>,
    requested_report: Option<PathBuf>,
    task_locations: Option<TaskLocations>,
    outcome: std::thread::Result<Result<()>>,
    dispatcher_join_failed: bool,
) -> Result<i32> {
    let (error, kind) = match outcome {
        Ok(Ok(())) if dispatcher_join_failed => (
            anyhow::anyhow!("GUI dispatcher join failed; worker completion is unconfirmed"),
            "error",
        ),
        Ok(Ok(())) => return Ok(task_code.unwrap_or(0)),
        Ok(Err(error)) => (error, "error"),
        Err(payload) => (
            anyhow::anyhow!("GUI panic: {}", crate::panic_detail(payload.as_ref())),
            "panic",
        ),
    };
    let error = error.context(GuiFailureContext {
        task_exit_code: if dispatcher_join_failed {
            None
        } else {
            task_code
        },
        dispatcher_join_failed,
        ui_failure_kind: kind.into(),
        requested_report,
        task_locations,
    });
    if dispatcher_join_failed || task_code == Some(4) {
        Err(error.context(UnconfirmedExecution))
    } else {
        Err(error)
    }
}

#[cfg(any(feature = "gui", test))]
fn merge_gui_shutdown(
    dispatcher_outcome: std::thread::Result<Option<i32>>,
    requested_report: Option<PathBuf>,
    task_locations: Option<TaskLocations>,
    ui_outcome: std::thread::Result<Result<()>>,
) -> Result<i32> {
    match dispatcher_outcome {
        Ok(task_code) => merge_gui_outcome(task_code, requested_report, task_locations, ui_outcome),
        Err(payload) => {
            let detail = format!(
                "GUI dispatcher panicked during shutdown; worker completion and task result are unconfirmed: {}",
                crate::panic_detail(payload.as_ref())
            );
            log::error!(target: "avelin::coordinator", "{detail}");
            // A failed dispatcher join is not proof that no worker started or
            // that a detached worker finished. Preserve any allocated locations
            // and original UI failure, while retaining Unknown at the host.
            merge_gui_outcome_with_dispatcher_status(
                None,
                requested_report,
                task_locations,
                ui_outcome,
                true,
            )
            .context(detail)
        }
    }
}

#[cfg(feature = "gui")]
fn launch_installed(manifest: &Manifest, target: &Path, locations: &TaskLocations) -> Result<u32> {
    let user_sid = transport::current_user_sid()?;
    let _lock = AppLock::acquire(&manifest.app_id, &user_sid)?;
    let entry = resources::verified_entry(manifest, target, &user_sid, &locations.task_id)?;
    let entry_hash = entry.sha256()?;
    let directory = PinnedDirectory::open(&locations.session_dir)?;
    ensure!(
        !directory.path().join("launch.json").exists(),
        "this task already recorded a launch"
    );
    use std::os::windows::process::CommandExt;
    let child = Command::new(entry.path())
        .current_dir(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
        .context("launch verified installed entry")?;
    let pid = child.id();
    let record = json!({"schema_version":1, "task_id":locations.task_id, "status":"started", "process_id":pid,
        "entry_path":entry.path(), "entry_sha256":entry_hash, "install_dir":target, "arguments":[], "user_sid":user_sid});
    // Once CreateProcess succeeds the UI must not offer an automatic retry merely
    // because evidence writing fails. The application process is never killed.
    if let Err(error) = execution_files::create_file(
        &directory,
        "launch.json",
        &serde_json::to_vec_pretty(&record)?,
    ) {
        log::error!(target: "avelin::coordinator", "application started as PID {pid}, but launch evidence could not be saved: {error:#}");
    }
    Ok(pid)
}

#[cfg(feature = "gui")]
fn run_gui(mut args: Args, backend: Arc<PinnedFile>, manifest: Manifest) -> Result<i32> {
    use avelin_core::protocol::TaskResponse;
    use avelin_ui::{UiBridge, UiCommand, UiPhase, UiSnapshot};
    let mut options = ui_options(&args, Some(&manifest));
    options.maintenance_bytes = fs::metadata(backend.path())?
        .len()
        .checked_add(fs::metadata(std::env::current_exe()?)?.len())
        .and_then(|bytes| {
            bytes.checked_add(serde_json::to_vec_pretty(&manifest).ok()?.len() as u64)
        })
        .context("maintenance size overflow")?;
    // Diagnostics and the page use the same resolved language, including auto.
    args.english = options.locale == avelin_core::presentation::Locale::English;
    let requested_report = args.report.clone();
    let (commands, receiver) = std::sync::mpsc::channel();
    let (sender, snapshots) = std::sync::mpsc::channel();
    let (response_sender, responses) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dispatcher_shutdown = shutdown.clone();
    let dispatcher_cancel = cancel.clone();
    let task_locations = SharedTaskLocations::default();
    let dispatcher_locations = task_locations.clone();
    let dispatcher = std::thread::spawn(move || {
        let mut started = false;
        let mut backend = Some(backend);
        let mut worker: Option<std::thread::JoinHandle<i32>> = None;
        let mut task_code = None;
        let mut started_target = None;
        let mut launch_pending = false;
        let mut launched = false;
        while !dispatcher_shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            if worker.as_ref().is_some_and(|worker| worker.is_finished()) {
                task_code = worker.take().map(|worker| worker.join().unwrap_or(4));
            }
            if launch_pending && task_code.is_some() {
                launch_pending = false;
                let result = if task_code == Some(0)
                    && !matches!(args.operation, Operation::Uninstall | Operation::Recover)
                    && !launched
                {
                    task_locations_snapshot(&dispatcher_locations)
                        .context("completed task locations unavailable")
                        .and_then(|locations| {
                            launch_installed(
                                &manifest,
                                started_target
                                    .as_deref()
                                    .context("installed target unavailable")?,
                                &locations,
                            )
                        })
                } else {
                    Err(anyhow::anyhow!(
                        "only a verified successful installation can launch its entry"
                    ))
                };
                let (succeeded, detail) = match result {
                    Ok(pid) => {
                        launched = true;
                        (true, format!("PID {pid}"))
                    }
                    Err(error) => (false, format!("{error:#}")),
                };
                let _ = response_sender.send(TaskResponse::LaunchResult { succeeded, detail });
            }
            let cmd = match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(cmd) => cmd,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if dispatcher_shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            match cmd {
                UiCommand::Cancel => {
                    if args.operation == Operation::Install {
                        dispatcher_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                        // Cancellation may arrive immediately after a blocked
                        // preflight response. There is no worker to acknowledge
                        // it in that case, so complete it here, exactly once.
                        if backend.is_some() && worker.is_none() && task_code.is_none() {
                            started = true;
                            task_code = Some(2);
                            let _ = sender.send(avelin_core::TaskSnapshot::finished(
                                TaskResult::Cancelled,
                                if args.english {
                                    "Cancelled before starting; no installation changes were made."
                                } else {
                                    "已在开始前取消，未修改安装内容。"
                                },
                            ));
                        }
                    }
                }
                UiCommand::LaunchInstalled => {
                    if started
                        && !launched
                        && !matches!(args.operation, Operation::Uninstall | Operation::Recover)
                    {
                        launch_pending = true;
                    } else {
                        let _ = response_sender.send(TaskResponse::LaunchResult {
                            succeeded: false,
                            detail: "launch is unavailable for this operation".into(),
                        });
                    }
                }
                UiCommand::Start {
                    install_dir,
                    desktop_shortcut,
                    start_menu_shortcut,
                    selected_components,
                    operation,
                    migration_confirmed,
                } if !started => {
                    started = true;
                    let requested_operation = match operation.unwrap_or(args.operation.into()) {
                        avelin_core::Operation::Install => Operation::Install,
                        avelin_core::Operation::Upgrade => Operation::Upgrade,
                        avelin_core::Operation::Modify => Operation::Modify,
                        avelin_core::Operation::Repair => Operation::Repair,
                        avelin_core::Operation::Recover => Operation::Recover,
                        avelin_core::Operation::Uninstall => Operation::Uninstall,
                    };
                    let maintenance_switch =
                        matches!(args.operation, Operation::Repair | Operation::Modify)
                            && matches!(requested_operation, Operation::Repair | Operation::Modify)
                            && manifest
                                .components
                                .iter()
                                .any(|component| !component.required);
                    let missing_confirmation = args
                        .component_context
                        .as_ref()
                        .is_some_and(|state| state.requires_migration_confirmation)
                        && !migration_confirmed;
                    if (requested_operation != args.operation && !maintenance_switch)
                        || missing_confirmation
                    {
                        task_code = Some(3);
                        let _ = sender.send(avelin_core::TaskSnapshot::finished(TaskResult::Failed,
                            "operation change is not permitted or legacy migration requires explicit confirmation"));
                        continue;
                    }
                    args.operation = requested_operation;
                    if args.operation != Operation::Install
                        && !args
                            .install_dir
                            .as_ref()
                            .is_some_and(|path| resources::same_path(path, &install_dir))
                    {
                        task_code = Some(3);
                        let _ = sender.send(avelin_core::TaskSnapshot::finished(
                            TaskResult::Failed,
                            "the directory of an existing installation cannot be changed",
                        ));
                        continue;
                    }
                    started_target = Some(install_dir.clone());
                    let mut args = args.clone();
                    args.install_dir = Some(install_dir);
                    if args.operation != Operation::Repair {
                        args.repair_overwrite = false;
                    }
                    args.components = if matches!(
                        args.operation,
                        Operation::Install | Operation::Upgrade | Operation::Modify
                    ) {
                        selected_components.map(|ids| ids.join(","))
                    } else {
                        None
                    };
                    if matches!(args.operation, Operation::Install | Operation::Upgrade) {
                        args.desktop_shortcut = Some(desktop_shortcut);
                        args.start_menu_shortcut = Some(start_menu_shortcut);
                    }
                    if args.operation == Operation::Recover {
                        // This command arrives only after the recovery page's
                        // explicit confirmation. Silent checks never take it.
                        args.recovery_apply = true;
                    }
                    // No worker, task journal or target mutation exists at this point.
                    // Only this pre-start response may return the UI to confirmation.
                    let inspection = crate::preflight::inspect_with_backend(
                        &args,
                        &manifest,
                        backend.take().expect("backend available before start"),
                    )
                    .map(|(pin, usage)| {
                        backend = Some(pin);
                        usage
                    });
                    for queued in receiver.try_iter() {
                        if matches!(queued, UiCommand::Cancel)
                            && args.operation == Operation::Install
                        {
                            dispatcher_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    if dispatcher_shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    if dispatcher_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        task_code = Some(2);
                        let _ = sender.send(avelin_core::TaskSnapshot::finished(
                            TaskResult::Cancelled,
                            if args.english {
                                "Cancelled before starting; no installation changes were made."
                            } else {
                                "已在开始前取消，未修改安装内容。"
                            },
                        ));
                        continue;
                    }
                    match inspection {
                        Ok(usage) if !usage.is_clear() => {
                            started = false;
                            let _ = response_sender.send(TaskResponse::PreflightBlocked {
                                detail: crate::preflight::detail(&usage, args.english),
                            });
                            continue;
                        }
                        Err(error) => {
                            task_code = Some(3);
                            let _ = sender.send(avelin_core::TaskSnapshot::finished(
                                TaskResult::Failed,
                                format!("AVELIN_TASK_NOT_STARTED\n{error:#}"),
                            ));
                            continue;
                        }
                        Ok(_) => {}
                    }
                    // A retained dispatcher clone would prevent removal of the installed uninstaller.
                    let backend = backend.take().expect("one backend per task");
                    let manifest = manifest.clone();
                    let snapshots = sender.clone();
                    let hooks = UiHooks {
                        snapshots: sender.clone(),
                        cancel: dispatcher_cancel.clone(),
                        shutdown: dispatcher_shutdown.clone(),
                        task_locations: dispatcher_locations.clone(),
                    };
                    let worker_locations = dispatcher_locations.clone();
                    let english = args.english;
                    worker = Some(std::thread::spawn(move || {
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                run(args, backend, manifest, Some(hooks))
                            }));
                        let (code, detail) = match outcome {
                            Ok(Ok(code)) => (code, None),
                            Ok(Err(error)) => {
                                let code = error_exit_code(&error);
                                log::error!(
                                    target: "avelin::coordinator",
                                    "GUI task worker returned an error (exit code {code}): {error:#}"
                                );
                                let detail = match (code == 4, english) {
                                    (true, true) => format!(
                                        "AVELIN_RESULT_UNKNOWN\nResult unconfirmed; the error does not confirm cleanup: {error:#}"
                                    ),
                                    (true, false) => format!(
                                        "AVELIN_RESULT_UNKNOWN\n结果未确认，错误不代表已清理：{error:#}"
                                    ),
                                    (false, true) => format!(
                                        "AVELIN_TASK_NOT_STARTED\nTask did not start: {error:#}"
                                    ),
                                    (false, false) => {
                                        format!("AVELIN_TASK_NOT_STARTED\n任务未启动：{error:#}")
                                    }
                                };
                                (code, Some(detail))
                            }
                            Err(payload) => {
                                let detail = crate::panic_detail(payload.as_ref());
                                log::error!(
                                    target: "avelin::coordinator",
                                    "GUI task worker panicked; task result is unconfirmed: {detail}"
                                );
                                (
                                    4,
                                    Some(if english {
                                        format!(
                                            "AVELIN_RESULT_UNKNOWN\nCoordinator panicked; result and cleanup are unconfirmed: {detail}"
                                        )
                                    } else {
                                        format!(
                                            "AVELIN_RESULT_UNKNOWN\n执行协调层发生异常；结果和清理状态未确认：{detail}"
                                        )
                                    }),
                                )
                            }
                        };
                        if let Some(detail) = detail {
                            // The worker's guard may have saved an Unknown fallback.
                            // Never present the merely requested report as persisted.
                            let saved_report = task_locations_snapshot(&worker_locations)
                                .and_then(|locations| locations.saved_report);
                            let detail = format!(
                                "{detail}\n{}",
                                task_diagnostics::report_guidance(saved_report.as_deref(), english)
                            );
                            let _ = snapshots.send(UiSnapshot {
                                phase: if code == 4 {
                                    UiPhase::Unknown
                                } else {
                                    UiPhase::Failed
                                },
                                completed: 0,
                                total: None,
                                detail,
                            });
                        }
                        code
                    }));
                }
                _ => {}
            }
        }
        dispatcher_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        dispatcher_shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        // Worker observation is bounded; the worker must publish its report and drop
        // its guard before main may exit. Blocking local disk I/O is not preempted.
        worker
            .map(|worker| {
                worker.join().unwrap_or_else(|payload| {
                    log::error!(
                        target: "avelin::coordinator",
                        "GUI task worker join failed; task result is unconfirmed: {}",
                        crate::panic_detail(payload.as_ref())
                    );
                    4
                })
            })
            .or(task_code)
    });
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        avelin_ui::run_connected(
            options,
            UiBridge {
                commands,
                snapshots,
                responses,
            },
        )
        .map_err(anyhow::Error::from)
    }));
    // Preserve the original UI cause even if joining the dispatcher also fails.
    // The dispatcher has not returned yet, so these records make no task claim.
    match &outcome {
        Ok(Err(error)) => log::error!(
            target: "avelin::coordinator",
            "GUI returned an error; awaiting coordinator task outcome: {error:#}"
        ),
        Err(payload) => log::error!(
            target: "avelin::coordinator",
            "GUI panicked; awaiting coordinator task outcome: {}",
            crate::panic_detail(payload.as_ref())
        ),
        Ok(Ok(())) => {}
    }
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let dispatcher_outcome = dispatcher.join();
    merge_gui_shutdown(
        dispatcher_outcome,
        requested_report,
        task_locations_snapshot(&task_locations),
        outcome,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installed_maintenance_rejects_external_inputs_and_ambiguous_silent_launch() {
        for tail in [
            vec!["--silent"],
            vec![
                "--gui",
                "--backend",
                "other.exe",
                "--manifest",
                "other.json",
            ],
            vec!["--silent", "--operation", "uninstall"],
            vec!["--gui", "--engine-fault", "rollback"],
        ] {
            let mut argv = vec!["avelin", "--maintenance", "--install-dir", r"C:\Example"];
            argv.extend(tail);
            let args = Args::try_parse_from(argv).unwrap();
            assert!(validate_launch(&args).is_err());
        }
        for tail in [
            vec!["--silent", "--operation", "repair"],
            vec!["--silent", "--operation", "modify", "--components", "core"],
            vec!["--silent", "--operation", "recover"],
        ] {
            let mut argv = vec!["avelin", "--maintenance", "--install-dir", r"C:\Example"];
            argv.extend(tail);
            validate_launch(&Args::try_parse_from(argv).unwrap()).unwrap();
        }
    }

    #[test]
    fn entry_rejects_non_rust_workers_before_creating_installation_state() {
        let directory =
            execution_files::create_private_session(&std::env::temp_dir(), "avelin-worker-test")
                .unwrap();
        let folder = directory.path().to_path_buf();
        let backend =
            execution_files::create_file(&directory, "external.exe", b"inert executable").unwrap();
        let manifest = execution_files::create_file(
            &directory,
            "manifest.json",
            include_bytes!("../../../docs/schemas/examples/manifest.json"),
        )
        .unwrap();
        let target = folder.join("installed");
        let report = folder.join("report.json");
        let args = Args::try_parse_from([
            "avelin-app",
            "--silent",
            "--backend",
            backend.path().to_str().unwrap(),
            "--manifest",
            manifest.path().to_str().unwrap(),
            "--install-dir",
            target.to_str().unwrap(),
            "--report",
            report.to_str().unwrap(),
        ])
        .unwrap();
        let error = entry(args).unwrap_err();
        assert!(error.to_string().contains("only a packaged Rust worker"));
        assert!(!target.exists());
        assert!(!report.exists());
        assert_eq!(fs::read_dir(&folder).unwrap().count(), 2);
        drop(backend);
        drop(manifest);
        drop(directory);
        fs::remove_file(folder.join("external.exe")).unwrap();
        fs::remove_file(folder.join("manifest.json")).unwrap();
        fs::remove_dir(folder).unwrap();
    }

    #[test]
    fn uninstall_acceptance_only_allows_removal_and_commit_pauses() {
        let parse = |extra: &[&str]| {
            let mut command = vec![
                "avelin-app",
                "--silent",
                "--operation",
                "uninstall",
                "--install-dir",
                r"C:\installed",
            ];
            if extra.contains(&"--ui-demo") {
                command.retain(|argument| *argument != "--silent");
            }
            command.extend_from_slice(extra);
            Args::try_parse_from(command).unwrap()
        };
        assert!(validate_launch(&parse(&["--engine-pause", "after-first-remove"])).is_ok());
        assert!(validate_launch(&parse(&["--engine-pause", "after-commit"])).is_ok());
        for point in [
            "prepared",
            "after-first-publish",
            "recovery-after-first-mutation",
        ] {
            assert!(validate_launch(&parse(&["--engine-pause", point])).is_err());
        }
        assert!(validate_launch(&parse(&["--engine-fault", "before-commit"])).is_err());
        assert!(
            validate_launch(&parse(&[
                "--engine-pause",
                "after-first-remove",
                "--engine-fault",
                "before-commit"
            ]))
            .is_err()
        );
        assert!(
            validate_launch(&parse(&[
                "--engine-pause",
                "after-first-remove",
                "--ui-demo"
            ]))
            .is_err()
        );
    }

    #[test]
    fn recovery_apply_requires_independently_verified_completion() {
        let report = avelin_engine::RecoveryReport {
            schema_version: 1,
            original_task_id: "original-task".into(),
            decision: "rollback".into(),
            applied: true,
            verified: true,
            restored: vec![],
            preserved: vec![],
        };
        let mut verified = report.clone();
        for pending in ["rollback", "confirm_committed"] {
            verified.decision = pending.into();
            assert!(verify_recovery_outcome(&report, &verified, true).is_err());
            assert!(verify_recovery_outcome(&report, &verified, false).is_ok());
        }
        verified.decision = "already_recovered".into();
        assert!(verify_recovery_outcome(&report, &verified, true).is_ok());
        assert!(verify_recovery_outcome(&report, &verified, false).is_ok());
        verified.verified = false;
        assert!(verify_recovery_outcome(&report, &verified, true).is_err());
        verified.verified = true;
        verified.original_task_id = "different-task".into();
        assert!(verify_recovery_outcome(&report, &verified, true).is_err());
    }

    #[test]
    fn sealed_product_lock_allows_worker_reads_but_denies_mutation() {
        let folder = std::env::temp_dir().join(format!("avelin-lock-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&folder).unwrap();
        let path = folder.join("product.lock");
        let mut writer = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        writeln!(writer, "{}", std::process::id()).unwrap();
        let mut lock = AppLock {
            _file: Some(writer),
            path: path.clone(),
            retain: false,
        };
        lock.seal(&folder.join("session"), &folder.join("installed"))
            .unwrap();
        // Match the worker's restrictive read sharing, not fs::read's defaults.
        let mut reader = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let mut contents = String::new();
        std::io::Read::read_to_string(&mut reader, &mut contents).unwrap();
        assert!(contents.contains("\nsession="));
        assert!(OpenOptions::new().write(true).open(&path).is_err());
        assert!(fs::remove_file(&path).is_err());
        drop(reader);
        drop(lock);
        assert!(!path.exists());
        fs::remove_dir(folder).unwrap();
    }

    #[test]
    fn registration_query_only_treats_missing_key_as_absent() {
        assert!(registry_open_result(0).unwrap());
        assert!(!registry_open_result(2).unwrap());
        for status in [5, 87, 1018] {
            assert!(registry_open_result(status).is_err());
        }
        assert!(registry_key_exists("bad\0path").is_err());
        assert!(
            !registry_key_exists(&format!(
                "Software\\Avelin\\Absent-{}",
                uuid::Uuid::new_v4().simple()
            ))
            .unwrap()
        );
        assert!(registry_key_exists("Software").unwrap());
    }

    #[test]
    fn uninstall_requires_bound_path_and_recorded_component_hash() {
        let directory =
            execution_files::create_private_session(&std::env::temp_dir(), "avelin-component-test")
                .unwrap();
        let path = directory.path().to_path_buf();
        let backend =
            execution_files::create_file(&directory, "uninstall.exe", b"inert test bytes").unwrap();
        let other =
            execution_files::create_file(&directory, "other.exe", b"inert test bytes").unwrap();
        let mut receipt = Receipt {
            selected_components: vec![],
            schema_version: SCHEMA_VERSION,
            app_id: "dev.avelin.component-test".into(),
            app_version: "0.1.0".into(),
            installation_id: "test".into(),
            task_id: "test".into(),
            commit_id: "test".into(),
            install_dir: path.to_string_lossy().into_owned(),
            manifest_sha256: String::new(),
            user_sid: String::new(),
            files: Vec::new(),
            uninstaller_sha256: None,
            shortcuts: Vec::new(),
            previous_commit_id: None,
        };
        assert!(validate_uninstaller(&path, &backend, &receipt).is_err());
        receipt.uninstaller_sha256 = Some(backend.sha256().unwrap());
        validate_uninstaller(&path, &backend, &receipt).unwrap();
        assert!(validate_uninstaller(&path, &other, &receipt).is_err());
        receipt.uninstaller_sha256 = Some(hash_bytes(b"different bytes"));
        assert!(validate_uninstaller(&path, &backend, &receipt).is_err());
        drop(other);
        drop(backend);
        drop(directory);
        fs::remove_file(path.join("other.exe")).unwrap();
        fs::remove_file(path.join("uninstall.exe")).unwrap();
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn ordinary_pipe_eof_preserves_terminal_but_partial_or_invalid_frames_do_not() {
        for (error, pending, expected_success) in [
            (std::io::Error::from_raw_os_error(109), 0, true),
            (std::io::Error::from_raw_os_error(109), 7, false),
            (
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid sequence"),
                0,
                false,
            ),
            (
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "heartbeat expired"),
                0,
                false,
            ),
            (
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "read denied"),
                0,
                false,
            ),
        ] {
            let mut state = BackendEvents::default();
            state.observe(&json!({"kind":"finish", "result":"Succeeded", "code":0}));
            observe_pipe_failure(&mut state, &error, pending);
            assert_eq!(state.outcome(Some(0), true).is_ok(), expected_success);
        }
    }

    #[test]
    fn exited_producer_cannot_hide_queued_conflict_or_progress_behind_finish() {
        use backend_events::TaskResult;
        let terminal = json!({"kind":"finish", "result":"Succeeded", "code":0});
        for (trailing, succeeds) in [
            (None, true),
            (Some(terminal.clone()), true),
            (Some(json!({"kind":"cancel_received"})), true),
            (
                Some(json!({"kind":"finish", "result":"Failed", "code":3})),
                false,
            ),
            (
                Some(json!({"kind":"finish", "result":"Other", "code":4})),
                false,
            ),
            (
                Some(json!({"kind":"progress", "phase":"running", "completed":1, "total":1})),
                false,
            ),
        ] {
            let name = format!(r"\\.\pipe\avelin-m0-event-test-{}", uuid::Uuid::new_v4());
            let token = "0123456789abcdef0123456789abcdef";
            let server = transport::PipeServer::bind(&name, token, "event-test").unwrap();
            let first = terminal.clone();
            let expected_len = 1 + usize::from(trailing.is_some());
            let producer = std::thread::spawn(move || {
                let mut client = transport::Connection::connect(
                    &name,
                    token,
                    "event-test",
                    Duration::from_secs(2),
                )
                .unwrap();
                client.send(first).unwrap();
                if let Some(event) = trailing {
                    client.send(event).unwrap();
                }
            });
            let mut connection = Some(
                server
                    .accept_from(Duration::from_secs(2), std::process::id())
                    .unwrap(),
            );
            producer.join().unwrap(); // All messages were queued before observation.
            let mut state = BackendEvents::default();
            let mut raw_events = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !observation_complete(Some(0), connection.is_some()) {
                assert!(Instant::now() < deadline, "event stream did not close");
                let pipe = connection.as_mut().unwrap();
                match pipe.recv_timeout(Duration::from_millis(50)) {
                    Ok(event) => {
                        state.observe(&event);
                        raw_events.push(event);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(error) => {
                        observe_pipe_failure(&mut state, &error, pipe.pending_input_len());
                        connection = None;
                    }
                }
            }
            assert_eq!(
                raw_events.len(),
                expected_len,
                "must retain rejected payloads too"
            );
            let result = state.outcome(Some(0), connection.is_none());
            if succeeds {
                assert_eq!(result, Ok(TaskResult::Succeeded));
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[test]
    fn failed_cancel_write_still_allows_queued_terminal_to_be_observed() {
        let name = format!(
            r"\\.\pipe\avelin-m0-cancel-race-test-{}",
            uuid::Uuid::new_v4()
        );
        let token = "0123456789abcdef0123456789abcdef";
        let server = transport::PipeServer::bind(&name, token, "cancel-race").unwrap();
        let producer = std::thread::spawn(move || {
            let mut client =
                transport::Connection::connect(&name, token, "cancel-race", Duration::from_secs(2))
                    .unwrap();
            client
                .send(json!({"kind":"finish", "result":"Succeeded", "code":0}))
                .unwrap();
        });
        let mut pipe = server
            .accept_from(Duration::from_secs(2), std::process::id())
            .unwrap();
        producer.join().unwrap();
        assert!(pipe.send_cancel().is_err());
        let mut state = BackendEvents::default();
        state.observe(&pipe.recv_timeout(Duration::from_secs(2)).unwrap());
        let error = pipe.recv_timeout(Duration::from_secs(2)).unwrap_err();
        observe_pipe_failure(&mut state, &error, pipe.pending_input_len());
        assert_eq!(
            state.outcome(Some(0), true),
            Ok(backend_events::TaskResult::Succeeded)
        );
    }

    fn test_hooks() -> (Option<UiHooks>, SharedTaskLocations) {
        let locations = SharedTaskLocations::default();
        let hooks = UiHooks {
            #[cfg(feature = "gui")]
            snapshots: std::sync::mpsc::channel().0,
            cancel: Default::default(),
            shutdown: Default::default(),
            task_locations: locations.clone(),
        };
        (Some(hooks), locations)
    }

    fn sample_locations() -> TaskLocations {
        TaskLocations {
            task_id: "synthetic-task".into(),
            session_dir: PathBuf::from(r"C:\测试\session"),
            report_path: PathBuf::from(r"C:\测试\session\report.json"),
            saved_report: Some(PathBuf::from(r"C:\测试\session\report.json")),
        }
    }

    fn exited_backend_guard(
        folder: &Path,
        report_path: PathBuf,
        hooks: Option<UiHooks>,
    ) -> BackendGuard {
        use std::os::windows::process::CommandExt;

        let lock_path = folder.join("product.lock");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .unwrap();
        // Null stdin gives the existing fixture EOF. It exits naturally without
        // installation changes or process termination.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "coordinator::tests::backend_guard_fixture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();
        assert!(child.wait().unwrap().success());
        BackendGuard {
            child,
            lock: AppLock {
                _file: Some(file),
                path: lock_path,
                retain: false,
            },
            task: "persisted-report-test".into(),
            target: folder.join("target"),
            session: folder.to_path_buf(),
            report_path,
            hooks,
            completed: false,
            english: false,
            fault_after_files: None,
            last_progress: None,
            last_work_progress: None,
        }
    }

    #[test]
    fn default_and_explicit_locations_are_absolute_without_claiming_a_saved_report() {
        for requested_report in [None, Some(PathBuf::from("target/relative-report.json"))] {
            let (hooks, shared) = test_hooks();
            assert!(task_locations_snapshot(&shared).is_none());
            // A relative session models an unusual relative TEMP setting without
            // changing process-global environment variables or the current dir.
            let session = PathBuf::from("target/relative-session");
            let locations = allocate_task_locations(
                "allocated-task".into(),
                session.clone(),
                requested_report.as_deref(),
                &hooks,
            )
            .unwrap();
            assert_eq!(locations.session_dir, std::path::absolute(session).unwrap());
            let expected_report = requested_report
                .clone()
                .unwrap_or_else(|| locations.session_dir.join("report.json"));
            assert_eq!(
                locations.report_path,
                std::path::absolute(expected_report).unwrap()
            );
            assert!(locations.saved_report.is_none());
            assert_eq!(task_locations_snapshot(&shared), Some(locations.clone()));
            let error = merge_gui_outcome(
                Some(3),
                requested_report.clone(),
                task_locations_snapshot(&shared),
                Ok(Err(anyhow::anyhow!("UI failed after session allocation"))),
            )
            .unwrap_err();
            let context = error.downcast_ref::<GuiFailureContext>().unwrap();
            assert_eq!(context.requested_report, requested_report);
            assert_eq!(context.task_locations.as_ref(), Some(&locations));
        }
    }

    #[test]
    fn preflight_failure_and_not_started_do_not_invent_task_locations() {
        let (_, shared) = test_hooks();
        for code in [None, Some(3)] {
            let error = merge_gui_outcome(
                code,
                None,
                task_locations_snapshot(&shared),
                Ok(Err(anyhow::anyhow!("UI failed before session allocation"))),
            )
            .unwrap_err();
            let context = error.downcast_ref::<GuiFailureContext>().unwrap();
            assert_eq!(context.task_exit_code, code);
            assert!(context.task_locations.is_none());
        }
        // Even an attempted saved-report update cannot invent an allocated task.
        let (hooks, shared) = test_hooks();
        record_saved_report(&hooks, Path::new(r"C:\not-created.json"));
        assert!(task_locations_snapshot(&shared).is_none());
    }

    #[test]
    fn poisoned_location_bookkeeping_does_not_change_a_persisted_task_result() {
        let folder = std::env::temp_dir().join(format!(
            "avelin-poisoned-locations-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&folder).unwrap();
        let (hooks, shared) = test_hooks();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = shared.lock().unwrap();
            panic!("synthetic bookkeeping poison");
        }));
        assert!(poisoned.is_err());
        let locations =
            allocate_task_locations("persisted-report-test".into(), folder.clone(), None, &hooks)
                .unwrap();
        let mut guard = exited_backend_guard(&folder, locations.report_path.clone(), hooks);
        let report = json!({"result":"Succeeded","coordinator_exit_code":0});
        let saved_report =
            persist_and_publish_report(&mut guard, &report, &mut Vec::new()).unwrap();
        assert_eq!(saved_report, locations.report_path);
        drop(guard);
        assert_eq!(
            fs::read(&locations.report_path).unwrap(),
            serde_json::to_vec_pretty(&report).unwrap()
        );
        let snapshot = task_locations_snapshot(&shared).unwrap();
        assert_eq!(snapshot.saved_report, Some(locations.report_path.clone()));
        assert_eq!(
            merge_gui_outcome(Some(0), None, Some(snapshot), Ok(Ok(()))).unwrap(),
            0
        );
        assert!(!folder.join("product.lock").exists());
        fs::remove_file(&locations.report_path).unwrap();
        fs::remove_dir(&folder).unwrap();
    }

    #[test]
    fn observation_budget_is_explicit_and_bounded() {
        let args = Args::try_parse_from(["avelin-app", "--install-dir", r"C:\test"]).unwrap();
        assert_eq!(args.observation_timeout_secs, 600);
        for invalid in ["0", "3601"] {
            assert!(
                Args::try_parse_from([
                    "avelin-app",
                    "--install-dir",
                    r"C:\test",
                    "--observation-timeout-secs",
                    invalid
                ])
                .is_err()
            );
        }
        assert!(
            Args::try_parse_from([
                "avelin-app",
                "--install-dir",
                r"C:\test",
                "--cancel-after-ms",
                "1",
                "--cancel-after-files",
                "1"
            ])
            .is_err()
        );
    }

    #[test]
    fn launch_defaults_require_gui_or_an_explicit_cli_destination() {
        let no_args = Args::try_parse_from(["avelin-app"]).unwrap();
        assert_eq!(requests_gui(&no_args), cfg!(feature = "gui"));
        assert_eq!(validate_launch(&no_args).is_ok(), cfg!(feature = "gui"));
        let cli = Args::try_parse_from([
            "avelin-app",
            "--install-dir",
            r"C:\test path",
            "--report",
            "report.json",
        ])
        .unwrap();
        assert!(!requests_gui(&cli));
        validate_launch(&cli).unwrap();
    }

    #[test]
    fn demo_accepts_manifest_preview_but_rejects_a_backend_before_execution() {
        let preview =
            Args::try_parse_from(["avelin-app", "--ui-demo", "--manifest", "preview.json"])
                .unwrap();
        assert_eq!(validate_launch(&preview).is_ok(), cfg!(feature = "gui"));
        let mut ambiguous = preview;
        ambiguous.backend = Some("backend.exe".into());
        assert!(
            validate_launch(&ambiguous)
                .unwrap_err()
                .to_string()
                .contains("cannot execute --backend")
        );
    }

    #[test]
    fn uninstall_rejects_demo_and_cancellation_before_execution() {
        for extra in [
            vec!["--ui-demo"],
            vec!["--cancel-after-files", "1"],
            vec!["--cancel-after-ms", "1"],
            vec!["--fault-after-files", "1"],
            vec!["--desktop-shortcut", "false"],
        ] {
            let mut argv = vec![
                "avelin-app",
                "--operation",
                "uninstall",
                "--install-dir",
                r"C:\test path",
            ];
            argv.extend(extra);
            let args = Args::try_parse_from(argv).unwrap();
            assert!(validate_launch(&args).is_err());
        }
        let args = Args::try_parse_from([
            "avelin-app",
            "--operation",
            "uninstall",
            "--install-dir",
            r"C:\test path",
            "--silent",
        ])
        .unwrap();
        assert!(!requests_gui(&args));
        validate_launch(&args).unwrap();
    }

    #[test]
    fn silent_launch_and_shortcut_overrides_are_explicit() {
        let args = Args::try_parse_from([
            "avelin-app",
            "--silent",
            "--desktop-shortcut=false",
            "--start-menu-shortcut",
        ])
        .unwrap();
        validate_launch(&args).unwrap();
        assert!(!requests_gui(&args));
        assert_eq!(args.desktop_shortcut, Some(false));
        assert_eq!(args.start_menu_shortcut, Some(true));
        for incompatible in ["--gui", "--ui-demo"] {
            assert!(Args::try_parse_from(["avelin-app", "--silent", incompatible]).is_err());
        }
        assert!(
            Args::try_parse_from(["avelin-app", "--silent", "--desktop-shortcut=maybe"]).is_err()
        );
        let missing_uninstall_target =
            Args::try_parse_from(["avelin-app", "--silent", "--operation", "uninstall"]).unwrap();
        assert!(validate_launch(&missing_uninstall_target).is_err());
        let gui_uninstall = Args::try_parse_from([
            "avelin-app",
            "--gui",
            "--operation",
            "uninstall",
            "--install-dir",
            r"C:\test",
        ])
        .unwrap();
        assert_eq!(
            validate_launch(&gui_uninstall).is_ok(),
            cfg!(feature = "gui")
        );
    }

    #[test]
    fn maintenance_flags_require_explicit_operations_and_never_infer_apply() {
        for extra in [
            vec!["--repair-overwrite"],
            vec!["--recovery-apply"],
            vec!["--recovery-session", r"C:\old-task"],
        ] {
            let mut argv = vec!["avelin-app", "--silent"];
            argv.extend(extra);
            assert!(validate_launch(&Args::try_parse_from(argv).unwrap()).is_err());
        }
        let check = Args::try_parse_from([
            "avelin-app",
            "--silent",
            "--operation",
            "recover",
            "--recovery-session",
            r"C:\old-task",
        ])
        .unwrap();
        validate_launch(&check).unwrap();
        assert!(!check.recovery_apply);
        assert!(!requests_gui(&check));
        let apply = Args::try_parse_from([
            "avelin-app",
            "--silent",
            "--operation",
            "recover",
            "--recovery-session",
            r"C:\old-task",
            "--recovery-apply",
        ])
        .unwrap();
        validate_launch(&apply).unwrap();
        assert!(apply.recovery_apply);
        let repair = Args::try_parse_from([
            "avelin-app",
            "--silent",
            "--operation",
            "repair",
            "--repair-overwrite",
        ])
        .unwrap();
        validate_launch(&repair).unwrap();
        assert!(repair.repair_overwrite);
        for operation in ["repair", "recover"] {
            for extra in ["--desktop-shortcut", "--start-menu-shortcut", "--ui-demo"] {
                let args = Args::try_parse_from([
                    "avelin-app",
                    "--operation",
                    operation,
                    "--install-dir",
                    r"C:\original",
                    extra,
                ])
                .unwrap();
                assert!(validate_launch(&args).is_err());
            }
        }
    }

    #[test]
    fn post_spawn_errors_preserve_unknown_exit_code() {
        let error = anyhow::anyhow!("journal write failed").context(UnconfirmedExecution);
        assert_eq!(error_exit_code(&error), 4);
        assert_eq!(error_exit_code(&anyhow::anyhow!("invalid manifest")), 3);
    }

    #[test]
    fn injected_ui_error_preserves_each_task_result_and_original_detail() {
        // Synthetic UI failure: this does not exercise a graphics driver.
        let detail = "injected UI initialization error";
        let requested_report = PathBuf::from(r"C:\测试\requested-report.json");
        for task_code in [None, Some(0), Some(2), Some(3), Some(4)] {
            let locations = task_code.map(|_| sample_locations());
            let error = merge_gui_outcome(
                task_code,
                Some(requested_report.clone()),
                locations.clone(),
                Ok(Err(anyhow::anyhow!(detail))),
            )
            .unwrap_err();
            assert_eq!(
                error_exit_code(&error),
                if task_code == Some(4) { 4 } else { 3 }
            );
            assert!(format!("{error:#}").contains(detail));
            assert_eq!(error.is::<UnconfirmedExecution>(), task_code == Some(4));
            let context = error.downcast_ref::<GuiFailureContext>().unwrap();
            assert_eq!(context.task_exit_code, task_code);
            assert!(!context.dispatcher_join_failed);
            assert_eq!(context.ui_failure_kind, "error");
            assert_eq!(context.requested_report.as_ref(), Some(&requested_report));
            assert_eq!(context.task_locations, locations);
            assert_eq!(
                serde_json::to_value(context).unwrap()["task_exit_code"],
                json!(task_code)
            );
        }
    }

    #[test]
    fn normal_ui_keeps_each_task_exit_code() {
        for code in [0, 2, 3, 4] {
            assert_eq!(
                merge_gui_outcome(Some(code), None, Some(sample_locations()), Ok(Ok(()))).unwrap(),
                code
            );
        }
        assert_eq!(merge_gui_outcome(None, None, None, Ok(Ok(()))).unwrap(), 0);
    }

    #[test]
    fn injected_ui_panic_keeps_each_task_result_and_panic_detail() {
        // Inject both supported Rust panic payload forms without opening a window.
        for task_code in [None, Some(0), Some(2), Some(3), Some(4)] {
            for payload in [
                Box::new("injected UI panic") as Box<dyn std::any::Any + Send>,
                Box::new(String::from("injected UI panic")),
            ] {
                let locations = task_code.map(|_| sample_locations());
                let error = merge_gui_outcome(task_code, None, locations.clone(), Err(payload))
                    .unwrap_err();
                assert_eq!(
                    error_exit_code(&error),
                    if task_code == Some(4) { 4 } else { 3 }
                );
                assert!(format!("{error:#}").contains("injected UI panic"));
                let context = error.downcast_ref::<GuiFailureContext>().unwrap();
                assert_eq!(context.task_exit_code, task_code);
                assert!(!context.dispatcher_join_failed);
                assert_eq!(context.ui_failure_kind, "panic");
                assert!(context.requested_report.is_none());
                assert_eq!(context.task_locations, locations);
            }
        }
    }

    #[test]
    fn dispatcher_panic_preserves_locations_and_original_ui_cause_as_unknown() {
        for (kind, locations) in ["normal", "error", "panic"]
            .into_iter()
            .flat_map(|kind| [None, Some(sample_locations())].map(|locations| (kind, locations)))
        {
            let ui_outcome = match kind {
                "error" => Ok(Err(anyhow::anyhow!("original UI error"))),
                "panic" => Err(Box::new("original UI panic") as Box<dyn std::any::Any + Send>),
                _ => Ok(Ok(())),
            };
            let error = merge_gui_shutdown(
                Err(Box::new("synthetic dispatcher panic")),
                None,
                locations.clone(),
                ui_outcome,
            )
            .unwrap_err();
            assert_eq!(error_exit_code(&error), 4);
            let context = error.downcast_ref::<GuiFailureContext>().unwrap();
            assert_eq!(context.task_exit_code, None);
            assert!(context.dispatcher_join_failed);
            assert_eq!(context.task_locations, locations);
            assert_eq!(
                context.ui_failure_kind,
                if kind == "panic" { "panic" } else { "error" }
            );
            let serialized = serde_json::to_value(context).unwrap();
            assert_eq!(serialized["task_exit_code"], Value::Null);
            assert_eq!(serialized["dispatcher_join_failed"], true);
            let context_detail = context.to_string();
            assert!(context_detail.contains("task completion and exit code are unknown"));
            assert!(!context_detail.contains("no coordinator task was started"));
            assert!(!context_detail.contains("returned exit code"));
            let detail = format!("{error:#}");
            assert!(detail.contains("synthetic dispatcher panic"));
            if kind != "normal" {
                assert!(detail.contains(&format!("original UI {kind}")));
            }
        }
    }

    #[test]
    fn stdout_error_or_panic_cannot_overwrite_persisted_result_during_guard_drop() {
        struct BrokenOutput {
            panic: bool,
        }
        impl Write for BrokenOutput {
            fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
                if self.panic {
                    panic!("synthetic stdout presentation panic");
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "reader exited",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for panic in [false, true] {
            let folder = std::env::temp_dir().join(format!(
                "avelin-report-guard-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir(&folder).unwrap();
            let lock_path = folder.join("product.lock");
            let report_path = folder.join("report.json");
            let (hooks, shared) = test_hooks();
            allocate_task_locations("persisted-report-test".into(), folder.clone(), None, &hooks)
                .unwrap();
            let mut guard = exited_backend_guard(&folder, report_path.clone(), hooks);
            let report = json!({"result": "Succeeded", "coordinator_exit_code": 0});
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                persist_and_publish_report(&mut guard, &report, &mut BrokenOutput { panic })
            }));
            if panic {
                assert!(outcome.is_err());
            } else {
                outcome.unwrap().unwrap();
            }
            drop(guard);
            // Moving `completed = true` below output would cause the panic case
            // to write an Unknown report here, so this checks production ordering.
            assert_eq!(
                fs::read(&report_path).unwrap(),
                serde_json::to_vec_pretty(&report).unwrap()
            );
            assert_eq!(
                task_locations_snapshot(&shared).unwrap().saved_report,
                Some(report_path.clone())
            );
            assert!(!lock_path.exists());
            fs::remove_file(&report_path).unwrap();
            fs::remove_dir(&folder).unwrap();
        }
    }

    #[test]
    fn failed_primary_report_tracks_only_a_successful_unknown_fallback() {
        for fallback_blocked in [false, true] {
            let folder = std::env::temp_dir().join(format!(
                "avelin-report-fallback-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir(&folder).unwrap();
            // Directories at file destinations create deterministic failures
            // without permissions changes or unreliable disk-full injection.
            let report_path = folder.join("report.json");
            fs::create_dir(&report_path).unwrap();
            let fallback = folder.join("unknown-result.json");
            if fallback_blocked {
                fs::create_dir(&fallback).unwrap();
            }
            #[allow(unused_mut)]
            let (mut hooks, shared) = test_hooks();
            #[cfg(feature = "gui")]
            let snapshots = {
                let (sender, receiver) = std::sync::mpsc::channel();
                hooks.as_mut().unwrap().snapshots = sender;
                receiver
            };
            allocate_task_locations("persisted-report-test".into(), folder.clone(), None, &hooks)
                .unwrap();
            let mut guard = exited_backend_guard(&folder, report_path.clone(), hooks);
            let result = persist_and_publish_report(
                &mut guard,
                &json!({"result":"Succeeded","coordinator_exit_code":0}),
                &mut Vec::new(),
            );
            assert!(result.is_err());
            assert!(!guard.completed);
            assert!(
                task_locations_snapshot(&shared)
                    .unwrap()
                    .saved_report
                    .is_none()
            );
            drop(guard);
            let locations = task_locations_snapshot(&shared).unwrap();
            assert_eq!(locations.report_path, report_path);
            assert_eq!(
                locations.saved_report,
                (!fallback_blocked).then(|| fallback.clone())
            );
            #[cfg(feature = "gui")]
            {
                let snapshot = snapshots.recv().unwrap();
                assert_eq!(snapshot.phase, avelin_ui::UiPhase::Unknown);
                assert!(snapshot.detail.contains("AVELIN_RESULT_UNKNOWN"));
                assert!(
                    !snapshot
                        .detail
                        .contains(&format!("已保存报告：{}", report_path.display()))
                );
                if fallback_blocked {
                    assert!(snapshot.detail.contains("报告未成功保存"));
                    assert!(!snapshot.detail.contains("已保存报告："));
                } else {
                    assert!(
                        snapshot
                            .detail
                            .contains(&format!("已保存报告：{}", fallback.display()))
                    );
                }
            }
            assert!(!folder.join("product.lock").exists());
            if fallback_blocked {
                fs::remove_dir(&fallback).unwrap();
            } else {
                let report: Value = serde_json::from_slice(&fs::read(&fallback).unwrap()).unwrap();
                assert_eq!(report["result"], "Unknown");
                assert_eq!(report["task_id"], "persisted-report-test");
                assert_eq!(report["report_path"], fallback.to_string_lossy().as_ref());
                assert_eq!(report["diagnostic"]["code"], "AVELIN_RESULT_UNKNOWN");
                fs::remove_file(&fallback).unwrap();
            }
            fs::remove_dir(&report_path).unwrap();
            fs::remove_dir(&folder).unwrap();
        }
    }

    #[test]
    fn waiting_for_self_is_rejected() {
        assert!(wait_for_parent(std::process::id()).is_err());
        // PID 0 yields access denied or invalid parameter depending on Windows;
        // only an actual ERROR_INVALID_PARAMETER is allowed to mean already exited.
    }

    #[test]
    #[ignore = "fixture subprocess used by backend_guard_preserves_lock_until_exit"]
    fn backend_guard_fixture() {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
    }

    #[test]
    fn backend_guard_preserves_lock_until_exit() {
        use std::os::windows::{
            io::{AsHandle, AsRawHandle},
            process::CommandExt,
        };
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn WaitForSingleObject(handle: *mut std::ffi::c_void, timeout: u32) -> u32;
        }

        for running in [true, false] {
            let folder = std::env::temp_dir().join(format!(
                "avelin-guard-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir(&folder).unwrap();
            let lock_path = folder.join("product.lock");
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
                .unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "coordinator::tests::backend_guard_fixture",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(0x08000000)
                .spawn()
                .unwrap();
            let handle = child.as_handle().try_clone_to_owned().unwrap();
            if !running {
                drop(child.stdin.take());
                child.wait().unwrap();
            }
            let (hooks, shared) = test_hooks();
            allocate_task_locations("guard-test".into(), folder.clone(), None, &hooks).unwrap();
            let guard = BackendGuard {
                child,
                lock: AppLock {
                    _file: Some(file),
                    path: lock_path.clone(),
                    retain: false,
                },
                task: "guard-test".into(),
                target: folder.join("target"),
                session: folder.clone(),
                report_path: folder.join("report.json"),
                hooks,
                completed: false,
                english: false,
                fault_after_files: None,
                last_progress: None,
                last_work_progress: None,
            };
            drop(guard);
            // Dropping Child closes its stdin; this test-only child then exits naturally.
            assert_eq!(
                unsafe { WaitForSingleObject(handle.as_raw_handle(), 5_000) },
                0
            );
            assert_eq!(lock_path.exists(), running);
            let report: Value =
                serde_json::from_slice(&fs::read(folder.join("report.json")).unwrap()).unwrap();
            assert_eq!(report["result"], "Unknown");
            assert_eq!(report["product_lock_retained"], running);
            assert_eq!(
                task_locations_snapshot(&shared).unwrap().saved_report,
                Some(folder.join("report.json"))
            );
            fs::remove_file(folder.join("report.json")).unwrap();
            if running {
                fs::remove_file(lock_path).unwrap();
            }
            fs::remove_dir(folder).unwrap();
        }
    }
}
