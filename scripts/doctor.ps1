#requires -Version 7.2
[CmdletBinding()]
param(
    [switch]$Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This environment inventory requires Windows and PowerShell 7.2+.' }

function Get-CommandPath {
    param([string]$Name)
    $command = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($command) { return $command.Source }
    return $null
}

function Invoke-ReadOnlyProbe {
    param([string]$Executable, [string[]]$Arguments)
    if (-not $Executable) {
        return [pscustomobject]@{ available = $false; exit_code = $null; output = $null }
    }
    try {
        $probeStart = New-Object System.Diagnostics.ProcessStartInfo
        $probeStart.FileName = $Executable
        $probeStart.UseShellExecute = $false
        $probeStart.CreateNoWindow = $true
        $probeStart.RedirectStandardOutput = $true
        $probeStart.RedirectStandardError = $true
        foreach ($argument in $Arguments) { $probeStart.ArgumentList.Add($argument) }
        $probeProcess = New-Object System.Diagnostics.Process
        $probeProcess.StartInfo = $probeStart
        $null = $probeProcess.Start()
        $probeStdout = $probeProcess.StandardOutput.ReadToEndAsync()
        $probeStderr = $probeProcess.StandardError.ReadToEndAsync()
        if (-not $probeProcess.WaitForExit(10000)) {
            # Only this read-only version probe is stopped, never an installer.
            $probeProcess.Kill()
            $probeProcess.WaitForExit()
            $probeProcess.Dispose()
            return [pscustomobject]@{ available = $true; exit_code = -2; output = 'Read-only probe timed out after 10 seconds; version remains unverified.' }
        }
        $probeOutput = @($probeStdout.GetAwaiter().GetResult(), $probeStderr.GetAwaiter().GetResult())
        $probeExit = $probeProcess.ExitCode
        $probeProcess.Dispose()
        return [pscustomobject]@{
            available = $true
            exit_code = $probeExit
            output = ($probeOutput -join [Environment]::NewLine).Trim()
        }
    }
    catch {
        return [pscustomobject]@{ available = $true; exit_code = -1; output = $_.Exception.Message }
    }
}

function Get-FileVersion {
    param([string]$Path)
    if ($Path -and (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return (Get-Item -LiteralPath $Path).VersionInfo.FileVersion
    }
    return $null
}

$rustupPath = Get-CommandPath 'rustup.exe'
$rustcPath = Get-CommandPath 'rustc.exe'
$cargoPath = Get-CommandPath 'cargo.exe'
$nsisPath = Get-CommandPath 'makensis.exe'

if (-not $nsisPath) {
    foreach ($programRoot in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
        if ($programRoot) {
            $nsisCandidate = Join-Path $programRoot 'NSIS\makensis.exe'
            if (Test-Path -LiteralPath $nsisCandidate -PathType Leaf) {
                $nsisPath = $nsisCandidate
                break
            }
        }
    }
}

# Selecting an already installed toolchain avoids a rustup proxy downloading a
# toolchain requested by a newly created rust-toolchain.toml in this directory.
$toolchainProbe = Invoke-ReadOnlyProbe $rustupPath @('toolchain', 'list')
$installedToolchains = @()
$selectedToolchain = $null
if ($toolchainProbe.available -and $toolchainProbe.exit_code -eq 0) {
    $toolchainLines = @($toolchainProbe.output -split '\r?\n' |
        Where-Object { $_ -match '^\S+' -and $_ -notmatch '^no installed toolchains' })
    $installedToolchains = @($toolchainLines | ForEach-Object { ($_ -split '\s+')[0] })
    $selectedLine = $toolchainLines | Where-Object { $_ -match '\(.*active.*\)' } |
        Select-Object -First 1
    if (-not $selectedLine) {
        $selectedLine = $toolchainLines | Where-Object { $_ -match '\(.*default.*\)' } |
            Select-Object -First 1
    }
    if (-not $selectedLine) { $selectedLine = $toolchainLines | Select-Object -First 1 }
    if ($selectedLine) { $selectedToolchain = ($selectedLine -split '\s+')[0] }
}

$rustupDirectory = $env:RUSTUP_HOME
if (-not $rustupDirectory) { $rustupDirectory = Join-Path $env:USERPROFILE '.rustup' }
$toolchainDirectory = Join-Path $rustupDirectory 'toolchains'
$diskToolchains = @()
if (Test-Path -LiteralPath $toolchainDirectory -PathType Container) {
    $diskToolchains = @(Get-ChildItem -LiteralPath $toolchainDirectory -Directory |
        Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'bin\rustc.exe') -PathType Leaf })
}
if (-not $selectedToolchain -and $diskToolchains.Count -gt 0) {
    $preferredToolchain = $diskToolchains | Where-Object { $_.Name -eq 'stable-x86_64-pc-windows-msvc' } | Select-Object -First 1
    if (-not $preferredToolchain) { $preferredToolchain = $diskToolchains | Select-Object -First 1 }
    $selectedToolchain = $preferredToolchain.Name
    $installedToolchains = @($diskToolchains.Name)
}

