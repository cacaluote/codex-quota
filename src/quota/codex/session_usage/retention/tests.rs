use std::fs;
use std::os::windows::fs::OpenOptionsExt;

use serde_json::Value;

use super::super::parser::parse_timestamp_nanos;
use super::super::test_support::*;
use super::super::*;
use super::*;
use crate::quota::pricing::PriceTable;

fn boundary(context: &TestContext, seconds: i64) -> PeriodBoundary {
    PeriodBoundary::from_start(
        parse_timestamp_nanos(Some(&Value::String(context.at(seconds))))
            .and_then(system_time_from_unix_nanos)
            .unwrap(),
    )
    .unwrap()
}

fn baseline(
    context: &TestContext,
    date: &str,
    boundary: Option<&PeriodBoundary>,
) -> LocalUsageSnapshot {
    let mut tracker =
        SessionUsageTracker::with_paths(context.root.clone(), context.root.join("baseline.json"));
    tracker.cache_path = None;
    tracker.refresh_for_period(date, boundary).unwrap().0
}

fn normalize(mut snapshot: LocalUsageSnapshot) -> LocalUsageSnapshot {
    for volumes in [&mut snapshot.today_volume, &mut snapshot.period_volume]
        .into_iter()
        .flatten()
    {
        volumes.0.sort_by_cached_key(|entry| format!("{entry:?}"));
    }
    snapshot
}

fn assert_same(actual: LocalUsageSnapshot, expected: LocalUsageSnapshot, label: &str) {
    let actual = normalize(actual);
    let expected = normalize(expected);
    let prices = PriceTable::from_pricing_md(crate::quota::pricing::OFFICIAL_PRICING).unwrap();
    for (actual, expected) in [
        (&actual.today_volume, &expected.today_volume),
        (&actual.period_volume, &expected.period_volume),
    ] {
        assert_eq!(
            actual.as_ref().and_then(|value| value.cost(&prices)),
            expected.as_ref().and_then(|value| value.cost(&prices)),
            "{label}: cost"
        );
        assert_eq!(
            actual
                .as_ref()
                .and_then(ModelVolumes::cache_hit_percent_tenths),
            expected
                .as_ref()
                .and_then(ModelVolumes::cache_hit_percent_tenths),
            "{label}: hit rate"
        );
    }
    assert_eq!(actual, expected, "{label}: complete snapshot");
}

fn history(context: &TestContext) -> Vec<Value> {
    let mut values = vec![
        session_meta(&context.at(-300_000), PARENT_ID, Some("openai"), None),
        turn_context_model(&context.at(-300_000), "gpt-6.1-sol"),
    ];
    for (seconds, total, last, source) in [
        (-299_999, 500, Some(100), Some("codex")),
        (-299_998, 400, Some(100), Some("codex")),
        (-299_997, 200, Some(200), Some("other")),
        (-299_996, 550, Some(50), Some("codex")),
        (-299_995, 300, Some(300), None),
        (-299_994, 450, Some(100), Some("codex")),
        (-299_993, 150, Some(50), Some("other")),
    ] {
        values.push(token_count(&context.at(seconds), total, last, source));
    }
    values
}

#[test]
fn checkpoints_preserve_append_and_restart() {
    let context = TestContext::new("retention-resume");
    let file = context.rollout(PARENT_ID);
    write_jsonl(&file, &history(&context));
    let start = boundary(&context, 0);
    let mut tracker = context.tracker();
    assert_same(
        tracker
            .refresh_for_period(&context.date, Some(&start))
            .unwrap()
            .0,
        baseline(&context, &context.date, Some(&start)),
        "initial",
    );
    assert_eq!(tracker.cache.files[0].events.len(), 4);
    for (index, (total, last, source)) in [
        (500, Some(50), Some("codex")), // recovery below the source peak
        (550, Some(50), Some("codex")), // repeated peak with different last
        (600, Some(50), Some("codex")),
        (150, Some(50), Some("other")),
        (250, Some(50), Some("other")),
        (300, Some(300), None),
        (650, None, Some("codex")), // cumulative fallback needs the old high-water
    ]
    .into_iter()
    .enumerate()
    {
        append_jsonl(
            &file,
            &token_count(
                &context.at(i64::try_from(index).unwrap() + 1),
                total,
                last,
                source,
            ),
        );
        if index % 2 == 0 {
            tracker = context.tracker();
        }
        assert_same(
            tracker
                .refresh_for_period(&context.date, Some(&start))
                .unwrap()
                .0,
            baseline(&context, &context.date, Some(&start)),
            &format!("append {index}"),
        );
    }
    tracker.enable_incremental_for_test();
    tracker
        .refresh_for_period(&context.date, Some(&start))
        .unwrap();
    append_jsonl(
        &file,
        &token_count(&context.at(20), 700, Some(50), Some("codex")),
    );
    tracker.mark_changed_for_test(file);
    assert_same(
        tracker
            .refresh_for_period(&context.date, Some(&start))
            .unwrap()
            .0,
        baseline(&context, &context.date, Some(&start)),
        "watcher incremental",
    );
}

