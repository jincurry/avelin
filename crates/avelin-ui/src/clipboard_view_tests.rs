//! Headless tests of the real input state and application interaction boundaries.
//! Transfers are injected; GPUI's TestPlatform owns every test window. These
//! tests neither launch the native application nor read the system clipboard.

use super::{
    DirectoryMenuLabels, InstallerView, Locale, Paste, PathBuf, Root, UiNotice, UiOptions, UiPhase,
    UiState, directory_context_menu, prepare_logo,
};
use crate::UiSnapshot;
use crate::clipboard::{ClipboardAction, ClipboardEdit};
use gpui_kit::component::input::Undo;
use gpui_kit::component::native_menu::NativeMenu;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext, Context, Entity, EntityInputHandler, Focusable, TestAppContext, Window,
    WindowHandle, WindowOptions,
};

pub(super) struct Fixture {
    pub(super) window: WindowHandle<Root>,
    pub(super) view: Entity<InstallerView>,
}

impl Fixture {
    fn new(cx: &mut TestAppContext) -> Self {
        Self::with_options(
            cx,
            UiOptions {
                install_dir: PathBuf::from("C:\\initial"),
                ..UiOptions::default()
            },
        )
    }

    pub(super) fn with_options(cx: &mut TestAppContext, options: UiOptions) -> Self {
        options
            .presentation
            .validate()
            .expect("valid test presentation");
        let logo = prepare_logo(&options.presentation).expect("valid test logo");
        let mut view = None;
        let window = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.open_window(WindowOptions::default(), |window, cx| {
                let entity = cx.new(|cx| InstallerView::new(options, None, logo, None, window, cx));
                view = Some(entity.clone());
                cx.new(|cx| Root::new(entity, window, cx))
            })
            .unwrap()
        });
        let fixture = Self {
            window,
            view: view.unwrap(),
        };
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.render_frame(cx)
        })
        .unwrap();
        fixture
    }

    pub(super) fn update<R>(
        &self,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut InstallerView, &mut Window, &mut Context<InstallerView>) -> R,
    ) -> R {
        cx.update_window(self.window.into(), |_, window, cx| {
            self.view.update(cx, |view, cx| f(view, window, cx))
        })
        .unwrap()
    }

    fn undo(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.dispatch_action(Box::new(Undo), cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn assert_value(&self, cx: &mut TestAppContext, expected: &str) {
        self.update(cx, |view, _, cx| {
            assert_eq!(view.directory.read(cx).value().as_ref(), expected);
        });
    }

    fn type_text(&self, cx: &mut TestAppContext, text: &str) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.input(text, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }
}

#[gpui_kit::test]
fn failed_cut_preserves_selection_and_existing_undo(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory.update(cx, |input, cx| {
            input.replace_all("C:\\edited\\中文", window, cx);
            input.set_selected_range(10..16, cx);
        });
        let before = view.directory.read(cx).value();
        let selection = view.directory.read(cx).selected_range();
        let mut called = false;
        view.directory_clipboard_using(ClipboardAction::Cut, window, cx, |_, action, selected| {
            called = true;
            assert!(matches!(action, ClipboardAction::Cut));
            assert_eq!(selected, "中文");
            Err("stage=open win32=5 attempts=4".into())
        });
        assert!(called);
        assert_eq!(view.directory.read(cx).value(), before);
        assert_eq!(view.directory.read(cx).selected_range(), selection);
        assert!(view.status_note.is_some());
    });
    fixture.undo(cx);
    fixture.assert_value(cx, "C:\\initial");
}

#[gpui_kit::test]
fn successful_cut_has_one_undo_step(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory.update(cx, |input, cx| {
            input.set_value("C:\\应用\\🦀\\bin", window, cx);
            input.set_selected_range(3..9, cx);
        });
        let mut called = false;
        view.directory_clipboard_using(ClipboardAction::Cut, window, cx, |_, _, selected| {
            called = true;
            assert_eq!(selected, "应用");
            Ok(ClipboardEdit::Cut)
        });
        assert!(called);
        assert_eq!(view.directory.read(cx).value().as_ref(), "C:\\\\🦀\\bin");
    });
    fixture.undo(cx);
    fixture.assert_value(cx, "C:\\应用\\🦀\\bin");
    fixture.update(cx, |view, _, cx| {
        assert_eq!(view.directory.read(cx).selected_range(), 3..9);
    });
}

