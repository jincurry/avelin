<#
.SYNOPSIS
Open one long-lived native M0 window for a recorded manual acceptance case.
.DESCRIPTION
Windows PowerShell 5.1 x64 or PowerShell 7, without development tools. Verifies
the complete supplied sealed kit before making a new evidence directory. Demo
uses --ui-demo; LaunchError uses deliberately missing backend/manifest paths.
Neither mode starts a real installation. This script never drives or closes UI,
changes security settings, kills a process, retries, or certifies a manual pass.
Checksums detect changes; they do not authenticate an unsigned kit.
.PARAMETER KitDirectory
Existing sealed compatibility kit, including kit.json and checksums.json.
.PARAMETER ScenarioId
Operator-selected case identifier, recorded without inferring its result.
.PARAMETER ImageId
Operator-declared machine/image label; not proof of a clean environment.
.PARAMETER OutputDirectory
New directory outside the kit. Omit to create a unique directory in the current
filesystem location. An existing path is always rejected, including PlanOnly.
.PARAMETER PlanOnly
Validate the entire kit and print the plan. Create no files or processes.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$KitDirectory,
    [Parameter(Mandatory = $true)][ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$')][string]$ScenarioId,
    [Parameter(Mandatory = $true)][ValidateLength(1, 256)][string]$ImageId,
    [ValidateSet('Demo', 'LaunchError')][string]$Mode = 'Demo',
    [switch]$English,
    [string]$OutputDirectory,
    [ValidateRange(60, 3600)][int]$ObservationTimeoutSeconds = 1200,
    [switch]$PlanOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$script:Utf8 = New-Object Text.UTF8Encoding($false, $true)

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Get-SafePath([string]$Path, [string]$Within = '') {
    Assert-True (-not [string]::IsNullOrWhiteSpace($Path)) 'A filesystem path is required.'
    if (-not [IO.Path]::IsPathRooted($Path)) { $Path = Join-Path (Get-Location).ProviderPath $Path }
    $absolute = [IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    if ($Within) {
        $boundary = [IO.Path]::GetFullPath($Within).TrimEnd('\', '/')
        Assert-True ($absolute.StartsWith($boundary + '\', [StringComparison]::OrdinalIgnoreCase)) "Path escapes its boundary: $absolute"
    }
    $cursor = $absolute
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force
            Assert-True (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse point rejected: $cursor"
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if ($null -eq $parent) { break }
        $cursor = $parent.FullName
    }
    return $absolute
}

function Resolve-KitFile([string]$RelativePath) {
    Assert-True (-not [string]::IsNullOrWhiteSpace($RelativePath)) 'Empty kit path.'
    Assert-True (-not [IO.Path]::IsPathRooted($RelativePath)) 'Kit paths must be relative.'
    Assert-True ($RelativePath -notmatch '(^|[\\/])\.\.?([\\/]|$)|:') "Ambiguous kit path: $RelativePath"
    return Get-SafePath (Join-Path $script:KitRoot $RelativePath) $script:KitRoot
}

function Read-Json([string]$Path) {
    return ([IO.File]::ReadAllText($Path, $script:Utf8) | ConvertFrom-Json)
}

function Get-TreeFiles([string]$Directory) {
    foreach ($item in @(Get-ChildItem -LiteralPath $Directory -Force)) {
        $safe = Get-SafePath $item.FullName $script:KitRoot
        if ($item.PSIsContainer) { Get-TreeFiles $safe } else { $safe }
    }
}

function Test-Kit {
    $indexPath = Resolve-KitFile 'checksums.json'
    $indexHashBefore = (Get-FileHash -LiteralPath $indexPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $index = Read-Json $indexPath
    Assert-True ($index.schema_version -eq 1 -and $index.algorithm -ceq 'SHA-256') 'Unsupported checksum schema or algorithm.'
    $script:Indexed = @{}
    foreach ($file in @($index.files)) {
        $relative = ([string]$file.path).Replace('\', '/')
        Assert-True ($relative -ine 'checksums.json' -and -not $script:Indexed.ContainsKey($relative)) "Duplicate or self-referencing checksum entry: $relative"
        $path = Resolve-KitFile $relative
        Assert-True (Test-Path -LiteralPath $path -PathType Leaf) "Kit file missing: $relative"
        Assert-True ([string]$file.sha256 -cmatch '^[0-9a-f]{64}$' -and [long]$file.size -ge 0) "Invalid checksum entry: $relative"
        Assert-True ((Get-Item -LiteralPath $path).Length -eq [long]$file.size) "Kit size differs: $relative"
        Assert-True ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $file.sha256) "Kit hash differs: $relative"
        $script:Indexed[$relative] = $file
    }
    $all = @(Get-TreeFiles $script:KitRoot)
    Assert-True ($all.Count -eq ($script:Indexed.Count + 1)) 'Kit contains files absent from checksums.json.'
    foreach ($path in $all) {
        $relative = $path.Substring($script:KitRoot.Length + 1).Replace('\', '/')
        Assert-True ($relative -ceq 'checksums.json' -or $script:Indexed.ContainsKey($relative)) "Unlisted kit file: $relative"
    }
    Assert-True ($script:Indexed.ContainsKey('kit.json')) 'kit.json is not indexed.'
    $script:Kit = Read-Json (Resolve-KitFile 'kit.json')
    Assert-True ($script:Kit.schema_version -eq 1 -and $script:Kit.app_id -match '^dev\.avelin\.m0\.compat\.[a-f0-9]{32}$') 'Unexpected kit identity or schema.'
    Assert-True ($script:Kit.paths.runtime -ceq 'tools/avelin-app.exe' -and $script:Indexed.ContainsKey('tools/avelin-app.exe')) 'The indexed bare runtime tools/avelin-app.exe is required.'
    $indexHashAfter = (Get-FileHash -LiteralPath $indexPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Assert-True ($indexHashBefore -ceq $indexHashAfter) 'Checksum index changed during verification.'
    return [ordered]@{
        status = 'VERIFIED'; indexed_file_count = $script:Indexed.Count
        checksums_sha256 = $indexHashAfter; kit_id = $script:Kit.kit_id
        authenticity = 'NOT_ESTABLISHED_BY_UNSIGNED_CHECKSUMS'
        verified_at_utc = [DateTime]::UtcNow.ToString('o')
    }
}

function Write-NewJson([string]$Name, [object]$Value) {
    $path = Get-SafePath (Join-Path $script:RunDirectory $Name) $script:RunDirectory
    $bytes = $script:Utf8.GetBytes((ConvertTo-Json -InputObject $Value -Depth 30) + [Environment]::NewLine)
    $file = [IO.File]::Open($path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try { $file.Write($bytes, 0, $bytes.Length); $file.Flush($true) } finally { $file.Dispose() }
}

function Copy-PinnedFile([string]$Source, [string]$Name, [string]$ExpectedHash = '') {
    $sourcePath = Get-SafePath $Source
    $destinationPath = Get-SafePath (Join-Path $script:RunDirectory $Name) $script:RunDirectory
    $before = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($ExpectedHash) { Assert-True ($before -ceq $ExpectedHash) "Verified input changed before pinning: $sourcePath" }
    $read = [IO.File]::Open($sourcePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $write = [IO.File]::Open($destinationPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $read.CopyTo($write) } finally { $write.Dispose() }
    } finally { $read.Dispose() }
    $after = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash.ToLowerInvariant()
    $copied = (Get-FileHash -LiteralPath $destinationPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Assert-True ($before -ceq $after -and $after -ceq $copied) 'Input changed while pinning; no application was launched.'
    return [ordered]@{ original_path = $sourcePath; snapshot_path = $destinationPath; sha256 = $copied; bytes = (Get-Item -LiteralPath $destinationPath).Length }
}

# .NET Framework has no ArgumentList. Pass Windows C argv directly to
# CreateProcess with quoting; never evaluate command text through a shell.
function Quote-Argument([AllowEmptyString()][string]$Value) {
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}

Assert-True ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT -and [Environment]::Is64BitOperatingSystem -and [Environment]::Is64BitProcess) 'Run Windows PowerShell 5.1 x64 or PowerShell 7 x64 on Windows.'
Assert-True (-not [string]::IsNullOrWhiteSpace($ImageId)) 'ImageId must contain an operator label.'
$script:KitRoot = Get-SafePath $KitDirectory
Assert-True (Test-Path -LiteralPath $script:KitRoot -PathType Container) 'KitDirectory does not exist.'
$integrity = Test-Kit
$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + $ScenarioId + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
if (-not $OutputDirectory) { $OutputDirectory = Join-Path (Get-Location).ProviderPath ('m0-manual-' + $runId) }
$script:RunDirectory = Get-SafePath $OutputDirectory
Assert-True (-not ($script:RunDirectory -ieq $script:KitRoot -or $script:RunDirectory.StartsWith($script:KitRoot + '\', [StringComparison]::OrdinalIgnoreCase))) 'OutputDirectory must be outside the sealed kit.'
Assert-True (-not (Test-Path -LiteralPath $script:RunDirectory)) 'OutputDirectory already exists; use a new directory. Evidence is never overwritten.'
$tempDirectory = Join-Path $script:RunDirectory 'temp'
$localDirectory = Join-Path $script:RunDirectory 'local-app-data'
$sourceRuntime = Resolve-KitFile $script:Kit.paths.runtime
$runtimeSnapshot = Join-Path $script:RunDirectory 'tools\avelin-app.exe'
$backendPath = Join-Path $script:RunDirectory 'deliberately-missing-backend.exe'
$manifestPath = Join-Path $script:RunDirectory 'deliberately-missing-manifest.json'
$arguments = @(if ($Mode -eq 'Demo') { '--ui-demo' } else { '--gui', '--backend', $backendPath, '--manifest', $manifestPath })
if ($English) { $arguments += '--english' }
$plan = [ordered]@{
    schema_version = 1; status = 'PLAN_ONLY_NOT_EXECUTED'; run_id = $runId
    mode = $Mode; scenario_id = $ScenarioId; image_id = $ImageId; english = [bool]$English
    kit_directory = $script:KitRoot; integrity = $integrity; run_directory = $script:RunDirectory
    source_runtime = $sourceRuntime; runtime_snapshot = $runtimeSnapshot
    runtime_sha256 = $script:Indexed['tools/avelin-app.exe'].sha256
    launcher_path = $PSCommandPath; launcher_sha256 = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash.ToLowerInvariant()
    arguments = @($arguments); child_environment = [ordered]@{ TEMP = $tempDirectory; TMP = $tempDirectory; LOCALAPPDATA = $localDirectory }
    observation_limit_seconds = $ObservationTimeoutSeconds; capture_limit_per_stream_bytes = 4194304
    launch_method = 'ProcessStartInfo; UseShellExecute=false, WindowStyle=Normal, CreateNoWindow=true suppresses an extra console; the application creates its visible native window.'
    environment_scope = 'Only child TEMP/TMP/LOCALAPPDATA are replaced. Other environment is inherited; no security or display setting is changed. ImageId is an unverified operator declaration.'
    on_timeout = 'Record uncertainty and retain PID/creation time/output. Never kill, retry, close the window or remove evidence. If the observer host exits, redirected pipes can close and later output is unconfirmed.'
    acceptance = 'NOT_RECORDED'; scope = 'Demo or native preflight failure dialog only; no real installation, GPU failure injection, UI automation or automatic manual acceptance.'
}
if ($PlanOnly) { $plan | ConvertTo-Json -Depth 15; return }

$null = New-Item -ItemType Directory -Path $script:RunDirectory
foreach ($directory in @((Join-Path $script:RunDirectory 'tools'), $tempDirectory, $localDirectory, (Join-Path $script:RunDirectory 'evidence'))) {
    $null = New-Item -ItemType Directory -Path $directory
}
$runtime = Copy-PinnedFile $sourceRuntime 'tools\avelin-app.exe' $plan.runtime_sha256
$harness = Copy-PinnedFile $PSCommandPath 'tools\start-m0-manual-acceptance.ps1' $plan.launcher_sha256
$null = Copy-PinnedFile (Resolve-KitFile 'checksums.json') 'kit-checksums.json' $integrity.checksums_sha256
$null = Copy-PinnedFile (Resolve-KitFile 'kit.json') 'kit.json' $script:Indexed['kit.json'].sha256
Write-NewJson 'plan.json' $plan
Write-NewJson 'kit-integrity.json' $integrity
Write-NewJson 'case-record.json' ([ordered]@{
    schema_version = 1; status = 'NOT_RECORDED'; scenario_id = $ScenarioId; mode = $Mode; image_id = $ImageId
    run_id = $runId; kit_id = $script:Kit.kit_id; runtime_sha256 = $runtime.sha256
    created_at_utc = [DateTime]::UtcNow.ToString('o'); recorded_by = ''; recorded_at_utc = $null
    environment_declaration = [ordered]@{
        kind = 'Unspecified'; image_id = $ImageId; independently_verified = $false
        windows_edition_build = ''; account_privilege = ''; session_kind = ''; gpu_adapter = ''
        display_resolution = ''; display_scale = ''; monitor_layout = ''; input_method = ''
        protection_and_uac_state = ''; environment_record = ''; notes = ''
    }
    observations = @(); evidence = @(); expected = ''; actual = ''; limitations = @(); issue_references = @()
    process_record = 'process.json'; launch_record = 'launch.json'; stdout = 'stdout.log'; stderr = 'stderr.log'
    allowed_manual_statuses = @('NOT_RECORDED', 'PASSED', 'FAILED', 'BLOCKED', 'NOT_APPLICABLE')
    instruction = 'An operator must record the actual steps, expected/observed behavior, evidence paths and limitations. Exiting with any code, a log marker or PlanOnly never sets a manual PASSED. Do not infer clean Windows, IME, screen-reader, DPI, RDP or GPU-failure acceptance from unrelated observations.'
})

# Asynchronous raw-byte pumps are independent of the PowerShell runspace. Each
# stream is capped on disk and still drained after truncation to avoid blocking
# the child. Only an observed zero-byte read establishes EOF.
if (-not ('AvelinM0ManualAcceptanceV1.Capture' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.Threading;
using System.Threading.Tasks;
namespace AvelinM0ManualAcceptanceV1 {
    public sealed class Capture {
        readonly Task stdout, stderr;
        long outputBytes, errorBytes;
        volatile bool outputEof, errorEof, limitExceeded;
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
            if (isOutput) outputEof = true; else errorEof = true;
        }
        public bool Complete { get { return stdout.IsCompleted && stderr.IsCompleted; } }
        public bool Faulted { get { return stdout.IsFaulted || stderr.IsFaulted; } }
        public bool OutputEof { get { return outputEof; } }
        public bool ErrorEof { get { return errorEof; } }
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
$startInfo = New-Object Diagnostics.ProcessStartInfo
$startInfo.FileName = $runtime.snapshot_path
$startInfo.Arguments = (@($arguments | ForEach-Object { Quote-Argument $_ }) -join ' ')
$startInfo.WorkingDirectory = $script:RunDirectory
$startInfo.UseShellExecute = $false
$startInfo.WindowStyle = [Diagnostics.ProcessWindowStyle]::Normal
$startInfo.CreateNoWindow = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
$startInfo.StandardOutputEncoding = $script:Utf8
$startInfo.StandardErrorEncoding = $script:Utf8
foreach ($entry in $plan.child_environment.GetEnumerator()) { $startInfo.EnvironmentVariables[$entry.Key] = $entry.Value }
$process = New-Object Diagnostics.Process
$process.StartInfo = $startInfo
$capture = $null
$clock = [Diagnostics.Stopwatch]::StartNew()
$record = [ordered]@{
    schema_version = 1; status = 'NOT_STARTED'; observation_status = 'STARTING'; acceptance = 'NOT_RECORDED'
    scenario_id = $ScenarioId; mode = $Mode; image_id = $ImageId; english = [bool]$English; run_directory = $script:RunDirectory
    runtime = $runtime; harness = $harness; arguments = @($arguments); child_environment = $plan.child_environment
    executable = $runtime.snapshot_path; executable_sha256 = $runtime.sha256; kit_integrity = $integrity
    process_id = $null; pid = $null; process_creation_time_utc = $null; process_creation_time_filetime_utc = $null
    started_at_utc = [DateTime]::UtcNow.ToString('o'); exited_at_utc = $null; observation_ended_at_utc = $null
    exit_code = $null; observation_wall_ms = $null; observation_limit_seconds = $ObservationTimeoutSeconds
    stdout_path = $stdoutPath; stderr_path = $stderrPath; stdout_bytes_observed = $null; stderr_bytes_observed = $null
    stdout_eof = $false; stderr_eof = $false; logs_complete_at_eof = $false; output_utf8_valid = $null; capture_limit_exceeded = $false
    timed_out = $false; process_was_killed = $false; error = $null; runtime_summary_markers = @()
    temporary_directory = $tempDirectory; local_app_data_directory = $localDirectory
}
try {
    if ($Mode -eq 'LaunchError') { Assert-True (-not (Test-Path -LiteralPath $backendPath) -and -not (Test-Path -LiteralPath $manifestPath)) 'LaunchError requires both input paths to be absent.' }
    Assert-True ((Get-FileHash -LiteralPath $runtime.snapshot_path -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $runtime.sha256) 'Pinned runtime changed before launch.'
    Assert-True ($process.Start()) 'Process.Start returned false.'
    $null = $process.Handle
    $record.pid = $process.Id
    $record.process_id = $process.Id
    $record.process_creation_time_utc = $process.StartTime.ToUniversalTime().ToString('o')
    $record.process_creation_time_filetime_utc = $process.StartTime.ToUniversalTime().ToFileTimeUtc()
    $record.status = 'RUNNING'
    $record.observation_status = 'OBSERVING_INTERACTIVE_SESSION'
    $capture = New-Object AvelinM0ManualAcceptanceV1.Capture($process, $stdoutPath, $stderrPath)
    Write-NewJson 'launch.json' $record
    Write-Output ('AVELIN_MANUAL_ACCEPTANCE_STARTED ' + (ConvertTo-Json -Compress -InputObject ([ordered]@{ run_directory = $script:RunDirectory; process_id = $record.process_id; scenario_id = $ScenarioId; mode = $Mode; runtime_sha256 = $runtime.sha256 })))
    while ($true) {
        $process.Refresh()
        if ($process.HasExited) {
            $record.status = 'EXITED'; $record.exit_code = $process.ExitCode
            $record.exited_at_utc = $process.ExitTime.ToUniversalTime().ToString('o')
        }
        if ($capture.Faulted) { throw ('Output capture failed: ' + $capture.Error) }
        if ($process.HasExited -and $capture.Complete) {
            $record.logs_complete_at_eof = $capture.OutputEof -and $capture.ErrorEof -and -not $capture.LimitExceeded
            $record.observation_status = if ($capture.LimitExceeded) { 'EXIT_OBSERVED_CAPTURE_TRUNCATED' } else { 'EXIT_AND_EOF_OBSERVED' }
            break
        }
        if ($clock.Elapsed.TotalSeconds -ge $ObservationTimeoutSeconds) {
            $record.observation_status = 'OBSERVATION_TIMEOUT'; $record.timed_out = $true
            $record.error = 'Observation limit reached. The application was not killed, closed or retried. Output after this observation is unconfirmed.'
            break
        }
        Start-Sleep -Milliseconds 100
    }
}
catch {
    $record.observation_status = 'HARNESS_OR_CAPTURE_FAILED'; $record.error = $_.Exception.Message
    if ($null -ne $record.pid) {
        try {
            $process.Refresh()
            if ($process.HasExited) { $record.status = 'EXITED'; $record.exit_code = $process.ExitCode; $record.exited_at_utc = $process.ExitTime.ToUniversalTime().ToString('o') }
            else { $record.status = 'RUNNING' }
        } catch { $record.status = 'UNCONFIRMED' }
    }
}
finally {
    $clock.Stop()
    $record.observation_wall_ms = $clock.ElapsedMilliseconds
    $record.observation_ended_at_utc = [DateTime]::UtcNow.ToString('o')
    if ($null -ne $capture) {
        $record.stdout_bytes_observed = $capture.OutputBytes; $record.stderr_bytes_observed = $capture.ErrorBytes
        $record.stdout_eof = $capture.OutputEof; $record.stderr_eof = $capture.ErrorEof
        $record.capture_limit_exceeded = $capture.LimitExceeded
    }
    if ($record.logs_complete_at_eof) {
        try {
            $null = $script:Utf8.GetString([IO.File]::ReadAllBytes($stdoutPath))
            $stderrText = $script:Utf8.GetString([IO.File]::ReadAllBytes($stderrPath))
            $record.output_utf8_valid = $true
            foreach ($match in [regex]::Matches($stderrText, '(?m)^AVELIN_RUNTIME_LOG_SUMMARY (.+)\r?$')) {
                try { $record.runtime_summary_markers += ($match.Groups[1].Value | ConvertFrom-Json) }
                catch { $record.runtime_summary_markers += [ordered]@{ parse_error = $_.Exception.Message; raw = $match.Groups[1].Value } }
            }
        } catch { $record.output_utf8_valid = $false; $record.observation_status = 'EXIT_OBSERVED_INVALID_UTF8'; $record.error = $_.Exception.Message }
    }
    if (-not (Test-Path -LiteralPath (Join-Path $script:RunDirectory 'launch.json'))) { Write-NewJson 'launch.json' $record }
    Write-NewJson 'process.json' $record
    # Keep the process and pumps alive for the full observation. On uncertainty
    # do not explicitly close pipes; host exit may still close inherited handles.
    if ($record.status -eq 'EXITED' -and $null -ne $capture -and $capture.Complete) { $process.Dispose() }
}
Write-Output ('AVELIN_MANUAL_ACCEPTANCE_OBSERVATION ' + (ConvertTo-Json -Compress -InputObject ([ordered]@{ run_directory = $script:RunDirectory; status = $record.status; observation_status = $record.observation_status; process_id = $record.process_id; exit_code = $record.exit_code; logs_complete_at_eof = $record.logs_complete_at_eof; acceptance = 'NOT_RECORDED' })))
if ($record.observation_status -notin @('EXIT_AND_EOF_OBSERVED')) { throw "Manual observer did not complete: $($record.observation_status). Evidence is retained in $script:RunDirectory" }
