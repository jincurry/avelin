# Focused cross-version checks for install mode, PlanOnly, JSON and native argv.
# Runs only a bounded PowerShell child that echoes arguments; never an installer.
[CmdletBinding()]
param([string]$OutputDirectory)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$harnessPath = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../run-m0-compatibility.ps1'))
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($harnessPath, [ref]$tokens, [ref]$errors)
if (@($errors).Count -gt 0) { throw 'Guest harness has syntax errors.' }
foreach ($name in @('Assert-True', 'Get-SafePath', 'Read-Json', 'Write-Json', 'Quote-Argument', 'Get-InstallArguments', 'Invoke-Probe')) {
    $definition = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }.GetNewClosure(), $false)
    if (-not $definition) { throw "Function missing: $name" }
    . ([scriptblock]::Create($definition.Extent.Text))
}
$utf8 = New-Object Text.UTF8Encoding($false)
if (-not $OutputDirectory) { $OutputDirectory = Join-Path ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..'))) ('target/m0-compat-script-tests/' + [Guid]::NewGuid().ToString('N')) }
$script:RunDirectory = Get-SafePath $OutputDirectory ''
if (Test-Path -LiteralPath $script:RunDirectory) { throw 'Test output directory already exists.' }
$null = New-Item -ItemType Directory -Path $script:RunDirectory -Force
$script:TempDirectory = Join-Path $script:RunDirectory 'temp'
$script:LocalDirectory = Join-Path $script:RunDirectory 'local-app-data'
$null = New-Item -ItemType Directory -Path $script:TempDirectory
$null = New-Item -ItemType Directory -Path $script:LocalDirectory
$unusedPlanResults = Join-Path $script:RunDirectory 'plan-only-must-not-create'
$silentPlan = (& $harnessPath -PlanOnly -ResultsRoot $unusedPlanResults) | ConvertFrom-Json
$guiPlan = (& $harnessPath -PlanOnly -GuiInstall -ResultsRoot $unusedPlanResults -OperationTimeoutSeconds 600) | ConvertFrom-Json
Assert-True ($silentPlan.status -ceq 'PLAN_ONLY_NOT_EXECUTED' -and $silentPlan.gui_install -eq $false -and $silentPlan.install_mode -ceq 'SILENT' -and $silentPlan.operation_timeout_seconds -eq 180) 'Default PlanOnly changed install mode or timeout.'
Assert-True ($guiPlan.status -ceq 'PLAN_ONLY_NOT_EXECUTED' -and $guiPlan.gui_install -eq $true -and $guiPlan.install_mode -ceq 'GUI_MANUAL_START_AND_CLOSE' -and $guiPlan.operation_timeout_seconds -eq 600) 'GUI PlanOnly lost mode or custom timeout.'
foreach ($plan in @($silentPlan, $guiPlan)) { Assert-True ($plan.uninstall_mode -ceq 'VALIDATED_RESIDENT_CLI' -and $plan.manual_acceptance -ceq 'PENDING') 'Plan incorrectly certified GUI or changed uninstall mode.' }
Assert-True (-not (Test-Path -LiteralPath $unusedPlanResults)) 'PlanOnly created a result directory.'
Write-Json 'plan-silent.json' $silentPlan
Write-Json 'plan-gui.json' $guiPlan
$fixtureInstall = 'C:\fixture with space\install'
$fixtureReport = 'C:\fixture with space\report.json'
$expectedSilent = @('--operation', 'install', '--install-dir', $fixtureInstall, '--report', $fixtureReport, '--observation-timeout-secs', '165')
$silentArguments = @(Get-InstallArguments $fixtureInstall $fixtureReport 180 $false)
Assert-True ($silentArguments.Count -eq $expectedSilent.Count) 'Default install argument count changed.'
for ($index = 0; $index -lt $expectedSilent.Count; $index++) { Assert-True ($silentArguments[$index] -ceq $expectedSilent[$index]) "Default install argument changed: $index" }
$expectedGui = @('--operation', 'install', '--install-dir', $fixtureInstall, '--report', $fixtureReport, '--observation-timeout-secs', '585', '--gui')
$guiArguments = @(Get-InstallArguments $fixtureInstall $fixtureReport 600 $true)
Assert-True ($guiArguments.Count -eq $expectedGui.Count) 'GUI install argument count changed.'
for ($index = 0; $index -lt $expectedGui.Count; $index++) { Assert-True ($guiArguments[$index] -ceq $expectedGui[$index]) "GUI install argument changed: $index" }
$jsonPath = Join-Path $script:RunDirectory 'array.json'
[IO.File]::WriteAllText($jsonPath, '[{"path":"a"},{"path":"b"},{"path":"c"}]', $utf8)
$parsed = @(Read-Json $jsonPath)
Assert-True ($parsed.Count -eq 3 -and $parsed[0].path -ceq 'a' -and $parsed[2].path -ceq 'c') 'Top-level JSON array did not enumerate consistently.'
$childPath = Join-Path $script:RunDirectory 'echo args.ps1'
$childSource = @'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
$args | ConvertTo-Json -Compress
'@
[IO.File]::WriteAllText($childPath, $childSource, $utf8)
$expected = @('plain', 'space value', 'C:\path with space\', 'embedded"quote', 'backslash\"quote', '$INSTDIR', '${X}', 'literal$(expression)', '')
$powershellPath = Join-Path $PSHOME 'powershell.exe'
if (-not (Test-Path -LiteralPath $powershellPath)) { $powershellPath = Join-Path $PSHOME 'pwsh.exe' }
$result = Invoke-Probe $powershellPath (@('-NoProfile', '-NonInteractive', '-File', $childPath) + $expected) 'argv' 15 ''
Assert-True ($result.exit_code -eq 0) 'Argument echo child failed.'
Assert-True ($result.gui_install -eq $false -and $result.logs_complete_at_eof -eq $true -and $result.process_was_killed -eq $false -and $null -eq $result.report_observed) 'Process completion metadata is incorrect.'
Assert-True ((Test-Path -LiteralPath $result.stdout_path -PathType Leaf) -and (Test-Path -LiteralPath $result.stderr_path -PathType Leaf)) 'Completed process log paths do not exist.'
$actual = @(Read-Json (Join-Path $script:RunDirectory 'argv.stdout.log'))
Assert-True ($actual.Count -eq $expected.Count) 'Native argv roundtrip changed the argument count.'
for ($index = 0; $index -lt $expected.Count; $index++) { Assert-True ($actual[$index] -ceq $expected[$index]) "Native argv roundtrip changed argument $index." }
Write-Json 'summary.json' ([ordered]@{ status = 'PASSED'; powershell_version = $PSVersionTable.PSVersion.ToString(); checks = @('Default and GUI PlanOnly mode/timeout without result creation', 'Manual acceptance remains pending and uninstall remains resident CLI', 'Default install argv unchanged and GUI adds only --gui', 'JSON array shape', 'Windows native argv quoting including empty and trailing backslash', 'EOF log metadata and no process termination'); installer_executed = $false; gui_executed = $false })
Write-Host "Script checks passed: $script:RunDirectory"
