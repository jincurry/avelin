# 源码开发

当前实现面向 Windows x64：Rust CLI、协调器、GPUI 窗口和原生 Rust worker 运行在 x64；默认 NSIS 后端的插件使用 x86 ABI。本文从仓库源码构建，可分别生成 Rust 或 NSIS 安装包，不依赖开发者电脑里的历史安装包或 `target/` 产物。

## 环境

在仓库根目录使用 **64 位 PowerShell 7.2+**。预先准备：

- Rust stable 的 `x86_64-pc-windows-msvc` 工具链和 `rustfmt`。
- Rust 后端需要 `x86_64-pc-windows-msvc`；NSIS bridge 和完整源码检查另需 `i686-pc-windows-msvc`。
- Visual Studio Build Tools 的 C++ x64 工具及 Windows SDK；NSIS bridge 另需 x86 工具。
- NSIS 构包需要 NSIS 3 Unicode；当前工具包流程核验 NSIS **3.12**。Rust 构包与普通源码测试不需要 NSIS。
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

该入口依次检查格式，运行 core、CLI、Rust 引擎、带 GUI 功能的 app、UI 的源码测试，以及 x86 bridge 测试。UI 的视图测试使用测试上下文，不打开安装器窗口。默认忽略的真实 NSIS/时序探针仍不运行，50 000 文件边界测试被显式跳过。它不调用下面的真实进程安装验收，也不运行性能测试。

日志和 `summary.json` 写入新的 `target/source-check/<run>/`。可指定 `-OutputDirectory`，但目录必须尚不存在；`-RustToolchain` 允许选择另一个已安装的 x64 MSVC 工具链。源码测试通过不等于真实安装流程验收通过。

定向检查示例：

```powershell
cargo fmt --all -- --check
cargo test --locked -p avelin-cli
cargo test --locked -p avelin-engine
cargo test --locked -p avelin-core -- --skip manifest_validates_fifty_thousand_files_and_rejects_the_next
cargo test --locked -p avelin-ui
cargo test --locked -p avelin-app --features gui
cargo test --locked -p avelin-nsis-bridge --target i686-pc-windows-msvc
```

## 从源码生成一个 Setup

以下命令在仓库根目录执行，假设使用默认 `target/`；若配置了 `CARGO_TARGET_DIR`，需相应调整产物路径。保持 `CARGO_BUILD_TARGET` 未设置，x86 只通过 bridge 命令显式选择。每一步成功后再执行下一步。

先构建 x64 GUI runtime、CLI、Rust 引擎和可运行样例：

```powershell
cargo build --locked --release -p avelin-engine -p avelin-cli -p avelin-app --features avelin-app/gui --bins
```

需要构建 NSIS 包时，再构建 x86 bridge：

```powershell
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

使用这次源码构建的组件，生成 Rust 安装包：

```powershell
& './target/release/avelin-cli.exe' build --engine rust `
  --config (Join-Path $demoRoot 'avelin.toml') `
  --runtime './target/release/avelin-app.exe' `
  --rust-worker './target/release/avelin-engine.exe' `
  --output (Join-Path $demoRoot 'Avelin-Sample-Rust-Setup.exe')
```

此命令不调用 NSIS 或 bridge。Rust 载荷目前不压缩，包大小限制与开销见 [CLI 契约](cli.md)。保留 Release 产物的配套关系，不将旧工具包中的同名 runtime 与新 worker 任意混用。

若选择默认 NSIS 后端，原命令仍可使用。NSIS 路径替换为你的实际安装路径：

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

## Rust 引擎真实进程验收

源码测试完成后，可在本机显式运行完整安装链。脚本使用新的随机产品 ID、全新安装目录和独立临时目录；真正创建当前用户的测试快捷方式和卸载登记。输出目录必须是本项目 `target/` 下尚不存在的路径。

```powershell
$acceptanceRoot = Join-Path $PWD ('target/rust-engine-acceptance/' + [Guid]::NewGuid().ToString('N'))
pwsh -NoProfile -File tests/harness/run-rust-engine.ps1 `
  -Runtime ./target/release/avelin-app.exe `
  -Cli ./target/release/avelin-cli.exe `
  -RustWorker ./target/release/avelin-engine.exe `
  -Sample ./target/release/nimbus-demo.exe `
  -OutputRoot $acceptanceRoot
```

