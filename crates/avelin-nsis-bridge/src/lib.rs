//! Unicode NSIS plugin. Exported entry points use the NSIS cdecl ABI.
//! Only the calling NSIS thread accesses the plugin stack; one bounded I/O
//! thread owns the pipe and observes cancellation even during a File command.
#![cfg(windows)]
#![allow(dead_code)]

mod owned_files;
#[path = "../../../apps/avelin-app/src/transport.rs"]
mod transport;

use serde_json::{Value, json};
use std::{
    ffi::c_void,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[repr(C)]
pub struct StackNode {
    next: *mut StackNode,
    text: [u16; 1],
}
#[repr(C)]
pub struct ExtraParameters {
    exec_flags: *mut c_void,
    execute_code: Option<unsafe extern "system" fn(i32, *mut c_void) -> i32>,
    validate_filename: Option<unsafe extern "system" fn(*mut u16)>,
    register_callback:
        Option<unsafe extern "system" fn(*mut c_void, unsafe extern "C" fn(u32) -> usize) -> i32>,
}
#[link(name = "kernel32")]
extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
    fn GlobalFree(memory: *mut c_void) -> *mut c_void;
    fn GetModuleHandleExW(flags: u32, address: *const u16, module: *mut *mut c_void) -> i32;
}

struct Shared {
    state: AtomicU8, // 0 continue, 1 cancel, 2 disconnected
    stop: AtomicBool,
    error: Mutex<String>,
}
impl Shared {
    fn fail(&self, message: String) {
        if let Ok(mut error) = self.error.lock() {
            *error = message;
        }
        self.state.store(2, Ordering::Release);
    }
}
struct ThreadExit(Arc<Shared>);
impl Drop for ThreadExit {
    fn drop(&mut self) {
        if !self.0.stop.load(Ordering::Acquire) && self.0.state.load(Ordering::Acquire) != 2 {
            self.0.fail("error:io_thread_stopped".into());
        }
    }
}
enum Command {
    Send(Value, mpsc::Sender<Result<(), String>>),
}
struct Worker {
    commands: mpsc::SyncSender<Command>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    fn new(mut connection: transport::Connection) -> io::Result<Self> {
        let (commands, receiver) = mpsc::sync_channel(16);
        let shared = Arc::new(Shared {
            state: AtomicU8::new(0),
            stop: AtomicBool::new(false),
            error: Mutex::new(String::new()),
        });
        let state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("avelin-nsis-pipe".into())
            .spawn(move || {
                let _exit = ThreadExit(Arc::clone(&state));
                while !state.stop.load(Ordering::Acquire) {
                    // Sleep on the command queue so Emit wakes us immediately.
                    // The bounded idle wait still polls cancellation/heartbeats
                    // while NSIS is busy extracting a file or deleting payloads.
                    match receiver.recv_timeout(Duration::from_millis(5)) {
                        Ok(Command::Send(message, reply)) => {
                            let result = connection
                                .send(message)
                                .map_err(|e| format!("error:pipe_write:{e}"));
                            if let Err(error) = &result {
                                state.fail(error.clone());
                            }
                            let failed = result.is_err();
                            let _ = reply.send(result);
                            if failed {
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    // Poll after every command, without imposing a fresh pipe
                    // wait on each synchronous progress notification.
                    match connection.recv_timeout(Duration::ZERO) {
                        Ok(message) if message["kind"] == "cancel" => {
                            state.state.store(1, Ordering::Release);
                            if let Err(error) = connection.send(json!({"kind":"cancel_received"})) {
                                state.fail(format!("error:cancel_ack:{error}"));
                                break;
                            }
                        }
                        Ok(_) => {
                            state.fail("error:protocol:unexpected server command".into());
                            break;
                        }
                        Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
                        Err(error) => {
                            state.fail(format!("error:pipe_read:{error}"));
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            commands,
            shared,
            thread: Some(handle),
        })
    }

    fn send(&self, message: Value) -> Result<(), String> {
        if self.shared.state.load(Ordering::Acquire) == 2 {
            return Err(self.last_error());
        }
        let (reply, result) = mpsc::channel();
        self.commands
            .try_send(Command::Send(message, reply))
            .map_err(|e| format!("error:command_queue:{e}"))?;
        result
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| "error:command_timeout".to_owned())?
    }

    fn last_error(&self) -> String {
        self.shared
            .error
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "error:state_poisoned".into())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        // Native calls are PIPE_NOWAIT, with a two-second write deadline.
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

static WORKER: OnceLock<Mutex<Option<Worker>>> = OnceLock::new();
static LAST_ERROR: OnceLock<Mutex<String>> = OnceLock::new();
fn worker() -> &'static Mutex<Option<Worker>> {
    WORKER.get_or_init(|| Mutex::new(None))
}
fn remember(error: &str) {
    if let Ok(mut value) = LAST_ERROR.get_or_init(|| Mutex::new(String::new())).lock() {
        *value = error.into();
    }
}

unsafe extern "C" fn plugin_callback(message: u32) -> usize {
    if message == 0 {
        // NSPIM_UNLOAD: join before NSIS releases the DLL.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(mut current) = worker().lock() {
                current.take();
            }
        }));
    }
    0
}

unsafe fn register_lifetime(extra: *mut ExtraParameters) -> Result<(), String> {
    if extra.is_null() {
        return Err("error:abi:missing_extra_parameters".into());
    }
    let callback = (*extra)
        .register_callback
        .ok_or("error:abi:missing_register_callback")?;
    let mut module = ptr::null_mut();
    if GetModuleHandleExW(0x6, Connect as *const () as *const u16, &mut module) == 0 {
        return Err(format!(
            "error:abi:module_handle:{}",
            io::Error::last_os_error()
        ));
    }
    if callback(module, plugin_callback) < 0 {
        return Err("error:abi:callback_registration".into());
    }
    Ok(())
}

unsafe fn pop(stack: *mut *mut StackNode, limit: usize) -> Result<String, String> {
    if stack.is_null() || (*stack).is_null() {
        return Err("error:abi:missing_argument".into());
    }
    let node = *stack;
    let text = (*node).text.as_ptr();
    let mut length = 0;
    while length < limit && *text.add(length) != 0 {
        length += 1;
    }
    let result = if length == limit {
        Err("error:abi:unterminated_argument".into())
    } else {
        String::from_utf16(std::slice::from_raw_parts(text, length))
            .map_err(|_| "error:argument:invalid_utf16".into())
    };
    *stack = (*node).next;
    GlobalFree(node.cast());
    result
}

unsafe fn push(stack: *mut *mut StackNode, limit: usize, value: &str) {
    if stack.is_null() || limit == 0 {
        return;
    }
    let node =
        GlobalAlloc(0x0040, std::mem::size_of::<StackNode>() + limit * 2).cast::<StackNode>();
    if node.is_null() {
        return;
    }
    (*node).next = *stack;
    for (index, character) in value.encode_utf16().take(limit - 1).enumerate() {
        *(*node).text.as_mut_ptr().add(index) = character;
    }
    *stack = node;
}

unsafe fn invoke(
    size: i32,
    stack: *mut *mut StackNode,
    arguments: usize,
    action: impl FnOnce(Vec<String>) -> Result<String, String>,
) {
    if !(2..=1_048_576).contains(&size) {
        return;
    }
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut values = Vec::with_capacity(arguments);
        let mut first_error = None;
        // Pop every supplied argument even when one is invalid.
        for _ in 0..arguments {
            match pop(stack, size as usize) {
                Ok(value) => values.push(value),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => action(values),
        }
    }))
    .unwrap_or_else(|_| Err("error:plugin_panic".into()));
    let value = match outcome {
        Ok(value) => value,
        Err(error) => {
            remember(&error);
            error
        }
    };
    push(stack, size as usize, &value);
}

