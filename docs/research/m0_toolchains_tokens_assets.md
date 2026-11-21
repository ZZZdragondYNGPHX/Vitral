# 工具链、设计令牌与离线资源

Vitral 的输入核心使用 Rust。Linux、Windows 和 Android 的打包脚本均从仓库根目录运行。
宿主职责见[架构与模块](../architecture.md)，交互要求见 [PROPOSAL.md](../PROPOSAL.md)。

## Linux

- Rust workspace 构建 CLI、C ABI 共享库和公共核心。
- 桌面管理中心使用独立的 Tauri Cargo manifest。
- Fcitx5 插件通过 CMake 构建，使用 Fcitx5 Core、Qt 6 和 libsystemd。
- 状态面板使用 Qt Widgets、Qt DBus、LayerShellQt、KWayland 和 KF6WindowSystem。
- 管理中心需要 GTK 3、WebKitGTK 4.1 等 Tauri Linux 依赖。
- 原生拼音引擎使用 librime，发行包内的字典和语法文件放在 `usr/share/vitral/assets/`。
- `scripts/package-linux.sh` 组合插件、核心库、主题、管理中心和资源，输出 x86_64 tar.zst。

## Android

- 使用 JDK 17、Android SDK 35、NDK 27.3.13750724 和 cargo-ndk。
- minSdk 为 29，JNI 构建目标为 arm64-v8a、armeabi-v7a 和 x86_64。
- 项目的 Gradle wrapper 确定构建工具版本。
- `scripts/build-android.sh` 在限内存的 systemd 用户服务中先构建 JNI，再打包 APK。
- JNI 与 ONNX Runtime 原生库随 APK 提供，模型直接从安装包资源读取或提取供核心使用。
- 构建脚本用 apksigner 检查签名，再用 zipalign 检查 16 KiB 对齐。
- 指定发行签名时通过 `VITRAL_ANDROID_KEYSTORE`、`VITRAL_ANDROID_KEY_ALIAS` 和口令环境变量传入。

## Windows

- Linux 主机需要 cargo-xwin、clang-cl、LLVM 链接工具与 x86_64-pc-windows-msvc Rust 目标。
- xwin 使用 Windows SDK 和 MSVC CRT，缓存位置可由 `XWIN_CACHE_DIR` 指定。
- `scripts/package-windows.sh` 构建 TSF DLL 和桌面 EXE。
- Windows SDK 提供 TSF、DirectComposition、Direct2D 和 DirectWrite 接口。
- ZIP 自带 x64 ONNX Runtime。设备上的其他运行库及注册命令见 [Windows 指南](../windows.md)。

## 离线资源

`scripts/assets-manifest.json` 保存上游地址、版本或提交、摘要、许可及目标文件。
`scripts/fetch-assets.sh` 下载并准备资源；`--verify` 离线检查缓存和展开后的文件。
默认下载缓存位于 `~/.cache/vitral/assets`，可通过 `VITRAL_ASSET_CACHE` 指向其他磁盘。

| 资源目录 | 用途 |
| --- | --- |
| `assets/rime/` | 雾凇词库、小鹤双拼、全拼方案和万象语法。 |
| `assets/opencc/` | OpenCC 配置和繁简转换字典。 |
| `assets/handwriting/` | 笔迹分类器、输出类别表和 IDS 部件索引。 |
| `assets/reranker/` | 带词表的 INT8 双向语义模型。 |
| `assets/voice/sensevoice-small/` | 离线语音模型和 CTC 词表。 |

## 设计令牌与宿主接口

在 [design-tokens.json](../../tokens/design-tokens.json) 中编辑深浅颜色、圆角、字号、手写时序和键盘布局。
运行 `python scripts/gen-tokens.py` 生成 Rust、C++、Kotlin 和桌面前端文件。
运行 `python scripts/gen-tokens.py --check` 检查生成结果与 JSON 是否一致。

Linux 的 C ABI 声明位于 `crates/vitral-ffi/include/vitral_core.h`。
Android 的 JNI 封装位于 `crates/vitral-jni`，Kotlin 调用端位于 `dev.vitral.ime.core`。
Windows 管道服务将 TSF 编辑器和管理中心的命令路由到同一会话的核心实例。
上层可以在不依赖系统输入框的情况下，通过 CLI 会话接口调用核心。
