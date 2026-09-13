use std::time::Instant;

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, KillTimer, SetTimer};

use super::layout::{
    PanelAnimation, anchored_destination, animation_shape_rect, dip_to_px, expanded_destination,
    monitor_info, px_to_dip,
};
use super::presentation::{COMPACT_PANEL_HEIGHT_DIP, panel_height_dip};
use super::renderer::{Renderer, TransitionVisual, VisualState};
use super::{
    ANIMATION_FRAME_MILLIS, AppWindow, COLLAPSE_ANIMATION_DURATION, COLLAPSED_DIP,
    EXPAND_ANIMATION_DURATION, PANEL_WIDTH_DIP, TIMER_ANIMATION, TIMER_RING,
};
use crate::error::AppError;

impl AppWindow {
    /// 展开态面板高度（dip）：随超额行可见性收缩。锁损坏时按精简高度
    /// 兜底——渲染路径本身会因锁损坏报错。
    pub(super) fn panel_height(&self) -> f32 {
        self.state
            .lock()
            .map_or(COMPACT_PANEL_HEIGHT_DIP, |state| panel_height_dip(&state))
    }

    pub(super) fn desired_size(&self) -> (i32, i32) {
        if self.expanded {
            (dip_to_px(PANEL_WIDTH_DIP, self.dpi), self.panel_height_px())
        } else {
            let side = dip_to_px(COLLAPSED_DIP, self.dpi);
            (side, side)
        }
    }

    fn panel_height_px(&self) -> i32 {
        dip_to_px(self.panel_height(), self.dpi)
    }

    pub(super) fn render(&mut self) -> Result<(), AppError> {
        if !self.overlay_active {
            return Ok(());
        }
        if self.animation.is_some() {
            return self.render_animation_frame(Instant::now());
        }
        let mut rect = RECT::default();
        // SAFETY: hwnd is live while AppWindow is reachable from the window procedure.
        unsafe { GetWindowRect(self.hwnd, &mut rect)? };
        self.render_at(
            POINT {
                x: rect.left,
                y: rect.top,
            },
            rect.right - rect.left,
            rect.bottom - rect.top,
        )
    }

    pub(super) fn render_at(
        &mut self,
        destination: POINT,
        width: i32,
        height: i32,
    ) -> Result<(), AppError> {
        let visual = if self.expanded {
            VisualState::Expanded
        } else {
            VisualState::Collapsed
        };
        self.render_at_visual(destination, width, height, visual)
    }

