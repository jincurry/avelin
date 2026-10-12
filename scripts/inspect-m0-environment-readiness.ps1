<#
.SYNOPSIS
Read-only inventory of the local Windows test-environment prerequisites.
.DESCRIPTION
Runs collect-m0-environment.ps1, queries named virtualization features and
services, and counts local Hyper-V guests when the provider is accessible.
Does not scan disk images, enumerate remote hosts, start programs or guests,
enable features, change policy, or claim an independent clean Windows test.
Compatible with Windows PowerShell 5.1 and PowerShell 7. Optional -Output
writes one new UTF-8 JSON file; existing files are never overwritten.
#>
[CmdletBinding()]
param(
    [string]$Output,
    [ValidateLength(1, 256)][string]$ImageId = 'development-host-unverified'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-SafeError([object]$Failure) {
    $message = [string]$Failure.Exception.Message
    if ($env:USERPROFILE) { $message = $message.Replace($env:USERPROFILE, '<user-profile>') }
    if ($env:USERNAME) { $message = $message.Replace($env:USERNAME, '<user>') }
    return ($message -replace 'S-1-\d+(?:-\d+)+', '<sid>')
}

function Read-Probe([string]$Source, [scriptblock]$Query) {
    try { return [ordered]@{ status = 'Available'; source = $Source; value = (& $Query) } }
    catch { return [ordered]@{ status = 'Unavailable'; source = $Source; error = (Get-SafeError $_) } }
}

$outputPath = $null
if ($Output) {
    $outputPath = [IO.Path]::GetFullPath($Output)
    if (Test-Path -LiteralPath $outputPath) { throw 'Output already exists; choose a new file.' }
}
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Windows is required.' }

$collectorPath = Join-Path $PSScriptRoot 'collect-m0-environment.ps1'
$hostInventory = (& $collectorPath -ImageId $ImageId | Out-String) | ConvertFrom-Json
$featureNames = @('Containers-DisposableClientVM', 'Microsoft-Hyper-V-All', 'Microsoft-Hyper-V', 'VirtualMachinePlatform')
$features = foreach ($featureName in $featureNames) {
    $probe = Read-Probe "CIM Win32_OptionalFeature: $featureName" {
        $items = @(Get-CimInstance -ClassName Win32_OptionalFeature -Filter "Name='$featureName'" -ErrorAction Stop)
        if ($items.Count -ne 1) { throw "Expected one feature record; returned $($items.Count)." }
        $state = [int]$items[0].InstallState
        $label = switch ($state) { 1 { 'Enabled' }; 2 { 'Disabled' }; 3 { 'Absent' }; default { 'Unknown' } }
        [ordered]@{ name = $featureName; install_state = $state; state = $label }
    }
    [ordered]@{ name = $featureName; observation = $probe }
}
$services = foreach ($serviceName in @('vmms', 'vmcompute', 'TermService')) {
    [ordered]@{ name = $serviceName; observation = (Read-Probe "Get-Service $serviceName" {
        $service = Get-Service -Name $serviceName -ErrorAction Stop
        [ordered]@{ status = [string]$service.Status; start_type = [string]$service.StartType }
    }) }
}
$commands = foreach ($commandName in @('Get-VM', 'VBoxManage.exe', 'vmrun.exe')) {
    [ordered]@{
        name = $commandName
        available_in_current_session = [bool](Get-Command $commandName -ErrorAction SilentlyContinue)
        limitation = 'Command discovery only; absence from this session does not prove the product or guest is absent.'
    }
}
$sandbox = Read-Probe 'Known system path: System32/WindowsSandbox.exe; no execution' {
    $path = Join-Path $env:SystemRoot 'System32\WindowsSandbox.exe'
    [ordered]@{ file_present = [bool](Test-Path -LiteralPath $path -PathType Leaf) }
}
$hyperv = Read-Probe 'Local CIM root/virtualization/v2 Msvm_ComputerSystem; virtual-machine count only' {
    $guests = @(Get-CimInstance -Namespace root/virtualization/v2 -ClassName Msvm_ComputerSystem -Filter "Caption='Virtual Machine'" -ErrorAction Stop)
    [ordered]@{
        guest_count = $guests.Count
        guest_states = @($guests | ForEach-Object { [ordered]@{ enabled_state = [int]$_.EnabledState } })
        limitation = 'Guest OS, clean-image identity, credentials and acceptance readiness are not inferred. Guest names and paths are not collected.'
    }
}
$report = [ordered]@{
    schema_version = 1
    collected_at_utc = [DateTime]::UtcNow.ToString('o')
    status = 'READ_ONLY_PREREQUISITE_INVENTORY_COMPLETED'
    collector = [ordered]@{
        powershell_version = $PSVersionTable.PSVersion.ToString()
        process_64_bit = [Environment]::Is64BitProcess
        script_sha256 = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash.ToLowerInvariant()
        environment_collector_sha256 = (Get-FileHash -LiteralPath $collectorPath -Algorithm SHA256).Hash.ToLowerInvariant()
    }
    local_virtualization = [ordered]@{ features = @($features); services = @($services); commands = @($commands); sandbox_launcher = $sandbox; hyperv_guests = $hyperv }
    host_environment = $hostInventory
    independent_environment = [ordered]@{
        status = 'NOT_ESTABLISHED'
        required_operator_evidence = @('Exact Windows image and snapshot identity', 'Authorized isolated desktop and ordinary account', 'Clean runtime baseline and protection state', 'Actual console/RDP and application renderer observations')
    }
    limitations = @(
        'Point-in-time prerequisite inventory, not a product acceptance result.'
        'Unavailable is a failed or inaccessible query; it never means disabled or absent.'
        'A hypervisor, enabled VirtualMachinePlatform, running vmcompute or installed client does not establish an available Windows guest.'
        'Sandbox feature or launcher presence does not establish successful launch or replace the Win10/Win11, standard-user and RDP matrix.'
        'No local/private image paths, other sessions, remote endpoints or user credentials are searched.'
        'No feature, virtual machine, RDP server, security setting, display setting or execution policy is changed.'
    )
}
$json = $report | ConvertTo-Json -Depth 22
if ($outputPath) {
    $null = [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($outputPath))
    $stream = [IO.File]::Open($outputPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try {
        $bytes = (New-Object Text.UTF8Encoding($false)).GetBytes($json + [Environment]::NewLine)
        $stream.Write($bytes, 0, $bytes.Length)
    }
    finally { $stream.Dispose() }
}
Write-Output $json
