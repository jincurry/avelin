use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc::TryRecvError};
use std::time::{Duration, Instant};

use gpui_kit::component::{
    Disableable, Root, Sizable, Theme, ThemeMode,
    button::{Button, ButtonCustomVariant, ButtonVariants},
    checkbox::Checkbox,
    input::{Copy, Cut, Escape, Input, InputEvent, InputState, Paste, SelectAll},
    native_menu::NativeMenu,
    progress::Progress,
};
#[cfg(test)]
use gpui_kit::test::TestSupportExt;
use gpui_kit::{prelude::FluentBuilder, *};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::clipboard::{self, ClipboardAction, ClipboardEdit};
use crate::interaction::DirectoryPicker;
use crate::state::{LaunchStatus, UiState};
use crate::{Locale, Presentation, UiBridge, UiCommand, UiError, UiOptions, UiPhase};
use avelin_core::Operation;

#[path = "components_view.rs"]
mod components_view;

#[cfg(test)]
#[path = "components_view_tests.rs"]
mod components_view_tests;

#[cfg(test)]
#[path = "clipboard_view_tests.rs"]
mod clipboard_view_tests;

#[cfg(test)]
#[path = "presentation_view_tests.rs"]
mod presentation_view_tests;

#[cfg(test)]
#[path = "operation_view_tests.rs"]
mod operation_view_tests;

#[cfg(test)]
#[path = "preflight_view_tests.rs"]
mod preflight_view_tests;

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryMenuLabels {
    cut: String,
    copy: String,
    paste: String,
    select_all: String,
}

impl DirectoryMenuLabels {
    fn new(presentation: &Presentation, locale: Locale) -> Self {
        let text = |key| {
            presentation
                .text(locale, key, &[])
                .expect("validated menu text")
        };
        Self {
            cut: text("menu.cut"),
            copy: text("menu.copy"),
            paste: text("menu.paste"),
            select_all: text("menu.select_all"),
        }
    }
}

/// The input invokes this builder while its entity is already being updated.
/// Keep only a focus handle and plain values here; never read/update that entity.
fn directory_context_menu(
    menu: NativeMenu,
    focus: &FocusHandle,
    editable: bool,
    labels: &DirectoryMenuLabels,
    window: &mut Window,
    cx: &mut App,
    text_available: impl FnOnce() -> bool,
) -> NativeMenu {
    focus.focus(window, cx);
    // Selection can change on right-click after the last parent render. Keep
    // Copy/Cut available here and let the action handler check the live selection.
    // Format availability does not open/read the clipboard just to show a menu.
    menu.menu_with_disabled(labels.cut.clone(), !editable, Box::new(Cut))
        .menu_with_disabled(labels.copy.clone(), !editable, Box::new(Copy))
        .menu_with_disabled(
            labels.paste.clone(),
            !(editable && text_available()),
            Box::new(Paste),
        )
        .separator()
        .menu(labels.select_all.clone(), Box::new(SelectAll))
}

/// Opens an explicitly labelled demonstration. No filesystem changes are made.
pub fn run(options: UiOptions) -> Result<(), UiError> {
    run_inner(options, None, None)
}

/// Opens only the file-free demonstration and exits after the given duration.
/// Intended for an observable native startup probe, not visual acceptance.
pub fn run_smoke(options: UiOptions, duration: Duration) -> Result<(), UiError> {
    run_inner(options, None, Some(duration))
}

/// Opens a view driven by the host coordinator. Only the host sends real results.
pub fn run_connected(options: UiOptions, bridge: UiBridge) -> Result<(), UiError> {
    run_inner(options, Some(bridge), None)
}

fn run_inner(
    options: UiOptions,
    bridge: Option<UiBridge>,
    smoke_duration: Option<Duration>,
) -> Result<(), UiError> {
    // Reject configuration errors before creating GPUI or a native window.
    options
        .presentation
        .validate()
        .map_err(|error| UiError(format!("invalid presentation: {error:#}")))?;
    if options.operation == Operation::Upgrade {
        let previous = options
            .previous_version
            .as_deref()
            .ok_or_else(|| UiError("upgrade requires a verified previous version".into()))?;
        avelin_core::model::ensure_newer_version(previous, &options.app_version)
            .map_err(|error| UiError(format!("invalid upgrade versions: {error:#}")))?;
    }
    let logo = prepare_logo(&options.presentation)?;
    // Probe-only clocks start at UI entry, not process creation. They measure
    // callbacks, never visual correctness, presentation success or interactivity.
    let probe_started = smoke_duration.map(|_| Instant::now());
    if probe_started.is_some() {
        eprintln!("AVELIN_UI_PROBE_ENTRY elapsed_us=0");
    }
    let failure = Arc::new(Mutex::new(None));
    let capture = failure.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                cx.bind_keys([KeyBinding::new("escape", Escape, Some("AvelinInstaller"))]);
                let window_options = WindowOptions {
                    window_bounds: Some(WindowBounds::centered(size(px(680.), px(540.)), cx)),
                    window_min_size: Some(size(px(600.), px(500.))),
                    ..WindowOptions::default()
                };
                if let Err(error) = cx.open_window(window_options, |window, cx| {
                    window.set_window_title(&window_title(&options));
                    window.activate_window();
                    eprintln!("AVELIN_UI_WINDOW_CREATED");
                    if let Some(started) = probe_started {
                        eprintln!(
                            "AVELIN_UI_PROBE_WINDOW elapsed_us={} scale_factor={}",
                            started.elapsed().as_micros(),
                            window.scale_factor()
                        );
                    }
                    Theme::change(
                        if options.dark {
                            ThemeMode::Dark
                        } else {
                            ThemeMode::Light
                        },
                        Some(window),
                        cx,
                    );
                    if let Some(specs) = window.gpu_specs() {
                        eprintln!(
                            "AVELIN_UI_RENDERER device={:?} software_emulated={} driver={:?}",
                            specs.device_name, specs.is_software_emulated, specs.driver_name
                        );
                    }
                    let view = cx.new(|cx| {
                        InstallerView::new(options, bridge, logo, probe_started, window, cx)
                    });
                    if let Some(duration) = smoke_duration {
                        let mut probe_window = window.to_async(cx);
                        cx.spawn(async move |cx| {
                            cx.background_executor().timer(duration).await;
                            let _ = probe_window.update(|window, cx| {
                                eprintln!(
                                    "AVELIN_UI_SMOKE_TIMEOUT elapsed_ms={}",
                                    duration.as_millis()
                                );
                                if let Some(started) = probe_started {
                                    eprintln!(
                                        "AVELIN_UI_PROBE_TIMER elapsed_us={}",
                                        started.elapsed().as_micros()
                                    );
                                }
                                // Windows retains the focused text input handler
                                // in its platform window. Render one unfocused
                                // frame to release that handler before shutdown;
                                // otherwise the probe's leak detector sees its
                                // InputState after the event loop has stopped.
                                window.disable_focus(cx);
                                // GPUI currently dispatches next-frame callbacks
                                // before drawing. Wait through that frame before
                                // quitting from the following callback.
                                window.on_next_frame(|window, _| {
                                    window.on_next_frame(|_, cx| {
                                        eprintln!("AVELIN_UI_PROBE_INPUT_RELEASE_FRAME");
                                        cx.quit();
                                    });
                                });
                            });
                        })
                        .detach();
                    }
                    let closing_view = view.downgrade();
                    window.on_window_should_close(cx, move |_, cx| {
                        if closing_view
                            .update(cx, |view, cx| view.request_close(cx))
                            .is_err()
                        {
                            cx.quit();
                        }
                        // This installer owns one application window. Finish
                        // its event loop through GPUI's application shutdown,
                        // rather than letting WM_CLOSE destroy the HWND before
                        // GPUI releases its platform window. The host still
                        // joins its coordinator after run_connected returns.
                        false
                    });
                    cx.new(|cx| Root::new(view, window, cx))
                }) {
                    if let Ok(mut failure) = capture.lock() {
                        *failure = Some(error.to_string());
                    }
                    cx.quit();
                }
            });
    }));
    if let Err(payload) = result {
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
            })
            .unwrap_or_else(|| "GPUI initialization or event loop panicked".into());
        return Err(UiError(message));
    }
    let error = failure.lock().ok().and_then(|mut value| value.take());
    error.map_or(Ok(()), |message| Err(UiError(message)))
}

