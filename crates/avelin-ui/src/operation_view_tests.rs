//! M2 interaction tests use TestPlatform only, without shell links or native apps.

use super::clipboard_view_tests::Fixture;
use super::{
    LaunchStatus, Locale, Operation, UiBridge, UiCommand, UiNotice, UiOptions, UiPhase, UiState,
};
use crate::{UiResponse, UiSnapshot};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, TestAppContext};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

pub(super) fn connected(
    cx: &mut TestAppContext,
    options: UiOptions,
) -> (
    Fixture,
    Receiver<UiCommand>,
    Sender<UiSnapshot>,
    Sender<UiResponse>,
) {
    let fixture = Fixture::with_options(cx, options);
    let (commands, command_rx) = mpsc::channel();
    let (snapshot_tx, snapshots) = mpsc::channel();
    let (response_tx, responses) = mpsc::channel();
    fixture.update(cx, |view, _, cx| {
        view.state = UiState::for_operation(true, view.options.operation);
        view.bridge = Some(UiBridge {
            commands,
            snapshots,
            responses,
        });
        cx.notify();
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx)
    })
    .unwrap();
    (fixture, command_rx, snapshot_tx, response_tx)
}

#[gpui_kit::test]
fn connected_install_submits_the_actual_shortcut_choices(cx: &mut TestAppContext) {
    let (fixture, commands, _snapshots, _responses) = connected(
        cx,
        UiOptions {
            install_dir: PathBuf::from("C:\\Chosen App"),
            desktop_shortcut: false,
            start_menu_shortcut: true,
            ..UiOptions::default()
        },
    );
    fixture.update(cx, |view, _, _| {
        assert!(!view.desktop);
        assert!(view.start_menu);
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(window.try_find("desktop").is_some());
        assert!(window.try_find("start-menu").is_some());
        window.click("desktop", cx);
        window.click("start-menu", cx);
        window.click("consent", cx);
        window.click("start", cx);
    })
    .unwrap();
    assert_eq!(
        commands.try_recv().unwrap(),
        UiCommand::Start {
            install_dir: PathBuf::from("C:\\Chosen App"),
            desktop_shortcut: true,
            start_menu_shortcut: false,
            selected_components: None,
            operation: Some(Operation::Install),
            migration_confirmed: false,
        }
    );
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
}

#[gpui_kit::test]
fn uninstall_uses_fixed_path_and_never_sends_cancel(cx: &mut TestAppContext) {
    let (fixture, commands, snapshots, _responses) = connected(
        cx,
        UiOptions {
            operation: Operation::Uninstall,
            install_dir: PathBuf::from("C:\\Installed App"),
            ..UiOptions::default()
        },
    );
    cx.update_window(fixture.window.into(), |_, window, _| {
        for id in ["browse", "desktop", "start-menu", "launch"] {
            assert!(window.try_find(id).is_none(), "uninstall hides {id}");
        }
        assert_eq!(window.find("start").label(), Some("卸载"));
    })
    .unwrap();
    fixture.update(cx, |view, window, cx| {
        assert!(view.may_close(cx));
        // Even a stale hidden input entity cannot redirect controlled uninstall.
        view.directory
            .update(cx, |input, cx| input.set_value("C:\\wrong", window, cx));
        view.browse(window, cx);
        assert!(!view.picker.pending());
        view.accepted = true;
        view.start(cx);
        view.cancel(cx);
        assert!(!view.may_close(cx));
        assert_eq!(view.status_note, Some(UiNotice::UninstallActive));
    });
    assert!(!cx.did_prompt_for_paths());
    assert_eq!(
        commands.try_recv().unwrap(),
        UiCommand::Start {
            install_dir: PathBuf::from("C:\\Installed App"),
            desktop_shortcut: false,
            start_menu_shortcut: false,
            selected_components: None,
            operation: Some(Operation::Uninstall),
            migration_confirmed: false,
        }
    );
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    snapshots
        .send(UiSnapshot {
            phase: UiPhase::Installing,
            ..UiSnapshot::default()
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.title(), "正在卸载");
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("cancel").is_none());
        assert!(window.try_find("close").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn uninstall_terminal_text_is_localized_without_a_launch_action(cx: &mut TestAppContext) {
    for (phase, chinese, english) in [
        (UiPhase::Succeeded, "已卸载", "Uninstalled"),
        (UiPhase::Failed, "未能完成卸载", "Could not uninstall"),
    ] {
        let (fixture, commands, snapshots, _responses) = connected(
            cx,
            UiOptions {
                operation: Operation::Uninstall,
                ..UiOptions::default()
            },
        );
        snapshots
            .send(UiSnapshot {
                phase,
                detail: "receipt check: {raw}".into(),
                ..UiSnapshot::default()
            })
            .unwrap();
        fixture.update(cx, |view, _, cx| {
            view.poll(cx);
            assert_eq!(view.title(), chinese);
            assert!(view.may_close(cx));
            view.launch(cx);
        });
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("launch").is_none());
            assert!(window.try_find("close").is_some());
            window.click("language", cx);
        })
        .unwrap();
        fixture.update(cx, |view, _, _| {
            assert_eq!(view.title(), english);
            assert_eq!(view.detail(), "receipt check: {raw}");
        });
        assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    }
}

