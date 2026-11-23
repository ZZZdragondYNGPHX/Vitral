# Vitral 仓库指南

品牌名使用英文 Vitral。产品行为见[产品规格](docs/PROPOSAL.md)，平台职责见[架构与模块](docs/handoff/PROJECT.md)。界面参考位于 [UI 原型](docs/handoff/ui_prototypes/)。视觉参数在 [design-tokens.json](tokens/design-tokens.json) 中维护，运行 `scripts/gen-tokens.py` 生成各平台的副本。

## 本机体验与开发

用户在 `~/Software/Vitral` 体验安装版。针对该目录反馈的问题，在 `~/Projects/Vitral` 修改源码并验证，再将发布产物部署回体验目录。

## 构建与测试

- 先检查磁盘空间。Cargo、编译器、CMake、pytest、Gradle 和 Tauri 构建通过 `scripts/locked-run.sh` 共用全局锁，串行执行。打包脚本内部持锁，可直接运行。
- 多平台验证按 Linux、Android、Windows 顺序执行。Android 构建与模拟器分开运行，使用独立且限内存的 systemd 服务。Gradle 使用 `--no-daemon`，pytest 使用 `-n 0`。
- 保留 `.cargo/config.toml` 的 3 个构建任务和单测试线程，dev/test profile 的行表调试信息及禁用增量编译的配置。Gradle 堆上限为 1280 MiB，其余进程与线程上限见 `scripts/env.sh`。
- 多音字索引、手写模型、IDS 和语法库在对应缓存锁内完成首次构造。ONNX 会话使用单个推理线程。
- 验证结束后清理临时构建目录和测试数据，保留依赖缓存及交付包。

```bash
./scripts/locked-run.sh cargo test --workspace --locked
./scripts/locked-run.sh cargo test -p vitral-core --no-default-features
./scripts/locked-run.sh cargo clippy --workspace -- -D warnings
./scripts/fetch-assets.sh --verify
./scripts/test-all.sh -q
```

## 跨端约定

当前文字复制同步至系统剪贴板，并通过 WebDAV 独立持久化；手机快捷候选在本机独立隐藏或持久清除，桌面候选栏不显示快捷粘贴。剪贴板历史独立保存。文件接收方校验 SHA-256 后删除中转副本。生命周期和界面规格见[产品规格](docs/PROPOSAL.md)。测试资源与平台用例见[测试指南](docs/handoff/TEST_INFRA.md)。
