//! Current-user Windows registration and shell links.
use anyhow::{Context, Result, ensure};
use avelin_core::{Manifest, ShortcutKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    path::{Path, PathBuf},
    ptr,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub display_name: String,
    pub display_version: String,
    pub install_location: String,
    pub uninstall_string: String,
    pub quiet_uninstall_string: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modify_path: Option<String>,
    pub no_modify: u32,
    pub no_repair: u32,
}

impl Registration {
    pub fn new(manifest: &Manifest, target: &Path) -> Self {
        let command = |silent| {
            format!(
                "\"{}\" --{} --operation uninstall --backend \"{}\" --manifest \"{}\" --install-dir \"{}\"",
                target.join(".avelin").join("avelin-app.exe").display(),
                if silent { "silent" } else { "gui" },
                target.join("uninstall.exe").display(),
                target.join(".avelin").join("manifest.json").display(),
                target.display()
            )
        };
        Self {
            display_name: manifest.app_name.clone(),
            display_version: manifest.app_version.clone(),
            install_location: target.to_string_lossy().into_owned(),
            uninstall_string: command(false),
            quiet_uninstall_string: command(true),
            modify_path: Some(format!(
                "\"{}\" --gui --maintenance --install-dir \"{}\"",
                target.join(".avelin").join("avelin-app.exe").display(),
                target.display()
            )),
            no_modify: 0,
            no_repair: 1,
        }
    }

    /// Old installations and durable recovery records have exactly seven values.
    /// Only that complete historical shape is accepted, never a partial modern key.
    pub fn legacy(manifest: &Manifest, target: &Path) -> Self {
        let mut value = Self::new(manifest, target);
        value.modify_path = None;
        value.no_modify = 1;
        value
    }

    fn values(&self) -> Values {
        let mut values = BTreeMap::new();
        for (key, value) in [
            ("DisplayName", &self.display_name),
            ("DisplayVersion", &self.display_version),
            ("InstallLocation", &self.install_location),
            ("UninstallString", &self.uninstall_string),
            ("QuietUninstallString", &self.quiet_uninstall_string),
        ] {
            values.insert(
                key.to_string(),
                (
                    1,
                    wide(value).iter().flat_map(|v| v.to_le_bytes()).collect(),
                ),
            );
        }
        for (key, value) in [("NoModify", self.no_modify), ("NoRepair", self.no_repair)] {
            values.insert(key.to_string(), (4, value.to_le_bytes().to_vec()));
        }
        if let Some(value) = &self.modify_path {
            values.insert(
                "ModifyPath".into(),
                (
                    1,
                    wide(value).iter().flat_map(|v| v.to_le_bytes()).collect(),
                ),
            );
        }
        values
    }
}

type Values = BTreeMap<String, (u32, Vec<u8>)>;
/// Exact, bounded values of the product's one current-user uninstall key.
/// None and an existing empty key are different states during recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistrationSnapshot {
    pub values: Option<Values>,
}

/// false records durable intent, true records a flushed and verified mutation.
/// The owner binds these records to the task before permitting recovery.
pub(crate) type RegistrationObserver =
    Box<dyn FnMut(&RegistrationSnapshot, &RegistrationSnapshot, bool) -> Result<()>>;

pub(crate) fn registration_snapshot(id: &str) -> Result<RegistrationSnapshot> {
    Ok(RegistrationSnapshot {
        values: raw_snapshot(id)?,
    })
}

pub(crate) fn registration_expected(registration: Option<&Registration>) -> RegistrationSnapshot {
    RegistrationSnapshot {
        values: registration.map(Registration::values),
    }
}

pub(crate) fn snapshot_matches_product(
    snapshot: &RegistrationSnapshot,
    manifest: &Manifest,
    target: &Path,
) -> bool {
    snapshot == &registration_expected(Some(&Registration::new(manifest, target)))
        || snapshot == &registration_expected(Some(&Registration::legacy(manifest, target)))
}

pub(crate) fn registration_for_product(
    manifest: &Manifest,
    target: &Path,
) -> Result<Option<Registration>> {
    let Some(values) = raw_snapshot(&manifest.app_id)? else {
        return Ok(None);
    };
    for registration in [
        Registration::new(manifest, target),
        Registration::legacy(manifest, target),
    ] {
        if values == registration.values() {
            return Ok(Some(registration));
        }
    }
    anyhow::bail!("registered product differs from its complete current or legacy registration")
}

