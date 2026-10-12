use anyhow::{Context, Result, ensure};
use avelin_core::{
    Manifest, RECEIPT_NAME, Receipt, hash_bytes, hash_file, install_root, reject_reparse,
    session::{SessionRecord, process_status},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::Path,
};

fn registry_state(key: &str, install_dir: &Path) -> Value {
    use std::{ffi::c_void, ptr};
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(
            key: *mut c_void,
            subkey: *const u16,
            options: u32,
            access: u32,
            output: *mut *mut c_void,
        ) -> i32;
        fn RegQueryValueExW(
            key: *mut c_void,
            name: *const u16,
            reserved: *mut u32,
            kind: *mut u32,
            data: *mut u8,
            length: *mut u32,
        ) -> i32;
        fn RegCloseKey(key: *mut c_void) -> i32;
    }
    struct Key(*mut c_void);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    let subkey: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let name: Vec<u16> = "InstallLocation".encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut raw = ptr::null_mut();
        let status = RegOpenKeyExW(
            0x80000001u32 as i32 as isize as *mut c_void,
            subkey.as_ptr(),
            0,
            0x20119,
            &mut raw,
        );
        if matches!(status, 2 | 3) {
            return json!({"state":"Missing","key":key});
        }
        if status != 0 {
            return json!({"state":"Unconfirmed","win32_error":status,"key":key});
        }
        let key_handle = Key(raw);
        let (mut kind, mut length) = (0, 0);
        let status = RegQueryValueExW(
            key_handle.0,
            name.as_ptr(),
            ptr::null_mut(),
            &mut kind,
            ptr::null_mut(),
            &mut length,
        );
        if status != 0 || kind != 1 || length > 65536 || length % 2 != 0 {
            return json!({"state":"Present","location_matches":false,"win32_error":status});
        }
        let mut value = vec![0u16; length as usize / 2];
        let status = RegQueryValueExW(
            key_handle.0,
            name.as_ptr(),
            ptr::null_mut(),
            &mut kind,
            value.as_mut_ptr().cast(),
            &mut length,
        );
        if status != 0 || kind != 1 || length % 2 != 0 {
            return json!({"state":"Unconfirmed","win32_error":status});
        }
        value.truncate(length as usize / 2);
        if value.last() == Some(&0) {
            value.pop();
        }
        match String::from_utf16(&value) {
            Ok(location) => {
                json!({"state":"Present","install_location":location,"location_matches":avelin_core::same_windows_name(&location,&install_dir.to_string_lossy())})
            }
            Err(_) => json!({"state":"Unconfirmed","error":"invalid registry UTF-16"}),
        }
    }
}

fn read_json<T: DeserializeOwned>(path: &Path, maximum: u64) -> Result<T> {
    reject_reparse(path)?;
    ensure!(
        fs::metadata(path)?.len() <= maximum,
        "diagnostic input exceeds size bound"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= maximum,
        "diagnostic input grew beyond size bound"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn file_state(path: &Path, expected: Option<&str>) -> Value {
    let observe = || -> Result<Value> {
        reject_reparse(path)?;
        match fs::metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(json!({"state":"Missing"}))
            }
            Err(error) => Err(error.into()),
            Ok(metadata) => {
                ensure!(metadata.is_file(), "expected a regular file");
                if let Some(expected) = expected {
                    let actual = hash_file(path)?;
                    Ok(
                        json!({"state":if actual==expected {"Matches"} else {"Changed"},"sha256":actual,"size":metadata.len()}),
                    )
                } else {
                    Ok(json!({"state":"Present","size":metadata.len()}))
                }
            }
        }
    };
    observe().unwrap_or_else(|error| json!({"state":"Unconfirmed","error":format!("{error:#}")}))
}

fn journal_summary(path: &Path, task: &str) -> Value {
    let observe = || -> Result<Value> {
        reject_reparse(path)?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!({"state":"Missing"}));
            }
            Err(error) => return Err(error.into()),
        };
        let mut reader = BufReader::new(file);
        let mut count = 0;
        let mut last = Value::Null;
        loop {
            let mut line = String::new();
            let size = reader.by_ref().take(32769).read_line(&mut line)?;
            if size == 0 {
                break;
            }
            ensure!(
                size <= 32768 && line.ends_with('\n'),
                "truncated or oversized journal record"
            );
            let event: Value = serde_json::from_str(&line)?;
            ensure!(event["task_id"] == task, "journal task identity mismatch");
            count += 1;
            ensure!(count <= 200_000, "diagnostic journal record limit exceeded");
            // Whitelist fields; never echo an arbitrary JSON record or session token.
            last = json!({"kind":event["kind"],"phase":event["phase"],"completed":event["completed"],
                "result":event["result"],"code":event["code"],"coordinator_received_elapsed_ms":event["coordinator_received_elapsed_ms"]});
        }
        Ok(json!({"state":"Read","event_count":count,"last_event":last}))
    };
    observe().unwrap_or_else(|error| json!({"state":"Unconfirmed","error":format!("{error:#}")}))
}

