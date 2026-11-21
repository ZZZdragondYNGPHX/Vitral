# 个人词频、设备恢复与离线语音

## 本机输入与恢复

拼音选词和手写确认共用 `RetentionEngine` 的 Frecency 词频衰减、置顶和擦除记录。Koofr 恢复先读取并解析配置、词频快照、CRDT 增量、导入词条和剪贴板历史，读取失败时返回错误。读取完成后，`VitralCore::restore_from_koofr` 在核心状态锁内应用数据。恢复剪贴板历史时清空本机 Quick Paste。Quick Paste 只存在于设备内存。

## 语音输入

平台经 `voice_start`、`voice_push_pcm`、`voice_stop` 提交 16 kHz 单声道有符号 16 位 PCM。停止录音时核心取走采样缓冲，释放输入状态锁，再执行推理。返回的 `duration_ms` 和 `sample_count` 覆盖整次录音。单次推理最多处理 30 秒，因此段边界可能截断词语。

`native-voice` 使用现有 ONNX Runtime 环境、一个推理线程和按需加载的 SenseVoice-Small int8 模型。模型与词表位于 `assets/voice/sensevoice-small/`。由 `kaldi-fbank` 提取 80 维 Hamming 窗声学特征，每隔 6 帧拼接 7 帧，按模型元数据归一化。从 CTC 峰值读取语言标签和发射位置，用 Wanxiang 语法对每个汉字位置的声学候选进行有限宽度搜索。对平均后验置信度低的片段，以两种语速重新提取特征，按同语言声学置信度选择结果。混读时外文专名仍可能丢字。模型缺失时，语音输入返回 `ModelUnavailable`。短于一帧或峰值低于 64 个 PCM 量化单位的片段按静音处理。[SenseVoice 参考实现](https://github.com/k2-fsa/sherpa-onnx/blob/master/sherpa-onnx/csrc/offline-recognizer-sense-voice-impl.h)

## 翻译、润色和摘要

`VitralCore::ai_inline` 向用户配置的 `ai_endpoint` 提交系统指令和原文。Ollama 使用 `/api/chat`，OpenAI 兼容接口使用 `/v1/chat/completions`。两者均接受完整聊天接口地址。配置项包括 `ai_model`、可选的 `ai_api_key` 和 `ai_timeout_ms`。默认模型为 `qwen2.5:7b`，超时为 30 秒。密钥随请求作为 Bearer 凭据发送，序列化配置时省略。

`OfflineGuard` 在发送请求前检查纯离线开关。本机转写可在纯离线模式下使用。翻译、润色和摘要使用独立的显式调用。C 和 JNI 的模式值分别为 `0`、`1` 和 `2`。
