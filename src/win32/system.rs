use std::mem::size_of;
use std::path::PathBuf;
use std::ptr;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HANDLE, HINSTANCE, HMODULE,
    HRSRC,
};
use windows::Win32::System::LibraryLoader::{
    FindResourceW, GetModuleHandleW, LoadResource, LockResource, SizeofResource,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW, RegSetValueExW,
};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::Input::Ime::ImmDisableIME;
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, HICON, IDC_ARROW, LoadCursorW, LoadIconW, RT_GROUP_ICON, RT_ICON,
    RegisterClassExW, WNDCLASSEXW,
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

/// 应用标识（AppUserModelID）与它在系统 UI 里显示的名字。
///
/// 非打包应用没有接口能直接指定通知标题：那个标题取的是进程 AUMID 对应的
/// 显示名，没登记过就退回可执行文件名（通知里会看到 `codex-quota.exe`）。
/// 这里登记的是**不需要创建快捷方式**的那条最小路径。
///
/// 壳层会按 AUMID 缓存身份（显示名 + 图标），改了注册表它不一定重读，所以
/// 早期用错图标值的那版换了新 ID：`CodexQuota` → `CodexQuota.Overlay`。
const APP_USER_MODEL_ID: &str = "CodexQuota.Overlay";
/// 之前用过的 ID：它的图标登记是坏的，且已进过壳层缓存，启动时顺手删掉。
const PREVIOUS_USER_MODEL_ID: &str = "CodexQuota";
const APP_DISPLAY_NAME: &str = "Codex Quota";
/// 与 app.rc 里嵌入的是同一份图标里最接近 32×32 的那张，运行时从资源重组成
/// 单图 `.ico` 落盘供 `IconUri` 指向。
const APP_ICON_FILE: &str = "app.ico";

/// 登记应用身份：设置进程 AUMID，并在注册表里写好显示名与图标。
///
/// 全部尽力而为——任何一步失败只记日志。最坏结果就是通知标题继续显示
/// 可执行文件名，功能与投递都不受影响。
pub(super) fn register_app_identity() {
    let aumid = wide(APP_USER_MODEL_ID);
    // SAFETY: aumid is NUL-terminated and outlives the call; must be set before any shell UI.
    if let Err(error) = unsafe { SetCurrentProcessExplicitAppUserModelID(PCWSTR(aumid.as_ptr())) } {
        crate::logging::log(&format!("无法设置应用标识：{error}"));
    }
    remove_previous_registration();
    if let Err(error) = write_display_name() {
        crate::logging::log(&format!("无法登记通知显示名：{error}"));
    }
}

/// 删掉早期那版 AUMID 项；失败无所谓，只是一个多余的键。
fn remove_previous_registration() {
    let key_path = wide(&format!(
        "Software\\Classes\\AppUserModelId\\{PREVIOUS_USER_MODEL_ID}"
    ));
    // SAFETY: deletes only our own previous key, which has no subkeys.
    let _ = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(key_path.as_ptr())) };
}

/// 把内嵌图标里的一张重组成单图 `.ico` 写到应用数据目录，返回可供 `IconUri`
/// 使用的路径。
///
/// 通知身份的图标要给**图标文件**：给 exe + 资源 ID 时实测解析不出来（空白
/// 图标），所以这里落一份出来——但只落通知需要的那张（约 2KB），不把整份
/// 8 尺寸的图标再嵌一遍进二进制。
fn materialize_icon() -> Option<PathBuf> {
    let path = crate::config::app_data_dir().ok()?.join(APP_ICON_FILE);
    let bytes = single_image_icon()?;
    // 写失败但文件已存在就继续用旧的：通知图标不值得让启动或登记失败。
    if std::fs::write(&path, bytes).is_err() && !path.exists() {
        return None;
    }
    Some(path)
}

