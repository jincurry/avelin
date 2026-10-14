mod component_selection;
mod coordinator;
mod elevation;
mod execution_files;
mod failure_report;
mod gui_stdio;
mod installed_maintenance;
mod maintenance;
#[cfg(feature = "gui")]
mod online_update;
mod preflight;
mod resources;
mod runtime_log;
mod transport;
mod upgrade;
use clap::Parser;
use failure_report::{FailureRecord, TaskEvidence};

fn main() {
    let args = coordinator::Args::parse();
    let graphical = cfg!(feature = "gui") && coordinator::requests_gui(&args);
    let _gui_streams = if graphical {
        match gui_stdio::initialize() {
            Ok(streams) => Some(streams),
            Err(error) => {
                let record = FailureRecord::new(
                    "AVELIN_GUI_STDIO",
                    &format!("could not initialize graphical output handles: {error}"),
                    3,
                    TaskEvidence::NotStarted,
                    args.report.clone(),
                    diagnostics_snapshot(),
                );
                report_failure(&record, true, args.english);
                std::process::exit(3);
            }
        }
    } else {
        None
    };
    if let Err(error) = execution_files::initialize_dll_search() {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "AVELIN_EXECUTION_SECURITY: {error:#}");
        std::process::exit(3);
    }
    let english = args.english;
    let requested_report = args.report.clone();
    // Help/version/argument errors have already exited, and explicit CLI paths
    // never initialize either the GUI or its process-wide diagnostic logger.
    if graphical && let Err(error) = runtime_log::initialize() {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "AVELIN_DIAGNOSTICS_UNAVAILABLE: {error}");
    }
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| coordinator::entry(args)));
    let code = match outcome {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => {
            let code = coordinator::error_exit_code(&error);
            log::error!(target: "avelin_app::failure", "process_exit_code={code} detail={error:#}");
            runtime_log::flush();
            let record = record_for_error(&error, code, requested_report);
            report_failure(&record, graphical, english);
            code
        }
        Err(payload) => {
            let detail = format!("Rust panic: {}", panic_detail(payload.as_ref()));
            log::error!(target: "avelin_app::failure", "process_exit_code=4 {detail}");
            runtime_log::flush();
            let record = FailureRecord::new(
                "AVELIN_RUNTIME_PANIC",
                &detail,
                4,
                TaskEvidence::Unknown,
                requested_report,
                diagnostics_snapshot(),
            );
            report_failure(&record, graphical, english);
            4 // A panic cannot establish the outcome of an operation already started.
        }
    };
    if graphical {
        use std::io::Write;
        let diagnostics = runtime_log::finish();
        if let Ok(summary) = serde_json::to_string(&diagnostics) {
            let _ = writeln!(std::io::stderr(), "AVELIN_RUNTIME_LOG_SUMMARY {summary}");
        }
    }
    std::process::exit(code);
}

fn diagnostics_snapshot() -> serde_json::Value {
    serde_json::to_value(runtime_log::snapshot())
        .unwrap_or_else(|error| serde_json::json!({"snapshot_unavailable":error.to_string()}))
}

fn record_for_error(
    error: &anyhow::Error,
    code: i32,
    requested_report: Option<std::path::PathBuf>,
) -> FailureRecord {
    let gui_context = error.downcast_ref::<coordinator::GuiFailureContext>();
    let task = if let Some(context) = gui_context {
        if context.dispatcher_join_failed {
            TaskEvidence::Unknown
        } else {
            context
                .task_exit_code
                .map_or(TaskEvidence::NotStarted, |exit_code| {
                    TaskEvidence::Returned { exit_code }
                })
        }
    } else if code == 4 {
        TaskEvidence::Unknown
    } else {
        TaskEvidence::NotReported
    };
    let failure_code = if gui_context.is_some_and(|context| context.ui_failure_kind == "panic") {
        "AVELIN_UI_PANIC"
    } else if gui_context.is_some() || is_ui_error(error) {
        "AVELIN_UI_INIT"
    } else {
        "AVELIN_RUNTIME_ERROR"
    };
    let report = gui_context
        .and_then(|context| context.requested_report.clone())
        .or(requested_report);
    FailureRecord::new(
        failure_code,
        &format!("{error:#}"),
        code,
        task,
        report,
        diagnostics_snapshot(),
    )
    .with_task_locations(gui_context.and_then(|context| context.task_locations.clone()))
}

fn is_ui_error(error: &anyhow::Error) -> bool {
    #[cfg(feature = "gui")]
    {
        error.is::<avelin_ui::UiError>()
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = error;
        false
    }
}

fn panic_detail(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).into()
    } else {
        "non-string panic payload".into()
    }
}

