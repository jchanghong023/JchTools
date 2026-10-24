use super::background::{background_allowed, pipe_name};
use super::deadline::IoDeadline;
use super::message::{read_json, write_json};
use super::{BROKER_PROTOCOL, QUERY_TIMEOUT};
use jchtools_foundation::logging;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

pub(super) fn connect(start: bool, deadline: Instant) -> Result<File, String> {
    let started = Instant::now();
    tracing::debug!(
        event = "xberg_broker_connect_started",
        allow_start = start,
        "开始连接共享代理"
    );
    let mut last_error_code = None;
    let mut attempt = 0u64;
    let result = (|| {
        let name = pipe_name()?;
        let open = || OpenOptions::new().read(true).write(true).open(&name);
        if Instant::now() >= deadline {
            return Err("共享 Xberg 代理连接期限已到".into());
        }
        match open() {
            Ok(pipe) => return Ok(pipe),
            Err(error) => last_error_code = error.raw_os_error(),
        }
        if start {
            background_allowed()?;
            // 代理是 JchTools/截图服务自身的内部模式；只有赢得会话锁的代理可创建引擎。
            let executable = if cfg!(feature = "test-hooks")
                && std::env::var_os("JCHTOOLS_TEST_STATE_DIR").is_some()
            {
                std::env::var_os("JCHTOOLS_TEST_BROKER_EXE").map(PathBuf::from)
            } else {
                None
            }
            .map_or_else(|| std::env::current_exe().map_err(|e| e.to_string()), Ok)?;
            tracing::info!(
                event = "xberg_broker_spawn_started",
                executable = "JchTools",
                mode = "xberg_broker",
                "开始启动共享代理候选进程"
            );
            let child = Command::new(executable)
                .arg("--xberg-broker")
                .creation_flags(0x0800_0000)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| {
                    tracing::warn!(
                        event = "xberg_broker_spawn_failed",
                        stage = "spawn",
                        error_code = e.raw_os_error(),
                        elapsed_ms = logging::elapsed_ms(started),
                        "共享代理候选进程启动失败"
                    );
                    format!("无法启动共享 Xberg 代理：{e}")
                })?;
            tracing::info!(
                event = "xberg_broker_spawn_completed",
                peer_pid = child.id(),
                "共享代理候选进程已启动"
            );
        }
        loop {
            attempt = attempt.saturating_add(1);
            match open() {
                Ok(pipe) => return Ok(pipe),
                Err(error) => last_error_code = error.raw_os_error(),
            }
            tracing::debug!(
                event = "xberg_broker_connect_retry",
                attempt,
                "共享代理尚未可连接，继续既有轮询"
            );
            if Instant::now() >= deadline {
                return Err("共享 Xberg 代理连接超时".into());
            }
            std::thread::sleep(
                Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    })();
    if result.is_ok() {
        tracing::debug!(
            event = "xberg_broker_connect_completed",
            attempt,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理连接完成"
        );
    } else {
        tracing::warn!(
            event = "xberg_broker_connect_failed",
            stage = "connect",
            attempt,
            error_code = last_error_code,
            elapsed_ms = logging::elapsed_ms(started),
            timed_out = Instant::now() >= deadline,
            "共享代理连接失败"
        );
    }
    result
}

fn pipe_server_pid(pipe: &File) -> Option<u32> {
    let mut pid = 0;
    // SAFETY: File 持有有效客户端管道句柄，pid 是有效输出指针；查询不改变通信状态。
    let ok = unsafe {
        windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId(
            pipe.as_raw_handle().cast(),
            &raw mut pid,
        )
    };
    (ok != 0).then_some(pid)
}

/// XB-08/T-23：登记确认只属于原请求；它不代替终态，也不改变引擎的一行响应协议。
pub(super) fn submission_acknowledgement(request: &Value, engine_pid: u32) -> Value {
    json!({
        "id": request["id"],
        "ok": true,
        "jchtools_request_submitted": true,
        "jchtools_xberg_pid": engine_pid,
        "jchtools_broker_pid": std::process::id(),
        "jchtools_broker_protocol": BROKER_PROTOCOL
    })
}

pub(super) fn read_broker_terminal(
    reader: &mut impl BufRead,
    request_id: &Value,
    submitted: Option<&AtomicBool>,
    mut terminal_deadline: Option<&mut Option<IoDeadline>>,
) -> Result<Value, String> {
    let mut acknowledged = false;
    loop {
        let response = read_json(reader)?;
        if response["jchtools_broker_protocol"] != BROKER_PROTOCOL {
            return Err(
                "常驻 Xberg 代理协议不兼容，请结束旧版本会话后重试；未启动第二个引擎".into(),
            );
        }
        if &response["id"] != request_id {
            return Err("共享 Xberg 响应 ID 不匹配".into());
        }
        if response["jchtools_request_submitted"] != true {
            return Ok(response);
        }
        let state = submitted.ok_or("共享 Xberg 返回了未请求的登记确认")?;
        if acknowledged || response["ok"] != true {
            return Err("共享 Xberg 请求登记确认异常".into());
        }
        acknowledged = true;
        if let Some(deadline) = terminal_deadline.as_mut() {
            // 先撤销同步读看门狗，再发布可取消登记，避免期限与确认交错。
            drop(deadline.take());
        }
        state.store(true, Ordering::Release);
        tracing::debug!(
            event = "xberg_request_submission_confirmed",
            engine_pid = response["jchtools_xberg_pid"].as_u64(),
            "原请求已登记并排队，可以发送请求级取消"
        );
    }
}

pub(super) fn exchange(pipe: File, envelope: &Value, timeout: Duration) -> Result<Value, String> {
    exchange_with_submission(pipe, envelope, timeout, None)
}

fn exchange_with_submission(
    mut pipe: File,
    envelope: &Value,
    timeout: Duration,
    submitted: Option<&AtomicBool>,
) -> Result<Value, String> {
    let started = Instant::now();
    let mut stage = "io_deadline";
    let peer_pid = pipe_server_pid(&pipe);
    tracing::debug!(event = "xberg_exchange_started", "共享代理消息交换开始");
    let result = (|| {
        // 截图原请求在取消后仍可能处于不可中断推理；读终态不能沿用请求期限。
        // 写入仍有界，终态由引擎响应、通信断裂或用户显式强退交付。
        let mut deadline = Some(IoDeadline::new(timeout)?);
        stage = "write";
        write_json(&mut pipe, envelope)?;
        tracing::debug!(
            event = "xberg_request_sent",
            peer_pid,
            "共享代理请求消息已发送"
        );
        stage = "read_and_protocol";
        let response = read_broker_terminal(
            &mut BufReader::new(pipe),
            &envelope["request"]["id"],
            submitted,
            (envelope["request"]["command"] == "ocr_snapshot").then_some(&mut deadline),
        )?;
        tracing::debug!(
            event = "xberg_response_received",
            peer_pid,
            broker_pid = response["jchtools_broker_pid"].as_u64(),
            engine_pid = response["jchtools_xberg_pid"].as_u64(),
            "共享代理响应已接收并验证"
        );
        Ok(response)
    })();
    if result.is_err() {
        tracing::warn!(
            event = "xberg_exchange_failed",
            peer_pid,
            stage,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理消息交换失败"
        );
    }
    result
}

pub(super) fn log_response(
    side: &'static str,
    command: &'static str,
    response: &Value,
    started: Instant,
    peer_pid: Option<u32>,
) {
    let elapsed_ms = logging::elapsed_ms(started);
    let broker_pid = response["jchtools_broker_pid"].as_u64();
    let engine_pid = response["jchtools_xberg_pid"].as_u64();
    let accepted = response["accepted"].as_bool();
    let error_code = response["error_code"]
        .as_i64()
        .or_else(|| response["code"].as_i64());
    if response["ok"] != true {
        tracing::warn!(
            event = "xberg_response_failed",
            side,
            command,
            stage = "remote",
            error_kind = super::diagnostic_error_kind(response),
            error_code,
            peer_pid,
            broker_pid,
            engine_pid,
            elapsed_ms,
            "共享引擎返回失败结果，通信成功不代表业务成功"
        );
    } else if command == "keepalive" {
        tracing::debug!(
            event = "xberg_response_completed",
            side,
            command,
            peer_pid,
            broker_pid,
            engine_pid,
            elapsed_ms,
            "共享引擎保活完成"
        );
    } else {
        tracing::debug!(
            event = "xberg_response_completed",
            side,
            command,
            peer_pid,
            broker_pid,
            engine_pid,
            accepted,
            elapsed_ms,
            page_count = response["document"]["pages"].as_array().map(Vec::len),
            content_bytes = response["document"]["content"].as_str().map(str::len),
            markdown_bytes = response["markdown"].as_str().map(str::len),
            running = response["running"].as_bool(),
            active = response["active"].as_u64(),
            stopping = response["stopping"].as_bool(),
            "共享引擎响应完成"
        );
    }
}

pub(crate) fn request(
    root: &Path,
    value: &Value,
    timeout: Duration,
    cancel: &AtomicBool,
    parent_operation_id: &str,
) -> Result<Value, String> {
    let started = Instant::now();
    let id = value["id"].as_str().unwrap_or_default();
    let span = logging::operation_span_with_id("xberg_runtime", "broker_request", id);
    let _entered = span.enter();
    let command = super::diagnostic_command(value["command"].as_str());
    if command == "keepalive" {
        tracing::debug!(
            event = "xberg_client_request_started",
            parent_operation_id,
            request_id = id,
            command,
            timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            "客户端提交共享引擎请求"
        );
    } else {
        tracing::debug!(
            event = "xberg_client_request_started",
            parent_operation_id,
            request_id = id,
            command,
            timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            "客户端提交共享引擎请求"
        );
    }
    let result = request_via_broker(root, value, timeout, cancel, parent_operation_id);
    if let Ok(response) = &result {
        log_response("client", command, response, started, None);
    } else {
        tracing::warn!(
            event = "xberg_client_request_failed",
            stage = "transport_or_cancel",
            command,
            elapsed_ms = logging::elapsed_ms(started),
            "共享引擎客户端请求失败"
        );
    }
    result
}

/// 原连接异常时只查询同一代理的原请求登记；不以取消 ACK 或模型 Ready 代替终态。
fn wait_snapshot_terminal_after_disconnect(broker: &File, broker_pid: u32, target_id: &Value) {
    loop {
        // SAFETY: 本次提交前持有的代理进程句柄，仅查询退出，不终结任何进程。
        if unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(
                broker.as_raw_handle().cast(),
                0,
            )
        } == 0
        {
            return;
        }
        let response = connect(false, Instant::now() + Duration::from_secs(1))
            .and_then(|pipe| {
                if pipe_server_pid(&pipe) != Some(broker_pid) {
                    return Err("原代理已不可连接，仍等待其退出".into());
                }
                exchange(pipe, &json!({"request":{
                    "id":logging::new_operation_id(), "command":"broker-state", "target_id":target_id,
                }}), Duration::from_secs(1))
            });
        if response
            .is_ok_and(|response| response["ok"] == true && response["original_pending"] == false)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 提交前预检：三段期限检查（零期限、交换期限溢出、Instant 转换溢出）与
/// 根目录规范化。返回连接期限与规范化后的根目录，供原请求信封与取消信封共用。
/// 交换期限依赖连接后的 remaining，仍由 request_via_broker 主流程计算。
fn preflight_deadline_and_root(
    root: &Path,
    timeout: Duration,
) -> Result<(Instant, PathBuf), String> {
    if timeout.is_zero() {
        tracing::warn!(
            event = "xberg_client_preflight_failed",
            stage = "timeout_budget",
            timed_out = true,
            "共享请求期限已到，未启动代理"
        );
        return Err("共享 Xberg 请求期限已到".into());
    }
    let _exchange_extra = timeout.checked_add(QUERY_TIMEOUT).ok_or_else(|| {
        tracing::warn!(
            event = "xberg_client_preflight_failed",
            stage = "timeout_range",
            "共享请求期限过大，未启动代理"
        );
        "Xberg 请求期限过大，未启动共享代理"
    })?;
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        tracing::warn!(
            event = "xberg_client_preflight_failed",
            stage = "deadline_range",
            "共享请求期限无法转换，未启动代理"
        );
        "Xberg 请求期限过大，未启动共享代理"
    })?;
    let root = std::fs::canonicalize(root).map_err(|e| {
        tracing::warn!(
            event = "xberg_client_preflight_failed",
            stage = "runtime_directory",
            error_code = e.raw_os_error(),
            "共享引擎目录不可读，未提交请求"
        );
        format!("Xberg 目录不可读：{e}")
    })?;
    Ok((deadline, root))
}

