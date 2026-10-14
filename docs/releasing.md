# 维护与发布

## 源码仓库

仓库提供 Rust 原生引擎的预览源码，包含 MIT、贡献说明、公开文档和源码检查。源码提交、工具包生成和公开 Release 分别记录版本与验证结果。

提交前：

```powershell
python scripts/check-public-source.py
pwsh -NoProfile -File scripts/check-source.ps1
git status --short
git diff --cached --stat
```

第一条检查 Git 候选、个人路径、常见凭据特征和公开文档链接，不是完整秘密审计。暂存后再次执行，避免把已暂存但现在被忽略的内部文件提交。源码测试不执行安装器。

`.gitignore` 保留源码、样例、schema 和明确选择的公开文档，排除本机报告、原型、生成产物与签名材料。原文件继续留在本地，无需删除历史证据。新增公开文档时同时更新 `.gitignore` 和 `packaging/public-docs.json`。

## 二进制发行单独处理

已有本地开发包不能直接视为公开 Release 附件。完整构建证据可能含构建路径、Windows 用户 SID、机器信息和原始 Cargo 元数据；上传前检查实际附件内容。

1. 从明确的源码版本构建配套 x64 runtime、CLI、样例和 Rust worker，公开构建需映射编译源码路径。
2. 用 `scripts/collect-third-party.py` 收集锁定依赖和工具许可材料，并核验输出。
3. 使用 `scripts/package-rust-devkit.ps1`；公共文档从明确清单复制，并附项目 LICENSE/NOTICE。
4. 校验 HASHES 与 ZIP 摘要，在新测试目录验证安装、升级、启动和卸载。
5. 导出公开副本，复核附件中的个人信息、第三方声明、发行身份和签名安排，再发布预览版本。

### Rust 工具包

在已构建 Release 组件的仓库中，先生成当前锁文件的第三方材料。收集器只记录 Windows x64 依赖，不需要额外安装器编译器。

```powershell
python scripts/collect-third-party.py --project-root . --output target/rust-licenses-new
pwsh -NoProfile -File scripts/package-rust-devkit.ps1 `
  -CliPath target/release/avelin-cli.exe `
  -RuntimePath target/release/avelin-app.exe `
  -RustWorkerPath target/release/avelin-engine.exe `
  -SamplePath target/release/nimbus-demo.exe `
  -LicenseDirectory target/rust-licenses-new `
  -OutputDirectory target/rust-package-new
pwsh -NoProfile -File scripts/Verify-RustDevKit.ps1 `
  -KitDirectory target/rust-package-new/Avelin-Rust-DeveloperKit `
  -ZipPath target/rust-package-new/Avelin-Rust-DeveloperKit.zip
```

每一步成功后再执行下一步。打包输出目录必须不存在。脚本验证组件、许可材料及公开文档清单，构建演示 Setup，并生成逐文件摘要、源码快照及 ZIP；它不执行安装或发布。独立验证器核对文件集合、字节数和摘要，也可核对 ZIP。摘要证明与清单一致，不证明可信发布者身份。

解压工具包后，`bin/` 内配套 CLI、runtime、worker 可直接使用；示例及构包命令见包内 README。`SOURCE-SNAPSHOT.json` 记录打包时源码字节，不能替代可复现构建证明。完整原始第三方材料保留在 `third-party/`，不要把它误当成已脱敏的公开附件。

### 完整验收入口

