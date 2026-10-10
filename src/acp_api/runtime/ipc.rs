//! 当前 Windows 用户及登录会话专用控制通道；不以 HTTP 端口猜测服务归属。
#[cfg(windows)]
use crate::acp_api::ServiceDiscovery;
use crate::acp_api::{ServiceError, ServiceErrorKind, ServiceStatus};
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
const PROTOCOL: u32 = 1;
#[cfg(windows)]
const MAX_FRAME: usize = crate::acp_api::settings::CONTROL_FRAME_LIMIT;
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) enum Operation {
    Status,
    Apply,
    Stop,
    Discover,
}
pub(super) enum ControlOperation {
    Apply,
    Stop,
}
pub(super) struct Control {
    pub(super) operation: ControlOperation,
    pub(super) reply: oneshot::Sender<Result<ServiceStatus, ServiceError>>,
}
#[cfg(windows)]
#[derive(Serialize, Deserialize)]
struct Response<T> {
    protocol: u32,
    pid: u32,
    result: Result<T, ServiceError>,
}
pub(super) fn error(message: impl Into<String>) -> ServiceError {
    ServiceError::new(ServiceErrorKind::Io, message)
}
#[cfg(windows)]
async fn send<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<(), ServiceError> {
    let data = serde_json::to_vec(value).map_err(|e| error(e.to_string()))?;
    if data.len() > MAX_FRAME {
        return Err(error("模型服务控制消息过大"));
    }
    let length = u32::try_from(data.len()).map_err(|e| error(e.to_string()))?;
    stream
        .write_u32_le(length)
        .await
        .map_err(|e| error(format!("写入模型服务控制管道失败：{e}")))?;
    stream
        .write_all(&data)
        .await
        .map_err(|e| error(format!("写入模型服务控制管道失败：{e}")))?;
    stream.flush().await.map_err(|e| error(e.to_string()))
}
#[cfg(windows)]
async fn receive<T: serde::de::DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<T, ServiceError> {
    let length = stream
        .read_u32_le()
        .await
        .map_err(|e| error(format!("读取模型服务控制管道失败：{e}")))? as usize;
    if length > MAX_FRAME {
        return Err(error("模型服务控制消息过大"));
    }
    let mut data = vec![0; length];
    stream
        .read_exact(&mut data)
        .await
        .map_err(|e| error(e.to_string()))?;
    serde_json::from_slice(&data).map_err(|e| error(format!("模型服务控制消息无效：{e}")))
}