fn validate_snapshot(snapshot: &RegistrationSnapshot) -> Result<()> {
    if let Some(values) = &snapshot.values {
        for (name, (kind, bytes)) in values {
            ensure!(
                NAMES.contains(&name.as_str()) && bytes.len() <= 65536,
                "registration recovery contains an unsupported value"
            );
            if matches!(name.as_str(), "NoModify" | "NoRepair") {
                ensure!(
                    *kind == 4 && bytes.len() == 4,
                    "invalid registration DWORD snapshot"
                );
            } else {
                ensure!(
                    *kind == 1 && bytes.len() >= 2 && bytes.len() % 2 == 0,
                    "invalid registration text snapshot"
                );
                let units: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                ensure!(
                    units.last() == Some(&0) && !units[..units.len() - 1].contains(&0),
                    "invalid registration text terminator"
                );
                String::from_utf16(&units[..units.len() - 1])?;
            }
        }
    }
    Ok(())
}
const NAMES: [&str; 8] = [
    "DisplayName",
    "DisplayVersion",
    "InstallLocation",
    "UninstallString",
    "QuietUninstallString",
    "NoModify",
    "NoRepair",
    "ModifyPath",
];
type Handle = *mut c_void;
const HKCU: Handle = 0x80000001u32 as i32 as isize as Handle;
#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(
        key: Handle,
        name: *const u16,
        options: u32,
        access: u32,
        output: *mut Handle,
    ) -> i32;
    fn RegCreateKeyExW(
        key: Handle,
        name: *const u16,
        reserved: u32,
        class: *mut u16,
        options: u32,
        access: u32,
        security: *const c_void,
        output: *mut Handle,
        disposition: *mut u32,
    ) -> i32;
    fn RegQueryValueExW(
        key: Handle,
        name: *const u16,
        reserved: *mut u32,
        kind: *mut u32,
        data: *mut u8,
        size: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        key: Handle,
        name: *const u16,
        reserved: u32,
        kind: u32,
        data: *const u8,
        size: u32,
    ) -> i32;
    fn RegDeleteKeyExW(key: Handle, name: *const u16, access: u32, reserved: u32) -> i32;
    fn RegDeleteValueW(key: Handle, name: *const u16) -> i32;
    fn RegQueryInfoKeyW(
        key: Handle,
        class: *mut u16,
        class_len: *mut u32,
        reserved: *mut u32,
        subkeys: *mut u32,
        max_subkey: *mut u32,
        max_class: *mut u32,
        values: *mut u32,
        max_name: *mut u32,
        max_value: *mut u32,
        security: *mut u32,
        time: *mut c_void,
    ) -> i32;
    fn RegFlushKey(key: Handle) -> i32;
    fn RegCloseKey(key: Handle) -> i32;
}
struct Key(Handle);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn key_name(id: &str) -> Result<Vec<u16>> {
    avelin_core::safe_id(id)?;
    Ok(wide(&format!(
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.{id}"
    )))
}
fn raw_snapshot(id: &str) -> Result<Option<Values>> {
    let mut handle = ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKCU,
            key_name(id)?.as_ptr(),
            0,
            0x0001 | 0x0100,
            &mut handle,
        )
    };
    if status == 2 {
        return Ok(None);
    }
    ensure!(status == 0, "open product registration ({status})");
    let key = Key(handle);
    Ok(Some(snapshot_key(&key)?))
}

fn snapshot_key(key: &Key) -> Result<Values> {
    let (mut count, mut children) = (0, 0);
    let status = unsafe {
        RegQueryInfoKeyW(
            key.0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut children,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut count,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    ensure!(
        status == 0 && children == 0,
        "product registration has unexpected subkeys or cannot be inspected"
    );
    let mut values = Values::new();
    for name in NAMES {
        let (mut kind, mut size) = (0, 0);
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                wide(name).as_ptr(),
                ptr::null_mut(),
                &mut kind,
                ptr::null_mut(),
                &mut size,
            )
        };
        if status == 2 {
            continue;
        }
        ensure!(
            status == 0 && size <= 65536,
            "read product registration ({status})"
        );
        let mut bytes = vec![0; size as usize];
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                wide(name).as_ptr(),
                ptr::null_mut(),
                &mut kind,
                bytes.as_mut_ptr(),
                &mut size,
            )
        };
        ensure!(
            status == 0 && size as usize <= bytes.len(),
            "registration changed during read"
        );
        bytes.truncate(size as usize);
        values.insert(name.to_owned(), (kind, bytes));
    }
    ensure!(
        count as usize == values.len(),
        "product registration contains unrelated values; preserve it"
    );
    Ok(values)
}
pub fn verify_registration(id: &str, expected: Option<&Registration>) -> Result<()> {
    ensure!(
        raw_snapshot(id)? == expected.map(Registration::values),
        "product registration does not match the owned installation"
    );
    Ok(())
}

const REGISTRATION_ACCESS: u32 = 0x0001 | 0x0002 | 0x0001_0000 | 0x0100;

/// A live identity for this operation's registry mutations. Holding a key does
/// not lock its values: snapshots are rechecked before each write. Observable
/// competing changes cause conservative failure; registered values are
/// neither atomic nor isolated from an unrelated writer to this same key.
/// Crucially, recovery never reopens an updated/created key by pathname.
pub struct RegistrationChange {
    id: String,
    key: Option<Key>,
    before: Option<Values>,
    expected: Values,
    created: bool,
    deleted: bool,
    modified: bool,
    observer: Option<RegistrationObserver>,
}

