use std::ffi::c_void;
use std::mem::size_of;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::Shell::{
    NIIF_NONE, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, DefWindowProcW, DestroyWindow, GWL_EXSTYLE, GWLP_USERDATA, GetWindowLongPtrW,
    KillTimer, MA_NOACTIVATE, PBT_APMRESUMEAUTOMATIC, PostMessageW, PostQuitMessage,
    RegisterWindowMessageW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_CLOSE, WM_COMMAND, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE,
    WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_TIMER,
    WS_EX_TOPMOST,
};
use windows::core::w;

use super::layout::{ExpansionAlignment, system_animations_enabled};
use super::notify::{self, Notification, NotificationSwitches, Notifier};
use super::presence::CodexPresenceWatcher;
use super::renderer::Renderer;
use super::system::{load_app_icon, set_autostart};
use super::tray::{
    copy_wide_fixed, handle_tray_message, refresh_interval_for_command, tray_icon_flags,
};
use super::{
    AppWindow, CMD_AUTOSTART, CMD_EXIT, CMD_FOLLOW_CODEX, CMD_NOTIFY_OVERFLOW, CMD_NOTIFY_RESET,
    CMD_PANEL_PERSISTENT, CMD_REFRESH, CMD_SHOW, CMD_TOPMOST, TIMER_ANIMATION, TIMER_REDRAW,
    TIMER_RING, TRAY_ID, WM_APP_COLLAPSE, WM_APP_EXPAND, WM_APP_PRESENCE_CHANGED, WM_APP_SHOW,
    WM_APP_TRAY, WM_APP_UPDATED,
};
use crate::config::{self, AppConfigV1};
use crate::error::AppError;
use crate::quota::{AppState, CodexWorker};

impl AppWindow {
    pub(super) fn new(config: AppConfigV1, state: Arc<Mutex<AppState>>) -> Self {
        Self {
            hwnd: HWND::default(),
            config,
            state,
            worker: None,
            presence_watcher: None,
            renderer: None,
            outside_click_hook: None,
            animation: None,
            ring_frame_millis: None,
            animations_enabled: true,
            expanded: false,
            expansion_alignment: ExpansionAlignment::Start,
            visible: false,
            overlay_active: false,
            codex_present: false,
            presence_generation: 0,
            dragging: false,
            pointer_down: false,
            drag_cursor_origin: Default::default(),
            drag_window_origin: Default::default(),
            dpi: 96,
            taskbar_created: 0,
            tray_added: false,
            tray_uses_v4: false,
            tray_menu_open: false,
            last_tray_menu_closed: None,
            notifier: Notifier::default(),
            notify_state: crate::notify_state::load(),
        }
    }

    pub(super) fn initialize(&mut self, hwnd: HWND) -> Result<(), AppError> {
        self.hwnd = hwnd;
        self.animations_enabled = system_animations_enabled();
        // SAFETY: hwnd is a newly-created live window on this UI thread.
        self.dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        // SAFETY: the registered message contains no pointer payload.
        self.taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        self.add_tray_icon()?;
        self.apply_topmost()?;
        if self.config.follow_codex {
            self.start_presence_watcher()
        } else {
            self.activate_overlay()
        }
    }

    fn activate_overlay(&mut self) -> Result<(), AppError> {
        if self.overlay_active {
            return Ok(());
        }
        self.reset_state();
        self.expanded = false;
        self.animation = None;
        let (width, height) = self.desired_size();
        self.renderer = Some(Renderer::new(width, height, self.dpi)?);
        // SAFETY: HWND timer posts WM_TIMER and is removed whenever overlay resources are released.
        if unsafe { SetTimer(Some(self.hwnd), TIMER_REDRAW, 30_000, None) } == 0 {
            self.renderer = None;
            return Err(AppError::Windows("无法创建界面刷新计时器".to_owned()));
        }
        self.overlay_active = true;

        let notify_hwnd = self.hwnd.0 as usize;
        self.spawn_worker(notify_hwnd);
        if let Err(error) = self.resize_for_state() {
            self.deactivate_overlay();
            return Err(error);
        }
        self.visible = true;
        // SAFETY: no-activate show leaves focus with the current foreground window.
        let _ = unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
        self.ensure_topmost("激活悬浮窗")?;
        Ok(())
    }

