# Shared isolated Windows fixture helpers; no top-level build or installation.

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
    Assert-True (Test-Path -LiteralPath $uninstallerPath -PathType Leaf) 'Installed Rust uninstaller is missing.'
    Assert-True ((Get-FileHash -LiteralPath $uninstallerPath -Algorithm SHA256).Hash.ToLowerInvariant() -ceq $receipt.uninstaller_sha256) 'Installed Rust uninstaller differs from the committed receipt hash.'
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

function Assert-CommittedReceiptCandidate {
    param([object]$Receipt, [object]$Candidate, [string]$BackendHash)
    Assert-True ($BackendHash -cmatch '^[0-9a-f]{64}$') 'Invalid independent worker hash.'
    $expected = $Candidate | ConvertTo-Json -Depth 30 | ConvertFrom-Json -AsHashtable
    $actual = $Receipt | ConvertTo-Json -Depth 30 | ConvertFrom-Json -AsHashtable
    Assert-True (-not $expected.ContainsKey('uninstaller_sha256') -or $null -eq $expected.uninstaller_sha256) 'Fresh install candidate already declares an uninstaller hash.'
    $expected['uninstaller_sha256'] = $BackendHash
    Assert-True ($actual.Count -eq $expected.Count) 'Committed receipt fields differ from the candidate contract.'
    foreach ($field in $expected.Keys) {
        Assert-True ($actual.ContainsKey($field) -and (ConvertTo-Json -InputObject $actual[$field] -Depth 30 -Compress) -ceq (ConvertTo-Json -InputObject $expected[$field] -Depth 30 -Compress)) "Committed receipt changed candidate field: $field"
    }
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
    # Auxiliary App extraction stays inside this run, not shared user temp.
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
