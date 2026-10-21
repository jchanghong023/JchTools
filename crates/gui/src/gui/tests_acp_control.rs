//! ACP 控制管道真实双端用例（自 gui.rs 的 gui_tests 拆分）。
#![warn(
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
use super::gui_tests::*;
use super::*;

#[cfg(windows)]
pub(super) mod acp_control_test {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::ffi::OsString;
    use std::fmt::Write;
    use std::sync::atomic::AtomicBool;
    use std::sync::MutexGuard;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    // 不改变产品控制协议；测试监听同用户/会话、独立 state root 派生的私有管道。
    fn pipe_name() -> String {
        use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
        use windows_sys::Win32::Security::{
            GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        #[link(name = "kernel32")]
        extern "system" {
            fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
        }
        let mut token = std::ptr::null_mut();
        // SAFETY: 当前进程伪句柄无需输入或释放，在当前进程中有效。
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: 当前进程伪句柄有效，token 为可写输出指针。
        let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) };
        assert_ne!(opened, 0);
        let mut size = 0;
        // SAFETY: 零长查询只取得 TOKEN_USER 所需空间。
        unsafe {
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &raw mut size);
        }
        let word = std::mem::size_of::<usize>();
        let mut storage = vec![0_usize; usize::try_from(size).unwrap().div_ceil(word)];
        // SAFETY: storage 按 usize 对齐且至少 size 字节，token 为成功打开的句柄。
        let received = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                storage.as_mut_ptr().cast(),
                size,
                &raw mut size,
            )
        };
        // SAFETY: 关闭本函数唯一拥有的真实 token 句柄。
        unsafe { CloseHandle(token) };
        assert_ne!(received, 0);
        // SAFETY: 成功的 TOKEN_USER 查询返回正确对齐、仍存活的完整结构。
        let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
        let mut sid = std::ptr::null_mut();
        // SAFETY: SID 属于仍存活的 storage；转换函数写入其分配的 UTF-16 字符串。
        let converted = unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut sid) };
        assert_ne!(converted, 0);
        let mut length = 0;
        loop {
            // SAFETY: 转换成功后 sid 指向以零结尾的字符串，length 未越过终止符。
            let character = unsafe { sid.add(length) };
            // SAFETY: character 位于尚未释放的转换结果中，包含可读 UTF-16 码元。
            if unsafe { *character } == 0 {
                break;
            }
            length += 1;
        }
        // SAFETY: 前述扫描确定了有效字符串长度，分配尚未释放。
        let user = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid, length) });
        // SAFETY: 转换函数的输出使用 LocalFree 释放一次。
        unsafe { LocalFree(sid.cast()) };
        let mut session = 0;
        // SAFETY: 当前 PID 有效，session 为可写输出指针。
        let queried = unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) };
        assert_ne!(queried, 0);
        let root = jchtools_foundation::xberg_settings::state_dir().unwrap();
        let digest = Sha256::digest(root.as_os_str().to_string_lossy().as_bytes());
        let mut suffix = String::with_capacity(digest.len() * 2);
        for byte in digest {
            write!(&mut suffix, "{byte:02x}").unwrap();
        }
        format!(r"\\.\pipe\jchtools-acp-http-{user}-{session}-{suffix}")
    }

    struct TestStateRoot {
        previous: Option<OsString>,
        _root: tempfile::TempDir,
        _lock: MutexGuard<'static, ()>,
    }
    impl TestStateRoot {
        fn new() -> Self {
            let lock = jchtools_foundation::test_support::env_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let root = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("JCHTOOLS_TEST_STATE_DIR");
            std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.path().join("state"));
            Self {
                previous,
                _root: root,
                _lock: lock,
            }
        }
    }
    impl Drop for TestStateRoot {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(root) => std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root),
                None => std::env::remove_var("JCHTOOLS_TEST_STATE_DIR"),
            }
        }
    }

    #[derive(Clone)]
    struct ControlServerState {
        state: Arc<Mutex<ServiceStatus>>,
        operations: Arc<Mutex<Vec<String>>>,
        hold_queries: Arc<AtomicBool>,
        release_queries: CancellationToken,
        stopped: CancellationToken,
        shutdown: CancellationToken,
        apply_finished: Arc<AtomicBool>,
    }

    pub(crate) struct ControlStub {
        server: ControlServerState,
        done: mpsc::Receiver<()>,
        worker: Option<std::thread::JoinHandle<()>>,
        ui_state: Rc<RefCell<State>>,
        _environment: TestStateRoot,
    }

    impl ControlStub {
        pub(crate) fn new(app: &GuiTestApp) -> Self {
            let environment = TestStateRoot::new();
            let running = ServiceConfig {
                executable: std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                arguments: Vec::new(),
                port: 8765,
            };
            let state = Arc::new(Mutex::new(ServiceStatus {
                phase: jchtools_acp_api::acp_api::ServicePhase::Ready,
                saved_config: Some(ServiceConfig {
                    port: 8766,
                    ..running.clone()
                }),
                running_config: Some(running),
                service_pid: Some(std::process::id()),
                executing: 1,
                ..ServiceStatus::default()
            }));
            let operations = Arc::new(Mutex::new(Vec::new()));
            let hold_queries = Arc::new(AtomicBool::new(false));
            let release_queries = CancellationToken::new();
            let stopped = CancellationToken::new();
            let shutdown = CancellationToken::new();
            let apply_finished = Arc::new(AtomicBool::new(false));
            let (ready_tx, ready_rx) = mpsc::channel();
            let (done_tx, done) = mpsc::channel();
            let name = pipe_name();
            let server = ControlServerState {
                state,
                operations,
                hold_queries,
                release_queries,
                stopped,
                shutdown,
                apply_finished,
            };
            let worker = {
                let server = server.clone();
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    runtime.block_on(async move {
                        let mut listener = ServerOptions::new()
                            .first_pipe_instance(true)
                            .reject_remote_clients(true)
                            .create(&name)
                            .unwrap();
                        ready_tx.send(()).unwrap();
                        let mut workers = tokio::task::JoinSet::new();
                        loop {
                            tokio::select! {
                                () = server.shutdown.cancelled() => break,
                                connected = listener.connect() => connected.unwrap(),
                            }
                            let pipe = listener;
                            listener = ServerOptions::new()
                                .reject_remote_clients(true)
                                .create(&name)
                                .unwrap();
                            let server = server.clone();
                            workers.spawn(async move { consume(pipe, &server).await });
                            while workers.try_join_next().is_some() {}
                        }
                        while workers.join_next().await.is_some() {}
                    });
                    let _ = done_tx.send(());
                })
            };
            let fixture = Self {
                server,
                done,
                worker: Some(worker),
                ui_state: app.state.clone(),
                _environment: environment,
            };
            ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            fixture
        }

        pub(crate) fn status(&self) -> ServiceStatus {
            self.server.state.blocking_lock().clone()
        }
        pub(crate) fn count(&self, operation: &str) -> usize {
            self.server
                .operations
                .blocking_lock()
                .iter()
                .filter(|seen| *seen == operation)
                .count()
        }
        pub(crate) fn apply_finished(&self) -> bool {
            self.server.apply_finished.load(Ordering::Acquire)
        }
        pub(crate) fn hold_status_queries(&self) {
            self.server.hold_queries.store(true, Ordering::Release);
        }
        pub(crate) fn release_status_queries(&self) {
            self.server.release_queries.cancel();
        }
        pub(crate) fn wait_for(app: &GuiTestApp, done: impl Fn() -> bool) -> bool {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                apply_acp_messages(&app.ui, &app.state);
                if done() {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        pub(crate) fn finish(&mut self, app: &GuiTestApp) -> bool {
            // 不经产品 Stop 做兜底，否则会掩盖真实 GUI 没有派发 Stop 的回归。
            app.state.borrow_mut().acp_observer.take();
            self.server.stopped.cancel();
            self.server.release_queries.cancel();
            let queued_stop_consumed = self.wait_for_queued_stop();
            let completed = Self::wait_for(app, || !app.ui.get_acp_request_pending());
            self.server.shutdown.cancel();
            let joined = self.join_worker();
            queued_stop_consumed && completed && joined
        }
        fn wait_for_queued_stop(&self) -> bool {
            if !self.ui_state.borrow().acp_explicit_stopped {
                return true;
            }
            // 门禁修复而控制线程尚未修复时，Stop 可能仍排在 Apply 后。
            // 兜底取消释放 Apply 后，先让这条已有请求进入隔离端点，再恢复环境。
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.count("Stop") == 0 {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            true
        }
        fn join_worker(&mut self) -> bool {
            if self.worker.is_none() {
                return true;
            }
            if self.done.recv_timeout(Duration::from_secs(3)).is_err()
                && !self
                    .worker
                    .as_ref()
                    .is_some_and(std::thread::JoinHandle::is_finished)
            {
                return false;
            }
            self.worker
                .take()
                .is_some_and(|worker| worker.join().is_ok())
        }
    }

    impl Drop for ControlStub {
        fn drop(&mut self) {
            self.ui_state.borrow_mut().acp_observer.take();
            self.server.stopped.cancel();
            self.server.release_queries.cancel();
            let _ = self.wait_for_queued_stop();
            self.server.shutdown.cancel();
            let _ = self.join_worker();
        }
    }

    async fn consume(
        mut pipe: NamedPipeServer,
        server: &ControlServerState,
    ) -> std::io::Result<()> {
        let ControlServerState {
            state,
            operations,
            hold_queries,
            release_queries,
            stopped,
            shutdown,
            apply_finished,
        } = server;
        let operation = tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            operation = async {
                let length = pipe.read_u32_le().await?;
                if usize::try_from(length).unwrap() > acp_settings::CONTROL_FRAME_LIMIT {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "超长控制帧"));
                }
                let mut bytes = vec![0; usize::try_from(length).unwrap()];
                pipe.read_exact(&mut bytes).await?;
                let request: serde_json::Value = serde_json::from_slice(&bytes)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
                request["operation"].as_str().map(str::to_owned)
                    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "缺少控制操作"))
            } => operation?,
        };
        let result: std::result::Result<ServiceStatus, jchtools_acp_api::acp_api::ServiceError> =
            match operation.as_str() {
                "Apply" => {
                    state.lock().await.phase = jchtools_acp_api::acp_api::ServicePhase::Draining;
                    operations.lock().await.push(operation.clone());
                    // 真实 IPC 回复保持悬挂：串行 observer 无法在这里偷跑下一条 Stop。
                    tokio::select! {
                        () = stopped.cancelled() => {},
                        () = shutdown.cancelled() => {},
                    }
                    apply_finished.store(true, Ordering::Release);
                    Err(jchtools_acp_api::acp_api::ServiceError::new(
                        jchtools_acp_api::acp_api::ServiceErrorKind::Stopping,
                        "隔离控制端：Stop 已取消在途 Apply",
                    ))
                }
                "Stop" => {
                    let status = {
                        let mut status = state.lock().await;
                        status.phase = jchtools_acp_api::acp_api::ServicePhase::Stopped;
                        status.running_config = None;
                        status.service_pid = None;
                        status.agent_pid = None;
                        status.executing = 0;
                        status.waiting = 0;
                        status.clone()
                    };
                    stopped.cancel();
                    operations.lock().await.push(operation.clone());
                    Ok(status)
                }
                "Status" => {
                    operations.lock().await.push(operation.clone());
                    if hold_queries.load(Ordering::Acquire) {
                        tokio::select! {
                            () = release_queries.cancelled() => {},
                            () = shutdown.cancelled() => {},
                        }
                    }
                    Ok(state.lock().await.clone())
                }
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        operation,
                    ))
                }
            };
        let response = serde_json::to_vec(&serde_json::json!({
            "protocol": 1,
            "pid": std::process::id(),
            "result": result,
        }))?;
        pipe.write_u32_le(u32::try_from(response.len()).unwrap())
            .await?;
        pipe.write_all(&response).await?;
        pipe.flush().await
    }
}
