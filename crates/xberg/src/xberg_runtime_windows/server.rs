use super::background::{background_allowed, identity, pipe_name};
use super::client::{log_response, submission_acknowledgement};
use super::deadline::IoDeadline;
use super::engine::{
    activate_runtime, fail_pending, kill_engine, log_engine_exit, wait_engine, Engine,
};
use super::message::{read_json, write_json};
use super::{diagnostic_id, BROKER_PROTOCOL, QUERY_TIMEOUT};
use fs2::FileExt;
use jchtools_foundation::{logging, xberg_settings};
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::BufReader;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
};

/// 引擎终态从 pending 移除后，客户端管道仍可能等待取走响应。
/// 停止看门狗必须等已解析登记的请求处理结束，不能仅凭引擎场景已空就退出代理。
struct ActiveHandler(Arc<AtomicUsize>);
impl ActiveHandler {
    fn new(active: &Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(active))
    }
}
impl Drop for ActiveHandler {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) fn create_pipe(name: &str) -> std::io::Result<File> {
    use windows_sys::Win32::Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
    };
    use windows_sys::Win32::System::Pipes::CreateNamedPipeW;
    let wide = |value: &str| value.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let name = wide(name);
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;OW)");
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: 有效宽字符串和可写输出指针；descriptor 随后由 LocalFree 释放。
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())
            .map_err(std::io::Error::other)?,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: 属性与名字在调用期间存活；拒绝远程客户端，允许多个本地请求并发连接。
    let raw = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            3,
            8,
            64,
            65536,
            65536,
            0,
            &raw const attributes,
        )
    };
    // LocalFree 也会调用 Win32；必须在它之前保存创建失败的原始错误码。
    let create_error = (raw == INVALID_HANDLE_VALUE).then(std::io::Error::last_os_error);
    // SAFETY: descriptor 为上方 API 分配的内存，管道创建已复制所需内容。
    unsafe {
        LocalFree(descriptor);
    }
    if let Some(os_error) = create_error {
        return Err(os_error);
    }
    // SAFETY: 将新建管道句柄唯一所有权交给 File。
    Ok(unsafe { File::from_raw_handle(raw.cast()) })
}

/// 命名管道响应须等客户端读走后才关闭；控制确认也不能绕过这一收尾。
fn write_pipe_response(
    pipe: &mut File,
    response: &Value,
    caller_pid: Option<u64>,
    started: Instant,
) -> Result<(), String> {
    let _guard = IoDeadline::new(QUERY_TIMEOUT)?;
    write_json(pipe, response)?;
    tracing::debug!(
        event = "xberg_broker_response_sent",
        caller_pid,
        elapsed_ms = logging::elapsed_ms(started),
        "共享代理响应已发送"
    );
    // SAFETY: 有效管道，等待客户端取走响应后断开；受上方 I/O 期限保护。
    let flushed = unsafe {
        windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(pipe.as_raw_handle().cast())
    };
    if flushed == 0 {
        // SAFETY: 本线程紧邻失败的 FlushFileBuffers 查询错误码。
        let code = unsafe { GetLastError() };
        tracing::warn!(
            event = "xberg_broker_response_flush_failed",
            stage = "response_flush",
            caller_pid,
            code,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理响应已写出，但未确认客户端取走；保持原有返回语义"
        );
    }
    Ok(())
}

