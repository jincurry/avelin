# Windows 维护入口与占用检查

本页描述当前源码新增能力。已发布的 `v0.1.0-preview.1` 工具包不包含这些更改；需要使用当前源码配套构建 CLI、runtime 和 worker。

## 从系统应用列表维护

新安装登记 `ModifyPath`，允许系统的“更改 / 修改”操作打开 Avelin 维护页。在 Windows 的“已安装的应用”中查找应用并使用可用的修改入口；也可从“控制面板 → 程序和功能”选择应用后使用“更改”。Windows 版本和系统页面对按钮的展示可能不同，Avelin 不承诺系统提供独立的“修复”按钮。

维护页默认进入“修复安装”；有可选组件时可切换“更改组件”。页面锁定原安装目录，用户确认后才开始执行。系统卸载入口仍打开卸载确认页。

系统入口缺失或不便使用时，可直接打开同一维护页：

```powershell
& '<原安装目录>/.avelin/avelin-app.exe' --maintenance --gui --install-dir '<原安装目录>'
# 同一入口的简写，路径指向实际安装的完整载荷
& '<原安装目录>/uninstall.exe' --maintenance
```

`uninstall.exe` 不带参数时仍为卸载，`--silent` 或 `/S` 仍为静默卸载；`--maintenance` 只打开维护页，不自动修复或移除组件。

### 不需要重新寻找原始 Setup

安装目录的 `uninstall.exe` 保留完整离线载荷。维护启动器先核对 manifest、receipt、当前用户、完整载荷摘要和 runtime 摘要，再把经过验证的 runtime、worker 与 manifest 复制到新的私有临时目录，由副本执行维护。它不在安装目录内执行部署，避免自己占用即将替换的文件。

这不替代归属验证。内部 runtime、manifest 或离线载荷缺失/变化时，入口会拒绝；此时使用相同构建的原始 Setup。receipt 缺失或损坏时，不能仅凭应用名称或路径接管目录。原始 Setup 仍建议保留，具体资源保护规则见[恢复与修复](maintenance.md)。

### 旧安装如何获得入口

先前安装不会仅因下载新版工具自动获得按钮。用当前工具重新构建**更高应用版本**的安装包并正常升级后，才登记新的维护入口。不能用不同构建的同版本包绕过原包核验。

旧的完整卸载登记仍受支持：原有修复、更改组件及卸载流程保留旧登记形状；升级成功后写入新的登记。回滚和中断恢复保留原快照，不把新字段强加到历史记录中。

## 应用运行或文件被占用时

安装/升级/更改组件/修复/卸载在启动实际任务前检查相关受管文件。检查包括正在运行的应用、可能阻止替换/删除的文件句柄，以及无法完成的系统查询。未安装的组件不被当作已安装资源；升级同时检查旧归属文件和新的目标文件。

若发现阻塞，窗口显示可取得的应用名、PID、程序路径及被占用文件。保存工作、手动关闭相关应用后，点击“重新检查”；选择和安装目录保留，重新检查通过才启动任务。Avelin 不自动结束应用，也不自动重启系统。无法查询、权限不足或 Windows 报告需要重启时同样阻止启动，不能把检查失败解释成文件可用。

应用列表来自 Windows Restart Manager 的整组资源查询；文件占用列表来自独立的访问探测。系统没有给出准确对应关系时，不声称某个 PID 一定锁定某个文件。

检查结果只代表该时刻。检查通过后新出现的占用仍由部署事务处理，失败时按既有回滚/恢复规则处理；已经执行的任务不会因为后来的占用反馈重新变成可直接重试的未开始任务。

### 静默执行

静默执行遇到占用不会弹出重试窗口、等待用户关闭应用或启动 worker：退出码为 `3`，报告为 `result: "NotStarted"`、`worker_started: false`，诊断码为 `AVELIN_RESOURCES_IN_USE`。`preflight` 字段记录进程、文件、无法完成的查询和系统重启原因。关闭应用后重新运行命令，并给 `--report` 使用新的输出路径，避免覆盖前一次证据。

使用新工具包的外部 runtime，可直接从安装目录的离线载荷发起静默维护：

```powershell
& '<工具包>/bin/avelin-app.exe' --maintenance --silent --operation repair `
  --install-dir '<原安装目录>' --report '<新的报告路径>'
& '<工具包>/bin/avelin-app.exe' --maintenance --silent --operation modify `
  --install-dir '<原安装目录>' --components core,component-a --report '<新的报告路径>'
```

静默入口须显式选择维护操作，并使用安装目录外的配套 runtime；它等待私有副本完成并返回实际结果码。直接使用驻留 `.avelin/avelin-app.exe --maintenance --silent` 会拒绝，避免其等待时占用自己或提前退出却被脚本当作维护成功。系统和驻留 runtime 的 GUI 入口正常转交副本。`--maintenance` 不接受任意 `--backend` 或 `--manifest`，使用原安装目录中的已验证资料。

## 中断恢复仍走原证据路径

`recover` 不按普通 receipt 重新构造占用清单，因为未提交事务可能处于资源不完整状态。恢复核对原计划、会话、进程身份、备份、变更记录和提交标记；执行时若资源受占用或外部变更影响，保留证据并拒绝无法确认的操作。不能删除锁或标记来绕过检查。

系统维护页能读取完整安装资料时会复用现有的待恢复任务发现逻辑。安装资料本身不完整时，改用原始 Setup 和记录的恢复会话；原始恢复命令见[CLI 说明](cli.md)。

## 开发与验证边界

系统入口命令、私有副本、摘要验证、新旧登记/回滚、真实文件占用与释放后重试应分别由源码和真实进程检查确认。headless UI 测试验证交互状态，不能代替 Windows 应用列表、原生窗口或讲述人的人工验收。测试范围见[开发指南](development.md)。

微软的 [卸载登记说明](https://learn.microsoft.com/en-us/windows/win32/msi/uninstall-registry-key)列出 `ModifyPath`；[ARPNOMODIFY 说明](https://learn.microsoft.com/en-us/windows/win32/msi/arpnomodify)说明 `NoModify` 与系统更改入口的关系。Avelin 使用当前用户的 Win32 登记，修复由自己的维护页提供，保留 `NoRepair = 1`。
