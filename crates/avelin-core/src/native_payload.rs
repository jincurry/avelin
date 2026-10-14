//! Version 1 of the native worker's embedded payload. All integers are little endian.
//!
//! Layout: 16-byte magic, u32 version, u32 file count, u64 runtime length,
//! runtime bytes, then a u64 length and bytes for each manifest file in order.
//! Names exist only in the validated manifest. There is no compression or path
//! interpretation in the archive, and no bytes may follow the final file.
use crate::{Manifest, hash_bytes, reject_reparse};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

pub const MAGIC: &[u8; 16] = b"AVELIN-RUST-P001";
const VERSION: u32 = 1;
const MAX_SIZE: u64 = 1024 * 1024 * 1024;

/// Borrowed, fully checked payload slices in the manifest's file order.
#[derive(Debug)]
pub struct NativePayload<'a> {
    pub runtime: &'a [u8],
    pub files: Vec<&'a [u8]>,
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: u64) -> Result<&'a [u8]> {
        let length =
            usize::try_from(length).context("native payload length exceeds address space")?;
        let end = self
            .offset
            .checked_add(length)
            .context("native payload length overflow")?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .context("truncated native payload")?;
        self.offset = end;
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
}

impl<'a> NativePayload<'a> {
    pub fn parse(bytes: &'a [u8], manifest: &Manifest) -> Result<Self> {
        manifest.validate()?;
        ensure!(
            bytes.len() as u64 <= MAX_SIZE,
            "native payload exceeds 1 GiB"
        );
        let runtime_hash = manifest
            .runtime_sha256
            .as_deref()
            .context("native payload requires a runtime SHA-256")?;
        let mut reader = Reader { bytes, offset: 0 };
        ensure!(reader.take(16)? == MAGIC, "invalid native payload magic");
        ensure!(
            reader.u32()? == VERSION,
            "unsupported native payload version"
        );
        ensure!(
            reader.u32()? as usize == manifest.files.len(),
            "native payload file count differs from manifest"
        );
        let runtime_length = reader.u64()?;
        ensure!(runtime_length > 0, "empty native runtime");
        let runtime = reader.take(runtime_length)?;
        ensure!(
            hash_bytes(runtime).eq_ignore_ascii_case(runtime_hash),
            "native runtime SHA-256 mismatch"
        );
        let mut files = Vec::with_capacity(manifest.files.len());
        for file in &manifest.files {
            let length = reader.u64()?;
            ensure!(
                length == file.size,
                "native file length mismatch: {}",
                file.path
            );
            let bytes = reader.take(length)?;
            ensure!(
                hash_bytes(bytes).eq_ignore_ascii_case(&file.sha256),
                "native file SHA-256 mismatch: {}",
                file.path
            );
            files.push(bytes);
        }
        ensure!(
            reader.offset == bytes.len(),
            "trailing native payload bytes"
        );
        Ok(Self { runtime, files })
    }
}

