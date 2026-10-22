use super::tests_support::{redirect_component_env, sha256_bytes};
use std::fs;

#[test]
fn missing_worker_reports_repair_without_downloading_another_version() {
    let worker = super::SnapWorker {
        id: "snap-ocr-worker".to_string(),
        status: "ok".to_string(),
        url: "https://example.invalid/snap-ocr-worker.exe".to_string(),
        archive_type: "file".to_string(),
        install_path: "worker/v0.1.2/snap-ocr-worker.exe".to_string(),
        size_bytes: 1,
        sha256: "00".repeat(32),
        members: vec![],
        license: super::SnapLicense {
            component: "worker".to_string(),
            license: "MIT".to_string(),
            source: "https://example.invalid".to_string(),
        },
    };

    let message = super::worker_repair_message(&worker);
    assert!(message.contains("重新安装当前 JchTools 包"));
    assert!(message.contains("不会下载其他版本"));
}

// 覆盖 O-11/O-13：推理组件的在位校验——缺失、不完整与齐备分别得到明确的
// 结论，不冒称就绪；存在性检查不要求摘要（摘要校验随清单条目接入）。
#[test]
fn xberg_component_layout_reports_missing_incomplete_and_ready() {
    let temp = tempfile::tempdir().expect("临时目录应可创建");
    let component = temp.path().join("v2026.9.27-test");

    // 什么都没有：未配置。
    let error = super::xberg_layout_ready(&component).expect_err("空目录应报不完整");
    assert!(error.contains("推理组件不完整"), "unexpected: {error}");

    // 只放可执行文件：仍缺模型与运行库，且错误点名缺失项。
    fs::create_dir_all(&component).expect("组件目录应可创建");
    fs::write(component.join("xberg.exe"), b"stub").expect("stub 写入");
    let error = super::xberg_layout_ready(&component).expect_err("缺模型应报不完整");
    assert!(
        error.contains("det.onnx"),
        "错误应点名缺失的模型文件：{error}"
    );

    // 三件模型齐了但缺 onnxruntime.dll：仍不就绪。
    let models = component.join("models").join("snapshot-ocr");
    fs::create_dir_all(models.join("dict")).expect("模型目录应可创建");
    fs::write(models.join("det.onnx"), b"det").expect("stub 写入");
    fs::write(models.join("rec.onnx"), b"rec").expect("stub 写入");
    fs::write(models.join("dict").join("dict.txt"), b"dict").expect("stub 写入");
    let error = super::xberg_layout_ready(&component).expect_err("缺运行库应报不完整");
    assert!(
        error.contains("onnxruntime.dll"),
        "错误应点名缺失的运行库：{error}"
    );

    // 全部在位：就绪。
    fs::write(component.join("onnxruntime.dll"), b"ort").expect("stub 写入");
    assert!(super::xberg_layout_ready(&component).is_ok());
}

#[cfg(windows)]
#[test]
fn review_f40_pipe_name_uses_shared_token_identity_and_preserves_test_isolation() {
    let _guard = redirect_component_env();
    let isolated = super::pipe_name().expect("测试资产根必须有独立管道名");
    assert!(isolated.starts_with(r"\\.\pipe\jchtools-snap-ocr-test-"));
    std::env::remove_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT");
    let identity = jchtools_foundation::process_identity::user_session_key()
        .expect("从当前进程令牌获取用户与登录会话身份");
    let digest = sha256_bytes(identity.as_bytes());
    let expected = format!(r"\\.\pipe\jchtools-snap-ocr-{}", &digest[..16]);
    assert_eq!(super::pipe_name().expect("生产管道身份查询成功"), expected);
    assert_ne!(isolated, expected, "测试根不得触及生产用户会话管道");
}

