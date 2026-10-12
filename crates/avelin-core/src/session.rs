//! Local diagnostic evidence, not a recovery instruction or a public schema.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub creation_time_100ns: u64,
    pub executable: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecord {
    pub schema_version: u32,
    pub task_id: String,
    pub app_id: String,
    pub operation: String,
    pub install_dir: PathBuf,
    pub manifest_sha256: String,
    pub coordinator: ProcessIdentity,
    pub backend: ProcessIdentity,
    pub product_lock: PathBuf,
    pub report_path: PathBuf,
}

/// None means this PID is no longer running. Access errors remain errors.
#[cfg(windows)]
pub fn running_process(pid: u32) -> Result<Option<ProcessIdentity>> {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn QueryFullProcessImageNameW(
            handle: *mut c_void,
            flags: u32,
            name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn GetProcessTimes(
            handle: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }
    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    unsafe {
        let raw = OpenProcess(0x00101000, 0, pid); // SYNCHRONIZE | QUERY_LIMITED_INFORMATION
        if raw.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(87) {
                return Ok(None);
            }
            return Err(error).context("inspect process identity");
        }
        let handle = Handle(raw);
        match WaitForSingleObject(handle.0, 0) {
            0 => return Ok(None),
            258 => {}
            _ => return Err(std::io::Error::last_os_error()).context("query process lifetime"),
        }
        let (mut creation, mut exit, mut kernel, mut user) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        ensure!(
            GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) != 0,
            "GetProcessTimes: {}",
            std::io::Error::last_os_error()
        );
        let mut name = vec![0u16; 32768];
        let mut length = name.len() as u32;
        ensure!(
            QueryFullProcessImageNameW(handle.0, 0, name.as_mut_ptr(), &mut length) != 0,
            "QueryFullProcessImageNameW: {}",
            std::io::Error::last_os_error()
        );
        Ok(Some(ProcessIdentity {
            pid,
            creation_time_100ns: ((creation.high as u64) << 32) | creation.low as u64,
            executable: String::from_utf16(&name[..length as usize])?,
        }))
    }
}

#[cfg(not(windows))]
pub fn running_process(_pid: u32) -> Result<Option<ProcessIdentity>> {
    anyhow::bail!("process diagnosis requires Windows");
}

pub fn process_status(expected: &ProcessIdentity) -> serde_json::Value {
    match running_process(expected.pid) {
        Ok(Some(actual))
            if actual.creation_time_100ns == expected.creation_time_100ns
                && crate::same_windows_name(&actual.executable, &expected.executable) =>
        {
            serde_json::json!({"state":"Running","pid":expected.pid})
        }
        Ok(Some(_)) => serde_json::json!({"state":"DifferentProcess","pid":expected.pid}),
        Ok(None) => serde_json::json!({"state":"NotRunning","pid":expected.pid}),
        Err(error) => {
            serde_json::json!({"state":"Unconfirmed","pid":expected.pid,"error":format!("{error:#}")})
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn identity_detects_pid_reuse_without_trusting_the_number_alone() {
        let mut actual = running_process(std::process::id()).unwrap().unwrap();
        assert_eq!(process_status(&actual)["state"], "Running");
        actual.creation_time_100ns += 1;
        assert_eq!(process_status(&actual)["state"], "DifferentProcess");
    }
}
