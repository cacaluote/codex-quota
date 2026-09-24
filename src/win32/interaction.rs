use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    MONITOR_DEFAULTTONEAREST, MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetCursorPos, GetSystemMetrics, GetWindowRect, HTCLIENT, HTTRANSPARENT,
    MSLLHOOKSTRUCT, PostMessageW, SM_CXDRAG, SM_CYDRAG, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    SetTimer, SetWindowPos, SetWindowsHookExW, UnhookWindowsHookEx, WH_MOUSE_LL, WM_LBUTTONDOWN,
    WM_MBUTTONDOWN, WM_RBUTTONDOWN, WM_XBUTTONDOWN,
};

use super::layout::{
    SnapAnimation, anchored_destination, animation_shape_rect, dip_to_px, lerp, monitor_device,
    monitor_info, point_in_ball, point_in_rounded_rect, point_is_outside_rounded_rect, px_to_dip,
};
use super::renderer::BALL_RADIUS_DIP;
use super::{
    ANIMATION_FRAME_MILLIS, AppWindow, OUTSIDE_CLICK_HWND, TIMER_ANIMATION, WM_APP_COLLAPSE,
};
use crate::config::AnchorEdge;
use crate::error::AppError;

impl AppWindow {
    pub(super) fn update_outside_click_hook(&mut self) {
        let should_install = self.visible
            && self.overlay_active
            && self.expanded
            && self.animation.is_none()
            && self.config.collapse_on_outside_click;
        if should_install && self.outside_click_hook.is_none() {
            if let Err(error) = self.install_outside_click_hook() {
                crate::logging::log(&error.to_string());
            }
        } else if !should_install {
            self.remove_outside_click_hook();
        }
    }

