//! A small, synchronous clipboard transaction for the directory input.
//!
//! Opening the Windows clipboard can briefly fail while another application is
//! using it. Retry only those errors, with 30 ms of total requested waiting.
//! Neither clipboard contents nor the selected directory belong in diagnostics.

use std::{
    fmt,
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};

const MAX_TEXT_BYTES: usize = 1024 * 1024;
const OPEN_RETRY_MS: [u64; 4] = [2, 4, 8, 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClipboardAction {
    Copy,
    Cut,
    Paste,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClipboardEdit {
    Copied,
    Cut,
    Paste(String),
    NoText,
    NoSelection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ClipboardError {
    InvalidOwner,
    EmbeddedNul,
    TooLarge,
    InvalidUnicode,
    InvalidTextData,
    #[cfg(not(target_os = "windows"))]
    UnsupportedPlatform,
    Os {
        operation: &'static str,
        code: u32,
        attempts: usize,
        elapsed_ms: u128,
    },
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOwner => f.write_str("clipboard owner window is unavailable"),
            Self::EmbeddedNul => f.write_str("clipboard text contains an embedded NUL"),
            Self::TooLarge => f.write_str("clipboard text exceeds the 1 MiB limit"),
            Self::InvalidUnicode => f.write_str("clipboard text is not valid UTF-16"),
            Self::InvalidTextData => f.write_str("clipboard Unicode text has an invalid layout"),
            #[cfg(not(target_os = "windows"))]
            Self::UnsupportedPlatform => f.write_str("clipboard platform is unsupported"),
            Self::Os {
                operation,
                code,
                attempts,
                elapsed_ms,
            } => {
                write!(f, "clipboard {operation} failed (code={code}")?;
                if *attempts > 0 {
                    write!(f, ", attempts={attempts}, elapsed_ms={elapsed_ms}")?;
                }
                f.write_str(")")
            }
        }
    }
}

impl std::error::Error for ClipboardError {}

pub(super) fn transfer(
    owner: usize,
    action: ClipboardAction,
    selected: &str,
) -> Result<ClipboardEdit, ClipboardError> {
    #[cfg(target_os = "windows")]
    {
        transfer_with(
            &native::WindowsClipboard,
            owner,
            action,
            selected,
            std::thread::sleep,
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (owner, action, selected);
        Err(ClipboardError::UnsupportedPlatform)
    }
}

/// A non-owning format check for enabling the Paste menu item. Never opens or
/// reads the clipboard while rendering the input.
pub(super) fn text_available() -> bool {
    #[cfg(target_os = "windows")]
    {
        native::WindowsClipboard.text_available()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

trait ClipboardOs {
    /// Owned until `publish` succeeds; dropping it must release local storage.
    type Memory;

    fn open(&self, owner: usize) -> Result<(), ClipboardError>;
    fn close(&self);
    fn text_available(&self) -> bool;
    fn read(&self) -> Result<Vec<u16>, ClipboardError>;
    fn prepare(&self, text: &[u16]) -> Result<Self::Memory, ClipboardError>;
    fn empty(&self) -> Result<(), ClipboardError>;
    fn publish(&self, memory: &mut Self::Memory) -> Result<(), ClipboardError>;
}

/// Keep Open/Close on the same thread, including early returns and unwinding.
struct ClipboardSession<'a, O: ClipboardOs> {
    os: &'a O,
    _same_thread: PhantomData<Rc<()>>,
}

impl<O: ClipboardOs> Drop for ClipboardSession<'_, O> {
    fn drop(&mut self) {
        self.os.close();
    }
}

fn open_session<O: ClipboardOs>(
    os: &O,
    owner: usize,
    mut wait: impl FnMut(Duration),
) -> Result<ClipboardSession<'_, O>, ClipboardError> {
    let started = Instant::now();
    for attempt in 0..=OPEN_RETRY_MS.len() {
        match os.open(owner) {
            Ok(()) => {
                return Ok(ClipboardSession {
                    os,
                    _same_thread: PhantomData,
                });
            }
            Err(mut error) => {
                if !retryable_open_error(&error) || attempt == OPEN_RETRY_MS.len() {
                    if let ClipboardError::Os {
                        attempts,
                        elapsed_ms,
                        ..
                    } = &mut error
                    {
                        *attempts = attempt + 1;
                        *elapsed_ms = started.elapsed().as_millis();
                    }
                    return Err(error);
                }
                wait(Duration::from_millis(OPEN_RETRY_MS[attempt]));
            }
        }
    }
    unreachable!("bounded clipboard-open loop always returns")
}

fn retryable_open_error(error: &ClipboardError) -> bool {
    // Win32 returns ACCESS_DENIED when another window holds the clipboard.
    // BUSY and LOCK_VIOLATION also describe contention. Never retry unrelated
    // failures such as INVALID_WINDOW_HANDLE or NOT_ENOUGH_MEMORY.
    matches!(
        error,
        ClipboardError::Os {
            operation: "OpenClipboard",
            code: 5 | 33 | 170,
            ..
        }
    )
}

fn encode_text(text: &str) -> Result<Vec<u16>, ClipboardError> {
    if text.contains('\0') {
        return Err(ClipboardError::EmbeddedNul);
    }
    let length = text.encode_utf16().count();
    if length >= MAX_TEXT_BYTES / 2 {
        return Err(ClipboardError::TooLarge);
    }
    let mut encoded = Vec::with_capacity(length + 1);
    encoded.extend(text.encode_utf16());
    encoded.push(0);
    Ok(encoded)
}

fn decode_text(text: &[u16]) -> Result<String, ClipboardError> {
    if text.len() > MAX_TEXT_BYTES / 2 {
        return Err(ClipboardError::TooLarge);
    }
    let end = text
        .iter()
        .position(|unit| *unit == 0)
        .ok_or(ClipboardError::InvalidTextData)?;
    String::from_utf16(&text[..end]).map_err(|_| ClipboardError::InvalidUnicode)
}

fn transfer_with<O: ClipboardOs>(
    os: &O,
    owner: usize,
    action: ClipboardAction,
    selected: &str,
    wait: impl FnMut(Duration),
) -> Result<ClipboardEdit, ClipboardError> {
    if owner == 0 {
        return Err(ClipboardError::InvalidOwner);
    }
    match action {
        ClipboardAction::Copy | ClipboardAction::Cut => {
            if selected.is_empty() {
                return Ok(ClipboardEdit::NoSelection);
            }
            // Validate and allocate before EmptyClipboard can affect the old
            // value. No input mutation is allowed until publication succeeds.
            let encoded = encode_text(selected)?;
            let mut memory = os.prepare(&encoded)?;
            let _session = open_session(os, owner, wait)?;
            os.empty()?;
            os.publish(&mut memory)?;
            Ok(match action {
                ClipboardAction::Cut => ClipboardEdit::Cut,
                _ => ClipboardEdit::Copied,
            })
        }
        ClipboardAction::Paste => {
            let _session = open_session(os, owner, wait)?;
            if !os.text_available() {
                return Ok(ClipboardEdit::NoText);
            }
            let text = decode_text(&os.read()?)?;
            Ok(if text.is_empty() {
                ClipboardEdit::NoText
            } else {
                ClipboardEdit::Paste(text)
            })
        }
    }
}

#[cfg(target_os = "windows")]
mod native {
    use super::{ClipboardError, ClipboardOs, MAX_TEXT_BYTES};
    use windows::Win32::{
        Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
                OpenClipboard, SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
            Ole::CF_UNICODETEXT,
        },
    };

    pub(super) struct WindowsClipboard;

    pub(super) struct OwnedGlobal(Option<HGLOBAL>);

    impl Drop for OwnedGlobal {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take() {
                // GlobalFree returns NULL on success; its generated Result
                // wrapper is not a useful success indication. Always call it
                // exactly once for memory whose ownership was not transferred.
                unsafe {
                    let _ = GlobalFree(Some(handle));
                }
            }
        }
    }

    struct LockedGlobal {
        handle: HGLOBAL,
        pointer: *mut u16,
    }

    impl LockedGlobal {
        fn lock(handle: HGLOBAL) -> Result<Self, ClipboardError> {
            let pointer = unsafe { GlobalLock(handle) }.cast::<u16>();
            if pointer.is_null() {
                return Err(last_error("GlobalLock"));
            }
            Ok(Self { handle, pointer })
        }
    }

    impl Drop for LockedGlobal {
        fn drop(&mut self) {
            // Releasing the last lock returns zero even on success.
            unsafe {
                let _ = GlobalUnlock(self.handle);
            }
        }
    }

    fn os_error(operation: &'static str, error: windows::core::Error) -> ClipboardError {
        let raw = error.code().0 as u32;
        let code = if raw & 0xffff_0000 == 0x8007_0000 {
            raw & 0xffff
        } else {
            raw
        };
        ClipboardError::Os {
            operation,
            code,
            attempts: 0,
            elapsed_ms: 0,
        }
    }

    fn last_error(operation: &'static str) -> ClipboardError {
        os_error(operation, windows::core::Error::from_win32())
    }

    impl ClipboardOs for WindowsClipboard {
        type Memory = OwnedGlobal;

        fn open(&self, owner: usize) -> Result<(), ClipboardError> {
            unsafe { OpenClipboard(Some(HWND(owner as *mut _))) }
                .map_err(|error| os_error("OpenClipboard", error))
        }

        fn close(&self) {
            if let Err(error) = unsafe { CloseClipboard() } {
                log::warn!("{}", os_error("CloseClipboard", error));
            }
        }

        fn text_available(&self) -> bool {
            unsafe { IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)) }.is_ok()
        }

        fn read(&self) -> Result<Vec<u16>, ClipboardError> {
            let handle = unsafe { GetClipboardData(u32::from(CF_UNICODETEXT.0)) }
                .map_err(|error| os_error("GetClipboardData", error))?;
            let global = HGLOBAL(handle.0);
            let bytes = unsafe { GlobalSize(global) };
            if bytes == 0 {
                return Err(last_error("GlobalSize"));
            }
            if bytes > MAX_TEXT_BYTES {
                return Err(ClipboardError::TooLarge);
            }
            if bytes < 2 || bytes % 2 != 0 {
                return Err(ClipboardError::InvalidTextData);
            }
            let locked = LockedGlobal::lock(global)?;
            // GlobalSize bounds the allocation, which stays locked and owned
            // by Windows until this clipboard transaction ends. Stop at NUL
            // instead of copying potentially uninitialized trailing padding.
            // Never free the clipboard-owned handle.
            let mut text = Vec::new();
            for index in 0..bytes / 2 {
                let unit = unsafe { locked.pointer.add(index).read() };
                text.push(unit);
                if unit == 0 {
                    return Ok(text);
                }
            }
            Err(ClipboardError::InvalidTextData)
        }

        fn prepare(&self, text: &[u16]) -> Result<Self::Memory, ClipboardError> {
            let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of_val(text)) }
                .map_err(|error| os_error("GlobalAlloc", error))?;
            let memory = OwnedGlobal(Some(handle));
            let locked = LockedGlobal::lock(handle)?;
            // The allocation is precisely large enough for validated text and
            // its trailing NUL. The lock drops before memory can be published.
            unsafe {
                std::ptr::copy_nonoverlapping(text.as_ptr(), locked.pointer, text.len());
            }
            drop(locked);
            Ok(memory)
        }

        fn empty(&self) -> Result<(), ClipboardError> {
            unsafe { EmptyClipboard() }.map_err(|error| os_error("EmptyClipboard", error))
        }

        fn publish(&self, memory: &mut Self::Memory) -> Result<(), ClipboardError> {
            let handle = memory.0.expect("clipboard memory is published only once");
            unsafe { SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(handle.0))) }
                .map_err(|error| os_error("SetClipboardData", error))?;
            // Windows owns the HGLOBAL only after successful SetClipboardData.
            memory.0 = None;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque};

    #[derive(Default)]
    struct FakeState {
        events: Vec<&'static str>,
        open_results: VecDeque<Result<(), ClipboardError>>,
        available: bool,
        read: Vec<u16>,
        fail_prepare: bool,
        fail_empty: bool,
        fail_publish: bool,
        published: Vec<u16>,
    }

    #[derive(Default)]
    struct FakeOs(Rc<RefCell<FakeState>>);

    struct FakeMemory {
        state: Rc<RefCell<FakeState>>,
        text: Vec<u16>,
        transferred: bool,
    }

    impl Drop for FakeMemory {
        fn drop(&mut self) {
            if !self.transferred {
                self.state.borrow_mut().events.push("free");
            }
        }
    }

    fn failure(operation: &'static str, code: u32) -> ClipboardError {
        ClipboardError::Os {
            operation,
            code,
            attempts: 0,
            elapsed_ms: 0,
        }
    }

    impl ClipboardOs for FakeOs {
        type Memory = FakeMemory;

        fn open(&self, _: usize) -> Result<(), ClipboardError> {
            let mut state = self.0.borrow_mut();
            state.events.push("open");
            state.open_results.pop_front().unwrap_or(Ok(()))
        }

        fn close(&self) {
            self.0.borrow_mut().events.push("close");
        }

        fn text_available(&self) -> bool {
            self.0.borrow().available
        }

        fn read(&self) -> Result<Vec<u16>, ClipboardError> {
            let mut state = self.0.borrow_mut();
            state.events.push("read");
            Ok(state.read.clone())
        }

        fn prepare(&self, text: &[u16]) -> Result<Self::Memory, ClipboardError> {
            let mut state = self.0.borrow_mut();
            state.events.push("prepare");
            if state.fail_prepare {
                return Err(failure("GlobalAlloc", 8));
            }
            Ok(FakeMemory {
                state: self.0.clone(),
                text: text.to_vec(),
                transferred: false,
            })
        }

        fn empty(&self) -> Result<(), ClipboardError> {
            let mut state = self.0.borrow_mut();
            state.events.push("empty");
            if state.fail_empty {
                return Err(failure("EmptyClipboard", 1418));
            }
            Ok(())
        }

        fn publish(&self, memory: &mut Self::Memory) -> Result<(), ClipboardError> {
            let mut state = self.0.borrow_mut();
            state.events.push("publish");
            if state.fail_publish {
                return Err(failure("SetClipboardData", 8));
            }
            state.published = memory.text.clone();
            memory.transferred = true;
            Ok(())
        }
    }

    #[test]
    fn transient_busy_recovers_and_transfers_memory_after_publication() {
        let os = FakeOs::default();
        os.0.borrow_mut().open_results = VecDeque::from([
            Err(failure("OpenClipboard", 5)),
            Err(failure("OpenClipboard", 170)),
            Ok(()),
        ]);
        let mut waits = Vec::new();
        assert_eq!(
            transfer_with(&os, 1, ClipboardAction::Copy, "C:\\安装😀", |d| waits
                .push(d)),
            Ok(ClipboardEdit::Copied)
        );
        assert_eq!(waits, [Duration::from_millis(2), Duration::from_millis(4)]);
        let state = os.0.borrow();
        assert_eq!(
            state.events,
            [
                "prepare", "open", "open", "open", "empty", "publish", "close"
            ]
        );
        assert_eq!(decode_text(&state.published).unwrap(), "C:\\安装😀");
    }

    #[test]
    fn persistent_busy_stops_after_five_attempts_and_thirty_ms_wait() {
        let os = FakeOs::default();
        os.0.borrow_mut().open_results = (0..6).map(|_| Err(failure("OpenClipboard", 5))).collect();
        let mut waits = Vec::new();
        assert!(matches!(
            transfer_with(&os, 1, ClipboardAction::Cut, "selected", |d| waits.push(d)),
            Err(ClipboardError::Os {
                operation: "OpenClipboard",
                code: 5,
                attempts: 5,
                ..
            })
        ));
        assert_eq!(
            waits.iter().copied().sum::<Duration>(),
            Duration::from_millis(30)
        );
        assert_eq!(
            os.0.borrow().events,
            ["prepare", "open", "open", "open", "open", "open", "free"]
        );
    }

    #[test]
    fn non_contention_open_failure_is_not_retried() {
        let os = FakeOs::default();
        os.0.borrow_mut()
            .open_results
            .push_back(Err(failure("OpenClipboard", 1400)));
        assert!(matches!(
            transfer_with(&os, 1, ClipboardAction::Paste, "", |_| panic!(
                "must not wait"
            )),
            Err(ClipboardError::Os {
                operation: "OpenClipboard",
                code: 1400,
                attempts: 1,
                ..
            })
        ));
        assert_eq!(os.0.borrow().events, ["open"]);
    }

    #[test]
    fn failed_cut_does_not_produce_an_edit_and_frees_unpublished_memory() {
        for operation in ["empty", "publish"] {
            let os = FakeOs::default();
            os.0.borrow_mut().fail_empty = operation == "empty";
            os.0.borrow_mut().fail_publish = operation == "publish";
            let result = transfer_with(&os, 1, ClipboardAction::Cut, "keep me", |_| {});
            assert!(result.is_err());
            let state = os.0.borrow();
            assert!(state.published.is_empty());
            assert!(state.events.ends_with(&["close", "free"]));
            assert_eq!(
                state
                    .events
                    .iter()
                    .filter(|event| **event == "close")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn successful_cut_allows_edit_only_after_publication() {
        let os = FakeOs::default();
        assert_eq!(
            transfer_with(&os, 1, ClipboardAction::Cut, "cut", |_| {}),
            Ok(ClipboardEdit::Cut)
        );
        assert_eq!(
            os.0.borrow().events,
            ["prepare", "open", "empty", "publish", "close"]
        );
    }

    #[test]
    fn allocation_failure_never_opens_or_empties_clipboard() {
        let os = FakeOs::default();
        os.0.borrow_mut().fail_prepare = true;
        assert!(transfer_with(&os, 1, ClipboardAction::Copy, "selected", |_| {}).is_err());
        assert_eq!(os.0.borrow().events, ["prepare"]);
    }

    #[test]
    fn paste_distinguishes_absent_text_from_invalid_text_and_closes_both() {
        let os = FakeOs::default();
        assert_eq!(
            transfer_with(&os, 1, ClipboardAction::Paste, "", |_| {}),
            Ok(ClipboardEdit::NoText)
        );
        assert_eq!(os.0.borrow().events, ["open", "close"]);
        os.0.borrow_mut().events.clear();
        os.0.borrow_mut().available = true;
        os.0.borrow_mut().read = vec![0xd800, 0];
        assert_eq!(
            transfer_with(&os, 1, ClipboardAction::Paste, "", |_| {}),
            Err(ClipboardError::InvalidUnicode)
        );
        assert_eq!(os.0.borrow().events, ["open", "read", "close"]);
    }

    #[test]
    fn paste_preserves_unicode_and_stops_at_terminator() {
        let os = FakeOs::default();
        os.0.borrow_mut().available = true;
        let mut units = encode_text("中文😀\r\n").unwrap();
        units.extend_from_slice(&[0xd800, 1]);
        os.0.borrow_mut().read = units;
        assert_eq!(
            transfer_with(&os, 1, ClipboardAction::Paste, "", |_| {}),
            Ok(ClipboardEdit::Paste("中文😀\r\n".into()))
        );
    }

    #[test]
    fn empty_unicode_text_does_not_request_replacing_selection() {
        let os = FakeOs::default();
        os.0.borrow_mut().available = true;
        os.0.borrow_mut().read = vec![0];
        assert_eq!(
            transfer_with(&os, 1, ClipboardAction::Paste, "selected", |_| {}),
            Ok(ClipboardEdit::NoText)
        );
        assert_eq!(os.0.borrow().events, ["open", "read", "close"]);
    }

    #[test]
    fn invalid_owner_or_text_leaves_clipboard_untouched() {
        let os = FakeOs::default();
        assert_eq!(
            transfer_with(&os, 0, ClipboardAction::Copy, "a", |_| {}),
            Err(ClipboardError::InvalidOwner)
        );
        assert_eq!(
            transfer_with(&os, 1, ClipboardAction::Copy, "a\0b", |_| {}),
            Err(ClipboardError::EmbeddedNul)
        );
        assert_eq!(
            transfer_with(&os, 1, ClipboardAction::Cut, "", |_| {}),
            Ok(ClipboardEdit::NoSelection)
        );
        assert!(os.0.borrow().events.is_empty());
    }

    #[test]
    fn text_size_limit_includes_utf16_terminator_and_rejects_unterminated_data() {
        let largest = "a".repeat(MAX_TEXT_BYTES / 2 - 1);
        assert_eq!(encode_text(&largest).unwrap().len() * 2, MAX_TEXT_BYTES);
        assert_eq!(encode_text(&(largest + "a")), Err(ClipboardError::TooLarge));
        assert_eq!(decode_text(&[65, 66]), Err(ClipboardError::InvalidTextData));
        assert_eq!(
            decode_text(&vec![0; MAX_TEXT_BYTES / 2 + 1]),
            Err(ClipboardError::TooLarge)
        );
    }

    #[test]
    fn diagnostics_contain_only_operation_and_numeric_error() {
        assert_eq!(
            failure("OpenClipboard", 5).to_string(),
            "clipboard OpenClipboard failed (code=5)"
        );
        assert_eq!(
            ClipboardError::Os {
                operation: "OpenClipboard",
                code: 5,
                attempts: 5,
                elapsed_ms: 31,
            }
            .to_string(),
            "clipboard OpenClipboard failed (code=5, attempts=5, elapsed_ms=31)"
        );
        assert!(!retryable_open_error(&failure("SetClipboardData", 5)));
    }

    #[test]
    fn session_closes_during_unwinding() {
        let os = FakeOs::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _session = open_session(&os, 1, |_| {}).unwrap();
            panic!("synthetic failure");
        }));
        assert!(result.is_err());
        assert_eq!(os.0.borrow().events, ["open", "close"]);
    }
}
