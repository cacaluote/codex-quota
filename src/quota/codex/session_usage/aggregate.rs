use std::collections::{HashMap, HashSet};

use super::model::{FileCache, ParentLink, TokenEvent, TokenSignature};
use super::parser::event_is_on_date;
use crate::quota::pricing::PriceTable;

impl ModelVolumes {
    /// 按 models.dev 牌价把分桶用量换算成美元。空价格表返回 None
    /// （无法计价）；表内查不到的模型按 0 贡献（宁可少算不虚算）。
    // 真实 token 计数远低于 2^53，u64→f64 的精度损失在这里不可能出现。
    #[allow(clippy::cast_precision_loss)]
    pub(in crate::quota::codex) fn cost(&self, table: &PriceTable) -> Option<f64> {
        if table.is_empty() {
            return None;
        }
        Some(
            self.0
                .iter()
                .filter_map(|(model, volume)| {
                    table.lookup(model.as_deref()).map(|price| {
                        price.input * volume.uncached_input as f64 / 1_000_000.0
                            + price.cached_input * volume.cached_input as f64 / 1_000_000.0
                            + price.output * volume.output as f64 / 1_000_000.0
                    })
                })
                // f64::sum 对空迭代器的折叠恒等值是 -0.0，会渲染成 $-0.00；
                // 显式以 +0.0 折叠。
                .fold(0.0, |sum, value| sum + value),
        )
    }
}

/// 分桶用量：计价时 `uncached_input` 按原价、`cached_input` 按 `cache_read` 价、
/// `output` 按 output 价（`reasoning` ⊆ `output`）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::quota::codex) struct TokenVolume {
    pub(super) uncached_input: u64,
    pub(super) cached_input: u64,
    pub(super) output: u64,
}

/// 按模型分桶；`None` 表示事件前没有 `turn_context` 记录、模型未知。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::quota::codex) struct ModelVolumes(
    pub(in crate::quota::codex) Vec<(Option<String>, TokenVolume)>,
);

fn accumulate_volume(
    volumes: &mut ModelVolumes,
    model: Option<&str>,
    uncached_input: u64,
    cached_input: u64,
    output: u64,
) {
    let index = volumes
        .0
        .iter()
        .position(|(existing, _)| existing.as_deref() == model);
    let volume = if let Some(index) = index {
        &mut volumes.0[index].1
    } else {
        volumes
            .0
            .push((model.map(str::to_owned), TokenVolume::default()));
        let Some((_, volume)) = volumes.0.last_mut() else {
            return;
        };
        volume
    };
    volume.uncached_input = volume.uncached_input.saturating_add(uncached_input);
    volume.cached_input = volume.cached_input.saturating_add(cached_input);
    volume.output = volume.output.saturating_add(output);
}

fn merge_volumes(target: &mut ModelVolumes, source: ModelVolumes) {
    for (model, volume) in source.0 {
        accumulate_volume(
            target,
            model.as_deref(),
            volume.uncached_input,
            volume.cached_input,
            volume.output,
        );
    }
}

pub(super) fn cache_has_tokens_on_date(cache: &FileCache, date: &str) -> bool {
    cache
        .events
        .iter()
        .any(|event| event.delta_total > 0 && event_is_on_date(event, date))
}

pub(super) fn aggregate_today(
    date: &str,
    selected: &HashSet<String>,
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
) -> (u64, bool, usize, ModelVolumes) {
    aggregate_usage(selected, caches, rollout_index, |event| {
        event_is_on_date(event, date)
    })
}

pub(super) fn aggregate_period(
    start_nanos: i64,
    selected: &HashSet<String>,
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
) -> (u64, bool, usize, ModelVolumes) {
    aggregate_usage(selected, caches, rollout_index, |event| {
        event
            .timestamp_nanos
            .is_some_and(|timestamp| timestamp >= start_nanos)
    })
}

pub(super) fn cache_has_tokens_in_period(cache: &FileCache, start_nanos: i64) -> bool {
    cache.events.iter().any(|event| {
        event.delta_total > 0
            && event
                .timestamp_nanos
                .is_some_and(|timestamp| timestamp >= start_nanos)
    })
}

