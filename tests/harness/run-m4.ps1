#requires -Version 7.2
<# Portable kit normal-cycle acceptance. Uses no Cargo, system NSIS or GUI automation.
   Helpers are a local copy of the established M3 checks; frozen M3 is untouched. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$KitRoot,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [ValidateRange(10,600)][int]$TimeoutSeconds = 120
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
function Require([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }

function Full([string]$Path) { [IO.Path]::GetFullPath($Path).TrimEnd([char]'\', [char]'/') }

function SamePath([string]$Left, [string]$Right) { [string]::Equals((Full $Left), (Full $Right), [StringComparison]::OrdinalIgnoreCase) }

function Inside([string]$Path, [string]$Root) {
    $absolute = Full $Path; $boundary = Full $Root
    Require ($absolute.StartsWith($boundary + '\', [StringComparison]::OrdinalIgnoreCase)) "Path escaped fixture: $absolute"
    for ($cursor = $absolute; $cursor; $cursor = Split-Path -Parent $cursor) {
        if (Test-Path -LiteralPath $cursor) { Require (((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse point: $cursor" }
    }
    return $absolute
}

function Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }

function HashBytes([byte[]]$Bytes) { [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($Bytes)).ToLowerInvariant() }

function ReadBytes([IO.Stream]$Stream, [int]$Count) {
    $bytes = [byte[]]::new($Count); $read = 0
    while ($read -lt $Count) { $received = $Stream.Read($bytes, $read, $Count - $read); Require ($received -gt 0) 'Unexpected package EOF'; $read += $received }
    return ,$bytes
}

function Json([string]$Path) { [Text.UTF8Encoding]::new($false, $true).GetString([IO.File]::ReadAllBytes($Path)) | ConvertFrom-Json }

function Save([string]$Name, [object]$Value) { [IO.File]::WriteAllText((Inside (Join-Path $script:RunRoot $Name) $script:RunRoot), ($Value | ConvertTo-Json -Depth 40), [Text.UTF8Encoding]::new($false)) }

function Snapshot([string]$Root) {
    $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push($Root)
    $files = while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            $safe = Inside $item.FullName $Root
            if ($item.PSIsContainer) { $pending.Push($safe) } else { [pscustomobject]@{ path = $safe.Substring($Root.Length + 1); size = $item.Length; sha256 = Hash $safe } }
        }
    }
    @($files | Sort-Object path)
}

function AssertFiles([string]$Root, [object[]]$Expected, [switch]$Exact) {
    foreach ($file in $Expected) {
        $path = Inside (Join-Path $Root $file.path) $Root
        Require ((Test-Path -LiteralPath $path -PathType Leaf) -and (Hash $path) -ceq $file.sha256 -and (Get-Item -LiteralPath $path).Length -eq $file.size) "File mismatch: $path"
    }
    if ($Exact) { Require (@(Snapshot $Root).Count -eq $Expected.Count) "Unexpected remaining files: $Root" }
}

function RegistryState([string]$Subkey = $script:RegistrySubkey) {
    foreach ($view in @([Microsoft.Win32.RegistryView]::Registry32, [Microsoft.Win32.RegistryView]::Registry64)) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $view); $key = $null
        try {
            $key = $base.OpenSubKey($Subkey, $false); $values = [ordered]@{}
            if ($key) { foreach ($name in ($key.GetValueNames() | Sort-Object)) { $values[$name] = $key.GetValue($name) } }
            [pscustomobject]@{ view = "$view"; exists = $null -ne $key; values = $values }
        } finally { if ($key) { $key.Dispose() }; $base.Dispose() }
    }
}

function InvokeProcess([string]$Executable, [string[]]$Arguments, [string]$Name, [string]$AwaitReport = '', [string]$RawArguments = '', [int]$ExpectedExit = 0) {
    $info = [Diagnostics.ProcessStartInfo]::new($Executable); $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden; $info.WorkingDirectory = $script:RunRoot
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [Text.UTF8Encoding]::new($false, $true); $info.StandardErrorEncoding = $info.StandardOutputEncoding
    $null = $info.Environment.Remove('NSIS_MAKENSIS')
    $null = $info.Environment.Remove('NSISDIR')
    $info.Environment['PATH'] = [Environment]::GetFolderPath('System') + ';' + $env:SystemRoot
    $info.Environment['TEMP'] = $script:TempRoot; $info.Environment['TMP'] = $script:TempRoot; $info.Environment['LOCALAPPDATA'] = $script:LocalRoot
    if ($RawArguments) { Require (@($Arguments).Count -eq 0) 'Mixed argument forms'; $info.Arguments = $RawArguments } else { foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) } }
    $record = [ordered]@{ executable = $Executable; executable_sha256 = Hash $Executable; arguments = $Arguments; raw_arguments = $RawArguments; started_at_utc = [DateTime]::UtcNow.ToString('o'); status = 'RUNNING'; awaited_report = $AwaitReport }
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info; $null = $process.Start(); $record.process_id = $process.Id
    $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync(); $timer = [Diagnostics.Stopwatch]::StartNew()
    Write-Host "[$Name] started PID $($process.Id)"
    try {
        do {
            $exited = $process.WaitForExit(250); $reported = -not $AwaitReport
            if ($AwaitReport -and (Test-Path -LiteralPath $AwaitReport)) { try { $reported = (Json $AwaitReport).result -in @('Succeeded', 'Failed', 'Cancelled', 'Unknown') } catch { $reported = $false } }
            if ($exited -and $stdout.IsCompleted -and $stderr.IsCompleted -and ($reported -or $process.ExitCode -ne 0)) { break }
            if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) { $record.status = 'TIMED_OUT_PROCESS_NOT_KILLED'; throw "$Name timed out; process was not killed; inspect retained evidence before retrying." }
            if ($exited) { Start-Sleep -Milliseconds 100 }
        } while ($true)
        $record.exit_code = $process.ExitCode; $record.status = 'EXITED'
        [IO.File]::WriteAllText((Join-Path $script:RunRoot "$Name.stdout.log"), $stdout.GetAwaiter().GetResult(), [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllText((Join-Path $script:RunRoot "$Name.stderr.log"), $stderr.GetAwaiter().GetResult(), [Text.UTF8Encoding]::new($false))
        Require ($process.ExitCode -eq $ExpectedExit) "$Name exited $($process.ExitCode), expected $ExpectedExit; inspect logs"
    } finally { Save "$Name.process.json" $record; $process.Dispose() }
}

function CopyNew([string]$Source, [string]$Destination) {
    $null = Inside $Destination $script:RunRoot
    Require (-not (Test-Path -LiteralPath $Destination)) "Copy destination exists: $Destination"
    $null = New-Item -ItemType Directory -Path (Split-Path -Parent $Destination) -Force
    [IO.File]::Copy($Source, $Destination, $false)
    Require ((Hash $Source) -ceq (Hash $Destination)) "Copied bytes differ: $Destination"
}

function BuildPackage([string]$Label, [string]$Config, [string]$Compiler, [string]$Runtime, [string]$Bridge, [string]$Payload) {
    $setup = Join-Path $script:RunRoot "$Label-Setup.exe"
    $source = @(Snapshot $Payload); Save "$Label.source.json" $source
    $arguments = @('build', '--config', $Config, '--output', $setup)
    InvokeProcess $Compiler $arguments "build-$Label"
    $built = Json (Join-Path $script:RunRoot "build-$Label.stdout.log")
    Require ((SamePath $built.runtime_path (Join-Path $KitRoot 'avelin-app.exe')) -and (SamePath $built.bridge_path (Join-Path $KitRoot 'avelin_nsis_bridge.dll')) -and (SamePath $built.nsis_path (Join-Path $KitRoot 'tools/nsis/makensis.exe'))) 'Build did not select all three bundled components'
    $stream = [IO.File]::OpenRead($setup)
    try {
        $null = $stream.Seek(-72, [IO.SeekOrigin]::End); $footer = ReadBytes $stream 72
        Require ([Text.Encoding]::ASCII.GetString($footer, 56, 16) -ceq 'AVELIN-M0-PACK01') 'Expected an unsigned package footer'
        $start = [BitConverter]::ToUInt64($footer, 0); $backendSize = [BitConverter]::ToUInt64($footer, 8); $metadataSize = [BitConverter]::ToUInt64($footer, 16)
        Require ($metadataSize -gt 0 -and $metadataSize -le 32MB -and $start + $backendSize + $metadataSize -eq $stream.Length - 72) 'Invalid package metadata bounds'
        $null = $stream.Seek([long]($start + $backendSize), [IO.SeekOrigin]::Begin); $metadata = ReadBytes $stream ([int]$metadataSize)
    } finally { $stream.Dispose() }
    $manifest = [Text.UTF8Encoding]::new($false, $true).GetString($metadata) | ConvertFrom-Json
    [IO.File]::WriteAllBytes((Join-Path $script:RunRoot "$Label.embedded-manifest.json"), $metadata)
    Require ($manifest.runtime_sha256 -ceq (Hash $Runtime) -and @($manifest.files).Count -eq $source.Count) 'Packaged runtime/inventory differs from source'
    foreach ($file in $source) {
        $found = @($manifest.files | Where-Object path -CEQ $file.path)
        Require ($found.Count -eq 1 -and $found[0].sha256 -ceq $file.sha256 -and $found[0].size -eq $file.size) 'Packaged payload differs from independent source'
    }
    [pscustomobject]@{ label = $Label; setup = $setup; setup_sha256 = Hash $setup; manifest = $manifest; manifest_sha256 = HashBytes $metadata; runtime_sha256 = Hash $Runtime; source_files = $source; source_directory = $Payload }
}

function OperationReport([string]$Path, [string]$Operation, [string]$Label, [object]$Package) {
    $report = Json $Path
    Require ($report.result -ceq 'Succeeded' -and $report.exit_code -eq 0 -and $report.coordinator_exit_code -eq 0 -and -not $report.product_lock_retained) "$Label did not confirm success"
    Require ((SamePath $report.install_dir $script:InstallRoot) -and (SamePath $report.report_path $Path) -and @($report.events).Count -gt 0) 'Report destination/events mismatch'
    foreach ($event in $report.events) { Require ($event.task_id -ceq $report.task_id) 'Cross-task report event' }
    $session = Inside $report.session_dir $script:RunRoot; $plan = Json (Join-Path $session 'install-plan.json'); $identity = Json (Join-Path $session 'session.json')
    Require ($plan.task_id -ceq $report.task_id -and $plan.operation -ceq $Operation -and $identity.operation -ceq $Operation -and (SamePath $plan.install_dir $script:InstallRoot) -and $plan.app_id -ceq $script:AppId) 'Plan operation/identity mismatch'
    Require ($plan.manifest_sha256 -ceq $Package.manifest_sha256 -and $identity.manifest_sha256 -ceq $Package.manifest_sha256 -and $plan.user_sid -ceq $script:UserSid -and $identity.task_id -ceq $plan.task_id) 'Plan package/SID binding mismatch'
    Require ($plan.payload_file_count -eq $Package.source_files.Count -and $plan.payload_bytes -eq ($Package.source_files | Measure-Object size -Sum).Sum) 'Plan independent source count mismatch'
    Require (SamePath $identity.product_lock $script:LockPath) 'Unexpected product lock identity'
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while ((Test-Path -LiteralPath $script:LockPath) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 100 }
    Require (-not (Test-Path -LiteralPath $script:LockPath)) 'Product lock was not released'
    Save "$Label.evidence.json" @{ report_path = $Path; report_sha256 = Hash $Path; plan = $plan; session_identity = $identity; product_lock_released = $true }
    return $report
}

function VerifyInstalled([object]$Package, [object]$Report, [string]$Label, [int]$ShortcutCount) {
    AssertFiles $script:InstallRoot $Package.source_files
    $receiptPath = Join-Path $script:InstallRoot '.avelin-receipt.json'; $receipt = Json $receiptPath
    Require ($receipt.app_id -ceq $script:AppId -and $receipt.app_version -ceq $Package.manifest.app_version -and $receipt.task_id -ceq $Report.task_id -and $receipt.manifest_sha256 -ceq $Package.manifest_sha256 -and $receipt.user_sid -ceq $script:UserSid -and (SamePath $receipt.install_dir $script:InstallRoot)) 'Receipt identity mismatch'
    Require (@($receipt.files).Count -eq $Package.source_files.Count -and @($receipt.shortcuts).Count -eq $ShortcutCount) 'Receipt resource count mismatch'
    foreach ($file in $Package.source_files) {
        $found = @($receipt.files | Where-Object path -CEQ $file.path)
        Require ($found.Count -eq 1 -and $found[0].sha256 -ceq $file.sha256 -and $found[0].size -eq $file.size) 'Receipt differs from independent source'
    }
    $resident = Join-Path $script:InstallRoot '.avelin\avelin-app.exe'; $uninstaller = Join-Path $script:InstallRoot 'uninstall.exe'; $installedManifest = Join-Path $script:InstallRoot '.avelin\manifest.json'
    Require ((Hash $resident) -ceq $Package.runtime_sha256 -and (Hash $uninstaller) -ceq $receipt.uninstaller_sha256 -and (Hash $installedManifest) -ceq (Hash ([IO.Path]::ChangeExtension($Package.setup, 'manifest.json')))) 'Installed runtime/uninstaller/manifest mismatch'
    $shell = New-Object -ComObject WScript.Shell
    try {
        $linkEvidence = foreach ($record in $receipt.shortcuts) {
            $expected = switch ($record.kind) { 'desktop' { $script:DesktopPath }; 'start_menu' { $script:StartMenuPath }; default { throw 'Unexpected shortcut kind' } }
            Require ((SamePath $record.path $expected) -and (Hash $record.path) -ceq $record.sha256 -and (SamePath $record.target (Join-Path $script:InstallRoot $Package.manifest.entry))) 'Shortcut ownership mismatch'
            $link = $shell.CreateShortcut($record.path)
            try {
                Require ((SamePath $link.TargetPath $record.target) -and $link.Arguments -ceq '' -and (SamePath $link.WorkingDirectory $script:InstallRoot)) 'COM shortcut target/arguments/workdir mismatch'
                [pscustomobject]@{ kind = $record.kind; path = $record.path; sha256 = Hash $record.path; target = $link.TargetPath; arguments = $link.Arguments; working_directory = $link.WorkingDirectory }
            } finally { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($link) }
        }
    } finally { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) }
    $registrations = @(RegistryState | Where-Object exists); Require ($registrations.Count -gt 0) 'Registration missing'
    foreach ($registration in $registrations) {
        $v = $registration.values
        $normal = '"{0}" --gui --operation uninstall --backend "{1}" --manifest "{2}" --install-dir "{3}"' -f $resident, $uninstaller, $installedManifest, $script:InstallRoot
        $quiet = $normal.Replace(' --gui ', ' --silent ')
        Require ($v.UninstallString -ceq $normal -and $v.QuietUninstallString -ceq $quiet -and $v.DisplayVersion -ceq $Package.manifest.app_version -and $v.DisplayName -ceq $Package.manifest.app_name -and (SamePath $v.InstallLocation $script:InstallRoot)) 'Registered command or version identity mismatch'
    }
    CopyNew $receiptPath (Join-Path $script:RunRoot "$Label.receipt.json")
    Save "$Label.shortcuts.json" @($linkEvidence); Save "$Label.registry.json" $registrations
    return $receipt
}

Require ($IsWindows -and [Environment]::Is64BitProcess) 'Windows x64 PowerShell is required'
$KitRoot = Full $KitRoot
$script:RunRoot = Full $OutputDirectory
$null = Inside (Join-Path $script:RunRoot 'path-probe') $script:RunRoot
Require (-not (Test-Path -LiteralPath $script:RunRoot)) 'OutputDirectory must be new; previous evidence is retained'
$integrity = & (Join-Path $KitRoot 'scripts/Verify-DevKit.ps1') -KitRoot $KitRoot | ConvertFrom-Json
Require ($integrity.status -ceq 'PASS') 'Kit integrity failed'
$null = [IO.Directory]::CreateDirectory($script:RunRoot)
$script:TempRoot = Join-Path $script:RunRoot 'temp'; $script:LocalRoot = Join-Path $script:RunRoot 'local-app-data'
foreach ($path in @($script:TempRoot,$script:LocalRoot)) { $null = [IO.Directory]::CreateDirectory($path) }
$script:AppId = 'dev.avelin.m4.' + [Guid]::NewGuid().ToString('N')
$script:InstallRoot = Join-Path $script:RunRoot '安装 中文 M4'
$script:RegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:AppId
$script:UserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$script:LockPath = Join-Path $script:LocalRoot ('Avelin\M0\locks\' + $script:AppId + '-' + (HashBytes ([Text.Encoding]::UTF8.GetBytes($script:UserSid))).Substring(0,16) + '.lock')
$desktopFolder = [Environment]::GetFolderPath('DesktopDirectory'); $programsFolder = [Environment]::GetFolderPath('Programs')
$script:DesktopPath = Inside (Join-Path $desktopFolder ($script:AppId + '.lnk')) $desktopFolder
$script:StartMenuPath = Inside (Join-Path $programsFolder ($script:AppId + '.lnk')) $programsFolder
$cli = Join-Path $KitRoot 'avelin-cli.exe'; $runtime = Join-Path $KitRoot 'avelin-app.exe'; $bridge = Join-Path $KitRoot 'avelin_nsis_bridge.dll'
$summary = [ordered]@{schema_version=1;status='RUNNING';kit_root=$KitRoot;run_directory=$script:RunRoot;app_id=$script:AppId;kit_integrity=$integrity;scope='Portable developer toolkit normal cycle using a synthetic Nimbus payload, not two real-product integrations';environment_scope='Current user on this Windows host; child PATH contains only Windows/System32 and Windows; NSIS_MAKENSIS removed';gui_executed=$false;performance_measured=$false}
try {
    Require (@(RegistryState | Where-Object exists).Count -eq 0 -and -not (Test-Path -LiteralPath $script:DesktopPath) -and -not (Test-Path -LiteralPath $script:StartMenuPath)) 'Unique fixture resources already exist'
    $packages = @{}
    foreach ($label in @('a','b')) {
        $project = Join-Path $script:RunRoot "project-$label 中文"
        $payload = Join-Path $project 'dist'
        $entry = if ($label -ceq 'a') { 'app.exe' } else { 'bin\app.exe' }
        $version = if ($label -ceq 'a') { '1.0.0' } else { '1.1.0' }
        CopyNew (Join-Path $KitRoot 'sample/nimbus-demo.exe') (Join-Path $payload $entry)
        $data = @{ 'shared.txt' = "Shared version $version"; 'unchanged.txt' = 'Same payload content' }
        if ($label -ceq 'a') { $data['obsolete\retired.txt'] = 'Old owned content' } else { $data['added.txt'] = 'New owned content' }
        foreach ($relative in $data.Keys) {
            $path = Inside (Join-Path $payload $relative) $script:RunRoot
            $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $path))
            [IO.File]::WriteAllText($path,$data[$relative],[Text.UTF8Encoding]::new($false))
        }
        $initArgs = @('init','--directory',$project,'--app-id',$script:AppId,'--name','M4 中文样例','--app-version',$version,'--entry',$entry,'--payload',$payload)
        InvokeProcess $cli $initArgs "init-$label"
        $config = Join-Path $project 'avelin.toml'
        $initialized = Json (Join-Path $script:RunRoot "init-$label.stdout.log")
        Require ((SamePath $initialized.config $config) -and (SamePath $initialized.payload $payload)) 'Init output paths differ'
        Require ($initialized.payload_path_mode -ceq 'relative_to_config' -and [IO.File]::ReadAllText($config) -match '(?m)^directory = "dist"\s*$') 'In-project payload configuration should contain the relative dist directory'
        $before = @(Snapshot $project)
        InvokeProcess $cli $initArgs "init-$label-reject-existing" '' '' 1
        AssertFiles $project $before -Exact
        InvokeProcess $cli @('check','--config',$config) "check-$label"
        $packages[$label] = BuildPackage $label $config $cli $runtime $bridge $payload
        $builtFiles = @($packages[$label].setup,[IO.Path]::ChangeExtension($packages[$label].setup,'backend.exe'),[IO.Path]::ChangeExtension($packages[$label].setup,'manifest.json'))
        $hashes = @($builtFiles | ForEach-Object { Hash $_ })
        InvokeProcess $cli @('build','--config',$config,'--output',$packages[$label].setup) "build-$label-reject-existing" '' '' 1
        for ($i=0; $i -lt $builtFiles.Count; $i++) { Require ((Hash $builtFiles[$i]) -ceq $hashes[$i]) 'Existing build output was modified' }
    }
    $a = $packages.a; $b = $packages.b; $summary.packages = @($a,$b)
    $null = [IO.Directory]::CreateDirectory((Join-Path $script:InstallRoot 'user-data'))
    $sentinels = @(foreach ($relative in @('user-kept.txt','user-data\用户文件.txt')) {
        $path = Inside (Join-Path $script:InstallRoot $relative) $script:InstallRoot
        [IO.File]::WriteAllText($path,'Unmanaged content '+$script:AppId,[Text.UTF8Encoding]::new($false))
        @{path=$relative;size=(Get-Item -LiteralPath $path).Length;sha256=(Hash $path)}
    })
    Save 'sentinels.json' $sentinels
    $installReport = Join-Path $script:RunRoot 'install-a.report.json'
    InvokeProcess $a.setup @('--silent','--install-dir',$script:InstallRoot,'--report',$installReport) 'install-a' $installReport
    $installed = OperationReport $installReport 'install' 'install-a' $a
    $receiptA = VerifyInstalled $a $installed 'installed-a' 1
    Require ($receiptA.shortcuts[0].kind -ceq 'start_menu' -and -not (Test-Path -LiteralPath $script:DesktopPath)) 'Init shortcut defaults differ'
    AssertFiles $script:InstallRoot $sentinels
    $upgradeReport = Join-Path $script:RunRoot 'upgrade.report.json'
    InvokeProcess $b.setup @('--silent','--operation','upgrade','--install-dir',$script:InstallRoot,'--report',$upgradeReport) 'upgrade' $upgradeReport
    $upgraded = OperationReport $upgradeReport 'upgrade' 'upgrade' $b
    $receiptB = VerifyInstalled $b $upgraded 'upgraded-b' 1
    Require ($receiptB.installation_id -ceq $receiptA.installation_id -and $receiptB.previous_commit_id -ceq $receiptA.commit_id -and $receiptB.commit_id -cne $receiptA.commit_id -and $receiptB.task_id -cne $receiptA.task_id) 'Upgrade installation/commit lineage differs'
    foreach ($removed in @('app.exe','obsolete\retired.txt','obsolete','.avelin\upgrade-state.json')) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $removed))) "Unexpected obsolete resource: $removed" }
    AssertFiles $script:InstallRoot $sentinels
    $journal = @([IO.File]::ReadAllLines((Join-Path $upgraded.session_dir 'upgrade-journal.jsonl')) | ForEach-Object { $_ | ConvertFrom-Json })
    Require ($journal[-1].event -ceq 'committed' -and $journal[-1].result -ceq 'verified' -and $journal[-1].task_id -ceq $upgraded.task_id) 'Upgrade journal lacks a verified terminal commit'
    InvokeProcess (Join-Path $script:InstallRoot $b.manifest.entry) @() 'entry-b'
    Require ([IO.File]::ReadAllText((Join-Path $script:RunRoot 'entry-b.stdout.log')).Replace("`r`n","`n") -ceq "Nimbus Notes — Avelin M0 sample application`nThis executable is a local installation test payload.`n" -and (Get-Item -LiteralPath (Join-Path $script:RunRoot 'entry-b.stderr.log')).Length -eq 0) 'Sample execution contract differs'
    $registration = @(RegistryState | Where-Object exists)[0].values
    $resident = Join-Path $script:InstallRoot '.avelin\avelin-app.exe'; $uninstallReport = Join-Path $script:RunRoot 'uninstall-b.report.json'
    $arguments = $registration.QuietUninstallString.Substring(('"'+$resident+'"').Length).TrimStart() + ' --report "' + $uninstallReport + '"'
    InvokeProcess $resident @() 'uninstall-b' $uninstallReport $arguments
    $uninstalled = OperationReport $uninstallReport 'uninstall' 'uninstall-b' $b
    AssertFiles $script:InstallRoot $sentinels -Exact
    foreach ($removed in @('.avelin-receipt.json','uninstall.exe','.avelin')) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $removed))) "Internal resource remains: $removed" }
    Require (-not (Test-Path -LiteralPath $script:StartMenuPath) -and -not (Test-Path -LiteralPath $script:DesktopPath) -and @(RegistryState | Where-Object exists).Count -eq 0) 'Uninstall left owned shortcuts or registration'
    $summary.final_files = @(Snapshot $script:InstallRoot)
    $summary.final_registry = @(RegistryState)
    $summary.process_boundary = 'Launcher exit codes and stdout/stderr EOF observed. Uninstall result/backend/coordinator codes also checked against terminal report; copied coordinator OS exit code is not independently captured by this harness.'
    $summary.status = 'PASSED'
} catch { $summary.status = 'FAILED'; $summary.failure = $_.Exception.Message; throw }
finally { $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o'); Save 'summary.json' $summary; Write-Host "Evidence retained: $script:RunRoot" }
