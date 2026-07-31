#![allow(
    clippy::borrow_as_ptr,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::default_trait_access,
    clippy::items_after_statements,
    clippy::struct_excessive_bools
)]

mod renderer;

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use renderer::{Renderer, TransitionVisual, VisualState};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HANDLE, HINSTANCE, HWND,
    LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MONITORINFOEXW, MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegSetValueExW,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::Shell::{
    NIF_GUID, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIIF_NONE, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CallNextHookEx, CreatePopupMenu,
    CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW, EndMenu,
    FindWindowW, GWLP_USERDATA, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowRect, HHOOK, HICON, HMENU, HTCLIENT, HTTRANSPARENT, IDC_ARROW, KillTimer, LoadCursorW,
    LoadIconW, MA_NOACTIVATE, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, MSLLHOOKSTRUCT,
    PBT_APMRESUMEAUTOMATIC, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SM_CXDRAG, SM_CYDRAG, SPI_GETCLIENTAREAANIMATION, SW_HIDE,
    SW_SHOWNOACTIVATE, SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    SetWindowPos, SetWindowsHookExW, ShowWindow, SystemParametersInfoW, TPM_BOTTOMALIGN,
    TPM_RIGHTALIGN, TrackPopupMenu, UnhookWindowsHookEx, WH_MOUSE_LL, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST,
    WM_POWERBROADCAST, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WM_XBUTTONDOWN,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{GUID, PCWSTR, w};

use crate::config::{self, AnchorEdge, AppConfigV1};
use crate::error::AppError;
use crate::quota::{AppState, CodexWorker};

const CLASS_NAME: PCWSTR = w!("CodexQuotaOverlayWindow");
const WINDOW_NAME: PCWSTR = w!("Codex Quota");
const MUTEX_NAME: PCWSTR = w!("Local\\CodexQuota.Singleton");
const WM_APP_UPDATED: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const WM_APP_SHOW: u32 = WM_APP + 3;
const WM_APP_COLLAPSE: u32 = WM_APP + 4;
const TIMER_REDRAW: usize = 1;
const TIMER_ANIMATION: usize = 2;
const ANIMATION_FRAME_MILLIS: u32 = 16;
const EXPAND_ANIMATION_DURATION: Duration = Duration::from_millis(160);
const COLLAPSE_ANIMATION_DURATION: Duration = Duration::from_millis(130);
const TRAY_ID: u32 = 1;
const APP_ICON_RESOURCE_ID: usize = 1;
const TRAY_GUID: GUID = GUID::from_u128(0x8b7c4c57_30cc_4c8d_a114_8764698645c1);
const CMD_SHOW: usize = 1001;
const CMD_REFRESH: usize = 1002;
const CMD_USAGE: usize = 1003;
const CMD_TOPMOST: usize = 1004;
const CMD_AUTOSTART: usize = 1005;
const CMD_EXIT: usize = 1006;
const CMD_PANEL_PERSISTENT: usize = 1007;
const CMD_REFRESH_1_MIN: usize = 1008;
const CMD_REFRESH_5_MIN: usize = 1009;
const CMD_REFRESH_10_MIN: usize = 1010;
const CMD_REFRESH_30_MIN: usize = 1011;
const COLLAPSED_DIP: f32 = 56.0;
const PANEL_WIDTH_DIP: f32 = 276.0;
const PANEL_HEIGHT_DIP: f32 = 148.0;
const TRAY_REOPEN_GUARD: Duration = Duration::from_millis(500);
static OUTSIDE_CLICK_HWND: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ExpansionAlignment {
    #[default]
    Start,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayEventAction {
    OpenContextMenu,
    Ignore,
}

#[derive(Clone, Copy, Debug)]
struct PanelAnimation {
    started_at: Instant,
    duration: Duration,
    anchor_rect: RECT,
    work_area: RECT,
    canvas_destination: POINT,
    ball_center_screen: POINT,
    expanding: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct AnimationSample {
    expansion: f32,
    ball_opacity: f32,
    panel_opacity: f32,
    finished: bool,
}

impl PanelAnimation {
    fn sample(self, now: Instant) -> AnimationSample {
        let elapsed = now.saturating_duration_since(self.started_at);
        let timeline = (elapsed.as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0);
        let mut sample = animation_sample(self.expanding, timeline);
        sample.finished = elapsed >= self.duration;
        sample
    }
}

/// Runs the Windows UI message loop until the user exits from the tray menu.
///
/// # Errors
///
/// Returns an error when the process cannot initialize required Win32, rendering, configuration,
/// or tray resources.
pub fn run() -> Result<(), AppError> {
    let app_dir = config::app_data_dir()?;
    crate::logging::init(&app_dir);
    // SAFETY: the process manifest already declares Per-Monitor V2. This call is an idempotent
    // fallback for development launches where a manifest may not have been embedded yet.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let instance = SingleInstance::acquire()?;
    if !instance.is_primary {
        // SAFETY: this is a best-effort lookup of our registered top-level window class.
        if let Ok(hwnd) = unsafe { FindWindowW(CLASS_NAME, PCWSTR::null()) } {
            // SAFETY: the message carries no pointer data and targets the existing instance.
            let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_SHOW, WPARAM(0), LPARAM(0)) };
        }
        return Ok(());
    }
    crate::logging::log("应用已启动");

    // SAFETY: a null module name requests the current executable module.
    let module = unsafe { GetModuleHandleW(None) }?;
    let hinstance = HINSTANCE(module.0);
    register_window_class(hinstance)?;

    let config = config::load().unwrap_or_else(|error| {
        crate::logging::log(&error.to_string());
        AppConfigV1::default()
    });
    let state = Arc::new(Mutex::new(AppState {
        quota_refresh_interval: config.quota_refresh_interval(),
        ..AppState::default()
    }));
    let initial_monitor =
        monitor_for_device(&config.placement.monitor_device).unwrap_or_else(primary_monitor);
    let dpi = 96;
    let initial_size = dip_to_px(COLLAPSED_DIP, dpi);
    let monitor_info = monitor_info(initial_monitor)?;
    let initial_position = position_from_config(&config, &monitor_info, initial_size, dpi);
    let app = Box::new(AppWindow::new(config, state));
    let app_ptr = Box::into_raw(app);

    // SAFETY: app_ptr remains owned by the HWND until WM_NCDESTROY reconstructs and drops the Box.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            CLASS_NAME,
            WINDOW_NAME,
            WS_POPUP,
            initial_position.x,
            initial_position.y,
            initial_size,
            initial_size,
            None,
            None,
            Some(hinstance),
            Some(app_ptr.cast()),
        )
    };
    let hwnd = match hwnd {
        Ok(hwnd) => hwnd,
        Err(error) => {
            // SAFETY: CreateWindowExW failed, so the window never took ownership of app_ptr.
            unsafe { drop(Box::from_raw(app_ptr)) };
            return Err(error.into());
        }
    };

    // SAFETY: the pointer was installed during WM_NCCREATE and stays valid until WM_NCDESTROY.
    // SAFETY: the HWND owns app_ptr until normal destruction.
    let initialization = unsafe { &mut *app_ptr }
        .initialize(hwnd)
        .and_then(|()| unsafe { &mut *app_ptr }.resize_for_state());
    if let Err(error) = initialization {
        // SAFETY: releases HWND ownership of app_ptr through WM_NCDESTROY before returning.
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err(error);
    }
    // SAFETY: the no-activate show mode preserves the current foreground window.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    // SAFETY: initialization succeeded and the HWND remains live.
    unsafe { &mut *app_ptr }.render()?;

    let mut message = MSG::default();
    // SAFETY: standard UI-thread message loop; message points to initialized writable storage.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        // SAFETY: DispatchMessageW consumes the message produced by GetMessageW.
        unsafe { DispatchMessageW(&message) };
    }
    drop(instance);
    Ok(())
}

