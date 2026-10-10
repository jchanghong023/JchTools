//! AH-04/AH-12：HTTP/1.1 预读管线字节不得遮蔽首事件/完成前的断连取消。
//! 真实 TCP → Axum 产品入口 → 官方 SDK 合成 Agent；不是实际模型验收。
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fixtures/acp_api/main.rs"]
mod fixture;

use fixture::{
    consumer::{audit, body, completion, release, wait_event, Http, Server},
    Plan,
};
use serde_json::Value;
use std::{
    fmt::Write as _,
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

/// 先于 Server 析构释放本测试的 barrier；任一早期断言失败也不留下静默轮次。
struct ReleaseBarriers {
    root: PathBuf,
    plans: Vec<Plan>,
}
impl Drop for ReleaseBarriers {
    fn drop(&mut self) {
        for plan in &self.plans {
            let _released =
                std::fs::write(self.root.join(format!("release-{}", plan.tag)), b"release");
        }
    }
}

fn pipeline_chat(port: u16, plan: &Plan, stream: bool, following_requests: usize) -> TcpStream {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let body = body(plan, stream).to_string();
    // 同一次写入携带下一条真实 GET：不能用 Connection: close 或空 readbuf 的
    // 单请求断连替代本回归。预读的 GET 字节在第一条响应尚未生成时留在连接内。
    // 额外合法标头使管线尾部明显超过第一条 POST，不依赖一字节写入时序。
    let pipeline_padding = "p".repeat(4096);
    let mut bytes = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    for index in 0..following_requests {
        let connection = if index + 1 == following_requests {
            "close"
        } else {
            "keep-alive"
        };
        write!(
            bytes,
            "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AH-Pipeline: {pipeline_padding}\r\nConnection: {connection}\r\n\r\n"
        )
        .unwrap();
    }
    socket.write_all(bytes.as_bytes()).unwrap();
    socket.flush().unwrap();
    socket
}

/// 等待消费者 audit，而不是延迟后猜测取消已发生。返回失败证据以便先完成正常 Stop。
fn wait_consumed_cancel(root: &std::path::Path, started: &Value, tag: &str) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let events = audit(root);
        let received = events.iter().any(|event| {
            event["event"] == "cancel-received"
                && event["session"] == started["session"]
                && event["pid"] == started["pid"]
        });
        let consumed = events.iter().any(|event| {
            event["event"] == "cancelled" && event["tag"] == tag && event["pid"] == started["pid"]
        });
        if (received && consumed) || Instant::now() >= deadline {
            return events;
        }
        // 仅按明确 audit 条件轮询；无“睡够时间就算成功”的同步。
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_buffered_disconnect_cancels_only_its_turn(stream: bool) {
    // 共享夹具通过正式 http::serve 装配真实连接观察器，不绕过产品监听入口。
    let server = Server::start(env!("CARGO_BIN_EXE_jchtools-acp-fixture"), "normal");
    let mut victim = Plan::new("silent");
    victim.barrier = true;
    let mut survivor = Plan::new("text");
    survivor.barrier = true;
    survivor.chunks = vec!["独立会话首段".into(), "独立会话尾段".into()];
    let _barriers = ReleaseBarriers {
        root: server.root.clone(),
        plans: vec![victim.clone(), survivor.clone()],
    };

    // 尾部超过 Hyper 单次预读容量，同时覆盖其 read-ahead 和内核中未消费的字节。
    let socket = pipeline_chat(server.port, &victim, stream, 32);
    let started = wait_event(&server.root, "started", &victim.tag);
    // silent 在 barrier 前没有文本/完成；不能先收到 SSE 再断连来绕过未提交响应头路径。
    assert!(!audit(&server.root)
        .iter()
        .any(|event| { event["event"] == "completed" && event["tag"] == victim.tag }));
    let mut other = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&survivor, true)),
        None,
    );
    assert_eq!(other.status, 200);
    assert_eq!(other.delta(), survivor.chunks[0]);
    let other_started = wait_event(&server.root, "started", &survivor.tag);
    assert_eq!(other_started["pid"], started["pid"]);
    assert_ne!(other_started["session"], started["session"]);

    socket.shutdown(Shutdown::Both).unwrap();
    drop(socket);
    let cancel_events = wait_consumed_cancel(&server.root, &started, &victim.tag);

    // 红测超时也先放行两个 barrier，完成独立会话并显式正常 Stop，然后才报告断言。
    // 取消证据必须来自放行前的快照，不能把 Stop 导致的取消当成断连取消。
    release(&server.root, &victim);
    release(&server.root, &survivor);
    assert_eq!(other.finish_stream(), survivor.chunks[1]);
    wait_event(&server.root, "completed", &survivor.tag);
    drop(other);
    let next = Plan::new("text");
    let response = server.post(&next);
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), next.text());
    let next_started = wait_event(&server.root, "started", &next.tag);
    let final_events = audit(&server.root);
    server.shutdown();

    assert_eq!(next_started["pid"], started["pid"], "断连不应重建 Agent");
    assert_eq!(
        final_events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1,
        "独立会话须继续使用同一个 Agent"
    );
    assert!(
        !final_events.iter().any(|event| {
            (event["event"] == "cancelled" && event["tag"] == survivor.tag)
                || (event["event"] == "cancel-received"
                    && event["session"] == other_started["session"])
        }),
        "断连取消污染了另一会话: {final_events:?}"
    );
    assert!(
        cancel_events.iter().any(|event| {
            event["event"] == "cancel-received"
                && event["session"] == started["session"]
                && event["pid"] == started["pid"]
        }),
        "管线 read-ahead 遮蔽断连，Agent 未实际收到对应 ACP cancel: {cancel_events:?}"
    );
    assert!(
        cancel_events.iter().any(|event| {
            event["event"] == "cancelled"
                && event["tag"] == victim.tag
                && event["pid"] == started["pid"]
        }),
        "Agent 未在放行 barrier 前消费对应轮次取消: {cancel_events:?}"
    );
}

/// AH-04/AH-12：首个 SSE 事件前也必须取消，已有管线字节不能隐藏 EOF。
#[test]
fn pipelined_disconnect_before_first_stream_event_consumes_acp_cancel() {
    assert_buffered_disconnect_cancels_only_its_turn(true);
}

/// AH-04/AH-12：非流式响应尚未完成时也必须取消，不能等文本写出才发现 EOF。
#[test]
fn pipelined_disconnect_before_json_completion_consumes_acp_cancel() {
    assert_buffered_disconnect_cancels_only_its_turn(false);
}

/// AH-04/AH-13：正常可读不能误判断连；观察器不得消费下一条真实管线请求。
#[test]
fn live_pipeline_preserves_every_request_without_cancelling() {
    let server = Server::start(env!("CARGO_BIN_EXE_jchtools-acp-fixture"), "normal");
    let plan = Plan::new("text");
    let mut socket = pipeline_chat(server.port, &plan, false, 32);
    socket
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let mut responses = String::new();
    socket.read_to_string(&mut responses).unwrap();
    let started = wait_event(&server.root, "started", &plan.tag);
    wait_event(&server.root, "completed", &plan.tag);
    let events = audit(&server.root);
    server.shutdown();
    assert_eq!(responses.matches("HTTP/1.1 200 OK\r\n").count(), 33);
    assert_eq!(responses.matches("\"object\":\"list\"").count(), 32);
    assert!(responses.contains(&plan.text()));
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    assert!(
        !events.iter().any(|event| {
            event["event"] == "cancel-received" && event["session"] == started["session"]
        }),
        "可读管线请求被误判为断连：{events:?}"
    );
}
