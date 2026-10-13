# 可选组件使用指南

当前源码支持**一个完整离线包、一个必选主程序、若干平面可选组件**。安装后可使用原始 Setup 增删组件。已发布的 `v0.1.0-preview.1` 工具包不包含此能力；使用本文配置前，请从当前源码配套构建 CLI、runtime 和 Rust worker。

组件是应用开发者定义的文件组。Avelin 提供配置、选择界面和按选择部署的能力，不自带业务组件。本文中的“可选组件 A / B”只是示例，实际产品应使用能说明用途的名称。

## 安装软件的人怎么用

首次运行 Setup 时，组件显示为紧凑清单：主程序始终选中，可选项按打包配置初始化。每项展示名称、说明和文件大小，勾选后确认并开始安装。

- 切换语言或主题会保留选择。
- 更改组件、安装目录或快捷方式后，需要重新确认选项。
- 执行后锁定本次选择，不能在进行中的任务里改选。
- 只有主程序的包不显示组件选择区，继续使用普通安装流程。

同版本原始 Setup 会打开维护界面；有可选组件时，可在“修复安装”和“更改组件”之间切换。“更改组件”按已安装内容初始化，标出将添加/将移除的项，没有变化时不能提交。

## 开发者怎样配置

先准备一个完整的应用发布目录：

```text
MyInstaller/
  avelin.toml
  payload/
    Application.exe
    runtime/required.dll
    component-a/data.txt
    component-b/data.txt
```

采用 TOML schema 2，为每个文件声明唯一组件归属：

```toml
schema_version = 2

[app]
id = "com.example.application"
name = "示例应用"
version = "1.0.0"
entry = "Application.exe"

[payload]
directory = "payload"

[install]
directory_name = "ExampleApplication"
desktop_shortcut = false
start_menu_shortcut = true

[[components]]
id = "core"
required = true
name = { "zh-CN" = "主程序", "en-US" = "Application" }
description = { "zh-CN" = "运行应用所需的文件", "en-US" = "Files required to run the application" }
paths = ["Application.exe", "runtime/"]

[[components]]
id = "component-a"
required = false
default_selected = true
name = { "zh-CN" = "可选组件 A", "en-US" = "Optional component A" }
description = { "zh-CN" = "此示例的第一组可选文件", "en-US" = "The first optional file group in this example" }
paths = ["component-a/"]

[[components]]
id = "component-b"
required = false
default_selected = false
name = { "zh-CN" = "可选组件 B", "en-US" = "Optional component B" }
description = { "zh-CN" = "此示例的第二组可选文件", "en-US" = "The second optional file group in this example" }
paths = ["component-b/"]
```

### 字段和约束

| 字段 | 用途 |
|---|---|
| `id` | 稳定组件身份，跨版本保持不变；`core` 为保留的主程序 ID |
| `required` | 只有 `core` 为 `true`，其余组件为 `false` |
| `default_selected` | 首次安装默认选择；可选项省略为 `false`，`core` 省略为 `true` |
| `name` / `description` | 中英文名称和说明，包含 `zh-CN` 与 `en-US` |
| `paths` | payload 内的精确文件路径，或以 `/` 结尾的目录路径 |

一份配置支持 1–32 个组件，必须包含 `core`。入口 EXE 必须属于 `core`；运行所需 DLL 和共享资源也应放在 `core`，由开发者确认依赖完整。

ID 长度为 1–64 个 ASCII 字符，以小写字母开头，只使用小写字母、数字、`_` 和 `-`。每个名称最多 80 个 UTF-16 单元，说明最多 320 个；两种语言都必须提供非空文本，不能包含控制字符。

规则不支持 glob、排除规则、绝对路径或父目录跳转。目录规则递归包含其文件。每个 payload 文件必须归属一个组件；重复覆盖、目录与子目录重叠、未归属文件、没有文件的组件或不存在的规则都会使 `check` 失败。不要用 `paths = ["./"]` 隐式收入全部内容。

组件 ID 不能随显示名称一起随意重命名。升级用 ID 继承选择，改名会被视作旧组件消失和新组件出现。

旧 schema 1 配置仍可构包，全部文件自动归为 `core`，不显示选择区。当前 `init` 新建 schema 2 配置，初始只有显式的 `core` 文件组；拆出可选组件时，需要把对应路径从 `core` 中移出。

## 检查与打包

使用同次源码构建产生的三个执行组件：

