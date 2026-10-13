//! Windows files that remain fixed while the coordinator verifies and uses them.
//!
//! A checked pathname alone is not a capability: a parent can be renamed, or a
//! checked file can be replaced before CreateProcess opens it. Keep read handles
//! to the file and every directory from the drive root until use is finished.
//! These guards are not a signature, and do not isolate mutually hostile code
//! running as the same Windows user (which also controls this process).

use anyhow::{Context, Result, ensure};
use avelin_core::hash_bytes;
use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr,
    sync::Mutex,
};

type RawHandle = *mut c_void;
const INVALID_HANDLE: RawHandle = -1isize as RawHandle;
const FILE_LIST_DIRECTORY: u32 = 1;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_SHARE_READ: u32 = 1;
const OPEN_EXISTING: u32 = 3;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}

#[repr(C)]
#[derive(Default)]
struct FileInformation {
    attributes: u32,
    creation: FileTime,
    access: FileTime,
    write: FileTime,
    volume_serial: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}

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
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const c_void,
        creation: u32,
        flags: u32,
        template: RawHandle,
    ) -> RawHandle;
    fn GetFileInformationByHandle(file: RawHandle, information: *mut FileInformation) -> i32;
    fn CreateDirectoryW(name: *const u16, security: *const SecurityAttributes) -> i32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetCurrentProcess() -> RawHandle;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
    fn SetDefaultDllDirectories(flags: u32) -> i32;
    fn SetDllDirectoryW(directory: *const u16) -> i32;
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

struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// Validate the raw spelling before Path::components can remove dot segments.
fn checked_path(path: &Path) -> Result<PathBuf> {
    let text = path.to_str().context("execution path must be Unicode")?;
    ensure!(
        text.encode_utf16().count() < 32_760 && !text.chars().any(char::is_control),
        "invalid execution path length or control character"
    );
    let mut text = text.replace('/', "\\");
    let bytes = text.as_bytes();
    ensure!(
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\',
        "execution path must be an absolute local drive path"
    );
    // GetTempPathW/Rust temp_dir normally include one final separator. It is
    // not a dot segment or an extra path component; preserve the drive root.
    if text.len() > 3 && text.ends_with('\\') && !text[..text.len() - 1].ends_with('\\') {
        text.pop();
    }
    if text.len() > 3 {
        for component in text[3..].split('\\') {
            checked_name(component)?;
        }
    }
    let root = wide(&text[..3]);
    // Reject network-mapped drives as well as UNC/device namespace spellings.
    ensure!(
        matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2 | 3 | 5 | 6),
        "execution path must be on a local disk"
    );
    Ok(PathBuf::from(text))
}

fn checked_name(name: &str) -> Result<()> {
    ensure!(
        !name.contains(['/', '\\']) && avelin_core::relative_path(name).is_ok(),
        "execution file name must be one unambiguous Windows component"
    );
    Ok(())
}

fn native_path(path: &Path) -> Vec<u16> {
    // Only called after checked_path, so extended spelling cannot introduce
    // device names, dot segments, alternate streams, or a remote namespace.
    wide(&format!(
        r"\\?\{}",
        path.to_str().expect("validated Unicode path")
    ))
}

fn information(file: &File) -> Result<FileInformation> {
    let mut value = FileInformation::default();
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut value) } != 0,
        "inspect opened execution file: {}",
        std::io::Error::last_os_error()
    );
    Ok(value)
}

fn ensure_regular_file(file: &File) -> Result<()> {
    let value = information(file)?;
    ensure!(
        value.attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) == 0,
        "execution file is a directory or reparse point"
    );
    ensure!(
        value.links == 1,
        "execution file must have exactly one hard link"
    );
    Ok(())
}