    fn install_outside_click_hook(&mut self) -> Result<(), AppError> {
        // SAFETY: this installs a global low-level mouse hook (thread ID zero). A WH_MOUSE_LL
        // procedure is dispatched to the thread that installed it instead of being injected
        // into other processes, so it does not live in a DLL and no module handle is used —
        // `hMod` is passed as NULL. The returned handle is owned by AppWindow until explicit
        // removal.
        let hook =
            unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(outside_click_mouse_proc), None, 0) }
                .map_err(|error| AppError::Windows(format!("无法启用点击外部收起：{error}")))?;
        OUTSIDE_CLICK_HWND.store(self.hwnd.0 as usize, Ordering::Release);
        self.outside_click_hook = Some(hook);
        Ok(())
    }

    pub(super) fn remove_outside_click_hook(&mut self) {
        OUTSIDE_CLICK_HWND.store(0, Ordering::Release);
        let Some(hook) = self.outside_click_hook.take() else {
            return;
        };
        // SAFETY: hook is uniquely owned by this AppWindow and removed at most once.
        if let Err(error) = unsafe { UnhookWindowsHookEx(hook) } {
            crate::logging::log(&format!("无法关闭点击外部收起：{error}"));
        }
    }

    pub(super) fn begin_drag(&mut self) -> Result<(), AppError> {
        if self.animation.is_some() {
            return Ok(());
        }
        // 上一次松手的吸附可能还在滑：先落位，再从落点开始这次拖拽，否则拖拽
        // 的起点会和吸附动画抢同一个窗口位置。
        if let Err(error) = self.finish_snap() {
            crate::logging::log(&error.to_string());
        }
        self.drag_velocity.reset();
        let mut cursor = POINT::default();
        let mut rect = RECT::default();
        // SAFETY: both outputs are valid and hwnd is live.
        if unsafe { GetCursorPos(&mut cursor) }.is_ok()
            && unsafe { GetWindowRect(self.hwnd, &mut rect) }.is_ok()
        {
            self.pointer_down = true;
            self.dragging = false;
            self.drag_cursor_origin = cursor;
            self.drag_window_origin = POINT {
                x: rect.left,
                y: rect.top,
            };
            // SAFETY: capture is released on button-up or window teardown.
            unsafe { SetCapture(self.hwnd) };
        }
        // 按下这一下必须自己重绘：按住期间可能一个鼠标移动都不来，而在拖拽阈值
        // 以内 `update_drag` 也不重绘——不主动画这一帧，"按下缩放"就等于没有。
        self.render()
    }

    pub(super) fn update_drag(&mut self) -> Result<(), AppError> {
        if !self.pointer_down {
            return Ok(());
        }
        let mut cursor = POINT::default();
        // SAFETY: cursor points to initialized writable storage.
        unsafe { GetCursorPos(&mut cursor)? };
        let dx = cursor.x - self.drag_cursor_origin.x;
        let dy = cursor.y - self.drag_cursor_origin.y;
        // SAFETY: system metric reads have no pointer lifetime requirements.
        let threshold_x = unsafe { GetSystemMetrics(SM_CXDRAG) };
        let threshold_y = unsafe { GetSystemMetrics(SM_CYDRAG) };
        if !self.dragging && dx.abs() <= threshold_x && dy.abs() <= threshold_y {
            return Ok(());
        }
        self.dragging = true;
        self.drag_velocity.sample(cursor, Instant::now());
        // SAFETY: the live popup window is moved without activation or resizing.
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                self.drag_window_origin.x + dx,
                self.drag_window_origin.y + dy,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER,
            )?;
        }
        self.render()
    }

    /// 光标进入球（`WM_MOUSEMOVE`）。
    ///
    /// `WM_NCHITTEST` 把圆外标成 `HTTRANSPARENT`，所以这条消息通常只在球上到达；
    /// 反过来，"离开"不会有下一条消息，必须靠 `TrackMouseEvent` 要一条
    /// `WM_MOUSELEAVE`。
    pub(super) fn handle_mouse_move(&mut self) -> Result<(), AppError> {
        // 不假设"收到移动就等于光标在球上"：拖拽期间窗口握着鼠标捕获，光标跑到
        // 屏幕另一头也会收到这条消息。那时若照旧算成悬停，松手后球会带着高亮
        // 停在离光标很远的地方。
        let hovered = self.cursor_is_over_ball();
        if hovered != self.hovered {
            self.hovered = hovered;
            if hovered {
                self.track_mouse_leave();
            }
            self.render()?;
        }
        self.update_drag()
    }

    /// 光标此刻是否落在球的命中圆内（与 `hit_test` 用同一个半径）。
    fn cursor_is_over_ball(&self) -> bool {
        let mut cursor = POINT::default();
        let mut rect = RECT::default();
        // SAFETY: both outputs are valid writable storage and hwnd is live.
        if unsafe { GetCursorPos(&mut cursor) }.is_err()
            || unsafe { GetWindowRect(self.hwnd, &mut rect) }.is_err()
        {
            return false;
        }
        point_in_ball(
            cursor.x - rect.left,
            cursor.y - rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            dip_to_px(BALL_RADIUS_DIP, self.dpi),
        )
    }

    /// 光标离开球（`WM_MOUSELEAVE`）。
    pub(super) fn handle_mouse_leave(&mut self) -> Result<(), AppError> {
        if self.hovered {
            self.hovered = false;
            self.render()?;
        }
        Ok(())
    }

    /// 登记一次"离开"通知。每次悬停只需要登记一次，`WM_MOUSELEAVE` 是一次性的。
    fn track_mouse_leave(&self) {
        let mut event = TRACKMOUSEEVENT {
            cbSize: u32::try_from(size_of::<TRACKMOUSEEVENT>()).unwrap_or_default(),
            dwFlags: TME_LEAVE,
            hwndTrack: self.hwnd,
            dwHoverTime: 0,
        };
        // SAFETY: hwndTrack is this live window and the structure outlives the synchronous call.
        if let Err(error) = unsafe { TrackMouseEvent(&mut event) } {
            crate::logging::log(&format!("无法登记光标离开通知：{error}"));
        }
    }

    pub(super) fn end_drag(&mut self) -> Result<(), AppError> {
        if !self.pointer_down {
            return Ok(());
        }
        self.pointer_down = false;
        // SAFETY: balances SetCapture from begin_drag; failure means capture was already lost.
        let _ = unsafe { ReleaseCapture() };
        if self.dragging {
            self.dragging = false;
            // 吸附从"手此刻的速度"接着滑，所以要把松手瞬间的速度量出来。
            let velocity = self.drag_velocity.release_velocity(Instant::now());
            self.snap_to_edge(velocity)?;
        } else {
            self.toggle_expanded()?;
        }
        Ok(())
    }

    /// 松手后的吸附：滑到最近的那条工作区边缘，起点速度用手的速度。
    pub(super) fn snap_to_edge(&mut self, velocity: (f32, f32)) -> Result<(), AppError> {
        self.place_at_edge(true, velocity)
    }

    /// 立刻吸附落位，不滑动。
    ///
    /// 用于显示器配置变化和 DPI 变化——这种时候是"世界变了"，滑动只会让窗口
    /// 慢半拍地飘到新位置。
    pub(super) fn snap_to_edge_now(&mut self) -> Result<(), AppError> {
        self.finish_snap()?;
        self.place_at_edge(false, (0.0, 0.0))
    }

    fn place_at_edge(&mut self, animate: bool, velocity: (f32, f32)) -> Result<(), AppError> {
        let mut rect = RECT::default();
        // SAFETY: hwnd is live and rect is writable.
        unsafe { GetWindowRect(self.hwnd, &mut rect)? };
        let monitor = unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) };
        let info = monitor_info(monitor)?;
        let work = info.info.monitorInfo.rcWork;
        let distances = [
            (rect.left - work.left).abs(),
            (work.right - rect.right).abs(),
            (rect.top - work.top).abs(),
            (work.bottom - rect.bottom).abs(),
        ];
        let index = distances
            .iter()
            .enumerate()
            .min_by_key(|(_, distance)| *distance)
            .map_or(1, |(index, _)| index);
        let edge = match index {
            0 => AnchorEdge::Left,
            1 => AnchorEdge::Right,
            2 => AnchorEdge::Top,
            _ => AnchorEdge::Bottom,
        };
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let (mut x, mut y) = (rect.left, rect.top);
        match edge {
            AnchorEdge::Left => x = work.left,
            AnchorEdge::Right => x = work.right - width,
            AnchorEdge::Top => y = work.top,
            AnchorEdge::Bottom => y = work.bottom - height,
        }
        x = x.clamp(work.left, work.right - width);
        y = y.clamp(work.top, work.bottom - height);
        self.config.placement.edge = edge;
        self.config.placement.monitor_device = monitor_device(&info);
        let offset_px = match edge {
            AnchorEdge::Left | AnchorEdge::Right => y - work.top,
            AnchorEdge::Top | AnchorEdge::Bottom => x - work.left,
        };
        self.config.placement.offset_dip = px_to_dip(offset_px, self.dpi);
        self.save_config();

        let from = POINT {
            x: rect.left,
            y: rect.top,
        };
        let to = POINT { x, y };
        if animate {
            self.start_snap(from, to, velocity)
        } else {
            self.place_now(to)
        }
    }

    /// 起一段吸附动画；不值得动的时候直接落位。
    ///
    /// "不值得"有三种：起点就是终点（松手时本来贴着边）、动效被系统关掉、
    /// 现在根本没有可见的球（隐藏或面板展开着）。三者都省掉一次定时器和
    /// 十几帧重绘。
    fn start_snap(&mut self, from: POINT, to: POINT, velocity: (f32, f32)) -> Result<(), AppError> {
        let animation = SnapAnimation::new(from, to, velocity, Instant::now());
        if animation.is_noop() || !self.animations_enabled || !self.visible || self.expanded {
            return self.place_now(to);
        }
        self.snap = Some(animation);
        // SAFETY: the HWND timer is UI-thread-owned and stopped in render_snap_frame.
        // 面板动画与吸附动画互斥（拖拽时球不可能是展开的），因此共用这一个 ID。
        if unsafe {
            SetTimer(
                Some(self.hwnd),
                TIMER_ANIMATION,
                ANIMATION_FRAME_MILLIS,
                None,
            )
        } == 0
        {
            self.snap = None;
            crate::logging::log("无法创建吸附动画计时器，已改用即时落位");
            return self.place_now(to);
        }
        self.render_snap_frame(Instant::now())
    }

    /// 本帧的吸附：把窗口放到插值位置，动画结束就收表。
    pub(super) fn render_snap_frame(&mut self, now: Instant) -> Result<(), AppError> {
        let Some(animation) = self.snap else {
            return self.render();
        };
        let sample = animation.sample(now);
        if sample.finished {
            self.snap = None;
            self.stop_animation_timer();
        }
        let (width, height) = self.desired_size();
        self.render_at(sample.destination, width, height)
    }

    /// 立即结束在途的吸附并贴到落点。
    ///
    /// 按下、DPI 变化、显示器变化、以及任何要收起悬浮窗的路径都得先走这里，
    /// 否则动画会继续和它们抢窗口位置。
    pub(super) fn finish_snap(&mut self) -> Result<(), AppError> {
        let Some(animation) = self.snap.take() else {
            return Ok(());
        };
        self.stop_animation_timer();
        self.place_now(animation.destination())
    }

    /// 把窗口放到指定位置并重绘（不动画）。
    fn place_now(&mut self, destination: POINT) -> Result<(), AppError> {
        // SAFETY: moving without activation preserves the host application's focus.
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                destination.x,
                destination.y,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER,
            )?;
        }
        self.render()
    }

    pub(super) fn handle_dpi_changed(
        &mut self,
        new_dpi: u32,
        suggested: *const RECT,
    ) -> Result<(), AppError> {
        self.finish_snap()?;
        self.stop_animation_timer();
        self.animation = None;
        self.dpi = new_dpi.max(96);
        let target = if suggested.is_null() {
            let mut current = RECT::default();
            // SAFETY: hwnd is live and current is writable.
            unsafe { GetWindowRect(self.hwnd, &mut current)? };
            current
        } else {
            // SAFETY: WM_DPICHANGED guarantees that lParam points to a RECT valid during dispatch.
            unsafe { *suggested }
        };
        let (width, height) = self.desired_size();
        let monitor = unsafe {
            MonitorFromPoint(
                POINT {
                    x: target.left + (target.right - target.left) / 2,
                    y: target.top + (target.bottom - target.top) / 2,
                },
                MONITOR_DEFAULTTONEAREST,
            )
        };
        let work = monitor_info(monitor)?.info.monitorInfo.rcWork;
        let destination = anchored_destination(
            target,
            self.config.placement.edge,
            width,
            height,
            work,
            self.expansion_alignment,
        );
        let result = self.render_at(destination, width, height);
        self.update_outside_click_hook();
        result
    }

    pub(super) fn hit_test(&self) -> LRESULT {
        let mut cursor = POINT::default();
        let mut rect = RECT::default();
        // SAFETY: outputs are valid and hwnd is live.
        if unsafe { GetCursorPos(&mut cursor) }.is_err()
            || unsafe { GetWindowRect(self.hwnd, &mut rect) }.is_err()
        {
            return LRESULT(HTCLIENT as isize);
        }
        let x = cursor.x - rect.left;
        let y = cursor.y - rect.top;
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let inside = if let Some(animation) = self.animation {
            let sample = animation.sample(Instant::now());
            let shape = animation_shape_rect(
                animation,
                sample.expansion,
                self.dpi,
                self.panel_height(),
                self.config.placement.edge,
                self.expansion_alignment,
            );
            // 动画期间形状是插值后的窗口边界矩形，起点圆角半径取窗口半边长
            // （28 dip = COLLAPSED_DIP / 2，近似为圆），终点是面板圆角 16 dip。
            let radius = lerp(28.0, 16.0, sample.expansion);
            point_in_rounded_rect(
                cursor.x - shape.left,
                cursor.y - shape.top,
                shape.right - shape.left,
                shape.bottom - shape.top,
                dip_to_px(radius, self.dpi),
            )
        } else if self.expanded {
            point_in_rounded_rect(x, y, width, height, dip_to_px(16.0, self.dpi))
        } else {
            // 命中半径与渲染的圆同源：此前用窗口半边长（28 dip）比视觉半径
            // （27 dip）大 1 dip，最外一圈会点击穿透。
            point_in_ball(x, y, width, height, dip_to_px(BALL_RADIUS_DIP, self.dpi))
        };
        LRESULT(if inside {
            HTCLIENT as isize
        } else {
            HTTRANSPARENT as isize
        })
    }
}

