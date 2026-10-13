//! Shared task vocabulary and validation of the existing backend payloads. The transport owns framing,
//! authentication and sequence numbers; only validated payloads affect UI state.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

/// Local UI intent. Clipboard, picker and window actions remain UI-local.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskCommand {
    Start {
        install_dir: PathBuf,
        /// The user's desktop shortcut choice for this installation.
        desktop_shortcut: bool,
        /// The user's Start menu shortcut choice for this installation.
        start_menu_shortcut: bool,
        #[serde(default)]
        selected_components: Option<Vec<String>>,
        #[serde(default)]
        operation: Option<crate::Operation>,
        #[serde(default)]
        migration_confirmed: bool,
    },
    Cancel,
    /// Explicit post-install action; the coordinator must revalidate the installed entry.
    LaunchInstalled,
}

// Serde ignores extra fields for internally tagged unit variants. Decode via
// empty struct variants so every command rejects unregistered input fields,
// while callers and serialization keep the existing public unit variants.
#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum TaskCommandWire {
    Start {
        install_dir: PathBuf,
        desktop_shortcut: bool,
        start_menu_shortcut: bool,
        #[serde(default)]
        selected_components: Option<Vec<String>>,
        #[serde(default)]
        operation: Option<crate::Operation>,
        #[serde(default)]
        migration_confirmed: bool,
    },
    Cancel {},
    LaunchInstalled {},
}

impl<'de> Deserialize<'de> for TaskCommand {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match TaskCommandWire::deserialize(deserializer)? {
            TaskCommandWire::Start {
                install_dir,
                desktop_shortcut,
                start_menu_shortcut,
                selected_components,
                operation,
                migration_confirmed,
            } => Self::Start {
                install_dir,
                desktop_shortcut,
                start_menu_shortcut,
                selected_components,
                operation,
                migration_confirmed,
            },
            TaskCommandWire::Cancel {} => Self::Cancel,
            TaskCommandWire::LaunchInstalled {} => Self::LaunchInstalled,
        })
    }
}

/// Host action feedback independent of the confirmed task result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "response", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskResponse {
    LaunchResult {
        succeeded: bool,
        detail: String,
    },
    /// No worker or transaction has started. The host remains ready to check a
    /// new Start request after the user resolves the reported file usage.
    PreflightBlocked {
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskPhase {
    #[default]
    Ready,
    Preparing,
    Installing,
    Cancelling,
    Finalizing,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

impl TaskPhase {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Unknown
        )
    }

    pub fn is_running(self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Installing | Self::Cancelling | Self::Finalizing
        )
    }
}

/// Progress counters share one unit. None denotes indeterminate work.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskSnapshot {
    pub phase: TaskPhase,
    pub completed: u64,
    pub total: Option<u64>,
    pub detail: String,
}

impl TaskSnapshot {
    pub fn progress(progress: Progress) -> Option<Self> {
        Some(Self {
            phase: progress.phase.task_phase()?,
            completed: progress.completed,
            total: Some(progress.total),
            detail: String::new(),
        })
    }