#[gpui_kit::test]
fn launch_is_explicit_and_failure_retry_does_not_replace_install_success(cx: &mut TestAppContext) {
    let (fixture, commands, snapshots, responses) = connected(
        cx,
        UiOptions {
            app_name: "Cedar".into(),
            ..UiOptions::default()
        },
    );
    snapshots
        .send(UiSnapshot {
            phase: UiPhase::Succeeded,
            detail: "receipt verified".into(),
            ..UiSnapshot::default()
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| view.poll(cx));
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("launch").label(), Some("启动 Cedar"));
        window.click("launch", cx);
        window.click("launch", cx);
    })
    .unwrap();
    assert_eq!(commands.try_recv().unwrap(), UiCommand::LaunchInstalled);
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    responses
        .send(UiResponse::LaunchResult {
            succeeded: false,
            detail: "CreateProcess: Win32=5".into(),
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.phase(), UiPhase::Succeeded);
        assert_eq!(view.detail(), "receipt verified");
        assert_eq!(
            view.state.launch_status(),
            &LaunchStatus::Failed("CreateProcess: Win32=5".into())
        );
        assert!(view.state.can_launch());
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("language", cx);
        window.click("launch", cx);
    })
    .unwrap();
    assert_eq!(commands.try_recv().unwrap(), UiCommand::LaunchInstalled);
    responses
        .send(UiResponse::LaunchResult {
            succeeded: true,
            detail: "pid=1234".into(),
        })
        .unwrap();
    drop(snapshots);
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.options.locale, Locale::English);
        assert_eq!(view.launch_label(), "Launched");
        assert_eq!(view.state.launch_status(), &LaunchStatus::Succeeded);
        assert!(!view.state.can_launch());
        assert_eq!(view.phase(), UiPhase::Succeeded);
        assert_eq!(view.detail(), "receipt verified");
    });
}

#[gpui_kit::test]
fn response_disconnect_and_demo_cannot_invent_a_launch_result(cx: &mut TestAppContext) {
    let (fixture, commands, snapshots, responses) = connected(cx, UiOptions::default());
    snapshots
        .send(UiSnapshot {
            phase: UiPhase::Succeeded,
            ..UiSnapshot::default()
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        view.launch(cx);
    });
    assert_eq!(commands.try_recv().unwrap(), UiCommand::LaunchInstalled);
    drop(responses);
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.state.launch_status(), &LaunchStatus::Unavailable);
        assert_eq!(view.phase(), UiPhase::Succeeded);
        assert!(!view.state.can_launch());
    });
    let demo = Fixture::with_options(cx, UiOptions::default());
    demo.update(cx, |view, _, cx| {
        view.accepted = true;
        view.start(cx);
        for _ in 0..17 {
            view.state.advance_demo();
        }
        assert_eq!(view.phase(), UiPhase::Succeeded);
        assert!(!view.shows_launch());
        view.launch(cx);
        assert_eq!(view.state.launch_status(), &LaunchStatus::Idle);
    });
}

