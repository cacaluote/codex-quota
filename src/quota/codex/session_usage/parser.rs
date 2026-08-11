use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::RefreshDiagnostics;
use super::files::{open_session_file, path_key};
use super::model::{
    CandidateFile, FileCache, ParentLink, RootMeta, TokenCounters, TokenEvent, TokenSignature,
    UsageHighWater,
};
use crate::quota::codex::protocol::local_calendar_date_at;

pub(super) fn update_candidate_cache(
    candidate: &CandidateFile,
    caches: &mut HashMap<String, FileCache>,
    diagnostics: &mut RefreshDiagnostics,
) {
    let key = path_key(&candidate.path);
    if !caches.contains_key(&key)
        && let Some(old_key) = caches.iter().find_map(|(old_key, cache)| {
            cache.has_same_identity(candidate).then(|| old_key.clone())
        })
        && let Some(mut moved) = caches.remove(&old_key)
    {
        moved.path.clone_from(&key);
        caches.insert(key.clone(), moved);
    }

    let mut cache = caches
        .remove(&key)
        .unwrap_or_else(|| FileCache::empty(candidate));
    if !cache.can_resume(candidate) {
        cache = FileCache::empty(candidate);
    }
    cache.path.clone_from(&key);
    cache.filename_thread_id.clone_from(&candidate.thread_id);
    cache.creation_time = candidate.creation_time;

    if candidate.length > cache.offset {
        diagnostics.files_read = diagnostics.files_read.saturating_add(1);
        if parse_file_append(candidate, &mut cache).is_err() {
            cache.uncertain = true;
            cache.parse_errors = cache.parse_errors.saturating_add(1);
        }
    }
    cache.length = candidate.length;
    cache.last_write_time = candidate.last_write_time;
    caches.insert(key, cache);
}

fn parse_file_append(candidate: &CandidateFile, cache: &mut FileCache) -> io::Result<()> {
    let mut file = open_session_file(&candidate.path)?;
    file.seek(SeekFrom::Start(cache.offset))?;
    let mut reader = BufReader::new(file);
    let mut previous_signature = cache.events.last().map(|event| event.signature.clone());
    let mut signatures_by_source: HashMap<Option<String>, TokenSignature> = HashMap::new();
    for event in &cache.events {
        if event.signature.total.is_some() {
            signatures_by_source.insert(event.source.clone(), event.signature.clone());
        }
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
    previous_signature: &mut Option<TokenSignature>,
    signatures_by_source: &mut HashMap<Option<String>, TokenSignature>,
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

fn parse_token_event(
    payload: Option<&Value>,
    timestamp: Option<i64>,
    cache: &mut FileCache,
    previous_signature: &mut Option<TokenSignature>,
    signatures_by_source: &mut HashMap<Option<String>, TokenSignature>,
) {
    let Some(payload) = payload else {
        return;
    };
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return;
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
    let duplicate = total.is_some()
        && (signatures_by_source.get(&source) == Some(&signature)
            || previous_signature.as_ref() == Some(&signature));
    let delta_total = if duplicate {
        0
    } else if let Some(last) = last {
        last.effective_total()
    } else if let Some(total) = total {
        cumulative_delta(cache.high_water.as_ref(), total)
    } else {
        0
    };
    if let Some(total) = total {
        update_high_water(&mut cache.high_water, total);
        signatures_by_source.insert(source.clone(), signature.clone());
    }
    *previous_signature = Some(signature.clone());
    if timestamp.is_none() {
        cache.token_without_timestamp = true;
    }
    cache.events.push(TokenEvent {
        timestamp_nanos: timestamp,
        signature,
        source,
        delta_total,
    });
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
        output: value.get("output_tokens").and_then(Value::as_u64),
        reasoning_output: value.get("reasoning_output_tokens").and_then(Value::as_u64),
        total: value.get("total_tokens").and_then(Value::as_u64),
    };
    counters.has_value().then_some(counters)
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

fn update_high_water(high_water: &mut Option<UsageHighWater>, total: &TokenCounters) {
    let current = high_water.get_or_insert_with(UsageHighWater::default);
    current.input = max_option(current.input, total.input);
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
    event
        .timestamp_nanos
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

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_tokens), Some(200));
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
