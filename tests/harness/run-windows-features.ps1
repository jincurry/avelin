#requires -Version 7.2
<# Read-only runtime detection and update publishing plus current-user lifecycle
   acceptance. Every product uses a fresh UUID. Outputs remain below target;
   no runtime is downloaded or installed, no update URL is contacted, and no UAC
   request or recursive cleanup is made. Completed products uninstall themselves. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Runtime,
    [Parameter(Mandatory)][string]$Cli,
    [Parameter(Mandatory)][string]$RustWorker,
    [Parameter(Mandatory)][string]$OutputRoot,
    [string]$Sample = (Join-Path $PSScriptRoot '../../target/release/nimbus-demo.exe'),
    [ValidateRange(30, 1800)][int]$TimeoutSeconds = 180
)
. (Join-Path $PSScriptRoot 'run-rust-engine-interruption.ps1') -Runtime $Runtime -Cli $Cli -RustWorker $RustWorker -Sample $Sample -OutputRoot $OutputRoot -TimeoutSeconds $TimeoutSeconds -FunctionsOnly
Require ($IsWindows -and [Environment]::Is64BitProcess) 'Use Windows x64 PowerShell 7.2+'
$projectRoot = Full (Join-Path $PSScriptRoot '../..')
$script:RunRoot = Inside $OutputRoot (Join-Path $projectRoot 'target')
Require (-not (Test-Path -LiteralPath $script:RunRoot)) 'Choose a fresh OutputRoot'
$null = [IO.Directory]::CreateDirectory($script:RunRoot)
$script:RuntimeBinary = Full $Runtime; $script:CliBinary = Full $Cli
$script:WorkerBinary = Full $RustWorker; $script:SampleBinary = Full $Sample
foreach ($binary in @($script:RuntimeBinary, $script:CliBinary, $script:WorkerBinary, $script:SampleBinary)) { Require (Test-Path -LiteralPath $binary -PathType Leaf) "Missing binary: $binary" }
$script:UserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
$elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$summary = [ordered]@{
    schema_version=1; status='RUNNING'; cases=@(); elevated=$elevated
    started_at_utc=[DateTime]::UtcNow.ToString('o')
    components=@(foreach ($binary in @($script:RuntimeBinary,$script:CliBinary,$script:WorkerBinary,$script:SampleBinary)) { @{path=$binary;sha256=Hash $binary} })
    limitations=@('No network transfer or native GUI/UAC automation', 'A missing .NET 999.0 runtime is an intentional blocking fixture', 'Elevated hosts cannot prove the non-elevated rejection branch')
}
function Pass([string]$Label) {
    $summary.cases += @{label=$Label;status='PASSED'}
    Save (Join-Path $script:RunRoot 'summary.json') $summary
    Write-Host "[$Label] passed"
}
function NewFeatureCase([string]$Label) {
    $script:CaseRoot = Inside (Join-Path $script:RunRoot $Label) $script:RunRoot
    $script:InstallRoot = Join-Path $script:CaseRoot 'installed'
    $script:LocalRoot = Join-Path $script:CaseRoot 'local-data'
    $script:TempRoot = Join-Path $script:CaseRoot 'temp'
    foreach ($directory in @($script:CaseRoot,$script:LocalRoot,$script:TempRoot)) { $null=[IO.Directory]::CreateDirectory($directory) }
    $script:AppId = 'dev.avelin.features.' + [Guid]::NewGuid().ToString('N')
    $script:RegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:AppId
    Require (-not (RegistrySnapshot).exists) 'Unique test registration exists'
}
function FeatureConfig([string]$Label, [string]$Extra = '', [string]$Scope = 'current_user') {
    $project = Inside (Join-Path $script:CaseRoot "project-$Label") $script:RunRoot
    $payload = Join-Path $project 'payload'; $null=[IO.Directory]::CreateDirectory($payload)
    [IO.File]::Copy($script:SampleBinary,(Join-Path $payload 'app.exe'),$false)
    $path = Join-Path $project 'avelin.toml'
    WriteNew $path @"
schema_version = 2
[app]
id = "$($script:AppId)"
name = "Windows feature acceptance fixture"
version = "1.0.0"
entry = "app.exe"
[payload]
directory = "payload"
[install]
directory_name = "AvelinFeature-$($script:AppId.Split('.')[-1])"
scope = "$Scope"
desktop_shortcut = false
start_menu_shortcut = false
[[components]]
id = "core"
required = true
name = { "zh-CN" = "主程序", "en-US" = "Application" }
description = { "zh-CN" = "运行所需文件", "en-US" = "Files required to run the application" }
paths = ["app.exe"]
$Extra
"@
    return $path
}
function FeaturePackage([string]$Label, [string]$Config) {
    $setup = Join-Path $script:CaseRoot "$Label-Setup.exe"
    InvokeFixtureProcess $script:CliBinary @('build','--config',$Config,'--output',$setup,'--runtime',$script:RuntimeBinary,'--rust-worker',$script:WorkerBinary) "build-$Label"
    [pscustomobject]@{Setup=$setup; ManifestPath=[IO.Path]::ChangeExtension($setup,'manifest.json'); Manifest=Json ([IO.Path]::ChangeExtension($setup,'manifest.json'))}
}
function AssertNotStarted {
    Require (-not (Test-Path -LiteralPath $script:InstallRoot)) 'Blocked task created installation target'
    Require (-not (RegistrySnapshot).exists) 'Blocked task registered a current-user product'
    $locks = Join-Path $script:LocalRoot 'Avelin/M0/locks'
    if (Test-Path -LiteralPath $locks) { Require (@(Get-ChildItem -LiteralPath $locks -File -Filter "$($script:AppId)-*.lock").Count -eq 0) 'Blocked task created a product lock' }
}
function MachineRegistrationExists {
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine,[Microsoft.Win32.RegistryView]::Registry64)
    $key = $null
    try { $key=$base.OpenSubKey($script:RegistrySubkey,$false); return $null -ne $key }
    finally { if($key){$key.Dispose()}; $base.Dispose() }
}
try {
    NewFeatureCase 'configuration'
    $allRequirements = @'
[[prerequisites]]
kind = "vc_redist_x64"
min_version = "14.40.33810.0"
[[prerequisites]]
kind = "dotnet_desktop_x64"
min_version = "8.0.0"
[[prerequisites]]
kind = "webview2_evergreen"
min_version = "120.0.0.0"
[updates]
feed_url = "https://example.invalid/avelin/stable.json"
channel = "stable"
'@
    $config = FeatureConfig 'valid' $allRequirements
    InvokeFixtureProcess $script:CliBinary @('check','--config',$config) 'check-valid'
    AssertNotStarted
    Pass 'schema2-runtime-and-update-configuration'
    $invalid = [ordered]@{
        'unknown-runtime-field' = "[[prerequisites]]`nkind=`"vc_redist_x64`"`nmin_version=`"14.40.33810.0`"`ncommand=`"evil.exe`""
        'unknown-runtime-kind' = "[[prerequisites]]`nkind=`"custom_command`"`nmin_version=`"1.0.0`""
        'invalid-runtime-version' = "[[prerequisites]]`nkind=`"dotnet_desktop_x64`"`nmin_version=`"8.0.0-preview.1`""
        'duplicate-runtime-kind' = "[[prerequisites]]`nkind=`"dotnet_desktop_x64`"`nmin_version=`"8.0.0`"`n[[prerequisites]]`nkind=`"dotnet_desktop_x64`"`nmin_version=`"9.0.0`""
        'insecure-update-feed' = "[updates]`nfeed_url=`"http://example.invalid/feed.json`""
    }
    foreach($label in $invalid.Keys) {
        $config = FeatureConfig $label $invalid[$label]
        InvokeFixtureProcess $script:CliBinary @('check','--config',$config) "reject-$label" -ExpectedExit 1
        AssertNotStarted
        Pass "reject-$label"
    }
    NewFeatureCase 'missing-runtime'
    $config = FeatureConfig 'missing' "[[prerequisites]]`nkind=`"dotnet_desktop_x64`"`nmin_version=`"999.0.0`""
    $package = FeaturePackage 'missing' $config
    Require ($package.Manifest.prerequisites.Count -eq 1 -and $package.Manifest.prerequisites[0].min_version -ceq '999.0.0') 'Runtime requirement was lost during packaging'
    $reportPath=Join-Path $script:CaseRoot 'missing-runtime.report.json'
    InvokeFixtureProcess $package.Setup @('--silent','--install-dir',$script:InstallRoot,'--report',$reportPath) 'missing-runtime' -ExpectedExit 3
    $report=Json $reportPath
    Require ($report.result -ceq 'NotStarted' -and $report.worker_started -eq $false -and $report.diagnostic.code -ceq 'AVELIN_PREREQUISITES_UNSATISFIED') 'Missing runtime must block before worker startup'
    Require ($report.prerequisites.items.Count -eq 1 -and $report.prerequisites.items[0].status -ceq 'missing') 'Missing runtime report is not precise'
    AssertNotStarted
    Pass 'missing-runtime-blocks-before-worker-target-or-lock'

    NewFeatureCase 'no-requirements'
    $package = FeaturePackage 'plain' (FeatureConfig 'plain')
    Require ($package.Manifest.PSObject.Properties.Name -cnotcontains 'prerequisites') 'Empty prerequisite list must remain omitted'
    $reportPath=Join-Path $script:CaseRoot 'install.report.json'
    InvokeFixtureProcess $package.Setup @('--silent','--install-dir',$script:InstallRoot,'--report',$reportPath) 'install' -AwaitReport $reportPath
    Require ((Json $reportPath).result -ceq 'Succeeded' -and (RegistrySnapshot).exists) 'Unconditional package installation did not succeed'
    Require ((Hash (Join-Path $script:InstallRoot 'app.exe')) -ceq (Hash $script:SampleBinary)) 'Installed payload differs'
    Pass 'no-requirements-install'
    $reportPath=Join-Path $script:CaseRoot 'uninstall.report.json'
    InvokeFixtureProcess $script:RuntimeBinary @('--silent','--operation','uninstall','--backend',(Join-Path $script:InstallRoot 'uninstall.exe'),'--manifest',(Join-Path $script:InstallRoot '.avelin/manifest.json'),'--install-dir',$script:InstallRoot,'--report',$reportPath) 'uninstall' -AwaitReport $reportPath
    Require ((Json $reportPath).result -ceq 'Succeeded' -and -not (RegistrySnapshot).exists) 'Unconditional package uninstall did not succeed'
    Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot 'app.exe'))) 'Uninstall retained owned payload'
    Pass 'no-requirements-uninstall'
    $feedPath=Join-Path $script:CaseRoot 'stable.json'
    InvokeFixtureProcess $script:CliBinary @('update-feed','--setup',$package.Setup,'--setup-url','https://example.invalid/Example-Setup.exe','--output',$feedPath) 'create-feed'
    $feed=Json $feedPath
    Require ($feed.app_id -ceq $script:AppId -and $feed.version -ceq '1.0.0' -and $feed.platform -ceq 'windows-x64' -and $feed.channel -ceq 'stable') 'Published update identity differs'
    Require ($feed.sha256 -ceq (Hash $package.Setup) -and $feed.size -eq (Get-Item -LiteralPath $package.Setup).Length) 'Published update hash/size do not bind actual Setup bytes'
    Pass 'update-feed-binds-actual-setup'
    $badFeed=Join-Path $script:CaseRoot 'insecure.json'
    InvokeFixtureProcess $script:CliBinary @('update-feed','--setup',$package.Setup,'--setup-url','http://example.invalid/Setup.exe','--output',$badFeed) 'reject-insecure-setup-url' -ExpectedExit 1
    Require (-not (Test-Path -LiteralPath $badFeed)) 'Invalid HTTP Setup URL produced an update feed'
    Pass 'update-feed-rejects-http'
    InvokeFixtureProcess $script:CliBinary @('update-check','--setup',$package.Setup) 'reject-unconfigured-update' -ExpectedExit 1
    Require ((Get-Content -LiteralPath (Join-Path $script:CaseRoot 'reject-unconfigured-update.stderr.log') -Raw) -match 'no online update feed') 'No-feed update error is not explicit'
    Pass 'update-check-rejects-no-feed-without-network'

    if (-not $elevated) {
        NewFeatureCase 'all-users-rejection'
        $package = FeaturePackage 'machine' (FeatureConfig 'machine' '' 'all_users')
        $script:InstallRoot = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)) $package.Manifest.directory_name
        Require (-not (Test-Path -LiteralPath $script:InstallRoot) -and -not (MachineRegistrationExists)) 'Unique machine fixture already exists'
        $reportPath=Join-Path $script:CaseRoot 'non-elevated.report.json'
        InvokeFixtureProcess $package.Setup @('--silent','--install-dir',$script:InstallRoot,'--report',$reportPath) 'reject-non-elevated' -ExpectedExit 3
        Require (-not (Test-Path -LiteralPath $script:InstallRoot) -and -not (MachineRegistrationExists)) 'Non-elevated silent attempt changed machine target or registration'
        AssertNotStarted
        $errorText=(Get-Content -LiteralPath (Join-Path $script:CaseRoot 'reject-non-elevated.stderr.log') -Raw)
        Require ($errorText -match 'AVELIN_ELEVATION_REQUIRED') 'Missing elevation must return its specific startup diagnostic'
        Pass 'all-users-silent-refuses-without-elevation'
    } else {
        $summary.cases += @{label='all-users-silent-refuses-without-elevation';status='NeedsNonElevated'}
    }
    $summary.status='PASSED'
} catch {
    $summary.status='FAILED'; $summary.failure=$_.Exception.Message; throw
} finally {
    $summary.finished_at_utc=[DateTime]::UtcNow.ToString('o')
    Save (Join-Path $script:RunRoot 'summary.json') $summary
}
Write-Output ($summary | ConvertTo-Json -Depth 30)
