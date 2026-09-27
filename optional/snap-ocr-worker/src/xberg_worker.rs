//! Xberg 推理子进程客户端：`xberg worker` 本地 stdio JSON 行协议（XB-05/XB-08）。
//!
//! 每个 snap-ocr 服务持有一个常驻 `xberg worker` 子进程：请求写 stdin、响应读
//! stdout，`id` 关联；模型由 Xberg 侧在首个 `ocr_snapshot` 时懒加载并跨请求复用
//! （O-13 常驻语义）。生命周期由调用方管理：服务退出时关闭 stdin，等待在途请求
//! 完成后子进程正常退出，超时才强杀（O-16）。
//!
//! 取消（O-19）语义：共享 stdio 无法对单个请求取消，`recognize` 收到取消后立即
//! 返回 [`ClientError::Cancelled`]（结果窗即刻恢复），在途识别在 Xberg 侧完成后
//! 其响应因无等待者而被丢弃；发送新请求前会清理这些过期条目。
//!
//! 协议纯度：stdout 只承载协议行；Xberg 的诊断日志走 stderr，这里直接丢弃
//! （O-29：不落盘、不进日志）。截图字节只在内存中经 base64 传递（O-29）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

/// 等待响应的轮询间隔：远小于一次识别耗时，同时保证取消及时生效。
const POLL_INTERVAL: Duration = Duration::from_millis(8);

/// 客户端错误（O-30 分类：取消 / 推理失败 / 子进程退出 / 通信失败）。
#[derive(Debug, Clone)]
pub enum ClientError {
    /// 用户取消：在途请求被放弃，其结果随后台完成被丢弃。
    Cancelled,
    /// Xberg 返回的失败响应（`ok:false`；消息不含图像内容）。
    Backend(String),
    /// 子进程已退出（携带已知时的退出码）。
    ProcessExited(Option<i32>),
    /// 与子进程的 stdin 通信失败。
    Io(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "用户取消识别"),
            Self::Backend(message) => write!(formatter, "{message}"),
            Self::ProcessExited(code) => match code {
                Some(code) => write!(formatter, "推理子进程已退出（退出码 {code}）"),
                None => write!(formatter, "推理子进程已退出"),
            },
            Self::Io(message) => write!(formatter, "与推理子进程的通信失败：{message}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// `snapshot_state` 报告的截图通道状态（Xberg 侧 SNAP-17）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotState {
    /// 尚未发起过识别请求（模型懒加载前）。
    Uninitialized,
    /// 模型正在加载（仅在请求处理中可观察）。
    Loading,
    /// 模型已就绪且常驻。
    Ready,
    /// 上次加载失败；携带一行错误摘要。
    Error(String),
}

impl SnapshotState {
    /// 由协议字符串与错误摘要构造；未知字符串按错误处理，不冒称就绪。
    fn parse(state: &str, error: Option<&str>) -> Self {
        match state {
            "uninitialized" => Self::Uninitialized,
            "loading" => Self::Loading,
            "ready" => Self::Ready,
            "error" => Self::Error(error.unwrap_or_default().to_owned()),
            other => Self::Error(format!("未知的推理通道状态：{other}")),
        }
    }

    /// O-13 的模型状态字符串（与主程序/管道协议的取值一致）。
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Uninitialized => "uninitialized",
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Error(_) => "error",
        }
    }
}

/// 常驻 `xberg worker` 子进程的协议客户端。
///
/// 仅供服务的工作线程使用（串行请求）；响应由独立读取线程按 `id` 收纳，
/// 等待方轮询取回，取消检查点在轮询间隙（O-19）。
pub struct XbergWorkerClient {
    stdin: Option<ChildStdin>,
    child: Child,
    responses: Arc<Mutex<HashMap<u64, Value>>>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}

