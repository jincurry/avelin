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
schema_version = 2

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

[[components]]
id = "core"
required = true
name = { "zh-CN" = "主程序", "en-US" = "Application" }
description = { "zh-CN" = "应用运行所需文件", "en-US" = "Files required to run the application" }
paths = ["MyApp.exe"]
```

以上最小例子只有一个 EXE；真实目录的每个文件都需要归属。当前 `init` 会把扫描到的全部文件加入 `core.paths`。只有 `core` 时不显示组件选择区。需要选装时，将文件从 `core` 拆出并定义 `[[components]]`，见[可选组件使用指南](optional-components.md)与[通用示例](../examples/components/README.md)。旧 schema 1 配置仍受支持。

需要运行库前置检查、HTTPS 完整包更新或全用户安装时，在 schema 2 中分别添加 `[[prerequisites]]`、`[updates]`、`install.scope = "all_users"`。按应用实际需求配置，省略时保持当前用户离线安装。字段与完整示例见[Windows 部署指南](windows-deployment.md)。

组件能力需要当前源码构建的配套工具，已发布的 `v0.1.0-preview.1` 二进制尚不支持。

品牌配置参考 [Nimbus 配置](../examples/nimbus/avelin-m1.toml)、[中文覆盖](../examples/nimbus/locales/zh-CN.json)与[英文覆盖](../examples/nimbus/locales/en-US.json)。展示结构见 [presentation schema](schemas/presentation.schema.json)，仅支持注册的键和正确占位符。

## 生成安装包

使用同次构建的 `avelin-cli.exe`、`avelin-app.exe` 和 `avelin-engine.exe`：

```powershell
& '<CLI>' build --config ./MyInstaller/avelin.toml `
  --runtime '<avelin-app.exe>' --rust-worker '<avelin-engine.exe>' `
  --output ./MyInstaller/output/MyApp-1.0.0-Setup.exe
```

CLI 同目录已有 runtime 和 worker 时，可以省略 `--runtime` 和 `--rust-worker`。

当前原生载荷不压缩。包读取仍限制每层嵌入区域不超过 1 GiB，其中包括安装执行组件、runtime、应用文件和清单开销；这不是允许应用文件单独占满 1 GiB 的承诺。超限会拒绝构包。

每次使用新的输出路径。最终向应用用户分发 Setup；backend、manifest 等构建副产物留作核验。第三方声明及发行材料按[维护与发布](releasing.md)处理。

## 安装、升级、卸载

双击 Setup 后确认并开始；静默执行须显式选择：

```powershell
& '<Setup.exe>' --silent --install-dir '<新的绝对安装目录>' --report '<报告路径>'
& '<新版Setup.exe>' --silent --operation upgrade --install-dir '<原安装目录>' --report '<报告路径>'
```

升级保持产品 ID、当前用户和安装目录，使用更高的 SemVer 版本。安装后核对入口、快捷方式并按产品业务验证功能。卸载使用 Windows 已登记入口，并核对数据保留。应用目录外的数据兼容性由应用负责。

当前仅支持 Rust 安装之间的升级。旧版非 Rust 安装需使用原卸载入口卸载，再安装新包；请先处理原应用的数据保留要求。

若结果为 `Unknown` 或提示未完成任务，保留会话、日志、备份及事务标记，使用报告中的会话路径进行只读诊断：

```powershell
& '<CLI>' diagnose --session '<报告中的 session_dir>'
```

Rust 任务使用原始 Setup 检查，再明确执行恢复：

```powershell
& '<CLI>' recover --session '<报告中的 session_dir>' --setup '<原始Setup.exe>' --check
& '<CLI>' recover --session '<报告中的 session_dir>' --setup '<原始Setup.exe>' --apply
```

恢复会验证任务、安装归属和实际资源。未提交的变更可在证据完整时回退，已提交操作经核验后确认；记录不足或出现外部冲突时保留现场并拒绝。不要删除标记后直接重新安装。缺少新恢复证据的历史任务不自动获得跨进程恢复能力。

## 修复已经安装的应用

当前源码构建的新安装可以从 Windows 系统的“更改”入口打开维护页，使用安装目录中的已验证离线载荷；有可选组件时可切换“更改组件”。系统按钮因版本和页面而异，直接命令及旧安装如何获得入口见[Windows 维护入口](windows-maintenance.md)。

保留与已安装版本对应的原始 Rust Setup。修复要求 manifest 和 backend 内容摘要与原安装一致；不同内容重建的同版本包会被拒绝。

```powershell
& '<原始Setup.exe>' --silent --operation repair --install-dir '<原安装目录>' --report '<新的报告路径>'
```

默认补回缺失资源，保留已修改的文件并在结果中列出。需要恢复修改过的受管文件时，可明确添加 `--repair-overwrite`，程序先备份再替换。用户数据和未知文件保持原样。修复前如果存在未完成事务，须先恢复。界面与静默入口共用资源保护规则，细节见[恢复与修复](maintenance.md)。

操作限制、取消和结果解释见[兼容约定](compatibility.md)。Nimbus 是测试样例，不能代替真实应用验收。

## 安装后更改组件

从系统“更改”入口或双击同版本同构建的原始 Setup，在维护页选“更改组件”，勾选最终需要保留的内容。清单显示将添加/移除的项，确认后执行；没有改动时不能提交。修复只恢复原来已选的内容，不会补装未选择的组件。

静默模式明确传入完整目标集合：

```powershell
& '<原始Setup.exe>' --silent --operation modify --install-dir '<原安装目录>' `
  --components core,component-a --report '<新的报告路径>'
```

`core,component-a` 表示最终只选择这两项，而不是往当前安装追加。升级及旧记录迁移规则见[可选组件](optional-components.md)。
