# Contributing to Avelin

欢迎提交修复、文档改进和可复现的问题。当前目标为 Windows x64，部署可选择默认 NSIS 或 Rust 引擎。先阅读[开发指南](docs/development.md)和[架构](docs/architecture.md)。

## 本地检查

```powershell
python scripts/check-public-source.py
pwsh -NoProfile -File scripts/check-source.ps1
```

Python 检查提交内容和公开链接；PowerShell 检查格式与源码测试，不执行安装器。修改安装行为时，再选择相关验收脚本，在专用测试目录使用唯一产品 ID；不要对正式安装进行实验。

## 提交变更

- 聚焦一个具体问题，说明触发条件、前后行为和实际验证结果。
- Rust 格式使用 `cargo fmt`，避免在行为修复中混入大范围改名。
- 配置、协议或安装记录变化时，同步更新公开文档与 schema。
- 不提交构建产物、凭据、签名私钥、个人环境日志或用户数据。
- 保留第三方原声明；新增依赖说明用途，并检查许可和分发材料。
- 未执行的 GUI、环境或故障用例写“未验证”，不从单元测试推导通过。

报告问题时提供版本、最小配置、复现步骤和预期/实际结果。附加日志前移除用户名、SID、个人路径、应用数据和令牌等信息。

## License

贡献的 Avelin 原创内容按仓库 [MIT License](LICENSE) 提供。请只提交有权授权的内容；第三方引用保留来源和适用条款。
