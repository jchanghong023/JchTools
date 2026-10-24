use super::client::log_response;
use super::deadline::IoDeadline;
use super::message::{read_engine_response, write_json, EngineResponse};
use super::{startup_config, Pending, PendingRequest, ProcessIdToSessionId, QUERY_TIMEOUT};
use jchtools_foundation::{logging, xberg_settings};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

/// 同一个 broker 替换或重启引擎时，也必须为每次 spawn 排他预留性能目录。
pub(super) fn create_engine_perf_directory(state_dir: &Path) -> std::io::Result<PathBuf> {
    let parent = state_dir.join("perf-logs").join("xberg");
    std::fs::create_dir_all(&parent)?;
    let directory = parent.join(logging::new_operation_id());
    std::fs::create_dir(&directory)?;
    Ok(directory)
}

pub(super) struct Engine {
    pub(super) child: Child,
    pub(super) root: PathBuf,
    outgoing: mpsc::SyncSender<(Value, tracing::Span)>,
    pub(super) pending: Pending,
    pub(super) broken: Arc<AtomicBool>,
    _job: Job,
    lifecycle_span: tracing::Span,
    started: Instant,
}
impl Engine {
    pub(super) fn spawn(root: &Path) -> Result<Self, String> {
        let span = logging::operation_span("xberg_runtime", "engine_spawn");
        let _entered = span.enter();
        let started = Instant::now();
        let mut stage = "existing_engine_guard";
        tracing::info!(
            event = "xberg_engine_spawn_started",
            executable = "xberg.exe",
            mode = "worker",
            "开始创建唯一共享引擎"
        );
        let result = (|| {
            reject_existing_engine()?;
            stage = "startup_config";
            let mut command = Command::new(root.join("xberg.exe"));
            command
                .args(["worker", "--no-config-discovery", "--config-json"])
                .arg(startup_config(root)?.to_string())
                .current_dir(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(0x0800_0000);
            for name in [
                "HF_HUB_OFFLINE",
                "HUGGINGFACE_HUB_OFFLINE",
                "TRANSFORMERS_OFFLINE",
                "HF_DATASETS_OFFLINE",
                "NO_COLOR",
            ] {
                command.env(name, "1");
            }
            command
                // 只使用当前保存目录随引擎发布的截图模型，不继承开发机覆盖路径。
                .env_remove("XBERG_SNAPSHOT_MODEL_DIR")
                // 禁止继承上游裁剪图及 OCR 正文自动转储开关。
                .env_remove("XBERG_SNAPSHOT_DEBUG_CROPS")
                .env("ORT_DYLIB_PATH", root.join("onnxruntime.dll"))
                .env("HF_HOME", root)
                .env("HF_HUB_CACHE", root.join("models"))
                .env("XBERG_MAX_REQUEST_BODY_BYTES", "104857600")
                .env("XBERG_API_ALLOW_LOCAL_URI_INPUTS", "1")
                .env("XBERG_ORT_EP", "cpu")
                .env("XBERG_SENSEVOICE_MODEL_DIR", root.join("models"))
                .env("XBERG_SHERPA_DLL_DIR", root.join("sherpa-onnx"))
                .env("XBERG_FFMPEG_DLL_DIR", root.join("ffmpeg"))
                .env(
                    "XBERG_CACHE_DIR",
                    xberg_settings::state_dir()?.join("xberg-cache"),
                );
            let perf_directory = match create_engine_perf_directory(&xberg_settings::state_dir()?) {
                Ok(directory) => directory,
                Err(error) => {
                    tracing::warn!(
                        event = "xberg_performance_log_disabled",
                        stage = "perf_log_directory",
                        error_code = error.raw_os_error(),
                        "无法独占创建引擎性能目录，本次不采集性能数据，业务继续"
                    );
                    // 外部引擎没有运行期关闭开关，目录不可创建才会安静禁用 perf。
                    // 使用必为普通文件的 exe 下子路径触发该既有语义；显式覆盖继承值，
                    // 不能 env_remove 后回退引擎的共享 logs/，也不能复用旧性能目录。
                    root.join("xberg.exe").join("perf-disabled")
                }
            };
            command.env("XBERG_PERF_LOG_DIR", perf_directory);
            stage = "spawn";
            let mut child = command
                .spawn()
                .map_err(|e| format!("共享 Xberg 启动失败：{e}"))?;
            tracing::info!(
                event = "xberg_engine_process_started",
                engine_pid = child.id(),
                executable = "xberg.exe",
                "共享 Xberg 引擎进程已启动"
            );
            stage = "job_attach";
            let job = match Job::attach(&child) {
                Ok(job) => job,
                Err(error) => {
                    if let Err(error) = child.kill() {
                        tracing::warn!(
                            event = "xberg_engine_kill_failed",
                            stage = "job_attach_cleanup",
                            engine_pid = child.id(),
                            error_code = error.raw_os_error(),
                            "作业绑定失败后的引擎终结失败"
                        );
                    }
                    match child.wait() {
                        Ok(status) => {
                            tracing::info!(event = "xberg_engine_exited", stage = "job_attach_cleanup", engine_pid = child.id(), exit_code = status.code(), exit_status = %status, elapsed_ms = logging::elapsed_ms(started), "作业绑定失败后的引擎已退出");
                        }
                        Err(error) => tracing::warn!(
                            event = "xberg_engine_wait_failed",
                            stage = "job_attach_cleanup",
                            engine_pid = child.id(),
                            error_code = error.raw_os_error(),
                            "作业绑定失败后的引擎退出等待失败"
                        ),
                    }
                    return Err(error);
                }
            };
            stage = "stdio_setup";
            let stdin = child.stdin.take().ok_or("Xberg stdin 缺失")?;
            let stdout = child.stdout.take().ok_or("Xberg stdout 缺失")?;
            let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
            let broken = Arc::new(AtomicBool::new(false));
            let (outgoing, incoming) = mpsc::sync_channel::<(Value, tracing::Span)>(32);
            let engine_pid = child.id();
            spawn_engine_writer_thread(
                stdin,
                incoming,
                Arc::clone(&pending),
                Arc::clone(&broken),
                engine_pid,
            );
            spawn_engine_reader_thread(
                stdout,
                Arc::clone(&pending),
                Arc::clone(&broken),
                engine_pid,
            );
            Ok(Self {
                child,
                root: root.to_path_buf(),
                outgoing,
                pending,
                broken,
                _job: job,
                lifecycle_span: tracing::Span::current(),
                started,
            })
        })();
        if let Ok(engine) = &result {
            tracing::info!(
                event = "xberg_engine_spawn_completed",
                engine_pid = engine.child.id(),
                elapsed_ms = logging::elapsed_ms(started),
                "唯一共享引擎创建完成"
            );
        } else {
            tracing::warn!(
                event = "xberg_engine_spawn_failed",
                stage,
                elapsed_ms = logging::elapsed_ms(started),
                "唯一共享引擎创建失败"
            );
        }
        result
    }
    pub(super) fn submit(&self, request: Value) -> Result<mpsc::Receiver<Value>, String> {
        let started = Instant::now();
        tracing::debug!(
            event = "xberg_engine_submit_started",
            engine_pid = self.child.id(),
            command = super::diagnostic_command(request["command"].as_str()),
            "开始登记并排队共享引擎请求"
        );
        let id = request["id"]
            .as_str()
            .ok_or_else(|| {
                tracing::warn!(
                    event = "xberg_engine_submit_failed",
                    stage = "request_id",
                    "共享请求 ID 无效"
                );
                "请求 ID 无效"
            })?
            .to_string();
        let (send, receive) = mpsc::channel();
        let mut pending = self.pending.lock().map_err(|_| {
            tracing::warn!(
                event = "xberg_engine_submit_failed",
                stage = "pending_lock",
                "共享请求登记表锁异常"
            );
            "共享请求表异常"
        })?;
        if self.broken.load(Ordering::Acquire) {
            tracing::warn!(
                event = "xberg_engine_submit_failed",
                stage = "broken",
                "共享请求被拒绝：引擎通信已中断"
            );
            return Err("共享 Xberg 通信已中断；不会为仍存活的引擎启动副本".into());
        }
        if pending.len() >= 64 || pending.contains_key(&id) {
            tracing::warn!(
                event = "xberg_engine_submit_failed",
                stage = "queue_limit_or_duplicate",
                pending = pending.len(),
                "共享请求被拒绝：请求队列已满或 ID 重复"
            );
            return Err("共享 Xberg 请求队列已满或 ID 重复".into());
        }
        let lane = match request["command"].as_str() {
            Some("extract" | "transcribe") => Some("document"),
            Some("ocr_snapshot") => Some("snapshot"),
            _ => None,
        };
        if lane.is_some() && pending.values().any(|entry| entry.lane == lane) {
            tracing::warn!(
                event = "xberg_engine_submit_failed",
                stage = "lane_busy",
                lane = lane.unwrap_or("?"),
                "共享请求被拒绝：同场景上一请求未结束"
            );
            return Err("该场景的上一个请求尚未确认结束，请等待其终态；其他场景可继续使用".into());
        }
        pending.insert(
            id.clone(),
            PendingRequest {
                sender: send,
                lane,
                span: tracing::Span::current(),
                started,
                command: super::diagnostic_command(request["command"].as_str()),
                cancel_requested: false,
            },
        );
        let cancel_target = (request["command"] == "cancel")
            .then(|| request["target_id"].as_str().map(str::to_owned))
            .flatten();
        if self
            .outgoing
            .try_send((request, tracing::Span::current()))
            .is_err()
        {
            pending.remove(&id);
            tracing::warn!(
                event = "xberg_engine_submit_failed",
                stage = "outgoing_queue",
                "共享请求排队失败，已撤销登记"
            );
            return Err("共享 Xberg 请求队列不可用".into());
        }
        if let Some(target_id) = cancel_target {
            if let Some(target) = pending.get_mut(&target_id) {
                // 只有实际进入唯一 FIFO 的取消才赋予截图显式强退资格；ACK 不是终态。
                target.cancel_requested = true;
            }
        }
        tracing::debug!(
            event = "xberg_engine_submit_completed",
            engine_pid = self.child.id(),
            pending = pending.len(),
            lane,
            elapsed_ms = logging::elapsed_ms(started),
            "共享引擎请求已登记并排队"
        );
        Ok(receive)
    }
}

/// 引擎 stdin 写线程：逐条写入 FIFO 请求并记录提交日志；写入失败即宣告
/// 通信断裂（broken）并交付全部在途失败终态，随后退出线程。
fn spawn_engine_writer_thread(
    mut stdin: ChildStdin,
    incoming: mpsc::Receiver<(Value, tracing::Span)>,
    pending: Pending,
    broken: Arc<AtomicBool>,
    engine_pid: u32,
) {
    let writer_span = tracing::Span::current();
    std::thread::spawn(move || {
        let _entered = writer_span.enter();
        for (request, request_span) in incoming {
            let _request_entered = request_span.enter();
            let started = Instant::now();
            tracing::debug!(
                event = "xberg_engine_write_started",
                engine_pid,
                command = super::diagnostic_command(request["command"].as_str()),
                "开始向共享引擎提交请求"
            );
            let result =
                IoDeadline::new(QUERY_TIMEOUT).and_then(|_guard| write_json(&mut stdin, &request));
            if result.is_err() {
                tracing::warn!(
                    event = "xberg_engine_write_failed",
                    stage = "stdin_write",
                    engine_pid,
                    elapsed_ms = logging::elapsed_ms(started),
                    "共享引擎 stdin 写入失败，通信断裂"
                );
                fail_pending(&pending, &broken);
                break;
            }
            tracing::debug!(
                event = "xberg_engine_write_completed",
                engine_pid,
                elapsed_ms = logging::elapsed_ms(started),
                "共享引擎请求已写入 stdin"
            );
        }
    });
}

/// 引擎 stdout 读线程：解析单行响应并按 id 交付等待端；超限、无效 JSON、
/// 缺失 id 与未知 id 均按通信断裂处理，退出循环前交付全部在途失败终态
/// （XB-05/XB-07）。
fn spawn_engine_reader_thread(
    stdout: ChildStdout,
    pending: Pending,
    broken: Arc<AtomicBool>,
    engine_pid: u32,
) {
    let reader_span = tracing::Span::current();
    std::thread::spawn(move || {
        let _entered = reader_span.enter();
        let mut reader = BufReader::new(stdout);
        loop {
            let mut response = match read_engine_response(&mut reader) {
                Ok(EngineResponse::Json(response)) => response,
                Ok(EngineResponse::Oversized { id, bytes }) => {
                    if deliver_oversized_failure(&pending, engine_pid, id, bytes) {
                        continue;
                    }
                    break;
                }
                Err(error) => {
                    let stage = if error == "共享 Xberg 返回了无效 JSON" {
                        "stdout_decode"
                    } else {
                        "stdout_read"
                    };
                    tracing::warn!(
                        event = "xberg_engine_response_failed",
                        engine_pid,
                        stage,
                        "共享引擎响应读取失败，通信断裂"
                    );
                    break;
                }
            };
            response = prune_forwarded_document(response);
            let Some(id) = response["id"].as_str() else {
                tracing::warn!(
                    event = "xberg_engine_response_failed",
                    engine_pid,
                    stage = "response_id_missing",
                    "共享引擎响应缺少 id，通信断裂"
                );
                break;
            };
            let sender = pending
                .lock()
                .ok()
                .and_then(|mut entries| entries.remove(id));
            let Some(sender) = sender else {
                // 未知 id（含重复响应）是引擎侧协议异常：静默丢弃会让等待者
                // 只能靠超时报错，而在途项不随超时删除，场景 lane 被永久占用
                // 直到引擎死亡。与超限响应无法定位请求的路径同口径：warn 后
                // 按通信断裂交付全部在途失败并释放 lane（XB-05/XB-07）。
                tracing::warn!(
                    event = "xberg_engine_response_failed",
                    engine_pid,
                    stage = "response_id_unknown",
                    response_id_bytes = id.len(),
                    "共享引擎返回未知请求 ID 的响应，通信断裂"
                );
                break;
            };
            let _request_entered = sender.span.enter();
            log_response(
                "engine",
                sender.command,
                &response,
                sender.started,
                Some(engine_pid),
            );
            if sender.sender.send(response).is_err() {
                tracing::warn!(
                    event = "xberg_engine_response_delivery_failed",
                    stage = "broker_receiver_closed",
                    engine_pid,
                    "共享引擎请求已有终态，但代理等待端已离开"
                );
            }
        }
        fail_pending(&pending, &broken);
    });
}

/// 超限响应的失败终态交付：能按 id 定位在途请求时按 response_too_large
/// 交付并返回 true（继续读下一行）；无法定位时返回 false，由调用方按
/// 通信断裂退出读循环。
fn deliver_oversized_failure(
    pending: &Pending,
    engine_pid: u32,
    id: Option<String>,
    bytes: u64,
) -> bool {
    if let Some(id) = id {
        let sender = pending
            .lock()
            .ok()
            .and_then(|mut entries| entries.remove(&id));
        if let Some(sender) = sender {
            let _request_entered = sender.span.enter();
            tracing::warn!(
                event = "xberg_engine_response_failed",
                engine_pid,
                stage = "message_limit",
                bytes,
                elapsed_ms = logging::elapsed_ms(sender.started),
                "共享引擎响应超限，该请求按失败终态交付"
            );
            if sender
                .sender
                .send(json!({
                    "id": id,
                    "ok": false,
                    "error_kind": "response_too_large",
                    "error": format!("共享 Xberg 响应超过消息上限（{bytes} 字节）")
                }))
                .is_err()
            {
                tracing::warn!(
                    event = "xberg_engine_response_delivery_failed",
                    stage = "broker_receiver_closed",
                    engine_pid,
                    "超限失败终态已生成，但代理等待端已离开"
                );
            }
            return true;
        }
    }
    tracing::warn!(
        event = "xberg_engine_response_failed",
        engine_pid,
        stage = "message_limit_unmapped",
        bytes,
        "共享引擎返回超限响应且无法定位请求"
    );
    false
}

/// 升级期间旧截图服务可能仍持有独占引擎。不能接管未知 stdio，也不能另开副本。
fn reject_existing_engine() -> Result<(), String> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: 只读获取进程目录快照，不访问进程内容。
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err("无法检查已有 Xberg 进程，未启动新引擎".into());
    }
    // SAFETY: 快照句柄所有权转移给 File，析构仅调用 CloseHandle。
    let snapshot = unsafe { File::from_raw_handle(snapshot.cast()) };
    // SAFETY: PROCESSENTRY32W 是 C POD 结构，dwSize 按 API 要求填写。
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = u32::try_from(std::mem::size_of_val(&entry)).map_err(|e| e.to_string())?;
    let mut session = 0;
    // SAFETY: 有效进程 ID 与可写的 session 指针。
    let pid = unsafe { GetCurrentProcessId() };
    // SAFETY: session 是有效输出指针，pid 是当前进程。
    if unsafe { ProcessIdToSessionId(pid, &raw mut session) } == 0 {
        return Err("无法确认当前登录会话".into());
    }
    // SAFETY: 有效快照与已初始化长度的输出结构。
    let mut found = unsafe { Process32FirstW(snapshot.as_raw_handle().cast(), &raw mut entry) };
    while found != 0 {
        let length = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..length]).eq_ignore_ascii_case("xberg.exe") {
            let mut other_session = u32::MAX;
            // SAFETY: 快照提供的进程 ID，输出指针有效；已退出进程的查询允许失败。
            if unsafe { ProcessIdToSessionId(entry.th32ProcessID, &raw mut other_session) } != 0
                && other_session == session
            {
                return Err(format!("当前会话已有 Xberg（PID {}），无法接管其通信；请先结束旧实例。未启动第二个引擎", entry.th32ProcessID));
            }
        }
        // SAFETY: 同一有效快照及输出结构，逐项只读枚举。
        found = unsafe { Process32NextW(snapshot.as_raw_handle().cast(), &raw mut entry) };
    }
    Ok(())
}

