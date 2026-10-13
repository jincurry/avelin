//! Local-only, current-user named pipes shared with the Rust worker.
//! Polling uses PIPE_NOWAIT so no thread can be stranded in native I/O.
use serde_json::{Value, json};
use std::{
    ffi::c_void,
    io, ptr, thread,
    time::{Duration, Instant},
};

pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const HEARTBEAT: Duration = Duration::from_millis(500);
const PEER_TIMEOUT: Duration = Duration::from_secs(5);
type RawHandle = *mut c_void;
const INVALID_HANDLE: RawHandle = -1isize as RawHandle;

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit: i32,
}
#[repr(C)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        instances: u32,
        out_size: u32,
        in_size: u32,
        timeout: u32,
        security: *const SecurityAttributes,
    ) -> RawHandle;
    fn ConnectNamedPipe(pipe: RawHandle, overlapped: *mut c_void) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const c_void,
        creation: u32,
        flags: u32,
        template: RawHandle,
    ) -> RawHandle;
    fn SetNamedPipeHandleState(
        pipe: RawHandle,
        mode: *const u32,
        count: *const u32,
        timeout: *const u32,
    ) -> i32;
    fn ReadFile(
        file: RawHandle,
        buffer: *mut c_void,
        count: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WriteFile(
        file: RawHandle,
        buffer: *const c_void,
        count: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CloseHandle(handle: RawHandle) -> i32;
    fn GetCurrentProcess() -> RawHandle;
    fn GetNamedPipeClientProcessId(pipe: RawHandle, pid: *mut u32) -> i32;
    fn GetNamedPipeServerProcessId(pipe: RawHandle, pid: *mut u32) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: RawHandle, access: u32, token: *mut RawHandle) -> i32;
    fn GetTokenInformation(
        token: RawHandle,
        class: u32,
        information: *mut c_void,
        length: u32,
        required: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *const c_void, text: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}

struct Handle(RawHandle);
// Handles are owned; Connection is moved to exactly one I/O thread.
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn expired() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "pipe operation timed out")
}
fn idle_error(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(232 | 231 | 536))
}

fn validate_inputs(name: &str, token: &str, task: &str) -> io::Result<()> {
    let suffix = name
        .strip_prefix(r"\\.\pipe\avelin-m0-")
        .ok_or_else(|| invalid("invalid local pipe prefix"))?;
    if suffix.is_empty()
        || suffix.len() > 160
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid("invalid local pipe name"));
    }
    if !(32..=256).contains(&token.len())
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid(
            "token must contain 32..256 ASCII identifier characters",
        ));
    }
    if task.is_empty() || task.len() > 128 || task.chars().any(char::is_control) {
        return Err(invalid("invalid task id"));
    }
    Ok(())
}

pub fn current_user_sid() -> io::Result<String> {
    unsafe {
        let mut raw = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), 0x0008, &mut raw) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle(raw);
        let mut required = 0;
        GetTokenInformation(token.0, 1, ptr::null_mut(), 0, &mut required);
        if required == 0 {
            return Err(io::Error::last_os_error());
        }
        // usize storage guarantees alignment for TOKEN_USER and its SID pointer.
        let mut storage = vec![0usize; (required as usize).div_ceil(std::mem::size_of::<usize>())];
        if GetTokenInformation(
            token.0,
            1,
            storage.as_mut_ptr().cast(),
            required,
            &mut required,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sid = (*(storage.as_ptr().cast::<SidAndAttributes>())).sid;
        let mut sid_text = ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut sid_text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid_owner = LocalMemory(sid_text.cast());
        let mut length = 0;
        while *sid_text.add(length) != 0 {
            length += 1;
        }
        let sid = String::from_utf16(std::slice::from_raw_parts(sid_text, length))
            .map_err(|_| invalid("invalid token SID"))?;
        drop(sid_owner);
        Ok(sid)
    }
}

fn current_user_security() -> io::Result<LocalMemory> {
    let sid = current_user_sid()?;
    unsafe {
        // Protected DACL: only the process's current user receives access.
        let sddl = wide(&format!("D:P(A;;GA;;;{sid})"));
        let mut descriptor = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(LocalMemory(descriptor))
    }
}

