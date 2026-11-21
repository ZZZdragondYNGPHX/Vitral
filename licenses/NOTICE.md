# Vitral 随附资源

| 资源 | 作者、来源与许可 |
| --- | --- |
| 雾凇词库与小鹤双拼方案 | iDvel 与 rime-ice 贡献者，GPL-3.0，包内 `rime/LICENSE`。[rime-ice](https://github.com/iDvel/rime-ice) |
| 万象简体语法 | amzxyz 与 rime-wanxiang-data 贡献者，CC BY 4.0。随附 LTS 语法文件。[上游数据与许可](https://github.com/amzxyz/rime-wanxiang-data) |
| DirectMap 手写模型 | Dave Morrissey 与 Tomoe 贡献者，LGPL 2.1。Vitral 将上游的传统 `hw_model.pt` 导出为 INT8 ONNX，并生成配套类别表。[原始权重与训练源码](https://github.com/mcyph/cnn_chinese_hw/tree/b87194fb5d68534207b5ecd1ecaa2f03686fac2b) |
| IDS 字形数据 | CJKVI Database，基于 CHISE IDS。随包保留原始版权标记，数据依上游 GPLv2 条款使用。[CJKVI 数据说明](https://github.com/cjkvi/cjkvi-ids#licenses) |
| rbt3 双向模型 | 哈工大讯飞联合实验室 HFL 的 rbt3，geofqiu0 提供 ONNX 转换，Apache-2.0。[模型与词表](https://huggingface.co/geofqiu0/rbt3-ONNX) |
| SenseVoice-Small | Alibaba Group / FunAudioLLM，使用 sherpa-onnx 分发的 INT8 转换，适用随附的 FunASR Model Open Source License Agreement。[原始模型](https://huggingface.co/FunAudioLLM/SenseVoiceSmall) |
| OpenCC（Linux） | BYVoid 与 OpenCC 贡献者，Apache-2.0。[OpenCC](https://github.com/BYVoid/OpenCC) |
| ONNX Runtime | Microsoft Corporation，MIT。许可及第三方声明随包提供。[ONNX Runtime](https://github.com/microsoft/onnxruntime) |

资源清单中的 URL 和 SHA-256 指向构建时使用的文件。手写转换与量化过程见源码中的 `scripts/export_hw_onnx.py`。

- `ext-data-control-v1.xml`: Wayland protocol definitions, X11-style permissive license; copyright and license in `Wayland-data-control.txt`. Source: https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/staging/ext-data-control/ext-data-control-v1.xml