#[gpui_kit::test]
fn upgrade_confirmation_keeps_the_installed_path_and_actual_choices(cx: &mut TestAppContext) {
    let (fixture, commands, snapshots, responses) = connected(
        cx,
        UiOptions {
            app_name: "Cedar".into(),
            app_version: "0.4.0".into(),
            previous_version: Some("0.3.0".into()),
            operation: Operation::Upgrade,
            install_dir: PathBuf::from("C:\\Installed Cedar"),
            desktop_shortcut: false,
            start_menu_shortcut: true,
            ..UiOptions::default()
        },
    );
    fixture.update(cx, |view, window, cx| {
        assert_eq!(view.upgrade_versions(), "版本 0.3.0 → 0.4.0");
        assert_eq!(super::window_title(&view.options), "Cedar 升级");
        assert!(view.may_close(cx));
        view.directory
            .update(cx, |input, cx| input.set_value("C:\\wrong", window, cx));
        view.browse(window, cx);
        assert!(!view.picker.pending());
        view.start(cx); // Consent has not been checked.
        assert_eq!(view.phase(), UiPhase::Ready);
    });
    assert!(!cx.did_prompt_for_paths());
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(window.try_find("browse").is_none());
        assert_eq!(window.find("start").label(), Some("升级"));
        window.click("language", cx);
        window.click("desktop", cx);
        window.click("start-menu", cx);
        window.click("consent", cx);
        window.click("start", cx);
    })
    .unwrap();
    assert_eq!(
        commands.try_recv().unwrap(),
        UiCommand::Start {
            install_dir: PathBuf::from("C:\\Installed Cedar"),
            desktop_shortcut: true,
            start_menu_shortcut: false,
            selected_components: None,
            operation: Some(Operation::Upgrade),
            migration_confirmed: false,
        }
    );
    fixture.update(cx, |view, _, cx| {
        assert_eq!(view.upgrade_versions(), "Version 0.3.0 → 0.4.0");
        assert_eq!(view.options.locale, Locale::English);
        view.cancel(cx);
        assert!(!view.may_close(cx));
        assert_eq!(view.status_note, Some(UiNotice::UpgradeActive));
    });
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    snapshots
        .send(UiSnapshot {
            phase: UiPhase::Installing,
            completed: 42,
            total: Some(100),
            detail: String::new(),
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.title(), "Upgrading");
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("cancel").is_none());
        assert!(window.try_find("close").is_none());
    })
    .unwrap();
    snapshots
        .send(UiSnapshot {
            phase: UiPhase::Succeeded,
            detail: "new receipt verified".into(),
            ..UiSnapshot::default()
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.title(), "Upgraded");
    });
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty)); // Success never auto-launches.
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("launch").label(), Some("Launch Cedar"));
        window.click("launch", cx);
    })
    .unwrap();
    assert_eq!(commands.try_recv().unwrap(), UiCommand::LaunchInstalled);
    responses
        .send(UiResponse::LaunchResult {
            succeeded: false,
            detail: "entry changed".into(),
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.phase(), UiPhase::Succeeded);
        assert_eq!(view.detail(), "new receipt verified");
        assert_eq!(
            view.state.launch_status(),
            &LaunchStatus::Failed("entry changed".into())
        );
    });
}

#[gpui_kit::test]
fn upgrade_failure_keeps_localization_and_raw_diagnostics_without_launch(cx: &mut TestAppContext) {
    let (fixture, commands, snapshots, _responses) = connected(
        cx,
        UiOptions {
            operation: Operation::Upgrade,
            previous_version: Some("0.3.0".into()),
            app_version: "0.4.0".into(),
            ..UiOptions::default()
        },
    );
    snapshots
        .send(UiSnapshot {
            phase: UiPhase::Failed,
            detail: "upgrade verification: {raw}".into(),
            ..UiSnapshot::default()
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.title(), "未能完成升级");
        assert!(view.may_close(cx));
        view.launch(cx);
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("launch").is_none());
        assert!(window.try_find("close").is_some());
        window.click("language", cx);
    })
    .unwrap();
    fixture.update(cx, |view, _, _| {
        assert_eq!(view.title(), "Could not upgrade");
        assert_eq!(view.detail(), "upgrade verification: {raw}");
    });
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn invalid_upgrade_versions_fail_before_native_initialization() {
    for previous in [None, Some("invalid"), Some("0.4.0+old"), Some("0.5.0")] {
        let result = super::run(UiOptions {
            operation: Operation::Upgrade,
            previous_version: previous.map(str::to_owned),
            app_version: "0.4.0+new".into(),
            ..UiOptions::default()
        });
        assert!(result.is_err());
    }
}

