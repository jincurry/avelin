//! Best-effort diagnostics for the GUI runtime, independent of installation outcomes.
//!
//! Records and file size are bounded. Formatting and writing happen synchronously,
//! so an individual filesystem write can still stall on a slow device. Contending
//! or recursive log calls are dropped rather than waiting for the writer. This is
//! a diagnostic aid, not an exhaustive event journal or a GPU health detector.

use log::{Level, LevelFilter, Log, Metadata, Record};
use serde::Serialize;
use std::fmt::{self, Write as _};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_MESSAGE_BYTES: usize = 2 * 1024;
const MAX_TARGET_BYTES: usize = 256;
const MAX_SOURCE_FILE_BYTES: usize = 256;
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_SUMMARY_BYTES: usize = 16 * 1024;
const MAX_ERROR_BYTES: usize = 1024;
const TRUNCATED_SUFFIX: &str = "… [truncated]";

static LOGGER: OnceLock<RuntimeLogger> = OnceLock::new();
static REGISTRATION: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticSnapshot {
    pub initialized: bool,
    pub log_path: Option<PathBuf>,
    pub error_count: u64,
    pub warning_count: u64,
    pub dropped_count: u64,
    pub truncated_records: u64,
    pub truncated: bool,
    pub file_limit_reached: bool,
    /// Successfully written bytes, including any partial final line after an I/O error.
    pub bytes_written: usize,
    pub write_error: Option<String>,
    /// Saved only by `finish`; the separate summary has a 16 KiB limit.
    pub summary_path: Option<PathBuf>,
    pub summary_write_error: Option<String>,
}

pub struct RuntimeLogger {
    state: Mutex<LogState>,
    initialized: AtomicBool,
    errors: AtomicU64,
    warnings: AtomicU64,
    dropped: AtomicU64,
    truncated_records: AtomicU64,
}

struct LogState {
    writer: Option<Box<dyn Write + Send>>,
    path: Option<PathBuf>,
    bytes_written: usize,
    byte_limit: usize,
    file_limit_reached: bool,
    write_error: Option<String>,
    final_snapshot: Option<DiagnosticSnapshot>,
}

#[derive(Serialize)]
struct DiagnosticRecord<'a> {
    pid: u32,
    /// Unix milliseconds; null if the host clock is earlier than the Unix epoch.
    time: Option<u64>,
    level: &'a str,
    target: &'a str,
    message: &'a str,
    /// Caller metadata is useful when dependencies emit an empty log target.
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u32>,
}

/// Register once, only when the caller is about to enter an actual GUI request.
///
/// A file-open failure still registers the sink so counts and `write_error` remain
/// observable. `Err` means another global logger prevented registration. Neither
/// case is evidence of installation failure, and neither should trigger a retry.
pub fn initialize() -> Result<&'static RuntimeLogger, String> {
    let logger = LOGGER.get_or_init(|| match open_log() {
        Ok((file, path)) => RuntimeLogger::new(Some(Box::new(file)), Some(path), None),
        Err(error) => RuntimeLogger::new(None, None, Some(error)),
    });
    let registration = REGISTRATION.get_or_init(|| {
        log::set_logger(logger).map_err(|error| {
            let detail = bounded_text(
                &format!("global logger registration failed: {error}"),
                MAX_ERROR_BYTES,
            )
            .0;
            let mut state = logger
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.disable(detail.clone());
            detail
        })?;
        logger.initialized.store(true, Ordering::Relaxed);
        log::set_max_level(LevelFilter::Warn);
        Ok(())
    });
    registration.as_ref().map(|()| logger).map_err(Clone::clone)
}

/// Counts are individually sampled, not an atomic transaction with the log file.
pub fn snapshot() -> DiagnosticSnapshot {
    LOGGER
        .get()
        .map(RuntimeLogger::snapshot)
        .unwrap_or(DiagnosticSnapshot {
            initialized: false,
            log_path: None,
            error_count: 0,
            warning_count: 0,
            dropped_count: 0,
            truncated_records: 0,
            truncated: false,
            file_limit_reached: false,
            bytes_written: 0,
            write_error: None,
            summary_path: None,
            summary_write_error: None,
        })
}

pub fn flush() {
    if let Some(logger) = LOGGER.get() {
        Log::flush(logger);
    }
}

/// Flush and persist one bounded summary before normal process termination.
/// The JSONL limit is 1 MiB; this separate summary has its own 16 KiB limit.
/// This is a snapshot at the first `finish`, not evidence that later/background
/// logging cannot occur. Repeated calls return that same snapshot without writes.
/// Any summary failure is returned for the caller to show on stderr; it does not
/// replace installation/task results. A partial failed sidecar is retained.
pub fn finish() -> DiagnosticSnapshot {
    LOGGER
        .get()
        .map(RuntimeLogger::finish)
        .unwrap_or_else(snapshot)
}

