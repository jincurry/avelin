# Windows 部署配置示例

这里的 `avelin.toml` 展示前置运行库、HTTPS 更新源和全用户安装，产品名称、组件和 URL 均为通用示例。**不要直接把占位 URL 或最低版本当作自己应用的要求。**

1. 在此目录建立 `payload/`，放入自己的 `Application.exe` 和发布文件。
2. 修改产品 ID、名称、版本和目录名，把每个发布文件分配到一个组件。
3. 只保留实际需要的 `[[prerequisites]]`；自包含程序可全部删除。
4. 没有自己的 HTTPS 更新服务器时删除 `[updates]`。
5. 若只安装到当前用户，将 `install.scope` 改为 `current_user` 或省略。

```powershell
avelin-cli check --config .\examples\windows-features\avelin.toml
avelin-cli build --config .\examples\windows-features\avelin.toml --output .\out\Example-Setup.exe
```

仓库不附带占位 EXE，未准备真实 payload 时检查应报缺失文件。全用户 Setup 会请求管理员权限，并固定部署到系统 Program Files 下的 `ExampleApplication`；静默运行应使用管理员终端。

完整行为、发布更新命令和限制见[Windows 部署指南](../../docs/windows-deployment.md)。