使用 `scripts/ci-rust-windows.ps1`，见[开发指南](development.md#维护者完整工具包验收)。Rust 入口从源码测试与构建开始，采集依赖材料、打包并解压校验，再使用解压组件运行生命周期、中断、恢复/修复、可选组件、Windows 维护与部署配置验收。全用户真实流程需要提升权限，未执行或受阻须单独记录；构包成功不代表这些流程通过。

`.github/workflows/rust-windows.yml` 提供 GitHub 托管 Windows 入口；`.github/workflows/windows.yml` 保留手动自托管入口。完整本机日志可能包含路径、SID 和测试资源，不作为公开附件自动上传；云端执行结果以对应源码提交的实际运行状态为准。自托管 runner 的完整证据需在后续 checkout 清理工作目录前自行归档。

### 公开构建与附件导出

Rust 的错误位置和 `file!()` 等字符串可能把源码绝对路径编译进 EXE；仅去掉调试符号不能消除这些路径。应从发行提交的干净 checkout 构建，在该 checkout 根目录设置编译路径映射。保留静态 CRT 配置；`CARGO_ENCODED_RUSTFLAGS` 会覆盖仓库的 rustflags。

```powershell
$buildSource = (Get-Location).Path
$previousFlags = $env:CARGO_ENCODED_RUSTFLAGS
try {
  $publicFlags = @('-C', 'target-feature=+crt-static')
  foreach ($source in @($env:USERPROFILE, $env:USERPROFILE.Replace('\', '/'))) {
    $publicFlags += "--remap-path-prefix=$source=/build/user"
  }
  foreach ($source in @($buildSource, $buildSource.Replace('\', '/'))) {
    $publicFlags += "--remap-path-prefix=$source=/src/avelin"
  }
  $env:CARGO_ENCODED_RUSTFLAGS = $publicFlags -join [char]31
  pwsh -NoProfile -File scripts/ci-rust-windows.ps1 `
    -OutputDirectory target/public-validation-new `
    -CargoTargetDirectory target/public-cargo
  if ($LASTEXITCODE -ne 0) { throw 'Public build and acceptance failed' }
} finally {
  $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags
}
```

如果 Cargo 缓存、Rust 工具链或其他依赖位于用户目录之外，还要为这些实际来源增加映射。重建后审查实际二进制，不能对已有 EXE 做文本替换。上述入口执行真实验收，证据仍留在本地。

原始 Cargo 元数据用于本机溯源，包含缓存和源码路径。`scripts/export-public-release.py` 从已经验证的工具包生成新公开目录：

```powershell
$kit = 'target/public-validation-new/package/Avelin-Rust-DeveloperKit'
$kitHash = (Get-FileHash "$kit/HASHES.json" -Algorithm SHA256).Hash.ToLowerInvariant()
$releaseCommit = git rev-parse HEAD
python scripts/export-public-release.py `
  --kit $kit --expected-kit-hashes-sha256 $kitHash `
  --project-root . --commit $releaseCommit --version 0.1.0-preview.1 `
  --output target/public-release-new
```

发行源码 checkout 可以与运行导出工具的维护 checkout 分开；此时 `--project-root` 指向具有发行提交的仓库，`--commit` 使用完整发行提交 ID。运行前明确候选 `HASHES.json` 摘要，导出器会再次核对全部输入文件及第三方清单，拒绝额外、缺失或被修改的文件。

导出器将源码快照的路径集合及文件内容绑定到指定 Git 提交，只允许明确记录的 Git LF/CRLF 归一化。它为 Cargo 元数据生成明确标注的公开依赖图，保留版本、依赖关系与特性，去掉本机绝对路径，并记录原始与公开材料的摘要。许可证、声明、上游源码、NOTICE、所有 EXE 均保留原字节；原始工具包不被修改。

公开包内的 `PUBLIC-RELEASE.json` 说明转换范围，`BUILD-INFO.json`、文件清单、ZIP 和摘要同步更新。导出器扫描 UTF-8/UTF-16 和 gzip 内容中的用户目录、常见凭据模式；可重复传入 `--deny-text` 增加本次发行禁止出现的私有标识。命中只报文件和类别，不打印敏感值。这是针对性检查，不能替代人工复核。

随后使用原验证器核对公开文件树与最终 ZIP：

```powershell
pwsh -NoProfile -File scripts/Verify-RustDevKit.ps1 `
  -KitDirectory target/public-release-new/Avelin-Rust-DeveloperKit `
  -ZipPath target/public-release-new/Avelin-0.1.0-preview.1-windows-x64.zip
```

导出器不重新编译或执行安装。只有在公开包 EXE 摘要与已验收组件完全相同时，才可引用对应验收结果。发布附件使用 ZIP 及其 `.sha256`，不要上传完整本机报告；先创建草稿、核对上传后附件，再公布为预览版。发布标签必须指向实际构建源码提交，不能用后续文档或工具提交冒充二进制来源。导出器自己的回归测试可单独运行 `python tests/harness/test-public-release.py`。

## 版本与结论

CLI 预览版本是 `0.1.0-preview.1`；其他 crate 内部版本不代表独立公开包版本，当前均 `publish = false`。schema 和被安装应用版本分开管理。不要按同名文件混搭组件，也不要把构建成功记为安装通过。

MIT 不替第三方文件重新授权。依赖源码、图标和二进制分发材料见[第三方说明](third-party.md)。保留未验证项与已知限制，按实际结果写发布说明。
