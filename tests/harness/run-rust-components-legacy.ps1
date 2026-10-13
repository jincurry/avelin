#requires -Version 7.2
<# Real schema-1 to schema-2 migration using separate old and current binaries.
   Fixtures live in a new target directory with UUID product IDs and no shortcuts.
   Original files, reports and process evidence are retained. Cleanup uses each
   installed product's matching runtime/backend; no recursive cleanup is used. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Runtime,
    [Parameter(Mandatory)][string]$Cli,
    [Parameter(Mandatory)][string]$RustWorker,
    [Parameter(Mandatory)][string]$LegacyTools,
    [Parameter(Mandatory)][string]$OutputRoot,
    [string]$Sample = (Join-Path $PSScriptRoot '../../target/release/nimbus-demo.exe'),
    [ValidateRange(30,1800)][int]$TimeoutSeconds = 180,
    # Continue a retained schema-1 fixture after fixing an acceptance assertion.
    [switch]$ResumeLegacyInstall
)
. (Join-Path $PSScriptRoot 'run-rust-engine-interruption.ps1') -Runtime $Runtime -Cli $Cli -RustWorker $RustWorker -OutputRoot $OutputRoot -Sample $Sample -TimeoutSeconds $TimeoutSeconds -FunctionsOnly
Require ($IsWindows -and [Environment]::Is64BitProcess) 'Use Windows x64 PowerShell 7.2+'
$projectRoot = Full (Join-Path $PSScriptRoot '../..')
$script:RunRoot = Inside $OutputRoot (Join-Path $projectRoot 'target')
Require ($ResumeLegacyInstall -or -not (Test-Path -LiteralPath $script:RunRoot)) 'Choose a fresh OutputRoot'
$null = [IO.Directory]::CreateDirectory($script:RunRoot)
if($ResumeLegacyInstall){
    $previousSummary=Json (Join-Path $script:RunRoot 'summary.json')
    Require ($previousSummary.status -ceq 'FAILED') 'Resume requires retained failed acceptance evidence'
    $initialSummary=Join-Path $script:RunRoot 'summary.initial-attempt.json'
    if(-not(Test-Path -LiteralPath $initialSummary)){WriteNew $initialSummary ($previousSummary|ConvertTo-Json -Depth 60)}
}
$script:RuntimeBinary=Full $Runtime; $script:CliBinary=Full $Cli; $script:WorkerBinary=Full $RustWorker; $script:SampleBinary=Full $Sample
$script:OldRuntime=Full (Join-Path $LegacyTools 'avelin-app.exe')
$script:OldCli=Full (Join-Path $LegacyTools 'avelin-cli.exe')
$script:OldWorker=Full (Join-Path $LegacyTools 'avelin-engine.exe')
$summary=[ordered]@{schema_version=1;status='RUNNING';cases=@();binaries=@();fixtures=@();started_at_utc=[DateTime]::UtcNow.ToString('o');limitations=@('Silent real-process compatibility only; no GUI or independent environment claim')}
foreach($binary in @($script:RuntimeBinary,$script:CliBinary,$script:WorkerBinary,$script:SampleBinary,$script:OldRuntime,$script:OldCli,$script:OldWorker)){
    Require (Test-Path -LiteralPath $binary -PathType Leaf) "Missing binary: $binary"
    $summary.binaries+=@{path=$binary;sha256=Hash $binary}
}
if($ResumeLegacyInstall){
    foreach($binary in @($summary.binaries|Where-Object {$_.path -cin @($script:OldRuntime,$script:OldCli,$script:OldWorker,$script:SampleBinary)})){
        Require (@($previousSummary.binaries|Where-Object {$_.path -ceq $binary.path -and $_.sha256 -ceq $binary.sha256}).Count -eq 1) 'Legacy fixture binary differs from the initial attempt'
    }
}
function Pass([string]$Label){$summary.cases+=@{label=$Label;status='PASSED'};Save (Join-Path $script:RunRoot 'summary.json') $summary;Write-Host "[$Label] passed"}
function NewLegacyCase([string]$Label){
    $script:CaseRoot=Inside (Join-Path $script:RunRoot $Label) $script:RunRoot
    $script:InstallRoot=Join-Path $script:CaseRoot 'installed 中文'
    $script:LocalRoot=Join-Path $script:CaseRoot 'local-data';$script:TempRoot=Join-Path $script:CaseRoot 'temp'
    foreach($directory in @($script:CaseRoot,$script:InstallRoot,$script:LocalRoot,$script:TempRoot)){$null=[IO.Directory]::CreateDirectory($directory)}
    $receiptPath=Join-Path $script:InstallRoot '.avelin-receipt.json'
    $resume=$ResumeLegacyInstall -and (Test-Path -LiteralPath $receiptPath)
    if($resume){
        $prior=Json $receiptPath
        Require ($prior.schema_version -eq 1 -and $prior.app_version -ceq '1.0.0') 'Resume requires an unchanged legacy installation'
        Require (@($previousSummary.fixtures|Where-Object {$_.app_id -ceq $prior.app_id -and (SamePath $_.install_dir $script:InstallRoot)}).Count -eq 1) 'Resume installation differs from recorded fixture'
        $script:AppId=$prior.app_id
    }else{$script:AppId='dev.avelin.component-legacy.'+[Guid]::NewGuid().ToString('N')}
    $script:RegistrySubkey='Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.'+$script:AppId
    Require ($resume -or -not (RegistrySnapshot).exists) 'Unique fixture registration exists'
    if(-not $resume){WriteNew (Join-Path $script:InstallRoot 'user-sentinel.txt') 'Unmanaged fixture data'}
    $summary.fixtures+=@{label=$Label;app_id=$script:AppId;install_dir=$script:InstallRoot}
}
function BuildMigrationPackage([string]$Label,[switch]$Legacy,[switch]$Mixed){
    $project=Join-Path $script:CaseRoot "project-$Label";$payload=Join-Path $project 'payload'
    $null=[IO.Directory]::CreateDirectory($payload)
    [IO.File]::Copy($script:SampleBinary,(Join-Path $payload 'app.exe'),$false)
    WriteNew (Join-Path $payload 'all-old/resource.txt') $(if($Legacy){'Legacy resource'}else{'Upgraded resource'})
    if($Mixed){WriteNew (Join-Path $payload 'mixed/old.txt') $(if($Legacy){'Legacy mixed resource'}else{'Upgraded mixed resource'})}
    if(-not $Legacy){
        WriteNew (Join-Path $payload 'all-new/resource.txt') 'New default-selected component'
        if($Mixed){WriteNew (Join-Path $payload 'mixed/new.txt') 'New file in formerly partial component'}
    }
    $schema=if($Legacy){1}else{2};$version=if($Legacy){'1.0.0'}else{'2.0.0'}
    $config=@"
schema_version = $schema
[app]
id = "$($script:AppId)"
name = "Component legacy migration fixture"
version = "$version"
entry = "app.exe"
[payload]
directory = "payload"
[install]
directory_name = "AvelinLegacyComponentsFixture"
desktop_shortcut = false
start_menu_shortcut = false
"@
    if(-not $Legacy){
        $config+=@"

[[components]]
id = "core"
required = true
name = { "zh-CN" = "主程序", "en-US" = "Application" }
description = { "zh-CN" = "基础文件", "en-US" = "Required files" }
paths = ["app.exe"]
"@
        $componentIds=@('all-old','all-new');if($Mixed){$componentIds+='mixed'}
        foreach($id in $componentIds){
            $config+=@"

[[components]]
id = "$id"
required = false
default_selected = true
name = { "zh-CN" = "可选组件 $id", "en-US" = "Optional component $id" }
description = { "zh-CN" = "兼容验收", "en-US" = "Compatibility fixture" }
paths = ["$id/"]
"@
        }
    }
    $configPath=Join-Path $project 'avelin.toml';WriteNew $configPath $config
    $setup=Join-Path $script:CaseRoot "$Label-Setup.exe"
    $builder=if($Legacy){$script:OldCli}else{$script:CliBinary}
    $runtimePath=if($Legacy){$script:OldRuntime}else{$script:RuntimeBinary}
    $workerPath=if($Legacy){$script:OldWorker}else{$script:WorkerBinary}
    InvokeFixtureProcess $builder @('check','--config',$configPath) "check-$Label"
    InvokeFixtureProcess $builder @('build','--config',$configPath,'--output',$setup,'--runtime',$runtimePath,'--rust-worker',$workerPath) "build-$Label"
    $manifest=Json ([IO.Path]::ChangeExtension($setup,'manifest.json'))
    if($Legacy){Require ($manifest.schema_version -eq 1) 'Legacy CLI emitted a non-schema-1 manifest; this is not a legacy compatibility test'}
    else{Require ($manifest.schema_version -eq 2) 'New package is not schema 2'}
    [pscustomobject]@{Setup=$setup;Manifest=$manifest}
}
function RunMigration([object]$Package,[string]$Operation,[string]$Label,[string[]]$Extra=@(),[int]$ExpectedExit=0){
    $report=Join-Path $script:CaseRoot "$Label.report.json"
    InvokeFixtureProcess $Package.Setup (@('--silent','--operation',$Operation,'--install-dir',$script:InstallRoot,'--report',$report)+$Extra) $Label -ExpectedExit $ExpectedExit -AwaitReport $(if($ExpectedExit -eq 0){$report}else{''})
    if(Test-Path -LiteralPath $report){Json $report}
}
function AssertMigrated([object]$Package,[object]$Before,[string[]]$Selection){
    $receipt=Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    Require ($receipt.schema_version -eq 2 -and $receipt.app_id -ceq $script:AppId -and $receipt.app_version -ceq '2.0.0') 'Migrated receipt schema or product identity differs'
    Require ($receipt.installation_id -ceq $Before.installation_id -and $receipt.previous_commit_id -ceq $Before.commit_id -and $receipt.commit_id -cne $Before.commit_id) 'Migration did not preserve installation and commit chain'
    Require ((@($receipt.selected_components|Sort-Object)-join ',') -ceq (@($Selection|Sort-Object)-join ',')) 'Migrated selection differs'
    $expected=@($Package.Manifest.files|Where-Object {$_.component_id -cin $Selection})
    Require ((@($receipt.files.path|Sort-Object)-join '|') -ceq (@($expected.path|Sort-Object)-join '|')) 'Receipt does not equal selected manifest resources'
    foreach($file in $receipt.files){Require ((Hash (Join-Path $script:InstallRoot $file.path)) -ceq $file.sha256) 'Migrated resource content differs'}
    foreach($file in @($Package.Manifest.files|Where-Object {$_.component_id -cnotin $Selection})){
        Require (-not(Test-Path -LiteralPath (Join-Path $script:InstallRoot $file.path))) 'Unselected component was installed'
    }
    Require ($null -eq $receipt.PSObject.Properties['shortcuts'] -or @($receipt.shortcuts).Count -eq 0) 'Unexpected shortcut was created'
}
function CleanLegacyProduct([string]$Label){
    $manifestPath=Join-Path $script:InstallRoot '.avelin/manifest.json'
    $manifest=Json $manifestPath
    $runtimePath=if($manifest.schema_version -eq 1){$script:OldRuntime}else{$script:RuntimeBinary}
    $report=Join-Path $script:CaseRoot "$Label.report.json"
    InvokeFixtureProcess $runtimePath @('--silent','--operation','uninstall','--backend',(Join-Path $script:InstallRoot 'uninstall.exe'),'--manifest',$manifestPath,'--install-dir',$script:InstallRoot,'--report',$report) $Label -AwaitReport $report
    Require (-not (RegistrySnapshot).exists) 'Fixture registration remains after uninstall'
    Require (-not(Test-Path -LiteralPath (Join-Path $script:InstallRoot '.avelin-receipt.json'))) 'Fixture receipt remains after uninstall'
    Require (Test-Path -LiteralPath (Join-Path $script:InstallRoot 'user-sentinel.txt')) 'Unmanaged sentinel was deleted'
}
try{
    foreach($mixed in @($false,$true)){
        $label=if($mixed){'mixed-paths'}else{'deterministic-paths'}
        NewLegacyCase $label
        if($ResumeLegacyInstall -and (Test-Path -LiteralPath (Join-Path $script:InstallRoot '.avelin-receipt.json'))){
            $old=[pscustomobject]@{Setup=(Join-Path $script:CaseRoot 'old-Setup.exe');Manifest=(Json (Join-Path $script:CaseRoot 'old-Setup.manifest.json'))}
            Require ($old.Manifest.schema_version -eq 1 -and $old.Manifest.app_id -ceq $script:AppId) 'Retained old package is not this schema-1 fixture'
        }else{
            $old=BuildMigrationPackage 'old' -Legacy -Mixed:$mixed
            $null=RunMigration $old 'install' 'legacy-install'
        }
        $before=Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
        Require ($before.schema_version -eq 1) 'Old runtime did not commit a schema-1 receipt'
        Require ($null -eq $before.PSObject.Properties['selected_components']) 'Old receipt unexpectedly contains component choices'
        Require ($null -eq $before.PSObject.Properties['shortcuts'] -or @($before.shortcuts).Count -eq 0) 'Legacy installation created shortcuts'
        Pass "$label-real-schema1-install"
        $new=BuildMigrationPackage 'new' -Mixed:$mixed
        if($mixed){
            $snapshot=ProductSnapshot;Save (Join-Path $script:CaseRoot 'before-rejected-upgrade.json') $snapshot
            $null=RunMigration $new 'upgrade' 'ambiguous-upgrade' @() 3
            Require (SameSnapshot $snapshot (ProductSnapshot)) 'Ambiguous migration changed installation files or registration'
            $diagnostic=[IO.File]::ReadAllText((Join-Path $script:CaseRoot 'ambiguous-upgrade.stderr.log'))
            Require ($diagnostic -match 'explicit|components|mixed') 'Failure did not identify component migration ambiguity'
            Pass 'mixed-paths-implicit-selection-rejected-without-mutation'
            $null=RunMigration $new 'upgrade' 'explicit-upgrade' @('--components','core,all-old,mixed')
            AssertMigrated $new $before @('core','all-old','mixed')
            Pass 'mixed-paths-explicit-selection-succeeds'
        }else{
            $null=RunMigration $new 'upgrade' 'inherited-upgrade'
            AssertMigrated $new $before @('core','all-old')
            Pass 'all-old-component-inherited'
            Pass 'all-new-default-selected-component-stays-unselected'
        }
        CleanLegacyProduct 'cleanup-uninstall'
        Pass "$label-uninstalled-with-unmanaged-data-preserved"
    }
    $summary.status='PASSED'
}catch{$summary.status='FAILED';$summary.failure=$_.Exception.Message;throw}
finally{$summary.finished_at_utc=[DateTime]::UtcNow.ToString('o');Save (Join-Path $script:RunRoot 'summary.json') $summary}
