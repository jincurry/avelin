//! Generate one validated configuration; never copy or execute application files.
use anyhow::{Context, Result, ensure};
use avelin_core::{SCHEMA_VERSION, install_root, reject_reparse, relative_path};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

pub struct Options {
    pub directory: PathBuf,
    pub app_id: String,
    pub name: String,
    pub app_version: String,
    pub entry: String,
    pub payload: PathBuf,
}

#[derive(Serialize)]
struct Config<'a> {
    schema_version: u32,
    app: App<'a>,
    payload: Payload<'a>,
    install: Install<'a>,
}
#[derive(Serialize)]
struct App<'a> {
    id: &'a str,
    name: &'a str,
    version: &'a str,
    entry: &'a str,
}
#[derive(Serialize)]
struct Payload<'a> {
    directory: &'a str,
}
#[derive(Serialize)]
struct Install<'a> {
    directory_name: &'a str,
    desktop_shortcut: bool,
    start_menu_shortcut: bool,
}

fn absolute_directory(path: &Path, label: &str) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty(),
        "{label} path must not be empty"
    );
    ensure!(
        !path
            .components()
            .any(|part| matches!(part, Component::ParentDir)),
        "{label} path must not contain '..'; use an explicit directory path"
    );
    let absolute =
        std::path::absolute(path).with_context(|| format!("resolve {label} directory"))?;
    install_root(&absolute).with_context(|| format!("invalid {label} directory; use a local directory below a drive root, without reparse points"))?;
    Ok(absolute)
}

/// Deny shared WRITE and DELETE on every ancestor, preventing both renames and
/// reparse conversion while the configuration is being created.
#[cfg(windows)]
fn pin_directories(path: &Path, create: bool) -> Result<Vec<File>> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    let mut ancestors: Vec<_> = path.ancestors().collect();
    ancestors.reverse();
    let mut pins = Vec::new();
    for directory in ancestors {
        if create && !directory.try_exists()? {
            match fs::create_dir(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("create configuration directory {}", directory.display())
                    });
                }
            }
        }
        let pin = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .custom_flags(0x02000000 | 0x00200000) // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
            .open(directory)
            .with_context(|| format!("open safe directory {}", directory.display()))?;
        let metadata = pin.metadata()?;
        ensure!(
            metadata.is_dir() && metadata.file_attributes() & 0x400 == 0,
            "reparse point or non-directory rejected: {}",
            directory.display()
        );
        pins.push(pin);
    }
    Ok(pins)
}

#[cfg(not(windows))]
fn pin_directories(_path: &Path, _create: bool) -> Result<Vec<File>> {
    anyhow::bail!("avelin-cli init currently requires Windows to protect directory creation")
}

fn create_config(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1).custom_flags(0x00200000); // No final reparse traversal.
    }
    let mut output = options.open(path).with_context(|| {
        format!(
            "create new {}; init never overwrites an existing file; choose another --directory",
            path.display()
        )
    })?;
    output
        .write_all(bytes)
        .context("write new configuration; inspect the retained file before retrying")?;
    output.sync_all()?;
    Ok(())
}

