# CLI 参数契约

CLI 工具预览版本为 `0.1.0-preview.1`。当前源码新增可选组件，配置、Manifest、Plan 与 Receipt 使用 schema `2`，仍读取支持范围内的旧 schema `1`。已发布的同名 `v0.1.0-preview.1` 二进制不含组件能力，不能仅凭版本字符串判断能力；请配套使用当前源码构建的 CLI、runtime 和 worker。Rust 中断恢复凭据有自己的版本，不能据此推断旧记录可恢复。

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
| `--name` | 应用显示名，遵循 manifest 的长度和控制字符约束 |
| `--app-version` | 应用的 SemVer 版本，独立于 CLI 预览版本 |
| `--entry` | 已存在的 payload 内相对文件名，可含子目录；拒绝绝对路径、`..` 和模糊 Windows 路径 |
| `--payload` | 已有应用文件目录；相对参数从当前工作目录解析，不是从 `--directory` 解析 |

如果载荷在配置目录内，TOML 保存相对路径（上例为 `dist`），便于整体移动。外部载荷保存明确的绝对路径；移动配置后需要自行更新该路径。配置本身必须在载荷目录之外，避免把生成的配置意外收进载荷。

初始快捷方式选项为 Desktop `false`、Start Menu `true`。当前 `init` 生成 schema `2`，将已扫描文件明确分配给必选 `core` 组件。添加可选组件时从 `core.paths` 移出相应文件，再定义新组；文件必须且只能归属一个组。配置不添加自定义 UI 字段，使用默认展示；品牌配置见[接入指南](getting-started.md)。

命令先通过与 `check` 相同的 TOML、资源清单和 Manifest 校验，再写配置。用户文本通过 TOML 序列化器转义；非法 ID/版本/入口、空载荷或现有配置均失败。它不复制、不修改、不执行应用文件；Windows 路径中的 reparse point/junction 被拒绝，写入期间固定目录句柄，并以 `CREATE_NEW` 拒绝覆盖。该初始化写入保护当前只支持 Windows。I/O 写入失败时保留已创建的文件或空目录供检查，不自动删除目录，也不在重试时覆盖文件。

成功输出 JSON 包含 `config`、`payload`（均为绝对路径）、`payload_path_mode`（`relative_to_config` 或 `absolute`）、`app_id`、`app_version`、`entry`、`files`、`validated: true`、`copied_application_files: false`、`executed_application: false` 和 `tool_version`。

## 检查与构建

`check --config <avelin.toml>` 读取并验证已有配置、payload 和品牌资源，成功仍输出 Manifest JSON。它不要求 runtime 或 Rust worker；配置有效不代表构建依赖已齐备。

```powershell
avelin-cli build --config .\project\avelin.toml --output .\out\Notes-Setup.exe
```

`build` 仅使用 Rust 引擎，无需选择后端。应用身份和版本规则由 TOML 与安装记录定义。

### 组件定位

```powershell
avelin-cli build --config .\project\avelin.toml `
  --runtime .\tools\avelin-app.exe --rust-worker .\tools\avelin-engine.exe `
  --output .\out\Notes-Rust-Setup.exe
```

| 组件 | 优先级 |
|---|---|
| x64 runtime | 显式 `--runtime` → CLI 同目录 `avelin-app.exe` |
| x64 Rust worker | 显式 `--rust-worker` → CLI 同目录 `avelin-engine.exe` |

这两个默认位置只依赖 CLI 所在目录，不读取当前工作目录的同名程序。缺少文件或显式路径无效会报错。旧 `--engine`、`--bridge`、`--nsis` 参数已移除，传入会返回参数错误。

构建输出 JSON 含 `engine: "rust"`、`setup`、`backend`、`manifest`、`stage`、`signed`、`files`、`runtime_path`、`rust_worker_path` 和 `tool_version`，路径记录实际选用的组件。

原生载荷使用有版本号、长度和摘要约束的未压缩格式。两层 EXE overlay 仍使用现有包边界校验；每层被嵌入内容加 manifest 不得超过 1 GiB，manifest 本身不超过 32 MiB。Rust worker、runtime 和元数据也占用空间，不能把包上限当成纯应用载荷额度。

### 输出与运行

构建输出 Setup 及 `.backend.exe`、`.manifest.json` 副产物，沿用已有“不得覆盖”保护。Setup 是给应用用户使用的完整入口；副产物用于诊断与核验。构建只生成文件，不执行安装，也不等于安装成功。