覆盖实际构包、安装、取消、部分写入失败、版本/资源冲突拒绝、升级回滚、正常升级、样例启动、按归属卸载和重装。引擎未知结果使用独立产品实例，验证提交后确认丢失、恢复失败、只读诊断和重入拒绝。断言来自独立输入文件及磁盘、COM 快捷方式和注册表观察，不只看退出码或 receipt。

同时验收 NSIS A → Rust B 迁移时，使用另一个新输出目录，并提供 NSIS 与 bridge。这会运行上述原生链，再验证本次组件组合的正常迁移及提交前失败回滚，不是任意历史 NSIS 版本认证：

```powershell
$migrationRoot = Join-Path $PWD ('target/rust-engine-migration/' + [Guid]::NewGuid().ToString('N'))
pwsh -NoProfile -File tests/harness/run-rust-engine.ps1 `
  -Runtime ./target/release/avelin-app.exe `
  -Cli ./target/release/avelin-cli.exe `
  -RustWorker ./target/release/avelin-engine.exe `
  -Sample ./target/release/nimbus-demo.exe `
  -OutputRoot $migrationRoot `
  -Nsis 'C:/Program Files (x86)/NSIS/makensis.exe' `
  -Bridge './target/i686-pc-windows-msvc/release/avelin_nsis_bridge.dll'
```

查看输出目录的 `summary.json`、各进程记录、任务报告及资源快照。`PASSED` 只适用于该报告明确运行的用例与候选；失败、超时或未知现场不被强制清理。故意恢复失败的场景会保留文件、备份、事务标记和测试登记，用于诊断；脚本不会删除标记后强行重试。真实 GUI、异常进程中断、物理磁盘耗尽、签名和独立环境须由各自证据确认，不能由此脚本外推。

异常进程中断由独立脚本验收：在暂存完成、首个文件发布和完整提交后三个暂停点，分别核验准确进程身份后终止 worker 或协调器，检查存活进程、持久证据、诊断和重入行为。它会保留需要检查的现场及测试登记，不自动恢复或清理：

```powershell
$interruptionRoot = Join-Path $PWD ('target/rust-engine-interruption/' + [Guid]::NewGuid().ToString('N'))
pwsh -NoProfile -File tests/harness/run-rust-engine-interruption.ps1 `
  -Runtime ./target/release/avelin-app.exe `
  -Cli ./target/release/avelin-cli.exe `
  -RustWorker ./target/release/avelin-engine.exe `
  -Sample ./target/release/nimbus-demo.exe `
  -OutputRoot $interruptionRoot
```

开发验收的隐藏参数 `--engine-fault` 支持 `after-staging`、`before-registration`、`after-registration`、`before-commit`、`after-commit`、`rollback`；`--engine-pause` 提供 `prepared`、`after-first-publish`、`after-commit` 的有界暂停点，供测试在核验准确 PID 后观察或注入异常退出。它们只用于实际 Rust 安装/升级测试，不是接入方的安装选项或恢复命令。不要用终止进程代替正常取消。

### 本阶段结果（更新于 2026-10-13）

同一组 Release 组件在本机 Windows 当前用户下取得以下结果；真实进程脚本中的 Rust 子进程 `PATH` 只包含 Windows 系统目录，原生链不依赖 NSIS 或 bridge：

| 日期 | 验收 | 结果与范围 |
|---|---|---|
| 2026-10-12 | 源码测试 | 281 项通过：core 54、CLI 27、UI 58、app 74、engine 库 29 / 二进制 20、x86 bridge 19 |
| 2026-10-12 | 真实安装生命周期 | 19 项通过：安装、取消与部分写入回滚、版本及资源冲突拒绝、升级与提交前回滚、启动、归属卸载和用户修改保留、重装及直接卸载入口、NSIS 迁移、提交后确认丢失与恢复失败诊断 |
| 2026-10-12 | 真实进程中断 | 6 项通过：上述三个暂停点分别终止 worker 和协调器，核验 Unknown、现场保留和相应重入行为 |
| 2026-10-13 | 可见 GUI | 当前缩放下的一次安装、启动和卸载通过；安装与启动由工具截图及报告确认，卸载品牌、目录、成功画面与关闭由人工确认，后台独立核验结果及资源 |

GUI 验收使用与上述真实进程验收相同的 Release 组件，未切换系统缩放。安装进程退出 0；卸载报告为 `Succeeded`，后端和协调器退出 0，只读诊断为 `RecordedStateMatchesObservedResources`，两者身份均为 `NotRunning`。受管载荷、receipt、卸载组件、登记及任务锁已移除。本次结论覆盖这一次可见正常流程，不增加多缩放或独立环境认证范围。

