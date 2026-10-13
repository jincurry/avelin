#requires -Version 7.0
<#
.SYNOPSIS
Read PE imports with MSVC dumpbin without launching the inspected EXE or DLL.
.DESCRIPTION
Uses vswhere to locate an explicit Hostx64/x64 dumpbin.exe. Saves unmodified
stdout/stderr for /headers and /imports, including delay-load import output.
Records tool and artifact hashes, PE architecture and imported DLL names.

This is a direct PE import audit, not a clean-machine compatibility test.
It does not resolve transitive dependencies, activate COM, call LoadLibrary,
execute inspected code, or enumerate DLL names computed at runtime.
Relative paths are resolved against the repository root. OutputDirectory must
be new. No existing output is overwritten and no files are removed.
.PARAMETER Files
One or more existing PE EXE/DLL files. Pass a PowerShell array when invoking
the script from PowerShell, for example: & ./scripts/inspect-m0-imports.ps1
-Files @('target/release/avelin-app.exe', 'target/release/avelin-engine.exe').
.PARAMETER OutputDirectory
New result directory. Defaults to target/m0-import-audit/<timestamp>-<suffix>.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string[]]$Files,
    [string]$OutputDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This audit requires Windows and MSVC dumpbin.' }
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$startedAt = [DateTime]::UtcNow

function Resolve-ProjectPath([string]$Path) {
    if ([IO.Path]::IsPathRooted($Path)) { return [IO.Path]::GetFullPath($Path) }
    return [IO.Path]::GetFullPath((Join-Path $projectRoot $Path))
}

function Assert-NoReparse([string]$Path) {
    $current = [IO.Path]::GetFullPath($Path)
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "Reparse path is outside this audit's supported inputs: $current"
            }
        }
        $parent = [IO.Path]::GetDirectoryName($current)
        if ($parent -eq $current) { break }
        $current = $parent
    }
}

