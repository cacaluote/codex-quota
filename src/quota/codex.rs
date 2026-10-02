mod app_server;
mod protocol;
mod session_usage;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use app_server::AppServerSession;
use protocol::{
    local_calendar_date, local_calendar_date_at, parse_account_result, parse_rate_limits_result,
};
use serde_json::json;
use session_usage::{ModelVolumes, SessionUsageTracker};

use super::model::{
    AppState, ConnectionStatus, DEFAULT_REFRESH_INTERVAL, QuotaPull, QuotaSnapshot,
};
use super::pricing::PriceTable;
use crate::codex_install::find_app_server_backend;
use crate::error::AppError;

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const LOCAL_USAGE_DEBOUNCE: Duration = Duration::from_millis(300);
const LOCAL_LOOP_WAKE_INTERVAL: Duration = Duration::from_millis(500);
const WATCHER_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const PRICE_REFRESH_START_DELAY: Duration = Duration::from_secs(30);
const PRICE_REFRESH_INTERVAL: Duration = Duration::from_hours(24);
const PRICE_FAILURE_SLOW_AFTER: Duration = Duration::from_hours(6);
const PRICE_FAILURE_SLOW_INTERVAL: Duration = Duration::from_hours(6);
/// 缺价触发的补拉之间至少隔这么久：新模型发布当天 models.dev 往往还没上价，
/// 没有冷却就会每轮本地刷新都发一次请求。日调度仍在兜底。
const PRICE_GAP_MIN_INTERVAL: Duration = Duration::from_hours(1);
/// 缺价模型的补拉窗口：从首次见到它起这么久内允许按 `PRICE_GAP_MIN_INTERVAL`
/// 反复补拉（最长 6 小时），之后停手等日调度。models.dev 可能永远不会上价的
/// 模型名（本地变体后缀之类）不该让程序每小时发请求直到永远。
const PRICE_GAP_RETRY_WINDOW: Duration = Duration::from_hours(6);
const PULL_RETRY_SECONDS: [u64; 3] = [1, 2, 5];

/// 重试冷却与数据到期时间分开；额外密集重试三次后按配置间隔继续。
struct PullSchedule {
    retry_not_before: Instant,
    retry_count: usize,
}

impl PullSchedule {
    fn new(now: Instant) -> Self {
        Self {
            retry_not_before: now,
            retry_count: 0,
        }
    }

    fn delay(
        &self,
        forced: bool,
        state: &Arc<Mutex<AppState>>,
        wall_now: SystemTime,
        now: Instant,
    ) -> Duration {
        pull_delay(forced, state, wall_now)
            .max(self.retry_not_before.saturating_duration_since(now))
    }

    fn after_attempt(&mut self, needs_retry: bool, refresh_interval: Duration, now: Instant) {
        let delay = if needs_retry {
            // 旧窗口的成功响应也消耗重试次数；低频阶段不再开启密集重试。
            let delay = PULL_RETRY_SECONDS
                .get(self.retry_count)
                .map_or(refresh_interval, |&seconds| Duration::from_secs(seconds));
            self.retry_count = self
                .retry_count
                .saturating_add(1)
                .min(PULL_RETRY_SECONDS.len());
            delay
        } else {
            self.retry_count = 0;
            Duration::ZERO
        };
        self.retry_not_before = now + delay;
    }

