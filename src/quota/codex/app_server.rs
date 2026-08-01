use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use super::protocol::classify_rpc_error;
use crate::error::AppError;

pub(crate) fn find_codex_executable() -> Result<PathBuf, AppError> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let installed = PathBuf::from(local_app_data)
            .join("Programs")
            .join("OpenAI")
            .join("Codex")
            .join("bin")
            .join("codex.exe");
        if installed.is_file() {
            return Ok(installed);
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("codex.exe");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(AppError::CliNotFound)
}

pub(super) struct AppServerSession {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_rx: Receiver<String>,
    pending_lines: VecDeque<String>,
    pending_requests: HashMap<u64, PendingRequest>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
    job: Option<JobHandle>,
}

impl AppServerSession {
    pub(super) fn spawn(
        executable: &PathBuf,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, AppError> {
        let mut child = Command::new(executable)
            .args(["app-server", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .map_err(|error| AppError::Spawn(error.to_string()))?;

        let job = JobHandle::new_and_assign(child.as_raw_handle()).inspect_err(|_| {
            let _ = child.kill();
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Spawn("未创建 app-server 标准输入".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Spawn("未创建 app-server 标准输出".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::Spawn("未创建 app-server 标准错误".to_owned()))?;

        let (stdout_tx, stdout_rx) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Some(method) = server_notification_method(&line) {
                    crate::logging::log(&format!("RPC ← {method}"));
                }
                if stdout_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr_thread = thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else {
                    break;
                };
                let lowercase = line.to_ascii_lowercase();
                if !lowercase.contains("token") && !lowercase.contains("authorization") {
                    crate::logging::log(&format!("app-server: {line}"));
                }
            }
        });

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout_rx,
            pending_lines: VecDeque::new(),
            pending_requests: HashMap::new(),
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            cancelled,
            job: Some(job),
        })
    }

    pub(super) fn send(&mut self, message: &Value) -> Result<(), AppError> {
        let rpc = outbound_rpc(message);
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| AppError::Protocol("app-server 标准输入已关闭".to_owned()))?;
        write_json_line(stdin, message)?;
        match rpc {
            Some(OutboundRpc::Request { id, method }) => {
                crate::logging::log(&format!("RPC → #{id} {method}"));
                self.pending_requests.insert(
                    id,
                    PendingRequest {
                        method,
                        started_at: Instant::now(),
                    },
                );
            }
            Some(OutboundRpc::Notification { method }) => {
                crate::logging::log(&format!("RPC → {method}"));
            }
            None => {}
        }
        Ok(())
    }

    pub(super) fn recv_line(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<String>, RecvTimeoutError> {
        if let Some(line) = self.pending_lines.pop_front() {
            return Ok(Some(line));
        }
        match self.stdout_rx.recv_timeout(timeout) {
            Ok(line) => Ok(Some(line)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn wait_for_response(
        &mut self,
        id: u64,
        timeout: Duration,
    ) -> Result<Value, AppError> {
        let result = wait_for_channel_response(
            &self.stdout_rx,
            &mut self.pending_lines,
            id,
            timeout,
            &self.cancelled,
        );
        let pending = self.pending_requests.remove(&id);
        log_rpc_response(id, pending.as_ref(), &result);
        result
    }

    pub(super) fn shutdown(&mut self) {
        self.stdin = None;
        self.job = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

struct PendingRequest {
    method: String,
    started_at: Instant,
}

#[derive(Debug, PartialEq, Eq)]
enum OutboundRpc {
    Request { id: u64, method: String },
    Notification { method: String },
}

fn outbound_rpc(message: &Value) -> Option<OutboundRpc> {
    let method = message.get("method")?.as_str()?.to_owned();
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        Some(OutboundRpc::Request { id, method })
    } else {
        Some(OutboundRpc::Notification { method })
    }
}

fn server_notification_method(line: &str) -> Option<String> {
    let message: Value = serde_json::from_str(line).ok()?;
    if message.get("id").is_some() {
        return None;
    }
    message.get("method")?.as_str().map(str::to_owned)
}

fn log_rpc_response(id: u64, pending: Option<&PendingRequest>, result: &Result<Value, AppError>) {
    let method = pending.map_or("未知方法", |request| request.method.as_str());
    let elapsed = pending.map_or(0, |request| request.started_at.elapsed().as_millis());
    let status = match result {
        Ok(_) => "成功",
        Err(AppError::Timeout) => "超时",
        Err(AppError::Cancelled) => "已取消",
        Err(_) => "失败",
    };
    crate::logging::log(&format!("RPC ← #{id} {method} {status} {elapsed}ms"));
}

impl Drop for AppServerSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn write_json_line<W: Write>(writer: &mut W, message: &Value) -> Result<(), AppError> {
    serde_json::to_writer(&mut *writer, message)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn wait_for_channel_response(
    receiver: &Receiver<String>,
    pending_lines: &mut VecDeque<String>,
    id: u64,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<Value, AppError> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(AppError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AppError::Timeout);
        }
        let line = match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => continue,
            Err(RecvTimeoutError::Timeout) => return Err(AppError::Timeout),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(AppError::Protocol("app-server 已关闭标准输出".to_owned()));
            }
        };
        if let Some(result) = decode_response_for_id(&line, id)? {
            return Ok(result);
        }
        pending_lines.push_back(line);
    }
}

fn decode_response_for_id(line: &str, id: u64) -> Result<Option<Value>, AppError> {
    let message: Value = serde_json::from_str(line)?;
    if message.get("id").and_then(Value::as_u64) != Some(id) {
        return Ok(None);
    }
    if let Some(error) = message.get("error") {
        return Err(classify_rpc_error(error));
    }
    message
        .get("result")
        .cloned()
        .map(Some)
        .ok_or_else(|| AppError::Protocol("响应缺少 result 字段".to_owned()))
}

struct JobHandle(HANDLE);

impl JobHandle {
    fn new_and_assign(process: RawHandle) -> Result<Self, AppError> {
        // SAFETY: A null security descriptor and name create a private job owned by this process.
        let job = unsafe { CreateJobObjectW(None, None)? };
        let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `information` has the exact layout required by the selected information class.
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const information).cast::<c_void>(),
                u32::try_from(std::mem::size_of_val(&information))
                    .map_err(|_| AppError::Windows("Job Object 结构大小溢出".to_owned()))?,
            )?;
            AssignProcessToJobObject(job, HANDLE(process))?;
        }
        Ok(Self(job))
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // SAFETY: The handle was created by CreateJobObjectW and is closed exactly once here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::*;