#[gpui_kit::test]
fn successful_paste_replaces_selection_and_has_one_undo_step(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory.update(cx, |input, cx| {
            input.set_value("C:\\old\\bin", window, cx);
            input.set_selected_range(3..6, cx);
        });
        view.directory_clipboard_using(ClipboardAction::Paste, window, cx, |_, _, selected| {
            assert_eq!(selected, "old");
            Ok(ClipboardEdit::Paste("新\r\n🦀".into()))
        });
        assert_eq!(view.directory.read(cx).value().as_ref(), "C:\\新🦀\\bin");
    });
    fixture.undo(cx);
    fixture.assert_value(cx, "C:\\old\\bin");
    fixture.update(cx, |view, _, cx| {
        assert_eq!(view.directory.read(cx).selected_range(), 3..6);
    });
}

#[gpui_kit::test]
fn absent_or_empty_paste_does_not_delete_selection_or_add_undo(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory.update(cx, |input, cx| {
            input.replace_all("C:\\edited", window, cx);
            input.set_selected_range(3..9, cx);
        });
        for result in [
            ClipboardEdit::NoText,
            ClipboardEdit::Paste(String::new()),
            ClipboardEdit::Paste("\r\n".into()),
        ] {
            view.directory_clipboard_using(ClipboardAction::Paste, window, cx, |_, _, _| {
                Ok(result)
            });
            assert_eq!(view.directory.read(cx).value().as_ref(), "C:\\edited");
            assert_eq!(view.directory.read(cx).selected_range(), 3..9);
        }
    });
    fixture.undo(cx);
    fixture.assert_value(cx, "C:\\initial");
}

#[gpui_kit::test]
fn disabled_readonly_picker_and_nonready_reject_before_transfer(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory
            .update(cx, |input, cx| input.select_all(window, cx));
        let original = view.directory.read(cx).value();
        let selection = view.directory.read(cx).selected_range();

        view.directory
            .update(cx, |input, cx| input.set_disabled(true, cx));
        view.directory_clipboard_using(ClipboardAction::Cut, window, cx, |_, _, _| {
            panic!("disabled input must not transfer")
        });
        view.directory.update(cx, |input, cx| {
            input.set_disabled(false, cx);
            input.set_readonly(true, cx);
        });
        view.directory_clipboard_using(ClipboardAction::Paste, window, cx, |_, _, _| {
            panic!("readonly input must not transfer")
        });
        view.directory
            .update(cx, |input, cx| input.set_readonly(false, cx));

        let request = view.picker.begin(UiPhase::Ready).unwrap();
        view.directory_clipboard_using(ClipboardAction::Cut, window, cx, |_, _, _| {
            panic!("pending picker must not transfer")
        });
        assert!(view.picker.finish(request, UiPhase::Ready));

        for phase in [
            UiPhase::Preparing,
            UiPhase::Installing,
            UiPhase::Cancelling,
            UiPhase::Finalizing,
            UiPhase::Succeeded,
            UiPhase::Failed,
            UiPhase::Cancelled,
            UiPhase::Unknown,
        ] {
            view.state = UiState::new(true);
            assert!(view.state.receive(UiSnapshot {
                phase,
                ..UiSnapshot::default()
            }));
            view.directory_clipboard_using(ClipboardAction::Cut, window, cx, |_, _, _| {
                panic!("nonready input must not transfer")
            });
        }
        assert_eq!(view.directory.read(cx).value(), original);
        assert_eq!(view.directory.read(cx).selected_range(), selection);
    });
}

#[gpui_kit::test]
fn ime_composition_rejects_all_transfers_without_mutating_mark(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory.update(cx, |input, cx| {
            input.set_value("C:\\路径", window, cx);
            input.replace_and_mark_text_in_range(Some(3..5), "ni", Some(0..2), window, cx);
        });
        let value = view.directory.read(cx).value();
        let selection = view.directory.read(cx).selected_range();
        let mark = view
            .directory
            .update(cx, |input, cx| input.marked_text_range(window, cx));
        assert_eq!(mark, Some(3..5));
        for action in [
            ClipboardAction::Copy,
            ClipboardAction::Cut,
            ClipboardAction::Paste,
        ] {
            view.directory_clipboard_using(action, window, cx, |_, _, _| {
                panic!("composition must not transfer")
            });
            assert_eq!(view.directory.read(cx).value(), value);
            assert_eq!(view.directory.read(cx).selected_range(), selection);
            assert_eq!(
                view.directory
                    .update(cx, |input, cx| input.marked_text_range(window, cx)),
                mark
            );
            assert!(view.status_note.is_some());
        }
    });
}