$selectedToolchainDirectory = $null
if ($selectedToolchain) { $selectedToolchainDirectory = Join-Path $toolchainDirectory $selectedToolchain }
if ($selectedToolchainDirectory -and (Test-Path -LiteralPath (Join-Path $selectedToolchainDirectory 'bin\rustc.exe') -PathType Leaf)) {
    $rustcProbe = Invoke-ReadOnlyProbe (Join-Path $selectedToolchainDirectory 'bin\rustc.exe') @('--version', '--verbose')
    $cargoProbe = Invoke-ReadOnlyProbe (Join-Path $selectedToolchainDirectory 'bin\cargo.exe') @('--version')
    $rustlibDirectory = Join-Path $selectedToolchainDirectory 'lib\rustlib'
    $observedTargets = @()
    if (Test-Path -LiteralPath $rustlibDirectory -PathType Container) {
        foreach ($targetDirectory in @(Get-ChildItem -LiteralPath $rustlibDirectory -Directory)) {
            $targetLibrary = Join-Path $targetDirectory.FullName 'lib'
            if ((Test-Path -LiteralPath $targetLibrary -PathType Container) -and
                @(Get-ChildItem -LiteralPath $targetLibrary -Filter 'libstd-*.rlib' -File).Count -gt 0) {
                $observedTargets += $targetDirectory.Name
            }
        }
    }
    $targetProbe = [pscustomobject]@{ available = $true; exit_code = 0; output = ($observedTargets -join [Environment]::NewLine); evidence = 'Installed rustlib directories containing libstd rlibs; no target installation attempted.' }
}
elseif ($rustupPath -and $selectedToolchain) {
    $rustcProbe = Invoke-ReadOnlyProbe $rustupPath @('run', $selectedToolchain, 'rustc', '--version', '--verbose')
    $cargoProbe = Invoke-ReadOnlyProbe $rustupPath @('run', $selectedToolchain, 'cargo', '--version')
    $targetProbe = Invoke-ReadOnlyProbe $rustupPath @('target', 'list', '--installed', '--toolchain', $selectedToolchain)
}
elseif (-not $rustupPath) {
    $rustcProbe = Invoke-ReadOnlyProbe $rustcPath @('--version', '--verbose')
    $cargoProbe = Invoke-ReadOnlyProbe $cargoPath @('--version')
    $targetProbe = [pscustomobject]@{ available = $false; exit_code = $null; output = 'rustup not found; installed target inventory unavailable.' }
}
else {
    $rustcProbe = [pscustomobject]@{ available = $false; exit_code = $null; output = 'An installed rustup toolchain could not be confirmed. No installation attempted.' }
    $cargoProbe = $rustcProbe
    $targetProbe = $rustcProbe
}

$nsisProbe = Invoke-ReadOnlyProbe $nsisPath @('/VERSION')
$vswherePath = Get-CommandPath 'vswhere.exe'
if (-not $vswherePath -and ${env:ProgramFiles(x86)}) {
    $vswhereCandidate = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $vswhereCandidate -PathType Leaf) { $vswherePath = $vswhereCandidate }
}
$vsProbe = Invoke-ReadOnlyProbe $vswherePath @('-all', '-products', '*', '-prerelease', '-format', 'json', '-utf8')
$visualStudio = @()
if ($vsProbe.available -and $vsProbe.exit_code -eq 0 -and $vsProbe.output) {
    try {
        $visualStudio = @($vsProbe.output | ConvertFrom-Json | ForEach-Object {
            [pscustomobject]@{
                display_name = $_.displayName
                version = $_.installationVersion
                path = $_.installationPath
            }
        })
    }
    catch { $visualStudio = @() }
}

$msvcCompilers = @()
foreach ($vsInstance in $visualStudio) {
    $msvcRoot = Join-Path $vsInstance.path 'VC\Tools\MSVC'
    if (Test-Path -LiteralPath $msvcRoot -PathType Container) {
        foreach ($msvcVersion in @(Get-ChildItem -LiteralPath $msvcRoot -Directory)) {
            foreach ($targetArchitecture in @('x64', 'x86', 'arm64')) {
                $compilerPath = Join-Path $msvcVersion.FullName "bin\Hostx64\$targetArchitecture\cl.exe"
                if (Test-Path -LiteralPath $compilerPath -PathType Leaf) {
                    $msvcCompilers += [pscustomobject]@{
                        target = $targetArchitecture
                        toolset = $msvcVersion.Name
                        file_version = Get-FileVersion $compilerPath
                        path = $compilerPath
                    }
                }
            }
        }
    }
}

