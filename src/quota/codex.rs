use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use super::model::{AppState, ConnectionStatus, QuotaSnapshot, QuotaWindow};
use crate::error::AppError;

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const NOTIFICATION_DEBOUNCE: Duration = Duration::from_millis(500);
const BACKOFF_SECONDS: [u64; 5] = [1, 2, 5, 10, 30];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerCommand {
    Refresh,
    RefreshIntervalChanged,
    Shutdown,
}

pub struct CodexWorker {
    command_tx: Sender<WorkerCommand>,
    join: Option<JoinHandle<()>>,
}

impl CodexWorker {
    pub fn spawn<F>(state: Arc<Mutex<AppState>>, notify: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        let (command_tx, command_rx) = mpsc::channel();
        let notify = Arc::new(notify);
        let join = thread::spawn(move || worker_loop(&state, &command_rx, &notify));
        Self {
            command_tx,
            join: Some(join),
        }
    }

    pub fn refresh(&self) {
        let _ = self.command_tx.send(WorkerCommand::Refresh);
    }

    pub fn refresh_interval_changed(&self) {
        let _ = self.command_tx.send(WorkerCommand::RefreshIntervalChanged);
    }

    pub fn shutdown(&mut self) {
        let _ = self.command_tx.send(WorkerCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for CodexWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum SessionOutcome {
    Shutdown,
}

#[derive(Debug, PartialEq, Eq)]
struct AccountPlanUpdate {
    plan_type: Option<String>,
}

struct SessionFailure {
    error: AppError,
    had_successful_read: bool,
}

impl SessionFailure {
    fn after_success(error: AppError) -> Self {
        Self {
            error,
            had_successful_read: true,
        }
    }
}

impl From<AppError> for SessionFailure {
    fn from(error: AppError) -> Self {
        Self {
            error,
            had_successful_read: false,
        }
    }
}

fn worker_loop<F>(
    state: &Arc<Mutex<AppState>>,
    command_rx: &Receiver<WorkerCommand>,
    notify: &Arc<F>,
) where
    F: Fn() + Send + Sync + 'static,
{
    let mut failure_count = 0_usize;

    loop {
        update_status(
            state,
            if failure_count == 0 {
                ConnectionStatus::Connecting
            } else {
                ConnectionStatus::Reconnecting {
                    attempt: u32::try_from(failure_count).unwrap_or(u32::MAX),
                }
            },
            None,
            notify,
        );

        match run_session(state, command_rx, notify) {
            Ok(SessionOutcome::Shutdown) => break,
            Err(failure) => {
                if failure.had_successful_read {
                    failure_count = 0;
                }
                let message = failure.error.to_string();
                crate::logging::log(&message);
                update_status(
                    state,
                    ConnectionStatus::Error {
                        message: message.clone(),
                    },
                    Some(message),
                    notify,
                );

                let backoff = reconnect_backoff(failure_count);
                failure_count = failure_count.saturating_add(1);
                match command_rx.recv_timeout(backoff) {
                    Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                    Ok(WorkerCommand::Refresh | WorkerCommand::RefreshIntervalChanged)
                    | Err(RecvTimeoutError::Timeout) => {}
                }
            }
        }
    }
}

fn run_session<F>(
    state: &Arc<Mutex<AppState>>,
    command_rx: &Receiver<WorkerCommand>,
    notify: &Arc<F>,
) -> Result<SessionOutcome, SessionFailure>
where
    F: Fn() + Send + Sync + 'static,
{
    let executable = find_codex_executable()?;
    let mut session = AppServerSession::spawn(&executable)?;
    let mut next_id = 1_u64;

    let initialize_id = next_request_id(&mut next_id);
    session.send(&json!({
        "method": "initialize",
        "id": initialize_id,
        "params": {
            "clientInfo": {
                "name": "codex_quota",
                "title": "Codex Quota",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    }))?;
    session.wait_for_response(initialize_id, INITIALIZE_TIMEOUT)?;
    session.send(&json!({ "method": "initialized", "params": {} }))?;

    read_and_publish(&mut session, &mut next_id, state, notify)?;
    if let Err(error) = read_account_and_publish(&mut session, &mut next_id, state, notify) {
        crate::logging::log(&format!("无法读取 Codex 方案类型：{error}"));
    }
    let mut next_refresh = refresh_deadline(state);
    let mut notification_refresh: Option<Instant> = None;

    loop {
        while let Ok(command) = command_rx.try_recv() {
            match command {
                WorkerCommand::Refresh => next_refresh = Instant::now(),
                WorkerCommand::RefreshIntervalChanged => next_refresh = refresh_deadline(state),
                WorkerCommand::Shutdown => {
                    session.shutdown();
                    return Ok(SessionOutcome::Shutdown);
                }
            }
        }

        let now = Instant::now();
        let due = notification_refresh
            .map_or(next_refresh, |notification| notification.min(next_refresh));
        if now >= due {
            read_and_publish(&mut session, &mut next_id, state, notify)
                .map_err(SessionFailure::after_success)?;
            next_refresh = refresh_deadline(state);
            notification_refresh = None;
            continue;
        }

        let wait = due
            .saturating_duration_since(now)
            .min(Duration::from_millis(250));
        match session.recv_line(wait) {
            Ok(Some(line)) => {
                if let Some(update) = parse_account_updated_notification(&line) {
                    publish_plan_type(state, update.plan_type, notify);
                }
                if is_rate_limit_notification(&line) {
                    notification_refresh = Some(debounced_refresh_deadline(Instant::now()));
                }
            }
            Ok(None) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(SessionFailure::after_success(AppError::Protocol(
                    "app-server 已关闭标准输出".to_owned(),
                )));
            }
        }
    }
}

fn read_and_publish<F>(
    session: &mut AppServerSession,
    next_id: &mut u64,
    state: &Arc<Mutex<AppState>>,
    notify: &Arc<F>,
) -> Result<(), AppError>
where
    F: Fn() + Send + Sync + 'static,
{
    let id = next_request_id(next_id);
    session.send(&json!({
        "method": "account/rateLimits/read",
        "id": id
    }))?;
    let result = session.wait_for_response(id, REQUEST_TIMEOUT)?;
    let snapshot = parse_rate_limits_result(result, SystemTime::now())?;

    if let Ok(mut current) = state.lock() {
        current.snapshot = Some(snapshot);
        current.status = ConnectionStatus::Online;
        current.last_error = None;
    }
    notify();
    Ok(())
}

fn read_account_and_publish<F>(
    session: &mut AppServerSession,
    next_id: &mut u64,
    state: &Arc<Mutex<AppState>>,
    notify: &Arc<F>,
) -> Result<(), AppError>
where
    F: Fn() + Send + Sync + 'static,
{
    let id = next_request_id(next_id);
    session.send(&json!({
        "method": "account/read",
        "id": id,
        "params": { "refreshToken": false }
    }))?;
    let result = session.wait_for_response(id, REQUEST_TIMEOUT)?;
    let plan_type = parse_account_result(result)?;
    publish_plan_type(state, plan_type, notify);
    Ok(())
}

fn publish_plan_type<F>(state: &Arc<Mutex<AppState>>, plan_type: Option<String>, notify: &Arc<F>)
where
    F: Fn() + Send + Sync + 'static,
{
    if let Ok(mut current) = state.lock() {
        current.plan_type = plan_type;
    }
    notify();
}

fn update_status<F>(
    state: &Arc<Mutex<AppState>>,
    status: ConnectionStatus,
    error: Option<String>,
    notify: &Arc<F>,
) where
    F: Fn() + Send + Sync + 'static,
{
    if let Ok(mut current) = state.lock() {
        current.status = status;
        current.last_error = error;
    }
    notify();
}

fn next_request_id(next: &mut u64) -> u64 {
    let value = *next;
    *next = next.saturating_add(1);
    value
}

fn reconnect_backoff(failure_count: usize) -> Duration {
    Duration::from_secs(BACKOFF_SECONDS[failure_count.min(BACKOFF_SECONDS.len() - 1)])
}

fn debounced_refresh_deadline(now: Instant) -> Instant {
    now + NOTIFICATION_DEBOUNCE
}

fn refresh_deadline(state: &Arc<Mutex<AppState>>) -> Instant {
    let now_system = SystemTime::now();
    let (refresh_interval, until_reset) =
        state
            .lock()
            .ok()
            .map_or((Duration::from_mins(5), None), |current| {
                let until_reset = current.snapshot.as_ref().and_then(|snapshot| {
                    [
                        snapshot.primary.resets_at.duration_since(now_system).ok(),
                        snapshot
                            .secondary
                            .as_ref()
                            .and_then(|window| window.resets_at.duration_since(now_system).ok()),
                    ]
                    .into_iter()
                    .flatten()
                    .min()
                });
                (current.quota_refresh_interval, until_reset)
            });
    Instant::now() + next_refresh_delay(refresh_interval, until_reset)
}

fn next_refresh_delay(refresh_interval: Duration, until_reset: Option<Duration>) -> Duration {
    until_reset.map_or(refresh_interval, |value| value.min(refresh_interval))
}

fn find_codex_executable() -> Result<PathBuf, AppError> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let installed = PathBuf::from(local_app_data)
            .join("Programs")
            .join("OpenAI")
            .join("Codex")
            .join("bin")
            .join("codex.exe");
        if installed.is_file() {
            return Ok(installed);
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("codex.exe");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(AppError::CliNotFound)
}

struct AppServerSession {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_rx: Receiver<String>,
    pending_lines: VecDeque<String>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    _job: JobHandle,
}

impl AppServerSession {
    fn spawn(executable: &PathBuf) -> Result<Self, AppError> {
        let mut child = Command::new(executable)
            .args(["app-server", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .map_err(|error| AppError::Spawn(error.to_string()))?;

        let job = JobHandle::new_and_assign(child.as_raw_handle()).inspect_err(|_| {
            let _ = child.kill();
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Spawn("未创建 app-server 标准输入".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Spawn("未创建 app-server 标准输出".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::Spawn("未创建 app-server 标准错误".to_owned()))?;

        let (stdout_tx, stdout_rx) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if stdout_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr_thread = thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else {
                    break;
                };
                let lowercase = line.to_ascii_lowercase();
                if !lowercase.contains("token") && !lowercase.contains("authorization") {
                    crate::logging::log(&format!("app-server: {line}"));
                }
            }
        });

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout_rx,
            pending_lines: VecDeque::new(),
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            _job: job,
        })
    }

    fn send(&mut self, message: &Value) -> Result<(), AppError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| AppError::Protocol("app-server 标准输入已关闭".to_owned()))?;
        write_json_line(stdin, message)
    }

    fn recv_line(&mut self, timeout: Duration) -> Result<Option<String>, RecvTimeoutError> {
        if let Some(line) = self.pending_lines.pop_front() {
            return Ok(Some(line));
        }
        match self.stdout_rx.recv_timeout(timeout) {
            Ok(line) => Ok(Some(line)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn wait_for_response(&mut self, id: u64, timeout: Duration) -> Result<Value, AppError> {
        wait_for_channel_response(&self.stdout_rx, &mut self.pending_lines, id, timeout)
    }

    fn shutdown(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

fn write_json_line<W: Write>(writer: &mut W, message: &Value) -> Result<(), AppError> {
    serde_json::to_writer(&mut *writer, message)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn wait_for_channel_response(
    receiver: &Receiver<String>,
    pending_lines: &mut VecDeque<String>,
    id: u64,
    timeout: Duration,
) -> Result<Value, AppError> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AppError::Timeout);
        }
        let line = match receiver.recv_timeout(remaining) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => return Err(AppError::Timeout),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(AppError::Protocol("app-server 已关闭标准输出".to_owned()));
            }
        };
        if let Some(result) = decode_response_for_id(&line, id)? {
            return Ok(result);
        }
        pending_lines.push_back(line);
    }
}

fn decode_response_for_id(line: &str, id: u64) -> Result<Option<Value>, AppError> {
    let message: Value = serde_json::from_str(line)?;
    if message.get("id").and_then(Value::as_u64) != Some(id) {
        return Ok(None);
    }
    if let Some(error) = message.get("error") {
        return Err(classify_rpc_error(error));
    }
    message
        .get("result")
        .cloned()
        .map(Some)
        .ok_or_else(|| AppError::Protocol("响应缺少 result 字段".to_owned()))
}

#[cfg(test)]
fn read_matching_response<R: BufRead>(reader: &mut R, id: u64) -> Result<Value, AppError> {
    for line in reader.lines() {
        if let Some(result) = decode_response_for_id(&line?, id)? {
            return Ok(result);
        }
    }
    Err(AppError::Protocol("输入结束前未收到对应响应".to_owned()))
}

impl Drop for AppServerSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct JobHandle(HANDLE);

impl JobHandle {
    fn new_and_assign(process: RawHandle) -> Result<Self, AppError> {
        // SAFETY: A null security descriptor and name create a private job owned by this process.
        let job = unsafe { CreateJobObjectW(None, None)? };
        let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `information` has the exact layout required by the selected information class.
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const information).cast::<c_void>(),
                u32::try_from(std::mem::size_of_val(&information))
                    .map_err(|_| AppError::Windows("Job Object 结构大小溢出".to_owned()))?,
            )?;
            AssignProcessToJobObject(job, HANDLE(process))?;
        }
        Ok(Self(job))
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // SAFETY: The handle was created by CreateJobObjectW and is closed exactly once here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRateLimit {
    limit_id: String,
    primary: Option<RawWindow>,
    secondary: Option<RawWindow>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWindow {
    used_percent: f64,
    window_duration_mins: u64,
    resets_at: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReadResult {
    rate_limits: Option<RawRateLimit>,
    #[serde(default)]
    rate_limits_by_limit_id: HashMap<String, RawRateLimit>,
}

#[derive(Debug, Deserialize)]
struct RawAccountReadResult {
    account: Option<RawAccount>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAccount {
    plan_type: Option<String>,
}

fn parse_rate_limits_result(
    value: Value,
    received_at: SystemTime,
) -> Result<QuotaSnapshot, AppError> {
    let result: RawReadResult = serde_json::from_value(value)?;
    let selected = result
        .rate_limits_by_limit_id
        .get("codex")
        .cloned()
        .or(result.rate_limits)
        .ok_or_else(|| AppError::Protocol("响应中没有 Codex 额度桶".to_owned()))?;
    let primary = selected
        .primary
        .as_ref()
        .ok_or_else(|| AppError::Protocol("Codex 额度桶缺少 primary".to_owned()))?;
    let converted_primary = convert_window(primary)?;
    let converted_secondary = selected
        .secondary
        .as_ref()
        .map(convert_window)
        .transpose()?;

    Ok(QuotaSnapshot {
        limit_id: selected.limit_id,
        primary: converted_primary,
        secondary: converted_secondary,
        received_at,
    })
}

fn parse_account_result(value: Value) -> Result<Option<String>, AppError> {
    let result: RawAccountReadResult = serde_json::from_value(value)?;
    Ok(result.account.and_then(|account| account.plan_type))
}

fn convert_window(raw: &RawWindow) -> Result<QuotaWindow, AppError> {
    let reset_seconds = u64::try_from(raw.resets_at)
        .map_err(|_| AppError::Protocol("重置时间戳不能为负数".to_owned()))?;
    Ok(QuotaWindow {
        used_percent: raw.used_percent,
        window_duration: Duration::from_secs(raw.window_duration_mins.saturating_mul(60)),
        resets_at: UNIX_EPOCH + Duration::from_secs(reset_seconds),
    })
}

fn is_rate_limit_notification(line: &str) -> bool {
    serde_json::from_str::<Value>(line).is_ok_and(|value| {
        value.get("method").and_then(Value::as_str) == Some("account/rateLimits/updated")
    })
}

fn parse_account_updated_notification(line: &str) -> Option<AccountPlanUpdate> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("method").and_then(Value::as_str) != Some("account/updated") {
        return None;
    }
    Some(AccountPlanUpdate {
        plan_type: value
            .pointer("/params/planType")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn classify_rpc_error(error: &Value) -> AppError {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("未知 RPC 错误")
        .to_owned();
    let lowercase = message.to_ascii_lowercase();
    if lowercase.contains("auth")
        || lowercase.contains("login")
        || lowercase.contains("unauthorized")
    {
        AppError::Authentication(message)
    } else {
        AppError::Protocol(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parser_accepts_primary_only_response() {
        let result = json!({
            "rateLimits": {
                "limitId": "codex",
                "primary": {
                    "usedPercent": 25,
                    "windowDurationMins": 10_080,
                    "resetsAt": 200_000
                },
                "secondary": null,
                "unknownFutureField": true
            }
        });
        let snapshot = parse_rate_limits_result(result, UNIX_EPOCH);
        assert!(
            snapshot
                .is_ok_and(|value| { (value.primary.used_percent - 25.0).abs() < f64::EPSILON })
        );
    }

    #[test]
    fn parser_prefers_codex_bucket_from_multi_bucket_response() {
        let result = json!({
            "rateLimits": {
                "limitId": "legacy",
                "primary": { "usedPercent": 90, "windowDurationMins": 60, "resetsAt": 200_000 }
            },
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "primary": { "usedPercent": 12, "windowDurationMins": 10_080, "resetsAt": 200_000 }
                },
                "other": {
                    "limitId": "other",
                    "primary": { "usedPercent": 80, "windowDurationMins": 60, "resetsAt": 200_000 }
                }
            }
        });
        let snapshot = parse_rate_limits_result(result, UNIX_EPOCH);
        assert!(snapshot.is_ok_and(|value| value.limit_id == "codex"));
    }

    #[test]
    fn parser_accepts_secondary_window() {
        let result = json!({
            "rateLimits": {
                "limitId": "codex",
                "primary": { "usedPercent": 10, "windowDurationMins": 300, "resetsAt": 200_000 },
                "secondary": { "usedPercent": 20, "windowDurationMins": 10_080, "resetsAt": 300_000 }
            }
        });
        let snapshot = parse_rate_limits_result(result, UNIX_EPOCH);
        assert!(snapshot.is_ok_and(|value| value.secondary.is_some()));
    }

    #[test]
    fn parser_rejects_missing_primary_window() {
        let result = json!({ "rateLimits": { "limitId": "codex", "primary": null } });
        let error = parse_rate_limits_result(result, UNIX_EPOCH);
        assert!(matches!(error, Err(AppError::Protocol(_))));
    }

    #[test]
    fn account_parser_reads_chatgpt_plan_type() {
        let result = json!({
            "account": {
                "type": "chatgpt",
                "email": "user@example.com",
                "planType": "pro",
                "unknownFutureField": true
            },
            "requiresOpenaiAuth": true
        });

        assert_eq!(
            parse_account_result(result).ok(),
            Some(Some("pro".to_owned()))
        );
    }

    #[test]
    fn account_parser_accepts_account_without_plan_type() {
        let result = json!({
            "account": { "type": "apiKey" },
            "requiresOpenaiAuth": true
        });

        assert_eq!(parse_account_result(result).ok(), Some(None));
    }

    #[test]
    fn account_updated_notification_reads_plan_type() {
        let update = parse_account_updated_notification(
            r#"{"method":"account/updated","params":{"authMode":"chatgpt","planType":"plus"}}"#,
        );

        assert_eq!(
            update,
            Some(AccountPlanUpdate {
                plan_type: Some("plus".to_owned())
            })
        );
    }

    #[test]
    fn account_updated_notification_clears_null_plan_type() {
        let update = parse_account_updated_notification(
            r#"{"method":"account/updated","params":{"authMode":null,"planType":null}}"#,
        );

        assert_eq!(update, Some(AccountPlanUpdate { plan_type: None }));
    }

    #[test]
    fn notification_detector_ignores_other_methods() {
        assert!(!is_rate_limit_notification(
            r#"{"method":"account/updated","params":{}}"#
        ));
    }

    #[test]
    fn protocol_writer_emits_one_json_line() {
        let mut output = Cursor::new(Vec::new());
        let result = write_json_line(&mut output, &json!({ "id": 7, "method": "ping" }));
        let bytes = output.into_inner();
        assert!(result.is_ok());
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert!(serde_json::from_slice::<Value>(&bytes).is_ok());
    }

    #[test]
    fn buffered_reader_correlates_response_id() {
        let input = concat!(
            "{\"method\":\"account/rateLimits/updated\"}\n",
            "{\"id\":1,\"result\":{\"ignored\":true}}\n",
            "{\"id\":2,\"result\":{\"matched\":true}}\n"
        );
        let mut reader = Cursor::new(input.as_bytes());
        let response = read_matching_response(&mut reader, 2);
        assert!(response.is_ok_and(|value| value["matched"] == true));
    }

    #[test]
    fn channel_response_wait_times_out() {
        let (_sender, receiver) = mpsc::channel();
        let result =
            wait_for_channel_response(&receiver, &mut VecDeque::new(), 1, Duration::from_millis(1));
        assert!(matches!(result, Err(AppError::Timeout)));
    }

    #[test]
    fn channel_response_wait_preserves_interleaved_notification() {
        let (sender, receiver) = mpsc::channel();
        let notification = r#"{"method":"account/updated","params":{"planType":"pro"}}"#.to_owned();
        assert!(sender.send(notification.clone()).is_ok());
        assert!(
            sender
                .send(r#"{"id":2,"result":{"matched":true}}"#.to_owned())
                .is_ok()
        );
        let mut pending = VecDeque::new();

        let result = wait_for_channel_response(&receiver, &mut pending, 2, Duration::from_secs(1));

        assert_eq!(
            (result.ok(), pending.pop_front()),
            (Some(json!({ "matched": true })), Some(notification))
        );
    }

    #[test]
    fn repeated_notification_pushes_debounce_deadline_forward() {
        let first = Instant::now();
        let second = first + Duration::from_millis(100);
        assert!(debounced_refresh_deadline(second) > debounced_refresh_deadline(first));
    }

    #[test]
    fn configured_interval_controls_fallback_refresh_delay() {
        assert_eq!(
            next_refresh_delay(Duration::from_mins(10), None),
            Duration::from_mins(10)
        );
    }

    #[test]
    fn reset_deadline_takes_priority_over_fallback_interval() {
        assert_eq!(
            next_refresh_delay(Duration::from_mins(30), Some(Duration::from_mins(2))),
            Duration::from_mins(2)
        );
    }

    #[test]
    fn reconnect_backoff_caps_at_thirty_seconds() {
        assert_eq!(reconnect_backoff(0), Duration::from_secs(1));
        assert_eq!(reconnect_backoff(99), Duration::from_secs(30));
    }
}
