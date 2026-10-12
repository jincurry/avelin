[CmdletBinding()]
param(
    [switch]$PlanOnly,
    [string]$ProjectRoot = (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)),
    [string]$CliPath,
    [string]$AppPath,
    [string]$BridgePath,
    [ValidateSet('R01', 'R02', 'R03', 'R04')][string[]]$Cases = @('R01', 'R02', 'R03', 'R04'),
    [ValidateRange(15, 600)][int]$TimeoutSeconds = 90
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$casePlan = @(
    @{ id = 'R01'; fault = 'Terminate only the identified test coordinator after the first written-file event.'; expected = 'Backend exits by itself with code 4, known files are cleaned, no receipt/registration, stale product lock retained.' },
    @{ id = 'R02'; fault = 'Terminate only the identified test NSIS child after the first written-file event.'; expected = 'Coordinator reports Unknown and exits 4; diagnostic journal and partial-write evidence retained.' },
    @{ id = 'R03'; fault = 'Modify an installed managed text file before uninstall.'; expected = 'Uninstall refuses with nonzero status and a diagnostic; all installed files and unknown user files remain unchanged.' },
    @{ id = 'R04'; fault = 'Hold an installed managed file with .NET FileStream(FileShare.None) during uninstall.'; expected = 'Uninstall refuses with nonzero status and a diagnostic; no false success. Explicitly distinguish preflight refusal from backend deletion failure.' }
)
if ($PlanOnly) {
    [ordered]@{
        schema_version = 1; status = 'PLAN_ONLY_NOT_EXECUTED'; cases = @($casePlan | Where-Object { $_.id -in $Cases })
        scope = 'Local headless development-machine resilience only; no full M0 acceptance claim.'
        isolation = 'Unique run and per-case app IDs; empty targets; TEMP/TMP/LOCALAPPDATA inside the run. Preserve failed-operation scenes and logs. No recursive deletion, process-tree kill, or forced cancellation.'
    } | ConvertTo-Json -Depth 8
    exit 0
}

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Full-Path([string]$Path) { [IO.Path]::GetFullPath($Path).TrimEnd([char]'\', [char]'/') }
function Safe-Path([string]$Path, [string]$Root, [switch]$AllowRoot) {
    $absolute = Full-Path $Path
    $base = Full-Path $Root
    Assert-True ($absolute.StartsWith($base + '\', [StringComparison]::OrdinalIgnoreCase) -or ($AllowRoot -and $absolute.Equals($base, [StringComparison]::OrdinalIgnoreCase))) "Path escapes task root: $absolute"
    $cursor = $absolute
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            Assert-True (((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse point rejected: $cursor"
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if (-not $parent) { break }
        $cursor = $parent.FullName
    }
    $absolute
}
function Save-Json([string]$Path, [object]$Value) {
    $safe = Safe-Path $Path $script:RunRoot
    [IO.File]::WriteAllText($safe, ($Value | ConvertTo-Json -Depth 40), [Text.UTF8Encoding]::new($false))
}
function File-Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function File-Snapshot([string]$Directory) {
    if (-not (Test-Path -LiteralPath $Directory -PathType Container)) { return @() }
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push((Full-Path $Directory))
    $files = @()
    while ($pending.Count) {
        foreach ($item in @(Get-ChildItem -LiteralPath $pending.Pop() -Force)) {
            $safe = Safe-Path $item.FullName $Directory
            if ($item.PSIsContainer) { $pending.Push($safe); continue }
            $hash = $null; $readError = $null
            try { $hash = File-Hash $safe } catch { $readError = $_.Exception.Message }
            $files += [pscustomobject]@{ path = $safe.Substring((Full-Path $Directory).Length + 1); size = $item.Length; sha256 = $hash; read_error = $readError }
        }
    }
    @($files | Sort-Object path)
}
function Registry-Snapshot {
    foreach ($view in @([Microsoft.Win32.RegistryView]::Registry32, [Microsoft.Win32.RegistryView]::Registry64)) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $view)
        $key = $null
        try {
            $key = $base.OpenSubKey($script:Case.registry_subkey, $false)
            $values = [ordered]@{}
            if ($key) { foreach ($name in $key.GetValueNames()) { $values[$name] = $key.GetValue($name) } }
            [pscustomobject]@{ view = "$view"; exists = ($null -ne $key); values = $values }
        }
        finally { if ($key) { $key.Dispose() }; $base.Dispose() }
    }
}
function Session-Evidence {
    $sessions = @()
    foreach ($directory in @(Get-ChildItem -LiteralPath $script:Case.directory -Directory -Filter 'session-*')) {
        $safe = Safe-Path $directory.FullName $script:Case.directory
        $journal = Join-Path $safe 'events.jsonl'
        $events = @()
        if (Test-Path -LiteralPath $journal) {
            foreach ($line in @(Get-Content -LiteralPath $journal)) {
                if ($line) { try { $events += $line | ConvertFrom-Json } catch { $events += @{ incomplete_line = $line } } }
            }
        }
        # task.ini contains the authentication token: record its hash, never its content.
        $sessions += [pscustomobject]@{ directory = $safe; files = @(File-Snapshot $safe); events = $events }
    }
    $sessions
}
function Process-Identity([Diagnostics.Process]$Process, [string]$LaunchedExecutable) {
    $Process.Refresh()
    $imageBuffer = [Text.StringBuilder]::new(32768)
    $imageLength = $imageBuffer.Capacity
    $imageObserved = [M0Resilience.NativeProcess]::QueryFullProcessImageName($Process.Handle, 0, $imageBuffer, [ref]$imageLength)
    $identitySource = 'live process image'
    if ($imageObserved) { $executable = Full-Path $imageBuffer.ToString() }
    else {
        Assert-True ($LaunchedExecutable -and $Process.HasExited) 'Cannot identify a live process image; no fault may be injected.'
        $executable = Full-Path $LaunchedExecutable
        $identitySource = 'explicit ProcessStartInfo image; process exited before image observation'
    }
    [ordered]@{
        process_id = $Process.Id
        started_at_utc = $Process.StartTime.ToUniversalTime().ToString('o')
        start_time_ticks = $Process.StartTime.ToUniversalTime().Ticks
        executable = $executable
        identity_source = $identitySource
    }
}
function Owned-ProcessState {
    foreach ($owned in $script:OwnedProcesses) {
        $owned.process.Refresh()
        [pscustomobject]@{
            name = $owned.name; identity = $owned.identity; exited = $owned.process.HasExited
            exit_code = $(if ($owned.process.HasExited) { $owned.process.ExitCode } else { $null })
        }
    }
}
function Scoped-LiveProcesses {
    # This is a read-only inventory. It never supplies targets to a kill operation.
    @(Get-CimInstance Win32_Process | Where-Object {
        ($_.ExecutablePath -and $_.ExecutablePath.StartsWith($script:RunRoot + '\', [StringComparison]::OrdinalIgnoreCase)) -or
        ($_.CommandLine -and $_.CommandLine.Contains($script:Case.directory) -and $_.ProcessId -ne $PID)
    } | ForEach-Object { [pscustomobject]@{ process_id = $_.ProcessId; parent_process_id = $_.ParentProcessId; executable = $_.ExecutablePath; created_at = $_.CreationDate; command_line = $_.CommandLine } })
}
function Capture-Evidence([string]$Stage) {
    $evidence = [ordered]@{
        stage = $Stage; captured_at_utc = [DateTime]::UtcNow.ToString('o'); app_id = $script:Case.app_id
        installation_directory = $script:Case.install_dir; installation_exists = (Test-Path -LiteralPath $script:Case.install_dir)
        files = @(File-Snapshot $script:Case.install_dir); registry = @(Registry-Snapshot)
        locks = @(File-Snapshot (Join-Path $script:Case.local_app_data 'Avelin\M0\locks'))
        launch_error_logs = @(File-Snapshot (Join-Path $script:Case.local_app_data 'Avelin\M0\logs'))
        sessions = @(Session-Evidence); owned_processes = @(Owned-ProcessState); scoped_live_processes = @(Scoped-LiveProcesses)
    }
    Save-Json (Join-Path $script:Case.directory "$Stage.snapshot.json") $evidence
    $script:Case.snapshots += "$Stage.snapshot.json"
    $evidence
}
function Start-OwnedProcess([string]$Executable, [string[]]$Arguments, [string]$Name) {
    Assert-True (Test-Path -LiteralPath $Executable -PathType Leaf) "Executable missing: $Executable"
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable; $info.WorkingDirectory = $script:ProjectDirectory
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $info.Environment['TEMP'] = $script:Case.temp; $info.Environment['TMP'] = $script:Case.temp
    $info.Environment['LOCALAPPDATA'] = $script:Case.local_app_data
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    $sha = File-Hash $Executable
    $null = $process.Start()
    $identity = Process-Identity $process $Executable
    Assert-True ($identity.executable -eq (Full-Path $Executable)) 'Started executable identity differs from requested executable.'
    $owned = [pscustomobject]@{
        name = $Name; process = $process; identity = $identity; sha256 = $sha; arguments = $Arguments
        stdout = $process.StandardOutput.ReadToEndAsync(); stderr = $process.StandardError.ReadToEndAsync()
        parent_process_id = $PID; observed_child = $false; result = $null
    }
    $script:OwnedProcesses += $owned
    Save-Json (Join-Path $script:Case.directory "$Name.start.json") ([ordered]@{ identity = $identity; executable_sha256 = $sha; arguments = $Arguments; parent_process_id = $PID; environment = @{ TEMP = $script:Case.temp; TMP = $script:Case.temp; LOCALAPPDATA = $script:Case.local_app_data } })
    Write-Host "[$($script:Case.id)/$Name] started PID $($identity.process_id)"
    $owned
}
function Wait-OwnedProcess([object]$Owned) {
    $timer = [Diagnostics.Stopwatch]::StartNew(); $nextProgress = 15
    while (-not $Owned.process.WaitForExit(200)) {
        if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) {
            $script:UnknownBackground = $true
            throw "Unknown background state: $($Owned.name) PID $($Owned.identity.process_id) timed out; no process was killed; remaining cases stopped."
        }
        if ($timer.Elapsed.TotalSeconds -ge $nextProgress) {
            Write-Host "[$($script:Case.id)/$($Owned.name)] waiting ($([int]$timer.Elapsed.TotalSeconds)s)"; $nextProgress += 15
        }
    }
    if (-not $Owned.observed_child) {
        Assert-True ($Owned.stdout.Wait(5000) -and $Owned.stderr.Wait(5000)) 'Process exited but captured output remains open; stop the run for inspection.'
        [IO.File]::WriteAllText((Join-Path $script:Case.directory "$($Owned.name).stdout.log"), $Owned.stdout.GetAwaiter().GetResult())
        [IO.File]::WriteAllText((Join-Path $script:Case.directory "$($Owned.name).stderr.log"), $Owned.stderr.GetAwaiter().GetResult())
    }
    $Owned.result = [ordered]@{ identity = $Owned.identity; executable_sha256 = $Owned.sha256; parent_process_id = $Owned.parent_process_id; exit_code = $Owned.process.ExitCode; observed_exit_at_utc = [DateTime]::UtcNow.ToString('o') }
    Save-Json (Join-Path $script:Case.directory "$($Owned.name).process.json") $Owned.result
    $Owned.result
}
function Observe-Backend([object]$Coordinator) {
    $matches = @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $($Coordinator.identity.process_id)" | Where-Object {
        $_.ExecutablePath -and (Full-Path $_.ExecutablePath) -eq $script:Case.backend
    })
    Assert-True ($matches.Count -eq 1) "Expected exactly one dedicated NSIS child, observed $($matches.Count). No fault injected."
    $child = [Diagnostics.Process]::GetProcessById([int]$matches[0].ProcessId)
    # GetProcessById is lazy. Retain an OS handle while this child is alive so its
    # exit code remains queryable even after the process disappears from CIM.
    $null = $child.Handle
    $identity = Process-Identity $child
    Assert-True ($identity.start_time_ticks -ge $Coordinator.identity.start_time_ticks) 'Backend predates its claimed coordinator.'
    $null = Safe-Path $identity.executable $script:Case.directory
    $owned = [pscustomobject]@{
        name = 'backend'; process = $child; identity = $identity; sha256 = (File-Hash $identity.executable)
        arguments = @(); parent_process_id = $Coordinator.identity.process_id; observed_child = $true; result = $null
    }
    $script:OwnedProcesses += $owned
    Save-Json (Join-Path $script:Case.directory 'backend.start.json') ([ordered]@{ identity = $identity; executable_sha256 = $owned.sha256; parent_process_id = $matches[0].ParentProcessId; cim_creation_date = $matches[0].CreationDate; command_line = $matches[0].CommandLine; session_directories = @((Session-Evidence).directory) })
    $owned
}
function Inject-ProcessCrash([object]$Owned, [string]$Reason) {
    # Identity is recaptured immediately before this single-process fault injection.
    # Kill(false) explicitly excludes descendants; never use this function for cleanup/cancel.
    $fresh = [Diagnostics.Process]::GetProcessById($Owned.identity.process_id)
    try {
        $identity = Process-Identity $fresh
        Assert-True ($identity.start_time_ticks -eq $Owned.identity.start_time_ticks -and $identity.executable -eq $Owned.identity.executable) 'Fault target identity changed; refusing termination.'
        Assert-True ((File-Hash $identity.executable) -ceq $Owned.sha256) 'Fault target executable hash changed; refusing termination.'
        $cim = Get-CimInstance Win32_Process -Filter "ProcessId = $($identity.process_id)"
        Assert-True ($null -ne $cim -and $cim.ParentProcessId -eq $Owned.parent_process_id) 'Fault target parent identity changed; refusing termination.'
        if ($Owned.observed_child) { $null = Safe-Path $identity.executable $script:Case.directory }
        else { Assert-True ($Owned.name -eq 'install' -and $identity.executable -eq $script:AppExecutable) 'Only the explicitly started test install coordinator may be terminated.' }
        $script:Case.injection = [ordered]@{ type = 'single-process abnormal termination'; reason = $Reason; identity = $identity; parent_process_id = $cim.ParentProcessId; executable_sha256 = $Owned.sha256; timestamp_utc = [DateTime]::UtcNow.ToString('o'); descendants_included = $false; cancellation = $false }
        Save-Json (Join-Path $script:Case.directory 'fault-injection.json') $script:Case.injection
        $fresh.Kill($false)
    }
    finally { $fresh.Dispose() }
}
function Wait-FirstWrittenFile([object]$Coordinator) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        Assert-True (-not $Coordinator.process.HasExited) 'Coordinator exited before the first-file fault point.'
        foreach ($session in @(Session-Evidence)) {
            foreach ($event in $session.events) {
                if ($event.PSObject.Properties['kind'] -and $event.kind -eq 'progress' -and $event.phase -eq 'running' -and $event.completed -ge 1) {
                    $written = @($script:ExpectedFiles | Where-Object { Test-Path -LiteralPath (Join-Path $script:Case.install_dir $_.path) -PathType Leaf })
                    Assert-True ($written.Count -ge 1) 'Written-file event has no independently observable file.'
                    foreach ($file in $written) { Assert-True ((File-Hash (Join-Path $script:Case.install_dir $file.path)) -ceq $file.sha256) 'First written fixture differs from source hash.' }
                    return
                }
            }
        }
        Start-Sleep -Milliseconds 100
    }
    $script:UnknownBackground = $true
    throw 'First written-file fault point timed out; no process killed; remaining cases stopped.'
}
function Operation-Arguments([string]$Operation, [string]$Report, [int]$Delay = 0) {
    $backend = if ($Operation -eq 'install') { $script:Case.backend } else { Join-Path $script:Case.install_dir 'uninstall.exe' }
    @('--backend', $backend, '--manifest', $script:Case.manifest, '--install-dir', $script:Case.install_dir, '--report', $Report, '--operation', $Operation, '--step-delay-ms', "$Delay")
}
function Read-Report([string]$Path) {
    Assert-True (Test-Path -LiteralPath $Path -PathType Leaf) "Required operation report missing: $Path"
    Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
}
function Invoke-SessionDiagnostics([string]$Stage, [string]$ExpectedAssessment, [string]$ManagedPath, [string]$ExpectedFileState) {
    $sessionIndex = 0
    foreach ($session in @(Session-Evidence)) {
        $sessionIndex++
        Assert-True (Test-Path -LiteralPath (Join-Path $session.directory 'session.json')) 'Diagnostic prerequisites missing: use a freshly built coordinator with persisted session identity.'
        $name = "diagnose-$Stage-$sessionIndex"
        $diagnose = Start-OwnedProcess $script:CliExecutable @('diagnose', '--session', $session.directory) $name
        $processResult = Wait-OwnedProcess $diagnose
        Assert-True ($processResult.exit_code -eq 0) "Session diagnosis failed: $name"
        $diagnostic = Get-Content -LiteralPath (Join-Path $script:Case.directory "$name.stdout.log") -Raw | ConvertFrom-Json
        Save-Json (Join-Path $script:Case.directory "$name.json") $diagnostic
        $script:Case.diagnostics += [ordered]@{ stage = $Stage; session = $session.directory; output = "$name.json"; assessment = $diagnostic.assessment; file_state_expected = $ExpectedFileState }
        Assert-True ($diagnostic.read_only -and -not $diagnostic.automatic_retry_allowed -and $diagnostic.app_id -ceq $script:Case.app_id) 'Diagnostic command has an unexpected identity or mutation/retry contract.'
        Assert-True ($diagnostic.assessment -ceq $ExpectedAssessment) "Unexpected diagnostic assessment at ${Stage}: $($diagnostic.assessment)"
        if ($ManagedPath) {
            $fileState = @($diagnostic.files | Where-Object { $_.path.Replace('\', '/') -ceq $ManagedPath.Replace('\', '/') })
            Assert-True ($fileState.Count -eq 1 -and $fileState[0].state -ceq $ExpectedFileState) "Diagnostic file observation differs from $ExpectedFileState at ${Stage}: $ManagedPath"
        }
    }
    Assert-True ($sessionIndex -ge 1) 'No execution session is available for diagnosis.'
}
function Assert-SnapshotEqual([object[]]$Before, [object[]]$After) {
    Assert-True ($Before.Count -eq $After.Count) 'File count changed during refused uninstall.'
    foreach ($file in $Before) {
        $found = @($After | Where-Object { $_.path -ceq $file.path })
        Assert-True ($found.Count -eq 1 -and $found[0].sha256 -ceq $file.sha256 -and $found[0].size -eq $file.size) "File changed during refused uninstall: $($file.path)"
    }
}
function Assert-NoCommit {
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $script:Case.install_dir '.avelin-receipt.json'))) 'Interrupted install committed a receipt.'
    Assert-True (@(Registry-Snapshot | Where-Object exists).Count -eq 0) 'Interrupted install has an uninstall registration.'
}
function Assert-Quiescent {
    $live = @(Scoped-LiveProcesses)
    $ownedLive = @($script:OwnedProcesses | Where-Object { -not $_.process.HasExited })
    if ($live.Count -or $ownedLive.Count) { $script:UnknownBackground = $true; throw 'Background execution remains live or unaccounted for; remaining cases stopped. Inspect process snapshots.' }
}

Assert-True ($PSVersionTable.PSVersion.Major -ge 7) 'Use PowerShell 7 for structured ProcessStartInfo arguments and single-process Kill(false).'
Assert-True ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'This harness requires Windows.'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
namespace M0Resilience {
    public static class NativeProcess {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool QueryFullProcessImageName(IntPtr process, int flags, StringBuilder image, ref int size);
    }
}
'@
$script:ProjectDirectory = Full-Path $ProjectRoot
if (-not $CliPath) { $CliPath = Join-Path $script:ProjectDirectory 'target\debug\avelin-cli.exe' }
if (-not $AppPath) { $AppPath = Join-Path $script:ProjectDirectory 'target\debug\avelin-app.exe' }
if (-not $BridgePath) { $BridgePath = Join-Path $script:ProjectDirectory 'target\i686-pc-windows-msvc\debug\avelin_nsis_bridge.dll' }
$CliPath = Full-Path $CliPath; $script:AppExecutable = Full-Path $AppPath
$BridgePath = Full-Path $BridgePath
foreach ($executable in @($CliPath, $script:AppExecutable, $BridgePath)) { Assert-True (Test-Path -LiteralPath $executable -PathType Leaf) "Build prerequisite missing: $executable" }
$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
$script:RunRoot = Safe-Path (Join-Path $script:ProjectDirectory "target\m0-resilience\$runId") $script:ProjectDirectory
Assert-True (-not (Test-Path -LiteralPath $script:RunRoot)) 'Refusing to reuse run directory.'
$null = New-Item -ItemType Directory -Path $script:RunRoot
$toolDirectory = Safe-Path (Join-Path $script:RunRoot 'tools') $script:RunRoot
$null = New-Item -ItemType Directory -Path $toolDirectory
$toolSnapshots = @()
foreach ($tool in @(
    @{ source = $CliPath; name = 'avelin-cli.exe' },
    @{ source = $script:AppExecutable; name = 'avelin-app.exe' },
    @{ source = $BridgePath; name = 'avelin_nsis_bridge.dll' }
)) {
    $toolSource = $tool.source
    $toolDestination = Safe-Path (Join-Path $toolDirectory $tool.name) $toolDirectory
    $sourceHash = File-Hash $toolSource
    Copy-Item -LiteralPath $toolSource -Destination $toolDestination
    $copiedHash = File-Hash $toolDestination
    Assert-True ($sourceHash -ceq $copiedHash -and $sourceHash -ceq (File-Hash $toolSource)) 'Tool changed while snapshotting; start a fresh run after the build finishes.'
    $toolSnapshots += @{ source = $toolSource; copied_executable = $toolDestination; sha256 = $copiedHash }
}
$CliPath = Join-Path $toolDirectory 'avelin-cli.exe'
$script:CliExecutable = $CliPath
$script:AppExecutable = Join-Path $toolDirectory 'avelin-app.exe'
$BridgePath = Join-Path $toolDirectory 'avelin_nsis_bridge.dll'
Save-Json (Join-Path $script:RunRoot 'tools.json') $toolSnapshots
Copy-Item -LiteralPath $PSCommandPath -Destination (Join-Path $toolDirectory 'run-m0-resilience.ps1')
$fixtureSource = Safe-Path (Join-Path $script:ProjectDirectory 'examples\nimbus\payload') $script:ProjectDirectory
$fixtureTarget = Safe-Path (Join-Path $script:RunRoot 'fixtures') $script:RunRoot
$null = New-Item -ItemType Directory -Path $fixtureTarget
$sourceFiles = @(File-Snapshot $fixtureSource)
Assert-True ($sourceFiles.Count -ge 3 -and @($sourceFiles | Where-Object path -EQ 'nimbus-demo.exe').Count -eq 1) 'Nimbus fixture prerequisites missing.'
foreach ($file in $sourceFiles) {
    $destination = Safe-Path (Join-Path $fixtureTarget $file.path) $fixtureTarget
    $null = New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force
    Copy-Item -LiteralPath (Join-Path $fixtureSource $file.path) -Destination $destination
}
$script:ExpectedFiles = @(File-Snapshot $fixtureTarget)
Assert-SnapshotEqual $sourceFiles $script:ExpectedFiles
Save-Json (Join-Path $script:RunRoot 'fixture-snapshot.json') $script:ExpectedFiles
$script:Summary = [ordered]@{
    schema_version = 1; status = 'RUNNING'; started_at_utc = [DateTime]::UtcNow.ToString('o'); run_directory = $script:RunRoot
    scope = 'Local headless resilience tests only. Passing these cases does not establish full M0, clean-machine, VM, GUI, signature, upgrade, or release acceptance.'
    harness_sha256 = (File-Hash $PSCommandPath); cli_sha256 = (File-Hash $CliPath); app_sha256 = (File-Hash $script:AppExecutable); bridge_sha256 = (File-Hash $BridgePath); tool_snapshots = $toolSnapshots
    source_fixture_directory = $fixtureSource; expected_fixture_files = $script:ExpectedFiles; cases = @(); active_case = $null; failure = $null
}
$script:UnknownBackground = $false; $script:Case = $null; $script:OwnedProcesses = @(); $exclusiveStream = $null
Write-Host "M0 resilience evidence: $script:RunRoot"
try {
    foreach ($caseId in $Cases) {
        $script:Summary.active_case = $caseId
        $caseRoot = Safe-Path (Join-Path $script:RunRoot $caseId) $script:RunRoot
        $null = New-Item -ItemType Directory -Path $caseRoot
        $appId = 'dev.avelin.m0.resilience.' + $caseId.ToLowerInvariant() + '.' + [Guid]::NewGuid().ToString('N')
        $script:Case = [ordered]@{
            id = $caseId; status = 'RUNNING'; directory = $caseRoot; app_id = $appId
            registry_subkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $appId
            install_dir = (Join-Path $caseRoot 'install'); temp = (Join-Path $caseRoot 'temp'); local_app_data = (Join-Path $caseRoot 'local-app-data')
            backend = (Join-Path $caseRoot 'Setup.backend.exe'); manifest = (Join-Path $caseRoot 'Setup.manifest.json')
            snapshots = @(); diagnostics = @(); injection = $null; assertions = @(); retained_scene_reason = $null
        }
        $script:OwnedProcesses = @()
        foreach ($directory in @($script:Case.install_dir, $script:Case.temp, $script:Case.local_app_data)) { $null = New-Item -ItemType Directory -Path (Safe-Path $directory $caseRoot) }
        $before = Capture-Evidence 'before'
        Assert-True ($before.files.Count -eq 0 -and @($before.registry | Where-Object exists).Count -eq 0) 'Case target/registration is not fresh.'
        $config = Join-Path $caseRoot 'avelin.toml'; $setup = Join-Path $caseRoot 'Setup.exe'
        $configText = @"
schema_version = 1
[app]
id = "$appId"
name = "Avelin M0 Resilience $caseId"
version = "0.1.0"
entry = "nimbus-demo.exe"
[payload]
directory = "../fixtures"
[install]
directory_name = "AvelinM0Resilience"
start_menu_shortcut = false
"@
        [IO.File]::WriteAllText($config, $configText, [Text.UTF8Encoding]::new($false))
        $build = Start-OwnedProcess $CliPath @('build', '--config', $config, '--output', $setup, '--runtime', $script:AppExecutable, '--bridge', $BridgePath) 'build'
        $buildResult = Wait-OwnedProcess $build
        Assert-True ($buildResult.exit_code -eq 0) 'Fixture package build failed.'
        $manifest = Read-Report $script:Case.manifest
        Assert-True ($manifest.app_id -ceq $appId -and $manifest.start_menu_shortcut -eq $false) 'Built manifest identity/options differ from test configuration.'
        $script:Case['artifacts'] = @(@($setup, $script:Case.backend, $script:Case.manifest) | ForEach-Object { @{ path = $_; sha256 = (File-Hash $_) } })
        $installReportPath = Join-Path $caseRoot 'install.report.json'
        # The existing public step delay provides enough time to identify and
        # retain a handle to every setup backend, including R03/R04 prerequisites.
        $delay = 5000
        $coordinator = Start-OwnedProcess $script:AppExecutable (Operation-Arguments 'install' $installReportPath $delay) 'install'
        if ($caseId -in @('R01', 'R02')) {
            Wait-FirstWrittenFile $coordinator
            $backend = Observe-Backend $coordinator
            $atFault = Capture-Evidence 'before-fault'
            Assert-True ($atFault.files.Count -ge 1 -and $atFault.locks.Count -eq 1) 'Fault requires independently observed written files and one product lock.'
            $target = if ($caseId -eq 'R01') { $coordinator } else { $backend }
            Inject-ProcessCrash $target $caseId
            # Always observe BOTH participants before asserting outcomes or starting another case.
            $backendResult = Wait-OwnedProcess $backend
            $coordinatorResult = Wait-OwnedProcess $coordinator
            Assert-Quiescent
            $after = Capture-Evidence 'after-fault'
            Assert-NoCommit
            if ($caseId -eq 'R01') {
                Assert-True ($backendResult.exit_code -eq 4) 'Disconnected backend did not exit itself with Unknown/4.'
                Assert-True ($after.files.Count -eq 0) 'Disconnected backend left known installation files.'
                Assert-True ($after.locks.Count -eq 1 -and $after.locks[0].path -ceq $atFault.locks[0].path -and $after.locks[0].sha256) 'Coordinator crash did not preserve an identifiable interrupted-task product lock.'
                $retainedLockPath = Join-Path (Join-Path $script:Case.local_app_data 'Avelin\M0\locks') $after.locks[0].path
                Assert-True ([IO.File]::ReadAllText($retainedLockPath).Trim() -ceq [string]$coordinator.identity.process_id) 'Retained product lock does not identify the crashed coordinator.'
                if ($atFault.locks[0].sha256) { Assert-True ($after.locks[0].sha256 -ceq $atFault.locks[0].sha256) 'Readable product lock contents changed across the coordinator crash.' }
                else {
                    Assert-True (-not [string]::IsNullOrWhiteSpace($atFault.locks[0].read_error)) 'Before-fault lock has neither a hash nor an explicit read error.'
                    $script:Case['lock_observation_note'] = 'The active coordinator held the lock against reads. The before snapshot records that error; after exit the retained lock hash and exact coordinator PID content are verified.'
                }
                if (Test-Path -LiteralPath $installReportPath) { Assert-True ((Read-Report $installReportPath).result -ne 'Succeeded') 'Crashed coordinator left a success report.' }
                $script:Case.assertions = @('First fixture hash and progress event observed before crash.', 'Only the identified coordinator was terminated.', 'NSIS exited itself with 4 and cleaned all known files.', 'No receipt or registration; interrupted-task lock retained.')
                $script:Case.retained_scene_reason = 'Stale product lock and session journal retained for explicit recovery inspection; no cleanup was claimed from coordinator exit.'
            }
            else {
                $report = Read-Report $installReportPath
                Assert-True ($report.result -ceq 'Unknown' -and $report.coordinator_exit_code -eq 4 -and $coordinatorResult.exit_code -eq 4) 'Killed backend was not exposed as Unknown/4 by the coordinator.'
                Assert-True ($report.backend_pid -eq $backend.identity.process_id -and $report.exit_code -ne 0 -and @($report.events).Count -ge 1) 'Backend failure report lacks matching process identity/events/nonzero exit.'
                Assert-True ($after.files.Count -ge 1 -and $after.sessions.Count -ge 1) 'Backend-crash diagnostic partial-write scene is missing.'
                $script:Case['report'] = $report
                $script:Case.assertions = @('First fixture hash and progress event observed before crash.', 'Only the identified NSIS child was terminated.', 'Coordinator returned Unknown/4 with matching backend PID and execution events.', 'No committed receipt/registration; partial files and diagnostic session retained.')
                $script:Case.retained_scene_reason = 'Backend interruption leaves an explicitly Unknown partial installation; retain exact files, journal, and report.'
            }
            Invoke-SessionDiagnostics 'after-fault' 'NeedsReview' '' ''
            $afterDiagnosis = Capture-Evidence 'after-diagnosis'
            Assert-SnapshotEqual @($after.files) @($afterDiagnosis.files)
            Assert-SnapshotEqual @($after.locks) @($afterDiagnosis.locks)
        }
        else {
            Wait-FirstWrittenFile $coordinator
            $backend = Observe-Backend $coordinator
            $setupBackendResult = Wait-OwnedProcess $backend
            $installedResult = Wait-OwnedProcess $coordinator
            $installReport = Read-Report $installReportPath
            Assert-True ($setupBackendResult.exit_code -eq 0 -and $installedResult.exit_code -eq 0 -and $installReport.result -ceq 'Succeeded') 'Setup prerequisite did not succeed.'
            $installed = Capture-Evidence 'installed'
            $receipt = Read-Report (Join-Path $script:Case.install_dir '.avelin-receipt.json')
            Assert-True ($receipt.app_id -ceq $appId -and @($installed.registry | Where-Object exists).Count -ge 1) 'Successful setup has no matching receipt/registration.'
            foreach ($fixture in $script:ExpectedFiles) { Assert-True ((File-Hash (Join-Path $script:Case.install_dir $fixture.path)) -ceq $fixture.sha256) "Installed fixture hash mismatch: $($fixture.path)" }
            Invoke-SessionDiagnostics 'installed' 'RecordedStateMatchesObservedResources' '' ''
            foreach ($relative in @('user-kept.txt', 'assets\user-created.txt')) {
                $sentinel = Safe-Path (Join-Path $script:Case.install_dir $relative) $script:Case.install_dir
                Assert-True (-not (Test-Path -LiteralPath $sentinel)) 'Sentinel path unexpectedly exists.'
                $null = New-Item -ItemType Directory -Path (Split-Path -Parent $sentinel) -Force
                [IO.File]::WriteAllText($sentinel, "Unmanaged resilience sentinel $runId / $caseId / $relative")
            }
            $managed = @($script:ExpectedFiles | Where-Object { $_.path -like '*.txt' })[0]
            $managedPath = Safe-Path (Join-Path $script:Case.install_dir $managed.path) $script:Case.install_dir
            if ($caseId -eq 'R03') {
                [IO.File]::AppendAllText($managedPath, "`nUser changed this managed test fixture: $runId")
                Assert-True ((File-Hash $managedPath) -cne $managed.sha256) 'Managed-file modification was not effective.'
            }
            $beforeRefusal = Capture-Evidence 'before-refusal'
            if ($caseId -eq 'R04') {
                $exclusiveStream = [IO.FileStream]::new($managedPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::None)
                $script:Case.injection = [ordered]@{ type = 'exclusive managed-file handle'; path = $managedPath; share = 'None'; owner = (Process-Identity ([Diagnostics.Process]::GetCurrentProcess())); original_sha256 = $managed.sha256; opened_at_utc = [DateTime]::UtcNow.ToString('o') }
                Save-Json (Join-Path $caseRoot 'fault-injection.json') $script:Case.injection
                $null = Capture-Evidence 'exclusive-handle-held'
            }
            $uninstallReportPath = Join-Path $caseRoot 'uninstall.report.json'
            $uninstall = Start-OwnedProcess $script:AppExecutable (Operation-Arguments 'uninstall' $uninstallReportPath) 'uninstall'
            $uninstallResult = Wait-OwnedProcess $uninstall
            $heldEvidence = Capture-Evidence 'after-refusal-handle-state'
            $expectedDiagnosticFileState = if ($caseId -eq 'R03') { 'Changed' } else { 'Unconfirmed' }
            Invoke-SessionDiagnostics 'after-refusal' 'NeedsReview' $managed.path $expectedDiagnosticFileState
            if ($exclusiveStream) { $exclusiveStream.Dispose(); $exclusiveStream = $null; $script:Case.injection['released_at_utc'] = [DateTime]::UtcNow.ToString('o'); Save-Json (Join-Path $caseRoot 'fault-injection.json') $script:Case.injection }
            Assert-Quiescent
            $after = Capture-Evidence 'after-refusal'
            Assert-True ($uninstallResult.exit_code -ne 0) 'Refused uninstall exited successfully.'
            $stderr = Get-Content -LiteralPath (Join-Path $caseRoot 'uninstall.stderr.log') -Raw
            Assert-True (-not [string]::IsNullOrWhiteSpace($stderr) -or (Test-Path -LiteralPath $uninstallReportPath)) 'Refused uninstall has no diagnostic evidence.'
            if (Test-Path -LiteralPath $uninstallReportPath) {
                $report = Read-Report $uninstallReportPath
                Assert-True ($report.result -in @('Failed', 'Unknown') -and $report.coordinator_exit_code -ne 0) 'Uninstall failure has an incorrect operation result.'
                $script:Case['report'] = $report
            }
            $newSessions = @($after.sessions | Where-Object { $_.directory -notin @($beforeRefusal.sessions.directory) })
            $script:Case['failure_stage'] = if ($newSessions.Count -eq 0) { 'coordinator preflight refusal; NSIS deletion path was not executed' } else { 'backend execution started; inspect events for deletion failure stage' }
            Assert-SnapshotEqual @($beforeRefusal.files) @($after.files)
            Assert-True (($beforeRefusal.registry | ConvertTo-Json -Depth 8 -Compress) -ceq ($after.registry | ConvertTo-Json -Depth 8 -Compress)) 'Refused uninstall modified registration.'
            if ($caseId -eq 'R03') { Assert-True ($stderr.Contains('managed file changed or is missing')) 'Modified managed file was refused for an unexpected reason.' }
            else { Assert-True (@($heldEvidence.files | Where-Object { $_.path -ceq $managed.path -and $_.read_error }).Count -eq 1) 'Exclusive handle did not prevent managed-file reads during uninstall.' }
            if ($caseId -eq 'R04') {
                Invoke-SessionDiagnostics 'handle-released' 'RecordedStateMatchesObservedResources' $managed.path 'Matches'
                $script:Case['post_release_diagnostic_note'] = 'After release, the original successful INSTALL session again matches observed resources. This does not turn the refused uninstall into a success; its process exit and failure log remain nonzero/error.'
            }
            $script:Case.assertions = @('Successful fixture setup independently verified by hashes, receipt and registration.', 'Unknown root and nested user files retain hashes.', 'Uninstall returned nonzero with diagnostic evidence and no success result.', 'Every file and registry value survived refused uninstall unchanged.')
            $script:Case.retained_scene_reason = 'Expected uninstall refusal: preserve managed/unknown files, receipt, registration, and diagnostics. Exclusive test handle is released after evidence capture.'
        }
        $script:Case.status = 'PASSED'
        $script:Case['processes'] = @(Owned-ProcessState)
        Save-Json (Join-Path $caseRoot 'evidence.json') $script:Case
        $script:Summary.cases += $script:Case
        Save-Json (Join-Path $script:RunRoot 'summary.json') $script:Summary
        Write-Host "[$caseId] PASSED; scene retained: $($script:Case.retained_scene_reason)"
        foreach ($owned in $script:OwnedProcesses) { $owned.process.Dispose() }
        $script:OwnedProcesses = @()
    }
    $script:Summary.status = 'PASSED'; $script:Summary.active_case = $null
}
catch {
    $script:Summary.status = 'FAILED'; $script:Summary.failure = $_.Exception.Message
    if ($script:Case) {
        $script:Case.status = 'FAILED'; $script:Case['failure'] = $_.Exception.Message
        try {
            $failureEvidence = Capture-Evidence 'failure'
            if ($failureEvidence.scoped_live_processes.Count -or @($failureEvidence.owned_processes | Where-Object { -not $_.exited }).Count) { $script:UnknownBackground = $true }
        }
        catch { $script:Case['snapshot_error'] = $_.Exception.Message; $script:UnknownBackground = $true }
        Save-Json (Join-Path $script:Case.directory 'evidence.json') $script:Case
        if (@($script:Summary.cases | Where-Object { $_.id -eq $script:Case.id }).Count -eq 0) { $script:Summary.cases += $script:Case }
    }
    throw
}
finally {
    if ($exclusiveStream) { $exclusiveStream.Dispose() }
    $script:Summary['unknown_background_state'] = $script:UnknownBackground
    $script:Summary['finished_at_utc'] = [DateTime]::UtcNow.ToString('o')
    Save-Json (Join-Path $script:RunRoot 'summary.json') $script:Summary
    Write-Host "Evidence retained: $(Join-Path $script:RunRoot 'summary.json')"
}
