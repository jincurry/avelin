[CmdletBinding()]
param([string]$OutputRoot)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'M0.Performance.ps1')
if (-not $OutputRoot) {
    $project = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
    $OutputRoot = Join-Path $project ('target/m0-performance-tests/' + [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
}
$OutputRoot = [IO.Path]::GetFullPath($OutputRoot)
if (Test-Path -LiteralPath $OutputRoot) { throw 'Tests require a new output directory.' }
$null = [IO.Directory]::CreateDirectory($OutputRoot)
$pwsh = Join-Path $PSHOME 'pwsh.exe'
$cases = [Collections.Generic.List[object]]::new()
function Assert-True([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Write-Script([string]$Name, [string]$Text) {
    $path = Join-Path $OutputRoot $Name
    [IO.File]::WriteAllText($path, $Text, [Text.UTF8Encoding]::new($false))
    return $path
}
function Complete-Case([string]$Name) {
    $cases.Add([pscustomobject]@{ name = $Name; status = 'PASSED' })
    Write-Host "PASS $Name"
}
$summary = [ordered]@{ schema_version = 1; status = 'RUNNING'; output_root = $OutputRoot; cases = @(); failure = $null }
try {
    Initialize-M0PerformanceTypes
    $creation = [DateTime]::new(2026, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)
    $createFileTime = [uint64]$creation.ToFileTimeUtc()
    $exitFileTime = [uint64]$creation.AddSeconds(2).ToFileTimeUtc()
    $pending = [ordered]@{ start_ticks = $creation.Ticks; exit_ticks = $null; exited_at_utc = $null; exit_code = $null; exit_observed_ms = $null; cpu_time_ms = 0.0; terminal_verified = $false }
    # A non-signaled handle may already expose a final-looking exit code/times.
    # Neither it nor a zero/invalid signaled exit may finalize the row.
    $snapshot = [AvelinM0PerformanceV2.ProcessObservation]::Validate(258, $createFileTime, 0, 10000, 20000, 0, $creation.Ticks)
    Set-M0ProcessSnapshot -Row $pending -Snapshot $snapshot -ElapsedMs 1
    $snapshot = [AvelinM0PerformanceV2.ProcessObservation]::Validate(258, $createFileTime, $exitFileTime, 20000, 30000, 7, $creation.Ticks)
    Set-M0ProcessSnapshot -Row $pending -Snapshot $snapshot -ElapsedMs 2
    Assert-True (-not $pending.terminal_verified -and $null -eq $pending.exit_ticks -and $null -eq $pending.exit_code -and $null -eq $pending.exit_observed_ms -and $pending.cpu_time_ms -eq 5) 'An unsignaled process snapshot finalized the row.'
    $invalidSnapshots = @(
        @{ wait = [uint32]0; create = $createFileTime; exit = [uint64]0; kernel = [uint64]40000; user = [uint64]40000 },
        @{ wait = [uint32]0; create = $createFileTime; exit = ($createFileTime - [uint64]1); kernel = [uint64]40000; user = [uint64]40000 },
        @{ wait = [uint32]0; create = ($createFileTime + [uint64]1); exit = $exitFileTime; kernel = [uint64]40000; user = [uint64]40000 },
        @{ wait = [uint32]0; create = $createFileTime; exit = [uint64]::MaxValue; kernel = [uint64]40000; user = [uint64]40000 },
        @{ wait = [uint32]0; create = $createFileTime; exit = $exitFileTime; kernel = [uint64]::MaxValue; user = [uint64]40000 },
        @{ wait = [uint32]::MaxValue; create = $createFileTime; exit = $exitFileTime; kernel = [uint64]40000; user = [uint64]40000 }
    )
    foreach ($bad in $invalidSnapshots) {
        $rejectedSnapshot = $false
        try {
            $snapshot = [AvelinM0PerformanceV2.ProcessObservation]::Validate($bad.wait, $bad.create, $bad.exit, $bad.kernel, $bad.user, 0, $creation.Ticks)
            Set-M0ProcessSnapshot -Row $pending -Snapshot $snapshot -ElapsedMs 3
        } catch { $rejectedSnapshot = $true }
        Assert-True ($rejectedSnapshot -and -not $pending.terminal_verified -and $null -eq $pending.exit_ticks -and $null -eq $pending.exit_code -and $pending.cpu_time_ms -eq 5) 'An invalid native terminal snapshot modified final fields.'
    }
    $rejectedSnapshot = $false
    try {
        $snapshot = [AvelinM0PerformanceV2.ProcessObservation]::Validate(0, $createFileTime, $exitFileTime, 10000, 10000, 0, $creation.Ticks)
        Set-M0ProcessSnapshot -Row $pending -Snapshot $snapshot -ElapsedMs 4
    } catch { $rejectedSnapshot = $true }
    Assert-True ($rejectedSnapshot -and -not $pending.terminal_verified -and $null -eq $pending.exit_ticks -and $pending.cpu_time_ms -eq 5) 'Regressed final CPU was silently accepted.'
    $snapshot = [AvelinM0PerformanceV2.ProcessObservation]::Validate(0, $createFileTime, $exitFileTime, 50000, 60000, 259, $creation.Ticks)
    Set-M0ProcessSnapshot -Row $pending -Snapshot $snapshot -ElapsedMs 5
    Assert-True ($pending.terminal_verified -and $pending.exit_ticks -eq $creation.AddSeconds(2).Ticks -and $pending.exit_code -eq 259 -and $pending.cpu_time_ms -eq 11 -and $pending.exit_observed_ms -eq 5) 'Valid terminal snapshot failed to finalize times, CPU and code together.'
    Complete-Case 'native_terminal_validation_rejects_unsignaled_zero_invalid_times_and_cpu_regression'

    $data = Join-Path $OutputRoot 'isolated-data'; $null = [IO.Directory]::CreateDirectory($data)
    $basicScript = Write-Script 'arguments-child.ps1' @'
param([string]$Data, [string]$Text, [AllowEmptyString()][string]$Empty, [string]$TrailingSlash)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)
[IO.File]::WriteAllBytes((Join-Path $Data 'one.bin'),[byte[]]::new(2048))
[IO.File]::WriteAllBytes((Join-Path $Data 'two.bin'),[byte[]]::new(4096))
$allocation = [byte[]]::new(8MB)
for($i=0;$i -lt $allocation.Length;$i+=4096){$allocation[$i]=17}
$clock=[Diagnostics.Stopwatch]::StartNew(); $n=0L
while($clock.ElapsedMilliseconds -lt 350){$n+=1}
[Console]::Out.WriteLine(([ordered]@{text=$Text; empty=$Empty; slash=$TrailingSlash; cwd=[Environment]::CurrentDirectory; env=$env:AVELIN_PERF_TEST_MARKER; cpu_iterations=$n; cpu_ms_before_exit=[Diagnostics.Process]::GetCurrentProcess().TotalProcessorTime.TotalMilliseconds}|ConvertTo-Json -Compress))
[Console]::Error.WriteLine('stderr 完整 UTF-8')
Start-Sleep -Milliseconds 250
'@
    $markerBefore = [Environment]::GetEnvironmentVariable('AVELIN_PERF_TEST_MARKER')
    $quoted = 'space "quote" 中文'; $slash = 'C:\fixture with space\'
    $basic = Invoke-M0MeasuredProcess -Executable $pwsh -Arguments @('-NoProfile', '-NonInteractive', '-File', $basicScript, '-Data', $data, '-Text', $quoted, '-Empty', '', '-TrailingSlash', $slash) -WorkingDirectory $OutputRoot -Environment @{ AVELIN_PERF_TEST_MARKER = 'child-only' } -OutputPrefix (Join-Path $OutputRoot 'basic') -TimeoutSeconds 20 -SampleIntervalMs 50 -IsolatedRoot $OutputRoot -DirectoryTargets @{ fixture = $data; absent = (Join-Path $OutputRoot 'not-created') } -DirectorySampleIntervalMs 100
    Assert-True ($basic.status -ceq 'EXITED' -and $basic.exit_code -eq 0 -and $basic.logs_complete_at_eof -and -not $basic.process_was_killed) 'Basic process or EOF failed.'
    $stdout = Get-Content -LiteralPath $basic.stdout_path -Raw | ConvertFrom-Json
    Assert-True ($stdout.text -ceq $quoted -and $stdout.empty -ceq '' -and $stdout.slash -ceq $slash) 'Native argument roundtrip changed quotes/empty/backslash.'
    Assert-True ($stdout.env -ceq 'child-only' -and $stdout.cwd.TrimEnd('\') -ieq $OutputRoot.TrimEnd('\')) 'Child environment or working directory incorrect.'
    Assert-True ([Environment]::GetEnvironmentVariable('AVELIN_PERF_TEST_MARKER') -ceq $markerBefore) 'Child environment changed host environment.'
    $decoder = [Text.UTF8Encoding]::new($false, $true)
    Assert-True ($decoder.GetString([IO.File]::ReadAllBytes($basic.stderr_path)).Contains('stderr 完整 UTF-8')) 'stderr missing or not valid UTF-8.'
    Assert-True ($basic.observed_family_cpu_time_ms -gt 0 -and $basic.sampled_peak_family_working_set_bytes -gt 0 -and $basic.sampled_peak_family_private_bytes -gt 0) 'CPU/memory metrics were not observed.'
    Complete-Case 'native_arguments_environment_workdir_and_raw_eof'
    $sampleRows = @(Get-Content -LiteralPath $basic.samples_path -Raw | ConvertFrom-Json)
    Assert-True (@($basic.observed_processes | Where-Object { -not $_.terminal_verified -or $_.exit_ticks -lt $_.start_ticks -or $null -eq $_.exit_code }).Count -eq 0) 'A completed process lacks verified native terminal times.'
    $finalCpu = ($basic.observed_processes | Measure-Object cpu_time_ms -Sum).Sum
    Assert-True ([Math]::Abs($finalCpu - $basic.observed_family_cpu_time_ms) -lt 0.001 -and $basic.observed_processes[0].cpu_time_ms -ge $stdout.cpu_ms_before_exit -and $basic.root_process_lifetime_ms -ge 0) 'Final CPU sum, child CPU lower bound or root lifetime is invalid.'
    [double]$previousCpu = 0
    foreach ($sampleRow in $sampleRows) {
        Assert-True ($sampleRow.observed_family_cpu_time_ms -ge $previousCpu) 'Cumulative observed family CPU regressed.'
        $previousCpu = $sampleRow.observed_family_cpu_time_ms
    }
    Complete-Case 'native_creation_identity_terminal_cpu_and_family_sum_are_verified'
    $directorySample = @($sampleRows | Where-Object { $null -ne $_.directories } | Select-Object -Last 1)[0]
    Assert-True ($directorySample.directories.fixture.logical_file_bytes -eq 6144 -and $directorySample.directories.fixture.file_count -eq 2 -and $directorySample.directories.fixture.complete) 'Logical directory bytes/count incorrect.'
    Assert-True (-not $directorySample.directories.absent.exists -and $directorySample.directories.absent.logical_file_bytes -eq 0) 'Absent directory was not safely represented.'
    Assert-True ($sampleRows.Count -ge 2 -and @($sampleRows | Where-Object { $null -ne $_.actual_interval_ms -and $_.actual_interval_ms -gt 0 }).Count -gt 0 -and $directorySample.volumes.Count -eq 1) 'Actual intervals/volume observations missing.'
    Complete-Case 'scoped_directory_counts_intervals_and_volume_labels'

    $orphanMarker = Join-Path $OutputRoot 'descendant-completed.txt'
    $descendantScript = Write-Script 'descendant.ps1' @'
param([string]$Marker)
[Console]::Out.WriteLine('DESCENDANT_STARTED')
Start-Sleep -Milliseconds 4200
[IO.File]::WriteAllText($Marker,'natural completion')
[Console]::Out.WriteLine('DESCENDANT_EOF')
'@
    $parentScript = Write-Script 'short-parent.ps1' @'
param([string]$Pwsh,[string]$ChildScript,[string]$Marker)
$p=[Diagnostics.ProcessStartInfo]::new();$p.FileName=$Pwsh;$p.UseShellExecute=$false;$p.CreateNoWindow=$true
foreach($a in @('-NoProfile','-NonInteractive','-File',$ChildScript,'-Marker',$Marker)){$p.ArgumentList.Add($a)}
$child=[Diagnostics.Process]::Start($p)
[Console]::Out.WriteLine("CHILD_PID=$($child.Id)")
$child.Dispose()
exit 0
'@
    $family = Invoke-M0MeasuredProcess -Executable $pwsh -Arguments @('-NoProfile', '-NonInteractive', '-File', $parentScript, '-Pwsh', $pwsh, '-ChildScript', $descendantScript, '-Marker', $orphanMarker) -WorkingDirectory $OutputRoot -OutputPrefix (Join-Path $OutputRoot 'family') -TimeoutSeconds 30 -SampleIntervalMs 4000
    Assert-True ($family.status -ceq 'EXITED' -and $family.logs_complete_at_eof -and $family.observed_processes.Count -ge 2 -and (Test-Path -LiteralPath $orphanMarker)) 'Surviving descendant was missed or process ended before its completion.'
    $childRow = @($family.observed_processes | Where-Object { $_.parent_identity -ceq $family.root_process_identity -and $_.name -ceq 'pwsh' })
    Assert-True ($childRow.Count -eq 1 -and $childRow[0].start_ticks -le $family.observed_processes[0].exit_ticks) 'Descendant parent lifetime attribution incorrect.'
    Assert-True ($childRow[0].first_observed_ms -ge $family.root_exit_observed_ms) 'Fixture did not exercise discovery after parent exit.'
    Assert-True ($family.duration_ms -gt $family.root_exit_observed_ms + 1000 -and $family.live_process_identities_at_end.Count -eq 0) 'Observation did not continue after short parent exit.'
    # The detached child has its own hidden console, not the parent's redirected
    # pipe. Its sentinel/identity prove family tracking outlives root pipe EOF.
    Assert-True ((Get-Content -LiteralPath $family.stdout_path -Raw).Contains("CHILD_PID=$($childRow[0].pid)")) 'Parent EOF output was not retained.'
    Complete-Case 'retain_exited_parent_identity_until_descendant_and_eof_complete'

    $thirdLevelMarker = Join-Path $OutputRoot 'third-level-completed.txt'
    $workerScript = Write-Script 'middle-worker.ps1' @'
param([string]$Pwsh,[string]$ChildScript,[string]$Marker)
# Keep the intermediate identity observable; it still exits before its child.
Start-Sleep -Milliseconds 1500
$p=[Diagnostics.ProcessStartInfo]::new();$p.FileName=$Pwsh;$p.UseShellExecute=$false;$p.CreateNoWindow=$true
foreach($a in @('-NoProfile','-NonInteractive','-File',$ChildScript,'-Marker',$Marker)){$p.ArgumentList.Add($a)}
$child=[Diagnostics.Process]::Start($p)
$child.Dispose()
exit 0
'@
    $wrapperScript = Write-Script 'short-wrapper.ps1' @'
param([string]$Pwsh,[string]$WorkerScript,[string]$ChildScript,[string]$Marker)
$p=[Diagnostics.ProcessStartInfo]::new();$p.FileName=$Pwsh;$p.UseShellExecute=$false;$p.CreateNoWindow=$true
foreach($a in @('-NoProfile','-NonInteractive','-File',$WorkerScript,'-Pwsh',$Pwsh,'-ChildScript',$ChildScript,'-Marker',$Marker)){$p.ArgumentList.Add($a)}
$child=[Diagnostics.Process]::Start($p)
$child.Dispose()
exit 0
'@
    $threeLevel = Invoke-M0MeasuredProcess -Executable $pwsh -Arguments @('-NoProfile', '-NonInteractive', '-File', $wrapperScript, '-Pwsh', $pwsh, '-WorkerScript', $workerScript, '-ChildScript', $descendantScript, '-Marker', $thirdLevelMarker) -WorkingDirectory $OutputRoot -OutputPrefix (Join-Path $OutputRoot 'three-level') -TimeoutSeconds 30 -SampleIntervalMs 80
    # Windows may add a conhost descendant for each hidden console process.
    # Identify the three fixture workers by name; still validate every observed
    # identity (including console hosts) below.
    Assert-True ($threeLevel.status -ceq 'EXITED' -and $threeLevel.logs_complete_at_eof -and @($threeLevel.observed_processes | Where-Object name -CEQ 'pwsh').Count -eq 3 -and (Test-Path -LiteralPath $thirdLevelMarker)) 'Short wrapper/worker/survivor family did not complete.'
    $middle = @($threeLevel.observed_processes | Where-Object { $_.parent_identity -ceq $threeLevel.root_process_identity -and $_.name -ceq 'pwsh' })
    Assert-True ($middle.Count -eq 1) 'Intermediate process attribution is missing or ambiguous.'
    $survivor = @($threeLevel.observed_processes | Where-Object { $_.parent_identity -ceq $middle[0].process_identity -and $_.name -ceq 'pwsh' })
    Assert-True ($survivor.Count -eq 1 -and $survivor[0].start_ticks -ge $middle[0].start_ticks -and $survivor[0].start_ticks -le $middle[0].exit_ticks -and $survivor[0].exit_ticks -gt $middle[0].exit_ticks -and $middle[0].exit_ticks -gt $threeLevel.observed_processes[0].exit_ticks) 'Three-level lifetime chain is invalid.'
    Assert-True (@($threeLevel.observed_processes | Where-Object { -not $_.terminal_verified -or $_.exit_ticks -lt $_.start_ticks }).Count -eq 0 -and $threeLevel.live_process_identities_at_end.Count -eq 0 -and [Math]::Abs(($threeLevel.observed_processes | Measure-Object cpu_time_ms -Sum).Sum - $threeLevel.observed_family_cpu_time_ms) -lt 0.001) 'Three-level terminal CPU/family completion is invalid.'
    Complete-Case 'short_wrapper_worker_survivor_have_verified_native_lifetimes'

    $timeoutMarker = Join-Path $OutputRoot 'timeout-natural-completion.txt'
    $timeoutScript = Write-Script 'timeout-child.ps1' @'
param([string]$Marker)
[Console]::Out.WriteLine('BEFORE_TIMEOUT')
[Console]::Error.WriteLine('PARTIAL_STDERR')
Start-Sleep -Milliseconds 6500
[IO.File]::WriteAllText($Marker,'not killed')
[Console]::Out.WriteLine('AFTER_TIMEOUT_NATURAL_END')
'@
    $timeout = Invoke-M0MeasuredProcess -Executable $pwsh -Arguments @('-NoProfile', '-NonInteractive', '-File', $timeoutScript, '-Marker', $timeoutMarker) -WorkingDirectory $OutputRoot -OutputPrefix (Join-Path $OutputRoot 'timeout') -TimeoutSeconds 5 -SampleIntervalMs 80
    Assert-True ($timeout.status -ceq 'TIMED_OUT_PROCESS_NOT_KILLED' -and -not $timeout.logs_complete_at_eof -and -not $timeout.process_was_killed -and $null -eq $timeout.exit_code) 'Timeout was not recorded as unresolved without kill.'
    Assert-True ((Get-Content -LiteralPath $timeout.stdout_path -Raw).Contains('BEFORE_TIMEOUT') -and (Get-Content -LiteralPath $timeout.stderr_path -Raw).Contains('PARTIAL_STDERR')) 'Partial stdout/stderr were not persisted at timeout.'
    $stillLive = [Diagnostics.Process]::GetProcessById($timeout.pid)
    try {
        Assert-True ($stillLive.StartTime.ToUniversalTime().Ticks -eq $timeout.observed_processes[0].start_ticks -and -not $stillLive.HasExited) 'Timed out process was not alive with matching identity.'
        Assert-True ($stillLive.WaitForExit(15000)) 'Controlled timeout process did not exit naturally.'
    } finally { $stillLive.Dispose() }
    $drain = [Diagnostics.Stopwatch]::StartNew()
    do { $post = Get-Content -LiteralPath $timeout.stdout_path -Raw; if ($post.Contains('AFTER_TIMEOUT_NATURAL_END')) { break }; Start-Sleep -Milliseconds 50 } while ($drain.ElapsedMilliseconds -lt 3000)
    Assert-True ((Test-Path -LiteralPath $timeoutMarker) -and $post.Contains('AFTER_TIMEOUT_NATURAL_END')) 'Post-timeout natural completion/output drain failed.'
    $recorded = Get-Content -LiteralPath (Join-Path $OutputRoot 'timeout.process.json') -Raw | ConvertFrom-Json
    Assert-True (-not $recorded.logs_complete_at_eof -and $recorded.status -ceq 'TIMED_OUT_PROCESS_NOT_KILLED') 'Timeout metadata falsely changed to eventual pass.'
    Complete-Case 'timeout_partial_output_no_kill_and_later_natural_eof'

    $nonzero = Invoke-M0MeasuredProcess -Executable $pwsh -Arguments @('-NoProfile', '-NonInteractive', '-Command', '[Console]::Out.WriteLine("expected failure"); exit 7') -WorkingDirectory $OutputRoot -OutputPrefix (Join-Path $OutputRoot 'nonzero') -TimeoutSeconds 15 -SampleIntervalMs 50
    Assert-True ($nonzero.status -ceq 'EXITED' -and $nonzero.exit_code -eq 7 -and $nonzero.logs_complete_at_eof) 'Nonzero exit lost or falsely treated as timeout.'
    Complete-Case 'nonzero_exit_is_observed_not_reclassified'

    # Use cmd's native exit: this host's .NET Environment.Exit(259) returns a
    # different native termination code, so it cannot establish this edge case.
    $exit259 = Invoke-M0MeasuredProcess -Executable (Join-Path $env:SystemRoot 'System32/cmd.exe') -Arguments @('/d', '/c', 'exit 259') -WorkingDirectory $OutputRoot -OutputPrefix (Join-Path $OutputRoot 'exit-259') -TimeoutSeconds 15 -SampleIntervalMs 50
    Assert-True ($exit259.status -ceq 'EXITED' -and $exit259.exit_code -eq 259 -and $exit259.logs_complete_at_eof -and $exit259.observed_processes[0].terminal_verified -and $exit259.root_process_lifetime_ms -ge 0) 'Exit code 259 was mistaken for a running process.'
    Complete-Case 'signaled_process_exit_code_259_is_a_valid_final_code'

    $rejected = $false
    try { Invoke-M0MeasuredProcess -Executable $pwsh -Arguments @('-NoProfile', '-Command', 'exit 0') -WorkingDirectory $OutputRoot -OutputPrefix (Join-Path $OutputRoot 'escape') -IsolatedRoot $data -DirectoryTargets @{ outside = $OutputRoot } | Out-Null } catch { $rejected = $_.Exception.Message.Contains('outside the explicit isolated root') }
    Assert-True ($rejected -and -not (Test-Path -LiteralPath (Join-Path $OutputRoot 'escape.started.json'))) 'Out-of-scope directory sampling was not rejected before launch.'
    $rejected = $false
    try { Invoke-M0MeasuredProcess -Executable $pwsh -WorkingDirectory $OutputRoot -OutputPrefix (Join-Path $OutputRoot 'basic') | Out-Null } catch { $rejected = $_.Exception.Message.Contains('Refusing existing output') }
    Assert-True $rejected 'Existing evidence was not protected.'
    Complete-Case 'reject_out_of_scope_directory_and_existing_evidence'
    $summary.status = 'PASSED'; $summary.cases = $cases.ToArray()
} catch {
    $summary.status = 'FAILED'; $summary.failure = $_.Exception.ToString(); $summary.cases = $cases.ToArray()
    throw
} finally {
    $summary['powershell_version'] = $PSVersionTable.PSVersion.ToString()
    $summary['helper_sha256'] = (Get-FileHash -LiteralPath (Join-Path $PSScriptRoot 'M0.Performance.ps1') -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText((Join-Path $OutputRoot 'summary.json'), ($summary | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
}
$summary | ConvertTo-Json -Depth 8
