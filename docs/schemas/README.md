# 模型与协议

这些 JSON Schema 使用 draft 2020-12，描述当前实现结构；不是已发布的稳定 API。文件中的 `https://avelin.dev/schemas/m1/` 仅为 schema 标识命名空间，验证时加载本地文件，无须请求该站点。

| 文件 | 使用方式 |
|---|---|
| `model.schema.json` | 引用 `#/$defs/Manifest`、`#/$defs/InstallPlan`、`#/$defs/Receipt`、`#/$defs/ShortcutRecord`、`#/$defs/UpgradePlan`、`#/$defs/UpgradeDiff` |
| `protocol.schema.json` | 引用 `#/$defs/TaskCommand`、`#/$defs/TaskSnapshot`、`#/$defs/TaskResponse`、`#/$defs/BackendEvent` |
| `presentation.schema.json` | 可直接验证 `Manifest.presentation` |
| `updates.schema.json` | HTTPS 完整包更新 feed；摘要不是发布者签名 |
| `examples/*.json` | 最小模型、命令、快照与事件样例 |
| `presentation.example.json` | 默认模板及部分语言覆盖示例 |

JSON Schema 只验证结构和可表达的字段约束。Windows 路径、PNG 完整解码、占位符、跨字段计数、manifest 摘要、真实资源归属和当前用户必须继续由 Rust 实现验证。模型 schema 的 50 000 是保留的解析上限，不是当前产品容量或本轮测试目标。

## Windows 部署字段

schema 2 增加可省略的 `Manifest.prerequisites`、`Manifest.updates` 和 Manifest / InstallPlan / Receipt 的 `install_scope`。默认空要求、无更新源、`current_user`，序列化时省略默认值，旧字节与摘要兼容；schema 1 不接受全用户范围、非空前置要求或更新配置。新字段不是旧运行时的兼容承诺。

`prerequisites` 最多三项，每个 `kind` 只能出现一次；VC++/WebView2 四段数字，.NET Desktop 三段数字且匹配相同 major.minor。重复类型、真实注册信息和详细版本语义仍由 Rust 验证。`updates.feed_url` 绑定 HTTPS 元数据源，`channel` 默认为 `stable`。更新 feed 的版本优先级、平台、产品、跳转链、文件大小及真实 Setup 内容还须执行校验。

`examples/windows-manifest.json` 展示这些新字段；`examples/update-release.json` 只作结构演示，地址和摘要不能用于真实下载。全用户范围的 Program Files、管理员令牌、HKLM 与公共快捷方式归属由执行层核验，单独填写 `all_users` 不授予机器级写权限。使用和发布规则见[Windows 部署指南](../windows-deployment.md)。

维护窗口新增无外部路径/URL参数的 `check_for_updates`、`download_update` 意图。`update_status` 响应包含可空/省略的 `available_version`、`detail` 和 `launched`，表示网络或新 Setup 启动反馈，不改写当前安装事务终态。只在允许的维护状态处理这些意图；收到意图并不授权跳过校验或后台静默安装。

## 可选组件与 schema 版本

配置、manifest、plan、receipt 分别使用独立的 schema 2 常量；旧 schema 1 仍可读取。全局旧版常量没有直接改号。`examples/manifest.json`、`receipt.json`、`install-plan.json` 保留原样，用于核对旧摘要和省略字段兼容；`examples/components-*.json` 展示当前组件模型和 `modify`。

`components-receipt.json` 使用无末尾换行的紧凑 JSON，`components-modify-plan.json` 的前序 receipt 摘要绑定这份文件的原始字节，避免 Git 换行转换改变示例的绑定。不要只格式化 receipt 而不同时更新摘要。

