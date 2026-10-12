[CmdletBinding()]
param([Parameter(Mandatory)][string]$OutputDirectory)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$harness = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../run-m0-release-scale.ps1'))
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($harness, [ref]$tokens, [ref]$errors)
if (@($errors).Count) { throw 'Harness parsing failed.' }
foreach ($name in @('Assert-R12', 'Resolve-R12Path', 'Read-R12Json', 'Assert-R12Inventory', 'Assert-R12NoLock', 'Get-R12EventMetrics', 'Assert-R12Report', 'Get-R12Interval', 'Get-R12Cancellation', 'Assert-R12FixtureSize', 'Get-R12Invocation', 'Assert-R13Entry', 'Assert-R13Operation', 'Assert-R13CancellationTarget')) {
    $definition = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }.GetNewClosure(), $false)
    if (-not $definition) { throw "Missing function: $name" }
    . ([scriptblock]::Create($definition.Extent.Text))
}
function Expect-Rejection([scriptblock]$Action, [string]$Case) {
    $rejected = $false
    try { & $Action } catch { $rejected = $true }
    Assert-R12 $rejected "Expected rejection: $Case"
}
$output = Resolve-R12Path $OutputDirectory
Assert-R12 (-not (Test-Path -LiteralPath $output)) 'Test output exists.'
$null = [IO.Directory]::CreateDirectory($output)
$script:LocalRoot = Join-Path $output 'synthetic-local-app-data'
$script:AppId = 'test.product'
Assert-R12NoLock
$lockRoot = Join-Path $script:LocalRoot 'Avelin/M0/locks'
$null = [IO.Directory]::CreateDirectory($lockRoot)
Assert-R12NoLock
[IO.File]::WriteAllText((Join-Path $lockRoot 'unrelated-product.lock'), 'unrelated')
Assert-R12NoLock
[IO.File]::WriteAllText((Join-Path $lockRoot 'test.product-test.lock'), 'synthetic retained lock')
Expect-Rejection { Assert-R12NoLock } 'product lock retained'
$planOutput = Join-Path $output 'plan-must-not-exist'
$parameters = @{ PayloadRoot = 'C:\not-read'; OutputRoot = $planOutput; CliPath = 'C:\cli-not-executed'; AppPath = 'C:\app-not-executed'; BridgePath = 'C:\bridge-not-read'; MakensisPath = 'C:\nsis-not-executed' }
$plan = (& $harness @parameters -PlanOnly -Repetitions 3 -CancelDuringLargeFile -CancelDuringSmallFiles -OperationTimeoutSeconds 2400) | ConvertFrom-Json
Assert-R12 ($plan.status -ceq 'PLAN_ONLY_NOT_EXECUTED' -and $plan.repetitions -eq 3 -and $plan.build_count -eq 1 -and $plan.observer_timeout_seconds -eq 2520) 'Plan changed repetition or timeout semantics.'
Assert-R12 ($plan.install_entry -ceq 'COMPLETE_SETUP_CLI' -and $plan.uninstall_entry -match 'NOT_RESIDENT' -and $plan.cancellation_entry -ceq 'PINNED_RUNTIME_PLUS_BUILT_BACKEND') 'Plan misrepresents operation entry points.'
Assert-R12 (-not (Test-Path -LiteralPath $planOutput)) 'PlanOnly wrote output.'
$strictPlan = (& $harness @parameters -PlanOnly -R13Acceptance -MaximumPayloadFiles 50000 -Repetitions 3 -CancelDuringSmallFiles -OperationTimeoutSeconds 1800 -BuildTimeoutSeconds 1800) | ConvertFrom-Json
Assert-R12 ($strictPlan.r13_acceptance -eq $true -and $strictPlan.maximum_payload_files -eq 10000 -and $strictPlan.observer_timeout_seconds -eq 1920 -and $strictPlan.entry_timeout_seconds -eq 30 -and $strictPlan.cancellation_targeting_required -eq $true) 'R13 cap, time limits or strict targeting are incorrect.'
Assert-R12 ($strictPlan.install_entry -ceq 'COMPLETE_SETUP_CLI' -and $strictPlan.cancellation_entry -ceq 'COMPLETE_SETUP_CLI' -and $strictPlan.uninstall_entry -ceq 'REGISTERED_RESIDENT_RUNTIME_AND_MANIFEST' -and $strictPlan.installed_entry_execution -ceq 'NIMBUS_NO_ARGUMENTS_EXACT_STDOUT') 'R13 plan misrepresents actual required entry points.'
Assert-R12 ($plan.maximum_payload_files -eq 50000 -and $plan.installed_entry_execution -ceq 'NOT_REQUESTED' -and $plan.cancellation_targeting_required -eq $false) 'Legacy plan defaults changed.'
$boundedPlan = (& $harness @parameters -PlanOnly -R13Acceptance -MaximumPayloadFiles 16) | ConvertFrom-Json
Assert-R12 ($boundedPlan.maximum_payload_files -eq 16) 'A narrower explicit fixture bound was lost.'
Assert-R12 (-not (Test-Path -LiteralPath $planOutput)) 'Strict PlanOnly wrote output.'
Assert-R12FixtureSize 10000 10000
Expect-Rejection { Assert-R12FixtureSize 10001 10000 } 'count includes entry and large file at strict upper boundary'
Expect-Rejection { Assert-R12FixtureSize 0 10000 } 'empty fixture'
$invocationArgs = @('C:\setup.exe', 'C:\pinned\app.exe', 'C:\build\backend.exe', 'C:\build\manifest.json', 'C:\installed path', 'C:\report.json', 1800)
$resident = Get-R12Invocation 'uninstall' $true @invocationArgs
$external = Get-R12Invocation 'uninstall' $false @invocationArgs
$setupCancel = Get-R12Invocation 'cancel' $true @invocationArgs 1234
$externalCancel = Get-R12Invocation 'cancel' $false @invocationArgs 1234
Assert-R12 ($resident.executable -ceq 'C:\installed path\.avelin\avelin-app.exe' -and $resident.arguments -contains 'C:\installed path\.avelin\manifest.json' -and $resident.arguments -contains 'C:\installed path\uninstall.exe') 'Resident uninstall does not use installed components.'
Assert-R12 ($external.executable -ceq 'C:\pinned\app.exe' -and $external.arguments -contains 'C:\build\manifest.json') 'Legacy uninstall routing changed.'
Assert-R12 ($setupCancel.executable -ceq 'C:\setup.exe' -and $setupCancel.arguments -notcontains '--backend' -and $setupCancel.arguments -notcontains '--manifest' -and $setupCancel.arguments -contains '1234') 'Strict cancellation bypasses the complete Setup or loses the delay.'
Assert-R12 ($externalCancel.executable -ceq 'C:\pinned\app.exe' -and $externalCancel.arguments -contains '--backend' -and $externalCancel.arguments -contains '--manifest') 'Legacy cancellation routing changed.'
Expect-Rejection { & $harness @parameters -PlanOnly -OperationTimeoutSeconds 3601 } 'runtime max 3600'
Expect-Rejection { Resolve-R12Path 'C:\fixture-escaped\file' 'C:\fixture' } 'prefix sibling boundary'
$a = [pscustomobject]@{ path = 'a'; size = 1; sha256 = 'same' }
Assert-R12Inventory @($a) @($a) -Exact
Expect-Rejection { Assert-R12Inventory @($a, $a) @($a) -Exact } 'duplicate files'
Expect-Rejection { Assert-R12Inventory @([pscustomobject]@{ path = 'a'; size = 2; sha256 = 'same' }) @($a) -Exact } 'size mismatch'
Expect-Rejection { Assert-R12Inventory @($a) @() -Exact } 'unexpected file'
function Progress([string]$Phase, [int]$Completed, [int]$Time, [int]$Sequence) {
    [pscustomobject]@{ kind = 'progress'; phase = $Phase; completed = $Completed; total = 3; task_id = 'test-task'; protocol_version = 1; seq = $Sequence; coordinator_received_elapsed_ms = $Time }
}
$finish = [pscustomobject]@{ kind = 'finish'; result = 'Cancelled'; code = 2; task_id = 'test-task'; protocol_version = 1; seq = 9; coordinator_received_elapsed_ms = 220 }
$events = @((Progress 'extracting' 0 0 2), (Progress 'running' 1 10 3), (Progress 'extracting' 1 100 4), (Progress 'running' 2 200 7), (Progress 'cleanup_completed' 2 210 8), $finish)
$report = [pscustomobject]@{ task_id = 'test-task'; backend_pid = 123; install_dir = 'C:\fixture'; result = 'Cancelled'; exit_code = 2; coordinator_exit_code = 2; product_lock_retained = $false; protocol_fault = $null; events = $events; cancellation = [pscustomobject]@{ requested_elapsed_ms = 150; acknowledged_elapsed_ms = 155; stopped_elapsed_ms = 201; cleanup_completed_elapsed_ms = 210 } }
$stdoutPath = Join-Path $output 'synthetic-stdout.json'
[IO.File]::WriteAllText($stdoutPath, ($report | ConvertTo-Json -Depth 30 -Compress), [Text.UTF8Encoding]::new($false))
$process = [pscustomobject]@{ status = 'EXITED'; exit_code = 2; logs_complete_at_eof = $true; process_was_killed = $false; live_process_identities_at_end = @(); stdout_path = $stdoutPath }
Assert-R12Report $report $process 'Cancelled' 'C:\fixture' 3
$process.process_was_killed = $true
Expect-Rejection { Assert-R12Report $report $process 'Cancelled' 'C:\fixture' 3 } 'forced termination'
$process.process_was_killed = $false
[IO.File]::WriteAllBytes($stdoutPath, [byte[]]@(0xc3, 0x28))
Expect-Rejection { Assert-R12Report $report $process 'Cancelled' 'C:\fixture' 3 } 'invalid UTF-8 stdout'
[IO.File]::WriteAllText($stdoutPath, '{"different":"report"}', [Text.UTF8Encoding]::new($false))
Expect-Rejection { Assert-R12Report $report $process 'Cancelled' 'C:\fixture' 3 } 'stdout differs from persisted report'
[IO.File]::WriteAllText($stdoutPath, ($report | ConvertTo-Json -Depth 30 -Compress), [Text.UTF8Encoding]::new($false))
function Expect-ReportRejection([string]$Case) {
    # Keep stdout equal so the intended invariant, rather than the equality gate, is exercised.
    [IO.File]::WriteAllText($stdoutPath, ($report | ConvertTo-Json -Depth 30 -Compress), [Text.UTF8Encoding]::new($false))
    Expect-Rejection { Assert-R12Report $report $process 'Cancelled' 'C:\fixture' 3 } $Case
}
$report.events = @($events + $finish)
Expect-ReportRejection 'duplicate finish'
$report.events = $events; $report.coordinator_exit_code = 4
Expect-ReportRejection 'coordinator unknown'
$report.coordinator_exit_code = 2; $process.logs_complete_at_eof = $false
Expect-ReportRejection 'incomplete EOF'
$process.logs_complete_at_eof = $true; $report.events[1].seq = 1
Expect-ReportRejection 'sequence reversal'
$report.events[1].seq = 3; $report.events[1].task_id = 'foreign'
Expect-ReportRejection 'foreign task'
$report.events[1].task_id = 'test-task'
$large = Get-R12Cancellation $report 1 @(1, 2) 'LARGE_FILE' 150
$small = Get-R12Cancellation $report 1 @(1, 2) 'SMALL_FILES' 150
Assert-R12 ($large.status -ceq 'OBSERVED_REQUEST_IN_TARGET_STAGE' -and $small.status -ceq 'OBSERVED_REQUEST_IN_TARGET_STAGE' -and $small.request_inside_exact_target_file_interval -eq $true -and $large.request_to_cleanup_completed_ms -eq 60) 'Cancellation targeting/timing calculation failed.'
Assert-R13CancellationTarget $large
$report.cancellation.requested_elapsed_ms = 200; $report.cancellation.acknowledged_elapsed_ms = 200
$miss = Get-R12Cancellation $report 1 @(1, 2) 'LARGE_FILE' 150
Assert-R12 ($miss.status -ceq 'NOT_OBSERVED') 'An interval boundary was incorrectly accepted as a mid-file observation.'
Expect-Rejection { Assert-R13CancellationTarget $miss } 'strict targeting rejects functional cancellation that misses requested phase'
$report.cancellation.stopped_elapsed_ms = 199
Expect-Rejection { Get-R12Cancellation $report 1 @(1, 2) 'LARGE_FILE' 150 } 'out-of-order cancellation'
$metricEvents = @((Progress 'running' 0 0 2), (Progress 'running' 1 999 3), (Progress 'running' 2 1000 4), (Progress 'running' 3 1999 5))
$metrics = Get-R12EventMetrics $metricEvents
Assert-R12 ($metrics.maximum_recorded_events_in_1000ms_window -eq 2 -and $metrics.recorded_event_count -eq 4) 'Half-open one-second sliding event window is incorrect.'
$entryStdout = Join-Path $output 'synthetic-entry.stdout.log'
$entryStderr = Join-Path $output 'synthetic-entry.stderr.log'
$entryText = "Nimbus Notes — Avelin M0 sample application`r`nThis executable is a local installation test payload.`r`n"
[IO.File]::WriteAllText($entryStdout, $entryText, [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText($entryStderr, '', [Text.UTF8Encoding]::new($false))
$entryProcess = [pscustomobject]@{ status = 'EXITED'; exit_code = 0; logs_complete_at_eof = $true; process_was_killed = $false; live_process_identities_at_end = @(); observation_errors = @(); stdout_path = $entryStdout; stderr_path = $entryStderr }
Assert-R13Entry $entryProcess
[IO.File]::WriteAllText($entryStdout, 'wrong successful executable', [Text.UTF8Encoding]::new($false))
Expect-Rejection { Assert-R13Entry $entryProcess } 'entry zero exit with wrong output'
[IO.File]::WriteAllText($entryStdout, $entryText, [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText($entryStderr, 'unexpected error', [Text.UTF8Encoding]::new($false))
Expect-Rejection { Assert-R13Entry $entryProcess } 'entry stderr'
[IO.File]::WriteAllText($entryStderr, '', [Text.UTF8Encoding]::new($false))
$entryProcess.exit_code = 1
Expect-Rejection { Assert-R13Entry $entryProcess } 'entry nonzero exit'
$strictProcess = [pscustomobject]@{ observation_errors = @(); observed_processes = @([pscustomobject]@{ pid = 123; terminal_verified = $true; exit_code = 2 }) }
Assert-R13Operation $report $strictProcess
$strictProcess.observed_processes[0].terminal_verified = $false
Expect-Rejection { Assert-R13Operation $report $strictProcess } 'backend identity lacks natural terminal verification'
$strictProcess.observed_processes[0].terminal_verified = $true; $strictProcess.observed_processes[0].exit_code = 0
Expect-Rejection { Assert-R13Operation $report $strictProcess } 'backend native exit conflicts with report'
$strictProcess.observed_processes[0].exit_code = 2; $strictProcess.observation_errors = @('synthetic observation failure')
Expect-Rejection { Assert-R13Operation $report $strictProcess } 'strict operation observation error'
$summary = [ordered]@{ status = 'PASSED'; harness_sha256 = (Get-FileHash -LiteralPath $harness -Algorithm SHA256).Hash.ToLowerInvariant(); powershell_version = $PSVersionTable.PSVersion.ToString(); installer_executed = $false; checks = @('PlanOnly no side effects and distinct entry semantics', 'Runtime bound and path boundary rejection', 'Absent/empty/unrelated product locks and retained-lock rejection', 'Independent inventory size/duplicate/exact rejection', 'Terminal result, unique finish, ordered sequence/task, strict UTF-8 stdout/report equality and EOF rejection', 'Honest cancellation interval hits/misses and timing order', 'Half-open sliding recorded-event rate', 'R13 PlanOnly bounds at 10000 total payload files and narrower requested bounds', 'Actual resident and complete-Setup cancellation invocation routing with legacy defaults preserved', 'Strict cancellation miss rejected', 'Installed Nimbus exact UTF-8 stdout, stderr and exit contract', 'Reported backend natural process identity and exit-code agreement') }
[IO.File]::WriteAllText((Join-Path $output 'summary.json'), ($summary | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
Write-Host "Release scale harness checks passed: $output"
