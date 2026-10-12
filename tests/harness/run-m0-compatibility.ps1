<#
.SYNOPSIS
Run a prebuilt M0 compatibility kit on Windows PowerShell 5.1 or PowerShell 7.
.DESCRIPTION
No development tools, runtime installation, elevation, policy changes, network
requests, recursive deletion, or process termination. Each run has new TEMP and
LOCALAPPDATA directories under ResultsRoot. Failed/unknown scenes are retained.
Checksums detect changed kit files; this unsigned kit is not authenticated by
its own checksum file. Obtain the kit and its checksum through a trusted source.
This probe does not certify interactive first frame, visual correctness, DPI,
accessibility, clean-image provenance, Defender or SmartScreen acceptance.
.PARAMETER ResultsRoot
Parent of a newly generated result directory. Choose persistent guest storage.
.PARAMETER PlanOnly
Print the plan. If KitRoot is supplied, also validate every kit file hash and
the independent fixture manifest. Starts no processes and creates no files.
.PARAMETER GuiInstall
Add --gui only to the standalone Setup install. A person must confirm options,
start installation and close the completed window without changing its target.
Uninstallation remains CLI. Automated checks never certify GUI interaction.
#>
[CmdletBinding()]
param(
    [string]$KitRoot,
    [string]$ResultsRoot,
    [string]$ImageId = 'NOT_SUPPLIED',
    [ValidateSet('DevelopmentHost', 'CleanVm', 'Unspecified')][string]$EnvironmentKind = 'Unspecified',
    [switch]$PlanOnly,
    [switch]$GuiInstall,
    [ValidateRange(10, 120)][int]$ProbeTimeoutSeconds = 30,
    [ValidateRange(30, 3600)][int]$OperationTimeoutSeconds = 180
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$utf8 = New-Object Text.UTF8Encoding($false)

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Get-SafePath([string]$Path, [string]$Root) {
    $absolute = [IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    if ($Root) {
        $base = [IO.Path]::GetFullPath($Root).TrimEnd('\', '/')
        Assert-True ($absolute.StartsWith($base + '\', [StringComparison]::OrdinalIgnoreCase)) "Path escapes its root: $absolute"
    }
    $cursor = $absolute
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force
            Assert-True (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse point rejected: $cursor"
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if ($null -eq $parent) { break }
        $cursor = $parent.FullName
    }
    return $absolute
}

function Resolve-KitFile([string]$RelativePath) {
    Assert-True (-not [IO.Path]::IsPathRooted($RelativePath)) 'Kit paths must be relative.'
    Assert-True ($RelativePath -notmatch '(^|[\\/])\.\.?([\\/]|$)|:') "Ambiguous kit path: $RelativePath"
    return Get-SafePath (Join-Path $script:KitDirectory $RelativePath) $script:KitDirectory
}

function Read-Json([string]$Path) {
    # PS5.1 ConvertFrom-Json emits a root JSON array as one pipeline object.
    # Returning the captured value enumerates it consistently across PS5/PS7.
    $value = [IO.File]::ReadAllText($Path, [Text.Encoding]::UTF8) | ConvertFrom-Json
    return $value
}

function Write-Json([string]$Name, [object]$Value) {
    $path = Get-SafePath (Join-Path $script:RunDirectory $Name) $script:RunDirectory
    [IO.File]::WriteAllText($path, ($Value | ConvertTo-Json -Depth 40), $utf8)
}

function Get-Snapshot([string]$Directory) {
    $base = Get-SafePath $Directory ''
    if (-not (Test-Path -LiteralPath $base -PathType Container)) { return @() }
    $pending = New-Object 'Collections.Generic.Stack[string]'
    $pending.Push($base)
    $files = @()
    while ($pending.Count -gt 0) {
        foreach ($item in @(Get-ChildItem -LiteralPath $pending.Pop() -Force)) {
            $path = Get-SafePath $item.FullName $base
            if ($item.PSIsContainer) { $pending.Push($path); continue }
            $files += [pscustomobject]@{
                path = $path.Substring($base.Length + 1).Replace('\', '/')
                size = $item.Length
                sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        }
    }
    return @($files | Sort-Object path)
}

function Assert-Snapshot([string]$Directory, [object[]]$Expected, [switch]$Exact) {
    $actual = @(Get-Snapshot $Directory)
    $byPath = @{}
    foreach ($file in $actual) { $byPath[$file.path] = $file }
    foreach ($file in $Expected) {
        $relative = ([string]$file.path).Replace('\', '/')
        Assert-True ($byPath.ContainsKey($relative)) "Missing file: $relative"
        Assert-True ($byPath[$relative].sha256 -ceq $file.sha256 -and $byPath[$relative].size -eq $file.size) "Content mismatch: $relative"
    }
    if ($Exact) { Assert-True ($actual.Count -eq @($Expected).Count) 'Unexpected files exist in the tested directory.' }
}

function Test-Kit {
    $indexPath = Resolve-KitFile 'checksums.json'
    $index = Read-Json $indexPath
    Assert-True ($index.schema_version -eq 1) 'Unsupported checksum schema.'
    $indexed = @{}
    foreach ($file in @($index.files)) {
        $relative = ([string]$file.path).Replace('\', '/')
        Assert-True ($relative -cne 'checksums.json' -and -not $indexed.ContainsKey($relative)) "Duplicate or self-referencing checksum entry: $relative"
        $path = Resolve-KitFile $relative
        Assert-True (Test-Path -LiteralPath $path -PathType Leaf) "Kit file missing: $relative"
        Assert-True ((Get-Item -LiteralPath $path).Length -eq $file.size) "Kit size differs: $relative"
        Assert-True ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $file.sha256) "Kit hash differs: $relative"
        $indexed[$relative] = $true
    }
    $all = @(Get-Snapshot $script:KitDirectory | Where-Object { $_.path -cne 'checksums.json' })
    Assert-True ($all.Count -eq $indexed.Count) 'Kit contains files absent from checksums.json.'
    foreach ($file in $all) { Assert-True ($indexed.ContainsKey($file.path)) "Unlisted kit file: $($file.path)" }
    $script:Kit = Read-Json (Resolve-KitFile 'kit.json')
    Assert-True ($script:Kit.schema_version -eq 1 -and $script:Kit.app_id -match '^dev\.avelin\.m0\.compat\.[a-f0-9]{32}$') 'Unexpected kit schema or test application ID.'
    foreach ($required in @('cli', 'runtime', 'bridge', 'ui_probe', 'setup', 'manifest', 'fixture_snapshot', 'guest_script')) {
        Assert-True ($indexed.ContainsKey([string]$script:Kit.paths.$required)) "Required artifact is unlisted: $required"
    }
    # The executing harness must be the exact version bundled with this kit.
    Assert-True ((Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash -eq (Get-FileHash -LiteralPath (Resolve-KitFile $script:Kit.paths.guest_script) -Algorithm SHA256).Hash) 'Executing harness differs from the kit harness.'
    $script:Expected = @(Read-Json (Resolve-KitFile $script:Kit.paths.fixture_snapshot))
    Assert-True ($script:Expected.Count -ge 3) 'Independent fixture snapshot is incomplete.'
    Assert-Snapshot (Resolve-KitFile 'fixture') $script:Expected -Exact
    $manifest = Read-Json (Resolve-KitFile $script:Kit.paths.manifest)
    Assert-True ($manifest.app_id -ceq $script:Kit.app_id -and $manifest.app_version -ceq $script:Kit.app_version) 'Manifest identity differs from the kit fixture.'
    Assert-True ($manifest.start_menu_shortcut -eq $false -and $manifest.entry -ceq 'nimbus-demo.exe') 'Fixture enables unsupported system actions.'
    Assert-True (@($manifest.files).Count -eq $script:Expected.Count) 'Manifest file count differs from the independent fixture.'
    $manifestByPath = @{}
    foreach ($file in $manifest.files) {
        $path = ([string]$file.path).Replace('\', '/')
        Assert-True (-not $manifestByPath.ContainsKey($path)) 'Duplicate manifest path.'
        $manifestByPath[$path] = $file
    }
    foreach ($file in $script:Expected) {
        Assert-True ($manifestByPath.ContainsKey($file.path)) "Manifest omitted fixture: $($file.path)"
        Assert-True ($manifestByPath[$file.path].sha256 -ceq $file.sha256 -and $manifestByPath[$file.path].size -eq $file.size) 'Manifest content differs from independent fixture.'
    }
    return [pscustomobject]@{ status = 'VERIFIED'; files = $indexed.Count; checksums_sha256 = (Get-FileHash -LiteralPath $indexPath -Algorithm SHA256).Hash.ToLowerInvariant(); authenticity = 'NOT_ESTABLISHED_BY_UNSIGNED_CHECKSUMS' }
}

# Windows C argv quoting, used because .NET Framework has no ArgumentList.
# Arguments go to CreateProcess, never through cmd.exe or PowerShell source.
function Quote-Argument([AllowEmptyString()][string]$Value) {
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}

function Get-InstallArguments([string]$InstallDirectory, [string]$ReportPath, [int]$TimeoutSeconds, [bool]$UseGui) {
    $arguments = @('--operation', 'install', '--install-dir', $InstallDirectory, '--report', $ReportPath, '--observation-timeout-secs', [string]([Math]::Max(1, $TimeoutSeconds - 15)))
    if ($UseGui) { $arguments += '--gui' }
    return $arguments
}

function Invoke-Probe {
    param([string]$Executable, [string[]]$Arguments, [string]$Name, [int]$TimeoutSeconds, [string]$AwaitReport, [switch]$PreserveLocalAppData, [switch]$GuiInstall)
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $Executable
    $start.Arguments = (@($Arguments | ForEach-Object { Quote-Argument $_ }) -join ' ')
    $start.WorkingDirectory = $script:RunDirectory
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    # Rust writes UTF-8 even on a legacy Windows console code page. The optional
    # PowerShell inventory child keeps its inherited decoder; its authoritative
    # environment.json is independently written as UTF-8 by the collector.
    if (-not $PreserveLocalAppData) {
        $start.StandardOutputEncoding = $utf8
        $start.StandardErrorEncoding = $utf8
    }
    $start.EnvironmentVariables['TEMP'] = $script:TempDirectory
    $start.EnvironmentVariables['TMP'] = $script:TempDirectory
    if (-not $PreserveLocalAppData) { $start.EnvironmentVariables['LOCALAPPDATA'] = $script:LocalDirectory }
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $start
    $record = [ordered]@{ executable = $Executable; sha256 = (Get-FileHash -LiteralPath $Executable -Algorithm SHA256).Hash.ToLowerInvariant(); arguments = $Arguments; started_at_utc = [DateTime]::UtcNow.ToString('o'); status = 'STARTING'; timeout_seconds = $TimeoutSeconds; gui_install = [bool]$GuiInstall; logs_complete_at_eof = $false; stdout_path = (Join-Path $script:RunDirectory "$Name.stdout.log"); stderr_path = (Join-Path $script:RunDirectory "$Name.stderr.log"); report_observed = $null; process_was_killed = $false; sample_target_interval_ms = 100; memory_scope = 'Launched process only; no descendant aggregation'; samples = @(); peak_sampled_working_set_bytes = 0; interactive_first_frame_ms = $null; preserves_original_local_app_data_for_inventory = [bool]$PreserveLocalAppData }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    try {
        $null = $process.Start()
        $record['pid'] = $process.Id
        $record['process_start_time_utc'] = $process.StartTime.ToUniversalTime().ToString('o')
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if ($GuiInstall) {
            $started = [ordered]@{ status = 'GUI_INSTALL_STARTED_MANUAL_INTERACTION_REQUIRED'; pid = $process.Id; process_start_time_utc = $record.process_start_time_utc; setup = $Executable; sha256 = $record.sha256; arguments = $Arguments; install_dir = $script:InstallDirectory; report = $AwaitReport; timeout_seconds = $TimeoutSeconds; instruction = 'Keep the fixture directory unchanged. Confirm options, click Start, then close the Completed window. No GUI interaction is automated or certified by this harness.' }
            Write-Json "$Name.started.json" $started
            Write-Host ('GUI install started: ' + ($started | ConvertTo-Json -Depth 5 -Compress))
        }
        while ($true) {
            $exited = $process.WaitForExit(100)
            if (-not $exited) {
                try {
                    $process.Refresh()
                    $record.samples += [pscustomobject]@{ elapsed_ms = $clock.ElapsedMilliseconds; working_set_bytes = $process.WorkingSet64; private_bytes = $process.PrivateMemorySize64 }
                    $record.peak_sampled_working_set_bytes = [Math]::Max([long]$record.peak_sampled_working_set_bytes, $process.WorkingSet64)
                }
                catch { $record['last_sample_error'] = $_.Exception.Message }
            }
            $reported = -not $AwaitReport
            if ($AwaitReport -and (Test-Path -LiteralPath $AwaitReport -PathType Leaf)) {
                try { $candidate = Read-Json $AwaitReport; $reported = $candidate.result -in @('Succeeded', 'Cancelled', 'Failed', 'Unknown') } catch { $reported = $false }
            }
            if ($exited -and $stdout.IsCompleted -and $stderr.IsCompleted -and ($reported -or $process.ExitCode -ne 0)) { break }
            if ($clock.Elapsed.TotalSeconds -ge $TimeoutSeconds) {
                $record.status = 'TIMED_OUT_PROCESS_NOT_KILLED'
                $record['launcher_exited'] = $exited
                $record.report_observed = if ($AwaitReport) { $reported } else { $null }
                $record['note'] = 'No process was killed. No later test or install is started. Incomplete redirected output may remain in the process pipes.'
                if ($stdout.IsCompleted) { [IO.File]::WriteAllText((Join-Path $script:RunDirectory "$Name.stdout.log"), $stdout.GetAwaiter().GetResult(), $utf8) }
                if ($stderr.IsCompleted) { [IO.File]::WriteAllText((Join-Path $script:RunDirectory "$Name.stderr.log"), $stderr.GetAwaiter().GetResult(), $utf8) }
                $record.logs_complete_at_eof = $stdout.IsCompleted -and $stderr.IsCompleted
                throw "$Name exceeded $TimeoutSeconds seconds; process was not killed."
            }
            if ($exited) { Start-Sleep -Milliseconds 100 }
        }
        [IO.File]::WriteAllText((Join-Path $script:RunDirectory "$Name.stdout.log"), $stdout.GetAwaiter().GetResult(), $utf8)
        [IO.File]::WriteAllText((Join-Path $script:RunDirectory "$Name.stderr.log"), $stderr.GetAwaiter().GetResult(), $utf8)
        $record.logs_complete_at_eof = $true
        $record.report_observed = if ($AwaitReport) { $reported } else { $null }
        $record.status = 'EXITED'
        $record['exit_code'] = $process.ExitCode
        return [pscustomobject]$record
    }
    catch {
        if ($record.status -eq 'STARTING') { $record.status = 'FAILED_TO_COMPLETE' }
        $record['error'] = $_.Exception.Message
        throw
    }
    finally {
        $record['wall_ms'] = $clock.ElapsedMilliseconds
        $intervals = @()
        for ($index = 1; $index -lt $record.samples.Count; $index++) { $intervals += $record.samples[$index].elapsed_ms - $record.samples[$index - 1].elapsed_ms }
        $record['max_observed_sample_interval_ms'] = if ($intervals.Count -gt 0) { ($intervals | Measure-Object -Maximum).Maximum } else { $null }
        Write-Json "$Name.process.json" $record
        $process.Dispose()
    }
}

function Get-Registration {
    $states = @()
    foreach ($view in @([Microsoft.Win32.RegistryView]::Registry32, [Microsoft.Win32.RegistryView]::Registry64)) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $view)
        $key = $null
        try {
            $key = $base.OpenSubKey($script:RegistrySubkey, $false)
            $values = @{}
            if ($key) { foreach ($name in $key.GetValueNames()) { $values[$name] = $key.GetValue($name) } }
            $states += [pscustomobject]@{ view = [string]$view; exists = ($null -ne $key); values = $values }
        }
        finally { if ($key) { $key.Dispose() }; $base.Dispose() }
    }
    return $states
}

function Assert-Operation([string]$ReportPath, [object]$Process) {
    Assert-True ($Process.exit_code -eq 0) 'Operation launcher failed.'
    $report = Read-Json $ReportPath
    Assert-True ($report.result -ceq 'Succeeded' -and $report.exit_code -eq 0 -and $report.coordinator_exit_code -eq 0 -and $report.product_lock_retained -eq $false) 'Operation did not confirm success and backend exit.'
    Assert-True ((Get-SafePath $report.install_dir '') -eq $script:InstallDirectory) 'Report identifies an unexpected installation directory.'
    $events = @($report.events)
    Assert-True ($events.Count -gt 0) 'No backend events were recorded.'
    $terminal = $events[-1]
    Assert-True ($terminal.kind -ceq 'finish' -and $terminal.result -ceq 'Succeeded' -and $terminal.code -eq 0) 'Final backend event conflicts with success.'
    return $report
}

$plan = [ordered]@{
    schema_version = 1; status = 'PLAN_ONLY_NOT_EXECUTED'
    gui_install = [bool]$GuiInstall; install_mode = $(if ($GuiInstall) { 'GUI_MANUAL_START_AND_CLOSE' } else { 'SILENT' }); uninstall_mode = 'VALIDATED_RESIDENT_CLI'; operation_timeout_seconds = $OperationTimeoutSeconds; manual_acceptance = 'PENDING'
    scope = 'Prebuilt unsigned M0 fixture and native window probe; manual graphics and clean-image acceptance remain separate.'
    steps = @('Verify every kit file and independent fixture SHA-256.', 'Record the supplied image ID and optional read-only environment probe.', 'Run help/version for CLI, runtime, and UI probe.', 'Run five 3-second native UI probes; record window/render markers and raw adapter output.', 'Install one unique current-user fixture from its standalone Setup and verify files, committed receipt, runtime, and HKCU registration.', 'Run the installed fixture, add unknown sentinels, invoke the validated resident uninstall command, and verify only sentinels remain.')
    on_timeout = 'Stop remaining steps, keep processes and evidence; never force termination.'
    manual_checks = @('Trusted clean-image identity and no extra runtimes/development tools.', 'Ordinary account, UAC and default protection state.', 'Interactive first frame, pixel correctness, clicking, text input, focus, accessibility.', 'Software adapter/RDP/DPI matrix and explicit graphics-failure entry.', 'Download provenance, SmartScreen, Defender and signing acceptance.')
}
if ($GuiInstall) { $plan['gui_instruction'] = 'Keep the generated fixture directory unchanged. Confirm options, click Start, wait for Completed and close the window. GUI screenshots and interaction observations are separate manual evidence.' }
if ($PlanOnly -and -not $KitRoot) { $plan | ConvertTo-Json -Depth 10; return }
Assert-True (-not [string]::IsNullOrWhiteSpace($KitRoot)) 'Supply -KitRoot.'
$script:KitDirectory = Get-SafePath $KitRoot ''
$integrity = Test-Kit
$plan['integrity'] = $integrity
if ($PlanOnly) { $plan | ConvertTo-Json -Depth 10; return }
Assert-True ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT -and [Environment]::Is64BitOperatingSystem -and [Environment]::Is64BitProcess) 'Run 64-bit Windows PowerShell 5.1 or PowerShell 7 on Windows x64.'
Assert-True (-not [string]::IsNullOrWhiteSpace($ResultsRoot)) 'Supply a persistent -ResultsRoot outside the kit.'
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
Assert-True (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) 'Use a non-elevated account. No elevation was requested.'
$resultParent = Get-SafePath $ResultsRoot ''
Assert-True (-not ($resultParent -eq $script:KitDirectory -or $resultParent.StartsWith($script:KitDirectory + '\', [StringComparison]::OrdinalIgnoreCase))) 'ResultsRoot must be outside the immutable kit.'
$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
$script:RunDirectory = Get-SafePath (Join-Path $resultParent $runId) $resultParent
Assert-True (-not (Test-Path -LiteralPath $script:RunDirectory)) 'Refusing to reuse a result directory.'
$null = New-Item -ItemType Directory -Path $script:RunDirectory -Force
$script:TempDirectory = Get-SafePath (Join-Path $script:RunDirectory 'temp') $script:RunDirectory
$script:LocalDirectory = Get-SafePath (Join-Path $script:RunDirectory 'local-app-data') $script:RunDirectory
$script:InstallDirectory = Get-SafePath (Join-Path $script:LocalDirectory 'fixture-install') $script:LocalDirectory
foreach ($directory in @($script:TempDirectory, $script:LocalDirectory, $script:InstallDirectory)) { $null = New-Item -ItemType Directory -Path $directory }
$script:RegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:Kit.app_id
$summary = [ordered]@{ schema_version = 1; status = 'RUNNING'; run_directory = $script:RunDirectory; started_at_utc = [DateTime]::UtcNow.ToString('o'); image_id = $ImageId; declared_environment_kind = $EnvironmentKind; environment_declaration_is_evidence = $false; kit_id = $script:Kit.kit_id; app_id = $script:Kit.app_id; integrity = $integrity; powershell_version = $PSVersionTable.PSVersion.ToString(); effective_administrator = $false; environment_acceptance = 'NOT_ASSESSED'; cases = @(); active_case = 'environment'; manual_checks = $plan.manual_checks; failure = $null }
$summary['gui_install'] = [bool]$GuiInstall
$summary['install_mode'] = $plan.install_mode
$summary['uninstall_mode'] = $plan.uninstall_mode
$summary['operation_timeout_seconds'] = $OperationTimeoutSeconds
$summary['manual_acceptance'] = 'PENDING'
$originalLocalAppData = Get-SafePath ([Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)) ''
$guardDirectory = Get-SafePath (Join-Path $originalLocalAppData 'AvelinM0CompatibilityGuards') $originalLocalAppData
$guardPath = Get-SafePath (Join-Path $guardDirectory ($script:Kit.app_id + '.guard.json')) $guardDirectory
$guardHandle = $null
$uninstallConfirmed = $false
$summary['run_guard_path'] = $guardPath
$summary['run_guard_scope'] = 'Stable test-only guard under the original user LocalApplicationData; independent of ResultsRoot and child environment overrides.'
Write-Json 'summary.json' $summary
Write-Host "Evidence directory: $script:RunDirectory"
try {
    # LOCALAPPDATA changes for each run, so the product's own lock alone cannot
    # detect an unresolved earlier run. Use a stable test-only location under
    # the real user profile, independent of both ResultsRoot and child overrides.
    $null = New-Item -ItemType Directory -Path $guardDirectory -Force
    try { $guardHandle = [IO.File]::Open($guardPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::Read) }
    catch { throw "An existing or inaccessible run guard blocks this application identity: $guardPath. Review its retained run; no automatic unlock or retry was attempted." }
    $guardBytes = $utf8.GetBytes(([ordered]@{ app_id = $script:Kit.app_id; run_directory = $script:RunDirectory; status = 'RUNNING'; created_at_utc = [DateTime]::UtcNow.ToString('o') } | ConvertTo-Json))
    $guardHandle.Write($guardBytes, 0, $guardBytes.Length)
    $guardHandle.Flush($true)
    Assert-True (@(Get-Registration | Where-Object { $_.exists }).Count -eq 0) 'This kit application ID is already registered. Use a fresh image or a new kit; no existing install was changed.'
    if ($script:Kit.paths.environment_probe) {
        $powershellExe = Join-Path $PSHOME 'powershell.exe'
        if (-not (Test-Path -LiteralPath $powershellExe)) { $powershellExe = Join-Path $PSHOME 'pwsh.exe' }
        $environment = Invoke-Probe $powershellExe @('-NoProfile', '-NonInteractive', '-File', (Resolve-KitFile $script:Kit.paths.environment_probe), '-Output', (Join-Path $script:RunDirectory 'environment.json'), '-ImageId', $ImageId) 'environment' 120 '' -PreserveLocalAppData
        Assert-True ($environment.exit_code -eq 0) 'Environment collection failed; see the retained logs.'
        $summary['environment_record'] = 'environment.json'
    }
    else { $summary['environment_record'] = 'NOT_INCLUDED' }
    $summary.active_case = 'help-version'
    foreach ($tool in @('cli', 'runtime', 'ui_probe')) {
        foreach ($argument in @('--help', '--version')) {
            $name = $tool + '-' + $argument.TrimStart('-')
            $result = Invoke-Probe (Resolve-KitFile $script:Kit.paths.$tool) @($argument) $name $ProbeTimeoutSeconds ''
            Assert-True ($result.exit_code -eq 0) "$name failed."
            $summary.cases += [pscustomobject]@{ id = $name; status = 'PASSED'; process_record = "$name.process.json" }
        }
    }
    $summary.active_case = 'ui-smoke'
    for ($iteration = 1; $iteration -le 5; $iteration++) {
        $name = 'ui-smoke-' + $iteration
        $result = Invoke-Probe (Resolve-KitFile $script:Kit.paths.ui_probe) @('--smoke-seconds', '3') $name $ProbeTimeoutSeconds ''
        $diagnostics = [IO.File]::ReadAllText((Join-Path $script:RunDirectory "$name.stderr.log"), [Text.Encoding]::UTF8)
        $windowCreated = $diagnostics.Contains('AVELIN_UI_WINDOW_CREATED')
        $renderCalled = $diagnostics.Contains('AVELIN_UI_VIEW_RENDER_CALLED')
        $selfClosed = $diagnostics.Contains('AVELIN_UI_SMOKE_TIMEOUT elapsed_ms=3000')
        $clockObservations = [ordered]@{ origin = 'UI run_inner entry, excludes OS process loading'; window_elapsed_us = $null; render_elapsed_us = $null; frame_callback_elapsed_us = $null; window_scale_factor = $null; render_scale_factor = $null }
        foreach ($marker in @('WINDOW', 'RENDER', 'FRAME_CALLBACK')) {
            $match = [regex]::Match($diagnostics, ('(?m)^AVELIN_UI_PROBE_' + $marker + ' elapsed_us=(\d+)(?: scale_factor=([0-9.]+))?'))
            if ($match.Success) {
                $clockObservations[$marker.ToLowerInvariant() + '_elapsed_us'] = [long]$match.Groups[1].Value
                if ($marker -ne 'FRAME_CALLBACK' -and $match.Groups[2].Success) {
                    $clockObservations[$marker.ToLowerInvariant() + '_scale_factor'] = [double]::Parse($match.Groups[2].Value, [Globalization.CultureInfo]::InvariantCulture)
                }
            }
        }
        $logSummary = [regex]::Match($diagnostics, '(?m)^AVELIN_UI_PROBE_LOG_SUMMARY errors=(\d+) warnings=(\d+)')
        $loggedErrors = $null
        $loggedWarnings = $null
        if ($logSummary.Success) { $loggedErrors = [long]$logSummary.Groups[1].Value; $loggedWarnings = [long]$logSummary.Groups[2].Value }
        $passed = $result.exit_code -eq 0 -and $windowCreated -and $renderCalled -and $selfClosed -and $logSummary.Success -and $loggedErrors -eq 0
        $summary.cases += [pscustomobject]@{ id = $name; status = $(if ($passed) { 'WINDOW_PROBE_PASSED' } else { 'WINDOW_PROBE_FAILED' }); iteration = $iteration; first_observation_in_this_run = ($iteration -eq 1); cold_cache_verified = $false; window_created = $windowCreated; view_render_called = $renderCalled; self_closed_after_three_seconds = $selfClosed; interactive_first_frame = 'NOT_MEASURED'; ui_clock_observations = $clockObservations; logged_errors = $loggedErrors; logged_warnings = $loggedWarnings; logger_summary_available = $logSummary.Success; renderer_lines = @($diagnostics -split '\r?\n' | Where-Object { $_ -like 'AVELIN_UI_RENDERER*' }); process_record = "$name.process.json" }
        # An explicit failed GUI probe is retained; fixture installation is an
        # independent test. An observation timeout already stopped the run.
    }
    $summary.active_case = 'install'
    $installReportPath = Join-Path $script:RunDirectory 'install.report.json'
    $installArguments = @(Get-InstallArguments $script:InstallDirectory $installReportPath $OperationTimeoutSeconds ([bool]$GuiInstall))
    $installProcess = Invoke-Probe (Resolve-KitFile $script:Kit.paths.setup) $installArguments 'install' $OperationTimeoutSeconds $installReportPath -GuiInstall:$GuiInstall
    $installReport = Assert-Operation $installReportPath $installProcess
    Assert-Snapshot $script:InstallDirectory $script:Expected
    $receiptPath = Get-SafePath (Join-Path $script:InstallDirectory '.avelin-receipt.json') $script:InstallDirectory
    $receipt = Read-Json $receiptPath
    Assert-True ($receipt.schema_version -eq 1 -and $receipt.app_id -ceq $script:Kit.app_id -and $receipt.app_version -ceq $script:Kit.app_version -and $receipt.task_id -ceq $installReport.task_id) 'Receipt identity differs from installed fixture/task.'
    Assert-True ((Get-SafePath $receipt.install_dir '') -eq $script:InstallDirectory) 'Receipt path differs from isolated install path.'
    foreach ($field in @('installation_id', 'commit_id', 'user_sid')) { Assert-True (-not [string]::IsNullOrWhiteSpace($receipt.$field)) "Receipt omitted $field." }
    Assert-True (@($receipt.files).Count -eq $script:Expected.Count) 'Receipt file count differs from independent fixture.'
    $receiptByPath = @{}
    foreach ($file in $receipt.files) { $relative = ([string]$file.path).Replace('\', '/'); Assert-True (-not $receiptByPath.ContainsKey($relative)) 'Duplicate receipt path.'; $receiptByPath[$relative] = $file }
    foreach ($file in $script:Expected) {
        Assert-True ($receiptByPath.ContainsKey($file.path)) 'Receipt omitted fixture path.'
        Assert-True ($receiptByPath[$file.path].sha256 -ceq $file.sha256 -and $receiptByPath[$file.path].size -eq $file.size) 'Receipt differs from independent fixture hashes.'
    }
    $resident = Get-SafePath (Join-Path $script:InstallDirectory '.avelin\avelin-app.exe') $script:InstallDirectory
    $residentManifest = Get-SafePath (Join-Path $script:InstallDirectory '.avelin\manifest.json') $script:InstallDirectory
    $uninstaller = Get-SafePath (Join-Path $script:InstallDirectory 'uninstall.exe') $script:InstallDirectory
    Assert-True ((Get-FileHash -LiteralPath $resident -Algorithm SHA256).Hash -eq (Get-FileHash -LiteralPath (Resolve-KitFile $script:Kit.paths.runtime) -Algorithm SHA256).Hash) 'Installed runtime differs from the kit runtime.'
    Assert-True ((Get-FileHash -LiteralPath $residentManifest -Algorithm SHA256).Hash -eq (Get-FileHash -LiteralPath (Resolve-KitFile $script:Kit.paths.manifest) -Algorithm SHA256).Hash) 'Installed manifest differs from the kit manifest.'
    Assert-True (Test-Path -LiteralPath $uninstaller -PathType Leaf) 'Installed uninstaller is absent.'
    $registration = @(Get-Registration)
    $registered = @($registration | Where-Object { $_.exists })
    Assert-True ($registered.Count -ge 1) 'HKCU uninstall registration is absent.'
    $expectedCommand = '"{0}" --operation uninstall --backend "{1}" --manifest "{2}" --install-dir "{3}"' -f $resident, $uninstaller, $residentManifest, $script:InstallDirectory
    foreach ($state in $registered) {
        Assert-True ($state.values['DisplayVersion'] -ceq $script:Kit.app_version -and (Get-SafePath $state.values['InstallLocation'] '') -eq $script:InstallDirectory) 'Registration path/version differs from fixture.'
        Assert-True ($state.values['UninstallString'] -ceq $expectedCommand -and $state.values['QuietUninstallString'] -ceq $expectedCommand) 'Registered uninstall command differs from the known resident entry point.'
    }
    Write-Json 'installed.evidence.json' ([ordered]@{ receipt = $receipt; registration = $registration; files = @(Get-Snapshot $script:InstallDirectory) })
    $diagnosticProcess = Invoke-Probe (Resolve-KitFile $script:Kit.paths.cli) @('diagnose', '--session', $installReport.session_dir) 'installed-diagnosis' $ProbeTimeoutSeconds ''
    Assert-True ($diagnosticProcess.exit_code -eq 0) 'Read-only installed-session diagnosis failed.'
    $diagnosis = Read-Json (Join-Path $script:RunDirectory 'installed-diagnosis.stdout.log')
    Assert-True ($diagnosis.assessment -ceq 'RecordedStateMatchesObservedResources') 'Read-only diagnosis found a receipt, manifest, process, registration, or terminal conflict.'
    $fixture = Invoke-Probe (Join-Path $script:InstallDirectory 'nimbus-demo.exe') @() 'installed-fixture' $ProbeTimeoutSeconds ''
    Assert-True ($fixture.exit_code -eq 0) 'Installed fixture did not run.'
    $summary.cases += [pscustomobject]@{ id = 'install'; status = 'PASSED'; mode = $plan.install_mode; gui_interaction_acceptance = 'NOT_ASSESSED'; process_record = 'install.process.json'; report = 'install.report.json'; evidence = 'installed.evidence.json' }
    $summary.active_case = 'uninstall'
    $sentinels = @()
    foreach ($relative in @('user-kept.txt', 'assets/user-kept.txt')) {
        $path = Get-SafePath (Join-Path $script:InstallDirectory $relative) $script:InstallDirectory
        Assert-True (-not (Test-Path -LiteralPath $path)) 'Sentinel collides with installed fixture.'
        $null = New-Item -ItemType Directory -Path (Split-Path -Parent $path) -Force
        [IO.File]::WriteAllText($path, ('Unknown user fixture retained: ' + $runId + ' ' + $relative), $utf8)
        $sentinels += [pscustomobject]@{ path = $relative; size = (Get-Item -LiteralPath $path).Length; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
    }
    Write-Json 'sentinel-snapshot.json' $sentinels
    $uninstallReportPath = Join-Path $script:RunDirectory 'uninstall.report.json'
    # The registered command was compared byte-for-byte above. Pass the same
    # known argv directly, plus an evidence report path; never evaluate text.
    $uninstallProcess = Invoke-Probe $resident @('--operation', 'uninstall', '--backend', $uninstaller, '--manifest', $residentManifest, '--install-dir', $script:InstallDirectory, '--report', $uninstallReportPath, '--observation-timeout-secs', [string]([Math]::Max(1, $OperationTimeoutSeconds - 15))) 'uninstall' $OperationTimeoutSeconds $uninstallReportPath
    $null = Assert-Operation $uninstallReportPath $uninstallProcess
    Assert-Snapshot $script:InstallDirectory $sentinels -Exact
    Assert-True (@(Get-Registration | Where-Object { $_.exists }).Count -eq 0) 'Test registration remains after uninstall.'
    $uninstallConfirmed = $true
    Write-Json 'uninstalled.evidence.json' ([ordered]@{ registration = @(Get-Registration); files = @(Get-Snapshot $script:InstallDirectory); sentinels = $sentinels })
    $summary.cases += [pscustomobject]@{ id = 'uninstall'; status = 'PASSED'; report = 'uninstall.report.json'; evidence = 'uninstalled.evidence.json' }
    $summary.status = if (@($summary.cases | Where-Object { $_.status -eq 'WINDOW_PROBE_FAILED' }).Count -eq 0) { 'AUTOMATED_CHECKS_PASSED_MANUAL_ACCEPTANCE_PENDING' } elseif ($GuiInstall) { 'GRAPHICS_PROBE_FAILED_GUI_INSTALL_CHAIN_PASSED_MANUAL_ACCEPTANCE_PENDING' } else { 'GRAPHICS_PROBE_FAILED_SILENT_CHAIN_PASSED' }
    $summary.active_case = $null
}
catch { $summary.status = 'FAILED_OR_UNCONFIRMED'; $summary.failure = $_.Exception.Message; throw }
finally {
    $summary['finished_at_utc'] = [DateTime]::UtcNow.ToString('o')
    try { $summary['registration_after'] = @(Get-Registration) } catch { $summary['registration_read_error'] = $_.Exception.Message }
    if ($null -ne $guardHandle) {
        try {
            $guardBytes = $utf8.GetBytes(([ordered]@{ app_id = $script:Kit.app_id; run_directory = $script:RunDirectory; status = $summary.status; uninstall_confirmed = $uninstallConfirmed; updated_at_utc = [DateTime]::UtcNow.ToString('o') } | ConvertTo-Json))
            $guardHandle.Position = 0
            $guardHandle.SetLength(0)
            $guardHandle.Write($guardBytes, 0, $guardBytes.Length)
            $guardHandle.Flush($true)
        }
        finally { $guardHandle.Dispose() }
        if ($uninstallConfirmed) {
            # Only this known leaf guard is removed, after verified uninstall.
            # A failed/unknown run always retains its guard and forensic scene.
            $safeGuard = Get-SafePath $guardPath $guardDirectory
            Remove-Item -LiteralPath $safeGuard
        }
    }
    $summary['run_guard_retained'] = Test-Path -LiteralPath $guardPath
    Write-Json 'summary.json' $summary
    Write-Host "Retained result: $(Join-Path $script:RunDirectory 'summary.json')"
}
