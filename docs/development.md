# 源码开发

当前实现面向 Windows x64：Rust CLI、Rust 协调器和 GPUI 窗口运行在 x64，NSIS 插件使用 x86 ABI。实际文件部署由 NSIS 完成。本文从仓库源码构建，不依赖开发者电脑里的历史安装包或 `target/` 产物。

## 环境

在仓库根目录使用 **64 位 PowerShell 7.2+**。预先准备：

- Rust stable 的 `x86_64-pc-windows-msvc` 工具链和 `rustfmt`。
- `x86_64-pc-windows-msvc`、`i686-pc-windows-msvc` 两个 Rust target。
- Visual Studio Build Tools 的 C++ x64/x86 工具及 Windows SDK。
- 构建 Setup 时需要 NSIS 3 Unicode；当前工具包流程核验 NSIS **3.12**。只运行源码测试不需要 NSIS。
- 生成第三方清单和完整工具包时另需 Python 3.11+；收集器只使用标准库。

安装 Rust 后，可显式准备组件和目标：

```powershell
rustup toolchain install stable --profile minimal --component rustfmt --component clippy
rustup target add --toolchain stable x86_64-pc-windows-msvc i686-pc-windows-msvc
```

仓库的 `rust-toolchain.toml` 选择 stable，依赖由 `Cargo.lock` 锁定。尚未声明最低 Rust 版本；stable 更新后需要重新验证，不能由锁文件推断整个工具链逐字节可复现。不要将本地验收曾观察到的 Rust 版本当成所有贡献者必须安装的版本。

只读检查环境：

```powershell
pwsh -NoProfile -File scripts/doctor.ps1
pwsh -NoProfile -File scripts/doctor.ps1 -Json
```

`doctor` 记录已安装工具，不安装软件；它不会证明最终用户环境兼容。检查/构建脚本要求已安装工具，Cargo 可能下载锁文件中的依赖。

## 源码检查

```powershell
pwsh -NoProfile -File scripts/check-source.ps1
```

该入口依次检查格式，运行 core、CLI、带 GUI 功能的 app、UI 的源码测试，以及 x86 bridge 测试。UI 的视图测试使用测试上下文，不打开安装器窗口。默认忽略的真实 NSIS/时序探针仍不运行，50 000 文件边界测试被显式跳过。它不执行安装、升级、卸载或性能测试。

日志和 `summary.json` 写入新的 `target/source-check/<run>/`。可指定 `-OutputDirectory`，但目录必须尚不存在；`-RustToolchain` 允许选择另一个已安装的 x64 MSVC 工具链。源码测试通过不等于真实安装流程验收通过。

定向检查示例：

```powershell
cargo fmt --all -- --check
cargo test --locked -p avelin-cli
cargo test --locked -p avelin-core -- --skip manifest_validates_fifty_thousand_files_and_rejects_the_next
cargo test --locked -p avelin-ui
cargo test --locked -p avelin-app --features gui
cargo test --locked -p avelin-nsis-bridge --target i686-pc-windows-msvc
```

## 从源码生成一个 Setup

以下命令在仓库根目录执行，假设使用默认 `target/`；若配置了 `CARGO_TARGET_DIR`，需相应调整产物路径。保持 `CARGO_BUILD_TARGET` 未设置，x86 只通过 bridge 命令显式选择。每一步成功后再执行下一步。

先构建 x64 GUI runtime、CLI、可运行样例，以及 x86 bridge：

```powershell
cargo build --locked --release -p avelin-app -p avelin-cli --features avelin-app/gui --bins
cargo build --locked --release -p avelin-nsis-bridge --target i686-pc-windows-msvc
```

准备仅包含发布产物的新样例目录，并使用 CLI 生成配置：

```powershell
$demoRoot = Join-Path 'target/development-demo' ([Guid]::NewGuid().ToString('N'))
$payloadDirectory = Join-Path $demoRoot 'payload'
New-Item -ItemType Directory -Path $payloadDirectory | Out-Null
Copy-Item -LiteralPath 'target/release/nimbus-demo.exe' -Destination $payloadDirectory
& './target/release/avelin-cli.exe' init --directory $demoRoot `
  --app-id dev.avelin.sample --name 'Avelin sample' --app-version 1.0.0 `
  --entry nimbus-demo.exe --payload $payloadDirectory
& './target/release/avelin-cli.exe' check --config (Join-Path $demoRoot 'avelin.toml')
```

