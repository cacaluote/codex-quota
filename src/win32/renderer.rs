#![allow(
    clippy::borrow_as_ptr,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]

use std::ffi::c_void;
use std::ptr;
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_F, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_HOLLOW,
    D2D1_FIGURE_END_OPEN, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL,
    D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE_SOLID, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_LINE_JOIN_ROUND,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES, D2D1_SWEEP_DIRECTION_CLOCKWISE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory,
    ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWriteCreateFactory,
    IDWriteFactory, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
    HGDIOBJ, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};
use windows::core::{Interface, PCWSTR};
use windows_numerics::Vector2;

use super::layout::{
    HOVER_TWEEN, PRESS_TWEEN, PULSE_PERIOD, RELEASE_TWEEN, RING_PULSE_FRAME_MILLIS,
    RING_SPIN_FRAME_MILLIS, RING_TWEEN_FRAME_MILLIS, SPIN_PERIOD, SPIN_SWEEP, Tween, phase_at,
    pulse_opacity,
};
use super::presentation::{
    FIRST_ROW_TOP_DIP, PlanColor, ROW_STEP_DIP, ball_is_pulling, ball_quota,
    display_period_total_token_estimate, display_period_total_value, display_windows,
    format_local_timestamp, format_reset_column, format_token_usage, format_usd, panel_title,
    period_labels, period_overflow_visible, plan_type_color, plan_type_label, quota_window_label,
    today_overflow_visible,
};
use crate::config::{ColorStyle, UnitStyle};
use crate::error::AppError;
use crate::quota::{AppState, QuotaColor, QuotaPull, QuotaWindow};

const BACKGROUND_RGB: (u8, u8, u8) = (0x12, 0x17, 0x20);
const BACKGROUND: D2D1_COLOR_F = rgba(BACKGROUND_RGB.0, BACKGROUND_RGB.1, BACKGROUND_RGB.2, 0.94);
/// 截图铺满位图的实心底。RGB 与 [`BACKGROUND`] 共用 [`BACKGROUND_RGB`]，仅 alpha 为 1。
const SNAPSHOT_BACKGROUND: D2D1_COLOR_F =
    rgba(BACKGROUND_RGB.0, BACKGROUND_RGB.1, BACKGROUND_RGB.2, 1.0);
const TRACK: D2D1_COLOR_F = rgba(0x42, 0x4a, 0x57, 0.88);
/// 悬停时的底色与轨道：只提亮、不换色相——球上唯一的彩色语义是额度。
const BACKGROUND_HOVER: D2D1_COLOR_F = rgba(0x1e, 0x25, 0x33, 0.97);
const TRACK_HOVER: D2D1_COLOR_F = rgba(0x54, 0x5e, 0x6e, 0.95);
/// 不确定进度弧：中性浅灰，与绿/黄/红三个额度色都不撞。
const SPINNER: D2D1_COLOR_F = rgba(0xc3, 0xcb, 0xd6, 1.0);
const PRIMARY_TEXT: D2D1_COLOR_F = rgba(0xf2, 0xf5, 0xf8, 1.0);
const SECONDARY_TEXT: D2D1_COLOR_F = rgba(0xa8, 0xb1, 0xbe, 1.0);
const UNKNOWN: D2D1_COLOR_F = rgba(0x7d, 0x87, 0x95, 1.0);

/// 一套额度状态配色：健康 / 警告 / 紧张。
///
/// 两套的色相基本同族（绿—琥珀—红），差别只在饱和度与明度，因此风格名按
/// 「处理方式」取而不是按颜色本身取（「薄荷」「珊瑚」在两套里都成立，读不出
/// 区别）。这是这三个色值唯一的出处，别处不许再写字面量。
///
/// 「未知」灰（[`UNKNOWN`]）与套餐徽标色不属于这套配色：前者是"没有数据"，
/// 两套风格下都该保持沉默；后者是身份不是严重度，换配色不该动它。
#[derive(Clone, Copy)]
struct Palette {
    healthy: D2D1_COLOR_F,
    warning: D2D1_COLOR_F,
    critical: D2D1_COLOR_F,
}

/// 「柔和」：为深色玻璃底调的一组中等饱和色。既有观感，也是默认。
const SOFT_PALETTE: Palette = Palette {
    healthy: rgba(0x45, 0xd4, 0x83, 1.0),
    warning: rgba(0xf2, 0xc9, 0x4c, 1.0),
    critical: rgba(0xf0, 0x66, 0x66, 1.0),
};

/// 「鲜艳」：同一族色相拉满饱和度与明度，在暗底上更抢眼。
///
/// 「紧张」取珊瑚红而非纯红：饱和度拉满后纯红在暗底上会显得发闷，往品红偏
/// 一点才和其它两色一样"亮"。注意它和「正常」青绿一样偏离了「柔和」的色相，
/// 前者靠近套餐徽标的 Business/Edu 青绿，换这套配色时这两处会读成一族颜色。
const VIVID_PALETTE: Palette = Palette {
    healthy: rgba(0x3d, 0xf2, 0xc1, 1.0),
    warning: rgba(0xff, 0xc8, 0x57, 1.0),
    critical: rgba(0xff, 0x64, 0x7c, 1.0),
};

impl Palette {
    const fn of(style: ColorStyle) -> Self {
        match style {
            ColorStyle::Soft => SOFT_PALETTE,
            ColorStyle::Vivid => VIVID_PALETTE,
        }
    }
}
const PLAN_NEUTRAL: D2D1_COLOR_F = rgba(0x8f, 0x9b, 0xaa, 1.0);
const PLAN_PLUS: D2D1_COLOR_F = rgba(0x5a, 0xa7, 0xff, 1.0);
const PLAN_PRO: D2D1_COLOR_F = rgba(0xa7, 0x8b, 0xfa, 1.0);
const PLAN_BUSINESS: D2D1_COLOR_F = rgba(0x4f, 0xd1, 0xc5, 1.0);
const PLAN_ENTERPRISE: D2D1_COLOR_F = rgba(0xd8, 0xb4, 0x5c, 1.0);
const PLAN_EDU: D2D1_COLOR_F = rgba(0x56, 0xc7, 0xd9, 1.0);
const TRANSPARENT: D2D1_COLOR_F = rgba(0, 0, 0, 0.0);
const TIME_COLUMN_LEFT: f32 = 148.0;
/// 重置列的右边界（dip）：比面板内边距（18）再靠近边缘一点。
///
/// 这一列要装 `09/18 23:26 (23h59m)` 这类最长的组合（实测 125.5 DIP），而
/// 148 → `width - 18` 只有 122 DIP。文字左对齐，这里放宽的是**裁剪边界**、
/// 不是文字位置：日常长度看起来毫无变化，只有最长的那几种才会用到这几点余量。
/// 右边界不能靠"让给左边"来换——左侧紧邻百分比/价值列，见下面的 const 守卫。
const RESET_COLUMN_RIGHT_PADDING: f32 = 10.0;
/// 重置列起点必须让开百分比/价值列（96 → 140）。
///
/// 与 `layout.rs` 里的节拍守卫同样的写法：`expect` 而不是 `assert!`，因为常量
/// 断言会被 `clippy::assertions_on_constants` 判成"恒定的断言"（本仓库 deny），
/// 而这个检查的对象恰恰就是常量本身。
#[expect(
    clippy::manual_assert,
    reason = "常量断言会被 assertions_on_constants 拒绝，这里的检查对象就是常量本身"
)]
const _: () = if TIME_COLUMN_LEFT < 140.0 {
    panic!("重置列会压到百分比/价值列");
};
/// 文本字号（dip）：居中百分比、面板标题、正文。初次创建与设备资源重建
/// 必须共用同一组常量，否则 DPI 变化或一次绘制失败重试后字号会漂移。
const PERCENT_FONT_SIZE: f32 = 18.0;
const TITLE_FONT_SIZE: f32 = 14.0;
const BODY_FONT_SIZE: f32 = 13.0;
/// 收起球背景圆的半径（dip）。窗口边长是 56 dip，留 1 dip 余量避免抗锯齿
/// 被裁切；命中测试共用此常量，两处不得各写各的数字。
pub(super) const BALL_RADIUS_DIP: f32 = 27.0;
/// 轨道圆与进度弧的半径（dip）及描边宽度。外沿 = 半径 + 描边/2 = 24.5 dip，
/// 仍在背景圆 27 dip 之内，因此弧不会压到球的边缘。
const RING_RADIUS_DIP: f32 = 23.0;
const RING_STROKE_DIP: f32 = 3.0;
/// 不确定进度弧画在内圈：比额度弧细、半径更小，任何额度值下都不会与它重叠，
/// 也不会压到中心数字（数字半高 13 dip，内圈内沿 18 dip）。
const SPIN_RADIUS_DIP: f32 = 19.0;
const SPIN_STROKE_DIP: f32 = 2.0;
/// 按下时球缩小 6%：够看出"被按住了"，又不会小到像换了个控件。
const PRESS_SCALE: f32 = 0.06;

#[derive(Clone, Copy, Debug)]
pub(super) enum VisualState {
    Collapsed,
    Expanded,
    Transition(TransitionVisual),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TransitionVisual {
    pub expansion: f32,
    pub ball_opacity: f32,
    pub panel_opacity: f32,
    pub ball_center: (f32, f32),
    pub shape_bounds: (f32, f32, f32, f32),
}

const fn rgba(red: u8, green: u8, blue: u8, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: red as f32 / 255.0,
        g: green as f32 / 255.0,
        b: blue as f32 / 255.0,
        a: alpha,
    }
}

pub struct Renderer {
    surface: DibSurface,
    factory: ID2D1Factory,
    target: ID2D1DCRenderTarget,
    write_factory: IDWriteFactory,
    brushes: Brushes,
    /// 圆角端点的描边样式，供额度弧与不确定进度弧共用。
    stroke_style: ID2D1StrokeStyle,
    percent_format: IDWriteTextFormat,
    title_format: IDWriteTextFormat,
    body_format: IDWriteTextFormat,
    ring: RingState,
    pointer: PointerState,
    /// UI 层推送的指针目标（悬停、按下）；补间的推进发生在真正画球时。
    pointer_targets: (bool, bool),
    /// 当前笔刷用的配色风格。绘制前与状态里的风格比对，不一致就只重建那三支
    /// 状态笔刷——面板截图是"克隆状态 + 新建渲染器"，把风格留在状态里，这条
    /// 路径才不用自己记得同步。
    palette: ColorStyle,
    dpi: u32,
}

struct Brushes {
    background: ID2D1SolidColorBrush,
    background_hover: ID2D1SolidColorBrush,
    track: ID2D1SolidColorBrush,
    track_hover: ID2D1SolidColorBrush,
    spinner: ID2D1SolidColorBrush,
    primary_text: ID2D1SolidColorBrush,
    secondary_text: ID2D1SolidColorBrush,
    /// 额度状态三色（配色风格决定具体色值，见 [`Palette`]）。字段名按语义取，
    /// 不按颜色取：换一套风格它们就不再是"绿/黄/红"了。
    healthy: ID2D1SolidColorBrush,
    warning: ID2D1SolidColorBrush,
    critical: ID2D1SolidColorBrush,
    unknown: ID2D1SolidColorBrush,
    plan_neutral: ID2D1SolidColorBrush,
    plan_plus: ID2D1SolidColorBrush,
    plan_pro: ID2D1SolidColorBrush,
    plan_business: ID2D1SolidColorBrush,
    plan_enterprise: ID2D1SolidColorBrush,
    plan_edu: ID2D1SolidColorBrush,
}

struct DibSurface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}

