//! 覆盖 P-10 / AH-04：ACP prompt 摘要统计实际新增文本与后端会话复用，不泄露正文或工作目录。
//! 合成官方 SDK Agent 与真实 HTTP，不代替真实 OpenCode 模型验收。
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fixtures/acp_api/main.rs"]
mod fixture;
use fixture::{consumer::{audit, completion, wait_event, Http, Server}, Plan, MODEL_A};
use serde_json::json;

#[test]
fn prompt_summary_counts_only_submitted_messages_and_never_logs_content() {
    const CHILD: &str = "JCHTOOLS_PROMPT_SUMMARY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "prompt_summary_counts_only_submitted_messages_and_never_logs_content", "--nocapture"])
            .env(CHILD, "1").env("JCH_LOG_LEVEL", "info").status().unwrap();
        assert!(result.success(), "隔离摘要验证失败");
        return;
    }
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let directory = repo.join(".tmp/acp-prompt-summary");
    assert!(std::process::Command::new("python").arg(repo.join("scripts/make_tmp.py"))
        .args(["workspace", "--destination"]).arg(&directory).status().unwrap().success());
    let logs = tempfile::tempdir_in(directory).unwrap();
    let guard = jchtools::logging::init(logs.path()).expect("测试日志初始化");
    let server = Server::start(env!("CARGO_BIN_EXE_jchtools-acp-fixture"), "normal");
    let first = Plan::new("text");
    let secret = "SYNTHETIC_SECRET_凭据_正文";
    let texts = [
        ("system", format!("system-{secret}")),
        ("developer", format!("developer-{secret}")),
        ("assistant", format!("assistant-{secret}")),
        ("user", format!("使用 scout；保持约束 {secret}\n{}", first.prompt())),
    ];
    let mut messages = texts.iter().map(|(role,text)|json!({"role":role,"content":text})).collect::<Vec<_>>();
    let first_body = json!({"model":MODEL_A,"cwd":server.root,"messages":messages});
    let response = Http::open(server.port,"POST","/v1/chat/completions",Some(&first_body),None);
    assert_eq!(response.status,200);
    let session = response.headers["x-jchtools-session-id"].clone();
    let reply = completion(&response.json()).to_owned();
    let first_event = wait_event(&server.root,"started",&first.tag);
    assert!(first_event["prompt"].as_str().unwrap().contains(secret));
    let second = Plan::new("text");
    let new_text = format!("新增中文 {secret}\n{}",second.prompt());
    messages.extend([json!({"role":"assistant","content":reply}),json!({"role":"user","content":new_text})]);
    let second_body = json!({"model":MODEL_A,"cwd":server.root,"messages":messages});
    let response = Http::open(server.port,"POST","/v1/chat/completions",Some(&second_body),Some(&session));
    assert_eq!(response.status,200);
    assert_eq!(completion(&response.json()),second.text());
    let second_event = wait_event(&server.root,"started",&second.tag);
    assert_eq!(first_event["session"],second_event["session"]);
    assert_eq!(second_event["prompt"],format!("[user]\n{new_text}"));
    // 未见过的显式标识没有 ACP 状态，不能把“提供请求头”记作复用。
    let third = Plan::new("text");
    let third_body = json!({"model":MODEL_A,"cwd":server.root,"messages":[{"role":"user","content":third.prompt()}]});
    let response = Http::open(server.port,"POST","/v1/chat/completions",Some(&third_body),Some("new-explicit-session"));
    assert_eq!(response.status,200);
    assert_eq!(completion(&response.json()),third.text());
    let third_event = wait_event(&server.root,"started",&third.tag);
    assert_ne!(first_event["session"],third_event["session"]);
    // 同目录的探测状态可被无请求头的首轮接管；实际 ACP 会话未新建。
    let before_probe = audit(&server.root);
    let probe = before_probe.iter().find(|event|event["event"]=="session").unwrap()["session"].clone();
    let sessions_before_probe = before_probe.iter().filter(|event|event["event"]=="session").count();
    let fourth = Plan::new("text");
    let fourth_body = json!({"model":MODEL_A,"cwd":server.workspace,"messages":[{"role":"user","content":fourth.prompt()}]});
    let response = Http::open(server.port,"POST","/v1/chat/completions",Some(&fourth_body),None);
    assert_eq!(response.status,200);
    assert_eq!(completion(&response.json()),fourth.text());
    let fourth_event = wait_event(&server.root,"started",&fourth.tag);
    assert_eq!(fourth_event["session"],probe);
    assert_eq!(audit(&server.root).iter().filter(|event|event["event"]=="session").count(),sessions_before_probe);
    server.shutdown();
    drop(guard); // 非阻塞 worker 退役并刷盘后再读取。
    let recorded = std::fs::read_dir(logs.path().join("logs")).unwrap().map(|entry|std::fs::read_to_string(entry.unwrap().path()).unwrap()).collect::<Vec<_>>().join("\n");
    assert!(!recorded.contains(secret),"诊断不得包含合成敏感正文");
    assert!(!recorded.contains(server.workspace.to_str().unwrap()),"摘要不得记录工作目录");
    assert!(!recorded.contains(server.root.to_str().unwrap()),"摘要不得记录任务目录");
    let summary = recorded.lines().filter(|line|line.contains("event=acp_prompt_summary")).collect::<Vec<_>>();
    assert_eq!(summary.len(),4,"每次实际 prompt 提交须有一条摘要");
    for line in &summary {
        let id = line.split("request_id=").nth(1).unwrap().split_whitespace().next().unwrap();
        assert!(uuid::Uuid::parse_str(id.trim_matches('"')).is_ok(),"请求关联 ID 必须可核对");
    }
    assert!(summary[0].contains("reused_session=false"));
    for field in ["message_count=4","system_count=1","developer_count=1","assistant_count=1","user_count=1"] {
        assert!(summary[0].contains(field),"首轮缺统计 {field}");
    }
    assert!(summary[0].contains(&format!("text_bytes={}",texts.iter().map(|(_,text)|text.len()).sum::<usize>())));
    assert!(summary[1].contains("reused_session=true"));
    for field in ["message_count=1","system_count=0","developer_count=0","assistant_count=0","user_count=1"] {
        assert!(summary[1].contains(field),"续轮应只统计新增内容 {field}");
    }
    assert!(summary[1].contains(&format!("text_bytes={}",new_text.len())));
    assert!(summary[2].contains("reused_session=false"),"未知显式标识须新建 ACP 会话");
    assert!(summary[3].contains("reused_session=true"),"无请求头接管探测状态也属于实际复用");
}
