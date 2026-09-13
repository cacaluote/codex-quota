use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::RefreshDiagnostics;
use super::files::{open_session_file, path_key};
use super::model::{
    BalanceObservation, CandidateFile, FileCache, ParentLink, RateLimitSnapshotEntry,
    RateLimitWindowEntry, RootMeta, TokenCounters, TokenEvent, TokenSignature, UsageHighWater,
};
use crate::quota::codex::protocol::local_calendar_date_at;

pub(super) fn update_candidate_cache(
    candidate: &CandidateFile,
    caches: &mut HashMap<String, FileCache>,
    diagnostics: &mut RefreshDiagnostics,
) -> bool {
    let key = path_key(&candidate.path);
    let mut cache_dirty = false;
    if !caches.contains_key(&key)
        && let Some(old_key) = caches.iter().find_map(|(old_key, cache)| {
            cache.has_same_identity(candidate).then(|| old_key.clone())
        })
        && let Some(mut moved) = caches.remove(&old_key)
    {
        moved.path.clone_from(&key);
        if let Some(entry) = moved.latest_rate_limits.as_mut() {
            entry.source_path.clone_from(&key);
        }
        caches.insert(key.clone(), moved);
        cache_dirty = true;
    }

    let existing = caches.remove(&key);
    cache_dirty |= existing.is_none();
    let mut cache = existing.unwrap_or_else(|| FileCache::empty(candidate));
    if !cache.can_resume(candidate) {
        cache = FileCache::empty(candidate);
        cache_dirty = true;
    }
    cache_dirty |= cache.path != key
        || cache.filename_thread_id != candidate.thread_id
        || cache.creation_time != candidate.creation_time
        || cache.length != candidate.length
        || cache.last_write_time != candidate.last_write_time;
    cache.path.clone_from(&key);
    cache.filename_thread_id.clone_from(&candidate.thread_id);
    cache.creation_time = candidate.creation_time;

    if candidate.length > cache.offset {
        let previous_offset = cache.offset;
        let previous_root = cache.root.clone();
        let previous_events = cache.events.len();
        let previous_high_water = cache.high_water.clone();
        let previous_max_timestamp = cache.max_timestamp_nanos;
        let previous_token_without_timestamp = cache.token_without_timestamp;
        let previous_uncertain = cache.uncertain;
        let previous_parse_errors = cache.parse_errors;
        let previous_rate_limits = cache.latest_rate_limits.clone();
        let previous_current_model = cache.current_model.clone();
        let previous_balance_observations = cache.balance_observations.clone();
        diagnostics.files_read = diagnostics.files_read.saturating_add(1);
        if parse_file_append(candidate, &mut cache).is_err() {
            cache.uncertain = true;
            cache.parse_errors = cache.parse_errors.saturating_add(1);
        }
        diagnostics.token_events_added = diagnostics
            .token_events_added
            .saturating_add(cache.events.len().saturating_sub(previous_events));
        cache_dirty |= previous_offset != cache.offset
            || previous_root != cache.root
            || previous_events != cache.events.len()
            || previous_high_water != cache.high_water
            || previous_max_timestamp != cache.max_timestamp_nanos
            || previous_token_without_timestamp != cache.token_without_timestamp
            || previous_uncertain != cache.uncertain
            || previous_parse_errors != cache.parse_errors
            || previous_rate_limits != cache.latest_rate_limits
            || previous_current_model != cache.current_model
            || previous_balance_observations != cache.balance_observations;
    }
    cache.length = candidate.length;
    cache.last_write_time = candidate.last_write_time;
    caches.insert(key, cache);
    cache_dirty
}