impl RuntimeLogger {
    fn new(
        writer: Option<Box<dyn Write + Send>>,
        path: Option<PathBuf>,
        write_error: Option<String>,
    ) -> Self {
        Self {
            state: Mutex::new(LogState {
                writer,
                path,
                bytes_written: 0,
                byte_limit: MAX_FILE_BYTES,
                file_limit_reached: false,
                write_error,
                final_snapshot: None,
            }),
            initialized: AtomicBool::new(false),
            errors: AtomicU64::new(0),
            warnings: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            truncated_records: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> DiagnosticSnapshot {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.capture_snapshot(&state)
    }

    fn capture_snapshot(&self, state: &LogState) -> DiagnosticSnapshot {
        let truncated_records = self.truncated_records.load(Ordering::Relaxed);
        DiagnosticSnapshot {
            initialized: self.initialized.load(Ordering::Relaxed),
            log_path: state.path.clone(),
            error_count: self.errors.load(Ordering::Relaxed),
            warning_count: self.warnings.load(Ordering::Relaxed),
            dropped_count: self.dropped.load(Ordering::Relaxed),
            truncated_records,
            truncated: truncated_records > 0 || state.file_limit_reached,
            file_limit_reached: state.file_limit_reached,
            bytes_written: state.bytes_written,
            write_error: state.write_error.clone(),
            summary_path: state
                .final_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.summary_path.clone()),
            summary_write_error: state
                .final_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.summary_write_error.clone()),
        }
    }

    fn finish(&self) -> DiagnosticSnapshot {
        self.flush();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(snapshot) = &state.final_snapshot {
            return snapshot.clone();
        }
        let mut snapshot = self.capture_snapshot(&state);
        if let Some(log_path) = &state.path {
            let summary_path = log_path.with_extension("summary.json");
            snapshot.summary_path = Some(summary_path.clone());
            if let Err(error) = save_summary(&summary_path, &snapshot) {
                snapshot.summary_path = None;
                snapshot.summary_write_error = Some(error);
            }
        }
        state.final_snapshot = Some(snapshot.clone());
        snapshot
    }

    fn write_record(&self, state: &mut LogState, record: &Record<'_>) {
        if state.writer.is_none() || state.file_limit_reached {
            increment(&self.dropped);
            return;
        }

        // Format directly into a bounded writer: never allocate the full message
        // before truncation, including when the log argument is a large object.
        let mut message = BoundedText::new(MAX_MESSAGE_BYTES);
        let formatted = message.write_fmt(*record.args());
        if formatted.is_err() && !message.truncated {
            state.disable("diagnostic message formatter failed".into());
            increment(&self.dropped);
            return;
        }
        let (message, message_truncated) = message.finish();
        let (target, target_truncated) = bounded_text(record.target(), MAX_TARGET_BYTES);
        let source_file = record
            .file()
            .map(|file| bounded_text(file, MAX_SOURCE_FILE_BYTES));
        let file_truncated = source_file
            .as_ref()
            .is_some_and(|(_, truncated)| *truncated);
        if message_truncated || target_truncated || file_truncated {
            increment(&self.truncated_records);
        }
        let entry = DiagnosticRecord {
            pid: std::process::id(),
            time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64),
            level: record.level().as_str(),
            target: &target,
            message: &message,
            file: source_file.as_ref().map(|(file, _)| file.as_str()),
            line: record.line(),
        };
        let mut line = match serde_json::to_vec(&entry) {
            Ok(line) => line,
            Err(error) => {
                state.disable(
                    bounded_text(
                        &format!("diagnostic JSON encoding failed: {error}"),
                        MAX_ERROR_BYTES,
                    )
                    .0,
                );
                increment(&self.dropped);
                return;
            }
        };
        line.push(b'\n');
        if line.len() > MAX_LINE_BYTES
            || line.len() > state.byte_limit.saturating_sub(state.bytes_written)
        {
            state.file_limit_reached = true;
            increment(&self.dropped);
            return;
        }

