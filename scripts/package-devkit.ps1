#requires -Version 7.2
<# Build a new portable developer preview. No installer or sample is executed. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$CliPath,
    [Parameter(Mandatory)][string]$RuntimePath,
    [Parameter(Mandatory)][string]$BridgePath,
    [Parameter(Mandatory)][string]$SamplePath,
    [Parameter(Mandatory)][string]$NsisRoot,
    [Parameter(Mandatory)][string]$LicenseDirectory,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [string]$ProjectRoot = (Split-Path -Parent $PSScriptRoot)
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
function Full([string]$Path) { [IO.Path]::GetFullPath($Path).TrimEnd([char]'\',[char]'/') }
function Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function NoReparse([string]$Path) {
    for ($cursor = [IO.Path]::GetFullPath($Path); $cursor; $cursor = [IO.Path]::GetDirectoryName($cursor)) {
        try { $attributes = [IO.File]::GetAttributes($cursor) }
        catch [IO.FileNotFoundException] { continue }
        catch [IO.DirectoryNotFoundException] { continue }
        if ($attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse point: $cursor" }
    }
}
function WriteJson([string]$Path, $Value) { [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 60), [Text.UTF8Encoding]::new($false)) }
function CopyNew([string]$Source, [string]$Destination) {
    NoReparse $Source; NoReparse $Destination
    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $Destination))
    [IO.File]::Copy($Source, $Destination, $false)
    if ((Hash $Source) -cne (Hash $Destination)) { throw "Copy mismatch: $Destination" }
}
function CopyTree([string]$Source, [string]$Destination) {
    NoReparse $Source
    $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push((Full $Source))
    while ($pending.Count) {
        foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            NoReparse $item.FullName
            if ($item.PSIsContainer) { $pending.Push($item.FullName) }
            else { CopyNew $item.FullName (Join-Path $Destination ([IO.Path]::GetRelativePath($Source,$item.FullName))) }
        }
    }
}
function Probe([string]$Executable, [string[]]$Arguments) {
    $info = [Diagnostics.ProcessStartInfo]::new($Executable); $info.UseShellExecute=$false; $info.CreateNoWindow=$true
    $info.WindowStyle=[Diagnostics.ProcessWindowStyle]::Hidden; $info.RedirectStandardOutput=$true; $info.RedirectStandardError=$true
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $process=[Diagnostics.Process]::new(); $process.StartInfo=$info
    try {
        $null=$process.Start(); $out=$process.StandardOutput.ReadToEndAsync(); $err=$process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(10000)) { throw "Read-only version probe timed out: $Executable" }
        $stdout=$out.GetAwaiter().GetResult(); $stderr=$err.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) { throw "Version probe failed: $Executable $stderr" }
        return $stdout.Trim()
    } finally { $process.Dispose() }
}
function PeMachine([string]$Path, [int]$Expected) {
    $stream=[IO.File]::OpenRead($Path); $reader=[IO.BinaryReader]::new($stream)
    try {
        if ($reader.ReadUInt16() -ne 0x5a4d) { throw "Not a PE file: $Path" }
        $stream.Position=0x3c; $offset=$reader.ReadUInt32(); $stream.Position=$offset
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne $Expected) { throw "Wrong PE machine: $Path" }
    } finally { $reader.Dispose(); $stream.Dispose() }
}
$ProjectRoot=Full $ProjectRoot; $OutputDirectory=Full $OutputDirectory; $NsisRoot=Full $NsisRoot; $LicenseDirectory=Full $LicenseDirectory
if (Test-Path -LiteralPath $OutputDirectory) { throw 'OutputDirectory must be new; failed output is retained for inspection.' }
NoReparse $OutputDirectory; NoReparse $NsisRoot; NoReparse $LicenseDirectory
foreach ($path in @($CliPath,$RuntimePath,$BridgePath,$SamplePath)) { NoReparse $path; if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing input: $path" } }
PeMachine $CliPath 0x8664; PeMachine $RuntimePath 0x8664; PeMachine $BridgePath 0x014c; PeMachine $SamplePath 0x8664
$version=Probe (Full $CliPath) @('--version')
if ($version -cne 'avelin-cli 0.1.0-preview.1') { throw "Unexpected developer preview CLI: $version" }
$nsisVersion=Probe (Join-Path $NsisRoot 'makensis.exe') @('/VERSION')
if ($nsisVersion -cne 'v3.12') { throw "NSIS 3.12 is required, observed $nsisVersion" }
if ((Hash (Join-Path $NsisRoot 'makensis.exe')) -cne 'b043e554afefbfc56315669d0b4779793aeae67f0f2a7a790e2ea91f05298eff' -or (Hash (Join-Path $NsisRoot 'Bin/makensis.exe')) -cne '25d1aa7081db1a9de9690b59983f9652b7409a189c3444328cc1841eff693e8d') { throw 'NSIS executables differ from the pinned 3.12 preview inputs' }
if (-not (Test-Path -LiteralPath (Join-Path $LicenseDirectory 'inventory.json') -PathType Leaf)) { throw 'Generate the third-party inventory first.' }
$licenseInventory = Get-Content -LiteralPath (Join-Path $LicenseDirectory 'inventory.json') -Raw | ConvertFrom-Json
$licenseFiles = Get-Content -LiteralPath (Join-Path $LicenseDirectory 'FILES.json') -Raw | ConvertFrom-Json
if ($licenseInventory.schema_version -ne 1 -or $licenseFiles.schema_version -ne 1 -or @($licenseFiles.files).Count -eq 0) { throw 'Invalid third-party inventory' }
if ($licenseInventory.project_license.spdx -cne 'MIT') { throw 'Regenerate license materials for the current MIT workspace.' }
$projectLicensePath = Join-Path $ProjectRoot 'LICENSE'
NoReparse $projectLicensePath
$projectLicenseRecords = @($licenseInventory.project_license.license_texts)
if ($projectLicenseRecords.Count -ne 1 -or $projectLicenseRecords[0].sha256 -cne (Hash $projectLicensePath)) { throw 'Project license text differs from collected materials.' }
$licenseSeen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($entry in $licenseFiles.files) {
    $relative = [string]$entry.path
    if ([IO.Path]::IsPathRooted($relative) -or $relative -match '(^|[\\/])\.\.?([\\/]|$)' -or $relative.Contains(':') -or -not $licenseSeen.Add($relative.Replace('/','\'))) { throw 'Invalid third-party evidence path' }
    $path = Full (Join-Path $LicenseDirectory $relative)
    if (-not $path.StartsWith($LicenseDirectory+'\',[StringComparison]::OrdinalIgnoreCase)) { throw 'Third-party path escaped its root' }
    NoReparse $path
    if ((Get-Item -LiteralPath $path).Length -ne $entry.size -or (Hash $path) -cne $entry.sha256) { throw "Third-party evidence changed: $relative" }
}
$kit=Join-Path $OutputDirectory 'Avelin-DeveloperKit'; $null=[IO.Directory]::CreateDirectory($kit)
$records=[Collections.Generic.List[object]]::new()
foreach ($pair in @(@($CliPath,'avelin-cli.exe'),@($RuntimePath,'avelin-app.exe'),@($BridgePath,'avelin_nsis_bridge.dll'),@($SamplePath,'sample/nimbus-demo.exe'))) {
    CopyNew (Full $pair[0]) (Join-Path $kit $pair[1])
    $records.Add(@{path=$pair[1];source=(Full $pair[0]);size=(Get-Item -LiteralPath $pair[0]).Length;sha256=(Hash $pair[0])})
}
# Keep the compiler, stubs, plugin and resource trees with their upstream notices.
# Exclude only installed UI launchers, installer/uninstaller and shortcuts.
foreach ($name in @('makensis.exe','COPYING','nsisconf.nsh')) { CopyNew (Join-Path $NsisRoot $name) (Join-Path $kit ('tools/nsis/'+$name)) }
foreach ($name in @('Bin','Include','Stubs','Plugins','Contrib','Docs','Examples')) { CopyTree (Join-Path $NsisRoot $name) (Join-Path $kit ('tools/nsis/'+$name)) }
CopyTree $LicenseDirectory (Join-Path $kit 'third-party')
foreach ($name in @('Verify-DevKit.ps1')) { CopyNew (Join-Path $ProjectRoot ('scripts/'+$name)) (Join-Path $kit ('scripts/'+$name)) }
CopyNew (Join-Path $ProjectRoot 'tests/harness/run-m4.ps1') (Join-Path $kit 'tests/harness/run-m4.ps1')
# Copy only explicitly reviewed public documentation, including on a workstation
# that still contains ignored internal reports and personal prototypes.
$docManifestPath = Join-Path $ProjectRoot 'packaging/public-docs.json'
NoReparse $docManifestPath
$docManifest = Get-Content -LiteralPath $docManifestPath -Raw | ConvertFrom-Json
if ($docManifest.schema_version -ne 1 -or @($docManifest.files).Count -eq 0) { throw 'Invalid public documentation manifest' }
$docNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($relative in $docManifest.files) {
    if ($relative -isnot [string] -or [IO.Path]::IsPathRooted($relative) -or $relative.Contains(':') -or $relative.Contains('\') -or -not $docNames.Add($relative)) { throw 'Invalid public documentation path' }
    foreach ($segment in $relative.Split('/')) {
        if (-not $segment -or $segment -in @('.', '..') -or $segment.EndsWith('.') -or $segment.EndsWith(' ')) { throw 'Ambiguous public documentation path' }
    }
    if ([IO.Path]::GetExtension($relative) -notin @('.md', '.json')) { throw 'Unsupported public documentation type' }
    CopyNew (Join-Path $ProjectRoot ('docs/'+$relative)) (Join-Path $kit ('docs/'+$relative))
}
foreach ($name in @('LICENSE', 'NOTICE.md', 'CONTRIBUTING.md')) { CopyNew (Join-Path $ProjectRoot $name) (Join-Path $kit $name) }
$readme=@'
# Avelin 0.1.0-preview.1 Windows x64 开发者工具包

完整解压到本地目录。用现有应用的发布文件即可打包，无需安装 Rust、编写 NSIS 脚本或单独安装 NSIS。本包携带 Unicode NSIS 3.12；组件清单见 `BUILD-INFO.json`，逐文件完整性见 `HASHES.json`。

```powershell
# 只读完整性检查；此步骤需要 PowerShell 7.2+。
pwsh -NoProfile -File .\scripts\Verify-DevKit.ps1

# 先把实际应用发布文件放到 C:\MyApp\dist，entry 是该目录内相对 EXE 路径。
.\avelin-cli.exe init --directory C:\MyApp --app-id com.example.myapp --name MyApp --app-version 1.0.0 --entry MyApp.exe --payload C:\MyApp\dist
.\avelin-cli.exe check --config C:\MyApp\avelin.toml
.\avelin-cli.exe build --config C:\MyApp\avelin.toml --output C:\MyApp\out\MyApp-Setup.exe
```

直接调用 CLI 或最终 Setup 不需要 PowerShell。`init` 校验现有发布目录、生成配置，不复制或执行应用；已有配置不会覆盖。`build` 生成 Setup、backend 和 manifest；请把全新的输出文件名用于每次构建。最终分发 Setup；第三方声明及发行材料按 `docs/releasing.md` 处理，构建 sidecar 与日志留作诊断。

启动 Setup 安装；更高版本的同一产品 Setup 可确认升级。静默安装需 `--silent --install-dir <目录>`，静默升级另加 `--operation upgrade`。成功后的 GUI 启动需要明确点击。升级固定原目录且不提供取消；未知内容和符合归属保留规则的修改文件会保留。详细契约见 `docs/compatibility.md`，接入教程见 `docs/getting-started.md`。

本包为未签名开发者预览，打包本身不执行安装。实际验收状态须依据本次包的配套记录；内置 Nimbus 仅用于工具链测试。`HASHES.json` 验证文件完整性，不代表可信发布者签名。

## 可重复执行的本机正常流程

```powershell
pwsh -NoProfile -File .\tests\harness\run-m4.ps1 -KitRoot . -OutputDirectory C:\Avelin-M4-NewRun
```

输出目录必须不存在。脚本实际执行唯一产品的初始化、打包、安装、升级、启动和登记卸载，核对用户内容保留，证据留在指定目录；不会切换系统显示设置，也不代表两款真实产品验收。

第三方清单与许可文本在 `third-party/`。Avelin 自有内容采用根目录 LICENSE 中的 MIT；第三方许可独立适用。材料可能含原始 Cargo 元数据和构建路径，公开附件应另行检查。

`docs/cli.md`、`docs/getting-started.md` 和 `docs/compatibility.md` 是使用入口。本包只附公开文档和 schemas；开发文档中的源码、examples 和维护脚本引用需在源码仓库中阅读，工具包不包含完整仓库。
'@
[IO.File]::WriteAllText((Join-Path $kit 'README.md'),$readme,[Text.UTF8Encoding]::new($false))
$publicComponents = @($records | ForEach-Object { @{path=$_.path;size=$_.size;sha256=$_.sha256} })
$buildInfo=[ordered]@{schema_version=1;kit_version='0.1.0-preview.1';created_at_utc=[DateTime]::UtcNow.ToString('o');scope='Unsigned developer preview; packaging does not perform installation acceptance or publication.';components=$publicComponents;nsis_version=$nsisVersion;nsis_launcher_sha256=(Hash (Join-Path $kit 'tools/nsis/makensis.exe'));nsis_compiler_sha256=(Hash (Join-Path $kit 'tools/nsis/Bin/makensis.exe'));config_schema=1;ipc_protocol=1;signed=$false;system_settings_changed=$false;tools_required_for_end_user_setup=@();tools_required_for_packaging=@();tools_required_for_harness=@('PowerShell 7.2+');public_project_license='MIT'}
$buildInfo.third_party_inventory_status = $licenseInventory.status
$buildInfo.third_party_declared_unknowns = @($licenseInventory.unknowns).Count
WriteJson (Join-Path $kit 'BUILD-INFO.json') $buildInfo
$files=@(Get-ChildItem -LiteralPath $kit -File -Recurse -Force | Sort-Object FullName | ForEach-Object { @{path=[IO.Path]::GetRelativePath($kit,$_.FullName).Replace('\','/');size=$_.Length;sha256=(Hash $_.FullName)} })
WriteJson (Join-Path $kit 'HASHES.json') @{schema_version=1;algorithm='SHA256';files=$files}
$verification = & (Join-Path $kit 'scripts/Verify-DevKit.ps1') -KitRoot $kit | ConvertFrom-Json
if ($verification.status -cne 'PASS' -or $verification.checked_files -ne $files.Count) { throw 'Packaged hash verifier did not pass' }
$zipPath=Join-Path $OutputDirectory 'Avelin-0.1.0-preview.1-windows-x64.zip'
[IO.Compression.ZipFile]::CreateFromDirectory($kit,$zipPath,[IO.Compression.CompressionLevel]::Optimal,$true)
$zip=[IO.Compression.ZipFile]::OpenRead($zipPath)
try {
    $names=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $expected=@($files.path)+@('HASHES.json')
    if ($zip.Entries.Count -ne $expected.Count) { throw 'ZIP entry count mismatch' }
    foreach ($entry in $zip.Entries) {
        if (-not $entry.FullName.StartsWith('Avelin-DeveloperKit/',[StringComparison]::Ordinal)) { throw 'ZIP prefix mismatch' }
        $relative=$entry.FullName.Substring('Avelin-DeveloperKit/'.Length)
        if (-not $names.Add($relative) -or $relative -cnotin $expected) { throw 'ZIP contains an unexpected or duplicate entry' }
        $stream=$entry.Open()
        try { $hash=[Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($stream)).ToLowerInvariant() } finally { $stream.Dispose() }
        $disk=Join-Path $kit $relative
        if ($entry.Length -ne (Get-Item -LiteralPath $disk).Length -or $hash -cne (Hash $disk)) { throw "ZIP content differs: $relative" }
    }
} finally { $zip.Dispose() }
$result=[ordered]@{schema_version=1;status='PASSED';kit_path=$kit;zip_path=$zipPath;zip_size=(Get-Item -LiteralPath $zipPath).Length;zip_sha256=(Hash $zipPath);verified_files=$files.Count;zip_entries=$files.Count+1;components=$records.ToArray();no_installation_performed=$true;no_publication_performed=$true}
WriteJson (Join-Path $OutputDirectory 'packaging-verification.json') $result
$result | ConvertTo-Json -Depth 8
