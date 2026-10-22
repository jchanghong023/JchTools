use super::*;

/// 覆盖 T-23/T-25/XB-15/XB-17：从批处理公开入口停止当前文件，保留已完成
/// 结果且不开始下一文件；截图及唯一共享引擎继续可用。合成引擎只用于协议
/// 回归，不替代真实 GUI / 模型验收。超时后释放夹具并收尾，避免反证遗留进程。
fn assert_batch_stop_cancels_current_file(media: bool) {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    prepare(root);
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../resources/markdown-assets.json")).unwrap();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let relative = member["path"].as_str().unwrap();
        if relative != "xberg.exe" {
            let target = root.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, b"synthetic asset").unwrap();
        }
    }
    jchtools_markdown::markdown_assets::save_runtime_dir(root).unwrap();
    let input = root.join("input");
    let output = root.join("output");
    std::fs::create_dir(&input).unwrap();
    std::fs::create_dir(&output).unwrap();
    let extension = if media { "mp4" } else { "txt" };
    for name in ["01-complete", "02-long", "03-next"] {
        std::fs::write(
            input.join(format!("{name}.{extension}")),
            b"synthetic input",
        )
        .unwrap();
    }
    let state = xberg_runtime::request(
        root,
        json!({"command":"snapshot_state"}),
        Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(state["ok"], true, "合成引擎启动失败：{state}");
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    let options = jchtools_markdown::markdown::Options {
        input_dir: input,
        output_dir: output.clone(),
        flat: false,
        groups: vec![if media {
            jchtools_markdown::markdown::FormatGroup::Media
        } else {
            jchtools_markdown::markdown::FormatGroup::Other
        }],
        timeout_secs: 15,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let token = cancel.clone();
    let started = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = started.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let batch = std::thread::spawn(move || {
        let result = jchtools_markdown::markdown::run(&options, &token, |event| {
            if let jchtools_markdown::markdown::Event::FileStarted { relative, .. } = event {
                observed.lock().unwrap().push(relative);
            }
        });
        send.send(result).unwrap();
    });
    let marker = root.join(if media {
        "media-started"
    } else {
        "document-started"
    });
    // CI runner 在整套测试并行进程压力下，批处理初始化（能力握手/资产就绪）
    // 可能明显慢于本地；等待窗口与本文件其他会话等待一致放宽到 60s。批处理若
    // 在当前文件开始前提前结束（run() 提前返回），立即报告其结果而非盲等；
    // T-23 的「停止必须尽快生效」断言在标记出现之后仍按 3s 严格计时。
    let start_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if marker.exists() {
            break;
        }
        if let Ok(early) = receive.try_recv() {
            panic!(
                "批次在当前文件开始前结束：{early:?}（已观察 FileStarted：{:?}）",
                started.lock().unwrap()
            );
        }
        assert!(
            Instant::now() < start_deadline,
            "批次未开始当前文件（已观察 FileStarted：{:?}）",
            started.lock().unwrap(),
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let completed = output.join(format!("01-complete_{extension}.md"));
    let completed_bytes = std::fs::read(&completed).unwrap();
    let screenshot = xberg_runtime::request(
        root,
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        screenshot["jchtools_xberg_pid"],
        state["jchtools_xberg_pid"]
    );
    cancel.store(true, Ordering::Release);
    let first = receive.recv_timeout(Duration::from_secs(3));
    let prompt = first.is_ok();
    let result = if let Ok(result) = first {
        result
    } else {
        // 修复前失败时也先释放夹具、回收线程和进程，再报告反证。
        std::fs::write(root.join("release-document"), b"release").unwrap();
        receive.recv_timeout(Duration::from_secs(15)).unwrap()
    };
    batch.join().unwrap();
    let after = xberg_runtime::request(
        root,
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(after["text"], "截图结果");
    assert_eq!(after["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
    assert!(
        prompt,
        "停止请求必须尽快取消当前文件，不得等待当前文件自然结束"
    );
    let summary = result.unwrap();
    assert!(summary.stopped);
    assert_eq!(summary.success, 1, "取消的当前文件不得计为成功");
    assert_eq!(summary.failed, 0, "用户取消不得计为失败");
    assert_eq!(std::fs::read(completed).unwrap(), completed_bytes);
    assert!(!output.join(format!("02-long_{extension}.md")).exists());
    assert!(!output.join(format!("02-long_{extension}_media")).exists());
    assert!(!output.join(format!("03-next_{extension}.md")).exists());
    assert_eq!(started.lock().unwrap().len(), 2, "停止后不得开始下一文件");
}

/// T-03 平台门槛：转 Markdown 仅支持 Windows 11 x64 工作站（platform_preflight
/// 按契约拒绝 Server/旧构建）。CI 的 GitHub windows runner 是 Windows Server，
/// 批处理在该环境无法按契约执行；这三个协议回归测试据此跳过，不伪造覆盖。
/// 被跳过的行为（T-23/T-25 批处理停止）在受支持平台（Windows 11 主机）由
/// fulltest/slowtest 的同一测试保持覆盖。
fn skip_when_platform_unsupported() -> bool {
    match jchtools_markdown::markdown::page_readiness() {
        Err(message) if message.contains("Windows 11 x64") => {
            eprintln!("skip：{message}（CI runner 不在 T-03 支持平台内；覆盖见 Windows 11 主机的本地测试门）");
            true
        }
        _ => false,
    }
}

// 覆盖 T-23/T-25/XB-15/XB-17。
#[test]
fn batch_stop_cancels_current_document_and_keeps_shared_snapshot_alive() {
    if skip_when_platform_unsupported() {
        return;
    }
    assert_batch_stop_cancels_current_file(false);
}

// 覆盖 T-19/T-23/T-25/XB-15/XB-17。
#[test]
fn batch_stop_cancels_current_media_and_keeps_shared_snapshot_alive() {
    if skip_when_platform_unsupported() {
        return;
    }
    assert_batch_stop_cancels_current_file(true);
}

/// 覆盖 T-23/T-24/XB-17：用户停止时，取消确认不能冒充原请求已经结束；
/// 未结束诊断必须经过批处理公开入口保留，停止后不开始下一文件，截图仍可用。
#[test]
fn batch_stop_preserves_unfinished_request_diagnostic_and_snapshot_alive() {
    if skip_when_platform_unsupported() {
        return;
    }
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    prepare(root);
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../resources/markdown-assets.json")).unwrap();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let relative = member["path"].as_str().unwrap();
        if relative != "xberg.exe" {
            let target = root.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, b"synthetic asset").unwrap();
        }
    }
    jchtools_markdown::markdown_assets::save_runtime_dir(root).unwrap();
    let input = root.join("input");
    let output = root.join("output");
    std::fs::create_dir(&input).unwrap();
    std::fs::create_dir(&output).unwrap();
    for name in ["01-unresponsive.txt", "02-next.txt"] {
        std::fs::write(input.join(name), b"synthetic input").unwrap();
    }
    let state = xberg_runtime::request(
        root,
        json!({"command":"snapshot_state"}),
        Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(state["ok"], true);
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    let options = jchtools_markdown::markdown::Options {
        input_dir: input,
        output_dir: output.clone(),
        flat: false,
        groups: vec![jchtools_markdown::markdown::FormatGroup::Other],
        timeout_secs: 15,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let token = cancel.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let batch = std::thread::spawn(move || {
        let mut events = Vec::new();
        let result = jchtools_markdown::markdown::run(&options, &token, |event| events.push(event));
        send.send((result, events)).unwrap();
    });
    // 同批处理标记等待口径：提前结束立即报告批处理结果，慢负载由 60s 窗口吸收。
    let start_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if root.join("document-started").exists() {
            break;
        }
        if let Ok(early) = receive.try_recv() {
            panic!("批次在无响应请求开始前结束：{early:?}");
        }
        assert!(Instant::now() < start_deadline, "批次未进入无响应请求");
        std::thread::sleep(Duration::from_millis(10));
    }
    cancel.store(true, Ordering::Release);
    let (result, events) = receive.recv_timeout(Duration::from_secs(3)).unwrap();
    batch.join().unwrap();
    let after = xberg_runtime::request(
        root,
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(after["text"], "截图结果");
    assert_eq!(after["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
    let summary = result.unwrap();
    assert!(summary.stopped);
    assert_eq!(
        summary.failed, 1,
        "取消接口已确认但原请求未结束必须保留为异常"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            jchtools_markdown::markdown::Event::FileFinished { success: false, message, .. }
                if message.contains("未在 1 秒内结束") && message.contains("保留未结束任务")
        )),
        "必须向界面保留原请求未结束的具体诊断：{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            jchtools_markdown::markdown::Event::FileCancelled { .. }
        )),
        "取消失败不得冒充普通取消"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                jchtools_markdown::markdown::Event::FileStarted { .. }
            ))
            .count(),
        1,
        "停止后不得继续下一文件"
    );
    assert_eq!(
        std::fs::read_dir(output).unwrap().count(),
        0,
        "不得留下未完成结果"
    );
}
