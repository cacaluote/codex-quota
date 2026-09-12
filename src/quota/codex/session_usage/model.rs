use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CACHE_VERSION;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct UsageCacheV1 {
    pub(super) version: u32,
    pub(super) codex_home: String,
    pub(super) date: String,
    pub(super) files: Vec<FileCache>,
    pub(super) latest_rate_limits: Option<RateLimitSnapshotEntry>,
    pub(super) lifetime: Option<LifetimeAggregate>,
    pub(super) lifetime_sources: Vec<String>,
}

impl UsageCacheV1 {
    pub(super) fn empty(codex_dir: Option<&Path>) -> Self {
        Self {
            version: CACHE_VERSION,
            codex_home: codex_dir.map_or_else(String::new, path_key),
            date: String::new(),
            files: Vec::new(),
            latest_rate_limits: None,
            lifetime: None,
            lifetime_sources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct RateLimitWindowEntry {
    pub(super) used_percent: f64,
    pub(super) window_minutes: u64,
    pub(super) resets_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct RateLimitSnapshotEntry {
    pub(super) timestamp_nanos: i64,
    pub(super) limit_id: String,
    /// Path of the session file the snapshot was parsed from; used to drop
    /// the persisted snapshot once that file no longer exists on disk.
    pub(super) source_path: String,
    pub(super) primary: RateLimitWindowEntry,
    pub(super) secondary: Option<RateLimitWindowEntry>,
    pub(super) plan_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct LifetimeAggregate {
    pub(super) tokens: u64,
    pub(super) reliable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct FileCache {
    pub(super) path: String,
    pub(super) filename_thread_id: Option<String>,
    pub(super) creation_time: u64,
    pub(super) last_write_time: u64,
    pub(super) length: u64,
    pub(super) offset: u64,
    pub(super) root: Option<RootMeta>,
    pub(super) events: Vec<TokenEvent>,
    pub(super) high_water: Option<UsageHighWater>,
    pub(super) max_timestamp_nanos: Option<i64>,
    pub(super) token_without_timestamp: bool,
    pub(super) uncertain: bool,
    pub(super) parse_errors: usize,
    pub(super) latest_rate_limits: Option<RateLimitSnapshotEntry>,
}

impl FileCache {
    pub(super) fn empty(candidate: &CandidateFile) -> Self {
        Self {
            path: path_key(&candidate.path),
            filename_thread_id: candidate.thread_id.clone(),
            creation_time: candidate.creation_time,
            last_write_time: 0,
            length: 0,
            offset: 0,
            root: None,
            events: Vec::new(),
            high_water: None,
            max_timestamp_nanos: None,
            token_without_timestamp: false,
            uncertain: false,
            parse_errors: 0,
            latest_rate_limits: None,
        }
    }

    pub(super) fn has_same_identity(&self, candidate: &CandidateFile) -> bool {
        // Distinct same-thread rollouts (Codex resume) can share a creation
        // timestamp, so only a content-preserving rename may inherit a cache.
        self.creation_time == candidate.creation_time
            && self.filename_thread_id == candidate.thread_id
            && self.length == candidate.length
    }

    pub(super) fn can_resume(&self, candidate: &CandidateFile) -> bool {
        self.creation_time == candidate.creation_time
            && candidate.length >= self.offset
            && (candidate.length > self.length
                || (candidate.length == self.length
                    && candidate.last_write_time == self.last_write_time))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RootMeta {
    pub(super) thread_id: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) timestamp_nanos: Option<i64>,
    pub(super) parent: ParentLink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum ParentLink {
    None,
    Parent(String),
    Ambiguous,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TokenCounters {
    pub(super) input: Option<u64>,
    pub(super) cached_input: Option<u64>,
    pub(super) output: Option<u64>,
    pub(super) reasoning_output: Option<u64>,
    pub(super) total: Option<u64>,
}

impl TokenCounters {
    pub(super) fn has_value(&self) -> bool {
        self.input.is_some()
            || self.cached_input.is_some()
            || self.output.is_some()
            || self.reasoning_output.is_some()
            || self.total.is_some()
    }

    pub(super) fn effective_total(&self) -> u64 {
        self.total.unwrap_or_else(|| {
            self.input
                .unwrap_or(0)
                .saturating_add(self.output.unwrap_or(0))
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct UsageHighWater {
    pub(super) input: Option<u64>,
    pub(super) output: Option<u64>,
    pub(super) total: Option<u64>,
}

impl UsageHighWater {
    pub(super) fn effective_total(&self) -> u64 {
        self.total.unwrap_or_else(|| {
            self.input
                .unwrap_or(0)
                .saturating_add(self.output.unwrap_or(0))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TokenSignature {
    pub(super) total: Option<TokenCounters>,
    pub(super) last: Option<TokenCounters>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TokenEvent {
    pub(super) timestamp_nanos: Option<i64>,
    pub(super) signature: TokenSignature,
    pub(super) source: Option<String>,
    pub(super) delta_total: u64,
}

#[derive(Debug, Clone)]
pub(super) struct CandidateFile {
    pub(super) path: PathBuf,
    pub(super) thread_id: Option<String>,
    pub(super) creation_time: u64,
    pub(super) last_write_time: u64,
    pub(super) length: u64,
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn cache_deserialization_accepts_fields_removed_from_v1_runtime_model() {
        let value = json!({
            "version": CACHE_VERSION,
            "codex_home": "C:\\\\Users\\\\test\\\\.codex",
            "date": "2026-08-10",
            "latest_rate_limits": null,
            "lifetime": null,
            "lifetime_sources": [],
            "files": [{
                "path": "rollout.jsonl",
                "filename_thread_id": null,
                "creation_time": 1,
                "last_write_time": 2,
                "length": 3,
                "offset": 3,
                "root_meta_seen": true,
                "root": null,
                "events": [{
                    "timestamp_nanos": 1,
                    "signature": { "total": null, "last": null },
                    "source": null,
                    "has_total": false,
                    "delta_total": 0
                }],
                "high_water": {
                    "input": 1,
                    "cached_input": 2,
                    "output": 3,
                    "reasoning_output": 4,
                    "total": 4
                },
                "max_timestamp_nanos": 1,
                "token_without_timestamp": false,
                "uncertain": false,
                "parse_errors": 0,
                "latest_rate_limits": null
            }]
        });

        let parsed = serde_json::from_value::<UsageCacheV1>(value);

        assert!(parsed.is_ok());
    }
}
