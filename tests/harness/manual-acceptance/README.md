# M0 人工验收启动器

`start-m0-manual-acceptance.ps1` 可独立复制到验收扩展包的 `tools/`。支持 Windows PowerShell 5.1 x64 / PowerShell 7 x64，不需要开发仓库、Cargo、NSIS 或 Python。它读取原封的 R6 兼容性包，并打开一个持续到操作者正常关闭的原生窗口。

## 使用

以下为扩展包目录中的示例。将目录替换成实际位置；从任意工作目录执行时使用脚本和 R6 包的绝对路径。

```powershell
# 只验证完整 R6 checksums 并显示计划，不创建输出或启动窗口。
& 'D:\Avelin-Acceptance\tools\start-m0-manual-acceptance.ps1' `
  -KitDirectory 'D:\Avelin-R6' -ScenarioId 'UI-01-host-cn' `
  -ImageId 'development-host-01' -PlanOnly

# 打开中文演示窗口，人工执行键盘、目录选择、DPI 或输入法场景。
& 'D:\Avelin-Acceptance\tools\start-m0-manual-acceptance.ps1' `
  -KitDirectory 'D:\Avelin-R6' -ScenarioId 'IME-01-host-cn' `
  -ImageId 'development-host-01' -Mode Demo `
  -OutputDirectory 'D:\Avelin-Results\IME-01-run-001'

# 打开英文原生启动失败提示框；这是缺失输入的预检错误，不是真实 GPU 故障。
& 'D:\Avelin-Acceptance\tools\start-m0-manual-acceptance.ps1' `
  -KitDirectory 'D:\Avelin-R6' -ScenarioId 'ENTRY-01-preflight-en' `
  -ImageId 'development-host-01' -Mode LaunchError -English `
  -OutputDirectory 'D:\Avelin-Results\ENTRY-01-run-001'
```

`OutputDirectory` 必须不存在并位于 R6 包外。省略后会在当前目录生成含 UTC 时间、场景 ID 和随机后缀的新目录。不得往 sealed R6 包里添加脚本或输出；完整校验会拒绝未收录文件。校验只能检测文件变化，不证明未签名包的来源真实性。

观察上限默认 1,200 秒，可用 `-ObservationTimeoutSeconds` 设为 60–3,600 秒。人工完成场景后正常关闭应用或提示框，等待脚本记录进程退出和两条流的 EOF。超时仅保留现场和不确定状态，不杀进程、不关闭窗口、不自动重试；观察脚本宿主退出后管道可能关闭，后续日志不视为完整。

## 记录与判定

输出包含：

- `plan.json`、`kit-integrity.json`、原包 `kit.json` / `kit-checksums.json`：参数、完整校验、包身份。
- `tools/`：本次使用的 runtime 和脚本固定副本及记录中的 SHA-256。
- `launch.json`、`process.json`：PID、进程创建时间、实际退出码、捕获状态、EOF 和超时结果。
- `stdout.log`、`stderr.log`：异步保留原始字节，每条流最多 4 MiB；超出后继续排空，并明确记录截断。
- `temp/`、`local-app-data/`：仅子进程隔离的环境目录；运行时日志保留在其中。
- `case-record.json`、`evidence/`：人工结论模板与截图等证据目录。脚本不自动截图或采集输入。

`case-record.json.status` 初始始终为 `NOT_RECORDED`。执行人填写环境声明、实际步骤、预期与观察结果、证据相对路径和限制，再决定 `PASSED` / `FAILED` / `BLOCKED` / `NOT_APPLICABLE`。`ImageId` 是人工标签，不能独立证明干净 Windows。退出码、日志标记与 `PlanOnly` 均不能替代人工验收。

可另行使用原 R6 包内的只读 `tools/collect-m0-environment.ps1 -ImageId <标签> -Output <本次结果目录\environment.json>` 采集环境，然后在 `environment_declaration.environment_record` 填入 `environment.json`。环境文件也不等于干净镜像、实际渲染器或保护状态验收。

`Demo` 没有真实安装后端；`LaunchError` 使用确定缺失的 backend/manifest 路径，仅覆盖原生故障提示入口。真实安装/取消/卸载使用既有兼容性流程。完整人工步骤与证据要求见扩展包中的环境验收手册。
