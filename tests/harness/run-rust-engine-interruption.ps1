#requires -Version 7.2
<# Real process interruption at three native upgrade transaction boundaries.
   Each case uses a new UUID product and a private directory under workspace target.
   LOCALAPPDATA isolates test locks only: the Windows SID and shell folders stay real.
   Shortcuts are disabled. Six unique current-user uninstall keys and interrupted
   scenes may remain deliberately available for inspection; no automatic repair,
   recursive deletion, process-name killing or process-tree killing is performed.
   The only forced terminations use an open Windows process handle after checking
   the session-recorded PID, creation FILETIME and executable path against that handle. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Runtime,
    [Parameter(Mandatory)][string]$Cli,
    [Parameter(Mandatory)][string]$RustWorker,
    [Parameter(Mandatory)][string]$OutputRoot,
    [string]$Sample = (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) 'examples/nimbus/payload/nimbus-demo.exe'),
    [ValidateRange(30, 1800)][int]$TimeoutSeconds = 180,
    # Internal reuse by the maintenance harness; define helpers without running cases.
    [switch]$FunctionsOnly
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$script:Utf8 = [Text.UTF8Encoding]::new($false, $true)
function Require([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Full([string]$Path) { [IO.Path]::GetFullPath($Path).TrimEnd([char]'\', [char]'/') }
function SamePath([string]$Left, [string]$Right) { [string]::Equals((Full $Left), (Full $Right), [StringComparison]::OrdinalIgnoreCase) }
function Inside([string]$Path, [string]$Root) {
    $absolute = Full $Path; $boundary = Full $Root
    Require ($absolute.StartsWith($boundary + '\', [StringComparison]::OrdinalIgnoreCase)) "Path escaped fixture: $absolute"
    for ($cursor = $absolute; $cursor; $cursor = Split-Path -Parent $cursor) {
        if (Test-Path -LiteralPath $cursor) {
            Require (((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse point: $cursor"
        }
    }
    return $absolute
}
function Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Json([string]$Path) { $script:Utf8.GetString([IO.File]::ReadAllBytes($Path)) | ConvertFrom-Json }
function Save([string]$Path, [object]$Value) {
    $path = Inside $Path $script:RunRoot
    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $path))
    [IO.File]::WriteAllText($path, ($Value | ConvertTo-Json -Depth 60), $script:Utf8)
}
function WriteNew([string]$Path, [string]$Text) {
    $path = Inside $Path $script:RunRoot
    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $path))
    $stream = [IO.File]::Open($path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $stream.Write($script:Utf8.GetBytes($Text)) } finally { $stream.Dispose() }
}
function Snapshot([string]$Root) {
    if (-not (Test-Path -LiteralPath $Root)) { return }
    $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push($Root)
    $items = while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            $safe = Inside $item.FullName $Root
            if ($item.PSIsContainer) {
                $pending.Push($safe)
                [pscustomobject]@{ path = $safe.Substring($Root.Length + 1); kind = 'directory'; size = 0; sha256 = $null }
            } else {
                [pscustomobject]@{ path = $safe.Substring($Root.Length + 1); kind = 'file'; size = $item.Length; sha256 = Hash $safe }
            }
        }
    }
    @($items | Sort-Object path)
}
function RegistrySnapshot {
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, [Microsoft.Win32.RegistryView]::Registry64)
    $key = $null
    try {
        $key = $base.OpenSubKey($script:RegistrySubkey, $false); $values = [ordered]@{}
        if ($key) {
            Require ($key.SubKeyCount -eq 0) 'Unexpected product registration subkeys'
            foreach ($name in ($key.GetValueNames() | Sort-Object)) {
                $values[$name] = @{ value = $key.GetValue($name); kind = "$($key.GetValueKind($name))" }
            }
        }
        [pscustomobject]@{ exists = $null -ne $key; values = $values }
    } finally { if ($key) { $key.Dispose() }; $base.Dispose() }
}
function ProductSnapshot { [pscustomobject]@{ files = @(Snapshot $script:InstallRoot); registry = RegistrySnapshot } }
function SameSnapshot([object]$Before, [object]$After) { ($Before | ConvertTo-Json -Depth 60 -Compress) -ceq ($After | ConvertTo-Json -Depth 60 -Compress) }
function LockSnapshot([string]$Path) {
    if (Test-Path -LiteralPath $Path) { return @{ exists = $true; sha256 = Hash $Path } }
    return @{ exists = $false; sha256 = $null }
}

# Holding this handle closes the PID-reuse race between validation and termination.
# No process name or enumerated process list is used to choose a victim.
if (-not ('AvelinInterruption.VerifiedProcess' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;
namespace AvelinInterruption {
    public sealed class VerifiedProcess : IDisposable {
        [StructLayout(LayoutKind.Sequential)] struct FileTime { public uint Low; public uint High; }
        [DllImport("kernel32.dll", SetLastError=true)] static extern SafeProcessHandle OpenProcess(uint access, bool inherit, uint pid);
        [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetProcessTimes(SafeProcessHandle process, out FileTime created, out FileTime exited, out FileTime kernel, out FileTime user);
        [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool QueryFullProcessImageName(SafeProcessHandle process, uint flags, StringBuilder image, ref uint size);
        [DllImport("kernel32.dll", SetLastError=true)] static extern bool TerminateProcess(SafeProcessHandle process, uint code);
        [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForSingleObject(SafeProcessHandle process, uint milliseconds);
        [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetExitCodeProcess(SafeProcessHandle process, out uint code);
        readonly SafeProcessHandle handle;
        public uint ProcessId { get; private set; }
        public ulong CreationTime100ns { get; private set; }
        public string Executable { get; private set; }
        public VerifiedProcess(uint pid, ulong expectedCreated, string expectedExecutable) {
            if (pid == (uint)System.Diagnostics.Process.GetCurrentProcess().Id) throw new InvalidOperationException("Refuse harness self-termination");
            handle = OpenProcess(0x00101001, false, pid); // SYNCHRONIZE | QUERY_LIMITED_INFORMATION | TERMINATE
            try {
                if (handle.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error());
                FileTime created, exited, kernel, user;
                if (!GetProcessTimes(handle, out created, out exited, out kernel, out user)) throw new Win32Exception(Marshal.GetLastWin32Error());
                CreationTime100ns = ((ulong)created.High << 32) | created.Low;
                var image = new StringBuilder(32768); uint length = (uint)image.Capacity;
                if (!QueryFullProcessImageName(handle, 0, image, ref length)) throw new Win32Exception(Marshal.GetLastWin32Error());
                Executable = image.ToString(); ProcessId = pid;
                if (CreationTime100ns != expectedCreated || !String.Equals(System.IO.Path.GetFullPath(Executable), System.IO.Path.GetFullPath(expectedExecutable), StringComparison.OrdinalIgnoreCase))
                    throw new InvalidOperationException("Process identity differs from recorded task; termination refused");
                if (WaitForSingleObject(handle, 0) != 258) throw new InvalidOperationException("Recorded task process already exited");
            } catch { handle.Dispose(); throw; }
        }
        public bool Wait(int milliseconds) {
            uint result = WaitForSingleObject(handle, checked((uint)milliseconds));
            if (result == 0) return true;
            if (result == 258) return false;
            throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        public uint ExitCode {
            get { if (!Wait(0)) throw new InvalidOperationException("Process is still running"); uint code; if (!GetExitCodeProcess(handle, out code)) throw new Win32Exception(Marshal.GetLastWin32Error()); return code; }
        }
        public void Terminate() {
            if (Wait(0)) throw new InvalidOperationException("Recorded task exited before interruption");
            if (!TerminateProcess(handle, 137)) throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        public void Dispose() { handle.Dispose(); }
    }
}
'@
}

function StartFixtureProcess([string]$Executable, [string[]]$Arguments, [string]$Label) {
    $info = [Diagnostics.ProcessStartInfo]::new($Executable)
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true; $info.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
    $info.WorkingDirectory = $script:CaseRoot; $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = $script:Utf8; $info.StandardErrorEncoding = $script:Utf8
    $info.Environment['LOCALAPPDATA'] = $script:LocalRoot; $info.Environment['TEMP'] = $script:TempRoot; $info.Environment['TMP'] = $script:TempRoot
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    $null = $process.Start()
    $record = [ordered]@{ label = $Label; executable = $Executable; executable_sha256 = Hash $Executable; arguments = $Arguments; process_id = $process.Id; status = 'RUNNING'; started_at_utc = [DateTime]::UtcNow.ToString('o') }
    Save (Join-Path $script:CaseRoot "$Label.process.json") $record
    [pscustomobject]@{ Process = $process; Stdout = $process.StandardOutput.ReadToEndAsync(); Stderr = $process.StandardError.ReadToEndAsync(); Record = $record; Label = $Label }
}
function FinishFixtureProcess([object]$Running, [int]$ExpectedExit, [string]$AwaitReport = '') {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    try {
        do {
            $exited = $Running.Process.WaitForExit(100); $reported = -not $AwaitReport
            if ($AwaitReport -and (Test-Path -LiteralPath $AwaitReport)) {
                try { $reported = (Json $AwaitReport).result -in @('Succeeded', 'Failed', 'Cancelled', 'Unknown') } catch { $reported = $false }
            }
            if ($exited -and $Running.Stdout.IsCompleted -and $Running.Stderr.IsCompleted -and ($reported -or $Running.Process.ExitCode -ne 0)) { break }
            Require ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds) "$($Running.Label) timed out; retained process was not killed"
        } while ($true)
        $Running.Record.status = 'EXITED'; $Running.Record.exit_code = $Running.Process.ExitCode
        [IO.File]::WriteAllText((Join-Path $script:CaseRoot "$($Running.Label).stdout.log"), $Running.Stdout.GetAwaiter().GetResult(), $script:Utf8)
        [IO.File]::WriteAllText((Join-Path $script:CaseRoot "$($Running.Label).stderr.log"), $Running.Stderr.GetAwaiter().GetResult(), $script:Utf8)
        Require ($Running.Process.ExitCode -eq $ExpectedExit) "$($Running.Label) exited $($Running.Process.ExitCode), expected $ExpectedExit"
    } finally {
        Save (Join-Path $script:CaseRoot "$($Running.Label).process.json") $Running.Record
        $Running.Process.Dispose()
    }
}
function InvokeFixtureProcess([string]$Executable, [string[]]$Arguments, [string]$Label, [int]$ExpectedExit = 0, [string]$AwaitReport = '') {
    $running = StartFixtureProcess $Executable $Arguments $Label
    FinishFixtureProcess $running $ExpectedExit $AwaitReport
}
function BuildPackage([string]$Label, [string]$Version, [switch]$Shortcuts) {
    $project = Inside (Join-Path $script:CaseRoot "project-$Label") $script:RunRoot
    $payload = Join-Path $project 'payload'; $null = [IO.Directory]::CreateDirectory($payload)
    [IO.File]::Copy($script:SampleBinary, (Join-Path $payload 'app.exe'), $false)
    WriteNew (Join-Path $payload '00-version.txt') "Version $Version"
    WriteNew (Join-Path $payload 'nested/data.txt') "Data $Version"
    if ($Version -ceq '1.0.0') { WriteNew (Join-Path $payload 'retired.txt') 'Old owned resource' }
    else { WriteNew (Join-Path $payload 'new.txt') 'New owned resource' }
    $config = Join-Path $project 'avelin.toml'
    $shortcutDefault = if ($Shortcuts) { 'true' } else { 'false' }
    WriteNew $config @"
schema_version = 1
[app]
id = "$($script:AppId)"
name = "Native interruption fixture"
version = "$Version"
entry = "app.exe"
[payload]
directory = "payload"
[install]
directory_name = "AvelinInterruptionFixture"
desktop_shortcut = $shortcutDefault
start_menu_shortcut = $shortcutDefault
"@
    $setup = Join-Path $script:CaseRoot "$Label-Setup.exe"
    InvokeFixtureProcess $script:CliBinary @('build', '--config', $config, '--output', $setup, '--runtime', $script:RuntimeBinary, '--rust-worker', $script:WorkerBinary) "build-$Label"
    $built = Json (Join-Path $script:CaseRoot "build-$Label.stdout.log")
    Require ($built.engine -ceq 'rust') 'Requested native backend was not built'
    $manifest = Json ([IO.Path]::ChangeExtension($setup, 'manifest.json'))
    Require ($manifest.app_id -ceq $script:AppId -and $manifest.app_version -ceq $Version) 'Built fixture identity differs'
    foreach ($file in $manifest.files) {
        $source = Inside (Join-Path $payload $file.path) $payload
        Require ((Hash $source) -ceq $file.sha256 -and (Get-Item -LiteralPath $source).Length -eq $file.size) 'Manifest differs from fixture input'
    }
    [pscustomobject]@{ Setup = $setup; Backend = [IO.Path]::ChangeExtension($setup, 'backend.exe'); Manifest = $manifest }
}
function VerifyVersion([object]$Package, [string]$TaskId = '') {
    $receipt = Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    Require ($receipt.app_id -ceq $script:AppId -and $receipt.app_version -ceq $Package.Manifest.app_version -and $receipt.user_sid -ceq $script:UserSid -and (SamePath $receipt.install_dir $script:InstallRoot)) 'Installed receipt identity differs'
    if ($TaskId) { Require ($receipt.task_id -ceq $TaskId) 'Installed receipt belongs to another task' }
    foreach ($file in $Package.Manifest.files) {
        $path = Inside (Join-Path $script:InstallRoot $file.path) $script:InstallRoot
        Require ((Hash $path) -ceq $file.sha256 -and (Get-Item -LiteralPath $path).Length -eq $file.size) 'Installed payload differs from expected version'
    }
    Require ((Hash (Join-Path $script:InstallRoot '.avelin/avelin-app.exe')) -ceq (Hash $script:RuntimeBinary)) 'Installed runtime differs'
    Require ((Hash (Join-Path $script:InstallRoot 'uninstall.exe')) -ceq $receipt.uninstaller_sha256) 'Installed worker differs from receipt'
    $registration = RegistrySnapshot
    Require ($registration.exists -and $registration.values['DisplayVersion'].value -ceq $Package.Manifest.app_version) 'Installed registration version differs'
    Require ((Hash (Join-Path $script:InstallRoot 'user-sentinel.txt')) -ceq $script:SentinelHash) 'Unknown user content changed'
}
function WaitPause([object]$Running, [string]$Point) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        Require (-not $Running.Process.HasExited) 'Coordinator exited before the requested pause'
        foreach ($directory in Get-ChildItem -LiteralPath $script:CaseRoot -Directory -Filter 'session-*') {
            $session = Inside $directory.FullName $script:RunRoot
            $pausePath = Join-Path $session 'engine-pause.json'
            if (-not (Test-Path -LiteralPath $pausePath)) { continue }
            try { $pause = Json $pausePath; $identity = Json (Join-Path $session 'session.json'); $plan = Json (Join-Path $session 'install-plan.json') } catch { continue }
            Require ($pause.task_id -ceq $identity.task_id -and $pause.task_id -ceq $plan.task_id -and $pause.point -ceq $Point -and $pause.backend_pid -eq $identity.backend.pid) 'Pause evidence identity differs'
            Require ($identity.coordinator.pid -eq $Running.Process.Id -and $identity.operation -ceq 'upgrade' -and $plan.operation -ceq 'upgrade' -and $identity.app_id -ceq $script:AppId -and $plan.app_version -ceq '1.1.0' -and (SamePath $identity.install_dir $script:InstallRoot)) 'Pause belongs to another process or product'
            Require ((SamePath $identity.coordinator.executable $Running.Record.executable) -and (Hash $identity.backend.executable) -ceq (Hash $script:PackageB.Backend)) 'Recorded executable differs from the known fixture'
            # The worker resumes after 30 seconds. Leave a three-second margin
            # so a slow evidence read cannot kill it after this pause expired.
            $pauseDeadline = (Get-Item -LiteralPath $pausePath).LastWriteTimeUtc.AddSeconds(27)
            Require ([DateTime]::UtcNow -lt $pauseDeadline) 'Pause is already too old to interrupt at a known boundary'
            return [pscustomobject]@{ Session = $session; Identity = $identity; Pause = $pause; Plan = $plan; Deadline = $pauseDeadline }
        }
        Start-Sleep -Milliseconds 20
    }
    throw 'Pause observation timed out; no process was killed'
}
function Journal([string]$Session) {
    $path = Join-Path $Session 'upgrade-journal.jsonl'
    if (Test-Path -LiteralPath $path) { @([IO.File]::ReadAllLines($path, $script:Utf8) | Where-Object { $_ } | ForEach-Object { $_ | ConvertFrom-Json }) }
}
function AssertPaused([object]$Evidence, [string]$Point, [object]$Baseline) {
    $journal = @(Journal $Evidence.Session)
    foreach ($event in $journal) { Require ($event.task_id -ceq $Evidence.Identity.task_id) 'Cross-task native journal' }
    $markerPath = Join-Path $script:InstallRoot '.avelin/upgrade-state.json'
    if ($Point -ceq 'prepared') {
        Require (-not (Test-Path -LiteralPath $markerPath)) 'Prepared pause already changed the transaction marker'
        Require (SameSnapshot $Baseline (ProductSnapshot)) 'Prepared pause changed installed resources'
        Require (@($journal | Where-Object event -EQ 'committed').Count -eq 0) 'Prepared pause is already committed'
    } else {
        $marker = Json $markerPath
        Require ($marker.task_id -ceq $Evidence.Identity.task_id) 'Missing or foreign transaction marker'
        Require (@($journal | Where-Object event -EQ 'published').Count -ge 1) 'Mutation pause was not reached after publication'
        if ($Point -ceq 'after-first-publish') {
            Require (@($journal | Where-Object event -EQ 'committed').Count -eq 0) 'First-publication pause happened after commit'
            $first = @($script:PackageB.Manifest.files | Where-Object path -CEQ '00-version.txt')[0]
            Require ((Hash (Join-Path $script:InstallRoot '00-version.txt')) -ceq $first.sha256) 'First new payload was not actually published'
        } else {
            Require (@($journal | Where-Object event -EQ 'committed').Count -eq 1) 'Post-commit pause has no unique durable commit'
            VerifyVersion $script:PackageB $Evidence.Identity.task_id
        }
    }
    Save (Join-Path $script:CaseRoot 'pause.evidence.json') @{ pause = $Evidence.Pause; identity = $Evidence.Identity; journal = $journal; product = ProductSnapshot }
}

if ($FunctionsOnly) { return }
Require ($IsWindows -and [Environment]::Is64BitProcess) 'Use Windows x64 PowerShell 7.2+'
$projectRoot = Full (Join-Path $PSScriptRoot '../..')
$script:RunRoot = Inside $OutputRoot (Join-Path $projectRoot 'target')
Require (-not (Test-Path -LiteralPath $script:RunRoot)) 'Choose a new OutputRoot; interrupted evidence cannot be replaced'
$null = [IO.Directory]::CreateDirectory($script:RunRoot)
$script:RuntimeBinary = Full $Runtime; $script:CliBinary = Full $Cli; $script:WorkerBinary = Full $RustWorker; $script:SampleBinary = Full $Sample
foreach ($binary in @($script:RuntimeBinary, $script:CliBinary, $script:WorkerBinary, $script:SampleBinary)) { Require (Test-Path -LiteralPath $binary -PathType Leaf) "Missing component: $binary" }
$script:UserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$summary = [ordered]@{ schema_version = 1; status = 'RUNNING'; started_at_utc = [DateTime]::UtcNow.ToString('o'); run_root = $script:RunRoot; cases = @(); components = @(); limitations = @('Only the explicitly paused boundaries are covered; arbitrary instruction interruption is not exhaustively proven', 'No machine reboot, system-volume exhaustion, signing or independent Windows environment claims'); retained_scenes = @() }
foreach ($binary in @($script:RuntimeBinary, $script:CliBinary, $script:WorkerBinary, $script:SampleBinary)) { $summary.components += @{ path = $binary; sha256 = Hash $binary } }
try {
    foreach ($point in @('prepared', 'after-first-publish', 'after-commit')) {
        foreach ($victim in @('backend', 'coordinator')) {
            $label = "$point-kill-$victim"; Write-Host "[$label] creating isolated upgrade fixture"
            $script:CaseRoot = Inside (Join-Path $script:RunRoot $label) $script:RunRoot
            $script:LocalRoot = Join-Path $script:CaseRoot 'local-data'; $script:TempRoot = Join-Path $script:CaseRoot 'temp'; $script:InstallRoot = Join-Path $script:CaseRoot 'installed 中文'
            foreach ($directory in @($script:CaseRoot, $script:LocalRoot, $script:TempRoot, $script:InstallRoot)) { $null = [IO.Directory]::CreateDirectory($directory) }
            $script:AppId = 'dev.avelin.interrupt.' + [Guid]::NewGuid().ToString('N')
            $script:RegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:AppId
            Require (-not (RegistrySnapshot).exists) 'Unique fixture registration already exists'
            WriteNew (Join-Path $script:InstallRoot 'user-sentinel.txt') 'Preserve unrelated user content through interruption'
            $script:SentinelHash = Hash (Join-Path $script:InstallRoot 'user-sentinel.txt')
            Save (Join-Path $script:CaseRoot 'fixture.json') @{ app_id = $script:AppId; install_dir = $script:InstallRoot; registry_subkey = $script:RegistrySubkey; sid = $script:UserSid; point = $point; victim = $victim }
            $packageA = BuildPackage 'a' '1.0.0'; $script:PackageB = BuildPackage 'b' '1.1.0'
            $installReportPath = Join-Path $script:CaseRoot 'install-a.report.json'
            InvokeFixtureProcess $packageA.Setup @('--silent', '--install-dir', $script:InstallRoot, '--report', $installReportPath) 'install-a' -AwaitReport $installReportPath
            $installReport = Json $installReportPath
            Require ($installReport.result -ceq 'Succeeded' -and $installReport.coordinator_exit_code -eq 0) 'Initial installation failed'
            VerifyVersion $packageA $installReport.task_id
            $baseline = ProductSnapshot; Save (Join-Path $script:CaseRoot 'baseline.json') $baseline
            $reportPath = Join-Path $script:CaseRoot 'interrupted-upgrade.report.json'
            $running = StartFixtureProcess $script:PackageB.Setup @('--silent', '--operation', 'upgrade', '--install-dir', $script:InstallRoot, '--report', $reportPath, '--engine-pause', $point) 'interrupted-upgrade'
            $evidence = WaitPause $running $point
            $backendHandle = $null; $coordinatorHandle = $null
            try {
                $backendHandle = [AvelinInterruption.VerifiedProcess]::new([uint32]$evidence.Identity.backend.pid, [uint64]$evidence.Identity.backend.creation_time_100ns, $evidence.Identity.backend.executable)
                $coordinatorHandle = [AvelinInterruption.VerifiedProcess]::new([uint32]$evidence.Identity.coordinator.pid, [uint64]$evidence.Identity.coordinator.creation_time_100ns, $evidence.Identity.coordinator.executable)
                AssertPaused $evidence $point $baseline
                $victimHandle = if ($victim -ceq 'backend') { $backendHandle } else { $coordinatorHandle }
                Save (Join-Path $script:CaseRoot 'termination.intent.json') @{ task_id = $evidence.Identity.task_id; victim = $victim; process_id = $victimHandle.ProcessId; creation_time_100ns = $victimHandle.CreationTime100ns; executable = $victimHandle.Executable; expected_termination_code = 137; point = $point; verified_at_utc = [DateTime]::UtcNow.ToString('o') }
                Require ([DateTime]::UtcNow -lt $evidence.Deadline) 'Pause verification exceeded its safe window; no process was killed'
                $victimHandle.Terminate()
                Require ($victimHandle.Wait(10000) -and $victimHandle.ExitCode -eq 137) 'Selected task process did not terminate as observed'
                Require ($backendHandle.Wait(30000) -and $coordinatorHandle.Wait(30000)) 'Surviving task process did not stop; no additional process was killed'
                $backendExit = $backendHandle.ExitCode; $coordinatorExit = $coordinatorHandle.ExitCode
                Require ($backendExit -eq $(if ($victim -ceq 'backend') { 137 } else { 4 }) -and $coordinatorExit -eq $(if ($victim -ceq 'coordinator') { 137 } else { 4 })) 'Interrupted lifecycle returned an unexpected process outcome'
                Save (Join-Path $script:CaseRoot 'termination.observed.json') @{ task_id = $evidence.Identity.task_id; backend_exit_code = $backendExit; coordinator_exit_code = $coordinatorExit; both_recorded_processes_stopped = $true }
            } finally {
                if ($backendHandle) { $backendHandle.Dispose() }; if ($coordinatorHandle) { $coordinatorHandle.Dispose() }
            }
            FinishFixtureProcess $running $(if ($victim -ceq 'coordinator') { 137 } else { 4 })
            if ($victim -ceq 'backend') {
                $report = Json $reportPath
                Require ($report.result -ceq 'Unknown' -and $report.coordinator_exit_code -eq 4 -and $report.task_id -ceq $evidence.Identity.task_id) 'Killed backend was falsely reported as confirmed success/failure/cancellation'
            } else {
                Require (-not (Test-Path -LiteralPath $reportPath)) 'Killed coordinator unexpectedly published a final report'
                $engineResult = Json (Join-Path $evidence.Session 'engine-result.json')
                $expectedTransactionCode = if ($point -ceq 'after-commit') { 4 } else { 3 }
                Require ($engineResult.engine -ceq 'rust' -and $engineResult.transaction_outcome_code -eq $expectedTransactionCode) 'Surviving worker did not persist the expected transaction outcome before failed IPC delivery'
                $journal = @(Journal $evidence.Session)
                $expectedTerminal = if ($point -ceq 'after-commit') { 'committed' } else { 'rolled_back' }
                Require ($journal.Count -gt 0 -and $journal[-1].event -ceq $expectedTerminal) 'Surviving worker journal does not confirm rollback or the retained commit'
            }
            $markerPath = Join-Path $script:InstallRoot '.avelin/upgrade-state.json'; $lockPath = $evidence.Identity.product_lock
            $null = Inside $lockPath $script:LocalRoot
            if ($point -ceq 'prepared' -or ($point -ceq 'after-first-publish' -and $victim -ceq 'coordinator')) {
                Require (SameSnapshot $baseline (ProductSnapshot)) 'Pre-commit survivor did not preserve or restore the original installation exactly'
                Require (-not (Test-Path -LiteralPath $markerPath)) 'Confirmed original installation retains a marker'
            } else {
                Require ((Json $markerPath).task_id -ceq $evidence.Identity.task_id) 'Unconfirmed mutation lost its transaction marker'
                if ($point -ceq 'after-commit') { VerifyVersion $script:PackageB $evidence.Identity.task_id }
            }
            Require ((Test-Path -LiteralPath $lockPath) -eq ($victim -ceq 'coordinator')) 'Product lock retention does not match coordinator lifetime'
            $productBeforeDiagnosis = ProductSnapshot; $sessionBeforeDiagnosis = @(Snapshot $evidence.Session); $lockBeforeDiagnosis = LockSnapshot $lockPath
            InvokeFixtureProcess $script:CliBinary @('diagnose', '--session', $evidence.Session) 'diagnose'
            $diagnosis = Json (Join-Path $script:CaseRoot 'diagnose.stdout.log')
            Require ($diagnosis.read_only -and -not $diagnosis.automatic_retry_allowed -and $diagnosis.assessment -ceq 'NeedsReview' -and $diagnosis.task_id -ceq $evidence.Identity.task_id) 'Interruption diagnosis falsely confirmed completion or authorized automatic recovery'
            Require ($diagnosis.processes.backend.state -ceq 'NotRunning' -and $diagnosis.processes.coordinator.state -ceq 'NotRunning') 'Diagnosis did not observe both task processes stopped'
            Require ((SameSnapshot $productBeforeDiagnosis (ProductSnapshot)) -and (SameSnapshot $sessionBeforeDiagnosis @(Snapshot $evidence.Session)) -and (SameSnapshot $lockBeforeDiagnosis (LockSnapshot $lockPath))) 'Read-only diagnosis changed product or interruption evidence'
            $retryReportPath = Join-Path $script:CaseRoot 'retry.report.json'
            if ($point -ceq 'prepared' -and $victim -ceq 'backend') {
                InvokeFixtureProcess $script:PackageB.Setup @('--silent', '--operation', 'upgrade', '--install-dir', $script:InstallRoot, '--report', $retryReportPath) 'retry' -AwaitReport $retryReportPath
                $retry = Json $retryReportPath; Require ($retry.result -ceq 'Succeeded') 'Untouched pre-mutation installation could not safely retry'
                VerifyVersion $script:PackageB $retry.task_id
                $cleanupReport = Join-Path $script:CaseRoot 'cleanup.report.json'
                InvokeFixtureProcess $script:RuntimeBinary @('--silent', '--operation', 'uninstall', '--backend', (Join-Path $script:InstallRoot 'uninstall.exe'), '--manifest', (Join-Path $script:InstallRoot '.avelin/manifest.json'), '--install-dir', $script:InstallRoot, '--report', $cleanupReport) 'cleanup' -AwaitReport $cleanupReport
                Require ((Json $cleanupReport).result -ceq 'Succeeded' -and -not (RegistrySnapshot).exists) 'Resolved safe-retry fixture could not be uninstalled'
                Require ((Hash (Join-Path $script:InstallRoot 'user-sentinel.txt')) -ceq $script:SentinelHash) 'Safe retry cleanup changed user content'
                $retryResult = 'AllowedAfterCompletePreflight'
            } else {
                InvokeFixtureProcess $script:PackageB.Setup @('--silent', '--operation', 'upgrade', '--install-dir', $script:InstallRoot, '--report', $retryReportPath) 'retry' -ExpectedExit 3
                $retryError = [IO.File]::ReadAllText((Join-Path $script:CaseRoot 'retry.stderr.log'))
                Require ($retryError -match '(?i)(unfinished|unresolved|product lock|upgrade state|pending)') 'Reentry failed without the intended lock/marker reason'
                Require ((SameSnapshot $productBeforeDiagnosis (ProductSnapshot)) -and (SameSnapshot $lockBeforeDiagnosis (LockSnapshot $lockPath))) 'Rejected reentry changed retained product or lock'
                $retryResult = 'RefusedPendingInspection'
                $summary.retained_scenes += @{ app_id = $script:AppId; install_dir = $script:InstallRoot; registry_subkey = $script:RegistrySubkey; session = $evidence.Session; marker_present = Test-Path -LiteralPath $markerPath; product_lock = $lockPath }
            }
            $caseResult = @{ label = $label; status = 'PASSED'; task_id = $evidence.Identity.task_id; session = $evidence.Session; backend_exit = $backendExit; coordinator_exit = $coordinatorExit; retry = $retryResult; diagnosis = $diagnosis.assessment }
            Save (Join-Path $script:CaseRoot 'result.json') $caseResult
            $summary.cases += $caseResult; Save (Join-Path $script:RunRoot 'summary.json') $summary
            Write-Host "[$label] passed; $retryResult"
        }
    }
    $summary.status = 'PASSED'
} catch {
    $summary.status = 'FAILED'; $summary.failure = $_.Exception.Message
    throw
} finally {
    $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o')
    Save (Join-Path $script:RunRoot 'summary.json') $summary
    Write-Host "Interruption evidence retained: $script:RunRoot"
}
