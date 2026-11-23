# Vitral

Vitral 是一款面向 Linux、Android 和 Windows 的中文输入法。默认使用小鹤双拼，也可以切换到全拼。拼音、手写和语音识别都在本机运行。

三个平台的安装包和 SHA-256 校验文件可从 [GitHub Releases](https://github.com/ZZZdragondYNGPHX/Vitral/releases) 下载。

## 开始输入

| 平台 | 安装与启用 |
| --- | --- |
| Linux x86_64 | 安装发行版所需的 Fcitx5、Qt 6 和桌面依赖，将发布包中的 `usr/` 文件安装到对应系统目录。在 Fcitx5 配置工具中添加 Vitral，在应用菜单中打开 Vitral 管理中心。 |
| Android 10 及以上 | 安装 APK，在系统键盘设置中启用 Vitral，然后在文本框中切换输入法。 |
| Windows 10/11 x64 | 安装运行库、解压 ZIP，用管理员 PowerShell 注册文本服务，然后在简体中文键盘列表中添加 Vitral。具体命令见 [Windows 安装与验证](docs/windows.md)。 |

用小鹤双拼输入 `nihc`，或在设置中切换到全拼并输入 `nihao`，选择“你好”上屏。遇到会写但不会拼的字，打开手写查音可以同时看到带声调的拼音和小鹤编码。连续手写在确认之前保留整句，点选字位可以修正识别结果。

Linux 下，Ctrl+Space 用于启用或停用 Vitral，默认单按 Shift 切换 Vitral 内的中英文，可在“快捷键”页修改。请在 Fcitx5 的全局配置中移除与 Shift 冲突的备用触发键。管理中心“输入与外观 → 输入法内英文模式”可以关闭内部英文，改用系统键盘输入英文。

回车提交原始输入串（例如 `ai`）；桌面方向键选择后按空格或鼠标点击确认候选，移动端点击确认。在“快捷键”页可为常用功能配置按键和输入码，功能输入码与剪贴板收藏简码互不覆盖。

## 词库与设备同步

- 词库支持搜狗 `.scel`、Rime `.dict.yaml`、TSV 和纯文本。选词和手写确认共用个人词频。
- 快捷粘贴在候选栏首栏首位显示最近复制的内容，文字候选顺延；每台设备可各粘贴一次，输入法失焦后清除。剪贴板面板保存历史，支持置顶和缩写，例如给常用文本绑定 `ksb`。
- 配置 WebDAV 后可同步个人词库、设置和剪贴板。需要加密保密时，在各设备上填写相同的自定义同步密钥；留空使用内置通用密钥。文件优先走局域网，跨网中转文件在接收方校验后删除。纯离线模式关闭网络操作。
- 本机可进行语音转写。翻译、润色和摘要使用用户配置的 Ollama 或兼容接口。

## 从源码构建

在 Linux 主机上准备对应平台的工具链，再获取离线资源，按需要运行打包脚本。发布包输出到 `target/release/`。

```bash
source scripts/env.sh
./scripts/fetch-assets.sh
./scripts/package-linux.sh
./scripts/build-android.sh
./scripts/package-windows.sh
```

依赖和目标架构见[工具链与资源](docs/research/m0_toolchains_tokens_assets.md)。词库和模型的来源、校验值以及各自的许可见[资源清单](scripts/assets-manifest.json)。

## 许可

Vitral 源码可按 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 使用。发行包内的词库、模型和运行库分别遵循[第三方许可说明](licenses/NOTICE.md)。

## 开发与验证

[产品规格](docs/PROPOSAL.md) · [架构与模块](docs/handoff/PROJECT.md) · [测试指南](docs/handoff/TEST_INFRA.md) · [设计令牌](tokens/design-tokens.json)
