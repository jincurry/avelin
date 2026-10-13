# 模型与协议

这些 JSON Schema 使用 draft 2020-12，描述当前实现结构；不是已发布的稳定 API。文件中的 `https://avelin.dev/schemas/m1/` 仅为 schema 标识命名空间，验证时加载本地文件，无须请求该站点。

| 文件 | 使用方式 |
|---|---|
| `model.schema.json` | 引用 `#/$defs/Manifest`、`#/$defs/InstallPlan`、`#/$defs/Receipt`、`#/$defs/ShortcutRecord`、`#/$defs/UpgradePlan`、`#/$defs/UpgradeDiff` |
| `protocol.schema.json` | 引用 `#/$defs/TaskCommand`、`#/$defs/TaskSnapshot`、`#/$defs/TaskResponse`、`#/$defs/BackendEvent` |
| `presentation.schema.json` | 可直接验证 `Manifest.presentation` |
| `examples/*.json` | 最小模型、命令、快照与事件样例 |
| `presentation.example.json` | 默认模板及部分语言覆盖示例 |

JSON Schema 只验证结构和可表达的字段约束。Windows 路径、PNG 完整解码、占位符、跨字段计数、manifest 摘要、真实资源归属和当前用户必须继续由 Rust 实现验证。模型 schema 的 50 000 是保留的解析上限，不是当前产品容量或本轮测试目标。

`BackendEvent` 是原有后端负载，`TaskSnapshot` 是供 UI 使用的共享快照，两者并非相同格式。传输层还校验 `protocol_version`、任务、顺序和对端身份。`replay-*.json` 用于 `avelin-cli replay` 的语义回放，不提供传输认证或安装成功的磁盘证据。

M2 区分 manifest 的快捷方式默认值与 plan 的实际选择；receipt 的 `shortcuts` 记录当前用户受控路径、类型、入口和摘要。结构有效仍不代表该资源属于当前任务，运行时必须复核路径、SID、目标和实际文件。`LaunchInstalled` 不接受外部路径；独立 `TaskResponse` 的 `launch_result` 不改变 `TaskSnapshot` 的安装终态。中英展示目录覆盖安装、卸载、升级与启动反馈。

M3 的 `operation: upgrade` 必须带 `upgrade`，安装或卸载不能携带升级上下文。`UpgradePlan` 绑定旧版本、manifest 摘要、原始 receipt **字节**摘要、旧 task/commit、稳定的 installation ID，以及 payload 新增、替换、移除、不变的完整差异表。`required_space_bytes` 至少覆盖两份新 payload 与一份旧 payload，执行层另计运行时、元数据和系统资源开销并核对磁盘空间。暂存和备份位于安装目录外、相互独立的任务私有目录；模型不创建目录。

升级只接受更高 SemVer 优先级，忽略 build metadata；同版本、降级、身份冲突、仅改变 Windows 等价路径拼写和文件/目录结构切换均拒绝。`Receipt.previous_commit_id` 为可选字段，旧记录仍可读取且不会凭空写入该字段；`installation_id` 保持必填并跨升级保留。Schema 不替代这些语义与真实文件核验。

Rust 维护操作新增 `operation: repair` 和 `operation: recover`，不携带 `upgrade` 上下文，也不放宽升级版本规则。修复由执行层核验相同 manifest、安装归属和原包载荷；恢复另绑定原任务会话及持久变更证据。操作名称属于计划词汇，单独通过模型验证不表示允许写入。

修复保留版本和 `installation_id`，但为本次任务分配新的 `task_id` / `commit_id`，并把 `previous_commit_id` 指向修复前提交；即使 payload 无需修改，确认修复仍留下独立的提交记录。恢复核验或回退已有事务，不伪造原任务当时的终态。

维护任务报告在原有任务结果外包含 `repair` 或 `recovery` 详情以及 `maintenance_status`。`RepairComplete` 表示核验后的完整修复，`RepairPreservedConflicts` 表示仍保留修改资源，`RecoveryChecked` 表示只完成检查，`RecoveryComplete` 表示已执行并核验恢复。自动化还须检查对应任务、详情、进程结果和真实资源，不能把 `Succeeded` 一概当作“全部修复完成”。

离线 schema 检查：在 Python 环境安装 `jsonschema==4.23.0` 后，从项目根目录执行：

```powershell
python tests/harness/check-m1-schemas.py
python tests/harness/check-m2-schemas.py
python tests/harness/check-m3-schemas.py
```

脚本名称沿用对应模型引入阶段。M1 检查展示示例兼容性；M2 检查默认、选择、归属、启动响应和错误字段反例；M3 检查升级上下文、差异、提交血缘及维护操作约束。它们只验证结构及样例，不执行安装。

脚本保留的 `--run` / `--m2-run` 是读取历史 M2/M3 格式证据的只读选项，不适用于新的 Rust harness 输出。当前真实流程使用 [Rust 生命周期验收](../../tests/harness/run-rust-engine.ps1)、[中断验收](../../tests/harness/run-rust-engine-interruption.ps1)和[维护验收](../../tests/harness/run-rust-maintenance.ps1)，核对进程、报告、登记和磁盘资源。

这些脚本需要 Python 的 jsonschema 包；源码仓库不包含历史本机依赖缓存或报告。当前运行行为见[兼容约定](../compatibility.md)，模块职责见[架构](../architecture.md)。
