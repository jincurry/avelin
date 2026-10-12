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

离线 schema 检查：在 Python 环境安装 `jsonschema==4.23.0` 后，从项目根目录执行：

```powershell
python tests/harness/check-m1-schemas.py
python tests/harness/check-m2-schemas.py
python tests/harness/check-m2-schemas.py --run '<run-m2.ps1 的已完成证据目录>'
python tests/harness/check-m3-schemas.py
python tests/harness/check-m3-schemas.py --m2-run '<冻结 M2 证据目录>'
python tests/harness/check-m3-schemas.py --run '<已通过的 M3 证据目录>'
```

M1 检查保留示例兼容性；M2 检查默认/选择/归属/启动响应以及错误字段反例，`--run` 另检查该轮实际 manifest、receipt 和 plan 的结构。它要求实际 harness 已通过，不能取代进程、报告和磁盘资源核验，更不判定 GUI 视觉通过。执行入口为 [run-m2.ps1](../../tests/harness/run-m2.ps1) 与 [run-m2-gui.ps1](../../tests/harness/run-m2-gui.ps1)，语义规则由 Rust 与实际闭环共同验证。

M3 检查包括旧 schema 示例、升级上下文/差异/提交血缘正反例和新增占位符。`--m2-run` 读取冻结的实际 M2 记录，核对其结构、报告摘要与事件负载兼容性；`--run` 读取已通过的 [run-m3.ps1](../../tests/harness/run-m3.ps1) 产物，另核对旧 receipt 原始字节摘要及升级血缘。两者只读，不重演安装；SemVer、Windows 路径、磁盘资源和恢复仍依赖 Rust 及真实执行验证。

这些脚本需要 Python 的 jsonschema 包；执行时使用自己的隔离环境和实际生成的证据目录。源码仓库不包含历史本机依赖缓存或报告。当前运行行为见[兼容约定](../compatibility.md)，模块职责见[架构](../architecture.md)。
