# Avelin 文档

当前仅使用 Rust 原生引擎。CLI 的 `build` 将配置、品牌资源及应用发布目录打包为单个 Setup；源码构建与真实进程验收见开发指南。

| 文档 | 内容 |
|---|---|
| [接入指南](getting-started.md) | 用发布目录、TOML 和品牌资源生成 Rust Setup |
| [CLI 参数](cli.md) | init/check/build、组件定位、诊断与覆盖保护 |
| [兼容约定](compatibility.md) | 支持范围、升级、卸载、退出码与恢复边界 |
| [恢复、修复与组件更改](maintenance.md) | 中断恢复、同版本维护、冲突保留与备份 |
| [Windows 维护入口与占用检查](windows-maintenance.md) | 从系统更改入口维护、离线载荷复用、关闭应用后重新检查 |
| [Windows 部署能力](windows-deployment.md) | 前置运行库检测、HTTPS 完整包更新、全用户安装与 UAC |
| [开发指南](development.md) | 从新克隆准备环境、检查、构建与 CI |
| [架构](architecture.md) | 代码职责、执行路径与状态模型 |
| [模型与协议](schemas/README.md) | 本地 JSON Schema 与样例 |
| [路线图](roadmap.md) | 当前能力和后续方向 |
| [可选组件](optional-components.md) | 当前源码：清单选装、打包配置、升级继承与同版本增删；已发布旧工具包不含此能力 |
| [第三方材料](third-party.md) | 依赖许可、来源及材料生成 |
| [维护与发布](releasing.md) | 源码提交检查与二进制交付边界 |

开发约定见[贡献指南](../CONTRIBUTING.md)，许可证见 [MIT](../LICENSE)。此索引面向外部使用者；内部评审、个人机器报告、历史原型和未发布构建记录留在本地，相对链接只指向公开源码文件。