pub(super) fn fail_pending(pending: &Pending, broken: &AtomicBool) {
    broken.store(true, Ordering::Release);
    tracing::warn!(
        event = "xberg_engine_communication_broken",
        "共享引擎通信断裂：在途请求全部按失败交付"
    );
    if let Ok(mut entries) = pending.lock() {
        for (id, send) in entries.drain() {
            let _entered = send.span.enter();
            tracing::warn!(
                event = "xberg_engine_pending_failed",
                stage = "communication_broken",
                error_kind = "process_exited",
                elapsed_ms = logging::elapsed_ms(send.started),
                "共享引擎在途请求按失败终态交付"
            );
            if send.sender.send(json!({"id":id,"ok":false,"error_kind":"process_exited","error":"共享 Xberg 通信中断或进程退出"})).is_err() {
                tracing::warn!(event = "xberg_engine_response_delivery_failed", stage = "broker_receiver_closed", "通信断裂失败终态已生成，但代理等待端已离开");
            }
        }
    } else {
        tracing::warn!(
            event = "xberg_engine_pending_delivery_failed",
            stage = "pending_lock",
            error_kind = "mutex_poisoned",
            "共享引擎通信已断裂，但请求登记表锁异常，不能交付在途终态"
        );
    }
}

/// 转发前剔除客户端不消费的重型字段，收窄响应体积的主要来源。
///
/// Markdown 产物消费 `content`（引擎最终 Markdown，markdown_document 零改写
/// 采用）、`warnings`（顶层副本，恒在）以及 T-14（2026-10-04）起产物要保留的
/// `images`（`data_base64`/`data` 是图片落盘的数据通道，转发侧必须放行）；
/// `pages`/`tables` 仍剔除，`children`/`ocr_elements` 虽不再被文档适配器消费，
/// 仍按原样转发。剔除只发生在代理转发侧，引擎落盘缓存与 CLI 行为不受影响；
/// 嵌入子文档逐层递归处理，路径、正文与告警保持逐字节不变。
pub(super) fn prune_forwarded_document(mut response: Value) -> Value {
    if let Some(document) = response.get_mut("document") {
        prune_document_heavy_fields(document, 0);
    }
    response
}

