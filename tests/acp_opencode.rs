//! AH-02～AH-04/AH-06～AH-10/AH-12：真实公开 HTTP → 独立 JchTools 后台 → 官方 SDK → OpenCode。
//! 本阶段只验 Models、基本文本 JSON/SSE 和单 Agent 单并发；不代表 GUI、工具或并行验收通过。
//! OpenCode 版本按实际运行记录；截图 v2.0.26 仅为本次基准，不限定产品版本。
//! 入口：cargo test --features test-hooks --test acp_opencode -- --ignored --nocapture
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fixtures/acp_opencode.rs"]
mod support;

use jchtools::acp_api::ServicePhase;
use serde_json::json;
use support::{Harness, OwnedProcess, MODEL};

#[test]
#[ignore = "需要真实 OpenCode、Zen MiMo-V2.6-Flash Free 及当次显式模型验收授权"]
fn real_opencode_models_json_and_incremental_sse_single_agent() {
    if support::child_operation() {
        return;
    }
    let harness = Harness::new();
    let version = harness.version();
    println!(
        "OpenCode 实际版本：{version}；命令：{} acp；模型：{MODEL}",
        harness.executable.display()
    );
    let saved = harness.call(
        "save",
        Some(&serde_json::to_value(harness.config()).unwrap()),
    );
    assert!(saved.get("ok").is_some(), "保存/启动真实后台失败：{saved}");
    let ready = harness.ready();
    let service_pid = ready.service_pid.expect("真实后台未发布 PID");
    let agent_pid = ready.agent_pid.expect("真实 OpenCode 未发布 PID");
    assert_ne!(service_pid, agent_pid);
    assert_ne!(service_pid, std::process::id());
    let service = OwnedProcess::open(service_pid);
    let agent = OwnedProcess::open(agent_pid);
    harness.observe(service_pid, agent_pid);
    let connected = harness.call("ensure", None);
    assert_eq!(connected["ok"]["service_pid"], service_pid);
    assert_eq!(connected["ok"]["agent_pid"], agent_pid);

    let models = harness.request("models", None, service_pid, agent_pid);
    assert_eq!(models["body"]["object"], "list");
    assert!(
        models["body"]["data"]
            .as_array()
            .expect("/v1/models 缺少模型数组")
            .iter()
            .any(|item| item["id"] == MODEL),
        "真实 ACP 协商列表没有指定免费模型 {MODEL}；不能改选其他/付费模型：{models}"
    );
    let json_marker = format!("JCH_JSON_{}", uuid::Uuid::new_v4().simple());
    let request = json!({"model": MODEL, "stream": false, "messages": [{"role":"user", "content": format!("This is a basic text-only connectivity check. Do not use tools, inspect files, or run commands. Reply with exactly this short text: {json_marker}")}]});
    let completion = harness.request("json", Some(&request), service_pid, agent_pid);
    assert_eq!(completion["body"]["object"], "chat.completion");
    assert_eq!(completion["body"]["model"], MODEL);
    assert_eq!(
        completion["body"]["choices"][0]["message"]["role"],
        "assistant"
    );
    assert_eq!(completion["body"]["choices"][0]["finish_reason"], "stop");
    let text = completion["body"]["choices"][0]["message"]["content"]
        .as_str()
        .expect("JSON 缺少实际文本");
    assert!(
        text.contains(&json_marker),
        "非流式结果没有本请求随机标记：{completion}"
    );
    println!("JSON 实际结果（service={service_pid}, agent={agent_pid}）：{text}");
    harness.observe(service_pid, agent_pid);

    let sse_marker = format!("JCH_SSE_{}", uuid::Uuid::new_v4().simple());
    let request = json!({"model": MODEL, "stream": true, "messages": [{"role":"user", "content": format!("This is a basic text-only streaming check. Do not use tools, inspect files, or run commands. Write exactly 24 short numbered lines, numbered 1 through 24. Each line must contain this marker verbatim: {sse_marker}. Do not add an introduction or a conclusion.")}]});
    let streamed = harness.request("sse", Some(&request), service_pid, agent_pid);
    let text = streamed["text"].as_str().expect("SSE 缺少实际拼接文本");
    assert!(
        text.contains(&sse_marker),
        "SSE 结果没有本请求随机标记：{streamed}"
    );
    assert!(!text.contains(&json_marker), "SSE 串入前一请求文本");
    assert!(
        streamed["content_deltas"].as_u64().unwrap() >= 2,
        "SSE 只有一个全文块，不构成已观察的增量：{streamed}"
    );
    assert_eq!(streamed["done"], true);
    assert_eq!(streamed["finish_reason"], "stop");
    assert_eq!(streamed["first_delta_status"]["service_pid"], service_pid);
    assert_eq!(streamed["first_delta_status"]["agent_pid"], agent_pid);
    assert_eq!(
        streamed["first_delta_status"]["executing"], 1,
        "首次文本增量必须在 Agent 请求完成前可观察：{streamed}"
    );
    assert!(streamed["first_delta_ms"].as_u64().unwrap() <= streamed["done_ms"].as_u64().unwrap());
    println!(
        "SSE 实际结果（service={service_pid}, agent={agent_pid}，{} 增量）：\n{text}",
        streamed["content_deltas"]
    );
    harness.observe(service_pid, agent_pid);

    let stopped = harness.call("stop", None);
    assert!(stopped.get("ok").is_some(), "安全 Stop 失败：{stopped}");
    service.wait_gone();
    agent.wait_gone();
    let after = harness.call("status", None);
    assert_eq!(
        after["ok"]["phase"],
        serde_json::to_value(ServicePhase::Stopped).unwrap()
    );
    assert!(after["ok"]["service_pid"].is_null());
    assert!(after["ok"]["agent_pid"].is_null());
    harness.record("acceptance.json", &json!({"result":"PASS", "scope":"Models/basic text JSON/incremental SSE/single concurrency only", "version":version,"model":MODEL,"executable":harness.executable,"arguments":["acp"],"service_pid":service_pid,"agent_pid":agent_pid,"models":models,"json":completion,"sse":streamed,"stop":stopped,"after_stop":after}));
    println!(
        "Stage 1 PASS；证据目录：{}；GUI/工具/并行/其他完整验收 NOT RUN",
        harness.root.display()
    );
}
