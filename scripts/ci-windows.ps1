#requires -Version 7.2
<#
.SYNOPSIS
Build, package and validate the current source on a provisioned Windows host.
.DESCRIPTION
Uses installed tools only; never installs Rust, targets, NSIS, Python or SDKs.
Cargo may fetch dependencies selected by Cargo.lock. Builds fresh components,
then a new devkit, then runs its normal-cycle harness. No GUI automation,
performance measurement, signing or fault injection. Existing evidence is never
overwritten or recursively deleted. A failed run keeps its files for inspection.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$OutputDirectory,
    [string]$NsisRoot = $env:AVELIN_NSIS_ROOT,
    [string]$RustToolchain = 'stable',
    [string]$ExpectedRustVersion,
    [string]$PythonPath = $env:AVELIN_PYTHON,
    [string]$CargoTargetDirectory
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($env:CARGO_BUILD_TARGET) { throw 'Unset CARGO_BUILD_TARGET; this entry uses an x64 host and an explicit x86 bridge target.' }
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$runRoot = [IO.Path]::GetFullPath($OutputDirectory, $projectRoot)
if (Test-Path -LiteralPath $runRoot) { throw "OutputDirectory already exists; choose a new evidence directory: $runRoot" }
if (-not $IsWindows -or -not [Environment]::Is64BitProcess) { throw 'This entry requires Windows x64 PowerShell 7.2+.' }
$null = New-Item -ItemType Directory -Path $runRoot
$logRoot = Join-Path $runRoot 'logs'
$null = New-Item -ItemType Directory -Path $logRoot
$summary = [ordered]@{
    schema_version = 1; status = 'RUNNING'; kit_version = '0.1.0-preview.1'
    started_at_utc = [DateTime]::UtcNow.ToString('o'); project_root = $projectRoot
    output_directory = $runRoot; source_build = $true; cloud_execution_claim = 'NOT_ASSERTED'
    excluded = @('50000-file boundary test', 'performance', 'signing', 'failure injection', 'independent environment qualification', 'native GUI/DPI/theme/Narrator matrix')
    commands = [Collections.Generic.List[object]]::new()
}
$originalToolchain = $env:RUSTUP_TOOLCHAIN
$originalCargoTarget = $env:CARGO_TARGET_DIR

