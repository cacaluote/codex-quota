mod app_server;
mod protocol;
mod session_usage;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use app_server::AppServerSession;
pub(crate) use app_server::find_codex_executable;
use protocol::{
    is_rate_limit_notification, local_calendar_date, local_calendar_date_at, parse_account_result,
    parse_account_updated_notification, parse_rate_limits_result, parse_token_usage,
};
use serde_json::json;
use session_usage::SessionUsageTracker;

use super::model::{AppState, ConnectionStatus};
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
    today_reliable: bool,
    period_boundary: Option<PeriodBoundary>,
    period_boundary_tokens: u64,
    period_boundary_reliable: bool,
    last_error: Option<String>,
    period_baseline: Option<PeriodUsageBaseline>,
}

#[derive(Debug)]
struct PeriodUsageBaseline {
    date: String,
    boundary: PeriodBoundary,
    middle_days_tokens: u64,
}

impl LocalUsageStatus {
    fn set_period_baseline(
        &mut self,
        date: &str,
        period_boundary: Option<PeriodBoundary>,
        middle_days_tokens: Option<u64>,
    ) {
        self.period_baseline =
            period_boundary
                .zip(middle_days_tokens)
                .map(|(boundary, middle_days_tokens)| PeriodUsageBaseline {
                    date: date.to_owned(),
                    boundary,
                    middle_days_tokens,
                });
    }

    fn synchronize_period_boundary(&mut self, period_boundary: Option<PeriodBoundary>) -> bool {
        if self.period_boundary == period_boundary {
            false
        } else {
            self.period_boundary = period_boundary;
            self.period_boundary_tokens = 0;
            self.period_boundary_reliable = false;
            self.period_baseline = None;
            true
        }
    }

