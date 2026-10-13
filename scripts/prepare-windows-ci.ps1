#requires -Version 7.2
<#
.SYNOPSIS
Prepare the pinned portable NSIS compiler for Windows CI.
.DESCRIPTION
Downloads the official NSIS 3.12 ZIP, checks its complete SHA-256 and the two
reviewed compiler hashes, then extracts into a new directory. No installer,
system configuration, PATH mutation or existing-directory replacement is used.
Rust builds do not need NSIS; the full CI suite uses it for migration fixtures
and the existing third-party attribution collector.
#>
[CmdletBinding()]
param([Parameter(Mandatory)][string]$OutputDirectory)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This preparation entry requires Windows.' }
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$outputRoot = [IO.Path]::GetFullPath($OutputDirectory, $projectRoot).TrimEnd([char]'\', [char]'/')
if (Test-Path -LiteralPath $outputRoot) { throw 'OutputDirectory must be new; existing files are never replaced.' }
for ($cursor = $outputRoot; $cursor; $cursor = Split-Path -Parent $cursor) {
    if (Test-Path -LiteralPath $cursor) {
        if (((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw 'OutputDirectory must not pass through a reparse point.'
        }
    }
}
$null = New-Item -ItemType Directory -Path $outputRoot -ErrorAction Stop
$archivePath = Join-Path $outputRoot 'nsis-3.12.zip'
$archiveUrl = 'https://downloads.sourceforge.net/project/nsis/NSIS%203/3.12/nsis-3.12.zip'
$archiveSha256 = '56581f90db321581c5381193d796fffcf2d24b2f8fed2160a6c6a3baa67f2c4f'
Invoke-WebRequest -Uri $archiveUrl -OutFile $archivePath -UserAgent 'Avelin-CI/1.0' -MaximumRedirection 8 -TimeoutSec 180
if ((Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant() -cne $archiveSha256) {
    throw 'Downloaded NSIS ZIP differs from the reviewed 3.12 archive; nothing was extracted.'
}

$archive = [IO.Compression.ZipFile]::OpenRead($archivePath)
try {
    if ($archive.Entries.Count -lt 1 -or $archive.Entries.Count -gt 10000) { throw 'Unexpected NSIS archive entry count.' }
    $names = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $entries = [Collections.Generic.List[object]]::new()
    $expandedBytes = 0L
    foreach ($entry in $archive.Entries) {
        $name = $entry.FullName.Replace('\', '/')
        $directory = $name.EndsWith('/')
        $relative = $name.TrimEnd([char]'/')
        if (-not $relative.StartsWith('nsis-3.12/', [StringComparison]::Ordinal) -and $relative -cne 'nsis-3.12') {
            throw 'NSIS archive entry is outside its versioned root.'
        }
        foreach ($part in $relative.Split('/')) {
            if (-not $part -or $part -in @('.', '..') -or $part -match '[<>:"|?*\x00-\x1f]' -or $part -match '[. ]$' -or $part -match '^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)') {
                throw 'NSIS archive contains an unsafe Windows path.'
            }
        }
        $unixType = ($entry.ExternalAttributes -shr 16) -band 0xf000
        if ($unixType -notin @(0, 0x4000, 0x8000) -or ($entry.ExternalAttributes -band 0x400) -ne 0) {
            throw 'NSIS archive contains a link or special file.'
        }
        if (-not $names.Add($relative)) { throw 'NSIS archive contains duplicate paths.' }
        $destination = [IO.Path]::GetFullPath((Join-Path $outputRoot $relative))
        if (-not $destination.StartsWith($outputRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'NSIS archive entry escaped OutputDirectory.'
        }
        $expandedBytes += $entry.Length
        if ($expandedBytes -gt 100MB) { throw 'NSIS archive expanded size exceeds the expected bound.' }
        $entries.Add(@{ entry = $entry; destination = $destination; directory = $directory })
    }
    foreach ($item in $entries) {
        if ($item.directory) {
            $null = [IO.Directory]::CreateDirectory($item.destination)
            continue
        }
        $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $item.destination))
        $inputStream = $item.entry.Open()
        try {
            $outputStream = [IO.File]::Open($item.destination, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            try { $inputStream.CopyTo($outputStream) } finally { $outputStream.Dispose() }
        } finally { $inputStream.Dispose() }
    }
} finally { $archive.Dispose() }

$nsisRoot = Join-Path $outputRoot 'nsis-3.12'
$compilerPins = [ordered]@{
    'makensis.exe' = 'b043e554afefbfc56315669d0b4779793aeae67f0f2a7a790e2ea91f05298eff'
    'Bin/makensis.exe' = '25d1aa7081db1a9de9690b59983f9652b7409a189c3444328cc1841eff693e8d'
}
foreach ($relative in $compilerPins.Keys) {
    if ((Get-FileHash -LiteralPath (Join-Path $nsisRoot $relative) -Algorithm SHA256).Hash.ToLowerInvariant() -cne $compilerPins[$relative]) {
        throw 'Extracted NSIS compiler differs from its reviewed bytes.'
    }
}
$version = (& (Join-Path $nsisRoot 'makensis.exe') /VERSION | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $version -cne 'v3.12') { throw 'Pinned NSIS compiler did not report v3.12.' }
$result = [ordered]@{
    schema_version = 1; status = 'PASSED'; version = $version; nsis_root = $nsisRoot
    source_url = $archiveUrl; archive_sha256 = $archiveSha256; compiler_sha256 = $compilerPins
}
$json = $result | ConvertTo-Json -Depth 6
[IO.File]::WriteAllText((Join-Path $outputRoot 'summary.json'), $json, [Text.UTF8Encoding]::new($false))
Write-Output $json
