<#
.SYNOPSIS
Exercise failure reporting and the real runtime diagnostic sink without opening a GUI or installing software.
.DESCRIPTION
PowerShell 7 on Windows. Pins both executables and this harness; isolates every
child's TEMP, TMP and LOCALAPPDATA below a unique target/m0-r4-diagnostics run.
The runtime is invoked only with help/version or an explicit install directory
and deliberately missing manifest/backend. The diagnostic example injects
synthetic Warn/Error records; it does not simulate a GPU fault.
Observation includes complete stdout/stderr EOF. Timeout/capture uncertainty
stops later processes and never kills a child. Existing evidence is never removed.
.PARAMETER OutputDirectory
A new directory beneath this repository's target/m0-r4-diagnostics directory.
.PARAMETER PlanOnly
Print the plan without creating directories or launching processes.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$RuntimePath,
    [Parameter(Mandatory)][string]$ProbePath,
    [string]$OutputDirectory,
    [switch]$PlanOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$script:Utf8 = [Text.UTF8Encoding]::new($false, $true)
$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$resultsRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'target\m0-r4-diagnostics'))

function Assert-Condition([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
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

function Write-Json([string]$Path, [object]$Value) {
    $safe = Resolve-SafePath $Path $script:RunDirectory
    [IO.File]::WriteAllText($safe, (ConvertTo-Json -InputObject $Value -Depth 30) + [Environment]::NewLine, $script:Utf8)
}

function Copy-PinnedFile([string]$Source, [string]$Destination) {
    $sourcePath = Resolve-SafePath $Source
    $destinationPath = Resolve-SafePath $Destination $script:RunDirectory
    Assert-Condition (Test-Path -LiteralPath $sourcePath -PathType Leaf) "Missing input: $sourcePath"
    $before = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $read = [IO.File]::Open($sourcePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $write = [IO.File]::Open($destinationPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $read.CopyTo($write) } finally { $write.Dispose() }
    } finally { $read.Dispose() }
    $after = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $copied = (Get-FileHash -LiteralPath $destinationPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Assert-Condition ($before -ceq $after -and $after -ceq $copied) 'Input changed while being pinned; no process was launched.'
    return [ordered]@{ original_path = $sourcePath; snapshot_path = $destinationPath; sha256 = $copied; bytes = (Get-Item -LiteralPath $destinationPath).Length }
}

$sourceRuntime = Resolve-SafePath $RuntimePath
$sourceProbe = Resolve-SafePath $ProbePath
if (-not $OutputDirectory) {
    $id = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
    $OutputDirectory = Join-Path $resultsRoot $id
}
$script:RunDirectory = Resolve-SafePath $OutputDirectory $resultsRoot
$specifications = @(
    [ordered]@{ id = 'D01-runtime-help'; kind = 'help'; executable = 'runtime'; arguments = @('--help'); block = 'None' },
    [ordered]@{ id = 'D02-runtime-version'; kind = 'help'; executable = 'runtime'; arguments = @('--version'); block = 'None' },
    [ordered]@{ id = 'D03-probe-help'; kind = 'help'; executable = 'probe'; arguments = @('--help'); block = 'None' },
    [ordered]@{ id = 'D04-probe-version'; kind = 'help'; executable = 'probe'; arguments = @('--version'); block = 'None' },
    [ordered]@{ id = 'D05-cli-failure-primary'; kind = 'failure'; executable = 'runtime'; arguments = @(); block = 'None' },
    [ordered]@{ id = 'D06-cli-failure-fallback'; kind = 'failure'; executable = 'runtime'; arguments = @(); block = 'Primary' },
    [ordered]@{ id = 'D07-cli-failure-no-log'; kind = 'failure'; executable = 'runtime'; arguments = @(); block = 'Both' },
    [ordered]@{ id = 'D08-probe-normal'; kind = 'normal'; executable = 'probe'; arguments = @('--scenario', 'normal'); block = 'None' },
    [ordered]@{ id = 'D09-probe-storm'; kind = 'storm'; executable = 'probe'; arguments = @('--scenario', 'storm'); block = 'None' },
    [ordered]@{ id = 'D10-probe-fallback'; kind = 'normal'; executable = 'probe'; arguments = @('--scenario', 'normal'); block = 'Primary' },
    [ordered]@{ id = 'D11-probe-no-log'; kind = 'normal'; executable = 'probe'; arguments = @('--scenario', 'normal'); block = 'Both' }
)
$plan = [ordered]@{
    schema_version = 1; status = 'PLAN_ONLY_NOT_EXECUTED'
    source_runtime = $sourceRuntime; source_probe = $sourceProbe; output_directory = $script:RunDirectory
    cases = $specifications; total_planned_processes = $specifications.Count
    failure_arguments = 'Explicit --install-dir, --report, --backend and --manifest; manifest/backend intentionally absent; --english; no --gui or --ui-demo.'
    observation_limit_seconds = 30; capture_limit_per_stream_bytes = 4 * 1024 * 1024
    isolation = 'Child-only TEMP, TMP and LOCALAPPDATA below each new case directory; parent environment unchanged.'
    blocking_method = 'Owned regular-file sentinels at local/Avelin and optionally temp/AvelinM0; no permission, policy or OS configuration changes.'
    on_timeout = 'Preserve process identity and observed output; stop later processes; never kill.'
    evidence_scope = 'Real CLI pre-dispatch failure and synthetic diagnostic sink behavior; no GUI, native dialog, GPU fault, backend, install or uninstall is intentionally started.'
    size_limits = 'Runtime JSONL <= 1 MiB, separate final summary <= 16 KiB. Probe exit 0 means the synthetic exercise completed; Error records are expected.'
}
if ($PlanOnly) { $plan | ConvertTo-Json -Depth 15; return }
Assert-Condition ($PSVersionTable.PSVersion.Major -ge 7) 'PowerShell 7 or later is required.'
Assert-Condition ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'Windows is required.'
Assert-Condition (-not (Test-Path -LiteralPath $script:RunDirectory)) 'OutputDirectory already exists; choose a new directory.'
Assert-Condition (Test-Path -LiteralPath $sourceRuntime -PathType Leaf) 'RuntimePath does not exist.'
Assert-Condition (Test-Path -LiteralPath $sourceProbe -PathType Leaf) 'ProbePath does not exist.'
$null = New-Item -ItemType Directory -Path $script:RunDirectory
$toolsDirectory = Join-Path $script:RunDirectory 'tools'
$null = New-Item -ItemType Directory -Path $toolsDirectory
$runtime = Copy-PinnedFile $sourceRuntime (Join-Path $toolsDirectory 'avelin-app.exe')
$probe = Copy-PinnedFile $sourceProbe (Join-Path $toolsDirectory 'runtime-diagnostics-probe.exe')
$harness = Copy-PinnedFile $PSCommandPath (Join-Path $toolsDirectory 'run-m0-diagnostics.ps1')

# Capture raw bytes in asynchronous C# pumps. Unlike line-based callbacks this
# preserves newline/NUL bytes exactly. UTF-8 is decoded strictly after full EOF.
if (-not ('AvelinM0Diagnostics.Capture' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.Threading;
using System.Threading.Tasks;
namespace AvelinM0Diagnostics {
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

function Invoke-ObservedProcess([object]$Executable, [string[]]$Arguments, [string]$CaseDirectory, [string]$TempDirectory, [string]$LocalDirectory) {
    $stdoutPath = Join-Path $CaseDirectory 'stdout.log'
    $stderrPath = Join-Path $CaseDirectory 'stderr.log'
    Assert-Condition (((Get-FileHash -LiteralPath $Executable.snapshot_path -Algorithm SHA256).Hash.ToLowerInvariant()) -ceq $Executable.sha256) 'Pinned executable changed before launch.'
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable.snapshot_path
    $startInfo.WorkingDirectory = $CaseDirectory
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.StandardOutputEncoding = $script:Utf8
    $startInfo.StandardErrorEncoding = $script:Utf8
    foreach ($argument in $Arguments) { $startInfo.ArgumentList.Add($argument) }
    $startInfo.Environment['TEMP'] = $TempDirectory
    $startInfo.Environment['TMP'] = $TempDirectory
    $startInfo.Environment['LOCALAPPDATA'] = $LocalDirectory
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $capture = $null
    $record = [ordered]@{
        status = 'STARTING'; process_state = 'NOT_STARTED'; pid = $null; process_creation_time_utc = $null; process_creation_time_filetime_utc = $null
        executable = $Executable; arguments = $Arguments; started_at_utc = [DateTime]::UtcNow.ToString('o')
        exit_code = $null; observation_wall_ms = $null; stdout = $stdoutPath; stderr = $stderrPath
        stdout_bytes = $null; stderr_bytes = $null; logs_complete_at_eof = $false; capture_limit_exceeded = $false
        timed_out = $false; process_was_killed = $false; stop_remaining_processes = $false; error = $null
    }
    try {
        Assert-Condition ($process.Start()) 'Process.Start returned false.'
        $null = $process.Handle
        $record.pid = $process.Id
        $record.process_creation_time_utc = $process.StartTime.ToUniversalTime().ToString('o')
        $record.process_creation_time_filetime_utc = $process.StartTime.ToUniversalTime().ToFileTimeUtc()
        $record.process_state = 'RUNNING'
        $capture = [AvelinM0Diagnostics.Capture]::new($process, $stdoutPath, $stderrPath)
        Write-Json (Join-Path $CaseDirectory 'process.json') $record
        while ($true) {
            $process.Refresh()
            if ($process.HasExited) { $record.process_state = 'EXITED'; $record.exit_code = $process.ExitCode }
            if ($capture.Faulted) { throw ('Output capture failed: ' + $capture.Error) }
            if ($capture.LimitExceeded) { throw 'Output exceeded the capture budget; retained evidence is incomplete.' }
            if ($process.HasExited -and $capture.Complete) { $record.logs_complete_at_eof = $true; $record.status = 'EXIT_AND_EOF_OBSERVED'; break }
            if ($clock.ElapsedMilliseconds -ge 30000) {
                $record.status = 'OBSERVATION_TIMEOUT'; $record.timed_out = $true; $record.stop_remaining_processes = $true
                $record.error = '30-second observation limit reached; the child was not killed. Later processes were stopped.'
                break
            }
            Start-Sleep -Milliseconds 50
        }
    }
    catch {
        $record.status = 'HARNESS_OR_CAPTURE_FAILED'; $record.error = $_.Exception.Message; $record.stop_remaining_processes = $true
        if ($record.pid) {
            try { $process.Refresh(); if ($process.HasExited) { $record.process_state = 'EXITED'; $record.exit_code = $process.ExitCode } else { $record.process_state = 'RUNNING' } }
            catch { $record.process_state = 'UNCONFIRMED' }
        }
    }
    finally {
        $clock.Stop(); $record.observation_wall_ms = $clock.ElapsedMilliseconds
        if ($null -ne $capture) {
            $record.stdout_bytes = $capture.OutputBytes; $record.stderr_bytes = $capture.ErrorBytes
            $record.capture_limit_exceeded = $capture.LimitExceeded
        }
        Write-Json (Join-Path $CaseDirectory 'process.json') $record
        # Dispose releases our handle; it does not terminate the child. Output
        # after a timeout is not certified complete even if capture later ends.
        $process.Dispose()
    }
    return $record
}

function Get-ChildFiles([string]$Directory) {
    return @([IO.Directory]::GetFiles($Directory, '*', [IO.SearchOption]::AllDirectories) | ForEach-Object {
        $path = Resolve-SafePath $_ $Directory
        [ordered]@{ path = $path; bytes = (Get-Item -LiteralPath $path).Length; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
    })
}

function Invoke-DiagnosticCase([object]$Specification) {
    $caseDirectory = Resolve-SafePath (Join-Path $script:RunDirectory $Specification.id) $script:RunDirectory
    $null = New-Item -ItemType Directory -Path $caseDirectory
    $temp = Join-Path $caseDirectory 'temp'; $local = Join-Path $caseDirectory 'local'
    $null = New-Item -ItemType Directory -Path $temp
    $null = New-Item -ItemType Directory -Path $local
    $sentinels = @()
    if ($Specification.block -in @('Primary', 'Both')) {
        $path = Join-Path $local 'Avelin'
        [IO.File]::WriteAllText($path, 'Owned primary regular-file blocking sentinel.', $script:Utf8)
        $sentinels += [ordered]@{ path = $path; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }
    }
    if ($Specification.block -eq 'Both') {
        $path = Join-Path $temp 'AvelinM0'
        [IO.File]::WriteAllText($path, 'Owned fallback regular-file blocking sentinel.', $script:Utf8)
        $sentinels += [ordered]@{ path = $path; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }
    }
    $installPath = Join-Path $caseDirectory 'never-created-install-root'
    $reportPath = Join-Path $caseDirectory 'never-created-report.json'
    $backendPath = Join-Path $caseDirectory 'missing-backend.exe'
    $manifestPath = Join-Path $caseDirectory 'missing-manifest.json'
    [string[]]$arguments = $Specification.arguments
    if ($Specification.kind -eq 'failure') {
        $arguments = @('--install-dir', $installPath, '--report', $reportPath, '--backend', $backendPath, '--manifest', $manifestPath, '--english')
    }
    foreach ($absent in @($installPath, $reportPath, $backendPath, $manifestPath)) {
        Assert-Condition (-not (Test-Path -LiteralPath $absent)) "Precondition violated; path must be absent: $absent"
    }
    $executable = if ($Specification.executable -eq 'runtime') { $runtime } else { $probe }
    $process = Invoke-ObservedProcess $executable $arguments $caseDirectory $temp $local
    $case = [ordered]@{
        id = $Specification.id; kind = $Specification.kind; blocking = $Specification.block
        status = 'NOT_VALIDATED'; process = $process; stop_remaining_processes = $process.stop_remaining_processes
        observations = [ordered]@{}; isolated_files = @(); error = $null
    }
    if ($process.stop_remaining_processes) {
        $case.status = 'STOPPED_WITH_UNCONFIRMED_OBSERVATION'; $case.error = $process.error
        Write-Json (Join-Path $caseDirectory 'validation.json') $case
        return $case
    }
    try {
        Assert-Condition ($process.logs_complete_at_eof -and $process.process_state -eq 'EXITED') 'Full exit and output EOF are required.'
        $stdout = [IO.File]::ReadAllText($process.stdout, $script:Utf8)
        $stderr = [IO.File]::ReadAllText($process.stderr, $script:Utf8)
        Assert-Condition (([IO.FileInfo]$process.stdout).Length -eq $process.stdout_bytes -and ([IO.FileInfo]$process.stderr).Length -eq $process.stderr_bytes) 'Captured byte totals must match the files.'
        Assert-Condition (-not [regex]::IsMatch($stdout + $stderr, 'AVELIN_UI_(PROBE_|WINDOW_CREATED|VIEW_RENDER_CALLED|RENDERER|SMOKE_)')) 'Unexpected GUI/render marker in a non-GUI exercise.'
        $case.observations.strict_utf8_and_complete_output = $true
        $case.observations.gui_render_markers_observed = $false
        foreach ($sentinel in $sentinels) {
            Assert-Condition ((Get-FileHash -LiteralPath $sentinel.path -Algorithm SHA256).Hash -ceq $sentinel.sha256) 'Owned blocking sentinel changed.'
        }
        foreach ($absent in @($installPath, $reportPath, $backendPath, $manifestPath)) {
            Assert-Condition (-not (Test-Path -LiteralPath $absent)) "Unexpected installation/input/report path created: $absent"
        }
        $case.isolated_files = @(Get-ChildFiles $local) + @(Get-ChildFiles $temp)
        $failureLogs = @($case.isolated_files | Where-Object { [IO.Path]::GetFileName($_.path) -like 'launch-error-*.json' })
        $runtimeLogs = @($case.isolated_files | Where-Object { [IO.Path]::GetExtension($_.path) -eq '.jsonl' })
        $summaryLogs = @($case.isolated_files | Where-Object { [IO.Path]::GetFileName($_.path) -like '*.summary.json' })
        Assert-Condition (@($case.isolated_files | Where-Object { [IO.Path]::GetExtension($_.path) -in @('.exe', '.dll', '.lock') }).Count -eq 0) 'Unexpected backend, payload or lock artifact.'

        if ($Specification.kind -eq 'help') {
            Assert-Condition ($process.exit_code -eq 0) 'Help/version must exit 0.'
            Assert-Condition ($stdout.Length -gt 0 -and $stderr.Length -eq 0) 'Help/version output is missing or stderr is unexpected.'
            Assert-Condition ($case.isolated_files.Count -eq 0) 'Help/version must not create logs or child-environment files.'
            $case.observations.logs_created = 0
        }
        elseif ($Specification.kind -eq 'failure') {
            Assert-Condition ($process.exit_code -eq 3) 'Explicit pre-dispatch runtime failure must remain exit 3.'
            Assert-Condition ($stdout.Length -eq 0) 'Failure path unexpectedly wrote to stdout.'
            Assert-Condition ($stderr.Contains('AVELIN_RUNTIME_ERROR') -and $stderr.Contains('Process exit code: 3')) 'Stable failure code/exit text missing.'
            Assert-Condition ($stderr.Contains('No confirmed task result is attached to this failure.')) 'Task NotReported explanation missing.'
            Assert-Condition ($runtimeLogs.Count -eq 0 -and $summaryLogs.Count -eq 0) 'Explicit CLI failure must not initialize the GUI diagnostic sink.'
            $case.observations.failure_code = 'AVELIN_RUNTIME_ERROR'
            $case.observations.task_evidence = 'NotReported'
            $case.observations.backend_launch_evidence = 'Manifest and backend absent before/after invocation; pre-dispatch failure; no install root, task report, backend/payload/lock artifact. No independent child-process trace collected.'
            if ($Specification.block -eq 'Both') {
                Assert-Condition ($failureLogs.Count -eq 0) 'Both blocked paths must produce no saved failure record.'
                Assert-Condition ($stderr.Contains('Failure log was NOT saved.') -and $stderr.Contains('Log write attempts failed:')) 'Unsuccessful save must be explicit on stderr.'
                Assert-Condition ($stderr.Contains((Join-Path $local 'Avelin')) -and $stderr.Contains((Join-Path $temp 'AvelinM0'))) 'Both failed attempt paths must remain visible.'
                $case.observations.saved_failure_record = $null
                $case.observations.runtime_initialized_in_saved_record = $null
            }
            else {
                Assert-Condition ($failureLogs.Count -eq 1) 'Exactly one saved failure record is required.'
                $path = $failureLogs[0].path
                $expectedBase = if ($Specification.block -eq 'Primary') { $temp } else { $local }
                $null = Resolve-SafePath $path $expectedBase
                $record = [IO.File]::ReadAllText($path, $script:Utf8) | ConvertFrom-Json
                Assert-Condition ($record.failure_code -ceq 'AVELIN_RUNTIME_ERROR' -and $record.process_exit_code -eq 3) 'Failure record code/exit mismatch.'
                Assert-Condition ($record.task_evidence.state -ceq 'NotReported') 'Failure record must preserve NotReported.'
                Assert-Condition ($record.runtime_diagnostics.initialized -eq $false -and $null -eq $record.runtime_diagnostics.log_path) 'GUI logger initialized during an explicit CLI failure.'
                Assert-Condition ($record.requested_report -ceq $reportPath) 'Requested report evidence was lost.'
                if ($Specification.block -eq 'Primary') {
                    Assert-Condition ($stderr.Contains('Log write attempts failed:') -and $stderr.Contains((Join-Path $local 'Avelin'))) 'Primary save failure reason must remain visible after fallback.'
                }
                $case.observations.saved_failure_record = $path
                $case.observations.runtime_initialized_in_saved_record = $false
                $case.observations.record = $record
            }
        }
        else {
            Assert-Condition ($process.exit_code -eq 0) 'Synthetic probe did not complete with exit 0.'
            Assert-Condition ($stderr.Length -eq 0) 'Synthetic probe wrote unexpected stderr.'
            $result = $stdout | ConvertFrom-Json
            Assert-Condition ($result.probe_kind -ceq 'synthetic_runtime_diagnostics' -and $result.status -ceq 'PROBE_COMPLETED') 'Synthetic probe envelope missing.'
            Assert-Condition ($result.registration_reused -and -not $result.graphics_initialized -and -not $result.gpu_fault_injected -and -not $result.installation_executed) 'Synthetic exercise scope/registration evidence mismatch.'
            $snapshot = $result.snapshot
            $expectedWarnings = if ($Specification.kind -eq 'storm') { 2001 } else { 1 }
            Assert-Condition ($snapshot.initialized -and $snapshot.error_count -eq 1 -and $snapshot.warning_count -eq $expectedWarnings) 'Injected log counts mismatch.'
            Assert-Condition ($failureLogs.Count -eq 0) 'Synthetic log Error must not create a task/runtime failure record.'
            if ($Specification.block -eq 'Both') {
                Assert-Condition ($null -eq $snapshot.log_path -and $null -eq $snapshot.summary_path) 'All-blocked probe must not claim a saved log/summary.'
                Assert-Condition ($snapshot.write_error.Length -gt 0 -and $snapshot.dropped_count -eq 2 -and $snapshot.bytes_written -eq 0) 'All-blocked sink failure/drop counts are missing.'
                Assert-Condition ($runtimeLogs.Count -eq 0 -and $summaryLogs.Count -eq 0) 'All-blocked paths unexpectedly contain logs.'
            }
            else {
                Assert-Condition ($runtimeLogs.Count -eq 1 -and $summaryLogs.Count -eq 1) 'One JSONL and one bounded final summary are required.'
                $expectedBase = if ($Specification.block -eq 'Primary') { $temp } else { $local }
                $logPath = Resolve-SafePath $snapshot.log_path $expectedBase
                $summaryPath = Resolve-SafePath $snapshot.summary_path $expectedBase
                Assert-Condition ($runtimeLogs[0].path -ceq $logPath -and $summaryLogs[0].path -ceq $summaryPath) 'Saved paths do not match the diagnostic snapshot.'
                Assert-Condition ($null -eq $snapshot.write_error -and $null -eq $snapshot.summary_write_error) 'Unexpected diagnostic persistence failure.'
                $raw = [IO.File]::ReadAllText($logPath, $script:Utf8)
                $logBytes = ([IO.FileInfo]$logPath).Length
                Assert-Condition ($logBytes -le 1MB -and $logBytes -eq $snapshot.bytes_written) 'JSONL file exceeded the cap or its byte count mismatched.'
                Assert-Condition ($raw.EndsWith("`n") -and $raw.Contains('\u0000') -and $raw.Contains('\n')) 'JSONL escapes or final complete newline missing.'
                $entries = @($raw.Split("`n", [StringSplitOptions]::RemoveEmptyEntries) | ForEach-Object { $_ | ConvertFrom-Json })
                foreach ($entry in $entries) {
                    $recordFields = @($entry.PSObject.Properties.Name)
                    $requiredFields = @('pid', 'time', 'level', 'target', 'message')
                    $allowedFields = $requiredFields + @('file', 'line')
                    Assert-Condition (@($requiredFields | Where-Object { $_ -notin $recordFields }).Count -eq 0 -and @($recordFields | Where-Object { $_ -notin $allowedFields }).Count -eq 0) 'JSONL record is missing required fields or contains unexpected fields.'
                    if ('file' -in $recordFields) {
                        Assert-Condition ($entry.file -is [string] -and [Text.Encoding]::UTF8.GetByteCount($entry.file) -le 256) 'JSONL caller file exceeds its UTF-8 bound or has an invalid type.'
                    }
                    if ('line' -in $recordFields) {
                        Assert-Condition (($entry.line -is [int] -or $entry.line -is [long]) -and $entry.line -ge 0 -and $entry.line -le 4294967295) 'JSONL caller line must be an unsigned 32-bit number.'
                    }
                    Assert-Condition ($entry.pid -eq $process.pid -and $entry.level -in @('ERROR', 'WARN') -and $entry.target -ceq 'avelin::synthetic_probe') 'JSONL process/level/target mismatch.'
                    Assert-Condition ($script:Utf8.GetByteCount($entry.message) -le 2048) 'Message exceeded its UTF-8 byte limit.'
                }
                Assert-Condition ($entries[0].message.Contains('中文') -and $entries[0].message.Contains([char]0) -and $entries[0].message.Contains("`n")) 'Chinese/newline/NUL content did not round-trip.'
                $sidecar = [IO.File]::ReadAllText($summaryPath, $script:Utf8) | ConvertFrom-Json
                Assert-Condition (([IO.FileInfo]$summaryPath).Length -le 16KB) 'Final summary exceeded its separate 16 KiB limit.'
                foreach ($name in @('initialized', 'log_path', 'error_count', 'warning_count', 'dropped_count', 'truncated_records', 'truncated', 'file_limit_reached', 'bytes_written', 'write_error', 'summary_path', 'summary_write_error')) {
                    Assert-Condition ($sidecar.$name -ceq $snapshot.$name) "Persisted summary field mismatch: $name"
                }
                if ($Specification.kind -eq 'storm') {
                    Assert-Condition ($snapshot.file_limit_reached -and $snapshot.truncated -and $snapshot.truncated_records -gt 0 -and $snapshot.dropped_count -gt 0) 'Storm truncation/file-budget/drop evidence is missing.'
                    Assert-Condition ($entries.Count + $snapshot.dropped_count -eq 2002) 'Persisted and dropped storm records must account for all attempts.'
                }
                else {
                    Assert-Condition ($entries.Count -eq 2 -and $snapshot.dropped_count -eq 0 -and -not $snapshot.truncated) 'Normal synthetic records were dropped or truncated.'
                }
                $case.observations.persisted_jsonl_records = $entries.Count
                $case.observations.jsonl_bytes = $logBytes
                $case.observations.separate_summary_bytes = ([IO.FileInfo]$summaryPath).Length
            }
            $case.observations.synthetic_probe = $result
        }
        $case.status = 'DIAGNOSTIC_CASE_PASSED'
    }
    catch { $case.status = 'DIAGNOSTIC_CASE_FAILED'; $case.error = $_.Exception.Message }
    Write-Json (Join-Path $caseDirectory 'validation.json') $case
    return $case
}

$originalEnvironment = @{}
foreach ($name in @('TEMP', 'TMP', 'LOCALAPPDATA')) { $originalEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$summary = [ordered]@{
    schema_version = 1; status = 'RUNNING'; started_at_utc = [DateTime]::UtcNow.ToString('o'); finished_at_utc = $null
    output_directory = $script:RunDirectory; powershell_version = $PSVersionTable.PSVersion.ToString()
    runtime = $runtime; probe = $probe; harness = $harness; plan = $plan; cases = @(); failure = $null
    parent_environment_modified = $false; installations_executed = 0; actual_gui_faults_injected = 0
    full_gui_compatibility = 'NOT_VERIFIED'; native_error_dialog_acceptance = 'NOT_VERIFIED'
}
Write-Json (Join-Path $script:RunDirectory 'summary.json') $summary
Write-Host "Diagnostic evidence: $script:RunDirectory"
try {
    $stop = $false
    foreach ($specification in $specifications) {
        Write-Host ("Diagnostic case: " + $specification.id)
        $case = Invoke-DiagnosticCase $specification
        $summary.cases += $case
        Write-Json (Join-Path $script:RunDirectory 'summary.json') $summary
        if ($case.stop_remaining_processes) { $stop = $true; break }
    }
    if ($stop) { $summary.status = 'STOPPED_WITH_UNCONFIRMED_OBSERVATION' }
    elseif (@($summary.cases | Where-Object { $_.status -ne 'DIAGNOSTIC_CASE_PASSED' }).Count -gt 0) { $summary.status = 'DIAGNOSTICS_FAILED' }
    elseif ($summary.cases.Count -ne $specifications.Count) { $summary.status = 'DIAGNOSTICS_INCOMPLETE' }
    else { $summary.status = 'DIAGNOSTICS_PASSED' }
}
catch { $summary.status = 'HARNESS_FAILED'; $summary.failure = $_.Exception.Message }
finally {
    foreach ($name in @('TEMP', 'TMP', 'LOCALAPPDATA')) {
        if ([Environment]::GetEnvironmentVariable($name, 'Process') -cne $originalEnvironment[$name]) { $summary.parent_environment_modified = $true }
    }
    if ($summary.parent_environment_modified) { $summary.status = 'HARNESS_FAILED'; $summary.failure = 'Parent environment unexpectedly changed.' }
    $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o')
    Write-Json (Join-Path $script:RunDirectory 'summary.json') $summary
}
$summary | ConvertTo-Json -Depth 30
if ($summary.status -ne 'DIAGNOSTICS_PASSED') { exit 1 }