/// 读 `RT_GROUP_ICON` 目录，挑出最接近 32×32 的一张，拼成标准单图 `.ico`。
fn single_image_icon() -> Option<Vec<u8>> {
    // SAFETY: a null module name requests the current executable module.
    let module = unsafe { GetModuleHandleW(None) }.ok()?;
    let name = PCWSTR::from_raw(ptr::without_provenance(APP_ICON_RESOURCE_ID));
    // SAFETY: the resource id follows the MAKEINTRESOURCE convention; invalid handles are checked.
    let group = unsafe { FindResourceW(Some(module), name, RT_GROUP_ICON) };
    if group.is_invalid() {
        return None;
    }
    // SAFETY: group is a valid resource handle of this module.
    let directory = unsafe { resource_bytes(module, group)? };
    let entry = pick_entry(directory)?;
    let image_name = PCWSTR::from_raw(ptr::without_provenance(usize::from(entry.image_id)));
    // SAFETY: same convention, with the image id taken from the group directory above.
    let image = unsafe { FindResourceW(Some(module), image_name, RT_ICON) };
    if image.is_invalid() {
        return None;
    }
    // SAFETY: image is a valid resource handle of this module.
    let image_bytes = unsafe { resource_bytes(module, image)? };
    Some(assemble_icon(entry, image_bytes))
}

/// 载入并锁定一个资源，返回与模块同寿命的字节切片。
///
/// 资源常驻在模块映像里，锁定后无需（也不能）释放，所以返回 `'static`。
///
/// # Safety
///
/// `resource` 必须是 `module` 上有效的资源句柄。
unsafe fn resource_bytes(module: HMODULE, resource: HRSRC) -> Option<&'static [u8]> {
    // SAFETY: caller guarantees the handle belongs to this module.
    let size = usize::try_from(unsafe { SizeofResource(Some(module), resource) }).ok()?;
    // SAFETY: same guarantee; the handle is locked immediately below.
    let handle = unsafe { LoadResource(Some(module), resource) }.ok()?;
    // SAFETY: locking a loaded resource yields a pointer valid while the module is loaded.
    let pointer = unsafe { LockResource(handle) }.cast::<u8>();
    if pointer.is_null() || size == 0 {
        return None;
    }
    // SAFETY: the module owns `size` readable bytes at pointer for its whole lifetime.
    Some(unsafe { std::slice::from_raw_parts(pointer, size) })
}

/// `RT_GROUP_ICON` 目录里的一个条目（14 字节，按固定偏移解析）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IconEntry {
    width: u8,
    height: u8,
    color_count: u8,
    planes: u16,
    bit_count: u16,
    /// 对应 `RT_ICON` 的资源 id。
    image_id: u16,
}

/// 挑最接近 32×32 的一张：通知里的图标位就那么大，带全尺寸没有意义。
fn pick_entry(directory: &[u8]) -> Option<IconEntry> {
    let count = usize::from(read_u16(directory, 4)?);
    let mut best: Option<(u32, IconEntry)> = None;
    for index in 0..count {
        let offset = 6 + index * 14;
        let entry = IconEntry {
            width: directory.get(offset).copied()?,
            height: directory.get(offset + 1).copied()?,
            color_count: directory.get(offset + 2).copied()?,
            planes: read_u16(directory, offset + 4)?,
            bit_count: read_u16(directory, offset + 6)?,
            image_id: read_u16(directory, offset + 12)?,
        };
        // 目录里 0 表示 256。
        let width = u32::from(if entry.width == 0 { 255 } else { entry.width });
        let distance = width.abs_diff(32);
        if best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, entry));
        }
    }
    best.map(|(_, entry)| entry)
}

/// 拼一个只含一张图的标准 `.ico`：`ICONDIR` + 一个 `ICONDIRENTRY` + 图像数据。
fn assemble_icon(entry: IconEntry, image: &[u8]) -> Vec<u8> {
    const IMAGE_OFFSET: u32 = 6 + 16;
    let mut icon = Vec::with_capacity(IMAGE_OFFSET as usize + image.len());
    icon.extend_from_slice(&0_u16.to_le_bytes()); // reserved
    icon.extend_from_slice(&1_u16.to_le_bytes()); // type: icon
    icon.extend_from_slice(&1_u16.to_le_bytes()); // 单张图
    icon.push(entry.width);
    icon.push(entry.height);
    icon.push(entry.color_count);
    icon.push(0); // reserved
    icon.extend_from_slice(&entry.planes.to_le_bytes());
    icon.extend_from_slice(&entry.bit_count.to_le_bytes());
    icon.extend_from_slice(&u32::try_from(image.len()).unwrap_or(u32::MAX).to_le_bytes());
    icon.extend_from_slice(&IMAGE_OFFSET.to_le_bytes());
    icon.extend_from_slice(image);
    icon
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([raw[0], raw[1]]))
}