// 覆盖 C-2（XB-09 2026-10-02 修订后的存在性口径）：设置页保存的组件目录
// 下，推理组件成员检查必须基于 resolve_xberg_component 解析出的组件目录，
// 而非资产根相对 install_path——否则配置目录永远无法就绪（在位校验通过后
// 必报「推理组件包校验失败」）。
// 覆盖树内全部 snapshot 成员以桩字节（与清单摘要不同）在场：存在性口径下
// 必须通过；删除成员后错误基于保存目录点名缺失项。
#[test]
fn readiness_inference_pack_honors_configured_component_dir() {
    let guard = redirect_component_env();
    let external = guard.root.path().join("external-component");
    // 与 worker 一样从完整共同清单取 snapshot 成员，不能用截图下载小清单
    // 作为就绪夹具而遗漏 sherpa-onnx 等所有场景共用运行依赖。
    let shared: serde_json::Value =
        serde_json::from_str(include_str!("../../../../resources/markdown-assets.json"))
            .expect("共享清单必须可解析");
    for member in shared["xberg"]["members"].as_array().expect("共享成员清单") {
        let relative = member["path"].as_str().expect("成员相对路径");
        if !jchtools_xberg::xberg_runtime::asset_for_scenario(relative, "snapshot") {
            continue;
        }
        let target = external.join(relative);
        fs::create_dir_all(target.parent().expect("成员路径有父目录")).expect("创建覆盖树成员目录");
        fs::write(&target, b"replaced-engine").expect("预置桩成员（字节与清单不同）");
    }
    fs::write(external.join("xberg.exe"), b"engine").expect("预置引擎文件供设置链校验");
    jchtools_foundation::xberg_settings::save(&external).expect("经设置链保存组件目录");

    super::readiness_inference_pack(guard.root.path())
        .expect("桩字节成员应通过存在性检查（替换引擎免摘要校验）");

    fs::remove_file(
        external
            .join("models")
            .join("snapshot-ocr")
            .join("rec.onnx"),
    )
    .expect("删除一个截图模型");
    let error = super::readiness_inference_pack(guard.root.path()).expect_err("缺失成员必须失败");
    assert!(
        error.contains("rec.onnx") && (error.contains("缺失") || error.contains("缺少")),
        "错误应基于保存目录点名缺失成员（在位校验与清单成员检查均为存在性口径）：{error}"
    );
    assert!(
        !error.contains("推理组件包校验失败"),
        "成员检查不得再走资产根相对路径（C-2）：{error}"
    );
}

// XB-09/XB-19/XB-20、O-09：设置页截图就绪必须接受且仅接受 worker
// snapshot 所需的同一完整成员集合。这里只测存在性，不执行合成模型/EXE。
#[test]
fn review_f30_snapshot_readiness_rejects_every_worker_required_missing_member() {
    let guard = redirect_component_env();
    let component = guard.root.path().join("shared-component");
    let worker_manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../../resources/markdown-assets.json"))
            .expect("读取 worker 使用的固定清单");
    let required: Vec<&str> = worker_manifest["xberg"]["members"]
        .as_array()
        .expect("worker 清单成员")
        .iter()
        .map(|member| member["path"].as_str().expect("成员相对路径"))
        .filter(|path| jchtools_xberg::xberg_runtime::asset_for_scenario(path, "snapshot"))
        .collect();
    assert!(required
        .iter()
        .any(|path| *path == "onnxruntime_providers_shared.dll"));
    assert!(required.iter().any(|path| path.starts_with("sherpa-onnx/")));
    for relative in &required {
        let target = component.join(relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, b"presence-only replacement").unwrap();
    }
    jchtools_foundation::xberg_settings::save(&component).unwrap();
    jchtools_xberg::xberg_runtime::validate_assets(&component, "snapshot")
        .expect("仅截图和共同成员完整，不需要文档/音频模型或 FFmpeg");
    super::readiness_inference_pack(guard.root.path()).expect("完整截图成员应报告就绪");

    let mut wrongly_ready = Vec::new();
    for relative in required {
        let target = component.join(relative);
        fs::remove_file(&target).unwrap();
        let worker_error = jchtools_xberg::xberg_runtime::validate_assets(&component, "snapshot")
            .expect_err("worker 必须拒绝缺少所需成员的目录");
        let readiness = super::readiness_inference_pack(guard.root.path());
        fs::write(&target, b"presence-only replacement").unwrap();
        assert!(
            worker_error.contains(relative),
            "worker 必须指明缺失成员：{worker_error}"
        );
        if readiness.is_ok() {
            wrongly_ready.push(relative.to_string());
        }
    }
    assert!(
        wrongly_ready.is_empty(),
        "设置页错误地报告截图就绪，但 worker 已拒绝缺少的成员：{wrongly_ready:?}"
    );
}

// ── S7-02/S7-05：就绪检查的完整性校验与失败原因透传 ──
// 覆盖 O-09：自动启动直接取得 worker 路径，缓存副本必须先通过完整性校验。
#[test]
fn worker_install_path_rejects_unverified_cached_worker() {
    let guard = redirect_component_env();
    std::env::remove_var("JCHTOOLS_TEST_BUNDLED_WORKER");
    let manifest = super::load_manifest().expect("内置清单必须可解析");
    let target = guard.root.path().join(&manifest.workers[0].install_path);

    let missing = super::worker_install_path().expect_err("缺失缓存 worker 不得交给启动链");
    assert!(missing.contains("工作进程"), "必须点名工作进程：{missing}");

    fs::create_dir_all(target.parent().expect("worker 路径有父目录"))
        .expect("创建缓存 worker 目录");
    fs::write(&target, b"unverified-worker").expect("预置未校验缓存 worker");
    let corrupted = super::worker_install_path().expect_err("损坏缓存 worker 不得交给启动链");
    assert!(
        corrupted.contains("校验失败"),
        "必须说明缓存 worker 校验失败：{corrupted}"
    );
    assert!(
        missing.contains("重新安装当前 JchTools 包")
            && corrupted.contains("重新安装当前 JchTools 包"),
        "缺失或损坏 worker 的启动提示必须指引修复：{missing}；{corrupted}"
    );
}