/// 面板的离屏像素快照：像素尺寸与预乘 BGRA 缓冲。
///
/// 缓冲是独立于 [`Renderer`] 的一份拷贝：`DibSurface` 在 Drop 里会释放 DIB，
/// 导出时必须拷出来，不能借用渲染器的内存。
pub(super) struct PanelBitmap {
    pub(super) width: i32,
    pub(super) height: i32,
    /// 自上而下、每行 `width * 4` 字节，通道顺序 BGRA，alpha 已预乘。
    pub(super) pixels: Vec<u8>,
}

/// 悬浮球百分环的动画状态：额度补间 + 低额度脉冲 + 不确定进度弧。
///
/// 状态放在渲染器里，因为动画只在真正画球时推进；UI 层只负责按
/// [`Renderer::animation_frame_interval`] 驱动帧定时器，并在不需要帧时停表。
#[derive(Debug)]
struct RingState {
    animation: Option<Tween>,
    /// 中心数字的滚动动画：与弧同时起步，但用更短的时长先落定。
    label_animation: Option<Tween>,
    /// 上一次已知的目标比例；None 表示不可知或为 0（此时不画弧）。
    target: Option<f64>,
    /// 脉冲相位起点：渲染器创建时开始，使球每次出现都从最亮处起步。
    pulse_epoch: Instant,
    /// 不确定进度弧的相位起点：同样从创建时开始，转动方向与弧一致。
    spin_epoch: Instant,
    /// 是否允许动效：系统动画开关 ∧ 球可见 ∧ 面板未展开 ∧ 未在拖拽。
    enabled: bool,
    /// 上一帧是否有脉冲在跑（决定帧率与是否需要继续出帧）。
    pulsing: bool,
    /// 上一帧是否有旋转弧在跑。
    spinning: bool,
    /// 下一个已知值要从 0 扫入：只在球刚出现时置位，消费一次即清。
    sweep_pending: bool,
}

impl RingState {
    fn new(now: Instant) -> Self {
        Self {
            animation: None,
            label_animation: None,
            target: None,
            pulse_epoch: now,
            spin_epoch: now,
            enabled: true,
            pulsing: false,
            spinning: false,
            // 渲染器随悬浮窗一起创建，因此首个值就是"球刚出现"。
            sweep_pending: true,
        }
    }

    /// 关闭动效时立即回到终值，避免停在补间中途或某帧的暗色上。
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.animation = None;
            self.label_animation = None;
            self.pulsing = false;
            self.spinning = false;
        }
    }

    /// 球重新出现（取消隐藏后再次显示）：忘掉上次的值，让下一个已知值重新扫入。
    fn expect_appearance(&mut self) {
        self.animation = None;
        self.label_animation = None;
        self.target = None;
        self.sweep_pending = true;
    }

    /// 下一次拿到已知值时重新扫入，但**不清掉**当前值。
    ///
    /// 手动刷新期间球被清空（见 `draw_ball_content`），刷新结束后新值应该像球刚
    /// 出现那样从 0 扫进去，而不是"啪"地跳回来。
    fn expect_resweep(&mut self) {
        self.sweep_pending = true;
    }

    /// 值变化时起/续补间；球刚出现的首个值从 0 扫入；变得不可知或归零
    /// 直接跳——这些是离散状态，插值会停在无意义的位置。中心数字与弧
    /// 同时起步，但用更短的时长先落定。
    fn retarget(&mut self, target: Option<f64>, now: Instant) {
        if self.target == target {
            // 值没变：让进行中的补间跑完，不要重新开始。
            return;
        }
        // 必须在覆盖 target 之前采样：起点是旧值（或在途的插值）。
        let arc_from = self.fraction(now);
        let label_from = self.label_fraction(now).or(self.target);
        self.target = target;
        if !self.enabled {
            self.animation = None;
            self.label_animation = None;
            return;
        }
        let (animation, label_animation) = match (arc_from, target) {
            // 球刚出现：弧与数字都从 0 起步。
            (None, Some(to)) if self.sweep_pending => {
                self.sweep_pending = false;
                (
                    Some(Tween::sweep(to, now)),
                    Some(Tween::label_sweep(to, now)),
                )
            }
            // 值变化：以当前插值结果为新起点，补间中途换目标时不会跳变。
            (Some(from), Some(to)) => (
                Some(Tween::new(from, to, now)),
                label_from.map(|from| Tween::label_tween(from, to, now)),
            ),
            // 其余情况直接跳：变得不可知或归零是离散状态，插值会停在无意义的
            // 位置；数据从不可知恢复时球一直在，也无需再扫入一次。
            _ => (None, None),
        };
        self.animation = animation;
        self.label_animation = label_animation;
    }

    /// 本帧弧要画的比例；None 表示不画弧。
    fn fraction(&mut self, now: Instant) -> Option<f64> {
        if let Some(animation) = self.animation {
            let sample = animation.sample(now);
            if sample.finished {
                self.animation = None;
            }
            return Some(sample.fraction);
        }
        self.target
    }

    /// 本帧中心数字要显示的比例；None 表示数字不在滚动，用静态标签。
    fn label_fraction(&mut self, now: Instant) -> Option<f64> {
        let animation = self.label_animation?;
        let sample = animation.sample(now);
        if sample.finished {
            // 收尾交给静态标签，避免最后一位出现格式化差异。
            self.label_animation = None;
            return None;
        }
        Some(sample.fraction)
    }

    /// 本帧脉冲的不透明度系数；`wanted` 表示当前处于**仍有余额**的低额度。
    ///
    /// 已耗尽（0%）不在其中：它是冻结状态，只有时钟能改变它，重复的动效不带
    /// 来新信息，而它恰恰是停留最久的状态（5 小时窗口按小时、周窗口按天）。
    fn pulse_factor(&mut self, now: Instant, wanted: bool) -> f32 {
        self.pulsing = wanted && self.enabled;
        if !self.pulsing {
            return 1.0;
        }
        let phase = phase_at(&mut self.pulse_epoch, now, PULSE_PERIOD);
        pulse_opacity(phase)
    }

    /// 本帧不确定进度弧的起点比例；None 表示不画。
    ///
    /// 它只在**真正在途**时出现（见 `ball_is_pulling`）：一次拉取在途，或启动阶段
    /// 还没拿到第一份额度。两种都终结在同一处——首次读取成功（球上有值了），
    /// 或彻底失败（`status` 离开 `Connecting`），因此不会像"等待重试"那样无限转。
    fn spin_phase(&mut self, now: Instant, wanted: bool) -> Option<f64> {
        self.spinning = wanted && self.enabled;
        if !self.spinning {
            return None;
        }
        let phase = phase_at(&mut self.spin_epoch, now, SPIN_PERIOD);
        Some(phase.as_secs_f64() / SPIN_PERIOD.as_secs_f64())
    }

    /// 还需要多少毫秒出一帧；None 表示当前不需要帧定时器。
    fn frame_interval(&self, now: Instant) -> Option<u32> {
        if !self.enabled {
            return None;
        }
        // 弧与数字各自独立：任一在动都需要继续出帧（数字若配得更长也不会卡住）。
        let tweening = self
            .animation
            .is_some_and(|animation| !animation.sample(now).finished)
            || self
                .label_animation
                .is_some_and(|animation| !animation.sample(now).finished);
        if tweening {
            return Some(RING_TWEEN_FRAME_MILLIS);
        }
        if self.spinning {
            return Some(RING_SPIN_FRAME_MILLIS);
        }
        self.pulsing.then_some(RING_PULSE_FRAME_MILLIS)
    }
}

/// 一条由布尔驱动的补间：目标是"在不在"，值是 0..1 的强度。
///
/// 值本身是唯一的事实来源，补间只是它的一次过渡；目标没变就绝不重起补间，
/// 否则每帧都会从当前值重新出发、永远到不了终点。
#[derive(Debug)]
struct BoolTween {
    target: bool,
    value: f32,
    tween: Option<Tween>,
}

impl BoolTween {
    fn new() -> Self {
        Self {
            target: false,
            value: 0.0,
            tween: None,
        }
    }

    /// 目标变化时从当前值起步；`enabled` 为假则直接落到目标值。
    fn set_target(&mut self, target: bool, duration: Duration, enabled: bool, now: Instant) {
        if target == self.target {
            return;
        }
        self.target = target;
        let to = if target { 1.0 } else { 0.0 };
        if enabled {
            self.tween = Some(Tween::timed(
                f64::from(self.value),
                f64::from(to),
                now,
                duration,
            ));
        } else {
            self.tween = None;
            self.value = to;
        }
    }

    /// 立即落到目标值并丢弃在途补间。
    fn snap(&mut self) {
        self.tween = None;
        self.value = if self.target { 1.0 } else { 0.0 };
    }

    fn sample(&mut self, now: Instant) -> f32 {
        if let Some(tween) = self.tween {
            let sample = tween.sample(now);
            self.value = sample.fraction as f32;
            if sample.finished {
                self.tween = None;
            }
        }
        self.value
    }

    /// 是否有一条补间在跑。稳态（值已达目标）不算——悬停住不动时不该有帧。
    fn tweening(&self) -> bool {
        self.tween.is_some()
    }
}
/// 指针态动效：悬停提亮与按下缩放。
///
/// 目标是布尔（光标在不在球上、按钮按没按下），采样出 0..1 的强度。关闭动效
/// 时直接给目标值："不播放动画"不等于"不指示状态"。
#[derive(Debug)]
struct PointerState {
    hover: BoolTween,
    press: BoolTween,
    enabled: bool,
}

impl PointerState {
    fn new() -> Self {
        Self {
            hover: BoolTween::new(),
            press: BoolTween::new(),
            enabled: true,
        }
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.hover.snap();
            self.press.snap();
        }
    }

    fn set_targets(&mut self, hovered: bool, pressed: bool, now: Instant) {
        self.hover
            .set_target(hovered, HOVER_TWEEN, self.enabled, now);
        // 按下快、回弹慢：手感差别全在这两个时长上。
        let press_duration = if pressed { PRESS_TWEEN } else { RELEASE_TWEEN };
        self.press
            .set_target(pressed, press_duration, self.enabled, now);
    }

    /// 本帧的（悬停强度, 按下强度）。
    fn sample(&mut self, now: Instant) -> (f32, f32) {
        (self.hover.sample(now), self.press.sample(now))
    }

    fn tweening(&self) -> bool {
        self.hover.tweening() || self.press.tweening()
    }
}

/// 两个候选帧间隔取更快的那个；都是 None 才是真的不需要出帧。
fn faster_interval(first: Option<u32>, second: Option<u32>) -> Option<u32> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (first, None) | (None, first) => first,
    }
}

