use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIN_BALLOONUSERCLICK, NOTIFY_ICON_DATA_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, EndMenu, GetCursorPos, HMENU, MF_CHECKED, MF_GRAYED,
    MF_POPUP, MF_SEPARATOR, MF_STRING, PostMessageW, SetForegroundWindow, TPM_BOTTOMALIGN,
    TPM_RIGHTALIGN, TrackPopupMenu, WM_CONTEXTMENU, WM_NULL, WM_RBUTTONUP,
};
use windows::core::PCWSTR;

use super::presentation::format_token_usage;
use super::{
    AppWindow, CMD_AUTOSTART, CMD_COLOR_STYLE_SOFT, CMD_COLOR_STYLE_VIVID, CMD_COPY_PANEL,
    CMD_EXIT, CMD_FOLLOW_CODEX, CMD_NOTIFY_OVERFLOW, CMD_NOTIFY_RESET, CMD_PANEL_PERSISTENT,
    CMD_REFRESH, CMD_REFRESH_1_MIN, CMD_REFRESH_2_MIN, CMD_REFRESH_5_MIN, CMD_REFRESH_10_MIN,
    CMD_REFRESH_30_MIN, CMD_RESET_COUNTDOWN, CMD_SHOW, CMD_TOKEN_UNIT_EN, CMD_TOKEN_UNIT_ZH,
    CMD_TOPMOST, TRAY_REOPEN_GUARD, WM_APP_EXPAND,
};
#[cfg(debug_assertions)]
use super::{CMD_TEST_NOTIFY_BALANCE, CMD_TEST_NOTIFY_RESET};
use crate::config::{ColorStyle, UnitStyle};
use crate::error::AppError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayEventAction {
    OpenContextMenu,
    /// 用户点了通知气泡：展开面板。
    ShowPanel,
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
    notify_on_reset: bool,
    notify_on_overflow: bool,
    show_reset_countdown: bool,
    token_unit: UnitStyle,
    color_style: ColorStyle,
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
    match tray_event_action(event, uses_v4) {
        TrayEventAction::OpenContextMenu => {}
        TrayEventAction::ShowPanel => {
            // SAFETY: the window owns the handler and posting by value carries no pointer.
            let hwnd = unsafe { (*app_ptr).hwnd };
            let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_EXPAND, WPARAM(0), LPARAM(0)) };
            return Ok(());
        }
        TrayEventAction::Ignore => return Ok(()),
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
                notify_on_reset: (*app_ptr).config.notify_on_reset,
                notify_on_overflow: (*app_ptr).config.notify_on_overflow,
                show_reset_countdown: (*app_ptr).config.show_reset_countdown,
                token_unit: (*app_ptr).config.token_unit,
                color_style: (*app_ptr).config.color_style,
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
    let menu = build_tray_menu(state)?;
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
        // TrackPopupMenu 返回后必须投递一条空消息，否则菜单可能残留不消失。
        // 本窗口是 WS_EX_NOACTIVATE，上面的 SetForegroundWindow 未必成功，
        // 前台未切换时这一步就是唯一的兜底。
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    }
    Ok(())
}

