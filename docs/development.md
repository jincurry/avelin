# 源码开发

CLI、协调器、GPUI 窗口和 Rust worker 均运行在 Windows x64。本文从仓库源码构建，不依赖历史安装包或已有 `target/` 产物。

## 环境

在仓库根目录使用 **64 位 PowerShell 7.2+**。预先准备：

- Rust stable 的 `x86_64-pc-windows-msvc` 工具链和 `rustfmt`。
- Visual Studio Build Tools 的 C++ x64 工具及 Windows SDK。
- 生成第三方清单和完整工具包时另需 Python 3.11+；收集器只使用标准库。

安装 Rust 后，可显式准备组件和目标：

```powershell
rustup toolchain install stable --profile minimal --component rustfmt --component clippy
rustup target add --toolchain stable x86_64-pc-windows-msvc
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

该入口依次检查格式，运行 core、CLI、Rust 引擎、带 GUI 功能的 app、UI 的源码测试。UI 的视图测试使用测试上下文，不打开安装器窗口。默认忽略的时序探针仍不运行，50 000 文件边界测试被显式跳过。它不调用下面的真实进程安装验收，也不运行性能测试。

日志和 `summary.json` 写入新的 `target/source-check/<run>/`。可指定 `-OutputDirectory`，但目录必须尚不存在；`-RustToolchain` 允许选择另一个已安装的 x64 MSVC 工具链。源码测试通过不等于真实安装流程验收通过。

定向检查示例：

```powershell
cargo fmt --all -- --check
cargo test --locked -p avelin-cli
cargo test --locked -p avelin-engine
cargo test --locked -p avelin-core -- --skip manifest_validates_fifty_thousand_files_and_rejects_the_next
cargo test --locked -p avelin-ui
cargo test --locked -p avelin-app --features gui
```

## 从源码生成一个 Setup

以下命令在仓库根目录执行，假设使用默认 `target/`；若配置了 `CARGO_TARGET_DIR`，需相应调整产物路径。保持 `CARGO_BUILD_TARGET` 未设置。每一步成功后再执行下一步。

先构建 x64 GUI runtime、CLI、Rust 引擎和可运行样例：

```powershell
cargo build --locked --release -p avelin-engine -p avelin-cli -p avelin-app --features avelin-app/gui --bins
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
& './target/release/avelin-cli.exe' build `
  --config (Join-Path $demoRoot 'avelin.toml') `
  --runtime './target/release/avelin-app.exe' `
  --rust-worker './target/release/avelin-engine.exe' `
  --output (Join-Path $demoRoot 'Avelin-Sample-Rust-Setup.exe')
```

Rust 载荷目前不压缩，包大小限制与开销见 [CLI 契约](cli.md)。保留 Release 产物的配套关系，不将旧工具包中的同名 runtime 与新 worker 任意混用。

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

恢复与修复由维护脚本验收。它覆盖未提交回退、提交后确认、恢复再次中断与重复执行、活动任务与身份不符拒绝，以及补回缺失资源、保留修改、覆盖前备份和不匹配包拒绝；所有场景使用独立测试产品：

```powershell
$maintenanceRoot = Join-Path $PWD ('target/rust-maintenance/' + [Guid]::NewGuid().ToString('N'))
pwsh -NoProfile -File tests/harness/run-rust-maintenance.ps1 `
  -Runtime ./target/release/avelin-app.exe `
  -Cli ./target/release/avelin-cli.exe `
  -RustWorker ./target/release/avelin-engine.exe `
  -Sample ./target/release/nimbus-demo.exe `
  -OutputRoot $maintenanceRoot
```

开发验收的隐藏参数 `--engine-fault` 支持 `after-staging`、`before-registration`、`after-registration`、`before-commit`、`after-commit`、`rollback`；`--engine-pause` 提供 `prepared`、`after-first-publish`、`after-first-remove`、`after-commit`、`recovery-after-first-mutation` 的有界暂停点，供测试在核验准确 PID 后观察或注入异常退出。它们只用于实际 Rust 事务测试，不是接入方的安装选项或恢复命令。不要用终止进程代替正常取消。

