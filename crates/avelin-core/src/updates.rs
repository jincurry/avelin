//! Explicit HTTPS update discovery and complete-package downloads.
//!
//! The configured feed's HTTPS origin is the trust anchor. SHA-256 detects
//! corruption and binds the downloaded package to that feed; it is not a
//! publisher signature. This module never executes a downloaded package.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

pub const MAX_FEED_BYTES: u64 = 64 * 1024;
pub const MAX_SETUP_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const REQUEST_DEADLINE: Duration = Duration::from_secs(10 * 60);
static NEVER_CANCEL: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfig {
    pub feed_url: String,
    #[serde(default = "stable_channel")]
    pub channel: String,
}

fn stable_channel() -> String {
    "stable".into()
}

impl UpdateConfig {
    pub fn validate(&self) -> Result<()> {
        HttpsUrl::parse(&self.feed_url)?;
        validate_channel(&self.channel)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateRelease {
    pub schema_version: u32,
    pub app_id: String,
    pub version: String,
    pub channel: String,
    pub platform: String,
    pub setup_url: String,
    pub size: u64,
    pub sha256: String,
}

impl UpdateRelease {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported update feed schema");
        crate::safe_id(&self.app_id)?;
        let version = semver::Version::parse(&self.version)?;
        validate_channel(&self.channel)?;
        ensure!(
            self.channel != "stable" || version.pre.is_empty(),
            "stable update feed cannot contain a prerelease"
        );
        ensure!(
            self.platform == "windows-x64",
            "unsupported update platform"
        );
        HttpsUrl::parse(&self.setup_url)?;
        ensure!(
            self.size > 0 && self.size <= MAX_SETUP_BYTES,
            "update size is outside supported bounds"
        );
        ensure!(
            self.sha256.len() == 64 && self.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid update SHA-256"
        );
        Ok(())
    }
}

fn validate_channel(channel: &str) -> Result<()> {
    ensure!(
        !channel.is_empty()
            && channel.len() <= 32
            && channel
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "update channel must contain 1-32 lowercase letters, digits or hyphens"
    );
    Ok(())
}

/// A successful check is read-only. Same/older versions report no update.
pub fn check(
    config: &UpdateConfig,
    app_id: &str,
    current_version: &str,
) -> Result<Option<UpdateRelease>> {
    check_cancel(config, app_id, current_version, &NEVER_CANCEL)
}

pub fn check_cancel(
    config: &UpdateConfig,
    app_id: &str,
    current_version: &str,
    cancel: &AtomicBool,
) -> Result<Option<UpdateRelease>> {
    check_with_cancel(&NativeTransport, config, app_id, current_version, cancel)
}

#[cfg(test)]
fn check_with(
    transport: &impl Transport,
    config: &UpdateConfig,
    app_id: &str,
    current_version: &str,
) -> Result<Option<UpdateRelease>> {
    check_with_cancel(transport, config, app_id, current_version, &NEVER_CANCEL)
}

fn check_with_cancel(
    transport: &impl Transport,
    config: &UpdateConfig,
    app_id: &str,
    current_version: &str,
    cancel: &AtomicBool,
) -> Result<Option<UpdateRelease>> {
    config.validate()?;
    crate::safe_id(app_id)?;
    let current = semver::Version::parse(current_version)?;
    let mut bytes = Vec::new();
    transfer_cancel(
        transport,
        &config.feed_url,
        MAX_FEED_BYTES,
        None,
        &mut bytes,
        cancel,
    )?;
    let release: UpdateRelease = serde_json::from_slice(&bytes).context("invalid update feed")?;
    release.validate()?;
    ensure!(
        release.app_id == app_id,
        "update feed belongs to another application"
    );
    ensure!(
        release.channel == config.channel,
        "update feed channel does not match configuration"
    );
    let available = semver::Version::parse(&release.version)?;
    // Build metadata is not precedence (1.0.0+new cannot upgrade 1.0.0+old).
    if available.cmp_precedence(&current).is_le() {
        return Ok(None);
    }
    Ok(Some(release))
}

/// Keep this guard alive until the installer process has opened the package.
/// Windows handles prevent pathname replacement and content changes during use.
#[derive(Debug)]
pub struct DownloadedUpdate {
    pub path: PathBuf,
    pub release: UpdateRelease,
    /// The caller also binds package policy (including install scope) to its
    /// existing installation before launching the verified Setup.
    pub manifest: crate::Manifest,
    _file: File,
    _directories: Vec<File>,
}

/// Download to a sibling staging file, verify, and atomically publish a *new*
/// destination. Existing destinations are never replaced; errors never execute.
pub fn download(release: &UpdateRelease, destination: &Path) -> Result<DownloadedUpdate> {
    download_cancel(release, destination, &NEVER_CANCEL)
}

pub fn download_cancel(
    release: &UpdateRelease,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<DownloadedUpdate> {
    download_with_cancel(&NativeTransport, release, destination, cancel)
}

/// Build feed metadata from the exact finished Setup, after any external
/// signing step. A feed generated before signing will intentionally fail hashes.
pub fn release_for_setup(setup: &Path, setup_url: &str, channel: &str) -> Result<UpdateRelease> {
    let setup = std::path::absolute(setup)?;
    let _directories = pin_directories(setup.parent().context("Setup has no parent")?)?;
    let file = open_pinned(&setup)?;
    let size = file.metadata()?.len();
    ensure!(
        size > 0 && size <= MAX_SETUP_BYTES,
        "Setup exceeds supported update size"
    );
    let (_, manifest) = crate::embedded(&setup)?.context("Setup has no embedded Avelin package")?;
    let release = UpdateRelease {
        schema_version: 1,
        app_id: manifest.app_id,
        version: manifest.app_version,
        channel: channel.to_owned(),
        platform: "windows-x64".into(),
        setup_url: setup_url.to_owned(),
        size,
        sha256: crate::hash_file(&setup)?,
    };
    release.validate()?;
    verify_setup(&setup, &file, &release)?;
    Ok(release)
}

#[cfg(test)]
fn download_with(
    transport: &impl Transport,
    release: &UpdateRelease,
    destination: &Path,
) -> Result<DownloadedUpdate> {
    download_with_cancel(transport, release, destination, &NEVER_CANCEL)
}

fn download_with_cancel(
    transport: &impl Transport,
    release: &UpdateRelease,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<DownloadedUpdate> {
    ensure!(!cancel.load(Ordering::SeqCst), "update download cancelled");
    release.validate()?;
    let destination = std::path::absolute(destination)?;
    crate::install_root(&destination)?;
    let parent = destination
        .parent()
        .context("update destination has no parent")?;
    let directories = pin_directories(parent)?;
    ensure!(
        !destination.try_exists()?,
        "update destination already exists"
    );
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let staging = parent.join(format!(
        ".avelin-download-{}-{}-{}.part",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Keep DELETE authority on this exact staging file for atomic rename,
        // while denying competing writes, deletes and replacements.
        options.access_mode(0xc001_0000).share_mode(1);
    }
    let opened = options.open(&staging)?;
    let staged = StagingFile(staging.clone());
    // On errors, close the non-delete-shared writer before cleanup removes the
    // exact staging file that this operation successfully created.
    let mut writer = opened;
    let actual_hash = transfer_cancel(
        transport,
        &release.setup_url,
        release.size,
        Some(release.size),
        &mut writer,
        cancel,
    )?;
    ensure!(
        actual_hash.eq_ignore_ascii_case(&release.sha256),
        "downloaded update SHA-256 mismatch"
    );
    writer.sync_all()?;
    verify_setup(&staging, &writer, release)?;
    ensure!(!cancel.load(Ordering::SeqCst), "update download cancelled");
    publish_open_file(&writer, &staging, &destination, directories.last())?;
    drop(writer);
    drop(staged);
    let file = open_pinned(&destination)?;
    // Recheck after acquiring the final non-write/non-delete handle. Never use
    // a digest from an earlier pathname lookup to authorize execution.
    let manifest = verify_setup(&destination, &file, release)?;
    Ok(DownloadedUpdate {
        path: destination,
        release: release.clone(),
        manifest,
        _file: file,
        _directories: directories,
    })
}

struct StagingFile(PathBuf);
impl Drop for StagingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Verify the platform and embedded product/version as well as network hashes.
fn verify_setup(path: &Path, file: &File, release: &UpdateRelease) -> Result<crate::Manifest> {
    ensure!(
        file.metadata()?.len() == release.size,
        "update length mismatch"
    );
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    ensure!(
        hex::encode(digest.finalize()).eq_ignore_ascii_case(&release.sha256),
        "update file changed after download"
    );
    reader.seek(SeekFrom::Start(0))?;
    let mut dos = [0u8; 64];
    reader
        .read_exact(&mut dos)
        .context("update is not a Windows Setup")?;
    ensure!(&dos[..2] == b"MZ", "update is not a Windows executable");
    let pe = u32::from_le_bytes(dos[60..64].try_into()?) as u64;
    ensure!(
        pe >= 64 && pe + 26 <= release.size,
        "invalid update PE header"
    );
    reader.seek(SeekFrom::Start(pe))?;
    let mut header = [0u8; 26];
    reader.read_exact(&mut header)?;
    ensure!(
        &header[..4] == b"PE\0\0" && u16::from_le_bytes(header[4..6].try_into()?) == 0x8664,
        "update is not a Windows x64 executable"
    );
    ensure!(
        u16::from_le_bytes(header[24..26].try_into()?) == 0x20b,
        "update is not a PE32+ executable"
    );
    let (_, manifest) = crate::embedded(path)?.context("update has no embedded Avelin package")?;
    ensure!(
        manifest.app_id == release.app_id && manifest.app_version == release.version,
        "downloaded Setup product/version differs from update feed"
    );
    Ok(manifest)
}

#[cfg(windows)]
fn pin_directories(parent: &Path) -> Result<Vec<File>> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    let mut prefixes: Vec<_> = parent.ancestors().collect();
    prefixes.reverse();
    let mut guards = Vec::new();
    for path in prefixes {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .custom_flags(0x0220_0000)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_dir() && metadata.file_attributes() & 0x400 == 0,
            "update parent is not a regular directory"
        );
        guards.push(file);
    }
    Ok(guards)
}

#[cfg(not(windows))]
fn pin_directories(parent: &Path) -> Result<Vec<File>> {
    crate::reject_reparse(parent)?;
    Ok(vec![File::open(parent)?])
}

fn open_pinned(path: &Path) -> Result<File> {
    crate::reject_reparse(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1).custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    ensure!(file.metadata()?.is_file(), "update is not a regular file");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            file.metadata()?.file_attributes() & 0x400 == 0,
            "update file is a reparse point"
        );
        require_single_link(&file)?;
    }
    Ok(file)
}

