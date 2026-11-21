# 系统输入上下文与选区编辑

## Fcitx5 与 Wayland

Fcitx5 的输入上下文负责预编辑、候选列表与上屏。Vitral 将有顺序和注释的候选交给当前上下文，这样使用客户端候选窗的应用也能收到相同的数据。Qt 状态胶囊单独显示输入状态与手写画布。在 Wayland 会话中，状态胶囊用 LayerShellQt 设置屏幕锚点，管理中心沿用普通应用窗口。经典候选窗的主题和字号属于 Fcitx5 的共享设置，在管理中心保存外观选项时应用。[Fcitx5 输入法开发文档](https://fcitx-im.org/wiki/Develop_an_simple_input_method)

## Android 窗口与原生模型

`InputMethodService` 将识别中的字串交给当前 `InputConnection`，确认候选后再上屏。停靠和悬浮共用同一输入窗口。悬浮时同时修改窗口的位置和尺寸，窗口外的编辑区可继续接收触摸。停靠高度和拖动边界从显示区域扣除系统导航栏，避免三键导航遮住底行。[Android 输入法开发文档](https://developer.android.com/develop/ui/views/touch-and-input/creating-input-method)

APK 打包三种 ABI 的 JNI 与 ONNX Runtime。Rust 链接器使用 16 KiB 最大页尺寸，检查时分别查看 ELF 的 `PT_LOAD` 对齐和 APK 内共享库的 ZIP 对齐。模拟器可验证编辑器连接、触摸和候选提交，120 Hz 显示的帧间隔需要在对应刷新率的设备上测量。[Android 原生库页尺寸指南](https://developer.android.com/guide/practices/page-sizes)

## Windows TSF

TSF 通过 COM 类工厂加载文本服务。按键与候选操作在当前上下文的编辑会话中更新 `ITfComposition`，确认后结束组合并移动选区。DirectComposition 候选窗依据文本范围的屏幕位置显示，点选时编辑焦点仍在宿主应用。TSF 与 Tauri 通过登录会话内的命名管道共用输入引擎。[Microsoft Text Services Framework](https://learn.microsoft.com/en-us/windows/win32/tsf/text-services-framework)

## 选区翻译与润色

用户触发工具后，输入法保存当前编辑器和选区原文，再调用配置的 AI 服务。Ollama 请求提供模型与消息并关闭流式输出，以完整回答生成可点击预览。响应抵达时重新读取编辑器与选区，用户接受预览时再次核对原文，避免焦点移动或选区变化后覆盖其他文本。[Ollama 聊天接口](https://docs.ollama.com/api/chat)
