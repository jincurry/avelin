//! Installation scope and the Windows machine-storage trust boundary.
//! Machine operations never derive system paths from caller-controlled environment
//! variables, and never adopt a pre-existing user-writable state directory.
use crate::ShortcutKind;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallScope {
    #[default]
    CurrentUser,
    AllUsers,
}

impl InstallScope {
    pub fn is_current_user(&self) -> bool {
        *self == Self::CurrentUser
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    crate::same_windows_name(
        &a.to_string_lossy().replace('/', "\\"),
        &b.to_string_lossy().replace('/', "\\"),
    )
}

pub fn require_elevation(scope: InstallScope) -> Result<()> {
    ensure!(
        scope == InstallScope::CurrentUser || is_elevated()?,
        "all-users installation requires an elevated administrator"
    );
    if scope == InstallScope::AllUsers {
        platform::require_machine_owner()?;
    }
    Ok(())
}

pub fn default_install_root(scope: InstallScope, directory_name: &str) -> Result<PathBuf> {
    crate::relative_path(directory_name)?;
    ensure!(
        !directory_name.contains(['/', '\\']),
        "installation directory name must be one component"
    );
    let base = match scope {
        InstallScope::CurrentUser => {
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is missing")?)
        }
        InstallScope::AllUsers => platform::folder(0x26)?,
    };
    let result = base.join(directory_name);
    validate_target(scope, &result)?;
    Ok(result)
}

pub fn shortcut_folder(scope: InstallScope, kind: ShortcutKind) -> Result<PathBuf> {
    platform::folder(match (scope, kind) {
        (InstallScope::CurrentUser, ShortcutKind::Desktop) => 0x10,
        (InstallScope::CurrentUser, ShortcutKind::StartMenu) => 0x02,
        (InstallScope::AllUsers, ShortcutKind::Desktop) => 0x19,
        (InstallScope::AllUsers, ShortcutKind::StartMenu) => 0x17,
    })
}

pub fn state_root(scope: InstallScope) -> Result<PathBuf> {
    Ok(match scope {
        InstallScope::CurrentUser => {
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is missing")?)
                .join("Avelin/M0")
        }
        InstallScope::AllUsers => platform::folder(0x23)?.join("Avelin/Machine"),
    })
}

pub fn product_lock_path(scope: InstallScope, app_id: &str, sid: &str) -> Result<PathBuf> {
    crate::safe_id(app_id)?;
    let owner = if scope == InstallScope::AllUsers {
        "machine"
    } else {
        sid
    };
    Ok(state_root(scope)?.join("locks").join(format!(
        "{app_id}-{}.lock",
        &crate::hash_bytes(owner.as_bytes())[..16]
    )))
}

/// Read-only validation, also used before requesting UAC consent.
pub fn validate_target(scope: InstallScope, target: &Path) -> Result<()> {
    crate::install_root(target)?;
    if scope == InstallScope::CurrentUser {
        return Ok(());
    }
    let program_files = platform::folder(0x26)?;
    ensure!(
        target
            .parent()
            .is_some_and(|parent| same_path(parent, &program_files)),
        "all-users destination must be a direct child of the system Program Files directory"
    );
    crate::reject_reparse(target)?;
    platform::validate_security(&program_files, true)?;
    if target.try_exists()? {
        ensure!(
            target.is_dir(),
            "machine installation destination is not a directory"
        );
        platform::validate_security(target, false)?;
    }
    Ok(())
}

pub fn validate_owned_installation(
    scope: InstallScope,
    target: &Path,
    receipt_sid: &str,
    actor_sid: &str,
) -> Result<()> {
    ensure!(
        !receipt_sid.is_empty() && !actor_sid.is_empty(),
        "installation owner identity is missing"
    );
    match scope {
        InstallScope::CurrentUser => ensure!(
            receipt_sid == actor_sid,
            "installation belongs to another Windows user"
        ),
        InstallScope::AllUsers => {
            require_elevation(scope)?;
            validate_target(scope, target)?;
        }
    }
    Ok(())
}

/// Creates only missing machine-owned state directories; an existing weak or
/// user-owned directory is rejected without changing its security descriptor.
pub fn ensure_machine_state() -> Result<PathBuf> {
    require_elevation(InstallScope::AllUsers)?;
    let program_data = platform::folder(0x23)?;
    let parent = program_data.join("Avelin");
    let root = parent.join("Machine");
    for path in [&parent, &root, &root.join("locks"), &root.join("sessions")] {
        crate::reject_reparse(path)?;
        platform::create_protected_directory(path)?;
        platform::validate_security(path, false)?;
    }
    Ok(root)
}

pub fn validate_session(scope: InstallScope, path: &Path) -> Result<()> {
    if scope == InstallScope::CurrentUser {
        return Ok(());
    }
    require_elevation(scope)?;
    let root = state_root(scope)?;
    let sessions = root.join("sessions");
    ensure!(
        path.parent()
            .is_some_and(|parent| same_path(parent, &sessions)),
        "machine task session must be directly under protected machine sessions"
    );
    for dir in [
        root.parent().context("machine state parent missing")?,
        &root,
        &sessions,
        path,
    ] {
        crate::reject_reparse(dir)?;
        ensure!(dir.is_dir(), "machine task directory is unavailable");
        platform::validate_security(dir, false)?;
    }
    Ok(())
}

/// Called immediately after creating an empty session, before writing inputs.
pub fn protect_machine_session(path: &Path) -> Result<()> {
    require_elevation(InstallScope::AllUsers)?;
    let root = ensure_machine_state()?;
    ensure!(
        path.parent()
            .is_some_and(|parent| same_path(parent, &root.join("sessions"))),
        "new machine session has an unexpected parent"
    );
    crate::reject_reparse(path)?;
    ensure!(
        path.is_dir() && std::fs::read_dir(path)?.next().is_none(),
        "only a new empty machine session can be protected"
    );
    platform::set_protection(path, false)?;
    validate_session(InstallScope::AllUsers, path)
}

/// The engine creates and journals this directory first. Never creates it here.
pub fn ensure_machine_target(path: &Path) -> Result<()> {
    require_elevation(InstallScope::AllUsers)?;
    let program_files = platform::folder(0x26)?;
    ensure!(
        path.parent()
            .is_some_and(|parent| same_path(parent, &program_files)),
        "new machine installation has an unexpected parent"
    );
    crate::reject_reparse(path)?;
    ensure!(
        path.is_dir() && std::fs::read_dir(path)?.next().is_none(),
        "only a new empty installation directory can be protected"
    );
    platform::set_protection(path, true)?;
    validate_target(InstallScope::AllUsers, path)
}

pub fn is_elevated() -> Result<bool> {
    platform::is_elevated()
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        ffi::c_void,
        fs::File,
        os::windows::io::{AsRawHandle, FromRawHandle},
        ptr,
    };
    type Handle = *mut c_void;
    #[repr(C)]
    struct Attributes {
        length: u32,
        descriptor: Handle,
        inherit: i32,
    }
    #[repr(C)]
    struct AclSize {
        count: u32,
        used: u32,
        free: u32,
    }
    #[repr(C)]
    struct AceHeader {
        kind: u8,
        flags: u8,
        size: u16,
    }
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
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn LocalFree(memory: Handle) -> Handle;
        fn CreateDirectoryW(path: *const u16, attributes: *const Attributes) -> i32;
        fn CreateFileW(
            path: *const u16,
            access: u32,
            share: u32,
            security: Handle,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: u32,
            data: Handle,
            length: u32,
            required: *mut u32,
        ) -> i32;
        fn GetSecurityInfo(
            handle: Handle,
            object: u32,
            information: u32,
            owner: *mut Handle,
            group: *mut Handle,
            dacl: *mut Handle,
            sacl: *mut Handle,
            descriptor: *mut Handle,
        ) -> u32;
        fn SetSecurityInfo(
            handle: Handle,
            object: u32,
            information: u32,
            owner: Handle,
            group: Handle,
            dacl: Handle,
            sacl: Handle,
        ) -> u32;
        fn ConvertSidToStringSidW(sid: Handle, text: *mut *mut u16) -> i32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: u32,
            descriptor: *mut Handle,
            size: *mut u32,
        ) -> i32;
        fn GetSecurityDescriptorOwner(
            descriptor: Handle,
            owner: *mut Handle,
            defaulted: *mut i32,
        ) -> i32;
        fn GetSecurityDescriptorDacl(
            descriptor: Handle,
            present: *mut i32,
            dacl: *mut Handle,
            defaulted: *mut i32,
        ) -> i32;
        fn GetAclInformation(acl: Handle, information: Handle, size: u32, class: u32) -> i32;
        fn GetAce(acl: Handle, index: u32, ace: *mut Handle) -> i32;
    }
    struct Memory(Handle);
    impl Drop for Memory {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }
    pub(super) fn folder(id: i32) -> Result<PathBuf> {
        let mut path = [0u16; 260];
        ensure!(
            unsafe { SHGetFolderPathW(ptr::null_mut(), id, ptr::null_mut(), 0, path.as_mut_ptr()) }
                >= 0,
            "system folder lookup failed"
        );
        let end = path
            .iter()
            .position(|v| *v == 0)
            .context("invalid system folder")?;
        let result = PathBuf::from(String::from_utf16(&path[..end])?);
        ensure!(
            result.is_absolute() && result.is_dir(),
            "system folder is unavailable"
        );
        crate::reject_reparse(&result)?;
        Ok(result)
    }
    pub(super) fn is_elevated() -> Result<bool> {
        let mut token = ptr::null_mut();
        ensure!(
            unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) } != 0,
            "cannot inspect process elevation"
        );
        let mut elevated = 0u32;
        let mut size = 0;
        let ok = unsafe {
            GetTokenInformation(token, 20, (&mut elevated as *mut u32).cast(), 4, &mut size)
        };
        unsafe {
            CloseHandle(token);
        }
        ensure!(ok != 0 && size == 4, "cannot read process elevation");
        Ok(elevated != 0)
    }

    // New files use the token's default owner. A personal SID would retain
    // WRITE_DAC from a later non-elevated process despite the protected DACL.
    pub(super) fn require_machine_owner() -> Result<()> {
        let mut token = ptr::null_mut();
        ensure!(
            unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) } != 0,
            "cannot inspect machine file owner"
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
        unsafe {
            GetTokenInformation(token.0, 4, ptr::null_mut(), 0, &mut size);
        }
        ensure!(
            size > 0 && size <= 65536,
            "invalid machine file owner token"
        );
        let mut data = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        ensure!(
            unsafe { GetTokenInformation(token.0, 4, data.as_mut_ptr().cast(), size, &mut size) }
                != 0,
            "cannot read machine file owner"
        );
        let owner = unsafe { *data.as_ptr().cast::<Handle>() };
        ensure!(
            trusted(&sid_text(owner)?, false),
            "elevated token must create machine files owned by Administrators or SYSTEM"
        );
        Ok(())
    }
    fn open(path: &Path, writing_security: bool) -> Result<File> {
        crate::reject_reparse(path)?;
        let access = 0x0002_0000 | 0x80 | 1 | if writing_security { 0x000c_0000 } else { 0 };
        let handle = unsafe {
            CreateFileW(
                wide(path.to_str().context("security path is not Unicode")?).as_ptr(),
                access,
                1,
                ptr::null_mut(),
                3,
                0x0220_0000,
                ptr::null_mut(),
            )
        };
        ensure!(
            handle != -1isize as Handle,
            "cannot inspect protected directory {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        Ok(unsafe { File::from_raw_handle(handle) })
    }
    fn sid_text(sid: Handle) -> Result<String> {
        ensure!(!sid.is_null(), "security principal missing");
        let mut text = ptr::null_mut();
        ensure!(
            unsafe { ConvertSidToStringSidW(sid, &mut text) } != 0,
            "invalid security principal"
        );
        let memory = Memory(text.cast());
        let mut len = 0;
        while len < 256 && unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        ensure!(len < 256, "security principal is too long");
        let value = String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) })?;
        drop(memory);
        Ok(value)
    }
    fn trusted(sid: &str, system_anchor: bool) -> bool {
        matches!(sid, "S-1-5-18" | "S-1-5-32-544")
            || (system_anchor
                && sid == "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464")
    }
    pub(super) fn validate_security(path: &Path, system_anchor: bool) -> Result<()> {
        let file = open(path, false)?;
        let (mut owner, mut dacl, mut descriptor) =
            (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
        ensure!(
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
            } == 0,
            "cannot inspect machine directory security"
        );
        let _memory = Memory(descriptor);
        validate_acl(path, owner, dacl, system_anchor)
    }

    fn validate_acl(path: &Path, owner: Handle, dacl: Handle, system_anchor: bool) -> Result<()> {
        ensure!(
            trusted(&sid_text(owner)?, system_anchor),
            "machine directory has a non-system owner: {}",
            path.display()
        );
        ensure!(
            !dacl.is_null(),
            "machine directory has an unrestricted DACL"
        );
        let mut size = AclSize {
            count: 0,
            used: 0,
            free: 0,
        };
        ensure!(
            unsafe {
                GetAclInformation(
                    dacl,
                    (&mut size as *mut AclSize).cast(),
                    std::mem::size_of::<AclSize>() as u32,
                    2,
                )
            } != 0
                && size.count <= 1024,
            "invalid machine directory DACL"
        );
        for index in 0..size.count {
            let mut ace = ptr::null_mut();
            ensure!(
                unsafe { GetAce(dacl, index, &mut ace) } != 0,
                "cannot read machine permission entry"
            );
            let header = unsafe { &*ace.cast::<AceHeader>() };
            // An inheritance-only write grant is still unsafe on machine task
            // storage: it would make newly written evidence user-writable.
            // The system anchor is different: every product root is created
            // with a protected DACL before it receives any child files.
            if header.kind == 1 || (system_anchor && header.flags & 8 != 0) {
                continue;
            }
            ensure!(
                header.kind == 0 && header.size >= 12,
                "unsupported machine directory permission entry"
            );
            let mask = unsafe { *ace.cast::<u8>().add(4).cast::<u32>() };
            if mask & (0x5000_0000 | 0x000d_0156) != 0 {
                let principal = sid_text(unsafe { ace.cast::<u8>().add(8).cast() })?;
                ensure!(
                    trusted(&principal, system_anchor),
                    "machine directory permits non-administrator writes: {}",
                    path.display()
                );
            }
        }
        Ok(())
    }
    fn descriptor(readable: bool) -> Result<Memory> {
        let sddl = if readable {
            "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)"
        } else {
            "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
        };
        parse_descriptor(sddl)
    }

    fn parse_descriptor(sddl: &str) -> Result<Memory> {
        let mut raw = ptr::null_mut();
        ensure!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide(sddl).as_ptr(),
                    1,
                    &mut raw,
                    ptr::null_mut(),
                )
            } != 0,
            "cannot construct machine storage security"
        );
        Ok(Memory(raw))
    }
    pub(super) fn create_protected_directory(path: &Path) -> Result<()> {
        let descriptor = descriptor(false)?;
        let attributes = Attributes {
            length: std::mem::size_of::<Attributes>() as u32,
            descriptor: descriptor.0,
            inherit: 0,
        };
        if unsafe {
            CreateDirectoryW(
                wide(path.to_str().context("machine path is not Unicode")?).as_ptr(),
                &attributes,
            )
        } == 0
        {
            let error = std::io::Error::last_os_error();
            ensure!(
                error.raw_os_error() == Some(183),
                "cannot create machine storage: {error}"
            );
            ensure!(path.is_dir(), "machine storage path is not a directory");
        }
        Ok(())
    }
    pub(super) fn set_protection(path: &Path, readable: bool) -> Result<()> {
        let file = open(path, true)?;
        let descriptor = descriptor(readable)?;
        let (mut owner, mut dacl) = (ptr::null_mut(), ptr::null_mut());
        let (mut defaulted, mut present) = (0, 0);
        ensure!(
            unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted) } != 0,
            "machine owner missing"
        );
        ensure!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
            } != 0
                && present != 0,
            "machine DACL missing"
        );
        ensure!(
            unsafe {
                SetSecurityInfo(
                    file.as_raw_handle(),
                    1,
                    0x8000_0005,
                    owner,
                    ptr::null_mut(),
                    dacl,
                    ptr::null_mut(),
                )
            } == 0,
            "cannot protect new machine directory"
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        fn validate_sddl(sddl: &str, anchor: bool) -> Result<()> {
            let descriptor = parse_descriptor(sddl)?;
            let (mut owner, mut dacl) = (ptr::null_mut(), ptr::null_mut());
            let (mut present, mut defaulted) = (0, 0);
            ensure!(
                unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted) }
                    != 0,
                "owner unavailable"
            );
            ensure!(
                unsafe {
                    GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
                } != 0
                    && present != 0,
                "DACL unavailable"
            );
            validate_acl(Path::new("test-security-descriptor"), owner, dacl, anchor)
        }
        #[test]
        fn inherited_write_permissions_are_rejected_for_machine_evidence() {
            let inherited = "O:BAG:BAD:P(A;OICI;FA;;;BA)(A;OICIIO;FA;;;BU)";
            assert!(validate_sddl(inherited, false).is_err());
            // The system anchor's inherited ACL is replaced on the new root.
            validate_sddl(inherited, true).unwrap();
            assert!(validate_sddl("O:BAG:BAD:P(A;OICI;FA;;;BA)(A;OICI;FW;;;BU)", false).is_err());
            validate_sddl(
                "O:BAG:BAD:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICI;0x1200a9;;;BU)",
                false,
            )
            .unwrap();
        }
        #[test]
        fn strong_dacl_does_not_make_a_personal_owner_safe() {
            assert!(
                validate_sddl(
                    "O:S-1-5-21-100-100-100-1001D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)",
                    false
                )
                .is_err()
            );
            validate_sddl("O:SYD:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)", false).unwrap();
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub(super) fn is_elevated() -> Result<bool> {
        Ok(false)
    }
    pub(super) fn require_machine_owner() -> Result<()> {
        anyhow::bail!("Windows security is required")
    }
    pub(super) fn folder(_: i32) -> Result<PathBuf> {
        anyhow::bail!("Windows system folders are required")
    }
    pub(super) fn validate_security(_: &Path, _: bool) -> Result<()> {
        anyhow::bail!("Windows security is required")
    }
    pub(super) fn create_protected_directory(_: &Path) -> Result<()> {
        anyhow::bail!("Windows security is required")
    }
    pub(super) fn set_protection(_: &Path, _: bool) -> Result<()> {
        anyhow::bail!("Windows security is required")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scope_is_bound_to_manifest_receipt_and_plan_without_changing_legacy_json() {
        let manifest: crate::Manifest = serde_json::from_str(include_str!(
            "../../../docs/schemas/examples/components-manifest.json"
        ))
        .unwrap();
        let receipt: crate::Receipt = serde_json::from_str(include_str!(
            "../../../docs/schemas/examples/components-receipt.json"
        ))
        .unwrap();
        assert!(
            serde_json::to_value(&manifest)
                .unwrap()
                .get("install_scope")
                .is_none()
        );
        assert!(
            serde_json::to_value(&receipt)
                .unwrap()
                .get("install_scope")
                .is_none()
        );
        let mut machine = manifest.clone();
        machine.install_scope = InstallScope::AllUsers;
        machine.validate().unwrap();
        assert!(receipt.validate_manifest(&machine).is_err());
        let mut plan = crate::InstallPlan::new(
            &manifest,
            crate::Operation::Install,
            "scope-test-task".into(),
            PathBuf::from(r"C:\AvelinScopeFixture"),
            "S-1-5-21-1".into(),
        )
        .unwrap();
        assert!(
            serde_json::to_value(&plan)
                .unwrap()
                .get("install_scope")
                .is_none()
        );
        plan.install_scope = InstallScope::AllUsers;
        assert!(plan.validate(&manifest).is_err());
        // Flipping just the scope never overrides the manifest hash binding.
        assert!(plan.validate(&machine).is_err());
    }

    #[test]
    fn only_machine_maintenance_may_change_the_recorded_administrator() {
        let mut manifest: crate::Manifest = serde_json::from_str(include_str!(
            "../../../docs/schemas/examples/components-manifest.json"
        ))
        .unwrap();
        let mut receipt: crate::Receipt = serde_json::from_str(include_str!(
            "../../../docs/schemas/examples/components-receipt.json"
        ))
        .unwrap();
        let mut plan: crate::InstallPlan = serde_json::from_str(include_str!(
            "../../../docs/schemas/examples/components-modify-plan.json"
        ))
        .unwrap();
        let digest = crate::hash_bytes(&serde_json::to_vec(&receipt).unwrap());
        plan.upgrade.as_mut().unwrap().previous_receipt_sha256 = digest.clone();
        plan.validate_modify(&manifest, &receipt, &digest).unwrap();
        plan.user_sid = "S-1-5-21-900-900-900-1002".into();
        assert!(plan.validate_modify(&manifest, &receipt, &digest).is_err());
        manifest.install_scope = InstallScope::AllUsers;
        receipt.install_scope = InstallScope::AllUsers;
        plan.install_scope = InstallScope::AllUsers;
        let manifest_hash = crate::hash_bytes(&serde_json::to_vec(&manifest).unwrap());
        receipt.manifest_sha256 = manifest_hash.clone();
        plan.manifest_sha256 = manifest_hash.clone();
        plan.upgrade.as_mut().unwrap().previous_manifest_sha256 = manifest_hash;
        let digest = crate::hash_bytes(&serde_json::to_vec(&receipt).unwrap());
        plan.upgrade.as_mut().unwrap().previous_receipt_sha256 = digest.clone();
        // Structural permission only; the worker separately requires elevation,
        // the Program Files boundary and protected machine evidence.
        plan.validate_modify(&manifest, &receipt, &digest).unwrap();
        receipt.install_scope = InstallScope::CurrentUser;
        assert!(plan.validate_modify(&manifest, &receipt, &digest).is_err());
    }

    #[test]
    fn current_user_ownership_stays_strict() {
        assert!(
            validate_owned_installation(
                InstallScope::CurrentUser,
                Path::new("unused"),
                "S-1-5-21-1",
                "S-1-5-21-2"
            )
            .is_err()
        );
        assert!(
            validate_owned_installation(
                InstallScope::CurrentUser,
                Path::new("unused"),
                "S-1-5-21-1",
                "S-1-5-21-1"
            )
            .is_ok()
        );
    }
    #[cfg(windows)]
    #[test]
    fn machine_paths_are_independent_of_actor_and_reject_temporary_storage() {
        let a = product_lock_path(InstallScope::AllUsers, "org.avelin.test", "S-1-5-21-1").unwrap();
        let b = product_lock_path(InstallScope::AllUsers, "org.avelin.test", "S-1-5-21-2").unwrap();
        assert_eq!(a, b);
        assert_ne!(
            product_lock_path(InstallScope::CurrentUser, "org.avelin.test", "S-1-5-21-1").unwrap(),
            product_lock_path(InstallScope::CurrentUser, "org.avelin.test", "S-1-5-21-2").unwrap()
        );
        assert!(
            validate_target(
                InstallScope::AllUsers,
                &std::env::temp_dir().join("avelin-scope-test")
            )
            .is_err()
        );
        assert!(validate_session(InstallScope::AllUsers, &std::env::temp_dir()).is_err());
        let target = default_install_root(InstallScope::AllUsers, "AvelinScopeTest").unwrap();
        assert!(validate_target(InstallScope::AllUsers, &target.join("nested")).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn machine_mutation_requires_an_elevated_token() {
        if !is_elevated().unwrap() {
            assert!(require_elevation(InstallScope::AllUsers).is_err());
            assert!(ensure_machine_state().is_err());
        }
        require_elevation(InstallScope::CurrentUser).unwrap();
    }
}
