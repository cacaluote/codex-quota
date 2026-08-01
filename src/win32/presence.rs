use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    APPMODEL_ERROR_NO_PACKAGE, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES, ERROR_SUCCESS,
    FILETIME, GetLastError, HANDLE, WAIT_FAILED,
};
use windows::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::Threading::{
    CreateEventW, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, SetEvent, WaitForMultipleObjects,
};
use windows::core::PWSTR;

use crate::error::AppError;
use crate::quota::find_codex_executable;

const CODEX_APP_PACKAGE_FAMILY: &str = "OpenAI.Codex_2p2nqsd0c76g0";
const IDLE_SCAN_INTERVAL: Duration = Duration::from_secs(2);
const CLI_MINIMUM_LIFETIME: Duration = Duration::from_millis(800);
const EXIT_GRACE_PERIOD: Duration = Duration::from_secs(2);
const CLI_PATH_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const ERROR_LOG_INTERVAL: Duration = Duration::from_mins(1);
const MAX_PROCESS_WAIT_HANDLES: usize = 63;

#[derive(Clone, Copy)]
enum WatcherCommand {
    Recheck,
    Shutdown,
}

pub(crate) struct CodexPresenceWatcher {
    command_tx: Sender<WatcherCommand>,
    wake_event: Arc<OwnedHandle>,
    join: Option<JoinHandle<()>>,
}

impl CodexPresenceWatcher {
    pub(crate) fn spawn<F>(notify: F) -> Result<Self, AppError>
    where
        F: Fn(bool) + Send + Sync + 'static,
    {
        // SAFETY: the unnamed auto-reset event is converted immediately into its sole owning guard.
        let event = unsafe { CreateEventW(None, false, false, None) }?;
        // SAFETY: CreateEventW returned a newly-owned handle which is transferred exactly once.
        let wake_event = Arc::new(unsafe { OwnedHandle::from_raw_handle(event.0) });
        let worker_event = Arc::clone(&wake_event);
        let (command_tx, command_rx) = mpsc::channel();
        let notify = Arc::new(notify);
        let join = thread::spawn(move || watcher_loop(&command_rx, &worker_event, &notify));
        Ok(Self {
            command_tx,
            wake_event,
            join: Some(join),
        })
    }

    pub(crate) fn recheck(&self) {
        self.send(WatcherCommand::Recheck);
    }

