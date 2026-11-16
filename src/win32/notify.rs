//! 额度通知：判定"额度窗口被重置"与"本周期首次动用余额"，并把结果弹成
//! 托盘通知气泡。
//!
//! 两条规则都遵循同一个前提：**只在程序自己看到的那一下通知**。程序没在
//! 运行（或休眠、跟随模式收起）期间发生的事，事后不补发。

use std::mem::size_of;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    NIF_INFO, NIIF_NONE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};

use super::TRAY_ID;
use super::presentation::format_usd;
use super::tray::copy_wide_fixed;
use crate::error::AppError;
use crate::notify_state::NotifyState;
use crate::quota::{CREDITS_USD_RATE, QuotaSnapshot, QuotaWindow};

/// 余额通知阈值（credits）：低于它的实扣视为账面上的零头抖动，不值得打扰。
/// 0.25 credits = $0.01。credits 是原始口径，阈值也用它。
const OVERFLOW_NOTIFY_CREDITS: f64 = 0.25;
/// "重置时间恰好前移一个窗口长度"的容差。
const RESET_JUMP_TOLERANCE: f64 = 0.10;
/// 提前重置后的少量新用量，以及可区分额度回填与小幅修正的最低降幅。
const RESTORED_MAX_USED: f64 = 1.0;
const RESTORED_MIN_DROP: f64 = 5.0;
/// 观测盲区下限：两次快照间隔超过它就认为中间没在看，不补发。
const MIN_BLIND_GAP: Duration = Duration::from_mins(15);

/// 两条通知的开关（来自配置）。
#[derive(Clone, Copy, Debug)]
pub(super) struct NotificationSwitches {
    pub(super) reset: bool,
    pub(super) overflow: bool,
}

/// 一条待弹出的通知。
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Notification {
    /// 某个额度窗口被服务端换窗（重置）。
    Reset {
        /// 窗口短名（`5h` / `周` / `月`），标题里已有「额度」，正文不再重复。
        window: String,
        remaining_percent: f64,
    },
    /// 本周期首次动用余额（credits 溢出实扣）。
    Overflow {
        /// 账户级 credits 实扣（原始口径）。
        credits: f64,
    },
}

impl Notification {
    /// 通知气泡的标题与正文。
    ///
    /// `period` 是期间前缀（`本周` / `本月`），由调用方从当前长期额度窗口取，
    /// 与面板的「本周使用」等行**同一个来源**——否则面板说周、通知说"本期"，
    /// 两处读的是同一段时间却用两个词。
    pub(super) fn balloon_text(&self, period: &str) -> (&'static str, String) {
        match self {
            Self::Reset {
                window,
                remaining_percent,
            } => (
                "额度已重置",
                format!(
                    "{window}{}窗口已重置，当前剩余 {remaining_percent:.0}%",
                    window_separator(window)
                ),
            ),
            // credits 是日志里的原始口径，精确显示；美元是我们按购买价换算的，
            // 所以标注「约」。
            Self::Overflow { credits } => (
                "已开始动用余额",
                format!(
                    "{period}已扣 {} credits（约 {}）",
                    format_credits(*credits),
                    format_usd(Some(credits * CREDITS_USD_RATE))
                ),
            ),
        }
    }
}

/// 同一次观测里多个窗口一起重置时，只投递一条气泡，保留各窗口的实际余量。
pub(super) fn notification_texts(
    notifications: &[Notification],
    period: &str,
) -> Vec<(&'static str, String)> {
    let reset_bodies: Vec<_> = notifications
        .iter()
        .filter(|notification| matches!(notification, Notification::Reset { .. }))
        .map(|notification| notification.balloon_text(period).1)
        .collect();
    let mut reset_sent = false;
    notifications
        .iter()
        .filter_map(|notification| {
            if matches!(notification, Notification::Reset { .. }) {
                if reset_sent {
                    return None;
                }
                reset_sent = true;
                Some(("额度已重置", reset_bodies.join("；")))
            } else {
                Some(notification.balloon_text(period))
            }
        })
        .collect()
}

/// 窗口名与「窗口」之间的分隔：`5h` 这种拉丁结尾加一个空格才不挤，
/// 纯中文的「周」「月」「3天」不加（中文之间塞空格反而别扭）。
fn window_separator(window: &str) -> &'static str {
    match window.chars().last() {
        Some(last) if last.is_ascii_alphanumeric() => " ",
        _ => "",
    }
}

/// credits 保留两位小数，整数不带小数位。
///
/// 日志里的余额有 10 位小数，那是账本精度、不是给人看的；两位足够分辨单次
/// 扣费（实测最小扣费在 1e-3 credits 量级）。credits 是账户级**原始**口径，
/// 所以精确显示它，美元换算值才标注「约」。
fn format_credits(credits: f64) -> String {
    let rounded = (credits * 100.0).round() / 100.0;
    if rounded.fract().abs() < f64::EPSILON {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.2}")
    }
}

/// 一次观测的结果。
pub(super) struct NotifyOutcome {
    pub(super) notifications: Vec<Notification>,
    /// 持久化状态有变化，需要写回磁盘。
    pub(super) state_dirty: bool,
}

/// 通知判定状态。随 overlay 存活，`reset()` 在释放资源时清空。
#[derive(Debug, Default)]
pub(super) struct Notifier {
    /// 常规换窗的比较基准；各窗口的重置时间只前进，避免迟到的旧周期覆盖它。
    observed: Option<QuotaSnapshot>,
    /// 本地日志不携带账号身份，不能替换提前重置比较用的服务端快照。
    observed_authenticated: Option<QuotaSnapshot>,
    /// 上一次真正观测到的时刻（墙上时钟）。休眠、断网、跟随模式收起都会
    /// 让它与当前时间拉开距离——那是"我们没在看"，不是"刚刚发生"。
    observed_at: Option<SystemTime>,
    /// 本周期是否见过"未达阈值"的实扣（余额通知的起点）。
    ///
    /// 不变式：只能由**够新**且显示未达阈值的观测建立；数据一旦过期、或观测
    /// 中断，立即作废。累计值写在旧快照上的 0 代表不了"现在还没溢出"。
    overflow_armed: bool,
    /// 当前周期键；None 表示账号没有长窗口。
    period: Option<i64>,
    /// 本周期是否已经处理过（弹过，或开关关着时被消费掉）。
    period_handled: bool,
}

impl Notifier {
    /// 释放额度查询资源时调用：丢掉上一次快照，避免下次激活时比出假重置。
    pub(super) fn reset(&mut self) {
        self.observed = None;
        self.observed_authenticated = None;
        self.observed_at = None;
        self.overflow_armed = false;
        self.period = None;
        self.period_handled = false;
    }

