//! Configuration integration through the real headless view, without native windows.

use super::clipboard_view_tests::Fixture;
use super::{
    BrandPalette, DirectoryMenuLabels, Locale, Presentation, UiBridge, UiCommand, UiNotice,
    UiOptions, UiPhase, UiState, contrast, logo_initial, mix_color, window_title,
};
use crate::UiSnapshot;
use avelin_core::presentation::LocalePreference;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, TestAppContext};
use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

fn branded_options() -> UiOptions {
    let mut presentation = Presentation {
        accent_color: "#FFE083".into(),
        ..Presentation::default()
    };
    presentation.strings.insert(
        Locale::Chinese,
        [
            ("window.title", "{app_name} 安装中心"),
            ("directory.browse", "选择位置"),
            ("action.demo", "开始预览"),
            ("brand.version", "构建 {version}"),
            ("progress.percent", "已完成 {percent}%"),
            ("menu.copy", "复制路径"),
            ("notice.directory_required", "请先选择位置。"),
            ("error.disconnected", "连接已中断，请查看日志。"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect(),
    );
    UiOptions {
        app_name: "松林 Tools".into(),
        app_version: "9.8.7+{local}".into(),
        install_dir: PathBuf::from("C:\\initial"),
        presentation,
        ..UiOptions::default()
    }
}

#[gpui_kit::test]
fn packaged_brand_and_text_reach_the_compact_view(cx: &mut TestAppContext) {
    let fixture = Fixture::with_options(cx, branded_options());
    cx.update_window(fixture.window.into(), |_, window, _| {
        assert_eq!(window.find("browse").label(), Some("选择位置"));
        assert_eq!(window.find("start").label(), Some("开始预览"));
    })
    .unwrap();
    fixture.update(cx, |view, _, _| {
        assert_eq!(window_title(&view.options), "松林 Tools 安装中心");
        assert_eq!(view.version_text(), "构建 9.8.7+{local}");
        assert_eq!(view.progress_text(Some(42.)), "已完成 42%");
        assert_eq!(view.brand.accent, 0xffe083);
        assert_eq!(view.brand.foreground, 0x000000);
        assert_eq!(logo_initial(&view.options.app_name), "松");
        assert!(view.logo.is_none());
        assert!(view.demo());
        assert_eq!(view.phase(), UiPhase::Ready);
    });
}

#[gpui_kit::test]
fn language_switch_uses_builtin_fallback_and_preserves_input(cx: &mut TestAppContext) {
    let mut options = branded_options();
    options.presentation.locale = LocalePreference::Auto;
    options.presentation.fallback_locale = Locale::Chinese;
    options.locale = options.presentation.resolve_locale(Some("fr-FR"));
    let fixture = Fixture::with_options(cx, options);
    fixture.update(cx, |view, _, cx| {
        view.directory
            .update(cx, |input, cx| input.set_selected_range(3..7, cx));
        view.status_note = Some(UiNotice::DirectoryRequired);
        assert_eq!(view.text(view.status_note.unwrap().key()), "请先选择位置。");
        assert_eq!(
            DirectoryMenuLabels::new(&view.options.presentation, view.options.locale).copy,
            "复制路径"
        );
        // Missing Chinese keys use the Chinese built-in catalog.
        assert_eq!(
            DirectoryMenuLabels::new(&view.options.presentation, view.options.locale).cut,
            "剪切"
        );
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("language", cx);
        window.render_frame(cx);
        assert_eq!(window.find("browse").label(), Some("Browse…"));
    })
    .unwrap();
    fixture.update(cx, |view, _, cx| {
        assert_eq!(view.options.locale, Locale::English);
        assert_eq!(
            view.text(view.status_note.unwrap().key()),
            "Enter an installation directory."
        );
        assert_eq!(
            DirectoryMenuLabels::new(&view.options.presentation, view.options.locale).copy,
            "Copy"
        );
        assert_eq!(window_title(&view.options), "松林 Tools · Setup");
        assert_eq!(view.directory.read(cx).value().as_ref(), "C:\\initial");
        assert_eq!(view.directory.read(cx).selected_range(), 3..7);
        assert_eq!(view.phase(), UiPhase::Ready);
        assert!(!view.accepted);
    });
}

#[gpui_kit::test]
fn configured_view_preserves_shared_command_and_terminal_semantics(cx: &mut TestAppContext) {
    let fixture = Fixture::with_options(cx, branded_options());
    let (commands, command_rx) = mpsc::channel();
    let (snapshot_tx, snapshots) = mpsc::channel();
    let (_response_tx, responses) = mpsc::channel();
    fixture.update(cx, |view, _, cx| {
        view.bridge = Some(UiBridge {
            commands,
            snapshots,
            responses,
        });
        view.state = UiState::new(true);
        view.accepted = true;
        view.start(cx);
        view.start(cx);
        assert_eq!(view.phase(), UiPhase::Preparing);
        assert!(view.state.awaiting_start());
    });
    assert_eq!(
        command_rx.try_recv().unwrap(),
        UiCommand::Start {
            install_dir: PathBuf::from("C:\\initial"),
            desktop_shortcut: true,
            start_menu_shortcut: true,
            selected_components: None,
            operation: Some(avelin_core::Operation::Install),
            migration_confirmed: false,
        }
    );
    assert_eq!(command_rx.try_recv(), Err(TryRecvError::Empty));
    snapshot_tx
        .send(UiSnapshot {
            phase: UiPhase::Installing,
            completed: 42,
            total: Some(100),
            detail: "Rust: 文件名 {raw} error=5".into(),
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        view.cancel(cx);
        view.cancel(cx);
        assert_eq!(view.phase(), UiPhase::Cancelling);
        view.options.locale = Locale::English;
        assert_eq!(view.detail(), "Rust: 文件名 {raw} error=5");
    });
    assert_eq!(command_rx.try_recv().unwrap(), UiCommand::Cancel);
    assert_eq!(command_rx.try_recv(), Err(TryRecvError::Empty));
    snapshot_tx
        .send(UiSnapshot {
            phase: UiPhase::Installing,
            ..UiSnapshot::default()
        })
        .unwrap();
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.phase(), UiPhase::Cancelling);
    });
    snapshot_tx
        .send(UiSnapshot {
            phase: UiPhase::Cancelled,
            detail: "confirmed by executor".into(),
            ..UiSnapshot::default()
        })
        .unwrap();
    snapshot_tx
        .send(UiSnapshot {
            phase: UiPhase::Succeeded,
            ..UiSnapshot::default()
        })
        .unwrap();
    drop(snapshot_tx);
    fixture.update(cx, |view, _, cx| {
        view.poll(cx);
        assert_eq!(view.phase(), UiPhase::Cancelled);
        assert_eq!(view.detail(), "confirmed by executor");
        assert!(!view.state.reset_demo());
    });
}