#[test]
fn boundary_state_preserves_overflow_and_credits() {
    for previous_capped in [false, true] {
        let context = TestContext::new("retention-overflow");
        let file = context.rollout(PARENT_ID);
        let old = context.at(-300_000);
        let mut values = vec![
            session_meta(&old, PARENT_ID, Some("openai"), None),
            turn_context_model(&old, "gpt-6.1-sol"),
        ];
        for index in 0..20_u64 {
            values.push(token_count_with_rate_limits(
                &context.at(-299_999 + i64::try_from(index).unwrap()),
                100 + index * 10,
                Some(10),
                codex_rate_limits_with_credits(90.0, Some(0.0), Some("100.00")),
            ));
        }
        // A zero-delta snapshot changes the cap state AFTER the source peak.
        values.push(token_count_with_rate_limits(
            &context.at(-299_900),
            290,
            Some(10),
            codex_rate_limits_with_credits(
                if previous_capped { 100.0 } else { 90.0 },
                Some(0.0),
                Some("100.00"),
            ),
        ));
        write_jsonl(&file, &values);
        let mut tracker = context.tracker();
        let start = boundary(&context, 0);
        tracker
            .refresh_for_period(&context.date, Some(&start))
            .unwrap();
        for (seconds, total, percent, balance) in [
            (1, 300, 100.0, "90.00"),
            (2, 310, 100.0, "89.00"),
            (3, 320, 2.0, "89.00"),
        ] {
            append_jsonl(
                &file,
                &token_count_with_rate_limits(
                    &context.at(seconds),
                    total,
                    Some(10),
                    codex_rate_limits_with_credits(percent, Some(0.0), Some(balance)),
                ),
            );
        }
        let actual = context
            .tracker()
            .refresh_for_period(&context.date, Some(&start))
            .unwrap()
            .0;
        assert!((actual.today_credits_spent - 1.0).abs() < 1e-12);
        assert_eq!(
            actual.today_overflow_tokens,
            if previous_capped { 20 } else { 10 }
        );
        assert_same(
            actual,
            baseline(&context, &context.date, Some(&start)),
            "overflow boundary",
        );
    }
}

#[test]
fn model_tier_switches_preserve_pricing() {
    let context = TestContext::new("retention-models");
    let file = context.rollout(PARENT_ID);
    let mut values = history(&context);
    for (seconds, model, tier, input, cached) in [
        (-200_000, "gpt-6-astra", "ultrafast", 272_001, 200_000),
        (-190_000, "gpt-6.1-sol", "priority", 100, 90),
        (-180_000, "gpt-6.1-sol", "priority", 50, 45),
    ] {
        values.push(turn_context_model(&context.at(seconds), model));
        let mut settings = thread_settings_tier(&context.at(seconds), Some(tier));
        settings["payload"]["thread_settings"]["model"] = Value::String(model.to_owned());
        values.push(settings);
        values.push(token_count_buckets(
            &context.at(seconds + 1),
            input,
            cached,
            10,
            5,
            Some("codex"),
        ));
    }
    write_jsonl(&file, &values);
    let start = boundary(&context, 0);
    context
        .tracker()
        .refresh_for_period(&context.date, Some(&start))
        .unwrap();
    for (seconds, model, tier, input, cached) in [
        (1, "gpt-6.1-sol", "priority", 272_000, 200_000),
        (2, "gpt-6-astra", "ultrafast", 272_001, 200_000),
        (3, "gpt-6.1-sol", "default", 300_000, 100_000),
    ] {
        append_jsonl(&file, &turn_context_model(&context.at(seconds), model));
        append_jsonl(
            &file,
            &thread_settings_tier(&context.at(seconds), Some(tier)),
        );
        append_jsonl(
            &file,
            &token_count_buckets(&context.at(seconds), input, cached, 10, 5, Some("codex")),
        );
        assert_same(
            context
                .tracker()
                .refresh_for_period(&context.date, Some(&start))
                .unwrap()
                .0,
            baseline(&context, &context.date, Some(&start)),
            "model/tier/context change",
        );
    }
}