fn assessment(
    processes: &[Value],
    recorded: Option<&str>,
    install_matches: bool,
    removed: bool,
    operation: &str,
    evidence_valid: bool,
) -> &'static str {
    if processes.iter().any(|p| p["state"] == "Running") {
        return "Active";
    }
    if !evidence_valid || processes.iter().any(|p| p["state"] == "Unconfirmed") {
        return "NeedsReview";
    }
    match recorded {
        Some("Succeeded")
            if (matches!(operation, "install" | "upgrade") && install_matches)
                || (operation == "uninstall" && removed) =>
        {
            "RecordedStateMatchesObservedResources"
        }
        Some("Cancelled") if operation == "install" && removed => {
            "RecordedStateMatchesObservedResources"
        }
        _ => "NeedsReview",
    }
}

fn terminal_agrees(report: &Value, journal: &Value) -> bool {
    let expected = match report["recorded_result"].as_str() {
        Some("Succeeded") => 0,
        Some("Cancelled") => 2,
        Some("Failed") => 3,
        _ => return false,
    };
    report["state"] == "Read"
        && journal["state"] == "Read"
        && report["backend_exit_code"] == expected
        && report["coordinator_exit_code"] == expected
        && journal["last_event"]["kind"] == "finish"
        && journal["last_event"]["result"] == report["recorded_result"]
        && journal["last_event"]["code"] == expected
}

