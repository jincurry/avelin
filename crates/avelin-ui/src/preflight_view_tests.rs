//! The real view retries only a host-rejected preflight, never an executed task.

use super::operation_view_tests::connected;
use super::{Locale, Operation, UiOptions, UiPhase};
use crate::{UiComponent, UiResponse, UiSnapshot};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, TestAppContext};
use std::sync::mpsc::TryRecvError;

fn blocked() -> UiResponse {
    UiResponse::PreflightBlocked {
        detail: "Example (PID 42)\napp.exe".into(),
    }
}

#[gpui_kit::test]
fn blocked_preflight_preserves_choices_and_retries_exactly_once(cx: &mut TestAppContext) {
    let (fixture, commands, _snapshots, responses) = connected(
        cx,
        UiOptions {
            operation: Operation::Uninstall,
            ..UiOptions::default()
        },
    );
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("consent", cx);
        window.click("start", cx);
    })
    .unwrap();
    let original = commands.try_recv().unwrap();
    responses.send(blocked()).unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.phase(), UiPhase::Ready);
        assert!(view.accepted);
        assert!(view.may_close(cx));
        assert!(view.start_enabled());
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("preflight-blocked").is_some());
        assert_eq!(window.find("start").label(), Some("重新检查并继续"));
        window.click("language", cx);
        window.render_frame(cx);
        assert_eq!(window.find("start").label(), Some("Recheck and continue"));
        window.click("start", cx);
    })
    .unwrap();
    fixture.update(cx, |view, _, cx| {
        assert_eq!(view.options.locale, Locale::English);
        view.start(cx);
        assert_eq!(view.phase(), UiPhase::Preparing);
        assert!(view.state.preflight_blocked().is_none());
        assert!(!view.may_close(cx));
    });
    assert_eq!(commands.try_recv().unwrap(), original);
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
}

#[gpui_kit::test]
fn editing_a_blocked_install_discards_stale_usage_and_requires_confirmation(
    cx: &mut TestAppContext,
) {
    let (fixture, commands, _snapshots, responses) = connected(cx, UiOptions::default());
    fixture.update(cx, |view, _, cx| {
        view.accepted = true;
        view.start(cx);
    });
    commands.try_recv().unwrap();
    responses.send(blocked()).unwrap();
    fixture.update(cx, |view, _, cx| view.poll(cx));
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("desktop", cx);
    })
    .unwrap();
    fixture.update(cx, |view, _, cx| {
        assert!(view.state.preflight_blocked().is_none());
        assert!(!view.accepted);
        view.start(cx);
        assert_eq!(view.phase(), UiPhase::Ready);
        view.accepted = true;
        view.start(cx);
    });
    commands.try_recv().unwrap();
    responses.send(blocked()).unwrap();
    fixture.update(cx, |view, window, cx| {
        view.poll(cx);
        view.directory.update(cx, |input, cx| {
            input.replace_all("C:\\Another App", window, cx);
        });
    });
    cx.run_until_parked();
    fixture.update(cx, |view, _, cx| {
        assert!(view.state.preflight_blocked().is_none());
        assert!(!view.accepted);
        view.start(cx);
    });
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
}

#[gpui_kit::test]
fn changing_a_component_clears_a_preflight_block(cx: &mut TestAppContext) {
    let (fixture, commands, _snapshots, responses) = connected(
        cx,
        UiOptions {
            components: vec![
                UiComponent {
                    id: "core".into(),
                    name: ("主程序".into(), "Application".into()),
                    description: (String::new(), String::new()),
                    size: 1,
                    required: true,
                    selected: true,
                    installed: false,
                },
                UiComponent {
                    id: "optional".into(),
                    name: ("可选组件".into(), "Optional component".into()),
                    description: (String::new(), String::new()),
                    size: 1,
                    required: false,
                    selected: false,
                    installed: false,
                },
            ],
            ..UiOptions::default()
        },
    );
    fixture.update(cx, |view, _, cx| {
        view.accepted = true;
        view.start(cx);
    });
    commands.try_recv().unwrap();
    responses.send(blocked()).unwrap();
    fixture.update(cx, |view, _, cx| view.poll(cx));
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("component-optional", cx);
    })
    .unwrap();
    fixture.update(cx, |view, _, cx| {
        assert!(view.options.components[1].selected);
        assert!(!view.accepted);
        assert!(view.state.preflight_blocked().is_none());
        view.start(cx);
    });
    assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
}

#[gpui_kit::test]
fn late_preflight_feedback_does_not_revive_an_executing_or_terminal_view(cx: &mut TestAppContext) {
    for phase in [
        UiPhase::Preparing,
        UiPhase::Installing,
        UiPhase::Finalizing,
        UiPhase::Succeeded,
        UiPhase::Failed,
        UiPhase::Cancelled,
        UiPhase::Unknown,
    ] {
        let (fixture, commands, snapshots, responses) = connected(cx, UiOptions::default());
        fixture.update(cx, |view, _, cx| {
            view.accepted = true;
            view.start(cx);
        });
        commands.try_recv().unwrap();
        snapshots
            .send(UiSnapshot {
                phase,
                ..UiSnapshot::default()
            })
            .unwrap();
        responses.send(blocked()).unwrap();
        fixture.update(cx, |view, _, cx| {
            view.poll(cx);
            assert_eq!(view.phase(), phase);
            assert!(view.state.preflight_blocked().is_none());
            view.start(cx);
        });
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("preflight-blocked").is_none());
            assert!(window.try_find("start").is_none());
        })
        .unwrap();
        assert_eq!(commands.try_recv(), Err(TryRecvError::Empty));
    }
}