pub struct PipeServer {
    handle: Handle,
    token: String,
    task: String,
}
impl PipeServer {
    pub fn bind(name: &str, token: &str, task_id: &str) -> io::Result<Self> {
        validate_inputs(name, token, task_id)?;
        let security = current_user_security()?;
        let attributes = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            descriptor: security.0,
            inherit: 0,
        };
        let name = wide(name);
        // DUPLEX | FIRST_PIPE_INSTANCE; BYTE | NOWAIT | REJECT_REMOTE_CLIENTS.
        let raw = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                0x00080003,
                0x00000009,
                1,
                65536,
                65536,
                0,
                &attributes,
            )
        };
        if raw == INVALID_HANDLE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle: Handle(raw),
            token: token.into(),
            task: task_id.into(),
        })
    }

    /// Test-only endpoint without process binding. Production must use accept_from.
    #[cfg(test)]
    pub fn accept(self, timeout: Duration) -> io::Result<Connection> {
        self.accept_checked(timeout, None)
    }

    /// Authorize the launched process before the acknowledgement enables deployment work.
    /// Consumes the endpoint; a rejected peer cannot reconnect to this task.
    pub fn accept_from(self, timeout: Duration, expected_pid: u32) -> io::Result<Connection> {
        self.accept_checked(timeout, Some(expected_pid))
    }

    fn accept_checked(
        self,
        timeout: Duration,
        expected_pid: Option<u32>,
    ) -> io::Result<Connection> {
        let deadline = Instant::now() + timeout;
        loop {
            let connected = unsafe { ConnectNamedPipe(self.handle.0, ptr::null_mut()) };
            let error = io::Error::last_os_error();
            if connected != 0 || error.raw_os_error() == Some(535) {
                break;
            }
            if !idle_error(&error) {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err(expired());
            }
            thread::sleep(Duration::from_millis(5));
        }
        let mut connection = Connection::new(self.handle, self.task, true);
        let hello =
            connection.receive(deadline.saturating_duration_since(Instant::now()), false)?;
        if hello["kind"] != "hello" || hello["token"].as_str() != Some(self.token.as_str()) {
            return Err(invalid("unauthorized hello"));
        }
        if let Some(expected_pid) = expected_pid {
            if connection.peer_pid()? != expected_pid {
                return Err(invalid(
                    "unexpected pipe peer process; acknowledgement withheld",
                ));
            }
        }
        connection.send(json!({"kind":"hello_ack", "token":self.token}))?;
        connection.authenticated = true;
        Ok(connection)
    }
}

pub struct Connection {
    handle: Handle,
    task: String,
    server: bool,
    tx_seq: u64,
    rx_seq: u64,
    input: Vec<u8>,
    last_peer: Instant,
    last_sent: Instant,
    authenticated: bool,
    control_write_error: Option<io::Error>,
}
impl Connection {
    fn new(handle: Handle, task: String, server: bool) -> Self {
        Self {
            handle,
            task,
            server,
            tx_seq: 0,
            rx_seq: 0,
            input: Vec::new(),
            last_peer: Instant::now(),
            last_sent: Instant::now(),
            authenticated: false,
            control_write_error: None,
        }
    }

