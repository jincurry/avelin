# 通用可选组件示例

此示例演示“示例应用 / 主程序 / 可选组件 A / 可选组件 B”的紧凑清单。组件 A 默认选中，组件 B 默认不选中；两项分别包含一个文本文件，便于核对部署结果。它们是测试载荷，不是 Avelin 自带的业务功能。

需要当前源码构建的配套工具；已发布 `v0.1.0-preview.1` 不支持 schema 2 组件配置。

## 构包

在仓库根目录使用 PowerShell。先构建工具与仓库自带的演示入口，再将演示 EXE 复制为此产品的入口：

```powershell
cargo build --locked --release -p avelin-engine -p avelin-cli -p avelin-app --features avelin-app/gui --bins
Copy-Item -LiteralPath ./target/release/nimbus-demo.exe -Destination ./examples/components/payload/Application.exe

& './target/release/avelin-cli.exe' check --config ./examples/components/avelin.toml
& './target/release/avelin-cli.exe' build --config ./examples/components/avelin.toml `
  --runtime ./target/release/avelin-app.exe --rust-worker ./target/release/avelin-engine.exe `
  --output ./work/components-example/Example-Setup.exe
```

入口复用 `nimbus-demo` 作为执行夹具，安装器展示使用本示例的通用产品名及组件名。真实接入时用自己的 EXE 替换入口，并修改产品 ID、版本及路径归属。生成的 `payload/Application.exe` 已在本目录忽略，不要提交二进制。

每次构包使用新的输出路径；构建拒绝覆盖既有产物。构包本身不会安装应用。

## 看界面与验证

打开生成的 Setup 即可查看清单；主程序不可取消，A / B 可勾选。确认并安装会实际创建当前用户的安装资源。

静默示例也会执行真实安装，请使用新的空目录和新的报告路径：

```powershell
& './work/components-example/Example-Setup.exe' --silent `
  --install-dir "$env:LOCALAPPDATA\AvelinComponentsExample" `
  --components core,component-a --report ./work/components-example/install.json

& './work/components-example/Example-Setup.exe' --silent --operation modify `
  --install-dir "$env:LOCALAPPDATA\AvelinComponentsExample" `
  --components core,component-b --report ./work/components-example/modify.json
```

首次安装后应有 `component-a/data.txt`，没有 `component-b/data.txt`。更改后结果相反。`--components` 是完整集合，不能省略 `core`。

再次打开**同一个 Setup**，维护页可选择“修复安装”或“更改组件”。修复只处理当前已选组件；不会重新安装已取消的 A。需要清理示例时使用系统登记的卸载入口，不手工删除安装记录。

组件移除会保留修改过的受管文件和用户新增内容。因此，修改文本文件后再取消组件时，该修改文件被保留是预期行为，应同时查看执行报告。

配置与边界见[可选组件指南](../../docs/optional-components.md)。