/// 组装托盘菜单（不含光标定位与弹出）。
///
/// 与弹出分开是为了让结构本身可测：顶层行数是这东西唯一的"设计指标"，
/// 拆成函数才能在测试里数出来。
///
/// 顶层只放"现在要做什么"和常调项：三个动作，然后是三个互斥单选组
/// （单选组本身就是子菜单，留在顶层才不用第三层），最后是收着复选框开关的
/// 「设置」和退出。分组依据是**控件类型**，不是"感觉像一类"。
///
/// # Errors
///
/// 创建菜单或追加菜单项失败时返回错误（菜单句柄或内存不足）。
fn build_tray_menu(state: TrayMenuState) -> Result<PopupMenu, AppError> {
    let menu = PopupMenu::create()?;
    let refresh_menu = PopupMenu::create()?;
    let notify_menu = PopupMenu::create()?;
    let unit_menu = PopupMenu::create()?;
    let color_menu = PopupMenu::create()?;
    let settings_menu = PopupMenu::create()?;
    let show = wide(if state.visible {
        "隐藏悬浮球"
    } else {
        "显示悬浮球"
    });
    let refresh = wide("立即刷新");
    let copy_panel = wide("面板截图");
    let refresh_interval = wide("刷新间隔");
    let notifications = wide("通知");
    let token_unit = wide("用量单位");
    let color_style = wide("颜色风格");
    let settings = wide("设置");
    let exit = wide("退出");
    append_refresh_interval_entries(refresh_menu.0, state)?;
    append_notify_entries(notify_menu.0, state)?;
    append_token_unit_entries(unit_menu.0, state)?;
    append_color_style_entries(color_menu.0, state)?;
    append_settings_entries(settings_menu.0, state)?;
    append_menu_command(menu.0, CMD_SHOW, &show, false, !state.follow_codex)?;
    append_menu_command(
        menu.0,
        CMD_REFRESH,
        &refresh,
        false,
        refresh_command_is_enabled(state),
    )?;
    append_menu_command(
        menu.0,
        CMD_COPY_PANEL,
        &copy_panel,
        false,
        screenshot_command_is_enabled(state),
    )?;
    attach_submenu(menu.0, refresh_menu, &refresh_interval)?;
    attach_submenu(menu.0, notify_menu, &notifications)?;
    attach_submenu(menu.0, unit_menu, &token_unit)?;
    attach_submenu(menu.0, color_menu, &color_style)?;
    // SAFETY: menu is valid and separators do not carry string data.
    unsafe { AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null())? };
    attach_submenu(menu.0, settings_menu, &settings)?;
    // SAFETY: menu is valid and the final menu strings stay live through TrackPopupMenu.
    unsafe { AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null())? };
    append_menu_command(menu.0, CMD_EXIT, &exit, false, true)?;
    #[cfg(debug_assertions)]
    append_debug_entries(menu.0)?;
    Ok(menu)
}

/// 用量单位两个选项里那个样例值：**同一个数量**在两种单位下的写法，摆在一起才
/// 有得对照（12,000 恰好是 1.2万 = 12K，不需要四舍五入去凑）。
///
/// 取这么小而不是 `1024.3万`：菜单宽度由最宽的那一项决定，而那一版光两个全角
/// 括号就各占一个汉字宽、后面还挂着六位数字，让这个只有两项的子菜单比「通知」
/// 那一组宽出一截。样例必须短，见 `token_unit_samples_stay_short`。
const UNIT_SAMPLE_TOKENS: u64 = 12_000;

fn append_token_unit_entries(menu: HMENU, state: TrayMenuState) -> Result<(), AppError> {
    // 样例由真正的格式化路径生成，不手写：分档规则一改（比如万位小数），标签
    // 跟着走，否则菜单里会放着一个应用根本不会那样写的数字。
    let chinese = wide(&format!(
        "中文 {}",
        format_token_usage(Some(UNIT_SAMPLE_TOKENS), UnitStyle::Zh)
    ));
    let western = wide(&format!(
        "西文 {}",
        format_token_usage(Some(UNIT_SAMPLE_TOKENS), UnitStyle::En)
    ));
    append_menu_command(
        menu,
        CMD_TOKEN_UNIT_ZH,
        &chinese,
        state.token_unit == UnitStyle::Zh,
        true,
    )?;
    append_menu_command(
        menu,
        CMD_TOKEN_UNIT_EN,
        &western,
        state.token_unit == UnitStyle::En,
        true,
    )?;
    Ok(())
}

fn append_color_style_entries(menu: HMENU, state: TrayMenuState) -> Result<(), AppError> {
    // 两项就是两套配色的全部：风格名按"处理方式"取而不是按颜色本身取——
    // 两套都是绿/琥珀/红，说「薄荷」「珊瑚」在两边都成立，读不出区别。
    let soft = wide("柔和");
    let vivid = wide("鲜艳");
    append_menu_command(
        menu,
        CMD_COLOR_STYLE_SOFT,
        &soft,
        state.color_style == ColorStyle::Soft,
        true,
    )?;
    append_menu_command(
        menu,
        CMD_COLOR_STYLE_VIVID,
        &vivid,
        state.color_style == ColorStyle::Vivid,
        true,
    )?;
    Ok(())
}