#[test]
fn expanded_window_restores_original_history() {
    let context = TestContext::new("retention-rollback");
    write_jsonl(&context.rollout(PARENT_ID), &history(&context));
    let current = boundary(&context, 0);
    context
        .tracker()
        .refresh_for_period(&context.date, Some(&current))
        .unwrap();
    let expanded = boundary(&context, -300_000);
    let mut tracker = context.tracker();
    assert_same(
        tracker
            .refresh_for_period(&context.date, Some(&expanded))
            .unwrap()
            .0,
        baseline(&context, &context.date, Some(&expanded)),
        "window moved backward",
    );
    assert_eq!(tracker.cache.files[0].events.len(), 7);
}

#[test]
fn new_fork_restores_parent_replay() {
    let context = TestContext::new("retention-fork");
    let file = context.rollout(PARENT_ID);
    let mut values = history(&context);
    values.push(turn_context_model(&context.at(-100), "gpt-6.1-sol"));
    write_jsonl(&file, &values);
    let start = boundary(&context, 0);
    context
        .tracker()
        .refresh_for_period(&context.date, Some(&start))
        .unwrap();
    let mut child = vec![
        session_meta(&context.at(-200), CHILD_ID, Some("openai"), Some(PARENT_ID)),
        turn_context_model(&context.at(-200), "gpt-6.1-sol"),
    ];
    // Replay all earlier signatures, but with new timestamps inside the window.
    for value in values.iter().filter(|value| {
        value.pointer("/payload/type").and_then(Value::as_str) == Some("token_count")
    }) {
        let mut replay = value.clone();
        replay["timestamp"] = Value::String(context.at(1));
        child.push(replay);
    }
    child.push(token_count(&context.at(2), 700, Some(150), Some("codex")));
    write_jsonl(&context.rollout(CHILD_ID), &child);
    let mut tracker = context.tracker();
    let actual = tracker
        .refresh_for_period(&context.date, Some(&start))
        .unwrap()
        .0;
    assert_eq!(actual.today_tokens, 150);
    assert_same(
        actual,
        baseline(&context, &context.date, Some(&start)),
        "new fork",
    );
    assert_eq!(
        tracker
            .cache
            .files
            .iter()
            .find(|file| file.path.ends_with(&format!("{PARENT_ID}.jsonl")))
            .unwrap()
            .events
            .len(),
        7
    );
}

#[test]
fn new_copy_restores_thread_deduplication() {
    for conflicting in [false, true] {
        let context = TestContext::new("retention-copy");
        let file = context.rollout(PARENT_ID);
        let mut values = history(&context);
        values.push(token_count(&context.at(1), 700, Some(150), Some("codex")));
        write_jsonl(&file, &values);
        let start = boundary(&context, 0);
        context
            .tracker()
            .refresh_for_period(&context.date, Some(&start))
            .unwrap();
        if conflicting {
            values[2]["payload"]["info"]["last_token_usage"]["total_tokens"] = Value::from(99);
        }
        write_jsonl(&context.archived_rollout(PARENT_ID), &values);
        assert_same(
            context
                .tracker()
                .refresh_for_period(&context.date, Some(&start))
                .unwrap()
                .0,
            baseline(&context, &context.date, Some(&start)),
            "new archived copy",
        );
    }
}

#[test]
fn archive_and_truncation_preserve_continuation() {
    let context = TestContext::new("retention-file-lifecycle");
    let original = context.rollout(PARENT_ID);
    let archived = context.archived_rollout(PARENT_ID);
    write_jsonl(&original, &history(&context));
    let start = boundary(&context, 0);
    let mut tracker = context.tracker();
    tracker
        .refresh_for_period(&context.date, Some(&start))
        .unwrap();
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::rename(&original, &archived).unwrap();
    append_jsonl(
        &archived,
        &token_count(&context.at(1), 600, Some(50), Some("codex")),
    );
    assert_same(
        tracker
            .refresh_for_period(&context.date, Some(&start))
            .unwrap()
            .0,
        baseline(&context, &context.date, Some(&start)),
        "archived and appended",
    );
    write_jsonl(
        &archived,
        &[
            session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
            turn_context_model(&context.at(0), "gpt-6.1-sol"),
            token_count(&context.at(2), 50, Some(50), Some("codex")),
        ],
    );
    assert_same(
        context
            .tracker()
            .refresh_for_period(&context.date, Some(&start))
            .unwrap()
            .0,
        baseline(&context, &context.date, Some(&start)),
        "truncated and replaced",
    );
}