    pub(crate) fn shutdown(&mut self) {
        self.send(WatcherCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    fn send(&self, command: WatcherCommand) {
        let _ = self.command_tx.send(command);
        // SAFETY: wake_event remains live through the Arc while this method executes.
        let _ = unsafe { SetEvent(win32_handle(&self.wake_event)) };
    }
}

impl Drop for CodexPresenceWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ProcessEntry {
    pid: u32,
    parent_pid: u32,
    executable_name: String,
}

struct AppCandidate {
    pid: u32,
    parent_pid: u32,
    handle: OwnedHandle,
}

struct CliCandidate {
    handle: OwnedHandle,
    age: Duration,
}

struct PresenceScan {
    active_handles: Vec<OwnedHandle>,
    pending_cli_handles: Vec<OwnedHandle>,
    next_cli_activation: Option<Duration>,
}

impl PresenceScan {
    fn is_present(&self) -> bool {
        !self.active_handles.is_empty()
    }
}

struct ResolvedCliPath {
    path: PathBuf,
    normalized: String,
}

fn watcher_loop<F>(
    command_rx: &Receiver<WatcherCommand>,
    wake_event: &Arc<OwnedHandle>,
    notify: &Arc<F>,
) where
    F: Fn(bool) + Send + Sync + 'static,
{
    let own_pid = std::process::id();
    let mut published_presence = false;
    let mut absence_started: Option<Instant> = None;
    let mut cli_path: Option<ResolvedCliPath> = None;
    let mut next_cli_path_refresh = Instant::now();
    let mut last_error_log: Option<Instant> = None;

    loop {
        if should_shutdown(command_rx) {
            return;
        }

        let now = Instant::now();
        if now >= next_cli_path_refresh {
            cli_path = resolve_cli_path();
            next_cli_path_refresh = now + CLI_PATH_REFRESH_INTERVAL;
        }

        let scan = match scan_codex_processes(cli_path.as_ref(), own_pid) {
            Ok(scan) => scan,
            Err(error) => {
                if last_error_log.is_none_or(|last| now.duration_since(last) >= ERROR_LOG_INTERVAL)
                {
                    crate::logging::log(&format!("Codex 进程扫描失败：{error}"));
                    last_error_log = Some(now);
                }
                if wait_after_scan_error(command_rx) {
                    return;
                }
                continue;
            }
        };

        if scan.is_present() {
            absence_started = None;
            if !published_presence {
                notify(true);
                published_presence = true;
            }
            if wait_until_rescan(command_rx, wake_event, &scan.active_handles, None) {
                return;
            }
            continue;
        }

        let now = Instant::now();
        let grace_remaining = if published_presence {
            let started = *absence_started.get_or_insert(now);
            let elapsed = now.saturating_duration_since(started);
            if elapsed >= EXIT_GRACE_PERIOD {
                notify(false);
                published_presence = false;
                absence_started = None;
                None
            } else {
                Some(EXIT_GRACE_PERIOD.saturating_sub(elapsed))
            }
        } else {
            None
        };

        let timeout = minimum_timeout(
            grace_remaining,
            scan.next_cli_activation,
            IDLE_SCAN_INTERVAL,
        );
        if wait_until_rescan(
            command_rx,
            wake_event,
            &scan.pending_cli_handles,
            Some(timeout),
        ) {
            return;
        }
    }
}

fn should_shutdown(command_rx: &Receiver<WatcherCommand>) -> bool {
    loop {
        match command_rx.try_recv() {
            Ok(WatcherCommand::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => return true,
            Ok(WatcherCommand::Recheck) => {}
            Err(mpsc::TryRecvError::Empty) => return false,
        }
    }
}

fn wait_after_scan_error(command_rx: &Receiver<WatcherCommand>) -> bool {
    match command_rx.recv_timeout(IDLE_SCAN_INTERVAL) {
        Ok(WatcherCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => true,
        Ok(WatcherCommand::Recheck) | Err(RecvTimeoutError::Timeout) => false,
    }
}

fn wait_until_rescan(
    command_rx: &Receiver<WatcherCommand>,
    wake_event: &OwnedHandle,
    process_handles: &[OwnedHandle],
    requested_timeout: Option<Duration>,
) -> bool {
    let timeout = if process_handles.len() > MAX_PROCESS_WAIT_HANDLES {
        Some(requested_timeout.map_or(IDLE_SCAN_INTERVAL, |value| value.min(IDLE_SCAN_INTERVAL)))
    } else {
        requested_timeout
    };
    let handles = if process_handles.len() > MAX_PROCESS_WAIT_HANDLES {
        Vec::new()
    } else {
        process_handles.iter().map(win32_handle).collect()
    };

    if let Err(error) = wait_for_handles(wake_event, &handles, timeout) {
        crate::logging::log(&format!("等待 Codex 进程状态失败：{error}"));
        return wait_after_scan_error(command_rx);
    }
    false
}

fn wait_for_handles(
    wake_event: &OwnedHandle,
    process_handles: &[HANDLE],
    timeout: Option<Duration>,
) -> Result<(), AppError> {
    let mut handles = Vec::with_capacity(process_handles.len() + 1);
    handles.push(win32_handle(wake_event));
    handles.extend_from_slice(process_handles);
    let milliseconds = timeout.map_or(u32::MAX, duration_to_wait_millis);
    // SAFETY: every handle is owned by a guard that outlives this blocking call.
    let result = unsafe { WaitForMultipleObjects(&handles, false, milliseconds) };
    if result == WAIT_FAILED {
        return Err(AppError::Windows(format!(
            "WaitForMultipleObjects 返回错误 {}",
            unsafe { GetLastError() }.0
        )));
    }
    Ok(())
}

fn duration_to_wait_millis(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis().max(1)).unwrap_or(u32::MAX - 1)
}

fn minimum_timeout(
    first: Option<Duration>,
    second: Option<Duration>,
    fallback: Duration,
) -> Duration {
    first
        .into_iter()
        .chain(second)
        .min()
        .map_or(fallback, |value| value.min(fallback))
}

fn resolve_cli_path() -> Option<ResolvedCliPath> {
    let path = find_codex_executable().ok()?;
    Some(ResolvedCliPath {
        normalized: normalize_path(&path),
        path,
    })
}

fn scan_codex_processes(
    cli_path: Option<&ResolvedCliPath>,
    own_pid: u32,
) -> Result<PresenceScan, AppError> {
    let entries = enumerate_processes()?;
    let now_filetime = unsafe { GetSystemTimeAsFileTime() };
    let mut app_candidates = Vec::new();
    let mut cli_candidates = Vec::new();

    for entry in entries {
        if entry.executable_name.eq_ignore_ascii_case("ChatGPT.exe") {
            let Ok(handle) = open_process(entry.pid) else {
                continue;
            };
            if process_package_family(&handle)
                .ok()
                .flatten()
                .is_some_and(|family| is_codex_app_family(&family))
            {
                app_candidates.push(AppCandidate {
                    pid: entry.pid,
                    parent_pid: entry.parent_pid,
                    handle,
                });
            }
            continue;
        }

        let Some(expected_cli) = cli_path else {
            continue;
        };
        if !is_external_cli_candidate(&entry.executable_name, entry.parent_pid, own_pid)
            || !expected_cli.path.is_file()
        {
            continue;
        }
        let Ok(handle) = open_process(entry.pid) else {
            continue;
        };
        let Ok(image_path) = process_image_path(&handle) else {
            continue;
        };
        if normalize_path(&image_path) == expected_cli.normalized {
            cli_candidates.push(CliCandidate {
                age: process_age(&handle, now_filetime).unwrap_or_default(),
                handle,
            });
        }
    }

    let relations: Vec<_> = app_candidates
        .iter()
        .map(|candidate| (candidate.pid, candidate.parent_pid))
        .collect();
    let root_pids = app_root_pids(&relations);
    let mut active_handles: Vec<_> = app_candidates
        .into_iter()
        .filter(|candidate| root_pids.contains(&candidate.pid))
        .map(|candidate| candidate.handle)
        .collect();
    let mut pending_cli_handles = Vec::new();
    let mut next_cli_activation: Option<Duration> = None;

    for candidate in cli_candidates {
        if candidate.age >= CLI_MINIMUM_LIFETIME {
            active_handles.push(candidate.handle);
        } else {
            let remaining = CLI_MINIMUM_LIFETIME.saturating_sub(candidate.age);
            next_cli_activation =
                Some(next_cli_activation.map_or(remaining, |current| current.min(remaining)));
            pending_cli_handles.push(candidate.handle);
        }
    }

    Ok(PresenceScan {
        active_handles,
        pending_cli_handles,
        next_cli_activation,
    })
}

fn enumerate_processes() -> Result<Vec<ProcessEntry>, AppError> {
    // SAFETY: the returned snapshot handle is transferred immediately to an owning guard.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }?;
    // SAFETY: CreateToolhelp32Snapshot returned a newly-owned handle transferred exactly once.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot.0) };
    let mut raw = PROCESSENTRY32W {
        dwSize: u32::try_from(size_of::<PROCESSENTRY32W>())
            .map_err(|_| AppError::Windows("进程快照结构大小溢出".to_owned()))?,
        ..Default::default()
    };
    // SAFETY: snapshot is a process snapshot and raw points to initialized writable storage.
    if unsafe { Process32FirstW(win32_handle(&snapshot), &mut raw) }.is_err() {
        if unsafe { GetLastError() } == ERROR_NO_MORE_FILES {
            return Ok(Vec::new());
        }
        return Err(AppError::Windows(format!(
            "无法读取首个进程：{}",
            unsafe { GetLastError() }.0
        )));
    }

    let mut entries = Vec::new();
    loop {
        entries.push(ProcessEntry {
            pid: raw.th32ProcessID,
            parent_pid: raw.th32ParentProcessID,
            executable_name: wide_array_to_string(&raw.szExeFile),
        });
        // SAFETY: snapshot and raw remain valid throughout enumeration.
        if unsafe { Process32NextW(win32_handle(&snapshot), &mut raw) }.is_err() {
            if unsafe { GetLastError() } == ERROR_NO_MORE_FILES {
                break;
            }
            return Err(AppError::Windows(format!(
                "无法继续读取进程：{}",
                unsafe { GetLastError() }.0
            )));
        }
    }
    Ok(entries)
}

fn open_process(pid: u32) -> Result<OwnedHandle, AppError> {
    // SAFETY: the requested rights are read/synchronize-only and pid came from a live snapshot.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    }?;
    // SAFETY: OpenProcess returned a newly-owned handle transferred exactly once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

fn process_package_family(handle: &OwnedHandle) -> Result<Option<String>, AppError> {
    let handle = win32_handle(handle);
    let mut length = 0_u32;
    // SAFETY: the first call intentionally supplies no buffer to query the required length.
    let status = unsafe { GetPackageFamilyName(handle, &mut length, None) };
    if status == APPMODEL_ERROR_NO_PACKAGE {
        return Ok(None);
    }
    if status != ERROR_INSUFFICIENT_BUFFER || length == 0 {
        return Err(AppError::Windows(format!(
            "GetPackageFamilyName 长度查询失败：{}",
            status.0
        )));
    }
    let mut buffer = vec![0_u16; length as usize];
    // SAFETY: buffer has the exact element capacity reported by the first call and stays live.
    let status =
        unsafe { GetPackageFamilyName(handle, &mut length, Some(PWSTR(buffer.as_mut_ptr()))) };
    if status != ERROR_SUCCESS {
        return Err(AppError::Windows(format!(
            "GetPackageFamilyName 读取失败：{}",
            status.0
        )));
    }
    Ok(Some(wide_slice_to_string(&buffer)))
}

fn process_image_path(handle: &OwnedHandle) -> Result<PathBuf, AppError> {
    let mut buffer = vec![0_u16; 32_768];
    let mut length = u32::try_from(buffer.len())
        .map_err(|_| AppError::Windows("进程路径缓冲区大小溢出".to_owned()))?;
    // SAFETY: buffer is writable for length UTF-16 units and handle has query rights.
    unsafe {
        QueryFullProcessImageNameW(
            win32_handle(handle),
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )?;
    }
    buffer.truncate(length as usize);
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

fn process_age(handle: &OwnedHandle, now: FILETIME) -> Result<Duration, AppError> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all FILETIME outputs are valid writable storage and the handle has query rights.
    unsafe {
        GetProcessTimes(
            win32_handle(handle),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )?;
    }
    let ticks = filetime_ticks(now).saturating_sub(filetime_ticks(creation));
    Ok(Duration::from_nanos(ticks.saturating_mul(100)))
}

fn filetime_ticks(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

fn app_root_pids(relations: &[(u32, u32)]) -> HashSet<u32> {
    let package_pids: HashSet<_> = relations.iter().map(|(pid, _)| *pid).collect();
    relations
        .iter()
        .filter(|(_, parent_pid)| !package_pids.contains(parent_pid))
        .map(|(pid, _)| *pid)
        .collect()
}

fn is_codex_app_family(family: &str) -> bool {
    family == CODEX_APP_PACKAGE_FAMILY
}

fn is_external_cli_candidate(executable_name: &str, parent_pid: u32, own_pid: u32) -> bool {
    parent_pid != own_pid && executable_name.eq_ignore_ascii_case("codex.exe")
}

fn normalize_path(path: &Path) -> String {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let value = resolved.to_string_lossy().to_lowercase().replace('/', "\\");
    value
        .strip_prefix("\\\\?\\")
        .map_or(value.clone(), str::to_owned)
}

fn wide_array_to_string(value: &[u16]) -> String {
    wide_slice_to_string(value)
}

fn wide_slice_to_string(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn win32_handle(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_root_selection_ignores_same_package_children_in_any_order() {
        let roots = app_root_pids(&[(102, 101), (100, 50), (101, 100)]);

        assert_eq!(roots, HashSet::from([100]));
    }

    #[test]
    fn app_root_selection_preserves_independent_package_roots() {
        let roots = app_root_pids(&[(100, 50), (101, 100), (200, 60)]);

        assert_eq!(roots, HashSet::from([100, 200]));
    }

    #[test]
    fn cli_lifetime_below_threshold_remains_pending() {
        let age = Duration::from_millis(799);

        assert!(age < CLI_MINIMUM_LIFETIME);
    }

    #[test]
    fn cli_lifetime_at_threshold_is_active() {
        let age = Duration::from_millis(800);

        assert!(age >= CLI_MINIMUM_LIFETIME);
    }

    #[test]
    fn quota_owned_app_server_is_not_an_external_cli_candidate() {
        assert!(!is_external_cli_candidate("codex.exe", 42, 42));
    }

    #[test]
    fn similarly_named_chatgpt_package_is_rejected() {
        assert!(!is_codex_app_family("OpenAI.ChatGPT_2p2nqsd0c76g0"));
    }

    #[test]
    fn grace_period_wins_over_idle_interval_when_equal() {
        assert_eq!(
            minimum_timeout(Some(EXIT_GRACE_PERIOD), None, IDLE_SCAN_INTERVAL),
            EXIT_GRACE_PERIOD
        );
    }
}