/// Copy a file while checking the exact bytes written, not a prior pathname read.
fn write_checked(
    output: &mut File,
    path: &Path,
    expected_size: u64,
    expected_hash: &str,
) -> Result<()> {
    reject_reparse(path)?;
    let mut input = File::open(path).with_context(|| format!("open {}", path.display()))?;
    ensure!(
        input.metadata()?.is_file(),
        "native payload input is not a regular file"
    );
    ensure!(
        input.metadata()?.len() == expected_size,
        "native input size changed: {}",
        path.display()
    );
    let mut hasher = Sha256::new();
    let mut remaining = expected_size;
    let mut buffer = [0; 64 * 1024];
    while remaining > 0 {
        let capacity = remaining.min(buffer.len() as u64) as usize;
        let read = input.read(&mut buffer[..capacity])?;
        ensure!(
            read != 0,
            "native input truncated while copying: {}",
            path.display()
        );
        output.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    ensure!(
        input.read(&mut buffer[..1])? == 0,
        "native input grew while copying: {}",
        path.display()
    );
    ensure!(
        hex::encode(hasher.finalize()).eq_ignore_ascii_case(expected_hash),
        "native input SHA-256 changed while copying: {}",
        path.display()
    );
    Ok(())
}

/// Write a new native payload; existing outputs are never replaced. A failed
/// copy leaves its incomplete output for the build's private staging evidence.
pub fn write_payload(
    source: &Path,
    runtime: &Path,
    manifest: &Manifest,
    output: &Path,
) -> Result<()> {
    manifest.validate()?;
    reject_reparse(source)?;
    reject_reparse(runtime)?;
    reject_reparse(output)?;
    ensure!(source.is_dir(), "native payload source is not a directory");
    let runtime_hash = manifest
        .runtime_sha256
        .as_deref()
        .context("native payload requires a runtime SHA-256")?;
    let runtime_length = std::fs::metadata(runtime)?.len();
    ensure!(runtime_length > 0, "empty native runtime");
    let total = manifest.files.iter().try_fold(
        32_u64
            .checked_add(runtime_length)
            .context("native runtime size overflow")?,
        |total, file| {
            total
                .checked_add(8)
                .and_then(|n| n.checked_add(file.size))
                .context("native payload size overflow")
        },
    )?;
    ensure!(total <= MAX_SIZE, "native payload exceeds 1 GiB");
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .with_context(|| format!("create new native payload {}", output.display()))?;
    out.write_all(MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&(manifest.files.len() as u32).to_le_bytes())?;
    out.write_all(&runtime_length.to_le_bytes())?;
    write_checked(&mut out, runtime, runtime_length, runtime_hash)?;
    for file in &manifest.files {
        // Split the normalized Windows path explicitly so this format is also
        // reproducible by tooling running on a non-Windows host.
        let path = file
            .path
            .split('\\')
            .fold(source.to_path_buf(), |base, part| base.join(part));
        out.write_all(&file.size.to_le_bytes())?;
        write_checked(&mut out, &path, file.size, &file.sha256)?;
    }
    out.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileResource, SCHEMA_VERSION};
    use std::path::PathBuf;

    fn manifest() -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION,
            install_scope: crate::InstallScope::CurrentUser,
            prerequisites: vec![],
            updates: None,
            components: vec![],
            app_id: "dev.avelin.native-fixture".into(),
            app_name: "Native fixture".into(),
            app_version: "1.0.0".into(),
            entry: "app.exe".into(),
            directory_name: "NativeFixture".into(),
            start_menu_shortcut: false,
            desktop_shortcut: false,
            runtime_sha256: Some(hash_bytes(b"runtime")),
            presentation: None,
            files: [("app.exe", &b"app"[..]), (r"中文\empty.txt", &b""[..])]
                .into_iter()
                .map(|(path, bytes)| FileResource {
                    component_id: None,
                    path: path.into(),
                    size: bytes.len() as u64,
                    sha256: hash_bytes(bytes),
                })
                .collect(),
        }
    }

    fn encoded() -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend(2_u32.to_le_bytes());
        bytes.extend(7_u64.to_le_bytes());
        bytes.extend(b"runtime");
        bytes.extend(3_u64.to_le_bytes());
        bytes.extend(b"app");
        bytes.extend(0_u64.to_le_bytes());
        bytes
    }

    #[test]
    fn parses_borrowed_runtime_and_ordered_files_including_empty_unicode_path() {
        let bytes = encoded();
        let parsed = NativePayload::parse(&bytes, &manifest()).unwrap();
        assert_eq!(parsed.runtime, b"runtime");
        assert_eq!(parsed.runtime.as_ptr(), bytes[32..].as_ptr());
        assert_eq!(parsed.files, [b"app".as_slice(), b"".as_slice()]);
    }

    #[test]
    fn component_selection_preserves_full_payload_offsets_and_hash_validation() {
        let mut manifest = manifest();
        manifest.schema_version = crate::MANIFEST_SCHEMA_VERSION;
        let core = crate::ComponentDefinition::core();
        let mut optional = core.clone();
        optional.id = "extra".into();
        optional.required = false;
        optional.default_selected = false;
        manifest.components = vec![core, optional];
        manifest.files[0].component_id = Some("core".into());
        manifest.files[1].component_id = Some("extra".into());
        let mut bytes = encoded();
        let parsed = NativePayload::parse(&bytes, &manifest).unwrap();
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(
            manifest
                .resolve_selection(&["core".into()])
                .unwrap()
                .files
                .len(),
            1
        );
        // Even the unselected optional file's length is checked. A reader must
        // never reinterpret a complete payload using a filtered manifest.
        let optional_length_offset = bytes.len() - 8;
        bytes[optional_length_offset] = 1;
        assert!(NativePayload::parse(&bytes, &manifest).is_err());
        bytes = encoded();
        manifest.files[1].sha256 = hash_bytes(b"different optional content");
        assert!(NativePayload::parse(&bytes, &manifest).is_err());
    }

    #[test]
    fn all_truncations_and_trailing_bytes_are_rejected() {
        let mut bytes = encoded();
        for length in 0..bytes.len() {
            assert!(
                NativePayload::parse(&bytes[..length], &manifest()).is_err(),
                "accepted truncation at {length}"
            );
        }
        bytes.push(0);
        assert!(NativePayload::parse(&bytes, &manifest()).is_err());
    }

    #[test]
    fn malformed_headers_lengths_and_hashes_are_rejected_without_panics() {
        for offset in [0, 16, 20, 24, 32, 39, 47, 50] {
            let mut bytes = encoded();
            bytes[offset] ^= 0xff;
            assert!(
                NativePayload::parse(&bytes, &manifest()).is_err(),
                "accepted corruption at {offset}"
            );
        }
        let mut bytes = encoded();
        bytes[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(NativePayload::parse(&bytes, &manifest()).is_err());
        let mut manifest = manifest();
        manifest.files[0].size = u64::MAX;
        bytes = encoded();
        bytes[39..47].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(NativePayload::parse(&bytes, &manifest).is_err());
    }

    #[test]
    fn requires_valid_manifest_and_trusted_runtime_digest() {
        let bytes = encoded();
        let mut m = manifest();
        m.runtime_sha256 = None;
        assert!(NativePayload::parse(&bytes, &m).is_err());
        m.runtime_sha256 = Some(hash_bytes(b"other runtime"));
        assert!(NativePayload::parse(&bytes, &m).is_err());
        m = manifest();
        m.files[0].sha256 = hash_bytes(b"other file");
        assert!(NativePayload::parse(&bytes, &m).is_err());
        m = manifest();
        m.files[1].path = r"..\escaped.txt".into();
        assert!(NativePayload::parse(&bytes, &m).is_err());
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "avelin-native-payload-{}-{now}-{serial}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::create_dir(path.join("中文")).unwrap();
            std::fs::write(path.join("runtime.exe"), b"runtime").unwrap();
            std::fs::write(path.join("app.exe"), b"app").unwrap();
            std::fs::write(path.join("中文/empty.txt"), b"").unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for name in ["runtime.exe", "app.exe", "中文/empty.txt", "payload.bin"] {
                let _ = std::fs::remove_file(self.0.join(name));
            }
            let _ = std::fs::remove_dir(self.0.join("中文"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn writer_round_trips_exact_bytes_and_preserves_existing_output() {
        let fixture = Fixture::new();
        let output = fixture.0.join("payload.bin");
        let runtime = fixture.0.join("runtime.exe");
        write_payload(&fixture.0, &runtime, &manifest(), &output).unwrap();
        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(bytes, encoded());
        NativePayload::parse(&bytes, &manifest()).unwrap();
        assert!(write_payload(&fixture.0, &runtime, &manifest(), &output).is_err());
        assert_eq!(std::fs::read(output).unwrap(), bytes);
    }

    #[test]
    fn writer_refuses_size_and_hash_changes_after_inventory() {
        let fixture = Fixture::new();
        let runtime = fixture.0.join("runtime.exe");
        let output = fixture.0.join("payload.bin");
        for changed in [b"bad".as_slice(), b"longer".as_slice(), b"".as_slice()] {
            std::fs::write(fixture.0.join("app.exe"), changed).unwrap();
            assert!(write_payload(&fixture.0, &runtime, &manifest(), &output).is_err());
            std::fs::remove_file(&output).unwrap();
        }
        std::fs::write(&runtime, b"corrupt").unwrap();
        assert!(write_payload(&fixture.0, &runtime, &manifest(), &output).is_err());
    }

    #[test]
    fn writer_rejects_oversized_or_invalid_inventory_before_creating_output() {
        let fixture = Fixture::new();
        let runtime = fixture.0.join("runtime.exe");
        let output = fixture.0.join("payload.bin");
        for size in [MAX_SIZE, u64::MAX] {
            let mut m = manifest();
            m.files[0].size = size;
            assert!(write_payload(&fixture.0, &runtime, &m, &output).is_err());
            assert!(!output.exists());
        }
        let mut m = manifest();
        m.files[1].path = r"..\escape.txt".into();
        assert!(write_payload(&fixture.0, &runtime, &m, &output).is_err());
        assert!(!output.exists());
    }
}