/// CreateOwnedDirectory "absolute_path" ; Pop $0
#[no_mangle]
pub unsafe extern "C" fn CreateOwnedDirectory(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    _extra: *mut ExtraParameters,
) {
    invoke(size, stack, 1, |arguments| {
        owned_files::create_directory(&arguments[0])
            .map(|_| "created".to_owned())
            .map_err(|error| format!("error:create_directory:{error:#}"))
    });
}

#[no_mangle]
pub unsafe extern "C" fn OwnedFileHash(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    _extra: *mut ExtraParameters,
) {
    invoke(size, stack, 1, |arguments| {
        owned_files::hash_file(&arguments[0]).map_err(|error| format!("error:hash:{error:#}"))
    });
}

#[no_mangle]
pub unsafe extern "C" fn PrepareUpgradeReceipt(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    _extra: *mut ExtraParameters,
) {
    invoke(size, stack, 5, |a| {
        owned_files::prepare_upgrade_receipt(&a[0], &a[1], &a[2], &a[3], &a[4])
            .map_err(|error| format!("error:receipt:{error:#}"))
    });
}

#[no_mangle]
pub unsafe extern "C" fn AppendUpgradeJournal(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    _extra: *mut ExtraParameters,
) {
    invoke(size, stack, 6, |a| {
        owned_files::append_upgrade_journal(&a[0], &a[1], &a[2], &a[3], &a[4], &a[5])
            .map(|_| "OK".into())
            .map_err(|error| format!("error:journal:{error:#}"))
    });
}

