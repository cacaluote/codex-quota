# Codex Quota

一个轻量的 Windows 原生 Codex 额度悬浮窗。它以本机 Codex 会话日志为主要来源，在桌面边缘持续显示剩余额度、重置时间、Token 用量与 API 牌价等价价值。

## 功能
<img alt="CodexQ-ball" width="230" src="https://github.com/user-attachments/assets/334533e0-cbc9-43cb-a95d-215cac1bf3cb" />
<img alt="codexQ-panel" width="230" src="https://github.com/user-attachments/assets/50cbdb82-90dc-44a7-8125-ff72ead31d16" />
<img alt="CodexQ-menu" width="230" src="https://github.com/user-attachments/assets/f7b2e359-5621-412f-b479-ae421fe41736" />



- 以悬浮球显示额度状态，单击可展开或收起详情面板
- 显示主、次额度窗口的剩余百分比与重置时间，以及今日、本期的 Token 用量和对应的 API 牌价等价美元
- 悬浮球的百分环与中心数字在球出现时从 0 扫入、额度变化时平滑过渡（数字用更短的时长先落定），剩余额度偏低（<20% 或已耗尽）时以约 1.4 秒周期做低频脉冲提示；不拖拽、不展开面板、系统动画开启时才动
- 支持拖拽，并自动吸附到当前显示器的四条边缘
- 支持多显示器和 DPI 缩放
- 可配置始终置顶、开机启动、面板常驻及“跟随 Codex”
- 支持手动刷新，以及 1、2、5、10、30 分钟的自动刷新间隔
- 本地日志优先：不常驻任何 Codex 子进程，仅在需要时临时拉起 `codex app-server` 读取一次账户额度
- 使用 Win32、Direct2D 和 DirectWrite 实现，无 WebView 依赖

## 运行要求

- Windows 10/11
- 已安装并登录 Codex CLI，且本机留有会话日志（`%USERPROFILE%\.codex\sessions`）：额度快照与今日/本期用量都从这些日志读取
- `codex.exe` 位于以下任一位置（仅“按需读取账户额度”时需要；只读本地日志显示时不需要）：
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

托盘菜单提供显示/隐藏悬浮球、立即刷新、刷新间隔（1、2、5、10、30 分钟）、始终置顶、开机启动、跟随 Codex、面板常驻和退出。

数据默认来自本地：启动时直接读取本地日志中最近一次额度快照并立即显示，正常情况下不启动任何 Codex 子进程。仅在以下时机临时拉起 `codex app-server --stdio` 读取一次账户额度与方案类型、随即退出：本地没有快照、快照年龄达到“刷新间隔”、快照的额度窗口已过重置时间，或手动点击“立即刷新”（同时刷新模型价格表）。读取失败时保留本地显示，并按 1、2、5、10、30 秒退避后重试。面板中的百分比在快照超过“刷新间隔两倍”仍未更新时才会整列收成 `--`，中间的余量留给按需读取。

“跟随 Codex”默认关闭。启用后，应用在后台监听 Codex Windows App 和 Codex CLI；检测到 Codex 运行时显示悬浮球，最后一个实例退出后自动收起并释放额度查询和渲染资源。跟随期间不能手动显示或隐藏悬浮球，开机启动设置不受影响。

## 面板内容

悬浮球显示主额度窗口的剩余百分比，任一额度窗口耗尽时显示 `0`，没有可用快照时显示 `--`。展开后的面板自上而下为：

- **5h额度 / 周额度 / 月额度**：短、长两个额度窗口各一行，显示剩余百分比与该窗口的重置时间（本地时间 `月/日 时:分`）
- **今日使用**：今日 Token 用量与 API 牌价等价价值
- **今日超额**：触顶后本机的溢出 Token 用量与 credits 实扣折算美元（本机溢出与实扣都为 0 时该行隐藏）
- **本期使用**：本期 Token 用量与等价价值
- **本期超额**：同“今日超额”，按当前额度周期统计
- **本期估值**：本期窗口满额的估算价值，右侧灰字为社区参考值 `$120`
- **更新时间**：快照的实际更新时间，超期时追加“已过期”并转为黄色

剩余百分比低于 50% 显示黄色、低于 20% 显示红色；Token 用量按万/亿压缩显示，价值超过 `$100` 时省略小数。

## 数据口径与近似

“今日使用”和“本期使用”均从 `%CODEX_HOME%\sessions`（未设置时为 `%USERPROFILE%\.codex\sessions`）及 `archived_sessions` 中只读解析官方 OpenAI 会话的 `token_count` 元数据。“本期使用”按本地额度快照提供的窗口起点过滤本地事件。程序启动、监听异常及统计日期或周期变化时执行全量扫描，正常运行时监听会话目录并只增量处理变化的 JSONL。本地日志缺失或不可靠时，对应数值显示 `--`。这些本地数值仅代表本机留下日志的使用量，不包含其他设备、临时会话或已删除日志；同一天切换多个官方账号时，本地日志也可能无法区分账号，该局限同样适用于本地额度快照与方案徽标。

