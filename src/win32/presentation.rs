use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use crate::config::UnitStyle;
use crate::quota::{AppState, ConnectionStatus, QuotaColor, QuotaPull, QuotaWindow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlanColor {
    Neutral,
    Plus,
    Pro,
    Business,
    Enterprise,
    Edu,
    Unknown,
}

pub(super) fn quota_window_label(
    window: Option<&QuotaWindow>,
    plan_type: Option<&str>,
    short_term: bool,
) -> String {
    window.map_or_else(
        || {
            if short_term {
                "5h额度"
            } else if plan_type == Some("free") {
                "月额度"
            } else {
                "周额度"
            }
            .to_owned()
        },
        QuotaWindow::window_label,
    )
}

/// 期间行（本周使用 / 本周超额 / 本周估值）里的那个"期"的标签。
///
/// "期"按定义就是**长期额度窗口**（`current_period_boundary` 取它的
/// `resets_at - window_duration`），而窗口名由 [`QuotaWindow::window_short_label`]
/// 分类。所以这里直接复用同一个分类器、只加「本」字——**不能手写第二份阶梯**：
/// 免费账号的长期窗口是月，各写各的就会出现「月额度」配「本周使用」这种自相矛盾。
///
/// 窗口未知（启动中、快照过期）时按 `plan_type` 兜底，判据与
/// [`quota_window_label`] 一致。5h 窗口不套「本」（「本5h使用」不像话），
/// 退回中性的「本期」。
pub(super) fn period_label(window: Option<&QuotaWindow>, plan_type: Option<&str>) -> String {
    let Some(window) = window else {
        return if plan_type == Some("free") {
            "本月"
        } else {
            "本周"
        }
        .to_owned();
    };
    let short = window.window_short_label();
    if short == "5h" {
        "本期".to_owned()
    } else {
        format!("本{short}")
    }
}

/// 期间行的三个标签，由 [`period_label`] 加后缀**在一处**拼出来。
///
/// 不在调用点手写后缀：漏掉一个「使用」这种错，像素测试看不见（文字短一截
/// 仍然画得下），宽度测试也拦不住（变短只会更宽松）。集中生成后，内容测试
/// 就是唯一的出口。
pub(super) struct PeriodLabels {
    pub(super) usage: String,
    pub(super) overflow: String,
    pub(super) estimate: String,
}

pub(super) fn period_labels(window: Option<&QuotaWindow>, plan_type: Option<&str>) -> PeriodLabels {
    let period = period_label(window, plan_type);
    PeriodLabels {
        usage: format!("{period}使用"),
        overflow: format!("{period}超额"),
        estimate: format!("{period}估值"),
    }
}

/// The windows safe to render as percentages. A snapshot beyond its freshness
/// window is withheld entirely: it predates unknown server-side changes (a
/// reset, another device's usage) and misleads more than a blank. The on-demand
/// pull replaces it within seconds.
pub(super) fn display_windows(
    state: &AppState,
    now: SystemTime,
) -> (Option<&QuotaWindow>, Option<&QuotaWindow>) {
    if state.is_stale(now) {
        return (None, None);
    }
    state
        .snapshot
        .as_ref()
        .map_or((None, None), |snapshot| snapshot.active_windows(now))
}

pub(super) fn display_period_total_value(state: &AppState, now: SystemTime) -> Option<f64> {
    let (_, long_term) = display_windows(state, now);
    long_term?;
    state.period_total_value_estimate
}

/// 期间满额 token 估算 = 本期本机 token ÷ 长期窗口已用% × 100。
///
/// 这是 [`display_period_total_value`] 的 token 版本：同一个「窗口跑满」的量在
/// 两个单位下的说法，门控与下限也照抄（快照过期、长期窗口不活跃、已用% < 1、
/// 本机本期无用量都给不出有意义的估算）。它取代了原来那个固定 `$120` 常量：
/// 常量不随账号变化，而这个估算跟着本机用量与真实百分比走。
///
/// **不依赖价格表**——token 数来自本机日志、百分比来自额度快照，两者都在本地，
/// 所以价格表还没拉下来（美元列是 `--`）时这一列照样有值。两列因此各自门控。
pub(super) fn display_period_total_token_estimate(
    state: &AppState,
    now: SystemTime,
) -> Option<u64> {
    let (_, long_term) = display_windows(state, now);
    let used_percent = long_term?.used_percent;
    let tokens = state.current_period_tokens?;
    // 与 `period_total_value_estimate` 同一个 1% 下限：百分比越接近 0，放大倍数
    // 越离谱，估出来的 token 数也就越没有意义。f64→u64 的 `as` 在溢出时饱和，
    // 不会回绕。
    (used_percent >= 1.0 && tokens > 0)
        .then(|| (tokens as f64 * 100.0 / used_percent).round() as u64)
}

/// 面板内容行位：首行顶部与行距（dip）。
pub(super) const FIRST_ROW_TOP_DIP: f32 = 42.0;
pub(super) const ROW_STEP_DIP: f32 = 27.0;
/// 6 行基础内容：两组额度行、今日使用、本期使用、本期估值、更新时间；
/// 超额行按可见性追加。行位 = 首行顶 + 行距×(n−1)，末行高 23 + 底边距 27。
const BASE_PANEL_ROWS: f32 = 6.0;
/// 末行下方的留白（dip）。
const PANEL_BOTTOM_PADDING_DIP: f32 = 50.0;

/// 无超额（6 行，常驻状态）时的展开高度，也是额度状态锁损坏时的兜底高度。
pub(super) const COMPACT_PANEL_HEIGHT_DIP: f32 =
    FIRST_ROW_TOP_DIP + (BASE_PANEL_ROWS - 1.0) * ROW_STEP_DIP + PANEL_BOTTOM_PADDING_DIP;

/// 超额行是否显示：本机溢出 token 与 credits 实扣任一非零。多设备场景下
/// 本机 token 可能为 0 而实扣 > 0（其他设备的消耗），必须显示；数据不可靠
/// （None）视为未发生，面板保持精简——大多数时间没有溢出。
pub(super) fn today_overflow_visible(state: &AppState) -> bool {
    overflow_visible(state.today_overflow_tokens, state.today_overflow_cost)
}

pub(super) fn period_overflow_visible(state: &AppState) -> bool {
    overflow_visible(
        state.current_period_overflow_tokens,
        state.current_period_overflow_cost,
    )
}

fn overflow_visible(tokens: Option<u64>, cost: Option<f64>) -> bool {
    tokens.is_some_and(|value| value > 0) || cost.is_some_and(|value| value > 0.0)
}

/// 面板高度随超额行数动态收缩：6 行（无溢出，常驻状态）227 dip，7 行
/// 254，8 行（今日+本期都溢出）281。行位 = 首行顶 + 行距×(n−1)，末行高
/// 23 + 底边距 27。
pub(super) fn panel_height_dip(state: &AppState) -> f32 {
    let rows = BASE_PANEL_ROWS
        + f32::from(u8::from(today_overflow_visible(state)))
        + f32::from(u8::from(period_overflow_visible(state)));
    FIRST_ROW_TOP_DIP + (rows - 1.0) * ROW_STEP_DIP + PANEL_BOTTOM_PADDING_DIP
}

/// The floating ball's glanceable quota: the primary window's remaining, or
/// the only window of single-window accounts. It reads 0 while any active
/// window is exhausted — the account cannot serve requests regardless of the
/// primary window — and `--` while no fresh window is known.
pub(super) fn ball_quota(state: &AppState, now: SystemTime) -> (String, f64, QuotaColor) {
    // 用户点"立即刷新"时把环和数字一起清空：那一刻球上有两个互相矛盾的指示——
    // 一条说"这是当前读数"的额度弧，和一条说"正在取新读数"的旋转弧——叠在一起
    // 读起来像是数字自己在转。清空后球只剩一件事要说：正在刷新。
    // 只对 Forced 生效：自动拉取每两分钟一次，跟着清空就没法看了。
    if matches!(state.quota_pull, QuotaPull::Forced) {
        return ("--".to_owned(), 0.0, QuotaColor::Unknown);
    }
    let (short_term, long_term) = display_windows(state, now);
    let Some(window) = short_term.or(long_term) else {
        return ("--".to_owned(), 0.0, QuotaColor::Unknown);
    };
    // 与重置通知共用同一套「卡死」语义（`QuotaSnapshot::is_blocked`）；能走到
    // 这里说明快照没过期门控，所以两种口径看到的是同一对窗口。
    let blocked = state
        .snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.is_blocked(now));
    if blocked {
        return ("0".to_owned(), 0.0, QuotaColor::Critical);
    }
    let remaining = window.remaining_percent();
    (format!("{remaining:.0}"), remaining, window.color())
}

