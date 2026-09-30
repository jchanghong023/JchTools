//! Xberg 推理子进程客户端：`xberg worker` 本地 stdio JSON 行协议（XB-05/XB-08）。
//!
//! 每个 snap-ocr 服务持有一个常驻 `xberg worker` 子进程：请求写 stdin、响应读
//! stdout，`id` 关联；模型由 Xberg 侧在首个 `ocr_snapshot` 时懒加载并跨请求复用
//! （O-13 常驻语义）。生命周期由调用方管理：服务退出时关闭 stdin，等待在途请求
//! 完成后子进程正常退出，超时才强杀（O-16）。
//!
//! 进程级兜底（E2'-1/O-16）：子进程一律加入 kill-on-close Job（`InferenceJob`，
//! 句柄由本客户端持有）。服务进程无论经哪条路径死亡——「强制退出」的
//! `std::process::exit(0)`（不运行任何 Drop）、被任务管理器等外部杀死、崩溃——
//! 内核都会在回收句柄时终结组内全部进程，卡在不可中断推理中的子进程不会成为
//! `CREATE_NO_WINDOW` 的无窗口孤儿。
//!
//! 取消（O-19/XB-08）语义：共享 stdio 无法对单个请求取消，取消走「终止进程」
//! 路径——`recognize` 收到取消后立即返回 [`ClientError::Cancelled`]（结果窗即刻
//! 恢复），调用方随后用 [`XbergWorkerClient::abort`] 杀死子进程，被放弃的请求
//! 随进程死亡、不再阻塞后续任务；服务侧触发重载（重新 spawn + 预热）。
//!
//! 请求级超时（XB-08/O-13）：Xberg worker 不实现内部超时（WORKER.md 故障职责
//! 边界：单文件计时与超时杀进程由调用方负责），挂起的子进程（进程活着但不读
//! stdin / 不写 stdout）若无客户端超时会让服务永久 busy。客户端从请求发出
//! （含写阶段）起按 `DEFAULT_REQUEST_TIMEOUT` 计时，超时返回
//! [`ClientError::Timeout`]；调用方应与「子进程退出」同路径终止子进程并降级
//! （超时是故障，与用户取消的自动重载语义区分）。
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

/// 请求级超时默认值（识别请求从发出到响应的上限，含写阶段）：给慢速冷启动
/// 模型加载与低速磁盘留足余量，同时保证挂起子进程不会让服务永久 busy。
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// 请求行分块写入的大小：分块只为在块间隙轮询取消与超时；管道写满且子进程
/// 不读时单次 WriteFile 仍会阻塞在内核，由 Windows 写入看门狗（CancelIoEx）
/// 在超时后取消挂起写入（见 [`write_request_line`]）。
const WRITE_CHUNK_BYTES: usize = 64 * 1024;

