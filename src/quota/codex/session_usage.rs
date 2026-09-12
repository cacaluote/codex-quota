mod aggregate;
mod files;
mod model;
mod parser;
mod watcher;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use aggregate::{
    ModelVolumes, aggregate_lifetime, aggregate_period, aggregate_today,
    cache_has_tokens_in_period, cache_has_tokens_on_date,
};
use files::{
    codex_home, discover_candidates, inspect_changed_candidates, is_date_partition,
    is_date_partition_in_range, load_cache, modified_on_date, modified_on_or_after_date, path_key,
    save_cache,
};
use model::{
    CandidateFile, FileCache, LifetimeAggregate, ParentLink, RateLimitSnapshotEntry,
    RateLimitWindowEntry, UsageCacheV1,
};
use parser::{event_is_on_date, system_time_from_unix_nanos, update_candidate_cache};
use watcher::SessionChangeWatcher;

use super::PeriodBoundary;
use super::protocol::{local_calendar_date, local_calendar_date_at};
use crate::quota::{QuotaSnapshot, QuotaWindow};

// Reparse persisted deltas calculated before model-aware duplicate detection.
const CACHE_VERSION: u32 = 4;
const CACHE_FILENAME: &str = "usage-cache-v1.json";

#[derive(Debug, Clone, PartialEq)]
pub(super) struct LocalUsageSnapshot {
    pub(super) today_tokens: u64,
    pub(super) today_reliable: bool,
    pub(super) current_period_tokens: u64,
    pub(super) current_period_reliable: bool,
    /// 分桶用量（reliable 才有），用于按模型计价。
    pub(super) today_volume: Option<ModelVolumes>,
    pub(super) period_volume: Option<ModelVolumes>,
    pub(super) quota: Option<QuotaSnapshot>,
    pub(super) plan_type: Option<String>,
    pub(super) lifetime_tokens: Option<u64>,
    pub(super) lifetime_reliable: bool,
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
    lifetime_selected: HashSet<String>,
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
    latest_rate_limits: Option<RateLimitSnapshotEntry>,
    lifetime: Option<LifetimeAggregate>,
    lifetime_sources: Option<Vec<String>>,
    cache_dirty: bool,
}

struct DerivedQuota {
    latest_rate_limits: Option<RateLimitSnapshotEntry>,
    lifetime: Option<LifetimeAggregate>,
    /// Contributor paths when the lifetime was recomputed this scan;
    /// `None` when the previous aggregate is carried forward.
    lifetime_sources: Option<Vec<String>>,
}

impl ScanContext {
    fn new_full(
        codex_dir: &Path,
        today: &str,
        boundary: Option<&PeriodBoundary>,
        cached_files: Vec<FileCache>,
    ) -> Result<Self, SessionUsageError> {
        let discovery = discover_candidates(codex_dir)?;
        let mut context = Self::from_candidates(
            today,
            boundary,
            cached_files,
            &discovery.candidates,
            discovery.errors,
            0,
            RefreshMode::FullScan,
        );
        // A full scan is the only place that sees every candidate, so it also
        // feeds the lifetime aggregation and the newest rate-limit snapshot.
        context.lifetime_selected = context.candidate_by_path.keys().cloned().collect();
        Ok(context)
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
            lifetime_selected: HashSet::new(),
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
        let selected_roots: Vec<String> = self
            .today_selected
            .union(&self.period_selected)
            .cloned()
            .collect();
        let mut dependencies = HashSet::new();
        self.parse_roots(selected_roots, &mut dependencies);
        self.dependencies = dependencies;

        if !self.lifetime_selected.is_empty() {
            let lifetime_roots: Vec<String> = self.lifetime_selected.iter().cloned().collect();
            let mut lifetime_dependencies = HashSet::new();
            self.parse_roots(lifetime_roots, &mut lifetime_dependencies);
        }
    }