#[cfg(all(windows, test))]
fn publish_new(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .read(true)
        .access_mode(0x8001_0000)
        .share_mode(1)
        .open(source)?;
    let directories = pin_directories(destination.parent().context("output parent")?)?;
    publish_open_file(&file, source, destination, directories.last())
}

#[cfg(windows)]
fn publish_open_file(
    file: &File,
    source: &Path,
    destination: &Path,
    parent: Option<&File>,
) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::{ffi::c_void, os::windows::io::AsRawHandle, ptr};
    #[repr(C)]
    struct RenameInformation {
        replace: u8,
        root: *mut c_void,
        length: u32,
        name: [u16; 1],
    }
    #[repr(C)]
    #[derive(Default)]
    struct IoStatusBlock {
        status: usize,
        information: usize,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            file: *mut c_void,
            status: *mut IoStatusBlock,
            information: *const c_void,
            length: u32,
            class: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }
    let _parent = parent.context("update publication requires its pinned parent")?;
    ensure!(
        source.parent() == destination.parent(),
        "update publication must stay in its original directory"
    );
    let name: Vec<u16> = destination
        .file_name()
        .context("output filename")?
        .encode_wide()
        .collect();
    let offset = std::mem::offset_of!(RenameInformation, name);
    let length = std::mem::size_of::<RenameInformation>() + name.len() * 2;
    // usize storage guarantees native alignment for the variable-size header.
    let mut storage = vec![0usize; length.div_ceil(std::mem::size_of::<usize>())];
    let information = storage.as_mut_ptr().cast::<RenameInformation>();
    unsafe {
        (*information).replace = 0;
        // A simple filename with NULL RootDirectory renames in the source's
        // *existing* parent. It avoids reopening any directory with WRITE_DATA,
        // so strict no-WRITE/no-DELETE ancestor guards remain held throughout.
        (*information).root = ptr::null_mut();
        (*information).length = (name.len() * 2) as u32;
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            storage.as_mut_ptr().cast::<u8>().add(offset).cast(),
            name.len(),
        );
    }
    let mut status = IoStatusBlock::default();
    let result = unsafe {
        NtSetInformationFile(
            file.as_raw_handle().cast(),
            &mut status,
            information.cast(),
            length as u32,
            10,
        )
    };
    ensure!(
        result >= 0,
        "publish new update: {}",
        std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(result) } as i32)
    );
    Ok(())
}