fn prune_document_heavy_fields(value: &mut Value, depth: usize) {
    // 嵌套深度与引擎提取期的归档/嵌入深度上限同量级，防御性兜底即可。
    const MAX_PRUNE_DEPTH: usize = 8;
    if depth > MAX_PRUNE_DEPTH {
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.remove("pages");
    object.remove("tables");
    if let Some(children) = object.get_mut("children").and_then(Value::as_array_mut) {
        for child in children {
            if let Some(result) = child.get_mut("result") {
                prune_document_heavy_fields(result, depth + 1);
            }
        }
    }
}

// Job 只由代理拥有，GUI/截图服务退出不会关闭此句柄。
struct Job(windows_sys::Win32::Foundation::HANDLE);
// SAFETY: 句柄只在 Engine 的互斥锁内转移/访问；Drop 只执行一次 CloseHandle。
unsafe impl Send for Job {}
impl Job {
    fn attach(child: &Child) -> Result<Self, String> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        // SAFETY: 创建无继承的未命名 job，不传外部指针。
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            let error_code = std::io::Error::last_os_error().raw_os_error();
            tracing::warn!(
                event = "xberg_engine_job_failed",
                stage = "job_create",
                engine_pid = child.id(),
                error_code,
                "无法创建共享引擎进程组"
            );
            return Err("无法创建共享引擎进程组".into());
        }
        let job = Self(handle);
        // SAFETY: C POD 结构允许全零初始化。
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: 有效 job 及同步读取的配置结构。
        if unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(std::mem::size_of_val(&limits)).map_err(|e| e.to_string())?,
            )
        } == 0
        {
            let error_code = std::io::Error::last_os_error().raw_os_error();
            tracing::warn!(
                event = "xberg_engine_job_failed",
                stage = "job_configure",
                engine_pid = child.id(),
                error_code,
                "无法配置共享引擎进程组"
            );
            return Err("无法配置共享引擎进程组".into());
        }
        // SAFETY: child 为当前代理创建的有效进程，job 由本代理持有。
        if unsafe { AssignProcessToJobObject(handle, child.as_raw_handle().cast()) } == 0 {
            let error_code = std::io::Error::last_os_error().raw_os_error();
            tracing::warn!(
                event = "xberg_engine_job_failed",
                stage = "job_assign",
                engine_pid = child.id(),
                error_code,
                "无法将共享引擎加入进程组"
            );
            return Err("无法将共享引擎加入进程组".into());
        }
        Ok(job)
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        /* SAFETY: 独占有效 job 句柄。 */
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub(super) fn kill_engine(engine: &mut Engine, reason: &'static str) -> std::io::Result<()> {
    let _entered = engine.lifecycle_span.enter();
    tracing::info!(
        event = "xberg_engine_terminate_started",
        engine_pid = engine.child.id(),
        reason,
        "开始终结共享引擎"
    );
    let result = engine.child.kill();
    if let Err(error) = &result {
        tracing::warn!(
            event = "xberg_engine_kill_failed",
            engine_pid = engine.child.id(),
            reason,
            error_code = error.raw_os_error(),
            "共享引擎终结调用失败"
        );
    }
    result
}