| 数据 | schema 2 新约束 |
|---|---|
| Manifest | 保存完整包的 `components` 与所有文件；每个文件必须有 `component_id`；恰有一个必选 `core`，入口属于它 |
| InstallPlan | `selected_components` 是排序且含 `core` 的完整最终选集；`selection_source` 记录来源；`payload_file_count` / `payload_bytes` 统计选集，`package_payload_bytes` 统计完整包 |
| Receipt | `selected_components` 与 `files` 严格对应；文件保留发行原始 hash，即使用户之后修改或删除了文件 |
| UpgradePlan | 原始 receipt 字节摘要、前后选集、受管文件差异；`modify` 另外绑定 `modify_snapshot_sha256` 和 `backend_sha256` |

`selection_source` 可为 `default`、`inherited`、`explicit`、`legacy_migration` 或 `receipt`。首次安装采用默认或显式选择；升级继承旧选择，不自动加入新组件；修复、卸载使用 receipt；旧版有歧义的迁移需要显式选择。

JSON Schema 分别限制两个版本：schema 1 不接受实际组件目录、文件归属或非空选集，schema 2 必须有组件目录、归属和冻结选集。它还检查组件 ID、双语文本、必选状态、选择不重复且包含 `core`。组件 ID 唯一、文件是否属于声明的组件、选集是否已排序、receipt 是否恰好等于选集、真实路径冲突、统计和摘要绑定继续由 Rust 纯模型及执行层验证，不能仅凭 Schema 通过就执行。

`operation: modify` 仅接受 schema 2，复用 `upgrade` 字段承载原安装上下文；它要求同一完整 manifest、相同版本与 backend、原状态快照及发生变化的选集，不能有文件替换差异。未变化组件保留其实际修改或缺失状态。`upgrade` 仍要求更高版本，不能携带 modify 快照；repair/recover/uninstall 不能借此改变选集。

UI `start` 命令新增可省略的 `selected_components`、`operation` 和 `migration_confirmed`；旧命令仍可读取。这里只是用户意图，coordinator 必须验证允许的操作、旧版迁移确认、安装身份与选集，然后冻结为 plan。原生完整载荷的二进制版本仍为 1，按完整 manifest 顺序校验，不能用选集重新解释载荷偏移。

## 原有模型与验收

`BackendEvent` 是原有后端负载，`TaskSnapshot` 是供 UI 使用的共享快照，两者并非相同格式。传输层还校验 `protocol_version`、任务、顺序和对端身份。`replay-*.json` 用于 `avelin-cli replay` 的语义回放，不提供传输认证或安装成功的磁盘证据。

M2 区分 manifest 的快捷方式默认值与 plan 的实际选择；receipt 的 `shortcuts` 记录当前用户受控路径、类型、入口和摘要。结构有效仍不代表该资源属于当前任务，运行时必须复核路径、SID、目标和实际文件。`LaunchInstalled` 不接受外部路径；独立 `TaskResponse` 的 `launch_result` 不改变 `TaskSnapshot` 的安装终态。中英展示目录覆盖安装、卸载、升级与启动反馈。

M3 的 `operation: upgrade` 必须带 `upgrade`，安装或卸载不能携带升级上下文。`UpgradePlan` 绑定旧版本、manifest 摘要、原始 receipt **字节**摘要、旧 task/commit、稳定的 installation ID，以及 payload 新增、替换、移除、不变的完整差异表。`required_space_bytes` 至少覆盖两份新 payload 与一份旧 payload，执行层另计运行时、元数据和系统资源开销并核对磁盘空间。暂存和备份位于安装目录外、相互独立的任务私有目录；模型不创建目录。

升级只接受更高 SemVer 优先级，忽略 build metadata；同版本、降级、身份冲突、仅改变 Windows 等价路径拼写和文件/目录结构切换均拒绝。`Receipt.previous_commit_id` 为可选字段，旧记录仍可读取且不会凭空写入该字段；`installation_id` 保持必填并跨升级保留。Schema 不替代这些语义与真实文件核验。

Rust 维护操作新增 `operation: repair` 和 `operation: recover`，不携带 `upgrade` 上下文，也不放宽升级版本规则。修复由执行层核验相同 manifest、安装归属和原包载荷；恢复另绑定原任务会话及持久变更证据。操作名称属于计划词汇，单独通过模型验证不表示允许写入。