#[gpui_kit::test]
fn keyboard_cut_and_dispatched_paste_are_captured_during_composition(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory.update(cx, |input, cx| {
            input.set_value("C:\\original", window, cx);
            input.replace_and_mark_text_in_range(Some(3..11), "ni", Some(0..2), window, cx);
        });
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("ctrl-x", cx);
    })
    .unwrap();
    cx.run_until_parked();
    fixture.assert_value(cx, "C:\\ni");
    fixture.update(cx, |view, window, cx| {
        assert!(
            view.status_note.is_some(),
            "the production capture handler must receive Ctrl+X"
        );
        assert_eq!(
            view.directory
                .update(cx, |input, cx| input.marked_text_range(window, cx)),
            Some(3..5)
        );
        view.status_note = None;
    });
    // Native-menu selections use the same dispatch_action path. Dispatch the
    // action directly: opening the Windows menu itself is not a headless test.
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.dispatch_action(Box::new(Paste), cx);
    })
    .unwrap();
    cx.run_until_parked();
    fixture.update(cx, |view, window, cx| {
        assert!(view.status_note.is_some());
        assert_eq!(view.directory.read(cx).value().as_ref(), "C:\\ni");
        assert_eq!(
            view.directory
                .update(cx, |input, cx| input.marked_text_range(window, cx)),
            Some(3..5)
        );
    });
}

#[gpui_kit::test]
fn unfocused_directory_does_not_transfer_or_change_text(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.focus_handle.focus(window, cx);
        assert!(!view.directory.read(cx).focus_handle(cx).is_focused(window));
        view.directory_clipboard_using(ClipboardAction::Cut, window, cx, |_, _, _| {
            panic!("another focus must not transfer")
        });
        assert_eq!(view.directory.read(cx).value().as_ref(), "C:\\initial");
    });
}

#[gpui_kit::test]
fn context_menu_builds_while_input_is_borrowed_without_reading_clipboard(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        // The component invokes its custom builder from InputState::defer_in,
        // which holds an update borrow on this exact input. Reproduce that
        // contract instead of calling the helper outside the input update.
        let focus = view.directory.read(cx).focus_handle(cx);
        for (editable, chinese, available) in [
            (true, true, true),
            (true, false, false),
            (false, true, true),
            (false, false, false),
        ] {
            view.focus_handle.focus(window, cx);
            let labels = DirectoryMenuLabels::new(
                &view.options.presentation,
                if chinese {
                    Locale::Chinese
                } else {
                    Locale::English
                },
            );
            let mut queried = false;
            view.directory.update(cx, |input, cx| {
                input.set_selected_range(3..10, cx);
                let before_value = input.value();
                let before_range = input.selected_range();
                let menu = directory_context_menu(
                    NativeMenu::new(),
                    &focus,
                    editable,
                    &labels,
                    window,
                    cx,
                    || {
                        assert!(!queried, "format availability is queried at most once");
                        queried = true;
                        available
                    },
                );
                assert!(!menu.is_empty());
                assert!(focus.is_focused(window));
                assert_eq!(input.value(), before_value);
                assert_eq!(input.selected_range(), before_range);
            });
            assert_eq!(queried, editable, "disabled menus need no format query");
        }
    });
}

#[gpui_kit::test]
fn typing_clears_directory_validation_and_clipboard_failure_notices(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory
            .update(cx, |input, cx| input.set_value("", window, cx));
        view.accepted = true;
        view.start(cx);
        assert_eq!(view.status_note, Some(UiNotice::DirectoryRequired));
        assert_eq!(view.phase(), UiPhase::Ready);
    });
    fixture.type_text(cx, "C:\\corrected");
    fixture.assert_value(cx, "C:\\corrected");
    fixture.update(cx, |view, window, cx| {
        assert_eq!(view.status_note, None);
        view.directory_clipboard_using(ClipboardAction::Paste, window, cx, |_, _, _| {
            Err("simulated unavailable clipboard".into())
        });
        assert_eq!(view.status_note, Some(UiNotice::ClipboardUnavailable));
    });
    fixture.type_text(cx, "\\next");
    fixture.assert_value(cx, "C:\\corrected\\next");
    fixture.update(cx, |view, _, _| assert_eq!(view.status_note, None));
}

#[gpui_kit::test]
fn input_changes_clear_only_input_related_notices(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    for (notice, should_clear) in [
        (UiNotice::DirectoryRequired, true),
        (UiNotice::ClipboardComposition, true),
        (UiNotice::ClipboardInputChanged, true),
        (UiNotice::ClipboardNoText, true),
        (UiNotice::ClipboardUnavailable, true),
        (UiNotice::PickerPending, false),
        (UiNotice::PickerUnavailable, false),
        (UiNotice::OperationActive, false),
        (UiNotice::OperationFinishing, false),
    ] {
        fixture.update(cx, |view, _, _| view.status_note = Some(notice));
        fixture.type_text(cx, "x");
        fixture.update(cx, |view, _, _| {
            assert_eq!(
                view.status_note,
                if should_clear { None } else { Some(notice) },
                "the real input Change event must respect the notice scope: {notice:?}"
            );
        });
    }
}