struct AppWindow {
    hwnd: HWND,
    config: AppConfigV1,
    state: Arc<Mutex<AppState>>,
    worker: Option<CodexWorker>,
    renderer: Option<Renderer>,
    outside_click_hook: Option<HHOOK>,
    animation: Option<PanelAnimation>,
    animations_enabled: bool,
    expanded: bool,
    expansion_alignment: ExpansionAlignment,
    visible: bool,
    dragging: bool,
    pointer_down: bool,
    drag_cursor_origin: POINT,
    drag_window_origin: POINT,
    dpi: u32,
    taskbar_created: u32,
    tray_added: bool,
    tray_uses_v4: bool,
    tray_menu_open: bool,
    last_tray_menu_closed: Option<Instant>,
}

impl AppWindow {
    fn new(config: AppConfigV1, state: Arc<Mutex<AppState>>) -> Self {
        Self {
            hwnd: HWND::default(),
            config,
            state,
            worker: None,
            renderer: None,
            outside_click_hook: None,
            animation: None,
            animations_enabled: true,
            expanded: false,
            expansion_alignment: ExpansionAlignment::Start,
            visible: true,
            dragging: false,
            pointer_down: false,
            drag_cursor_origin: POINT::default(),
            drag_window_origin: POINT::default(),
            dpi: 96,
            taskbar_created: 0,
            tray_added: false,
            tray_uses_v4: false,
            tray_menu_open: false,
            last_tray_menu_closed: None,
        }
    }

