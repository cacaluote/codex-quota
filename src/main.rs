#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    if let Err(error) = codex_quota::win32::run() {
        codex_quota::logging::log(&format!("应用退出：{error}"));
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("codex-quota only supports Windows");
}
