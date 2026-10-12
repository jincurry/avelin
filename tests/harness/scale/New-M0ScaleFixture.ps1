[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$OutputRoot,
    [Parameter(Mandatory)][string]$EntryPath,
    [ValidateRange(2,50000)][int]$TotalFiles = 1000,
    [ValidateRange(0,1024)][int]$LargeFileMiB = 0,
    [int]$Seed = 19790427,
    [switch]$PlanOnly
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
function Require([bool]$Condition,[string]$Message) { if (-not $Condition) { throw $Message } }
function Absolute-Safe([string]$Path) {
    $full=[IO.Path]::GetFullPath($Path).TrimEnd('\','/')
    $at=$full
    while ($at) {
        if (Test-Path -LiteralPath $at) { Require (((Get-Item -LiteralPath $at -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "Reparse point: $at" }
        $parent=[IO.Directory]::GetParent($at)
        $at=if ($parent) { $parent.FullName } else { $null }
    }
    return $full
}
function Sha([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
$OutputRoot=Absolute-Safe $OutputRoot
$EntryPath=Absolute-Safe $EntryPath
Require (-not (Test-Path -LiteralPath $OutputRoot)) 'Fixture output must be a new directory.'
Require (Test-Path -LiteralPath $EntryPath -PathType Leaf) 'Fixed entry executable is missing.'
$smallCount=$TotalFiles-1-[int]($LargeFileMiB -gt 0)
$plan=[ordered]@{schema_version=1;status='PLAN_ONLY_NOT_EXECUTED';output_root=$OutputRoot;total_files=$TotalFiles;small_files=$smallCount;large_file_mib=$LargeFileMiB;entry_files=1;seed=$Seed;scope='Deterministic synthetic payload only. No installation or process execution.'}
if ($PlanOnly) { $plan | ConvertTo-Json; return }
$estimate=[long]$LargeFileMiB*1MB + [long]$smallCount*4468 + (Get-Item -LiteralPath $EntryPath).Length
Require (([IO.DriveInfo]::new([IO.Path]::GetPathRoot($OutputRoot))).AvailableFreeSpace -gt ($estimate+1GB)) 'Insufficient fixture generation headroom.'
$null=[IO.Directory]::CreateDirectory($OutputRoot)
$payload=Join-Path $OutputRoot 'payload'
$null=[IO.Directory]::CreateDirectory($payload)
$utf8=[Text.UTF8Encoding]::new($false)
$timer=[Diagnostics.Stopwatch]::StartNew()
$random=[Random]::new($Seed)
$files=[Collections.Generic.List[object]]::new()
function Add-FileRecord([string]$Path,[string]$Relative) {
    $files.Add([pscustomobject]@{path=$Relative;size=(Get-Item -LiteralPath $Path).Length;sha256=Sha $Path})
}
try {
    if ($LargeFileMiB -gt 0) {
        $path=Join-Path $payload '000-large.bin'
        $stream=[IO.File]::Open($path,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
        $buffer=[byte[]]::new(1MB)
        try { for ($i=0;$i -lt $LargeFileMiB;$i++) { $random.NextBytes($buffer); $stream.Write($buffer,0,$buffer.Length) } } finally { $stream.Dispose() }
        Add-FileRecord $path '000-large.bin'
    }
    for ($i=0;$i -lt $smallCount;$i++) {
        $relative='small/group-{0:D3}/item-{1:D6}.bin' -f [int][Math]::Floor($i/100),$i
        $path=Join-Path $payload $relative
        $null=[IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($path))
        $bytes=[byte[]]::new(4096+(($i%13)*31))
        $random.NextBytes($bytes)
        [IO.File]::WriteAllBytes($path,$bytes)
        Add-FileRecord $path $relative
    }
    $entryHash=Sha $EntryPath
    [IO.File]::Copy($EntryPath,(Join-Path $payload 'entry.exe'),$false)
    Add-FileRecord (Join-Path $payload 'entry.exe') 'entry.exe'
    Require ($files[$files.Count-1].sha256 -ceq $entryHash) 'Entry changed while copying.'
    Require ($files.Count -eq $TotalFiles) 'Generated total file count mismatch.'
    $inventory=@($files | Sort-Object {$_.path})
    $inventoryPath=Join-Path $OutputRoot 'inventory.json'
    [IO.File]::WriteAllText($inventoryPath,(ConvertTo-Json -InputObject $inventory -Depth 8),$utf8)
    $summary=[ordered]@{schema_version=1;status='FIXTURE_CREATED';generated_at_utc=[DateTime]::UtcNow.ToString('o');payload_root=$payload;total_files=$TotalFiles;small_files=$smallCount;large_file_mib=$LargeFileMiB;payload_bytes=($files | Measure-Object -Property size -Sum).Sum;seed=$Seed;generator='System.Random(seed).NextBytes; 1MiB large-file blocks and separate variable small-file buffers. Algorithm is tied to the recorded .NET runtime.';dotnet=[Runtime.InteropServices.RuntimeInformation]::FrameworkDescription;powershell=$PSVersionTable.PSVersion.ToString();generation_ms=$timer.ElapsedMilliseconds;entry_source=$EntryPath;entry_sha256=$entryHash;inventory_path=$inventoryPath;inventory_sha256=Sha $inventoryPath;generator_source=$PSCommandPath;generator_sha256=Sha $PSCommandPath}
    [IO.File]::WriteAllText((Join-Path $OutputRoot 'fixture.json'),($summary | ConvertTo-Json -Depth 8),$utf8)
    $summary | ConvertTo-Json -Depth 8
} catch {
    [IO.File]::WriteAllText((Join-Path $OutputRoot 'generation-failed.json'),([ordered]@{status='FAILED_RETAINED';message=$_.Exception.Message;completed_files=$files.Count}|ConvertTo-Json),$utf8)
    throw
}
