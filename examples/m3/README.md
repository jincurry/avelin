# M3 normal upgrade fixture

`tests/harness/run-m3.ps1` copies these sources into a fresh directory, replaces the application ID and installation directory with unique values, and supplies the existing Nimbus sample executable plus `examples/nimbus/branding/nimbus.png`. It builds A with the supplied previous components and B with the supplied new components. It never installs the fixed example application ID.

| Resource | A → B |
|---|---|
| Entry | `nimbus-a.exe` → `bin/nimbus-b.exe`; the same small executable is used under different paths |
| `shared.txt` | Replaced with B bytes |
| `unchanged.txt` | Retained with identical bytes |
| `obsolete/retired.txt` | Removed; its now-empty `obsolete` directory is cleaned with nonrecursive directory removal |
| `retired-user.txt` | Modified by this fixture, then preserved when B stops owning it |
| `added.txt` | Added; an earlier test-owned collision must be rejected |
| Unknown root/subdirectory files | Preserved throughout upgrade and uninstall |
| Shortcuts | A selects both; B selects Start Menu only. A modified desktop link is preserved, and Start Menu is redirected to B's entry |

The harness checks same-version/downgrade/identity refusals, modified replacement/selected-link conflicts, and unknown file collisions without injecting execution failures. It snapshots installed files, registry values and link bytes before each refused operation. Test-created conflicts are moved to the evidence directory before continuing; nothing is recursively deleted.

The normal chain ends with B's actual registered quiet uninstall. Its retained evidence includes independent original/backup/staging hashes and journal ordering: all new resources staged and all old resources backed up before deletion, followed by registration and the complete receipt as the final published resource.

After that chain passes, `gui-fixture.json` supports `tests/harness/run-m3-gui.ps1 -FixturePath <path> -Operation Prepare|Upgrade|Finish`. `Prepare` installs A silently in a separate fresh directory. `Upgrade` opens one real B upgrade window, observes its process identity and requires explicit Launch evidence before the window is closed. `Finish` uses B's actual registered quiet uninstall and checks that the two user files remain. The helper does not click controls or declare visual acceptance.

Performance, signing, independent environments, repeated DPI checks and failure-injection recovery are outside this fixture.