#[test]
fn quota_arrival_and_day_change_restore_history() {
    let context = TestContext::new("retention-scope-changes");
    let file = context.rollout(PARENT_ID);
    let mut values = history(&context);
    values.push(token_count(
        &context.at(-80_000),
        600,
        Some(50),
        Some("codex"),
    ));
    values.push(token_count(&context.at(1), 650, Some(50), Some("codex")));
    write_jsonl(&file, &values);
    let mut tracker = context.tracker();
    assert_same(
        tracker.refresh_for_period(&context.date, None).unwrap().0,
        baseline(&context, &context.date, None),
        "before quota boundary arrived",
    );
    let start = boundary(&context, -300_000);
    assert_same(
        context
            .tracker()
            .refresh_for_period(&context.date, Some(&start))
            .unwrap()
            .0,
        baseline(&context, &context.date, Some(&start)),
        "quota boundary arrived",
    );
    let tomorrow = boundary(&context, 86_400).local_date().to_owned();
    assert_same(
        context
            .tracker()
            .refresh_for_period(&tomorrow, None)
            .unwrap()
            .0,
        baseline(&context, &tomorrow, None),
        "date advanced",
    );
    assert_same(
        context
            .tracker()
            .refresh_for_period(&context.date, Some(&start))
            .unwrap()
            .0,
        baseline(&context, &context.date, Some(&start)),
        "date moved backward",
    );
}

#[test]
fn out_of_order_and_parallel_balances_agree() {
    let context = TestContext::new("retention-order-balances");
    let file = context.rollout(PARENT_ID);
    let mut values = history(&context);
    // Stop prefix trimming at the first in-window timestamp. Later historical
    // records must stay in file order, including cap changes and signatures.
    for (seconds, total, balance) in [
        (1, 600, "50.00"),
        (-100_000, 650, "60.00"),
        (3, 700, "49.50"),
        (5, 750, "49.20"),
    ] {
        values.push(token_count_with_rate_limits(
            &context.at(seconds),
            total,
            Some(50),
            codex_rate_limits_with_credits(50.0, Some(20.0), Some(balance)),
        ));
    }
    write_jsonl(&file, &values);
    write_jsonl(
        &context.rollout(CHILD_ID),
        &[
            session_meta(&context.at(-200_000), CHILD_ID, Some("openai"), None),
            turn_context_model(&context.at(-200_000), "gpt-6.1-sol"),
            token_count_with_rate_limits(
                &context.at(-199_999),
                100,
                Some(100),
                codex_rate_limits_with_credits(40.0, Some(10.0), Some("70.00")),
            ),
            token_count_with_rate_limits(
                &context.at(2),
                150,
                Some(50),
                codex_rate_limits_with_credits(50.0, Some(20.0), Some("49.80")),
            ),
            token_count_with_rate_limits(
                &context.at(4),
                200,
                Some(50),
                codex_rate_limits_with_credits(50.0, Some(20.0), Some("49.30")),
            ),
        ],
    );
    let start = boundary(&context, 0);
    context
        .tracker()
        .refresh_for_period(&context.date, Some(&start))
        .unwrap();
    assert_same(
        context
            .tracker()
            .refresh_for_period(&context.date, Some(&start))
            .unwrap()
            .0,
        baseline(&context, &context.date, Some(&start)),
        "out of order and parallel credits",
    );
}