fn window_title(options: &UiOptions) -> String {
    if matches!(
        options.operation,
        Operation::Repair | Operation::Recover | Operation::Modify
    ) {
        return format!(
            "{} · {}",
            options.app_name,
            match (options.operation, options.locale) {
                (Operation::Repair, Locale::Chinese) => "修复",
                (Operation::Repair, Locale::English) => "Repair",
                (Operation::Modify, Locale::Chinese) => "更改组件",
                (Operation::Modify, Locale::English) => "Change components",
                (_, Locale::Chinese) => "恢复未完成的操作",
                (_, Locale::English) => "Recover interrupted operation",
            }
        );
    }
    options
        .presentation
        .text(
            options.locale,
            match options.operation {
                Operation::Uninstall => "window.uninstall_title",
                Operation::Upgrade => "window.upgrade_title",
                Operation::Install => "window.title",
                Operation::Repair | Operation::Recover | Operation::Modify => {
                    unreachable!("handled maintenance title")
                }
            },
            &[("app_name", &options.app_name)],
        )
        .expect("validated window title")
}

fn prepare_logo(presentation: &Presentation) -> Result<Option<Arc<RenderImage>>, UiError> {
    presentation
        .logo_png
        .as_ref()
        .map(|bytes| {
            // The format is fixed and the source is owned package bytes. This never
            // enters GPUI's file/URL resource loader. Keep the decoded image for render.
            Image::from_bytes(ImageFormat::Png, bytes.clone())
                .to_image_data(SvgRenderer::new(Arc::new(gpui_kit::assets::Assets)))
                .map_err(|error| UiError(format!("could not render embedded PNG logo: {error:#}")))
        })
        .transpose()
}

fn logo_initial(app_name: &str) -> String {
    app_name.trim().chars().next().unwrap_or('?').to_string()
}

#[derive(Clone, Copy, Debug)]
struct BrandPalette {
    accent: u32,
    foreground: u32,
    hover: u32,
    active: u32,
    light_progress: u32,
    dark_progress: u32,
}

impl BrandPalette {
    fn new(presentation: &Presentation) -> Self {
        let accent = presentation.accent_rgb().expect("validated accent color");
        let foreground = presentation
            .accent_foreground_rgb()
            .expect("validated accent color");
        // Shift away from the chosen text color, so hover/active cannot reduce
        // readable text contrast. Preserve the already-accepted default palette.
        let (hover, active) = if accent == 0x36594c {
            (0x274237, 0x1f352c)
        } else {
            let target = if foreground == 0xffffff {
                0x000000
            } else {
                0xffffff
            };
            (
                mix_color(accent, target, 0.18),
                mix_color(accent, target, 0.35),
            )
        };
        let dark_progress = if accent == 0x36594c {
            0x9eb4a8
        } else {
            readable_progress(accent, 0x18231e, 0xffffff)
        };
        Self {
            accent,
            foreground,
            hover,
            active,
            light_progress: readable_progress(accent, 0xffffff, 0x000000),
            dark_progress,
        }
    }

    fn progress(self, dark: bool) -> u32 {
        if dark {
            self.dark_progress
        } else {
            self.light_progress
        }
    }
}

fn mix_color(from: u32, to: u32, amount: f64) -> u32 {
    [16, 8, 0].into_iter().fold(0, |color, shift| {
        let a = ((from >> shift) & 255) as f64;
        let b = ((to >> shift) & 255) as f64;
        color | (((a + (b - a) * amount).round() as u32) << shift)
    })
}

