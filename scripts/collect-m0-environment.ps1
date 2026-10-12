<#
.SYNOPSIS
Collect read-only Windows M0 compatibility evidence without installing anything.
.DESCRIPTION
Compatible with Windows PowerShell 5.1 and PowerShell 7. JSON is written to
stdout. Optional -Output writes the same UTF-8 JSON to a new file; existing
files are never overwritten. Relative output paths use the current directory.
Unavailable queries include an error and are never interpreted as disabled or
absent. This inventory does not prove that a machine is clean or identify the
renderer actually selected by the application. No account name or SID is saved.
.PARAMETER Output
Optional new JSON file. Missing parent directories are created.
.PARAMETER ImageId
Operator-supplied VM/image identity. Defaults to unknown; not independently
verified. Do not put credentials or personal information in this value.
#>
[CmdletBinding()]
param(
    [string]$Output,
    [ValidateLength(1, 256)][string]$ImageId = 'unknown'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-SafeError([object]$Failure) {
    $message = [string]$Failure.Exception.Message
    if ([string]::IsNullOrWhiteSpace($message)) { $message = [string]$Failure }
    if ($env:USERPROFILE) { $message = $message.Replace($env:USERPROFILE, '<user-profile>') }
    if ($env:USERNAME) { $message = $message.Replace($env:USERNAME, '<user>') }
    return ($message -replace 'S-1-\d+(?:-\d+)+', '<sid>')
}

function Read-Evidence([scriptblock]$Query, [string]$Source, [switch]$Array) {
    try {
        if ($Array) { $value = @(& $Query) } else { $value = & $Query }
        return [ordered]@{ status = 'Available'; source = $Source; value = $value }
    }
    catch { return [ordered]@{ status = 'Unavailable'; source = $Source; error = (Get-SafeError $_) } }
}

function Read-RegistryValues([string]$Hive, [string]$SubKey, [string[]]$Names, [string]$View = 'Registry64') {
    $source = "$Hive\$SubKey ($View)"
    $base = $null; $key = $null
    try {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
            [Microsoft.Win32.RegistryHive]::$Hive, [Microsoft.Win32.RegistryView]::$View)
        $key = $base.OpenSubKey($SubKey, $false)
        if ($null -eq $key) { return [ordered]@{ status = 'Absent'; source = $source; meaning = 'Registry key is not present; not an effective-policy verdict.' } }
        $values = [ordered]@{}
        $present = $key.GetValueNames()
        foreach ($name in $Names) {
            if ($present -contains $name) {
                $values[$name] = [ordered]@{ status = 'Available'; value = $key.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) }
            }
            else { $values[$name] = [ordered]@{ status = 'Absent'; meaning = 'No explicit value; OS default or another policy source may apply.' } }
        }
        return [ordered]@{ status = 'Available'; source = $source; values = $values }
    }
    catch { return [ordered]@{ status = 'Unavailable'; source = $source; error = (Get-SafeError $_) } }
    finally { if ($key) { $key.Dispose() }; if ($base) { $base.Dispose() } }
}

function Select-Fields([object]$Record, [string[]]$Names) {
    $result = [ordered]@{}
    foreach ($name in $Names) {
        $property = $Record.PSObject.Properties[$name]
        if ($null -ne $property -and $null -ne $property.Value) { $result[$name] = $property.Value }
        else { $result[$name] = [ordered]@{ status = 'Unavailable'; error = 'Property was not returned by the provider.' } }
    }
    return $result
}

