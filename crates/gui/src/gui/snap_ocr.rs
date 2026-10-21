//! 截图 OCR 页：就绪/初始化流程、命名管道请求与热键映射接线（自 gui.rs 拆分）。
use super::*;

/// 已安装资产只在工作线程校验；打开工具页不会联网。
pub(super) fn start_snap_readiness(ui: &AppWindow, state: &Rc<RefCell<State>>, _out: &EventSender) {
    if ui.get_snap_initializing() {
        return;
    }
    let (generation, sender) = {
        let mut state = state.borrow_mut();
        state.snap_generation = state.snap_generation.wrapping_add(1);
        (state.snap_generation, state.snap_sender.clone())
    };
    ui.set_snap_ready(false);
    ui.set_snap_asset_status("正在离线校验已安装组件…".into());
    ui.set_snap_asset_detail("".into());
    ui.set_snap_error("".into());
    std::thread::spawn(move || {
        let _ = sender.send(SnapMessage::Readiness(
            generation,
            snap_ocr_assets::readiness(),
        ));
        wake_event_loop();
    });
}

fn start_snap_initialize(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    if ui.get_snap_ready() || ui.get_snap_initializing() {
        return;
    }
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (generation, sender) = {
        let mut state = state.borrow_mut();
        state.snap_generation = state.snap_generation.wrapping_add(1);
        state.snap_init_cancel = Some(cancel.clone());
        (state.snap_generation, state.snap_sender.clone())
    };
    ui.set_snap_ready(false);
    ui.set_snap_initializing(true);
    ui.set_snap_error("".into());
    ui.set_snap_asset_detail("".into());
    ui.set_snap_progress("正在准备组件…".into());
    ui.set_snap_asset_status("正在初始化可选组件…".into());
    std::thread::spawn(move || {
        let result = snap_ocr_assets::initialize(&cancel, |progress| {
            let _ = sender.send(SnapMessage::Progress(generation, progress));
            wake_event_loop();
        });
        let result = if cancel.load(Ordering::Acquire) {
            Err("用户取消初始化".to_owned())
        } else {
            result
        };
        let _ = sender.send(SnapMessage::Initialized(generation, result));
        wake_event_loop();
    });
}

/// 每条请求打开独立管道连接；服务重启时无需保存失效句柄。
#[cfg(windows)]
pub(super) fn snap_pipe_request(request: &serde_json::Value) -> Result<serde_json::Value, String> {
    snap_pipe_request_on(
        &snap_ocr_assets::pipe_name()?,
        request,
        SNAP_PIPE_READ_TIMEOUT,
    )
}

