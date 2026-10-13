# 架构与代码导览

当前采用 GPUI Kit + Rust 协调层，构包时可选择默认 NSIS 后端或原生 Rust 引擎，两者都生成离线单 EXE。GUI 和静默入口、应用配置及持久模型共用。

## 执行路径

```mermaid
flowchart LR
  Config[TOML 与发布目录] --> CLI[avelin-cli]
  CLI --> Package[Setup: runtime + backend + manifest]
  Package --> Coordinator[Rust 协调层]
  UI[GPUI 界面] <-->|命令与快照| Coordinator
  Coordinator <-->|命名管道| Bridge[NSIS + x86 bridge]
  Coordinator <-->|命名管道| Native[Rust x64 worker]
  Bridge --> Resources[文件、快捷方式与卸载登记]
  Bridge --> Receipt[安装记录与任务日志]
  Native --> Resources
  Native --> Receipt
```

每个包只携带所选后端。图形和静默入口使用同一协调逻辑。UI 不直接部署文件或根据动画判定成功。协调层核对后端结果、进程状态和受管资源，确认实际结果后发布终态。

## 阅读顺序

1. `apps/avelin-cli/src/main.rs`：工具命令；`init.rs` 生成配置，`components.rs` 解析组件，`builder.rs` 生成 NSIS 脚本，`native_builder.rs` 打包 Rust worker 与载荷。
2. `crates/avelin-core/src/`：配置、载荷、包格式、安装记录、计划和协议。
3. `apps/avelin-app/src/coordinator.rs`：会话、任务锁、后端进程、GUI/静默流程和结果核验。
4. `crates/avelin-engine/src/`：Rust worker、部署事务、文件归属、系统入口和恢复日志。
5. `crates/avelin-nsis-bridge/src/` 与 `packaging/nsis/`：NSIS 后端通信、受管操作和脚本模板。
6. `crates/avelin-ui/src/`：界面和输入行为。

协调器负责身份、计划、锁、后端生命周期和结果核验；Rust 引擎负责自己的部署与补偿，NSIS 保留原受控执行路径。后端选择不会绕过既有路径和资源归属检查。

## 三种状态

| 数据 | 作用 |
|---|---|
| Manifest | 包里的身份、版本、载荷摘要和展示资源 |
| InstallPlan / UpgradePlan | 当前任务操作、选择及核验过的升级差异 |
| Receipt | 已提交安装的身份、版本、资源归属与提交关联 |

任务日志描述执行中的变更，receipt 描述已提交状态，两者不可互代。UI 快照用于显示，不能充当磁盘成功证据。字段见[本地 schemas](schemas/README.md)。

## 包装和执行边界

NSIS 包将 x64 runtime、x86 NSIS backend 和 manifest 组装进单 EXE overlay。bridge 使用 x86 NSIS ABI，与 x64 UI 不要求相同位数。

Rust 包的外层仍为 runtime + backend + manifest；backend 是 x64 Rust worker 加原生载荷和相同 manifest 的第二层 overlay。原生载荷含版本、runtime 和依清单排序的文件字节，当前不压缩。读取时核对边界、长度、文件数量和摘要，拒绝尾随或残缺内容。两层都受既有 1 GiB 嵌入区域上限约束。

运行时核验并释放所选后端，通过现有带任务身份的受限命名管道交换结构化进度、取消确认和终态。Rust 路径不加载 bridge，也不启动 NSIS。驻留卸载前端仍先转移到临时目录，再执行归属卸载，以便清理自己的原目录。

升级先暂存和备份，执行变更后最后提交完整 receipt。多文件与注册表变更不是一个原子事务；有限进程内恢复无法确认时返回 Unknown 并保留证据。失联不会直接解释为已取消或成功。

Rust 引擎在会话内保存 `engine-journal.jsonl`（安装/卸载）或 `upgrade-journal.jsonl`（升级）；变更期间使用安装根内的 `.avelin/upgrade-state.json` 标记未完成事务。日志记录变更意图、实际结果、提交或恢复结论；恢复失败保留 `engine-recovery.json`、备份和标记。后续操作检测到未完成标记即拒绝继续覆盖。

标记和日志提供中断诊断及有限进程内回滚证据，不构成跨进程自动恢复。`diagnose` 只读观察；没有自动恢复或通过删除标记继续的命令。已提交资源与终态确认分开处理，不能因失去终态而盲目回滚已完成的提交。

路径、摘要、用户身份、实例互斥和资源归属均属于执行规则。卸载只删除符合归属与摘要要求的资源，保留未知内容及已修改资源。完整行为见[兼容约定](compatibility.md)。
