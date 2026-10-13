# Upgrade configuration examples

These A/B configurations and payload text files illustrate a complete-package upgrade. Supply your own entry executables and branding before building; the fixed example product ID is for documentation only. Use a unique test ID and installation directory for actual acceptance.

| Resource | A → B |
|---|---|
| Entry | `nimbus-a.exe` → `bin/nimbus-b.exe` |
| `shared.txt` | Replaced with B bytes |
| `unchanged.txt` | Retained with identical bytes |
| `obsolete/retired.txt` | Removed when its ownership and hash still match |
| `retired-user.txt` | Preserved if modified after A installed it |
| `added.txt` | Added; existing conflicting content must be rejected |
| Unknown user files | Preserved throughout upgrade and uninstall |

Build each version using `avelin-cli build --config <configuration> --output <new-path>`. Installation requires matching Rust runtime and worker components.

The current [Rust lifecycle harness](../../tests/harness/run-rust-engine.ps1) generates its own isolated A/B inputs and unique product identities. It verifies installation, upgrade, resource conflicts, rollback and ownership-based uninstall. See the [development guide](../../docs/development.md) for execution and evidence scope.
