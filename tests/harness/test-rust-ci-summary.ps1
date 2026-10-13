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
    [ValidateSet('optional-components', 'windows-maintenance')][string]$EvidenceDirectory = 'optional-components') {
    $run = Join-Path $testRoot $Name
    $null = [IO.Directory]::CreateDirectory($run)
    $commandStatus = if ($Failed) { 'FAILED' } else { 'EXITED_ZERO' }
    $status = if ($Failed) { 'FAILED' } else { 'PASSED' }
    $commandName = if ($EvidenceDirectory -ceq 'windows-maintenance') { 'windows-maintenance' } else { 'rust-optional-components' }
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
[ordered]@{ status='PASSED'; projection_cases=$caseCount; evidence=$testRoot } | ConvertTo-Json
