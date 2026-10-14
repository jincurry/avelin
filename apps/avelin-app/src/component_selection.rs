//! Resolve user intent against the full package and the verified installed receipt.
use crate::{coordinator::Operation, execution_files::PinnedFile, resources};
use anyhow::{Context, Result, ensure};
use avelin_core::{Manifest, Receipt, SelectionSource, resolve_selection};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct ComponentContext {
    pub selected: Vec<String>,
    pub source: SelectionSource,
    pub installed: Vec<String>,
    pub receipt_sha256: Option<String>,
    pub migration_details: Vec<(String, String)>,
    pub requires_migration_confirmation: bool,
}

pub fn parse_explicit(value: &str) -> Result<Vec<String>> {
    avelin_core::parse_selection(value).context("invalid --components final selection")
}

pub fn resolve(
    manifest: &Manifest,
    target: &Path,
    sid: &str,
    operation: Operation,
    explicit: Option<&str>,
    allow_migration_preview: bool,
) -> Result<ComponentContext> {
    let explicit = explicit.map(parse_explicit).transpose()?;
    let mut context = ComponentContext {
        selected: manifest.default_selection(),
        source: SelectionSource::Default,
        installed: vec![],
        receipt_sha256: None,
        migration_details: vec![],
        requires_migration_confirmation: false,
    };
    if matches!(
        operation,
        Operation::Upgrade | Operation::Modify | Operation::Repair | Operation::Uninstall
    ) {
        let old = if operation == Operation::Upgrade {
            let input = PinnedFile::open(&target.join(".avelin/manifest.json"))?;
            serde_json::from_slice(&input.read_bytes()?)?
        } else {
            manifest.clone()
        };
        ensure!(
            old.install_scope == manifest.install_scope,
            "component maintenance cannot change installation scope"
        );
        let (receipt, hash): (Receipt, _) = resources::read_receipt_with_hash(&old, target, sid)?;
        context.installed = receipt.selection(&old)?;
        context.receipt_sha256 = Some(hash);
        if operation == Operation::Upgrade {
            if old.schema_version == 1 && manifest.schema_version == 2 {
                let migration = avelin_core::legacy_selection(manifest, &receipt)?;
                context.selected = migration.selected_components;
                context.source = SelectionSource::LegacyMigration;
                for id in migration.ambiguous_components {
                    let files: Vec<_> = manifest
                        .files
                        .iter()
                        .filter(|f| f.component_id.as_deref() == Some(id.as_str()))
                        .collect();
                    let existing = files
                        .iter()
                        .filter(|f| {
                            receipt
                                .files
                                .iter()
                                .any(|old| avelin_core::same_windows_name(&f.path, &old.path))
                        })
                        .count();
                    context.migration_details.push((
                        format!("组件 {id} 含 {existing} 个原有文件和 {} 个新增文件。勾选将安装全部内容；不勾选将移除原有未修改文件，保留已修改文件。", files.len() - existing),
                        format!("Component {id}: {existing} existing and {} new files. Select to install all; deselect to remove original files and preserve user changes.", files.len() - existing),
                    ));
                }
                context.requires_migration_confirmation =
                    !context.migration_details.is_empty() && explicit.is_none();
                ensure!(
                    !context.requires_migration_confirmation || allow_migration_preview,
                    "legacy component migration needs an explicit --components selection"
                );
            } else {
                context.selected = avelin_core::inherited_selection(manifest, &old, &receipt)?;
                context.source = SelectionSource::Inherited;
            }
            for id in &context.installed {
                if id != "core"
                    && !manifest
                        .components
                        .iter()
                        .any(|component| &component.id == id)
                {
                    context.migration_details.push((format!("原组件 {id} 已从新包移除；其原版文件将删除，修改文件将保留。"),
                        format!("Component {id} was removed from this package; original files will be removed and changed files preserved.")));
                }
            }
        } else {
            context.selected = context.installed.clone();
            context.source = SelectionSource::Receipt;
        }
    }
    if let Some(ids) = explicit {
        context.selected = ids;
        context.source = SelectionSource::Explicit;
    }
    resolve_selection(manifest, &context.selected).context("invalid component selection")?;
    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_selection_is_complete_unambiguous_and_canonical() {
        assert_eq!(parse_explicit("b,core,a").unwrap(), ["a", "b", "core"]);
        for bad in ["", ",", "core,", "a", "core,core", "core, a", "core ,a"] {
            assert!(parse_explicit(bad).is_err(), "{bad:?}");
        }
    }
}
