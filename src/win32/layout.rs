use std::mem::size_of;
use std::ptr;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MONITORINFOEXW, MonitorFromPoint,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};

use super::{COLLAPSED_DIP, PANEL_WIDTH_DIP};
use crate::config::{AnchorEdge, AppConfigV1};
use crate::error::AppError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ExpansionAlignment {
    #[default]
    Start,
    End,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PanelAnimation {
    pub(super) started_at: Instant,
    pub(super) duration: Duration,
    pub(super) anchor_rect: RECT,
    pub(super) work_area: RECT,
    pub(super) canvas_destination: POINT,
    pub(super) ball_center_screen: POINT,
    pub(super) expanding: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct AnimationSample {
    pub(super) expansion: f32,
    pub(super) ball_opacity: f32,
    pub(super) panel_opacity: f32,
    pub(super) finished: bool,
}

impl PanelAnimation {
    pub(super) fn sample(self, now: Instant) -> AnimationSample {
        let elapsed = now.saturating_duration_since(self.started_at);
        let timeline = (elapsed.as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0);
        let mut sample = animation_sample(self.expanding, timeline);
        sample.finished = elapsed >= self.duration;
        sample
    }
}

/// 悬浮球的一条补间：把某个 0..1 的值从旧值推到新值，按 `Instant` + 时长采样。
///
/// 弧与中心数字在额度变化时补间、球刚出现时从 0 扫入，指针态的悬停提亮与
/// 按下缩放也复用它——四处都只是"从 a 到 b 的缓出"，没有各自的时间轴。
#[derive(Clone, Copy, Debug)]
pub(super) struct Tween {
    started_at: Instant,
    duration: Duration,
    from: f64,
    to: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct TweenSample {
    pub(super) fraction: f64,
    pub(super) finished: bool,
}

impl Tween {
    /// 数值变化的补间时长：明显可见但不拖沓，通常远早于下一次刷新到来。
    pub(super) const DURATION: Duration = Duration::from_millis(250);
    /// 球出现时的扫入时长：略长一些，读起来像仪表自检。
    pub(super) const SWEEP_DURATION: Duration = Duration::from_millis(700);
    /// 中心数字在**数值变化**时的滚动时长。
    ///
    /// 不变式：`LABEL_TWEEN < DURATION` 且 `LABEL_SWEEP < SWEEP_DURATION`。
    /// 数字若比弧后落定，就会出现"环已静止、数字还在滚"的割裂观感，并且
    /// 让帧定时器多跑一段。两个时长分别对应两种弧动画，必须各自更短。
    pub(super) const LABEL_TWEEN: Duration = Duration::from_millis(200);
    /// 中心数字在**球出现扫入**时的滚动时长：比 `LABEL_TWEEN` 从容，
    /// 700ms 的扫入里数字先报完数。
    pub(super) const LABEL_SWEEP: Duration = Duration::from_millis(500);

    pub(super) fn new(from: f64, to: f64, now: Instant) -> Self {
        Self::with_duration(from, to, now, Self::DURATION)
    }

    /// 球刚出现：弧从 0 扫到当前比例。
    pub(super) fn sweep(to: f64, now: Instant) -> Self {
        Self::with_duration(0.0, to, now, Self::SWEEP_DURATION)
    }

    /// 中心数字随**数值变化**滚动：与弧同时起步，但更早落定。
    pub(super) fn label_tween(from: f64, to: f64, now: Instant) -> Self {
        Self::with_duration(from, to, now, Self::LABEL_TWEEN)
    }

    /// 中心数字随**球出现扫入**从 0 滚动。
    pub(super) fn label_sweep(to: f64, now: Instant) -> Self {
        Self::with_duration(0.0, to, now, Self::LABEL_SWEEP)
    }

    /// 指针态专用：显式给出时长，因为悬停、按下、回弹三段的节奏不同。
    pub(super) fn timed(from: f64, to: f64, now: Instant, duration: Duration) -> Self {
        Self::with_duration(from, to, now, duration)
    }

    fn with_duration(from: f64, to: f64, now: Instant, duration: Duration) -> Self {
        Self {
            started_at: now,
            duration,
            from,
            to,
        }
    }

    pub(super) fn sample(self, now: Instant) -> TweenSample {
        let elapsed = now.saturating_duration_since(self.started_at);
        let timeline = (elapsed.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0);
        let progress = f64::from(ease_out_cubic(timeline as f32));
        TweenSample {
            fraction: self.from + (self.to - self.from) * progress,
            finished: elapsed >= self.duration,
        }
    }
}

/// 指针态补间的时长：悬停提亮淡入淡出。
pub(super) const HOVER_TWEEN: Duration = Duration::from_millis(150);
/// 按下的缩放时长：按下要立刻有反应。
pub(super) const PRESS_TWEEN: Duration = Duration::from_millis(80);
/// 松开的回弹时长：比按下慢一点，手感才不脆。
pub(super) const RELEASE_TWEEN: Duration = Duration::from_millis(120);

/// 拖拽松手后的边缘吸附：把窗口从松手位置滑到落点。
///
/// 与面板动画同样按 `Instant` + 时长采样，便于单测。
///
/// 曲线不是单纯的缓出，而是三次 Hermite：**起点速度取松手那一刻手的速度**，
/// 终点速度为 0。单纯缓出的话第一帧速度是平均速度的 2–3 倍——手还在 1px/ms
/// 的时候球已经 3px/ms 冲出去了，看起来就是"接手时先顿一下再弹射"。Hermite
/// 让球从手的速度接着往下减，位置和速度在交接处都连续。
#[derive(Clone, Copy, Debug)]
pub(super) struct SnapAnimation {
    started_at: Instant,
    duration: Duration,
    from: POINT,
    to: POINT,
    /// 松手时的光标速度（px/ms），已按"只保留朝落点的分量、且不越过落点"夹住。
    velocity: (f32, f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SnapSample {
    pub(super) destination: POINT,
    pub(super) finished: bool,
}

impl SnapAnimation {
    /// 滑动的时长随距离变化：固定 140ms 在两种情况下都不对——只差几个像素时
    /// 拖得让人等，横跨半个屏幕时又是"啪"一下跳过去。按距离给时长，短距离依旧
    /// 干脆，长距离有足够帧数把"滑过去"演出来。
    const MIN_DURATION: Duration = Duration::from_millis(100);
    const MAX_DURATION: Duration = Duration::from_millis(350);
    /// 每像素追加的时长，单位 ms/px。
    const MS_PER_PIXEL: f32 = 0.30;

    pub(super) fn new(from: POINT, to: POINT, velocity: (f32, f32), now: Instant) -> Self {
        let duration = duration_for(from, to);
        Self {
            started_at: now,
            duration,
            from,
            to,
            velocity: clamp_to_target(from, to, duration, velocity),
        }
    }

    /// 起点与落点相同（松手时本来就在边上）就没有可滑的距离，直接算完成——
    /// 否则会白跑一帧。
    pub(super) fn is_noop(self) -> bool {
        self.from == self.to
    }

    /// 最终落点。中途被打断（用户又按下、DPI 变化）时用它直接落位。
    pub(super) fn destination(self) -> POINT {
        self.to
    }

    pub(super) fn sample(self, now: Instant) -> SnapSample {
        let elapsed = now.saturating_duration_since(self.started_at);
        let span = self.duration.as_millis() as f32;
        let timeline = ((elapsed.as_secs_f32() * 1000.0) / span).clamp(0.0, 1.0);
        SnapSample {
            destination: POINT {
                x: hermite(
                    self.from.x as f32,
                    self.to.x as f32,
                    self.velocity.0,
                    span,
                    timeline,
                )
                .round() as i32,
                y: hermite(
                    self.from.y as f32,
                    self.to.y as f32,
                    self.velocity.1,
                    span,
                    timeline,
                )
                .round() as i32,
            },
            finished: elapsed >= self.duration,
        }
    }
}

/// 三次 Hermite：起点 `p0`、终点 `p1`、起点速度 `v0`（px/ms）、终点速度 0，
/// 跨度 `span`（ms）。
///
/// `h10` 在 t=0 处的导数是 1，所以起点速度正好是 `v0`——这就是"速度连续"的来源。
/// 终点速度取 0 对应缓出（滑到边缘停住），于是 `h11` 项恒为 0，不用算。
fn hermite(p0: f32, p1: f32, v0: f32, span: f32, t: f32) -> f32 {
    let squared = t * t;
    let cubed = squared * t;
    let h00 = 2.0 * cubed - 3.0 * squared + 1.0;
    let h10 = cubed - 2.0 * squared + t;
    let h01 = -2.0 * cubed + 3.0 * squared;
    h00 * p0 + h01 * p1 + h10 * span * v0
}

/// 只保留指向落点的那部分速度，并夹在"不会越过落点"的范围内。
///
/// Hermite 从 `p0` 到 `p1`、终点速度为 0 时，起点速度一旦超过 `3·(p1−p0)/T`
/// 就会冲过落点再退回来——对吸附来说那意味着球滑出屏幕边缘。背向落点的速度
/// 直接丢掉：那是"松手时手正好在回甩"，让它先倒退再前进只会看起来在犹豫。
fn clamp_to_target(from: POINT, to: POINT, duration: Duration, velocity: (f32, f32)) -> (f32, f32) {
    let span = duration.as_millis() as f32;
    let limit = |delta: f32, speed: f32| {
        if delta.abs() < f32::EPSILON {
            return 0.0;
        }
        let cap = 3.0 * delta / span;
        if delta > 0.0 {
            speed.clamp(0.0, cap)
        } else {
            speed.clamp(cap, 0.0)
        }
    };
    (
        limit((to.x - from.x) as f32, velocity.0),
        limit((to.y - from.y) as f32, velocity.1),
    )
}

/// 吸附滑动的时长：`MIN_DURATION` 起，每像素加 `MS_PER_PIXEL`，封顶 `MAX_DURATION`。
///
/// 900px 的横跨（屏幕中央拖到边）约 350ms / 22 帧，40px 的微调约 112ms / 7 帧。
fn duration_for(from: POINT, to: POINT) -> Duration {
    let dx = f64::from(to.x - from.x);
    let dy = f64::from(to.y - from.y);
    let distance = (dx * dx + dy * dy).sqrt() as f32;
    // 毫秒取整再构造：`from_secs_f32` 会把 0.35 变成 349999994ns，和 `MAX_DURATION`
    // 不相等，封顶也就比较不出来。
    let millis =
        SnapAnimation::MIN_DURATION.as_millis() as f32 + distance * SnapAnimation::MS_PER_PIXEL;
    let capped = millis
        .round()
        .min(SnapAnimation::MAX_DURATION.as_millis() as f32);
    Duration::from_millis(capped as u64)
}

/// 低额度脉冲：还有余额的弧的不透明度在 `PULSE_MIN_OPACITY..=1.0` 之间正弦
/// 起伏，周期 `PULSE_PERIOD`（约 0.7Hz，远低于 WCAG 的 3Hz 闪烁阈值）。
///
/// 只作用于**仍有余额**的低额度；已耗尽与不可知都不画弧，也就不脉冲。
pub(super) const PULSE_PERIOD: Duration = Duration::from_millis(1400);
pub(super) const PULSE_MIN_OPACITY: f32 = 0.35;
/// 不确定进度弧转一圈的周期，以及它扫过的比例。
pub(super) const SPIN_PERIOD: Duration = Duration::from_millis(1600);
pub(super) const SPIN_SWEEP: f64 = 0.28;
/// 补间期间的帧间隔：与面板动画同帧率。
///
/// **必须小于系统时钟节拍**。`SetTimer` 的到期时间被量化到节拍上（默认
/// 15.625ms，可用 `GetSystemTimeAdjustment` 读到），而且是从**触发时刻**起算
/// 下一次到期：请求 16ms 会因为 15.6ms 那一拍差一点点而顺延到第二拍，实际
/// 变成 31.2ms / 32fps——比请求值慢一倍，且这不是抖动而是稳定地慢。请求
/// 10ms 则每一拍都到期：在 64Hz 节拍上得到 15.6ms（64fps），在更细的节拍上
/// 只会更快。所有动效都有界（最长 700ms 的扫入），多出来的帧很便宜。
pub(super) const RING_TWEEN_FRAME_MILLIS: u32 = 10;
/// 仅脉冲时的帧间隔：缓慢的明暗变化不需要 60fps（109ms 一拍也够）。
pub(super) const RING_PULSE_FRAME_MILLIS: u32 = 100;
/// 仅旋转时的帧间隔。
///
/// 匀速旋转对帧间隔最敏感：相位按 `Instant` 算，帧间隔一不均匀就变成角速度
/// 忽快忽慢。同样取 10ms 让它每拍都触发（详见 [`RING_TWEEN_FRAME_MILLIS`]）。
pub(super) const RING_SPIN_FRAME_MILLIS: u32 = 10;

/// 把上面那条约束钉成编译期检查：改大它的人会在 `cargo check` 时被拦下。
///
/// 上界取 15ms 而不是"随便一个小于 16 的值"：节拍是 15.625ms，只有 ≤15 才能
/// 保证每一拍都到期；16 会稳定地退化成两拍。
///
/// `expect` 而不是 `assert!`：常量断言会被 `clippy::assertions_on_constants` 判成
/// "恒定的断言"（`clippy::all` 在本仓库是 deny），而这个检查正是要拦"恒定的值"。
#[expect(
    clippy::manual_assert,
    reason = "常量断言会被 assertions_on_constants 拒绝，这里的检查对象就是常量本身"
)]
const _: () = if RING_TWEEN_FRAME_MILLIS > 15 || RING_SPIN_FRAME_MILLIS > 15 {
    panic!("补间帧间隔必须 ≤15ms（系统节拍 15.625ms），否则每两拍才触发一次，帧率直接减半");
};

pub(super) fn pulse_opacity(elapsed: Duration) -> f32 {
    let period = PULSE_PERIOD.as_secs_f32();
    let phase = elapsed.as_secs_f32() % period;
    let wave = 0.5 + 0.5 * (std::f32::consts::TAU * phase / period).cos();
    PULSE_MIN_OPACITY + (1.0 - PULSE_MIN_OPACITY) * wave
}

/// 当前相位，并顺带把 `epoch` 前进整数个周期。
///
/// 直接算 `now - epoch` 的话，长时间运行后差值会到 10^6 秒量级，转成 f32
/// 只剩约 0.1 秒分辨率，周期只有一两秒的动效就会变粗糙。锚定到"当前相位"
/// 后精度恒定，且 epoch 只前进整数个周期，相位本身不变。
pub(super) fn phase_at(epoch: &mut Instant, now: Instant, period: Duration) -> Duration {
    let elapsed = now.saturating_duration_since(*epoch);
    // `Duration` 没实现取模，按纳秒取余；余数必然小于一个周期。
    let phase = Duration::from_nanos((elapsed.as_nanos() % period.as_nanos()) as u64);
    // phase ≤ elapsed = now − epoch，减法不会下溢；退路只是保留原锚点。
    *epoch = now.checked_sub(phase).unwrap_or(*epoch);
    phase
}

pub(super) struct MonitorDetails {
    pub(super) info: MONITORINFOEXW,
}

pub(super) fn monitor_info(monitor: HMONITOR) -> Result<MonitorDetails, AppError> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: u32::try_from(size_of::<MONITORINFOEXW>())
                .map_err(|_| AppError::Windows("显示器结构大小溢出".to_owned()))?,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: MONITORINFOEXW begins with MONITORINFO and cbSize identifies the extended structure.
    let ok = unsafe { GetMonitorInfoW(monitor, ptr::from_mut(&mut info).cast::<MONITORINFO>()) };
    if !ok.as_bool() {
        return Err(AppError::Windows("无法读取显示器工作区".to_owned()));
    }
    Ok(MonitorDetails { info })
}

pub(super) fn primary_monitor() -> HMONITOR {
    // SAFETY: the default flag guarantees a monitor handle even if the point lies outside a display.
    unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }
}

pub(super) fn monitor_for_device(device: &str) -> Option<HMONITOR> {
    if device.is_empty() {
        return None;
    }
    struct Search<'a> {
        target: &'a str,
        found: Option<HMONITOR>,
    }
    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        // SAFETY: data points to Search for the synchronous duration of EnumDisplayMonitors.
        let search = unsafe { &mut *(data.0 as *mut Search<'_>) };
        if monitor_info(monitor)
            .is_ok_and(|details| monitor_device(&details).eq_ignore_ascii_case(search.target))
        {
            search.found = Some(monitor);
            return false.into();
        }
        true.into()
    }
    let mut search = Search {
        target: device,
        found: None,
    };
    // SAFETY: callback is synchronous and data points to search until enumeration returns.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(callback),
            LPARAM(ptr::from_mut(&mut search) as isize),
        )
    };
    search.found
}

