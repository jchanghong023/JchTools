//! AH-06～AH-10：独立后台装配；GUI facade 只在既有 worker 上调用。
mod client;
mod ipc;
mod process;
mod service;

pub use client::{
    apply_and_restart, discover, ensure_started, save_config, status, stop, stop_for_maintenance,
};
pub use service::serve;

use crate::acp_api::ServiceError;

#[cfg(all(test, windows))]
use crate::acp_api::{settings, ServiceConfig, ServiceErrorKind, ServicePhase, ServiceStatus};
#[cfg(all(test, windows))]
use ipc::Operation;
#[cfg(all(test, windows))]
use service::{control_loop, Running};
#[cfg(all(test, windows))]
use std::time::Duration;
#[cfg(all(test, windows))]
use tokio::sync::{mpsc, oneshot, watch};
#[cfg(all(test, windows))]
use tokio_util::sync::CancellationToken;

fn runtime() -> Result<tokio::runtime::Runtime, ServiceError> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| {
            tracing::error!(
                event = "acp_runtime_build_failed",
                component = "acp_runtime",
                stage = "build",
                error_type = ?e.kind(),
                error_code = ?e.raw_os_error(),
                "创建 ACP 模型服务运行时失败"
            );
            ipc::error(format!("创建模型服务运行时失败：{e}"))
        })
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
#[cfg(all(test, windows))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::acp_api::{backend_channel, BackendCommand};
    use std::ffi::OsString;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, MutexGuard,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct IsolatedRoot {
        _lock: MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
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
                _temp: temp,
                previous,
            }
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

    fn instance_available_from_other_thread() -> bool {
        // Windows mutexes are recursive on the owning thread; another thread must probe.
        std::thread::spawn(|| ipc::lock("service", false).unwrap().is_some())
            .join()
            .unwrap()
    }

    // Stop must retire the controller/pipe even when its error response is preserved.
    // Only the backend boundary is controlled: Running::finish, the process owner,
    // HTTP listener, controller and authenticated named-pipe transport are real.
    #[test]
    fn stop_backend_error_releases_controller_pipe_and_instance_after_resources_finish() {
        let _isolation = IsolatedRoot::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                    .await
                    .unwrap();
                let address = listener.local_addr().unwrap();
                let config = ServiceConfig {
                    executable: std::env::var("ComSpec").expect("Windows command interpreter"),
                    arguments: vec!["/D".into(), "/C".into(), "exit 0".into()],
                    port: address.port(),
                };
                let (state, status) = watch::channel(ServiceStatus {
                    phase: ServicePhase::Ready,
                    running_config: Some(config.clone()),
                    service_pid: Some(std::process::id()),
                    ..ServiceStatus::default()
                });
                let process = process::ProcessOwner::start(
                    config,
                    settings::workspace_dir().unwrap(),
                    state.clone(),
                );
                let connection = tokio::time::timeout(
                    Duration::from_secs(3),
                    process.handle.connect(),
                )
                .await
                .expect("test-owned child must connect within the bound")
                .unwrap();
                let crate::acp_api::AgentConnection {
                    transport,
                    mut exit,
                    finished,
                    ..
                } = connection;
                drop(transport);
                // Natural exit may win the owner's select before this acknowledgement.
                let _ = finished.send(Ok(()));
                let exit_status = tokio::time::timeout(
                    Duration::from_secs(3),
                    exit.wait_for(Option::is_some),
                )
                .await
                .expect("test-owned child must exit naturally")
                .unwrap()
                .clone()
                .unwrap();
                assert_eq!(exit_status.code, Some(0));
                assert_eq!(exit_status.error, None);
                assert_eq!(state.borrow().agent_pid, None);

                let (backend, mut backend_commands) = backend_channel(status.clone());
                let backend_boundary = tokio::spawn(async move {
                    match backend_commands.recv().await {
                        Some(BackendCommand::Stop { reply }) => {
                            // The real BackendHandle maps a closed acknowledgement to
                            // AgentDisconnected; no product transition/result is mocked.
                            drop(reply);
                        }
                        command => panic!("Stop must reach the actual backend boundary: {command:?}"),
                    }
                });
                let http_shutdown = CancellationToken::new();
                let http_finished = Arc::new(AtomicBool::new(false));
                let completed = http_finished.clone();
                let shutdown = http_shutdown.clone();
                let http_backend = backend.clone();
                let http = tokio::spawn(async move {
                    let result = crate::acp_api::http::serve(
                        listener,
                        crate::acp_api::http::router(http_backend),
                        shutdown,
                    )
                    .await;
                    completed.store(true, Ordering::SeqCst);
                    result
                });
                // Prove the real HTTP task is serving, without issuing a backend command.
                let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
                socket
                    .write_all(b"GET /test-owned-missing-route HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                let mut response = Vec::new();
                tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut response))
                    .await
                    .expect("real HTTP listener must answer")
                    .unwrap();
                assert!(response.starts_with(b"HTTP/1.1 404"));
                drop(socket);

                let running = Running {
                    backend,
                    process,
                    http_shutdown,
                    http,
                };
                let shutdown = CancellationToken::new();
                let controller_finished = Arc::new(AtomicBool::new(false));
                let completed = controller_finished.clone();
                let (done, mut finished) = oneshot::channel();
                let (published, listening) = oneshot::channel();
                let service_shutdown = shutdown.clone();
                let service = async {
                    let instance = ipc::lock("service", false).unwrap().unwrap();
                    let (commands, requests) = mpsc::unbounded_channel();
                    let (ready, started) = oneshot::channel();
                    let ipc_task = tokio::spawn(ipc::serve(
                        status,
                        uuid::Uuid::new_v4(),
                        commands,
                        service_shutdown,
                        ready,
                    ));
                    started.await.unwrap().unwrap();
                    let controller = tokio::spawn(async move {
                        control_loop(Some(running), state.clone(), requests).await;
                        completed.store(true, Ordering::SeqCst);
                    });
                    published.send(()).unwrap();
                    ipc_task.await.unwrap().unwrap();
                    controller.await.unwrap();
                    drop(instance);
                    let _ = done.send(());
                };
                let exercise = async {
                    listening.await.unwrap();
                    let failure = tokio::time::timeout(
                        Duration::from_secs(3),
                        ipc::exchange::<ServiceStatus>(Operation::Stop),
                    )
                    .await
                    .expect("Stop error must be returned after actual resource cleanup")
                    .expect_err("Stop must explicitly report the typed backend failure");
                    assert_eq!(failure.kind, ServiceErrorKind::AgentDisconnected);
                    assert!(http_finished.load(Ordering::SeqCst), "HTTP task must really finish");
                    let reclaimed_listener = tokio::net::TcpListener::bind(address).await.unwrap();
                    drop(reclaimed_listener);
                    backend_boundary.await.unwrap();

                    let natural_completion = matches!(
                        tokio::time::timeout(Duration::from_millis(500), &mut finished).await,
                        Ok(Ok(()))
                    );
                    let controller_retired = controller_finished.load(Ordering::SeqCst);
                    let pipe_absent = tokio::time::timeout(
                        Duration::from_secs(3),
                        ipc::exchange::<crate::acp_api::ServiceDiscovery>(Operation::Discover),
                    )
                    .await
                    .expect("post-Stop pipe probe must be bounded")
                    .unwrap()
                    .is_none();
                    let instance_released = instance_available_from_other_thread();
                    // Always release test-owned IPC/controller before asserting the red result.
                    shutdown.cancel();
                    if !natural_completion {
                        tokio::time::timeout(Duration::from_secs(3), &mut finished)
                            .await
                            .expect("test-owned service cleanup must be bounded")
                            .unwrap();
                    }
                    assert!(instance_available_from_other_thread());
                    assert!(ipc::exchange::<ServiceStatus>(Operation::Status).await.unwrap().is_none());

                    // Recreate the same lifetime lock and first pipe instance after cleanup.
                    let instance = ipc::lock("service", false).unwrap().unwrap();
                    let (new_state, new_status) = watch::channel(ServiceStatus::default());
                    let (commands, requests) = mpsc::unbounded_channel();
                    let (ready, started) = oneshot::channel();
                    let replacement = tokio::spawn(ipc::serve(
                        new_status,
                        uuid::Uuid::new_v4(),
                        commands,
                        CancellationToken::new(),
                        ready,
                    ));
                    started.await.unwrap().unwrap();
                    let controller = tokio::spawn(control_loop(None, new_state, requests));
                    let stopped = tokio::time::timeout(
                        Duration::from_secs(3),
                        ipc::exchange::<ServiceStatus>(Operation::Stop),
                    )
                    .await
                    .expect("replacement Stop must be bounded")
                    .unwrap()
                    .unwrap();
                    assert_eq!(stopped.phase, ServicePhase::Stopped);
                    tokio::time::timeout(Duration::from_secs(3), async {
                        replacement.await.unwrap().unwrap();
                        controller.await.unwrap();
                    })
                    .await
                    .expect("replacement service must naturally retire");
                    drop(instance);
                    assert!(
                        natural_completion && controller_retired && pipe_absent && instance_released,
                        "Stop Err must preserve its report AND retire after resource cleanup: \
                         service_completed={natural_completion}, controller_retired={controller_retired}, \
                         pipe_absent={pipe_absent}, instance_released={instance_released}"
                    );
                };
                tokio::time::timeout(Duration::from_secs(12), async {
                    tokio::join!(service, exercise);
                })
                .await
                .expect("Stop-error regression must have bounded cleanup");
            });
    }
}