    fn current_period_with_local_today(&self, local_today: u64) -> Option<u64> {
        let baseline = self.period_baseline.as_ref()?;
        let needs_today = baseline.boundary.local_date() != self.date;
        if (needs_today && !self.today_reliable)
            || !self.period_boundary_reliable
            || baseline.date != self.date
            || Some(&baseline.boundary) != self.period_boundary.as_ref()
        {
            return None;
        }
        let boundary_and_middle = self
            .period_boundary_tokens
            .saturating_add(baseline.middle_days_tokens);
        Some(if needs_today {
            boundary_and_middle.saturating_add(local_today)
        } else {
            boundary_and_middle
        })
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
        refresh_local_usage(&mut usage_tracker, &mut local_usage, state, notify);
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
    refresh_local_usage(usage_tracker, local_usage, state, notify);
    match read_usage_and_publish(session, next_id, state, notify, local_usage) {
        Ok(()) => Ok(()),
        Err(AppError::Cancelled) => Err(AppError::Cancelled),
        Err(error) => {
            crate::logging::log(&format!("无法读取 Token 用量：{error}"));
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

fn read_usage_and_publish<F>(
    session: &mut AppServerSession,
    next_id: &mut u64,
    state: &Arc<Mutex<AppState>>,
    notify: &Arc<F>,
    local_usage: &mut LocalUsageStatus,
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
    let today = local_calendar_date();
    let period_boundary = current_period_boundary(state);
    let usage = parse_token_usage(
        result,
        &today,
        period_boundary.as_ref().map(PeriodBoundary::local_date),
    )?;
    local_usage.set_period_baseline(&today, period_boundary, usage.current_period_middle_days);
    publish_rpc_token_usage(
        state,
        usage.today,
        usage.current_period,
        usage.lifetime,
        &today,
        local_usage,
        notify,
    );
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

fn publish_rpc_token_usage<F>(
    state: &Arc<Mutex<AppState>>,
    today: Option<u64>,
    rpc_current_period: Option<u64>,
    lifetime: Option<u64>,
    usage_date: &str,
    local_usage: &LocalUsageStatus,
    notify: &Arc<F>,
) where
    F: Fn() + Send + Sync + 'static,
{
    let mut changed = false;
    let local_today_reliable = local_usage.today_reliable && local_usage.date == usage_date;
    if let Ok(mut current) = state.lock() {
        if !local_today_reliable
            && let Some(today) = today
            && current.today_tokens != Some(today)
        {
            current.today_tokens = Some(today);
            changed = true;
        }
        let local_current_period =
            local_usage.current_period_with_local_today(current.today_tokens.unwrap_or(0));
        let current_period = local_current_period.or(rpc_current_period);
        if let Some(current_period) = current_period
            && current.current_period_tokens != Some(current_period)
        {
            current.current_period_tokens = Some(current_period);
            changed = true;
        }
        if let Some(lifetime) = lifetime
            && current.lifetime_tokens != Some(lifetime)
        {
            current.lifetime_tokens = Some(lifetime);
            changed = true;
        }
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
        status.today_reliable = false;
        status.last_error = None;
        status.period_baseline = None;
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

    match tracker.refresh(status.period_boundary.as_ref()) {
        Ok((snapshot, diagnostics)) => {
            status.today_reliable = snapshot.today_reliable;
            status.period_boundary_tokens = snapshot.period_boundary_tokens;
            status.period_boundary_reliable = snapshot.period_boundary_reliable;
            status.last_error = None;
            if snapshot.today_reliable || snapshot.period_boundary_reliable {
                publish_local_token_usage(
                    state,
                    snapshot.today_reliable.then_some(snapshot.today_tokens),
                    status,
                    notify,
                );
            }
            if diagnostics.parse_errors > 0
                || diagnostics.discovery_errors > 0
                || diagnostics.deferred_files > 0
                || diagnostics.cache_write_failed
            {
                crate::logging::log(&format!(
                    "Codex 本地用量扫描：候选文件 {}，读取文件 {}，Token 事件 {}，发现错误 {}，解析错误 {}，待定文件 {}，缓存写入失败 {}",
                    diagnostics.files_scanned,
                    diagnostics.files_read,
                    diagnostics.token_events,
                    diagnostics.discovery_errors,
                    diagnostics.parse_errors,
                    diagnostics.deferred_files,
                    diagnostics.cache_write_failed
                ));
            }
        }
        Err(error) => {
            status.today_reliable = false;
            status.period_boundary_reliable = false;
            let message = error.to_string();
            if status.last_error.as_deref() != Some(message.as_str()) {
                crate::logging::log(&format!("无法读取 Codex 本地用量：{message}"));
                status.last_error = Some(message);
            }
        }
    }
}

fn publish_local_token_usage<F>(
    state: &Arc<Mutex<AppState>>,
    today: Option<u64>,
    local_usage: &LocalUsageStatus,
    notify: &Arc<F>,
) where
    F: Fn() + Send + Sync + 'static,
{
    let mut changed = false;
    if let Ok(mut current) = state.lock() {
        if let Some(today) = today
            && current.today_tokens != Some(today)
        {
            current.today_tokens = Some(today);
            changed = true;
        }
        let current_period =
            local_usage.current_period_with_local_today(current.today_tokens.unwrap_or(0));
        if let Some(current_period) = current_period
            && current.current_period_tokens != Some(current_period)
        {
            current.current_period_tokens = Some(current_period);
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

    fn local_usage_status(reliable: bool, middle_days_tokens: Option<u64>) -> LocalUsageStatus {
        let period_boundary = period_boundary("2026-08-01");
        LocalUsageStatus {
            date: "2026-08-10".to_owned(),
            today_reliable: reliable,
            period_boundary: Some(period_boundary.clone()),
            period_boundary_tokens: 0,
            period_boundary_reliable: reliable,
            period_baseline: middle_days_tokens.map(|middle_days_tokens| PeriodUsageBaseline {
                date: "2026-08-10".to_owned(),
                boundary: period_boundary,
                middle_days_tokens,
            }),
            ..LocalUsageStatus::default()
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
    fn rpc_usage_does_not_override_reliable_local_today_value() {
        let state = Arc::new(Mutex::new(AppState {
            today_tokens: Some(10),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});
        let local_usage = local_usage_status(true, Some(30));

        publish_rpc_token_usage(
            &state,
            Some(20),
            Some(50),
            Some(40),
            "2026-08-10",
            &local_usage,
            &notify,
        );

        assert_eq!(
            state.lock().ok().map(|state| (
                state.today_tokens,
                state.current_period_tokens,
                state.lifetime_tokens
            )),
            Some((Some(10), Some(40), Some(40)))
        );
    }

    #[test]
    fn rpc_usage_fills_today_when_local_snapshot_is_unreliable() {
        let state = Arc::new(Mutex::new(AppState::default()));
        let notify = Arc::new(|| {});
        let local_usage = local_usage_status(false, Some(30));

        publish_rpc_token_usage(
            &state,
            Some(20),
            Some(50),
            None,
            "2026-08-10",
            &local_usage,
            &notify,
        );

        assert_eq!(
            state
                .lock()
                .ok()
                .map(|state| (state.today_tokens, state.current_period_tokens)),
            Some((Some(20), Some(50)))
        );
    }

    #[test]
    fn rpc_period_is_used_when_start_day_boundary_is_unreliable() {
        let state = Arc::new(Mutex::new(AppState {
            today_tokens: Some(25),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});
        let mut local_usage = local_usage_status(true, Some(20));
        local_usage.period_boundary_reliable = false;

        publish_rpc_token_usage(
            &state,
            Some(30),
            Some(100),
            None,
            "2026-08-10",
            &local_usage,
            &notify,
        );

        assert_eq!(
            state
                .lock()
                .ok()
                .map(|state| (state.today_tokens, state.current_period_tokens)),
            Some((Some(25), Some(100)))
        );
    }

    #[test]
    fn missing_rpc_usage_preserves_last_successful_values() {
        let state = Arc::new(Mutex::new(AppState {
            today_tokens: Some(10),
            current_period_tokens: Some(30),
            lifetime_tokens: Some(40),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});
        let local_usage = local_usage_status(false, None);

        publish_rpc_token_usage(
            &state,
            None,
            None,
            None,
            "2026-08-10",
            &local_usage,
            &notify,
        );

        assert_eq!(
            state.lock().ok().map(|state| (
                state.today_tokens,
                state.current_period_tokens,
                state.lifetime_tokens
            )),
            Some((Some(10), Some(30), Some(40)))
        );
    }

    #[test]
    fn missing_rpc_today_bucket_still_combines_history_with_local_today() {
        let state = Arc::new(Mutex::new(AppState {
            today_tokens: Some(25),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});
        let local_usage = local_usage_status(true, Some(100));

        publish_rpc_token_usage(
            &state,
            None,
            Some(100),
            None,
            "2026-08-10",
            &local_usage,
            &notify,
        );

        assert_eq!(
            state
                .lock()
                .ok()
                .and_then(|state| state.current_period_tokens),
            Some(125)
        );
    }

    #[test]
    fn current_period_combines_local_boundary_rpc_middle_and_local_today() {
        let mut local_usage = local_usage_status(true, Some(20));
        local_usage.period_boundary_tokens = 15;

        assert_eq!(local_usage.current_period_with_local_today(25), Some(60));
    }

    #[test]
    fn missing_rpc_middle_days_does_not_publish_partial_local_period() {
        let state = Arc::new(Mutex::new(AppState {
            today_tokens: Some(25),
            current_period_tokens: Some(90),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});
        let mut local_usage = local_usage_status(true, None);
        local_usage.period_boundary_tokens = 15;

        publish_rpc_token_usage(
            &state,
            None,
            None,
            None,
            "2026-08-10",
            &local_usage,
            &notify,
        );

        assert_eq!(
            state
                .lock()
                .ok()
                .and_then(|state| state.current_period_tokens),
            Some(90)
        );
    }

    #[test]
    fn local_refresh_advances_current_period_without_another_rpc_read() {
        let state = Arc::new(Mutex::new(AppState {
            today_tokens: Some(10),
            current_period_tokens: Some(110),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});
        let local_usage = local_usage_status(true, Some(100));

        publish_local_token_usage(&state, Some(25), &local_usage, &notify);

        assert_eq!(
            state
                .lock()
                .ok()
                .map(|state| (state.today_tokens, state.current_period_tokens)),
            Some((Some(25), Some(125)))
        );
    }

    #[test]
    fn mismatched_baseline_date_does_not_change_current_period() {
        let state = Arc::new(Mutex::new(AppState {
            current_period_tokens: Some(110),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});
        let mut local_usage = local_usage_status(true, Some(100));
        local_usage.date = "2026-08-11".to_owned();

        publish_local_token_usage(&state, Some(25), &local_usage, &notify);

        assert_eq!(
            state
                .lock()
                .ok()
                .and_then(|state| state.current_period_tokens),
            Some(110)
        );
    }

    #[test]
    fn changed_period_boundary_invalidates_historical_baseline() {
        let mut local_usage = local_usage_status(true, Some(100));

        let changed = local_usage.synchronize_period_boundary(PeriodBoundary::from_start(
            period_start() + Duration::from_secs(1),
        ));

        assert!(changed && local_usage.period_baseline.is_none());
    }

    #[test]
    fn rpc_usage_is_used_when_local_reliability_belongs_to_previous_date() {
        let state = Arc::new(Mutex::new(AppState {
            today_tokens: Some(10),
            current_period_tokens: Some(110),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});
        let local_usage = local_usage_status(true, Some(100));

        publish_rpc_token_usage(
            &state,
            Some(20),
            Some(120),
            None,
            "2026-08-11",
            &local_usage,
            &notify,
        );

        assert_eq!(
            state
                .lock()
                .ok()
                .map(|state| (state.today_tokens, state.current_period_tokens)),
            Some((Some(20), Some(120)))
        );
    }

    #[test]
    fn period_starting_today_uses_only_boundary_slice() {
        let mut local_usage = local_usage_status(true, Some(0));
        local_usage.period_boundary_tokens = 25;
        if let Some(baseline) = local_usage.period_baseline.as_mut() {
            baseline.boundary.local_date = "2026-08-10".to_owned();
        }
        if let Some(boundary) = local_usage.period_boundary.as_mut() {
            boundary.local_date = "2026-08-10".to_owned();
        }

        let current_period = local_usage.current_period_with_local_today(100);

        assert_eq!(current_period, Some(25));
    }

    #[test]
    fn hybrid_current_period_uses_saturating_addition() {
        let local_usage = local_usage_status(true, Some(u64::MAX));

        let current_period = local_usage.current_period_with_local_today(1);

        assert_eq!(current_period, Some(u64::MAX));
    }
}
