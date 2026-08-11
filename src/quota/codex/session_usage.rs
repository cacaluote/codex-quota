mod aggregate;
mod files;
mod model;
mod parser;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use aggregate::{aggregate_period_boundary, aggregate_today, cache_has_tokens_on_date};
use files::{
    codex_home, discover_candidates, is_date_partition, load_cache, modified_on_date,
    modified_on_or_after_date, path_key, save_cache,
};
use model::{CandidateFile, FileCache, ParentLink, UsageCacheV1};
use parser::{event_is_on_date, system_time_from_unix_nanos, update_candidate_cache};

use super::PeriodBoundary;
use super::protocol::{local_calendar_date, local_calendar_date_at};

const CACHE_VERSION: u32 = 1;
const CACHE_FILENAME: &str = "usage-cache-v1.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalUsageSnapshot {
    pub(super) today_tokens: u64,
    pub(super) today_reliable: bool,
    pub(super) period_boundary_tokens: u64,
    pub(super) period_boundary_reliable: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RefreshDiagnostics {
    pub(super) files_scanned: usize,
    pub(super) files_read: usize,
    pub(super) token_events: usize,
    pub(super) parse_errors: usize,
    pub(super) discovery_errors: usize,
    pub(super) deferred_files: usize,
    pub(super) cache_write_failed: bool,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum SessionUsageError {
    #[error("无法定位 Codex 会话目录")]
    CodexHomeUnavailable,
    #[error("无法读取 Codex 会话目录：{0}")]
    Discovery(#[source] io::Error),
}

#[derive(Debug)]
pub(super) struct SessionUsageTracker {
    codex_dir: Option<PathBuf>,
    cache_path: Option<PathBuf>,
    cache: UsageCacheV1,
}

struct ScanContext {
    candidate_by_path: HashMap<String, CandidateFile>,
    rollout_index: HashMap<String, Vec<String>>,
    caches: HashMap<String, FileCache>,
    today_selected: HashSet<String>,
    boundary_selected: HashSet<String>,
    dependencies: HashSet<String>,
    diagnostics: RefreshDiagnostics,
}

impl ScanContext {
    fn new(
        codex_dir: &Path,
        today: &str,
        boundary: Option<&PeriodBoundary>,
        cached_files: Vec<FileCache>,
    ) -> Result<Self, SessionUsageError> {
        let discovery = discover_candidates(codex_dir)?;
        let candidates = discovery.candidates;
        let candidate_by_path: HashMap<String, CandidateFile> = candidates
            .iter()
            .cloned()
            .map(|candidate| (path_key(&candidate.path), candidate))
            .collect();
        let mut rollout_index: HashMap<String, Vec<String>> = HashMap::new();
        for candidate in &candidates {
            if let Some(thread_id) = candidate.thread_id.as_ref() {
                rollout_index
                    .entry(thread_id.clone())
                    .or_default()
                    .push(path_key(&candidate.path));
            }
        }
        for paths in rollout_index.values_mut() {
            paths.sort();
        }

        let caches: HashMap<String, FileCache> = cached_files
            .into_iter()
            .map(|cache| (cache.path.clone(), cache))
            .collect();
        let today_selected = candidates
            .iter()
            .filter_map(|candidate| {
                let key = path_key(&candidate.path);
                let cached_today = caches
                    .get(&key)
                    .is_some_and(|cache| cache_has_tokens_on_date(cache, today));
                (cached_today
                    || is_date_partition(&candidate.path, today)
                    || modified_on_date(candidate, today))
                .then_some(key)
            })
            .collect();
        let boundary_selected = boundary.map_or_else(HashSet::new, |boundary| {
            candidates
                .iter()
                .filter_map(|candidate| {
                    let key = path_key(&candidate.path);
                    let cached_boundary = caches.get(&key).is_some_and(|cache| {
                        cache_has_tokens_on_date(cache, boundary.local_date())
                    });
                    (cached_boundary
                        || is_date_partition(&candidate.path, boundary.local_date())
                        || modified_on_or_after_date(candidate, boundary.local_date()))
                    .then_some(key)
                })
                .collect()
        });

        Ok(Self {
            candidate_by_path,
            rollout_index,
            caches,
            today_selected,
            boundary_selected,
            dependencies: HashSet::new(),
            diagnostics: RefreshDiagnostics {
                files_scanned: candidates.len(),
                discovery_errors: discovery.errors,
                ..RefreshDiagnostics::default()
            },
        })
    }

    fn parse_selected_and_dependencies(&mut self) {
        let initial_selected: Vec<String> = self
            .today_selected
            .union(&self.boundary_selected)
            .cloned()
            .collect();
        for key in &initial_selected {
            if let Some(candidate) = self.candidate_by_path.get(key) {
                update_candidate_cache(candidate, &mut self.caches, &mut self.diagnostics);
            }
        }

        let mut pending = initial_selected;
        while let Some(key) = pending.pop() {
            let Some(ParentLink::Parent(parent_id)) = self
                .caches
                .get(&key)
                .and_then(|cache| cache.root.as_ref())
                .map(|root| &root.parent)
            else {
                continue;
            };
            let Some(parent_paths) = self.rollout_index.get(parent_id) else {
                continue;
            };
            for parent_key in parent_paths {
                if self.dependencies.insert(parent_key.clone())
                    && let Some(candidate) = self.candidate_by_path.get(parent_key)
                {
                    update_candidate_cache(candidate, &mut self.caches, &mut self.diagnostics);
                    pending.push(parent_key.clone());
                }
            }
        }
    }

    fn finish(
        mut self,
        today: &str,
        boundary: Option<&PeriodBoundary>,
    ) -> (LocalUsageSnapshot, RefreshDiagnostics, Vec<FileCache>) {
        let (today_tokens, today_aggregate_reliable, today_deferred) = aggregate_today(
            today,
            &self.today_selected,
            &self.caches,
            &self.rollout_index,
        );
        let today_reliable = today_aggregate_reliable && self.diagnostics.discovery_errors == 0;
        let (period_boundary_tokens, period_boundary_reliable, boundary_deferred) = boundary
            .map_or((0, false, 0), |boundary| {
                let has_openai_file = self.boundary_selected.iter().any(|key| {
                    self.caches.get(key).is_some_and(|cache| {
                        cache.root.as_ref().is_some_and(|root| {
                            root.provider.as_deref() == Some("openai")
                                && (root
                                    .timestamp_nanos
                                    .and_then(system_time_from_unix_nanos)
                                    .and_then(local_calendar_date_at)
                                    .as_deref()
                                    == Some(boundary.local_date())
                                    || cache.events.iter().any(|event| {
                                        event_is_on_date(event, boundary.local_date())
                                    }))
                        })
                    })
                });
                let (tokens, reliable, deferred) = aggregate_period_boundary(
                    boundary.local_date(),
                    boundary.start_nanos(),
                    &self.boundary_selected,
                    &self.caches,
                    &self.rollout_index,
                );
                (
                    tokens,
                    has_openai_file && reliable && self.diagnostics.discovery_errors == 0,
                    deferred,
                )
            });
        self.diagnostics.deferred_files = today_deferred.saturating_add(boundary_deferred);
        let selected: HashSet<_> = self
            .today_selected
            .union(&self.boundary_selected)
            .cloned()
            .collect();
        self.diagnostics.token_events = selected
            .iter()
            .filter_map(|key| self.caches.get(key))
            .map(|cache| cache.events.len())
            .sum();
        self.diagnostics.parse_errors = selected
            .iter()
            .filter_map(|key| self.caches.get(key))
            .map(|cache| cache.parse_errors)
            .sum();

        let mut retained = selected;
        retained.extend(self.dependencies);
        self.caches
            .retain(|key, _| retained.contains(key) && self.candidate_by_path.contains_key(key));
        let mut files: Vec<_> = self.caches.into_values().collect();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        (
            LocalUsageSnapshot {
                today_tokens,
                today_reliable,
                period_boundary_tokens,
                period_boundary_reliable,
            },
            self.diagnostics,
            files,
        )
    }
}

impl SessionUsageTracker {
    pub(super) fn new() -> Self {
        let codex_dir = codex_home();
        let cache_path = crate::config::app_data_dir()
            .ok()
            .map(|directory| directory.join(CACHE_FILENAME));
        let cache = load_valid_cache(cache_path.as_deref(), codex_dir.as_deref());
        Self {
            codex_dir,
            cache_path,
            cache,
        }
    }

    #[cfg(test)]
    fn with_paths(codex_dir: PathBuf, cache_path: PathBuf) -> Self {
        let cache = load_valid_cache(Some(&cache_path), Some(&codex_dir));
        Self {
            cache,
            codex_dir: Some(codex_dir),
            cache_path: Some(cache_path),
        }
    }

    pub(super) fn refresh(
        &mut self,
        period_boundary: Option<&PeriodBoundary>,
    ) -> Result<(LocalUsageSnapshot, RefreshDiagnostics), SessionUsageError> {
        self.refresh_for_period(&local_calendar_date(), period_boundary)
    }

    #[cfg(test)]
    fn refresh_for_date(
        &mut self,
        date: &str,
    ) -> Result<(LocalUsageSnapshot, RefreshDiagnostics), SessionUsageError> {
        self.refresh_for_period(date, None)
    }

    fn refresh_for_period(
        &mut self,
        date: &str,
        period_boundary: Option<&PeriodBoundary>,
    ) -> Result<(LocalUsageSnapshot, RefreshDiagnostics), SessionUsageError> {
        let Some(codex_dir) = self.codex_dir.as_deref() else {
            return Err(SessionUsageError::CodexHomeUnavailable);
        };
        if !codex_dir.is_dir() {
            return Err(SessionUsageError::CodexHomeUnavailable);
        }

        let mut scan = ScanContext::new(
            codex_dir,
            date,
            period_boundary,
            std::mem::take(&mut self.cache.files),
        )?;
        scan.parse_selected_and_dependencies();
        let (snapshot, mut diagnostics, files) = scan.finish(date, period_boundary);
        self.cache.version = CACHE_VERSION;
        self.cache.codex_home = path_key(codex_dir);
        date.clone_into(&mut self.cache.date);
        self.cache.files = files;
        if let Some(cache_path) = self.cache_path.as_deref()
            && save_cache(cache_path, &self.cache).is_err()
        {
            diagnostics.cache_write_failed = true;
        }

        Ok((snapshot, diagnostics))
    }
}

fn load_valid_cache(cache_path: Option<&Path>, codex_dir: Option<&Path>) -> UsageCacheV1 {
    cache_path
        .and_then(load_cache)
        .filter(|cache| {
            cache.version == CACHE_VERSION
                && cache.codex_home == codex_dir.map_or_else(String::new, path_key)
        })
        .unwrap_or_else(|| UsageCacheV1::empty(codex_dir))
}

#[cfg(test)]
mod test_support {
    use std::fs::{self, OpenOptions};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::{Value, json};
    use time::format_description::well_known::Rfc3339;
    use time::{Duration as TimeDuration, OffsetDateTime};

    use super::parser::{parse_timestamp_nanos, system_time_from_unix_nanos};
    use super::*;
    use crate::quota::codex::protocol::local_calendar_date_at;

    pub(super) const PARENT_ID: &str = "11111111-1111-4111-8111-111111111111";
    pub(super) const CHILD_ID: &str = "22222222-2222-4222-8222-222222222222";
    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    pub(super) struct TestContext {
        pub(super) root: PathBuf,
        pub(super) cache: PathBuf,
        pub(super) timestamp: String,
        pub(super) date: String,
    }

    impl TestContext {
        pub(super) fn new(name: &str) -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "codex-quota-session-usage-{name}-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            assert!(fs::create_dir_all(&root).is_ok());
            let timestamp = OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| "2026-08-10T12:00:00Z".to_owned());
            let date = parse_timestamp_nanos(Some(&Value::String(timestamp.clone())))
                .and_then(system_time_from_unix_nanos)
                .and_then(local_calendar_date_at)
                .unwrap_or_else(local_calendar_date);
            let cache = root.join("app-data").join(CACHE_FILENAME);
            Self {
                root,
                cache,
                timestamp,
                date,
            }
        }

        pub(super) fn rollout(&self, thread_id: &str) -> PathBuf {
            self.root
                .join("sessions")
                .join("2000")
                .join("01")
                .join("01")
                .join(format!("rollout-2000-01-01T00-00-00-{thread_id}.jsonl"))
        }

        pub(super) fn archived_rollout(&self, thread_id: &str) -> PathBuf {
            self.root
                .join("archived_sessions")
                .join(format!("rollout-2000-01-01T00-00-00-{thread_id}.jsonl"))
        }

        pub(super) fn tracker(&self) -> SessionUsageTracker {
            SessionUsageTracker::with_paths(self.root.clone(), self.cache.clone())
        }

        pub(super) fn at(&self, seconds: i64) -> String {
            let base = OffsetDateTime::parse(&self.timestamp, &Rfc3339)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH);
            (base + TimeDuration::seconds(seconds))
                .format(&Rfc3339)
                .unwrap_or_else(|_| self.timestamp.clone())
        }
    }

    impl Drop for TestContext {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    pub(super) fn session_meta(
        timestamp: &str,
        thread_id: &str,
        provider: Option<&str>,
        parent: Option<&str>,
    ) -> Value {
        json!({
            "timestamp": timestamp,
            "type": "session_meta",
            "payload": {
                "id": thread_id,
                "model_provider": provider,
                "forked_from_id": parent
            }
        })
    }

    pub(super) fn token_count(
        timestamp: &str,
        total: u64,
        last: Option<u64>,
        source: Option<&str>,
    ) -> Value {
        json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": total.saturating_sub(10),
                        "output_tokens": 10,
                        "total_tokens": total
                    },
                    "last_token_usage": last.map(|last| json!({
                        "input_tokens": last.saturating_sub(1),
                        "output_tokens": 1,
                        "total_tokens": last
                    }))
                },
                "rate_limits": source.map(|limit_id| json!({ "limit_id": limit_id }))
            }
        })
    }

    pub(super) fn turn_context(timestamp: &str) -> Value {
        json!({
            "timestamp": timestamp,
            "type": "turn_context",
            "payload": { "model": "gpt-test" }
        })
    }

    pub(super) fn write_jsonl(path: &Path, values: &[Value]) {
        assert!(
            path.parent()
                .is_some_and(|parent| fs::create_dir_all(parent).is_ok())
        );
        let content = values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert!(fs::write(path, content).is_ok());
    }

    pub(super) fn append_jsonl(path: &Path, value: &Value) {
        use std::io::Write;

        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        assert!(writeln!(file, "{value}").is_ok());
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use serde_json::Value;

    use super::parser::{parse_timestamp_nanos, system_time_from_unix_nanos};
    use super::test_support::*;
    use super::*;
    use crate::quota::codex::protocol::local_calendar_date_at;

    fn timestamp_as_system_time(timestamp: &str) -> Option<SystemTime> {
        parse_timestamp_nanos(Some(&Value::String(timestamp.to_owned())))
            .and_then(system_time_from_unix_nanos)
    }

    #[test]
    fn period_boundary_includes_start_instant_and_excludes_earlier_usage() {
        let context = TestContext::new("period-boundary");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(-10), PARENT_ID, Some("openai"), None),
                token_count(&context.at(-1), 100, Some(100), Some("codex")),
                token_count(&context.at(0), 150, Some(50), Some("codex")),
                token_count(&context.at(1), 175, Some(25), Some("codex")),
            ],
        );
        let period_boundary =
            timestamp_as_system_time(&context.timestamp).and_then(PeriodBoundary::from_start);
        let result = context
            .tracker()
            .refresh_for_period(&context.date, period_boundary.as_ref());

        assert_eq!(
            result.ok().map(|value| (
                value.0.today_tokens,
                value.0.period_boundary_tokens,
                value.0.period_boundary_reliable,
            )),
            Some((175, 75, true))
        );
    }

    #[test]
    fn missing_start_day_log_keeps_today_reliable_but_boundary_unreliable() {
        let context = TestContext::new("missing-period-boundary");
        let period_boundary =
            timestamp_as_system_time(&context.timestamp).and_then(PeriodBoundary::from_start);
        let result = context
            .tracker()
            .refresh_for_period(&context.date, period_boundary.as_ref());

        assert_eq!(
            result.ok().map(|value| (
                value.0.today_tokens,
                value.0.today_reliable,
                value.0.period_boundary_reliable,
            )),
            Some((0, true, false))
        );
    }

    #[test]
    fn refresh_does_not_carry_today_value_across_date_switch() {
        let context = TestContext::new("date-switch");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        let mut tracker = context.tracker();
        let first = tracker.refresh_for_date(&context.date);
        let other_date = parse_timestamp_nanos(Some(&Value::String(context.at(-172_800))))
            .and_then(system_time_from_unix_nanos)
            .and_then(local_calendar_date_at)
            .unwrap_or_else(|| "1900-01-01".to_owned());

        let second = tracker.refresh_for_date(&other_date);
        let third = tracker.refresh_for_date(&context.date);

        assert_eq!(
            (
                first.ok().map(|value| value.0.today_tokens),
                second.ok().map(|value| value.0.today_tokens),
                third.ok().map(|value| value.0.today_tokens)
            ),
            (Some(100), Some(0), Some(100))
        );
    }

    #[test]
    fn discovery_error_marks_otherwise_empty_snapshot_unreliable() {
        let scan = ScanContext {
            candidate_by_path: HashMap::new(),
            rollout_index: HashMap::new(),
            caches: HashMap::new(),
            today_selected: HashSet::new(),
            boundary_selected: HashSet::new(),
            dependencies: HashSet::new(),
            diagnostics: RefreshDiagnostics {
                discovery_errors: 1,
                ..RefreshDiagnostics::default()
            },
        };

        let (snapshot, _, _) = scan.finish("2026-08-10", None);

        assert!(!snapshot.today_reliable);
    }
}