fn open_directory(path: &Path) -> Result<File> {
    let name = native_path(path);
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            // Attribute-only handles do not enforce data sharing constraints
            // on Windows. LIST_DIRECTORY is the minimum data-read access that
            // makes the no-WRITE/no-DELETE share mode protect this directory.
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            // Do not share DELETE (rename) or WRITE (reparse conversion).
            // Creating ordinary child files does not need a data-write handle
            // to this directory and remains possible.
            FILE_SHARE_READ,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("pin execution directory {}", path.display()));
    }
    let file = unsafe { File::from_raw_handle(raw) };
    let value = information(&file)?;
    ensure!(
        value.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
            && value.attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "execution directory is not a plain directory: {}",
        path.display()
    );
    Ok(file)
}

/// Retains the directory itself and every ancestor up to the drive root.
pub struct PinnedDirectory {
    path: PathBuf,
    _ancestors: Vec<File>,
}

impl PinnedDirectory {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_or_create(path, None)
    }

    fn open_or_create(path: &Path, security: Option<&SecurityAttributes>) -> Result<Self> {
        let path = checked_path(path)?;
        let mut prefixes: Vec<_> = path.ancestors().map(Path::to_path_buf).collect();
        prefixes.reverse();
        let mut ancestors = Vec::with_capacity(prefixes.len());
        for prefix in prefixes {
            match open_directory(&prefix) {
                Ok(file) => ancestors.push(file),
                Err(error)
                    if security.is_some()
                        && error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|e| matches!(e.raw_os_error(), Some(2 | 3))) =>
                {
                    let name = native_path(&prefix);
                    if unsafe { CreateDirectoryW(name.as_ptr(), security.unwrap()) } == 0 {
                        let error = std::io::Error::last_os_error();
                        // A concurrent creator may win the parent race. It is
                        // still opened without following reparse points below.
                        if error.raw_os_error() != Some(183) {
                            return Err(error).context("create execution parent directory");
                        }
                    }
                    ancestors.push(open_directory(&prefix)?);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            path,
            _ancestors: ancestors,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn current_user_sid() -> Result<String> {
    let mut raw = ptr::null_mut();
    ensure!(
        unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut raw) } != 0,
        "open current user token: {}",
        std::io::Error::last_os_error()
    );
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut required = 0;
    unsafe { GetTokenInformation(token.as_raw_handle(), 1, ptr::null_mut(), 0, &mut required) };
    ensure!(
        required >= std::mem::size_of::<SidAndAttributes>() as u32,
        "invalid token user size"
    );
    // usize backing storage supplies the alignment TOKEN_USER requires.
    let mut buffer = vec![0usize; (required as usize).div_ceil(std::mem::size_of::<usize>())];
    ensure!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                1,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } != 0,
        "read current user token: {}",
        std::io::Error::last_os_error()
    );
    let user = unsafe { &*buffer.as_ptr().cast::<SidAndAttributes>() };
    let mut text = ptr::null_mut();
    ensure!(
        unsafe { ConvertSidToStringSidW(user.sid, &mut text) } != 0,
        "convert current user SID: {}",
        std::io::Error::last_os_error()
    );
    let _memory = LocalMemory(text.cast());
    let mut length = 0;
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    Ok(String::from_utf16(unsafe {
        std::slice::from_raw_parts(text, length)
    })?)
}

fn private_security() -> Result<LocalMemory> {
    let sid = current_user_sid()?;
    // Owner=current process user; protected DACL; only that SID receives full
    // access, inherited by files and subdirectories. No inherited Users ACE.
    let sddl = wide(&format!("O:{sid}D:P(A;OICI;FA;;;{sid})"));
    let mut descriptor = ptr::null_mut();
    ensure!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } != 0,
        "construct private execution directory DACL: {}",
        std::io::Error::last_os_error()
    );
    Ok(LocalMemory(descriptor))
}