// 覆盖 O-09（S7-02）：bundled worker 文件在场但损坏（与清单大小/摘要不符）时，
// 就绪检查的工作进程段必须与启动链同口径报「未就绪」并携带具体原因——修复前
// 该分支只检查 path.is_file()，损坏 worker 在 readiness 报已就绪、启动链才失败。
// 内置清单当前为 pending-build，readiness() 会在更早的占位检查处短路，无法端到
// 端触达 bundled 分支；本用例以合成 worker 直接驱动工作进程段，与启动链共用
// verify_bundled_worker 判据保证同口径。
#[test]
fn worker_readiness_rejects_corrupted_bundled_worker() {
    let guard = redirect_component_env();
    let worker_dir = tempfile::tempdir().expect("创建 bundled worker 目录");
    let worker_path = worker_dir.path().join("snap-ocr-worker.exe");
    fs::write(&worker_path, b"corrupted-worker").expect("写入损坏的 worker");
    std::env::set_var("JCHTOOLS_TEST_BUNDLED_WORKER", &worker_path);

    let worker = super::SnapWorker {
        id: "snap-ocr-worker".to_string(),
        status: "ok".to_string(),
        url: "https://example.invalid/snap-ocr-worker.exe".to_string(),
        archive_type: "file".to_string(),
        install_path: "worker/v0.1.2/snap-ocr-worker.exe".to_string(),
        // 大小与摘要均为「正确内容」的期望值：实际文件是另一种字节（损坏）。
        size_bytes: b"healthy-worker".len() as u64,
        sha256: sha256_bytes(b"healthy-worker"),
        members: vec![],
        license: super::SnapLicense {
            component: "worker".to_string(),
            license: "MIT".to_string(),
            source: "https://example.invalid".to_string(),
        },
    };

    // 大小不符：必须报未就绪，且错误点名大小差异。
    let error = super::worker_readiness(&worker, guard.root.path())
        .expect_err("损坏的 bundled worker 必须在就绪检查报未就绪");
    assert!(
        error.contains("校验失败") || error.contains("未就绪"),
        "提示必须明确指出工作进程未通过校验：{error}"
    );
    assert!(
        error.contains("大小") || error.contains("SHA256"),
        "提示必须携带具体失败原因（大小或摘要）：{error}"
    );
    assert!(
        error.contains("重新安装当前 JchTools 包"),
        "离线检查必须指引用户修复随包 worker：{error}"
    );

    // 大小正确、摘要不符：错误改为点名摘要差异（两种失败可区分）。
    fs::write(&worker_path, b"healthy-workXr").expect("写入摘要不符的 worker");
    let error = super::worker_readiness(&worker, guard.root.path())
        .expect_err("摘要不符的 bundled worker 必须报未就绪");
    assert!(
        error.contains("SHA256") && error.contains("预期"),
        "摘要不符必须点名摘要差异：{error}"
    );

    // 内容正确的 bundled worker：通过（就绪判定不得误伤完好文件）。
    fs::write(&worker_path, b"healthy-worker").expect("写入完好的 worker");
    super::worker_readiness(&worker, guard.root.path())
        .expect("完好的 bundled worker 必须通过就绪检查");
}

// O-09：发布 profile 开启 debug-assertions，用户可将便携包解压到任意
// target/debug 目录；目录拼写不能授权执行摘要不符的随包 worker。
#[test]
fn review_f31_release_worker_integrity_is_not_bypassed_by_target_debug_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary
        .path()
        .join("portable")
        .join("target")
        .join("debug");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("snap-ocr-worker.exe");
    let mut manifest = super::load_manifest().expect("内置清单可解析");
    let worker = manifest.workers.first_mut().expect("清单包含 worker");
    worker.status = "ok".to_string();
    worker.size_bytes = b"healthy-worker".len() as u64;
    worker.sha256 = sha256_bytes(b"healthy-worker");

    fs::write(&path, b"healthy-worker").unwrap();
    super::verify_bundled_worker(&path, worker).expect("正确字节不应因目录名被拒绝");
    let mut accepted_corruption = Vec::new();
    for bytes in [b"short".as_slice(), b"healthy-workXr".as_slice()] {
        fs::write(&path, bytes).unwrap();
        if super::verify_bundled_worker(&path, worker).is_ok() {
            accepted_corruption.push(bytes);
        }
    }
    assert!(
        accepted_corruption.is_empty(),
        "O-09：任意 target/debug 目录不能豁免大小或摘要校验：{accepted_corruption:?}"
    );
}