fn create_new_key(id: &str) -> Result<Key> {
    let mut handle = ptr::null_mut();
    let mut disposition = 0;
    let status = unsafe {
        RegCreateKeyExW(
            HKCU,
            key_name(id)?.as_ptr(),
            0,
            ptr::null_mut(),
            0,
            REGISTRATION_ACCESS,
            ptr::null(),
            &mut handle,
            &mut disposition,
        )
    };
    ensure!(status == 0, "create product registration ({status})");
    let key = Key(handle);
    ensure!(
        disposition == 1,
        "product registration appeared before publication; existing key was preserved"
    );
    Ok(key)
}

fn delete_key(key: &Key) -> Result<()> {
    // An empty relative name deletes this opened key. Never resolve the product
    // pathname again: an externally recreated key is a different object.
    let status = unsafe { RegDeleteKeyExW(key.0, wide("").as_ptr(), 0x0100, 0) };
    ensure!(status == 0, "delete opened product registration ({status})");
    Ok(())
}

fn flush_registration_parent() -> Result<()> {
    let mut parent = ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKCU,
            wide(r"Software\Microsoft\Windows\CurrentVersion\Uninstall").as_ptr(),
            0,
            0x0001 | 0x0100,
            &mut parent,
        )
    };
    ensure!(
        status == 0,
        "open product registration parent for flush ({status})"
    );
    let parent = Key(parent);
    ensure!(
        unsafe { RegFlushKey(parent.0) } == 0,
        "flush product registration deletion"
    );
    Ok(())
}

impl RegistrationChange {
    fn observe(
        &mut self,
        before: &RegistrationSnapshot,
        after: &RegistrationSnapshot,
        complete: bool,
    ) -> Result<()> {
        if let Some(observer) = self.observer.as_mut() {
            observer(before, after, complete)?;
        }
        Ok(())
    }

    fn state(&self) -> RegistrationSnapshot {
        RegistrationSnapshot {
            values: if self.deleted {
                None
            } else {
                Some(self.expected.clone())
            },
        }
    }

    fn matches_current(&self) -> Result<()> {
        let key = self
            .key
            .as_ref()
            .context("registration handle is no longer live")?;
        ensure!(
            snapshot_key(key)? == self.expected,
            "registration changed externally; preserve its current values"
        );
        Ok(())
    }

    fn write_values(&mut self, values: &Values) -> Result<()> {
        // Compensation can target a partial or empty pre-operation key.
        let removed: Vec<String> = self
            .expected
            .keys()
            .filter(|name| !values.contains_key(*name))
            .cloned()
            .collect();
        for name in removed {
            self.matches_current()?;
            let before = self.state();
            let mut after = before.clone();
            after.values.as_mut().unwrap().remove(&name);
            self.observe(&before, &after, false)?;
            self.matches_current()?;
            let key = self.key.as_ref().context("registration handle missing")?;
            let status = unsafe { RegDeleteValueW(key.0, wide(&name).as_ptr()) };
            ensure!(status == 0, "delete product registration value ({status})");
            self.modified = true;
            self.expected.remove(&name);
            self.flush_and_observe(&before, &after)?;
        }
        for (name, (kind, bytes)) in values {
            self.matches_current()?;
            if self.expected.get(name) == Some(&(*kind, bytes.clone())) {
                continue;
            }
            let before = self.state();
            let mut after = before.clone();
            after
                .values
                .as_mut()
                .unwrap()
                .insert(name.clone(), (*kind, bytes.clone()));
            self.observe(&before, &after, false)?;
            self.matches_current()?;
            let key = self.key.as_ref().context("registration handle missing")?;
            let status = unsafe {
                RegSetValueExW(
                    key.0,
                    wide(name).as_ptr(),
                    0,
                    *kind,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                )
            };
            ensure!(status == 0, "write product registration ({status})");
            // Record only a successful value mutation; a failed first write
            // never authorizes recovery to replace an external state.
            self.modified = true;
            self.expected.insert(name.clone(), (*kind, bytes.clone()));
            self.flush_and_observe(&before, &after)?;
        }
        self.matches_current()?;
        ensure!(
            unsafe { RegFlushKey(self.key.as_ref().unwrap().0) } == 0,
            "flush product registration"
        );
        Ok(())
    }

    fn flush_and_observe(
        &mut self,
        before: &RegistrationSnapshot,
        after: &RegistrationSnapshot,
    ) -> Result<()> {
        ensure!(
            unsafe { RegFlushKey(self.key.as_ref().context("registration handle missing")?.0) }
                == 0,
            "flush product registration mutation"
        );
        self.matches_current()?;
        self.observe(before, after, true)
    }

    fn remove(&mut self) -> Result<()> {
        self.matches_current()?;
        let before = self.state();
        let after = RegistrationSnapshot { values: None };
        self.observe(&before, &after, false)?;
        self.matches_current()?;
        delete_key(self.key.as_ref().context("registration handle missing")?)?;
        self.deleted = true;
        self.modified = true;
        // The deleted object has no values that recovery may update. Close it
        // before CREATE_NEW restoration; any replacement key must be preserved.
        self.key.take();
        flush_registration_parent()?;
        ensure!(
            raw_snapshot(&self.id)?.is_none(),
            "product registration was recreated; preserve it"
        );
        self.observe(&before, &after, true)
    }

