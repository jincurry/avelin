use anyhow::{Context, Result, bail, ensure};
pub mod model;
pub mod native_payload;
pub mod presentation;
pub mod protocol;
pub mod session;
pub use model::{
    FileReplacement, InstallPlan, Operation, ShortcutKind, ShortcutRecord, UpgradeDiff, UpgradePlan,
};
pub use protocol::{
    BackendEvent, BackendEvents, Progress, ProgressPhase, TaskCommand, TaskPhase, TaskResult,
    TaskSnapshot,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path},
};

pub const SCHEMA_VERSION: u32 = 1;
pub const RECEIPT_NAME: &str = ".avelin-receipt.json";
pub const MAGIC: &[u8; 16] = b"AVELIN-M0-PACK01";
pub const TRAILER_SIZE: u64 = 72;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileResource {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub app_id: String,
    pub app_name: String,
    pub app_version: String,
    pub entry: String,
    pub directory_name: String,
    #[serde(default)]
    pub start_menu_shortcut: bool,
    pub files: Vec<FileResource>,
    #[serde(default)]
    pub runtime_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<presentation::Presentation>,
    /// A package default; each task records its actual choice in InstallPlan.
    #[serde(default, skip_serializing_if = "is_false")]
    pub desktop_shortcut: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema_version: u32,
    pub app_id: String,
    pub app_version: String,
    pub installation_id: String,
    pub task_id: String,
    pub commit_id: String,
    /// The preceding successful commit, absent on pre-M3 and fresh-install receipts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_commit_id: Option<String>,
    pub install_dir: String,
    pub manifest_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uninstaller_sha256: Option<String>,
    pub user_sid: String,
    pub files: Vec<FileResource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shortcuts: Vec<ShortcutRecord>,
}

fn is_false(value: &bool) -> bool {
    !value
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("Open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut block = [0; 64 * 1024];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        hasher.update(&block[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Conservative M0 subset; reject ambiguous paths instead of changing them.
pub fn relative_path(value: &str) -> Result<String> {
    ensure!(
        !value.is_empty() && value.encode_utf16().count() < 180,
        "invalid relative path length"
    );
    let normalized = value.replace('/', "\\");
    for part in normalized.split('\\') {
        ensure!(
            !part.is_empty() && part != "." && part != "..",
            "empty/dot path component"
        );
        ensure!(
            !part.starts_with(' ') && !part.ends_with([' ', '.']),
            "leading space or trailing space/dot is outside the M0 path subset"
        );
        ensure!(
            !part
                .chars()
                .any(|c| c.is_control() || "<>:\"|?*".contains(c)),
            "invalid Windows path character"
        );
        let base = part.split('.').next().unwrap().to_ascii_uppercase();
        ensure!(
            !matches!(
                base.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
            ),
            "reserved Windows name"
        );
        ensure!(
            !(base.starts_with("COM") || base.starts_with("LPT"))
                || !matches!(base.chars().nth(3), Some('1'..='9' | '¹' | '²' | '³'))
                || base.chars().count() != 4,
            "reserved Windows device"
        );
    }
    Ok(normalized)
}

pub fn safe_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 100
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "app id must be an ASCII identifier"
    );
    Ok(())
}

#[cfg(windows)]
pub fn same_windows_name(a: &str, b: &str) -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CompareStringOrdinal(
            a: *const u16,
            alen: i32,
            b: *const u16,
            blen: i32,
            ignore: i32,
        ) -> i32;
    }
    let a: Vec<u16> = a.encode_utf16().collect();
    let b: Vec<u16> = b.encode_utf16().collect();
    unsafe { CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1) == 2 }
}
#[cfg(not(windows))]
pub fn same_windows_name(a: &str, b: &str) -> bool {
    a.to_uppercase() == b.to_uppercase()
}