    fn request_now(&mut self, now: Instant) {
        self.retry_not_before = now;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerCommand {
    Refresh,
    RefreshPrices,
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

    pub fn refresh_prices(&self) {
        let _ = self.command_tx.send(WorkerCommand::RefreshPrices);
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
    let mut local = LocalPipeline::new();
    let mut prices = PricePipeline::load();
    let mut pull_schedule = PullSchedule::new(Instant::now());
    // 手动刷新标记：置位后那一次拉取跳过"快照还新鲜"的判断。
    let mut forced_pull = false;
    let mut next_watcher_retry = Instant::now() + WATCHER_RETRY_INTERVAL;
    let mut local_refresh_at: Option<Instant> = None;

    update_status(state, ConnectionStatus::Connecting, None, notify);
    local.tracker.require_full_scan();
    refresh_local_usage(&mut local, &mut prices, state, notify);

    loop {
        // 每轮按当前快照重新算到期时间，避免沿用重置前安排的常规刷新时间。
        if pull_schedule
            .delay(forced_pull, state, SystemTime::now(), Instant::now())
            .is_zero()
        {
            let forced = std::mem::take(&mut forced_pull);
            let needs_retry = match pull_rpc_snapshot(state, notify, cancelled, forced) {
                Ok(()) => {
                    refresh_local_usage(&mut local, &mut prices, state, notify);
                    // RPC 成功也可能仍返回旧窗口；同样退避，直到窗口真正更新。
                    local_pull_delay(state, SystemTime::now()).is_zero()
                }
                Err(AppError::Cancelled) => break,
                Err(error) => {
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
                    true
                }
            };
            let refresh_interval = state.lock().map_or(DEFAULT_REFRESH_INTERVAL, |current| {
                current.quota_refresh_interval
            });
            pull_schedule.after_attempt(needs_retry, refresh_interval, Instant::now());
        }

        prices.refresh_if_due(&mut local_refresh_at);

        poll_local_changes(
            &mut local.tracker,
            &mut next_watcher_retry,
            &mut local_refresh_at,
        );
        let now = Instant::now();
        let due = local_refresh_at.unwrap_or(now + LOCAL_LOOP_WAKE_INTERVAL);
        let wait = due
            .saturating_duration_since(now)
            .min(LOCAL_LOOP_WAKE_INTERVAL)
            .min(pull_schedule.delay(forced_pull, state, SystemTime::now(), now));
        match command_rx.recv_timeout(wait) {
            Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(WorkerCommand::Refresh) => {
                // 手动刷新：这次不等快照变旧，直接向服务端要一次（见 pull_delay）。
                forced_pull = true;
                local.tracker.require_full_scan();
                refresh_local_usage(&mut local, &mut prices, state, notify);
                pull_schedule.request_now(Instant::now());
            }
            Ok(WorkerCommand::RefreshIntervalChanged) => {
                pull_schedule.request_now(Instant::now());
            }
            Ok(WorkerCommand::RefreshPrices) => prices.schedule_now(),
            Err(RecvTimeoutError::Timeout) => {}
        }

        let due_refresh = local_refresh_at.is_some_and(|deadline| Instant::now() >= deadline);
        let date_changed = local.status.date != local_calendar_date();
        if due_refresh || date_changed {
            refresh_local_usage(&mut local, &mut prices, state, notify);
            local_refresh_at = None;
        }
    }
}

/// 启动时首次价格拉取的延迟：缓存数据未满刷新间隔就等满间隔再拉
/// （消除频繁重启下的冗余全量拉取）；无拉取记录或缓存已超间隔，则稍等
/// 启动关键路径走完（30 秒）再拉。
fn price_refresh_delay(fetched_at: Option<u64>, now: SystemTime) -> Duration {
    let Some(fetched_at) = fetched_at else {
        return PRICE_REFRESH_START_DELAY;
    };
    let age = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(fetched_at))
        .and_then(|fetched| now.duration_since(fetched).ok())
        .unwrap_or_default(); // 时钟异常（fetched_at 在未来）按刚拉取处理
    PRICE_REFRESH_INTERVAL
        .saturating_sub(age)
        .max(PRICE_REFRESH_START_DELAY)
}

/// 网络失败先快速退避，再每小时尝试；持续失败满六小时后改为每六小时。
/// 成功后的每日刷新由调用方单独安排，失败计数不再兼作调度状态。
fn price_refresh_failure_delay(failures: usize, failed_for: Duration) -> Duration {
    const EARLY_DELAYS: [Duration; 6] = [
        Duration::from_secs(30),
        Duration::from_mins(1),
        Duration::from_mins(2),
        Duration::from_mins(5),
        Duration::from_mins(15),
        Duration::from_mins(30),
    ];
    if failed_for >= PRICE_FAILURE_SLOW_AFTER {
        return PRICE_FAILURE_SLOW_INTERVAL;
    }
    EARLY_DELAYS
        .get(failures.saturating_sub(1))
        .copied()
        .unwrap_or(Duration::from_hours(1))
}

/// 本机用量刷新这条管线的状态：会话日志解析器 + 按日/周期的显示口径。
///
/// 与 [`PricePipeline`] 一起把主循环四个刷新点的参数收到两三个：六个参数摊在
/// 各分支里，三条时基（额度快照、价格表、本地日志）会淹在参数列表里。
struct LocalPipeline {
    tracker: SessionUsageTracker,
    status: LocalUsageStatus,
}

impl LocalPipeline {
    fn new() -> Self {
        Self {
            tracker: SessionUsageTracker::new(),
            status: LocalUsageStatus::default(),
        }
    }
}

/// 价格表这条管线的状态：表本身、缺口观测、以及拉取调度（连续失败计数、
/// 首次失败时刻、下一次拉取时刻、上一次真正发起的时刻）。
struct PricePipeline {
    table: PriceTable,
    gaps: PriceGaps,
    failures: usize,
    first_failure: Option<Instant>,
    next_refresh: Instant,
    /// None 表示本次运行还没拉过：缺价触发的补拉因此不必等冷却。缓存还新鲜
    /// （下一次日调度也许在 23 小时之后）却已经在用新模型，正是最该立刻补一次
    /// 的场景。
    last_attempt: Option<Instant>,
}

impl PricePipeline {
    fn new(table: PriceTable) -> Self {
        Self {
            table,
            gaps: PriceGaps::default(),
            failures: 0,
            first_failure: None,
            next_refresh: Instant::now() + PRICE_REFRESH_START_DELAY,
            last_attempt: None,
        }
    }

    /// 读回 app-data 里上次成功拉取的价格表，并按缓存年龄排好第一次拉取。
    fn load() -> Self {
        let table = PriceTable::load();
        let delay = price_refresh_delay(table.fetched_at(), SystemTime::now());
        Self {
            next_refresh: Instant::now() + delay,
            ..Self::new(table)
        }
    }

    /// 主循环里的价格表这一步：日调度到点就拉；没到点但本机在用价格表里没有
    /// 的模型、冷却已过、也没处在失败退避里，就把下一次拉取提前到现在。
    ///
    /// 缺价补拉只改 `next_refresh`、不当场拉，是为了让拉取只有一条路径——失败
    /// 退避与"成功后重算已发布成本"都留在那一支里。
    fn refresh_if_due(&mut self, local_refresh_at: &mut Option<Instant>) {
        if Instant::now() >= self.next_refresh {
            self.last_attempt = Some(Instant::now());
            match self.table.refresh() {
                Ok(()) => {
                    self.failures = 0;
                    self.first_failure = None;
                    self.next_refresh = Instant::now() + PRICE_REFRESH_INTERVAL;
                    // 价格表更新后重算已发布的成本。
                    *local_refresh_at = Some(Instant::now() + LOCAL_USAGE_DEBOUNCE);
                }
                Err(error) => {
                    self.failures = self.failures.saturating_add(1);
                    let now = Instant::now();
                    let first = *self.first_failure.get_or_insert(now);
                    self.next_refresh = now
                        + price_refresh_failure_delay(
                            self.failures,
                            now.saturating_duration_since(first),
                        );
                    crate::logging::log(&format!("models.dev 价格刷新失败：{error}"));
                }
            }
            return;
        }
        if self.gaps.wants_pull
            // 失败退避有自己的时钟；缺价补拉不能在它到点之前插队。
            && self.failures == 0
            && self
                .last_attempt
                .is_none_or(|last| Instant::now() >= last + PRICE_GAP_MIN_INTERVAL)
        {
            self.gaps.wants_pull = false;
            self.next_refresh = Instant::now();
        }
    }

    /// 托盘"立即刷新"用：立刻尝试，但保留连续失败时长与自动退避阶段。
    fn schedule_now(&mut self) {
        self.next_refresh = Instant::now();
    }
}

/// 一个缺价模型的等待状态。
struct PendingGap {
    /// 首次观测到"有用量但没价"的时刻，补拉窗口从这里起算。
    first_seen: Instant,
    /// 超过补拉窗口后置位：不再为它提前补拉，只等日调度。
    gave_up: bool,
}

/// 价格表缺口的观测状态，worker 线程局部、随主循环存活。
///
/// 一份状态服务三件事：按模型去重的"缺价"日志、价格表补齐后的闭环日志，
/// 以及"要不要提前补拉一次"的触发与停手判定。日志按模型去重（而不是每轮
/// 本地刷新各报一次），所以它既不会刷屏，也不会因为补拉冷却而漏报。
#[derive(Default)]
struct PriceGaps {
    /// 已经报过缺价、当前仍缺价的模型名（补价后移出，于是再次缺价会重报）。
    reported: HashSet<String>,
    /// 报过缺价、还在等价格表补齐的模型名 → 等待状态。
    pending: HashMap<String, PendingGap>,
    /// 还有"未停手"的缺口：主循环据此提前拉一次价格表（冷却见
    /// `PRICE_GAP_MIN_INTERVAL`）。
    wants_pull: bool,
}

impl PriceGaps {
    /// 用一次本地刷新的观测结果更新状态，返回本次要写进日志的行。
    ///
    /// 返回文本而不是直接写日志：日志是进程级单例，只有把"该说什么"和
    /// "写到哪"分开，测试才能断言前者。
    ///
    /// `unpriced` 是本次刷新观测到的缺价模型名，可能同时来自今日与本期两个
    /// 窗口（于是有重复）；去重与"报过没有"都在这里判定。结算与观测分开：
    /// 先拿当前价格表结清所有还在等的模型——包括本次窗口里已经没有用量的
    /// 那些——否则一个用完就不再出现的模型会让程序一直以为缺口还在。
    fn observe(&mut self, unpriced: &[&str], prices: &PriceTable, now: Instant) -> Vec<String> {
        let mut lines = Vec::new();
        let mut tracked: Vec<String> = self.pending.keys().cloned().collect();
        // `HashMap` 迭代顺序不定，排序只为让日志行序稳定（测试断言得动）。
        tracked.sort_unstable();
        for model in tracked {
            if prices.lookup(Some(&model)).is_some() {
                self.pending.remove(&model);
                self.reported.remove(&model);
                lines.push(format!("价格表已补齐：{model}"));
                continue;
            }
            if let Some(gap) = self.pending.get_mut(&model)
                && !gap.gave_up
                && now.saturating_duration_since(gap.first_seen) >= PRICE_GAP_RETRY_WINDOW
            {
                gap.gave_up = true;
                lines.push(format!("价格表仍缺模型：{model}（暂停补拉，等每日刷新）"));
            }
        }
        for name in unpriced {
            // `insert` 返回 false 说明这个名字已经报过——日志只留一条开头。
            if self.reported.insert((*name).to_owned()) {
                lines.push(format!("价格表缺模型：{name}"));
            }
            self.pending
                .entry((*name).to_owned())
                .or_insert(PendingGap {
                    first_seen: now,
                    gave_up: false,
                });
        }
        self.wants_pull = self.pending.values().any(|gap| !gap.gave_up);
        lines
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
///
/// `forced` 区分手动刷新与自动拉取：球上的不确定进度弧对前者的策略不同
/// （见 `win32::presentation::ball_is_pulling`）。
fn pull_rpc_snapshot<F>(
    state: &Arc<Mutex<AppState>>,
    notify: &Arc<F>,
    cancelled: &Arc<AtomicBool>,
    forced: bool,
) -> Result<(), AppError>
where
    F: Fn() + Send + Sync + 'static,
{
    let _in_flight = PullInFlight::new(state, notify, forced);
    let backend = find_app_server_backend()?;
    crate::logging::log(&format!(
        "额度查询后端：{}，路径：{}",
        backend.source.label(),
        backend.path.display()
    ));
    let mut session = AppServerSession::spawn(&backend.path, Arc::clone(cancelled))?;
    let mut next_id = initialize_session(&mut session)?;
    let result = read_rate_limits_and_publish(&mut session, &mut next_id, state, notify);
    if let Err(error) = read_account_and_publish(&mut session, &mut next_id, state, notify) {
        crate::logging::log(&format!("无法读取 Codex 方案类型：{error}"));
    }
    session.shutdown();
    result
}

/// 把 [`AppState::quota_pull`] 标记为在途，并在离开作用域时**一定**回到 Idle。
///
/// 用 RAII 而不是在每个返回点手写复位：拉取有六七条提前返回的失败路径，漏掉
/// 任何一条，球上的旋转弧就会一直转下去——那正是这个动效最不能犯的错。
struct PullInFlight<'a, F: Fn() + Send + Sync + 'static> {
    state: &'a Arc<Mutex<AppState>>,
    notify: &'a Arc<F>,
}

impl<'a, F> PullInFlight<'a, F>
where
    F: Fn() + Send + Sync + 'static,
{
    fn new(state: &'a Arc<Mutex<AppState>>, notify: &'a Arc<F>, forced: bool) -> Self {
        let pull = if forced {
            QuotaPull::Forced
        } else {
            QuotaPull::Automatic
        };
        publish_quota_pull(state, pull, notify);
        Self { state, notify }
    }
}

impl<F: Fn() + Send + Sync + 'static> Drop for PullInFlight<'_, F> {
    fn drop(&mut self) {
        publish_quota_pull(self.state, QuotaPull::Idle, self.notify);
    }
}

/// 只在状态真的变化时写回并通知：拉取开始时若球上什么都没变，就不必多推一次
/// 重绘。
fn publish_quota_pull<F>(state: &Arc<Mutex<AppState>>, pull: QuotaPull, notify: &Arc<F>)
where
    F: Fn() + Send + Sync + 'static,
{
    let changed = state.lock().is_ok_and(|mut current| {
        if current.quota_pull == pull {
            false
        } else {
            current.quota_pull = pull;
            true
        }
    });
    if changed {
        notify();
    }
}

fn snapshot_received_at(state: &Arc<Mutex<AppState>>) -> Option<SystemTime> {
    state.lock().ok().and_then(|current| {
        current
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.received_at)
    })
}

/// 这次该等多久再拉：手动刷新（`forced`）立即拉，否则取快照新鲜度与
/// 最近窗口重置时间中较早的一个。显示门控仍在 2N，给常规拉取留出余量。
///
/// 托盘菜单的"立即刷新"必须真的向服务端要一次——否则一个叫立即刷新的按钮
/// 只重扫本地日志，用户会以为程序卡住了。
fn pull_delay(forced: bool, state: &Arc<Mutex<AppState>>, now: SystemTime) -> Duration {
    if forced {
        return Duration::ZERO;
    }
    local_pull_delay(state, now)
}

fn local_pull_delay(state: &Arc<Mutex<AppState>>, now: SystemTime) -> Duration {
    let Ok(current) = state.lock() else {
        return Duration::ZERO;
    };
    let Some(snapshot) = current.snapshot.as_ref() else {
        return Duration::ZERO;
    };
    let threshold = current.quota_refresh_interval;
    let freshness_delay = now
        .duration_since(snapshot.received_at)
        .map_or(threshold, |age| threshold.saturating_sub(age));
    let next_reset = snapshot
        .secondary
        .as_ref()
        .map_or(snapshot.primary.resets_at, |secondary| {
            snapshot.primary.resets_at.min(secondary.resets_at)
        });
    freshness_delay.min(next_reset.duration_since(now).unwrap_or_default())
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

fn current_period_boundary(state: &Arc<Mutex<AppState>>) -> Option<PeriodBoundary> {
    let current = state.lock().ok()?;
    let now = SystemTime::now();
    // 过期快照的窗口可能已被服务端替换（外部重置/自然滚动），从它推导
    // 边界会把上一期的用量当成本期聚合并短暂显示——等按需 pull 纠正。
    if current.is_stale(now) {
        return None;
    }
    // An expired long-term window has been replaced server-side; deriving a
    // boundary from it would keep accumulating a period that no longer exists.
    let (_, long_term) = current.snapshot.as_ref()?.active_windows(now);
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

fn refresh_local_usage<F>(
    local: &mut LocalPipeline,
    prices: &mut PricePipeline,
    state: &Arc<Mutex<AppState>>,
    notify: &Arc<F>,
) where
    F: Fn() + Send + Sync + 'static,
{
    let today = local_calendar_date();
    let mut identity_changed = false;
    if local.status.date != today {
        local.status.date.clone_from(&today);
        local.status.last_error = None;
        if let Ok(mut current) = state.lock() {
            // `||` 会短路，必须把各字段都清掉后再判断是否需要通知。
            let cleared = [
                current.today_tokens.take().is_some(),
                current.today_cache_hit_percent_tenths.take().is_some(),
                current.today_overflow_tokens.take().is_some(),
                current.today_overflow_cost.take().is_some(),
            ];
            identity_changed |= cleared.into_iter().any(|was_present| was_present);
        }
    }
    let period_boundary = current_period_boundary(state);
    if local.status.synchronize_period_boundary(period_boundary)
        && let Ok(mut current) = state.lock()
    {
        let cleared = [
            current.current_period_tokens.take().is_some(),
            current
                .current_period_cache_hit_percent_tenths
                .take()
                .is_some(),
            current.current_period_overflow_tokens.take().is_some(),
            current.current_period_overflow_cost.take().is_some(),
        ];
        identity_changed |= cleared.into_iter().any(|was_present| was_present);
    }
    if identity_changed {
        notify();
    }

    let refresh_started = Instant::now();
    match local.tracker.refresh(local.status.period_boundary.as_ref()) {
        Ok((snapshot, diagnostics)) => {
            local.status.last_error = None;
            publish_local_quota(
                state,
                snapshot.quota.clone(),
                snapshot.plan_type.clone(),
                notify,
            );
            publish_local_usage_snapshot(state, &snapshot, prices, notify);
            if should_log_local_usage_refresh(&diagnostics) {
                crate::logging::log(&local_usage_refresh_description(&diagnostics));
            }
            // The snapshot published above can unlock a period boundary this
            // pass could not use (cold start with no prior snapshot). Rerun
            // once with it so the period usage is not stuck until the next
            // external trigger.
            let boundary_now = current_period_boundary(state);
            if local.status.synchronize_period_boundary(boundary_now)
                && let Ok((snapshot, diagnostics)) =
                    local.tracker.refresh(local.status.period_boundary.as_ref())
            {
                publish_local_usage_snapshot(state, &snapshot, prices, notify);
                if should_log_local_usage_refresh(&diagnostics) {
                    crate::logging::log(&local_usage_refresh_description(&diagnostics));
                }
            }
        }
        Err(error) => {
            publish_local_token_usage(state, None, None, notify);
            publish_local_cache_hit_percent_tenths(state, None, None, notify);
            publish_local_cost(state, None, None, None, notify);
            publish_local_overflow(state, OverflowUsage::default(), notify);
            let message = error.to_string();
            crate::logging::log(&format!(
                "Codex 本地用量刷新失败：总计 {}，错误 {message}",
                format_elapsed(refresh_started.elapsed())
            ));
            local.status.last_error = Some(message);
        }
    }
}

/// 发布一次本地聚合快照的显示值：套餐内价值/估值、溢出（tokens + credits
/// 实扣）、token 用量与累计。
fn publish_local_usage_snapshot<F>(
    state: &Arc<Mutex<AppState>>,
    snapshot: &session_usage::LocalUsageSnapshot,
    prices: &mut PricePipeline,
    notify: &Arc<F>,
) where
    F: Fn() + Send + Sync + 'static,
{
    let today_cost = snapshot
        .today_volume
        .as_ref()
        .and_then(|v| v.cost(&prices.table));
    let period_cost = snapshot
        .period_volume
        .as_ref()
        .and_then(|volumes| volumes.cost(&prices.table));
    let today_cache_hit_percent_tenths = snapshot
        .today_volume
        .as_ref()
        .and_then(ModelVolumes::cache_hit_percent_tenths);
    let period_cache_hit_percent_tenths = snapshot
        .period_volume
        .as_ref()
        .and_then(ModelVolumes::cache_hit_percent_tenths);
    // 成本静默少算的唯一出口：把"有用量但价格表里没有"的模型交给缺口状态机
    // （按模型名去重、并决定要不要提前补拉一次）。放在发布显示值之前，日志的
    // 时序才与"这一份快照"对应。
    let mut unpriced: Vec<&str> = Vec::new();
    for volumes in [
        snapshot.today_volume.as_ref(),
        snapshot.period_volume.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        unpriced.extend(volumes.unpriced_models(&prices.table));
    }
    for line in prices
        .gaps
        .observe(&unpriced, &prices.table, Instant::now())
    {
        crate::logging::log(&line);
    }
    publish_local_cost(
        state,
        today_cost,
        period_cost,
        period_total_value_estimate(state, period_cost),
        notify,
    );
    publish_local_overflow(
        state,
        OverflowUsage {
            today_tokens: snapshot
                .today_reliable
                .then_some(snapshot.today_overflow_tokens),
            today_credits: snapshot
                .today_reliable
                .then_some(snapshot.today_credits_spent),
            period_tokens: snapshot
                .current_period_reliable
                .then_some(snapshot.current_period_overflow_tokens),
            period_credits: snapshot
                .current_period_reliable
                .then_some(snapshot.current_period_credits_spent),
        },
        notify,
    );
    publish_local_token_usage(
        state,
        snapshot.today_reliable.then_some(snapshot.today_tokens),
        snapshot
            .current_period_reliable
            .then_some(snapshot.current_period_tokens),
        notify,
    );
    publish_local_cache_hit_percent_tenths(
        state,
        today_cache_hit_percent_tenths,
        period_cache_hit_percent_tenths,
        notify,
    );
}

fn publish_local_cost<F>(
    state: &Arc<Mutex<AppState>>,
    today: Option<f64>,
    period: Option<f64>,
    estimate: Option<f64>,
    notify: &Arc<F>,
) where
    F: Fn() + Send + Sync + 'static,
{
    let mut changed = false;
    if let Ok(mut current) = state.lock() {
        if current.today_cost != today {
            current.today_cost = today;
            changed = true;
        }
        if current.current_period_cost != period {
            current.current_period_cost = period;
            changed = true;
        }
        if current.period_total_value_estimate != estimate {
            current.period_total_value_estimate = estimate;
            changed = true;
        }
    }
    if changed {
        notify();
    }
}

/// credits 购买价：500 credits = $20，余额实付美元按此口径折算。
/// 实测扣费与 API 牌价并不相等（约为牌价的一半），这里显示的是真实支付。
pub(crate) const CREDITS_USD_RATE: f64 = 0.04;

fn credits_to_usd(credits: f64) -> f64 {
    credits * CREDITS_USD_RATE
}

/// 溢出（余额）口径的一组显示值。
///
/// credits 是账户级**原始**口径（日志里的余额观测差分），美元是 ×0.04 的换算
/// 值——在这里一次性换算，`AppState` 同时保留两者，展示层不必再换算回去。
/// token 数是本机触顶后事件的实测值，口径与 credits 不同。
#[derive(Debug, Clone, Copy, Default)]
struct OverflowUsage {
    today_tokens: Option<u64>,
    today_credits: Option<f64>,
    period_tokens: Option<u64>,
    period_credits: Option<f64>,
}

fn publish_local_overflow<F>(state: &Arc<Mutex<AppState>>, usage: OverflowUsage, notify: &Arc<F>)
where
    F: Fn() + Send + Sync + 'static,
{
    let today_cost = usage.today_credits.map(credits_to_usd);
    let period_cost = usage.period_credits.map(credits_to_usd);
    let mut changed = false;
    if let Ok(mut current) = state.lock() {
        changed |= assign(&mut current.today_overflow_tokens, usage.today_tokens);
        changed |= assign(&mut current.today_overflow_credits, usage.today_credits);
        changed |= assign(&mut current.today_overflow_cost, today_cost);
        changed |= assign(
            &mut current.current_period_overflow_tokens,
            usage.period_tokens,
        );
        changed |= assign(
            &mut current.current_period_overflow_credits,
            usage.period_credits,
        );
        changed |= assign(&mut current.current_period_overflow_cost, period_cost);
    }
    if changed {
        notify();
    }
}

/// 只在值真的变化时写回，并报告是否变化（避免无谓重绘）。
fn assign<T: PartialEq>(field: &mut T, value: T) -> bool {
    if *field == value {
        return false;
    }
    *field = value;
    true
}

/// 本期估值 = 本期套餐内已用美元 ÷ 周额度已用百分比 × 100（溢出扣费在
/// 额度之外单独计价，不计入满额价值）。百分比过小（新窗口/外部重置后）、
/// 本机尚无用量或快照过期时给不出有意义的估算，返回 None。
fn period_total_value_estimate(
    state: &Arc<Mutex<AppState>>,
    period_cost: Option<f64>,
) -> Option<f64> {
    let cost = period_cost?;
    let now = SystemTime::now();
    let current = state.lock().ok()?;
    if current.is_stale(now) {
        return None;
    }
    let (_, long_term) = current.snapshot.as_ref()?.active_windows(now);
    let used_percent = long_term?.used_percent;
    (used_percent >= 1.0 && cost > 0.0).then(|| cost * 100.0 / used_percent)
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

fn publish_local_cache_hit_percent_tenths<F>(
    state: &Arc<Mutex<AppState>>,
    today: Option<u16>,
    current_period: Option<u16>,
    notify: &Arc<F>,
) where
    F: Fn() + Send + Sync + 'static,
{
    let changed = state.lock().is_ok_and(|mut current| {
        let mut changed = false;
        changed |= assign(&mut current.today_cache_hit_percent_tenths, today);
        changed |= assign(
            &mut current.current_period_cache_hit_percent_tenths,
            current_period,
        );
        changed
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_install::{BackendSource, find_cli_executable};
    use crate::quota::QuotaWindow;

    #[test]
    #[ignore = "requires installed Desktop, cached ChatGPT login, and no discoverable standalone CLI"]
    fn desktop_backend_refreshes_live_quota() {
        assert!(find_cli_executable().is_none());
        let backend = find_app_server_backend().unwrap();
        assert_eq!(backend.source, BackendSource::Desktop);
        assert!(backend.path.is_absolute());
        let state = Arc::new(Mutex::new(AppState::default()));
        let notify = Arc::new(|| {});
        let cancelled = Arc::new(AtomicBool::new(false));

        pull_rpc_snapshot(&state, &notify, &cancelled, true).unwrap();

        let current = state.lock().unwrap();
        assert!(current.snapshot.is_some() && current.plan_type.is_some());
        assert_eq!(current.quota_pull, QuotaPull::Idle);
    }

    #[test]
    #[ignore = "requires installed standalone Codex CLI and a cached ChatGPT login"]
    fn cli_backend_reads_live_quota() {
        let executable = find_cli_executable().unwrap();
        let state = Arc::new(Mutex::new(AppState::default()));
        let notify = Arc::new(|| {});
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut session = AppServerSession::spawn(&executable, cancelled).unwrap();
        let mut next_id = initialize_session(&mut session).unwrap();

        let quota_result =
            read_rate_limits_and_publish(&mut session, &mut next_id, &state, &notify);
        let account_result = read_account_and_publish(&mut session, &mut next_id, &state, &notify);
        session.shutdown();

        quota_result.unwrap();
        account_result.unwrap();
        let current = state.lock().unwrap();
        assert!(current.snapshot.is_some() && current.plan_type.is_some());
    }

    #[test]
    #[ignore = "requires installed Desktop and CODEX_HOME with file auth configured but no login"]
    fn desktop_backend_respects_empty_codex_home() {
        assert!(std::env::var_os("CODEX_HOME").is_some());
        let backend = find_app_server_backend().unwrap();
        assert_eq!(backend.source, BackendSource::Desktop);
        let state = Arc::new(Mutex::new(AppState::default()));
        let notify = Arc::new(|| {});
        let cancelled = Arc::new(AtomicBool::new(false));

        let result = pull_rpc_snapshot(&state, &notify, &cancelled, true);

        assert!(matches!(result, Err(AppError::Authentication(_))));
        let current = state.lock().unwrap();
        assert!(current.snapshot.is_none() && current.plan_type.is_none());
        assert_eq!(current.quota_pull, QuotaPull::Idle);
    }

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
    fn fresh_snapshot_preserves_period_boundary() {
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(quota_snapshot(SystemTime::now())),
            ..AppState::default()
        }));

        // 单一 168h 窗口即长窗口：边界 = resets_at − 窗口时长。
        let boundary = current_period_boundary(&state);
        assert!(boundary.is_some());
    }

    #[test]
    fn stale_snapshot_hides_period_boundary() {
        // 外部重置等场景下，过期快照描述的窗口可能已被替换；从它推导
        // 边界会把上一期的用量当成本期闪现，必须等按需 pull 纠正。
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(quota_snapshot(SystemTime::UNIX_EPOCH)),
            ..AppState::default()
        }));

        assert_eq!(current_period_boundary(&state), None);
    }

    #[test]
    fn regular_pull_tracks_interval() {
        for minutes in [1_u64, 2, 5, 10, 30] {
            let state = Arc::new(Mutex::new(AppState {
                quota_refresh_interval: Duration::from_mins(minutes),
                ..AppState::default()
            }));

            let now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
            state.lock().unwrap().snapshot = Some(quota_snapshot(now));
            assert_eq!(local_pull_delay(&state, now), Duration::from_mins(minutes));
        }
    }

    #[test]
    fn regular_pull_uses_remaining_freshness() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(quota_snapshot(now - Duration::from_mins(2))),
            quota_refresh_interval: Duration::from_mins(5),
            ..AppState::default()
        }));

        assert_eq!(local_pull_delay(&state, now), Duration::from_mins(3));
    }

    #[test]
    fn manual_pull_bypasses_freshness() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(quota_snapshot(now - Duration::from_mins(1))),
            quota_refresh_interval: Duration::from_mins(5),
            ..AppState::default()
        }));

        // 自动路径还要等 4 分钟；手动刷新必须立刻拉。
        assert_eq!(local_pull_delay(&state, now), Duration::from_mins(4));
        assert_eq!(pull_delay(true, &state, now), Duration::ZERO);
        assert_eq!(pull_delay(false, &state, now), Duration::from_mins(4));
    }

    #[test]
    fn expired_window_triggers_pull() {
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
    fn reset_preempts_regular_refresh() {
        let wall_now = SystemTime::UNIX_EPOCH + Duration::from_hours(16);
        let now = Instant::now();
        let mut snapshot = quota_snapshot(wall_now);
        snapshot.primary.resets_at = wall_now + Duration::from_mins(1);
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(snapshot),
            quota_refresh_interval: Duration::from_mins(2),
            ..AppState::default()
        }));
        let schedule = PullSchedule::new(now);

        assert_eq!(
            schedule.delay(false, &state, wall_now, now),
            Duration::from_mins(1)
        );
        let elapsed = Duration::from_mins(1);
        assert_eq!(
            schedule.delay(false, &state, wall_now + elapsed, now + elapsed),
            Duration::ZERO,
            "窗口到期时快照仍未满两分钟，也必须立即拉取"
        );
    }

    #[test]
    fn earliest_window_reset_triggers_pull() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let mut snapshot = quota_snapshot(now);
        snapshot.primary.resets_at = now + Duration::from_mins(1);
        snapshot.secondary = Some(QuotaWindow {
            used_percent: 50.0,
            window_duration: Duration::from_hours(5),
            resets_at: now + Duration::from_secs(30),
        });
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(snapshot),
            quota_refresh_interval: Duration::from_mins(2),
            ..AppState::default()
        }));

        assert_eq!(local_pull_delay(&state, now), Duration::from_secs(30));
    }

    #[test]
    fn local_snapshot_advances_pull_schedule() {
        let wall_now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let now = Instant::now();
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(quota_snapshot(wall_now)),
            quota_refresh_interval: Duration::from_mins(2),
            ..AppState::default()
        }));
        let schedule = PullSchedule::new(now);
        assert_eq!(
            schedule.delay(false, &state, wall_now, now),
            Duration::from_mins(2)
        );

        let elapsed = Duration::from_secs(10);
        let mut new_snapshot = quota_snapshot(wall_now + elapsed);
        new_snapshot.primary.resets_at = wall_now + Duration::from_secs(20);
        publish_local_quota(&state, Some(new_snapshot), None, &Arc::new(|| {}));

        assert_eq!(
            schedule.delay(false, &state, wall_now + elapsed, now + elapsed),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn clock_jump_triggers_expired_pull() {
        let wall_now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let now = Instant::now();
        let mut snapshot = quota_snapshot(wall_now);
        snapshot.primary.resets_at = wall_now + Duration::from_mins(1);
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(snapshot),
            quota_refresh_interval: Duration::from_mins(2),
            ..AppState::default()
        }));
        let schedule = PullSchedule::new(now);

        assert_eq!(
            schedule.delay(
                false,
                &state,
                wall_now + Duration::from_mins(1),
                now + Duration::from_secs(5)
            ),
            Duration::ZERO
        );
    }

    #[test]
    fn expired_window_retry_backoff() {
        // 服务端仍返回已到期窗口时退避重试；取得新窗口后恢复常规刷新。
        let wall_now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let now = Instant::now();
        let mut snapshot = quota_snapshot(wall_now);
        snapshot.primary.resets_at = wall_now;
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(snapshot),
            quota_refresh_interval: Duration::from_mins(2),
            ..AppState::default()
        }));
        let mut schedule = PullSchedule::new(now);
        let mut elapsed = Duration::ZERO;

        for seconds in [1, 2, 5, 120, 120] {
            // 模拟 RPC 成功返回一份收到时间很新、但窗口仍未更新的快照。
            state.lock().unwrap().snapshot.as_mut().unwrap().received_at = wall_now + elapsed;
            schedule.after_attempt(
                local_pull_delay(&state, wall_now + elapsed).is_zero(),
                Duration::from_mins(2),
                now + elapsed,
            );
            let delay = Duration::from_secs(seconds);
            assert_eq!(
                schedule.delay(false, &state, wall_now + elapsed, now + elapsed),
                delay
            );
            elapsed += delay;
        }

        state.lock().unwrap().snapshot = Some(quota_snapshot(wall_now + elapsed));
        schedule.after_attempt(false, Duration::from_mins(2), now + elapsed);
        assert_eq!(schedule.retry_count, 0);
        assert_eq!(
            schedule.delay(false, &state, wall_now + elapsed, now + elapsed),
            Duration::from_mins(2),
            "取得新窗口后恢复两分钟常规刷新"
        );

        // 下次窗口到期会重新获得三次密集重试机会。
        schedule.after_attempt(true, Duration::from_mins(2), now + elapsed);
        assert_eq!(
            schedule.retry_not_before,
            now + elapsed + Duration::from_secs(1)
        );
    }

    #[test]
    fn rapid_retries_are_limited() {
        let now = Instant::now();
        for minutes in [1, 2, 5, 10, 30] {
            let interval = Duration::from_mins(minutes);
            let mut schedule = PullSchedule::new(now);
            let mut attempted_at = now;
            // 首次失败后额外重试三次，之后持续按配置间隔尝试。
            for expected in [
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(5),
                interval,
                interval,
                interval,
            ] {
                schedule.after_attempt(true, interval, attempted_at);
                assert_eq!(
                    schedule.retry_not_before,
                    attempted_at + expected,
                    "{minutes}m: expected {expected:?}"
                );
                attempted_at += expected;
            }
        }
    }

    #[test]
    fn interval_change_preserves_retry_count() {
        let now = Instant::now();
        let mut schedule = PullSchedule::new(now);
        schedule.after_attempt(true, Duration::from_mins(5), now);

        // 调整设置不重置密集重试次数，即使两次请求之间已经过去很久。
        let attempted_at = now + Duration::from_mins(3);
        schedule.request_now(attempted_at);
        schedule.after_attempt(true, Duration::from_mins(2), attempted_at);
        assert_eq!(
            schedule.retry_not_before,
            attempted_at + Duration::from_secs(2)
        );
        schedule.after_attempt(true, Duration::from_mins(5), attempted_at);
        assert_eq!(
            schedule.retry_not_before,
            attempted_at + Duration::from_secs(5)
        );
        // 密集重试用完后，低频阶段采用当前设置的间隔。
        for minutes in [2, 5] {
            let interval = Duration::from_mins(minutes);
            schedule.request_now(attempted_at);
            schedule.after_attempt(true, interval, attempted_at);
            assert_eq!(schedule.retry_not_before, attempted_at + interval);
        }
    }

    #[test]
    fn expired_window_respects_request_backoff() {
        let wall_now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let now = Instant::now();
        let mut snapshot = quota_snapshot(wall_now);
        snapshot.primary.resets_at = wall_now;
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(snapshot),
            ..AppState::default()
        }));
        let mut schedule = PullSchedule::new(now);
        for seconds in [1, 2, 5, 300] {
            schedule.after_attempt(true, Duration::from_mins(5), now);
            assert_eq!(
                schedule.delay(
                    false,
                    &state,
                    wall_now + Duration::from_secs(1),
                    now + Duration::from_secs(1)
                ),
                Duration::from_secs(seconds - 1)
            );
        }
    }

    #[test]
    fn a_manual_refresh_interrupts_retry_waiting() {
        let wall_now = SystemTime::UNIX_EPOCH + Duration::from_hours(1);
        let now = Instant::now();
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(quota_snapshot(wall_now)),
            ..AppState::default()
        }));
        let mut schedule = PullSchedule::new(now);
        let interval = Duration::from_mins(5);
        schedule.after_attempt(true, interval, now);
        schedule.request_now(now);

        assert_eq!(schedule.delay(true, &state, wall_now, now), Duration::ZERO);

        for _ in 0..2 {
            schedule.after_attempt(true, interval, now);
        }
        // 低频阶段的等待也能手动打断；失败后继续低频，避免重新密集重试。
        let later = now + interval;
        schedule.after_attempt(true, interval, later);
        assert_eq!(schedule.retry_not_before, later + interval);
        schedule.request_now(later);
        assert_eq!(
            schedule.delay(true, &state, wall_now + interval, later),
            Duration::ZERO
        );
        schedule.after_attempt(true, interval, later);
        assert_eq!(schedule.retry_not_before, later + interval);
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
    fn missing_local_quota_preserves_state() {
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
    fn local_quota_preserves_existing_plan() {
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
    fn local_quota_restores_online_status() {
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
    fn credits_convert_at_the_purchase_rate() {
        // 500 credits = $20 → 1 credit = $0.04；实测扣费按此口径折算实付。
        assert!((credits_to_usd(73.22) - 2.928_8).abs() < 1e-9);
        assert!(credits_to_usd(0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn overflow_credits_convert_to_dollars() {
        let state = Arc::new(Mutex::new(AppState::default()));
        let notify = Arc::new(|| {});

        publish_local_overflow(
            &state,
            OverflowUsage {
                today_tokens: Some(200),
                today_credits: Some(20.0),
                period_tokens: Some(1_500),
                period_credits: Some(310.0),
            },
            &notify,
        );

        let current = state.lock().unwrap();
        assert_eq!(current.today_overflow_tokens, Some(200));
        assert_eq!(current.today_overflow_credits, Some(20.0));
        assert_eq!(current.today_overflow_cost, Some(0.8));
        assert_eq!(current.current_period_overflow_tokens, Some(1_500));
        assert_eq!(current.current_period_overflow_credits, Some(310.0));
        assert_eq!(current.current_period_overflow_cost, Some(12.4));
    }

    #[test]
    fn overflow_values_stay_in_sync() {
        // 单位换错过两次（阈值从美元换成 credits 时调用方没跟着改），所以把
        // 数据契约钉住：发布后两个口径必须始终是 1 : 0.04。
        let state = Arc::new(Mutex::new(AppState::default()));
        let notify = Arc::new(|| {});

        publish_local_overflow(
            &state,
            OverflowUsage {
                today_tokens: Some(1),
                today_credits: Some(310.25),
                period_tokens: Some(2),
                period_credits: Some(73.22),
            },
            &notify,
        );

        let current = state.lock().unwrap();
        let today = current.today_overflow_credits.unwrap() * CREDITS_USD_RATE;
        let period = current.current_period_overflow_credits.unwrap() * CREDITS_USD_RATE;
        assert!((current.today_overflow_cost.unwrap() - today).abs() < 1e-9);
        assert!((current.current_period_overflow_cost.unwrap() - period).abs() < 1e-9);
        assert!((period - 2.928_8).abs() < 1e-9);
    }

    #[test]
    fn missing_overflow_marks_windows_unreliable() {
        let state = Arc::new(Mutex::new(AppState {
            today_overflow_tokens: Some(200),
            today_overflow_credits: Some(20.0),
            ..AppState::default()
        }));
        let notify = Arc::new(|| {});

        publish_local_overflow(&state, OverflowUsage::default(), &notify);

        let current = state.lock().unwrap();
        assert_eq!(current.today_overflow_tokens, None);
        assert_eq!(current.today_overflow_credits, None);
        assert_eq!(current.today_overflow_cost, None);
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
    fn period_estimate_scales_usage() {
        let mut snapshot = quota_snapshot(SystemTime::now());
        snapshot.primary.used_percent = 10.0;
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(snapshot),
            ..AppState::default()
        }));

        let estimate = period_total_value_estimate(&state, Some(12.4));

        assert!(estimate.is_some_and(|value| (value - 124.0).abs() < 1e-9));
    }

    #[test]
    fn period_estimate_requires_minimum_usage() {
        let mut snapshot = quota_snapshot(SystemTime::now());
        snapshot.primary.used_percent = 0.5;
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(snapshot),
            ..AppState::default()
        }));

        assert_eq!(period_total_value_estimate(&state, Some(12.4)), None);
    }

    #[test]
    fn period_estimate_requires_local_cost() {
        // 本机无用量（cost=0）但服务端已有消耗时，0÷x% 的估算没有意义。
        let mut snapshot = quota_snapshot(SystemTime::now());
        snapshot.primary.used_percent = 10.0;
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(snapshot),
            ..AppState::default()
        }));

        assert_eq!(period_total_value_estimate(&state, Some(0.0)), None);
    }

    #[test]
    fn period_value_estimate_hides_stale_snapshot() {
        let state = Arc::new(Mutex::new(AppState {
            snapshot: Some(quota_snapshot(SystemTime::UNIX_EPOCH)),
            ..AppState::default()
        }));

        assert_eq!(
            period_total_value_estimate(&state, Some(12.4)),
            None,
            "过期快照的百分比不可信，估算必须为 None"
        );
    }

    #[test]
    fn price_failure_backoff_caps_hourly() {
        let delays: Vec<_> = (1..=8)
            .map(|failures| price_refresh_failure_delay(failures, Duration::from_hours(1)))
            .collect();
        assert_eq!(
            delays,
            [
                Duration::from_secs(30),
                Duration::from_mins(1),
                Duration::from_mins(2),
                Duration::from_mins(5),
                Duration::from_mins(15),
                Duration::from_mins(30),
                Duration::from_hours(1),
                Duration::from_hours(1),
            ]
        );
    }

    #[test]
    fn prolonged_price_failure_slows_retry() {
        assert_eq!(
            price_refresh_failure_delay(
                99,
                PRICE_FAILURE_SLOW_AFTER.saturating_sub(Duration::from_secs(1)),
            ),
            Duration::from_hours(1)
        );
        assert_eq!(
            price_refresh_failure_delay(99, PRICE_FAILURE_SLOW_AFTER),
            PRICE_FAILURE_SLOW_INTERVAL
        );
    }

    #[test]
    fn manual_price_refresh_preserves_backoff() {
        let mut prices = PricePipeline::new(PriceTable::default());
        let first_failure = Instant::now();
        prices.failures = 9;
        prices.first_failure = Some(first_failure);
        prices.next_refresh = Instant::now() + PRICE_FAILURE_SLOW_INTERVAL;

        prices.schedule_now();

        assert!(prices.next_refresh <= Instant::now());
        assert_eq!(prices.failures, 9);
        assert_eq!(prices.first_failure, Some(first_failure));
    }

    #[test]
    fn missing_price_respects_failure_backoff() {
        let mut prices = PricePipeline::new(PriceTable::default());
        let now = Instant::now();
        let scheduled = now + PRICE_FAILURE_SLOW_INTERVAL;
        prices.failures = 9;
        prices.first_failure = Some(now);
        prices.next_refresh = scheduled;
        prices.gaps.wants_pull = true;

        prices.refresh_if_due(&mut None);

        assert_eq!(prices.next_refresh, scheduled);
        assert!(prices.gaps.wants_pull);
    }

    #[test]
    fn fresh_prices_delay_initial_refresh() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let fetched_at = (now - Duration::from_hours(1))
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // 缓存 1 小时前拉取过：等满 24 小时间隔的剩余部分，而不是再拉一次。
        assert_eq!(
            price_refresh_delay(Some(fetched_at), now),
            PRICE_REFRESH_INTERVAL
                .checked_sub(Duration::from_hours(1))
                .unwrap()
        );
    }

    #[test]
    fn unavailable_prices_use_start_delay() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let stale_fetched_at = (now - Duration::from_hours(25))
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        assert_eq!(price_refresh_delay(None, now), PRICE_REFRESH_START_DELAY);
        assert_eq!(
            price_refresh_delay(Some(stale_fetched_at), now),
            PRICE_REFRESH_START_DELAY
        );
    }

    /// 缺价日志按模型名去重：一轮本地扫描每几秒就可能重跑一次，逐轮上报会把
    /// 日志刷满，也就没人看得见"到底缺哪个模型"。
    #[test]
    fn price_gap_logging_deduplicated() {
        let mut gaps = PriceGaps::default();
        let now = Instant::now();
        let prices = PriceTable::default();

        assert_eq!(
            gaps.observe(&["gpt-new"], &prices, now),
            vec!["价格表缺模型：gpt-new"]
        );
        assert!(gaps.wants_pull, "有缺口就该安排一次提前补拉");

        assert!(
            gaps.observe(&["gpt-new"], &prices, now + Duration::from_mins(1))
                .is_empty()
        );
        assert!(gaps.wants_pull, "缺口还在，补拉意图应保持");
    }

    /// "少算了"必须有闭合的另一半：价格表补齐时回写一条，并且缺口关上之后
    /// 不再安排补拉——否则模型用完不再出现，程序会以为缺口永远在。
    #[test]
    fn resolved_price_gap_closes_log() {
        let mut gaps = PriceGaps::default();
        let now = Instant::now();

        gaps.observe(&["gpt-new"], &PriceTable::default(), now);
        let prices = PriceTable::from_models_dev(
            r#"{"openai":{"models":{"gpt-new":{"cost":{"input":4,"output":20,"cache_read":0.4}}}}}"#,
        )
        .expect("应解析出价格表");

        // 本窗口已经不再出现这个模型，闭环仍要报出来。
        assert_eq!(
            gaps.observe(&[], &prices, now + Duration::from_secs(90)),
            vec!["价格表已补齐：gpt-new"]
        );
        assert!(!gaps.wants_pull, "缺口关闭后不该再补拉");
        // 同一个模型再次缺价时重新开一轮（不是一次报过就永远沉默）。
        assert_eq!(
            gaps.observe(
                &["gpt-new"],
                &PriceTable::default(),
                now + Duration::from_mins(2)
            ),
            vec!["价格表缺模型：gpt-new"]
        );
    }

    /// 永远不进价格表的模型名（本地变体后缀之类）不能让它每小时发请求到永远：
    /// 补拉窗口耗尽后停手，只留一条日志说明改由日调度兜底。
    #[test]
    fn price_gap_retry_window_expires() {
        let mut gaps = PriceGaps::default();
        let now = Instant::now();
        let prices = PriceTable::default();

        gaps.observe(&["gpt-never"], &prices, now);
        let just_inside_window = PRICE_GAP_RETRY_WINDOW.saturating_sub(Duration::from_secs(1));
        let inside_window = gaps.observe(&["gpt-never"], &prices, now + just_inside_window);
        assert!(inside_window.is_empty());
        assert!(gaps.wants_pull, "窗口内继续补拉");

        assert_eq!(
            gaps.observe(&["gpt-never"], &prices, now + PRICE_GAP_RETRY_WINDOW),
            vec!["价格表仍缺模型：gpt-never（暂停补拉，等每日刷新）"]
        );
        assert!(!gaps.wants_pull, "停手之后不再提前补拉");
        // 停手只报一次。
        assert!(
            gaps.observe(&["gpt-never"], &prices, now + PRICE_GAP_RETRY_WINDOW * 2)
                .is_empty()
        );
    }

    #[test]
    fn elapsed_duration_is_formatted_in_milliseconds() {
        assert_eq!(format_elapsed(Duration::from_nanos(1_234_567)), "1.235 ms");
    }

    #[test]
    fn refresh_mode_labels() {
        for (mode, expected) in [
            (session_usage::RefreshMode::FullScan, "扫描"),
            (session_usage::RefreshMode::WatcherIncremental, "监听"),
        ] {
            let diagnostics = session_usage::RefreshDiagnostics {
                mode,
                ..session_usage::RefreshDiagnostics::default()
            };
            assert_eq!(refresh_mode_description(&diagnostics), expected, "{mode:?}");
        }
    }

    #[test]
    fn refresh_logging_policy() {
        use session_usage::{RefreshDiagnostics, RefreshMode};

        let incremental = RefreshDiagnostics {
            mode: RefreshMode::WatcherIncremental,
            ..RefreshDiagnostics::default()
        };
        for (case, diagnostics, expected) in [
            (
                "no_changes",
                RefreshDiagnostics {
                    files_scanned: 1,
                    aggregation_skipped: true,
                    cache_write_skipped: true,
                    ..incremental
                },
                false,
            ),
            (
                "no_new_tokens",
                RefreshDiagnostics {
                    files_scanned: 1,
                    files_read: 1,
                    ..incremental
                },
                false,
            ),
            (
                "new_tokens",
                RefreshDiagnostics {
                    token_events_added: 1,
                    ..incremental
                },
                true,
            ),
            (
                "discovery_error",
                RefreshDiagnostics {
                    discovery_errors: 1,
                    ..incremental
                },
                true,
            ),
            (
                "cache_write_error",
                RefreshDiagnostics {
                    cache_write_failed: true,
                    ..incremental
                },
                true,
            ),
            (
                "deferred_file",
                RefreshDiagnostics {
                    deferred_files: 1,
                    ..incremental
                },
                true,
            ),
            (
                "full_scan",
                RefreshDiagnostics {
                    aggregation_skipped: true,
                    cache_write_skipped: true,
                    ..RefreshDiagnostics::default()
                },
                true,
            ),
        ] {
            assert_eq!(
                should_log_local_usage_refresh(&diagnostics),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn watcher_log_reports_completed_work() {
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
    fn scan_log_omits_empty_phases() {
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
    fn refresh_log_reports_failures() {
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
    fn local_usage_published_together() {
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
    fn unreliable_period_preserves_today() {
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
