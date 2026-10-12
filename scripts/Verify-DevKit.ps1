#requires -Version 7.2
[CmdletBinding()]
param([string]$KitRoot = (Split-Path -Parent $PSScriptRoot))
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$KitRoot = [IO.Path]::GetFullPath($KitRoot).TrimEnd([char]'\', [char]'/')
function NoReparse([string]$Path) {
    for ($cursor = [IO.Path]::GetFullPath($Path); $cursor; $cursor = [IO.Path]::GetDirectoryName($cursor)) {
        try { $attributes = [IO.File]::GetAttributes($cursor) }
        catch [IO.FileNotFoundException] { continue }
        catch [IO.DirectoryNotFoundException] { continue }
        if ($attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse point is not a kit input: $cursor" }
    }
}
NoReparse $KitRoot
$inventoryPath = Join-Path $KitRoot 'HASHES.json'
NoReparse $inventoryPath
$inventory = Get-Content -LiteralPath $inventoryPath -Raw | ConvertFrom-Json
if ($inventory.schema_version -ne 1 -or $inventory.algorithm -cne 'SHA256' -or @($inventory.files).Count -eq 0) { throw 'Unsupported or empty kit inventory' }
$seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($record in $inventory.files) {
    $relative = [string]$record.path
    if ([IO.Path]::IsPathRooted($relative) -or $relative -match '(^|[\\/])\.\.?([\\/]|$)' -or $relative.Contains(':') -or -not $seen.Add($relative.Replace('/', '\'))) { throw "Invalid or duplicate kit inventory path: $relative" }
    $path = [IO.Path]::GetFullPath((Join-Path $KitRoot $relative))
    if (-not $path.StartsWith($KitRoot + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Inventory escaped kit root' }
    NoReparse $path
    if (-not (Test-Path -LiteralPath $path -PathType Leaf) -or (Get-Item -LiteralPath $path).Length -ne $record.size -or (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $record.sha256) { throw "Kit file mismatch: $relative" }
}
[pscustomobject]@{status='PASS';checked_files=$seen.Count;kit_root=$KitRoot;scope='Local file integrity against the bundled inventory; this is not publisher authentication or a code signature.'} | ConvertTo-Json
