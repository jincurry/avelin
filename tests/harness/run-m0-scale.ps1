[CmdletBinding()]
param(
    [switch]$PlanOnly,
    [string]$ProjectRoot = (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)),
    [string]$CliPath,
    [string]$AppPath,
    [string]$BridgePath,
    [ValidateRange(1, 100000)][int]$Files = 1000,
    [ValidateRange(1, 1024)][int]$LargeFileMiB = 128,
    [ValidateRange(20, 2000)][int]$SampleIntervalMs = 100,
    [ValidateRange(30, 1200)][int]$TimeoutSeconds = 900,
    [switch]$CancelDuringLargeFile,
    [ValidateRange(0, 120000)][Nullable[int]]$CancelAfterMs
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$cancelEnabled = $CancelDuringLargeFile -or $PSBoundParameters.ContainsKey('CancelAfterMs')
if ($CancelDuringLargeFile -and $PSBoundParameters.ContainsKey('CancelAfterMs')) { throw 'Choose -CancelDuringLargeFile or -CancelAfterMs, not both.' }
if ($PlanOnly) {
    [ordered]@{
        status = 'PLAN_ONLY_NOT_EXECUTED'; small_files = $Files; large_file_mib = $LargeFileMiB
        total_payload_files = $Files + 2; cancellation_enabled = $cancelEnabled
        scope = 'Local smoke and performance sample, not a supported capacity or release claim.'
        generation = 'Fixed-seed System.Random streaming bytes, plus the independent Nimbus executable; no zero-filled large file.'
        isolation = 'Unique target/m0-scale/<run>, application ID, TEMP/TMP/LOCALAPPDATA and installation paths. No recursive deletion or force-kill.'
        disk_budget_bytes = 2GB; process_sample_target_interval_ms = $SampleIntervalMs
        cancellation = 'Optional real --cancel-after-ms signal. -CancelDuringLargeFile derives a delay from the normal run interval midpoint. Prove request between large-file extracting/running observations; otherwise mark NOT_OBSERVED. File execution itself has no cancellation checkpoint.'
    } | ConvertTo-Json
    exit 0
}

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Full-Path([string]$Path) {
    return [IO.Path]::GetFullPath($Path).TrimEnd([char]'\', [char]'/')
}
function Safe-Path([string]$Path, [string]$Root) {
    $absolute = Full-Path $Path
    $base = Full-Path $Root
    Assert-True ($absolute.StartsWith($base + '\', [StringComparison]::OrdinalIgnoreCase)) "Path escaped the task directory: $absolute"
    $cursor = $absolute
    while ($cursor) {
        if ([IO.File]::Exists($cursor) -or [IO.Directory]::Exists($cursor)) {
            Assert-True (([IO.File]::GetAttributes($cursor) -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse point rejected: $cursor"
        }
        $parent = [IO.Directory]::GetParent($cursor)
        $cursor = if ($parent) { $parent.FullName } else { $null }
    }
    return $absolute
}
function Save-Json([string]$Name, [object]$Value) {
    $path = Safe-Path (Join-Path $script:RunRoot $Name) $script:RunRoot
    [IO.File]::WriteAllText($path, ($Value | ConvertTo-Json -Depth 40), [Text.UTF8Encoding]::new($false))
}
function Hash([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}
function Snapshot([string]$Directory) {
    if (-not (Test-Path -LiteralPath $Directory)) { return }
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($Directory)
    while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            $path = Safe-Path $item.FullName $Directory
            if ($item.PSIsContainer) { $pending.Push($path) }
            else { [pscustomobject]@{ path = $path.Substring($Directory.Length + 1); size = $item.Length; sha256 = Hash $path } }
        }
    }
}
function Assert-Files([string]$Directory, [object[]]$Expected, [switch]$Exact) {
    $actual = @(Snapshot $Directory)
    $byPath = @{}
    foreach ($file in $actual) { $byPath[$file.path] = $file }
    foreach ($file in $Expected) {
        Assert-True ($byPath.ContainsKey($file.path)) "Missing installed file: $($file.path)"
        Assert-True ($byPath[$file.path].sha256 -ceq $file.sha256 -and $byPath[$file.path].size -eq $file.size) "Independent SHA-256/size mismatch: $($file.path)"
    }
    if ($Exact) { Assert-True ($actual.Count -eq $Expected.Count) "Unexpected retained files in $Directory" }
}
function Registry-State {
    foreach ($view in @([Microsoft.Win32.RegistryView]::Registry32, [Microsoft.Win32.RegistryView]::Registry64)) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $view)
        $key = $null
        try {
            $key = $base.OpenSubKey($script:RegistryKey, $false)
            [pscustomobject]@{ view = "$view"; exists = ($null -ne $key); location = $(if ($key) { $key.GetValue('InstallLocation') } else { $null }) }
        }
        finally { if ($key) { $key.Dispose() }; $base.Dispose() }
    }
}
function Assert-NoRegistration {
    Assert-True (@(Registry-State | Where-Object exists).Count -eq 0) 'Task-specific uninstall registration remains.'
}
function Run-Process([string]$Executable, [string[]]$Arguments, [string]$Name) {
    $executedHash = Hash $Executable
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable
    $info.WorkingDirectory = $script:ProjectDirectory
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($arg in $Arguments) { $info.ArgumentList.Add($arg) }
    $info.Environment['TEMP'] = $script:TempDirectory
    $info.Environment['TMP'] = $script:TempDirectory
    $info.Environment['LOCALAPPDATA'] = $script:LocalDirectory
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    $start = [DateTime]::UtcNow
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $null = $process.Start()
    $processId = $process.Id
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    # A PID is reusable. Keep each active PID bound to its first observed start
    # time, and retire it before discovering children if that identity exits.
    $known = @{ $processId = $process.StartTime.ToUniversalTime().Ticks }
    $observed = @{}
    $identityChanges = [Collections.Generic.List[object]]::new()
    $samples = [Collections.Generic.List[object]]::new()
    $intervals = [Collections.Generic.List[long]]::new()
    $lastSample = $null
    $nextNotice = 15
    $status = 'EXITED'
    Write-Host "[$Name] starting $([IO.Path]::GetFileName($Executable))"
    while ($true) {
        $now = $timer.ElapsedMilliseconds
        if ($null -ne $lastSample) { $intervals.Add($now - $lastSample) }
        $lastSample = $now
        $snapshotCutoffTicks = [DateTime]::UtcNow.Ticks
        $pairs = [AvelinM0Scale.ProcessTree]::Parents()
        foreach ($trackedId in @($known.Keys)) {
            $check = $null
            try {
                $check = [Diagnostics.Process]::GetProcessById($trackedId)
                if ($check.HasExited) { $known.Remove($trackedId); continue }
                $startTicks = $check.StartTime.ToUniversalTime().Ticks
                if ($startTicks -ne $known[$trackedId]) {
                    $identityChanges.Add([pscustomobject]@{ pid = $trackedId; expected_start_ticks = $known[$trackedId]; observed_start_ticks = $startTicks; elapsed_ms = $now })
                    $known.Remove($trackedId)
                }
            }
            catch [ArgumentException] { $known.Remove($trackedId) }
            catch [InvalidOperationException] { $known.Remove($trackedId) }
            finally { if ($check) { $check.Dispose() } }
        }
        do {
            $added = $false
            foreach ($pair in $pairs) {
                if ($known.ContainsKey([int]$pair[1]) -and -not $known.ContainsKey([int]$pair[0])) {
                    $candidate = $null
                    try {
                        $candidate = [Diagnostics.Process]::GetProcessById([int]$pair[0])
                        $candidateStart = $candidate.StartTime.ToUniversalTime().Ticks
                        if (-not $candidate.HasExited -and $candidateStart -ge $known[[int]$pair[1]] -and $candidateStart -le $snapshotCutoffTicks) {
                            $known[[int]$pair[0]] = $candidateStart
                            $added = $true
                        }
                    }
                    catch [ArgumentException] { }
                    catch [InvalidOperationException] { }
                    finally { if ($candidate) { $candidate.Dispose() } }
                }
            }
        } while ($added)
        [long]$workingSet = 0
        [long]$privateBytes = 0
        $live = [Collections.Generic.List[int]]::new()
        foreach ($trackedId in @($known.Keys)) {
            $member = $null
            try {
                $member = [Diagnostics.Process]::GetProcessById($trackedId)
                if ($member.HasExited -or $member.StartTime.ToUniversalTime().Ticks -ne $known[$trackedId]) { continue }
                $member.Refresh()
                $live.Add($trackedId)
                $workingSet += $member.WorkingSet64
                $privateBytes += $member.PrivateMemorySize64
                $identity = '{0}:{1}' -f $trackedId, $known[$trackedId]
                if (-not $observed.ContainsKey($identity)) {
                    $observed[$identity] = [ordered]@{
                        pid = $trackedId; name = $member.ProcessName; started_at_utc = $member.StartTime.ToUniversalTime().ToString('o')
                        process_identity = $identity
                        first_observed_ms = $now; last_observed_ms = $now; first_absent_ms = $null
                        sampled_peak_working_set_bytes = 0L; sampled_peak_private_bytes = 0L
                    }
                }
                $row = $observed[$identity]
                $row.last_observed_ms = $now
                $row.sampled_peak_working_set_bytes = [Math]::Max($row.sampled_peak_working_set_bytes, $member.WorkingSet64)
                $row.sampled_peak_private_bytes = [Math]::Max($row.sampled_peak_private_bytes, $member.PrivateMemorySize64)
            }
            catch [ArgumentException] { }
            catch [InvalidOperationException] { }
            finally { if ($member) { $member.Dispose() } }
        }
        foreach ($identity in @($observed.Keys)) {
            $row = $observed[$identity]
            if ((-not $live.Contains($row.pid) -or ('{0}:{1}' -f $row.pid, $known[$row.pid]) -ne $identity) -and $null -eq $row.first_absent_ms) { $row.first_absent_ms = $now }
        }
        $samples.Add([pscustomobject]@{ elapsed_ms = $now; working_set_bytes = $workingSet; private_bytes = $privateBytes; process_ids = $live.ToArray() })
        if ($process.HasExited -and $stdout.IsCompleted -and $stderr.IsCompleted) {
            if (@($live | Where-Object { $_ -ne $processId }).Count -gt 0) { $status = 'UNKNOWN_DESCENDANTS_STILL_RUNNING' }
            break
        }
        if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) { $status = 'TIMED_OUT_PROCESS_NOT_KILLED'; break }
        if ($timer.Elapsed.TotalSeconds -ge $nextNotice) {
            Write-Host "[$Name] running $([int]$timer.Elapsed.TotalSeconds)s; observed $($observed.Count) processes"
            $nextNotice += 15
        }
        Start-Sleep -Milliseconds $SampleIntervalMs
    }
    $timer.Stop()
    $result = [ordered]@{
        executable = $Executable; executable_sha256 = $executedHash; arguments = $Arguments; pid = $processId
        started_at_utc = $start.ToString('o'); duration_ms = $timer.ElapsedMilliseconds; status = $status
        exit_code = $(if ($process.HasExited) { $process.ExitCode } else { $null })
        sample_target_interval_ms = $SampleIntervalMs; sample_count = $samples.Count
        actual_max_interval_ms = $(if ($intervals.Count) { ($intervals | Measure-Object -Maximum).Maximum } else { $null })
        actual_mean_interval_ms = $(if ($intervals.Count) { ($intervals | Measure-Object -Average).Average } else { $null })
        sampled_peak_family_working_set_bytes = ($samples | Measure-Object working_set_bytes -Maximum).Maximum
        sampled_peak_family_private_bytes = ($samples | Measure-Object private_bytes -Maximum).Maximum
        sampling_scope = 'Launcher plus descendants discovered with Toolhelp32Snapshot, bound to PID/start-time identities. Only currently live known parent identities expand the family. Peaks are discrete, non-atomic observations; per-process readings need not represent one simultaneous family total. Short-lived descendants and between-sample spikes may be missed. Working sets may double-count shared pages. Sampling adds overhead.'
        observed_processes = @($observed.Values); rejected_pid_reuse = $identityChanges.ToArray()
        live_process_ids_at_end = $live.ToArray()
    }
    Save-Json "$Name.process.json" $result
    Save-Json "$Name.samples.json" $samples.ToArray()
    if ($status -ne 'EXITED') { $process.Dispose(); throw "$Name ended with $status; no process was killed. Stop and inspect retained evidence." }
    [IO.File]::WriteAllText((Join-Path $script:RunRoot "$Name.stdout.log"), $stdout.GetAwaiter().GetResult())
    [IO.File]::WriteAllText((Join-Path $script:RunRoot "$Name.stderr.log"), $stderr.GetAwaiter().GetResult())
    $process.Dispose()
    return [pscustomobject]$result
}
function Read-Report([string]$Name, [object]$Process, [string]$Expected) {
    $report = Get-Content -LiteralPath (Join-Path $script:RunRoot "$Name.report.json") -Raw | ConvertFrom-Json
    Assert-True ($report.result -ceq $Expected) "Expected $Expected, got $($report.result): $($report.message)"
    $expectedCode = if ($Expected -eq 'Cancelled') { 2 } else { 0 }
    Assert-True ($report.exit_code -eq $expectedCode -and $Process.exit_code -eq $expectedCode) 'Report and process exits disagree.'
    Assert-True (@($report.events).Count -gt 0) 'Report has no backend events.'
    return $report
}

Assert-True ($PSVersionTable.PSVersion.Major -ge 7 -and [Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'Run in Windows PowerShell 7.'
$script:ProjectDirectory = Full-Path $ProjectRoot
if (-not $CliPath) { $CliPath = Join-Path $script:ProjectDirectory 'target\debug\avelin-cli.exe' }
if (-not $AppPath) { $AppPath = Join-Path $script:ProjectDirectory 'target\debug\avelin-app.exe' }
if (-not $BridgePath) { $BridgePath = Join-Path $script:ProjectDirectory 'target\i686-pc-windows-msvc\debug\avelin_nsis_bridge.dll' }
$CliPath = Full-Path $CliPath
$AppPath = Full-Path $AppPath
$BridgePath = Full-Path $BridgePath
$entrySource = Join-Path $script:ProjectDirectory 'examples\nimbus\payload\nimbus-demo.exe'
foreach ($required in @($CliPath, $AppPath, $BridgePath, $entrySource)) { Assert-True (Test-Path -LiteralPath $required -PathType Leaf) "Required built artifact is missing: $required" }
$smallSizeExtras = 78L * [long][Math]::Floor($Files / 13) + [long](($Files % 13) * (($Files % 13) - 1) / 2)
$estimatedPayloadBytes = [long]$LargeFileMiB * 1MB + [long]$Files * 4096 + 31L * $smallSizeExtras + (Get-Item -LiteralPath $entrySource).Length
# Peak: source, staging payload, staging backend, sidecar backend, setup,
# embedded backend in TEMP, and installed payload. Allow 5% package expansion,
# sixteen runtime copies and 32 MiB for scripts, manifests, logs and sampling.
$estimatedDiskBytes = [long][Math]::Ceiling(7.35 * $estimatedPayloadBytes + 16L * (Get-Item -LiteralPath $AppPath).Length + 32MB)
$scaleRoot = Safe-Path (Join-Path $script:ProjectDirectory 'target\m0-scale') $script:ProjectDirectory
$existingScaleBytes = 0L
if (Test-Path -LiteralPath $scaleRoot) { $existingScaleBytes = [long](Get-ChildItem -LiteralPath $scaleRoot -Recurse -Force -File | Measure-Object Length -Sum).Sum }
Assert-True ($estimatedDiskBytes + $existingScaleBytes -lt 2GB) "Existing scale evidence ($existingScaleBytes bytes) plus conservative run peak ($estimatedDiskBytes bytes) exceeds the strict total 2 GiB budget. Choose a smaller fixture or explicitly archive generated copies."
$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
$script:RunRoot = Safe-Path (Join-Path $script:ProjectDirectory "target\m0-scale\$runId") $script:ProjectDirectory
Assert-True (-not (Test-Path -LiteralPath $script:RunRoot)) 'Refusing to reuse an existing run directory.'
$null = New-Item -ItemType Directory -Path $script:RunRoot
$harnessSnapshot = Safe-Path (Join-Path $script:RunRoot 'harness-source.ps1') $script:RunRoot
[IO.File]::Copy($PSCommandPath, $harnessSnapshot, $false)
$script:TempDirectory = Safe-Path (Join-Path $script:RunRoot 'temp') $script:RunRoot
$script:LocalDirectory = Safe-Path (Join-Path $script:RunRoot 'local-app-data') $script:RunRoot
$fixture = Safe-Path (Join-Path $script:RunRoot 'fixtures') $script:RunRoot
foreach ($directory in @($script:TempDirectory, $script:LocalDirectory, $fixture)) { $null = New-Item -ItemType Directory -Path $directory }
$toolDirectory = Safe-Path (Join-Path $script:RunRoot 'tools') $script:RunRoot
$null = New-Item -ItemType Directory -Path $toolDirectory
$toolSources = [ordered]@{ cli = $CliPath; app = $AppPath; bridge = $BridgePath }
$pinnedTools = @{}
foreach ($toolName in $toolSources.Keys) {
    $source = $toolSources[$toolName]
    $destination = Safe-Path (Join-Path $toolDirectory ([IO.Path]::GetFileName($source))) $script:RunRoot
    $sourceHash = Hash $source
    [IO.File]::Copy($source, $destination, $false)
    Assert-True ((Hash $destination) -ceq $sourceHash) "Tool changed while pinning the run input: $source"
    $pinnedTools[$toolName] = $destination
}
$CliPath = $pinnedTools.cli
$AppPath = $pinnedTools.app
$BridgePath = $pinnedTools.bridge
$appId = 'dev.avelin.m0.scale.' + [Guid]::NewGuid().ToString('N')
$script:RegistryKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $appId
$summary = [ordered]@{
    schema_version = 1; status = 'RUNNING'; active_case = 'GENERATE'; failure = $null; started_at_utc = [DateTime]::UtcNow.ToString('o')
    run_directory = $script:RunRoot; app_id = $appId; registry_key = $script:RegistryKey
    scope = 'One local unsigned development-machine smoke/performance sample; not a supported capacity or clean-machine/release claim.'
    small_file_count = $Files; large_file_mib = $LargeFileMiB; total_payload_file_count = $Files + 2
    random_seed = 19790427; random_algorithm = 'System.Random(seed).NextBytes; generated in 1 MiB blocks, runtime dependent'
    runtime = [Runtime.InteropServices.RuntimeInformation]::FrameworkDescription; generator_buffer_bytes = 1MB
    conservative_disk_estimate_bytes = $estimatedDiskBytes; disk_budget_bytes = 2GB
    existing_scale_evidence_bytes = $existingScaleBytes
    disk_estimate_note = 'Seven payload copies at peak with 5% size expansion, sixteen runtime copies and 32 MiB ancillary allowance, plus every retained scale run. No automatic deletion.'
    harness_sha256 = Hash $harnessSnapshot; harness_source_snapshot = $harnessSnapshot
    cli_sha256 = Hash $CliPath; app_sha256 = Hash $AppPath
    bridge_sha256 = Hash $BridgePath; tool_sources = $toolSources; pinned_tools = $pinnedTools
    cancellation = [ordered]@{ status = 'NOT_TESTED'; reason = $(if ($cancelEnabled) { 'Pending successful normal install/uninstall; no timed cancellation has been sent.' } else { 'No timed cancellation requested; file-count cancellation is not substituted.' }) }
    cases = @()
}
$previousEnv = @{ TEMP = $env:TEMP; TMP = $env:TMP; LOCALAPPDATA = $env:LOCALAPPDATA }
$env:TEMP = $script:TempDirectory
$env:TMP = $script:TempDirectory
$env:LOCALAPPDATA = $script:LocalDirectory
try {
    Save-Json 'summary.json' $summary
    Write-Host "M0 scale evidence: $script:RunRoot"
    Assert-NoRegistration
    if (-not ('AvelinM0Scale.ProcessTree' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
namespace AvelinM0Scale {
 public static class ProcessTree {
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct Entry {
   public uint size, usage, id; public UIntPtr heap; public uint module, threads, parent; public int priority; public uint flags;
   [MarshalAs(UnmanagedType.ByValTStr, SizeConst=260)] public string executable;
  }
  [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint pid);
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] static extern bool Process32FirstW(IntPtr snapshot, ref Entry entry);
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] static extern bool Process32NextW(IntPtr snapshot, ref Entry entry);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
  public static int[][] Parents() {
   var snapshot=CreateToolhelp32Snapshot(2,0);
   if(snapshot==new IntPtr(-1)) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
   var rows=new List<int[]>();
   try { var e=new Entry { size=(uint)Marshal.SizeOf<Entry>() };
    if(Process32FirstW(snapshot,ref e)) do { rows.Add(new[]{(int)e.id,(int)e.parent}); } while(Process32NextW(snapshot,ref e));
   } finally { CloseHandle(snapshot); }
   return rows.ToArray();
  }
 }
}
'@
    }
    $random = [Random]::new(19790427)
    $buffer = [byte[]]::new(1MB)
    $largeRelative = '000-large.bin'
    $largePath = Safe-Path (Join-Path $fixture $largeRelative) $fixture
    $stream = [IO.File]::Open($largePath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    $generationTimer = [Diagnostics.Stopwatch]::StartNew()
    try {
        for ($block = 0; $block -lt $LargeFileMiB; $block++) { $random.NextBytes($buffer); $stream.Write($buffer, 0, $buffer.Length) }
    }
    finally { $stream.Dispose() }
    for ($index = 0; $index -lt $Files; $index++) {
        $relative = 'small\group-{0:D3}\item-{1:D6}.bin' -f [int][Math]::Floor($index / 100), $index
        $path = Safe-Path (Join-Path $fixture $relative) $fixture
        $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($path))
        $bytes = [byte[]]::new(4096 + (($index % 13) * 31))
        $random.NextBytes($bytes)
        [IO.File]::WriteAllBytes($path, $bytes)
    }
    Copy-Item -LiteralPath $entrySource -Destination (Join-Path $fixture 'entry.exe')
    $summary['fixture_generation_ms'] = $generationTimer.ElapsedMilliseconds
    $expected = @(Snapshot $fixture | Sort-Object path)
    Assert-True ($expected.Count -eq $Files + 2) 'Generated fixture count is incorrect.'
    Save-Json 'fixture-snapshot.json' $expected
    $config = @"
schema_version = 1
[app]
id = "$appId"
name = "Avelin M0 Scale"
version = "0.1.0"
entry = "entry.exe"
[payload]
directory = "fixtures"
[install]
directory_name = "AvelinM0Scale"
start_menu_shortcut = false
"@
    $configPath = Join-Path $script:RunRoot 'avelin.toml'
    [IO.File]::WriteAllText($configPath, $config, [Text.UTF8Encoding]::new($false))
    $setup = Join-Path $script:RunRoot 'ScaleSetup.exe'
    $backend = Join-Path $script:RunRoot 'ScaleSetup.backend.exe'
    $manifestPath = Join-Path $script:RunRoot 'ScaleSetup.manifest.json'
    $summary.active_case = 'BUILD'
    Save-Json 'summary.json' $summary
    $build = Run-Process $CliPath @('build', '--config', $configPath, '--output', $setup, '--runtime', $AppPath, '--bridge', $BridgePath) 'build'
    Assert-True ($build.exit_code -eq 0) 'Builder failed; inspect build logs. M0 limits are retained.'
    $metadata = Get-Content -LiteralPath (Join-Path $script:RunRoot 'build.stdout.log') | Where-Object { $_.StartsWith('{') } | Select-Object -Last 1 | ConvertFrom-Json
    $stage = Safe-Path $metadata.stage $script:RunRoot
    $nsisScript = Safe-Path (Join-Path $stage 'installer.nsi') $script:RunRoot
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    Assert-True ($manifest.app_id -ceq $appId -and $manifest.start_menu_shortcut -eq $false) 'Built manifest identity/shortcut settings differ; refusing to install.'
    Assert-True (@($manifest.files).Count -eq $expected.Count) 'Manifest count differs from independent source inventory.'
    $summary['build'] = [ordered]@{
        process = $build; build_wall_ms = $build.duration_ms; nsis_script_bytes = (Get-Item -LiteralPath $nsisScript).Length
        nsis_script_lines = ([IO.File]::ReadLines($nsisScript) | Measure-Object).Count; package_bytes = (Get-Item -LiteralPath $setup).Length
        backend_bytes = (Get-Item -LiteralPath $backend).Length
        nsis_compiler_observations = @($build.observed_processes | Where-Object name -eq 'makensis')
        timing_note = 'Build wall time includes inventory, hashing, staging, NSIS compilation and package assembly. For makensis, last_observed_ms <= exit <= first_absent_ms; short-lived processes may be missed. These are sampled compiler lifetime bounds, not exact compiler-only timings.'
    }
    if ($metadata.PSObject.Properties.Name -contains 'makensis_wall_ms') {
        $summary.build['makensis_wall_ms'] = $metadata.makensis_wall_ms
        $summary.build['makensis_timing_note'] = 'Builder stopwatch around makensis spawn and wait; includes process launch overhead.'
    }
    Save-Json 'summary.json' $summary
    $installDirectory = Safe-Path (Join-Path $script:RunRoot 'install-中文 space') $script:RunRoot
    Assert-True (-not (Test-Path -LiteralPath $installDirectory)) 'Installation path already exists.'
    $summary.active_case = 'INSTALL'
    Save-Json 'summary.json' $summary
    $install = Run-Process $setup @('--install-dir', $installDirectory, '--report', (Join-Path $script:RunRoot 'install.report.json'), '--operation', 'install') 'install'
    $installReport = Read-Report 'install' $install 'Succeeded'
    Assert-Files $installDirectory $expected
    $receipt = Get-Content -LiteralPath (Join-Path $installDirectory '.avelin-receipt.json') -Raw | ConvertFrom-Json
    Assert-True ($receipt.app_id -ceq $appId -and (Full-Path $receipt.install_dir) -ceq $installDirectory) 'Committed receipt identity/target differs.'
    Assert-True (@($receipt.files).Count -eq $expected.Count) 'Receipt file count differs from independent source inventory.'
    $receiptFiles = @{}
    foreach ($file in $receipt.files) { Assert-True (-not $receiptFiles.ContainsKey($file.path)) 'Receipt contains duplicate paths.'; $receiptFiles[$file.path] = $file }
    foreach ($file in $expected) {
        Assert-True ($receiptFiles.ContainsKey($file.path)) "Receipt omits $($file.path)"
        Assert-True ($receiptFiles[$file.path].sha256 -ceq $file.sha256 -and $receiptFiles[$file.path].size -eq $file.size) "Receipt metadata differs for $($file.path)"
    }
    $registration = @(Registry-State | Where-Object exists)
    Assert-True ($registration.Count -gt 0) 'Successful install has no test-specific uninstall registration.'
    foreach ($state in $registration) { Assert-True ((Full-Path $state.location) -ceq $installDirectory) 'Uninstall registration points outside the unique target.' }
    $summary.cases += [ordered]@{ id = 'INSTALL'; status = 'PASSED'; process = $install; event_count = @($installReport.events).Count; coordinator_elapsed_ms = $installReport.elapsed_ms; independent_verified_files = $expected.Count }
    $sentinelPath = Safe-Path (Join-Path $installDirectory 'user-kept.txt') $script:RunRoot
    [IO.File]::WriteAllText($sentinelPath, "Independent user data $runId")
    $sentinel = @([pscustomobject]@{ path = 'user-kept.txt'; size = (Get-Item -LiteralPath $sentinelPath).Length; sha256 = Hash $sentinelPath })
    $summary.active_case = 'UNINSTALL'
    Save-Json 'summary.json' $summary
    $uninstall = Run-Process $AppPath @('--backend', (Join-Path $installDirectory 'uninstall.exe'), '--manifest', $manifestPath, '--install-dir', $installDirectory, '--report', (Join-Path $script:RunRoot 'uninstall.report.json'), '--operation', 'uninstall') 'uninstall'
    $uninstallReport = Read-Report 'uninstall' $uninstall 'Succeeded'
    Assert-Files $installDirectory $sentinel -Exact
    Assert-NoRegistration
    $summary.cases += [ordered]@{ id = 'UNINSTALL'; status = 'PASSED'; process = $uninstall; event_count = @($uninstallReport.events).Count; coordinator_elapsed_ms = $uninstallReport.elapsed_ms; unknown_user_file_preserved = $true }
    Save-Json 'summary.json' $summary
    if ($cancelEnabled) {
        $summary.active_case = 'TIMED_CANCEL'
        Save-Json 'summary.json' $summary
        $largeIndex = -1
        for ($i = 0; $i -lt $manifest.files.Count; $i++) { if ($manifest.files[$i].path -ceq $largeRelative) { $largeIndex = $i; break } }
        Assert-True ($largeIndex -ge 0) 'Large file is absent from the built manifest.'
        $cancelDelay = $CancelAfterMs
        if ($CancelDuringLargeFile) {
            $normalBegin = @($installReport.events | Where-Object { $_.kind -eq 'progress' -and $_.phase -eq 'extracting' -and $_.completed -eq $largeIndex })
            $normalEnd = @($installReport.events | Where-Object { $_.kind -eq 'progress' -and $_.phase -eq 'running' -and $_.completed -eq $largeIndex + 1 })
            Assert-True ($normalBegin.Count -gt 0 -and $normalEnd.Count -gt 0) 'Normal report lacks large-file phase observations. Rebuild the instrumented runtime/bridge/CLI before automatic timing.'
            $cancelDelay = [int][Math]::Floor(($normalBegin[0].coordinator_received_elapsed_ms + $normalEnd[0].coordinator_received_elapsed_ms) / 2)
        }
        $cancelDirectory = Safe-Path (Join-Path $script:RunRoot 'cancel-中文 space') $script:RunRoot
        $cancel = Run-Process $AppPath @('--backend', $backend, '--manifest', $manifestPath, '--install-dir', $cancelDirectory, '--report', (Join-Path $script:RunRoot 'cancel.report.json'), '--operation', 'install', '--cancel-after-ms', "$cancelDelay") 'cancel'
        $cancelReport = Read-Report 'cancel' $cancel 'Cancelled'
        Assert-Files $cancelDirectory @() -Exact
        Assert-Files $installDirectory $sentinel -Exact
        Assert-NoRegistration
        $cancelTimes = $cancelReport.cancellation
        Assert-True ($null -ne $cancelTimes.requested_elapsed_ms -and $null -ne $cancelTimes.acknowledged_elapsed_ms -and $null -ne $cancelTimes.stopped_elapsed_ms -and $null -ne $cancelTimes.cleanup_completed_elapsed_ms) 'Cancellation timing evidence is incomplete.'
        Assert-True ($cancelTimes.requested_elapsed_ms -le $cancelTimes.acknowledged_elapsed_ms -and $cancelTimes.acknowledged_elapsed_ms -le $cancelTimes.stopped_elapsed_ms -and $cancelTimes.stopped_elapsed_ms -le $cancelTimes.cleanup_completed_elapsed_ms) 'Cancellation observations are not ordered request/acknowledgement/checkpoint/cleanup.'
        $begin = @($cancelReport.events | Where-Object { $_.kind -eq 'progress' -and $_.phase -eq 'extracting' -and $_.completed -eq $largeIndex })
        $end = @($cancelReport.events | Where-Object { $_.kind -eq 'progress' -and $_.phase -eq 'running' -and $_.completed -eq $largeIndex + 1 })
        $duringLarge = $begin.Count -gt 0 -and $end.Count -gt 0 -and $begin[0].coordinator_received_elapsed_ms -lt $cancelTimes.requested_elapsed_ms -and $cancelTimes.requested_elapsed_ms -lt $end[0].coordinator_received_elapsed_ms
        $summary.cancellation = [ordered]@{
            status = $(if ($duringLarge) { 'OBSERVED_REQUEST_DURING_LARGE_FILE_INTERVAL' } else { 'NOT_OBSERVED' })
            requested_delay_ms = $cancelDelay; delay_from_normal_interval_midpoint = [bool]$CancelDuringLargeFile; observed_times = $cancelTimes; large_file_path = $largeRelative
            large_file_begin = $begin; large_file_end = $end
            request_to_acknowledgement_ms = $cancelTimes.acknowledged_elapsed_ms - $cancelTimes.requested_elapsed_ms
            request_to_checkpoint_stop_ms = $cancelTimes.stopped_elapsed_ms - $cancelTimes.requested_elapsed_ms
            request_to_cleanup_completed_ms = $cancelTimes.cleanup_completed_elapsed_ms - $cancelTimes.requested_elapsed_ms
            note = 'All timing is coordinator observation, not disk I/O instrumentation. The real signal can be acknowledged during NSIS File; stopping and cleanup occur at the next checkpoint. No file-count barrier or step delay was used. If the request misses the observed large-file interval, mid-file cancellation remains unmeasured.'
        }
        $summary.cases += [ordered]@{ id = 'TIMED_CANCEL'; status = 'PASSED'; process = $cancel; event_count = @($cancelReport.events).Count; no_managed_files_or_receipt_remain = $true }
    }
    $summary.status = 'PASSED'
    $summary.active_case = $null
}
catch {
    $summary.status = 'FAILED'
    $summary.failure = $_.Exception.Message
    if ($cancelEnabled -and $summary.cancellation.status -eq 'NOT_TESTED') { $summary.cancellation.reason = 'An earlier case failed; no timed cancellation was sent. Large-file cancellation remains unmeasured.' }
    $summary['registry_at_failure'] = @(Registry-State)
    throw
}
finally {
    foreach ($name in $previousEnv.Keys) { [Environment]::SetEnvironmentVariable($name, $previousEnv[$name], 'Process') }
    $summary['finished_at_utc'] = [DateTime]::UtcNow.ToString('o')
    $summary['retained_bytes'] = (Get-ChildItem -LiteralPath $script:RunRoot -Recurse -Force -File | Measure-Object Length -Sum).Sum
    $summary['all_scale_runs_retained_bytes'] = (Get-ChildItem -LiteralPath $scaleRoot -Recurse -Force -File | Measure-Object Length -Sum).Sum
    if ($summary.all_scale_runs_retained_bytes -ge 2GB) { $summary.status = 'FAILED'; $summary.failure = 'Retained scale artifacts exceeded the strict total 2 GiB budget.' }
    Save-Json 'summary.json' $summary
    Write-Host "Evidence retained: $(Join-Path $script:RunRoot 'summary.json')"
}
Assert-True ($summary.status -eq 'PASSED') $summary.failure
