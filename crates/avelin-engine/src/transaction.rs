use crate::{
    EngineObserver, EngineOutcome, EngineRequest, owned_files as owned,
    system::{self, Registration},
};
use anyhow::{Context, Result, bail, ensure};
use avelin_core::{Manifest, Operation, RECEIPT_NAME, Receipt, ShortcutKind, hash_bytes};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MARKER: &str = ".avelin/upgrade-state.json";
fn text(path: &Path) -> Result<&str> {
    path.to_str().context("deployment path is not Unicode")
}
#[derive(Debug, Clone)]
struct WindowsPath(Vec<u16>);
impl Ord for WindowsPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
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
        let compared = unsafe {
            CompareStringOrdinal(
                self.0.as_ptr(),
                self.0.len() as i32,
                other.0.as_ptr(),
                other.0.len() as i32,
                1,
            )
        };
        match compared {
            1 => std::cmp::Ordering::Less,
            2 => std::cmp::Ordering::Equal,
            3 => std::cmp::Ordering::Greater,
            _ => panic!("Windows ordinal comparison failed for valid bounded paths"),
        }
    }
}
impl PartialOrd for WindowsPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for WindowsPath {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for WindowsPath {}
fn key(path: &Path) -> WindowsPath {
    WindowsPath(
        path.to_string_lossy()
            .replace('/', "\\")
            .encode_utf16()
            .collect(),
    )
}
fn inside(path: &Path, parent: &Path) -> bool {
    let path: Vec<_> = path.components().collect();
    let parent: Vec<_> = parent.components().collect();
    path.len() >= parent.len()
        && path
            .iter()
            .zip(parent.iter())
            .all(|(a, b)| key(Path::new(a.as_os_str())) == key(Path::new(b.as_os_str())))
}
fn read(path: &Path) -> Result<Vec<u8>> {
    owned::read_bytes(text(path)?, 32 * 1024 * 1024)
}
fn digest(path: &Path) -> Result<String> {
    owned::hash_file(text(path)?)
}
fn confirm(path: &Path, expected: &str) -> Result<()> {
    ensure!(
        digest(path)?.eq_ignore_ascii_case(expected),
        "owned resource changed: {}",
        path.display()
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resource {
    path: PathBuf,
    sha256: String,
    size: u64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    schema_version: u32,
    task_id: String,
    backup: Vec<Resource>,
    preserved: Vec<Resource>,
    registration: Registration,
}
struct Staged {
    source: PathBuf,
    resource: Resource,
}
struct Backup {
    source: PathBuf,
    resource: Resource,
}
struct Prepared {
    stage: Vec<Staged>,
    backup: Vec<Backup>,
    preserved: Vec<Resource>,
    registration: Option<Registration>,
    next_registration: Option<Registration>,
    shortcut_evidence: Vec<u8>,
}

fn validate_receipt(
    receipt: &Receipt,
    manifest: &Manifest,
    request: &EngineRequest<'_>,
) -> Result<()> {
    ensure!(
        receipt.schema_version == avelin_core::SCHEMA_VERSION
            && receipt.app_id == manifest.app_id
            && receipt.app_version == manifest.app_version
            && receipt.files == manifest.files
            && receipt.manifest_sha256 == hash_bytes(&serde_json::to_vec(manifest)?)
            && receipt.user_sid == request.plan.user_sid
            && key(Path::new(&receipt.install_dir)) == key(&request.plan.install_dir),
        "receipt does not match the current user, product, manifest and destination"
    );
    avelin_core::safe_id(&receipt.installation_id)?;
    avelin_core::safe_id(&receipt.task_id)?;
    avelin_core::safe_id(&receipt.commit_id)?;
    Ok(())
}

fn allowed_old(
    manifest: &Manifest,
    receipt: &Receipt,
    receipt_bytes: &[u8],
    manifest_bytes: &[u8],
    root: &Path,
) -> Result<BTreeMap<WindowsPath, (PathBuf, String)>> {
    let mut allowed = BTreeMap::new();
    for item in &manifest.files {
        let path = root.join(&item.path);
        allowed.insert(key(&path), (path, item.sha256.clone()));
    }
    for (relative, hash) in [
        (RECEIPT_NAME, hash_bytes(receipt_bytes)),
        (".avelin/manifest.json", hash_bytes(manifest_bytes)),
        (
            ".avelin/avelin-app.exe",
            manifest
                .runtime_sha256
                .clone()
                .context("installed runtime hash missing")?,
        ),
        (
            "uninstall.exe",
            receipt
                .uninstaller_sha256
                .clone()
                .context("installed worker hash missing")?,
        ),
    ] {
        let path = root.join(relative);
        allowed.insert(key(&path), (path, hash));
    }
    let mut seen = BTreeSet::new();
    for shortcut in &receipt.shortcuts {
        let path = system::shortcut_path(&manifest.app_id, shortcut.kind)?;
        ensure!(
            seen.insert(shortcut.kind)
                && key(&path) == key(Path::new(&shortcut.path))
                && key(Path::new(&shortcut.target)) == key(&root.join(&manifest.entry)),
            "invalid shortcut ownership record"
        );
        ensure!(
            shortcut.sha256.len() == 64 && shortcut.sha256.bytes().all(|v| v.is_ascii_hexdigit()),
            "invalid shortcut digest"
        );
        allowed.insert(key(&path), (path, shortcut.sha256.clone()));
    }
    Ok(allowed)
}

fn validate_request(request: &EngineRequest<'_>) -> Result<()> {
    let plan = request.plan;
    plan.validate(request.manifest)?;
    validate_receipt(request.receipt, request.manifest, request)?;
    ensure!(
        request.session_dir.is_absolute() && request.session_dir.is_dir(),
        "session directory is unavailable"
    );
    avelin_core::reject_reparse(request.session_dir)?;
    ensure!(
        !inside(request.session_dir, &plan.install_dir)
            && !inside(&plan.install_dir, request.session_dir),
        "session and installation directories overlap"
    );
    ensure!(
        !plan.install_dir.join(MARKER).try_exists()?,
        "unfinished transaction requires inspection before any new operation"
    );
    ensure!(
        request.step_delay_ms <= 60_000,
        "step delay exceeds supported bound"
    );
    if let Some(point) = request.fault_point {
        ensure!(
            matches!(
                point,
                "after-staging"
                    | "before-registration"
                    | "after-registration"
                    | "before-commit"
                    | "after-commit"
                    | "rollback"
            ),
            "unknown native fault point"
        );
    }
    if let Some(point) = request.pause_point {
        ensure!(
            matches!(point, "prepared" | "after-first-publish" | "after-commit"),
            "unknown native pause point"
        );
    }
    if plan.operation != Operation::Uninstall {
        ensure!(
            request.receipt.task_id == plan.task_id && request.receipt.uninstaller_sha256.is_none(),
            "invalid new receipt candidate"
        );
        let payload = request.payload.context("install requires native payload")?;
        ensure!(
            payload.files.len() == request.manifest.files.len(),
            "native file count mismatch"
        );
        ensure!(
            request.manifest.runtime_sha256.as_deref()
                == Some(hash_bytes(payload.runtime).as_str()),
            "native runtime digest mismatch"
        );
        for (bytes, file) in payload.files.iter().zip(&request.manifest.files) {
            ensure!(
                bytes.len() as u64 == file.size && hash_bytes(bytes) == file.sha256,
                "native file integrity mismatch: {}",
                file.path
            );
        }
    }
    let mut seen = BTreeSet::new();
    for shortcut in &request.shortcuts {
        ensure!(
            seen.insert(shortcut.kind)
                && key(Path::new(&shortcut.path))
                    == key(&system::shortcut_path(&plan.app_id, shortcut.kind)?)
                && key(Path::new(&shortcut.target))
                    == key(&plan.install_dir.join(&request.manifest.entry)),
            "shortcut is outside its current-user destination"
        );
    }
    ensure!(
        seen.contains(&ShortcutKind::Desktop) == plan.desktop_shortcut
            && seen.contains(&ShortcutKind::StartMenu) == plan.start_menu_shortcut,
        "shortcut selection does not match the plan"
    );
    Ok(())
}

fn prepare(request: &EngineRequest<'_>, observer: &mut impl EngineObserver) -> Result<Prepared> {
    validate_request(request)?;
    let root = &request.plan.install_dir;
    let mut backup_resources = Vec::new();
    let mut preserved = Vec::new();
    let registration = match request.plan.operation {
        Operation::Install => {
            for relative in [RECEIPT_NAME, "uninstall.exe", ".avelin"]
                .into_iter()
                .chain(request.manifest.files.iter().map(|v| v.path.as_str()))
            {
                let path = root.join(relative);
                avelin_core::reject_reparse(&path)?;
                ensure!(
                    !path.try_exists()?,
                    "fresh install destination exists: {}",
                    path.display()
                );
            }
            for shortcut in &request.shortcuts {
                ensure!(
                    !Path::new(&shortcut.path).try_exists()?,
                    "shortcut already exists"
                );
            }
            system::verify_registration(&request.plan.app_id, None)?;
            None
        }
        Operation::Upgrade => {
            let receipt_bytes = read(&request.session_dir.join("previous-receipt.json"))?;
            let manifest_bytes = read(&request.session_dir.join("previous-manifest.json"))?;
            let old: Manifest = serde_json::from_slice(&manifest_bytes)?;
            let receipt: Receipt = serde_json::from_slice(&receipt_bytes)?;
            validate_receipt(&receipt, &old, request)?;
            request.plan.validate_upgrade(
                request.manifest,
                &old,
                &receipt,
                &hash_bytes(&receipt_bytes),
            )?;
            ensure!(
                request.receipt.installation_id == receipt.installation_id
                    && request.receipt.previous_commit_id.as_deref()
                        == Some(receipt.commit_id.as_str())
                    && request.receipt.task_id != receipt.task_id
                    && request.receipt.commit_id != receipt.commit_id,
                "new upgrade receipt does not bind a fresh commit to the previous installation"
            );
            let allowed = allowed_old(&old, &receipt, &receipt_bytes, &manifest_bytes, root)?;
            let inventory: Inventory = serde_json::from_slice(&read(
                &request.session_dir.join("upgrade-resources.json"),
            )?)?;
            ensure!(
                inventory.schema_version == 1 && inventory.task_id == request.plan.task_id,
                "upgrade inventory identity mismatch"
            );
            let expected = Registration::new(&old, root);
            ensure!(
                inventory.registration == expected,
                "upgrade registration snapshot mismatch"
            );
            system::verify_registration(&request.plan.app_id, Some(&expected))?;
            let mut included = BTreeSet::new();
            for item in &inventory.backup {
                ensure!(
                    included.insert(key(&item.path))
                        && allowed
                            .get(&key(&item.path))
                            .is_some_and(|(_, hash)| hash.eq_ignore_ascii_case(&item.sha256)),
                    "upgrade backup contains an unowned or duplicate resource"
                );
                confirm(&item.path, &item.sha256)?;
                ensure!(
                    fs::metadata(&item.path)?.len() == item.size,
                    "upgrade backup size changed"
                );
            }
            let mut preserved_keys = BTreeSet::new();
            for item in &inventory.preserved {
                ensure!(
                    preserved_keys.insert(key(&item.path))
                        && !included.contains(&key(&item.path))
                        && allowed.contains_key(&key(&item.path)),
                    "invalid preserved upgrade resource"
                );
                confirm(&item.path, &item.sha256)?;
            }
            for (name, (path, hash)) in &allowed {
                if path.try_exists()? {
                    let actual = digest(path)?;
                    ensure!(
                        (actual.eq_ignore_ascii_case(hash) && included.contains(name))
                            || (!actual.eq_ignore_ascii_case(hash)
                                && preserved_keys.contains(name)),
                        "upgrade inventory omits or misclassifies an existing owned resource"
                    );
                } else {
                    ensure!(
                        !included.contains(name) && !preserved_keys.contains(name),
                        "upgrade resource disappeared"
                    );
                }
            }
            for relative in [
                RECEIPT_NAME,
                "uninstall.exe",
                ".avelin/avelin-app.exe",
                ".avelin/manifest.json",
            ] {
                ensure!(
                    included.contains(&key(&root.join(relative))),
                    "upgrade requires every internal component backup"
                );
            }
            for path in request
                .manifest
                .files
                .iter()
                .map(|v| root.join(&v.path))
                .chain(request.shortcuts.iter().map(|v| PathBuf::from(&v.path)))
            {
                avelin_core::reject_reparse(&path)?;
                ensure!(
                    !path.try_exists()? || included.contains(&key(&path)),
                    "upgrade conflicts with an unknown or modified resource"
                );
            }
            backup_resources = inventory.backup;
            preserved = inventory.preserved;
            Some(expected)
        }
        Operation::Uninstall => {
            let receipt_bytes = read(&root.join(RECEIPT_NAME))?;
            let manifest_bytes = read(&root.join(".avelin/manifest.json"))?;
            let installed: Receipt = serde_json::from_slice(&receipt_bytes)?;
            ensure!(
                serde_json::to_value(&installed)? == serde_json::to_value(request.receipt)?,
                "uninstall receipt changed"
            );
            let installed_manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
            ensure!(
                hash_bytes(&serde_json::to_vec(&installed_manifest)?)
                    == request.plan.manifest_sha256,
                "installed manifest changed"
            );
            let allowed = allowed_old(
                request.manifest,
                request.receipt,
                &receipt_bytes,
                &manifest_bytes,
                root,
            )?;
            for (_, (path, hash)) in allowed {
                if path.try_exists()? {
                    let actual = digest(&path)?;
                    let item = Resource {
                        size: fs::metadata(&path)?.len(),
                        path,
                        sha256: actual.clone(),
                    };
                    if actual.eq_ignore_ascii_case(&hash) {
                        backup_resources.push(item);
                    } else {
                        preserved.push(item);
                    }
                }
            }
            for relative in [
                RECEIPT_NAME,
                "uninstall.exe",
                ".avelin/avelin-app.exe",
                ".avelin/manifest.json",
            ] {
                ensure!(
                    backup_resources
                        .iter()
                        .any(|v| key(&v.path) == key(&root.join(relative))),
                    "uninstall internal component is missing or changed"
                );
            }
            let expected = Registration::new(request.manifest, root);
            system::verify_registration(&request.plan.app_id, Some(&expected))?;
            Some(expected)
        }
    };
    let next_registration = (request.plan.operation != Operation::Uninstall)
        .then(|| Registration::new(request.manifest, root));
    let stage_dir = request.session_dir.join("staging");
    let backup_dir = request.session_dir.join("backup");
    owned::create_directory(text(&stage_dir)?)?;
    owned::create_directory(text(&backup_dir)?)?;
    if let Some(upgrade) = &request.plan.upgrade {
        ensure!(
            key(&stage_dir) == key(&upgrade.staging_dir)
                && key(&backup_dir) == key(&upgrade.backup_dir),
            "upgrade private directories do not match plan"
        );
    }
    let mut stage = Vec::new();
    let mut shortcut_evidence = String::from("[shortcuts]\r\n");
    if request.plan.operation != Operation::Uninstall {
        let payload = request.payload.context("native payload missing")?;
        let mut add = |name: &str, path: PathBuf, bytes: &[u8]| -> Result<String> {
            let source = stage_dir.join(name);
            let sha256 = owned::write_new_bytes(text(&source)?, bytes)?;
            stage.push(Staged {
                source,
                resource: Resource {
                    path,
                    sha256: sha256.clone(),
                    size: bytes.len() as u64,
                },
            });
            Ok(sha256)
        };
        for (index, (bytes, file)) in payload
            .files
            .iter()
            .zip(&request.manifest.files)
            .enumerate()
        {
            if observer.checkpoint()? {
                return Err(Cancelled.into());
            }
            add(&format!("payload-{index:06}"), root.join(&file.path), bytes)?;
            observer.progress("preparing", 0, request.plan.payload_file_count)?;
        }
        add(
            "runtime.exe",
            root.join(".avelin/avelin-app.exe"),
            payload.runtime,
        )?;
        add(
            "manifest.json",
            root.join(".avelin/manifest.json"),
            &serde_json::to_vec_pretty(request.manifest)?,
        )?;
        // Keep the exact verified worker, including its embedded manifest and payload.
        let worker_hash = digest(request.worker_exe)?;
        let worker_source = stage_dir.join("uninstall.exe");
        owned::publish(
            text(request.worker_exe)?,
            text(&worker_source)?,
            &worker_hash,
        )?;
        let mut receipt = request.receipt.clone();
        receipt.uninstaller_sha256 = Some(worker_hash.clone());
        receipt.shortcuts.clear();
        for shortcut in &request.shortcuts {
            let name = match shortcut.kind {
                ShortcutKind::Desktop => "desktop",
                ShortcutKind::StartMenu => "start_menu",
            };
            let bytes = system::shortcut_bytes(Path::new(&shortcut.target), root)?;
            let hash = add(
                &format!("{name}.lnk"),
                PathBuf::from(&shortcut.path),
                &bytes,
            )?;
            shortcut_evidence.push_str(&format!("{name}_sha256={hash}\r\n"));
            let mut record = shortcut.clone();
            record.sha256 = hash;
            receipt.shortcuts.push(record);
        }
        add(
            "receipt.json",
            root.join(RECEIPT_NAME),
            &serde_json::to_vec_pretty(&receipt)?,
        )?;
        // Keep receipt last: it commits the complete payload and resources.
        let receipt = stage.pop().context("staged receipt missing")?;
        stage.push(Staged {
            resource: Resource {
                path: root.join("uninstall.exe"),
                sha256: worker_hash,
                size: fs::metadata(&worker_source)?.len(),
            },
            source: worker_source,
        });
        stage.push(receipt);
    }
    let mut backup = Vec::new();
    for (index, item) in backup_resources.into_iter().enumerate() {
        if observer.checkpoint()? {
            return Err(Cancelled.into());
        }
        let source = backup_dir.join(format!("old-{index}.bin"));
        owned::publish(text(&item.path)?, text(&source)?, &item.sha256)?;
        backup.push(Backup {
            source,
            resource: item,
        });
    }
    for item in &backup {
        confirm(&item.resource.path, &item.resource.sha256)?;
        confirm(&item.source, &item.resource.sha256)?;
    }
    for item in &stage {
        confirm(&item.source, &item.resource.sha256)?;
    }
    Ok(Prepared {
        stage,
        backup,
        preserved,
        registration,
        next_registration,
        shortcut_evidence: shortcut_evidence.into_bytes(),
    })
}

struct Journal {
    path: PathBuf,
    task: String,
}
impl Journal {
    fn record(&self, event: &str, path: &Path, sha256: &str, result: &str) -> Result<()> {
        owned::append_upgrade_journal(
            text(&self.path)?,
            &self.task,
            event,
            text(path)?,
            sha256,
            result,
        )
    }
}

struct Transaction<'a> {
    request: &'a EngineRequest<'a>,
    prepared: Prepared,
    journal: Journal,
    created: Vec<Resource>,
    removed: Vec<usize>,
    directories: Vec<PathBuf>,
    marker_hash: Option<String>,
    registration_change: Option<system::RegistrationChange>,
    committed: bool,
    rollback_terminal: bool,
}
impl Transaction<'_> {
    fn directory(&mut self, path: &Path) -> Result<()> {
        if path.is_dir() {
            avelin_core::reject_reparse(path)?;
            return Ok(());
        }
        let parent = path.parent().context("deployment parent missing")?;
        self.directory(parent)?;
        self.journal
            .record("directory_intent", path, "", "create")?;
        owned::create_directory(text(path)?)?;
        self.directories.push(path.into());
        self.journal
            .record("directory_created", path, "", "created")
    }
    fn marker(&mut self) -> Result<()> {
        let root = &self.request.plan.install_dir;
        self.directory(&root.join(".avelin"))?;
        let bytes = serde_json::to_vec_pretty(
            &serde_json::json!({"schema_version":1,"engine":"rust","task_id":self.request.plan.task_id,
            "app_id":self.request.plan.app_id,"operation":self.request.plan.operation,"installation_id":self.request.receipt.installation_id,
            "session_dir":self.request.session_dir,"journal":self.journal.path,"notice":"Unfinished native transaction. Preserve the session, backups and marker for diagnosis; never automatically rerun."}),
        )?;
        let path = root.join(MARKER);
        self.journal
            .record("marker_intent", &path, &hash_bytes(&bytes), "create")?;
        self.marker_hash = Some(owned::write_new_bytes(text(&path)?, &bytes)?);
        self.journal.record(
            "marker_created",
            &path,
            self.marker_hash.as_deref().unwrap(),
            "created",
        )
    }
    fn checkpoint(&self, observer: &mut impl EngineObserver, current: u64) -> Result<()> {
        if observer.checkpoint()? {
            return Err(Cancelled.into());
        }
        if self
            .request
            .cancel_after_files
            .is_some_and(|n| current >= n)
        {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(3) {
                if observer.checkpoint()? {
                    return Err(Cancelled.into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            bail!("cancel checkpoint expired without an authenticated cancellation command");
        }
        ensure!(
            !self.request.fault_after_files.is_some_and(|n| current >= n),
            "injected failure after {current} files"
        );
        ensure!(
            !(self.request.fault_point == Some("rollback") && current >= 1),
            "injected failure before recovery test"
        );
        Ok(())
    }
    fn fault(&self, point: &str) -> Result<()> {
        ensure!(
            self.request.fault_point != Some(point),
            "injected native failure at {point}"
        );
        Ok(())
    }
    fn pause(&self, point: &str, observer: &mut impl EngineObserver) -> Result<()> {
        if self.request.pause_point != Some(point) {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(
            &serde_json::json!({"schema_version":1,"task_id":self.request.plan.task_id,"point":point,"backend_pid":std::process::id()}),
        )?;
        owned::write_new_bytes(
            text(&self.request.session_dir.join("engine-pause.json"))?,
            &bytes,
        )?;
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(30) {
            if observer.checkpoint()? {
                return Err(Cancelled.into());
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        Ok(())
    }
    fn run(&mut self, observer: &mut impl EngineObserver) -> Result<()> {
        let total = self.request.plan.payload_file_count;
        self.pause("prepared", observer)?;
        observer.progress("preparing", 0, total)?;
        self.checkpoint(observer, 0)?;
        self.fault("after-staging")?;
        self.marker()?;
        let payload_paths: BTreeSet<_> = self
            .request
            .manifest
            .files
            .iter()
            .map(|v| key(&self.request.plan.install_dir.join(&v.path)))
            .collect();
        let mut removed_payload = 0;
        for index in 0..self.prepared.backup.len() {
            let item = &self.prepared.backup[index].resource;
            self.journal.record(
                "delete_intent",
                &item.path,
                &item.sha256,
                &index.to_string(),
            )?;
            let result = owned::delete(text(&item.path)?, &item.sha256)?;
            ensure!(
                result == "deleted",
                "old resource changed before transaction deletion"
            );
            self.removed.push(index);
            self.journal
                .record("deleted", &item.path, &item.sha256, &index.to_string())?;
            if self.request.plan.operation == Operation::Uninstall
                && payload_paths.contains(&key(&item.path))
            {
                removed_payload += 1;
                observer.progress("running", removed_payload, total)?;
            }
            // Backup is already durable. Losing the controller stops before the next mutation.
            if observer.checkpoint()? {
                return Err(Cancelled.into());
            }
        }
        let mut current = 0;
        if self.request.plan.operation == Operation::Uninstall {
            current = total;
            observer.progress("running", current, total)?;
        }
        for index in 0..self.prepared.stage.len() {
            let destination = self.prepared.stage[index].resource.path.clone();
            if destination == self.request.plan.install_dir.join(RECEIPT_NAME) {
                continue;
            }
            self.directory(destination.parent().context("destination parent")?)?;
            let item = &self.prepared.stage[index];
            self.journal.record(
                "publish_intent",
                &destination,
                &item.resource.sha256,
                "create_new",
            )?;
            owned::publish(
                text(&item.source)?,
                text(&destination)?,
                &item.resource.sha256,
            )?;
            self.created.push(item.resource.clone());
            self.journal
                .record("published", &destination, &item.resource.sha256, "created")?;
            if index == 0 {
                self.pause("after-first-publish", observer)?;
            }
            if index < self.request.manifest.files.len() {
                current += 1;
                observer.progress("extracting", current, total)?;
                let delay = Instant::now();
                while delay.elapsed() < Duration::from_millis(self.request.step_delay_ms) {
                    if observer.checkpoint()? {
                        return Err(Cancelled.into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                self.checkpoint(observer, current)?;
            } else if observer.checkpoint()? {
                return Err(Cancelled.into());
            }
        }
        self.fault("before-registration")?;
        self.journal.record(
            "registry_intent",
            &self.request.plan.install_dir,
            "",
            "replace_owned",
        )?;
        system::replace_registration(
            &self.request.plan.app_id,
            self.prepared.registration.as_ref(),
            self.prepared.next_registration.as_ref(),
            &mut self.registration_change,
        )?;
        self.journal.record(
            "registry_written",
            &self.request.plan.install_dir,
            "",
            "verified",
        )?;
        self.fault("after-registration")?;
        for item in &self.created {
            confirm(&item.path, &item.sha256)?;
        }
        for item in &self.prepared.preserved {
            confirm(&item.path, &item.sha256)?;
        }
        let created_paths: BTreeSet<_> = self.created.iter().map(|v| key(&v.path)).collect();
        for index in &self.removed {
            let path = &self.prepared.backup[*index].resource.path;
            if !created_paths.contains(&key(path)) {
                ensure!(!path.try_exists()?, "retired resource reappeared");
            }
        }
        if self.request.plan.operation != Operation::Uninstall {
            owned::write_new_bytes(
                text(&self.request.session_dir.join("shortcuts.ini"))?,
                &self.prepared.shortcut_evidence,
            )?;
        }
        self.fault("before-commit")?;
        observer.progress("finalizing", total, total)?;
        if observer.checkpoint()? {
            return Err(Cancelled.into());
        }
        if self.request.plan.operation != Operation::Uninstall {
            let receipt = self
                .prepared
                .stage
                .iter()
                .find(|v| v.resource.path == self.request.plan.install_dir.join(RECEIPT_NAME))
                .context("prepared receipt missing")?;
            self.journal.record(
                "publish_intent",
                &receipt.resource.path,
                &receipt.resource.sha256,
                "commit_receipt",
            )?;
            owned::publish(
                text(&receipt.source)?,
                text(&receipt.resource.path)?,
                &receipt.resource.sha256,
            )?;
            self.created.push(receipt.resource.clone());
            self.journal.record(
                "published",
                &receipt.resource.path,
                &receipt.resource.sha256,
                "committed_receipt",
            )?;
            confirm(&receipt.resource.path, &receipt.resource.sha256)?;
        }
        self.journal
            .record("committed", &self.request.plan.install_dir, "", "verified")?;
        self.committed = true;
        self.pause("after-commit", observer)?;
        self.clear_marker()?;
        self.remove_empty_directories();
        observer.progress("finalizing", total, total)?;
        Ok(())
    }
    fn clear_marker(&mut self) -> Result<()> {
        if let Some(hash) = self.marker_hash.as_ref() {
            ensure!(
                owned::delete(text(&self.request.plan.install_dir.join(MARKER))?, hash)?
                    == "deleted",
                "transaction marker changed or disappeared"
            );
            self.marker_hash = None;
        }
        Ok(())
    }
    fn remove_empty_directories(&self) {
        let root = &self.request.plan.install_dir;
        let mut directories = BTreeSet::new();
        for path in self
            .prepared
            .backup
            .iter()
            .map(|v| &v.resource.path)
            .chain(self.created.iter().map(|v| &v.path))
        {
            for parent in path.ancestors().skip(1).take_while(|v| inside(v, root)) {
                directories.insert(parent.to_path_buf());
            }
        }
        directories.insert(root.join(".avelin"));
        directories.insert(root.to_path_buf());
        let mut directories: Vec<_> = directories.into_iter().collect();
        directories.sort_by_key(|v| std::cmp::Reverse(v.components().count()));
        for path in directories {
            let _ = owned::remove_empty_directory(text(&path).unwrap_or(""));
        }
    }
    fn rollback(&mut self) -> Result<()> {
        self.journal.record(
            "rollback_begin",
            &self.request.plan.install_dir,
            "",
            "restore_owned_resources",
        )?;
        ensure!(
            self.request.fault_point != Some("rollback"),
            "injected recovery failure; evidence deliberately retained"
        );
        // Every compensation is content-bound. No recursive deletion or overwrite occurs.
        for item in self.created.iter().rev() {
            self.journal.record(
                "rollback_delete_intent",
                &item.path,
                &item.sha256,
                "delete_new",
            )?;
            let result = owned::delete(text(&item.path)?, &item.sha256)?;
            ensure!(
                result == "deleted" || result == "missing",
                "new resource modified during recovery; preserve it"
            );
        }
        for index in self.removed.iter().rev() {
            let item = &self.prepared.backup[*index];
            self.journal.record(
                "restore_intent",
                &item.resource.path,
                &item.resource.sha256,
                &index.to_string(),
            )?;
            owned::publish(
                text(&item.source)?,
                text(&item.resource.path)?,
                &item.resource.sha256,
            )?;
            self.journal.record(
                "restored",
                &item.resource.path,
                &item.resource.sha256,
                "verified",
            )?;
        }
        if let Some(change) = self.registration_change.as_mut() {
            change.restore()?;
        }
        for item in &self.prepared.backup {
            confirm(&item.resource.path, &item.resource.sha256)?;
        }
        for item in &self.prepared.preserved {
            confirm(&item.path, &item.sha256)?;
        }
        system::verify_registration(
            &self.request.plan.app_id,
            self.prepared.registration.as_ref(),
        )?;
        for path in self.directories.iter().rev() {
            let _ = owned::remove_empty_directory(text(path)?);
        }
        self.journal.record(
            "rolled_back",
            &self.request.plan.install_dir,
            "",
            "verified",
        )?;
        self.rollback_terminal = true;
        self.clear_marker()?;
        for path in self.directories.iter().rev() {
            let _ = owned::remove_empty_directory(text(path)?);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("authenticated cancellation requested")
    }
}
impl std::error::Error for Cancelled {}

pub(super) fn execute(
    request: EngineRequest<'_>,
    observer: &mut impl EngineObserver,
) -> Result<EngineOutcome> {
    let prepared = match prepare(&request, observer) {
        Ok(prepared) => prepared,
        Err(error) if error.is::<Cancelled>() => {
            let _ = observer.progress("cancelling", 0, request.plan.payload_file_count);
            let _ = observer.progress("cleanup_completed", 0, request.plan.payload_file_count);
            return Ok(EngineOutcome {
                exit_code: 2,
                detail: "Cancelled during preparation; installation resources were unchanged."
                    .into(),
            });
        }
        Err(error) => return Err(error),
    };
    let journal = Journal {
        path: request
            .session_dir
            .join(if request.plan.operation == Operation::Upgrade {
                "upgrade-journal.jsonl"
            } else {
                "engine-journal.jsonl"
            }),
        task: request.plan.task_id.clone(),
    };
    journal.record("begin", &request.plan.install_dir, "", "prepared")?;
    // Persist the full compensation map before the first installation mutation.
    let recovery = serde_json::json!({"schema_version":1,"task_id":request.plan.task_id,"install_dir":request.plan.install_dir,
        "created":prepared.stage.iter().map(|v|&v.resource).collect::<Vec<_>>(),
        "backup":prepared.backup.iter().map(|v|serde_json::json!({"source":v.source,"resource":v.resource})).collect::<Vec<_>>(),
        "registration":prepared.registration,"next_registration":prepared.next_registration});
    owned::write_new_bytes(
        text(&request.session_dir.join("engine-recovery.json"))?,
        &serde_json::to_vec_pretty(&recovery)?,
    )?;
    let mut transaction = Transaction {
        request: &request,
        prepared,
        journal,
        created: vec![],
        removed: vec![],
        directories: vec![],
        marker_hash: None,
        registration_change: None,
        committed: false,
        rollback_terminal: false,
    };
    match transaction.run(observer) {
        Ok(()) => {
            if request.fault_point == Some("after-commit") {
                return Ok(EngineOutcome{exit_code:4,detail:"Injected loss of terminal confirmation after a durable commit; inspect the committed journal.".into()});
            }
            Ok(EngineOutcome {
                exit_code: 0,
                detail: String::new(),
            })
        }
        Err(error) => {
            if transaction.committed {
                return Ok(EngineOutcome {
                    exit_code: 4,
                    detail: format!(
                        "Deployment was durably committed, but final confirmation failed: {error:#}. Inspect the committed journal; no rollback was attempted."
                    ),
                });
            }
            if error.is::<owned::CleanupIncomplete>() {
                let _ = transaction.journal.record(
                    "rollback_incomplete",
                    &request.plan.install_dir,
                    "",
                    "partial_publication_requires_inspection",
                );
                return Ok(EngineOutcome {
                    exit_code: 4,
                    detail: format!(
                        "{error:#}; a partial output could not be removed. Preserve the marker, session and backups."
                    ),
                });
            }
            let cancelled = error.is::<Cancelled>();
            let _ = observer.progress(
                "cancelling",
                transaction.created.len().min(request.manifest.files.len()) as u64,
                request.plan.payload_file_count,
            );
            match transaction.rollback() {
                Ok(()) => {
                    let _ =
                        observer.progress("cleanup_completed", 0, request.plan.payload_file_count);
                    Ok(EngineOutcome {
                        exit_code: if cancelled { 2 } else { 3 },
                        detail: format!(
                            "{error:#}; original installation resources were restored or left unchanged"
                        ),
                    })
                }
                Err(recovery) => {
                    if !transaction.rollback_terminal {
                        let _ = transaction.journal.record(
                            "rollback_incomplete",
                            &request.plan.install_dir,
                            "",
                            "preserve_session_and_backups",
                        );
                    }
                    Ok(EngineOutcome {
                        exit_code: 4,
                        detail: format!(
                            "{error:#}; recovery incomplete: {recovery:#}. Preserve the session and transaction marker."
                        ),
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use avelin_core::{FileResource, InstallPlan, native_payload::NativePayload};
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
        worker: PathBuf,
        manifest: Manifest,
        plan: InstallPlan,
        receipt: Receipt,
    }
    impl Fixture {
        fn new() -> Self {
            let id = uuid::Uuid::new_v4().simple().to_string();
            let base = std::env::temp_dir().join(format!("avelin-native-test-{id}"));
            fs::create_dir(&base).unwrap();
            let root = base.join("installed");
            let worker = base.join("worker.exe");
            fs::write(&worker, b"test worker").unwrap();
            let manifest = Manifest {
                schema_version: 1,
                app_id: format!("org.avelin.native-test.{id}"),
                app_name: "Native engine fixture".into(),
                app_version: "1.0.0".into(),
                entry: "entry.txt".into(),
                directory_name: "NativeFixture".into(),
                start_menu_shortcut: false,
                desktop_shortcut: false,
                files: vec![FileResource {
                    path: "entry.txt".into(),
                    size: 7,
                    sha256: hash_bytes(b"payload"),
                }],
                runtime_sha256: Some(hash_bytes(b"runtime")),
                presentation: None,
            };
            let plan = InstallPlan::new(
                &manifest,
                Operation::Install,
                "task-install".into(),
                root.clone(),
                "S-1-5-21-100-100-100-1001".into(),
            )
            .unwrap();
            let receipt = Receipt {
                schema_version: 1,
                app_id: manifest.app_id.clone(),
                app_version: manifest.app_version.clone(),
                installation_id: "installation".into(),
                task_id: plan.task_id.clone(),
                commit_id: "commit-install".into(),
                previous_commit_id: None,
                install_dir: root.to_string_lossy().into_owned(),
                manifest_sha256: plan.manifest_sha256.clone(),
                uninstaller_sha256: None,
                user_sid: plan.user_sid.clone(),
                files: manifest.files.clone(),
                shortcuts: vec![],
            };
            Self {
                base,
                root,
                worker,
                manifest,
                plan,
                receipt,
            }
        }
        fn session(&self, name: &str) -> PathBuf {
            let path = self.base.join(name);
            fs::create_dir(&path).unwrap();
            path
        }
        fn request<'a>(
            &'a self,
            session: &'a Path,
            payload: &'a NativePayload<'a>,
        ) -> EngineRequest<'a> {
            EngineRequest {
                manifest: &self.manifest,
                plan: &self.plan,
                receipt: &self.receipt,
                session_dir: session,
                worker_exe: &self.worker,
                payload: Some(payload),
                shortcuts: vec![],
                fault_after_files: None,
                cancel_after_files: None,
                step_delay_ms: 0,
                fault_point: None,
                pause_point: None,
            }
        }
        fn upgrade_inputs(&self, session: &Path, new: &Manifest) -> (InstallPlan, Receipt) {
            let receipt_bytes = fs::read(self.root.join(RECEIPT_NAME)).unwrap();
            let manifest_bytes = fs::read(self.root.join(".avelin/manifest.json")).unwrap();
            let old: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
            let old_receipt: Receipt = serde_json::from_slice(&receipt_bytes).unwrap();
            let mut backup = vec![];
            let mut preserved = vec![];
            for (_, (path, hash)) in allowed_old(
                &old,
                &old_receipt,
                &receipt_bytes,
                &manifest_bytes,
                &self.root,
            )
            .unwrap()
            {
                if path.exists() {
                    let actual = digest(&path).unwrap();
                    let resource = Resource {
                        size: fs::metadata(&path).unwrap().len(),
                        path,
                        sha256: actual.clone(),
                    };
                    if actual == hash {
                        backup.push(resource);
                    } else {
                        preserved.push(resource);
                    }
                }
            }
            fs::write(session.join("previous-receipt.json"), &receipt_bytes).unwrap();
            fs::write(session.join("previous-manifest.json"), &manifest_bytes).unwrap();
            fs::write(session.join("upgrade-resources.json"),serde_json::to_vec_pretty(&serde_json::json!({"schema_version":1,"task_id":"task-upgrade","backup":backup,"preserved":preserved,"registration":Registration::new(&old,&self.root)})).unwrap()).unwrap();
            let upgrade = avelin_core::UpgradePlan {
                previous_version: old.app_version.clone(),
                previous_manifest_sha256: old_receipt.manifest_sha256.clone(),
                previous_receipt_sha256: hash_bytes(&receipt_bytes),
                previous_task_id: old_receipt.task_id.clone(),
                previous_commit_id: old_receipt.commit_id.clone(),
                installation_id: old_receipt.installation_id.clone(),
                diff: avelin_core::UpgradeDiff::between(&old, new).unwrap(),
                staging_dir: session.join("staging"),
                backup_dir: session.join("backup"),
                required_space_bytes: 1024 * 1024,
            };
            let plan = InstallPlan::new_upgrade(
                new,
                &old,
                &old_receipt,
                &hash_bytes(&receipt_bytes),
                "task-upgrade".into(),
                upgrade,
            )
            .unwrap();
            let mut receipt = old_receipt;
            receipt.app_version = new.app_version.clone();
            receipt.previous_commit_id = Some(receipt.commit_id.clone());
            receipt.commit_id = "commit-upgrade".into();
            receipt.task_id = plan.task_id.clone();
            receipt.manifest_sha256 = plan.manifest_sha256.clone();
            receipt.files = new.files.clone();
            receipt.uninstaller_sha256 = None;
            (plan, receipt)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = system::restore_registration(
                &self.manifest.app_id,
                None,
                Some(&Registration::new(&self.manifest, &self.root)),
            );
            let mut next = self.manifest.clone();
            next.app_version = "2.0.0".into();
            let _ = system::restore_registration(
                &next.app_id,
                None,
                Some(&Registration::new(&next, &self.root)),
            );
            // This freshly generated UUID directory is the entire fixture's scope.
            // It never contains paths or junctions supplied by a caller.
            assert_eq!(self.base.parent(), Some(std::env::temp_dir().as_path()));
            assert!(
                self.base
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("avelin-native-test-")
            );
            let _ = fs::remove_dir_all(&self.base);
        }
    }
    #[derive(Default)]
    struct Observer {
        cancel_on_file: bool,
        cancel: bool,
    }
    impl EngineObserver for Observer {
        fn progress(&mut self, phase: &str, current: u64, _: u64) -> Result<()> {
            if self.cancel_on_file && phase == "extracting" && current == 1 {
                self.cancel = true;
            }
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<bool> {
            Ok(self.cancel)
        }
    }
    fn payload() -> NativePayload<'static> {
        NativePayload {
            runtime: b"runtime",
            files: vec![b"payload"],
        }
    }
    fn terminal(session: &Path) -> String {
        let contents = fs::read_to_string(session.join("engine-journal.jsonl")).unwrap();
        let row: serde_json::Value =
            serde_json::from_str(contents.lines().last().unwrap()).unwrap();
        row["event"].as_str().unwrap().to_string()
    }
    #[test]
    fn native_install_and_owned_uninstall_preserve_modified_and_unrelated_files() {
        let fixture = Fixture::new();
        let session = fixture.session("install");
        let payload = payload();
        assert_eq!(
            execute(
                fixture.request(&session, &payload),
                &mut Observer::default()
            )
            .unwrap()
            .exit_code,
            0
        );
        assert_eq!(
            fs::read(fixture.root.join("entry.txt")).unwrap(),
            b"payload"
        );
        assert_eq!(
            fs::read(fixture.root.join(".avelin/avelin-app.exe")).unwrap(),
            b"runtime"
        );
        assert_eq!(terminal(&session), "committed");
        assert!(!fixture.root.join(MARKER).exists());
        let mut receipt = fixture.receipt.clone();
        receipt.uninstaller_sha256 = Some(digest(&fixture.root.join("uninstall.exe")).unwrap());
        fs::write(
            fixture.root.join(RECEIPT_NAME),
            serde_json::to_vec_pretty(&receipt).unwrap(),
        )
        .unwrap();
        fs::write(fixture.root.join("entry.txt"), b"user modified payload").unwrap();
        fs::write(fixture.root.join("notes.txt"), b"unrelated notes").unwrap();
        let session = fixture.session("uninstall");
        let plan = InstallPlan::new(
            &fixture.manifest,
            Operation::Uninstall,
            "task-uninstall".into(),
            fixture.root.clone(),
            fixture.plan.user_sid.clone(),
        )
        .unwrap();
        let mut request = fixture.request(&session, &payload);
        request.plan = &plan;
        request.receipt = &receipt;
        request.payload = None;
        assert_eq!(
            execute(request, &mut Observer::default())
                .unwrap()
                .exit_code,
            0
        );
        assert_eq!(
            fs::read(fixture.root.join("entry.txt")).unwrap(),
            b"user modified payload"
        );
        assert_eq!(
            fs::read(fixture.root.join("notes.txt")).unwrap(),
            b"unrelated notes"
        );
        assert!(!fixture.root.join(RECEIPT_NAME).exists());
        assert!(!fixture.root.join("uninstall.exe").exists());
        system::verify_registration(&fixture.manifest.app_id, None).unwrap();
    }
    #[test]
    fn failure_after_registration_removes_only_new_owned_resources() {
        let fixture = Fixture::new();
        let session = fixture.session("failure");
        let payload = payload();
        fs::create_dir(&fixture.root).unwrap();
        fs::write(fixture.root.join("notes.txt"), b"keep").unwrap();
        let mut request = fixture.request(&session, &payload);
        request.fault_point = Some("after-registration");
        assert_eq!(
            execute(request, &mut Observer::default())
                .unwrap()
                .exit_code,
            3
        );
        assert_eq!(terminal(&session), "rolled_back");
        assert_eq!(fs::read(fixture.root.join("notes.txt")).unwrap(), b"keep");
        assert!(!fixture.root.join("entry.txt").exists());
        assert!(!fixture.root.join(MARKER).exists());
        system::verify_registration(&fixture.manifest.app_id, None).unwrap();
    }
    #[test]
    fn cancellation_requires_observer_command_and_rolls_back() {
        let fixture = Fixture::new();
        let session = fixture.session("cancel");
        let payload = payload();
        let mut request = fixture.request(&session, &payload);
        request.cancel_after_files = Some(1);
        assert_eq!(
            execute(
                request,
                &mut Observer {
                    cancel_on_file: true,
                    cancel: false
                }
            )
            .unwrap()
            .exit_code,
            2
        );
        assert!(!fixture.root.exists());
        assert_eq!(terminal(&session), "rolled_back");
    }
    #[test]
    fn recovery_failure_retains_marker_and_blocks_another_attempt() {
        let fixture = Fixture::new();
        let session = fixture.session("unknown");
        let payload = payload();
        let mut request = fixture.request(&session, &payload);
        request.fault_point = Some("rollback");
        assert_eq!(
            execute(request, &mut Observer::default())
                .unwrap()
                .exit_code,
            4
        );
        assert!(fixture.root.join(MARKER).exists());
        assert!(session.join("engine-recovery.json").exists());
        assert_eq!(terminal(&session), "rollback_incomplete");
        let again = fixture.session("again");
        assert!(execute(fixture.request(&again, &payload), &mut Observer::default()).is_err());
        assert!(!again.join("staging").exists());
    }
    #[test]
    fn conflict_is_rejected_before_destination_or_registration_mutation() {
        let fixture = Fixture::new();
        let session = fixture.session("conflict");
        let payload = payload();
        fs::create_dir(&fixture.root).unwrap();
        fs::write(fixture.root.join("entry.txt"), b"existing").unwrap();
        assert!(
            execute(
                fixture.request(&session, &payload),
                &mut Observer::default()
            )
            .is_err()
        );
        assert_eq!(
            fs::read(fixture.root.join("entry.txt")).unwrap(),
            b"existing"
        );
        assert!(!session.join("staging").exists());
        system::verify_registration(&fixture.manifest.app_id, None).unwrap();
    }
    #[test]
    fn lost_confirmation_after_commit_keeps_the_complete_installation() {
        let fixture = Fixture::new();
        let session = fixture.session("committed");
        let payload = payload();
        let mut request = fixture.request(&session, &payload);
        request.fault_point = Some("after-commit");
        assert_eq!(
            execute(request, &mut Observer::default())
                .unwrap()
                .exit_code,
            4
        );
        assert_eq!(terminal(&session), "committed");
        assert!(!fixture.root.join(MARKER).exists());
        assert_eq!(
            fs::read(fixture.root.join("entry.txt")).unwrap(),
            b"payload"
        );
        system::verify_registration(
            &fixture.manifest.app_id,
            Some(&Registration::new(&fixture.manifest, &fixture.root)),
        )
        .unwrap();
    }
    #[test]
    fn upgrade_failure_restores_exact_previous_bytes_and_registration() {
        for fault in ["before-registration", "after-registration", "before-commit"] {
            let fixture = Fixture::new();
            let install = fixture.session("install");
            let payload = payload();
            assert_eq!(
                execute(
                    fixture.request(&install, &payload),
                    &mut Observer::default()
                )
                .unwrap()
                .exit_code,
                0
            );
            let before_receipt = fs::read(fixture.root.join(RECEIPT_NAME)).unwrap();
            let before_manifest = fs::read(fixture.root.join(".avelin/manifest.json")).unwrap();
            let mut next = fixture.manifest.clone();
            next.app_version = "2.0.0".into();
            next.files[0].sha256 = hash_bytes(b"updated");
            let payload = NativePayload {
                runtime: b"runtime",
                files: vec![b"updated"],
            };
            let session = fixture.session("upgrade");
            let (plan, receipt) = fixture.upgrade_inputs(&session, &next);
            let mut request = fixture.request(&session, &payload);
            request.manifest = &next;
            request.plan = &plan;
            request.receipt = &receipt;
            request.fault_point = Some(fault);
            let outcome = execute(request, &mut Observer::default()).unwrap();
            assert_eq!(outcome.exit_code, 3, "{}", outcome.detail);
            assert_eq!(
                fs::read(fixture.root.join("entry.txt")).unwrap(),
                b"payload"
            );
            assert_eq!(
                fs::read(fixture.root.join(RECEIPT_NAME)).unwrap(),
                before_receipt
            );
            assert_eq!(
                fs::read(fixture.root.join(".avelin/manifest.json")).unwrap(),
                before_manifest
            );
            assert!(!fixture.root.join(MARKER).exists());
            system::verify_registration(
                &fixture.manifest.app_id,
                Some(&Registration::new(&fixture.manifest, &fixture.root)),
            )
            .unwrap();
            let journal = fs::read_to_string(session.join("upgrade-journal.jsonl")).unwrap();
            let final_row: serde_json::Value =
                serde_json::from_str(journal.lines().last().unwrap()).unwrap();
            assert_eq!(final_row["event"], "rolled_back");
        }
    }
    #[test]
    fn upgrade_preserves_modified_retired_file_and_commits_complete_new_receipt_last() {
        let fixture = Fixture::new();
        let install = fixture.session("install");
        let payload = payload();
        assert_eq!(
            execute(
                fixture.request(&install, &payload),
                &mut Observer::default()
            )
            .unwrap()
            .exit_code,
            0
        );
        fs::write(fixture.root.join("entry.txt"), b"user retained content").unwrap();
        let mut next = fixture.manifest.clone();
        next.app_version = "2.0.0".into();
        next.entry = "new.txt".into();
        next.files[0].path = "new.txt".into();
        next.files[0].sha256 = hash_bytes(b"updated");
        let payload = NativePayload {
            runtime: b"runtime",
            files: vec![b"updated"],
        };
        let session = fixture.session("upgrade");
        let (plan, receipt) = fixture.upgrade_inputs(&session, &next);
        let mut request = fixture.request(&session, &payload);
        request.manifest = &next;
        request.plan = &plan;
        request.receipt = &receipt;
        let outcome = execute(request, &mut Observer::default()).unwrap();
        assert_eq!(outcome.exit_code, 0, "{}", outcome.detail);
        assert_eq!(
            fs::read(fixture.root.join("entry.txt")).unwrap(),
            b"user retained content"
        );
        assert_eq!(fs::read(fixture.root.join("new.txt")).unwrap(), b"updated");
        let committed: Receipt =
            serde_json::from_slice(&fs::read(fixture.root.join(RECEIPT_NAME)).unwrap()).unwrap();
        assert_eq!(committed.installation_id, fixture.receipt.installation_id);
        assert_eq!(
            committed.previous_commit_id.as_deref(),
            Some(fixture.receipt.commit_id.as_str())
        );
        assert!(committed.uninstaller_sha256.is_some());
        let journal = fs::read_to_string(session.join("upgrade-journal.jsonl")).unwrap();
        let records: Vec<serde_json::Value> = journal
            .lines()
            .map(|v| serde_json::from_str(v).unwrap())
            .collect();
        let registration = records
            .iter()
            .position(|v| v["event"] == "registry_written")
            .unwrap();
        let commit = records
            .iter()
            .position(|v| v["result"] == "committed_receipt")
            .unwrap();
        assert!(registration < commit);
        assert_eq!(records.last().unwrap()["event"], "committed");
    }
    #[test]
    fn windows_path_identity_keeps_distinct_unicode_names_and_rejects_case_alias_overlap() {
        assert_ne!(
            key(Path::new(r"C:\Apps\straße")),
            key(Path::new(r"C:\Apps\strasse"))
        );
        assert_eq!(
            key(Path::new(r"C:\Apps\APP")),
            key(Path::new(r"c:/apps/app"))
        );
        assert!(inside(
            Path::new(r"c:\apps\app\session"),
            Path::new(r"C:\APPS\App")
        ));
        assert!(!inside(
            Path::new(r"C:\Apps\Apple"),
            Path::new(r"C:\Apps\App")
        ));
    }
    #[test]
    fn preparation_cancellation_is_cancelled_without_installation_mutation() {
        let fixture = Fixture::new();
        let session = fixture.session("cancel-preparing");
        let payload = payload();
        let outcome = execute(
            fixture.request(&session, &payload),
            &mut Observer {
                cancel_on_file: false,
                cancel: true,
            },
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 2);
        assert!(!fixture.root.exists());
        system::verify_registration(&fixture.manifest.app_id, None).unwrap();
    }
    #[test]
    fn rollback_preserves_a_new_file_modified_after_publication_and_reports_unknown() {
        struct Editor {
            path: PathBuf,
        }
        impl EngineObserver for Editor {
            fn progress(&mut self, phase: &str, current: u64, _: u64) -> Result<()> {
                if phase == "extracting" && current == 1 {
                    fs::write(&self.path, b"external edit")?;
                    bail!("simulated loss after external edit");
                }
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<bool> {
                Ok(false)
            }
        }
        let fixture = Fixture::new();
        let session = fixture.session("external-edit");
        let payload = payload();
        let outcome = execute(
            fixture.request(&session, &payload),
            &mut Editor {
                path: fixture.root.join("entry.txt"),
            },
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 4);
        assert_eq!(
            fs::read(fixture.root.join("entry.txt")).unwrap(),
            b"external edit"
        );
        assert!(fixture.root.join(MARKER).exists());
        assert_eq!(terminal(&session), "rollback_incomplete");
    }
    #[test]
    fn an_error_path_containing_cleanup_incomplete_still_rolls_back() {
        struct Collider {
            path: PathBuf,
        }
        impl EngineObserver for Collider {
            fn progress(&mut self, phase: &str, current: u64, _: u64) -> Result<()> {
                if phase == "extracting" && current == 1 {
                    fs::write(&self.path, b"external file")?;
                }
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<bool> {
                Ok(false)
            }
        }
        let mut fixture = Fixture::new();
        fixture.manifest.files.push(FileResource {
            path: "cleanup_incomplete.txt".into(),
            size: 5,
            sha256: hash_bytes(b"other"),
        });
        fixture.plan = InstallPlan::new(
            &fixture.manifest,
            Operation::Install,
            fixture.plan.task_id.clone(),
            fixture.root.clone(),
            fixture.plan.user_sid.clone(),
        )
        .unwrap();
        fixture.receipt.manifest_sha256 = fixture.plan.manifest_sha256.clone();
        fixture.receipt.files = fixture.manifest.files.clone();
        let session = fixture.session("word-in-path");
        let payload = NativePayload {
            runtime: b"runtime",
            files: vec![b"payload", b"other"],
        };
        let outcome = execute(
            fixture.request(&session, &payload),
            &mut Collider {
                path: fixture.root.join("cleanup_incomplete.txt"),
            },
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 3, "{}", outcome.detail);
        assert!(outcome.detail.contains("cleanup_incomplete.txt"));
        assert!(!fixture.root.join("entry.txt").exists());
        assert!(!fixture.root.join(MARKER).exists());
        assert_eq!(
            fs::read(fixture.root.join("cleanup_incomplete.txt")).unwrap(),
            b"external file"
        );
        assert_eq!(terminal(&session), "rolled_back");
    }

    #[test]
    fn external_registration_deletion_before_upgrade_registration_is_never_recreated() {
        struct DeleteRegistration {
            id: String,
            old: Registration,
            deleted: bool,
        }
        impl EngineObserver for DeleteRegistration {
            fn progress(&mut self, phase: &str, current: u64, _: u64) -> Result<()> {
                if phase == "extracting" && current == 1 && !self.deleted {
                    system::replace_registration(&self.id, Some(&self.old), None, &mut None)?;
                    self.deleted = true;
                }
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<bool> {
                Ok(false)
            }
        }
        let fixture = Fixture::new();
        let install = fixture.session("install");
        let payload = payload();
        assert_eq!(
            execute(
                fixture.request(&install, &payload),
                &mut Observer::default()
            )
            .unwrap()
            .exit_code,
            0
        );
        let before = fs::read(fixture.root.join(RECEIPT_NAME)).unwrap();
        let mut next = fixture.manifest.clone();
        next.app_version = "2.0.0".into();
        next.files[0].sha256 = hash_bytes(b"updated");
        let payload = NativePayload {
            runtime: b"runtime",
            files: vec![b"updated"],
        };
        let session = fixture.session("external-reg-deletion");
        let (plan, receipt) = fixture.upgrade_inputs(&session, &next);
        let mut request = fixture.request(&session, &payload);
        request.manifest = &next;
        request.plan = &plan;
        request.receipt = &receipt;
        let outcome = execute(
            request,
            &mut DeleteRegistration {
                id: fixture.manifest.app_id.clone(),
                old: Registration::new(&fixture.manifest, &fixture.root),
                deleted: false,
            },
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 4, "{}", outcome.detail);
        system::verify_registration(&fixture.manifest.app_id, None).unwrap();
        assert_eq!(
            fs::read(fixture.root.join("entry.txt")).unwrap(),
            b"payload"
        );
        assert_eq!(fs::read(fixture.root.join(RECEIPT_NAME)).unwrap(), before);
        assert!(fixture.root.join(MARKER).exists());
        assert_eq!(
            fs::read_to_string(session.join("upgrade-journal.jsonl"))
                .unwrap()
                .lines()
                .last()
                .map(|v| serde_json::from_str::<serde_json::Value>(v).unwrap()["event"].clone())
                .unwrap(),
            "rollback_incomplete"
        );
    }
    #[test]
    fn locked_old_file_with_cleanup_incomplete_in_its_name_restores_preceding_deletions() {
        use std::os::windows::fs::OpenOptionsExt;
        let mut fixture = Fixture::new();
        fixture.manifest.entry = "cleanup_incomplete.txt".into();
        fixture.manifest.files[0].path = fixture.manifest.entry.clone();
        fixture.plan = InstallPlan::new(
            &fixture.manifest,
            Operation::Install,
            fixture.plan.task_id.clone(),
            fixture.root.clone(),
            fixture.plan.user_sid.clone(),
        )
        .unwrap();
        fixture.receipt.manifest_sha256 = fixture.plan.manifest_sha256.clone();
        fixture.receipt.files = fixture.manifest.files.clone();
        let install = fixture.session("install");
        let payload = payload();
        assert_eq!(
            execute(
                fixture.request(&install, &payload),
                &mut Observer::default()
            )
            .unwrap()
            .exit_code,
            0
        );
        let receipt_before = fs::read(fixture.root.join(RECEIPT_NAME)).unwrap();
        let manifest_before = fs::read(fixture.root.join(".avelin/manifest.json")).unwrap();
        let mut next = fixture.manifest.clone();
        next.app_version = "2.0.0".into();
        next.files[0].sha256 = hash_bytes(b"updated");
        let payload = NativePayload {
            runtime: b"runtime",
            files: vec![b"updated"],
        };
        let session = fixture.session("locked-path");
        let (plan, receipt) = fixture.upgrade_inputs(&session, &next);
        let mut request = fixture.request(&session, &payload);
        request.manifest = &next;
        request.plan = &plan;
        request.receipt = &receipt;
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(fixture.root.join("cleanup_incomplete.txt"))
            .unwrap();
        let outcome = execute(request, &mut Observer::default()).unwrap();
        drop(lock);
        assert_eq!(outcome.exit_code, 3, "{}", outcome.detail);
        assert_eq!(
            fs::read(fixture.root.join(RECEIPT_NAME)).unwrap(),
            receipt_before
        );
        assert_eq!(
            fs::read(fixture.root.join(".avelin/manifest.json")).unwrap(),
            manifest_before
        );
        assert_eq!(
            fs::read(fixture.root.join("cleanup_incomplete.txt")).unwrap(),
            b"payload"
        );
        assert!(!fixture.root.join(MARKER).exists());
        let records = fs::read_to_string(session.join("upgrade-journal.jsonl")).unwrap();
        let records: Vec<serde_json::Value> = records
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(records.iter().any(|row| row["event"] == "deleted"));
        assert_eq!(records.last().unwrap()["event"], "rolled_back");
    }
}
