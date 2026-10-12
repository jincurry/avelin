<#
.SYNOPSIS
Build the unsigned Avelin M0 Nimbus installer without running it.
.DESCRIPTION
Requires Windows x64, an already installed stable MSVC Rust toolchain with the
x86_64-pc-windows-msvc and i686-pc-windows-msvc targets, MSVC x86/x64 build tools,
a Windows SDK, and NSIS. Uses Cargo.lock and the selected build profile. Cargo may download
locked crate dependencies; this script never installs or updates tools/targets.

Builds core, CLI, nimbus-demo, the selected bare app runtime, and the x86 NSIS
bridge. Refreshes only examples/nimbus/payload/nimbus-demo.exe from that build.
The default runtime is headless in target/<profile>. -Gui builds the GUI runtime
in target/ui-build/<profile>, preserving the headless target directory's app
binary. Debug is the default profile; Release uses Cargo's release profile.

Relative -Output and -Nsis paths are resolved against the repository root.
Setup, backend, manifest and build-record output files must not already exist.
Build staging directories are retained for inspection. No installer is launched,
no signing is performed, and no file or directory is recursively removed.
The installed stable version is recorded; this is not a byte-reproducible build.
.PARAMETER Gui
Use the native GPUI app runtime. May take longer on the first build.
.PARAMETER Profile
Cargo build profile: Debug (default) or Release. All Rust artifacts, including
the x86 bridge, use the selected profile.
.PARAMETER Output
New .exe output file. Defaults to target/m0-build/<UTC timestamp>-<unique suffix>/
Nimbus-M0-Setup.exe for Debug or target/m0-release/<UTC timestamp>-<unique suffix>/
Nimbus-M0-Setup.exe for Release. Existing files and sidecars are never overwritten.
.PARAMETER Nsis
Path to makensis.exe. Defaults to NSIS_MAKENSIS, PATH, then standard NSIS folders.
.PARAMETER Help
Show this help without checking tools or building anything.
.EXAMPLE
pwsh -File scripts/build-m0.ps1
.EXAMPLE
pwsh -File scripts/build-m0.ps1 -Gui
.EXAMPLE
pwsh -File scripts/build-m0.ps1 -Profile Release
.EXAMPLE
pwsh -File scripts/build-m0.ps1 -Profile Release -Gui
.EXAMPLE
pwsh -File scripts/build-m0.ps1 -Gui -Output target/my-build/Nimbus-Setup.exe
.EXAMPLE
pwsh -File scripts/build-m0.ps1 -Nsis 'C:\Program Files (x86)\NSIS\makensis.exe'
#>
[CmdletBinding()]
param(
    [switch]$Gui,
    [string]$Output,
    [string]$Nsis,
    [ValidateSet('Debug', 'Release')][string]$Profile = 'Debug',
    [Alias('h')][switch]$Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($Help) { Get-Help $PSCommandPath -Full; return }

$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$hostTarget = 'x86_64-pc-windows-msvc'
$bridgeTarget = 'i686-pc-windows-msvc'
$startedAt = [DateTime]::UtcNow
$profileDirectory = $Profile.ToLowerInvariant()
$profileArguments = @()
if ($Profile -eq 'Release') { $profileArguments += '--release' }

function Get-ApplicationPath([string]$Name) {
    $found = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($found) { return $found.Source }
    return $null
}

function Resolve-ProjectPath([string]$Path) {
    if ([IO.Path]::IsPathRooted($Path)) { return [IO.Path]::GetFullPath($Path) }
    return [IO.Path]::GetFullPath((Join-Path $projectRoot $Path))
}

function Invoke-Checked([string]$Executable, [string[]]$Arguments) {
    Write-Host ("Building: {0} {1}" -f $Executable, ($Arguments -join ' '))
    & $Executable @Arguments
    if ($LASTEXITCODE -ne 0) { throw "Command failed with exit code ${LASTEXITCODE}: $Executable" }
}

function Read-Probe([string]$Executable, [string[]]$Arguments) {
    $lines = @(& $Executable @Arguments 2>&1)
    if ($LASTEXITCODE -ne 0) { throw "Tool check failed: $Executable $($Arguments -join ' ')`n$($lines -join "`n")" }
    return ($lines -join [Environment]::NewLine).Trim()
}

function Assert-NewOutputs([string[]]$Paths) {
    foreach ($path in $Paths) {
        if (Test-Path -LiteralPath $path) { throw "Output already exists; choose a new -Output path: $path" }
    }
}

function Assert-PeMachine([string]$Path, [uint16]$Expected) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Build artifact is missing: $Path" }
    $stream = [IO.File]::OpenRead($Path)
    $reader = New-Object IO.BinaryReader($stream)
    try {
        if ($reader.ReadUInt16() -ne 0x5a4d) { throw "Artifact has no MZ header: $Path" }
        $stream.Position = 0x3c
        $peOffset = $reader.ReadUInt32()
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne $Expected) {
            throw "Artifact has the wrong PE architecture: $Path"
        }
    }
    finally { $reader.Dispose(); $stream.Dispose() }
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT -or -not [Environment]::Is64BitOperatingSystem) {
    throw 'This M0 build script requires Windows x64.'
}
foreach ($requiredFile in @('Cargo.toml', 'Cargo.lock', 'examples/nimbus/avelin.toml')) {
    if (-not (Test-Path -LiteralPath (Join-Path $projectRoot $requiredFile) -PathType Leaf)) {
        throw "Required project file is missing: $requiredFile"
    }
}
# A build.target override changes artifact paths. Refuse that ambiguity instead
# of accidentally packaging a stale binary from the normal host directory.
if ($env:CARGO_BUILD_TARGET) { throw 'Unset CARGO_BUILD_TARGET before using this script; it builds the MSVC x64 host plus an explicit x86 bridge.' }

if (-not $Output) {
    $buildId = $startedAt.ToString('yyyyMMddTHHmmssfffZ') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
    $outputRoot = if ($Profile -eq 'Release') { 'target/m0-release' } else { 'target/m0-build' }
    $Output = Join-Path "$outputRoot/$buildId" 'Nimbus-M0-Setup.exe'
}
$outputPath = Resolve-ProjectPath $Output
if ([IO.Path]::GetExtension($outputPath) -ine '.exe') { throw '-Output must name a new .exe file.' }
$recordPath = [IO.Path]::ChangeExtension($outputPath, 'build.json')
$backendPath = [IO.Path]::ChangeExtension($outputPath, 'backend.exe')
$manifestPath = [IO.Path]::ChangeExtension($outputPath, 'manifest.json')
$outputFiles = @($outputPath, $backendPath, $manifestPath, $recordPath)
Assert-NewOutputs $outputFiles

$cargoPath = Get-ApplicationPath 'cargo.exe'
$rustupPath = Get-ApplicationPath 'rustup.exe'
if (-not $cargoPath -or -not $rustupPath) { throw 'cargo.exe and rustup.exe must already be installed and available on PATH.' }
$toolchains = Read-Probe $rustupPath @('toolchain', 'list')
if (-not ($toolchains -split '\r?\n' | Where-Object { ($_ -split '\s+')[0] -eq "stable-$hostTarget" })) {
    throw "The stable-$hostTarget toolchain is not installed. No installation was attempted."
}
$rustVersion = Read-Probe $rustupPath @('run', 'stable', 'rustc', '--version', '--verbose')
if ($rustVersion -notmatch '(?m)^host: x86_64-pc-windows-msvc\r?$') { throw 'The installed stable compiler must have the x86_64-pc-windows-msvc host.' }
$installedTargets = (Read-Probe $rustupPath @('target', 'list', '--installed', '--toolchain', 'stable')) -split '\r?\n'
foreach ($target in @($hostTarget, $bridgeTarget)) {
    if ($installedTargets -notcontains $target) { throw "Rust target $target is not installed for stable. No installation was attempted." }
}
$cargoVersion = Read-Probe $cargoPath @('+stable', '--version')

$vswherePath = Get-ApplicationPath 'vswhere.exe'
if (-not $vswherePath -and ${env:ProgramFiles(x86)}) {
    $candidate = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
    if (Test-Path -LiteralPath $candidate -PathType Leaf) { $vswherePath = $candidate }
}
if (-not $vswherePath) { throw 'vswhere.exe was not found; MSVC x86/x64 build tools cannot be confirmed. See scripts/doctor.ps1.' }
$msvcPath = Read-Probe $vswherePath @('-latest', '-products', '*', '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64', '-property', 'installationPath')
if (-not $msvcPath) { throw 'MSVC x86/x64 build tools were not found. See scripts/doctor.ps1; no tools were installed.' }
$sdkFound = $false
foreach ($sdkKey in @('HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots', 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots')) {
    if (Test-Path -LiteralPath $sdkKey) {
        $sdkRegistration = Get-ItemProperty -LiteralPath $sdkKey -Name KitsRoot10 -ErrorAction SilentlyContinue
        if ($sdkRegistration) {
            $sdkRoot = $sdkRegistration.KitsRoot10
            if ($sdkRoot -and (Test-Path -LiteralPath (Join-Path $sdkRoot 'Lib') -PathType Container)) { $sdkFound = $true; break }
        }
    }
}
if (-not $sdkFound) { throw 'A Windows 10/11 SDK library directory was not found. See scripts/doctor.ps1.' }

if (-not $Nsis) { $Nsis = $env:NSIS_MAKENSIS }
if (-not $Nsis) { $Nsis = Get-ApplicationPath 'makensis.exe' }
if (-not $Nsis) {
    foreach ($programRoot in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
        if ($programRoot) {
            $candidate = Join-Path $programRoot 'NSIS/makensis.exe'
            if (Test-Path -LiteralPath $candidate -PathType Leaf) { $Nsis = $candidate; break }
        }
    }
}
if (-not $Nsis) { throw 'NSIS makensis.exe was not found. Supply -Nsis; no tools were installed.' }
$nsisPath = Resolve-ProjectPath $Nsis
if (-not (Test-Path -LiteralPath $nsisPath -PathType Leaf)) { throw "NSIS compiler does not exist: $nsisPath" }
$nsisVersion = Read-Probe $nsisPath @('/VERSION')

$defaultTargetDir = Join-Path $projectRoot 'target'
$runtimeTargetDir = if ($Gui) { Join-Path $defaultTargetDir 'ui-build' } else { $defaultTargetDir }
$cliPath = Join-Path $defaultTargetDir "$profileDirectory/avelin-cli.exe"
$samplePath = Join-Path $defaultTargetDir "$profileDirectory/nimbus-demo.exe"
$runtimePath = Join-Path $runtimeTargetDir "$profileDirectory/avelin-app.exe"
$bridgePath = Join-Path $defaultTargetDir "$bridgeTarget/$profileDirectory/avelin_nsis_bridge.dll"
$payloadPath = Join-Path $projectRoot 'examples/nimbus/payload/nimbus-demo.exe'
$configPath = Join-Path $projectRoot 'examples/nimbus/avelin.toml'

Push-Location $projectRoot
try {
    Invoke-Checked $cargoPath (@('+stable', 'build', '--locked', '-p', 'avelin-core', '-p', 'avelin-cli', '--target-dir', $defaultTargetDir) + $profileArguments)
    $runtimeArguments = @('+stable', 'build', '--locked', '-p', 'avelin-app', '--bin', 'avelin-app', '--no-default-features', '--target-dir', $runtimeTargetDir) + $profileArguments
    if ($Gui) { $runtimeArguments += @('--features', 'gui') }
    Invoke-Checked $cargoPath $runtimeArguments
    Invoke-Checked $cargoPath (@('+stable', 'build', '--locked', '-p', 'avelin-nsis-bridge', '--lib', '--target', $bridgeTarget, '--target-dir', $defaultTargetDir) + $profileArguments)
    Assert-PeMachine $cliPath 0x8664
    Assert-PeMachine $samplePath 0x8664
    Assert-PeMachine $runtimePath 0x8664
    Assert-PeMachine $bridgePath 0x014c

    foreach ($payloadItem in @((Split-Path -Parent $payloadPath), $payloadPath)) {
        if ((Test-Path -LiteralPath $payloadItem) -and ((Get-Item -LiteralPath $payloadItem).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Refusing to copy the demo through a reparse point: $payloadItem"
        }
    }
    Copy-Item -LiteralPath $samplePath -Destination $payloadPath -Force
    Assert-NewOutputs $outputFiles
    Invoke-Checked $cliPath @('build', '--config', $configPath, '--runtime', $runtimePath, '--bridge', $bridgePath, '--nsis', $nsisPath, '--output', $outputPath)

    $record = [ordered]@{
        schema_version = 1
        started_at_utc = $startedAt.ToString('o')
        finished_at_utc = [DateTime]::UtcNow.ToString('o')
        profile = $profileDirectory
        gui = [bool]$Gui
        target_directory = $defaultTargetDir
        runtime_target_directory = $runtimeTargetDir
        powershell = $PSVersionTable.PSVersion.ToString()
        rust = $rustVersion
        cargo = $cargoVersion
        cargo_path = $cargoPath
        nsis = $nsisVersion
        nsis_path = $nsisPath
        msvc_installation_path = $msvcPath
        windows_sdk_root = $sdkRoot
        build_script_sha256 = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash
        cargo_lock_sha256 = (Get-FileHash -LiteralPath (Join-Path $projectRoot 'Cargo.lock') -Algorithm SHA256).Hash
        config_sha256 = (Get-FileHash -LiteralPath $configPath -Algorithm SHA256).Hash
        cli = $cliPath
        cli_bytes = (Get-Item -LiteralPath $cliPath).Length
        cli_sha256 = (Get-FileHash -LiteralPath $cliPath -Algorithm SHA256).Hash
        demo = $samplePath
        demo_bytes = (Get-Item -LiteralPath $samplePath).Length
        demo_sha256 = (Get-FileHash -LiteralPath $samplePath -Algorithm SHA256).Hash
        runtime = $runtimePath
        runtime_bytes = (Get-Item -LiteralPath $runtimePath).Length
        runtime_sha256 = (Get-FileHash -LiteralPath $runtimePath -Algorithm SHA256).Hash
        bridge = $bridgePath
        bridge_bytes = (Get-Item -LiteralPath $bridgePath).Length
        bridge_sha256 = (Get-FileHash -LiteralPath $bridgePath -Algorithm SHA256).Hash
        backend = $backendPath
        backend_bytes = (Get-Item -LiteralPath $backendPath).Length
        backend_sha256 = (Get-FileHash -LiteralPath $backendPath -Algorithm SHA256).Hash
        manifest = $manifestPath
        manifest_sha256 = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash
        setup = $outputPath
        setup_bytes = (Get-Item -LiteralPath $outputPath).Length
        setup_sha256 = (Get-FileHash -LiteralPath $outputPath -Algorithm SHA256).Hash
        signed = $false
        installer_executed = $false
    }
    $recordJson = $record | ConvertTo-Json -Depth 4
    $recordStream = [IO.File]::Open($recordPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
    $recordWriter = New-Object IO.StreamWriter($recordStream, (New-Object Text.UTF8Encoding($false)))
    try { $recordWriter.WriteLine($recordJson) } finally { $recordWriter.Dispose(); $recordStream.Dispose() }
    Write-Output "Built unsigned M0 installer: $outputPath"
    Write-Output "Build record: $recordPath"
}
finally { Pop-Location }
