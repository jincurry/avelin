#requires -Version 7.2
# Observe one actual upgrade window; no UI driving and no scripted visual acceptance.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$FixturePath,
    [Parameter(Mandatory)][ValidateSet('Prepare', 'Upgrade', 'Finish')][string]$Operation,
    [ValidateRange(30, 3600)][int]$TimeoutSeconds = 1200
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
function Require([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Full([string]$Path) { [IO.Path]::GetFullPath($Path).TrimEnd([char]'\', [char]'/') }
function SamePath([string]$Left, [string]$Right) { [string]::Equals((Full $Left), (Full $Right), [StringComparison]::OrdinalIgnoreCase) }
function ReadJson([string]$Path) { [IO.File]::ReadAllText($Path) | ConvertFrom-Json }
function Save([string]$Name, [object]$Value) { [IO.File]::WriteAllText((Join-Path $script:EvidenceRoot $Name), ($Value | ConvertTo-Json -Depth 40), [Text.UTF8Encoding]::new($false)) }
function Inside([string]$Path) {
    $absolute = Full $Path
    Require ($absolute.StartsWith($script:RunRoot + '\', [StringComparison]::OrdinalIgnoreCase)) 'Fixture path escaped its run directory'
    for ($cursor = $absolute; $cursor; $cursor = Split-Path -Parent $cursor) {
        if (Test-Path -LiteralPath $cursor) { Require (((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) 'Reparse point in fixture' }
    }
    return $absolute
}
function Registration {
    foreach ($view in @([Microsoft.Win32.RegistryView]::Registry32, [Microsoft.Win32.RegistryView]::Registry64)) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $view); $key = $null
        try {
            $key = $base.OpenSubKey('Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:Fixture.app_id, $false)
            $values = [ordered]@{}
            if ($key) { foreach ($name in ($key.GetValueNames() | Sort-Object)) { $values[$name] = $key.GetValue($name) } }
            [pscustomobject]@{ view = "$view"; exists = $null -ne $key; values = $values }
        } finally { if ($key) { $key.Dispose() }; $base.Dispose() }
    }
}
function VerifyInstalled([object]$Manifest, [string]$Source, [int]$LinkCount) {
    $receiptPath = Inside (Join-Path $script:Fixture.install_dir '.avelin-receipt.json'); $receipt = ReadJson $receiptPath
    Require ($receipt.app_id -ceq $script:Fixture.app_id -and $receipt.app_version -ceq $Manifest.app_version -and (SamePath $receipt.install_dir $script:Fixture.install_dir) -and @($receipt.files).Count -eq @($Manifest.files).Count -and @($receipt.shortcuts).Count -eq $LinkCount) 'Installed receipt identity or inventory mismatch'
    foreach ($file in $Manifest.files) {
        $sourceFile = Inside (Join-Path $Source $file.path); $installed = Inside (Join-Path $script:Fixture.install_dir $file.path)
        $record = @($receipt.files | Where-Object path -CEQ $file.path)
        Require ((Hash $sourceFile) -ceq $file.sha256 -and (Hash $installed) -ceq $file.sha256 -and (Get-Item -LiteralPath $installed).Length -eq $file.size -and $record.Count -eq 1 -and $record[0].sha256 -ceq $file.sha256) 'Installed payload differs from independent source'
    }
    Require ((Hash (Join-Path $script:Fixture.install_dir '.avelin\avelin-app.exe')) -ceq $Manifest.runtime_sha256 -and (Hash (Join-Path $script:Fixture.install_dir 'uninstall.exe')) -ceq $receipt.uninstaller_sha256) 'Installed runtime or uninstaller hash mismatch'
    $packageManifest = if ($Manifest.app_version -ceq $script:ManifestA.app_version) { $script:Fixture.manifest_a } else { $script:Fixture.manifest_b }
    Require ((Hash (Join-Path $script:Fixture.install_dir '.avelin\manifest.json')) -ceq (Hash $packageManifest)) 'Installed manifest differs from the accepted package'
    $shell = New-Object -ComObject WScript.Shell
    try {
        foreach ($record in $receipt.shortcuts) {
            $expected = switch ($record.kind) { 'desktop' { $script:Fixture.desktop_path }; 'start_menu' { $script:Fixture.start_menu_path }; default { throw 'Unexpected shortcut kind' } }
            Require ((SamePath $record.path $expected) -and (Hash $record.path) -ceq $record.sha256 -and (SamePath $record.target (Join-Path $script:Fixture.install_dir $Manifest.entry))) 'Shortcut ownership mismatch'
            $link = $shell.CreateShortcut($record.path)
            try { Require ((SamePath $link.TargetPath $record.target) -and $link.Arguments -ceq '' -and (SamePath $link.WorkingDirectory $script:Fixture.install_dir)) 'Shortcut COM target, arguments or working directory mismatch' }
            finally { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($link) }
        }
    } finally { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) }
    Require (@(Registration | Where-Object exists).Count -gt 0) 'Product registration missing'
    return $receipt
}

$FixturePath = Full $FixturePath; $script:RunRoot = Full (Split-Path -Parent $FixturePath); $script:Fixture = ReadJson $FixturePath
$fixture = $script:Fixture
foreach ($path in @($fixture.setup_a, $fixture.setup_b, $fixture.manifest_a, $fixture.manifest_b, $fixture.source_a, $fixture.source_b, $fixture.install_dir, $fixture.temp, $fixture.local_app_data, $fixture.lock_path)) { $null = Inside $path }
Require ((Hash $fixture.setup_a) -ceq $fixture.setup_a_sha256 -and (Hash $fixture.setup_b) -ceq $fixture.setup_b_sha256 -and (Hash $fixture.manifest_a) -ceq $fixture.manifest_a_sha256 -and (Hash $fixture.manifest_b) -ceq $fixture.manifest_b_sha256) 'Fixture package or manifest changed'
$manifestA = ReadJson $fixture.manifest_a; $manifestB = ReadJson $fixture.manifest_b
Require ($manifestA.app_id -ceq $fixture.app_id -and $manifestB.app_id -ceq $fixture.app_id -and $manifestA.app_version -cne $manifestB.app_version) 'Fixture product/version mismatch'
$script:EvidenceRoot = Inside (Join-Path $script:RunRoot ('gui-' + $Operation.ToLowerInvariant() + '-' + [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)))
Require (-not (Test-Path -LiteralPath $script:EvidenceRoot)) 'Evidence directory already exists'
$null = New-Item -ItemType Directory -Path $script:EvidenceRoot
$report = Join-Path $script:EvidenceRoot 'report.json'
$info = [Diagnostics.ProcessStartInfo]::new(); $info.UseShellExecute = $false; $info.CreateNoWindow = $true
$info.WindowStyle = if ($Operation -eq 'Upgrade') { [Diagnostics.ProcessWindowStyle]::Normal } else { [Diagnostics.ProcessWindowStyle]::Hidden }
$info.WorkingDirectory = $script:RunRoot; $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
$info.StandardOutputEncoding = [Text.UTF8Encoding]::new($false, $true); $info.StandardErrorEncoding = $info.StandardOutputEncoding
$info.Environment['TEMP'] = $fixture.temp; $info.Environment['TMP'] = $fixture.temp; $info.Environment['LOCALAPPDATA'] = $fixture.local_app_data
if ($Operation -eq 'Prepare') {
    Require (-not (Test-Path -LiteralPath $fixture.install_dir) -and @(Registration | Where-Object exists).Count -eq 0 -and -not (Test-Path -LiteralPath $fixture.desktop_path) -and -not (Test-Path -LiteralPath $fixture.start_menu_path)) 'GUI fixture already exists or has registered resources'
    $null = New-Item -ItemType Directory -Path (Join-Path $fixture.install_dir 'user-data') -Force
    $sentinels = foreach ($relative in @('user-kept.txt', 'user-data\用户文件.txt')) {
        $path = Inside (Join-Path $fixture.install_dir $relative)
        [IO.File]::WriteAllText($path, ('GUI upgrade user data ' + [Guid]::NewGuid().ToString()), [Text.UTF8Encoding]::new($false))
        [pscustomobject]@{ path = $relative; size = (Get-Item -LiteralPath $path).Length; sha256 = Hash $path }
    }
    [IO.File]::WriteAllText((Join-Path $script:RunRoot 'gui-sentinels.json'), ($sentinels | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
    $info.FileName = $fixture.setup_a
    foreach ($arg in @('--silent', '--install-dir', $fixture.install_dir, '--desktop-shortcut', 'true', '--start-menu-shortcut', 'true', '--report', $report)) { $info.ArgumentList.Add($arg) }
} elseif ($Operation -eq 'Upgrade') {
    $original = VerifyInstalled $manifestA $fixture.source_a 2
    Save 'previous-receipt.json' $original
    $info.FileName = $fixture.setup_b
    foreach ($arg in @('--gui', '--operation', 'upgrade', '--install-dir', $fixture.install_dir, '--desktop-shortcut', 'false', '--start-menu-shortcut', 'true', '--report', $report)) { $info.ArgumentList.Add($arg) }
} else {
    $original = VerifyInstalled $manifestB $fixture.source_b 1
    $resident = Inside (Join-Path $fixture.install_dir '.avelin\avelin-app.exe'); $backend = Inside (Join-Path $fixture.install_dir 'uninstall.exe'); $installedManifest = Inside (Join-Path $fixture.install_dir '.avelin\manifest.json')
    $registered = @(Registration | Where-Object exists)[0].values.QuietUninstallString
    $expected = '"{0}" --silent --operation uninstall --backend "{1}" --manifest "{2}" --install-dir "{3}"' -f $resident, $backend, $installedManifest, $fixture.install_dir
    Require ($registered -ceq $expected) 'Registered B quiet uninstall command mismatch'
    $info.FileName = $resident; $info.Arguments = $registered.Substring(('"' + $resident + '"').Length).TrimStart() + ' --report "' + $report + '"'
    Save 'registered-entry.json' @{ command = $registered; source = 'actual HKCU QuietUninstallString'; receipt = $original }
}
$process = [Diagnostics.Process]::new(); $process.StartInfo = $info; $observedCoordinator = $null
$state = [ordered]@{ operation = $Operation; fixture = $FixturePath; executable = $info.FileName; executable_sha256 = Hash $info.FileName; arguments = @($info.ArgumentList); raw_arguments = $info.Arguments; started_at_utc = [DateTime]::UtcNow.ToString('o'); report = $report; status = 'RUNNING'; visual_acceptance = 'NOT_ASSERTED_BY_SCRIPT' }
try {
    $null = $process.Start(); $state.launcher_pid = $process.Id
    $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync(); Save 'process.json' $state
    Write-Host ($state | ConvertTo-Json -Depth 10)
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($true) {
        $exited = $process.WaitForExit(250); $reported = $false; $currentReport = $null
        if (Test-Path -LiteralPath $report) { try { $currentReport = ReadJson $report; $reported = $currentReport.result -in @('Succeeded', 'Failed', 'Cancelled', 'Unknown') } catch { } }
        if ($reported -and $null -eq $observedCoordinator) {
            $identity = (ReadJson (Join-Path $currentReport.session_dir 'session.json')).coordinator
            $null = Inside $identity.executable; $state.coordinator = $identity
            try { $observedCoordinator = [Diagnostics.Process]::GetProcessById([int]$identity.pid); $null = $observedCoordinator.Handle } catch { $observedCoordinator = $null }
            if ($observedCoordinator) { Require ($observedCoordinator.StartTime.ToUniversalTime().ToFileTimeUtc() -eq [long]$identity.creation_time_100ns -and (SamePath $observedCoordinator.MainModule.FileName $identity.executable)) 'Coordinator process identity mismatch' }
            Save 'process.json' $state
        }
        if ($exited -and $stdout.IsCompleted -and $stderr.IsCompleted -and ($reported -or $process.ExitCode -ne 0)) { break }
        if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) { $state.status = 'TIMED_OUT_PROCESS_NOT_KILLED'; throw 'Observation timed out; process was not killed; inspect retained state before continuing' }
        if ($exited) { Start-Sleep -Milliseconds 100 }
    }
    $state.launcher_exit_code = $process.ExitCode
    [IO.File]::WriteAllText((Join-Path $script:EvidenceRoot 'stdout.log'), $stdout.GetAwaiter().GetResult(), [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText((Join-Path $script:EvidenceRoot 'stderr.log'), $stderr.GetAwaiter().GetResult(), [Text.UTF8Encoding]::new($false))
    Require ($process.ExitCode -eq 0 -and (Test-Path -LiteralPath $report)) 'Operation failed or has no final report'
    if ($Operation -eq 'Upgrade') { Require ($null -ne $observedCoordinator) 'GUI task process was not observed while active; lifecycle evidence is incomplete' }
    if ($observedCoordinator) { Require ($observedCoordinator.WaitForExit(5000)) 'Coordinator remains active'; $state.coordinator_exit_code = $observedCoordinator.ExitCode; Require ($observedCoordinator.ExitCode -eq 0) 'Coordinator exited with a failure' }
    $result = ReadJson $report; $null = Inside $result.session_dir
    Require ($result.result -ceq 'Succeeded' -and $result.exit_code -eq 0 -and $result.coordinator_exit_code -eq 0 -and -not $result.product_lock_retained -and (SamePath $result.install_dir $fixture.install_dir)) 'Task report did not confirm success'
    $plan = ReadJson (Join-Path $result.session_dir 'install-plan.json')
    $expectedOperation = switch ($Operation) { 'Prepare' { 'install' }; 'Upgrade' { 'upgrade' }; 'Finish' { 'uninstall' } }
    Require ($plan.operation -ceq $expectedOperation -and $plan.task_id -ceq $result.task_id -and $plan.app_id -ceq $fixture.app_id) 'Task plan identity/operation mismatch'
    if ($Operation -eq 'Finish') {
        foreach ($file in $manifestB.files) { Require (-not (Test-Path -LiteralPath (Join-Path $fixture.install_dir $file.path))) 'Owned B payload remained after uninstall' }
        foreach ($relative in @('.avelin-receipt.json', '.avelin', 'uninstall.exe')) { Require (-not (Test-Path -LiteralPath (Join-Path $fixture.install_dir $relative))) 'Owned internal resource remained after uninstall' }
        Require (-not (Test-Path -LiteralPath $fixture.desktop_path) -and -not (Test-Path -LiteralPath $fixture.start_menu_path) -and @(Registration | Where-Object exists).Count -eq 0) 'Shortcut or registration remained after uninstall'
        $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push($fixture.install_dir); $remaining = 0
        while ($pending.Count) {
            foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
                $safe = Inside $item.FullName
                if ($item.PSIsContainer) { $pending.Push($safe) } else { $remaining++ }
            }
        }
        Require ($remaining -eq 2) 'Unexpected remaining GUI fixture files'
    } else {
        $current = if ($Operation -eq 'Prepare') { VerifyInstalled $manifestA $fixture.source_a 2 } else { VerifyInstalled $manifestB $fixture.source_b 1 }
        Save 'installed-receipt.json' $current
        if ($Operation -eq 'Upgrade') {
            Require ($current.installation_id -ceq $original.installation_id -and $current.previous_commit_id -ceq $original.commit_id -and $current.task_id -ceq $result.task_id -and -not $plan.desktop_shortcut -and $plan.start_menu_shortcut -and -not (Test-Path -LiteralPath $fixture.desktop_path)) 'Upgrade lineage or selected shortcut mismatch'
            $launchPath = Join-Path $result.session_dir 'launch.json'; $launch = ReadJson $launchPath
            $entry = Inside (Join-Path $fixture.install_dir $manifestB.entry)
            Require ($launch.status -ceq 'started' -and $launch.process_id -gt 0 -and $launch.task_id -ceq $result.task_id -and (SamePath $launch.entry_path $entry) -and $launch.entry_sha256 -ceq (Hash $entry) -and @($launch.arguments).Count -eq 0) 'Explicit GUI Launch evidence is missing or does not bind the upgraded entry'
            Save 'launch.json' $launch
            foreach ($file in $manifestA.files) { if ($file.path -notin @($manifestB.files.path)) { Require (-not (Test-Path -LiteralPath (Join-Path $fixture.install_dir $file.path))) 'Obsolete A payload remained after GUI upgrade' } }
            Require (-not (Test-Path -LiteralPath (Join-Path $fixture.install_dir 'obsolete'))) 'Empty obsolete A directory remained after GUI upgrade'
            Require (-not (Test-Path -LiteralPath (Join-Path $fixture.install_dir '.avelin\upgrade-state.json'))) 'Upgrade marker remained'
        }
    }
    $sentinels = @(ReadJson (Join-Path $script:RunRoot 'gui-sentinels.json'))
    foreach ($file in $sentinels) { $path = Inside (Join-Path $fixture.install_dir $file.path); Require ((Hash $path) -ceq $file.sha256 -and (Get-Item -LiteralPath $path).Length -eq $file.size) 'User sentinel changed' }
    Require (-not (Test-Path -LiteralPath $fixture.lock_path)) 'Product lock remains after completed process'
    Save 'resource-verification.json' @{ status = 'PASSED'; operation = $Operation; plan = $plan; sentinels = $sentinels; registry = @(Registration); lock_absent = $true; visual_acceptance = 'NOT_ASSERTED_BY_SCRIPT' }
    $state.status = 'TASK_PASSED_VISUAL_CHECK_SEPARATE'; $state.report_sha256 = Hash $report; $state.session = $result.session_dir
} catch { if ($state.status -eq 'RUNNING') { $state.status = 'FAILED' }; $state.error = $_.Exception.Message; throw }
finally { $state.finished_at_utc = [DateTime]::UtcNow.ToString('o'); Save 'process.json' $state; if ($observedCoordinator) { $observedCoordinator.Dispose() }; $process.Dispose(); Write-Host "GUI evidence: $script:EvidenceRoot" }