#[gpui_kit::test]
fn maintenance_requires_consent_keeps_the_path_and_hides_install_choices(cx: &mut TestAppContext) {
    for operation in [Operation::Repair, Operation::Recover] {
        let (fixture, commands, snapshots, _responses) = connected(
            cx,
            UiOptions {
                operation,
                install_dir: PathBuf::from("C:\\Existing Product"),
                maintenance_details: vec![("缺失：app.exe".into(), "Missing: app.exe".into())],
                ..UiOptions::default()
            },
        );
        fixture.update(cx, |view, window, cx| {
            view.directory
                .update(cx, |input, cx| input.set_value("C:\\Wrong", window, cx));
            view.browse(window, cx);
            view.start(cx);
            assert_eq!(view.phase(), UiPhase::Ready);
            assert!(!view.picker.pending());
            assert!(view.fixed_directory());
            if operation == Operation::Repair {
                assert!(view.repair_policy().contains("保留"));
            }
        });
        assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
        assert!(!cx.did_prompt_for_paths());
        cx.update_window(fixture.window.into(), |_, window, cx| {
            for id in ["browse", "desktop", "start-menu", "launch"] {
                assert!(window.try_find(id).is_none(), "maintenance hides {id}");
            }
            assert_eq!(
                window.find("start").label(),
                Some(if operation == Operation::Repair {
                    "修复"
                } else {
                    "恢复"
                })
            );
            window.click("consent", cx);
            window.click("start", cx);
        })
        .unwrap();
        assert_eq!(
            commands.try_recv().unwrap(),
            UiCommand::Start {
                install_dir: PathBuf::from("C:\\Existing Product"),
                desktop_shortcut: false,
                start_menu_shortcut: false,
                selected_components: None,
                operation: Some(operation),
                migration_confirmed: false,
            }
        );
        fixture.update(cx, |view, _, cx| {
            view.cancel(cx);
            assert!(!view.may_close(cx));
        });
        assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
        snapshots
            .send(UiSnapshot {
                phase: UiPhase::Succeeded,
                detail: "保留 1 项已修改资源；此项未修复。".into(),
                ..UiSnapshot::default()
            })
            .unwrap();
        fixture.update(cx, |view, _, cx| {
            view.poll(cx);
            assert!(view.may_close(cx));
            assert_eq!(view.shows_launch(), operation == Operation::Repair);
            assert_eq!(view.detail(), "保留 1 项已修改资源；此项未修复。");
            assert_eq!(
                view.title(),
                if operation == Operation::Repair {
                    "修复流程完成"
                } else {
                    "恢复完成"
                }
            );
        });
    }
}

#[gpui_kit::test]
fn repair_mode_and_recovery_titles_follow_language_switch(cx: &mut TestAppContext) {
    for operation in [Operation::Repair, Operation::Recover] {
        let (fixture, _commands, _snapshots, _responses) = connected(
            cx,
            UiOptions {
                operation,
                repair_overwrite: operation == Operation::Repair,
                maintenance_details: vec![("已修改：app.exe".into(), "Changed: app.exe".into())],
                ..UiOptions::default()
            },
        );
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.click("language", cx);
        })
        .unwrap();
        fixture.update(cx, |view, _, _| {
            assert_eq!(
                view.title(),
                if operation == Operation::Repair {
                    "Inspect and repair"
                } else {
                    "Interrupted operation found"
                }
            );
            assert!(super::window_title(&view.options).contains(
                if operation == Operation::Repair {
                    "Repair"
                } else {
                    "Recover"
                }
            ));
            assert_eq!(
                view.options.repair_overwrite,
                operation == Operation::Repair
            );
            if operation == Operation::Repair {
                assert!(
                    view.repair_policy()
                        .contains("backed up before replacement")
                );
            }
        });
    }
}