/// 截图服务读响应的 deadline（C-1）：监督线程是全部管道命令的唯一串行执行者，
/// 服务挂起时一次永久阻塞会让界面按钮永久禁用且无恢复路径（O-11/O-30）。
/// 心跳/控制类命令正常都在毫秒级返回；10 秒已远超正常窗口，仅用于兜底挂起。
#[cfg(windows)]
const SNAP_PIPE_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// 在指定命名管道上执行一次「单连接请求-响应」（C-1：读响应带 deadline，
/// 服务挂起时按超时错误返回而不是永久阻塞）。生产用固定管道名与默认超时；
/// 回归测试经管道名与超时参数注入，用本地服务端模拟挂起。
#[cfg(windows)]
pub(super) fn snap_pipe_request_on(
    pipe: &str,
    request: &serde_json::Value,
    read_timeout: Duration,
) -> Result<serde_json::Value, String> {
    let operation_id = jchtools_foundation::logging::new_operation_id();
    let span = jchtools_foundation::logging::operation_span_with_id(
        "snap_control_client",
        "request",
        &operation_id,
    );
    let _entered = span.enter();
    let started = Instant::now();
    let command = request["command"]
        .as_str()
        .filter(|command| {
            matches!(
                *command,
                "ping"
                    | "get-state"
                    | "capture"
                    | "retry-load"
                    | "attach-main-exe"
                    | "set-hotkey"
                    | "set-autostart"
                    | "exit"
            )
        })
        .unwrap_or("other");
    let polling = command == "get-state";
    if polling {
        tracing::debug!(
            event = "snap_control_request_started",
            command,
            "截图控制查询开始"
        );
    } else {
        tracing::debug!(
            event = "snap_control_request_started",
            command,
            connect_timeout_ms = 2000,
            read_timeout_ms = u64::try_from(read_timeout.as_millis()).unwrap_or(u64::MAX),
            "截图控制请求开始"
        );
    }
    let mut stage = "connect";
    let mut peer_pid = 0_u32;
    let result = (|| {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(pipe)
            {
                Ok(stream) => break stream,
                // 服务只有一个管道实例。前一条响应写出后，它还需断开客户端并
                // 重新等待连接；立即发送下一条请求时应等待这个正常交接窗口。
                Err(error) if error.raw_os_error() == Some(231) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(format!("无法连接截图服务：{error}")),
            }
        };
        use std::os::windows::io::AsRawHandle;
        // SAFETY: stream 是有效连接句柄，peer_pid 指针在同步查询期间有效。
        let _ = unsafe {
            windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId(
                stream.as_raw_handle().cast(),
                &raw mut peer_pid,
            )
        };
        stage = "encode";
        let mut payload =
            serde_json::to_vec(request).map_err(|error| format!("请求编码失败：{error}"))?;
        // 复用既有编码缓冲区附加低敏ID，不复制控制请求对象或修改业务字段。
        if request.is_object() {
            payload.pop();
            if payload.len() > 1 {
                payload.push(b',');
            }
            payload.extend_from_slice(b"\"diagnostic_id\":\"");
            payload.extend_from_slice(operation_id.as_bytes());
            payload.extend_from_slice(b"\"}");
        }
        payload.push(b'\n');
        stage = "send";
        stream
            .write_all(&payload)
            .map_err(|error| format!("发送服务请求失败：{error}"))?;
        stage = "flush";
        stream
            .flush()
            .map_err(|error| format!("刷新服务请求失败：{error}"))?;
        stage = "clone_connection";
        // 读响应放入独立线程、主线程限时 join（jchtools_file_core::process::join_with_deadline）：
        // 超时后取消原句柄上的未决读，让读线程以错误返回并退出（防线程泄漏）。
        let mut read_stream = stream
            .try_clone()
            .map_err(|error| format!("复制服务连接失败：{error}"))?;
        stage = "spawn_reader";
        let reader_span = tracing::Span::current();
        let reader = std::thread::Builder::new()
            .name("snap-pipe-read".into())
            .spawn(move || {
                let _entered = reader_span.enter();
                let mut response = String::new();
                BufReader::new(&mut read_stream)
                    .read_line(&mut response)
                    .map(|_| response)
            })
            .map_err(|error| format!("启动响应读取线程失败：{error}"))?;
        stage = "receive";
        let response = match jchtools_file_core::process::join_with_deadline(reader, read_timeout) {
            Some(Ok(response)) => response,
            Some(Err(error)) => return Err(format!("读取服务响应失败：{error}")),
            None => {
                stage = "receive_timeout";
                cancel_pipe_io(&stream);
                return Err(format!(
                    "截图服务响应超时（{read_timeout:.1?} 未返回）；请在设置页重试后台连接"
                ));
            }
        };
        if response.is_empty() {
            return Err("截图服务未返回响应".into());
        }
        stage = "decode";
        serde_json::from_str::<serde_json::Value>(&response)
            .map_err(|error| format!("截图服务响应格式错误：{error}"))
    })();
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match &result {
        Ok(response) if polling => tracing::debug!(
            event = "snap_control_request_completed",
            command,
            peer_pid,
            elapsed_ms,
            ok = response["ok"].as_bool(),
            "截图控制查询完成"
        ),
        Ok(response) => tracing::debug!(
            event = "snap_control_request_completed",
            command,
            peer_pid,
            elapsed_ms,
            ok = response["ok"].as_bool(),
            diagnostic_id_received = response["diagnostic_id"] == operation_id,
            "截图控制请求完成"
        ),
        Err(_) => tracing::warn!(
            event = "snap_control_request_failed",
            command,
            stage,
            peer_pid,
            elapsed_ms,
            error_type = "control_ipc",
            "截图控制请求失败"
        ),
    }
    result
}

/// 取消句柄上的未决同步 I/O（C-1）：读线程阻塞在复制句柄的 read_line 上，
/// 对原句柄调用 CancelIoEx 即可跨线程取消该未决读（两句柄指向同一管道实例）。
#[cfg(windows)]
fn cancel_pipe_io(stream: &std::fs::File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::CancelIoEx;
    // SAFETY: stream 在调用期间存活且句柄有效；CancelIoEx 只取消该句柄关联管道
    // 实例上的未决 I/O（lpoverlapped 为空表示取消全部），不触碰其他资源；返回值
    // 仅指示是否存在未决 I/O，失败时读线程会随连接关闭自行退出，无需处理。
    unsafe {
        CancelIoEx(stream.as_raw_handle(), std::ptr::null());
    }
}

#[cfg(not(windows))]
pub(super) fn snap_pipe_request(_request: &serde_json::Value) -> Result<serde_json::Value, String> {
    Err("截图服务仅支持 Windows".into())
}

/// 热键录制时 Shift+数字行/标点在 US 布局下的反向映射表（E-4）：带 Shift 的按键
/// 事件文本是布局映射后的符号（如 Shift+7 → "&"），录制必须还原回基础键。
/// 本表必须与 optional/snap-ocr-worker 侧录制逻辑的同一映射逐对保持一致
/// （US 布局主键区 Shift 符号共 21 对：数字行 10 对 + 标点 11 对）。
pub(super) const SNAP_SHIFT_SYMBOL_BASE: [(char, char); 21] = [
    ('!', '1'),
    ('@', '2'),
    ('#', '3'),
    ('$', '4'),
    ('%', '5'),
    ('^', '6'),
    ('&', '7'),
    ('*', '8'),
    ('(', '9'),
    (')', '0'),
    ('~', '`'),
    ('_', '-'),
    ('+', '='),
    ('{', '['),
    ('}', ']'),
    ('|', '\\'),
    (':', ';'),
    ('"', '\''),
    ('<', ','),
    ('>', '.'),
    ('?', '/'),
];

