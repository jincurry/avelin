#requires -Version 7.2
<#
.SYNOPSIS
Build, package and validate the Rust engine on a provisioned Windows host.
.DESCRIPTION
Uses installed tools only; never installs Rust, targets, NSIS, Python or SDKs.
Cargo may fetch dependencies selected by Cargo.lock. Builds current components,
packages and extracts a Rust devkit, then runs all 19 lifecycle and 6 interruption
cases against those extracted binaries. NSIS and the x86 bridge are required for
the migration fixtures and shared license collector, not Rust installation.
OutputDirectory must be a new directory under this repository's target tree.
Interrupted scenes, user-scoped test registrations and private evidence remain
available for inspection; this entry never recursively deletes them. Interruption
termination targets only session-bound process identities in the test harness.
No GUI automation, performance measurement, signing or tool installation.
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
$targetRoot = Join-Path $projectRoot 'target'
if (-not $runRoot.StartsWith($targetRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'OutputDirectory must be a new directory below the repository target directory.'
}
for ($ancestor = $runRoot; $ancestor; $ancestor = Split-Path -Parent $ancestor) {
    if ((Test-Path -LiteralPath $ancestor) -and (((Get-Item -LiteralPath $ancestor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "OutputDirectory cannot traverse a reparse point: $ancestor"
    }
}
if (Test-Path -LiteralPath $runRoot) { throw "OutputDirectory already exists; choose a new evidence directory: $runRoot" }
if (-not $IsWindows -or -not [Environment]::Is64BitProcess) { throw 'This entry requires Windows x64 PowerShell 7.2+.' }
$null = New-Item -ItemType Directory -Path $runRoot
$logRoot = Join-Path $runRoot 'logs'
$null = New-Item -ItemType Directory -Path $logRoot
$summary = [ordered]@{
    schema_version = 1; status = 'RUNNING'; engine = 'rust'
    started_at_utc = [DateTime]::UtcNow.ToString('o'); project_root = $projectRoot
    output_directory = $runRoot; source_build = $true; cloud_execution_claim = 'NOT_ASSERTED'
    excluded = @('50000-file boundary test', 'performance', 'signing', 'arbitrary instruction or reboot interruption', 'independent environment qualification', 'native GUI/DPI/theme/Narrator matrix')
    retained_evidence = 'Interrupted fixtures and their user-scoped registrations are deliberately retained; inspect the harness summaries before cleanup.'
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
function ReadUserShellFolders {
    foreach ($folder in @([Environment+SpecialFolder]::DesktopDirectory, [Environment+SpecialFolder]::Programs)) {
        $verified = [Environment]::GetFolderPath($folder)
        $unverified = [Environment]::GetFolderPath($folder, [Environment+SpecialFolderOption]::DoNotVerify)
        [ordered]@{
            folder = $folder.ToString()
            default_nonempty = -not [string]::IsNullOrEmpty($verified)
            unverified_absolute = [IO.Path]::IsPathFullyQualified($unverified)
            exists = [IO.Directory]::Exists($unverified)
        }
    }
}

try {
    # Real lifecycle fixtures create current-user shortcuts. Check the existing
    # profile before the expensive build; do not create folders or expose paths.
    $summary.user_shell_folders = @(ReadUserShellFolders)
    Require (@($summary.user_shell_folders | Where-Object { -not ($_.default_nonempty -and $_.unverified_absolute -and $_.exists) }).Count -eq 0) 'Required current-user Desktop or Programs folder is unavailable; provision the Windows user profile before running lifecycle acceptance.'
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
    $harnessSources = @('tests/harness/run-rust-engine.ps1', 'tests/harness/run-rust-engine-interruption.ps1', 'tests/harness/windows-shell-link.ps1')
    $summary.harness_sources = @(foreach ($relative in $harnessSources) {
        @{ path = $relative; sha256 = Hash (Join-Path $projectRoot $relative) }
    })
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
    $null = InvokeRecorded 'source-tests' $rustup @('run', $RustToolchain, 'cargo', 'test', '--locked', '-p', 'avelin-core', '-p', 'avelin-ui', '-p', 'avelin-app', '-p', 'avelin-cli', '-p', 'avelin-engine', '--features', 'avelin-app/gui', '--', '--skip', 'manifest_validates_fifty_thousand_files_and_rejects_the_next')
    $null = InvokeRecorded 'bridge-tests' $rustup @('run', $RustToolchain, 'cargo', 'test', '--locked', '--target', 'i686-pc-windows-msvc', '-p', 'avelin-nsis-bridge')
    $null = InvokeRecorded 'host-release' $rustup @('run', $RustToolchain, 'cargo', 'build', '--locked', '--release', '-p', 'avelin-app', '-p', 'avelin-cli', '-p', 'avelin-engine', '--features', 'avelin-app/gui', '--bins')
    $null = InvokeRecorded 'bridge-release' $rustup @('run', $RustToolchain, 'cargo', 'build', '--locked', '--release', '--target', 'i686-pc-windows-msvc', '-p', 'avelin-nsis-bridge')
    $hostRelease = Join-Path $env:CARGO_TARGET_DIR 'release'
    $components = [ordered]@{
        cli = Join-Path $hostRelease 'avelin-cli.exe'; runtime = Join-Path $hostRelease 'avelin-app.exe'
        rust_worker = Join-Path $hostRelease 'avelin-engine.exe'; sample = Join-Path $hostRelease 'nimbus-demo.exe'
        bridge = Join-Path $env:CARGO_TARGET_DIR 'i686-pc-windows-msvc/release/avelin_nsis_bridge.dll'
    }
    $summary.components = @(foreach ($name in $components.Keys) { [pscustomobject]@{ name = $name; path = $components[$name]; sha256 = Hash $components[$name] } })
    $licenseDirectory = Join-Path $runRoot 'third-party'
    $null = InvokeRecorded 'collect-licenses' $python @((Join-Path $projectRoot 'scripts/collect-third-party.py'), '--project-root', $projectRoot, '--nsis-root', $nsisRootPath, '--output', $licenseDirectory)
    $null = InvokeRecorded 'verify-licenses' $python @((Join-Path $projectRoot 'scripts/collect-third-party.py'), '--verify', $licenseDirectory)
    $packageOutput = Join-Path $runRoot 'package'
    $null = InvokeRecorded 'package' $pwsh @('-NoProfile', '-File', (Join-Path $projectRoot 'scripts/package-rust-devkit.ps1'), '-CliPath', $components.cli, '-RuntimePath', $components.runtime, '-RustWorkerPath', $components.rust_worker, '-SamplePath', $components.sample, '-LicenseDirectory', $licenseDirectory, '-OutputDirectory', $packageOutput)
    $package = ReadJson (Join-Path $packageOutput 'packaging-verification.json')
    $stagingKitRoot = [IO.Path]::GetFullPath([string]$package.kit_path)
    $zipPath = [IO.Path]::GetFullPath([string]$package.zip_path)
    foreach ($path in @($stagingKitRoot, $zipPath)) {
        Require ($path.StartsWith($packageOutput + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) 'Packager returned an artifact outside this run package directory.'
    }
    Require ($package.verified -eq $true -and $package.delivered_files -gt 0) 'Packager did not report successful verification.'
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
    $verification = (InvokeRecorded 'verify-extracted-kit' $pwsh @('-NoProfile', '-File', (Join-Path $kitRoot 'Verify-RustDevKit.ps1'), '-KitDirectory', $kitRoot, '-ZipPath', $zipPath)) | ConvertFrom-Json
    Require ($verification.verified -eq $true) 'Extracted kit did not report successful file verification.'
    Save (Join-Path $runRoot 'extracted-kit-verification.json') $verification
    $relativeComponents = [ordered]@{
        cli = 'bin/avelin-cli.exe'; runtime = 'bin/avelin-app.exe'
        rust_worker = 'bin/avelin-engine.exe'; sample = 'sample/payload/nimbus-demo.exe'
    }
    $extractedComponents = [ordered]@{}
    foreach ($name in $relativeComponents.Keys) {
        $extractedComponents[$name] = Join-Path $kitRoot $relativeComponents[$name]
        Require ((Hash $extractedComponents[$name]) -ceq (Hash $components[$name])) "Extracted component differs from this source build: $name"
    }
    $summary.extracted_components = @(foreach ($name in $extractedComponents.Keys) {
        @{ name = $name; path = $extractedComponents[$name]; sha256 = Hash $extractedComponents[$name] }
    })
    $sampleSetup = Join-Path $kitRoot 'sample/Nimbus-Rust-Setup.exe'
    $summary.packaged_sample = @{
        setup = $sampleSetup; setup_sha256 = Hash $sampleSetup
        payload_sha256 = Hash $extractedComponents.sample
        verification = 'ZIP and extracted contents verified; lifecycle fixtures use these exact extracted runtime, worker, CLI and sample bytes.'
    }
    $lifecycleDirectory = Join-Path $runRoot 'lifecycle'
    $null = InvokeRecorded 'rust-lifecycle' $pwsh @('-NoProfile', '-File', (Join-Path $projectRoot 'tests/harness/run-rust-engine.ps1'), '-Runtime', $extractedComponents.runtime, '-Cli', $extractedComponents.cli, '-RustWorker', $extractedComponents.rust_worker, '-Sample', $extractedComponents.sample, '-Nsis', $makensis, '-Bridge', $components.bridge, '-OutputRoot', $lifecycleDirectory)
    $lifecycleReport = Join-Path $lifecycleDirectory 'summary.json'
    $lifecycle = ReadJson $lifecycleReport
    Require ($lifecycle.status -ceq 'PASSED' -and $lifecycle.migration_requested -eq $true) 'Rust lifecycle did not pass with NSIS migration enabled.'
    $expectedLifecycle = @(
        'install-collision', 'cancel-install', 'fail-install', 'install-a', 'same-version',
        'upgrade-collision', 'modified-replacement', 'modified-selected-shortcut',
        'upgrade-before-commit-restores-a', 'upgrade-b', 'launch-b', 'downgrade',
        'uninstall-b-preserves-user-edits', 'reinstall-collision', 'reinstall-and-direct-uninstall',
        'nsis-to-rust-before-commit-restores-nsis', 'nsis-to-rust-normal-migration',
        'post-commit-confirmation-loss-preserves-committed-b', 'incomplete-rollback-diagnosis-and-reentry-refusal'
    )
    Require (@($lifecycle.completed_cases).Count -eq $expectedLifecycle.Count) 'Lifecycle case count differs from the required 19 cases.'
    foreach ($case in $expectedLifecycle) {
        Require (@($lifecycle.completed_cases | Where-Object { $_ -ceq $case }).Count -eq 1) "Lifecycle case absent or duplicated: $case"
    }
    # The lifecycle harness copies inputs into its fixture. Bind those bytes back
    # to the extracted ZIP components rather than trusting a successful exit alone.
    $lifecycleComponents = [ordered]@{}
    foreach ($name in $extractedComponents.Keys) {
        $fixtureBinary = Join-Path $lifecycleDirectory ('tools/' + (Split-Path -Leaf $extractedComponents[$name]))
        Require ((Hash $fixtureBinary) -ceq (Hash $extractedComponents[$name])) "Lifecycle used a different component: $name"
        $lifecycleComponents[$name] = @{ path = $fixtureBinary; sha256 = Hash $fixtureBinary }
    }
    Require ((Hash (Join-Path $lifecycleDirectory 'tools/avelin_nsis_bridge.dll')) -ceq (Hash $components.bridge)) 'Migration bridge differs from this source build.'
    $summary.lifecycle = @{
        status = $lifecycle.status; case_count = @($lifecycle.completed_cases).Count
        report = $lifecycleReport; report_sha256 = Hash $lifecycleReport
        components = $lifecycleComponents; migration_required = $true
    }
    $interruptionDirectory = Join-Path $runRoot 'interruption'
    $null = InvokeRecorded 'rust-interruption' $pwsh @('-NoProfile', '-File', (Join-Path $projectRoot 'tests/harness/run-rust-engine-interruption.ps1'), '-Runtime', $extractedComponents.runtime, '-Cli', $extractedComponents.cli, '-RustWorker', $extractedComponents.rust_worker, '-Sample', $extractedComponents.sample, '-OutputRoot', $interruptionDirectory)
    $interruptionReport = Join-Path $interruptionDirectory 'summary.json'
    $interruption = ReadJson $interruptionReport
    Require ($interruption.status -ceq 'PASSED' -and @($interruption.cases).Count -eq 6) 'Rust interruption did not pass all 6 required cases.'
    foreach ($point in @('prepared', 'after-first-publish', 'after-commit')) {
        foreach ($victim in @('backend', 'coordinator')) {
            $label = "$point-kill-$victim"
            $case = @($interruption.cases | Where-Object { $_.label -ceq $label })
            Require ($case.Count -eq 1 -and $case[0].status -ceq 'PASSED') "Interruption case absent, duplicated or failed: $label"
        }
    }
    Require (@($interruption.components).Count -eq $extractedComponents.Count) 'Interruption component count differs.'
    foreach ($name in $extractedComponents.Keys) {
        $record = @($interruption.components | Where-Object { [string]::Equals([string]$_.path, $extractedComponents[$name], [StringComparison]::OrdinalIgnoreCase) })
        Require ($record.Count -eq 1 -and $record[0].sha256 -ceq (Hash $extractedComponents[$name])) "Interruption component does not match the extracted ZIP: $name"
    }
    $summary.interruption = @{
        status = $interruption.status; case_count = @($interruption.cases).Count
        report = $interruptionReport; report_sha256 = Hash $interruptionReport
        retained_scenes = $interruption.retained_scenes
    }
    # Make sure validation left the distributable and extracted inputs unchanged.
    $finalVerification = (InvokeRecorded 'verify-kit-after-acceptance' $pwsh @('-NoProfile', '-File', (Join-Path $kitRoot 'Verify-RustDevKit.ps1'), '-KitDirectory', $kitRoot, '-ZipPath', $zipPath)) | ConvertFrom-Json
    Require ($finalVerification.verified -eq $true -and (Hash $zipPath) -ceq $zipHash) 'Package changed during acceptance.'
    foreach ($record in $summary.components) {
        Require ((Hash $record.path) -ceq $record.sha256) "Source build output changed during validation: $($record.name)"
    }
    Require ((Hash (Join-Path $projectRoot 'Cargo.lock')) -ceq $summary.cargo_lock_sha256) 'Cargo.lock changed during validation.'
    foreach ($record in $summary.harness_sources) {
        Require ((Hash (Join-Path $projectRoot $record.path)) -ceq $record.sha256) 'Acceptance harness changed during validation.'
    }
    $finalInventory = @(Inventory $nsisRootPath)
    Require (($finalInventory | ConvertTo-Json -Depth 5 -Compress) -ceq ($nsisInventory | ConvertTo-Json -Depth 5 -Compress)) 'NSIS source tree changed during validation.'
    $summary.package = $package
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