pub fn create(options: Options) -> Result<Value> {
    let directory = absolute_directory(&options.directory, "configuration")?;
    let payload = absolute_directory(&options.payload, "payload")?;
    let config_path = directory.join("avelin.toml");
    // Refuse before inventory work and before creating missing directories.
    reject_reparse(&config_path)?;
    ensure!(
        !config_path.try_exists()?,
        "configuration already exists: {}; init never overwrites it",
        config_path.display()
    );
    ensure!(
        payload.is_dir(),
        "payload directory does not exist: {}; --payload must point to existing application files, not a destination to create",
        payload.display()
    );
    let _payload_pins = pin_directories(&payload, false)?;
    let directory_text = directory
        .to_str()
        .context("configuration directory must be Unicode")?;
    let payload_text = payload
        .to_str()
        .context("payload directory must be Unicode")?;
    let config_text = config_path
        .to_str()
        .context("configuration path must be Unicode")?;
    let normalized_payload = payload_text.replace('/', "\\");
    let payload_prefix = format!("{}\\", normalized_payload.trim_end_matches('\\'));
    ensure!(
        !config_text
            .replace('/', "\\")
            .to_uppercase()
            .starts_with(&payload_prefix.to_uppercase()),
        "configuration must be outside the payload directory; place application files in a separate dist/payload subdirectory"
    );
    let directory_prefix = format!(
        "{}\\",
        directory_text.replace('/', "\\").trim_end_matches('\\')
    );
    let relative = normalized_payload
        .get(..directory_prefix.len())
        .is_some_and(|prefix| avelin_core::same_windows_name(prefix, &directory_prefix));
    let payload_value = if relative {
        normalized_payload[directory_prefix.len()..].to_string()
    } else {
        payload_text.to_string()
    };
    let entry = relative_path(&options.entry)
        .context("--entry must be a safe relative payload filename")?;
    let config = Config {
        schema_version: SCHEMA_VERSION,
        app: App {
            id: &options.app_id,
            name: &options.name,
            version: &options.app_version,
            entry: &entry,
        },
        payload: Payload {
            directory: &payload_value,
        },
        install: Install {
            directory_name: &options.app_id,
            desktop_shortcut: false,
            start_menu_shortcut: true,
        },
    };
    // A serializer quotes user text as data; no TOML interpolation or templates.
    let toml = toml::to_string_pretty(&config).context("serialize configuration as TOML")?;
    let (_, manifest) = crate::config::load_text(&directory, &toml)
        .context("init input is invalid; no configuration file was written")?;
    let _output_pins = pin_directories(&directory, true)?;
    create_config(&config_path, toml.as_bytes())?;
    Ok(json!({
        "config": config_path, "payload": payload, "payload_path_mode": if relative { "relative_to_config" } else { "absolute" },
        "app_id": manifest.app_id, "app_version": manifest.app_version, "entry": manifest.entry,
        "files": manifest.files.len(), "validated": true, "copied_application_files": false,
        "executed_application": false, "tool_version": env!("CARGO_PKG_VERSION"),
    }))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("avelin-m4-init-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            fs::create_dir(path.join("dist")).unwrap();
            fs::write(path.join("dist/app.exe"), b"test data, never executed").unwrap();
            Self(path)
        }
        fn options(&self) -> Options {
            Options {
                directory: self.0.clone(),
                app_id: "dev.avelin.init-fixture".into(),
                name: "示例 O'Brien \\ 工具 [Preview] = \"Desktop\"".into(),
                app_version: "1.0.0-preview.1".into(),
                entry: "app.exe".into(),
                payload: self.0.join("dist"),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Exact test-created paths only, including failed-init directories.
            for path in [
                "avelin.toml",
                "external/avelin.toml",
                "dist/app.exe",
                "dist/avelin.toml",
            ] {
                let _ = fs::remove_file(self.0.join(path));
            }
            for path in ["junction", "external", "dist"] {
                let _ = fs::remove_dir(self.0.join(path));
            }
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn generated_toml_round_trips_through_check_without_copying_or_running_payload() {
        let fixture = Fixture::new();
        let before = fs::read(fixture.0.join("dist/app.exe")).unwrap();
        let expected_name = fixture.options().name;
        let result = create(fixture.options()).unwrap();
        assert_eq!(result["payload_path_mode"], "relative_to_config");
        let config_path = fixture.0.join("avelin.toml");
        let text = fs::read_to_string(&config_path).unwrap();
        let raw: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(raw["payload"]["directory"].as_str(), Some("dist"));
        let (_, manifest) = crate::config::load(&config_path).unwrap();
        assert_eq!(manifest.app_name, expected_name);
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(fs::read(fixture.0.join("dist/app.exe")).unwrap(), before);
        assert_eq!(fs::read_dir(fixture.0.join("dist")).unwrap().count(), 1);
    }

    #[test]
    fn external_payload_is_explicit_and_existing_config_is_never_overwritten() {
        let fixture = Fixture::new();
        let mut options = fixture.options();
        options.directory = fixture.0.join("external");
        let result = create(options).unwrap();
        assert_eq!(result["payload_path_mode"], "absolute");
        let path = fixture.0.join("external/avelin.toml");
        crate::config::load(&path).unwrap();
        let before = fs::read(&path).unwrap();
        let mut options = fixture.options();
        options.directory = fixture.0.join("external");
        assert!(
            create(options)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn invalid_values_and_payload_overlap_leave_no_config() {
        let fixture = Fixture::new();
        for variant in 0..6 {
            let mut options = fixture.options();
            match variant {
                0 => options.app_id = "../escape".into(),
                1 => options.app_version = "not-semver".into(),
                2 => options.name = "line\n[install]\nentry='injected'".into(),
                3 => options.entry = "missing.exe".into(),
                4 => options.entry = "../app.exe".into(),
                _ => options.directory = fixture.0.join("dist"),
            }
            assert!(create(options).is_err(), "variant {variant}");
            assert!(!fixture.0.join("avelin.toml").exists());
            assert!(!fixture.0.join("dist/avelin.toml").exists());
        }
    }

    #[test]
    fn pinned_directory_cannot_be_replaced_while_creating_configuration() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let pins = pin_directories(&fixture.0.join("dist"), false).unwrap();
        assert!(fs::rename(fixture.0.join("dist"), fixture.0.join("moved")).is_err());
        assert!(
            OpenOptions::new()
                .write(true)
                .share_mode(7)
                .custom_flags(0x02200000)
                .open(fixture.0.join("dist"))
                .is_err()
        );
        drop(pins);
        drop(
            OpenOptions::new()
                .write(true)
                .share_mode(7)
                .custom_flags(0x02200000)
                .open(fixture.0.join("dist"))
                .unwrap(),
        );
        assert!(fs::rename(fixture.0.join("dist"), fixture.0.join("moved")).is_ok());
        fs::rename(fixture.0.join("moved"), fixture.0.join("dist")).unwrap();
    }

    #[test]
    fn junctions_are_rejected_without_writing_through_them() {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn DeviceIoControl(
                handle: *mut std::ffi::c_void,
                code: u32,
                input: *const std::ffi::c_void,
                input_length: u32,
                output: *mut std::ffi::c_void,
                output_length: u32,
                returned: *mut u32,
                overlapped: *mut std::ffi::c_void,
            ) -> i32;
        }
        let fixture = Fixture::new();
        let junction = fixture.0.join("junction");
        let target = fixture.0.join("dist");
        fs::create_dir(&junction).unwrap();
        let substitute: Vec<u16> = format!(r"\??\{}", target.display())
            .encode_utf16()
            .collect();
        let print: Vec<u16> = target.to_str().unwrap().encode_utf16().collect();
        let mut bytes = Vec::new();
        bytes.extend(0xa0000003u32.to_le_bytes());
        bytes.extend(((8 + (substitute.len() + print.len() + 2) * 2) as u16).to_le_bytes());
        bytes.extend(0u16.to_le_bytes());
        bytes.extend(0u16.to_le_bytes());
        bytes.extend(((substitute.len() * 2) as u16).to_le_bytes());
        bytes.extend((((substitute.len() + 1) * 2) as u16).to_le_bytes());
        bytes.extend(((print.len() * 2) as u16).to_le_bytes());
        for unit in substitute
            .into_iter()
            .chain(Some(0))
            .chain(print)
            .chain(Some(0))
        {
            bytes.extend(unit.to_le_bytes());
        }
        let handle = OpenOptions::new()
            .write(true)
            .share_mode(7)
            .custom_flags(0x02200000)
            .open(&junction)
            .unwrap();
        let mut returned = 0;
        assert_ne!(
            unsafe {
                DeviceIoControl(
                    handle.as_raw_handle(),
                    0x000900a4,
                    bytes.as_ptr().cast(),
                    bytes.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        drop(handle);
        let mut options = fixture.options();
        options.directory = junction.clone();
        assert!(create(options).is_err());
        let mut options = fixture.options();
        options.payload = junction;
        assert!(create(options).is_err());
        assert!(!target.join("avelin.toml").exists());
        assert!(!fixture.0.join("avelin.toml").exists());
        assert_eq!(
            fs::read(target.join("app.exe")).unwrap(),
            b"test data, never executed"
        );
    }
}