#[gpui_kit::test]
fn channel_error_remains_localizable_after_it_occurs(cx: &mut TestAppContext) {
    let fixture = Fixture::with_options(cx, branded_options());
    let (commands, _command_rx) = mpsc::channel();
    let (snapshot_tx, snapshots) = mpsc::channel();
    let (_response_tx, responses) = mpsc::channel();
    drop(snapshot_tx);
    fixture.update(cx, |view, _, cx| {
        view.bridge = Some(UiBridge {
            commands,
            snapshots,
            responses,
        });
        view.state = UiState::new(true);
        view.poll(cx);
        assert_eq!(view.phase(), UiPhase::Unknown);
        assert_eq!(view.detail(), "连接已中断，请查看日志。");
        cx.notify();
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("language", cx);
        window.render_frame(cx);
    })
    .unwrap();
    fixture.update(cx, |view, _, _| {
        assert_eq!(
            view.detail(),
            "Coordinator disconnected. The result is not confirmed."
        );
        assert_eq!(view.phase(), UiPhase::Unknown);
        assert!(view.state.may_close());
    });
}

#[gpui_kit::test]
fn embedded_png_is_decoded_into_the_view_without_a_path(cx: &mut TestAppContext) {
    // One RGBA pixel (#229944), including valid PNG checksums.
    const PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 80, 154, 233, 242,
        31, 0, 3, 223, 1, 255, 217, 203, 54, 132, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    let mut options = branded_options();
    options.presentation.logo_png = Some(PNG.to_vec());
    let fixture = Fixture::with_options(cx, options);
    fixture.update(cx, |view, _, _| {
        let logo = view.logo.as_ref().expect("embedded image retained by view");
        assert_eq!(logo.frame_count(), 1);
        assert_eq!(logo.as_bytes(0).unwrap(), &[0x44, 0x99, 0x22, 0xff]);
    });
}

#[test]
fn all_public_entry_points_reject_bad_presentation_before_native_startup() {
    let mut invalid_template = UiOptions::default();
    invalid_template.presentation.template = "not-supported".into();
    assert!(
        super::run(invalid_template)
            .unwrap_err()
            .0
            .contains("invalid presentation")
    );

    let mut invalid_logo = UiOptions::default();
    invalid_logo.presentation.logo_png = Some(b"not a PNG".to_vec());
    assert!(
        super::run_smoke(invalid_logo, Duration::from_secs(1))
            .unwrap_err()
            .0
            .contains("invalid presentation")
    );

    let mut invalid_color = UiOptions::default();
    invalid_color.presentation.accent_color = "red".into();
    let (commands, command_rx) = mpsc::channel();
    let (_snapshot_tx, snapshots) = mpsc::channel();
    let (_response_tx, responses) = mpsc::channel();
    assert!(
        super::run_connected(
            invalid_color,
            UiBridge {
                commands,
                snapshots,
                responses,
            }
        )
        .unwrap_err()
        .0
        .contains("invalid presentation")
    );
    assert_eq!(command_rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn custom_accents_keep_header_button_states_and_progress_readable() {
    for accent in [
        "#FFFFFF", "#000000", "#FFE083", "#00FFFF", "#FF00FF", "#777777", "#007F40", "#36594C",
    ] {
        let presentation = Presentation {
            accent_color: accent.into(),
            ..Presentation::default()
        };
        presentation.validate().unwrap();
        let palette = BrandPalette::new(&presentation);
        for background in [palette.accent, palette.hover, palette.active] {
            assert!(
                contrast(background, palette.foreground) >= 4.5,
                "{accent}: header/button contrast"
            );
        }
        for (dark, background) in [(false, 0xffffff), (true, 0x18231e)] {
            let bar = palette.progress(dark);
            assert!(
                contrast(bar, mix_color(background, bar, 0.2)) >= 3.,
                "{accent}: progress dark={dark}"
            );
        }
    }
    let default = BrandPalette::new(&Presentation::default());
    assert_eq!(
        (
            default.accent,
            default.hover,
            default.active,
            default.dark_progress
        ),
        (0x36594c, 0x274237, 0x1f352c, 0x9eb4a8)
    );
}
