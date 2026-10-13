# 接入现有应用

Avelin 接收已构建的 Windows 应用发布目录。入口必须是目录内的 EXE；初始化不会复制、编译或运行应用。源码构建见[开发指南](development.md)，参数定义见 [CLI](cli.md)。

## 准备输入

- 稳定产品 ID、显示名称、SemVer 版本和入口相对路径。
- 完整发布目录，以及应用需要的运行库。
- 用户数据位置和卸载后的保留要求。
- 可选的 PNG logo、品牌色和中英文案覆盖。

当前仅支持当前用户范围。管理员、服务、驱动和任意安装脚本不在当前能力内；应用自己的数据库迁移及运行库可用性需另行确认。

## 初始化与检查

```text
MyInstaller/
  payload/MyApp.exe
  branding/logo.png       可选
  locales/zh-CN.json      可选
  locales/en-US.json      可选
  avelin.toml             由 init 生成
```

将发布文件放入 payload 后，在 MyInstaller 的父目录执行（将 `<CLI>` 替换为 exe 路径）：

```powershell
& '<CLI>' init --directory ./MyInstaller --app-id com.example.myapp --name MyApp --app-version 1.0.0 --entry MyApp.exe --payload ./MyInstaller/payload
& '<CLI>' check --config ./MyInstaller/avelin.toml
```

配置目录必须在载荷之外。载荷位于配置目录内时，生成的 TOML 使用相对路径；外部载荷使用绝对路径。现有配置不会被覆盖。

```toml
schema_version = 1

[app]
id = "com.example.myapp"
name = "MyApp"
version = "1.0.0"
entry = "MyApp.exe"

[payload]
directory = "payload"

[install]
directory_name = "MyApp"
desktop_shortcut = false
start_menu_shortcut = true
```

品牌配置参考 [Nimbus 配置](../examples/nimbus/avelin-m1.toml)、[中文覆盖](../examples/nimbus/locales/zh-CN.json)与[英文覆盖](../examples/nimbus/locales/en-US.json)。展示结构见 [presentation schema](schemas/presentation.schema.json)，仅支持注册的键和正确占位符。

## 生成安装包

后端在构包命令选择，不需要修改 `avelin.toml`。使用同次源码构建的 CLI、runtime 和所选后端组件；此前只包含 NSIS 的工具包不会自动获得 Rust worker。

### Rust 原生引擎

准备 `avelin-cli.exe`、`avelin-app.exe` 和 `avelin-engine.exe`，源码命令见[开发指南](development.md)。明确选择 Rust 后端：

```powershell
& '<CLI>' build --engine rust --config ./MyInstaller/avelin.toml `
  --runtime '<avelin-app.exe>' --rust-worker '<avelin-engine.exe>' `
  --output ./MyInstaller/output/MyApp-1.0.0-Rust-Setup.exe
```

CLI 同目录已有这两个组件时，可以省略 `--runtime` 和 `--rust-worker`。此路径构建和安装均不依赖 NSIS 或 bridge；不能同时传入 `--bridge` / `--nsis`。

当前原生载荷不压缩。包读取仍限制每层嵌入区域不超过 1 GiB，其中包括安装执行组件、runtime、应用文件和清单开销；这不是允许应用文件单独占满 1 GiB 的承诺。超限会拒绝构包。

### NSIS 后端（默认）

原有构包方式保留；省略 `--engine` 等同于 `--engine nsis`：

```powershell
& '<CLI>' build --config ./MyInstaller/avelin.toml `
  --runtime '<avelin-app.exe>' --bridge '<avelin_nsis_bridge.dll>' `
  --nsis '<NSIS目录/makensis.exe>' --output ./MyInstaller/output/MyApp-1.0.0-Setup.exe
```

NSIS 便携工具包可自动定位同目录 runtime/bridge 与 tools/nsis。环境变量 NSIS_MAKENSIS 优先于包内 NSIS，构建 JSON 记录实际选用路径；Rust 构包不使用该变量。

每次使用新的输出路径。最终向应用用户分发 Setup；backend、manifest 等构建副产物留作核验。第三方声明及发行材料按[维护与发布](releasing.md)处理。

## 安装、升级、卸载

双击 Setup 后确认并开始；静默执行须显式选择：

```powershell
& '<Setup.exe>' --silent --install-dir '<新的绝对安装目录>' --report '<报告路径>'
& '<新版Setup.exe>' --silent --operation upgrade --install-dir '<原安装目录>' --report '<报告路径>'
```

升级保持产品 ID、当前用户和安装目录，使用更高的 SemVer 版本。安装后核对入口、快捷方式并按产品业务验证功能。卸载使用 Windows 已登记入口，并核对数据保留。应用目录外的数据兼容性由应用负责。

这组运行参数同时适用于两个后端；不在用户安装时重新选择引擎。Rust 升级可接管满足当前记录与完整性约定的 NSIS 安装，并交接驻留卸载入口；应先用自己的 A/B 测试包验证迁移。缺少必要完整性字段的早期实验安装不在自动接管范围。

若结果为 `Unknown` 或提示未完成任务，保留会话、日志、备份及事务标记，使用报告中的会话路径进行只读诊断：

```powershell
& '<CLI>' diagnose --session '<报告中的 session_dir>'
```

当前没有自动跨进程恢复命令。不要删除标记后直接重新安装；诊断命令也不会代你执行回滚或重试。

操作限制、取消和结果解释见[兼容约定](compatibility.md)。Nimbus 是测试样例，不能代替真实应用验收。
