//! User-requested handoff to a verified full Setup's existing upgrade page.
use anyhow::{Context, Result, ensure};
use avelin_core::{
    InstallScope, Manifest,
    updates::{DownloadedUpdate, UpdateRelease},
};
use std::{
    fmt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureStage {
    Check,
    Download,
    Launch,
}

impl FailureStage {
    fn code(self) -> &'static str {
        match self {
            Self::Check => "AVELIN_UPDATE_CHECK_FAILED",
            Self::Download => "AVELIN_UPDATE_DOWNLOAD_FAILED",
            Self::Launch => "AVELIN_UPDATE_LAUNCH_FAILED",
        }
    }

    fn message(self, english: bool) -> &'static str {
        match (self, english) {
            (Self::Check, false) => "暂时无法检查更新，请检查网络连接后重试。",
            (Self::Check, true) => {
                "Could not check for updates. Check your connection and try again."
            }
            (Self::Download, false) => {
                "更新包下载或校验未完成，请检查网络连接和可用磁盘空间后重试。"
            }
            (Self::Download, true) => {
                "Could not download or verify the update. Check your connection and free disk space, then try again."
            }
            (Self::Launch, false) => {
                "无法打开升级窗口，请重试。如仍失败，请使用应用发布者提供的安装包。"
            }
            (Self::Launch, true) => {
                "Could not open the upgrade window. Try again, or use the installer provided by the application publisher."
            }
        }
    }
}

impl fmt::Display for FailureStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

// Feed/download URLs are unnecessary for diagnosing the local failure. Omit
// complete URL tokens, including user info and query strings, even if an error
// from a future transport includes them. Keep the OS error and context text.
fn diagnostic_detail(error: &anyhow::Error) -> String {
    let detail = format!("{error:#}");
    let mut redacted = String::with_capacity(detail.len());
    for part in detail.split_inclusive(char::is_whitespace) {
        if part.contains("://") {
            redacted.push_str("[URL omitted]");
            let whitespace = part.trim_end_matches(char::is_whitespace).len();
            redacted.push_str(&part[whitespace..]);
        } else {
            redacted.push_str(part);
        }
    }
    redacted
}

/// Send technical context to the existing bounded log, never to the main flow.
/// The caller retains its release/selection state so the action can be retried.
pub fn failure_message(error: &anyhow::Error, fallback: FailureStage, english: bool) -> String {
    let stage = error
        .downcast_ref::<FailureStage>()
        .copied()
        .unwrap_or(fallback);
    log::error!(target: "avelin_app::online_update", "{}: {}", stage.code(), diagnostic_detail(error));
    crate::runtime_log::flush();
    stage.message(english).into()
}

fn validate_handoff(previous: &Manifest, downloaded: &Manifest) -> Result<()> {
    downloaded.validate()?;
    ensure!(
        previous.app_id == downloaded.app_id,
        "update belongs to a different application"
    );
    avelin_core::model::ensure_newer_version(&previous.app_version, &downloaded.app_version)?;
    ensure!(
        previous.install_scope == downloaded.install_scope,
        "an online update cannot change installation scope"
    );
    ensure!(
        previous.updates == downloaded.updates,
        "an online update cannot change its trusted feed or channel; review and install that package manually"
    );
    Ok(())
}

pub struct UpdateHandoff {
    child: std::process::Child,
    _download: DownloadedUpdate,
    _directory: crate::execution_files::PinnedDirectory,
}
impl UpdateHandoff {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
    pub fn wait(mut self) -> i32 {
        self.child
            .wait()
            .ok()
            .and_then(|status| status.code())
            .unwrap_or(4)
    }
}

