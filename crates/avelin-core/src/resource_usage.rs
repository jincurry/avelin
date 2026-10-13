//! Read-only observations of processes and file handles that may block maintenance.
//!
//! Restart Manager returns a process list for the whole registered resource set,
//! not a process-to-file mapping. Keep that distinction in diagnostic reports.
//! The observation is advisory: the caller must still handle a new lock appearing
//! between this check and its transaction. This module never stops a process.
use crate::session::ProcessIdentity;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsageProcess {
    pub pid: u32,
    pub creation_time_100ns: u64,
    pub name: String,
    pub executable: Option<String>,
    /// Empty when Restart Manager only supplies a set-wide association.
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedFile {
    pub path: String,
    pub win32_error: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsageIssue {
    pub scope: String,
    pub win32_error: Option<u32>,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsageReport {
    pub processes: Vec<UsageProcess>,
    /// Paths whose delete-access probe encountered a sharing/locking violation.
    /// This does not attribute the lock to any particular process above.
    pub locked_files: Vec<LockedFile>,
    /// Failed checks are not evidence that the resources are available.
    pub issues: Vec<UsageIssue>,
    pub reboot_reasons: u32,
    pub checked_files: usize,
}

impl UsageReport {
    pub fn is_clear(&self) -> bool {
        self.processes.is_empty()
            && self.locked_files.is_empty()
            && self.issues.is_empty()
            && self.reboot_reasons == 0
    }
}

/// Inspect existing, absolute managed file paths without changing their contents.
/// Missing files are normal (for example during repair) and are ignored. Invalid
/// input is an error; operating-system check failures are returned in `issues`.
/// Exclusions require the PID and creation time, so PID reuse cannot bypass a check.
#[cfg(windows)]
pub fn inspect(paths: &[PathBuf], excluded: &[ProcessIdentity]) -> Result<UsageReport> {
    windows::inspect(paths, excluded)
}

#[cfg(not(windows))]
pub fn inspect(_paths: &[PathBuf], _excluded: &[ProcessIdentity]) -> Result<UsageReport> {
    anyhow::bail!("resource usage inspection requires Windows")
}

#[cfg(windows)]
mod windows {
    use super::*;
    use anyhow::{Context, ensure};
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::c_void,
        fs,
        path::Path,
        ptr,
    };

    const REGISTER_BATCH: usize = 256;
    const MAX_QUERY_ATTEMPTS: usize = 5;
    const MAX_PROCESSES: usize = 32_768;
    const ERROR_MORE_DATA: u32 = 234;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    impl FileTime {
        fn value(self) -> u64 {
            ((self.high as u64) << 32) | self.low as u64
        }
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct UniqueProcess {
        pid: u32,
        creation: FileTime,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcessInfo {
        process: UniqueProcess,
        app_name: [u16; 256],
        service_name: [u16; 64],
        app_type: u32,
        app_status: u32,
        session_id: u32,
        restartable: i32,
    }
    impl Default for ProcessInfo {
        fn default() -> Self {
            // All fields are integer scalars/arrays; zero is a valid initial value.
            unsafe { std::mem::zeroed() }
        }
    }
    #[link(name = "rstrtmgr")]
    unsafe extern "system" {
        fn RmStartSession(session: *mut u32, flags: u32, key: *mut u16) -> u32;
        fn RmRegisterResources(
            session: u32,
            file_count: u32,
            files: *const *const u16,
            app_count: u32,
            apps: *const UniqueProcess,
            service_count: u32,
            services: *const *const u16,
        ) -> u32;
        fn RmGetList(
            session: u32,
            needed: *mut u32,
            count: *mut u32,
            apps: *mut ProcessInfo,
            reboot_reasons: *mut u32,
        ) -> u32;
        fn RmEndSession(session: u32) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            sharing: u32,
            security: *mut c_void,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    struct Session(Option<u32>);
    impl Session {
        fn close(mut self, report: &mut UsageReport) {
            let status = unsafe { RmEndSession(self.0.take().unwrap()) };
            if status != 0 {
                issue(report, "RmEndSession", status);
            }
        }
    }
    impl Drop for Session {
        fn drop(&mut self) {
            if let Some(session) = self.0.take() {
                unsafe { RmEndSession(session) };
            }
        }
    }

    fn issue(report: &mut UsageReport, scope: &str, status: u32) {
        report.issues.push(UsageIssue {
            scope: scope.into(),
            win32_error: Some(status),
            message: std::io::Error::from_raw_os_error(status as i32).to_string(),
        });
    }

    fn normalized_paths(paths: &[PathBuf]) -> Result<Vec<String>> {
        let mut unique = BTreeMap::<String, Vec<String>>::new();
        let mut result = Vec::new();
        for path in paths {
            ensure!(
                path.is_absolute(),
                "resource path must be absolute and nonempty"
            );
            let text = path.to_str().context("resource path must be Unicode")?;
            ensure!(
                !text.contains('\0') && text.encode_utf16().count() < 32_767,
                "resource path contains NUL or exceeds the Windows path limit"
            );
            // Inputs already follow the managed-path contract. Reject dot segments
            // instead of silently redirecting a caller's resource selection.
            ensure!(
                !path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir)),
                "resource path contains a parent segment"
            );
            let text = text.replace('/', "\\");
            // Full Unicode uppercasing can merge distinct Windows names (for
            // example a sharp-s and "SS"). Use it only as a bucket, and confirm
            // aliases with Windows ordinal comparison before skipping a path.
            let aliases = unique.entry(text.to_uppercase()).or_default();
            if !aliases
                .iter()
                .any(|alias| crate::same_windows_name(alias, &text))
            {
                aliases.push(text.clone());
                result.push(text);
            }
        }
        Ok(result)
    }

    fn decode_name(value: &[u16]) -> String {
        let end = value
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(value.len());
        String::from_utf16_lossy(&value[..end])
            .chars()
            .filter(|c| !c.is_control())
            .collect()
    }

    fn query(session: u32, report: &mut UsageReport) -> Vec<ProcessInfo> {
        let mut values = Vec::new();
        for _ in 0..MAX_QUERY_ATTEMPTS {
            let mut needed = 0;
            let mut count = values.len() as u32;
            let mut reboot_reasons = 0;
            let status = unsafe {
                RmGetList(
                    session,
                    &mut needed,
                    &mut count,
                    if values.is_empty() {
                        ptr::null_mut()
                    } else {
                        values.as_mut_ptr()
                    },
                    &mut reboot_reasons,
                )
            };
            report.reboot_reasons |= reboot_reasons;
            if status == 0 {
                if count as usize > values.len() {
                    report.issues.push(UsageIssue {
                        scope: "RmGetList".into(),
                        win32_error: None,
                        message: "Restart Manager returned an invalid process count".into(),
                    });
                    return Vec::new();
                }
                values.truncate(count as usize);
                return values;
            }
            if status != ERROR_MORE_DATA {
                issue(report, "RmGetList", status);
                return Vec::new();
            }
            let capacity = (needed as usize).max(values.len().saturating_add(8));
            if capacity > MAX_PROCESSES {
                report.issues.push(UsageIssue {
                    scope: "RmGetList".into(),
                    win32_error: Some(ERROR_MORE_DATA),
                    message: "Restart Manager process list exceeds the inspection limit".into(),
                });
                return Vec::new();
            }
            values.resize(capacity, ProcessInfo::default());
        }
        report.issues.push(UsageIssue {
            scope: "RmGetList".into(),
            win32_error: Some(ERROR_MORE_DATA),
            message: "Resource users kept changing; inspect again before continuing".into(),
        });
        Vec::new()
    }

    fn probe(path: &str, wide: &[u16], report: &mut UsageReport) {
        // Request delete access, which replacement/removal needs, but never mark
        // for deletion or write bytes. Share every access mode ourselves.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0x0001_0000,
                7,
                ptr::null_mut(),
                3,
                0,
                ptr::null_mut(),
            )
        };
        if handle as isize != -1 {
            unsafe { CloseHandle(handle) };
            return;
        }
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(1) as u32;
        match code {
            2 | 3 => {} // A file disappeared since the metadata check.
            32 | 33 => report.locked_files.push(LockedFile {
                path: path.into(),
                win32_error: code,
            }),
            _ => issue(report, path, code),
        }
    }

    pub(super) fn inspect(paths: &[PathBuf], excluded: &[ProcessIdentity]) -> Result<UsageReport> {
        let paths = normalized_paths(paths)?;
        let mut report = UsageReport::default();
        let mut existing = Vec::new();
        for path in paths {
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => existing.push(path),
                Ok(_) => report.issues.push(UsageIssue {
                    scope: path,
                    win32_error: None,
                    message: "Managed resource is not a regular file".into(),
                }),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => report.issues.push(UsageIssue {
                    scope: path,
                    win32_error: error.raw_os_error().map(|code| code as u32),
                    message: error.to_string(),
                }),
            }
        }
        report.checked_files = existing.len();
        if existing.is_empty() {
            return Ok(report);
        }
        let wide: Vec<Vec<u16>> = existing
            .iter()
            .map(|path| path.encode_utf16().chain([0]).collect())
            .collect();
        let mut session = 0;
        let mut key = [0u16; 33];
        let status = unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) };
        if status != 0 {
            issue(&mut report, "RmStartSession", status);
        } else {
            let guard = Session(Some(session));
            for group in wide.chunks(REGISTER_BATCH) {
                let pointers: Vec<_> = group.iter().map(|name| name.as_ptr()).collect();
                let status = unsafe {
                    RmRegisterResources(
                        session,
                        pointers.len() as u32,
                        pointers.as_ptr(),
                        0,
                        ptr::null(),
                        0,
                        ptr::null(),
                    )
                };
                if status != 0 {
                    issue(&mut report, "RmRegisterResources", status);
                    break;
                }
            }
            let mut seen = BTreeSet::new();
            for process in query(session, &mut report) {
                let pid = process.process.pid;
                let creation_time_100ns = process.process.creation.value();
                if !seen.insert((pid, creation_time_100ns))
                    || excluded.iter().any(|item| {
                        item.pid == pid && item.creation_time_100ns == creation_time_100ns
                    })
                {
                    continue;
                }
                // Restart Manager already supplies a creation-time-bound identity.
                // Enrichment may fail for a protected process; retain the RM entry.
                let executable = match crate::session::running_process(pid) {
                    Ok(Some(actual)) if actual.creation_time_100ns == creation_time_100ns => {
                        Some(actual.executable)
                    }
                    Ok(_) => continue, // The observed instance has exited or its PID was reused.
                    Err(_) => None,
                };
                let mut name = decode_name(&process.app_name);
                if name.is_empty() {
                    name = executable
                        .as_deref()
                        .and_then(|path| Path::new(path).file_name())
                        .and_then(|name| name.to_str())
                        .unwrap_or("Unknown application")
                        .into();
                }
                report.processes.push(UsageProcess {
                    pid,
                    creation_time_100ns,
                    name,
                    executable,
                    files: Vec::new(),
                });
            }
            guard.close(&mut report);
        }
        for (path, wide) in existing.iter().zip(&wide) {
            probe(path, wide, &mut report);
        }
        report
            .processes
            .sort_by_key(|process| (process.pid, process.creation_time_100ns));
        Ok(report)
    }

    #[test]
    fn restart_manager_structures_match_the_windows_abi() {
        assert_eq!(std::mem::size_of::<FileTime>(), 8);
        assert_eq!(std::mem::size_of::<UniqueProcess>(), 12);
        assert_eq!(std::mem::size_of::<ProcessInfo>(), 668);
    }

    #[test]
    fn deduplication_preserves_distinct_unicode_paths() {
        let paths = normalized_paths(&[
            PathBuf::from("C:\\test\\Straße.bin"),
            PathBuf::from("C:\\test\\STRASSE.bin"),
            PathBuf::from("C:\\test\\straße.BIN"),
        ])
        .unwrap();
        assert_eq!(paths.len(), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_inspection_never_means_clear() {
        let mut report = UsageReport::default();
        assert!(report.is_clear());
        report.issues.push(UsageIssue {
            scope: "RmGetList".into(),
            win32_error: Some(5),
            message: "Access denied".into(),
        });
        assert!(!report.is_clear());
        assert_eq!(
            serde_json::from_str::<UsageReport>(&serde_json::to_string(&report).unwrap()).unwrap(),
            report
        );
        report.issues.clear();
        report.reboot_reasons = 1;
        assert!(!report.is_clear());
    }

    #[cfg(windows)]
    #[test]
    fn empty_missing_and_invalid_paths() {
        assert!(inspect(&[], &[]).unwrap().is_clear());
        assert!(inspect(&[PathBuf::new()], &[]).is_err());
        assert!(inspect(&[PathBuf::from("relative")], &[]).is_err());
        assert!(inspect(&[PathBuf::from("C:\\bad\0name")], &[]).is_err());
        let missing = std::env::temp_dir().join(format!(
            "avelin-usage-missing-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(inspect(&[missing], &[]).unwrap().is_clear());
    }

    #[cfg(windows)]
    #[test]
    fn running_image_exclusion_requires_creation_time() {
        let identity = crate::session::running_process(std::process::id())
            .unwrap()
            .unwrap();
        let path = PathBuf::from(&identity.executable);
        let blocked = inspect(std::slice::from_ref(&path), &[]).unwrap();
        assert!(
            blocked
                .processes
                .iter()
                .any(|process| process.pid == identity.pid
                    && process.creation_time_100ns == identity.creation_time_100ns),
            "{blocked:?}"
        );
        let excluded =
            inspect(std::slice::from_ref(&path), std::slice::from_ref(&identity)).unwrap();
        assert!(
            !excluded
                .processes
                .iter()
                .any(|process| process.pid == identity.pid),
            "{excluded:?}"
        );
        let mut reused = identity.clone();
        reused.creation_time_100ns += 1;
        let blocked = inspect(&[path], &[reused]).unwrap();
        assert!(
            blocked
                .processes
                .iter()
                .any(|process| process.pid == identity.pid),
            "{blocked:?}"
        );
    }

    // A fixture child of this test executable, never an arbitrary user process.
    // Its handle closes on a release marker, or automatically after 45 seconds.
    #[cfg(windows)]
    #[test]
    #[ignore = "invoked only by the resource-usage fixture parent"]
    fn hold_file_fixture_child() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = PathBuf::from(
            std::env::var_os("AVELIN_USAGE_TEST_CHILD").expect("fixture child directory"),
        );
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(directory.join("文件.bin"))
            .unwrap();
        std::fs::write(directory.join("ready"), b"ready").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        while !directory.join("release").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        drop(handle);
    }

    #[cfg(windows)]
    #[test]
    fn real_child_lock_is_reported_then_clears_after_release() {
        use std::{
            fs,
            process::{Child, Command, Stdio},
            time::{Duration, Instant, SystemTime, UNIX_EPOCH},
        };
        struct Fixture {
            directory: PathBuf,
            child: Option<Child>,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::write(self.directory.join("release"), b"release");
                if let Some(mut child) = self.child.take() {
                    let _ = child.wait();
                }
                for name in ["文件.bin", "ready", "release"] {
                    let _ = fs::remove_file(self.directory.join(name));
                }
                for index in 0..256 {
                    let _ = fs::remove_file(self.directory.join(format!("unlocked-{index}.bin")));
                }
                let _ = fs::remove_dir(&self.directory);
            }
        }
        let mut fixture = Fixture {
            directory: std::env::temp_dir().join(format!(
                "avelin-usage-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
            child: None,
        };
        fs::create_dir(&fixture.directory).unwrap();
        let path = fixture.directory.join("文件.bin");
        fs::write(&path, b"contents remain unchanged").unwrap();
        // The held file is in the second registration batch. A partial first-batch
        // implementation must not silently report the complete resource set clear.
        let mut paths = Vec::new();
        for index in 0..256 {
            let unlocked = fixture.directory.join(format!("unlocked-{index}.bin"));
            fs::write(&unlocked, b"unlocked").unwrap();
            paths.push(unlocked);
        }
        paths.extend([path.clone(), path.clone()]);
        fixture.child = Some(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "resource_usage::tests::hold_file_fixture_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("AVELIN_USAGE_TEST_CHILD", &fixture.directory)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fixture.directory.join("ready").exists() {
            assert!(
                Instant::now() < deadline,
                "fixture did not obtain its handle"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let pid = fixture.child.as_ref().unwrap().id();
        let identity = crate::session::running_process(pid).unwrap().unwrap();
        let report = inspect(&paths, &[]).unwrap();
        assert_eq!(report.checked_files, 257);
        assert!(report.issues.is_empty(), "{report:?}");
        assert!(
            report.processes.iter().any(|process| process.pid == pid
                && process.creation_time_100ns == identity.creation_time_100ns),
            "{report:?}"
        );
        assert!(
            report
                .processes
                .iter()
                .all(|process| process.files.is_empty()),
            "RM does not provide per-file attribution"
        );
        assert_eq!(report.locked_files.len(), 1, "{report:?}");
        assert_eq!(report.locked_files[0].path, path.to_string_lossy());
        assert!(!report.is_clear());
        fs::write(fixture.directory.join("release"), b"release").unwrap();
        assert!(fixture.child.take().unwrap().wait().unwrap().success());
        let clear = inspect(&paths, &[]).unwrap();
        assert!(clear.is_clear(), "{clear:?}");
        assert_eq!(fs::read(&path).unwrap(), b"contents remain unchanged");
    }
}