/// PublishOwnedFile "source" "destination" "expected_sha256_or_empty" ; Pop $0
/// Returns the digest of newly created bytes, or an error-prefixed diagnostic.
#[no_mangle]
pub unsafe extern "C" fn PublishOwnedFile(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    _extra: *mut ExtraParameters,
) {
    invoke(size, stack, 3, |arguments| {
        owned_files::publish(&arguments[0], &arguments[1], &arguments[2])
            .map_err(|error| format!("error:publish:{error:#}"))
    });
}

/// DeleteOwnedFile "absolute_path" "expected_sha256" ; Pop $0
/// Returns deleted, missing, preserved, or an error-prefixed diagnostic.
#[no_mangle]
pub unsafe extern "C" fn DeleteOwnedFile(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    _extra: *mut ExtraParameters,
) {
    invoke(size, stack, 2, |arguments| {
        owned_files::delete(&arguments[0], &arguments[1])
            .map(str::to_owned)
            .map_err(|error| format!("error:delete:{error:#}"))
    });
}

/// NSIS syntax: Connect "pipe_name" "token" "task_id" ; Pop $0
#[no_mangle]
pub unsafe extern "C" fn Connect(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    extra: *mut ExtraParameters,
) {
    invoke(size, stack, 3, |arguments| {
        register_lifetime(extra)?;
        let mut current = worker().lock().map_err(|_| "error:state_poisoned")?;
        if current.is_some() {
            return Err("error:already_connected".into());
        }
        let connection = transport::Connection::connect(
            &arguments[0],
            &arguments[1],
            &arguments[2],
            Duration::from_secs(5),
        )
        .map_err(|e| format!("error:connect:{e}"))?;
        *current = Some(Worker::new(connection).map_err(|e| format!("error:thread:{e}"))?);
        Ok("OK".into())
    });
}

