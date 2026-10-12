//! Failure presentation is independent of GPUI and never changes a task result.

use crate::coordinator::TaskLocations;
use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_DETAIL_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state")]
pub enum TaskEvidence {
    NotStarted,
    Returned { exit_code: i32 },
    Unknown,
    NotReported,
}

#[derive(Debug, Serialize)]
pub struct FailureRecord {
    pub schema_version: u32,
    pub timestamp_unix_ms: u128,
    pub version: &'static str,
    pub pid: u32,
    pub failure_code: &'static str,
    pub process_exit_code: i32,
    pub task_evidence: TaskEvidence,
    pub requested_report: Option<PathBuf>,
    /// Locations recorded by the coordinator, independently of the task result.
    pub task_locations: Option<TaskLocations>,
    pub detail: String,
    pub detail_truncated: bool,
    pub runtime_diagnostics: serde_json::Value,
}

impl FailureRecord {
    pub fn new(
        failure_code: &'static str,
        detail: &str,
        process_exit_code: i32,
        task_evidence: TaskEvidence,
        requested_report: Option<PathBuf>,
        runtime_diagnostics: serde_json::Value,
    ) -> Self {
        let mut end = detail.len().min(MAX_DETAIL_BYTES);
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            schema_version: 1,
            timestamp_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            version: env!("CARGO_PKG_VERSION"),
            pid: std::process::id(),
            failure_code,
            process_exit_code,
            task_evidence,
            requested_report,
            task_locations: None,
            detail: detail[..end].to_owned(),
            detail_truncated: end != detail.len(),
            runtime_diagnostics,
        }
    }

    pub fn with_task_locations(mut self, locations: Option<TaskLocations>) -> Self {
        self.task_locations = locations;
        self
    }
}

#[derive(Debug, Serialize)]
pub struct LogAttempt {
    pub directory: PathBuf,
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct SavedFailure {
    pub path: Option<PathBuf>,
    pub failed_attempts: Vec<LogAttempt>,
}

pub fn save(record: &FailureRecord) -> SavedFailure {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|base| base.join("Avelin/M0/logs"));
    let fallback = std::env::temp_dir().join("AvelinM0/logs");
    save_to(record, local.into_iter().chain(std::iter::once(fallback)))
}

fn save_to(record: &FailureRecord, bases: impl IntoIterator<Item = PathBuf>) -> SavedFailure {
    let mut saved = SavedFailure {
        path: None,
        failed_attempts: Vec::new(),
    };
    for base in bases {
        let attempt = (|| -> anyhow::Result<PathBuf> {
            avelin_core::reject_reparse(&base)?;
            fs::create_dir_all(&base)?;
            avelin_core::reject_reparse(&base)?;
            let path = base.join(format!(
                "launch-error-{}.json",
                uuid::Uuid::new_v4().simple()
            ));
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)?;
            let bytes = serde_json::to_vec_pretty(record)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            Ok(path)
        })();
        match attempt {
            Ok(path) => {
                saved.path = Some(path);
                break;
            }
            Err(error) => saved.failed_attempts.push(LogAttempt {
                directory: base,
                error: format!("{error:#}"),
            }),
        }
    }
    saved
}