修复保留版本和 `installation_id`，但为本次任务分配新的 `task_id` / `commit_id`，并把 `previous_commit_id` 指向修复前提交；即使 payload 无需修改，确认修复仍留下独立的提交记录。恢复核验或回退已有事务，不伪造原任务当时的终态。

维护任务报告在原有任务结果外包含 `repair` 或 `recovery` 详情以及 `maintenance_status`。`RepairComplete` 表示核验后的完整修复，`RepairPreservedConflicts` 表示仍保留修改资源，`RecoveryChecked` 表示只完成检查，`RecoveryComplete` 表示已执行并核验恢复。自动化还须检查对应任务、详情、进程结果和真实资源，不能把 `Succeeded` 一概当作“全部修复完成”。

## 启动前占用反馈与系统入口

`TaskResponse` 新增 `{"response":"preflight_blocked","detail":"..."}`。这是协调器拒绝一次尚未启动的请求，不是 `BackendEvent`、任务终态或回滚成功。UI 只在已请求 Start 且未观察到执行时恢复可确认状态，显示占用详情和“重新检查”；已执行/终态/失联状态不接受迟到的反馈。重新检查仍发完整 Start 意图，再由协调器验证，不能重放已经启动过的任务。

只读占用结果 `UsageReport` 包含 `processes`、`locked_files`、`issues`、`reboot_reasons`、`checked_files`。进程以 PID 和创建时间标识；应用列表与文件锁列表是独立观察，没有确切归属时不把它们组合成进程到文件的映射。查询问题不是“未占用”。静默阻塞报告使用 `result: NotStarted`、退出码 `3`、`worker_started: false`、诊断码 `AVELIN_RESOURCES_IN_USE` 及 `preflight`，没有新建安装任务的成功/失败结果。恢复操作依原始证据处理，不按普通 receipt 做占用预检。

新 Win32 登记在原七值基础上增加 `ModifyPath`，`NoModify=0`、`NoRepair=1`；命令指向已安装 runtime 的 `--maintenance --gui --install-dir` 入口。它校验安装身份和完整离线载荷后复制到私有目录执行，不信任注册表中的任意命令。新字段不是 manifest/receipt schema 的组成部分。

历史完整七值登记（无 `ModifyPath`、`NoModify=1`）仍合法；混合或篡改后的登记形状拒绝。内部登记模型的 `modify_path` 可省略，旧 `upgrade-resources.json` 和恢复快照仍按其原字节/原字段集合核验。升级写入当前形状，修复/更改保留原形状，回滚还原准确旧值，不通过填补字段把历史快照变成新记录。使用方式见[Windows 维护入口](../windows-maintenance.md)。

离线 schema 检查：在 Python 环境安装 `jsonschema==4.23.0` 后，从项目根目录执行：

```powershell
python tests/harness/check-m1-schemas.py
python tests/harness/check-m2-schemas.py
python tests/harness/check-m3-schemas.py
```

脚本名称沿用对应模型引入阶段。M1 检查展示示例兼容性；M2 检查默认、选择、归属、启动响应和错误字段反例；M3 检查升级上下文、差异、提交血缘及维护操作约束。它们只验证结构及样例，不执行安装。

脚本保留的 `--run` / `--m2-run` 是读取历史 M2/M3 格式证据的只读选项，不适用于新的 Rust harness 输出。当前真实流程使用 [Rust 生命周期验收](../../tests/harness/run-rust-engine.ps1)、[中断验收](../../tests/harness/run-rust-engine-interruption.ps1)和[维护验收](../../tests/harness/run-rust-maintenance.ps1)，核对进程、报告、登记和磁盘资源。

这些脚本需要 Python 的 jsonschema 包；源码仓库不包含历史本机依赖缓存或报告。当前运行行为见[兼容约定](../compatibility.md)，模块职责见[架构](../architecture.md)。
