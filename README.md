# Codex Quota

一个轻量的 Windows 原生 Codex 额度悬浮窗。它通过本机已登录的 Codex CLI 读取账户额度，在桌面边缘持续显示剩余额度、重置时间和连接状态。

## 功能

- 以悬浮球显示额度状态，单击可展开或收起详情面板
- 显示主、次额度窗口的剩余百分比与重置时间
- 支持拖拽，并自动吸附到当前显示器的四条边缘
- 支持多显示器和 DPI 缩放
- 可配置始终置顶、开机启动及面板常驻
- 支持手动刷新，以及 1、5、10、30 分钟的自动刷新间隔
- Codex 额度发生变化时自动刷新，连接中断后自动重连
- 提供系统托盘菜单，可快速打开官方 Usage 页面
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

托盘菜单提供显示/隐藏、立即刷新、刷新间隔、官方 Usage 页面、始终置顶、开机启动、面板常驻和退出等操作。

## 配置与日志

应用数据保存在：

```text
%LOCALAPPDATA%\codex-quota\
```

其中：

- `config.json`：窗口位置和应用设置
- `codex-quota.log`：运行日志
- `codex-quota.log.old`：轮换后的上一份日志

配置文件损坏时，程序会将其重命名为 `config.invalid-<时间戳>.json`，随后使用默认配置启动。

## 开发检查

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test
```

## 隐私说明

程序不会要求单独填写 OpenAI 凭据。它仅在本机启动 `codex app-server --stdio`，并复用 Codex CLI 当前的登录状态读取账户额度。

## 许可证

MIT
