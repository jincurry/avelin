# Avelin · 初栖

**用 Rust 原生界面，为 Windows 应用构建可品牌化的离线安装器。**

A Windows installer toolkit with a native Rust UI and deployment engine.

Avelin 将应用发布目录、TOML 配置和品牌资源构建成单个 Setup.exe。GPUI Kit 绘制界面，Rust 负责配置、构包与任务协调；部署统一使用 Rust 原生引擎。项目处于 **开发者预览阶段**，当前 CLI 版本为 `0.1.0-preview.1`。

## 当前能力

- 配置初始化、资源与载荷检查、单 EXE 构建。
- 中英界面、浅深主题、PNG 品牌图标与文案覆盖。
- 当前用户安装、完整包升级、按资源归属卸载。
- 桌面/开始菜单快捷方式、显式静默入口、成功后手动启动。
- 安装取消、任务日志、安装记录、结果核验和只读诊断。
- Rust 中断事务的显式恢复；同版本修复缺失资源，默认保留修改内容。
- 卸载保留未知用户文件和已修改的受管资源。
- `build` 使用原生 Rust worker 构包和部署，工具链只需 Windows x64 组件。

CLI、GUI runtime 和 Rust worker 均面向 Windows x64。尚未提供全用户提权、服务/驱动、在线更新或差分包。恢复边界见[兼容约定](docs/compatibility.md)。

## 下载预览工具包

[v0.1.0-preview.1](https://github.com/jincurry/avelin/releases/tag/v0.1.0-preview.1) 提供 Windows x64 ZIP 与 SHA-256。解压后 `bin/` 内 CLI、runtime、worker 可配套构包，无需从源码编译；另附 Nimbus 演示 Setup、文档和第三方材料。该预览版未签名，校验与使用步骤见 Release 说明及包内 README。

可选安装组件仍处于[设计提案](docs/optional-components.md)阶段，当前预览版不包含此能力。

## 从源码开始

需要 Windows x64、PowerShell 7.2+、Rust MSVC 工具链、MSVC Build Tools 和 Windows SDK。构建工具只需 x64 目标。

```powershell
# 检查已安装工具，不自动安装或更新
pwsh -NoProfile -File scripts/doctor.ps1

# 格式与源码测试，不执行安装器
pwsh -NoProfile -File scripts/check-source.ps1

# 构建 GUI runtime、CLI、Rust 引擎和样例
cargo build --locked --release -p avelin-engine -p avelin-cli -p avelin-app --features avelin-app/gui --bins
```

环境准备和从零生成样例 Setup 见[开发指南](docs/development.md)。构建不会自动安装；完整验收脚本会创建实际测试资源，执行前请阅读说明。

分发可复用的 Rust 开发工具包时，使用 `scripts/package-rust-devkit.ps1`，并通过 `scripts/Verify-RustDevKit.ps1` 校验。`scripts/ci-rust-windows.ps1` 串联源码构建、工具包与真实流程验收，另有 GitHub 托管 Windows 工作流。参数及步骤见[维护与发布](docs/releasing.md#rust-工具包)。

已有发布目录可用 CLI 初始化配置：

```powershell
avelin-cli init --directory ./MyInstaller --app-id com.example.myapp --name MyApp --app-version 1.0.0 --entry MyApp.exe --payload ./MyAppDist
avelin-cli check --config ./MyInstaller/avelin.toml
```

`avelin-cli` 需在 PATH 中，或改为实际 exe 路径。使用同次构建的 runtime 和 Rust worker 生成原生包：

```powershell
& './target/release/avelin-cli.exe' build `
  --config ./MyInstaller/avelin.toml `
  --runtime ./target/release/avelin-app.exe `
  --rust-worker ./target/release/avelin-engine.exe `
  --output ./MyInstaller/output/MyApp-Rust-Setup.exe
```

原生载荷目前不压缩，沿用 1 GiB 嵌入区域上限；需为 worker、runtime 和清单留出空间。组件定位、构包与使用步骤见[接入指南](docs/getting-started.md)和 [CLI 参数](docs/cli.md)。

## 代码结构

| 路径 | 职责 |
|---|---|
| `apps/avelin-cli` | 配置、载荷清单和原生载荷构包 |
| `apps/avelin-app` | GUI/静默入口、协调器、进程通信和结果核验 |
| `crates/avelin-core` | 模型、协议、路径检查和包格式 |
| `crates/avelin-engine` | 原生 Rust 部署、归属卸载、持久事务、恢复和修复 |
| `crates/avelin-ui` | GPUI 视图、语言和交互 |
| `packaging` | 公开文档清单和第三方材料输入 |
| `examples` | 样例配置、品牌和升级数据 |
| `scripts` / `tests/harness` | 开发工具和显式验收入口 |

[架构](docs/architecture.md) · [文档目录](docs/README.md) · [路线图](docs/roadmap.md) · [贡献指南](CONTRIBUTING.md)

恢复与修复的使用方式、原包要求及冲突处理见[维护操作指南](docs/maintenance.md)。旧版非 Rust 安装需用原卸载器卸载后再安装新包；当前版本不再提供跨引擎迁移。历史中断记录不会自动补出新恢复协议所需证据。

## 项目状态

当前源码仅保留 Rust 引擎，安装、升级、卸载、恢复和修复共用同一条执行链。验收范围与复现入口见[开发指南](docs/development.md#rust-引擎真实进程验收)。性能、签名和独立环境验证不在本轮范围。请使用新测试产品 ID 和目录验证自己的应用。

个人环境报告、历史内部评审、原型和生成产物不进入源码提交。当前不承诺长期稳定 API 或任意历史包兼容，具体行为以公开契约为准。

## 许可证

Avelin 自有代码和文档采用 [MIT](LICENSE)，署名为 Avelin contributors。第三方文件保留各自许可证，见 [NOTICE](NOTICE.md) 和[第三方材料](docs/third-party.md)。
