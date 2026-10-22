#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]

//! AH-02/AH-05/AH-13/AH-A10/AH-A11：经HTTP和官方SDK触发真实文件及终端回调。
//! SDK夹具补充验证客户端能力，不等同OpenCode实际工具路径或真实模型验收。
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fixtures/acp_api/main.rs"]
mod fixture;
use fixture::{
    consumer::{audit, body, completion, release, wait_event, Http, Server},
    Plan, MODEL_A,
};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn server() -> Server {
    Server::start(env!("CARGO_BIN_EXE_jchtools-acp-fixture"), "normal")
}

#[path = "acp_callbacks/interactions.rs"]
mod interactions;
#[path = "acp_callbacks/permissions_files.rs"]
mod permissions_files;
#[path = "acp_callbacks/terminals.rs"]
mod terminals;

// 在任何断言 panic 前释放本测试自有 barrier/terminal；随后 Server 才安全收尾。
struct ReleaseMarker(PathBuf);
impl ReleaseMarker {
    fn prompt(root: &Path, plan: &Plan) -> Self {
        Self(root.join(format!("release-{}", plan.tag)))
    }
    fn terminal(workspace: &Path, plan: &Plan) -> Self {
        Self(workspace.join(format!("terminal-release-{}", plan.tag)))
    }
    fn release(&self) {
        std::fs::write(&self.0, b"release").unwrap();
    }
}
impl Drop for ReleaseMarker {
    fn drop(&mut self) {
        let _released = std::fs::write(&self.0, b"release");
    }
}

type HttpOutcome = (u16, Value);

// 无论测试正常完成或断言失败，先放行自有进程，再有界等待 HTTP 线程收尾。
struct PendingHttp {
    release: ReleaseMarker,
    receive: mpsc::Receiver<HttpOutcome>,
    job: Option<thread::JoinHandle<()>>,
}
impl PendingHttp {
    fn start(
        release: ReleaseMarker,
        port: u16,
        method: &'static str,
        route: &'static str,
        request: Option<Value>,
    ) -> Self {
        let (send, receive) = mpsc::channel();
        let job = thread::spawn(move || {
            let response = Http::open(port, method, route, request.as_ref(), None);
            let _sent = send.send((response.status, response.json()));
        });
        Self {
            release,
            receive,
            job: Some(job),
        }
    }
    fn finish(&mut self, observed: Option<HttpOutcome>) -> HttpOutcome {
        self.release.release();
        let result = observed.unwrap_or_else(|| {
            self.receive
                .recv_timeout(Duration::from_secs(10))
                .expect("放行后 HTTP 请求未在看门狗期限内收尾")
        });
        self.job.take().unwrap().join().unwrap();
        result
    }
}
impl Drop for PendingHttp {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            self.release.release();
            // 超时只用于防止 red 测试自身挂死，不强杀 Agent 或终端。
            if !matches!(
                self.receive.recv_timeout(Duration::from_secs(10)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                let _joined = job.join();
            }
        }
    }
}
