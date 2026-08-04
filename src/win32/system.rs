use std::mem::size_of;
use std::ptr;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HANDLE, HINSTANCE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegSetValueExW,
};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::Input::Ime::ImmDisableIME;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, HICON, IDC_ARROW, LoadCursorW, LoadIconW, RegisterClassExW, WNDCLASSEXW,
};
use windows::core::PCWSTR;

use super::window::window_proc;
use super::{APP_ICON_RESOURCE_ID, CLASS_NAME, MUTEX_NAME};
use crate::error::AppError;

pub(super) struct SingleInstance {
    handle: HANDLE,
    pub(super) is_primary: bool,
}

impl SingleInstance {
    pub(super) fn acquire() -> Result<Self, AppError> {
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

pub(super) fn disable_ime_for_current_thread() -> bool {
    // SAFETY: GetCurrentThreadId has no caller-side preconditions.
    let thread_id = unsafe { GetCurrentThreadId() };
    // SAFETY: run calls this on the UI thread before creating its first top-level window, as
    // required by ImmDisableIME. This app has no text input controls that require an IME.
    unsafe { ImmDisableIME(thread_id) }.as_bool()
}

pub(super) fn register_window_class(instance: HINSTANCE) -> Result<(), AppError> {
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

pub(super) fn load_app_icon() -> Result<HICON, AppError> {
    // SAFETY: a null module name requests the current executable module.
    let module = unsafe { GetModuleHandleW(None) }?;
    let resource = PCWSTR::from_raw(ptr::without_provenance(APP_ICON_RESOURCE_ID));
    // SAFETY: resource uses the Win32 MAKEINTRESOURCE convention and ID 1 is embedded by app.rc.
    // LoadIconW returns a shared resource icon that must not be destroyed by this process.
    unsafe { LoadIconW(Some(HINSTANCE(module.0)), resource) }
        .map_err(|error| AppError::Windows(format!("无法加载应用图标：{error}")))
}

pub(super) fn set_autostart(enabled: bool) -> Result<(), AppError> {
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

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
