#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::MutexGuard;
use windows_sys::Win32::Foundation::GENERIC_ALL;
use windows_sys::Win32::Security::{
    EqualSid, GetAce, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, ACCESS_ALLOWED_ACE,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeInfo, PeekNamedPipe};

struct IsolatedRoot {
    _lock: MutexGuard<'static, ()>,
    temp: tempfile::TempDir,
    previous: Option<OsString>,
}

impl IsolatedRoot {
    fn new() -> Self {
        let lock = jchtools_foundation::test_support::env_lock()
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
        unsafe { GetSecurityDescriptorOwner(security.0, &raw mut owner, &raw mut defaulted) },
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
                    assert!(matches!(receive::<Request>(&mut next).await.unwrap().operation, Operation::Discover));
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
            let request = serde_json::to_vec(&Request {
                operation: Operation::Status,
                diagnostic_id: uuid::Uuid::new_v4(),
            })
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
                    GetNamedPipeServerProcessId(server.as_raw_handle().cast(), &raw mut kernel_pid)
                },
                0
            );
            assert_eq!(kernel_pid, std::process::id());
            assert!(matches!(
                receive::<Request>(&mut server).await.unwrap().operation,
                Operation::Discover
            ));
            let descriptor =
                ServiceDiscovery::from_status(uuid::Uuid::new_v4(), &ServiceStatus::default());
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