function Read-RegisteredProducts([string]$Hive, [string]$View) {
    $subKey = 'SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall'
    $source = "$Hive\$subKey ($View), matching development tools and VC redistributables only"
    $base = $null; $key = $null
    try {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey(
            [Microsoft.Win32.RegistryHive]::$Hive, [Microsoft.Win32.RegistryView]::$View)
        $key = $base.OpenSubKey($subKey, $false)
        if ($null -eq $key) { return [ordered]@{ status = 'Absent'; source = $source; products = @() } }
        $names = $key.GetSubKeyNames()
        if ($names.Length -gt 5000) { throw 'Uninstall registry contains more than the bounded 5000 entries; inventory not completed.' }
        $products = @(); $unavailable = @()
        foreach ($name in $names) {
            $product = $null
            try {
                $product = $key.OpenSubKey($name, $false)
                if ($null -eq $product) { throw 'Registered product disappeared during collection.' }
                $displayName = [string]$product.GetValue('DisplayName', '')
                if ($displayName -match '(?i)Visual Studio|Build Tools|Windows.*(SDK|Software Development Kit)|NSIS|Nullsoft.*Install|Rust(up)?|Visual C\+\+.*(Redistributable|Runtime)|VC\+\+.*Runtime') {
                    $products += [ordered]@{ name = $displayName; version = $product.GetValue('DisplayVersion', $null); publisher = $product.GetValue('Publisher', $null) }
                }
            }
            catch { $unavailable += (Get-SafeError $_) }
            finally { if ($product) { $product.Dispose() } }
        }
        if ($unavailable.Count -gt 0) { return [ordered]@{ status = 'Unavailable'; source = $source; error = 'One or more registered products could not be queried; partial list only.'; errors = $unavailable; products = $products } }
        return [ordered]@{ status = 'Available'; source = $source; products = $products; limitation = 'Only selected registered products are listed. No matching product does not prove absence of portable or unregistered tools.' }
    }
    catch { return [ordered]@{ status = 'Unavailable'; source = $source; error = (Get-SafeError $_) } }
    finally { if ($key) { $key.Dispose() }; if ($base) { $base.Dispose() } }
}

function Read-KnownExecutable([string]$Name, [string]$Path, [string]$Source) {
    try {
        if ([string]::IsNullOrWhiteSpace($Path)) { throw 'The known installation root was not defined.' }
        $attributes = [IO.File]::GetAttributes($Path)
        if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) { throw 'Expected an executable file, found a directory.' }
        $version = [Diagnostics.FileVersionInfo]::GetVersionInfo($Path)
        return [ordered]@{ name = $Name; status = 'Available'; source = $Source; file_version = $version.FileVersion; product_version = $version.ProductVersion; limitation = 'File presence only; executable is not run and usable toolchain/dependencies are not verified.' }
    }
    catch [IO.FileNotFoundException] { return [ordered]@{ name = $Name; status = 'Absent'; source = $Source } }
    catch [IO.DirectoryNotFoundException] { return [ordered]@{ name = $Name; status = 'Absent'; source = $Source } }
    catch { return [ordered]@{ name = $Name; status = 'Unavailable'; source = $Source; error = (Get-SafeError $_) } }
}

$outputPath = $null
if ($Output) {
    $outputPath = [IO.Path]::GetFullPath($Output)
    if (Test-Path -LiteralPath $outputPath) { throw "Output exists; choose a new file: $outputPath" }
}