    fn initialize(&mut self, hwnd: HWND) -> Result<(), AppError> {
        self.hwnd = hwnd;
        self.animations_enabled = system_animations_enabled();
        // SAFETY: hwnd is a newly-created live window on this UI thread.
        self.dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let (width, height) = self.desired_size();
        self.renderer = Some(Renderer::new(width, height, self.dpi)?);
        // SAFETY: the registered message contains no pointer payload.
        self.taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        self.add_tray_icon()?;
        // SAFETY: HWND timer posts WM_TIMER and is removed during destruction.
        if unsafe { SetTimer(Some(hwnd), TIMER_REDRAW, 30_000, None) } == 0 {
            return Err(AppError::Windows("无法创建界面刷新计时器".to_owned()));
        }
        self.apply_topmost()?;

        let notify_hwnd = hwnd.0 as usize;
        self.worker = Some(CodexWorker::spawn(Arc::clone(&self.state), move || {
            let target = HWND(notify_hwnd as *mut c_void);
            // SAFETY: posting by value is safe even if shutdown races; failure is intentionally ignored.
            let _ = unsafe { PostMessageW(Some(target), WM_APP_UPDATED, WPARAM(0), LPARAM(0)) };
        }));
        Ok(())
    }

    fn desired_size(&self) -> (i32, i32) {
        if self.expanded {
            (
                dip_to_px(PANEL_WIDTH_DIP, self.dpi),
                dip_to_px(PANEL_HEIGHT_DIP, self.dpi),
            )
        } else {
            let side = dip_to_px(COLLAPSED_DIP, self.dpi);
            (side, side)
        }
    }

    fn render(&mut self) -> Result<(), AppError> {
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

    fn render_at(&mut self, destination: POINT, width: i32, height: i32) -> Result<(), AppError> {
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
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.resize(width, height, self.dpi)?;
            let state = self
                .state
                .lock()
                .map_err(|_| AppError::Render("额度状态锁已损坏".to_owned()))?
                .clone();
            renderer.render(self.hwnd, destination, visual, &state)?;
        }
        Ok(())
    }

