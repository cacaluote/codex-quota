use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::time::Instant;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    MONITOR_DEFAULTTONEAREST, MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetCursorPos, GetSystemMetrics, GetWindowRect, HTCLIENT, HTTRANSPARENT,
    MSLLHOOKSTRUCT, PostMessageW, SM_CXDRAG, SM_CYDRAG, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    SetWindowPos, SetWindowsHookExW, UnhookWindowsHookEx, WH_MOUSE_LL, WM_LBUTTONDOWN,
    WM_MBUTTONDOWN, WM_RBUTTONDOWN, WM_XBUTTONDOWN,
};

use super::layout::{
    anchored_destination, animation_shape_rect, dip_to_px, lerp, monitor_device, monitor_info,
    point_in_rounded_rect, point_is_outside_rounded_rect, px_to_dip,
};
use super::{AppWindow, OUTSIDE_CLICK_HWND, WM_APP_COLLAPSE};
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
        // SAFETY: a null module name requests the executable that contains the hook procedure.
        let module = unsafe { GetModuleHandleW(None) }?;
        // SAFETY: the callback is process-lifetime code, thread ID zero requests a low-level global
        // hook, and the returned handle remains owned by AppWindow until explicit removal.
        let hook = unsafe {
            SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(outside_click_mouse_proc),
                Some(HINSTANCE(module.0)),
                0,
            )
        }
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

    pub(super) fn begin_drag(&mut self) {
        if self.animation.is_some() {
            return;
        }
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

    pub(super) fn end_drag(&mut self) -> Result<(), AppError> {
        if !self.pointer_down {
            return Ok(());
        }
        self.pointer_down = false;
        // SAFETY: balances SetCapture from begin_drag; failure means capture was already lost.
        let _ = unsafe { ReleaseCapture() };
        if self.dragging {
            self.dragging = false;
            self.snap_to_edge()?;
        } else {
            self.toggle_expanded()?;
        }
        Ok(())
    }

    pub(super) fn snap_to_edge(&mut self) -> Result<(), AppError> {
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
        // SAFETY: moving without activation preserves the host application's focus.
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                x,
                y,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER,
            )?;
        }
        self.config.placement.edge = edge;
        self.config.placement.monitor_device = monitor_device(&info);
        let offset_px = match edge {
            AnchorEdge::Left | AnchorEdge::Right => y - work.top,
            AnchorEdge::Top | AnchorEdge::Bottom => x - work.left,
        };
        self.config.placement.offset_dip = px_to_dip(offset_px, self.dpi);
        self.save_config();
        self.render()
    }

    pub(super) fn handle_dpi_changed(
        &mut self,
        new_dpi: u32,
        suggested: *const RECT,
    ) -> Result<(), AppError> {
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
            let radius = width.min(height) / 2;
            let dx = x - width / 2;
            let dy = y - height / 2;
            dx * dx + dy * dy <= radius * radius
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
}
