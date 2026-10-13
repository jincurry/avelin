//! Native Windows deployment transactions. No script interpreter performs mutations.
#[cfg(windows)]
mod owned_files;
#[cfg(windows)]
mod recovery;
#[cfg(windows)]
mod system;
#[cfg(windows)]
mod transaction;

use anyhow::Result;
use avelin_core::{InstallPlan, Manifest, Receipt, ShortcutRecord, native_payload::NativePayload};
use std::path::Path;

pub struct EngineRequest<'a> {
    pub manifest: &'a Manifest,
    pub plan: &'a InstallPlan,
    pub receipt: &'a Receipt,
    pub session_dir: &'a Path,
    pub worker_exe: &'a Path,
    pub payload: Option<&'a NativePayload<'a>>,
    pub shortcuts: Vec<ShortcutRecord>,
    pub fault_after_files: Option<u64>,
    pub cancel_after_files: Option<u64>,
    pub step_delay_ms: u64,
    /// Explicit acceptance-test failure; never interpreted as a real system error.
    pub fault_point: Option<&'a str>,
    /// Bounded process-termination acceptance checkpoint.
    pub pause_point: Option<&'a str>,
    pub recovery_session: Option<&'a Path>,
    pub recovery_apply: bool,
    pub repair_overwrite: bool,
}

pub trait EngineObserver {
    fn progress(&mut self, phase: &str, current: u64, total: u64) -> Result<()>;
    /// True requests cancellation; an error also stops work and attempts rollback.
    fn checkpoint(&mut self) -> Result<bool>;
}

#[derive(Debug)]
pub struct EngineOutcome {
    /// The shared TaskResult code: success 0, cancelled 2, failed 3, unknown 4.
    pub exit_code: i32,
    pub detail: String,
}

/// Errors before a transaction begins have not modified installation resources.
/// After beginning a transaction, recovery failures return an Unknown outcome.
#[cfg(windows)]
pub fn execute(
    request: EngineRequest<'_>,
    observer: &mut impl EngineObserver,
) -> Result<EngineOutcome> {
    if request.plan.operation == avelin_core::Operation::Recover {
        return recovery::execute(request, observer);
    }
    let _lock = recovery::ProductLock::acquire(&request.plan.app_id, &request.plan.user_sid)?;
    transaction::execute(request, observer)
}

#[cfg(not(windows))]
pub fn execute(_: EngineRequest<'_>, _: &mut impl EngineObserver) -> Result<EngineOutcome> {
    anyhow::bail!("the native deployment engine requires Windows")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryReport {
    pub schema_version: u32,
    pub original_task_id: String,
    pub decision: String,
    pub applied: bool,
    pub verified: bool,
    pub restored: Vec<String>,
    pub preserved: Vec<String>,
}
pub type RecoveryInspection = RecoveryReport;

pub struct RecoveryRequest<'a> {
    pub manifest: &'a Manifest,
    pub target: &'a Path,
    pub session: &'a Path,
    pub user_sid: &'a str,
    pub apply: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairReport {
    pub schema_version: u32,
    pub task_id: String,
    pub fully_repaired: bool,
    pub restored: Vec<String>,
    pub preserved: Vec<String>,
    pub backups: Vec<String>,
}

pub fn inspect_recovery(
    manifest: &Manifest,
    target: &Path,
    session: &Path,
    user_sid: &str,
) -> Result<RecoveryInspection> {
    #[cfg(windows)]
    {
        recovery::inspect(&RecoveryRequest {
            manifest,
            target,
            session,
            user_sid,
            apply: false,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (manifest, target, session, user_sid);
        anyhow::bail!("recovery requires Windows")
    }
}
pub fn recover(
    request: RecoveryRequest<'_>,
    observer: &mut impl EngineObserver,
) -> Result<RecoveryReport> {
    #[cfg(windows)]
    {
        recovery::recover(request, observer, None)
    }
    #[cfg(not(windows))]
    {
        let _ = (request, observer);
        anyhow::bail!("recovery requires Windows")
    }
}
pub fn verify_repair(manifest: &Manifest, target: &Path, user_sid: &str) -> Result<RepairReport> {
    #[cfg(windows)]
    {
        transaction::verify_repair(manifest, target, user_sid)
    }
    #[cfg(not(windows))]
    {
        let _ = (manifest, target, user_sid);
        anyhow::bail!("repair requires Windows")
    }
}
