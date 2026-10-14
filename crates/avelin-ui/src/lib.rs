//! Compact native UI. `run` is an explicitly labelled, file-free demonstration.
//! `run_connected` displays coordinator snapshots without inventing installation results.
//! Hosts must parse help, version and silent-mode arguments BEFORE calling either entry point.

mod clipboard;
mod interaction;
mod state;
mod view;

use std::fmt;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

pub use avelin_core::presentation::{Locale, Presentation};
pub use avelin_core::protocol::{
    TaskCommand as UiCommand, TaskPhase as UiPhase, TaskResponse as UiResponse,
    TaskSnapshot as UiSnapshot,
};
pub use view::{run, run_connected, run_smoke};

/// A host-validated component. Text pairs are Chinese and English respectively.
/// `installed` is the receipt baseline, not a prediction about a running task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiComponent {
    pub id: String,
    pub name: (String, String),
    pub description: (String, String),
    pub size: u64,
    pub required: bool,
    pub selected: bool,
    pub installed: bool,
}

#[derive(Clone, Debug)]
pub struct UiOptions {
    pub app_name: String,
    pub app_version: String,
    pub install_dir: PathBuf,
    pub operation: avelin_core::Operation,
    /// Validated package scope. Machine installations have a fixed directory.
    pub install_scope: avelin_core::InstallScope,
    /// The host verified an installed product with a configured update feed.
    pub allow_updates: bool,
    /// Verified installed version for an in-place upgrade; supplied by the host.
    pub previous_version: Option<String>,
    /// Read-only maintenance findings checked by the host before confirmation.
    /// These describe missing/changed resources or the interrupted transaction.
    pub maintenance_details: Vec<(String, String)>,
    /// Explicit original-package restoration mode; changed resources are backed up first.
    pub repair_overwrite: bool,
    /// Empty for packages without component metadata. Required-only lists are hidden.
    pub components: Vec<UiComponent>,
    /// Package/runtime bytes retained for maintenance, separate from selected app files.
    pub maintenance_bytes: u64,
    /// Host verified that this same-version original package can change components.
    pub allow_modify: bool,
    pub migration_details: Vec<(String, String)>,
    pub migration_confirmation_required: bool,
    pub desktop_shortcut: bool,
    pub start_menu_shortcut: bool,
    /// Validated, package-contained branding and text. The UI never reads config paths or URLs.
    pub presentation: Presentation,
    /// Effective runtime choice, resolved by the host from presentation and command-line options.
    pub locale: Locale,
    /// Effective runtime choice; the user can toggle this without changing the packaged config.
    pub dark: bool,
}

impl Default for UiOptions {
    fn default() -> Self {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\AvelinDemo"));
        Self {
            app_name: "Nimbus".into(),
            app_version: "2.4.0".into(),
            install_dir: base.join("AvelinDemo").join("Nimbus"),
            operation: avelin_core::Operation::Install,
            install_scope: avelin_core::InstallScope::CurrentUser,
            allow_updates: false,
            previous_version: None,
            maintenance_details: Vec::new(),
            repair_overwrite: false,
            components: Vec::new(),
            maintenance_bytes: 0,
            allow_modify: false,
            migration_details: Vec::new(),
            migration_confirmation_required: false,
            desktop_shortcut: true,
            start_menu_shortcut: true,
            presentation: Presentation::default(),
            locale: Locale::Chinese,
            dark: false,
        }
    }
}

pub struct UiBridge {
    pub commands: Sender<UiCommand>,
    pub snapshots: Receiver<UiSnapshot>,
    /// Post-install action results; never overwrite a confirmed installation result.
    pub responses: Receiver<UiResponse>,
}

#[derive(Debug)]
pub struct UiError(pub String);

impl fmt::Display for UiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AVELIN_UI_INIT: {}", self.0)
    }
}

impl std::error::Error for UiError {}