    pub fn restore(&mut self) -> Result<()> {
        if !self.created && !self.modified {
            return Ok(());
        }
        if self.deleted {
            let before = self
                .before
                .clone()
                .context("deleted registration has no original snapshot")?;
            let absent = RegistrationSnapshot { values: None };
            let empty = RegistrationSnapshot {
                values: Some(Values::new()),
            };
            self.observe(&absent, &empty, false)?;
            let key = create_new_key(&self.id)?;
            self.key = Some(key);
            self.expected = Values::new();
            self.created = true;
            self.deleted = false;
            self.flush_and_observe(&absent, &empty)?;
            // A failure leaves a guarded, known partial restoration and is
            // reported Unknown. It never falls back to a pathname overwrite.
            self.write_values(&before)?;
            return Ok(());
        }
        self.matches_current()?;
        if let Some(before) = self.before.clone() {
            self.write_values(&before)?;
        } else {
            self.remove()?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub fn replace_registration(
    id: &str,
    before: Option<&Registration>,
    after: Option<&Registration>,
    change: &mut Option<RegistrationChange>,
) -> Result<()> {
    replace_registration_observed(id, before, after, change, None)
}

pub(crate) fn replace_registration_observed(
    id: &str,
    before: Option<&Registration>,
    after: Option<&Registration>,
    change: &mut Option<RegistrationChange>,
    mut observer: Option<RegistrationObserver>,
) -> Result<()> {
    ensure!(
        change.is_none(),
        "registration mutation guard already exists"
    );
    let before_values = before.map(Registration::values);
    let after_values = after.map(Registration::values);
    if before_values.is_none() && after_values.is_none() {
        return verify_registration(id, None);
    }
    let (key, created) = if let Some(expected) = &before_values {
        let mut handle = ptr::null_mut();
        let status = unsafe {
            RegOpenKeyExW(
                HKCU,
                key_name(id)?.as_ptr(),
                0,
                REGISTRATION_ACCESS,
                &mut handle,
            )
        };
        ensure!(
            status == 0,
            "open owned product registration ({status}); nothing was changed"
        );
        let key = Key(handle);
        ensure!(
            snapshot_key(&key)? == *expected,
            "product registration changed before mutation; external values were preserved"
        );
        (key, false)
    } else {
        if let Some(record) = observer.as_mut() {
            record(
                &RegistrationSnapshot { values: None },
                &RegistrationSnapshot {
                    values: Some(Values::new()),
                },
                false,
            )?;
        }
        (create_new_key(id)?, true)
    };
    // CREATE_NEW already changed the registry; retain its exact handle before
    // any further fallible work. Opening an existing key has changed nothing.
    *change = Some(RegistrationChange {
        id: id.into(),
        key: Some(key),
        before: before_values.clone(),
        expected: before_values.unwrap_or_default(),
        created,
        deleted: false,
        modified: false,
        observer,
    });
    let guard = change.as_mut().unwrap();
    if created {
        guard.flush_and_observe(
            &RegistrationSnapshot { values: None },
            &RegistrationSnapshot {
                values: Some(Values::new()),
            },
        )?;
    }
    if let Some(values) = after_values {
        guard.write_values(&values)?;
    } else {
        guard.remove()?;
    }
    verify_registration(id, after)
}

/// The caller must authorize expected_current from durable, task-bound evidence.
/// A fresh pinned key is compared before every mutation. A competing writer or
/// recreated key is preserved. Every compensation has the same durable observer.
pub(crate) fn restore_registration_snapshot(
    id: &str,
    expected_current: &RegistrationSnapshot,
    target: &RegistrationSnapshot,
    mut observer: Option<RegistrationObserver>,
) -> Result<()> {
    validate_snapshot(expected_current)?;
    validate_snapshot(target)?;
    ensure!(
        registration_snapshot(id)? == *expected_current,
        "product registration changed since recovery inspection"
    );
    if expected_current == target {
        return Ok(());
    }
    let (key, created) = if let Some(expected) = &expected_current.values {
        let mut handle = ptr::null_mut();
        let status = unsafe {
            RegOpenKeyExW(
                HKCU,
                key_name(id)?.as_ptr(),
                0,
                REGISTRATION_ACCESS,
                &mut handle,
            )
        };
        ensure!(status == 0, "open recovery registration ({status})");
        let key = Key(handle);
        ensure!(
            snapshot_key(&key)? == *expected,
            "registration changed before recovery; preserve it"
        );
        (key, false)
    } else {
        if let Some(record) = observer.as_mut() {
            record(
                expected_current,
                &RegistrationSnapshot {
                    values: Some(Values::new()),
                },
                false,
            )?;
        }
        (create_new_key(id)?, true)
    };
    let mut guard = RegistrationChange {
        id: id.into(),
        key: Some(key),
        before: expected_current.values.clone(),
        expected: expected_current.values.clone().unwrap_or_default(),
        created,
        deleted: false,
        modified: false,
        observer,
    };
    if created {
        guard.flush_and_observe(
            expected_current,
            &RegistrationSnapshot {
                values: Some(Values::new()),
            },
        )?;
    }
    if let Some(values) = &target.values {
        guard.write_values(values)?;
    } else {
        guard.remove()?;
    }
    ensure!(
        registration_snapshot(id)? == *target,
        "recovered product registration did not verify"
    );
    Ok(())
}

/// Test-fixture cleanup with the same strict compare/update path. Production
/// compensation uses the live RegistrationChange instead of reacquiring names.
#[cfg(test)]
pub fn restore_registration(
    id: &str,
    before: Option<&Registration>,
    after: Option<&Registration>,
) -> Result<()> {
    if raw_snapshot(id)? == before.map(Registration::values) {
        return Ok(());
    }
    replace_registration(id, after, before, &mut None)
}

pub fn shortcut_path(app_id: &str, kind: ShortcutKind) -> Result<PathBuf> {
    avelin_core::safe_id(app_id)?;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHGetFolderPathW(
            window: Handle,
            folder: i32,
            token: Handle,
            flags: u32,
            path: *mut u16,
        ) -> i32;
    }
    let mut path = [0u16; 260];
    let folder = match kind {
        ShortcutKind::Desktop => 0x10,
        ShortcutKind::StartMenu => 0x02,
    };
    ensure!(
        unsafe {
            SHGetFolderPathW(
                ptr::null_mut(),
                folder,
                ptr::null_mut(),
                0,
                path.as_mut_ptr(),
            )
        } >= 0,
        "resolve current-user shortcut folder"
    );
    let end = path
        .iter()
        .position(|v| *v == 0)
        .context("invalid shell folder")?;
    let result = PathBuf::from(String::from_utf16(&path[..end])?).join(format!("{app_id}.lnk"));
    avelin_core::reject_reparse(&result)?;
    Ok(result)
}

/// Serialize the shell link in memory; filesystem publication uses CREATE_NEW.
pub fn shortcut_bytes(target: &Path, working_dir: &Path) -> Result<Vec<u8>> {
    use windows::{
        Win32::{
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize, IPersistStream, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
            },
            UI::Shell::{IShellLinkW, SHCreateMemStream, ShellLink},
        },
        core::{Interface, PCWSTR},
    };
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }
    let _apartment = Apartment;
    let target = wide(target.to_str().context("shortcut target Unicode")?);
    let working = wide(
        working_dir
            .to_str()
            .context("shortcut working directory Unicode")?,
    );
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(PCWSTR(target.as_ptr()))?;
        link.SetWorkingDirectory(PCWSTR(working.as_ptr()))?;
        let stream = SHCreateMemStream(None).context("allocate shortcut memory stream")?;
        let persist: IPersistStream = link.cast()?;
        persist.Save(&stream, true)?;
        let mut stat = STATSTG::default();
        stream.Stat(&mut stat, STATFLAG_NONAME)?;
        ensure!(
            stat.cbSize > 0 && stat.cbSize <= 1024 * 1024,
            "shortcut serialization size exceeded"
        );
        stream.Seek(0, STREAM_SEEK_SET, None)?;
        let mut bytes = vec![0; stat.cbSize as usize];
        let mut read = 0;
        stream
            .Read(
                bytes.as_mut_ptr().cast(),
                bytes.len() as u32,
                Some(&mut read),
            )
            .ok()?;
        ensure!(
            read as usize == bytes.len(),
            "shortcut serialization was truncated"
        );
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_registration_accepts_only_complete_current_or_legacy_shapes() {
        let fixture = RegistryFixture::new();
        let mut manifest: Manifest =
            serde_json::from_str(include_str!("../../../docs/schemas/examples/manifest.json"))
                .unwrap();
        manifest.app_id = fixture.id.clone();
        let target = Path::new(r"C:\Apps\示例应用");
        let old = Registration::legacy(&manifest, target);
        let current = Registration::new(&manifest, target);
        assert_eq!(old.values().len(), 7);
        assert_eq!(current.values().len(), 8);
        assert_eq!(
            current.modify_path.as_deref(),
            Some(
                r#""C:\Apps\示例应用\.avelin\avelin-app.exe" --gui --maintenance --install-dir "C:\Apps\示例应用""#
            )
        );
        let old_json = serde_json::to_value(&old).unwrap();
        assert!(old_json.get("modify_path").is_none());
        assert_eq!(
            serde_json::from_value::<Registration>(old_json).unwrap(),
            old
        );
        fixture.install(&old);
        assert_eq!(
            registration_for_product(&manifest, target).unwrap(),
            Some(old.clone())
        );
        let mut change = None;
        replace_registration(&fixture.id, Some(&old), Some(&current), &mut change).unwrap();
        assert_eq!(
            registration_for_product(&manifest, target).unwrap(),
            Some(current.clone())
        );
        assert!(snapshot_matches_product(
            &registration_expected(Some(&old)),
            &manifest,
            target
        ));
        assert!(snapshot_matches_product(
            &registration_expected(Some(&current)),
            &manifest,
            target
        ));
        change.as_mut().unwrap().restore().unwrap();
        verify_registration(&fixture.id, Some(&old)).unwrap();

        let mut malformed = current.clone();
        malformed.modify_path = None;
        replace_registration(&fixture.id, Some(&old), Some(&malformed), &mut None).unwrap();
        assert!(registration_for_product(&manifest, target).is_err());
        assert!(!snapshot_matches_product(
            &registration_expected(Some(&malformed)),
            &manifest,
            target
        ));
        let mut foreign = current.clone();
        foreign.modify_path = Some("unrelated.exe --maintenance".into());
        assert!(!snapshot_matches_product(
            &registration_expected(Some(&foreign)),
            &manifest,
            target
        ));
    }

