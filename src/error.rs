use std::io;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("未找到可用的 Codex CLI")]
    CliNotFound,
    #[error("无法启动 Codex app-server：{0}")]
    Spawn(String),
    #[error("Codex app-server 通信失败：{0}")]
    Io(#[from] io::Error),
    #[error("Codex app-server 返回了无效 JSON：{0}")]
    Json(#[from] serde_json::Error),
    #[error("Codex app-server 协议错误：{0}")]
    Protocol(String),
    #[error("Codex 尚未登录或认证已失效：{0}")]
    Authentication(String),
    #[error("等待 Codex app-server 响应超时")]
    Timeout,
    #[error("Windows 操作失败：{0}")]
    Windows(String),
    #[error("渲染失败：{0}")]
    Render(String),
    #[error("配置读写失败：{0}")]
    Config(String),
}

impl From<windows::core::Error> for AppError {
    fn from(value: windows::core::Error) -> Self {
        Self::Windows(value.to_string())
    }
}
