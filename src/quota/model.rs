use std::time::{Duration, SystemTime};

use crate::config::{ColorStyle, UnitStyle};

/// 菜单未配置/状态锁损坏时的兜底刷新间隔。
pub(crate) const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_mins(5);

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWindow {
    pub used_percent: f64,
    pub window_duration: Duration,
    pub resets_at: SystemTime,
}

impl QuotaWindow {
    #[must_use]
    pub fn remaining_percent(&self) -> f64 {
        (100.0 - self.used_percent).clamp(0.0, 100.0)
    }

    #[must_use]
    pub fn color(&self) -> QuotaColor {
        match self.remaining_percent() {
            value if value >= 50.0 => QuotaColor::Healthy,
            value if value >= 20.0 => QuotaColor::Warning,
            _ => QuotaColor::Critical,
        }
    }

    #[must_use]
    pub fn window_label(&self) -> String {
        format!("{}额度", format_duration_name(self.window_duration))
    }

    /// 不带「额度」后缀的窗口名，用于和「窗口」搭配的句子（通知正文）。
    #[must_use]
    pub fn window_short_label(&self) -> String {
        format_duration_name(self.window_duration)
    }

    /// A window whose reset time has passed was already replaced server-side,
    /// so its remaining percentage is unknowable and must not be displayed.
    #[must_use]
    pub fn has_expired(&self, now: SystemTime) -> bool {
        self.resets_at <= now
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaColor {
    Healthy,
    Warning,
    Critical,
    Unknown,
}

/// 额度拉取的**在途**状态，驱动悬浮球上的不确定进度弧。
///
/// 只有真正在途才算：退避等待不是进度，让它无限旋转就等于又给应用装了一个
/// 永不停止的动效。在途的时长由一次 RPC 决定（有超时兜底），因此这段旋转
/// 天然有界。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuotaPull {
    /// 没有在拉取。
    #[default]
    Idle,
    /// 用户手动刷新（托盘「立即刷新」）：无论球上有没有值都转，这是这次点击
    /// 唯一的反馈，而托盘菜单一关就没有别的动静了。
    Forced,
    /// 自动拉取（快照变旧、窗口重置到期、或启动后的第一次）：只在球上本来
    /// 没值可显示时才转，否则会按刷新间隔周期性闪动。
    Automatic,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaSnapshot {
    pub limit_id: String,
    pub primary: QuotaWindow,
    pub secondary: Option<QuotaWindow>,
    pub received_at: SystemTime,
}

impl QuotaSnapshot {
    #[must_use]
    pub fn quota_windows(&self) -> (Option<&QuotaWindow>, Option<&QuotaWindow>) {
        let primary = &self.primary;
        let Some(secondary) = self.secondary.as_ref() else {
            return if primary.window_duration <= Duration::from_hours(24) {
                (Some(primary), None)
            } else {
                (None, Some(primary))
            };
        };
        if primary.window_duration <= secondary.window_duration {
            (Some(primary), Some(secondary))
        } else {
            (Some(secondary), Some(primary))
        }
    }

    /// Short/long-term windows that have not been replaced by a server-side
    /// reset yet; expired windows carry no usable percentage.
    #[must_use]
    pub fn active_windows(&self, now: SystemTime) -> (Option<&QuotaWindow>, Option<&QuotaWindow>) {
        let (short_term, long_term) = self.quota_windows();
        (
            short_term.filter(|window| !window.has_expired(now)),
            long_term.filter(|window| !window.has_expired(now)),
        )
    }

    /// Whether any active window is exhausted. The account cannot serve
    /// requests while any window sits at 100%, so a window that just rolled
    /// over restored no usable capacity while another one is still full. This
    /// is the one definition of "blocked" shared by the floating ball's zero
    /// reading and by the reset notifications.
    #[must_use]
    pub fn is_blocked(&self, now: SystemTime) -> bool {
        let (short_term, long_term) = self.active_windows(now);
        [short_term, long_term]
            .into_iter()
            .flatten()
            .any(|window| window.remaining_percent() <= 0.0)
    }

    /// Whether any window of this snapshot has been reset server-side, which
    /// makes an on-demand refresh due regardless of the snapshot's age.
    #[must_use]
    pub fn has_expired_window(&self, now: SystemTime) -> bool {
        self.primary.has_expired(now)
            || self
                .secondary
                .as_ref()
                .is_some_and(|window| window.has_expired(now))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting,
    Online,
    Reconnecting { attempt: u32 },
    Error { message: String },
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub status: ConnectionStatus,
    pub snapshot: Option<QuotaSnapshot>,
    /// 是否正在向服务端拉取额度（见 [`QuotaPull`]）。
    pub quota_pull: QuotaPull,
    pub plan_type: Option<String>,
    pub today_tokens: Option<u64>,
    pub current_period_tokens: Option<u64>,
    /// 今日套餐内输入 token 的缓存命中率，单位为 0.1%（0–1000）。
    pub today_cache_hit_percent_tenths: Option<u16>,
    /// 本期套餐内输入 token 的缓存命中率，单位为 0.1%。
    pub current_period_cache_hit_percent_tenths: Option<u16>,
    pub last_error: Option<String>,
    pub quota_refresh_interval: Duration,
    /// 今日已用 token 的 API 牌价等价价值（套餐内，分桶计价；不可靠/无价格表为 None）。
    pub today_cost: Option<f64>,
    /// 本期已用 token 的 API 牌价等价价值（套餐内）。
    pub current_period_cost: Option<f64>,
    /// 本期窗口满额的 API 等价价值估算 = 本期套餐内已用 ÷ 周额度已用%。
    pub period_total_value_estimate: Option<f64>,
    /// 今日本机溢出用量（触顶后事件的实测 token；不可靠为 None）。
    pub today_overflow_tokens: Option<u64>,
    /// 今日 credits 实扣（**账户级原始口径**，积分；不可靠为 None）。
    pub today_overflow_credits: Option<f64>,
    /// 今日 credits 实扣折算美元（账户级，×$0.04/积分；由上一项换算）。
    pub today_overflow_cost: Option<f64>,
    /// 本期本机溢出用量（实测）。
    pub current_period_overflow_tokens: Option<u64>,
    /// 本期 credits 实扣（账户级原始口径，积分）。
    pub current_period_overflow_credits: Option<f64>,
    /// 本期 credits 实扣折算美元（账户级）。
    pub current_period_overflow_cost: Option<f64>,
    /// 重置时间列是否附带紧凑倒计时（配置 → 渲染的载体，与
    /// [`AppState::quota_refresh_interval`] 同样的做法）。
    pub show_reset_countdown: bool,
    /// 套餐内使用行是否显示输入缓存命中率（配置 → 渲染）。
    pub show_cache_hit_rate: bool,
    /// Token 用量的单位风格。
    pub token_unit: UnitStyle,
    /// 额度状态配色的风格（配置 → 渲染的载体，与 [`AppState::token_unit`] 同样的做法）。
    pub color_style: ColorStyle,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            status: ConnectionStatus::Connecting,
            snapshot: None,
            quota_pull: QuotaPull::Idle,
            plan_type: None,
            today_tokens: None,
            current_period_tokens: None,
            today_cache_hit_percent_tenths: None,
            current_period_cache_hit_percent_tenths: None,
            last_error: None,
            quota_refresh_interval: Duration::from_mins(5),
            today_cost: None,
            current_period_cost: None,
            period_total_value_estimate: None,
            today_overflow_tokens: None,
            today_overflow_credits: None,
            today_overflow_cost: None,
            current_period_overflow_tokens: None,
            current_period_overflow_credits: None,
            current_period_overflow_cost: None,
            // 默认与 `AppConfigV1::default()` 一致；真正的来源是配置，这两项只是
            // 在没有配置时的兜底（例如测试与截图路径）。
            show_reset_countdown: true,
            show_cache_hit_rate: true,
            token_unit: UnitStyle::Zh,
            color_style: ColorStyle::Soft,
        }
    }
}

impl AppState {
    /// 显示门控阈值 = 2×刷新间隔。按需拉取在 1×间隔就已触发（见 codex
    /// 侧 `local_pull_threshold`），正常情况下新快照在数据被判过期前到位；
    /// 超过 2×间隔仍无新快照说明拉取持续失败，百分比/本期/估值扣成 --。
    #[must_use]
    pub fn stale_after(&self) -> Duration {
        self.quota_refresh_interval.saturating_mul(2)
    }

