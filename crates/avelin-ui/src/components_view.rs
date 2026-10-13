//! Component choices remain presentation state until one Start command freezes them.

use super::*;
#[cfg(test)]
use gpui_kit::test::TestSupportExt;

fn size_text(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024. * 1024. * 1024.))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024. * 1024.))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.)
    } else {
        format!("{bytes} B")
    }
}

impl InstallerView {
    pub(super) fn modify(&self) -> bool {
        self.options.operation == Operation::Modify
    }

    pub(super) fn has_component_choices(&self) -> bool {
        self.options
            .components
            .iter()
            .any(|component| !component.required)
            && matches!(
                self.options.operation,
                Operation::Install | Operation::Upgrade | Operation::Modify
            )
    }

    pub(super) fn components_changed(&self) -> bool {
        self.options
            .components
            .iter()
            .any(|component| component.selected != component.installed)
    }

    pub(super) fn selected_components(&self) -> Option<Vec<String>> {
        if self.options.components.is_empty()
            || !matches!(
                self.options.operation,
                Operation::Install | Operation::Upgrade | Operation::Modify
            )
        {
            return None;
        }
        Some(
            self.options
                .components
                .iter()
                .filter(|component| component.required || component.selected)
                .map(|component| component.id.clone())
                .collect(),
        )
    }

    pub(super) fn start_enabled(&self) -> bool {
        self.picker.can_edit(self.phase())
            && self.state.can_start(self.accepted)
            && (!self.modify() || self.components_changed())
            && (!self.needs_migration_confirmation() || self.migration_confirmed)
    }

    fn needs_migration_confirmation(&self) -> bool {
        self.options.migration_confirmation_required && (self.upgrade() || self.modify())
    }

    pub(super) fn set_component(&mut self, id: &str, selected: bool, cx: &mut Context<Self>) {
        if !self.has_component_choices() || !self.picker.can_edit(self.phase()) {
            return;
        }
        let Some(component) = self
            .options
            .components
            .iter_mut()
            .find(|component| component.id == id)
        else {
            return;
        };
        if component.required || component.selected == selected {
            return;
        }
        component.selected = selected;
        self.accepted = false;
        self.migration_confirmed = false;
        self.state.clear_preflight_blocked();
        cx.notify();
    }

    pub(super) fn can_switch_maintenance(&self) -> bool {
        self.options.allow_modify
            && self
                .options
                .components
                .iter()
                .any(|component| !component.required)
            && (self.repair() || self.modify())
    }

    pub(super) fn switch_maintenance(
        &mut self,
        operation: Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_switch_maintenance()
            || !matches!(operation, Operation::Repair | Operation::Modify)
            || operation == self.options.operation
            || !self.state.set_operation(operation)
        {
            return;
        }
        self.options.operation = operation;
        self.accepted = false;
        self.migration_confirmed = false;
        self.status_note = None;
        window.set_window_title(&window_title(&self.options));
        cx.notify();
    }

    pub(super) fn component_size_summary(&self) -> String {
        let bytes = self
            .options
            .components
            .iter()
            .filter(|component| component.required || component.selected)
            .fold(0_u64, |total, component| {
                total.saturating_add(component.size)
            });
        format!(
            "{} {}",
            self.localized("所选组件文件", "Selected component files"),
            size_text(bytes)
        )
    }

