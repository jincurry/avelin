//! A graphical process can outlive the terminal or bootstrap that launched it.
//! Rust's print macros panic on a broken pipe, including those inside UI
//! dependencies. Replace only pipe/missing output handles before GUI threads
//! start; keep real consoles and explicit file redirection intact. Structured
//! GUI diagnostics still go to runtime_log's bounded files.
use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io,
    os::windows::io::AsRawHandle,
};

type Handle = *mut c_void;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const STD_ERROR_HANDLE: u32 = -12i32 as u32;
const FILE_TYPE_DISK: u32 = 1;
const FILE_TYPE_CHAR: u32 = 2;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(kind: u32) -> Handle;
    fn SetStdHandle(kind: u32, handle: Handle) -> i32;
    fn GetFileType(handle: Handle) -> u32;
}

/// Retain the replacement handles until the graphical process exits. This
/// function changes process-wide handles and must run before starting threads.
pub struct GuiStandardStreams {
    _replacements: Vec<File>,
}

pub fn initialize() -> io::Result<GuiStandardStreams> {
    let mut replacements = Vec::new();
    for kind in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let handle = unsafe { GetStdHandle(kind) };
        let file_type = unsafe { GetFileType(handle) };
        if matches!(file_type, FILE_TYPE_DISK | FILE_TYPE_CHAR) {
            continue;
        }
        // Use the Windows device path so neither CWD nor a caller's environment
        // selects the sink. A pipe that is still connected is also replaced: its
        // reader may disappear at any point after this initialization.
        let sink = OpenOptions::new().write(true).open(r"\\.\NUL")?;
        if unsafe { SetStdHandle(kind, sink.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        replacements.push(sink);
    }
    Ok(GuiStandardStreams {
        _replacements: replacements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        process::{Command, Stdio},
    };

    const MARKER: &str = "AVELIN_GUI_STDIO_TEST";
    const READY: &str = "avelin-stdio-child-ready";
    const MESSAGE: &str = "avelin-stdio-child-write";

    // Every process-wide handle change stays in a dedicated child, so libtest
    // and concurrent tests retain their original streams.
    #[test]
    fn subprocess_probe() {
        let Ok(mode) = std::env::var(MARKER) else {
            return;
        };
        println!("{READY}");
        std::io::stdout().flush().unwrap();
        let mut signal = [0];
        std::io::stdin().read_exact(&mut signal).unwrap();
        if mode == "reproduce" {
            let panic = std::panic::catch_unwind(|| eprintln!("{MESSAGE}"));
            std::process::exit(if panic.is_err() { 0 } else { 1 });
        }
        let _streams = initialize().unwrap();
        // These are intentionally panicking macros, matching UI dependencies.
        println!("{MESSAGE}");
        eprintln!("{MESSAGE}");
        std::io::stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
        std::process::exit(0);
    }

    fn child(mode: &str, stderr: Stdio) -> std::process::Child {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "gui_stdio::tests::subprocess_probe",
                "--nocapture",
            ])
            .env(MARKER, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap()
    }

    fn wait_until_ready(child: &mut std::process::Child) -> BufReader<std::process::ChildStdout> {
        let mut output = BufReader::new(child.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "child exited before probe"
            );
            if line.contains(READY) {
                return output;
            }
        }
    }

    #[test]
    fn closed_parent_pipes_reproduce_print_panic_and_gui_bootstrap_prevents_it() {
        for mode in ["reproduce", "graphical"] {
            let mut child = child(mode, Stdio::piped());
            let output = wait_until_ready(&mut child);
            drop(output);
            drop(child.stderr.take());
            child.stdin.take().unwrap().write_all(&[1]).unwrap();
            assert!(
                child.wait().unwrap().success(),
                "stdio probe failed: {mode}"
            );
        }
    }

    #[test]
    fn gui_bootstrap_preserves_explicit_file_redirection() {
        let path = std::env::temp_dir().join(format!(
            "avelin-gui-stdio-{}.log",
            uuid::Uuid::new_v4().simple()
        ));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let mut child = child("graphical", Stdio::from(file));
        let output = wait_until_ready(&mut child);
        drop(output);
        child.stdin.take().unwrap().write_all(&[1]).unwrap();
        assert!(child.wait().unwrap().success());
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(
            text.contains(MESSAGE),
            "GUI stderr file must remain observable"
        );
    }
}