/// NSIS syntax: Emit "phase" "completed" "total" ; Pop $0
#[no_mangle]
pub unsafe extern "C" fn Emit(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    _extra: *mut ExtraParameters,
) {
    invoke(size, stack, 3, |arguments| {
        let phase = &arguments[0];
        if phase.is_empty()
            || phase.len() > 64
            || !phase
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("error:argument:phase".into());
        }
        let completed: u64 = arguments[1]
            .parse()
            .map_err(|_| "error:argument:completed")?;
        let total: u64 = arguments[2].parse().map_err(|_| "error:argument:total")?;
        if completed > total {
            return Err("error:argument:progress_range".into());
        }
        let current = worker().lock().map_err(|_| "error:state_poisoned")?;
        current.as_ref().ok_or("error:not_connected")?.send(
            json!({"kind":"progress", "phase":phase, "completed":completed, "total":total}),
        )?;
        Ok("OK".into())
    });
}

/// NSIS syntax: CheckPoint ; Pop $0 (continue / cancel / disconnected).
#[no_mangle]
pub unsafe extern "C" fn CheckPoint(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    _extra: *mut ExtraParameters,
) {
    invoke(size, stack, 0, |_| {
        let current = worker().lock().map_err(|_| "error:state_poisoned")?;
        let Some(current) = current.as_ref() else {
            return Ok("disconnected".into());
        };
        Ok(match current.shared.state.load(Ordering::Acquire) {
            0 => "continue",
            1 => "cancel",
            _ => "disconnected",
        }
        .into())
    });
}

/// M0 test instrumentation only: bounded barrier for deterministic cancellation.
/// Production scripts must not call this after ordinary progress notifications.
#[no_mangle]
pub unsafe extern "C" fn WaitForCancel(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    _extra: *mut ExtraParameters,
) {
    invoke(size, stack, 1, |arguments| {
        let milliseconds: u64 = arguments[0].parse().map_err(|_| "error:argument:timeout")?;
        if milliseconds > 5000 {
            return Err("error:argument:timeout_limit".into());
        }
        let current = worker().lock().map_err(|_| "error:state_poisoned")?;
        let Some(current) = current.as_ref() else {
            return Ok("disconnected".into());
        };
        let deadline = std::time::Instant::now() + Duration::from_millis(milliseconds);
        loop {
            match current.shared.state.load(Ordering::Acquire) {
                1 => return Ok("cancel".into()),
                2 => return Ok("disconnected".into()),
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return Ok("timeout".into());
            }
            thread::sleep(Duration::from_millis(2));
        }
    });
}

/// NSIS syntax: Finish "Succeeded|Cancelled|Failed" "code" ; Pop $0.
/// Always stops and joins the I/O thread, including a failed final write.
#[no_mangle]
pub unsafe extern "C" fn Finish(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    _extra: *mut ExtraParameters,
) {
    invoke(size, stack, 2, |arguments| {
        if !matches!(
            arguments[0].as_str(),
            "Succeeded" | "Cancelled" | "Failed" | "Unknown"
        ) {
            return Err("error:argument:result".into());
        }
        let code: i32 = arguments[1].parse().map_err(|_| "error:argument:code")?;
        let current = worker()
            .lock()
            .map_err(|_| "error:state_poisoned")?
            .take()
            .ok_or("error:not_connected")?;
        let result = current.send(json!({"kind":"finish", "result":arguments[0], "code":code}));
        drop(current);
        result?;
        Ok("OK".into())
    });
}

