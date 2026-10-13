# Avelin UI — compact native interface

Uses the published `gpui-kit = 0.6.1` facade and its matching GPUI dependencies.
The Cargo lockfile at the workspace root records the complete resolved graph.

```text
cargo run -p avelin-ui --example ui-smoke
cargo run -p avelin-ui --example ui-smoke -- --en --dark
cargo run -p avelin-ui --example ui-smoke -- --smoke-seconds 3
```

The probe is a console executable so initialization errors and the actual window
renderer adapter report remain visible. It opens a native GPUI window with the
configured brand header, a directory input/picker, options, language/theme
switches, simulated progress, cancellation and terminal state.

`run(UiOptions)` is always a labelled demonstration. It does not install software,
create directories, shortcuts or receipts, or calculate disk free space.
Only demonstration mode advances progress on a timer.

`run_connected(UiOptions, UiBridge)` sends `UiCommand::Start` / `Cancel` intentions
through a standard Rust channel and reads `UiSnapshot` from the coordinator.
It does not verify a successful installation itself. The host must validate
options, identity, paths, receipt/exit-code agreement, cancellation and cleanup.
Channel loss before a terminal snapshot yields `Unknown`, never success.
Start and Cancel only submit intentions. Their pending presentation is separate
from coordinator snapshots: in-flight progress cannot clear "Cancelling", and
only an actual terminal snapshot resolves that request. A late progress message
cannot replace the first terminal result. Connected mode has no timer that
creates successful, failed or cancelled installation results.
`UiCommand`, `UiPhase`, `UiSnapshot` and `UiResponse` re-export the shared task types from
`avelin-core::protocol`. The view retains only local interaction state; it does
not parse backend wire messages or define a public plugin API.
`UiOptions.operation` chooses Install, Uninstall, Upgrade, Repair, Recover or Modify. Install displays the actual
desktop/Start menu choices, initialized by `desktop_shortcut` and
`start_menu_shortcut`; Start submits the user's choices. Uninstall displays a
branded confirmation with the fixed installed location, hides path editing and
shortcut choices, and sends the host-provided path unchanged. It has no Cancel
action because the current backend cannot cancel uninstall. Close is available
before starting and after the confirmed result; an active close request explains
that the operation must finish.

Upgrade uses the fixed installed location and the host-verified `previous_version`
to show the old and new versions before consent. It retains real shortcut choices;
the host initializes them from the existing receipt. It has no Cancel action and
does not turn an active close request into cancellation. Missing, invalid, equal
or decreasing upgrade versions fail before GPUI initialization. The host remains
responsible for the receipt, ownership and executable verification.

After a confirmed connected install or upgrade succeeds, Launch explicitly sends
`LaunchInstalled`; demo and uninstall never offer it. The host revalidates the
installed entry. `UiBridge.responses` carries `LaunchResult` independently of the
installation snapshots. Pending launch is disabled, a failed launch can be
retried explicitly, and success disables repeated launch. A dropped response
channel reports launch unavailable without changing the confirmed installation
result. Success uses localized UI text; launch error diagnostics stay verbatim.

## Packaged presentation

`UiOptions.presentation` carries `avelin_core::presentation::Presentation`:
the `compact` template, locale preference/fallback, theme, `#RRGGBB` accent,
optional PNG bytes and partial Chinese/English catalog overrides. The host
resolves the initial `UiOptions.locale` / `dark` values from this configuration
and any command-line overrides; language/theme buttons change those effective
runtime choices. `app_name` and `app_version` remain product identity options.

All three entry points validate presentation before GPUI initialization. Invalid
templates, colors, PNGs, keys or placeholders return `UiError`; they do not
silently use a default. The logo is decoded once for rendering and retained as
`RenderImage`, without file paths or remote resource loading. If absent, the
header displays the first character of the application name.

The accent colors the header, enabled Start button and progress. Header/button
text uses a contrasting black or white foreground; hover/active colors maintain
that contrast. Progress adapts very pale/dark colors against its track. The
accepted default button opacity and dark progress colors are preserved.

Standard labels, menus, notices and accessibility names use the core catalogs.
Missing overrides use that language's built-in text. Automatic locale detection
uses `fallback_locale` when the host language cannot be matched. Window title,
version and progress support the catalog's named placeholders. Backend diagnostic
details remain verbatim; UI-generated errors retranslate when language changes.

## Component list and maintenance

`UiOptions.components` carries host-validated `UiComponent` entries: stable ID,
Chinese/English name and description, logical file bytes, required/selected state,
and the installed receipt baseline. The view uses a compact checkbox list for
Install, Upgrade and Modify. Required-only packages hide the list; Repair and
Recover never edit the selection. Component/maintenance labels currently use
built-in bilingual text; component names/descriptions come from the package.

Stable checkbox IDs retain keyboard focus across selection renders. Required
items cannot be deselected. Selection changes clear confirmation; Start sends
the operation and complete selected ID list atomically, after which edits are
locked. The main area scrolls while the footer remains fixed. Selected bytes
exclude the separately disclosed `maintenance_bytes`; the host still checks
actual temporary/backup space.

The host sets `allow_modify` only for a verified same-build maintenance package.
Repair/Modify switching is local until Start and clears confirmation. Modify
uses receipt defaults, marks additions/removals, and disables a no-op Start.
`migration_details` and `migration_confirmation_required` add an independent
legacy-file-mapping consent; changing selection clears both confirmations.
Modify does not offer a post-success Launch action, because it does not also
repair existing missing application files.

`--smoke-seconds 3` uses the demo-only `run_smoke` entry point and closes after
three seconds. Diagnostic lines identify native window creation, actual adapter,
and the first view render call. These are startup observations, not a screenshot
or proof of successful frame presentation, visual quality or the GPU matrix.

The host must parse `--help`, `--version` and silent-mode arguments before calling
either run function. `UiError` reports initialization/window errors and caught
Rust unwinds; it cannot recover from process aborts or driver crashes. A GUI
subsystem host must display a non-GPUI native error prompt and useful next steps.
The smoke console output is not evidence that this product fallback is complete.

Window closing during an operation is deferred with an explanation; cancellation
must be requested explicitly. The native close action and the footer button use
the same current-state guard; neither action implicitly sends Cancel. Forced
process exit and backend loss still require
the coordinator/backend lifecycle policy from the project ADR.

Headless view tests cover configured labels/branding, PNG rendering input,
language fallback, input preservation, actual shortcut selection, controlled
uninstall, fixed-path upgrade confirmation, and explicit launch/result isolation. They
do not establish pixel appearance, native IME or screen-reader behavior. The
current supported behavior and limits are described in the
[compatibility contract](../../docs/compatibility.md). Presentation changes need
their own relevant evidence; source tests do not establish native visual results.

API sources: [GPUI Kit](https://github.com/longbridge/gpui-kit),
[installation guide](https://gpui-kit.com/docs/installation/),
[official example](https://github.com/longbridge/gpui-kit/tree/main/examples/hello_world).