    fn parse_roots(&mut self, roots: Vec<String>, visited: &mut HashSet<String>) {
        let mut pending = roots;
        while let Some(key) = pending.pop() {
            if let Some(candidate) = self.candidate_by_path.get(&key) {
                self.cache_dirty |=
                    update_candidate_cache(candidate, &mut self.caches, &mut self.diagnostics);
            }
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
                if visited.insert(parent_key.clone())
                    && let Some(candidate) = self.candidate_by_path.get(parent_key)
                {
                    self.cache_dirty |=
                        update_candidate_cache(candidate, &mut self.caches, &mut self.diagnostics);
                    pending.push(parent_key.clone());
                }
            }
        }
    }

    fn derive_quota_and_lifetime(
        &self,
        previous_rate_limits: Option<RateLimitSnapshotEntry>,
        previous_lifetime: Option<LifetimeAggregate>,
    ) -> DerivedQuota {
        // Only files that still exist on disk may contribute: a cache entry
        // whose file was deleted must not keep feeding the snapshot or the
        // lifetime total until the next trim.
        let live_keys: HashSet<&String> = self
            .caches
            .keys()
            .filter(|key| self.candidate_by_path.contains_key(*key))
            .collect();
        let scan_latest = live_keys
            .iter()
            .filter_map(|key| {
                self.caches
                    .get(*key)
                    .and_then(|cache| cache.latest_rate_limits.clone())
            })
            .max_by_key(|entry| entry.timestamp_nanos);
        // Discovery gaps make candidate absence meaningless: with an
        // incomplete candidate set, absence says the directory could not be
        // enumerated, not that the file was deleted. Carry the persisted
        // values unchanged until a complete scan revalidates them.
        let candidates_complete = self.diagnostics.discovery_errors == 0;
        let carried_rate_limits = if candidates_complete {
            // The persisted snapshot survives trims, so it can outlive its
            // own source file; carry it only while that file still exists.
            previous_rate_limits
                .filter(|previous| self.candidate_by_path.contains_key(&previous.source_path))
        } else {
            previous_rate_limits
        };
        let latest_rate_limits = newest_rate_limit_entry(carried_rate_limits, scan_latest);
        if self.diagnostics.mode == RefreshMode::FullScan && candidates_complete {
            let selected: HashSet<String> = live_keys.into_iter().cloned().collect();
            let (tokens, reliable) =
                aggregate_lifetime(&selected, &self.caches, &self.rollout_index);
            let mut lifetime_sources: Vec<String> = selected.into_iter().collect();
            lifetime_sources.sort();
            DerivedQuota {
                latest_rate_limits,
                lifetime: Some(LifetimeAggregate { tokens, reliable }),
                lifetime_sources: Some(lifetime_sources),
            }
        } else {
            DerivedQuota {
                latest_rate_limits,
                lifetime: previous_lifetime,
                lifetime_sources: None,
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
        previous_rate_limits: Option<RateLimitSnapshotEntry>,
        previous_lifetime: Option<LifetimeAggregate>,
    ) -> ScanResult {
        let mut snapshot = if let Some((snapshot, deferred_files)) = reused {
            self.diagnostics.aggregation_skipped = true;
            self.diagnostics.deferred_files = deferred_files;
            snapshot
        } else {
            let (today_tokens, today_aggregate_reliable, today_deferred, today_volumes) =
                aggregate_today(
                    today,
                    &self.today_selected,
                    &self.caches,
                    &self.rollout_index,
                );
            let today_reliable = today_aggregate_reliable && self.diagnostics.discovery_errors == 0;
            let (current_period_tokens, current_period_reliable, period_deferred, period_volumes) =
                boundary.map_or((0, false, 0, ModelVolumes::default()), |boundary| {
                    // 窗口内本机没有任何会话文件痕迹时，本期就是可靠的 0
                    // （与今日口径一致）；只有存在文件却锚定不到窗口起点时
                    // 才保守显示 --（日志可能未覆盖窗口开头）。
                    let anchored = self.period_selected.is_empty()
                        || period_has_openai_start_file(
                            &self.caches,
                            &self.period_selected,
                            boundary,
                        );
                    let (tokens, reliable, deferred, period_volumes) = aggregate_period(
                        boundary.start_nanos(),
                        &self.period_selected,
                        &self.caches,
                        &self.rollout_index,
                    );
                    (
                        tokens,
                        anchored && reliable && self.diagnostics.discovery_errors == 0,
                        deferred,
                        period_volumes,
                    )
                });
            self.diagnostics.deferred_files = today_deferred.saturating_add(period_deferred);
            LocalUsageSnapshot {
                today_tokens,
                today_reliable,
                current_period_tokens,
                current_period_reliable,
                today_volume: today_reliable.then_some(today_volumes),
                period_volume: current_period_reliable.then_some(period_volumes),
                quota: None,
                plan_type: None,
                lifetime_tokens: None,
                lifetime_reliable: false,
            }
        };
        let derived = self.derive_quota_and_lifetime(previous_rate_limits, previous_lifetime);
        snapshot.quota = derived
            .latest_rate_limits
            .as_ref()
            .and_then(quota_snapshot_from_entry);
        snapshot.plan_type = derived
            .latest_rate_limits
            .as_ref()
            .and_then(|entry| entry.plan_type.clone());
        snapshot.lifetime_tokens = derived
            .lifetime
            .as_ref()
            .filter(|aggregate| aggregate.reliable)
            .map(|aggregate| aggregate.tokens);
        snapshot.lifetime_reliable = derived
            .lifetime
            .as_ref()
            .is_some_and(|aggregate| aggregate.reliable);
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
            latest_rate_limits: derived.latest_rate_limits,
            lifetime: derived.lifetime,
            lifetime_sources: derived.lifetime_sources,
            cache_dirty: self.cache_dirty,
        }
    }
}

