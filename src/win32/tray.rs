use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, POINT};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NOTIFY_ICON_DATA_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, EndMenu, GetCursorPos, HMENU, MF_CHECKED, MF_GRAYED,
    MF_POPUP, MF_SEPARATOR, MF_STRING, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RIGHTALIGN,
    TrackPopupMenu, WM_CONTEXTMENU, WM_RBUTTONUP,
};
use windows::core::PCWSTR;

use super::{
    AppWindow, CMD_AUTOSTART, CMD_EXIT, CMD_FOLLOW_CODEX, CMD_PANEL_PERSISTENT, CMD_REFRESH,
    CMD_REFRESH_1_MIN, CMD_REFRESH_2_MIN, CMD_REFRESH_5_MIN, CMD_REFRESH_10_MIN,
    CMD_REFRESH_30_MIN, CMD_SHOW, CMD_TOPMOST, TRAY_REOPEN_GUARD,
};
use crate::error::AppError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayEventAction {
    OpenContextMenu,
    Ignore,
}

struct PopupMenu(HMENU);

#[derive(Clone, Copy, Default)]
struct TrayMenuState {
    visible: bool,
    overlay_active: bool,
    always_on_top: bool,
    start_with_windows: bool,
    follow_codex: bool,
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

pub(super) unsafe fn handle_tray_message(
    app_ptr: *mut AppWindow,
    lparam: LPARAM,
) -> Result<(), AppError> {
    let event = lparam.0 as u32 & 0xffff;
    // SAFETY: app_ptr is the live Box pointer stored in GWLP_USERDATA on the UI thread.
    let uses_v4 = unsafe { (*app_ptr).tray_uses_v4 };
    if tray_event_action(event, uses_v4) != TrayEventAction::OpenContextMenu {
        return Ok(());
    }

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
                overlay_active: (*app_ptr).overlay_active,
                always_on_top: (*app_ptr).config.always_on_top,
                start_with_windows: (*app_ptr).config.start_with_windows,
                follow_codex: (*app_ptr).config.follow_codex,
                collapse_on_outside_click: (*app_ptr).config.collapse_on_outside_click,
                quota_refresh_interval_secs: (*app_ptr).config.quota_refresh_interval().as_secs(),
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
    let refresh_interval = wide("刷新间隔");
    let topmost = wide("始终置顶");
    let autostart = wide("开机启动");
    let follow_codex = wide("跟随 Codex");
    let panel_persistent = wide("面板常驻");
    let exit = wide("退出");
    append_refresh_interval_entries(refresh_menu.0, state)?;
    append_menu_command(menu.0, CMD_SHOW, &show, false, !state.follow_codex)?;
    append_menu_command(
        menu.0,
        CMD_REFRESH,
        &refresh,
        false,
        refresh_command_is_enabled(state),
    )?;
    // SAFETY: menu is valid and each string is NUL-terminated for the duration of appending.
    unsafe {
        AppendMenuW(
            menu.0,
            MF_POPUP,
            refresh_menu.0.0 as usize,
            PCWSTR(refresh_interval.as_ptr()),
        )?;
        // After successful attachment, the parent menu owns and destroys the submenu.
        std::mem::forget(refresh_menu);
    }
    // SAFETY: menu is valid and separators do not carry string data.
    unsafe { AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null())? };
    append_menu_command(menu.0, CMD_TOPMOST, &topmost, state.always_on_top, true)?;
    append_menu_command(
        menu.0,
        CMD_AUTOSTART,
        &autostart,
        state.start_with_windows,
        true,
    )?;
    append_menu_command(
        menu.0,
        CMD_FOLLOW_CODEX,
        &follow_codex,
        state.follow_codex,
        true,
    )?;
    append_menu_command(
        menu.0,
        CMD_PANEL_PERSISTENT,
        &panel_persistent,
        panel_is_persistent(state.collapse_on_outside_click),
        true,
    )?;
    // SAFETY: menu is valid and the final menu strings stay live through TrackPopupMenu.
    unsafe { AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null())? };
    append_menu_command(menu.0, CMD_EXIT, &exit, false, true)?;
    // SAFETY: foreground ownership is required for dismissal; both HWND and menu are live.
    unsafe {
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

fn append_refresh_interval_entries(menu: HMENU, state: TrayMenuState) -> Result<(), AppError> {
    let one_minute = wide("1 分钟");
    let two_minute = wide("2 分钟");
    let five_minute = wide("5 分钟");
    let ten_minute = wide("10 分钟");
    let thirty_minute = wide("30 分钟");
    for (command, seconds, label) in [
        (CMD_REFRESH_1_MIN, 60, &one_minute),
        (CMD_REFRESH_2_MIN, 2 * 60, &two_minute),
        (CMD_REFRESH_5_MIN, 5 * 60, &five_minute),
        (CMD_REFRESH_10_MIN, 10 * 60, &ten_minute),
        (CMD_REFRESH_30_MIN, 30 * 60, &thirty_minute),
    ] {
        append_menu_command(
            menu,
            command,
            label,
            state.quota_refresh_interval_secs == seconds,
            true,
        )?;
    }
    Ok(())
}

fn append_menu_command(
    menu: HMENU,
    command: usize,
    label: &[u16],
    checked: bool,
    enabled: bool,
) -> Result<(), AppError> {
    let flags = MF_STRING
        | if checked {
            MF_CHECKED
        } else {
            Default::default()
        }
        | if enabled {
            Default::default()
        } else {
            MF_GRAYED
        };
    // SAFETY: menu is live and label is NUL-terminated and borrowed for the duration of this call.
    unsafe { AppendMenuW(menu, flags, command, PCWSTR(label.as_ptr()))? };
    Ok(())
}

pub(super) fn panel_is_persistent(collapse_on_outside_click: bool) -> bool {
    !collapse_on_outside_click
}

pub(super) fn refresh_interval_for_command(command: usize) -> Option<Duration> {
    match command {
        CMD_REFRESH_1_MIN => Some(Duration::from_mins(1)),
        CMD_REFRESH_2_MIN => Some(Duration::from_mins(2)),
        CMD_REFRESH_5_MIN => Some(Duration::from_mins(5)),
        CMD_REFRESH_10_MIN => Some(Duration::from_mins(10)),
        CMD_REFRESH_30_MIN => Some(Duration::from_mins(30)),
        _ => None,
    }
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

pub(super) fn tray_icon_flags() -> NOTIFY_ICON_DATA_FLAGS {
    NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP
}

fn refresh_command_is_enabled(state: TrayMenuState) -> bool {
    state.overlay_active
}

fn should_suppress_tray_reopen(closed: Instant, event: Instant) -> bool {
    event
        .checked_duration_since(closed)
        .is_some_and(|elapsed| elapsed <= TRAY_REOPEN_GUARD)
}

pub(super) fn copy_wide_fixed<const N: usize>(value: &str, destination: &mut [u16; N]) {
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
    use windows::Win32::UI::Shell::NIF_GUID;
    use windows::Win32::UI::WindowsAndMessaging::{WM_LBUTTONUP, WM_RBUTTONUP};

    use super::*;

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
    fn two_minute_tray_command_maps_to_two_minute_interval() {
        assert_eq!(
            refresh_interval_for_command(CMD_REFRESH_2_MIN),
            Some(Duration::from_mins(2))
        );
    }

    #[test]
    fn version_four_tray_icon_requests_standard_tooltip() {
        assert!(tray_icon_flags().contains(NIF_SHOWTIP));
    }

    #[test]
    fn portable_tray_icon_does_not_use_path_bound_guid() {
        assert!(!tray_icon_flags().contains(NIF_GUID));
    }

    #[test]
    fn immediate_refresh_is_disabled_without_active_overlay_resources() {
        assert!(!refresh_command_is_enabled(TrayMenuState::default()));
    }

    #[test]
    fn automatic_collapse_leaves_panel_persistent_unchecked() {
        assert!(!panel_is_persistent(true));
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
