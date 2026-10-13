//! Cross-process compensation for schema-2 native transactions. The journal is
//! evidence, never an arbitrary list of filesystem commands. All destinations
//! are reconstructed from the package and original ownership receipt.
use crate::{
    EngineObserver, EngineOutcome, EngineRequest, RecoveryReport, RecoveryRequest,
    owned_files as owned, system,
    transaction::{Backup, MARKER, Resource, Staged, confirm, digest, inside, key, read, text},
};
use anyhow::{Context, Result, ensure};
use avelin_core::{
    InstallPlan, Manifest, Operation, RECEIPT_NAME, Receipt, hash_bytes,
    session::{SessionRecord, running_process},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::c_void,
    fs,
    path::{Path, PathBuf},
    ptr,
};

type Handle = *mut c_void;
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(attributes: *const c_void, owner: i32, name: *const u16) -> Handle;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn ReleaseMutex(handle: Handle) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn LocalFree(memory: Handle) -> Handle;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: u32,
        data: *mut c_void,
        size: u32,
        required: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *const c_void, text: *mut *mut u16) -> i32;
}
pub(crate) fn current_user_sid() -> Result<String> {
    unsafe {
        let mut token = ptr::null_mut();
        ensure!(
            OpenProcessToken(GetCurrentProcess(), 8, &mut token) != 0,
            "cannot inspect current user"
        );
        struct Token(Handle);
        impl Drop for Token {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        let token = Token(token);
        let mut size = 0;
        GetTokenInformation(token.0, 1, ptr::null_mut(), 0, &mut size);
        ensure!(size > 0 && size < 65536, "invalid user token size");
        let mut data = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        ensure!(
            GetTokenInformation(token.0, 1, data.as_mut_ptr().cast(), size, &mut size) != 0,
            "cannot read current user"
        );
        let sid = *(data.as_ptr().cast::<*const c_void>());
        let mut raw = ptr::null_mut();
        ensure!(
            ConvertSidToStringSidW(sid, &mut raw) != 0,
            "cannot format current user"
        );
        let mut length = 0;
        while *raw.add(length) != 0 {
            length += 1;
        }
        let result = String::from_utf16(std::slice::from_raw_parts(raw, length));
        LocalFree(raw.cast());
        Ok(result?)
    }
}

/// Kernel lifetime, unlike a PID text file, releases after abrupt termination.
/// Every native transaction and recovery uses the same per-user product mutex.
pub(crate) struct ProductLock(Handle);
impl ProductLock {
    pub(crate) fn acquire(product: &str, sid: &str) -> Result<Self> {
        let key = hash_bytes(format!("{sid}\n{product}").as_bytes());
        let name: Vec<u16> = format!("Global\\Avelin-Rust-{key}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        ensure!(!handle.is_null(), "cannot acquire native product mutex");
        let status = unsafe { WaitForSingleObject(handle, 0) };
        if !matches!(status, 0 | 0x80) {
            unsafe {
                CloseHandle(handle);
            };
            anyhow::bail!("another native transaction is active for this product");
        }
        Ok(Self(handle))
    }
}
impl Drop for ProductLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryRecord {
    schema_version: u32,
    commit_protocol: String,
    plan: InstallPlan,
    receipt: Receipt,
    original_manifest: Option<Manifest>,
    session_sha256: Option<String>,
    plan_sha256: Option<String>,
    product_lock_sha256: Option<String>,
    stage: Vec<Staged>,
    backup: Vec<Backup>,
    preserved: Vec<Resource>,
    absent_directories: Vec<PathBuf>,
    registration: system::RegistrationSnapshot,
    next_registration: system::RegistrationSnapshot,
}

pub(crate) fn make_record(
    request: &EngineRequest<'_>,
    stage: &[Staged],
    backup: &[Backup],
    preserved: &[Resource],
    before: Option<&system::Registration>,
    after: Option<&system::Registration>,
) -> Result<RecoveryRecord> {
    let session_path = request.session_dir.join("session.json");
    let (session_sha256, product_lock_sha256) = if session_path.try_exists()? {
        let bytes = read(&session_path)?;
        let record: SessionRecord = serde_json::from_slice(&bytes)?;
        (
            Some(hash_bytes(&bytes)),
            Some(digest(&record.product_lock)?),
        )
    } else {
        (None, None)
    };
    let plan_path = request.session_dir.join("install-plan.json");
    let plan_sha256 = if plan_path.try_exists()? {
        Some(digest(&plan_path)?)
    } else {
        None
    };
    let mut directories = BTreeSet::new();
    for path in stage
        .iter()
        .map(|v| v.resource.path.clone())
        .chain(Some(request.plan.install_dir.join(MARKER)))
    {
        for parent in path
            .ancestors()
            .skip(1)
            .take_while(|v| inside(v, &request.plan.install_dir))
        {
            avelin_core::reject_reparse(parent)?;
            if !parent.try_exists()? {
                directories.insert(parent.to_path_buf());
            }
        }
    }
    let original_manifest = if request.plan.operation == Operation::Upgrade {
        Some(serde_json::from_slice(&read(
            &request.session_dir.join("previous-manifest.json"),
        )?)?)
    } else {
        None
    };
    Ok(RecoveryRecord {
        schema_version: 2,
        commit_protocol: "durable_commit_seal_v1".into(),
        plan: request.plan.clone(),
        receipt: request.receipt.clone(),
        original_manifest,
        session_sha256,
        plan_sha256,
        product_lock_sha256,
        stage: stage.to_vec(),
        backup: backup.to_vec(),
        preserved: preserved.to_vec(),
        absent_directories: directories.into_iter().collect(),
        registration: system::registration_expected(before),
        next_registration: system::registration_expected(after),
    })
}
pub(crate) fn marker_bytes(request: &EngineRequest<'_>, record_hash: &str) -> Result<Vec<u8>> {
    marker_for(
        &request.plan,
        request.receipt,
        request.session_dir,
        record_hash,
    )
}
fn marker_for(
    plan: &InstallPlan,
    receipt: &Receipt,
    session: &Path,
    record_hash: &str,
) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(
        &serde_json::json!({"schema_version":2,"engine":"rust","task_id":plan.task_id,"app_id":plan.app_id,"operation":plan.operation,"installation_id":receipt.installation_id,"session_dir":session,"recovery_sha256":record_hash,"notice":"Unfinished native transaction. Use recovery with the original package and retained session."}),
    )?)
}
pub(crate) fn commit_bytes(task: &str, record_hash: &str) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(
        &serde_json::json!({"schema_version":1,"task_id":task,"recovery_sha256":record_hash,"decision":"committed"}),
    )?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistryStep {
    before: system::RegistrationSnapshot,
    after: system::RegistrationSnapshot,
    complete: bool,
}
pub(crate) fn registry_observer(
    session: &Path,
) -> Box<dyn FnMut(&system::RegistrationSnapshot, &system::RegistrationSnapshot, bool) -> Result<()>>
{
    let path = session.join("engine-registry.jsonl");
    Box::new(move |before, after, complete| {
        let mut bytes = serde_json::to_vec(&RegistryStep {
            before: before.clone(),
            after: after.clone(),
            complete,
        })?;
        bytes.push(b'\n');
        owned::append_durable_bytes(text(&path)?, &bytes)
    })
}

