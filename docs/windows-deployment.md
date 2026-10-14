# Windows 部署：运行库、在线更新与全用户安装

这些能力位于当前源码，使用同一构建的 CLI、runtime 与 Rust worker。已发布的 `v0.1.0-preview.1` 不包含这些选项。平台仍为 Windows x64。

三项均由发布者在 schema 2 配置中明确开启；省略时保持原行为：当前用户安装、不要求额外运行库、不联网检查更新。完整配置示例见 [windows-features](../examples/windows-features/README.md)。

## 1. 前置运行库检测

这里声明的是**被安装应用**需要的环境。Avelin 自己不因此依赖 .NET、VC++ 或 WebView2。

在 TOML 顶层添加实际需要的项目：

```toml
[[prerequisites]]
kind = "vc_redist_x64"
min_version = "14.38.33135.0"

[[prerequisites]]
kind = "dotnet_desktop_x64"
min_version = "8.0.0"

[[prerequisites]]
kind = "webview2_evergreen"
min_version = "120.0.0.0"
```

| 类型 | 检测规则 |
|---|---|
| `vc_redist_x64` | 微软登记的 x64 VC++ 14.x 运行库，四段数字版本至少满足最低版本 |
| `dotnet_desktop_x64` | x64 .NET Desktop Runtime；要求相同 `major.minor`、不低于指定补丁版本的稳定版本，例如 8.0.12 满足 8.0.0，9.0.0 不替代 8.0.0 |
| `webview2_evergreen` | 微软登记的 WebView2 Evergreen，四段数字版本至少满足最低版本 |

每种类型最多一项，最多三项。运行库缺失或检测失败时不开始安装事务；窗口显示具体要求和微软官方下载地址，补齐后重新检查。检测不自动下载、安装第三方运行库，也不执行任意安装脚本。静默模式输出前置阻塞报告，调用方应按报告诊断后重试。

自包含 .NET、应用本地部署 VC++、固定版本 WebView2 应按应用自己的发布方式打包，通常不声明对应的机器运行库。检测到注册信息不等于对应用业务功能的完整验证。

## 2. 在线更新

### 应用配置

```toml
[updates]
feed_url = "https://updates.example.com/application/stable.json"
channel = "stable"
```

`channel` 省略时为 `stable`，使用 1–32 位小写字母、数字或连字符。稳定通道不接受 prerelease 版本；发布预览版本可使用 `preview` 等独立通道和 feed。

更新源必须为 HTTPS。先发布完整 Setup，再发布与该最终文件匹配的 JSON 元数据。不同通道使用不同 feed 地址；安装包里固定产品 ID、当前版本和 feed 配置，用户输入的任意 URL 不能覆盖这份配置。

### 发布新版本

1. 提高 `app.version`，保持 `app.id` 与安装范围不变，构建新的完整 Setup。
2. 若使用外部签名，在生成 feed **之前**完成签名。
3. 从最终 Setup 生成 feed；命令读取真实包身份、长度和 SHA-256：

```powershell
avelin-cli update-feed --setup .\out\Application-2.0.0-Setup.exe `
  --setup-url 'https://downloads.example.com/Application-2.0.0-Setup.exe' `
  --channel stable --output .\out\stable.json
```

4. 将 Setup 上传到 `setup_url`，再将 `stable.json` 发布到配置中的 `feed_url`。不在 Avelin 中保存网站或云存储写入凭据。

`update-feed` 不上传文件，不改写已存在的输出。生成后再更改或签名 Setup 会导致下载校验失败，须重新生成 feed。字段结构见 [update feed schema](schemas/updates.schema.json)，[结构样例](schemas/examples/update-release.json)中的 URL、大小与全零摘要均为占位值，不能用于真实更新。

包读取器沿用现有 Authenticode 附加布局支持：PE 安全目录指向文件尾证书表，原包尾标记与证书表之间最多七个零字节对齐；其他布局会拒绝。它只定位和校验 Avelin 包，不验证签名证书、发布者或时间戳。签名制作与签名信任验收仍由发布者另行完成，本功能不提供签名。

### 检查与下载

CLI 使用一个已有 Setup 的内嵌产品配置检查：

```powershell
avelin-cli update-check --setup .\Application-1.0.0-Setup.exe
avelin-cli update-check --setup .\Application-1.0.0-Setup.exe `
  --download .\downloads\Application-2.0.0-Setup.exe
