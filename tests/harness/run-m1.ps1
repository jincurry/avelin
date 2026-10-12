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
function InvokeProcess([string]$Executable, [string[]]$Arguments, [string]$Name, [string]$AwaitReport = '', [string]$RawArguments = '') {
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
        Require ($process.ExitCode -eq 0) "$Name exited $($process.ExitCode); inspect logs"
    } finally { Save "$Name.process.json" $record; $process.Dispose() }
}
function OperationReport([string]$Path, [string]$Operation) {
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
    Save "$Operation.evidence.json" ([ordered]@{ report_path = $Path; report_sha256 = Hash $Path; plan = $plan; session_identity = $identity; product_lock_released = $true })
    return $report
}

Require ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'Windows is required'
$ProjectRoot = Full $ProjectRoot; $CliPath = Full $CliPath; $AppPath = Full $AppPath; $BridgePath = Full $BridgePath
foreach ($binary in @($CliPath, $AppPath, $BridgePath)) { Require (Test-Path -LiteralPath $binary -PathType Leaf) "Missing binary: $binary" }
$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
$script:RunRoot = Inside (Join-Path $ProjectRoot "target\m1-harness\$runId") $ProjectRoot
Require (-not (Test-Path -LiteralPath $script:RunRoot)) 'Refusing to reuse a run directory'
$null = New-Item -ItemType Directory -Path $script:RunRoot -Force
$script:TempRoot = Join-Path $script:RunRoot 'temp'; $script:LocalRoot = Join-Path $script:RunRoot 'local-app-data'; $fixture = Join-Path $script:RunRoot 'fixture'
$null = New-Item -ItemType Directory -Path $script:TempRoot, $script:LocalRoot, $fixture
$script:AppId = 'dev.avelin.m1.harness.' + [Guid]::NewGuid().ToString('N'); $directoryName = 'AvelinM1-' + $runId
$script:InstallRoot = Inside (Join-Path $script:RunRoot $directoryName) $script:RunRoot
$script:RegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:AppId
$script:UserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$script:LockPath = Join-Path $script:LocalRoot ('Avelin\M0\locks\' + $script:AppId + '-' + (HashBytes ([Text.Encoding]::UTF8.GetBytes($script:UserSid))).Substring(0, 16) + '.lock')
$summary = [ordered]@{ schema_version = 1; status = 'RUNNING'; run_directory = $script:RunRoot; app_id = $script:AppId; scope = 'M1 normal CLI build/install/entry/resident-uninstall only; no GUI, cancellation, fault injection, performance, signing or independent-environment acceptance'; binaries = @(@($CliPath, $AppPath, $BridgePath) | ForEach-Object { @{ path = $_; sha256 = Hash $_ } }) }
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
    $config = [IO.File]::ReadAllText((Join-Path $example 'avelin-m1.toml'))
    Require ($config.Contains('id = "dev.avelin.m1.nimbus"') -and $config.Contains('directory_name = "AvelinM1Nimbus"')) 'M1 template identity changed; update harness'
    $config = $config.Replace('id = "dev.avelin.m1.nimbus"', ('id = "' + $script:AppId + '"')).Replace('directory_name = "AvelinM1Nimbus"', ('directory_name = "' + $directoryName + '"'))
    $configPath = Join-Path $fixture 'avelin.toml'; [IO.File]::WriteAllText($configPath, $config, [Text.UTF8Encoding]::new($false))
    $setup = Join-Path $script:RunRoot 'M1Setup.exe'; $buildArguments = @('build', '--config', $configPath, '--output', $setup, '--runtime', $AppPath, '--bridge', $BridgePath)
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
    $installReportPath = Join-Path $script:RunRoot 'install.report.json'; InvokeProcess $setup @('--install-dir', $script:InstallRoot, '--report', $installReportPath) 'install' $installReportPath
    $installed = OperationReport $installReportPath 'install'; AssertFiles $script:InstallRoot $script:SourceFiles
    $receipt = Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    Require ($receipt.app_id -ceq $script:AppId -and $receipt.task_id -ceq $installed.task_id -and $receipt.manifest_sha256 -ceq $script:ManifestHash -and $receipt.user_sid -ceq $script:UserSid -and (SamePath $receipt.install_dir $script:InstallRoot) -and $receipt.app_version -ceq $manifest.app_version) 'Receipt identity mismatch'
    Require (@($receipt.files).Count -eq $script:SourceFiles.Count) 'Receipt file count differs from independent source'
    foreach ($file in $script:SourceFiles) { $recorded = @($receipt.files | Where-Object path -CEQ $file.path); Require ($recorded.Count -eq 1 -and $recorded[0].sha256 -ceq $file.sha256 -and $recorded[0].size -eq $file.size) 'Receipt file metadata differs from independent source' }
    Save 'installed.receipt.json' $receipt
    $resident = Join-Path $script:InstallRoot '.avelin\avelin-app.exe'; $residentManifest = Join-Path $script:InstallRoot '.avelin\manifest.json'; $uninstaller = Join-Path $script:InstallRoot 'uninstall.exe'
    Require ((Hash $resident) -ceq (Hash $AppPath) -and (Hash $uninstaller) -ceq $receipt.uninstaller_sha256) 'Resident runtime or uninstaller hash mismatch'
    Require ((Hash $residentManifest) -ceq (Hash ([IO.Path]::ChangeExtension($setup, 'manifest.json')))) 'Resident manifest differs from the package sidecar'
    $beforeEntry = @(Snapshot $script:InstallRoot); InvokeProcess (Join-Path $script:InstallRoot 'nimbus-demo.exe') @() 'entry'
    Require ([IO.File]::ReadAllText((Join-Path $script:RunRoot 'entry.stdout.log')).Replace("`r`n", "`n") -ceq "Nimbus Notes — Avelin M0 sample application`nThis executable is a local installation test payload.`n" -and (Get-Item (Join-Path $script:RunRoot 'entry.stderr.log')).Length -eq 0) 'Nimbus stdout/stderr contract mismatch'
    AssertFiles $script:InstallRoot $beforeEntry -Exact
    $sentinels = foreach ($relative in @('user-kept.txt', 'assets\user-created.txt')) { $path = Inside (Join-Path $script:InstallRoot $relative) $script:InstallRoot; Require (-not (Test-Path -LiteralPath $path)) 'Sentinel collision'; [IO.File]::WriteAllText($path, "Unmanaged M1 fixture $runId", [Text.UTF8Encoding]::new($false)); [pscustomobject]@{ path = $relative; size = (Get-Item -LiteralPath $path).Length; sha256 = Hash $path } }
    Save 'sentinels.json' @($sentinels)
    $registrations = @(RegistryState | Where-Object exists); Require ($registrations.Count -gt 0) 'Uninstall registration missing'
    $expectedCommand = '"{0}" --operation uninstall --backend "{1}" --manifest "{2}" --install-dir "{3}"' -f $resident, $uninstaller, $residentManifest, $script:InstallRoot
    foreach ($registration in $registrations) { Require ($registration.values.UninstallString -ceq $expectedCommand -and (SamePath $registration.values.InstallLocation $script:InstallRoot) -and $registration.values.DisplayVersion -ceq $manifest.app_version) 'Unexpected registered uninstall command or identity' }
    Save 'installed.registry.json' $registrations
    $uninstallReportPath = Join-Path $script:RunRoot 'uninstall.report.json'
    $registeredArguments = $registrations[0].values.UninstallString.Substring(('"' + $resident + '"').Length).TrimStart() + ' --report "' + $uninstallReportPath + '"'
    # The verified registration is passed to CreateProcess, never evaluated by a shell.
    InvokeProcess $resident @() 'uninstall' $uninstallReportPath $registeredArguments
    $null = OperationReport $uninstallReportPath 'uninstall'; AssertFiles $script:InstallRoot @($sentinels) -Exact
    foreach ($removed in @('.avelin-receipt.json', 'uninstall.exe', '.avelin')) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $removed))) "Managed resource remains: $removed" }
    Require (@(RegistryState | Where-Object exists).Count -eq 0) 'Uninstall registry key remains'
    Save 'files.after.json' @(Snapshot $script:InstallRoot); $summary.status = 'PASSED'
} catch { $summary.status = 'FAILED'; $summary.failure = $_.Exception.Message; throw }
finally { $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o'); Save 'summary.json' $summary; Write-Host "Evidence retained: $script:RunRoot" }