pub fn reject_reparse(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "symlink rejected: {}",
                    ancestor.display()
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "reparse point rejected: {}",
                        ancestor.display()
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn install_root(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "installation path must be absolute");
    #[cfg(windows)]
    {
        use std::path::Prefix;
        ensure!(
            matches!(path.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(),Prefix::Disk(_))),
            "M0 requires a local drive path"
        );
    }
    ensure!(
        path.components().count() >= 3,
        "drive roots cannot be installation targets"
    );
    ensure!(
        !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
        "dot segments rejected"
    );
    let text = path.to_str().context("path must be Unicode")?;
    ensure!(
        text.encode_utf16().count() < 220
            && !text
                .chars()
                .any(|c| c.is_control() || "\"|<>?*".contains(c)),
        "unsupported installation path"
    );
    #[cfg(windows)]
    {
        let normalized = text.replace('/', "\\");
        for component in normalized[3..].split('\\') {
            relative_path(component).context("ambiguous installation path component")?;
        }
    }
    reject_reparse(path)
}

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported manifest schema"
        );
        safe_id(&self.app_id)?;
        if let Some(presentation) = &self.presentation {
            presentation.validate()?;
        }
        ensure!(
            self.app_name.encode_utf16().count() <= 120
                && !self.app_name.is_empty()
                && !self.app_name.chars().any(char::is_control),
            "invalid application name"
        );
        semver::Version::parse(&self.app_version)?;
        ensure!(
            relative_path(&self.directory_name)? == self.directory_name
                && !self.directory_name.contains('\\'),
            "invalid default directory name"
        );
        ensure!(!self.files.is_empty(), "empty payload");
        ensure!(
            self.files.len() <= 50_000,
            "M0 manifest limit is 50000 files"
        );
        if let Some(runtime_hash) = &self.runtime_sha256 {
            ensure!(
                runtime_hash.len() == 64 && runtime_hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid runtime SHA-256"
            );
        }

        // Sort by components, not the full path: "a-b" must not separate
        // the file "a" from its conflicting descendant "a\\child".
        // Cache UTF-16 once; do not allocate or lowercase on each comparison.
        fn path_key(path: &str) -> Vec<u16> {
            #[cfg(windows)]
            {
                path.encode_utf16().collect()
            }
            #[cfg(not(windows))]
            {
                path.to_uppercase().encode_utf16().collect()
            }
        }
        fn ordinal(a: &[u16], b: &[u16]) -> Option<std::cmp::Ordering> {
            #[cfg(windows)]
            {
                #[link(name = "kernel32")]
                unsafe extern "system" {
                    fn CompareStringOrdinal(
                        a: *const u16,
                        alen: i32,
                        b: *const u16,
                        blen: i32,
                        ignore: i32,
                    ) -> i32;
                }
                match unsafe {
                    CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1)
                } {
                    1 => Some(std::cmp::Ordering::Less),
                    2 => Some(std::cmp::Ordering::Equal),
                    3 => Some(std::cmp::Ordering::Greater),
                    _ => None,
                }
            }
            #[cfg(not(windows))]
            {
                Some(a.cmp(b))
            }
        }
        fn compare_paths(a: &[u16], b: &[u16]) -> Option<std::cmp::Ordering> {
            let mut left = a.split(|unit| *unit == b'\\' as u16);
            let mut right = b.split(|unit| *unit == b'\\' as u16);
            loop {
                match (left.next(), right.next()) {
                    (Some(a), Some(b)) => match ordinal(a, b)? {
                        std::cmp::Ordering::Equal => {}
                        order => return Some(order),
                    },
                    (None, Some(_)) => return Some(std::cmp::Ordering::Less),
                    (Some(_), None) => return Some(std::cmp::Ordering::Greater),
                    (None, None) => return Some(std::cmp::Ordering::Equal),
                }
            }
        }
        let mut paths: Vec<(&str, Vec<u16>)> = Vec::with_capacity(self.files.len() + 2);
        // Installer-owned files also cannot become payload directories.
        paths.push((RECEIPT_NAME, path_key(RECEIPT_NAME)));
        paths.push(("uninstall.exe", path_key("uninstall.exe")));
        for item in &self.files {
            ensure!(
                relative_path(&item.path)? == item.path,
                "path must be normalized"
            );
            ensure!(
                !same_windows_name(&item.path, RECEIPT_NAME)
                    && !same_windows_name(&item.path, "uninstall.exe"),
                "reserved installer file"
            );
            ensure!(
                !same_windows_name(item.path.split('\\').next().unwrap(), ".avelin"),
                "reserved installer directory"
            );
            ensure!(
                item.sha256.len() == 64 && item.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid SHA-256"
            );
            paths.push((&item.path, path_key(&item.path)));
        }
        let mut comparison_failed = false;
        paths.sort_unstable_by(|a, b| {
            compare_paths(&a.1, &b.1).unwrap_or_else(|| {
                comparison_failed = true;
                std::cmp::Ordering::Equal
            })
        });
        ensure!(!comparison_failed, "Windows path comparison failed");
        for pair in paths.windows(2) {
            let mut right = pair[1].1.split(|unit| *unit == b'\\' as u16);
            let mut prefix = true;
            for left in pair[0].1.split(|unit| *unit == b'\\' as u16) {
                match right.next() {
                    Some(value)
                        if ordinal(left, value).context("Windows path comparison failed")?
                            == std::cmp::Ordering::Equal => {}
                    _ => {
                        prefix = false;
                        break;
                    }
                }
            }
            if prefix {
                let conflict = if right.next().is_none() {
                    "duplicate Windows path"
                } else {
                    "file/directory collision"
                };
                bail!("{conflict}: {} / {}", pair[0].0, pair[1].0);
            }
        }
        ensure!(
            self.files.iter().any(|f| f.path == self.entry),
            "entry must name a payload file"
        );
        Ok(())
    }
}

