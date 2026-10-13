//! Exclusive publication and content-owned deletion for the native engine.
//! Every ancestor remains pinned and the hash/delete use the same file handle.
//! This protects ordinary filesystem races; it is not a sandbox for hostile
//! software running as the same user and able to alter this process.

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    ffi::c_void,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::windows::io::{AsRawHandle, FromRawHandle},
    path::{Path, PathBuf},
    ptr,
};

const READ: u32 = 0x8000_0000;
const WRITE: u32 = 0x4000_0000;
const DELETE: u32 = 0x0001_0000;
const SHARE_READ: u32 = 1;
const DIRECTORY: u32 = 0x10;
const REPARSE: u32 = 0x400;
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
type Handle = *mut c_void;

#[derive(Debug)]
pub struct CleanupIncomplete {
    operation: anyhow::Error,
    cleanup: anyhow::Error,
}
impl std::fmt::Display for CleanupIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "publication failed: {:#}; partial output cleanup failed: {:#}",
            self.operation, self.cleanup
        )
    }
}
impl std::error::Error for CleanupIncomplete {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.operation.as_ref())
    }
}

#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}
#[repr(C)]
#[derive(Default)]
struct Information {
    attributes: u32,
    creation: FileTime,
    access: FileTime,
    write: FileTime,
    volume: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}
#[repr(C)]
struct Disposition {
    delete_file: u8,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const c_void,
        creation: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn GetFileInformationByHandle(file: Handle, information: *mut Information) -> i32;
    fn SetFileInformationByHandle(file: Handle, class: u32, data: *const c_void, size: u32) -> i32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn CreateDirectoryW(name: *const u16, security: *const c_void) -> i32;
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn checked_path(text: &str) -> Result<PathBuf> {
    ensure!(
        text.encode_utf16().count() < 32_760 && !text.chars().any(char::is_control),
        "invalid path length or control character"
    );
    let text = text.replace('/', "\\");
    let bytes = text.as_bytes();
    ensure!(
        bytes.len() > 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\',
        "expected an absolute local file path"
    );
    for part in text[3..].split('\\') {
        ensure!(
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with(['.', ' '])
                && !part.contains([':', '*', '?', '"', '<', '>', '|']),
            "ambiguous path component"
        );
        let stem = part.split('.').next().unwrap().to_ascii_uppercase();
        let numbered_device = (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && matches!(stem.as_bytes()[3], b'1'..=b'9');
        ensure!(
            !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") && !numbered_device,
            "reserved Windows path component"
        );
    }
    ensure!(
        matches!(
            unsafe { GetDriveTypeW(wide(&text[..3]).as_ptr()) },
            2 | 3 | 5 | 6
        ),
        "file must be on a local disk"
    );
    Ok(PathBuf::from(text))
}

fn open(path: &Path, access: u32, creation: u32, flags: u32) -> Result<File> {
    let name = wide(&format!(
        r"\\?\{}",
        path.to_str().context("non-Unicode path")?
    ));
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            SHARE_READ,
            ptr::null(),
            creation,
            flags | OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == -1isize as Handle {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("open {}", path.display()));
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn information(file: &File) -> Result<Information> {
    let mut result = Information::default();
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut result) } != 0,
        "inspect file: {}",
        std::io::Error::last_os_error()
    );
    Ok(result)
}

fn regular(file: &File) -> Result<()> {
    let info = information(file)?;
    ensure!(
        info.attributes & (DIRECTORY | REPARSE) == 0 && info.links == 1,
        "expected a plain file with one hard link"
    );
    Ok(())
}

fn pin_parents(path: &Path) -> Result<Vec<File>> {
    let mut ancestors: Vec<_> = path.parent().context("file parent")?.ancestors().collect();
    ancestors.reverse();
    let mut pins = Vec::with_capacity(ancestors.len());
    for parent in ancestors {
        // LIST_DIRECTORY is required: attribute-only handles do not enforce
        // the no-WRITE/no-DELETE sharing constraint on Windows.
        let file = open(parent, 1 | 0x80, 3, 0x0200_0000)?;
        let info = information(&file)?;
        ensure!(
            info.attributes & DIRECTORY != 0 && info.attributes & REPARSE == 0,
            "parent must be a plain directory"
        );
        pins.push(file);
    }
    Ok(pins)
}