/// 本期窗口起点当天是否存在 openai 会话痕迹：没有锚定痕迹时“本期”的
/// 可靠性无从谈起，宁可显示 --。
fn period_has_openai_start_file(
    caches: &HashMap<String, FileCache>,
    period_selected: &HashSet<String>,
    boundary: &PeriodBoundary,
) -> bool {
    period_selected.iter().any(|key| {
        caches.get(key).is_some_and(|cache| {
            cache.root.as_ref().is_some_and(|root| {
                root.provider.as_deref() == Some("openai")
                    && (root
                        .timestamp_nanos
                        .and_then(system_time_from_unix_nanos)
                        .and_then(local_calendar_date_at)
                        .as_deref()
                        == Some(boundary.local_date())
                        || cache
                            .events
                            .iter()
                            .any(|event| event_is_on_date(event, boundary.local_date())))
            })
        })
    })
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

    pub(super) fn retry_watcher(&mut self) -> bool {
        let was_incremental = self.change_monitor.supports_incremental();
        self.ensure_watcher();
        !was_incremental && self.change_monitor.supports_incremental()
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

    fn build_scan(
        &mut self,
        full_scan: bool,
        codex_dir: &Path,
        date: &str,
        period_boundary: Option<&PeriodBoundary>,
    ) -> Result<ScanContext, SessionUsageError> {
        let cached_files = std::mem::take(&mut self.cache.files);
        if full_scan {
            self.pending_paths.clear();
            ScanContext::new_full(codex_dir, date, period_boundary, cached_files)
        } else {
            let changed_paths = std::mem::take(&mut self.pending_paths);
            Ok(ScanContext::new_incremental(
                date,
                period_boundary,
                cached_files,
                std::mem::take(&mut self.candidate_index),
                &changed_paths,
            ))
        }
    }

    fn refresh_for_period(
        &mut self,
        date: &str,
        period_boundary: Option<&PeriodBoundary>,
    ) -> Result<(LocalUsageSnapshot, RefreshDiagnostics), SessionUsageError> {
        let total_started = Instant::now();
        self.ensure_watcher();
        self.poll_changes();
        let codex_dir = self
            .codex_dir
            .clone()
            .filter(|directory| directory.is_dir())
            .ok_or(SessionUsageError::CodexHomeUnavailable)?;
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
        let previous_rate_limits = self.cache.latest_rate_limits.clone();
        let previous_lifetime = self.cache.lifetime.clone();
        let previous_lifetime_sources = self.cache.lifetime_sources.clone();

        let discovery_started = Instant::now();
        let mut scan = self.build_scan(full_scan, &codex_dir, date, period_boundary)?;
        // A cached file that no longer exists invalidates the carried
        // snapshot and lifetime totals: discard them and let the promoted
        // full scan recompute both from the surviving files only.
        let (promoted_scan, cached_paths_dropped) = rebalance_scan_after_deletions(
            scan,
            full_scan,
            &previous_cache_paths,
            &previous_lifetime_sources,
            &codex_dir,
            date,
            period_boundary,
        )?;
        scan = promoted_scan;
        let previous_rate_limits = (!cached_paths_dropped)
            .then_some(previous_rate_limits)
            .flatten();
        let previous_lifetime = (!cached_paths_dropped)
            .then_some(previous_lifetime)
            .flatten();
        scan.diagnostics.discovery_elapsed = discovery_started.elapsed();

        let read_parse_started = Instant::now();
        scan.parse_selected_and_dependencies();
        scan.diagnostics.read_parse_elapsed = read_parse_started.elapsed();

        let reused = self.reusable_aggregation(&scan, &scope);
        let aggregation_started = Instant::now();
        let ScanResult {
            snapshot,
            mut diagnostics,
            files,
            candidate_index,
            selected,
            dependencies,
            latest_rate_limits,
            lifetime,
            lifetime_sources,
            cache_dirty,
        } = scan.finish(
            date,
            period_boundary,
            reused,
            previous_rate_limits.clone(),
            previous_lifetime.clone(),
        );
        let mut cache_dirty = cache_dirty;
        diagnostics.aggregation_elapsed = aggregation_started.elapsed();
        let retained_paths: HashSet<_> = files.iter().map(|cache| cache.path.clone()).collect();
        cache_dirty |= cache_date_changed
            || retained_paths != previous_cache_paths
            || previous_rate_limits != latest_rate_limits
            || previous_lifetime != lifetime;
        self.cache.version = CACHE_VERSION;
        self.cache.codex_home = path_key(&codex_dir);
        date.clone_into(&mut self.cache.date);
        self.cache.files = files;
        self.cache.latest_rate_limits = latest_rate_limits;
        self.cache.lifetime = lifetime;
        if let Some(lifetime_sources) = lifetime_sources {
            cache_dirty |= lifetime_sources != self.cache.lifetime_sources;
            self.cache.lifetime_sources = lifetime_sources;
        }
        self.candidate_index = candidate_index;

        self.write_cache(&mut diagnostics, cache_dirty);
        diagnostics.total_elapsed = total_started.elapsed();
        self.remember_aggregation(scope, selected, dependencies, &snapshot, &diagnostics);

        Ok((snapshot, diagnostics))
    }

    fn reusable_aggregation(
        &self,
        scan: &ScanContext,
        scope: &RefreshScope,
    ) -> Option<(LocalUsageSnapshot, usize)> {
        self.last_aggregation
            .as_ref()
            .filter(|last| {
                !scan.cache_dirty
                    && scan.diagnostics.discovery_errors == 0
                    && last.scope == *scope
                    && last.selected == scan.selected()
                    && last.dependencies == scan.dependencies
            })
            .map(|last| (last.snapshot.clone(), last.deferred_files))
    }

    fn remember_aggregation(
        &mut self,
        scope: RefreshScope,
        selected: HashSet<String>,
        dependencies: HashSet<String>,
        snapshot: &LocalUsageSnapshot,
        diagnostics: &RefreshDiagnostics,
    ) {
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

/// Detects cached or lifetime-contributing files that disappeared from disk.
/// Returns the scan to use (an incremental scan is promoted to a full scan so
/// the carried snapshot and lifetime totals are recomputed from surviving
/// files) and whether any referenced path was dropped.
fn rebalance_scan_after_deletions(
    scan: ScanContext,
    full_scan: bool,
    previous_cache_paths: &HashSet<String>,
    previous_lifetime_sources: &[String],
    codex_dir: &Path,
    date: &str,
    period_boundary: Option<&PeriodBoundary>,
) -> Result<(ScanContext, bool), SessionUsageError> {
    if scan.diagnostics.discovery_errors != 0 {
        return Ok((scan, false));
    }
    let dropped = previous_cache_paths
        .iter()
        .chain(previous_lifetime_sources.iter())
        .any(|path| !scan.candidate_by_path.contains_key(path));
    if !dropped || full_scan {
        return Ok((scan, dropped));
    }
    let cached_files: Vec<FileCache> = scan.caches.values().cloned().collect();
    let promoted = ScanContext::new_full(codex_dir, date, period_boundary, cached_files)?;
    Ok((promoted, true))
}

fn newest_rate_limit_entry(
    previous: Option<RateLimitSnapshotEntry>,
    scan: Option<RateLimitSnapshotEntry>,
) -> Option<RateLimitSnapshotEntry> {
    match (previous, scan) {
        (Some(previous), Some(scan)) => {
            if scan.timestamp_nanos >= previous.timestamp_nanos {
                Some(scan)
            } else {
                Some(previous)
            }
        }
        (previous, scan) => previous.or(scan),
    }
}

fn quota_snapshot_from_entry(entry: &RateLimitSnapshotEntry) -> Option<QuotaSnapshot> {
    let received_at = system_time_from_unix_nanos(entry.timestamp_nanos)?;
    let secondary = match entry.secondary.as_ref() {
        Some(window) => Some(quota_window_from_entry(window)?),
        None => None,
    };
    Some(QuotaSnapshot {
        limit_id: entry.limit_id.clone(),
        primary: quota_window_from_entry(&entry.primary)?,
        secondary,
        received_at,
    })
}

fn quota_window_from_entry(window: &RateLimitWindowEntry) -> Option<QuotaWindow> {
    let resets_at = u64::try_from(window.resets_at).ok()?;
    Some(QuotaWindow {
        used_percent: window.used_percent,
        window_duration: Duration::from_secs(window.window_minutes.saturating_mul(60)),
        resets_at: UNIX_EPOCH + Duration::from_secs(resets_at),
    })
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
        token_count_with_rate_limits(
            timestamp,
            total,
            last,
            source.map_or_else(|| Value::Null, |limit_id| json!({ "limit_id": limit_id })),
        )
    }

    pub(super) fn token_count_with_rate_limits(
        timestamp: &str,
        total: u64,
        last: Option<u64>,
        rate_limits: Value,
    ) -> Value {
        let mut event = json!({
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
                }
            }
        });
        event["payload"]["rate_limits"] = rate_limits;
        event
    }

    pub(super) fn codex_rate_limits(
        used_percent: f64,
        window_minutes: u64,
        resets_at: i64,
        plan_type: Option<&str>,
    ) -> Value {
        json!({
            "limit_id": "codex",
            "primary": {
                "used_percent": used_percent,
                "window_minutes": window_minutes,
                "resets_at": resets_at
            },
            "secondary": null,
            "plan_type": plan_type
        })
    }

    pub(super) fn turn_context_model(timestamp: &str, model: &str) -> Value {
        json!({
            "timestamp": timestamp,
            "type": "turn_context",
            "payload": { "model": model }
        })
    }

    /// 带分桶明细的 `token_count`：`total` 与 `last` 相同（单次请求快照）。
    pub(super) fn token_count_buckets(
        timestamp: &str,
        input: u64,
        cached_input: u64,
        output: u64,
        reasoning_output: u64,
        source: Option<&str>,
    ) -> Value {
        let usage = json!({
            "input_tokens": input,
            "cached_input_tokens": cached_input,
            "output_tokens": output,
            "reasoning_output_tokens": reasoning_output,
            "total_tokens": input + output
        });
        json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": usage,
                    "last_token_usage": usage
                },
                "rate_limits": source.map_or_else(
                    || Value::Null,
                    |limit_id| json!({ "limit_id": limit_id })
                )
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

    pub(super) fn epoch_seconds(timestamp: &str) -> i64 {
        parse_timestamp_nanos(Some(&Value::String(timestamp.to_owned())))
            .map(|nanos| nanos / 1_000_000_000)
            .unwrap_or_default()
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
    use std::fs;
    use std::time::SystemTime;

    use serde_json::Value;
    use serde_json::json;

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
    fn missing_start_day_log_shows_zero_period_usage() {
        // 窗口起点已知且本机窗口内没有任何会话文件：本期与今日一致地
        // 显示可靠的 0（本机口径），而不是 --。
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
                value.0.current_period_tokens,
            )),
            Some((0, true, true, 0))
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
    fn watcher_retry_recovers_incremental_monitor_and_requires_full_scan() {
        let context = TestContext::new("watcher-retry");
        let mut tracker = context.tracker();
        tracker.change_monitor = ChangeMonitor::Retry;
        tracker.full_scan_required = false;

        let recovered = tracker.retry_watcher();

        assert!(
            recovered
                && tracker.change_monitor.supports_incremental()
                && tracker.full_scan_required
        );
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
            lifetime_selected: HashSet::new(),
            dependencies: HashSet::new(),
            diagnostics: RefreshDiagnostics {
                discovery_errors: 1,
                ..RefreshDiagnostics::default()
            },
            cache_dirty: false,
        };

        let snapshot = scan.finish("2026-08-10", None, None, None, None).snapshot;

        assert!(!snapshot.today_reliable);
    }

    #[test]
    fn lifetime_sums_sequential_resume_rollouts_of_same_thread() {
        let context = TestContext::new("resume-lifetime");
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

        assert!(result.is_ok_and(|value| {
            value.0.lifetime_tokens == Some(150) && value.0.lifetime_reliable
        }));
    }

    #[test]
    fn refresh_exposes_latest_rate_limit_snapshot_with_plan_type() {
        let context = TestContext::new("rate-limit-snapshot");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(1),
                    100,
                    Some(100),
                    codex_rate_limits(45.0, 10_080, 2_000_000_000, Some("plus")),
                ),
                token_count_with_rate_limits(
                    &context.at(2),
                    150,
                    Some(50),
                    codex_rate_limits(46.0, 10_080, 2_000_000_000, Some("plus")),
                ),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);
        let snapshot = result.ok().map(|value| value.0);
        let quota = snapshot.as_ref().and_then(|value| value.quota.as_ref());

        assert_eq!(
            snapshot
                .as_ref()
                .and_then(|value| value.plan_type.as_deref()),
            Some("plus")
        );
        assert!(quota.is_some_and(|quota| {
            (quota.primary.used_percent - 46.0).abs() < f64::EPSILON
                && quota.primary.window_duration == Duration::from_hours(168)
                && quota.primary.resets_at == UNIX_EPOCH + Duration::from_secs(2_000_000_000)
                && quota.secondary.is_none()
                && quota.limit_id == "codex"
        }));
        assert_eq!(
            snapshot.and_then(|value| value.quota.map(|quota| quota.received_at)),
            timestamp_as_system_time(&context.at(2))
        );
    }

    #[test]
    fn rate_limit_snapshot_ignores_regressed_timestamp() {
        let context = TestContext::new("rate-limit-regressed");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(5),
                    100,
                    Some(100),
                    codex_rate_limits(50.0, 10_080, 2_000_000_000, None),
                ),
                token_count_with_rate_limits(
                    &context.at(2),
                    150,
                    Some(50),
                    codex_rate_limits(20.0, 10_080, 2_000_000_000, None),
                ),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert!(result.is_ok_and(|value| {
            value
                .0
                .quota
                .is_some_and(|quota| (quota.primary.used_percent - 50.0).abs() < f64::EPSILON)
        }));
    }

    #[test]
    fn rate_limit_snapshot_ignores_non_codex_limit() {
        let context = TestContext::new("rate-limit-other-source");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(1),
                    100,
                    Some(100),
                    json!({
                        "limit_id": "other",
                        "primary": {
                            "used_percent": 90.0,
                            "window_minutes": 60,
                            "resets_at": 2_000_000_000
                        },
                        "plan_type": "pro"
                    }),
                ),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert!(
            result.is_ok_and(|value| { value.0.quota.is_none() && value.0.plan_type.is_none() })
        );
    }

    #[test]
    fn rate_limit_snapshot_survives_without_today_selection() {
        let context = TestContext::new("rate-limit-no-today-selection");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(1),
                    100,
                    Some(100),
                    codex_rate_limits(30.0, 10_080, 2_000_000_000, Some("pro")),
                ),
            ],
        );
        let old_times = std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH);
        assert!(
            std::fs::File::options()
                .write(true)
                .open(&file)
                .and_then(|file| file.set_times(old_times))
                .is_ok()
        );
        let today = local_calendar_date();
        let result = context.tracker().refresh_for_date(&today);

        assert!(result.is_ok_and(|value| {
            value
                .0
                .quota
                .is_some_and(|quota| (quota.primary.used_percent - 30.0).abs() < f64::EPSILON)
        }));
    }

    #[test]
    fn lifetime_counts_fork_usage_once() {
        let context = TestContext::new("lifetime-fork");
        let parent = context.rollout(PARENT_ID);
        let child = context.rollout(CHILD_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(&context.at(-10), PARENT_ID, Some("openai"), None),
                token_count(&context.at(-9), 100, Some(100), Some("codex")),
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

        assert_eq!(
            result
                .ok()
                .map(|value| (value.0.lifetime_tokens, value.0.lifetime_reliable)),
            Some((Some(150), true))
        );
    }

    #[test]
    fn lifetime_survives_cache_reload() {
        let context = TestContext::new("lifetime-reload");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
                token_count(&context.at(2), 180, Some(80), Some("codex")),
            ],
        );
        let mut tracker = context.tracker();
        let first = tracker.refresh_for_date(&context.date);
        drop(tracker);

        let second = context.tracker().refresh_for_date(&context.date);

        assert_eq!(
            (
                first.ok().and_then(|value| value.0.lifetime_tokens),
                second.ok().and_then(|value| value.0.lifetime_tokens)
            ),
            (Some(180), Some(180))
        );
    }

    #[test]
    fn stale_cache_version_is_discarded() {
        let context = TestContext::new("cache-version-discard");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(1),
                    100,
                    Some(100),
                    codex_rate_limits(46.0, 10_080, 2_000_000_000, Some("plus")),
                ),
            ],
        );
        assert!(
            context
                .cache
                .parent()
                .is_some_and(|parent| fs::create_dir_all(parent).is_ok())
        );
        let stale = json!({
            "version": CACHE_VERSION - 1,
            "codex_home": path_key(&context.root),
            "date": "2000-01-01",
            "latest_rate_limits": null,
            "lifetime": null,
            "files": []
        });
        assert!(
            fs::write(
                &context.cache,
                serde_json::to_string(&stale).unwrap_or_default()
            )
            .is_ok()
        );

        let result = context.tracker().refresh_for_date(&context.date);

        assert!(result.is_ok_and(|value| {
            value
                .0
                .quota
                .is_some_and(|quota| (quota.primary.used_percent - 46.0).abs() < f64::EPSILON)
        }));
    }

    #[test]
    fn deleted_file_stops_contributing_to_lifetime_and_snapshot() {
        let context = TestContext::new("deleted-file");
        let parent = context.rollout(PARENT_ID);
        let child = context.rollout(CHILD_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(1),
                    100,
                    Some(100),
                    codex_rate_limits(50.0, 10_080, 2_000_000_000, None),
                ),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta(&context.at(2), CHILD_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(3),
                    40,
                    Some(40),
                    codex_rate_limits(30.0, 10_080, 2_000_000_000, None),
                ),
            ],
        );
        let mut tracker = context.tracker();
        tracker.enable_incremental_for_test();
        let (first, _) = tracker
            .refresh_for_date(&context.date)
            .expect("first refresh should succeed");

        assert_eq!(first.lifetime_tokens, Some(140));
        assert!(
            first
                .quota
                .as_ref()
                .is_some_and(|quota| (quota.primary.used_percent - 30.0).abs() < f64::EPSILON)
        );

        assert!(fs::remove_file(&child).is_ok());
        tracker.mark_changed_for_test(child.clone());

        let (second, second_diagnostics) = tracker
            .refresh_for_date(&context.date)
            .expect("second refresh should succeed");

        assert_eq!(second_diagnostics.mode, RefreshMode::FullScan);
        assert_eq!(second_diagnostics.discovery_errors, 0);
        assert_eq!(second.lifetime_tokens, Some(100));
        assert!(
            second
                .quota
                .as_ref()
                .is_some_and(|quota| (quota.primary.used_percent - 50.0).abs() < f64::EPSILON)
        );
    }

    #[test]
    fn deleted_file_snapshot_does_not_survive_cache_reload() {
        let context = TestContext::new("deleted-file-reload");
        let parent = context.rollout(PARENT_ID);
        let child = context.rollout(CHILD_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(1),
                    100,
                    Some(100),
                    codex_rate_limits(50.0, 10_080, 2_000_000_000, None),
                ),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta(&context.at(2), CHILD_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(3),
                    40,
                    Some(40),
                    codex_rate_limits(30.0, 10_080, 2_000_000_000, None),
                ),
            ],
        );
        let mut tracker = context.tracker();
        let _ = tracker.refresh_for_date(&context.date);
        drop(tracker);
        assert!(fs::remove_file(&child).is_ok());

        // Simulate a restart: the on-disk cache still carries the deleted
        // file's newer snapshot, and the watcher reports the removal.
        let mut tracker = context.tracker();
        tracker.enable_incremental_for_test();
        tracker.mark_changed_for_test(child.clone());
        let (reloaded, _) = tracker
            .refresh_for_date(&context.date)
            .expect("reloaded refresh should succeed");

        assert!(
            reloaded
                .quota
                .as_ref()
                .is_some_and(|quota| (quota.primary.used_percent - 50.0).abs() < f64::EPSILON)
        );
        assert_eq!(reloaded.lifetime_tokens, Some(100));
    }

    #[test]
    fn deleted_trimmed_snapshot_source_stops_winning() {
        let context = TestContext::new("deleted-trimmed-snapshot");
        let old = context.rollout(PARENT_ID);
        let recent = context.rollout(CHILD_ID);
        // Both files predate today, so neither is selected by date; the old
        // file is additionally not selected by mtime and gets trimmed from
        // the cache while its snapshot stays the persisted newest one.
        write_jsonl(
            &old,
            &[
                session_meta(&context.at(-172_900), PARENT_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(-172_800 + 100),
                    100,
                    Some(100),
                    codex_rate_limits(50.0, 10_080, 2_000_000_000, None),
                ),
            ],
        );
        write_jsonl(
            &recent,
            &[
                session_meta(&context.at(-172_900), CHILD_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(-172_800),
                    40,
                    Some(40),
                    codex_rate_limits(30.0, 10_080, 2_000_000_000, None),
                ),
            ],
        );
        let old_times = std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH);
        assert!(
            std::fs::File::options()
                .write(true)
                .open(&old)
                .and_then(|file| file.set_times(old_times))
                .is_ok()
        );
        let mut tracker = context.tracker();
        tracker.enable_incremental_for_test();
        let (first, _) = tracker
            .refresh_for_date(&context.date)
            .expect("first refresh should succeed");

        assert!(
            first
                .quota
                .as_ref()
                .is_some_and(|quota| (quota.primary.used_percent - 50.0).abs() < f64::EPSILON)
        );
        assert_eq!(first.lifetime_tokens, Some(140));

        assert!(fs::remove_file(&old).is_ok());
        tracker.mark_changed_for_test(old.clone());

        let (second, second_diagnostics) = tracker
            .refresh_for_date(&context.date)
            .expect("second refresh should succeed");

        assert_eq!(second_diagnostics.mode, RefreshMode::FullScan);
        assert!(
            second
                .quota
                .as_ref()
                .is_some_and(|quota| (quota.primary.used_percent - 30.0).abs() < f64::EPSILON)
        );
        assert_eq!(second.lifetime_tokens, Some(40));
    }

    #[test]
    fn deleted_trimmed_lifetime_contributor_recomputes() {
        let context = TestContext::new("deleted-trimmed-lifetime");
        let old = context.rollout(PARENT_ID);
        let recent = context.rollout(CHILD_ID);
        write_jsonl(
            &old,
            &[
                session_meta(&context.at(-172_900), PARENT_ID, Some("openai"), None),
                token_count(&context.at(-172_800), 100, Some(100), Some("codex")),
            ],
        );
        write_jsonl(
            &recent,
            &[
                session_meta(&context.at(0), CHILD_ID, Some("openai"), None),
                token_count(&context.at(1), 40, Some(40), Some("codex")),
            ],
        );
        let old_times = std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH);
        assert!(
            std::fs::File::options()
                .write(true)
                .open(&old)
                .and_then(|file| file.set_times(old_times))
                .is_ok()
        );
        let mut tracker = context.tracker();
        tracker.enable_incremental_for_test();
        let (first, _) = tracker
            .refresh_for_date(&context.date)
            .expect("first refresh should succeed");
        assert_eq!(first.lifetime_tokens, Some(140));

        assert!(fs::remove_file(&old).is_ok());
        tracker.mark_changed_for_test(old.clone());

        let (second, second_diagnostics) = tracker
            .refresh_for_date(&context.date)
            .expect("second refresh should succeed");

        assert_eq!(second_diagnostics.mode, RefreshMode::FullScan);
        assert_eq!(second.lifetime_tokens, Some(40));
    }

    #[test]
    fn archive_move_updates_snapshot_source_path() {
        let context = TestContext::new("archive-move-source");
        let sessions_path = context.rollout(PARENT_ID);
        write_jsonl(
            &sessions_path,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(1),
                    100,
                    Some(100),
                    codex_rate_limits(50.0, 10_080, 2_000_000_000, None),
                ),
            ],
        );
        let mut tracker = context.tracker();
        tracker.enable_incremental_for_test();
        let _ = tracker.refresh_for_date(&context.date);

        let archived_path = context.archived_rollout(PARENT_ID);
        assert!(
            archived_path
                .parent()
                .is_some_and(|parent| fs::create_dir_all(parent).is_ok())
        );
        assert!(fs::rename(&sessions_path, &archived_path).is_ok());
        tracker.mark_changed_for_test(sessions_path.clone());
        tracker.mark_changed_for_test(archived_path.clone());
        let _ = tracker.refresh_for_date(&context.date);

        let persisted = fs::read(&context.cache)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let source_path = persisted
            .as_ref()
            .and_then(|cache| cache.pointer("/latest_rate_limits/source_path"))
            .and_then(Value::as_str);
        assert_eq!(source_path, Some(path_key(&archived_path).as_str()));
    }

    #[test]
    fn incomplete_discovery_carries_previous_snapshot_and_lifetime() {
        fn scan_with(discovery_errors: usize) -> ScanContext {
            ScanContext {
                candidate_by_path: HashMap::new(),
                rollout_index: HashMap::new(),
                caches: HashMap::new(),
                today_selected: HashSet::new(),
                period_selected: HashSet::new(),
                lifetime_selected: HashSet::new(),
                dependencies: HashSet::new(),
                diagnostics: RefreshDiagnostics {
                    discovery_errors,
                    ..RefreshDiagnostics::default()
                },
                cache_dirty: false,
            }
        }
        let previous_rate_limits = RateLimitSnapshotEntry {
            timestamp_nanos: 1_700_000_000_000_000_000,
            limit_id: "codex".to_owned(),
            source_path: "missing-in-candidates".to_owned(),
            primary: RateLimitWindowEntry {
                used_percent: 50.0,
                window_minutes: 10_080,
                resets_at: 2_000_000_000,
            },
            secondary: None,
            plan_type: None,
        };
        let previous_lifetime = LifetimeAggregate {
            tokens: 500,
            reliable: true,
        };

        let incomplete = scan_with(1)
            .finish(
                "2026-08-10",
                None,
                None,
                Some(previous_rate_limits.clone()),
                Some(previous_lifetime.clone()),
            )
            .snapshot;

        assert!(
            incomplete
                .quota
                .as_ref()
                .is_some_and(|quota| (quota.primary.used_percent - 50.0).abs() < f64::EPSILON)
        );
        assert_eq!(incomplete.lifetime_tokens, Some(500));

        let complete = scan_with(0)
            .finish(
                "2026-08-10",
                None,
                None,
                Some(previous_rate_limits),
                Some(previous_lifetime),
            )
            .snapshot;

        assert!(complete.quota.is_none());
        assert_eq!(complete.lifetime_tokens, Some(0));
    }

    #[test]
    fn local_refresh_computes_period_usage_on_cold_start() {
        use std::sync::{Arc, Mutex};

        use crate::quota::AppState;
        use crate::quota::pricing::PriceTable;

        use super::super::{LocalUsageStatus, refresh_local_usage};

        let context = TestContext::new("cold-start-period");
        let file = context.rollout(PARENT_ID);
        // The weekly window starts exactly at the first token event, so the
        // period aggregation has something to include once the boundary is
        // derived from the published snapshot.
        let resets_at = epoch_seconds(&context.at(1)) + 7 * 24 * 3600;
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count_with_rate_limits(
                    &context.at(1),
                    100,
                    Some(100),
                    codex_rate_limits(10.0, 10_080, resets_at, Some("plus")),
                ),
                token_count(&context.at(3), 150, Some(50), Some("codex")),
            ],
        );
        let state = Arc::new(Mutex::new(AppState::default()));
        let notify = Arc::new(|| {});
        let mut tracker = context.tracker();
        let mut local_usage = LocalUsageStatus::default();

        refresh_local_usage(
            &mut tracker,
            &mut local_usage,
            &PriceTable::default(),
            &state,
            &notify,
        );

        let current = state.lock().unwrap();
        assert!(current.snapshot.is_some());
        assert_eq!(current.current_period_tokens, Some(150));
    }

    #[test]
    fn local_refresh_shows_zero_period_usage_for_empty_fresh_window() {
        use std::sync::{Arc, Mutex};

        use crate::quota::AppState;
        use crate::quota::pricing::PriceTable;

        use super::super::{LocalUsageStatus, refresh_local_usage};

        let context = TestContext::new("empty-fresh-window");
        let file = context.rollout(PARENT_ID);
        // 周窗口起点在所有事件之后（模拟外部重置后的全新窗口），本机窗口内
        // 零用量：本期应显示可靠的 0 而不是 --。
        let resets_at = epoch_seconds(&context.at(5)) + 7 * 24 * 3600;
        let rate_limits = serde_json::json!({
            "limit_id": "codex",
            "primary": {
                "used_percent": 0.0,
                "window_minutes": 300,
                "resets_at": epoch_seconds(&context.at(4)) + 5 * 3600
            },
            "secondary": {
                "used_percent": 0.0,
                "window_minutes": 10_080,
                "resets_at": resets_at
            },
            "plan_type": "plus"
        });
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
                token_count_with_rate_limits(&context.at(2), 100, Some(100), rate_limits),
            ],
        );
        // 文件改动时间留在窗口起点之前，保证 period_selected 为空。
        let old_times = std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH);
        assert!(
            std::fs::File::options()
                .write(true)
                .open(&file)
                .and_then(|handle| handle.set_times(old_times))
                .is_ok()
        );
        let state = Arc::new(Mutex::new(AppState::default()));
        let notify = Arc::new(|| {});
        let mut tracker = context.tracker();
        let mut local_usage = LocalUsageStatus::default();
        let prices = PriceTable::from_models_dev(
            r#"{"openai":{"models":{"gpt-5.6-sol":{"cost":{"input":4,"output":20,"cache_read":0.4}}}}}"#,
        )
        .unwrap();

        refresh_local_usage(&mut tracker, &mut local_usage, &prices, &state, &notify);

        let current = state.lock().unwrap();
        assert_eq!(current.current_period_tokens, Some(0));
        assert_eq!(current.current_period_cost, Some(0.0));
        // 本机零用量时满额估算没有意义。
        assert_eq!(current.period_total_value_estimate, None);
    }
}