    /// 处理一次状态更新，返回需要弹出的通知。`now` 是墙上时钟，用于判断
    /// 距离上次观测过去了多久。
    pub(super) fn observe(
        &mut self,
        state: &mut NotifyState,
        snapshot: Option<&QuotaSnapshot>,
        overflow_credits: Option<f64>,
        switches: NotificationSwitches,
        refresh_interval: Duration,
        now: SystemTime,
    ) -> NotifyOutcome {
        let mut state_dirty = false;

        // 观测中断（休眠、断网拉不到快照、跟随模式收起）：这段时间里发生的
        // 事一律当"没看见"，两条通知都要重新建立起点。
        let interrupted = self.observed_at.is_some_and(|last| {
            now.duration_since(last).unwrap_or_default() > blind_gap_limit(refresh_interval)
        });
        self.observed_at = Some(now);
        if interrupted {
            self.overflow_armed = false;
        }

        let mut notifications =
            self.observe_resets(snapshot, switches.reset, refresh_interval, now, interrupted);

        let period = snapshot.and_then(period_key);
        if period != self.period {
            self.period = period;
            self.overflow_armed = false;
            // 同一个周期已经通知过（跨进程），就不要重来一次。
            self.period_handled =
                period.is_some_and(|key| state.notified_overflow_period == Some(key));
        }
        // 余额实扣是本周期累计值，起点（`overflow_armed`）的不变式是：
        // **必须由够新的、显示未达阈值的观测建立；数据一旦过期立即作废。**
        // 两条都不可省——唤醒后先发布的旧快照写着 0 也不能建起点，而观测
        // 间隔够短、但数据本身很旧时同样要作废，否则恢复新鲜数据后会补发。
        let data_age = snapshot.map_or(Duration::ZERO, |current| {
            now.duration_since(current.received_at).unwrap_or_default()
        });
        let overflow_usable = data_age <= blind_gap_limit(refresh_interval);
        if !overflow_usable {
            // 数据过期：当前值不可知，起点随之作废，等下一份新鲜数据重建。
            // 这里不碰 period_handled，也不写盘——过期不等于"事件已处理"。
            self.overflow_armed = false;
        } else if let Some(credits) = overflow_credits {
            if credits < OVERFLOW_NOTIFY_CREDITS {
                self.overflow_armed = true;
            } else if period.is_some() && self.overflow_armed && !self.period_handled {
                if switches.overflow {
                    notifications.push(Notification::Overflow { credits });
                }
                // 开关关着时也记为已处理：之后打开开关不补弹过去的事件。
                self.period_handled = true;
                state.notified_overflow_period = period;
                state_dirty = true;
            }
        }

        NotifyOutcome {
            notifications,
            state_dirty,
        }
    }

    fn observe_resets(
        &mut self,
        snapshot: Option<&QuotaSnapshot>,
        enabled: bool,
        refresh_interval: Duration,
        now: SystemTime,
        interrupted: bool,
    ) -> Vec<Notification> {
        let Some(next) = snapshot else {
            return Vec::new();
        };
        let limit = blind_gap_limit(refresh_interval);
        let mut windows = Vec::new();
        // 另一个窗口仍触顶时，换窗没有恢复可用容量，不发送重置通知。
        if enabled && !interrupted && !next.is_blocked(now) {
            if let Some(previous) = self.observed.as_ref()
                && continuous_snapshots(previous, next, limit, now)
                && !matches!(
                    (&previous.account_id, &next.account_id),
                    (Some(previous), Some(next)) if previous != next
                )
            {
                windows = reset_windows(previous, next);
            }
            if let Some(previous) = self.observed_authenticated.as_ref()
                && next.account_id.is_some()
                && previous.account_id == next.account_id
                && continuous_snapshots(previous, next, limit, now)
            {
                for window in restored_windows(previous, next, now) {
                    if !windows.contains(&window) {
                        windows.push(window);
                    }
                }
            }
        }
        let notifications = windows
            .into_iter()
            .map(|window| Notification::Reset {
                window: window.window_short_label(),
                remaining_percent: window.remaining_percent(),
            })
            .collect();
        if self
            .observed
            .as_ref()
            .is_none_or(|previous| next.received_at >= previous.received_at)
        {
            self.observed = Some(reset_baseline(self.observed.as_ref(), next));
        }
        // 重复发布、旧快照和未知账号都不覆盖这个比较起点。
        if next.account_id.is_some()
            && self
                .observed_authenticated
                .as_ref()
                .is_none_or(|previous| next.received_at > previous.received_at)
        {
            self.observed_authenticated = Some(next.clone());
        }
        notifications
    }
}

/// 日志写入时间可能晚于 RPC，但仍携带旧周期。只为常规换窗保留各窗口见过的
/// 最晚边界；提前重置仍比较未经合并的服务端快照，显示数据也不受影响。
fn reset_baseline(previous: Option<&QuotaSnapshot>, next: &QuotaSnapshot) -> QuotaSnapshot {
    let mut baseline = next.clone();
    let Some(previous) = previous.filter(|previous| {
        previous.limit_id == next.limit_id
            && !matches!(
                (&previous.account_id, &next.account_id),
                (Some(previous), Some(next)) if previous != next
            )
    }) else {
        return baseline;
    };
    for window in std::iter::once(&mut baseline.primary).chain(baseline.secondary.iter_mut()) {
        if let Some(previous) = std::iter::once(&previous.primary)
            .chain(previous.secondary.iter())
            .find(|previous| previous.window_duration == window.window_duration)
        {
            window.resets_at = window.resets_at.max(previous.resets_at);
        }
    }
    // 未知身份的本地日志不抹去已知账号，后续确认切号时才能重建比较基准。
    if baseline.account_id.is_none() {
        baseline.account_id.clone_from(&previous.account_id);
    }
    baseline
}

fn continuous_snapshots(
    previous: &QuotaSnapshot,
    next: &QuotaSnapshot,
    limit: Duration,
    now: SystemTime,
) -> bool {
    previous.limit_id == next.limit_id
        && next.received_at > previous.received_at
        && next
            .received_at
            .duration_since(previous.received_at)
            .is_ok_and(|gap| gap <= limit)
        && now
            .duration_since(next.received_at)
            .is_ok_and(|age| age <= limit)
}

fn restored_windows<'a>(
    previous: &QuotaSnapshot,
    next: &'a QuotaSnapshot,
    now: SystemTime,
) -> Vec<&'a QuotaWindow> {
    let (previous_short, previous_long) = previous.quota_windows();
    let (next_short, next_long) = next.quota_windows();
    [(previous_short, next_short), (previous_long, next_long)]
        .into_iter()
        .filter_map(|(previous, next)| {
            let (previous, next) = (previous?, next?);
            (previous.window_duration == next.window_duration
                && !window_reset(previous, next)
                && !next.has_expired(now)
                && (0.0..=100.0).contains(&previous.used_percent)
                && (0.0..=RESTORED_MAX_USED).contains(&next.used_percent)
                && previous.used_percent - next.used_percent >= RESTORED_MIN_DROP
                && next.resets_at >= previous.resets_at)
                .then_some(next)
        })
        .collect()
}

/// 盲区上限：至少 15 分钟，同时要放得下用户设置的刷新间隔，否则按时长
/// 刷新本身就会被误判成盲区。
fn blind_gap_limit(refresh_interval: Duration) -> Duration {
    MIN_BLIND_GAP.max(refresh_interval.saturating_mul(2))
}