价值按 models.dev 的 OpenAI 牌价折算：未缓存 input 按 input 价、缓存命中按 `cache_read` 价、output（含 reasoning）按 output 价，模型取每个事件之前最近一次 `turn_context` 记录（会话中途可切换模型）。价格表中查不到价格的模型不计价（宁可少算不虚算），尚无价格表时价值列显示 `--`。

“本期估值”是账号侧估算：本期已用美元 ÷ 周额度已用百分比 × 100，即本期窗口跑满时的 API 等价价值。周额度已用不足 1%、本机本期无用量或快照已过期时无法估算，显示 `--`；右侧灰字 `$120` 是社区参考的周额度美元价值，为固定常量，不随账号变化。“今日超额”和“本期超额”中的美元来自账号级 credits 余额观测的负跳变（只有两次观测都落在统计窗口内才计入，跨窗口的差额无法确定发生时刻，不归入今日或本期），按 500 credits = $20 折算实付，因此可能包含其他设备的消耗，与“本机溢出 Token”口径不同。

## 配置与日志

应用数据保存在：

```text
%LOCALAPPDATA%\codex-quota\
```

其中：

- `config.json`：窗口位置和应用设置
- `usage-cache-v1.json`：本机 Codex 会话用量的增量解析缓存，不包含对话正文或凭据
- `models-dev-openai.json`：上次成功拉取的 OpenAI 模型价格表
- `codex-quota.log`：运行日志
- `codex-quota.log.old`：轮换后的上一份日志

配置文件损坏时，程序会将其重命名为 `config.invalid-<时间戳>.json`，随后使用默认配置启动。`config.json` 的字段：

- `placement`：`monitor_device`（显示器设备名）、`edge`（`left`/`right`/`top`/`bottom`）、`offset_dip`（距该边的位置），拖动后自动更新
- `always_on_top`、`start_with_windows`、`follow_codex`、`collapse_on_outside_click`：与托盘菜单“始终置顶”“开机启动”“跟随 Codex”“面板常驻”一一对应
- `quota_refresh_interval_secs`：刷新间隔，默认 300 秒，可设为 60–3600 秒
- `follow_codex_check_interval_secs`：控制“跟随 Codex”的空闲检查间隔，默认 2 秒，可设置为 1–60 秒。修改后需重启应用生效

`usage-cache-v1.json` 的内部缓存格式版本会随升级自动作废重建（一次性全量扫描，通常在几十毫秒内完成）。模型价格表在启动后拉取一次（若已有 24 小时内的缓存则等到满 24 小时再拉），之后每 24 小时刷新；拉取失败按 30 秒、1、2、5、5 分钟退避重试，连续失败后暂停一天。托盘菜单的“立即刷新”会同时刷新额度与价格表。

程序不包含文本输入控件，因此会在创建首个窗口前禁用 UI 线程的 IME，避免原生托盘菜单首次弹出时加载第三方输入法组件。

## 开发检查

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test
```

## 隐私说明

程序不会读取或缓存会话中的提示词、回复、工具输出和凭据，只解析 `token_count` 元数据（用量计数、额度百分比、重置时间、方案类型、credits 余额）。

默认情况下程序不启动任何 Codex 子进程：额度百分比、重置时间与方案类型均来自本地会话日志中 `token_count` 事件自带的额度快照；该快照由 Codex CLI 在每次对话回合时写入，因此正在使用 Codex 时数据为分钟级新鲜，空闲时停留在最近一次快照（面板会标注更新时间）。跨设备或其他账号的最新用量只有在下一次本地会话活动或按需读取后才会体现。上面列出的按需读取时机（无快照、快照到期、窗口重置、手动刷新）会在本机启动 `codex app-server --stdio`，复用 Codex CLI 当前的登录状态读取账户额度、周期边界与方案类型，读取完成后立即结束该进程。

程序唯一的网络访问是向 `https://models.dev/api.json` 拉取模型价格表（用于把 Token 用量换算成 API 牌价等价美元），不上传任何本地数据；拉取结果只保留 OpenAI 模型的价格并缓存在 `%LOCALAPPDATA%\codex-quota\models-dev-openai.json`。该请求失败不影响额度与用量显示，只是价值列显示 `--`。

## 许可证

本项目源代码采用 [MIT License](LICENSE) 发布，完整许可文本见 [LICENSE](LICENSE)。

Copyright (c) 2026 Codex Quota contributors

本项目是独立的第三方工具，与 OpenAI 或 Codex 官方无隶属、代理或背书关系。OpenAI、Codex 等名称及相关标识归各自权利人所有。