/// 取消后原请求未在 1 秒内结束时的六分支文案：取消待确认（cancel_pending）、
/// 取消失败（cancel_failed）与取消已确认三态各按是否超时给出对应原因。
fn cancellation_deadline_reason(
    timed_out: bool,
    cancel_pending: bool,
    cancel_failed: bool,
) -> String {
    if cancel_pending {
        if timed_out {
            "Xberg 请求超时且取消接口未在 1 秒内确认"
        } else {
            "请求已取消但取消接口未在 1 秒内确认"
        }
    } else if cancel_failed {
        if timed_out {
            "Xberg 请求超时且取消接口失败，原请求未在 1 秒内结束"
        } else {
            "取消接口失败，原请求未在 1 秒内结束"
        }
    } else if timed_out {
        "Xberg 请求超时且取消已确认，原请求未在 1 秒内结束"
    } else {
        "请求已取消且取消已确认，原请求未在 1 秒内结束"
    }
    .to_string()
}

/// 收割取消线程结果。harvested=false 表示本轮无收割；harvested=true 时 error
/// 为 None 表示收割到取消成功、Some(error) 表示收割到取消失败并携带错误。
/// 不变量：登记确认后 cancellation_receive 为 Some 时 cancellation_error 必为
/// None，因此收割结果可以直接覆盖调用方的 cancellation_error。
fn poll_cancellation(
    receive: &mut Option<mpsc::Receiver<Result<(), String>>>,
) -> (bool, Option<String>) {
    let Some(receiver) = receive.as_ref() else {
        return (false, None);
    };
    match receiver.try_recv() {
        Ok(result) => {
            *receive = None;
            (true, result.err())
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            tracing::warn!(
                event = "xberg_cancel_failed",
                stage = "cancel_thread",
                "请求级取消接口线程提前退出"
            );
            *receive = None;
            (true, Some("取消接口线程退出".into()))
        }
        Err(mpsc::TryRecvError::Empty) => (false, None),
    }
}