fn report_failure(record: &FailureRecord, graphical: bool, english: bool) {
    use std::io::Write;
    let saved = failure_report::save(record);
    let message = failure_report::message(record, &saved, english);
    let _ = writeln!(std::io::stderr(), "{message}");
    if graphical && let Err(error) = native_error_dialog(&message) {
        let _ = writeln!(std::io::stderr(), "AVELIN_NATIVE_PROMPT_FAILED: {error}");
        log::error!(target: "avelin_app::failure", "AVELIN_NATIVE_PROMPT_FAILED: {error}");
        runtime_log::flush();
    }
}

#[cfg(windows)]
fn native_error_dialog(message: &str) -> Result<(), String> {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(
            window: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            flags: u32,
        ) -> i32;
    }
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let caption: Vec<u16> = "Avelin · 初栖".encode_utf16().chain(Some(0)).collect();
    let result = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            0x00010010,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
fn native_error_dialog(_message: &str) -> Result<(), String> {
    Err("Native error dialog is only available on Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captures_both_rust_panic_payload_forms() {
        assert_eq!(
            panic_detail(&"render initialization failed"),
            "render initialization failed"
        );
        assert_eq!(panic_detail(&String::from("driver error")), "driver error");
        assert_eq!(panic_detail(&42u32), "non-string panic payload");
    }

    #[test]
    fn failure_record_preserves_gui_task_result_without_presenting_a_dialog() {
        let context = coordinator::GuiFailureContext {
            task_exit_code: Some(0),
            dispatcher_join_failed: false,
            ui_failure_kind: "panic".into(),
            requested_report: Some(std::path::PathBuf::from("known-report.json")),
            task_locations: None,
        };
        let error = anyhow::anyhow!("injected UI panic after commit").context(context);
        let record = record_for_error(&error, 3, None);
        assert_eq!(record.failure_code, "AVELIN_UI_PANIC");
        assert_eq!(
            record.task_evidence,
            TaskEvidence::Returned { exit_code: 0 }
        );
        assert_eq!(record.process_exit_code, 3);
        assert_eq!(
            record.requested_report.unwrap(),
            std::path::PathBuf::from("known-report.json")
        );
    }

    #[test]
    fn default_report_location_is_preserved_separately_from_requested_path_and_task_result() {
        let session = std::env::temp_dir().join("AvelinM0-main-location-fixture");
        for code in [0, 2, 3, 4] {
            for kind in ["error", "panic"] {
                let locations = coordinator::TaskLocations {
                    task_id: "known-task-id".into(),
                    session_dir: session.clone(),
                    report_path: session.join("report.json"),
                    saved_report: (code != 3).then(|| session.join("report.json")),
                };
                let error = anyhow::anyhow!("injected UI failure").context(
                    coordinator::GuiFailureContext {
                        task_exit_code: Some(code),
                        dispatcher_join_failed: false,
                        ui_failure_kind: kind.into(),
                        requested_report: None,
                        task_locations: Some(locations.clone()),
                    },
                );
                let process_code = if code == 4 { 4 } else { 3 };
                let record = record_for_error(&error, process_code, None);
                assert_eq!(
                    record.task_evidence,
                    TaskEvidence::Returned { exit_code: code }
                );
                assert_eq!(record.process_exit_code, process_code);
                assert_eq!(record.task_locations, Some(locations));
                assert!(record.requested_report.is_none());
                assert_eq!(
                    record.failure_code,
                    if kind == "panic" {
                        "AVELIN_UI_PANIC"
                    } else {
                        "AVELIN_UI_INIT"
                    }
                );
            }
        }
        let error = anyhow::anyhow!("preflight failed before a task session existed");
        let record = record_for_error(&error, 3, None);
        assert_eq!(record.task_evidence, TaskEvidence::NotReported);
        assert!(record.task_locations.is_none());
    }

    #[test]
    fn dispatcher_join_failure_is_unknown_without_inventing_a_returned_task_code() {
        let session = std::env::temp_dir().join("AvelinM0-dispatcher-location-fixture");
        let locations = coordinator::TaskLocations {
            task_id: "possibly-active-task".into(),
            session_dir: session.clone(),
            report_path: session.join("report.json"),
            saved_report: None,
        };
        let error = anyhow::anyhow!("dispatcher join failed before worker completion was known")
            .context(coordinator::GuiFailureContext {
                task_exit_code: None,
                dispatcher_join_failed: true,
                ui_failure_kind: "error".into(),
                requested_report: None,
                task_locations: Some(locations.clone()),
            });
        let record = record_for_error(&error, 4, None);
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["task_evidence"]["state"], "Unknown");
        assert!(json["task_evidence"].get("exit_code").is_none());
        assert_eq!(record.process_exit_code, 4);
        assert_eq!(record.task_locations, Some(locations));
        let detail = format!("{error:#}");
        assert!(!detail.contains("task returned exit code"));
        assert!(!detail.contains("no coordinator task was started"));
    }
}
