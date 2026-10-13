//! Load and validate package configuration and inventory existing payload files.
use anyhow::{Context, Result, ensure};
use avelin_core::{
    CONFIG_SCHEMA_VERSION, ComponentDefinition, FileResource, MANIFEST_SCHEMA_VERSION, Manifest,
    SCHEMA_VERSION, hash_file, reject_reparse, relative_path, same_windows_name,
};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    schema_version: u32,
    app: App,
    payload: Payload,
    install: Install,
    #[serde(default)]
    ui: Option<crate::presentation_config::UiConfig>,
    #[serde(default)]
    components: Option<Vec<ComponentConfig>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentConfig {
    id: String,
    required: bool,
    #[serde(default)]
    default_selected: Option<bool>,
    name: std::collections::BTreeMap<String, String>,
    description: std::collections::BTreeMap<String, String>,
    paths: Vec<String>,
}

fn assign_components(
    config: Option<Vec<ComponentConfig>>,
    files: &mut [FileResource],
) -> Result<Vec<ComponentDefinition>> {
    let Some(config) = config else {
        for file in files {
            file.component_id = Some("core".into());
        }
        return Ok(vec![ComponentDefinition::core()]);
    };
    ensure!(
        !config.is_empty() && config.len() <= 32,
        "configuration requires 1 to 32 components"
    );
    let mut definitions = Vec::new();
    let mut ids = std::collections::BTreeSet::new();
    // Rules are compared independently of inventory: an overlapping directory
    // cannot become valid merely because its overlapping subtree is empty today.
    let mut rules: Vec<(String, bool)> = Vec::new();
    for component in config {
        let definition = ComponentDefinition {
            default_selected: component.default_selected.unwrap_or(component.id == "core"),
            id: component.id,
            required: component.required,
            name: component.name,
            description: component.description,
        };
        definition.validate()?;
        ensure!(
            ids.insert(definition.id.clone()),
            "duplicate component ID: {}",
            definition.id
        );
        ensure!(
            !component.paths.is_empty(),
            "component {} has no paths",
            definition.id
        );
        for raw in component.paths {
            let directory = raw.ends_with('/');
            let value = if directory {
                &raw[..raw.len() - 1]
            } else {
                raw.as_str()
            };
            let path =
                relative_path(value).with_context(|| format!("invalid component path: {raw}"))?;
            let contains = |parent: &str, child: &str| {
                child.rsplit_once('\\').is_some()
                    && child
                        .match_indices('\\')
                        .any(|(i, _)| same_windows_name(parent, &child[..i]))
            };
            for (previous, previous_directory) in &rules {
                ensure!(
                    !same_windows_name(previous, &path)
                        && !(*previous_directory && contains(previous, &path))
                        && !(directory && contains(&path, previous)),
                    "overlapping component path rules: {previous} / {raw}"
                );
            }
            let mut count = 0;
            for file in files.iter_mut() {
                if (!directory && same_windows_name(&path, &file.path))
                    || (directory && contains(&path, &file.path))
                {
                    ensure!(
                        file.component_id.is_none(),
                        "file belongs to multiple components: {}",
                        file.path
                    );
                    file.component_id = Some(definition.id.clone());
                    count += 1;
                }
            }
            ensure!(
                count > 0,
                "component path is missing or contains no files: {raw}"
            );
            rules.push((path, directory));
        }
        definitions.push(definition);
    }
    for file in files {
        ensure!(
            file.component_id.is_some(),
            "payload file has no component owner: {}",
            file.path
        );
    }
    Ok(definitions)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct App {
    id: String,
    name: String,
    version: String,
    entry: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    directory: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Install {
    directory_name: String,
    #[serde(default)]
    desktop_shortcut: bool,
    #[serde(default)]
    start_menu_shortcut: bool,
}

fn inventory(base: &Path, at: &Path, files: &mut Vec<FileResource>) -> Result<()> {
    reject_reparse(at)?;
    for entry in fs::read_dir(at)? {
        let path = entry?.path();
        reject_reparse(&path)?;
        if path.is_dir() {
            inventory(base, &path, files)?;
        } else if path.is_file() {
            let rel = relative_path(
                path.strip_prefix(base)?
                    .to_str()
                    .context("non-Unicode filename")?,
            )?;
            files.push(FileResource {
                path: rel,
                size: fs::metadata(&path)?.len(),
                sha256: hash_file(&path)?,
                component_id: None,
            });
        } else {
            anyhow::bail!("unsupported payload entry: {}", path.display());
        }
    }
    Ok(())
}
pub fn load(config_path: &Path) -> Result<(PathBuf, Manifest)> {
    let config_path = std::path::absolute(config_path).context("resolve configuration path")?;
    reject_reparse(&config_path)?;
    let text = fs::read_to_string(&config_path).with_context(|| {
        format!(
            "read configuration {}; pass --config <existing avelin.toml> or create one with avelin-cli init --help",
            config_path.display()
        )
    })?;
    load_text(config_path.parent().context("config parent")?, &text)
}

/// `init` validates serialized TOML through exactly the same path as `check`.
pub(crate) fn load_text(config_directory: &Path, text: &str) -> Result<(PathBuf, Manifest)> {
    let cfg: Config = toml::from_str(text)
        .context("parse avelin.toml; check section names and TOML string quoting")?;
    ensure!(
        matches!(cfg.schema_version, SCHEMA_VERSION | CONFIG_SCHEMA_VERSION),
        "unsupported config schema"
    );
    ensure!(
        cfg.schema_version != SCHEMA_VERSION || cfg.components.is_none(),
        "components require config schema 2"
    );
    ensure!(
        cfg.schema_version != CONFIG_SCHEMA_VERSION || cfg.components.is_some(),
        "config schema 2 requires explicit components"
    );
    let source = config_directory.join(cfg.payload.directory);
    reject_reparse(&std::path::absolute(&source)?)?;
    ensure!(
        source.is_dir(),
        "payload directory is missing or is not a directory: {}; set [payload].directory to existing application files (relative to avelin.toml or absolute)",
        source.display()
    );
    let source = fs::canonicalize(&source)
        .with_context(|| format!("read payload directory {}", source.display()))?;
    let mut files = Vec::new();
    inventory(&source, &source, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let components = assign_components(cfg.components, &mut files)?;
    let manifest = Manifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        components,
        app_id: cfg.app.id,
        app_name: cfg.app.name,
        app_version: cfg.app.version,
        entry: relative_path(&cfg.app.entry)?,
        directory_name: cfg.install.directory_name,
        desktop_shortcut: cfg.install.desktop_shortcut,
        start_menu_shortcut: cfg.install.start_menu_shortcut,
        files,
        runtime_sha256: None,
        presentation: cfg
            .ui
            .as_ref()
            .map(|ui| ui.load(config_directory))
            .transpose()?,
    };
    manifest.validate().context(
        "validate app/payload configuration; [app].entry must name an existing payload file",
    )?;
    Ok((source, manifest))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("avelin-component-config-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            fs::create_dir(path.join("payload")).unwrap();
            fs::create_dir(path.join("payload/extra")).unwrap();
            fs::create_dir(path.join("payload/tools")).unwrap();
            for file in ["app.exe", "extra/data.bin", "tools/cli.exe"] {
                fs::write(path.join("payload").join(file), b"fixture data").unwrap();
            }
            Self(path)
        }
        fn load(&self, text: &str) -> Result<Manifest> {
            load_text(&self.0, text).map(|(_, m)| m)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for file in ["app.exe", "extra/data.bin", "tools/cli.exe"] {
                let _ = fs::remove_file(self.0.join("payload").join(file));
            }
            for folder in ["payload/extra", "payload/tools", "payload", ""] {
                let _ = fs::remove_dir(self.0.join(folder));
            }
        }
    }
    const HEADER: &str = r#"schema_version = 2
[app]
id = "dev.avelin.component-fixture"
name = "Component fixture"
version = "1.0.0"
entry = "app.exe"
[payload]
directory = "payload"
[install]
directory_name = "ComponentFixture"
"#;
    fn group(id: &str, required: bool, paths: &str) -> String {
        format!(
            r#"
[[components]]
id = "{id}"
required = {required}
name = {{ "zh-CN" = "组件", "en-US" = "Component" }}
description = {{ "zh-CN" = "组件说明", "en-US" = "Component description" }}
paths = [{paths}]
"#
        )
    }
    fn complete() -> String {
        format!(
            "{}{}{}{}",
            HEADER,
            group("core", true, "\"app.exe\""),
            group("extra", false, "\"extra/\""),
            group("tools", false, "\"tools/cli.exe\"")
        )
    }

    #[test]
    fn schema_two_builds_complete_manifest_with_one_owner_per_file() {
        let fixture = Fixture::new();
        let manifest = fixture.load(&complete()).unwrap();
        assert_eq!(manifest.schema_version, 2);
        assert_eq!(manifest.components.len(), 3);
        assert_eq!(manifest.files.len(), 3);
        assert_eq!(manifest.default_selection(), ["core"]);
        assert_eq!(manifest.files[1].component_id.as_deref(), Some("extra"));
        assert!(manifest.components[0].default_selected);
    }
    #[test]
    fn legacy_config_outputs_new_single_core_manifest_and_schema_two_needs_explicit_ownership() {
        let fixture = Fixture::new();
        let legacy = HEADER.replace("schema_version = 2", "schema_version = 1");
        let manifest = fixture.load(&legacy).unwrap();
        assert_eq!(manifest.schema_version, 2);
        assert_eq!(manifest.components, [ComponentDefinition::core()]);
        assert!(
            manifest
                .files
                .iter()
                .all(|f| f.component_id.as_deref() == Some("core"))
        );
        assert!(fixture.load(HEADER).is_err());
        assert!(
            fixture
                .load(&complete().replace("schema_version = 2", "schema_version = 1"))
                .is_err()
        );
    }
    #[test]
    fn missing_overlapping_ambiguous_unsafe_and_unowned_paths_are_rejected() {
        let fixture = Fixture::new();
        for rules in [
            vec![group("core", true, "\"app.exe\"")],
            vec![
                group("core", true, "\"app.exe\",\"extra/\",\"tools/\""),
                group("extra", false, "\"EXTRA/data.bin\""),
            ],
            vec![
                group("core", true, "\"app.exe\",\"extra/\",\"tools/\""),
                group("extra", false, "\"extra/\""),
            ],
            vec![
                group("core", true, "\"app.exe\",\"extra/\",\"tools/\""),
                group("extra", false, "\"missing/\""),
            ],
            vec![
                group("core", true, "\"app.exe\",\"extra/\",\"tools/\""),
                group("extra", false, "\"../outside\""),
            ],
            vec![
                group("core", true, "\"app.exe\",\"extra/\",\"tools/\""),
                group("extra", false, "\"*.bin\""),
            ],
            vec![
                group("core", true, "\"app.exe\",\"extra/\",\"tools/\""),
                group("extra", false, ""),
            ],
            vec![
                group("core", true, "\"extra/\",\"tools/\""),
                group("extra", false, "\"app.exe\""),
            ],
        ] {
            assert!(
                fixture
                    .load(&format!("{HEADER}{}", rules.concat()))
                    .is_err(),
                "accepted {}",
                rules.concat()
            );
        }
    }
    #[test]
    fn unsupported_dependency_fields_invalid_localization_and_false_core_default_fail() {
        let fixture = Fixture::new();
        let text = complete();
        for invalid in [
            text.replace(
                "required = true",
                "required = true\ndefault_selected = false",
            ),
            text.replace("required = false", "required = false\nrequires = ['core']"),
            text.replace("\"en-US\" = \"Component\"", "\"fr\" = \"Component\""),
            text.replace("id = \"extra\"", "id = \"core\""),
            text.replace("id = \"extra\"", "id = \"UPPER\""),
        ] {
            assert!(fixture.load(&invalid).is_err());
        }
    }
}