pub(super) fn monitor_device(details: &MonitorDetails) -> String {
    String::from_utf16_lossy(&details.info.szDevice)
        .trim_end_matches('\0')
        .to_owned()
}

pub(super) fn position_from_config(
    config: &AppConfigV1,
    monitor: &MonitorDetails,
    side: i32,
    dpi: u32,
) -> POINT {
    let work = monitor.info.monitorInfo.rcWork;
    let offset = dip_to_px(config.placement.offset_dip.max(0.0), dpi);
    match config.placement.edge {
        AnchorEdge::Left => POINT {
            x: work.left,
            y: (work.top + offset).clamp(work.top, work.bottom - side),
        },
        AnchorEdge::Right => POINT {
            x: work.right - side,
            y: (work.top + offset).clamp(work.top, work.bottom - side),
        },
        AnchorEdge::Top => POINT {
            x: (work.left + offset).clamp(work.left, work.right - side),
            y: work.top,
        },
        AnchorEdge::Bottom => POINT {
            x: (work.left + offset).clamp(work.left, work.right - side),
            y: work.bottom - side,
        },
    }
}

pub(super) fn expanded_destination(
    rect: RECT,
    edge: AnchorEdge,
    width: i32,
    height: i32,
    work: RECT,
) -> (POINT, ExpansionAlignment) {
    match edge {
        AnchorEdge::Left | AnchorEdge::Right => {
            let (y, alignment) =
                expansion_axis(rect.top, rect.bottom, height, work.top, work.bottom);
            let x = if edge == AnchorEdge::Left {
                rect.left
            } else {
                rect.right.saturating_sub(width)
            };
            (
                POINT {
                    x: clamp_axis(x, width, work.left, work.right),
                    y,
                },
                alignment,
            )
        }
        AnchorEdge::Top | AnchorEdge::Bottom => {
            let (x, alignment) =
                expansion_axis(rect.left, rect.right, width, work.left, work.right);
            let y = if edge == AnchorEdge::Top {
                rect.top
            } else {
                rect.bottom.saturating_sub(height)
            };
            (
                POINT {
                    x,
                    y: clamp_axis(y, height, work.top, work.bottom),
                },
                alignment,
            )
        }
    }
}

