use std::collections::{HashMap, HashSet};

use super::model::{BalanceObservation, FileCache, ParentLink, TokenEvent, TokenSignature};
use super::parser::{event_is_on_date, ts_is_on_date};
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

pub(super) fn cache_has_tokens_on_date(cache: &FileCache, date: &str) -> bool {
    cache
        .events
        .iter()
        .any(|event| event.delta_total > 0 && event_is_on_date(event, date))
}

/// 单窗口聚合结果。`tokens`/`volumes` 是套餐内用量（不含余额溢出），
/// `overflow_tokens` 是本机触顶后的溢出用量，两者相加等于窗口内全部实际
/// 用量；`credits_spent` 是窗口内 credits 实扣（账户级观测，含其他设备的
/// 消耗，与本地 token 口径不同——它回答“余额付了多少钱”而非“值多少钱”）。
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct WindowAggregate {
    pub(super) tokens: u64,
    pub(super) reliable: bool,
    pub(super) deferred_files: usize,
    pub(super) volumes: ModelVolumes,
    pub(super) overflow_tokens: u64,
    pub(super) credits_spent: f64,
}

pub(super) fn aggregate_today(
    date: &str,
    selected: &HashSet<String>,
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
    balance_baseline: Option<BalanceObservation>,
) -> WindowAggregate {
    aggregate_usage(
        selected,
        caches,
        rollout_index,
        |event| event_is_on_date(event, date),
        |timestamp| ts_is_on_date(timestamp, date),
        balance_baseline,
    )
}

pub(super) fn aggregate_period(
    start_nanos: i64,
    selected: &HashSet<String>,
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
    balance_baseline: Option<BalanceObservation>,
) -> WindowAggregate {
    aggregate_usage(
        selected,
        caches,
        rollout_index,
        |event| {
            event
                .timestamp_nanos
                .is_some_and(|timestamp| timestamp >= start_nanos)
        },
        |timestamp| timestamp.is_some_and(|value| value >= start_nanos),
        balance_baseline,
    )
}

pub(super) fn cache_has_tokens_in_period(cache: &FileCache, start_nanos: i64) -> bool {
    cache.events.iter().any(|event| {
        event.delta_total > 0
            && event
                .timestamp_nanos
                .is_some_and(|timestamp| timestamp >= start_nanos)
    })
}

fn aggregate_usage<F, G>(
    selected: &HashSet<String>,
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
    includes: F,
    includes_ts: G,
    balance_baseline: Option<BalanceObservation>,
) -> WindowAggregate
where
    F: Fn(&TokenEvent) -> bool,
    G: Fn(Option<i64>) -> bool,
{
    let mut aggregate = WindowAggregate {
        reliable: true,
        ..WindowAggregate::default()
    };
    let mut groups: HashMap<String, Vec<&FileCache>> = HashMap::new();

    for key in selected {
        let Some(cache) = caches.get(key) else {
            aggregate.reliable = false;
            aggregate.deferred_files = aggregate.deferred_files.saturating_add(1);
            continue;
        };
        let has_usage = cache
            .events
            .iter()
            .any(|event| event.delta_total > 0 && includes(event));
        let Some(root) = cache.root.as_ref() else {
            if has_usage || cache.uncertain {
                aggregate.reliable = false;
                aggregate.deferred_files = aggregate.deferred_files.saturating_add(1);
            }
            continue;
        };
        if root.provider.as_deref() != Some("openai") {
            if root.provider.is_none() && has_usage {
                aggregate.reliable = false;
                aggregate.deferred_files = aggregate.deferred_files.saturating_add(1);
            }
            continue;
        }
        if cache.uncertain || cache.token_without_timestamp {
            aggregate.reliable = false;
            aggregate.deferred_files = aggregate.deferred_files.saturating_add(1);
            continue;
        }
        let Some(thread_id) = root.thread_id.as_ref() else {
            if has_usage {
                aggregate.reliable = false;
                aggregate.deferred_files = aggregate.deferred_files.saturating_add(1);
            }
            continue;
        };
        groups.entry(thread_id.clone()).or_default().push(cache);
    }

    let mut observations: Vec<(i64, f64)> = Vec::new();
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
            match replay_prefix(canonical, caches, rollout_index) {
                Ok(prefix) => {
                    tally_timeline(
                        canonical,
                        prefix,
                        &includes,
                        &mut aggregate,
                        &mut observations,
                    );
                }
                Err(()) => defer_timeline(canonical, &includes, &mut aggregate),
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
                aggregate.reliable = false;
                aggregate.deferred_files = aggregate.deferred_files.saturating_add(1);
            }
            continue;
        };
        for chain in chains {
            match replay_prefix(chain, caches, rollout_index) {
                Ok(prefix) => {
                    tally_timeline(chain, prefix, &includes, &mut aggregate, &mut observations);
                }
                Err(()) => defer_timeline(chain, &includes, &mut aggregate),
            }
        }
    }
    // 窗口内没有更早的观测时，持久化的账户余额基线（可能来自已被裁剪的
    // 旧文件）作为时间线起点播种——“空档后的下降归入首次观测日”靠它落地。
    if let Some(baseline) = balance_baseline {
        observations.push((baseline.timestamp_nanos, baseline.balance));
    }
    // 余额观测来自不同线程组的并行会话，必须先按时间排序成账户级时间线，
    // 否则同一笔扣费的重复观测无法相邻抵消。
    observations.sort_by_key(|(timestamp, _)| *timestamp);
    aggregate.credits_spent = credits_debits(&observations, &includes_ts);
    aggregate
}

