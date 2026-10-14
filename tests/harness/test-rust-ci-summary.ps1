#requires -Version 7.2
<# Offline summary projection tests. No installer, registry, network or build.
   Evidence is retained in a fresh target directory for inspection. #>
[CmdletBinding()]
param()
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$testRoot = Join-Path $projectRoot ('target/ci-summary-test-' + [Guid]::NewGuid().ToString('N'))
$null = [IO.Directory]::CreateDirectory($testRoot)
$summarizer = Join-Path $projectRoot 'scripts/summarize-rust-ci.ps1'
$privateMarker = 'PRIVATE_FIXTURE_TEXT_MUST_NOT_BE_EXPORTED'
$caseCount = 0
function SaveJson([string]$Path, $Value) {
    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $Path))
    [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 10), [Text.UTF8Encoding]::new($false))
}
function Check([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Project([string]$Name, $ComponentEvidence, [switch]$Absent, [switch]$Failed,
    [ValidateSet('optional-components', 'windows-maintenance', 'windows-features', 'all-users')][string]$EvidenceDirectory = 'optional-components') {
    $run = Join-Path $testRoot $Name
    $null = [IO.Directory]::CreateDirectory($run)
    $commandStatus = if ($Failed) { 'FAILED' } else { 'EXITED_ZERO' }
    $status = if ($Failed) { 'FAILED' } else { 'PASSED' }
    $commandName = switch ($EvidenceDirectory) {
        'windows-maintenance' { 'windows-maintenance' }
        'windows-features' { 'windows-features' }
        'all-users' { 'rust-all-users' }
        default { 'rust-optional-components' }
    }
    SaveJson (Join-Path $run 'summary.json') @{
        status = $status
        commands = @(@{ name = $commandName; status = $commandStatus; arguments = $privateMarker },
            @{ name = $privateMarker; status = 'FAILED'; arguments = $privateMarker })
        failure = $privateMarker
    }
    if (-not $Absent) { SaveJson (Join-Path $run "$EvidenceDirectory/summary.json") $ComponentEvidence }
    $output = Join-Path $run 'public.json'
    $exitCode = if ($Failed) { 1 } else { 0 }
    $null = & $summarizer -RunDirectory $run -OutputPath $output -Commit ('a' * 40) -ValidationExitCode $exitCode
    $text = [IO.File]::ReadAllText($output)
    Check (-not $text.Contains($privateMarker)) 'Private labels, paths, commands or details leaked into public summary'
    return ($text | ConvertFrom-Json -AsHashtable)
}
$missing = Project 'missing-evidence' $null -Absent
Check ($missing.optional_component_case_count -eq 0) 'Missing component evidence must produce zero cases'
Check ($missing.status -ceq 'PASSED') 'Successful parent verification status changed'
$caseCount++
$mixedEvidence = @{ status='PASSED'; cases=@(
    @{label='install-default';status='PASSED';detail=$privateMarker},
    @{label=$privateMarker;status='PASSED'},
    @{label='failure';status='FAILED'},
    @{label='wrong-case';status='passed'},
    @{label='invalid-type';status=@('PASSED')},
    @{label='no-status'},
    $null,
    $privateMarker,
    42
) }
$mixed = Project 'mixed-evidence' $mixedEvidence
Check ($mixed.optional_component_case_count -eq 2) 'Only object cases with exact string PASSED may count'
Check ($mixed.last_command -ceq 'rust-optional-components') 'Optional component CI command is not recognized'
Check ($mixed.completed_commands -eq 1) 'Successful optional component command was not counted'
$caseCount++
$malformed = Project 'non-object-evidence' @('not-a-summary', $privateMarker)
Check ($malformed.optional_component_case_count -eq 0) 'Non-object component evidence must produce zero cases'
$caseCount++
$failed = Project 'failed-run' $mixedEvidence -Failed
Check ($failed.status -ceq 'FAILED') 'Component rows must not turn a failed verification into success'
Check ($failed.optional_component_case_count -eq 2) 'Failed run must retain its valid completed case count'
Check ($failed.failed_command -ceq 'rust-optional-components') 'Failed optional component command is not identified'
Check ($failed.failure_category -ceq 'command_failed') 'Failed command category changed'
$caseCount++
$windowsMissing = Project 'windows-missing-evidence' $null -Absent -EvidenceDirectory 'windows-maintenance'
Check ($windowsMissing.windows_maintenance_case_count -eq 0) 'Missing Windows maintenance evidence must produce zero cases'
Check ($windowsMissing.last_command -ceq 'windows-maintenance') 'Windows maintenance command is not recognized'
Check ($windowsMissing.completed_commands -eq 1) 'Unknown command must not affect public command counts'
$caseCount++
$windowsEvidence = @{
    status = 'PASSED'
    cases = $mixedEvidence.cases
    components = @(@{ path = "C:\PrivateFixture\$privateMarker\tool.exe"; sha256 = $privateMarker })
    preflight = @{
        processes = @(@{ name = $privateMarker; executable = $privateMarker; pid = 12345 })
        locked_files = @(@{ path = $privateMarker; win32_error = 32 })
        issues = @(@{ scope = $privateMarker; message = $privateMarker })
    }
    failure = $privateMarker
    failure_trace = @(@{ function = $privateMarker; line = 123 })
    user_sid = $privateMarker
}
$windowsMixed = Project 'windows-mixed-evidence' $windowsEvidence -EvidenceDirectory 'windows-maintenance'
Check ($windowsMixed.windows_maintenance_case_count -eq 2) 'Only Windows maintenance object cases with exact string PASSED may count'
Check ($windowsMixed.optional_component_case_count -eq 0) 'Windows evidence must not populate optional-component counts'
Check (-not $windowsMixed.Contains('windows_maintenance') -and -not $windowsMixed.Contains('preflight')) 'Private Windows evidence objects must not be exposed'
$caseCount++
$windowsMalformed = Project 'windows-non-object-evidence' @('not-a-summary', $privateMarker) -EvidenceDirectory 'windows-maintenance'
Check ($windowsMalformed.windows_maintenance_case_count -eq 0) 'Malformed Windows maintenance evidence must produce zero cases'
$caseCount++
$windowsFailed = Project 'windows-failed-run' $windowsEvidence -Failed -EvidenceDirectory 'windows-maintenance'
Check ($windowsFailed.status -ceq 'FAILED') 'Windows maintenance rows must not turn failed verification into success'
Check ($windowsFailed.windows_maintenance_case_count -eq 2) 'Failed run must retain its completed Windows maintenance case count'
Check ($windowsFailed.failed_command -ceq 'windows-maintenance') 'Failed Windows maintenance command is not identified'
Check ($windowsFailed.failure_category -ceq 'command_failed') 'Failed Windows maintenance command category changed'
$caseCount++
$featuresMixed = Project 'features-mixed-evidence' $windowsEvidence -EvidenceDirectory 'windows-features'
Check ($featuresMixed.windows_feature_case_count -eq 2) 'Feature summary must count only exact PASSED rows'
Check ($featuresMixed.windows_feature_status -ceq 'PASSED') 'Feature stage status is absent'
Check ($featuresMixed.last_command -ceq 'windows-features') 'Feature command is not recognized'
Check ($featuresMixed.all_users_case_count -eq 0 -and $null -eq $featuresMixed.all_users_status) 'Missing machine evidence must not be interpreted as acceptance'
$caseCount++
$machineMissing = Project 'machine-needs-elevation' @{status='NeedsElevation';cases=@(@{label=$privateMarker;status='PASSED'})} -EvidenceDirectory 'all-users'
Check ($machineMissing.all_users_status -ceq 'NeedsElevation') 'Missing elevation status must be preserved'
Check ($machineMissing.all_users_case_count -eq 0) 'NeedsElevation must never count machine cases as passed'
$caseCount++
$machineMixed = Project 'machine-mixed-evidence' $windowsEvidence -EvidenceDirectory 'all-users'
Check ($machineMixed.all_users_case_count -eq 2) 'Machine summary must count only exact PASSED rows'
Check ($machineMixed.all_users_status -ceq 'PASSED') 'Machine stage status is absent'
Check ($machineMixed.last_command -ceq 'rust-all-users') 'Machine CI command is not recognized'
$caseCount++
$denialSkipped = Project 'feature-needs-non-elevated' @{status='PASSED';cases=@(@{label='all-users-silent-refuses-without-elevation';status='NeedsNonElevated';detail=$privateMarker})} -EvidenceDirectory 'windows-features'
Check ($denialSkipped.non_elevated_rejection_status -ceq 'NeedsNonElevated') 'Elevated token must not invent non-elevated refusal evidence'
Check ($denialSkipped.windows_feature_case_count -eq 0) 'Skipped non-elevated case must not count as passed'
$caseCount++
$machineFailed = Project 'machine-failed-run' $windowsEvidence -Failed -EvidenceDirectory 'all-users'
Check ($machineFailed.status -ceq 'FAILED' -and $machineFailed.failed_command -ceq 'rust-all-users') 'Machine failure must remain visible'
$caseCount++
# A localized rollback message must not hide the worker's useful error class.
# The worker detail is untrusted; only fixed enum codes may leave this fixture.
foreach ($section in @('lifecycle', 'maintenance')) {
    foreach ($variant in @('classified', 'missing', 'non-string')) {
        $run = Join-Path $testRoot "$section-backend-$variant"
        SaveJson (Join-Path $run 'summary.json') @{
            status='FAILED'; commands=@(); failure=$privateMarker
        }
        $task = @{
            result='Failed'; exit_code=3; coordinator_exit_code=3
            diagnostic=@{code='AVELIN_TASK_FAILED';reason='升级失败，已恢复旧安装。'}
            events=@(@{phase='cleanup_completed'})
        }
        if ($variant -ceq 'classified') {
            $task.backend_terminal_detail = "CreateFileW C:\PrivateFixture\$privateMarker failed (os error 32)"
        } elseif ($variant -ceq 'non-string') {
            $task.backend_terminal_detail = @{ message=$privateMarker; path="C:\PrivateFixture\$privateMarker" }
        }
        $relative = if ($section -ceq 'lifecycle') { 'lifecycle/after-commit-upgrade.report.json' } else { 'maintenance/repair/repair-preserve.report.json' }
        SaveJson (Join-Path $run $relative) $task
        $output = Join-Path $run 'public.json'
        $null = & $summarizer -RunDirectory $run -OutputPath $output -Commit ('b' * 40) -ValidationExitCode 1
        $text = [IO.File]::ReadAllText($output)
        Check (-not $text.Contains($privateMarker) -and -not $text.Contains('PrivateFixture') -and -not $text.Contains('CreateFileW')) 'Raw backend detail escaped the public projection'
        $public = $text | ConvertFrom-Json -AsHashtable
        Check ($public.status -ceq 'FAILED') 'Backend classification changed the failed outcome'
        $last = $public[$section].last_task
        Check ($null -ne $last -and $last.result -ceq 'Failed') 'Failed task was not projected'
        Check (($last.reason_codes -join ',') -ceq 'unclassified_error') 'Localized display reason unexpectedly replaced backend evidence'
        if ($variant -ceq 'classified') {
            Check (($last.backend_reason_codes -join ',') -ceq 'io_error') 'Worker I/O failure class was lost behind localized rollback text'
        } else {
            Check ($last.backend_reason_codes.Count -eq 0) 'Absent or malformed worker detail invented a failure class'
        }
        $caseCount++
    }
}
[ordered]@{ status='PASSED'; projection_cases=$caseCount; evidence=$testRoot } | ConvertTo-Json
