#requires -Version 7.2
<# Real-process Rust recovery and repair acceptance in new, isolated fixtures.
   Only fixture-owned files, shortcut records and UUID registration keys are edited.
   Intentional interruption uses the same PID/creation-time/executable-bound process
   handles as the existing interruption harness. No recursive or process-tree cleanup. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Runtime,
    [Parameter(Mandatory)][string]$Cli,
    [Parameter(Mandatory)][string]$RustWorker,
    [Parameter(Mandatory)][string]$Sample,
    [Parameter(Mandatory)][string]$OutputRoot,
    [ValidateRange(30, 1800)][int]$TimeoutSeconds = 180,
    # Internal GUI fixture preparation: initialize helpers and an isolated root,
    # but do not run or claim acceptance cases.
    [switch]$FunctionsOnly
)
$initializeOnly = [bool]$FunctionsOnly
. (Join-Path $PSScriptRoot 'run-rust-engine-interruption.ps1') -Runtime $Runtime -Cli $Cli -RustWorker $RustWorker -Sample $Sample -OutputRoot $OutputRoot -TimeoutSeconds $TimeoutSeconds -FunctionsOnly
. (Join-Path $PSScriptRoot 'windows-shell-link.ps1')
Require ($IsWindows -and [Environment]::Is64BitProcess) 'Use Windows x64 PowerShell 7.2+'
$projectRoot = Full (Join-Path $PSScriptRoot '../..')
$script:RunRoot = Inside $OutputRoot (Join-Path $projectRoot 'target')
Require (-not (Test-Path -LiteralPath $script:RunRoot)) 'Maintenance OutputRoot must be new'
$null = [IO.Directory]::CreateDirectory($script:RunRoot)
$script:RuntimeBinary = Full $Runtime; $script:CliBinary = Full $Cli; $script:WorkerBinary = Full $RustWorker; $script:SampleBinary = Full $Sample
$script:UserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$summary = [ordered]@{ schema_version = 1; status = 'RUNNING'; cases = @(); components = @(); started_at_utc = [DateTime]::UtcNow.ToString('o'); limitations = @('Explicit transaction checkpoints only', 'Cross-user denial uses a mismatched retained-session SID, not a second interactive account', 'No reboot, signing, performance or GUI qualification') }
foreach ($binary in @($script:RuntimeBinary, $script:CliBinary, $script:WorkerBinary, $script:SampleBinary)) { $summary.components += @{ path = $binary; sha256 = Hash $binary } }
function Pass([string]$Name) {
    $summary.cases += @{ label = $Name; status = 'PASSED' }
    Save (Join-Path $script:RunRoot 'summary.json') $summary
}
function NewCase([string]$Label) {
    $script:CaseRoot = Inside (Join-Path $script:RunRoot $Label) $script:RunRoot
    $script:LocalRoot = Join-Path $script:CaseRoot 'local-data'; $script:TempRoot = Join-Path $script:CaseRoot 'temp'; $script:InstallRoot = Join-Path $script:CaseRoot 'installed 中文 🦀 العربية'
    foreach ($directory in @($script:CaseRoot, $script:LocalRoot, $script:TempRoot, $script:InstallRoot)) { $null = [IO.Directory]::CreateDirectory($directory) }
    $script:AppId = 'dev.avelin.maintenance.' + [Guid]::NewGuid().ToString('N')
    $script:RegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:AppId
    Require (-not (RegistrySnapshot).exists) 'Unique maintenance registration already exists'
    WriteNew (Join-Path $script:InstallRoot 'user-sentinel.txt') 'Unmanaged content preserved across maintenance'
    $script:SentinelHash = Hash (Join-Path $script:InstallRoot 'user-sentinel.txt')
}
function Operation([object]$Package, [string]$Operation, [string]$Label, [string[]]$Extra = @(), [int]$ExpectedExit = 0) {
    $report = Join-Path $script:CaseRoot "$Label.report.json"
    $executable = $Package.Setup
    if ($Operation -ceq 'uninstall') { $executable = $script:RuntimeBinary; $Extra += @(UninstallArguments) }
    InvokeFixtureProcess $executable (@('--silent', '--operation', $Operation, '--install-dir', $script:InstallRoot, '--report', $report) + $Extra) $Label -ExpectedExit $ExpectedExit -AwaitReport $(if ($ExpectedExit -eq 0) { $report } else { '' })
    if (Test-Path -LiteralPath $report) { return Json $report }
    return $null
}
function UninstallArguments {
    $backend = Inside (Join-Path $script:InstallRoot 'uninstall.exe') $script:InstallRoot
    $manifest = Inside (Join-Path $script:InstallRoot '.avelin/manifest.json') $script:InstallRoot
    $receipt = Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    Require ($receipt.app_id -ceq $script:AppId -and (Hash $backend) -ceq $receipt.uninstaller_sha256) 'Installed uninstall worker ownership differs'
    @('--backend', $backend, '--manifest', $manifest)
}
function WaitMaintenancePause([object]$Running, [object]$Package, [string]$Operation, [string]$Point) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        if ($Running.Process.HasExited) {
            $exitCode = $Running.Process.ExitCode
            FinishFixtureProcess $Running $exitCode
            throw "Maintenance coordinator exited $exitCode before pause"
        }
        foreach ($directory in Get-ChildItem -LiteralPath $script:CaseRoot -Directory -Filter 'session-*') {
            $session = Inside $directory.FullName $script:RunRoot
            $pausePath = Join-Path $session 'engine-pause.json'
            if (-not (Test-Path -LiteralPath $pausePath)) { continue }
            try { $pause = Json $pausePath; $identity = Json (Join-Path $session 'session.json'); $plan = Json (Join-Path $session 'install-plan.json') } catch { continue }
            if ($identity.coordinator.pid -ne $Running.Process.Id) { continue }
            Require ($pause.task_id -ceq $identity.task_id -and $plan.task_id -ceq $identity.task_id -and $pause.point -ceq $Point -and $pause.backend_pid -eq $identity.backend.pid) 'Maintenance pause identity differs'
            Require ($identity.operation -ceq $Operation -and $plan.operation -ceq $Operation -and $identity.app_id -ceq $script:AppId -and (SamePath $identity.install_dir $script:InstallRoot)) 'Maintenance pause product/operation differs'
            Require ((SamePath $identity.coordinator.executable $Running.Record.executable) -and (Hash $identity.backend.executable) -ceq (Hash $Package.Backend)) 'Maintenance pause executable differs'
            $deadline = (Get-Item -LiteralPath $pausePath).LastWriteTimeUtc.AddSeconds(27)
            Require ([DateTime]::UtcNow -lt $deadline) 'Maintenance pause expired; no process was killed'
            return @{ Session = $session; Identity = $identity; Deadline = $deadline }
        }
        Start-Sleep -Milliseconds 20
    }
    throw 'Maintenance pause timed out; no process was killed'
}
function StopPausedBackend([object]$Running, [object]$Evidence) {
    $backend = $null; $coordinator = $null
    try {
        $backend = [AvelinInterruption.VerifiedProcess]::new([uint32]$Evidence.Identity.backend.pid, [uint64]$Evidence.Identity.backend.creation_time_100ns, $Evidence.Identity.backend.executable)
        $coordinator = [AvelinInterruption.VerifiedProcess]::new([uint32]$Evidence.Identity.coordinator.pid, [uint64]$Evidence.Identity.coordinator.creation_time_100ns, $Evidence.Identity.coordinator.executable)
        Require ([DateTime]::UtcNow -lt $Evidence.Deadline) 'Maintenance pause expired before verified termination'
        $backend.Terminate()
        Require ($backend.Wait(10000) -and $backend.ExitCode -eq 137 -and $coordinator.Wait(30000)) 'Interrupted maintenance processes did not stop'
        Save (Join-Path $script:CaseRoot "$($Running.Label).termination.json") @{ backend_pid = $Evidence.Identity.backend.pid; backend_creation = $Evidence.Identity.backend.creation_time_100ns; backend_exit = $backend.ExitCode; coordinator_exit = $coordinator.ExitCode; task_id = $Evidence.Identity.task_id }
    } finally { if ($backend) { $backend.Dispose() }; if ($coordinator) { $coordinator.Dispose() } }
    FinishFixtureProcess $Running 4
}
function Recover([object]$Package, [string]$OriginalSession, [string]$Label, [switch]$Apply, [int]$ExpectedExit = 0) {
    $extra = @('--recovery-session', $OriginalSession)
    if ($Apply) { $extra += '--recovery-apply' }
    $task = Operation $Package 'recover' $Label $extra $ExpectedExit
    if ($ExpectedExit -ne 0) { return $null }
    Require ($task.result -ceq 'Succeeded' -and $task.coordinator_exit_code -eq 0) 'Recovery coordinator did not confirm success'
    $session = Inside $task.session_dir $script:RunRoot
    $report = Json (Join-Path $session 'recovery-report.json')
    Require ($report.verified -eq $true -and $report.applied -eq [bool]$Apply -and $report.original_task_id) 'Recovery report does not confirm requested check/apply mode'
    return $report
}
function AssertUnchangedState([object]$Before, [string]$Session, [object[]]$SessionBefore) {
    Require ((SameSnapshot $Before (ProductSnapshot)) -and (SameSnapshot $SessionBefore @(Snapshot $Session))) 'Rejected or read-only recovery changed original resources/evidence'
}
function LiveIdentitySnapshot([string]$Session) {
    # The live coordinator exclusively owns its event log. Compare immutable
    # identity/plan/recovery records without opening the active log writer.
    @(foreach ($name in @('session.json', 'install-plan.json', 'manifest.json', 'engine-recovery.json', 'engine-pause.json')) {
        $path = Inside (Join-Path $Session $name) $Session
        @{ name = $name; sha256 = Hash $path }
    })
}
function RecoveryCases([string]$OperationName, [string]$Point) {
    $label = "recover-$OperationName-$Point"; NewCase $label
    $a = BuildPackage 'a' '1.0.0'; $b = BuildPackage 'b' '1.1.0'
    if ($OperationName -ne 'install') { $installed = Operation $a 'install' 'install-a'; VerifyVersion $a $installed.task_id }
    $baseline = ProductSnapshot
    $active = if ($OperationName -eq 'upgrade') { $b } else { $a }
    $reportPath = Join-Path $script:CaseRoot 'interrupted.report.json'
    $executable = $active.Setup
    $arguments = @('--silent', '--operation', $OperationName, '--install-dir', $script:InstallRoot, '--report', $reportPath, '--engine-pause', $Point)
    if ($OperationName -ceq 'uninstall') { $executable = $script:RuntimeBinary; $arguments += @(UninstallArguments) }
    $running = StartFixtureProcess $executable $arguments 'interrupted'
    $evidence = WaitMaintenancePause $running $active $OperationName $Point
    # Refusal while either authenticated process remains live must not touch it.
    $beforeLive = ProductSnapshot; $sessionLive = @(LiveIdentitySnapshot $evidence.Session)
    $null = Recover $active $evidence.Session 'reject-active' -Apply -ExpectedExit 3
    Require ((SameSnapshot $beforeLive (ProductSnapshot)) -and (SameSnapshot $sessionLive @(LiveIdentitySnapshot $evidence.Session))) 'Active recovery refusal changed original resources/identity'
    StopPausedBackend $running $evidence
    $before = ProductSnapshot; $sessionBefore = @(Snapshot $evidence.Session)
    $check = Recover $active $evidence.Session 'check'
    AssertUnchangedState $before $evidence.Session $sessionBefore
    Require ($check.decision -ceq $(if ($Point -eq 'after-commit') { 'confirm_committed' } else { 'rollback' })) 'Recovery selected the wrong transaction outcome'
    if ($OperationName -eq 'upgrade' -and $Point -eq 'after-first-publish') {
        InvokeFixtureProcess $script:CliBinary @('recover', '--session', $evidence.Session, '--setup', $active.Setup, '--check') 'cli-recovery-check'
        AssertUnchangedState $before $evidence.Session $sessionBefore
        Pass 'recover-cli-check-read-only'
        $identityPath = Join-Path $evidence.Session 'install-plan.json'; $identityBytes = [IO.File]::ReadAllBytes($identityPath)
        $identity = Json $identityPath; $identity.user_sid = 'S-1-5-21-1-2-3-1001'
        Save $identityPath $identity
        $changedSession = @(Snapshot $evidence.Session)
        $null = Recover $active $evidence.Session 'reject-foreign-user' -Apply -ExpectedExit 3
        AssertUnchangedState $before $evidence.Session $changedSession
        [IO.File]::WriteAllBytes($identityPath, $identityBytes)
        Pass 'recover-reject-foreign-user'
        $manifestPath = Inside (Join-Path $evidence.Session 'engine-recovery.json') $evidence.Session
        $manifestBytes = [IO.File]::ReadAllBytes($manifestPath)
        $tampered = Json $manifestPath
        Require (@($tampered.stage).Count -gt 0) 'Recovery tamper fixture needs a staged resource'
        $tampered.stage[0].resource.sha256 = '0' * 64
        Save $manifestPath $tampered
        $changedSession = @(Snapshot $evidence.Session)
        $null = Recover $active $evidence.Session 'reject-tampered-evidence' -Apply -ExpectedExit 3
        AssertUnchangedState $before $evidence.Session $changedSession
        [IO.File]::WriteAllBytes($manifestPath, $manifestBytes)
        Pass 'recover-reject-tampered-evidence'
        $publishedPath = Join-Path $script:InstallRoot '00-version.txt'; $publishedBytes = [IO.File]::ReadAllBytes($publishedPath)
        [IO.File]::AppendAllText($publishedPath, 'User edit after interruption', $script:Utf8)
        $edited = ProductSnapshot; $sessionBefore = @(Snapshot $evidence.Session)
        $null = Recover $active $evidence.Session 'reject-post-interruption-edit' -Apply -ExpectedExit 3
        AssertUnchangedState $edited $evidence.Session $sessionBefore
        [IO.File]::WriteAllBytes($publishedPath, $publishedBytes)
        Pass 'recover-preserves-post-interruption-edit'
        $repairBefore = ProductSnapshot
        $null = Operation $a 'repair' 'repair-reject-pending' @() 3
        Require (SameSnapshot $repairBefore (ProductSnapshot)) 'Repair changed an unresolved transaction'
        Pass 'repair-reject-pending-recovery'
        $recoveryOutput = Join-Path $script:CaseRoot 'interrupted-recovery.report.json'
        $recovering = StartFixtureProcess $active.Setup @('--silent', '--operation', 'recover', '--recovery-session', $evidence.Session, '--recovery-apply', '--install-dir', $script:InstallRoot, '--report', $recoveryOutput, '--engine-pause', 'recovery-after-first-mutation') 'interrupted-recovery'
        $recoveryPause = WaitMaintenancePause $recovering $active 'recover' 'recovery-after-first-mutation'
        StopPausedBackend $recovering $recoveryPause
        Pass 'recover-interruption-retains-evidence'
    }
    $recovered = Recover $active $evidence.Session 'apply' -Apply
    if ($Point -eq 'after-commit' -and $OperationName -ne 'uninstall') { VerifyVersion $active }
    elseif ($Point -eq 'after-commit') { Require (-not (RegistrySnapshot).exists -and -not (Test-Path -LiteralPath (Join-Path $script:InstallRoot '.avelin-receipt.json'))) 'Committed uninstall recovery recreated managed resources' }
    else { Require (SameSnapshot $baseline (ProductSnapshot)) 'Rollback recovery did not restore the independent baseline' }
    Require ((Hash (Join-Path $script:InstallRoot 'user-sentinel.txt')) -ceq $script:SentinelHash) 'Recovery changed unmanaged data'
    $after = ProductSnapshot
    $again = Recover $active $evidence.Session 'apply-again' -Apply
    Require ($again.decision -ceq 'already_recovered' -and (SameSnapshot $after (ProductSnapshot))) 'Repeated recovery is not idempotent'
    Pass $label
    if (Test-Path -LiteralPath (Join-Path $script:InstallRoot '.avelin-receipt.json')) {
        $cleanup = if ($Point -eq 'after-commit') { $active } else { $a }
        $null = Operation $cleanup 'uninstall' 'cleanup'
    }
}
function ShortcutSnapshot([object[]]$Records) {
    @(foreach ($record in $Records) {
        $folder = if ($record.kind -ceq 'desktop') { [Environment]::GetFolderPath('DesktopDirectory') } else { [Environment]::GetFolderPath('Programs') }
        $expected = Inside (Join-Path $folder "$($script:AppId).lnk") $folder
        Require (SamePath $expected $record.path) 'Unexpected maintenance shortcut path'
        @{ kind = $record.kind; exists = Test-Path -LiteralPath $expected; sha256 = $(if (Test-Path -LiteralPath $expected) { Hash $expected } else { $null }) }
    })
}
function RepairEvidence([object]$Task) {
    Require ($Task.result -ceq 'Succeeded' -and $Task.coordinator_exit_code -eq 0) 'Repair coordinator did not confirm completion'
    $session = Inside $Task.session_dir $script:RunRoot
    $report = Json (Join-Path $session 'repair-report.json')
    Require ($report.task_id -ceq $Task.task_id -and $report.schema_version -eq 1) 'Repair report does not match this task'
    return $report
}
function RepairCases {
    NewCase 'repair'
    $a = BuildPackage 'a' '1.0.0' -Shortcuts
    $different = BuildPackage 'different' '1.0.0' -Shortcuts
    # Same identity/version but a distinct manifest must not be accepted as the
    # original repair source. Rebuild the independently changed input once.
    $differentData = Join-Path $script:CaseRoot 'project-different/payload/nested/data.txt'
    [IO.File]::AppendAllText($differentData, 'Different same-version payload', $script:Utf8)
    $differentSetup = Join-Path $script:CaseRoot 'different-manifest-Setup.exe'
    InvokeFixtureProcess $script:CliBinary @('build', '--config', (Join-Path $script:CaseRoot 'project-different/avelin.toml'), '--output', $differentSetup, '--runtime', $script:RuntimeBinary, '--rust-worker', $script:WorkerBinary) 'build-different-manifest'
    $different = @{ Setup = $differentSetup }
    $newer = BuildPackage 'newer' '1.1.0' -Shortcuts
    $installed = Operation $a 'install' 'install-a'
    VerifyVersion $a $installed.task_id
    $receipt = Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    $shortcuts = @($receipt.shortcuts)
    Require ($shortcuts.Count -eq 2) 'Repair fixture needs two owned shortcuts'
    $null = ShortcutSnapshot $shortcuts
    $missing = Inside (Join-Path $script:InstallRoot 'nested/data.txt') $script:InstallRoot
    $missingSource = Inside (Join-Path $script:CaseRoot 'missing-payload-before-repair.bin') $script:CaseRoot
    $missingRecord = @($a.Manifest.files | Where-Object { SamePath (Join-Path $script:InstallRoot $_.path) $missing })
    Require ($missingRecord.Count -eq 1 -and (Hash $missing) -ceq $missingRecord[0].sha256) 'Missing fixture payload is not owned'
    Move-Item -LiteralPath $missing -Destination $missingSource
    $desktop = @($shortcuts | Where-Object kind -CEQ 'desktop')[0]
    Require ((Hash $desktop.path) -ceq $desktop.sha256) 'Missing fixture shortcut is not owned'
    $removedLink = Inside (Join-Path $script:CaseRoot 'removed-desktop.lnk') $script:CaseRoot
    Move-Item -LiteralPath $desktop.path -Destination $removedLink
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, [Microsoft.Win32.RegistryView]::Registry64)
    try {
        $registration = RegistrySnapshot
        Require ($registration.exists -and $registration.values.DisplayVersion.value -ceq '1.0.0') 'Repair fixture registration is not the installed version'
        $base.DeleteSubKey($script:RegistrySubkey, $true)
    } finally { $base.Dispose() }
    $modified = Inside (Join-Path $script:InstallRoot '00-version.txt') $script:InstallRoot
    [IO.File]::AppendAllText($modified, 'User customization preserved by default', $script:Utf8)
    $modifiedHash = Hash $modified
    $preserved = RepairEvidence (Operation $a 'repair' 'repair-preserve')
    Require ($preserved.fully_repaired -eq $false -and @($preserved.preserved).Count -gt 0 -and (Hash $modified) -ceq $modifiedHash) 'Repair silently overwrote or hid a modified payload'
    Require ((Hash $missing) -ceq (Hash $missingSource) -and (RegistrySnapshot).exists) 'Repair did not restore missing payload/registration'
    $desktopAfter = [AvelinAcceptance.ShellLink]::Read($desktop.path)
    Require ((SamePath $desktopAfter.TargetPath (Join-Path $script:InstallRoot 'app.exe')) -and $desktopAfter.Arguments -ceq '' -and (SamePath $desktopAfter.WorkingDirectory $script:InstallRoot)) 'Repair did not restore the missing shortcut'
    Require ((Hash (Join-Path $script:InstallRoot 'user-sentinel.txt')) -ceq $script:SentinelHash) 'Repair changed unmanaged data'
    Pass 'repair-missing-resources-preserves-modified'
    foreach ($candidate in @(@{ package = $different; label = 'repair-reject-wrong-manifest' }, @{ package = $newer; label = 'repair-reject-wrong-version' })) {
        $before = ProductSnapshot; $linksBefore = @(ShortcutSnapshot $shortcuts)
        $null = Operation $candidate.package 'repair' $candidate.label @() 3
        Require ((SameSnapshot $before (ProductSnapshot)) -and (SameSnapshot $linksBefore @(ShortcutSnapshot $shortcuts))) 'Rejected repair changed files/registration/shortcuts'
        Pass $candidate.label
    }
    $overwritten = RepairEvidence (Operation $a 'repair' 'repair-overwrite' @('--repair-overwrite'))
    Require ($overwritten.fully_repaired -eq $true -and @($overwritten.backups).Count -gt 0) 'Explicit repair did not report retained backups'
    $matchedBackup = $false
    foreach ($backup in $overwritten.backups) {
        $path = Inside $backup $script:RunRoot
        if ((Test-Path -LiteralPath $path -PathType Leaf) -and (Hash $path) -ceq $modifiedHash) { $matchedBackup = $true }
    }
    Require $matchedBackup 'Explicit repair backup does not contain the original modified bytes'
    VerifyVersion $a
    Pass 'repair-explicit-overwrite-keeps-backup'
    $repeat = RepairEvidence (Operation $a 'repair' 'repair-repeat')
    Require ($repeat.fully_repaired -eq $true -and @($repeat.preserved).Count -eq 0) 'Repeated repair did not confirm an already-correct installation'
    VerifyVersion $a
    Pass 'repair-repeat-is-safe'
    $null = Operation $a 'uninstall' 'cleanup'
    Require (-not (RegistrySnapshot).exists -and (Hash (Join-Path $script:InstallRoot 'user-sentinel.txt')) -ceq $script:SentinelHash) 'Repaired installation did not uninstall while preserving user data'
    foreach ($link in @(ShortcutSnapshot $shortcuts)) { Require (-not $link.exists) 'Owned repair shortcut remained after uninstall' }
}

if ($initializeOnly) { return }
try {
    foreach ($operation in @('install', 'upgrade', 'uninstall')) {
        $firstPoint = if ($operation -ceq 'uninstall') { 'after-first-remove' } else { 'after-first-publish' }
        foreach ($point in @($firstPoint, 'after-commit')) { RecoveryCases $operation $point }
    }
    RepairCases
    $summary.status = 'PASSED'
} catch {
    $summary.status = 'FAILED'; $summary.failure = $_.Exception.Message
    $summary.failure_trace = @(foreach ($frame in ($_.ScriptStackTrace -split "`r?`n")) {
        if ($frame -match '^at ([A-Za-z0-9_<>-]+), .+: line ([0-9]+)$') { @{ function = $Matches[1]; line = [int]$Matches[2] } }
    })
    throw
} finally {
    $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o')
    Save (Join-Path $script:RunRoot 'summary.json') $summary
}