    struct RegistryFixture {
        id: String,
    }
    impl RegistryFixture {
        fn new() -> Self {
            Self {
                id: format!("org.avelin.registry-test.{}", uuid::Uuid::new_v4().simple()),
            }
        }
        fn registration(&self, version: &str) -> Registration {
            Registration {
                display_name: "Test fixture".into(),
                display_version: version.into(),
                install_location: r"C:\Avelin Test".into(),
                uninstall_string: "test uninstall".into(),
                quiet_uninstall_string: "test quiet uninstall".into(),
                modify_path: None,
                no_modify: 1,
                no_repair: 1,
            }
        }
        fn install(&self, registration: &Registration) {
            replace_registration(&self.id, None, Some(registration), &mut None).unwrap();
        }
        fn delete(&self) {
            assert_eq!(
                unsafe { RegDeleteKeyExW(HKCU, key_name(&self.id).unwrap().as_ptr(), 0x0100, 0) },
                0
            );
        }
    }
    impl Drop for RegistryFixture {
        fn drop(&mut self) {
            // Exact UUID key allocated by this fixture; never a real product.
            let _ =
                unsafe { RegDeleteKeyExW(HKCU, key_name(&self.id).unwrap().as_ptr(), 0x0100, 0) };
        }
    }

    #[test]
    fn persisted_registry_steps_can_restore_a_partial_update_after_handles_are_dropped() {
        use std::{cell::RefCell, rc::Rc};
        let fixture = RegistryFixture::new();
        let old = fixture.registration("1");
        let mut next = fixture.registration("2");
        next.display_name = "Updated fixture".into();
        fixture.install(&old);
        let events = Rc::new(RefCell::new(Vec::new()));
        let captured = events.clone();
        let id = fixture.id.clone();
        let observer: RegistrationObserver = Box::new(move |before, after, complete| {
            assert_eq!(
                registration_snapshot(&id)?,
                if complete { after } else { before }.clone()
            );
            captured
                .borrow_mut()
                .push((before.clone(), after.clone(), complete));
            ensure!(
                !complete,
                "simulate interruption immediately after one flushed write"
            );
            Ok(())
        });
        let mut change = None;
        assert!(
            replace_registration_observed(
                &fixture.id,
                Some(&old),
                Some(&next),
                &mut change,
                Some(observer)
            )
            .is_err()
        );
        drop(change);
        let partial = registration_snapshot(&fixture.id).unwrap();
        assert_ne!(partial, registration_expected(Some(&old)));
        assert_ne!(partial, registration_expected(Some(&next)));
        assert_eq!(events.borrow().len(), 2);
        assert_eq!(events.borrow()[0].1, partial);
        restore_registration_snapshot(
            &fixture.id,
            &partial,
            &registration_expected(Some(&old)),
            None,
        )
        .unwrap();
        verify_registration(&fixture.id, Some(&old)).unwrap();
    }

