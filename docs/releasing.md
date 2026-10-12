# 维护与发布

## 源码仓库

首次公开可以先提供当前 NSIS 预览源码。仓库已配置 MIT、贡献说明、公开文档和源码检查；GitHub 创建、可见性、分支与推送由维护者自行处理。

提交前：

```powershell
python scripts/check-public-source.py
pwsh -NoProfile -File scripts/check-source.ps1
git status --short
git diff --cached --stat
```

第一条检查 Git 候选、个人路径、常见凭据特征和公开文档链接，不是完整秘密审计。暂存后再次执行，避免把已暂存但现在被忽略的内部文件提交。源码测试不执行安装器。

`.gitignore` 保留源码、样例、schema 和明确选择的公开文档，排除本机报告、原型、生成产物与签名材料。原文件继续留在本地，无需删除历史证据。新增公开文档时同时更新 `.gitignore` 和 `packaging/public-docs.json`。

## 二进制发行单独处理

已有本地开发包不能直接视为公开 Release 附件。完整构建证据可能含构建路径、Windows 用户 SID、机器信息和原始 Cargo 元数据；上传前检查实际附件内容。

1. 从明确的源码版本构建配套 runtime、CLI、bridge 和样例。
2. 用 `scripts/collect-third-party.py` 收集锁定依赖和工具许可材料，并核验输出。
3. 用 `scripts/package-devkit.ps1` 包装；公共文档从明确清单复制，并附项目 LICENSE/NOTICE。
4. 校验 HASHES 与 ZIP 摘要，在新测试目录验证安装、升级、启动和卸载。
5. 复核附件中的个人信息、第三方声明、发行身份和签名安排，再发布预览版本。

完整链入口为 `scripts/ci-windows.ps1`，见[开发指南](development.md)。自托管 workflow 将完整日志和证据留在本次 runner 工作目录；需要长期保存时自行在 runner 端归档，后续 checkout 可能清理工作目录。普通 PR 只运行 hosted 源码检查。

## 版本与结论

CLI 预览版本是 `0.1.0-preview.1`；其他 crate 内部版本不代表独立公开包版本，当前均 `publish = false`。schema 和被安装应用版本分开管理。不要按同名文件混搭组件，也不要把构建成功记为安装通过。

MIT 不替第三方文件重新授权。NSIS 源码、图标和二进制分发材料见[第三方说明](third-party.md)。保留未验证项与已知限制，按实际结果写发布说明。
