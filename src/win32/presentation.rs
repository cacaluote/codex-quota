use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use crate::quota::{AppState, ConnectionStatus, QuotaColor, QuotaWindow};

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

/// 社区参考的周额度美元价值：账号侧估算（本期套餐内已用 ÷ 周已用%）之外
/// 的第二个参照点。静态常量，不做新鲜度门控。
pub(super) const COMMUNITY_WEEKLY_VALUE_USD: f64 = 120.0;

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
    let (short_term, long_term) = display_windows(state, now);
    let Some(window) = short_term.or(long_term) else {
        return ("--".to_owned(), 0.0, QuotaColor::Unknown);
    };
    let blocked = [short_term, long_term]
        .into_iter()
        .flatten()
        .any(|active| active.remaining_percent() <= 0.0);
    if blocked {
        return ("0".to_owned(), 0.0, QuotaColor::Critical);
    }
    let remaining = window.remaining_percent();
    (format!("{remaining:.0}"), remaining, window.color())
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

fn format_token_count(tokens: u64) -> String {
    match tokens {
        0..=9_999 => tokens.to_string(),
        10_000..=99_999_999 => format_compact_token_unit(tokens, 10_000, "万"),
        _ => format_compact_token_unit(tokens, 100_000_000, "亿"),
    }
}

pub(super) fn format_token_usage(tokens: Option<u64>) -> String {
    tokens.map_or_else(
        || "--".to_owned(),
        |value| format!("{} Token", format_token_count(value)),
    )
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
        assert_eq!(format_token_count(4_280), "4280");
    }

    #[test]
    fn token_count_uses_ten_thousand_unit() {
        assert_eq!(format_token_count(42_803_000), "4280.3万");
    }

    #[test]
    fn today_usage_appends_token_unit() {
        assert_eq!(format_token_usage(Some(42_803_000)), "4280.3万 Token");
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
        assert_eq!(format_token_count(128_000_000), "1.3亿");
    }

    #[test]
    fn token_count_omits_zero_decimal() {
        assert_eq!(format_token_count(100_000_000), "1亿");
    }

    #[test]
    fn zero_token_count_remains_zero() {
        assert_eq!(format_token_count(0), "0");
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

    #[test]
    fn window_duration_overrides_plan_fallback_label() {
        let monthly = window(Duration::from_hours(24 * 30));
        assert_eq!(
            quota_window_label(Some(&monthly), Some("plus"), false),
            "月额度"
        );
    }
}