    #[test]
    fn interrupted_registry_compensation_can_be_retried_from_its_recorded_partial_state() {
        let fixture = RegistryFixture::new();
        let old = fixture.registration("1");
        let mut next = fixture.registration("2");
        next.display_name = "Changed name".into();
        fixture.install(&next);
        let before = registration_expected(Some(&next));
        let target = registration_expected(Some(&old));
        let id = fixture.id.clone();
        let observer: RegistrationObserver = Box::new(move |before, after, complete| {
            assert_eq!(
                registration_snapshot(&id)?,
                if complete { after } else { before }.clone()
            );
            ensure!(!complete, "simulate interruption during compensation");
            Ok(())
        });
        assert!(
            restore_registration_snapshot(&fixture.id, &before, &target, Some(observer)).is_err()
        );
        let interrupted = registration_snapshot(&fixture.id).unwrap();
        assert_ne!(interrupted, before);
        assert_ne!(interrupted, target);
        restore_registration_snapshot(&fixture.id, &interrupted, &target, None).unwrap();
        restore_registration_snapshot(&fixture.id, &target, &target, None).unwrap();
        verify_registration(&fixture.id, Some(&old)).unwrap();
    }

    #[test]
    fn registry_recovery_preserves_external_changes_and_rejects_unregistered_values() {
        let fixture = RegistryFixture::new();
        let old = fixture.registration("1");
        let external = fixture.registration("external");
        fixture.install(&external);
        assert!(
            restore_registration_snapshot(
                &fixture.id,
                &registration_expected(Some(&old)),
                &registration_expected(None),
                None
            )
            .is_err()
        );
        verify_registration(&fixture.id, Some(&external)).unwrap();
        let mut invalid = registration_expected(Some(&external));
        invalid
            .values
            .as_mut()
            .unwrap()
            .insert("External".into(), (4, vec![0; 4]));
        assert!(
            restore_registration_snapshot(
                &fixture.id,
                &registration_expected(Some(&external)),
                &invalid,
                None
            )
            .is_err()
        );
        verify_registration(&fixture.id, Some(&external)).unwrap();
    }