    fn spawn_worker(&mut self, notify_hwnd: usize) {
        self.worker = Some(CodexWorker::spawn(Arc::clone(&self.state), move || {
            let target = HWND(notify_hwnd as *mut c_void);
            // SAFETY: posting by value is safe even if shutdown races; failure is intentionally ignored.
            let _ = unsafe { PostMessageW(Some(target), WM_APP_UPDATED, WPARAM(0), LPARAM(0)) };
        }));
    }

    fn deactivate_overlay(&mut self) {
        if !self.overlay_active {
            return;
        }
        self.visible = false;
        self.remove_outside_click_hook();
        self.stop_animation_timer();
        self.stop_ring_frames();
        self.animation = None;
        // 释放资源期间不再观测额度：丢掉上一次快照，避免下次激活时比出假重置。
        self.notifier.reset();
        self.expanded = false;
        self.pointer_down = false;
        self.dragging = false;
        // SAFETY: the live window is hidden before its backing bitmap is released.
        let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        // SAFETY: removes only this window's fixed redraw timer and releases any owned capture.
        let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_REDRAW) };
        let _ = unsafe { ReleaseCapture() };
        if let Some(mut worker) = self.worker.take() {
            worker.shutdown();
        }
        self.renderer = None;
        self.overlay_active = false;
        self.reset_state();
    }

    fn reset_state(&self) {
        if let Ok(mut state) = self.state.lock() {
            *state = AppState {
                quota_refresh_interval: self.config.quota_refresh_interval(),
                ..AppState::default()
            };
        }
    }

    fn start_presence_watcher(&mut self) -> Result<(), AppError> {
        self.stop_presence_watcher();
        self.presence_generation = self.presence_generation.wrapping_add(1);
        let generation = self.presence_generation;
        let notify_hwnd = self.hwnd.0 as usize;
        self.presence_watcher = Some(CodexPresenceWatcher::spawn(
            self.config.follow_codex_check_interval(),
            move |present| {
                let target = HWND(notify_hwnd as *mut c_void);
                // SAFETY: the message carries only scalar state and a generation number.
                let _ = unsafe {
                    PostMessageW(
                        Some(target),
                        WM_APP_PRESENCE_CHANGED,
                        WPARAM(usize::from(present)),
                        LPARAM(generation as isize),
                    )
                };
            },
        )?);
        self.codex_present = false;
        Ok(())
    }

    fn stop_presence_watcher(&mut self) {
        self.presence_generation = self.presence_generation.wrapping_add(1);
        if let Some(mut watcher) = self.presence_watcher.take() {
            watcher.shutdown();
        }
        self.codex_present = false;
    }

    fn set_follow_codex(&mut self, enabled: bool) -> Result<(), AppError> {
        if self.config.follow_codex == enabled {
            return Ok(());
        }
        if enabled {
            self.start_presence_watcher()?;
            self.config.follow_codex = true;
            self.deactivate_overlay();
        } else {
            self.config.follow_codex = false;
            self.stop_presence_watcher();
            self.activate_overlay()?;
        }
        self.save_config();
        Ok(())
    }

    fn handle_presence_changed(&mut self, present: bool, generation: u32) -> Result<(), AppError> {
        if !self.config.follow_codex || generation != self.presence_generation {
            return Ok(());
        }
        if self.codex_present == present {
            return Ok(());
        }
        if present {
            self.activate_overlay()?;
        } else {
            self.deactivate_overlay();
        }
        self.codex_present = present;
        Ok(())
    }

    fn show(&mut self) -> Result<(), AppError> {
        if !self.overlay_active {
            return Ok(());
        }
        // 只有"从隐藏到可见"才算球重新出现。单实例唤醒路径（第二次启动进程
        // 会 PostMessage WM_APP_SHOW）在球已经可见时走到这里，不应重扫一遍。
        let appearing = !self.visible;
        self.visible = true;
        if appearing && let Some(renderer) = self.renderer.as_mut() {
            renderer.restart_ring_sweep();
        }
        // SAFETY: no-activate show leaves focus with the current foreground window.
        let _ = unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
        self.ensure_topmost("重新显示悬浮窗")?;
        let result = self.resize_for_state();
        self.update_outside_click_hook();
        result
    }

    fn toggle_visibility(&mut self) -> Result<(), AppError> {
        if self.visible {
            self.finish_animation()?;
            self.visible = false;
            self.stop_ring_frames();
            self.update_outside_click_hook();
            // SAFETY: hwnd is live.
            let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            Ok(())
        } else {
            self.show()
        }
    }

    /// 点通知气泡：先确保球可见，再展开面板。
    fn show_expanded(&mut self) -> Result<(), AppError> {
        self.show()?;
        self.set_expanded(true)
    }

    /// 每次收到新快照时判一次通知：判定与去重都交给 `Notifier`，这里只负责
    /// 取状态、弹气泡、落盘。
    fn check_notifications(&mut self) {
        let (snapshot, overflow_credits) = match self.state.lock() {
            Ok(current) => (
                current.snapshot.clone(),
                current.current_period_overflow_credits,
            ),
            Err(_) => return,
        };
        let switches = NotificationSwitches {
            reset: self.config.notify_on_reset,
            overflow: self.config.notify_on_overflow,
        };
        let outcome = self.notifier.observe(
            &mut self.notify_state,
            snapshot.as_ref(),
            overflow_credits,
            switches,
            self.config.quota_refresh_interval(),
            SystemTime::now(),
        );
        if outcome.state_dirty {
            crate::notify_state::save(&self.notify_state);
        }
        for notification in outcome.notifications {
            self.show_notification(&notification);
        }
    }

    /// 弹一条通知气泡。判定与去重都在 `Notifier` 里，这里只管投递，
    /// 因此调试入口可以复用它而不碰任何判定状态。
    fn show_notification(&self, notification: &Notification) {
        let (title, body) = notification.balloon_text();
        crate::logging::log(&format!("通知：{title} / {body}"));
        if let Err(error) = notify::show_balloon(self.hwnd, title, &body) {
            crate::logging::log(&error.to_string());
        }
    }

    fn apply_topmost(&self) -> Result<(), AppError> {
        let insert_after = if self.config.always_on_top {
            Some(windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST)
        } else {
            Some(windows::Win32::UI::WindowsAndMessaging::HWND_NOTOPMOST)
        };
        // SAFETY: this changes only z-order and does not activate or move the window.
        unsafe {
            SetWindowPos(
                self.hwnd,
                insert_after,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            )?;
        }
        Ok(())
    }

    fn ensure_topmost(&self, context: &str) -> Result<(), AppError> {
        if !self.config.always_on_top {
            return Ok(());
        }
        // SAFETY: hwnd is a live top-level window owned by this UI thread.
        let extended_style = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) };
        if extended_style & WS_EX_TOPMOST.0 as isize != 0 {
            return Ok(());
        }
        crate::logging::log(&format!("检测到始终置顶状态丢失（{context}），正在恢复"));
        self.apply_topmost()
    }

    fn add_tray_icon(&mut self) -> Result<(), AppError> {
        let icon = load_app_icon()?;
        let mut data = NOTIFYICONDATAW {
            cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>())
                .map_err(|_| AppError::Windows("托盘结构大小溢出".to_owned()))?,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            uFlags: tray_icon_flags(),
            uCallbackMessage: WM_APP_TRAY,
            hIcon: icon,
            dwInfoFlags: NIIF_NONE,
            ..Default::default()
        };
        copy_wide_fixed("Codex Quota", &mut data.szTip);
        // SAFETY: data is fully initialized and remains alive for both shell calls.
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            return Err(AppError::Windows("无法添加托盘图标".to_owned()));
        }
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: the icon identified by HWND, ID, and GUID was just added.
        self.tray_uses_v4 = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool();
        self.tray_added = true;
        Ok(())
    }

    fn remove_tray_icon(&mut self) {
        if !self.tray_added {
            return;
        }
        let data = NOTIFYICONDATAW {
            cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        };
        // SAFETY: this removes only the icon identified by this window and its fixed ID.
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
        self.tray_added = false;
        self.tray_uses_v4 = false;
    }

    fn handle_taskbar_created(&mut self) {
        self.tray_added = false;
        if let Err(error) = self.add_tray_icon() {
            crate::logging::log(&error.to_string());
        }
        if let Err(error) = self.ensure_topmost("任务栏重建") {
            crate::logging::log(&error.to_string());
        }
    }

    fn command(&mut self, command: usize) -> Result<(), AppError> {
        if let Some(interval) = refresh_interval_for_command(command) {
            self.config.set_quota_refresh_interval(interval);
            if let Ok(mut state) = self.state.lock() {
                state.quota_refresh_interval = self.config.quota_refresh_interval();
            }
            if let Some(worker) = &self.worker {
                worker.refresh_interval_changed();
            }
            self.save_config();
            return self.render();
        }

        // 调试构建专供：手动弹一条通知，仅供验证投递（声音/点击/应用名）。
        #[cfg(debug_assertions)]
        if let Some(notification) = notify::test_notification_for_command(command) {
            self.show_notification(&notification);
            return Ok(());
        }

        match command {
            CMD_SHOW if !self.config.follow_codex => self.toggle_visibility(),
            CMD_REFRESH => {
                if let Some(worker) = &self.worker {
                    worker.refresh();
                    worker.refresh_prices();
                }
                Ok(())
            }
            CMD_TOPMOST => {
                self.config.always_on_top = !self.config.always_on_top;
                self.apply_topmost()?;
                self.save_config();
                Ok(())
            }
            CMD_NOTIFY_RESET => {
                self.config.notify_on_reset = !self.config.notify_on_reset;
                self.save_config();
                Ok(())
            }
            CMD_NOTIFY_OVERFLOW => {
                self.config.notify_on_overflow = !self.config.notify_on_overflow;
                self.save_config();
                Ok(())
            }
            CMD_AUTOSTART => {
                let new_value = !self.config.start_with_windows;
                set_autostart(new_value)?;
                self.config.start_with_windows = new_value;
                self.save_config();
                Ok(())
            }
            CMD_PANEL_PERSISTENT => {
                self.config.collapse_on_outside_click = !self.config.collapse_on_outside_click;
                self.update_outside_click_hook();
                self.save_config();
                Ok(())
            }
            CMD_FOLLOW_CODEX => self.set_follow_codex(!self.config.follow_codex),
            CMD_EXIT => {
                // SAFETY: requests normal teardown of this live window.
                unsafe { DestroyWindow(self.hwnd)? };
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(super) fn save_config(&self) {
        if let Err(error) = config::save(&self.config) {
            crate::logging::log(&error.to_string());
        }
    }
}

impl Drop for AppWindow {
    fn drop(&mut self) {
        self.stop_presence_watcher();
        self.remove_outside_click_hook();
        self.remove_tray_icon();
        if !self.hwnd.is_invalid() {
            // SAFETY: best-effort cleanup during HWND teardown.
            let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_REDRAW) };
            // SAFETY: best-effort cleanup of the fixed animation timer during HWND teardown.
            let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_ANIMATION) };
            // SAFETY: best-effort cleanup of the fixed ring animation timer during HWND teardown.
            let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_RING) };
            let _ = unsafe { ReleaseCapture() };
        }
        if let Some(mut worker) = self.worker.take() {
            worker.shutdown();
        }
    }
}

