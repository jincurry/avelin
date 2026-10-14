//! Real view interactions on TestPlatform; no network, elevation, or desktop use.
use super::operation_view_tests::connected;
use super::{Operation, UiCommand, UiOptions, UiPhase};
use crate::{UiResponse, UiSnapshot};
use avelin_core::InstallScope;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, TestAppContext};
use std::{path::PathBuf, sync::mpsc::TryRecvError};

#[gpui_kit::test]
fn maintenance_update_check_download_and_handoff_are_separate_actions(cx: &mut TestAppContext) {
    let (fixture, commands, _snapshots, responses) = connected(
        cx,
        UiOptions {
            operation: Operation::Repair,
            allow_updates: true,
            ..UiOptions::default()
        },
    );
    fixture.update(cx, |view, _, _| view.accepted = true);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("check-updates", cx);
    })
    .unwrap();
    assert_eq!(commands.try_recv().unwrap(), UiCommand::CheckForUpdates);
    fixture.update(cx, |view, _, cx| {
        view.update_action(false, cx);
        view.start(cx);
        assert!(!view.start_enabled());
        assert!(view.may_close(cx));
    });
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    responses
        .send(UiResponse::UpdateStatus {
            available_version: Some("3.0.0".into()),
            detail: "New version available".into(),
            launched: false,
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| view.poll(cx));
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("download-update").label(),
            Some("下载并升级到 3.0.0")
        );
        window.click("download-update", cx);
    })
    .unwrap();
    assert_eq!(commands.try_recv().unwrap(), UiCommand::DownloadUpdate);
    responses
        .send(UiResponse::UpdateStatus {
            available_version: Some("3.0.0".into()),
            detail: "Installer opened".into(),
            launched: true,
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.phase(), UiPhase::Ready);
        assert!(!view.start_enabled());
        assert!(view.may_close(cx));
        view.start(cx);
        view.update_action(false, cx);
    });
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("check-updates").is_none());
        assert!(window.try_find("download-update").is_none());
        assert!(window.try_find("close").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn failed_update_check_can_retry_and_missing_config_hides_actions(cx: &mut TestAppContext) {
    let (fixture, commands, _snapshots, responses) = connected(
        cx,
        UiOptions {
            operation: Operation::Repair,
            allow_updates: true,
            ..UiOptions::default()
        },
    );
    fixture.update(cx, |view, _, cx| view.update_action(false, cx));
    assert_eq!(commands.try_recv().unwrap(), UiCommand::CheckForUpdates);
    responses
        .send(UiResponse::UpdateStatus {
            available_version: None,
            detail: "Network unavailable".into(),
            launched: false,
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert!(view.state.can_check_update());
        view.update_action(false, cx);
    });
    assert_eq!(commands.try_recv().unwrap(), UiCommand::CheckForUpdates);
    let (fixture, commands, _snapshots, _responses) = connected(
        cx,
        UiOptions {
            operation: Operation::Repair,
            ..UiOptions::default()
        },
    );
    fixture.update(cx, |view, _, cx| view.update_action(false, cx));
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    cx.update_window(fixture.window.into(), |_, window, _| {
        assert!(window.try_find("check-updates").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn all_users_directory_is_fixed_and_application_launch_is_suppressed(cx: &mut TestAppContext) {
    let target = PathBuf::from("C:\\Program Files\\Example");
    let (fixture, commands, snapshots, _responses) = connected(
        cx,
        UiOptions {
            install_scope: InstallScope::AllUsers,
            install_dir: target.clone(),
            ..UiOptions::default()
        },
    );
    cx.update_window(fixture.window.into(), |_, window, _| {
        assert!(window.try_find("browse").is_none());
    })
    .unwrap();
    fixture.update(cx, |view, window, cx| {
        assert!(view.fixed_directory());
        view.directory
            .update(cx, |input, cx| input.set_value("C:\\wrong", window, cx));
        view.browse(window, cx);
        assert!(!view.picker.pending());
        view.accepted = true;
        view.start(cx);
    });
    let UiCommand::Start { install_dir, .. } = commands.try_recv().unwrap() else {
        panic!("expected Start")
    };
    assert_eq!(install_dir, target);
    assert!(!cx.did_prompt_for_paths());
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("cancel").is_some(),
            "all-users install remains cancellable before commit"
        );
    })
    .unwrap();
    snapshots
        .send(UiSnapshot {
            phase: UiPhase::Succeeded,
            ..UiSnapshot::default()
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert!(!view.shows_launch());
        view.launch(cx);
    });
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("launch").is_none());
        assert!(window.try_find("close").is_some());
    })
    .unwrap();
}