pub(super) fn aggregate_lifetime(
    selected: &HashSet<String>,
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
) -> (u64, bool) {
    // 累计价值暂不显示，分桶在这里被丢弃。
    let (tokens, reliable, _, _) = aggregate_usage(selected, caches, rollout_index, |_| true);
    (tokens, reliable)
}

fn aggregate_usage<F>(
    selected: &HashSet<String>,
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
    includes: F,
) -> (u64, bool, usize, ModelVolumes)
where
    F: Fn(&TokenEvent) -> bool,
{
    let mut reliable = true;
    let mut deferred_files = 0usize;
    let mut groups: HashMap<String, Vec<&FileCache>> = HashMap::new();

    for key in selected {
        let Some(cache) = caches.get(key) else {
            reliable = false;
            deferred_files = deferred_files.saturating_add(1);
            continue;
        };
        let has_usage = cache
            .events
            .iter()
            .any(|event| event.delta_total > 0 && includes(event));
        let Some(root) = cache.root.as_ref() else {
            if has_usage || cache.uncertain {
                reliable = false;
                deferred_files = deferred_files.saturating_add(1);
            }
            continue;
        };
        if root.provider.as_deref() != Some("openai") {
            if root.provider.is_none() && has_usage {
                reliable = false;
                deferred_files = deferred_files.saturating_add(1);
            }
            continue;
        }
        if cache.uncertain || cache.token_without_timestamp {
            reliable = false;
            deferred_files = deferred_files.saturating_add(1);
            continue;
        }
        let Some(thread_id) = root.thread_id.as_ref() else {
            if has_usage {
                reliable = false;
                deferred_files = deferred_files.saturating_add(1);
            }
            continue;
        };
        groups.entry(thread_id.clone()).or_default().push(cache);
    }

    let mut total = 0u64;
    let mut volumes = ModelVolumes::default();
    for files in groups.values_mut() {
        files.sort_by_key(|file| std::cmp::Reverse(file.events.len()));
        let Some(canonical) = files.first().copied() else {
            continue;
        };
        if files
            .iter()
            .skip(1)
            .all(|other| event_prefix_matches(&other.events, &canonical.events))
        {
            match replayed_usage(canonical, caches, rollout_index, &includes) {
                Ok((sum, group_volumes)) => {
                    total = total.saturating_add(sum);
                    merge_volumes(&mut volumes, group_volumes);
                }
                Err(()) => {
                    defer_timeline(canonical, &includes, &mut reliable, &mut deferred_files);
                }
            }
            continue;
        }
        // Codex resume writes a new rollout that keeps the thread id but
        // starts with a reset token counter and no replayed events, so
        // same-thread files are not necessarily copies of one timeline.
        // Distinct timelines that never overlap are sequential continuations
        // whose events add up; any overlap leaves no canonical ordering and
        // still defers the group.
        let Some(chains) = sequential_timelines(files) else {
            if files.iter().any(|file| {
                file.events
                    .iter()
                    .any(|event| event.delta_total > 0 && includes(event))
            }) {
                reliable = false;
                deferred_files = deferred_files.saturating_add(1);
            }
            continue;
        };
        for chain in chains {
            match replayed_usage(chain, caches, rollout_index, &includes) {
                Ok((sum, chain_volumes)) => {
                    total = total.saturating_add(sum);
                    merge_volumes(&mut volumes, chain_volumes);
                }
                Err(()) => defer_timeline(chain, &includes, &mut reliable, &mut deferred_files),
            }
        }
    }
    (total, reliable, deferred_files, volumes)
}

fn replayed_usage<F>(
    cache: &FileCache,
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
    includes: &F,
) -> Result<(u64, ModelVolumes), ()>
where
    F: Fn(&TokenEvent) -> bool,
{
    let prefix = replay_prefix(cache, caches, rollout_index)?;
    let mut volumes = ModelVolumes::default();
    let total = cache.events.iter().skip(prefix).fold(0u64, |sum, event| {
        if includes(event) {
            accumulate_volume(
                &mut volumes,
                event.model.as_deref(),
                event.delta_uncached_input,
                event.delta_cached_input,
                event.delta_output,
            );
            sum.saturating_add(event.delta_total)
        } else {
            sum
        }
    });
    Ok((total, volumes))
}

