#requires -Version 7.2
<# Package supplied Windows x64 binaries into a new Rust developer kit.
No source compilation, installation, signing or publication is performed. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$CliPath,
    [Parameter(Mandatory)][string]$RuntimePath,
    [Parameter(Mandatory)][string]$RustWorkerPath,
    [Parameter(Mandatory)][string]$SamplePath,
    [Parameter(Mandatory)][string]$LicenseDirectory,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [string]$ProjectRoot = (Split-Path -Parent $PSScriptRoot)
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows -or -not [Environment]::Is64BitProcess) { throw 'Use Windows x64 PowerShell 7.2 or newer.' }
function Full([string]$Path) { [IO.Path]::TrimEndingDirectorySeparator([IO.Path]::GetFullPath($Path)) }
function Require([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function NoReparse([string]$Path) {
    for ($cursor = [IO.Path]::GetFullPath($Path); $cursor; $cursor = [IO.Path]::GetDirectoryName($cursor)) {
        try { $attributes = [IO.File]::GetAttributes($cursor) }
        catch [IO.FileNotFoundException] { continue }
        catch [IO.DirectoryNotFoundException] { continue }
        Require (-not ($attributes -band [IO.FileAttributes]::ReparsePoint)) "Reparse point: $cursor"
    }
}
function Relative([string]$Root, [string]$Path) {
    $value = [IO.Path]::GetRelativePath($Root, $Path).Replace('\','/')
    Require (-not [IO.Path]::IsPathRooted($value) -and $value -notmatch '(^|/)\.\.(/|$)' -and -not $value.Contains(':')) 'Path escaped its declared root.'
    return $value
}
function Child([string]$Root, [string]$RelativePath) {
    Require ($RelativePath.Length -gt 0 -and -not [IO.Path]::IsPathRooted($RelativePath) -and $RelativePath -notmatch '[\\:"<>|?*\x00-\x1f]') 'Invalid relative evidence path.'
    foreach ($segment in $RelativePath.Split('/')) {
        Require ($segment.Length -gt 0 -and $segment -notin @('.','..') -and -not $segment.EndsWith('.') -and -not $segment.EndsWith(' ')) 'Ambiguous relative evidence path.'
        Require ($segment -notmatch '^(?i:CON|PRN|AUX|NUL|COM[1-9¹²³]|LPT[1-9¹²³])(?:\.|$)') 'Reserved path component.'
    }
    $path = Full (Join-Path $Root $RelativePath)
    Require ($path.StartsWith((Full $Root)+'\',[StringComparison]::OrdinalIgnoreCase)) 'Evidence escaped its root.'
    NoReparse $path
    return $path
}
function NewText([string]$Path, [string]$Value) {
    NoReparse $Path
    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $Path))
    $stream = [IO.File]::Open($Path,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
    try { $bytes = [Text.UTF8Encoding]::new($false).GetBytes($Value); $stream.Write($bytes,0,$bytes.Length); $stream.Flush($true) }
    finally { $stream.Dispose() }
}
function NewJson([string]$Path, $Value) { NewText $Path ($Value | ConvertTo-Json -Depth 80) }
function CopyNew([string]$Source, [string]$Destination) {
    NoReparse $Source; NoReparse $Destination
    Require ([IO.File]::Exists($Source)) "Missing input: $Source"
    $before = Hash $Source
    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $Destination))
    [IO.File]::Copy($Source,$Destination,$false)
    Require ((Hash $Destination) -ceq $before -and (Hash $Source) -ceq $before) "Copy changed: $Source"
}
function TreeFiles([string]$Root) {
    NoReparse $Root
    $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push((Full $Root))
    while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            NoReparse $item.FullName
            if ($item.PSIsContainer) { $pending.Push($item.FullName) } else { $item }
        }
    }
}
function CopyTree([string]$Source, [string]$Destination) {
    foreach ($item in TreeFiles $Source) { CopyNew $item.FullName (Child $Destination (Relative $Source $item.FullName)) }
}
function Record([string]$Root, [string]$Path) {
    NoReparse $Path
    [ordered]@{path=(Relative $Root $Path);size=(Get-Item -LiteralPath $Path).Length;sha256=(Hash $Path)}
}
function Run([string]$Executable, [string[]]$Arguments, [string]$Directory, [int]$TimeoutMs=300000) {
    $info = [Diagnostics.ProcessStartInfo]::new($Executable)
    $info.UseShellExecute=$false; $info.CreateNoWindow=$true; $info.WindowStyle=[Diagnostics.ProcessWindowStyle]::Hidden
    $info.WorkingDirectory=$Directory; $info.RedirectStandardOutput=$true; $info.RedirectStandardError=$true
    $info.StandardOutputEncoding=[Text.UTF8Encoding]::new($false); $info.StandardErrorEncoding=[Text.UTF8Encoding]::new($false)
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $process=[Diagnostics.Process]::new(); $process.StartInfo=$info
    try {
        $null=$process.Start(); $stdout=$process.StandardOutput.ReadToEndAsync(); $stderr=$process.StandardError.ReadToEndAsync()
        Require ($process.WaitForExit($TimeoutMs)) 'Packaging command timed out; its output and process are retained for inspection.'
        $result=[ordered]@{exit_code=$process.ExitCode;stdout=$stdout.GetAwaiter().GetResult();stderr=$stderr.GetAwaiter().GetResult()}
        Require ($result.exit_code -eq 0) "Packaging command failed: $($result.stderr)"
        return $result
    } finally { $process.Dispose() }
}
function PeX64([string]$Path) {
    NoReparse $Path
    $reader=[IO.BinaryReader]::new([IO.File]::OpenRead($Path))
    try {
        Require ($reader.ReadUInt16() -eq 0x5a4d) 'Expected PE executable.'
        $reader.BaseStream.Position=0x3c; $offset=$reader.ReadUInt32(); $reader.BaseStream.Position=$offset
        Require ($reader.ReadUInt32() -eq 0x4550 -and $reader.ReadUInt16() -eq 0x8664) 'Expected Windows x64 executable.'
    } finally { $reader.Dispose() }
}
function SourceFiles {
    $result=Run 'git' @('-c','core.quotepath=false','ls-files','--cached','--others','--exclude-standard','-z') $ProjectRoot
    @($result.stdout.Split([char]0,[StringSplitOptions]::RemoveEmptyEntries) | Sort-Object -Unique)
}

