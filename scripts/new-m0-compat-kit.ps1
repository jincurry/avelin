<#
.SYNOPSIS
Build a portable, unsigned M0 compatibility kit; never run its installer.
.DESCRIPTION
Requires PowerShell 7 and existing Release binaries plus NSIS on the build
machine. Does not install tools or change policy. Copies fixed artifacts and
source snapshots into a new directory, builds a unique current-user fixture,
and hashes every kit file. Transfer the entire directory to a test guest.
The guest script runs with built-in 64-bit Windows PowerShell 5.1. Its PlanOnly
mode verifies kit hashes without creating files or starting processes.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$CliPath,
    [Parameter(Mandatory)][string]$RuntimePath,
    [Parameter(Mandatory)][string]$BridgePath,
    [Parameter(Mandatory)][string]$UiProbePath,
    [string]$EnvironmentProbePath,
    [string]$FixtureExePath,
    [string]$OutputDirectory,
    [string]$NsisPath,
    [ValidateRange(30, 900)][int]$BuildTimeoutSeconds = 180
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'Build the kit with PowerShell 7. The guest harness supports Windows PowerShell 5.1.' }
$project = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$utf8 = [Text.UTF8Encoding]::new($false)

function Safe-Path([string]$Path, [string]$Root) {
    $full = [IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    if ($Root) {
        $base = [IO.Path]::GetFullPath($Root).TrimEnd('\', '/')
        if (-not $full.StartsWith($base + '\', [StringComparison]::OrdinalIgnoreCase)) { throw "Path escapes kit: $full" }
    }
    $cursor = $full
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            if (((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Reparse point rejected: $cursor" }
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if (-not $parent) { break }
        $cursor = $parent.FullName
    }
    return $full
}

function Copy-New([string]$Source, [string]$Relative) {
    $from = Safe-Path $Source ''
    if (-not (Test-Path -LiteralPath $from -PathType Leaf)) { throw "Source file missing: $from" }
    $target = Safe-Path (Join-Path $script:KitDirectory $Relative) $script:KitDirectory
    if (Test-Path -LiteralPath $target) { throw "Refusing to overwrite: $target" }
    $null = New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force
    Copy-Item -LiteralPath $from -Destination $target
    if ((Get-FileHash -LiteralPath $from -Algorithm SHA256).Hash -ne (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash) { throw "Copy hash differs: $Relative" }
    return $target
}

function Write-NewJson([string]$Relative, [object]$Value) {
    $target = Safe-Path (Join-Path $script:KitDirectory $Relative) $script:KitDirectory
    if (Test-Path -LiteralPath $target) { throw "Refusing to overwrite: $target" }
    [IO.File]::WriteAllText($target, ($Value | ConvertTo-Json -Depth 30), $utf8)
}

function Get-Snapshot([string]$Directory) {
    $root = Safe-Path $Directory ''
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($root)
    $files = @()
    while ($pending.Count -gt 0) {
        foreach ($item in @(Get-ChildItem -LiteralPath $pending.Pop() -Force)) {
            $path = Safe-Path $item.FullName $root
            if ($item.PSIsContainer) { $pending.Push($path); continue }
            $files += [pscustomobject]@{ path = $path.Substring($root.Length + 1).Replace('\', '/'); size = $item.Length; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
        }
    }
    return @($files | Sort-Object path)
}

function Assert-Pe([string]$Path, [uint16]$Machine) {
    $reader = [IO.BinaryReader]::new([IO.File]::OpenRead((Safe-Path $Path '')))
    try {
        if ($reader.ReadUInt16() -ne 0x5a4d) { throw "Not a PE file: $Path" }
        $reader.BaseStream.Position = 0x3c
        $offset = $reader.ReadUInt32()
        $reader.BaseStream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne $Machine) { throw "Wrong PE machine: $Path" }
    }
    finally { $reader.Dispose() }
}

if (-not $FixtureExePath) { $FixtureExePath = Join-Path $project 'target/release/nimbus-demo.exe' }
if (-not $NsisPath) { $NsisPath = Join-Path ${env:ProgramFiles(x86)} 'NSIS/makensis.exe' }
foreach ($path in @($CliPath, $RuntimePath, $UiProbePath, $FixtureExePath)) { Assert-Pe $path 0x8664 }
Assert-Pe $BridgePath 0x014c
$NsisPath = Safe-Path $NsisPath ''
if (-not (Test-Path -LiteralPath $NsisPath -PathType Leaf)) { throw 'Supply an existing -NsisPath. No compiler installation was attempted.' }
$kitId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $project ('target/m0-compat-kit/' + $kitId) }
$script:KitDirectory = Safe-Path $OutputDirectory ''
if (Test-Path -LiteralPath $script:KitDirectory) { throw 'OutputDirectory already exists. Choose a new directory.' }
$null = New-Item -ItemType Directory -Path $script:KitDirectory -Force
$appId = 'dev.avelin.m0.compat.' + [Guid]::NewGuid().ToString('N')
$paths = [ordered]@{ cli = 'tools/avelin-cli.exe'; runtime = 'tools/avelin-app.exe'; bridge = 'tools/avelin_nsis_bridge.dll'; ui_probe = 'tools/ui-smoke.exe'; setup = 'Compat-Setup.exe'; manifest = 'Compat-Setup.manifest.json'; fixture_snapshot = 'fixture-snapshot.json'; guest_script = 'run-m0-compatibility.ps1'; environment_probe = $null }
$cli = Copy-New $CliPath $paths.cli
$runtime = Copy-New $RuntimePath $paths.runtime
$bridge = Copy-New $BridgePath $paths.bridge
$null = Copy-New $UiProbePath $paths.ui_probe
$null = Copy-New $FixtureExePath 'fixture/nimbus-demo.exe'
$null = Copy-New (Join-Path $project 'examples/nimbus/payload/hello.txt') 'fixture/hello.txt'
$null = Copy-New (Join-Path $project 'examples/nimbus/payload/assets/readme.txt') 'fixture/assets/readme.txt'
$null = Copy-New (Join-Path $project 'tests/harness/run-m0-compatibility.ps1') $paths.guest_script
if ($EnvironmentProbePath) {
    $paths.environment_probe = 'tools/collect-m0-environment.ps1'
    $null = Copy-New $EnvironmentProbePath $paths.environment_probe
}
foreach ($relative in @(
    'Cargo.lock', 'Cargo.toml', 'rust-toolchain.toml', '.cargo/config.toml',
    'scripts/new-m0-compat-kit.ps1',
    'apps/avelin-cli/src/builder.rs', 'apps/avelin-cli/src/bin/nimbus-demo.rs',
    'apps/avelin-app/Cargo.toml', 'apps/avelin-app/src/main.rs',
    'apps/avelin-app/src/coordinator.rs', 'apps/avelin-app/src/failure_report.rs',
    'apps/avelin-app/src/execution_files.rs', 'crates/avelin-core/src/lib.rs',
    'apps/avelin-app/src/task_diagnostics.rs',
    'apps/avelin-app/src/backend_events.rs', 'apps/avelin-app/src/transport.rs',
    'apps/avelin-app/src/runtime_log.rs',
    'crates/avelin-ui/Cargo.toml', 'crates/avelin-ui/src/lib.rs',
    'crates/avelin-ui/src/state.rs', 'crates/avelin-ui/src/interaction.rs',
    'crates/avelin-ui/src/clipboard.rs', 'crates/avelin-ui/src/clipboard_view_tests.rs',
    'crates/avelin-ui/examples/ui-smoke.rs', 'crates/avelin-ui/src/view.rs',
    'crates/avelin-nsis-bridge/src/lib.rs', 'packaging/nsis/m0-header.nsh',
    'tests/harness/prepare-m0-gui-fixture.ps1',
    'tests/harness/run-m0.ps1',
    'tests/harness/start-m0-ui-session.ps1',
    'tests/harness/verify-m0-gui-fixture.ps1',
    'tests/harness/test-m0-gui-fixture-options.ps1',
    'tests/harness/compatibility/test-compatibility-harness.ps1'
)) {
    $null = Copy-New (Join-Path $project $relative) ('source/' + $relative)
}
$fixtureSnapshot = @(Get-Snapshot (Join-Path $script:KitDirectory 'fixture'))
Write-NewJson $paths.fixture_snapshot $fixtureSnapshot
$config = @"
schema_version = 1
[app]
id = "$appId"
name = "Avelin M0 Compatibility Fixture"
version = "0.1.0"
entry = "nimbus-demo.exe"
[payload]
directory = "fixture"
[install]
directory_name = "AvelinM0Compatibility"
start_menu_shortcut = false
"@
$configPath = Join-Path $script:KitDirectory 'avelin.toml'
[IO.File]::WriteAllText($configPath, $config, $utf8)
$setupPath = Join-Path $script:KitDirectory $paths.setup
$start = [Diagnostics.ProcessStartInfo]::new()
$start.FileName = $cli
$start.WorkingDirectory = $script:KitDirectory
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$start.RedirectStandardOutput = $true
$start.RedirectStandardError = $true
foreach ($argument in @('build', '--config', $configPath, '--output', $setupPath, '--runtime', $runtime, '--bridge', $bridge, '--nsis', $NsisPath)) { $start.ArgumentList.Add($argument) }
$process = [Diagnostics.Process]::new()
$process.StartInfo = $start
$clock = [Diagnostics.Stopwatch]::StartNew()
$buildRecord = [ordered]@{ started_at_utc = [DateTime]::UtcNow.ToString('o'); status = 'STARTING'; profile = 'Caller-supplied Release binaries'; nsis_path = $NsisPath; nsis_sha256 = (Get-FileHash -LiteralPath $NsisPath -Algorithm SHA256).Hash.ToLowerInvariant(); source_snapshot_note = 'Source captured at kit assembly; this is provenance evidence, not proof of reproducible binary/source correspondence.'; installation_executed = $false }
try {
    $null = $process.Start()
    $buildRecord['pid'] = $process.Id
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    while (-not ($process.WaitForExit(250) -and $stdout.IsCompleted -and $stderr.IsCompleted)) {
        if ($clock.Elapsed.TotalSeconds -ge $BuildTimeoutSeconds) {
            $buildRecord.status = 'TIMED_OUT_PROCESS_NOT_KILLED'
            throw 'Kit builder timed out. No process was killed. Keep this failed directory and inspect processes before starting another build.'
        }
    }
    [IO.File]::WriteAllText((Join-Path $script:KitDirectory 'build.stdout.log'), $stdout.GetAwaiter().GetResult(), $utf8)
    [IO.File]::WriteAllText((Join-Path $script:KitDirectory 'build.stderr.log'), $stderr.GetAwaiter().GetResult(), $utf8)
    $buildRecord['exit_code'] = $process.ExitCode
    if ($process.ExitCode -ne 0) { throw 'Fixture packaging failed; see retained build logs.' }
    $buildRecord.status = 'BUILT_NOT_EXECUTED'
}
catch {
    if ($buildRecord.status -eq 'STARTING') { $buildRecord.status = 'FAILED' }
    $buildRecord['failure'] = $_.Exception.Message
    throw
}
finally { $buildRecord['wall_ms'] = $clock.ElapsedMilliseconds; Write-NewJson 'build-record.json' $buildRecord; $process.Dispose() }
Write-NewJson 'kit.json' ([ordered]@{ schema_version = 1; kit_id = $kitId; created_at_utc = [DateTime]::UtcNow.ToString('o'); app_id = $appId; app_version = '0.1.0'; signed = $false; paths = $paths; guest_powershell_minimum = '5.1 x64'; install_scope = 'current user, fresh isolated target'; fixture_file_count = $fixtureSnapshot.Count; installed_on_build_host = $false })
$readme = @'
# Avelin M0 portable compatibility kit (unsigned)

Copy the entire directory to a Windows x64 guest. Do not install Rust, NSIS,
Visual Studio or extra VC runtimes to make a failed probe work. Keep UAC,
Defender, SmartScreen and download-origin metadata unchanged. Use a trusted
clean-image identity and a non-elevated ordinary account for acceptance.

From built-in 64-bit Windows PowerShell 5.1, first verify without execution:

    .\run-m0-compatibility.ps1 -KitRoot $PWD.Path -PlanOnly

Use a native Windows PowerShell 5.1 session opened from the Windows Start menu.
Starting powershell.exe from a PowerShell 7 host can inherit that host's
PSModulePath and load an incompatible Core module. In the observed Codex host,
this selected Utility 7.0.0.0 and made Get-FileHash unavailable. This is a child
launch environment issue; the installer binaries and machine settings do not
need modification. To run the read-only PlanOnly check from PowerShell 7, use
an explicit child environment so Windows PowerShell builds its own module path:

    $kitPath = $PWD.Path
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
    $start.UseShellExecute = $false
    foreach ($value in @('-NoProfile', '-NonInteractive', '-File', (Join-Path $kitPath 'run-m0-compatibility.ps1'), '-KitRoot', $kitPath, '-PlanOnly')) { $start.ArgumentList.Add($value) }
    $null = $start.Environment.Remove('PSModulePath')
    $child = [Diagnostics.Process]::Start($start)
    $child.WaitForExit()
    $child.ExitCode

Require exit code 0 before proceeding. For an actual run from PowerShell 7,
replace '-PlanOnly' in that argument list with the ResultsRoot, ImageId and
EnvironmentKind arguments shown below; retain the child-only environment
removal. This changes neither the parent process nor user/machine environment,
execution policy or protection settings. The kit has no Explorer/.cmd launcher.

For the default automated chain, run against persistent guest storage outside
this immutable kit. Setup installation and resident uninstallation use CLI:

    .\run-m0-compatibility.ps1 -KitRoot $PWD.Path -ResultsRoot C:\AvelinEvidence -ImageId '<exact image/snapshot ID>' -EnvironmentKind CleanVm

For an observed GUI installation through the same standalone Setup, add
-GuiInstall and allow time for manual interaction:

    .\run-m0-compatibility.ps1 -KitRoot $PWD.Path -ResultsRoot C:\AvelinEvidence -ImageId '<exact image/snapshot ID>' -EnvironmentKind CleanVm -GuiInstall -OperationTimeoutSeconds 600

GUI mode adds --gui only to Setup installation. After the five window probes,
the harness prints the launched Setup PID, target and report path and writes
install.started.json. Keep that generated fixture directory unchanged. Confirm
the options and click Start, wait for Completed, then close the installer.
The harness does not click the GUI. After the window closes, it checks the
installed files, runs the sample and invokes the validated resident CLI
uninstall entry point. Uninstallation is not a GUI test. Capture GUI screenshots
and observations separately: summary.json still records manual acceptance as
pending even when all automated resource checks pass. A timeout retains the
process, evidence and guard; it never closes the window or kills a process.

Use a writable ResultsRoot for the current user. Use EnvironmentKind
DevelopmentHost for developer baselines; declaring CleanVm does not prove clean
image conditions. The script never changes
execution policy or removes download-origin metadata. If existing policy or
protection blocks execution, record that result and stop; do not bypass it.

Checksums detect changes but are not a signature or authentication. Confirm the
kit source and checksums.json SHA-256 using the separately retained host record.
Files under source/ are provenance snapshots, not a complete buildable checkout.
The R6 GUI helper snapshots require the full development repository and its
PowerShell 7/build tools; they are not additional guest entry points. Use the
root run-m0-compatibility.ps1 for this kit's guest workflow.
PlanOnly runs no EXE and writes no result directories. Add -GuiInstall to PlanOnly
to inspect that mode without launching it. Full execution opens five
three-second windows, then installs and uninstalls one isolated fixture. It
keeps user-data sentinels and raw logs. Timeouts stop subsequent steps, retain
evidence, and never kill the process. Do not rerun a failed/unknown installation
on the same application identity before reviewing its state. A CreateNew guard
under the original user LocalApplicationData/AvelinM0CompatibilityGuards blocks
repeat/concurrent runs of the same app ID across different ResultsRoot values.
Unconfirmed operations and timeouts keep the guard. It can be removed after
this run's confirmed uninstallation and independent resource checks, even if a
later evidence-writing step fails. No automatic
recovery or unlocking is provided.

Window-created/render-called markers are not an interactive first frame or a
visual correctness verdict. The logger summary must be present with zero errors
for a window probe to pass. Cold cache is not inferred from the first sample.
Software adapter, RDP, DPI, accessibility, protection, signing and clean-machine
acceptance still require their own observations and environment evidence.
'@
[IO.File]::WriteAllText((Join-Path $script:KitDirectory 'README.md'), $readme, $utf8)
Write-NewJson 'checksums.json' ([ordered]@{ schema_version = 1; algorithm = 'SHA-256'; self_excluded = 'checksums.json'; files = @(Get-Snapshot $script:KitDirectory) })
$checksumHash = (Get-FileHash -LiteralPath (Join-Path $script:KitDirectory 'checksums.json') -Algorithm SHA256).Hash.ToLowerInvariant()
[ordered]@{ status = 'BUILT_NOT_EXECUTED'; kit_directory = $script:KitDirectory; app_id = $appId; checksum_index_sha256 = $checksumHash; file_count = @(Get-Snapshot $script:KitDirectory).Count; installed_on_build_host = $false } | ConvertTo-Json -Depth 5