/// 客户端错误（O-30 分类：取消 / 推理失败 / 子进程退出 / 通信失败 / 超时）。
#[derive(Debug, Clone)]
pub enum ClientError {
    /// 用户取消：调用方应随后 `Self::abort` 终止子进程（XB-08）。
    Cancelled,
    /// Xberg 返回的失败响应（`ok:false`；消息不含图像内容）。`kind` 是响应的
    /// 结构化 `error_kind`，与 Xberg `snapshot_ocr.rs` 的取值全集对齐：
    /// `asset_invalid` / `input_invalid` / `no_text` / `cancelled` / `internal`
    /// （SNAP-15）；旧版 Xberg 或非快照失败响应可能缺失（`None`）。注意
    /// `ok:true` + `error_kind:"no_text"` 是无文字图片的成功响应，不走本变体。
    Backend {
        /// 一行用户可读的错误摘要（不含图像内容）。
        message: String,
        /// 结构化错误类别（缺失为 `None`）。
        kind: Option<String>,
    },
    /// 请求级超时：子进程在超时上限内未完成响应（进程可能仍活着但已挂起）。
    /// 调用方应 `Self::abort` 终止子进程并降级——超时是故障，与用户取消
    /// （自动重载）语义区分（XB-08/O-13）。
    Timeout,
    /// 子进程已退出（携带已知时的退出码）。
    ProcessExited(Option<i32>),
    /// 与子进程的 stdin 通信失败。
    Io(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "用户取消识别"),
            Self::Backend { message, .. } => write!(formatter, "{message}"),
            Self::Timeout => write!(formatter, "识别超时：推理子进程未在超时上限内响应"),
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
    /// kill-on-close Job 兜底（仅 Windows）：本客户端独占句柄，Drop 或持有
    /// 进程死亡时内核终结组内进程（O-16/E2'-1，见 [`InferenceJob`]）。
    #[cfg(windows)]
    job: Option<InferenceJob>,
    responses: Arc<Mutex<HashMap<u64, Value>>>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
    /// 请求级超时（从请求发出含写阶段起算）：见模块头「请求级超时」。
    request_timeout: Duration,
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
    /// 请求不再携带配置），环境变量（`ORT_DYLIB_PATH` + 离线开关）经
    /// `inference_environment` 注入。
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
        command.envs(inference_environment(component_dir));
        Self::spawn_command(command)
    }

    /// 注入入口：执行给定命令并建立协议会话（测试用模拟子进程走这里）。
    ///
    /// # Errors
    /// 子进程启动失败。
    pub fn spawn_command(mut command: Command) -> Result<Self, String> {
        // xberg.exe 是控制台子系统（CUI）程序，而本服务是 GUI 子系统、自身无
        // 控制台：不加 CREATE_NO_WINDOW（0x0800_0000）时，Windows 会为每个子进程
        // 新建常驻黑窗（占用任务栏，用户误关即杀死推理子进程，O-13 连锁失效）。
        // 与 src/process.rs、src/markdown.rs 对同一 xberg.exe 的既有处理一致。
        // 标志加在本注入入口而非仅 spawn()：测试 mock（同为 CUI）一并不弹窗。
        // 不叠加 CREATE_BREAKAWAY_FROM_JOB（0x0100_0000）：主程序 spawn 本服务时
        // 不挂 Job（src/gui.rs snap_supervisor_ensure），服务自身不在任何宿主
        // Job 内，子进程无需脱离；随后加入的是本客户端自建的 kill-on-close Job
        //（见下），CREATE_NO_WINDOW 与 Job 成员身份互不影响。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("无法启动推理子进程：{error}"))?;
        // kill-on-close Job 兜底（E2'-1/O-16）：服务进程经强退（exit(0) 不运行
        // Drop）、被外部杀死或崩溃死亡时，内核回收 Job 句柄即终结子进程及其
        // 后代；正常路径（abort/shutdown/Drop）仍走显式 kill，Job 只是兜底。
        // 与 src/markdown.rs `MediaWorker` 对 xberg.exe 的 Job attach 同一模式。
        #[cfg(windows)]
        let job = match InferenceJob::attach(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
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
            #[cfg(windows)]
            job: Some(job),
            responses,
            reader: Some(reader),
            next_id: 1,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        })
    }

    /// 覆盖请求级超时（测试注入短超时；生产保持 `DEFAULT_REQUEST_TIMEOUT`）。
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// 推理子进程的系统 PID：仅供诊断与回归测试从外部观察子进程存活性，
    /// 不参与协议。
    #[must_use]
    pub fn child_id(&self) -> u32 {
        self.child.id()
    }

    /// 查询截图通道状态（`snapshot_state`）。
    ///
    /// # Errors
    /// 子进程退出、通信失败或请求级超时；状态查询本身的失败响应也归入
    /// [`ClientError::Backend`]（携带结构化 `error_kind`，如有）。
    pub fn snapshot_state(&mut self) -> Result<SnapshotState, ClientError> {
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now() + self.request_timeout;
        let id = self.send_request(json!({"command": "snapshot_state"}), &cancel, deadline)?;
        let response = self.wait_response(id, &cancel, deadline)?;
        Self::require_success(&response)?;
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
    /// `text:""` + `error_kind:"no_text"`——这是成功响应，不是失败）。取消在
    /// 轮询间隙生效（O-19）；收到取消后由调用方终止子进程并重载（XB-08，见
    /// [`Self::abort`]）。请求级超时从请求发出（含写阶段）起算，超时返回
    /// [`ClientError::Timeout`]。
    ///
    /// # Errors
    /// 取消、超时、子进程退出、通信失败或 Xberg 失败响应。
    pub fn recognize(
        &mut self,
        png: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Option<String>, ClientError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(png);
        // 超时从请求发出（含写阶段）起算：Xberg worker 无内部超时（WORKER.md：
        // 超时杀进程是调用方职责），挂起的子进程由客户端按时判定。
        let deadline = Instant::now() + self.request_timeout;
        let id = self.send_request(
            json!({"command": "ocr_snapshot", "image_base64": encoded}),
            cancel,
            deadline,
        )?;
        let response = self.wait_response(id, cancel, deadline)?;
        Self::require_success(&response)?;
        let text = response.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(text.to_owned()))
        }
    }

    /// 用户取消的进程级终止（XB-08「终止进程」路径）：立即终止子进程所在进程
    /// 组并回收（Job 覆盖子进程的后代），不等在途识别完成。stdio 单连接无法
    /// 只取消单个请求，取消即整连接作废；客户端随后不可复用，后续识别须由
    /// 服务侧重载（重新 spawn，O-13）。
    pub fn abort(&mut self) {
        self.stdin.take();
        // 先终结整组（E2'-1：后代进程一并回收），子进程自身再显式 kill 兜底。
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
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

    /// 发送一行请求并返回关联 `id`；同时清理已放弃请求的过期响应。写入从
    /// `deadline`（请求发出时刻 + 请求级超时）起受取消与超时约束：4K 截图的
    /// base64 约 5MB，子进程挂起（不读 stdin）时一次性 `write_all` 会无限
    /// 阻塞在内核，分块 + 看门狗保证写阶段同样可取消/超时。
    fn send_request(
        &mut self,
        mut request: Value,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<u64, ClientError> {
        let id = self.next_id;
        self.next_id = id.wrapping_add(1).max(1);
        request["id"] = json!(id);
        if let Ok(mut responses) = self.responses.lock() {
            responses.retain(|&pending, _| pending >= id);
        }
        let line = request.to_string();
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(ClientError::ProcessExited(None));
        };
        match write_request_line(stdin, line.as_bytes(), cancel, deadline) {
            Ok(()) => Ok(id),
            Err(WriteStop::Cancelled) => Err(ClientError::Cancelled),
            Err(WriteStop::TimedOut) => Err(ClientError::Timeout),
            // 写入失败优先升级为进程退出分类（半死连接对同一连接 retry 无效，
            // 服务侧必须据此降级，不得把退出误报成 Io 让模型冒称就绪）；进程
            // 仍在且已到 deadline 时按超时报告。
            Err(WriteStop::Io(error)) => {
                if self.child.try_wait().ok().flatten().is_some() {
                    Err(ClientError::ProcessExited(self.child_exit_code()))
                } else if Instant::now() >= deadline {
                    Err(ClientError::Timeout)
                } else {
                    Err(ClientError::Io(error.to_string()))
                }
            }
        }
    }

    /// 从响应表取走指定请求的响应（轮询与进程退出前的最后一次捞取共用）。
    fn take_response(&self, id: u64) -> Option<Value> {
        self.responses
            .lock()
            .ok()
            .and_then(|mut map| map.remove(&id))
    }

    /// 非 `ok:true` 的响应统一转 [`ClientError::Backend`]（携带结构化
    /// `error_kind`，如有）。
    fn require_success(response: &Value) -> Result<(), ClientError> {
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            let (message, kind) = failure_parts(response);
            return Err(ClientError::Backend { message, kind });
        }
        Ok(())
    }

    /// 轮询等待指定 `id` 的响应；取消在轮询间隙生效；`deadline` 到期返回
    /// [`ClientError::Timeout`]。子进程退出优先于超时报告（已死的进程必须如实
    /// 报告退出而不是超时）。
    fn wait_response(
        &mut self,
        id: u64,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<Value, ClientError> {
        loop {
            if let Some(response) = self.take_response(id) {
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
                if let Some(response) = self.take_response(id) {
                    return Ok(response);
                }
                return Err(ClientError::ProcessExited(self.child_exit_code()));
            }
            if Instant::now() >= deadline {
                return Err(ClientError::Timeout);
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
///
/// 协议帧完整性（E'低危4）：stdout 只承载换行终止的 JSON 行。EOF 前最后一段
/// 不带换行终止的「半行」是截断/断连残留（进程已死或写侧损坏），不得当作有效
/// 响应收纳——否则半死连接的垃圾残行会被当成失败响应返回（Backend），调用方
/// 对同一连接 retry 必然无效；丢弃后由 [`XbergWorkerClient::wait_response`]
/// 的进程退出检测给出可降级的 [`ClientError::ProcessExited`] 分类（读侧先报
/// Io / 收到残行而 `child.try_wait()` 为 Some 时，统一升级为进程退出）。
// Arc 按值是线程 'static 边界的要求，函数体内并未消费它。
#[expect(
    clippy::needless_pass_by_value,
    reason = "reader 线程需要拥有 Arc 才满足 'static"
)]
fn pump(mut reader: BufReader<ChildStdout>, responses: Arc<Mutex<HashMap<u64, Value>>>) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            // EOF（子进程退出或写端关闭）与读错误（连接已坏）都只结束读取，
            // 不影响已收纳的响应；存活/退出判定交给 wait_response。
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if !line.ends_with('\n') {
                    // 半行：协议帧残缺，连接已死；不再读也不收纳该响应。
                    break;
                }
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
    }
}