fn write_display_name() -> Result<(), AppError> {
    let key_path = wide(&format!(
        "Software\\Classes\\AppUserModelId\\{APP_USER_MODEL_ID}"
    ));
    let display_name = wide("DisplayName");
    let icon_uri = wide("IconUri");
    let name = wide(APP_DISPLAY_NAME);
    let icon = materialize_icon().map(|path| wide(&path.display().to_string()));
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
            "无法创建应用标识项：{}",
            status.0
        )));
    }
    // SAFETY: UTF-16 values are reinterpreted as bytes including their terminating NUL.
    let name_bytes = unsafe {
        std::slice::from_raw_parts(name.as_ptr().cast::<u8>(), name.len() * size_of::<u16>())
    };
    // SAFETY: key is open for KEY_SET_VALUE and name_bytes is a complete REG_SZ value.
    let mut result = unsafe {
        RegSetValueExW(
            key,
            PCWSTR(display_name.as_ptr()),
            None,
            REG_SZ,
            Some(name_bytes),
        )
    };
    if result == ERROR_SUCCESS
        && let Some(icon) = icon
    {
        // SAFETY: same as above, with the executable path as the icon source.
        let icon_bytes = unsafe {
            std::slice::from_raw_parts(icon.as_ptr().cast::<u8>(), icon.len() * size_of::<u16>())
        };
        // SAFETY: key is still open for KEY_SET_VALUE and icon_bytes is a complete REG_SZ value.
        result = unsafe {
            RegSetValueExW(
                key,
                PCWSTR(icon_uri.as_ptr()),
                None,
                REG_SZ,
                Some(icon_bytes),
            )
        };
    }
    // SAFETY: key is owned by this function and is closed exactly once.
    let _ = unsafe { RegCloseKey(key) };
    if result != ERROR_SUCCESS {
        return Err(AppError::Windows(format!(
            "无法写入应用显示名：{}",
            result.0
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一份 `RT_GROUP_ICON` 目录：3 个 u16 头 + 每个 14 字节的条目。
    fn group_directory(entries: &[(u8, u8, u16)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(
            &u16::try_from(entries.len())
                .expect("条目数不该溢出")
                .to_le_bytes(),
        );
        for (width, height, image_id) in entries {
            bytes.push(*width);
            bytes.push(*height);
            bytes.push(0);
            bytes.push(0);
            bytes.extend_from_slice(&1_u16.to_le_bytes());
            bytes.extend_from_slice(&32_u16.to_le_bytes());
            bytes.extend_from_slice(&100_u32.to_le_bytes());
            bytes.extend_from_slice(&image_id.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn the_entry_closest_to_the_notification_icon_size_wins() {
        let directory = group_directory(&[(16, 16, 1), (32, 32, 2), (48, 48, 3), (0, 0, 4)]);

        assert_eq!(pick_entry(&directory).map(|entry| entry.image_id), Some(2));

        // 没有 32 时取更接近的那档；0 表示 256，不该被误当成 0。
        let without_32 = group_directory(&[(16, 16, 1), (64, 64, 2)]);
        assert_eq!(pick_entry(&without_32).map(|entry| entry.image_id), Some(1));
        let large = group_directory(&[(48, 48, 1), (0, 0, 2)]);
        assert_eq!(pick_entry(&large).map(|entry| entry.image_id), Some(1));
    }

    #[test]
    fn a_truncated_directory_yields_no_entry() {
        let mut directory = group_directory(&[(32, 32, 1)]);
        directory.truncate(directory.len() - 2);

        assert_eq!(pick_entry(&directory), None);
    }

    #[test]
    fn assembling_puts_the_image_right_after_a_single_entry_header() {
        let entry = IconEntry {
            width: 32,
            height: 32,
            color_count: 0,
            planes: 1,
            bit_count: 32,
            image_id: 7,
        };
        let icon = assemble_icon(entry, &[0xAA; 40]);

        assert_eq!(&icon[..6], &[0, 0, 1, 0, 1, 0], "单图 ICONDIR");
        assert_eq!((icon[6], icon[7]), (32, 32));
        assert_eq!(u16::from_le_bytes([icon[12], icon[13]]), 32, "位深");
        assert_eq!(
            u32::from_le_bytes([icon[14], icon[15], icon[16], icon[17]]),
            40
        );
        assert_eq!(
            u32::from_le_bytes([icon[18], icon[19], icon[20], icon[21]]),
            22
        );
        assert_eq!(icon.len(), 22 + 40);
        assert!(
            icon[22..].iter().all(|byte| *byte == 0xAA),
            "图像数据原样拼接"
        );
    }
}