/// Read-only: never releases a lock, executes a backend, edits a receipt or deletes resources.
pub fn inspect(session: &Path) -> Result<Value> {
    let session = std::path::absolute(session)?;
    reject_reparse(&session)?;
    let record: SessionRecord = read_json(&session.join("session.json"), 64 * 1024).context(
        "session.json is missing or invalid; older M0 sessions require manual inspection",
    )?;
    ensure!(
        record.schema_version == 1
            && matches!(
                record.operation.as_str(),
                "install" | "uninstall" | "upgrade"
            ),
        "unsupported diagnostic session schema or operation"
    );
    let manifest: Manifest = read_json(&session.join("manifest.json"), 32 * 1024 * 1024)?;
    manifest.validate()?;
    ensure!(
        record.app_id == manifest.app_id
            && record.manifest_sha256 == hash_bytes(&serde_json::to_vec(&manifest)?),
        "session manifest identity mismatch"
    );
    install_root(&record.install_dir)?;
    let processes = [
        process_status(&record.coordinator),
        process_status(&record.backend),
    ];
    let files: Vec<Value> = manifest
        .files
        .iter()
        .map(|resource| {
            let mut state = file_state(
                &record.install_dir.join(&resource.path),
                Some(&resource.sha256),
            );
            state["path"] = json!(resource.path);
            state
        })
        .collect();
    let receipt_path = record.install_dir.join(RECEIPT_NAME);
    let receipt_presence = file_state(&receipt_path, None);
    let receipt = if receipt_presence["state"] == "Present" {
        match read_json::<Receipt>(&receipt_path, 32 * 1024 * 1024) {
            Ok(receipt) => {
                json!({"state":if receipt.schema_version==1 && receipt.app_id==record.app_id
                && receipt.manifest_sha256==record.manifest_sha256 && receipt.install_dir==record.install_dir.to_string_lossy()
                && (record.operation=="uninstall" || receipt.task_id==record.task_id) {"Matches"} else {"IdentityMismatch"},
                "task_id":receipt.task_id,"commit_id":receipt.commit_id})
            }
            Err(error) => json!({"state":"Unconfirmed","error":format!("{error:#}")}),
        }
    } else {
        receipt_presence
    };
    let runtime = file_state(
        &record.install_dir.join(".avelin/avelin-app.exe"),
        manifest.runtime_sha256.as_deref(),
    );
    let uninstaller = file_state(&record.install_dir.join("uninstall.exe"), None);
    let installed_manifest_path = record.install_dir.join(".avelin/manifest.json");
    let mut installed_manifest = file_state(&installed_manifest_path, None);
    if installed_manifest["state"] == "Present" {
        installed_manifest = match read_json::<Manifest>(&installed_manifest_path, 32 * 1024 * 1024)
        {
            Ok(installed)
                if hash_bytes(&serde_json::to_vec(&installed)?) == record.manifest_sha256 =>
            {
                json!({"state":"Matches"})
            }
            Ok(_) => json!({"state":"Changed"}),
            Err(error) => json!({"state":"Unconfirmed","error":format!("{error:#}")}),
        };
    }
    let lock = file_state(&record.product_lock, None);
    let report = match read_json::<Value>(&record.report_path, 32 * 1024 * 1024) {
        Ok(report)
            if report["task_id"] == record.task_id
                && report["install_dir"].as_str() == record.install_dir.to_str() =>
        {
            json!({"state":"Read","recorded_result":report["result"],"backend_exit_code":report["exit_code"],"coordinator_exit_code":report["coordinator_exit_code"]})
        }
        Ok(_) => json!({"state":"IdentityMismatch"}),
        Err(error) => json!({"state":"Unconfirmed","error":format!("{error:#}")}),
    };
    let key = format!(
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.{}",
        record.app_id
    );
    let registry = registry_state(&key, &record.install_dir);
    let install_matches = files.iter().all(|f| f["state"] == "Matches")
        && receipt["state"] == "Matches"
        && runtime["state"] == "Matches"
        && uninstaller["state"] == "Present"
        && installed_manifest["state"] == "Matches"
        && registry["state"] == "Present"
        && registry["location_matches"] == true;
    let removed = files.iter().all(|f| f["state"] == "Missing")
        && receipt["state"] == "Missing"
        && runtime["state"] == "Missing"
        && uninstaller["state"] == "Missing"
        && installed_manifest["state"] == "Missing"
        && registry["state"] == "Missing";
    let journal = journal_summary(&session.join("events.jsonl"), &record.task_id);
    let marker_path = record.install_dir.join(".avelin/upgrade-state.json");
    let upgrade_marker = upgrade_marker_state(&marker_path);
    let upgrade_journal = if record.operation == "upgrade" {
        upgrade_journal_summary(&session.join("upgrade-journal.jsonl"), &record.task_id)
    } else {
        Value::Null
    };
    let evidence_valid = terminal_agrees(&report, &journal)
        && registry["state"] != "Unconfirmed"
        && lock["state"] == "Missing"
        && upgrade_marker["state"] == "Missing"
        && (record.operation != "upgrade"
            || (upgrade_journal["state"] == "Read"
                && upgrade_journal["last_event"]["event"] == "committed"));
    let conclusion = assessment(
        &processes,
        report["recorded_result"].as_str(),
        install_matches,
        removed,
        &record.operation,
        evidence_valid,
    );
    Ok(
        json!({"schema_version":1,"read_only":true,"task_id":record.task_id,"app_id":record.app_id,
        "operation":record.operation,"session_dir":session,"install_dir":record.install_dir,"assessment":conclusion,
        "processes":{"coordinator":processes[0],"backend":processes[1]},"recorded_report":report,
        "receipt":receipt,"files":files,"runtime":runtime,"installed_manifest":installed_manifest,"uninstaller":uninstaller,
        "registry":registry,"product_lock":lock,"journal":journal,"automatic_retry_allowed":false,
        "upgrade_transaction":{"marker":upgrade_marker,"journal":upgrade_journal},
        "next_step":if conclusion=="Active" {"Wait for this task to stop; do not start another backend."} else if upgrade_marker["state"]!="Missing" {"Upgrade state is not finalized. Preserve the marker, staging, backup and session journal; inspect recovery evidence before modifying this installation. This command never removes the marker or runs recovery."} else {"Review recorded results and resource differences. This command never clears a lock or retries an installation."}}),
    )
}

fn upgrade_marker_state(path: &Path) -> Value {
    let presence = file_state(path, None);
    if presence["state"] != "Present" {
        return presence;
    }
    match read_json::<Value>(path, 64 * 1024) {
        Ok(marker) => json!({"state":"Present","path":path,"task_id":marker["task_id"],
            "session_dir":marker["session_dir"],
            "journal_path":marker.get("journal").or_else(|| marker.get("journal_path")),
            "meaning":"An upgrade is not finalized. Do not start another modifying operation."}),
        Err(error) => json!({"state":"Unconfirmed","path":path,"error":format!("{error:#}")}),
    }
}