    #[test]
    fn interrupted_key_creation_retains_the_empty_state_for_cross_process_cleanup() {
        let fixture = RegistryFixture::new();
        let next = fixture.registration("1");
        let mut change = None;
        let observer: RegistrationObserver = Box::new(|before, after, complete| {
            assert_eq!(before.values, None);
            assert_eq!(after.values, Some(Values::new()));
            ensure!(!complete, "simulate interruption after key creation");
            Ok(())
        });
        assert!(
            replace_registration_observed(
                &fixture.id,
                None,
                Some(&next),
                &mut change,
                Some(observer)
            )
            .is_err()
        );
        drop(change);
        let empty = RegistrationSnapshot {
            values: Some(Values::new()),
        };
        assert_eq!(registration_snapshot(&fixture.id).unwrap(), empty);
        restore_registration_snapshot(&fixture.id, &empty, &registration_expected(None), None)
            .unwrap();
        assert_eq!(
            registration_snapshot(&fixture.id).unwrap(),
            registration_expected(None)
        );
    }

    #[test]
    fn missing_or_modified_registration_is_rejected_without_a_mutation_guard() {
        let fixture = RegistryFixture::new();
        let old = fixture.registration("1");
        let next = fixture.registration("2");
        let mut change = None;
        assert!(replace_registration(&fixture.id, Some(&old), Some(&next), &mut change).is_err());
        assert!(change.is_none());
        assert_eq!(raw_snapshot(&fixture.id).unwrap(), None);
        let mut external = fixture.registration("external");
        external.display_name = "An external owner".into();
        fixture.install(&external);
        assert!(replace_registration(&fixture.id, Some(&old), Some(&next), &mut change).is_err());
        assert!(change.is_none());
        verify_registration(&fixture.id, Some(&external)).unwrap();
    }

    #[test]
    fn fresh_publication_does_not_adopt_a_key_created_after_preflight() {
        let fixture = RegistryFixture::new();
        let next = fixture.registration("2");
        verify_registration(&fixture.id, None).unwrap();
        let external = fixture.registration("external");
        fixture.install(&external);
        let mut change = None;
        assert!(replace_registration(&fixture.id, None, Some(&next), &mut change).is_err());
        assert!(change.is_none());
        verify_registration(&fixture.id, Some(&external)).unwrap();
    }

    #[test]
    fn recovery_refuses_external_values_even_when_its_own_updates_preceded_them() {
        let fixture = RegistryFixture::new();
        let old = fixture.registration("1");
        let next = fixture.registration("2");
        fixture.install(&old);
        let mut change = None;
        replace_registration(&fixture.id, Some(&old), Some(&next), &mut change).unwrap();
        let external = fixture.registration("external");
        let mut external_change = None;
        replace_registration(
            &fixture.id,
            Some(&next),
            Some(&external),
            &mut external_change,
        )
        .unwrap();
        assert!(change.as_mut().unwrap().restore().is_err());
        verify_registration(&fixture.id, Some(&external)).unwrap();
    }

    #[test]
    fn recovery_keeps_the_original_handle_and_never_updates_a_recreated_key() {
        let fixture = RegistryFixture::new();
        let old = fixture.registration("1");
        let next = fixture.registration("2");
        fixture.install(&old);
        let mut change = None;
        replace_registration(&fixture.id, Some(&old), Some(&next), &mut change).unwrap();
        fixture.delete();
        let external = fixture.registration("external");
        let mut external_change = None;
        let recreated =
            replace_registration(&fixture.id, None, Some(&external), &mut external_change).is_ok();
        assert!(change.as_mut().unwrap().restore().is_err());
        drop(change);
        if !recreated {
            fixture.install(&external);
        }
        verify_registration(&fixture.id, Some(&external)).unwrap();
    }

    #[test]
    fn successful_own_deletion_cannot_restore_over_a_replacement_key() {
        let fixture = RegistryFixture::new();
        let old = fixture.registration("1");
        fixture.install(&old);
        let mut change = None;
        replace_registration(&fixture.id, Some(&old), None, &mut change).unwrap();
        let external = fixture.registration("external");
        fixture.install(&external);
        assert!(change.as_mut().unwrap().restore().is_err());
        verify_registration(&fixture.id, Some(&external)).unwrap();
    }

