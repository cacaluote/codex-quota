use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use crate::error::AppError;
use crate::quota::{QuotaSnapshot, QuotaWindow};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRateLimit {
    limit_id: String,
    primary: Option<RawWindow>,
    secondary: Option<RawWindow>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWindow {
    used_percent: f64,
    window_duration_mins: u64,
    resets_at: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReadResult {
    rate_limits: Option<RawRateLimit>,
    #[serde(default)]
    rate_limits_by_limit_id: HashMap<String, RawRateLimit>,
}

#[derive(Debug, Deserialize)]
struct RawAccountReadResult {
    account: Option<RawAccount>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAccount {
    plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawUsageReadResult {
    summary: Option<RawUsageSummary>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawUsageSummary {
    lifetime_tokens: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct LifetimeUsage {
    pub(super) lifetime: Option<u64>,
}

pub(super) fn parse_rate_limits_result(
    value: Value,
    received_at: SystemTime,
) -> Result<QuotaSnapshot, AppError> {
    let result: RawReadResult = serde_json::from_value(value)?;
    let selected = result
        .rate_limits_by_limit_id
        .get("codex")
        .cloned()
        .or(result.rate_limits)
        .ok_or_else(|| AppError::Protocol("响应中没有 Codex 额度桶".to_owned()))?;
    let primary = selected
        .primary
        .as_ref()
        .ok_or_else(|| AppError::Protocol("Codex 额度桶缺少 primary".to_owned()))?;
    let converted_primary = convert_window(primary)?;
    let converted_secondary = selected
        .secondary
        .as_ref()
        .map(convert_window)
        .transpose()?;

    Ok(QuotaSnapshot {
        limit_id: selected.limit_id,
        primary: converted_primary,
        secondary: converted_secondary,
        received_at,
    })
}

pub(super) fn parse_account_result(value: Value) -> Result<Option<String>, AppError> {
    let result: RawAccountReadResult = serde_json::from_value(value)?;
    Ok(result.account.and_then(|account| account.plan_type))
}

pub(super) fn parse_lifetime_usage(value: Value) -> Result<LifetimeUsage, AppError> {
    let result: RawUsageReadResult = serde_json::from_value(value)?;
    Ok(LifetimeUsage {
        lifetime: result.summary.and_then(|summary| summary.lifetime_tokens),
    })
}

pub(super) fn local_calendar_date() -> String {
    // SAFETY: GetLocalTime returns a SYSTEMTIME value without borrowing caller-owned storage.
    let local = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02}", local.wYear, local.wMonth, local.wDay)
}

pub(super) fn local_calendar_date_at(time: SystemTime) -> Option<String> {
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
        FileTimeToSystemTime(&raw const file_time, &raw mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &raw const utc, &raw mut local).ok()?;
    }
    Some(format!(
        "{:04}-{:02}-{:02}",
        local.wYear, local.wMonth, local.wDay
    ))
}

fn convert_window(raw: &RawWindow) -> Result<QuotaWindow, AppError> {
    let reset_seconds = u64::try_from(raw.resets_at)
        .map_err(|_| AppError::Protocol("重置时间戳不能为负数".to_owned()))?;
    Ok(QuotaWindow {
        used_percent: raw.used_percent,
        window_duration: Duration::from_secs(raw.window_duration_mins.saturating_mul(60)),
        resets_at: UNIX_EPOCH + Duration::from_secs(reset_seconds),
    })
}

pub(super) fn classify_rpc_error(error: &Value) -> AppError {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("未知 RPC 错误")
        .to_owned();
    let lowercase = message.to_ascii_lowercase();
    if lowercase.contains("auth")
        || lowercase.contains("login")
        || lowercase.contains("unauthorized")
    {
        AppError::Authentication(message)
    } else {
        AppError::Protocol(message)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parser_accepts_primary_only_response() {
        let result = json!({
            "rateLimits": {
                "limitId": "codex",
                "primary": {
                    "usedPercent": 25,
                    "windowDurationMins": 10_080,
                    "resetsAt": 200_000
                },
                "secondary": null,
                "unknownFutureField": true
            }
        });
        let snapshot = parse_rate_limits_result(result, UNIX_EPOCH);
        assert!(
            snapshot
                .is_ok_and(|value| { (value.primary.used_percent - 25.0).abs() < f64::EPSILON })
        );
    }

    #[test]
    fn parser_prefers_codex_bucket_from_multi_bucket_response() {
        let result = json!({
            "rateLimits": {
                "limitId": "legacy",
                "primary": { "usedPercent": 90, "windowDurationMins": 60, "resetsAt": 200_000 }
            },
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "primary": { "usedPercent": 12, "windowDurationMins": 10_080, "resetsAt": 200_000 }
                },
                "other": {
                    "limitId": "other",
                    "primary": { "usedPercent": 80, "windowDurationMins": 60, "resetsAt": 200_000 }
                }
            }
        });
        let snapshot = parse_rate_limits_result(result, UNIX_EPOCH);
        assert!(snapshot.is_ok_and(|value| value.limit_id == "codex"));
    }

    #[test]
    fn parser_accepts_secondary_window() {
        let result = json!({
            "rateLimits": {
                "limitId": "codex",
                "primary": { "usedPercent": 10, "windowDurationMins": 300, "resetsAt": 200_000 },
                "secondary": { "usedPercent": 20, "windowDurationMins": 10_080, "resetsAt": 300_000 }
            }
        });
        let snapshot = parse_rate_limits_result(result, UNIX_EPOCH);
        assert!(snapshot.is_ok_and(|value| value.secondary.is_some()));
    }

    #[test]
    fn parser_rejects_missing_primary_window() {
        let result = json!({ "rateLimits": { "limitId": "codex", "primary": null } });
        let error = parse_rate_limits_result(result, UNIX_EPOCH);
        assert!(matches!(error, Err(AppError::Protocol(_))));
    }

    #[test]
    fn account_parser_reads_chatgpt_plan_type() {
        let result = json!({
            "account": {
                "type": "chatgpt",
                "email": "user@example.com",
                "planType": "pro",
                "unknownFutureField": true
            },
            "requiresOpenaiAuth": true
        });

        assert_eq!(
            parse_account_result(result).ok(),
            Some(Some("pro".to_owned()))
        );
    }

    #[test]
    fn account_parser_accepts_account_without_plan_type() {
        let result = json!({
            "account": { "type": "apiKey" },
            "requiresOpenaiAuth": true
        });

        assert_eq!(parse_account_result(result).ok(), Some(None));
    }

    #[test]
    fn usage_parser_reads_lifetime_and_ignores_daily_buckets() {
        let result = json!({
            "summary": {
                "lifetimeTokens": 900_000,
                "unknownFutureField": true
            },
            "dailyUsageBuckets": [
                { "startDate": "2026-07-29", "tokens": 1_000 },
                { "startDate": "2026-07-30", "tokens": 12_345 },
                { "startDate": "2026-07-31", "tokens": 67_890 },
                { "startDate": "2026-08-01", "tokens": 2_000 }
            ],
            "unknownFutureField": true
        });

        assert_eq!(
            parse_lifetime_usage(result).ok(),
            Some(LifetimeUsage {
                lifetime: Some(900_000),
            })
        );
    }

    #[test]
    fn usage_parser_accepts_null_summary() {
        let result = json!({
            "summary": null,
            "dailyUsageBuckets": null
        });

        assert_eq!(
            parse_lifetime_usage(result).ok(),
            Some(LifetimeUsage { lifetime: None })
        );
    }
}
