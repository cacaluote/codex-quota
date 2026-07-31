use std::ffi::OsStr;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

use crate::error::AppError;

const CONFIG_VERSION: u32 = 1;
pub const DEFAULT_QUOTA_REFRESH_INTERVAL_SECS: u64 = 5 * 60;
pub const MIN_QUOTA_REFRESH_INTERVAL_SECS: u64 = 60;
pub const MAX_QUOTA_REFRESH_INTERVAL_SECS: u64 = 60 * 60;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnchorEdge {
    Left,
    #[default]
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WindowPlacement {
    pub monitor_device: String,
    pub edge: AnchorEdge,
    pub offset_dip: f32,
}

impl Default for WindowPlacement {
    fn default() -> Self {
        Self {
            monitor_device: String::new(),
            edge: AnchorEdge::Right,
            offset_dip: 96.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfigV1 {
    pub version: u32,
    pub placement: WindowPlacement,
    pub always_on_top: bool,
    pub start_with_windows: bool,
    pub collapse_on_outside_click: bool,
    pub quota_refresh_interval_secs: u64,
}

impl Default for AppConfigV1 {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            placement: WindowPlacement::default(),
            always_on_top: true,
            start_with_windows: false,
            collapse_on_outside_click: true,
            quota_refresh_interval_secs: DEFAULT_QUOTA_REFRESH_INTERVAL_SECS,
        }
    }
}

impl AppConfigV1 {
    #[must_use]
    pub fn quota_refresh_interval(&self) -> Duration {
        Duration::from_secs(self.quota_refresh_interval_secs.clamp(
            MIN_QUOTA_REFRESH_INTERVAL_SECS,
            MAX_QUOTA_REFRESH_INTERVAL_SECS,
        ))
    }

    pub fn set_quota_refresh_interval(&mut self, interval: Duration) {
        self.quota_refresh_interval_secs = interval.as_secs().clamp(
            MIN_QUOTA_REFRESH_INTERVAL_SECS,
            MAX_QUOTA_REFRESH_INTERVAL_SECS,
        );
    }

    fn normalize(&mut self) {
        self.version = CONFIG_VERSION;
        self.set_quota_refresh_interval(self.quota_refresh_interval());
    }
}

/// Returns the per-user application data directory.
///
/// # Errors
///
/// Returns an error when `LOCALAPPDATA` is unavailable.
pub fn app_data_dir() -> Result<PathBuf, AppError> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| AppError::Config("LOCALAPPDATA 环境变量不存在".to_owned()))?;
    Ok(PathBuf::from(local_app_data).join("codex-quota"))
}

/// Returns the per-user configuration path.
///
/// # Errors
///
/// Returns an error when the application data directory cannot be resolved.
pub fn config_path() -> Result<PathBuf, AppError> {
    Ok(app_data_dir()?.join("config.json"))
}

/// Loads the configuration, preserving a malformed file before using defaults.
///
/// # Errors
///
/// Returns an error when the configuration path cannot be read or preserved.
pub fn load() -> Result<AppConfigV1, AppError> {
    let path = config_path()?;
    load_from(&path)
}

/// Atomically saves the configuration for the current user.
///
/// # Errors
///
/// Returns an error when serialization, directory creation, or file replacement fails.
pub fn save(config: &AppConfigV1) -> Result<(), AppError> {
    let path = config_path()?;
    save_to(&path, config)
}

fn load_from(path: &Path) -> Result<AppConfigV1, AppError> {
    if !path.exists() {
        return Ok(AppConfigV1::default());
    }

    let content = fs::read_to_string(path).map_err(|error| AppError::Config(error.to_string()))?;
    match serde_json::from_str::<AppConfigV1>(&content) {
        Ok(mut config) => {
            config.normalize();
            Ok(config)
        }
        Err(error) => {
            preserve_invalid_config(path)?;
            crate::logging::log(&format!("配置文件损坏，已回退默认值：{error}"));
            Ok(AppConfigV1::default())
        }
    }
}