#[cfg(not(windows))]
fn publish_open_file(
    _file: &File,
    source: &Path,
    destination: &Path,
    _parent: Option<&File>,
) -> Result<()> {
    fs::hard_link(source, destination).context("publish new update without replacement")?;
    fs::remove_file(source)?;
    Ok(())
}

#[cfg(all(not(windows), test))]
fn publish_new(source: &Path, destination: &Path) -> Result<()> {
    publish_open_file(&File::open(source)?, source, destination, None)
}

#[cfg(windows)]
fn require_single_link(file: &File) -> Result<()> {
    use std::{ffi::c_void, os::windows::io::AsRawHandle};
    #[repr(C)]
    #[derive(Default)]
    struct FileInformation {
        attributes: u32,
        creation: [u32; 2],
        access: [u32; 2],
        write: [u32; 2],
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(file: *mut c_void, information: *mut FileInformation) -> i32;
    }
    let mut information = FileInformation::default();
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) } != 0,
        "inspect update file: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        information.links == 1,
        "update executable must have exactly one hard link"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpsUrl {
    host: String,
    port: u16,
    target: String,
}

impl HttpsUrl {
    fn parse(url: &str) -> Result<Self> {
        ensure!(
            url.len() <= 8192 && url.is_ascii(),
            "update URL must be ASCII and at most 8192 bytes (percent-encode non-ASCII paths)"
        );
        ensure!(
            !url.bytes().any(|b| b <= 0x20 || b == 0x7f) && !url.contains(['\\', '#']),
            "update URL contains forbidden characters"
        );
        let rest = url
            .strip_prefix("https://")
            .context("update URLs must use https://")?;
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..end];
        ensure!(
            !authority.is_empty() && !authority.contains(['@', '%']),
            "update URL credentials/encoded host are not allowed"
        );
        let (host, port) = if authority.starts_with('[') {
            let end = authority.find(']').context("invalid IPv6 update URL")?;
            let host = &authority[1..end];
            host.parse::<std::net::Ipv6Addr>()
                .context("invalid IPv6 update host")?;
            let suffix = &authority[end + 1..];
            let port = if suffix.is_empty() {
                443
            } else {
                suffix
                    .strip_prefix(':')
                    .context("invalid update port")?
                    .parse::<u16>()?
            };
            (host.to_owned(), port)
        } else {
            let (host, port) = match authority.split_once(':') {
                Some((host, port)) => (host, port.parse::<u16>()?),
                None => (authority, 443),
            };
            ensure!(
                host.len() <= 253
                    && host.split('.').all(|part| !part.is_empty()
                        && part.len() <= 63
                        && !part.starts_with('-')
                        && !part.ends_with('-')
                        && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')),
                "invalid update host"
            );
            (host.to_ascii_lowercase(), port)
        };
        ensure!(port > 0, "invalid update port");
        let tail = &rest[end..];
        let target = if tail.starts_with('/') {
            tail.to_owned()
        } else {
            format!("/{tail}")
        };
        let bytes = target.as_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'%' {
                ensure!(
                    bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                        && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit),
                    "invalid URL percent escape"
                );
            }
        }
        Ok(Self { host, port, target })
    }

    fn origin(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 443 {
            format!("https://{host}")
        } else {
            format!("https://{host}:{}", self.port)
        }
    }

    fn redirect(&self, location: &str) -> Result<Self> {
        ensure!(!location.is_empty(), "empty update redirect");
        let url = if location.starts_with("//") {
            format!("https:{location}")
        } else if location.contains("://") {
            location.to_owned()
        } else if location.starts_with('/') {
            format!("{}{location}", self.origin())
        } else if location.starts_with('?') {
            format!(
                "{}{}{location}",
                self.origin(),
                self.target.split('?').next().unwrap()
            )
        } else {
            ensure!(
                !location.split('/').next().unwrap().contains(':'),
                "unsupported update redirect scheme"
            );
            let path = self.target.split('?').next().unwrap();
            let directory = path
                .rsplit_once('/')
                .map(|(directory, _)| directory)
                .unwrap_or("");
            format!("{}{directory}/{location}", self.origin())
        };
        Self::parse(&url)
    }
}