// 覆盖 O-09/O-30（S7-05）：readiness 对资产的具体失败原因（缺失、大小不符、
// 摘要不符）必须透传到返回文案，不得压成统一的「未安装或校验失败」——修复前
// GUI 只能显示压缩文案，失败种类丢失。以首个资产（字体 zip 成员）驱动三类。
#[test]
fn readiness_reports_distinct_asset_failure_kinds() {
    let guard = redirect_component_env();
    let mut manifest = super::load_manifest().expect("内置清单必须可解析");
    // 内置字体不再检查缓存；用非内置资产继续锁定失败原因透传。
    manifest.assets[0].id = "external-test-asset".into();
    let font = &manifest.assets[0];
    let member = &font.members[0];
    let target = guard.root.path().join(&member.install_path);

    // 缺失：点名资产 id，且不得出现压缩文案或大小/摘要标记。
    let missing = super::local_asset_readiness(&manifest.assets, guard.root.path())
        .expect_err("非内置资产缺失必须报未就绪");
    assert!(missing.contains(&font.id), "必须点名失败资产：{missing}");
    assert!(
        !missing.contains("未安装或校验失败"),
        "压缩文案必须被具体原因取代：{missing}"
    );
    assert!(
        !missing.contains("大小") && !missing.contains("SHA256"),
        "缺失类不得混入大小/摘要标记：{missing}"
    );

    // 大小不符：透传「大小 X，预期 Y」。
    fs::create_dir_all(target.parent().expect("成员路径有父目录")).expect("创建字体目录");
    fs::write(&target, b"too-short").expect("写入过短的字体成员");
    let sized = super::local_asset_readiness(&manifest.assets, guard.root.path())
        .expect_err("大小不符必须报未就绪");
    assert!(
        sized.contains("大小") && sized.contains(&member.size_bytes.to_string()),
        "必须透传大小原因：{sized}"
    );
    assert!(sized.contains(&font.id), "必须点名失败资产：{sized}");

    // 摘要不符（大小正确、内容错误）：透传「SHA256 ...，预期 ...」。
    {
        use std::io::Write as _;
        let size = usize::try_from(member.size_bytes).expect("清单大小必须可转换");
        let mut file = fs::File::create(&target).expect("创建字体成员文件");
        file.write_all(&vec![0_u8; size])
            .expect("写入内容错误的字体成员");
    }
    let digest = super::local_asset_readiness(&manifest.assets, guard.root.path())
        .expect_err("摘要不符必须报未就绪");
    assert!(
        digest.contains("SHA256") && digest.contains(&member.sha256),
        "必须透传摘要原因：{digest}"
    );
}

// ── C-3：下载中断重试 ──

// 覆盖 O-05/XB-25：主程序的新装检查不要求外部字体缓存。
#[test]
fn local_readiness_needs_no_external_font_cache() {
    let root = tempfile::tempdir().expect("创建隔离缓存");
    let manifest = super::load_manifest().expect("解析清单");
    assert!(super::local_asset_readiness(&manifest.assets, root.path()).is_ok());
    assert!(root
        .path()
        .read_dir()
        .expect("读取隔离缓存")
        .next()
        .is_none());
}

// 覆盖 O-11 许可口径：notice 必须列出推理组件包的许可条目并指向安装目录
//（与 markdown 侧「许可随组件树自带」口径对称）——修复前只遍历
// assets + workers，不含 xberg_inference。
#[test]
fn notice_lists_xberg_inference_license() {
    let manifest = super::SnapAssetManifest {
        schema_version: 1,
        assets: vec![],
        workers: vec![],
        xberg_inference: Some(super::SnapInferencePack {
            tag: "vtest-tag".to_string(),
            asset: super::SnapAsset {
                id: "xberg-inference".to_string(),
                url: "https://fixtures.invalid/pack.zip".to_string(),
                archive_type: "zip".to_string(),
                install_path: None,
                size_bytes: 42,
                sha256: "ab".repeat(32),
                members: vec![],
                license: super::SnapLicense {
                    component: "Xberg 推理组件".to_string(),
                    license: "MIT".to_string(),
                    source: "https://fixtures.invalid".to_string(),
                },
            },
        }),
    };
    let root = tempfile::tempdir().expect("创建测试目录");
    let notice = root.path().join("THIRD_PARTY_NOTICES.md");
    super::write_notice(&notice, &manifest).expect("写入 notice");
    let text = fs::read_to_string(&notice).expect("读取 notice");
    assert!(
        text.contains("Xberg 推理组件") && text.contains("MIT"),
        "notice 必须列出推理组件包的许可条目：{text}"
    );
    assert!(
        text.contains("xberg-inference"),
        "推理组件条目应指向安装目录 xberg-inference/：{text}"
    );
}
