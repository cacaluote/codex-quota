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
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
    D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT, D2D1_SWEEP_DIRECTION_CLOCKWISE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory,
    ID2D1RenderTarget, ID2D1SolidColorBrush,
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
    PULSE_PERIOD, RING_PULSE_FRAME_MILLIS, RING_TWEEN_FRAME_MILLIS, RingAnimation, pulse_opacity,
};
use super::presentation::{
    COMMUNITY_WEEKLY_VALUE_USD, FIRST_ROW_TOP_DIP, PlanColor, ROW_STEP_DIP, ball_quota,
    display_period_total_value, display_windows, format_local_timestamp, format_token_usage,
    format_usd, panel_title, period_overflow_visible, plan_type_color, plan_type_label,
    quota_window_label, today_overflow_visible,
};
use crate::error::AppError;
use crate::quota::{AppState, QuotaColor, QuotaWindow};

const BACKGROUND: D2D1_COLOR_F = rgba(0x12, 0x17, 0x20, 0.94);
const TRACK: D2D1_COLOR_F = rgba(0x42, 0x4a, 0x57, 0.88);
const PRIMARY_TEXT: D2D1_COLOR_F = rgba(0xf2, 0xf5, 0xf8, 1.0);
const SECONDARY_TEXT: D2D1_COLOR_F = rgba(0xa8, 0xb1, 0xbe, 1.0);
const GREEN: D2D1_COLOR_F = rgba(0x45, 0xd4, 0x83, 1.0);
const YELLOW: D2D1_COLOR_F = rgba(0xf2, 0xc9, 0x4c, 1.0);
const RED: D2D1_COLOR_F = rgba(0xf0, 0x66, 0x66, 1.0);
const UNKNOWN: D2D1_COLOR_F = rgba(0x7d, 0x87, 0x95, 1.0);
const PLAN_NEUTRAL: D2D1_COLOR_F = rgba(0x8f, 0x9b, 0xaa, 1.0);
const PLAN_PLUS: D2D1_COLOR_F = rgba(0x5a, 0xa7, 0xff, 1.0);
const PLAN_PRO: D2D1_COLOR_F = rgba(0xa7, 0x8b, 0xfa, 1.0);
const PLAN_BUSINESS: D2D1_COLOR_F = rgba(0x4f, 0xd1, 0xc5, 1.0);
const PLAN_ENTERPRISE: D2D1_COLOR_F = rgba(0xd8, 0xb4, 0x5c, 1.0);
const PLAN_EDU: D2D1_COLOR_F = rgba(0x56, 0xc7, 0xd9, 1.0);
const TRANSPARENT: D2D1_COLOR_F = rgba(0, 0, 0, 0.0);
const TIME_COLUMN_LEFT: f32 = 148.0;
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
    percent_format: IDWriteTextFormat,
    title_format: IDWriteTextFormat,
    body_format: IDWriteTextFormat,
    ring: RingState,
    dpi: u32,
}

