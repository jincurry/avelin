<#
.SYNOPSIS
Check connected GUI argument safety without starting a GUI, building or installing.
#>
[CmdletBinding()]
param()
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'PowerShell 7 is required.' }
$source = Join-Path $PSScriptRoot 'start-m0-ui-session.ps1'
$tokens = $null; $parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($source, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw 'UI session harness does not parse.' }
# Import only pure argument-validation functions, never the launch code.
foreach ($name in @('Assert-Condition', 'Get-ValidatedConnectedOptions')) {
    $definitions = @($ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name }, $false))
    if ($definitions.Count -ne 1) { throw "Expected one validation function: $name" }
    . ([scriptblock]::Create($definitions[0].Extent.Text))
}
$base = @('--gui', '--backend', 'C:\fixture\backend.exe', '--manifest', 'C:\fixture\manifest.json', '--install-dir', 'C:\fixture\install', '--report', 'C:\fixture\report.json', '--operation', 'install')
$passed = @()
foreach ($case in @(
    @{ name = 'legacy-no-options'; tail = @(); delay = 0; fault = 0 },
    @{ name = 'legacy-zero-delay'; tail = @('--step-delay-ms', '0'); delay = 0; fault = 0 },
    @{ name = 'legacy-observation-delay'; tail = @('--step-delay-ms', '10000'); delay = 10000; fault = 0 },
    @{ name = 'fault-only'; tail = @('--fault-after-files', '1'); delay = 0; fault = 1 },
    @{ name = 'delay-and-fault'; tail = @('--step-delay-ms', '1000', '--fault-after-files', '3'); delay = 1000; fault = 3 },
    @{ name = 'fault-and-delay'; tail = @('--fault-after-files', '1', '--step-delay-ms', '1000'); delay = 1000; fault = 1 }
)) {
    $actual = Get-ValidatedConnectedOptions ($base + $case.tail) $base 3
    Assert-Condition ($actual.delay_ms -eq $case.delay -and $actual.fault_after_files -eq $case.fault) "Wrong options: $($case.name)"
    $passed += $case.name
}
foreach ($case in @(
    @{ name = 'automatic-cancel-forbidden'; tail = @('--cancel-after-files', '1') },
    @{ name = 'unknown-option'; tail = @('--english', '1') },
    @{ name = 'duplicate-delay'; tail = @('--step-delay-ms', '1', '--step-delay-ms', '2') },
    @{ name = 'duplicate-fault'; tail = @('--fault-after-files', '1', '--fault-after-files', '2') },
    @{ name = 'missing-value'; tail = @('--fault-after-files') },
    @{ name = 'zero-fault-forbidden'; tail = @('--fault-after-files', '0') },
    @{ name = 'fault-beyond-payload'; tail = @('--fault-after-files', '4') },
    @{ name = 'negative-fault'; tail = @('--fault-after-files', '-1') },
    @{ name = 'oversized-delay'; tail = @('--step-delay-ms', '10001') },
    @{ name = 'integer-overflow'; tail = @('--fault-after-files', '999999999999999999999') },
    @{ name = 'noninteger-value'; tail = @('--fault-after-files', '1.0') },
    @{ name = 'case-changed-option'; tail = @('--Fault-After-Files', '1') },
    @{ name = 'too-many-options'; tail = @('--fault-after-files', '1', '--step-delay-ms', '1', '--fault-after-files', '2') }
)) {
    $rejected = $false
    try { $null = Get-ValidatedConnectedOptions ($base + $case.tail) $base 3 } catch { $rejected = $true }
    Assert-Condition $rejected "Unsafe options accepted: $($case.name)"
    $passed += $case.name
}
$changedBase = @($base)
$changedBase[4] = 'C:\other\manifest.json'
$rejected = $false
try { $null = Get-ValidatedConnectedOptions $changedBase $base 3 } catch { $rejected = $true }
Assert-Condition $rejected 'Changed mandatory argv accepted.'
$passed += 'mandatory-argv-change'
$prepare = Join-Path $PSScriptRoot 'prepare-m0-gui-fixture.ps1'
$plan = (& $prepare -PlanOnly -StepDelayMs 1000 -FaultAfterFiles 1) | ConvertFrom-Json
Assert-Condition ($plan.status -ceq 'PLAN_ONLY_NOT_EXECUTED' -and $plan.fault_after_files -eq 1 -and $plan.step_delay_ms -eq 1000) 'Preparation plan lost fault/delay declaration.'
$passed += 'prepare-plan-only'
[ordered]@{ status = 'PASSED'; cases = $passed.Count; passed = $passed; execution = 'Argument validation and PlanOnly only; no GUI, build or installation.' } | ConvertTo-Json -Depth 4