fn defer_timeline<F>(
    cache: &FileCache,
    includes: &F,
    reliable: &mut bool,
    deferred_files: &mut usize,
) where
    F: Fn(&TokenEvent) -> bool,
{
    if cache
        .events
        .iter()
        .any(|event| event.delta_total > 0 && includes(event))
    {
        *reliable = false;
        *deferred_files = deferred_files.saturating_add(1);
    }
}

/// Collapses duplicate copies (files whose events are an exact prefix of a
/// longer file) and returns the remaining distinct timelines when each starts
/// strictly after the previous one ends, the shape of Codex resume rollouts
/// that continue a thread with a reset token counter. Returns `None` when any
/// two timelines overlap, which leaves no canonical ordering to count.
fn sequential_timelines<'a>(files: &[&'a FileCache]) -> Option<Vec<&'a FileCache>> {
    let mut timelines: Vec<&'a FileCache> = Vec::new();
    for file in files {
        if timelines
            .iter()
            .any(|kept| event_prefix_matches(&file.events, &kept.events))
        {
            continue;
        }
        timelines.push(file);
    }
    timelines.sort_by_key(|file| file.events.first().and_then(|event| event.timestamp_nanos));
    for pair in timelines.windows(2) {
        let previous_end = pair[0]
            .events
            .iter()
            .filter_map(|event| event.timestamp_nanos)
            .max();
        let starts_later = pair[1]
            .events
            .first()
            .and_then(|event| event.timestamp_nanos)
            .is_some_and(|start| previous_end.is_some_and(|end| start > end));
        if !starts_later {
            return None;
        }
    }
    Some(timelines)
}

fn event_prefix_matches(prefix: &[TokenEvent], complete: &[TokenEvent]) -> bool {
    prefix.len() <= complete.len()
        && prefix.iter().zip(complete).all(|(left, right)| {
            left.timestamp_nanos == right.timestamp_nanos && left.signature == right.signature
        })
}

fn replay_prefix(
    cache: &FileCache,
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
) -> Result<usize, ()> {
    let root = cache.root.as_ref().ok_or(())?;
    let ParentLink::Parent(parent_id) = &root.parent else {
        return if matches!(root.parent, ParentLink::None) {
            Ok(0)
        } else {
            Err(())
        };
    };
    let cutoff = root.timestamp_nanos.ok_or(())?;
    let parent_paths = rollout_index.get(parent_id).ok_or(())?;
    let mut timelines = Vec::with_capacity(parent_paths.len());
    for path in parent_paths {
        let parent = caches.get(path).ok_or(())?;
        if parent.token_without_timestamp
            || parent
                .max_timestamp_nanos
                .is_none_or(|timestamp| timestamp < cutoff)
        {
            return Err(());
        }
        timelines.push(
            parent
                .events
                .iter()
                .filter(|event| {
                    event
                        .timestamp_nanos
                        .is_some_and(|timestamp| timestamp <= cutoff)
                })
                .map(|event| event.signature.clone())
                .collect::<Vec<_>>(),
        );
    }
    let first = timelines.first().ok_or(())?;
    if timelines.iter().skip(1).any(|timeline| timeline != first) {
        return Err(());
    }
    Ok(matching_replay_prefix(&cache.events, first))
}