/// credits 余额观测合并成账户级时间线后取负跳变：并行会话对同一笔扣费的
/// 重复观测排序后同值相邻（差为 0），不会重复计入；赠送的正跳变不计。
/// 扣费归属到观测到它的后一个事件所在窗口——长空档后看到的下降（其他
/// 设备消耗）因此落在空档后的第一个事件上，与实测行为一致。
///
/// 阈值只挡浮点噪声：余额字符串带 10 位小数，同值重复观测的差分精确为 0，
/// 解析误差量级 ~1e-12；真实扣费实测最小也在 1e-3 credits 量级。原 0.005
/// 的阈值会整段丢弃小额扣费且不累计余量，已按原始精度保留。
const BALANCE_EPSILON: f64 = 1e-6;

fn credits_debits<G>(observations: &[(i64, f64)], includes_ts: &G) -> f64
where
    G: Fn(Option<i64>) -> bool,
{
    let mut spent = 0.0;
    for pair in observations.windows(2) {
        let delta = pair[1].1 - pair[0].1;
        if delta < -BALANCE_EPSILON && includes_ts(Some(pair[1].0)) {
            spent -= delta;
        }
    }
    spent
}

fn defer_timeline<F>(cache: &FileCache, includes: &F, aggregate: &mut WindowAggregate)
where
    F: Fn(&TokenEvent) -> bool,
{
    if cache
        .events
        .iter()
        .any(|event| event.delta_total > 0 && includes(event))
    {
        aggregate.reliable = false;
        aggregate.deferred_files = aggregate.deferred_files.saturating_add(1);
    }
}

/// 服务端把窗口进度封顶在 100（10k+ 真实事件无一越界）；触顶即视为满。
fn window_capped(event: &TokenEvent) -> bool {
    const CAPPED_PERCENT: f64 = 99.99;
    event
        .primary_percent
        .is_some_and(|percent| percent >= CAPPED_PERCENT)
        || event
            .secondary_percent
            .is_some_and(|percent| percent >= CAPPED_PERCENT)
}

/// 遍历一条时间线：余额观测取自文件级采集列表（含重放区域，排序合并后
/// 自然去重），用量按溢出标记分计。溢出判定：前一事件报告触顶且本事件
/// 仍触顶——溢出用量不推进冻结的进度；进度回落说明窗口已被服务端重置，
/// 其后的事件回到套餐内。时间线首事件没有前值，以自身报告触顶为准（覆盖
/// 文件从溢出中段开始写入的场景）；跨界请求（把进度从 <100 顶到 100）按
/// 套餐内计，误差以一个请求为界。
fn tally_timeline<F>(
    cache: &FileCache,
    prefix: usize,
    includes: &F,
    aggregate: &mut WindowAggregate,
    observations: &mut Vec<(i64, f64)>,
) where
    F: Fn(&TokenEvent) -> bool,
{
    observations.extend(
        cache
            .balance_observations
            .iter()
            .map(|observation| (observation.timestamp_nanos, observation.balance)),
    );
    let mut previous_capped = false;
    for (index, event) in cache.events.iter().enumerate() {
        let capped_now = window_capped(event);
        let overflow = if index == 0 {
            capped_now
        } else {
            previous_capped && capped_now
        };
        previous_capped = capped_now;
        if index < prefix || !includes(event) || event.delta_total == 0 {
            continue;
        }
        if overflow {
            aggregate.overflow_tokens = aggregate.overflow_tokens.saturating_add(event.delta_total);
        } else {
            aggregate.tokens = aggregate.tokens.saturating_add(event.delta_total);
            accumulate_volume(
                &mut aggregate.volumes,
                event.model.as_deref(),
                event.delta_uncached_input,
                event.delta_cached_input,
                event.delta_output,
            );
        }
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
