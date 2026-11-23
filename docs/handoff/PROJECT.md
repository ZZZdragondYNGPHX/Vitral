# Vitral 架构与模块

`vitral-core` 负责候选、笔迹、录音、词频和设备数据。系统输入法负责接收按键或笔迹、维护宿主预编辑区、显示候选，以及将确认后的文本提交给当前编辑器。设置和输入法使用同一份配置。

## 平台入口

| 平台 | 输入服务 | 设置与核心连接 |
| --- | --- | --- |
| Linux | `linux/fcitx5-vitral` 中的 Fcitx5 插件经 C ABI 驱动核心，Qt 面板显示状态和手写工具。 | Tauri 管理中心通过会话 D-Bus 连接输入服务。 |
| Android | `apps/android` 中的 `InputMethodService` 经 JNI 传递按键、笔迹和 PCM，并通过 `InputConnection` 更新文本。 | Kotlin / Compose 设置与键盘共用核心接口。 |
| Windows | `crates/vitral-tsf` 提供 TSF COM 类工厂、文本服务和编辑会话。DirectComposition 面板提供不抢占焦点的候选窗。 | TSF 和 Tauri 通过当前用户、当前登录会话的命名管道访问 `vitral-desktop.exe --engine`。 |

## 功能索引

| 编号 | 职责 | 主要位置 |
| --- | --- | --- |
| F01 | 工作区、工具链和平台构建 | `Cargo.toml`、`scripts/` |
| F02 | 设计令牌及各平台生成文件 | `tokens/`、`scripts/gen-tokens.py` |
| F03 | 下载、缓存、校验离线字典和模型 | `scripts/assets-manifest.json`、`scripts/fetch-assets.sh` |
| F04 | C 与 JNI 调用约定、对象生命周期 | `crates/vitral-ffi`、`crates/vitral-jni` |
| F05 | 小鹤双拼、全拼、候选及繁简转换 | `vitral-core/src/eng` |
| F06 | 全拼音节图、整句搜索与语法评分 | `eng/quanpin.rs`、`eng/gram.rs` |
| F07 | 词库格式解析、自动注音与共享索引 | `vitral-core/src/imp` |
| F08 | 单字手写、拼音和小鹤码反查 | `vitral-core/src/hw` |
| F09 | 笔迹切分、Top-8 混淆集、重排和手动锁字 | `hw/lattice.rs`、`hw/rerank.rs` |
| F10 | 个人词频、置顶、简码和按应用记忆 | `vitral-core/src/ret.rs`、`eng/` |
| F11 | 快捷粘贴、持久化历史、设备同步与文件传输 | `clip.rs`、`sync.rs`、`dav.rs`、`lan.rs` |
| F12 | 本地模型中心、RAM 分档、语音识别（SenseVoice / Qwen3-ASR）、本地 Qwen3、预测输入、用户画像蒸馏与纯离线开关 | `vitral-core/src/models.rs`、`ai/`、`ai.rs`、`predict.rs`、`profile.rs`、`scripts/models-catalog.json` |
| F13 | Fcitx5 输入上下文、候选窗和状态面板 | `linux/fcitx5-vitral` |
| F14 | 桌面管理中心、词库和外观设置 | `apps/desktop-tauri` |
| F15 | Android 键盘布局、手势、停靠和悬浮 | `apps/android/app/src/main` |
| F16 | Android 手写画布、设置、麦克风与 APK | `apps/android`、`scripts/build-android.sh` |
| F17 | Windows 注册、TSF 预编辑与候选窗 | `crates/vitral-tsf` |
| F18 | Windows 桌面管理中心和安装包 | `apps/desktop-tauri/src-tauri`、`scripts/package-windows.sh` |
| F19 | 选区翻译、润色、录音摘要与设备恢复 | `ai.rs`、`commands.rs`、各平台设置界面 |
| F20 | 工具链、解码、手写、语音和平台设计说明 | `docs/research/` |
| F21 | 跨模块与平台验收 | [测试指南](TEST_INFRA.md) |

表中 Rust 核心模块位于 `crates/vitral-core/src/`。C ABI 的字段和缓冲区释放函数见 [vitral_core.h](../../crates/vitral-ffi/include/vitral_core.h)。拼音与手写返回预编辑内容、候选和最终提交文本。宿主只向持有焦点的编辑器提交结果。应用切换、输入法关闭以及选择快捷粘贴分别触发独立的核心操作。

配置与云端文件的用途见[产品规格](../PROPOSAL.md)。注册 Windows 文本服务及卸载时使用的命令见[Windows 指南](../windows.md)。