#[test]
fn unknown_period_defers_history_pruning() {
    let context = TestContext::new("retention-await-period");
    write_jsonl(&context.rollout(PARENT_ID), &history(&context));
    let mut tracker = context.tracker();
    let first = tracker.refresh_for_date(&context.date).unwrap();
    assert_eq!(first.1.token_events_pruned, 0);
    assert_eq!(tracker.cache.files[0].events.len(), 7);
    assert_eq!(tracker.cache.files[0].history_start_nanos, None);
    let start = boundary(&context, 0);
    let known = tracker
        .refresh_for_period(&context.date, Some(&start))
        .unwrap();
    assert_eq!(
        (known.1.history_files_restored, known.1.token_events_pruned),
        (0, 3)
    );
    assert!(tracker.cache.files[0].history_start_nanos.is_some());
    let repeated = tracker
        .refresh_for_period(&context.date, Some(&start))
        .unwrap();
    assert_eq!(
        (repeated.1.files_read, repeated.1.token_events_pruned),
        (0, 0)
    );
    assert!(repeated.1.cache_write_skipped);
    let wider = boundary(&context, -300_000);
    let restored = tracker
        .refresh_for_period(&context.date, Some(&wider))
        .unwrap();
    assert_eq!(restored.1.history_files_restored, 1);
    assert_same(
        restored.0,
        baseline(&context, &context.date, Some(&wider)),
        "period expanded",
    );
}

#[test]
fn failed_restore_reports_unknown_then_recovers() {
    for fork in [false, true] {
        let context = TestContext::new("retention-read-failure");
        let file = context.rollout(PARENT_ID);
        let mut values = history(&context);
        values.push(turn_context_model(&context.at(-100), "gpt-6.1-sol"));
        write_jsonl(&file, &values);
        let start = boundary(&context, 0);
        context
            .tracker()
            .refresh_for_period(&context.date, Some(&start))
            .unwrap();
        let requested = if fork {
            write_jsonl(
                &context.rollout(CHILD_ID),
                &[
                    session_meta(&context.at(-200), CHILD_ID, Some("openai"), Some(PARENT_ID)),
                    turn_context_model(&context.at(-200), "gpt-6.1-sol"),
                    token_count(&context.at(1), 500, Some(100), Some("codex")),
                    token_count(&context.at(2), 600, Some(100), Some("codex")),
                ],
            );
            start
        } else {
            boundary(&context, -300_000)
        };
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&file)
            .unwrap();
        let mut tracker = context.tracker();
        let failed = tracker
            .refresh_for_period(&context.date, Some(&requested))
            .unwrap();
        assert!(!failed.0.current_period_reliable, "fork={fork}");
        assert_eq!(failed.0.period_volume, None, "fork={fork}");
        assert_eq!(failed.1.history_files_restored, 0, "fork={fork}");
        assert!(failed.1.parse_errors > 0, "fork={fork}");
        assert!(
            tracker
                .cache
                .files
                .iter()
                .any(|file| file.history_start_nanos.is_some()),
            "fork={fork}"
        );
        drop(locked);
        let recovered = tracker
            .refresh_for_period(&context.date, Some(&requested))
            .unwrap();
        assert_eq!(recovered.1.history_files_restored, 1, "fork={fork}");
        assert_same(
            recovered.0,
            baseline(&context, &context.date, Some(&requested)),
            "retry recovery",
        );
    }
}

#[test]
fn stale_metadata_still_selects_restored_usage() {
    let context = TestContext::new("retention-cached-selection");
    let file = context.rollout(PARENT_ID);
    write_jsonl(&file, &history(&context));
    context
        .tracker()
        .refresh_for_period(&context.date, Some(&boundary(&context, 0)))
        .unwrap();
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
        .unwrap();
    let expanded = boundary(&context, -300_000);
    let restored = context
        .tracker()
        .refresh_for_period(&context.date, Some(&expanded))
        .unwrap();
    assert_eq!(restored.1.history_files_restored, 1);
    assert_eq!(restored.0.current_period_tokens, 650);
    assert!(restored.0.current_period_reliable);
    assert_eq!(
        context
            .tracker()
            .refresh_for_period(&context.date, Some(&expanded))
            .unwrap()
            .0
            .current_period_tokens,
        650
    );
}

#[test]
fn archived_cache_restores_expanded_window() {
    let context = TestContext::new("retention-archive-expansion");
    let original = context.rollout(PARENT_ID);
    let archived = context.archived_rollout(PARENT_ID);
    write_jsonl(&original, &history(&context));
    context
        .tracker()
        .refresh_for_period(&context.date, Some(&boundary(&context, 0)))
        .unwrap();
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::rename(original, archived).unwrap();
    let expanded = boundary(&context, -300_000);
    let restored = context
        .tracker()
        .refresh_for_period(&context.date, Some(&expanded))
        .unwrap();
    assert_eq!(restored.1.history_files_restored, 1);
    assert_eq!(restored.0.current_period_tokens, 650);
    assert_same(
        restored.0,
        baseline(&context, &context.date, Some(&expanded)),
        "archived and expanded",
    );
}