/// Optional bounded diagnostic, for example after a disconnected checkpoint.
#[no_mangle]
pub unsafe extern "C" fn LastError(
    _window: *mut c_void,
    size: i32,
    _variables: *mut u16,
    stack: *mut *mut StackNode,
    _extra: *mut ExtraParameters,
) {
    invoke(size, stack, 0, |_| {
        if let Some(current) = worker()
            .lock()
            .map_err(|_| "error:state_poisoned")?
            .as_ref()
        {
            let detail = current.last_error();
            if !detail.is_empty() {
                return Ok(detail);
            }
        }
        Ok(LAST_ERROR
            .get_or_init(|| Mutex::new(String::new()))
            .lock()
            .map_err(|_| "error:state_poisoned")?
            .clone())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress_stream(count: u64, request_cancel: bool) -> u128 {
        let name = format!(
            r"\\.\pipe\avelin-m0-worker-stream-{}-{}",
            std::process::id(),
            count
        );
        let token = "0123456789abcdef0123456789abcdef";
        let server = transport::PipeServer::bind(&name, token, "stream-task").unwrap();
        let server_thread = thread::spawn(move || {
            let mut connection = server.accept(Duration::from_secs(3)).unwrap();
            let mut next = 0;
            let mut acknowledged = false;
            loop {
                let event = connection.recv_timeout(Duration::from_secs(3)).unwrap();
                match event["kind"].as_str() {
                    Some("progress") => {
                        assert_eq!(event["completed"], next);
                        next += 1;
                        if request_cancel && next == count / 2 {
                            connection.send_cancel().unwrap();
                        }
                    }
                    Some("cancel_received") => {
                        assert!(request_cancel && !acknowledged);
                        acknowledged = true;
                    }
                    Some("finish") => {
                        assert_eq!(next, count);
                        assert_eq!(acknowledged, request_cancel);
                        assert_eq!(event["result"], "Succeeded");
                        break;
                    }
                    _ => panic!("unexpected stream event: {event}"),
                }
            }
        });
        let connection =
            transport::Connection::connect(&name, token, "stream-task", Duration::from_secs(3))
                .unwrap();
        let current = Worker::new(connection).unwrap();
        let started = std::time::Instant::now();
        for completed in 0..count {
            current
                .send(json!({"kind":"progress", "phase":"running", "completed":completed,"total":count}))
                .unwrap();
        }
        let elapsed = started.elapsed().as_millis();
        if request_cancel {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while current.shared.state.load(Ordering::Acquire) == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "cancel was not observed"
                );
                thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(current.shared.state.load(Ordering::Acquire), 1);
        }
        current
            .send(json!({"kind":"finish","result":"Succeeded","code":0}))
            .unwrap();
        drop(current);
        server_thread.join().unwrap();
        elapsed
    }

    #[test]
    fn queued_progress_preserves_order_and_observes_cancellation() {
        progress_stream(128, true);
    }

    #[test]
    #[ignore = "manual mechanism timing, not a machine-independent performance gate"]
    fn measure_synchronous_progress_stream() {
        println!(
            "500 synchronous progress events: {} ms",
            progress_stream(500, false)
        );
    }

    unsafe extern "system" fn register_callback(
        _module: *mut c_void,
        _callback: unsafe extern "C" fn(u32) -> usize,
    ) -> i32 {
        0
    }
    type Entry =
        unsafe extern "C" fn(*mut c_void, i32, *mut u16, *mut *mut StackNode, *mut ExtraParameters);
    unsafe fn call(entry: Entry, arguments: &[&str]) -> String {
        let mut stack = ptr::null_mut();
        for argument in arguments.iter().rev() {
            push(&mut stack, 1024, argument);
        }
        let mut extra = ExtraParameters {
            exec_flags: ptr::null_mut(),
            execute_code: None,
            validate_filename: None,
            register_callback: Some(register_callback),
        };
        entry(
            ptr::null_mut(),
            1024,
            ptr::null_mut(),
            &mut stack,
            &mut extra,
        );
        let result = pop(&mut stack, 1024).unwrap();
        assert!(stack.is_null(), "plugin must consume exactly its arguments");
        result
    }

    #[test]
    fn unicode_abi_cancel_barrier_and_finish_joins_worker() {
        let name = format!(r"\\.\pipe\avelin-m0-api-test-{}", std::process::id());
        let token = "0123456789abcdef0123456789abcdef";
        let server = transport::PipeServer::bind(&name, token, "任务-1").unwrap();
        let server_thread = thread::spawn(move || {
            let mut connection = server.accept(Duration::from_secs(3)).unwrap();
            assert_eq!(
                connection.recv_timeout(Duration::from_secs(3)).unwrap()["phase"],
                "running"
            );
            connection.send_cancel().unwrap();
            assert_eq!(
                connection.recv_timeout(Duration::from_secs(3)).unwrap()["kind"],
                "cancel_received"
            );
            assert_eq!(
                connection.recv_timeout(Duration::from_secs(3)).unwrap()["result"],
                "Cancelled"
            );
        });
        unsafe {
            assert_eq!(call(Connect, &[&name, token, "任务-1"]), "OK");
            assert_eq!(call(Emit, &["running", "1", "3"]), "OK");
            assert_eq!(call(WaitForCancel, &["3000"]), "cancel");
            assert_eq!(call(CheckPoint, &[]), "cancel");
            assert_eq!(call(Finish, &["Cancelled", "2"]), "OK");
            assert_eq!(call(CheckPoint, &[]), "disconnected");
        }
        assert!(worker().lock().unwrap().is_none());
        server_thread.join().unwrap();
    }

    #[test]
    #[ignore = "requires a built i686 DLL and locally installed Unicode NSIS"]
    fn real_nsis_process_loads_dll_and_reports_finish() {
        use std::{
            fs, os::windows::process::CommandExt, path::PathBuf, process::Command, time::Instant,
        };
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let output = crate_dir.join(".smoke");
        fs::create_dir_all(&output).unwrap();
        let script_path = output.join("bridge-smoke.nsi");
        let executable = output.join("bridge-smoke.exe");
        let dll_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        assert!(
            dll_dir.join("avelin_nsis_bridge.dll").is_file(),
            "build the DLL before running this ignored test"
        );
        for path in [&output, &dll_dir] {
            assert!(
                !path.to_string_lossy().contains(['$', '"', '\n', '\r']),
                "test fixture path needs a literal NSIS-safe directory"
            );
        }
        let pipe = format!(r"\\.\pipe\avelin-m0-real-nsis-{}", std::process::id());
        let token = "0123456789abcdef0123456789abcdef";
        let listener = transport::PipeServer::bind(&pipe, token, "real-nsis-task").unwrap();
        let source = format!(
            r#"Unicode true
Name "Avelin bridge smoke"
OutFile "{}"
RequestExecutionLevel user
SilentInstall silent
!addplugindir /x86-unicode "{}"
Section
  avelin_nsis_bridge::Connect "{}" "{}" "real-nsis-task"
  Pop $0
  StrCmp $0 "OK" 0 failure
  avelin_nsis_bridge::Emit "running" "1" "3"
  Pop $0
  StrCmp $0 "OK" 0 failure
  avelin_nsis_bridge::WaitForCancel "3000"
  Pop $0
  StrCmp $0 "cancel" 0 failure
  avelin_nsis_bridge::Finish "Cancelled" "2"
  Pop $0
  StrCmp $0 "OK" 0 failure
  SetErrorLevel 0
  Goto done
failure:
  SetErrorLevel 91
done:
SectionEnd
"#,
            executable.display(),
            dll_dir.display(),
            pipe,
            token
        );
        fs::write(&script_path, source).unwrap();
        let compiler =
            PathBuf::from(std::env::var_os("ProgramFiles(x86)").unwrap()).join("NSIS/makensis.exe");
        let build = Command::new(compiler)
            .arg("/V2")
            .arg(&script_path)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "NSIS compile failed: {} {}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
        let mut child = Command::new(&executable)
            .arg("/S")
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();
        let mut connection = match listener.accept(Duration::from_secs(15)) {
            Ok(connection) => connection,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("NSIS handshake failed: {error}");
            }
        };
        let progress = connection.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(progress["completed"], 1);
        connection.send_cancel().unwrap();
        assert_eq!(
            connection.recv_timeout(Duration::from_secs(3)).unwrap()["kind"],
            "cancel_received"
        );
        let terminal = connection.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(terminal["result"], "Cancelled");
        assert_eq!(terminal["code"], 2);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "NSIS exit: {status}");
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("NSIS did not exit after Finish");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
