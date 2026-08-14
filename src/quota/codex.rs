mod app_server;
mod protocol;
mod session_usage;

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use app_server::AppServerSession;
pub(crate) use app_server::find_codex_executable;
use protocol::{
    is_rate_limit_notification, local_calendar_date, local_calendar_date_at, parse_account_result,
    parse_account_updated_notification, parse_lifetime_usage, parse_rate_limits_result,
};
use serde_json::json;
use session_usage::SessionUsageTracker;

use super::model::{AppState, ConnectionStatus};
use crate::error::AppError;

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const NOTIFICATION_DEBOUNCE: Duration = Duration::from_millis(500);
const LOCAL_USAGE_DEBOUNCE: Duration = Duration::from_millis(300);
const BACKOFF_SECONDS: [u64; 5] = [1, 2, 5, 10, 30];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerCommand {
    Refresh,
    RefreshIntervalChanged,
    Shutdown,
}

pub struct CodexWorker {
    command_tx: Sender<WorkerCommand>,
    cancelled: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl CodexWorker {
    pub fn spawn<F>(state: Arc<Mutex<AppState>>, notify: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        let (command_tx, command_rx) = mpsc::channel();
        let notify = Arc::new(notify);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let join = thread::spawn(move || {
            worker_loop(&state, &command_rx, &notify, &worker_cancelled);
        });
        Self {
            command_tx,
            cancelled,
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
        self.cancelled.store(true, Ordering::Release);
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

struct SessionFailure {
    error: AppError,
    had_successful_read: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PeriodBoundary {
    start: SystemTime,
    local_date: String,
    start_nanos: i64,
}

impl PeriodBoundary {
    fn from_start(start: SystemTime) -> Option<Self> {
        let local_date = local_calendar_date_at(start)?;
        let start_nanos = i64::try_from(
            start
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()?
                .as_nanos(),
        )
        .ok()?;
        Some(Self {
            start,
            local_date,
            start_nanos,
        })
    }

    fn local_date(&self) -> &str {
        &self.local_date
    }

    fn start_nanos(&self) -> i64 {
        self.start_nanos
    }
}

#[derive(Debug, Default)]
struct LocalUsageStatus {
    date: String,
    period_boundary: Option<PeriodBoundary>,
    last_error: Option<String>,
}

impl LocalUsageStatus {
    fn synchronize_period_boundary(&mut self, period_boundary: Option<PeriodBoundary>) -> bool {
        if self.period_boundary == period_boundary {
            false
        } else {
            self.period_boundary = period_boundary;
            true
        }
    }
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
    cancelled: &Arc<AtomicBool>,
) where
    F: Fn() + Send + Sync + 'static,
{
    let mut failure_count = 0_usize;
    let mut usage_tracker = SessionUsageTracker::new();
    let mut local_usage = LocalUsageStatus::default();

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

        match run_session(
            state,
            command_rx,
            notify,
            cancelled,
            &mut usage_tracker,
            &mut local_usage,
        ) {
            Ok(SessionOutcome::Shutdown) => break,
            Err(failure) => {
                if matches!(&failure.error, AppError::Cancelled) {
                    break;
                }
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
                if current_period_boundary(state).is_none() {
                    refresh_local_usage(&mut usage_tracker, &mut local_usage, state, notify);
                }

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
    cancelled: &Arc<AtomicBool>,
    usage_tracker: &mut SessionUsageTracker,
    local_usage: &mut LocalUsageStatus,
) -> Result<SessionOutcome, SessionFailure>
where
    F: Fn() + Send + Sync + 'static,
{
    let executable = find_codex_executable()?;
    let mut session = AppServerSession::spawn(&executable, Arc::clone(cancelled))?;
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

    usage_tracker.require_full_scan();
    read_and_publish(
        &mut session,
        &mut next_id,
        state,
        notify,
        usage_tracker,
        local_usage,
    )?;
    if let Err(error) = read_account_and_publish(&mut session, &mut next_id, state, notify) {
        crate::logging::log(&format!("无法读取 Codex 方案类型：{error}"));
    }
    let mut next_refresh = refresh_deadline(state);
    let mut notification_refresh: Option<Instant> = None;
    let mut local_usage_refresh: Option<Instant> = None;

    loop {
        while let Ok(command) = command_rx.try_recv() {
            match command {
                WorkerCommand::Refresh => {
                    usage_tracker.require_full_scan();
                    next_refresh = Instant::now();
                }
                WorkerCommand::RefreshIntervalChanged => next_refresh = refresh_deadline(state),
                WorkerCommand::Shutdown => {
                    session.shutdown();
                    return Ok(SessionOutcome::Shutdown);
                }
            }
        }

        if usage_tracker.poll_changes() {
            local_usage_refresh = Some(Instant::now() + LOCAL_USAGE_DEBOUNCE);
        }

        let now = Instant::now();
        let rpc_due = notification_refresh
            .map_or(next_refresh, |notification| notification.min(next_refresh));
        let due = local_usage_refresh.map_or(rpc_due, |local| local.min(rpc_due));
        if now >= due {
            if now >= rpc_due {
                read_and_publish(
                    &mut session,
                    &mut next_id,
                    state,
                    notify,
                    usage_tracker,
                    local_usage,
                )
                .map_err(SessionFailure::after_success)?;
                next_refresh = refresh_deadline(state);
                notification_refresh = None;
                local_usage_refresh = None;
            } else {
                refresh_local_usage(usage_tracker, local_usage, state, notify);
                local_usage_refresh = None;
            }
            continue;
        }

        let wait = due
            .saturating_duration_since(now)
            .min(Duration::from_millis(100));
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
    usage_tracker: &mut SessionUsageTracker,
    local_usage: &mut LocalUsageStatus,
) -> Result<(), AppError>
where
    F: Fn() + Send + Sync + 'static,
{
    read_rate_limits_and_publish(session, next_id, state, notify)?;
    let period_boundary = current_period_boundary(state);
    if usage_tracker.needs_refresh(period_boundary.as_ref()) {
        refresh_local_usage(usage_tracker, local_usage, state, notify);
    }
    match read_lifetime_and_publish(session, next_id, state, notify) {
        Ok(()) => Ok(()),
        Err(AppError::Cancelled) => Err(AppError::Cancelled),
        Err(error) => {
            crate::logging::log(&format!("无法读取累计 Token 用量：{error}"));
            Ok(())
        }
    }
}

fn read_rate_limits_and_publish<F>(
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

fn read_lifetime_and_publish<F>(
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
        "method": "account/usage/read",
        "id": id
    }))?;
    let result = session.wait_for_response(id, REQUEST_TIMEOUT)?;
    let usage = parse_lifetime_usage(result)?;
    publish_lifetime_usage(state, usage.lifetime, notify);
    Ok(())
}

fn current_period_boundary(state: &Arc<Mutex<AppState>>) -> Option<PeriodBoundary> {
    let current = state.lock().ok()?;
    let (_, long_term) = current.snapshot.as_ref()?.quota_windows();
    let window = long_term?;
    window
        .resets_at
        .checked_sub(window.window_duration)
        .and_then(PeriodBoundary::from_start)
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

fn publish_lifetime_usage<F>(state: &Arc<Mutex<AppState>>, lifetime: Option<u64>, notify: &Arc<F>)
where
    F: Fn() + Send + Sync + 'static,
{
    let mut changed = false;
    if let Ok(mut current) = state.lock()
        && let Some(lifetime) = lifetime
        && current.lifetime_tokens != Some(lifetime)
    {
        current.lifetime_tokens = Some(lifetime);
        changed = true;
    }
    if changed {
        notify();
    }
}

fn refresh_local_usage<F>(
    tracker: &mut SessionUsageTracker,
    status: &mut LocalUsageStatus,
    state: &Arc<Mutex<AppState>>,
    notify: &Arc<F>,
) where
    F: Fn() + Send + Sync + 'static,
{
    let today = local_calendar_date();
    let mut identity_changed = false;
    if status.date != today {
        status.date.clone_from(&today);
        status.last_error = None;
        if let Ok(mut current) = state.lock()
            && current.today_tokens.take().is_some()
        {
            identity_changed = true;
        }
    }
    let period_boundary = current_period_boundary(state);
    if status.synchronize_period_boundary(period_boundary)
        && let Ok(mut current) = state.lock()
        && current.current_period_tokens.take().is_some()
    {
        identity_changed = true;
    }
    if identity_changed {
        notify();
    }

    let refresh_started = Instant::now();
    match tracker.refresh(status.period_boundary.as_ref()) {
        Ok((snapshot, diagnostics)) => {
            status.last_error = None;
            publish_local_token_usage(
                state,
                snapshot.today_reliable.then_some(snapshot.today_tokens),
                snapshot
                    .current_period_reliable
                    .then_some(snapshot.current_period_tokens),
                notify,
            );
            if should_log_local_usage_refresh(&diagnostics) {
                crate::logging::log(&local_usage_refresh_description(&diagnostics));
            }
        }
        Err(error) => {
            publish_local_token_usage(state, None, None, notify);
            let message = error.to_string();
            crate::logging::log(&format!(
                "Codex 本地用量刷新失败：总计 {}，错误 {message}",
                format_elapsed(refresh_started.elapsed())
            ));
            status.last_error = Some(message);
        }
    }
}

fn should_log_local_usage_refresh(diagnostics: &session_usage::RefreshDiagnostics) -> bool {
    diagnostics.mode != session_usage::RefreshMode::WatcherIncremental
        || diagnostics.token_events_added != 0
        || diagnostics.cache_write_failed
        || diagnostics.discovery_errors != 0
        || diagnostics.parse_errors != 0
        || diagnostics.deferred_files != 0
}

fn refresh_mode_description(diagnostics: &session_usage::RefreshDiagnostics) -> &'static str {
    match diagnostics.mode {
        session_usage::RefreshMode::FullScan => "扫描",
        session_usage::RefreshMode::WatcherIncremental => "监听",
    }
}

fn format_elapsed(duration: Duration) -> String {
    format!("{:.3} ms", duration.as_secs_f64() * 1_000.0)
}

fn format_elapsed_value(duration: Duration) -> String {
    format!("{:.3}", duration.as_secs_f64() * 1_000.0)
}

fn local_usage_refresh_description(diagnostics: &session_usage::RefreshDiagnostics) -> String {
    let mut description = format!(
        "Codex 本地用量（{}）：{}",
        refresh_mode_description(diagnostics),
        format_elapsed(diagnostics.total_elapsed)
    );
    if diagnostics.mode == session_usage::RefreshMode::FullScan || diagnostics.files_scanned != 0 {
        let operation = if diagnostics.mode == session_usage::RefreshMode::FullScan {
            "扫描"
        } else {
            "检查"
        };
        let _ = write!(
            description,
            "；{operation} {}",
            format_elapsed_value(diagnostics.discovery_elapsed)
        );
    }
    if diagnostics.files_read != 0 {
        let _ = write!(
            description,
            "，解析 {}",
            format_elapsed_value(diagnostics.read_parse_elapsed)
        );
    }
    if !diagnostics.aggregation_skipped {
        let _ = write!(
            description,
            "，聚合 {}",
            format_elapsed_value(diagnostics.aggregation_elapsed)
        );
    }
    if diagnostics.cache_write_failed {
        let _ = write!(
            description,
            "，写缓存失败 {}",
            format_elapsed_value(diagnostics.cache_write_elapsed)
        );
    } else if !diagnostics.cache_write_skipped {
        let _ = write!(
            description,
            "，写缓存 {}",
            format_elapsed_value(diagnostics.cache_write_elapsed)
        );
    }
    if diagnostics.mode == session_usage::RefreshMode::FullScan {
        let _ = write!(
            description,
            "；候选 {}，读取 {}，新增 Token 事件 {}",
            diagnostics.files_scanned, diagnostics.files_read, diagnostics.token_events_added
        );
    } else {
        let _ = write!(
            description,
            "；文件 {}/{}，新增 Token 事件 {}",
            diagnostics.files_scanned, diagnostics.files_read, diagnostics.token_events_added
        );
    }
    if diagnostics.discovery_errors != 0
        || diagnostics.parse_errors != 0
        || diagnostics.deferred_files != 0
    {
        description.push_str("；错误：");
        let mut separator = "";
        for (label, count) in [
            ("文件", diagnostics.discovery_errors),
            ("解析", diagnostics.parse_errors),
            ("待定", diagnostics.deferred_files),
        ] {
            if count != 0 {
                let _ = write!(description, "{separator}{label} {count}");
                separator = "，";
            }
        }
    }
    description
}

fn publish_local_token_usage<F>(
    state: &Arc<Mutex<AppState>>,
    today: Option<u64>,
    current_period: Option<u64>,
    notify: &Arc<F>,
) where
    F: Fn() + Send + Sync + 'static,
{
    let mut changed = false;
    if let Ok(mut current) = state.lock() {
        if current.today_tokens != today {
            current.today_tokens = today;
            changed = true;
        }
        if current.current_period_tokens != current_period {
            current.current_period_tokens = current_period;
            changed = true;
        }
    }
    if changed {
        notify();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn period_start() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_hours(500_000)
    }

    fn period_boundary(local_date: &str) -> PeriodBoundary {
        let start = period_start();
        PeriodBoundary {
            start,
            local_date: local_date.to_owned(),
            start_nanos: i64::try_from(
                start
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            )
            .unwrap(),
        }
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

    #[test]
    fn elapsed_duration_is_formatted_in_milliseconds() {
        assert_eq!(format_elapsed(Duration::from_nanos(1_234_567)), "1.235 ms");
    }

    #[test]
    fn full_scan_diagnostics_use_explicit_log_label() {
        assert_eq!(
            refresh_mode_description(&session_usage::RefreshDiagnostics::default()),
            "扫描"
        );
    }

    #[test]
    fn watcher_incremental_diagnostics_use_explicit_log_label() {
        assert_eq!(
            refresh_mode_description(&session_usage::RefreshDiagnostics {
                mode: session_usage::RefreshMode::WatcherIncremental,
                ..session_usage::RefreshDiagnostics::default()
            }),
            "监听"
        );
    }

    #[test]
    fn watcher_incremental_no_op_is_not_logged() {
        assert!(!should_log_local_usage_refresh(
            &session_usage::RefreshDiagnostics {
                mode: session_usage::RefreshMode::WatcherIncremental,
                files_scanned: 1,
                aggregation_skipped: true,
                cache_write_skipped: true,
                ..session_usage::RefreshDiagnostics::default()
            }
        ));
    }

    #[test]
    fn watcher_incremental_without_new_token_events_is_not_logged() {
        assert!(!should_log_local_usage_refresh(
            &session_usage::RefreshDiagnostics {
                mode: session_usage::RefreshMode::WatcherIncremental,
                files_scanned: 1,
                files_read: 1,
                ..session_usage::RefreshDiagnostics::default()
            }
        ));
    }

    #[test]
    fn watcher_incremental_with_new_token_events_is_logged() {
        assert!(should_log_local_usage_refresh(
            &session_usage::RefreshDiagnostics {
                mode: session_usage::RefreshMode::WatcherIncremental,
                token_events_added: 1,
                ..session_usage::RefreshDiagnostics::default()
            }
        ));
    }

    #[test]
    fn watcher_incremental_with_errors_is_logged() {
        assert!(should_log_local_usage_refresh(
            &session_usage::RefreshDiagnostics {
                mode: session_usage::RefreshMode::WatcherIncremental,
                discovery_errors: 1,
                ..session_usage::RefreshDiagnostics::default()
            }
        ));
    }

    #[test]
    fn watcher_incremental_with_cache_write_failure_is_logged() {
        assert!(should_log_local_usage_refresh(
            &session_usage::RefreshDiagnostics {
                mode: session_usage::RefreshMode::WatcherIncremental,
                cache_write_failed: true,
                ..session_usage::RefreshDiagnostics::default()
            }
        ));
    }

    #[test]
    fn full_scan_is_always_logged() {
        let diagnostics = session_usage::RefreshDiagnostics {
            aggregation_skipped: true,
            cache_write_skipped: true,
            ..session_usage::RefreshDiagnostics::default()
        };

        assert!(should_log_local_usage_refresh(&diagnostics));
    }

    #[test]
    fn watcher_incremental_with_deferred_files_is_logged() {
        let deferred = session_usage::RefreshDiagnostics {
            mode: session_usage::RefreshMode::WatcherIncremental,
            deferred_files: 1,
            ..session_usage::RefreshDiagnostics::default()
        };

        assert!(should_log_local_usage_refresh(&deferred));
    }

    #[test]
    fn compact_watcher_log_keeps_only_completed_work() {
        assert_eq!(
            local_usage_refresh_description(&session_usage::RefreshDiagnostics {
                mode: session_usage::RefreshMode::WatcherIncremental,
                files_scanned: 1,
                files_read: 1,
                discovery_elapsed: Duration::from_micros(909),
                read_parse_elapsed: Duration::from_micros(189),
                aggregation_elapsed: Duration::from_micros(728),
                cache_write_elapsed: Duration::from_micros(3_416),
                total_elapsed: Duration::from_micros(5_466),
                ..session_usage::RefreshDiagnostics::default()
            }),
            "Codex 本地用量（监听）：5.466 ms；检查 0.909，解析 0.189，聚合 0.728，写缓存 3.416；文件 1/1，新增 Token 事件 0"
        );
    }

    #[test]
    fn compact_full_scan_log_omits_skipped_phases_and_zero_errors() {
        assert_eq!(
            local_usage_refresh_description(&session_usage::RefreshDiagnostics {
                files_scanned: 176,
                discovery_elapsed: Duration::from_micros(26_420),
                total_elapsed: Duration::from_micros(30_054),
                aggregation_skipped: true,
                cache_write_skipped: true,
                ..session_usage::RefreshDiagnostics::default()
            }),
            "Codex 本地用量（扫描）：30.054 ms；扫描 26.420；候选 176，读取 0，新增 Token 事件 0"
        );
    }

    #[test]
    fn compact_refresh_log_keeps_failures_and_nonzero_errors() {
        assert_eq!(
            local_usage_refresh_description(&session_usage::RefreshDiagnostics {
                mode: session_usage::RefreshMode::WatcherIncremental,
                files_scanned: 1,
                files_read: 1,
                discovery_errors: 1,
                parse_errors: 2,
                deferred_files: 3,
                cache_write_failed: true,
                aggregation_skipped: true,
                discovery_elapsed: Duration::from_micros(900),
                read_parse_elapsed: Duration::from_micros(200),
                cache_write_elapsed: Duration::from_micros(300),
                total_elapsed: Duration::from_micros(1_500),
                ..session_usage::RefreshDiagnostics::default()
            }),
            "Codex 本地用量（监听）：1.500 ms；检查 0.900，解析 0.200，写缓存失败 0.300；文件 1/1，新增 Token 事件 0；错误：文件 1，解析 2，待定 3"
        );
    }

    #[test]
    fn lifetime_publish_does_not_change_local_usage_values() {
        let state = Arc::new(Mutex::new(AppState {
            today_tokens: Some(56_879_410),
            current_period_tokens: Some(60_050_978),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_lifetime_usage(&state, Some(40), &notify);

        assert_eq!(
            state.lock().ok().map(|state| (
                state.today_tokens,
                state.current_period_tokens,
                state.lifetime_tokens
            )),
            Some((Some(56_879_410), Some(60_050_978), Some(40)))
        );
    }

    #[test]
    fn missing_lifetime_preserves_last_successful_value() {
        let state = Arc::new(Mutex::new(AppState {
            lifetime_tokens: Some(40),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_lifetime_usage(&state, None, &notify);

        assert_eq!(
            state.lock().ok().and_then(|state| state.lifetime_tokens),
            Some(40)
        );
    }

    #[test]
    fn local_publish_sets_today_and_complete_period_together() {
        let state = Arc::new(Mutex::new(AppState::default()));
        let notify = Arc::new(|| {});

        publish_local_token_usage(&state, Some(50_000), Some(60_100_978), &notify);

        assert_eq!(
            state
                .lock()
                .ok()
                .map(|state| (state.today_tokens, state.current_period_tokens)),
            Some((Some(50_000), Some(60_100_978)))
        );
    }

    #[test]
    fn unreliable_local_snapshot_clears_previous_values() {
        let state = Arc::new(Mutex::new(AppState {
            today_tokens: Some(10),
            current_period_tokens: Some(30),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_local_token_usage(&state, None, None, &notify);

        assert_eq!(
            state
                .lock()
                .ok()
                .map(|state| (state.today_tokens, state.current_period_tokens)),
            Some((None, None))
        );
    }

    #[test]
    fn unreliable_period_does_not_clear_reliable_today() {
        let state = Arc::new(Mutex::new(AppState {
            today_tokens: Some(10),
            current_period_tokens: Some(30),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_local_token_usage(&state, Some(25), None, &notify);

        assert_eq!(
            state
                .lock()
                .ok()
                .map(|state| (state.today_tokens, state.current_period_tokens)),
            Some((Some(25), None))
        );
    }

    #[test]
    fn changed_period_boundary_updates_local_scope() {
        let mut local_usage = LocalUsageStatus {
            period_boundary: Some(period_boundary("2026-08-01")),
            ..LocalUsageStatus::default()
        };
        let next_boundary = PeriodBoundary::from_start(period_start() + Duration::from_secs(1));

        let changed = local_usage.synchronize_period_boundary(next_boundary.clone());

        assert!(changed && local_usage.period_boundary == next_boundary);
    }
}