fn parse_file_append(candidate: &CandidateFile, cache: &mut FileCache) -> io::Result<()> {
    let mut file = open_session_file(&candidate.path)?;
    file.seek(SeekFrom::Start(cache.offset))?;
    let mut reader = BufReader::new(file);
    let mut previous_signature = cache.events.last().map(|event| SourceBaseline {
        signature: event.signature.clone(),
        model: event.model.clone(),
    });
    let mut signatures_by_source: HashMap<Option<String>, SourceBaseline> = HashMap::new();
    for event in &cache.events {
        remember_source_signature(
            &mut signatures_by_source,
            event.source.clone(),
            event.model.clone(),
            &event.signature,
        );
    }

    loop {
        let mut bytes = Vec::new();
        let read = reader.read_until(b'\n', &mut bytes)?;
        if read == 0 {
            break;
        }
        if bytes.last() != Some(&b'\n') {
            break;
        }
        cache.offset = cache
            .offset
            .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        if bytes.is_empty() {
            continue;
        }
        let Ok(line) = std::str::from_utf8(&bytes) else {
            cache.parse_errors = cache.parse_errors.saturating_add(1);
            cache.uncertain = true;
            continue;
        };
        parse_relevant_line(
            candidate,
            cache,
            line,
            &mut previous_signature,
            &mut signatures_by_source,
        );
    }
    Ok(())
}