unsafe extern "system" fn outside_click_mouse_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 && is_mouse_button_down(wparam.0 as u32) {
        let hwnd_address = OUTSIDE_CLICK_HWND.load(Ordering::Acquire);
        if hwnd_address != 0 {
            let hwnd = HWND(hwnd_address as *mut c_void);
            // SAFETY: WH_MOUSE_LL supplies a valid MSLLHOOKSTRUCT pointer for non-negative codes.
            let mouse = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            let mut rect = RECT::default();
            // SAFETY: hwnd is the live target registered with the hook and rect is writable.
            if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
                // SAFETY: hwnd remains live while registered and DPI lookup has no output pointer.
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
                if point_is_outside_rounded_rect(mouse.pt, rect, dip_to_px(16.0, dpi)) {
                    // SAFETY: the message carries no pointer payload and does not consume the click.
                    let _ =
                        unsafe { PostMessageW(Some(hwnd), WM_APP_COLLAPSE, WPARAM(0), LPARAM(0)) };
                }
            }
        }
    }
    // SAFETY: every hook invocation must continue through the system hook chain.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn is_mouse_button_down(event: u32) -> bool {
    matches!(
        event,
        WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_RBUTTONDOWN | WM_XBUTTONDOWN
    )
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE;

    use super::*;

    #[test]
    fn mouse_button_filter_accepts_only_button_down_events() {
        assert!(
            [
                WM_LBUTTONDOWN,
                WM_MBUTTONDOWN,
                WM_RBUTTONDOWN,
                WM_XBUTTONDOWN
            ]
            .into_iter()
            .all(is_mouse_button_down)
                && !is_mouse_button_down(WM_MOUSEMOVE)
        );
    }

    /// 速度只在拖拽过程中量得出来：第一次采样没有前一个点，只能记 0。
    #[test]
    fn drag_velocity_needs_two_samples() {
        let start = Instant::now();
        let mut velocity = DragVelocity::default();
        velocity.sample(POINT { x: 100, y: 100 }, start);
        assert_eq!(velocity.release_velocity(start), (0.0, 0.0));
        velocity.reset();
        assert_eq!(velocity.release_velocity(start), (0.0, 0.0));
    }

    /// 松手前手停了，速度必须归零——否则"拖到位、停一下、再松手"会从静止里
    /// 冒出一个陈旧的高速，球自己弹出去。
    #[test]
    fn drag_velocity_forgets_a_stale_release() {
        let start = Instant::now();
        let mut velocity = DragVelocity::default();
        velocity.sample(POINT { x: 100, y: 100 }, start);
        // 10ms 里向左 10px：约 1px/ms（指数平滑后是它的 0.35 倍）。
        velocity.sample(POINT { x: 90, y: 100 }, start + Duration::from_millis(10));

        let moving = velocity.release_velocity(start + Duration::from_millis(12));
        assert!(
            (moving.0 + 0.35).abs() < 0.01 && moving.1.abs() < f32::EPSILON,
            "紧接着松手应当读到刚才那个速度：{moving:?}"
        );
        assert_eq!(
            velocity.release_velocity(start + Duration::from_millis(200)),
            (0.0, 0.0),
            "停手 190ms 之后再松手，速度必须是 0"
        );
    }
}

