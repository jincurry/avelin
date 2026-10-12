# CLI 参数契约

CLI 工具预览版本为 `0.1.0-preview.1`。这是构建工具的版本，不是安装包应用版本、runtime ABI 或 manifest/schema 版本；其他 crate 版本保持原值，schema 仍为 `1`。

## 创建配置

```powershell
avelin-cli init --directory .\project --app-id dev.example.notes --name '示例 Notes' --app-version 1.0.0 --entry Notes.exe --payload .\project\dist
avelin-cli check --config .\project\avelin.toml
```

六个参数均必填，命令不交互询问：

| 参数 | 含义 |
|---|---|
| `--directory` | 配置目录；必要时逐层创建，最终只新建 `avelin.toml` |
| `--app-id` | 现有 schema 支持的 ASCII 产品 ID；同时作为初始 `install.directory_name` |
| `--name` | 应用显示名，遵循现有 manifest 和 NSIS INI 字符限制 |
| `--app-version` | 应用的 SemVer 版本，独立于 CLI 预览版本 |
| `--entry` | 已存在的 payload 内相对文件名，可含子目录；拒绝绝对路径、`..` 和模糊 Windows 路径 |
| `--payload` | 已有应用文件目录；相对参数从当前工作目录解析，不是从 `--directory` 解析 |

如果载荷在配置目录内，TOML 保存相对路径（上例为 `dist`），便于整体移动。外部载荷保存明确的绝对路径；移动配置后需要自行更新该路径。配置本身必须在载荷目录之外，避免把生成的配置意外收进载荷。

初始快捷方式选项为 Desktop `false`、Start Menu `true`。配置不添加自定义 UI 字段，使用现有默认展示；需要品牌配置时可按 [接入指南](getting-started.md) 编辑生成文件。

命令先通过与 `check` 相同的 TOML、资源清单和 Manifest 校验，再写配置。用户文本通过 TOML 序列化器转义；非法 ID/版本/入口、空载荷或现有配置均失败。它不复制、不修改、不执行应用文件；Windows 路径中的 reparse point/junction 被拒绝，写入期间固定目录句柄，并以 `CREATE_NEW` 拒绝覆盖。该初始化写入保护当前只支持 Windows。I/O 写入失败时保留已创建的文件或空目录供检查，不自动删除目录，也不在重试时覆盖文件。

成功输出 JSON 包含 `config`、`payload`（均为绝对路径）、`payload_path_mode`（`relative_to_config` 或 `absolute`）、`app_id`、`app_version`、`entry`、`files`、`validated: true`、`copied_application_files: false`、`executed_application: false` 和 `tool_version`。

## 检查与构建

`check --config <avelin.toml>` 读取并验证已有配置、payload 和品牌资源，成功仍输出 Manifest JSON。它不要求 NSIS、runtime 或 bridge；配置有效不代表构建依赖已齐备。

```powershell
avelin-cli build --config .\project\avelin.toml --output .\out\Notes-Setup.exe
```

组件解析顺序：

| 组件 | 优先级 |
|---|---|
| runtime | 显式 `--runtime` → CLI 同目录 `avelin-app.exe` |
| x86 bridge | 显式 `--bridge` → CLI 同目录 `avelin_nsis_bridge.dll` → 原仓库 `target/i686-pc-windows-msvc/<profile>` 开发回退 |
| NSIS | 显式 `--nsis` → `NSIS_MAKENSIS` → CLI 所在工具包的 `tools/nsis/makensis.exe` →系统 NSIS 安装位置 |

推荐工具包布局是根目录并列放置 CLI/runtime/bridge，以及完整 `tools/nsis` 目录（包含 `Bin`、`Include`、`Stubs`、`Plugins` 等）。也兼容已有 `bin`、`bin/current` 和 `bin/previous` 布局的工具包根目录。仓库 bridge 回退优先匹配 CLI 的 debug/release 目录，之后尝试另一 profile；已携带同目录 bridge 的工具包不使用该回退。

显式参数或环境变量指定无效文件时直接失败，不悄悄替换。缺少依赖会给出实际路径及 `--runtime`、`--bridge`、`--nsis`/`NSIS_MAKENSIS` 的操作提示。只复制 `makensis.exe` 不足以组成 NSIS；编译失败保留构建目录及诊断。

构建成功 JSON 保留原有 `setup`、`backend`、`manifest`、`stage`、`signed`、`files` 和 `makensis_wall_ms`，新增实际解析后的绝对路径 `runtime_path`、`bridge_path`、`nsis_path`，以及 `tool_version`。这些字段可用于确认实际使用了工具包内组件。

输出 Setup 及 `.backend.exe`、`.manifest.json` 副产物沿用已有“不得覆盖”保护。以上功能只负责配置和构包，不改变安装、升级、卸载或恢复语义。
