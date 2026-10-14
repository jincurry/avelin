#requires -Version 7.2
<#
.SYNOPSIS
Project private Rust CI evidence into a public diagnostic summary.
.DESCRIPTION
Reads only bounded regular files below the selected run directory. Public text
comes from fixed enums; errors and stderr become classified reason codes, never
copied or redacted free text. Paths, identities, arguments and raw logs stay private.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$RunDirectory,
    [Parameter(Mandatory)][string]$OutputPath,
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-f]{40}$')][string]$Commit,
    [int]$ValidationExitCode = 1
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath($RunDirectory).TrimEnd([char]'\', [char]'/')
function SafeFile([string]$Relative) {
    $path = [IO.Path]::GetFullPath((Join-Path $root $Relative))
    if (-not $path.StartsWith($root + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { return $null }
    for ($cursor = $path; $cursor; $cursor = Split-Path -Parent $cursor) {
        if (Test-Path -LiteralPath $cursor) {
            if (((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return $null }
        }
    }
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        $file = Get-Item -LiteralPath $path
        if ($file.Length -le 16MB) { return $file }
    }
    return $null
}
function ReadJson([string]$Relative) {
    $file = SafeFile $Relative
    if ($file) {
        try {
            $value = Get-Content -LiteralPath $file.FullName -Raw -Encoding utf8 | ConvertFrom-Json -AsHashtable
            if ($value -is [Collections.IDictionary]) { return $value }
        } catch { }
    }
    return @{}
}
function SelectEnum($Value, [string[]]$Allowed) {
    foreach ($name in $Allowed) { if ($Value -is [string] -and $Value -ceq $name) { return $name } }
    return $null
}
function Number($Value) {
    $parsed = 0L
    if ($null -ne $Value -and [long]::TryParse([string]$Value, [ref]$parsed) -and $parsed -ge [int]::MinValue -and $parsed -le [uint32]::MaxValue) { return $parsed }
    return $null
}
function ReasonCodes($Value) {
    if ($Value -isnot [string]) { return @() }
    # No captured substrings are returned. Even unmatched secrets or user text
    # can only produce the literal unclassified_error code.
    $text = $Value.Substring(0, [Math]::Min($Value.Length, 16384))
    $patterns = [ordered]@{
        known_folder_unavailable = '(?i)Required current-user Desktop or Programs folder is unavailable'
        empty_path = '(?i)(empty string|path.{0,40}(empty|null)|value cannot be null)'
        access_denied = '(?i)(access.{0,8}denied|unauthorized|os error 5\b)'
        missing_path = '(?i)(cannot find|not found|does not exist|no such file|os error [23]\b)'
        process_exit_mismatch = '(?i)exited -?\d+, expected -?\d+'
        timeout = '(?i)(timed out|timeout)'
        shortcut = '(?i)(shortcut|known.folder|desktop|start.menu)'
        registry = '(?i)(registry|registration)'
        product_lock = '(?i)product.lock'
        recovery_evidence = '(?i)(recovery|retained.session|transaction.evidence)'
        user_identity = '(?i)(identity|user.sid|Windows.user)'
        repair = '(?i)repair'
        resource_conflict = '(?i)(conflict|already exists|existing resource)'
        integrity_mismatch = '(?i)(hash|digest|checksum|signature|mismatch|differs)'
        protocol = '(?i)(protocol|pipe|authenticated|terminal|handshake)'
        rollback = '(?i)(rollback|rolled.back|cleanup)'
        invalid_argument = '(?i)(cannot bind argument|invalid argument|invalid parameter)'
        path_validation = '(?i)(reparse|outside|escaped|path.{0,25}(unsafe|invalid))'
        io_error = '(?i)(I/O|os error|input/output|file in use|sharing violation)'
        source_assertion = '(?i)(assertion|panicked|test result: FAILED)'
    }
    $codes = @(foreach ($key in $patterns.Keys) { if ($text -match $patterns[$key]) { $key } })
    if ($codes.Count -eq 0 -and $text.Trim()) { return @('unclassified_error') }
    return $codes
}
$caseNames = @(
    'install-collision', 'cancel-install', 'fail-install', 'install-a', 'same-version',
    'upgrade-collision', 'modified-replacement', 'modified-selected-shortcut',
    'upgrade-before-commit-restores-a', 'upgrade-b', 'launch-b', 'downgrade',
    'uninstall-b-preserves-user-edits', 'reinstall-collision', 'reinstall-and-direct-uninstall',
    'post-commit-confirmation-loss-preserves-committed-b', 'incomplete-rollback-diagnosis-and-reentry-refusal'
)
$commandNames = @('rustup-installed', 'rustc-version', 'rustup-which-rustc', 'rustup-which-cargo', 'rustup-which-rustfmt', 'rust-targets', 'python-version', 'fmt', 'source-tests', 'host-release', 'collect-licenses', 'verify-licenses', 'package', 'verify-extracted-kit', 'rust-lifecycle', 'rust-interruption', 'verify-kit-after-acceptance')
$processNames = @($caseNames) + @('upgrade-before-commit', 'uninstall-b', 'reinstall-b', 'final-uninstall-bootstrap', 'after-commit-install', 'after-commit-upgrade', 'after-commit-uninstall', 'rollback-install', 'rollback-upgrade', 'rollback-reject-reentry', 'after-commit-diagnose', 'rollback-diagnose')
foreach ($label in @('native-a', 'native-b', 'after-commit-a', 'after-commit-b', 'rollback-a', 'rollback-b')) {
    $processNames += "check-$label", "build-$label"
}
$functionNames = @('Require', 'Full', 'SamePath', 'Inside', 'Hash', 'HashBytes', 'Json', 'Save', 'WriteNew', 'CopyNew', 'Snapshot', 'FileRecord', 'AssertFiles', 'RegistryState', 'ProductSnapshot', 'AssertUnchanged', 'InvokeProcess', 'ReadBytes', 'HashRegion', 'VerifyNativeBundle', 'NewProduct', 'BuildPackage', 'OperationReport', 'VerifyInstalled', 'RejectOperation', 'ArchiveOwned', 'ModifyShortcut', 'UninstallRegistered', 'FindSessionRecords', 'UninstallDirect', 'AddSentinels', 'RecoverableUpgradeFault', 'DiagnoseReadOnly', 'ReadJournal', 'VerifyPartialInstallCleanup', 'UnknownUpgradeFixture', '<ScriptBlock>')
$maintenanceCases = @(
    'recover-install-after-first-publish', 'recover-install-after-commit',
    'recover-upgrade-after-first-publish', 'recover-upgrade-after-commit',
    'recover-uninstall-after-first-remove', 'recover-uninstall-after-commit',
    'recover-cli-check-read-only', 'recover-reject-foreign-user', 'recover-reject-tampered-evidence',
    'recover-preserves-post-interruption-edit', 'repair-reject-pending-recovery', 'recover-interruption-retains-evidence',
    'repair-missing-resources-preserves-modified', 'repair-reject-wrong-manifest', 'repair-reject-wrong-version',
    'repair-explicit-overwrite-keeps-backup', 'repair-repeat-is-safe'
)
$commandNames += 'rust-maintenance', 'rust-optional-components', 'windows-maintenance', 'windows-features', 'rust-all-users', 'schema-dependencies', 'schema-m3', 'schema-report-encoding', 'schema-windows-features'
$functionNames += 'Pass', 'NewCase', 'Operation', 'UninstallArguments', 'WaitMaintenancePause', 'StopPausedBackend', 'Recover', 'AssertUnchangedState', 'LiveIdentitySnapshot', 'RecoveryCases', 'ShortcutSnapshot', 'RepairEvidence', 'RepairCases', 'StartFixtureProcess', 'FinishFixtureProcess', 'InvokeFixtureProcess', 'VerifyVersion', 'RegistrySnapshot', 'SameSnapshot'
$report = ReadJson 'summary.json'
$lifecycle = ReadJson 'lifecycle/summary.json'
$interruption = ReadJson 'interruption/summary.json'
$maintenance = ReadJson 'maintenance/summary.json'
$optionalComponents = ReadJson 'optional-components/summary.json'
$windowsMaintenance = ReadJson 'windows-maintenance/summary.json'
$windowsFeatures = ReadJson 'windows-features/summary.json'
$allUsers = ReadJson 'all-users/summary.json'
$package = ReadJson 'package/packaging-verification.json'
$public = [ordered]@{
    schema_version = 5; status = 'FAILED'; commit = $Commit; runner = 'windows-2025'; engine = 'rust'
    lifecycle_case_count = 0; interruption_case_count = 0; maintenance_case_count = 0; optional_component_case_count = 0; windows_maintenance_case_count = 0
    windows_feature_case_count = 0; windows_feature_status = $null; all_users_case_count = 0; all_users_status = $null; non_elevated_rejection_status = $null
    package_sha256 = $null; failed_command = $null; last_command = $null; last_command_status = $null
    completed_commands = 0; failure_category = 'preparation_or_invocation_failed'
    error_codes = @(ReasonCodes $report['failure']); user_shell_folders = @()
    lifecycle = [ordered]@{ status = $null; completed_cases = @(); next_uncompleted_case = $null; error_codes = @(ReasonCodes $lifecycle['failure']); failure_trace = @(); shortcut_checks = @(); last_process = $null; last_task = $null }
    maintenance = [ordered]@{ status = $null; completed_cases = @(); error_codes = @(ReasonCodes $maintenance['failure']); failure_trace = @(); last_process = $null; last_task = $null }
}
if ($ValidationExitCode -eq 0 -and $report['status'] -ceq 'PASSED') { $public.status = 'PASSED'; $public.failure_category = $null }
elseif ($report.Count) { $public.failure_category = 'verification_failed_after_command' }
$public.optional_component_case_count = @($optionalComponents['cases'] | Where-Object { $_ -is [Collections.IDictionary] -and $_['status'] -is [string] -and $_['status'] -ceq 'PASSED' }).Count
$public.windows_maintenance_case_count = @($windowsMaintenance['cases'] | Where-Object { $_ -is [Collections.IDictionary] -and $_['status'] -is [string] -and $_['status'] -ceq 'PASSED' }).Count
$public.windows_feature_status = SelectEnum $windowsFeatures['status'] @('RUNNING', 'PASSED', 'FAILED')
$public.windows_feature_case_count = @($windowsFeatures['cases'] | Where-Object { $_ -is [Collections.IDictionary] -and $_['status'] -is [string] -and $_['status'] -ceq 'PASSED' }).Count
$public.all_users_status = SelectEnum $allUsers['status'] @('RUNNING', 'PASSED', 'FAILED', 'NeedsElevation')
if ($public.all_users_status -cne 'NeedsElevation') {
    $public.all_users_case_count = @($allUsers['cases'] | Where-Object { $_ -is [Collections.IDictionary] -and $_['status'] -is [string] -and $_['status'] -ceq 'PASSED' }).Count
}
$denialCases = @($windowsFeatures['cases'] | Where-Object { $_ -is [Collections.IDictionary] -and $_['label'] -ceq 'all-users-silent-refuses-without-elevation' })
if ($denialCases.Count -eq 1) { $public.non_elevated_rejection_status = SelectEnum $denialCases[0]['status'] @('PASSED', 'FAILED', 'NeedsNonElevated') }
foreach ($command in @($report['commands'])) {
    if ($command -isnot [Collections.IDictionary]) { continue }
    $name = SelectEnum $command['name'] $commandNames
    if (-not $name) { continue }
    $status = SelectEnum $command['status'] @('STARTING', 'RUNNING', 'EXITED_ZERO', 'FAILED')
    $public.last_command = $name; $public.last_command_status = $status
    if ($status -ceq 'EXITED_ZERO') { $public.completed_commands++ }
    else { $public.failed_command = $name; $public.failure_category = 'command_failed' }
}
foreach ($folder in @($report['user_shell_folders'])) {
    if ($folder -isnot [Collections.IDictionary]) { continue }
    $name = SelectEnum $folder['folder'] @('DesktopDirectory', 'Programs')
    if ($name -and $folder['default_nonempty'] -is [bool] -and $folder['unverified_absolute'] -is [bool] -and $folder['exists'] -is [bool]) {
        $public.user_shell_folders += @{ folder = $name; default_nonempty = $folder['default_nonempty']; unverified_absolute = $folder['unverified_absolute']; exists = $folder['exists'] }
    }
}
if ($package['verified'] -eq $true -and $package['zip_sha256'] -is [string] -and $package['zip_sha256'] -cmatch '^[0-9a-f]{64}$') { $public.package_sha256 = $package['zip_sha256'] }
$public.lifecycle.status = SelectEnum $lifecycle['status'] @('RUNNING', 'PASSED', 'FAILED')
$public.lifecycle.completed_cases = @(foreach ($name in $caseNames) { if (@($lifecycle['completed_cases']) -ccontains $name) { $name } })
$public.lifecycle_case_count = $public.lifecycle.completed_cases.Count
$public.lifecycle.next_uncompleted_case = @($caseNames | Where-Object { $public.lifecycle.completed_cases -cnotcontains $_ } | Select-Object -First 1) | Select-Object -First 1
foreach ($trace in @($lifecycle['failure_trace'])) {
    if ($trace -isnot [Collections.IDictionary]) { continue }
    $name = SelectEnum $trace['function'] $functionNames; $line = Number $trace['line']
    if ($name -and $line -gt 0 -and $line -lt 10000) { $public.lifecycle.failure_trace += @{ function = $name; line = $line } }
}
$shortcutFields = @('target_matches', 'arguments_empty', 'working_directory_matches', 'target_empty', 'working_directory_empty', 'target_exists', 'working_directory_exists', 'target_contains_question_mark', 'working_directory_contains_question_mark', 'target_has_short_component')
foreach ($label in @('installed-a', 'upgraded-b', 'reinstalled-b', 'after-commit-installed', 'after-commit-installed-b', 'rollback-installed')) {
    foreach ($kind in @('desktop', 'start_menu')) {
        $name = "$label-$kind"
        $check = ReadJson "lifecycle/$name.shortcut-check.json"
        if ($check['kind'] -cne $kind) { continue }
        $projected = [ordered]@{ name = $name; kind = $kind }
        $valid = $true
        foreach ($field in $shortcutFields) {
            if ($check[$field] -isnot [bool]) { $valid = $false; break }
            $projected[$field] = $check[$field]
        }
        if ($valid) { $public.lifecycle.shortcut_checks += $projected }
    }
}
$latestProcess = $null
foreach ($name in $processNames) {
    $file = SafeFile "lifecycle/$name.process.json"
    if ($file -and ($null -eq $latestProcess -or $file.LastWriteTimeUtc -gt $latestProcess.file.LastWriteTimeUtc)) { $latestProcess = @{ name = $name; file = $file } }
}
if ($latestProcess) {
    $name = $latestProcess.name; $process = ReadJson "lifecycle/$name.process.json"
    $stderrCodes = @(); $stderr = SafeFile "lifecycle/$name.stderr.log"
    if ($stderr) { $stderrCodes = @(ReasonCodes (Get-Content -LiteralPath $stderr.FullName -TotalCount 8 | Out-String)) }
    $public.lifecycle.last_process = @{ name = $name; status = SelectEnum $process['status'] @('STARTING', 'RUNNING', 'EXITED', 'TIMED_OUT_PROCESS_NOT_KILLED'); exit_code = Number $process['exit_code']; stderr_codes = $stderrCodes }
}
$latestTask = $null
foreach ($name in $processNames) {
    $file = SafeFile "lifecycle/$name.report.json"
    if ($file -and ($null -eq $latestTask -or $file.LastWriteTimeUtc -gt $latestTask.file.LastWriteTimeUtc)) { $latestTask = @{ name = $name; file = $file } }
}
if ($latestTask) {
    $task = ReadJson "lifecycle/$($latestTask.name).report.json"
    $phase = $null
    foreach ($event in @($task['events'])) {
        if ($event -isnot [Collections.IDictionary]) { continue }
        $candidate = SelectEnum $event['phase'] @('preparing', 'extracting', 'running', 'finalizing', 'cancelling', 'cleanup_completed')
        if ($candidate) { $phase = $candidate }
    }
    $diagnostic = if ($task['diagnostic'] -is [Collections.IDictionary]) { $task['diagnostic'] } else { @{} }
    # A confirmed rollback can replace the displayed reason with a generic
    # localized message. Classify the retained worker detail separately, using
    # the same fixed codes; never export its free text, paths or identities.
    $public.lifecycle.last_task = @{ name = $latestTask.name; result = SelectEnum $task['result'] @('Succeeded', 'Failed', 'Cancelled', 'Unknown', 'NotStarted'); exit_code = Number $task['exit_code']; coordinator_exit_code = Number $task['coordinator_exit_code']; last_phase = $phase; diagnostic_code = SelectEnum $diagnostic['code'] @('AVELIN_TASK_FAILED', 'AVELIN_RESULT_UNKNOWN'); reason_codes = @(ReasonCodes $diagnostic['reason']); backend_reason_codes = @(ReasonCodes $task['backend_terminal_detail']) }
}
foreach ($point in @('prepared', 'after-first-publish', 'after-commit')) {
    foreach ($victim in @('backend', 'coordinator')) {
        $label = "$point-kill-$victim"
        if (@($interruption['cases'] | Where-Object { $_ -is [Collections.IDictionary] -and $_['label'] -ceq $label -and $_['status'] -ceq 'PASSED' }).Count -eq 1) { $public.interruption_case_count++ }
    }
}
$public.maintenance.status = SelectEnum $maintenance['status'] @('RUNNING', 'PASSED', 'FAILED')
foreach ($name in $maintenanceCases) {
    if (@($maintenance['cases'] | Where-Object { $_ -is [Collections.IDictionary] -and $_['label'] -ceq $name -and $_['status'] -ceq 'PASSED' }).Count -eq 1) { $public.maintenance.completed_cases += $name }
}
$public.maintenance_case_count = $public.maintenance.completed_cases.Count
foreach ($trace in @($maintenance['failure_trace'])) {
    if ($trace -isnot [Collections.IDictionary]) { continue }
    $name = SelectEnum $trace['function'] $functionNames; $line = Number $trace['line']
    if ($name -and $line -gt 0 -and $line -lt 10000) { $public.maintenance.failure_trace += @{ function = $name; line = $line } }
}
$maintenanceProcesses = @('build-a', 'build-b', 'install-a', 'interrupted', 'reject-active', 'check', 'cli-recovery-check', 'reject-foreign-user', 'reject-tampered-evidence', 'reject-post-interruption-edit', 'repair-reject-pending', 'interrupted-recovery', 'apply', 'apply-again', 'cleanup', 'build-different', 'build-different-manifest', 'build-newer', 'repair-preserve', 'repair-reject-wrong-manifest', 'repair-reject-wrong-version', 'repair-overwrite', 'repair-repeat')
$latestMaintenanceProcess = $null; $latestMaintenanceTask = $null
foreach ($case in @($maintenanceCases[0..5]) + @('repair')) {
    foreach ($name in $maintenanceProcesses) {
        $relative = "maintenance/$case/$name"
        $file = SafeFile "$relative.process.json"
        if ($file -and ($null -eq $latestMaintenanceProcess -or $file.LastWriteTimeUtc -gt $latestMaintenanceProcess.file.LastWriteTimeUtc)) { $latestMaintenanceProcess = @{ name = $name; case = $case; relative = $relative; file = $file } }
        $file = SafeFile "$relative.report.json"
        if ($file -and ($null -eq $latestMaintenanceTask -or $file.LastWriteTimeUtc -gt $latestMaintenanceTask.file.LastWriteTimeUtc)) { $latestMaintenanceTask = @{ name = $name; case = $case; relative = $relative; file = $file } }
    }
}
if ($latestMaintenanceProcess) {
    $latest = $latestMaintenanceProcess; $process = ReadJson "$($latest.relative).process.json"
    $stderrCodes = @(); $stderr = SafeFile "$($latest.relative).stderr.log"
    if ($stderr) { $stderrCodes = @(ReasonCodes (Get-Content -LiteralPath $stderr.FullName -TotalCount 8 | Out-String)) }
    $public.maintenance.last_process = @{ name = $latest.name; case = $latest.case; status = SelectEnum $process['status'] @('RUNNING', 'EXITED'); exit_code = Number $process['exit_code']; stderr_codes = $stderrCodes }
}
if ($latestMaintenanceTask) {
    $latest = $latestMaintenanceTask; $task = ReadJson "$($latest.relative).report.json"
    $diagnostic = if ($task['diagnostic'] -is [Collections.IDictionary]) { $task['diagnostic'] } else { @{} }
    $public.maintenance.last_task = @{ name = $latest.name; case = $latest.case; result = SelectEnum $task['result'] @('Succeeded', 'Failed', 'Cancelled', 'Unknown', 'NotStarted'); coordinator_exit_code = Number $task['coordinator_exit_code']; maintenance_status = SelectEnum $task['maintenance_status'] @('RecoveryChecked', 'RecoveryComplete', 'RepairComplete', 'RepairPreservedConflicts'); reason_codes = @(ReasonCodes $diagnostic['reason']); backend_reason_codes = @(ReasonCodes $task['backend_terminal_detail']) }
}
$output = [IO.Path]::GetFullPath($OutputPath)
if (Test-Path -LiteralPath $output) { throw 'Public output file already exists.' }
$null = [IO.Directory]::CreateDirectory((Split-Path -Parent $output))
$json = $public | ConvertTo-Json -Depth 12
[IO.File]::WriteAllText($output, $json, [Text.UTF8Encoding]::new($false))
Write-Output $json