指定这次源码构建的组件，生成新安装包。NSIS 路径替换为你的实际安装路径：

```powershell
& './target/release/avelin-cli.exe' build `
  --config (Join-Path $demoRoot 'avelin.toml') `
  --runtime './target/release/avelin-app.exe' `
  --bridge './target/i686-pc-windows-msvc/release/avelin_nsis_bridge.dll' `
  --nsis 'C:/Program Files (x86)/NSIS/makensis.exe' `
  --output (Join-Path $demoRoot 'Avelin-Sample-Setup.exe')
```

这一步只生成包，不启动安装。CLI 输出实际组件路径；Setup 及其 sidecar 文件必须使用新路径，已有文件不会被覆盖。要测试另一个同时存在的产品，使用不同 `app.id`；测试同一产品升级则保持 ID、增加版本并按[兼容约定](compatibility.md)执行。

已有发布目录接入和品牌配置见[接入指南](getting-started.md)、[CLI 说明](cli.md)。`scripts/build-m0.ps1` 保留为早期 Nimbus 样例构建入口；新贡献者可以直接采用上面的源码命令。

## 项目结构

| 目录 | 职责 |
|---|---|
| `apps/avelin-cli` | `init/check/build/replay/diagnose`，配置、资源、载荷与 NSIS 脚本生成 |
| `apps/avelin-app` | 安装协调器、进程/IPC 生命周期、结果和恢复判断、GUI 入口 |
| `crates/avelin-core` | 共享配置、manifest、plan、receipt、协议和路径约束 |
| `crates/avelin-ui` | GPUI 视图与交互；接收状态，不自行认定安装成功 |
| `crates/avelin-nsis-bridge` | x86 NSIS 插件，受限 IPC 与受管资源操作 |
| `examples` | 示例配置、品牌资源和用于构建的输入 |
| `scripts` | 源码检查、构建、许可收集、打包、完整验收入口 |
| `tests/harness` | 真实运行验收脚本；普通源码检查不会自动调用 |

## 两类 CI

### 贡献者源码检查

`.github/workflows/source.yml` 在 PR、默认分支推送和手工触发时运行，使用 GitHub-hosted `windows-2025`。它准备 stable 工具链和双架构 target，调用同一个 `check-source.ps1`，保存结果。该工作流只有读取仓库权限，不需要仓库 secret，不使用自托管 runner，不调用安装流程。

GitHub 官方将 `windows-2025` 列为 Windows x64 hosted runner 标签；实际执行机器和 Rust 版本以 workflow 日志为准。[GitHub runner 文档](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)

### 维护者完整工具包验收

`scripts/ci-windows.ps1` 从源码构建 Release、收集许可证据、打包、校验 ZIP，并执行工具包自己的安装 A → 升级 B → 运行样例 → 注册卸载正常链。它会创建测试产品的文件、快捷方式和当前用户卸载登记，应在明确用于验收的工作区执行。

```powershell
pwsh -NoProfile -File scripts/ci-windows.ps1 `
  -OutputDirectory target/maintainer-validation/run-001 `
  -NsisRoot 'C:/Program Files (x86)/NSIS' -RustToolchain stable
```

输出目录必须是新目录。默认记录实际 Rust 版本；复现指定工具链时可加 `-ExpectedRustVersion <exact-version>`，脚本只校验，不下载。NSIS 3.12 的启动器和实际编译器仍核对固定 SHA-256。Python 可通过 `-PythonPath` 指定。

现有 `.github/workflows/windows.yml` 是手工 self-hosted 入口，需要预先配置 `avelin-tools` runner、`AVELIN_NSIS_ROOT` 和可选 `AVELIN_PYTHON`。它显式检查此前验收的 Rust 1.97.1；维护者变更工具链前应重新核验并更新该约定。不要让它自动执行外部 PR。提交配置不代表云端已通过；实际远程运行状态只依据 GitHub 运行结果。

完整工具包还需要[第三方材料](third-party.md)和[发布流程](releasing.md)中的核验。源码 CI、真实安装验收、签名及公开 Release 分别记录结果。