$sdkInfo = $null
foreach ($sdkKey in @('HKLM:\SOFTWARE\Microsoft\Microsoft SDKs\Windows\v10.0', 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Microsoft SDKs\Windows\v10.0')) {
    if (Test-Path -LiteralPath $sdkKey) {
        $sdk = Get-ItemProperty -LiteralPath $sdkKey
        $sdkInfo = [pscustomobject]@{ registry_key = $sdkKey; version = $sdk.ProductVersion; path = $sdk.InstallationFolder }
        break
    }
}

$isElevated = $null
try {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    $isElevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}
catch { }

$osInfo = $null
try {
    $os = Get-CimInstance -ClassName Win32_OperatingSystem
    $osInfo = [pscustomobject]@{ caption = $os.Caption; version = $os.Version; build = $os.BuildNumber; architecture = $os.OSArchitecture }
}
catch {
    $osInfo = [pscustomobject]@{ caption = 'Unavailable'; version = [Environment]::OSVersion.VersionString; build = $null; architecture = $env:PROCESSOR_ARCHITECTURE }
}

$developmentEvidence = @()
if ($rustupPath -or $rustcPath -or $cargoPath) { $developmentEvidence += 'Rust development tooling is present.' }
if ($visualStudio.Count -gt 0 -or $msvcCompilers.Count -gt 0) { $developmentEvidence += 'Visual Studio or MSVC development tooling is present.' }
if ($nsisPath) { $developmentEvidence += 'NSIS build tooling is present.' }
if ($sdkInfo) { $developmentEvidence += 'A Windows SDK installation is registered.' }

$report = [ordered]@{
    schema_version = 1
    observed_at_utc = [DateTime]::UtcNow.ToString('o')
    read_only = $true
    environment = [ordered]@{
        os = $osInfo
        powershell = $PSVersionTable.PSVersion.ToString()
        elevated = $isElevated
        clean_endpoint_validation = 'NOT_PERFORMED'
        environment_classification = $(if ($developmentEvidence.Count -gt 0) { 'DEVELOPMENT_MACHINE' } else { 'UNVERIFIED_MACHINE' })
        evidence = $developmentEvidence
        limitations = @(
            'This inventory does not prove compatibility with a clean Windows installation or an ordinary unelevated account.',
            'No VM, signature, graphics, antivirus, installation, or uninstall acceptance test was performed.',
            'No software, targets, system policies, registry values, or PATH entries were installed or changed.'
        )
    }
    rust = [ordered]@{
        rustup_path = $rustupPath
        rustc_path = $rustcPath
        cargo_path = $cargoPath
        installed_toolchains = $installedToolchains
        toolchain_inventory = $toolchainProbe
        selected_installed_toolchain = $selectedToolchain
        rustc = $rustcProbe
        cargo = $cargoProbe
        installed_targets = $targetProbe
    }
    nsis = [ordered]@{ path = $nsisPath; version = $nsisProbe }
    msvc = [ordered]@{
        cl_on_path = Get-CommandPath 'cl.exe'
        linker_on_path = Get-CommandPath 'link.exe'
        visual_studio_instances = $visualStudio
        compilers = $msvcCompilers
        windows_sdk = $sdkInfo
    }
}

if ($Json) {
    $report | ConvertTo-Json -Depth 10
}
else {
    Write-Output 'Avelin development environment inventory (read only)'
    Write-Output ("Environment: {0}; elevated: {1}" -f $report.environment.environment_classification, $isElevated)
    Write-Output ("OS: {0} {1} ({2})" -f $osInfo.caption, $osInfo.version, $osInfo.architecture)
    Write-Output ("Rust: {0}" -f $rustcProbe.output)
    Write-Output ("Cargo: {0}" -f $cargoProbe.output)
    Write-Output ("Installed targets: {0}" -f ($targetProbe.output -replace '\r?\n', ', '))
    Write-Output ("NSIS: {0}; path: {1}" -f $nsisProbe.output, $nsisPath)
    foreach ($compiler in $msvcCompilers) {
        Write-Output ("MSVC {0}: {1} ({2})" -f $compiler.target, $compiler.toolset, $compiler.path)
    }
    foreach ($limitation in $report.environment.limitations) { Write-Output $limitation }
}