#[cfg(windows)]
mod platform {
    use super::{
        error, mpsc, oneshot, receive, send, watch, CancellationToken, Control, ControlOperation,
        Operation, Response, ServiceDiscovery, ServiceError, ServiceStatus, PROTOCOL,
    };
    use sha2::{Digest, Sha256};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    use std::os::windows::io::AsRawHandle;
    use std::time::Duration;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
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
        WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
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
        if size
            < u32::try_from(std::mem::size_of::<TOKEN_USER>()).map_err(|e| error(e.to_string()))?
        {
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
            let root = crate::xberg_settings::state_dir().map_err(error)?;
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
            return Err(error(format!(
                "创建模型服务单实例锁失败：{}",
                std::io::Error::last_os_error()
            )));
        }
        let handle = Handle(raw);
        // SAFETY: handle 是 mutex；同步 facade/service 均在本线程持有和释放。
        match unsafe { WaitForSingleObject(raw, if wait { 30_000 } else { 0 }) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Some(InstanceLock { handle })),
            windows_sys::Win32::Foundation::WAIT_TIMEOUT => Ok(None),
            _ => Err(error("等待模型服务单实例锁失败")),
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
    fn ensure_same_pipe_user(pipe: HANDLE, server: bool) -> Result<u32, ServiceError> {
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
        // SAFETY: 只以最低查询权限打开内核提供的 PID，不继承、不改变进程。
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(error("无法读取模型服务对端进程身份"));
        }
        let process = Handle(process);
        if process_session(pid)? != process_session(std::process::id())? {
            return Err(error("拒绝跨 Windows 登录会话访问模型服务控制管道"));
        }
        let peer_data = token_user(process.0)?;
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
        Ok(pid)
    }
    pub(in crate::acp_api::runtime) async fn exchange<T: serde::de::DeserializeOwned>(
        operation: Operation,
    ) -> Result<Option<T>, ServiceError> {
        // 查询从首次连接到响应共用一个 budget；生命周期收尾仍不设置强制超时。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let name = pipe_name()?;
        let connected = async {
            loop {
                match ClientOptions::new().open(&name) {
                    Ok(pipe) => return Ok(Some(pipe)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.cast_signed()) => {
                        // 不阻塞 executor，不启动后台；等服务发布下一监听实例。
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(e) => return Err(error(format!("连接模型服务控制管道失败：{e}"))),
                }
            }
        };
        let Some(mut pipe) = tokio::time::timeout_at(deadline, connected)
            .await
            .map_err(|_| error("模型服务控制查询超时"))??
        else {
            return Ok(None);
        };
        // 在写入任何控制操作前核验真实端点 TokenUser 和会话；不信任名字/自报 PID。
        let actual_pid = ensure_same_pipe_user(pipe.as_raw_handle().cast(), false)?;
        let work = async {
            send(&mut pipe, &operation).await?;
            let response: Response<T> = receive(&mut pipe).await?;
            if response.protocol != PROTOCOL || response.pid != actual_pid {
                return Err(error("模型服务控制协议或进程归属不匹配"));
            }
            response.result.map(Some)
        };
        if matches!(operation, Operation::Stop | Operation::Apply) {
            work.await
        } else {
            tokio::time::timeout_at(deadline, work)
                .await
                .map_err(|_| error("模型服务控制查询超时"))?
        }
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
        .map_err(|e| error(format!("创建模型服务控制管道失败：{e}")))
    }
    pub(in crate::acp_api::runtime) async fn serve(
        status: watch::Receiver<ServiceStatus>,
        instance_id: uuid::Uuid,
        commands: mpsc::UnboundedSender<Control>,
        shutdown: CancellationToken,
        ready: oneshot::Sender<Result<(), ServiceError>>,
    ) -> Result<(), ServiceError> {
        let mut pipe = match create_pipe(true) {
            Ok(pipe) => {
                let _ = ready.send(Ok(()));
                pipe
            }
            Err(error) => {
                let _ = ready.send(Err(error.clone()));
                return Err(error);
            }
        };
        let mut workers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                result = pipe.connect() => result.map_err(|e| error(format!("接受模型服务控制连接失败：{e}")))?,
            }
            let mut connected = pipe;
            pipe = create_pipe(false)?;
            if let Err(error) = ensure_same_pipe_user(connected.as_raw_handle().cast(), true) {
                tracing::warn!(kind = ?error.kind, "拒绝非同一用户或登录会话的模型服务控制连接");
                // 尚未解析任何操作；拒绝不能触发状态发现、磁盘读取或生命周期变更。
                let _ = connected.disconnect();
                continue;
            }
            let status = status.clone();
            let commands = commands.clone();
            let shutdown = shutdown.clone();
            workers.spawn(async move {
                let operation: Operation =
                    tokio::time::timeout(Duration::from_secs(15), receive(&mut connected))
                        .await
                        .map_err(|_| error("模型服务控制请求超时"))??;
                // 收尾失败仍要退役后台；回执写入失败也不能留下已封口的控制器。
                // guard 在控制器实际完成和回执发送之后释放，不提前取消资源收尾。
                let _stop_reply_guard =
                    matches!(operation, Operation::Stop).then(|| shutdown.clone().drop_guard());
                let result = if matches!(operation, Operation::Discover) {
                    // Discover 不进入生命周期队列，也不读取磁盘 saved_config。
                    let discovery = ServiceDiscovery::from_status(instance_id, &status.borrow());
                    return send(
                        &mut connected,
                        &Response {
                            protocol: PROTOCOL,
                            pid: std::process::id(),
                            result: Ok(discovery),
                        },
                    )
                    .await;
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
                        .send(Control { operation, reply })
                        .map_err(|_| error("模型服务控制器已停止"))?;
                    answer.await.map_err(|_| error("模型服务控制器异常退出"))?
                };
                let response = Response {
                    protocol: PROTOCOL,
                    pid: std::process::id(),
                    result,
                };
                send(&mut connected, &response).await
            });
            // 回收已完成控制连接，不累积历史响应。
            while workers.try_join_next().is_some() {}
        }
        while workers.join_next().await.is_some() {}
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used, clippy::expect_used)]

        use super::*;
        use std::ffi::OsString;
        use std::path::PathBuf;
        use std::sync::MutexGuard;
        use windows_sys::Win32::Foundation::GENERIC_ALL;
        use windows_sys::Win32::Security::{
            EqualSid, GetAce, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
            ACCESS_ALLOWED_ACE,
        };
        use windows_sys::Win32::System::Pipes::{GetNamedPipeInfo, PeekNamedPipe};

        struct IsolatedRoot {
            _lock: MutexGuard<'static, ()>,
            temp: tempfile::TempDir,
            previous: Option<OsString>,
        }

        impl IsolatedRoot {
            fn new() -> Self {
                let lock = crate::asset_util::test_env::env_lock()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let temp = tempfile::tempdir().unwrap();
                let previous = std::env::var_os("JCHTOOLS_TEST_STATE_DIR");
                std::env::set_var("JCHTOOLS_TEST_STATE_DIR", temp.path().join("state"));
                Self {
                    _lock: lock,
                    temp,
                    previous,
                }
            }

            fn root(&self) -> PathBuf {
                self.temp.path().join("state")
            }
        }

        impl Drop for IsolatedRoot {
            fn drop(&mut self) {
                match self.previous.take() {
                    Some(value) => std::env::set_var("JCHTOOLS_TEST_STATE_DIR", value),
                    None => std::env::remove_var("JCHTOOLS_TEST_STATE_DIR"),
                }
            }
        }

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        }

        fn current_user() -> Vec<usize> {
            let mut token = std::ptr::null_mut();
            // SAFETY: 当前进程伪句柄无需关闭。
            let process = unsafe { GetCurrentProcess() };
            assert_ne!(
                // SAFETY: 当前进程伪句柄有效，成功返回的 token 由 Handle 释放。
                unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) },
                0
            );
            let token = Handle(token);
            let mut size = 0;
            // SAFETY: 零长查询只取得 TokenUser 所需缓冲区大小。
            unsafe {
                GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &raw mut size);
            }
            assert!(size >= u32::try_from(std::mem::size_of::<TOKEN_USER>()).unwrap());
            let mut data = vec![0_usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            assert_ne!(
                // SAFETY: 缓冲区正确对齐且至少 size 字节，token 在查询期间有效。
                unsafe {
                    GetTokenInformation(
                        token.0,
                        TokenUser,
                        data.as_mut_ptr().cast(),
                        size,
                        &raw mut size,
                    )
                },
                0
            );
            data
        }

        // 覆盖 AH-15/AH-A14：检查实际 descriptor Owner，不能以 OW 占位组冒充当前用户。
        #[test]
        fn security_descriptor_owner_is_actual_token_user() {
            let user_data = current_user();
            // SAFETY: 成功 TokenUser 查询的缓冲区正确对齐，存活至本测试结束。
            let user = unsafe { &*user_data.as_ptr().cast::<TOKEN_USER>() };
            let security = Security::new().unwrap();
            let mut owner = std::ptr::null_mut();
            let mut defaulted = 0;
            assert_ne!(
                // SAFETY: Security 持有有效 descriptor，输出指针有效。
                unsafe {
                    GetSecurityDescriptorOwner(security.0, &raw mut owner, &raw mut defaulted)
                },
                0
            );
            assert!(
                !owner.is_null(),
                "控制通道 descriptor 必须显式指定当前 TokenUser Owner"
            );
            assert_ne!(
                // SAFETY: 两个 SID 均在各自存活的系统查询缓冲区内。
                unsafe { EqualSid(owner, user.User.Sid) },
                0,
                "descriptor Owner 不是当前进程实际 TokenUser"
            );
        }

        // 覆盖 AH-15/AH-A14：解析实际 DACL 的 allow ACE，要求显式授予实际 TokenUser。
        #[test]
        fn security_descriptor_dacl_explicitly_allows_actual_token_user() {
            let user_data = current_user();
            // SAFETY: 成功 TokenUser 查询的缓冲区正确对齐并在本测试期间存活。
            let user = unsafe { &*user_data.as_ptr().cast::<TOKEN_USER>() };
            let security = Security::new().unwrap();
            let mut present = 0;
            let mut defaulted = 0;
            let mut acl = std::ptr::null_mut();
            assert_ne!(
                // SAFETY: Security 持有有效 descriptor，所有输出指针有效。
                unsafe {
                    GetSecurityDescriptorDacl(
                        security.0,
                        &raw mut present,
                        &raw mut acl,
                        &raw mut defaulted,
                    )
                },
                0
            );
            assert_ne!(present, 0, "控制通道必须有显式 DACL");
            assert!(!acl.is_null(), "NULL DACL 会向所有用户开放控制通道");
            let mut user_allowed = false;
            // SAFETY: 非 NULL DACL 来自有效 descriptor，生命周期覆盖整个遍历。
            for index in 0..u32::from(unsafe { (*acl).AceCount }) {
                let mut ace = std::ptr::null_mut();
                assert_ne!(
                    // SAFETY: index 在 DACL AceCount 内，输出指针有效。
                    unsafe { GetAce(acl, index, &raw mut ace) },
                    0
                );
                // SAFETY: GetAce 返回有效 ACE，所有 ACE 均以 ACE_HEADER 开始。
                let header = unsafe { &*ace.cast::<windows_sys::Win32::Security::ACE_HEADER>() };
                // Win32 ACCESS_ALLOWED_ACE_TYPE 为 0；其它 ACE 不构成此处的显式授权。
                if header.AceType != 0 {
                    continue;
                }
                // SAFETY: 已确认 ACCESS_ALLOWED_ACE 类型，SID 位于 SidStart。
                let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
                let sid = (&raw const allowed.SidStart).cast_mut().cast();
                // SAFETY: ACE SID 和 TokenUser SID 均在各自有效缓冲区内。
                if unsafe { EqualSid(sid, user.User.Sid) } != 0 {
                    user_allowed |= allowed.Mask & GENERIC_ALL != 0;
                }
            }
            assert!(
                user_allowed,
                "DACL 必须授予实际 TokenUser；仅 SY/OW ACE 不满足同用户授权"
            );
        }

        // 覆盖 AH-15/AH-A13：busy 不是后台缺席，Discover 等下一监听实例且不创建配置。
        #[test]
        fn busy_discover_waits_for_next_listener_with_no_state_side_effects() {
            let isolation = IsolatedRoot::new();
            runtime().block_on(async {
                // 用系统默认 ACL 隔离 busy 行为，避免产品 ACL 缺陷掩盖连接等待回归。
                let occupied = ServerOptions::new()
                    .first_pipe_instance(true)
                    .create(pipe_name().unwrap())
                    .unwrap();
                let occupying_client = ClientOptions::new().open(pipe_name().unwrap()).unwrap();
                occupied.connect().await.unwrap();
                let discovery = exchange::<ServiceDiscovery>(Operation::Discover);
                tokio::pin!(discovery);
                // 先轮询真实 exchange，再同步发布监听实例；不依赖 sleep 或线程调度。
                tokio::select! {
                    biased;
                    result = &mut discovery => panic!("已有实例 busy 时必须有界等待而非立即返回: {result:?}"),
                    () = std::future::ready(()) => {}
                }
                let mut next = ServerOptions::new().create(pipe_name().unwrap()).unwrap();
                drop(occupying_client);
                drop(occupied);
                let expected =
                    ServiceDiscovery::from_status(uuid::Uuid::new_v4(), &ServiceStatus::default());
                let reply = expected.clone();
                let responder = async {
                    next.connect().await.unwrap();
                    assert!(matches!(receive::<Operation>(&mut next).await.unwrap(), Operation::Discover));
                    send(
                        &mut next,
                        &Response {
                            protocol: PROTOCOL,
                            pid: std::process::id(),
                            result: Ok(reply),
                        },
                    )
                    .await
                    .unwrap();
                };
                let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
                    tokio::join!(&mut discovery, responder)
                })
                .await
                .expect("释放 busy 实例后 Discover 必须在有界时间内完成");
                assert_eq!(result.unwrap(), Some(expected));
                assert!(!isolation.root().exists(), "Discover 不得创建配置、工作区或启动后台");
            });
        }

        struct RunningServer {
            shutdown: CancellationToken,
            task: tokio::task::JoinHandle<Result<(), ServiceError>>,
        }

        impl Drop for RunningServer {
            fn drop(&mut self) {
                self.shutdown.cancel();
                self.task.abort();
            }
        }

        // 大 Status 跨越真实管道 quota；非读取者不能使监听取消后永久等待 worker。
        #[test]
        fn status_nonreading_client_does_not_block_listener_shutdown() {
            let _isolation = IsolatedRoot::new();
            let frame_limit = super::super::MAX_FRAME;
            let mut config = crate::acp_api::ServiceConfig {
                executable: "acp-test-agent.exe".to_owned(),
                arguments: vec![String::new()],
                ..crate::acp_api::ServiceConfig::default()
            };
            let overhead = serde_json::to_vec(&config).unwrap().len();
            config.arguments[0] = "x".repeat(frame_limit / 4 - overhead);
            crate::acp_api::settings::validate_config(&config).unwrap();
            crate::acp_api::settings::save_config(&config).unwrap();
            let original = ServiceStatus {
                phase: crate::acp_api::ServicePhase::Ready,
                saved_config: Some(config.clone()),
                running_config: Some(config),
                service_pid: Some(std::process::id()),
                ..ServiceStatus::default()
            };
            let response_len = serde_json::to_vec(&Response {
                protocol: PROTOCOL,
                pid: std::process::id(),
                result: Ok(original.clone()),
            })
            .unwrap()
            .len();
            assert!(
                response_len <= frame_limit,
                "合法配置的响应必须在 MAX_FRAME 内"
            );
            runtime().block_on(async {
                let (_state, status) = watch::channel(original);
                let (commands, _requests) = mpsc::unbounded_channel();
                let shutdown = CancellationToken::new();
                let (ready, started) = oneshot::channel();
                let mut server = RunningServer {
                    task: tokio::spawn(serve(
                        status,
                        uuid::Uuid::new_v4(),
                        commands,
                        shutdown.clone(),
                        ready,
                    )),
                    shutdown,
                };
                let mut client = None;
                let observed = tokio::time::timeout(Duration::from_secs(10), async {
                    started
                        .await
                        .map_err(|e| e.to_string())?
                        .map_err(|e| e.message)?;
                    // 使用未注册到 Tokio 的真实客户端，避免驱动主动预读缓解背压。
                    // 客户端全程不调用 Read；PeekNamedPipe 仅观察，不消费任何字节。
                    client = Some(
                        std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(pipe_name().map_err(|e| e.message)?)
                            .map_err(|e| e.to_string())?,
                    );
                    let client = client.as_mut().unwrap();
                    let mut output_quota = 0_u32;
                    let mut input_quota = 0_u32;
                    // SAFETY: 客户端持有有效管道句柄，查询输出指针在调用期间有效。
                    if unsafe {
                        GetNamedPipeInfo(
                            client.as_raw_handle().cast(),
                            std::ptr::null_mut(),
                            &raw mut output_quota,
                            &raw mut input_quota,
                            std::ptr::null_mut(),
                        )
                    } == 0
                    {
                        return Err(std::io::Error::last_os_error().to_string());
                    }
                    if response_len + 4 <= output_quota as usize {
                        return Err(format!(
                            "Status 未跨越管道写缓冲：frame={}, quota={output_quota}",
                            response_len + 4
                        ));
                    }
                    let request = serde_json::to_vec(&Operation::Status)
                        .map_err(|e| e.to_string())?;
                    let mut frame = u32::try_from(request.len())
                        .map_err(|e| e.to_string())?
                        .to_le_bytes()
                        .to_vec();
                    frame.extend_from_slice(&request);
                    if frame.len() > input_quota as usize {
                        return Err("Status 请求必须完整放入输入缓冲，不能阻塞测试 executor".to_owned());
                    }
                    std::io::Write::write_all(client, &frame).map_err(|e| e.to_string())?;
                    loop {
                        let mut header = [0_u8; 4];
                        let mut copied = 0_u32;
                        let mut available = 0_u32;
                        // SAFETY: 不消费数据；header 和所有输出指针在调用期间有效。
                        if unsafe {
                            PeekNamedPipe(
                                client.as_raw_handle().cast(),
                                header.as_mut_ptr().cast(),
                                4,
                                &raw mut copied,
                                &raw mut available,
                                std::ptr::null_mut(),
                            )
                        } == 0
                        {
                            return Err(std::io::Error::last_os_error().to_string());
                        }
                        if copied == 4 && available > 4 {
                            let actual_len = u32::from_le_bytes(header) as usize;
                            if actual_len != response_len {
                                return Err(format!(
                                    "Status 响应长度错误：actual={actual_len}, expected={response_len}"
                                ));
                            }
                            // 真实响应头及 body 已出现，证明请求被处理、写入已发起。
                            // 不把内核写 pending 等同于 worker pending：Mio 可排队整帧。
                            return Ok((available, output_quota));
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await;
                server.shutdown.cancel();
                let exit = tokio::time::timeout(Duration::from_secs(10), &mut server.task).await;
                let exited_with_client_connected = exit.is_ok();
                // 先断开非读取者，再等待/回收测试自有服务；断言不能跳过失败路径清理。
                drop(client);
                let cleanup = match exit {
                    Ok(result) => result
                        .map_err(|e| e.to_string())
                        .and_then(|result| result.map_err(|e| e.message)),
                    Err(_) => {
                        if let Ok(result) =
                            tokio::time::timeout(Duration::from_secs(10), &mut server.task).await
                        {
                            result
                                .map_err(|e| e.to_string())
                                .and_then(|result| result.map_err(|e| e.message))
                        } else {
                            server.task.abort();
                            let _ = (&mut server.task).await;
                            Err("断开客户端后测试自有服务仍未收尾，已 abort 并 await".to_owned())
                        }
                    }
                };
                assert!(cleanup.is_ok(), "测试自有服务清理失败：{cleanup:?}");
                assert!(
                    matches!(observed, Ok(Ok(_))),
                    "必须观察真实 Status 响应，不能以未处理请求的 shutdown 冒充成功：{observed:?}"
                );
                assert!(
                    exited_with_client_connected,
                    "非读取 Status 客户端仍连接时，serve 必须在 10s 看门狗内正常退出；\
                     已先断开客户端并完成清理；响应观测={observed:?}"
                );
            });
        }

        // 覆盖 AH-15/AH-A13/AH-A14：真实同用户管道 Discover 不读配置、不进入生命周期队列。
        #[test]
        fn same_user_discover_preserves_status_and_lifecycle_queue() {
            let isolation = IsolatedRoot::new();
            std::fs::create_dir(isolation.root()).unwrap();
            let saved = isolation.root().join("config.sqlite3");
            std::fs::write(&saved, b"invalid SQLite must remain untouched").unwrap();
            runtime().block_on(async {
                let original = ServiceStatus::default();
                let (state, status) = watch::channel(original.clone());
                let (commands, mut requests) = mpsc::unbounded_channel();
                let shutdown = CancellationToken::new();
                let (ready, started) = oneshot::channel();
                let instance = uuid::Uuid::new_v4();
                let mut server = RunningServer {
                    task: tokio::spawn(serve(status, instance, commands, shutdown.clone(), ready)),
                    shutdown,
                };
                started.await.unwrap().unwrap();
                let result = tokio::time::timeout(
                    Duration::from_secs(3),
                    exchange::<ServiceDiscovery>(Operation::Discover),
                )
                .await
                .expect("同用户 Discover 必须完成")
                .unwrap();
                assert_eq!(
                    result,
                    Some(ServiceDiscovery::from_status(instance, &original))
                );
                assert_eq!(*state.borrow(), original, "Discover 不得改变运行状态");
                assert!(matches!(
                    requests.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
                assert_eq!(
                    std::fs::read(&saved).unwrap(),
                    b"invalid SQLite must remain untouched"
                );
                assert!(!isolation.root().join("acp-workspace").exists());
                server.shutdown.cancel();
                tokio::time::timeout(Duration::from_secs(3), &mut server.task)
                    .await
                    .expect("测试自有控制服务必须有界收尾")
                    .unwrap()
                    .unwrap();
            });
        }

        // 覆盖 AH-15/AH-A14：内核服务 PID 优先于响应中的伪报 PID，同会话也不能绕过归属。
        #[test]
        fn discover_rejects_self_reported_pid_different_from_kernel_server_pid() {
            let isolation = IsolatedRoot::new();
            runtime().block_on(async {
                let mut server = ServerOptions::new()
                    .first_pipe_instance(true)
                    .create(pipe_name().unwrap())
                    .unwrap();
                let responder = async {
                    server.connect().await.unwrap();
                    let mut kernel_pid = 0;
                    assert_ne!(
                        // SAFETY: 已连接管道句柄有效，PID 输出指针有效。
                        unsafe {
                            GetNamedPipeServerProcessId(
                                server.as_raw_handle().cast(),
                                &raw mut kernel_pid,
                            )
                        },
                        0
                    );
                    assert_eq!(kernel_pid, std::process::id());
                    assert!(matches!(
                        receive::<Operation>(&mut server).await.unwrap(),
                        Operation::Discover
                    ));
                    let descriptor = ServiceDiscovery::from_status(
                        uuid::Uuid::new_v4(),
                        &ServiceStatus::default(),
                    );
                    let legitimate = Response {
                        protocol: PROTOCOL,
                        pid: kernel_pid,
                        result: Ok(descriptor.clone()),
                    };
                    // 先证明完整合法的响应可解码；负例仅改变自报 PID，不能靠坏 JSON 通过。
                    let mut spoof: Response<ServiceDiscovery> =
                        serde_json::from_slice(&serde_json::to_vec(&legitimate).unwrap()).unwrap();
                    assert_eq!(spoof.protocol, PROTOCOL);
                    assert_eq!(spoof.pid, kernel_pid);
                    assert_eq!(spoof.result.as_ref().unwrap(), &descriptor);
                    spoof.pid = kernel_pid.wrapping_add(1);
                    send(&mut server, &spoof).await.unwrap();
                };
                let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
                    tokio::join!(exchange::<ServiceDiscovery>(Operation::Discover), responder)
                })
                .await
                .expect("伪报 PID 响应必须有界拒绝");
                let failure = result.expect_err("同会话响应不得伪造内核服务 PID");
                assert_eq!(failure.message, "模型服务控制协议或进程归属不匹配");
                assert!(
                    !isolation.root().exists(),
                    "伪报 PID 不得触发后台启动或状态创建"
                );
            });
        }
    }
}
#[cfg(windows)]
pub(super) use platform::{exchange, lock, serve};

// 产品只支持 Windows；其余构建维度明确拒绝，不提供替代网络控制服务。
#[cfg(not(windows))]
pub(super) fn lock(_kind: &str, _wait: bool) -> Result<Option<()>, ServiceError> {
    Err(error("ACP 后台服务仅支持 Windows"))
}
#[cfg(not(windows))]
pub(super) async fn exchange<T: serde::de::DeserializeOwned>(
    _operation: Operation,
) -> Result<Option<T>, ServiceError> {
    Err(error("ACP 后台服务仅支持 Windows"))
}
#[cfg(not(windows))]
pub(super) async fn serve(
    _status: watch::Receiver<ServiceStatus>,
    _instance_id: uuid::Uuid,
    _commands: mpsc::UnboundedSender<Control>,
    _shutdown: CancellationToken,
    ready: oneshot::Sender<Result<(), ServiceError>>,
) -> Result<(), ServiceError> {
    let failure = error("ACP 后台服务仅支持 Windows");
    let _ = ready.send(Err(failure.clone()));
    Err(failure)
}
