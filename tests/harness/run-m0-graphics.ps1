<#
.SYNOPSIS
Observe repeated, non-installing native M0 UI startup probes.
.DESCRIPTION
Windows PowerShell 5.1 / PowerShell 7. Runs only a pinned ui-smoke.exe with
--smoke-seconds 3. Does not run Setup, click windows, change display/security
settings, or terminate processes. Each mode has a first observation followed
by subsequent observations; OS/driver caches are not cleared or certified cold.
The 45-second observation limit includes log draining. A timeout preserves
observed evidence and stops the remaining probes without killing the process.
.PARAMETER UiProbePath
Defaults to target/ui-build/release/examples/ui-smoke.exe under the repository.
.PARAMETER OutputDirectory
New, non-existing result directory. Defaults to a unique target/m0-graphics run.
Relative paths are resolved against the repository root.
.PARAMETER Iterations
Number of probes per mode; default 5 means ten total process launches.
.PARAMETER PlanOnly
Print the plan without creating files or launching processes.
.PARAMETER MeasurementNote
Optional operator-provided context such as known simultaneous build load.
#>
[CmdletBinding()]
param(
    [string]$UiProbePath,
    [string]$OutputDirectory,
    [ValidateRange(1, 50)][int]$Iterations = 5,
    [string]$MeasurementNote = 'Host background workload was not controlled or measured.',
    [switch]$PlanOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$script:Utf8 = New-Object Text.UTF8Encoding($false)
$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))