/// 失败响应中的一行错误描述与结构化类别（Xberg 合同保证不含图像内容）。
///
/// `error_kind` 只在失败响应上读取：`ok:true` + `error_kind:"no_text"` 是
/// 无文字图片的成功响应（SNAP-15），不是失败，不得误判。
fn failure_parts(response: &Value) -> (String, Option<String>) {
    let message = response
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("推理子进程返回失败")
        .to_owned();
    let kind = response
        .get("error_kind")
        .and_then(Value::as_str)
        .map(str::to_owned);
    (message, kind)
}

/// 写阶段的中断原因（区分取消 / 超时 / 底层 IO，供调用方映射 ClientError）。
enum WriteStop {
    Cancelled,
    TimedOut,
    Io(std::io::Error),
}

impl From<std::io::Error> for WriteStop {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// 把一行请求（payload + 换行终止符）分块写入 stdin，块间检查取消与超时
/// （O-19/XB-08；卡死子进程不再把写阶段变成无限阻塞）。
///
/// Windows 上另有写入看门狗：管道写满且子进程不读时，单次 WriteFile 仍会阻塞
/// 在内核（分块间隙的检查救不了正在阻塞的这一次写入），看门狗在超时后用
/// `CancelIoEx` 取消 stdin 上挂起的写入，被取消的 WriteFile 以错误返回，写
/// 循环据此按时返回超时。被取消的请求行可能只写入了一半——协议流已损坏，
/// 调用方随后会按超时语义终止子进程，不会复用该连接。
fn write_request_line(
    stdin: &mut ChildStdin,
    payload: &[u8],
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<(), WriteStop> {
    #[cfg(windows)]
    let watchdog = WriteWatchdog::spawn(stdin, deadline);
    let result = (|| -> Result<(), WriteStop> {
        for chunk in payload.chunks(WRITE_CHUNK_BYTES) {
            if cancel.load(Ordering::Acquire) {
                return Err(WriteStop::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(WriteStop::TimedOut);
            }
            stdin.write_all(chunk)?;
        }
        if cancel.load(Ordering::Acquire) {
            return Err(WriteStop::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(WriteStop::TimedOut);
        }
        stdin.write_all(b"\n")?;
        Ok(())
    })();
    #[cfg(windows)]
    watchdog.finish();
    result
}

/// 写入看门狗（仅 Windows）：请求写阶段超过 deadline 后，反复对 stdin 句柄
/// `CancelIoEx`，直到写循环结束（finish）——反复触发是为了覆盖「看门狗恰好
/// 在两次分块写入之间触发、取消不到任何挂起操作」的竞态，保证 deadline 之后
/// 任意时刻仍阻塞在内核的写入都会被取消。正常完成时写循环先 finish（置位并
/// join），看门狗在 deadline 前退出，不产生任何取消。
#[cfg(windows)]
struct WriteWatchdog {
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// 裸句柄的 Send 包装（仅 Windows，供写入看门狗线程）：句柄只是指针宽度的
/// 值，跨线程传递本身安全；有效性由 [`WriteWatchdog::finish`] 的 join 保证
/// ——写循环结束（join 返回）前 stdin 句柄不会被释放。
#[cfg(windows)]
struct SendHandle(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: 句柄值仅在包装后跨线程传递，不在原线程并发使用；看门狗线程在
// finish 的 join 前返回，join 之后写循环（及 stdin）才可能被释放。
#[cfg(windows)]
unsafe impl Send for SendHandle {}

#[cfg(windows)]
impl SendHandle {
    /// 取出裸句柄（消耗包装体）。经方法调用取值是为了绕开 Rust 2021 精确
    /// 捕获（RFC 2229）：闭包体内直接解构字段会按字段路径捕获裸指针本身、
    /// 绕过 Send 包装；方法调用捕获整个包装体，Send 约束才生效。
    fn get(self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0
    }
}

#[cfg(windows)]
impl WriteWatchdog {
    fn spawn(stdin: &ChildStdin, deadline: Instant) -> Self {
        use std::os::windows::io::AsRawHandle;
        let handle = SendHandle(stdin.as_raw_handle());
        let done = Arc::new(AtomicBool::new(false));
        let thread_done = Arc::clone(&done);
        let thread = std::thread::Builder::new()
            .name("snap-ocr-write-watchdog".into())
            .spawn(move || {
                // 经 get() 取值保证捕获 Send 包装体（精确捕获语义见其注释）。
                let raw = handle.get();
                while Instant::now() < deadline {
                    if thread_done.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                while !thread_done.load(Ordering::Acquire) {
                    // SAFETY: stdin 句柄在 finish 的 join 之前保持有效；取消的
                    // 是本客户端自己发起的阻塞写入，不影响其他句柄。
                    unsafe {
                        windows_sys::Win32::System::IO::CancelIoEx(raw, std::ptr::null());
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            })
            .ok();
        Self { done, thread }
    }

    /// 写循环结束：置位并回收看门狗线程（deadline 前完成时看门狗不触发取消）。
    fn finish(self) {
        // 实际工作由 Drop 完成：写闭包 panic 展开时同样置位并 join，
        // 看门狗线程不泄漏、也不在句柄失效后继续 CancelIoEx。
        drop(self);
    }
}

#[cfg(windows)]
impl Drop for WriteWatchdog {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 推理子进程的 kill-on-close Job 对象（仅 Windows）：本客户端独占 Job 句柄，
/// 句柄关闭（Drop，或持有进程任何形式的死亡——`std::process::exit(0)` 强退、
/// 被外部杀死、崩溃——时内核回收）即终结组内全部进程（含子进程的后代）。
/// 这是 O-16「释放模型」的进程级兜底：识别卡死时用户点「强制退出」走
/// `exit(0)`、跳过全部清理，若无此兜底，卡在不可中断推理中的 `xberg.exe`
/// 将以 `CREATE_NO_WINDOW` 的不可见孤儿常驻（E2'-1）。与根仓 `src/markdown.rs`
/// 的 `MediaProcessJob` 同一 Win32 模式；worker 是独立包无法共享实现，本地
/// 复刻（Job 语义调整时须两侧同步）。
#[cfg(windows)]
struct InferenceJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl InferenceJob {
    /// 为子进程组建 kill-on-close 进程组（E2'-1/O-16）。
    ///
    /// # Errors
    /// Job 创建、配置或挂接失败（此时子进程已被调用方回收）。
    fn attach(child: &Child) -> Result<Self, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        // SAFETY: 未命名 Job Object，不传入外部指针。
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(format!(
                "无法创建推理进程组：{}",
                std::io::Error::last_os_error()
            ));
        }
        let job = Self { handle };
        // SAFETY: 纯 C 结构；置零后只设置 KILL_ON_JOB_CLOSE 标志。
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: Job 句柄和同步调用期间的结构体指针均有效。
        let configured = unsafe {
            SetInformationJobObject(
                job.handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .map_err(|_| "推理进程组结构大小超出范围".to_string())?,
            )
        };
        if configured == 0 {
            return Err(format!(
                "无法配置推理进程组：{}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: Child 保持有效且拥有该进程句柄，Job 句柄同样有效。
        let assigned = unsafe { AssignProcessToJobObject(job.handle, child.as_raw_handle()) };
        if assigned == 0 {
            return Err(format!(
                "无法把推理子进程加入进程组：{}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(job)
    }

    /// 显式终结组内全部进程（`abort` 的整组回收路径；句柄关闭的内核兜底
    /// 之外的可选主动终止）。
    fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: Job 句柄在本对象销毁前有效；终结组内推理子进程及其后代。
        let _ = unsafe { TerminateJobObject(self.handle, 1) };
    }
}

#[cfg(windows)]
impl Drop for InferenceJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: 本对象独占 Job 句柄；KILL_ON_JOB_CLOSE 会回收残留子进程。
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// 绝对路径序列化：启动配置里的模型根必须是本地绝对路径（O-10）。
fn absolute(path: &Path) -> PathBuf {
    std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
}

/// 组装推理子进程的环境变量（XB-04）：`ORT_DYLIB_PATH` 指向组件目录内的
/// onnxruntime.dll（路径型），其余 7 个是与根仓 `src/markdown.rs` 的
/// `media_worker_environment` 同口径的纯开关型离线变量——两侧同为
/// `xberg.exe worker` 子命令，而 worker 内部可能存在 HF hub 回退，离线防线
/// 必须一致，否则截图 OCR 的推理子进程就处于离线口径之外（B'-1）。
/// 不设 `HF_HOME`/`HF_HUB_CACHE` 等路径型缓存变量：与 markdown 侧约定一致，
/// 不为组件目录引入额外路径假设。该口径现有三处实现（`src/markdown.rs`
/// `media_worker_environment`、`src/markdown_document.rs`
/// `apply_offline_environment`、本函数），跨包无法共享；调整口径时须三处同步。
fn inference_environment(component_dir: &Path) -> Vec<(String, String)> {
    [
        (
            "ORT_DYLIB_PATH",
            component_dir
                .join("onnxruntime.dll")
                .to_string_lossy()
                .into_owned(),
        ),
        ("HF_HUB_OFFLINE", "1".to_string()),
        ("HUGGINGFACE_HUB_OFFLINE", "1".to_string()),
        ("TRANSFORMERS_OFFLINE", "1".to_string()),
        ("HF_DATASETS_OFFLINE", "1".to_string()),
        ("NO_COLOR", "1".to_string()),
        ("XBERG_ORT_EP", "cpu".to_string()),
        ("XBERG_MAX_CONCURRENT_REQUESTS", "1".to_string()),
        (
            "XBERG_PERF_LOG_DIR",
            std::env::temp_dir()
                .join("JchTools-xberg-perf")
                .to_string_lossy()
                .into_owned(),
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value))
    .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::inference_environment;

    // B'-1 回归（XB-04）：环境组装恰好含 8 个变量——7 个与 markdown 侧
    // `media_worker_environment` 同口径的纯开关 + 路径型 `ORT_DYLIB_PATH`。
    #[test]
    fn inference_environment_carries_offline_switches_and_dylib_path() {
        let root = Path::new("C:\\fake\\xberg-component");
        let vars = inference_environment(root);
        let map: HashMap<String, String> = vars.iter().cloned().collect();
        for (name, want) in [
            ("HF_HUB_OFFLINE", "1"),
            ("HUGGINGFACE_HUB_OFFLINE", "1"),
            ("TRANSFORMERS_OFFLINE", "1"),
            ("HF_DATASETS_OFFLINE", "1"),
            ("NO_COLOR", "1"),
            ("XBERG_ORT_EP", "cpu"),
            ("XBERG_MAX_CONCURRENT_REQUESTS", "1"),
        ] {
            // XBERG_PERF_LOG_DIR 单独断言（路径值含专用临时目录名）。
            assert_eq!(
                map.get(name).map(String::as_str),
                Some(want),
                "{name} 应为纯开关值 {want}（与 markdown 侧离线口径一致）"
            );
        }
        assert!(
            map.get("XBERG_PERF_LOG_DIR")
                .is_some_and(|v| v.contains("JchTools-xberg-perf")),
            "XBERG_PERF_LOG_DIR 应指向专用临时目录（与 markdown 侧三处统一）"
        );
        assert_eq!(
            map.get("ORT_DYLIB_PATH").map(String::as_str),
            Some(root.join("onnxruntime.dll").to_string_lossy().as_ref()),
            "ORT_DYLIB_PATH 应指向组件目录内的 onnxruntime.dll"
        );
        assert_eq!(map.len(), 9, "不应混入其他变量：{vars:?}");
    }
}
