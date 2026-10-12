#requires -Version 7.2
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$CliPath,
    [Parameter(Mandatory)][string]$AppPath,
    [Parameter(Mandatory)][string]$BridgePath,
    [string]$ProjectRoot = (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)),
    [string]$NsisPath,
    [ValidateRange(10, 600)][int]$TimeoutSeconds = 120
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
function RegistryState {
    foreach ($view in @([Microsoft.Win32.RegistryView]::Registry32, [Microsoft.Win32.RegistryView]::Registry64)) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $view); $key = $null
        try {
            $key = $base.OpenSubKey($script:RegistrySubkey, $false); $values = @{}
            if ($key) { foreach ($name in $key.GetValueNames()) { $values[$name] = $key.GetValue($name) } }
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
function OperationReport([string]$Path, [string]$Operation, [string]$EvidenceName = $Operation) {
    $report = Json $Path
    Require ($report.result -ceq 'Succeeded' -and $report.exit_code -eq 0 -and $report.coordinator_exit_code -eq 0 -and -not $report.product_lock_retained) "$Operation did not confirm success"
    Require ((SamePath $report.install_dir $script:InstallRoot) -and (SamePath $report.report_path $Path) -and @($report.events).Count -gt 0) 'Report destination/events mismatch'
    foreach ($event in $report.events) { Require ($event.task_id -ceq $report.task_id) 'Cross-task report event' }
    $session = Inside $report.session_dir $script:RunRoot; $plan = Json (Join-Path $session 'install-plan.json'); $identity = Json (Join-Path $session 'session.json')
    Require ($plan.task_id -ceq $report.task_id -and $plan.operation -ceq $Operation -and (SamePath $plan.install_dir $script:InstallRoot) -and $plan.app_id -ceq $script:AppId) 'Plan task/operation/target binding mismatch'
    Require ($plan.manifest_sha256 -ceq $script:ManifestHash -and $plan.payload_file_count -eq $script:SourceFiles.Count -and $plan.payload_bytes -eq ($script:SourceFiles | Measure-Object size -Sum).Sum) 'Plan package/count binding mismatch'
    Require ($plan.user_sid -ceq $script:UserSid -and $identity.task_id -ceq $plan.task_id -and $identity.operation -ceq $Operation -and $identity.manifest_sha256 -ceq $script:ManifestHash) 'Session identity mismatch'
    Require (SamePath $identity.product_lock $script:LockPath) 'Unexpected product lock identity'
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while ((Test-Path -LiteralPath $script:LockPath) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 100 }
    Require (-not (Test-Path -LiteralPath $script:LockPath)) 'Product lock was not released'
    Save "$EvidenceName.evidence.json" ([ordered]@{ report_path = $Path; report_sha256 = Hash $Path; plan = $plan; session_identity = $identity; product_lock_released = $true })
    return $report
}

Require ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'Windows is required'
$ProjectRoot = Full $ProjectRoot; $CliPath = Full $CliPath; $AppPath = Full $AppPath; $BridgePath = Full $BridgePath
foreach ($binary in @($CliPath, $AppPath, $BridgePath)) { Require (Test-Path -LiteralPath $binary -PathType Leaf) "Missing binary: $binary" }
$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
$script:RunRoot = Inside (Join-Path $ProjectRoot "target\m2-harness\$runId") $ProjectRoot
Require (-not (Test-Path -LiteralPath $script:RunRoot)) 'Refusing to reuse a run directory'
$null = New-Item -ItemType Directory -Path $script:RunRoot -Force
$script:TempRoot = Join-Path $script:RunRoot 'temp'; $script:LocalRoot = Join-Path $script:RunRoot 'local-app-data'; $fixture = Join-Path $script:RunRoot 'fixture'
$null = New-Item -ItemType Directory -Path $script:TempRoot, $script:LocalRoot, $fixture
$script:AppId = 'dev.avelin.m2.harness.' + [Guid]::NewGuid().ToString('N'); $directoryName = 'Avelin M2 中文-' + $runId
$script:InstallRoot = Inside (Join-Path $script:RunRoot $directoryName) $script:RunRoot
$script:RegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:AppId
$script:UserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$script:LockPath = Join-Path $script:LocalRoot ('Avelin\M0\locks\' + $script:AppId + '-' + (HashBytes ([Text.Encoding]::UTF8.GetBytes($script:UserSid))).Substring(0, 16) + '.lock')
$summary = [ordered]@{ schema_version = 1; status = 'RUNNING'; run_directory = $script:RunRoot; app_id = $script:AppId; scope = 'M2 actual silent install, shortcut targets/ownership, modified and unknown file preservation, collision refusal, safe reinstall and resident quiet uninstall; GUI recorded separately; no scale/performance/signing/independent environment'; binaries = @(@($CliPath, $AppPath, $BridgePath) | ForEach-Object { @{ path = $_; sha256 = Hash $_ } }) }
try {
    Require (@(RegistryState | Where-Object exists).Count -eq 0) 'Unique registry key already exists'
    $example = Inside (Join-Path $ProjectRoot 'examples\nimbus') $ProjectRoot
    foreach ($folder in @('payload', 'branding', 'locales')) {
        $source = Inside (Join-Path $example $folder) $example; $captured = @(Snapshot $source); $destination = Inside (Join-Path $fixture $folder) $fixture
        $null = New-Item -ItemType Directory -Path $destination
        foreach ($file in $captured) { $copy = Inside (Join-Path $destination $file.path) $destination; $null = New-Item -ItemType Directory -Path (Split-Path -Parent $copy) -Force; Copy-Item -LiteralPath (Join-Path $source $file.path) -Destination $copy }
        AssertFiles $destination $captured -Exact; Save "$folder.source.json" $captured
        if ($folder -eq 'payload') { $script:SourceFiles = $captured }
    }
    Require (@($script:SourceFiles | Where-Object path -CEQ 'nimbus-demo.exe').Count -eq 1) 'Build/copy nimbus-demo.exe before running this harness'
    $config = [IO.File]::ReadAllText((Join-Path $example 'avelin-m2.toml'))
    Require ($config.Contains('id = "dev.avelin.m2.nimbus"') -and $config.Contains('directory_name = "AvelinM2Nimbus"')) 'M2 template identity changed; update harness'
    $config = $config.Replace('id = "dev.avelin.m2.nimbus"', ('id = "' + $script:AppId + '"')).Replace('directory_name = "AvelinM2Nimbus"', ('directory_name = "' + $directoryName + '"'))
    $configPath = Join-Path $fixture 'avelin.toml'; [IO.File]::WriteAllText($configPath, $config, [Text.UTF8Encoding]::new($false))
    $setup = Join-Path $script:RunRoot 'M2Setup.exe'; $buildArguments = @('build', '--config', $configPath, '--output', $setup, '--runtime', $AppPath, '--bridge', $BridgePath)
    if ($NsisPath) { $buildArguments += @('--nsis', (Full $NsisPath)) }
    InvokeProcess $CliPath $buildArguments 'build'
    # Read the newly built unsigned footer's original metadata, avoiding JSON reserialization differences.
    $package = [IO.File]::OpenRead($setup)
    try {
        $null = $package.Seek(-72, [IO.SeekOrigin]::End); $footer = ReadBytes $package 72
        Require ([Text.Encoding]::ASCII.GetString($footer, 56, 16) -ceq 'AVELIN-M0-PACK01') 'Expected an unsigned package footer'
        $start = [BitConverter]::ToUInt64($footer, 0); $backendLength = [BitConverter]::ToUInt64($footer, 8); $metadataLength = [BitConverter]::ToUInt64($footer, 16)
        Require ($metadataLength -gt 0 -and $metadataLength -le 32MB -and $start + $backendLength + $metadataLength -eq $package.Length - 72) 'Invalid package metadata bounds'
        $null = $package.Seek([long]($start + $backendLength), [IO.SeekOrigin]::Begin); $metadata = ReadBytes $package ([int]$metadataLength)
    } finally { $package.Dispose() }
    $script:ManifestHash = HashBytes $metadata; $manifest = [Text.UTF8Encoding]::new($false, $true).GetString($metadata) | ConvertFrom-Json
    [IO.File]::WriteAllBytes((Join-Path $script:RunRoot 'embedded-manifest.json'), $metadata)
    Require ($manifest.app_id -ceq $script:AppId -and $manifest.presentation.template -ceq 'compact' -and $manifest.presentation.accent_color -ceq '#245DA8') 'Packaged brand configuration missing'
    Require ((HashBytes ([byte[]]$manifest.presentation.logo_png)) -ceq (Hash (Join-Path $fixture 'branding\nimbus.png')) -and $manifest.presentation.strings.'zh-CN'.'window.title' -ceq '安装 {app_name}') 'Embedded logo/translation differs from fixture'
    $summary.package = @{ path = $setup; sha256 = Hash $setup; manifest_sha256 = $script:ManifestHash; runtime_sha256 = $manifest.runtime_sha256 }
    Require ($manifest.desktop_shortcut -and $manifest.start_menu_shortcut) 'Packaged shortcut defaults missing'
    $desktopFolder = [Environment]::GetFolderPath([Environment+SpecialFolder]::DesktopDirectory)
    $programsFolder = [Environment]::GetFolderPath([Environment+SpecialFolder]::Programs)
    $expectedDesktop = Inside (Join-Path $desktopFolder ($script:AppId + '.lnk')) $desktopFolder
    $expectedStartMenu = Inside (Join-Path $programsFolder ($script:AppId + '.lnk')) $programsFolder
    Require (-not (Test-Path -LiteralPath $expectedDesktop) -and -not (Test-Path -LiteralPath $expectedStartMenu)) 'Unique shortcut destinations already exist; refusing this fixture'
    Save 'shortcuts.before.json' @{ desktop = $expectedDesktop; start_menu = $expectedStartMenu; both_absent = $true }
    $null = New-Item -ItemType Directory -Path (Join-Path $script:InstallRoot 'assets') -Force
    $sentinels = foreach ($relative in @('user-kept.txt', 'assets\user-created.txt')) {
        $path = Inside (Join-Path $script:InstallRoot $relative) $script:InstallRoot
        Require (-not (Test-Path -LiteralPath $path)) 'Sentinel collision'
        [IO.File]::WriteAllText($path, "Unmanaged M2 fixture $runId", [Text.UTF8Encoding]::new($false))
        [pscustomobject]@{ path = $relative; size = (Get-Item -LiteralPath $path).Length; sha256 = Hash $path }
    }
    Save 'sentinels.json' @($sentinels)
    $installReportPath = Join-Path $script:RunRoot 'install.report.json'
    InvokeProcess $setup @('--silent', '--install-dir', $script:InstallRoot, '--report', $installReportPath) 'install' $installReportPath
    $installed = OperationReport $installReportPath 'install'
    AssertFiles $script:InstallRoot $script:SourceFiles
    AssertFiles $script:InstallRoot @($sentinels)
    $receipt = Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    Require ($receipt.app_id -ceq $script:AppId -and $receipt.task_id -ceq $installed.task_id -and $receipt.manifest_sha256 -ceq $script:ManifestHash -and $receipt.user_sid -ceq $script:UserSid -and (SamePath $receipt.install_dir $script:InstallRoot) -and $receipt.app_version -ceq $manifest.app_version) 'Receipt identity mismatch'
    Require (@($receipt.files).Count -eq $script:SourceFiles.Count) 'Receipt file count differs from independent source'
    foreach ($file in $script:SourceFiles) {
        $recorded = @($receipt.files | Where-Object path -CEQ $file.path)
        Require ($recorded.Count -eq 1 -and $recorded[0].sha256 -ceq $file.sha256 -and $recorded[0].size -eq $file.size) 'Receipt file metadata differs from independent source'
    }
    Save 'installed.receipt.json' $receipt
    $resident = Join-Path $script:InstallRoot '.avelin\avelin-app.exe'
    $residentManifest = Join-Path $script:InstallRoot '.avelin\manifest.json'
    $uninstaller = Join-Path $script:InstallRoot 'uninstall.exe'
    Require ((Hash $resident) -ceq (Hash $AppPath) -and (Hash $uninstaller) -ceq $receipt.uninstaller_sha256) 'Resident runtime or uninstaller hash mismatch'
    Require ((Hash $residentManifest) -ceq (Hash ([IO.Path]::ChangeExtension($setup, 'manifest.json')))) 'Resident manifest differs from package'
    $beforeEntry = @(Snapshot $script:InstallRoot)
    InvokeProcess (Join-Path $script:InstallRoot 'nimbus-demo.exe') @() 'entry'
    Require ([IO.File]::ReadAllText((Join-Path $script:RunRoot 'entry.stdout.log')).Replace("`r`n", "`n") -ceq "Nimbus Notes — Avelin M0 sample application`nThis executable is a local installation test payload.`n" -and (Get-Item (Join-Path $script:RunRoot 'entry.stderr.log')).Length -eq 0) 'Nimbus stdout/stderr contract mismatch'
    AssertFiles $script:InstallRoot $beforeEntry -Exact
    $links = @($receipt.shortcuts)
    Require ($links.Count -eq 2) 'Expected two owned shortcut records'
    $shell = New-Object -ComObject WScript.Shell
    $desktop = @($links | Where-Object { $_.kind -ceq 'desktop' -and (SamePath $_.path $expectedDesktop) })
    $startMenu = @($links | Where-Object { $_.kind -ceq 'start_menu' -and (SamePath $_.path $expectedStartMenu) })
    Require ($desktop.Count -eq 1 -and $startMenu.Count -eq 1) 'Receipt paths/kinds must match independently reserved fixture shortcut names'
    $shortcutEvidence = foreach ($record in $links) {
        Require ((Test-Path -LiteralPath $record.path -PathType Leaf) -and (Hash $record.path) -ceq $record.sha256) 'Shortcut ownership hash mismatch'
        Require (SamePath $record.target (Join-Path $script:InstallRoot $manifest.entry)) 'Receipt shortcut target mismatch'
        $link = $shell.CreateShortcut($record.path)
        Require ((SamePath $link.TargetPath (Join-Path $script:InstallRoot $manifest.entry)) -and $link.Arguments -ceq '' -and (SamePath $link.WorkingDirectory $script:InstallRoot)) 'Shortcut target, arguments or working directory mismatch'
        [pscustomobject]@{ path = $record.path; sha256 = Hash $record.path; target = $link.TargetPath; arguments = $link.Arguments; working_directory = $link.WorkingDirectory }
    }
    Save 'shortcuts.created.json' @($shortcutEvidence)
    $registrations = @(RegistryState | Where-Object exists)
    Require ($registrations.Count -gt 0) 'Uninstall registration missing'
    foreach ($registration in $registrations) {
        $values = $registration.values
        $expectedNormal = '"{0}" --gui --operation uninstall --backend "{1}" --manifest "{2}" --install-dir "{3}"' -f $resident, $uninstaller, $residentManifest, $script:InstallRoot
        $expectedQuiet = '"{0}" --silent --operation uninstall --backend "{1}" --manifest "{2}" --install-dir "{3}"' -f $resident, $uninstaller, $residentManifest, $script:InstallRoot
        Require ($values.UninstallString -ceq $expectedNormal -and $values.QuietUninstallString -ceq $expectedQuiet) 'Registered entry mode or arguments differ from independently expected commands'
        Require ((SamePath $values.InstallLocation $script:InstallRoot) -and $values.DisplayVersion -ceq $manifest.app_version -and $values.DisplayName -ceq $manifest.app_name -and $values.NoModify -eq 1 -and $values.NoRepair -eq 1) 'Registered identity or capabilities mismatch'
    }
    Save 'installed.registry.json' $registrations
    # Modify a test-owned payload and shortcut. Uninstall must preserve both byte-for-byte.
    $modifiedPayload = Inside (Join-Path $script:InstallRoot 'hello.txt') $script:InstallRoot
    [IO.File]::AppendAllText($modifiedPayload, "`nUser edit M2 $runId", [Text.UTF8Encoding]::new($false))
    $modifiedLink = $shell.CreateShortcut($desktop[0].path)
    $modifiedLink.Description = "User customization M2 $runId"; $modifiedLink.Save()
    $modifiedLinkHash = Hash $desktop[0].path
    Require ($modifiedLinkHash -cne $desktop[0].sha256) 'Shortcut edit did not change bytes'
    $modified = [pscustomobject]@{ path = 'hello.txt'; size = (Get-Item -LiteralPath $modifiedPayload).Length; sha256 = Hash $modifiedPayload }
    $missingPayload = Inside (Join-Path $script:InstallRoot 'assets\readme.txt') $script:InstallRoot
    Require ((Hash $missingPayload) -ceq @($script:SourceFiles | Where-Object path -CEQ 'assets\readme.txt')[0].sha256) 'Missing-payload fixture ownership mismatch'
    Remove-Item -LiteralPath $missingPayload
    Save 'preservation.before.json' @{ user_files = @($sentinels); modified_file = $modified; modified_shortcut = @{ path = $desktop[0].path; sha256 = $modifiedLinkHash }; missing_file = $missingPayload }
    $uninstallReportPath = Join-Path $script:RunRoot 'uninstall.report.json'
    $registeredArguments = $registrations[0].values.QuietUninstallString.Substring(('"' + $resident + '"').Length).TrimStart() + ' --report "' + $uninstallReportPath + '"'
    InvokeProcess $resident @() 'uninstall' $uninstallReportPath $registeredArguments
    $null = OperationReport $uninstallReportPath 'uninstall'
    AssertFiles $script:InstallRoot (@($sentinels) + @($modified)) -Exact
    Require ((Hash $desktop[0].path) -ceq $modifiedLinkHash -and -not (Test-Path -LiteralPath $startMenu[0].path)) 'Modified shortcut was removed or unchanged shortcut remains'
    foreach ($removed in @('.avelin-receipt.json', 'uninstall.exe', '.avelin')) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $removed))) "Managed resource remains: $removed" }
    Require (@(RegistryState | Where-Object exists).Count -eq 0) 'Uninstall registry key remains'
    Save 'files.after-uninstall.json' @(Snapshot $script:InstallRoot)
    # A preserved payload-name collision must refuse reinstall before any writes.
    $beforeConflict = @(Snapshot $script:InstallRoot)
    InvokeProcess $setup @('--silent', '--install-dir', $script:InstallRoot) 'reinstall-collision' '' '' 3
    $collisionError = [IO.File]::ReadAllText((Join-Path $script:RunRoot 'reinstall-collision.stderr.log'))
    Require ($collisionError.Contains('installation destination conflicts with an existing resource') -and $collisionError.Contains($modifiedPayload)) 'Reinstall failed for a reason other than the preserved payload collision'
    AssertFiles $script:InstallRoot $beforeConflict -Exact
    Require ((Hash $desktop[0].path) -ceq $modifiedLinkHash -and @(RegistryState | Where-Object exists).Count -eq 0 -and -not (Test-Path -LiteralPath $script:LockPath)) 'Rejected reinstall changed resources or retained lock'
    # Relocate only files created by this fixture, preserving their contents as evidence.
    $archive = Inside (Join-Path $script:RunRoot 'preserved-user-edits') $script:RunRoot
    $null = New-Item -ItemType Directory -Path $archive
    $archivePayload = Inside (Join-Path $archive 'hello.txt') $archive
    $archiveLink = Inside (Join-Path $archive 'desktop.lnk') $archive
    Require (-not (Test-Path -LiteralPath $archivePayload) -and -not (Test-Path -LiteralPath $archiveLink)) 'Archive destination collision'
    Require ((Hash $modifiedPayload) -ceq $modified.sha256 -and (Hash $desktop[0].path) -ceq $modifiedLinkHash) 'Refusing to move fixture files that changed again'
    Move-Item -LiteralPath $modifiedPayload -Destination $archivePayload
    Move-Item -LiteralPath $desktop[0].path -Destination $archiveLink
    Require ((Hash $archivePayload) -ceq $modified.sha256 -and (Hash $archiveLink) -ceq $modifiedLinkHash) 'Archived user edits changed'
    $reinstallReportPath = Join-Path $script:RunRoot 'reinstall.report.json'
    InvokeProcess $setup @('--silent', '--install-dir', $script:InstallRoot, '--desktop-shortcut', 'false', '--start-menu-shortcut', 'true', '--report', $reinstallReportPath) 'reinstall' $reinstallReportPath
    $null = OperationReport $reinstallReportPath 'install' 'reinstall'
    AssertFiles $script:InstallRoot $script:SourceFiles
    AssertFiles $script:InstallRoot @($sentinels)
    $reinstalledReceipt = Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    Require (@($reinstalledReceipt.shortcuts).Count -eq 1 -and -not (Test-Path -LiteralPath $desktop[0].path) -and (Test-Path -LiteralPath $startMenu[0].path)) 'Explicit shortcut choices were not honored'
    $reinstallPlan = (Json (Join-Path $script:RunRoot 'reinstall.evidence.json')).plan
    Require (-not $reinstallPlan.desktop_shortcut -and $reinstallPlan.start_menu_shortcut) 'Plan did not record actual selected options'
    Save 'reinstalled.receipt.json' $reinstalledReceipt
    $registrations = @(RegistryState | Where-Object exists)
    $finalReport = Join-Path $script:RunRoot 'final-uninstall.report.json'
    $registeredArguments = $registrations[0].values.QuietUninstallString.Substring(('"' + $resident + '"').Length).TrimStart() + ' --report "' + $finalReport + '"'
    InvokeProcess $resident @() 'final-uninstall' $finalReport $registeredArguments
    $null = OperationReport $finalReport 'uninstall' 'final-uninstall'
    AssertFiles $script:InstallRoot @($sentinels) -Exact
    Require (-not (Test-Path -LiteralPath $desktop[0].path) -and -not (Test-Path -LiteralPath $startMenu[0].path) -and @(RegistryState | Where-Object exists).Count -eq 0) 'Final managed links or registration remain'
    Save 'files.after.json' @(Snapshot $script:InstallRoot)
    Save 'gui-fixture.json' @{ setup = $setup; setup_sha256 = Hash $setup; install_dir = $script:InstallRoot; app_id = $script:AppId; temp = $script:TempRoot; local_app_data = $script:LocalRoot; manifest = [IO.Path]::ChangeExtension($setup, 'manifest.json'); scope = 'Optional separate native GUI acceptance, never implied by this script'; shortcut_paths = @($links | ForEach-Object path) }
    $summary.status = 'PASSED'
} catch { $summary.status = 'FAILED'; $summary.failure = $_.Exception.Message; throw }
finally { $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o'); Save 'summary.json' $summary; Write-Host "Evidence retained: $script:RunRoot" }