    /// Publish only after the coordinator has checked exit and persistent evidence.
    pub fn finished(result: TaskResult, detail: impl Into<String>) -> Self {
        Self {
            phase: result.into(),
            completed: 0,
            total: None,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    Preparing,
    Extracting,
    Running,
    Finalizing,
    Cancelling,
    CleanupCompleted,
}

impl ProgressPhase {
    pub fn task_phase(self) -> Option<TaskPhase> {
        Some(match self {
            Self::Preparing => TaskPhase::Preparing,
            Self::Extracting | Self::Running => TaskPhase::Installing,
            Self::Finalizing => TaskPhase::Finalizing,
            Self::Cancelling => TaskPhase::Cancelling,
            // Cleanup telemetry alone never confirms a cancelled task.
            Self::CleanupCompleted => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub phase: ProgressPhase,
    pub completed: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskResult {
    Succeeded,
    Cancelled,
    Failed,
    Unknown,
}

impl From<TaskResult> for TaskPhase {
    fn from(result: TaskResult) -> Self {
        match result {
            TaskResult::Succeeded => Self::Succeeded,
            TaskResult::Cancelled => Self::Cancelled,
            TaskResult::Failed => Self::Failed,
            TaskResult::Unknown => Self::Unknown,
        }
    }
}

impl std::fmt::Display for TaskResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TaskResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "Succeeded",
            Self::Cancelled => "Cancelled",
            Self::Failed => "Failed",
            Self::Unknown => "Unknown",
        }
    }

    pub fn code(self) -> i32 {
        match self {
            Self::Succeeded => 0,
            Self::Cancelled => 2,
            Self::Failed => 3,
            Self::Unknown => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendEvent {
    Progress(Progress),
    CancelReceived,
    Finished(TaskResult),
}

impl BackendEvent {
    /// Parse an already authenticated payload; transport envelope fields may be present.
    /// Retains the existing M0 handling of unrelated metadata for wire compatibility.
    pub fn parse(value: &Value) -> Result<Self, &'static str> {
        match value["kind"].as_str() {
            Some("progress") => {
                let phase = match value["phase"].as_str() {
                    Some("preparing") => ProgressPhase::Preparing,
                    Some("extracting") => ProgressPhase::Extracting,
                    Some("running") => ProgressPhase::Running,
                    Some("finalizing") => ProgressPhase::Finalizing,
                    Some("cancelling") => ProgressPhase::Cancelling,
                    Some("cleanup_completed") => ProgressPhase::CleanupCompleted,
                    _ => return Err("unknown or missing progress phase"),
                };
                let completed = value["completed"]
                    .as_u64()
                    .ok_or("progress completed must be an unsigned integer")?;
                let total = value["total"]
                    .as_u64()
                    .ok_or("progress total must be an unsigned integer")?;
                if completed > total {
                    return Err("progress completed exceeds total");
                }
                Ok(Self::Progress(Progress {
                    phase,
                    completed,
                    total,
                }))
            }
            Some("cancel_received") => Ok(Self::CancelReceived),
            Some("finish") => {
                let result = match value["result"].as_str() {
                    Some("Succeeded") => TaskResult::Succeeded,
                    Some("Cancelled") => TaskResult::Cancelled,
                    Some("Failed") => TaskResult::Failed,
                    Some("Unknown") => TaskResult::Unknown,
                    _ => return Err("unknown or missing terminal result"),
                };
                if value["code"].as_i64() != Some(i64::from(result.code())) {
                    return Err("terminal result/code mismatch");
                }
                Ok(Self::Finished(result))
            }
            _ => Err("unknown or missing backend event kind"),
        }
    }

    pub fn to_value(self) -> Value {
        match self {
            Self::Progress(progress) => {
                json!({"kind":"progress", "phase":progress.phase, "completed":progress.completed, "total":progress.total})
            }
            Self::CancelReceived => json!({"kind":"cancel_received"}),
            Self::Finished(result) => {
                json!({"kind":"finish", "result":result, "code":result.code()})
            }
        }
    }
}

impl Serialize for BackendEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BackendEvent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Default)]
pub struct BackendEvents {
    terminal: Option<TaskResult>,
    protocol_fault: Option<String>,
}

impl BackendEvents {
    pub fn observe(&mut self, value: &Value) -> Option<BackendEvent> {
        let event = match BackendEvent::parse(value) {
            Ok(event) => event,
            Err(reason) => {
                self.record_fault(reason);
                return None;
            }
        };
        if self.protocol_fault.is_some() {
            return None;
        }
        if let Some(first) = self.terminal {
            match event {
                // Same terminal payload with a new envelope sequence is idempotent.
                BackendEvent::Finished(result) if result == first => {}
                BackendEvent::Finished(_) => self.record_fault("conflicting terminal event"),
                BackendEvent::Progress(_) => self.record_fault("progress after terminal event"),
                // The worker I/O thread can acknowledge an in-flight cancellation
                // while its finish call is joining. It cannot change task metrics.
                BackendEvent::CancelReceived => {}
            }
            return None;
        }
        if let BackendEvent::Finished(result) = event {
            self.terminal = Some(result);
        }
        Some(event)
    }

    pub fn record_fault(&mut self, reason: impl Into<String>) {
        self.protocol_fault.get_or_insert_with(|| reason.into());
    }

    pub fn protocol_fault(&self) -> Option<&str> {
        self.protocol_fault.as_deref()
    }

    pub fn accepts_commands(&self) -> bool {
        self.terminal.is_none() && self.protocol_fault.is_none()
    }

    pub fn outcome(
        &self,
        child_exit: Option<i32>,
        stream_drained: bool,
    ) -> Result<TaskResult, String> {
        if let Some(reason) = &self.protocol_fault {
            return Err(format!("backend protocol fault: {reason}"));
        }
        if !stream_drained {
            return Err("backend event stream was not drained before observation ended".into());
        }
        let result = self.terminal.ok_or("backend terminal event missing")?;
        if child_exit != Some(result.code()) {
            return Err(format!(
                "terminal/exit mismatch: expected {}, got {child_exit:?}",
                result.code()
            ));
        }
        Ok(result)
    }
}

/// A terminal alone cannot end observation: more events may already be queued.
pub fn observation_complete(child_exit: Option<i32>, connection_open: bool) -> bool {
    child_exit.is_some() && !connection_open
}

/// Replays payload semantics without processes or filesystem changes. This does not
/// establish transport authentication or verify any installed resources. A Finished
/// payload is only a candidate until the complete stream and exit agree.
pub fn replay_backend_events(
    events: &[Value],
    child_exit: Option<i32>,
    stream_drained: bool,
) -> Vec<TaskSnapshot> {
    let mut state = BackendEvents::default();
    let mut snapshots = Vec::new();
    for value in events {
        if let Some(BackendEvent::Progress(progress)) = state.observe(value) {
            if let Some(snapshot) = TaskSnapshot::progress(progress) {
                snapshots.push(snapshot);
            }
        }
    }
    let final_snapshot = match state.outcome(child_exit, stream_drained) {
        Ok(result) => TaskSnapshot::finished(result, ""),
        Err(reason) => TaskSnapshot::finished(TaskResult::Unknown, reason),
    };
    snapshots.push(final_snapshot);
    snapshots
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preflight_blocked_is_feedback_not_a_terminal_and_rejects_extra_fields() {
        let response = TaskResponse::PreflightBlocked {
            detail: "Example (PID 42): bin/example.exe".into(),
        };
        let value = json!({
            "response":"preflight_blocked", "detail":"Example (PID 42): bin/example.exe"
        });
        assert_eq!(serde_json::to_value(&response).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<TaskResponse>(value.clone()).unwrap(),
            response
        );
        for invalid in [
            json!({"response":"preflight_blocked"}),
            json!({"response":"preflight_blocked","detail":false}),
            json!({"response":"preflight_blocked","detail":"","succeeded":false}),
        ] {
            assert!(serde_json::from_value::<TaskResponse>(invalid).is_err());
        }
        assert!(serde_json::from_value::<TaskSnapshot>(value).is_err());
    }

    #[test]
    fn launch_action_and_response_are_separate_from_unchanged_task_snapshots() {
        assert_eq!(
            serde_json::to_value(TaskCommand::LaunchInstalled).unwrap(),
            json!({"command":"launch_installed"})
        );
        let response = TaskResponse::LaunchResult {
            succeeded: false,
            detail: "entry changed".into(),
        };
        let value = json!({"response":"launch_result","succeeded":false,"detail":"entry changed"});
        assert_eq!(serde_json::to_value(&response).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<TaskResponse>(value).unwrap(),
            response
        );
        assert!(
            serde_json::from_value::<TaskResponse>(
                json!({"response":"launch_result","succeeded":true,"detail":"","extra":true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<TaskCommand>(
                json!({"command":"launch_installed","path":"untrusted.exe"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<TaskCommand>(
                json!({"command":"cancel","path":"untrusted.exe"})
            )
            .is_err()
        );
        for command in [
            TaskCommand::Cancel,
            TaskCommand::LaunchInstalled,
            TaskCommand::Start {
                install_dir: PathBuf::from("C:\\App"),
                desktop_shortcut: true,
                start_menu_shortcut: false,
                selected_components: None,
                operation: None,
                migration_confirmed: false,
            },
        ] {
            let value = serde_json::to_value(&command).unwrap();
            assert_eq!(
                serde_json::from_value::<TaskCommand>(value.clone()).unwrap(),
                command
            );
            let mut extra = value;
            extra["unexpected"] = json!(true);
            assert!(serde_json::from_value::<TaskCommand>(extra).is_err());
        }
        assert_eq!(
            serde_json::to_value(TaskSnapshot::finished(TaskResult::Succeeded, "verified"))
                .unwrap(),
            json!({"phase":"Succeeded","completed":0,"total":null,"detail":"verified"})
        );
    }

    fn finish(result: TaskResult) -> Value {
        json!({"kind":"finish", "result":result.as_str(), "code":result.code()})
    }

    #[test]
    fn every_current_script_phase_and_nonmonotonic_cleanup_is_valid() {
        let mut state = BackendEvents::default();
        for phase in [
            "preparing",
            "extracting",
            "running",
            "finalizing",
            "cancelling",
            "cleanup_completed",
        ] {
            assert!(matches!(
                state.observe(&json!({"kind":"progress", "phase":phase, "completed":1, "total":3})),
                Some(BackendEvent::Progress(_))
            ));
        }
        assert!(state.protocol_fault().is_none());
    }

    #[test]
    fn malformed_payloads_are_rejected_without_a_default_phase_or_counter() {
        for value in [
            Value::Null,
            json!({}),
            json!({"kind":42}),
            json!({"kind":"future_event"}),
            json!({"kind":"progress", "phase":"future_phase", "completed":0, "total":1}),
            json!({"kind":"progress", "completed":0, "total":1}),
            json!({"kind":"progress", "phase":"running", "total":1}),
            json!({"kind":"progress", "phase":"running", "completed":0}),
            json!({"kind":"progress", "phase":"running", "completed":-1, "total":1}),
            json!({"kind":"progress", "phase":"running", "completed":0, "total":-1}),
            json!({"kind":"progress", "phase":"running", "completed":1.0, "total":1}),
            json!({"kind":"progress", "phase":"running", "completed":"1", "total":1}),
            json!({"kind":"progress", "phase":"running", "completed":2, "total":1}),
            json!({"kind":"progress", "phase":"running", "completed":0, "total":null}),
            json!({"kind":"finish", "result":"Other", "code":4}),
            json!({"kind":"finish", "result":"Succeeded"}),
            json!({"kind":"finish", "result":"Succeeded", "code":"0"}),
        ] {
            let mut state = BackendEvents::default();
            assert_eq!(state.observe(&value), None, "{value}");
            assert!(state.protocol_fault().is_some(), "{value}");
            assert!(!state.accepts_commands());
            assert!(state.outcome(Some(0), true).is_err());
        }
    }

    #[test]
    fn four_terminal_results_require_their_matching_code_and_actual_exit() {
        for result in [
            TaskResult::Succeeded,
            TaskResult::Cancelled,
            TaskResult::Failed,
            TaskResult::Unknown,
        ] {
            let mut state = BackendEvents::default();
            assert_eq!(
                state.observe(&finish(result)),
                Some(BackendEvent::Finished(result))
            );
            assert_eq!(state.outcome(Some(result.code()), true), Ok(result));
            assert!(state.outcome(None, true).is_err());
            assert!(state.outcome(Some(99), true).is_err());
            let mut bad = finish(result);
            bad["code"] = json!(99);
            assert!(BackendEvent::parse(&bad).is_err());
        }
    }

    #[test]
    fn matching_terminal_repeats_are_idempotent_and_late_ack_has_no_effect() {
        let mut state = BackendEvents::default();
        let mut event = finish(TaskResult::Succeeded);
        event["seq"] = json!(2);
        state.observe(&event);
        event["seq"] = json!(3);
        assert_eq!(state.observe(&event), None);
        assert_eq!(state.observe(&json!({"kind":"cancel_received"})), None);
        assert_eq!(state.outcome(Some(0), true), Ok(TaskResult::Succeeded));
        assert!(!state.accepts_commands());
    }

    #[test]
    fn conflicting_terminal_never_replaces_the_first_candidate_or_recovers() {
        let mut state = BackendEvents::default();
        state.observe(&finish(TaskResult::Failed));
        assert_eq!(state.observe(&finish(TaskResult::Succeeded)), None);
        assert_eq!(state.terminal, Some(TaskResult::Failed));
        state.observe(&finish(TaskResult::Failed));
        assert!(state.outcome(Some(3), true).is_err());
        assert_eq!(state.protocol_fault(), Some("conflicting terminal event"));
    }

    #[test]
    fn malformed_or_progress_after_terminal_latches_unknown_without_ui_event() {
        for event in [
            json!({"kind":"progress", "phase":"running", "completed":1, "total":1}),
            json!({"kind":"finish", "result":"Succeeded", "code":3}),
            json!({"kind":"unknown"}),
        ] {
            let mut state = BackendEvents::default();
            state.observe(&finish(TaskResult::Succeeded));
            assert_eq!(state.observe(&event), None);
            assert_eq!(state.terminal, Some(TaskResult::Succeeded));
            assert!(state.outcome(Some(0), true).is_err());
        }
    }

    #[test]
    fn payload_or_transport_fault_cannot_be_healed_by_later_valid_events() {
        let mut state = BackendEvents::default();
        state.record_fault("invalid framing");
        assert_eq!(state.observe(&json!({"kind":"cancel_received"})), None);
        assert_eq!(state.observe(&finish(TaskResult::Succeeded)), None);
        state.record_fault("later failure");
        assert_eq!(state.protocol_fault(), Some("invalid framing"));
        assert!(state.outcome(Some(0), true).is_err());
    }

    #[test]
    fn exit_and_terminal_cannot_skip_the_rest_of_the_stream() {
        let mut state = BackendEvents::default();
        state.observe(&finish(TaskResult::Succeeded));
        assert!(!observation_complete(Some(0), true));
        assert!(!observation_complete(None, false));
        assert!(observation_complete(Some(0), false));
        assert!(state.outcome(Some(0), false).is_err());
        assert!(BackendEvents::default().outcome(Some(0), true).is_err());
    }

    #[test]
    fn payload_round_trip_preserves_existing_shapes_and_shared_mapping() {
        for phase in [
            ProgressPhase::Preparing,
            ProgressPhase::Extracting,
            ProgressPhase::Running,
            ProgressPhase::Finalizing,
            ProgressPhase::Cancelling,
            ProgressPhase::CleanupCompleted,
        ] {
            let event = BackendEvent::Progress(Progress {
                phase,
                completed: 1,
                total: 3,
            });
            let value = serde_json::to_value(event).unwrap();
            assert_eq!(value["kind"], "progress");
            assert_eq!(
                serde_json::from_value::<BackendEvent>(value).unwrap(),
                event
            );
            assert_eq!(
                TaskSnapshot::progress(Progress {
                    phase,
                    completed: 1,
                    total: 3
                })
                .map(|s| s.phase),
                phase.task_phase()
            );
        }
        for result in [
            TaskResult::Succeeded,
            TaskResult::Cancelled,
            TaskResult::Failed,
            TaskResult::Unknown,
        ] {
            let event = BackendEvent::Finished(result);
            assert_eq!(serde_json::to_value(event).unwrap(), finish(result));
            assert_eq!(
                TaskSnapshot::finished(result, "reason").phase,
                TaskPhase::from(result)
            );
        }
    }

    #[test]
    fn replay_uses_runtime_validation_and_never_publishes_an_unconfirmed_terminal() {
        for result in [
            TaskResult::Succeeded,
            TaskResult::Cancelled,
            TaskResult::Failed,
            TaskResult::Unknown,
        ] {
            let events = vec![
                json!({"kind":"progress","phase":"running","completed":1,"total":2}),
                finish(result),
            ];
            let snapshots = replay_backend_events(&events, Some(result.code()), true);
            assert_eq!(snapshots[0].phase, TaskPhase::Installing);
            assert_eq!(snapshots.last().unwrap().phase, TaskPhase::from(result));
            assert_eq!(
                replay_backend_events(&events, None, false)
                    .last()
                    .unwrap()
                    .phase,
                TaskPhase::Unknown
            );
        }
        let conflicting = vec![finish(TaskResult::Succeeded), finish(TaskResult::Failed)];
        let snapshots = replay_backend_events(&conflicting, Some(0), true);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].phase, TaskPhase::Unknown);
        let late_ack = vec![
            finish(TaskResult::Succeeded),
            json!({"kind":"cancel_received"}),
        ];
        assert_eq!(
            replay_backend_events(&late_ack, Some(0), true)
                .last()
                .unwrap()
                .phase,
            TaskPhase::Succeeded
        );
    }

    #[test]
    fn documented_replays_cover_success_cancellation_failure_and_loss() {
        for (source, exit, expected) in [
            (
                include_str!("../../../docs/schemas/examples/replay-succeeded.json"),
                Some(0),
                TaskPhase::Succeeded,
            ),
            (
                include_str!("../../../docs/schemas/examples/replay-cancelled.json"),
                Some(2),
                TaskPhase::Cancelled,
            ),
            (
                include_str!("../../../docs/schemas/examples/replay-failed.json"),
                Some(3),
                TaskPhase::Failed,
            ),
            (
                include_str!("../../../docs/schemas/examples/replay-unknown.json"),
                None,
                TaskPhase::Unknown,
            ),
        ] {
            let events: Vec<Value> = serde_json::from_str(source).unwrap();
            let snapshots = replay_backend_events(&events, exit, true);
            assert_eq!(snapshots.last().unwrap().phase, expected);
            assert_eq!(
                snapshots
                    .iter()
                    .filter(|snapshot| snapshot.phase.is_terminal())
                    .count(),
                1
            );
        }
        let command: TaskCommand =
            serde_json::from_str(include_str!("../../../docs/schemas/examples/command.json"))
                .unwrap();
        assert!(matches!(command, TaskCommand::Start { .. }));
        let snapshot: TaskSnapshot =
            serde_json::from_str(include_str!("../../../docs/schemas/examples/snapshot.json"))
                .unwrap();
        assert_eq!(snapshot.phase, TaskPhase::Installing);
        assert_eq!(snapshot.total, Some(2));
    }
}