/// 把一个子菜单挂到父菜单上。
///
/// 挂接成功后由父菜单负责销毁，所以必须 `forget` 掉守卫，否则 `PopupMenu::drop`
/// 会把刚挂上去的子菜单拆掉。失败时守卫照常析构，不会漏句柄。
///
/// # Errors
///
/// `AppendMenuW` 失败（菜单句柄无效或内存不足）时返回错误。
fn attach_submenu(parent: HMENU, submenu: PopupMenu, label: &[u16]) -> Result<(), AppError> {
    // SAFETY: parent is valid and label is NUL-terminated for the duration of the call;
    // AppendMenuW copies the text, and ownership of the submenu transfers on success.
    unsafe {
        AppendMenuW(
            parent,
            MF_POPUP,
            submenu.0.0 as usize,
            PCWSTR(label.as_ptr()),
        )?;
    }
    std::mem::forget(submenu);
    Ok(())
}

/// 设置块：全是复选框开关，收进「设置 ▸」以缩短顶层。
///
/// 默认勾选状态据此不再在顶层可见——这些开关都是装完就不动的，换顶层 5 行的
/// 高度划算。
///
/// # Errors
///
/// 菜单项追加失败时返回错误（菜单句柄或内存不足）。
fn append_settings_entries(menu: HMENU, state: TrayMenuState) -> Result<(), AppError> {
    let topmost = wide("始终置顶");
    let autostart = wide("开机启动");
    let follow_codex = wide("跟随 Codex");
    let panel_persistent = wide("面板常驻");
    let reset_countdown = wide("重置倒计时");
    append_menu_command(menu, CMD_TOPMOST, &topmost, state.always_on_top, true)?;
    append_menu_command(
        menu,
        CMD_AUTOSTART,
        &autostart,
        state.start_with_windows,
        true,
    )?;
    append_menu_command(
        menu,
        CMD_FOLLOW_CODEX,
        &follow_codex,
        state.follow_codex,
        true,
    )?;
    append_menu_command(
        menu,
        CMD_PANEL_PERSISTENT,
        &panel_persistent,
        panel_is_persistent(state.collapse_on_outside_click),
        true,
    )?;
    append_menu_command(
        menu,
        CMD_RESET_COUNTDOWN,
        &reset_countdown,
        state.show_reset_countdown,
        true,
    )?;
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

/// 仅调试构建：菜单最下方追加两条"测试通知"，用来免等真实事件验证投递路径。
#[cfg(debug_assertions)]
fn append_debug_entries(menu: HMENU) -> Result<(), AppError> {
    let reset = wide("测试通知：额度重置");
    let balance = wide("测试通知：余额使用");
    // SAFETY: menu is valid and separators do not carry string data.
    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())? };
    append_menu_command(menu, CMD_TEST_NOTIFY_RESET, &reset, false, true)?;
    append_menu_command(menu, CMD_TEST_NOTIFY_BALANCE, &balance, false, true)?;
    Ok(())
}