$ProjectRoot=Full $ProjectRoot
$LicenseDirectory=Full $LicenseDirectory; $output=Full $OutputDirectory
NoReparse $ProjectRoot; NoReparse $LicenseDirectory; NoReparse $output
Require (-not (Test-Path -LiteralPath $output)) 'OutputDirectory must be new; failed output is retained for inspection.'
# In-repository output must be ignored so generated files cannot enter the source snapshot.
if ($output.StartsWith($ProjectRoot+'\',[StringComparison]::OrdinalIgnoreCase)) {
    $relativeOutput=Relative $ProjectRoot $output
    $null=Run 'git' @('check-ignore','--quiet','--no-index','--',($relativeOutput+'/')) $ProjectRoot
}
$candidateId=[guid]::NewGuid().ToString('N')
$components=[ordered]@{
    'avelin-cli.exe'=(Full $CliPath)
    'avelin-app.exe'=(Full $RuntimePath)
    'avelin-engine.exe'=(Full $RustWorkerPath)
    'nimbus-demo.exe'=(Full $SamplePath)
}
$inputComponents=@(foreach ($name in $components.Keys) {
    $path=$components[$name]; PeX64 $path
    [ordered]@{path=$name;size=(Get-Item -LiteralPath $path).Length;sha256=(Hash $path)}
})
$licenseInventory=Get-Content -LiteralPath (Join-Path $LicenseDirectory 'inventory.json') -Raw | ConvertFrom-Json
$licenseFiles=Get-Content -LiteralPath (Join-Path $LicenseDirectory 'FILES.json') -Raw | ConvertFrom-Json
Require ($licenseInventory.schema_version -eq 1 -and $licenseFiles.schema_version -eq 1 -and $licenseFiles.algorithm -ceq 'SHA256' -and @($licenseFiles.files).Count -gt 0) 'Invalid license evidence.'
Require ($licenseInventory.cargo_lock_sha256 -ceq (Hash (Join-Path $ProjectRoot 'Cargo.lock'))) 'License evidence does not match the current lockfile.'
Require ($licenseInventory.cargo_lock_sha256 -cmatch '^[0-9a-f]{64}$') 'Invalid license lockfile digest.'
Require ($licenseInventory.project_license.spdx -ceq 'MIT') 'Expected MIT project license.'
$metadata=@($licenseInventory.cargo_metadata | Where-Object target -CEQ 'x86_64-pc-windows-msvc')
Require ($metadata.Count -eq 1) 'Missing x64 dependency evidence.'
foreach ($name in @('avelin-app','avelin-cli','avelin-engine')) { Require ($name -cin $metadata[0].roots) "Missing dependency root: $name" }
$projectLicenseRecords=@($licenseInventory.project_license.license_texts)
Require ($projectLicenseRecords.Count -eq 1 -and $projectLicenseRecords[0].sha256 -ceq (Hash (Join-Path $ProjectRoot 'LICENSE'))) 'Collected project license differs.'
$licenseSeen=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$null=$licenseSeen.Add('FILES.json')
foreach ($entry in $licenseFiles.files) {
    Require ($entry.path -is [string] -and $licenseSeen.Add($entry.path)) 'Duplicate license evidence path.'
    Require (($entry.size -is [int] -or $entry.size -is [long]) -and $entry.size -ge 0 -and $entry.sha256 -cmatch '^[0-9a-f]{64}$') 'Invalid license file record.'
    $path=Child $LicenseDirectory $entry.path
    Require ((Get-Item -LiteralPath $path).Length -eq $entry.size -and (Hash $path) -ceq $entry.sha256) "License evidence changed: $($entry.path)"
}
$actualLicenseFiles=@(TreeFiles $LicenseDirectory)
Require ($actualLicenseFiles.Count -eq $licenseSeen.Count) 'License inventory does not describe its exact file set.'
foreach ($file in $actualLicenseFiles) { Require ($licenseSeen.Contains((Relative $LicenseDirectory $file.FullName))) 'Unlisted license evidence file.' }
$sourceNames=@(SourceFiles)
$sourceRecords=@(foreach ($relative in $sourceNames) {
    $path=Child $ProjectRoot $relative
    if ([IO.File]::Exists($path)) { Record $ProjectRoot $path }
    else { [ordered]@{path=$relative.Replace('\','/');state='deleted_in_working_tree'} }
})
$revision=(Run 'git' @('rev-parse','HEAD') $ProjectRoot).stdout.Trim()
$workingChanges=(Run 'git' @('status','--porcelain=v1','--untracked-files=all') $ProjectRoot).stdout
$createdAt=[DateTime]::UtcNow.ToString('o')
$kit=Join-Path $output 'Avelin-Rust-DeveloperKit'
$null=[IO.Directory]::CreateDirectory($kit)
foreach ($name in @('avelin-cli.exe','avelin-app.exe','avelin-engine.exe')) { CopyNew $components[$name] (Join-Path $kit "bin/$name") }
CopyNew $components['nimbus-demo.exe'] (Join-Path $kit 'sample/payload/nimbus-demo.exe')
CopyNew (Join-Path $ProjectRoot 'examples/nimbus/branding/nimbus.png') (Join-Path $kit 'sample/branding/nimbus.png')
CopyNew (Join-Path $ProjectRoot 'scripts/Verify-RustDevKit.ps1') (Join-Path $kit 'Verify-RustDevKit.ps1')
foreach ($name in @('LICENSE','NOTICE.md')) { CopyNew (Join-Path $ProjectRoot $name) (Join-Path $kit $name) }
$publicDocs=Get-Content -LiteralPath (Join-Path $ProjectRoot 'packaging/public-docs.json') -Raw | ConvertFrom-Json
Require ($publicDocs.schema_version -eq 1 -and @($publicDocs.files).Count -gt 0) 'Invalid public documentation whitelist.'
$docNames=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($name in $publicDocs.files) {
    Require ($name -is [string] -and [IO.Path]::GetExtension($name) -cin @('.md','.json') -and $docNames.Add($name)) 'Invalid public document path.'
    CopyNew (Child (Join-Path $ProjectRoot 'docs') $name) (Child (Join-Path $kit 'docs') $name)
}
CopyTree $LicenseDirectory (Join-Path $kit 'third-party')
NewText (Join-Path $kit 'sample/avelin.toml') @'
schema_version = 1

[app]
id = "dev.avelin.native.sample"
name = "Nimbus 笔记 · Rust 预览"
version = "1.0.0"
entry = "nimbus-demo.exe"

[payload]
directory = "payload"

[install]
directory_name = "AvelinRustNimbus"
desktop_shortcut = false
start_menu_shortcut = false

[ui]
template = "compact"
theme = "light"
locale = "zh-CN"
fallback_locale = "en-US"
accent_color = "#245DA8"
logo = "branding/nimbus.png"
'@

# Build with the copied CLI. Its sibling runtime and worker are found without overrides.
# Keep its immutable staging evidence outside the delivered folder; nothing is deleted.
$buildRoot=Join-Path $output 'build'; $null=[IO.Directory]::CreateDirectory($buildRoot)
$buildOutput=Join-Path $buildRoot 'sample/Nimbus-Rust-Setup.exe'
$build=Run (Join-Path $kit 'bin/avelin-cli.exe') @('build','--config',(Join-Path $kit 'sample/avelin.toml'),'--output',$buildOutput) $kit
NewText (Join-Path $output 'build.stdout.json') $build.stdout
NewText (Join-Path $output 'build.stderr.txt') $build.stderr
$buildReport=$build.stdout | ConvertFrom-Json
Require ($buildReport.engine -ceq 'rust' -and -not $buildReport.signed) 'Expected unsigned Rust build report.'
foreach ($suffix in @('.exe','.backend.exe','.manifest.json')) {
    CopyNew (Join-Path $buildRoot "sample/Nimbus-Rust-Setup$suffix") (Join-Path $kit "sample/Nimbus-Rust-Setup$suffix")
}
$sampleManifest=Get-Content -LiteralPath (Join-Path $kit 'sample/Nimbus-Rust-Setup.manifest.json') -Raw | ConvertFrom-Json
Require ($sampleManifest.app_id -ceq 'dev.avelin.native.sample' -and $sampleManifest.app_version -ceq '1.0.0') 'Sample manifest differs from the fixed demo contract.'
# Both fields deserialize with default=false. desktop_shortcut omits false on
# serialization; start_menu_shortcut currently emits it. Accept either absent
# or an explicit JSON false, while rejecting true and non-boolean values.
foreach ($name in @('desktop_shortcut','start_menu_shortcut')) {
    $property=$sampleManifest.PSObject.Properties[$name]
    Require ($null -eq $property -or ($property.Value -is [bool] -and $property.Value -eq $false)) "Sample manifest must disable $name."
}
Require ($sampleManifest.runtime_sha256 -ceq (Hash (Join-Path $kit 'bin/avelin-app.exe'))) 'Sample runtime does not match the delivered runtime.'
foreach ($record in $inputComponents) { Require ((Hash $components[$record.path]) -ceq $record.sha256) "Release component changed while packaging: $($record.path)" }
Require ((@((SourceFiles)) -join "`0") -ceq ($sourceNames -join "`0")) 'Source file set changed while packaging.'
foreach ($record in $sourceRecords) {
    $path=Child $ProjectRoot $record.path
    if ($record.Contains('sha256')) { Require ([IO.File]::Exists($path) -and (Hash $path) -ceq $record.sha256) "Source changed while packaging: $($record.path)" }
    else { Require (-not [IO.File]::Exists($path)) 'Deleted source reappeared while packaging.' }
}
$cliVersion=(Run (Join-Path $kit 'bin/avelin-cli.exe') @('--version') $kit 10000).stdout.Trim()
NewJson (Join-Path $kit 'SOURCE-SNAPSHOT.json') ([ordered]@{
    schema_version=1;algorithm='SHA256';git_head=$revision;working_tree_dirty=([bool]$workingChanges.Trim());captured_at_utc=$createdAt
    scope='Current tracked and nonignored source files, including uncommitted changes; relative paths only. Packaging-time snapshot, not proof of a reproducible build.';files=$sourceRecords
})
NewJson (Join-Path $kit 'BUILD-INFO.json') ([ordered]@{
    schema_version=1;candidate_id=$candidateId;created_at_utc=$createdAt;status='UNSIGNED_RUST_DEVELOPER_KIT';engine='rust';platform='x86_64-pc-windows-msvc';signed=$false
    cli_version=$cliVersion;git_head=$revision;source_snapshot='SOURCE-SNAPSHOT.json';source_snapshot_sha256=(Hash (Join-Path $kit 'SOURCE-SNAPSHOT.json'))
    cargo_lock_sha256=(Hash (Join-Path $ProjectRoot 'Cargo.lock'));components=@(foreach ($name in @('avelin-cli.exe','avelin-app.exe','avelin-engine.exe')) { Record $kit (Join-Path $kit "bin/$name") })
    public_documents=@($publicDocs.files)
    sample=@{app_id='dev.avelin.native.sample';version='1.0.0';config='sample/avelin.toml';setup='sample/Nimbus-Rust-Setup.exe';desktop_shortcut=$false;start_menu_shortcut=$false;payload=(Record $kit (Join-Path $kit 'sample/payload/nimbus-demo.exe'))}
    license_evidence=@{directory='third-party';inventory_sha256=(Hash (Join-Path $kit 'third-party/inventory.json'));raw_local_metadata_preserved=$true;declared_limitations='third-party/unknowns.json'}
    validation=@{packaging='Exclusive copies and hashes, PE machine, fixed sample manifest and Rust build checked';installation='Not run by this script; separate acceptance evidence is required';source='Inputs captured when packaging; this script does not rebuild source'}
})
NewText (Join-Path $kit 'README.md') @'
# Avelin Rust Windows x64 开发者工具包

完整解压后使用，组件保持同一套。本包未签名；构包只校验输入与包结构，不执行安装，也不声明通过安装验收。

## 验证与构包

在本目录打开 PowerShell 7.2+：

```powershell
.\Verify-RustDevKit.ps1
.\bin\avelin-cli.exe build --config .\sample\avelin.toml --output ..\output\Nimbus-Rust-Setup.exe
```

输出位置必须是新的；成功后生成 Setup、backend 和 manifest。CLI 自动使用同目录的 `avelin-app.exe` 与 `avelin-engine.exe`，不需要本机 Rust 工具链。最终 Setup 也不需要 PowerShell。接入自己的应用时修改示例产品 ID、名称、版本、入口与 payload 目录，不要沿用示例 ID。

## 演示程序

双击 `sample/Nimbus-Rust-Setup.exe`，确认目录后开始安装。产品为「Nimbus 笔记 · Rust 预览」1.0.0，ID 为 `dev.avelin.native.sample`，默认不创建快捷方式。示例 `nimbus-demo.exe` 是输出文本后退出的控制台程序；安装完成后可以点击启动。卸载使用安装目录内的 `uninstall.exe` 或 Windows 已安装的应用入口。

同版本同构建的 Setup 会显示修复确认页；静默修复使用 `--silent --operation repair`。默认补齐缺失资源并保留修改内容；明确指定 `--repair-overwrite` 才在保留备份后恢复修改过的资源。卸载前关闭已启动的应用。

## 支持范围

当前用户离线完整安装、安装取消、同产品固定目录的更高版本升级、同构建修复，以及按资源归属卸载。未知文件和修改过的载荷按归属规则保留；新载荷路径冲突时拒绝覆盖。升级、卸载和维护操作不提供活动取消。

中断后保留原 Setup 和会话证据。`avelin-cli recover --session <原会话目录> --setup <原Setup>` 先只读检查，确认后添加 `--apply` 执行验证过的回滚或提交确认；也可在原 Setup 的恢复确认页操作。活动进程、身份不符、证据篡改和用户后改冲突均拒绝恢复。详见 `docs/maintenance.md`。

不支持全用户安装、服务或驱动、在线下载、差分更新、无人确认的自动恢复或任意脚本钩子。Rust 载荷当前未压缩，每层嵌入区域上限 1 GiB，组件和元数据也占用空间。无法确认恢复结果时返回 Unknown 并保留事务证据；先诊断现场。详见 `docs/compatibility.md` 和 `docs/cli.md`。

## 完整性、来源与许可

`Verify-RustDevKit.ps1` 只读校验全部文件及内部元数据，拒绝额外文件；可用 `-ZipPath <归档路径>` 同时校验 ZIP。`HASHES.json` 覆盖除自身外的全部文件，ZIP 外另附归档及清单摘要。摘要证明完整性，不等于可信发布者签名。`SOURCE-SNAPSHOT.json` 记录打包时的源码字节，不证明给定二进制来自该源码或构建可复现。`BUILD-INFO.json` 记录组件与本次构包检查。

自有源码与示例资产采用 MIT，见 `LICENSE`、`NOTICE.md`。`third-party/` 保留许可采集材料与 `unknowns.json` 中声明的限制；原始元数据可能含本机构建路径，或包含未链接进本包的依赖。公开发布附件前应单独审阅这些材料。

`docs/` 来自仓库公开文档白名单，本工具包使用 Rust 引擎。文档中的源码、维护脚本和 examples 引用需在源码仓库中阅读；本包不包含完整仓库。构包日志和暂存证据留在 ZIP 外。
'@
$hashes=@(TreeFiles $kit | Sort-Object FullName | ForEach-Object { Record $kit $_.FullName })
NewJson (Join-Path $kit 'HASHES.json') ([ordered]@{schema_version=1;algorithm='SHA256';scope='Every delivered regular file except HASHES.json itself';files=$hashes})
$manifestHash=Hash (Join-Path $kit 'HASHES.json')
$zip=Join-Path $output 'Avelin-Rust-DeveloperKit.zip'
NoReparse $zip; Require (-not [IO.File]::Exists($zip)) 'Archive already exists.'
[IO.Compression.ZipFile]::CreateFromDirectory($kit,$zip,[IO.Compression.CompressionLevel]::Optimal,$true)
# The shipped verifier checks exact disk membership and reads every ZIP entry back.
$verification=& (Join-Path $kit 'Verify-RustDevKit.ps1') -KitDirectory $kit -ZipPath $zip | ConvertFrom-Json
Require ($verification.verified -eq $true -and $verification.delivered_files -eq $hashes.Count+1) 'Packaged verifier did not pass.'
$zipHash=Hash $zip
NewText (Join-Path $output 'Avelin-Rust-DeveloperKit.zip.sha256') "$zipHash  Avelin-Rust-DeveloperKit.zip`n"
NewText (Join-Path $output 'Avelin-Rust-DeveloperKit.HASHES.sha256') "$manifestHash  Avelin-Rust-DeveloperKit/HASHES.json`n"
$result=[ordered]@{
    schema_version=1;status='PASSED';verified=$true;candidate_id=$candidateId
    kit_path=$kit;zip_path=$zip;zip_size=(Get-Item -LiteralPath $zip).Length
    zip_sha256=$zipHash;hashes_sha256=$manifestHash;delivered_files=$hashes.Count+1
    source_snapshot_sha256=(Hash (Join-Path $kit 'SOURCE-SNAPSHOT.json'))
    installation_executed=$false;publication_executed=$false
}
NewJson (Join-Path $output 'packaging-verification.json') $result
$result | ConvertTo-Json
