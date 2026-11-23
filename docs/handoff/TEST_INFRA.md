# Vitral 测试指南

先获取离线资源并构建需要验证的平台。用例从工作区查找 CLI、C ABI 动态库、Fcitx5 插件、Android APK / JNI 和 Windows DLL / EXE。Windows 用例可在 Linux 上检查交叉编译产物。桌面注册、实际提交文本和不同缩放比例下的候选定位需要在 Windows 会话内验证。

## 运行

在仓库根目录创建测试环境。Python 版本以 `tests/e2e/pyproject.toml` 为准。

```bash
python -m venv tests/e2e/.venv
tests/e2e/.venv/bin/python -m pip install -e tests/e2e
./scripts/test-all.sh -q
```

脚本先运行 Rust 工作区测试并校验资源，再在限内存的 systemd 用户服务中串行执行 pytest。只验证 Windows 可使用 `./scripts/test-all.sh -q -m requires_windows`。可通过 `VITRAL_TEST_PYTHON` 指定解释器。需要选择特定产物时，在测试服务环境中设置 `VITRAL_WINDOWS_TSF_DLL`、`VITRAL_WINDOWS_TAURI_EXE` 等变量。

## 用例分层

| 标记 | 检查内容 |
| --- | --- |
| `tier1` | 单项功能、字段和文件格式，例如 ABI 版本、方案切换、模型清单及 PE 导出。 |
| `tier2` | 输入边界和失败处理，包括损坏词库、空音频、明确字形锁定、不可用的网络服务。 |
| `tier3` | 功能组合，例如导入词库后参与候选排序、多个设备并发合并剪贴板。 |
| `tier4` | 完整流程，包括长句输入、语音到翻译、文件接收后清理、设备恢复及跨平台产物验证。 |

平台标记为 `requires_core`、`requires_linux`、`requires_android` 和 `requires_windows`。设计令牌使用 `tokens` 标记。套件的目标发现规则在 `tests/e2e/vitral_e2e/discovery.py`。

## 测试数据与服务

- `tests/fixtures/` 包含带期望结果的拼音、词库、笔迹和 16 kHz PCM 语音样本。清单保存文件摘要。
- `tests/e2e/harnesses/` 在本地提供 WebDAV、LocalSend 和 AI 接口。通过请求记录检查跨设备覆盖、独立消费、接收校验与删除。
- 编译产物和打包路径可由自动化检查。输入法切换、真实麦克风、屏幕缩放和设备刷新率在对应平台验证。[Windows 实机步骤](../windows.md)
