//! Update discovery and handoff are separate from the installation task state.
use super::*;

impl InstallerView {
    pub(super) fn shows_updates(&self) -> bool {
        self.options.allow_updates
            && !self.demo()
            && (self.repair() || self.modify())
            && self.phase() == UiPhase::Ready
    }

    pub(super) fn update_action(&mut self, download: bool, cx: &mut Context<Self>) {
        if !self.shows_updates() || !self.state.request_update(download) {
            return;
        }
        let command = if download {
            UiCommand::DownloadUpdate
        } else {
            UiCommand::CheckForUpdates
        };
        if self
            .bridge
            .as_ref()
            .is_none_or(|bridge| bridge.commands.send(command).is_err())
        {
            self.state.close_responses();
        }
        cx.notify();
    }

    pub(super) fn render_updates(&self, cx: &mut Context<Self>) -> AnyElement {
        let pending = self.state.update_pending();
        let title = if self.state.update_launched() {
            self.localized("升级安装器已打开", "Update installer opened")
        } else if pending == Some(UpdateRequest::Download) {
            self.localized(
                "正在下载并验证安装包…",
                "Downloading and verifying the installer…",
            )
        } else if pending == Some(UpdateRequest::Check) {
            self.localized("正在检查更新…", "Checking for updates…")
        } else {
            self.localized("软件更新", "Software updates")
        };
        div()
            .id("software-updates")
            .flex()
            .flex_col()
            .gap_2()
            .mb_5()
            .p_4()
            .border_1()
            .border_color(rgb(if self.options.dark {
                0x35433b
            } else {
                0xe3e7e3
            }))
            .rounded_md()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .when_some(self.state.update_detail(), |container, detail| {
                container.child(
                    div()
                        .id("update-detail")
                        .role(Role::Status)
                        .aria_label(detail.to_owned())
                        .map(|container| {
                            #[cfg(test)]
                            let container = container.test_support();
                            container
                        })
                        .text_sm()
                        .whitespace_normal()
                        .child(detail.to_owned()),
                )
            })
            .when(self.state.update_launched(), |container| {
                container.child(div().text_sm().child(self.localized(
                    "请在新窗口中确认升级。此维护窗口可以关闭。",
                    "Confirm the upgrade in the new window. You can close this maintenance window.",
                )))
            })
            .when(!self.state.update_launched(), |container| {
                container.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new("check-updates")
                                .label(self.localized("检查更新", "Check for updates"))
                                .disabled(!self.state.can_check_update())
                                .on_click(
                                    cx.listener(|view, _, _, cx| view.update_action(false, cx)),
                                ),
                        )
                        .when_some(self.state.update_version(), |row, version| {
                            row.child(
                                Button::new("download-update")
                                    .label(self.localized(
                                        &format!("下载并升级到 {version}"),
                                        &format!("Download and upgrade to {version}"),
                                    ))
                                    .disabled(!self.state.can_download_update())
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.update_action(true, cx)),
                                    ),
                            )
                        }),
                )
            })
            .into_any_element()
    }
}