#[test]
fn archived_checkpoints_preserve_cached_selection() {
    let context = TestContext::new("retention-archive-cached-timestamps");
    let original = context.rollout(PARENT_ID);
    let archived = context.archived_rollout(PARENT_ID);
    let mut values = history(&context);
    values.push(token_count(&context.at(1), 600, Some(50), Some("codex")));
    write_jsonl(&original, &values);
    fs::File::options()
        .write(true)
        .open(&original)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
        .unwrap();
    // Seed the real loader with an already parsed cache. Server timestamps
    // can lead mtime, so cached events must continue to select this file.
    let discovered = discover_candidates(&context.root).unwrap();
    let mut caches = HashMap::new();
    update_candidate_cache(
        &discovered.candidates[0],
        &mut caches,
        &mut RefreshDiagnostics::default(),
    );
    let mut cache = UsageCacheV1::empty(Some(&context.root));
    cache.files = caches.into_values().collect();
    save_cache(&context.cache, &cache).unwrap();
    let start = boundary(&context, 0);
    let initial = context
        .tracker()
        .refresh_for_period(&context.date, Some(&start))
        .unwrap();
    assert_eq!(
        (initial.0.today_tokens, initial.1.token_events_pruned),
        (50, 3)
    );
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::rename(original, archived).unwrap();
    let renamed = context
        .tracker()
        .refresh_for_period(&context.date, Some(&start))
        .unwrap();
    assert_eq!(
        (renamed.0.today_tokens, renamed.0.current_period_tokens),
        (50, 50)
    );
    assert_eq!(renamed.1.files_read, 0);
    assert_same(
        renamed.0,
        initial.0,
        "cached selection after archive rename",
    );
}

#[test]
fn uncertain_or_linked_files_keep_history() {
    for (label, provider, parent, untimed) in [
        ("unknown provider", None, None, false),
        ("other provider", Some("other"), None, false),
        ("ambiguous link", Some("openai"), Some(PARENT_ID), false),
        ("missing timestamp", Some("openai"), None, true),
    ] {
        let context = TestContext::new("retention-conservative");
        let mut values = history(&context);
        values[0] = session_meta(&context.at(-300_000), PARENT_ID, provider, parent);
        if untimed {
            values[2].as_object_mut().unwrap().remove("timestamp");
        }
        write_jsonl(&context.rollout(PARENT_ID), &values);
        let mut tracker = context.tracker();
        let result = tracker
            .refresh_for_period(&context.date, Some(&boundary(&context, 0)))
            .unwrap();
        assert_eq!(result.1.token_events_pruned, 0, "{label}");
        assert_eq!(tracker.cache.files[0].events.len(), 7, "{label}");
        assert_eq!(tracker.cache.files[0].history_start_nanos, None, "{label}");
    }
}

#[test]
fn first_required_day_keeps_coverage_anchor() {
    let context = TestContext::new("retention-first-day-anchor");
    let start = boundary(&context, -172_800);
    let midnight = required_start_nanos(&context.date, Some(&start)).unwrap();
    let timestamp = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(midnight))
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let mut values = history(&context);
    values.push(token_count(&timestamp, 600, Some(50), Some("codex")));
    values.push(token_count(&context.at(1), 650, Some(50), Some("codex")));
    write_jsonl(&context.rollout(PARENT_ID), &values);
    let expected = baseline(&context, &context.date, Some(&start));
    assert!(expected.current_period_reliable);
    context
        .tracker()
        .refresh_for_period(&context.date, Some(&start))
        .unwrap();
    assert_same(
        context
            .tracker()
            .refresh_for_period(&context.date, Some(&start))
            .unwrap()
            .0,
        expected,
        "first-day coverage after restart",
    );
}

#[test]
fn local_midnight_and_invalid_dates() {
    for date in ["2026-09-27", "2026-10-03", "2024-02-29"] {
        let start = local_midnight_nanos(date).unwrap();
        assert_eq!(
            local_calendar_date_at(system_time_from_unix_nanos(start).unwrap()).as_deref(),
            Some(date)
        );
        assert_ne!(
            local_calendar_date_at(system_time_from_unix_nanos(start - 100).unwrap()).as_deref(),
            Some(date)
        );
    }
    for date in [
        "",
        "2026-02-30",
        "2026-13-01",
        "2026-10-03-extra",
        "1900-01-01",
    ] {
        assert_eq!(local_midnight_nanos(date), None, "{date}");
    }
}