pub(super) fn anchored_destination(
    rect: RECT,
    edge: AnchorEdge,
    width: i32,
    height: i32,
    work: RECT,
    alignment: ExpansionAlignment,
) -> POINT {
    let mut x = match alignment {
        ExpansionAlignment::Start => rect.left,
        ExpansionAlignment::End => rect.right.saturating_sub(width),
    };
    let mut y = match alignment {
        ExpansionAlignment::Start => rect.top,
        ExpansionAlignment::End => rect.bottom.saturating_sub(height),
    };
    match edge {
        AnchorEdge::Left => x = rect.left,
        AnchorEdge::Right => x = rect.right.saturating_sub(width),
        AnchorEdge::Top => y = rect.top,
        AnchorEdge::Bottom => y = rect.bottom.saturating_sub(height),
    }
    POINT {
        x: clamp_axis(x, width, work.left, work.right),
        y: clamp_axis(y, height, work.top, work.bottom),
    }
}

fn expansion_axis(
    start: i32,
    end: i32,
    extent: i32,
    work_start: i32,
    work_end: i32,
) -> (i32, ExpansionAlignment) {
    let end_aligned = end.saturating_sub(extent);
    let start_fits = start >= work_start && extent <= work_end.saturating_sub(start);
    let end_fits = end_aligned >= work_start && end <= work_end;
    if start_fits || !end_fits {
        (
            clamp_axis(start, extent, work_start, work_end),
            ExpansionAlignment::Start,
        )
    } else {
        (
            clamp_axis(end_aligned, extent, work_start, work_end),
            ExpansionAlignment::End,
        )
    }
}

