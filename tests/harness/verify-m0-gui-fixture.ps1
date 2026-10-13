<#
.SYNOPSIS
Verify a GUI fixture from independent resources and actual process evidence.
.DESCRIPTION
Never drives GUI. -Uninstall is an explicit isolated test cleanup after a
verified successful install. Unknown, missing evidence and timeouts retain the
fixture. No process is killed, no recursive deletion or retry is performed.
#>
[CmdletBinding()]
param(
    [switch]$PlanOnly,
    [string]$FixturePath,
    [string]$ReportPath,
    [ValidateSet('Succeeded', 'Cancelled', 'Failed')][string]$Expected,
    [string]$GuiProcessRecord,
    [switch]$Uninstall,
    [ValidateRange(30, 600)][int]$TimeoutSeconds = 120
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($PlanOnly) {
    [ordered]@{ schema_version = 1; status = 'PLAN_ONLY_NOT_EXECUTED'; required = @('FixturePath', 'ReportPath', 'Expected', 'GuiProcessRecord'); resource_checks = @('Pinned inputs/artifacts', 'Exited GUI process and closed output streams', 'Report/task/session identity', 'Snapshot and receipt', 'HKCU Registry32/64 and disabled shortcuts'); uninstall = 'Only after verified Succeeded; two new user sentinels must survive. Preserve all evidence on uncertainty.' } | ConvertTo-Json -Depth 4
    exit 0
}
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'PowerShell 7 is required.' }
foreach ($argument in @($FixturePath, $ReportPath, $Expected, $GuiProcessRecord)) { if (-not $argument) { throw 'Supply FixturePath, ReportPath, Expected and GuiProcessRecord.' } }
$fixture = [Text.UTF8Encoding]::new($false, $true).GetString([IO.File]::ReadAllBytes($FixturePath)) | ConvertFrom-Json
$script:RunRoot = [IO.Path]::GetFullPath($fixture.run_directory).TrimEnd('\', '/')
$expectedRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../target/m0-r6-gui-fixture')).TrimEnd('\', '/')
if (-not $script:RunRoot.StartsWith($expectedRoot + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Fixture is outside the GUI fixture root.' }
if ([IO.Path]::GetFullPath($FixturePath) -cne (Join-Path $script:RunRoot 'fixture.json')) { throw 'Fixture path and recorded run directory differ.' }
if ($fixture.schema_version -ne 1 -or $fixture.status -cne 'BUILT_NOT_EXECUTED' -or $fixture.app_id -cnotmatch '^dev\.avelin\.m0\.gui\.[0-9a-f]{32}$') { throw 'Unrecognized fixture identity.' }
$helperSource = Join-Path $script:RunRoot 'tools/windows-fixture-common.ps1'
# Reject reparse ancestors before loading even the pinned function definitions.
$pathCursor = $helperSource
while ($pathCursor) {
    if ((Test-Path -LiteralPath $pathCursor) -and ((Get-Item -LiteralPath $pathCursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Reparse point rejected: $pathCursor" }
    $parentDirectory = [IO.Directory]::GetParent($pathCursor)
    if (-not $parentDirectory) { break }
    $pathCursor = $parentDirectory.FullName
}
$helperInput = @($fixture.inputs | Where-Object path -CEQ $helperSource)
if ($helperInput.Count -ne 1 -or (Get-FileHash -LiteralPath $helperSource -Algorithm SHA256).Hash.ToLowerInvariant() -cne $helperInput[0].sha256) { throw 'Pinned helper hash differs.' }
$tokens = $null; $parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($helperSource, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw 'Pinned helper source does not parse.' }
$helperNames = @('Assert-True', 'Get-FullPath', 'Assert-ContainedPath', 'Write-TaskJson', 'Read-StrictUtf8Json', 'Get-FileSnapshot', 'Assert-ResourceSnapshot', 'Get-TestRegistryState', 'Assert-NoTestSystemResources', 'Assert-Receipt', 'Assert-CommittedReceiptCandidate', 'Add-TestUserFiles', 'Invoke-TaskProcess')
foreach ($name in $helperNames) {
    $definitions = @($ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }, $false))
    if ($definitions.Count -ne 1) { throw "Expected one shared function: $name" }
    . ([scriptblock]::Create($definitions[0].Extent.Text))
}
$null = Assert-ContainedPath $script:RunRoot $expectedRoot
$script:ProjectDirectory = $fixture.project_directory
$script:RunId = $fixture.run_id
$script:TemporaryDirectory = Assert-ContainedPath $fixture.paths.temporary_directory $script:RunRoot
$script:LocalAppDataDirectory = Assert-ContainedPath $fixture.paths.local_app_data_directory $script:RunRoot
$script:TestAppId = $fixture.app_id; $script:TestVersion = $fixture.app_version
$script:TestRegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:TestAppId
$script:TestShortcutPaths = @($fixture.shortcut_paths)
$installDirectory = Assert-ContainedPath $fixture.paths.install_dir $script:RunRoot
$ReportPath = Assert-ContainedPath $ReportPath $script:RunRoot
$GuiProcessRecord = Assert-ContainedPath $GuiProcessRecord $script:RunRoot
Assert-True ($ReportPath -ceq $fixture.paths.report) 'Report is not this fixture explicit GUI report.'
foreach ($item in @($fixture.inputs) + @($fixture.artifacts)) {
    $path = Assert-ContainedPath $item.path $script:RunRoot
    Assert-True ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $item.sha256 -and (Get-Item -LiteralPath $path).Length -eq $item.size) "Pinned file changed: $path"
}
$snapshot = @(Read-StrictUtf8Json $fixture.paths.snapshot)
Assert-True (($snapshot | ConvertTo-Json -Depth 8 -Compress) -ceq ($fixture.expected_fixture_files | ConvertTo-Json -Depth 8 -Compress)) 'Independent snapshots differ.'

function Assert-NoProductLock {
    $lockDirectory = Assert-ContainedPath (Join-Path $script:LocalAppDataDirectory 'Avelin/M0/locks') $script:RunRoot
    if (Test-Path -LiteralPath $lockDirectory -PathType Container) {
        Assert-True (@(Get-ChildItem -LiteralPath $lockDirectory -Force | Where-Object Name -Like "$script:TestAppId-*.lock").Count -eq 0) 'The fixture product lock still exists.'
    }
}

function Assert-ReportIdentity([object]$Report, [string]$Path, [string]$Result, [int]$ProcessExitCode, [ValidateSet('install', 'uninstall')][string]$Operation = 'install', [object]$PreviousReceipt = $null) {
    Assert-True ($Report.result -cne 'Unknown') 'Unknown task outcome: retain the fixture; no cleanup or retry is allowed.'
    Assert-True ($Report.result -ceq $Result) "Expected $Result, observed $($Report.result)."
    $expectedCode = @{ Succeeded = 0; Cancelled = 2; Failed = 3 }[$Result]
    Assert-True ($Report.exit_code -eq $expectedCode -and $Report.coordinator_exit_code -eq $expectedCode -and $ProcessExitCode -eq $expectedCode) 'Backend, coordinator and observed process exit codes do not agree.'
    Assert-True (-not $Report.product_lock_retained) 'Task still retains its product lock.'
    Assert-True ((Get-FullPath $Report.install_dir) -ceq $installDirectory) 'Report installation path differs.'
    $taskId = [Guid]::Empty
    Assert-True ([Guid]::TryParse([string]$Report.task_id, [ref]$taskId)) 'Invalid report task ID.'
    $session = Assert-ContainedPath $Report.session_dir $script:RunRoot
    Assert-True ((Split-Path -Parent $session) -ceq (Split-Path -Parent $Path) -and (Split-Path -Leaf $session) -cmatch '^session-[0-9a-f]{32}$') 'Report session does not match the explicit-report layout.'
    $candidate = Read-StrictUtf8Json (Assert-ContainedPath (Join-Path $session 'receipt-candidate.json') $session)
    $plan = Read-StrictUtf8Json (Assert-ContainedPath (Join-Path $session 'install-plan.json') $session)
    $identity = Read-StrictUtf8Json (Assert-ContainedPath (Join-Path $session 'session.json') $session)
    Assert-True ($Report.operation -ceq $Operation -and $plan.task_id -ceq $Report.task_id -and $identity.task_id -ceq $Report.task_id -and $plan.operation -ceq $Operation -and $identity.operation -ceq $Operation) 'Plan/session task or operation differs from the report.'
    Assert-True ($candidate.app_id -ceq $script:TestAppId -and $candidate.app_version -ceq $script:TestVersion -and (Get-FullPath $candidate.install_dir) -ceq $installDirectory) 'Session candidate belongs to another fixture.'
    Assert-True ($plan.app_id -ceq $candidate.app_id -and $plan.app_version -ceq $candidate.app_version -and (Get-FullPath $plan.install_dir) -ceq $installDirectory -and $plan.user_sid -ceq $candidate.user_sid -and $plan.manifest_sha256 -ceq $candidate.manifest_sha256 -and $identity.manifest_sha256 -ceq $candidate.manifest_sha256) 'Plan/session product evidence differs from the candidate.'
    if ($Operation -ceq 'install') {
        Assert-True ($candidate.task_id -ceq $Report.task_id) 'Install candidate belongs to another task.'
    } else {
        Assert-True ($null -ne $PreviousReceipt -and $candidate.task_id -cne $Report.task_id -and ($candidate | ConvertTo-Json -Depth 30 -Compress) -ceq ($PreviousReceipt | ConvertTo-Json -Depth 30 -Compress)) 'Uninstall candidate differs from the independently verified installed receipt.'
    }
    Assert-True ($candidate.user_sid -ceq [Security.Principal.WindowsIdentity]::GetCurrent().User.Value) 'Session candidate belongs to a different Windows user.'
    Assert-True (($candidate.files | ConvertTo-Json -Depth 8 -Compress) -ceq ($snapshot | ConvertTo-Json -Depth 8 -Compress)) 'Session candidate resources differ from the independent fixture.'
    Assert-True (@($Report.events).Count -gt 0) 'Report contains no backend events.'
    foreach ($event in @($Report.events)) { Assert-True ($event.task_id -ceq $Report.task_id) 'Report mixes task identities.' }
    $finish = @($Report.events | Where-Object kind -CEQ 'finish')
    Assert-True ($finish.Count -eq 1 -and $finish[0].result -ceq $Result -and $finish[0].code -eq $expectedCode) 'Missing or conflicting backend finish evidence.'
    if ($Result -ne 'Succeeded') { Assert-True (@($Report.events | Where-Object { $_.PSObject.Properties['phase'] -and $_.phase -ceq 'cleanup_completed' }).Count -ge 1) 'No completed cleanup event.' }
    return $candidate
}

$verificationId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
$evidencePath = Join-Path $script:RunRoot ("verification-$verificationId.json")
$evidence = [ordered]@{ schema_version = 1; status = 'VERIFYING'; fixture_path = $FixturePath; fixture_sha256 = (Get-FileHash -LiteralPath $FixturePath -Algorithm SHA256).Hash.ToLowerInvariant(); verifier_sha256 = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash.ToLowerInvariant(); expected = $Expected; gui_process_record = $GuiProcessRecord; report_path = $ReportPath; uninstall_requested = [bool]$Uninstall; failure = $null }
try {
    $process = Read-StrictUtf8Json $GuiProcessRecord
    Assert-True ($process.status -ceq 'EXITED') 'GUI has not been observed exited. Preserve the fixture and finish observing the original process.'
    Assert-True ($process.observation_status -ceq 'EXIT_AND_EOF_OBSERVED' -and $process.logs_complete_at_eof -and $process.output_utf8_valid) 'GUI output evidence is incomplete or invalid UTF-8.'
    Assert-True ($process.process_id -gt 0 -and $process.started_at_utc) 'Missing GUI process identity.'
    Assert-True ((Get-FullPath $process.executable) -ceq $fixture.paths.runtime -and $process.executable_sha256 -ceq @($fixture.inputs | Where-Object path -CEQ $fixture.paths.runtime)[0].sha256) 'GUI executable differs from the pinned runtime.'
    $expectedArguments = @($fixture.gui_arguments)
    if ('--english' -in @($process.arguments)) { $expectedArguments += '--english' }
    Assert-True (($process.arguments | ConvertTo-Json -Compress) -ceq ($expectedArguments | ConvertTo-Json -Compress)) 'GUI argv differs from this fixture.'
    Assert-True ((Get-FullPath $process.temporary_directory) -ceq $script:TemporaryDirectory -and (Get-FullPath $process.local_app_data_directory) -ceq $script:LocalAppDataDirectory) 'GUI child environment differs from the fixture.'
    foreach ($path in @($process.stdout_path, $process.stderr_path)) { $null = Assert-ContainedPath $path $script:RunRoot; Assert-True (Test-Path -LiteralPath $path -PathType Leaf) 'GUI stream evidence is missing.' }
    $report = Read-StrictUtf8Json $ReportPath
    $candidate = Assert-ReportIdentity $report $ReportPath $Expected $process.exit_code
    Assert-NoProductLock
    Assert-True ((Get-Item -LiteralPath $ReportPath).LastWriteTimeUtc -ge [DateTime]::Parse($process.started_at_utc).ToUniversalTime()) 'Task report predates this GUI launch.'
    $stdoutReports = @()
    $stdout = [Text.UTF8Encoding]::new($false, $true).GetString([IO.File]::ReadAllBytes($process.stdout_path))
    foreach ($line in $stdout -split '\r?\n') {
        try { $value = $line | ConvertFrom-Json -ErrorAction Stop } catch { continue }
        if ($null -ne $value -and $value.PSObject.Properties['task_id'] -and $value.PSObject.Properties['result']) { $stdoutReports += $value }
    }
    Assert-True ($stdoutReports.Count -eq 1 -and ($stdoutReports[0] | ConvertTo-Json -Depth 40 -Compress) -ceq ($report | ConvertTo-Json -Depth 40 -Compress)) 'GUI stdout does not contain exactly this persisted task report.'
    if ($Expected -eq 'Succeeded') {
        Assert-ResourceSnapshot $installDirectory $snapshot
        $receipt = Assert-Receipt $installDirectory $snapshot
        Assert-CommittedReceiptCandidate $receipt $candidate ((Get-FileHash -LiteralPath $fixture.paths.backend -Algorithm SHA256).Hash.ToLowerInvariant())
        $resident = Assert-ContainedPath (Join-Path $installDirectory '.avelin/avelin-app.exe') $installDirectory
        $residentManifest = Assert-ContainedPath (Join-Path $installDirectory '.avelin/manifest.json') $installDirectory
        $uninstaller = Assert-ContainedPath (Join-Path $installDirectory 'uninstall.exe') $installDirectory
        Assert-True ((Get-FileHash -LiteralPath $resident -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $process.executable_sha256) 'Resident runtime differs from the pinned executable.'
        Assert-True ((Get-FileHash -LiteralPath $residentManifest -Algorithm SHA256).Hash -ceq (Get-FileHash -LiteralPath $fixture.paths.manifest -Algorithm SHA256).Hash) 'Resident manifest differs.'
        Assert-True (Test-Path -LiteralPath $uninstaller -PathType Leaf) 'Installed uninstaller is missing.'
        $expectedFiles = @($snapshot.path) + @('.avelin-receipt.json', '.avelin\avelin-app.exe', '.avelin\manifest.json', 'uninstall.exe')
        $actualFiles = @(Get-FileSnapshot $installDirectory)
        Assert-True ($actualFiles.Count -eq $expectedFiles.Count -and @($actualFiles | Where-Object path -NotIn $expectedFiles).Count -eq 0) 'Unexpected installed resources.'
        $registry = @(Get-TestRegistryState | Where-Object exists)
        Assert-True ($registry.Count -ge 1) 'Successful install has no HKCU uninstall registration.'
        $command = '"{0}" --gui --operation uninstall --backend "{1}" --manifest "{2}" --install-dir "{3}"' -f $resident, $uninstaller, $residentManifest, $installDirectory
        $quietCommand = '"{0}" --silent --operation uninstall --backend "{1}" --manifest "{2}" --install-dir "{3}"' -f $resident, $uninstaller, $residentManifest, $installDirectory
        foreach ($state in $registry) {
            Assert-True ($state.values['DisplayVersion'] -ceq $script:TestVersion -and (Get-FullPath $state.values['InstallLocation']) -ceq $installDirectory -and $state.values['UninstallString'] -ceq $command -and $state.values['QuietUninstallString'] -ceq $quietCommand) 'HKCU uninstall registration differs.'
        }
        foreach ($shortcut in $script:TestShortcutPaths) { Assert-True (-not (Test-Path -LiteralPath $shortcut)) 'Disabled shortcut was created.' }
        $evidence['receipt'] = $receipt
    }
    else {
        Assert-ResourceSnapshot $installDirectory @() -Exact
        Assert-NoTestSystemResources
        Assert-True (-not $Uninstall) 'Uninstall is only valid after verified Succeeded.'
    }
    $evidence['report'] = $report; $evidence['report_sha256'] = (Get-FileHash -LiteralPath $ReportPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $evidence['files_after_gui'] = @(Get-FileSnapshot $installDirectory)
    $evidence['registry_after_gui'] = @(Get-TestRegistryState)
    $evidence.status = 'GUI_TASK_RESOURCES_VERIFIED'
    Write-TaskJson $evidencePath $evidence
    if ($Uninstall) {
        # CreateNew blocks repeated cleanup after partial writes or uncertainty.
        $guard = Assert-ContainedPath (Join-Path $script:RunRoot 'uninstall-attempt.guard') $script:RunRoot
        $guardStream = [IO.File]::Open($guard, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        $guardStream.Dispose()
        $sentinels = @(Add-TestUserFiles $installDirectory)
        Write-TaskJson (Join-Path $script:RunRoot 'uninstall-sentinels.json') $sentinels
        $uninstallReportPath = Join-Path $script:RunRoot 'cli-uninstall.report.json'
        Assert-True (-not (Test-Path -LiteralPath $uninstallReportPath)) 'Uninstall report already exists.'
        $uninstallProcess = Invoke-TaskProcess $fixture.paths.runtime @('--silent', '--backend', (Join-Path $installDirectory 'uninstall.exe'), '--manifest', $fixture.paths.manifest, '--install-dir', $installDirectory, '--operation', 'uninstall', '--report', $uninstallReportPath) 'cli-uninstall'
        $uninstallReport = Read-StrictUtf8Json $uninstallReportPath
        $null = Assert-ReportIdentity $uninstallReport $uninstallReportPath 'Succeeded' $uninstallProcess.exit_code 'uninstall' $receipt
        Assert-True ($uninstallReport.task_id -cne $report.task_id) 'Uninstall reused the install task ID.'
        Assert-ResourceSnapshot $installDirectory $sentinels -Exact
        Assert-NoTestSystemResources
        Assert-NoProductLock
        $evidence['uninstall'] = @{ process = $uninstallProcess; report = $uninstallReport; report_path = $uninstallReportPath; report_sha256 = (Get-FileHash -LiteralPath $uninstallReportPath -Algorithm SHA256).Hash.ToLowerInvariant(); sentinels = $sentinels; files_after = @(Get-FileSnapshot $installDirectory); registry_after = @(Get-TestRegistryState) }
        $evidence.status = 'GUI_TASK_AND_CLI_UNINSTALL_RESOURCES_VERIFIED'
    }
}
catch { $evidence.status = 'VERIFICATION_FAILED_SCENE_RETAINED'; $evidence.failure = $_.Exception.Message; throw }
finally { Write-TaskJson $evidencePath $evidence }
[ordered]@{ status = $evidence.status; evidence_path = $evidencePath; fixture_path = $FixturePath; note = 'Resource verification does not itself prove visual or interaction acceptance.' } | ConvertTo-Json -Depth 4
