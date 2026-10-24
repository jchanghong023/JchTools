//! AH-06～AH-17：GUI facade 与安装维护使用的模型服务客户端控制入口。
//! 查询只使用私有控制管道，绝不自动拉起服务或探测 HTTP 端口。
use super::ipc::{self, Operation};
use super::runtime;
use crate::acp_api::{
    settings, ServiceConfig, ServiceDiscovery, ServiceError, ServiceErrorKind, ServicePhase,
    ServiceStatus,
};
use std::process::Stdio;
use std::time::{Duration, Instant};

fn inactive() -> Result<ServiceStatus, ServiceError> {
    let saved_config = settings::load_config()?;
    Ok(ServiceStatus {
        phase: if saved_config.is_some() {
            ServicePhase::Stopped
        } else {
            ServicePhase::Unconfigured
        },
        saved_config,
        ..ServiceStatus::default()
    })
}
/// 查询只使用私有管道，绝不自动拉起服务或探测 HTTP 端口。
pub fn status() -> Result<ServiceStatus, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "status", || {
        runtime()?
            .block_on(ipc::exchange(Operation::Status))?
            .map_or_else(inactive, Ok)
    })
}
/// AH-15：只查询已有后台；缺席时不读取配置，也不启动后台或 Agent。
pub fn discover() -> Result<Option<ServiceDiscovery>, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "discover", || {
        let discovery: Option<ServiceDiscovery> =
            runtime()?.block_on(ipc::exchange(Operation::Discover))?;
        // 退休后台仅为维护保留 OS 退出观察入口，不重新公布为可连接服务。
        Ok(discovery.filter(|discovery| discovery.phase != ServicePhase::Stopped))
    })
}
pub fn ensure_started() -> Result<ServiceStatus, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "ensure_started", || {
        let rt = runtime()?;
        if let Some(state) = rt.block_on(ipc::exchange(Operation::Status))? {
            return Ok(state);
        }
        let config = settings::load_config()?;
        if config.is_none() {
            return inactive();
        }
        // 同一用户/会话所有 GUI 使用独立启动锁；服务另持 lifetime lock。
        let _startup = ipc::lock("startup", true)?.ok_or_else(|| {
            tracing::error!(
                event = "acp_service_start_failed",
                component = "acp_runtime",
                stage = "startup_lock_timeout",
                error_type = "timeout",
                timeout_ms = 30_000,
                "等待 ACP 模型服务启动锁超时"
            );
            ipc::error("等待模型服务启动锁超时")
        })?;
        if let Some(state) = rt.block_on(ipc::exchange(Operation::Status))? {
            return Ok(state);
        }
        let mut executable = std::env::current_exe().map_err(|e| {
            tracing::error!(
                event = "acp_service_start_failed",
                component = "acp_runtime",
                stage = "locate_executable",
                error_type = ?e.kind(),
                error_code = ?e.raw_os_error(),
                "定位 ACP 模型服务程序失败"
            );
            ipc::error(format!("定位 JchTools 程序失败：{e}"))
        })?;
        if cfg!(any(test, feature = "test-hooks")) {
            if let Some(path) = std::env::var_os("JCHTOOLS_TEST_ACP_SERVICE_EXE") {
                executable = std::path::PathBuf::from(path);
                if !executable.is_absolute() || !executable.is_file() {
                    return Err(ipc::error("测试服务程序必须为存在的绝对路径"));
                }
            }
        }
        let mut command = std::process::Command::new(executable);
        command
            .arg("--acp-http-service")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let started = Instant::now();
        tracing::info!(
            event = "acp_service_spawn_started",
            component = "acp_runtime",
            executable_role = "jchtools_service",
            timeout_ms = 15000,
            "ACP 模型服务后台启动开始"
        );
        let mut service = command
    .spawn()
    .map_err(|e| {
        tracing::error!(event = "acp_service_spawn_failed", component = "acp_runtime", stage = "spawn", error_type = ?e.kind(), error_code = ?e.raw_os_error(), elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "ACP 模型服务后台启动失败");
        ipc::error(format!("启动模型服务后台失败：{e}"))
    })?;
        tracing::info!(
            event = "acp_service_spawn_completed",
            component = "acp_runtime",
            peer_pid = service.id(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "ACP 模型服务后台子进程已启动"
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(state) = rt.block_on(ipc::exchange::<ServiceStatus>(Operation::Status))? {
                tracing::info!(event = "acp_service_control_ready", component = "acp_runtime", peer_pid = service.id(), phase = ?state.phase, elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "ACP 模型服务后台控制管道已就绪");
                return Ok(state);
            }
            if let Some(exit) = service.try_wait().map_err(|e| {
                tracing::error!(
                    event = "acp_service_start_failed",
                    component = "acp_runtime",
                    peer_pid = service.id(),
                    stage = "poll_child_exit",
                    error_type = ?e.kind(),
                    error_code = ?e.raw_os_error(),
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                    "检查 ACP 模型服务后台进程失败"
                );
                ipc::error(e.to_string())
            })? {
                tracing::error!(event = "acp_service_start_failed", component = "acp_runtime", peer_pid = service.id(), stage = "child_exit", exit_code = ?exit.code(), elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "ACP 模型服务后台启动期间退出");
                return Err(ipc::error(format!("模型服务后台启动时退出：{exit}")));
            }
            if Instant::now() >= deadline {
                tracing::error!(
                    event = "acp_service_start_failed",
                    component = "acp_runtime",
                    peer_pid = service.id(),
                    stage = "control_ready_timeout",
                    timeout_ms = 15000,
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                    "ACP 模型服务后台控制管道就绪超时"
                );
                return Err(ipc::error("模型服务后台未及时建立控制管道"));
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    })
}
pub fn save_config(config: &ServiceConfig) -> Result<ServiceStatus, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "save_config", || {
        settings::save_config(config)?;
        if let Some(state) = runtime()?.block_on(ipc::exchange(Operation::Status))? {
            return Ok(state);
        }
        ensure_started()
    })
}
pub fn apply_and_restart() -> Result<ServiceStatus, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "apply_and_restart", || {
        // Apply 只作用已存在的后台；退出后的迟到请求不能反向创建新实例。
        runtime()?
            .block_on(ipc::exchange(Operation::Apply))?
            .ok_or_else(|| {
                ServiceError::new(
                    ServiceErrorKind::NotReady,
                    "模型服务后台已退出，无法应用配置",
                )
            })
    })
}
pub fn stop() -> Result<ServiceStatus, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "stop", || {
        runtime()?
            .block_on(ipc::exchange(Operation::Stop))?
            .map_or_else(inactive, Ok)
    })
}