```

返回 `available`、`up_to_date` 或 `downloaded` JSON；发现相同/更低版本时为 `up_to_date`，不会降级。下载命令只保存已验证的完整包，不执行它。输出父目录须存在，输出文件必须为新路径。

已安装应用的修复/更改组件维护窗口提供“检查更新”入口。检测到更高版本后，用户选择下载并进入原有升级确认流程；升级仍要核验旧安装、组件选择、文件占用、可写资源与事务证据。关闭维护窗口会取消待处理更新，取消后不启动新包。更新下载成功不等于安装成功。

检查、下载校验或启动升级失败时，窗口显示对应阶段的提示并保留重试入口；详细错误保存在诊断日志。当前原生 GUI 已检查等待、连接失败、重试和关闭；成功下载与校验覆盖来自受控传输测试，实际发布者 HTTPS feed 到升级窗口的完整流程仍需结合真实更新站点验证。

### 完整性与失败行为

- 只接受同一产品、同一通道、`windows-x64` 和更高 SemVer 优先级；build metadata 不增加优先级。
- feed 最多 64 KiB，Setup 下载最多 2 GiB；已有每层嵌入区域 1 GiB 的打包限制仍然生效。
- 所有重定向逐跳校验 HTTPS，最多五次；拒绝 URL 用户名/密码、HTTP 降级、无效证书、非成功响应、截断和超限内容。
- 使用 Windows WinHTTP 与系统正常 TLS 证书验证；不导入根证书、不忽略证书错误、不自动发送 Windows 凭据。连接/读写有超时，单次传输总预算十分钟。
- 下载到同目录临时文件，核对长度、SHA-256、Windows x64 PE 结构及内嵌产品/版本，才原子发布新文件。已有文件不覆盖，失败不运行下载内容。
- 维护窗口自动交接的新包，其安装范围、feed URL 和通道必须与现有安装相同；更换更新源须手动审阅并运行新包。程序在新 Setup 退出前保持已验证文件与目录的保护句柄。

**信任来源是包内配置的 HTTPS feed；SHA-256 是内容完整性校验，不是发布者签名。** 管理好该域名和 feed 发布权限。当前没有差分更新、后台定时轮询、断点续传或无人确认的自动安装；完整离线 Setup 仍可独立使用。

## 3. 全用户安装与提权

全用户安装已完成本机管理员真实流程验收：安装与公共资源权限、离线修复、组件添加/移除、升级继承选择、中断证据、只读恢复检查、恢复和卸载共 11 项通过。原生维护窗口也已确认可切换组件、执行组件更改并关闭。证据来自一个管理员账户；跨账户登录、独立环境和签名不由此推定通过。

```toml
[install]
scope = "all_users"
directory_name = "ExampleApplication"
desktop_shortcut = false
start_menu_shortcut = true
```

`scope` 可为 `current_user` 或 `all_users`；省略为 `current_user`。安装范围在打包时确定，不在运行时把同一安装切换为另一范围。

| 范围 | 安装与系统资源 |
|---|---|
| `current_user` | 当前用户目录、HKCU 卸载登记、当前用户 Desktop / Start Menu，保持已有行为 |
| `all_users` | 系统解析的 64 位 Program Files 下 `directory_name` 直系目录、HKLM 卸载登记、公共 Desktop / Start Menu，需要管理员令牌 |

全用户安装固定到受支持的 Program Files 位置，不能把任意可写目录包装成管理员目标。图形安装通过 Windows UAC 请求提升；用户取消后不执行部署。静默部署需要从已有管理员终端运行，不隐式弹出提权对话框。

安装、升级、修复、组件更改、卸载与恢复都核对范围和机器级归属。全用户安装不以某个管理员的个人 SID 接管共享资源；普通用户通过受控入口请求维护所需权限。旧省略范围的 manifest、plan 和 receipt 继续解释为 `current_user`，不迁移为机器安装。

公共快捷方式在创建时使用受保护的文件权限，普通用户不能改写其内容或权限；系统共享目录独立授予的“删除子项”权限仍可能允许删除快捷方式，Avelin 不修改该系统策略，后续修复将缺失项重建并继续校验文件哈希。

同一安装不支持当前用户/全用户原地互转。需要改范围时，使用原卸载入口移除旧安装，再使用新范围包重新安装。应用自己的文档、账户配置与数据库仍应保存到适当的用户数据目录；Program Files 中的受管发布文件应保持只读。

服务、驱动、系统重启续装、企业部署策略和任意安装钩子仍不包含在此功能中。测试边界见[开发指南](development.md)，原有用户数据保护规则见[兼容约定](compatibility.md)。