$nativeLoad = Read-Evidence {
    if (-not ('AvelinM0Environment.Native' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
namespace AvelinM0Environment {
    public static class Native {
        [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)]
        public struct VersionInfo {
            public uint Size, Major, Minor, Build, Platform;
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst=128)] public string ServicePack;
            public ushort ServicePackMajor, ServicePackMinor, SuiteMask;
            public byte ProductType, Reserved;
        }
        [DllImport("ntdll.dll", CharSet=CharSet.Unicode)] static extern int RtlGetVersion(ref VersionInfo info);
        public static VersionInfo OsVersion() {
            VersionInfo value = new VersionInfo(); value.Size = (uint)Marshal.SizeOf(typeof(VersionInfo));
            int status = RtlGetVersion(ref value); if (status != 0) throw new InvalidOperationException("RtlGetVersion NTSTATUS: " + status);
            return value;
        }
        [DllImport("advapi32.dll", SetLastError=true)] static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
        [DllImport("advapi32.dll", SetLastError=true)] static extern bool GetTokenInformation(IntPtr token, int kind, IntPtr data, int length, out int needed);
        [DllImport("advapi32.dll")] static extern bool IsWellKnownSid(IntPtr sid, int kind);
        [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
        [StructLayout(LayoutKind.Sequential)] struct SidAndAttributes { public IntPtr Sid; public uint Attributes; }
        [StructLayout(LayoutKind.Sequential)] struct TokenGroups { public uint Count; public SidAndAttributes First; }
        public sealed class AccountInfo { public bool Elevated, AdministratorMember, AdministratorGroupDenyOnly; }
        public static AccountInfo Account() {
            IntPtr token;
            if (!OpenProcessToken(Process.GetCurrentProcess().Handle, 8, out token)) throw new Win32Exception(Marshal.GetLastWin32Error());
            try {
                AccountInfo result = new AccountInfo(); int needed;
                IntPtr elevation = Marshal.AllocHGlobal(4);
                try { if (!GetTokenInformation(token, 20, elevation, 4, out needed)) throw new Win32Exception(Marshal.GetLastWin32Error()); result.Elevated = Marshal.ReadInt32(elevation) != 0; }
                finally { Marshal.FreeHGlobal(elevation); }
                GetTokenInformation(token, 2, IntPtr.Zero, 0, out needed);
                if (needed <= 0) throw new Win32Exception(Marshal.GetLastWin32Error());
                IntPtr groups = Marshal.AllocHGlobal(needed);
                try {
                    if (!GetTokenInformation(token, 2, groups, needed, out needed)) throw new Win32Exception(Marshal.GetLastWin32Error());
                    int count = Marshal.ReadInt32(groups);
                    int offset = Marshal.OffsetOf(typeof(TokenGroups), "First").ToInt32();
                    int stride = Marshal.SizeOf(typeof(SidAndAttributes));
                    for (int i=0; i<count; i++) {
                        SidAndAttributes entry = (SidAndAttributes)Marshal.PtrToStructure(IntPtr.Add(groups, offset + i*stride), typeof(SidAndAttributes));
                        if (IsWellKnownSid(entry.Sid, 26)) { result.AdministratorMember = true; result.AdministratorGroupDenyOnly = (entry.Attributes & 0x10) != 0; }
                    }
                } finally { Marshal.FreeHGlobal(groups); }
                return result;
            } finally { CloseHandle(token); }
        }
        [DllImport("wtsapi32.dll", SetLastError=true)] static extern bool WTSQuerySessionInformationW(IntPtr server, int session, int kind, out IntPtr data, out int size);
        [DllImport("wtsapi32.dll")] static extern void WTSFreeMemory(IntPtr data);
        [DllImport("user32.dll")] public static extern int GetSystemMetrics(int index);
        public static int SessionProtocol() {
            IntPtr data; int size;
            if (!WTSQuerySessionInformationW(IntPtr.Zero, Process.GetCurrentProcess().SessionId, 16, out data, out size)) throw new Win32Exception(Marshal.GetLastWin32Error());
            try { if (size < 2) throw new InvalidOperationException("WTS protocol response is too short."); return (int)(ushort)Marshal.ReadInt16(data); }
            finally { WTSFreeMemory(data); }
        }
        [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
        [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct MonitorInfo {
            public int Size; public Rect Monitor, Work; public uint Flags;
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst=32)] public string Device;
        }
        delegate bool MonitorCallback(IntPtr monitor, IntPtr dc, IntPtr rect, IntPtr data);
        [DllImport("user32.dll", SetLastError=true)] static extern bool EnumDisplayMonitors(IntPtr dc, IntPtr rect, MonitorCallback callback, IntPtr data);
        [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool GetMonitorInfoW(IntPtr monitor, ref MonitorInfo info);
        [DllImport("shcore.dll")] static extern int GetScaleFactorForMonitor(IntPtr monitor, out int scale);
        [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct DeviceMode {
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst=32)] public string DeviceName;
            public ushort SpecVersion, DriverVersion, Size, DriverExtra;
            public uint Fields; public int PositionX, PositionY; public uint Orientation, FixedOutput;
            public short Color, Duplex, YResolution, TTOption, Collate;
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst=32)] public string FormName;
            public ushort LogPixels; public uint BitsPerPixel, Width, Height, Flags, Frequency;
            public uint IcmMethod, IcmIntent, MediaType, DitherType, Reserved1, Reserved2, PanningWidth, PanningHeight;
        }
        [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern bool EnumDisplaySettingsW(string device, int index, ref DeviceMode mode);
        [DllImport("user32.dll")] public static extern uint GetDpiForSystem();
        [DllImport("user32.dll")] static extern IntPtr GetThreadDpiAwarenessContext();
        [DllImport("user32.dll")] static extern int GetAwarenessFromDpiAwarenessContext(IntPtr context);
        public static int DpiAwareness() { return GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()); }
        public sealed class Display {
            public string Device, Status, Error, ScaleStatus, ScaleError, ResolutionStatus, ResolutionError;
            public bool Primary; public Rect Bounds; public int? ScalePercent;
            public uint? PixelWidth, PixelHeight, RefreshHz;
        }
        public static Display[] Displays() {
            List<Display> list = new List<Display>();
            MonitorCallback callback = delegate(IntPtr monitor, IntPtr dc, IntPtr rect, IntPtr data) {
                Display display = new Display();
                try {
                    MonitorInfo info = new MonitorInfo(); info.Size = Marshal.SizeOf(typeof(MonitorInfo));
                    if (!GetMonitorInfoW(monitor, ref info)) throw new Win32Exception(Marshal.GetLastWin32Error());
                    display.Device = info.Device; display.Primary = (info.Flags & 1) != 0; display.Bounds = info.Monitor; display.Status = "Available";
                    try {
                        int scale; int status = GetScaleFactorForMonitor(monitor, out scale);
                        if (status != 0) throw new InvalidOperationException("GetScaleFactorForMonitor HRESULT: " + status);
                        display.ScalePercent = scale; display.ScaleStatus = "Available";
                    } catch (Exception error) { display.ScaleStatus = "Unavailable"; display.ScaleError = error.Message; }
                    try {
                        DeviceMode mode = new DeviceMode(); mode.Size = (ushort)Marshal.SizeOf(typeof(DeviceMode));
                        if (!EnumDisplaySettingsW(info.Device, -1, ref mode)) throw new InvalidOperationException("EnumDisplaySettingsW did not return the current display mode.");
                        display.PixelWidth = mode.Width; display.PixelHeight = mode.Height; display.RefreshHz = mode.Frequency; display.ResolutionStatus = "Available";
                    } catch (Exception error) { display.ResolutionStatus = "Unavailable"; display.ResolutionError = error.Message; }
                } catch (Exception error) { display.Status = "Unavailable"; display.Error = error.Message; }
                list.Add(display); return true;
            };
            if (!EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, callback, IntPtr.Zero)) throw new Win32Exception(Marshal.GetLastWin32Error());
            return list.ToArray();
        }
    }
}
'@
    }
    return 'Read-only Win32 helpers loaded; no DPI/display/policy setting is changed.'
} 'In-process Add-Type compilation of read-only Win32 queries'

