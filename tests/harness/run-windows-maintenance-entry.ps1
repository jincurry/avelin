#requires -Version 7.2
<# Real-process checks of resource-use refusal/retry and installed maintenance.
   Uses UUID products and fresh directories below target. File holders are our own
   PowerShell children; a release marker closes their handles and exits normally.
   No process is terminated, no system setting changes, and no recursive cleanup.
   This script does not claim native UI, Narrator or Windows Settings visual QA. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Runtime,
    [Parameter(Mandatory)][string]$Cli,
    [Parameter(Mandatory)][string]$RustWorker,
    [Parameter(Mandatory)][string]$OutputRoot,
    [string]$Sample = (Join-Path $PSScriptRoot '../../target/release/nimbus-demo.exe'),
    [ValidateRange(30, 1800)][int]$TimeoutSeconds = 180
)
. (Join-Path $PSScriptRoot 'run-rust-maintenance.ps1') -Runtime $Runtime -Cli $Cli -RustWorker $RustWorker -Sample $Sample -OutputRoot $OutputRoot -TimeoutSeconds $TimeoutSeconds -FunctionsOnly
$summary.limitations = @('Silent real-process checks; native UI, Narrator and Windows Settings visuals are separate', 'Read-only resource observations do not prevent new locks after a successful check', 'No process termination, reboot, signing or independent-machine claims')
$script:ActiveHolders = [Collections.Generic.List[object]]::new()
$script:PowerShellBinary = (Get-Process -Id $PID).Path

