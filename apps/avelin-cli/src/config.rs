//! Load and validate package configuration and inventory existing payload files.
use anyhow::{Context, Result, ensure};
use avelin_core::{
    FileResource, Manifest, SCHEMA_VERSION, hash_file, reject_reparse, relative_path,
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
        cfg.schema_version == SCHEMA_VERSION,
        "unsupported config schema"
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
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
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