$osActual = Read-Evidence {
    $version = [AvelinM0Environment.Native]::OsVersion()
    $family = 'Windows (unclassified)'
    if ($version.ProductType -eq 1 -and $version.Major -eq 10) {
        if ($version.Build -ge 22000) { $family = 'Windows 11' } else { $family = 'Windows 10' }
    }
    elseif ($version.ProductType -ne 1) { $family = 'Windows Server (edition reported separately)' }
    return [ordered]@{ family = $family; major = $version.Major; minor = $version.Minor; build = $version.Build; product_type = $version.ProductType; classification = 'Client family uses actual kernel build >= 22000 for Windows 11; registry ProductName is not used to classify it.' }
} 'ntdll!RtlGetVersion'

$osCim = Read-Evidence {
    $record = Get-CimInstance -ClassName Win32_OperatingSystem
    Select-Fields $record @('Caption', 'Version', 'BuildNumber', 'OperatingSystemSKU', 'OSArchitecture', 'TotalVisibleMemorySize', 'FreePhysicalMemory', 'LastBootUpTime')
} 'CIM Win32_OperatingSystem (memory values in KiB)'

$hardware = Read-Evidence {
    Select-Fields (Get-CimInstance -ClassName Win32_ComputerSystem) @('Manufacturer', 'Model', 'SystemType', 'TotalPhysicalMemory', 'NumberOfLogicalProcessors', 'HypervisorPresent')
} 'CIM Win32_ComputerSystem; no machine name or serial number'