/// 前后两次快照里发生了服务端换窗的窗口（按短/长槽位配对）。
fn reset_windows<'a>(previous: &'a QuotaSnapshot, next: &'a QuotaSnapshot) -> Vec<&'a QuotaWindow> {
    let (previous_short, previous_long) = previous.quota_windows();
    let (next_short, next_long) = next.quota_windows();
    [(previous_short, next_short), (previous_long, next_long)]
        .into_iter()
        .filter_map(|(previous, next)| {
            let (previous, next) = (previous?, next?);
            window_reset(previous, next).then_some(next)
        })
        .collect()
}

/// 服务端是否把这个窗口换成了下一个周期：重置时间恰好前移一个窗口长度。
///
/// 这是常规换窗的证据；百分比回落的提前重置另由同账号服务端快照判断。
/// 窗口长度变化不在此处视为重置。
fn window_reset(previous: &QuotaWindow, next: &QuotaWindow) -> bool {
    if previous.window_duration != next.window_duration {
        return false;
    }
    let Some(expected) = previous.resets_at.checked_add(previous.window_duration) else {
        return false;
    };
    // duration_since 在反向时返回错误，其 duration() 就是绝对差。
    let drift = next
        .resets_at
        .duration_since(expected)
        .unwrap_or_else(|error| error.duration());
    drift <= previous.window_duration.mul_f64(RESET_JUMP_TOLERANCE)
}

/// 周期键：长窗口（周/月）的重置时间，UNIX 秒；没有长窗口时为 None。
fn period_key(snapshot: &QuotaSnapshot) -> Option<i64> {
    let (_, long_term) = snapshot.quota_windows();
    let seconds = long_term?
        .resets_at
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    i64::try_from(seconds).ok()
}

/// 调试构建专供：把"测试通知"菜单命令映射成一条有代表性的通知。
///
/// 只走投递路径（气泡、声音、点击），**不经过判定状态机**，所以随便点也
/// 不会污染通知起点或去重记录。发布构建里这套菜单项与函数都不存在。
#[cfg(debug_assertions)]
pub(super) fn test_notification_for_command(command: usize) -> Option<Notification> {
    match command {
        super::CMD_TEST_NOTIFY_RESET => Some(Notification::Reset {
            window: "5h".to_owned(),
            remaining_percent: 99.0,
        }),
        super::CMD_TEST_NOTIFY_BALANCE => Some(Notification::Overflow { credits: 310.25 }),
        _ => None,
    }
}

