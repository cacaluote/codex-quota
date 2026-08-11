use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Storage::FileSystem::{
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

use super::SessionUsageError;
use super::model::{CandidateFile, UsageCacheV1};
use crate::quota::codex::protocol::local_calendar_date_at;

const MAX_DISCOVERY_DEPTH: usize = 3;

pub(super) struct Discovery {
    pub(super) candidates: Vec<CandidateFile>,
    pub(super) errors: usize,
}

pub(super) fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(|profile| PathBuf::from(profile).join(".codex"))
        })
}

pub(super) fn discover_candidates(codex_dir: &Path) -> Result<Discovery, SessionUsageError> {
    let mut paths = Vec::new();
    let mut errors = 0usize;
    let sessions = codex_dir.join("sessions");
    if existing_directory(&sessions).map_err(SessionUsageError::Discovery)? {
        collect_jsonl_recursive(&sessions, 0, &mut paths, &mut errors)
            .map_err(SessionUsageError::Discovery)?;
    }
    let archived = codex_dir.join("archived_sessions");
    if existing_directory(&archived).map_err(SessionUsageError::Discovery)? {
        let entries = fs::read_dir(&archived).map_err(SessionUsageError::Discovery)?;
        for entry in entries {
            match entry {
                Ok(entry) if is_jsonl(&entry.path()) => paths.push(entry.path()),
                Ok(_) => {}
                Err(_) => errors = errors.saturating_add(1),
            }
        }
    }
    paths.sort();
    paths.dedup();

    let mut candidates = Vec::with_capacity(paths.len());
    for path in paths {
        match path.metadata() {
            Ok(metadata) => candidates.push(CandidateFile {
                thread_id: thread_id_from_filename(&path),
                creation_time: metadata.creation_time(),
                last_write_time: metadata.last_write_time(),
                length: metadata.file_size(),
                path,
            }),
            Err(_) => errors = errors.saturating_add(1),
        }
    }
    Ok(Discovery { candidates, errors })
}

fn existing_directory(path: &Path) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn collect_jsonl_recursive(
    directory: &Path,
    depth: usize,
    paths: &mut Vec<PathBuf>,
    errors: &mut usize,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let Ok(entry) = entry else {
            *errors = errors.saturating_add(1);
            continue;
        };
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            *errors = errors.saturating_add(1);
            continue;
        };
        if file_type.is_dir() && depth < MAX_DISCOVERY_DEPTH {
            if collect_jsonl_recursive(&path, depth + 1, paths, errors).is_err() {
                *errors = errors.saturating_add(1);
            }
        } else if is_jsonl(&path) {
            paths.push(path);
        }
    }
    Ok(())
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str) == Some("jsonl")
}