/// 构造请求级取消信封并派生独立取消线程，返回 1 秒强截止期限与结果接收端。
/// 期限溢出（「取消接口期限无效」）经返回的 Result 传播给主循环，不能吞掉。
fn spawn_cancellation(
    root: &Path,
    value: &Value,
    target_id: &str,
    cancel_id: &str,
) -> Result<(Instant, mpsc::Receiver<Result<(), String>>), String> {
    let cancellation = json!({"runtime_dir":root,"request":{"id":cancel_id,"command":"cancel","target_id":value["id"]},"diagnostic":{"parent_operation_id":target_id,"caller_pid":std::process::id()}});
    let io_deadline = Instant::now()
        .checked_add(Duration::from_secs(1))
        .ok_or("取消接口期限无效")?;
    let (cancel_send, cancel_receive) = mpsc::channel();
    let cancel_deadline = io_deadline;
    let span = logging::operation_span_with_id("xberg_runtime", "cancel", cancel_id);
    std::thread::spawn(move || {
        let _entered = span.enter();
        let started = Instant::now();
        let result = connect(false, cancel_deadline)
            .and_then(|pipe| {
                let remaining = cancel_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("取消接口期限已到".into());
                }
                exchange(pipe, &cancellation, remaining)
            })
            .and_then(|response| {
                log_response("cancel", "cancel", &response, started, None);
                super::checked(response)
            })
            .map(|response| {
                tracing::info!(
                    event = "xberg_cancel_completed",
                    accepted = response["accepted"].as_bool(),
                    elapsed_ms = logging::elapsed_ms(started),
                    "请求级取消接口返回，仍等待原请求终态"
                );
            });
        if result.is_err() {
            tracing::warn!(
                event = "xberg_cancel_failed",
                stage = "cancel_exchange",
                elapsed_ms = logging::elapsed_ms(started),
                "请求级取消接口失败，原任务仍由代理管理"
            );
        }
        if cancel_send.send(result).is_err() {
            tracing::warn!(
                event = "xberg_cancel_delivery_failed",
                stage = "caller_left",
                "取消接口已返回，但原请求等待端已离开"
            );
        }
    });
    Ok((io_deadline, cancel_receive))
}

