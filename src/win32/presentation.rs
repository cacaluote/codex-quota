use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use crate::quota::{AppState, ConnectionStatus, QuotaSnapshot, QuotaWindow};

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

pub(super) fn classify_quota_windows(
    snapshot: &QuotaSnapshot,
) -> (Option<&QuotaWindow>, Option<&QuotaWindow>) {
    let primary = &snapshot.primary;
    let Some(secondary) = snapshot.secondary.as_ref() else {
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
    use super::*;

    fn window(duration: Duration) -> QuotaWindow {
        QuotaWindow {
            used_percent: 10.0,
            window_duration: duration,
            resets_at: UNIX_EPOCH,
        }
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
        let (short_term, long_term) = classify_quota_windows(&snapshot);
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
        let (short_term, long_term) = classify_quota_windows(&snapshot);
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

    #[test]
    fn window_duration_overrides_plan_fallback_label() {
        let monthly = window(Duration::from_hours(24 * 30));
        assert_eq!(
            quota_window_label(Some(&monthly), Some("plus"), false),
            "月额度"
        );
    }
}
