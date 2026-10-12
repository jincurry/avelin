# Reusable Windows / PowerShell 7 process-family sampler. Dot-source this file.
# Every output prefix must be new. Timeouts stop observation, never the process.
function Initialize-M0PerformanceTypes {
    if ('AvelinM0PerformanceV2.ProcessTree' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading.Tasks;
using Microsoft.Win32.SafeHandles;
namespace AvelinM0PerformanceV2 {
 // Process.HasExited can observe an exit code before the retained kernel
 // process handle is signaled. Only the signaled handle plus validated native
 // times constitute a final observation; 259 is also a valid final exit code.
 public sealed class ProcessSnapshot {
  public bool TerminalVerified { get; private set; }
  public long CreationTicks { get; private set; }
  public long? ExitTicks { get; private set; }
  public string ExitedAtUtc { get; private set; }
  public int? ExitCode { get; private set; }
  public double CpuTimeMs { get; private set; }
  internal ProcessSnapshot(bool terminal, long creation, long? exit, int? code, double cpu) {
   TerminalVerified=terminal; CreationTicks=creation; ExitTicks=exit; ExitCode=code; CpuTimeMs=cpu;
   ExitedAtUtc=exit.HasValue ? new DateTime(exit.Value,DateTimeKind.Utc).ToString("o") : null;
  }
 }
 public static class ProcessObservation {
  const uint WAIT_OBJECT_0=0, WAIT_TIMEOUT=258, WAIT_FAILED=0xffffffff;
  [StructLayout(LayoutKind.Sequential)] struct FileTime {
   public uint low, high;
   public ulong Value { get { return ((ulong)high << 32) | low; } }
  }
  [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForSingleObject(SafeProcessHandle process, uint milliseconds);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetProcessTimes(SafeProcessHandle process, out FileTime create, out FileTime exit, out FileTime kernel, out FileTime user);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetExitCodeProcess(SafeProcessHandle process, out uint code);
  static long Timestamp(ulong value, string name) {
   if(value==0 || value>(ulong)DateTime.SpecifyKind(DateTime.MaxValue,DateTimeKind.Utc).ToFileTimeUtc()) throw new InvalidOperationException("Invalid native "+name+" FILETIME: "+value);
   return DateTime.FromFileTimeUtc((long)value).Ticks;
  }
  // Pure validation seam also exercises invalid/incomplete native snapshots.
  public static ProcessSnapshot Validate(uint wait, ulong create, ulong exit, ulong kernel, ulong user, uint code, long expectedCreationTicks) {
   if(wait!=WAIT_OBJECT_0 && wait!=WAIT_TIMEOUT) throw new InvalidOperationException("Unexpected process wait result: "+wait);
   long creation=Timestamp(create,"creation");
   if(creation!=expectedCreationTicks) throw new InvalidOperationException("Retained process creation identity changed.");
   if(kernel>long.MaxValue || user>long.MaxValue) throw new InvalidOperationException("Invalid native process CPU times.");
   double cpu=kernel/10000.0+user/10000.0;
   if(wait==WAIT_TIMEOUT) return new ProcessSnapshot(false,creation,null,null,cpu);
   long exited=Timestamp(exit,"exit");
   if(exited<creation) throw new InvalidOperationException("Native process exit precedes creation.");
   return new ProcessSnapshot(true,creation,exited,unchecked((int)code),cpu);
  }
  public static ProcessSnapshot Read(SafeProcessHandle process, long expectedCreationTicks) {
   uint wait=WaitForSingleObject(process,0);
   if(wait==WAIT_FAILED) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(),"WaitForSingleObject(process) failed.");
   if(wait!=WAIT_OBJECT_0 && wait!=WAIT_TIMEOUT) throw new InvalidOperationException("Unexpected process wait result: "+wait);
   FileTime create,exit,kernel,user;
   if(!GetProcessTimes(process,out create,out exit,out kernel,out user)) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(),"GetProcessTimes failed.");
   uint code=0;
   if(wait==WAIT_OBJECT_0 && !GetExitCodeProcess(process,out code)) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(),"GetExitCodeProcess failed for signaled process.");
   return Validate(wait,create.Value,exit.Value,kernel.Value,user.Value,code,expectedCreationTicks);
  }
 }
 public static class ProcessTree {
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct Entry {
   public uint size, usage, id; public UIntPtr heap; public uint module, threads, parent; public int priority; public uint flags;
   [MarshalAs(UnmanagedType.ByValTStr, SizeConst=260)] public string executable;
  }
  [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint pid);
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool Process32FirstW(IntPtr snapshot, ref Entry entry);
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool Process32NextW(IntPtr snapshot, ref Entry entry);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
  public static int[][] Parents() {
   var snapshot=CreateToolhelp32Snapshot(2,0);
   if(snapshot==new IntPtr(-1)) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
   var rows=new List<int[]>();
   try { var e=new Entry { size=(uint)Marshal.SizeOf<Entry>() };
    if(!Process32FirstW(snapshot,ref e)) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
    do { rows.Add(new[]{(int)e.id,(int)e.parent}); } while(Process32NextW(snapshot,ref e));
    var error=Marshal.GetLastWin32Error();
    if(error!=18) throw new System.ComponentModel.Win32Exception(error);
   } finally { CloseHandle(snapshot); }
   return rows.ToArray();
  }
 }
 // Drain raw bytes directly to disk. On timeout, this lease keeps both pipes
 // open until natural EOF, even after PowerShell returns from the sampler.
 public sealed class OutputLease {
  public Process Process { get; private set; }
  public Task Stdout { get; private set; }
  public Task Stderr { get; private set; }
  public OutputLease(Process process, string stdout, string stderr) {
   Process=process;
   var first=new FileStream(stdout,FileMode.CreateNew,FileAccess.Write,FileShare.ReadWrite,65536,true);
   FileStream second;
   try { second=new FileStream(stderr,FileMode.CreateNew,FileAccess.Write,FileShare.ReadWrite,65536,true); }
   catch { first.Dispose(); throw; }
   Stdout=Pump(process.StandardOutput.BaseStream,first);
   Stderr=Pump(process.StandardError.BaseStream,second);
  }
  static async Task Pump(Stream input, FileStream output) {
   using(output) {
    var buffer=new byte[65536]; int count;
    while((count=await input.ReadAsync(buffer,0,buffer.Length).ConfigureAwait(false))!=0) {
     await output.WriteAsync(buffer,0,count).ConfigureAwait(false);
     await output.FlushAsync().ConfigureAwait(false);
    }
   }
  }
  public bool Eof { get { return Stdout.IsCompletedSuccessfully && Stderr.IsCompletedSuccessfully; } }
  public bool Faulted { get { return Stdout.IsFaulted || Stderr.IsFaulted; } }
  public async Task ReleaseAfterNaturalCompletion() {
   try { await Task.WhenAll(Process.WaitForExitAsync(),Stdout,Stderr).ConfigureAwait(false); }
   catch { /* Evidence records stream failures; never terminate the process. */ }
   finally { Process.Dispose(); }
  }
 }
}
'@
}