/// 弹一条托盘通知气泡；点击气泡会以 `NIN_BALLOONUSERCLICK` 回到托盘回调。
///
/// 正文不放图标：`NIIF_NONE` 的语义就是"无图标"。不传它的话 Windows 会按
/// `dwInfoFlags` 塞一个系统图标（信息/警告），跟本应用无关。
pub(super) fn show_balloon(hwnd: HWND, title: &str, body: &str) -> Result<(), AppError> {
    let mut data = NOTIFYICONDATAW {
        cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>())
            .map_err(|_| AppError::Windows("托盘结构大小溢出".to_owned()))?,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_INFO,
        dwInfoFlags: NIIF_NONE,
        ..Default::default()
    };
    copy_wide_fixed(title, &mut data.szInfoTitle);
    copy_wide_fixed(body, &mut data.szInfo);
    // SAFETY: data is fully initialized and the tray icon identified by HWND and ID exists.
    if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
        return Err(AppError::Windows("无法显示托盘通知".to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_hours(1);
    const FIVE_HOURS: Duration = Duration::from_hours(5);
    const WEEK: Duration = Duration::from_hours(24 * 7);

    fn base() -> SystemTime {
        UNIX_EPOCH + Duration::from_hours(500_000)
    }

    fn window(used_percent: f64, duration: Duration, resets_at: SystemTime) -> QuotaWindow {
        QuotaWindow {
            used_percent,
            window_duration: duration,
            resets_at,
        }
    }

    fn snapshot(
        short: QuotaWindow,
        long: Option<QuotaWindow>,
        received_at: SystemTime,
    ) -> QuotaSnapshot {
        QuotaSnapshot {
            account_id: None,
            limit_id: "codex".to_owned(),
            primary: short,
            secondary: long,
            received_at,
        }
    }

    /// 长窗口重置时间固定为 base()+WEEK，便于构造周期键。
    fn weekly(resets_at: SystemTime) -> QuotaWindow {
        window(0.0, WEEK, resets_at)
    }

    fn switches() -> NotificationSwitches {
        NotificationSwitches {
            reset: true,
            overflow: true,
        }
    }

    fn observe(
        notifier: &mut Notifier,
        state: &mut NotifyState,
        snapshot: &QuotaSnapshot,
        credits: Option<f64>,
    ) -> Vec<Notification> {
        observe_with(notifier, state, snapshot, credits, switches())
    }

    /// 测试里把快照的时间戳同时当作观测时刻，两者保持一致。
    fn observe_with(
        notifier: &mut Notifier,
        state: &mut NotifyState,
        snapshot: &QuotaSnapshot,
        credits: Option<f64>,
        switches: NotificationSwitches,
    ) -> Vec<Notification> {
        notifier
            .observe(
                state,
                Some(snapshot),
                credits,
                switches,
                Duration::from_mins(5),
                snapshot.received_at,
            )
            .notifications
    }

    /// 观测时刻与快照时间戳解耦：唤醒时"旧快照现在才被发布"靠它复现。
    fn observe_at(
        notifier: &mut Notifier,
        state: &mut NotifyState,
        snapshot: &QuotaSnapshot,
        credits: Option<f64>,
        now: SystemTime,
    ) -> Vec<Notification> {
        notifier
            .observe(
                state,
                Some(snapshot),
                credits,
                switches(),
                Duration::from_mins(5),
                now,
            )
            .notifications
    }

    #[test]
    fn unified_reset_notifies_once() {
        // 2026-10-15 本机真实观测：剩余 92% / 48% 同时恢复 100%，
        // 两个重置时间均未推进完整周期。
        let mut previous = snapshot(
            window(
                8.0,
                FIVE_HOURS,
                UNIX_EPOCH + Duration::from_secs(1_792_061_393),
            ),
            Some(window(
                52.0,
                WEEK,
                UNIX_EPOCH + Duration::from_secs(1_792_472_647),
            )),
            UNIX_EPOCH + Duration::from_secs(1_792_044_682),
        );
        previous.account_id = Some("account-a".to_owned());
        let mut next = snapshot(
            window(
                0.0,
                FIVE_HOURS,
                UNIX_EPOCH + Duration::from_secs(1_792_063_248),
            ),
            Some(window(
                0.0,
                WEEK,
                UNIX_EPOCH + Duration::from_secs(1_792_650_048),
            )),
            UNIX_EPOCH + Duration::from_secs(1_792_045_256),
        );
        next.account_id.clone_from(&previous.account_id);
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        assert!(observe(&mut notifier, &mut state, &previous, Some(0.0)).is_empty());

        // 本地日志先到，不可借用上一个服务端查询的身份直接通知。
        let mut local = next.clone();
        local.account_id = None;
        assert!(observe(&mut notifier, &mut state, &local, Some(0.0)).is_empty());
        let notifications = observe(&mut notifier, &mut state, &next, Some(0.0));
        assert_eq!(
            notifications,
            vec![
                Notification::Reset {
                    window: "5h".to_owned(),
                    remaining_percent: 100.0
                },
                Notification::Reset {
                    window: "周".to_owned(),
                    remaining_percent: 100.0
                },
            ]
        );
        assert_eq!(
            notification_texts(&notifications, "本周"),
            vec![(
                "额度已重置",
                "5h 窗口已重置，当前剩余 100%；周窗口已重置，当前剩余 100%".to_owned(),
            )]
        );
        assert!(observe(&mut notifier, &mut state, &next, Some(0.0)).is_empty());
        // 再次刷新、以及晚到的旧快照，都不能重复触发。
        next.received_at += Duration::from_mins(2);
        assert!(observe(&mut notifier, &mut state, &next, Some(0.0)).is_empty());
        assert!(observe(&mut notifier, &mut state, &previous, Some(0.0)).is_empty());
        next.received_at += Duration::from_mins(2);
        assert!(observe(&mut notifier, &mut state, &next, Some(0.0)).is_empty());
    }

    #[test]
    fn local_rollover_prevents_rpc_duplicate() {
        let start = base();
        let mut previous = snapshot(
            window(80.0, FIVE_HOURS, start + Duration::from_mins(5)),
            None,
            start,
        );
        previous.account_id = Some("account-a".to_owned());
        let mut next = previous.clone();
        next.primary.used_percent = 0.0;
        next.primary.resets_at += FIVE_HOURS;
        next.received_at += Duration::from_mins(6);
        next.account_id = None;
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        observe(&mut notifier, &mut state, &previous, Some(0.0));
        assert_eq!(
            observe(&mut notifier, &mut state, &next, Some(0.0)).len(),
            1
        );
        next.account_id.clone_from(&previous.account_id);
        next.received_at += Duration::from_secs(1);
        assert!(observe(&mut notifier, &mut state, &next, Some(0.0)).is_empty());
    }

    #[test]
    fn late_old_window_cannot_repeat_reset() {
        // 2026-10-29：13:34:40 RPC 已见新周期，13:34:43 的日志仍带旧周期，
        // 13:34:45 RPC 再见同一新周期；日志时间更新不代表额度周期更新。
        for (authenticated, boundary_drift) in [(false, 0), (false, 2), (true, 0)] {
            let reset = UNIX_EPOCH + Duration::from_secs(1_793_252_078);
            let mut previous = snapshot(
                window(9.0, FIVE_HOURS, reset),
                Some(window(
                    10.0,
                    WEEK,
                    UNIX_EPOCH + Duration::from_secs(1_793_776_477),
                )),
                reset - Duration::from_secs(8),
            );
            previous.account_id = Some("account-a".to_owned());
            let mut next = previous.clone();
            next.received_at = reset + Duration::from_secs(2);
            next.primary = window(0.0, FIVE_HOURS, reset + FIVE_HOURS + Duration::from_secs(5));
            let mut notifier = Notifier::default();
            let mut state = NotifyState::default();
            observe(&mut notifier, &mut state, &previous, None);
            assert_eq!(observe(&mut notifier, &mut state, &next, None).len(), 1);

            let mut late = previous.clone();
            late.received_at = reset + Duration::from_secs(5);
            late.primary.used_percent = 10.0;
            if !authenticated {
                late.account_id = None;
            }
            assert!(observe(&mut notifier, &mut state, &late, None).is_empty());
            next.received_at = reset + Duration::from_secs(7);
            next.primary.resets_at += Duration::from_secs(boundary_drift);
            assert!(
                observe(&mut notifier, &mut state, &next, None).is_empty(),
                "authenticated={authenticated}, boundary_drift={boundary_drift}"
            );

            // 下一次真正换窗仍正常通知。
            next.received_at = next.primary.resets_at - Duration::from_secs(1);
            assert!(observe(&mut notifier, &mut state, &next, None).is_empty());
            next.received_at += Duration::from_secs(2);
            next.primary.resets_at += FIVE_HOURS;
            assert_eq!(observe(&mut notifier, &mut state, &next, None).len(), 1);
        }
    }

    #[test]
    fn reset_baseline_follows_scope_changes() {
        for scope in ["account", "bucket", "duration"] {
            let start = base();
            let mut previous = snapshot(window(80.0, FIVE_HOURS, start + FIVE_HOURS), None, start);
            previous.account_id = Some("account-a".to_owned());
            let mut notifier = Notifier::default();
            let mut state = NotifyState::default();
            observe(&mut notifier, &mut state, &previous, None);
            let mut local = previous.clone();
            local.account_id = None;
            local.received_at += Duration::from_secs(1);
            observe(&mut notifier, &mut state, &local, None);

            let mut next = previous.clone();
            next.received_at += Duration::from_secs(2);
            next.primary.resets_at = start + Duration::from_secs(5);
            match scope {
                "account" => next.account_id = Some("account-b".to_owned()),
                "bucket" => next.limit_id = "other".to_owned(),
                "duration" => next.primary.window_duration = HOUR,
                _ => unreachable!(),
            }
            assert!(observe(&mut notifier, &mut state, &next, None).is_empty());
            next.received_at += Duration::from_secs(4);
            next.primary.resets_at += next.primary.window_duration;
            next.primary.used_percent = 0.0;
            assert_eq!(
                observe(&mut notifier, &mut state, &next, None).len(),
                1,
                "{scope}"
            );
        }
    }

    #[test]
    fn regressed_short_preserves_weekly_reset() {
        let start = base();
        let mut previous = snapshot(
            window(80.0, FIVE_HOURS, start + Duration::from_secs(5)),
            Some(window(80.0, WEEK, start + Duration::from_secs(10))),
            start,
        );
        previous.account_id = Some("account-a".to_owned());
        let mut next = previous.clone();
        next.received_at += Duration::from_secs(6);
        next.primary.used_percent = 0.0;
        next.primary.resets_at += FIVE_HOURS;
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        observe(&mut notifier, &mut state, &previous, None);
        assert_eq!(observe(&mut notifier, &mut state, &next, None).len(), 1);

        let mut late = previous.clone();
        late.received_at += Duration::from_secs(11);
        late.account_id = None;
        late.secondary.as_mut().unwrap().used_percent = 0.0;
        late.secondary.as_mut().unwrap().resets_at += WEEK;
        assert_eq!(
            observe(&mut notifier, &mut state, &late, None),
            vec![Notification::Reset {
                window: "周".to_owned(),
                remaining_percent: 100.0,
            }]
        );
        next.received_at += Duration::from_secs(6);
        next.secondary = late.secondary;
        assert!(observe(&mut notifier, &mut state, &next, None).is_empty());
    }

    #[test]
    fn early_reset_accepts_small_usage() {
        for (used, moved_boundary) in [(0.0, false), (1.0, false), (1.0, true)] {
            let start = base();
            let mut previous = snapshot(window(40.0, FIVE_HOURS, start + HOUR * 3), None, start);
            previous.account_id = Some("account-a".to_owned());
            let mut next = previous.clone();
            next.received_at += Duration::from_mins(5);
            next.primary.used_percent = used;
            if moved_boundary {
                next.primary.resets_at = next.received_at + FIVE_HOURS;
            }
            let mut notifier = Notifier::default();
            let mut state = NotifyState::default();
            observe(&mut notifier, &mut state, &previous, Some(0.0));
            let notifications = observe(&mut notifier, &mut state, &next, Some(0.0));
            assert_eq!(
                notifications,
                vec![Notification::Reset {
                    window: "5h".to_owned(),
                    remaining_percent: 100.0 - used,
                }],
                "used={used} moved_boundary={moved_boundary}"
            );
        }
    }

    #[test]
    fn early_reset_rejects_uncertain_changes() {
        for label in [
            "different-account",
            "missing-before-account",
            "missing-after-account",
            "small-drop",
            "not-full",
            "changed-duration",
            "reversed-boundary",
            "blind-gap",
            "stale-data",
            "expired",
            "blocked",
            "bucket-changed",
            "disabled",
        ] {
            let start = base();
            let mut previous = snapshot(window(40.0, FIVE_HOURS, start + HOUR * 3), None, start);
            previous.account_id = Some("account-a".to_owned());
            let mut next = previous.clone();
            next.received_at += Duration::from_mins(5);
            next.primary.used_percent = 0.0;
            next.primary.resets_at = next.received_at + FIVE_HOURS;
            let mut now = next.received_at;
            let mut switches = switches();
            match label {
                "different-account" => next.account_id = Some("account-b".to_owned()),
                "missing-before-account" => previous.account_id = None,
                "missing-after-account" => next.account_id = None,
                "small-drop" => previous.primary.used_percent = 4.0,
                "not-full" => next.primary.used_percent = 2.0,
                "changed-duration" => next.primary.window_duration = HOUR * 10,
                "reversed-boundary" => next.primary.resets_at = start + HOUR * 2,
                "blind-gap" => {
                    next.received_at += HOUR;
                    now = next.received_at;
                }
                "stale-data" => now += HOUR,
                "expired" => next.primary.resets_at = start,
                "blocked" => next.secondary = Some(window(100.0, WEEK, start + WEEK)),
                "bucket-changed" => next.limit_id = "other".to_owned(),
                "disabled" => switches.reset = false,
                _ => unreachable!(),
            }
            let mut notifier = Notifier::default();
            let mut state = NotifyState::default();
            observe(&mut notifier, &mut state, &previous, Some(0.0));
            let result = notifier.observe(
                &mut state,
                Some(&next),
                Some(0.0),
                switches,
                Duration::from_mins(5),
                now,
            );
            assert!(
                result.notifications.is_empty(),
                "{label}: {:?}",
                result.notifications
            );
            // 关闭开关时也消费这次恢复，之后打开不补发。
            next.received_at = now + Duration::from_mins(1);
            assert!(
                observe(&mut notifier, &mut state, &next, Some(0.0)).is_empty(),
                "{label}"
            );
        }
    }

    #[test]
    fn merged_balloon_preserves_each_window() {
        let notifications = vec![
            Notification::Reset {
                window: "5h".to_owned(),
                remaining_percent: 99.0,
            },
            Notification::Reset {
                window: "周".to_owned(),
                remaining_percent: 100.0,
            },
            Notification::Overflow { credits: 1.0 },
        ];
        let texts = notification_texts(&notifications, "本周");
        assert_eq!(texts.len(), 2);
        assert_eq!(
            texts[0],
            (
                "额度已重置",
                "5h 窗口已重置，当前剩余 99%；周窗口已重置，当前剩余 100%".to_owned()
            )
        );
        assert_eq!(texts[1], notifications[2].balloon_text("本周"));
    }

    #[test]
    fn five_hour_rollover_notifies() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let weekly_reset = start + WEEK;
        // 重置发生在 start+5h，我们 5 分钟后看到新窗口——观测间隔远小于盲区上限。
        let noticed = start + Duration::from_mins(5);

        observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(80.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(weekly(weekly_reset)),
                start,
            ),
            Some(0.0),
        );
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(1.0, FIVE_HOURS, start + FIVE_HOURS * 2),
                Some(weekly(weekly_reset)),
                noticed,
            ),
            Some(0.0),
        );

        assert_eq!(
            notifications,
            vec![Notification::Reset {
                window: "5h".to_owned(),
                remaining_percent: 99.0,
            }]
        );
    }

    #[test]
    fn weekly_rollover_notifies() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let noticed = start + Duration::from_mins(5);

        observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(10.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(window(90.0, WEEK, start + WEEK)),
                start,
            ),
            Some(0.0),
        );
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(10.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(window(0.0, WEEK, start + WEEK * 2)),
                noticed,
            ),
            Some(0.0),
        );

        assert_eq!(
            notifications,
            vec![Notification::Reset {
                window: "周".to_owned(),
                remaining_percent: 100.0,
            }]
        );
    }

    #[test]
    fn five_hour_rollover_respects_weekly_exhaustion() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let weekly_reset = start + WEEK;

        observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(45.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(window(100.0, WEEK, weekly_reset)),
                start,
            ),
            Some(0.0),
        );
        // 周额度打满，5h 窗口照常换窗：5h 的时钟确实重开了，但账号依旧发不出
        // 请求，这条"剩余 100%"帮不上任何忙。
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(0.0, FIVE_HOURS, start + FIVE_HOURS * 2),
                Some(window(100.0, WEEK, weekly_reset)),
                start + Duration::from_mins(5),
            ),
            Some(0.0),
        );

        assert!(
            notifications.is_empty(),
            "周额度打满时 5h 换窗不提醒：{notifications:?}"
        );
    }

    #[test]
    fn restored_capacity_triggers_notification() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let weekly_reset = start + WEEK;
        let short = || window(0.0, FIVE_HOURS, start + FIVE_HOURS * 2);

        observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(45.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(window(100.0, WEEK, weekly_reset)),
                start,
            ),
            Some(0.0),
        );
        // 周额度打满期间的 5h 换窗保持静默。
        assert!(
            observe(
                &mut notifier,
                &mut state,
                &snapshot(
                    short(),
                    Some(window(100.0, WEEK, weekly_reset)),
                    start + Duration::from_mins(5),
                ),
                Some(0.0),
            )
            .is_empty()
        );

        // 周窗口自己换窗：账号真正恢复可用，这次必须提醒。
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                short(),
                Some(window(0.0, WEEK, weekly_reset + WEEK)),
                start + Duration::from_mins(10),
            ),
            Some(0.0),
        );

        assert_eq!(
            notifications,
            vec![Notification::Reset {
                window: "周".to_owned(),
                remaining_percent: 100.0,
            }],
            "恢复可用的那次换窗不得被抑制"
        );
    }

    #[test]
    fn weekly_rollover_respects_short_exhaustion() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        // 周窗口在 start+5min 换窗，两次观测相隔 10 分钟——远小于盲区上限，
        // 排除"中断不补发"这条规则的干扰。
        let weekly_reset = start + Duration::from_mins(5);
        let short = window(100.0, FIVE_HOURS, start + FIVE_HOURS);

        observe(
            &mut notifier,
            &mut state,
            &snapshot(short.clone(), Some(window(90.0, WEEK, weekly_reset)), start),
            Some(0.0),
        );
        // 周额度回来了，5h 却还打满：账号仍被卡住，同样不值得打扰。
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                short,
                Some(window(0.0, WEEK, weekly_reset + WEEK)),
                start + Duration::from_mins(10),
            ),
            Some(0.0),
        );
        assert!(
            notifications.is_empty(),
            "5h 打满时周换窗不提醒：{notifications:?}"
        );

        // 5h 随后换窗，两个窗口都可用：这才是"可以干活了"。
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(0.0, FIVE_HOURS, start + FIVE_HOURS * 2),
                Some(window(0.0, WEEK, weekly_reset + WEEK)),
                start + Duration::from_mins(15),
            ),
            Some(0.0),
        );

        assert_eq!(
            notifications,
            vec![Notification::Reset {
                window: "5h".to_owned(),
                remaining_percent: 100.0,
            }]
        );
    }

    #[test]
    fn simultaneous_rollovers_notify_both() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();

        observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(45.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(window(100.0, WEEK, start + WEEK)),
                start,
            ),
            Some(0.0),
        );
        // 同一帧里两个窗口都换：账号完全恢复，两条都要弹。
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(0.0, FIVE_HOURS, start + FIVE_HOURS * 2),
                Some(window(0.0, WEEK, start + WEEK * 2)),
                start + Duration::from_mins(5),
            ),
            Some(0.0),
        );

        assert_eq!(
            notifications,
            vec![
                Notification::Reset {
                    window: "5h".to_owned(),
                    remaining_percent: 100.0,
                },
                Notification::Reset {
                    window: "周".to_owned(),
                    remaining_percent: 100.0,
                },
            ]
        );
    }

    #[test]
    fn single_window_rollover_notifies() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();

        // 只有 5h 的账号没有"另一个窗口打满"这回事。
        observe(
            &mut notifier,
            &mut state,
            &snapshot(window(80.0, FIVE_HOURS, start + FIVE_HOURS), None, start),
            Some(0.0),
        );
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(1.0, FIVE_HOURS, start + FIVE_HOURS * 2),
                None,
                start + Duration::from_mins(5),
            ),
            Some(0.0),
        );

        assert_eq!(
            notifications,
            vec![Notification::Reset {
                window: "5h".to_owned(),
                remaining_percent: 99.0,
            }]
        );
    }

    #[test]
    fn expired_blocker_allows_reset_notification() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        // 周窗口的重置时间已过：服务端早已把它换掉了，百分比不可知，不能再拿
        // 它当"账号还卡着"的依据。
        let stale_weekly = window(100.0, WEEK, start + Duration::from_mins(3));

        observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(45.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(stale_weekly.clone()),
                start,
            ),
            Some(0.0),
        );
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(0.0, FIVE_HOURS, start + FIVE_HOURS * 2),
                Some(stale_weekly),
                start + Duration::from_mins(8),
            ),
            Some(0.0),
        );

        assert_eq!(
            notifications,
            vec![Notification::Reset {
                window: "5h".to_owned(),
                remaining_percent: 100.0,
            }]
        );
    }

    #[test]
    fn account_switch_without_reset_stays_silent() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();

        observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(80.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(weekly(start + WEEK)),
                start,
            ),
            Some(0.0),
        );
        // 百分比暴跌，但重置时间没动：切账号，不是重置。
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(3.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(weekly(start + WEEK)),
                start + HOUR,
            ),
            Some(0.0),
        );

        assert!(
            notifications.is_empty(),
            "切账号不得当成重置：{notifications:?}"
        );
    }

    #[test]
    fn duration_change_is_not_reset() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();

        observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(80.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(weekly(start + WEEK)),
                start,
            ),
            Some(0.0),
        );
        // 窗口长度从 5h 变 10h：换套餐，不是重置。
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(1.0, Duration::from_hours(10), start + FIVE_HOURS * 2),
                Some(weekly(start + WEEK)),
                start + FIVE_HOURS,
            ),
            Some(0.0),
        );

        assert!(
            notifications.is_empty(),
            "窗口长度变了不算重置：{notifications:?}"
        );
    }

    #[test]
    fn blind_gap_suppresses_backfill() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();

        observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(100.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(weekly(start + WEEK)),
                start,
            ),
            Some(0.0),
        );
        // 3 小时没观测（休眠/关机）：中间跨过重置点也不补发。
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(1.0, FIVE_HOURS, start + FIVE_HOURS * 2),
                Some(weekly(start + WEEK)),
                start + Duration::from_hours(3),
            ),
            Some(0.0),
        );

        assert!(
            notifications.is_empty(),
            "盲区跨过的重置不补发：{notifications:?}"
        );
    }

    #[test]
    fn the_first_observation_never_notifies() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();

        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(1.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(weekly(start + WEEK)),
                start,
            ),
            Some(0.0),
        );

        assert!(notifications.is_empty());
    }

    #[test]
    fn long_refresh_interval_observes_reset() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let long_interval = Duration::from_mins(30);

        notifier.observe(
            &mut state,
            Some(&snapshot(
                window(80.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(weekly(start + WEEK)),
                start,
            )),
            Some(0.0),
            switches(),
            long_interval,
            start,
        );
        let noticed = start + Duration::from_mins(50);
        let outcome = notifier.observe(
            &mut state,
            Some(&snapshot(
                window(1.0, FIVE_HOURS, start + FIVE_HOURS * 2),
                Some(weekly(start + WEEK)),
                noticed,
            )),
            Some(0.0),
            switches(),
            long_interval,
            noticed,
        );

        assert_eq!(outcome.notifications.len(), 1, "盲区上限要放得下刷新间隔");
    }

    #[test]
    fn unreliable_overflow_preserves_notification_state() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let first = snapshot(
            window(10.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            start,
        );
        let second = snapshot(
            window(9.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            start + Duration::from_mins(1),
        );

        observe(&mut notifier, &mut state, &first, Some(0.0));
        // 数据不可靠：不弹，也不能把本期的机会吃掉。
        assert!(observe(&mut notifier, &mut state, &second, None).is_empty());
        let notifications = observe(&mut notifier, &mut state, &second, Some(1.0));

        assert_eq!(
            notifications,
            vec![Notification::Overflow { credits: 1.0 }],
            "不可靠观测之后仍应能通知"
        );
    }

    #[test]
    fn overflow_notifies_once_per_period() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let period = Some(weekly(start + WEEK));
        let first = snapshot(
            window(10.0, FIVE_HOURS, start + FIVE_HOURS),
            period.clone(),
            start,
        );
        let second = snapshot(
            window(9.0, FIVE_HOURS, start + FIVE_HOURS),
            period,
            start + Duration::from_mins(1),
        );

        observe(&mut notifier, &mut state, &first, Some(0.0));
        assert_eq!(
            observe(&mut notifier, &mut state, &second, Some(1.5)).len(),
            1
        );
        assert!(
            observe(&mut notifier, &mut state, &second, Some(5.0)).is_empty(),
            "同一周期只通知一次"
        );
    }

    #[test]
    fn overflow_at_startup_is_not_backfilled() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let snapshot = snapshot(
            window(10.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            start,
        );

        // 启动时本期已经在扣余额：没先见过"未溢出"，不补发。
        let notifications = observe(&mut notifier, &mut state, &snapshot, Some(5.0));

        assert!(
            notifications.is_empty(),
            "启动即已溢出不得补发：{notifications:?}"
        );
    }

    #[test]
    fn overflow_rearms_in_a_new_period() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let first = snapshot(
            window(10.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            start,
        );
        let second = snapshot(
            window(9.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            start + Duration::from_mins(1),
        );
        let next_period = snapshot(
            window(0.0, FIVE_HOURS, start + FIVE_HOURS * 2),
            Some(weekly(start + WEEK * 2)),
            start + Duration::from_mins(2),
        );

        observe(&mut notifier, &mut state, &first, Some(0.0));
        assert_eq!(
            observe(&mut notifier, &mut state, &second, Some(1.5)).len(),
            1
        );

        // 新周期：余额通知重新从"未溢出"开始（这一帧只有两个窗口的重置通知）。
        let rolled = observe(&mut notifier, &mut state, &next_period, Some(0.0));
        assert!(
            !rolled.is_empty()
                && rolled
                    .iter()
                    .all(|item| matches!(item, Notification::Reset { .. })),
            "换周期这一帧不应有余额通知：{rolled:?}"
        );

        let notifications = observe(&mut notifier, &mut state, &next_period, Some(2.5));

        assert_eq!(notifications, vec![Notification::Overflow { credits: 2.5 }]);
    }

    #[test]
    fn disabled_overflow_consumes_event() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let first = snapshot(
            window(10.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            start,
        );
        let second = snapshot(
            window(9.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            start + Duration::from_mins(1),
        );
        let off = NotificationSwitches {
            reset: true,
            overflow: false,
        };

        observe(&mut notifier, &mut state, &first, Some(0.0));
        let outcome = notifier.observe(
            &mut state,
            Some(&second),
            Some(1.5),
            off,
            Duration::from_mins(5),
            second.received_at,
        );
        assert!(outcome.notifications.is_empty());
        assert!(
            outcome.state_dirty,
            "被消费掉的事件也要落盘，防止换个进程再弹"
        );

        // 之后打开开关也不补弹。
        assert!(observe(&mut notifier, &mut state, &second, Some(5.0)).is_empty());
    }

    #[test]
    fn disabled_reset_stays_silent() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let off = NotificationSwitches {
            reset: false,
            overflow: true,
        };

        observe_with(
            &mut notifier,
            &mut state,
            &snapshot(
                window(80.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(weekly(start + WEEK)),
                start,
            ),
            Some(0.0),
            off,
        );
        let notifications = observe_with(
            &mut notifier,
            &mut state,
            &snapshot(
                window(1.0, FIVE_HOURS, start + FIVE_HOURS * 2),
                Some(weekly(start + WEEK)),
                start + FIVE_HOURS,
            ),
            Some(0.0),
            off,
        );

        assert!(notifications.is_empty());
    }

    #[test]
    fn persisted_period_prevents_duplicate_notification() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState {
            notified_overflow_period: period_key(&snapshot(
                window(10.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(weekly(start + WEEK)),
                start,
            )),
            ..NotifyState::default()
        };
        let current = snapshot(
            window(10.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            start,
        );

        // 缓存重建让算出来的实扣先回落再涨回来，不得重复通知。
        observe(&mut notifier, &mut state, &current, Some(0.0));

        assert!(observe(&mut notifier, &mut state, &current, Some(3.0)).is_empty());
    }

    #[test]
    fn overflow_notification_requires_long_window() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        // 只有 5h 窗口的账号没有"本期"概念。
        let snapshot = snapshot(window(10.0, FIVE_HOURS, start + FIVE_HOURS), None, start);

        observe(&mut notifier, &mut state, &snapshot, Some(0.0));

        assert!(observe(&mut notifier, &mut state, &snapshot, Some(5.0)).is_empty());
    }

    #[test]
    fn observation_gap_disarms_overflow() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        // 同一个额度窗口，只有观测时刻在变——隔离出余额通知的行为。
        let window_snapshot = |received_at: SystemTime| {
            snapshot(
                window(10.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(weekly(start + WEEK)),
                received_at,
            )
        };

        observe(
            &mut notifier,
            &mut state,
            &window_snapshot(start),
            Some(0.0),
        );

        // 休眠 3 小时后回来：期间别的设备可能已经在扣余额，不能当"刚刚发生"补发。
        let after_sleep = start + Duration::from_hours(3);
        assert!(
            observe(
                &mut notifier,
                &mut state,
                &window_snapshot(after_sleep),
                Some(5.0)
            )
            .is_empty(),
            "中断后不得补发余额通知"
        );

        // 重新建立起点之后，正常观测到的 0 → 正 仍要提醒。
        let resumed = after_sleep + Duration::from_mins(1);
        observe(
            &mut notifier,
            &mut state,
            &window_snapshot(resumed),
            Some(5.0),
        );
        let armed = resumed + Duration::from_mins(1);
        observe(
            &mut notifier,
            &mut state,
            &window_snapshot(armed),
            Some(0.0),
        );
        let spent = armed + Duration::from_mins(1);
        let notifications = observe(
            &mut notifier,
            &mut state,
            &window_snapshot(spent),
            Some(6.0),
        );

        assert_eq!(
            notifications,
            vec![Notification::Overflow { credits: 6.0 }],
            "中断后重建起点，之后的新扣费仍应提醒"
        );
    }

    #[test]
    fn gap_with_stale_snapshot_suppresses_backfill() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let before_sleep = snapshot(
            window(10.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            start,
        );

        observe(&mut notifier, &mut state, &before_sleep, Some(0.0));

        // 唤醒时先发布的是休眠前的旧快照：它的时间戳很旧，但"我们刚看到它"
        // 是现在——所以判据必须是观测时刻，不是快照时间戳。
        let wake = start + Duration::from_hours(3);
        assert!(
            observe_at(&mut notifier, &mut state, &before_sleep, Some(5.0), wake).is_empty(),
            "唤醒后先到旧快照也不得补发"
        );
        let fresh = snapshot(
            window(10.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            wake,
        );
        assert!(
            observe_at(
                &mut notifier,
                &mut state,
                &fresh,
                Some(5.0),
                wake + Duration::from_mins(1),
            )
            .is_empty(),
            "旧快照之后到位的新快照同样不得补发"
        );
    }

    #[test]
    fn stale_zero_cannot_rearm_overflow() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let before_sleep = snapshot(
            window(10.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            start,
        );

        observe(&mut notifier, &mut state, &before_sleep, Some(0.0));

        // 唤醒后先到的是休眠前那份快照，它带的 credits 也是旧值 0。
        // 不能用它把起点重新建起来。
        let wake = start + Duration::from_hours(3);
        assert!(observe_at(&mut notifier, &mut state, &before_sleep, Some(0.0), wake).is_empty());

        // 紧接着到达的新数据已经显示溢出：不得因为刚才那个旧 0 而补发。
        let fresh = snapshot(
            window(10.0, FIVE_HOURS, start + FIVE_HOURS),
            Some(weekly(start + WEEK)),
            wake + Duration::from_mins(1),
        );
        assert!(
            observe_at(
                &mut notifier,
                &mut state,
                &fresh,
                Some(5.0),
                wake + Duration::from_mins(1),
            )
            .is_empty(),
            "旧零值不得重新启用余额通知"
        );

        // 用够新的数据重新建立起点后，正常扣费仍要提醒。
        let rearmed = wake + Duration::from_mins(2);
        observe_at(&mut notifier, &mut state, &fresh, Some(0.0), rearmed);
        let spent = wake + Duration::from_mins(3);
        let notifications = observe_at(&mut notifier, &mut state, &fresh, Some(5.0), spent);

        assert_eq!(notifications, vec![Notification::Overflow { credits: 5.0 }]);
    }

    #[test]
    fn stale_reading_disarms_overflow() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        let weekly_window = || Some(weekly(start + WEEK));
        let short_window = || window(10.0, FIVE_HOURS, start + FIVE_HOURS);
        // 一份真正过期的快照：时间戳比首次观测还早 3 小时。
        let outdated = snapshot(
            short_window(),
            weekly_window(),
            start - Duration::from_hours(3),
        );

        // 新鲜数据 0 credits：建立起点。
        observe(
            &mut notifier,
            &mut state,
            &snapshot(short_window(), weekly_window(), start),
            Some(0.0),
        );

        // 之后只收到过期快照，但每次**观测间隔**都没超过盲区阈值，
        // 所以不会被判成"中断"——只有数据新鲜度这一道能拦住它。
        for minutes in [10, 20] {
            let now = start + Duration::from_mins(minutes);
            assert!(
                observe_at(&mut notifier, &mut state, &outdated, Some(0.0), now).is_empty(),
                "过期数据本身不该弹通知"
            );
        }

        // 恢复新鲜数据时已经显示扣费：起点必须已经作废，不得补发。
        let recovered = start + Duration::from_mins(21);
        assert!(
            observe_at(
                &mut notifier,
                &mut state,
                &snapshot(short_window(), weekly_window(), recovered),
                Some(5.0),
                recovered,
            )
            .is_empty(),
            "过期数据必须作废起点，否则恢复后会补发"
        );

        // 用新鲜数据重新建立起点后，正常扣费仍要提醒。
        let rearm_at = recovered + Duration::from_mins(1);
        observe_at(
            &mut notifier,
            &mut state,
            &snapshot(short_window(), weekly_window(), rearm_at),
            Some(0.0),
            rearm_at,
        );
        let spent = rearm_at + Duration::from_mins(1);
        let notifications = observe_at(
            &mut notifier,
            &mut state,
            &snapshot(short_window(), weekly_window(), spent),
            Some(5.0),
            spent,
        );

        assert_eq!(notifications, vec![Notification::Overflow { credits: 5.0 }]);
    }

    #[test]
    fn notifier_reset_drops_snapshot() {
        let start = base();
        let mut notifier = Notifier::default();
        let mut state = NotifyState::default();
        observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(80.0, FIVE_HOURS, start + FIVE_HOURS),
                Some(weekly(start + WEEK)),
                start,
            ),
            Some(0.0),
        );

        notifier.reset();
        let notifications = observe(
            &mut notifier,
            &mut state,
            &snapshot(
                window(1.0, FIVE_HOURS, start + FIVE_HOURS * 2),
                Some(weekly(start + WEEK)),
                start + FIVE_HOURS,
            ),
            Some(0.0),
        );

        assert!(
            notifications.is_empty(),
            "重激活后不得比出假重置：{notifications:?}"
        );
    }

    #[test]
    fn balloon_includes_window_and_amount() {
        let five_hour = Notification::Reset {
            window: "5h".to_owned(),
            remaining_percent: 99.4,
        };
        assert_eq!(
            five_hour.balloon_text("本周"),
            ("额度已重置", "5h 窗口已重置，当前剩余 99%".to_owned())
        );

        // 纯中文窗口名不加空格。
        let weekly = Notification::Reset {
            window: "周".to_owned(),
            remaining_percent: 100.0,
        };
        assert_eq!(weekly.balloon_text("本周").1, "周窗口已重置，当前剩余 100%");

        let (title, body) = Notification::Overflow { credits: 310.25 }.balloon_text("本周");
        assert_eq!(title, "已开始动用余额");
        assert_eq!(body, "本周已扣 310.25 credits（约 $12.41）");
    }

    /// 余额正文里的期间前缀跟着长期窗口走，和面板同一套词。
    #[test]
    fn overflow_balloon_matches_panel_period() {
        let overflow = Notification::Overflow { credits: 1.0 };
        assert_eq!(
            overflow.balloon_text("本周").1,
            "本周已扣 1 credits（约 $0.04）"
        );
        assert_eq!(
            overflow.balloon_text("本月").1,
            "本月已扣 1 credits（约 $0.04）"
        );
        // 5h 单窗口账号拿不到期间键（`period_key` 为 None），这条通知根本不会弹；
        // 真弹了也退回中性说法，不会硬说"本周"。
        assert_eq!(
            overflow.balloon_text("本期").1,
            "本期已扣 1 credits（约 $0.04）"
        );
    }

    #[test]
    fn credits_formatting_precision() {
        assert_eq!(format_credits(310.0), "310");
        assert_eq!(format_credits(310.249_999_999_999_94), "310.25");
        assert_eq!(format_credits(0.5), "0.50");
    }

    #[test]
    fn window_separator_only_follows_latin_names() {
        assert_eq!(window_separator("5h"), " ");
        assert_eq!(window_separator("3天"), "");
        assert_eq!(window_separator("周"), "");
        assert_eq!(window_separator("月"), "");
        assert_eq!(window_separator("2周"), "");
    }

    /// 测试入口只映射两条命令，别的命令一律不放行（避免误触发投递）。
    #[cfg(debug_assertions)]
    #[test]
    fn notification_test_command_mapping() {
        assert_eq!(
            test_notification_for_command(super::super::CMD_TEST_NOTIFY_RESET),
            Some(Notification::Reset {
                window: "5h".to_owned(),
                remaining_percent: 99.0,
            })
        );
        assert_eq!(
            test_notification_for_command(super::super::CMD_TEST_NOTIFY_BALANCE),
            Some(Notification::Overflow { credits: 310.25 })
        );
        assert_eq!(test_notification_for_command(0), None);
        assert_eq!(test_notification_for_command(1001), None);
    }

    #[test]
    fn period_key_tracks_long_reset() {
        let start = base();
        // 只有 5h 一个窗口时没有"本期"，周期键为 None。
        let only = window(50.0, FIVE_HOURS, start + FIVE_HOURS);
        assert_eq!(period_key(&snapshot(only.clone(), None, start)), None);

        let expected =
            i64::try_from((start + WEEK).duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap();
        assert_eq!(
            period_key(&snapshot(only, Some(weekly(start + WEEK)), start)),
            Some(expected)
        );
    }
}
