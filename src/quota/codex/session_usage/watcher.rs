use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

pub(super) struct SessionChangeWatcher {
    _watcher: RecommendedWatcher,
    receiver: Receiver<notify::Result<Event>>,
    codex_dir: PathBuf,
}

#[derive(Debug, Default)]
pub(super) struct WatcherChanges {
    pub(super) paths: HashSet<PathBuf>,
    pub(super) requires_full_scan: bool,
}

impl WatcherChanges {
    pub(super) fn has_changes(&self) -> bool {
        self.requires_full_scan || !self.paths.is_empty()
    }
}

impl SessionChangeWatcher {
    pub(super) fn start(codex_dir: &Path) -> notify::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(sender)?;
        watcher.watch(codex_dir, RecursiveMode::Recursive)?;
        Ok(Self {
            _watcher: watcher,
            receiver,
            codex_dir: codex_dir.to_owned(),
        })
    }

    pub(super) fn drain(&self) -> WatcherChanges {
        let mut changes = WatcherChanges::default();
        for result in self.receiver.try_iter() {
            match result {
                Ok(event) => self.collect_event(&event, &mut changes),
                Err(_) => changes.requires_full_scan = true,
            }
        }
        changes
    }

    fn collect_event(&self, event: &Event, changes: &mut WatcherChanges) {
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        if event.paths.is_empty() {
            changes.requires_full_scan = true;
            return;
        }
        for path in &event.paths {
            match classify_session_path(&self.codex_dir, path) {
                SessionPathKind::Jsonl => {
                    changes.paths.insert(path.clone());
                }
                SessionPathKind::Directory => changes.requires_full_scan = true,
                SessionPathKind::Irrelevant => {}
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionPathKind {
    Jsonl,
    Directory,
    Irrelevant,
}

fn classify_session_path(codex_dir: &Path, path: &Path) -> SessionPathKind {
    let Ok(relative) = path.strip_prefix(codex_dir) else {
        return SessionPathKind::Irrelevant;
    };
    let Some(root) = relative.components().next() else {
        return SessionPathKind::Irrelevant;
    };
    let root = root.as_os_str();
    if root != OsStr::new("sessions") && root != OsStr::new("archived_sessions") {
        return SessionPathKind::Irrelevant;
    }
    if path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
    {
        SessionPathKind::Jsonl
    } else if path.extension().is_none() {
        SessionPathKind::Directory
    } else {
        SessionPathKind::Irrelevant
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn old_session_jsonl_is_tracked_regardless_of_partition_date() {
        let root = Path::new("C:\\Users\\test\\.codex");
        let path = root
            .join("sessions")
            .join("2020")
            .join("01")
            .join("01")
            .join("rollout.jsonl");

        assert_eq!(classify_session_path(root, &path), SessionPathKind::Jsonl);
    }

    #[test]
    fn session_directory_change_requires_full_scan() {
        let root = Path::new("C:\\Users\\test\\.codex");
        let path = root.join("sessions").join("2026").join("08").join("12");

        assert_eq!(
            classify_session_path(root, &path),
            SessionPathKind::Directory
        );
    }

    #[test]
    fn unrelated_codex_file_is_ignored() {
        let root = Path::new("C:\\Users\\test\\.codex");
        let path = root.join("config.toml");

        assert_eq!(
            classify_session_path(root, &path),
            SessionPathKind::Irrelevant
        );
    }

    #[test]
    fn watcher_reports_append_to_existing_old_partition_file() {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "codex-quota-watcher-{}-{sequence}",
            std::process::id()
        ));
        let file = root
            .join("sessions")
            .join("2000")
            .join("01")
            .join("01")
            .join("rollout.jsonl");
        assert!(
            file.parent()
                .is_some_and(|parent| fs::create_dir_all(parent).is_ok())
        );
        assert!(fs::write(&file, "{}\n").is_ok());
        let watcher = SessionChangeWatcher::start(&root).unwrap();
        assert!(
            OpenOptions::new()
                .append(true)
                .open(&file)
                .and_then(|mut output| writeln!(output, "{{}}"))
                .is_ok()
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let observed = loop {
            let changes = watcher.drain();
            if changes.paths.contains(&file) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };

        drop(watcher);
        let _ = fs::remove_dir_all(root);
        assert!(observed);
    }
}