$processors = Read-Evidence {
    @(Get-CimInstance -ClassName Win32_Processor | ForEach-Object { Select-Fields $_ @('Name', 'Manufacturer', 'NumberOfCores', 'NumberOfLogicalProcessors', 'AddressWidth', 'VirtualizationFirmwareEnabled', 'SecondLevelAddressTranslationExtensions') })
} 'CIM Win32_Processor' -Array

$account = Read-Evidence {
    $value = [AvelinM0Environment.Native]::Account()
    [ordered]@{ elevated = $value.Elevated; administrator_group_in_token = $value.AdministratorMember; administrator_group_deny_only = $value.AdministratorGroupDenyOnly; membership_scope = 'Current process token, including deny-only Administrators group in a UAC filtered token; not an AD/local-group directory inventory.' }
} 'Win32 process token TokenElevation and TokenGroups; no username or SID emitted'

$session = Read-Evidence {
    $protocol = [AvelinM0Environment.Native]::SessionProtocol()
    $kind = 'OtherOrUnknown'
    if ($protocol -eq 0) { $kind = 'LocalConsole' } elseif ($protocol -eq 2) { $kind = 'RDP' }
    [ordered]@{ current_process_session_id = [Diagnostics.Process]::GetCurrentProcess().SessionId; protocol = $kind; protocol_code = $protocol; remote_session_metric = ([AvelinM0Environment.Native]::GetSystemMetrics(4096) -ne 0); limitation = 'Current process session only. Does not assert that no other RDP sessions exist.' }
} 'WTSClientProtocolType and SM_REMOTESESSION'

$displays = Read-Evidence {
    @([AvelinM0Environment.Native]::Displays() | ForEach-Object {
        $display = $_
        if ($display.Status -ne 'Available') { [ordered]@{ status = 'Unavailable'; error = $display.Error }; return }
        $scale = [ordered]@{ status = $display.ScaleStatus; source = 'shcore!GetScaleFactorForMonitor'; percent = $display.ScalePercent }
        if ($display.ScaleStatus -ne 'Available') { $scale['error'] = $display.ScaleError }
        $resolution = [ordered]@{ status = $display.ResolutionStatus; source = 'EnumDisplaySettingsW current display mode, physical pixels'; width = $display.PixelWidth; height = $display.PixelHeight; refresh_hz = $display.RefreshHz }
        if ($display.ResolutionStatus -ne 'Available') { $resolution['error'] = $display.ResolutionError }
        [ordered]@{ status = 'Available'; device = $display.Device; primary = $display.Primary; desktop_bounds = [ordered]@{ left = $display.Bounds.Left; top = $display.Bounds.Top; width = $display.Bounds.Right - $display.Bounds.Left; height = $display.Bounds.Bottom - $display.Bounds.Top }; bounds_units = 'Caller DPI-awareness-dependent desktop coordinates, not a physical-pixel resolution assertion.'; current_resolution = $resolution; per_monitor_scale = $scale }
    })
} 'EnumDisplayMonitors/GetMonitorInfo of the current desktop; includes current-session surfaces only' -Array

$displayDrivers = Read-Evidence {
    @(Get-CimInstance -ClassName Win32_VideoController | ForEach-Object { Select-Fields $_ @('Name', 'AdapterCompatibility', 'DriverVersion', 'DriverDate', 'VideoProcessor', 'CurrentHorizontalResolution', 'CurrentVerticalResolution', 'CurrentBitsPerPixel', 'Status') })
} 'CIM Win32_VideoController: installed/visible adapters, not the renderer selected by Avelin; null resolution is Unavailable' -Array

$defender = Read-Evidence {
    if (-not (Get-Command Get-MpComputerStatus -ErrorAction SilentlyContinue)) { throw 'Get-MpComputerStatus cmdlet is not available in this PowerShell session.' }
    Select-Fields (Get-MpComputerStatus -ErrorAction Stop) @('AMProductVersion', 'AMEngineVersion', 'AMRunningMode', 'AMServiceEnabled', 'AntivirusEnabled', 'AntispywareEnabled', 'BehaviorMonitorEnabled', 'RealTimeProtectionEnabled', 'IoavProtectionEnabled', 'NISEnabled', 'IsTamperProtected', 'AntivirusSignatureVersion', 'AntivirusSignatureLastUpdated', 'RebootRequired')
} 'Get-MpComputerStatus; no exclusions or private scan history collected'