#[gpui_kit::test]
fn language_button_relocalizes_an_existing_notice(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory
            .update(cx, |input, cx| input.set_value("", window, cx));
        view.accepted = true;
        view.start(cx);
        assert_eq!(
            view.text(view.status_note.unwrap().key()),
            "请输入安装目录。"
        );
    });
    for (locale, text) in [
        (Locale::English, "Enter an installation directory."),
        (Locale::Chinese, "请输入安装目录。"),
    ] {
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.click("language", cx);
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        fixture.update(cx, |view, _, _| {
            assert_eq!(view.options.locale, locale);
            assert_eq!(view.status_note, Some(UiNotice::DirectoryRequired));
            assert_eq!(view.text(view.status_note.unwrap().key()), text);
            assert_eq!(view.phase(), UiPhase::Ready);
        });
    }
}

#[gpui_kit::test]
fn browse_selection_is_one_undo_step_and_keeps_earlier_edits(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory.update(cx, |input, cx| {
            input.replace_all("C:\\typed\\中文", window, cx);
            input.set_selected_range(3..8, cx);
        });
        view.browse(window, cx);
        assert!(view.picker.pending());
    });
    // TestPlatform resolves the real async Browse path without a native dialog.
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|options| {
        assert!(options.directories);
        assert!(!options.files);
        assert!(!options.multiple);
        Some(vec![PathBuf::from("D:\\selected\\🦀")])
    });
    cx.run_until_parked();
    fixture.assert_value(cx, "D:\\selected\\🦀");
    fixture.update(cx, |view, window, cx| {
        assert!(!view.picker.pending());
        assert!(view.directory.read(cx).focus_handle(cx).is_focused(window));
    });
    fixture.undo(cx);
    fixture.assert_value(cx, "C:\\typed\\中文");
    fixture.undo(cx);
    fixture.assert_value(cx, "C:\\initial");
}

#[gpui_kit::test]
fn cancelled_browse_preserves_text_selection_and_undo(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory.update(cx, |input, cx| {
            input.replace_all("C:\\typed", window, cx);
            input.set_selected_range(3..8, cx);
        });
        view.browse(window, cx);
        assert!(!view.may_close(cx));
        assert_eq!(view.status_note, Some(UiNotice::PickerPending));
    });
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();
    fixture.update(cx, |view, _, cx| {
        assert!(!view.picker.pending());
        assert_eq!(view.directory.read(cx).value().as_ref(), "C:\\typed");
        assert_eq!(view.directory.read(cx).selected_range(), 3..8);
        assert_eq!(view.status_note, None);
        assert!(view.may_close(cx));
    });
    fixture.undo(cx);
    fixture.assert_value(cx, "C:\\initial");
}

#[gpui_kit::test]
fn failed_browse_preserves_text_selection_and_undo(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, window, cx| {
        view.directory.update(cx, |input, cx| {
            input.replace_all("C:\\typed", window, cx);
            input.set_selected_range(3..8, cx);
        });
    });
    cx.run_until_parked();
    fixture.update(cx, |view, window, cx| {
        let request = view.picker.begin(view.phase()).unwrap();
        // TestPlatform cannot return a native picker error. Inject that result
        // at the production completion boundary; do not claim native coverage.
        view.finish_browse(request, Err(()), window, cx);
        assert!(!view.picker.pending());
        assert_eq!(view.status_note, Some(UiNotice::PickerUnavailable));
        assert_eq!(view.directory.read(cx).value().as_ref(), "C:\\typed");
        assert_eq!(view.directory.read(cx).selected_range(), 3..8);
    });
    fixture.undo(cx);
    fixture.assert_value(cx, "C:\\initial");
    fixture.update(cx, |view, _, _| {
        assert_eq!(view.status_note, Some(UiNotice::PickerUnavailable));
    });
}

#[gpui_kit::test]
fn phase_change_clears_close_warning_and_uses_current_close_guard(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.update(cx, |view, _, _| {
        view.state = UiState::new(true);
        assert!(view.state.request_start(true));
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx)
    })
    .unwrap();
    fixture.update(cx, |view, _, cx| {
        assert!(!view.may_close(cx));
        assert_eq!(view.status_note, Some(UiNotice::OperationActive));
        view.state.receive(UiSnapshot {
            phase: UiPhase::Finalizing,
            ..UiSnapshot::default()
        });
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx)
    })
    .unwrap();
    fixture.update(cx, |view, _, cx| {
        assert_eq!(view.status_note, None);
        assert!(!view.may_close(cx));
        assert_eq!(view.status_note, Some(UiNotice::OperationFinishing));
        view.state.receive(UiSnapshot {
            phase: UiPhase::Succeeded,
            ..UiSnapshot::default()
        });
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx)
    })
    .unwrap();
    fixture.update(cx, |view, _, cx| {
        assert_eq!(view.status_note, None);
        assert!(view.may_close(cx));
    });
}
