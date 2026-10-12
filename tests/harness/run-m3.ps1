#requires -Version 7.2
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$CliPath,
    [Parameter(Mandatory)][string]$AppPath,
    [Parameter(Mandatory)][string]$BridgePath,
    [Parameter(Mandatory)][string]$PreviousCliPath,
    [Parameter(Mandatory)][string]$PreviousAppPath,
    [Parameter(Mandatory)][string]$PreviousBridgePath,
    [string]$ProjectRoot = (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)),
    [string]$NsisPath,
    [ValidateRange(10, 600)][int]$TimeoutSeconds = 120
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# Local copies of the established M2 process/report helpers; the frozen M2 harness is untouched.
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
function CopyTree([string]$Source, [string]$Destination) {
    $snapshot = @(Snapshot $Source)
    $null = New-Item -ItemType Directory -Path $Destination -Force
    foreach ($file in $snapshot) { CopyNew (Join-Path $Source $file.path) (Join-Path $Destination $file.path) }
    AssertFiles $Destination $snapshot -Exact
}
function ProductSnapshot {
    $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push($script:InstallRoot)
    $directories = @(while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            $safe = Inside $item.FullName $script:InstallRoot
            if ($item.PSIsContainer) { $pending.Push($safe); $safe }
        }
    }) | Sort-Object
    $links = foreach ($path in @($script:DesktopPath, $script:StartMenuPath)) {
        $exists = Test-Path -LiteralPath $path
        [pscustomobject][ordered]@{ path = $path; exists = $exists; size = $(if ($exists) { (Get-Item -LiteralPath $path).Length } else { 0 }); sha256 = $(if ($exists) { Hash $path } else { $null }) }
    }
    [pscustomobject][ordered]@{ files = @(Snapshot $script:InstallRoot); directories = @($directories); registry = @(RegistryState); alien_registry = @(RegistryState $script:AlienRegistrySubkey); shortcuts = @($links) }
}
function AssertUnchanged([object]$Before, [string]$Label) {
    $after = ProductSnapshot
    Save "$Label.after.json" $after
    Require (($Before | ConvertTo-Json -Depth 40 -Compress) -ceq ($after | ConvertTo-Json -Depth 40 -Compress)) "$Label changed installation, registration or shortcuts"
    Require (-not (Test-Path -LiteralPath $script:LockPath)) "$Label retained the product lock"
}
function RejectUpgrade([object]$Package, [string]$Label, [string]$ReasonPattern) {
    $before = ProductSnapshot; Save "$Label.before.json" $before
    $report = Join-Path $script:RunRoot "$Label.report.json"
    InvokeProcess $Package.setup @('--silent', '--operation', 'upgrade', '--install-dir', $script:InstallRoot, '--desktop-shortcut', 'false', '--start-menu-shortcut', 'true', '--report', $report) $Label '' '' 3
    $errorText = [IO.File]::ReadAllText((Join-Path $script:RunRoot "$Label.stderr.log"))
    Require ($errorText -match $ReasonPattern) "$Label failed for an unexpected reason; inspect stderr"
    if (Test-Path -LiteralPath $report) {
        $rejected = Json $report
        Require (-not ($rejected.PSObject.Properties.Name -contains 'result') -or $rejected.result -cne 'Succeeded') "$Label unexpectedly reported success"
    }
    AssertUnchanged $before $Label
    Save "$Label.evidence.json" @{ status = 'PASSED'; expected_exit_code = 3; expected_diagnostic_pattern = $ReasonPattern; product_resources_unchanged = $true; lock_released = $true }
}
function BuildPackage([string]$Label, [string]$Config, [string]$Compiler, [string]$Runtime, [string]$Bridge, [string]$Payload) {
    $setup = Join-Path $script:RunRoot "$Label-Setup.exe"
    $source = @(Snapshot $Payload); Save "$Label.source.json" $source
    $arguments = @('build', '--config', $Config, '--output', $setup, '--runtime', $Runtime, '--bridge', $Bridge)
    if ($NsisPath) { $arguments += @('--nsis', (Full $NsisPath)) }
    InvokeProcess $Compiler $arguments "build-$Label"
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
function ArchiveFixtureFile([string]$Path, [string]$Name, [string]$ExpectedHash) {
    $Path = Full $Path
    Require ($Path.StartsWith($script:RunRoot + '\', [StringComparison]::OrdinalIgnoreCase) -or (SamePath $Path $script:DesktopPath) -or (SamePath $Path $script:StartMenuPath)) 'Archive source escaped the fixture or its exact reserved shortcut paths'
    Require ((Hash $Path) -ceq $ExpectedHash) 'Fixture file changed unexpectedly; refusing to move it'
    $destination = Inside (Join-Path $script:RunRoot "preserved-user-edits\$Name") $script:RunRoot
    Require (-not (Test-Path -LiteralPath $destination)) 'Archive destination collision'
    $null = New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force
    Move-Item -LiteralPath $Path -Destination $destination
    Require ((Hash $destination) -ceq $ExpectedHash) 'Archived fixture bytes changed'
}
function VerifyUpgradePlan([object]$Report, [object]$A, [object]$B, [object]$BeforeReceipt, [string]$BeforeReceiptHash) {
    $plan = Json (Join-Path $Report.session_dir 'install-plan.json'); $upgrade = $plan.upgrade
    Require ($null -ne $upgrade -and $upgrade.previous_version -ceq $A.manifest.app_version -and $upgrade.previous_manifest_sha256 -ceq $A.manifest_sha256 -and $upgrade.previous_receipt_sha256 -ceq $BeforeReceiptHash) 'Upgrade does not bind the exact original package and receipt bytes'
    Require ($upgrade.previous_task_id -ceq $BeforeReceipt.task_id -and $upgrade.previous_commit_id -ceq $BeforeReceipt.commit_id -and $upgrade.installation_id -ceq $BeforeReceipt.installation_id) 'Upgrade source installation/commit/task mismatch'
    Require (-not $plan.desktop_shortcut -and $plan.start_menu_shortcut) 'Upgrade plan did not retain actual shortcut choices'
    $before = @{}; foreach ($file in $A.source_files) { $before[$file.path] = $file }
    $after = @{}; foreach ($file in $B.source_files) { $after[$file.path] = $file }
    $expected = @{ added = @(); replaced = @(); removed = @(); unchanged = @() }
    foreach ($file in $B.source_files) {
        if (-not $before.ContainsKey($file.path)) { $expected.added += $file }
        elseif ($before[$file.path].sha256 -ceq $file.sha256) { $expected.unchanged += $file }
        else { $expected.replaced += [pscustomobject]@{ before = $before[$file.path]; after = $file } }
    }
    foreach ($file in $A.source_files) { if (-not $after.ContainsKey($file.path)) { $expected.removed += $file } }
    foreach ($kind in @('added', 'replaced', 'removed', 'unchanged')) {
        $actual = @($upgrade.diff.$kind)
        Require ($actual.Count -eq @($expected[$kind]).Count) "Upgrade $kind count differs from independently computed diff"
        foreach ($file in $expected[$kind]) {
            if ($kind -eq 'replaced') {
                $found = @($actual | Where-Object { $_.after.path -ceq $file.after.path -and $_.before.sha256 -ceq $file.before.sha256 -and $_.after.sha256 -ceq $file.after.sha256 -and $_.before.size -eq $file.before.size -and $_.after.size -eq $file.after.size })
            } else { $found = @($actual | Where-Object { $_.path -ceq $file.path -and $_.size -eq $file.size -and $_.sha256 -ceq $file.sha256 }) }
            Require ($found.Count -eq 1) "Upgrade $kind metadata differs from independent source"
        }
    }
    $stage = Inside $upgrade.staging_dir $script:RunRoot; $backup = Inside $upgrade.backup_dir $script:RunRoot
    Require (-not (SamePath $stage $backup) -and -not $stage.StartsWith($script:InstallRoot + '\', [StringComparison]::OrdinalIgnoreCase) -and -not $backup.StartsWith($script:InstallRoot + '\', [StringComparison]::OrdinalIgnoreCase)) 'Upgrade staging/backup must be outside the installation'
    Require ($upgrade.required_space_bytes -ge ($B.source_files | Measure-Object size -Sum).Sum) 'Upgrade space requirement is smaller than incoming payload'
    Save 'upgrade-plan.independent.json' @{ status = 'PASSED'; upgrade = $upgrade; independent_diff = $expected; previous_receipt_sha256 = $BeforeReceiptHash }
}
function VerifyUpgradeEvidence([object]$Report, [object]$A, [object]$B, [object]$Receipt) {
    $session = Inside $Report.session_dir $script:RunRoot
    $plan = Json (Join-Path $session 'install-plan.json'); $inventory = Json (Join-Path $session 'upgrade-resources.json')
    $before = Json (Join-Path $script:RunRoot 'upgrade.before.json')
    Require ($inventory.schema_version -eq 1 -and $inventory.task_id -ceq $Report.task_id) 'Upgrade resource inventory identity mismatch'
    $expectedBackup = @{}; $known = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($relative in (@($A.source_files.path) + @('.avelin-receipt.json', '.avelin\manifest.json', '.avelin\avelin-app.exe', 'uninstall.exe'))) { $null = $known.Add((Full (Join-Path $script:InstallRoot $relative))) }
    $retiredPath = Full (Join-Path $script:InstallRoot 'retired-user.txt')
    foreach ($file in $before.files) {
        $absolute = Full (Join-Path $script:InstallRoot $file.path)
        if ($known.Contains($absolute) -and -not (SamePath $absolute $retiredPath)) { $expectedBackup[$absolute] = $file }
    }
    $expectedBackup[(Full $script:StartMenuPath)] = @($before.shortcuts | Where-Object { SamePath $_.path $script:StartMenuPath })[0]
    $backup = @( $inventory.backup ); Require ($backup.Count -eq $expectedBackup.Count) 'Backup inventory differs from independently observed old owned resources'
    $iniLines = [IO.File]::ReadAllLines((Join-Path $session 'upgrade-plan.ini'), [Text.Encoding]::Unicode)
    $sections = @{}; $section = $null
    foreach ($line in $iniLines) {
        if ($line -match '^\[([^\]]+)\]$') {
            $section = $Matches[1]; Require (-not $sections.ContainsKey($section)) 'Duplicate upgrade execution-plan section'
            $sections[$section] = @{}
        } elseif ($line -match '^([^=]+)=(.*)$') {
            Require ($null -ne $section) 'Upgrade execution-plan key outside a section'
            $key = $Matches[1]; $value = $Matches[2]
            Require (-not $sections[$section].ContainsKey($key)) "Duplicate upgrade execution-plan key in [$section]"
            $sections[$section][$key] = $value
        } else { Require ([string]::IsNullOrWhiteSpace($line)) 'Unexpected upgrade execution-plan syntax' }
    }
    Require ($sections.ContainsKey('upgrade') -and $sections.ContainsKey('old')) 'Missing upgrade execution-plan section'
    $ini = $sections.old
    Require ([int]$sections.upgrade.old_count -eq $backup.Count -and $ini.Count -eq 2 * $backup.Count) 'Execution plan backup count mismatch'
    $cleanupDirectories = @()
    if ($sections.ContainsKey('old_directories')) {
        $directories = $sections.old_directories; $directoryCount = [int]$directories['count']
        Require ($directories.ContainsKey('count') -and $directoryCount -ge 0 -and $directoryCount -le 10000 -and $directories.Keys.Count -eq $directoryCount + 1) 'Invalid optional old-directory inventory'
        $seen = @{}
        for ($i = 0; $i -lt $directoryCount; $i++) {
            $relative = [string]$directories["path$i"]
            Require (-not [IO.Path]::IsPathRooted($relative) -and $relative -notmatch '(^|[\\/])\.\.?([\\/]|$)') 'Old-directory cleanup candidate is not a safe relative path'
            $path = Inside (Join-Path $script:InstallRoot $relative) $script:InstallRoot
            Require (-not $seen.ContainsKey($path)) 'Duplicate old-directory cleanup candidate'
            Require (@($A.source_files | Where-Object { (Full (Join-Path $script:InstallRoot $_.path)).StartsWith($path + '\', [StringComparison]::OrdinalIgnoreCase) }).Count -gt 0) 'Old-directory cleanup candidate is not an original payload ancestor'
            Require (@($before.directories | Where-Object { SamePath $_ $path }).Count -eq 1) 'Old-directory cleanup candidate was not an original directory'
            $seen[$path] = $true; $cleanupDirectories += $path
        }
    }
    $backupProof = for ($i = 0; $i -lt $backup.Count; $i++) {
        $item = $backup[$i]; $key = Full $item.path
        Require ($expectedBackup.ContainsKey($key)) 'Upgrade attempted to own an unknown or modified resource'
        $observed = $expectedBackup[$key]
        Require ($item.sha256 -ceq $observed.sha256 -and $item.size -eq $observed.size -and (SamePath $ini["path$i"] $key) -and $ini["hash$i"] -ceq $item.sha256) 'Execution plan or resource inventory differs from pre-upgrade bytes'
        $saved = Inside (Join-Path $plan.upgrade.backup_dir "old-$i.bin") $script:RunRoot
        Require ((Hash $saved) -ceq $observed.sha256 -and (Get-Item -LiteralPath $saved).Length -eq $observed.size) 'Retained upgrade backup differs from original bytes'
        $expectedBackup.Remove($key)
        [pscustomobject]@{ index = $i; original_path = $key; backup_path = $saved; size = $observed.size; sha256 = $observed.sha256 }
    }
    Require ($expectedBackup.Count -eq 0 -and @(Snapshot $plan.upgrade.backup_dir).Count -eq $backup.Count) 'Missing or unexpected backup files'
    $preserved = @($inventory.preserved)
    Require ($preserved.Count -eq 2) 'Expected only the modified retired payload and unselected owned desktop link in preservation evidence'
    foreach ($expectedPath in @((Join-Path $script:InstallRoot 'retired-user.txt'), $script:DesktopPath)) {
        $item = @($preserved | Where-Object { SamePath $_.path $expectedPath })
        Require ($item.Count -eq 1 -and (Hash $expectedPath) -ceq $item[0].sha256 -and (Get-Item -LiteralPath $expectedPath).Length -eq $item[0].size) 'Modified retired resource was changed or incorrectly recorded'
    }
    $registeredBefore = @($before.registry | Where-Object exists)[0].values
    Require ($inventory.registration.display_name -ceq $registeredBefore.DisplayName -and $inventory.registration.display_version -ceq $registeredBefore.DisplayVersion -and $inventory.registration.uninstall_string -ceq $registeredBefore.UninstallString -and $inventory.registration.quiet_uninstall_string -ceq $registeredBefore.QuietUninstallString -and (SamePath $inventory.registration.install_location $script:InstallRoot)) 'Backup registration is not the actual original registration'
    $staged = [Collections.Generic.List[object]]::new()
    for ($i = 0; $i -lt @($B.manifest.files).Count; $i++) {
        $file = $B.manifest.files[$i]; $source = @($B.source_files | Where-Object path -CEQ $file.path)[0]
        $staged.Add([pscustomobject]@{ path = ('payload-{0:d6}' -f $i); size = $source.size; sha256 = $source.sha256 })
    }
    $receiptPath = Join-Path $script:InstallRoot '.avelin-receipt.json'
    foreach ($pair in @(@('runtime.exe', (Join-Path $script:InstallRoot '.avelin\avelin-app.exe')), @('manifest.json', (Join-Path $script:InstallRoot '.avelin\manifest.json')), @('uninstall.exe', (Join-Path $script:InstallRoot 'uninstall.exe')), @('receipt.json', $receiptPath), @('start-menu.lnk', $script:StartMenuPath))) {
        $staged.Add([pscustomobject]@{ path = $pair[0]; size = (Get-Item -LiteralPath $pair[1]).Length; sha256 = Hash $pair[1] })
    }
    AssertFiles $plan.upgrade.staging_dir $staged.ToArray() -Exact
    Require ((Hash (Join-Path $session 'previous-receipt.json')) -ceq $plan.upgrade.previous_receipt_sha256 -and (Hash (Join-Path $session 'previous-manifest.json')) -ceq (Hash ([IO.Path]::ChangeExtension($A.setup, 'manifest.json')))) 'Retained original receipt or manifest changed'
    $journalPath = Join-Path $session 'upgrade-journal.jsonl'
    $journal = @([IO.File]::ReadAllLines($journalPath) | ForEach-Object { $_ | ConvertFrom-Json })
    Require ($journal.Count -gt 0 -and $journal[0].event -ceq 'begin' -and $journal[-1].event -ceq 'committed' -and $journal[-1].result -ceq 'verified' -and (SamePath $journal[-1].path $receiptPath)) 'Upgrade journal has no final verified commit'
    $indexed = for ($i = 0; $i -lt $journal.Count; $i++) {
        $item = $journal[$i]
        Require ($item.schema_version -eq 1 -and $item.task_id -ceq $Report.task_id -and $item.time_ms -ge 0) 'Upgrade journal identity/time mismatch'
        [pscustomobject]@{ index = $i; record = $item }
    }
    Require (@($journal | Where-Object { $_.event -in @('committed', 'rolled_back', 'rollback_incomplete') }).Count -eq 1) 'Ambiguous upgrade terminal journal'
    $ready = @($indexed | Where-Object { $_.record.event -ceq 'staged' })
    $deletes = @($indexed | Where-Object { $_.record.event -ceq 'delete_intent' })
    Require ($ready.Count -eq 1 -and $deletes.Count -eq $backup.Count -and $ready[0].index -lt $deletes[0].index -and $ready[0].record.result -ceq 'all_new_resources_ready' -and $ready[0].record.sha256 -ceq (Hash $receiptPath) -and (SamePath $ready[0].record.path (Join-Path $plan.upgrade.staging_dir 'receipt.json'))) 'Old-resource deletion began before complete new staging'
    foreach ($proof in $backupProof) {
        $completed = @($indexed | Where-Object { $_.record.event -ceq 'backed_up' -and $_.record.result -ceq [string]$proof.index })
        $intent = @($indexed | Where-Object { $_.record.event -ceq 'backup_intent' -and $_.record.result -ceq [string]$proof.index })
        Require ($completed.Count -eq 1 -and $intent.Count -eq 1 -and $intent[0].index -lt $completed[0].index -and $completed[0].index -lt $deletes[0].index -and $completed[0].record.sha256 -ceq $proof.sha256 -and (SamePath $completed[0].record.path $proof.original_path)) 'A required backup was not completed before old resources were removed'
    }
    $published = @($indexed | Where-Object { $_.record.event -ceq 'published' })
    $commitIntent = @($indexed | Where-Object { $_.record.event -ceq 'commit_intent' })
    $registration = @($indexed | Where-Object { $_.record.event -ceq 'registry_updated' })
    Require ($published.Count -gt 0 -and (SamePath $published[-1].record.path $receiptPath) -and $published[-1].record.sha256 -ceq (Hash $receiptPath) -and $commitIntent.Count -eq 1 -and $commitIntent[0].record.result -ceq 'publish_complete_receipt_last' -and $commitIntent[0].index -lt $published[-1].index -and $registration.Count -eq 1 -and $registration[0].record.result -ceq 'known_fields_only' -and $registration[0].index -lt $commitIntent[0].index) 'The complete receipt was not the final published resource after registration'
    Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot '.avelin\upgrade-state.json'))) 'Successful upgrade left an unfinished marker'
    Save 'upgrade-resources.independent.json' @{ status = 'PASSED'; backups = @($backupProof); preserved = $preserved; staging = $staged.ToArray(); optional_old_directory_candidates = $cleanupDirectories; journal_path = $journalPath; journal_sha256 = Hash $journalPath; journal_records = $journal.Count; all_new_staged_before_old_deletion = $true; all_old_backed_up_before_deletion = $true; complete_receipt_published_last = $true; committed_once = $true; marker_absent = $true }
}

Require ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'Windows is required'
$ProjectRoot = Full $ProjectRoot
$CliPath = Full $CliPath; $AppPath = Full $AppPath; $BridgePath = Full $BridgePath
$PreviousCliPath = Full $PreviousCliPath; $PreviousAppPath = Full $PreviousAppPath; $PreviousBridgePath = Full $PreviousBridgePath
foreach ($binary in @($CliPath, $AppPath, $BridgePath, $PreviousCliPath, $PreviousAppPath, $PreviousBridgePath)) { Require (Test-Path -LiteralPath $binary -PathType Leaf) "Missing binary: $binary" }
Require ((Hash $AppPath) -cne (Hash $PreviousAppPath)) 'Use the frozen M2 runtime for A and the new M3 runtime for B; runtime replacement must be observable'
$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
$script:RunRoot = Inside (Join-Path $ProjectRoot "target\m3-harness\$runId") $ProjectRoot
Require (-not (Test-Path -LiteralPath $script:RunRoot)) 'Refusing to reuse a run directory'
$null = New-Item -ItemType Directory -Path $script:RunRoot
$script:TempRoot = Join-Path $script:RunRoot 'temp'; $script:LocalRoot = Join-Path $script:RunRoot 'local-app-data'; $fixture = Join-Path $script:RunRoot 'fixture'
$null = New-Item -ItemType Directory -Path $script:TempRoot, $script:LocalRoot
$script:AppId = 'dev.avelin.m3.harness.' + [Guid]::NewGuid().ToString('N'); $alienId = $script:AppId + '.alien'
$directoryName = 'Avelin M3 中文-' + $runId; $script:InstallRoot = Inside (Join-Path $script:RunRoot $directoryName) $script:RunRoot
$script:RegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:AppId
$script:AlienRegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $alienId
$script:UserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$script:LockPath = Join-Path $script:LocalRoot ('Avelin\M0\locks\' + $script:AppId + '-' + (HashBytes ([Text.Encoding]::UTF8.GetBytes($script:UserSid))).Substring(0, 16) + '.lock')
$desktopFolder = [Environment]::GetFolderPath([Environment+SpecialFolder]::DesktopDirectory); $programsFolder = [Environment]::GetFolderPath([Environment+SpecialFolder]::Programs)
$script:DesktopPath = Inside (Join-Path $desktopFolder ($script:AppId + '.lnk')) $desktopFolder
$script:StartMenuPath = Inside (Join-Path $programsFolder ($script:AppId + '.lnk')) $programsFolder
$summary = [ordered]@{ schema_version = 1; status = 'RUNNING'; app_id = $script:AppId; run_directory = $script:RunRoot; scope = 'Normal M2-compatible A to M3 B upgrade, preflight refusals, verified payload/shortcut/runtime replacement, user preservation, explicit sample launch and registered quiet uninstall. No fault injection, performance, signing, independent environment or GUI automation.'; binaries = @(@($CliPath, $AppPath, $BridgePath, $PreviousCliPath, $PreviousAppPath, $PreviousBridgePath) | ForEach-Object { @{ path = $_; sha256 = Hash $_ } }) }
try {
    Require (@(RegistryState | Where-Object exists).Count -eq 0 -and @(RegistryState $script:AlienRegistrySubkey | Where-Object exists).Count -eq 0 -and -not (Test-Path -LiteralPath $script:DesktopPath) -and -not (Test-Path -LiteralPath $script:StartMenuPath)) 'Unique installation resources already exist'
    CopyTree (Join-Path $ProjectRoot 'examples\m3') $fixture
    CopyNew (Join-Path $ProjectRoot 'examples\nimbus\branding\nimbus.png') (Join-Path $fixture 'branding\nimbus.png')
    $sample = Join-Path $ProjectRoot 'examples\nimbus\payload\nimbus-demo.exe'
    CopyNew $sample (Join-Path $fixture 'payload-a\nimbus-a.exe'); CopyNew $sample (Join-Path $fixture 'payload-b\bin\nimbus-b.exe')
    foreach ($name in @('a', 'b')) {
        $path = Join-Path $fixture "$name.toml"; $config = [IO.File]::ReadAllText($path)
        Require ($config.Contains('id = "dev.avelin.m3.nimbus"') -and $config.Contains('directory_name = "AvelinM3Nimbus"')) 'M3 example identity changed; update harness'
        $config = $config.Replace('id = "dev.avelin.m3.nimbus"', ('id = "' + $script:AppId + '"')).Replace('directory_name = "AvelinM3Nimbus"', ('directory_name = "' + $directoryName + '"'))
        [IO.File]::WriteAllText($path, $config, [Text.UTF8Encoding]::new($false))
    }
    $alienConfig = Join-Path $fixture 'alien.toml'
    [IO.File]::WriteAllText($alienConfig, [IO.File]::ReadAllText((Join-Path $fixture 'b.toml')).Replace(('id = "' + $script:AppId + '"'), ('id = "' + $alienId + '"')), [Text.UTF8Encoding]::new($false))
    $a = BuildPackage 'a-old' (Join-Path $fixture 'a.toml') $PreviousCliPath $PreviousAppPath $PreviousBridgePath (Join-Path $fixture 'payload-a')
    $aProbe = BuildPackage 'a-probe' (Join-Path $fixture 'a.toml') $CliPath $AppPath $BridgePath (Join-Path $fixture 'payload-a')
    $b = BuildPackage 'b' (Join-Path $fixture 'b.toml') $CliPath $AppPath $BridgePath (Join-Path $fixture 'payload-b')
    $alien = BuildPackage 'b-alien' $alienConfig $CliPath $AppPath $BridgePath (Join-Path $fixture 'payload-b')
    $summary.packages = @($a, $aProbe, $b, $alien)
    $null = New-Item -ItemType Directory -Path (Join-Path $script:InstallRoot 'user-data') -Force
    $sentinels = foreach ($relative in @('user-kept.txt', 'user-data\用户文件.txt')) {
        $path = Inside (Join-Path $script:InstallRoot $relative) $script:InstallRoot
        [IO.File]::WriteAllText($path, "Unmanaged M3 fixture $runId", [Text.UTF8Encoding]::new($false))
        [pscustomobject]@{ path = $relative; size = (Get-Item -LiteralPath $path).Length; sha256 = Hash $path }
    }
    Save 'sentinels.json' @($sentinels)
    $installReport = Join-Path $script:RunRoot 'install-a.report.json'
    InvokeProcess $a.setup @('--silent', '--install-dir', $script:InstallRoot, '--report', $installReport) 'install-a' $installReport
    $installedA = OperationReport $installReport 'install' 'install-a' $a
    $receiptA = VerifyInstalled $a $installedA 'installed-a' 2
    AssertFiles $script:InstallRoot @($sentinels)
    $receiptAHash = Hash (Join-Path $script:RunRoot 'installed-a.receipt.json')
    CopyNew $script:StartMenuPath (Join-Path $script:RunRoot 'original-start-menu.lnk')
    RejectUpgrade $aProbe 'reject-same-version' '(?i)(newer|version|downgrade|版本)'
    RejectUpgrade $alien 'reject-identity' '(?i)(identity|app.?id|different.product|mismatch|upgrade requires an installed product)'

    $shared = Inside (Join-Path $script:InstallRoot 'shared.txt') $script:InstallRoot
    [IO.File]::AppendAllText($shared, "User modification $runId", [Text.UTF8Encoding]::new($false)); $sharedHash = Hash $shared
    RejectUpgrade $b 'reject-modified-replacement' '(?is)(?=.*shared\.txt)(?=.*(modified|hash|digest|changed|conflict|mismatch))'
    ArchiveFixtureFile $shared 'shared-modified.txt' $sharedHash
    CopyNew (Join-Path $a.source_directory 'shared.txt') $shared
    $collision = Inside (Join-Path $script:InstallRoot 'added.txt') $script:InstallRoot
    [IO.File]::WriteAllText($collision, "User-owned collision $runId", [Text.UTF8Encoding]::new($false)); $collisionHash = Hash $collision
    RejectUpgrade $b 'reject-unknown-collision' '(?is)(?=.*added\.txt)(?=.*(existing|owned|conflict|collision))'
    ArchiveFixtureFile $collision 'added-user-collision.txt' $collisionHash

    $shell = New-Object -ComObject WScript.Shell
    try {
        $link = $shell.CreateShortcut($script:StartMenuPath)
        try { $link.Description = "Modified selected M3 link $runId"; $link.Save() } finally { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($link) }
    } finally { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) }
    $selectedLinkHash = Hash $script:StartMenuPath
    Require ($selectedLinkHash -cne @($receiptA.shortcuts | Where-Object kind -CEQ 'start_menu')[0].sha256) 'Selected-link modification did not alter its bytes'
    RejectUpgrade $b 'reject-modified-selected-shortcut' '(?i)(shortcut|\.lnk)'
    ArchiveFixtureFile $script:StartMenuPath 'selected-link-modified.lnk' $selectedLinkHash
    # Restore exactly the fixture's original link; no existing destination is overwritten.
    [IO.File]::Copy((Join-Path $script:RunRoot 'original-start-menu.lnk'), $script:StartMenuPath, $false)
    Require ((Hash $script:StartMenuPath) -ceq @($receiptA.shortcuts | Where-Object kind -CEQ 'start_menu')[0].sha256) 'Restored fixture link changed bytes'

    $retired = Inside (Join-Path $script:InstallRoot 'retired-user.txt') $script:InstallRoot
    [IO.File]::AppendAllText($retired, "Retired resource user edit $runId", [Text.UTF8Encoding]::new($false))
    $retiredEvidence = [pscustomobject]@{ path = 'retired-user.txt'; size = (Get-Item -LiteralPath $retired).Length; sha256 = Hash $retired }
    $shell = New-Object -ComObject WScript.Shell
    try {
        $link = $shell.CreateShortcut($script:DesktopPath)
        try { $link.Description = "Modified unselected M3 link $runId"; $link.Save() } finally { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($link) }
    } finally { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) }
    $desktopModifiedHash = Hash $script:DesktopPath
    Require ($desktopModifiedHash -cne @($receiptA.shortcuts | Where-Object kind -CEQ 'desktop')[0].sha256) 'Unselected-link modification did not alter its bytes'
    Save 'upgrade.before.json' (ProductSnapshot)
    Save 'preserved.expected.json' @{ user_files = @($sentinels); retired_file = $retiredEvidence; unselected_desktop = @{ path = $script:DesktopPath; sha256 = $desktopModifiedHash } }
    $upgradeReport = Join-Path $script:RunRoot 'upgrade.report.json'
    InvokeProcess $b.setup @('--silent', '--operation', 'upgrade', '--install-dir', $script:InstallRoot, '--desktop-shortcut', 'false', '--start-menu-shortcut', 'true', '--report', $upgradeReport) 'upgrade' $upgradeReport
    $upgraded = OperationReport $upgradeReport 'upgrade' 'upgrade' $b
    VerifyUpgradePlan $upgraded $a $b $receiptA $receiptAHash
    $receiptB = VerifyInstalled $b $upgraded 'upgraded-b' 1
    VerifyUpgradeEvidence $upgraded $a $b $receiptB
    Require ($receiptB.installation_id -ceq $receiptA.installation_id -and $receiptB.previous_commit_id -ceq $receiptA.commit_id -and $receiptB.commit_id -cne $receiptA.commit_id -and $receiptB.task_id -cne $receiptA.task_id) 'Upgrade installation/commit lineage mismatch'
    Require ($receiptB.uninstaller_sha256 -cne $receiptA.uninstaller_sha256 -and $b.runtime_sha256 -cne $a.runtime_sha256) 'Upgrade did not replace uninstall components'
    Require ($receiptB.shortcuts[0].kind -ceq 'start_menu' -and (Hash $script:DesktopPath) -ceq $desktopModifiedHash) 'Selected link was not replaced or modified unselected link was changed'
    foreach ($removed in @('nimbus-a.exe', 'obsolete\retired.txt', 'obsolete')) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $removed))) "Obsolete resource or empty directory remains: $removed" }
    AssertFiles $script:InstallRoot (@($sentinels) + @($retiredEvidence))
    Save 'upgrade.after.json' (ProductSnapshot)
    $beforeLaunch = ProductSnapshot
    InvokeProcess (Join-Path $script:InstallRoot $b.manifest.entry) @() 'entry-b'
    Require ([IO.File]::ReadAllText((Join-Path $script:RunRoot 'entry-b.stdout.log')).Replace("`r`n", "`n") -ceq "Nimbus Notes — Avelin M0 sample application`nThis executable is a local installation test payload.`n" -and (Get-Item -LiteralPath (Join-Path $script:RunRoot 'entry-b.stderr.log')).Length -eq 0) 'Upgraded entry stdout/stderr differs from the sample contract'
    AssertUnchanged $beforeLaunch 'entry-b'
    RejectUpgrade $aProbe 'reject-downgrade' '(?i)(newer|version|downgrade|版本)'

    $registration = @(RegistryState | Where-Object exists)[0].values
    $resident = Join-Path $script:InstallRoot '.avelin\avelin-app.exe'; $uninstallReport = Join-Path $script:RunRoot 'uninstall-b.report.json'
    $arguments = $registration.QuietUninstallString.Substring(('"' + $resident + '"').Length).TrimStart() + ' --report "' + $uninstallReport + '"'
    InvokeProcess $resident @() 'uninstall-b' $uninstallReport $arguments
    $null = OperationReport $uninstallReport 'uninstall' 'uninstall-b' $b
    AssertFiles $script:InstallRoot (@($sentinels) + @($retiredEvidence)) -Exact
    foreach ($removed in @('.avelin-receipt.json', 'uninstall.exe', '.avelin')) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $removed))) "Internal resource remains: $removed" }
    Require (-not (Test-Path -LiteralPath $script:StartMenuPath) -and (Hash $script:DesktopPath) -ceq $desktopModifiedHash -and @(RegistryState | Where-Object exists).Count -eq 0) 'Final links/registration differ from expected preservation'
    Save 'uninstall.after.json' (ProductSnapshot)
    ArchiveFixtureFile $script:DesktopPath 'unselected-desktop-preserved.lnk' $desktopModifiedHash
    Save 'preserved-desktop-archive.json' @{ original_path = $script:DesktopPath; preserved_after_upgrade_and_uninstall = $true; archived_path = (Join-Path $script:RunRoot 'preserved-user-edits\unselected-desktop-preserved.lnk'); sha256 = $desktopModifiedHash }
    Save 'gui-fixture.json' @{ schema_version = 1; app_id = $script:AppId; setup_a = $a.setup; setup_a_sha256 = $a.setup_sha256; setup_b = $b.setup; setup_b_sha256 = $b.setup_sha256; manifest_a = [IO.Path]::ChangeExtension($a.setup, 'manifest.json'); manifest_b = [IO.Path]::ChangeExtension($b.setup, 'manifest.json'); manifest_a_sha256 = Hash ([IO.Path]::ChangeExtension($a.setup, 'manifest.json')); manifest_b_sha256 = Hash ([IO.Path]::ChangeExtension($b.setup, 'manifest.json')); source_a = $a.source_directory; source_b = $b.source_directory; install_dir = (Join-Path $script:RunRoot ('GUI 升级-' + $runId)); temp = $script:TempRoot; local_app_data = $script:LocalRoot; desktop_path = $script:DesktopPath; start_menu_path = $script:StartMenuPath; lock_path = $script:LockPath; scope = 'Prepare A silently, observe one actual GUI upgrade and explicit Launch, then Finish using registered B quiet uninstall. No scripted visual acceptance.' }
    $summary.status = 'PASSED'
} catch { $summary.status = 'FAILED'; $summary.failure = $_.Exception.Message; throw }
finally { $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o'); Save 'summary.json' $summary; Write-Host "Evidence retained: $script:RunRoot" }