/// 悬浮球是否该显示不确定进度弧（"正在拉取额度"）。
///
/// 手动刷新是用户刚点下去的动作，无论球上有没有值都给这一下反馈；自动拉取
/// 只在球上本来没值可显示（`--`）时才转——否则按刷新间隔周期性闪动就成了噪音，
/// 而且那种情况下球上也没有别的信息可以看。
///
/// **启动阶段（`Connecting`）同样要转**：那时还没轮到第一次额度 RPC，先跑的是
/// 本地会话日志的全量扫描（实测 2.8 秒，解析占 2.78 秒），期间球上只有 `--`
/// 什么都不动，看着像已经卡死。面板此刻写的也是「正在连接 Codex」，两者说的
/// 是同一件事。判定里的"球上没值"是必要条件：只要有值可显示，就不该转。
pub(super) fn ball_is_pulling(state: &AppState, color: QuotaColor) -> bool {
    if !matches!(color, QuotaColor::Unknown) {
        return matches!(state.quota_pull, QuotaPull::Forced);
    }
    match state.quota_pull {
        QuotaPull::Forced | QuotaPull::Automatic => true,
        QuotaPull::Idle => matches!(state.status, ConnectionStatus::Connecting),
    }
}

pub(super) fn panel_title(state: &AppState) -> (&str, Option<&str>) {
    match &state.status {
        ConnectionStatus::Connecting => ("正在连接 Codex", None),
        ConnectionStatus::Online => ("Codex", state.plan_type.as_deref()),
        ConnectionStatus::Reconnecting { .. } => ("正在重新连接", None),
        ConnectionStatus::Error { .. } => ("Codex 暂不可用", None),
    }
}

pub(super) fn format_local_timestamp(time: SystemTime) -> String {
    try_format_local_timestamp(time).unwrap_or_else(|| "--".to_owned())
}

