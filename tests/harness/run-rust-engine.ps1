#requires -Version 7.2
<# Real-process acceptance for the optional Rust backend. All payloads and evidence are
   isolated below a new workspace target directory. Only the two exact, UUID-named
   current-user shortcuts and the matching uninstall key live outside that directory.
   Failed scenes are retained. This script never kills processes or recursively deletes. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Runtime,
    [Parameter(Mandatory)][string]$Cli,
    [Parameter(Mandatory)][string]$RustWorker,
    [Parameter(Mandatory)][string]$OutputRoot,
    [string]$Sample = (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) 'examples/nimbus/payload/nimbus-demo.exe'),
    [string]$Nsis,
    [string]$Bridge,
    [ValidateRange(10, 1800)][int]$TimeoutSeconds = 180
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
function HashBytes([byte[]]$Bytes) { [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($Bytes)).ToLowerInvariant() }
function Json([string]$Path) { $script:Utf8.GetString([IO.File]::ReadAllBytes($Path)) | ConvertFrom-Json }
function Save([string]$Name, [object]$Value) {
    $path = Inside (Join-Path $script:RunRoot $Name) $script:RunRoot
    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $path))
    [IO.File]::WriteAllText($path, ($Value | ConvertTo-Json -Depth 60), $script:Utf8)
}
function WriteNew([string]$Path, [string]$Contents) {
    $path = Inside $Path $script:RunRoot
    Require (-not (Test-Path -LiteralPath $path)) "Refusing to replace fixture file: $path"
    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $path))
    $stream = [IO.File]::Open($path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $bytes = $script:Utf8.GetBytes($Contents); $stream.Write($bytes) } finally { $stream.Dispose() }
}
function CopyNew([string]$Source, [string]$Destination) {
    $destination = Inside $Destination $script:RunRoot
    Require (-not (Test-Path -LiteralPath $destination)) "Copy destination exists: $destination"
    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $destination))
    [IO.File]::Copy($Source, $destination, $false)
    Require ((Hash $Source) -ceq (Hash $destination)) "Copied bytes differ: $destination"
}
function Snapshot([string]$Root) {
    if (-not (Test-Path -LiteralPath $Root)) { return }
    $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push($Root)
    $files = while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            $safe = Inside $item.FullName $Root
            if ($item.PSIsContainer) { $pending.Push($safe) }
            else { [pscustomobject]@{ path = $safe.Substring($Root.Length + 1); size = $item.Length; sha256 = Hash $safe } }
        }
    }
    @($files | Sort-Object path)
}
function FileRecord([string]$Path) {
    $path = Inside $Path $script:InstallRoot
    [pscustomobject]@{ path = $path.Substring($script:InstallRoot.Length + 1); size = (Get-Item -LiteralPath $path).Length; sha256 = Hash $path }
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
            $key = $base.OpenSubKey($script:RegistrySubkey, $false); $values = [ordered]@{}; $kinds = [ordered]@{}
            if ($key) { foreach ($name in ($key.GetValueNames() | Sort-Object)) { $values[$name] = $key.GetValue($name); $kinds[$name] = "$($key.GetValueKind($name))" } }
            [pscustomobject]@{ view = "$view"; exists = $null -ne $key; values = $values; kinds = $kinds }
        } finally { if ($key) { $key.Dispose() }; $base.Dispose() }
    }
}
function ProductSnapshot {
    $pending = [Collections.Generic.Stack[string]]::new()
    if (Test-Path -LiteralPath $script:InstallRoot) { $pending.Push($script:InstallRoot) }
    $directories = @(while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            $safe = Inside $item.FullName $script:InstallRoot
            if ($item.PSIsContainer) { $pending.Push($safe); $safe }
        }
    }) | Sort-Object
    $links = foreach ($path in @($script:DesktopPath, $script:StartMenuPath)) {
        $exists = Test-Path -LiteralPath $path
        [pscustomobject]@{ path = $path; exists = $exists; sha256 = $(if ($exists) { Hash $path } else { $null }) }
    }
    [pscustomobject]@{ files = @(Snapshot $script:InstallRoot); directories = @($directories); registry = @(RegistryState); shortcuts = @($links) }
}
function AssertUnchanged([object]$Before, [string]$Label) {
    $after = ProductSnapshot; Save "$Label.after.json" $after
    Require (($Before | ConvertTo-Json -Depth 50 -Compress) -ceq ($after | ConvertTo-Json -Depth 50 -Compress)) "$Label changed product resources"
    Require (-not (Test-Path -LiteralPath $script:LockPath)) "$Label retained the product lock"
}
function InvokeProcess([string]$Executable, [string[]]$Arguments, [string]$Name, [string]$AwaitReport = '', [string]$RawArguments = '', [int]$ExpectedExit = 0, [switch]$AllowNsis) {
    $info = [Diagnostics.ProcessStartInfo]::new($Executable)
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true; $info.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
    $info.WorkingDirectory = $script:RunRoot; $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = $script:Utf8; $info.StandardErrorEncoding = $script:Utf8
    $info.Environment['TEMP'] = $script:TempRoot; $info.Environment['TMP'] = $script:TempRoot; $info.Environment['LOCALAPPDATA'] = $script:LocalRoot
    if (-not $AllowNsis) {
        $null = $info.Environment.Remove('NSIS_MAKENSIS'); $null = $info.Environment.Remove('NSISDIR')
        $info.Environment['PATH'] = [Environment]::GetFolderPath('System') + ';' + $env:SystemRoot
    }
    if ($RawArguments) { Require (@($Arguments).Count -eq 0) 'Mixed argument forms'; $info.Arguments = $RawArguments }
    else { foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) } }
    $record = [ordered]@{ executable = $Executable; executable_sha256 = Hash $Executable; arguments = $Arguments; raw_arguments = $RawArguments; started_at_utc = [DateTime]::UtcNow.ToString('o'); status = 'STARTING'; awaited_report = $AwaitReport; nsis_environment_available = [bool]$AllowNsis }
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    try {
        $null = $process.Start(); $record.process_id = $process.Id; $record.process_start_utc = $process.StartTime.ToUniversalTime().ToString('o'); $record.status = 'RUNNING'
        Save "$Name.process.json" $record
        $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync(); $timer = [Diagnostics.Stopwatch]::StartNew()
        Write-Host "[$Name] started PID $($process.Id)"
        do {
            $exited = $process.WaitForExit(200); $reported = -not $AwaitReport
            if ($AwaitReport -and (Test-Path -LiteralPath $AwaitReport)) {
                try { $reported = (Json $AwaitReport).result -in @('Succeeded', 'Failed', 'Cancelled', 'Unknown') } catch { $reported = $false }
            }
            if ($exited -and $stdout.IsCompleted -and $stderr.IsCompleted -and ($reported -or $process.ExitCode -ne 0)) { break }
            if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) {
                $record.status = 'TIMED_OUT_PROCESS_NOT_KILLED'
                throw "$Name timed out. No process was killed. Inspect the recorded process and retained scene before another run."
            }
            if ($exited) { Start-Sleep -Milliseconds 100 }
        } while ($true)
        $record.exit_code = $process.ExitCode; $record.status = 'EXITED'
        [IO.File]::WriteAllText((Join-Path $script:RunRoot "$Name.stdout.log"), $stdout.GetAwaiter().GetResult(), $script:Utf8)
        [IO.File]::WriteAllText((Join-Path $script:RunRoot "$Name.stderr.log"), $stderr.GetAwaiter().GetResult(), $script:Utf8)
        Require ($process.ExitCode -eq $ExpectedExit) "$Name exited $($process.ExitCode), expected $ExpectedExit; inspect retained logs"
    } finally { $record.finished_observation_utc = [DateTime]::UtcNow.ToString('o'); Save "$Name.process.json" $record; $process.Dispose() }
}
function ReadBytes([IO.Stream]$Stream, [int]$Count) {
    $bytes = [byte[]]::new($Count); $read = 0
    while ($read -lt $Count) { $received = $Stream.Read($bytes, $read, $Count - $read); Require ($received -gt 0) 'Unexpected package EOF'; $read += $received }
    return ,$bytes
}
function HashRegion([IO.Stream]$Stream, [long]$Count) {
    Require ($Count -ge 0 -and $Count -le $Stream.Length - $Stream.Position) 'Requested hash region exceeds stream bounds'
    $hasher = [Security.Cryptography.IncrementalHash]::CreateHash([Security.Cryptography.HashAlgorithmName]::SHA256)
    try {
        $buffer = [byte[]]::new(65536)
        while ($Count -gt 0) {
            $received = $Stream.Read($buffer, 0, [int][Math]::Min($buffer.Length, $Count))
            Require ($received -gt 0) 'Hash region reached unexpected EOF'
            $hasher.AppendData($buffer, 0, $received); $Count -= $received
        }
        return [Convert]::ToHexString($hasher.GetHashAndReset()).ToLowerInvariant()
    } finally { $hasher.Dispose() }
}
function VerifyNativeBundle([string]$Path, [object]$Manifest, [string]$ManifestHash) {
    $stream = [IO.File]::OpenRead($Path)
    try {
        $null = $stream.Seek(-72, [IO.SeekOrigin]::End); $footer = ReadBytes $stream 72
        Require ($script:Utf8.GetString($footer, 56, 16) -ceq 'AVELIN-M0-PACK01') 'Native worker bundle footer missing'
        $workerSize = [BitConverter]::ToUInt64($footer, 0); $payloadSize = [BitConverter]::ToUInt64($footer, 8); $manifestSize = [BitConverter]::ToUInt64($footer, 16)
        Require ($workerSize -eq (Get-Item -LiteralPath $script:WorkerBinary).Length -and $workerSize + $payloadSize + $manifestSize -eq $stream.Length - 72) 'Native bundle bounds or executable size differ'
        $stream.Position = 0
        Require ((HashRegion $stream $workerSize) -ceq (Hash $script:WorkerBinary)) 'Native backend executable is not the supplied Rust worker'
        Require ($script:Utf8.GetString((ReadBytes $stream 16)) -ceq 'AVELIN-RUST-P001') 'Native payload magic differs'
        Require ([BitConverter]::ToUInt32((ReadBytes $stream 4), 0) -eq 1 -and [BitConverter]::ToUInt32((ReadBytes $stream 4), 0) -eq @($Manifest.files).Count) 'Native payload version or file count differs'
        $runtimeSize = [BitConverter]::ToUInt64((ReadBytes $stream 8), 0)
        Require ($runtimeSize -eq (Get-Item -LiteralPath $script:RuntimeBinary).Length -and (HashRegion $stream $runtimeSize) -ceq $Manifest.runtime_sha256) 'Native payload runtime differs'
        foreach ($file in $Manifest.files) {
            $size = [BitConverter]::ToUInt64((ReadBytes $stream 8), 0)
            Require ($size -eq $file.size -and (HashRegion $stream $size) -ceq $file.sha256) "Native stored file differs: $($file.path)"
        }
        Require ($stream.Position -eq $workerSize + $payloadSize -and (HashRegion $stream $manifestSize) -ceq $ManifestHash) 'Native bundle has trailing payload or different manifest bytes'
    } finally { $stream.Dispose() }
}
function NewProduct([string]$Label) {
    $script:CaseLabel = $Label; $script:AppId = 'dev.avelin.rust.' + [Guid]::NewGuid().ToString('N')
    $script:InstallRoot = Inside (Join-Path $script:RunRoot "$Label installed 中文") $script:RunRoot
    $script:RegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:AppId
    # The lock name is derived independently from the current coordinator contract.
    $sidTag = (HashBytes ($script:Utf8.GetBytes($script:UserSid))).Substring(0, 16)
    $script:LockPath = Join-Path $script:LocalRoot "Avelin\M0\locks\$($script:AppId)-$sidTag.lock"
    $script:DesktopPath = Inside (Join-Path $script:DesktopFolder ($script:AppId + '.lnk')) $script:DesktopFolder
    $script:StartMenuPath = Inside (Join-Path $script:ProgramsFolder ($script:AppId + '.lnk')) $script:ProgramsFolder
    Require (@(RegistryState | Where-Object exists).Count -eq 0 -and -not (Test-Path -LiteralPath $script:DesktopPath) -and -not (Test-Path -LiteralPath $script:StartMenuPath)) 'Unique fixture registration/shortcut already exists'
    $null = [IO.Directory]::CreateDirectory($script:InstallRoot)
    Save "$Label.identity.json" @{ app_id = $script:AppId; install_dir = $script:InstallRoot; registry_subkey = $script:RegistrySubkey; desktop = $script:DesktopPath; start_menu = $script:StartMenuPath; user_sid = $script:UserSid; product_lock = $script:LockPath }
}
function BuildPackage([string]$Label, [string]$Version, [ValidateSet('a', 'b')][string]$Layout, [ValidateSet('rust', 'nsis')][string]$Engine = 'rust') {
    $project = Inside (Join-Path $script:RunRoot "project-$Label 中文") $script:RunRoot; $payload = Join-Path $project 'dist'
    $entry = if ($Layout -ceq 'a') { 'app.exe' } else { 'bin\app.exe' }
    CopyNew $script:SampleBinary (Join-Path $payload $entry)
    $data = [ordered]@{ 'shared.txt' = "Shared version $Version"; 'unchanged.txt' = 'Same payload content'; 'assets\中文.txt' = 'Unicode payload' }
    if ($Layout -ceq 'a') { $data['obsolete\retired.txt'] = 'Old owned content'; $data['retired-user.txt'] = 'Old mutable test content' }
    else { $data['added.txt'] = 'New owned content' }
    foreach ($relative in $data.Keys) { WriteNew (Join-Path $payload $relative) $data[$relative] }
    $config = Join-Path $project 'avelin.toml'
    $configText = @"
schema_version = 1
[app]
id = "$($script:AppId)"
name = "Rust 引擎验收样例"
version = "$Version"
entry = "$($entry.Replace('\', '/'))"
[payload]
directory = "dist"
[install]
directory_name = "AvelinRustFixture"
desktop_shortcut = true
start_menu_shortcut = true
[ui]
template = "compact"
theme = "light"
locale = "zh-CN"
fallback_locale = "en-US"
accent_color = "#245DA8"
"@
    WriteNew $config $configText
    $source = @(Snapshot $payload); Save "$Label.source.json" $source
    InvokeProcess $script:CliBinary @('check', '--config', $config) "check-$Label"
    $setup = Join-Path $script:RunRoot "$Label-Setup.exe"
    $arguments = @('build', '--config', $config, '--output', $setup, '--runtime', $script:RuntimeBinary, '--engine', $Engine)
    if ($Engine -ceq 'rust') { $arguments += @('--rust-worker', $script:WorkerBinary) }
    else { $arguments += @('--bridge', $script:BridgeBinary, '--nsis', $Nsis) }
    InvokeProcess $script:CliBinary $arguments "build-$Label" -AllowNsis:($Engine -ceq 'nsis')
    $built = Json (Join-Path $script:RunRoot "build-$Label.stdout.log")
    Require ($built.engine -ceq $Engine) 'Builder did not report the requested execution engine'
    $stream = [IO.File]::OpenRead($setup)
    try {
        $null = $stream.Seek(-72, [IO.SeekOrigin]::End); $footer = ReadBytes $stream 72
        Require ($script:Utf8.GetString($footer, 56, 16) -ceq 'AVELIN-M0-PACK01') 'Expected unsigned outer package footer'
        $start = [BitConverter]::ToUInt64($footer, 0); $backendSize = [BitConverter]::ToUInt64($footer, 8); $metadataSize = [BitConverter]::ToUInt64($footer, 16)
        Require ($metadataSize -gt 0 -and $metadataSize -le 32MB -and $start + $backendSize + $metadataSize -eq $stream.Length - 72) 'Invalid outer package bounds'
        $stream.Position = [long]$start
        Require ((HashRegion $stream $backendSize) -ceq (Hash ([IO.Path]::ChangeExtension($setup, 'backend.exe')))) 'Embedded backend differs from the built sidecar'
        $null = $stream.Seek([long]($start + $backendSize), [IO.SeekOrigin]::Begin); $metadata = ReadBytes $stream ([int]$metadataSize)
    } finally { $stream.Dispose() }
    $manifest = $script:Utf8.GetString($metadata) | ConvertFrom-Json
    [IO.File]::WriteAllBytes((Join-Path $script:RunRoot "$Label.embedded-manifest.json"), $metadata)
    Require ($manifest.app_id -ceq $script:AppId -and $manifest.app_version -ceq $Version -and $manifest.entry -ceq $entry -and $manifest.runtime_sha256 -ceq (Hash $script:RuntimeBinary)) 'Packaged identity/entry/runtime differs from fixture'
    $manifestFile = [IO.Path]::ChangeExtension($setup, 'manifest.json')
    $sidecar = Json $manifestFile
    Require (@($manifest.files).Count -eq $source.Count -and ($sidecar | ConvertTo-Json -Depth 60 -Compress) -ceq ($manifest | ConvertTo-Json -Depth 60 -Compress)) 'Packaged inventory or manifest sidecar structure mismatch'
    foreach ($file in $source) {
        $actual = @($manifest.files | Where-Object path -CEQ $file.path)
        Require ($actual.Count -eq 1 -and $actual[0].sha256 -ceq $file.sha256 -and $actual[0].size -eq $file.size) 'Packaged payload differs from independent input'
    }
    if ($Engine -ceq 'rust') { VerifyNativeBundle ([IO.Path]::ChangeExtension($setup, 'backend.exe')) $manifest (HashBytes $metadata) }
    [pscustomobject]@{ label = $Label; engine = $Engine; setup = $setup; setup_sha256 = Hash $setup; manifest = $manifest; manifest_sha256 = HashBytes $metadata; manifest_file_sha256 = Hash $manifestFile; source_files = $source; source_directory = $payload }
}
function OperationReport([string]$Path, [string]$Operation, [string]$Label, [object]$Package, [string]$ExpectedResult = 'Succeeded', [int]$ExpectedCode = 0) {
    $report = Json $Path
    Require ($report.result -ceq $ExpectedResult -and $report.exit_code -eq $ExpectedCode -and $report.coordinator_exit_code -eq $ExpectedCode -and -not $report.product_lock_retained) "$Label terminal result differs from $ExpectedResult/$ExpectedCode"
    Require ((SamePath $report.install_dir $script:InstallRoot) -and (SamePath $report.report_path $Path) -and @($report.events).Count -gt 0 -and $report.backend_pid -gt 0) 'Report destination/events/process mismatch'
    foreach ($event in $report.events) { Require ($event.task_id -ceq $report.task_id) 'Cross-task report event' }
    $session = Inside $report.session_dir $script:RunRoot; $plan = Json (Join-Path $session 'install-plan.json'); $identity = Json (Join-Path $session 'session.json')
    Require ($plan.task_id -ceq $report.task_id -and $plan.operation -ceq $Operation -and $identity.operation -ceq $Operation -and $plan.app_id -ceq $script:AppId -and (SamePath $plan.install_dir $script:InstallRoot)) 'Task/operation/target identity mismatch'
    Require ($plan.manifest_sha256 -ceq $Package.manifest_sha256 -and $identity.manifest_sha256 -ceq $Package.manifest_sha256 -and $plan.user_sid -ceq $script:UserSid -and $identity.task_id -ceq $plan.task_id) 'Plan/session package or user mismatch'
    Require ($plan.payload_file_count -eq $Package.source_files.Count -and $plan.payload_bytes -eq ($Package.source_files | Measure-Object size -Sum).Sum) 'Plan counts differ from source'
    Require (SamePath $identity.product_lock $script:LockPath) 'Unexpected product lock identity'
    Require (-not (Test-Path -LiteralPath $script:LockPath)) 'Completed task retained product lock'
    Save "$Label.evidence.json" @{ report_path = $Path; report_sha256 = Hash $Path; plan = $plan; identity = $identity; observed_result = $ExpectedResult }
    return $report
}
function VerifyInstalled([object]$Package, [object]$Report, [string]$Label, [string[]]$ShortcutKinds) {
    AssertFiles $script:InstallRoot $Package.source_files
    $receiptPath = Join-Path $script:InstallRoot '.avelin-receipt.json'; $receipt = Json $receiptPath
    Require ($receipt.app_id -ceq $script:AppId -and $receipt.app_version -ceq $Package.manifest.app_version -and $receipt.task_id -ceq $Report.task_id -and $receipt.manifest_sha256 -ceq $Package.manifest_sha256 -and $receipt.user_sid -ceq $script:UserSid -and (SamePath $receipt.install_dir $script:InstallRoot)) 'Receipt identity mismatch'
    Require (@($receipt.files).Count -eq $Package.source_files.Count -and @($receipt.shortcuts).Count -eq $ShortcutKinds.Count -and $receipt.installation_id -and $receipt.commit_id) 'Receipt resource counts or commit identity differ'
    foreach ($file in $Package.source_files) {
        $actual = @($receipt.files | Where-Object path -CEQ $file.path)
        Require ($actual.Count -eq 1 -and $actual[0].sha256 -ceq $file.sha256 -and $actual[0].size -eq $file.size) 'Receipt differs from independent source'
    }
    $resident = Join-Path $script:InstallRoot '.avelin\avelin-app.exe'; $uninstaller = Join-Path $script:InstallRoot 'uninstall.exe'; $manifest = Join-Path $script:InstallRoot '.avelin\manifest.json'
    Require ((Hash $resident) -ceq (Hash $script:RuntimeBinary) -and (Hash $uninstaller) -ceq $receipt.uninstaller_sha256 -and (Hash $manifest) -ceq $Package.manifest_file_sha256) 'Installed runtime/uninstaller/manifest integrity differs'
    $links = foreach ($kind in $ShortcutKinds) {
        $path = if ($kind -ceq 'desktop') { $script:DesktopPath } else { $script:StartMenuPath }
        $records = @($receipt.shortcuts | Where-Object kind -CEQ $kind)
        Require ($records.Count -eq 1 -and (SamePath $records[0].path $path) -and (Hash $path) -ceq $records[0].sha256 -and (SamePath $records[0].target (Join-Path $script:InstallRoot $Package.manifest.entry))) 'Shortcut ownership mismatch'
        # WScript.Shell converts these properties through the system ANSI
        # code page. Read the persisted link through IShellLinkW instead.
        $link = [AvelinAcceptance.ShellLink]::Read($path)
        $shortcutCheck = [ordered]@{
            kind = $kind
            target_matches = -not [string]::IsNullOrEmpty($link.TargetPath) -and (SamePath $link.TargetPath (Join-Path $script:InstallRoot $Package.manifest.entry))
            arguments_empty = $link.Arguments -ceq ''
            working_directory_matches = -not [string]::IsNullOrEmpty($link.WorkingDirectory) -and (SamePath $link.WorkingDirectory $script:InstallRoot)
            target_empty = [string]::IsNullOrEmpty($link.TargetPath)
            working_directory_empty = [string]::IsNullOrEmpty($link.WorkingDirectory)
            target_exists = [IO.File]::Exists($link.TargetPath)
            working_directory_exists = [IO.Directory]::Exists($link.WorkingDirectory)
            target_contains_question_mark = $link.TargetPath.Contains('?')
            working_directory_contains_question_mark = $link.WorkingDirectory.Contains('?')
            target_has_short_component = $link.TargetPath -match '(?i)(^|\\)[^\\]*~[0-9]+(\\|$)'
        }
        Save "$Label-$kind.shortcut-check.json" $shortcutCheck
        Require ($shortcutCheck.target_matches -and $shortcutCheck.arguments_empty -and $shortcutCheck.working_directory_matches) 'Actual shortcut target/arguments/workdir differ'
        @{ kind = $kind; path = $path; sha256 = Hash $path; target = $link.TargetPath; arguments = $link.Arguments; working_directory = $link.WorkingDirectory }
    }
    $registration = @(RegistryState | Where-Object exists); Require ($registration.Count -gt 0) 'Registration missing'
    foreach ($record in $registration) {
        $v = $record.values
        $normal = '"{0}" --gui --operation uninstall --backend "{1}" --manifest "{2}" --install-dir "{3}"' -f $resident, $uninstaller, $manifest, $script:InstallRoot
        Require ($v.UninstallString -ceq $normal -and $v.QuietUninstallString -ceq $normal.Replace(' --gui ', ' --silent ') -and $v.DisplayVersion -ceq $Package.manifest.app_version -and $v.DisplayName -ceq $Package.manifest.app_name -and (SamePath $v.InstallLocation $script:InstallRoot) -and $v.NoModify -eq 1 -and $v.NoRepair -eq 1) 'Registered commands/identity/capabilities differ'
        foreach ($name in @('UninstallString', 'QuietUninstallString', 'DisplayName', 'DisplayVersion', 'InstallLocation')) { Require ($record.kinds[$name] -ceq 'String') "Registration type differs: $name" }
        foreach ($name in @('NoModify', 'NoRepair')) { Require ($record.kinds[$name] -ceq 'DWord') "Registration type differs: $name" }
    }
    CopyNew $receiptPath (Join-Path $script:RunRoot "$Label.receipt.json")
    Save "$Label.shortcuts.json" @($links); Save "$Label.registry.json" $registration
    return $receipt
}
function RejectOperation([object]$Package, [string]$Operation, [string]$Label, [string]$ReasonPattern, [string[]]$Extra = @()) {
    $before = ProductSnapshot; Save "$Label.before.json" $before
    $reportPath = Join-Path $script:RunRoot "$Label.report.json"
    InvokeProcess $Package.setup (@('--silent', '--operation', $Operation, '--install-dir', $script:InstallRoot, '--report', $reportPath) + $Extra) $Label -ExpectedExit 3
    $stderr = [IO.File]::ReadAllText((Join-Path $script:RunRoot "$Label.stderr.log"))
    Require ($stderr -match $ReasonPattern) "$Label failed for an unexpected reason: inspect stderr"
    if (Test-Path -LiteralPath $reportPath) { $result = Json $reportPath; Require ($result.result -cne 'Succeeded') "$Label reported success" }
    AssertUnchanged $before $Label
    Save "$Label.check.json" @{ status = 'PASSED'; expected_exit_code = 3; resources_unchanged = $true; diagnostic_pattern = $ReasonPattern }
}
function ArchiveOwned([string]$Path, [string]$Label, [string]$ExpectedHash) {
    $path = Full $Path
    Require ($path.StartsWith($script:InstallRoot + '\', [StringComparison]::OrdinalIgnoreCase) -or (SamePath $path $script:DesktopPath) -or (SamePath $path $script:StartMenuPath)) 'Archive source is not this fixture resource'
    Require ((Hash $path) -ceq $ExpectedHash) 'Test resource changed again; refusing to move it'
    $destination = Inside (Join-Path $script:RunRoot "preserved-user-edits\$Label") $script:RunRoot
    Require (-not (Test-Path -LiteralPath $destination)) 'Archive destination exists'
    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $destination))
    Move-Item -LiteralPath $path -Destination $destination
    Require ((Hash $destination) -ceq $ExpectedHash) 'Archived bytes differ'
    Save "$Label.archive.json" @{ original = $path; archive = $destination; sha256 = $ExpectedHash }
}
function ModifyShortcut([string]$Path, [string]$Label) {
    Require ((SamePath $Path $script:DesktopPath) -or (SamePath $Path $script:StartMenuPath)) 'Unexpected shortcut mutation'
    [AvelinAcceptance.ShellLink]::SetDescription($Path, "User customization $Label $($script:AppId)")
    return Hash $Path
}
function UninstallRegistered([object]$Package, [string]$Label) {
    $registry = @(RegistryState | Where-Object exists); Require ($registry.Count -gt 0) 'Missing registered uninstall entry'
    $resident = Join-Path $script:InstallRoot '.avelin\avelin-app.exe'; $prefix = '"' + $resident + '"'
    $command = [string]$registry[0].values.QuietUninstallString; Require ($command.StartsWith($prefix, [StringComparison]::Ordinal)) 'Uninstall command executable mismatch'
    $reportPath = Join-Path $script:RunRoot "$Label.report.json"
    $arguments = $command.Substring($prefix.Length).TrimStart() + ' --report "' + $reportPath + '"'
    InvokeProcess $resident @() $Label $reportPath $arguments
    $null = OperationReport $reportPath 'uninstall' $Label $Package
    Require (@(RegistryState | Where-Object exists).Count -eq 0) 'Uninstall registry key remains'
    foreach ($relative in @('.avelin-receipt.json', 'uninstall.exe', '.avelin')) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $relative))) "Internal resource remains: $relative" }
}
function FindSessionRecords([string]$Root) {
    $pending = [Collections.Generic.Stack[string]]::new()
    if (Test-Path -LiteralPath $Root) { $pending.Push($Root) }
    while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            $safe = Inside $item.FullName $Root
            if ($item.PSIsContainer) { $pending.Push($safe) }
            elseif ($item.Name -ceq 'session.json') {
                # A newly created session may be observed during its first write.
                # It becomes eligible only once a complete identity can be parsed.
                try { $identity = Json $safe } catch { continue }
                [pscustomobject]@{ path = $safe; identity = $identity }
            }
        }
    }
}
function UninstallDirect([object]$Package, [string]$Label) {
    $before = @(FindSessionRecords $script:TempRoot)
    $knownSessions = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($record in $before) { $null = $knownSessions.Add($record.path) }
    Save "$Label.sessions-before.json" @($before)
    $uninstaller = Join-Path $script:InstallRoot 'uninstall.exe'
    $receipt = Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    Require ($receipt.app_id -ceq $script:AppId -and (Hash $uninstaller) -ceq $receipt.uninstaller_sha256) 'Direct uninstall bootstrap identity differs'
    InvokeProcess $uninstaller @('--silent') "$Label-bootstrap"
    # The bootstrap intentionally detaches the resident frontend and returns 0.
    # Observe the new task's identity/report; bootstrap exit alone proves no outcome.
    $timer = [Diagnostics.Stopwatch]::StartNew(); $observed = $null; $reportPath = $null
    do {
        $candidates = @(FindSessionRecords $script:TempRoot | Where-Object {
            -not $knownSessions.Contains($_.path) -and $_.identity.app_id -ceq $script:AppId -and
            $_.identity.operation -ceq 'uninstall' -and (SamePath $_.identity.install_dir $script:InstallRoot)
        })
        Require ($candidates.Count -le 1) 'Direct uninstall created multiple matching task sessions'
        if ($candidates.Count -eq 1) {
            $observed = $candidates[0]
            Require ($observed.identity.schema_version -eq 1 -and $observed.identity.manifest_sha256 -ceq $Package.manifest_sha256) 'Direct uninstall session schema/package identity differs'
            $session = Split-Path -Parent $observed.path
            $reportPath = Inside $observed.identity.report_path $script:TempRoot
            Require (SamePath $reportPath (Join-Path $session 'report.json')) 'Direct uninstall did not use its own default session report'
            if (Test-Path -LiteralPath $reportPath) {
                try { $result = Json $reportPath } catch { $result = $null }
                if ($null -ne $result -and $result.result -in @('Succeeded', 'Failed', 'Cancelled', 'Unknown')) { break }
            }
        }
        if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) {
            Save "$Label.wait.json" @{ status = 'TIMED_OUT_PROCESS_NOT_KILLED'; bootstrap_only = $true; observed_session = $observed }
            throw "$Label has no confirmed terminal task report. No process was killed; inspect retained evidence."
        }
        Start-Sleep -Milliseconds 100
    } while ($true)
    Require ($result.task_id -ceq $observed.identity.task_id -and $result.backend_pid -eq $observed.identity.backend.pid) 'Direct uninstall report belongs to a different task or backend'
    $coordinator = Inside $observed.identity.coordinator.executable $script:TempRoot
    Require ([IO.Path]::GetFileName($coordinator) -ceq 'uninstall-coordinator.exe' -and (Hash $coordinator) -ceq (Hash $script:RuntimeBinary)) 'Direct uninstall coordinator executable differs from the supplied runtime'
    $observation = 0
    do {
        $observation++; $observationLabel = "$Label-observe-$observation"
        InvokeProcess $script:CliBinary @('diagnose', '--session', (Split-Path -Parent $observed.path)) $observationLabel
        $diagnosis = Json (Join-Path $script:RunRoot "$observationLabel.stdout.log")
        Require ($diagnosis.read_only -and $diagnosis.task_id -ceq $observed.identity.task_id -and $diagnosis.app_id -ceq $script:AppId -and $diagnosis.operation -ceq 'uninstall') 'Direct uninstall process observation has a different identity'
        $states = @($diagnosis.processes.coordinator.state, $diagnosis.processes.backend.state)
        Require (@($states | Where-Object { $_ -notin @('Running', 'NotRunning', 'DifferentProcess') }).Count -eq 0) 'Direct uninstall process state cannot be confirmed'
        if (@($states | Where-Object { $_ -ceq 'Running' }).Count -eq 0) { break }
        if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) { throw "$Label reported a result but its identified processes remain active; no process was killed." }
        Start-Sleep -Milliseconds 100
    } while ($true)
    $null = OperationReport $reportPath 'uninstall' $Label $Package
    Require (@(RegistryState | Where-Object exists).Count -eq 0) 'Direct uninstall left its registration'
    foreach ($relative in @('.avelin-receipt.json', 'uninstall.exe', '.avelin')) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $relative))) "Direct uninstall left an internal resource: $relative" }
    Save "$Label.bootstrap-proof.json" @{ status = 'PASSED'; bootstrap_success_is_not_task_success = $true; session_identity = $observed.identity; report_path = $reportPath; report_sha256 = Hash $reportPath; confirmed_task_result = $result.result; process_observation = $diagnosis.processes; resource_assertions = 'Registration and installer internals absent; caller separately verifies user-file and shortcut preservation.' }
}
function AddSentinels {
    foreach ($relative in @('user-kept.txt', 'user-data\用户文件.txt')) {
        $path = Join-Path $script:InstallRoot $relative
        WriteNew $path "Unmanaged fixture data $($script:AppId)"
        FileRecord $path
    }
}
function RecoverableUpgradeFault([object]$Package, [string]$Label, [string]$Checkpoint, [string[]]$Extra = @()) {
    $before = ProductSnapshot; Save "$Label.before.json" $before
    $receiptHash = Hash (Join-Path $script:InstallRoot '.avelin-receipt.json')
    $reportPath = Join-Path $script:RunRoot "$Label.report.json"
    $arguments = @('--silent', '--operation', 'upgrade', '--install-dir', $script:InstallRoot, '--engine-fault', $Checkpoint, '--report', $reportPath) + $Extra
    InvokeProcess $Package.setup $arguments $Label $reportPath -ExpectedExit 3
    $report = OperationReport $reportPath 'upgrade' $Label $Package 'Failed' 3
    AssertUnchanged $before $Label
    Require ((Hash (Join-Path $script:InstallRoot '.avelin-receipt.json')) -ceq $receiptHash -and -not (Test-Path -LiteralPath (Join-Path $script:InstallRoot '.avelin\upgrade-state.json'))) 'Recovered upgrade changed receipt or retained unresolved marker'
    Require (@(Snapshot (Join-Path $report.session_dir 'backup')).Count -gt 0) 'Recovered upgrade has no retained backup evidence'
    Save "$Label.check.json" @{ status = 'PASSED'; checkpoint = $Checkpoint; old_product_bytes_and_registration_restored = $true; previous_receipt_sha256 = $receiptHash; session_dir = $report.session_dir }
}
function DiagnoseReadOnly([object]$Report, [string]$Label, [string]$ExpectedAssessment) {
    $before = ProductSnapshot; $sessionBefore = @(Snapshot $Report.session_dir)
    InvokeProcess $script:CliBinary @('diagnose', '--session', $Report.session_dir) $Label
    $diagnostic = Json (Join-Path $script:RunRoot "$Label.stdout.log")
    Require ($diagnostic.read_only -and -not $diagnostic.automatic_retry_allowed -and $diagnostic.app_id -ceq $script:AppId -and $diagnostic.assessment -ceq $ExpectedAssessment) 'Session diagnosis identity/assessment/read-only contract differs'
    $after = ProductSnapshot
    Require (($before | ConvertTo-Json -Depth 50 -Compress) -ceq ($after | ConvertTo-Json -Depth 50 -Compress)) 'Diagnosis changed product resources'
    AssertFiles $Report.session_dir $sessionBefore -Exact
    Save "$Label.check.json" @{ status = 'PASSED'; assessment = $diagnostic.assessment; product_and_session_unchanged = $true }
}
function ReadJournal([object]$Report, [ValidateSet('upgrade-journal.jsonl', 'engine-journal.jsonl')][string]$Name = 'upgrade-journal.jsonl') {
    $path = Inside (Join-Path $Report.session_dir $Name) $script:RunRoot
    Require (Test-Path -LiteralPath $path -PathType Leaf) 'Native transaction journal missing'
    $journal = @([IO.File]::ReadAllLines($path, $script:Utf8) | ForEach-Object { $_ | ConvertFrom-Json })
    Require ($journal.Count -gt 0 -and $journal[0].event -ceq 'begin') 'Native transaction journal has no begin record'
    foreach ($event in $journal) { Require ($event.schema_version -eq 1 -and $event.task_id -ceq $Report.task_id) 'Journal has an unsupported schema or foreign task identity' }
    return $journal
}
function VerifyPartialInstallCleanup([object]$Report, [object]$Package, [string]$Label, [int]$ExpectedPublished = 2) {
    $work = @($Report.events | Where-Object { $_.kind -ceq 'progress' -and $_.phase -in @('extracting', 'running') -and $_.completed -ge 1 })
    Require ($work.Count -gt 0 -and ($work.completed | Measure-Object -Maximum).Maximum -eq $ExpectedPublished) 'Cancellation/failure work progress did not reach the configured file boundary'
    $cleanup = @($Report.events | Where-Object { $_.kind -ceq 'progress' -and $_.phase -ceq 'cleanup_completed' })
    $terminal = @($Report.events | Where-Object kind -CEQ 'finish')
    Require ($cleanup.Count -eq 1 -and $terminal.Count -eq 1 -and $cleanup[0].seq -lt $terminal[0].seq -and $terminal[0].result -ceq $Report.result -and $terminal[0].code -eq $Report.coordinator_exit_code) 'Cleanup and terminal events disagree or are out of order'
    if ($Report.result -ceq 'Cancelled') {
        $acknowledged = @($Report.events | Where-Object kind -CEQ 'cancel_received')
        Require ($acknowledged.Count -eq 1 -and $acknowledged[0].seq -lt $cleanup[0].seq) 'Cancelled result lacks an authenticated cancellation acknowledgement before cleanup'
    }
    $journal = @(ReadJournal $Report 'engine-journal.jsonl')
    $published = @($journal | Where-Object event -CEQ 'published')
    Require ($published.Count -eq $ExpectedPublished -and $journal[-1].event -ceq 'rolled_back' -and $journal[-1].result -ceq 'verified') 'Journal does not prove partial publication followed by verified rollback'
    Require (@($journal | Where-Object event -In @('committed', 'rollback_incomplete')).Count -eq 0 -and @($journal | Where-Object event -CEQ 'rollback_begin').Count -eq 1) 'Cancelled/failed installation has conflicting journal outcomes'
    $rollbackIndex = -1
    for ($i = 0; $i -lt $journal.Count; $i++) { if ($journal[$i].event -ceq 'rollback_begin') { $rollbackIndex = $i; break } }
    $proof = foreach ($event in $published) {
        $path = Inside $event.path $script:InstallRoot
        $relative = $path.Substring($script:InstallRoot.Length + 1)
        $expected = @($Package.source_files | Where-Object path -CEQ $relative)
        Require ($expected.Count -eq 1 -and $event.sha256 -ceq $expected[0].sha256 -and $event.result -ceq 'created') 'Published journal resource differs from the independently captured source'
        $publishIndex = [Array]::IndexOf($journal, $event)
        $intentIndex = -1; $deleteIndex = -1
        for ($i = 0; $i -lt $journal.Count; $i++) {
            $item = $journal[$i]
            if ($item.event -in @('publish_intent', 'rollback_delete_intent') -and (SamePath $item.path $path) -and $item.sha256 -ceq $expected[0].sha256) {
                if ($item.event -ceq 'publish_intent') { $intentIndex = $i } else { $deleteIndex = $i }
            }
        }
        Require ($intentIndex -ge 0 -and $intentIndex -lt $publishIndex -and $publishIndex -lt $rollbackIndex -and $deleteIndex -gt $rollbackIndex) 'Publish intent/publication/rollback ordering is invalid'
        Require (-not (Test-Path -LiteralPath $path)) 'Rolled-back published resource is still installed'
        @{ path = $relative; source_sha256 = $expected[0].sha256; publication_index = $publishIndex; rollback_delete_index = $deleteIndex; absent_after_cleanup = $true }
    }
    # Staged bytes remain independently readable after cleanup; verify every payload
    # against the source captured before build, not the recovery map's planned entries.
    $staged = for ($i = 0; $i -lt @($Package.manifest.files).Count; $i++) {
        $file = $Package.manifest.files[$i]
        $expected = @($Package.source_files | Where-Object path -CEQ $file.path)
        Require ($expected.Count -eq 1) 'Staged manifest path is missing from independent source'
        [pscustomobject]@{ path = ('payload-{0:d6}' -f $i); size = $expected[0].size; sha256 = $expected[0].sha256 }
    }
    AssertFiles (Join-Path $Report.session_dir 'staging') @($staged)
    Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot '.avelin-receipt.json')) -and -not (Test-Path -LiteralPath (Join-Path $script:InstallRoot '.avelin\upgrade-state.json'))) 'Cleaned partial install left a receipt or unfinished marker'
    Save "$Label.publication-proof.json" @{ status = 'PASSED'; task_id = $Report.task_id; published_resources = @($proof); staged_payload_verified = @($staged).Count; journal_outcome = $journal[-1]; observation_limit = 'Durable publication records and independently hashed staging/output state; file writes were not sampled live.' }
}
function UnknownUpgradeFixture([ValidateSet('after-commit', 'rollback')][string]$Checkpoint) {
    NewProduct $Checkpoint
    $old = BuildPackage "$Checkpoint-a" '1.0.0' 'a'; $new = BuildPackage "$Checkpoint-b" '1.1.0' 'b'
    $sentinels = @(AddSentinels); $installPath = Join-Path $script:RunRoot "$Checkpoint-install.report.json"
    InvokeProcess $old.setup @('--silent', '--install-dir', $script:InstallRoot, '--report', $installPath) "$Checkpoint-install" $installPath
    $installed = OperationReport $installPath 'install' "$Checkpoint-install" $old
    $oldReceipt = VerifyInstalled $old $installed "$Checkpoint-installed" @('desktop', 'start_menu')
    Save "$Checkpoint.before.json" (ProductSnapshot)
    $reportPath = Join-Path $script:RunRoot "$Checkpoint-upgrade.report.json"
    InvokeProcess $new.setup @('--silent', '--operation', 'upgrade', '--install-dir', $script:InstallRoot, '--engine-fault', $Checkpoint, '--report', $reportPath) "$Checkpoint-upgrade" $reportPath -ExpectedExit 4
    $report = OperationReport $reportPath 'upgrade' "$Checkpoint-upgrade" $new 'Unknown' 4
    AssertFiles $script:InstallRoot $sentinels
    $journal = @(ReadJournal $report); $marker = Join-Path $script:InstallRoot '.avelin\upgrade-state.json'
    if ($Checkpoint -ceq 'after-commit') {
        $receipt = VerifyInstalled $new $report 'after-commit-installed-b' @('desktop', 'start_menu')
        Require ($receipt.installation_id -ceq $oldReceipt.installation_id -and $receipt.previous_commit_id -ceq $oldReceipt.commit_id -and $receipt.commit_id -cne $oldReceipt.commit_id) 'Committed result lost its upgrade lineage'
        Require ($journal[-1].event -ceq 'committed' -and $journal[-1].result -ceq 'verified' -and @($journal | Where-Object event -In @('rollback_begin', 'rolled_back', 'rollback_incomplete')).Count -eq 0) 'Post-commit confirmation fault rolled back or lost the verified commit'
        foreach ($relative in @('app.exe', 'retired-user.txt', 'obsolete\retired.txt', 'obsolete')) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $relative))) "Committed B retained old owned resource: $relative" }
        DiagnoseReadOnly $report 'after-commit-diagnose' 'NeedsReview'
        Save 'after-commit.resources.json' (ProductSnapshot)
        Require (-not (Test-Path -LiteralPath $marker)) 'Verified committed upgrade retained an unresolved transaction marker'
        UninstallRegistered $new 'after-commit-uninstall'; AssertFiles $script:InstallRoot $sentinels -Exact
        Require (-not (Test-Path -LiteralPath $script:DesktopPath) -and -not (Test-Path -LiteralPath $script:StartMenuPath)) 'Post-commit fixture shortcuts remain'
    } else {
        Require (Test-Path -LiteralPath $marker -PathType Leaf) 'Incomplete recovery lost its transaction marker'
        Require (@($journal | Where-Object event -CEQ 'rollback_incomplete').Count -eq 1 -and @($journal | Where-Object event -CEQ 'committed').Count -eq 0) 'Incomplete rollback has incorrect terminal evidence'
        $backup = Join-Path $report.session_dir 'backup'; Require (@(Snapshot $backup).Count -gt 0) 'Incomplete rollback lost its backups'
        $recovery = Join-Path $report.session_dir 'engine-recovery.json'; Require (Test-Path -LiteralPath $recovery -PathType Leaf) 'Incomplete rollback recovery record missing'
        $receiptPath = Join-Path $script:InstallRoot '.avelin-receipt.json'
        if (Test-Path -LiteralPath $receiptPath) { Require ((Json $receiptPath).commit_id -ceq $oldReceipt.commit_id) 'Incomplete rollback published a new installed receipt' }
        DiagnoseReadOnly $report 'rollback-diagnose' 'NeedsReview'
        RejectOperation $new 'upgrade' 'rollback-reject-reentry' '(?i)(marker|recover|incomplete|unfinished|pending|not finalized|not finalised)'
        AssertFiles $script:InstallRoot $sentinels
        Save 'rollback.retained.json' @{ status = 'EXPECTED_UNKNOWN_SCENE_RETAINED'; app_id = $script:AppId; install_dir = $script:InstallRoot; marker = $marker; marker_sha256 = Hash $marker; backup = @(Snapshot $backup); recovery = Json $recovery; resources = ProductSnapshot; reason = 'A deliberately incomplete recovery is retained for diagnosis. No automatic cleanup, retry or marker deletion is authorized by this harness.' }
    }
}

