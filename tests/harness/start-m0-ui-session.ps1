<#
.SYNOPSIS
Launch one isolated, visible M0 UI session and retain process/log evidence.
.DESCRIPTION
PowerShell 7 on Windows. Demo uses --ui-demo. LaunchError uses --gui with
deliberately absent backend/manifest paths, exercising the native error dialog
before an installation worker starts. Connected uses a separately prepared,
isolated GUI fixture; an operator must initiate its installation in the UI.
This script never drives the UI or initiates installation by clicking Start.
An operator or an authorized computer-use tool closes the application normally.
The 600-second observation limit includes stdout/stderr EOF; expiry never kills
the process and never establishes a task, visual, installation or UI pass.
.PARAMETER RuntimePath
The GUI-enabled bare avelin-app.exe to copy, hash and launch for this session.
.PARAMETER RunLabel
Short filesystem-safe label; a timestamp and random suffix always make a new run.
.PARAMETER FixturePath
Connected only: fixture.json created by prepare-m0-gui-fixture.ps1.
.PARAMETER DelayMs
Connected only: existing runtime --step-delay-ms, allowing an observed cancel.
.PARAMETER PlanOnly
Print the plan without creating files, compiling the capture helper or launching.
#>
[CmdletBinding()]
param(
    [string]$RuntimePath,
    [ValidateSet('Demo', 'LaunchError', 'Connected')][string]$Mode = 'Demo',
    [string]$FixturePath,
    [ValidateRange(0, 10000)][int]$DelayMs = 0,
    [switch]$English,
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$')][string]$RunLabel = 'session',
    [switch]$PlanOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$script:Utf8 = [Text.UTF8Encoding]::new($false, $true)
$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$resultsRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'target\m0-r6-ui'))

