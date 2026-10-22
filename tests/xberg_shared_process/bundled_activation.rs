use super::*;

fn place_base_assets(root: &Path) {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../resources/markdown-assets.json")).unwrap();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let path = member["path"].as_str().unwrap();
        if path == "xberg.exe"
            || path.starts_with("ffmpeg/")
            || path.starts_with("models/sense_voice_")
            || path.starts_with("models/vad/")
        {
            continue;
        }
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, b"synthetic base asset").unwrap();
    }
}

/// XB-14/XB-21：真实代理把 idle 检查、唯一引擎替换与来源提交串为一个边界。
/// 合成引擎只证明生命周期/事务，不代表目标模型真实推理已经验收。
#[test]
fn bundled_activation_refuses_active_work_and_preserves_source_on_failure() {
    use jchtools_foundation::xberg_settings;
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let old = temporary.path().join("old");
    let bundled = temporary.path().join("bundled");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&bundled).unwrap();
    prepare(&old);
    place_base_assets(&old);
    place_base_assets(&bundled);
    common::mock_engine_copy("tests/fixtures/shared_xberg.rs", &bundled.join("xberg.exe"));
    let state = shared_state(&old);
    let mut shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    let previous_source = xberg_settings::settings().unwrap().source;
    std::fs::write(old.join("long.txt"), "document").unwrap();
    let document_root = old.clone();
    let document = std::thread::spawn(move || {
        xberg_runtime::request(
            &document_root,
            json!({"command":"extract","path":document_root.join("long.txt")}),
            Duration::from_secs(30),
            &AtomicBool::new(false),
        )
        .unwrap()
    });
    let until = Instant::now() + Duration::from_secs(15);
    while !old.join("document-started").exists() {
        assert!(Instant::now() < until, "长文档必须先进入推理");
        std::thread::sleep(Duration::from_millis(10));
    }
    let rejected = xberg_runtime::activate_bundled_runtime(&bundled, false);
    let still_active = !document.is_finished();
    let unchanged = xberg_settings::required().unwrap() == old.canonicalize().unwrap();
    let during = xberg_runtime::request(
        &old,
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    std::fs::write(old.join("release-document"), "1").unwrap();
    let terminal = document.join().unwrap();
    assert!(rejected.is_err(), "在途推理必须拒绝迁移");
    assert!(still_active && unchanged, "迁移拒绝不得中断任务或提前切源");
    assert_eq!(during["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
    assert_eq!(
        during["text"], "截图结果",
        "没有音频资产仍能在文档执行期间截图"
    );
    assert_eq!(terminal["ok"], true);
    assert_eq!(terminal["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
    assert!(
        !bundled.join("starts.txt").exists(),
        "活跃时不得启动目标引擎"
    );

    let database = xberg_settings::open(&xberg_settings::state_dir().unwrap()).unwrap();
    database
        .execute_batch(
            "CREATE TRIGGER reject_runtime_source BEFORE INSERT ON app_settings
         WHEN NEW.key='xberg_source'
         BEGIN SELECT RAISE(ABORT, 'synthetic save failure'); END;",
        )
        .unwrap();
    let failed = xberg_runtime::activate_bundled_runtime(&bundled, false);
    let restored = shared_state(&old);
    shared.engine_pid = restored["jchtools_xberg_pid"].as_u64();
    assert!(failed.is_err(), "真实 SQLite 保存失败不得冒称迁移成功");
    assert_eq!(
        xberg_settings::required().unwrap(),
        old.canonicalize().unwrap()
    );
    assert_eq!(xberg_settings::settings().unwrap().source, previous_source);
    assert_eq!(
        restored["jchtools_broker_pid"],
        state["jchtools_broker_pid"]
    );
    assert!(!process_alive(
        state["jchtools_xberg_pid"].as_u64().unwrap(),
        "xberg.exe"
    ));
    for pid in std::fs::read_to_string(bundled.join("starts.txt"))
        .unwrap()
        .lines()
    {
        assert!(
            !process_alive(pid.parse().unwrap(), "xberg.exe"),
            "失败目标必须退出后才恢复原引擎"
        );
    }
    database
        .execute_batch("DROP TRIGGER reject_runtime_source")
        .unwrap();
    std::fs::write(bundled.join("xberg.exe"), b"not an executable").unwrap();
    assert!(xberg_runtime::activate_bundled_runtime(&bundled, false).is_err());
    let recovered = shared_state(&old);
    shared.engine_pid = recovered["jchtools_xberg_pid"].as_u64();
    assert_eq!(
        xberg_settings::required().unwrap(),
        old.canonicalize().unwrap()
    );
    assert_eq!(xberg_settings::settings().unwrap().source, previous_source);

    common::mock_engine_copy("tests/fixtures/shared_xberg.rs", &bundled.join("xberg.exe"));
    xberg_runtime::activate_bundled_runtime(&bundled, false).unwrap();
    let activated = shared_state(&bundled);
    shared.engine_pid = activated["jchtools_xberg_pid"].as_u64();
    assert_eq!(
        xberg_settings::required().unwrap(),
        bundled.canonicalize().unwrap()
    );
    assert_eq!(
        activated["jchtools_broker_pid"],
        state["jchtools_broker_pid"]
    );
    assert!(!process_alive(
        recovered["jchtools_xberg_pid"].as_u64().unwrap(),
        "xberg.exe"
    ));
    let stale = xberg_runtime::request(
        &old,
        json!({"command":"keepalive"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(stale["ok"], false, "切源后旧客户端不能把引擎切回旧根");
    assert_eq!(
        shared_state(&bundled)["jchtools_xberg_pid"],
        activated["jchtools_xberg_pid"]
    );
}

/// XB-21/XB-26：旧来源媒体已就绪、冷启动无活动槽时，也必须拒绝丢失转录能力。
/// 经真实公开迁移入口、离线成员事务、代理激活握手和 SQLite 提交边界验证；
/// 合成收据/引擎只验证事务及协议，不代表真实音轨或模型验收。
#[test]
fn review_f32_cold_migration_preserves_ready_media_on_transcription_false() {
    use jchtools_foundation::xberg_settings;
    use sha2::{Digest, Sha256};

    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let old = temporary.path().join("old");
    let bundled = temporary.path().join("bundled");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&bundled).unwrap();
    prepare(&old);
    place_base_assets(&old);
    place_base_assets(&bundled);
    review_engine_copy(&bundled);
    std::fs::write(bundled.join("disable-transcription"), b"1").unwrap();
    let _bundle = ReviewEnvironment::set("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", &bundled);
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../resources/markdown-assets.json")).unwrap();
    let bytes = b"matching optional audio asset";
    let digest = hex::encode(Sha256::digest(bytes));
    let mut audio = Vec::new();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let path = member["path"].as_str().unwrap();
        if !(path.starts_with("ffmpeg/")
            || path.starts_with("models/sense_voice_")
            || path.starts_with("models/vad/"))
        {
            continue;
        }
        let target = old.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, bytes).unwrap();
        audio.push(json!({"path":path,"size_bytes":bytes.len(),"sha256":digest}));
    }
    assert!(!audio.is_empty());
    xberg_runtime::validate_assets(&old, "media").unwrap();
    let engine = std::fs::read(bundled.join("xberg.exe")).unwrap();
    let components: Vec<_> = ["document", "snapshot"]
        .into_iter()
        .map(|id| {
            json!({
                "id":id,"archive":format!("xberg-{id}-x86_64-pc-windows-msvc.zip"),
                "requires":[],"size_bytes":1,"sha256":digest,
                "members":[{
                    "path":"xberg.exe","size_bytes":engine.len(),
                    "sha256":hex::encode(Sha256::digest(&engine))
                }]
            })
        })
        .collect();
    let release = "https://github.com/jchanghong023/xberg/releases/download/v2099.1";
    let receipt = json!({
        "owner":"JchTools-bundled-xberg-v1","tag":"v2099.1",
        "source":"https://api.github.com/repos/jchanghong023/xberg/releases/latest",
        "manifest":{
            "archive":"xberg-components-x86_64-pc-windows-msvc.json",
            "url":format!("{release}/xberg-components-x86_64-pc-windows-msvc.json"),
            "size_bytes":1,"sha256":digest
        },
        "components":components,
        "audio_assets":{
            "id":"audio-assets","archive":"xberg-audio-assets-x86_64-pc-windows-msvc.zip",
            "url":format!("{release}/xberg-audio-assets-x86_64-pc-windows-msvc.zip"),
            "requires":[],"size_bytes":1,"sha256":digest,"members":audio
        }
    });
    std::fs::write(
        bundled.join("jchtools-xberg-release.json"),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    let original_source = xberg_settings::settings().unwrap().source;
    assert!(!old.join("starts.txt").exists(), "冷启动前不能预热旧引擎");
    let mut progress = Vec::new();
    let migrated = jchtools_markdown::markdown_assets::migrate_bundled_runtime(
        &AtomicBool::new(false),
        |message| progress.push(message),
    )
    .unwrap();
    // 即使先红断言失败，也通过进程守卫回收本测试启动的目标/恢复引擎。
    let state = shared_state(&migrated);
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    assert!(
        bundled.join("starts.txt").exists(),
        "必须实际到达目标引擎握手，不能因收据/迁入前置失败误绿：{progress:?}"
    );
    assert_eq!(
        migrated,
        old.canonicalize().unwrap(),
        "transcription:false 不能替代冷启动前媒体已就绪的持久来源"
    );
    assert_eq!(
        xberg_settings::required().unwrap(),
        old.canonicalize().unwrap()
    );
    assert_eq!(xberg_settings::settings().unwrap().source, original_source);
    for member in receipt["audio_assets"]["members"].as_array().unwrap() {
        let path = member["path"].as_str().unwrap();
        assert_eq!(std::fs::read(old.join(path)).unwrap(), bytes);
        assert!(!bundled.join(path).exists(), "失败迁移应撤回本次复制成员");
    }
    let media = xberg_runtime::request(
        &old,
        json!({"command":"transcribe","path":old.join("short.txt")}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(media["ok"], true);
    assert_eq!(media["markdown"], "media");
}
