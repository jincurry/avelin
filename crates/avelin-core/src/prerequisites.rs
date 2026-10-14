//! Read-only checks of Microsoft-installed Windows runtime prerequisites.
//!
//! No executable is started, no registry value is written, and no network
//! connection is made. Portable .NET and app-local runtime deployments should
//! omit the corresponding machine prerequisite from their package.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const VC_KEY: &str = r"SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64";
const DOTNET_KEY: &str =
    r"SOFTWARE\dotnet\Setup\InstalledVersions\x64\sharedfx\Microsoft.WindowsDesktop.App";
const WEBVIEW_KEY: &str =
    r"SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

/// These are requirements of the packaged application, not of Avelin itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Requirement {
    VcRedistX64 { min_version: String },
    DotnetDesktopX64 { min_version: String },
    Webview2Evergreen { min_version: String },
}

impl Requirement {
    pub fn min_version(&self) -> &str {
        match self {
            Self::VcRedistX64 { min_version }
            | Self::DotnetDesktopX64 { min_version }
            | Self::Webview2Evergreen { min_version } => min_version,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::VcRedistX64 { .. } => "vc_redist_x64",
            Self::DotnetDesktopX64 { .. } => "dotnet_desktop_x64",
            Self::Webview2Evergreen { .. } => "webview2_evergreen",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::VcRedistX64 { .. } => "Microsoft Visual C++ 2015–2022 Redistributable (x64)",
            Self::DotnetDesktopX64 { .. } => "Microsoft .NET Desktop Runtime (x64)",
            Self::Webview2Evergreen { .. } => "Microsoft Edge WebView2 Evergreen Runtime",
        }
    }