/// AH-17：安装维护只控制已有后台，不读取 SQLite，也不启动服务或 Agent。
/// Stop 回执之后仍等原后台的 OS 进程对象释放，不能把 ACK 当作 EXE 已可替换。
pub fn stop_for_maintenance() -> Result<(), ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "maintenance_stop", || {
        let _startup =
            ipc::lock("startup", true)?.ok_or_else(|| ipc::error("等待模型服务维护启动锁超时"))?;
        #[cfg(windows)]
        {
            let rt = runtime()?;
            // Discover 直接投影内存状态；认证连接时就保留 OS 对象，不读 SQLite。
            let Some((_discovery, peer)) = rt.block_on(ipc::exchange_with_peer::<
                ServiceDiscovery,
            >(Operation::Discover, None))?
            else {
                let _instance = ipc::lock("service", false)?
                    .ok_or_else(|| ipc::error("模型服务仍占用运行锁且控制管道不可用"))?;
                tracing::info!(
                    event = "acp_maintenance_stop_completed",
                    component = "acp_runtime",
                    outcome = "already_absent",
                    "ACP 安装维护确认后台不存在"
                );
                return Ok(());
            };
            let pid = peer.pid();
            tracing::info!(
                event = "acp_maintenance_stop_started",
                component = "acp_runtime",
                peer_pid = pid,
                "ACP 安装维护请求安全停止已有后台"
            );
            // GUI Stop 可并发；只允许对仍由原 OS 对象占据的身份发送控制。
            // 控制错误直接非零；监听已消失时仍只等待已经认证并持有的原对象。
            let stopped = rt.block_on(ipc::exchange_with_peer::<ServiceStatus>(
                Operation::Stop,
                Some(&peer),
            ))?;
            if let Some((state, _stop_peer)) = stopped {
                if state.phase != ServicePhase::Stopped {
                    return Err(ipc::error("模型服务维护停止未完成安全收尾"));
                }
            }
            // 安全 Stop ACK 不是 OS 退出；无限等待不授予强杀权限。
            peer.wait_for_exit()?;
            let _instance = ipc::lock("service", false)?
                .ok_or_else(|| ipc::error("模型服务维护后仍有后台占用运行锁"))?;
            tracing::info!(
                event = "acp_maintenance_stop_completed",
                component = "acp_runtime",
                peer_pid = pid,
                outcome = "process_exited",
                "ACP 安装维护确认后台和自有资源已安全释放"
            );
            Ok(())
        }
        #[cfg(not(windows))]
        {
            Err(ipc::error("ACP 后台服务仅支持 Windows"))
        }
    })
}
