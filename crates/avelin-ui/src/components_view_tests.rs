//! Headless tests exercise the real checkbox handlers and the single frozen command.

use super::operation_view_tests::connected;
use super::{Operation, UiCommand, UiOptions, UiPhase};
use crate::UiComponent;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, InputEvent, KeyUpEvent, Keystroke, TestAppContext};
use std::sync::mpsc::TryRecvError;

fn options(operation: Operation) -> UiOptions {
    UiOptions {
        operation,
        allow_modify: true,
        components: vec![
            UiComponent {
                id: "core".into(),
                name: ("主程序".into(), "Application".into()),
                description: ("运行必需".into(), "Required to run".into()),
                size: 1024,
                required: true,
                selected: true,
                installed: true,
            },
            UiComponent {
                id: "tools".into(),
                name: ("工具".into(), "Tools".into()),
                description: ("可选工具".into(), "Optional tools".into()),
                size: 2048,
                required: false,
                selected: false,
                installed: false,
            },
        ],
        maintenance_bytes: 4096,
        ..UiOptions::default()
    }
}

#[gpui_kit::test]
fn checkbox_selection_clears_confirmation_and_is_frozen_with_start(cx: &mut TestAppContext) {
    let (fixture, commands, _snapshots, _responses) = connected(cx, options(Operation::Install));
    fixture.update(cx, |view, _, _| view.accepted = true);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("component-tools", cx);
        window.render_frame(cx);
        // The kit deliberately retains the previous focus on pointer clicks.
        // Enter the checkbox through the real tab map before testing Space.
        for _ in 0..12 {
            if window.find("component-tools").focused() == Some(true) {
                break;
            }
            window.focus_next(cx);
            window.render_frame(cx);
        }
        assert_eq!(window.find("component-tools").focused(), Some(true));
        window.press("space", cx);
        window.dispatch_event(
            KeyUpEvent {
                keystroke: Keystroke::parse("space").unwrap(),
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        assert_eq!(window.find("component-tools").checked(), Some(false));
        window.press("space", cx);
        window.dispatch_event(
            KeyUpEvent {
                keystroke: Keystroke::parse("space").unwrap(),
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        assert_eq!(window.find("component-tools").checked(), Some(true));
        assert_eq!(window.find("component-tools").focused(), Some(true));
    })
    .unwrap();
    fixture.update(cx, |view, _, cx| {
        assert!(!view.accepted);
        assert_eq!(
            view.selected_components(),
            Some(vec!["core".into(), "tools".into()])
        );
        assert!(view.component_size_summary().contains("3.0 KiB"));
        view.set_component("core", false, cx);
        assert!(view.options.components[0].selected);
        view.start(cx);
        assert_eq!(view.phase(), UiPhase::Ready);
    });
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("consent", cx);
        window.click("start", cx);
    })
    .unwrap();
    assert!(matches!(commands.try_recv().unwrap(), UiCommand::Start {
        selected_components: Some(ids), operation: Some(Operation::Install), ..
    } if ids == ["core", "tools"]));
    fixture.update(cx, |view, window, cx| {
        view.set_component("tools", false, cx);
        view.switch_maintenance(Operation::Modify, window, cx);
        assert!(view.options.components[1].selected);
        assert_eq!(view.options.operation, Operation::Install);
    });
}

#[gpui_kit::test]
fn maintenance_switch_preserves_choices_and_modify_rejects_noop(cx: &mut TestAppContext) {
    let (fixture, commands, _snapshots, _responses) = connected(cx, options(Operation::Repair));
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(window.try_find("component-tools").is_none());
        window.click("maintenance-modify", cx);
        window.render_frame(cx);
        assert_eq!(window.find("start").label(), Some("无需更改"));
        assert!(window.try_find("desktop").is_none());
    })
    .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.accepted = true;
        view.start(cx);
        assert_eq!(view.phase(), UiPhase::Ready);
    });
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("component-tools", cx);
        window.click("maintenance-repair", cx);
        window.click("maintenance-modify", cx);
        window.click("language", cx);
    })
    .unwrap();
    fixture.update(cx, |view, _, cx| {
        assert!(!view.accepted);
        assert!(view.options.components[1].selected);
        assert_eq!(view.title(), "Change installed components");
        view.accepted = true;
        view.start(cx);
    });
    assert!(matches!(commands.try_recv().unwrap(), UiCommand::Start {
        operation: Some(Operation::Modify), selected_components: Some(ids),
        desktop_shortcut: false, start_menu_shortcut: false, ..
    } if ids == ["core", "tools"]));
}

#[gpui_kit::test]
fn migration_needs_separate_consent_and_a_new_choice_invalidates_it(cx: &mut TestAppContext) {
    let (fixture, commands, _snapshots, _responses) = connected(
        cx,
        UiOptions {
            migration_confirmation_required: true,
            migration_details: vec![(
                "旧文件已映射到工具组件".into(),
                "Existing files map to Tools".into(),
            )],
            ..options(Operation::Modify)
        },
    );
    fixture.update(cx, |view, _, cx| {
        view.set_component("tools", true, cx);
        view.accepted = true;
        view.start(cx);
        assert_eq!(view.phase(), UiPhase::Ready);
    });
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("migration-consent", cx);
        window.click("consent", cx);
    })
    .unwrap();
    fixture.update(cx, |view, _, cx| {
        assert!(view.start_enabled());
        view.set_component("tools", false, cx);
        assert!(!view.migration_confirmed);
        assert!(!view.accepted);
        view.set_component("tools", true, cx);
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("migration-consent", cx);
        window.click("consent", cx);
        window.click("start", cx);
    })
    .unwrap();
    assert!(matches!(
        commands.try_recv().unwrap(),
        UiCommand::Start {
            migration_confirmed: true,
            ..
        }
    ));
}

#[gpui_kit::test]
fn core_only_and_recovery_hide_component_controls(cx: &mut TestAppContext) {
    for operation in [Operation::Install, Operation::Repair, Operation::Recover] {
        let mut opts = options(operation);
        if operation != Operation::Recover {
            opts.components.truncate(1);
        }
        let (fixture, _commands, _snapshots, _responses) = connected(cx, opts);
        cx.update_window(fixture.window.into(), |_, window, _| {
            assert!(window.try_find("components").is_none());
            assert!(window.try_find("maintenance-modify").is_none());
        })
        .unwrap();
        fixture.update(cx, |view, _, cx| {
            view.set_component("tools", true, cx);
            assert!(!view.has_component_choices());
        });
    }
}

#[gpui_kit::test]
fn upgrade_explains_removed_components_without_extra_migration_consent(cx: &mut TestAppContext) {
    let (fixture, _commands, _snapshots, _responses) = connected(
        cx,
        UiOptions {
            previous_version: Some("1.0.0".into()),
            app_version: "2.0.0".into(),
            migration_details: vec![(
                "原组件已从新包移除".into(),
                "An installed component was removed from this package".into(),
            )],
            ..options(Operation::Upgrade)
        },
    );
    cx.update_window(fixture.window.into(), |_, window, _| {
        assert!(window.try_find("component-migration").is_some());
        assert!(window.try_find("migration-consent").is_none());
    })
    .unwrap();
    fixture.update(cx, |view, _, _| {
        view.accepted = true;
        assert!(view.start_enabled());
    });
}
