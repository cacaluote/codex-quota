mod app_server;
mod protocol;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use app_server::AppServerSession;
pub(crate) use app_server::find_codex_executable;
use protocol::{
    is_rate_limit_notification, local_calendar_date, parse_account_result,
    parse_account_updated_notification, parse_rate_limits_result, parse_today_tokens,
};
use serde_json::json;

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

        match run_session(state, command_rx, notify, cancelled) {
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
) -> Result<(), AppError>
where
    F: Fn() + Send + Sync + 'static,
{
    read_rate_limits_and_publish(session, next_id, state, notify)?;
    match read_usage_and_publish(session, next_id, state, notify) {
        Ok(()) => Ok(()),
        Err(AppError::Cancelled) => Err(AppError::Cancelled),
        Err(error) => {
            publish_today_tokens(state, None, notify);
            crate::logging::log(&format!("无法读取今日 Token：{error}"));
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
    let tokens = parse_today_tokens(result, &local_calendar_date())?;
    publish_today_tokens(state, tokens, notify);
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

fn publish_today_tokens<F>(state: &Arc<Mutex<AppState>>, tokens: Option<u64>, notify: &Arc<F>)
where
    F: Fn() + Send + Sync + 'static,
{
    if let Ok(mut current) = state.lock() {
        current.today_tokens = tokens;
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

#[cfg(test)]
mod tests {
    use super::*;

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
