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

//! AH-03/AH-04/AH-05/AH-12/AH-13：真实 TCP → Axum → 产品适配 → 官方 SDK Agent。
//! 这些是合成协议测试，不声称 AH-A03～AH-A09 的真实 OpenCode 验收通过。
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fixtures/acp_api/main.rs"]
mod fixture;
use fixture::{
    consumer::{audit, body, completion, release, wait_event, Http, Server},
    Plan, MODEL_A, MODEL_B, MODEL_C,
};
use serde_json::{json, Value};
use std::{
    thread,
    time::{Duration, Instant},
};

fn server(mode: &str) -> Server {
    Server::start(env!("CARGO_BIN_EXE_jchtools-acp-fixture"), mode)
}
fn wait_counts(server: &Server, executing: usize, waiting: usize) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = server.backend.status();
        if status.executing == executing && status.waiting == waiting {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "容量未到 {executing}+{waiting}: {status:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[path = "acp_http_contract/capacity.rs"]
mod capacity;
#[path = "acp_http_contract/models.rs"]
mod models;
#[path = "acp_http_contract/prompts.rs"]
mod prompts;
#[path = "acp_http_contract/sessions.rs"]
mod sessions;

fn continuation(first: &Plan, second: &Plan, model: &str, cwd: &std::path::Path) -> Value {
    json!({"model":model,"cwd":cwd,"messages":[
        {"role":"user","content":first.prompt()},
        {"role":"assistant","content":first.text()},
        {"role":"user","content":second.prompt()}
    ]})
}
