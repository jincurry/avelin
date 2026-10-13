//! Native Windows deployment transactions. No script interpreter performs mutations.
#[cfg(windows)]
mod owned_files;
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
    transaction::execute(request, observer)
}

#[cfg(not(windows))]
pub fn execute(_: EngineRequest<'_>, _: &mut impl EngineObserver) -> Result<EngineOutcome> {
    anyhow::bail!("the native deployment engine requires Windows")
}