#[test]
fn previous_cache_version_rebuilds_history() {
    let context = TestContext::new("retention-version-rebuild");
    write_jsonl(&context.rollout(PARENT_ID), &history(&context));
    let start = boundary(&context, 0);
    context
        .tracker()
        .refresh_for_period(&context.date, Some(&start))
        .unwrap();
    let mut cached: Value = serde_json::from_slice(&fs::read(&context.cache).unwrap()).unwrap();
    cached["version"] = Value::from(26_100_201_u32);
    cached["files"][0]["events"][0]["delta_total"] = Value::from(99_999);
    cached["files"][0]
        .as_object_mut()
        .unwrap()
        .remove("history_start_nanos");
    fs::write(&context.cache, serde_json::to_vec(&cached).unwrap()).unwrap();
    let rebuilt = context
        .tracker()
        .refresh_for_period(&context.date, Some(&boundary(&context, -300_000)))
        .unwrap();
    assert_eq!(rebuilt.1.files_read, 1);
    assert_eq!(rebuilt.0.current_period_tokens, 650);
    assert_eq!(load_cache(&context.cache).unwrap().version, CACHE_VERSION);
}

#[test]
#[ignore = "Reads a fixed snapshot of local session logs; run explicitly"]
fn real_snapshot_preserves_usage_and_size() {
    let source_dir = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("codex-quota");
    let source = ["usage-cache.json", "usage-cache-v1.json"]
        .into_iter()
        .map(|name| source_dir.join(name))
        .find(|path| path.is_file())
        .unwrap();
    let source_cache = load_cache(&source).unwrap();
    let context = TestContext::new("retention-real-snapshot");
    let original_home = Path::new(&source_cache.codex_home);
    for file in &source_cache.files {
        let path = Path::new(&file.path);
        let destination = context.root.join(path.strip_prefix(original_home).unwrap());
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::copy(path, destination).unwrap();
    }
    let limits = source_cache
        .latest_rate_limits
        .as_ref()
        .unwrap()
        .secondary
        .as_ref()
        .unwrap();
    let start = PeriodBoundary::from_start(
        UNIX_EPOCH
            + Duration::from_secs(
                u64::try_from(limits.resets_at).unwrap() - limits.window_minutes * 60,
            ),
    )
    .unwrap();
    let mut scan =
        ScanContext::new_full(&context.root, &source_cache.date, Some(&start), Vec::new()).unwrap();
    scan.parse_selected_and_dependencies(&source_cache.date, Some(&start));
    let selected = scan.selected();
    let mut retained = selected;
    retained.extend(scan.dependencies.iter().cloned());
    let mut full = UsageCacheV1::empty(Some(&context.root));
    full.date.clone_from(&source_cache.date);
    full.files = scan
        .caches
        .iter()
        .filter(|(path, _)| retained.contains(*path))
        .map(|(_, file)| file.clone())
        .collect();
    full.latest_rate_limits = scan.derive_quota(None).latest_rate_limits;
    let before = serde_json::to_vec(&full).unwrap().len();
    let events_before: usize = full.files.iter().map(|file| file.events.len()).sum();
    let result = scan.finish(&source_cache.date, Some(&start), None, None);
    let expected = result.snapshot;
    full.files = result.files;
    let after = serde_json::to_vec(&full).unwrap().len();
    let events_after: usize = full.files.iter().map(|file| file.events.len()).sum();
    assert!(events_after < events_before);
    save_cache(&context.cache, &full).unwrap();
    let restarted = context
        .tracker()
        .refresh_for_period(&source_cache.date, Some(&start))
        .unwrap();
    assert_eq!(restarted.1.files_read, 0);
    assert_same(restarted.0, expected.clone(), "real cached restart");
    assert_same(
        baseline(&context, &source_cache.date, Some(&start)),
        expected,
        "real cold scan",
    );
    eprintln!(
        "RETENTION bytes={before}->{after} events={events_before}->{events_after} files={} boundary={}",
        full.files.len(),
        start.local_date()
    );
}