fn clamp_axis(position: i32, extent: i32, work_start: i32, work_end: i32) -> i32 {
    let available = work_end.saturating_sub(work_start);
    if extent >= available {
        work_start
    } else {
        position.clamp(work_start, work_end - extent)
    }
}

pub(super) fn dip_to_px(value: f32, dpi: u32) -> i32 {
    (value * dpi as f32 / 96.0).round() as i32
}

pub(super) fn px_to_dip(value: i32, dpi: u32) -> f32 {
    value as f32 * 96.0 / dpi.max(1) as f32
}

pub(super) fn lerp(start: f32, end: f32, progress: f32) -> f32 {
    start + (end - start) * progress.clamp(0.0, 1.0)
}

/// 面板动画与百分环补间共用的缓动：先快后慢。
pub(super) fn ease_out_cubic(progress: f32) -> f32 {
    let inverse = 1.0 - progress.clamp(0.0, 1.0);
    1.0 - inverse * inverse * inverse
}

fn animation_sample(expanding: bool, timeline: f32) -> AnimationSample {
    let timeline = timeline.clamp(0.0, 1.0);
    let eased = ease_out_cubic(timeline);
    let (expansion, ball_opacity, panel_opacity) = if expanding {
        (
            eased,
            1.0 - (timeline / 0.3).clamp(0.0, 1.0),
            ((timeline - 0.4) / 0.6).clamp(0.0, 1.0),
        )
    } else {
        (
            1.0 - eased,
            ((timeline - 0.55) / 0.45).clamp(0.0, 1.0),
            1.0 - (timeline / 0.45).clamp(0.0, 1.0),
        )
    };
    AnimationSample {
        expansion,
        ball_opacity,
        panel_opacity,
        finished: timeline >= 1.0,
    }
}

