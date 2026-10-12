//! Build-time assets become bounded, self-contained manifest data.
use anyhow::{Context, Result, ensure};
use avelin_core::{
    presentation::{Locale, LocalePreference, Presentation, ThemeMode},
    reject_reparse, relative_path,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    template: Option<String>,
    theme: Option<ThemeMode>,
    locale: Option<LocalePreference>,
    fallback_locale: Option<Locale>,
    accent_color: Option<String>,
    logo: Option<PathBuf>,
    strings_dir: Option<PathBuf>,
}

fn local_asset(base: &Path, relative: &Path) -> Result<PathBuf> {
    let text = relative.to_str().context("UI asset path must be Unicode")?;
    let normalized = relative_path(text)
        .context("UI assets must be relative paths inside the config directory")?;
    let path = base.join(normalized);
    reject_reparse(&std::path::absolute(&path)?)?;
    Ok(path)
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("open UI asset {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "UI asset must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "UI asset exceeds {limit} bytes: {}",
        path.display()
    );
    Ok(bytes)
}

impl UiConfig {
    pub fn load(&self, config_directory: &Path) -> Result<Presentation> {
        let mut presentation = Presentation::default();
        if let Some(value) = &self.template {
            presentation.template = value.clone();
        }
        if let Some(value) = &self.theme {
            presentation.theme = *value;
        }
        if let Some(value) = &self.locale {
            presentation.locale = *value;
        }
        if let Some(value) = &self.fallback_locale {
            presentation.fallback_locale = *value;
        }
        if let Some(value) = &self.accent_color {
            presentation.accent_color = value.clone();
        }
        if let Some(path) = &self.logo {
            let path = local_asset(config_directory, path)?;
            presentation.logo_png = Some(read_bounded(&path, 512 * 1024)?);
        }
        if let Some(directory) = &self.strings_dir {
            let directory = local_asset(config_directory, directory)?;
            ensure!(
                directory.is_dir(),
                "strings_dir must be an existing directory"
            );
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_str().context("language filename must be Unicode")?;
                let locale = match name {
                    "zh-CN.json" => Locale::Chinese,
                    "en-US.json" => Locale::English,
                    _ => anyhow::bail!(
                        "unsupported file in strings_dir: {name}; expected zh-CN.json or en-US.json"
                    ),
                };
                reject_reparse(&entry.path())?;
                let strings: BTreeMap<String, String> =
                    serde_json::from_slice(&read_bounded(&entry.path(), 64 * 1024)?)
                        .with_context(|| format!("parse language overrides {name}"))?;
                presentation.strings.insert(locale, strings);
            }
        }
        presentation
            .validate()
            .context("invalid UI configuration")?;
        Ok(presentation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("avelin-m1-ui-config-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn branding_and_partial_translations_survive_a_manifest_round_trip() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("payload")).unwrap();
        fs::write(fixture.0.join("payload/app.exe"), b"test fixture").unwrap();
        fs::create_dir(fixture.0.join("locales")).unwrap();
        fs::write(
            fixture.0.join("locales/en-US.json"),
            br#"{"window.title":"Install {app_name}"}"#,
        )
        .unwrap();
        let logo = include_bytes!("../../../examples/nimbus/branding/nimbus.png");
        fs::write(fixture.0.join("logo.png"), logo).unwrap();
        let base = r#"
schema_version = 1
[app]
id = "dev.avelin.m1.fixture"
name = "Fixture"
version = "0.2.0"
entry = "app.exe"
[payload]
directory = "payload"
[install]
directory_name = "AvelinM1Fixture"
"#;
        let path = fixture.0.join("avelin.toml");
        fs::write(&path, base).unwrap();
        let (_, legacy) = crate::builder::load(&path).unwrap();
        assert!(
            serde_json::to_value(&legacy)
                .unwrap()
                .get("presentation")
                .is_none()
        );
        fs::write(&path, format!("{base}\n[ui]\naccent_color='#245DA8'\nlocale='en-US'\nlogo='logo.png'\nstrings_dir='locales'\n")).unwrap();
        let (_, manifest) = crate::builder::load(&path).unwrap();
        let decoded: avelin_core::Manifest =
            serde_json::from_slice(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        decoded.validate().unwrap();
        let presentation = decoded.presentation.unwrap();
        assert_eq!(presentation.logo_png.unwrap(), logo);
        assert_eq!(presentation.accent_color, "#245DA8");
        let presentation = manifest.presentation.unwrap();
        assert_eq!(
            presentation
                .text(Locale::English, "window.title", &[("app_name", "Fixture")])
                .unwrap(),
            "Install Fixture"
        );
        assert_eq!(
            presentation
                .text(Locale::Chinese, "window.title", &[("app_name", "Fixture")])
                .unwrap(),
            Presentation::default()
                .text(Locale::Chinese, "window.title", &[("app_name", "Fixture")])
                .unwrap()
        );
    }

    #[test]
    fn defaults_are_self_contained_and_invalid_options_fail_before_building() {
        let fixture = Fixture::new();
        UiConfig::default().load(&fixture.0).unwrap();
        for text in [
            "template='web'",
            "accent_color='not-a-color'",
            "logo='../outside.png'",
            "strings_dir='missing'",
        ] {
            let config: UiConfig = toml::from_str(text).unwrap();
            assert!(config.load(&fixture.0).is_err(), "{text}");
        }
        assert!(toml::from_str::<UiConfig>("unknown_option=true").is_err());
        assert!(toml::from_str::<UiConfig>("locale='made-up'").is_err());
    }

    #[test]
    fn malformed_oversized_or_unknown_locale_assets_are_rejected() {
        let fixture = Fixture::new();
        let locales = fixture.0.join("locales");
        fs::create_dir(&locales).unwrap();
        let config: UiConfig = toml::from_str("strings_dir='locales'").unwrap();
        let english = locales.join("en-US.json");
        fs::write(&english, b"not JSON").unwrap();
        assert!(config.load(&fixture.0).is_err());
        fs::write(&english, vec![b' '; 64 * 1024 + 1]).unwrap();
        assert!(config.load(&fixture.0).is_err());
        fs::write(&english, b"{}").unwrap();
        fs::write(locales.join("xx.json"), b"{}").unwrap();
        assert!(config.load(&fixture.0).is_err());
        fs::write(fixture.0.join("bad.png"), b"not PNG").unwrap();
        let config: UiConfig = toml::from_str("logo='bad.png'").unwrap();
        assert!(config.load(&fixture.0).is_err());
    }
}