    fn render_animation_frame(&mut self, now: Instant) -> Result<(), AppError> {
        let Some(animation) = self.animation else {
            return self.render();
        };
        let sample = animation.sample(now);
        let shape = animation_shape_rect(
            animation,
            sample.expansion,
            self.dpi,
            self.config.placement.edge,
            self.expansion_alignment,
        );
        let width = dip_to_px(PANEL_WIDTH_DIP, self.dpi);
        let height = dip_to_px(PANEL_HEIGHT_DIP, self.dpi);
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

    fn resize_for_state(&mut self) -> Result<(), AppError> {
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

    fn toggle_expanded(&mut self) -> Result<(), AppError> {
        self.set_expanded(!self.expanded)
    }

    fn set_expanded(&mut self, expanded: bool) -> Result<(), AppError> {
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
            let height = dip_to_px(PANEL_HEIGHT_DIP, self.dpi);
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

    fn finish_animation(&mut self) -> Result<(), AppError> {
        if self.animation.is_none() {
            return Ok(());
        }
        self.stop_animation_timer();
        self.animation = None;
        let result = self.resize_for_state();
        self.update_outside_click_hook();
        result
    }

    fn stop_animation_timer(&self) {
        if !self.hwnd.is_invalid() {
            // SAFETY: removes only this window's fixed animation timer ID.
            let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_ANIMATION) };
        }
    }

    fn update_outside_click_hook(&mut self) {
        let should_install = self.visible
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

    fn remove_outside_click_hook(&mut self) {
        OUTSIDE_CLICK_HWND.store(0, Ordering::Release);
        let Some(hook) = self.outside_click_hook.take() else {
            return;
        };
        // SAFETY: hook is uniquely owned by this AppWindow and removed at most once.
        if let Err(error) = unsafe { UnhookWindowsHookEx(hook) } {
            crate::logging::log(&format!("无法关闭点击外部收起：{error}"));
        }
    }

    fn begin_drag(&mut self) {
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

    fn update_drag(&mut self) -> Result<(), AppError> {
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
                SWP_NOACTIVATE | SWP_NOSIZE,
            )?;
        }
        self.render()
    }

    fn end_drag(&mut self) -> Result<(), AppError> {
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

    fn snap_to_edge(&mut self) -> Result<(), AppError> {
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
            SetWindowPos(self.hwnd, None, x, y, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE)?;
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

    fn handle_dpi_changed(&mut self, new_dpi: u32, suggested: *const RECT) -> Result<(), AppError> {
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

    fn show(&mut self) -> Result<(), AppError> {
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
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_GUID,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: icon,
            guidItem: TRAY_GUID,
            dwInfoFlags: NIIF_NONE,
            ..Default::default()
        };
        copy_wide_fixed("Codex 额度", &mut data.szTip);
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
            uFlags: NIF_GUID,
            guidItem: TRAY_GUID,
            ..Default::default()
        };
        // SAFETY: this removes only our fixed-GUID icon.
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
            CMD_SHOW => self.toggle_visibility(),
            CMD_REFRESH => {
                if let Some(worker) = &self.worker {
                    worker.refresh();
                }
                Ok(())
            }
            CMD_USAGE => {
                // SAFETY: all strings are static NUL-terminated literals. The result is best-effort.
                let _ = unsafe {
                    ShellExecuteW(
                        Some(self.hwnd),
                        w!("open"),
                        w!("https://chatgpt.com/codex/settings/usage"),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        SW_SHOWNORMAL,
                    )
                };
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
            CMD_EXIT => {
                // SAFETY: requests normal teardown of this live window.
                unsafe { DestroyWindow(self.hwnd)? };
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn save_config(&self) {
        if let Err(error) = config::save(&self.config) {
            crate::logging::log(&error.to_string());
        }
    }

    fn hit_test(&self) -> LRESULT {
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

impl Drop for AppWindow {
    fn drop(&mut self) {
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

struct PopupMenu(HMENU);

#[derive(Clone, Copy)]
struct TrayMenuState {
    visible: bool,
    always_on_top: bool,
    start_with_windows: bool,
    collapse_on_outside_click: bool,
    quota_refresh_interval_secs: u64,
}

impl PopupMenu {
    fn create() -> Result<Self, AppError> {
        // SAFETY: the returned menu is owned by this guard and destroyed in Drop.
        Ok(Self(unsafe { CreatePopupMenu() }?))
    }
}

impl Drop for PopupMenu {
    fn drop(&mut self) {
        // SAFETY: this guard uniquely owns the popup menu handle.
        let _ = unsafe { DestroyMenu(self.0) };
    }
}

unsafe fn handle_tray_message(app_ptr: *mut AppWindow, lparam: LPARAM) -> Result<(), AppError> {
    let event = lparam.0 as u32 & 0xffff;
    // SAFETY: app_ptr is the live Box pointer stored in GWLP_USERDATA on the UI thread.
    let uses_v4 = unsafe { (*app_ptr).tray_uses_v4 };
    if tray_event_action(event, uses_v4) == TrayEventAction::OpenContextMenu {
        // SAFETY: the field is read only on this UI thread, including modal-loop re-entry.
        if unsafe { (*app_ptr).tray_menu_open } {
            // SAFETY: called only while this thread has an active TrackPopupMenu modal loop.
            unsafe { EndMenu()? };
            return Ok(());
        }

        let now = Instant::now();
        // SAFETY: Instant is Copy and the field is UI-thread-owned.
        let last_closed = unsafe { (*app_ptr).last_tray_menu_closed };
        if last_closed.is_some_and(|closed| should_suppress_tray_reopen(closed, now)) {
            // SAFETY: consume only the duplicate callback suppression on the UI thread.
            unsafe { (*app_ptr).last_tray_menu_closed = None };
            return Ok(());
        }

        // Copy all display state before entering the modal loop, so no AppWindow reference crosses
        // the re-entrant TrackPopupMenu call.
        let (hwnd, menu_state) = unsafe {
            (
                (*app_ptr).hwnd,
                TrayMenuState {
                    visible: (*app_ptr).visible,
                    always_on_top: (*app_ptr).config.always_on_top,
                    start_with_windows: (*app_ptr).config.start_with_windows,
                    collapse_on_outside_click: (*app_ptr).config.collapse_on_outside_click,
                    quota_refresh_interval_secs: (*app_ptr)
                        .config
                        .quota_refresh_interval()
                        .as_secs(),
                },
            )
        };
        // SAFETY: the flag is UI-thread-owned and intentionally visible to nested tray callbacks.
        unsafe { (*app_ptr).tray_menu_open = true };
        let result = display_tray_menu(hwnd, menu_state);
        // SAFETY: TrackPopupMenu has returned, so the modal-loop state can be cleared.
        unsafe {
            (*app_ptr).tray_menu_open = false;
            (*app_ptr).last_tray_menu_closed = Some(Instant::now());
        }
        result
    } else {
        Ok(())
    }
}

fn display_tray_menu(hwnd: HWND, state: TrayMenuState) -> Result<(), AppError> {
    let mut cursor = POINT::default();
    // SAFETY: cursor is initialized writable storage.
    unsafe { GetCursorPos(&mut cursor)? };
    let menu = PopupMenu::create()?;
    let refresh_menu = PopupMenu::create()?;
    let show = wide(if state.visible {
        "隐藏悬浮球"
    } else {
        "显示悬浮球"
    });
    let refresh = wide("立即刷新");
    let refresh_interval = wide("额度刷新间隔");
    let one_minute_label = wide("1 分钟");
    let five_minute_label = wide("5 分钟");
    let ten_minute_label = wide("10 分钟");
    let thirty_minute_label = wide("30 分钟");
    let usage = wide("打开官方 Usage 页面");
    let topmost = wide("始终置顶");
    let autostart = wide("开机启动");
    let panel_persistent = wide("面板常驻");
    let exit = wide("退出");
    // SAFETY: menu is valid and each string is NUL-terminated for the duration of appending.
    unsafe {
        for (command, seconds, label) in [
            (CMD_REFRESH_1_MIN, 60, &one_minute_label),
            (CMD_REFRESH_5_MIN, 5 * 60, &five_minute_label),
            (CMD_REFRESH_10_MIN, 10 * 60, &ten_minute_label),
            (CMD_REFRESH_30_MIN, 30 * 60, &thirty_minute_label),
        ] {
            AppendMenuW(
                refresh_menu.0,
                MF_STRING
                    | if state.quota_refresh_interval_secs == seconds {
                        MF_CHECKED
                    } else {
                        Default::default()
                    },
                command,
                PCWSTR(label.as_ptr()),
            )?;
        }
        AppendMenuW(menu.0, MF_STRING, CMD_SHOW, PCWSTR(show.as_ptr()))?;
        AppendMenuW(menu.0, MF_STRING, CMD_REFRESH, PCWSTR(refresh.as_ptr()))?;
        AppendMenuW(
            menu.0,
            MF_POPUP,
            refresh_menu.0.0 as usize,
            PCWSTR(refresh_interval.as_ptr()),
        )?;
        // SAFETY: after successful attachment, the parent menu owns and destroys the submenu.
        std::mem::forget(refresh_menu);
        AppendMenuW(menu.0, MF_STRING, CMD_USAGE, PCWSTR(usage.as_ptr()))?;
        AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(
            menu.0,
            MF_STRING
                | if state.always_on_top {
                    MF_CHECKED
                } else {
                    Default::default()
                },
            CMD_TOPMOST,
            PCWSTR(topmost.as_ptr()),
        )?;
        AppendMenuW(
            menu.0,
            MF_STRING
                | if state.start_with_windows {
                    MF_CHECKED
                } else {
                    Default::default()
                },
            CMD_AUTOSTART,
            PCWSTR(autostart.as_ptr()),
        )?;
        AppendMenuW(
            menu.0,
            MF_STRING
                | if panel_is_persistent(state.collapse_on_outside_click) {
                    MF_CHECKED
                } else {
                    Default::default()
                },
            CMD_PANEL_PERSISTENT,
            PCWSTR(panel_persistent.as_ptr()),
        )?;
        AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(menu.0, MF_STRING, CMD_EXIT, PCWSTR(exit.as_ptr()))?;

        // Foreground ownership is required by TrackPopupMenu for correct dismissal.
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            menu.0,
            TPM_RIGHTALIGN | TPM_BOTTOMALIGN,
            cursor.x,
            cursor.y,
            None,
            hwnd,
            None,
        );
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
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
    let app_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut AppWindow;
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
            if app.animation.is_some() {
                app.render()
            } else if app.expanded {
                app.resize_for_state()
            } else {
                app.render()
            }
        }
        WM_APP_SHOW => app.show(),
        WM_APP_COLLAPSE => app.set_expanded(false),
        WM_COMMAND => app.command(wparam.0 & 0xffff),
        WM_LBUTTONDOWN => {
            app.begin_drag();
            Ok(())
        }
        WM_MOUSEMOVE => app.update_drag(),
        WM_LBUTTONUP => app.end_drag(),
        WM_NCHITTEST => return app.hit_test(),
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_DPICHANGED => {
            let dpi = u32::try_from(wparam.0 & 0xffff).unwrap_or(96);
            app.handle_dpi_changed(dpi, lparam.0 as *const RECT)
        }
        WM_DISPLAYCHANGE => app.finish_animation().and_then(|()| app.snap_to_edge()),
        WM_SETTINGCHANGE => {
            app.animations_enabled = system_animations_enabled();
            if app.animations_enabled {
                Ok(())
            } else {
                app.finish_animation()
            }
        }
        WM_POWERBROADCAST if wparam.0 == PBT_APMRESUMEAUTOMATIC as usize => {
            if let Some(worker) = &app.worker {
                worker.refresh();
            }
            Ok(())
        }
        WM_TIMER if wparam.0 == TIMER_REDRAW => app.render(),
        WM_TIMER if wparam.0 == TIMER_ANIMATION => app.render_animation_frame(Instant::now()),
        WM_CLOSE => app.toggle_visibility(),
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

fn register_window_class(instance: HINSTANCE) -> Result<(), AppError> {
    // SAFETY: loads a shared system cursor; the class does not own it.
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }?;
    let icon = load_app_icon()?;
    let class = WNDCLASSEXW {
        cbSize: u32::try_from(size_of::<WNDCLASSEXW>())
            .map_err(|_| AppError::Windows("窗口类结构大小溢出".to_owned()))?,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hIcon: icon,
        hCursor: cursor,
        lpszClassName: CLASS_NAME,
        hIconSm: icon,
        ..Default::default()
    };
    // SAFETY: class points to static names/resources and a process-lifetime window procedure.
    if unsafe { RegisterClassExW(&class) } == 0 {
        return Err(AppError::Windows("无法注册悬浮窗口类".to_owned()));
    }
    Ok(())
}

fn load_app_icon() -> Result<HICON, AppError> {
    // SAFETY: a null module name requests the current executable module.
    let module = unsafe { GetModuleHandleW(None) }?;
    let resource = PCWSTR::from_raw(ptr::without_provenance(APP_ICON_RESOURCE_ID));
    // SAFETY: resource uses the Win32 MAKEINTRESOURCE convention and ID 1 is embedded by app.rc.
    // LoadIconW returns a shared resource icon that must not be destroyed by this process.
    unsafe { LoadIconW(Some(HINSTANCE(module.0)), resource) }
        .map_err(|error| AppError::Windows(format!("无法加载应用图标：{error}")))
}

struct SingleInstance {
    handle: HANDLE,
    is_primary: bool,
}

impl SingleInstance {
    fn acquire() -> Result<Self, AppError> {
        // SAFETY: the mutex name is a static NUL-terminated string; the handle is closed in Drop.
        let handle = unsafe { CreateMutexW(None, false, MUTEX_NAME) }?;
        // SAFETY: GetLastError immediately follows CreateMutexW on the same thread.
        let is_primary = unsafe { GetLastError() } != ERROR_ALREADY_EXISTS;
        Ok(Self { handle, is_primary })
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: handle is owned by this guard and closed exactly once.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

struct MonitorDetails {
    info: MONITORINFOEXW,
}

fn monitor_info(monitor: HMONITOR) -> Result<MonitorDetails, AppError> {
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

fn primary_monitor() -> HMONITOR {
    // SAFETY: the default flag guarantees a monitor handle even if the point lies outside a display.
    unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }
}

fn monitor_for_device(device: &str) -> Option<HMONITOR> {
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

fn monitor_device(details: &MonitorDetails) -> String {
    String::from_utf16_lossy(&details.info.szDevice)
        .trim_end_matches('\0')
        .to_owned()
}

fn position_from_config(
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

fn expanded_destination(
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

fn anchored_destination(
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

fn set_autostart(enabled: bool) -> Result<(), AppError> {
    let key_path = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let value_name = wide("CodexQuota");
    let mut key = HKEY::default();
    // SAFETY: all buffers are NUL-terminated and key receives an owned registry handle.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(AppError::Windows(format!(
            "无法打开开机启动注册表项：{}",
            status.0
        )));
    }
    let result = if enabled {
        let executable =
            std::env::current_exe().map_err(|error| AppError::Config(error.to_string()))?;
        let command = wide(&format!("\"{}\"", executable.display()));
        // SAFETY: UTF-16 command is reinterpreted as bytes including its terminating NUL.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                command.as_ptr().cast::<u8>(),
                command.len() * size_of::<u16>(),
            )
        };
        // SAFETY: key is open for KEY_SET_VALUE and bytes contains a complete REG_SZ value.
        unsafe { RegSetValueExW(key, PCWSTR(value_name.as_ptr()), None, REG_SZ, Some(bytes)) }
    } else {
        // SAFETY: deletes only our exact value name from the opened per-user Run key.
        unsafe { RegDeleteValueW(key, PCWSTR(value_name.as_ptr())) }
    };
    // SAFETY: key is owned by this function and is closed exactly once.
    let _ = unsafe { RegCloseKey(key) };
    if result != ERROR_SUCCESS
        && (enabled || result != windows::Win32::Foundation::ERROR_FILE_NOT_FOUND)
    {
        return Err(AppError::Windows(format!(
            "无法更新开机启动设置：{}",
            result.0
        )));
    }
    Ok(())
}

fn dip_to_px(value: f32, dpi: u32) -> i32 {
    (value * dpi as f32 / 96.0).round() as i32
}

fn px_to_dip(value: i32, dpi: u32) -> f32 {
    value as f32 * 96.0 / dpi.max(1) as f32
}

fn lerp(start: f32, end: f32, progress: f32) -> f32 {
    start + (end - start) * progress.clamp(0.0, 1.0)
}

fn animation_sample(expanding: bool, timeline: f32) -> AnimationSample {
    let timeline = timeline.clamp(0.0, 1.0);
    let inverse = 1.0 - timeline;
    let eased = 1.0 - inverse * inverse * inverse;
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

fn animation_shape_rect(
    animation: PanelAnimation,
    expansion: f32,
    dpi: u32,
    edge: AnchorEdge,
    alignment: ExpansionAlignment,
) -> RECT {
    let width = dip_to_px(lerp(COLLAPSED_DIP, PANEL_WIDTH_DIP, expansion), dpi);
    let height = dip_to_px(lerp(COLLAPSED_DIP, PANEL_HEIGHT_DIP, expansion), dpi);
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

fn system_animations_enabled() -> bool {
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

fn point_in_rounded_rect(x: i32, y: i32, width: i32, height: i32, radius: i32) -> bool {
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

fn is_mouse_button_down(event: u32) -> bool {
    matches!(
        event,
        WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_RBUTTONDOWN | WM_XBUTTONDOWN
    )
}

fn panel_is_persistent(collapse_on_outside_click: bool) -> bool {
    !collapse_on_outside_click
}

fn refresh_interval_for_command(command: usize) -> Option<Duration> {
    match command {
        CMD_REFRESH_1_MIN => Some(Duration::from_mins(1)),
        CMD_REFRESH_5_MIN => Some(Duration::from_mins(5)),
        CMD_REFRESH_10_MIN => Some(Duration::from_mins(10)),
        CMD_REFRESH_30_MIN => Some(Duration::from_mins(30)),
        _ => None,
    }
}

fn point_is_outside_rounded_rect(point: POINT, rect: RECT, radius: i32) -> bool {
    !point_in_rounded_rect(
        point.x - rect.left,
        point.y - rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
        radius,
    )
}

fn is_tray_context_event(event: u32, uses_v4: bool) -> bool {
    if uses_v4 {
        event == WM_CONTEXTMENU
    } else {
        event == WM_RBUTTONUP
    }
}

fn tray_event_action(event: u32, uses_v4: bool) -> TrayEventAction {
    if is_tray_context_event(event, uses_v4) {
        TrayEventAction::OpenContextMenu
    } else {
        TrayEventAction::Ignore
    }
}

fn should_suppress_tray_reopen(closed: Instant, event: Instant) -> bool {
    event
        .checked_duration_since(closed)
        .is_some_and(|elapsed| elapsed <= TRAY_REOPEN_GUARD)
}

fn copy_wide_fixed<const N: usize>(value: &str, destination: &mut [u16; N]) {
    destination.fill(0);
    for (target, source) in destination
        .iter_mut()
        .take(N.saturating_sub(1))
        .zip(value.encode_utf16())
    {
        *target = source;
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!((lerp(COLLAPSED_DIP, PANEL_WIDTH_DIP, 0.5) - 166.0).abs() < f32::EPSILON);
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
            AnchorEdge::Right,
            ExpansionAlignment::Start,
        );
        assert_eq!(
            (shape.left, shape.top, shape.right, shape.bottom),
            (1_651, 407, 1_707, 463)
        );
    }

    #[test]
    fn tray_refresh_commands_map_to_expected_intervals() {
        assert_eq!(
            refresh_interval_for_command(CMD_REFRESH_1_MIN),
            Some(Duration::from_mins(1))
        );
        assert_eq!(
            refresh_interval_for_command(CMD_REFRESH_5_MIN),
            Some(Duration::from_mins(5))
        );
        assert_eq!(
            refresh_interval_for_command(CMD_REFRESH_10_MIN),
            Some(Duration::from_mins(10))
        );
        assert_eq!(
            refresh_interval_for_command(CMD_REFRESH_30_MIN),
            Some(Duration::from_mins(30))
        );
        assert_eq!(refresh_interval_for_command(CMD_EXIT), None);
    }

    #[test]
    fn rounded_hit_test_excludes_transparent_corner() {
        assert!(!point_in_rounded_rect(0, 0, 100, 50, 16));
        assert!(point_in_rounded_rect(16, 2, 100, 50, 16));
    }

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

    #[test]
    fn automatic_collapse_leaves_panel_persistent_unchecked() {
        assert!(!panel_is_persistent(true));
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

    #[test]
    fn v4_tray_context_ignores_legacy_right_button_event() {
        assert!(!is_tray_context_event(WM_RBUTTONUP, true));
    }

    #[test]
    fn tray_left_click_is_ignored() {
        assert_eq!(
            (
                tray_event_action(WM_LBUTTONUP, true),
                tray_event_action(WM_LBUTTONUP, false)
            ),
            (TrayEventAction::Ignore, TrayEventAction::Ignore)
        );
    }

    #[test]
    fn immediate_tray_callback_after_menu_close_is_suppressed() {
        let closed = Instant::now();
        assert!(should_suppress_tray_reopen(
            closed,
            closed + Duration::from_millis(10)
        ));
    }
}