function Assert-Condition([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Resolve-SafePath([string]$Path, [string]$Within) {
    if (-not [IO.Path]::IsPathRooted($Path)) { $Path = Join-Path $repositoryRoot $Path }
    $absolute = [IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    if ($Within) {
        $root = [IO.Path]::GetFullPath($Within).TrimEnd('\', '/')
        Assert-Condition ($absolute.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) "Path escapes run directory: $absolute"
    }
    $cursor = $absolute
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force
            Assert-Condition (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse path rejected: $cursor"
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if ($null -eq $parent) { break }
        $cursor = $parent.FullName
    }
    return $absolute
}

function Write-Json([string]$Path, [object]$Value) {
    $safe = Resolve-SafePath $Path $script:RunDirectory
    [IO.File]::WriteAllText($safe, ($Value | ConvertTo-Json -Depth 30) + [Environment]::NewLine, $script:Utf8)
}

function Read-ObservedLog([string]$Path) {
    # A timed-out process may still be writing. Read the bytes currently on
    # disk with sharing enabled; this is a bounded observation, not log EOF.
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    try {
        $reader = New-Object IO.StreamReader($stream, [Text.Encoding]::UTF8)
        try { return $reader.ReadToEnd() } finally { $reader.Dispose() }
    }
    finally { $stream.Dispose() }
}

function Copy-PinnedFile([string]$Source, [string]$Destination) {
    $sourcePath = Resolve-SafePath $Source ''
    $destinationPath = Resolve-SafePath $Destination $script:RunDirectory
    Assert-Condition (Test-Path -LiteralPath $sourcePath -PathType Leaf) "Missing input: $sourcePath"
    $before = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $read = [IO.File]::Open($sourcePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $write = [IO.File]::Open($destinationPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $read.CopyTo($write) } finally { $write.Dispose() }
    }
    finally { $read.Dispose() }
    $after = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $copied = (Get-FileHash -LiteralPath $destinationPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Assert-Condition ($before -ceq $after -and $after -ceq $copied) 'Input changed while being snapshotted; no probe was launched.'
    return [ordered]@{ original_path = $sourcePath; snapshot_path = $destinationPath; sha256 = $copied; bytes = (Get-Item -LiteralPath $destinationPath).Length }
}

if (-not $UiProbePath) { $UiProbePath = 'target\ui-build\release\examples\ui-smoke.exe' }
$sourceProbe = Resolve-SafePath $UiProbePath ''
if (-not $OutputDirectory) {
    $runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
    $OutputDirectory = 'target\m0-graphics\' + $runId
}
$script:RunDirectory = Resolve-SafePath $OutputDirectory ''
$modes = @('default', 'without-direct-composition')
$plan = [ordered]@{
    schema_version = 1
    status = 'PLAN_ONLY_NOT_EXECUTED'
    source_probe = $sourceProbe
    output_directory = $script:RunDirectory
    iterations_per_mode = $Iterations
    total_planned_probes = $Iterations * $modes.Count
    modes = @(
        [ordered]@{ name = 'default'; child_environment = 'Remove GPUI_DISABLE_DIRECT_COMPOSITION in this child only.' },
        [ordered]@{ name = 'without-direct-composition'; child_environment = 'Set GPUI_DISABLE_DIRECT_COMPOSITION=1 in this child only.' }
    )
    arguments = @('--smoke-seconds', '3')
    observation_limit_seconds = 45
    memory_sampling_target_interval_ms = 100
    measurement_note = $MeasurementNote
    launch_method = 'ProcessStartInfo: UseShellExecute=false, CreateNoWindow=true; GPUI may open its native window.'
    isolation = 'Pinned executable and harness under run/tools; TEMP, TMP and LOCALAPPDATA below each probe directory. Other process environment is inherited.'
    required_evidence = @('UI entry', 'window callback and scale', 'render callback and scale', 'frame callback', 'self-close timer', 'logger summary with zero errors', 'process exit 0 and complete logs')
    on_timeout = 'Preserve PID, process creation time, executable SHA-256 and observed logs; stop later probes; never kill.'
    acceptance = 'STARTUP_PROBES_PASSED is limited to the listed callbacks and zero recorded framework Error messages.'
    limitations = @(
        'UI elapsed_us starts at run_inner entry, excludes OS process loading, and is not interactive first frame or a GPU present fence.'
        'Working set/private bytes are non-atomic discrete observations of this probe process only, not exact peaks or whole-system memory. Host workload is not controlled; this is not an uncontended benchmark.'
        'The first observation and subsequent observations are separate; cold OS/driver caches are not established.'
        'No visual, clicking, focus, accessibility, multi-monitor movement, RDP, software-adapter, clean-image, signing, or protection acceptance is inferred.'
    )
}
if ($PlanOnly) { $plan | ConvertTo-Json -Depth 12; return }
Assert-Condition ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'This harness requires Windows.'
Assert-Condition (-not (Test-Path -LiteralPath $script:RunDirectory)) 'OutputDirectory already exists; choose a new directory.'
Assert-Condition (Test-Path -LiteralPath $sourceProbe -PathType Leaf) 'UiProbePath does not exist.'
$null = New-Item -ItemType Directory -Path $script:RunDirectory
$toolsDirectory = Resolve-SafePath (Join-Path $script:RunDirectory 'tools') $script:RunDirectory
$null = New-Item -ItemType Directory -Path $toolsDirectory
$snapshot = Copy-PinnedFile $sourceProbe (Join-Path $toolsDirectory 'ui-smoke.exe')
$harnessSnapshot = Copy-PinnedFile $PSCommandPath (Join-Path $toolsDirectory 'run-m0-graphics.ps1')

# The asynchronous readers run in C#, so their callbacks do not require a
# PowerShell runspace. Each captured line is flushed to disk immediately. On
# timeout, these files contain evidence observed so far, not a claim of EOF.
if (-not ('AvelinM0Graphics.Capture' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Threading.Tasks;
namespace AvelinM0Graphics {
    public sealed class Capture {
        readonly Task stdout, stderr;
        public Capture(Process process, string stdoutPath, string stderrPath) {
            stdout = Pump(process.StandardOutput, stdoutPath);
            stderr = Pump(process.StandardError, stderrPath);
        }
        static async Task Pump(StreamReader reader, string path) {
            using (FileStream file = new FileStream(path, FileMode.CreateNew, FileAccess.Write, FileShare.ReadWrite))
            using (StreamWriter writer = new StreamWriter(file, new UTF8Encoding(false))) {
                writer.AutoFlush = true;
                string line;
                while ((line = await reader.ReadLineAsync().ConfigureAwait(false)) != null) {
                    await writer.WriteLineAsync(line).ConfigureAwait(false);
                }
            }
        }
        public bool Complete { get { return stdout.IsCompleted && stderr.IsCompleted; } }
        public bool Faulted { get { return stdout.IsFaulted || stderr.IsFaulted; } }
        public string Error {
            get {
                string a = stdout.Exception == null ? "" : stdout.Exception.GetBaseException().Message;
                string b = stderr.Exception == null ? "" : stderr.Exception.GetBaseException().Message;
                return a + (a.Length != 0 && b.Length != 0 ? "; " : "") + b;
            }
        }
    }
}
'@
}

function Get-MarkerEvidence([string]$Text) {
    $clock = [ordered]@{ origin = 'UI run_inner entry; excludes OS process loading'; entry = $false; window_elapsed_us = $null; window_scale_factor = $null; render_elapsed_us = $null; render_scale_factor = $null; frame_callback_elapsed_us = $null; timer_elapsed_us = $null; meaning = 'Callback observations only; no interactive-first-frame or GPU-present guarantee.' }
    $missing = @()
    $entry = [regex]::Matches($Text, '(?m)^AVELIN_UI_PROBE_ENTRY elapsed_us=0\r?$')
    $clock.entry = $entry.Count -eq 1
    if (-not $clock.entry) { $missing += 'Exactly one UI entry marker is required.' }
    foreach ($marker in @('WINDOW', 'RENDER', 'FRAME_CALLBACK', 'TIMER')) {
        $suffix = ''
        if ($marker -eq 'WINDOW' -or $marker -eq 'RENDER') { $suffix = ' scale_factor=([0-9]+(?:\.[0-9]+)?)' }
        $found = [regex]::Matches($Text, ('(?m)^AVELIN_UI_PROBE_' + $marker + ' elapsed_us=(\d+)' + $suffix + '\r?$'))
        if ($found.Count -ne 1) { $missing += ('Exactly one ' + $marker + ' clock marker is required.'); continue }
        $clock[$marker.ToLowerInvariant() + '_elapsed_us'] = [long]$found[0].Groups[1].Value
        if ($suffix) { $clock[$marker.ToLowerInvariant() + '_scale_factor'] = [double]::Parse($found[0].Groups[2].Value, [Globalization.CultureInfo]::InvariantCulture) }
    }
    $summaryMarkers = [regex]::Matches($Text, '(?m)^AVELIN_UI_PROBE_LOG_SUMMARY errors=(\d+) warnings=(\d+)\r?$')
    $errors = $null; $warnings = $null
    if ($summaryMarkers.Count -eq 1) {
        $errors = [long]$summaryMarkers[0].Groups[1].Value
        $warnings = [long]$summaryMarkers[0].Groups[2].Value
    }
    else { $missing += 'Exactly one logger summary is required.' }
    $window = [regex]::IsMatch($Text, '(?m)^AVELIN_UI_WINDOW_CREATED\r?$')
    $render = [regex]::IsMatch($Text, '(?m)^AVELIN_UI_VIEW_RENDER_CALLED\r?$')
    $timer = [regex]::IsMatch($Text, '(?m)^AVELIN_UI_SMOKE_TIMEOUT elapsed_ms=3000\r?$')
    if (-not ($window -and $render -and $timer)) { $missing += 'Window, render, and 3-second self-close markers are required.' }
    $errorLines = [regex]::Matches($Text, '(?m)^AVELIN_UI_LOG level=ERROR ').Count
    $orderedClocks = $false
    if ($null -ne $clock.window_elapsed_us -and $null -ne $clock.render_elapsed_us -and $null -ne $clock.frame_callback_elapsed_us -and $null -ne $clock.timer_elapsed_us) {
        $orderedClocks = $clock.window_elapsed_us -le $clock.render_elapsed_us -and $clock.render_elapsed_us -le $clock.frame_callback_elapsed_us -and $clock.frame_callback_elapsed_us -le $clock.timer_elapsed_us
        if (-not $orderedClocks) { $missing += 'UI callback clock order is inconsistent.' }
    }
    return [ordered]@{
        clock = $clock; required_evidence_complete = ($missing.Count -eq 0); missing_or_conflicting_evidence = $missing
        window_created = $window; view_render_called = $render; self_close_timer_called = $timer; callback_clock_order_valid = $orderedClocks
        logger_summary_available = ($summaryMarkers.Count -eq 1); logged_errors = $errors; logged_warnings = $warnings; observed_error_log_lines = $errorLines
        renderer_lines = @($Text -split '\r?\n' | Where-Object { $_ -like 'AVELIN_UI_RENDERER*' })
        renderer_scope = 'Only the application-reported renderer lines above identify its selected device. Missing lines leave the selected renderer unknown.'
    }
}

function Invoke-UiProbe([string]$Mode, [int]$Iteration, [int]$GlobalOrdinal) {
    $id = $Mode + '-' + $Iteration.ToString('00')
    $caseDirectory = Resolve-SafePath (Join-Path $script:RunDirectory $id) $script:RunDirectory
    $null = New-Item -ItemType Directory -Path $caseDirectory
    $temp = Join-Path $caseDirectory 'temp'; $localData = Join-Path $caseDirectory 'local-app-data'
    foreach ($path in @($temp, $localData)) { $null = New-Item -ItemType Directory -Path $path }
    $stdoutPath = Join-Path $caseDirectory 'stdout.log'; $stderrPath = Join-Path $caseDirectory 'stderr.log'
    $memoryPath = Join-Path $caseDirectory 'memory-samples.jsonl'
    $memoryWriter = New-Object IO.StreamWriter($memoryPath, $false, $script:Utf8)
    $memoryWriter.AutoFlush = $true
    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = $snapshot.snapshot_path
    $startInfo.Arguments = '--smoke-seconds 3'
    $startInfo.WorkingDirectory = $caseDirectory
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.StandardOutputEncoding = $script:Utf8
    $startInfo.StandardErrorEncoding = $script:Utf8
    $startInfo.EnvironmentVariables['TEMP'] = $temp
    $startInfo.EnvironmentVariables['TMP'] = $temp
    $startInfo.EnvironmentVariables['LOCALAPPDATA'] = $localData
    if ($Mode -eq 'default') { $startInfo.EnvironmentVariables.Remove('GPUI_DISABLE_DIRECT_COMPOSITION') }
    else { $startInfo.EnvironmentVariables['GPUI_DISABLE_DIRECT_COMPOSITION'] = '1' }
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $startInfo
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $capture = $null; $lastSample = $null
    $sampleCount = 0; $sampleErrors = 0; $intervals = New-Object 'Collections.Generic.List[long]'
    $maxWorking = 0L; $maxPrivate = 0L
    $record = [ordered]@{
        id = $id; mode = $Mode; iteration = $Iteration; global_ordinal = $GlobalOrdinal
        first_observation_in_run = ($GlobalOrdinal -eq 1); first_observation_in_mode = ($Iteration -eq 1); cold_cache_verified = $false
        status = 'STARTING'; process_state = 'NOT_STARTED'; started_at_utc = [DateTime]::UtcNow.ToString('o'); pid = $null; process_creation_time_utc = $null; process_creation_time_filetime_utc = $null
        executable_path = $snapshot.snapshot_path; executable_sha256 = $snapshot.sha256; arguments = @('--smoke-seconds', '3')
        child_direct_composition_override = $(if ($Mode -eq 'default') { 'REMOVED_FROM_CHILD_ENVIRONMENT' } else { '1' })
        exit_code = $null; exit_observed_wall_ms = $null; observation_wall_ms = $null; stdout = ($id + '/stdout.log'); stderr = ($id + '/stderr.log'); logs_complete_at_eof = $false
        timed_out = $false; process_was_killed = $false; stop_remaining_probes = $false; memory = $null; observations = $null; error = $null
    }
    try {
        Assert-Condition ($process.Start()) 'Process.Start returned false.'
        $null = $process.Handle
        $record.pid = $process.Id
        $record.process_creation_time_utc = $process.StartTime.ToUniversalTime().ToString('o')
        $record.process_creation_time_filetime_utc = $process.StartTime.ToUniversalTime().ToFileTimeUtc()
        $record.process_state = 'RUNNING'
        $capture = New-Object AvelinM0Graphics.Capture($process, $stdoutPath, $stderrPath)
        Write-Json (Join-Path $caseDirectory 'process.json') $record
        while ($true) {
            $process.Refresh()
            $exited = $process.HasExited
            if ($exited -and $null -eq $record.exit_observed_wall_ms) {
                $record.exit_observed_wall_ms = $clock.ElapsedMilliseconds
                $record.exit_code = $process.ExitCode
                $record.process_state = 'EXITED'
            }
            if ($capture.Faulted) { throw ('Log capture failed: ' + $capture.Error) }
            if ($exited -and $capture.Complete) { $record.logs_complete_at_eof = $true; break }
            if ($clock.ElapsedMilliseconds -ge 45000) {
                $record.timed_out = $true
                $record.stop_remaining_probes = $true
                $record.status = 'OBSERVATION_TIMEOUT'
                $record.error = '45-second observation limit reached; process was not killed. Logs contain observations up to the cutoff and may lack EOF.'
                break
            }
            if (-not $exited) {
                $elapsed = $clock.ElapsedMilliseconds
                $interval = $null
                if ($null -ne $lastSample) { $interval = $elapsed - $lastSample; $intervals.Add([long]$interval) }
                $lastSample = $elapsed
                try {
                    $working = $process.WorkingSet64
                    $private = $process.PrivateMemorySize64
                    $sample = [ordered]@{ elapsed_ms = $elapsed; interval_since_previous_attempt_ms = $interval; status = 'Available'; working_set_bytes = $working; private_bytes = $private }
                    $sampleCount++
                    $maxWorking = [Math]::Max($maxWorking, $working); $maxPrivate = [Math]::Max($maxPrivate, $private)
                }
                catch { $sampleErrors++; $sample = [ordered]@{ elapsed_ms = $elapsed; interval_since_previous_attempt_ms = $interval; status = 'Unavailable'; error = $_.Exception.Message } }
                $memoryWriter.WriteLine(($sample | ConvertTo-Json -Compress))
            }
            Start-Sleep -Milliseconds 100
        }
        $text = ''
        if (Test-Path -LiteralPath $stderrPath) { $text = Read-ObservedLog $stderrPath }
        $record.observations = Get-MarkerEvidence $text
        if (-not $record.timed_out) {
            $passed = $record.exit_code -eq 0 -and $record.logs_complete_at_eof -and $record.observations.required_evidence_complete -and $record.observations.logged_errors -eq 0 -and $record.observations.observed_error_log_lines -eq 0
            $record.status = $(if ($passed) { 'STARTUP_PROBE_PASSED' } else { 'STARTUP_PROBE_FAILED' })
        }
    }
    catch {
        $record.status = 'HARNESS_OR_PROCESS_QUERY_FAILED'
        $record.error = $_.Exception.Message
        # On a harness failure, child state is uncertain; do not launch more
        # probes or close/kill its GUI. Preserve the identity already captured.
        $record.stop_remaining_probes = $true
        if ($record.pid) {
            try { $process.Refresh(); if ($process.HasExited) { $record.process_state = 'EXITED'; $record.exit_code = $process.ExitCode } else { $record.process_state = 'RUNNING' } }
            catch { $record.process_state = 'UNCONFIRMED' }
        }
    }
    finally {
        $clock.Stop(); $record.observation_wall_ms = $clock.ElapsedMilliseconds
        $memoryWriter.Dispose()
        $minimum = $null; $maximum = $null; $mean = $null
        if ($intervals.Count -gt 0) {
            $statistics = $intervals | Measure-Object -Minimum -Maximum -Average
            $minimum = $statistics.Minimum; $maximum = $statistics.Maximum; $mean = $statistics.Average
        }
        $record.memory = [ordered]@{
            scope = 'This exact process object/creation identity only; no PID rediscovery, child aggregation, or system total.'
            nature = 'Non-atomic discrete working set and private byte observations; maxima are sampled values, not exact peaks.'
            target_interval_ms = 100; successful_samples = $sampleCount; unavailable_samples = $sampleErrors
            observed_working_set_max_bytes = $(if ($sampleCount -gt 0) { $maxWorking } else { $null })
            observed_private_max_bytes = $(if ($sampleCount -gt 0) { $maxPrivate } else { $null })
            actual_interval_ms = [ordered]@{ count = $intervals.Count; minimum = $minimum; maximum = $maximum; average = $mean }
            samples = ($id + '/memory-samples.jsonl')
        }
        Write-Json (Join-Path $caseDirectory 'process.json') $record
        # Dispose closes our query handle only; it does not terminate a process.
        # Capture tasks retain streams while this host remains alive. No promise
        # is made about capturing output produced after an observation timeout.
        $process.Dispose()
    }
    return [pscustomobject]$record
}

$summary = [ordered]@{
    schema_version = 1; status = 'RUNNING'; started_at_utc = [DateTime]::UtcNow.ToString('o'); finished_at_utc = $null
    output_directory = $script:RunDirectory; powershell_version = $PSVersionTable.PSVersion.ToString()
    executable = $snapshot; harness = $harnessSnapshot; plan = $plan; cases = @(); groups = @()
    parent_direct_composition_override_present = ($null -ne [Environment]::GetEnvironmentVariable('GPUI_DISABLE_DIRECT_COMPOSITION', 'Process'))
    parent_environment_modified = $false; installations_executed = 0; full_gui_compatibility = 'NOT_VERIFIED'; failure = $null
}
Write-Json (Join-Path $script:RunDirectory 'summary.json') $summary
Write-Host "Graphics evidence: $script:RunDirectory"
try {
    $ordinal = 0; $stop = $false
    foreach ($mode in $modes) {
        for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
            $ordinal++
            Write-Host ("UI startup probe {0}/{1}: {2} iteration {3}" -f $ordinal, ($Iterations * $modes.Count), $mode, $iteration)
            $case = Invoke-UiProbe $mode $iteration $ordinal
            $summary.cases += $case
            Write-Json (Join-Path $script:RunDirectory 'summary.json') $summary
            if ($case.stop_remaining_probes) { $stop = $true; break }
        }
        if ($stop) { break }
    }
    foreach ($mode in $modes) {
        $groupCases = @($summary.cases | Where-Object { $_.mode -eq $mode })
        $summary.groups += [ordered]@{
            mode = $mode; expected_count = $Iterations; observed_count = $groupCases.Count
            first_observation = @($groupCases | Where-Object { $_.first_observation_in_mode } | ForEach-Object { $_.id })
            subsequent_observations = @($groupCases | Where-Object { -not $_.first_observation_in_mode } | ForEach-Object { $_.id })
            cold_cache_verified = $false
            status = $(if ($groupCases.Count -eq $Iterations -and @($groupCases | Where-Object { $_.status -ne 'STARTUP_PROBE_PASSED' }).Count -eq 0) { 'STARTUP_PROBES_PASSED' } else { 'STARTUP_PROBES_INCOMPLETE_OR_FAILED' })
        }
    }
    if ($stop) { $summary.status = 'STOPPED_WITH_UNCONFIRMED_OBSERVATION' }
    elseif (@($summary.cases | Where-Object { $_.status -ne 'STARTUP_PROBE_PASSED' }).Count -gt 0) { $summary.status = 'STARTUP_PROBES_FAILED' }
    else { $summary.status = 'STARTUP_PROBES_PASSED' }
}
catch { $summary.status = 'HARNESS_FAILED'; $summary.failure = $_.Exception.Message }
finally {
    $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o')
    Write-Json (Join-Path $script:RunDirectory 'summary.json') $summary
}
$summary | ConvertTo-Json -Depth 30
if ($summary.status -ne 'STARTUP_PROBES_PASSED') { exit 1 }
