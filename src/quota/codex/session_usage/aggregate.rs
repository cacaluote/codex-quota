use std::collections::{HashMap, HashSet};

use super::model::{FileCache, ParentLink, TokenEvent, TokenSignature};
use super::parser::event_is_on_date;

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
) -> (u64, bool, usize) {
    aggregate_usage(selected, caches, rollout_index, |event| {
        event_is_on_date(event, date)
    })
}

pub(super) fn aggregate_period(
    start_nanos: i64,
    selected: &HashSet<String>,
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
) -> (u64, bool, usize) {
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

fn aggregate_usage<F>(
    selected: &HashSet<String>,
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
    includes: F,
) -> (u64, bool, usize)
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
    for files in groups.values_mut() {
        files.sort_by_key(|file| std::cmp::Reverse(file.events.len()));
        let Some(canonical) = files.first().copied() else {
            continue;
        };
        if files
            .iter()
            .skip(1)
            .any(|other| !event_prefix_matches(&other.events, &canonical.events))
        {
            if files.iter().any(|file| {
                file.events
                    .iter()
                    .any(|event| event.delta_total > 0 && includes(event))
            }) {
                reliable = false;
                deferred_files = deferred_files.saturating_add(1);
            }
            continue;
        }
        match replay_prefix(canonical, caches, rollout_index) {
            Ok(prefix) => {
                total = canonical
                    .events
                    .iter()
                    .skip(prefix)
                    .fold(total, |sum, event| {
                        if includes(event) {
                            sum.saturating_add(event.delta_total)
                        } else {
                            sum
                        }
                    });
            }
            Err(()) => {
                if canonical
                    .events
                    .iter()
                    .any(|event| event.delta_total > 0 && includes(event))
                {
                    reliable = false;
                    deferred_files = deferred_files.saturating_add(1);
                }
            }
        }
    }
    (total, reliable, deferred_files)
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

        let result = context.tracker().refresh_for_date(&context.date);

        assert_eq!(result.ok().map(|value| value.0.today_tokens), Some(150));
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