function Invoke-Dumpbin([string]$InputFile, [string]$Option, [string]$LogBase) {
    $stdoutPath = "$LogBase.stdout.log"
    $stderrPath = "$LogBase.stderr.log"
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $script:dumpbinPath
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.Environment['VSLANG'] = '1033'
    foreach ($argument in @('/nologo', $Option, $InputFile)) {
        $start.ArgumentList.Add($argument)
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        if (-not $process.Start()) { throw 'Unable to start dumpbin.' }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $timedOut = -not $process.WaitForExit(30000)
        if ($timedOut) {
            # Only the dumpbin process created above; never the inspected binary.
            $process.Kill()
            $process.WaitForExit()
        }
        $outText = $stdout.GetAwaiter().GetResult()
        $errText = $stderr.GetAwaiter().GetResult()
        [IO.File]::WriteAllText($stdoutPath, $outText, [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllText($stderrPath, $errText, [Text.UTF8Encoding]::new($false))
        return [ordered]@{
            executable = $script:dumpbinPath
            arguments = @('/nologo', $Option, $InputFile)
            exit_code = $process.ExitCode
            timed_out = $timedOut
            elapsed_ms = $watch.ElapsedMilliseconds
            stdout = $stdoutPath
            stderr = $stderrPath
        }
    }
    finally { $process.Dispose() }
}

function Read-DirectoryEntry([string]$Headers, [string]$Name) {
    $pattern = '(?m)^\s*([0-9A-F]+)\s+\[\s*([0-9A-F]+)\]\s+RVA \[size\] of ' +
        [regex]::Escape($Name) + '\s*$'
    $match = [regex]::Match($Headers, $pattern, [Text.RegularExpressions.RegexOptions]::IgnoreCase)
    if (-not $match.Success) { return $null }
    return [ordered]@{
        rva_hex = $match.Groups[1].Value
        size_hex = $match.Groups[2].Value
        present = ([Convert]::ToUInt64($match.Groups[1].Value, 16) -ne 0 -or
            [Convert]::ToUInt64($match.Groups[2].Value, 16) -ne 0)
    }
}

$resolvedInputs = @($Files | ForEach-Object {
    $path = Resolve-ProjectPath $_
    Assert-NoReparse $path
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Input file does not exist: $path"
    }
    $path
})
$vswherePath = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
if (-not (Test-Path -LiteralPath $vswherePath -PathType Leaf)) {
    throw "vswhere was not found at the standard installer path: $vswherePath"
}
$candidates = @(& $vswherePath -latest -products '*' `
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
    -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe')
if ($LASTEXITCODE -ne 0 -or $candidates.Count -eq 0) {
    throw 'vswhere could not locate MSVC Hostx64/x64 dumpbin.exe.'
}
$script:dumpbinPath = [IO.Path]::GetFullPath(($candidates | Sort-Object -Descending | Select-Object -First 1))
if (-not (Test-Path -LiteralPath $script:dumpbinPath -PathType Leaf)) {
    throw "dumpbin does not exist: $script:dumpbinPath"
}

if (-not $OutputDirectory) {
    $runName = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ') + '-' +
        [Guid]::NewGuid().ToString('N').Substring(0, 8)
    $OutputDirectory = Join-Path 'target/m0-import-audit' $runName
}
$outputRoot = Resolve-ProjectPath $OutputDirectory
Assert-NoReparse $outputRoot
if (Test-Path -LiteralPath $outputRoot) {
    throw "Output directory already exists; choose a new directory: $outputRoot"
}
New-Item -ItemType Directory -Path $outputRoot | Out-Null
Copy-Item -LiteralPath $PSCommandPath -Destination (Join-Path $outputRoot 'inspect-m0-imports.ps1')

$records = [Collections.Generic.List[object]]::new()
$index = 0
foreach ($path in $resolvedInputs) {
    $index++
    $prefix = '{0:D2}-{1}' -f $index, ([IO.Path]::GetFileName($path) -replace '[^a-zA-Z0-9_.-]', '_')
    $shaBefore = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    $bytesBefore = (Get-Item -LiteralPath $path).Length
    $headerResult = Invoke-Dumpbin $path '/headers' (Join-Path $outputRoot "$prefix.headers")
    $importResult = Invoke-Dumpbin $path '/imports' (Join-Path $outputRoot "$prefix.imports")
    $shaAfter = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    $headers = [IO.File]::ReadAllText($headerResult.stdout)
    $imports = [IO.File]::ReadAllText($importResult.stdout)
    $machine = [regex]::Match($headers, '(?m)^\s*([0-9A-F]+) machine \(([^)]+)\)\s*$')
    $subsystem = [regex]::Match($headers, '(?m)^\s*([0-9A-F]+) subsystem \(([^)]+)\)\s*$')
    $directDirectory = Read-DirectoryEntry $headers 'Import Directory'
    $delayDirectory = Read-DirectoryEntry $headers 'Delay Import Directory'
    $direct = [Collections.Generic.List[string]]::new()
    $delay = [Collections.Generic.List[string]]::new()
    $section = ''
    foreach ($line in ($imports -split '\r?\n')) {
        if ($line -match '^\s*Section contains the following imports:\s*$') {
            $section = 'direct'
        }
        elseif ($line -match '^\s*Section contains the following delay load imports:\s*$') {
            $section = 'delay'
        }
        elseif ($line -match '^\s*Summary\s*$') { $section = '' }
        elseif ($line -match '^\s{4}([^\s]+\.dll)\s*$') {
            if ($section -eq 'direct') { $direct.Add($Matches[1]) }
            elseif ($section -eq 'delay') { $delay.Add($Matches[1]) }
        }
    }
    $directNames = @($direct | Sort-Object -Unique)
    $delayNames = @($delay | Sort-Object -Unique)
    $allNames = @(@($directNames) + @($delayNames) | Sort-Object -Unique)
    $runtimeFamily = @($allNames | Where-Object {
        $_ -match '^(msvcrt|ucrtbase|vcruntime[^.]*|msvcp[^.]*|msvcr[^.]*|concrt[^.]*|vcomp[^.]*|vcamp[^.]*|api-ms-win-crt-[^.]*)\.dll$'
    })
    $dynamicApis = @([regex]::Matches($imports,
        '(?m)^\s+[0-9A-F]+\s+(_?(?:LoadLibrary(?:Ex)?[AW]?|GetProcAddress|LdrLoadDll|LdrGetProcedureAddress)(?:@\d+)?)\s*$') |
        ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique)
    $complete = $headerResult.exit_code -eq 0 -and $importResult.exit_code -eq 0 -and
        -not $headerResult.timed_out -and -not $importResult.timed_out -and
        $shaBefore -eq $shaAfter -and $machine.Success -and
        $null -ne $directDirectory -and $null -ne $delayDirectory
    if ($complete) {
        $complete = ($directDirectory.present -eq ($directNames.Count -gt 0)) -and
            ($delayDirectory.present -eq ($delayNames.Count -gt 0))
    }
    $records.Add([ordered]@{
        file = $path
        bytes = $bytesBefore
        sha256_before = $shaBefore
        sha256_after = $shaAfter
        unchanged_during_audit = $shaBefore -eq $shaAfter
        machine_hex = if ($machine.Success) { $machine.Groups[1].Value } else { $null }
        architecture = if ($machine.Success) { $machine.Groups[2].Value } else { $null }
        subsystem = if ($subsystem.Success) { $subsystem.Groups[2].Value } else { $null }
        direct_import_directory = $directDirectory
        delay_import_directory = $delayDirectory
        direct_import_dlls = $directNames
        delay_import_dlls = $delayNames
        c_cpp_runtime_family_imports = $runtimeFamily
        dynamic_loader_api_imports = $dynamicApis
        assessment = if ($complete) { 'StaticImportsAudited' } else { 'Incomplete' }
        commands = @($headerResult, $importResult)
    })
}

$summary = [ordered]@{
    schema_version = 1
    started_at_utc = $startedAt.ToString('o')
    finished_at_utc = [DateTime]::UtcNow.ToString('o')
    powershell = $PSVersionTable.PSVersion.ToString()
    script_sha256 = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash
    vswhere_path = $vswherePath
    dumpbin_path = $script:dumpbinPath
    dumpbin_file_version = [Diagnostics.FileVersionInfo]::GetVersionInfo($script:dumpbinPath).FileVersion
    dumpbin_sha256 = (Get-FileHash -LiteralPath $script:dumpbinPath -Algorithm SHA256).Hash
    assessment = if (@($records | Where-Object assessment -ne 'StaticImportsAudited').Count -eq 0) {
        'StaticImportsAudited'
    } else { 'Incomplete' }
    inspected_code_executed = $false
    transitive_imports_inspected = $false
    runtime_dynamic_loads_enumerated = $false
    clean_machine_compatibility_established = $false
    limitation = 'Direct and delay-load PE imports only. No clean-machine, API availability, COM activation, driver, transitive dependency, or runtime dynamic-load validation.'
    import_option_reference = 'https://learn.microsoft.com/en-us/cpp/build/reference/imports-dumpbin?view=msvc-170'
    artifacts = $records.ToArray()
}
$summaryPath = Join-Path $outputRoot 'summary.json'
$summary | ConvertTo-Json -Depth 15 | Set-Content -LiteralPath $summaryPath -Encoding utf8NoBOM
Write-Output $summaryPath
if ($summary.assessment -ne 'StaticImportsAudited') {
    throw "Import audit is incomplete; inspect the preserved raw logs and summary: $summaryPath"
}