/// Creates a unique directory with a private, inheritable DACL in the same
/// CreateDirectoryW call. Missing base ancestors are created and pinned in order.
/// Existing directories are never rewritten and a UUID collision fails closed.
pub fn create_private_session(base: &Path, prefix: &str) -> Result<PinnedDirectory> {
    ensure!(
        !prefix.is_empty()
            && prefix.len() <= 80
            && prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
        "invalid execution session prefix"
    );
    let security = private_security()?;
    let attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor: security.0,
        inherit: 0,
    };
    let mut directory = PinnedDirectory::open_or_create(base, Some(&attributes))?;
    let path = checked_path(
        &directory
            .path
            .join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple())),
    )?;
    let name = native_path(&path);
    ensure!(
        unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } != 0,
        "create private execution directory: {}",
        std::io::Error::last_os_error()
    );
    // Keep the base handles throughout creation and while the new child opens.
    directory._ancestors.push(open_directory(&path)?);
    directory.path = path;
    Ok(directory)
}

/// A read-only, non-inherited handle that prevents write/delete sharing.
/// Drop this guard before intentionally deleting the file or its ancestors.
pub struct PinnedFile {
    path: PathBuf,
    file: Mutex<File>,
    _parent: PinnedDirectory,
}

impl PinnedFile {
    pub fn open(path: &Path) -> Result<Self> {
        let path = checked_path(path)?;
        let parent = PinnedDirectory::open(path.parent().context("execution file has no parent")?)?;
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
            .with_context(|| format!("pin execution file {}", path.display()))?;
        ensure_regular_file(&file)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
            _parent: parent,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_bytes(&self) -> Result<Vec<u8>> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| anyhow::anyhow!("execution file lock poisoned"))?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        let read = file.read_to_end(&mut bytes);
        // All reads (including hashes/copies) use this handle. Rewind even when
        // the read fails so a later caller never inherits a partial cursor.
        let rewind = file.seek(SeekFrom::Start(0));
        read.context("read pinned execution file")?;
        rewind.context("rewind pinned execution file")?;
        Ok(bytes)
    }

    pub fn sha256(&self) -> Result<String> {
        Ok(hash_bytes(&self.read_bytes()?))
    }
}

/// New files only. Verification happens after reopening the exclusive writer as
/// a pinned reader; a replacement in that transition cannot bypass the hash.
pub fn create_file(directory: &PinnedDirectory, name: &str, bytes: &[u8]) -> Result<PinnedFile> {
    checked_name(name)?;
    let path = checked_path(&directory.path().join(name))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)
        .with_context(|| format!("create new execution file {}", path.display()))?;
    ensure_regular_file(&file)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let pinned = PinnedFile::open(&path)?;
    ensure!(
        pinned.sha256()? == hash_bytes(bytes),
        "created execution file hash mismatch"
    );
    Ok(pinned)
}

/// Copy from the already pinned source handle, never by reopening its pathname.
pub fn copy_file(
    directory: &PinnedDirectory,
    name: &str,
    source: &PinnedFile,
) -> Result<PinnedFile> {
    create_file(directory, name, &source.read_bytes()?)
}

/// Exclusive in-place update of a verified existing record. Errors can leave a
/// partial record; the caller must preserve that scene and report Unknown.
pub fn update_existing_file(
    directory: &PinnedDirectory,
    name: &str,
    expected_sha256: &str,
    bytes: &[u8],
) -> Result<()> {
    checked_name(name)?;
    ensure!(
        expected_sha256.len() == 64 && expected_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid expected execution record hash"
    );
    let path = checked_path(&directory.path().join(name))?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)
        .with_context(|| format!("open existing execution record {}", path.display()))?;
    ensure_regular_file(&file)?;
    let mut original = Vec::new();
    file.read_to_end(&mut original)?;
    ensure!(
        hash_bytes(&original).eq_ignore_ascii_case(expected_sha256),
        "existing execution record hash mismatch"
    );
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.set_len(bytes.len() as u64)?;
    file.sync_all()?;
    Ok(())
}