$codeIntegrity = Read-Evidence {
    @(Get-CimInstance -Namespace root\Microsoft\Windows\DeviceGuard -ClassName Win32_DeviceGuard -ErrorAction Stop | ForEach-Object { Select-Fields $_ @('CodeIntegrityPolicyEnforcementStatus', 'UsermodeCodeIntegrityPolicyEnforcementStatus', 'SecurityServicesConfigured', 'SecurityServicesRunning', 'VirtualizationBasedSecurityStatus') })
} 'CIM Win32_DeviceGuard; integrity enforcement codes 0=Off, 1=Audit, 2=Enforced when available; not a per-file trust verdict' -Array

$executionPolicy = Read-Evidence {
    @(Get-ExecutionPolicy -List | ForEach-Object { [ordered]@{ scope = [string]$_.Scope; policy = [string]$_.ExecutionPolicy } })
} 'Get-ExecutionPolicy -List; applies to PowerShell scripts, not executable-signature acceptance' -Array

$cargoRoot = $env:CARGO_HOME
if (-not $cargoRoot -and $env:USERPROFILE) { $cargoRoot = Join-Path $env:USERPROFILE '.cargo' }
$knownTools = @()
foreach ($tool in @('rustup', 'rustc', 'cargo')) {
    $knownPath = $null
    if ($cargoRoot) { $knownPath = Join-Path $cargoRoot "bin\$tool.exe" }
    $knownTools += Read-KnownExecutable $tool $knownPath 'Explicit CARGO_HOME/bin or conventional current-user .cargo/bin; no recursive search'
}
foreach ($programRoot in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
    if ($programRoot) { $knownTools += Read-KnownExecutable 'makensis' (Join-Path $programRoot 'NSIS\makensis.exe') 'Conventional Program Files/NSIS directory; no recursive search' }
}