function Set-M0ProcessSnapshot {
    param([Parameter(Mandatory)][Collections.IDictionary]$Row,
          [Parameter(Mandatory)]$Snapshot, [Parameter(Mandatory)][double]$ElapsedMs)
    if ($Snapshot.CreationTicks -ne $Row.start_ticks) { throw 'Native process snapshot identity mismatch.' }
    if ($Snapshot.CpuTimeMs -lt $Row.cpu_time_ms) { throw 'Native process CPU time regressed for a retained identity.' }
    # No final field is assigned until native times, CPU and identity all pass.
    if ($Snapshot.TerminalVerified) {
        $Row.exit_ticks = $Snapshot.ExitTicks; $Row.exited_at_utc = $Snapshot.ExitedAtUtc
        $Row.exit_code = $Snapshot.ExitCode; $Row.cpu_time_ms = $Snapshot.CpuTimeMs
        $Row.exit_observed_ms = $ElapsedMs; $Row.terminal_verified = $true
    } else { $Row.cpu_time_ms = $Snapshot.CpuTimeMs }
}

function Assert-M0PerformancePath {
    param([Parameter(Mandatory)][string]$Path, [string]$IsolatedRoot)
    $full = [IO.Path]::GetFullPath($Path).TrimEnd([char]'\', [char]'/')
    if ($IsolatedRoot) {
        $root = [IO.Path]::GetFullPath($IsolatedRoot).TrimEnd([char]'\', [char]'/')
        if ($full -ine $root -and -not $full.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) {
            throw "Directory target is outside the explicit isolated root: $full"
        }
    }
    $cursor = $full
    while ($cursor) {
        if ([IO.File]::Exists($cursor) -or [IO.Directory]::Exists($cursor)) {
            if (([IO.File]::GetAttributes($cursor) -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Reparse point rejected: $cursor"
            }
        }
        $parent = [IO.Directory]::GetParent($cursor)
        $cursor = if ($parent) { $parent.FullName } else { $null }
    }
    return $full
}

function Get-M0DirectoryObservation {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$IsolatedRoot)
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $errors = [Collections.Generic.List[string]]::new()
    [long]$bytes = 0; [long]$files = 0; [long]$directories = 0
    $exists = [IO.Directory]::Exists($Path)
    $pending = [Collections.Generic.Stack[string]]::new()
    if ($exists) { $pending.Push($Path) }
    while ($pending.Count) {
        $directory = $pending.Pop()
        try {
            $null = Assert-M0PerformancePath -Path $directory -IsolatedRoot $IsolatedRoot
            foreach ($item in [IO.DirectoryInfo]::new($directory).EnumerateFileSystemInfos()) {
                # Never traverse junctions/symlinks or count their targets.
                if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                    $errors.Add("Reparse point skipped: $($item.FullName)"); continue
                }
                if (($item.Attributes -band [IO.FileAttributes]::Directory) -ne 0) {
                    $directories++; $pending.Push($item.FullName)
                } else { $files++; $bytes += ([IO.FileInfo]$item).Length }
            }
        } catch { $errors.Add($_.Exception.Message) }
    }
    $timer.Stop()
    [pscustomobject]@{
        path = $Path; exists = $exists; logical_file_bytes = $bytes; file_count = $files; directory_count = $directories
        scan_duration_ms = $timer.Elapsed.TotalMilliseconds; complete = ($errors.Count -eq 0); errors = $errors.ToArray()
    }
}

function Invoke-M0MeasuredProcess {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Executable,
        [AllowEmptyCollection()][string[]]$Arguments = @(),
        [Parameter(Mandatory)][string]$WorkingDirectory,
        [Collections.IDictionary]$Environment = @{},
        [Parameter(Mandatory)][string]$OutputPrefix,
        [ValidateRange(1, 7200)][int]$TimeoutSeconds = 900,
        [ValidateRange(20, 10000)][int]$SampleIntervalMs = 100,
        [string]$IsolatedRoot,
        [Collections.IDictionary]$DirectoryTargets = @{},
        [ValidateRange(100, 60000)][int]$DirectorySampleIntervalMs = 1000
    )
    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'
    if ($PSVersionTable.PSVersion.Major -lt 7 -or [Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        throw 'Invoke-M0MeasuredProcess requires Windows and PowerShell 7.'
    }
    Initialize-M0PerformanceTypes
    $Executable = [IO.Path]::GetFullPath($Executable)
    $WorkingDirectory = Assert-M0PerformancePath $WorkingDirectory
    $OutputPrefix = Assert-M0PerformancePath $OutputPrefix
    if (-not [IO.File]::Exists($Executable)) { throw "Executable not found: $Executable" }
    if (-not [IO.Directory]::Exists($WorkingDirectory)) { throw "Working directory not found: $WorkingDirectory" }
    if (-not [IO.Directory]::Exists([IO.Path]::GetDirectoryName($OutputPrefix))) { throw 'Output prefix parent must already exist.' }
    $paths = [ordered]@{}
    foreach ($kind in @('process.json', 'samples.json', 'started.json', 'stdout.log', 'stderr.log')) {
        $paths[$kind] = "$OutputPrefix.$kind"
        if ([IO.File]::Exists($paths[$kind]) -or [IO.Directory]::Exists($paths[$kind])) { throw "Refusing existing output: $($paths[$kind])" }
    }
    $targets = [ordered]@{}
    if ($DirectoryTargets.Count) {
        if (-not $IsolatedRoot) { throw 'Directory sampling requires an explicit IsolatedRoot.' }
        $IsolatedRoot = Assert-M0PerformancePath $IsolatedRoot
        if (-not [IO.Directory]::Exists($IsolatedRoot) -or [IO.Path]::GetPathRoot($IsolatedRoot).TrimEnd('\') -ieq $IsolatedRoot) {
            throw 'IsolatedRoot must be an existing task directory, not a volume root.'
        }
        foreach ($key in $DirectoryTargets.Keys) { $targets[$key] = Assert-M0PerformancePath -Path $DirectoryTargets[$key] -IsolatedRoot $IsolatedRoot }
    }
    $volumeRoots = @($targets.Values | ForEach-Object { [IO.Path]::GetPathRoot($_) } | Sort-Object -Unique)
    $baselineDirectories = [ordered]@{}
    foreach ($key in $targets.Keys) { $baselineDirectories[$key] = Get-M0DirectoryObservation -Path $targets[$key] -IsolatedRoot $IsolatedRoot }
    $baselineVolumes = @($volumeRoots | ForEach-Object {
        $drive = [IO.DriveInfo]::new($_)
        [pscustomobject]@{ root = $_; available_free_bytes = $drive.AvailableFreeSpace; total_free_bytes = $drive.TotalFreeSpace }
    })
    $hash = (Get-FileHash -LiteralPath $Executable -Algorithm SHA256).Hash.ToLowerInvariant()
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable; $info.WorkingDirectory = $WorkingDirectory
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    foreach ($arg in $Arguments) { $info.ArgumentList.Add($arg) }
    foreach ($key in $Environment.Keys) {
        if ($null -eq $Environment[$key]) { $null = $info.Environment.Remove([string]$key) }
        else { $info.Environment[[string]$key] = [string]$Environment[$key] }
    }
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    $known = [Collections.Generic.List[object]]::new()
    $samples = [Collections.Generic.List[object]]::new()
    $intervals = [Collections.Generic.List[double]]::new()
    $errors = [Collections.Generic.List[object]]::new()
    $rejected = [Collections.Generic.List[object]]::new()
    $status = 'OBSERVATION_FAILED_PROCESS_NOT_KILLED'; $failure = $null
    $lease = $null; $started = $false; $lastSampleMs = $null; $lastDirectoryMs = $null
    $rootExitMs = $null; $nextNotice = 15; $lastLive = @()
    $startedAt = [DateTime]::UtcNow
    $timer = [Diagnostics.Stopwatch]::StartNew()
    try {
        $null = $process.Start(); $started = $true
        # Holding process handles retains the exact identity after exit; parents
        # are not discarded before their surviving children can be discovered.
        $null = $process.Handle
        $rootStart = $process.StartTime.ToUniversalTime()
        $known.Add([ordered]@{
            process = $process; pid = $process.Id; parent_identity = $null; name = [IO.Path]::GetFileNameWithoutExtension($Executable)
            start_ticks = $rootStart.Ticks; started_at_utc = $rootStart.ToString('o'); process_identity = "$($process.Id):$($rootStart.Ticks)"
            first_observed_ms = $timer.Elapsed.TotalMilliseconds; last_observed_ms = $null; exit_observed_ms = $null
            exit_ticks = $null; exited_at_utc = $null; exit_code = $null; cpu_time_ms = 0.0; terminal_verified = $false
            sampled_peak_working_set_bytes = 0L; sampled_peak_private_bytes = 0L
        })
        $lease = [AvelinM0PerformanceV2.OutputLease]::new($process, $paths['stdout.log'], $paths['stderr.log'])
        [IO.File]::WriteAllText($paths['started.json'], ([ordered]@{
            schema_version = 1; pid = $process.Id; process_identity = $known[0].process_identity
            started_at_utc = $rootStart.ToString('o'); executable = $Executable; executable_sha256 = $hash
            arguments = @($Arguments); working_directory = $WorkingDirectory; process_was_killed = $false
        } | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
        Write-Host "[$([IO.Path]::GetFileName($OutputPrefix))] observing PID $($process.Id)"
        while ($true) {
            $sampleTimer = [Diagnostics.Stopwatch]::StartNew()
            $now = $timer.Elapsed.TotalMilliseconds
            $interval = if ($null -eq $lastSampleMs) { $null } else { $now - $lastSampleMs }
            if ($null -ne $interval) { $intervals.Add($interval) }; $lastSampleMs = $now
            # Learn exact known-parent exit times before expanding their family.
            foreach ($row in $known) {
                if (-not $row.terminal_verified) {
                    try {
                        $snapshot = [AvelinM0PerformanceV2.ProcessObservation]::Read($row.process.SafeHandle, $row.start_ticks)
                        Set-M0ProcessSnapshot -Row $row -Snapshot $snapshot -ElapsedMs $now
                    } catch { $errors.Add([pscustomobject]@{ elapsed_ms = $now; pid = $row.pid; message = $_.Exception.Message }) }
                }
            }
            if ($null -eq $rootExitMs -and $known[0].terminal_verified) { $rootExitMs = $now }
            $snapshotCutoff = [DateTime]::UtcNow.Ticks
            $pairs = [AvelinM0PerformanceV2.ProcessTree]::Parents()
            do {
                $added = $false
                foreach ($pair in $pairs) {
                    $candidateId = [int]$pair[0]; $parentId = [int]$pair[1]
                    $parents = @($known | Where-Object { $_.pid -eq $parentId })
                    if (-not $parents.Count -or @($known | Where-Object { $_.pid -eq $candidateId -and -not $_.terminal_verified }).Count) { continue }
                    $candidate = $null
                    try {
                        $candidate = [Diagnostics.Process]::GetProcessById($candidateId); $null = $candidate.Handle
                        $candidateStart = $candidate.StartTime.ToUniversalTime()
                        $identity = "$candidateId`:$($candidateStart.Ticks)"
                        if (@($known | Where-Object { $_.process_identity -ceq $identity }).Count) { continue }
                        $parent = @($parents | Where-Object {
                            $candidateStart.Ticks -ge $_.start_ticks -and ($null -eq $_.exit_ticks -or $candidateStart.Ticks -le $_.exit_ticks)
                        } | Sort-Object start_ticks -Descending | Select-Object -First 1)
                        if (-not $parent.Count -or $candidateStart.Ticks -gt $snapshotCutoff) {
                            $rejected.Add([pscustomobject]@{ pid = $candidateId; parent_pid = $parentId; started_at_utc = $candidateStart.ToString('o'); elapsed_ms = $now; reason = 'Outside retained parent lifetime or snapshot cutoff' })
                            continue
                        }
                        $known.Add([ordered]@{
                            process = $candidate; pid = $candidateId; parent_identity = $parent[0].process_identity; name = $candidate.ProcessName
                            start_ticks = $candidateStart.Ticks; started_at_utc = $candidateStart.ToString('o'); process_identity = $identity
                            first_observed_ms = $now; last_observed_ms = $null; exit_observed_ms = $null; exit_ticks = $null
                            exited_at_utc = $null; exit_code = $null; cpu_time_ms = 0.0; terminal_verified = $false
                            sampled_peak_working_set_bytes = 0L; sampled_peak_private_bytes = 0L
                        })
                        $candidate = $null; $added = $true
                    } catch [ArgumentException] { }
                      catch [InvalidOperationException] { }
                      catch [ComponentModel.Win32Exception] { $errors.Add([pscustomobject]@{ elapsed_ms = $now; pid = $candidateId; message = $_.Exception.Message }) }
                    finally { if ($candidate) { $candidate.Dispose() } }
                }
            } while ($added)
            [long]$working = 0; [long]$private = 0; [double]$cpu = 0
            $members = [Collections.Generic.List[object]]::new()
            foreach ($row in $known) {
                if ($row.terminal_verified) { $cpu += $row.cpu_time_ms; continue }
                try {
                    $member = $row.process
                    $snapshot = [AvelinM0PerformanceV2.ProcessObservation]::Read($member.SafeHandle, $row.start_ticks)
                    Set-M0ProcessSnapshot -Row $row -Snapshot $snapshot -ElapsedMs $now
                    if ($row.terminal_verified) { $cpu += $row.cpu_time_ms; continue }
                    $member.Refresh()
                    if ($member.StartTime.ToUniversalTime().Ticks -ne $row.start_ticks) { throw "Process identity changed: $($row.process_identity)" }
                    $ws = $member.WorkingSet64; $pb = $member.PrivateMemorySize64
                    $row.last_observed_ms = $now; $working += $ws; $private += $pb; $cpu += $row.cpu_time_ms
                    $row.sampled_peak_working_set_bytes = [Math]::Max($row.sampled_peak_working_set_bytes, $ws)
                    $row.sampled_peak_private_bytes = [Math]::Max($row.sampled_peak_private_bytes, $pb)
                    $members.Add([pscustomobject]@{ pid = $row.pid; process_identity = $row.process_identity; working_set_bytes = $ws; private_bytes = $pb; cpu_time_ms = $row.cpu_time_ms })
                } catch { $errors.Add([pscustomobject]@{ elapsed_ms = $now; pid = $row.pid; message = $_.Exception.Message }) }
            }
            # A metric read can fail while its process is still alive. Do not
            # mistake a missing metric row for proof that the process exited.
            $lastLive = @($known | Where-Object { -not $_.terminal_verified } | ForEach-Object process_identity)
            if ($null -eq $rootExitMs -and $known[0].terminal_verified) { $rootExitMs = $now }
            $directoryRows = $null; $volumes = $null
            if ($targets.Count -and ($null -eq $lastDirectoryMs -or $now - $lastDirectoryMs -ge $DirectorySampleIntervalMs -or ($known[0].terminal_verified -and $lastLive.Count -eq 0))) {
                $lastDirectoryMs = $now; $directoryRows = [ordered]@{}
                foreach ($key in $targets.Keys) { $directoryRows[$key] = Get-M0DirectoryObservation -Path $targets[$key] -IsolatedRoot $IsolatedRoot }
                $volumes = @($baselineVolumes | ForEach-Object {
                    $drive = [IO.DriveInfo]::new($_.root)
                    [pscustomobject]@{ root = $_.root; available_free_bytes = $drive.AvailableFreeSpace; total_free_bytes = $drive.TotalFreeSpace; global_available_free_delta_bytes = $drive.AvailableFreeSpace - $_.available_free_bytes }
                })
            }
            $sampleTimer.Stop()
            $samples.Add([pscustomobject]@{
                elapsed_ms = $now; sampled_at_utc = [DateTime]::UtcNow.ToString('o'); actual_interval_ms = $interval
                observation_duration_ms = $sampleTimer.Elapsed.TotalMilliseconds
                working_set_bytes = $working; private_bytes = $private; observed_family_cpu_time_ms = $cpu
                process_ids = @($members | ForEach-Object pid); processes = $members.ToArray(); directories = $directoryRows; volumes = $volumes
            })
            if ($lease.Faulted) { throw 'Redirected output pump failed; raw logs may be incomplete.' }
            if ($known[0].terminal_verified -and $lease.Eof -and $lastLive.Count -eq 0) { $status = 'EXITED'; break }
            if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) { $status = 'TIMED_OUT_PROCESS_NOT_KILLED'; break }
            if ($timer.Elapsed.TotalSeconds -ge $nextNotice) {
                Write-Host "[$([IO.Path]::GetFileName($OutputPrefix))] $([int]$timer.Elapsed.TotalSeconds)s; $($known.Count) observed identities"
                $nextNotice += 15
            }
            # Sampling and directory scans are included in the actual interval.
            # Sleep only the remainder so cheap samples approximate the target.
            $sleep = [Math]::Max(1, $SampleIntervalMs - [int]$sampleTimer.ElapsedMilliseconds)
            Start-Sleep -Milliseconds $sleep
        }
    } catch { $failure = $_.Exception.ToString() }
    finally { $timer.Stop() }
    if ($status -ceq 'EXITED' -and $errors.Count) {
        $status = 'OBSERVATION_FAILED_PROCESS_NOT_KILLED'
        $failure = 'Processes exited, but one or more process observations failed; inspect observation_errors.'
    }
    $observed = @($known | ForEach-Object {
        $copy = [ordered]@{}
        foreach ($key in $_.Keys) { if ($key -ne 'process') { $copy[$key] = $_[$key] } }
        [pscustomobject]$copy
    })
    $eof = $null -ne $lease -and $lease.Eof
    $result = [ordered]@{
        schema_version = 1; executable = $Executable; executable_sha256 = $hash; arguments = @($Arguments); working_directory = $WorkingDirectory
        environment_override_keys = @($Environment.Keys); pid = $(if ($started) { $process.Id } else { $null })
        root_process_identity = $(if ($known.Count) { $known[0].process_identity } else { $null })
        started_at_utc = $startedAt.ToString('o'); ended_observation_at_utc = [DateTime]::UtcNow.ToString('o')
        duration_ms = $timer.Elapsed.TotalMilliseconds; root_exit_observed_ms = $rootExitMs; status = $status; failure = $failure
        root_process_lifetime_ms = $(if ($known.Count -and $known[0].terminal_verified) { ($known[0].exit_ticks - $known[0].start_ticks) / 10000.0 } else { $null })
        exit_code = $(if ($known.Count -and $known[0].terminal_verified) { $known[0].exit_code } else { $null })
        terminal_observation_method = 'Retained SafeProcessHandle signaled by WaitForSingleObject(0), then GetProcessTimes creation/exit/CPU validation and GetExitCodeProcess; code 259 is a valid final exit code.'
        process_was_killed = $false; logs_complete_at_eof = $eof; stdout_path = $paths['stdout.log']; stderr_path = $paths['stderr.log']
        stdout_eof = ($null -ne $lease -and $lease.Stdout.IsCompletedSuccessfully); stderr_eof = ($null -ne $lease -and $lease.Stderr.IsCompletedSuccessfully)
        log_encoding = 'Raw child stdout/stderr bytes, without decoding or encoding conversion.'
        stdout_fault = $(if ($null -ne $lease -and $lease.Stdout.IsFaulted) { $lease.Stdout.Exception.ToString() } else { $null })
        stderr_fault = $(if ($null -ne $lease -and $lease.Stderr.IsFaulted) { $lease.Stderr.Exception.ToString() } else { $null })
        stdout_sha256_at_eof = $(if ($eof) { (Get-FileHash -LiteralPath $paths['stdout.log'] -Algorithm SHA256).Hash.ToLowerInvariant() } else { $null })
        stderr_sha256_at_eof = $(if ($eof) { (Get-FileHash -LiteralPath $paths['stderr.log'] -Algorithm SHA256).Hash.ToLowerInvariant() } else { $null })
        sample_target_interval_ms = $SampleIntervalMs; sample_count = $samples.Count
        actual_max_interval_ms = $(if ($intervals.Count) { ($intervals | Measure-Object -Maximum).Maximum } else { $null })
        actual_mean_interval_ms = $(if ($intervals.Count) { ($intervals | Measure-Object -Average).Average } else { $null })
        sampled_peak_family_working_set_bytes = $(if ($samples.Count) { ($samples | Measure-Object working_set_bytes -Maximum).Maximum } else { $null })
        sampled_peak_family_private_bytes = $(if ($samples.Count) { ($samples | Measure-Object private_bytes -Maximum).Maximum } else { $null })
        observed_family_cpu_time_ms = $(if ($samples.Count) { $samples[$samples.Count - 1].observed_family_cpu_time_ms } else { $null })
        observed_processes = $observed; rejected_parent_lifetime_matches = $rejected.ToArray(); observation_errors = $errors.ToArray()
        live_process_identities_at_end = $lastLive; samples_path = $paths['samples.json']
        directory_target_interval_ms = $DirectorySampleIntervalMs; directory_baseline = $baselineDirectories; volume_baseline = $baselineVolumes
        sampling_scope = 'Discrete, sequential process-family observations, bound to retained PID + creation-time identities and parent lifetimes. Known exited parents remain eligible to discover surviving children. Entirely unobserved short-lived intermediate processes and descendants, between-sample memory peaks, and CPU of undiscovered processes can be missed. Working sets can double-count shared pages. CPU is cumulative observed process CPU time, not normalized utilization. Root exit, observed-family completion and EOF have distinct timings. Sampling adds overhead; this is not an uninstrumented benchmark.'
        directory_scope = 'Only explicit targets under IsolatedRoot are enumerated; reparse targets are rejected/skipped, file contents are not read. Counts and logical file lengths are non-atomic and exclude allocation/compression/hardlink accounting. Directory scans add measured observation overhead and can miss transient files. Volume free-space change includes all activity on that volume and is NOT attributable installer disk usage. Overlapping directory targets must not be summed.'
        timeout_scope = 'No process termination. After timeout/failure, output pipes continue draining until natural EOF while the hosting PowerShell remains alive; logs may still grow. Metadata is the observation-end snapshot, not a later completion report. The caller must stop dependent actions and reconcile surviving processes.'
    }
    try {
        [IO.File]::WriteAllText($paths['samples.json'], (ConvertTo-Json -InputObject @($samples.ToArray()) -Depth 15), [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllText($paths['process.json'], ($result | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
    } finally {
        foreach ($row in $known) { if ($row.process -ne $process) { $row.process.Dispose() } }
        if ($lease) { $null = $lease.ReleaseAfterNaturalCompletion() }
        else { $process.Dispose() }
    }
    return [pscustomobject]$result
}