        let Some(writer) = state.writer.as_mut() else {
            return;
        };
        let mut offset = 0;
        while offset < line.len() {
            match writer.write(&line[offset..]) {
                Ok(0) => {
                    state.disable(
                        "diagnostic write returned zero bytes; final line may be incomplete".into(),
                    );
                    increment(&self.dropped);
                    return;
                }
                Ok(count) if count <= line.len() - offset => {
                    offset += count;
                    state.bytes_written += count;
                }
                Ok(_) => {
                    state.disable("diagnostic writer returned an invalid byte count".into());
                    increment(&self.dropped);
                    return;
                }
                Err(error) => {
                    // Stop also on Interrupted: a best-effort logger must not spin
                    // indefinitely during repeated errors or cancellation signals.
                    state.disable(
                        bounded_text(
                            &format!(
                                "diagnostic write failed; final line may be incomplete: {error}"
                            ),
                            MAX_ERROR_BYTES,
                        )
                        .0,
                    );
                    increment(&self.dropped);
                    return;
                }
            }
        }
    }
}

impl Log for RuntimeLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= Level::Warn
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        match record.level() {
            Level::Error => increment(&self.errors),
            Level::Warn => increment(&self.warnings),
            _ => return,
        }
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => {
                increment(&self.dropped);
                return;
            }
        };
        // Formatting implementations can panic. Catch inside the mutex guard so
        // diagnostic failures do not poison it or unwind through product code.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.write_record(&mut state, record)
        }))
        .is_err()
        {
            state.disable("diagnostic formatter or writer panicked; sink disabled".into());
            increment(&self.dropped);
        }
    }

    fn flush(&self) {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        let Some(writer) = state.writer.as_mut() else {
            return;
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| writer.flush()));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => state.disable(
                bounded_text(
                    &format!("diagnostic flush failed: {error}"),
                    MAX_ERROR_BYTES,
                )
                .0,
            ),
            Err(_) => {
                state.disable("diagnostic writer panicked during flush; sink disabled".into())
            }
        }
    }
}

impl LogState {
    fn disable(&mut self, detail: String) {
        self.writer = None;
        if self.write_error.is_none() {
            self.write_error = Some(detail);
        }
    }
}

fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

struct BoundedText {
    text: String,
    limit: usize,
    truncated: bool,
}

impl BoundedText {
    fn new(limit: usize) -> Self {
        Self {
            text: String::with_capacity(limit),
            limit,
            truncated: false,
        }
    }

    fn finish(mut self) -> (String, bool) {
        if self.truncated && self.limit >= TRUNCATED_SUFFIX.len() {
            let keep = utf8_prefix_len(&self.text, self.limit - TRUNCATED_SUFFIX.len());
            self.text.truncate(keep);
            self.text.push_str(TRUNCATED_SUFFIX);
        }
        (self.text, self.truncated)
    }
}

impl fmt::Write for BoundedText {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.truncated {
            return Err(fmt::Error);
        }
        let remaining = self.limit.saturating_sub(self.text.len());
        if value.len() <= remaining {
            self.text.push_str(value);
            Ok(())
        } else {
            self.text
                .push_str(&value[..utf8_prefix_len(value, remaining)]);
            self.truncated = true;
            Err(fmt::Error)
        }
    }
}

fn utf8_prefix_len(value: &str, limit: usize) -> usize {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn bounded_text(value: &str, limit: usize) -> (String, bool) {
    let mut text = BoundedText::new(limit);
    let _ = text.write_str(value);
    text.finish()
}

fn open_log() -> Result<(File, PathBuf), String> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|base| base.join("Avelin/M0/logs"));
    let fallback = std::env::temp_dir().join("AvelinM0/logs");
    let mut failures = BoundedText::new(MAX_ERROR_BYTES);
    for base in local.into_iter().chain(std::iter::once(fallback)) {
        match open_log_at(&base) {
            Ok(result) => return Ok(result),
            Err(error) => {
                let _ = write!(failures, "{}: {error}; ", base.display());
            }
        }
    }
    Err(failures.finish().0)
}