fn save_to(path: &Path, config: &AppConfigV1) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("配置路径没有父目录".to_owned()))?;
    fs::create_dir_all(parent).map_err(|error| AppError::Config(error.to_string()))?;

    let temp_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(config)?;
    fs::write(&temp_path, bytes).map_err(|error| AppError::Config(error.to_string()))?;

    let from = wide_null(temp_path.as_os_str());
    let to = wide_null(path.as_os_str());
    // SAFETY: Both UTF-16 buffers are NUL-terminated and remain alive for the duration of the call.
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(|error| AppError::Config(error.to_string()))?;
    }
    Ok(())
}

fn preserve_invalid_config(path: &Path) -> Result<(), AppError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let backup = path.with_file_name(format!("config.invalid-{timestamp}.json"));
    fs::rename(path, backup).map_err(|error| AppError::Config(error.to_string()))
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("codex-quota-{name}-{}", std::process::id()))
    }

    #[test]
    fn missing_config_returns_defaults() {
        let directory = unique_test_dir("missing");
        let path = directory.join("config.json");
        let _ = fs::remove_dir_all(&directory);
        assert_eq!(load_from(&path).ok(), Some(AppConfigV1::default()));
    }

    #[test]
    fn saved_config_round_trips() {
        let directory = unique_test_dir("roundtrip");
        let path = directory.join("config.json");
        let _ = fs::remove_dir_all(&directory);
        let mut config = AppConfigV1 {
            always_on_top: false,
            ..AppConfigV1::default()
        };
        config.set_quota_refresh_interval(Duration::from_mins(10));
        let result = save_to(&path, &config).and_then(|()| load_from(&path));
        let _ = fs::remove_dir_all(&directory);
        assert_eq!(result.ok(), Some(config));
    }

    #[test]
    fn legacy_config_enables_outside_click_collapse_by_default() {
        let config = serde_json::from_str::<AppConfigV1>(
            r#"{
                "version": 1,
                "placement": {
                    "monitor_device": "",
                    "edge": "right",
                    "offset_dip": 96.0
                },
                "always_on_top": true,
                "start_with_windows": false
            }"#,
        );
        assert_eq!(
            config.ok().map(|value| value.collapse_on_outside_click),
            Some(true)
        );
    }

    #[test]
    fn legacy_config_uses_five_minute_refresh_interval() {
        let config = serde_json::from_str::<AppConfigV1>(
            r#"{
                "version": 1,
                "placement": {
                    "monitor_device": "",
                    "edge": "right",
                    "offset_dip": 96.0
                },
                "always_on_top": true,
                "start_with_windows": false
            }"#,
        );
        assert_eq!(
            config.ok().map(|value| value.quota_refresh_interval()),
            Some(Duration::from_mins(5))
        );
    }

    #[test]
    fn loaded_refresh_interval_is_clamped_to_supported_range() {
        let directory = unique_test_dir("refresh-clamp");
        let path = directory.join("config.json");
        let _ = fs::remove_dir_all(&directory);
        assert!(fs::create_dir_all(&directory).is_ok());
        assert!(
            fs::write(
                &path,
                r#"{
                    "version": 1,
                    "quota_refresh_interval_secs": 1
                }"#,
            )
            .is_ok()
        );
        let loaded = load_from(&path);
        let _ = fs::remove_dir_all(&directory);
        assert_eq!(
            loaded.ok().map(|value| value.quota_refresh_interval()),
            Some(Duration::from_mins(1))
        );
    }

    #[test]
    fn refresh_interval_setter_clamps_values_above_one_hour() {
        let mut config = AppConfigV1::default();
        config.set_quota_refresh_interval(Duration::from_hours(2));
        assert_eq!(config.quota_refresh_interval(), Duration::from_hours(1));
    }

    #[test]
    fn malformed_config_is_preserved_and_defaults_are_returned() {
        let directory = unique_test_dir("malformed");
        let path = directory.join("config.json");
        let _ = fs::remove_dir_all(&directory);
        assert!(fs::create_dir_all(&directory).is_ok());
        assert!(fs::write(&path, "not-json").is_ok());
        let loaded = load_from(&path);
        let backup_exists = fs::read_dir(&directory).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config.invalid-")
            })
        });
        let _ = fs::remove_dir_all(&directory);
        assert!(loaded.is_ok_and(|config| config == AppConfigV1::default()) && backup_exists);
    }
}
