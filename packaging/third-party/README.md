# 第三方许可采集输入

`inputs/supplements.json` 固定上游位置、版本绑定、SHA256 和实际文件。它是离线生成器的输入，不是自动选择许可证的规则。

- `inputs/upstream/` 保存 Cargo 发布包遗漏、但可在同一源码提交或其父仓库找到的原始 LICENSE/NOTICE。
- `simd_helpers`、`seahash` 的发布提交没有 LICENSE；保存后续仅新增 LICENSE 的完整上游比较结果，并验证除此之外没有源码差异。生成器保留原始比较记录。
- `mac 0.1.1` 的发布包及已查阅上游均没有独立许可全文。生成器保存发布包 README/Cargo 中实际的 MIT/Apache-2.0 声明与完整未改源码，并在 `unknowns.json` 明示这个事实，不编造作者或许可文本。

修改依赖或这些输入时，重新核对上游版本关系、许可原文及摘要，再运行 [采集器](../../scripts/collect-third-party.py)。不能把新的许可证文本直接绑定到没有证据的旧版本。输入中的链接是来源记录，生成器不联网。

执行、输出范围和现有限制见[第三方材料](../../docs/third-party.md)。Avelin 自有内容采用根目录 MIT；这里的上游文件保留各自版权和许可，不能统一替换为 MIT。
