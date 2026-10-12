[CmdletBinding()]
param(
    [switch]$PlanOnly,
    [Parameter(Mandatory)][string]$PayloadRoot,
    [Parameter(Mandatory)][string]$OutputRoot,
    [string]$MakensisPath = 'C:\Program Files (x86)\NSIS\makensis.exe',
    [ValidateSet('PerFileFirst', 'RecursiveFirst')][string]$Order = 'PerFileFirst',
    [ValidateRange(20, 2000)][int]$SampleIntervalMs = 100,
    [ValidateRange(250, 30000)][int]$DirectorySampleIntervalMs = 2000,
    [ValidateRange(30, 3600)][int]$TimeoutSeconds = 1800
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-Expansion([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Get-ExpansionFullPath([string]$Path) {
    return [IO.Path]::GetFullPath($Path).TrimEnd([char]'\', [char]'/')
}
function Assert-ExpansionPath([string]$Path) {
    $full = Get-ExpansionFullPath $Path
    $cursor = $full
    while ($cursor) {
        if ([IO.File]::Exists($cursor) -or [IO.Directory]::Exists($cursor)) {
            Assert-Expansion (([IO.File]::GetAttributes($cursor) -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse point rejected: $cursor"
        }
        $parent = [IO.Directory]::GetParent($cursor)
        $cursor = if ($parent) { $parent.FullName } else { $null }
    }
    return $full
}
function Get-ExpansionHash([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}
function Save-ExpansionJson([string]$Path, [object]$Value) {
    [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 40), [Text.UTF8Encoding]::new($false))
}
function Get-ExpansionInventory([string]$Directory) {
    $base = Assert-ExpansionPath $Directory
    Assert-Expansion ([IO.Directory]::Exists($base)) "Directory is missing: $base"
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($base)
    while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            Assert-Expansion (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse point rejected: $($item.FullName)"
            if ($item.PSIsContainer) { $pending.Push($item.FullName) }
            else {
                $relative = $item.FullName.Substring($base.Length + 1)
                # This experiment uses generated fixtures, not general-purpose package metadata.
                # Constrain every segment rather than relying on quoting for wildcards/macros.
                Assert-Expansion ($relative -cmatch '^[A-Za-z0-9_. -]+(?:\\[A-Za-z0-9_. -]+)*$') "Unsupported fixture path: $relative"
                [pscustomobject]@{ path = $relative; size = $item.Length; sha256 = Get-ExpansionHash $item.FullName }
            }
        }
    }
}
function Assert-ExpansionInventory([object[]]$Expected, [object[]]$Actual) {
    Assert-Expansion ($Expected.Count -eq $Actual.Count) "File count differs: expected $($Expected.Count), observed $($Actual.Count)."
    $found = @{}
    foreach ($file in $Actual) {
        Assert-Expansion (-not $found.ContainsKey($file.path)) "Duplicate file path: $($file.path)"
        $found[$file.path] = $file
    }
    foreach ($file in $Expected) {
        Assert-Expansion ($found.ContainsKey($file.path)) "Missing file: $($file.path)"
        $observed = $found[$file.path]
        Assert-Expansion ($file.size -eq $observed.size -and $file.sha256 -ceq $observed.sha256) "File size/hash differs: $($file.path)"
    }
}
function ConvertTo-ExpansionNsisLiteral([string]$Value) {
    Assert-Expansion (-not ($Value.ToCharArray() | Where-Object { [char]::IsControl($_) })) 'NSIS literals cannot contain control characters.'
    # Escape dollars before quotes so inserted NSIS escapes remain active.
    return $Value.Replace('$', '$$').Replace('"', '$\"')
}
function New-ExpansionScript([string]$Mode, [string]$Source, [string]$Target, [string]$EventsPath, [string]$BackendPath, [object[]]$Files) {
    Assert-Expansion ($Mode -in @('per-file', 'recursive')) "Unknown expansion mode: $Mode"
    Assert-Expansion ($Files.Count -gt 0) 'At least one fixture file is required.'
    $sourceLiteral = ConvertTo-ExpansionNsisLiteral $Source
    $targetLiteral = ConvertTo-ExpansionNsisLiteral $Target
    $eventsLiteral = ConvertTo-ExpansionNsisLiteral $EventsPath
    $backendLiteral = ConvertTo-ExpansionNsisLiteral $BackendPath
    $total = $Files.Count
    $lines = [Collections.Generic.List[string]]::new()
    foreach ($line in @(
        '; Avelin M0 isolated expansion experiment. NOT a production installer.',
        'Unicode true', 'Name "Avelin M0 expansion experiment"',
        ('OutFile "' + $backendLiteral + '"'), 'RequestExecutionLevel user',
        'SilentInstall silent', 'AutoCloseWindow true', 'SetCompressor /SOLID lzma',
        'SetOverwrite off', ('InstallDir "' + $targetLiteral + '"'),
        'Var EventHandle', 'Var Completed', 'Var Stage',
        'Function RecordExpansion',
        'System::Call "kernel32::GetTickCount64() l .r1"',
        ('FileWrite $EventHandle "$1|$Stage|$Completed|' + $total + '$\r$\n"'),
        'IfErrors event_failed', 'Return', 'event_failed:', 'SetErrorLevel 3', 'Quit', 'FunctionEnd',
        'Section "Expand"', 'SetShellVarContext current',
        ('FileOpen $EventHandle "' + $eventsLiteral + '" w'), 'IfErrors failed',
        'StrCpy $Completed 0', 'StrCpy $Stage "begin"', 'Call RecordExpansion',
        'ClearErrors', 'CreateDirectory "$INSTDIR"', 'IfErrors failed', 'SetOutPath "$INSTDIR"', 'IfErrors failed'
    )) { $lines.Add($line) }
    if ($Mode -eq 'per-file') {
        $lastParent = ''
        for ($index = 0; $index -lt $Files.Count; $index++) {
            $relative = [string]$Files[$index].path
            $parent = [IO.Path]::GetDirectoryName($relative)
            if ($parent -cne $lastParent) {
                $suffix = if ($parent) { '\' + (ConvertTo-ExpansionNsisLiteral $parent) } else { '' }
                $lines.Add('SetOutPath "$INSTDIR' + $suffix + '"')
                $lines.Add('IfErrors failed')
                $lastParent = $parent
            }
            $lines.Add('ClearErrors')
            $lines.Add('File "' + $sourceLiteral + '\' + (ConvertTo-ExpansionNsisLiteral $relative) + '"')
            $lines.Add('IfErrors failed')
            $lines.Add('StrCpy $Completed ' + ($index + 1))
            $lines.Add('StrCpy $Stage "progress"')
            $lines.Add('Call RecordExpansion')
        }
    }
    else {
        $lines.Add('ClearErrors')
        $lines.Add('File /r "' + $sourceLiteral + '\*.*"')
        $lines.Add('IfErrors failed')
        $lines.Add('StrCpy $Completed ' + $total)
        $lines.Add('StrCpy $Stage "progress"')
        $lines.Add('Call RecordExpansion')
    }
    foreach ($line in @(
        'StrCpy $Stage "finish"', 'Call RecordExpansion', 'FileClose $EventHandle',
        'SetErrorLevel 0', 'Quit', 'failed:', 'FileClose $EventHandle', 'SetErrorLevel 3', 'Quit', 'SectionEnd'
    )) { $lines.Add($line) }
    return ($lines -join "`r`n") + "`r`n"
}
function Get-ExpansionEvents([string]$Path, [string]$Mode, [int]$FileCount) {
    $events = [Collections.Generic.List[object]]::new()
    $firstTick = $null
    foreach ($line in [IO.File]::ReadAllLines($Path)) {
        Assert-Expansion ($line -match '^(\d+)\|(begin|progress|finish)\|(\d+)\|(\d+)$') "Malformed expansion event: $line"
        [long]$tick = $Matches[1]
        if ($null -eq $firstTick) { $firstTick = $tick }
        Assert-Expansion ($tick -ge $firstTick) 'GetTickCount64 event time went backwards.'
        Assert-Expansion ([int]$Matches[4] -eq $FileCount) 'Event total differs from source inventory.'
        $events.Add([pscustomobject]@{ tick_ms = $tick; elapsed_ms = $tick - $firstTick; kind = $Matches[2]; completed = [int]$Matches[3]; total = [int]$Matches[4] })
    }
    $progress = @($events | Where-Object kind -eq 'progress')
    $expectedCount = if ($Mode -eq 'per-file') { $FileCount } else { 1 }
    Assert-Expansion ($events.Count -eq $expectedCount + 2) 'Unexpected expansion event count.'
    Assert-Expansion ($progress.Count -eq $expectedCount) 'Unexpected progress callback count.'
    Assert-Expansion ($events[0].kind -ceq 'begin' -and $events[0].completed -eq 0) 'Initial expansion event is missing.'
    Assert-Expansion ($events[$events.Count - 1].kind -ceq 'finish' -and $events[$events.Count - 1].completed -eq $FileCount) 'Final expansion event is missing.'
    for ($index = 1; $index -lt $events.Count - 1; $index++) {
        Assert-Expansion ($events[$index].kind -ceq 'progress') 'Only progress callbacks may occur between begin and finish.'
    }
    $previousTick = $events[0].tick_ms
    $previousCompleted = 0
    [long]$largestGap = 0
    [int]$largestStep = 0
    for ($index = 0; $index -lt $progress.Count; $index++) {
        $event = $progress[$index]
        $expectedCompleted = if ($Mode -eq 'per-file') { $index + 1 } else { $FileCount }
        Assert-Expansion ($event.completed -eq $expectedCompleted -and $event.tick_ms -ge $previousTick) 'Expansion progress sequence differs from the generated script.'
        $largestStep = [Math]::Max($largestStep, $event.completed - $previousCompleted)
        $largestGap = [Math]::Max($largestGap, $event.tick_ms - $previousTick)
        $previousTick = $event.tick_ms
        $previousCompleted = $event.completed
    }
    Assert-Expansion ($events[$events.Count - 1].tick_ms -ge $previousTick) 'Final expansion time went backwards.'
    $buckets = @($events | Group-Object { [long][Math]::Floor($_.elapsed_ms / 1000.0) } | ForEach-Object { [pscustomobject]@{ second_from_first_event = [int]$_.Name; event_count = $_.Count } } | Sort-Object second_from_first_event)
    return [pscustomobject]@{
        events = $events.ToArray(); one_second_buckets = $buckets
        event_count = $events.Count; progress_event_count = $progress.Count
        peak_events_in_one_second_bucket = ($buckets | Measure-Object event_count -Maximum).Maximum
        expansion_elapsed_ms = $events[$events.Count - 1].elapsed_ms
        largest_progress_step_files = $largestStep; longest_progress_gap_ms = $largestGap
        scope = 'NSIS synchronous local timestamp/file callbacks; NOT bridge transport throughput, UI redraw rate, or production installation progress. Begin/finish count in event rate. Fixed one-second buckets originate at first begin; subsecond runs are not normalized/extrapolated.'
    }
}

if ($PlanOnly) {
    [ordered]@{
        status = 'PLAN_ONLY_NOT_EXECUTED'; payload_root = $PayloadRoot; output_root = $OutputRoot; makensis = $MakensisPath; order = $Order
        sample_target_interval_ms = $SampleIntervalMs; directory_sample_target_interval_ms = $DirectorySampleIntervalMs
        timeout_seconds = $TimeoutSeconds
        scope = 'Same immutable payload, per-file File versus File /r, fixed Unicode/user/silent/solid-LZMA configuration. Isolated expansion and local callback granularity only; no production receipt, bridge, registry, uninstall, or GUI claims.'
        outputs = 'Separate fresh retained extraction directories, scripts, executables, complete hashes, timestamped events, compiler/runtime process and directory samples. No deletion or force-kill.'
    } | ConvertTo-Json -Depth 8
    exit 0
}

Assert-Expansion ($PSVersionTable.PSVersion.Major -ge 7 -and [Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) 'Run this experiment in PowerShell 7 on Windows.'
$PayloadRoot = Assert-ExpansionPath $PayloadRoot
$OutputRoot = Assert-ExpansionPath $OutputRoot
$MakensisPath = Assert-ExpansionPath $MakensisPath
Assert-Expansion ([IO.Directory]::Exists($PayloadRoot)) 'Payload root does not exist.'
Assert-Expansion ([IO.File]::Exists($MakensisPath)) 'makensis executable does not exist.'
Assert-Expansion (-not [IO.Directory]::Exists($OutputRoot) -and -not [IO.File]::Exists($OutputRoot)) 'Choose a new output directory; existing evidence is never overwritten.'
Assert-Expansion (-not $OutputRoot.StartsWith($PayloadRoot + '\', [StringComparison]::OrdinalIgnoreCase) -and -not $PayloadRoot.StartsWith($OutputRoot + '\', [StringComparison]::OrdinalIgnoreCase) -and -not $PayloadRoot.Equals($OutputRoot, [StringComparison]::OrdinalIgnoreCase)) 'Payload and output directories must not overlap.'
$helper = Assert-ExpansionPath (Join-Path $PSScriptRoot 'scale\M0.Performance.ps1')
Assert-Expansion ([IO.File]::Exists($helper)) 'Shared performance sampler is missing.'
. $helper
$null = [IO.Directory]::CreateDirectory($OutputRoot)
[IO.File]::Copy($PSCommandPath, (Join-Path $OutputRoot 'harness-source.ps1'), $false)
[IO.File]::Copy($helper, (Join-Path $OutputRoot 'sampler-source.ps1'), $false)
$summary = [ordered]@{
    schema_version = 1; status = 'RUNNING'; active_case = 'INVENTORY'; failure = $null
    started_at_utc = [DateTime]::UtcNow.ToString('o'); completed_at_utc = $null
    payload_root = $PayloadRoot; output_root = $OutputRoot; order = $Order
    powershell = $PSVersionTable.PSVersion.ToString(); framework = [Runtime.InteropServices.RuntimeInformation]::FrameworkDescription
    makensis_path = $MakensisPath; makensis_sha256 = Get-ExpansionHash $MakensisPath
    harness_sha256 = Get-ExpansionHash (Join-Path $OutputRoot 'harness-source.ps1')
    sampler_sha256 = Get-ExpansionHash (Join-Path $OutputRoot 'sampler-source.ps1')
    configuration = 'Unicode true; RequestExecutionLevel user; SilentInstall silent; SetCompressor /SOLID lzma; SetOverwrite off; makensis /NOCONFIG /WX /V2'
    scope = 'Isolated extraction experiment, not the Avelin production installation contract. File callbacks include local timestamp/log overhead; bridge, receipt, registry, uninstall and GUI are absent. Sequential warm-cache observations; order is recorded and no cache flush is attempted.'
    source_file_count = $null; source_bytes = $null; source_manifest_sha256 = $null; cases = @()
}
try {
    Save-ExpansionJson (Join-Path $OutputRoot 'summary.json') $summary
    Write-Host "Expansion comparison evidence: $OutputRoot"
    $expected = @(Get-ExpansionInventory $PayloadRoot | Sort-Object path -CaseSensitive)
    Assert-Expansion ($expected.Count -gt 0) 'Empty payload is not an expansion benchmark.'
    Save-ExpansionJson (Join-Path $OutputRoot 'source-manifest.json') $expected
    $summary.source_file_count = $expected.Count
    $summary.source_bytes = [long]($expected | Measure-Object size -Sum).Sum
    $summary.source_manifest_sha256 = Get-ExpansionHash (Join-Path $OutputRoot 'source-manifest.json')
    $modes = if ($Order -eq 'PerFileFirst') { @('per-file', 'recursive') } else { @('recursive', 'per-file') }
    foreach ($mode in $modes) {
        $summary.active_case = $mode
        Save-ExpansionJson (Join-Path $OutputRoot 'summary.json') $summary
        $caseRoot = Join-Path $OutputRoot $mode
        $temp = Join-Path $caseRoot 'temp'
        $target = Join-Path $caseRoot 'expanded'
        $eventsPath = Join-Path $caseRoot 'expansion.events.log'
        $backend = Join-Path $caseRoot 'expand.exe'
        $scriptPath = Join-Path $caseRoot 'expansion.nsi'
        $null = [IO.Directory]::CreateDirectory($temp)
        $scriptText = New-ExpansionScript $mode $PayloadRoot $target $eventsPath $backend $expected
        [IO.File]::WriteAllText($scriptPath, $scriptText, [Text.UTF8Encoding]::new($true))
        $environment = @{ TEMP = $temp; TMP = $temp }
        $compile = Invoke-M0MeasuredProcess -Executable $MakensisPath -Arguments @('/NOCONFIG', '/WX', '/V2', $scriptPath) -WorkingDirectory $caseRoot -Environment $environment -OutputPrefix (Join-Path $caseRoot 'compile') -TimeoutSeconds $TimeoutSeconds -SampleIntervalMs $SampleIntervalMs -IsolatedRoot $OutputRoot -DirectoryTargets @{ temp = $temp } -DirectorySampleIntervalMs $DirectorySampleIntervalMs
        Assert-Expansion ($compile.status -ceq 'EXITED' -and $compile.exit_code -eq 0) "$mode compiler did not finish successfully; inspect retained process evidence."
        Assert-Expansion ($compile.logs_complete_at_eof -and -not $compile.process_was_killed -and @($compile.live_process_identities_at_end).Count -eq 0) "$mode compiler EOF or process-family completion was not verified."
        Assert-Expansion ([IO.File]::Exists($backend)) 'Compiler returned without an executable.'
        Assert-Expansion (-not [IO.Directory]::Exists($target) -and -not [IO.File]::Exists($target)) 'Extraction target exists; refusing to run.'
        Assert-Expansion (-not [IO.File]::Exists($eventsPath)) 'Event log exists; refusing to overwrite.'
        # Confirm both modes compile and run against the same unchanged input bytes.
        $sourceAfterCompile = @(Get-ExpansionInventory $PayloadRoot | Sort-Object path -CaseSensitive)
        Assert-ExpansionInventory $expected $sourceAfterCompile
        $run = Invoke-M0MeasuredProcess -Executable $backend -Arguments @('/S') -WorkingDirectory $caseRoot -Environment $environment -OutputPrefix (Join-Path $caseRoot 'expand') -TimeoutSeconds $TimeoutSeconds -SampleIntervalMs $SampleIntervalMs -IsolatedRoot $OutputRoot -DirectoryTargets @{ temp = $temp; output = $target } -DirectorySampleIntervalMs $DirectorySampleIntervalMs
        Assert-Expansion ($run.status -ceq 'EXITED' -and $run.exit_code -eq 0) "$mode extraction did not finish successfully; inspect retained process evidence."
        Assert-Expansion ($run.logs_complete_at_eof -and -not $run.process_was_killed -and @($run.live_process_identities_at_end).Count -eq 0) "$mode extraction EOF or process-family completion was not verified."
        $actual = @(Get-ExpansionInventory $target | Sort-Object path -CaseSensitive)
        Save-ExpansionJson (Join-Path $caseRoot 'expanded-manifest.json') $actual
        Assert-ExpansionInventory $expected $actual
        $events = Get-ExpansionEvents $eventsPath $mode $expected.Count
        Save-ExpansionJson (Join-Path $caseRoot 'events.json') $events
        $record = [ordered]@{
            mode = $mode; status = 'PASSED'; source_manifest_sha256 = $summary.source_manifest_sha256
            script_path = $scriptPath; script_sha256 = Get-ExpansionHash $scriptPath; script_bytes = (Get-Item -LiteralPath $scriptPath).Length; script_lines = [IO.File]::ReadAllLines($scriptPath).Count
            backend_sha256 = Get-ExpansionHash $backend; backend_bytes = (Get-Item -LiteralPath $backend).Length
            compile = $compile; expansion = $run; verified_files = $actual.Count
            expanded_manifest_sha256 = Get-ExpansionHash (Join-Path $caseRoot 'expanded-manifest.json')
            event_count = $events.event_count; progress_event_count = $events.progress_event_count
            peak_events_in_one_second_bucket = $events.peak_events_in_one_second_bucket
            expansion_elapsed_ms = $events.expansion_elapsed_ms; largest_progress_step_files = $events.largest_progress_step_files; longest_progress_gap_ms = $events.longest_progress_gap_ms
            event_scope = $events.scope; retained_output = $target
        }
        Save-ExpansionJson (Join-Path $caseRoot 'result.json') $record
        $summary.cases += $record
        Save-ExpansionJson (Join-Path $OutputRoot 'summary.json') $summary
    }
    $finalSource = @(Get-ExpansionInventory $PayloadRoot | Sort-Object path -CaseSensitive)
    Assert-ExpansionInventory $expected $finalSource
    $summary.status = 'PASSED_SCOPED_EXPANSION_COMPARISON'
    $summary.active_case = 'COMPLETE'
}
catch {
    $summary.status = 'FAILED_OR_INCOMPLETE'
    $summary.failure = $_.Exception.Message
    throw
}
finally {
    $summary.completed_at_utc = [DateTime]::UtcNow.ToString('o')
    Save-ExpansionJson (Join-Path $OutputRoot 'summary.json') $summary
    Write-Host "Expansion comparison: $($summary.status) — $OutputRoot"
}