pub(super) fn animation_shape_rect(
    animation: PanelAnimation,
    expansion: f32,
    dpi: u32,
    panel_height_dip: f32,
    edge: AnchorEdge,
    alignment: ExpansionAlignment,
) -> RECT {
    let width = dip_to_px(lerp(COLLAPSED_DIP, PANEL_WIDTH_DIP, expansion), dpi);
    let height = dip_to_px(lerp(COLLAPSED_DIP, panel_height_dip, expansion), dpi);
    let destination = anchored_destination(
        animation.anchor_rect,
        edge,
        width,
        height,
        animation.work_area,
        alignment,
    );
    RECT {
        left: destination.x,
        top: destination.y,
        right: destination.x + width,
        bottom: destination.y + height,
    }
}

pub(super) fn system_animations_enabled() -> bool {
    let mut enabled = 1_i32;
    // SAFETY: pvParam points to writable BOOL-compatible storage for the synchronous query.
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(ptr::from_mut(&mut enabled).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    result.map_or(true, |()| enabled != 0)
}

pub(super) fn point_in_rounded_rect(x: i32, y: i32, width: i32, height: i32, radius: i32) -> bool {
    if x < 0 || y < 0 || x >= width || y >= height {
        return false;
    }
    let corner_x = if x < radius {
        radius
    } else if x >= width - radius {
        width - radius - 1
    } else {
        return true;
    };
    let corner_y = if y < radius {
        radius
    } else if y >= height - radius {
        height - radius - 1
    } else {
        return true;
    };
    let dx = x - corner_x;
    let dy = y - corner_y;
    dx * dx + dy * dy <= radius * radius
}

pub(super) fn point_is_outside_rounded_rect(point: POINT, rect: RECT, radius: i32) -> bool {
    !point_in_rounded_rect(
        point.x - rect.left,
        point.y - rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
        radius,
    )
}

/// 收起态球的命中判定：以窗口中心为圆心、给定像素半径为半径的圆。半径由
/// 调用方传入渲染所用的同一常量，避免命中区域与画出的圆不一致。
pub(super) fn point_in_ball(x: i32, y: i32, width: i32, height: i32, radius: i32) -> bool {
    let dx = x - width / 2;
    let dy = y - height / 2;
    dx * dx + dy * dy <= radius * radius
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AnchorEdge;
    use crate::win32::{COLLAPSE_ANIMATION_DURATION, EXPAND_ANIMATION_DURATION};

    #[test]
    fn expanding_animation_starts_with_only_ball_content_visible() {
        let sample = animation_sample(true, 0.0);
        assert!(
            sample.expansion.abs() < f32::EPSILON
                && (sample.ball_opacity - 1.0).abs() < f32::EPSILON
                && sample.panel_opacity.abs() < f32::EPSILON
                && !sample.finished
        );
    }

    #[test]
    fn expanding_animation_finishes_with_only_panel_content_visible() {
        let sample = animation_sample(true, 1.0);
        assert!(
            (sample.expansion - 1.0).abs() < f32::EPSILON
                && sample.ball_opacity.abs() < f32::EPSILON
                && (sample.panel_opacity - 1.0).abs() < f32::EPSILON
                && sample.finished
        );
    }

    #[test]
    fn collapsing_animation_finishes_with_only_ball_content_visible() {
        let sample = animation_sample(false, 1.0);
        assert!(
            sample.expansion.abs() < f32::EPSILON
                && (sample.ball_opacity - 1.0).abs() < f32::EPSILON
                && sample.panel_opacity.abs() < f32::EPSILON
                && sample.finished
        );
    }

    #[test]
    fn panel_width_interpolation_reaches_midpoint_at_half_expansion() {
        assert!((lerp(COLLAPSED_DIP, PANEL_WIDTH_DIP, 0.5) - 172.0).abs() < f32::EPSILON);
    }

    #[test]
    fn collapsed_animation_shape_stays_inside_fixed_right_anchored_canvas() {
        let animation = PanelAnimation {
            started_at: Instant::now(),
            duration: COLLAPSE_ANIMATION_DURATION,
            anchor_rect: RECT {
                left: 1_431,
                top: 407,
                right: 1_707,
                bottom: 555,
            },
            work_area: RECT {
                left: 0,
                top: 0,
                right: 1_920,
                bottom: 1_040,
            },
            canvas_destination: POINT { x: 1_431, y: 407 },
            ball_center_screen: POINT { x: 1_679, y: 435 },
            expanding: false,
        };
        let shape = animation_shape_rect(
            animation,
            0.0,
            96,
            308.0,
            AnchorEdge::Right,
            ExpansionAlignment::Start,
        );
        assert_eq!(
            (shape.left, shape.top, shape.right, shape.bottom),
            (1_651, 407, 1_707, 463)
        );
    }

    #[test]
    fn expanded_animation_shape_follows_the_dynamic_panel_height() {
        // 展开终点高度由调用方按数据传入：无超额 227、双行超额 281。
        let animation = PanelAnimation {
            started_at: Instant::now(),
            duration: EXPAND_ANIMATION_DURATION,
            anchor_rect: RECT {
                left: 1_431,
                top: 407,
                right: 1_487,
                bottom: 463,
            },
            work_area: RECT {
                left: 0,
                top: 0,
                right: 1_920,
                bottom: 1_040,
            },
            canvas_destination: POINT { x: 1_431, y: 407 },
            ball_center_screen: POINT { x: 1_459, y: 435 },
            expanding: true,
        };
        let compact = animation_shape_rect(
            animation,
            1.0,
            96,
            227.0,
            AnchorEdge::Right,
            ExpansionAlignment::Start,
        );
        let tall = animation_shape_rect(
            animation,
            1.0,
            96,
            281.0,
            AnchorEdge::Right,
            ExpansionAlignment::Start,
        );

        assert_eq!(compact.bottom - compact.top, dip_to_px(227.0, 96));
        assert_eq!(tall.bottom - tall.top, dip_to_px(281.0, 96));
        assert_eq!(tall.right - tall.left, compact.right - compact.left);
    }

    #[test]
    fn rounded_hit_test_excludes_transparent_corner() {
        assert!(!point_in_rounded_rect(0, 0, 100, 50, 16));
        assert!(point_in_rounded_rect(16, 2, 100, 50, 16));
    }

    #[test]
    fn collapsed_ball_hit_test_matches_the_drawn_radius() {
        use super::point_in_ball;
        use crate::win32::renderer::BALL_RADIUS_DIP;

        // 56 dip 的正方形窗口、96 dpi：圆心 (28,28)，画出的圆半径 27 px，
        // 最外一圈（距圆心 28 px）是透明区，不得命中。
        let radius = dip_to_px(BALL_RADIUS_DIP, 96);
        assert_eq!(radius, 27);
        assert!(point_in_ball(1, 28, 56, 56, radius));
        assert!(!point_in_ball(0, 28, 56, 56, radius));
        // 旧实现用窗口半边长（28 px）当半径，会把透明的最外圈也算成球内。
        assert!(point_in_ball(0, 28, 56, 56, 56 / 2));
    }

    #[test]
    fn ring_tween_starts_at_the_old_value_and_settles_on_the_new_one() {
        let start = Instant::now();
        let animation = Tween::new(0.42, 0.08, start);

        let first = animation.sample(start);
        assert!((first.fraction - 0.42).abs() < f64::EPSILON && !first.finished);

        let middle = animation.sample(start + Tween::DURATION / 2);
        assert!(
            middle.fraction < 0.42 && middle.fraction > 0.08,
            "补间中点必须落在两端之间：{}",
            middle.fraction
        );

        let settled = animation.sample(start + Tween::DURATION);
        assert!((settled.fraction - 0.08).abs() < f64::EPSILON && settled.finished);

        // 超时后停在终点，不会越过目标。
        let late = animation.sample(start + Duration::from_secs(5));
        assert!((late.fraction - 0.08).abs() < f64::EPSILON);
    }

    #[test]
    fn ring_sweep_fills_from_zero_over_the_longer_sweep_duration() {
        let start = Instant::now();
        let sweep = Tween::sweep(0.65, start);

        assert!(sweep.sample(start).fraction.abs() < f64::EPSILON);
        assert!(!sweep.sample(start + Tween::SWEEP_DURATION / 2).finished);

        let settled = sweep.sample(start + Tween::SWEEP_DURATION);
        assert!((settled.fraction - 0.65).abs() < f64::EPSILON && settled.finished);
        // 扫入比数值补间长，扫入时长下不应提前结束。
        assert!(!sweep.sample(start + Tween::DURATION).finished);
    }

    #[test]
    fn label_rolls_are_shorter_than_the_arc_motion_they_accompany() {
        use std::hint::black_box;

        // 不变式：数字必须比同场景的弧先落定，否则出现"环已静止、数字还在滚"。
        // black_box 让断言不再是常量表达式，绕过 clippy::assertions_on_constants。
        assert!(black_box(Tween::LABEL_TWEEN) < black_box(Tween::DURATION));
        assert!(black_box(Tween::LABEL_SWEEP) < black_box(Tween::SWEEP_DURATION));
    }

    #[test]
    fn ring_tween_eases_out_so_it_is_past_halfway_at_midpoint() {
        let start = Instant::now();
        let animation = Tween::new(0.0, 1.0, start);

        let middle = animation.sample(start + Tween::DURATION / 2).fraction;

        assert!(middle > 0.5, "缓出曲线中点应已过半：{middle}");
    }

    #[test]
    fn ease_out_cubic_pins_both_ends_and_clamps_out_of_range_input() {
        assert!(ease_out_cubic(0.0).abs() < f32::EPSILON);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < f32::EPSILON);
        assert!(ease_out_cubic(-1.0).abs() < f32::EPSILON);
        assert!((ease_out_cubic(2.0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn snap_animation_slides_from_the_release_point_to_the_edge() {
        let start = Instant::now();
        let from = POINT { x: 400, y: 220 };
        let to = POINT { x: 0, y: 200 };
        // 松手时手是静止的：曲线退化成两端速度都为 0 的平滑过渡。
        let animation = SnapAnimation::new(from, to, (0.0, 0.0), start);

        let begin = animation.sample(start);
        assert_eq!(begin.destination, from);
        assert!(!begin.finished);

        // 始终落在两端之间。
        let middle = animation.sample(start + animation.duration / 2);
        assert_eq!(middle.destination.x, 200, "静止松手时中点是严格半程");
        assert!(middle.destination.x > to.x && middle.destination.y > to.y);

        let end = animation.sample(start + animation.duration);
        assert_eq!(end.destination, to);
        assert!(end.finished);

        // 超时采样也必须停在落点上，不能滑过头。
        let late = animation.sample(start + animation.duration * 4);
        assert_eq!(late.destination, to);
        assert!(late.finished);
    }

    /// 吸附必须从"手的速度"接着滑：第一个 16ms 帧走过的距离应当约等于
    /// `v0 × 16ms`，而不是缓出曲线自己定的一条速度。
    #[test]
    fn snap_starts_at_the_release_speed() {
        let start = Instant::now();
        let from = POINT { x: 900, y: 400 };
        let to = POINT { x: 0, y: 400 };
        // 手以 1.5px/ms 朝边缘移动（约 1500px/s，日常拖动速度）。
        let moving = SnapAnimation::new(from, to, (-1.5, 0.0), start);
        let still = SnapAnimation::new(from, to, (0.0, 0.0), start);

        let step = |animation: SnapAnimation| {
            let sample = animation.sample(start + Duration::from_millis(16));
            (from.x - sample.destination.x) as f32
        };
        let expected = 1.5 * 16.0;
        assert!(
            (step(moving) - expected).abs() <= 3.0,
            "第一个 16ms 帧应约走 {expected}px，实际 {}px",
            step(moving)
        );
        assert!(
            step(moving) > step(still) * 2.0,
            "带着手速起步应当明显快于静止松手（{}px vs {}px）",
            step(moving),
            step(still)
        );
    }

    /// 背向落点的手速不能让球先倒着走，超大的手速不能让球冲过边缘。
    ///
    /// 两个方向都要覆盖：背向的靠"只保留朝落点的分量"挡，朝落点但过快的靠
    /// `3·(p1−p0)/T` 这个上界挡——少测一边就会漏掉一条。
    #[test]
    fn snap_never_overshoots_the_edge() {
        let start = Instant::now();
        let from = POINT { x: 900, y: 400 };
        let to = POINT { x: 0, y: 400 };

        for velocity in [
            (-1.5, 0.0),   // 正常的朝落点速度
            (-20.0, 0.0),  // 快到足以冲过落点
            (-500.0, 0.0), // 离谱地快
            (8.0, 0.0),    // 背向落点
            (200.0, 0.0),  // 背向且离谱
            (0.0, 40.0),   // 纵向手速：横向滑行时它没有用武之地
        ] {
            let animation = SnapAnimation::new(from, to, velocity, start);
            for step in 0..=40 {
                let sample = animation.sample(start + animation.duration * step / 40);
                assert!(
                    sample.destination.x >= to.x && sample.destination.x <= from.x,
                    "速度 {velocity:?} 下 x 越界：{:?}",
                    sample.destination
                );
                assert_eq!(
                    sample.destination.y, from.y,
                    "横向速度不该影响 y（速度已按方向分量夹住）"
                );
            }
        }
    }

    #[test]
    fn snap_animation_reports_a_noop_when_it_is_already_at_the_edge() {
        let start = Instant::now();
        let point = POINT { x: 32, y: 64 };

        assert!(SnapAnimation::new(point, point, (0.0, 0.0), start).is_noop());
        assert!(
            !SnapAnimation::new(point, POINT { x: 33, y: 64 }, (0.0, 0.0), start).is_noop(),
            "一个像素的差也要滑，否则松手后看起来是瞬移"
        );
    }

    #[test]
    fn snap_duration_grows_with_distance_and_stops_at_a_cap() {
        let origin = POINT { x: 0, y: 0 };
        let near = duration_for(origin, POINT { x: 40, y: 0 });
        let middle = duration_for(origin, POINT { x: 400, y: 0 });
        let far = duration_for(origin, POINT { x: 900, y: 0 });
        let diagonal = duration_for(origin, POINT { x: 900, y: 900 });

        assert!(near < middle && middle < far, "距离越远滑得越久");
        assert!(near >= SnapAnimation::MIN_DURATION, "再近也不低于下限");
        assert_eq!(far, SnapAnimation::MAX_DURATION, "横跨半个屏幕也不超过上限");
        assert_eq!(diagonal, SnapAnimation::MAX_DURATION, "对角距离同样封顶");
        // 40px 的微调仍然干脆：约 112ms。
        assert!(near < Duration::from_millis(130), "{near:?}");
    }

    /// 回归：静止松手的长距离滑动，头一帧不能吃掉太多路程。
    ///
    /// 固定 140ms + 三次缓出时，900px 的滑动在第一个 16ms 帧里就走掉三成（约
    /// 270px），看起来是"跳"而不是"滑"——这正是"吸附不流畅"的来源之一。
    #[test]
    fn long_snap_moves_less_than_a_tenth_in_its_first_frame() {
        let start = Instant::now();
        let from = POINT { x: 900, y: 400 };
        let to = POINT { x: 0, y: 400 };
        let animation = SnapAnimation::new(from, to, (0.0, 0.0), start);

        let first = animation.sample(start + Duration::from_millis(16));
        let travelled = (from.x - first.destination.x) as f32 / (from.x - to.x) as f32;
        assert!(
            travelled < 0.03,
            "第一个 16ms 帧只该走一小段，实际走了 {:.1}%",
            travelled * 100.0
        );
        assert!(!first.finished, "16ms 远没滑完");
    }

    #[test]
    fn phase_at_keeps_its_precision_after_a_million_periods() {
        let start = Instant::now();
        let mut epoch = start;
        // 约 18 天之后（1e6 个 1.6 秒周期）：不重新锚定的话 f32 秒数在这个
        // 量级只剩约 0.1 秒分辨率，旋转会一格一格地跳。
        let late = start + SPIN_PERIOD * 1_000_000 + SPIN_PERIOD / 4;

        let phase = phase_at(&mut epoch, late, SPIN_PERIOD);

        assert!(
            phase.abs_diff(SPIN_PERIOD / 4) < Duration::from_micros(1),
            "长时间运行后相位漂移：{phase:?}"
        );
        // epoch 只前进整数个周期：它必须仍然落在 now 之前一个周期之内。
        assert!(late.saturating_duration_since(epoch) < SPIN_PERIOD);
    }

    #[test]
    fn pulse_opacity_cycles_between_full_and_minimum() {
        assert!((pulse_opacity(Duration::ZERO) - 1.0).abs() < 1e-6);
        assert!((pulse_opacity(PULSE_PERIOD / 2) - PULSE_MIN_OPACITY).abs() < 1e-6);
        assert!((pulse_opacity(PULSE_PERIOD) - 1.0).abs() < 1e-6);

        // 整个周期内都落在 [MIN, 1]，且相邻采样不跳变。
        let step = PULSE_PERIOD / 40;
        let mut previous = pulse_opacity(Duration::ZERO);
        for index in 1..=40 {
            let value = pulse_opacity(step * index);
            assert!((PULSE_MIN_OPACITY..=1.0).contains(&value), "越界：{value}");
            assert!((value - previous).abs() < 0.1, "相邻帧跳变过大：{value}");
            previous = value;
        }
    }

    #[test]
    fn point_beyond_card_requests_outside_collapse() {
        assert!(point_is_outside_rounded_rect(
            POINT { x: 300, y: 80 },
            RECT {
                left: 0,
                top: 0,
                right: 276,
                bottom: 148,
            },
            16,
        ));
    }

    #[test]
    fn point_inside_card_does_not_request_outside_collapse() {
        assert!(!point_is_outside_rounded_rect(
            POINT { x: 146, y: 74 },
            RECT {
                left: 0,
                top: 0,
                right: 276,
                bottom: 148,
            },
            16,
        ));
    }

    #[test]
    fn placement_is_clamped_inside_work_area() {
        let mut config = AppConfigV1::default();
        config.placement.offset_dip = 50_000.0;
        let details = MonitorDetails {
            info: MONITORINFOEXW {
                monitorInfo: MONITORINFO {
                    rcWork: RECT {
                        left: 0,
                        top: 0,
                        right: 1920,
                        bottom: 1040,
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        let point = position_from_config(&config, &details, 56, 96);
        assert_eq!(point, POINT { x: 1864, y: 984 });
    }

    #[test]
    fn right_anchored_expansion_preserves_outer_edge() {
        let (destination, alignment) = expanded_destination(
            RECT {
                left: 1800,
                top: 100,
                right: 1856,
                bottom: 156,
            },
            AnchorEdge::Right,
            276,
            148,
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            },
        );
        assert_eq!(
            (destination, alignment),
            (POINT { x: 1580, y: 100 }, ExpansionAlignment::Start)
        );
    }

    #[test]
    fn top_anchored_expansion_flips_left_when_right_side_does_not_fit() {
        let (destination, alignment) = expanded_destination(
            RECT {
                left: 1860,
                top: 0,
                right: 1916,
                bottom: 56,
            },
            AnchorEdge::Top,
            276,
            148,
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            },
        );
        assert_eq!(
            (destination, alignment),
            (POINT { x: 1640, y: 0 }, ExpansionAlignment::End)
        );
    }

    #[test]
    fn collapse_restores_far_edge_after_leftward_expansion() {
        let destination = anchored_destination(
            RECT {
                left: 1640,
                top: 0,
                right: 1916,
                bottom: 148,
            },
            AnchorEdge::Top,
            56,
            56,
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            },
            ExpansionAlignment::End,
        );
        assert_eq!(destination, POINT { x: 1860, y: 0 });
    }
}
