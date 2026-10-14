#requires -Version 7.2
<# Elevated, real-process acceptance for one UUID machine installation.
   Uses Program Files, HKLM and the two public shell folders. Only this fixture's
   resources are touched. Removal goes through the installer, never a recursive
   filesystem or registry cleanup. On failure the exact scene is retained.
   Run from an elevated Windows x64 PowerShell; this script never triggers UAC. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Runtime,
    [Parameter(Mandatory)][string]$Cli,
    [Parameter(Mandatory)][string]$RustWorker,
    [Parameter(Mandatory)][string]$Sample,
    [Parameter(Mandatory)][string]$OutputRoot,
    [ValidateRange(30, 1800)][int]$TimeoutSeconds = 180
)
. (Join-Path $PSScriptRoot 'run-rust-engine-interruption.ps1') -Runtime $Runtime -Cli $Cli -RustWorker $RustWorker -Sample $Sample -OutputRoot $OutputRoot -TimeoutSeconds $TimeoutSeconds -FunctionsOnly
. (Join-Path $PSScriptRoot 'windows-shell-link.ps1')
Require ($IsWindows -and [Environment]::Is64BitProcess) 'Use Windows x64 PowerShell 7.2+'
$principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
Require ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) 'NEEDS_ELEVATION: machine acceptance requires an elevated PowerShell; no cases ran'
$projectRoot = Full (Join-Path $PSScriptRoot '../..')
$script:RunRoot = Inside $OutputRoot (Join-Path $projectRoot 'target')
Require (-not (Test-Path -LiteralPath $script:RunRoot)) 'Machine OutputRoot must be new'
$null = [IO.Directory]::CreateDirectory($script:RunRoot)
$script:CaseRoot = $script:RunRoot
$script:LocalRoot = Join-Path $script:RunRoot 'local-data'
$script:TempRoot = Join-Path $script:RunRoot 'temp'
foreach ($path in @($script:LocalRoot, $script:TempRoot)) { $null = [IO.Directory]::CreateDirectory($path) }
$script:RuntimeBinary = Full $Runtime; $script:CliBinary = Full $Cli; $script:WorkerBinary = Full $RustWorker; $script:SampleBinary = Full $Sample
$id = [Guid]::NewGuid().ToString('N')
$script:AppId = 'dev.avelin.machine.' + $id
$directoryName = 'AvelinMachine-' + $id
$programFiles = [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
$script:InstallRoot = Inside (Join-Path $programFiles $directoryName) $programFiles
$machineRoot = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)) 'Avelin/Machine'
$machineSessions = Join-Path $machineRoot 'sessions'
$script:RegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:AppId
$script:UserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$publicDesktop = [Environment]::GetFolderPath([Environment+SpecialFolder]::CommonDesktopDirectory)
$publicPrograms = [Environment]::GetFolderPath([Environment+SpecialFolder]::CommonPrograms)
$shortcuts = @((Join-Path $publicDesktop "$($script:AppId).lnk"), (Join-Path $publicPrograms "$($script:AppId).lnk"))
$summary = [ordered]@{
    schema_version = 1; status = 'RUNNING'; cases = @(); components = @(); app_id = $script:AppId
    install_dir = $script:InstallRoot; started_at_utc = [DateTime]::UtcNow.ToString('o')
    limitations = @('Elevated silent real-process checks; no native GUI or UAC consent automation', 'One administrator account; cross-account support is validated by scope and ownership rules, not a second interactive login', 'Interruption uses an explicit transaction checkpoint')
}
foreach ($binary in @($script:RuntimeBinary, $script:CliBinary, $script:WorkerBinary, $script:SampleBinary)) { $summary.components += @{ path = $binary; sha256 = Hash $binary } }
function Pass([string]$Label) {
    $summary.cases += @{ label = $Label; status = 'PASSED' }
    Save (Join-Path $script:RunRoot 'summary.json') $summary
    Write-Host "[$Label] passed"
}
function MachineRegistration([Microsoft.Win32.RegistryHive]$Hive = [Microsoft.Win32.RegistryHive]::LocalMachine) {
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey($Hive, [Microsoft.Win32.RegistryView]::Registry64)
    $key = $null
    try {
        $key = $base.OpenSubKey($script:RegistrySubkey, $false)
        $values = [ordered]@{}
        if ($key) { foreach ($name in ($key.GetValueNames() | Sort-Object)) { $values[$name] = $key.GetValue($name) } }
        [pscustomobject]@{ exists = $null -ne $key; values = $values }
    } finally { if ($key) { $key.Dispose() }; $base.Dispose() }
}
function AssertMachineAcl([string]$Path) {
    Require (Test-Path -LiteralPath $Path) "Missing machine resource: $Path"
    Require (((Get-Item -LiteralPath $Path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Machine evidence became a reparse point: $Path"
    $acl = Get-Acl -LiteralPath $Path
    $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
    Require ($owner -in @('S-1-5-18', 'S-1-5-32-544')) "Machine resource is owned by a personal user SID: $Path"
    # Includes file/child writes, delete, WRITE_DAC/OWNER and generic write/all.
    $writeMask = [long]0x500D0156
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow) { continue }
        if (([long]$rule.FileSystemRights -band $writeMask) -ne 0) {
            Require ($rule.IdentityReference.Value -in @('S-1-5-18', 'S-1-5-32-544')) "Machine resource permits non-administrator writes: $Path"
        }
    }
}
function AssertEvidenceDirectory([string]$Path) {
    $session = Inside $Path $machineSessions
    Require (SamePath (Split-Path -Parent $session) $machineSessions) 'Session is not a direct protected machine child'
    foreach ($ancestor in @((Split-Path -Parent $machineRoot), $machineRoot, $machineSessions, $session)) { AssertMachineAcl $ancestor }
    foreach ($resource in Get-ChildItem -LiteralPath $session -Recurse -Force) {
        $safe = Inside $resource.FullName $session
        AssertMachineAcl $safe
    }
}
function AssertSession([object]$Report) {
    Require ($Report.result -ceq 'Succeeded' -and $Report.coordinator_exit_code -eq 0) 'Machine task did not confirm success'
    $session = Inside $Report.session_dir $machineSessions
    Require (SamePath (Split-Path -Parent $session) $machineSessions) 'Session is not a direct protected machine child'
    AssertEvidenceDirectory $session
    foreach ($name in @('session.json', 'install-plan.json', 'receipt-candidate.json', 'engine-result.json')) { AssertMachineAcl (Join-Path $session $name) }
    $identity = Json (Join-Path $session 'session.json')
    $plan = Json (Join-Path $session 'install-plan.json')
    $candidate = Json (Join-Path $session 'receipt-candidate.json')
    Require ($identity.install_scope -ceq 'all_users' -and $plan.install_scope -ceq 'all_users' -and $candidate.install_scope -ceq 'all_users') 'Machine task lost its scope'
    Require ((SamePath $identity.install_dir $script:InstallRoot) -and $identity.app_id -ceq $script:AppId) 'Machine session identity differs'
}
function BuildMachinePackage([string]$Label, [string]$Version) {
    $project = Join-Path $script:RunRoot "project-$Label"
    $payload = Join-Path $project 'payload'
    $null = [IO.Directory]::CreateDirectory($payload)
    [IO.File]::Copy($script:SampleBinary, (Join-Path $payload 'app.exe'), $false)
    WriteNew (Join-Path $payload 'core/version.txt') "Core $Version"
    WriteNew (Join-Path $payload 'optional/feature.txt') "Optional $Version"
    $config = Join-Path $project 'avelin.toml'
    WriteNew $config @"
schema_version = 2
[app]
id = "$($script:AppId)"
name = "Machine acceptance fixture"
version = "$Version"
entry = "app.exe"
[payload]
directory = "payload"
[install]
scope = "all_users"
directory_name = "$directoryName"
desktop_shortcut = true
start_menu_shortcut = true
[[components]]
id = "core"
required = true
name = { "en-US" = "Application", "zh-CN" = "主程序" }
description = { "en-US" = "Required application files", "zh-CN" = "必需的应用文件" }
paths = ["app.exe", "core/"]
[[components]]
id = "optional"
required = false
default_selected = false
name = { "en-US" = "Optional component", "zh-CN" = "可选组件" }
description = { "en-US" = "Additional application files", "zh-CN" = "按需安装的附加文件" }
paths = ["optional/"]
"@
    $setup = Join-Path $script:RunRoot "$Label-Setup.exe"
    InvokeFixtureProcess $script:CliBinary @('check', '--config', $config) "check-$Label"
    InvokeFixtureProcess $script:CliBinary @('build', '--config', $config, '--output', $setup, '--runtime', $script:RuntimeBinary, '--rust-worker', $script:WorkerBinary) "build-$Label"
    $manifest = Json ([IO.Path]::ChangeExtension($setup, 'manifest.json'))
    Require ($manifest.install_scope -ceq 'all_users' -and $manifest.app_id -ceq $script:AppId) 'Packaged machine scope differs'
    [pscustomobject]@{ Setup = $setup; Backend = [IO.Path]::ChangeExtension($setup, 'backend.exe'); Manifest = $manifest }
}
function MachineOperation([object]$Package, [string]$Operation, [string]$Label, [string[]]$Extra = @(), [int]$ExpectedExit = 0, [switch]$Offline, [string]$Target = $script:InstallRoot) {
    $report = Join-Path $script:RunRoot "$Label.report.json"
    $exe = $Package.Setup
    $arguments = @('--silent', '--operation', $Operation, '--install-dir', $Target, '--report', $report) + $Extra
    if ($Offline) {
        $exe = $script:RuntimeBinary
        $arguments += '--maintenance'
    } elseif ($Operation -eq 'uninstall') {
        $exe = $script:RuntimeBinary
        $arguments += @('--backend', (Join-Path $script:InstallRoot 'uninstall.exe'), '--manifest', (Join-Path $script:InstallRoot '.avelin/manifest.json'))
    }
    InvokeFixtureProcess $exe $arguments $Label -ExpectedExit $ExpectedExit -AwaitReport $(if ($ExpectedExit -eq 0) { $report } else { '' })
    if ($ExpectedExit -eq 0) { $result = Json $report; AssertSession $result; return $result }
    return $null
}
function AssertInstalled([object]$Package, [string[]]$Components) {
    $receipt = Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    Require ($receipt.install_scope -ceq 'all_users' -and $receipt.app_id -ceq $script:AppId -and $receipt.app_version -ceq $Package.Manifest.app_version) 'Installed scope/product/version differs'
    Require ((@($receipt.selected_components | Sort-Object) -join ',') -ceq (@($Components | Sort-Object) -join ',')) 'Installed selection differs'
    AssertMachineAcl $script:InstallRoot
    foreach ($resource in Get-ChildItem -LiteralPath $script:InstallRoot -Recurse -Force) { AssertMachineAcl $resource.FullName }
    foreach ($file in $receipt.files) { Require ((Hash (Inside (Join-Path $script:InstallRoot $file.path) $script:InstallRoot)) -ceq $file.sha256) 'Installed managed bytes differ' }
    Require ((Hash (Join-Path $script:InstallRoot '.avelin/avelin-app.exe')) -ceq $Package.Manifest.runtime_sha256 -and $Package.Manifest.runtime_sha256 -ceq (Hash $script:RuntimeBinary)) 'Installed machine runtime differs'
    Require ((Hash (Join-Path $script:InstallRoot 'uninstall.exe')) -ceq $receipt.uninstaller_sha256 -and $receipt.uninstaller_sha256 -ceq (Hash $Package.Backend)) 'Installed offline worker differs'
    Require ((Hash (Join-Path $script:InstallRoot '.avelin/manifest.json')) -ceq (Hash ([IO.Path]::ChangeExtension($Package.Setup, 'manifest.json')))) 'Installed machine manifest differs'
    foreach ($file in $Package.Manifest.files | Where-Object { $_.component_id -cnotin $Components }) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $file.path))) 'Unselected component exists' }
    $registration = MachineRegistration
    Require ($registration.exists -and $registration.values.DisplayVersion -ceq $Package.Manifest.app_version -and (SamePath $registration.values.InstallLocation $script:InstallRoot)) 'HKLM registration differs'
    Require (-not (MachineRegistration ([Microsoft.Win32.RegistryHive]::CurrentUser)).exists) 'Machine task also registered an HKCU product'
    Require ($registration.values.NoModify -eq 0 -and $registration.values.ModifyPath.Contains('--maintenance')) 'Machine maintenance registration missing'
    foreach ($shortcut in $shortcuts) {
        Require (Test-Path -LiteralPath $shortcut) 'Public shortcut missing'
        $link = [AvelinAcceptance.ShellLink]::Read($shortcut)
        Require ((SamePath $link.TargetPath (Join-Path $script:InstallRoot 'app.exe')) -and (SamePath $link.WorkingDirectory $script:InstallRoot)) 'Public shortcut points outside fixture'
        AssertMachineAcl $shortcut
    }
}
function InterruptUpgrade([object]$Package) {
    $report = Join-Path $script:RunRoot 'interrupted.report.json'
    $running = StartFixtureProcess $Package.Setup @('--silent', '--operation', 'upgrade', '--install-dir', $script:InstallRoot, '--report', $report, '--engine-pause', 'after-first-publish') 'interrupted'
    $timer = [Diagnostics.Stopwatch]::StartNew(); $evidence = $null
    while ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds -and -not $evidence) {
        Require (-not $running.Process.HasExited) 'Machine upgrade exited before the requested checkpoint'
        foreach ($directory in Get-ChildItem -LiteralPath $machineSessions -Directory -Filter 'session-*') {
            $pausePath = Join-Path $directory.FullName 'engine-pause.json'
            if (-not (Test-Path -LiteralPath $pausePath)) { continue }
            try { $identity = Json (Join-Path $directory.FullName 'session.json'); $pause = Json $pausePath } catch { continue }
            if ($identity.coordinator.pid -ne $running.Process.Id) { continue }
            Require ((SamePath $identity.install_dir $script:InstallRoot) -and (SamePath (Split-Path -Parent $directory.FullName) $machineSessions) -and $identity.app_id -ceq $script:AppId -and $identity.install_scope -ceq 'all_users' -and $identity.operation -ceq 'upgrade' -and $pause.task_id -ceq $identity.task_id -and $pause.backend_pid -eq $identity.backend.pid -and $pause.point -ceq 'after-first-publish') 'Interruption evidence differs'
            Require ((Hash $identity.backend.executable) -ceq (Hash $Package.Backend)) 'Paused worker is not this fixture package'
            $evidence = @{ session = $directory.FullName; identity = $identity; deadline = (Get-Item -LiteralPath $pausePath).LastWriteTimeUtc.AddSeconds(27) }
            break
        }
        if (-not $evidence) { Start-Sleep -Milliseconds 20 }
    }
    Require ($null -ne $evidence) 'Machine interruption checkpoint timed out; no process was killed'
    $backend = $null; $coordinator = $null
    try {
        $backend = [AvelinInterruption.VerifiedProcess]::new([uint32]$evidence.identity.backend.pid, [uint64]$evidence.identity.backend.creation_time_100ns, $evidence.identity.backend.executable)
        $coordinator = [AvelinInterruption.VerifiedProcess]::new([uint32]$evidence.identity.coordinator.pid, [uint64]$evidence.identity.coordinator.creation_time_100ns, $evidence.identity.coordinator.executable)
        Require ([DateTime]::UtcNow -lt $evidence.deadline) 'Machine interruption checkpoint expired'
        $backend.Terminate()
        Require ($backend.Wait(10000) -and $backend.ExitCode -eq 137 -and $coordinator.Wait(30000)) 'Interrupted machine task did not stop'
    } finally { if ($backend) { $backend.Dispose() }; if ($coordinator) { $coordinator.Dispose() } }
    FinishFixtureProcess $running 4
    AssertEvidenceDirectory $evidence.session
    $lock = Inside $evidence.identity.product_lock (Join-Path $machineRoot 'locks')
    AssertMachineAcl (Split-Path -Parent $lock)
    $interrupted = Json $report
    Require ($interrupted.result -ceq 'Unknown' -and $interrupted.coordinator_exit_code -eq 4 -and $interrupted.exit_code -eq 137 -and $interrupted.task_id -ceq $evidence.identity.task_id -and (SamePath $interrupted.session_dir $evidence.session)) 'Interrupted coordinator evidence differs'
    if (Test-Path -LiteralPath $lock) {
        AssertMachineAcl $lock
    } else {
        # The coordinator removes its lock after observing the worker exit.
        # The Unknown result still retains the journal/marker for recovery.
        Require ($interrupted.product_lock_retained -eq $false) 'A reported retained product lock is missing'
    }
    foreach ($name in @('engine-recovery.json', 'install-plan.json', 'session.json', 'upgrade-journal.jsonl')) { AssertMachineAcl (Join-Path $evidence.session $name) }
    return $evidence.session
}
try {
    Require (-not (Test-Path -LiteralPath $script:InstallRoot)) 'UUID Program Files target already exists'
    Require (-not (MachineRegistration).exists -and -not (MachineRegistration ([Microsoft.Win32.RegistryHive]::CurrentUser)).exists) 'UUID registration already exists'
    foreach ($shortcut in $shortcuts) { Require (-not (Test-Path -LiteralPath $shortcut)) 'UUID public shortcut already exists' }
    $a = BuildMachinePackage 'a' '1.0.0'; $b = BuildMachinePackage 'b' '1.1.0'; $c = BuildMachinePackage 'c' '1.2.0'
    Pass 'package-all-users-scope'
    $badTarget = Join-Path $script:RunRoot 'forbidden-user-target'
    $null = MachineOperation $a 'install' 'reject-user-target' -Target $badTarget -ExpectedExit 3
    $rejection = [IO.File]::ReadAllText((Join-Path $script:RunRoot 'reject-user-target.stderr.log'))
    Require ($rejection.Contains('all-users destination must be a direct child of the system Program Files directory')) 'Wrong-target check failed for an unrelated reason'
    Require (-not (Test-Path -LiteralPath $badTarget) -and -not (MachineRegistration).exists) 'Rejected target changed machine state'
    Pass 'reject-user-writable-target-before-mutation'
    $null = MachineOperation $a 'install' 'install' @('--components', 'core')
    AssertInstalled $a @('core')
    Pass 'install-hklm-public-shortcuts-protected-files'
    $missing = Inside (Join-Path $script:InstallRoot 'core/version.txt') $script:InstallRoot
    $receipt = Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    $owned = @($receipt.files | Where-Object { $_.path.Replace('\', '/') -ceq 'core/version.txt' })
    Require ($owned.Count -eq 1 -and (Hash $missing) -ceq $owned[0].sha256) 'Missing-file repair fixture ownership differs'
    Remove-Item -LiteralPath $missing
    $null = MachineOperation $a 'repair' 'offline-repair' -Offline
    AssertInstalled $a @('core')
    Pass 'offline-machine-repair-without-setup'
    $null = MachineOperation $a 'modify' 'offline-add-component' @('--components', 'core,optional') -Offline
    AssertInstalled $a @('core', 'optional')
    Pass 'offline-machine-component-add'
    $null = MachineOperation $b 'upgrade' 'upgrade'
    AssertInstalled $b @('core', 'optional')
    Pass 'machine-upgrade-inherits-selection'
    $session = InterruptUpgrade $c
    Pass 'machine-interruption-retains-protected-evidence'
    $originalTask = (Json (Join-Path $session 'session.json')).task_id
    $beforeCheck = [pscustomobject]@{ files = @(Snapshot $script:InstallRoot); registry = MachineRegistration }
    $checked = MachineOperation $c 'recover' 'recover-check' @('--recovery-session', $session)
    Require ($checked.maintenance_status -ceq 'RecoveryChecked' -and $checked.recovery.verified -and -not $checked.recovery.applied -and $checked.recovery.original_task_id -ceq $originalTask) 'Recovery inspection evidence differs'
    Require (SameSnapshot $beforeCheck ([pscustomobject]@{ files = @(Snapshot $script:InstallRoot); registry = MachineRegistration })) 'Read-only recovery inspection changed installation resources'
    Pass 'machine-recovery-inspection'
    $recovered = MachineOperation $c 'recover' 'recover-apply' @('--recovery-session', $session, '--recovery-apply')
    Require ($recovered.maintenance_status -ceq 'RecoveryComplete' -and $recovered.recovery.verified -and $recovered.recovery.applied -and $recovered.recovery.original_task_id -ceq $originalTask) 'Applied recovery evidence differs'
    AssertMachineAcl (Join-Path $session 'recovery-complete.json')
    AssertInstalled $b @('core', 'optional')
    Pass 'machine-recovery-restores-owned-version'
    $null = MachineOperation $b 'modify' 'offline-remove-component' @('--components', 'core') -Offline
    AssertInstalled $b @('core')
    Pass 'offline-machine-component-remove'
    $null = MachineOperation $b 'uninstall' 'uninstall'
    Require (-not (MachineRegistration).exists) 'HKLM uninstall registration remains'
    Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot '.avelin-receipt.json'))) 'Uninstall receipt remains'
    foreach ($shortcut in $shortcuts) { Require (-not (Test-Path -LiteralPath $shortcut)) 'Public shortcut remains' }
    foreach ($file in $b.Manifest.files) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $file.path))) 'Managed payload remains' }
    foreach ($name in @('uninstall.exe', '.avelin/avelin-app.exe', '.avelin/manifest.json', '.avelin/upgrade-state.json')) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $name))) 'Managed maintenance resource remains' }
    Pass 'machine-uninstall-removes-managed-resources'
    $summary.status = 'PASSED'
} catch {
    $summary.status = 'FAILED'; $summary.error = $_.Exception.Message
    throw
} finally {
    $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o')
    Save (Join-Path $script:RunRoot 'summary.json') $summary
}