运行步骤见[接入指南](getting-started.md#安装升级卸载)，退出码和恢复约定见[兼容约定](compatibility.md)。`replay` 重放已解码事件用于检查状态映射；`diagnose --session <目录>` 只读观察已有任务，不会移除锁、修改标记或执行恢复。

## 检查与恢复中断事务

组件选择与同版本更改见本文下方的对应章节。恢复使用中断前已冻结的选集，不能在恢复命令中改选。

```powershell
avelin-cli recover --session '<原任务会话目录>' --setup '<该任务的原始Setup.exe>' --check
avelin-cli recover --session '<原任务会话目录>' --setup '<该任务的原始Setup.exe>' --apply
```

不提供 `--check` / `--apply` 时默认为只读检查；两者不能同时使用。CLI 启动所提供的原始 Setup，复用 runtime 和独立 Rust worker 的恢复流程。检查不会清除事务标记、释放遗留产品锁或变更应用文件；执行恢复需要明确的 `--apply`。原任务仍活动、源包或安装身份不符、恢复证据不足时拒绝。

对应 runtime 入口为：

```powershell
& '<原始Setup.exe>' --silent --operation recover --recovery-session '<原任务会话目录>'
& '<原始Setup.exe>' --silent --operation recover --recovery-session '<原任务会话目录>' --recovery-apply
```

检查结论与执行结果应分别读取，不能用“检查命令正常结束”判断恢复已经完成。已提交结果需重新核验后确认；未提交的变更按可验证备份回退。恢复保留独立结果记录，不把旧任务原本的失联报告改写成当时已确认成功。详见[恢复与修复](maintenance.md)。

报告中的 `maintenance_status` 区分 `RecoveryChecked` 与 `RecoveryComplete`，`recovery` 字段记录具体决定及是否执行。GUI 在检查后展示维护确认页，用户确认才执行恢复。

## 同版本修复

修复由原始 Setup 执行：

```powershell
& '<原始Setup.exe>' --silent --operation repair --install-dir '<原安装目录>' --report '<新的报告路径>'
& '<原始Setup.exe>' --silent --operation repair --install-dir '<原安装目录>' --repair-overwrite --report '<新的报告路径>'
```

默认补回缺失的受管资源，保留已修改内容并报告。`--repair-overwrite` 明确选择恢复修改过的受管资源，执行前先备份；未知用户文件不属于覆盖范围。修复包必须与原安装的 manifest 和 backend 内容摘要一致，不能仅凭同一版本号接管；不同内容重建的同版本包及缺失安装归属记录会被拒绝。

修复保持应用版本及安装位置，不能把修复参数用于降级或绕过升级规则。存在未完成事务时先恢复，再发起新的修复任务。详细资源处理规则见[恢复与修复](maintenance.md)。

修复报告包含 `repair.fully_repaired`、`restored`、`preserved` 和 `backups`。维护状态 `RepairComplete` 与 `RepairPreservedConflicts` 分别表示完整修复和仍保留修改项；任务正常结束不自动表示所有冲突都已修复。保留备份及其记录，确认需要后再由用户管理。

## 组件选择与同版本更改

以下参数属于构建后的 **Setup runtime**，不属于 `avelin-cli build`：

```powershell
& '<Setup.exe>' --silent --install-dir '<新安装目录>' --components core,component-a --report '<新报告路径>'
& '<原始Setup.exe>' --silent --operation modify --install-dir '<原安装目录>' --components core,component-b --report '<新报告路径>'
& '<新版Setup.exe>' --silent --operation upgrade --install-dir '<原安装目录>' --components core,component-a --report '<新报告路径>'
```

| 参数/场景 | 契约 |
|---|---|
| `--components <逗号分隔的ID>` | 完整目标集合，必须含 `core`；拒绝空、重复、未知 ID |
| 首次安装省略组件参数 | 使用包内默认选择 |
| 升级省略组件参数 | 继承原选择；新出现的可选组件不自动选中 |
| `--operation modify` | 使用同版本同构建的原始 Setup 增删组件；静默模式必须提供 `--components` |
| repair / recover / uninstall | 拒绝 `--components`，按原安装记录或冻结计划执行 |

GUI 使用清单选择，同版本原包维护页提供“修复安装 / 更改组件”。旧 schema 1 文件归属迁移若需要确认，GUI 展示映射并额外确认；静默模式显式传入完整组件集合表达迁移意图，不另设迁移开关。文件保护与原包核验仍然生效。配置、路径规则和空间说明见[可选组件](optional-components.md)。

## 使用已安装离线载荷维护

`--maintenance` 是 runtime 参数，使用 `--install-dir` 指定的已安装 manifest、receipt、runtime 和完整 worker 载荷，验证后从私有副本维护；不与任意 `--backend` / `--manifest` 组合。GUI 默认发现待恢复任务或进入同版本修复，组件包可切换更改页。静默必须明确选择维护操作，例如：

```powershell
& '<工具包>/bin/avelin-app.exe' --maintenance --silent --operation repair --install-dir '<原安装目录>' --report '<新报告路径>'
& '<工具包>/bin/avelin-app.exe' --maintenance --silent --operation modify --install-dir '<原安装目录>' --components core,component-a --report '<新报告路径>'
```

外部 runtime 等待静默结果并返回实际退出码；安装目录内的驻留 runtime 不接受 `--maintenance --silent`，请使用外部配套 runtime 或原始 Setup，驻留 GUI 入口不受影响。普通操作占用预检阻塞时，静默退出 `3`，报告 `result: "NotStarted"`、`worker_started: false` 和诊断码 `AVELIN_RESOURCES_IN_USE`；`preflight` 包含进程/PID、被占用文件和未能完成的检查。释放占用后用新报告路径重试。GUI 提供“重新检查”，不会强制退出应用。`recover` 仍使用原证据规则。完整命令限制和降级路径见[Windows 维护入口](windows-maintenance.md)。