pub(super) fn handle(
    pipe: File,
    engine: &Mutex<Option<Engine>>,
    stopping: &AtomicBool,
    active_handlers: &Arc<AtomicUsize>,
) -> Result<(), String> {
    let started = Instant::now();
    let mut reader = BufReader::new(pipe);
    let envelope = {
        let _guard = IoDeadline::new(QUERY_TIMEOUT)?;
        read_json(&mut reader)?
    };
    // 完整 JSON 及读取期限看门狗结束后才登记，未提交请求的连接不能拖住退出。
    // 与停止变更、停止看门狗共用状态锁，确保退出检查不会越过已登记响应的交付。
    let _handler = {
        let _slot = engine.lock().map_err(|_| "后台状态异常")?;
        if stopping.load(Ordering::Acquire) {
            None
        } else {
            Some(ActiveHandler::new(active_handlers))
        }
    };
    let request = envelope["request"].clone();
    let fallback_id;
    let request_id = if let Some(id) =
        diagnostic_id(envelope["diagnostic"]["operation_id"].as_str())
            .or_else(|| diagnostic_id(request["id"].as_str()))
    {
        id
    } else {
        fallback_id = logging::new_operation_id();
        &fallback_id
    };
    let span = logging::operation_span_with_id("xberg_runtime", "broker_handle", request_id);
    let _entered = span.enter();
    let command = super::diagnostic_command(request["command"].as_str());
    let parent_operation_id = diagnostic_id(envelope["diagnostic"]["parent_operation_id"].as_str());
    let caller_pid = envelope["diagnostic"]["caller_pid"].as_u64();
    if command == "keepalive" {
        tracing::debug!(
            event = "xberg_broker_request_started",
            parent_operation_id,
            caller_pid,
            command,
            timeout_ms = request["timeout_ms"].as_u64(),
            "共享代理接收到客户端请求"
        );
    } else {
        tracing::debug!(
            event = "xberg_broker_request_started",
            parent_operation_id,
            caller_pid,
            command,
            timeout_ms = request["timeout_ms"].as_u64(),
            target_request_id = diagnostic_id(request["target_id"].as_str()),
            "共享代理接收到客户端请求"
        );
    }
    let mut stage = "dispatch";
    let result = (|| {
        if matches!(
            request["command"].as_str(),
            Some("broker-state" | "broker-stop" | "broker-force-stop")
        ) {
            // 状态快照与停止变更在锁内完成；交付由 Flush 和 ActiveHandler 保证。
            let mut slot = engine.lock().map_err(|_| "后台状态异常")?;
            if request["command"] == "broker-stop" {
                stopping.store(true, Ordering::Release);
            }
            if request["command"] == "broker-force-stop" {
                let task_active = slot.as_ref().is_some_and(|e| {
                    e.pending.lock().map_or(true, |p| {
                        p.values().any(|v| match v.lane {
                            Some("snapshot") => !v.cancel_requested,
                            Some(_) => true,
                            None => false,
                        })
                    })
                });
                if task_active {
                    tracing::warn!(
                        event = "xberg_broker_control_failed",
                        stage = "active_tasks",
                        command,
                        "共享代理拒绝强制停止：仍有其他场景任务"
                    );
                    let response = json!({"id":request["id"],"ok":false,"error":"文档、媒体或未取消的截图任务尚未结束，不能强制退出","jchtools_broker_protocol":BROKER_PROTOCOL});
                    drop(slot);
                    return write_pipe_response(reader.get_mut(), &response, caller_pid, started);
                }
                stopping.store(true, Ordering::Release);
                if let Some(mut current) = slot.take() {
                    kill_engine(&mut current, "force_stop").map_err(|e| e.to_string())?;
                    wait_engine(&mut current, "force_stop").map_err(|e| e.to_string())?;
                    fail_pending(&current.pending, &current.broken);
                }
            }
            let active = slot
                .as_ref()
                .and_then(|e| {
                    e.pending
                        .lock()
                        .ok()
                        .map(|p| p.values().filter(|v| v.lane.is_some()).count())
                })
                .unwrap_or(0);
            let original_pending = request["target_id"].as_str().map(|target_id| {
                slot.as_ref().is_some_and(|current| {
                    current
                        .pending
                        .lock()
                        .map_or(true, |pending| pending.contains_key(target_id))
                })
            });
            let response = json!({"id":request["id"],"ok":true,"running":true,"active":active,"original_pending":original_pending,"stopping":stopping.load(Ordering::Acquire),"jchtools_broker_protocol":BROKER_PROTOCOL});
            drop(slot); // 控制客户端未读响应不能阻塞其他请求或停止回收。
            log_response("broker", command, &response, started, None);
            stage = "response_write";
            return write_pipe_response(reader.get_mut(), &response, caller_pid, started);
        }
        stage = "runtime_dir";
        let root = PathBuf::from(envelope["runtime_dir"].as_str().ok_or("缺少 Xberg 目录")?);
        let response = (|| {
            stage = "engine_lock";
            let mut slot = engine.lock().map_err(|_| "共享引擎状态异常")?;
            stage = "stopping_gate";
            if stopping.load(Ordering::Acquire) && request["command"] != "cancel" {
                return Err("后台正在退出，不接受新请求".into());
            }
            if request["command"] == "broker-activate-bundled" {
                stage = "bundled_activation";
                let root = std::fs::canonicalize(&root)
                    .map_err(|error| format!("随包 Xberg 目录不可读：{error}"))?;
                return activate_runtime(
                    &mut slot,
                    &root,
                    request["preserve_media"].as_bool().unwrap_or(false),
                )
                .map(|mut response| {
                    response["id"] = request["id"].clone();
                    response
                });
            }
            stage = "engine_exit_probe";
            if let Some(current) = slot.as_mut() {
                if let Some(status) = current.child.try_wait().map_err(|e| e.to_string())? {
                    log_engine_exit(current, status, "observed_exit");
                    slot.take();
                }
            }
            // T-23：通信断裂（响应超限/非法行/流中断）不终止引擎进程时，broken 永不
            // 复位会让后续所有请求连坐失败。终结断裂引擎并重建，让批次继续；在途
            // 请求已由 fail_pending 逐个交付失败终态，此处不存在并发在途项。
            let broken = slot
                .as_ref()
                .is_some_and(|engine| engine.broken.load(Ordering::Acquire));
            if broken {
                if let Some(mut dead) = slot.take() {
                    tracing::warn!(
                        event = "xberg_engine_rebuild_started",
                        reason = "communication_broken",
                        old_pid = dead.child.id(),
                        "共享引擎通信断裂：终结旧引擎并重建（T-23）"
                    );
                    let _ = kill_engine(&mut dead, "broken_rebuild");
                    let _ = wait_engine(&mut dead, "broken_rebuild");
                }
            }
            if let Some(current) = slot.as_mut() {
                if current.root != root {
                    stage = "runtime_switch_validate";
                    let saved = std::fs::canonicalize(xberg_settings::required()?)
                        .map_err(|e| e.to_string())?;
                    if saved != root {
                        return Err("请求使用旧目录，请读取最新共享配置".into());
                    }
                    if current
                        .pending
                        .lock()
                        .map_err(|_| "共享请求表异常")?
                        .values()
                        .any(|p| p.lane.is_some())
                    {
                        return Err("新来源已保存，等待当前任务安全结束后切换".into());
                    }
                    kill_engine(current, "runtime_switch").map_err(|e| e.to_string())?;
                    wait_engine(current, "runtime_switch").map_err(|e| e.to_string())?;
                    slot.take();
                }
            }
            if slot.is_none() {
                // 取消的目标只存在于引擎内的在途任务；引擎不在场（进程退出、通信
                // 断裂终结后的空档或停止窗口）时目标必然已按失败终态交付，为一条
                // 取消重新拉起引擎只会白付进程启动与模型加载，停止窗口内还会被
                // 看门狗立刻终结。按「无事可取消」直接应答，不进入 spawn 分支。
                if request["command"] == "cancel" {
                    tracing::info!(
                        event = "xberg_cancel_without_engine",
                        accepted = false,
                        "取消目标的引擎已不在场，不重新启动引擎"
                    );
                    return Ok(json!({"id":request["id"],"ok":true,"accepted":false}));
                }
                stage = "saved_runtime_validate";
                let saved = std::fs::canonicalize(xberg_settings::required()?)
                    .map_err(|e| format!("已保存的 Xberg 目录不可读：{e}"))?;
                if saved != root {
                    return Err("请求目录与 SQLite 保存的共享 Xberg 目录不一致".into());
                }
                stage = "engine_spawn";
                *slot = Some(Engine::spawn(&saved)?);
            }
            let current = slot.as_ref().ok_or("共享引擎初始化失败")?;
            if current.root != root {
                return Err("Xberg 新目录已保存；常驻引擎仍使用原目录。请从托盘退出后台并重新打开 JchTools 后使用新目录，不会中断当前任务或启动第二个引擎".into());
            }
            if request["command"] == "keepalive" {
                return Ok(
                    json!({"id":request["id"],"ok":true,"jchtools_xberg_pid":current.child.id()}),
                );
            }
            stage = "engine_submit";
            let receive = current.submit(request.clone())?;
            let timeout = Duration::from_millis(request["timeout_ms"].as_u64().unwrap_or(15_000))
                + QUERY_TIMEOUT;
            let pid = current.child.id();
            drop(slot); // 等待响应时不持有引擎锁；截图、取消和查询可立即提交。
            if envelope["acknowledge_submission"] == true {
                stage = "submission_acknowledgement";
                let _guard = IoDeadline::new(QUERY_TIMEOUT)?;
                write_json(reader.get_mut(), &submission_acknowledgement(&request, pid))?;
                tracing::debug!(
                    event = "xberg_broker_submission_acknowledged",
                    engine_pid = pid,
                    "原请求已登记排队，向原连接交付确认"
                );
            }
            stage = "engine_terminal_wait";
            let terminal = if request["command"] == "ocr_snapshot" {
                // 模型 Ready 和取消 ACK 均不能释放截图 lane；持续等待原请求真实终态。
                receive
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
            } else {
                receive.recv_timeout(timeout)
            };
            let result = terminal.map_err(|error| {
                tracing::warn!(
                    event = "xberg_broker_terminal_wait_failed",
                    engine_pid = pid,
                    stage = "engine_terminal_wait",
                    timed_out = matches!(error, mpsc::RecvTimeoutError::Timeout),
                    elapsed_ms = logging::elapsed_ms(started),
                    "共享代理未收到请求终态，保留场景占用"
                );
                "共享 Xberg 未返回请求终态；未终止其他任务".to_string()
            });
            // 超时/客户端离开不删除在途项；只有引擎终态或通信失效才能解除场景占用。
            result.map(|mut response| {
                response["jchtools_xberg_pid"] = json!(pid);
                response["jchtools_broker_pid"] = json!(std::process::id());
                response
            })
        })();
        let mut response = response.unwrap_or_else(|error: String| {
            tracing::warn!(
                event = "xberg_broker_dispatch_failed",
                command,
                stage,
                elapsed_ms = logging::elapsed_ms(started),
                "共享代理请求调度失败"
            );
            json!({"id":request["id"],"ok":false,"error_kind":"shared_runtime","error":error})
        });
        response["jchtools_broker_protocol"] = json!(BROKER_PROTOCOL);
        response["jchtools_broker_pid"] = json!(std::process::id());
        log_response("broker", command, &response, started, None);
        stage = "response_write";
        write_pipe_response(reader.get_mut(), &response, caller_pid, started)
    })();
    match &result {
        Ok(()) if command == "keepalive" => tracing::debug!(
            event = "xberg_broker_request_completed",
            caller_pid,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理已完成保活响应发送"
        ),
        Ok(()) => tracing::debug!(
            event = "xberg_broker_request_completed",
            caller_pid,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理已完成请求响应发送"
        ),
        Err(_) => tracing::warn!(
            event = "xberg_broker_request_failed",
            caller_pid,
            command,
            stage,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理请求处理失败"
        ),
    }
    result
}