struct Brushes {
    background: ID2D1SolidColorBrush,
    track: ID2D1SolidColorBrush,
    primary_text: ID2D1SolidColorBrush,
    secondary_text: ID2D1SolidColorBrush,
    green: ID2D1SolidColorBrush,
    yellow: ID2D1SolidColorBrush,
    red: ID2D1SolidColorBrush,
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

/// 悬浮球百分环的动画状态：额度补间 + 低额度脉冲。
///
/// 状态放在渲染器里，因为动画只在真正画球时推进；UI 层只负责按
/// [`RingState::frame_interval`] 驱动帧定时器，并在不需要帧时停表。
#[derive(Debug)]
struct RingState {
    animation: Option<RingAnimation>,
    /// 中心数字的滚动动画：与弧同时起步，但用更短的时长先落定。
    label_animation: Option<RingAnimation>,
    /// 上一次已知的目标比例；None 表示不可知或为 0（此时不画弧）。
    target: Option<f64>,
    /// 脉冲相位起点：渲染器创建时开始，使球每次出现都从最亮处起步。
    pulse_epoch: Instant,
    /// 是否允许动效：系统动画开关 ∧ 球可见 ∧ 面板未展开 ∧ 未在拖拽。
    enabled: bool,
    /// 上一帧是否有脉冲在跑（决定帧率与是否需要继续出帧）。
    pulsing: bool,
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
            enabled: true,
            pulsing: false,
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
        }
    }

    /// 球重新出现（取消隐藏后再次显示）：忘掉上次的值，让下一个已知值重新扫入。
    fn expect_appearance(&mut self) {
        self.animation = None;
        self.label_animation = None;
        self.target = None;
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
                    Some(RingAnimation::sweep(to, now)),
                    Some(RingAnimation::label_sweep(to, now)),
                )
            }
            // 值变化：以当前插值结果为新起点，补间中途换目标时不会跳变。
            (Some(from), Some(to)) => (
                Some(RingAnimation::new(from, to, now)),
                label_from.map(|from| RingAnimation::label_tween(from, to, now)),
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

    /// 本帧脉冲的不透明度系数；`wanted` 表示当前处于低额度状态。
    fn pulse_factor(&mut self, now: Instant, wanted: bool) -> f32 {
        self.pulsing = wanted && self.enabled;
        if !self.pulsing {
            return 1.0;
        }
        // 相位重新锚定：`now - epoch` 长时间运行后会到 10^6 秒量级，转成 f32
        // 只剩约 0.1 秒分辨率，脉冲会变粗糙。锚定到"当前相位"后精度恒定，
        // 且 epoch 只前进整数个周期，相位本身不变。
        let elapsed = now.saturating_duration_since(self.pulse_epoch);
        // `Duration` 没实现取模，按纳秒取余；余数必然小于一个周期（1.4e9 ns）。
        let phase = Duration::from_nanos((elapsed.as_nanos() % PULSE_PERIOD.as_nanos()) as u64);
        // phase ≤ elapsed = now − epoch，减法不会下溢；退路只是保留原锚点。
        self.pulse_epoch = now.checked_sub(phase).unwrap_or(self.pulse_epoch);
        pulse_opacity(phase)
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
        self.pulsing.then_some(RING_PULSE_FRAME_MILLIS)
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
        let brushes = create_brushes(&target)?;
        let percent_format = create_text_format(&write_factory, PERCENT_FONT_SIZE, true)?;
        let title_format = create_text_format(&write_factory, TITLE_FONT_SIZE, false)?;
        let body_format = create_text_format(&write_factory, BODY_FONT_SIZE, false)?;

        Ok(Self {
            surface,
            factory,
            target,
            write_factory,
            brushes,
            percent_format,
            title_format,
            body_format,
            ring: RingState::new(Instant::now()),
            dpi,
        })
    }

    /// 由 UI 层在每帧绘制前设置：系统动画开关 ∧ 球可见 ∧ 面板未展开 ∧ 未拖拽。
    /// 关闭时会丢弃进行中的补间，避免停在中间帧。
    pub(super) fn set_ring_animation_enabled(&mut self, enabled: bool) {
        self.ring.set_enabled(enabled);
    }

    /// 悬浮球动效还需多少毫秒出一帧；None 表示可以停掉帧定时器。
    pub(super) fn ring_frame_interval(&self) -> Option<u32> {
        self.ring.frame_interval(Instant::now())
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

    fn recreate_device_resources(&mut self, dpi: u32) -> Result<(), AppError> {
        self.target = create_target(&self.factory, dpi)?;
        self.brushes = create_brushes(&self.target)?;
        self.percent_format = create_text_format(&self.write_factory, PERCENT_FONT_SIZE, true)?;
        self.title_format = create_text_format(&self.write_factory, TITLE_FONT_SIZE, false)?;
        self.body_format = create_text_format(&self.write_factory, BODY_FONT_SIZE, false)?;
        self.dpi = dpi;
        Ok(())
    }

    fn draw(&mut self, visual: VisualState, state: &AppState) -> Result<(), AppError> {
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
            self.target.Clear(Some(&TRANSPARENT));
        }

        match visual {
            VisualState::Collapsed => self.draw_ball(state)?,
            VisualState::Expanded => self.draw_panel(state),
            VisualState::Transition(transition) => {
                self.draw_transition(state, transition)?;
            }
        }

        // SAFETY: EndDraw balances BeginDraw above on the same UI thread.
        unsafe { self.target.EndDraw(None, None) }?;
        Ok(())
    }

    fn draw_ball(&mut self, state: &AppState) -> Result<(), AppError> {
        let scale = self.dpi as f32 / 96.0;
        let center = Vector2 {
            X: self.surface.width as f32 / (2.0 * scale),
            Y: self.surface.height as f32 / (2.0 * scale),
        };
        self.draw_ball_background(center);
        self.draw_ball_content(state, center, 1.0)
    }

    fn draw_ball_background(&self, center: Vector2) {
        let circle = D2D1_ELLIPSE {
            point: center,
            radiusX: BALL_RADIUS_DIP,
            radiusY: BALL_RADIUS_DIP,
        };
        // SAFETY: the geometry and brush are renderer-owned and valid for this immediate call.
        unsafe {
            self.target.FillEllipse(&circle, &self.brushes.background);
        }
    }

    fn draw_ball_content(
        &mut self,
        state: &AppState,
        center: Vector2,
        opacity: f32,
    ) -> Result<(), AppError> {
        let opacity = opacity.clamp(0.0, 1.0);
        let now = Instant::now();
        // The ball is the glanceable summary: primary window remaining (or the
        // only window of single-window accounts), reading 0 while any active
        // window is exhausted and -- while no fresh snapshot is known.
        let (label, remaining, color) = ball_quota(state, SystemTime::now());
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
        // 低额度（<20% 或已耗尽）时脉冲。没有弧可脉冲时改脉冲轨道圆——
        // 否则这个状态下球面上没有任何在动的东西。
        let pulse = self
            .ring
            .pulse_factor(now, matches!(color, QuotaColor::Critical));
        let track_opacity = if arc.is_some() {
            opacity
        } else {
            opacity * pulse
        };

        let track = D2D1_ELLIPSE {
            point: center,
            radiusX: RING_RADIUS_DIP,
            radiusY: RING_RADIUS_DIP,
        };
        // SAFETY: the brush is renderer-owned and its opacity is restored before returning.
        unsafe {
            self.brushes.track.SetOpacity(track_opacity);
            self.target
                .DrawEllipse(&track, &self.brushes.track, RING_STROKE_DIP, None);
            self.brushes.track.SetOpacity(1.0);
        }

        if let Some(fraction) = arc {
            let brush = self.brush_for(color);
            // SAFETY: the brush is renderer-owned and its opacity is restored after the draw.
            unsafe { brush.SetOpacity(opacity * pulse) };
            let result = self.draw_progress_arc(center, RING_RADIUS_DIP, fraction, brush);
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
            self.draw_ball_content(
                state,
                Vector2 {
                    X: transition.ball_center.0,
                    Y: transition.ball_center.1,
                },
                transition.ball_opacity,
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
        // 行位按可见行顺序排布：超额行大多不发生，未发生时不占位，面板
        // 高度由 panel_height_dip 按同一可见性规则收缩。
        let mut top = FIRST_ROW_TOP_DIP;
        self.draw_quota_row(&short_term_label, short_term, top, width, opacity);
        top += ROW_STEP_DIP;
        self.draw_quota_row(&long_term_label, long_term, top, width, opacity);
        top += ROW_STEP_DIP;
        self.draw_token_usage_row(
            "今日使用",
            state.today_tokens,
            Some(state.today_cost),
            top,
            width,
            opacity,
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
            );
            top += ROW_STEP_DIP;
        }
        self.draw_token_usage_row(
            "本期使用",
            state.current_period_tokens,
            Some(state.current_period_cost),
            top,
            width,
            opacity,
        );
        top += ROW_STEP_DIP;
        if period_overflow_visible(state) {
            self.draw_token_usage_row(
                "本期超额",
                state.current_period_overflow_tokens,
                Some(state.current_period_overflow_cost),
                top,
                width,
                opacity,
            );
            top += ROW_STEP_DIP;
        }
        self.draw_period_total_value_row(state, top, opacity);
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
                    right: width - 18.0,
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
            &format_local_timestamp(window.resets_at),
            &self.body_format,
            &self.brushes.secondary_text,
            D2D_RECT_F {
                left: TIME_COLUMN_LEFT,
                top,
                right: width - 18.0,
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
                &self.brushes.yellow
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
    fn draw_token_usage_row(
        &self,
        label: &str,
        tokens: Option<u64>,
        cost: Option<Option<f64>>,
        top: f32,
        width: f32,
        opacity: f32,
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
            // 价值列与额度百分比同色（健康绿），视觉上与额度行成组。
            self.draw_text_with_opacity(
                &format_usd(cost),
                &self.body_format,
                if cost.is_some() {
                    &self.brushes.green
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
            &format_token_usage(tokens),
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

    /// 本期估值行：账号侧估算（绿色，96→140 列）+ 社区参考值（灰字，
    /// 时间列位置）。两个数值按约定不带文字标注，靠颜色深浅区分。
    fn draw_period_total_value_row(&self, state: &AppState, top: f32, opacity: f32) {
        let estimate = display_period_total_value(state, SystemTime::now());
        self.draw_text_with_opacity(
            "本期估值",
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
                &self.brushes.green
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
        self.draw_text_with_opacity(
            &format_usd(Some(COMMUNITY_WEEKLY_VALUE_USD)),
            &self.body_format,
            &self.brushes.secondary_text,
            D2D_RECT_F {
                left: TIME_COLUMN_LEFT,
                top,
                right: 200.0,
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
        if fraction >= 0.999 {
            let ellipse = D2D1_ELLIPSE {
                point: center,
                radiusX: radius,
                radiusY: radius,
            };
            // SAFETY: immediate draw with valid geometry and renderer-owned brush.
            unsafe {
                self.target
                    .DrawEllipse(&ellipse, brush, RING_STROKE_DIP, None);
            }
            return Ok(());
        }

        let angle = std::f64::consts::TAU * fraction;
        let start = Vector2 {
            X: center.X,
            Y: center.Y - radius,
        };
        let end = Vector2 {
            X: center.X + (angle.sin() as f32 * radius),
            Y: center.Y - (angle.cos() as f32 * radius),
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
                arcSize: if fraction > 0.5 {
                    D2D1_ARC_SIZE_LARGE
                } else {
                    D2D1_ARC_SIZE_SMALL
                },
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            sink.Close()?;
            self.target
                .DrawGeometry(&geometry, brush, RING_STROKE_DIP, None);
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
            QuotaColor::Healthy => &self.brushes.green,
            QuotaColor::Warning => &self.brushes.yellow,
            QuotaColor::Critical => &self.brushes.red,
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

fn create_brushes(target: &ID2D1DCRenderTarget) -> Result<Brushes, AppError> {
    let render_target: ID2D1RenderTarget = target.cast()?;
    let brush = |color: &D2D1_COLOR_F| {
        // SAFETY: color is borrowed only for this synchronous factory call.
        unsafe { render_target.CreateSolidColorBrush(color, None) }
    };
    Ok(Brushes {
        background: brush(&BACKGROUND)?,
        track: brush(&TRACK)?,
        primary_text: brush(&PRIMARY_TEXT)?,
        secondary_text: brush(&SECONDARY_TEXT)?,
        green: brush(&GREEN)?,
        yellow: brush(&YELLOW)?,
        red: brush(&RED)?,
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

    fn clear(&self) {
        let byte_len = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .map(|height| width * height * 4)
            })
            .unwrap_or(0);
        // SAFETY: bits points to this surface's width*height*4-byte DIB allocation.
        unsafe { ptr::write_bytes(self.bits, 0, byte_len) };
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
    use std::time::{Duration, Instant};

    use super::RingState;
    use crate::win32::layout::{
        PULSE_MIN_OPACITY, PULSE_PERIOD, RING_PULSE_FRAME_MILLIS, RING_TWEEN_FRAME_MILLIS,
        RingAnimation, pulse_opacity,
    };

    #[test]
    fn first_known_value_sweeps_in_from_zero() {
        let start = Instant::now();
        let mut ring = RingState::new(start);

        ring.retarget(Some(0.4), start);

        // 球刚出现：从 0 扫入，而不是直接落在目标值上。
        assert_eq!(ring.fraction(start), Some(0.0));
        assert_eq!(ring.frame_interval(start), Some(RING_TWEEN_FRAME_MILLIS));

        let swept = start + RingAnimation::SWEEP_DURATION;
        assert!((ring.fraction(swept).unwrap_or_default() - 0.4).abs() < 1e-9);
        // 扫入结束后没有脉冲，就不再需要帧定时器。
        assert_eq!(ring.frame_interval(swept), None);
    }

    #[test]
    fn value_reappearing_after_unknown_snaps_without_a_sweep() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.4), start);
        let swept = start + RingAnimation::SWEEP_DURATION;
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
        let swept = start + RingAnimation::SWEEP_DURATION;
        assert!((ring.fraction(swept).unwrap_or_default() - 0.4).abs() < 1e-9);

        ring.expect_appearance();
        ring.retarget(Some(0.4), swept);

        assert_eq!(ring.fraction(swept), Some(0.0), "重新出现应再次从 0 扫入");
        let reswept = swept + RingAnimation::SWEEP_DURATION;
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
        let swept = start + RingAnimation::SWEEP_DURATION;
        assert!((ring.fraction(swept).unwrap_or_default() - 0.4).abs() < 1e-9);

        ring.retarget(Some(0.1), swept);

        let first = ring.fraction(swept).unwrap_or_default();
        assert!((first - 0.4).abs() < 1e-9, "补间应从旧值起步：{first}");
        assert_eq!(ring.frame_interval(swept), Some(RING_TWEEN_FRAME_MILLIS));

        let settled = swept + RingAnimation::DURATION;
        let value = ring.fraction(settled).unwrap_or_default();
        assert!((value - 0.1).abs() < 1e-9);
        assert_eq!(ring.frame_interval(settled), None);
    }

    #[test]
    fn retargeting_mid_tween_restarts_from_the_interpolated_value() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.5), start);
        let swept = start + RingAnimation::SWEEP_DURATION;
        assert!((ring.fraction(swept).unwrap_or_default() - 0.5).abs() < 1e-9);
        // 值变化才起补间：0.5 → 0.1。
        ring.retarget(Some(0.1), swept);
        let middle = swept + RingAnimation::DURATION / 2;
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
        let settled = ring
            .fraction(middle + RingAnimation::DURATION)
            .unwrap_or_default();
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
        let rolled = start + RingAnimation::LABEL_SWEEP;
        assert_eq!(ring.label_fraction(rolled), None);
        let arc = ring.fraction(rolled).unwrap_or_default();
        assert!(arc > 0.0 && arc < 0.5, "弧应仍在扫入中：{arc}");
        assert_eq!(ring.frame_interval(rolled), Some(RING_TWEEN_FRAME_MILLIS));

        let settled = start + RingAnimation::SWEEP_DURATION;
        assert!((ring.fraction(settled).unwrap_or_default() - 0.5).abs() < 1e-9);
        assert_eq!(ring.frame_interval(settled), None);
    }

    #[test]
    fn label_rolls_from_the_value_it_currently_shows_when_the_target_changes() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.5), start);
        let swept = start + RingAnimation::SWEEP_DURATION;
        assert!((ring.fraction(swept).unwrap_or_default() - 0.5).abs() < 1e-9);

        ring.retarget(Some(0.1), swept);

        assert_eq!(ring.label_fraction(swept), Some(0.5), "从当前显示值起步");
        // 数字用更短的时长先落定，此刻弧还在补间。
        let rolled = swept + RingAnimation::LABEL_TWEEN;
        assert_eq!(ring.label_fraction(rolled), None);
        let arc = ring.fraction(rolled).unwrap_or_default();
        assert!(arc < 0.5 && arc > 0.1, "弧应仍在补间中：{arc}");
        assert_eq!(ring.frame_interval(rolled), Some(RING_TWEEN_FRAME_MILLIS));

        // 弧落定后必须停止请求帧——数字比弧长会把这条测试打红。
        let settled = swept + RingAnimation::DURATION;
        assert!((ring.fraction(settled).unwrap_or_default() - 0.1).abs() < 1e-9);
        assert_eq!(ring.frame_interval(settled), None);
    }

    #[test]
    fn label_does_not_roll_when_the_target_becomes_unknown() {
        let start = Instant::now();
        let mut ring = RingState::new(start);
        ring.retarget(Some(0.4), start);
        let swept = start + RingAnimation::SWEEP_DURATION;

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
}