    pub fn download_url(&self) -> String {
        match self {
            Self::VcRedistX64 { .. } => {
                "https://learn.microsoft.com/cpp/windows/latest-supported-vc-redist".into()
            }
            Self::DotnetDesktopX64 { .. } => {
                // Validation rejects anything except a stable numeric version.
                let parts: Vec<_> = self.min_version().split('.').collect();
                if parts.len() == 3 && self.validate().is_ok() {
                    format!(
                        "https://dotnet.microsoft.com/download/dotnet/{}.{}",
                        parts[0], parts[1]
                    )
                } else {
                    "https://dotnet.microsoft.com/download/dotnet".into()
                }
            }
            Self::Webview2Evergreen { .. } => {
                "https://developer.microsoft.com/microsoft-edge/webview2/".into()
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        let count = if matches!(self, Self::DotnetDesktopX64 { .. }) {
            3
        } else {
            4
        };
        let version = NumericVersion::parse(self.min_version(), count)?;
        match self {
            Self::VcRedistX64 { .. } => ensure!(
                version.0[0] == 14,
                "VC++ prerequisite must use version 14.x.x.x"
            ),
            Self::DotnetDesktopX64 { .. } => ensure!(
                version.0[0] >= 3,
                ".NET Desktop prerequisite must use a stable version 3.0.0 or newer"
            ),
            Self::Webview2Evergreen { .. } => ensure!(
                version.0 != [0; 4],
                "WebView2 prerequisite version must be greater than zero"
            ),
        }
        Ok(())
    }
}

/// Reject duplicate kinds so the package has one unambiguous policy per runtime.
pub fn validate(requirements: &[Requirement]) -> Result<()> {
    ensure!(
        requirements.len() <= 3,
        "at most three runtime prerequisites are supported"
    );
    let mut kinds = BTreeSet::new();
    for requirement in requirements {
        requirement
            .validate()
            .with_context(|| format!("invalid {} prerequisite", requirement.kind()))?;
        ensure!(
            kinds.insert(requirement.kind()),
            "duplicate {} prerequisite",
            requirement.kind()
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteStatus {
    Installed,
    Missing,
    CheckFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrerequisiteItem {
    pub requirement: Requirement,
    pub status: PrerequisiteStatus,
    pub detected_versions: Vec<String>,
    pub download_url: String,
    pub detail_zh: String,
    pub detail_en: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrerequisiteReport {
    pub items: Vec<PrerequisiteItem>,
}

impl PrerequisiteReport {
    pub fn is_satisfied(&self) -> bool {
        self.items
            .iter()
            .all(|item| item.status == PrerequisiteStatus::Installed)
    }

    /// Ready-to-display blocking details; callers supply their own retry button.
    pub fn blocking_detail(&self, english: bool) -> String {
        self.items
            .iter()
            .filter(|item| item.status != PrerequisiteStatus::Installed)
            .map(|item| {
                if english {
                    item.detail_en.clone()
                } else {
                    item.detail_zh.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Check machine registrations and the current user's WebView2 registration.
pub fn inspect(requirements: &[Requirement]) -> Result<PrerequisiteReport> {
    inspect_with(requirements, &SystemRegistry, false)
}

/// An all-users installation cannot rely on one user's WebView2 registration.
pub fn inspect_for_machine(requirements: &[Requirement]) -> Result<PrerequisiteReport> {
    inspect_with(requirements, &SystemRegistry, true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Hive {
    Machine,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum View {
    Registry32,
    Registry64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    String(String),
    Dword(u32),
    Other,
}

trait Registry {
    fn values(&self, hive: Hive, view: View, path: &str)
    -> Result<Option<BTreeMap<String, Value>>>;
}

struct SystemRegistry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct NumericVersion([u32; 4]);

impl NumericVersion {
    fn parse(text: &str, count: usize) -> Result<Self> {
        Self::parse_impl(text, count, true)
    }

    fn parse_registered(text: &str, count: usize) -> Result<Self> {
        Self::parse_impl(text, count, false)
    }

    fn parse_impl(text: &str, count: usize, canonical: bool) -> Result<Self> {
        ensure!(text.len() <= 43, "version is too long");
        let parts = text.split('.').collect::<Vec<_>>();
        ensure!(
            parts.len() == count,
            "version must have {count} numeric parts"
        );
        let mut result = [0; 4];
        for (index, part) in parts.iter().enumerate() {
            ensure!(
                !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()),
                "version must contain only decimal numbers"
            );
            ensure!(
                !canonical || part.len() == 1 || !part.starts_with('0'),
                "version must not contain leading zeroes"
            );
            result[index] = part.parse().context("version part is out of range")?;
        }
        Ok(Self(result))
    }

    fn display(self, count: usize) -> String {
        self.0[..count]
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".")
    }
}

#[derive(Default)]
struct Detection {
    versions: BTreeSet<NumericVersion>,
    errors: Vec<String>,
}

impl Detection {
    fn read(
        &mut self,
        registry: &impl Registry,
        hive: Hive,
        view: View,
        key: &str,
    ) -> Option<BTreeMap<String, Value>> {
        match registry.values(hive, view, key) {
            Ok(values) => values,
            Err(error) => {
                self.errors.push(format!("{hive:?}/{view:?}: {error:#}"));
                None
            }
        }
    }
}

fn inspect_with(
    requirements: &[Requirement],
    registry: &impl Registry,
    machine_only: bool,
) -> Result<PrerequisiteReport> {
    validate(requirements)?;
    let mut report = PrerequisiteReport::default();
    for requirement in requirements {
        let detection = match requirement {
            Requirement::VcRedistX64 { .. } => detect_vc(registry),
            Requirement::DotnetDesktopX64 { .. } => detect_dotnet(registry),
            Requirement::Webview2Evergreen { .. } => detect_webview(registry, machine_only),
        };
        let count = if matches!(requirement, Requirement::DotnetDesktopX64 { .. }) {
            3
        } else {
            4
        };
        let minimum = NumericVersion::parse(requirement.min_version(), count)?;
        let satisfied = detection.versions.iter().any(|version| {
            *version >= minimum
                && match requirement {
                    Requirement::DotnetDesktopX64 { .. } => version.0[..2] == minimum.0[..2],
                    Requirement::VcRedistX64 { .. } => version.0[0] == 14,
                    Requirement::Webview2Evergreen { .. } => true,
                }
        });
        let status = if satisfied {
            PrerequisiteStatus::Installed
        } else if detection.errors.is_empty() {
            PrerequisiteStatus::Missing
        } else {
            PrerequisiteStatus::CheckFailed
        };
        let detected_versions = detection
            .versions
            .iter()
            .rev()
            .map(|v| v.display(count))
            .collect::<Vec<_>>();
        let detected = if detected_versions.is_empty() {
            "—".into()
        } else {
            let mut shown = detected_versions
                .iter()
                .take(12)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            if detected_versions.len() > 12 {
                shown.push_str(&format!(", … (+{})", detected_versions.len() - 12));
            }
            shown
        };
        let name = requirement.name();
        let min = requirement.min_version();
        let download_url = requirement.download_url();
        let compatible_zh = if count == 3 {
            "（仅接受相同主版本和次版本的稳定补丁版本）"
        } else {
            ""
        };
        let compatible_en = if count == 3 {
            " (stable patch within the same major.minor only)"
        } else {
            ""
        };
        let (state_zh, state_en) = match status {
            PrerequisiteStatus::Installed => ("已满足", "Satisfied"),
            PrerequisiteStatus::Missing => (
                "缺少所需运行库或版本过低",
                "Required runtime is missing or too old",
            ),
            PrerequisiteStatus::CheckFailed => {
                ("无法确认运行库状态", "Could not verify runtime status")
            }
        };
        let error = detection
            .errors
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let error_zh = if error.is_empty() {
            String::new()
        } else {
            format!("\n检测信息：{error}")
        };
        let error_en = if error.is_empty() {
            String::new()
        } else {
            format!("\nCheck details: {error}")
        };
        let action_zh = if satisfied {
            String::new()
        } else {
            format!("\n请通过微软官方网站安装或修复运行库，然后重新检查：{download_url}")
        };
        let action_en = if satisfied {
            String::new()
        } else {
            format!(
                "\nInstall or repair the runtime from Microsoft's official site, then check again: {download_url}"
            )
        };
        report.items.push(PrerequisiteItem {
            requirement: requirement.clone(), status, detected_versions, download_url,
            detail_zh: format!("{name}：{state_zh}。要求 ≥ {min}{compatible_zh}；检测到：{detected}。{error_zh}{action_zh}"),
            detail_en: format!("{name}: {state_en}. Required ≥ {min}{compatible_en}; detected: {detected}.{error_en}{action_en}"),
        });
    }
    Ok(report)
}

fn detect_vc(registry: &impl Registry) -> Detection {
    let mut detection = Detection::default();
    // Both documented registry views are considered, but the architecture is
    // always the explicit x64 subkey, never an x86 redistributable registration.
    for view in [View::Registry64, View::Registry32] {
        let Some(values) = detection.read(registry, Hive::Machine, view, VC_KEY) else {
            continue;
        };
        match values.get("Installed") {
            Some(Value::Dword(0)) => continue,
            Some(Value::Dword(1)) => {}
            _ => {
                detection
                    .errors
                    .push("VC++ Installed must be DWORD 0 or 1".into());
                continue;
            }
        }
        match vc_registered_version(&values) {
            Ok(version) => {
                detection.versions.insert(version);
            }
            Err(error) => detection.errors.push(format!("VC++ version: {error:#}")),
        }
    }
    detection
}

fn vc_registered_version(values: &BTreeMap<String, Value>) -> Result<NumericVersion> {
    // Microsoft also publishes each segment as a DWORD. In particular, actual
    // Version strings can end in ".00", which is normalized numerically.
    let names = ["Major", "Minor", "Bld", "Rbld"];
    let dword = if names.iter().all(|name| values.contains_key(*name)) {
        let mut parts = [0; 4];
        for (index, name) in names.iter().enumerate() {
            let Some(Value::Dword(value)) = values.get(*name) else {
                anyhow::bail!("{name} must be DWORD");
            };
            parts[index] = *value;
        }
        Some(NumericVersion(parts))
    } else {
        None
    };
    let version = match values.get("Version") {
        Some(Value::String(text)) => {
            let text = text.strip_prefix('v').unwrap_or(text);
            if let Ok(version) = NumericVersion::parse_registered(text, 4) {
                if let Some(dword) = dword {
                    ensure!(
                        version == dword,
                        "Version and numeric registry values disagree"
                    );
                }
                version
            } else {
                // A shortened string is usable only when the complete published
                // DWORD values supply the actual fourth part; never assume zero.
                let prefix = NumericVersion::parse_registered(text, 3)?;
                let full = dword.context("three-part Version requires Major/Minor/Bld/Rbld")?;
                ensure!(
                    prefix.0[..3] == full.0[..3],
                    "Version and numeric registry values disagree"
                );
                full
            }
        }
        None => dword.context("Version and complete numeric registry values are missing")?,
        _ => anyhow::bail!("Version is not REG_SZ"),
    };
    ensure!(version.0[0] == 14, "expected a 14.x.x.x version");
    Ok(version)
}

fn detect_dotnet(registry: &impl Registry) -> Detection {
    let mut detection = Detection::default();
    // Microsoft.DotNet.Build.Tasks.Installers authors this key as always32.
    let Some(values) = detection.read(registry, Hive::Machine, View::Registry32, DOTNET_KEY) else {
        return detection;
    };
    for (name, value) in values {
        if value == Value::Dword(0) {
            continue;
        }
        // Preview registrations never satisfy a stable runtime requirement.
        if semver::Version::parse(&name)
            .is_ok_and(|version| !version.pre.is_empty() || !version.build.is_empty())
        {
            continue;
        }
        match (NumericVersion::parse(&name, 3), value) {
            (Ok(version), Value::Dword(1)) => {
                detection.versions.insert(version);
            }
            _ => detection
                .errors
                .push(format!("Invalid .NET Desktop registration: {name}")),
        }
    }
    detection
}

fn detect_webview(registry: &impl Registry, machine_only: bool) -> Detection {
    let mut detection = Detection::default();
    let mut locations = vec![(Hive::Machine, View::Registry32)];
    if !machine_only {
        locations.push((Hive::User, View::Registry64));
    }
    for (hive, view) in locations {
        let Some(values) = detection.read(registry, hive, view, WEBVIEW_KEY) else {
            continue;
        };
        match values.get("pv") {
            None => {}
            Some(Value::String(value)) if value.is_empty() || value == "0.0.0.0" => {}
            Some(Value::String(value)) => match NumericVersion::parse(value, 4) {
                Ok(version) => {
                    detection.versions.insert(version);
                }
                Err(_) => detection
                    .errors
                    .push("WebView2 pv is not a valid four-part version".into()),
            },
            _ => detection.errors.push("WebView2 pv is not REG_SZ".into()),
        }
    }
    detection
}

#[cfg(not(windows))]
impl Registry for SystemRegistry {
    fn values(&self, _: Hive, _: View, _: &str) -> Result<Option<BTreeMap<String, Value>>> {
        anyhow::bail!("Windows runtime detection requires Windows")
    }
}

#[cfg(windows)]
impl Registry for SystemRegistry {
    fn values(
        &self,
        hive: Hive,
        view: View,
        path: &str,
    ) -> Result<Option<BTreeMap<String, Value>>> {
        windows::values(hive, view, path)
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{ffi::c_void, ptr};
    type Hkey = *mut c_void;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(
            key: Hkey,
            subkey: *const u16,
            options: u32,
            access: u32,
            result: *mut Hkey,
        ) -> i32;
        fn RegEnumValueW(
            key: Hkey,
            index: u32,
            name: *mut u16,
            name_len: *mut u32,
            reserved: *mut u32,
            kind: *mut u32,
            data: *mut u8,
            data_len: *mut u32,
        ) -> i32;
        fn RegCloseKey(key: Hkey) -> i32;
    }
    struct Key(Hkey);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    pub(super) fn values(
        hive: Hive,
        view: View,
        path: &str,
    ) -> Result<Option<BTreeMap<String, Value>>> {
        let hive = match hive {
            Hive::Machine => 0x80000002u32,
            Hive::User => 0x80000001u32,
        } as i32 as isize as Hkey;
        let access = 0x0001
            | match view {
                View::Registry32 => 0x0200,
                View::Registry64 => 0x0100,
            };
        let path = path.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let mut raw = ptr::null_mut();
        let code = unsafe { RegOpenKeyExW(hive, path.as_ptr(), 0, access, &mut raw) };
        if code == 2 || code == 3 {
            return Ok(None);
        }
        ensure!(code == 0, "RegOpenKeyExW failed ({code})");
        let key = Key(raw);
        let mut values = BTreeMap::new();
        for index in 0..=2048 {
            let mut name = [0u16; 256];
            let mut name_len = name.len() as u32;
            let mut data = [0u8; 4096];
            let mut data_len = data.len() as u32;
            let mut kind = 0;
            let code = unsafe {
                RegEnumValueW(
                    key.0,
                    index,
                    name.as_mut_ptr(),
                    &mut name_len,
                    ptr::null_mut(),
                    &mut kind,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if code == 259 {
                return Ok(Some(values));
            }
            ensure!(index < 2048, "runtime registry exceeds 2048 values");
            ensure!(
                code == 0,
                "RegEnumValueW failed ({code}); runtime registry exceeds safe bounds or is unreadable"
            );
            ensure!(
                (name_len as usize) < name.len() && (data_len as usize) <= data.len(),
                "invalid runtime registry value length"
            );
            let name = String::from_utf16(&name[..name_len as usize])
                .context("runtime registry name is not UTF-16")?;
            let bytes = &data[..data_len as usize];
            let value = match kind {
                4 if bytes.len() == 4 => {
                    Value::Dword(u32::from_le_bytes(bytes.try_into().unwrap()))
                }
                1 if bytes.is_empty() => Value::String(String::new()),
                1 => {
                    ensure!(
                        bytes.len() >= 2 && bytes.len().is_multiple_of(2),
                        "runtime REG_SZ has invalid length"
                    );
                    let mut text = bytes
                        .chunks_exact(2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .collect::<Vec<_>>();
                    ensure!(
                        text.pop() == Some(0) && !text.contains(&0),
                        "runtime REG_SZ is not a terminated string"
                    );
                    Value::String(
                        String::from_utf16(&text).context("runtime REG_SZ is not UTF-16")?,
                    )
                }
                _ => Value::Other,
            };
            ensure!(
                values.insert(name, value).is_none(),
                "runtime registry changed during inspection"
            );
        }
        anyhow::bail!("runtime registry exceeds 2048 values")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct MockRegistry {
        keys: BTreeMap<(Hive, View, String), BTreeMap<String, Value>>,
        fail: bool,
    }
    impl Registry for MockRegistry {
        fn values(
            &self,
            hive: Hive,
            view: View,
            path: &str,
        ) -> Result<Option<BTreeMap<String, Value>>> {
            ensure!(!self.fail, "access denied");
            Ok(self.keys.get(&(hive, view, path.into())).cloned())
        }
    }
    impl MockRegistry {
        fn set(&mut self, hive: Hive, view: View, key: &str, name: &str, value: Value) {
            self.keys
                .entry((hive, view, key.into()))
                .or_default()
                .insert(name.into(), value);
        }
    }
    fn vc() -> Requirement {
        Requirement::VcRedistX64 {
            min_version: "14.40.33810.0".into(),
        }
    }
    fn dotnet() -> Requirement {
        Requirement::DotnetDesktopX64 {
            min_version: "8.0.5".into(),
        }
    }
    fn webview() -> Requirement {
        Requirement::Webview2Evergreen {
            min_version: "120.0.0.0".into(),
        }
    }
    fn status(requirement: Requirement, registry: &MockRegistry) -> PrerequisiteStatus {
        inspect_with(&[requirement], registry, false).unwrap().items[0].status
    }

    #[test]
    fn strict_config_rejects_unknown_fields_and_kinds() {
        for json in [
            r#"{"kind":"dotnet_desktop_x64","min_version":"8.0.0","path":"evil.exe"}"#,
            r#"{"kind":"arbitrary_command","min_version":"8.0.0"}"#,
        ] {
            assert!(serde_json::from_str::<Requirement>(json).is_err());
        }
        let json = r#"{"kind":"vc_redist_x64","min_version":"14.0.0.0"}"#;
        assert!(
            serde_json::from_str::<Requirement>(json)
                .unwrap()
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn requirements_validate_versions_and_duplicates() {
        for min in [
            "8.0",
            "8.0.0-preview.1",
            "08.0.0",
            "8.0.0+local",
            "8.0.4294967296",
            "8.0.0\n",
            "2.0.0",
        ] {
            assert!(
                Requirement::DotnetDesktopX64 {
                    min_version: min.into()
                }
                .validate()
                .is_err(),
                "{min}"
            );
        }
        assert!(
            Requirement::VcRedistX64 {
                min_version: "15.0.0.0".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            Requirement::Webview2Evergreen {
                min_version: "0.0.0.0".into()
            }
            .validate()
            .is_err()
        );
        assert!(validate(&[vc(), vc()]).is_err());
        assert!(validate(&[vc(), dotnet(), webview()]).is_ok());
    }

    #[test]
    fn empty_requirements_do_not_query_the_machine() {
        assert!(
            inspect_with(
                &[],
                &MockRegistry {
                    fail: true,
                    ..Default::default()
                },
                false
            )
            .unwrap()
            .is_satisfied()
        );
    }

    #[test]
    fn missing_and_check_failure_are_distinct() {
        let missing = inspect_with(
            &[vc(), dotnet(), webview()],
            &MockRegistry::default(),
            false,
        )
        .unwrap();
        assert!(
            missing
                .items
                .iter()
                .all(|i| i.status == PrerequisiteStatus::Missing)
        );
        let failed = inspect_with(
            &[vc(), dotnet(), webview()],
            &MockRegistry {
                fail: true,
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert!(
            failed
                .items
                .iter()
                .all(|i| i.status == PrerequisiteStatus::CheckFailed)
        );
        assert!(!failed.is_satisfied());
        assert!(failed.blocking_detail(true).contains("access denied"));
    }

    #[test]
    fn vc_requires_install_flag_and_numeric_compatible_version() {
        let mut registry = MockRegistry::default();
        registry.set(
            Hive::Machine,
            View::Registry64,
            VC_KEY,
            "Version",
            Value::String("v14.44.35211.0".into()),
        );
        assert_eq!(status(vc(), &registry), PrerequisiteStatus::CheckFailed);
        registry.set(
            Hive::Machine,
            View::Registry64,
            VC_KEY,
            "Installed",
            Value::Dword(0),
        );
        assert_eq!(status(vc(), &registry), PrerequisiteStatus::Missing);
        registry.set(
            Hive::Machine,
            View::Registry64,
            VC_KEY,
            "Installed",
            Value::Dword(1),
        );
        assert_eq!(status(vc(), &registry), PrerequisiteStatus::Installed);
        registry.set(
            Hive::Machine,
            View::Registry64,
            VC_KEY,
            "Version",
            Value::String("v14.9.99999.0".into()),
        );
        assert_eq!(status(vc(), &registry), PrerequisiteStatus::Missing);
    }

    #[test]
    fn vc_accepts_x64_registration_in_32_bit_view() {
        let mut registry = MockRegistry::default();
        registry.set(
            Hive::Machine,
            View::Registry32,
            VC_KEY,
            "Version",
            Value::String("v14.40.33810.0".into()),
        );
        registry.set(
            Hive::Machine,
            View::Registry32,
            VC_KEY,
            "Installed",
            Value::Dword(1),
        );
        assert_eq!(status(vc(), &registry), PrerequisiteStatus::Installed);
    }

    #[test]
    fn vc_normalizes_real_registry_versions_and_uses_exact_rebuild_value() {
        let mut values = BTreeMap::from([
            ("Version".into(), Value::String("v14.44.35211.00".into())),
            ("Major".into(), Value::Dword(14)),
            ("Minor".into(), Value::Dword(44)),
            ("Bld".into(), Value::Dword(35211)),
            ("Rbld".into(), Value::Dword(0)),
        ]);
        assert_eq!(
            vc_registered_version(&values).unwrap().display(4),
            "14.44.35211.0"
        );
        values.insert("Version".into(), Value::String("v14.44.35211".into()));
        values.insert("Rbld".into(), Value::Dword(7));
        assert_eq!(
            vc_registered_version(&values).unwrap().display(4),
            "14.44.35211.7"
        );
        values.remove("Rbld");
        assert!(vc_registered_version(&values).is_err());
    }

    #[test]
    fn vc_rejects_disagreeing_string_and_numeric_version() {
        let values = BTreeMap::from([
            ("Version".into(), Value::String("v14.44.35211.00".into())),
            ("Major".into(), Value::Dword(14)),
            ("Minor".into(), Value::Dword(44)),
            ("Bld".into(), Value::Dword(35211)),
            ("Rbld".into(), Value::Dword(7)),
        ]);
        assert!(vc_registered_version(&values).is_err());
    }

    #[test]
    fn dotnet_requires_same_major_minor_and_stable_patch() {
        let mut registry = MockRegistry::default();
        for version in ["8.0.4", "8.0.30-preview.1", "8.1.0", "9.0.0"] {
            registry.set(
                Hive::Machine,
                View::Registry32,
                DOTNET_KEY,
                version,
                Value::Dword(1),
            );
        }
        assert_eq!(status(dotnet(), &registry), PrerequisiteStatus::Missing);
        registry.set(
            Hive::Machine,
            View::Registry32,
            DOTNET_KEY,
            "8.0.10",
            Value::Dword(1),
        );
        assert_eq!(status(dotnet(), &registry), PrerequisiteStatus::Installed);
    }

    #[test]
    fn dotnet_ignores_other_architecture_and_wrong_registration_type() {
        let mut registry = MockRegistry::default();
        registry.set(
            Hive::Machine,
            View::Registry32,
            &DOTNET_KEY.replace("x64", "x86"),
            "8.0.10",
            Value::Dword(1),
        );
        assert_eq!(status(dotnet(), &registry), PrerequisiteStatus::Missing);
        registry.set(
            Hive::Machine,
            View::Registry32,
            DOTNET_KEY,
            "8.0.10",
            Value::String("1".into()),
        );
        assert_eq!(status(dotnet(), &registry), PrerequisiteStatus::CheckFailed);
    }

    #[test]
    fn webview_machine_scope_does_not_accept_user_only_runtime() {
        let mut registry = MockRegistry::default();
        registry.set(
            Hive::User,
            View::Registry64,
            WEBVIEW_KEY,
            "pv",
            Value::String("140.0.3485.0".into()),
        );
        assert_eq!(status(webview(), &registry), PrerequisiteStatus::Installed);
        assert!(
            !inspect_with(&[webview()], &registry, true)
                .unwrap()
                .is_satisfied()
        );
        registry.set(
            Hive::Machine,
            View::Registry32,
            WEBVIEW_KEY,
            "pv",
            Value::String("140.0.3485.0".into()),
        );
        assert!(
            inspect_with(&[webview()], &registry, true)
                .unwrap()
                .is_satisfied()
        );
    }

    #[test]
    fn webview_zero_empty_missing_are_not_installed_and_invalid_data_fails() {
        let mut registry = MockRegistry::default();
        for version in ["", "0.0.0.0", "119.9.9999.999"] {
            registry.set(
                Hive::Machine,
                View::Registry32,
                WEBVIEW_KEY,
                "pv",
                Value::String(version.into()),
            );
            assert_eq!(status(webview(), &registry), PrerequisiteStatus::Missing);
        }
        registry.set(
            Hive::Machine,
            View::Registry32,
            WEBVIEW_KEY,
            "pv",
            Value::Other,
        );
        assert_eq!(
            status(webview(), &registry),
            PrerequisiteStatus::CheckFailed
        );
    }

    #[test]
    fn report_has_localized_action_and_official_links() {
        let report = inspect_with(
            &[vc(), dotnet(), webview()],
            &MockRegistry::default(),
            false,
        )
        .unwrap();
        assert!(report.blocking_detail(false).contains("重新检查"));
        assert!(report.blocking_detail(true).contains("check again"));
        assert_eq!(
            report.items[1].download_url,
            "https://dotnet.microsoft.com/download/dotnet/8.0"
        );
        let copy: PrerequisiteReport =
            serde_json::from_slice(&serde_json::to_vec(&report).unwrap()).unwrap();
        assert_eq!(copy, report);
    }

    #[test]
    #[cfg(windows)]
    fn local_machine_read_only_smoke_check() {
        let report = inspect(&[vc(), dotnet(), webview()]).unwrap();
        assert_eq!(report.items.len(), 3);
        for item in report.items {
            assert!(!item.detail_en.is_empty() && !item.detail_zh.is_empty());
            assert!(item.download_url.starts_with("https://"));
        }
    }
}
