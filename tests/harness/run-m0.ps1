[CmdletBinding()]
param(
    [switch]$PlanOnly,
    [switch]$UseDefaultReports,
    [switch]$NormalOnly,
    [string]$ProjectRoot = (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)),
    [string]$CliPath,
    [string]$AppPath,
    [string]$BridgePath,
    [ValidateRange(10, 600)][int]$TimeoutSeconds = 120
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# The source fixture is captured before build/install, independently of the
# backend's receipt. Each case must compare files against that snapshot.
$casePlan = @(
    [pscustomobject]@{
        id = 'M0-H01'; operation = 'install'
        assertions = @('Every fixture relative path and SHA-256 matches the installed file.', 'The committed receipt identifies this app, version, directory, managed files, and exact installed uninstaller SHA-256.', 'The task-specific HKCU key is correct and the disabled shortcut is absent.')
    },
    [pscustomobject]@{
        id = 'M0-H02'; operation = 'uninstall'
        assertions = @('All fixture-owned program files and test-only system resources are removed.', 'A user-created sentinel retains its original SHA-256.', 'The nonempty installation directory is preserved.', 'Uninstall evidence agrees with the absence of an installed receipt.')
    },
    [pscustomobject]@{
        id = 'M0-H03'; operation = 'cancel-after-files'
        assertions = @('Cancellation is requested through the public coordinator interface, never by killing the backend.', 'The result is Cancelled only after cleanup is reported and verified.', 'No new committed receipt or unapproved managed resources remain.', 'User sentinels in a sibling directory are unchanged; the fresh installation target starts empty.')
    },
    [pscustomobject]@{
        id = 'M0-H04'; operation = 'fault-after-files'
        assertions = @('A deterministic write-stage fault produces Failed rather than Succeeded.', 'The report records the failure and cleanup outcome.', 'Resource differences agree with the documented cleanup outcome.', 'User sentinels in a sibling directory are unchanged; the fresh installation target starts empty.')
    },
    [pscustomobject]@{
        id = 'M0-H05'; operation = 'installed-frontend-uninstall'
        assertions = @('A new packaged install registers the expected quoted resident-frontend command.', 'Execute the registered arguments through the installed frontend, and wait for the final child report rather than trusting the launcher exit.', 'The installed frontend, manifest, receipt, uninstaller, managed files, and test registration are removed.', 'Unknown root and nested user files retain their hashes.')
    }
)

$executionScope = [ordered]@{
    mode = $(if ($NormalOnly) { 'NORMAL_ONLY' } else { 'FULL_FIVE_CASES' })
    normal_only = [bool]$NormalOnly
    cancellation_or_fault_injection_enabled = -not [bool]$NormalOnly
    deferred_cases = @(if ($NormalOnly) { 'M0-H03'; 'M0-H04' })
    installed_entry_coverage = $(if ($NormalOnly) { 'M0-H01 installed Nimbus, no arguments, exact two-line stdout and empty stderr; included in H01, not a separate case.' } else { 'NOT_RUN: the legacy five-case mode does not execute the installed application entry.' })
}
if ($NormalOnly) {
    $casePlan = @($casePlan | Where-Object { $_.id -in @('M0-H01', 'M0-H02', 'M0-H05') })
    $casePlan[0].assertions += 'The installed Nimbus entry exits successfully with its exact two-line UTF-8 stdout, empty stderr, and unchanged installed files.'
}

$reportCoverage = [ordered]@{
    mode = $(if ($UseDefaultReports) { 'DEFAULT_REPORTS_FROM_CURRENT_PROCESS_STDOUT' } else { 'EXPLICIT_REPORT_PATHS' })
    default_report_operations = @(if ($UseDefaultReports) { 'M0-H01'; 'M0-H02'; if (-not $NormalOnly) { 'M0-H03'; 'M0-H04' }; 'M0-H05-install' })
    association = 'Default reports are resolved only from the current process stdout task_id/session_dir, constrained to this run child TEMP, and cross-checked against the persisted JSON; no latest-directory search.'
    resident_uninstall_boundary = 'M0-H05 resident uninstall keeps an explicit --report to observe its detached coordinator. Its default report discovery is not covered.'
}

