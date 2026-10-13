# 第三方材料与许可边界

Avelin 自有源码和文档采用 [MIT](../LICENSE)。依赖、上游许可原文、图标和附带源码保持各自条款，根许可证不将它们重新授权。概要见 [NOTICE](../NOTICE.md)。

## 当前主要组件

| 对象 | 锁定材料及处理 |
|---|---|
| GPUI Kit 0.6.1 / gpui-pre 0.3.4 | 锁定 crate 声明 Apache-2.0；分发相关产物时保留许可和适用声明 |
| 默认图标 | Lucide ISC、Feather 派生图标 MIT；保留原图标声明 |
| option-ext 0.2.0 | MPL-2.0；现有 Windows 图记录为 build/dev，材料生成器保存完整未改源码与许可 |
| NSIS 3.12 | COPYING 包含 zlib、bzip2、LZMA CPL-1.0 和 LZMA 链接例外；保存匹配版本的官方源码归档 |
| Rust 标准库 | 保存实际构建工具链携带的 COPYRIGHT-library.html |
| 字体 | 使用 Microsoft YaHei UI、Georgia 和系统回退名称，不复制字体文件 |

具体依赖由 Cargo.lock 与每次实际生成的清单决定。Cargo metadata 可能包含 build/dev 和功能合并节点，清单不等于所有包都链接进最终 EXE。不能把上游大仓库其他组件的许可套到本项目锁定的 crate。

## 保留的仓库输入

[packaging/third-party](../packaging/third-party/README.md) 保存原始上游补文、版本绑定证明、supplements.json 和 NSIS 3.12 源码归档，总量约 2 MB。它们是许可采集器的输入，应随源码提交，不属于缓存。第三方版权声明及公开作者归属保持原样。

有些发布 crate 漏带独立 LICENSE：simd_helpers 和 seahash 已补入上游仅添加许可的后续提交原文及对照证明；mac 0.1.1 保留其实际 README/Cargo 声明和完整未改源码，没有编造作者授权全文。

## 生成与核验

在依赖已由 Cargo 取得、NSIS 已准备的源码工作区执行。Python 3.11+，仅使用标准库：

```powershell
python scripts/collect-third-party.py --project-root . --nsis-root 'C:/Program Files (x86)/NSIS' --output target/third-party-new
python scripts/collect-third-party.py --verify target/third-party-new
```

输出使用空或新目录，生成器不构建、不安装、不联网。它保存实际许可/NOTICE、声明、来源、明列组件的源码，以及逐文件 FILES.json。校验通过只表示材料与清单字节一致，不是法律认证。

x64 的依赖可达范围以 `avelin-app`、`avelin-cli` 和 `avelin-engine` 为入口，x86 另记录 NSIS bridge。Rust 工具包要求材料包含 worker 入口；不能直接复用未记录 worker 的旧清单。收集器目前保留两个后端的完整材料，所以即使只分发 Rust 工具包，采集阶段仍需要 NSIS 3.12。

当前已核验材料覆盖 534 个 Windows 图依赖和 976 条实际许可文本；更新锁文件后应重新采集，不能继承旧数字。仅在锁文件、未进入采集图的包另列。

## 源码公开与二进制分发

MIT 允许使用 Avelin 自有内容；第三方文件仍按原许可处理。Apache-2.0 包含保留许可、适用声明及修改说明等要求，MPL 对覆盖文件及其修改生效。[Apache 官方许可](https://www.apache.org/licenses/LICENSE-2.0)、[Mozilla MPL FAQ](https://www.mozilla.org/en-US/MPL/2.0/FAQ/)

二进制发布前保留以下明确事项：mac 0.1.1 上游缺独立许可全文；Microsoft 编译/静态运行时再分发资格未由采集器认证。它们不应被改写为“禁止公开 Avelin 源码”，也不能因项目选了 MIT 就视为自动解决。

生成材料可能包含本机构建路径及原始 Cargo 元数据。公开附件需另作内容检查；不要直接上传整个本机采集或验收目录。发行步骤见[维护与发布](releasing.md)。