impl std::fmt::Debug for XbergWorkerClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("XbergWorkerClient")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl XbergWorkerClient {
    /// 生产入口：从 Xberg 组件目录启动 `worker` 子进程。
    ///
    /// 组件目录须含 `xberg.exe`、`onnxruntime.dll` 与 `models/snapshot-ocr`
    /// 模型集；启动配置把模型根固定为组件内绝对路径（XB-06：配置在启动时固定，
    /// 请求不再携带配置），`ORT_DYLIB_PATH` 只注入子进程环境。
    ///
    /// # Errors
    /// 子进程启动失败。
    pub fn spawn(component_dir: &Path) -> Result<Self, String> {
        let exe = component_dir.join("xberg.exe");
        let models = component_dir.join("models").join("snapshot-ocr");
        let config = json!({"snapshot_ocr": {"models_dir": absolute(&models)}});
        let mut command = Command::new(&exe);
        command
            .arg("worker")
            // 启动配置在启动时固定且只含本功能需要的能力（XB-06）；跳过配置
            // 文件自动发现，避免工作目录里的无关 xberg 配置影响服务启动。
            .arg("--no-config-discovery")
            .arg("--config-json")
            .arg(config.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // 诊断日志一律走 stderr；丢弃即可（O-29 不落盘）。
            .stderr(Stdio::null());
        command.env("ORT_DYLIB_PATH", component_dir.join("onnxruntime.dll"));
        Self::spawn_command(command)
    }

    /// 注入入口：执行给定命令并建立协议会话（测试用模拟子进程走这里）。
    ///
    /// # Errors
    /// 子进程启动失败。
    pub fn spawn_command(mut command: Command) -> Result<Self, String> {
        let mut child = command
            .spawn()
            .map_err(|error| format!("无法启动推理子进程：{error}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
            let _ = child.kill();
            return Err("推理子进程的标准流不可用".to_owned());
        };
        let responses = Arc::new(Mutex::new(HashMap::new()));
        let reader_responses = Arc::clone(&responses);
        let reader = std::thread::Builder::new()
            .name("snap-ocr-xberg-reader".into())
            .spawn(move || pump(BufReader::new(stdout), reader_responses))
            .map_err(|error| format!("推理响应读取线程启动失败：{error}"))?;
        Ok(Self {
            stdin: Some(stdin),
            child,
            responses,
            reader: Some(reader),
            next_id: 1,
        })
    }

    /// 查询截图通道状态（`snapshot_state`）。
    ///
    /// # Errors
    /// 子进程退出或通信失败；状态查询本身的失败响应也归入 [`ClientError::Backend`]。
    pub fn snapshot_state(&mut self) -> Result<SnapshotState, ClientError> {
        let id = self.send_request(json!({"command": "snapshot_state"}))?;
        let response = self.wait_response(id, &AtomicBool::new(false))?;
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            let message = failure_message(&response);
            return Err(ClientError::Backend(message));
        }
        let state = response
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let error = response.get("error").and_then(Value::as_str);
        Ok(SnapshotState::parse(state, error))
    }

    /// 识别一张内存 PNG（`ocr_snapshot`）。
    ///
    /// 成功返回布局文本；无文字图片返回 `Ok(None)`（Xberg 侧 `ok:true` +
    /// `text:""` + `error_kind:"no_text"`）。取消在轮询间隙生效（O-19 细化：
    /// 在途识别后台完成后被丢弃）。
    ///
    /// # Errors
    /// 取消、子进程退出、通信失败或 Xberg 失败响应。
    pub fn recognize(
        &mut self,
        png: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Option<String>, ClientError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(png);
        let id = self.send_request(json!({"command": "ocr_snapshot", "image_base64": encoded}))?;
        let response = self.wait_response(id, cancel)?;
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(ClientError::Backend(failure_message(&response)));
        }
        let text = response.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(text.to_owned()))
        }
    }

    /// 优雅关闭：关闭 stdin，等在途请求完成后子进程自行退出；超时强杀兜底
    /// （O-16 的进程级兜底，服务侧另有 10 秒「强制退出」入口）。
    ///
    /// # Errors
    /// 等待/终止子进程失败（已尽力清理）。
    pub fn shutdown(mut self, timeout: Duration) -> Result<(), String> {
        self.stdin.take(); // 关闭 stdin：EOF 即批次结束信号（WORKER.md 退出语义）。
        let deadline = Instant::now() + timeout;
        let mut failure: Option<String> = None;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    if let Err(error) = self.child.kill() {
                        failure = Some(format!("强杀推理子进程失败：{error}"));
                    }
                    let _ = self.child.wait();
                    break;
                }
                Ok(None) => std::thread::sleep(POLL_INTERVAL),
                Err(error) => {
                    failure = Some(format!("推理子进程状态不可读：{error}"));
                    break;
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        match failure {
            Some(message) => Err(message),
            None => Ok(()),
        }
    }

    /// 发送一行请求并返回关联 `id`；同时清理已放弃请求的过期响应。
    fn send_request(&mut self, mut request: Value) -> Result<u64, ClientError> {
        let id = self.next_id;
        self.next_id = id.wrapping_add(1).max(1);
        request["id"] = json!(id);
        if let Ok(mut responses) = self.responses.lock() {
            responses.retain(|&pending, _| pending >= id);
        }
        let line = request.to_string();
        let stdin = self
            .stdin
            .as_mut()
            .ok_or(ClientError::ProcessExited(None))?;
        let written = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush());
        match written {
            Ok(()) => Ok(id),
            // 写入失败通常意味着子进程已退出；给出可重试的分类错误（O-13）。
            Err(error) => {
                if self.child.try_wait().ok().flatten().is_some() {
                    Err(ClientError::ProcessExited(self.child_exit_code()))
                } else {
                    Err(ClientError::Io(error.to_string()))
                }
            }
        }
    }

    /// 轮询等待指定 `id` 的响应；取消在轮询间隙生效。
    fn wait_response(&mut self, id: u64, cancel: &AtomicBool) -> Result<Value, ClientError> {
        loop {
            if let Some(response) = self
                .responses
                .lock()
                .ok()
                .and_then(|mut map| map.remove(&id))
            {
                return Ok(response);
            }
            if cancel.load(Ordering::Acquire) {
                return Err(ClientError::Cancelled);
            }
            if self
                .child
                .try_wait()
                .map_err(|error| ClientError::Io(error.to_string()))?
                .is_some()
            {
                // 先再取一次响应（可能已写出但线程尚未收纳），随后如实报告退出。
                if let Some(response) = self
                    .responses
                    .lock()
                    .ok()
                    .and_then(|mut map| map.remove(&id))
                {
                    return Ok(response);
                }
                return Err(ClientError::ProcessExited(self.child_exit_code()));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn child_exit_code(&mut self) -> Option<i32> {
        self.child
            .try_wait()
            .ok()
            .flatten()
            .and_then(|status| status.code())
    }
}

impl Drop for XbergWorkerClient {
    fn drop(&mut self) {
        // 非.shutdown 路径（如 Load 失败替换旧客户端）也保证不遗留子进程：
        // 关闭 stdin 请求正常退出，短暂等待后强杀。
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.child.try_wait().map_or(true, |state| state.is_none()) {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// 响应读取循环：逐行解析 stdout，把响应按 `id` 收纳；EOF（子进程退出）或
/// 解析失败只是结束读取，不影响已收纳的响应被取回。
// Arc 按值是线程 'static 边界的要求，函数体内并未消费它。
#[expect(
    clippy::needless_pass_by_value,
    reason = "reader 线程需要拥有 Arc 才满足 'static"
)]
fn pump(reader: BufReader<ChildStdout>, responses: Arc<Mutex<HashMap<u64, Value>>>) {
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let Ok(mut map) = responses.lock() else {
            break;
        };
        map.insert(id, value);
    }
}

/// 失败响应中的一行错误描述（Xberg 合同保证不含图像内容）。
fn failure_message(response: &Value) -> String {
    response
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("推理子进程返回失败")
        .to_owned()
}

/// 绝对路径序列化：启动配置里的模型根必须是本地绝对路径（O-10）。
fn absolute(path: &Path) -> PathBuf {
    std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
}