pub(super) fn is_date_partition(path: &Path, date: &str) -> bool {
    let mut parts = date.split('-');
    let (Some(year), Some(month), Some(day)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let components: Vec<_> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    components.windows(4).any(|window| {
        window[0].eq_ignore_ascii_case("sessions")
            && window[1] == year
            && window[2] == month
            && window[3] == day
    })
}

pub(super) fn modified_on_date(candidate: &CandidateFile, date: &str) -> bool {
    let modified = windows_ticks_to_system_time(candidate.last_write_time);
    modified.and_then(local_calendar_date_at).as_deref() == Some(date)
}

pub(super) fn modified_on_or_after_date(candidate: &CandidateFile, date: &str) -> bool {
    let modified = windows_ticks_to_system_time(candidate.last_write_time);
    modified
        .and_then(local_calendar_date_at)
        .is_some_and(|modified_date| modified_date.as_str() >= date)
}

fn windows_ticks_to_system_time(ticks: u64) -> Option<SystemTime> {
    const WINDOWS_EPOCH_OFFSET_SECONDS: u64 = 11_644_473_600;
    const TICKS_PER_SECOND: u64 = 10_000_000;
    let seconds = ticks / TICKS_PER_SECOND;
    let unix_seconds = seconds.checked_sub(WINDOWS_EPOCH_OFFSET_SECONDS)?;
    let nanos = u32::try_from((ticks % TICKS_PER_SECOND) * 100).ok()?;
    Some(UNIX_EPOCH + Duration::new(unix_seconds, nanos))
}

pub(super) fn open_session_file(path: &Path) -> io::Result<File> {
    let share_mode = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;
    OpenOptions::new()
        .read(true)
        .share_mode(share_mode)
        .open(path)
}

pub(super) fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

pub(super) fn load_cache(path: &Path) -> Option<UsageCacheV1> {
    let mut file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(super) fn save_cache(path: &Path, cache: &UsageCacheV1) -> Result<(), io::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "缓存路径没有父目录"))?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec(cache).map_err(io::Error::other)?)?;
    let from = wide_null(temp.as_os_str());
    let to = wide_null(path.as_os_str());
    // SAFETY: both buffers are NUL-terminated and remain alive for the duration of the call.
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(io::Error::other)?;
    }
    Ok(())
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn thread_id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let candidate = stem.rsplit('-').take(5).collect::<Vec<_>>();
    if candidate.len() != 5 {
        return None;
    }
    let candidate = candidate.into_iter().rev().collect::<Vec<_>>().join("-");
    let valid = candidate.len() == 36
        && candidate.chars().enumerate().all(|(index, value)| {
            matches!(index, 8 | 13 | 18 | 23) && value == '-'
                || !matches!(index, 8 | 13 | 18 | 23) && value.is_ascii_hexdigit()
        });
    valid.then_some(candidate.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn shared_reader_allows_session_to_be_appended_and_archived() {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "codex-quota-shared-session-{}-{sequence}",
            std::process::id()
        ));
        let active = root.join("sessions").join("rollout.jsonl");
        let archived = root.join("archived_sessions").join("rollout.jsonl");
        assert!(
            active
                .parent()
                .is_some_and(|path| fs::create_dir_all(path).is_ok())
        );
        assert!(
            archived
                .parent()
                .is_some_and(|path| fs::create_dir_all(path).is_ok())
        );
        assert!(fs::write(&active, "{}\n").is_ok());
        let reader = open_session_file(&active).unwrap();
        let mut writer = OpenOptions::new().append(true).open(&active).unwrap();
        assert!(writeln!(writer, "{{}}").is_ok());
        drop(writer);

        let renamed = fs::rename(&active, &archived);

        drop(reader);
        let _ = fs::remove_dir_all(&root);
        assert!(renamed.is_ok());
    }
}

#[cfg(test)]
mod tracker_tests {
    use serde_json::json;

    use super::super::test_support::*;
    use super::*;

    #[test]
    fn persisted_cache_resumes_from_previous_offset() {
        let context = TestContext::new("cache-resume");
        let file = context.rollout(PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        let first = context.tracker().refresh_for_date(&context.date);
        append_jsonl(
            &file,
            &token_count(&context.at(2), 150, Some(50), Some("codex")),
        );

        let second = context.tracker().refresh_for_date(&context.date);

        assert_eq!(
            (
                first.ok().map(|value| value.0.today_tokens),
                second.ok().map(|value| value.0.today_tokens),
                context.cache.is_file()
            ),
            (Some(100), Some(150), true)
        );
    }

    #[test]
    fn cache_excludes_unrelated_session_content() {
        let context = TestContext::new("cache-privacy");
        let file = context.rollout(PARENT_ID);
        let secret = "prompt-content-must-not-enter-cache";
        write_jsonl(
            &file,
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                json!({
                    "timestamp": context.at(1),
                    "type": "response_item",
                    "payload": { "type": "message", "content": secret }
                }),
                token_count(&context.at(2), 100, Some(100), Some("codex")),
            ],
        );

        let result = context.tracker().refresh_for_date(&context.date);
        let persisted = fs::read_to_string(&context.cache).unwrap_or_default();

        assert!(result.is_ok());
        assert!(!persisted.contains(secret));
    }

    #[test]
    fn unchanged_file_is_not_read_again() {
        let context = TestContext::new("unchanged-file");
        write_jsonl(
            &context.rollout(PARENT_ID),
            &[
                session_meta(&context.at(0), PARENT_ID, Some("openai"), None),
                token_count(&context.at(1), 100, Some(100), Some("codex")),
            ],
        );
        let mut tracker = context.tracker();
        let first = tracker.refresh_for_date(&context.date);

        let second = tracker.refresh_for_date(&context.date);

        assert_eq!(
            (
                first.ok().map(|value| value.1.files_read),
                second.ok().map(|value| value.1.files_read)
            ),
            (Some(1), Some(0))
        );
    }
}