fn append_notify_entries(menu: HMENU, state: TrayMenuState) -> Result<(), AppError> {
    let reset = wide("额度重置提醒");
    let overflow = wide("余额使用提醒");
    append_menu_command(menu, CMD_NOTIFY_RESET, &reset, state.notify_on_reset, true)?;
    append_menu_command(
        menu,
        CMD_NOTIFY_OVERFLOW,
        &overflow,
        state.notify_on_overflow,
        true,
    )?;
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

/// 命令 → 用量单位；`None` 表示这条命令不是单位选项。
///
/// 与 [`refresh_interval_for_command`] 同样的做法：把"菜单项 ↔ 设置值"的映射
/// 单独放出来，`AppWindow::command` 只负责落地，映射本身可以单测——中/西文写反
/// 是那种只在用户切换的那一刻才暴露的错。
pub(super) fn token_unit_for_command(command: usize) -> Option<UnitStyle> {
    match command {
        CMD_TOKEN_UNIT_ZH => Some(UnitStyle::Zh),
        CMD_TOKEN_UNIT_EN => Some(UnitStyle::En),
        _ => None,
    }
}

/// 命令 → 配色风格；`None` 表示这条命令不是配色选项。与
/// [`token_unit_for_command`] 同样的做法与理由。
pub(super) fn color_style_for_command(command: usize) -> Option<ColorStyle> {
    match command {
        CMD_COLOR_STYLE_SOFT => Some(ColorStyle::Soft),
        CMD_COLOR_STYLE_VIVID => Some(ColorStyle::Vivid),
        _ => None,
    }
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
    } else if event == NIN_BALLOONUSERCLICK {
        TrayEventAction::ShowPanel
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

/// 截图命令与「立即刷新」同一门控：面板资源没激活（跟随 Codex 且 Codex 未运行）
/// 时既没有可刷新的数据，也没有可截的画面。悬浮球只是隐藏或收起不影响截图。
/// 转发给刷新门控，避免两处条件各写一份后漂移。
fn screenshot_command_is_enabled(state: TrayMenuState) -> bool {
    refresh_command_is_enabled(state)
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
    use windows::Win32::UI::Shell::{NIF_GUID, NIN_BALLOONTIMEOUT};
    use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, WM_LBUTTONUP, WM_RBUTTONUP};

    use super::*;

    /// 顶层行数是这个菜单唯一的设计指标：**开关请放进「设置 ▸」**。
    ///
    /// 要在顶层加项，先想清楚它是不是"现在要做什么"、高频项，或者一个互斥单选组
    /// ——否则顶层会一路长回当初那 11 行。
    #[test]
    fn top_level_stays_short() {
        let menu = build_tray_menu(TrayMenuState::default()).expect("构建托盘菜单");
        // SAFETY: 菜单是刚建好的活句柄，只查询项数。
        let count = unsafe { GetMenuItemCount(Some(menu.0)) };
        // 9 项 + 2 条分隔线 = 11（`GetMenuItemCount` 把分隔线也算一项）。
        // 9 项 = 显示/隐藏、立即刷新、面板截图、刷新间隔▸、通知▸、用量单位▸、
        //        颜色风格▸、设置▸、退出。
        let expected = 9 + 2;
        #[cfg(debug_assertions)]
        let expected = expected + 3; // 调试构建另有分隔线 + 两条测试通知
        assert_eq!(
            count, expected,
            "顶层项数变了：开关请加进「设置 ▸」（append_settings_entries），\
             顶层只留动作、高频项和互斥单选组"
        );
    }

    /// 复选框开关收在「设置」里，单选组不进这里（它们各自是顶层子菜单）。
    #[test]
    fn switches_live_inside_the_settings_submenu() {
        let settings = PopupMenu::create().expect("创建子菜单");
        append_settings_entries(settings.0, TrayMenuState::default()).expect("追加设置项");
        // SAFETY: 子菜单是刚建好的活句柄，只查询项数。
        let count = unsafe { GetMenuItemCount(Some(settings.0)) };
        assert_eq!(
            count, 5,
            "「设置」里应当是 5 个复选框开关：始终置顶、开机启动、跟随 Codex、\
             面板常驻、重置倒计时"
        );
    }

    /// 单选组子菜单的项数也是结构的一部分：刷新 5 档、通知 2 项、单位 2 种、
    /// 配色 2 套。
    #[test]
    fn radio_group_submenus_keep_their_choice_counts() {
        for (build, expected, what) in [
            (
                &(|menu: HMENU| append_refresh_interval_entries(menu, TrayMenuState::default()))
                    as &dyn Fn(HMENU) -> Result<(), AppError>,
                5,
                "刷新间隔",
            ),
            (
                &(|menu: HMENU| append_notify_entries(menu, TrayMenuState::default())),
                2,
                "通知",
            ),
            (
                &(|menu: HMENU| append_token_unit_entries(menu, TrayMenuState::default())),
                2,
                "用量单位",
            ),
            (
                &(|menu: HMENU| append_color_style_entries(menu, TrayMenuState::default())),
                2,
                "颜色风格",
            ),
        ] {
            let menu = PopupMenu::create().expect("创建子菜单");
            build(menu.0).expect("追加选项");
            // SAFETY: 子菜单是刚建好的活句柄，只查询项数。
            let count = unsafe { GetMenuItemCount(Some(menu.0)) };
            assert_eq!(count, expected, "{what} 的档数变了");
        }
    }

    /// 用量单位那一组只有两项，宽度却由标签里的样例决定：样例一长，这个子菜单
    /// 就比「通知」「刷新间隔」宽出一截（`中文（1024.3万）` 那版就是如此）。
    /// 换样例值之前先看这一条——它量的是菜单的宽度预算，不是格式化规则。
    #[test]
    fn token_unit_samples_stay_short() {
        for unit in [UnitStyle::Zh, UnitStyle::En] {
            let sample = format_token_usage(Some(UNIT_SAMPLE_TOKENS), unit);
            assert!(
                sample.chars().count() <= 5,
                "{unit:?} 的样例「{sample}」太长，会把用量单位子菜单撑宽"
            );
        }
    }

    /// 中/西文写反只在用户切换的那一刻才暴露，所以映射单独测。
    #[test]
    fn tray_token_unit_commands_map_to_expected_styles() {
        assert_eq!(
            token_unit_for_command(CMD_TOKEN_UNIT_ZH),
            Some(UnitStyle::Zh)
        );
        assert_eq!(
            token_unit_for_command(CMD_TOKEN_UNIT_EN),
            Some(UnitStyle::En)
        );
        assert_eq!(token_unit_for_command(CMD_REFRESH), None);
        assert_eq!(token_unit_for_command(CMD_RESET_COUNTDOWN), None);
    }

    /// 两套配色写反是那种只在用户点下去的那一刻才暴露的错。
    #[test]
    fn tray_color_style_commands_map_to_expected_styles() {
        assert_eq!(
            color_style_for_command(CMD_COLOR_STYLE_SOFT),
            Some(ColorStyle::Soft)
        );
        assert_eq!(
            color_style_for_command(CMD_COLOR_STYLE_VIVID),
            Some(ColorStyle::Vivid)
        );
        assert_eq!(color_style_for_command(CMD_REFRESH), None);
        // 两组单选、两条映射互不串门：单位命令不能被当成配色命令接住。
        assert_eq!(color_style_for_command(CMD_TOKEN_UNIT_ZH), None);
        assert_eq!(token_unit_for_command(CMD_COLOR_STYLE_VIVID), None);
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
    fn balloon_click_expands_the_panel() {
        assert_eq!(
            tray_event_action(NIN_BALLOONUSERCLICK, true),
            TrayEventAction::ShowPanel
        );
        // 气泡超时/关闭不做事，右键仍然只开菜单。
        assert_eq!(
            tray_event_action(NIN_BALLOONTIMEOUT, true),
            TrayEventAction::Ignore
        );
        assert_eq!(
            tray_event_action(WM_CONTEXTMENU, true),
            TrayEventAction::OpenContextMenu
        );
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
    fn screenshot_is_disabled_while_following_codex_without_codex() {
        // 跟随模式且 Codex 未运行：面板资源已释放，没有可截的画面。
        assert!(!screenshot_command_is_enabled(TrayMenuState::default()));
        assert!(!screenshot_command_is_enabled(TrayMenuState {
            follow_codex: true,
            overlay_active: false,
            ..Default::default()
        }));
    }

    #[test]
    fn screenshot_stays_enabled_while_the_ball_is_hidden_or_collapsed() {
        // 隐藏或收起都不释放面板资源，截图仍然可用。
        assert!(screenshot_command_is_enabled(TrayMenuState {
            overlay_active: true,
            visible: false,
            ..Default::default()
        }));
        assert!(screenshot_command_is_enabled(TrayMenuState {
            overlay_active: true,
            visible: true,
            ..Default::default()
        }));
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