function Passed([string]$Label) { Pass $Label; Write-Host "[$Label] passed" }
function BuildUsagePackage([string]$Label, [string]$Version) {
    $project = Inside (Join-Path $script:CaseRoot "project-$Label") $script:RunRoot
    $payload = Join-Path $project 'payload'
    $null = [IO.Directory]::CreateDirectory($payload)
    [IO.File]::Copy($script:SampleBinary, (Join-Path $payload 'app.exe'), $false)
    WriteNew (Join-Path $payload 'core/version.txt') "Version $Version"
    WriteNew (Join-Path $payload 'core/repair.txt') "Repairable $Version"
    WriteNew (Join-Path $payload 'extra/feature.txt') "Optional component $Version"
    $config = Join-Path $project 'avelin.toml'
    WriteNew $config @"
schema_version = 2
[app]
id = "$($script:AppId)"
name = "Windows maintenance acceptance"
version = "$Version"
entry = "app.exe"
[payload]
directory = "payload"
[install]
directory_name = "AvelinUsageFixture"
desktop_shortcut = false
start_menu_shortcut = false
[[components]]
id = "core"
required = true
name = { "zh-CN" = "主程序", "en-US" = "Application" }
description = { "zh-CN" = "基础文件", "en-US" = "Required files" }
paths = ["app.exe", "core/"]
[[components]]
id = "extra"
required = false
default_selected = false
name = { "zh-CN" = "可选组件", "en-US" = "Optional component" }
description = { "zh-CN" = "按需安装", "en-US" = "Install when needed" }
paths = ["extra/"]
"@
    $setup = Join-Path $script:CaseRoot "$Label-Setup.exe"
    InvokeFixtureProcess $script:CliBinary @('build', '--config', $config, '--output', $setup, '--runtime', $script:RuntimeBinary, '--rust-worker', $script:WorkerBinary) "build-$Label"
    $manifest = Json ([IO.Path]::ChangeExtension($setup, 'manifest.json'))
    Require ($manifest.app_id -ceq $script:AppId -and $manifest.app_version -ceq $Version) 'Fixture package identity differs'
    [pscustomobject]@{ Setup = $setup; Backend = [IO.Path]::ChangeExtension($setup, 'backend.exe'); Manifest = $manifest }
}
function AssertUsageSelection([object]$Package, [string[]]$Ids) {
    $receipt = Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    Require ($receipt.app_id -ceq $script:AppId -and $receipt.app_version -ceq $Package.Manifest.app_version -and $receipt.user_sid -ceq $script:UserSid) 'Receipt product/version/user differs'
    Require ((@($receipt.selected_components | Sort-Object) -join ',') -ceq (@($Ids | Sort-Object) -join ',')) 'Installed selection differs'
    $expected = @($Package.Manifest.files | Where-Object { $_.component_id -cin $Ids })
    Require ((@($receipt.files.path | Sort-Object) -join '|') -ceq (@($expected.path | Sort-Object) -join '|')) 'Receipt files do not match selected resources'
    foreach ($file in $expected) {
        Require ((Hash (Join-Path $script:InstallRoot $file.path)) -ceq $file.sha256) 'Installed payload differs'
    }
    Require ((Hash (Join-Path $script:InstallRoot 'user-sentinel.txt')) -ceq $script:SentinelHash) 'Unmanaged sentinel changed'
}
function AssertMaintenanceRegistration {
    $registration = RegistrySnapshot
    $resident = Join-Path $script:InstallRoot '.avelin/avelin-app.exe'
    $expected = '"{0}" --gui --maintenance --install-dir "{1}"' -f $resident, $script:InstallRoot
    Require ($registration.exists -and $registration.values.Contains('ModifyPath')) 'ModifyPath was not registered'
    Require ($registration.values['ModifyPath'].value -ceq $expected -and $registration.values['ModifyPath'].kind -ceq 'String') 'Maintenance registration command differs'
    Require ($registration.values['NoModify'].value -eq 0 -and $registration.values['NoModify'].kind -ceq 'DWord') 'Maintenance entry remains disabled'
    # Repair and component changes are reached through the shared Change entry.
    Require ($registration.values['NoRepair'].value -eq 1) 'Unexpected Windows direct-repair registration'
}
function Maintenance([string]$Mode, [string]$Label, [string[]]$Extra = @(), [int]$ExpectedExit = 0, [switch]$Resident) {
    $report = Join-Path $script:CaseRoot "$Label.report.json"
    $executable = if ($Resident) { Join-Path $script:InstallRoot '.avelin/avelin-app.exe' } else { $script:RuntimeBinary }
    $arguments = @('--maintenance', '--silent', '--operation', $Mode, '--install-dir', $script:InstallRoot, '--report', $report) + $Extra
    InvokeFixtureProcess $executable $arguments $Label -ExpectedExit $ExpectedExit -AwaitReport $(if ($ExpectedExit -eq 0) { $report } else { '' })
    if ($Resident -and $ExpectedExit -ne 0 -and -not (Test-Path -LiteralPath $report)) { return $null }
    Require (Test-Path -LiteralPath $report -PathType Leaf) 'Maintenance did not create its report'
    Json $report
}
function AssertPrivateMaintenance([object]$Task) {
    Require ($Task.result -ceq 'Succeeded' -and $Task.coordinator_exit_code -eq 0) 'Installed maintenance did not succeed'
    $session = Inside $Task.session_dir $script:RunRoot
    $identity = Json (Join-Path $session 'session.json')
    foreach ($process in @($identity.coordinator, $identity.backend)) {
        $image = Inside $process.executable $script:RunRoot
        Require (-not $image.StartsWith($script:InstallRoot + '\', [StringComparison]::OrdinalIgnoreCase)) 'Installed maintenance held an executing image inside the product'
    }
    Require ((Hash $identity.coordinator.executable) -ceq (Hash $script:RuntimeBinary)) 'Private coordinator differs from installed runtime'
}
function ArchiveOriginalPackage([object]$Package) {
    foreach ($path in @($Package.Setup, $Package.Backend, [IO.Path]::ChangeExtension($Package.Setup, 'manifest.json'))) {
        $source = Inside $path $script:RunRoot
        $destination = Inside ($path + '.archived') $script:RunRoot
        Require ((Test-Path -LiteralPath $source -PathType Leaf) -and -not (Test-Path -LiteralPath $destination)) 'Original package archival input differs'
        Move-Item -LiteralPath $source -Destination $destination
        Require (-not (Test-Path -LiteralPath $source)) 'Original package remained at its launch path'
    }
}
function StartFileHolder([string]$Path, [string]$Label) {
    $path = Inside $Path $script:InstallRoot
    Require (Test-Path -LiteralPath $path -PathType Leaf) 'File holder target is absent'
    $holderScript = Join-Path $script:CaseRoot "$Label.holder.ps1"
    WriteNew $holderScript @'
param([string]$Path, [string]$Ready, [string]$Release, [int]$Seconds)
$ErrorActionPreference = 'Stop'
$handle = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
try {
    $process = [Diagnostics.Process]::GetCurrentProcess()
    $identity = @{ pid = $PID; creation_time_100ns = $process.StartTime.ToUniversalTime().ToFileTimeUtc(); executable = $process.MainModule.FileName; path = $Path }
    [IO.File]::WriteAllText(($Ready + '.pending'), ($identity | ConvertTo-Json -Compress), [Text.UTF8Encoding]::new($false))
    [IO.File]::Move(($Ready + '.pending'), $Ready)
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    while (-not [IO.File]::Exists($Release) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 20 }
} finally { $handle.Dispose() }
'@
    $ready = Join-Path $script:CaseRoot "$Label.holder.ready.json"
    $release = Join-Path $script:CaseRoot "$Label.holder.release"
    $running = StartFixtureProcess $script:PowerShellBinary @('-NoProfile', '-NonInteractive', '-File', $holderScript, '-Path', $path, '-Ready', $ready, '-Release', $release, '-Seconds', ($TimeoutSeconds * 3).ToString()) "$Label-holder"
    $holder = [pscustomobject]@{ Running = $running; Release = $release; Identity = $null; Path = $path; Released = $false }
    $script:ActiveHolders.Add($holder)
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while (-not (Test-Path -LiteralPath $ready -PathType Leaf)) {
        Require (-not $running.Process.HasExited) 'File holder exited before opening its file'
        Require ($timer.Elapsed.TotalSeconds -lt 15) 'File holder did not report its handle'
        Start-Sleep -Milliseconds 20
    }
    $holder.Identity = Json $ready
    Require ($holder.Identity.pid -eq $running.Process.Id -and (SamePath $holder.Identity.executable $script:PowerShellBinary) -and (SamePath $holder.Identity.path $path)) 'Holder identity differs from its launched child'
    return $holder
}
function ReleaseFileHolder([object]$Holder) {
    if ($Holder.Released) { return }
    $Holder.Released = $true
    WriteNew $Holder.Release 'Release the fixture-owned file handle'
    FinishFixtureProcess $Holder.Running 0
}
function AssertBusyAttempt([scriptblock]$Action, [object]$Holder, [string]$Label) {
    $before = ProductSnapshot
    $report = & $Action
    Require ($report.result -ceq 'NotStarted' -and $report.coordinator_exit_code -eq 3 -and $report.worker_started -eq $false) 'Occupied attempt started a worker or reported a task result'
    Require ($report.diagnostic.code -ceq 'AVELIN_RESOURCES_IN_USE') 'Occupied attempt lacks its structured diagnostic code'
    Require (@($report.preflight.processes | Where-Object { $_.pid -eq $Holder.Identity.pid -and $_.creation_time_100ns -eq $Holder.Identity.creation_time_100ns }).Count -eq 1) 'Occupying process identity is missing or duplicated'
    Require (@($report.preflight.locked_files | Where-Object { SamePath $_.path $Holder.Path }).Count -eq 1) 'Occupied file was not identified'
    Require (@($report.preflight.issues).Count -eq 0) 'Resource check failed rather than reporting the real lock'
    Require ($report.PSObject.Properties.Name -cnotcontains 'task_id' -and $report.PSObject.Properties.Name -cnotcontains 'session_dir') 'Preflight fabricated a transaction identity'
    Require (SameSnapshot $before (ProductSnapshot)) 'Occupied refusal changed payload, receipt or registration'
    Require (-not $Holder.Running.Process.HasExited) 'Installer stopped the occupying application'
    Save (Join-Path $script:CaseRoot "$Label.unchanged.json") @{ before = $before; after = ProductSnapshot; holder = $Holder.Identity }
    Passed $Label
}
function NormalInstalledOperation([string]$Mode, [string]$Label, [string[]]$Extra = @(), [int]$ExpectedExit = 0) {
    $report = Join-Path $script:CaseRoot "$Label.report.json"
    $arguments = @('--silent', '--operation', $Mode, '--install-dir', $script:InstallRoot, '--report', $report, '--backend', (Join-Path $script:InstallRoot 'uninstall.exe'), '--manifest', (Join-Path $script:InstallRoot '.avelin/manifest.json')) + $Extra
    InvokeFixtureProcess $script:RuntimeBinary $arguments $Label -ExpectedExit $ExpectedExit -AwaitReport $(if ($ExpectedExit -eq 0) { $report } else { '' })
    Require (Test-Path -LiteralPath $report) 'Installed-resource operation did not create its report'
    Json $report
}
function AssertRecoveredEntry([object]$Task, [string]$Decision, [bool]$Applied) {
    AssertPrivateMaintenance $Task
    $recovery = Json (Join-Path (Inside $Task.session_dir $script:RunRoot) 'recovery-report.json')
    Require ($recovery.verified -eq $true -and $recovery.applied -eq $Applied -and $recovery.decision -ceq $Decision) 'Installed recovery entry selected the wrong outcome'
}

try {
    NewCase 'usage-and-maintenance'
    $a = BuildUsagePackage 'a' '1.0.0'
    $b = BuildUsagePackage 'b' '1.1.0'
    $installed = Operation $a 'install' 'install-core' @('--components', 'core')
    AssertUsageSelection $a @('core')
    AssertMaintenanceRegistration
    Passed 'maintenance-command-registered'
    ArchiveOriginalPackage $a

    $missing = Inside (Join-Path $script:InstallRoot 'core/repair.txt') $script:InstallRoot
    $retained = Inside (Join-Path $script:CaseRoot 'removed-for-repair.txt') $script:RunRoot
    $missingHash = Hash $missing
    Move-Item -LiteralPath $missing -Destination $retained
    $holder = StartFileHolder (Join-Path $script:InstallRoot 'core/version.txt') 'repair'
    AssertBusyAttempt { Maintenance 'repair' 'repair-blocked' @() 3 } $holder 'repair-occupied-refused-without-mutation'
    ReleaseFileHolder $holder
    $repaired = Maintenance 'repair' 'repair-retry'
    AssertPrivateMaintenance $repaired
    Require ((Hash $missing) -ceq $missingHash) 'Repair retry did not restore the missing file'
    AssertUsageSelection $a @('core')
    AssertMaintenanceRegistration
    Passed 'repair-release-retry-without-original-setup'

    $holder = StartFileHolder (Join-Path $script:InstallRoot 'core/version.txt') 'modify'
    AssertBusyAttempt { Maintenance 'modify' 'modify-blocked' @('--components', 'core,extra') 3 } $holder 'modify-occupied-refused-without-mutation'
    ReleaseFileHolder $holder
    $modified = Maintenance 'modify' 'modify-retry' @('--components', 'core,extra')
    AssertPrivateMaintenance $modified
    AssertUsageSelection $a @('core', 'extra')
    Passed 'modify-release-retry-without-original-setup'

    $beforeResident = ProductSnapshot
    $resident = Maintenance 'repair' 'resident-silent-rejected' @() 3 -Resident
    Require ($null -eq $resident) 'Rejected resident silent mode fabricated a task report'
    $residentError = [IO.File]::ReadAllText((Join-Path $script:CaseRoot 'resident-silent-rejected.stderr.log'), $script:Utf8)
    Require ($residentError.Contains('silent installed maintenance must be launched from an external runtime')) 'Resident silent mode failed for an unrelated reason'
    Require (SameSnapshot $beforeResident (ProductSnapshot)) 'Rejected resident silent mode changed the product'
    Passed 'resident-silent-maintenance-requires-external-runtime'

    # A controlled post-commit failure retains real confirmation work without
    # terminating a process. Its installed receipt and full package remain intact.
    $null = Maintenance 'modify' 'remove-extra-for-recovery' @('--components', 'core')
    AssertUsageSelection $a @('core')
    $failed = NormalInstalledOperation 'modify' 'pending-modify' @('--components', 'core,extra', '--engine-fault', 'after-commit') 4
    Require ($failed.result -ceq 'Unknown' -and $failed.coordinator_exit_code -eq 4) 'Controlled failure did not retain an unresolved transaction'
    $originalSession = Inside $failed.session_dir $script:RunRoot
    $pending = ProductSnapshot
    AssertUsageSelection $a @('core', 'extra')
    $committedReceipt = Hash (Join-Path $script:InstallRoot '.avelin-receipt.json')
    $committedResources = @($pending.files | Where-Object path -CNE '.avelin\upgrade-state.json')
    $check = Maintenance 'recover' 'entry-recovery-check' @('--recovery-session', $originalSession)
    AssertRecoveredEntry $check 'confirm_committed' $false
    Require (SameSnapshot $pending (ProductSnapshot)) 'Recovery check mutated the pending product'
    Passed 'installed-recovery-check-is-read-only'
    $applied = Maintenance 'recover' 'entry-recovery-apply' @('--recovery-session', $originalSession, '--recovery-apply')
    AssertRecoveredEntry $applied 'confirm_committed' $true
    $confirmed = ProductSnapshot
    Require ((SameSnapshot $committedResources $confirmed.files) -and (SameSnapshot $pending.registry $confirmed.registry)) 'Recovery confirmation changed committed resources or registration'
    Require ((Hash (Join-Path $script:InstallRoot '.avelin-receipt.json')) -ceq $committedReceipt) 'Recovery confirmation changed the committed receipt'
    AssertUsageSelection $a @('core', 'extra')
    Passed 'installed-recovery-apply-without-original-setup'
    $again = Maintenance 'recover' 'entry-recovery-repeat' @('--recovery-session', $originalSession, '--recovery-apply')
    AssertRecoveredEntry $again 'already_recovered' $true
    Require (SameSnapshot $confirmed (ProductSnapshot)) 'Repeated recovery changed the confirmed product'
    Passed 'installed-recovery-repeat-is-idempotent'

    $holder = StartFileHolder (Join-Path $script:InstallRoot 'core/version.txt') 'upgrade'
    AssertBusyAttempt { Operation $b 'upgrade' 'upgrade-blocked' @() 3 } $holder 'upgrade-occupied-refused-without-mutation'
    ReleaseFileHolder $holder
    $upgraded = Operation $b 'upgrade' 'upgrade-retry'
    Require ($upgraded.result -ceq 'Succeeded' -and $upgraded.coordinator_exit_code -eq 0) 'Upgrade retry failed'
    AssertUsageSelection $b @('core', 'extra')
    AssertMaintenanceRegistration
    Passed 'upgrade-release-retry-keeps-maintenance-entry'

    $holder = StartFileHolder (Join-Path $script:InstallRoot 'uninstall.exe') 'uninstall'
    AssertBusyAttempt { Operation $b 'uninstall' 'uninstall-blocked' @() 3 } $holder 'uninstall-occupied-refused-without-mutation'
    ReleaseFileHolder $holder
    $uninstalled = Operation $b 'uninstall' 'uninstall-retry'
    Require ($uninstalled.result -ceq 'Succeeded' -and $uninstalled.coordinator_exit_code -eq 0) 'Uninstall retry failed'
    Require (-not (RegistrySnapshot).exists -and -not (Test-Path -LiteralPath (Join-Path $script:InstallRoot '.avelin-receipt.json'))) 'Uninstall retained product registration/receipt'
    Require ((Hash (Join-Path $script:InstallRoot 'user-sentinel.txt')) -ceq $script:SentinelHash) 'Uninstall removed unmanaged data'
    Passed 'uninstall-release-retry-removes-maintenance-entry'
    $summary.status = 'PASSED'
} catch {
    $summary.status = 'FAILED'
    $summary.failure = $_.Exception.Message
    $summary.failure_trace = @(foreach ($frame in ($_.ScriptStackTrace -split "`r?`n")) {
        if ($frame -match '^at ([A-Za-z0-9_<>-]+), .+: line ([0-9]+)$') { @{ function = $Matches[1]; line = [int]$Matches[2] } }
    })
    throw
} finally {
    foreach ($holder in $script:ActiveHolders) {
        if (-not $holder.Released) {
            try { ReleaseFileHolder $holder } catch { Write-Warning "Fixture holder release failed: $($_.Exception.Message)" }
        }
    }
    $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o')
    Save (Join-Path $script:RunRoot 'summary.json') $summary
}