pub(super) fn wait_engine(
    engine: &mut Engine,
    reason: &'static str,
) -> std::io::Result<std::process::ExitStatus> {
    let result = engine.child.wait();
    match &result {
        Ok(status) => log_engine_exit(engine, *status, reason),
        Err(error) => {
            let _entered = engine.lifecycle_span.enter();
            tracing::warn!(
                event = "xberg_engine_wait_failed",
                engine_pid = engine.child.id(),
                reason,
                error_code = error.raw_os_error(),
                elapsed_ms = logging::elapsed_ms(engine.started),
                "共享引擎退出等待失败"
            );
        }
    }
    result
}

pub(super) fn log_engine_exit(
    engine: &Engine,
    status: std::process::ExitStatus,
    reason: &'static str,
) {
    let _entered = engine.lifecycle_span.enter();
    if reason == "observed_exit" && !status.success() {
        tracing::warn!(event = "xberg_engine_exited", engine_pid = engine.child.id(), reason, exit_code = status.code(), exit_status = %status, elapsed_ms = logging::elapsed_ms(engine.started), "共享引擎进程异常退出");
    } else {
        tracing::info!(event = "xberg_engine_exited", engine_pid = engine.child.id(), reason, exit_code = status.code(), exit_status = %status, success = status.success(), elapsed_ms = logging::elapsed_ms(engine.started), "共享引擎进程已退出");
    }
}