迁移用例的 A 版是本次 runtime、CLI 配合既有 NSIS header 和 bridge 生成并实际安装的 NSIS 包；正常迁移和提交前失败恢复均已验证。这不认证所有历史 runtime 或早期实验包。中断结果只覆盖列出的事务边界，不证明任意指令处中断或机器重启均能恢复。

50 000 文件边界、性能、签名和独立 Windows 环境均未纳入本次结果。物理磁盘耗尽、重启续装和自动跨进程恢复也未由上述测试证明。每次更换组件或实际应用输入后，应保存新的组件摘要和验收报告。

## 项目结构

| 目录 | 职责 |
|---|---|
| `apps/avelin-cli` | `init/check/build/replay/diagnose`，配置、资源、原生载荷打包与 NSIS 脚本生成 |
| `apps/avelin-app` | 安装协调器、进程/IPC 生命周期、结果和恢复判断、GUI 入口 |
| `crates/avelin-core` | 共享配置、manifest、plan、receipt、协议和路径约束 |
| `crates/avelin-engine` | Rust worker、部署事务、系统资源与有限回滚 |
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

`scripts/ci-rust-windows.ps1` 提供完整 Rust 工具包验收：检查源码测试、构建 Release 组件、收集包含 worker 的依赖材料、调用公开打包脚本、解压并校验 ZIP，然后使用解压组件运行上述 19 项生命周期及 6 项中断场景。NSIS 迁移用例也必须通过；缺少前提时明确失败，不将跳过迁移的结果计为完整通过。

```powershell
pwsh -NoProfile -File scripts/ci-rust-windows.ps1 `
  -OutputDirectory target/rust-ci/run-001 `
  -NsisRoot 'C:/Program Files (x86)/NSIS' -RustToolchain stable `
  -CargoTargetDirectory target
```

输出目录必须是本仓库 `target/` 下的新目录。`-CargoTargetDirectory target` 显式复用已有 Cargo 缓存，仍执行构建并记录组件摘要；省略时使用本次运行独立的 Cargo 目录。该入口需要 NSIS 3.12 和 x86 bridge 构建目标，用于真实 NSIS 迁移和现有第三方收集流程；Rust 工具包自身不携带或调用 NSIS 编译器。

两项真实进程脚本会创建唯一测试产品的文件、快捷方式和当前用户卸载登记。中断与无法确认恢复的场景保留现场及相关登记，供核对日志和资源；脚本不会批量清理或自动修复。它们不打开 GUI，不改变缩放，不执行性能或签名测试。

`scripts/ci-windows.ps1` 保留 NSIS 工具包验收：从源码构建 Release、收集许可证据、打包、校验 ZIP，并执行工具包自己的安装 A → 升级 B → 运行样例 → 注册卸载正常链。两种后端各自保存报告。

```powershell
pwsh -NoProfile -File scripts/ci-windows.ps1 `
  -OutputDirectory target/maintainer-validation/run-001 `
  -NsisRoot 'C:/Program Files (x86)/NSIS' -RustToolchain stable
```

输出目录必须是新目录。默认记录实际 Rust 版本；复现指定工具链时可加 `-ExpectedRustVersion <exact-version>`，脚本只校验，不下载。NSIS 3.12 的启动器和实际编译器仍核对固定 SHA-256。Python 可通过 `-PythonPath` 指定。

`.github/workflows/rust-windows.yml` 使用 GitHub 托管的 `windows-2025`，准备 stable MSVC 工具链、两个目标和许可收集所需组件，再调用同一 Rust 完整入口。`scripts/prepare-windows-ci.ps1` 从官方发行源取得固定版本的 NSIS 便携 ZIP，校验归档及编译器摘要后解压到新目录；不运行系统安装器。托管入口不需要自托管 runner 或仓库 secret。

`.github/workflows/windows.yml` 是手工 self-hosted 入口，可选择 NSIS 或 Rust，默认保持 NSIS。它需要预先配置 `avelin-tools` runner、`AVELIN_NSIS_ROOT` 和可选 `AVELIN_PYTHON`，并显式检查此前验收的 Rust 1.97.1。完整日志和测试现场不自动作为公开附件上传。提交配置不代表云端已通过；实际运行状态只依据对应提交的 GitHub 结果。

完整工具包还需要[第三方材料](third-party.md)和[发布流程](releasing.md)中的核验。源码 CI、真实安装验收、签名及公开 Release 分别记录结果。
