# Avelin · 初栖

**用 Rust 原生界面，为 Windows 应用构建可品牌化的离线安装器。**

A Windows installer toolkit with a native Rust UI and an NSIS execution backend.

Avelin 将应用发布目录、TOML 配置和品牌资源构建成单个 Setup.exe。GPUI Kit 绘制界面，Rust 负责配置、构包与任务协调，NSIS 负责部署。项目处于 **开发者预览阶段**，当前 CLI 版本为 `0.1.0-preview.1`。

## 当前能力

- 配置初始化、资源与载荷检查、单 EXE 构建。
- 中英界面、浅深主题、PNG 品牌图标与文案覆盖。
- 当前用户安装、完整包升级、按资源归属卸载。
- 桌面/开始菜单快捷方式、显式静默入口、成功后手动启动。
- 安装取消、任务日志、安装记录、结果核验和只读诊断。
- 卸载保留未知用户文件和已修改的受管资源。

目标为 Windows x64，NSIS 插件为 x86。尚未提供全用户提权、服务/驱动、在线更新、差分包或 Rust 原生部署引擎。恢复边界见[兼容约定](docs/compatibility.md)。

## 从源码开始

需要 Windows x64、PowerShell 7.2+、Rust MSVC 工具链、MSVC x64/x86 Build Tools 和 Windows SDK。构建安装包还需要 Unicode NSIS 3.12。

```powershell
# 检查已安装工具，不自动安装或更新
pwsh -NoProfile -File scripts/doctor.ps1

# 格式与源码测试，不执行安装器
pwsh -NoProfile -File scripts/check-source.ps1

# 构建 GUI runtime、CLI、样例和 x86 bridge
cargo build --locked --release -p avelin-app -p avelin-cli --features avelin-app/gui --bins
cargo build --locked --release -p avelin-nsis-bridge --target i686-pc-windows-msvc
```

环境准备和从零生成样例 Setup 见[开发指南](docs/development.md)。构建不会自动安装；完整验收脚本会创建实际测试资源，执行前请阅读说明。

已有发布目录可用 CLI 初始化配置：

```powershell
avelin-cli init --directory ./MyInstaller --app-id com.example.myapp --name MyApp --app-version 1.0.0 --entry MyApp.exe --payload ./MyAppDist
avelin-cli check --config ./MyInstaller/avelin.toml
```

`avelin-cli` 需在 PATH 中，或改为实际 exe 路径。构包还需配套 runtime、bridge 和 NSIS，见[接入指南](docs/getting-started.md)与 [CLI 参数](docs/cli.md)。

## 代码结构

| 路径 | 职责 |
|---|---|
| `apps/avelin-cli` | 配置、载荷清单、NSIS 脚本生成和构包 |
| `apps/avelin-app` | GUI/静默入口、协调器、进程通信和结果核验 |
| `crates/avelin-core` | 模型、协议、路径检查和包格式 |
| `crates/avelin-ui` | GPUI 视图、语言和交互 |
| `crates/avelin-nsis-bridge` | x86 NSIS 插件 |
| `packaging` | NSIS 模板、公开文档清单和第三方材料输入 |
| `examples` | 样例配置、品牌和升级数据 |
| `scripts` / `tests/harness` | 开发工具和显式验收入口 |

[架构](docs/architecture.md) · [文档目录](docs/README.md) · [路线图](docs/roadmap.md) · [贡献指南](CONTRIBUTING.md)

## 项目状态

正常安装、升级、启动和卸载已有本机验证，这不代表任意应用或全部 Windows 环境均通过。源码检查与真实 GUI、故障恢复、签名和发布验收分别记录。请使用新测试产品 ID 和目录验证自己的应用。

个人环境报告、历史内部评审、原型和生成产物不进入源码提交。当前不承诺长期稳定 API 或任意历史包兼容，具体行为以公开契约为准。

## 许可证

Avelin 自有代码和文档采用 [MIT](LICENSE)，署名为 Avelin contributors。第三方文件保留各自许可证，见 [NOTICE](NOTICE.md) 和[第三方材料](docs/third-party.md)。