impl Renderer {
    pub fn new(width: i32, height: i32, dpi: u32) -> Result<Self, AppError> {
        let surface = DibSurface::new(width, height)?;
        // SAFETY: Direct2D and DirectWrite factories are created and used only on the UI thread.
        let factory = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }?;
        // SAFETY: Shared DirectWrite factory creation has no borrowed output lifetime.
        let write_factory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }?;
        let target = create_target(&factory, dpi)?;
        let brushes = create_brushes(&target, ColorStyle::default())?;
        // 描边样式是工厂级资源（与设备无关），因此不随设备资源重建。
        let stroke_style = create_round_stroke_style(&factory)?;
        let percent_format = create_text_format(&write_factory, PERCENT_FONT_SIZE, true)?;
        let title_format = create_text_format(&write_factory, TITLE_FONT_SIZE, false)?;
        let body_format = create_text_format(&write_factory, BODY_FONT_SIZE, false)?;

        Ok(Self {
            surface,
            factory,
            target,
            write_factory,
            brushes,
            stroke_style,
            percent_format,
            title_format,
            body_format,
            ring: RingState::new(Instant::now()),
            pointer: PointerState::new(),
            pointer_targets: (false, false),
            palette: ColorStyle::default(),
            dpi,
        })
    }

    /// 由 UI 层在每帧绘制前设置：系统动画开关 ∧ 球可见 ∧ 面板未展开 ∧ 未拖拽。
    /// 关闭时会丢弃进行中的补间，避免停在中间帧；但悬停与按下本身仍会立刻
    /// 显示出来——系统要的是"别播放动画"，不是"别指示状态"。
    pub(super) fn set_animations_enabled(&mut self, enabled: bool) {
        self.ring.set_enabled(enabled);
        self.pointer.set_enabled(enabled);
    }

    /// 由 UI 层在每帧绘制前推送当前的指针态。
    pub(super) fn set_pointer_targets(&mut self, hovered: bool, pressed: bool) {
        self.pointer_targets = (hovered, pressed);
    }

    /// 悬浮球动效还需多少毫秒出一帧；None 表示可以停掉帧定时器。
    pub(super) fn animation_frame_interval(&self) -> Option<u32> {
        let now = Instant::now();
        let ring = self.ring.frame_interval(now);
        // 指针补间固定用补间帧率：它只有 80–150ms，值得跑满帧。
        let pointer = self.pointer.tweening().then_some(RING_TWEEN_FRAME_MILLIS);
        faster_interval(ring, pointer)
    }

    /// 悬浮球重新出现（取消隐藏后再次显示）时调用，让弧重新从 0 扫入。
    pub(super) fn restart_ring_sweep(&mut self) {
        self.ring.expect_appearance();
    }

    pub fn resize(&mut self, width: i32, height: i32, dpi: u32) -> Result<(), AppError> {
        if self.surface.width != width || self.surface.height != height {
            self.surface = DibSurface::new(width, height)?;
        }
        if self.dpi != dpi {
            self.recreate_device_resources(dpi)?;
        }
        Ok(())
    }

    pub fn render(
        &mut self,
        hwnd: HWND,
        destination: POINT,
        visual: VisualState,
        state: &AppState,
    ) -> Result<(), AppError> {
        let result = self.draw(visual, state);
        if result.is_err() {
            self.recreate_device_resources(self.dpi)?;
            self.draw(visual, state)?;
        }
        self.surface.commit(hwnd, destination)
    }

    /// 把当前展开面板画进本渲染器，并把像素拷成一份独立快照。
    ///
    /// 只在临时渲染器上调用：画面尺寸取自 surface 自身的像素尺寸，调用方按面板
    /// 的 DIP 尺寸乘 DPI 建好渲染器即可。不走 `draw` 的 `Expanded` 分支——那条路
    /// 清成透明再画圆角玻璃底，截图贴到白底/黑底上会在四角露馅。这里铺不透明
    /// 面板色再画内容，整张图是直角实心卡。`draw_panel_content` 取 `&self`，不
    /// 推进 `RingState`；`draw_snapshot` 的 `&mut self` 只作用于这个临时渲染器。
    pub(super) fn panel_snapshot(&mut self, state: &AppState) -> Result<PanelBitmap, AppError> {
        self.draw_snapshot(state)?;
        Ok(PanelBitmap {
            width: self.surface.width,
            height: self.surface.height,
            pixels: self.copy_pixels(),
        })
    }

    fn recreate_device_resources(&mut self, dpi: u32) -> Result<(), AppError> {
        self.target = create_target(&self.factory, dpi)?;
        self.brushes = create_brushes(&self.target, self.palette)?;
        self.percent_format = create_text_format(&self.write_factory, PERCENT_FONT_SIZE, true)?;
        self.title_format = create_text_format(&self.write_factory, TITLE_FONT_SIZE, false)?;
        self.body_format = create_text_format(&self.write_factory, BODY_FONT_SIZE, false)?;
        self.dpi = dpi;
        Ok(())
    }

    /// 把状态里的配色风格落到笔刷上：只有真的换了风格才重建，且只重建那三支
    /// 状态笔刷（底色、文字、轨道、徽标都不随风格变）。
    fn sync_palette(&mut self, style: ColorStyle) -> Result<(), AppError> {
        if self.palette == style {
            return Ok(());
        }
        let palette = Palette::of(style);
        let render_target: ID2D1RenderTarget = self.target.cast()?;
        let brush = |color: &D2D1_COLOR_F| {
            // SAFETY: color is borrowed only for this synchronous factory call.
            unsafe { render_target.CreateSolidColorBrush(color, None) }
        };
        self.brushes.healthy = brush(&palette.healthy)?;
        self.brushes.warning = brush(&palette.warning)?;
        self.brushes.critical = brush(&palette.critical)?;
        self.palette = style;
        Ok(())
    }

    fn draw(&mut self, visual: VisualState, state: &AppState) -> Result<(), AppError> {
        self.sync_palette(state.color_style)?;
        self.prepare_draw(&TRANSPARENT)?;

        match visual {
            VisualState::Collapsed => self.draw_ball(state)?,
            VisualState::Expanded => self.draw_panel(state),
            VisualState::Transition(transition) => {
                self.draw_transition(state, transition)?;
            }
        }

        self.end_draw()
    }

    /// 截图专用：不透明面板色铺满位图，再画文字，不画圆角透明底。
    fn draw_snapshot(&mut self, state: &AppState) -> Result<(), AppError> {
        self.sync_palette(state.color_style)?;
        self.prepare_draw(&SNAPSHOT_BACKGROUND)?;
        self.draw_panel_content(state, 1.0);
        self.end_draw()
    }

    fn prepare_draw(&mut self, clear: &D2D1_COLOR_F) -> Result<(), AppError> {
        self.surface.clear();
        let bounds = RECT {
            left: 0,
            top: 0,
            right: self.surface.width,
            bottom: self.surface.height,
        };
        // SAFETY: the memory DC and its selected DIB remain valid for the full draw operation.
        unsafe {
            self.target.BindDC(self.surface.dc, &bounds)?;
            self.target
                .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            self.target.BeginDraw();
            self.target.Clear(Some(clear));
        }
        Ok(())
    }

    fn end_draw(&mut self) -> Result<(), AppError> {
        // SAFETY: EndDraw balances the BeginDraw in prepare_draw on the same UI thread.
        unsafe { self.target.EndDraw(None, None) }?;
        Ok(())
    }

    fn copy_pixels(&self) -> Vec<u8> {
        // SAFETY: bits points to this surface's byte_len()-byte DIB allocation, which stays
        // valid while the surface is alive; the bytes are copied out before the renderer drops.
        unsafe { std::slice::from_raw_parts(self.surface.bits, self.surface.byte_len()) }.to_vec()
    }

    fn draw_ball(&mut self, state: &AppState) -> Result<(), AppError> {
        let now = Instant::now();
        self.pointer
            .set_targets(self.pointer_targets.0, self.pointer_targets.1, now);
        let (hover, press) = self.pointer.sample(now);
        // 按下把整个球（底色 + 两个环 + 数字）一起缩小，不是只改某一层。
        let ball_scale = 1.0 - PRESS_SCALE * press;
        let dpi_scale = self.dpi as f32 / 96.0;
        let center = Vector2 {
            X: self.surface.width as f32 / (2.0 * dpi_scale),
            Y: self.surface.height as f32 / (2.0 * dpi_scale),
        };
        self.draw_ball_background(center, ball_scale, hover);
        self.draw_ball_content(state, center, 1.0, ball_scale, hover, now)
    }

    /// 球的底色。悬停时把底色整体提亮一档：分层窗口的圆角外没有任何边界可画，
    /// 而底色一变，整个球的存在感就变了——这是"可以点"最省的提示。
    fn draw_ball_background(&self, center: Vector2, ball_scale: f32, hover: f32) {
        let radius = BALL_RADIUS_DIP * ball_scale;
        let circle = D2D1_ELLIPSE {
            point: center,
            radiusX: radius,
            radiusY: radius,
        };
        // SAFETY: the geometry and brush are renderer-owned and valid for this immediate call.
        unsafe {
            self.target.FillEllipse(&circle, &self.brushes.background);
            if hover > 0.0 {
                self.brushes.background_hover.SetOpacity(hover);
                self.target
                    .FillEllipse(&circle, &self.brushes.background_hover);
                self.brushes.background_hover.SetOpacity(1.0);
            }
        }
    }

    fn draw_ball_content(
        &mut self,
        state: &AppState,
        center: Vector2,
        opacity: f32,
        ball_scale: f32,
        hover: f32,
        now: Instant,
    ) -> Result<(), AppError> {
        let opacity = opacity.clamp(0.0, 1.0);
        let hover = hover.clamp(0.0, 1.0);
        let ring_radius = RING_RADIUS_DIP * ball_scale;
        // The ball is the glanceable summary: primary window remaining (or the
        // only window of single-window accounts), reading 0 while any active
        // window is exhausted and -- while no fresh snapshot is known.
        let (label, remaining, color) = ball_quota(state, SystemTime::now());
        // 手动刷新期间球被清空（见 `ball_quota`），刷新结束后新值应该像球刚出现
        // 那样从 0 扫进去，而不是"啪"地跳回来。
        if matches!(state.quota_pull, QuotaPull::Forced) {
            self.ring.expect_resweep();
        }
        // 只有 0 < 剩余量 才有弧；不可知与 0 都是离散状态，直接跳不做补间。
        self.ring
            .retarget((remaining > 0.0).then_some(remaining / 100.0), now);
        let arc = self.ring.fraction(now).filter(|fraction| *fraction > 0.0);
        // 数字跟着弧滚，但用自己的（更短的）时长；滚完后回落到静态标签，
        // 保证静止时的数值与面板完全一致。
        let label = match (arc, self.ring.label_fraction(now)) {
            (Some(_), Some(fraction)) => format!("{:.0}", fraction * 100.0),
            _ => label,
        };
        // 只有"还有余额的低额度"才呼吸；0% 与不可知都不动，应用因此可以停表。
        let low = matches!(color, QuotaColor::Critical) && remaining > 0.0;
        let pulse = self.ring.pulse_factor(now, low);
        let track_opacity = if arc.is_some() {
            opacity
        } else {
            opacity * pulse
        };
        // 已耗尽与无数据都没有弧，环一律保持中性轨道：这个球的视觉语言只有
        // "弧长 = 余量""整环 = 接近满额"两条，红色也已经专属给 <20%，给 0% 换
        // 一只红色整环会同时破坏两者（满额与耗尽共用一个形状）。0% 因此不靠环
        // 也不靠动效说话，它只报中心那个 `0`。
        let track = D2D1_ELLIPSE {
            point: center,
            radiusX: ring_radius,
            radiusY: ring_radius,
        };
        // 悬停时轨道在两种颜色之间交叉淡入，透明度不变：暗轨道变亮，但不会
        // 看起来像多了一条额度弧。
        // SAFETY: both brushes are renderer-owned and their opacity is restored below.
        unsafe {
            let base = track_opacity * (1.0 - hover);
            if base > 0.0 {
                self.brushes.track.SetOpacity(base);
                self.target
                    .DrawEllipse(&track, &self.brushes.track, RING_STROKE_DIP, None);
                self.brushes.track.SetOpacity(1.0);
            }
            if hover > 0.0 {
                self.brushes.track_hover.SetOpacity(track_opacity * hover);
                self.target
                    .DrawEllipse(&track, &self.brushes.track_hover, RING_STROKE_DIP, None);
                self.brushes.track_hover.SetOpacity(1.0);
            }
        }

        // 不确定进度弧画在内圈：无论额度弧取什么值都不会与它重叠。
        let spinning = ball_is_pulling(state, color);
        if let Some(phase) = self.ring.spin_phase(now, spinning) {
            let result = self.draw_arc(
                center,
                SPIN_RADIUS_DIP * ball_scale,
                SPIN_STROKE_DIP,
                phase,
                SPIN_SWEEP,
                &self.brushes.spinner,
            );
            result?;
        }

        if let Some(fraction) = arc {
            let brush = self.brush_for(color);
            // SAFETY: the brush is renderer-owned and its opacity is restored after the draw.
            unsafe { brush.SetOpacity(opacity * pulse) };
            let result = self.draw_progress_arc(center, ring_radius, fraction, brush);
            // SAFETY: restores the shared brush even if geometry creation failed.
            unsafe { brush.SetOpacity(1.0) };
            result?;
        }
        self.draw_text_with_opacity(
            &label,
            &self.percent_format,
            &self.brushes.primary_text,
            D2D_RECT_F {
                left: center.X - 24.0,
                top: center.Y - 13.0,
                right: center.X + 24.0,
                bottom: center.Y + 13.0,
            },
            opacity,
        );
        Ok(())
    }

    fn draw_panel(&self, state: &AppState) {
        let scale = self.dpi as f32 / 96.0;
        self.draw_rounded_background(
            D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: self.surface.width as f32 / scale,
                bottom: self.surface.height as f32 / scale,
            },
            16.0,
        );
        self.draw_panel_content(state, 1.0);
    }

    fn draw_transition(
        &mut self,
        state: &AppState,
        transition: TransitionVisual,
    ) -> Result<(), AppError> {
        let radius = 28.0 + (16.0 - 28.0) * transition.expansion.clamp(0.0, 1.0);
        let shape = D2D_RECT_F {
            left: transition.shape_bounds.0,
            top: transition.shape_bounds.1,
            right: transition.shape_bounds.2,
            bottom: transition.shape_bounds.3,
        };
        self.draw_rounded_background(shape, radius);
        if transition.ball_opacity > 0.0 {
            // 过渡中的球只是淡出/淡入的一层，不跟着指针缩放或提亮：那两件事
            // 属于"球是一个可点的控件"，而这一刻它正在变成面板。（试过把按下
            // 那一刻的缩放冻结下来带进过渡，但过渡里球的外圈已经变成面板的容器
            // 形状，缩放带不走——结果会是"外圈弹回、内圈还缩着"的不一致，而球
            // 在展开的头 30% 就淡完了，那点差别看不出来。）
            self.draw_ball_content(
                state,
                Vector2 {
                    X: transition.ball_center.0,
                    Y: transition.ball_center.1,
                },
                transition.ball_opacity,
                1.0,
                0.0,
                Instant::now(),
            )?;
        }
        if transition.panel_opacity > 0.0 {
            // SAFETY: the finite clip is popped before this draw call returns.
            unsafe {
                self.target
                    .PushAxisAlignedClip(&shape, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            }
            self.draw_panel_content(state, transition.panel_opacity);
            // SAFETY: balances the PushAxisAlignedClip immediately above.
            unsafe { self.target.PopAxisAlignedClip() };
        }
        Ok(())
    }

    fn draw_rounded_background(&self, bounds: D2D_RECT_F, radius: f32) {
        let panel = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: bounds.left + 0.5,
                top: bounds.top + 0.5,
                right: bounds.right - 0.5,
                bottom: bounds.bottom - 0.5,
            },
            radiusX: radius,
            radiusY: radius,
        };
        // SAFETY: the rounded rectangle and brush are valid for this immediate call.
        unsafe {
            self.target
                .FillRoundedRectangle(&panel, &self.brushes.background);
        };
    }

    fn draw_panel_content(&self, state: &AppState, opacity: f32) {
        let scale = self.dpi as f32 / 96.0;
        let width = self.surface.width as f32 / scale;

        let (status, plan_type) = panel_title(state);
        self.draw_text_with_opacity(
            status,
            &self.title_format,
            &self.brushes.primary_text,
            D2D_RECT_F {
                left: 18.0,
                top: 14.0,
                right: if plan_type.is_some() {
                    64.0
                } else {
                    width - 18.0
                },
                bottom: 36.0,
            },
            opacity,
        );
        if let Some(plan_type) = plan_type {
            self.draw_text_with_opacity(
                plan_type_label(plan_type),
                &self.title_format,
                self.brush_for_plan_type(plan_type),
                D2D_RECT_F {
                    left: 64.0,
                    top: 14.0,
                    right: width - 18.0,
                    bottom: 36.0,
                },
                opacity,
            );
        }

        let (short_term, long_term) = display_windows(state, SystemTime::now());
        let short_term_label = quota_window_label(short_term, state.plan_type.as_deref(), true);
        let long_term_label = quota_window_label(long_term, state.plan_type.as_deref(), false);
        // 期间行的前缀与上面那行的窗口名同源（周额度 → 本周，月额度 → 本月），
        // 三个标签一起生成，免得某个调用点漏掉后缀。
        let period = period_labels(long_term, state.plan_type.as_deref());
        // 行位按可见行顺序排布：超额行大多不发生，未发生时不占位，面板
        // 高度由 panel_height_dip 按同一可见性规则收缩。
        let mut top = FIRST_ROW_TOP_DIP;
        self.draw_quota_row(
            &short_term_label,
            short_term,
            top,
            width,
            opacity,
            state.show_reset_countdown,
        );
        top += ROW_STEP_DIP;
        self.draw_quota_row(
            &long_term_label,
            long_term,
            top,
            width,
            opacity,
            state.show_reset_countdown,
        );
        top += ROW_STEP_DIP;
        self.draw_token_usage_row(
            "今日使用",
            state.today_tokens,
            Some(state.today_cost),
            top,
            width,
            opacity,
            state.token_unit,
        );
        top += ROW_STEP_DIP;
        if today_overflow_visible(state) {
            self.draw_token_usage_row(
                "今日超额",
                state.today_overflow_tokens,
                Some(state.today_overflow_cost),
                top,
                width,
                opacity,
                state.token_unit,
            );
            top += ROW_STEP_DIP;
        }
        self.draw_token_usage_row(
            &period.usage,
            state.current_period_tokens,
            Some(state.current_period_cost),
            top,
            width,
            opacity,
            state.token_unit,
        );
        top += ROW_STEP_DIP;
        if period_overflow_visible(state) {
            self.draw_token_usage_row(
                &period.overflow,
                state.current_period_overflow_tokens,
                Some(state.current_period_overflow_cost),
                top,
                width,
                opacity,
                state.token_unit,
            );
            top += ROW_STEP_DIP;
        }
        self.draw_period_total_value_row(state, &period.estimate, top, width, opacity);
        top += ROW_STEP_DIP;
        self.draw_update_row(state, top, width, opacity);
    }

    fn draw_quota_row(
        &self,
        label: &str,
        window: Option<&QuotaWindow>,
        top: f32,
        width: f32,
        opacity: f32,
        show_countdown: bool,
    ) {
        self.draw_text_with_opacity(
            label,
            &self.body_format,
            &self.brushes.secondary_text,
            D2D_RECT_F {
                left: 18.0,
                top,
                right: 94.0,
                bottom: top + 23.0,
            },
            opacity,
        );
        let Some(window) = window else {
            self.draw_text_with_opacity(
                "--",
                &self.body_format,
                &self.brushes.unknown,
                D2D_RECT_F {
                    left: TIME_COLUMN_LEFT,
                    top,
                    right: width - RESET_COLUMN_RIGHT_PADDING,
                    bottom: top + 23.0,
                },
                opacity,
            );
            return;
        };
        self.draw_text_with_opacity(
            &format!("{:.0}%", window.remaining_percent()),
            &self.body_format,
            self.brush_for(window.color()),
            D2D_RECT_F {
                left: 96.0,
                top,
                right: 140.0,
                bottom: top + 23.0,
            },
            opacity,
        );
        self.draw_text_with_opacity(
            &format_reset_column(window, SystemTime::now(), show_countdown),
            &self.body_format,
            &self.brushes.secondary_text,
            D2D_RECT_F {
                left: TIME_COLUMN_LEFT,
                top,
                right: width - RESET_COLUMN_RIGHT_PADDING,
                bottom: top + 23.0,
            },
            opacity,
        );
    }

    fn draw_update_row(&self, state: &AppState, top: f32, width: f32, opacity: f32) {
        self.draw_text_with_opacity(
            "更新时间",
            &self.body_format,
            &self.brushes.secondary_text,
            D2D_RECT_F {
                left: 18.0,
                top,
                right: 94.0,
                bottom: top + 23.0,
            },
            opacity,
        );
        let now = SystemTime::now();
        let value = state.snapshot.as_ref().map_or_else(
            || "--".to_owned(),
            |snapshot| {
                let timestamp = format_local_timestamp(snapshot.received_at);
                if state.is_stale(now) {
                    format!("{timestamp} · 已过期")
                } else {
                    timestamp
                }
            },
        );
        self.draw_text_with_opacity(
            &value,
            &self.body_format,
            if state.is_stale(now) {
                &self.brushes.warning
            } else {
                &self.brushes.secondary_text
            },
            D2D_RECT_F {
                left: TIME_COLUMN_LEFT,
                top,
                right: width - 18.0,
                bottom: top + 23.0,
            },
            opacity,
        );
    }

    /// `cost` 外层 `None` 表示该行没有价值列（累计行，tokens 占满宽度）；
    /// 内层 `None` 表示价值未知（价值列显示 `--`，同额度行的未知态）。
    /// 有价值列时布局与额度行对齐：label | $价值(96→140) | tokens(140→)。
    #[allow(clippy::option_option)] // 三态（无价值列 / 未知 / 有值）用双 Option 语义最直接。
    #[allow(clippy::too_many_arguments)]
    fn draw_token_usage_row(
        &self,
        label: &str,
        tokens: Option<u64>,
        cost: Option<Option<f64>>,
        top: f32,
        width: f32,
        opacity: f32,
        unit: UnitStyle,
    ) {
        let label_right = if cost.is_some() {
            94.0
        } else {
            TIME_COLUMN_LEFT
        };
        self.draw_text_with_opacity(
            label,
            &self.body_format,
            &self.brushes.secondary_text,
            D2D_RECT_F {
                left: 18.0,
                top,
                right: label_right,
                bottom: top + 23.0,
            },
            opacity,
        );
        if let Some(cost) = cost {
            // 价值列与额度百分比同色（健康色），视觉上与额度行成组。
            self.draw_text_with_opacity(
                &format_usd(cost),
                &self.body_format,
                if cost.is_some() {
                    &self.brushes.healthy
                } else {
                    &self.brushes.unknown
                },
                D2D_RECT_F {
                    left: 96.0,
                    top,
                    right: 140.0,
                    bottom: top + 23.0,
                },
                opacity,
            );
        }
        self.draw_text_with_opacity(
            &format_token_usage(tokens, unit),
            &self.body_format,
            if tokens.is_some() {
                &self.brushes.secondary_text
            } else {
                &self.brushes.unknown
            },
            D2D_RECT_F {
                left: TIME_COLUMN_LEFT,
                top,
                right: width - 18.0,
                bottom: top + 23.0,
            },
            opacity,
        );
    }

    /// 期间估值行：账号侧估算（健康色美元，96→140 列）+ 满额 token 估算（灰字，
    /// token 列位置）。两个数值按约定不带文字标注，靠颜色深浅区分。
    /// `label` 是已经拼好的行名（`本周估值`），由 `presentation` 统一生成。
    fn draw_period_total_value_row(
        &self,
        state: &AppState,
        label: &str,
        top: f32,
        width: f32,
        opacity: f32,
    ) {
        let now = SystemTime::now();
        let estimate = display_period_total_value(state, now);
        self.draw_text_with_opacity(
            label,
            &self.body_format,
            &self.brushes.secondary_text,
            D2D_RECT_F {
                left: 18.0,
                top,
                right: 94.0,
                bottom: top + 23.0,
            },
            opacity,
        );
        self.draw_text_with_opacity(
            &format_usd(estimate),
            &self.body_format,
            if estimate.is_some() {
                &self.brushes.healthy
            } else {
                &self.brushes.unknown
            },
            D2D_RECT_F {
                left: 96.0,
                top,
                right: 140.0,
                bottom: top + 23.0,
            },
            opacity,
        );
        // 灰字列与别的行的 token 列同位同款（同为「多少 token」），所以宽度也
        // 走同一条 `TIME_COLUMN_LEFT → width - 18`，而不是原来给 `$120` 那种短
        // 文本留的窄矩形。
        let tokens = display_period_total_token_estimate(state, now);
        self.draw_text_with_opacity(
            &format_token_usage(tokens, state.token_unit),
            &self.body_format,
            if tokens.is_some() {
                &self.brushes.secondary_text
            } else {
                &self.brushes.unknown
            },
            D2D_RECT_F {
                left: TIME_COLUMN_LEFT,
                top,
                right: width - 18.0,
                bottom: top + 23.0,
            },
            opacity,
        );
    }

    fn draw_progress_arc(
        &self,
        center: Vector2,
        radius: f32,
        fraction: f64,
        brush: &ID2D1SolidColorBrush,
    ) -> Result<(), AppError> {
        self.draw_arc(center, radius, RING_STROKE_DIP, 0.0, fraction, brush)
    }

    /// 从 `from` 起顺时针画 `sweep` 比例的一段弧（比例是整圈的占比，起点在 12 点）。
    ///
    /// 额度弧的 `from` 恒为 0，不确定进度弧靠 `from` 转动——两者共用同一段
    /// 几何代码，所以描边宽度、圆角端点与抗锯齿表现完全一致。
    #[allow(clippy::too_many_arguments)]
    fn draw_arc(
        &self,
        center: Vector2,
        radius: f32,
        stroke: f32,
        from: f64,
        sweep: f64,
        brush: &ID2D1SolidColorBrush,
    ) -> Result<(), AppError> {
        if sweep >= 0.999 {
            let ellipse = D2D1_ELLIPSE {
                point: center,
                radiusX: radius,
                radiusY: radius,
            };
            // SAFETY: immediate draw with valid geometry and renderer-owned brush.
            unsafe {
                self.target
                    .DrawEllipse(&ellipse, brush, stroke, Some(&self.stroke_style));
            }
            return Ok(());
        }

        let start_angle = std::f64::consts::TAU * from;
        let end_angle = std::f64::consts::TAU * (from + sweep);
        let start = Vector2 {
            X: center.X + (start_angle.sin() as f32 * radius),
            Y: center.Y - (start_angle.cos() as f32 * radius),
        };
        let end = Vector2 {
            X: center.X + (end_angle.sin() as f32 * radius),
            Y: center.Y - (end_angle.cos() as f32 * radius),
        };
        // SAFETY: factory, geometry, and sink are UI-thread COM objects. The sink is closed before
        // the geometry is drawn, and all stack geometry data outlives its immediate COM call.
        unsafe {
            let geometry = self.factory.CreatePathGeometry()?;
            let sink = geometry.Open()?;
            sink.BeginFigure(start, D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: end,
                size: D2D_SIZE_F {
                    width: radius,
                    height: radius,
                },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: if sweep > 0.5 {
                    D2D1_ARC_SIZE_LARGE
                } else {
                    D2D1_ARC_SIZE_SMALL
                },
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            sink.Close()?;
            self.target
                .DrawGeometry(&geometry, brush, stroke, Some(&self.stroke_style));
        }
        Ok(())
    }

    fn draw_text_with_opacity(
        &self,
        text: &str,
        format: &IDWriteTextFormat,
        brush: &ID2D1SolidColorBrush,
        bounds: D2D_RECT_F,
        opacity: f32,
    ) {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: UTF-16 storage and layout rectangle remain alive for the synchronous draw call;
        // the renderer-owned brush opacity is restored immediately afterward.
        unsafe {
            brush.SetOpacity(opacity.clamp(0.0, 1.0));
            self.target.DrawText(
                &utf16,
                format,
                &bounds,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
            brush.SetOpacity(1.0);
        }
    }

    fn brush_for(&self, color: QuotaColor) -> &ID2D1SolidColorBrush {
        match color {
            QuotaColor::Healthy => &self.brushes.healthy,
            QuotaColor::Warning => &self.brushes.warning,
            QuotaColor::Critical => &self.brushes.critical,
            QuotaColor::Unknown => &self.brushes.unknown,
        }
    }

    fn brush_for_plan_type(&self, plan_type: &str) -> &ID2D1SolidColorBrush {
        match plan_type_color(plan_type) {
            PlanColor::Neutral => &self.brushes.plan_neutral,
            PlanColor::Plus => &self.brushes.plan_plus,
            PlanColor::Pro => &self.brushes.plan_pro,
            PlanColor::Business => &self.brushes.plan_business,
            PlanColor::Enterprise => &self.brushes.plan_enterprise,
            PlanColor::Edu => &self.brushes.plan_edu,
            PlanColor::Unknown => &self.brushes.unknown,
        }
    }
}

fn create_target(factory: &ID2D1Factory, dpi: u32) -> Result<ID2D1DCRenderTarget, AppError> {
    let properties = D2D1_RENDER_TARGET_PROPERTIES {
        // This surface is tiny and updated only during short transitions. A software DC target
        // avoids loading a vendor D3D driver stack whose idle memory dwarfs the bitmap itself.
        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: dpi as f32,
        dpiY: dpi as f32,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    // SAFETY: properties is valid for the duration of the factory call.
    unsafe { factory.CreateDCRenderTarget(&properties) }.map_err(AppError::from)
}

/// 圆角端点的描边样式：弧的两端是半圆而不是平口。
///
/// 短弧最能看出差别——剩余 2% 时平口端点是一条几乎看不见的细刺，圆角端点是一颗
/// 小圆点；长弧两端也一样，平口会让"余量"在视觉上被削掉一截。额度弧与不确定
/// 进度弧共用这一份样式，两处的端头形状因此必然一致。
fn create_round_stroke_style(factory: &ID2D1Factory) -> Result<ID2D1StrokeStyle, AppError> {
    let properties = D2D1_STROKE_STYLE_PROPERTIES {
        startCap: D2D1_CAP_STYLE_ROUND,
        endCap: D2D1_CAP_STYLE_ROUND,
        dashCap: D2D1_CAP_STYLE_ROUND,
        lineJoin: D2D1_LINE_JOIN_ROUND,
        // 实线样式下 miterLimit 不参与，写上 D2D 文档的默认值以免依赖 0.0 的巧合。
        miterLimit: 10.0,
        dashStyle: D2D1_DASH_STYLE_SOLID,
        dashOffset: 0.0,
    };
    // SAFETY: properties is a plain data struct and the factory stays alive for this call.
    let style = unsafe { factory.CreateStrokeStyle(&properties, None) }?;
    Ok(style)
}

fn create_brushes(target: &ID2D1DCRenderTarget, style: ColorStyle) -> Result<Brushes, AppError> {
    let palette = Palette::of(style);
    let render_target: ID2D1RenderTarget = target.cast()?;
    let brush = |color: &D2D1_COLOR_F| {
        // SAFETY: color is borrowed only for this synchronous factory call.
        unsafe { render_target.CreateSolidColorBrush(color, None) }
    };
    Ok(Brushes {
        background: brush(&BACKGROUND)?,
        background_hover: brush(&BACKGROUND_HOVER)?,
        track: brush(&TRACK)?,
        track_hover: brush(&TRACK_HOVER)?,
        spinner: brush(&SPINNER)?,
        primary_text: brush(&PRIMARY_TEXT)?,
        secondary_text: brush(&SECONDARY_TEXT)?,
        healthy: brush(&palette.healthy)?,
        warning: brush(&palette.warning)?,
        critical: brush(&palette.critical)?,
        unknown: brush(&UNKNOWN)?,
        plan_neutral: brush(&PLAN_NEUTRAL)?,
        plan_plus: brush(&PLAN_PLUS)?,
        plan_pro: brush(&PLAN_PRO)?,
        plan_business: brush(&PLAN_BUSINESS)?,
        plan_enterprise: brush(&PLAN_ENTERPRISE)?,
        plan_edu: brush(&PLAN_EDU)?,
    })
}

fn create_text_format(
    factory: &IDWriteFactory,
    size: f32,
    centered: bool,
) -> Result<IDWriteTextFormat, AppError> {
    let variable = wide("Segoe UI Variable");
    let fallback = wide("Segoe UI");
    let locale = wide("zh-CN");
    // SAFETY: all PCWSTR values reference NUL-terminated vectors that live through each call.
    let format = unsafe {
        factory.CreateTextFormat(
            PCWSTR(variable.as_ptr()),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            PCWSTR(locale.as_ptr()),
        )
    }
    .or_else(|_| unsafe {
        factory.CreateTextFormat(
            PCWSTR(fallback.as_ptr()),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            PCWSTR(locale.as_ptr()),
        )
    })?;
    // SAFETY: alignment setters mutate only this format COM object on the UI thread.
    unsafe {
        format.SetTextAlignment(if centered {
            DWRITE_TEXT_ALIGNMENT_CENTER
        } else {
            DWRITE_TEXT_ALIGNMENT_LEADING
        })?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    }
    Ok(format)
}

impl DibSurface {
    fn new(width: i32, height: i32) -> Result<Self, AppError> {
        // SAFETY: a memory DC has no borrowed window lifetime and is released in Drop.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return Err(AppError::Windows("无法创建内存绘图上下文".to_owned()));
        }
        let mut bits = ptr::null_mut::<c_void>();
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>())
                    .map_err(|_| AppError::Render("位图头大小溢出".to_owned()))?,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: info and output pointer are valid. No section handle is supplied; GDI owns the
        // allocation until bitmap deletion. The returned bits pointer remains valid while selected.
        let bitmap =
            unsafe { CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) }?;
        if bits.is_null() {
            // SAFETY: these objects were successfully created by this function and are not shared.
            unsafe {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                let _ = DeleteDC(dc);
            }
            return Err(AppError::Render("无法映射分层窗口位图".to_owned()));
        }
        // SAFETY: bitmap is a valid GDI bitmap and dc is a compatible memory DC.
        let previous = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
        Ok(Self {
            dc,
            bitmap,
            previous,
            bits: bits.cast(),
            width,
            height,
        })
    }

    fn byte_len(&self) -> usize {
        usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .map(|height| width * height * 4)
            })
            .unwrap_or(0)
    }

    fn clear(&self) {
        // SAFETY: bits points to this surface's byte_len()-byte DIB allocation.
        unsafe { ptr::write_bytes(self.bits, 0, self.byte_len()) };
    }

    fn commit(&self, hwnd: HWND, destination: POINT) -> Result<(), AppError> {
        let size = SIZE {
            cx: self.width,
            cy: self.height,
        };
        let source = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: the source DC contains a selected 32-bit premultiplied BGRA DIB of the stated size.
        unsafe {
            UpdateLayeredWindow(
                hwnd,
                None,
                Some(&destination),
                Some(&size),
                Some(self.dc),
                Some(&source),
                Default::default(),
                Some(&blend),
                ULW_ALPHA,
            )
        }?;
        Ok(())
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        // SAFETY: the old object is restored before deleting the selected bitmap and its memory DC.
        unsafe {
            let _ = SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant, SystemTime};

    use super::{AppState, PointerState, Renderer, RingState, VisualState};
    use crate::config::ColorStyle;
    use crate::quota::{QuotaPull, QuotaSnapshot, QuotaWindow};
    use crate::win32::layout::{
        HOVER_TWEEN, PRESS_TWEEN, PULSE_MIN_OPACITY, PULSE_PERIOD, RELEASE_TWEEN,
        RING_PULSE_FRAME_MILLIS, RING_SPIN_FRAME_MILLIS, RING_TWEEN_FRAME_MILLIS, SPIN_PERIOD,
        Tween, pulse_opacity,
    };

    #[test]
    fn first_known_value_sweeps_in_from_zero() {
        let start = Instant::now();
        let mut ring = RingState::new(start);

        ring.retarget(Some(0.4), start);

        // 球刚出现：从 0 扫入，而不是直接落在目标值上。
        assert_eq!(ring.fraction(start), Some(0.0));
        assert_eq!(ring.frame_interval(start), Some(RING_TWEEN_FRAME_MILLIS));

        let swept = start + Tween::SWEEP_DURATION;
        assert!((ring.fraction(swept).unwrap_or_default() - 0.4).abs() < 1e-9);
        // 扫入结束后没有脉冲，就不再需要帧定时器。
        assert_eq!(ring.frame_interval(swept), None);
    }

    #[test]
    fn value_reappearing_after_unknown_snaps_without_a_sweep() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.4), start);
        let swept = start + Tween::SWEEP_DURATION;
        assert!((ring.fraction(swept).unwrap_or_default() - 0.4).abs() < 1e-9);

        // 数据变得不可知（例如快照过期），随后又恢复。
        ring.retarget(None, swept);
        let restored = swept + Duration::from_secs(1);
        ring.retarget(Some(0.3), restored);

        // 球一直在，弧只是恢复了数据：直接落在当前比例，不做扫入。
        assert_eq!(ring.fraction(restored), Some(0.3));
        assert_eq!(ring.frame_interval(restored), None);
    }

    #[test]
    fn appearance_makes_the_next_value_sweep_again() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.4), start);
        let swept = start + Tween::SWEEP_DURATION;
        assert!((ring.fraction(swept).unwrap_or_default() - 0.4).abs() < 1e-9);

        ring.expect_appearance();
        ring.retarget(Some(0.4), swept);

        assert_eq!(ring.fraction(swept), Some(0.0), "重新出现应再次从 0 扫入");
        let reswept = swept + Tween::SWEEP_DURATION;
        assert!((ring.fraction(reswept).unwrap_or_default() - 0.4).abs() < 1e-9);
    }

    #[test]
    fn disabled_animations_also_skip_the_appearance_sweep() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.set_enabled(false);

        ring.retarget(Some(0.4), start);

        assert_eq!(ring.fraction(start), Some(0.4), "系统关闭动画时直接是终值");
        assert_eq!(ring.frame_interval(start), None);
    }

    #[test]
    fn changed_value_tweens_and_stops_asking_for_frames_when_settled() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.4), start);
        // 先让"球刚出现"的扫入跑完，再改值。
        let swept = start + Tween::SWEEP_DURATION;
        assert!((ring.fraction(swept).unwrap_or_default() - 0.4).abs() < 1e-9);

        ring.retarget(Some(0.1), swept);

        let first = ring.fraction(swept).unwrap_or_default();
        assert!((first - 0.4).abs() < 1e-9, "补间应从旧值起步：{first}");
        assert_eq!(ring.frame_interval(swept), Some(RING_TWEEN_FRAME_MILLIS));

        let settled = swept + Tween::DURATION;
        let value = ring.fraction(settled).unwrap_or_default();
        assert!((value - 0.1).abs() < 1e-9);
        assert_eq!(ring.frame_interval(settled), None);
    }

    #[test]
    fn retargeting_mid_tween_restarts_from_the_interpolated_value() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.5), start);
        let swept = start + Tween::SWEEP_DURATION;
        assert!((ring.fraction(swept).unwrap_or_default() - 0.5).abs() < 1e-9);
        // 值变化才起补间：0.5 → 0.1。
        ring.retarget(Some(0.1), swept);
        let middle = swept + Tween::DURATION / 2;
        let interpolated = ring.fraction(middle).unwrap_or_default();
        assert!(
            interpolated < 0.5 && interpolated > 0.1,
            "补间中点应落在两端之间：{interpolated}"
        );

        ring.retarget(Some(0.9), middle);

        let restarted = ring.fraction(middle).unwrap_or_default();
        assert!(
            (restarted - interpolated).abs() < 1e-9,
            "补间中途换目标不得跳变：{interpolated} → {restarted}"
        );
        let settled = ring.fraction(middle + Tween::DURATION).unwrap_or_default();
        assert!((settled - 0.9).abs() < 1e-9, "新目标应能跑到位：{settled}");
    }

    #[test]
    fn unknown_or_zero_target_drops_the_arc_without_tweening() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.3), start);
        let later = start + Duration::from_millis(10);

        ring.retarget(None, later);

        assert_eq!(ring.fraction(later), None);
        assert_eq!(ring.frame_interval(later), None);
    }

    #[test]
    fn disabling_animations_snaps_to_the_target() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.6), start);
        let later = start + Duration::from_millis(20);
        ring.retarget(Some(0.2), later);

        ring.set_enabled(false);

        assert_eq!(ring.fraction(later), Some(0.2), "关闭动效后直接是终值");
        assert_eq!(ring.frame_interval(later), None);
        assert!((ring.pulse_factor(later, true) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn pulse_runs_only_while_enabled_and_requested() {
        let start = Instant::now();
        let mut ring = RingState::new(start);

        assert!((ring.pulse_factor(start, false) - 1.0).abs() < f32::EPSILON);
        assert_eq!(ring.frame_interval(start), None);

        assert!((ring.pulse_factor(start, true) - 1.0).abs() < 1e-6);
        assert_eq!(ring.frame_interval(start), Some(RING_PULSE_FRAME_MILLIS));

        let dim = ring.pulse_factor(start + PULSE_PERIOD / 2, true);
        assert!((dim - PULSE_MIN_OPACITY).abs() < 1e-6);

        ring.set_enabled(false);
        assert!((ring.pulse_factor(start, true) - 1.0).abs() < f32::EPSILON);
        assert_eq!(ring.frame_interval(start), None);
    }

    #[test]
    fn label_rolls_on_its_own_shorter_duration_while_the_arc_keeps_sweeping() {
        let start = Instant::now();
        let mut ring = RingState::new(start);

        ring.retarget(Some(0.5), start);

        // 数字与弧同时从 0 起步。
        assert_eq!(ring.label_fraction(start), Some(0.0));
        // 数字先落定：滚动结束后不再出值，交给静态标签；弧此时还在扫。
        let rolled = start + Tween::LABEL_SWEEP;
        assert_eq!(ring.label_fraction(rolled), None);
        let arc = ring.fraction(rolled).unwrap_or_default();
        assert!(arc > 0.0 && arc < 0.5, "弧应仍在扫入中：{arc}");
        assert_eq!(ring.frame_interval(rolled), Some(RING_TWEEN_FRAME_MILLIS));

        let settled = start + Tween::SWEEP_DURATION;
        assert!((ring.fraction(settled).unwrap_or_default() - 0.5).abs() < 1e-9);
        assert_eq!(ring.frame_interval(settled), None);
    }

    #[test]
    fn label_rolls_from_the_value_it_currently_shows_when_the_target_changes() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.5), start);
        let swept = start + Tween::SWEEP_DURATION;
        assert!((ring.fraction(swept).unwrap_or_default() - 0.5).abs() < 1e-9);

        ring.retarget(Some(0.1), swept);

        assert_eq!(ring.label_fraction(swept), Some(0.5), "从当前显示值起步");
        // 数字用更短的时长先落定，此刻弧还在补间。
        let rolled = swept + Tween::LABEL_TWEEN;
        assert_eq!(ring.label_fraction(rolled), None);
        let arc = ring.fraction(rolled).unwrap_or_default();
        assert!(arc < 0.5 && arc > 0.1, "弧应仍在补间中：{arc}");
        assert_eq!(ring.frame_interval(rolled), Some(RING_TWEEN_FRAME_MILLIS));

        // 弧落定后必须停止请求帧——数字比弧长会把这条测试打红。
        let settled = swept + Tween::DURATION;
        assert!((ring.fraction(settled).unwrap_or_default() - 0.1).abs() < 1e-9);
        assert_eq!(ring.frame_interval(settled), None);
    }

    #[test]
    fn label_does_not_roll_when_the_target_becomes_unknown() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.4), start);
        let swept = start + Tween::SWEEP_DURATION;

        ring.retarget(None, swept);

        assert_eq!(ring.label_fraction(swept), None, "未知态交给静态标签 --");
        assert_eq!(ring.frame_interval(swept), None);
    }

    #[test]
    fn disabling_animations_stops_the_label_roll_too() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.5), start);

        ring.set_enabled(false);

        assert_eq!(ring.label_fraction(start), None);
        assert_eq!(ring.fraction(start), Some(0.5), "弧也应直接是终值");
    }

    #[test]
    fn pulse_phase_survives_long_running_reanchoring() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        // 约 16 天之后（1e6 个周期）相位必须与刚起步的四分之一周期一致：
        // 不重新锚定的话 f32 秒数在这个量级只剩约 0.1 秒分辨率。
        let late = start + PULSE_PERIOD * 1_000_000 + PULSE_PERIOD / 4;

        let value = ring.pulse_factor(late, true);

        assert!(
            (value - pulse_opacity(PULSE_PERIOD / 4)).abs() < 1e-6,
            "长时间运行后相位漂移：{value}"
        );
    }

    #[test]
    fn tween_frame_rate_wins_while_a_tween_and_pulse_overlap() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.2), start);
        let later = start + Duration::from_millis(10);
        ring.retarget(Some(0.05), later);

        // 相位起点后 10ms 仍接近最亮（脉冲从最亮处起步）。
        assert!(ring.pulse_factor(later, true) > 0.99);
        assert_eq!(ring.frame_interval(later), Some(RING_TWEEN_FRAME_MILLIS));
    }

    #[test]
    fn hover_tween_fades_in_and_stops_asking_for_frames() {
        let start = Instant::now();
        let mut pointer = PointerState::new();

        pointer.set_targets(true, false, start);
        assert!(pointer.sample(start).0.abs() < f32::EPSILON);
        assert!(pointer.tweening(), "补间期间应当要求出帧");

        let middle = pointer.sample(start + HOVER_TWEEN / 2).0;
        assert!(middle > 0.5, "缓出曲线中点应已过半：{middle}");

        let settled = pointer.sample(start + HOVER_TWEEN).0;
        assert!((settled - 1.0).abs() < 1e-6);
        // 稳态不再要求出帧：光标停在球上不动时不该有帧定时器。
        assert!(!pointer.tweening());
    }

    #[test]
    fn hover_tween_fades_out_from_wherever_it_was() {
        let start = Instant::now();
        let mut pointer = PointerState::new();
        pointer.set_targets(true, false, start);
        let halfway = start + HOVER_TWEEN / 2;
        let value = pointer.sample(halfway).0;
        assert!(value > 0.5 && value < 1.0);

        pointer.set_targets(false, false, halfway);

        // 从当前值往回走，而不是先跳到 1.0 再淡出。
        assert!((pointer.sample(halfway).0 - value).abs() < 1e-6);
        assert!(pointer.sample(halfway + HOVER_TWEEN).0.abs() < 1e-6);
    }

    #[test]
    fn press_scales_in_faster_than_it_releases() {
        let start = Instant::now();
        let mut pointer = PointerState::new();

        pointer.set_targets(false, true, start);
        assert!(
            (pointer.sample(start + PRESS_TWEEN).1 - 1.0).abs() < 1e-6,
            "按下要在 PRESS_TWEEN 内到位"
        );

        let released = start + PRESS_TWEEN;
        pointer.set_targets(false, false, released);

        // 回弹配得更长：走过同样长的按下时长时，还没有回到 0。
        let mid_release = pointer.sample(released + PRESS_TWEEN).1;
        assert!(mid_release > 0.0, "回弹不该和按下一样快：{mid_release}");
        assert!(pointer.sample(released + RELEASE_TWEEN).1.abs() < 1e-6);
    }

    #[test]
    fn disabled_animations_jump_the_pointer_state_to_its_target() {
        let start = Instant::now();
        let mut pointer = PointerState::new();
        pointer.set_enabled(false);

        pointer.set_targets(true, true, start);

        // 没有动画，但状态照样指示出来——系统关的是"动画"，不是"提示"。
        let (hover, press) = pointer.sample(start);
        assert!((hover - 1.0).abs() < f32::EPSILON);
        assert!((press - 1.0).abs() < f32::EPSILON);
        assert!(!pointer.tweening());
    }

    #[test]
    fn spinner_rotates_while_pulling_and_stops_asking_for_frames() {
        let start = Instant::now();
        let mut ring = RingState::new(start);

        // 不在拉取：没有旋转弧，也没有帧。
        assert_eq!(ring.spin_phase(start, false), None);
        assert_eq!(ring.frame_interval(start), None);

        let phase = ring.spin_phase(start, true).expect("拉取时应当有旋转弧");
        assert!(phase.abs() < 1e-6, "旋转弧从 12 点起步");
        assert_eq!(ring.frame_interval(start), Some(RING_SPIN_FRAME_MILLIS));

        let half = ring
            .spin_phase(start + SPIN_PERIOD / 2, true)
            .expect("仍在拉取");
        assert!((half - 0.5).abs() < 1e-6, "半个周期后应转过半圈：{half}");

        // 拉取结束：旋转弧消失，帧定时器随之停表。
        assert_eq!(ring.spin_phase(start + SPIN_PERIOD, false), None);
        assert_eq!(ring.frame_interval(start + SPIN_PERIOD), None);
    }

    /// 手动刷新时环被清空：球上只剩"正在刷新"这一件事，不再同时出现一条
    /// 说"这是当前读数"的额度弧。
    #[test]
    fn manual_refresh_paints_no_quota_arc_on_the_ring() {
        use crate::win32::layout::dip_to_px;

        let now = SystemTime::now();
        let state = AppState {
            snapshot: Some(QuotaSnapshot {
                limit_id: "codex".to_owned(),
                primary: QuotaWindow {
                    used_percent: 38.0,
                    window_duration: Duration::from_hours(5),
                    resets_at: now + Duration::from_hours(4),
                },
                secondary: None,
                received_at: now,
            }),
            ..AppState::default()
        };
        let center = dip_to_px(crate::win32::COLLAPSED_DIP, 96) / 2;
        let radius = dip_to_px(super::RING_RADIUS_DIP, 96);
        // 3 点方向：剩余 62% 的额度弧一定扫过这里。
        let sample = |state: &AppState| {
            let (pixels, side) = ball_pixels(state, false, false);
            pixel_at(&pixels, side, center + radius, center)
        };

        let idle = sample(&state);
        assert!(
            idle[1] > 0xa0 && idle[1] > idle[0] && idle[1] > idle[2],
            "有值时这里是绿色的额度弧：{idle:?}"
        );

        let forced = sample(&AppState {
            quota_pull: QuotaPull::Forced,
            ..state.clone()
        });
        assert!(
            forced[1] < 0x60 && forced[1] < idle[1],
            "刷新中这里应只剩中性轨道：{forced:?}"
        );
    }

    /// 0% 不呼吸：它是冻结状态，动效不携带新信息，而且**没有帧**——应用因此
    /// 可以在卡死期间彻底停表，这正是它停留最久的状态。
    #[test]
    fn exhausted_never_pulses_and_never_asks_for_frames() {
        let start = Instant::now();
        let mut ring = RingState::new(start);

        for offset in [
            Duration::ZERO,
            PULSE_PERIOD / 2,
            PULSE_PERIOD,
            Duration::from_hours(6),
        ] {
            let now = start + offset;
            assert!((ring.pulse_factor(now, false) - 1.0).abs() < f32::EPSILON);
            assert_eq!(ring.frame_interval(now), None);
        }
    }

    /// 离屏 D2D 绘制。这是全仓库唯一依赖真实 GDI/D2D 的测试，需要能创建内存 DC
    /// 的桌面会话；断言只取不变量（尺寸、透明度、预乘关系），不比对具体文字像素，
    /// 避免随字体与 DPI 漂移。
    fn offscreen_panel_renderer() -> (Renderer, AppState, i32, i32) {
        use crate::win32::PANEL_WIDTH_DIP;
        use crate::win32::layout::dip_to_px;
        use crate::win32::presentation::panel_height_dip;

        let state = AppState::default();
        let width = dip_to_px(PANEL_WIDTH_DIP, 96);
        let height = dip_to_px(panel_height_dip(&state), 96);
        let renderer = Renderer::new(width, height, 96).expect("创建离屏渲染器");
        (renderer, state, width, height)
    }

    fn pixel_at(pixels: &[u8], width: i32, x: i32, y: i32) -> [u8; 4] {
        let index = usize::try_from((y * width + x) * 4).unwrap();
        pixels[index..index + 4].try_into().unwrap()
    }

    #[test]
    fn expanded_draw_keeps_transparent_corners() {
        let (mut renderer, state, width, height) = offscreen_panel_renderer();
        renderer
            .draw(VisualState::Expanded, &state)
            .expect("绘制展开面板");
        let pixels = renderer.copy_pixels();

        for (x, y) in [
            (0, 0),
            (width - 1, 0),
            (0, height - 1),
            (width - 1, height - 1),
        ] {
            assert_eq!(
                pixel_at(&pixels, width, x, y)[3],
                0,
                "({x},{y}) 落在圆角之外，应当全透明"
            );
        }
        let background = pixel_at(&pixels, width, width / 2, 1)[3];
        assert!(background > 0, "面板顶边中点应当有背景");
        assert!(background < 255, "悬浮窗背景是半透明的，不该出现不透明像素");

        for pixel in pixels.chunks_exact(4) {
            assert!(
                pixel[0] <= pixel[3] && pixel[1] <= pixel[3] && pixel[2] <= pixel[3],
                "像素通道超过了 alpha，说明缓冲不是预乘的"
            );
        }
    }

    #[test]
    fn panel_snapshot_is_opaque_without_rounded_corners() {
        let (mut renderer, state, width, height) = offscreen_panel_renderer();
        let bitmap = renderer.panel_snapshot(&state).expect("渲染面板");

        assert_eq!((bitmap.width, bitmap.height), (width, height));
        assert_eq!(
            bitmap.pixels.len(),
            usize::try_from(width * height * 4).unwrap()
        );

        const OPAQUE_BACKGROUND: [u8; 4] = [
            super::BACKGROUND_RGB.2,
            super::BACKGROUND_RGB.1,
            super::BACKGROUND_RGB.0,
            255,
        ];
        for (x, y) in [
            (0, 0),
            (width - 1, 0),
            (0, height - 1),
            (width - 1, height - 1),
        ] {
            assert_eq!(
                pixel_at(&bitmap.pixels, width, x, y),
                OPAQUE_BACKGROUND,
                "({x},{y}) 截图四角应铺成不透明面板色"
            );
        }
        assert_eq!(
            pixel_at(&bitmap.pixels, width, width / 2, 1),
            OPAQUE_BACKGROUND,
            "截图顶边中点应是不透明面板色，不再留半透明内缩"
        );
        assert!(
            bitmap.pixels.chunks_exact(4).all(|pixel| pixel[3] == 255),
            "截图每一像素都应当不透明"
        );
        assert!(
            bitmap
                .pixels
                .chunks_exact(4)
                .any(|pixel| pixel != OPAQUE_BACKGROUND),
            "截图应当含有文字等内容，不能整张都是纯底"
        );
    }

    /// 任一额度窗口耗尽的快照：`ball_quota` 读 0，且因为账号不可服务而恒为 Critical。
    fn exhausted_state() -> AppState {
        let now = SystemTime::now();
        AppState {
            snapshot: Some(QuotaSnapshot {
                limit_id: "codex".to_owned(),
                primary: QuotaWindow {
                    used_percent: 100.0,
                    window_duration: Duration::from_hours(5),
                    resets_at: now + Duration::from_hours(4),
                },
                secondary: None,
                received_at: now,
            }),
            ..AppState::default()
        }
    }

    /// 球上的一帧：可指定指针态与额度状态，返回球大小的离屏像素。
    ///
    /// 动效**关闭**，于是指针态直接落在目标值上、像素断言不依赖时间；拉取中的
    /// 旋转弧属于动效本身，要用 [`animated_ball_pixels`]。
    fn ball_pixels(state: &AppState, hovered: bool, pressed: bool) -> (Vec<u8>, i32) {
        use crate::win32::COLLAPSED_DIP;
        use crate::win32::layout::dip_to_px;

        let side = dip_to_px(COLLAPSED_DIP, 96);
        let mut renderer = Renderer::new(side, side, 96).expect("创建离屏渲染器");
        renderer.set_animations_enabled(false);
        renderer.set_pointer_targets(hovered, pressed);
        renderer
            .draw(VisualState::Collapsed, state)
            .expect("绘制悬浮球");
        (renderer.copy_pixels(), side)
    }

    /// 动效开启的一帧（无指针态）：只能靠动画表达的像素断言用它。
    fn animated_ball_pixels(state: &AppState) -> (Vec<u8>, i32) {
        use crate::win32::COLLAPSED_DIP;
        use crate::win32::layout::dip_to_px;

        let side = dip_to_px(COLLAPSED_DIP, 96);
        let mut renderer = Renderer::new(side, side, 96).expect("创建离屏渲染器");
        renderer
            .draw(VisualState::Collapsed, state)
            .expect("绘制悬浮球");
        (renderer.copy_pixels(), side)
    }

    /// 重置列最长的字符串必须真的放得下——超出会被 `DrawText` 的矩形裁掉。
    ///
    /// 这里用 DirectWrite 实测宽度，而不是估算：倒计时的括号部分是新加的、
    /// 最容易溢出的内容，而列宽只有 122 DIP。`format_usd` 的注释里已经为裁剪
    /// 问题做过一次取舍（≥$100 去小数），这条测试把同类问题钉在宽度上。
    #[test]
    fn reset_column_fits_its_longest_strings() {
        use crate::win32::layout::dip_to_px;
        use crate::win32::presentation::format_reset_column;
        use windows::Win32::Graphics::DirectWrite::{DWRITE_TEXT_METRICS, IDWriteTextLayout};

        let width = dip_to_px(crate::win32::PANEL_WIDTH_DIP, 96);
        let renderer = Renderer::new(width, dip_to_px(227.0, 96), 96).expect("创建离屏渲染器");
        let column = width as f32 - super::RESET_COLUMN_RIGHT_PADDING - super::TIME_COLUMN_LEFT;
        let measure = |text: &str| {
            let utf16: Vec<u16> = text.encode_utf16().collect();
            // SAFETY: the UTF-16 buffer and format outlive the synchronous layout call.
            let layout: IDWriteTextLayout = unsafe {
                renderer.write_factory.CreateTextLayout(
                    &utf16,
                    &renderer.body_format,
                    f32::MAX,
                    f32::MAX,
                )
            }
            .expect("创建文本布局");
            let mut metrics = DWRITE_TEXT_METRICS::default();
            // SAFETY: metrics is valid writable storage for this synchronous call.
            unsafe { layout.GetMetrics(&mut metrics) }.expect("读取文本度量");
            metrics.width
        };

        // 候选串由真正的格式化路径生成，而不是写死：阶梯规则一改（比如把
        // "≥10 天不再写小时"的门槛调大），这里量的就是新输出。
        let now = SystemTime::now();
        let window = |remaining: Duration| QuotaWindow {
            used_percent: 40.0,
            window_duration: Duration::from_hours(5),
            resets_at: now + remaining,
        };
        let mut candidates = vec![
            format_reset_column(&window(Duration::from_hours(30)), now, true),
            // 贴着上限的那一档：9 天 23 小时。
            format_reset_column(&window(Duration::from_hours(9 * 24 + 23)), now, true),
            // 月窗口的封顶写法。
            format_reset_column(&window(Duration::from_hours(31 * 24 + 23)), now, true),
            format_reset_column(&window(Duration::from_hours(6)), now, true),
            // 小时档带分钟之后的最宽形态：这是全表最紧的一条。
            format_reset_column(&window(Duration::from_mins(23 * 60 + 59)), now, true),
            format_reset_column(&window(Duration::from_mins(6)), now, true),
            format_reset_column(&window(Duration::ZERO), now, true),
            // 关掉倒计时时只有本地时间。
            format_reset_column(&window(Duration::from_hours(24)), now, false),
        ];
        // 防御：这几种情形必须真的覆盖到上面那些形态，否则测试会静默失效。
        assert!(
            candidates.iter().any(|text| text.contains("(9d23h)")),
            "{candidates:?}"
        );
        assert!(
            candidates.iter().any(|text| text.contains("(31d)")),
            "{candidates:?}"
        );
        assert!(
            candidates.iter().any(|text| text.contains("(23h59m)")),
            "小时档带分钟后的宽度是最紧的一条：{candidates:?}"
        );
        candidates.dedup();

        for text in &candidates {
            let measured = measure(text);
            assert!(
                measured <= column,
                "「{text}」实测 {measured:.1} DIP，列宽只有 {column:.1} DIP，会被裁字"
            );
        }

        // 左边紧邻的百分比列也不能被挤压：`100%` 是它的最宽值。
        let percent_column = 140.0 - 96.0;
        let percent = measure("100%");
        assert!(
            percent <= percent_column,
            "「100%」实测 {percent:.1} DIP，百分比列只有 {percent_column:.1} DIP"
        );

        // 标签列（18 → 94）同样要守：窗口名和期间行前缀都随长期窗口的时长变化，
        // `本2周使用` / `本90天超额` 这类是最长的形态。
        let label_column = 94.0 - 18.0;
        let mut labels: Vec<String> = ["今日使用", "今日超额", "更新时间"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        for hours in [5, 24 * 3, 24 * 7, 24 * 14, 24 * 30, 24 * 90] {
            let window = QuotaWindow {
                used_percent: 40.0,
                window_duration: Duration::from_hours(hours),
                resets_at: now + Duration::from_hours(1),
            };
            // 用渲染器真正会画的那些字符串——不是自己重拼一份。
            let period = crate::win32::presentation::period_labels(Some(&window), None);
            labels.push(window.window_label());
            labels.push(period.usage);
            labels.push(period.overflow);
            labels.push(period.estimate);
        }
        for text in &labels {
            let measured = measure(text);
            assert!(
                measured <= label_column,
                "「{text}」实测 {measured:.1} DIP，标签列只有 {label_column:.1} DIP，会被裁字"
            );
        }
    }

    /// 悬停把底色提亮、按下把球缩小：两者都不依赖动画帧，关掉动效也照样指示。
    #[test]
    fn pointer_state_brightens_and_shrinks_the_ball() {
        use crate::win32::layout::dip_to_px;

        let center = dip_to_px(crate::win32::COLLAPSED_DIP, 96) / 2;
        // 圆内一点（半径 18）：在轨道环内侧、中心数字上方，只有底色。
        let inner = |hovered_pressed: (bool, bool)| {
            let (pixels, side) =
                ball_pixels(&AppState::default(), hovered_pressed.0, hovered_pressed.1);
            pixel_at(&pixels, side, center, center - 18)
        };
        let idle = inner((false, false));
        let hovered = inner((true, false));
        assert!(
            hovered[0] > idle[0] && hovered[1] > idle[1] && hovered[2] > idle[2],
            "悬停应把底色整体提亮：{idle:?} → {hovered:?}"
        );

        // 贴着圆边的一点（半径 26~27）：球缩小时它会落到球外。
        let edge = |pressed: bool| {
            let (pixels, side) = ball_pixels(&AppState::default(), false, pressed);
            pixel_at(&pixels, side, center, 1)
        };
        let relaxed = edge(false);
        let pressed = edge(true);
        assert!(relaxed[3] > 200, "未按下时该点应在球内：{relaxed:?}");
        assert_eq!(pressed[3], 0, "按下时球应缩到该点之外：{pressed:?}");
    }

    /// 拉取中的旋转弧画在内圈：不拉取时那个位置是纯底色，拉取时是浅灰弧。
    ///
    /// 旋转弧是动效，动效关闭时它连同别的动画一起消失（`set_animations_enabled`
    /// 会把它关掉），所以这一帧必须开着动效画。
    #[test]
    fn spinner_appears_on_the_inner_ring_only_while_pulling() {
        use crate::win32::layout::dip_to_px;

        let center = dip_to_px(crate::win32::COLLAPSED_DIP, 96) / 2;
        let radius = dip_to_px(super::SPIN_RADIUS_DIP, 96);
        let sample = |state: &AppState| {
            let (pixels, side) = animated_ball_pixels(state);
            pixel_at(&pixels, side, center, center - radius)
        };

        // 已经连上、没在拉取：不画。状态要写明——`AppState::default()` 是
        // 「正在连接」，那是启动阶段，属于"该转"的情形。
        let idle = sample(&AppState {
            status: crate::quota::ConnectionStatus::Online,
            ..AppState::default()
        });
        assert!(
            idle[0] < 0x60 && idle[2] < 0x60,
            "不拉取时这里是底色：{idle:?}"
        );

        // 启动阶段（还没读到第一份额度）就该转：那时球上只有 `--`，
        // 本地日志全量扫描要跑几秒。
        let starting = sample(&AppState::default());
        assert!(
            starting[0] > 0xb0 && starting[1] > 0xb0 && starting[2] > 0xb0,
            "启动阶段应当已经画出旋转弧：{starting:?}"
        );

        // 渲染器创建与绘制的间隔只有几百微秒，相位仍在 12 点附近；采样点就在
        // 弧的起点上（这段弧顺时针扫过 0.28 圈）。
        let pulling = sample(&AppState {
            quota_pull: QuotaPull::Forced,
            ..AppState::default()
        });
        assert!(
            pulling[0] > 0xb0 && pulling[1] > 0xb0 && pulling[2] > 0xb0,
            "拉取中应当画出一段浅灰旋转弧：{pulling:?}"
        );
    }

    /// 弧的两端是圆角：极短的弧看起来是一颗小圆点，而不是一条几乎看不见的细刺。
    ///
    /// 采样点选在弧起点**逆时针**方向 1 dip 处：那里只有圆角端头盖得住（3 dip 描边
    /// 的端头是半径 1.5 dip 的半圆），平口端点就只剩轨道色；再往外 3 dip 处作为对照，
    /// 证明那颗色点确实来自端头。
    #[test]
    fn progress_arc_ends_are_rounded() {
        use crate::win32::layout::dip_to_px;

        let now = SystemTime::now();
        // 剩余 3%：弧很短，端的形状决定它能不能被看见。
        let state = AppState {
            snapshot: Some(QuotaSnapshot {
                limit_id: "codex".to_owned(),
                primary: QuotaWindow {
                    used_percent: 97.0,
                    window_duration: Duration::from_hours(5),
                    resets_at: now + Duration::from_hours(4),
                },
                secondary: None,
                received_at: now,
            }),
            ..AppState::default()
        };
        let (pixels, side) = ball_pixels(&state, false, false);
        let center = dip_to_px(crate::win32::COLLAPSED_DIP, 96) / 2;
        // 弧起点在 12 点：(center, center − RING_RADIUS_DIP)。
        let ring_y = center - dip_to_px(super::RING_RADIUS_DIP, 96);

        // 缓冲是预乘 BGRA：下标 0 是蓝，2 是红。
        let cap = pixel_at(&pixels, side, center - 1, ring_y);
        assert!(
            cap[2] > 0x80 && cap[2] > cap[0],
            "弧起点外侧应被圆角端头盖成额度色：{cap:?}"
        );

        let beyond = pixel_at(&pixels, side, center - 3, ring_y);
        assert!(
            beyond[2] < 0x80 && beyond[0] >= beyond[2],
            "端头之外应还是中性轨道：{beyond:?}"
        );
    }

    /// 卡死（0%）与无数据（`--`）都不画弧、都不脉冲，环也必须画得完全一样：
    /// 这个球的视觉语言只有"弧长 = 余量""整环 = 接近满额"，红色专属给 <20%，
    /// 给 0% 换一只红色整环会让满额与耗尽共用同一个形状。两者的区分只有中心的
    /// `0` / `--`，这条测试锁住这个取舍，免得以后又被"优化"回去。
    #[test]
    fn exhausted_ball_keeps_the_neutral_track() {
        use crate::win32::COLLAPSED_DIP;
        use crate::win32::layout::dip_to_px;

        let side = dip_to_px(COLLAPSED_DIP, 96);
        let ring_y = side / 2 - dip_to_px(super::RING_RADIUS_DIP, 96);
        let sample = |state: &AppState| {
            let mut renderer = Renderer::new(side, side, 96).expect("创建离屏渲染器");
            renderer
                .draw(VisualState::Collapsed, state)
                .expect("绘制悬浮球");
            pixel_at(&renderer.copy_pixels(), side, side / 2, ring_y)
        };

        // 缓冲是预乘 BGRA：下标 0 是蓝，2 是红。
        let blocked = sample(&exhausted_state());
        assert!(blocked[3] > 0, "卡死的轨道仍要画出来");
        assert!(
            blocked[2] < 0x80 && blocked[0] >= blocked[2],
            "卡死的轨道必须是中性暗色，不能是额度色：{blocked:?}"
        );

        let unknown = sample(&AppState::default());
        assert_eq!(blocked, unknown, "0% 与 -- 的环必须一模一样");
    }

    /// 换配色风格必须真的落到像素上，而且只落在额度色上。
    ///
    /// 六个组合（三档状态 × 两套配色）画出来的弧都是不同颜色，说明风格确实从
    /// 状态走到了那三支笔刷；中性球（没有快照，整只球只有轨道、底和文字）则
    /// 一个字节都不许变，说明换配色没有顺手把底色文字也换掉。
    #[test]
    fn color_style_switch_repaints_only_the_quota_colors() {
        use crate::win32::COLLAPSED_DIP;
        use crate::win32::layout::dip_to_px;

        let now = SystemTime::now();
        let side = dip_to_px(COLLAPSED_DIP, 96);
        let center = side / 2;
        // 弧起点在 12 点，三种剩余量下那里都被弧盖住。
        let ring_y = center - dip_to_px(super::RING_RADIUS_DIP, 96);
        let arc_pixel = |used_percent: f64, color_style| {
            let state = AppState {
                snapshot: Some(QuotaSnapshot {
                    limit_id: "codex".to_owned(),
                    primary: QuotaWindow {
                        used_percent,
                        window_duration: Duration::from_hours(5),
                        resets_at: now + Duration::from_hours(4),
                    },
                    secondary: None,
                    received_at: now,
                }),
                color_style,
                ..AppState::default()
            };
            pixel_at(&ball_pixels(&state, false, false).0, side, center, ring_y)
        };

        let mut seen: Vec<[u8; 4]> = Vec::new();
        for (style, name) in [(ColorStyle::Soft, "柔和"), (ColorStyle::Vivid, "鲜艳")] {
            // 剩余 90% / 40% / 5%：健康、警告、紧张各一档。
            for used_percent in [10.0, 60.0, 95.0] {
                let pixel = arc_pixel(used_percent, style);
                assert!(
                    !seen.contains(&pixel),
                    "{name}配色下已用 {used_percent}% 的弧和其它组合撞色：{pixel:?}（已见 {seen:?}）"
                );
                seen.push(pixel);
            }
        }

        let neutral = |color_style| {
            ball_pixels(
                &AppState {
                    color_style,
                    ..AppState::default()
                },
                false,
                false,
            )
            .0
        };
        let soft = neutral(ColorStyle::Soft);
        let vivid = neutral(ColorStyle::Vivid);
        let differing = soft.iter().zip(&vivid).filter(|(a, b)| a != b).count();
        assert_eq!(
            differing, 0,
            "没有额度可言的球只有轨道/底色/文字，换配色不该动它一个像素"
        );
    }
}