#[derive(Deserialize)]
struct JournalRow {
    schema_version: u32,
    task_id: String,
    event: String,
    path: PathBuf,
    sha256: String,
}
struct Loaded {
    record: RecoveryRecord,
    record_hash: String,
    session: SessionRecord,
    rows: Vec<JournalRow>,
    committed: bool,
    finished: bool,
}
fn validate_resources(record: &RecoveryRecord, manifest: &Manifest, session: &Path) -> Result<()> {
    let root = &record.plan.install_dir;
    let mut allowed = BTreeSet::new();
    for m in std::iter::once(manifest).chain(record.original_manifest.as_ref()) {
        m.validate()?;
        ensure!(
            m.app_id == manifest.app_id,
            "previous manifest belongs to another product"
        );
        for file in &m.files {
            allowed.insert(key(&root.join(&file.path)));
        }
    }
    for name in [
        RECEIPT_NAME,
        ".avelin/manifest.json",
        ".avelin/avelin-app.exe",
        "uninstall.exe",
    ] {
        allowed.insert(key(&root.join(name)));
    }
    for kind in [
        avelin_core::ShortcutKind::Desktop,
        avelin_core::ShortcutKind::StartMenu,
    ] {
        allowed.insert(key(&system::shortcut_path(&manifest.app_id, kind)?));
    }
    let check = |resource: &Resource| -> Result<()> {
        ensure!(
            allowed.contains(&key(&resource.path)),
            "recovery destination is outside package ownership"
        );
        avelin_core::reject_reparse(&resource.path)?;
        ensure!(
            resource.sha256.len() == 64 && resource.sha256.bytes().all(|v| v.is_ascii_hexdigit()),
            "invalid recovery resource digest"
        );
        Ok(())
    };
    let mut stages = BTreeSet::new();
    for item in &record.stage {
        check(&item.resource)?;
        ensure!(
            stages.insert(key(&item.resource.path))
                && item.source.parent() == Some(session.join("staging").as_path()),
            "invalid or duplicate staged resource"
        );
        confirm(&item.source, &item.resource.sha256)?;
        ensure!(
            fs::metadata(&item.source)?.len() == item.resource.size,
            "staging size mismatch"
        );
    }
    let mut backups = BTreeSet::new();
    for (index, item) in record.backup.iter().enumerate() {
        check(&item.resource)?;
        ensure!(
            backups.insert(key(&item.resource.path))
                && item.source == session.join("backup").join(format!("old-{index}.bin")),
            "invalid or duplicate backup resource"
        );
        confirm(&item.source, &item.resource.sha256)?;
        ensure!(
            fs::metadata(&item.source)?.len() == item.resource.size,
            "backup size mismatch"
        );
    }
    let mut preserved = BTreeSet::new();
    for item in &record.preserved {
        check(item)?;
        ensure!(
            preserved.insert(key(&item.path))
                && !stages.contains(&key(&item.path))
                && !backups.contains(&key(&item.path)),
            "preserved resource overlaps mutation"
        );
        confirm(&item.path, &item.sha256)?;
    }
    for dir in &record.absent_directories {
        ensure!(inside(dir, root), "directory evidence escapes installation");
        avelin_core::reject_reparse(dir)?;
    }
    Ok(())
}
fn dead(identity: &avelin_core::session::ProcessIdentity) -> Result<()> {
    ensure!(
        running_process(identity.pid)?.as_ref() != Some(identity),
        "original task process is still running"
    );
    Ok(())
}
fn load(request: &RecoveryRequest<'_>) -> Result<Loaded> {
    ensure!(
        current_user_sid()? == request.user_sid,
        "recovery requested for another Windows user"
    );
    request.manifest.validate()?;
    avelin_core::install_root(request.target)?;
    ensure!(
        request.session.is_absolute()
            && !inside(request.session, request.target)
            && !inside(request.target, request.session),
        "recovery session and installation overlap"
    );
    avelin_core::reject_reparse(request.session)?;
    let bytes = read(&request.session.join("engine-recovery.json"))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        value["schema_version"] == 2,
        "legacy recovery evidence is insufficient; keep it for manual inspection"
    );
    let record: RecoveryRecord = serde_json::from_slice(&bytes)?;
    let record_hash = hash_bytes(&bytes);
    ensure!(
        record.schema_version == 2 && record.commit_protocol == "durable_commit_seal_v1",
        "unsupported recovery protocol"
    );
    record.plan.validate(request.manifest)?;
    ensure!(
        record.plan.user_sid == request.user_sid
            && key(&record.plan.install_dir) == key(request.target)
            && !matches!(record.plan.operation, Operation::Recover),
        "recovery identity mismatch"
    );
    ensure!(
        record.receipt.schema_version == avelin_core::SCHEMA_VERSION
            && record.receipt.app_id == request.manifest.app_id
            && record.receipt.app_version == request.manifest.app_version
            && record.receipt.files == request.manifest.files
            && record.receipt.manifest_sha256 == record.plan.manifest_sha256
            && record.receipt.user_sid == request.user_sid
            && key(Path::new(&record.receipt.install_dir)) == key(request.target),
        "recovery receipt identity mismatch"
    );
    for id in [
        &record.receipt.installation_id,
        &record.receipt.task_id,
        &record.receipt.commit_id,
    ] {
        avelin_core::safe_id(id)?;
    }
    let expected_after = system::registration_expected(
        (record.plan.operation != Operation::Uninstall)
            .then(|| system::Registration::new(request.manifest, request.target))
            .as_ref(),
    );
    ensure!(
        record.next_registration == expected_after,
        "recovery registration target differs from package"
    );
    let expected_before = match record.plan.operation {
        Operation::Install => system::registration_expected(None),
        Operation::Upgrade => {
            let previous = read(&request.session.join("previous-receipt.json"))?;
            let receipt: Receipt = serde_json::from_slice(&previous)?;
            let old = record
                .original_manifest
                .as_ref()
                .context("previous manifest missing")?;
            record.plan.validate_upgrade(
                request.manifest,
                old,
                &receipt,
                &hash_bytes(&previous),
            )?;
            ensure!(
                record.receipt.installation_id == receipt.installation_id
                    && record.receipt.previous_commit_id.as_deref()
                        == Some(receipt.commit_id.as_str()),
                "recovery upgrade receipt lost previous installation identity"
            );
            system::registration_expected(Some(&system::Registration::new(old, request.target)))
        }
        Operation::Repair => {
            let previous = read(&request.session.join("prior-receipt.json"))?;
            let receipt: Receipt = serde_json::from_slice(&previous)?;
            crate::transaction::validate_repair_candidate(&receipt, &record.receipt, &record.plan)?;
            ensure!(
                record.backup.iter().any(|v| key(&v.resource.path)
                    == key(&request.target.join(RECEIPT_NAME))
                    && v.resource.sha256 == hash_bytes(&previous)),
                "repair recovery lacks its original receipt backup"
            );
            system::registration_expected(Some(&system::Registration::new(
                request.manifest,
                request.target,
            )))
        }
        _ => system::registration_expected(Some(&system::Registration::new(
            request.manifest,
            request.target,
        ))),
    };
    ensure!(
        record.registration == expected_before
            || (record.plan.operation == Operation::Repair
                && record.registration == system::registration_expected(None)),
        "recovery registration source differs from original ownership"
    );
    confirm(
        &request.session.join("install-plan.json"),
        record
            .plan_sha256
            .as_deref()
            .context("missing persistent plan identity")?,
    )?;
    let original_plan: InstallPlan =
        serde_json::from_slice(&read(&request.session.join("install-plan.json"))?)?;
    ensure!(original_plan == record.plan, "recovery plan changed");
    let session_bytes = read(&request.session.join("session.json"))?;
    ensure!(
        Some(hash_bytes(&session_bytes)).as_ref() == record.session_sha256.as_ref(),
        "session identity changed"
    );
    let session: SessionRecord = serde_json::from_slice(&session_bytes)?;
    ensure!(
        session.schema_version == 1
            && session.task_id == record.plan.task_id
            && session.app_id == record.plan.app_id
            && session.operation == record.plan.operation.as_str()
            && session.manifest_sha256 == record.plan.manifest_sha256
            && key(&session.install_dir) == key(request.target),
        "original session does not match transaction"
    );
    dead(&session.backend)?;
    dead(&session.coordinator)?;
    let local =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA unavailable")?);
    let expected_lock = local.join("Avelin/M0/locks").join(format!(
        "{}-{}.lock",
        request.manifest.app_id,
        &hash_bytes(request.user_sid.as_bytes())[..16]
    ));
    ensure!(
        key(&session.product_lock) == key(&expected_lock),
        "retained lock path is not this user's product lock"
    );
    if session.product_lock.try_exists()? {
        confirm(
            &session.product_lock,
            record
                .product_lock_sha256
                .as_deref()
                .context("missing retained lock evidence")?,
        )?;
        let pid = String::from_utf8(read(&session.product_lock)?)?
            .lines()
            .next()
            .context("retained lock owner is missing")?
            .trim()
            .parse::<u32>()?;
        ensure!(
            pid == session.coordinator.pid,
            "retained lock owner differs from original task"
        );
    }
    validate_resources(&record, request.manifest, request.session)?;
    let journal = request
        .session
        .join(if record.plan.operation == Operation::Upgrade {
            "upgrade-journal.jsonl"
        } else {
            "engine-journal.jsonl"
        });
    let bytes = read(&journal)?;
    ensure!(
        bytes.ends_with(b"\n"),
        "transaction journal has an incomplete record; preserve evidence"
    );
    let mut rows = Vec::new();
    for line in bytes.split(|v| *v == b'\n').filter(|v| !v.is_empty()) {
        let row: JournalRow = serde_json::from_slice(line)?;
        ensure!(
            row.schema_version == 1 && row.task_id == record.plan.task_id,
            "journal task identity mismatch"
        );
        rows.push(row);
    }
    ensure!(
        rows.first().is_some_and(|v| v.event == "begin"),
        "transaction begin evidence missing"
    );
    let marker_hash = hash_bytes(&marker_for(
        &record.plan,
        &record.receipt,
        request.session,
        &record_hash,
    )?);
    for row in rows
        .iter()
        .filter(|v| v.event == "marker_intent" || v.event == "marker_created")
    {
        ensure!(
            key(&row.path) == key(&request.target.join(MARKER)) && row.sha256 == marker_hash,
            "recovery descriptor differs from original durable marker intent"
        );
    }
    let committed_path = request.session.join("engine-commit.json");
    let committed = committed_path.try_exists()?;
    if committed {
        confirm(
            &committed_path,
            &hash_bytes(&commit_bytes(&record.plan.task_id, &record_hash)?),
        )?;
    }
    ensure!(
        committed || !rows.iter().any(|v| v.event == "committed"),
        "commit seal disappeared; preserve evidence"
    );
    let marker = request.target.join(MARKER);
    if marker.try_exists()? {
        confirm(
            &marker,
            &hash_bytes(&marker_for(
                &record.plan,
                &record.receipt,
                request.session,
                &record_hash,
            )?),
        )?;
    }
    let done = request.session.join("recovery-complete.json");
    let finished = done.try_exists()?;
    if finished {
        confirm(
            &done,
            &hash_bytes(&completion_bytes(
                &record.plan.task_id,
                &record_hash,
                committed,
            )?),
        )?;
    }
    let loaded = Loaded {
        record,
        record_hash,
        session,
        rows,
        committed,
        finished,
    };
    check_disk(&loaded)?;
    Ok(loaded)
}
fn permitted(loaded: &Loaded, event: &str, path: &Path, hash: &str) -> bool {
    loaded
        .rows
        .iter()
        .any(|v| v.event == event && key(&v.path) == key(path) && v.sha256 == hash)
}
fn registry_current_allowed(
    loaded: &Loaded,
    session: &Path,
) -> Result<system::RegistrationSnapshot> {
    let current = system::registration_snapshot(&loaded.record.plan.app_id)?;
    if loaded.committed {
        ensure!(
            current == loaded.record.next_registration,
            "committed registration changed"
        );
        return Ok(current);
    }
    let mut allowed = vec![loaded.record.registration.clone()];
    let path = session.join("engine-registry.jsonl");
    if path.try_exists()? {
        let bytes = read(&path)?;
        ensure!(
            bytes.ends_with(b"\n"),
            "partial registry evidence; preserve registration"
        );
        let mut pending: Option<RegistryStep> = None;
        for line in bytes.split(|v| *v == b'\n').filter(|v| !v.is_empty()) {
            let step: RegistryStep = serde_json::from_slice(line)?;
            validate_registry_step(
                &step,
                &loaded.record.registration,
                &loaded.record.next_registration,
            )?;
            if step.complete {
                ensure!(
                    pending
                        .as_ref()
                        .is_some_and(|v| v.before == step.before && v.after == step.after),
                    "registry completion has no matching durable intent"
                );
                allowed = vec![step.after];
                pending = None;
            } else {
                ensure!(
                    allowed.contains(&step.before),
                    "registry evidence is not linked to an owned prior state"
                );
                allowed = vec![step.before.clone(), step.after.clone()];
                pending = Some(step);
            }
        }
    }
    ensure!(
        allowed.contains(&current),
        "registration changed outside the interrupted transaction"
    );
    Ok(current)
}
fn validate_registry_step(
    step: &RegistryStep,
    before: &system::RegistrationSnapshot,
    after: &system::RegistrationSnapshot,
) -> Result<()> {
    for snapshot in [&step.before, &step.after] {
        if let Some(values) = &snapshot.values {
            for (name, value) in values {
                ensure!(
                    before.values.as_ref().and_then(|v| v.get(name)) == Some(value)
                        || after.values.as_ref().and_then(|v| v.get(name)) == Some(value),
                    "registry evidence contains an unrelated field or value"
                );
            }
        }
    }
    match (&step.before.values, &step.after.values) {
        (None, Some(values)) => ensure!(
            values.is_empty(),
            "registry creation must begin with an empty key"
        ),
        (Some(_), None) => {} // An owned key is deleted as one Windows operation.
        (Some(left), Some(right)) => {
            let names: BTreeSet<_> = left.keys().chain(right.keys()).collect();
            ensure!(
                names
                    .into_iter()
                    .filter(|name| left.get(*name) != right.get(*name))
                    .count()
                    == 1,
                "registry evidence must describe exactly one value mutation"
            );
        }
        _ => anyhow::bail!("registry evidence has no mutation"),
    }
    Ok(())
}
fn check_disk(loaded: &Loaded) -> Result<()> {
    let record = &loaded.record;
    let old: BTreeMap<_, _> = record
        .backup
        .iter()
        .map(|v| (key(&v.resource.path), &v.resource))
        .collect();
    let new: BTreeMap<_, _> = record
        .stage
        .iter()
        .map(|v| (key(&v.resource.path), &v.resource))
        .collect();
    for item in &record.stage {
        let path = &item.resource.path;
        if loaded.committed {
            confirm(path, &item.resource.sha256)?;
            continue;
        }
        if path.try_exists()? {
            let hash = digest(path)?;
            let before = old.get(&key(path));
            ensure!(
                before.is_some_and(|v| v.sha256 == hash)
                    || (hash == item.resource.sha256
                        && permitted(loaded, "publish_intent", path, &hash)),
                "resource was modified after interruption; preserve it: {}",
                path.display()
            );
        } else if old.contains_key(&key(path)) {
            ensure!(
                permitted(loaded, "delete_intent", path, &old[&key(path)].sha256),
                "owned original disappeared without a recorded mutation"
            );
        }
    }
    for item in &record.backup {
        if loaded.committed {
            if !new.contains_key(&key(&item.resource.path)) {
                ensure!(
                    !item.resource.path.try_exists()?,
                    "committed retired resource reappeared"
                );
            }
        } else if item.resource.path.try_exists()? {
            let hash = digest(&item.resource.path)?;
            ensure!(
                hash == item.resource.sha256
                    || new
                        .get(&key(&item.resource.path))
                        .is_some_and(|v| v.sha256 == hash
                            && permitted(loaded, "publish_intent", &v.path, &hash)),
                "original resource conflicts with recovery"
            );
        } else {
            ensure!(
                permitted(
                    loaded,
                    "delete_intent",
                    &item.resource.path,
                    &item.resource.sha256
                ),
                "original resource vanished without mutation intent"
            );
        }
    }
    for item in &record.preserved {
        confirm(&item.path, &item.sha256)?;
    }
    Ok(())
}
fn completion_bytes(task: &str, hash: &str, committed: bool) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(
        &serde_json::json!({"schema_version":1,"task_id":task,"recovery_sha256":hash,"decision":if committed{"confirm_committed"}else{"rollback"}}),
    )?)
}
fn report(loaded: &Loaded, apply: bool) -> RecoveryReport {
    RecoveryReport {
        schema_version: 1,
        original_task_id: loaded.record.plan.task_id.clone(),
        decision: if loaded.finished {
            "already_recovered"
        } else if loaded.committed {
            "confirm_committed"
        } else {
            "rollback"
        }
        .into(),
        applied: apply,
        verified: true,
        restored: if apply && !loaded.committed && !loaded.finished {
            loaded
                .record
                .backup
                .iter()
                .map(|v| v.resource.path.to_string_lossy().into_owned())
                .collect()
        } else {
            vec![]
        },
        preserved: loaded
            .record
            .preserved
            .iter()
            .map(|v| v.path.to_string_lossy().into_owned())
            .collect(),
    }
}
pub(crate) fn inspect(request: &RecoveryRequest<'_>) -> Result<RecoveryReport> {
    let _lock = ProductLock::acquire(&request.manifest.app_id, request.user_sid)?;
    let loaded = load(request)?;
    registry_current_allowed(&loaded, request.session)?;
    Ok(report(&loaded, false))
}
pub(crate) fn recover(
    request: RecoveryRequest<'_>,
    observer: &mut impl EngineObserver,
    pause: Option<(&Path, &str, &str)>,
) -> Result<RecoveryReport> {
    let _lock = ProductLock::acquire(&request.manifest.app_id, request.user_sid)?;
    let loaded = load(&request)?;
    let current_registry = registry_current_allowed(&loaded, request.session)?;
    if !request.apply {
        return Ok(report(&loaded, false));
    }
    ensure!(
        !observer.checkpoint()?,
        "recovery cancelled before mutation"
    );
    let mut changed = false;
    let mut checkpoint = || -> Result<()> {
        ensure!(
            !observer.checkpoint()?,
            "recovery interrupted; repeat recovery with the same evidence"
        );
        if !changed {
            changed = true;
            if let Some((session, task, "recovery-after-first-mutation")) = pause {
                owned::write_new_bytes(
                    text(&session.join("engine-pause.json"))?,
                    &serde_json::to_vec_pretty(
                        &serde_json::json!({"schema_version":1,"task_id":task,"original_task_id":loaded.record.plan.task_id,"point":"recovery-after-first-mutation","backend_pid":std::process::id()}),
                    )?,
                )?;
                let until = std::time::Instant::now();
                while until.elapsed() < std::time::Duration::from_secs(30) {
                    ensure!(
                        !observer.checkpoint()?,
                        "recovery interrupted during acceptance checkpoint"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(15));
                }
            }
        }
        Ok(())
    };
    if !loaded.committed {
        let old: BTreeMap<_, _> = loaded
            .record
            .backup
            .iter()
            .map(|v| (key(&v.resource.path), v))
            .collect();
        for item in loaded.record.stage.iter().rev() {
            if item.resource.path.try_exists()? {
                let actual = digest(&item.resource.path)?;
                if old
                    .get(&key(&item.resource.path))
                    .is_some_and(|v| v.resource.sha256 == actual)
                {
                    continue;
                }
                ensure!(
                    actual == item.resource.sha256,
                    "new resource changed during recovery"
                );
                ensure!(
                    owned::delete(text(&item.resource.path)?, &actual)? == "deleted",
                    "new resource changed during deletion"
                );
                checkpoint()?;
            }
        }
        for item in loaded.record.backup.iter().rev() {
            if item.resource.path.try_exists()? {
                confirm(&item.resource.path, &item.resource.sha256)?;
            } else {
                owned::publish(
                    text(&item.source)?,
                    text(&item.resource.path)?,
                    &item.resource.sha256,
                )?;
                checkpoint()?;
            }
        }
        system::restore_registration_snapshot(
            &loaded.record.plan.app_id,
            &current_registry,
            &loaded.record.registration,
            Some(registry_observer(request.session)),
        )?;
        for item in &loaded.record.backup {
            confirm(&item.resource.path, &item.resource.sha256)?;
        }
        for item in &loaded.record.stage {
            if !old.contains_key(&key(&item.resource.path)) {
                ensure!(
                    !item.resource.path.try_exists()?,
                    "new resource remains after recovery"
                );
            }
        }
    }
    for item in &loaded.record.preserved {
        confirm(&item.path, &item.sha256)?;
    }
    let expected = if loaded.committed {
        &loaded.record.next_registration
    } else {
        &loaded.record.registration
    };
    ensure!(
        system::registration_snapshot(&loaded.record.plan.app_id)? == *expected,
        "registration recovery verification failed"
    );
    let done = request.session.join("recovery-complete.json");
    if !done.try_exists()? {
        owned::write_new_bytes(
            text(&done)?,
            &completion_bytes(
                &loaded.record.plan.task_id,
                &loaded.record_hash,
                loaded.committed,
            )?,
        )?;
    }
    let marker = request.target.join(MARKER);
    if marker.try_exists()? {
        ensure!(
            owned::delete(
                text(&marker)?,
                &hash_bytes(&marker_for(
                    &loaded.record.plan,
                    &loaded.record.receipt,
                    request.session,
                    &loaded.record_hash
                )?)
            )? == "deleted",
            "transaction marker changed"
        );
    }
    if !loaded.committed {
        let mut dirs = loaded.record.absent_directories.clone();
        dirs.sort_by_key(|v| std::cmp::Reverse(v.components().count()));
        for path in dirs {
            if let Some(row) = loaded.rows.iter().find(|row| {
                row.event == "directory_created"
                    && key(&row.path) == key(&path)
                    && !row.sha256.is_empty()
            }) {
                let _ = owned::remove_empty_directory_owned(text(&path)?, &row.sha256);
            }
        }
    }
    if loaded.session.product_lock.try_exists()? {
        ensure!(
            owned::delete(
                text(&loaded.session.product_lock)?,
                loaded
                    .record
                    .product_lock_sha256
                    .as_deref()
                    .context("retained lock identity missing")?
            )? == "deleted",
            "retained product lock changed"
        );
    }
    observer.progress("finalizing", 1, 1)?;
    Ok(report(&loaded, true))
}
pub(crate) fn execute(
    request: EngineRequest<'_>,
    observer: &mut impl EngineObserver,
) -> Result<EngineOutcome> {
    let recovery = RecoveryRequest {
        manifest: request.manifest,
        target: &request.plan.install_dir,
        session: request
            .recovery_session
            .context("recovery session is required")?,
        user_sid: &request.plan.user_sid,
        apply: request.recovery_apply,
    };
    // Preflight errors imply no recovery mutation. Errors after apply starts are
    // conservatively Unknown, so the original evidence remains actionable.
    inspect(&recovery)?;
    let result = recover(
        recovery,
        observer,
        request
            .pause_point
            .map(|v| (request.session_dir, request.plan.task_id.as_str(), v)),
    );
    match result {
        Ok(report) => {
            if let Err(error) = owned::write_new_bytes(
                text(&request.session_dir.join("recovery-report.json"))?,
                &serde_json::to_vec_pretty(&report)?,
            ) {
                return Ok(EngineOutcome {
                    exit_code: if report.applied { 4 } else { 3 },
                    detail: format!("Recovery report could not be persisted: {error:#}"),
                });
            }
            Ok(EngineOutcome {
                exit_code: 0,
                detail: format!(
                    "Recovery {}: {}",
                    if report.applied {
                        "completed"
                    } else {
                        "checked"
                    },
                    report.decision
                ),
            })
        }
        Err(error) => Ok(EngineOutcome {
            exit_code: if request.recovery_apply { 4 } else { 3 },
            detail: format!("{error:#}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn product_mutex_excludes_another_transaction_and_releases_with_its_owner() {
        let id = format!("org.avelin.mutex-test.{}", uuid::Uuid::new_v4().simple());
        let sid = current_user_sid().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let (peer_id, peer_sid) = (id.clone(), sid.clone());
        let peer = std::thread::spawn(move || {
            let _guard = ProductLock::acquire(&peer_id, &peer_sid).unwrap();
            ready_tx.send(()).unwrap();
            stop_rx.recv().unwrap();
        });
        ready_rx.recv().unwrap();
        assert!(ProductLock::acquire(&id, &sid).is_err());
        stop_tx.send(()).unwrap();
        peer.join().unwrap();
        assert!(ProductLock::acquire(&id, &sid).is_ok());
    }
}