/// Fixed footer preceding the optional Authenticode certificate table.
/// Hashes detect corruption; this is not publisher authentication.
pub fn assemble(runtime: &Path, backend: &Path, manifest: &Manifest, output: &Path) -> Result<()> {
    manifest.validate()?;
    let metadata = serde_json::to_vec(manifest)?;
    let backend_length = fs::metadata(backend)?.len();
    ensure!(
        backend_length > 0
            && metadata.len() <= 32 * 1024 * 1024
            && backend_length
                .checked_add(metadata.len() as u64)
                .is_some_and(|n| n <= 1024 * 1024 * 1024),
        "package exceeds the M0 reader bounds"
    );
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .with_context(|| format!("create new package {}", output.display()))?;
    std::io::copy(&mut File::open(runtime)?, &mut out)?;
    let start = out.stream_position()?;
    let payload = fs::read(backend)?;
    out.write_all(&payload)?;
    out.write_all(&metadata)?;
    let digest = Sha256::digest([payload.as_slice(), metadata.as_slice()].concat());
    out.write_all(&start.to_le_bytes())?;
    out.write_all(&(payload.len() as u64).to_le_bytes())?;
    out.write_all(&(metadata.len() as u64).to_le_bytes())?;
    out.write_all(&digest)?;
    out.write_all(MAGIC)?;
    out.sync_all()?;
    Ok(())
}

