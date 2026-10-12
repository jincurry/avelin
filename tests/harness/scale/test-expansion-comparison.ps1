[CmdletBinding()]
param(
    [string]$ProjectRoot = (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))),
    [string]$OutputRoot,
    [switch]$RunCompilerSmoke
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $OutputRoot) {
    $OutputRoot = Join-Path $ProjectRoot ('target\m0-expansion-tests\' + [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
}
if (Test-Path -LiteralPath $OutputRoot) { throw 'Test evidence directory already exists.' }
$null = [IO.Directory]::CreateDirectory($OutputRoot)
$harness = Join-Path $ProjectRoot 'tests\harness\run-m0-expansion-comparison.ps1'
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($harness, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw ($parseErrors.Message -join '; ') }
foreach ($function in $ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] }, $false)) {
    . ([scriptblock]::Create($function.Extent.Text))
}
function Require([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Require-Throws([scriptblock]$Action, [string]$Label) {
    $threw = $false
    try { & $Action } catch { $threw = $true }
    Require $threw "Expected rejection: $Label"
}
$groups = [Collections.Generic.List[object]]::new()
$summary = [ordered]@{ status = 'RUNNING'; powershell = $PSVersionTable.PSVersion.ToString(); harness_sha256 = Get-ExpansionHash $harness; groups = @(); compiler_smoke = 'NOT_REQUESTED' }
try {
    $planOutput = Join-Path $OutputRoot 'plan-must-not-exist'
    $planText = & (Join-Path $PSHOME 'pwsh.exe') -NoProfile -NonInteractive -File $harness -PayloadRoot (Join-Path $OutputRoot 'absent-input') -OutputRoot $planOutput -PlanOnly
    Require ($LASTEXITCODE -eq 0) 'PlanOnly failed.'
    $plan = $planText | ConvertFrom-Json
    Require ($plan.status -ceq 'PLAN_ONLY_NOT_EXECUTED' -and -not (Test-Path -LiteralPath $planOutput)) 'PlanOnly created output or reported execution.'
    $groups.Add(@{ name = 'plan_without_io'; status = 'PASSED' })

    Require ((ConvertTo-ExpansionNsisLiteral 'C:\$INSTDIR\a"b') -ceq 'C:\$$INSTDIR\a$\"b') 'NSIS literal escaping is wrong.'
    Require-Throws { ConvertTo-ExpansionNsisLiteral "bad`npath" } 'control character'
    $groups.Add(@{ name = 'literal_escape_and_control_rejection'; status = 'PASSED' })

    $fixture = Join-Path $OutputRoot 'fixture with $dollar'
    $null = [IO.Directory]::CreateDirectory((Join-Path $fixture 'nested\child'))
    [IO.File]::WriteAllBytes((Join-Path $fixture 'root.bin'), [byte[]](0, 3, 255, 5))
    [IO.File]::WriteAllText((Join-Path $fixture 'nested\space name.txt'), "Actual NSIS extraction`r`n")
    [IO.File]::WriteAllText((Join-Path $fixture 'nested\child\third.txt'), 'payload')
    $inventory = @(Get-ExpansionInventory $fixture | Sort-Object path -CaseSensitive)
    Require ($inventory.Count -eq 3) 'Fixture inventory missed a nested file.'
    Assert-ExpansionInventory $inventory @($inventory)
    Require-Throws { Assert-ExpansionInventory $inventory @($inventory[0], $inventory[1]) } 'missing file'
    $changed = @($inventory | ForEach-Object { [pscustomobject]@{ path = $_.path; size = $_.size; sha256 = $_.sha256 } })
    $changed[0].sha256 = 'different'
    Require-Throws { Assert-ExpansionInventory $inventory $changed } 'changed hash'
    Require-Throws { Assert-ExpansionInventory $inventory @($inventory[0], $inventory[0], $inventory[2]) } 'duplicate path'
    $groups.Add(@{ name = 'inventory_detects_missing_mutated_duplicate'; status = 'PASSED' })

    $eventsPath = Join-Path $OutputRoot 'events.txt'
    [IO.File]::WriteAllText($eventsPath, "100|begin|0|2`r`n101|progress|1|2`r`n1200|progress|2|2`r`n1201|finish|2|2`r`n")
    $events = Get-ExpansionEvents $eventsPath 'per-file' 2
    Require ($events.event_count -eq 4 -and $events.largest_progress_step_files -eq 1 -and $events.longest_progress_gap_ms -eq 1099 -and $events.peak_events_in_one_second_bucket -eq 2) 'Event measurements are wrong.'
    [IO.File]::WriteAllText($eventsPath, "100|begin|0|2`r`n101|progress|2|2`r`n102|finish|2|2`r`n")
    $events = Get-ExpansionEvents $eventsPath 'recursive' 2
    Require ($events.largest_progress_step_files -eq 2) 'Recursive extraction should report one whole-payload step.'
    Require-Throws { Get-ExpansionEvents $eventsPath 'per-file' 2 } 'skipped per-file callback'
    [IO.File]::WriteAllText($eventsPath, "100|begin|0|2`r`n105|progress|2|2`r`n102|finish|2|2`r`n")
    Require-Throws { Get-ExpansionEvents $eventsPath 'recursive' 2 } 'backwards finish time'
    $groups.Add(@{ name = 'event_counts_sequence_granularity_and_rate'; status = 'PASSED' })

    $perFile = New-ExpansionScript 'per-file' $fixture 'C:\new\target' 'C:\new\events.log' 'C:\new\backend.exe' $inventory
    $recursive = New-ExpansionScript 'recursive' $fixture 'C:\new\target' 'C:\new\events.log' 'C:\new\backend.exe' $inventory
    Require ($perFile.Contains('SilentInstall silent') -and $recursive.Contains('RequestExecutionLevel user')) 'Fixture execution policy changed.'
    Require (-not $perFile.Contains('File /r') -and $recursive.Contains('File /r')) 'The two script modes were not separated.'
    Require ($recursive.Contains('$$dollar')) 'Source path dollar was not protected.'
    Require (-not $perFile.Contains('WriteReg') -and -not $recursive.Contains('WriteUninstaller')) 'Isolated fixture gained product side effects.'
    $groups.Add(@{ name = 'script_mode_and_isolation_contract'; status = 'PASSED' })

    if ($RunCompilerSmoke) {
        $smokeOutput = Join-Path $OutputRoot 'compiler-smoke'
        & (Join-Path $PSHOME 'pwsh.exe') -NoProfile -NonInteractive -File $harness -PayloadRoot $fixture -OutputRoot $smokeOutput -TimeoutSeconds 60 -SampleIntervalMs 50 -DirectorySampleIntervalMs 250 *> (Join-Path $OutputRoot 'compiler-smoke.log')
        Require ($LASTEXITCODE -eq 0) 'Actual compiler/extraction smoke failed; inspect compiler-smoke.log.'
        $smoke = Get-Content -LiteralPath (Join-Path $smokeOutput 'summary.json') -Raw | ConvertFrom-Json
        Require ($smoke.status -ceq 'PASSED_SCOPED_EXPANSION_COMPARISON' -and $smoke.cases.Count -eq 2) 'Actual compiler/extraction smoke was incomplete.'
        Require (@($smoke.cases | Where-Object { $_.verified_files -ne 3 }).Count -eq 0) 'Actual nested extraction inventory differs.'
        $summary.compiler_smoke = 'PASSED_BOTH_MODES_THREE_FILES'
        $groups.Add(@{ name = 'actual_nsis_nested_payload_both_modes'; status = 'PASSED' })
    }
    $summary.status = 'PASSED'
}
catch { $summary.status = 'FAILED'; $summary.failure = $_.Exception.Message; throw }
finally {
    $summary.groups = $groups.ToArray()
    Save-ExpansionJson (Join-Path $OutputRoot 'summary.json') $summary
    Write-Host "$($summary.status): $($groups.Count) expansion comparison test groups — $OutputRoot"
}