    #[must_use]
    pub fn is_stale(&self, now: SystemTime) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            now.duration_since(snapshot.received_at)
                .is_ok_and(|elapsed| elapsed > self.stale_after())
        })
    }
}

#[must_use]
pub fn format_elapsed(then: SystemTime, now: SystemTime) -> String {
    let Ok(elapsed) = now.duration_since(then) else {
        return "刚刚更新".to_owned();
    };

    match elapsed.as_secs() {
        0..=59 => "刚刚更新".to_owned(),
        60..=3_599 => format!("{}分钟前更新", elapsed.as_secs() / 60),
        3_600..=86_399 => format!("{}小时前更新", elapsed.as_secs() / 3_600),
        _ => format!("{}天前更新", elapsed.as_secs() / 86_400),
    }
}

/// 窗口的短名：`5h` / `周` / `月` / `2周` / `3天` / `6小时` / `45分钟`。
/// 面板用的是加了「额度」后缀的 [`QuotaWindow::window_label`]，通知正文里
/// 用的是这个不带后缀的版本（标题已经有「额度」了）。
fn format_duration_name(duration: Duration) -> String {
    let minutes = duration.as_secs() / 60;
    if minutes == 300 {
        "5h".to_owned()
    } else if (40_320..=44_640).contains(&minutes) {
        "月".to_owned()
    } else if minutes == 10_080 {
        "周".to_owned()
    } else if minutes != 0 && minutes.is_multiple_of(10_080) {
        format!("{}周", minutes / 10_080)
    } else if minutes != 0 && minutes.is_multiple_of(1_440) {
        format!("{}天", minutes / 1_440)
    } else if minutes != 0 && minutes.is_multiple_of(60) {
        format!("{}小时", minutes / 60)
    } else {
        format!("{minutes}分钟")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(used_percent: f64) -> QuotaWindow {
        QuotaWindow {
            used_percent,
            window_duration: Duration::from_hours(168),
            resets_at: SystemTime::UNIX_EPOCH + Duration::from_secs(200_000),
        }
    }

    #[test]
    fn remaining_percent_clamps_values_above_one_hundred() {
        assert!(window(130.0).remaining_percent().abs() < f64::EPSILON);
    }

    #[test]
    fn remaining_percent_clamps_negative_usage() {
        assert!((window(-20.0).remaining_percent() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn color_is_warning_at_twenty_percent_remaining() {
        assert_eq!(window(80.0).color(), QuotaColor::Warning);
    }

    #[test]
    fn weekly_window_has_compact_label() {
        assert_eq!(window(10.0).window_label(), "周额度");
    }

    #[test]
    fn thirty_day_window_has_monthly_label() {
        let mut monthly = window(10.0);
        monthly.window_duration = Duration::from_hours(24 * 30);

        assert_eq!(monthly.window_label(), "月额度");
    }

    #[test]
    fn five_hour_window_keeps_compact_label() {
        let mut five_hour = window(10.0);
        five_hour.window_duration = Duration::from_hours(5);

        assert_eq!(five_hour.window_label(), "5h额度");
    }

    #[test]
    fn short_label_drops_the_quota_suffix() {
        let mut five_hour = window(10.0);
        five_hour.window_duration = Duration::from_hours(5);
        let mut monthly = window(10.0);
        monthly.window_duration = Duration::from_hours(24 * 30);
        let mut three_days = window(10.0);
        three_days.window_duration = Duration::from_hours(72);

        assert_eq!(five_hour.window_short_label(), "5h");
        assert_eq!(window(10.0).window_short_label(), "周");
        assert_eq!(monthly.window_short_label(), "月");
        assert_eq!(three_days.window_short_label(), "3天");
    }

    #[test]
    fn stale_threshold_is_twice_the_configured_refresh_interval() {
        let state = AppState {
            quota_refresh_interval: Duration::from_mins(30),
            ..AppState::default()
        };
        assert_eq!(state.stale_after(), Duration::from_hours(1));
    }

    #[test]
    fn stale_threshold_is_twice_the_interval_even_below_the_old_floor() {
        let state = AppState {
            quota_refresh_interval: Duration::from_mins(1),
            ..AppState::default()
        };

        assert_eq!(state.stale_after(), Duration::from_mins(2));
    }

    #[test]
    fn snapshot_stays_fresh_until_the_stale_threshold() {
        // 2N 门控：间隔 1 分钟时，年龄 1 分钟 < 2 分钟，仍未过期。
        let now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let state = AppState {
            snapshot: Some(QuotaSnapshot {
                limit_id: "codex".to_owned(),
                primary: window(10.0),
                secondary: None,
                received_at: now - Duration::from_mins(1),
            }),
            quota_refresh_interval: Duration::from_mins(1),
            ..AppState::default()
        };

        assert!(!state.is_stale(now));
    }

    #[test]
    fn expired_windows_are_filtered_from_active_windows() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(200_000);
        let snapshot = QuotaSnapshot {
            limit_id: "codex".to_owned(),
            primary: window(10.0),
            secondary: Some(QuotaWindow {
                used_percent: 20.0,
                window_duration: Duration::from_hours(168),
                resets_at: now + Duration::from_hours(1),
            }),
            received_at: now - Duration::from_mins(1),
        };

        let (short_term, long_term) = snapshot.active_windows(now);

        assert!(short_term.is_none());
        assert!(long_term.is_some_and(|window| !window.has_expired(now)));
        assert!(snapshot.has_expired_window(now));
    }

    /// 快照带长短两个窗口：`now` 之后的重置时间保证两者都还活跃。
    fn two_window_snapshot(
        short_used: f64,
        long_used: f64,
        long_resets_at: SystemTime,
        now: SystemTime,
    ) -> QuotaSnapshot {
        QuotaSnapshot {
            limit_id: "codex".to_owned(),
            primary: QuotaWindow {
                used_percent: short_used,
                window_duration: Duration::from_hours(5),
                resets_at: now + Duration::from_hours(5),
            },
            secondary: Some(QuotaWindow {
                used_percent: long_used,
                window_duration: Duration::from_hours(168),
                resets_at: long_resets_at,
            }),
            received_at: now,
        }
    }

    #[test]
    fn an_exhausted_active_window_blocks_the_account() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(200_000);
        // 5h 还有 55%：卡住账号的是打满的周窗口，不是短窗口。
        let snapshot = two_window_snapshot(45.0, 100.0, now + Duration::from_hours(72), now);

        assert!(snapshot.is_blocked(now));
    }

    #[test]
    fn remaining_quota_in_both_windows_is_not_blocked() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(200_000);
        let snapshot = two_window_snapshot(10.0, 90.0, now + Duration::from_hours(72), now);

        assert!(!snapshot.is_blocked(now));
    }

    #[test]
    fn a_window_past_its_reset_time_does_not_block() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(200_000);
        // 周窗口的重置时间已过：它已被服务端换掉，百分比不可知，不能据此
        // 认定账号被卡住。
        let snapshot = two_window_snapshot(1.0, 100.0, now - Duration::from_mins(1), now);

        assert!(!snapshot.is_blocked(now));
    }
}
