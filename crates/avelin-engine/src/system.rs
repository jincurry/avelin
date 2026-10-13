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
            no_modify: 1,
            no_repair: 1,
        }
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
        values
    }
}

type Values = BTreeMap<String, (u32, Vec<u8>)>;
const NAMES: [&str; 7] = [
    "DisplayName",
    "DisplayVersion",
    "InstallLocation",
    "UninstallString",
    "QuietUninstallString",
    "NoModify",
    "NoRepair",
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
/// competing changes cause conservative failure; seven registry values are
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

impl RegistrationChange {
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
        for (name, (kind, bytes)) in values {
            self.matches_current()?;
            if self.expected.get(name) == Some(&(*kind, bytes.clone())) {
                continue;
            }
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
        }
        self.matches_current()?;
        ensure!(
            unsafe { RegFlushKey(self.key.as_ref().unwrap().0) } == 0,
            "flush product registration"
        );
        Ok(())
    }

    fn remove(&mut self) -> Result<()> {
        self.matches_current()?;
        delete_key(self.key.as_ref().context("registration handle missing")?)?;
        self.deleted = true;
        self.modified = true;
        // The deleted object has no values that recovery may update. Close it
        // before CREATE_NEW restoration; any replacement key must be preserved.
        self.key.take();
        Ok(())
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
            let key = create_new_key(&self.id)?;
            self.key = Some(key);
            self.expected = Values::new();
            self.created = true;
            self.deleted = false;
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

pub fn replace_registration(
    id: &str,
    before: Option<&Registration>,
    after: Option<&Registration>,
    change: &mut Option<RegistrationChange>,
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
    });
    let guard = change.as_mut().unwrap();
    if let Some(values) = after_values {
        guard.write_values(&values)?;
    } else {
        guard.remove()?;
    }
    verify_registration(id, after)
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
        let working = std::env::temp_dir().join("Avelin 快捷方式空格");
        let target = working.join("应用.exe");
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
}
