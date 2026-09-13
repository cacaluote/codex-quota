//! 通知去重状态：只记录"已经通知过余额的周期"，防止缓存重建之后重复弹。
//!
//! 独立于 `config.json`：那是用户设置，损坏时会回退默认，把运行时状态混进去
//! 会让"配置损坏"顺带重置通知记录。这里损坏只意味着最多多弹一次。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::app_data_dir;

const STATE_FILE_NAME: &str = "notify-state.json";
const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyState {
    /// 结构版本，便于以后调整字段。
    pub version: u32,
    /// 已经因"首次动用余额"通知过的周期键（长窗口的重置时间，UNIX 秒）。
    pub notified_overflow_period: Option<i64>,
}

impl Default for NotifyState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            notified_overflow_period: None,
        }
    }
}

/// 读取通知状态；文件缺失或损坏都按"什么都没通知过"处理。
#[must_use]
pub fn load() -> NotifyState {
    state_path().map_or_else(|_| NotifyState::default(), |path| load_from(&path))
}

/// 写入通知状态；失败只记日志——最坏结果是多弹一次通知。
pub fn save(state: &NotifyState) {
    if let Ok(path) = state_path() {
        save_to(&path, state);
    }
}

fn state_path() -> Result<PathBuf, crate::error::AppError> {
    Ok(app_data_dir()?.join(STATE_FILE_NAME))
}

fn load_from(path: &Path) -> NotifyState {
    let Ok(text) = fs::read_to_string(path) else {
        return NotifyState::default();
    };
    let mut state = serde_json::from_str::<NotifyState>(&text).unwrap_or_default();
    state.version = STATE_VERSION;
    state
}

fn save_to(path: &Path, state: &NotifyState) {
    if let Some(parent) = path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        crate::logging::log(&format!("无法创建通知状态目录：{error}"));
        return;
    }
    let Ok(json) = serde_json::to_vec_pretty(state) else {
        return;
    };
    // 先写临时文件再改名：rename 在 Windows 上会替换同名文件，不会留下半截内容。
    let temp = path.with_extension("json.tmp");
    if let Err(error) = fs::write(&temp, json).and_then(|()| fs::rename(&temp, path)) {
        crate::logging::log(&format!("无法写入通知状态：{error}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("codex-quota-notify-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        directory.join(STATE_FILE_NAME)
    }

    #[test]
    fn missing_state_reads_as_nothing_notified() {
        let path = unique_path("missing");
        assert_eq!(load_from(&path), NotifyState::default());
    }

    #[test]
    fn state_round_trips() {
        let path = unique_path("roundtrip");
        let state = NotifyState {
            notified_overflow_period: Some(1_800_000_000),
            ..NotifyState::default()
        };

        save_to(&path, &state);
        let loaded = load_from(&path);
        let _ = fs::remove_dir_all(path.parent().unwrap());

        assert_eq!(loaded, state);
    }

    #[test]
    fn overwriting_keeps_only_the_latest_key() {
        let path = unique_path("overwrite");
        save_to(
            &path,
            &NotifyState {
                notified_overflow_period: Some(1),
                ..NotifyState::default()
            },
        );
        save_to(
            &path,
            &NotifyState {
                notified_overflow_period: Some(2),
                ..NotifyState::default()
            },
        );

        let loaded = load_from(&path);
        let _ = fs::remove_dir_all(path.parent().unwrap());

        assert_eq!(loaded.notified_overflow_period, Some(2));
    }

    #[test]
    fn corrupted_state_falls_back_to_defaults() {
        let path = unique_path("corrupted");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not-json").unwrap();

        let loaded = load_from(&path);
        let _ = fs::remove_dir_all(path.parent().unwrap());

        assert_eq!(loaded, NotifyState::default());
    }

    #[test]
    fn a_state_file_without_the_version_field_is_still_read() {
        let path = unique_path("legacy");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{ "notified_overflow_period": 42 }"#).unwrap();

        let loaded = load_from(&path);
        let _ = fs::remove_dir_all(path.parent().unwrap());

        assert_eq!(loaded.notified_overflow_period, Some(42));
        assert_eq!(loaded.version, STATE_VERSION);
    }
}
