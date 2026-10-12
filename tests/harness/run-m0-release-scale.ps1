[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$PayloadRoot,
    [Parameter(Mandatory)][string]$OutputRoot,
    [Parameter(Mandatory)][string]$CliPath,
    [Parameter(Mandatory)][string]$AppPath,
    [Parameter(Mandatory)][string]$BridgePath,
    [Parameter(Mandatory)][string]$MakensisPath,
    [ValidateRange(1, 10)][int]$Repetitions = 3,
    [switch]$CancelDuringLargeFile,
    [switch]$CancelDuringSmallFiles,
    [switch]$R13Acceptance,
    [ValidateRange(1, 50000)][int]$MaximumPayloadFiles = 50000,
    [ValidateRange(1, 300)][int]$EntryTimeoutSeconds = 30,
    [ValidateRange(1, 3600)][int]$OperationTimeoutSeconds = 3600,
    [ValidateRange(30, 7200)][int]$BuildTimeoutSeconds = 3600,
    [ValidateRange(20, 2000)][int]$SampleIntervalMs = 100,
    [ValidateRange(1000, 60000)][int]$DirectorySampleIntervalMs = 5000,
    [switch]$PlanOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-R12([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Resolve-R12Path([string]$Path, [string]$Parent = '') {
    $full = [IO.Path]::GetFullPath($Path).TrimEnd([char]'\', [char]'/')
    if ($Parent) {
        $base = [IO.Path]::GetFullPath($Parent).TrimEnd([char]'\', [char]'/')
        Assert-R12 ($full.StartsWith($base + '\', [StringComparison]::OrdinalIgnoreCase)) "Path escapes its permitted directory: $full"
    }
    $cursor = $full
    while ($cursor) {
        if ([IO.File]::Exists($cursor) -or [IO.Directory]::Exists($cursor)) {
            Assert-R12 (([IO.File]::GetAttributes($cursor) -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse point rejected: $cursor"
        }
        $parentInfo = [IO.Directory]::GetParent($cursor)
        $cursor = if ($parentInfo) { $parentInfo.FullName } else { $null }
    }
    return $full
}
function Get-R12Hash([string]$Path) { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Write-R12Json([string]$Path, [object]$Value) {
    $safe = Resolve-R12Path $Path $script:RunRoot
    [IO.File]::WriteAllText($safe, (ConvertTo-Json -InputObject $Value -Depth 50), [Text.UTF8Encoding]::new($false))
}
function Read-R12Json([string]$Path) {
    $text = [Text.UTF8Encoding]::new($false, $true).GetString([IO.File]::ReadAllBytes($Path))
    return ($text | ConvertFrom-Json)
}
function Get-R12Inventory([string]$Directory) {
    if (-not [IO.Directory]::Exists($Directory)) { return }
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($Directory)
    while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            $safe = Resolve-R12Path $item.FullName $Directory
            if ($item.PSIsContainer) { $pending.Push($safe) }
            else { [pscustomobject]@{ path = $safe.Substring($Directory.Length + 1); size = $item.Length; sha256 = Get-R12Hash $safe } }
        }
    }
}
function Assert-R12Inventory([object[]]$Actual, [object[]]$Expected, [switch]$Exact) {
    $map = @{}
    foreach ($file in $Actual) {
        Assert-R12 (-not $map.ContainsKey($file.path)) "Duplicate path: $($file.path)"
        $map[$file.path] = $file
    }
    foreach ($file in $Expected) {
        Assert-R12 ($map.ContainsKey($file.path)) "Missing file: $($file.path)"
        Assert-R12 ($map[$file.path].sha256 -ceq $file.sha256 -and $map[$file.path].size -eq $file.size) "SHA-256 or size differs: $($file.path)"
    }
    if ($Exact) { Assert-R12 ($Actual.Count -eq $Expected.Count) 'Unexpected additional files.' }
}
function Get-R12Registration {
    foreach ($view in @([Microsoft.Win32.RegistryView]::Registry32, [Microsoft.Win32.RegistryView]::Registry64)) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $view)
        $key = $null
        try {
            $key = $base.OpenSubKey($script:RegistryKey, $false)
            $values = @{}
            if ($key) { foreach ($name in $key.GetValueNames()) { $values[$name] = $key.GetValue($name) } }
            [pscustomobject]@{ view = [string]$view; exists = ($null -ne $key); values = $values }
        }
        finally { if ($key) { $key.Dispose() }; $base.Dispose() }
    }
}
function Assert-R12NoRegistration {
    Assert-R12 (@(Get-R12Registration | Where-Object exists).Count -eq 0) 'Task-specific uninstall registration remains.'
}
function Assert-R12NoLock {
    $folder = Join-Path $script:LocalRoot 'Avelin/M0/locks'
    $locks = @(if (Test-Path -LiteralPath $folder) { Get-ChildItem -LiteralPath $folder -File -Force | Where-Object { $_.Name.StartsWith($script:AppId + '-', [StringComparison]::Ordinal) } })
    Assert-R12 ($locks.Count -eq 0) 'Task-specific product lock remains. No automatic unlock was attempted.'
}
function Get-R12EventMetrics([object[]]$Events) {
    $times = @($Events | ForEach-Object { [long]$_.coordinator_received_elapsed_ms })
    $left = 0; $peak = 0
    for ($right = 0; $right -lt $times.Count; $right++) {
        while ($left -lt $right -and $times[$right] - $times[$left] -ge 1000) { $left++ }
        $peak = [Math]::Max($peak, $right - $left + 1)
    }
    [ordered]@{
        recorded_event_count = $Events.Count
        progress_event_count = @($Events | Where-Object kind -CEQ 'progress').Count
        finish_event_count = @($Events | Where-Object kind -CEQ 'finish').Count
        maximum_recorded_events_in_1000ms_window = $peak
        rate_scope = 'Maximum count in a sliding half-open 1000 ms window of coordinator receipt timestamps. Reported backend events only; heartbeat/transport messages omitted by the coordinator are not counted. Not UI update frequency.'
    }
}
function Assert-R12Report([object]$Report, [object]$Process, [string]$ExpectedResult, [string]$ExpectedDirectory, [int]$ExpectedFiles) {
    $code = if ($ExpectedResult -ceq 'Cancelled') { 2 } else { 0 }
    Assert-R12 ($Process.status -ceq 'EXITED' -and $Process.exit_code -eq $code -and $Process.logs_complete_at_eof -eq $true) 'Operation process or complete output capture did not reach its expected terminal state.'
    Assert-R12 ($Process.process_was_killed -eq $false -and @($Process.live_process_identities_at_end).Count -eq 0) 'Operation terminated a process or left an observed descendant running.'
    $stdoutReport = Read-R12Json $Process.stdout_path
    Assert-R12 ((ConvertTo-Json -InputObject $stdoutReport -Depth 50 -Compress) -ceq (ConvertTo-Json -InputObject $Report -Depth 50 -Compress)) 'Raw UTF-8 stdout JSON differs from the persisted report.'
    Assert-R12 ($Report.result -ceq $ExpectedResult -and $Report.exit_code -eq $code -and $Report.coordinator_exit_code -eq $code -and $Report.product_lock_retained -eq $false) 'Report does not confirm the expected coordinator/backend exit and released product lock.'
    Assert-R12 ($null -eq $Report.protocol_fault) 'Protocol fault was recorded.'
    Assert-R12 ((Resolve-R12Path $Report.install_dir) -ceq $ExpectedDirectory) 'Report targets an unexpected installation directory.'
    Assert-R12 ($Report.backend_pid -gt 0 -and -not [string]::IsNullOrWhiteSpace($Report.task_id)) 'Report has no backend/task identity.'
    $events = @($Report.events)
    Assert-R12 ($events.Count -gt 0) 'Report has no backend events.'
    $lastSeq = -1L; $lastTime = -1L
    foreach ($event in $events) {
        Assert-R12 ($event.task_id -ceq $Report.task_id -and $event.protocol_version -eq 1) 'Event task or protocol identity differs.'
        Assert-R12 ($event.seq -gt $lastSeq -and $event.coordinator_received_elapsed_ms -ge $lastTime) 'Event sequence or observation time is not ordered.'
        $lastSeq = [long]$event.seq; $lastTime = [long]$event.coordinator_received_elapsed_ms
        if ($event.kind -ceq 'progress') { Assert-R12 ($event.total -eq $ExpectedFiles -and $event.completed -ge 0 -and $event.completed -le $ExpectedFiles) 'Progress count differs from the fixture.' }
    }
    $finish = @($events | Where-Object kind -CEQ 'finish')
    Assert-R12 ($finish.Count -eq 1 -and $events[-1].kind -ceq 'finish' -and $finish[0].result -ceq $ExpectedResult -and $finish[0].code -eq $code) 'Expected one matching terminal finish event as the last report event.'
}
function Get-R12Interval([object[]]$Events, [int]$Index) {
    $begin = @($Events | Where-Object { $_.kind -ceq 'progress' -and $_.phase -ceq 'extracting' -and $_.completed -eq $Index })
    $end = @($Events | Where-Object { $_.kind -ceq 'progress' -and $_.phase -ceq 'running' -and $_.completed -eq ($Index + 1) })
    if ($begin.Count -ne 1 -or $end.Count -ne 1) { return $null }
    [pscustomobject]@{ begin_ms = [long]$begin[0].coordinator_received_elapsed_ms; end_ms = [long]$end[0].coordinator_received_elapsed_ms; index = $Index }
}
function Get-R12Cancellation([object]$Report, [int]$TargetIndex, [int[]]$SmallIndices, [string]$Kind, [long]$Delay) {
    $t = $Report.cancellation
    foreach ($name in @('requested_elapsed_ms', 'acknowledged_elapsed_ms', 'stopped_elapsed_ms', 'cleanup_completed_elapsed_ms')) {
        Assert-R12 ($null -ne $t.$name) "Missing cancellation observation: $name"
    }
    Assert-R12 ($t.requested_elapsed_ms -le $t.acknowledged_elapsed_ms -and $t.acknowledged_elapsed_ms -le $t.stopped_elapsed_ms -and $t.stopped_elapsed_ms -le $t.cleanup_completed_elapsed_ms) 'Cancellation observations are not ordered.'
    $target = Get-R12Interval @($Report.events) $TargetIndex
    $hitTarget = $null -ne $target -and $target.begin_ms -lt $t.requested_elapsed_ms -and $t.requested_elapsed_ms -lt $target.end_ms
    $smallHit = @()
    if ($Kind -ceq 'SMALL_FILES') {
        $smallSet = [Collections.Generic.HashSet[int]]::new($SmallIndices)
        $begins = @{}
        $smallEvents = [Collections.Generic.List[object]]::new()
        foreach ($event in $Report.events) {
            if ($event.kind -cne 'progress') { continue }
            if ($event.phase -ceq 'extracting' -and $smallSet.Contains([int]$event.completed)) {
                $begins[[int]$event.completed] = [long]$event.coordinator_received_elapsed_ms
                $smallEvents.Add($event)
            }
            elseif ($event.phase -ceq 'running' -and $smallSet.Contains([int]$event.completed - 1)) {
                $index = [int]$event.completed - 1
                if ($begins.ContainsKey($index) -and $begins[$index] -lt $t.requested_elapsed_ms -and $t.requested_elapsed_ms -lt $event.coordinator_received_elapsed_ms) { $smallHit += $index }
                $smallEvents.Add($event)
            }
        }
        # Between two completed small files is also inside the small-file stage.
        $before = @($smallEvents | Where-Object { $_.coordinator_received_elapsed_ms -lt $t.requested_elapsed_ms })
        $after = @($smallEvents | Where-Object { $_.coordinator_received_elapsed_ms -gt $t.requested_elapsed_ms })
        $hitStage = $before.Count -gt 0 -and $after.Count -gt 0
    }
    else { $hitStage = $hitTarget }
    [ordered]@{
        kind = $Kind; status = $(if ($hitStage) { 'OBSERVED_REQUEST_IN_TARGET_STAGE' } else { 'NOT_OBSERVED' })
        requested_delay_ms = $Delay; target_file_index = $TargetIndex; target_interval = $target
        request_inside_exact_target_file_interval = $hitTarget; observed_small_file_intervals_containing_request = $smallHit
        observed_times = $t
        request_to_acknowledgement_ms = $t.acknowledged_elapsed_ms - $t.requested_elapsed_ms
        request_to_checkpoint_stop_ms = $t.stopped_elapsed_ms - $t.requested_elapsed_ms
        request_to_cleanup_completed_ms = $t.cleanup_completed_elapsed_ms - $t.requested_elapsed_ms
        note = 'Coordinator observations, not disk instrumentation. Delay uses the normal operation interval midpoint. No artificial delay or file barrier. NSIS File is not interrupted internally; cancellation takes effect at a checkpoint. A passed cancellation result does not certify interval targeting.'
    }
}

function Assert-R12FixtureSize([int]$Count, [int]$Maximum) {
    Assert-R12 ($Count -gt 0 -and $Count -le $Maximum) "Fixture total must be 1..$Maximum files including entry and any large file."
}
function Get-R12Invocation([string]$Operation, [bool]$Strict, [string]$Setup, [string]$Runtime, [string]$Backend, [string]$Manifest, [string]$Target, [string]$Report, [int]$Timeout, [long]$CancelDelay = 0) {
    if ($Operation -ceq 'uninstall') {
        $executable = if ($Strict) { Join-Path $Target '.avelin/avelin-app.exe' } else { $Runtime }
        $manifestPath = if ($Strict) { Join-Path $Target '.avelin/manifest.json' } else { $Manifest }
        return [pscustomobject]@{ executable = $executable; arguments = @('--operation', 'uninstall', '--backend', (Join-Path $Target 'uninstall.exe'), '--manifest', $manifestPath, '--install-dir', $Target, '--report', $Report, '--observation-timeout-secs', "$Timeout"); entry = $(if ($Strict) { 'REGISTERED_RESIDENT_RUNTIME_AND_MANIFEST' } else { 'PINNED_RUNTIME_PLUS_INSTALLED_BACKEND_NOT_RESIDENT_LAUNCHER' }) }
    }
    Assert-R12 ($Operation -ceq 'cancel') 'Unsupported operation invocation.'
    $arguments = @('--operation', 'install', '--install-dir', $Target, '--report', $Report, '--cancel-after-ms', "$CancelDelay", '--observation-timeout-secs', "$Timeout")
    if (-not $Strict) { $arguments = @('--backend', $Backend, '--manifest', $Manifest) + $arguments }
    return [pscustomobject]@{ executable = $(if ($Strict) { $Setup } else { $Runtime }); arguments = $arguments; entry = $(if ($Strict) { 'COMPLETE_SETUP_CLI' } else { 'PINNED_RUNTIME_PLUS_BUILT_BACKEND' }) }
}
function Assert-R13Entry([object]$Process) {
    Assert-R12 ($Process.status -ceq 'EXITED' -and $Process.exit_code -eq 0 -and $Process.logs_complete_at_eof -eq $true -and $Process.process_was_killed -eq $false -and @($Process.live_process_identities_at_end).Count -eq 0 -and @($Process.observation_errors).Count -eq 0) 'Installed entry did not naturally complete with captured output.'
    $utf8 = [Text.UTF8Encoding]::new($false, $true)
    $stdout = $utf8.GetString([IO.File]::ReadAllBytes($Process.stdout_path)).Replace("`r`n", "`n")
    $stderr = $utf8.GetString([IO.File]::ReadAllBytes($Process.stderr_path))
    $expected = "Nimbus Notes — Avelin M0 sample application`nThis executable is a local installation test payload.`n"
    Assert-R12 ($stdout -ceq $expected -and $stderr.Length -eq 0) 'Installed Nimbus entry output differs from its two-line no-argument contract.'
}
function Assert-R13Operation([object]$Report, [object]$Process) {
    Assert-R12 (@($Process.observation_errors).Count -eq 0) 'Strict acceptance has process observation errors.'
    $backend = @($Process.observed_processes | Where-Object { $_.pid -eq $Report.backend_pid })
    Assert-R12 ($backend.Count -eq 1 -and $backend[0].terminal_verified -eq $true -and $backend[0].exit_code -eq $Report.exit_code) 'Reported backend lacks a matching observed natural terminal process identity.'
}
function Assert-R13CancellationTarget([object]$Observation) {
    Assert-R12 ($Observation.status -ceq 'OBSERVED_REQUEST_IN_TARGET_STAGE') 'Cancellation cleaned up, but the requested target stage was not observed. Stop; retain the missed attempt and do not automatically retry.'
}

$effectiveMaximum = if ($R13Acceptance) { [Math]::Min($MaximumPayloadFiles, 10000) } else { $MaximumPayloadFiles }
$plan = [ordered]@{
    schema_version = 1; status = 'PLAN_ONLY_NOT_EXECUTED'; payload_root = $PayloadRoot; output_root = $OutputRoot
    repetitions = $Repetitions; build_count = 1; cancel_large_file = [bool]$CancelDuringLargeFile; cancel_small_files = [bool]$CancelDuringSmallFiles
    operation_timeout_seconds = $OperationTimeoutSeconds; observer_timeout_seconds = $OperationTimeoutSeconds + 120; build_timeout_seconds = $BuildTimeoutSeconds
    process_sample_target_interval_ms = $SampleIntervalMs; directory_sample_target_interval_ms = $DirectorySampleIntervalMs
    r13_acceptance = [bool]$R13Acceptance; maximum_payload_files = $effectiveMaximum; entry_timeout_seconds = $EntryTimeoutSeconds
    install_entry = 'COMPLETE_SETUP_CLI'; uninstall_entry = $(if ($R13Acceptance) { 'REGISTERED_RESIDENT_RUNTIME_AND_MANIFEST' } else { 'PINNED_RUNTIME_PLUS_INSTALLED_BACKEND_NOT_RESIDENT_LAUNCHER' }); cancellation_entry = $(if ($R13Acceptance) { 'COMPLETE_SETUP_CLI' } else { 'PINNED_RUNTIME_PLUS_BUILT_BACKEND' })
    installed_entry_execution = $(if ($R13Acceptance) { 'NIMBUS_NO_ARGUMENTS_EXACT_STDOUT' } else { 'NOT_REQUESTED' }); cancellation_targeting_required = [bool]$R13Acceptance
    scope = 'Local unsigned Release-candidate CLI capacity/performance experiment; no GUI first-frame, clean-machine, cold-cache, percentile or supported-capacity claim.'
    isolation = 'One unique application ID and immutable copied runtime/CLI/bridge per invocation. Unique targets per repetition and cancellation. Read-only source payload. No force termination, recursive cleanup or automatic retry.'
}
if ($PlanOnly) { $plan | ConvertTo-Json -Depth 10; return }
Assert-R12 ($PSVersionTable.PSVersion.Major -ge 7 -and [Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'Requires PowerShell 7 on Windows.'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$script:RunRoot = Resolve-R12Path $OutputRoot (Join-Path $repo 'target')
$payload = Resolve-R12Path $PayloadRoot
Assert-R12 ([IO.Directory]::Exists($payload)) 'Payload directory is missing.'
Assert-R12 (-not (Test-Path -LiteralPath $script:RunRoot)) 'OutputRoot already exists; choose a new path. Existing evidence is never overwritten.'
Assert-R12 (-not $script:RunRoot.StartsWith($payload + '\', [StringComparison]::OrdinalIgnoreCase) -and -not $payload.StartsWith($script:RunRoot + '\', [StringComparison]::OrdinalIgnoreCase)) 'Payload and output directories must not contain each other.'
$inputs = [ordered]@{ cli = $CliPath; app = $AppPath; bridge = $BridgePath; makensis = $MakensisPath }
foreach ($key in @($inputs.Keys)) { $inputs[$key] = Resolve-R12Path $inputs[$key]; Assert-R12 ([IO.File]::Exists($inputs[$key])) "Missing tool: $key" }
$helper = Join-Path $PSScriptRoot 'scale/M0.Performance.ps1'
Assert-R12 ([IO.File]::Exists($helper)) "Sampling helper missing: $helper"
. $helper
$null = [IO.Directory]::CreateDirectory($script:RunRoot)
$round = if ($R13Acceptance) { 'r13' } else { 'r12' }
$script:AppId = "dev.avelin.m0.$round." + [Guid]::NewGuid().ToString('N')
$script:RegistryKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:AppId
$script:LocalRoot = Join-Path $script:RunRoot 'local-app-data'
$tempRoot = Join-Path $script:RunRoot 'temp'
$toolsRoot = Join-Path $script:RunRoot 'tools'
foreach ($directory in @($script:LocalRoot, $tempRoot, $toolsRoot)) { $null = [IO.Directory]::CreateDirectory($directory) }
$summary = [ordered]@{
    schema_version = 1; status = 'RUNNING'; active_case = 'PREFLIGHT'; failure = $null; started_at_utc = [DateTime]::UtcNow.ToString('o')
    run_directory = $script:RunRoot; app_id = $script:AppId; registry_key = $script:RegistryKey; plan = $plan
    input_identity = @(); cases = @(); cancellation_targeting = 'NOT_ASSESSED'; benchmark_summary = 'Raw repeated local samples; no reliable percentile or cold-cache claim.'
}
function Save-R12Summary { Write-R12Json (Join-Path $script:RunRoot 'summary.json') $summary }
function New-R12ReportPath([string]$Name) {
    $folder = Resolve-R12Path (Join-Path $script:RunRoot "operation-$Name") $script:RunRoot
    Assert-R12 (-not (Test-Path -LiteralPath $folder)) 'Operation evidence directory already exists.'
    $null = [IO.Directory]::CreateDirectory($folder)
    return (Join-Path $folder 'report.json')
}
# Kept as a small adapter so the shared sampler owns process, EOF and space observation.
function Invoke-R12Operation([string]$Executable, [string[]]$Arguments, [string]$Name, [int]$Timeout, [string[]]$Directories) {
    $summary.active_case = $Name; Save-R12Summary
    $directoryTargets = @{}
    for ($i = 0; $i -lt $Directories.Count; $i++) { $directoryTargets["path$i"] = $Directories[$i] }
    $result = Invoke-M0MeasuredProcess -Executable $Executable -Arguments $Arguments -WorkingDirectory $script:RunRoot -OutputPrefix (Join-Path $script:RunRoot $Name) -TimeoutSeconds $Timeout -Environment @{ TEMP = $tempRoot; TMP = $tempRoot; LOCALAPPDATA = $script:LocalRoot } -IsolatedRoot $script:RunRoot -DirectoryTargets $directoryTargets -SampleIntervalMs $SampleIntervalMs -DirectorySampleIntervalMs $DirectorySampleIntervalMs
    Assert-R12 ($result.status -ceq 'EXITED' -and $result.logs_complete_at_eof -eq $true) "Observation failed: $Name. Retain all evidence; no process is killed or operation retried."
    Assert-R12 ($result.process_was_killed -eq $false -and @($result.live_process_identities_at_end).Count -eq 0) 'Observed process family has not naturally completed.'
    return $result
}
try {
    Save-R12Summary
    [IO.File]::Copy($PSCommandPath, (Join-Path $toolsRoot 'harness-source.ps1'), $false)
    [IO.File]::Copy($helper, (Join-Path $toolsRoot 'sampler-source.ps1'), $false)
    $summary['harness_sha256'] = Get-R12Hash (Join-Path $toolsRoot 'harness-source.ps1')
    $summary['sampler_sha256'] = Get-R12Hash (Join-Path $toolsRoot 'sampler-source.ps1')
    foreach ($key in @($inputs.Keys)) {
        $source = $inputs[$key]; $hash = Get-R12Hash $source
        if ($key -ne 'makensis') {
            $copy = Join-Path $toolsRoot ([IO.Path]::GetFileName($source))
            [IO.File]::Copy($source, $copy, $false)
            Assert-R12 ((Get-R12Hash $copy) -ceq $hash) "Tool changed while pinning: $key"
            $inputs[$key] = $copy
        }
        $summary.input_identity += [ordered]@{ kind = $key; source = $source; executed_path = $inputs[$key]; sha256 = $hash; size = (Get-Item -LiteralPath $source).Length }
    }
    Assert-R12NoRegistration; Assert-R12NoLock
    $expected = @(Get-R12Inventory $payload | Sort-Object path)
    Assert-R12FixtureSize $expected.Count $effectiveMaximum
    Assert-R12 (@($expected | Where-Object path -CEQ 'entry.exe').Count -eq 1) 'Fixture requires entry.exe.'
    Write-R12Json (Join-Path $script:RunRoot 'fixture-snapshot.json') $expected
    $summary['payload_file_count'] = $expected.Count
    $summary['payload_bytes'] = [long]($expected | Measure-Object size -Sum).Sum
    $summary['fixture_snapshot_sha256'] = Get-R12Hash (Join-Path $script:RunRoot 'fixture-snapshot.json')
    $requiredFree = [long][Math]::Ceiling(8 * $summary.payload_bytes + 32 * (Get-Item -LiteralPath $inputs.app).Length + 1GB)
    $drive = [IO.DriveInfo]::new([IO.Path]::GetPathRoot($script:RunRoot))
    Assert-R12 ($drive.AvailableFreeSpace -gt $requiredFree) 'Insufficient free space for the conservative one-run working reserve.'
    $summary['disk_preflight'] = [ordered]@{ available_bytes = $drive.AvailableFreeSpace; conservative_required_free_bytes = $requiredFree; note = 'Reserve check, not measured disk peak or a hard runtime quota. Completed repetitions retain only non-managed sentinels, logs and sessions.' }
    $payloadToml = $payload.Replace('\', '/').Replace('"', '\"')
    $roundLabel = $round.ToUpperInvariant()
    $config = "schema_version = 1`n[app]`nid = `"$($script:AppId)`"`nname = `"Avelin $roundLabel Release Scale`"`nversion = `"0.1.0`"`nentry = `"entry.exe`"`n[payload]`ndirectory = `"$payloadToml`"`n[install]`ndirectory_name = `"Avelin${roundLabel}Scale`"`nstart_menu_shortcut = false`n"
    $configPath = Join-Path $script:RunRoot 'avelin.toml'
    [IO.File]::WriteAllText($configPath, $config, [Text.UTF8Encoding]::new($false))
    $setup = Join-Path $script:RunRoot 'ScaleSetup.exe'; $backend = Join-Path $script:RunRoot 'ScaleSetup.backend.exe'; $manifestPath = Join-Path $script:RunRoot 'ScaleSetup.manifest.json'
    $build = Invoke-R12Operation $inputs.cli @('build', '--config', $configPath, '--output', $setup, '--runtime', $inputs.app, '--bridge', $inputs.bridge, '--nsis', $inputs.makensis) 'build' $BuildTimeoutSeconds @($tempRoot, $script:RunRoot)
    Assert-R12 ($build.exit_code -eq 0) 'Production builder failed; no install attempted.'
    $metadata = Get-Content -LiteralPath $build.stdout_path | Where-Object { $_.StartsWith('{') } | Select-Object -Last 1 | ConvertFrom-Json
    $stage = Resolve-R12Path $metadata.stage $script:RunRoot
    $nsi = Resolve-R12Path (Join-Path $stage 'installer.nsi') $script:RunRoot
    $manifest = Read-R12Json $manifestPath
    Assert-R12 ($manifest.app_id -ceq $script:AppId -and $manifest.entry -ceq 'entry.exe' -and $manifest.start_menu_shortcut -eq $false -and $manifest.runtime_sha256 -ceq (Get-R12Hash $inputs.app)) 'Built manifest identity differs.'
    Assert-R12Inventory @($manifest.files) $expected -Exact
    $summary['build'] = [ordered]@{ process = $build; makensis_wall_ms = $metadata.makensis_wall_ms; nsis_script_bytes = (Get-Item -LiteralPath $nsi).Length; nsis_script_lines = ([IO.File]::ReadLines($nsi) | Measure-Object).Count; setup_bytes = (Get-Item -LiteralPath $setup).Length; setup_sha256 = Get-R12Hash $setup; backend_bytes = (Get-Item -LiteralPath $backend).Length; backend_sha256 = Get-R12Hash $backend; manifest_sha256 = Get-R12Hash $manifestPath; compilation_timing_scope = 'Builder stopwatch around makensis spawn/wait. Full process duration also includes inventory, staging, hashes and assembly. Sampler compiler observations are separate from build-family observations.' }
    $largeIndex = -1; $smallList = [Collections.Generic.List[int]]::new()
    for ($i = 0; $i -lt $manifest.files.Count; $i++) {
        if ($manifest.files[$i].path -ceq '000-large.bin') { $largeIndex = $i }
        elseif ($manifest.files[$i].path -cne 'entry.exe') { $smallList.Add($i) }
    }
    $smallIndices = $smallList.ToArray()
    Assert-R12 (-not $CancelDuringLargeFile -or $largeIndex -ge 0) 'Large-file cancellation requested but 000-large.bin is absent.'
    Assert-R12 (-not $CancelDuringSmallFiles -or $smallIndices.Count -gt 0) 'Small-file cancellation requested but no small files exist.'
    $cancelObservations = @()
    for ($rep = 1; $rep -le $Repetitions; $rep++) {
        $prefix = 'r{0:D2}' -f $rep
        $target = Resolve-R12Path (Join-Path $script:RunRoot "$prefix-install-中文 space") $script:RunRoot
        $reportPath = New-R12ReportPath "$prefix-install"
        Assert-R12 (-not (Test-Path -LiteralPath $target)) 'Installation target already exists.'
        Assert-R12NoRegistration; Assert-R12NoLock
        $install = Invoke-R12Operation $setup @('--operation', 'install', '--install-dir', $target, '--report', $reportPath, '--observation-timeout-secs', "$OperationTimeoutSeconds") "$prefix-install" ($OperationTimeoutSeconds + 120) @($tempRoot, $target, [IO.Path]::GetDirectoryName($reportPath))
        $report = Read-R12Json $reportPath
        Assert-R12Report $report $install 'Succeeded' $target $expected.Count
        if ($R13Acceptance) { Assert-R13Operation $report $install }
        Assert-R12NoLock
        $installed = @(Get-R12Inventory $target)
        Assert-R12Inventory $installed $expected
        Assert-R12 ($installed.Count -eq $expected.Count + 4) 'Installed file count differs from payload plus four managed metadata/runtime files.'
        foreach ($required in @('uninstall.exe', '.avelin-receipt.json', '.avelin\avelin-app.exe', '.avelin\manifest.json')) { Assert-R12 (@($installed | Where-Object path -CEQ $required).Count -eq 1) "Missing managed file: $required" }
        Assert-R12 ((Get-R12Hash (Join-Path $target '.avelin/avelin-app.exe')) -ceq (Get-R12Hash $inputs.app) -and (Get-R12Hash (Join-Path $target '.avelin/manifest.json')) -ceq (Get-R12Hash $manifestPath)) 'Installed runtime or manifest differs from pinned input.'
        $receipt = Read-R12Json (Join-Path $target '.avelin-receipt.json')
        Assert-R12 ($receipt.schema_version -eq 1 -and $receipt.app_id -ceq $script:AppId -and $receipt.app_version -ceq $manifest.app_version -and $receipt.task_id -ceq $report.task_id -and (Resolve-R12Path $receipt.install_dir) -ceq $target -and -not [string]::IsNullOrWhiteSpace($receipt.commit_id) -and -not [string]::IsNullOrWhiteSpace($receipt.installation_id) -and $receipt.user_sid -ceq [Security.Principal.WindowsIdentity]::GetCurrent().User.Value) 'Committed receipt identity differs.'
        Assert-R12Inventory @($receipt.files) $expected -Exact
        $registry = @(Get-R12Registration)
        Assert-R12 (@($registry | Where-Object exists).Count -gt 0) 'Successful install did not register uninstall.'
        $expectedUninstall = '"{0}\.avelin\avelin-app.exe" --operation uninstall --backend "{0}\uninstall.exe" --manifest "{0}\.avelin\manifest.json" --install-dir "{0}"' -f $target
        foreach ($item in @($registry | Where-Object exists)) {
            Assert-R12 ((Resolve-R12Path $item.values.InstallLocation) -ceq $target -and $item.values.UninstallString -ceq $expectedUninstall -and $item.values.QuietUninstallString -ceq $expectedUninstall) 'Registration target or uninstall command differs.'
        }
        Write-R12Json (Join-Path $script:RunRoot "$prefix-installed.evidence.json") ([ordered]@{ files = $installed; receipt = $receipt; registration = $registry; product_lock_absent = $true })
        $summary.cases += [ordered]@{ id = "$prefix-INSTALL"; repetition = $rep; status = 'PASSED'; entry = 'COMPLETE_SETUP_CLI'; process = $install; report_path = $reportPath; coordinator_elapsed_ms = $report.elapsed_ms; metrics = Get-R12EventMetrics @($report.events); verified_payload_files = $expected.Count }
        Save-R12Summary
        if ($R13Acceptance) {
            $entry = Invoke-R12Operation (Join-Path $target 'entry.exe') @() "$prefix-entry" $EntryTimeoutSeconds @($target)
            Assert-R13Entry $entry
            Assert-R12Inventory @(Get-R12Inventory $target) $installed -Exact
            $summary.cases += [ordered]@{ id = "$prefix-ENTRY"; repetition = $rep; status = 'PASSED'; entry = 'INSTALLED_NIMBUS_NO_ARGUMENTS'; process = $entry; stdout_contract = 'Nimbus two-line greeting, UTF-8; stderr empty'; installed_inventory_unchanged = $true }
            Save-R12Summary
        }
        $sentinelPaths = @('user-kept.txt')
        if ($R13Acceptance) { $sentinelPaths += 'small\group-000\user-kept.txt' }
        $sentinel = @(foreach ($relative in $sentinelPaths) {
            $sentinelPath = Resolve-R12Path (Join-Path $target $relative) $target
            Assert-R12 (-not (Test-Path -LiteralPath $sentinelPath)) 'User sentinel collides with a managed file.'
            $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($sentinelPath))
            [IO.File]::WriteAllText($sentinelPath, "$roundLabel user sentinel $($script:AppId) $rep $relative", [Text.UTF8Encoding]::new($false))
            [pscustomobject]@{ path = $relative; size = (Get-Item -LiteralPath $sentinelPath).Length; sha256 = Get-R12Hash $sentinelPath }
        })
        $uninstallPath = New-R12ReportPath "$prefix-uninstall"
        # The registration above is compared to known argv; never evaluate registry text.
        $uninstallInvocation = Get-R12Invocation 'uninstall' ([bool]$R13Acceptance) $setup $inputs.app $backend $manifestPath $target $uninstallPath $OperationTimeoutSeconds
        $uninstall = Invoke-R12Operation $uninstallInvocation.executable $uninstallInvocation.arguments "$prefix-uninstall" ($OperationTimeoutSeconds + 120) @($tempRoot, $target, [IO.Path]::GetDirectoryName($uninstallPath))
        $uninstallReport = Read-R12Json $uninstallPath
        Assert-R12Report $uninstallReport $uninstall 'Succeeded' $target $expected.Count
        if ($R13Acceptance) { Assert-R13Operation $uninstallReport $uninstall }
        $remaining = @(Get-R12Inventory $target)
        Assert-R12Inventory $remaining $sentinel -Exact
        Assert-R12NoRegistration; Assert-R12NoLock
        Write-R12Json (Join-Path $script:RunRoot "$prefix-uninstalled.evidence.json") ([ordered]@{ remaining_files = $remaining; registration = @(Get-R12Registration); product_lock_absent = $true; sentinel = $sentinel })
        $summary.cases += [ordered]@{ id = "$prefix-UNINSTALL"; repetition = $rep; status = 'PASSED'; entry = $uninstallInvocation.entry; process = $uninstall; report_path = $uninstallPath; coordinator_elapsed_ms = $uninstallReport.elapsed_ms; metrics = Get-R12EventMetrics @($uninstallReport.events); unknown_user_file_preserved = $true }
        Save-R12Summary
        $cancelKinds = @()
        if ($CancelDuringLargeFile) { $cancelKinds += [pscustomobject]@{ kind = 'LARGE_FILE'; name = 'large'; index = $largeIndex } }
        if ($CancelDuringSmallFiles) { $cancelKinds += [pscustomobject]@{ kind = 'SMALL_FILES'; name = 'small'; index = $smallIndices[[int][Math]::Floor($smallIndices.Count / 2)] } }
        foreach ($cancelKind in $cancelKinds) {
            $normalInterval = Get-R12Interval @($report.events) $cancelKind.index
            Assert-R12 ($null -ne $normalInterval -and $normalInterval.end_ms -ge $normalInterval.begin_ms) 'Normal report lacks target file timing.'
            $delay = [long][Math]::Floor(($normalInterval.begin_ms + $normalInterval.end_ms) / 2)
            $cancelTarget = Resolve-R12Path (Join-Path $script:RunRoot "$prefix-cancel-$($cancelKind.name)-中文 space") $script:RunRoot
            $cancelPath = New-R12ReportPath "$prefix-cancel-$($cancelKind.name)"
            Assert-R12 (-not (Test-Path -LiteralPath $cancelTarget)) 'Cancellation target already exists.'
            Assert-R12NoRegistration; Assert-R12NoLock
            $cancelInvocation = Get-R12Invocation 'cancel' ([bool]$R13Acceptance) $setup $inputs.app $backend $manifestPath $cancelTarget $cancelPath $OperationTimeoutSeconds $delay
            $cancel = Invoke-R12Operation $cancelInvocation.executable $cancelInvocation.arguments "$prefix-cancel-$($cancelKind.name)" ($OperationTimeoutSeconds + 120) @($tempRoot, $cancelTarget, [IO.Path]::GetDirectoryName($cancelPath))
            $cancelReport = Read-R12Json $cancelPath
            Assert-R12Report $cancelReport $cancel 'Cancelled' $cancelTarget $expected.Count
            if ($R13Acceptance) { Assert-R13Operation $cancelReport $cancel }
            $cancelRemaining = @(Get-R12Inventory $cancelTarget)
            Assert-R12Inventory $cancelRemaining @() -Exact
            Assert-R12Inventory @(Get-R12Inventory $target) $sentinel -Exact
            Assert-R12NoRegistration; Assert-R12NoLock
            $observed = Get-R12Cancellation $cancelReport $cancelKind.index $smallIndices $cancelKind.kind $delay
            $observed['normal_target_interval'] = $normalInterval
            $cancelObservations += $observed
            $summary.cancellation_targeting = if (@($cancelObservations | Where-Object status -CEQ 'NOT_OBSERVED').Count) { 'SOME_INTERVALS_NOT_OBSERVED' } else { 'ALL_REQUESTED_STAGES_OBSERVED' }
            Write-R12Json (Join-Path $script:RunRoot "$prefix-cancel-$($cancelKind.name).evidence.json") ([ordered]@{ remaining_files = $cancelRemaining; registration = @(Get-R12Registration); product_lock_absent = $true; cancellation = $observed })
            $summary.cases += [ordered]@{ id = "$prefix-CANCEL-$($cancelKind.kind)"; repetition = $rep; status = $(if ($R13Acceptance -and $observed.status -cne 'OBSERVED_REQUEST_IN_TARGET_STAGE') { 'FUNCTIONAL_PASSED_TARGET_NOT_OBSERVED' } else { 'PASSED' }); entry = $cancelInvocation.entry; process = $cancel; report_path = $cancelPath; coordinator_elapsed_ms = $cancelReport.elapsed_ms; metrics = Get-R12EventMetrics @($cancelReport.events); cancellation = $observed; no_managed_files_or_receipt_remain = $true }
            Save-R12Summary
            if ($R13Acceptance) { Assert-R13CancellationTarget $observed }
        }
    }
    Assert-R12Inventory @(Get-R12Inventory $payload) $expected -Exact
    foreach ($identity in $summary.input_identity) { Assert-R12 ((Get-R12Hash $identity.executed_path) -ceq $identity.sha256) "Input changed during execution: $($identity.kind)" }
    $summary.cancellation_targeting = if (-not $cancelObservations.Count) { 'NOT_REQUESTED' } elseif (@($cancelObservations | Where-Object status -CEQ 'NOT_OBSERVED').Count) { 'SOME_INTERVALS_NOT_OBSERVED' } else { 'ALL_REQUESTED_STAGES_OBSERVED' }
    $summary.status = 'FUNCTIONAL_CASES_PASSED'
    $summary.active_case = $null
}
catch {
    $summary.status = 'FAILED'; $summary.failure = $_.Exception.Message
    $summary['registry_at_failure'] = @(Get-R12Registration)
    throw
}
finally {
    $summary['finished_at_utc'] = [DateTime]::UtcNow.ToString('o')
    Save-R12Summary
    Write-Host "Scale evidence retained: $(Join-Path $script:RunRoot 'summary.json')"
}