/// 代理是内部常驻进程，只经命名管道通信，不使用任何继承来的标准句柄。
/// Windows 上 Rust std 即使把本进程 stdio 全部置为 null，CreateProcess 仍会沿
/// 可继承句柄链把祖父进程（cargo / PowerShell 的 `& cmd 2>&1` 捕获）的匿名管道
/// 写端带进本进程；代理常驻会让上层捕获永远等不到 EOF，测试与验收管线悬挂
/// （回归：tests/xberg_shared_process.rs broker_does_not_hold_client_capture_pipes）。
/// 此处仅在 serve() 最前执行：本进程尚未创建任何自有管道，std 句柄是 NUL
/// 字符设备，能命中的只有继承来的管道。
fn close_inherited_pipes() {
    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_PIPE};
    for value in (4usize..65536).step_by(4) {
        // SAFETY: handle 只是本进程句柄表的探测值，转换本身无副作用。
        let handle: HANDLE = value as HANDLE;
        let mut flags = 0u32;
        // SAFETY: 探测本进程句柄表中的值；无效值由返回 0 过滤，flags 是有效输出指针。
        let known = unsafe { GetHandleInformation(handle, &raw mut flags) };
        if known == 0 {
            continue;
        }
        // SAFETY: handle 已被 GetHandleInformation 确认有效；GetFileType 只读句柄类型。
        let file_type = unsafe { GetFileType(handle) };
        if file_type != FILE_TYPE_PIPE {
            continue;
        }
        // SAFETY: 关闭本进程内确认有效的继承管道句柄；此后不再有任何引用。
        let closed = unsafe { CloseHandle(handle) };
        debug_assert_ne!(closed, 0);
    }
}