```powershell
& '<CLI>' check --config ./MyInstaller/avelin.toml
& '<CLI>' build --config ./MyInstaller/avelin.toml `
  --runtime '<avelin-app.exe>' --rust-worker '<avelin-engine.exe>' `
  --output ./MyInstaller/output/Example-Setup.exe
```

`check` 检查组件、路径归属和整个载荷。`build` 生成单个完整 Setup，包含全部组件，不会根据默认勾选缩小包，也没有按需下载。

仓库提供[通用组件示例](../examples/components/README.md)。其中组件 A / B 是本地文本文件，便于核对是否部署。

## 静默安装与更改

`--components` 表示**完整目标集合**，不是追加选项。必须包含 `core`；空值、重复、未知 ID 或缺少 `core` 都会拒绝。使用逗号分隔：

```powershell
# 首次安装主程序与组件 A。
& '<Setup.exe>' --silent --install-dir '<新的绝对安装目录>' `
  --components core,component-a --report '<新的安装报告路径>'

# 同版本原包更改为主程序与组件 B：移除 A，添加 B。
& '<原始Setup.exe>' --silent --operation modify --install-dir '<原安装目录>' `
  --components core,component-b --report '<新的更改报告路径>'

# 更高版本升级，并显式指定升级后的完整集合。
& '<新版Setup.exe>' --silent --operation upgrade --install-dir '<原安装目录>' `
  --components core,component-a --report '<新的升级报告路径>'
```

静默 `modify` 必须给出完整组件集合。它要求同一产品、同一用户、同一安装实例和同一构建的原始 Setup；仅版本号相同不足以证明包一致。修复、恢复、卸载不接受 `--components`。

GUI 可通过 `--operation modify --install-dir '<原安装目录>'` 直接进入更改页，也可双击同版本原包，在维护页选择“更改组件”。

## 安装、升级与维护的选择规则

| 操作 | 组件选择 |
|---|---|
| 首次安装省略 `--components` | 必选 `core` 加配置中默认选中的可选组件 |
| 升级省略 `--components` | 继承已安装集合与新包的交集并保留 `core`；新可选组件不会因默认选中而自动加入 |
| 同版本更改 | 按确认的完整目标集合增删；GUI 初始为已安装集合 |
| 修复 | 只修复 receipt 中已经选择的组件，不补装从未选择的组件 |
| 恢复 | 使用中断前冻结的操作和选择，不提供重新勾选入口 |
| 卸载 | 按已提交的受管资源卸载，保留用户新增文件及已修改文件 |

取消勾选不等于删除组件目录的全部内容。更改流程只移除此前登记且仍匹配原内容的受管文件；用户新建文件和修改过的内容保留并报告。未知文件占用了准备新增的目标路径时拒绝覆盖。

更改保持安装实例身份，成功后提交新的选择和文件归属。中断时按持久事务恢复，不通过修改 receipt 或直接删目录手工拼接结果。

更改组件不会顺带修复未变化组件中的缺失或修改文件；需要补回它们时，单独使用“修复安装”。维护运行时、安装记录及原快捷方式仍须通过完整性检查，不能靠更改组件绕过损坏的维护入口。

## 从旧安装记录迁移

旧 schema 1 记录没有组件归属，表示当时整个包已安装。迁移到带组件的新包时，不能仅套用默认选择，以免误删已使用内容。

运行时对照旧受管文件与新组件路径。需要人工确认的映射会在 GUI 展示并要求额外确认；静默模式用 `--components` 明确完整目标集合，该显式集合同时表达迁移意图。证据不足或无法安全处理的冲突仍会拒绝，确认不会绕过文件保护。

## 文件大小与空间

“所选组件文件”是应用选中内容的逻辑字节，不是整个安装目录最终占用。维护需要保留完整包/运行时，另有事务暂存和备份开销。界面将维护文件另列，运行前仍执行实际空间检查。

少选组件能减少部署的应用文件，但不会缩小离线 Setup 的下载大小，也不意味着维护文件和临时空间同时减少。

## 实现边界

组件不支持嵌套、依赖、互斥、独立版本、联网下载或自定义安装脚本；公共依赖放在 `core`。应用自行处理缺少可选功能文件时的行为，安装器不执行应用内部数据库迁移。

Manifest 声明包内全部文件与组件；InstallPlan 冻结本次完整选集和操作；Receipt 记录提交后的选集及实际受管文件。修复、升级、卸载和恢复沿用这些归属信息，未选择的文件不会被当作“丢失”修复。

命令与维护结果见 [CLI](cli.md)、[维护指南](maintenance.md)及[模型与协议](schemas/README.md)。