fn try_format_local_timestamp(time: SystemTime) -> Option<String> {
    const WINDOWS_EPOCH_OFFSET_SECONDS: u64 = 11_644_473_600;
    const TICKS_PER_SECOND: u64 = 10_000_000;

    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    let ticks = duration
        .as_secs()
        .checked_add(WINDOWS_EPOCH_OFFSET_SECONDS)?
        .checked_mul(TICKS_PER_SECOND)?
        .checked_add(u64::from(duration.subsec_nanos()) / 100)?;
    let file_time = FILETIME {
        dwLowDateTime: u32::try_from(ticks & u64::from(u32::MAX)).ok()?,
        dwHighDateTime: u32::try_from(ticks >> 32).ok()?,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: both output structures are initialized and exclusively borrowed for their calls.
    unsafe {
        FileTimeToSystemTime(&file_time, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(format_calendar_time(&local))
}

fn format_calendar_time(time: &SYSTEMTIME) -> String {
    format!(
        "{:02}/{:02} {:02}:{:02}",
        time.wMonth, time.wDay, time.wHour, time.wMinute
    )
}

fn format_token_count(tokens: u64, unit: UnitStyle) -> String {
    match unit {
        UnitStyle::Zh => match tokens {
            0..=9_999 => tokens.to_string(),
            10_000..=99_999_999 => format_compact_token_unit(tokens, 10_000, "万"),
            _ => format_compact_token_unit(tokens, 100_000_000, "亿"),
        },
        UnitStyle::En => match tokens {
            0..=999 => tokens.to_string(),
            1_000..=999_999 => format_compact_token_unit(tokens, 1_000, "K"),
            1_000_000..=999_999_999 => format_compact_token_unit(tokens, 1_000_000, "M"),
            _ => format_compact_token_unit(tokens, 1_000_000_000, "B"),
        },
    }
}

/// Token 用量的显示值。单位后缀（` Token`）已去掉：面板行标签「今日使用 /
/// 本期使用 / 今日超额 / 本期超额」已经说明它是什么，而重置列只有 122 DIP，
/// 少 6 个字符就是少一分被裁剪的风险。
pub(super) fn format_token_usage(tokens: Option<u64>, unit: UnitStyle) -> String {
    tokens.map_or_else(|| "--".to_owned(), |value| format_token_count(value, unit))
}

/// 重置时间列的文本：本地时间，按需在其后附带紧凑倒计时。
///
/// 倒计时是**绘制那一刻**的值——面板静止时 30 秒才重绘一次，所以它按分钟跳，
/// 不逐秒走（因此格式里没有秒）。
pub(super) fn format_reset_column(
    window: &QuotaWindow,
    now: SystemTime,
    show_countdown: bool,
) -> String {
    let time = format_local_timestamp(window.resets_at);
    if !show_countdown {
        return time;
    }
    match window.resets_at.duration_since(now) {
        Ok(remaining) if !remaining.is_zero() => {
            format!("{time} ({})", format_compact_countdown(remaining))
        }
        // 过期的窗口在 `Snapshot::active_windows` 就被滤掉、整行显示 `--`，
        // 走不到这里；这里只是把函数补全。
        _ => time,
    }
}

/// 紧凑倒计时：无空格、不带秒。
///
/// `1d6h` / `1h30m` / `6m` / `<1m`。每个档位都报两个分量，第二个分量为 0 时
/// 省略，免得出现 `1d0h`、`6h0m`；天数 ≥ 10 时连小时一起省掉——重置列只有
/// 122 DIP（`TIME_COLUMN_LEFT` → 面板右边距），`9d23h` 已经贴着上限，月窗口的
/// `31d23h` 会溢出。这与 `format_usd` 在 ≥$100 去小数是同一个取舍：宁可少一位
/// 精度，也不要被裁字。
fn format_compact_countdown(remaining: Duration) -> String {
    let total_minutes = remaining.as_secs() / 60;
    let days = total_minutes / 1_440;
    let hours = (total_minutes % 1_440) / 60;
    let minutes = total_minutes % 60;
    if days >= 10 {
        format!("{days}d")
    } else if days > 0 {
        if hours == 0 {
            format!("{days}d")
        } else {
            format!("{days}d{hours}h")
        }
    } else if hours > 0 {
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{minutes}m")
        }
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "<1m".to_owned()
    }
}

/// 价值列宽度有限（96→140 dip）：≥$100 去小数，避免长数字被裁剪。
/// cost 不可能为负，负零/零统一显示 $0.00。
pub(super) fn format_usd(cost: Option<f64>) -> String {
    cost.map_or_else(
        || "--".to_owned(),
        |value| {
            if value <= 0.0 {
                "$0.00".to_owned()
            } else if value >= 100.0 {
                format!("${value:.0}")
            } else {
                format!("${value:.2}")
            }
        },
    )
}

fn format_compact_token_unit(tokens: u64, unit: u64, suffix: &str) -> String {
    let unit = u128::from(unit);
    let tenths = (u128::from(tokens) * 10 + unit / 2) / unit;
    if tenths.is_multiple_of(10) {
        format!("{}{suffix}", tenths / 10)
    } else {
        format!("{}.{:01}{suffix}", tenths / 10, tenths % 10)
    }
}

pub(super) fn plan_type_label(plan_type: &str) -> &str {
    match plan_type {
        "free" => "Free",
        "go" => "Go",
        "plus" => "Plus",
        "pro" => "Pro",
        "prolite" => "Pro Lite",
        "team" => "Team",
        "self_serve_business_usage_based" | "business" => "Business",
        "ent26" | "enterprise_cbp_usage_based" | "enterprise" => "Enterprise",
        "edu" => "Edu",
        "unknown" => "未知方案",
        value => value,
    }
}

pub(super) fn plan_type_color(plan_type: &str) -> PlanColor {
    match plan_type {
        "free" | "go" => PlanColor::Neutral,
        "plus" => PlanColor::Plus,
        "pro" | "prolite" => PlanColor::Pro,
        "team" | "self_serve_business_usage_based" | "business" => PlanColor::Business,
        "ent26" | "enterprise_cbp_usage_based" | "enterprise" => PlanColor::Enterprise,
        "edu" => PlanColor::Edu,
        _ => PlanColor::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::quota::QuotaSnapshot;

    fn window(duration: Duration) -> QuotaWindow {
        QuotaWindow {
            used_percent: 10.0,
            window_duration: duration,
            resets_at: UNIX_EPOCH + Duration::from_hours(1),
        }
    }

    #[test]
    fn overflow_rows_stay_hidden_until_usage_or_cost_is_nonzero() {
        // 默认/全零状态精简显示；不可靠（None）同样视为未发生。
        assert!(!today_overflow_visible(&AppState::default()));

        let zero = AppState {
            today_overflow_tokens: Some(0),
            today_overflow_cost: Some(0.0),
            ..AppState::default()
        };
        assert!(!today_overflow_visible(&zero));

        let tokens_only = AppState {
            today_overflow_tokens: Some(100),
            ..AppState::default()
        };
        assert!(today_overflow_visible(&tokens_only));

        // 多设备：本机 token 为 0 但 credits 实扣 > 0，行必须显示。
        let cost_only = AppState {
            today_overflow_tokens: Some(0),
            today_overflow_cost: Some(0.4),
            ..AppState::default()
        };
        assert!(today_overflow_visible(&cost_only));
    }

    #[test]
    fn panel_height_shrinks_with_hidden_overflow_rows() {
        // 6 行 227 / 7 行 254 / 8 行 281，与 renderer 的行位排布一致。
        assert!((panel_height_dip(&AppState::default()) - 227.0).abs() < f32::EPSILON);

        let one_overflow_row = AppState {
            current_period_overflow_tokens: Some(200),
            ..AppState::default()
        };
        assert!((panel_height_dip(&one_overflow_row) - 254.0).abs() < f32::EPSILON);

        let both_overflow_rows = AppState {
            today_overflow_tokens: Some(1),
            current_period_overflow_cost: Some(0.4),
            ..AppState::default()
        };
        assert!((panel_height_dip(&both_overflow_rows) - 281.0).abs() < f32::EPSILON);
    }

    #[test]
    fn compact_panel_height_constant_matches_the_six_row_panel() {
        // 锁损坏兜底用的常量必须与真实精简高度同源，避免再次漂移。
        assert!((COMPACT_PANEL_HEIGHT_DIP - 227.0).abs() < f32::EPSILON);
        assert!(
            (COMPACT_PANEL_HEIGHT_DIP - panel_height_dip(&AppState::default())).abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn calendar_time_omits_year_and_seconds() {
        let value = format_calendar_time(&SYSTEMTIME {
            wYear: 2026,
            wMonth: 8,
            wDay: 6,
            wHour: 0,
            wMinute: 1,
            wSecond: 2,
            ..Default::default()
        });
        assert_eq!(value, "08/06 00:01");
    }

    #[test]
    fn token_count_below_ten_thousand_stays_unscaled() {
        assert_eq!(format_token_count(4_280, UnitStyle::Zh), "4280");
    }

    #[test]
    fn token_count_uses_ten_thousand_unit() {
        assert_eq!(format_token_count(42_803_000, UnitStyle::Zh), "4280.3万");
    }

    /// 用量值不再带 `Token` 后缀：行标签已经说明它是什么。
    #[test]
    fn today_usage_has_no_unit_suffix() {
        assert_eq!(
            format_token_usage(Some(42_803_000), UnitStyle::Zh),
            "4280.3万"
        );
        assert_eq!(format_token_usage(Some(42_803_000), UnitStyle::En), "42.8M");
        assert_eq!(format_token_usage(None, UnitStyle::Zh), "--");
        assert_eq!(format_token_usage(None, UnitStyle::En), "--");
    }

    #[test]
    fn usd_format_drops_cents_at_hundred_dollars_and_above() {
        assert_eq!(format_usd(Some(1.234)), "$1.23");
        assert_eq!(format_usd(Some(45.67)), "$45.67");
        assert_eq!(format_usd(Some(1145.67)), "$1146");
        assert_eq!(format_usd(Some(12345.6)), "$12346");
        assert_eq!(format_usd(Some(0.004)), "$0.00");
        // 负零与零统一显示 $0.00。
        assert_eq!(format_usd(Some(-0.0)), "$0.00");
        assert_eq!(format_usd(Some(0.0)), "$0.00");
        assert_eq!(format_usd(None), "--");
    }

    #[test]
    fn token_count_uses_hundred_million_unit() {
        assert_eq!(format_token_count(128_000_000, UnitStyle::Zh), "1.3亿");
    }

    #[test]
    fn token_count_omits_zero_decimal() {
        assert_eq!(format_token_count(100_000_000, UnitStyle::Zh), "1亿");
    }

    #[test]
    fn zero_token_count_remains_zero() {
        assert_eq!(format_token_count(0, UnitStyle::Zh), "0");
    }

    /// 西文单位的分档与边界：999/1K/999.9K/1M/1B。
    #[test]
    fn western_token_units_step_at_each_thousand() {
        assert_eq!(format_token_count(999, UnitStyle::En), "999");
        assert_eq!(format_token_count(1_000, UnitStyle::En), "1K");
        assert_eq!(format_token_count(1_240, UnitStyle::En), "1.2K");
        assert_eq!(format_token_count(516_200, UnitStyle::En), "516.2K");
        assert_eq!(format_token_count(999_999, UnitStyle::En), "1000K");
        assert_eq!(format_token_count(1_000_000, UnitStyle::En), "1M");
        // 用户举例的那个值：516.2M。
        assert_eq!(format_token_count(516_200_000, UnitStyle::En), "516.2M");
        assert_eq!(format_token_count(1_000_000_000, UnitStyle::En), "1B");
        assert_eq!(format_token_count(0, UnitStyle::En), "0");
    }

    /// 中文单位的分档边界，逐档对齐西文那一组。
    #[test]
    fn chinese_token_units_step_at_ten_thousand() {
        assert_eq!(format_token_count(9_999, UnitStyle::Zh), "9999");
        assert_eq!(format_token_count(10_000, UnitStyle::Zh), "1万");
        assert_eq!(format_token_count(99_999_999, UnitStyle::Zh), "10000万");
        assert_eq!(format_token_count(516_200_000, UnitStyle::Zh), "5.2亿");
    }

    /// 重置列的倒计时阶梯：无空格、不带秒，天数 ≥ 10 时省掉小时以免超出列宽。
    #[test]
    fn reset_countdown_uses_two_units_without_seconds() {
        let minutes = |count: u64| Duration::from_mins(count);
        assert_eq!(
            format_compact_countdown(minutes(1_440) + minutes(360)),
            "1d6h"
        );
        // 小时为 0 时不写 `1d0h`。
        assert_eq!(format_compact_countdown(minutes(1_440)), "1d");
        assert_eq!(
            format_compact_countdown(minutes(9 * 1_440 + 23 * 60)),
            "9d23h"
        );
        // ≥10 天：只报天，否则月窗口的 `31d23h` 会溢出重置列。
        assert_eq!(format_compact_countdown(minutes(10 * 1_440)), "10d");
        assert_eq!(
            format_compact_countdown(minutes(31 * 1_440 + 23 * 60)),
            "31d"
        );
        // 小时档报两个分量；分钟为 0 时省略（不写 `6h0m`）。
        assert_eq!(format_compact_countdown(minutes(6 * 60 + 30)), "6h30m");
        assert_eq!(format_compact_countdown(minutes(60 + 30)), "1h30m");
        assert_eq!(format_compact_countdown(minutes(6 * 60)), "6h");
        assert_eq!(format_compact_countdown(minutes(6)), "6m");
        assert_eq!(format_compact_countdown(Duration::from_secs(59)), "<1m");
        assert_eq!(format_compact_countdown(Duration::ZERO), "<1m");
    }

    /// 开关关闭时只显示本地时间；开启时在其后追加倒计时。
    #[test]
    fn reset_column_appends_the_countdown_only_when_asked() {
        let now = SystemTime::now();
        let window = QuotaWindow {
            used_percent: 40.0,
            window_duration: Duration::from_hours(5),
            resets_at: now + Duration::from_hours(30),
        };
        let plain = format_reset_column(&window, now, false);
        assert!(!plain.contains('('), "关掉倒计时就只该有本地时间：{plain}");
        let with_countdown = format_reset_column(&window, now, true);
        assert_eq!(with_countdown, format!("{plain} (1d6h)"));
    }

    /// 期间行的前缀必须跟着长期窗口走：周窗口 → 本周，月窗口 → 本月。
    #[test]
    fn period_label_follows_the_long_term_window() {
        let window = |hours: u64| QuotaWindow {
            used_percent: 40.0,
            window_duration: Duration::from_hours(hours),
            resets_at: SystemTime::UNIX_EPOCH + Duration::from_hours(1_000),
        };
        // 用户账号的形态：长期窗口是周。
        assert_eq!(period_label(Some(&window(24 * 7)), None), "本周");
        // 免费账号：长期窗口是月，绝不能说「本周」。
        assert_eq!(period_label(Some(&window(24 * 30)), None), "本月");
        assert_eq!(period_label(Some(&window(24 * 14)), None), "本2周");
        assert_eq!(period_label(Some(&window(24 * 3)), None), "本3天");
        assert_eq!(period_label(Some(&window(24 * 90)), None), "本90天");
        // 5h 窗口不套「本」。
        assert_eq!(period_label(Some(&window(5)), None), "本期");
    }

    /// 窗口未知时的兜底判据必须与 `quota_window_label` 一致。
    #[test]
    fn period_label_falls_back_the_same_way_as_the_window_label() {
        assert_eq!(period_label(None, Some("free")), "本月");
        assert_eq!(period_label(None, Some("plus")), "本周");
        assert_eq!(period_label(None, None), "本周");
        assert_eq!(quota_window_label(None, Some("free"), false), "月额度");
        assert_eq!(quota_window_label(None, Some("plus"), false), "周额度");
    }

    /// 这两行是同一个窗口的两个说法，**任何时长下都不得互相矛盾**。
    ///
    /// 这是把上一次那个「月额度 + 本周使用」钉死的测试：标签只允许来自同一个
    /// 分类器，手写第二份阶梯就会在这里红。
    #[test]
    fn period_label_agrees_with_the_window_label_for_every_duration() {
        for hours in [5, 24, 24 * 3, 24 * 7, 24 * 14, 24 * 30, 24 * 31, 24 * 90] {
            let window = QuotaWindow {
                used_percent: 40.0,
                window_duration: Duration::from_hours(hours),
                resets_at: SystemTime::UNIX_EPOCH + Duration::from_hours(1_000),
            };
            let window_label = window.window_label();
            let period = period_label(Some(&window), None);
            let stem = window_label
                .strip_suffix("额度")
                .expect("窗口名带「额度」后缀");
            if stem == "5h" {
                // 唯一的中性例外：5h 窗口说「本期」。
                assert_eq!(period, "本期", "{hours}h");
            } else {
                assert_eq!(period, format!("本{stem}"), "{hours}h 的两行标签必须同源");
            }
        }
    }

    /// 三个行名必须**带全后缀**。
    ///
    /// 这条是补课：曾经把「本周使用」误传成前缀「本周」，像素测试和宽度测试
    /// 都发现不了（少一截照样画得下），只有断言内容才拦得住。
    #[test]
    fn period_row_labels_carry_the_full_row_name() {
        let window = |hours: u64| QuotaWindow {
            used_percent: 40.0,
            window_duration: Duration::from_hours(hours),
            resets_at: SystemTime::UNIX_EPOCH + Duration::from_hours(1_000),
        };

        let weekly = period_labels(Some(&window(24 * 7)), None);
        assert_eq!(weekly.usage, "本周使用");
        assert_eq!(weekly.overflow, "本周超额");
        assert_eq!(weekly.estimate, "本周估值");

        let monthly = period_labels(Some(&window(24 * 30)), None);
        assert_eq!(monthly.usage, "本月使用");
        assert_eq!(monthly.overflow, "本月超额");
        assert_eq!(monthly.estimate, "本月估值");

        assert_eq!(period_labels(Some(&window(5)), None).usage, "本期使用");
        // 未知窗口的兜底同样要带后缀。
        assert_eq!(period_labels(None, Some("free")).usage, "本月使用");
        assert_eq!(period_labels(None, Some("plus")).usage, "本周使用");
    }

    /// 四个档位的形状：天、小时（含分钟）、分钟、关掉开关。
    #[test]
    fn reset_column_matches_the_requested_shape() {
        let now = SystemTime::now();
        let column = |remaining: Duration| {
            format_reset_column(
                &QuotaWindow {
                    used_percent: 40.0,
                    window_duration: Duration::from_hours(5),
                    resets_at: now + remaining,
                },
                now,
                true,
            )
        };
        assert!(column(Duration::from_hours(30)).ends_with(" (1d6h)"));
        assert!(column(Duration::from_hours(6)).ends_with(" (6h)"));
        assert!(column(Duration::from_mins(90)).ends_with(" (1h30m)"));
        assert!(column(Duration::from_mins(6)).ends_with(" (6m)"));
    }

    #[test]
    fn plan_type_uses_friendly_pro_label() {
        assert_eq!(plan_type_label("pro"), "Pro");
    }

    #[test]
    fn online_title_includes_plan_without_quota_suffix() {
        let state = AppState {
            status: ConnectionStatus::Online,
            plan_type: Some("plus".to_owned()),
            ..AppState::default()
        };

        assert_eq!(panel_title(&state), ("Codex", Some("plus")));
    }

    #[test]
    fn usage_based_business_plan_uses_short_label() {
        assert_eq!(
            plan_type_label("self_serve_business_usage_based"),
            "Business"
        );
    }

    #[test]
    fn plus_plan_uses_blue_accent() {
        assert_eq!(plan_type_color("plus"), PlanColor::Plus);
    }

    #[test]
    fn enterprise_alias_uses_enterprise_accent() {
        assert_eq!(plan_type_color("ent26"), PlanColor::Enterprise);
    }

    #[test]
    fn unrecognized_plan_uses_unknown_accent() {
        assert_eq!(plan_type_color("future_plan"), PlanColor::Unknown);
    }

    #[test]
    fn lone_long_window_leaves_short_term_slot_empty() {
        let snapshot = QuotaSnapshot {
            limit_id: "codex".to_owned(),
            primary: window(Duration::from_hours(168)),
            secondary: None,
            received_at: UNIX_EPOCH,
        };
        let (short_term, long_term) = snapshot.active_windows(UNIX_EPOCH);
        assert!(short_term.is_none() && long_term.is_some());
    }

    #[test]
    fn shorter_window_is_classified_as_five_hour_quota() {
        let snapshot = QuotaSnapshot {
            limit_id: "codex".to_owned(),
            primary: window(Duration::from_hours(168)),
            secondary: Some(window(Duration::from_hours(5))),
            received_at: UNIX_EPOCH,
        };
        let (short_term, long_term) = snapshot.active_windows(UNIX_EPOCH);
        assert!(
            short_term.is_some_and(|value| value.window_duration == Duration::from_hours(5))
                && long_term
                    .is_some_and(|value| value.window_duration == Duration::from_hours(168))
        );
    }

    #[test]
    fn free_plan_uses_monthly_fallback_for_missing_long_window() {
        assert_eq!(quota_window_label(None, Some("free"), false), "月额度");
    }

    fn quota_window(used_percent: f64, duration: Duration) -> QuotaWindow {
        QuotaWindow {
            used_percent,
            window_duration: duration,
            resets_at: UNIX_EPOCH + Duration::from_hours(1),
        }
    }

    fn expired_quota_window(used_percent: f64, duration: Duration) -> QuotaWindow {
        QuotaWindow {
            used_percent,
            window_duration: duration,
            resets_at: UNIX_EPOCH - Duration::from_hours(1),
        }
    }

    fn ball_snapshot(primary: QuotaWindow, secondary: Option<QuotaWindow>) -> AppState {
        AppState {
            snapshot: Some(QuotaSnapshot {
                limit_id: "codex".to_owned(),
                primary,
                secondary,
                received_at: UNIX_EPOCH,
            }),
            ..AppState::default()
        }
    }

    #[test]
    fn ball_tracks_primary_window_while_not_blocked() {
        // Fresh 5h window with an unconstrained weekly window: the ball keeps
        // tracking the primary value instead of freezing on the weekly one.
        let state = ball_snapshot(
            quota_window(0.0, Duration::from_hours(5)),
            Some(quota_window(60.0, Duration::from_hours(168))),
        );

        assert_eq!(
            ball_quota(&state, UNIX_EPOCH),
            ("100".to_owned(), 100.0, QuotaColor::Healthy)
        );
    }

    #[test]
    fn ball_reads_zero_while_any_active_window_is_exhausted() {
        let state = ball_snapshot(
            quota_window(85.0, Duration::from_hours(5)),
            Some(quota_window(100.0, Duration::from_hours(168))),
        );

        assert_eq!(
            ball_quota(&state, UNIX_EPOCH),
            ("0".to_owned(), 0.0, QuotaColor::Critical)
        );
    }

    #[test]
    fn ball_reads_zero_when_the_only_window_is_exhausted() {
        let state = ball_snapshot(quota_window(100.0, Duration::from_hours(168)), None);

        assert_eq!(
            ball_quota(&state, UNIX_EPOCH),
            ("0".to_owned(), 0.0, QuotaColor::Critical)
        );
    }

    #[test]
    fn ball_falls_back_to_the_long_window_while_short_is_expired() {
        let state = ball_snapshot(
            expired_quota_window(50.0, Duration::from_hours(5)),
            Some(quota_window(70.0, Duration::from_hours(168))),
        );

        assert_eq!(
            ball_quota(&state, UNIX_EPOCH),
            ("30".to_owned(), 30.0, QuotaColor::Warning)
        );
    }

    #[test]
    fn ball_reads_unknown_without_active_windows() {
        let state = ball_snapshot(expired_quota_window(50.0, Duration::from_hours(5)), None);

        assert_eq!(
            ball_quota(&state, UNIX_EPOCH),
            ("--".to_owned(), 0.0, QuotaColor::Unknown)
        );
        assert_eq!(
            ball_quota(&AppState::default(), UNIX_EPOCH),
            ("--".to_owned(), 0.0, QuotaColor::Unknown)
        );
    }

    #[test]
    fn manual_refresh_clears_the_ball_so_only_the_spinner_speaks() {
        use crate::quota::QuotaPull;

        let now = SystemTime::now();
        let state = AppState {
            snapshot: Some(QuotaSnapshot {
                limit_id: "codex".to_owned(),
                primary: QuotaWindow {
                    used_percent: 38.0,
                    window_duration: Duration::from_hours(5),
                    resets_at: now + Duration::from_hours(4),
                },
                secondary: None,
                received_at: now,
            }),
            ..AppState::default()
        };
        assert_eq!(ball_quota(&state, now).0, "62", "先确认基线读数");

        // 手动刷新：环与数字一起清空，球上只剩旋转弧。
        let forced = AppState {
            quota_pull: QuotaPull::Forced,
            ..state.clone()
        };
        assert_eq!(
            ball_quota(&forced, now),
            ("--".to_owned(), 0.0, QuotaColor::Unknown)
        );

        // 自动拉取不动读数：它每两分钟一次，跟着清空就没法看了。
        let automatic = AppState {
            quota_pull: QuotaPull::Automatic,
            ..state.clone()
        };
        assert_eq!(ball_quota(&automatic, now).0, "62");
    }

    #[test]
    fn ball_spinner_only_covers_pulls_that_are_worth_showing() {
        use crate::quota::QuotaPull;

        let with = |status, pull| AppState {
            status,
            quota_pull: pull,
            ..AppState::default()
        };

        // 已经连上、没在拉取：不转——哪怕球上没值，也没有"正在取"这件事可说。
        assert!(!ball_is_pulling(
            &with(ConnectionStatus::Online, QuotaPull::Idle),
            QuotaColor::Unknown
        ));
        assert!(!ball_is_pulling(
            &with(ConnectionStatus::Online, QuotaPull::Idle),
            QuotaColor::Healthy
        ));
        // 手动刷新：无论球上有没有值都给这一下反馈。
        for color in [
            QuotaColor::Healthy,
            QuotaColor::Warning,
            QuotaColor::Critical,
            QuotaColor::Unknown,
        ] {
            assert!(ball_is_pulling(
                &with(ConnectionStatus::Online, QuotaPull::Forced),
                color
            ));
        }
        // 自动拉取：只在球上本来没值可显示时转，否则会按刷新间隔周期性闪动。
        assert!(ball_is_pulling(
            &with(ConnectionStatus::Online, QuotaPull::Automatic),
            QuotaColor::Unknown
        ));
        assert!(!ball_is_pulling(
            &with(ConnectionStatus::Online, QuotaPull::Automatic),
            QuotaColor::Healthy
        ));
        assert!(!ball_is_pulling(
            &with(ConnectionStatus::Online, QuotaPull::Automatic),
            QuotaColor::Critical
        ));
    }

    /// 启动阶段（还没到第一次额度 RPC）球上只有 `--`，此刻就该转起来。
    ///
    /// 否则本地日志全量扫描那 2.8 秒里，球是个一动不动的 `--`，看着像卡死；
    /// 面板同一时刻写的是「正在连接 Codex」。
    #[test]
    fn ball_spins_while_starting_up_before_the_first_pull() {
        use crate::quota::QuotaPull;

        let starting = AppState {
            status: ConnectionStatus::Connecting,
            quota_pull: QuotaPull::Idle,
            ..AppState::default()
        };
        assert!(
            ball_is_pulling(&starting, QuotaColor::Unknown),
            "启动阶段球上是 `--`，必须给出在动的反馈"
        );
        // 边界：启动阶段但球上已经有值（正常流程下不会发生）就不转——
        // 旋转弧的含义是"还没东西给你看"，不是"我在忙"。
        assert!(!ball_is_pulling(&starting, QuotaColor::Healthy));
        // 真正开始 RPC 拉取后照旧。
        let pulling = AppState {
            quota_pull: QuotaPull::Automatic,
            ..starting
        };
        assert!(ball_is_pulling(&pulling, QuotaColor::Unknown));
    }

    #[test]
    fn ball_and_panel_withhold_percentages_while_the_snapshot_is_stale() {
        // A snapshot hours old may predate a server-side reset or another
        // device's usage; its percentages must not render at all.
        let state = ball_snapshot(quota_window(0.0, Duration::from_hours(5)), None);
        let now = UNIX_EPOCH + Duration::from_mins(31);

        assert_eq!(
            ball_quota(&state, now),
            ("--".to_owned(), 0.0, QuotaColor::Unknown)
        );
        assert_eq!(display_windows(&state, now), (None, None));
    }

    #[test]
    fn cached_period_estimate_disappears_when_snapshot_becomes_stale() {
        let mut state = ball_snapshot(quota_window(10.0, Duration::from_hours(168)), None);
        state.period_total_value_estimate = Some(100.0);
        assert_eq!(display_period_total_value(&state, UNIX_EPOCH), Some(100.0));
        assert_eq!(
            display_period_total_value(&state, UNIX_EPOCH + Duration::from_mins(31)),
            None,
        );
    }

    #[test]
    fn cached_period_estimate_disappears_when_long_window_expires() {
        let mut long_term = quota_window(10.0, Duration::from_hours(168));
        long_term.resets_at = UNIX_EPOCH + Duration::from_mins(1);
        let mut state = ball_snapshot(quota_window(10.0, Duration::from_hours(5)), Some(long_term));
        state.period_total_value_estimate = Some(100.0);
        assert_eq!(display_period_total_value(&state, UNIX_EPOCH), Some(100.0));
        assert_eq!(
            display_period_total_value(&state, UNIX_EPOCH + Duration::from_mins(1)),
            None,
        );
    }

    #[test]
    fn cached_period_estimate_requires_a_long_window() {
        let mut state = ball_snapshot(quota_window(10.0, Duration::from_hours(5)), None);
        state.period_total_value_estimate = Some(100.0);
        assert_eq!(display_period_total_value(&state, UNIX_EPOCH), None);
        state.snapshot = None;
        assert_eq!(display_period_total_value(&state, UNIX_EPOCH), None);
    }

    /// 满额 token 估算：本期 token ÷ 已用%，与美元估算同源。
    #[test]
    fn period_token_estimate_scales_period_usage_to_a_full_window() {
        let mut state = ball_snapshot(quota_window(10.0, Duration::from_hours(168)), None);
        state.current_period_tokens = Some(3_000_000);

        assert_eq!(
            display_period_total_token_estimate(&state, UNIX_EPOCH),
            Some(30_000_000)
        );

        // 已用不足 1%：放大倍数失控，给不出有意义的估算（与美元列同一道下限）。
        state.snapshot.as_mut().expect("快照").primary.used_percent = 0.5;
        assert_eq!(
            display_period_total_token_estimate(&state, UNIX_EPOCH),
            None
        );
        // 本机本期没有用量（或不可靠）时同样无从放大。
        state.snapshot.as_mut().expect("快照").primary.used_percent = 10.0;
        state.current_period_tokens = Some(0);
        assert_eq!(
            display_period_total_token_estimate(&state, UNIX_EPOCH),
            None
        );
        state.current_period_tokens = None;
        assert_eq!(
            display_period_total_token_estimate(&state, UNIX_EPOCH),
            None
        );
    }

    /// 快照过期后百分比不再可信，放大出来的 token 数同样不可信；没有长期窗口时
    /// 期间行本身就不成立。两道门控与美元列一致。
    #[test]
    fn period_token_estimate_needs_a_fresh_long_window() {
        let mut state = ball_snapshot(quota_window(10.0, Duration::from_hours(168)), None);
        state.current_period_tokens = Some(3_000_000);
        assert!(display_period_total_token_estimate(&state, UNIX_EPOCH).is_some());

        assert_eq!(
            display_period_total_token_estimate(&state, UNIX_EPOCH + Duration::from_mins(31)),
            None
        );

        let mut short_only = ball_snapshot(quota_window(10.0, Duration::from_hours(5)), None);
        short_only.current_period_tokens = Some(3_000_000);
        assert_eq!(
            display_period_total_token_estimate(&short_only, UNIX_EPOCH),
            None
        );
    }

    /// token 列不依赖价格表：美元列算不出来（`--`）时它照样有值。
    #[test]
    fn period_token_estimate_stands_without_a_price_table() {
        let mut state = ball_snapshot(quota_window(10.0, Duration::from_hours(168)), None);
        state.current_period_tokens = Some(3_000_000);
        state.current_period_cost = None;
        state.period_total_value_estimate = None;

        assert_eq!(display_period_total_value(&state, UNIX_EPOCH), None);
        assert_eq!(
            display_period_total_token_estimate(&state, UNIX_EPOCH),
            Some(30_000_000)
        );
    }

    #[test]
    fn window_duration_overrides_plan_fallback_label() {
        let monthly = window(Duration::from_hours(24 * 30));
        assert_eq!(
            quota_window_label(Some(&monthly), Some("plus"), false),
            "月额度"
        );
    }
}