    fn read_matching_response<R: BufRead>(reader: &mut R, id: u64) -> Result<Value, AppError> {
        for line in reader.lines() {
            if let Some(result) = decode_response_for_id(&line?, id)? {
                return Ok(result);
            }
        }
        Err(AppError::Protocol("输入结束前未收到对应响应".to_owned()))
    }

    #[test]
    fn protocol_writer_emits_one_json_line() {
        let mut output = Cursor::new(Vec::new());
        let result = write_json_line(&mut output, &json!({ "id": 7, "method": "ping" }));
        let bytes = output.into_inner();
        assert!(result.is_ok());
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert!(serde_json::from_slice::<Value>(&bytes).is_ok());
    }

    #[test]
    fn outbound_rpc_keeps_only_request_id_and_method() {
        let event = outbound_rpc(&json!({
            "id": 7,
            "method": "account/read",
            "params": { "secret": "must-not-be-logged" }
        }));

        assert_eq!(
            event,
            Some(OutboundRpc::Request {
                id: 7,
                method: "account/read".to_owned()
            })
        );
    }

    #[test]
    fn server_notification_keeps_only_method_name() {
        let method = server_notification_method(
            r#"{"method":"account/updated","params":{"secret":"must-not-be-logged"}}"#,
        );

        assert_eq!(method.as_deref(), Some("account/updated"));
    }

    #[test]
    fn response_is_not_classified_as_server_notification() {
        let method = server_notification_method(
            r#"{"id":7,"method":"account/updated","result":{"ok":true}}"#,
        );

        assert!(method.is_none());
    }

    #[test]
    fn buffered_reader_correlates_response_id() {
        let input = concat!(
            "{\"method\":\"account/rateLimits/updated\"}\n",
            "{\"id\":1,\"result\":{\"ignored\":true}}\n",
            "{\"id\":2,\"result\":{\"matched\":true}}\n"
        );
        let mut reader = Cursor::new(input.as_bytes());
        let response = read_matching_response(&mut reader, 2);
        assert!(response.is_ok_and(|value| value["matched"] == true));
    }

    #[test]
    fn channel_response_wait_times_out() {
        let (_sender, receiver) = mpsc::channel();
        let cancelled = AtomicBool::new(false);
        let result = wait_for_channel_response(
            &receiver,
            &mut VecDeque::new(),
            1,
            Duration::from_millis(1),
            &cancelled,
        );
        assert!(matches!(result, Err(AppError::Timeout)));
    }

    #[test]
    fn channel_response_wait_preserves_interleaved_notification() {
        let (sender, receiver) = mpsc::channel();
        let notification = r#"{"method":"account/updated","params":{"planType":"pro"}}"#.to_owned();
        assert!(sender.send(notification.clone()).is_ok());
        assert!(
            sender
                .send(r#"{"id":2,"result":{"matched":true}}"#.to_owned())
                .is_ok()
        );
        let mut pending = VecDeque::new();
        let cancelled = AtomicBool::new(false);

        let result = wait_for_channel_response(
            &receiver,
            &mut pending,
            2,
            Duration::from_secs(1),
            &cancelled,
        );

        assert_eq!(
            (result.ok(), pending.pop_front()),
            (Some(json!({ "matched": true })), Some(notification))
        );
    }

    #[test]
    fn channel_response_wait_stops_when_cancelled() {
        let (_sender, receiver) = mpsc::channel();
        let cancelled = AtomicBool::new(true);

        let result = wait_for_channel_response(
            &receiver,
            &mut VecDeque::new(),
            1,
            Duration::from_secs(10),
            &cancelled,
        );

        assert!(matches!(result, Err(AppError::Cancelled)));
    }
}