pub(super) fn request_via_broker(
    root: &Path,
    value: &Value,
    timeout: Duration,
    cancel: &AtomicBool,
    parent_operation_id: &str,
) -> Result<Value, String> {
    let (deadline, root) = preflight_deadline_and_root(root, timeout)?;
    let pipe = connect(true, deadline)?;
    let snapshot_broker = if value["command"] == "ocr_snapshot" {
        let pid = pipe_server_pid(&pipe).ok_or("无法确认截图原代理进程")?;
        // SAFETY: 仅请求 SYNCHRONIZE，提交前持有句柄以防 PID 复用；无终结权限。
        let handle =
            unsafe { windows_sys::Win32::System::Threading::OpenProcess(0x0010_0000, 0, pid) };
        if handle.is_null() {
            return Err("无法观察截图原代理退出，未提交截图".into());
        }
        // SAFETY: 唯一拥有上方新句柄，File 析构调用 CloseHandle。
        Some((
            Arc::new(unsafe { File::from_raw_handle(handle.cast()) }),
            pid,
        ))
    } else {
        None
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        tracing::warn!(
            event = "xberg_client_preflight_failed",
            stage = "timeout_budget",
            timed_out = true,
            "共享请求连接已耗尽期限，未提交推理"
        );
        return Err("Xberg 请求期限已到，未提交推理".into());
    }
    let exchange_timeout = remaining
        .checked_add(QUERY_TIMEOUT)
        .ok_or("Xberg 请求期限过大，未提交推理")?;
    let acknowledge_submission = matches!(
        value["command"].as_str(),
        Some("extract" | "transcribe" | "ocr_snapshot")
    );
    let envelope = json!({"runtime_dir":root, "request":value, "acknowledge_submission":acknowledge_submission, "diagnostic":{"parent_operation_id":parent_operation_id,"caller_pid":std::process::id()}});
    let (send, receive) = mpsc::channel();
    let span = tracing::Span::current();
    // 状态只由本次原连接持有，线程结束便释放；不保留请求墓碑。
    let submitted = Arc::new(AtomicBool::new(!acknowledge_submission));
    let reader_submitted = Arc::clone(&submitted);
    let response_broker = snapshot_broker.clone();
    let reader = std::thread::spawn(move || {
        let _entered = span.enter();
        let result = exchange_with_submission(
            pipe,
            &envelope,
            exchange_timeout,
            acknowledge_submission.then_some(reader_submitted.as_ref()),
        );
        if result.is_err() && reader_submitted.load(Ordering::Acquire) {
            if let Some((broker, pid)) = response_broker {
                tracing::warn!(
                    event = "xberg_snapshot_terminal_transport_lost",
                    "截图终态连接异常，继续等待原请求结束或原代理退出"
                );
                wait_snapshot_terminal_after_disconnect(&broker, pid, &envelope["request"]["id"]);
            }
        }
        if send.send(result).is_err() {
            tracing::warn!(
                event = "xberg_client_response_delivery_failed",
                stage = "caller_left",
                "共享代理交换已结束，但客户端等待端已离开"
            );
        }
    });
    let end = deadline;
    let mut cancellation_sent = false;
    let mut cancellation_error = None;
    let mut cancellation_io_deadline = None;
    let mut cancellation_receive: Option<mpsc::Receiver<Result<(), String>>> = None;
    let mut timed_out = false;
    loop {
        let (harvested, new_error) = poll_cancellation(&mut cancellation_receive);
        if harvested {
            cancellation_error = new_error;
        }
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(result) => {
                let _ = reader.join();
                if cancellation_sent {
                    tracing::info!(
                        event = "xberg_cancel_original_terminal",
                        timed_out,
                        transport_ok = result.is_ok(),
                        "取消后已收到原请求终态"
                    );
                }
                return match (result, cancellation_error) {
                    (Err(error), Some(cancel_error)) if value["command"] == "ocr_snapshot" =>
                        Err(format!("取消接口失败：{cancel_error}；{error}；原请求已结束或原代理已退出")),
                    (Err(error), Some(cancel_error)) => Err(format!("取消接口失败：{cancel_error}；{error}；代理仍保留未结束任务，阻止同场景重入")),
                    (result, _) => result.map(|response| restore_timeout_kind(response, timed_out)),
                };
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!(
                    event = "xberg_response_thread_failed",
                    stage = "response_thread",
                    "共享引擎响应线程提前退出"
                );
                let _ = reader.join();
                if submitted.load(Ordering::Acquire) {
                    if let Some((broker, pid)) = snapshot_broker.as_ref() {
                        wait_snapshot_terminal_after_disconnect(broker, *pid, &value["id"]);
                    }
                }
                return Err("共享 Xberg 响应线程退出".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if value["command"] != "ocr_snapshot"
            && cancellation_sent
            && cancellation_io_deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
            let reason = cancellation_deadline_reason(
                timed_out,
                cancellation_receive.is_some(),
                cancellation_error.is_some(),
            );
            tracing::warn!(
                event = "xberg_cancel_original_timeout",
                timed_out,
                cancel_pending = cancellation_receive.is_some(),
                cancel_failed = cancellation_error.is_some(),
                stage = "original_terminal",
                "取消后原请求未在期限内结束，场景仍保持占用"
            );
            return Err(format!("{reason}；代理仍保留未结束任务，阻止同场景重入"));
        }
        // 耗时任务的取消意图可以先到，但独立取消连接不能越过原连接的登记确认。
        // submit 在持有 pending 锁时排入唯一 FIFO，确认后取消必定排在原请求后。
        if !cancellation_sent
            && submitted.load(Ordering::Acquire)
            && (cancel.load(Ordering::Acquire) || Instant::now() >= end)
        {
            cancellation_sent = true;
            timed_out = Instant::now() >= end && !cancel.load(Ordering::Acquire);
            let target_id = value["id"].as_str().unwrap_or_default();
            let cancel_id = format!("cancel-{target_id}");
            tracing::info!(event = "xberg_cancel_started", target_request_id = target_id, cancel_request_id = %cancel_id, timed_out, "开始发送请求级取消");
            let (io_deadline, cancel_receive) =
                spawn_cancellation(&root, value, target_id, &cancel_id)?;
            cancellation_io_deadline = Some(io_deadline);
            cancellation_receive = Some(cancel_receive);
            // 接受取消不等于任务结束；继续等待原请求的终态，禁止伪报成功。
        }
    }
}

/// 引擎把主动超时的 cancel 也报 cancelled；原请求终态到达后恢复真正的结束原因。
pub(super) fn restore_timeout_kind(mut response: Value, timed_out: bool) -> Value {
    if timed_out
        && matches!(
            response["error_kind"].as_str(),
            Some("cancelled" | "canceled")
        )
    {
        response["error_kind"] = json!("timeout");
        response["error"] = json!("Xberg 请求已超时");
    }
    response
}
