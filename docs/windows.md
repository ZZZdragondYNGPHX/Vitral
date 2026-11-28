# Vitral for Windows

适用于 Windows 10、Windows 11 的 x64 桌面应用。输入服务和离线模型需要 Microsoft Visual C++ v14 x64 运行库，管理中心需要 Microsoft Edge WebView2 Runtime。离线词库、手写与语音模型随 ZIP 一起提供。解压时保留目录结构。

缺少运行库时可从微软安装。[Visual C++ x64 运行库](https://aka.ms/vs/17/release/vc_redist.x64.exe) · [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)

## 安装

1. 将压缩包内的文件解压到固定目录，例如 `C:\Program Files\Vitral`。
2. 在管理员 PowerShell 中注册文本服务。

   ```powershell
   & "$env:SystemRoot\System32\regsvr32.exe" "C:\Program Files\Vitral\vitral_tsf.dll"
   ```

3. 在 Windows 设置中打开“时间和语言”，进入中文（简体，中国）的语言选项，在键盘列表中添加 Vitral。用 `Win+Space` 切换输入法。
4. 运行安装目录中的 `vitral-desktop.exe` 打开设置。可在这里切换全拼与小鹤双拼、导入词库或配置 WebDAV。

注册会为安装目录和输入服务、桌面引擎文件添加应用容器所需的只读执行权限，使 Windows 搜索等系统应用可以加载 Vitral。用户词频、设置和历史目录不开放给应用容器；应用容器和低完整性进程的后台连接仅允许同用户、同会话的前台应用使用输入命令。

## 升级与任务栏图标

升级时先解压到新的固定目录，再按安装步骤注册新目录中的 `vitral_tsf.dll`。Windows 会把输入服务加载到使用它的应用进程中；完成注册后，完全退出并重新打开这些应用，才能使用新版服务。

任务栏应显示可点击切换中英文的模式按钮和 Vitral 品牌图标。若升级后仍显示“简体”或旧图标，可在任务管理器中重新启动“Windows 资源管理器”。它也可能持有旧版 DLL；重新选择输入法不一定会卸载旧版。保存工作后注销并重新登录，也可以统一刷新所有进程。确认旧进程退出后再清理旧安装目录。

## 输入与验证

在记事本中使用默认小鹤双拼输入 `nihc`，按空格提交“你好”。切换全拼后输入 `nihao`，用数字键选择候选。右击状态胶囊可打开手写查音或连续手写，点击候选上屏。连续手写时点击字槽可轮换备选字，右击后选择“确认手写”。

切换到另一个文本框、应用或输入法，检查预编辑和候选窗随焦点收起。复制一段文本后，状态胶囊中的“粘贴”可消费本机快捷粘贴。麦克风、候选窗位置、多屏缩放和系统输入法切换需要在 Windows 桌面验证。

设置保存在 `%APPDATA%\Vitral`，本机历史与词频保存在 `%LOCALAPPDATA%\Vitral`。每个登录会话的输入服务和管理中心共享一个后台引擎。

## 卸载

先切换到其他输入法，在语言选项中移除 Vitral。用管理员 PowerShell 运行以下命令，关闭使用过 Vitral 的应用并注销当前会话后即可删除安装目录。

```powershell
& "$env:SystemRoot\System32\regsvr32.exe" /u "C:\Program Files\Vitral\vitral_tsf.dll"
```