struct Response {
    status: u32,
    location: Option<String>,
    length: Option<u64>,
    body: Box<dyn Read>,
}
trait Transport {
    fn get(&self, url: &HttpsUrl) -> Result<Response>;
}
struct NativeTransport;

#[cfg(test)]
fn transfer(
    transport: &impl Transport,
    url: &str,
    limit: u64,
    exact: Option<u64>,
    writer: &mut impl Write,
) -> Result<String> {
    transfer_cancel(transport, url, limit, exact, writer, &NEVER_CANCEL)
}

fn transfer_cancel(
    transport: &impl Transport,
    url: &str,
    limit: u64,
    exact: Option<u64>,
    writer: &mut impl Write,
    cancel: &AtomicBool,
) -> Result<String> {
    let started = Instant::now();
    let mut url = HttpsUrl::parse(url)?;
    let mut response = None;
    for hop in 0..=MAX_REDIRECTS {
        ensure!(!cancel.load(Ordering::SeqCst), "update request cancelled");
        ensure!(
            started.elapsed() < REQUEST_DEADLINE,
            "update request timed out"
        );
        let result = transport.get(&url)?;
        ensure!(!cancel.load(Ordering::SeqCst), "update request cancelled");
        if matches!(result.status, 301 | 302 | 303 | 307 | 308) {
            ensure!(hop < MAX_REDIRECTS, "too many update redirects");
            url = url.redirect(
                result
                    .location
                    .as_deref()
                    .context("update redirect has no Location")?,
            )?;
        } else {
            response = Some(result);
            break;
        }
    }
    let mut response = response.context("update redirect loop")?;
    ensure!(
        response.status == 200,
        "update server returned HTTP {}",
        response.status
    );
    if let Some(length) = response.length {
        ensure!(length <= limit, "update response exceeds size limit");
        if let Some(exact) = exact {
            ensure!(length == exact, "update Content-Length differs from feed");
        }
    }
    let mut total = 0u64;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        ensure!(!cancel.load(Ordering::SeqCst), "update download cancelled");
        ensure!(
            started.elapsed() < REQUEST_DEADLINE,
            "update download timed out"
        );
        let count = response
            .body
            .read(&mut buffer)
            .context("read HTTPS update response")?;
        ensure!(!cancel.load(Ordering::SeqCst), "update download cancelled");
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .context("update size overflow")?;
        ensure!(total <= limit, "update response exceeds size limit");
        writer.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
    }
    if let Some(length) = response.length {
        ensure!(total == length, "truncated update response");
    }
    if let Some(exact) = exact {
        ensure!(total == exact, "downloaded update length differs from feed");
    }
    Ok(hex::encode(digest.finalize()))
}

#[cfg(not(windows))]
impl Transport for NativeTransport {
    fn get(&self, _: &HttpsUrl) -> Result<Response> {
        anyhow::bail!("online updates currently require Windows")
    }
}