if ($PlanOnly) {
    [ordered]@{
        schema_version = 1
        status = 'PLAN_ONLY_NOT_EXECUTED'
        scope = 'Local development-machine M0 smoke tests; not clean-machine, VM, graphics, signature, or release acceptance.'
        execution_scope = $executionScope
        planned_case_count = $casePlan.Count
        report_coverage = $reportCoverage
        isolation = @(
            'Create one unique run directory inside target/m0-harness; never reuse an installation directory.',
            'Each test case has a separate installation path and report.',
            'Preserve logs and artifacts; perform no recursive directory cleanup by default.',
            'Before any optional recursive removal, verify the resolved absolute path is inside the unique run directory and reject reparse points.',
            'Registry keys and shortcuts must be uniquely named for this run before any installer is executed.'
        )
        cases = $casePlan
    } | ConvertTo-Json -Depth 6
    exit 0
}

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Get-FullPath {
    param([string]$Path)
    return [IO.Path]::GetFullPath($Path).TrimEnd([char]'\', [char]'/')
}

function Assert-ContainedPath {
    param([string]$Path, [string]$Root, [switch]$AllowRoot)
    $absolutePath = Get-FullPath $Path
    $absoluteRoot = Get-FullPath $Root
    $isRoot = $absolutePath.Equals($absoluteRoot, [StringComparison]::OrdinalIgnoreCase)
    $isChild = $absolutePath.StartsWith($absoluteRoot + '\', [StringComparison]::OrdinalIgnoreCase)
    Assert-True ($isChild -or ($AllowRoot -and $isRoot)) "Path is outside the permitted task directory: $absolutePath"
    $pathCursor = $absolutePath
    while ($pathCursor) {
        if (Test-Path -LiteralPath $pathCursor) {
            $pathItem = Get-Item -LiteralPath $pathCursor -Force
            Assert-True (($pathItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse point rejected: $pathCursor"
        }
        $parentDirectory = [IO.Directory]::GetParent($pathCursor)
        if (-not $parentDirectory) { break }
        $pathCursor = $parentDirectory.FullName
    }
    return $absolutePath
}

function Write-TaskJson {
    param([string]$Path, [object]$Value)
    $safePath = Assert-ContainedPath $Path $script:RunRoot
    [IO.File]::WriteAllText($safePath, ($Value | ConvertTo-Json -Depth 30), [Text.UTF8Encoding]::new($false))
}

function Read-StrictUtf8Json {
    param([string]$Path)
    return ([Text.UTF8Encoding]::new($false, $true).GetString([IO.File]::ReadAllBytes($Path)) | ConvertFrom-Json -ErrorAction Stop)
}

function Get-TaskReportArguments {
    param([string]$ExplicitPath)
    if (-not $UseDefaultReports) { '--report'; $ExplicitPath }
}

function Get-FileSnapshot {
    param([string]$Directory)
    $snapshotRoot = Get-FullPath $Directory
    if (-not (Test-Path -LiteralPath $snapshotRoot -PathType Container)) { return @() }
    $pendingDirectories = [Collections.Generic.Stack[string]]::new()
    $pendingDirectories.Push($snapshotRoot)
    $snapshot = @()
    while ($pendingDirectories.Count -gt 0) {
        $currentDirectory = $pendingDirectories.Pop()
        foreach ($item in @(Get-ChildItem -LiteralPath $currentDirectory -Force)) {
            $safeItemPath = Assert-ContainedPath $item.FullName $snapshotRoot
            if ($item.PSIsContainer) { $pendingDirectories.Push($safeItemPath); continue }
            $snapshot += [pscustomobject]@{
                path = $safeItemPath.Substring($snapshotRoot.Length + 1)
                size = $item.Length
                sha256 = (Get-FileHash -LiteralPath $safeItemPath -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        }
    }
    return @($snapshot | Sort-Object path)
}

function Invoke-TaskProcess {
    param([string]$Executable, [string[]]$Arguments, [string]$Name, [string]$AwaitReport, [string]$RawArguments)
    Assert-True (Test-Path -LiteralPath $Executable -PathType Leaf) "Executable is missing: $Executable"
    $executedSha256 = (Get-FileHash -LiteralPath $Executable -Algorithm SHA256).Hash.ToLowerInvariant()
    $processStart = [Diagnostics.ProcessStartInfo]::new()
    $processStart.FileName = $Executable
    $processStart.WorkingDirectory = $script:ProjectDirectory
    $processStart.UseShellExecute = $false
    $processStart.CreateNoWindow = $true
    $processStart.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
    $processStart.RedirectStandardOutput = $true
    $processStart.RedirectStandardError = $true
    $processStart.StandardOutputEncoding = [Text.UTF8Encoding]::new($false, $true)
    $processStart.StandardErrorEncoding = [Text.UTF8Encoding]::new($false, $true)
    # Auxiliary NSIS/app extraction stays inside this run, not shared user temp.
    $processStart.Environment['TEMP'] = $script:TemporaryDirectory
    $processStart.Environment['TMP'] = $script:TemporaryDirectory
    $processStart.Environment['LOCALAPPDATA'] = $script:LocalAppDataDirectory
    if ($RawArguments) {
        Assert-True (@($Arguments).Count -eq 0) 'Specify structured arguments or validated ARP arguments, not both.'
        $processStart.Arguments = $RawArguments
    }
    else {
        foreach ($argument in $Arguments) { $processStart.ArgumentList.Add($argument) }
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $processStart
    $startedAt = [DateTime]::UtcNow
    Write-Host ("[{0}] starting {1}" -f $Name, [IO.Path]::GetFileName($Executable))
    $null = $process.Start()
    $processId = $process.Id
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $nextProgress = 15
    while ($true) {
        $launcherExited = $process.WaitForExit(250)
        $reportObserved = -not $AwaitReport
        if ($AwaitReport -and (Test-Path -LiteralPath $AwaitReport -PathType Leaf)) {
            try {
                $candidateReport = Read-StrictUtf8Json $AwaitReport
                $reportObserved = $candidateReport.result -in @('Succeeded', 'Cancelled', 'Failed', 'Unknown')
            }
            catch { $reportObserved = $false }
        }
        $outputClosed = $stdoutTask.IsCompleted -and $stderrTask.IsCompleted
        if ($launcherExited -and $outputClosed -and $reportObserved) { break }
        if ($launcherExited -and $outputClosed -and $process.ExitCode -ne 0) { break }
        if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) {
            $timeoutRecord = [ordered]@{
                executable = $Executable; executable_sha256 = $executedSha256; arguments = $Arguments; raw_arguments = $RawArguments; process_id = $processId
                started_at_utc = $startedAt.ToString('o'); status = 'TIMED_OUT_PROCESS_NOT_KILLED'
                awaited_report = $AwaitReport; launcher_exited = $launcherExited; report_observed = $reportObserved
                note = 'No backend was killed. Remaining cases were not started; inspect this process and retained artifacts before retrying.'
            }
            Write-TaskJson (Join-Path $script:RunRoot "$Name.process.json") $timeoutRecord
            $process.Dispose()
            throw "$Name exceeded $TimeoutSeconds seconds. Process $processId was not killed; the test run is stopped."
        }
        if ($timer.Elapsed.TotalSeconds -ge $nextProgress) {
            Write-Host ("[{0}] waiting for process/output/report completion ({1}s)" -f $Name, [int]$timer.Elapsed.TotalSeconds)
            $nextProgress += 15
        }
        if ($launcherExited) { Start-Sleep -Milliseconds 100 }
    }
    $stdout = $stdoutTask.GetAwaiter().GetResult()
    $stderr = $stderrTask.GetAwaiter().GetResult()
    $processResult = [pscustomobject]@{
        executable = $Executable; executable_sha256 = $executedSha256; arguments = $Arguments; raw_arguments = $RawArguments; process_id = $processId
        started_at_utc = $startedAt.ToString('o'); duration_ms = $timer.ElapsedMilliseconds
        exit_code = $process.ExitCode; status = 'EXITED'
        awaited_report = $AwaitReport; report_observed = $reportObserved
        temporary_directory = $script:TemporaryDirectory
        stdout_path = Join-Path $script:RunRoot "$Name.stdout.log"
        stderr_path = Join-Path $script:RunRoot "$Name.stderr.log"
        process_record_path = Join-Path $script:RunRoot "$Name.process.json"
        report_path = $null; report_sha256 = $null; report_mode = $null
        task_id = $null; session_dir = $null; report_matches_stdout = $null
    }
    $process.Dispose()
    [IO.File]::WriteAllText((Join-Path $script:RunRoot "$Name.stdout.log"), $stdout, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText((Join-Path $script:RunRoot "$Name.stderr.log"), $stderr, [Text.UTF8Encoding]::new($false))
    Write-TaskJson (Join-Path $script:RunRoot "$Name.process.json") $processResult
    return $processResult
}

function Get-TestRegistryState {
    $states = @()
    foreach ($registryView in @([Microsoft.Win32.RegistryView]::Registry32, [Microsoft.Win32.RegistryView]::Registry64)) {
        $baseKey = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $registryView)
        $testKey = $null
        try {
            $testKey = $baseKey.OpenSubKey($script:TestRegistrySubkey, $false)
            $values = [ordered]@{}
            if ($testKey) {
                foreach ($valueName in $testKey.GetValueNames()) { $values[$valueName] = $testKey.GetValue($valueName) }
            }
            $states += [pscustomobject]@{ view = "$registryView"; exists = ($null -ne $testKey); values = $values }
        }
        finally {
            if ($testKey) { $testKey.Dispose() }
            $baseKey.Dispose()
        }
    }
    return $states
}

function Assert-NoTestSystemResources {
    foreach ($registryState in @(Get-TestRegistryState)) {
        Assert-True (-not $registryState.exists) "Test uninstall key remains in $($registryState.view): $script:TestRegistrySubkey"
    }
    foreach ($shortcutPath in $script:TestShortcutPaths) {
        Assert-True (-not (Test-Path -LiteralPath $shortcutPath)) "A disabled test shortcut exists: $shortcutPath"
    }
}

function New-TestInstallation {
    param([string]$Name)
    $directory = Assert-ContainedPath (Join-Path $script:RunRoot $Name) $script:RunRoot
    Assert-True (-not (Test-Path -LiteralPath $directory)) "Refusing to reuse an installation path: $directory"
    $null = New-Item -ItemType Directory -Path $directory
    $script:ObservedInstallations += $directory
    return [pscustomobject]@{ directory = $directory; sentinels = @() }
}

function Add-TestUserFiles {
    param([string]$Directory)
    $safeDirectory = Assert-ContainedPath $Directory $script:RunRoot
    $null = New-Item -ItemType Directory -Path (Join-Path $safeDirectory 'assets') -Force
    $sentinelFiles = @()
    foreach ($relativeSentinel in @('user-kept.txt', 'assets\user-created.txt')) {
        $sentinelPath = Assert-ContainedPath (Join-Path $safeDirectory $relativeSentinel) $script:RunRoot
        Assert-True (-not (Test-Path -LiteralPath $sentinelPath)) "Refusing to replace a preexisting sentinel path: $sentinelPath"
        [IO.File]::WriteAllText($sentinelPath, "Unmanaged test file $relativeSentinel : $script:RunId", [Text.UTF8Encoding]::new($false))
        $sentinelFiles += [pscustomobject]@{
            path = $relativeSentinel
            size = (Get-Item -LiteralPath $sentinelPath).Length
            sha256 = (Get-FileHash -LiteralPath $sentinelPath -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    return $sentinelFiles
}

function Assert-ResourceSnapshot {
    param([string]$Directory, [object[]]$Expected, [switch]$Exact)
    if ($null -eq $Expected) { $Expected = @() }
    $actualFiles = @(Get-FileSnapshot $Directory)
    $actualByPath = @{}
    foreach ($file in $actualFiles) { $actualByPath[$file.path] = $file }
    foreach ($file in $Expected) {
        Assert-True ($actualByPath.ContainsKey($file.path)) "Expected file is missing: $($file.path) in $Directory"
        Assert-True ($actualByPath[$file.path].sha256 -eq $file.sha256) "File content changed unexpectedly: $($file.path) in $Directory"
        Assert-True ($actualByPath[$file.path].size -eq $file.size) "File size differs: $($file.path) in $Directory"
    }
    if ($Exact) { Assert-True ($actualFiles.Count -eq $Expected.Count) "Unexpected files remain in $Directory; inspect the retained snapshot." }
}

function Read-OperationReport {
    param([string]$Path, [string]$ExpectedResult, [object]$ProcessResult, [switch]$ResolveDefault)
    $stdoutReport = $null
    if ($ResolveDefault) {
        Assert-True (-not $ProcessResult.raw_arguments -and '--report' -notin @($ProcessResult.arguments)) 'Default report coverage unexpectedly passed --report.'
        $stdoutPath = Assert-ContainedPath $ProcessResult.stdout_path $script:RunRoot
        $stdout = [Text.UTF8Encoding]::new($false, $true).GetString([IO.File]::ReadAllBytes($stdoutPath))
        $taskReports = @()
        foreach ($line in ($stdout -split '\r?\n')) {
            if ([string]::IsNullOrWhiteSpace($line)) { continue }
            try { $candidate = $line | ConvertFrom-Json -ErrorAction Stop }
            catch { continue }
            if ($null -ne $candidate -and $candidate.PSObject.Properties['task_id'] -and
                $candidate.PSObject.Properties['session_dir'] -and $candidate.PSObject.Properties['result']) {
                $taskReports += $candidate
            }
        }
        Assert-True ($taskReports.Count -eq 1) "Expected one task report in this process stdout, found $($taskReports.Count): $stdoutPath"
        $stdoutReport = $taskReports[0]
        $parsedTaskId = [Guid]::Empty
        Assert-True ([Guid]::TryParse([string]$stdoutReport.task_id, [ref]$parsedTaskId)) 'Stdout task report has no valid task identity.'
        Assert-True ([IO.Path]::IsPathFullyQualified([string]$stdoutReport.session_dir)) 'Stdout task session path is not absolute.'
        $expectedSessionRoot = Assert-ContainedPath (Join-Path $ProcessResult.temporary_directory 'AvelinM0') $script:TemporaryDirectory
        $sessionDirectory = Assert-ContainedPath ([string]$stdoutReport.session_dir) $expectedSessionRoot
        Assert-True ((Split-Path -Parent $sessionDirectory) -eq $expectedSessionRoot -and
            (Split-Path -Leaf $sessionDirectory) -cmatch '^session-[0-9a-f]{32}$') 'Default task session does not match the coordinator session layout.'
        $Path = Assert-ContainedPath (Join-Path $sessionDirectory 'report.json') $sessionDirectory
    }
    else { $Path = Assert-ContainedPath $Path $script:RunRoot }
    Assert-True (Test-Path -LiteralPath $Path -PathType Leaf) "Operation did not write its report: $Path"
    $operationReport = Read-StrictUtf8Json $Path
    if ($ResolveDefault) {
        foreach ($field in @('task_id', 'result', 'exit_code', 'coordinator_exit_code', 'session_dir', 'install_dir', 'backend_pid')) {
            Assert-True ($null -ne $operationReport.PSObject.Properties[$field] -and $null -ne $stdoutReport.PSObject.Properties[$field]) "Task report is missing $field."
            Assert-True ($operationReport.$field -ceq $stdoutReport.$field) "Persisted report $field differs from this process stdout."
        }
        Assert-True (($operationReport | ConvertTo-Json -Depth 50 -Compress) -ceq ($stdoutReport | ConvertTo-Json -Depth 50 -Compress)) 'Persisted task report differs from the current process stdout JSON.'
        foreach ($event in @($operationReport.events)) {
            Assert-True ($event.task_id -ceq $operationReport.task_id) 'Report event belongs to a different task.'
        }
    }
    Assert-True ($operationReport.result -ceq $ExpectedResult) "Expected $ExpectedResult but received $($operationReport.result): $($operationReport.message)"
    Assert-True (@($operationReport.events).Count -gt 0) "Operation report has no execution events: $Path"
    if ($ExpectedResult -eq 'Succeeded') {
        Assert-True ($operationReport.exit_code -eq 0 -and $ProcessResult.exit_code -eq 0) "Success conflicts with a nonzero backend/coordinator exit code: $Path"
    }
    if ($ExpectedResult -eq 'Failed') {
        Assert-True ($operationReport.exit_code -ne 0 -and $ProcessResult.exit_code -ne 0) "Failure was exposed as process success: $Path"
    }
    $ProcessResult.report_path = $Path
    $ProcessResult.report_sha256 = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    $ProcessResult.report_mode = $(if ($ResolveDefault) { 'DEFAULT_RESOLVED_FROM_CURRENT_PROCESS_STDOUT' } else { 'EXPLICIT_PATH' })
    $ProcessResult.task_id = $operationReport.task_id
    $ProcessResult.session_dir = $operationReport.session_dir
    $ProcessResult.report_matches_stdout = $(if ($ResolveDefault) { $true } else { $null })
    Write-TaskJson $ProcessResult.process_record_path $ProcessResult
    return $operationReport
}

function Assert-Receipt {
    param([string]$Directory, [object[]]$ExpectedFiles)
    $receiptPath = Join-Path $Directory '.avelin-receipt.json'
    Assert-True (Test-Path -LiteralPath $receiptPath -PathType Leaf) 'A successful install has no committed receipt.'
    $receipt = Get-Content -LiteralPath $receiptPath -Raw | ConvertFrom-Json
    Assert-True ($receipt.schema_version -eq 1) 'Receipt schema does not match the M0 contract.'
    Assert-True ($receipt.app_id -ceq $script:TestAppId) 'Receipt application identity differs from the independently generated test identity.'
    Assert-True ($receipt.app_version -ceq $script:TestVersion) 'Receipt application version differs from the fixture config.'
    Assert-True ((Get-FullPath $receipt.install_dir) -eq (Get-FullPath $Directory)) 'Receipt installation directory does not match the isolated test directory.'
    foreach ($identityField in @('installation_id', 'task_id', 'commit_id')) {
        Assert-True (-not [string]::IsNullOrWhiteSpace($receipt.$identityField)) "Receipt is missing $identityField."
    }
    Assert-True ($null -ne $receipt.PSObject.Properties['uninstaller_sha256']) 'Receipt is missing the committed uninstaller hash.'
    Assert-True ([string]$receipt.uninstaller_sha256 -cmatch '^[0-9a-f]{64}$') 'Receipt uninstaller SHA-256 is invalid.'
    $uninstallerPath = Assert-ContainedPath (Join-Path $Directory 'uninstall.exe') $script:RunRoot
    Assert-True (Test-Path -LiteralPath $uninstallerPath -PathType Leaf) 'Installed NSIS uninstaller is missing.'
    Assert-True ((Get-FileHash -LiteralPath $uninstallerPath -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $receipt.uninstaller_sha256) 'Installed NSIS uninstaller differs from the committed receipt hash.'
    $receiptFiles = @($receipt.files)
    Assert-True ($receiptFiles.Count -eq $ExpectedFiles.Count) 'Receipt file count differs from the independently captured fixture.'
    $receiptByPath = @{}
    foreach ($file in $receiptFiles) {
        Assert-True (-not $receiptByPath.ContainsKey($file.path)) "Receipt repeats a file: $($file.path)"
        $receiptByPath[$file.path] = $file
    }
    foreach ($file in $ExpectedFiles) {
        Assert-True ($receiptByPath.ContainsKey($file.path)) "Fixture file omitted from receipt: $($file.path)"
        Assert-True ($receiptByPath[$file.path].sha256 -eq $file.sha256 -and $receiptByPath[$file.path].size -eq $file.size) "Receipt content metadata differs from the fixture: $($file.path)"
    }
    return $receipt
}

function Save-CaseEvidence {
    param([string]$Id, [string]$Directory, [object]$OperationReport, [object]$ProcessResult, [object]$EntryEvidence)
    $caseEvidence = [ordered]@{
        id = $Id; status = 'PASSED'; installation_directory = $Directory
        report_result = $OperationReport.result; process = $ProcessResult
        report_path = $ProcessResult.report_path; report_sha256 = $ProcessResult.report_sha256
        report_mode = $ProcessResult.report_mode
        files_after = @(Get-FileSnapshot $Directory)
        registry_after = @(Get-TestRegistryState)
        shortcut_test = 'Configuration disabled; expected shortcut absent.'
    }
    if ($null -ne $EntryEvidence) { $caseEvidence['installed_entry'] = $EntryEvidence }
    if ($script:ExternalUserDirectory) {
        $caseEvidence['protected_external_directory'] = $script:ExternalUserDirectory
        $caseEvidence['protected_external_files'] = @(Get-FileSnapshot $script:ExternalUserDirectory)
    }
    $script:Summary.cases += $caseEvidence
    Write-TaskJson (Join-Path $script:RunRoot "$Id.evidence.json") $caseEvidence
    Write-Host "[$Id] PASSED"
}

Assert-True ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'This M0 harness requires Windows.'
$script:ProjectDirectory = Get-FullPath $ProjectRoot
if (-not $CliPath) { $CliPath = Join-Path $script:ProjectDirectory 'target\debug\avelin-cli.exe' }
if (-not $AppPath) { $AppPath = Join-Path $script:ProjectDirectory 'target\debug\avelin-app.exe' }
if (-not $BridgePath) { $BridgePath = Join-Path $script:ProjectDirectory 'target\i686-pc-windows-msvc\debug\avelin_nsis_bridge.dll' }
$CliPath = Get-FullPath $CliPath
$AppPath = Get-FullPath $AppPath
$BridgePath = Get-FullPath $BridgePath
Assert-True (Test-Path -LiteralPath $CliPath -PathType Leaf) "Build the CLI before running this harness: $CliPath"
Assert-True (Test-Path -LiteralPath $AppPath -PathType Leaf) "Build the app before running this harness: $AppPath"
Assert-True (Test-Path -LiteralPath $BridgePath -PathType Leaf) "Build the x86 NSIS bridge before running this harness: $BridgePath"

$payloadSource = Assert-ContainedPath (Join-Path $script:ProjectDirectory 'examples\nimbus\payload') $script:ProjectDirectory
$sourceSnapshot = @(Get-FileSnapshot $payloadSource)
Assert-True ($sourceSnapshot.Count -ge 3) 'Build/copy nimbus-demo.exe and the two text fixtures before running the harness.'
Assert-True (@($sourceSnapshot | Where-Object { $_.path -ceq 'nimbus-demo.exe' }).Count -eq 1) 'The independent executable fixture nimbus-demo.exe is missing.'
foreach ($reservedSentinel in @('user-kept.txt', 'assets\user-created.txt')) {
    Assert-True (@($sourceSnapshot | Where-Object { $_.path -eq $reservedSentinel }).Count -eq 0) "Fixture collides with a user-data sentinel: $reservedSentinel"
}

$taskParent = Assert-ContainedPath (Join-Path $script:ProjectDirectory 'target\m0-harness') $script:ProjectDirectory
$script:RunId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
$script:RunRoot = Assert-ContainedPath (Join-Path $taskParent $script:RunId) $taskParent
Assert-True (-not (Test-Path -LiteralPath $script:RunRoot)) 'Generated task directory already exists; refusing to reuse it.'
$null = New-Item -ItemType Directory -Path $script:RunRoot -Force
$script:TemporaryDirectory = Assert-ContainedPath (Join-Path $script:RunRoot 'temp') $script:RunRoot
$null = New-Item -ItemType Directory -Path $script:TemporaryDirectory
$script:LocalAppDataDirectory = Assert-ContainedPath (Join-Path $script:RunRoot 'local-app-data') $script:RunRoot
$null = New-Item -ItemType Directory -Path $script:LocalAppDataDirectory
$fixtureDirectory = Assert-ContainedPath (Join-Path $script:RunRoot 'fixtures') $script:RunRoot
$null = New-Item -ItemType Directory -Path $fixtureDirectory
foreach ($file in $sourceSnapshot) {
    $copyTarget = Assert-ContainedPath (Join-Path $fixtureDirectory $file.path) $fixtureDirectory
    $null = New-Item -ItemType Directory -Path (Split-Path -Parent $copyTarget) -Force
    Copy-Item -LiteralPath (Join-Path $payloadSource $file.path) -Destination $copyTarget
}
Assert-ResourceSnapshot $fixtureDirectory $sourceSnapshot -Exact
$originalSourceSnapshot = $sourceSnapshot
$generatedFixturePaths = @()
$deepFixturePath = Assert-ContainedPath (Join-Path $fixtureDirectory 'assets\deep\more\readme.txt') $fixtureDirectory
$generatedDeepFixture = -not (Test-Path -LiteralPath $deepFixturePath)
if ($generatedDeepFixture) {
    $null = New-Item -ItemType Directory -Path (Split-Path -Parent $deepFixturePath) -Force
    [IO.File]::WriteAllText($deepFixturePath, 'Independent nested-directory cleanup fixture.', [Text.UTF8Encoding]::new($false))
    $generatedFixturePaths += 'assets\deep\more\readme.txt'
}
$literalFixturePath = Assert-ContainedPath (Join-Path $fixtureDirectory '中文\${X}.txt') $fixtureDirectory
if (-not (Test-Path -LiteralPath $literalFixturePath)) {
    $null = New-Item -ItemType Directory -Path (Split-Path -Parent $literalFixturePath) -Force
    [IO.File]::WriteAllText($literalFixturePath, 'Literal data filename containing ${X}; expected unchanged.', [Text.UTF8Encoding]::new($false))
    $generatedFixturePaths += '中文\${X}.txt'
}
$sourceSnapshot = @(Get-FileSnapshot $fixtureDirectory)
Write-TaskJson (Join-Path $script:RunRoot 'fixture-snapshot.json') $sourceSnapshot

$script:TestAppId = 'dev.avelin.m0.nimbus.' + [Guid]::NewGuid().ToString('N')
$script:TestVersion = '0.1.0'
$script:TestRegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:TestAppId
$script:TestShortcutPaths = @()
foreach ($specialFolder in @('Programs', 'StartMenu', 'DesktopDirectory')) {
    $folderPath = [Environment]::GetFolderPath($specialFolder)
    if ($folderPath) { $script:TestShortcutPaths += Join-Path $folderPath ("Avelin M0 {0}.lnk" -f $script:TestAppId) }
}
Assert-NoTestSystemResources

$configPath = Join-Path $script:RunRoot 'avelin.toml'
$testConfig = @"
schema_version = 1

[app]
id = "$script:TestAppId"
name = "Avelin M0 Harness"
version = "$script:TestVersion"
entry = "nimbus-demo.exe"

[payload]
directory = "fixtures"

[install]
directory_name = "AvelinM0Harness"
start_menu_shortcut = false
"@
[IO.File]::WriteAllText($configPath, $testConfig, [Text.UTF8Encoding]::new($false))
$setupPath = Join-Path $script:RunRoot 'NimbusSetup.exe'
$backendPath = Join-Path $script:RunRoot 'NimbusSetup.backend.exe'
$manifestPath = Join-Path $script:RunRoot 'NimbusSetup.manifest.json'
$script:Summary = [ordered]@{
    schema_version = 1; started_at_utc = [DateTime]::UtcNow.ToString('o'); status = 'RUNNING'
    scope = 'Local development-machine smoke tests only; no VM, clean-machine, GPUI, signature, upgrade, or release acceptance claim.'
    execution_scope = $executionScope
    planned_case_count = $casePlan.Count
    planned_case_ids = @($casePlan.id)
    report_coverage = $reportCoverage
    run_directory = $script:RunRoot; app_id = $script:TestAppId; registry_subkey = $script:TestRegistrySubkey
    source_fixture_directory = $payloadSource; source_fixture_files = $originalSourceSnapshot
    expected_fixture_files = $sourceSnapshot; generated_nested_fixture = $generatedDeepFixture
    generated_fixture_paths = $generatedFixturePaths
    harness_sha256 = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash.ToLowerInvariant()
    cli_sha256 = (Get-FileHash -LiteralPath $CliPath -Algorithm SHA256).Hash.ToLowerInvariant()
    app_sha256 = (Get-FileHash -LiteralPath $AppPath -Algorithm SHA256).Hash.ToLowerInvariant()
    bridge_path = $BridgePath
    bridge_sha256 = (Get-FileHash -LiteralPath $BridgePath -Algorithm SHA256).Hash.ToLowerInvariant()
    cases = @(); active_case = 'BUILD'; failure = $null
}
$script:ObservedInstallations = @()
$script:ExternalUserDirectory = $null
Write-TaskJson (Join-Path $script:RunRoot 'summary.json') $script:Summary
Write-Host "M0 artifacts: $script:RunRoot"

try {
    $buildProcess = Invoke-TaskProcess $CliPath @('build', '--config', $configPath, '--output', $setupPath, '--runtime', $AppPath, '--bridge', $BridgePath) 'build'
    Assert-True ($buildProcess.exit_code -eq 0) 'Builder failed; inspect build.stderr.log and build.stdout.log.'
    foreach ($artifactPath in @($setupPath, $backendPath, $manifestPath)) {
        Assert-True (Test-Path -LiteralPath $artifactPath -PathType Leaf) "Expected build artifact is missing: $artifactPath"
    }
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    Assert-True ($manifest.app_id -ceq $script:TestAppId) 'Builder emitted a different app ID; refusing to execute it.'
    Assert-True ($manifest.start_menu_shortcut -eq $false) 'Builder unexpectedly enabled a shortcut; refusing to execute it.'
    $script:Summary['artifacts'] = @(@($setupPath, $backendPath, $manifestPath) | ForEach-Object {
        [pscustomobject]@{ path = $_; sha256 = (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash.ToLowerInvariant() }
    })

    $script:Summary.active_case = 'M0-H01'
    $normalCase = New-TestInstallation 'install-中文 space'
    $normalReportPath = Join-Path $script:RunRoot 'M0-H01.report.json'
    $normalArguments = @('--install-dir', $normalCase.directory, '--operation', 'install') + @(Get-TaskReportArguments $normalReportPath)
    $normalProcess = Invoke-TaskProcess $setupPath $normalArguments 'M0-H01'
    $normalReport = Read-OperationReport $normalReportPath 'Succeeded' $normalProcess -ResolveDefault:$UseDefaultReports
    Assert-ResourceSnapshot $normalCase.directory $sourceSnapshot
    $null = Assert-Receipt $normalCase.directory $sourceSnapshot
    Assert-True (Test-Path -LiteralPath (Join-Path $normalCase.directory 'uninstall.exe') -PathType Leaf) 'Installed NSIS uninstaller is missing.'
    $installedRegistryStates = @(Get-TestRegistryState | Where-Object { $_.exists })
    Assert-True ($installedRegistryStates.Count -ge 1) 'Successful install did not register its test-only HKCU uninstall key.'
    foreach ($state in $installedRegistryStates) {
        Assert-True ($state.values['DisplayVersion'] -eq $script:TestVersion) 'Uninstall registration version is incorrect.'
        Assert-True ((Get-FullPath $state.values['InstallLocation']) -eq $normalCase.directory) 'Uninstall registration points outside the expected installation directory.'
    }
    foreach ($shortcutPath in $script:TestShortcutPaths) {
        Assert-True (-not (Test-Path -LiteralPath $shortcutPath)) "A shortcut was created despite disabled configuration: $shortcutPath"
    }
    $normalEntryEvidence = $null
    if ($NormalOnly) {
        $beforeEntryFiles = @(Get-FileSnapshot $normalCase.directory)
        $installedEntry = Assert-ContainedPath (Join-Path $normalCase.directory 'nimbus-demo.exe') $script:RunRoot
        $entryProcess = Invoke-TaskProcess $installedEntry @() 'M0-H01-entry'
        $entryStdout = [Text.UTF8Encoding]::new($false, $true).GetString([IO.File]::ReadAllBytes($entryProcess.stdout_path)).Replace("`r`n", "`n")
        $entryStderr = [Text.UTF8Encoding]::new($false, $true).GetString([IO.File]::ReadAllBytes($entryProcess.stderr_path))
        $expectedEntryStdout = "Nimbus Notes — Avelin M0 sample application`nThis executable is a local installation test payload.`n"
        Assert-True ($entryProcess.exit_code -eq 0 -and $entryStdout -ceq $expectedEntryStdout -and $entryStderr.Length -eq 0) 'Installed Nimbus entry differs from its exit/stdout/stderr contract.'
        Assert-ResourceSnapshot $normalCase.directory $beforeEntryFiles -Exact
        $normalEntryEvidence = [ordered]@{
            status = 'PASSED'; process = $entryProcess
            stdout_contract = 'Exact Nimbus two-line UTF-8 greeting; CRLF normalized to LF; stderr empty.'
            installed_inventory_unchanged = $true
        }
    }
    # M0 accepts a fresh empty target. Unknown user files are added only after
    # installation, before the uninstall preservation test.
    $normalCase.sentinels = @(Add-TestUserFiles $normalCase.directory)
    Save-CaseEvidence 'M0-H01' $normalCase.directory $normalReport $normalProcess $normalEntryEvidence

    $script:Summary.active_case = 'M0-H02'
    $uninstallReportPath = Join-Path $script:RunRoot 'M0-H02.report.json'
    $uninstallArguments = @('--backend', (Join-Path $normalCase.directory 'uninstall.exe'), '--manifest', $manifestPath, '--install-dir', $normalCase.directory, '--operation', 'uninstall') + @(Get-TaskReportArguments $uninstallReportPath)
    $uninstallProcess = Invoke-TaskProcess $AppPath $uninstallArguments 'M0-H02'
    $uninstallReport = Read-OperationReport $uninstallReportPath 'Succeeded' $uninstallProcess -ResolveDefault:$UseDefaultReports
    Assert-ResourceSnapshot $normalCase.directory $normalCase.sentinels -Exact
    Assert-NoTestSystemResources
    Save-CaseEvidence 'M0-H02' $normalCase.directory $uninstallReport $uninstallProcess

    if (-not $NormalOnly) {
        $script:ExternalUserDirectory = Assert-ContainedPath (Join-Path $script:RunRoot 'external-user-data') $script:RunRoot
        $null = New-Item -ItemType Directory -Path $script:ExternalUserDirectory
        $script:ObservedInstallations += $script:ExternalUserDirectory
        $externalUserSnapshot = @(Add-TestUserFiles $script:ExternalUserDirectory)
        Write-TaskJson (Join-Path $script:RunRoot 'external-user-snapshot.json') $externalUserSnapshot

        foreach ($injection in @(
            [pscustomobject]@{ id = 'M0-H03'; name = 'cancel-中文 space'; option = '--cancel-after-files'; expected = 'Cancelled' },
            [pscustomobject]@{ id = 'M0-H04'; name = 'fault-中文 space'; option = '--fault-after-files'; expected = 'Failed' }
        )) {
            $script:Summary.active_case = $injection.id
            Assert-NoTestSystemResources
            $injectionCase = New-TestInstallation $injection.name
            $injectionReportPath = Join-Path $script:RunRoot ($injection.id + '.report.json')
            $injectionArguments = @('--backend', $backendPath, '--manifest', $manifestPath, '--install-dir', $injectionCase.directory, '--operation', 'install', $injection.option, '1') + @(Get-TaskReportArguments $injectionReportPath)
            $injectionProcess = Invoke-TaskProcess $AppPath $injectionArguments $injection.id
            $injectionReport = Read-OperationReport $injectionReportPath $injection.expected $injectionProcess -ResolveDefault:$UseDefaultReports
            Assert-ResourceSnapshot $injectionCase.directory @() -Exact
            Assert-ResourceSnapshot $script:ExternalUserDirectory $externalUserSnapshot -Exact
            Assert-NoTestSystemResources
            Save-CaseEvidence $injection.id $injectionCase.directory $injectionReport $injectionProcess
        }
    }

    $script:Summary.active_case = 'M0-H05'
    Assert-NoTestSystemResources
    $residentCase = New-TestInstallation 'resident-uninstall-中文 space'
    $residentInstallReportPath = Join-Path $script:RunRoot 'M0-H05-install.report.json'
    $residentInstallArguments = @('--install-dir', $residentCase.directory, '--operation', 'install') + @(Get-TaskReportArguments $residentInstallReportPath)
    $residentInstallProcess = Invoke-TaskProcess $setupPath $residentInstallArguments 'M0-H05-install'
    $residentInstallReport = Read-OperationReport $residentInstallReportPath 'Succeeded' $residentInstallProcess -ResolveDefault:$UseDefaultReports
    Assert-ResourceSnapshot $residentCase.directory $sourceSnapshot
    $residentReceipt = Assert-Receipt $residentCase.directory $sourceSnapshot
    $residentCase.sentinels = @(Add-TestUserFiles $residentCase.directory)
    $residentFrontend = Assert-ContainedPath (Join-Path $residentCase.directory '.avelin\avelin-app.exe') $script:RunRoot
    $residentBackend = Assert-ContainedPath (Join-Path $residentCase.directory 'uninstall.exe') $script:RunRoot
    $residentManifest = Assert-ContainedPath (Join-Path $residentCase.directory '.avelin\manifest.json') $script:RunRoot
    foreach ($residentFile in @($residentFrontend, $residentBackend, $residentManifest)) {
        Assert-True (Test-Path -LiteralPath $residentFile -PathType Leaf) "Resident uninstall component is missing: $residentFile"
    }
    Assert-True ((Get-FileHash -LiteralPath $residentFrontend -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $script:Summary.app_sha256) 'Installed frontend differs from the bare runtime captured at the start of the test run.'
    $residentRegistryStates = @(Get-TestRegistryState | Where-Object { $_.exists })
    Assert-True ($residentRegistryStates.Count -ge 1) 'Resident-frontend test has no uninstall registration.'
    $expectedUninstallCommand = '"{0}" --operation uninstall --backend "{1}" --manifest "{2}" --install-dir "{3}"' -f $residentFrontend, $residentBackend, $residentManifest, $residentCase.directory
    foreach ($residentRegistryState in $residentRegistryStates) {
        Assert-True ($residentRegistryState.values['UninstallString'] -ceq $expectedUninstallCommand) 'Registered UninstallString does not contain the expected quoted resident frontend and paths.'
        Assert-True ($residentRegistryState.values['QuietUninstallString'] -ceq $expectedUninstallCommand) 'Registered QuietUninstallString differs from the expected silent resident-frontend command.'
    }
    Write-TaskJson (Join-Path $script:RunRoot 'M0-H05.installed.json') ([ordered]@{
        process = $residentInstallProcess; report = $residentInstallReport; receipt = $residentReceipt
        uninstaller_sha256 = (Get-FileHash -LiteralPath $residentBackend -Algorithm SHA256).Hash.ToLowerInvariant()
        files = @(Get-FileSnapshot $residentCase.directory); registry = $residentRegistryStates
    })
    $residentUninstallReportPath = Join-Path $script:RunRoot 'M0-H05.report.json'
    # Preserve the actual registered argument serialization. It is compared to
    # the known test-only command above, and passed directly to CreateProcess,
    # never to cmd.exe/PowerShell or a string-built shell command.
    $registeredCommand = [string]$residentRegistryStates[0].values['UninstallString']
    $registeredArguments = $registeredCommand.Substring(('"' + $residentFrontend + '"').Length).TrimStart()
    $registeredArguments += ' --report "' + $residentUninstallReportPath + '"'
    $residentUninstallProcess = Invoke-TaskProcess -Executable $residentFrontend -Arguments @() -Name 'M0-H05' -AwaitReport $residentUninstallReportPath -RawArguments $registeredArguments
    $residentUninstallReport = Read-OperationReport $residentUninstallReportPath 'Succeeded' $residentUninstallProcess
    Assert-True ($residentUninstallReport.coordinator_exit_code -eq 0) 'Detached uninstall coordinator reported failure despite the launcher exit.'
    Assert-ResourceSnapshot $residentCase.directory $residentCase.sentinels -Exact
    Assert-NoTestSystemResources
    Save-CaseEvidence 'M0-H05' $residentCase.directory $residentUninstallReport $residentUninstallProcess

    Assert-True ($script:Summary.cases.Count -eq $casePlan.Count) 'The executed case count differs from the selected plan.'
    Assert-True (($script:Summary.cases.id -join ',') -ceq ($casePlan.id -join ',')) 'The executed cases differ from the selected plan.'
    $script:Summary.status = 'PASSED'
    $script:Summary.active_case = $null
    Write-Host ("{0} local M0 smoke cases passed ({1}). This is not clean-machine, graphics, signing, upgrade, or release acceptance." -f $casePlan.Count, $executionScope.mode)
}
catch {
    $script:Summary.status = 'FAILED'
    $script:Summary.failure = $_.Exception.Message
    $script:Summary['registry_at_failure'] = @(Get-TestRegistryState)
    $failureSnapshots = @()
    foreach ($observedInstallation in $script:ObservedInstallations) {
        try {
            $failureSnapshots += [pscustomobject]@{ directory = $observedInstallation; files = @(Get-FileSnapshot $observedInstallation); snapshot_error = $null }
        }
        catch {
            $failureSnapshots += [pscustomobject]@{ directory = $observedInstallation; files = @(); snapshot_error = $_.Exception.Message }
        }
    }
    $script:Summary['files_at_failure'] = $failureSnapshots
    throw
}
finally {
    $script:Summary['finished_at_utc'] = [DateTime]::UtcNow.ToString('o')
    Write-TaskJson (Join-Path $script:RunRoot 'summary.json') $script:Summary
    Write-Host "Evidence retained: $(Join-Path $script:RunRoot 'summary.json')"
}
