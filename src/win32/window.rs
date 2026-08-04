use std::ffi::c_void;
use std::mem::size_of;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::Shell::{
    NIIF_NONE, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, KillTimer, MA_NOACTIVATE,
    PBT_APMRESUMEAUTOMATIC, PostMessageW, PostQuitMessage, RegisterWindowMessageW, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetTimer, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY,
    WM_NCHITTEST, WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_TIMER,
};
use windows::core::w;

use super::layout::{ExpansionAlignment, system_animations_enabled};
use super::presence::CodexPresenceWatcher;
use super::renderer::Renderer;
use super::system::{load_app_icon, set_autostart};
use super::tray::{
    copy_wide_fixed, handle_tray_message, refresh_interval_for_command, tray_icon_flags,
};
use super::{
    AppWindow, CMD_AUTOSTART, CMD_EXIT, CMD_FOLLOW_CODEX, CMD_PANEL_PERSISTENT, CMD_REFRESH,
    CMD_SHOW, CMD_TOPMOST, TIMER_ANIMATION, TIMER_REDRAW, TRAY_ID, WM_APP_COLLAPSE,
    WM_APP_PRESENCE_CHANGED, WM_APP_SHOW, WM_APP_TRAY, WM_APP_UPDATED,
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
        self.worker = Some(CodexWorker::spawn(Arc::clone(&self.state), move || {
            let target = HWND(notify_hwnd as *mut c_void);
            // SAFETY: posting by value is safe even if shutdown races; failure is intentionally ignored.
            let _ = unsafe { PostMessageW(Some(target), WM_APP_UPDATED, WPARAM(0), LPARAM(0)) };
        }));
        if let Err(error) = self.resize_for_state() {
            self.deactivate_overlay();
            return Err(error);
        }
        self.visible = true;
        // SAFETY: no-activate show leaves focus with the current foreground window.
        let _ = unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
        Ok(())
    }

    fn deactivate_overlay(&mut self) {
        if !self.overlay_active {
            return;
        }
        self.visible = false;
        self.remove_outside_click_hook();
        self.stop_animation_timer();
        self.animation = None;
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
        self.visible = true;
        // SAFETY: no-activate show leaves focus with the current foreground window.
        let _ = unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
        let result = self.resize_for_state();
        self.update_outside_click_hook();
        result
    }

    fn toggle_visibility(&mut self) -> Result<(), AppError> {
        if self.visible {
            self.finish_animation()?;
            self.visible = false;
            self.update_outside_click_hook();
            // SAFETY: hwnd is live.
            let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            Ok(())
        } else {
            self.show()
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

        match command {
            CMD_SHOW if !self.config.follow_codex => self.toggle_visibility(),
            CMD_REFRESH => {
                if let Some(worker) = &self.worker {
                    worker.refresh();
                }
                Ok(())
            }
            CMD_TOPMOST => {
                self.config.always_on_top = !self.config.always_on_top;
                self.apply_topmost()?;
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
        app.tray_added = false;
        if let Err(error) = app.add_tray_icon() {
            crate::logging::log(&error.to_string());
        }
        return LRESULT(0);
    }

    let result = match message {
        WM_APP_UPDATED => {
            if !app.overlay_active {
                Ok(())
            } else if app.animation.is_some() {
                app.render()
            } else if app.expanded {
                app.resize_for_state()
            } else {
                app.render()
            }
        }
        WM_APP_SHOW if !app.config.follow_codex => app.show(),
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
        WM_DISPLAYCHANGE if app.overlay_active => {
            app.finish_animation().and_then(|()| app.snap_to_edge())
        }
        WM_SETTINGCHANGE => {
            app.animations_enabled = system_animations_enabled();
            if app.animations_enabled {
                Ok(())
            } else {
                app.finish_animation()
            }
        }
        WM_POWERBROADCAST if wparam.0 == PBT_APMRESUMEAUTOMATIC as usize => {
            if let Some(watcher) = &app.presence_watcher {
                watcher.recheck();
            }
            if let Some(worker) = &app.worker {
                worker.refresh();
            }
            Ok(())
        }
        WM_TIMER if wparam.0 == TIMER_REDRAW => app.render(),
        WM_TIMER if wparam.0 == TIMER_ANIMATION && app.overlay_active => {
            app.render_animation_frame(Instant::now())
        }
        WM_TIMER if wparam.0 == TIMER_ANIMATION => Ok(()),
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