pub(crate) fn serve() -> Result<(), String> {
    let span = logging::operation_span("xberg_runtime", "broker_serve");
    let _entered = span.enter();
    let started = Instant::now();
    let mut stage = "state_directory";
    tracing::info!(event = "xberg_broker_serve_started", "共享代理初始化开始");
    let result = (|| {
        close_inherited_pipes();
        let root = xberg_settings::state_dir()?;
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        stage = "session_lock";
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(format!("xberg-{}.lock", identity()?)))
            .map_err(|e| e.to_string())?;
        match lock.try_lock_exclusive() {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                tracing::debug!(
                    event = "xberg_broker_already_running",
                    "共享代理已在运行，本进程不重复启动"
                );
                return Ok(());
            }
            Err(e) => {
                tracing::error!(
                    event = "xberg_broker_lock_failed",
                    error_code = e.raw_os_error(),
                    "共享引擎锁失败"
                );
                return Err(format!("共享引擎锁失败：{e}"));
            }
        }
        let name = pipe_name()?;
        if let Err(error) = background_allowed() {
            tracing::warn!(
                event = "xberg_broker_start_blocked",
                reason = "tray_stopped",
                "共享代理被托盘退出标记阻止"
            );
            return Err(error);
        }
        tracing::info!(
            event = "xberg_broker_listening",
            pid = std::process::id(),
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理开始监听"
        );
        let engine: Arc<Mutex<Option<Engine>>> = Arc::new(Mutex::new(None));
        let stopping = Arc::new(AtomicBool::new(false));
        let active_handlers = Arc::new(AtomicUsize::new(0));
        let watch_handlers = Arc::clone(&active_handlers);
        let watch_engine = engine.clone();
        let watch_stopping = stopping.clone();
        let watch_span = tracing::Span::current();
        std::thread::spawn(move || {
            let _entered = watch_span.enter();
            let mut engine_lock_failure_logged = false;
            let mut pending_lock_failure_logged = false;
            loop {
                std::thread::sleep(Duration::from_millis(100));
                if !watch_stopping.load(Ordering::Acquire) {
                    // T-23 复审：reader 线程通信断裂时只能置 broken 并交付在途失败；
                    // 引擎进程本体若此后再无请求，会带着模型内存一直存活到代理退出。
                    // 非停止期巡检 broken 引擎并立即终结，把该窗口从「下一请求」
                    // 收敛到本循环周期；期间 handle() 的 T-23 重建分支同样以空槽重建，
                    // 两者不冲突（槽锁互斥）。
                    if let Ok(mut slot) = watch_engine.lock() {
                        let broken = slot
                            .as_ref()
                            .is_some_and(|engine| engine.broken.load(Ordering::Acquire));
                        if broken {
                            if let Some(mut dead) = slot.take() {
                                let kill_ok = kill_engine(&mut dead, "broken_watchdog").is_ok();
                                let wait_ok = wait_engine(&mut dead, "broken_watchdog").is_ok();
                                tracing::info!(
                                    event = "xberg_engine_watchdog_terminated",
                                    kill_ok,
                                    wait_ok,
                                    engine_pid = dead.child.id(),
                                    "看门狗已完成通信断裂引擎的终结处理（T-23）"
                                );
                            }
                        }
                    } else if !engine_lock_failure_logged {
                        engine_lock_failure_logged = true;
                        tracing::warn!(
                            event = "xberg_engine_watchdog_failed",
                            stage = "engine_lock",
                            error_kind = "mutex_poisoned",
                            "共享引擎看门狗状态锁异常，保留既有隔离状态"
                        );
                    }
                    continue;
                }
                if let Ok(mut slot) = watch_engine.lock() {
                    let active = slot.as_ref().is_some_and(|e| {
                        e.pending.lock().map_or_else(
                            |_| {
                                if !pending_lock_failure_logged {
                                    pending_lock_failure_logged = true;
                                    tracing::warn!(
                                        event = "xberg_engine_watchdog_failed",
                                        stage = "pending_lock",
                                        error_kind = "mutex_poisoned",
                                        "停止看门狗不能确认在途场景任务，保持后台运行"
                                    );
                                }
                                true
                            },
                            |p| p.values().any(|v| v.lane.is_some()),
                        )
                    });
                    if active {
                        continue;
                    }
                    if watch_handlers.load(Ordering::Acquire) != 0 {
                        // 请求可能已经从 pending 交付给 handle，但响应仍在管道
                        // 中；等 write_pipe_response 的有界刷出完成或连接失败。
                        continue;
                    }
                    if let Some(mut current) = slot.take() {
                        let kill_ok = kill_engine(&mut current, "stop_watchdog").is_ok();
                        let wait_ok = wait_engine(&mut current, "stop_watchdog").is_ok();
                        tracing::info!(
                            event = "xberg_engine_stop_watchdog_terminated",
                            kill_ok,
                            wait_ok,
                            engine_pid = current.child.id(),
                            "停止看门狗已完成共享引擎终结处理（XB-23）"
                        );
                    }
                    // P-10：exit(0) 会跳过 main 栈上的日志句柄析构，非阻塞写入线程
                    // 缓冲中的关键记录（含上方终结记录与本行）会整批丢失；退出前
                    // 显式刷盘，再终止进程。
                    tracing::info!(
                        event = "xberg_broker_stopped",
                        exit_code = 0,
                        elapsed_ms = logging::elapsed_ms(started),
                        "共享代理退出前刷盘诊断日志"
                    );
                    logging::flush_before_exit();
                    std::process::exit(0);
                } else if !engine_lock_failure_logged {
                    engine_lock_failure_logged = true;
                    tracing::warn!(
                        event = "xberg_engine_watchdog_failed",
                        stage = "engine_lock",
                        error_kind = "mutex_poisoned",
                        "停止看门狗状态锁异常，保持后台运行"
                    );
                }
            }
        });
        loop {
            stage = "listener";
            let pipe = match create_pipe(&name) {
                Ok(pipe) => pipe,
                Err(error) if error.raw_os_error() == Some(231) => {
                    // ERROR_PIPE_BUSY：64 个实例仍被客户端持有，只等待局部容量释放。
                    // 停止看门狗仍独立推进；不得因接入压力回收在途共享引擎。
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(error) => {
                    tracing::error!(
                        event = "xberg_pipe_create_failed",
                        stage = "create_pipe",
                        error_code = error.raw_os_error(),
                        "创建共享管道失败"
                    );
                    return Err(format!("创建共享管道失败：{error}"));
                }
            };
            // SAFETY: 同步管道的有效句柄；客户端先连接的 ERROR_PIPE_CONNECTED 也有效。
            let connected = unsafe {
                windows_sys::Win32::System::Pipes::ConnectNamedPipe(
                    pipe.as_raw_handle().cast(),
                    std::ptr::null_mut(),
                )
            };
            // SAFETY: 读取本线程紧邻 API 调用的错误码。
            if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                // P-10：管道接受失败是 IPC 关键异常，必须落盘（含 Windows 错误码）。
                // SAFETY: 与上一行读取同一错误码，两次调用之间没有任何其他 API。
                let code = unsafe { GetLastError() };
                tracing::warn!(
                    event = "xberg_pipe_accept_failed",
                    stage = "accept",
                    code,
                    "共享管道接受客户端连接失败"
                );
                continue;
            }
            let engine = Arc::clone(&engine);
            let stopping = stopping.clone();
            // XB-08/XB-23：停止后仍解析新连接，让已登记任务的取消可达。
            let active_handlers = Arc::clone(&active_handlers);
            let span = tracing::Span::current();
            std::thread::spawn(move || {
                let _entered = span.enter();
                if handle(pipe, &engine, &stopping, &active_handlers).is_err() {
                    tracing::warn!(
                        event = "xberg_broker_connection_failed",
                        stage = "receive_or_handle",
                        "共享代理连接处理失败"
                    );
                }
            });
        }
    })();
    if result.is_err() {
        tracing::error!(
            event = "xberg_broker_serve_failed",
            stage,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理服务失败"
        );
    }
    result
}