pub fn download_and_launch(
    manifest: &Manifest,
    target: &Path,
    release: &UpdateRelease,
    english: bool,
    shutdown: &AtomicBool,
) -> Result<UpdateHandoff> {
    ensure!(
        manifest.updates.is_some(),
        "this application has no configured update feed"
    );
    let base = if manifest.install_scope == InstallScope::AllUsers {
        avelin_core::scope::ensure_machine_state()?.join("sessions")
    } else {
        std::env::temp_dir().join("AvelinM0")
    };
    let directory = crate::execution_files::create_private_session(&base, "update")?;
    if manifest.install_scope == InstallScope::AllUsers {
        avelin_core::scope::protect_machine_session(directory.path())?;
    }
    let download: DownloadedUpdate = avelin_core::updates::download_cancel(
        release,
        &directory.path().join("Setup.exe"),
        shutdown,
    )?;
    (|| -> Result<UpdateHandoff> {
        validate_handoff(manifest, &download.manifest)?;
        // Revalidate installed identity after network I/O, before a new process exists.
        crate::resources::read_receipt(manifest, target, &crate::transport::current_user_sid()?)?;
        crate::upgrade::ensure_no_pending(target)?;
        ensure!(
            !shutdown.load(Ordering::Acquire),
            "update window was closed"
        );
        let mut command = std::process::Command::new(&download.path);
        command
            .args(["--gui", "--operation", "upgrade", "--install-dir"])
            .arg(target)
            .current_dir(directory.path());
        if english {
            command.arg("--english");
        }
        use std::os::windows::process::CommandExt;
        let child = command.creation_flags(0x08000000).spawn()?;
        // Pin the verified executable through process lifetime. Closing the old
        // confirmation window never terminates the new Setup or claims its result.
        Ok(UpdateHandoff {
            child,
            _download: download,
            _directory: directory,
        })
    })()
    .context(FailureStage::Launch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_errors_show_localized_retry_guidance_without_internal_details() {
        let error = anyhow::anyhow!("send HTTPS request: OS Error 12029 (FormatMessageW 317)");
        for stage in [
            FailureStage::Check,
            FailureStage::Download,
            FailureStage::Launch,
        ] {
            for english in [false, true] {
                let message = failure_message(&error, stage, english);
                assert_eq!(message, stage.message(english));
                assert!(!message.contains("AVELIN_") && !message.contains("FormatMessageW"));
                assert!(!message.contains("12029"));
                assert!(message.contains(if english { "ry again" } else { "重试" }));
            }
        }
        let launch = error.context(FailureStage::Launch);
        assert_eq!(
            failure_message(&launch, FailureStage::Download, false),
            FailureStage::Launch.message(false)
        );
    }

    #[test]
    fn update_diagnostics_keep_error_context_without_url_credentials_or_query_tokens() {
        let error = anyhow::anyhow!(
            "send HTTPS request https://user:secret@example.com/update?token=private: OS Error 12029"
        ).context("retry HTTPS://other:password@example.com/file failed");
        let detail = diagnostic_detail(&error);
        assert!(detail.contains("send HTTPS request") && detail.contains("OS Error 12029"));
        assert!(!detail.contains("secret") && !detail.contains("private"));
        assert!(!detail.contains("password") && !detail.contains("example.com"));
        assert_eq!(detail.matches("[URL omitted]").count(), 2);
    }

    #[test]
    fn downloaded_update_must_preserve_scope_feed_and_product() {
        let mut original: Manifest = serde_json::from_str(include_str!(
            "../../../docs/schemas/examples/components-manifest.json"
        ))
        .unwrap();
        original.updates = Some(avelin_core::updates::UpdateConfig {
            feed_url: "https://example.com/stable.json".into(),
            channel: "stable".into(),
        });
        let mut next = original.clone();
        next.app_version = "9.0.0".into();
        assert!(validate_handoff(&original, &next).is_ok());
        next.install_scope = InstallScope::AllUsers;
        assert!(validate_handoff(&original, &next).is_err());
        next = original.clone();
        next.app_version = "9.0.0".into();
        next.updates = None;
        assert!(validate_handoff(&original, &next).is_err());
        next = original.clone();
        assert!(validate_handoff(&original, &next).is_err());
        next.app_version = "9.0.0".into();
        next.app_id = "other.app".into();
        assert!(validate_handoff(&original, &next).is_err());
    }
}