    pub(super) fn render_components(&self, cx: &mut Context<Self>) -> AnyElement {
        let editable = self.picker.can_edit(self.phase());
        let muted = if self.options.dark {
            rgb(0xa7b5ae)
        } else {
            rgb(0x6b756f)
        };
        let border = if self.options.dark {
            rgb(0x35433b)
        } else {
            rgb(0xe3e7e3)
        };
        div()
            .id("components")
            .map(|container| {
                #[cfg(test)]
                let container = container.test_support();
                container
            })
            .flex().flex_col().gap_2()
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(self.localized("安装组件", "Components")))
            .child(
                div().flex().flex_col().border_1().border_color(border).rounded_md()
                    .children(self.options.components.iter().map(|component| {
                        let id = component.id.clone();
                        let name = self.localized(&component.name.0, &component.name.1);
                        let description = self.localized(&component.description.0, &component.description.1);
                        let status = if component.required {
                            self.localized("必选", "Required")
                        } else if (self.modify() || self.upgrade()) && component.selected && !component.installed {
                            self.localized("将添加", "Will add")
                        } else if (self.modify() || self.upgrade()) && !component.selected && component.installed {
                            self.localized("将移除", "Will remove")
                        } else if component.installed {
                            self.localized("已安装", "Installed")
                        } else if self.upgrade() {
                            self.localized("未安装", "Not installed")
                        } else {
                            self.localized("可选", "Optional")
                        };
                        let size = size_text(component.size);
                        Checkbox::new(SharedString::from(format!("component-{}", component.id)))
                            .w_full().p_2()
                            .checked(component.required || component.selected)
                            .disabled(!editable || component.required)
                            .accessibility_label(format!("{name}，{description}，{size}，{status}"))
                            .child(div().flex_1().min_w_0().flex().flex_col().gap_1()
                                .child(div().flex().items_center().justify_between().gap_2()
                                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(name))
                                    .child(div().flex_shrink_0().text_xs().text_color(muted).child(format!("{status} · {size}"))))
                                .child(div().text_xs().text_color(muted).whitespace_normal().child(description)))
                            // Stable IDs let the kit preserve focus on this checkbox across renders.
                            .on_change(cx.listener(move |view, checked, _, cx| view.set_component(&id, *checked, cx)))
                    })),
            )
            .child(div().text_xs().text_color(muted).whitespace_normal().child(format!(
                "{}；{} {}。{}",
                self.component_size_summary(),
                self.localized("另保留维护文件", "Additional maintenance files"),
                size_text(self.options.maintenance_bytes),
                self.localized("安装包仍包含全部组件；执行前会另行检查临时空间。", "The installer contains all components. Temporary space is checked before execution."),
            )))
            .into_any_element()
    }

    pub(super) fn render_maintenance_switch(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("maintenance-modes")
            .flex()
            .gap_2()
            .child(
                Button::new("maintenance-repair")
                    .label(self.localized("修复安装", "Repair installation"))
                    .disabled(self.repair() || self.phase() != UiPhase::Ready)
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.switch_maintenance(Operation::Repair, window, cx)
                    })),
            )
            .child(
                Button::new("maintenance-modify")
                    .label(self.localized("更改组件", "Change components"))
                    .disabled(self.modify() || self.phase() != UiPhase::Ready)
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.switch_maintenance(Operation::Modify, window, cx)
                    })),
            )
            .into_any_element()
    }

    pub(super) fn render_migration_confirmation(&self, cx: &mut Context<Self>) -> AnyElement {
        div().id("component-migration")
            .map(|container| {
                #[cfg(test)]
                let container = container.test_support();
                container
            })
            .flex().flex_col().gap_2()
            .children(self.options.migration_details.iter().map(|(zh, en)| div().text_sm().whitespace_normal().child(self.localized(zh, en))))
            .when(self.needs_migration_confirmation(), |container| container.child(Checkbox::new("migration-consent")
                .label(self.localized("我已查看旧版本文件与组件的对应关系，确认迁移", "I reviewed the mapping of existing files to components and confirm migration"))
                .checked(self.migration_confirmed)
                .disabled(self.phase() != UiPhase::Ready)
                .on_change(cx.listener(|view, checked, _, cx| {
                    if view.phase() == UiPhase::Ready {
                        view.migration_confirmed = *checked;
                        view.accepted = false;
                        view.state.clear_preflight_blocked();
                        cx.notify();
                    }
                }))))
            .into_any_element()
    }

    pub(super) fn render_modify_confirmation(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(px(24.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.title()),
            )
            .when(self.can_switch_maintenance(), |container| {
                container.child(self.render_maintenance_switch(cx))
            })
            .child(
                div()
                    .text_sm()
                    .whitespace_normal()
                    .child(self.description()),
            )
            .child(
                div()
                    .id("modify-directory")
                    .text_sm()
                    .whitespace_normal()
                    .aria_label(self.localized("固定安装目录", "Fixed installation directory"))
                    .child(self.options.install_dir.to_string_lossy().into_owned()),
            )
            .when(self.has_component_choices(), |container| {
                container.child(self.render_components(cx))
            })
            .when(self.needs_migration_confirmation(), |container| {
                container.child(self.render_migration_confirmation(cx))
            })
            .child(
                Checkbox::new("consent")
                    .label(
                        self.localized("我确认以上组件更改", "I confirm these component changes"),
                    )
                    .checked(self.accepted)
                    .disabled(self.phase() != UiPhase::Ready)
                    .on_change(cx.listener(|view, checked, _, cx| {
                        if view.phase() == UiPhase::Ready {
                            view.accepted = *checked;
                            cx.notify();
                        }
                    })),
            )
            .into_any_element()
    }
}