/// 录制主键归一化（E-4）：取单字符文本为主键；Shift 按下且主键为布局符号时
/// 反向映射回基础键（"&" → '7'），其余原样返回。多字符文本（输入法组合等）返回 None。
pub(super) fn normalize_recorded_primary_key(text: &str, shift: bool) -> Option<char> {
    let code = text
        .chars()
        .next()
        .filter(|_| text.chars().nth(1).is_none())?;
    if !shift {
        return Some(code);
    }
    Some(
        SNAP_SHIFT_SYMBOL_BASE
            .iter()
            .find_map(|(symbol, base)| (*symbol == code).then_some(*base))
            .unwrap_or(code),
    )
}

fn snap_named_hotkey(code: char) -> Option<&'static str> {
    use slint::platform::Key;
    [
        (Key::Backspace, "Backspace"),
        (Key::Tab, "Tab"),
        (Key::Return, "Return"),
        (Key::Escape, "Escape"),
        (Key::Space, "Space"),
        (Key::PageUp, "PageUp"),
        (Key::PageDown, "PageDown"),
        (Key::End, "End"),
        (Key::Home, "Home"),
        (Key::LeftArrow, "Left"),
        (Key::UpArrow, "Up"),
        (Key::RightArrow, "Right"),
        (Key::DownArrow, "Down"),
        (Key::Insert, "Insert"),
        (Key::Delete, "Delete"),
    ]
    .into_iter()
    .find_map(|(special, label)| (char::from(special) == code).then_some(label))
}

pub(super) fn wire_snap_ocr(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    let weak = ui.as_weak();
    let state_init = state.clone();
    let out_init = out.clone();
    ui.on_snap_initialize(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_snap_ready() {
                start_snap_readiness(&ui, &state_init, &out_init);
            } else {
                start_snap_initialize(&ui, &state_init);
            }
        }
    });
    let weak = ui.as_weak();
    let state_cancel = state.clone();
    ui.on_snap_cancel_initialize(move || {
        if let Some(cancel) = &state_cancel.borrow().snap_init_cancel {
            cancel.store(true, Ordering::Release);
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_asset_status("正在取消初始化…".into());
        }
    });
    let weak = ui.as_weak();
    let state_connect = state.clone();
    let out_connect = out.clone();
    ui.on_snap_connect(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_request_pending(true);
            ui.set_snap_service_status("正在连接截图服务…".into());
            ensure_snap_supervisor(&state_connect, &out_connect);
        }
    });
    let weak = ui.as_weak();
    ui.on_snap_record_key(move |key, control, alt, shift, meta| {
        let Some(ui) = weak.upgrade() else { return };
        if !(control || alt || shift || meta) {
            return;
        }
        let Some(code) = normalize_recorded_primary_key(&key, shift) else {
            return;
        };
        let mut combination = String::with_capacity(24);
        if control {
            combination.push_str("Ctrl+");
        }
        if alt {
            combination.push_str("Alt+");
        }
        if shift {
            combination.push_str("Shift+");
        }
        if meta {
            combination.push_str("Win+");
        }
        if code.is_ascii_alphanumeric() {
            combination.push(code.to_ascii_uppercase());
        } else if let Some(number) = (code as u32)
            .checked_sub(char::from(slint::platform::Key::F1) as u32)
            .filter(|number| *number < 24)
        {
            // number ∈ 1..=24：直接按十进制拼出 F1–F24。
            let number = number + 1;
            combination.push('F');
            combination.push_str(&number.to_string());
        } else if let Some(name) = snap_named_hotkey(code) {
            combination.push_str(name);
        } else {
            return;
        }
        ui.set_snap_hotkey_draft(combination.into());
        ui.set_snap_recording(false);
        ui.set_snap_error("".into());
    });
    let weak = ui.as_weak();
    let state_save = state.clone();
    ui.on_snap_save_settings(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_request_pending(true);
            snap_send_command(
                &state_save,
                SnapCommand::Request(serde_json::json!({
                    "command": "save-settings",
                    "hotkey": if ui.get_screen() == 7 { ui.get_snap_hotkey() } else { ui.get_snap_hotkey_draft() }.as_str(),
                    "autostart": if ui.get_screen() == 6 { ui.get_snap_autostart() } else { ui.get_snap_autostart_draft() },
                })),
            );
        }
    });
    let weak = ui.as_weak();
    ui.on_snap_draft_autostart(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_autostart_draft(enabled);
        }
    });
    let weak = ui.as_weak();
    let state_retry = state.clone();
    ui.on_snap_retry_load(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_request_pending(true);
            snap_send_command(
                &state_retry,
                SnapCommand::Request(serde_json::json!({"command": "retry-load"})),
            );
        }
    });
    let weak = ui.as_weak();
    let state_capture = state.clone();
    ui.on_snap_capture(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_snap_request_pending(true);
            snap_send_command(
                &state_capture,
                SnapCommand::Request(serde_json::json!({"command": "capture"})),
            );
        }
    });
}
