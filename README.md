# Codex Quota

一个轻量的 Windows 原生 Codex 额度悬浮窗。它通过本机已登录的 Codex CLI 读取账户额度，在桌面边缘持续显示剩余额度、重置时间和连接状态。

## 功能
<img width="459" height="429" alt="CodexQ-ball" width="200" src="https://github.com/user-attachments/assets/334533e0-cbc9-43cb-a95d-215cac1bf3cb" />
<img width="459" height="429" alt="codexQ-panel" width="200" src="https://github.com/user-attachments/assets/50cbdb82-90dc-44a7-8125-ff72ead31d16" />
<img width="458" height="429" alt="CodexQ-menu" width="200" src="https://github.com/user-attachments/assets/f7b2e359-5621-412f-b479-ae421fe41736" />



- 以悬浮球显示额度状态，单击可展开或收起详情面板
- 显示主、次额度窗口的剩余百分比、重置时间，以及今日、本期和累计 Token 用量
- 支持拖拽，并自动吸附到当前显示器的四条边缘
- 支持多显示器和 DPI 缩放
- 可配置始终置顶、开机启动、面板常驻及“跟随 Codex”
- 支持手动刷新，以及 1、2、5、10、30 分钟的自动刷新间隔
- Codex 额度发生变化时自动刷新，连接中断后自动重连
- 使用 Win32、Direct2D 和 DirectWrite 实现，无 WebView 依赖

## 运行要求

- Windows 10/11
- 已安装并登录 Codex CLI
- `codex.exe` 位于以下任一位置：
  - `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin\codex.exe`
  - `PATH` 环境变量包含的目录

可以先在 PowerShell 中确认 Codex CLI 可用：

```powershell
codex --version
```

## 从源码构建

准备以下开发环境：

- Rust stable（项目使用 Rust 2024 edition）
- Windows MSVC 工具链及 Visual Studio C++ Build Tools

克隆项目后执行：

```powershell
cargo build --release
```

生成的程序位于：

```text
target\release\codex-quota.exe
```

开发时可以直接运行：

```powershell
cargo run
```

## 使用方法

- 单击悬浮球：展开或收起额度面板
- 拖动悬浮球：移动位置；松开后自动吸附屏幕边缘
- 右键单击系统托盘图标：打开设置菜单

托盘菜单提供显示/隐藏、立即刷新、刷新间隔、始终置顶、开机启动、跟随 Codex、面板常驻和退出等操作。

“跟随 Codex”默认关闭。启用后，应用在后台监听 Codex Windows App 和 Codex CLI；检测到 Codex 运行时显示悬浮球，最后一个实例退出后自动收起并释放额度查询和渲染资源。跟随期间不能手动显示或隐藏悬浮球，开机启动设置不受影响。

## 配置与日志

应用数据保存在：

```text
%LOCALAPPDATA%\codex-quota\
```

其中：

- `config.json`：窗口位置和应用设置
- `usage-cache-v1.json`：本机 Codex 会话用量的增量解析缓存，不包含对话正文或凭据
- `codex-quota.log`：运行日志
- `codex-quota.log.old`：轮换后的上一份日志

配置文件损坏时，程序会将其重命名为 `config.invalid-<时间戳>.json`，随后使用默认配置启动。

`config.json` 中的 `follow_codex_check_interval_secs` 控制“跟随 Codex”的空闲检查间隔，默认 2 秒，可设置为 1–60 秒。修改后需重启应用生效。

程序不包含文本输入控件，因此会在创建首个窗口前禁用 UI 线程的 IME，避免原生托盘菜单首次弹出时加载第三方输入法组件。

## 开发检查

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test
```

## 隐私说明

程序不会要求单独填写 OpenAI 凭据。它在本机启动 `codex app-server --stdio`，并复用 Codex CLI 当前的登录状态读取账户额度、周期边界、重置时间及累计 Token 用量。

“今日使用”和“本期使用”均从 `%CODEX_HOME%\sessions`（未设置时为 `%USERPROFILE%\.codex\sessions`）及 `archived_sessions` 中只读解析官方 OpenAI 会话的 `token_count` 元数据；RPC 每日用量桶不会参与或兜底这两个数值。“本期使用”按 RPC 提供的精确额度开始时刻过滤本地事件。程序启动、监听异常及统计日期或周期变化时执行全量扫描，正常运行时监听会话目录并只增量处理变化的 JSONL。本地日志缺失或不可靠时，对应数值显示 `--`。这些本地数值仅代表本机留下日志的使用量，不包含其他设备、临时会话或已删除日志；同一天切换多个官方账号时，本地日志也可能无法区分账号。程序不会读取或缓存会话中的提示词、回复和工具输出。

## 许可证

本项目源代码采用 [MIT License](LICENSE) 发布，完整许可文本见 [LICENSE](LICENSE)。

Copyright (c) 2026 Codex Quota contributors

本项目是独立的第三方工具，与 OpenAI 或 Codex 官方无隶属、代理或背书关系。OpenAI、Codex 等名称及相关标识归各自权利人所有。