fn matching_replay_prefix(child: &[TokenEvent], parent: &[TokenSignature]) -> usize {
    let mut parent_offset = 0usize;
    let mut matched = 0usize;
    for event in child {
        let Some(relative) = parent[parent_offset..]
            .iter()
            .position(|signature| signature == &event.signature)
        else {
            break;
        };
        parent_offset += relative + 1;
        matched += 1;
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::{ModelVolumes, TokenVolume};
    use crate::quota::pricing::PriceTable;

    #[test]
    fn cost_weights_cached_input_and_output_separately() {
        let table = PriceTable::from_models_dev(
            r#"{"openai":{"models":{"m":{"cost":{"input":4,"output":20,"cache_read":0.4}}}}}"#,
        )
        .expect("应解析出价格表");
        let volumes = ModelVolumes(vec![(
            Some("m".to_owned()),
            TokenVolume {
                uncached_input: 1_000_000,
                cached_input: 9_000_000,
                output: 100_000,
            },
        )]);

        // 1M×$4 + 9M×$0.4 + 0.1M×$20 = $9.60；全按 input 价会得出 $41.6。
        let cost = volumes.cost(&table).unwrap();
        assert!((cost - 9.6).abs() < 1e-9);
        assert_eq!(volumes.cost(&PriceTable::default()), None);
    }

    #[test]
    fn refresh_strips_parent_replay_from_fork() {
        let context = TestContext::new("fork-replay");
        let parent = context.rollout(PARENT_ID);
        let child = context.rollout(CHILD_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
                turn_context(&context.at(5)),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta(&context.at(3), CHILD_ID, Some("openai"), Some(PARENT_ID)),
                token_count(&context.at(3), 100, Some(100), Some("codex")),
                token_count(&context.at(4), 150, Some(50), Some("codex")),
            ],
        );

        let snapshot = context
            .tracker()
            .refresh_for_date(&context.date)
            .ok()
            .map(|value| value.0);

        assert_eq!(snapshot.as_ref().map(|value| value.today_tokens), Some(150));
        // 子文件重放父文件的第一个事件（分桶被跳过），只计自己的增量。
        assert_eq!(
            snapshot.and_then(|value| value.today_volume),
            Some(ModelVolumes(vec![(
                None,
                TokenVolume {
                    uncached_input: 148,
                    cached_input: 0,
                    output: 2
                }
            )]))
        );
    }

    #[test]
    fn refresh_marks_missing_fork_parent_unreliable() {
        let context = TestContext::new("missing-parent");
        let child = context.rollout(CHILD_ID);
        write_jsonl(
            &child,
            &[
                session_meta(&context.at(0), CHILD_ID, Some("openai"), Some(PARENT_ID)),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_reliable), Some(false));
    }

    #[test]
    fn refresh_counts_active_and_archived_copy_once() {
        let context = TestContext::new("archived-copy");
        let values = [
            session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
            token_count(&context.at(1), 100, Some(100), Some("codex")),
        ];
        write_jsonl(&context.rollout(PARENT_ID), &values);
        write_jsonl(&context.archived_rollout(PARENT_ID), &values);

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_tokens), Some(100));
    }

    #[test]
    fn refresh_marks_inconsistent_active_and_archived_copies_unreliable() {
        let context = TestContext::new("archived-conflict");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        write_jsonl(
            &context.archived_rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 200, Some(200), Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_reliable), Some(false));
    }

    #[test]
    fn refresh_sums_sequential_resume_rollouts_of_same_thread() {
        let context = TestContext::new("resume-continuation");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        // Codex resume keeps the thread id but starts a reset token counter
        // and records no replayed events.
        write_jsonl(
            &context.archived_rollout(PARENT_ID),
            &[
                session_meta(&context.at(5), PARENT_ID, Some("openai"), None),
                token_count(&context.at(6), 50, Some(50), Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(
            result
                .ok()
                .map(|value| (value.0.today_tokens, value.0.today_reliable)),
            Some((150, true))
        );
    }

    #[test]
    fn refresh_defers_overlapping_same_thread_files_without_prefix() {
        let context = TestContext::new("resume-overlap");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
                token_count(&context.at(5), 150, Some(50), Some("codex")),
            ],
        );
        write_jsonl(
            &context.archived_rollout(PARENT_ID),
            &[
                session_meta(&context.at(2), PARENT_ID, Some("openai"), None),
                token_count(&context.at(3), 50, Some(50), Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_reliable), Some(false));
    }

    #[test]
    fn refresh_ignores_non_openai_provider() {
        let context = TestContext::new("provider-filter");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("custom"), None),
                token_count(&context.at(1), 100, Some(100), None),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(
            result
                .ok()
                .map(|value| (value.0.today_tokens, value.0.today_reliable)),
            Some((0, true))
        );
    }

    #[test]
    fn refresh_marks_missing_provider_with_today_usage_unreliable() {
        let context = TestContext::new("missing-provider");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, None, None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_reliable), Some(false));
    }
}