function Require([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Save([string]$Path, [object]$Value) {
    [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 30), [Text.UTF8Encoding]::new($false))
}
function FindTool([string]$Name, [string]$Configured = '') {
    if ($Configured) {
        $path = [IO.Path]::GetFullPath($Configured, $projectRoot)
        Require (Test-Path -LiteralPath $path -PathType Leaf) "Configured tool missing: $path"
        return $path
    }
    $command = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    Require ($null -ne $command) "Required installed tool unavailable: $Name"
    return $command.Source
}
function InvokeRecorded([string]$Name, [string]$Executable, [string[]]$Arguments) {
    $info = [Diagnostics.ProcessStartInfo]::new($Executable)
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
    $info.WorkingDirectory = $projectRoot
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $info.StandardErrorEncoding = $info.StandardOutputEncoding
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $record = [ordered]@{ name = $Name; executable = $Executable; arguments = $Arguments; status = 'STARTING' }
    $summary.commands.Add($record)
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    Write-Host "[$Name] starting"
    try {
        $null = $process.Start(); $record.pid = $process.Id; $record.status = 'RUNNING'
        $stdoutTask = $process.StandardOutput.ReadToEndAsync(); $stderrTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $stdout = $stdoutTask.GetAwaiter().GetResult(); $stderr = $stderrTask.GetAwaiter().GetResult()
        $record.exit_code = $process.ExitCode
        $record.stdout = Join-Path $logRoot "$Name.stdout.log"; $record.stderr = Join-Path $logRoot "$Name.stderr.log"
        [IO.File]::WriteAllText($record.stdout, $stdout, [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllText($record.stderr, $stderr, [Text.UTF8Encoding]::new($false))
        $record.status = if ($record.exit_code -eq 0) { 'EXITED_ZERO' } else { 'FAILED' }
        Require ($record.exit_code -eq 0) "$Name exited $($record.exit_code); inspect $logRoot"
        Write-Host "[$Name] passed"
        return $stdout.Trim()
    } finally { $process.Dispose() }
}
function Inventory([string]$Root) {
    $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push($Root)
    $rows = while ($pending.Count) {
        $directory = Get-Item -LiteralPath $pending.Pop() -Force
        Require (($directory.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Tool directory contains a reparse point: $($directory.FullName)"
        foreach ($item in Get-ChildItem -LiteralPath $directory.FullName -Force) {
            Require (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Tool tree contains a reparse point: $($item.FullName)"
            if ($item.PSIsContainer) { $pending.Push($item.FullName) }
            else { [pscustomobject]@{ path = [IO.Path]::GetRelativePath($Root, $item.FullName); size = $item.Length; sha256 = Hash $item.FullName } }
        }
    }
    return @($rows | Sort-Object path)
}
function ReadJson([string]$Path) { Get-Content -LiteralPath $Path -Raw -Encoding utf8 | ConvertFrom-Json }

try {
    $rustup = FindTool 'rustup.exe'
    $python = FindTool 'python.exe' $PythonPath
    $pwsh = Join-Path $PSHOME 'pwsh.exe'
    Require ($RustToolchain -match '^[A-Za-z0-9_.-]+$') 'RustToolchain must name a preinstalled rustup toolchain.'
    $installed = InvokeRecorded 'rustup-installed' $rustup @('toolchain', 'list')
    $toolchainNames = @($installed -split '\r?\n' | ForEach-Object { ($_ -split '\s+')[0] })
    Require (($toolchainNames -contains $RustToolchain) -or ($toolchainNames -contains "$RustToolchain-x86_64-pc-windows-msvc")) "Toolchain is not installed: $RustToolchain. No download was attempted."
    $env:RUSTUP_TOOLCHAIN = $RustToolchain
    $version = InvokeRecorded 'rustc-version' $rustup @('run', $RustToolchain, 'rustc', '--version', '--verbose')
    Require ($version -match '(?m)^host: x86_64-pc-windows-msvc\s*$') 'Expected a preinstalled x86_64-pc-windows-msvc Rust host.'
    if ($ExpectedRustVersion) {
        Require ($ExpectedRustVersion -match '^\d+\.\d+\.\d+$') 'ExpectedRustVersion must be an exact stable release such as 1.97.1.'
        Require ($version -match ('(?m)^release: ' + [regex]::Escape($ExpectedRustVersion) + '\s*$')) "Rust differs from explicitly requested $ExpectedRustVersion; provision that toolchain or omit -ExpectedRustVersion to record the available version."
    }
    $rustExecutables = @(foreach ($name in @('rustc', 'cargo', 'rustfmt')) {
        $path = InvokeRecorded "rustup-which-$name" $rustup @('which', '--toolchain', $RustToolchain, $name)
        [pscustomobject]@{ name = $name; path = $path; sha256 = Hash $path }
    })
    $targets = InvokeRecorded 'rust-targets' $rustup @('target', 'list', '--installed', '--toolchain', $RustToolchain)
    foreach ($target in @('x86_64-pc-windows-msvc', 'i686-pc-windows-msvc')) {
        Require (($targets -split '\r?\n') -contains $target) "Required Rust target is not installed: $target"
    }
    $pythonVersion = InvokeRecorded 'python-version' $python @('-c', 'import sys; assert sys.version_info >= (3, 11), "Python 3.11+ required"; print(sys.version)')
    if (-not $NsisRoot) { throw 'Set -NsisRoot or AVELIN_NSIS_ROOT to the provisioned NSIS 3.12 directory.' }
    $nsisRootPath = [IO.Path]::GetFullPath($NsisRoot, $projectRoot).TrimEnd([char]'\', [char]'/')
    $makensis = Join-Path $nsisRootPath 'makensis.exe'
    $compiler = Join-Path $nsisRootPath 'Bin/makensis.exe'
    Require ((Hash $makensis) -ceq 'b043e554afefbfc56315669d0b4779793aeae67f0f2a7a790e2ea91f05298eff') 'NSIS launcher differs from reviewed 3.12 bytes.'
    Require ((Hash $compiler) -ceq '25d1aa7081db1a9de9690b59983f9652b7409a189c3444328cc1841eff693e8d') 'NSIS actual compiler differs from reviewed 3.12 bytes.'
    $nsisVersion = InvokeRecorded 'nsis-version' $makensis @('/VERSION')
    Require ($nsisVersion -ceq 'v3.12') "Expected NSIS v3.12, observed $nsisVersion"
    $nsisInventory = @(Inventory $nsisRootPath)
    Save (Join-Path $runRoot 'nsis-source-inventory.json') $nsisInventory
    $summary.tools = @{ rust_toolchain = $RustToolchain; expected_rust_version = $ExpectedRustVersion; rustc_verbose = $version; rust_executable_hashes = $rustExecutables; python = $pythonVersion; nsis_version = $nsisVersion; nsis_root = $nsisRootPath; nsis_source_inventory_sha256 = Hash (Join-Path $runRoot 'nsis-source-inventory.json') }
    $summary.cargo_lock_sha256 = Hash (Join-Path $projectRoot 'Cargo.lock')
    $targetDirectory = if ($CargoTargetDirectory) { [IO.Path]::GetFullPath($CargoTargetDirectory, $projectRoot) } else { Join-Path $runRoot 'cargo-target' }
    Require (-not (Test-Path -LiteralPath $targetDirectory -PathType Leaf)) 'CargoTargetDirectory is a file.'
    $summary.build_storage = @{
        cargo_target_directory = $targetDirectory
        explicitly_selected = [bool]$CargoTargetDirectory
        directory_existed = Test-Path -LiteralPath $targetDirectory -PathType Container
        cache_reuse_allowed = [bool]$CargoTargetDirectory
        clean_environment_claim = $false
    }
    $env:CARGO_TARGET_DIR = $targetDirectory
    $null = InvokeRecorded 'fmt' $rustup @('run', $RustToolchain, 'cargo', 'fmt', '--all', '--', '--check')
    # The verified toolchain host is x64 MSVC. Native-host output preserves the
    # existing local Cargo cache layout; the bridge remains an explicit x86 target.
    $null = InvokeRecorded 'source-tests' $rustup @('run', $RustToolchain, 'cargo', 'test', '--locked', '-p', 'avelin-core', '-p', 'avelin-ui', '-p', 'avelin-app', '-p', 'avelin-cli', '--features', 'avelin-app/gui', '--', '--skip', 'manifest_validates_fifty_thousand_files_and_rejects_the_next')
    $null = InvokeRecorded 'bridge-tests' $rustup @('run', $RustToolchain, 'cargo', 'test', '--locked', '--target', 'i686-pc-windows-msvc', '-p', 'avelin-nsis-bridge')
    $null = InvokeRecorded 'host-release' $rustup @('run', $RustToolchain, 'cargo', 'build', '--locked', '--release', '-p', 'avelin-app', '-p', 'avelin-cli', '--features', 'avelin-app/gui', '--bins')
    $null = InvokeRecorded 'bridge-release' $rustup @('run', $RustToolchain, 'cargo', 'build', '--locked', '--release', '--target', 'i686-pc-windows-msvc', '-p', 'avelin-nsis-bridge')
    $hostRelease = Join-Path $env:CARGO_TARGET_DIR 'release'
    $components = [ordered]@{
        cli = Join-Path $hostRelease 'avelin-cli.exe'; runtime = Join-Path $hostRelease 'avelin-app.exe'
        sample = Join-Path $hostRelease 'nimbus-demo.exe'; bridge = Join-Path $env:CARGO_TARGET_DIR 'i686-pc-windows-msvc/release/avelin_nsis_bridge.dll'
    }
    $summary.components = @(foreach ($name in $components.Keys) { [pscustomobject]@{ name = $name; path = $components[$name]; sha256 = Hash $components[$name] } })
    $licenseDirectory = Join-Path $runRoot 'third-party'
    $null = InvokeRecorded 'collect-licenses' $python @((Join-Path $projectRoot 'scripts/collect-third-party.py'), '--project-root', $projectRoot, '--nsis-root', $nsisRootPath, '--output', $licenseDirectory)
    $null = InvokeRecorded 'verify-licenses' $python @((Join-Path $projectRoot 'scripts/collect-third-party.py'), '--verify', $licenseDirectory)
    $packageOutput = Join-Path $runRoot 'package'
    $packageText = InvokeRecorded 'package' $pwsh @('-NoProfile', '-File', (Join-Path $projectRoot 'scripts/package-devkit.ps1'), '-CliPath', $components.cli, '-RuntimePath', $components.runtime, '-BridgePath', $components.bridge, '-SamplePath', $components.sample, '-NsisRoot', $nsisRootPath, '-LicenseDirectory', $licenseDirectory, '-OutputDirectory', $packageOutput)
    $package = $packageText | ConvertFrom-Json
    $stagingKitRoot = [IO.Path]::GetFullPath([string]$package.kit_path)
    $zipPath = [IO.Path]::GetFullPath([string]$package.zip_path)
    foreach ($path in @($stagingKitRoot, $zipPath)) {
        Require ($path.StartsWith($packageOutput + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) 'Packager returned an artifact outside this run package directory.'
    }
    Require ($package.status -ceq 'PASSED') 'Packager did not report successful verification.'
    $zipHash = Hash $zipPath
    Require ($zipHash -ceq [string]$package.zip_sha256) 'ZIP bytes differ from the packager result.'
    $extractRoot = Join-Path $runRoot 'unpacked'
    Require (-not (Test-Path -LiteralPath $extractRoot)) 'ZIP extraction directory already exists; no files will be overwritten.'
    $kitDirectoryName = Split-Path -Leaf $stagingKitRoot
    $archive = [IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        $destinations = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        Require ($archive.Entries.Count -gt 0) 'ZIP is empty.'
        foreach ($entry in $archive.Entries) {
            $relative = $entry.FullName.Replace('\', '/')
            Require ($relative.StartsWith($kitDirectoryName + '/', [StringComparison]::Ordinal)) 'ZIP has an unexpected root directory.'
            Require (-not [IO.Path]::IsPathRooted($relative) -and -not $relative.Contains(':')) "Invalid ZIP path: $relative"
            foreach ($segment in $relative.Split('/')) {
                Require ($segment -notin @('.', '..') -and -not $segment.EndsWith('.') -and -not $segment.EndsWith(' ')) "Ambiguous ZIP path: $relative"
            }
            $destination = [IO.Path]::GetFullPath((Join-Path $extractRoot $relative)).TrimEnd([char]'\', [char]'/')
            Require ($destination.StartsWith($extractRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) 'ZIP entry escaped its extraction directory.'
            Require ($destinations.Add($destination)) "Duplicate ZIP destination: $relative"
            Require ((($entry.ExternalAttributes -shr 16) -band 0xf000) -ne 0xa000) "ZIP symbolic link is unsupported: $relative"
        }
        $entryCount = $archive.Entries.Count
    } finally { $archive.Dispose() }
    $null = New-Item -ItemType Directory -Path $extractRoot
    [IO.Compression.ZipFile]::ExtractToDirectory($zipPath, $extractRoot, $false)
    Require ((Hash $zipPath) -ceq $zipHash) 'ZIP changed during extraction.'
    $kitRoot = Join-Path $extractRoot $kitDirectoryName
    $summary.extraction = @{ zip_path = $zipPath; zip_sha256 = $zipHash; destination = $extractRoot; kit_root = $kitRoot; entries = $entryCount; overwrite = $false }
    Save (Join-Path $runRoot 'extraction.json') $summary.extraction
    $verification = (InvokeRecorded 'verify-kit' $pwsh @('-NoProfile', '-File', (Join-Path $kitRoot 'scripts/Verify-DevKit.ps1'), '-KitRoot', $kitRoot)) | ConvertFrom-Json
    Require ($verification.status -in @('PASS', 'PASSED') -and $verification.checked_files -gt 0) 'Extracted kit did not report successful file verification.'
    $relativeComponents = @{ cli = 'avelin-cli.exe'; runtime = 'avelin-app.exe'; bridge = 'avelin_nsis_bridge.dll'; sample = 'sample/nimbus-demo.exe' }
    foreach ($name in $components.Keys) {
        Require ((Hash (Join-Path $kitRoot $relativeComponents[$name])) -ceq (Hash $components[$name])) "Extracted component differs from this source build: $name"
    }
    $harnessDirectory = Join-Path $runRoot 'normal-cycle'
    $null = InvokeRecorded 'normal-cycle' $pwsh @('-NoProfile', '-File', (Join-Path $kitRoot 'tests/harness/run-m4.ps1'), '-KitRoot', $kitRoot, '-OutputDirectory', $harnessDirectory)
    $harnessSummary = ReadJson (Join-Path $harnessDirectory 'summary.json')
    Require ($harnessSummary.status -in @('PASS', 'PASSED')) 'Normal-cycle harness did not save a passing summary.'
    Require ((Hash (Join-Path $projectRoot 'Cargo.lock')) -ceq $summary.cargo_lock_sha256) 'Cargo.lock changed during validation.'
    $finalInventory = @(Inventory $nsisRootPath)
    Require (($finalInventory | ConvertTo-Json -Depth 5 -Compress) -ceq ($nsisInventory | ConvertTo-Json -Depth 5 -Compress)) 'NSIS source tree changed during validation.'
    $summary.package = $package
    $summary.harness_summary = Join-Path $harnessDirectory 'summary.json'
    $summary.harness_summary_sha256 = Hash $summary.harness_summary
    $summary.status = 'PASSED'
} catch {
    $summary.status = 'FAILED'; $summary.failure = $_.Exception.Message
    throw
} finally {
    $env:RUSTUP_TOOLCHAIN = $originalToolchain
    $env:CARGO_TARGET_DIR = $originalCargoTarget
    $summary.finished_at_utc = [DateTime]::UtcNow.ToString('o')
    Save (Join-Path $runRoot 'summary.json') $summary
}
Write-Output ($summary | ConvertTo-Json -Depth 30)