fn upgrade_journal_summary(path: &Path, task: &str) -> Value {
    let observe = || -> Result<Value> {
        reject_reparse(path)?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!({"state":"Missing"}));
            }
            Err(error) => return Err(error.into()),
        };
        let mut reader = BufReader::new(file);
        let mut count = 0usize;
        let mut last = Value::Null;
        loop {
            let mut line = String::new();
            let size = reader.by_ref().take(32769).read_line(&mut line)?;
            if size == 0 {
                break;
            }
            ensure!(
                size <= 32768 && line.ends_with('\n'),
                "truncated or oversized upgrade journal record"
            );
            let event: Value = serde_json::from_str(&line)?;
            ensure!(
                event["schema_version"] == 1 && event["task_id"] == task,
                "upgrade journal task identity mismatch"
            );
            count += 1;
            ensure!(count <= 1_000_000, "upgrade journal record limit exceeded");
            last = json!({"event":event["event"],"path":event["path"],"sha256":event["sha256"],"result":event["result"]});
        }
        Ok(json!({"state":"Read","event_count":count,"last_event":last,"path":path}))
    };
    observe().unwrap_or_else(|error| json!({"state":"Unconfirmed","error":format!("{error:#}")}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upgrade_diagnostics_reject_truncated_or_other_task_records() {
        let path = std::env::temp_dir().join(format!(
            "avelin-upgrade-diagnose-{}.jsonl",
            uuid::Uuid::new_v4().simple()
        ));
        fs::write(&path, "{\"schema_version\":1,\"task_id\":\"task\",\"event\":\"committed\",\"result\":\"verified\"}\n").unwrap();
        assert_eq!(upgrade_journal_summary(&path, "task")["state"], "Read");
        assert_eq!(
            upgrade_journal_summary(&path, "other")["state"],
            "Unconfirmed"
        );
        fs::write(&path, "{\"schema_version\":1,\"task_id\":\"task\"}").unwrap();
        assert_eq!(
            upgrade_journal_summary(&path, "task")["state"],
            "Unconfirmed"
        );
        // Real markers use journal; accept the older journal_path spelling
        // without following or modifying either untrusted diagnostic path.
        for field in ["journal", "journal_path"] {
            let mut marker =
                json!({"schema_version":1,"task_id":"task","session_dir":"C:\\evidence"});
            marker[field] = json!("C:\\evidence\\upgrade-journal.jsonl");
            let bytes = serde_json::to_vec(&marker).unwrap();
            fs::write(&path, &bytes).unwrap();
            let observed = upgrade_marker_state(&path);
            assert_eq!(observed["state"], "Present");
            assert_eq!(observed["task_id"], "task");
            assert_eq!(observed["journal_path"], marker[field]);
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn conflicting_exit_codes_or_missing_terminal_cannot_support_success() {
        let mut report = json!({"state":"Read","recorded_result":"Succeeded","backend_exit_code":0,"coordinator_exit_code":0});
        let mut journal =
            json!({"state":"Read","last_event":{"kind":"finish","result":"Succeeded","code":0}});
        assert!(terminal_agrees(&report, &journal));
        report["backend_exit_code"] = json!(3);
        assert!(!terminal_agrees(&report, &journal));
        report["backend_exit_code"] = json!(0);
        journal["last_event"]["kind"] = json!("progress");
        assert!(!terminal_agrees(&report, &journal));
    }
    #[test]
    fn a_report_never_overrides_an_active_process_or_conflicting_resources() {
        let stopped = [json!({"state":"NotRunning"})];
        assert_eq!(
            assessment(&stopped, Some("Succeeded"), true, false, "install", true),
            "RecordedStateMatchesObservedResources"
        );
        assert_eq!(
            assessment(&stopped, Some("Succeeded"), false, false, "install", true),
            "NeedsReview"
        );
        assert_eq!(
            assessment(
                &[json!({"state":"Running"})],
                Some("Succeeded"),
                true,
                false,
                "install",
                true
            ),
            "Active"
        );
        assert_eq!(
            assessment(&stopped, None, true, false, "install", true),
            "NeedsReview"
        );
        assert_eq!(
            assessment(&stopped, Some("Succeeded"), true, false, "install", false),
            "NeedsReview"
        );
    }
}
