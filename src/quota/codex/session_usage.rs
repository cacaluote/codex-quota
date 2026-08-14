mod aggregate;
mod files;
mod model;
mod parser;
mod watcher;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aggregate::{
    aggregate_period, aggregate_today, cache_has_tokens_in_period, cache_has_tokens_on_date,
};
use files::{
    codex_home, discover_candidates, inspect_changed_candidates, is_date_partition,
    is_date_partition_in_range, load_cache, modified_on_date, modified_on_or_after_date, path_key,
    save_cache,
};
use model::{CandidateFile, FileCache, ParentLink, UsageCacheV1};
use parser::{event_is_on_date, system_time_from_unix_nanos, update_candidate_cache};
use watcher::SessionChangeWatcher;

use super::PeriodBoundary;
use super::protocol::{local_calendar_date, local_calendar_date_at};

const CACHE_VERSION: u32 = 1;
const CACHE_FILENAME: &str = "usage-cache-v1.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalUsageSnapshot {
    pub(super) today_tokens: u64,
    pub(super) today_reliable: bool,
    pub(super) current_period_tokens: u64,
    pub(super) current_period_reliable: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum RefreshMode {
    #[default]
    FullScan,
    WatcherIncremental,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RefreshDiagnostics {
    pub(super) mode: RefreshMode,
    pub(super) files_scanned: usize,
    pub(super) files_read: usize,
    pub(super) token_events_added: usize,
    pub(super) parse_errors: usize,
    pub(super) discovery_errors: usize,
    pub(super) deferred_files: usize,
    pub(super) cache_write_failed: bool,
    pub(super) discovery_elapsed: Duration,
    pub(super) read_parse_elapsed: Duration,
    pub(super) aggregation_elapsed: Duration,
    pub(super) cache_write_elapsed: Duration,
    pub(super) total_elapsed: Duration,
    pub(super) aggregation_skipped: bool,
    pub(super) cache_write_skipped: bool,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum SessionUsageError {
    #[error("无法定位 Codex 会话目录")]
    CodexHomeUnavailable,
    #[error("无法读取 Codex 会话目录：{0}")]
    Discovery(#[source] io::Error),
}

pub(super) struct SessionUsageTracker {
    codex_dir: Option<PathBuf>,
    cache_path: Option<PathBuf>,
    cache: UsageCacheV1,
    candidate_index: HashMap<String, CandidateFile>,
    last_aggregation: Option<AggregationState>,
    cache_needs_write: bool,
    change_monitor: ChangeMonitor,
    pending_paths: HashSet<PathBuf>,
    full_scan_required: bool,
}

enum ChangeMonitor {
    Active(SessionChangeWatcher),
    Retry,
    #[cfg(test)]
    Disabled,
    #[cfg(test)]
    Simulated,
}

impl ChangeMonitor {
    fn supports_incremental(&self) -> bool {
        match self {
            Self::Active(_) => true,
            Self::Retry => false,
            #[cfg(test)]
            Self::Disabled => false,
            #[cfg(test)]
            Self::Simulated => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RefreshScope {
    date: String,
    period_boundary: Option<PeriodBoundary>,
}

#[derive(Debug, Clone)]
struct AggregationState {
    scope: RefreshScope,
    selected: HashSet<String>,
    dependencies: HashSet<String>,
    snapshot: LocalUsageSnapshot,
    deferred_files: usize,
}

struct ScanContext {
    candidate_by_path: HashMap<String, CandidateFile>,
    rollout_index: HashMap<String, Vec<String>>,
    caches: HashMap<String, FileCache>,
    today_selected: HashSet<String>,
    period_selected: HashSet<String>,
    dependencies: HashSet<String>,
    diagnostics: RefreshDiagnostics,
    cache_dirty: bool,
}

struct ScanResult {
    snapshot: LocalUsageSnapshot,
    diagnostics: RefreshDiagnostics,
    files: Vec<FileCache>,
    candidate_index: HashMap<String, CandidateFile>,
    selected: HashSet<String>,
    dependencies: HashSet<String>,
    cache_dirty: bool,
}

impl ScanContext {
    fn new_full(
        codex_dir: &Path,
        today: &str,
        boundary: Option<&PeriodBoundary>,
        cached_files: Vec<FileCache>,
    ) -> Result<Self, SessionUsageError> {
        let discovery = discover_candidates(codex_dir)?;
        Ok(Self::from_candidates(
            today,
            boundary,
            cached_files,
            &discovery.candidates,
            discovery.errors,
            0,
            RefreshMode::FullScan,
        ))
    }

    fn new_incremental(
        today: &str,
        boundary: Option<&PeriodBoundary>,
        cached_files: Vec<FileCache>,
        mut candidate_by_path: HashMap<String, CandidateFile>,
        changed_paths: &HashSet<PathBuf>,
    ) -> Self {
        let discovery = inspect_changed_candidates(changed_paths);
        for path in discovery.missing {
            candidate_by_path.remove(&path_key(&path));
        }
        for candidate in discovery.candidates {
            let candidate_key = path_key(&candidate.path);
            candidate_by_path.retain(|key, known| {
                key == &candidate_key
                    || known.creation_time != candidate.creation_time
                    || known.thread_id != candidate.thread_id
            });
            candidate_by_path.insert(candidate_key, candidate);
        }
        let candidates: Vec<_> = candidate_by_path.into_values().collect();
        Self::from_candidates(
            today,
            boundary,
            cached_files,
            &candidates,
            discovery.errors,
            changed_paths.len(),
            RefreshMode::WatcherIncremental,
        )
    }

    fn from_candidates(
        today: &str,
        boundary: Option<&PeriodBoundary>,
        cached_files: Vec<FileCache>,
        candidates: &[CandidateFile],
        discovery_errors: usize,
        inspected_files: usize,
        mode: RefreshMode,
    ) -> Self {
        let candidate_by_path: HashMap<String, CandidateFile> = candidates
            .iter()
            .cloned()
            .map(|candidate| (path_key(&candidate.path), candidate))
            .collect();
        let mut rollout_index: HashMap<String, Vec<String>> = HashMap::new();
        for candidate in candidates {
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
        let period_selected = boundary.map_or_else(HashSet::new, |boundary| {
            candidates
                .iter()
                .filter_map(|candidate| {
                    let key = path_key(&candidate.path);
                    let cached_period = caches.get(&key).is_some_and(|cache| {
                        cache_has_tokens_in_period(cache, boundary.start_nanos())
                    });
                    (cached_period
                        || is_date_partition_in_range(
                            &candidate.path,
                            boundary.local_date(),
                            today,
                        )
                        || modified_on_or_after_date(candidate, boundary.local_date()))
                    .then_some(key)
                })
                .collect()
        });

        Self {
            candidate_by_path,
            rollout_index,
            caches,
            today_selected,
            period_selected,
            dependencies: HashSet::new(),
            diagnostics: RefreshDiagnostics {
                mode,
                files_scanned: if mode == RefreshMode::FullScan {
                    candidates.len()
                } else {
                    inspected_files
                },
                discovery_errors,
                ..RefreshDiagnostics::default()
            },
            cache_dirty: false,
        }
    }

    fn parse_selected_and_dependencies(&mut self) {
        let initial_selected: Vec<String> = self
            .today_selected
            .union(&self.period_selected)
            .cloned()
            .collect();
        for key in &initial_selected {
            if let Some(candidate) = self.candidate_by_path.get(key) {
                self.cache_dirty |=
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
                    self.cache_dirty |=
                        update_candidate_cache(candidate, &mut self.caches, &mut self.diagnostics);
                    pending.push(parent_key.clone());
                }
            }
        }
    }

    fn selected(&self) -> HashSet<String> {
        self.today_selected
            .union(&self.period_selected)
            .cloned()
            .collect()
    }

    fn finish(
        mut self,
        today: &str,
        boundary: Option<&PeriodBoundary>,
        reused: Option<(LocalUsageSnapshot, usize)>,
    ) -> ScanResult {
        let snapshot = if let Some((snapshot, deferred_files)) = reused {
            self.diagnostics.aggregation_skipped = true;
            self.diagnostics.deferred_files = deferred_files;
            snapshot
        } else {
            let (today_tokens, today_aggregate_reliable, today_deferred) = aggregate_today(
                today,
                &self.today_selected,
                &self.caches,
                &self.rollout_index,
            );
            let today_reliable = today_aggregate_reliable && self.diagnostics.discovery_errors == 0;
            let (current_period_tokens, current_period_reliable, period_deferred) = boundary
                .map_or((0, false, 0), |boundary| {
                    let has_openai_start_file = self.period_selected.iter().any(|key| {
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
                    let (tokens, reliable, deferred) = aggregate_period(
                        boundary.start_nanos(),
                        &self.period_selected,
                        &self.caches,
                        &self.rollout_index,
                    );
                    (
                        tokens,
                        has_openai_start_file && reliable && self.diagnostics.discovery_errors == 0,
                        deferred,
                    )
                });
            self.diagnostics.deferred_files = today_deferred.saturating_add(period_deferred);
            LocalUsageSnapshot {
                today_tokens,
                today_reliable,
                current_period_tokens,
                current_period_reliable,
            }
        };
        let selected = self.selected();
        self.diagnostics.parse_errors = selected
            .iter()
            .filter_map(|key| self.caches.get(key))
            .map(|cache| cache.parse_errors)
            .sum();

        let dependencies = self.dependencies;
        let mut retained = selected.clone();
        retained.extend(dependencies.iter().cloned());
        self.caches
            .retain(|key, _| retained.contains(key) && self.candidate_by_path.contains_key(key));
        let mut files: Vec<_> = self.caches.into_values().collect();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        ScanResult {
            snapshot,
            diagnostics: self.diagnostics,
            files,
            candidate_index: self.candidate_by_path,
            selected,
            dependencies,
            cache_dirty: self.cache_dirty,
        }
    }
}

impl SessionUsageTracker {
    pub(super) fn new() -> Self {
        let codex_dir = codex_home();
        let cache_path = crate::config::app_data_dir()
            .ok()
            .map(|directory| directory.join(CACHE_FILENAME));
        let cache = load_valid_cache(cache_path.as_deref(), codex_dir.as_deref());
        let watcher = codex_dir
            .as_deref()
            .filter(|directory| directory.is_dir())
            .and_then(|directory| SessionChangeWatcher::start(directory).ok());
        let change_monitor = watcher.map_or(ChangeMonitor::Retry, ChangeMonitor::Active);
        Self {
            codex_dir,
            cache_path,
            cache,
            candidate_index: HashMap::new(),
            last_aggregation: None,
            cache_needs_write: false,
            change_monitor,
            pending_paths: HashSet::new(),
            full_scan_required: true,
        }
    }

    #[cfg(test)]
    fn with_paths(codex_dir: PathBuf, cache_path: PathBuf) -> Self {
        let cache = load_valid_cache(Some(&cache_path), Some(&codex_dir));
        Self {
            cache,
            candidate_index: HashMap::new(),
            codex_dir: Some(codex_dir),
            cache_path: Some(cache_path),
            last_aggregation: None,
            cache_needs_write: false,
            change_monitor: ChangeMonitor::Disabled,
            pending_paths: HashSet::new(),
            full_scan_required: true,
        }
    }

    #[cfg(test)]
    fn enable_incremental_for_test(&mut self) {
        self.change_monitor = ChangeMonitor::Simulated;
        self.full_scan_required = true;
    }

    #[cfg(test)]
    fn mark_changed_for_test(&mut self, path: PathBuf) {
        self.pending_paths.insert(path);
    }

    pub(super) fn poll_changes(&mut self) -> bool {
        let ChangeMonitor::Active(watcher) = &self.change_monitor else {
            return false;
        };
        let changes = watcher.drain();
        let has_changes = changes.has_changes();
        if changes.requires_full_scan {
            self.full_scan_required = true;
        }
        self.pending_paths.extend(changes.paths);
        has_changes
    }

    fn ensure_watcher(&mut self) {
        if matches!(self.change_monitor, ChangeMonitor::Retry)
            && let Some(codex_dir) = self
                .codex_dir
                .as_deref()
                .filter(|directory| directory.is_dir())
            && let Ok(watcher) = SessionChangeWatcher::start(codex_dir)
        {
            self.change_monitor = ChangeMonitor::Active(watcher);
            self.full_scan_required = true;
        }
    }

    pub(super) fn require_full_scan(&mut self) {
        self.full_scan_required = true;
    }

    pub(super) fn needs_refresh(&mut self, period_boundary: Option<&PeriodBoundary>) -> bool {
        self.ensure_watcher();
        self.poll_changes();
        let scope = RefreshScope {
            date: local_calendar_date(),
            period_boundary: period_boundary.cloned(),
        };
        !self.change_monitor.supports_incremental()
            || self.full_scan_required
            || !self.pending_paths.is_empty()
            || self.cache_needs_write
            || self.cache.date != scope.date
            || self
                .last_aggregation
                .as_ref()
                .is_none_or(|last| last.scope != scope)
    }

    fn unchanged_refresh(
        &self,
        date: &str,
        total_started: Instant,
    ) -> Option<(LocalUsageSnapshot, RefreshDiagnostics)> {
        if !self.pending_paths.is_empty() || self.cache_needs_write || self.cache.date != date {
            return None;
        }
        let last = self.last_aggregation.as_ref()?;
        let diagnostics = RefreshDiagnostics {
            mode: RefreshMode::WatcherIncremental,
            parse_errors: self
                .cache
                .files
                .iter()
                .filter(|cache| last.selected.contains(&cache.path))
                .map(|cache| cache.parse_errors)
                .sum(),
            deferred_files: last.deferred_files,
            aggregation_skipped: true,
            cache_write_skipped: true,
            total_elapsed: total_started.elapsed(),
            ..RefreshDiagnostics::default()
        };
        Some((last.snapshot.clone(), diagnostics))
    }

    fn write_cache(&mut self, diagnostics: &mut RefreshDiagnostics, cache_dirty: bool) {
        let cache_write_started = Instant::now();
        if cache_dirty || self.cache_needs_write {
            if let Some(cache_path) = self.cache_path.as_deref() {
                diagnostics.cache_write_failed = save_cache(cache_path, &self.cache).is_err();
                self.cache_needs_write = diagnostics.cache_write_failed;
            } else {
                diagnostics.cache_write_skipped = true;
                self.cache_needs_write = false;
            }
        } else {
            diagnostics.cache_write_skipped = true;
        }
        diagnostics.cache_write_elapsed = cache_write_started.elapsed();
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
        let total_started = Instant::now();
        self.ensure_watcher();
        self.poll_changes();
        let Some(codex_dir) = self.codex_dir.as_deref() else {
            return Err(SessionUsageError::CodexHomeUnavailable);
        };
        if !codex_dir.is_dir() {
            return Err(SessionUsageError::CodexHomeUnavailable);
        }
        let scope = RefreshScope {
            date: date.to_owned(),
            period_boundary: period_boundary.cloned(),
        };
        let scope_changed = self
            .last_aggregation
            .as_ref()
            .is_none_or(|last| last.scope != scope);
        let full_scan =
            !self.change_monitor.supports_incremental() || self.full_scan_required || scope_changed;
        if !full_scan && let Some(unchanged) = self.unchanged_refresh(date, total_started) {
            return Ok(unchanged);
        }
        let cache_date_changed = self.cache.date != date;
        let previous_cache_paths: HashSet<_> = self
            .cache
            .files
            .iter()
            .map(|cache| cache.path.clone())
            .collect();

        let discovery_started = Instant::now();
        let cached_files = std::mem::take(&mut self.cache.files);
        let mut scan = if full_scan {
            self.pending_paths.clear();
            ScanContext::new_full(codex_dir, date, period_boundary, cached_files)?
        } else {
            let changed_paths = std::mem::take(&mut self.pending_paths);
            ScanContext::new_incremental(
                date,
                period_boundary,
                cached_files,
                std::mem::take(&mut self.candidate_index),
                &changed_paths,
            )
        };
        scan.diagnostics.discovery_elapsed = discovery_started.elapsed();

        let read_parse_started = Instant::now();
        scan.parse_selected_and_dependencies();
        scan.diagnostics.read_parse_elapsed = read_parse_started.elapsed();

        let reused = self
            .last_aggregation
            .as_ref()
            .filter(|last| {
                !scan.cache_dirty
                    && scan.diagnostics.discovery_errors == 0
                    && last.scope == scope
                    && last.selected == scan.selected()
                    && last.dependencies == scan.dependencies
            })
            .map(|last| (last.snapshot.clone(), last.deferred_files));
        let aggregation_started = Instant::now();
        let ScanResult {
            snapshot,
            mut diagnostics,
            files,
            candidate_index,
            selected,
            dependencies,
            cache_dirty,
        } = scan.finish(date, period_boundary, reused);
        let mut cache_dirty = cache_dirty;
        diagnostics.aggregation_elapsed = aggregation_started.elapsed();
        let retained_paths: HashSet<_> = files.iter().map(|cache| cache.path.clone()).collect();
        cache_dirty |= cache_date_changed || retained_paths != previous_cache_paths;
        self.cache.version = CACHE_VERSION;
        self.cache.codex_home = path_key(codex_dir);
        date.clone_into(&mut self.cache.date);
        self.cache.files = files;
        self.candidate_index = candidate_index;

        self.write_cache(&mut diagnostics, cache_dirty);
        diagnostics.total_elapsed = total_started.elapsed();

        if diagnostics.discovery_errors == 0 {
            if diagnostics.mode == RefreshMode::FullScan {
                self.full_scan_required = false;
            }
            self.last_aggregation = Some(AggregationState {
                scope,
                selected,
                dependencies,
                snapshot: snapshot.clone(),
                deferred_files: diagnostics.deferred_files,
            });
        } else {
            self.full_scan_required = true;
            self.last_aggregation = None;
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
    fn current_period_includes_start_instant_and_excludes_earlier_usage() {
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
                value.0.current_period_tokens,
                value.0.current_period_reliable,
            )),
            Some((175, 75, true))
        );
    }

    #[test]
    fn missing_start_day_log_keeps_today_reliable_but_period_unreliable() {
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
                value.0.current_period_reliable,
            )),
            Some((0, true, false))
        );
    }

    #[test]
    fn token_without_timestamp_makes_current_period_unreliable() {
        let context = TestContext::new("period-missing-timestamp");
        let file = context.rollout(PARENT_ID);
        let mut event = token_count(&context.at(1), 100, Some(100), Some("codex"));
        if let Some(object) = event.as_object_mut() {
            object.remove("timestamp");
        }
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                event,
            ],
        );
        let period_boundary =
            timestamp_as_system_time(&context.timestamp).and_then(PeriodBoundary::from_start);

        let result = context
            .tracker()
            .refresh_for_period(&context.date, period_boundary.as_ref());

        assert_eq!(
            result.ok().map(|value| value.0.current_period_reliable),
            Some(false)
        );
    }

    #[test]
    fn current_period_keeps_previous_local_day_after_midnight() {
        let context = TestContext::new("period-cross-midnight");
        let file = context.rollout(PARENT_ID);
        let boundary_timestamp = context.at(-172_800);
        write_jsonl(
            &file,
            &[
                session_meta(&boundary_timestamp, PARENT_ID, Some("openai"), None),
                token_count(
                    &boundary_timestamp,
                    3_171_568,
                    Some(3_171_568),
                    Some("codex"),
                ),
                token_count(
                    &context.at(-86_400),
                    60_050_978,
                    Some(56_879_410),
                    Some("codex"),
                ),
                token_count(&context.at(0), 60_100_978, Some(50_000), Some("codex")),
            ],
        );
        let period_boundary =
            timestamp_as_system_time(&boundary_timestamp).and_then(PeriodBoundary::from_start);

        let result = context
            .tracker()
            .refresh_for_period(&context.date, period_boundary.as_ref());

        assert_eq!(
            result.ok().map(|value| (
                value.0.today_tokens,
                value.0.current_period_tokens,
                value.0.current_period_reliable,
            )),
            Some((50_000, 60_100_978, true))
        );
    }

    #[test]
    fn unchanged_refresh_reuses_aggregation_and_skips_cache_write() {
        let context = TestContext::new("unchanged-no-op");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        let boundary =
            timestamp_as_system_time(&context.timestamp).and_then(PeriodBoundary::from_start);
        let mut tracker = context.tracker();
        let _ = tracker.refresh_for_period(&context.date, boundary.as_ref());

        let second = tracker.refresh_for_period(&context.date, boundary.as_ref());

        assert_eq!(
            second.ok().map(|(_, diagnostics)| (
                diagnostics.files_read,
                diagnostics.token_events_added,
                diagnostics.aggregation_skipped,
                diagnostics.cache_write_skipped,
            )),
            Some((0, 0, true, true))
        );
    }

    #[test]
    fn watcher_incremental_reads_old_partition_file_after_append() {
        let context = TestContext::new("watcher-old-partition-append");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        let mut tracker = context.tracker();
        tracker.enable_incremental_for_test();
        let first = tracker.refresh_for_date(&context.date);
        append_jsonl(
            &file,
            &token_count(&context.at(2), 150, Some(50), Some("codex")),
        );
        tracker.mark_changed_for_test(file);

        let second = tracker.refresh_for_date(&context.date);

        assert_eq!(
            (
                first
                    .ok()
                    .map(|value| (value.1.mode, value.1.token_events_added)),
                second.ok().map(|value| (
                    value.0.today_tokens,
                    value.1.mode,
                    value.1.files_scanned,
                    value.1.files_read,
                    value.1.token_events_added,
                )),
            ),
            (
                Some((RefreshMode::FullScan, 1)),
                Some((150, RefreshMode::WatcherIncremental, 1, 1, 1))
            )
        );
    }

    #[test]
    fn watcher_incremental_skips_all_work_without_changes() {
        let context = TestContext::new("watcher-no-change");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        let mut tracker = context.tracker();
        tracker.enable_incremental_for_test();
        let _ = tracker.refresh_for_date(&context.date);

        let second = tracker.refresh_for_date(&context.date);

        assert_eq!(
            second.ok().map(|value| (
                value.1.mode,
                value.1.files_scanned,
                value.1.files_read,
                value.1.token_events_added,
                value.1.aggregation_skipped,
                value.1.cache_write_skipped,
            )),
            Some((RefreshMode::WatcherIncremental, 0, 0, 0, true, true))
        );
    }

    #[test]
    fn unchanged_watcher_state_does_not_need_timed_refresh() {
        let context = TestContext::new("watcher-no-timed-refresh");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        let mut tracker = context.tracker();
        tracker.enable_incremental_for_test();
        let _ = tracker.refresh_for_date(&context.date);

        let needs_refresh = tracker.needs_refresh(None);

        assert!(!needs_refresh);
    }

    #[test]
    fn watcher_change_needs_local_refresh() {
        let context = TestContext::new("watcher-change-needs-refresh");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        let mut tracker = context.tracker();
        tracker.enable_incremental_for_test();
        let _ = tracker.refresh_for_date(&context.date);
        tracker.mark_changed_for_test(file);

        let needs_refresh = tracker.needs_refresh(None);

        assert!(needs_refresh);
    }

    #[test]
    fn forced_full_scan_needs_local_refresh() {
        let context = TestContext::new("forced-scan-needs-refresh");
        let mut tracker = context.tracker();
        tracker.enable_incremental_for_test();
        tracker.full_scan_required = false;
        tracker.require_full_scan();

        assert!(tracker.needs_refresh(None));
    }

    #[test]
    fn watcher_incremental_loads_unselected_fork_parent_from_full_index() {
        let context = TestContext::new("watcher-fork-parent-index");
        let parent = context.rollout(PARENT_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(&context.at(-10), PARENT_ID, Some("openai"), None),
                token_count(&context.at(-9), 100, Some(100), Some("codex")),
                turn_context(&context.at(3)),
            ],
        );
        let old_times = std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH);
        assert!(
            std::fs::File::options()
                .write(true)
                .open(&parent)
                .and_then(|file| file.set_times(old_times))
                .is_ok()
        );
        let mut tracker = context.tracker();
        tracker.enable_incremental_for_test();
        let _ = tracker.refresh_for_date(&context.date);
        let child = context.rollout(CHILD_ID);
        write_jsonl(
            &child,
            &[
                session_meta(&context.at(3), CHILD_ID, Some("openai"), Some(PARENT_ID)),
                token_count(&context.at(3), 100, Some(100), Some("codex")),
                token_count(&context.at(4), 150, Some(50), Some("codex")),
            ],
        );
        tracker.mark_changed_for_test(child);

        let result = tracker.refresh_for_date(&context.date);

        assert_eq!(
            result
                .ok()
                .map(|value| (value.0.today_tokens, value.0.today_reliable, value.1.mode,)),
            Some((50, true, RefreshMode::WatcherIncremental))
        );
    }

    #[test]
    fn changed_period_scope_reaggregates_without_rewriting_unchanged_cache() {
        let context = TestContext::new("scope-only-change");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        let boundary =
            timestamp_as_system_time(&context.timestamp).and_then(PeriodBoundary::from_start);
        let mut tracker = context.tracker();
        let _ = tracker.refresh_for_date(&context.date);

        let second = tracker.refresh_for_period(&context.date, boundary.as_ref());

        assert_eq!(
            second.ok().map(|(snapshot, diagnostics)| (
                snapshot.current_period_tokens,
                diagnostics.files_read,
                diagnostics.aggregation_skipped,
                diagnostics.cache_write_skipped,
            )),
            Some((100, 0, false, true))
        );
    }

    #[test]
    fn appended_event_reaggregates_and_updates_cache() {
        let context = TestContext::new("append-invalidates-no-op");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        let boundary =
            timestamp_as_system_time(&context.timestamp).and_then(PeriodBoundary::from_start);
        let mut tracker = context.tracker();
        let _ = tracker.refresh_for_period(&context.date, boundary.as_ref());
        append_jsonl(
            &file,
            &token_count(&context.at(2), 150, Some(50), Some("codex")),
        );

        let second = tracker.refresh_for_period(&context.date, boundary.as_ref());

        assert_eq!(
            second.ok().map(|(snapshot, diagnostics)| (
                snapshot.today_tokens,
                diagnostics.files_read,
                diagnostics.token_events_added,
                diagnostics.aggregation_skipped,
                diagnostics.cache_write_skipped,
            )),
            Some((150, 1, 1, false, false))
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
            period_selected: HashSet::new(),
            dependencies: HashSet::new(),
            diagnostics: RefreshDiagnostics {
                discovery_errors: 1,
                ..RefreshDiagnostics::default()
            },
            cache_dirty: false,
        };

        let snapshot = scan.finish("2026-08-10", None, None).snapshot;

        assert!(!snapshot.today_reliable);
    }
}