Require ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'This harness requires Windows'
. (Join-Path $PSScriptRoot 'windows-shell-link.ps1')
Require (([bool]$Nsis) -eq ([bool]$Bridge)) 'Specify both Nsis and Bridge to include the optional migration case'
$projectRoot = Full (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$script:RunRoot = Inside $OutputRoot (Join-Path $projectRoot 'target')
Require (-not (Test-Path -LiteralPath $script:RunRoot)) 'OutputRoot must be a fresh directory beneath this workspace target'
foreach ($binary in @($Runtime, $Cli, $RustWorker, $Sample)) { Require (Test-Path -LiteralPath $binary -PathType Leaf) "Missing prerequisite: $binary" }
if ($Nsis) { $Nsis = Full $Nsis; Require ((Test-Path -LiteralPath $Nsis -PathType Leaf) -and (Test-Path -LiteralPath $Bridge -PathType Leaf)) 'Missing migration NSIS/bridge prerequisite' }
$null = [IO.Directory]::CreateDirectory($script:RunRoot)
$script:TempRoot = Join-Path $script:RunRoot 'temp'; $script:LocalRoot = Join-Path $script:RunRoot 'local-app-data'
$null = [IO.Directory]::CreateDirectory($script:TempRoot); $null = [IO.Directory]::CreateDirectory($script:LocalRoot)
$script:UserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$script:DesktopFolder = [Environment]::GetFolderPath([Environment+SpecialFolder]::DesktopDirectory)
$script:ProgramsFolder = [Environment]::GetFolderPath([Environment+SpecialFolder]::Programs)
$script:RuntimeBinary = Join-Path $script:RunRoot 'tools\avelin-app.exe'; CopyNew $Runtime $script:RuntimeBinary
$script:CliBinary = Join-Path $script:RunRoot 'tools\avelin-cli.exe'; CopyNew $Cli $script:CliBinary
$script:WorkerBinary = Join-Path $script:RunRoot 'tools\avelin-engine.exe'; CopyNew $RustWorker $script:WorkerBinary
$script:SampleBinary = Join-Path $script:RunRoot 'tools\nimbus-demo.exe'; CopyNew $Sample $script:SampleBinary
if ($Bridge) { $script:BridgeBinary = Join-Path $script:RunRoot 'tools\avelin_nsis_bridge.dll'; CopyNew $Bridge $script:BridgeBinary }
$summary = [ordered]@{ schema_version = 1; status = 'RUNNING'; started_at_utc = [DateTime]::UtcNow.ToString('o'); run_directory = $script:RunRoot; scope = 'Actual Rust build/install/cancel/partial-write failure/upgrade/run/owned uninstall/reinstall with independent resources and optional NSIS migration'; gui_executed = $false; performance_measured = $false; signing_verified = $false; environment = 'Current Windows user; Rust child PATH only contains Windows/System32 and Windows'; migration_requested = [bool]$Nsis; completed_cases = @() }
try {
    NewProduct 'native'
    $a = BuildPackage 'native-a' '1.0.0' 'a'; $b = BuildPackage 'native-b' '1.1.0' 'b'
    $summary.packages = @($a, $b); $sentinels = @(AddSentinels); Save 'native.sentinels.json' $sentinels

    $collision = Join-Path $script:InstallRoot 'shared.txt'; WriteNew $collision 'Unknown pre-install collision'
    RejectOperation $a 'install' 'install-collision' '(?i)(conflict|existing resource)'
    ArchiveOwned $collision 'install-collision.txt' (Hash $collision); $summary.completed_cases += 'install-collision'
    foreach ($case in @(@{ label = 'cancel-install'; flag = '--cancel-after-files'; result = 'Cancelled'; code = 2 }, @{ label = 'fail-install'; flag = '--fault-after-files'; result = 'Failed'; code = 3 })) {
        $reportPath = Join-Path $script:RunRoot "$($case.label).report.json"
        InvokeProcess $a.setup @('--silent', '--install-dir', $script:InstallRoot, '--report', $reportPath, $case.flag, '2', '--step-delay-ms', '20') $case.label $reportPath -ExpectedExit $case.code
        $report = OperationReport $reportPath 'install' $case.label $a $case.result $case.code
        VerifyPartialInstallCleanup $report $a $case.label
        AssertFiles $script:InstallRoot $sentinels -Exact
        Require (@(RegistryState | Where-Object exists).Count -eq 0 -and -not (Test-Path -LiteralPath $script:DesktopPath) -and -not (Test-Path -LiteralPath $script:StartMenuPath)) 'Cancellation/failure left registration or shortcuts'
        Save "$($case.label).resources.json" (ProductSnapshot); $summary.completed_cases += $case.label
    }
    $installPath = Join-Path $script:RunRoot 'install-a.report.json'
    InvokeProcess $a.setup @('--silent', '--install-dir', $script:InstallRoot, '--report', $installPath) 'install-a' $installPath
    $installed = OperationReport $installPath 'install' 'install-a' $a
    $receiptA = VerifyInstalled $a $installed 'installed-a' @('desktop', 'start_menu'); AssertFiles $script:InstallRoot $sentinels
    $summary.completed_cases += 'install-a'
    RejectOperation $a 'upgrade' 'same-version' '(?i)(newer|version|downgrade|版本)'; $summary.completed_cases += 'same-version'

    $collision = Join-Path $script:InstallRoot 'added.txt'; WriteNew $collision 'Unknown upgrade collision'
    RejectOperation $b 'upgrade' 'upgrade-collision' '(?i)(conflict|exists|unknown)'
    ArchiveOwned $collision 'upgrade-collision.txt' (Hash $collision); $summary.completed_cases += 'upgrade-collision'
    $shared = Join-Path $script:InstallRoot 'shared.txt'; [IO.File]::AppendAllText($shared, 'User changed replacement', $script:Utf8)
    RejectOperation $b 'upgrade' 'modified-replacement' '(?i)(changed|modified|mismatch)'
    ArchiveOwned $shared 'modified-replacement.txt' (Hash $shared); CopyNew (Join-Path $a.source_directory 'shared.txt') $shared
    $summary.completed_cases += 'modified-replacement'
    $originalStart = Join-Path $script:RunRoot 'original-start-menu.lnk'; CopyNew $script:StartMenuPath $originalStart
    $changedStartHash = ModifyShortcut $script:StartMenuPath 'selected'
    RejectOperation $b 'upgrade' 'modified-selected-shortcut' '(?i)(changed|modified|mismatch|shortcut)'
    ArchiveOwned $script:StartMenuPath 'modified-selected-shortcut.lnk' $changedStartHash
    Require (-not (Test-Path -LiteralPath $script:StartMenuPath)) 'Reserved shortcut unexpectedly reappeared'
    [IO.File]::Copy($originalStart, $script:StartMenuPath, $false)
    Require ((Hash $script:StartMenuPath) -ceq (Hash $originalStart)) 'Restored fixture shortcut differs'
    $summary.completed_cases += 'modified-selected-shortcut'

    $retired = Join-Path $script:InstallRoot 'retired-user.txt'; [IO.File]::AppendAllText($retired, 'Preserved user edit', $script:Utf8); $retiredRecord = FileRecord $retired
    $desktopHash = ModifyShortcut $script:DesktopPath 'unselected'
    RecoverableUpgradeFault $b 'upgrade-before-commit' 'before-commit' @('--desktop-shortcut', 'false', '--start-menu-shortcut', 'true')
    $summary.completed_cases += 'upgrade-before-commit-restores-a'
    $receiptBeforeHash = Hash (Join-Path $script:InstallRoot '.avelin-receipt.json'); Save 'upgrade.before.json' (ProductSnapshot)
    $upgradePath = Join-Path $script:RunRoot 'upgrade-b.report.json'
    InvokeProcess $b.setup @('--silent', '--operation', 'upgrade', '--install-dir', $script:InstallRoot, '--desktop-shortcut', 'false', '--start-menu-shortcut', 'true', '--report', $upgradePath) 'upgrade-b' $upgradePath
    $upgraded = OperationReport $upgradePath 'upgrade' 'upgrade-b' $b
    $receiptB = VerifyInstalled $b $upgraded 'upgraded-b' @('start_menu')
    Require ($receiptB.installation_id -ceq $receiptA.installation_id -and $receiptB.previous_commit_id -ceq $receiptA.commit_id -and $receiptB.commit_id -cne $receiptA.commit_id -and $receiptB.task_id -cne $receiptA.task_id) 'Upgrade lineage differs'
    $upgradePlan = (Json (Join-Path $upgraded.session_dir 'install-plan.json')).upgrade
    Require ($upgradePlan.previous_receipt_sha256 -ceq $receiptBeforeHash -and $upgradePlan.previous_manifest_sha256 -ceq $a.manifest_sha256 -and $upgradePlan.installation_id -ceq $receiptA.installation_id) 'Upgrade did not bind old receipt/package/installation'
    foreach ($relative in @('app.exe', 'obsolete\retired.txt', 'obsolete', '.avelin\upgrade-state.json')) { Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $relative))) "Obsolete resource remains: $relative" }
    AssertFiles $script:InstallRoot (@($sentinels) + @($retiredRecord)); Require ((Hash $script:DesktopPath) -ceq $desktopHash) 'Modified unselected shortcut changed'
    Save 'upgrade.after.json' (ProductSnapshot); $summary.completed_cases += 'upgrade-b'
    $beforeLaunch = ProductSnapshot; InvokeProcess (Join-Path $script:InstallRoot $b.manifest.entry) @() 'launch-b'
    Require ([IO.File]::ReadAllText((Join-Path $script:RunRoot 'launch-b.stdout.log')).Replace("`r`n", "`n") -ceq "Nimbus Notes — Avelin M0 sample application`nThis executable is a local installation test payload.`n" -and (Get-Item -LiteralPath (Join-Path $script:RunRoot 'launch-b.stderr.log')).Length -eq 0) 'Installed sample did not run with expected output'
    AssertUnchanged $beforeLaunch 'launch-b'; $summary.completed_cases += 'launch-b'
    RejectOperation $a 'upgrade' 'downgrade' '(?i)(newer|version|downgrade|版本)'; $summary.completed_cases += 'downgrade'

    $shared = Join-Path $script:InstallRoot 'shared.txt'; [IO.File]::AppendAllText($shared, 'User edited installed B', $script:Utf8); $modifiedB = FileRecord $shared
    UninstallRegistered $b 'uninstall-b'
    AssertFiles $script:InstallRoot (@($sentinels) + @($retiredRecord, $modifiedB)) -Exact
    Require (-not (Test-Path -LiteralPath $script:StartMenuPath) -and (Hash $script:DesktopPath) -ceq $desktopHash) 'Uninstall did not respect shortcut ownership'
    Save 'uninstall-b.resources.json' (ProductSnapshot); $summary.completed_cases += 'uninstall-b-preserves-user-edits'
    RejectOperation $b 'install' 'reinstall-collision' '(?i)(conflict|existing resource)'; $summary.completed_cases += 'reinstall-collision'
    ArchiveOwned $shared 'uninstall-preserved-shared.txt' $modifiedB.sha256
    ArchiveOwned $script:DesktopPath 'uninstall-preserved-desktop.lnk' $desktopHash
    $reinstallPath = Join-Path $script:RunRoot 'reinstall-b.report.json'
    InvokeProcess $b.setup @('--silent', '--install-dir', $script:InstallRoot, '--desktop-shortcut', 'false', '--start-menu-shortcut', 'true', '--report', $reinstallPath) 'reinstall-b' $reinstallPath
    $reinstalled = OperationReport $reinstallPath 'install' 'reinstall-b' $b
    $newReceipt = VerifyInstalled $b $reinstalled 'reinstalled-b' @('start_menu')
    Require ($newReceipt.installation_id -cne $receiptB.installation_id) 'Reinstall reused the removed installation identity'
    UninstallDirect $b 'final-uninstall'
    AssertFiles $script:InstallRoot (@($sentinels) + @($retiredRecord)) -Exact
    Require (-not (Test-Path -LiteralPath $script:DesktopPath) -and -not (Test-Path -LiteralPath $script:StartMenuPath)) 'Final native fixture shortcut remains'
    Save 'native.final.json' (ProductSnapshot); $summary.completed_cases += 'reinstall-and-direct-uninstall'

    if ($Nsis) {
        NewProduct 'migration'
        $legacy = BuildPackage 'migration-nsis-a' '1.0.0' 'a' 'nsis'; $native = BuildPackage 'migration-rust-b' '1.1.0' 'b' 'rust'
        $migrationSentinels = @(AddSentinels); $legacyPath = Join-Path $script:RunRoot 'migration-install-a.report.json'
        InvokeProcess $legacy.setup @('--silent', '--install-dir', $script:InstallRoot, '--report', $legacyPath) 'migration-install-a' $legacyPath
        $legacyReport = OperationReport $legacyPath 'install' 'migration-install-a' $legacy
        $legacyReceipt = VerifyInstalled $legacy $legacyReport 'migration-installed-a' @('desktop', 'start_menu')
        $legacyUninstallerHash = Hash (Join-Path $script:InstallRoot 'uninstall.exe')
        RecoverableUpgradeFault $native 'migration-before-commit' 'before-commit'
        $summary.completed_cases += 'nsis-to-rust-before-commit-restores-nsis'
        $migrationPath = Join-Path $script:RunRoot 'migration-upgrade-b.report.json'
        InvokeProcess $native.setup @('--silent', '--operation', 'upgrade', '--install-dir', $script:InstallRoot, '--report', $migrationPath) 'migration-upgrade-b' $migrationPath
        $migrationReport = OperationReport $migrationPath 'upgrade' 'migration-upgrade-b' $native
        $nativeReceipt = VerifyInstalled $native $migrationReport 'migration-upgraded-b' @('desktop', 'start_menu')
        Require ($nativeReceipt.installation_id -ceq $legacyReceipt.installation_id -and $nativeReceipt.previous_commit_id -ceq $legacyReceipt.commit_id -and $nativeReceipt.uninstaller_sha256 -cne $legacyUninstallerHash) 'Migration lineage or old uninstaller handoff failed'
        AssertFiles $script:InstallRoot $migrationSentinels
        UninstallRegistered $native 'migration-uninstall-b'; AssertFiles $script:InstallRoot $migrationSentinels -Exact
        Require (-not (Test-Path -LiteralPath $script:DesktopPath) -and -not (Test-Path -LiteralPath $script:StartMenuPath)) 'Migration shortcut remains'
        Save 'migration.final.json' (ProductSnapshot); $summary.completed_cases += 'nsis-to-rust-normal-migration'
    }
    UnknownUpgradeFixture 'after-commit'; $summary.completed_cases += 'post-commit-confirmation-loss-preserves-committed-b'
    UnknownUpgradeFixture 'rollback'; $summary.completed_cases += 'incomplete-rollback-diagnosis-and-reentry-refusal'
    $summary.status = 'PASSED'
    $summary.uncovered = @('GUI interaction', 'uninstall execution fault checkpoints', 'process interruption and recovery interruption', 'physical volume exhaustion', 'historical NSIS runtime version matrix', 'signing and independent environments')
} catch {
    $summary.status = 'FAILED'; $summary.failure = $_.Exception.Message
    # Keep source locations separately from private stack paths so CI can publish
    # a useful failure trace without exposing the account or fixture directory.
    $summary.failure_trace = @(
        foreach ($frame in ($_.ScriptStackTrace -split '\r?\n')) {
            if ($frame -match '^at (?<function>[A-Za-z0-9_<>-]+), .+: line (?<line>[0-9]+)$') {
                [pscustomobject]@{ function = $Matches.function; line = [int]$Matches.line }
            }
        }
    )
    if (Get-Variable InstallRoot -Scope Script -ErrorAction SilentlyContinue) { try { Save 'failure.resources.json' (ProductSnapshot) } catch { $summary.snapshot_failure = $_.Exception.Message } }
    throw
} finally {
    $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o')
    $summary.process_boundary = 'Actual setup, launch and launcher exit plus stdout/stderr EOF observed. Registered uninstall coordinator completion is represented by its task report, without independently sampling its OS exit. Direct uninstall bootstrap completion is followed by the matching default session report, diagnose identity-checked coordinator/backend process states, and independent resource assertions; bootstrap exit alone is not task success.'
    Save 'summary.json' $summary
    Write-Host "Evidence retained: $script:RunRoot"
}
