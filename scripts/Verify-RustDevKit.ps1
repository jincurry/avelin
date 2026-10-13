#requires -Version 7.2
<# Read-only verification of an extracted Rust developer kit and optional ZIP.
Integrity against bundled records does not authenticate the publisher. #>
[CmdletBinding()]
param(
    [string]$KitDirectory = $PSScriptRoot,
    [string]$ZipPath = ''
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows -or -not [Environment]::Is64BitProcess) { throw 'Use Windows x64 PowerShell 7.2 or newer.' }
function Require([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Full([string]$Path) { [IO.Path]::TrimEndingDirectorySeparator([IO.Path]::GetFullPath($Path)) }
function Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function NoReparse([string]$Path) {
    for ($cursor=[IO.Path]::GetFullPath($Path); $cursor; $cursor=[IO.Path]::GetDirectoryName($cursor)) {
        $attributes=[IO.File]::GetAttributes($cursor)
        Require (-not ($attributes -band [IO.FileAttributes]::ReparsePoint)) "Reparse point: $cursor"
    }
}
function ValidRelative($Value) {
    Require ($Value -is [string] -and $Value.Length -gt 0) 'Expected nonempty relative path.'
    Require (-not [IO.Path]::IsPathRooted($Value) -and $Value -notmatch '[\\:"<>|?*\x00-\x1f]') 'Invalid relative path.'
    foreach ($segment in $Value.Split('/')) {
        Require ($segment.Length -gt 0 -and $segment -notin @('.','..') -and -not $segment.EndsWith('.') -and -not $segment.EndsWith(' ')) 'Ambiguous relative path.'
        Require ($segment -notmatch '^(?i:CON|PRN|AUX|NUL|COM[1-9¹²³]|LPT[1-9¹²³])(?:\.|$)') 'Reserved path component.'
    }
}
function Child([string]$Root, $Value) {
    ValidRelative $Value
    $path=Full (Join-Path $Root $Value)
    Require ($path.StartsWith((Full $Root)+'\',[StringComparison]::OrdinalIgnoreCase)) 'Path escaped its declared root.'
    NoReparse $path
    return $path
}
function Json([string]$Path) { NoReparse $Path; Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json -Depth 100 }
function TreeFiles([string]$Root) {
    NoReparse $Root
    $pending=[Collections.Generic.Stack[string]]::new(); $pending.Push($Root)
    while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            NoReparse $item.FullName
            if ($item.PSIsContainer) { $pending.Push($item.FullName) } else { $item }
        }
    }
}
function ValidRecord($Record) {
    ValidRelative $Record.path
    Require (($Record.size -is [int] -or $Record.size -is [long]) -and $Record.size -ge 0) 'Invalid file size.'
    Require ($Record.sha256 -is [string] -and $Record.sha256 -cmatch '^[0-9a-f]{64}$') 'Invalid SHA-256 digest.'
}
function CheckRecord([string]$Root, $Record) {
    ValidRecord $Record
    $path=Child $Root $Record.path
    Require ([IO.File]::Exists($path) -and (Get-Item -LiteralPath $path).Length -eq $Record.size -and (Hash $path) -ceq $Record.sha256) "File mismatch: $($Record.path)"
}
function ExactInventory([string]$Root, [string]$Name) {
    $inventory=Json (Child $Root $Name)
    Require ($inventory.schema_version -eq 1 -and $inventory.algorithm -ceq 'SHA256' -and @($inventory.files).Count -gt 0) 'Unsupported or empty inventory.'
    $records=[Collections.Generic.Dictionary[string,object]]::new([StringComparer]::OrdinalIgnoreCase)
    $records.Add($Name,$null)
    foreach ($record in $inventory.files) {
        CheckRecord $Root $record
        Require ($records.TryAdd($record.path,$record)) 'Duplicate inventory path.'
    }
    $actual=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($file in TreeFiles $Root) {
        $relative=[IO.Path]::GetRelativePath($Root,$file.FullName).Replace('\','/')
        Require ($records.ContainsKey($relative) -and $actual.Add($relative)) "Unexpected file: $relative"
    }
    Require ($actual.Count -eq $records.Count) 'Inventory does not describe the exact file set.'
    return ,$records
}
function PeX64([string]$Path) {
    $reader=[IO.BinaryReader]::new([IO.File]::OpenRead($Path))
    try {
        Require ($reader.ReadUInt16() -eq 0x5a4d) 'Expected PE executable.'
        $reader.BaseStream.Position=0x3c; $offset=$reader.ReadUInt32(); $reader.BaseStream.Position=$offset
        Require ($reader.ReadUInt32() -eq 0x4550 -and $reader.ReadUInt16() -eq 0x8664) 'Expected Windows x64 executable.'
    } finally { $reader.Dispose() }
}

$KitDirectory=Full $KitDirectory
NoReparse $KitDirectory
Require ([IO.Directory]::Exists($KitDirectory)) 'KitDirectory must be an extracted kit directory.'
$records=ExactInventory $KitDirectory 'HASHES.json'
$required=@(
    'README.md','LICENSE','NOTICE.md','Verify-RustDevKit.ps1','BUILD-INFO.json','SOURCE-SNAPSHOT.json',
    'bin/avelin-cli.exe','bin/avelin-app.exe','bin/avelin-engine.exe','sample/payload/nimbus-demo.exe',
    'sample/branding/nimbus.png','sample/avelin.toml','sample/Nimbus-Rust-Setup.exe',
    'sample/Nimbus-Rust-Setup.backend.exe','sample/Nimbus-Rust-Setup.manifest.json',
    'third-party/FILES.json','third-party/inventory.json','third-party/unknowns.json'
)
foreach ($name in $required) { Require ($records.ContainsKey($name)) "Missing required kit file: $name" }
$build=Json (Child $KitDirectory 'BUILD-INFO.json')
Require ($build.schema_version -eq 1 -and $build.engine -ceq 'rust' -and $build.platform -ceq 'x86_64-pc-windows-msvc') 'Unsupported build information.'
Require ($build.signed -is [bool] -and -not $build.signed) 'Expected unsigned developer kit metadata.'
Require ($build.source_snapshot -ceq 'SOURCE-SNAPSHOT.json' -and $build.source_snapshot_sha256 -ceq $records['SOURCE-SNAPSHOT.json'].sha256) 'Source snapshot digest differs.'
$componentNames=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($record in $build.components) {
    CheckRecord $KitDirectory $record
    Require ($record.path -cin @('bin/avelin-cli.exe','bin/avelin-app.exe','bin/avelin-engine.exe') -and $componentNames.Add($record.path)) 'Invalid component set.'
    PeX64 (Child $KitDirectory $record.path)
}
Require ($componentNames.Count -eq 3) 'Missing Rust kit component.'
CheckRecord $KitDirectory $build.sample.payload
Require ($build.sample.payload.path -ceq 'sample/payload/nimbus-demo.exe') 'Invalid sample payload.'
foreach ($path in @('sample/payload/nimbus-demo.exe','sample/Nimbus-Rust-Setup.exe','sample/Nimbus-Rust-Setup.backend.exe')) { PeX64 (Child $KitDirectory $path) }
Require ($build.sample.app_id -ceq 'dev.avelin.native.sample' -and $build.sample.version -ceq '1.0.0' -and $build.sample.config -ceq 'sample/avelin.toml' -and $build.sample.setup -ceq 'sample/Nimbus-Rust-Setup.exe') 'Invalid sample contract.'
$sample=Json (Child $KitDirectory 'sample/Nimbus-Rust-Setup.manifest.json')
Require ($sample.schema_version -eq 1 -and $sample.app_id -ceq $build.sample.app_id -and $sample.app_version -ceq $build.sample.version -and $sample.entry -ceq 'nimbus-demo.exe') 'Sample manifest differs.'
Require ($sample.runtime_sha256 -ceq $records['bin/avelin-app.exe'].sha256) 'Sample runtime differs from the kit runtime.'
foreach ($name in @('desktop_shortcut','start_menu_shortcut')) {
    $property=$sample.PSObject.Properties[$name]
    Require ($null -eq $property -or ($property.Value -is [bool] -and $property.Value -eq $false)) 'Sample must disable shortcuts.'
    Require ($build.sample.$name -is [bool] -and -not $build.sample.$name) 'Sample build information must disable shortcuts.'
}
Require (@($sample.files).Count -eq 1 -and $sample.files[0].path -ceq 'nimbus-demo.exe') 'Unexpected sample payload file set.'
CheckRecord (Child $KitDirectory 'sample/payload') $sample.files[0]

$source=Json (Child $KitDirectory 'SOURCE-SNAPSHOT.json')
Require ($source.schema_version -eq 1 -and $source.algorithm -ceq 'SHA256' -and @($source.files).Count -gt 0 -and $source.git_head -ceq $build.git_head) 'Unsupported source snapshot.'
$sourceNames=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($record in $source.files) {
    ValidRelative $record.path
    Require ($sourceNames.Add($record.path)) 'Duplicate source snapshot path.'
    if ($record.PSObject.Properties['state']) {
        Require ($record.state -ceq 'deleted_in_working_tree' -and -not $record.PSObject.Properties['sha256']) 'Invalid deleted source record.'
    } else { ValidRecord $record }
}
$lockRecords=@($source.files | Where-Object path -CEQ 'Cargo.lock')
Require ($lockRecords.Count -eq 1 -and $lockRecords[0].sha256 -ceq $build.cargo_lock_sha256) 'Lockfile snapshot differs.'
$docNames=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($name in $build.public_documents) {
    ValidRelative $name
    Require ([IO.Path]::GetExtension($name) -cin @('.md','.json') -and $docNames.Add("docs/$name") -and $records.ContainsKey("docs/$name")) 'Invalid public document set.'
}
Require ($docNames.Count -gt 0) 'Empty public document set.'
foreach ($name in $records.Keys) {
    if ($name.StartsWith('docs/',[StringComparison]::OrdinalIgnoreCase)) { Require ($docNames.Contains($name)) 'Unexpected public document.' }
}

$licenseRoot=Child $KitDirectory 'third-party'
$null=ExactInventory $licenseRoot 'FILES.json'
$licenses=Json (Child $licenseRoot 'inventory.json')
Require ($licenses.schema_version -eq 1 -and $licenses.cargo_lock_sha256 -ceq $build.cargo_lock_sha256 -and $licenses.project_license.spdx -ceq 'MIT') 'Third-party evidence differs from this locked workspace.'
$licenseTexts=@($licenses.project_license.license_texts)
Require ($licenseTexts.Count -eq 1 -and $licenseTexts[0].sha256 -ceq $records['LICENSE'].sha256) 'Collected project license differs.'
$metadata=@($licenses.cargo_metadata | Where-Object target -CEQ 'x86_64-pc-windows-msvc')
Require ($metadata.Count -eq 1) 'Missing x64 dependency evidence.'
foreach ($name in @('avelin-app','avelin-cli','avelin-engine')) { Require ($name -cin $metadata[0].roots) "Missing dependency root: $name" }
Require ($build.license_evidence.directory -ceq 'third-party' -and $build.license_evidence.inventory_sha256 -ceq $records['third-party/inventory.json'].sha256 -and $build.license_evidence.declared_limitations -ceq 'third-party/unknowns.json') 'License inventory digest differs.'

$zipHash=$null
if ($ZipPath) {
    $ZipPath=Full $ZipPath; NoReparse $ZipPath
    $archive=[IO.Compression.ZipFile]::OpenRead($ZipPath)
    try {
        $seen=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($entry in $archive.Entries) {
            Require ($entry.FullName.StartsWith('Avelin-Rust-DeveloperKit/',[StringComparison]::Ordinal)) 'Unexpected ZIP root.'
            $relative=$entry.FullName.Substring('Avelin-Rust-DeveloperKit/'.Length)
            ValidRelative $relative
            Require ($records.ContainsKey($relative) -and $seen.Add($relative)) 'Unexpected or duplicate ZIP entry.'
            $stream=$entry.Open()
            try { $digest=[Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($stream)).ToLowerInvariant() }
            finally { $stream.Dispose() }
            $path=Child $KitDirectory $relative
            Require ($entry.Length -eq (Get-Item -LiteralPath $path).Length -and $digest -ceq (Hash $path)) "ZIP file differs: $relative"
        }
        Require ($seen.Count -eq $records.Count) 'ZIP omitted delivered files.'
    } finally { $archive.Dispose() }
    $zipHash=Hash $ZipPath
}
[ordered]@{
    schema_version=1;verified=$true;status='PASSED';kit_path=$KitDirectory
    delivered_files=$records.Count;hashes_sha256=(Hash (Child $KitDirectory 'HASHES.json'))
    zip_verified=[bool]$ZipPath;zip_path=$ZipPath;zip_sha256=$zipHash
    installation_executed=$false
    scope='Read-only integrity and structure validation; not publisher authentication, build provenance or installation acceptance.'
} | ConvertTo-Json