Windows 系统维护入口与占用提示另有 13 项真实进程验收。它检查应用登记、原 Setup 移走后的修复/组件更改、真实文件句柄阻塞与释放后重试，以及已安装资料完整时的恢复入口：

```powershell
$windowsMaintenanceRoot = Join-Path $PWD ('target/windows-maintenance/' + [Guid]::NewGuid().ToString('N'))
pwsh -NoProfile -File tests/harness/run-windows-maintenance-entry.ps1 `
  -Runtime ./target/release/avelin-app.exe `
  -Cli ./target/release/avelin-cli.exe `
  -RustWorker ./target/release/avelin-engine.exe `
  -Sample ./target/release/nimbus-demo.exe `
  -OutputRoot $windowsMaintenanceRoot
```

占用者是测试自己启动的子进程，通过释放标记正常退出。脚本确认被阻止时尚未创建执行任务、未改变受管内容或登记，不关闭用户应用。它不打开 Windows 设置或原生安装器窗口；界面状态由 headless UI 测试验证，真实桌面视觉与朗读另行确认。

### 结果记录

可选组件使用独立的真实进程脚本，覆盖默认/显式安装、更改、升级继承、冲突保护与中断恢复，共 29 项：

```powershell
$componentsRoot = Join-Path $PWD ('target/rust-components/' + [Guid]::NewGuid().ToString('N'))
pwsh -NoProfile -File tests/harness/run-rust-components.ps1 `
  -Runtime ./target/release/avelin-app.exe `
  -Cli ./target/release/avelin-cli.exe `
  -RustWorker ./target/release/avelin-engine.exe `
  -Sample ./target/release/nimbus-demo.exe `
  -OutputRoot $componentsRoot
```

旧 schema 1 安装迁移使用另一组 8 项兼容检查，需保留旧版配套 CLI、runtime、worker，并通过 `-LegacyTools` 指向其目录。脚本会实际确认旧工具生成 schema 1，随后检查原有组件继承、新默认组件不自动加入，以及混合旧/新文件的组件必须明确选择：

```powershell
$legacyRoot = Join-Path $PWD ('target/rust-components-legacy/' + [Guid]::NewGuid().ToString('N'))
pwsh -NoProfile -File tests/harness/run-rust-components-legacy.ps1 `
  -Runtime ./target/release/avelin-app.exe `
  -Cli ./target/release/avelin-cli.exe `
  -RustWorker ./target/release/avelin-engine.exe `
  -LegacyTools '<旧版配套工具所在目录>' `
  -OutputRoot $legacyRoot