fn luminance(color: u32) -> f64 {
    let channel = |shift: u32| {
        let value = ((color >> shift) & 255u32) as f64 / 255.;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}

fn contrast(a: u32, b: u32) -> f64 {
    let a = luminance(a);
    let b = luminance(b);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn readable_progress(accent: u32, background: u32, target: u32) -> u32 {
    // gpui-component 0.6.1 paints its track with the bar color at 20% alpha.
    // Adapt very pale/dark custom accents so the non-text bar clears 3:1.
    for step in 0..=100 {
        let color = mix_color(accent, target, step as f64 / 100.);
        let track = mix_color(background, color, 0.2);
        if contrast(color, track) >= 3. {
            return color;
        }
    }
    target
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiNotice {
    DirectoryRequired,
    ClipboardComposition,
    ClipboardInputChanged,
    ClipboardNoText,
    ClipboardUnavailable,
    PickerPending,
    PickerUnavailable,
    OperationActive,
    OperationFinishing,
    UninstallActive,
    UpgradeActive,
}

impl UiNotice {
    fn is_input_related(self) -> bool {
        matches!(
            self,
            Self::DirectoryRequired
                | Self::ClipboardComposition
                | Self::ClipboardInputChanged
                | Self::ClipboardNoText
                | Self::ClipboardUnavailable
        )
    }

    fn key(self) -> &'static str {
        match self {
            Self::DirectoryRequired => "notice.directory_required",
            Self::ClipboardComposition => "notice.clipboard_composition",
            Self::ClipboardInputChanged => "notice.clipboard_input_changed",
            Self::ClipboardNoText => "notice.clipboard_no_text",
            Self::ClipboardUnavailable => "notice.clipboard_unavailable",
            Self::PickerPending => "notice.picker_pending",
            Self::PickerUnavailable => "notice.picker_unavailable",
            Self::OperationActive => "notice.operation_active",
            Self::OperationFinishing => "notice.operation_finishing",
            Self::UninstallActive => "notice.uninstall_active",
            Self::UpgradeActive => "notice.upgrade_active",
        }
    }
}

struct InstallerView {
    options: UiOptions,
    logo: Option<Arc<RenderImage>>,
    brand: BrandPalette,
    directory: Entity<InputState>,
    _directory_subscription: Subscription,
    focus_handle: FocusHandle,
    picker: DirectoryPicker,
    last_rendered_phase: UiPhase,
    desktop: bool,
    start_menu: bool,
    accepted: bool,
    migration_confirmed: bool,
    state: UiState,
    bridge: Option<UiBridge>,
    last_demo_step: Instant,
    status_note: Option<UiNotice>,
    local_error: Option<&'static str>,
    render_reported: bool,
    probe_started: Option<Instant>,
}

impl InstallerView {
    fn new(
        mut options: UiOptions,
        bridge: Option<UiBridge>,
        logo: Option<Arc<RenderImage>>,
        probe_started: Option<Instant>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        for component in &mut options.components {
            component.selected |= component.required;
        }
        let directory = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(options.install_dir.to_string_lossy().into_owned())
        });
        let directory_subscription = cx.subscribe_in(
            &directory,
            window,
            |view, _, event: &InputEvent, _, cx| match event {
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false,
                } => view.start(cx),
                InputEvent::Change => {
                    view.accepted = false;
                    view.state.clear_preflight_blocked();
                    if view.status_note.is_some_and(UiNotice::is_input_related) {
                        view.status_note = None;
                    }
                    cx.notify();
                }
                _ => {}
            },
        );
        let focus_handle = cx.focus_handle().tab_stop(false);
        if options.operation == Operation::Install {
            directory.update(cx, |input, cx| input.focus(window, cx));
        } else {
            focus_handle.focus(window, cx);
        }
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                if this.update(cx, |view, cx| view.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        let demo = bridge.is_none();
        let brand = BrandPalette::new(&options.presentation);
        let state = UiState::for_operation(!demo, options.operation);
        let desktop = options.desktop_shortcut;
        let start_menu = options.start_menu_shortcut;
        Self {
            options,
            logo,
            brand,
            directory,
            _directory_subscription: directory_subscription,
            focus_handle,
            picker: DirectoryPicker::default(),
            last_rendered_phase: UiPhase::Ready,
            desktop,
            start_menu,
            accepted: false,
            migration_confirmed: false,
            state,
            bridge,
            last_demo_step: Instant::now(),
            status_note: None,
            local_error: None,
            render_reported: false,
            probe_started,
        }
    }

    fn text(&self, key: &str) -> String {
        self.text_with(key, &[])
    }

    fn text_with(&self, key: &str, args: &[(&str, &str)]) -> String {
        self.options
            .presentation
            .text(self.options.locale, key, args)
            .expect("validated presentation text")
    }

    fn disconnect(&mut self, key: &'static str) -> bool {
        if !self.state.disconnect("") {
            return false;
        }
        self.local_error = Some(key);
        true
    }

    fn detail(&self) -> String {
        // Only UI-generated messages are localized. Backend diagnostics remain verbatim.
        self.local_error
            .map(|key| self.text(key))
            .unwrap_or_else(|| self.state.snapshot().detail.clone())
    }

    fn version_text(&self) -> String {
        self.text_with("brand.version", &[("version", &self.options.app_version)])
    }

    fn progress_text(&self, percent: Option<f32>) -> String {
        percent.map_or_else(
            || self.text("progress.working"),
            |percent| self.text_with("progress.percent", &[("percent", &format!("{percent:.0}"))]),
        )
    }

    fn render_logo(&self) -> AnyElement {
        let container = div()
            .id("brand-logo")
            .aria_label(self.options.app_name.clone())
            .size(px(44.))
            .flex_shrink_0()
            .rounded_lg()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center();
        if let Some(logo) = &self.logo {
            container
                .child(
                    img(logo.clone())
                        .size(px(44.))
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element()
        } else {
            container
                .border_1()
                .border_color(rgb(mix_color(
                    self.brand.accent,
                    self.brand.foreground,
                    0.4,
                )))
                .text_size(px(28.))
                .child(logo_initial(&self.options.app_name))
                .into_any_element()
        }
    }

    fn directory_clipboard(
        &mut self,
        action: ClipboardAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.directory_clipboard_using(action, window, cx, |owner, action, selected| {
            clipboard::transfer(owner, action, selected).map_err(|error| error.to_string())
        });
    }

    /// Handle only the directory input's action path. A failed transfer never
    /// reaches InputState::replace, so text, selection and undo stay intact.
    fn directory_clipboard_using(
        &mut self,
        action: ClipboardAction,
        window: &mut Window,
        cx: &mut Context<Self>,
        transfer: impl FnOnce(usize, ClipboardAction, &str) -> Result<ClipboardEdit, String>,
    ) {
        if !self.directory.read(cx).focus_handle(cx).is_focused(window) {
            cx.propagate();
            return;
        }
        // Also consume rejected actions: falling through would invoke the
        // dependency's unchecked Cut or one-shot clipboard reader.
        cx.stop_propagation();
        if !self.picker.can_edit(self.phase()) || !self.directory.read(cx).is_editable() {
            return;
        }
        let composing = self.directory.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        });
        if composing {
            self.status_note = Some(UiNotice::ClipboardComposition);
            cx.notify();
            return;
        }
        let before_value = self.directory.read(cx).value();
        let before_range = self.directory.read(cx).selected_range();
        let selected = self.directory.read(cx).selected_text().to_string();
        let owner = HasWindowHandle::window_handle(window)
            .ok()
            .and_then(|handle| match handle.as_raw() {
                RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as usize),
                _ => None,
            })
            .unwrap_or_default();
        let result = transfer(owner, action, &selected);
        // Windows may invoke a foreign delayed-rendering clipboard owner.
        // Recheck before applying an edit if that call re-entered the UI.
        if matches!(result, Ok(ClipboardEdit::Cut | ClipboardEdit::Paste(_)))
            && (!self.picker.can_edit(self.phase())
                || !self.directory.read(cx).is_editable()
                || !self.directory.read(cx).focus_handle(cx).is_focused(window)
                || self.directory.read(cx).value() != before_value
                || self.directory.read(cx).selected_range() != before_range
                || self.directory.update(cx, |input, cx| {
                    input.marked_text_range(window, cx).is_some()
                }))
        {
            self.status_note = Some(UiNotice::ClipboardInputChanged);
            cx.notify();
            return;
        }
        match result {
            Ok(ClipboardEdit::Cut) => {
                self.directory
                    .update(cx, |input, cx| input.replace("", window, cx));
                self.status_note = None;
            }
            Ok(ClipboardEdit::Paste(text)) => {
                // This field is single-line. Keep the existing component's
                // newline policy, additionally discarding CR in Windows text.
                let text = text.replace(['\r', '\n'], "");
                if !text.is_empty() {
                    self.directory
                        .update(cx, |input, cx| input.replace(text, window, cx));
                }
                self.status_note = None;
            }
            Ok(ClipboardEdit::Copied | ClipboardEdit::NoSelection) => self.status_note = None,
            Ok(ClipboardEdit::NoText) => {
                self.status_note = Some(UiNotice::ClipboardNoText);
            }
            Err(error) => {
                log::warn!(target: "avelin_ui::clipboard", "operation={action:?} {error}");
                self.status_note = Some(UiNotice::ClipboardUnavailable);
            }
        }
        cx.notify();
    }

    fn demo(&self) -> bool {
        self.bridge.is_none()
    }

    fn uninstall(&self) -> bool {
        self.options.operation == Operation::Uninstall
    }

    fn upgrade(&self) -> bool {
        self.options.operation == Operation::Upgrade
    }

    fn repair(&self) -> bool {
        self.options.operation == Operation::Repair
    }

    fn recover(&self) -> bool {
        self.options.operation == Operation::Recover
    }

    fn maintenance(&self) -> bool {
        self.repair() || self.recover()
    }

    fn localized(&self, chinese: &str, english: &str) -> String {
        match self.options.locale {
            Locale::Chinese => chinese,
            Locale::English => english,
        }
        .to_owned()
    }

    fn repair_policy(&self) -> String {
        if self.options.repair_overwrite {
            self.localized(
                "已显式选择恢复原版：改过的受管文件会先备份，再恢复为此安装包的版本。用户新增文件保持不变。",
                "Original-package restoration was explicitly selected: changed managed files are backed up before replacement. Added user files remain unchanged.",
            )
        } else {
            self.localized(
                "改过的文件会保留，并在结果中列出未修复项。不会将修改直接视为损坏。",
                "Changed files are preserved and listed as unresolved findings. A modification is not treated as damage.",
            )
        }
    }

    fn fixed_directory(&self) -> bool {
        self.options.operation != Operation::Install
    }

    fn upgrade_versions(&self) -> String {
        self.text_with(
            "upgrade.versions",
            &[
                (
                    "previous_version",
                    self.options
                        .previous_version
                        .as_deref()
                        .expect("validated upgrade version"),
                ),
                ("version", &self.options.app_version),
            ],
        )
    }

    fn shows_launch(&self) -> bool {
        !self.demo()
            && !self.uninstall()
            && !self.recover()
            && !self.modify()
            && self.phase() == UiPhase::Succeeded
    }

    fn launch(&mut self, cx: &mut Context<Self>) {
        if !self.state.request_launch() {
            return;
        }
        if self
            .bridge
            .as_ref()
            .is_none_or(|bridge| bridge.commands.send(UiCommand::LaunchInstalled).is_err())
        {
            self.state.close_responses();
        }
        cx.notify();
    }

    fn launch_label(&self) -> String {
        match self.state.launch_status() {
            LaunchStatus::Pending => self.text("action.launching"),
            LaunchStatus::Succeeded => self.text("action.launched"),
            _ => self.text_with("action.launch", &[("app_name", &self.options.app_name)]),
        }
    }

    fn phase(&self) -> UiPhase {
        self.state.phase()
    }

    fn may_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.picker.pending() && self.phase() == UiPhase::Ready {
            self.status_note = Some(UiNotice::PickerPending);
            cx.notify();
            false
        } else if !self.state.may_close() {
            self.status_note = Some(if self.uninstall() {
                UiNotice::UninstallActive
            } else if self.upgrade() {
                UiNotice::UpgradeActive
            } else if self.state.can_cancel() {
                UiNotice::OperationActive
            } else {
                UiNotice::OperationFinishing
            });
            cx.notify();
            false
        } else {
            true
        }
    }

    fn request_close(&mut self, cx: &mut Context<Self>) {
        if self.may_close(cx) {
            cx.quit();
        }
    }

    fn close_on_escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        // Input controls first handle their own context menu/IME action. An
        // unconsumed Escape uses exactly the same close guard as the title bar.
        self.request_close(cx);
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        if self.bridge.is_some() {
            // A bounded drain keeps a noisy event producer from blocking the UI.
            for _ in 0..128 {
                if self.state.disconnected() {
                    break;
                }
                match self
                    .bridge
                    .as_ref()
                    .expect("connected bridge")
                    .snapshots
                    .try_recv()
                {
                    Ok(snapshot) => {
                        changed |= self.state.receive(snapshot);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        changed |= self.disconnect("error.disconnected");
                        break;
                    }
                }
            }
            for _ in 0..32 {
                match self
                    .bridge
                    .as_ref()
                    .expect("connected bridge")
                    .responses
                    .try_recv()
                {
                    Ok(response) => changed |= self.state.receive_response(response),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        changed |= self.state.close_responses();
                        break;
                    }
                }
            }
        } else if self.last_demo_step.elapsed() >= Duration::from_millis(450) {
            self.last_demo_step = Instant::now();
            changed = self.state.advance_demo();
        }
        if changed {
            // A previous close warning no longer describes a terminal result,
            // including Unknown after a channel disconnect.
            if self.phase().is_terminal() {
                self.status_note = None;
            }
            cx.notify();
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if !self.start_enabled() {
            return;
        }
        let directory = if self.fixed_directory() {
            self.options.install_dir.to_string_lossy().into_owned()
        } else {
            self.directory.read(cx).value().to_string()
        };
        if directory.trim().is_empty() {
            self.status_note = Some(UiNotice::DirectoryRequired);
            cx.notify();
            return;
        }
        let intent = UiCommand::Start {
            install_dir: if self.fixed_directory() {
                self.options.install_dir.clone()
            } else {
                PathBuf::from(directory)
            },
            desktop_shortcut: !self.uninstall()
                && !self.maintenance()
                && !self.modify()
                && self.desktop,
            start_menu_shortcut: !self.uninstall()
                && !self.maintenance()
                && !self.modify()
                && self.start_menu,
            selected_components: self.selected_components(),
            operation: Some(self.options.operation),
            migration_confirmed: self.migration_confirmed,
        };
        if !self.state.request_start(self.accepted) {
            return;
        }
        self.status_note = None;
        if let Some(bridge) = &self.bridge {
            if bridge.commands.send(intent).is_err() {
                self.disconnect(if self.uninstall() {
                    "error.uninstall_start_delivery"
                } else if self.upgrade() {
                    "error.upgrade_start_delivery"
                } else {
                    "error.start_delivery"
                });
                cx.notify();
                return;
            }
        }
        self.last_demo_step = Instant::now();
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.state.request_cancel() {
            return;
        }
        if let Some(bridge) = &self.bridge {
            if bridge.commands.send(UiCommand::Cancel).is_err() {
                self.disconnect("error.cancel_delivery");
                self.status_note = None;
                cx.notify();
                return;
            }
        }
        self.last_demo_step = Instant::now();
        cx.notify();
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.fixed_directory() {
            return;
        }
        let Some(request) = self.picker.begin(self.phase()) else {
            return;
        };
        self.status_note = None;
        cx.notify();
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(self.text("directory.prompt").into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let selected = match selected.await {
                Ok(Ok(paths)) => Ok(paths.and_then(|paths| paths.into_iter().next())),
                _ => Err(()),
            };
            let _ = this.update_in(cx, |view, window, cx| {
                view.finish_browse(request, selected, window, cx);
            });
        })
        .detach();
    }

    fn finish_browse(
        &mut self,
        request: u64,
        selected: Result<Option<PathBuf>, ()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.picker.finish(request, self.phase()) {
            return;
        }
        if self.status_note == Some(UiNotice::PickerPending) {
            self.status_note = None;
        }
        match selected {
            Ok(Some(path)) => {
                let value = path.to_string_lossy().into_owned();
                self.options.install_dir = path;
                self.directory.update(cx, |input, cx| {
                    // A user-selected path is an ordinary atomic edit. Retain
                    // the previous text and history so Undo can restore it.
                    input.replace_all(value, window, cx);
                });
            }
            Ok(None) => {}
            Err(()) => self.status_note = Some(UiNotice::PickerUnavailable),
        }
        self.directory
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn title(&self) -> String {
        if self.modify() {
            let (zh, en) = match self.phase() {
                UiPhase::Ready => ("更改安装组件", "Change installed components"),
                UiPhase::Succeeded => ("组件已更改", "Components changed"),
                UiPhase::Failed => ("未能更改组件", "Could not change components"),
                UiPhase::Unknown => ("结果待确认", "Result unconfirmed"),
                _ => ("正在更改组件", "Changing components"),
            };
            return self.localized(zh, en);
        }
        if self.maintenance() {
            let pair = match (self.repair(), self.phase()) {
                (true, UiPhase::Ready) => ("检查与修复", "Inspect and repair"),
                (true, UiPhase::Succeeded) => ("修复流程完成", "Repair finished"),
                (true, UiPhase::Failed) => ("未能完成修复", "Could not repair"),
                (false, UiPhase::Ready) => ("发现未完成的操作", "Interrupted operation found"),
                (false, UiPhase::Succeeded) => ("恢复完成", "Recovery completed"),
                (false, UiPhase::Failed) => ("未能完成恢复", "Could not recover"),
                (_, UiPhase::Unknown) => ("结果待确认", "Result unconfirmed"),
                (true, _) => ("正在修复", "Repairing"),
                (false, _) => ("正在恢复", "Recovering"),
            };
            return self.localized(pair.0, pair.1);
        }
        if self.upgrade() {
            return self.text(match self.phase() {
                UiPhase::Ready => "phase.upgrade_ready",
                UiPhase::Preparing => "phase.upgrade_preparing",
                UiPhase::Installing | UiPhase::Cancelling => "phase.upgrading",
                UiPhase::Finalizing => "phase.upgrade_finalizing",
                UiPhase::Succeeded => "phase.upgraded",
                UiPhase::Failed => "phase.upgrade_failed",
                UiPhase::Cancelled => "phase.cancelled",
                UiPhase::Unknown => "phase.unknown",
            });
        }
        if self.uninstall() {
            return self.text(match self.phase() {
                UiPhase::Ready => "phase.uninstall_ready",
                UiPhase::Preparing => "phase.uninstall_preparing",
                UiPhase::Installing | UiPhase::Cancelling => "phase.uninstalling",
                UiPhase::Finalizing => "phase.uninstall_finalizing",
                UiPhase::Succeeded => "phase.uninstalled",
                UiPhase::Failed => "phase.uninstall_failed",
                UiPhase::Cancelled => "phase.cancelled",
                UiPhase::Unknown => "phase.unknown",
            });
        }
        match self.phase() {
            UiPhase::Ready => self.text("phase.ready"),
            UiPhase::Preparing => self.text("phase.preparing"),
            UiPhase::Installing => self.text("phase.installing"),
            UiPhase::Cancelling => self.text("phase.cancelling"),
            UiPhase::Finalizing => self.text("phase.finalizing"),
            UiPhase::Succeeded => self.text("phase.succeeded"),
            UiPhase::Failed => self.text("phase.failed"),
            UiPhase::Cancelled => self.text("phase.cancelled"),
            UiPhase::Unknown => self.text("phase.unknown"),
        }
    }

    fn description(&self) -> String {
        if self.modify() {
            return self.localized(
                "勾选需要保留的组件。新增组件将安装，取消勾选的组件将移除；修改过的文件和用户新增文件会保留。",
                "Select the components to keep. Newly selected components are installed; deselected components are removed. Changed files and added user files are preserved.",
            );
        }
        if self.maintenance() {
            if self.phase() == UiPhase::Ready {
                return if self.repair() {
                    self.localized(
                        "补齐缺失的受管文件、快捷方式和应用登记，保留其他用户内容。请先查看检查结果。",
                        "Restore missing managed files, shortcuts and application registration. Preserve other user content. Review the findings first.",
                    )
                } else {
                    self.localized(
                        "检查已退出任务的记录。尚未提交时恢复原状；已提交时核对结果。确认后才执行恢复。",
                        "Inspect the exited task. Restore the previous state before commit, or verify an already committed result. Recovery requires confirmation.",
                    )
                };
            }
            return self.localized(
                "以执行报告为准；恢复或修复结果未确认前，请保留事务证据。",
                "Use the execution report to confirm the result. Retain transaction evidence until recovery or repair is confirmed.",
            );
        }
        if self.demo() {
            return self.text("description.demo");
        }
        if self.state.awaiting_start() {
            return self.text(if self.uninstall() {
                "description.uninstall_awaiting_start"
            } else if self.upgrade() {
                "description.upgrade_awaiting_start"
            } else {
                "description.awaiting_start"
            });
        }
        if self.uninstall() {
            return self.text(match self.phase() {
                UiPhase::Ready => "uninstall.description",
                UiPhase::Finalizing => "description.uninstall_finalizing",
                UiPhase::Unknown => "description.uninstall_unknown",
                _ => "description.uninstall_reported",
            });
        }
        if self.upgrade() {
            return self.text(match self.phase() {
                UiPhase::Ready => "upgrade.description",
                UiPhase::Finalizing => "description.upgrade_finalizing",
                UiPhase::Unknown => "description.upgrade_unknown",
                _ => "description.upgrade_reported",
            });
        }
        match self.phase() {
            UiPhase::Ready => self.text("description.ready"),
            UiPhase::Cancelling => self.text("description.cancelling"),
            UiPhase::Finalizing => self.text("description.finalizing"),
            UiPhase::Unknown => self.text("description.unknown"),
            _ => self.text("description.reported"),
        }
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.modify() {
            return self.render_modify_confirmation(cx);
        }
        if self.maintenance() {
            return self.render_maintenance_confirmation(cx);
        }
        if self.uninstall() {
            return self.render_uninstall_confirmation(cx);
        }
        if self.upgrade() {
            return self.render_upgrade_confirmation(cx);
        }
        let editable = self.picker.can_edit(self.phase());
        let menu_focus = self.directory.read(cx).focus_handle(cx);
        let menu_labels = DirectoryMenuLabels::new(&self.options.presentation, self.options.locale);
        let muted = if self.options.dark {
            rgb(0xa7b5ae)
        } else {
            rgb(0x6b756f)
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.text("directory.label")),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .capture_action(cx.listener(|view, _: &Copy, window, cx| {
                                view.directory_clipboard(ClipboardAction::Copy, window, cx)
                            }))
                            .capture_action(cx.listener(|view, _: &Cut, window, cx| {
                                view.directory_clipboard(ClipboardAction::Cut, window, cx)
                            }))
                            .capture_action(cx.listener(|view, _: &Paste, window, cx| {
                                view.directory_clipboard(ClipboardAction::Paste, window, cx)
                            }))
                            .child(
                                Input::new(&self.directory)
                                    .disabled(!editable)
                                    .context_menu(move |menu, window, cx| {
                                        directory_context_menu(
                                            menu,
                                            &menu_focus,
                                            editable,
                                            &menu_labels,
                                            window,
                                            cx,
                                            clipboard::text_available,
                                        )
                                    })
                                    .aria_label(self.text("directory.aria")),
                            ),
                    )
                    .child(
                        Button::new("browse")
                            .label(self.text("directory.browse"))
                            .disabled(!editable)
                            .on_click(cx.listener(|view, _, window, cx| view.browse(window, cx))),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(self.text("directory.hint")),
            )
            .when(self.has_component_choices(), |container| {
                container.child(self.render_components(cx))
            })
            .when(!self.uninstall(), |container| {
                container.child(
                    div()
                        .mt_2()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            Checkbox::new("desktop")
                                .label(self.text("option.desktop"))
                                .checked(self.desktop)
                                .disabled(!editable)
                                .on_change(cx.listener(|view, checked, _, cx| {
                                    if !view.picker.can_edit(view.phase()) {
                                        return;
                                    }
                                    view.desktop = *checked;
                                    view.accepted = false;
                                    view.state.clear_preflight_blocked();
                                    cx.notify();
                                })),
                        )
                        .child(
                            Checkbox::new("start-menu")
                                .label(self.text("option.start_menu"))
                                .checked(self.start_menu)
                                .disabled(!editable)
                                .on_change(cx.listener(|view, checked, _, cx| {
                                    if !view.picker.can_edit(view.phase()) {
                                        return;
                                    }
                                    view.start_menu = *checked;
                                    view.accepted = false;
                                    view.state.clear_preflight_blocked();
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div().mt_2().child(
                    Checkbox::new("consent")
                        .label(if self.demo() {
                            self.text("consent.demo")
                        } else {
                            self.text("consent.install")
                        })
                        .checked(self.accepted)
                        .disabled(!editable)
                        .on_change(cx.listener(|view, checked, _, cx| {
                            if !view.picker.can_edit(view.phase()) {
                                return;
                            }
                            view.accepted = *checked;
                            cx.notify();
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_status(&self) -> AnyElement {
        let snapshot = self.state.snapshot();
        let detail = self.detail();
        let percent = snapshot
            .total
            .filter(|total| *total > 0)
            .map(|total| (snapshot.completed as f64 / total as f64 * 100.).clamp(0., 100.) as f32);
        div()
            .flex()
            .flex_col()
            .gap_5()
            .py_4()
            .child(
                div()
                    .text_size(px(28.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.title()),
            )
            .child(div().text_sm().child(self.description()))
            .when(self.phase().is_running(), |container| {
                container
                    .child(
                        Progress::new("installation-progress")
                            .value(percent.unwrap_or(0.))
                            .loading(percent.is_none())
                            .color(rgb(self.brand.progress(self.options.dark)))
                            .accessibility_label(self.text(if self.uninstall() {
                                "progress.uninstall_aria"
                            } else if self.upgrade() {
                                "progress.upgrade_aria"
                            } else {
                                "progress.aria"
                            })),
                    )
                    .child(div().text_sm().child(self.progress_text(percent)))
            })
            .when(!detail.is_empty(), |container| {
                container.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .text_sm()
                        .whitespace_normal()
                        .child(detail),
                )
            })
            .when(self.shows_launch(), |container| {
                match self.state.launch_status() {
                    LaunchStatus::Pending => {
                        container.child(div().text_sm().child(self.text("action.launching")))
                    }
                    LaunchStatus::Succeeded => {
                        container.child(div().text_sm().child(self.text("action.launched")))
                    }
                    LaunchStatus::Failed(detail) => container.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_sm().child(self.text(if self.upgrade() {
                                "notice.upgrade_launch_failed"
                            } else {
                                "notice.launch_failed"
                            })))
                            .when(!detail.is_empty(), |container| {
                                container.child(
                                    div().text_sm().whitespace_normal().child(detail.clone()),
                                )
                            }),
                    ),
                    LaunchStatus::Unavailable => {
                        container.child(div().text_sm().child(self.text(if self.upgrade() {
                            "notice.upgrade_launch_unavailable"
                        } else {
                            "notice.launch_unavailable"
                        })))
                    }
                    LaunchStatus::Idle => container,
                }
            })
            .into_any_element()
    }

    fn render_upgrade_confirmation(&self, cx: &mut Context<Self>) -> AnyElement {
        let editable = self.phase() == UiPhase::Ready;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(px(24.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.text_with(
                        "upgrade.confirm_title",
                        &[("app_name", &self.options.app_name)],
                    )),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.upgrade_versions()),
            )
            .child(div().text_sm().child(self.text("upgrade.description")))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.text("upgrade.directory")),
            )
            .child(
                div()
                    .id("upgrade-directory")
                    .text_sm()
                    .whitespace_normal()
                    .aria_label(self.text("upgrade.directory"))
                    .child(self.options.install_dir.to_string_lossy().into_owned()),
            )
            .when(self.has_component_choices(), |container| {
                container.child(self.render_components(cx))
            })
            .when(
                self.options.migration_confirmation_required
                    || !self.options.migration_details.is_empty(),
                |container| container.child(self.render_migration_confirmation(cx)),
            )
            .child(
                Checkbox::new("desktop")
                    .label(self.text("option.desktop"))
                    .checked(self.desktop)
                    .disabled(!editable)
                    .on_change(cx.listener(|view, checked, _, cx| {
                        if view.phase() == UiPhase::Ready {
                            view.desktop = *checked;
                            view.accepted = false;
                            view.state.clear_preflight_blocked();
                            cx.notify();
                        }
                    })),
            )
            .child(
                Checkbox::new("start-menu")
                    .label(self.text("option.start_menu"))
                    .checked(self.start_menu)
                    .disabled(!editable)
                    .on_change(cx.listener(|view, checked, _, cx| {
                        if view.phase() == UiPhase::Ready {
                            view.start_menu = *checked;
                            view.accepted = false;
                            view.state.clear_preflight_blocked();
                            cx.notify();
                        }
                    })),
            )
            .child(
                Checkbox::new("consent")
                    .label(self.text(if self.demo() {
                        "consent.demo"
                    } else {
                        "consent.upgrade"
                    }))
                    .checked(self.accepted)
                    .disabled(!editable)
                    .on_change(cx.listener(|view, checked, _, cx| {
                        if view.phase() == UiPhase::Ready {
                            view.accepted = *checked;
                            cx.notify();
                        }
                    })),
            )
            .into_any_element()
    }

    fn render_maintenance_confirmation(&self, cx: &mut Context<Self>) -> AnyElement {
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
            .child(div().text_sm().child(self.description()))
            .when(self.can_switch_maintenance(), |container| {
                container.child(self.render_maintenance_switch(cx))
            })
            .child(
                div()
                    .id("maintenance-directory")
                    .text_sm()
                    .whitespace_normal()
                    .aria_label(self.localized("固定安装目录", "Fixed installation directory"))
                    .child(self.options.install_dir.to_string_lossy().into_owned()),
            )
            .children(
                self.options
                    .maintenance_details
                    .iter()
                    .map(|(chinese, english)| {
                        div()
                            .text_sm()
                            .whitespace_normal()
                            .child(self.localized(chinese, english))
                    }),
            )
            .when(self.repair(), |container| {
                container.child(
                    div()
                        .id("repair-policy")
                        .text_sm()
                        .whitespace_normal()
                        .child(self.repair_policy()),
                )
            })
            .child(
                Checkbox::new("consent")
                    .label(if self.repair() {
                        self.localized(
                            "我已查看检查结果，确认按上述方式修复",
                            "I reviewed the findings and confirm this repair",
                        )
                    } else {
                        self.localized(
                            "我已查看检查结果，确认恢复此未完成的操作",
                            "I reviewed the findings and confirm recovery",
                        )
                    })
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

    fn render_uninstall_confirmation(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .text_size(px(24.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.text_with(
                        "uninstall.confirm_title",
                        &[("app_name", &self.options.app_name)],
                    )),
            )
            .child(div().text_sm().child(self.text("uninstall.description")))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.text("uninstall.directory")),
            )
            .child(
                div()
                    .id("uninstall-directory")
                    .text_sm()
                    .whitespace_normal()
                    .aria_label(self.text("uninstall.directory"))
                    .child(self.options.install_dir.to_string_lossy().into_owned()),
            )
            .child(
                Checkbox::new("consent")
                    .label(if self.demo() {
                        self.text("consent.demo")
                    } else {
                        self.text("consent.uninstall")
                    })
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

impl Render for InstallerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.render_reported {
            self.render_reported = true;
            eprintln!("AVELIN_UI_VIEW_RENDER_CALLED");
            if let Some(started) = self.probe_started {
                eprintln!(
                    "AVELIN_UI_PROBE_RENDER elapsed_us={} scale_factor={}",
                    started.elapsed().as_micros(),
                    window.scale_factor()
                );
                // The framework's next-frame callback is not a GPU-present fence.
                window.on_next_frame(move |_, _| {
                    eprintln!(
                        "AVELIN_UI_PROBE_FRAME_CALLBACK elapsed_us={}",
                        started.elapsed().as_micros()
                    );
                });
            }
        }
        let dark = self.options.dark;
        let background = if dark { rgb(0x18231e) } else { rgb(0xffffff) };
        let foreground = if dark { rgb(0xe8eee9) } else { rgb(0x15191a) };
        let muted = if dark { rgb(0xa7b5ae) } else { rgb(0x6b756f) };
        let border = if dark { rgb(0x35433b) } else { rgb(0xe3e7e3) };
        let footer = if dark { rgb(0x202e26) } else { rgb(0xf7f9f7) };
        let primary_background = rgb(self.brand.accent);
        let primary = ButtonCustomVariant::new(cx)
            .color(primary_background.into())
            .foreground(rgb(self.brand.foreground).into())
            .hover(rgb(self.brand.hover).into())
            .active(rgb(self.brand.active).into());
        let phase = self.phase();
        let main = phase == UiPhase::Ready;
        let start_enabled = self.start_enabled();
        if phase != self.last_rendered_phase {
            let previous = self.last_rendered_phase;
            self.last_rendered_phase = phase;
            // A close warning can mention Cancel only while it is enabled.
            // Discard the old phase's note when its available actions change.
            self.status_note = None;
            // Preparing and Installing share the same enabled controls. Avoid
            // moving focus again when only their status text changes.
            if !(previous == UiPhase::Preparing && phase == UiPhase::Installing) {
                let view = cx.weak_entity();
                window.on_next_frame(move |window, cx| {
                    let _ = view.update(cx, |view, cx| {
                        if view.phase() != phase {
                            return;
                        }
                        if phase == UiPhase::Ready && !view.fixed_directory() {
                            view.directory
                                .update(cx, |input, cx| input.focus(window, cx));
                        } else if view.state.can_cancel() || phase.is_terminal() {
                            // Footer controls are last in the rendered tab order:
                            // Cancel while cancellable, Close after a result.
                            // Use the current frame, after removed controls have
                            // left the tab map, and retain the kit's key handling.
                            window.blur(cx);
                            window.focus_prev(cx);
                        } else {
                            view.focus_handle.focus(window, cx);
                        }
                    });
                });
            }
        }
        div()
            .key_context("AvelinInstaller")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close_on_escape))
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .font_family("Microsoft YaHei UI")
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .gap_3()
                    .px_7()
                    .py_5()
                    .bg(primary_background)
                    .text_color(rgb(self.brand.foreground))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_xs().child(if self.demo() {
                                self.text("header.demo")
                            } else if self.uninstall() {
                                self.text("header.uninstall")
                            } else if self.upgrade() {
                                self.text("header.upgrade")
                            } else {
                                self.text("header.setup")
                            }))
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        Button::new("language")
                                            .small()
                                            .label(self.text("language.switch"))
                                            .on_click(cx.listener(|view, _, window, cx| {
                                                view.options.locale =
                                                    if view.options.locale == Locale::Chinese {
                                                        Locale::English
                                                    } else {
                                                        Locale::Chinese
                                                    };
                                                window
                                                    .set_window_title(&window_title(&view.options));
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("theme")
                                            .small()
                                            .label(if dark {
                                                self.text("theme.light")
                                            } else {
                                                self.text("theme.dark")
                                            })
                                            .on_click(cx.listener(|view, _, window, cx| {
                                                view.options.dark = !view.options.dark;
                                                Theme::change(
                                                    if view.options.dark {
                                                        ThemeMode::Dark
                                                    } else {
                                                        ThemeMode::Light
                                                    },
                                                    Some(window),
                                                    cx,
                                                );
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_4()
                            .child(self.render_logo())
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(px(23.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(self.options.app_name.clone()),
                                    )
                                    .child(div().text_xs().child(self.version_text())),
                            ),
                    ),
            )
            .child(
                div()
                    .id("content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_7()
                    .py_5()
                    .when_some(self.state.preflight_blocked(), |container, detail| {
                        container.child(
                            div()
                                .id("preflight-blocked")
                                .role(Role::Alert)
                                .aria_label(self.localized(
                                    "暂时无法开始",
                                    "Cannot start yet",
                                ))
                                .aria_description(detail.to_owned())
                                .map(|container| {
                                    #[cfg(test)]
                                    let container = container.test_support();
                                    container
                                })
                                .flex()
                                .flex_col()
                                .gap_2()
                                .mb_5()
                                .p_4()
                                .border_1()
                                .border_color(if dark { rgb(0xfbbf24) } else { rgb(0xb45309) })
                                .rounded_md()
                                .text_sm()
                                .whitespace_normal()
                                .child(
                                    div().font_weight(FontWeight::SEMIBOLD).child(self.localized(
                                        "暂时无法开始",
                                        "Cannot start yet",
                                    )),
                                )
                                .child(detail.to_owned())
                                .child(self.localized(
                                    "任务尚未开始。请按以上提示处理；若列出正在运行的应用，请先保存工作并自行关闭，再点击“重新检查并继续”。也可以安全关闭此窗口，稍后再试。",
                                    "The task has not started. Resolve the issues above; if applications are listed, save your work and close them, then select Recheck and continue. You can also safely close this window and try later.",
                                )),
                        )
                    })
                    .child(if main {
                        self.render_main(cx)
                    } else {
                        self.render_status()
                    })
                    .when_some(self.status_note, |container, notice| {
                        container.child(
                            div()
                                .mt_3()
                                .text_sm()
                                .text_color(if dark { rgb(0xfbbf24) } else { rgb(0xb45309) })
                                .child(self.text(notice.key())),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_7()
                    .py_4()
                    .border_t_1()
                    .border_color(border)
                    .bg(footer)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_family("Georgia")
                                    .child(self.text("footer.brand")),
                            )
                            .child(div().text_xs().text_color(muted).child(if self.demo() {
                                self.text("footer.demo")
                            } else if self.uninstall() {
                                self.text("footer.uninstall")
                            } else if self.upgrade() {
                                self.text("footer.upgrade")
                            } else if self.maintenance() {
                                self.localized("安装维护", "Installation maintenance")
                            } else {
                                self.text("footer.install")
                            }))
                            .when(
                                main && self.has_component_choices() && !self.maintenance(),
                                |container| {
                                    container.child(
                                        div()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(self.component_size_summary()),
                                    )
                                },
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .when(main, |row| {
                                row.child(
                                    Button::new("start")
                                        .custom(primary)
                                        // Custom variants tint the idle background at 20% opacity.
                                        // Keep readable text on an opaque enabled button; applying this
                                        // only when enabled preserves the kit's disabled styling.
                                        .when(start_enabled, |button| button.bg(primary_background))
                                        .label(if self.state.preflight_blocked().is_some() {
                                            self.localized("重新检查并继续", "Recheck and continue")
                                        } else if self.modify() && !self.components_changed() {
                                            self.localized("无需更改", "No changes")
                                        } else if self.modify() {
                                            self.localized("应用更改", "Apply changes")
                                        } else if self.demo() {
                                            self.text("action.demo")
                                        } else if self.uninstall() {
                                            self.text("action.uninstall")
                                        } else if self.upgrade() {
                                            self.text("action.upgrade")
                                        } else if self.repair() {
                                            self.localized("修复", "Repair")
                                        } else if self.recover() {
                                            self.localized("恢复", "Recover")
                                        } else {
                                            self.text("action.install")
                                        })
                                        .disabled(!start_enabled)
                                        .on_click(cx.listener(|view, _, _, cx| view.start(cx))),
                                )
                            })
                            .when(phase.is_running() && !self.fixed_directory(), |row| {
                                row.child(
                                    Button::new("cancel")
                                        .label(self.text("action.cancel"))
                                        .disabled(!self.state.can_cancel())
                                        .on_click(cx.listener(|view, _, _, cx| view.cancel(cx))),
                                )
                            })
                            .when(self.shows_launch(), |row| {
                                row.child(
                                    Button::new("launch")
                                        .label(self.launch_label())
                                        .disabled(!self.state.can_launch())
                                        .on_click(cx.listener(|view, _, _, cx| view.launch(cx))),
                                )
                            })
                            .when(phase.is_terminal() && self.demo(), |row| {
                                row.child(
                                    Button::new("again")
                                        .label(self.text("action.retry"))
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            if view.state.reset_demo() {
                                                view.status_note = None;
                                                view.local_error = None;
                                                cx.notify();
                                            }
                                        })),
                                )
                            })
                            .when(phase.is_terminal() || main, |row| {
                                row.child(
                                    Button::new("close")
                                        .label(self.text("action.close"))
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            view.request_close(cx);
                                        })),
                                )
                            }),
                    ),
            )
    }
}
