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
    local_calendar_date, local_calendar_date_at, parse_account_result, parse_lifetime_usage,
    parse_rate_limits_result,
};
use serde_json::json;
use session_usage::SessionUsageTracker;

use super::model::{AppState, ConnectionStatus, QUOTA_STALE_FLOOR, QuotaSnapshot};
use crate::error::AppError;

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const LOCAL_USAGE_DEBOUNCE: Duration = Duration::from_millis(300);
const LOCAL_LOOP_WAKE_INTERVAL: Duration = Duration::from_millis(500);
const WATCHER_RETRY_INTERVAL: Duration = Duration::from_secs(5);
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

/// Drives the worker without a resident app-server: the local session-log
/// snapshot is the primary data source, and `codex app-server` is spawned only
/// for on-demand pulls once the snapshot grows stale or a window resets.
fn worker_loop<F>(
    state: &Arc<Mutex<AppState>>,
    command_rx: &Receiver<WorkerCommand>,
    notify: &Arc<F>,
    cancelled: &Arc<AtomicBool>,
) where
    F: Fn() + Send + Sync + 'static,
{
    let mut usage_tracker = SessionUsageTracker::new();
    let mut local_usage = LocalUsageStatus::default();
    let mut pull_failures = 0_usize;
    let mut next_pull_attempt = Instant::now();
    let mut next_watcher_retry = Instant::now() + WATCHER_RETRY_INTERVAL;
    let mut local_refresh_at: Option<Instant> = None;

    update_status(state, ConnectionStatus::Connecting, None, notify);
    usage_tracker.require_full_scan();
    refresh_local_usage(&mut usage_tracker, &mut local_usage, state, notify);

    loop {
        if Instant::now() >= next_pull_attempt {
            let pull_delay = local_pull_delay(state, SystemTime::now());
            if pull_delay.is_zero() {
                match pull_rpc_snapshot(state, notify, cancelled) {
                    Ok(()) => {
                        pull_failures = 0;
                        refresh_local_usage(&mut usage_tracker, &mut local_usage, state, notify);
                    }
                    Err(AppError::Cancelled) => break,
                    Err(error) => {
                        pull_failures = pull_failures.saturating_add(1);
                        let message = error.to_string();
                        crate::logging::log(&format!("按需读取 Codex 额度失败：{message}"));
                        // With a local snapshot the display stays valid and
                        // Online; only surface the failure when there is
                        // nothing to show at all.
                        if snapshot_received_at(state).is_none() {
                            update_status(
                                state,
                                ConnectionStatus::Error {
                                    message: message.clone(),
                                },
                                Some(message),
                                notify,
                            );
                        }
                    }
                }
                next_pull_attempt = Instant::now() + reconnect_backoff(pull_failures);
            } else {
                next_pull_attempt = Instant::now() + pull_delay;
            }
        }

        poll_local_changes(
            &mut usage_tracker,
            &mut next_watcher_retry,
            &mut local_refresh_at,
        );
        let now = Instant::now();
        let due = local_refresh_at.unwrap_or(now + LOCAL_LOOP_WAKE_INTERVAL);
        let wait = due
            .saturating_duration_since(now)
            .min(LOCAL_LOOP_WAKE_INTERVAL);
        match command_rx.recv_timeout(wait) {
            Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(WorkerCommand::Refresh) => {
                usage_tracker.require_full_scan();
                refresh_local_usage(&mut usage_tracker, &mut local_usage, state, notify);
                next_pull_attempt = Instant::now();
            }
            Ok(WorkerCommand::RefreshIntervalChanged) => {
                next_pull_attempt = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        let due_refresh = local_refresh_at.is_some_and(|deadline| Instant::now() >= deadline);
        let date_changed = local_usage.date != local_calendar_date();
        if due_refresh || date_changed {
            refresh_local_usage(&mut usage_tracker, &mut local_usage, state, notify);
            local_refresh_at = None;
        }
    }
}

fn poll_local_changes(
    usage_tracker: &mut SessionUsageTracker,
    next_watcher_retry: &mut Instant,
    local_refresh_at: &mut Option<Instant>,
) {
    let now = Instant::now();
    if now >= *next_watcher_retry {
        if usage_tracker.retry_watcher() {
            *local_refresh_at = Some(now + LOCAL_USAGE_DEBOUNCE);
        }
        *next_watcher_retry = now + WATCHER_RETRY_INTERVAL;
    }
    if usage_tracker.poll_changes() {
        *local_refresh_at = Some(now + LOCAL_USAGE_DEBOUNCE);
    }
}

fn initialize_session(session: &mut AppServerSession) -> Result<u64, AppError> {
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
    Ok(next_id)
}

/// Spawns app-server, reads the account-wide snapshot once, then tears the
/// process down again.
fn pull_rpc_snapshot<F>(
    state: &Arc<Mutex<AppState>>,
    notify: &Arc<F>,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), AppError>
where
    F: Fn() + Send + Sync + 'static,
{
    let executable = find_codex_executable()?;
    let mut session = AppServerSession::spawn(&executable, Arc::clone(cancelled))?;
    let mut next_id = initialize_session(&mut session)?;
    let result = read_rate_limits_and_publish(&mut session, &mut next_id, state, notify);
    if let Err(error) = read_account_and_publish(&mut session, &mut next_id, state, notify) {
        crate::logging::log(&format!("无法读取 Codex 方案类型：{error}"));
    }
    // The account-wide lifetime covers sessions from every device, which the
    // local aggregation cannot see; treat it as authoritative.
    if let Err(error) = read_lifetime_and_publish(&mut session, &mut next_id, state, notify) {
        crate::logging::log(&format!("无法读取累计 Token 用量：{error}"));
    }
    session.shutdown();
    result
}

fn snapshot_received_at(state: &Arc<Mutex<AppState>>) -> Option<SystemTime> {
    state.lock().ok().and_then(|current| {
        current
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.received_at)
    })
}

fn local_pull_threshold(state: &Arc<Mutex<AppState>>) -> Duration {
    state
        .lock()
        .map_or(QUOTA_STALE_FLOOR, |current| current.stale_after())
}

fn local_pull_delay(state: &Arc<Mutex<AppState>>, now: SystemTime) -> Duration {
    let threshold = local_pull_threshold(state);
    let Some(snapshot) = state
        .lock()
        .ok()
        .and_then(|current| current.snapshot.clone())
    else {
        return Duration::ZERO;
    };
    // A snapshot whose windows have been reset server-side cannot describe
    // the current windows; refresh it regardless of its age.
    if snapshot.has_expired_window(now) {
        return Duration::ZERO;
    }
    let Ok(age) = now.duration_since(snapshot.received_at) else {
        return threshold;
    };
    threshold.saturating_sub(age)
}

fn publish_local_quota<F>(
    state: &Arc<Mutex<AppState>>,
    quota: Option<QuotaSnapshot>,
    plan_type: Option<String>,
    notify: &Arc<F>,
) where
    F: Fn() + Send + Sync + 'static,
{
    // Without a locally parsed snapshot there is nothing to correct: keep the
    // current one (or none) and let the on-demand pull fill the gap.
    let Some(quota) = quota else {
        return;
    };
    let mut changed = false;
    if let Ok(mut current) = state.lock()
        && current
            .snapshot
            .as_ref()
            .is_none_or(|existing| existing.received_at <= quota.received_at)
    {
        if current.snapshot.as_ref() != Some(&quota) {
            current.snapshot = Some(quota);
            changed = true;
        }
        let merged_plan_type = plan_type.or_else(|| current.plan_type.clone());
        if current.plan_type != merged_plan_type {
            current.plan_type = merged_plan_type;
            changed = true;
        }
        if changed && !matches!(current.status, ConnectionStatus::Online) {
            current.status = ConnectionStatus::Online;
            current.last_error = None;
        }
    }
    if changed {
        notify();
    }
}

fn publish_local_lifetime<F>(state: &Arc<Mutex<AppState>>, lifetime: Option<u64>, notify: &Arc<F>)
where
    F: Fn() + Send + Sync + 'static,
{
    // The on-demand pull publishes the authoritative account-wide value; a
    // local aggregate only ever raises the display (its own events), and an
    // unreliable aggregate must not pull the displayed value back down.
    let Some(lifetime) = lifetime else {
        return;
    };
    let mut changed = false;
    if let Ok(mut current) = state.lock()
        && current
            .lifetime_tokens
            .is_none_or(|current| lifetime > current)
    {
        current.lifetime_tokens = Some(lifetime);
        changed = true;
    }
    if changed {
        notify();
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
    // An expired long-term window has been replaced server-side; deriving a
    // boundary from it would keep accumulating a period that no longer exists.
    let (_, long_term) = current.snapshot.as_ref()?.active_windows(SystemTime::now());
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
        && current
            .lifetime_tokens
            .is_none_or(|current| lifetime > current)
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
            publish_local_quota(state, snapshot.quota, snapshot.plan_type, notify);
            publish_local_token_usage(
                state,
                snapshot.today_reliable.then_some(snapshot.today_tokens),
                snapshot
                    .current_period_reliable
                    .then_some(snapshot.current_period_tokens),
                notify,
            );
            publish_local_lifetime(state, snapshot.lifetime_tokens, notify);
            if should_log_local_usage_refresh(&diagnostics) {
                crate::logging::log(&local_usage_refresh_description(&diagnostics));
            }
            // The snapshot published above can unlock a period boundary this
            // pass could not use (cold start with no prior snapshot). Rerun
            // once with it so the period usage is not stuck until the next
            // external trigger.
            let boundary_now = current_period_boundary(state);
            if status.synchronize_period_boundary(boundary_now)
                && let Ok((snapshot, diagnostics)) =
                    tracker.refresh(status.period_boundary.as_ref())
            {
                publish_local_token_usage(
                    state,
                    snapshot.today_reliable.then_some(snapshot.today_tokens),
                    snapshot
                        .current_period_reliable
                        .then_some(snapshot.current_period_tokens),
                    notify,
                );
                publish_local_lifetime(state, snapshot.lifetime_tokens, notify);
                if should_log_local_usage_refresh(&diagnostics) {
                    crate::logging::log(&local_usage_refresh_description(&diagnostics));
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::QuotaWindow;

    fn quota_snapshot(received_at: SystemTime) -> QuotaSnapshot {
        QuotaSnapshot {
            limit_id: "codex".to_owned(),
            primary: QuotaWindow {
                used_percent: 10.0,
                window_duration: Duration::from_hours(168),
                resets_at: SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000),
            },
            secondary: None,
            received_at,
        }
    }

    #[test]
    fn on_demand_pull_threshold_has_a_thirty_minute_floor() {
        let state = Arc::new(Mutex::new(AppState {
            quota_refresh_interval: Duration::from_mins(1),
            ..AppState::default()
        }));

        assert_eq!(local_pull_threshold(&state), QUOTA_STALE_FLOOR);
    }

    #[test]
    fn on_demand_pull_threshold_is_twice_the_refresh_interval_above_the_floor() {
        let state = Arc::new(Mutex::new(AppState {
            quota_refresh_interval: Duration::from_mins(30),
            ..AppState::default()
        }));

        assert_eq!(local_pull_threshold(&state), Duration::from_hours(1));
    }

    #[test]
    fn on_demand_pull_delay_uses_only_remaining_snapshot_freshness() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(quota_snapshot(now - Duration::from_mins(29))),
            quota_refresh_interval: Duration::from_mins(1),
            ..AppState::default()
        }));

        assert_eq!(local_pull_delay(&state, now), Duration::from_mins(1));
    }

    #[test]
    fn expired_window_makes_on_demand_pull_due() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let mut expired = quota_snapshot(now - Duration::from_mins(1));
        expired.primary.resets_at = now - Duration::from_mins(10);
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(expired),
            quota_refresh_interval: Duration::from_mins(30),
            ..AppState::default()
        }));

        assert_eq!(local_pull_delay(&state, now), Duration::ZERO);
    }

    #[test]
    fn newer_local_snapshot_replaces_older_snapshot() {
        let now = SystemTime::now();
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(quota_snapshot(now - Duration::from_mins(1))),
            plan_type: Some("free".to_owned()),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_local_quota(
            &state,
            Some(quota_snapshot(now)),
            Some("plus".to_owned()),
            &notify,
        );

        let current = state.lock().unwrap();
        assert_eq!(
            current
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.received_at),
            Some(now)
        );
        assert_eq!(current.plan_type.as_deref(), Some("plus"));
    }

    #[test]
    fn older_local_snapshot_keeps_newer_snapshot() {
        let now = SystemTime::now();
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(quota_snapshot(now)),
            plan_type: Some("pro".to_owned()),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_local_quota(
            &state,
            Some(quota_snapshot(now - Duration::from_mins(1))),
            Some("plus".to_owned()),
            &notify,
        );

        let current = state.lock().unwrap();
        assert_eq!(
            current
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.received_at),
            Some(now)
        );
        assert_eq!(current.plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn missing_local_quota_keeps_existing_snapshot_and_plan() {
        let now = SystemTime::now();
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(quota_snapshot(now)),
            plan_type: Some("pro".to_owned()),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_local_quota(&state, None, None, &notify);

        let current = state.lock().unwrap();
        assert!(current.snapshot.is_some());
        assert_eq!(current.plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn local_quota_without_plan_keeps_existing_plan() {
        let now = SystemTime::now();
        let state = Arc::new(Mutex::new(AppState {
            plan_type: Some("pro".to_owned()),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_local_quota(&state, Some(quota_snapshot(now)), None, &notify);

        assert_eq!(state.lock().unwrap().plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn local_quota_publish_transitions_error_status_to_online() {
        let now = SystemTime::now();
        let state = Arc::new(Mutex::new(AppState {
            status: ConnectionStatus::Error {
                message: "历史错误".to_owned(),
            },
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_local_quota(&state, Some(quota_snapshot(now)), None, &notify);

        assert!(matches!(
            state.lock().unwrap().status,
            ConnectionStatus::Online
        ));
    }

    #[test]
    fn local_lifetime_does_not_lower_the_displayed_value() {
        let state = Arc::new(Mutex::new(AppState {
            lifetime_tokens: Some(100),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_local_lifetime(&state, None, &notify);
        publish_local_lifetime(&state, Some(50), &notify);
        publish_local_lifetime(&state, Some(150), &notify);

        assert_eq!(state.lock().unwrap().lifetime_tokens, Some(150));
    }

    #[test]
    fn rpc_lifetime_updates_only_when_larger() {
        let state = Arc::new(Mutex::new(AppState {
            lifetime_tokens: Some(100),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_lifetime_usage(&state, None, &notify);
        publish_lifetime_usage(&state, Some(80), &notify);
        publish_lifetime_usage(&state, Some(120), &notify);

        assert_eq!(state.lock().unwrap().lifetime_tokens, Some(120));
    }

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