```

该脚本使用新的测试产品与安装目录，保留报告和证据；已完成的产品通过自身卸载入口清理。清单 GUI 的复选框、确认失效、维护切换与键盘焦点另有 headless UI 测试，它们不等同于原生像素和讲述人验收。

基础真实进程验收包含 17 项生命周期、6 项异常中断和 17 项恢复/修复，共 40 项；组件由上述独立脚本扩展覆盖。每组报告保存实际组件摘要、进程结果和资源证据，以各自 `summary.json` 的用例为准。源码测试、真实进程、可见 GUI 和环境验证分别记录，不相互替代。

此前安装、启动、卸载、修复和恢复的 GUI 证据保留；新增组件清单的 headless 交互测试不代表原生窗口及讲述人已经人工验收。性能、签名及独立环境测试不在本轮范围。

## 项目结构

| 目录 | 职责 |
|---|---|
| `apps/avelin-cli` | `init/check/build/replay/diagnose/recover`，配置、资源、原生载荷打包与恢复入口 |
| `apps/avelin-app` | 安装协调器、进程/IPC 生命周期、结果和恢复判断、GUI 入口 |
| `crates/avelin-core` | 共享配置、manifest、plan、receipt、协议和路径约束 |
| `crates/avelin-engine` | Rust worker、部署事务、系统资源与有限回滚 |
| `crates/avelin-ui` | GPUI 视图与交互；接收状态，不自行认定安装成功 |
| `examples` | 示例配置、品牌资源和用于构建的输入 |
| `scripts` | 源码检查、构建、许可收集、打包、完整验收入口 |
| `tests/harness` | 真实运行验收脚本；普通源码检查不会自动调用 |

## 两类 CI

### 贡献者源码检查

`.github/workflows/source.yml` 在 PR、默认分支推送和手工触发时运行，使用 GitHub-hosted `windows-2025`。它准备 stable 工具链和 x64 target，调用同一个 `check-source.ps1`，保存结果。该工作流只有读取仓库权限，不需要仓库 secret，不使用自托管 runner，不调用安装流程。

GitHub 官方将 `windows-2025` 列为 Windows x64 hosted runner 标签；实际执行机器和 Rust 版本以 workflow 日志为准。[GitHub runner 文档](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)

### 维护者完整工具包验收

`scripts/ci-rust-windows.ps1` 检查源码、构建 Release、收集依赖材料、生成工具包、解压校验，再使用解压组件运行生命周期、中断、恢复/修复、可选组件及 Windows 维护入口/占用重试场景。

```powershell
pwsh -NoProfile -File scripts/ci-rust-windows.ps1 `
  -OutputDirectory target/rust-ci/run-001 -RustToolchain stable `
  -CargoTargetDirectory target
```

输出必须是本仓库 `target/` 下的新目录。`-CargoTargetDirectory target` 显式复用 Cargo 缓存，仍执行构建并记录组件摘要；省略时使用本次运行独立目录。Python 可通过 `-PythonPath` 指定。

完整 CI 的离线 schema 检查还需要固定的 Python 依赖；CI 脚本本身不安装工具或库。在调用前准备隔离目录并设置 `PYTHONPATH`，workflow 已执行同一步骤：

```powershell
python -m pip install --no-deps --only-binary=:all: --target .\target\schema-tools -r .\tests\harness\schema-requirements.txt
$env:PYTHONPATH = Join-Path $PWD 'target/schema-tools'
python tests/harness/check-windows-feature-schemas.py
```

新结构检查覆盖范围、运行库、更新 feed 与维护命令；在线模块测试使用可控响应检查重定向、大小、摘要、包身份、取消及原子输出，并在本地启动临时自签 TLS 服务确认 Windows 拒绝不受信任证书。测试不更改宿主根证书、不执行下载的测试 EXE。机器级流程需要提升的终端，创建的测试产品使用独立 ID 和受控 Program Files 目录；未提升时报告未执行，不算通过。

真实进程脚本会创建唯一测试产品的文件、快捷方式和卸载登记。当前用户场景使用 HKCU 和个人快捷方式；提升后的全用户场景另使用受控 Program Files 目录、HKLM 和公共快捷方式。维护脚本只恢复其明确创建并核验的测试任务；其余中断与无法确认恢复的场景保留现场及相关登记，供核对日志和资源。它们不打开 GUI，不改变缩放，不执行性能或签名测试。全用户真实流程的当前验收边界见[Windows 部署指南](windows-deployment.md#3-全用户安装与提权)。

`.github/workflows/rust-windows.yml` 使用 GitHub 托管的 `windows-2025`，准备 stable MSVC x64 工具链后调用同一入口。`.github/workflows/windows.yml` 是手动自托管 Rust 验收入口，需要预先配置 `avelin-tools` runner、其当前明确校验的 Rust `1.97.1` 和可选 `AVELIN_PYTHON`。完整日志和测试现场不自动上传；实际云端结果以对应提交的执行状态为准。

完整工具包还需要[第三方材料](third-party.md)和[发布流程](releasing.md)中的核验。源码 CI、真实安装验收、签名及公开 Release 分别记录结果。