    fn render_at_visual(
        &mut self,
        destination: POINT,
        width: i32,
        height: i32,
        visual: VisualState,
    ) -> Result<(), AppError> {
        if self.renderer.is_some() {
            let state = self
                .state
                .lock()
                .map_err(|_| AppError::Render("额度状态锁已损坏".to_owned()))?
                .clone();
            // 动效开关必须在绘制前推送，否则关闭动效后仍会画出某一帧的暗色。
            let ring_enabled = self.ring_animations_allowed();
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.set_ring_animation_enabled(ring_enabled);
                renderer.resize(width, height, self.dpi)?;
                renderer.render(self.hwnd, destination, visual, &state)?;
            }
        }
        // 绘制推进了动效状态，据此驱动或停掉帧定时器。
        self.sync_ring_frames();
        Ok(())
    }

    /// 悬浮球动效是否允许运行：系统动画开关 ∧ 已激活 ∧ 球可见 ∧ 面板未展开
    /// ∧ 未在拖拽。绘制前推送与帧定时器门控共用这一处判定，避免两份条件漂移。
    fn ring_animations_allowed(&self) -> bool {
        self.overlay_active
            && self.visible
            && !self.expanded
            && !self.dragging
            && self.animations_enabled
    }

    /// 按渲染器给出的间隔驱动悬浮球动效的帧定时器。只在目标间隔变化时重设，
    /// 避免每帧重置定时器导致它永远不触发。
    fn sync_ring_frames(&mut self) {
        let desired = if self.ring_animations_allowed() {
            self.renderer
                .as_ref()
                .and_then(Renderer::ring_frame_interval)
        } else {
            None
        };
        if desired == self.ring_frame_millis {
            return;
        }
        let Some(millis) = desired else {
            self.stop_ring_frames();
            return;
        };
        if self.hwnd.is_invalid() {
            return;
        }
        self.ring_frame_millis = Some(millis);
        // SAFETY: the HWND timer is UI-thread-owned and removed by stop_ring_frames.
        if unsafe { SetTimer(Some(self.hwnd), TIMER_RING, millis, None) } == 0 {
            crate::logging::log("无法创建悬浮球动画计时器，本次动效已跳过");
            self.ring_frame_millis = None;
        }
    }

    pub(super) fn stop_ring_frames(&mut self) {
        self.ring_frame_millis = None;
        if !self.hwnd.is_invalid() {
            // SAFETY: removes only this window's fixed ring animation timer ID.
            let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_RING) };
        }
    }

    pub(super) fn render_animation_frame(&mut self, now: Instant) -> Result<(), AppError> {
        let Some(animation) = self.animation else {
            return self.render();
        };
        let sample = animation.sample(now);
        let panel_height_dip = self.panel_height();
        let shape = animation_shape_rect(
            animation,
            sample.expansion,
            self.dpi,
            panel_height_dip,
            self.config.placement.edge,
            self.expansion_alignment,
        );
        let width = dip_to_px(PANEL_WIDTH_DIP, self.dpi);
        let height = dip_to_px(panel_height_dip, self.dpi);
        let ball_center = (
            px_to_dip(
                animation.ball_center_screen.x - animation.canvas_destination.x,
                self.dpi,
            ),
            px_to_dip(
                animation.ball_center_screen.y - animation.canvas_destination.y,
                self.dpi,
            ),
        );
        let shape_bounds = (
            px_to_dip(shape.left - animation.canvas_destination.x, self.dpi),
            px_to_dip(shape.top - animation.canvas_destination.y, self.dpi),
            px_to_dip(shape.right - animation.canvas_destination.x, self.dpi),
            px_to_dip(shape.bottom - animation.canvas_destination.y, self.dpi),
        );
        self.render_at_visual(
            animation.canvas_destination,
            width,
            height,
            VisualState::Transition(TransitionVisual {
                expansion: sample.expansion,
                ball_opacity: sample.ball_opacity,
                panel_opacity: sample.panel_opacity,
                ball_center,
                shape_bounds,
            }),
        )?;

        if sample.finished {
            self.stop_animation_timer();
            self.animation = None;
            self.resize_for_state()?;
            self.update_outside_click_hook();
        }
        Ok(())
    }

    pub(super) fn resize_for_state(&mut self) -> Result<(), AppError> {
        let mut rect = RECT::default();
        // SAFETY: hwnd is live and rect is writable.
        unsafe { GetWindowRect(self.hwnd, &mut rect)? };
        let (width, height) = self.desired_size();
        let monitor = unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) };
        let work = monitor_info(monitor)?.info.monitorInfo.rcWork;
        let is_size_transition =
            rect.right - rect.left != width || rect.bottom - rect.top != height;
        let destination = if self.expanded && is_size_transition {
            let (destination, alignment) =
                expanded_destination(rect, self.config.placement.edge, width, height, work);
            self.expansion_alignment = alignment;
            destination
        } else {
            anchored_destination(
                rect,
                self.config.placement.edge,
                width,
                height,
                work,
                self.expansion_alignment,
            )
        };
        // UpdateLayeredWindow commits the new pixels, position, and size together. Calling
        // SetWindowPos first exposes the old 56-DIP bitmap for one compositor frame.
        self.render_at(destination, width, height)
    }

    pub(super) fn toggle_expanded(&mut self) -> Result<(), AppError> {
        self.set_expanded(!self.expanded)
    }

    pub(super) fn set_expanded(&mut self, expanded: bool) -> Result<(), AppError> {
        if !self.overlay_active {
            return Ok(());
        }
        if self.animation.is_some() {
            self.finish_animation()?;
        }
        if self.expanded == expanded {
            return Ok(());
        }
        self.expanded = expanded;
        self.remove_outside_click_hook();
        if self.animations_enabled && self.visible {
            return self.start_animation(expanded);
        }
        let result = self.resize_for_state();
        self.update_outside_click_hook();
        result
    }

    fn start_animation(&mut self, expanding: bool) -> Result<(), AppError> {
        let mut rect = RECT::default();
        // SAFETY: hwnd is live and rect is writable.
        unsafe { GetWindowRect(self.hwnd, &mut rect)? };
        let monitor = unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) };
        let work_area = monitor_info(monitor)?.info.monitorInfo.rcWork;
        let canvas_destination = if expanding {
            let width = dip_to_px(PANEL_WIDTH_DIP, self.dpi);
            let height = self.panel_height_px();
            let (destination, alignment) =
                expanded_destination(rect, self.config.placement.edge, width, height, work_area);
            self.expansion_alignment = alignment;
            destination
        } else {
            POINT {
                x: rect.left,
                y: rect.top,
            }
        };

        let side = dip_to_px(COLLAPSED_DIP, self.dpi);
        let ball_center_screen = if expanding {
            POINT {
                x: rect.left + (rect.right - rect.left) / 2,
                y: rect.top + (rect.bottom - rect.top) / 2,
            }
        } else {
            let destination = anchored_destination(
                rect,
                self.config.placement.edge,
                side,
                side,
                work_area,
                self.expansion_alignment,
            );
            POINT {
                x: destination.x + side / 2,
                y: destination.y + side / 2,
            }
        };
        self.animation = Some(PanelAnimation {
            started_at: Instant::now(),
            duration: if expanding {
                EXPAND_ANIMATION_DURATION
            } else {
                COLLAPSE_ANIMATION_DURATION
            },
            anchor_rect: rect,
            work_area,
            canvas_destination,
            ball_center_screen,
            expanding,
        });
        // SAFETY: the HWND timer is UI-thread-owned and is stopped at animation completion.
        if unsafe {
            SetTimer(
                Some(self.hwnd),
                TIMER_ANIMATION,
                ANIMATION_FRAME_MILLIS,
                None,
            )
        } == 0
        {
            self.animation = None;
            crate::logging::log("无法创建面板动画计时器，已改用即时切换");
            let result = self.resize_for_state();
            self.update_outside_click_hook();
            return result;
        }
        self.render_animation_frame(Instant::now())
    }

    pub(super) fn finish_animation(&mut self) -> Result<(), AppError> {
        if self.animation.is_none() {
            return Ok(());
        }
        self.stop_animation_timer();
        self.animation = None;
        let result = self.resize_for_state();
        self.update_outside_click_hook();
        result
    }

    pub(super) fn stop_animation_timer(&self) {
        if !self.hwnd.is_invalid() {
            // SAFETY: removes only this window's fixed animation timer ID.
            let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_ANIMATION) };
        }
    }
}
