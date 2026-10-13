#requires -Version 7.2
<# Component lifecycle acceptance. Uses new UUID products and paths under target.
   Shortcuts are disabled. Completed products are uninstalled by their own runtime.
   Evidence is retained; no recursive deletion. Interrupted workers are terminated
   only through handles verified against their recorded PID, creation time and image. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Runtime,
    [Parameter(Mandatory)][string]$Cli,
    [Parameter(Mandatory)][string]$RustWorker,
    [Parameter(Mandatory)][string]$OutputRoot,
    [string]$Sample = (Join-Path $PSScriptRoot '../../target/release/nimbus-demo.exe'),
    [ValidateRange(30, 1800)][int]$TimeoutSeconds = 180
)
. (Join-Path $PSScriptRoot 'run-rust-engine-interruption.ps1') -Runtime $Runtime -Cli $Cli -RustWorker $RustWorker -OutputRoot $OutputRoot -Sample $Sample -TimeoutSeconds $TimeoutSeconds -FunctionsOnly
$projectRoot = Full (Join-Path $PSScriptRoot '../..')
$script:RunRoot = Inside $OutputRoot (Join-Path $projectRoot 'target')
Require (-not (Test-Path -LiteralPath $script:RunRoot)) 'Choose a fresh OutputRoot'
$null = [IO.Directory]::CreateDirectory($script:RunRoot)
$script:RuntimeBinary = Full $Runtime; $script:CliBinary = Full $Cli
$script:WorkerBinary = Full $RustWorker; $script:SampleBinary = Full $Sample
foreach ($binary in @($script:RuntimeBinary, $script:CliBinary, $script:WorkerBinary, $script:SampleBinary)) { Require (Test-Path -LiteralPath $binary -PathType Leaf) "Missing binary: $binary" }
$script:UserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$summary = [ordered]@{ schema_version=1; status='RUNNING'; cases=@(); started_at_utc=[DateTime]::UtcNow.ToString('o'); limitations=@('Silent real-process flows; native interactive and Narrator checks are separate', 'Only explicit interruption checkpoints; no arbitrary power-loss claim') }
function Pass([string]$Label) { $summary.cases += @{label=$Label;status='PASSED'}; Save (Join-Path $script:RunRoot 'summary.json') $summary; Write-Host "[$Label] passed" }
function NewComponentCase([string]$Label) {
    $script:CaseRoot = Inside (Join-Path $script:RunRoot $Label) $script:RunRoot
    $script:InstallRoot = Join-Path $script:CaseRoot 'installed 中文'
    $script:LocalRoot = Join-Path $script:CaseRoot 'local-data'; $script:TempRoot = Join-Path $script:CaseRoot 'temp'
    foreach ($directory in @($script:CaseRoot,$script:InstallRoot,$script:LocalRoot,$script:TempRoot)) { $null=[IO.Directory]::CreateDirectory($directory) }
    $script:AppId = 'dev.avelin.components.' + [Guid]::NewGuid().ToString('N')
    $script:RegistrySubkey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Avelin.M0.' + $script:AppId
    Require (-not (RegistrySnapshot).exists) 'Unique test registration exists'
    WriteNew (Join-Path $script:InstallRoot 'user-sentinel.txt') 'Unmanaged user data'
}
function BuildComponents([string]$Label,[string]$Version,[switch]$AddC,[switch]$RemoveA,[switch]$MoveB) {
    $project=Join-Path $script:CaseRoot "project-$Label"; $payload=Join-Path $project 'payload'
    $null=[IO.Directory]::CreateDirectory($payload)
    [IO.File]::Copy($script:SampleBinary,(Join-Path $payload 'app.exe'),$false)
    WriteNew (Join-Path $payload 'core/settings.txt') "Core $Version"
    if (-not $RemoveA) { WriteNew (Join-Path $payload 'a/feature.txt') "A $Version" }
    WriteNew (Join-Path $payload 'b/feature.txt') "B $Version"
    if ($AddC) { WriteNew (Join-Path $payload 'c/feature.txt') "C $Version" }
    $corePaths=if($MoveB){'"app.exe", "core/", "b/"'}else{'"app.exe", "core/"'}
    $config=@"
schema_version = 2
[app]
id = "$($script:AppId)"
name = "Component acceptance fixture"
version = "$Version"
entry = "app.exe"
[payload]
directory = "payload"
[install]
directory_name = "AvelinComponentsFixture"
desktop_shortcut = false
start_menu_shortcut = false
[[components]]
id = "core"
required = true
name = { "zh-CN" = "主程序", "en-US" = "Application" }
description = { "zh-CN" = "基础文件", "en-US" = "Required files" }
paths = [$corePaths]
"@
    $ids=@(); if(-not $RemoveA){$ids+='a'}; if(-not $MoveB){$ids+='b'}; if($AddC){$ids+='c'}
    foreach($id in $ids){
        $default=if($id -in @('a','c')){'true'}else{'false'}
        $config += @"

[[components]]
id = "$id"
required = false
default_selected = $default
name = { "zh-CN" = "可选组件 $id", "en-US" = "Optional component $id" }
description = { "zh-CN" = "按需安装", "en-US" = "Install when needed" }
paths = ["$id/"]
"@
    }
    $configPath=Join-Path $project 'avelin.toml'; WriteNew $configPath $config
    $setup=Join-Path $script:CaseRoot "$Label-Setup.exe"
    InvokeFixtureProcess $script:CliBinary @('check','--config',$configPath) "check-$Label"
    InvokeFixtureProcess $script:CliBinary @('build','--config',$configPath,'--output',$setup,'--runtime',$script:RuntimeBinary,'--rust-worker',$script:WorkerBinary) "build-$Label"
    [pscustomobject]@{Setup=$setup;Backend=[IO.Path]::ChangeExtension($setup,'backend.exe');Manifest=Json ([IO.Path]::ChangeExtension($setup,'manifest.json'))}
}
function RunComponentOperation([object]$Package,[string]$Operation,[string]$Label,[string[]]$Extra=@(),[int]$ExpectedExit=0) {
    $report=Join-Path $script:CaseRoot "$Label.report.json"
    $exe=$Package.Setup; $arguments=@('--silent','--operation',$Operation,'--install-dir',$script:InstallRoot,'--report',$report)+$Extra
    if($Operation -eq 'uninstall'){
        $exe=$script:RuntimeBinary
        $arguments += @('--backend',(Join-Path $script:InstallRoot 'uninstall.exe'),'--manifest',(Join-Path $script:InstallRoot '.avelin/manifest.json'))
    }
    InvokeFixtureProcess $exe $arguments $Label -ExpectedExit $ExpectedExit -AwaitReport $(if($ExpectedExit -eq 0){$report}else{''})
    if(Test-Path -LiteralPath $report){Json $report}
}
function AssertSelection([object]$Package,[string[]]$Ids,[switch]$AllowChanged) {
    $receipt=Json (Join-Path $script:InstallRoot '.avelin-receipt.json')
    Require ($receipt.schema_version -eq 2 -and $receipt.app_id -ceq $script:AppId) 'Receipt schema/identity differs'
    Require ((@($receipt.selected_components|Sort-Object)-join ',') -ceq (@($Ids|Sort-Object)-join ',')) 'Receipt selection differs'
    $expected=@($Package.Manifest.files|Where-Object {$_.component_id -cin $Ids})
    Require ((@($receipt.files.path|Sort-Object)-join '|') -ceq (@($expected.path|Sort-Object)-join '|')) 'Receipt files do not equal selected resources'
    foreach($file in $receipt.files){
        Require ($file.component_id -cin $Ids) 'Unexpected component ownership'
        if(-not $AllowChanged){Require ((Hash (Join-Path $script:InstallRoot $file.path)) -ceq $file.sha256) 'Selected file content differs'}
    }
    foreach($file in @($Package.Manifest.files|Where-Object {$_.component_id -cnotin $Ids})){
        Require (-not (Test-Path -LiteralPath (Join-Path $script:InstallRoot $file.path))) 'Unselected component was deployed'
    }
}
function CleanProduct([object]$Package,[string]$Label) {
    $null=RunComponentOperation $Package 'uninstall' $Label
    Require (-not (RegistrySnapshot).exists) 'Uninstall registration remains'
    Require (Test-Path -LiteralPath (Join-Path $script:InstallRoot 'user-sentinel.txt')) 'Unmanaged sentinel removed'
}
function WaitComponentPause([object]$Running,[object]$Package,[string]$Point) {
    $timer=[Diagnostics.Stopwatch]::StartNew()
    while($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds){
        Require (-not $Running.Process.HasExited) 'Component coordinator exited before pause'
        foreach($directory in Get-ChildItem -LiteralPath $script:CaseRoot -Directory -Filter 'session-*'){
            $session=Inside $directory.FullName $script:RunRoot; $path=Join-Path $session 'engine-pause.json'
            if(-not(Test-Path -LiteralPath $path)){continue}
            try{$pause=Json $path;$identity=Json (Join-Path $session 'session.json')}catch{continue}
            if($identity.coordinator.pid -ne $Running.Process.Id){continue}
            Require ($pause.point -ceq $Point -and $pause.task_id -ceq $identity.task_id -and $identity.operation -ceq 'modify' -and $identity.app_id -ceq $script:AppId) 'Pause identity differs'
            Require ((Hash $identity.backend.executable) -ceq (Hash $Package.Backend)) 'Worker identity differs'
            $deadline=(Get-Item -LiteralPath $path).LastWriteTimeUtc.AddSeconds(27)
            Require ([DateTime]::UtcNow -lt $deadline) 'Pause deadline expired'
            return @{Session=$session;Identity=$identity;Deadline=$deadline}
        }
        Start-Sleep -Milliseconds 20
    }
    throw 'Component pause timed out; no process killed'
}
try {
    NewComponentCase 'lifecycle'; $a=BuildComponents 'a' '1.0.0'; $b=BuildComponents 'b' '1.1.0' -AddC
    foreach($bad in @('',',','a','core,core','core,unknown')){
        $before=ProductSnapshot
        $null=RunComponentOperation $a 'install' "invalid-$($summary.cases.Count)" @('--components',$bad) 3
        Require (SameSnapshot $before (ProductSnapshot)) 'Invalid selection changed resources'
        Pass "invalid-selection-$($summary.cases.Count)"
    }
    $null=RunComponentOperation $a 'install' 'install-core' @('--components','core'); AssertSelection $a @('core'); Pass 'install-core'
    $null=RunComponentOperation $a 'modify' 'add-a' @('--components','core,a'); AssertSelection $a @('core','a'); Pass 'modify-add'
    $receiptBefore=Hash (Join-Path $script:InstallRoot '.avelin-receipt.json')
    $noop=RunComponentOperation $a 'modify' 'no-changes' @('--components','core,a')
    Require ($noop.maintenance_status -ceq 'NoChanges' -and (Hash (Join-Path $script:InstallRoot '.avelin-receipt.json')) -ceq $receiptBefore) 'No-op changed receipt'
    Require ($noop.selection_source -ceq 'explicit' -and $noop.payload_file_count -eq 3 -and $noop.payload_bytes -gt 0) 'No-op selection summary missing'
    Pass 'modify-no-op'
    $null=RunComponentOperation $a 'modify' 'swap-a-b' @('--components','core,b'); AssertSelection $a @('core','b'); Pass 'modify-add-remove'
    $null=RunComponentOperation $b 'upgrade' 'upgrade-preserve'; AssertSelection $b @('core','b'); Pass 'upgrade-preserves-selection-no-new-default'
    $null=RunComponentOperation $a 'modify' 'wrong-package' @('--components','core,a') 3; AssertSelection $b @('core','b'); Pass 'modify-reject-wrong-package'
    $other=BuildComponents 'same-version-other-build' '1.1.0'
    $null=RunComponentOperation $other 'modify' 'wrong-build' @('--components','core,a') 3; AssertSelection $b @('core','b'); Pass 'modify-reject-same-version-different-build'
    $before=ProductSnapshot
    $null=RunComponentOperation $b 'modify' 'failure-before-commit' @('--components','core,a,b','--engine-fault','before-commit') 3
    Require (SameSnapshot $before (ProductSnapshot)) 'Failed modify did not restore original files and selection'; Pass 'modify-failure-restores-selection'
    foreach($operation in @('repair','recover','uninstall')){
        $before=ProductSnapshot
        $null=RunComponentOperation $b $operation "reject-selection-$operation" @('--components','core') 3
        Require (SameSnapshot $before (ProductSnapshot)) 'Fixed-scope operation accepted component override'; Pass "reject-selection-$operation"
    }
    $edited=Inside (Join-Path $script:InstallRoot 'core/settings.txt') $script:InstallRoot
    [IO.File]::WriteAllText($edited,'User settings',$script:Utf8); $editedHash=Hash $edited
    $missing=Inside (Join-Path $script:InstallRoot 'app.exe') $script:InstallRoot
    Remove-Item -LiteralPath $missing
    $null=RunComponentOperation $b 'modify' 'add-with-existing-damage' @('--components','core,a,b'); AssertSelection $b @('core','a','b') -AllowChanged
    Require ((Hash $edited) -ceq $editedHash -and -not(Test-Path -LiteralPath $missing)) 'Modify repaired or overwrote unchanged components'
    Pass 'modify-preserves-unchanged-missing-and-modified'
    $null=RunComponentOperation $b 'repair' 'repair-selected'
    Require ((Hash $edited) -ceq $editedHash -and (Test-Path -LiteralPath $missing) -and -not(Test-Path -LiteralPath (Join-Path $script:InstallRoot 'c/feature.txt'))) 'Repair selection or preservation differs'
    Pass 'repair-only-selected'
    $null=RunComponentOperation $b 'repair' 'restore-core' @('--repair-overwrite'); AssertSelection $b @('core','a','b')
    $retired=Inside (Join-Path $script:InstallRoot 'a/feature.txt') $script:InstallRoot
    [IO.File]::WriteAllText($retired,'User-owned changed optional resource',$script:Utf8); $retiredHash=Hash $retired
    $null=RunComponentOperation $b 'modify' 'remove-changed-a' @('--components','core,b')
    Require ((Hash $retired) -ceq $retiredHash) 'Changed removed component deleted'
    $before=ProductSnapshot
    $null=RunComponentOperation $b 'modify' 'readd-conflict' @('--components','core,a,b') 3
    Require (SameSnapshot $before (ProductSnapshot)) 'Readd conflict changed resources'; Pass 'modify-removal-preserves-edits-readd-refuses'
    CleanProduct $b 'uninstall-selected'
    Require ((Hash $retired) -ceq $retiredHash) 'Uninstall removed disowned user file'; Pass 'uninstall-preserves-removed-modified'

    NewComponentCase 'defaults'; $a=BuildComponents 'a' '1.0.0'
    $null=RunComponentOperation $a 'install' 'install-default'; AssertSelection $a @('core','a'); Pass 'install-default'
    $null=RunComponentOperation $a 'modify' 'install-all' @('--components','core,a,b'); AssertSelection $a @('core','a','b'); Pass 'install-all-components'
    $b=BuildComponents 'b' '1.1.0' -RemoveA -MoveB
    $null=RunComponentOperation $b 'upgrade' 'removed-and-reowned'; AssertSelection $b @('core'); Pass 'upgrade-removes-component-moves-ownership'
    CleanProduct $b 'uninstall'

    NewComponentCase 'unselected-conflict'; $a=BuildComponents 'a' '1.0.0'
    $foreign=Join-Path $script:InstallRoot 'b/feature.txt'; WriteNew $foreign 'Unmanaged optional path'; $foreignHash=Hash $foreign
    $null=RunComponentOperation $a 'install' 'ignore-unselected' @('--components','core')
    Require ((Hash $foreign) -ceq $foreignHash) 'Unselected file changed'; Pass 'install-ignores-unselected-destination'
    $before=ProductSnapshot
    $null=RunComponentOperation $a 'modify' 'unknown-add-conflict' @('--components','core,b') 3
    Require (SameSnapshot $before (ProductSnapshot)) 'Unknown component file overwritten'; Pass 'modify-rejects-unmanaged-destination'
    CleanProduct $a 'uninstall'; Require ((Hash $foreign) -ceq $foreignHash) 'Uninstall touched unselected file'; Pass 'uninstall-ignores-unselected-file'

    foreach($point in @('after-first-publish','after-first-remove','after-commit')){
        NewComponentCase "interrupt-$point"; $a=BuildComponents 'a' '1.0.0'
        $null=RunComponentOperation $a 'install' 'install'; AssertSelection $a @('core','a')
        $baseline=ProductSnapshot
        $running=StartFixtureProcess $script:RuntimeBinary @('--silent','--operation','modify','--backend',$a.Backend,'--manifest',([IO.Path]::ChangeExtension($a.Setup,'manifest.json')),'--install-dir',$script:InstallRoot,'--components','core,b','--engine-pause',$point,'--report',(Join-Path $script:CaseRoot 'interrupted.report.json')) 'interrupted'
        $evidence=WaitComponentPause $running $a $point
        $worker=$null
        try{
            $worker=[AvelinInterruption.VerifiedProcess]::new([uint32]$evidence.Identity.backend.pid,[uint64]$evidence.Identity.backend.creation_time_100ns,[string]$evidence.Identity.backend.executable)
            Require ([DateTime]::UtcNow -lt $evidence.Deadline) 'Pause ended before interruption'
            $worker.Terminate()
        }finally{if($worker){$worker.Dispose()}}
        FinishFixtureProcess $running 4
        $check=RunComponentOperation $a 'recover' 'recovery-check' @('--recovery-session',$evidence.Session)
        Require ($check.result -ceq 'Succeeded') 'Recovery inspection failed'
        Require ($check.PSObject.Properties.Name -cnotcontains 'selected_components' -and $check.PSObject.Properties.Name -cnotcontains 'payload_bytes') 'Recovery reported a wrapper selection instead of original transaction evidence'
        $null=RunComponentOperation $a 'recover' 'recovery-apply' @('--recovery-session',$evidence.Session,'--recovery-apply')
        if($point -eq 'after-commit'){AssertSelection $a @('core','b')}else{Require (SameSnapshot $baseline (ProductSnapshot)) 'Interrupted modify did not restore original product'}
        $null=RunComponentOperation $a 'recover' 'recovery-repeat' @('--recovery-session',$evidence.Session,'--recovery-apply')
        Pass "modify-recover-$point"
        CleanProduct $a 'uninstall'
    }
    $summary.status='PASSED'
}catch{$summary.status='FAILED';$summary.failure=$_.Exception.Message;throw}
finally{$summary.finished_at_utc=[DateTime]::UtcNow.ToString('o');Save (Join-Path $script:RunRoot 'summary.json') $summary}