fn parse_relevant_line(
    candidate: &CandidateFile,
    cache: &mut FileCache,
    line: &str,
    previous_signature: &mut Option<SourceBaseline>,
    signatures_by_source: &mut HashMap<Option<String>, SourceBaseline>,
) {
    if !line.contains("\"session_meta\"")
        && !line.contains("\"turn_context\"")
        && !line.contains("\"token_count\"")
    {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        cache.parse_errors = cache.parse_errors.saturating_add(1);
        if line.contains("\"token_count\"") || line.contains("\"session_meta\"") {
            cache.uncertain = true;
        }
        return;
    };
    let timestamp = parse_timestamp_nanos(value.get("timestamp"));
    if let Some(timestamp) = timestamp {
        cache.max_timestamp_nanos = Some(
            cache
                .max_timestamp_nanos
                .map_or(timestamp, |current| current.max(timestamp)),
        );
    }

    match value.get("type").and_then(Value::as_str) {
        Some("session_meta") if cache.root.is_none() => {
            cache.root = Some(parse_root_meta(
                value.get("payload").unwrap_or(&Value::Null),
                timestamp,
                candidate.thread_id.as_deref(),
            ));
        }
        Some("turn_context") => {
            // 会话可在中途切换模型；token_count 事件自身不带模型字段，
            // 逐事件归属到最近一次 turn_context 的模型。
            if let Some(model) = value
                .pointer("/payload/model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
            {
                cache.current_model = Some(model.to_owned());
            }
        }
        Some("event_msg") => parse_token_event(
            value.get("payload"),
            timestamp,
            cache,
            previous_signature,
            signatures_by_source,
        ),
        _ => {}
    }
}

/// 每个额度来源的峰值签名，连同其所属模型。模型的累计计数器在切换时
/// 会清零（实测 per-model），因此“累计未增长”的过期判定必须限定在
/// 同一模型内，否则切换后新模型的首个事件会被误判为过期快照而丢量。
struct SourceBaseline {
    signature: TokenSignature,
    model: Option<String>,
}

fn parse_token_event(
    payload: Option<&Value>,
    timestamp: Option<i64>,
    cache: &mut FileCache,
    previous_signature: &mut Option<SourceBaseline>,
    signatures_by_source: &mut HashMap<Option<String>, SourceBaseline>,
) {
    let Some(payload) = payload else {
        return;
    };
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return;
    }
    if let Some(entry) = parse_rate_limit_snapshot(payload, timestamp, &cache.path)
        && cache
            .latest_rate_limits
            .as_ref()
            .is_none_or(|existing| entry.timestamp_nanos >= existing.timestamp_nanos)
    {
        cache.latest_rate_limits = Some(entry);
    }
    // 余额观测独立于 Token 明细采集：info 缺失或无计数器的纯额度快照
    // 也可能是某段消耗前的最后基线，必须进入账户余额时间线。
    if let Some(balance) = parse_credits_balance(payload)
        && let Some(timestamp) = timestamp
    {
        cache.balance_observations.push(BalanceObservation {
            timestamp_nanos: timestamp,
            balance,
        });
    }
    let Some(info) = payload.get("info").filter(|info| !info.is_null()) else {
        return;
    };
    let Some(signature) = parse_token_signature(info) else {
        return;
    };
    let source = payload
        .pointer("/rate_limits/limit_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let total = signature.total.as_ref();
    let last = signature.last.as_ref();
    let previous_for_source = signatures_by_source.get(&source);
    let duplicate = total.is_some()
        && [previous_for_source, previous_signature.as_ref()]
            .into_iter()
            .flatten()
            .any(|baseline| {
                baseline.model == cache.current_model && baseline.signature == signature
            });
    // A later event can change last_token_usage while repeating the same
    // cumulative total. That is a replayed snapshot, not a new turn——
    // 但模型切换后计数器清零是真实的新用量，不得判为过期。
    let model_unchanged =
        previous_for_source.is_some_and(|baseline| baseline.model == cache.current_model);
    let stale_snapshot = !duplicate
        && last.is_some()
        && model_unchanged
        && cumulative_did_not_advance(
            previous_for_source.map(|baseline| &baseline.signature),
            total,
        );
    // `total_tokens = input_tokens + output_tokens` 且 `reasoning_output ⊆ output`、
    // `cached_input ⊆ input`（对 72k 真实事件做过不变量检验），因此计价桶为
    // input−cached（原价）、cached（cache_read 价）、output（output 价，含 reasoning）。
    // cache_write_input_tokens 实测恒 0，v1 不计价。
    let (delta_total, delta_uncached_input, delta_cached_input, delta_output) = token_deltas(
        duplicate,
        stale_snapshot,
        last,
        total,
        cache.high_water.as_ref(),
    );
    if let Some(total) = total {
        update_high_water(&mut cache.high_water, total);
    }
    remember_source_signature(
        signatures_by_source,
        source.clone(),
        cache.current_model.clone(),
        &signature,
    );
    *previous_signature = Some(SourceBaseline {
        signature: signature.clone(),
        model: cache.current_model.clone(),
    });
    if timestamp.is_none() {
        cache.token_without_timestamp = true;
    }
    let (primary_percent, secondary_percent) = parse_window_percents(payload);
    cache.events.push(TokenEvent {
        timestamp_nanos: timestamp,
        signature,
        source,
        delta_total,
        delta_uncached_input,
        delta_cached_input,
        delta_output,
        model: cache.current_model.clone(),
        primary_percent,
        secondary_percent,
    });
}

/// 事件增量四元组（total、未缓存 input、cached input、output）。
fn token_deltas(
    duplicate: bool,
    stale_snapshot: bool,
    last: Option<&TokenCounters>,
    total: Option<&TokenCounters>,
    high_water: Option<&UsageHighWater>,
) -> (u64, u64, u64, u64) {
    if duplicate || stale_snapshot {
        return (0, 0, 0, 0);
    }
    if let Some(last) = last {
        let cached = last.cached_input.unwrap_or(0);
        return (
            last.effective_total(),
            last.input.unwrap_or(0).saturating_sub(cached),
            cached,
            last.output.unwrap_or(0),
        );
    }
    if let Some(total) = total {
        // 实测 35k 真实事件全部携带 last_token_usage，此路径仅为兜底。
        let (uncached, cached, output) = cumulative_bucket_deltas(high_water, total);
        return (
            cumulative_delta(high_water, total),
            uncached,
            cached,
            output,
        );
    }
    (0, 0, 0, 0)
}

/// 事件自带的 codex 窗口进度。仅接受 `codex` limit：其他 limit 的进度
/// 口径不同，不得驱动溢出判定。
fn parse_window_percents(payload: &Value) -> (Option<f64>, Option<f64>) {
    let Some(limits) = codex_rate_limits(payload) else {
        return (None, None);
    };
    let primary = limits
        .pointer("/primary/used_percent")
        .and_then(Value::as_f64);
    let secondary = limits
        .pointer("/secondary/used_percent")
        .and_then(Value::as_f64);
    (primary, secondary)
}

/// credits 余额观测。仅接受 `codex` limit；余额是高精度十进制字符串
/// （如 "575.0172470000"），容忍数字形式。
fn parse_credits_balance(payload: &Value) -> Option<f64> {
    let limits = codex_rate_limits(payload)?;
    limits.pointer("/credits/balance").and_then(|value| {
        value
            .as_str()
            .and_then(|text| text.parse::<f64>().ok())
            .or_else(|| value.as_f64())
    })
}

fn codex_rate_limits(payload: &Value) -> Option<&Value> {
    let limits = payload
        .get("rate_limits")
        .filter(|value| !value.is_null())?;
    (limits.get("limit_id").and_then(Value::as_str) == Some("codex")).then_some(limits)
}

fn parse_rate_limit_snapshot(
    payload: &Value,
    timestamp: Option<i64>,
    source_path: &str,
) -> Option<RateLimitSnapshotEntry> {
    let limits = payload
        .get("rate_limits")
        .filter(|value| !value.is_null())?;
    if limits.get("limit_id").and_then(Value::as_str) != Some("codex") {
        return None;
    }
    let timestamp_nanos = timestamp?;
    let primary = parse_rate_limit_window(limits.get("primary")?)?;
    let secondary = limits
        .get("secondary")
        .filter(|value| !value.is_null())
        .and_then(parse_rate_limit_window);
    let plan_type = limits
        .get("plan_type")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    Some(RateLimitSnapshotEntry {
        timestamp_nanos,
        limit_id: "codex".to_owned(),
        source_path: source_path.to_owned(),
        primary,
        secondary,
        plan_type,
    })
}

fn parse_rate_limit_window(value: &Value) -> Option<RateLimitWindowEntry> {
    Some(RateLimitWindowEntry {
        used_percent: value.get("used_percent").and_then(Value::as_f64)?,
        window_minutes: value.get("window_minutes").and_then(Value::as_u64)?,
        resets_at: value.get("resets_at").and_then(Value::as_i64)?,
    })
}

fn parse_root_meta(
    payload: &Value,
    timestamp_nanos: Option<i64>,
    filename_thread_id: Option<&str>,
) -> RootMeta {
    let meta_thread_id = payload
        .get("id")
        .or_else(|| payload.get("thread_id"))
        .or_else(|| payload.get("threadId"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let thread_id = filename_thread_id
        .map(str::to_owned)
        .or_else(|| meta_thread_id.clone());
    let id_conflict = filename_thread_id
        .zip(meta_thread_id.as_deref())
        .is_some_and(|(filename, meta)| filename != meta);
    let provider = payload
        .get("model_provider")
        .or_else(|| payload.get("modelProvider"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);

    let mut parents = [
        payload.get("forked_from_id"),
        payload.get("parent_thread_id"),
        payload.pointer("/source/subagent/thread_spawn/parent_thread_id"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .filter(|value| !value.is_empty())
    .map(str::to_owned)
    .collect::<Vec<_>>();
    parents.sort();
    parents.dedup();
    let parent = if id_conflict || parents.len() > 1 {
        ParentLink::Ambiguous
    } else if let Some(parent) = parents.pop() {
        if thread_id.as_deref() == Some(parent.as_str()) {
            ParentLink::Ambiguous
        } else {
            ParentLink::Parent(parent)
        }
    } else {
        ParentLink::None
    };
    RootMeta {
        thread_id,
        provider,
        timestamp_nanos,
        parent,
    }
}

fn parse_token_signature(info: &Value) -> Option<TokenSignature> {
    let total = parse_counters(info.get("total_token_usage"));
    let last = parse_counters(info.get("last_token_usage"));
    (total.is_some() || last.is_some()).then_some(TokenSignature { total, last })
}

fn parse_counters(value: Option<&Value>) -> Option<TokenCounters> {
    let value = value?.as_object()?;
    let counters = TokenCounters {
        input: value.get("input_tokens").and_then(Value::as_u64),
        cached_input: value
            .get("cached_input_tokens")
            .or_else(|| value.get("cache_read_input_tokens"))
            .and_then(Value::as_u64),
        cache_write_input: value
            .get("cache_write_input_tokens")
            .and_then(Value::as_u64),
        output: value.get("output_tokens").and_then(Value::as_u64),
        reasoning_output: value.get("reasoning_output_tokens").and_then(Value::as_u64),
        total: value.get("total_tokens").and_then(Value::as_u64),
    };
    counters.has_value().then_some(counters)
}

fn remember_source_signature(
    signatures_by_source: &mut HashMap<Option<String>, SourceBaseline>,
    source: Option<String>,
    model: Option<String>,
    signature: &TokenSignature,
) {
    let Some(total) = signature.total.as_ref() else {
        return;
    };
    // Older snapshots can arrive after a higher cumulative total. Keep the
    // peak so a later partial recovery is not treated as a new turn——峰值
    // 按模型隔离，跨模型的更低累计是新计数器，必须覆盖。
    if let Some(baseline) = signatures_by_source.get(&source)
        && baseline.model == model
        && baseline
            .signature
            .total
            .as_ref()
            .is_some_and(|previous_total| {
                total.effective_total() <= previous_total.effective_total()
            })
    {
        return;
    }
    signatures_by_source.insert(
        source,
        SourceBaseline {
            signature: signature.clone(),
            model,
        },
    );
}

fn cumulative_did_not_advance(
    previous: Option<&TokenSignature>,
    total: Option<&TokenCounters>,
) -> bool {
    let Some(total) = total else {
        return false;
    };
    let Some(previous_total) = previous.and_then(|signature| signature.total.as_ref()) else {
        return false;
    };
    total.effective_total() <= previous_total.effective_total()
}

fn cumulative_delta(high_water: Option<&UsageHighWater>, total: &TokenCounters) -> u64 {
    let Some(high_water) = high_water else {
        return total.effective_total();
    };
    if total.input.is_some() || total.output.is_some() {
        total
            .input
            .unwrap_or(0)
            .saturating_sub(high_water.input.unwrap_or(0))
            .saturating_add(
                total
                    .output
                    .unwrap_or(0)
                    .saturating_sub(high_water.output.unwrap_or(0)),
            )
    } else {
        total
            .effective_total()
            .saturating_sub(high_water.effective_total())
    }
}

/// `cumulative_delta` 的分桶版本。cached 单独维护高水位；未缓存桶需要从
/// input 增量中再扣除 cached 增量（cached ⊆ input）。
fn cumulative_bucket_deltas(
    high_water: Option<&UsageHighWater>,
    total: &TokenCounters,
) -> (u64, u64, u64) {
    let (input_high, cached_high, output_high) = high_water.map_or((0, 0, 0), |water| {
        (
            water.input.unwrap_or(0),
            water.cached_input.unwrap_or(0),
            water.output.unwrap_or(0),
        )
    });
    let cached = total.cached_input.unwrap_or(0).saturating_sub(cached_high);
    let uncached = total
        .input
        .unwrap_or(0)
        .saturating_sub(input_high)
        .saturating_sub(cached);
    (
        uncached,
        cached,
        total.output.unwrap_or(0).saturating_sub(output_high),
    )
}

fn update_high_water(high_water: &mut Option<UsageHighWater>, total: &TokenCounters) {
    let current = high_water.get_or_insert_with(UsageHighWater::default);
    current.input = max_option(current.input, total.input);
    current.cached_input = max_option(current.cached_input, total.cached_input);
    current.output = max_option(current.output, total.output);
    current.total = max_option(current.total, total.total);
}

fn max_option(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    }
}

pub(super) fn event_is_on_date(event: &TokenEvent, date: &str) -> bool {
    ts_is_on_date(event.timestamp_nanos, date)
}

pub(super) fn ts_is_on_date(timestamp: Option<i64>, date: &str) -> bool {
    timestamp
        .and_then(system_time_from_unix_nanos)
        .and_then(local_calendar_date_at)
        .as_deref()
        == Some(date)
}

pub(super) fn parse_timestamp_nanos(value: Option<&Value>) -> Option<i64> {
    let parsed = OffsetDateTime::parse(value?.as_str()?, &Rfc3339).ok()?;
    i64::try_from(parsed.unix_timestamp_nanos()).ok()
}

pub(super) fn system_time_from_unix_nanos(nanos: i64) -> Option<SystemTime> {
    if nanos >= 0 {
        let nanos = u64::try_from(nanos).ok()?;
        Some(UNIX_EPOCH + Duration::from_nanos(nanos))
    } else {
        Some(UNIX_EPOCH.checked_sub(Duration::from_nanos(nanos.unsigned_abs()))?)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};

    use super::super::aggregate::{ModelVolumes, TokenVolume};
    use super::super::test_support::*;

    #[test]
    fn refresh_sums_exact_last_usage_from_cross_day_file() {
        let context = TestContext::new("exact-last");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
                token_count(&context.at(2), 150, Some(50), Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_tokens), Some(150));
    }

    #[test]
    fn refresh_deduplicates_repeated_snapshot_from_same_source() {
        let context = TestContext::new("same-source-repeat");
        let file = context.rollout(PARENT_ID);
        let repeated = token_count(&context.at(1), 100, Some(100), Some("codex"));
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                repeated.clone(),
                token_count(&context.at(2), 200, Some(100), Some("other")),
                repeated,
            ],
        );

        let snapshot = context
            .tracker()
            .refresh_for_date(&context.date)
            .ok()
            .map(|value| value.0);

        assert_eq!(snapshot.as_ref().map(|value| value.today_tokens), Some(200));
        // 重复快照的三桶增量为 0；两个有效事件（codex 首发 + other）各计 100。
        assert_eq!(
            snapshot.and_then(|value| value.today_volume),
            Some(ModelVolumes(vec![(
                None,
                TokenVolume {
                    uncached_input: 198,
                    cached_input: 0,
                    output: 2
                }
            )]))
        );
    }

    #[test]
    fn refresh_deduplicates_adjacent_snapshot_across_sources() {
        let context = TestContext::new("cross-source-repeat");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
                token_count(&context.at(2), 100, Some(100), Some("codex-other")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_tokens), Some(100));
    }

    #[test]
    fn refresh_saturates_token_sum_on_overflow() {
        let context = TestContext::new("overflow");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(
                    &context.at(1),
                    u64::MAX - 1,
                    Some(u64::MAX - 1),
                    Some("codex"),
                ),
                token_count(
                    &context.at(2),
                    u64::MAX,
                    Some(u64::MAX),
                    Some("codex-other"),
                ),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(
            result.ok().map(|value| value.0.today_tokens),
            Some(u64::MAX)
        );
    }

    #[test]
    fn refresh_ignores_last_usage_when_cumulative_total_does_not_advance() {
        let context = TestContext::new("stale-last");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 1_000, Some(200), Some("codex")),
                token_count(&context.at(2), 1_000, Some(50), Some("codex")),
                token_count(&context.at(3), 1_080, Some(80), Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_tokens), Some(280));
    }

    #[test]
    fn refresh_ignores_partial_recovery_below_source_high_water() {
        let context = TestContext::new("partial-recovery");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 1_000, Some(100), Some("codex")),
                token_count(&context.at(2), 900, Some(80), Some("codex")),
                token_count(&context.at(3), 950, Some(50), Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_tokens), Some(100));
    }

    #[test]
    fn refresh_counts_last_usage_after_source_total_passes_high_water() {
        let context = TestContext::new("pass-high-water");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 1_000, Some(100), Some("codex")),
                token_count(&context.at(2), 900, Some(80), Some("codex")),
                token_count(&context.at(3), 1_100, Some(100), Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_tokens), Some(200));
    }

    #[test]
    fn refresh_keeps_last_usage_when_other_source_has_lower_cumulative_total() {
        let context = TestContext::new("interleaved-lower-total");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 1_000, Some(100), Some("codex")),
                token_count(&context.at(2), 500, Some(50), Some("other")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_tokens), Some(150));
    }

    #[test]
    fn refresh_uses_cumulative_delta_when_last_usage_is_missing() {
        let context = TestContext::new("cumulative");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, None, Some("codex")),
                token_count(&context.at(2), 150, None, Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_tokens), Some(150));
    }

    #[test]
    fn refresh_retries_unterminated_last_line_after_append() {
        use std::io::Write;

        let context = TestContext::new("partial-line");
        let file = context.rollout(PARENT_ID);
        assert!(
            file.parent()
                .is_some_and(|parent| fs::create_dir_all(parent).is_ok())
        );
        let meta = session_meta(&context.at(0), PARENT_ID, Some("openai"), None);
        let event = token_count(&context.at(1), 100, Some(100), Some("codex"));
        assert!(fs::write(&file, format!("{meta}\n{event}")).is_ok());
        let mut tracker = context.tracker();
        let first = tracker.refresh_for_date(&context.date);
        let mut writer = OpenOptions::new().append(true).open(&file).unwrap();
        assert!(writeln!(writer).is_ok());
        drop(writer);

        let second = tracker.refresh_for_date(&context.date);

        assert_eq!(
            (
                first.ok().map(|value| value.0.today_tokens),
                second.ok().map(|value| value.0.today_tokens)
            ),
            (Some(0), Some(100))
        );
    }

    #[test]
    fn refresh_rebuilds_cache_after_file_truncation() {
        let context = TestContext::new("truncate");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 1_000, Some(1_000), Some("codex")),
            ],
        );
        let mut tracker = context.tracker();
        let first = tracker.refresh_for_date(&context.date);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(2), PARENT_ID, Some("openai"), None),
                token_count(&context.at(3), 50, Some(50), Some("codex")),
            ],
        );

        let second = tracker.refresh_for_date(&context.date);

        assert_eq!(
            (
                first.ok().map(|value| value.0.today_tokens),
                second.ok().map(|value| value.0.today_tokens)
            ),
            (Some(1_000), Some(50))
        );
    }

    #[test]
    fn model_switch_preserves_identical_usage_in_full_and_incremental_scans() {
        let prices = crate::quota::pricing::PriceTable::from_models_dev(
            r#"{"openai":{"models":{"model-a":{"cost":{"input":1,"cache_read":0.1,"output":2}},"model-b":{"cost":{"input":10,"cache_read":1,"output":20}}}}}"#,
        )
        .unwrap();
        for incremental in [false, true] {
            for source in ["codex", "codex-other"] {
                let context = TestContext::new("identical-model-switch");
                let file = context.rollout(PARENT_ID);
                write_jsonl(
                    &file,
                    &[
                        session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                        turn_context_model(&context.at(1), "model-a"),
                        token_count_buckets(&context.at(2), 1000, 500, 100, 0, Some("codex")),
                    ],
                );
                let mut tracker = context.tracker();
                if incremental {
                    tracker.refresh_for_date(&context.date).unwrap();
                    // Context may arrive separately from the first token event.
                    append_jsonl(&file, &turn_context_model(&context.at(3), "model-b"));
                    tracker.refresh_for_date(&context.date).unwrap();
                } else {
                    append_jsonl(&file, &turn_context_model(&context.at(3), "model-b"));
                }
                append_jsonl(
                    &file,
                    &token_count_buckets(&context.at(4), 1000, 500, 100, 0, Some(source)),
                );
                // The duplicate within model-b must still be suppressed.
                append_jsonl(
                    &file,
                    &token_count_buckets(&context.at(5), 1000, 500, 100, 0, Some(source)),
                );
                let snapshot = tracker.refresh_for_date(&context.date).unwrap().0;
                assert_eq!(snapshot.today_tokens, 2200);
                let cost = snapshot.today_volume.unwrap().cost(&prices).unwrap();
                assert!(
                    (cost - 0.00825).abs() < 1e-12,
                    "cost={cost}, incremental={incremental}, source={source}"
                );
            }
        }
    }

    #[test]
    fn refresh_splits_buckets_by_model_across_switches() {
        let context = TestContext::new("model-buckets");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                turn_context_model(&context.at(1), "gpt-5.6-sol"),
                token_count_buckets(&context.at(2), 25_445, 18_176, 182, 37, Some("codex")),
                turn_context_model(&context.at(3), "gpt-6-astra"),
                token_count_buckets(&context.at(4), 10_000, 8_000, 100, 20, Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(
            result.ok().and_then(|value| value.0.today_volume),
            Some(ModelVolumes(vec![
                (
                    Some("gpt-5.6-sol".to_owned()),
                    TokenVolume {
                        uncached_input: 25_445 - 18_176,
                        cached_input: 18_176,
                        output: 182
                    }
                ),
                (
                    Some("gpt-6-astra".to_owned()),
                    TokenVolume {
                        uncached_input: 2_000,
                        cached_input: 8_000,
                        output: 100
                    }
                ),
            ]))
        );
    }

    #[test]
    fn refresh_leaves_pre_turn_context_events_unattributed() {
        let context = TestContext::new("unattributed-model");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count_buckets(&context.at(1), 1_000, 0, 10, 0, Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(
            result.ok().and_then(|value| value.0.today_volume),
            Some(ModelVolumes(vec![(
                None,
                TokenVolume {
                    uncached_input: 1_000,
                    cached_input: 0,
                    output: 10
                }
            )]))
        );
    }

    #[test]
    fn refresh_cumulative_buckets_track_high_water_per_bucket() {
        let context = TestContext::new("cumulative-buckets");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                turn_context_model(&context.at(1), "gpt-5.6-sol"),
                // 无 last_token_usage：走累计高水位路径。
                token_count(&context.at(2), 1_000, None, Some("codex")),
                token_count(&context.at(3), 1_500, None, Some("codex")),
            ],
        );

        let snapshot = context
            .tracker()
            .refresh_for_date(&context.date)
            .ok()
            .map(|value| value.0);

        assert_eq!(
            snapshot.as_ref().map(|value| value.today_tokens),
            Some(1_500)
        );
        assert_eq!(
            snapshot.and_then(|value| value.today_volume),
            Some(ModelVolumes(vec![(
                Some("gpt-5.6-sol".to_owned()),
                TokenVolume {
                    uncached_input: 1_490,
                    cached_input: 0,
                    output: 10
                }
            )]))
        );
    }

    #[test]
    fn malformed_token_line_makes_openai_file_unreliable() {
        let context = TestContext::new("malformed");
        let file = context.rollout(PARENT_ID);
        assert!(
            file.parent()
                .is_some_and(|parent| fs::create_dir_all(parent).is_ok())
        );
        let meta = session_meta(&context.at(0), PARENT_ID, Some("openai"), None);
        assert!(
            fs::write(
                &file,
                format!(
                    "{meta}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\"}}\n"
                )
            )
            .is_ok()
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_reliable), Some(false));
    }
}