pub fn embedded(path: &Path) -> Result<Option<(Vec<u8>, Manifest)>> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    if size < TRAILER_SIZE {
        return Ok(None);
    }
    let mut end = size;
    let mut certificate_present = false;
    let mut headers_end = 0;
    // PE32 and PE32+ security directory uses a file offset, not an RVA.
    let mut dos = [0; 64];
    file.read_exact(&mut dos)?;
    if &dos[..2] == b"MZ" {
        let pe = u32::from_le_bytes(dos[60..64].try_into()?) as u64;
        ensure!(pe >= 64 && pe + 26 <= size, "invalid PE header");
        file.seek(SeekFrom::Start(pe))?;
        let mut header = [0; 26];
        file.read_exact(&mut header)?;
        ensure!(&header[..4] == b"PE\0\0", "invalid PE signature");
        let directory = match u16::from_le_bytes(header[24..26].try_into()?) {
            0x10b => 96,
            0x20b => 112,
            _ => bail!("unsupported PE format"),
        };
        let optional_size = u16::from_le_bytes(header[20..22].try_into()?) as u64;
        headers_end = pe + 24 + optional_size;
        ensure!(
            optional_size >= directory && headers_end <= size,
            "invalid PE optional header size"
        );
        file.seek(SeekFrom::Start(pe + 24 + directory - 4))?;
        let mut count = [0; 4];
        file.read_exact(&mut count)?;
        let count = u32::from_le_bytes(count) as u64;
        ensure!(
            count <= (optional_size - directory) / 8,
            "PE data directories exceed optional header"
        );
        // Bytes at the security-directory offset are meaningful only when the
        // optional header actually declares that entry.
        if count > 4 {
            file.seek(SeekFrom::Start(pe + 24 + directory + 4 * 8))?;
            let mut cert = [0; 8];
            file.read_exact(&mut cert)?;
            let pos = u32::from_le_bytes(cert[..4].try_into()?) as u64;
            let len = u32::from_le_bytes(cert[4..].try_into()?) as u64;
            if pos != 0 || len != 0 {
                ensure!(
                    pos >= headers_end && pos % 8 == 0 && len >= 8 && pos + len == size,
                    "unsupported certificate layout"
                );
                end = pos;
                certificate_present = true;
            }
        }
    }
    // Authenticode tools may align certificate data with up to seven zero bytes.
    // Unsigned packages have no such gap: their footer must end exactly at EOF.
    for padding in 0..=if certificate_present { 7 } else { 0 } {
        if end < TRAILER_SIZE + padding {
            continue;
        }
        file.seek(SeekFrom::Start(end - padding - TRAILER_SIZE))?;
        let mut footer = [0; 72];
        file.read_exact(&mut footer)?;
        if &footer[56..] != MAGIC {
            continue;
        }
        let mut alignment = [0; 7];
        file.read_exact(&mut alignment[..padding as usize])?;
        ensure!(
            alignment.iter().all(|byte| *byte == 0),
            "nonzero certificate alignment padding"
        );
        let start = u64::from_le_bytes(footer[..8].try_into()?);
        let backend_len = u64::from_le_bytes(footer[8..16].try_into()?);
        let metadata_len = u64::from_le_bytes(footer[16..24].try_into()?);
        ensure!(
            backend_len > 0 && metadata_len > 0 && metadata_len <= 32 * 1024 * 1024,
            "invalid package lengths"
        );
        let len = backend_len
            .checked_add(metadata_len)
            .context("package length overflow")?;
        ensure!(
            start >= headers_end && start.checked_add(len) == Some(end - padding - TRAILER_SIZE),
            "package bounds mismatch"
        );
        ensure!(
            len <= 1024 * 1024 * 1024,
            "M0 package reader limited to 1 GiB"
        );
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; len as usize];
        file.read_exact(&mut bytes)?;
        ensure!(
            Sha256::digest(&bytes)[..] == footer[24..56],
            "package integrity mismatch"
        );
        let manifest: Manifest = serde_json::from_slice(&bytes[backend_len as usize..])?;
        manifest.validate()?;
        bytes.truncate(backend_len as usize);
        return Ok(Some((bytes, manifest)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        time::Instant,
    };

    #[cfg(windows)]
    #[test]
    fn reject_ambiguous_installation_roots() {
        for root in [
            r"C:\Avelin:stream",
            r"C:\Avelin.\App",
            r"C:\Avelin \App",
            r"C:\Avelin\.\App",
            r"C:\Avelin\..\App",
            r"C:\Avelin\CON",
        ] {
            assert!(install_root(Path::new(root)).is_err(), "accepted {root}");
        }
    }

    fn manifest(paths: &[&str]) -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION,
            app_id: "com.example.core-test".into(),
            app_name: "中文 ${Brand}".into(),
            app_version: "1.2.3".into(),
            entry: paths.first().copied().unwrap_or("entry.exe").into(),
            directory_name: "中文 ${Directory}".into(),
            start_menu_shortcut: false,
            files: paths
                .iter()
                .map(|path| FileResource {
                    path: (*path).into(),
                    size: 0,
                    sha256: hash_bytes(b""),
                })
                .collect(),
            runtime_sha256: None,
            presentation: None,
            desktop_shortcut: false,
        }
    }

    struct Fixture {
        directory: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let directory = std::env::temp_dir().join(format!(
                "avelin-core-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            Self { directory }
        }
        fn path(&self, name: &str) -> PathBuf {
            self.directory.join(name)
        }
        fn package(&self, pe32_plus: bool, payload: &[u8]) -> Vec<u8> {
            let mut stub = vec![0u8; 512];
            stub[..2].copy_from_slice(b"MZ");
            stub[60..64].copy_from_slice(&128u32.to_le_bytes());
            stub[128..132].copy_from_slice(b"PE\0\0");
            let optional_size: u16 = if pe32_plus { 240 } else { 224 };
            stub[148..150].copy_from_slice(&optional_size.to_le_bytes());
            let magic: u16 = if pe32_plus { 0x20b } else { 0x10b };
            stub[152..154].copy_from_slice(&magic.to_le_bytes());
            let count_offset = 152 + if pe32_plus { 108 } else { 92 };
            stub[count_offset..count_offset + 4].copy_from_slice(&16u32.to_le_bytes());
            fs::write(self.path("runtime.exe"), stub).unwrap();
            fs::write(self.path("backend.bin"), payload).unwrap();
            // Each variant in this owned fixture starts a fresh package. The
            // production assembler deliberately refuses to overwrite outputs.
            if self.path("package.exe").exists() {
                fs::remove_file(self.path("package.exe")).unwrap();
            }
            assemble(
                &self.path("runtime.exe"),
                &self.path("backend.bin"),
                &manifest(&["entry.exe"]),
                &self.path("package.exe"),
            )
            .unwrap();
            fs::read(self.path("package.exe")).unwrap()
        }
        fn read(&self, bytes: &[u8]) -> Result<Option<(Vec<u8>, Manifest)>> {
            fs::write(self.path("package.exe"), bytes).unwrap();
            embedded(&self.path("package.exe"))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Only remove the exact files this fixture creates, never recurse.
            for name in ["runtime.exe", "backend.bin", "package.exe"] {
                let _ = fs::remove_file(self.path(name));
            }
            let _ = fs::remove_dir(&self.directory);
        }
    }
    fn security_directory(pe32_plus: bool) -> usize {
        128 + 24 + if pe32_plus { 112 } else { 96 } + 4 * 8
    }
    fn append_mock_certificate(bytes: &mut Vec<u8>, pe32_plus: bool) -> (usize, usize) {
        let padding = (8 - bytes.len() % 8) % 8;
        bytes.resize(bytes.len() + padding, 0);
        let position = bytes.len();
        let directory = security_directory(pe32_plus);
        bytes[directory..directory + 4].copy_from_slice(&(position as u32).to_le_bytes());
        bytes[directory + 4..directory + 8].copy_from_slice(&16u32.to_le_bytes());
        // A synthetic WIN_CERTIFICATE layout, NOT a valid Authenticode signature.
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&0x0200u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(b"TESTCERT");
        (position, padding)
    }

    #[test]
    fn windows_paths_reject_ambiguous_and_reserved_names() {
        for value in [
            "../x",
            "a/../b",
            "C:\\x",
            "\\\\host\\x",
            "con.txt",
            "a/COM¹.txt",
            "a.",
            "x:stream",
            "a//b",
        ] {
            assert!(relative_path(value).is_err(), "{value}");
        }
        assert_eq!(relative_path("中文/${X}.dll").unwrap(), "中文\\${X}.dll");
    }
    #[test]
    fn windows_comparison_recognizes_case_aliases() {
        assert!(same_windows_name("Assets\\FILE.txt", "assets\\file.TXT"));
    }

    #[test]
    fn manifest_rejects_case_aliases_and_parent_files_in_any_input_order() {
        for paths in [
            vec!["Assets\\File.bin", "assets\\FILE.BIN"],
            vec!["Ä.txt", "ä.TXT"],
            vec!["Σ.dat", "σ.DAT"],
            vec!["a", "a-b", "a\\child"],
            vec!["a\\child", "a0", "a", "a-b"],
            vec!["界面\\文件", "界面\\文件-副本", "界面\\文件\\child"],
            vec!["a\\b\\child", "A\\B", "a\\b-other"],
        ] {
            assert!(
                manifest(&paths).validate().is_err(),
                "accepted conflicting paths: {paths:?}"
            );
        }
    }

    #[test]
    fn manifest_preserves_legal_unicode_dollar_and_prefix_siblings() {
        let paths = [
            "中文\\${X}.dll",
            "a-b",
            "a0",
            "a\\child",
            "a\\child2",
            "folder\\uninstall.exe",
            ".avelin-more\\file",
        ];
        assert!(manifest(&paths).validate().is_ok());
        assert_eq!(manifest(&paths).files[0].path, "中文\\${X}.dll");
    }

    #[test]
    fn manifest_rejects_reserved_installer_roots_and_invalid_runtime_hashes() {
        for path in [
            ".avelin",
            ".AVELIN\\manifest.json",
            "uninstall.exe",
            "UNINSTALL.EXE\\child",
            ".avelin-receipt.json",
            ".AVELIN-RECEIPT.JSON\\child",
        ] {
            assert!(
                manifest(&[path]).validate().is_err(),
                "accepted installer-owned path {path}"
            );
        }
        for hash in [
            "".to_owned(),
            "a".repeat(63),
            "a".repeat(65),
            "g".repeat(64),
        ] {
            let mut value = manifest(&["entry.exe"]);
            value.runtime_sha256 = Some(hash);
            assert!(value.validate().is_err());
        }
        let mut value = manifest(&["entry.exe"]);
        value.runtime_sha256 = Some("AB".repeat(32));
        assert!(value.validate().is_ok());
    }

    #[test]
    fn manifest_validates_fifty_thousand_files_and_rejects_the_next() {
        let mut value = manifest(&["files\\item-00000.bin"]);
        value.files = (0..50_000)
            .map(|index| FileResource {
                path: format!("files\\item-{:05}.bin", (index * 7919) % 50_000),
                size: 0,
                sha256: "a".repeat(64),
            })
            .collect();
        let start = Instant::now();
        value.validate().unwrap();
        eprintln!(
            "validated 50000 deterministically shuffled Windows paths in {:?}",
            start.elapsed()
        );
        value.files.push(FileResource {
            path: "files\\next.bin".into(),
            size: 0,
            sha256: "a".repeat(64),
        });
        assert!(value.validate().unwrap_err().to_string().contains("50000"));
    }

    #[test]
    fn assemble_refuses_existing_output_without_changing_its_bytes() {
        let fixture = Fixture::new();
        let original = fixture.package(false, b"original backend");
        fs::write(fixture.path("backend.bin"), b"new backend").unwrap();
        assert!(
            assemble(
                &fixture.path("runtime.exe"),
                &fixture.path("backend.bin"),
                &manifest(&["entry.exe"]),
                &fixture.path("package.exe"),
            )
            .is_err()
        );
        assert_eq!(fs::read(fixture.path("package.exe")).unwrap(), original);
    }

    #[test]
    fn overlay_unsigned_roundtrip_and_nonpackage_detection() {
        let fixture = Fixture::new();
        let bytes = fixture.package(false, b"sample backend");
        assert_eq!(&bytes[bytes.len() - 16..], MAGIC);
        let (payload, value) = fixture.read(&bytes).unwrap().unwrap();
        assert_eq!(payload, b"sample backend");
        assert_eq!(value.app_id, "com.example.core-test");
        assert!(fixture.read(b"ordinary small file").unwrap().is_none());
    }

    #[test]
    fn overlay_unsigned_footer_must_end_at_eof() {
        let fixture = Fixture::new();
        for pe32_plus in [false, true] {
            let original = fixture.package(pe32_plus, b"backend");
            for count in 1..=7 {
                for byte in [0, 0x41, 0xff] {
                    let mut appended = original.clone();
                    appended.extend(std::iter::repeat_n(byte, count));
                    assert!(
                        fixture.read(&appended).unwrap().is_none(),
                        "accepted unsigned trailer followed by {count} bytes of {byte}"
                    );
                }
            }
        }
    }

    #[test]
    fn overlay_locates_footer_before_pe32_and_pe32plus_certificate_alignment() {
        let fixture = Fixture::new();
        for pe32_plus in [false, true] {
            let mut seen_padding = [false; 8];
            for size in 1..=8 {
                let payload = vec![b'x'; size];
                let mut bytes = fixture.package(pe32_plus, &payload);
                let (position, padding) = append_mock_certificate(&mut bytes, pe32_plus);
                seen_padding[padding] = true;
                assert_eq!(position % 8, 0);
                assert_eq!(fixture.read(&bytes).unwrap().unwrap().0, payload);
                // Certificate bytes are not payload bytes. Locating them is NOT
                // certificate verification, and this API must not claim it is.
                bytes[position + 8] ^= 0x01;
                assert_eq!(fixture.read(&bytes).unwrap().unwrap().0, payload);
            }
            assert!(seen_padding.into_iter().all(|seen| seen));
        }
    }

    #[test]
    fn overlay_rejects_nonzero_certificate_alignment_padding() {
        let fixture = Fixture::new();
        for pe32_plus in [false, true] {
            let mut checked_padding = [false; 8];
            for size in 1..=8 {
                let mut bytes = fixture.package(pe32_plus, &vec![b'x'; size]);
                let (position, padding) = append_mock_certificate(&mut bytes, pe32_plus);
                checked_padding[padding] = true;
                for index in position - padding..position {
                    bytes[index] = 1;
                    let error = fixture.read(&bytes).unwrap_err();
                    assert!(error.to_string().contains("alignment padding"));
                    bytes[index] = 0;
                }
            }
            assert!(checked_padding.into_iter().all(|seen| seen));
        }
    }

    #[test]
    fn overlay_validates_declared_optional_header_and_data_directories() {
        let fixture = Fixture::new();
        for pe32_plus in [false, true] {
            let original = fixture.package(pe32_plus, b"backend");
            let directory: u16 = if pe32_plus { 112 } else { 96 };
            let count_offset = 152 + directory as usize - 4;
            for size in [directory - 1, u16::MAX] {
                let mut bytes = original.clone();
                bytes[148..150].copy_from_slice(&size.to_le_bytes());
                assert!(fixture.read(&bytes).is_err());
            }
            for count in [17u32, u32::MAX] {
                let mut bytes = original.clone();
                bytes[count_offset..count_offset + 4].copy_from_slice(&count.to_le_bytes());
                assert!(fixture.read(&bytes).is_err());
            }
            // A short directory table is valid for an unsigned PE; it must
            // not be interpreted as declaring a security-directory entry.
            let mut bytes = original.clone();
            bytes[count_offset..count_offset + 4].copy_from_slice(&4u32.to_le_bytes());
            assert_eq!(fixture.read(&bytes).unwrap().unwrap().0, b"backend");
            append_mock_certificate(&mut bytes, pe32_plus);
            assert!(fixture.read(&bytes).unwrap().is_none());

            // A declared security entry must fit inside SizeOfOptionalHeader,
            // even when these bytes exist elsewhere in the overall file.
            let mut bytes = original;
            bytes[148..150].copy_from_slice(&(directory + 4 * 8).to_le_bytes());
            bytes[count_offset..count_offset + 4].copy_from_slice(&5u32.to_le_bytes());
            assert!(fixture.read(&bytes).is_err());
        }
    }

    #[test]
    fn receipt_preserves_legacy_input_without_inventing_uninstaller_identity() {
        let legacy = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "app_id": "com.example.core-test",
            "app_version": "1.2.3",
            "installation_id": "install",
            "task_id": "task",
            "commit_id": "commit",
            "install_dir": r"C:\Avelin",
            "manifest_sha256": hash_bytes(b"manifest"),
            "user_sid": "test-sid",
            "files": []
        });
        let mut receipt: Receipt = serde_json::from_value(legacy.clone()).unwrap();
        assert!(receipt.uninstaller_sha256.is_none());
        assert_eq!(serde_json::to_value(&receipt).unwrap(), legacy);

        let digest = hash_bytes(b"uninstaller");
        receipt.uninstaller_sha256 = Some(digest.clone());
        let encoded = serde_json::to_value(&receipt).unwrap();
        assert_eq!(encoded["uninstaller_sha256"], digest);
        let decoded: Receipt = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.uninstaller_sha256, Some(digest));
    }

    #[test]
    fn overlay_rejects_payload_metadata_digest_and_length_tampering() {
        let fixture = Fixture::new();
        let original = fixture.package(false, b"backend");
        let footer = original.len() - TRAILER_SIZE as usize;
        for index in [512, 512 + b"backend".len(), footer + 24] {
            let mut corrupted = original.clone();
            corrupted[index] ^= 1;
            assert!(
                fixture.read(&corrupted).is_err(),
                "tampering at {index} was accepted"
            );
        }
        for (offset, value) in [
            (0, u64::MAX),
            (8, u64::MAX),
            (8, 0),
            (16, 0),
            (16, 32 * 1024 * 1024 + 1),
        ] {
            let mut corrupted = original.clone();
            corrupted[footer + offset..footer + offset + 8].copy_from_slice(&value.to_le_bytes());
            assert!(
                fixture.read(&corrupted).is_err(),
                "invalid footer offset={offset}, value={value} was accepted"
            );
        }
    }

    #[test]
    fn overlay_rejects_truncated_certificate_and_out_of_bounds_pe_offsets() {
        let fixture = Fixture::new();
        for pe32_plus in [false, true] {
            let mut bytes = fixture.package(pe32_plus, b"backend");
            append_mock_certificate(&mut bytes, pe32_plus);
            bytes.pop();
            assert!(fixture.read(&bytes).is_err());
            let mut bytes = fixture.package(pe32_plus, b"backend");
            bytes[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(fixture.read(&bytes).is_err());
            let mut bytes = fixture.package(pe32_plus, b"backend");
            let directory = security_directory(pe32_plus);
            bytes[directory..directory + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            bytes[directory + 4..directory + 8].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(fixture.read(&bytes).is_err());
        }
    }
}