function Assert-Condition([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Get-ValidatedConnectedOptions([object[]]$FixtureArguments, [object[]]$ExpectedArguments, [int]$PayloadFileCount) {
    $extraCount = $FixtureArguments.Count - $ExpectedArguments.Count
    Assert-Condition ($extraCount -ge 0 -and $extraCount -le 4 -and $extraCount % 2 -eq 0) 'Unexpected fixture argument count.'
    for ($index = 0; $index -lt $ExpectedArguments.Count; $index++) {
        Assert-Condition ($FixtureArguments[$index] -ceq $ExpectedArguments[$index]) "Unexpected fixture argument at index $index."
    }
    $options = [ordered]@{ delay_ms = 0; fault_after_files = 0 }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    for ($index = $ExpectedArguments.Count; $index -lt $FixtureArguments.Count; $index += 2) {
        $name = [string]$FixtureArguments[$index]
        Assert-Condition ($name -ceq '--step-delay-ms' -or $name -ceq '--fault-after-files') "Unsupported fixture option: $name"
        Assert-Condition ($seen.Add($name)) "Repeated fixture option: $name"
        $value = 0
        $raw = [string]$FixtureArguments[$index + 1]
        Assert-Condition ($raw -cmatch '^[0-9]+$' -and [int]::TryParse($raw, [ref]$value)) "Invalid fixture option value: $name"
        if ($name -ceq '--step-delay-ms') {
            Assert-Condition ($value -le 10000) 'Invalid fixture step delay.'
            $options.delay_ms = $value
        } else {
            Assert-Condition ($value -ge 1 -and $value -le 100000 -and $value -le $PayloadFileCount) 'Fault injection must name an existing payload file count.'
            $options.fault_after_files = $value
        }
    }
    return $options
}

function Resolve-SafePath([string]$Path, [string]$Within = '') {
    if (-not [IO.Path]::IsPathRooted($Path)) { $Path = Join-Path $repositoryRoot $Path }
    $absolute = [IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    if ($Within) {
        $boundary = [IO.Path]::GetFullPath($Within).TrimEnd('\', '/')
        Assert-Condition ($absolute.StartsWith($boundary + '\', [StringComparison]::OrdinalIgnoreCase)) "Path escapes its allowed directory: $absolute"
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

function Write-NewJson([string]$Path, [object]$Value, [string]$Within = $script:RunDirectory) {
    $safe = Resolve-SafePath $Path $Within
    $bytes = $script:Utf8.GetBytes((ConvertTo-Json -InputObject $Value -Depth 30) + [Environment]::NewLine)
    $stream = [IO.File]::Open($safe, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) }
    finally { $stream.Dispose() }
}

function Copy-PinnedFile([string]$Source, [string]$Destination) {
    $sourcePath = Resolve-SafePath $Source
    $destinationPath = Resolve-SafePath $Destination $script:RunDirectory
    $before = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $read = [IO.File]::Open($sourcePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $write = [IO.File]::Open($destinationPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $read.CopyTo($write) } finally { $write.Dispose() }
    } finally { $read.Dispose() }
    $after = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $copied = (Get-FileHash -LiteralPath $destinationPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Assert-Condition ($before -ceq $after -and $after -ceq $copied) 'Input changed while being pinned; no application was launched.'
    return [ordered]@{ original_path = $sourcePath; snapshot_path = $destinationPath; sha256 = $copied; bytes = (Get-Item -LiteralPath $destinationPath).Length }
}

function Get-DiagnosticFiles([string]$Directory) {
    if (-not (Test-Path -LiteralPath $Directory -PathType Container)) { return }
    $safe = Resolve-SafePath $Directory $script:EvidenceBoundary
    foreach ($item in Get-ChildItem -LiteralPath $safe -Force) {
        $path = Resolve-SafePath $item.FullName $script:EvidenceBoundary
        if ($item.PSIsContainer) { Get-DiagnosticFiles $path }
        elseif ($item.Name -like '*.summary.json' -or $item.Name -like 'launch-error-*.json' -or $item.Name -like '*.jsonl') {
            [ordered]@{ path = $path; bytes_observed = $item.Length; kind = if ($item.Name -like '*.summary.json') { 'runtime-summary' } elseif ($item.Name -like '*.jsonl') { 'runtime-jsonl' } else { 'failure-record' } }
        }
    }
}

function Assert-FixtureFiles([object]$Fixture, [string]$Root) {
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($entry in @($Fixture.inputs) + @($Fixture.artifacts)) {
        $path = Resolve-SafePath $entry.path $Root
        Assert-Condition ($seen.Add($path)) "Repeated fixture input/artifact: $path"
        Assert-Condition (Test-Path -LiteralPath $path -PathType Leaf) "Missing fixture evidence: $path"
        Assert-Condition ([string]$entry.sha256 -match '^[0-9a-fA-F]{64}$') "Invalid recorded SHA-256: $path"
        Assert-Condition ((Get-Item -LiteralPath $path).Length -eq [long]$entry.size) "Fixture evidence size changed: $path"
        Assert-Condition (((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash) -ieq $entry.sha256) "Fixture evidence hash changed: $path"
    }
    foreach ($name in @('runtime', 'backend', 'manifest')) {
        Assert-Condition ($seen.Contains($Fixture.paths[$name])) "Required fixture path is not covered by its hash index: $name"
    }
}

$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + $RunLabel + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
$faultAfterFiles = 0
$script:RunDirectory = Resolve-SafePath (Join-Path $resultsRoot $runId) $resultsRoot
$script:EvidenceBoundary = $script:RunDirectory
$fixture = $null
$fixtureRecord = $null
$fixtureGuardPath = $null
if ($Mode -ne 'Connected') {
    Assert-Condition (-not $FixturePath -and $DelayMs -eq 0) 'FixturePath and DelayMs are only supported in Connected mode.'
    Assert-Condition (-not [string]::IsNullOrWhiteSpace($RuntimePath)) 'RuntimePath is required in Demo and LaunchError modes.'
    $sourceRuntime = Resolve-SafePath $RuntimePath
} else {
    Assert-Condition (-not [string]::IsNullOrWhiteSpace($FixturePath)) 'FixturePath is required in Connected mode.'
    $fixtureFile = Resolve-SafePath $FixturePath
    Assert-Condition (Test-Path -LiteralPath $fixtureFile -PathType Leaf) 'FixturePath does not exist.'
    $fixture = [IO.File]::ReadAllText($fixtureFile, $script:Utf8) | ConvertFrom-Json -AsHashtable
    Assert-Condition ($fixture.schema_version -eq 1) 'Unsupported fixture schema.'
    $fixtureRoot = Resolve-SafePath $fixture.run_directory (Join-Path $repositoryRoot 'target\m0-r6-gui-fixture')
    Assert-Condition ([IO.Path]::GetDirectoryName($fixtureFile) -ieq $fixtureRoot) 'fixture.json must be directly inside its declared run directory.'
    Assert-Condition ([IO.Path]::GetFileName($fixtureFile) -ieq 'fixture.json') 'Connected expects fixture.json.'
    foreach ($entry in $fixture.paths.GetEnumerator()) {
        Assert-Condition ([IO.Path]::IsPathRooted([string]$entry.Value)) "Fixture path must be absolute: $($entry.Key)"
        $null = Resolve-SafePath $entry.Value $fixtureRoot
    }
    foreach ($name in @('runtime', 'backend', 'manifest')) {
        Assert-Condition (Test-Path -LiteralPath $fixture.paths[$name] -PathType Leaf) "Missing fixture file: $name"
    }
    Assert-Condition ($fixture.status -ceq 'BUILT_NOT_EXECUTED') 'Fixture must be newly built and not executed.'
    Assert-Condition (Test-Path -LiteralPath $fixture.paths.install_dir -PathType Container) 'Fixture install directory must be an existing empty directory.'
    Assert-Condition (@(Get-ChildItem -LiteralPath $fixture.paths.install_dir -Force).Count -eq 0) 'Fixture install directory is not empty; prepare a fresh fixture.'
    Assert-Condition (-not (Test-Path -LiteralPath $fixture.paths.report)) 'Fixture report already exists; prepare a fresh fixture.'
    foreach ($name in @('temporary_directory', 'local_app_data_directory')) {
        Assert-Condition (Test-Path -LiteralPath $fixture.paths[$name] -PathType Container) "Missing fixture directory: $name"
    }
    Assert-Condition ($fixture.child_environment.TEMP -ieq $fixture.paths.temporary_directory -and $fixture.child_environment.TMP -ieq $fixture.paths.temporary_directory -and $fixture.child_environment.LOCALAPPDATA -ieq $fixture.paths.local_app_data_directory) 'Fixture environment does not match its isolated paths.'
    $sourceRuntime = Resolve-SafePath $fixture.paths.runtime $fixtureRoot
    if ($RuntimePath) { Assert-Condition ((Resolve-SafePath $RuntimePath) -ieq $sourceRuntime) 'RuntimePath must match the fixture runtime.' }
    $script:RunDirectory = Resolve-SafePath (Join-Path $fixtureRoot ('ui-session-' + $runId)) $fixtureRoot
    $script:EvidenceBoundary = $fixtureRoot
    $fixtureGuardPath = Join-Path $fixtureRoot 'ui-launch-guard.json'
    Assert-Condition (-not (Test-Path -LiteralPath $fixtureGuardPath)) 'This fixture was already claimed by a UI session; prepare a fresh fixture.'
    $fixtureRecord = [ordered]@{ path = $fixtureFile; sha256 = (Get-FileHash -LiteralPath $fixtureFile -Algorithm SHA256).Hash.ToLowerInvariant(); run_directory = $fixtureRoot; app_id = $fixture.app_id; launch_guard = $fixtureGuardPath }
    Assert-FixtureFiles $fixture $fixtureRoot
    $expectedArguments = @('--gui', '--backend', $fixture.paths.backend, '--manifest', $fixture.paths.manifest, '--install-dir', $fixture.paths.install_dir, '--report', $fixture.paths.report, '--operation', 'install')
    $fixtureArguments = @($fixture.gui_arguments)
    $options = Get-ValidatedConnectedOptions $fixtureArguments $expectedArguments @($fixture.expected_fixture_files).Count
    $fixtureDelay = $options.delay_ms
    $faultAfterFiles = $options.fault_after_files
    if ($fixture.ContainsKey('failure_injection')) {
        $injection = $fixture.failure_injection
        $expectedKind = if ($faultAfterFiles) { 'RUST_WRITE_STAGE_AFTER_FILES' } else { 'NONE' }
        $expectedResult = if ($faultAfterFiles) { 'Failed' } else { $null }
        Assert-Condition ($injection.kind -ceq $expectedKind -and $injection.after_files -eq $faultAfterFiles -and $injection.expected_task_result -ceq $expectedResult -and $injection.automatic_cancellation -ceq $false) 'Fixture injection metadata differs from its recorded arguments.'
    }
    if ($PSBoundParameters.ContainsKey('DelayMs')) { Assert-Condition ($DelayMs -eq $fixtureDelay) 'DelayMs must equal the value chosen when preparing the fixture.' }
    $DelayMs = $fixtureDelay
}
$toolsDirectory = Join-Path $script:RunDirectory 'tools'
$tempDirectory = if ($Mode -eq 'Connected') { $fixture.paths.temporary_directory } else { Join-Path $script:RunDirectory 'temp' }
$localDirectory = if ($Mode -eq 'Connected') { $fixture.paths.local_app_data_directory } else { Join-Path $script:RunDirectory 'local' }
$backendPath = Join-Path $script:RunDirectory 'missing.exe'
$manifestPath = Join-Path $script:RunDirectory 'missing.json'
$arguments = @(if ($Mode -eq 'Demo') { '--ui-demo' } elseif ($Mode -eq 'LaunchError') { '--gui', '--backend', $backendPath, '--manifest', $manifestPath } else {
    @($fixture.gui_arguments)
})
if ($English) { $arguments += '--english' }
$plan = [ordered]@{
    schema_version = 1; status = 'PLAN_ONLY_NOT_EXECUTED'; mode = $Mode; english = [bool]$English; run_label = $RunLabel
    source_runtime = $sourceRuntime; run_directory = $script:RunDirectory
    runtime_snapshot = if ($Mode -eq 'Connected') { $sourceRuntime } else { Join-Path $toolsDirectory 'avelin-app.exe' }; arguments = @($arguments)
    fixture = $fixtureRecord; delay_ms = $DelayMs
    fault_after_files = $faultAfterFiles
    failure_injection = if ($faultAfterFiles) { 'Rust write-stage injection; installation must still be started in the GUI. This is not a real graphics fault.' } else { 'None' }
    child_environment = [ordered]@{ TEMP = $tempDirectory; TMP = $tempDirectory; LOCALAPPDATA = $localDirectory }
    observation_limit_seconds = 600; capture_limit_per_stream_bytes = 4 * 1024 * 1024
    launch_method = 'ProcessStartInfo; WindowStyle=Normal, UseShellExecute=false, CreateNoWindow=true suppresses only an extra console; the application creates its visible GPUI window or native MessageBox.'
    isolation = 'Only child TEMP, TMP and LOCALAPPDATA are changed. Other environment is inherited. The parent environment is unchanged.'
    on_timeout = 'Record Unknown, PID, creation time and current output; do not kill, retry, close UI or delete evidence. An ending observer can close redirected pipes; output after timeout is not certified complete.'
    evidence_scope = 'Process identity, actual exit code, captured output and diagnostic locations only. No task, installation, visual, keyboard, GPU or cross-environment acceptance is inferred.'
}
if ($PlanOnly) { $plan | ConvertTo-Json -Depth 12; return }
Assert-Condition ($PSVersionTable.PSVersion.Major -ge 7) 'PowerShell 7 or later is required.'
Assert-Condition ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'Windows is required.'
Assert-Condition (Test-Path -LiteralPath $sourceRuntime -PathType Leaf) 'RuntimePath must identify the GUI-enabled bare runtime executable.'
Assert-Condition (-not (Test-Path -LiteralPath $script:RunDirectory)) 'The generated run directory already exists.'
$null = New-Item -ItemType Directory -Path $script:RunDirectory
$null = New-Item -ItemType Directory -Path $toolsDirectory
if ($Mode -ne 'Connected') {
    foreach ($directory in @($tempDirectory, $localDirectory)) { $null = New-Item -ItemType Directory -Path $directory }
    $runtime = Copy-PinnedFile $sourceRuntime $plan.runtime_snapshot
} else {
    $runtime = [ordered]@{ original_path = $sourceRuntime; snapshot_path = $sourceRuntime; sha256 = (Get-FileHash -LiteralPath $sourceRuntime -Algorithm SHA256).Hash.ToLowerInvariant(); bytes = (Get-Item -LiteralPath $sourceRuntime).Length }
}
$harness = Copy-PinnedFile $PSCommandPath (Join-Path $toolsDirectory 'start-m0-ui-session.ps1')
Write-NewJson (Join-Path $script:RunDirectory 'plan.json') $plan

# Raw asynchronous pumps preserve the application's UTF-8 bytes and flush while
# the operator is interacting. Truncated streams are drained to avoid blocking
# the child, but are explicitly recorded as incomplete evidence.
if (-not ('AvelinM0UiSessionV1.Capture' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.Threading;
using System.Threading.Tasks;
namespace AvelinM0UiSessionV1 {
    public sealed class Capture {
        readonly Task stdout, stderr;
        long outputBytes, errorBytes;
        volatile bool limitExceeded;
        public Capture(Process process, string output, string error) {
            stdout = Pump(process.StandardOutput.BaseStream, output, true);
            stderr = Pump(process.StandardError.BaseStream, error, false);
        }
        async Task Pump(Stream input, string path, bool isOutput) {
            const int limit = 4 * 1024 * 1024;
            long observed = 0;
            using (var file = new FileStream(path, FileMode.CreateNew, FileAccess.Write, FileShare.ReadWrite, 8192, true)) {
                var buffer = new byte[8192];
                int count;
                while ((count = await input.ReadAsync(buffer, 0, buffer.Length).ConfigureAwait(false)) != 0) {
                    int retained = (int)Math.Min(count, Math.Max(0, limit - observed));
                    observed += count;
                    if (retained > 0) {
                        await file.WriteAsync(buffer, 0, retained).ConfigureAwait(false);
                        await file.FlushAsync().ConfigureAwait(false);
                    }
                    if (retained != count) limitExceeded = true;
                    if (isOutput) Interlocked.Exchange(ref outputBytes, observed);
                    else Interlocked.Exchange(ref errorBytes, observed);
                }
            }
        }
        public bool Complete { get { return stdout.IsCompleted && stderr.IsCompleted; } }
        public bool Faulted { get { return stdout.IsFaulted || stderr.IsFaulted; } }
        public bool LimitExceeded { get { return limitExceeded; } }
        public long OutputBytes { get { return Interlocked.Read(ref outputBytes); } }
        public long ErrorBytes { get { return Interlocked.Read(ref errorBytes); } }
        public string Error {
            get {
                string a = stdout.Exception == null ? "" : stdout.Exception.GetBaseException().Message;
                string b = stderr.Exception == null ? "" : stderr.Exception.GetBaseException().Message;
                return a + (a.Length > 0 && b.Length > 0 ? "; " : "") + b;
            }
        }
    }
}
'@
}

$stdoutPath = Join-Path $script:RunDirectory 'stdout.log'
$stderrPath = Join-Path $script:RunDirectory 'stderr.log'
$startInfo = [Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = $runtime.snapshot_path
$startInfo.WorkingDirectory = $script:RunDirectory
$startInfo.UseShellExecute = $false
$startInfo.WindowStyle = [Diagnostics.ProcessWindowStyle]::Normal
$startInfo.CreateNoWindow = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
$startInfo.StandardOutputEncoding = $script:Utf8
$startInfo.StandardErrorEncoding = $script:Utf8
foreach ($argument in $arguments) { $startInfo.ArgumentList.Add($argument) }
foreach ($entry in $plan.child_environment.GetEnumerator()) { $startInfo.Environment[$entry.Key] = $entry.Value }
$process = [Diagnostics.Process]::new()
$process.StartInfo = $startInfo
$capture = $null
$clock = [Diagnostics.Stopwatch]::StartNew()
$record = [ordered]@{
    schema_version = 1; status = 'STARTING'; process_state = 'NOT_STARTED'; outcome = 'Unknown'
    observation_status = 'STARTING'
    mode = $Mode; english = [bool]$English; run_label = $RunLabel; run_directory = $script:RunDirectory
    runtime = $runtime; harness = $harness; arguments = @($arguments); child_environment = $plan.child_environment
    fixture = $fixtureRecord; executable = $runtime.snapshot_path; executable_sha256 = $runtime.sha256
    temporary_directory = $tempDirectory; local_app_data_directory = $localDirectory
    pid = $null; process_creation_time_utc = $null; process_creation_time_filetime_utc = $null
    process_id = $null
    started_at_utc = [DateTime]::UtcNow.ToString('o'); ended_at_utc = $null; observation_ended_at_utc = $null
    exit_code = $null; observation_wall_ms = $null; observation_limit_seconds = 600
    stdout = $stdoutPath; stderr = $stderrPath; stdout_bytes_observed = $null; stderr_bytes_observed = $null
    stdout_path = $stdoutPath; stderr_path = $stderrPath
    logs_complete_at_eof = $false; output_utf8_valid = $null; capture_limit_exceeded = $false
    timed_out = $false; process_was_killed = $false; error = $null
    runtime_summary_markers = @(); diagnostic_files = @(); diagnostic_scan_error = $null
    evidence_scope = $plan.evidence_scope
}
try {
    Assert-Condition (((Get-FileHash -LiteralPath $runtime.snapshot_path -Algorithm SHA256).Hash.ToLowerInvariant()) -ceq $runtime.sha256) 'Pinned runtime changed before launch.'
    if ($Mode -eq 'Connected') {
        Assert-Condition (((Get-FileHash -LiteralPath $fixtureRecord.path -Algorithm SHA256).Hash.ToLowerInvariant()) -ceq $fixtureRecord.sha256) 'Fixture record changed before launch.'
        Assert-FixtureFiles $fixture $fixtureRoot
        Assert-Condition (@(Get-ChildItem -LiteralPath $fixture.paths.install_dir -Force).Count -eq 0 -and -not (Test-Path -LiteralPath $fixture.paths.report)) 'Fixture is no longer fresh.'
        Write-NewJson $fixtureGuardPath ([ordered]@{ schema_version = 1; status = 'UI_SESSION_CLAIMED'; fixture_sha256 = $fixtureRecord.sha256; ui_run_directory = $script:RunDirectory; claimed_at_utc = [DateTime]::UtcNow.ToString('o'); reuse = 'Never reuse this fixture after any attempted UI launch; retain it for verification.' }) $fixtureRoot
    }
    if ($Mode -eq 'LaunchError') {
        Assert-Condition (-not (Test-Path -LiteralPath $backendPath) -and -not (Test-Path -LiteralPath $manifestPath)) 'LaunchError requires both fixture paths to be absent.'
    }
    Assert-Condition ($process.Start()) 'Process.Start returned false.'
    $null = $process.Handle
    $record.pid = $process.Id
    $record.process_id = $process.Id
    $record.process_creation_time_utc = $process.StartTime.ToUniversalTime().ToString('o')
    $record.process_creation_time_filetime_utc = $process.StartTime.ToUniversalTime().ToFileTimeUtc()
    $record.process_state = 'RUNNING'
    $record.status = 'OBSERVING_INTERACTIVE_SESSION'
    $capture = [AvelinM0UiSessionV1.Capture]::new($process, $stdoutPath, $stderrPath)
    Write-NewJson (Join-Path $script:RunDirectory 'launch.json') $record
    Write-Output ('AVELIN_UI_SESSION_STARTED ' + (ConvertTo-Json -Compress -InputObject ([ordered]@{ run_directory = $script:RunDirectory; pid = $record.pid; process_creation_time_utc = $record.process_creation_time_utc; mode = $Mode; runtime_sha256 = $runtime.sha256 })))
    while ($true) {
        $process.Refresh()
        if ($process.HasExited) {
            $record.process_state = 'EXITED'
            $record.exit_code = $process.ExitCode
            $record.ended_at_utc = $process.ExitTime.ToUniversalTime().ToString('o')
        }
        if ($capture.Faulted) { throw ('Output capture failed: ' + $capture.Error) }
        if ($process.HasExited -and $capture.Complete) {
            $record.logs_complete_at_eof = -not $capture.LimitExceeded
            $record.status = if ($capture.LimitExceeded) { 'EXIT_OBSERVED_CAPTURE_TRUNCATED' } else { 'EXIT_AND_EOF_OBSERVED' }
            $record.outcome = 'ExitObserved'
            break
        }
        if ($clock.ElapsedMilliseconds -ge 600000) {
            $record.status = 'OBSERVATION_TIMEOUT'
            $record.timed_out = $true
            $record.error = '600-second observation limit reached. The application was not killed or closed; output after this observation is unconfirmed.'
            break
        }
        Start-Sleep -Milliseconds 100
    }
}
catch {
    $record.status = 'HARNESS_OR_CAPTURE_FAILED'
    $record.error = $_.Exception.Message
    if ($null -ne $record.pid) {
        try {
            $process.Refresh()
            if ($process.HasExited) {
                $record.process_state = 'EXITED'; $record.exit_code = $process.ExitCode
                $record.ended_at_utc = $process.ExitTime.ToUniversalTime().ToString('o')
            } else { $record.process_state = 'RUNNING' }
        } catch { $record.process_state = 'UNCONFIRMED' }
    }
}
finally {
    $clock.Stop()
    $record.observation_wall_ms = $clock.ElapsedMilliseconds
    $record.observation_ended_at_utc = [DateTime]::UtcNow.ToString('o')
    if ($null -ne $capture) {
        $record.stdout_bytes_observed = $capture.OutputBytes
        $record.stderr_bytes_observed = $capture.ErrorBytes
        $record.capture_limit_exceeded = $capture.LimitExceeded
    }
    if ($record.logs_complete_at_eof) {
        try {
            $null = $script:Utf8.GetString([IO.File]::ReadAllBytes($stdoutPath))
            $stderrText = $script:Utf8.GetString([IO.File]::ReadAllBytes($stderrPath))
            $record.output_utf8_valid = $true
            foreach ($match in [regex]::Matches($stderrText, '(?m)^AVELIN_RUNTIME_LOG_SUMMARY (.+)\r?$')) {
                try { $record.runtime_summary_markers += ($match.Groups[1].Value | ConvertFrom-Json -AsHashtable) }
                catch { $record.runtime_summary_markers += [ordered]@{ parse_error = $_.Exception.Message; raw = $match.Groups[1].Value } }
            }
        } catch {
            $record.output_utf8_valid = $false
            $record.status = 'EXIT_OBSERVED_INVALID_UTF8'
            $record.error = $_.Exception.Message
        }
    }
    try {
        $record.diagnostic_files = @((Get-DiagnosticFiles $localDirectory); (Get-DiagnosticFiles $tempDirectory))
    } catch { $record.diagnostic_scan_error = $_.Exception.Message }
    if (-not (Test-Path -LiteralPath (Join-Path $script:RunDirectory 'launch.json'))) {
        Write-NewJson (Join-Path $script:RunDirectory 'launch.json') $record
    }
    $record.observation_status = $record.status
    $record.status = $record.process_state
    Write-NewJson (Join-Path $script:RunDirectory 'process.json') $record
    # Keep the Process object and asynchronous pumps alive throughout observation.
    # On uncertainty do not explicitly close redirected pipes or the process;
    # if the caller exits its PowerShell host, its handles may nevertheless close.
    if ($record.process_state -eq 'EXITED' -and $null -ne $capture -and $capture.Complete) { $process.Dispose() }
}
Write-Output ('AVELIN_UI_SESSION_OBSERVATION ' + (ConvertTo-Json -Compress -InputObject ([ordered]@{ run_directory = $script:RunDirectory; status = $record.status; process_state = $record.process_state; pid = $record.pid; exit_code = $record.exit_code; logs_complete_at_eof = $record.logs_complete_at_eof; acceptance = 'NOT_ASSESSED' })))