pub(super) unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: WM_NCCREATE lParam is a CREATESTRUCTW supplied synchronously by CreateWindowExW.
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        let app_ptr = create.lpCreateParams.cast::<AppWindow>();
        // SAFETY: the pointer originated from Box::into_raw and remains HWND-owned until WM_NCDESTROY.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize) };
    }
    // SAFETY: GWLP_USERDATA is either null or the Box pointer installed above.
    let app_ptr =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA) }
            as *mut AppWindow;
    if app_ptr.is_null() {
        // SAFETY: default processing is required before our instance pointer is installed.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }

    if message == WM_APP_TRAY {
        // No AppWindow reference is held across TrackPopupMenu: its modal loop can re-enter this
        // window procedure while the menu is open.
        let result = unsafe { handle_tray_message(app_ptr, lparam) };
        if let Err(error) = result {
            crate::logging::log(&error.to_string());
        }
        return LRESULT(0);
    }
    // SAFETY: the pointer has unique UI-thread access for the duration of this dispatch.
    let app = unsafe { &mut *app_ptr };

    if message == app.taskbar_created && app.taskbar_created != 0 {
        app.handle_taskbar_created();
        return LRESULT(0);
    }

    let result = match message {
        WM_APP_UPDATED => {
            if app.overlay_active {
                // 新快照到手：先判两条通知，再决定怎么重绘。
                app.check_notifications();
                if app.animation.is_some() || !app.expanded {
                    app.render()
                } else {
                    app.resize_for_state()
                }
            } else {
                Ok(())
            }
        }
        WM_APP_SHOW if !app.config.follow_codex => app.show(),
        // 点通知气泡展开面板：跟随模式下同样允许——能收到通知就说明球当时在，
        // 而 Codex 已退出时 overlay 不活跃，show/set_expanded 自己会空转。
        WM_APP_EXPAND => app.show_expanded(),
        WM_APP_COLLAPSE => app.set_expanded(false),
        WM_APP_PRESENCE_CHANGED => app.handle_presence_changed(wparam.0 != 0, lparam.0 as u32),
        WM_COMMAND => app.command(wparam.0 & 0xffff),
        WM_LBUTTONDOWN => {
            app.begin_drag();
            Ok(())
        }
        WM_MOUSEMOVE => app.update_drag(),
        WM_LBUTTONUP => app.end_drag(),
        WM_NCHITTEST => return app.hit_test(),
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_DPICHANGED => handle_window_dpi_changed(app, wparam, lparam),
        WM_DISPLAYCHANGE if app.overlay_active => app
            .finish_animation()
            .and_then(|()| app.snap_to_edge())
            .and_then(|()| app.ensure_topmost("显示器配置变化")),
        WM_SETTINGCHANGE => {
            app.animations_enabled = system_animations_enabled();
            // 面板动画立即收尾；球动效的开关在下一帧被推送，因此这里必须重绘，
            // 否则关闭动画后画面会停在补间或脉冲的中间帧上。
            app.finish_animation().and_then(|()| app.render())
        }
        WM_POWERBROADCAST if wparam.0 == PBT_APMRESUMEAUTOMATIC as usize => {
            if let Some(watcher) = &app.presence_watcher {
                watcher.recheck();
            }
            if let Some(worker) = &app.worker {
                worker.refresh();
            }
            app.ensure_topmost("系统唤醒")
        }
        WM_TIMER if wparam.0 == TIMER_REDRAW => {
            app.ensure_topmost("定时检查").and_then(|()| app.render())
        }
        WM_TIMER if wparam.0 == TIMER_ANIMATION && app.overlay_active => {
            app.render_animation_frame(Instant::now())
        }
        WM_TIMER if wparam.0 == TIMER_ANIMATION => Ok(()),
        WM_TIMER if wparam.0 == TIMER_RING && app.overlay_active => app.render(),
        WM_TIMER if wparam.0 == TIMER_RING => Ok(()),
        WM_CLOSE if !app.config.follow_codex => app.toggle_visibility(),
        WM_APP_SHOW | WM_DISPLAYCHANGE | WM_CLOSE => Ok(()),
        WM_DESTROY => {
            // SAFETY: stops the message loop after the window has begun normal destruction.
            unsafe { PostQuitMessage(0) };
            Ok(())
        }
        WM_NCDESTROY => {
            // SAFETY: clear HWND storage first so no later default message can reuse the pointer.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            // SAFETY: this exactly reconstructs the Box created by Box::into_raw in run().
            unsafe { drop(Box::from_raw(app_ptr)) };
            // SAFETY: allow the window manager to finish non-client teardown.
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
        _ => {
            // SAFETY: unhandled messages retain standard window behavior.
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        }
    };
    if let Err(error) = result {
        crate::logging::log(&error.to_string());
    }
    LRESULT(0)
}

fn handle_window_dpi_changed(
    app: &mut AppWindow,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Result<(), AppError> {
    let dpi = u32::try_from(wparam.0 & 0xffff).unwrap_or(96);
    if app.overlay_active {
        app.handle_dpi_changed(dpi, lparam.0 as *const RECT)
    } else {
        app.dpi = dpi.max(96);
        Ok(())
    }
}