fn open_log_at(base: &Path) -> io::Result<(File, PathBuf)> {
    if !base.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "diagnostic directory must be absolute",
        ));
    }
    avelin_core::reject_reparse(base).map_err(io::Error::other)?;
    std::fs::create_dir_all(base)?;
    avelin_core::reject_reparse(base).map_err(io::Error::other)?;
    let path = base.join(format!(
        "runtime-{}-{}.jsonl",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    avelin_core::reject_reparse(&path).map_err(io::Error::other)?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    // Recheck after opening. These path checks reduce accidental redirection but
    // do not establish handle-based containment against concurrent path mutation.
    avelin_core::reject_reparse(&path).map_err(io::Error::other)?;
    Ok((file, path))
}

fn save_summary(path: &Path, snapshot: &DiagnosticSnapshot) -> Result<(), String> {
    let result = (|| -> io::Result<()> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic summary path must be absolute",
            ));
        }
        let mut bytes = serde_json::to_vec_pretty(snapshot).map_err(io::Error::other)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_SUMMARY_BYTES {
            return Err(io::Error::other(
                "diagnostic summary exceeds the 16 KiB limit",
            ));
        }
        avelin_core::reject_reparse(path).map_err(io::Error::other)?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        avelin_core::reject_reparse(path).map_err(io::Error::other)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    result.map_err(|error| {
        bounded_text(
            &format!(
                "diagnostic summary was not saved completely ({}): {error}",
                path.display()
            ),
            MAX_ERROR_BYTES,
        )
        .0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct MemoryWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for MemoryWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn emit(logger: &RuntimeLogger, level: Level, target: &str, message: &str) {
        logger.log(
            &Record::builder()
                .level(level)
                .target(target)
                .args(format_args!("{message}"))
                .build(),
        );
    }

    #[test]
    fn unicode_and_escaped_control_characters_stay_bounded_valid_jsonl() {
        let writer = MemoryWriter::default();
        let logger = RuntimeLogger::new(Some(Box::new(writer.clone())), None, None);
        let message = "安装😀\n\"\u{0000}".repeat(1000);
        emit(&logger, Level::Error, &"窗口😀".repeat(100), &message);
        let bytes = writer.0.lock().unwrap();
        assert!(bytes.len() <= MAX_LINE_BYTES);
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        let entry: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(entry.as_object().unwrap().len(), 5);
        assert!(entry["message"].as_str().unwrap().len() <= MAX_MESSAGE_BYTES);
        assert!(entry["target"].as_str().unwrap().len() <= MAX_TARGET_BYTES);
        assert!(
            entry["message"]
                .as_str()
                .unwrap()
                .ends_with(TRUNCATED_SUFFIX)
        );
        assert_eq!(logger.snapshot().truncated_records, 1);
        assert_eq!(logger.snapshot().error_count, 1);
    }

    #[test]
    fn caller_location_survives_empty_targets_and_remains_optional_and_bounded() {
        let writer = MemoryWriter::default();
        let logger = RuntimeLogger::new(Some(Box::new(writer.clone())), None, None);
        let source = "registry/gpui-pre-windows-0.3.4/src/window.rs";
        logger.log(
            &Record::builder()
                .level(Level::Error)
                .target("")
                .file(Some(source))
                .line(Some(599))
                .args(format_args!("window handle fixture"))
                .build(),
        );
        emit(&logger, Level::Warn, "driver", "no caller metadata");
        let long_source = "目录😀\n\"\u{0000}".repeat(1000);
        logger.log(
            &Record::builder()
                .level(Level::Error)
                .target("")
                .file(Some(&long_source))
                .line(Some(u32::MAX))
                .args(format_args!("bounded caller fixture"))
                .build(),
        );
        let bytes = writer.0.lock().unwrap();
        let entries = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                assert!(line.len() + 1 <= MAX_LINE_BYTES);
                serde_json::from_slice::<serde_json::Value>(line).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["target"], "");
        assert_eq!(entries[0]["file"], source);
        assert_eq!(entries[0]["line"], 599);
        assert!(entries[1].get("file").is_none());
        assert!(entries[1].get("line").is_none());
        let saved_source = entries[2]["file"].as_str().unwrap();
        assert!(saved_source.len() <= MAX_SOURCE_FILE_BYTES);
        assert!(saved_source.ends_with(TRUNCATED_SUFFIX));
        assert_eq!(entries[2]["line"], u32::MAX);
        let snapshot = logger.snapshot();
        assert_eq!(snapshot.truncated_records, 1);
        assert_eq!(snapshot.dropped_count, 0);
        assert_eq!((snapshot.error_count, snapshot.warning_count), (2, 1));
    }

    #[test]
    fn a_log_storm_cannot_exceed_the_file_budget() {
        let writer = MemoryWriter::default();
        let logger = RuntimeLogger::new(Some(Box::new(writer.clone())), None, None);
        logger.state.lock().unwrap().byte_limit = 700;
        for _ in 0..1000 {
            emit(&logger, Level::Warn, "driver", &"x".repeat(80));
        }
        let bytes = writer.0.lock().unwrap();
        assert!(bytes.len() <= 700);
        assert!(!bytes.is_empty());
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            serde_json::from_slice::<serde_json::Value>(line).unwrap();
        }
        let state = logger.snapshot();
        assert_eq!(state.warning_count, 1000);
        assert!(state.file_limit_reached && state.truncated && state.dropped_count > 0);
        assert_eq!(state.bytes_written, bytes.len());
    }

    struct PartialFailure {
        first: bool,
    }
    impl Write for PartialFailure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.first {
                self.first = false;
                Ok(bytes.len().min(7))
            } else {
                Err(io::Error::other("injected disk error"))
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_write_failure_is_observable_and_disables_further_writes() {
        let logger = RuntimeLogger::new(Some(Box::new(PartialFailure { first: true })), None, None);
        emit(&logger, Level::Error, "driver", "first");
        emit(&logger, Level::Warn, "driver", "second");
        let state = logger.snapshot();
        assert_eq!(state.bytes_written, 7);
        assert_eq!(state.dropped_count, 2);
        assert!(state.write_error.unwrap().contains("injected disk error"));
    }

    #[test]
    fn no_file_and_recursive_contention_keep_counts_without_blocking() {
        let logger = RuntimeLogger::new(None, None, Some("injected open failure".into()));
        emit(&logger, Level::Warn, "driver", "no file");
        {
            let _guard = logger.state.lock().unwrap();
            emit(&logger, Level::Error, "driver", "recursive call");
        }
        emit(&logger, Level::Info, "driver", "ignored");
        let state = logger.snapshot();
        assert_eq!(
            (state.warning_count, state.error_count, state.dropped_count),
            (1, 1, 2)
        );
        assert_eq!(state.write_error.as_deref(), Some("injected open failure"));
    }

    struct PanicDisplay;
    impl fmt::Display for PanicDisplay {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            panic!("injected formatter panic")
        }
    }

    #[test]
    fn formatter_panic_does_not_escape_the_logger() {
        let logger = RuntimeLogger::new(Some(Box::new(MemoryWriter::default())), None, None);
        logger.log(
            &Record::builder()
                .level(Level::Error)
                .target("driver")
                .args(format_args!("{}", PanicDisplay))
                .build(),
        );
        let state = logger.snapshot();
        assert_eq!(state.dropped_count, 1);
        assert!(state.write_error.unwrap().contains("panicked"));
    }

    struct FlushFailure;
    impl Write for FlushFailure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("injected flush error"))
        }
    }

    #[test]
    fn flush_error_is_observable_without_reclassifying_logged_errors() {
        let logger = RuntimeLogger::new(Some(Box::new(FlushFailure)), None, None);
        emit(&logger, Level::Warn, "driver", "warning");
        logger.flush();
        let state = logger.snapshot();
        assert!(state.write_error.unwrap().contains("injected flush error"));
        assert_eq!(state.error_count, 0);
        assert_eq!(state.warning_count, 1);
    }

    #[test]
    fn finish_persists_one_bounded_summary_and_repeated_calls_do_not_overwrite() {
        let root = std::env::temp_dir()
            .join("AvelinM0RuntimeLogTests")
            .join(uuid::Uuid::new_v4().simple().to_string());
        let (file, path) = open_log_at(&root).unwrap();
        let logger = RuntimeLogger::new(Some(Box::new(file)), Some(path), None);
        emit(&logger, Level::Warn, "driver", "summary fixture");
        let first = logger.finish();
        let saved = first.summary_path.clone().unwrap();
        let bytes = std::fs::read(&saved).unwrap();
        assert!(bytes.len() <= MAX_SUMMARY_BYTES);
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["warning_count"], 1);
        assert_eq!(json["error_count"], 0);
        assert!(first.summary_write_error.is_none());
        assert_eq!(logger.finish().summary_path, Some(saved.clone()));
        assert_eq!(std::fs::read(saved).unwrap(), bytes);
    }

    #[test]
    fn finish_preserves_an_existing_sidecar_and_reports_the_save_failure() {
        let root = std::env::temp_dir()
            .join("AvelinM0RuntimeLogTests")
            .join(uuid::Uuid::new_v4().simple().to_string());
        let (file, path) = open_log_at(&root).unwrap();
        let blocked = path.with_extension("summary.json");
        std::fs::write(&blocked, b"owned collision sentinel").unwrap();
        let logger = RuntimeLogger::new(Some(Box::new(file)), Some(path), None);
        emit(&logger, Level::Error, "driver", "summary collision fixture");
        let snapshot = logger.finish();
        assert!(snapshot.summary_path.is_none());
        assert!(
            snapshot
                .summary_write_error
                .unwrap()
                .contains("not saved completely")
        );
        assert_eq!(snapshot.error_count, 1);
        assert_eq!(std::fs::read(blocked).unwrap(), b"owned collision sentinel");
    }
}