/// 调用方持有唯一引擎槽锁；成功回收之前绝不清空槽或启动替代进程。
fn stop_owned_engine(slot: &mut Option<Engine>, reason: &'static str) -> Result<(), String> {
    if let Some(current) = slot.as_mut() {
        if let Some(status) = current
            .child
            .try_wait()
            .map_err(|error| format!("查询旧引擎退出失败：{error}"))?
        {
            log_engine_exit(current, status, reason);
        } else {
            kill_engine(current, reason).map_err(|error| format!("停止旧共享引擎失败：{error}"))?;
            wait_engine(current, reason)
                .map_err(|error| format!("等待旧共享引擎退出失败：{error}"))?;
        }
        fail_pending(&current.pending, &current.broken);
    }
    slot.take();
    Ok(())
}

/// 在 submit/取消/生命周期共用的槽锁内完成迁移，关闭检查与提交之间的竞态窗口。
pub(super) fn activate_runtime(
    slot: &mut Option<Engine>,
    root: &Path,
    preserve_media: bool,
) -> Result<Value, String> {
    if let Some(current) = slot.as_ref() {
        if current
            .pending
            .lock()
            .map_err(|_| "共享请求表异常")?
            .values()
            .any(|request| request.lane.is_some())
        {
            return Err(
                "文档、媒体或截图任务尚未结束，暂不迁移共享目录；原配置和任务保持不变".into(),
            );
        }
    }
    for scenario in ["engine", "document", "snapshot"] {
        super::validate_assets(root, scenario)?;
    }
    let previous = slot.as_ref().map(|engine| engine.root.clone());
    let preserve_media = preserve_media
        || previous
            .as_ref()
            .is_some_and(|old| super::validate_assets(old, "media").is_ok());
    // 同根保留常驻模型，但仍校验调用者要求的媒体成员及编译能力后才提交来源。
    let same_root_alive = if let Some(current) = slot.as_mut().filter(|engine| engine.root == root)
    {
        !current.broken.load(Ordering::Acquire)
            && current
                .child
                .try_wait()
                .map_err(|error| format!("查询共享引擎失败：{error}"))?
                .is_none()
    } else {
        false
    };
    if !same_root_alive {
        stop_owned_engine(slot, "bundled_activation")?;
    }
    let result: Result<Value, String> = (|| {
        if slot.is_none() {
            *slot = Some(Engine::spawn(root)?);
        }
        let current = slot.as_ref().ok_or("随包共享引擎初始化失败")?;
        let capabilities = current
            .submit(json!({
                "id": logging::new_operation_id(),
                "command":"capabilities",
                "timeout_ms": u64::try_from(QUERY_TIMEOUT.as_millis()).unwrap_or(u64::MAX)
            }))?
            .recv_timeout(QUERY_TIMEOUT)
            .map_err(|error| format!("随包 Xberg 能力握手失败：{error}"))?;
        let capabilities = super::checked(capabilities)?;
        for command in ["extract", "ocr_snapshot"] {
            super::validate_capabilities(&capabilities, command)?;
        }
        if preserve_media {
            super::validate_assets(root, "media")?;
            super::validate_capabilities(&capabilities, "transcribe")?;
        }
        xberg_settings::save_bundled_directory(root)?;
        Ok(json!({"ok":true,"jchtools_xberg_pid":current.child.id()}))
    })();
    if let Err(error) = result {
        // 新来源尚未提交；失败目标必须先退出，才允许恢复旧根的惰性能力。
        stop_owned_engine(slot, "bundled_activation_rollback")
            .map_err(|rollback| format!("{error}；回收迁移目标失败：{rollback}；原配置未更改"))?;
        if let Some(previous) = previous {
            *slot =
                Some(Engine::spawn(&previous).map_err(|restore| {
                    format!("{error}；原配置保留，但旧引擎恢复失败：{restore}")
                })?);
        }
        return Err(format!("共享目录迁移失败，原配置保留：{error}"));
    }
    result
}
