use super::{
    error, mpsc, oneshot, receive, send, watch, CancellationToken, Control, ControlOperation,
    IncomingRequest, Operation, PublicDiscovery, Request, Response, ServiceDiscovery, ServiceError,
    ServiceStatus, PROTOCOL,
};
use sha2::{Digest, Sha256};
const HEX: &[u8; 16] = b"0123456789abcdef";
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::Duration;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tracing::Instrument;
use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_PIPE_BUSY, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Pipes::{
    GetNamedPipeClientProcessId, GetNamedPipeClientSessionId, GetNamedPipeServerProcessId,
    GetNamedPipeServerSessionId,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenProcess, OpenProcessToken, ReleaseMutex,
    WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};
#[link(name = "kernel32")]
extern "system" {
    fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: 本结构独占有效 Windows 句柄。
        unsafe {
            CloseHandle(self.0);
        }
    }
}
pub(in crate::acp_api::runtime) struct AuthenticatedPeer {
    pid: u32,
    process: OwnedHandle,
}
impl AuthenticatedPeer {
    pub(in crate::acp_api::runtime) fn pid(&self) -> u32 {
        self.pid
    }
    pub(in crate::acp_api::runtime) fn wait_for_exit(&self) -> Result<(), ServiceError> {
        // SAFETY: 认证连接时取得的同一 OS 对象一直由本结构持有；不按 PID 重开。
        let result = unsafe { WaitForSingleObject(self.process.as_raw_handle(), u32::MAX) };
        if result != WAIT_OBJECT_0 {
            let cause = std::io::Error::last_os_error();
            tracing::error!(event = "acp_maintenance_stop_failed", component = "acp_runtime", stage = "wait_process", peer_pid = self.pid, error_type = ?cause.kind(), error_code = cause.raw_os_error(), "ACP 安装维护等待后台退出失败");
            return Err(error("无法确认模型服务后台已退出"));
        }
        Ok(())
    }
}
struct LocalAllocation(*mut std::ffi::c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: 本结构独占 Windows LocalAlloc 系列 API 返回的内存。
        unsafe {
            LocalFree(self.0);
        }
    }
}
struct Security(*mut std::ffi::c_void);
impl Security {
    fn new() -> Result<Self, ServiceError> {
        let user = current_user_sid()?;
        // 显式 Owner 和用户 ACE；OW 是占位组，不能替代真实 TokenUser。
        let sddl: Vec<u16> = format!("O:{user}D:P(A;;GA;;;SY)(A;;GA;;;{user})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: 输入以零终止，输出指针有效，分配结果由本结构释放。
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(error("创建模型服务 owner ACL 失败"));
        }
        Ok(Self(descriptor))
    }
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}
impl Drop for Security {
    fn drop(&mut self) {
        // SAFETY: 此指针由 Windows 安全描述符转换函数分配，仅释放一次。
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn token_user(process: HANDLE) -> Result<Vec<usize>, ServiceError> {
    let mut token = std::ptr::null_mut();
    // SAFETY: process 在调用期间有效；成功输出的 token 由 Handle 唯一释放。
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(error("读取 Windows 进程用户失败"));
    }
    let token = Handle(token);
    let mut size = 0;
    // SAFETY: 零长查询仅取得所需缓冲区长度。
    unsafe {
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &raw mut size);
    }
    if size < u32::try_from(std::mem::size_of::<TOKEN_USER>()).map_err(|e| error(e.to_string()))? {
        return Err(error("读取 Windows 用户 SID 长度失败"));
    }
    let mut data = vec![0_usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: 缓冲区正确对齐，至少 size 字节；有效 token。
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            data.as_mut_ptr().cast(),
            size,
            &raw mut size,
        )
    } == 0
    {
        return Err(error("读取 Windows 用户 SID 失败"));
    }
    Ok(data)
}
fn current_user_sid() -> Result<String, ServiceError> {
    // SAFETY: 获取当前进程伪句柄，不转交所有权。
    let process = unsafe { GetCurrentProcess() };
    let data = token_user(process)?;
    // SAFETY: 成功的 TokenUser 查询返回有效且正确对齐的 TOKEN_USER。
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = std::ptr::null_mut();
    // SAFETY: SID 在 data 生命周期内有效；结果由 LocalAllocation 唯一释放。
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut sid) } == 0 {
        return Err(error("编码 Windows 用户 SID 失败"));
    }
    let allocation = LocalAllocation(sid.cast());
    let mut length = 0;
    // SAFETY: 转换 API 成功返回以零终止的 UTF-16 字符串。
    while unsafe { *sid.wrapping_add(length) } != 0 {
        length += 1;
    }
    // SAFETY: 前述扫描确定合法字符串范围，allocation 持有其分配。
    let user_id = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid, length) });
    drop(allocation);
    Ok(user_id)
}
fn process_session(pid: u32) -> Result<u32, ServiceError> {
    let mut session = 0;
    // SAFETY: 输出会话 ID 指针有效，PID 由内核端点或当前进程提供。
    if unsafe { ProcessIdToSessionId(pid, &raw mut session) } == 0 {
        return Err(error("读取 Windows 登录会话失败"));
    }
    Ok(session)
}
fn identity() -> Result<String, ServiceError> {
    let user_id = current_user_sid()?;
    let session = process_session(std::process::id())?;
    let mut identity = format!("{user_id}-{session}");
    // 仅显式隔离构建加入状态根；产品单实例不受配置或目录变更影响。
    if cfg!(any(test, feature = "test-hooks")) {
        let root = jchtools_foundation::xberg_settings::state_dir().map_err(error)?;
        let digest = Sha256::digest(root.as_os_str().to_string_lossy().as_bytes());
        identity.push('-');
        for byte in digest {
            identity.push(char::from(HEX[usize::from(byte >> 4)]));
            identity.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    Ok(identity)
}
fn pipe_name() -> Result<String, ServiceError> {
    Ok(format!(r"\\.\pipe\jchtools-acp-http-{}", identity()?))
}
pub(in crate::acp_api::runtime) struct InstanceLock {
    handle: Handle,
}
impl Drop for InstanceLock {
    fn drop(&mut self) {
        // SAFETY: 锁在创建线程取得且同线程析构，仍拥有 mutex。
        unsafe {
            ReleaseMutex(self.handle.0);
        }
    }
}
pub(in crate::acp_api::runtime) fn lock(
    kind: &str,
    wait: bool,
) -> Result<Option<InstanceLock>, ServiceError> {
    let name: Vec<u16> = format!("Local\\jchtools-acp-http-{kind}-{}", identity()?)
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let security = Security::new()?;
    let attributes = security.attributes();
    // SAFETY: 名字及 ACL 在调用中有效，不继承 handle。
    let raw = unsafe { CreateMutexW(&raw const attributes, 0, name.as_ptr()) };
    if raw.is_null() {
        let cause = std::io::Error::last_os_error();
        tracing::error!(event = "acp_control_lock_failed", component = "acp_control", stage = "create", lock_kind = kind, error_type = ?cause.kind(), error_code = cause.raw_os_error(), "ACP 控制单实例锁创建失败");
        return Err(error(format!("创建模型服务单实例锁失败：{cause}")));
    }
    let handle = Handle(raw);
    // SAFETY: handle 是 mutex；同步 facade/service 均在本线程持有和释放。
    match unsafe { WaitForSingleObject(raw, if wait { 30_000 } else { 0 }) } {
        WAIT_OBJECT_0 => Ok(Some(InstanceLock { handle })),
        WAIT_ABANDONED => {
            tracing::warn!(
                event = "acp_control_lock_recovered",
                component = "acp_control",
                lock_kind = kind,
                status = "abandoned_owner_recovered",
                "ACP 控制单实例锁此前的持有者异常退出，当前进程已接管"
            );
            Ok(Some(InstanceLock { handle }))
        }
        windows_sys::Win32::Foundation::WAIT_TIMEOUT => {
            tracing::debug!(
                event = "acp_control_lock_busy",
                component = "acp_control",
                lock_kind = kind,
                waiting = wait,
                "ACP 控制单实例锁仍由其他操作持有"
            );
            Ok(None)
        }
        _ => {
            let cause = std::io::Error::last_os_error();
            tracing::error!(event = "acp_control_lock_failed", component = "acp_control", stage = "wait", lock_kind = kind, error_type = ?cause.kind(), error_code = cause.raw_os_error(), "等待 ACP 控制单实例锁失败");
            Err(error("等待模型服务单实例锁失败"))
        }
    }
}
fn ensure_same_pipe_session(pipe: HANDLE) -> Result<(), ServiceError> {
    let mut client_session = 0;
    let mut server_session = 0;
    // SAFETY: 连接持有有效本地管道句柄，客户端会话输出指针有效。
    let client_result = unsafe { GetNamedPipeClientSessionId(pipe, &raw mut client_session) };
    // SAFETY: 同一有效管道句柄，服务端会话输出指针有效；不信任名称或请求字段。
    let server_result = unsafe { GetNamedPipeServerSessionId(pipe, &raw mut server_session) };
    if client_result == 0 || server_result == 0 {
        return Err(error("无法核验模型服务控制管道的登录会话"));
    }
    if client_session != server_session {
        return Err(error("拒绝跨 Windows 登录会话访问模型服务控制管道"));
    }
    Ok(())
}
fn ensure_same_pipe_user(pipe: HANDLE, server: bool) -> Result<AuthenticatedPeer, ServiceError> {
    ensure_same_pipe_session(pipe)?;
    let mut pid = 0;
    let result = if server {
        // SAFETY: 有效连接管道，输出内核客户端 PID 指针有效。
        unsafe { GetNamedPipeClientProcessId(pipe, &raw mut pid) }
    } else {
        // SAFETY: 有效连接管道，输出内核服务端 PID 指针有效。
        unsafe { GetNamedPipeServerProcessId(pipe, &raw mut pid) }
    };
    if result == 0 || pid == 0 {
        return Err(error("无法确认模型服务进程归属"));
    }
    // SAFETY: 查询和同步权限在认证连接仍存活时一并取得，不继承、不终止进程。
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if process.is_null() {
        return Err(error("无法读取模型服务对端进程身份"));
    }
    // SAFETY: 有效进程句柄唯一移交；保留到调用者完成控制/OS 退出观察。
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    if process_session(pid)? != process_session(std::process::id())? {
        return Err(error("拒绝跨 Windows 登录会话访问模型服务控制管道"));
    }
    let peer_data = token_user(process.as_raw_handle())?;
    // SAFETY: 获取当前进程伪句柄，不转交所有权。
    let current = unsafe { GetCurrentProcess() };
    let own_data = token_user(current)?;
    // SAFETY: 两个缓冲区来自成功的 TokenUser 查询，正确对齐且仍存活。
    let peer = unsafe { &*peer_data.as_ptr().cast::<TOKEN_USER>() };
    // SAFETY: own_data 来自成功的 TokenUser 查询，正确对齐且仍存活。
    let own = unsafe { &*own_data.as_ptr().cast::<TOKEN_USER>() };
    // SAFETY: SID 分别在仍存活的 TokenUser 缓冲区内。
    if unsafe { EqualSid(peer.User.Sid, own.User.Sid) } == 0 {
        return Err(error("拒绝其他 Windows 用户访问模型服务控制管道"));
    }
    Ok(AuthenticatedPeer { pid, process })
}
pub(in crate::acp_api::runtime) async fn exchange<T: serde::de::DeserializeOwned>(
    operation: Operation,
) -> Result<Option<T>, ServiceError> {
    Ok(exchange_with_peer(operation, None)
        .await?
        .map(|(response, _peer)| response))
}
pub(in crate::acp_api::runtime) async fn exchange_with_peer<T: serde::de::DeserializeOwned>(
    operation: Operation,
    expected: Option<&AuthenticatedPeer>,
) -> Result<Option<(T, AuthenticatedPeer)>, ServiceError> {
    let diagnostic_id = uuid::Uuid::new_v4();
    let diagnostic_text = diagnostic_id.to_string();
    let span = jchtools_foundation::logging::operation_span_with_id(
        "acp_control",
        "exchange",
        &diagnostic_text,
    );
    crate::acp_api::diagnostics::async_call("acp_control", "exchange", async {
        // 查询从首次连接到响应共用一个 budget；生命周期收尾仍不设置强制超时。
        let response_timeout_ms = if matches!(operation, Operation::Stop | Operation::Apply) {
            None
        } else {
            Some(15000_u64)
        };
        tracing::debug!(
            event = "acp_control_exchange_started",
            component = "acp_control",
            method = operation.name(),
            %diagnostic_id,
            connect_timeout_ms = 15000,
            ?response_timeout_ms,
            "ACP 控制管道客户端交换开始"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let name = pipe_name()?;
        let connected = async {
            let mut attempt = 0_u64;
            loop {
                attempt += 1;
                match ClientOptions::new().open(&name) {
                    Ok(pipe) => return Ok(Some(pipe)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.cast_signed()) => {
                        tracing::warn!(
                            event = "acp_control_connect_retry",
                            component = "acp_control",
                            method = operation.name(),
                            attempt,
                            reason = "pipe_busy",
                            "ACP 控制管道繁忙，等待下一监听实例"
                        );
                        // 不阻塞 executor，不启动后台；等服务发布下一监听实例。
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(e) => {
                        tracing::error!(event = "acp_control_connect_failed", component = "acp_control", stage = "open", method = operation.name(), error_type = ?e.kind(), error_code = ?e.raw_os_error(), "ACP 控制管道客户端连接失败");
                        return Err(error(format!("连接模型服务控制管道失败：{e}")))
                    }
                }
            }
        };
        let Some(mut pipe) = tokio::time::timeout_at(deadline, connected)
            .await
            .map_err(|_| {
                tracing::error!(event = "acp_control_exchange_timeout", component = "acp_control", stage = "connect", method = operation.name(), timeout_ms = 15000, error_type = "timeout", "ACP 控制管道连接超时");
                error("模型服务控制查询超时")
            })??
        else {
            tracing::debug!(
                event = "acp_control_peer_absent",
                component = "acp_control",
                method = operation.name(),
                status = "not_running",
                "ACP 控制管道后台缺席"
            );
            return Ok(None);
        };
        // 在写入任何控制操作前核验真实端点 TokenUser 和会话；不信任名字/自报 PID。
        let peer = ensure_same_pipe_user(pipe.as_raw_handle().cast(), false)?;
        let actual_pid = peer.pid;
        // expected 保留旧进程对象，PID 不会被复用；新连接不准控制另一实例。
        if expected.is_some_and(|expected| expected.pid != actual_pid) {
            return Err(error("模型服务维护控制连接已切换到其他后台实例"));
        }
        tracing::debug!(
            event = "acp_control_connected",
            component = "acp_control",
            peer_pid = actual_pid,
            method = operation.name(),
            "ACP 控制管道客户端已验证服务端"
        );
        let work = async {
            send(
                &mut pipe,
                &Request {
                    operation,
                    diagnostic_id,
                },
            )
            .await?;
            let response: Response<T> = receive(&mut pipe).await?;
            if response.protocol != PROTOCOL || response.pid != actual_pid {
                tracing::error!(event = "acp_control_exchange_failed", component = "acp_control", stage = "response_validation", method = operation.name(), peer_pid = actual_pid, expected_protocol = PROTOCOL, received_protocol = response.protocol, error_type = "protocol_or_peer_mismatch", "ACP 控制响应协议或对端身份不匹配");
                return Err(error("模型服务控制协议或进程归属不匹配"));
            }
            response.result.map(Some)
        };
        let response = if matches!(operation, Operation::Stop | Operation::Apply) {
            work.await
        } else {
            tokio::time::timeout_at(deadline, work)
                .await
                .map_err(|_| {
                    tracing::error!(event = "acp_control_exchange_timeout", component = "acp_control", stage = "response", method = operation.name(), peer_pid = actual_pid, timeout_ms = 15000, error_type = "timeout", "ACP 控制操作响应超时");
                    error("模型服务控制查询超时")
                })?
        }?;
        Ok(response.map(|response| (response, peer)))
    })
    .instrument(span)
    .await
}
fn create_pipe(first: bool) -> Result<NamedPipeServer, ServiceError> {
    let security = Security::new()?;
    let mut attributes = security.attributes();
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    // SAFETY: SECURITY_ATTRIBUTES 与其描述符在创建调用中有效，Tokio 复制配置。
    unsafe {
        options.create_with_security_attributes_raw(pipe_name()?, (&raw mut attributes).cast())
    }
    .map_err(|e| {
        tracing::error!(event = "acp_control_listener_failed", component = "acp_control", stage = "create", error_type = ?e.kind(), error_code = ?e.raw_os_error(), "ACP 控制管道监听创建失败");
        error(format!("创建模型服务控制管道失败：{e}"))
    })
}
#[cfg(test)]
pub(in crate::acp_api::runtime) async fn serve(
    status: watch::Receiver<ServiceStatus>,
    instance_id: uuid::Uuid,
    commands: mpsc::UnboundedSender<Control>,
    shutdown: CancellationToken,
    ready: oneshot::Sender<Result<(), ServiceError>>,
) -> Result<(), ServiceError> {
    serve_inner(status, instance_id, commands, shutdown, ready, None).await
}
pub(in crate::acp_api::runtime) async fn serve_for_process(
    status: watch::Receiver<ServiceStatus>,
    instance_id: uuid::Uuid,
    commands: mpsc::UnboundedSender<Control>,
    shutdown: CancellationToken,
    ready: oneshot::Sender<Result<(), ServiceError>>,
    finished: oneshot::Sender<()>,
) -> Result<(), ServiceError> {
    serve_inner(
        status,
        instance_id,
        commands,
        shutdown,
        ready,
        Some(finished),
    )
    .await
}
async fn serve_inner(
    status: watch::Receiver<ServiceStatus>,
    instance_id: uuid::Uuid,
    commands: mpsc::UnboundedSender<Control>,
    shutdown: CancellationToken,
    ready: oneshot::Sender<Result<(), ServiceError>>,
    mut finished: Option<oneshot::Sender<()>>,
) -> Result<(), ServiceError> {
    crate::acp_api::diagnostics::async_call("acp_control", "serve", async { let mut pipe = match create_pipe(true) {
    Ok(pipe) => {
        tracing::info!(event = "acp_control_listener_ready", component = "acp_control", protocol = PROTOCOL, "ACP 控制管道监听已就绪");
        let _ = ready.send(Ok(()));
        pipe
    }
    Err(error) => {
        let _ = ready.send(Err(error.clone()));
        return Err(error);
    }
};
let mut commands = Some(commands);
let mut workers = tokio::task::JoinSet::new();
let mut observing_exit = false;
loop {
    tokio::select! {
        () = shutdown.cancelled(), if !observing_exit => {
            tracing::info!(event = "acp_control_listener_stopping", component = "acp_control", reason = "shutdown_requested", "ACP 控制管道监听收到停止请求");
            if let Some(finished) = finished.take() {
                // 已提交 Stop/Apply 的回执和安全资源收尾全部完成后才允许 serve 返回。
                while let Some(result) = workers.join_next().await {
                    if let Err(error) = result {
                        tracing::error!(event = "acp_control_worker_failed", component = "acp_control", stage = "shutdown_join", cancelled = error.is_cancelled(), panicked = error.is_panic(), "ACP 控制管道连接任务收尾异常");
                    }
                }
                // 控制器在封口后还会完成已排队的控制请求；关闭最后的 admission sender。
                drop(commands.take());
                observing_exit = true;
                let _ = finished.send(());
                // 原 pipe/认证入口继续存活，覆盖 main 刷盘直到 OS 退出，不接受重启。
                continue;
            }
            break;
        },
        result = pipe.connect() => result.map_err(|e| {
            tracing::error!(event = "acp_control_listener_failed", component = "acp_control", stage = "accept", error_type = ?e.kind(), error_code = ?e.raw_os_error(), "ACP 控制管道接受连接失败");
            error(format!("接受模型服务控制连接失败：{e}"))
        })?,
    }
    let mut connected = pipe;
    pipe = create_pipe(false)?;
    let peer_pid = match ensure_same_pipe_user(connected.as_raw_handle().cast(), true) {
        Ok(peer) => peer.pid,
        Err(error) => {
            tracing::warn!(event = "acp_control_peer_rejected", component = "acp_control", stage = "peer_identity", error_type = ?error.kind, "拒绝非同一用户或登录会话的模型服务控制连接");
            // 未解析任何操作，拒绝不触发状态发现或生命周期变更。
            let _ = connected.disconnect();
            continue;
        }
    };
    let status = status.clone();
    let commands = commands.clone();
    let shutdown = shutdown.clone();
    workers.spawn(crate::acp_api::diagnostics::async_call("acp_control", "server_connection", async move {
        tracing::debug!(event = "acp_control_connection_received", component = "acp_control", peer_pid, "ACP 控制管道服务端已验证客户端");
        // 只取消尚未提交操作的连接；已接收的 Apply/Stop 仍等待控制器和回执收尾。
        let request: IncomingRequest = tokio::select! {
            biased;
            () = shutdown.cancelled(), if !observing_exit => {
                tracing::info!(event = "acp_control_connection_cancelled", component = "acp_control", stage = "receive", peer_pid, reason = "listener_shutdown", "ACP 控制管道停止时关闭尚未提交操作的连接");
                return Ok(());
            },
            result = tokio::time::timeout(Duration::from_secs(15), receive(&mut connected)) =>
                result.map_err(|_| {
                    tracing::error!(event = "acp_control_request_timeout", component = "acp_control", stage = "receive", peer_pid, timeout_ms = 15000, error_type = "timeout", "ACP 控制请求接收超时");
                    error("模型服务控制请求超时")
                })??,
        };
        let (operation, injected_id, request_protocol) = match request {
            IncomingRequest::PrivateControl(request) =>
                (request.operation, Some(request.diagnostic_id), "private_control"),
            IncomingRequest::PublicDiscovery(PublicDiscovery::Discover) =>
                (Operation::Discover, None, "public_discovery"),
        };
        let diagnostic_id = injected_id.unwrap_or_else(uuid::Uuid::new_v4);
        let diagnostic_text = diagnostic_id.to_string();
        let span = jchtools_foundation::logging::operation_span_with_id("acp_control", "server_request", &diagnostic_text);
        crate::acp_api::diagnostics::async_call("acp_control", "server_request", handle_control_request(
            &mut connected,
            ControlRequestContext {
                status,
                instance_id,
                commands,
                shutdown,
                diagnostic_id,
                request_protocol,
                peer_pid,
            },
            observing_exit,
            operation,
            injected_id,
        )).instrument(span).await
    }).instrument(jchtools_foundation::logging::operation_span("acp_control", "server_connection")));
    // 回收已完成控制连接，不累积历史响应。
    while let Some(result) = workers.try_join_next() {
        if let Err(error) = result {
            tracing::error!(event = "acp_control_worker_failed", component = "acp_control", stage = "worker_join", cancelled = error.is_cancelled(), panicked = error.is_panic(), "ACP 控制管道连接任务异常结束");
        }
    }
}
while let Some(result) = workers.join_next().await {
    if let Err(error) = result {
        tracing::error!(event = "acp_control_worker_failed", component = "acp_control", stage = "shutdown_join", cancelled = error.is_cancelled(), panicked = error.is_panic(), "ACP 控制管道连接任务收尾异常");
    }
}
Ok(()) }).await
}

// 控制请求的会话级上下文：worker 内克隆捕获，随连接一次性按值传入；
// 打包只影响参数形式，不改变 Discover 直答 / 终态投影 / Status 读库 / 生命周期排队。
struct ControlRequestContext {
    status: watch::Receiver<ServiceStatus>,
    instance_id: uuid::Uuid,
    commands: Option<mpsc::UnboundedSender<Control>>,
    shutdown: CancellationToken,
    diagnostic_id: uuid::Uuid,
    request_protocol: &'static str,
    peer_pid: u32,
}

// 单个控制请求的四路分发：Discover 直答、退出尾段终态投影、Status 读库、生命周期排队。
// observing_exit 按值传入，保持「worker spawn 时快照」的语义；
// Stop 的回执 guard 活到控制器完成与回执发送之后，不提前取消资源收尾。
async fn handle_control_request(
    connected: &mut NamedPipeServer,
    context: ControlRequestContext,
    observing_exit: bool,
    operation: Operation,
    injected_id: Option<uuid::Uuid>,
) -> Result<(), ServiceError> {
    let ControlRequestContext {
        status,
        instance_id,
        commands,
        shutdown,
        diagnostic_id,
        request_protocol,
        peer_pid,
    } = context;
    tracing::debug!(event = "acp_control_request_received", component = "acp_control", %diagnostic_id, diagnostic_id_from_peer = injected_id.is_some(), request_protocol, peer_pid, method = operation.name(), "ACP 控制管道收到操作");
    // 收尾失败仍要退役后台；回执写入失败也不能留下已封口的控制器。
    // guard 在控制器实际完成和回执发送之后释放，不提前取消资源收尾。
    let _stop_reply_guard =
        matches!(operation, Operation::Stop).then(|| shutdown.clone().drop_guard());
    let result = if matches!(operation, Operation::Discover) {
        // Discover 不进入生命周期队列，也不读取磁盘 saved_config。
        let discovery = ServiceDiscovery::from_status(instance_id, &status.borrow());
        return send(
            connected,
            &Response {
                protocol: PROTOCOL,
                pid: std::process::id(),
                result: Ok(discovery),
            },
        )
        .await;
    } else if observing_exit {
        if matches!(operation, Operation::Apply) {
            Err(error("模型服务后台已退役，无法应用配置"))
        } else {
            // 退出尾段的 Status/Stop 仅投影终态，不读库、不排队、不启动 Agent。
            Ok(status.borrow().clone())
        }
    } else if matches!(operation, Operation::Status) {
        // 配置写入不需排在 drain/stop 后；实时返回磁盘 saved 与当前 running。
        let saved = tokio::task::spawn_blocking(crate::acp_api::settings::load_config)
            .await
            .map_err(|e| error(format!("读取配置任务失败：{e}")))?;
        let mut current = status.borrow().clone();
        match saved {
            Ok(saved) => current.saved_config = saved,
            Err(error) => {
                current.phase = crate::acp_api::ServicePhase::Error;
                current.error = Some(error.message);
            }
        }
        Ok(current)
    } else {
        let operation = match operation {
            Operation::Apply => ControlOperation::Apply,
            Operation::Stop => ControlOperation::Stop,
            Operation::Status | Operation::Discover => {
                return Err(error("查询不能进入生命周期控制器"));
            }
        };
        let (reply, answer) = oneshot::channel();
        commands
            .as_ref()
            .ok_or_else(|| error("模型服务控制器已停止"))?
            .send(Control {
                operation,
                reply,
                span: tracing::Span::current(),
            })
            .map_err(|_| error("模型服务控制器已停止"))?;
        answer.await.map_err(|_| error("模型服务控制器异常退出"))?
    };
    if let Err(error) = &result {
        tracing::error!(event = "acp_control_operation_failed", component = "acp_control", method = operation.name(), error_type = ?error.kind, stage = "controller_result", "ACP 控制操作返回失败");
    }
    let response = Response {
        protocol: PROTOCOL,
        pid: std::process::id(),
        result,
    };
    send(connected, &response).await
}

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod tests;