    pub fn connect(name: &str, token: &str, task: &str, timeout: Duration) -> io::Result<Self> {
        validate_inputs(name, token, task)?;
        let name = wide(name);
        let deadline = Instant::now() + timeout;
        let handle = loop {
            // SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION: server cannot impersonate us.
            let raw = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    0xC0000000,
                    0,
                    ptr::null(),
                    3,
                    0x00110000,
                    ptr::null_mut(),
                )
            };
            if raw != INVALID_HANDLE {
                break Handle(raw);
            }
            let error = io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(2 | 231 | 536)) {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err(expired());
            }
            thread::sleep(Duration::from_millis(5));
        };
        let mode = 1u32; // PIPE_READMODE_BYTE | PIPE_NOWAIT.
        if unsafe { SetNamedPipeHandleState(handle.0, &mode, ptr::null(), ptr::null()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut connection = Self::new(handle, task.into(), false);
        connection.send(json!({"kind":"hello", "token":token}))?;
        let reply =
            connection.receive(deadline.saturating_duration_since(Instant::now()), false)?;
        if reply["kind"] != "hello_ack" || reply["token"].as_str() != Some(token) {
            return Err(invalid("unauthorized hello acknowledgement"));
        }
        connection.authenticated = true;
        Ok(connection)
    }

    pub fn peer_pid(&self) -> io::Result<u32> {
        let mut pid = 0;
        let result = unsafe {
            if self.server {
                GetNamedPipeClientProcessId(self.handle.0, &mut pid)
            } else {
                GetNamedPipeServerProcessId(self.handle.0, &mut pid)
            }
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(pid)
        }
    }

    pub fn send_cancel(&mut self) -> io::Result<()> {
        self.send(json!({"kind":"cancel"}))
    }

    pub fn send(&mut self, mut message: Value) -> io::Result<()> {
        let object = message
            .as_object_mut()
            .ok_or_else(|| invalid("message must be an object"))?;
        if !object.get("kind").is_some_and(Value::is_string) {
            return Err(invalid("missing message kind"));
        }
        let next = self
            .tx_seq
            .checked_add(1)
            .ok_or_else(|| invalid("sequence exhausted"))?;
        object.insert("protocol_version".into(), json!(1));
        object.insert("task_id".into(), json!(self.task));
        object.insert("seq".into(), json!(next));
        let mut bytes = serde_json::to_vec(&message)?;
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(invalid("message exceeds 16 KiB"));
        }
        bytes.push(b'\n');
        let deadline = Instant::now() + IO_TIMEOUT;
        let mut offset = 0;
        while offset < bytes.len() {
            let mut written = 0;
            let result = unsafe {
                WriteFile(
                    self.handle.0,
                    bytes[offset..].as_ptr().cast(),
                    (bytes.len() - offset) as u32,
                    &mut written,
                    ptr::null_mut(),
                )
            };
            if result == 0 {
                let error = io::Error::last_os_error();
                if !idle_error(&error) {
                    return Err(error);
                }
            } else {
                offset += written as usize;
            }
            if offset < bytes.len() {
                if Instant::now() >= deadline {
                    return Err(expired());
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
        self.tx_seq = next;
        self.last_sent = Instant::now();
        Ok(())
    }

    pub fn recv_timeout(&mut self, timeout: Duration) -> io::Result<Value> {
        self.receive(timeout, true)
    }

    /// Remaining unconsumed wire bytes. A closed pipe with a partial JSONL frame
    /// must not be mistaken for a clean end of a verified event stream.
    pub fn pending_input_len(&self) -> usize {
        self.input.len()
    }

    fn receive(&mut self, timeout: Duration, heartbeat: bool) -> io::Result<Value> {
        let deadline = Instant::now() + timeout;
        let mut attempted = false;
        loop {
            // Even a peer continuously streaming control frames cannot extend
            // this call forever. A zero-duration call still gets one poll.
            if attempted && Instant::now() >= deadline {
                return Err(expired());
            }
            attempted = true;
            if let Some(newline) = self.input.iter().position(|b| *b == b'\n') {
                if newline > MAX_MESSAGE_BYTES {
                    return Err(invalid("incoming message exceeds 16 KiB"));
                }
                let message: Value = serde_json::from_slice(&self.input[..newline])
                    .map_err(|_| invalid("invalid JSONL message"))?;
                self.input.drain(..=newline);
                let expected_sequence = self
                    .rx_seq
                    .checked_add(1)
                    .ok_or_else(|| invalid("sequence exhausted"))?;
                if message["protocol_version"].as_u64() != Some(1)
                    || message["task_id"].as_str() != Some(self.task.as_str())
                    || message["seq"].as_u64() != Some(expected_sequence)
                    || !message["kind"].is_string()
                {
                    return Err(invalid("protocol, task or sequence mismatch"));
                }
                self.rx_seq = expected_sequence;
                self.last_peer = Instant::now();
                match message["kind"].as_str() {
                    Some("ping") if self.authenticated => {
                        self.send_control("pong");
                        continue;
                    }
                    Some("pong") if self.authenticated => continue,
                    _ => return Ok(message),
                }
            }
            if self.input.len() > MAX_MESSAGE_BYTES {
                return Err(invalid("unterminated incoming message exceeds 16 KiB"));
            }
            let mut buffer = [0u8; 4096];
            let mut read = 0;
            let result = unsafe {
                ReadFile(
                    self.handle.0,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut read,
                    ptr::null_mut(),
                )
            };
            if result != 0 && read > 0 {
                self.input.extend_from_slice(&buffer[..read as usize]);
                continue;
            }
            if result == 0 {
                let error = io::Error::last_os_error();
                if !idle_error(&error) {
                    return Err(error);
                }
            }
            // Read first: elapsed local heartbeat timers do not prove the peer
            // was idle while bytes were waiting in the kernel pipe buffer.
            if heartbeat && self.authenticated {
                if self.last_peer.elapsed() > PEER_TIMEOUT {
                    let detail = self.control_write_error.as_ref().map_or_else(
                        || "peer heartbeat expired".to_owned(),
                        |error| {
                            format!("peer heartbeat expired after control write failed: {error}")
                        },
                    );
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, detail));
                }
                if self.last_sent.elapsed() > HEARTBEAT {
                    self.send_control("ping");
                }
            }
            if Instant::now() >= deadline {
                return Err(expired());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn send_control(&mut self, kind: &str) {
        if self.control_write_error.is_none() {
            if let Err(error) = self.send(json!({"kind":kind})) {
                // A control write may race a peer that wrote its final events
                // and closed. It must not impersonate read-side EOF and hide
                // those buffered events. Retain the error for idle diagnostics,
                // stop control writes, and let ReadFile drain or observe EOF.
                self.control_write_error = Some(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    fn pipe() -> String {
        format!(
            r"\\.\pipe\avelin-m0-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    #[test]
    fn real_pipe_progress_cancel_and_finish() {
        let name = pipe();
        let server = PipeServer::bind(&name, TOKEN, "test-task").unwrap();
        let child = thread::spawn(move || {
            let mut client =
                Connection::connect(&name, TOKEN, "test-task", Duration::from_secs(2)).unwrap();
            client
                .send(json!({"kind":"progress", "phase":"running", "completed":1, "total":3}))
                .unwrap();
            assert_eq!(
                client.recv_timeout(Duration::from_secs(2)).unwrap()["kind"],
                "cancel"
            );
            client
                .send(json!({"kind":"finish", "result":"Cancelled", "code":2}))
                .unwrap();
        });
        let mut connection = server
            .accept_from(Duration::from_secs(2), std::process::id())
            .unwrap();
        assert_eq!(connection.peer_pid().unwrap(), std::process::id());
        assert_eq!(
            connection.recv_timeout(Duration::from_secs(2)).unwrap()["completed"],
            1
        );
        connection.send_cancel().unwrap();
        assert_eq!(
            connection.recv_timeout(Duration::from_secs(2)).unwrap()["result"],
            "Cancelled"
        );
        child.join().unwrap();
    }
    #[test]
    fn wrong_token_is_rejected() {
        let name = pipe();
        let server = PipeServer::bind(&name, TOKEN, "test-task").unwrap();
        let child = thread::spawn(move || {
            Connection::connect(
                &name,
                "ffffffffffffffffffffffffffffffff",
                "test-task",
                Duration::from_secs(1),
            )
            .is_err()
        });
        assert!(server.accept(Duration::from_secs(1)).is_err());
        assert!(child.join().unwrap());
    }
    #[test]
    fn wrong_process_never_receives_permission_to_start() {
        let name = pipe();
        let server = PipeServer::bind(&name, TOKEN, "test-task").unwrap();
        let child = thread::spawn(move || {
            Connection::connect(&name, TOKEN, "test-task", Duration::from_secs(2)).is_err()
        });
        let error = match server.accept_from(Duration::from_secs(2), 0) {
            Ok(_) => panic!("wrong process was authorized"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("acknowledgement withheld"));
        assert!(
            child.join().unwrap(),
            "Connect succeeded despite the PID rejection"
        );
    }
    #[test]
    fn empty_listener_has_bounded_timeout() {
        let server = PipeServer::bind(&pipe(), TOKEN, "test-task").unwrap();
        let start = Instant::now();
        assert!(server.accept(Duration::from_millis(30)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn oversize_is_rejected_before_sequence_is_consumed() {
        let name = pipe();
        let server = PipeServer::bind(&name, TOKEN, "test-task").unwrap();
        let child = thread::spawn(move || {
            let mut client =
                Connection::connect(&name, TOKEN, "test-task", Duration::from_secs(2)).unwrap();
            let rejected = client
                .send(json!({"kind":"progress", "phase":"x".repeat(MAX_MESSAGE_BYTES)}))
                .unwrap_err();
            assert_eq!(rejected.kind(), io::ErrorKind::InvalidData);
            client
                .send(json!({"kind":"finish", "result":"Succeeded", "code":0}))
                .unwrap();
        });
        let mut connection = server.accept(Duration::from_secs(2)).unwrap();
        assert_eq!(
            connection.recv_timeout(Duration::from_secs(2)).unwrap()["seq"],
            2
        );
        child.join().unwrap();
    }

    #[test]
    fn wrong_sequence_is_rejected() {
        let name = pipe();
        let server = PipeServer::bind(&name, TOKEN, "test-task").unwrap();
        let child = thread::spawn(move || {
            let client =
                Connection::connect(&name, TOKEN, "test-task", Duration::from_secs(2)).unwrap();
            let bytes = b"{\"protocol_version\":1,\"task_id\":\"test-task\",\"seq\":9,\"kind\":\"progress\"}\n";
            let mut written = 0;
            assert_ne!(
                unsafe {
                    WriteFile(
                        client.handle.0,
                        bytes.as_ptr().cast(),
                        bytes.len() as u32,
                        &mut written,
                        ptr::null_mut(),
                    )
                },
                0
            );
            assert_eq!(written as usize, bytes.len());
        });
        let mut connection = server.accept(Duration::from_secs(2)).unwrap();
        assert_eq!(
            connection
                .recv_timeout(Duration::from_secs(2))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        child.join().unwrap();
    }

    #[test]
    fn eof_after_terminal_exposes_any_trailing_partial_frame() {
        let name = pipe();
        let server = PipeServer::bind(&name, TOKEN, "partial-frame").unwrap();
        let partial = b"{\"kind\":\"finish\"";
        let child = thread::spawn(move || {
            let mut client =
                Connection::connect(&name, TOKEN, "partial-frame", Duration::from_secs(2)).unwrap();
            client
                .send(json!({"kind":"finish", "result":"Succeeded", "code":0}))
                .unwrap();
            let mut written = 0;
            assert_ne!(
                unsafe {
                    WriteFile(
                        client.handle.0,
                        partial.as_ptr().cast(),
                        partial.len() as u32,
                        &mut written,
                        ptr::null_mut(),
                    )
                },
                0
            );
            assert_eq!(written as usize, partial.len());
        });
        let mut connection = server.accept(Duration::from_secs(2)).unwrap();
        child.join().unwrap();
        assert_eq!(
            connection.recv_timeout(Duration::from_secs(2)).unwrap()["result"],
            "Succeeded"
        );
        let error = connection.recv_timeout(Duration::from_secs(2)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(109));
        assert_eq!(connection.pending_input_len(), partial.len());
    }

    #[test]
    fn stale_heartbeat_timers_do_not_hide_queued_terminal_and_conflict() {
        let name = pipe();
        let server = PipeServer::bind(&name, TOKEN, "queued-terminals").unwrap();
        let child = thread::spawn(move || {
            let mut client =
                Connection::connect(&name, TOKEN, "queued-terminals", Duration::from_secs(2))
                    .unwrap();
            client
                .send(json!({"kind":"finish", "result":"Succeeded", "code":0}))
                .unwrap();
            client
                .send(json!({"kind":"finish", "result":"Failed", "code":3}))
                .unwrap();
        });
        let mut connection = server.accept(Duration::from_secs(2)).unwrap();
        child.join().unwrap();
        connection.last_sent = Instant::now() - HEARTBEAT - Duration::from_secs(1);
        connection.last_peer = Instant::now() - PEER_TIMEOUT - Duration::from_secs(1);
        assert_eq!(
            connection.recv_timeout(Duration::from_secs(2)).unwrap()["result"],
            "Succeeded"
        );
        assert_eq!(
            connection.recv_timeout(Duration::from_secs(2)).unwrap()["result"],
            "Failed"
        );
        assert_eq!(
            connection
                .recv_timeout(Duration::from_secs(2))
                .unwrap_err()
                .raw_os_error(),
            Some(109)
        );
        assert_eq!(connection.pending_input_len(), 0);
    }

    #[test]
    fn failed_pong_write_cannot_impersonate_read_eof_and_skip_queued_events() {
        let name = pipe();
        let server = PipeServer::bind(&name, TOKEN, "pong-race").unwrap();
        let child = thread::spawn(move || {
            let mut client =
                Connection::connect(&name, TOKEN, "pong-race", Duration::from_secs(2)).unwrap();
            client.send(json!({"kind":"ping"})).unwrap();
            client
                .send(json!({"kind":"finish", "result":"Succeeded", "code":0}))
                .unwrap();
            client
                .send(json!({"kind":"finish", "result":"Failed", "code":3}))
                .unwrap();
        });
        let mut connection = server.accept(Duration::from_secs(2)).unwrap();
        child.join().unwrap();
        // A failed nonblocking WriteFile may consume the existing I/O budget.
        // A receive timeout is not EOF; the next bounded poll must still drain.
        let deadline = Instant::now() + Duration::from_secs(4);
        let first = loop {
            match connection.recv_timeout(Duration::from_millis(100)) {
                Ok(message) => break message,
                Err(error)
                    if error.kind() == io::ErrorKind::TimedOut && Instant::now() < deadline => {}
                Err(error) => panic!("queued terminal lost after pong write: {error}"),
            }
        };
        assert_eq!(first["result"], "Succeeded");
        assert!(
            connection.control_write_error.is_some(),
            "closed peer must reject pong write"
        );
        assert_eq!(
            connection.recv_timeout(Duration::from_secs(2)).unwrap()["result"],
            "Failed"
        );
        assert_eq!(
            connection
                .recv_timeout(Duration::from_secs(2))
                .unwrap_err()
                .raw_os_error(),
            Some(109)
        );
        assert_eq!(connection.pending_input_len(), 0);
    }
}