fn hash(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn validate_hash(expected: &str) -> Result<()> {
    ensure!(
        expected.len() == 64 && expected.bytes().all(|v| v.is_ascii_hexdigit()),
        "expected a SHA256 digest"
    );
    Ok(())
}

fn mark_deleted(file: &File) -> Result<()> {
    let disposition = Disposition { delete_file: 1 };
    ensure!(
        unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                4,
                &disposition as *const _ as *const c_void,
                std::mem::size_of::<Disposition>() as u32,
            )
        } != 0,
        "delete opened file: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|v| matches!(v.raw_os_error(), Some(2 | 3)))
}

/// An existing directory is a collision, including an empty one.
pub fn create_directory(path: &str) -> Result<()> {
    let path = checked_path(path)?;
    let _parents = pin_parents(&path)?;
    let name = wide(&format!(r"\\?\{}", path.to_str().unwrap()));
    ensure!(
        unsafe { CreateDirectoryW(name.as_ptr(), ptr::null()) } != 0,
        "create new owned directory: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

/// Remove only an empty plain directory through a pinned, non-shareable handle.
/// Failure (including a nonempty directory) never expands to recursive removal.
pub fn remove_empty_directory(path: &str) -> Result<()> {
    let path = checked_path(path)?;
    let _parents = pin_parents(&path)?;
    let file = open(&path, 1 | 0x80 | DELETE, 3, 0x0200_0000)?;
    let info = information(&file)?;
    ensure!(
        info.attributes & DIRECTORY != 0 && info.attributes & REPARSE == 0,
        "expected a plain directory for empty cleanup"
    );
    mark_deleted(&file)
}

fn directory_fingerprint(info: &Information) -> String {
    let bytes: Vec<u8> = [
        info.volume,
        info.index_high,
        info.index_low,
        info.creation.high,
        info.creation.low,
    ]
    .into_iter()
    .flat_map(u32::to_le_bytes)
    .collect();
    hex::encode(Sha256::digest(bytes))
}
pub(crate) fn directory_identity(path: &str) -> Result<String> {
    let path = checked_path(path)?;
    let _parents = pin_parents(&path)?;
    let file = open(&path, 1 | 0x80, 3, 0x0200_0000)?;
    let info = information(&file)?;
    ensure!(
        info.attributes & DIRECTORY != 0 && info.attributes & REPARSE == 0,
        "expected a plain directory for identity"
    );
    Ok(directory_fingerprint(&info))
}
/// Compare volume, file ID and creation time on the very handle being deleted.
/// An external replacement, or a nonempty directory, is retained.
pub(crate) fn remove_empty_directory_owned(path: &str, identity: &str) -> Result<()> {
    validate_hash(identity)?;
    let path = checked_path(path)?;
    let _parents = pin_parents(&path)?;
    let file = open(&path, 1 | 0x80 | DELETE, 3, 0x0200_0000)?;
    let info = information(&file)?;
    ensure!(
        info.attributes & DIRECTORY != 0
            && info.attributes & REPARSE == 0
            && directory_fingerprint(&info) == identity,
        "directory identity changed; preserve it"
    );
    mark_deleted(&file)
}

/// Copies an immutable source to a CREATE_NEW destination. Optional expected
/// hash is checked before creating anything. The returned digest describes
/// bytes created by this call, not a later pathname lookup.
pub fn publish(source: &str, destination: &str, expected: &str) -> Result<String> {
    if !expected.is_empty() {
        validate_hash(expected)?;
    }
    let source = checked_path(source)?;
    let destination = checked_path(destination)?;
    let _source_parents = pin_parents(&source)?;
    let _destination_parents = pin_parents(&destination)?;
    let mut input = open(&source, READ, 3, 0)?;
    regular(&input)?;
    let digest = hash(&mut input)?;
    ensure!(
        expected.is_empty() || digest.eq_ignore_ascii_case(expected),
        "source hash mismatch"
    );
    input.seek(SeekFrom::Start(0))?;
    let mut output = open(&destination, READ | WRITE | DELETE, 1, 0)?;
    let result = (|| {
        regular(&output)?;
        std::io::copy(&mut input, &mut output)?;
        output.flush()?;
        output.sync_all()?;
        Ok(digest)
    })();
    match result {
        Ok(digest) => Ok(digest),
        Err(error) => {
            // A retained partial output has no successful creation digest.
            // Keep the transaction marker if that output cannot be removed.
            if let Err(cleanup) = mark_deleted(&output) {
                return Err(CleanupIncomplete {
                    operation: error,
                    cleanup,
                }
                .into());
            }
            Err(error)
        }
    }
}

/// Hashes and marks for deletion through one handle denying write and delete
/// sharing. A modified file is deliberately retained; missing is idempotent.
pub fn delete(path: &str, expected: &str) -> Result<&'static str> {
    validate_hash(expected)?;
    let path = checked_path(path)?;
    let _parents = match pin_parents(&path) {
        Ok(value) => value,
        Err(error) if missing(&error) => return Ok("missing"),
        Err(error) => return Err(error),
    };
    let mut file = match open(&path, READ | DELETE, 3, 0) {
        Ok(value) => value,
        Err(error) if missing(&error) => return Ok("missing"),
        Err(error) => return Err(error),
    };
    regular(&file)?;
    if !hash(&mut file)?.eq_ignore_ascii_case(expected) {
        return Ok("preserved");
    }
    mark_deleted(&file)?;
    Ok("deleted")
}

pub fn hash_file(path: &str) -> Result<String> {
    let path = checked_path(path)?;
    let _parents = pin_parents(&path)?;
    let mut file = open(&path, READ, 3, 0)?;
    regular(&file)?;
    hash(&mut file)
}

pub fn read_bytes(path: &str, limit: usize) -> Result<Vec<u8>> {
    let path = checked_path(path)?;
    let _parents = pin_parents(&path)?;
    let file = open(&path, READ, 3, 0)?;
    regular(&file)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "transaction input exceeds its size limit"
    );
    Ok(bytes)
}

