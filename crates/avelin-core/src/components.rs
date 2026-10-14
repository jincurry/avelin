//! Complete package ownership and deterministic component selection. Legacy
//! metadata stays byte-compatible; implicit core ownership is interpreted here.
use crate::{
    FileResource, MANIFEST_SCHEMA_VERSION, Manifest, RECEIPT_SCHEMA_VERSION, Receipt,
    SCHEMA_VERSION, hash_bytes,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComponentDefinition {
    pub id: String,
    pub required: bool,
    #[serde(default)]
    pub default_selected: bool,
    pub name: BTreeMap<String, String>,
    pub description: BTreeMap<String, String>,
}

impl ComponentDefinition {
    pub fn core() -> Self {
        Self {
            id: "core".into(),
            required: true,
            default_selected: true,
            name: [
                ("zh-CN".into(), "主程序".into()),
                ("en-US".into(), "Application".into()),
            ]
            .into(),
            description: [
                ("zh-CN".into(), "运行应用所需的文件".into()),
                (
                    "en-US".into(),
                    "Files required to run the application".into(),
                ),
            ]
            .into(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_component_id(&self.id),
            "invalid component ID: {}",
            self.id
        );
        ensure!(
            self.required == (self.id == "core"),
            "only core must be required"
        );
        ensure!(
            self.id != "core" || self.default_selected,
            "core must be selected by default"
        );
        for (label, text, limit) in [
            ("name", &self.name, 80),
            ("description", &self.description, 320),
        ] {
            ensure!(
                text.len() == 2 && text.contains_key("zh-CN") && text.contains_key("en-US"),
                "component {} requires zh-CN and en-US {}",
                self.id,
                label
            );
            for value in text.values() {
                ensure!(
                    !value.trim().is_empty()
                        && value.encode_utf16().count() <= limit
                        && !value.chars().any(char::is_control),
                    "invalid component {} {}",
                    self.id,
                    label
                );
            }
        }
        Ok(())
    }
}

pub fn valid_component_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

pub(crate) fn validate_manifest_components(manifest: &Manifest) -> Result<()> {
    if manifest.schema_version == SCHEMA_VERSION {
        ensure!(
            manifest.components.is_empty()
                && manifest.files.iter().all(|f| f.component_id.is_none()),
            "schema 1 cannot contain component fields"
        );
        return Ok(());
    }
    ensure!(
        manifest.schema_version == MANIFEST_SCHEMA_VERSION,
        "unsupported manifest schema"
    );
    ensure!(
        !manifest.components.is_empty() && manifest.components.len() <= 32,
        "manifest requires 1 to 32 components"
    );
    let mut counts = BTreeMap::new();
    for component in &manifest.components {
        component.validate()?;
        ensure!(
            counts.insert(component.id.as_str(), 0usize).is_none(),
            "duplicate component ID: {}",
            component.id
        );
    }
    ensure!(
        counts.contains_key("core"),
        "manifest requires core component"
    );
    for file in &manifest.files {
        let id = file
            .component_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("file lacks component ownership: {}", file.path))?;
        let count = counts
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("file references unknown component: {}", file.path))?;
        *count += 1;
        ensure!(
            file.path != manifest.entry || id == "core",
            "application entry must belong to core"
        );
    }
    ensure!(
        counts.values().all(|count| *count > 0),
        "empty component is not allowed"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSource {
    Default,
    Inherited,
    Explicit,
    LegacyMigration,
    Receipt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedResources {
    pub files: Vec<FileResource>,
    pub payload_file_count: u64,
    pub payload_bytes: u64,
}

/// Parse a complete explicit selection. Empty entries and duplicates are errors;
/// successful CLI input is normalized once before freezing it in a plan.
pub fn parse_selection(text: &str) -> Result<Vec<String>> {
    let mut ids = BTreeSet::new();
    for id in text.split(',') {
        ensure!(valid_component_id(id), "invalid component ID: {id}");
        ensure!(ids.insert(id.to_owned()), "duplicate component ID: {id}");
    }
    ensure!(
        ids.contains("core"),
        "component selection must include core"
    );
    Ok(ids.into_iter().collect())
}

pub fn resolve_selection(manifest: &Manifest, selection: &[String]) -> Result<SelectedResources> {
    manifest.validate()?;
    let legacy = manifest.schema_version == SCHEMA_VERSION;
    if legacy {
        ensure!(
            selection.is_empty() || selection == ["core"],
            "schema 1 only supports core"
        );
    } else {
        ensure!(
            !selection.is_empty() && selection.windows(2).all(|pair| pair[0] < pair[1]),
            "component selection must be sorted, unique and nonempty"
        );
        ensure!(
            selection.iter().any(|id| id == "core"),
            "component selection must include core"
        );
        for id in selection {
            ensure!(
                manifest
                    .components
                    .iter()
                    .any(|component| &component.id == id),
                "unknown selected component: {id}"
            );
        }
    }
    let files: Vec<_> = manifest
        .files
        .iter()
        .filter(|file| {
            legacy
                || selection
                    .binary_search(file.component_id.as_ref().expect("validated ownership"))
                    .is_ok()
        })
        .cloned()
        .collect();
    let payload_bytes = files.iter().try_fold(0u64, |sum, file| {
        sum.checked_add(file.size)
            .ok_or_else(|| anyhow::anyhow!("selected payload byte count overflow"))
    })?;
    Ok(SelectedResources {
        payload_file_count: files.len() as u64,
        files,
        payload_bytes,
    })
}

impl Manifest {
    pub fn default_selection(&self) -> Vec<String> {
        if self.schema_version == SCHEMA_VERSION {
            return vec!["core".into()];
        }
        let mut result: Vec<_> = self
            .components
            .iter()
            .filter(|c| c.required || c.default_selected)
            .map(|c| c.id.clone())
            .collect();
        result.sort();
        result
    }
    pub fn resolve_selection(&self, selected: &[String]) -> Result<SelectedResources> {
        resolve_selection(self, selected)
    }
}

impl Receipt {
    pub fn selection(&self, manifest: &Manifest) -> Result<Vec<String>> {
        self.validate_manifest(manifest)?;
        if self.schema_version == SCHEMA_VERSION {
            Ok(vec!["core".into()])
        } else {
            Ok(self.selected_components.clone())
        }
    }
    /// Structural equality to the complete selected resource set, independently
    /// of mutable on-disk contents. Preserves legacy serialization and hashes.
    pub fn validate_manifest(&self, manifest: &Manifest) -> Result<()> {
        manifest.validate()?;
        ensure!(
            self.install_scope == manifest.install_scope,
            "receipt installation scope differs from manifest"
        );
        ensure!(
            self.schema_version == manifest.schema_version
                && matches!(self.schema_version, SCHEMA_VERSION | RECEIPT_SCHEMA_VERSION),
            "receipt schema does not match manifest"
        );
        ensure!(
            self.schema_version != SCHEMA_VERSION || self.selected_components.is_empty(),
            "schema 1 receipt cannot contain component selection"
        );
        let selected = resolve_selection(manifest, &self.selected_components)?;
        ensure!(
            self.app_id == manifest.app_id
                && self.app_version == manifest.app_version
                && self.manifest_sha256 == hash_bytes(&serde_json::to_vec(manifest)?)
                && self.files == selected.files,
            "receipt does not match selected manifest resources"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacySelection {
    pub selected_components: Vec<String>,
    pub ambiguous_components: Vec<String>,
}

/// Selection suggestions only: path overlap is never evidence of disk ownership.
pub fn legacy_selection(new: &Manifest, receipt: &Receipt) -> Result<LegacySelection> {
    new.validate()?;
    ensure!(
        new.app_id == receipt.app_id,
        "component migration application identity mismatch"
    );
    ensure!(
        receipt.schema_version == SCHEMA_VERSION,
        "legacy selection requires schema 1 receipt"
    );
    let mut result = LegacySelection {
        selected_components: vec!["core".into()],
        ambiguous_components: vec![],
    };
    let old_paths: BTreeSet<_> = receipt
        .files
        .iter()
        .map(|file| crate::model::WindowsPath(file.path.clone()))
        .collect();
    for component in new.components.iter().filter(|c| !c.required) {
        let mut old_count = 0;
        let mut new_count = 0;
        for file in new
            .files
            .iter()
            .filter(|f| f.component_id.as_deref() == Some(&component.id))
        {
            if old_paths.contains(&crate::model::WindowsPath(file.path.clone())) {
                old_count += 1;
            } else {
                new_count += 1;
            }
        }
        if old_count > 0 && new_count == 0 {
            result.selected_components.push(component.id.clone());
        } else if old_count > 0 {
            result.ambiguous_components.push(component.id.clone());
        }
    }
    result.selected_components.sort();
    result.ambiguous_components.sort();
    Ok(result)
}

pub fn inherited_selection(
    new: &Manifest,
    old: &Manifest,
    receipt: &Receipt,
) -> Result<Vec<String>> {
    receipt.validate_manifest(old)?;
    new.validate()?;
    ensure!(
        new.app_id == old.app_id,
        "component inheritance application identity mismatch"
    );
    if receipt.schema_version == SCHEMA_VERSION {
        let result = legacy_selection(new, receipt)?;
        ensure!(
            result.ambiguous_components.is_empty(),
            "legacy component migration requires an explicit selection: {}",
            result.ambiguous_components.join(",")
        );
        return Ok(result.selected_components);
    }
    ensure!(
        new.schema_version == MANIFEST_SCHEMA_VERSION,
        "cannot migrate component installation back to schema 1"
    );
    let selected: Vec<_> = receipt
        .selected_components
        .iter()
        .filter(|id| new.components.iter().any(|c| &c.id == *id))
        .cloned()
        .collect();
    resolve_selection(new, &selected)?;
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InstallPlan, Operation, UpgradeDiff, UpgradePlan};

    fn manifest() -> Manifest {
        let mut value: Manifest =
            serde_json::from_str(include_str!("../../../docs/schemas/examples/manifest.json"))
                .unwrap();
        value.schema_version = MANIFEST_SCHEMA_VERSION;
        value.entry = "app.exe".into();
        let mut optional = ComponentDefinition::core();
        optional.id = "extra".into();
        optional.required = false;
        optional.default_selected = true;
        let mut additional = optional.clone();
        additional.id = "tools".into();
        additional.default_selected = false;
        value.components = vec![ComponentDefinition::core(), optional, additional];
        value.files = [
            ("app.exe", "core", 10),
            ("extra\\file.bin", "extra", 20),
            ("tools\\cli.exe", "tools", 30),
        ]
        .into_iter()
        .map(|(path, id, size)| FileResource {
            path: path.into(),
            size,
            sha256: hash_bytes(b"baseline"),
            component_id: Some(id.into()),
        })
        .collect();
        value
    }
    fn receipt(manifest: &Manifest, ids: &[&str]) -> Receipt {
        let mut receipt: Receipt =
            serde_json::from_str(include_str!("../../../docs/schemas/examples/receipt.json"))
                .unwrap();
        receipt.schema_version = manifest.schema_version;
        receipt.selected_components = ids.iter().map(|id| (*id).into()).collect();
        receipt.files = resolve_selection(manifest, &receipt.selected_components)
            .unwrap()
            .files;
        receipt.app_version = manifest.app_version.clone();
        receipt.manifest_sha256 = hash_bytes(&serde_json::to_vec(manifest).unwrap());
        receipt.uninstaller_sha256 = Some(hash_bytes(b"backend"));
        receipt
    }

    #[test]
    fn explicit_selection_is_complete_sorted_and_strict() {
        assert_eq!(
            parse_selection("tools,core,extra").unwrap(),
            ["core", "extra", "tools"]
        );
        for invalid in [
            "",
            ",",
            "core,",
            "core,core",
            "extra",
            " core",
            "core,UPPER",
            "core,*",
        ] {
            assert!(parse_selection(invalid).is_err(), "accepted {invalid}");
        }
        let manifest = manifest();
        assert_eq!(manifest.default_selection(), ["core", "extra"]);
        let selected = resolve_selection(&manifest, &["core".into(), "tools".into()]).unwrap();
        assert_eq!(selected.payload_file_count, 2);
        assert_eq!(selected.payload_bytes, 40);
        assert_eq!(selected.files[1].path, "tools\\cli.exe");
        for ids in [
            vec![],
            vec!["tools"],
            vec!["core", "core"],
            vec!["tools", "core"],
            vec!["core", "unknown"],
        ] {
            assert!(
                resolve_selection(
                    &manifest,
                    &ids.into_iter().map(String::from).collect::<Vec<_>>()
                )
                .is_err()
            );
        }
        assert_eq!(
            manifest.files.len(),
            3,
            "selection must never truncate the package manifest"
        );
    }

    #[test]
    fn manifest_ownership_and_required_component_cannot_be_forged() {
        let original = manifest();
        original.validate().unwrap();
        for variant in 0..8 {
            let mut value = original.clone();
            match variant {
                0 => value.files[0].component_id = Some("extra".into()),
                1 => value.files[0].component_id = None,
                2 => value.files[1].component_id = Some("unknown".into()),
                3 => value.components[0].required = false,
                4 => value.components[1].required = true,
                5 => value.components[0].default_selected = false,
                6 => value.components[1]
                    .name
                    .remove("en-US")
                    .map(|_| ())
                    .unwrap(),
                _ => value.schema_version = SCHEMA_VERSION,
            }
            assert!(value.validate().is_err(), "accepted variant {variant}");
        }
    }

    #[test]
    fn receipts_equal_the_exact_selected_set_and_keep_original_hashes() {
        let manifest = manifest();
        let original = receipt(&manifest, &["core"]);
        original.validate_manifest(&manifest).unwrap();
        let mut value = original.clone();
        value.files.push(manifest.files[1].clone());
        assert!(value.validate_manifest(&manifest).is_err());
        let mut value = original.clone();
        value.files.clear();
        assert!(value.validate_manifest(&manifest).is_err());
        let mut value = original.clone();
        value.files[0].component_id = Some("extra".into());
        assert!(value.validate_manifest(&manifest).is_err());
        let mut value = original;
        value.selected_components.push("extra".into());
        assert!(value.validate_manifest(&manifest).is_err());
    }

    #[test]
    fn upgrade_inherits_existing_choice_and_never_new_defaults() {
        let old = manifest();
        let receipt = receipt(&old, &["core", "tools"]);
        let mut new = old.clone();
        new.components[2].default_selected = true;
        new.components[1].default_selected = true;
        assert_eq!(
            inherited_selection(&new, &old, &receipt).unwrap(),
            ["core", "tools"]
        );
        new.components.retain(|c| c.id != "tools");
        new.files
            .retain(|f| f.component_id.as_deref() != Some("tools"));
        assert_eq!(inherited_selection(&new, &old, &receipt).unwrap(), ["core"]);
        let selected = resolve_selection(&new, &["core".into()]).unwrap();
        let diff = UpgradeDiff::between_files(&receipt.files, &selected.files).unwrap();
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.unchanged.len(), 1);
        assert!(diff.added.is_empty());
    }

    #[test]
    fn legacy_migration_requires_all_paths_and_explicit_mixed_confirmation() {
        let new = manifest();
        let mut old = new.clone();
        old.schema_version = SCHEMA_VERSION;
        old.components.clear();
        old.files
            .retain(|f| f.component_id.as_deref() != Some("tools"));
        for f in &mut old.files {
            f.component_id = None;
        }
        let receipt = receipt(&old, &[]);
        receipt.validate_manifest(&old).unwrap();
        assert_eq!(
            inherited_selection(&new, &old, &receipt).unwrap(),
            ["core", "extra"]
        );
        let mut mixed = new.clone();
        let mut added = mixed.files[1].clone();
        added.path = "extra\\new.bin".into();
        mixed.files.push(added);
        let suggestion = legacy_selection(&mixed, &receipt).unwrap();
        assert_eq!(suggestion.ambiguous_components, ["extra"]);
        assert_eq!(suggestion.selected_components, ["core"]);
        assert!(inherited_selection(&mixed, &old, &receipt).is_err());
    }

    #[test]
    fn selected_plan_binds_complete_package_and_exact_statistics() {
        let manifest = manifest();
        let plan = InstallPlan::new_selected(
            &manifest,
            Operation::Install,
            "new-task".into(),
            std::env::temp_dir().join("AvelinComponentTarget"),
            "S-1-5-21-1".into(),
            vec!["core".into()],
            SelectionSource::Explicit,
        )
        .unwrap();
        assert_eq!(plan.payload_bytes, 10);
        assert_eq!(plan.payload_file_count, 1);
        assert_eq!(plan.package_payload_bytes, Some(60));
        assert_eq!(
            plan.manifest_sha256,
            hash_bytes(&serde_json::to_vec(&manifest).unwrap())
        );
        let mut changed = plan.clone();
        changed.selected_components.push("extra".into());
        assert!(changed.validate(&manifest).is_err());
        let mut changed = plan;
        changed.payload_bytes = 60;
        assert!(changed.validate(&manifest).is_err());
    }

    #[test]
    fn modify_requires_same_package_backend_snapshot_and_changed_selection() {
        let manifest = manifest();
        let receipt = receipt(&manifest, &["core"]);
        let selected = vec!["core".into(), "tools".into()];
        let context = UpgradePlan {
            previous_version: manifest.app_version.clone(),
            previous_manifest_sha256: receipt.manifest_sha256.clone(),
            previous_receipt_sha256: hash_bytes(b"original receipt"),
            previous_task_id: receipt.task_id.clone(),
            previous_commit_id: receipt.commit_id.clone(),
            installation_id: receipt.installation_id.clone(),
            diff: UpgradeDiff::between_files(
                &receipt.files,
                &resolve_selection(&manifest, &selected).unwrap().files,
            )
            .unwrap(),
            staging_dir: std::env::temp_dir().join("AvelinComponentSession/stage"),
            backup_dir: std::env::temp_dir().join("AvelinComponentSession/backup"),
            required_space_bytes: 1024,
            selected_components: selected,
            previous_selected_components: receipt.selected_components.clone(),
            modify_snapshot_sha256: Some(hash_bytes(b"snapshot")),
            backend_sha256: receipt.uninstaller_sha256.clone(),
        };
        let plan = InstallPlan::new_modify(
            &manifest,
            &receipt,
            &hash_bytes(b"original receipt"),
            "new-task".into(),
            context.clone(),
        )
        .unwrap();
        plan.validate_modify(&manifest, &receipt, &hash_bytes(b"original receipt"))
            .unwrap();
        let mut wrong = context.clone();
        wrong.backend_sha256 = Some(hash_bytes(b"another backend"));
        assert!(
            InstallPlan::new_modify(
                &manifest,
                &receipt,
                &hash_bytes(b"original receipt"),
                "new-task".into(),
                wrong
            )
            .is_err()
        );
        let mut wrong = context.clone();
        wrong.modify_snapshot_sha256 = None;
        assert!(
            InstallPlan::new_modify(
                &manifest,
                &receipt,
                &hash_bytes(b"original receipt"),
                "new-task".into(),
                wrong
            )
            .is_err()
        );
        let mut same = context;
        same.selected_components = receipt.selected_components.clone();
        same.diff = UpgradeDiff::between_files(&receipt.files, &receipt.files).unwrap();
        assert!(
            InstallPlan::new_modify(
                &manifest,
                &receipt,
                &hash_bytes(b"original receipt"),
                "new-task".into(),
                same
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_manifest_and_plan_have_no_invented_component_serialization() {
        let raw = include_str!("../../../docs/schemas/examples/manifest.json");
        let manifest: Manifest = serde_json::from_str(raw).unwrap();
        let encoded = serde_json::to_value(&manifest).unwrap();
        assert!(encoded.get("components").is_none());
        assert!(encoded["files"][0].get("component_id").is_none());
        let receipt: Receipt =
            serde_json::from_str(include_str!("../../../docs/schemas/examples/receipt.json"))
                .unwrap();
        receipt.validate_manifest(&manifest).unwrap();
        assert_eq!(
            receipt.manifest_sha256,
            hash_bytes(&serde_json::to_vec(&manifest).unwrap())
        );
        let plan = InstallPlan::new(
            &manifest,
            Operation::Install,
            "new-task".into(),
            std::env::temp_dir().join("AvelinLegacyTarget"),
            "S-1-5-21-1".into(),
        )
        .unwrap();
        let encoded = serde_json::to_value(plan).unwrap();
        assert_eq!(encoded["schema_version"], 1);
        assert!(encoded.get("selected_components").is_none());
        assert!(encoded.get("selection_source").is_none());
    }

    #[test]
    fn documented_component_fixtures_match_real_model_bindings() {
        let manifest: Manifest = serde_json::from_str(include_str!(
            "../../../docs/schemas/examples/components-manifest.json"
        ))
        .unwrap();
        let receipt: Receipt = serde_json::from_str(include_str!(
            "../../../docs/schemas/examples/components-receipt.json"
        ))
        .unwrap();
        let plan: InstallPlan = serde_json::from_str(include_str!(
            "../../../docs/schemas/examples/components-install-plan.json"
        ))
        .unwrap();
        let modify: InstallPlan = serde_json::from_str(include_str!(
            "../../../docs/schemas/examples/components-modify-plan.json"
        ))
        .unwrap();
        manifest.validate().unwrap();
        receipt.validate_manifest(&manifest).unwrap();
        plan.validate(&manifest).unwrap();
        modify
            .validate_modify(
                &manifest,
                &receipt,
                &hash_bytes(include_bytes!(
                    "../../../docs/schemas/examples/components-receipt.json"
                )),
            )
            .unwrap();
    }
}
