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

mod interaction;
mod layout;
mod overlay;
mod presence;
mod presentation;
mod renderer;
mod system;
mod tray;
mod window;

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use layout::{
    ExpansionAlignment, PanelAnimation, dip_to_px, monitor_for_device, monitor_info,
    position_from_config, primary_monitor,
};
use presence::CodexPresenceWatcher;
use renderer::Renderer;
use system::{SingleInstance, register_window_class};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW, HHOOK, MSG,
    PostMessageW, WM_APP, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};
use windows::core::{GUID, PCWSTR, w};

use crate::config::{self, AppConfigV1};
use crate::error::AppError;
use crate::quota::{AppState, CodexWorker};

const CLASS_NAME: PCWSTR = w!("CodexQuotaOverlayWindow");
const WINDOW_NAME: PCWSTR = w!("Codex Quota");
const MUTEX_NAME: PCWSTR = w!("Local\\CodexQuota.Singleton");
const WM_APP_UPDATED: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const WM_APP_SHOW: u32 = WM_APP + 3;
const WM_APP_COLLAPSE: u32 = WM_APP + 4;
const WM_APP_PRESENCE_CHANGED: u32 = WM_APP + 5;
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
const CMD_TOPMOST: usize = 1004;
const CMD_AUTOSTART: usize = 1005;
const CMD_EXIT: usize = 1006;
const CMD_PANEL_PERSISTENT: usize = 1007;
const CMD_REFRESH_1_MIN: usize = 1008;
const CMD_REFRESH_5_MIN: usize = 1009;
const CMD_REFRESH_10_MIN: usize = 1010;
const CMD_REFRESH_30_MIN: usize = 1011;
const CMD_FOLLOW_CODEX: usize = 1012;
const CMD_REFRESH_2_MIN: usize = 1013;
const COLLAPSED_DIP: f32 = 56.0;
const PANEL_WIDTH_DIP: f32 = 276.0;
const PANEL_HEIGHT_DIP: f32 = 202.0;
const TRAY_REOPEN_GUARD: Duration = Duration::from_millis(500);
static OUTSIDE_CLICK_HWND: AtomicUsize = AtomicUsize::new(0);

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
    let initialization = unsafe { &mut *app_ptr }.initialize(hwnd);
    if let Err(error) = initialization {
        // SAFETY: releases HWND ownership of app_ptr through WM_NCDESTROY before returning.
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err(error);
    }
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
    presence_watcher: Option<CodexPresenceWatcher>,
    renderer: Option<Renderer>,
    outside_click_hook: Option<HHOOK>,
    animation: Option<PanelAnimation>,
    animations_enabled: bool,
    expanded: bool,
    expansion_alignment: ExpansionAlignment,
    visible: bool,
    overlay_active: bool,
    codex_present: bool,
    presence_generation: u32,
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
