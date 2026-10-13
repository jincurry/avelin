//! Coordinator diagnostics describe observed evidence, not guessed failure causes.

use super::backend_events::{Progress, ProgressPhase};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub(super) struct ProgressEvidence {
    phase: &'static str,
    completed: u64,
    total: u64,
}

impl From<Progress> for ProgressEvidence {
    fn from(progress: Progress) -> Self {
        Self {
            phase: match progress.phase {
                ProgressPhase::Preparing => "preparing",
                ProgressPhase::Extracting => "extracting",
                ProgressPhase::Running => "running",
                ProgressPhase::Finalizing => "finalizing",
                ProgressPhase::Cancelling => "cancelling",
                ProgressPhase::CleanupCompleted => "cleanup_completed",
            },
            completed: progress.completed,
            total: progress.total,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct TaskDiagnostic {
    pub code: &'static str,
    pub reason: String,
    reason_source: &'static str,
    backend_details: &'static str,
    last_progress: Option<ProgressEvidence>,
    last_work_progress: Option<ProgressEvidence>,
    observed_backend_exit_code: Option<i32>,
    configured_fault_after_files: Option<u64>,
    fault_configuration_proves_cause: bool,
}

impl TaskDiagnostic {
    pub(super) fn with_backend_detail(mut self, detail: Option<&str>) -> Self {
        if let Some(detail) = detail.filter(|value| !value.trim().is_empty()) {
            self.backend_details = "terminal_event_metadata";
            if self.reason_source == "fallback" {
                self.reason = detail.chars().take(2048).collect();
                self.reason_source = "backend";
            }
        }
        self
    }
}

pub(super) fn diagnostic(
    result: &str,
    message: &str,
    last_progress: Option<Progress>,
    last_work_progress: Option<Progress>,
    exit_code: Option<i32>,
    fault_after_files: Option<u64>,
) -> Option<TaskDiagnostic> {
    let code = match result {
        "Failed" => "AVELIN_TASK_FAILED",
        "Unknown" => "AVELIN_RESULT_UNKNOWN",
        _ => return None,
    };
    let has_reason = !message.trim().is_empty();
    Some(TaskDiagnostic {
        code,
        reason: if has_reason {
            message.to_owned()
        } else if result == "Failed" {
            "The backend reported failure without a detailed reason.".into()
        } else {
            "The task result and cleanup have not been confirmed.".into()
        },
        reason_source: if has_reason {
            "coordinator"
        } else {
            "fallback"
        },
        // Legacy workers have no reason field; native terminal metadata may
        // supplement this after envelope and payload validation.
        backend_details: "not_provided_by_current_protocol",
        last_progress: last_progress.map(Into::into),
        last_work_progress: last_work_progress.map(Into::into),
        observed_backend_exit_code: exit_code,
        configured_fault_after_files: fault_after_files,
        fault_configuration_proves_cause: false,
    })
}

pub(super) fn report_guidance(saved_report: Option<&Path>, english: bool) -> String {
    let report = match (saved_report, english) {
        (Some(path), true) => format!("Saved report: {}", path.display()),
        (Some(path), false) => format!("已保存报告：{}", path.display()),
        (None, true) => "No report was saved successfully.".into(),
        (None, false) => "报告未成功保存。".into(),
    };
    let next = if english {
        "Keep the evidence. Review the report/logs and verify the installation and cleanup state before retrying."
    } else {
        "请保留现场，查看报告和日志，核对安装及清理状态后再决定是否重试。"
    };
    format!("{report}\n{next}")
}

pub(super) fn terminal_detail(
    message: &str,
    diagnostic: Option<&TaskDiagnostic>,
    saved_report: Option<&Path>,
    english: bool,
) -> String {
    let Some(diagnostic) = diagnostic else {
        // Success/cancellation retain their existing detail and gain no error UI.
        return message.to_owned();
    };
    let reason = if diagnostic.reason_source != "fallback" || english {
        diagnostic.reason.as_str()
    } else if diagnostic.code == "AVELIN_TASK_FAILED" {
        "执行端报告失败，但未提供具体原因。"
    } else {
        "任务结果和清理状态尚未确认。"
    };
    let mut lines = vec![format!("{}\n{reason}", diagnostic.code)];
    lines.push(report_guidance(saved_report, english));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_backend_reason_has_stable_code_and_observed_progress() {
        let progress = Progress {
            phase: ProgressPhase::Extracting,
            completed: 1,
            total: 3,
        };
        let cleanup = Progress {
            phase: ProgressPhase::CleanupCompleted,
            completed: 2,
            total: 3,
        };
        let diagnostic =
            diagnostic("Failed", "", Some(cleanup), Some(progress), Some(3), None).unwrap();
        let value = serde_json::to_value(&diagnostic).unwrap();
        assert_eq!(value["code"], "AVELIN_TASK_FAILED");
        assert_eq!(value["last_progress"]["phase"], "cleanup_completed");
        assert_eq!(value["last_work_progress"]["completed"], 1);
        assert_eq!(value["observed_backend_exit_code"], 3);
        let chinese = terminal_detail(
            "",
            Some(&diagnostic),
            Some(Path::new(r"C:\证据\report.json")),
            false,
        );
        assert!(chinese.contains("未提供具体原因"));
        assert!(chinese.contains(r"已保存报告：C:\证据\report.json"));
        assert!(!chinese.contains("权限") && !chinese.contains("磁盘"));
    }

    #[test]
    fn coordinator_reason_is_retained_without_claiming_backend_details() {
        let message = "cancel cleanup incomplete";
        let diagnostic = diagnostic("Failed", message, None, None, Some(2), None).unwrap();
        assert_eq!(diagnostic.code, "AVELIN_TASK_FAILED");
        assert_eq!(diagnostic.reason, message);
        assert_eq!(
            diagnostic.backend_details,
            "not_provided_by_current_protocol"
        );
        let detail = terminal_detail(message, Some(&diagnostic), None, true);
        assert!(detail.contains(message));
        assert_eq!(diagnostic.observed_backend_exit_code, Some(2));
        assert!(detail.contains("No report was saved successfully."));
        assert!(!detail.contains("Saved report:"));
    }

    #[test]
    fn native_reason_is_bounded_and_does_not_override_independent_verification() {
        let native = diagnostic("Failed", "", None, None, Some(3), None)
            .unwrap()
            .with_backend_detail(Some("owned destination changed before publication"));
        assert_eq!(native.reason_source, "backend");
        assert_eq!(
            native.reason,
            "owned destination changed before publication"
        );
        assert_eq!(native.backend_details, "terminal_event_metadata");
        let verified = diagnostic(
            "Unknown",
            "persistent evidence mismatch",
            None,
            None,
            Some(0),
            None,
        )
        .unwrap()
        .with_backend_detail(Some("installation finished"));
        assert_eq!(verified.reason, "persistent evidence mismatch");
        assert_eq!(verified.reason_source, "coordinator");
        let long = diagnostic("Unknown", "", None, None, Some(4), None)
            .unwrap()
            .with_backend_detail(Some(&"界".repeat(4096)));
        assert_eq!(long.reason.chars().count(), 2048);
    }

    #[test]
    fn configured_injection_is_evidence_not_a_failure_cause() {
        let diagnostic = diagnostic("Failed", "", None, None, Some(3), Some(2)).unwrap();
        let value = serde_json::to_value(&diagnostic).unwrap();
        assert_eq!(value["configured_fault_after_files"], 2);
        assert_eq!(value["fault_configuration_proves_cause"], false);
        let english = terminal_detail("", Some(&diagnostic), None, true);
        assert!(english.contains("without a detailed reason"));
        assert!(!english.contains("caused by"));
    }

    #[test]
    fn unknown_keeps_uncertainty_and_does_not_invent_an_exit_or_saved_path() {
        let diagnostic = diagnostic("Unknown", "", None, None, None, None).unwrap();
        let detail = terminal_detail("", Some(&diagnostic), None, true);
        assert!(detail.contains("AVELIN_RESULT_UNKNOWN"));
        assert!(detail.contains("cleanup have not been confirmed"));
        assert_eq!(diagnostic.observed_backend_exit_code, None);
        assert!(!detail.contains("Saved report:") && !detail.contains("exit code: 0"));
    }

    #[test]
    fn success_and_cancellation_do_not_gain_failure_diagnostics() {
        for result in ["Succeeded", "Cancelled"] {
            let diagnostic = diagnostic(result, "existing detail", None, None, Some(0), Some(2));
            assert!(diagnostic.is_none());
            assert_eq!(
                terminal_detail(
                    "existing detail",
                    diagnostic.as_ref(),
                    Some(Path::new("report.json")),
                    false
                ),
                "existing detail"
            );
        }
    }
}
