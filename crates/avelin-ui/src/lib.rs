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

#[derive(Clone, Debug)]
pub struct UiOptions {
    pub app_name: String,
    pub app_version: String,
    pub install_dir: PathBuf,
    pub operation: avelin_core::Operation,
    /// Verified installed version for an in-place upgrade; supplied by the host.
    pub previous_version: Option<String>,
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
            previous_version: None,
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
