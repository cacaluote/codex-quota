use std::time::{Duration, SystemTime};

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
        format_duration_label(self.window_duration)
    }

    #[must_use]
    pub fn reset_label(&self, now: SystemTime) -> String {
        match self.resets_at.duration_since(now) {
            Ok(remaining) if !remaining.is_zero() => {
                format!("{}后重置", format_compact_duration(remaining))
            }
            _ => "等待刷新".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaColor {
    Healthy,
    Warning,
    Critical,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaSnapshot {
    pub limit_id: String,
    pub primary: QuotaWindow,
    pub secondary: Option<QuotaWindow>,
    pub received_at: SystemTime,
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
    pub plan_type: Option<String>,
    pub today_tokens: Option<u64>,
    pub last_error: Option<String>,
    pub quota_refresh_interval: Duration,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            status: ConnectionStatus::Connecting,
            snapshot: None,
            plan_type: None,
            today_tokens: None,
            last_error: None,
            quota_refresh_interval: Duration::from_mins(5),
        }
    }
}

impl AppState {
    #[must_use]
    pub fn stale_after(&self) -> Duration {
        self.quota_refresh_interval
            .saturating_mul(2)
            .max(Duration::from_mins(3))
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

fn format_duration_label(duration: Duration) -> String {
    let minutes = duration.as_secs() / 60;
    if minutes != 0 && minutes.is_multiple_of(10_080) {
        format!("{}周额度", minutes / 10_080)
    } else if minutes != 0 && minutes.is_multiple_of(1_440) {
        format!("{}天额度", minutes / 1_440)
    } else if minutes != 0 && minutes.is_multiple_of(60) {
        format!("{}小时额度", minutes / 60)
    } else {
        format!("{minutes}分钟额度")
    }
}

fn format_compact_duration(duration: Duration) -> String {
    let total_minutes = duration.as_secs() / 60;
    let days = total_minutes / 1_440;
    let hours = (total_minutes % 1_440) / 60;
    let minutes = total_minutes % 60;

    if days > 0 {
        format!("{days}天{hours}小时")
    } else if hours > 0 {
        format!("{hours}小时{minutes}分")
    } else if minutes > 0 {
        format!("{minutes}分钟")
    } else {
        "不到1分钟".to_owned()
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
    fn weekly_window_has_localized_label() {
        assert_eq!(window(10.0).window_label(), "1周额度");
    }

    #[test]
    fn one_minute_refresh_keeps_a_three_minute_minimum_stale_threshold() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let state = AppState {
            snapshot: Some(QuotaSnapshot {
                limit_id: "codex".to_owned(),
                primary: window(10.0),
                secondary: None,
                received_at: now - Duration::from_secs(181),
            }),
            quota_refresh_interval: Duration::from_mins(1),
            ..AppState::default()
        };
        assert!(state.is_stale(now));
    }

    #[test]
    fn stale_threshold_is_twice_the_configured_refresh_interval() {
        let state = AppState {
            quota_refresh_interval: Duration::from_mins(10),
            ..AppState::default()
        };
        assert_eq!(state.stale_after(), Duration::from_mins(20));
    }
}
