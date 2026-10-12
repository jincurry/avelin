#requires -Version 7.2
# Launch and observe one real GUI operation. This script does not drive the UI or declare visual acceptance.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$FixturePath,
    [Parameter(Mandatory)][ValidateSet('Install', 'Uninstall')][string]$Operation,
    [ValidateRange(30, 3600)][int]$TimeoutSeconds = 1200
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
function Require([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Full([string]$Path) { [IO.Path]::GetFullPath($Path).TrimEnd([char]'\', [char]'/') }
function SamePath([string]$Left, [string]$Right) { [string]::Equals((Full $Left), (Full $Right), [StringComparison]::OrdinalIgnoreCase) }
function ReadJson([string]$Path) { [IO.File]::ReadAllText($Path) | ConvertFrom-Json }
function Save([string]$Name, [object]$Value) { [IO.File]::WriteAllText((Join-Path $script:EvidenceRoot $Name), ($Value | ConvertTo-Json -Depth 30), [Text.UTF8Encoding]::new($false)) }

$FixturePath = Full $FixturePath
$fixture = ReadJson $FixturePath
$runRoot = Full (Split-Path -Parent $FixturePath)
foreach ($path in @($fixture.setup, $fixture.install_dir, $fixture.temp, $fixture.local_app_data, $fixture.manifest)) {
    Require ((Full $path).StartsWith($runRoot + '\', [StringComparison]::OrdinalIgnoreCase)) 'Fixture path escaped its run directory'
    for ($cursor = Full $path; $cursor; $cursor = Split-Path -Parent $cursor) {
        if (Test-Path -LiteralPath $cursor) { Require (((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) 'Reparse point in fixture' }
    }
}
Require ((Hash $fixture.setup) -ceq $fixture.setup_sha256) 'Setup changed since automatic acceptance'
$manifest = ReadJson $fixture.manifest
Require ($manifest.app_id -ceq $fixture.app_id) 'Fixture app identity mismatch'
$script:EvidenceRoot = Join-Path $runRoot ('gui-' + $Operation.ToLowerInvariant() + '-' + [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ'))
Require (-not (Test-Path -LiteralPath $script:EvidenceRoot)) 'Evidence directory already exists'
$null = New-Item -ItemType Directory -Path $script:EvidenceRoot
$report = Join-Path $script:EvidenceRoot 'report.json'
$info = [Diagnostics.ProcessStartInfo]::new()
$info.UseShellExecute = $false
$info.CreateNoWindow = $true
# This is the explicitly requested visible installer window, not a background service.
$info.WindowStyle = [Diagnostics.ProcessWindowStyle]::Normal
$info.WorkingDirectory = $runRoot
$info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
$info.StandardOutputEncoding = [Text.UTF8Encoding]::new($false, $true); $info.StandardErrorEncoding = $info.StandardOutputEncoding
$info.Environment['TEMP'] = $fixture.temp; $info.Environment['TMP'] = $fixture.temp; $info.Environment['LOCALAPPDATA'] = $fixture.local_app_data
if ($Operation -eq 'Install') {
    Require (-not (Test-Path -LiteralPath (Join-Path $fixture.install_dir '.avelin-receipt.json'))) 'Fixture already installed'
    foreach ($path in $fixture.shortcut_paths) { Require (-not (Test-Path -LiteralPath $path)) 'Fixture shortcut collision' }
    $info.FileName = $fixture.setup
    foreach ($argument in @('--gui', '--install-dir', $fixture.install_dir, '--report', $report)) { $info.ArgumentList.Add($argument) }
} else {
    $resident = Join-Path $fixture.install_dir '.avelin\avelin-app.exe'
    $backend = Join-Path $fixture.install_dir 'uninstall.exe'
    $installedManifest = Join-Path $fixture.install_dir '.avelin\manifest.json'
    $receipt = ReadJson (Join-Path $fixture.install_dir '.avelin-receipt.json')
    Require ($receipt.app_id -ceq $fixture.app_id -and (SamePath $receipt.install_dir $fixture.install_dir) -and (Hash $resident) -ceq $manifest.runtime_sha256 -and (Hash $backend) -ceq $receipt.uninstaller_sha256) 'Uninstall component or ownership mismatch'
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, [Microsoft.Win32.RegistryView]::Registry64)
    $key = $null
    try {
        $key = $base.OpenSubKey('Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $fixture.app_id, $false)
        Require ($null -ne $key) 'Normal uninstall registration missing'
        $registered = [string]$key.GetValue('UninstallString')
    } finally { if ($key) { $key.Dispose() }; $base.Dispose() }
    $expected = '"{0}" --gui --operation uninstall --backend "{1}" --manifest "{2}" --install-dir "{3}"' -f $resident, $backend, $installedManifest, $fixture.install_dir
    Require ($registered -ceq $expected) 'Registered normal command does not match the fixture'
    $info.FileName = $resident
    $info.Arguments = $registered.Substring(('"' + $resident + '"').Length).TrimStart() + ' --report "' + $report + '"'
    Save 'registered-entry.json' @{ command = $registered; source = 'actual HKCU Registry64 UninstallString'; receipt = $receipt }
}
$process = [Diagnostics.Process]::new(); $process.StartInfo = $info
$observedCoordinator = $null
$state = [ordered]@{ operation = $Operation; fixture = $FixturePath; executable = $info.FileName; executable_sha256 = Hash $info.FileName; arguments = @($info.ArgumentList); raw_arguments = $info.Arguments; started_at_utc = [DateTime]::UtcNow.ToString('o'); report = $report; status = 'RUNNING'; visual_acceptance = 'NOT_ASSERTED_BY_SCRIPT' }
try {
    $null = $process.Start(); $state.launcher_pid = $process.Id
    $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync()
    Save 'process.json' $state
    Write-Host ($state | ConvertTo-Json -Depth 10)
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while (-not ($process.WaitForExit(250) -and $stdout.IsCompleted -and $stderr.IsCompleted)) {
        if ($null -eq $observedCoordinator -and (Test-Path -LiteralPath $report)) {
            # Bind the actual task process while its completion window is still open.
            # This also observes the copied resident-uninstall frontend, not only its launcher.
            $currentReport = $null
            try { $currentReport = ReadJson $report } catch { }
            if ($currentReport -and $currentReport.result -in @('Succeeded', 'Failed', 'Cancelled', 'Unknown')) {
                $identity = (ReadJson (Join-Path $currentReport.session_dir 'session.json')).coordinator
                Require ((Full $identity.executable).StartsWith($runRoot + '\', [StringComparison]::OrdinalIgnoreCase)) 'Coordinator executable escaped fixture'
                $observedCoordinator = [Diagnostics.Process]::GetProcessById([int]$identity.pid)
                $null = $observedCoordinator.Handle
                Require ($observedCoordinator.StartTime.ToUniversalTime().ToFileTimeUtc() -eq [long]$identity.creation_time_100ns -and (SamePath $observedCoordinator.MainModule.FileName $identity.executable)) 'Coordinator process identity mismatch'
                $state.coordinator = $identity
                Save 'process.json' $state
            }
        }
        if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) { $state.status = 'TIMED_OUT_PROCESS_NOT_KILLED'; throw 'GUI observation timed out; no process was killed; inspect current state before continuing' }
        if ($process.HasExited) { Start-Sleep -Milliseconds 100 }
    }
    $state.launcher_exit_code = $process.ExitCode
    Require ($null -ne $observedCoordinator) 'Task process was not observed while active; lifecycle evidence is incomplete'
    Require ($observedCoordinator.WaitForExit(5000)) 'Task coordinator remains active after its streams ended'
    $state.coordinator_exit_code = $observedCoordinator.ExitCode
    Require ($observedCoordinator.ExitCode -eq 0) 'Task coordinator exited with a GUI/runtime failure'
    [IO.File]::WriteAllText((Join-Path $script:EvidenceRoot 'stdout.log'), $stdout.GetAwaiter().GetResult(), [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText((Join-Path $script:EvidenceRoot 'stderr.log'), $stderr.GetAwaiter().GetResult(), [Text.UTF8Encoding]::new($false))
    Require ($process.ExitCode -eq 0) 'GUI launcher failed; inspect logs'
    Require (Test-Path -LiteralPath $report) 'GUI closed without a task report; this is not an accepted operation'
    $result = ReadJson $report
    Require ($result.result -ceq 'Succeeded' -and $result.coordinator_exit_code -eq 0 -and $result.exit_code -eq 0 -and -not $result.product_lock_retained -and (SamePath $result.install_dir $fixture.install_dir)) 'GUI task did not confirm success'
    $state.status = 'TASK_PASSED_VISUAL_CHECK_SEPARATE'
    $state.report_sha256 = Hash $report
    $state.session = $result.session_dir
} catch { if ($state.status -eq 'RUNNING') { $state.status = 'FAILED' }; $state.error = $_.Exception.Message; throw }
finally { $state.finished_at_utc = [DateTime]::UtcNow.ToString('o'); Save 'process.json' $state; if ($observedCoordinator) { $observedCoordinator.Dispose() }; $process.Dispose(); Write-Host "GUI evidence: $script:EvidenceRoot" }