#[cfg(windows)]
mod winhttp {
    use super::*;
    use std::{ffi::c_void, ptr};
    type Handle = *mut c_void;
    #[link(name = "winhttp")]
    unsafe extern "system" {
        fn WinHttpOpen(
            agent: *const u16,
            access: u32,
            proxy: *const u16,
            bypass: *const u16,
            flags: u32,
        ) -> Handle;
        fn WinHttpConnect(session: Handle, server: *const u16, port: u16, reserved: u32) -> Handle;
        fn WinHttpOpenRequest(
            connection: Handle,
            verb: *const u16,
            object: *const u16,
            version: *const u16,
            referer: *const u16,
            accept: *const *const u16,
            flags: u32,
        ) -> Handle;
        fn WinHttpSetTimeouts(
            handle: Handle,
            resolve: i32,
            connect: i32,
            send: i32,
            receive: i32,
        ) -> i32;
        fn WinHttpSetOption(handle: Handle, option: u32, buffer: *const c_void, length: u32)
        -> i32;
        fn WinHttpSendRequest(
            request: Handle,
            headers: *const u16,
            headers_len: u32,
            optional: *const c_void,
            optional_len: u32,
            total: u32,
            context: usize,
        ) -> i32;
        fn WinHttpReceiveResponse(request: Handle, reserved: *mut c_void) -> i32;
        fn WinHttpQueryHeaders(
            request: Handle,
            level: u32,
            name: *const u16,
            buffer: *mut c_void,
            length: *mut u32,
            index: *mut u32,
        ) -> i32;
        fn WinHttpReadData(request: Handle, buffer: *mut c_void, count: u32, read: *mut u32)
        -> i32;
        fn WinHttpCloseHandle(handle: Handle) -> i32;
    }