    #[test]
    fn empty_key_created_by_this_operation_is_cleaned_if_the_first_value_write_fails() {
        let fixture = RegistryFixture::new();
        let created = create_new_key(&fixture.id).unwrap();
        let mut handle = ptr::null_mut();
        // Open the same new key with QUERY and DELETE but without SET_VALUE to
        // get a real OS access-denied result on the first RegSetValueExW call.
        assert_eq!(
            unsafe {
                RegOpenKeyExW(
                    created.0,
                    wide("").as_ptr(),
                    0,
                    0x0001 | 0x0001_0000 | 0x0100,
                    &mut handle,
                )
            },
            0
        );
        drop(created);
        let mut change = RegistrationChange {
            id: fixture.id.clone(),
            key: Some(Key(handle)),
            before: None,
            expected: Values::new(),
            created: true,
            deleted: false,
            modified: false,
            observer: None,
        };
        assert!(
            change
                .write_values(&fixture.registration("1").values())
                .is_err()
        );
        assert!(!change.modified);
        assert_eq!(raw_snapshot(&fixture.id).unwrap(), Some(Values::new()));
        change.restore().unwrap();
        assert_eq!(raw_snapshot(&fixture.id).unwrap(), None);
    }

    #[test]
    fn shell_link_roundtrips_the_exact_unicode_target_and_working_directory() {
        use windows::{
            Win32::{
                System::Com::{
                    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance,
                    CoInitializeEx, CoUninitialize, IPersistStream,
                },
                UI::Shell::{IShellLinkW, SHCreateMemStream, ShellLink},
            },
            core::Interface,
        };
        let working = std::env::temp_dir().join("Avelin 快捷方式空格 🦀 العربية");
        let target = working.join("应用 🦀.exe");
        let bytes = shortcut_bytes(&target, &working).unwrap();
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            {
                let link: IShellLinkW =
                    CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
                let persist: IPersistStream = link.cast().unwrap();
                let stream = SHCreateMemStream(Some(&bytes)).unwrap();
                persist.Load(&stream).unwrap();
                let mut actual = [0u16; 1024];
                link.GetPath(&mut actual, ptr::null_mut(), 4).unwrap();
                let end = actual.iter().position(|v| *v == 0).unwrap();
                assert_eq!(
                    String::from_utf16(&actual[..end]).unwrap(),
                    target.to_str().unwrap()
                );
                let mut actual = [0u16; 1024];
                link.GetWorkingDirectory(&mut actual).unwrap();
                let end = actual.iter().position(|v| *v == 0).unwrap();
                assert_eq!(
                    String::from_utf16(&actual[..end]).unwrap(),
                    working.to_str().unwrap()
                );
            }
            CoUninitialize();
        }
    }

    #[test]
    fn staged_shell_link_resolves_after_the_unicode_target_is_published() {
        use std::fs;
        use windows::{
            Win32::{
                System::Com::{
                    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance,
                    CoInitializeEx, CoUninitialize, IPersistFile, STGM_READ,
                },
                UI::Shell::{IShellLinkW, ShellLink},
            },
            core::{Interface, PCWSTR},
        };
        let root = std::env::temp_dir().join(format!("avelin-link-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let working = root.join("installed 中文 🦀 العربية");
        fs::create_dir(&working).unwrap();
        let target = working.join("app.exe");
        let shortcut = root.join("sample.lnk");
        assert!(!target.exists());
        // Deployment stages links before publishing the payload. Verify the
        // persisted link through the Unicode file interface after publication,
        // including characters outside legacy ANSI code pages.
        let bytes = shortcut_bytes(&target, &working).unwrap();
        fs::write(&shortcut, &bytes).unwrap();
        fs::write(&target, b"shortcut target fixture; never executed").unwrap();
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            {
                let link: IShellLinkW =
                    CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
                let persist: IPersistFile = link.cast().unwrap();
                persist
                    .Load(PCWSTR(wide(shortcut.to_str().unwrap()).as_ptr()), STGM_READ)
                    .unwrap();
                for flags in [0, 4] {
                    let mut actual = [0u16; 1024];
                    link.GetPath(&mut actual, ptr::null_mut(), flags).unwrap();
                    let end = actual.iter().position(|v| *v == 0).unwrap();
                    assert_eq!(
                        String::from_utf16(&actual[..end]).unwrap(),
                        target.to_str().unwrap(),
                        "GetPath flags={flags}"
                    );
                }
                let mut actual = [0u16; 1024];
                link.GetWorkingDirectory(&mut actual).unwrap();
                let end = actual.iter().position(|v| *v == 0).unwrap();
                assert_eq!(
                    String::from_utf16(&actual[..end]).unwrap(),
                    working.to_str().unwrap()
                );
                let mut arguments = [0u16; 1024];
                link.GetArguments(&mut arguments).unwrap();
                assert_eq!(arguments[0], 0);
            }
            CoUninitialize();
        }
        assert_eq!(fs::read(&shortcut).unwrap(), bytes);
        fs::remove_file(&shortcut).unwrap();
        fs::remove_file(&target).unwrap();
        fs::remove_dir(&working).unwrap();
        fs::remove_dir(&root).unwrap();
    }
}