pub fn write_new_bytes(path: &str, bytes: &[u8]) -> Result<String> {
    let path = checked_path(path)?;
    let _parents = pin_parents(&path)?;
    let mut file = open(&path, READ | WRITE | DELETE, 1, 0)?;
    let result = (|| {
        regular(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(hex::encode(Sha256::digest(bytes)))
    })();
    match result {
        Ok(hash) => Ok(hash),
        Err(error) => match mark_deleted(&file) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(CleanupIncomplete {
                operation: error,
                cleanup,
            }
            .into()),
        },
    }
}

/// Append one already bounded record, flushing before the associated mutation.
/// Readers reject an incomplete final record after abrupt termination.
pub(crate) fn append_durable_bytes(path: &str, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= 128 * 1024 && bytes.ends_with(b"\n"),
        "invalid durable record"
    );
    let path = checked_path(path)?;
    let _parents = pin_parents(&path)?;
    let mut file = open(
        &path,
        READ | WRITE,
        if path.try_exists()? { 3 } else { 1 },
        0,
    )?;
    regular(&file)?;
    ensure!(
        file.metadata()?.len() < 32 * 1024 * 1024,
        "durable evidence size limit reached"
    );
    file.seek(SeekFrom::End(0))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn append_upgrade_journal(
    path: &str,
    task: &str,
    event: &str,
    resource: &str,
    digest: &str,
    result: &str,
) -> Result<()> {
    ensure!(
        !task.is_empty()
            && task.len() <= 128
            && task
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
        "invalid transaction task identity"
    );
    ensure!(
        !event.is_empty()
            && event.len() <= 64
            && event.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
        "invalid transaction event"
    );
    ensure!(
        digest.is_empty() || validate_hash(digest).is_ok(),
        "invalid transaction digest"
    );
    let path = checked_path(path)?;
    let _parents = pin_parents(&path)?;
    let mut file = open(&path, READ | WRITE, if event == "begin" { 1 } else { 3 }, 0)?;
    regular(&file)?;
    file.seek(SeekFrom::End(0))?;
    let mut bytes = serde_json::to_vec(
        &serde_json::json!({"schema_version":1,"task_id":task,"event":event,"path":resource,"sha256":digest,"result":result,
        "time_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis()}),
    )?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU32, Ordering},
    };
    static NEXT: AtomicU32 = AtomicU32::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "avelin-owned-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn name(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().into()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for name in [
                "source",
                "target",
                "hardlink",
                "candidate.json",
                "receipt.json",
                "journal.jsonl",
            ] {
                let _ = fs::remove_file(self.0.join(name));
            }
            let _ = fs::remove_dir(&self.0);
        }
    }
    #[test]
    fn recovery_directory_cleanup_refuses_a_different_opened_directory() {
        let fixture = Fixture::new();
        let original = fixture.0.join("original");
        let replacement = fixture.0.join("replacement");
        create_directory(original.to_str().unwrap()).unwrap();
        create_directory(replacement.to_str().unwrap()).unwrap();
        let identity = directory_identity(original.to_str().unwrap()).unwrap();
        assert!(remove_empty_directory_owned(replacement.to_str().unwrap(), &identity).is_err());
        assert!(replacement.is_dir());
        remove_empty_directory_owned(original.to_str().unwrap(), &identity).unwrap();
        remove_empty_directory(replacement.to_str().unwrap()).unwrap();
    }
    #[test]
    fn publication_refuses_existing_destination_and_wrong_source_hash() {
        let f = Fixture::new();
        fs::write(f.name("source"), b"new").unwrap();
        fs::write(f.name("target"), b"user data").unwrap();
        assert!(publish(&f.name("source"), &f.name("target"), "").is_err());
        assert_eq!(fs::read(f.name("target")).unwrap(), b"user data");
        assert!(publish(&f.name("source"), &f.name("hardlink"), &"0".repeat(64)).is_err());
        assert!(!Path::new(&f.name("hardlink")).exists());
    }
    #[test]
    fn deletion_preserves_modified_files_and_removes_only_matching_bytes() {
        let f = Fixture::new();
        fs::write(f.name("source"), b"original").unwrap();
        let digest = publish(&f.name("source"), &f.name("target"), "").unwrap();
        assert_eq!(digest, hex::encode(Sha256::digest(b"original")));
        fs::write(f.name("target"), b"modified").unwrap();
        assert_eq!(delete(&f.name("target"), &digest).unwrap(), "preserved");
        assert_eq!(fs::read(f.name("target")).unwrap(), b"modified");
        fs::write(f.name("target"), b"original").unwrap();
        assert_eq!(delete(&f.name("target"), &digest).unwrap(), "deleted");
        assert_eq!(delete(&f.name("target"), &digest).unwrap(), "missing");
    }
    #[test]
    fn pinned_parent_and_open_file_block_rename_and_write() {
        let f = Fixture::new();
        fs::write(f.name("source"), b"original").unwrap();
        let path = checked_path(&f.name("source")).unwrap();
        let pins = pin_parents(&path).unwrap();
        let file = open(&path, READ | DELETE, 3, 0).unwrap();
        assert!(fs::write(&path, b"replacement").is_err());
        assert!(fs::rename(&path, f.name("target")).is_err());
        assert!(fs::rename(&f.0, f.0.with_extension("moved")).is_err());
        drop(file);
        drop(pins);
        assert_eq!(fs::read(path).unwrap(), b"original");
    }
    #[test]
    fn rejects_hardlinks_and_ambiguous_paths() {
        let f = Fixture::new();
        fs::write(f.name("source"), b"original").unwrap();
        fs::hard_link(f.name("source"), f.name("hardlink")).unwrap();
        let digest = hex::encode(Sha256::digest(b"original"));
        assert!(delete(&f.name("source"), &digest).is_err());
        for path in [
            r"C:\a\..\b",
            r"C:\a\file:stream",
            r"C:\a\NUL.txt",
            r"\\server\share\file",
            r"C:\a\file.",
        ] {
            assert!(checked_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn private_directory_creation_refuses_existing_empty_directory() {
        let f = Fixture::new();
        let path = f.name("internal");
        create_directory(&path).unwrap();
        assert!(create_directory(&path).is_err());
        assert!(Path::new(&path).is_dir());
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn upgrade_journal_requires_new_begin_and_appends_complete_task_records() {
        let f = Fixture::new();
        let path = f.name("journal.jsonl");
        assert!(
            append_upgrade_journal(&path, "task", "published", "C:\\app", "", "created").is_err()
        );
        append_upgrade_journal(&path, "task", "begin", "C:\\app", "", "preparing").unwrap();
        assert!(
            append_upgrade_journal(&path, "task", "begin", "C:\\app", "", "preparing").is_err()
        );
        append_upgrade_journal(
            &path,
            "task",
            "committed",
            "C:\\app",
            &"a".repeat(64),
            "verified",
        )
        .unwrap();
        let content = fs::read_to_string(path).unwrap();
        assert!(content.ends_with('\n'));
        let records: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["task_id"], "task");
        assert_eq!(records[1]["event"], "committed");
    }
}