/// 拖拽期间的光标速度采样，单位 px/ms（屏幕坐标）。
///
/// 吸附要从"松手那一刻手的速度"接着滑，所以速度必须在拖拽过程中量出来。用指数
/// 平滑而不是直接取最后一次差分：鼠标报点率常见 125–1000Hz，单次差分抖动很大，
/// 而抖动会原样变成吸附第一帧的速度。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DragVelocity {
    last: Option<(POINT, Instant)>,
    velocity: (f32, f32),
}

impl DragVelocity {
    /// 平滑系数：约等于最近三四次采样的平均。
    const SMOOTHING: f32 = 0.35;
    /// 松手前静置超过这个时长就算"停下来松手的"，速度记 0。
    const STALE: Duration = Duration::from_millis(80);

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn sample(&mut self, cursor: POINT, now: Instant) {
        if let Some((last, last_at)) = self.last {
            let elapsed_ms = now.saturating_duration_since(last_at).as_secs_f32() * 1000.0;
            if elapsed_ms > 0.0 {
                let step = (
                    (cursor.x - last.x) as f32 / elapsed_ms,
                    (cursor.y - last.y) as f32 / elapsed_ms,
                );
                let keep = 1.0 - Self::SMOOTHING;
                self.velocity = (
                    self.velocity.0 * keep + step.0 * Self::SMOOTHING,
                    self.velocity.1 * keep + step.1 * Self::SMOOTHING,
                );
            }
        }
        self.last = Some((cursor, now));
    }

    /// 松手瞬间的速度。
    ///
    /// 最后一段采样如果已经很旧，说明手在松手前就停了——这时必须记 0，否则
    /// "拖到位、停一下、再松手"会从静止里冒出一个陈旧的高速，球自己弹出去。
    pub(super) fn release_velocity(&self, now: Instant) -> (f32, f32) {
        match self.last {
            Some((_, at)) if now.saturating_duration_since(at) < Self::STALE => self.velocity,
            _ => (0.0, 0.0),
        }
    }
}
