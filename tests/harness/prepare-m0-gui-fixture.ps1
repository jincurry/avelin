<#
.SYNOPSIS
Build an isolated fixture for a separately observed GUI install; never launch it.
.DESCRIPTION
Requires PowerShell 7 and existing Release tools. PlanOnly writes nothing and
runs no process. The caller must use the recorded child environment and retain
the actual GUI process/output evidence for verify-m0-gui-fixture.ps1.
.PARAMETER FaultAfterFiles
Inject the existing NSIS write-stage fault after this many payload files.
Zero disables injection. A positive value must not exceed the fixture file count.
The operator must still start installation through the real GUI.
#>
[CmdletBinding()]
param(
    [switch]$PlanOnly,
    [string]$RuntimePath,
    [string]$CliPath,
    [string]$BridgePath,
    [string]$NsisPath,
    [ValidateRange(0, 10000)][int]$StepDelayMs = 0,
    [ValidateRange(0, 100000)][int]$FaultAfterFiles = 0,
    [ValidateRange(30, 600)][int]$TimeoutSeconds = 180
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($PlanOnly) {
    [ordered]@{
        schema_version = 1; status = 'PLAN_ONLY_NOT_EXECUTED'
        action = 'Build one unique fixture; do not launch GUI or install.'
        output = 'target/m0-r6-gui-fixture/<unique-id>/fixture.json'
        runtime_required = $true; step_delay_ms = $StepDelayMs
        fault_after_files = $FaultAfterFiles
        failure_injection = if ($FaultAfterFiles) { 'NSIS write-stage fault after the recorded file count; payload count is checked when preparing.' } else { 'None' }
        verification = 'Use the recorded argv/environment, capture the real GUI process exit and both output streams, then verify the explicit report.'
    } | ConvertTo-Json -Depth 4
    exit 0
}
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'PowerShell 7 is required.' }
if (-not $RuntimePath) { throw 'Supply an existing Release GUI -RuntimePath.' }
$script:ProjectDirectory = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$helperSource = Join-Path $PSScriptRoot 'run-m0.ps1'
# Import only reviewed function definitions, never the harness top-level code.
$tokens = $null; $parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($helperSource, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw 'Shared harness source does not parse.' }
$helperNames = @('Assert-True', 'Get-FullPath', 'Assert-ContainedPath', 'Write-TaskJson', 'Get-FileSnapshot', 'Assert-ResourceSnapshot', 'Get-TestRegistryState', 'Assert-NoTestSystemResources', 'Invoke-TaskProcess')
foreach ($name in $helperNames) {
    $definitions = @($ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }, $false))
    if ($definitions.Count -ne 1) { throw "Expected one shared function: $name" }
    . ([scriptblock]::Create($definitions[0].Extent.Text))
}
if (-not $CliPath) { $CliPath = Join-Path $script:ProjectDirectory 'target/release/avelin-cli.exe' }
if (-not $BridgePath) { $BridgePath = Join-Path $script:ProjectDirectory 'target/i686-pc-windows-msvc/release/avelin_nsis_bridge.dll' }
$RuntimePath = Assert-ContainedPath $RuntimePath $script:ProjectDirectory
$CliPath = Assert-ContainedPath $CliPath $script:ProjectDirectory
$BridgePath = Assert-ContainedPath $BridgePath $script:ProjectDirectory
foreach ($path in @($RuntimePath, $CliPath, $BridgePath)) { Assert-True (Test-Path -LiteralPath $path -PathType Leaf) "Missing tool: $path" }
if (-not $NsisPath) { $NsisPath = Join-Path ${env:ProgramFiles(x86)} 'NSIS/makensis.exe' }
$NsisPath = [IO.Path]::GetFullPath($NsisPath)
Assert-True (Test-Path -LiteralPath $NsisPath -PathType Leaf) "Missing existing NSIS compiler: $NsisPath"
$nsisHash = (Get-FileHash -LiteralPath $NsisPath -Algorithm SHA256).Hash.ToLowerInvariant()
$payloadSource = Assert-ContainedPath (Join-Path $script:ProjectDirectory 'examples/nimbus/payload') $script:ProjectDirectory
$sourceSnapshot = @(Get-FileSnapshot $payloadSource)
Assert-True ($FaultAfterFiles -le $sourceSnapshot.Count) 'FaultAfterFiles exceeds the payload file count; no fixture was created.'
Assert-True (@($sourceSnapshot | Where-Object path -CEQ 'nimbus-demo.exe').Count -eq 1) 'The payload must already contain the built nimbus-demo.exe.'
foreach ($relative in @('user-kept.txt', 'assets\user-created.txt')) { Assert-True (@($sourceSnapshot | Where-Object path -EQ $relative).Count -eq 0) "Reserved sentinel path in payload: $relative" }
$parent = Assert-ContainedPath (Join-Path $script:ProjectDirectory 'target/m0-r6-gui-fixture') $script:ProjectDirectory
$script:RunId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
$script:RunRoot = Assert-ContainedPath (Join-Path $parent $script:RunId) $parent
Assert-True (-not (Test-Path -LiteralPath $script:RunRoot)) 'Refusing to reuse a fixture directory.'
$null = New-Item -ItemType Directory -Path $script:RunRoot -Force
$paths = [ordered]@{}
foreach ($entry in @{
    runtime = 'tools/avelin-app.exe'; cli = 'tools/avelin-cli.exe'; bridge = 'tools/avelin_nsis_bridge.dll'
    helper_source = 'tools/run-m0.ps1'; payload = 'payload'; snapshot = 'fixture-snapshot.json'
    setup = 'Gui-Setup.exe'; backend = 'Gui-Setup.backend.exe'; manifest = 'Gui-Setup.manifest.json'
    install_dir = 'install-中文 space'; report = 'gui-install.report.json'
    temporary_directory = 'temp'; local_app_data_directory = 'local-app-data'
}.GetEnumerator()) { $paths[$entry.Key] = Assert-ContainedPath (Join-Path $script:RunRoot $entry.Value) $script:RunRoot }
foreach ($directory in @((Split-Path -Parent $paths.runtime), $paths.payload, $paths.install_dir, $paths.temporary_directory, $paths.local_app_data_directory)) { $null = New-Item -ItemType Directory -Path $directory -Force }
$inputs = @()
foreach ($pair in @(@($RuntimePath, $paths.runtime), @($CliPath, $paths.cli), @($BridgePath, $paths.bridge), @($helperSource, $paths.helper_source))) {
    Copy-Item -LiteralPath $pair[0] -Destination $pair[1]
    $sourceHash = (Get-FileHash -LiteralPath $pair[0] -Algorithm SHA256).Hash.ToLowerInvariant()
    Assert-True ((Get-FileHash -LiteralPath $pair[1] -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $sourceHash) 'Pinned input changed while copying.'
    $inputs += [pscustomobject]@{ source = $pair[0]; path = $pair[1]; sha256 = $sourceHash; size = (Get-Item -LiteralPath $pair[1]).Length }
}
foreach ($file in $sourceSnapshot) {
    $destination = Assert-ContainedPath (Join-Path $paths.payload $file.path) $paths.payload
    $null = New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force
    Copy-Item -LiteralPath (Join-Path $payloadSource $file.path) -Destination $destination
}
Assert-ResourceSnapshot $paths.payload $sourceSnapshot -Exact
Write-TaskJson $paths.snapshot $sourceSnapshot
$script:TemporaryDirectory = $paths.temporary_directory
$script:LocalAppDataDirectory = $paths.local_app_data_directory
$script:TestAppId = 'dev.avelin.m0.gui.' + [Guid]::NewGuid().ToString('N')
$script:TestVersion = '0.1.0'
$script:TestRegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:TestAppId
$script:TestShortcutPaths = @()
foreach ($folderName in @('Programs', 'StartMenu', 'DesktopDirectory')) {
    $folder = [Environment]::GetFolderPath($folderName)
    if ($folder) { $script:TestShortcutPaths += Join-Path $folder ("Avelin M0 {0}.lnk" -f $script:TestAppId) }
}
Assert-NoTestSystemResources
$configPath = Join-Path $script:RunRoot 'avelin.toml'
$config = @"
schema_version = 1
[app]
id = "$script:TestAppId"
name = "Avelin M0 GUI Fixture"
version = "$script:TestVersion"
entry = "nimbus-demo.exe"
[payload]
directory = "payload"
[install]
directory_name = "AvelinM0GuiFixture"
start_menu_shortcut = false
"@
[IO.File]::WriteAllText($configPath, $config, [Text.UTF8Encoding]::new($false))
$build = Invoke-TaskProcess $paths.cli @('build', '--config', $configPath, '--output', $paths.setup, '--runtime', $paths.runtime, '--bridge', $paths.bridge, '--nsis', $NsisPath) 'build'
Assert-True ($build.exit_code -eq 0) 'Fixture build failed; preserve the directory and inspect build logs.'
Assert-True ((Get-FileHash -LiteralPath $NsisPath -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $nsisHash) 'NSIS compiler changed during build.'
$manifest = Get-Content -LiteralPath $paths.manifest -Raw | ConvertFrom-Json
Assert-True ($manifest.app_id -ceq $script:TestAppId -and $manifest.app_version -ceq $script:TestVersion -and -not $manifest.start_menu_shortcut) 'Manifest identity or disabled-shortcut contract differs.'
Assert-True (($manifest.files | ConvertTo-Json -Depth 8 -Compress) -ceq ($sourceSnapshot | ConvertTo-Json -Depth 8 -Compress)) 'Manifest resources differ from the independent snapshot.'
$artifacts = @()
foreach ($path in @($paths.setup, $paths.backend, $paths.manifest, $paths.snapshot)) {
    $artifacts += [pscustomobject]@{ path = $path; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant(); size = (Get-Item -LiteralPath $path).Length }
}
$guiArguments = @('--gui', '--backend', $paths.backend, '--manifest', $paths.manifest, '--install-dir', $paths.install_dir, '--report', $paths.report, '--operation', 'install')
if ($StepDelayMs) { $guiArguments += @('--step-delay-ms', "$StepDelayMs") }
if ($FaultAfterFiles) { $guiArguments += @('--fault-after-files', "$FaultAfterFiles") }
$fixture = [ordered]@{
    schema_version = 1; status = 'BUILT_NOT_EXECUTED'; created_at_utc = [DateTime]::UtcNow.ToString('o')
    run_id = $script:RunId; run_directory = $script:RunRoot; project_directory = $script:ProjectDirectory
    app_id = $script:TestAppId; app_version = $script:TestVersion; registry_subkey = $script:TestRegistrySubkey
    failure_injection = @{ kind = if ($FaultAfterFiles) { 'NSIS_WRITE_STAGE_AFTER_FILES' } else { 'NONE' }; after_files = $FaultAfterFiles; expected_task_result = if ($FaultAfterFiles) { 'Failed' } else { $null }; automatic_cancellation = $false }
    shortcut_paths = $script:TestShortcutPaths; paths = $paths; gui_arguments = $guiArguments
    child_environment = @{ TEMP = $paths.temporary_directory; TMP = $paths.temporary_directory; LOCALAPPDATA = $paths.local_app_data_directory }
    source_fixture_directory = $payloadSource; expected_fixture_files = $sourceSnapshot
    inputs = $inputs; artifacts = $artifacts
    toolchain = @{ nsis_path = $NsisPath; nsis_sha256 = $nsisHash; powershell_version = $PSVersionTable.PSVersion.ToString() }
    harnesses = @(@($PSCommandPath, (Join-Path $PSScriptRoot 'verify-m0-gui-fixture.ps1')) | ForEach-Object { [pscustomobject]@{ path = $_; sha256 = (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash.ToLowerInvariant() } })
    baseline = @{ installed_files = @(Get-FileSnapshot $paths.install_dir); registry = @(Get-TestRegistryState) }
    build_process = $build; installation_executed = $false
    scope = 'Unique development-host fixture. GUI appearance and interaction need separate observation; resources are verified independently afterward.'
}
$fixturePath = Join-Path $script:RunRoot 'fixture.json'
Write-TaskJson $fixturePath $fixture
[ordered]@{ status = 'BUILT_NOT_EXECUTED'; fixture_path = $fixturePath; fixture_sha256 = (Get-FileHash -LiteralPath $fixturePath -Algorithm SHA256).Hash.ToLowerInvariant(); executable = $paths.runtime; arguments = $guiArguments; child_environment = $fixture.child_environment } | ConvertTo-Json -Depth 6