fn display_text(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// Build both console and native-dialog text from the same evidence.
/// A returned task status and a GUI/process failure are deliberately separate.
pub fn message(record: &FailureRecord, saved: &SavedFailure, english: bool) -> String {
    let task: String = match (&record.task_evidence, english) {
        (TaskEvidence::NotStarted, true) => "No installation task was started by this GUI session.".into(),
        (TaskEvidence::NotStarted, false) => "本次界面会话未启动安装任务。".into(),
        (TaskEvidence::Returned { exit_code: 0 }, true) => "The installation task returned Succeeded (0). This interface failure does not undo it; verify the task report before any further action.".into(),
        (TaskEvidence::Returned { exit_code: 0 }, false) => "安装任务已返回成功（0）。界面故障不会撤销该结果，请先核对任务报告。".into(),
        (TaskEvidence::Returned { exit_code: 2 }, true) => "The task returned Cancelled (2). Verify its report; this interface failure does not replace that result.".into(),
        (TaskEvidence::Returned { exit_code: 2 }, false) => "任务已返回取消（2）。请核对任务报告；界面故障不会替换该结果。".into(),
        (TaskEvidence::Returned { exit_code: 3 }, true) => "The task returned Failed (3). If a task report was generated, inspect it for changes and cleanup; the process failure alone does not prove cleanup.".into(),
        (TaskEvidence::Returned { exit_code: 3 }, false) => "任务已返回失败（3）。如已生成任务报告，请核对其中的变更与清理情况；进程错误本身不证明清理完成。".into(),
        (TaskEvidence::Unknown | TaskEvidence::Returned { exit_code: 4 }, true) => "The task outcome is Unknown (4). Review the task report and backend process before retrying; cleanup is unconfirmed.".into(),
        (TaskEvidence::Unknown | TaskEvidence::Returned { exit_code: 4 }, false) => "任务结果未确认（4）。请先核对任务报告和后台进程，再决定后续操作；清理尚未确认。".into(),
        (_, true) => "No confirmed task result is attached to this failure. Review any existing task report before retrying.".into(),
        (_, false) => "此错误未附带已确认的任务结果。请先核对已有任务报告，再决定后续操作。".into(),
    };
    let log = match (&saved.path, english) {
        (Some(path), true) => format!("Failure log: {}", path.display()),
        (Some(path), false) => format!("错误日志：{}", path.display()),
        (None, true) => "Failure log was NOT saved. Keep this error text.".into(),
        (None, false) => "错误日志未能保存，请保留此错误信息。".into(),
    };
    let mut text = format!(
        "Avelin · {}\n{}: {}\n\n{}\n\n{}\n\n{}",
        record.failure_code,
        if english {
            "Process exit code"
        } else {
            "进程退出码"
        },
        record.process_exit_code,
        display_text(&record.detail),
        task,
        log
    );
    if record.detail_truncated {
        text.push_str(if english {
            "\nError detail was truncated to 16 KiB."
        } else {
            "\n错误详情已截断至 16 KiB。"
        });
    }
    if let Some(report) = &record.requested_report {
        text.push_str(&format!(
            "\n{}{}",
            if english {
                "Requested task report (its existence must be checked): "
            } else {
                "指定任务报告（需核对是否已生成）："
            },
            report.display()
        ));
    }
    if let Some(locations) = &record.task_locations {
        text.push_str(&format!(
            "\n{}{}\n{}{}\n{}{}",
            if english { "Task ID: " } else { "任务 ID：" },
            locations.task_id,
            if english {
                "Task session: "
            } else {
                "任务会话："
            },
            locations.session_dir.display(),
            if english {
                "Resolved task report (allocation does not prove it was written): "
            } else {
                "实际任务报告位置（分配路径不代表已写入）："
            },
            locations.report_path.display()
        ));
        match (&locations.saved_report, english) {
            (Some(path), true) => text.push_str(&format!(
                "\nReport write completed at (verify the current file): {}",
                path.display()
            )),
            (Some(path), false) => text.push_str(&format!(
                "\n报告写入已完成（仍需核对当前文件）：{}",
                path.display()
            )),
            (None, true) => text.push_str("\nNo completed task report write was recorded."),
            (None, false) => text.push_str("\n未记录到任务报告写入完成。"),
        }
    }
    if let Some(path) = record
        .runtime_diagnostics
        .get("log_path")
        .and_then(|value| value.as_str())
    {
        text.push_str(&format!(
            "\n{}{}",
            if english {
                "Runtime diagnostics: "
            } else {
                "运行时诊断："
            },
            path
        ));
    }
    if !saved.failed_attempts.is_empty() {
        text.push_str(if english {
            "\nLog write attempts failed:"
        } else {
            "\n日志写入失败详情："
        });
        for attempt in &saved.failed_attempts {
            text.push_str(&format!(
                "\n{}: {}",
                attempt.directory.display(),
                display_text(&attempt.error)
            ));
        }
    }
    text.push_str(if english {
        "\n\nUse --help for available options. Explicit command-line execution requires --install-dir; use --report to choose a report path. No silent installation was started automatically."
    } else {
        "\n\n可用 --help 查看参数。显式命令行执行需指定 --install-dir；可用 --report 自选报告路径。未自动切换为静默安装。"
    });
    display_text(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(task: TaskEvidence) -> FailureRecord {
        FailureRecord::new(
            "AVELIN_UI_INIT",
            "injected detail",
            3,
            task,
            None,
            serde_json::Value::Null,
        )
    }
    fn test_root() -> PathBuf {
        let root = std::env::temp_dir()
            .join("AvelinM0FailureTests")
            .join(uuid::Uuid::new_v4().simple().to_string());
        fs::create_dir_all(&root).unwrap();
        root
    }
    #[test]
    fn bounded_unicode_detail_and_nul_safe_presentation() {
        let detail = format!("bad\0{}", "中".repeat(10_000));
        let record = FailureRecord::new(
            "AVELIN_UI_INIT",
            &detail,
            3,
            TaskEvidence::NotStarted,
            None,
            serde_json::Value::Null,
        );
        assert!(record.detail_truncated);
        assert!(record.detail.len() <= MAX_DETAIL_BYTES);
        let text = message(
            &record,
            &SavedFailure {
                path: None,
                failed_attempts: vec![],
            },
            true,
        );
        assert!(!text.contains('\0'));
        assert!(text.contains("NOT saved"));
        assert!(text.contains("No installation task was started"));
    }
    #[test]
    fn known_result_and_unknown_are_presented_separately_from_process_failure() {
        let saved = SavedFailure {
            path: None,
            failed_attempts: vec![],
        };
        let known = message(
            &fixture(TaskEvidence::Returned { exit_code: 0 }),
            &saved,
            true,
        );
        assert!(known.contains("Succeeded (0)"));
        assert!(known.contains("Process exit code: 3"));
        assert!(!known.contains("outcome is Unknown"));
        let unknown = message(
            &fixture(TaskEvidence::Returned { exit_code: 4 }),
            &saved,
            false,
        );
        assert!(unknown.contains("任务结果未确认"));
        assert!(unknown.contains("清理尚未确认"));
    }
    #[test]
    fn failed_primary_directory_falls_back_with_preserved_reason() {
        let root = test_root();
        let blocked = root.join("file-not-directory");
        fs::write(&blocked, b"owned sentinel").unwrap();
        let record = fixture(TaskEvidence::NotStarted);
        let first = save_to(&record, [blocked.clone(), root.join("fallback")]);
        assert_eq!(first.failed_attempts.len(), 1);
        let path = first.path.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["failure_code"], "AVELIN_UI_INIT");
        assert_eq!(json["task_evidence"]["state"], "NotStarted");
        let second = save_to(&record, [root.join("fallback")]);
        assert_ne!(second.path.as_ref(), Some(&path));
        assert_eq!(fs::read(&blocked).unwrap(), b"owned sentinel");
    }
    #[test]
    fn all_unwritable_candidates_are_reported_without_claiming_saved_log() {
        let root = test_root();
        let blocked = root.join("file-not-directory");
        fs::write(&blocked, b"owned sentinel").unwrap();
        let record = fixture(TaskEvidence::Unknown);
        let saved = save_to(&record, [blocked.clone(), blocked.join("nested")]);
        assert!(saved.path.is_none());
        assert_eq!(saved.failed_attempts.len(), 2);
        let text = message(&record, &saved, true);
        assert!(text.contains("Failure log was NOT saved"));
        assert!(text.contains("Log write attempts failed"));
    }

    #[test]
    fn task_locations_survive_saved_failure_and_localized_presentation() {
        let root = test_root();
        let session = root.join("任务会话");
        let resolved = session.join("report.json");
        let fallback = session.join("unknown-result.json");
        let locations = TaskLocations {
            task_id: "task-location-fixture".into(),
            session_dir: session.clone(),
            report_path: resolved.clone(),
            saved_report: Some(fallback.clone()),
        };
        let record =
            fixture(TaskEvidence::Returned { exit_code: 4 }).with_task_locations(Some(locations));
        let saved = save_to(&record, [root.join("failure-logs")]);
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(saved.path.as_ref().unwrap()).unwrap()).unwrap();
        assert!(json["requested_report"].is_null());
        assert_eq!(
            json["task_locations"]["session_dir"],
            session.to_str().unwrap()
        );
        assert_eq!(
            json["task_locations"]["report_path"],
            resolved.to_str().unwrap()
        );
        assert_eq!(
            json["task_locations"]["saved_report"],
            fallback.to_str().unwrap()
        );
        for english in [false, true] {
            let text = message(&record, &saved, english);
            assert!(text.contains("task-location-fixture"));
            assert!(text.contains(resolved.to_str().unwrap()));
            assert!(text.contains(fallback.to_str().unwrap()));
            assert!(text.contains(if english {
                "verify the current file"
            } else {
                "仍需核对当前文件"
            }));
        }
    }

    #[test]
    fn allocated_location_never_implies_report_saved_or_task_started() {
        let root = test_root();
        let record = fixture(TaskEvidence::Returned { exit_code: 3 }).with_task_locations(Some(
            TaskLocations {
                task_id: "allocated-only".into(),
                session_dir: root.clone(),
                report_path: root.join("never-written.json"),
                saved_report: None,
            },
        ));
        let saved = SavedFailure {
            path: None,
            failed_attempts: vec![],
        };
        for english in [false, true] {
            let text = message(&record, &saved, english);
            assert!(text.contains(if english {
                "No completed task report write was recorded"
            } else {
                "未记录到任务报告写入完成"
            }));
            assert!(!text.contains(if english {
                "Report write completed at"
            } else {
                "报告写入已完成"
            }));
            let text = message(&fixture(TaskEvidence::NotStarted), &saved, english);
            assert!(!text.contains(if english {
                "Task session:"
            } else {
                "任务会话："
            }));
        }
    }
}