    struct Internet(Handle);
    impl Internet {
        fn new(handle: Handle, label: &str) -> Result<Self> {
            ensure!(
                !handle.is_null(),
                "{label}: {}",
                std::io::Error::last_os_error()
            );
            Ok(Self(handle))
        }
        fn option(&self, option: u32, value: u32) -> Result<()> {
            ensure!(
                unsafe { WinHttpSetOption(self.0, option, (&value as *const u32).cast(), 4) } != 0,
                "configure HTTPS client: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        }
    }
    impl Drop for Internet {
        fn drop(&mut self) {
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }
    struct Body {
        request: Internet,
        _connection: Internet,
        _session: Internet,
    }
    impl Read for Body {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if bytes.is_empty() {
                return Ok(0);
            }
            let mut count = 0;
            if unsafe {
                WinHttpReadData(
                    self.request.0,
                    bytes.as_mut_ptr().cast(),
                    bytes.len().min(u32::MAX as usize) as u32,
                    &mut count,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(count as usize)
        }
    }
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn header(request: &Internet, level: u32) -> Result<Option<String>> {
        let mut length = 0;
        let result = unsafe {
            WinHttpQueryHeaders(
                request.0,
                level,
                ptr::null(),
                ptr::null_mut(),
                &mut length,
                ptr::null_mut(),
            )
        };
        if result == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(12150) {
                return Ok(None);
            }
            ensure!(
                error.raw_os_error() == Some(122),
                "read update response headers: {error}"
            );
        }
        ensure!(
            length <= 32 * 1024 && length % 2 == 0,
            "update response header too large"
        );
        let mut buffer = vec![0u16; length as usize / 2 + 1];
        ensure!(
            unsafe {
                WinHttpQueryHeaders(
                    request.0,
                    level,
                    ptr::null(),
                    buffer.as_mut_ptr().cast(),
                    &mut length,
                    ptr::null_mut(),
                )
            } != 0,
            "read update response header: {}",
            std::io::Error::last_os_error()
        );
        buffer.truncate(length as usize / 2);
        while buffer.last() == Some(&0) {
            buffer.pop();
        }
        Ok(Some(String::from_utf16(&buffer)?))
    }

    impl Transport for NativeTransport {
        fn get(&self, url: &HttpsUrl) -> Result<Response> {
            let agent = wide("Avelin-Updater/1");
            let session = Internet::new(
                unsafe { WinHttpOpen(agent.as_ptr(), 4, ptr::null(), ptr::null(), 0) },
                "open HTTPS session",
            )?;
            ensure!(
                unsafe { WinHttpSetTimeouts(session.0, 5_000, 10_000, 10_000, 15_000) } != 0,
                "set HTTPS timeouts: {}",
                std::io::Error::last_os_error()
            );
            // Explicitly allow TLS 1.2/1.3 only, keeping normal OS certificate
            // validation. Never set SECURITY_FLAG_IGNORE_* or import a CA.
            session.option(84, 0x0000_2800)?;
            let host = wide(&url.host);
            let connection = Internet::new(
                unsafe { WinHttpConnect(session.0, host.as_ptr(), url.port, 0) },
                "connect HTTPS server",
            )?;
            let verb = wide("GET");
            let object = wide(&url.target);
            let request = Internet::new(
                unsafe {
                    WinHttpOpenRequest(
                        connection.0,
                        verb.as_ptr(),
                        object.as_ptr(),
                        ptr::null(),
                        ptr::null(),
                        ptr::null(),
                        0x0080_0000,
                    )
                },
                "open HTTPS request",
            )?;
            request.option(63, 1 | 2 | 4)?; // cookies, redirects and automatic authentication disabled
            request.option(77, 2)?; // never automatically send Windows credentials
            ensure!(
                unsafe { WinHttpSendRequest(request.0, ptr::null(), 0, ptr::null(), 0, 0, 0) } != 0,
                "send HTTPS update request: {}",
                std::io::Error::last_os_error()
            );
            ensure!(
                unsafe { WinHttpReceiveResponse(request.0, ptr::null_mut()) } != 0,
                "receive HTTPS update response: {}",
                std::io::Error::last_os_error()
            );
            let status = header(&request, 19)?
                .context("update response has no status")?
                .parse::<u32>()?;
            let location = if matches!(status, 301 | 302 | 303 | 307 | 308) {
                header(&request, 33)?
            } else {
                None
            };
            let length = header(&request, 5)?
                .map(|text| text.parse::<u64>())
                .transpose()
                .context("invalid update Content-Length")?;
            Ok(Response {
                status,
                location,
                length,
                body: Box::new(Body {
                    request,
                    _connection: connection,
                    _session: session,
                }),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque, io::Cursor};

    struct MockTransport(RefCell<VecDeque<Response>>, RefCell<Vec<HttpsUrl>>);
    impl MockTransport {
        fn new(responses: Vec<Response>) -> Self {
            Self(RefCell::new(responses.into()), RefCell::new(Vec::new()))
        }
        fn bytes(bytes: Vec<u8>) -> Self {
            let length = bytes.len() as u64;
            Self::new(vec![Response {
                status: 200,
                location: None,
                length: Some(length),
                body: Box::new(Cursor::new(bytes)),
            }])
        }
    }
    impl Transport for MockTransport {
        fn get(&self, url: &HttpsUrl) -> Result<Response> {
            self.1.borrow_mut().push(url.clone());
            self.0
                .borrow_mut()
                .pop_front()
                .context("unexpected mock request")
        }
    }
    fn config() -> UpdateConfig {
        UpdateConfig {
            feed_url: "https://updates.example.com/stable.json".into(),
            channel: "stable".into(),
        }
    }
    fn release() -> UpdateRelease {
        UpdateRelease {
            schema_version: 1,
            app_id: "com.example.app".into(),
            version: "2.0.0".into(),
            channel: "stable".into(),
            platform: "windows-x64".into(),
            setup_url: "https://cdn.example.com/Setup.exe".into(),
            size: 3,
            sha256: crate::hash_bytes(b"abc"),
        }
    }
    fn response(
        status: u32,
        location: Option<&str>,
        length: Option<u64>,
        bytes: &[u8],
    ) -> Response {
        Response {
            status,
            location: location.map(str::to_owned),
            length,
            body: Box::new(Cursor::new(bytes.to_vec())),
        }
    }

    #[test]
    fn only_canonical_credential_free_https_urls_are_accepted() {
        for url in [
            "http://example.com/feed",
            "https://user:pass@example.com/feed",
            "https://@example.com/feed",
            "https://example.com\\@evil.test/x",
            "https://example.com/#fragment",
            "https://example.com/a\r\nb",
            "https://example.com/%xx",
            "https://%65xample.com/feed",
            "https://example.com:0/x",
            "https://example.com:65536/x",
            "https://-host.example/x",
            "https://example..com/x",
            "https://example.com/测试",
            "file:///C:/Setup.exe",
            "https://example.com:443:99/x",
        ] {
            assert!(HttpsUrl::parse(url).is_err(), "{url}");
        }
        for url in [
            "https://example.com",
            "https://example.com:8443/a?q=%20",
            "https://[::1]:8443/feed",
            "https://example.com/path%20name",
        ] {
            HttpsUrl::parse(url).unwrap();
        }
    }

    #[test]
    fn update_metadata_is_strict_and_bound_to_product_channel_platform_and_precedence() {
        let release = release();
        let mock = || MockTransport::bytes(serde_json::to_vec(&release).unwrap());
        assert!(
            check_with(&mock(), &config(), "com.example.app", "1.0.0")
                .unwrap()
                .is_some()
        );
        assert!(
            check_with(&mock(), &config(), "com.example.app", "2.0.0")
                .unwrap()
                .is_none()
        );
        assert!(
            check_with(&mock(), &config(), "com.example.app", "3.0.0")
                .unwrap()
                .is_none()
        );
        assert!(check_with(&mock(), &config(), "com.other.app", "1.0.0").is_err());
        let mut wrong_channel = config();
        wrong_channel.channel = "beta".into();
        assert!(check_with(&mock(), &wrong_channel, "com.example.app", "1.0.0").is_err());
        let mut metadata = serde_json::to_value(&release).unwrap();
        metadata["unexpected"] = true.into();
        assert!(
            check_with(
                &MockTransport::bytes(serde_json::to_vec(&metadata).unwrap()),
                &config(),
                "com.example.app",
                "1.0.0"
            )
            .is_err()
        );
        for field in ["schema_version", "platform", "size", "sha256", "version"] {
            let mut metadata = serde_json::to_value(&release).unwrap();
            metadata[field] = match field {
                "schema_version" => 2.into(),
                "size" => (MAX_SETUP_BYTES + 1).into(),
                "platform" => "linux-x64".into(),
                "sha256" => "abc".into(),
                _ => "not-semver".into(),
            };
            assert!(
                serde_json::from_value::<UpdateRelease>(metadata)
                    .unwrap()
                    .validate()
                    .is_err(),
                "{field}"
            );
        }
        let mut prerelease = release.clone();
        prerelease.version = "2.0.0-rc.1".into();
        assert!(prerelease.validate().is_err());
        prerelease.channel = "preview".into();
        prerelease.validate().unwrap();
        let mut build_metadata = release;
        build_metadata.version = "2.0.0+new".into();
        let mock = MockTransport::bytes(serde_json::to_vec(&build_metadata).unwrap());
        assert!(
            check_with(&mock, &config(), "com.example.app", "2.0.0+old")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn redirects_reject_downgrades_credentials_loops_and_unhandled_statuses_before_body() {
        for location in [
            "http://example.com/data",
            "https://user@evil.test/data",
            "file:///payload",
            "javascript:alert(1)",
        ] {
            let mock = MockTransport::new(vec![response(302, Some(location), None, b"")]);
            assert!(transfer(&mock, &config().feed_url, 100, None, &mut Vec::new()).is_err());
            assert_eq!(mock.1.borrow().len(), 1);
        }
        let mock = MockTransport::new(
            (0..=MAX_REDIRECTS)
                .map(|_| response(302, Some("/again"), None, b""))
                .collect(),
        );
        assert!(transfer(&mock, &config().feed_url, 100, None, &mut Vec::new()).is_err());
        assert_eq!(mock.1.borrow().len(), MAX_REDIRECTS + 1);
        for status in [201, 204, 206, 304, 401, 404, 500] {
            let mock = MockTransport::new(vec![response(status, None, Some(3), b"abc")]);
            let mut output = Vec::new();
            assert!(transfer(&mock, &config().feed_url, 100, None, &mut output).is_err());
            assert!(output.is_empty());
        }
    }

    #[test]
    fn https_cdn_and_relative_redirects_are_validated_every_hop() {
        let mock = MockTransport::new(vec![
            response(302, Some("/feeds/current.json"), None, b""),
            response(307, Some("https://cdn.example.com/feed.json"), None, b""),
            response(200, None, Some(3), b"abc"),
        ]);
        let mut output = Vec::new();
        assert_eq!(
            transfer(&mock, &config().feed_url, 3, Some(3), &mut output).unwrap(),
            crate::hash_bytes(b"abc")
        );
        assert_eq!(output, b"abc");
        let requests = mock.1.borrow();
        assert_eq!(requests[1].target, "/feeds/current.json");
        assert_eq!(requests[2].host, "cdn.example.com");
        let base = HttpsUrl::parse("https://example.com/a/feed.json?old").unwrap();
        assert_eq!(
            base.redirect("next.json?new").unwrap().target,
            "/a/next.json?new"
        );
        assert_eq!(base.redirect("?new").unwrap().target, "/a/feed.json?new");
        assert_eq!(
            base.redirect("//cdn.example.com/file").unwrap().host,
            "cdn.example.com"
        );
    }

    #[test]
    fn transfer_enforces_declared_streamed_exact_and_truncated_lengths() {
        for (length, bytes, limit, exact) in [
            (Some(100), b"abc".as_slice(), 3, None),
            (Some(4), b"abc".as_slice(), 10, None),
            (None, b"abcd".as_slice(), 3, None),
            (Some(3), b"abc".as_slice(), 10, Some(4)),
            (None, b"abc".as_slice(), 10, Some(4)),
        ] {
            let mock = MockTransport::new(vec![response(200, None, length, bytes)]);
            assert!(transfer(&mock, &config().feed_url, limit, exact, &mut Vec::new()).is_err());
        }
        let oversized = MockTransport::bytes(vec![b' '; MAX_FEED_BYTES as usize + 1]);
        assert!(check_with(&oversized, &config(), "com.example.app", "1.0.0").is_err());
    }

    #[test]
    fn cancellation_stops_before_network_or_publication_and_discards_inflight_bytes() {
        let cancelled = AtomicBool::new(true);
        let mock = MockTransport::new(vec![]);
        assert!(
            check_with_cancel(&mock, &config(), "com.example.app", "1.0.0", &cancelled).is_err()
        );
        assert!(mock.1.borrow().is_empty());
        let fixture = Fixture::new();
        let output = fixture.0.join("cancelled.exe");
        assert!(download_with_cancel(&mock, &release(), &output, &cancelled).is_err());
        assert!(!output.exists());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
        struct CancelReader(std::sync::Arc<AtomicBool>);
        impl Read for CancelReader {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                bytes[0] = b'x';
                self.0.store(true, Ordering::SeqCst);
                Ok(1)
            }
        }
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let mock = MockTransport::new(vec![Response {
            status: 200,
            location: None,
            length: None,
            body: Box::new(CancelReader(flag.clone())),
        }]);
        let mut output = Vec::new();
        assert!(transfer_cancel(&mock, &config().feed_url, 10, None, &mut output, &flag).is_err());
        assert!(output.is_empty());
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static ID: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "avelin-update-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn setup(&self) -> (UpdateRelease, Vec<u8>) {
            let mut runtime = vec![0u8; 512];
            runtime[..2].copy_from_slice(b"MZ");
            runtime[60..64].copy_from_slice(&64u32.to_le_bytes());
            runtime[64..68].copy_from_slice(b"PE\0\0");
            runtime[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
            runtime[84..86].copy_from_slice(&240u16.to_le_bytes());
            runtime[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
            fs::write(self.0.join("runtime.bin"), runtime).unwrap();
            fs::write(self.0.join("backend.bin"), b"non-executed fixture payload").unwrap();
            let manifest: crate::Manifest = serde_json::from_value(serde_json::json!({ "schema_version": 1, "app_id": "com.example.app", "app_name": "Example", "app_version": "2.0.0", "entry": "app.exe", "directory_name": "Example", "files": [{ "path": "app.exe", "size": 3, "sha256": crate::hash_bytes(b"abc") }] })).unwrap();
            let setup = self.0.join("source.exe");
            crate::assemble(
                &self.0.join("runtime.bin"),
                &self.0.join("backend.bin"),
                &manifest,
                &setup,
            )
            .unwrap();
            let release =
                release_for_setup(&setup, "https://cdn.example.com/Setup.exe", "stable").unwrap();
            (release, fs::read(setup).unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Ok(entries) = fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn verified_download_binds_embedded_identity_and_publishes_without_overwrite() {
        let fixture = Fixture::new();
        let (release, bytes) = fixture.setup();
        let destination = fixture.0.join("download.exe");
        let package =
            download_with(&MockTransport::bytes(bytes.clone()), &release, &destination).unwrap();
        assert_eq!(fs::read(&package.path).unwrap(), bytes);
        assert!(
            download_with(&MockTransport::bytes(bytes.clone()), &release, &destination).is_err()
        );
        #[cfg(windows)]
        {
            assert!(OpenOptions::new().write(true).open(&destination).is_err());
            assert!(fs::rename(&destination, fixture.0.join("renamed.exe")).is_err());
        }
        drop(package);
        let mut wrong = release;
        wrong.app_id = "com.other.app".into();
        assert!(
            download_with(
                &MockTransport::bytes(bytes),
                &wrong,
                &fixture.0.join("wrong.exe")
            )
            .is_err()
        );
        assert!(!fixture.0.join("wrong.exe").exists());
        assert!(
            !fs::read_dir(&fixture.0)
                .unwrap()
                .flatten()
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "part"))
        );
    }

    #[test]
    fn corrupted_or_non_setup_downloads_never_publish_and_staging_is_removed() {
        let fixture = Fixture::new();
        let (release, mut bytes) = fixture.setup();
        bytes[400] ^= 1;
        let output = fixture.0.join("corrupt.exe");
        assert!(download_with(&MockTransport::bytes(bytes), &release, &output).is_err());
        assert!(!output.exists());
        let fake = super::tests::release();
        assert!(download_with(&MockTransport::bytes(b"abc".to_vec()), &fake, &output).is_err());
        assert!(!output.exists());
        assert!(
            !fs::read_dir(&fixture.0)
                .unwrap()
                .flatten()
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "part"))
        );
    }

    #[test]
    fn atomic_publish_does_not_replace_a_racing_existing_destination() {
        let fixture = Fixture::new();
        let source = fixture.0.join("new.part");
        let destination = fixture.0.join("existing.exe");
        fs::write(&source, b"new").unwrap();
        fs::write(&destination, b"keep").unwrap();
        assert!(publish_new(&source, &destination).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"keep");
        assert_eq!(fs::read(&source).unwrap(), b"new");
    }

    #[test]
    fn pinned_directory_allows_atomic_handle_publication() {
        let fixture = Fixture::new();
        let source = fixture.0.join("link-source.part");
        let destination = fixture.0.join("link-result.exe");
        fs::write(&source, b"new").unwrap();
        let _directories = pin_directories(&fixture.0).unwrap();
        publish_new(&source, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new");
    }

    /// A real TLS handshake to an in-memory self-signed server, without touching
    /// the Windows certificate stores. The ordinary WinHTTP verifier must fail.
    #[cfg(windows)]
    #[test]
    fn native_https_rejects_untrusted_local_tls_without_modifying_trust() {
        use std::{
            io::{BufRead, BufReader},
            os::windows::process::CommandExt,
            process::{Command, Stdio},
        };
        let script = r#"
$ErrorActionPreference = 'Stop'
$rsa = [System.Security.Cryptography.RSA]::Create(2048)
$request = [System.Security.Cryptography.X509Certificates.CertificateRequest]::new('CN=localhost', $rsa, [System.Security.Cryptography.HashAlgorithmName]::SHA256, [System.Security.Cryptography.RSASignaturePadding]::Pkcs1)
$certificate = $request.CreateSelfSigned([DateTimeOffset]::Now.AddMinutes(-1), [DateTimeOffset]::Now.AddMinutes(5))
$pfx = $certificate.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Pfx)
$certificate.Dispose()
$certificate = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($pfx)
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$listener.Start()
[Console]::WriteLine($listener.LocalEndpoint.Port)
[Console]::Out.Flush()
try {
  $client = $listener.AcceptTcpClient()
  $client.ReceiveTimeout = 15000; $client.SendTimeout = 15000
  $tls = [System.Net.Security.SslStream]::new($client.GetStream(), $false)
  try { $tls.AuthenticateAsServer($certificate, $false, [System.Security.Authentication.SslProtocols]::Tls12, $false) } catch { [Console]::Error.WriteLine($_.Exception.ToString()) }
  $tls.Dispose(); $client.Dispose()
} finally { $listener.Stop(); $certificate.Dispose(); $rsa.Dispose() }
"#;
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .creation_flags(0x0800_0000)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut port = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut port)
            .unwrap();
        if port.trim().parse::<u16>().is_err() {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "TLS fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let result = NativeTransport.get(
            &HttpsUrl::parse(&format!("https://localhost:{}/feed.json", port.trim())).unwrap(),
        );
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        let error = match result {
            Ok(_) => panic!("untrusted TLS unexpectedly accepted"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("12175"),
            "expected ERROR_WINHTTP_SECURE_FAILURE, got {error}; server: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