$report = [ordered]@{
    schema_version = 1
    collected_at_utc = [DateTime]::UtcNow.ToString('o')
    image = [ordered]@{ id = $ImageId; source = 'Operator-supplied -ImageId'; independently_verified = $false; clean_machine_status = 'Unknown' }
    collector = [ordered]@{ powershell_version = $PSVersionTable.PSVersion.ToString(); process_64_bit = [Environment]::Is64BitProcess; os_64_bit = [Environment]::Is64BitOperatingSystem; native_helpers = $nativeLoad }
    os = [ordered]@{
        actual_version = $osActual
        cim = $osCim
        edition_and_patch = Read-RegistryValues 'LocalMachine' 'SOFTWARE\Microsoft\Windows NT\CurrentVersion' @('EditionID', 'DisplayVersion', 'CurrentBuildNumber', 'UBR', 'BuildLabEx', 'InstallationType')
        installed_hotfixes = Read-Evidence { @(Get-CimInstance -ClassName Win32_QuickFixEngineering | ForEach-Object { Select-Fields $_ @('HotFixID', 'Description', 'InstalledOn') }) } 'CIM Win32_QuickFixEngineering; partial Windows servicing inventory, excludes updates not exposed by this provider' -Array
    }
    hardware = $hardware
    processors = $processors
    account = $account
    current_session = $session
    graphics = [ordered]@{
        monitors = $displays
        system_dpi = Read-Evidence { [AvelinM0Environment.Native]::GetDpiForSystem() } 'GetDpiForSystem result for collector awareness; not true per-monitor DPI'
        collector_dpi_awareness = Read-Evidence { [AvelinM0Environment.Native]::DpiAwareness() } 'Current thread: -1=Invalid, 0=Unaware, 1=SystemAware, 2=PerMonitorAware; not changed by collector'
        adapters = $displayDrivers
        application_renderer = [ordered]@{ status = 'Unavailable'; error = 'Requires evidence from the actual Avelin UI probe log; adapter inventory does not identify selected renderer.' }
    }
    security = [ordered]@{
        uac = Read-RegistryValues 'LocalMachine' 'SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' @('EnableLUA', 'ConsentPromptBehaviorAdmin', 'ConsentPromptBehaviorUser', 'PromptOnSecureDesktop', 'ValidateAdminCodeSignatures', 'FilterAdministratorToken')
        defender = $defender
        code_integrity = $codeIntegrity
        powershell_execution_policy = $executionPolicy
        smartscreen_policy = Read-RegistryValues 'LocalMachine' 'SOFTWARE\Policies\Microsoft\Windows\System' @('EnableSmartScreen', 'ShellSmartScreenLevel')
        explorer_smartscreen = Read-RegistryValues 'LocalMachine' 'SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer' @('SmartScreenEnabled')
        applocker_policy_registration = Read-RegistryValues 'LocalMachine' 'SOFTWARE\Policies\Microsoft\Windows\SrpV2' @()
        executable_acceptance = [ordered]@{ status = 'Unavailable'; error = 'No executable launched. Inventory cannot establish whether SmartScreen, WDAC, AppLocker, or antivirus will accept a specific unsigned package.' }
    }
    tool_inventory = [ordered]@{
        limitation = 'Read-only selected registrations and known installation paths only; no full-disk scan and no tool/proxy execution. This cannot certify an uncontaminated clean machine.'
        registered_products = @(
            (Read-RegisteredProducts 'LocalMachine' 'Registry64'),
            (Read-RegisteredProducts 'LocalMachine' 'Registry32'),
            (Read-RegisteredProducts 'CurrentUser' 'Registry64'),
            (Read-RegisteredProducts 'CurrentUser' 'Registry32')
        )
        visual_studio_versions = Read-RegistryValues 'LocalMachine' 'SOFTWARE\Microsoft\VisualStudio\SxS\VS7' @() 'Registry32'
        windows_sdk_registration = @(
            (Read-RegistryValues 'LocalMachine' 'SOFTWARE\Microsoft\Windows Kits\Installed Roots' @() 'Registry64'),
            (Read-RegistryValues 'LocalMachine' 'SOFTWARE\Microsoft\Windows Kits\Installed Roots' @() 'Registry32')
        )
        known_executables = $knownTools
        vc_runtime_registrations = @(
            (Read-RegistryValues 'LocalMachine' 'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64' @('Installed', 'Version', 'Major', 'Minor', 'Bld', 'Rbld') 'Registry64'),
            (Read-RegistryValues 'LocalMachine' 'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64' @('Installed', 'Version', 'Major', 'Minor', 'Bld', 'Rbld') 'Registry32'),
            (Read-RegistryValues 'LocalMachine' 'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x86' @('Installed', 'Version', 'Major', 'Minor', 'Bld', 'Rbld') 'Registry64'),
            (Read-RegistryValues 'LocalMachine' 'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x86' @('Installed', 'Version', 'Major', 'Minor', 'Bld', 'Rbld') 'Registry32')
        )
    }
    limitations = @(
        'This is a non-atomic point-in-time inventory; resources and policies may change during collection.'
        'Unavailable includes access denial, missing provider/API, unsupported fields, and query failure; it never means Disabled or Absent.'
        'Virtualization firmware or HypervisorPresent alone does not prove this is a VM; image identity and clean state require operator evidence.'
        'No serial number, computer name, account name, SID, product key, Defender exclusions, or task token is intentionally collected.'
        'No installer, driver, toolchain, or security/display setting is installed, executed, enabled, disabled, or modified.'
    )
}

$json = $report | ConvertTo-Json -Depth 16
if ($outputPath) {
    $parent = [IO.Path]::GetDirectoryName($outputPath)
    $null = [IO.Directory]::CreateDirectory($parent)
    $stream = [IO.File]::Open($outputPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try {
        $bytes = (New-Object Text.UTF8Encoding($false)).GetBytes($json + [Environment]::NewLine)
        $stream.Write($bytes, 0, $bytes.Length)
    }
    finally { $stream.Dispose() }
}
Write-Output $json