/// Constrain subsequent default dynamic DLL loads to the executable directory
/// and System32. This runs after Windows has resolved the initial import table;
/// it cannot retroactively authenticate those initial DLLs or other processes.
pub fn initialize_dll_search() -> Result<()> {
    const APPLICATION_DIR: u32 = 0x200;
    const SYSTEM32: u32 = 0x800;
    ensure!(
        unsafe { SetDefaultDllDirectories(APPLICATION_DIR | SYSTEM32) } != 0,
        "set DLL search directories: {}",
        std::io::Error::last_os_error()
    );
    // An empty value explicitly removes the current working directory from
    // legacy search; a null value would restore the default and is not used.
    ensure!(
        unsafe { SetDllDirectoryW([0u16].as_ptr()) } != 0,
        "remove current directory from DLL search: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    /// Each test owns a new private UUID directory. Cleanup is an explicit list
    /// of files and empty directories we created; no recursive deletion is used.
    struct Fixture {
        root: PathBuf,
        files: Vec<PathBuf>,
        directories: Vec<PathBuf>,
    }
    impl Fixture {
        fn new() -> Self {
            let guard = create_private_session(&std::env::temp_dir(), "avelin-file-test").unwrap();
            Self {
                root: guard.path().to_path_buf(),
                files: Vec::new(),
                directories: Vec::new(),
            }
        }
        fn file(&mut self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.root.join(name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            file.write_all(bytes).unwrap();
            self.files.push(path.clone());
            path
        }
        fn directory(&mut self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::create_dir(&path).unwrap();
            self.directories.push(path.clone());
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for file in self.files.iter().rev() {
                let _ = fs::remove_file(file);
            }
            for directory in self.directories.iter().rev() {
                let _ = fs::remove_dir(directory);
            }
            let _ = fs::remove_dir(&self.root);
        }
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn DeviceIoControl(
            device: RawHandle,
            control: u32,
            input: *const c_void,
            input_size: u32,
            output: *mut c_void,
            output_size: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn LoadLibraryW(name: *const u16) -> RawHandle;
        fn FreeLibrary(module: RawHandle) -> i32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetSecurityInfo(
            handle: RawHandle,
            object_type: u32,
            requested: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn GetSecurityDescriptorControl(
            descriptor: *const c_void,
            control: *mut u16,
            revision: *mut u32,
        ) -> i32;
        fn GetAce(acl: *const c_void, index: u32, ace: *mut *mut c_void) -> i32;
    }

    #[repr(C)]
    struct Acl {
        revision: u8,
        reserved: u8,
        size: u16,
        count: u16,
        reserved2: u16,
    }

    #[derive(Debug)]
    struct Access {
        protected: bool,
        owner: String,
        entries: Vec<(u8, u32, String)>,
    }

    fn sid_text(sid: *const c_void) -> String {
        let mut text = ptr::null_mut();
        assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut text) }, 0);
        let _memory = LocalMemory(text.cast());
        let mut len = 0;
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) }).unwrap()
    }

    fn access(path: &Path) -> Access {
        let name = native_path(path);
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                0x0002_0000,
                FILE_SHARE_READ,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        assert_ne!(
            raw,
            INVALID_HANDLE,
            "open READ_CONTROL: {}",
            std::io::Error::last_os_error()
        );
        let file = unsafe { File::from_raw_handle(raw) };
        let (mut owner, mut dacl, mut descriptor) =
            (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
        assert_eq!(
            unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    1,
                    5,
                    &mut owner,
                    ptr::null_mut(),
                    &mut dacl,
                    ptr::null_mut(),
                    &mut descriptor,
                )
            },
            0
        );
        let _memory = LocalMemory(descriptor);
        let (mut control, mut revision) = (0, 0);
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
            0
        );
        assert!(!dacl.is_null(), "a null DACL grants everyone access");
        let acl = unsafe { &*dacl.cast::<Acl>() };
        let mut entries = Vec::new();
        for index in 0..acl.count {
            let mut ace = ptr::null_mut();
            assert_ne!(unsafe { GetAce(dacl, index.into(), &mut ace) }, 0);
            let ace = ace.cast::<u8>();
            assert_eq!(unsafe { *ace }, 0, "only ACCESS_ALLOWED_ACE expected");
            let flags = unsafe { *ace.add(1) };
            let mask = unsafe { ptr::read_unaligned(ace.add(4).cast::<u32>()) };
            let sid = sid_text(unsafe { ace.add(8).cast() });
            entries.push((flags, mask, sid));
        }
        Access {
            protected: control & 0x1000 != 0,
            owner: sid_text(owner),
            entries,
        }
    }

    fn rename_is_blocked(path: &Path) {
        let destination = path.with_file_name(format!("rename-{}", uuid::Uuid::new_v4().simple()));
        let result = fs::rename(path, &destination);
        // Restore immediately if the assertion is about to expose a defect.
        if result.is_ok() {
            fs::rename(&destination, path).unwrap();
        }
        assert!(
            result.is_err(),
            "pinned path was renamed: {}",
            path.display()
        );
    }

    #[test]
    fn pins_all_directory_ancestors_against_rename_and_reparse_writes() {
        let mut fixture = Fixture::new();
        let parent = fixture.directory("parent");
        let child = fixture.directory("parent\\child");
        let guard = PinnedDirectory::open(&child).unwrap();
        rename_is_blocked(&child);
        rename_is_blocked(&parent);
        rename_is_blocked(&fixture.root);
        let name = native_path(&child);
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                0x4000_0000,
                7,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if raw != INVALID_HANDLE {
            drop(unsafe { File::from_raw_handle(raw) });
        }
        assert_eq!(
            raw, INVALID_HANDLE,
            "pinned directory permits a data-write/reparse handle"
        );
        drop(guard);
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                0x4000_0000,
                7,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        assert_ne!(
            raw, INVALID_HANDLE,
            "test user should otherwise have write permission"
        );
        drop(unsafe { File::from_raw_handle(raw) });
    }

    #[test]
    fn pinned_file_blocks_writes_replacements_and_reads_same_bytes_repeatedly() {
        let mut fixture = Fixture::new();
        let path = fixture.file("backend.exe", b"pinned bytes");
        let file = PinnedFile::open(&path).unwrap();
        assert_eq!(file.path(), path);
        assert!(fs::write(&path, b"replacement").is_err());
        assert!(fs::remove_file(&path).is_err());
        rename_is_blocked(&path);
        rename_is_blocked(&fixture.root);
        for _ in 0..3 {
            assert_eq!(file.sha256().unwrap(), hash_bytes(b"pinned bytes"));
            assert_eq!(file.read_bytes().unwrap(), b"pinned bytes");
        }
        drop(file);
        fs::write(&path, b"writable after release").unwrap();
    }

    #[test]
    fn session_is_atomically_private_and_children_inherit_only_current_sid() {
        let mut fixture = Fixture::new();
        let base = fixture.root.join("new-parent\\nested");
        let directory = create_private_session(&base, "session").unwrap();
        fixture.directories.extend([
            fixture.root.join("new-parent"),
            base,
            directory.path().to_path_buf(),
        ]);
        let sid = current_user_sid().unwrap();
        let acl = access(directory.path());
        assert!(acl.protected);
        assert_eq!(acl.owner, sid);
        assert_eq!(acl.entries, vec![(3, 0x001f_01ff, sid.clone())]);
        let child = create_file(&directory, "payload.bin", b"data").unwrap();
        fixture.files.push(child.path().to_path_buf());
        let inherited = access(child.path());
        assert_eq!(inherited.entries.len(), 1);
        assert_eq!(inherited.entries[0].2, sid);
        assert_eq!(inherited.entries[0].1, 0x001f_01ff);
        assert_ne!(
            inherited.entries[0].0 & 0x10,
            0,
            "child should inherit the private ACE"
        );
    }

    #[test]
    fn creates_only_new_files_and_copies_pinned_source_handle() {
        let mut fixture = Fixture::new();
        let source_path = fixture.file("source.bin", b"source content");
        let directory = PinnedDirectory::open(&fixture.root).unwrap();
        let source = PinnedFile::open(&source_path).unwrap();
        assert!(create_file(&directory, "source.bin", b"overwrite").is_err());
        let copied = copy_file(&directory, "copy.bin", &source).unwrap();
        fixture.files.push(copied.path().to_path_buf());
        assert_eq!(copied.sha256().unwrap(), source.sha256().unwrap());
        assert_eq!(copied.read_bytes().unwrap(), b"source content");
        assert!(fs::write(copied.path(), b"replace copy").is_err());
        assert_eq!(source.read_bytes().unwrap(), b"source content");
        assert!(create_file(&directory, "..\\escape.bin", b"x").is_err());
    }

    #[test]
    fn rejects_hard_links_for_pins_and_existing_record_updates() {
        let mut fixture = Fixture::new();
        let original = fixture.file("original.bin", b"linked");
        let alias = fixture.root.join("alias.bin");
        fs::hard_link(&original, &alias).unwrap();
        fixture.files.push(alias.clone());
        assert!(PinnedFile::open(&original).is_err());
        assert!(PinnedFile::open(&alias).is_err());
        let directory = PinnedDirectory::open(&fixture.root).unwrap();
        assert!(
            update_existing_file(
                &directory,
                "original.bin",
                &hash_bytes(b"linked"),
                b"updated"
            )
            .is_err()
        );
        assert_eq!(fs::read(&original).unwrap(), b"linked");
    }

    #[test]
    fn existing_record_update_checks_old_hash_and_preserves_acl() {
        let mut fixture = Fixture::new();
        let path = fixture.file("receipt.json", b"previous longer bytes");
        let directory = PinnedDirectory::open(&fixture.root).unwrap();
        let old_acl = access(&path);
        assert!(
            update_existing_file(
                &directory,
                "receipt.json",
                &hash_bytes(b"different"),
                b"new"
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"previous longer bytes");
        let pin = PinnedFile::open(&path).unwrap();
        assert!(
            update_existing_file(&directory, "receipt.json", &pin.sha256().unwrap(), b"new")
                .is_err()
        );
        drop(pin);
        update_existing_file(
            &directory,
            "receipt.json",
            &hash_bytes(b"previous longer bytes"),
            b"new",
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let new_acl = access(&path);
        assert_eq!(new_acl.owner, old_acl.owner);
        assert_eq!(new_acl.protected, old_acl.protected);
        assert_eq!(new_acl.entries, old_acl.entries);
        assert!(update_existing_file(&directory, "absent.json", &hash_bytes(b""), b"new").is_err());
        assert!(!fixture.root.join("absent.json").exists());
    }

    fn create_junction(path: &Path, target: &Path) {
        let substitute: Vec<u16> = format!(r"\??\{}", target.display())
            .encode_utf16()
            .collect();
        let print: Vec<u16> = target.to_str().unwrap().encode_utf16().collect();
        let data_length = 8 + (substitute.len() + print.len() + 2) * 2;
        let mut buffer = Vec::new();
        buffer.extend(0xa000_0003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
        buffer.extend((data_length as u16).to_le_bytes());
        buffer.extend(0u16.to_le_bytes());
        buffer.extend(0u16.to_le_bytes());
        buffer.extend(((substitute.len() * 2) as u16).to_le_bytes());
        buffer.extend((((substitute.len() + 1) * 2) as u16).to_le_bytes());
        buffer.extend(((print.len() * 2) as u16).to_le_bytes());
        for unit in substitute
            .into_iter()
            .chain(Some(0))
            .chain(print)
            .chain(Some(0))
        {
            buffer.extend(unit.to_le_bytes());
        }
        let name = native_path(path);
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                0x4000_0000,
                7,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        assert_ne!(
            raw,
            INVALID_HANDLE,
            "open junction fixture: {}",
            std::io::Error::last_os_error()
        );
        let file = unsafe { File::from_raw_handle(raw) };
        let mut returned = 0;
        assert_ne!(
            unsafe {
                DeviceIoControl(
                    file.as_raw_handle(),
                    0x0009_00a4,
                    buffer.as_ptr().cast(),
                    buffer.len() as u32,
                    ptr::null_mut(),
                    0,
                    &mut returned,
                    ptr::null_mut(),
                )
            },
            0,
            "set isolated junction fixture: {}",
            std::io::Error::last_os_error()
        );
    }

    #[test]
    fn rejects_junctions_before_creating_or_opening_children() {
        let mut fixture = Fixture::new();
        let target = fixture.directory("actual");
        fixture.file("actual\\payload.bin", b"original");
        let junction = fixture.directory("junction");
        create_junction(&junction, &target);
        assert_eq!(
            fs::read(junction.join("payload.bin")).unwrap(),
            b"original",
            "fixture must resolve before testing rejection"
        );
        assert!(PinnedDirectory::open(&junction).is_err());
        assert!(PinnedFile::open(&junction.join("payload.bin")).is_err());
        assert!(create_private_session(&junction, "unsafe").is_err());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
    }

    #[test]
    fn rejects_ambiguous_and_remote_path_spellings() {
        for path in [
            r"relative\file",
            r"C:relative",
            r"\\server\share\file",
            r"\\?\C:\file",
            r"\\.\C:\file",
            r"C:\one\..\file",
            r"C:\one\.\file",
            r"C:\one\\file",
            r"C:\one\file:stream",
            r"C:\one\file.",
            r"C:\one\NUL",
            "C:\\one\\a\nb",
        ] {
            assert!(checked_path(Path::new(path)).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn dll_search_initialization_is_confined_to_child() {
        const MARKER: &str = "AVELIN_EXECUTION_FILES_TEST_DLL_CHILD";
        if let Some(name) = std::env::var_os(MARKER) {
            let name = name.to_str().unwrap();
            checked_name(name).unwrap();
            assert_eq!(fs::read(name).unwrap(), b"not a portable executable");
            let wide_name = wide(name);
            // Establish that the default loader sees this exact invalid DLL in
            // CWD. No valid executable code is introduced by this fixture.
            let before = unsafe { LoadLibraryW(wide_name.as_ptr()) };
            let before_error = std::io::Error::last_os_error();
            if !before.is_null() {
                unsafe { FreeLibrary(before) };
            }
            assert!(before.is_null());
            assert_eq!(before_error.raw_os_error(), Some(193));
            initialize_dll_search().unwrap();
            let after = unsafe { LoadLibraryW(wide_name.as_ptr()) };
            let after_error = std::io::Error::last_os_error();
            if !after.is_null() {
                unsafe { FreeLibrary(after) };
            }
            assert!(after.is_null());
            assert_eq!(
                after_error.raw_os_error(),
                Some(126),
                "CWD must no longer be searched"
            );
            return;
        }
        let mut fixture = Fixture::new();
        let name = format!("avelin-search-{}.dll", uuid::Uuid::new_v4().simple());
        fixture.file(&name, b"not a portable executable");
        // This module is shared by the coordinator and native worker. Libtest
        // omits the crate name but retains the embedding module's namespace.
        let test_name = format!(
            "{}::dll_search_initialization_is_confined_to_child",
            module_path!().split_once("::").unwrap().1
        );
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .env(MARKER, &name)
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child test filter did not execute the check"
        );
    }
}
