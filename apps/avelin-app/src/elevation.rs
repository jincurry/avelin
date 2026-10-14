//! Explicit Windows UAC handoff. Silent operations never call this module.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, c_void},
    os::windows::ffi::OsStrExt,
    ptr,
};

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

// Windows CommandLineToArgvW / CRT quoting: every argument is one literal value.
fn quoted(text: &OsStr) -> Result<Vec<u16>> {
    let mut result = vec![34];
    let mut slashes = 0;
    for unit in text.encode_wide() {
        ensure!(unit != 0, "NUL in elevation argument");
        if unit == 92 {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            92,
            if unit == 34 { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        result.push(unit);
    }
    result.extend(std::iter::repeat_n(92, slashes * 2));
    result.push(34);
    Ok(result)
}

#[repr(C)]
struct ShellExecuteInfo {
    size: u32,
    mask: u32,
    window: *mut c_void,
    verb: *const u16,
    file: *const u16,
    parameters: *const u16,
    directory: *const u16,
    show: i32,
    instance: *mut c_void,
    id_list: *mut c_void,
    class: *const u16,
    class_key: *mut c_void,
    hot_key: u32,
    icon: *mut c_void,
    process: *mut c_void,
}
#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteExW(info: *mut ShellExecuteInfo) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
    fn GetExitCodeProcess(handle: *mut c_void, code: *mut u32) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

pub fn relaunch() -> Result<i32> {
    let exe = crate::execution_files::PinnedFile::open(&std::env::current_exe()?)?;
    let name = wide(exe.path().as_os_str());
    let verb = wide(OsStr::new("runas"));
    let directory = wide(std::env::current_dir()?.as_os_str());
    let mut parameters = Vec::new();
    for arg in std::env::args_os().skip(1) {
        if !parameters.is_empty() {
            parameters.push(32);
        }
        parameters.extend(quoted(&arg)?);
    }
    parameters.push(0);
    let mut info = ShellExecuteInfo {
        size: size_of::<ShellExecuteInfo>() as u32,
        mask: 0x40 | 0x100 | 0x400,
        window: ptr::null_mut(),
        verb: verb.as_ptr(),
        file: name.as_ptr(),
        parameters: parameters.as_ptr(),
        directory: directory.as_ptr(),
        show: 1,
        instance: ptr::null_mut(),
        id_list: ptr::null_mut(),
        class: ptr::null(),
        class_key: ptr::null_mut(),
        hot_key: 0,
        icon: ptr::null_mut(),
        process: ptr::null_mut(),
    };
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(1223) {
            return Ok(2);
        }
        return Err(error)
            .context("AVELIN_ELEVATION_REQUIRED: Windows could not open administrator consent");
    }
    ensure!(
        !info.process.is_null(),
        "elevation returned no child process"
    );
    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let handle = Handle(info.process);
    ensure!(
        unsafe { WaitForSingleObject(handle.0, u32::MAX) } == 0,
        "elevated process observation failed"
    );
    let mut exit = 0;
    ensure!(
        unsafe { GetExitCodeProcess(handle.0, &mut exit) } != 0,
        "elevated process exit code unavailable"
    );
    Ok(exit as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_preserve_spaces_quotes_and_trailing_slashes() {
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn CommandLineToArgvW(line: *const u16, count: *mut i32) -> *mut *mut u16;
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn LocalFree(memory: *mut c_void) -> *mut c_void;
        }
        let inputs = [
            "Setup.exe",
            "",
            "a b",
            "C:\\a b\\",
            "a\"b",
            "中文路径",
            "--name",
            "& exit",
            "\\\\\" ",
        ];
        let mut line = Vec::new();
        for input in &inputs {
            if !line.is_empty() {
                line.push(32);
            }
            line.extend(quoted(OsStr::new(input)).unwrap());
        }
        line.push(0);
        let mut count = 0;
        let parsed = unsafe { CommandLineToArgvW(line.as_ptr(), &mut count) };
        assert!(!parsed.is_null());
        assert_eq!(count as usize, inputs.len());
        for (index, expected) in inputs.iter().enumerate() {
            let text = unsafe { *parsed.add(index) };
            let mut length = 0;
            while unsafe { *text.add(length) } != 0 {
                length += 1;
            }
            assert_eq!(
                String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) }).unwrap(),
                *expected
            );
        }
        unsafe {
            LocalFree(parsed.cast());
        }
        assert!(quoted(OsStr::new("bad\0value")).is_err());
    }
}
