use super::tests_support::{redirect_component_env, sha256_bytes};
use std::fs;
use std::sync::atomic::AtomicBool;

// P3-18 / O-06 / O-09：用已回填 worker 清单和有效 Xberg 成员实际初始化，
// 锁定最终返回给设置页的失败原因，而非仅检查 readiness 助手的返回值。
fn review_remediation_initialize_worker_failure(
    cached_bytes: Option<&[u8]>,
    bundled_bytes: Option<&[u8]>,
) -> (String, String) {
    let guard = redirect_component_env();
    std::env::remove_var("JCHTOOLS_TEST_BUNDLED_WORKER");
    let root = guard.root.path();
    let mut manifest = super::load_manifest().expect("解析内置清单");
    let worker = &mut manifest.workers[0];
    worker.status = "ok".into();
    worker.size_bytes = b"healthy-worker".len() as u64;
    worker.sha256 = sha256_bytes(b"healthy-worker");
    let cached_path = root.join(&worker.install_path);
    fs::create_dir_all(cached_path.parent().expect("worker 有父目录"))
        .expect("创建缓存 worker 目录");
    let missing_reason = fs::metadata(&cached_path)
        .expect_err("新建隔离目录中 worker 尚不存在")
        .to_string();
    if let Some(bytes) = cached_bytes {
        fs::write(&cached_path, bytes).expect("预置缓存 worker");
    }
    if let Some(bytes) = bundled_bytes {
        // 随包损坏用例始终带一份有效缓存，防止修复错误地回退到缓存副本。
        super::worker_ready(worker, root).expect("随包损坏场景的缓存 worker 必须完好");
        let bundled_path = root.join("bundle").join("snap-ocr-worker.exe");
        fs::create_dir_all(bundled_path.parent().expect("随包 worker 有父目录"))
            .expect("创建随包目录");
        fs::write(&bundled_path, bytes).expect("预置随包 worker");
        std::env::set_var("JCHTOOLS_TEST_BUNDLED_WORKER", &bundled_path);
    }

    let component = root.join("component");
    let shared: serde_json::Value =
        serde_json::from_str(include_str!("../../../../resources/markdown-assets.json"))
            .expect("共享清单必须可解析");
    for member in shared["xberg"]["members"].as_array().expect("共享成员清单") {
        let relative = member["path"].as_str().expect("成员相对路径");
        if jchtools_xberg::xberg_runtime::asset_for_scenario(relative, "snapshot") {
            let target = component.join(relative);
            fs::create_dir_all(target.parent().expect("组件成员有父目录")).expect("创建组件目录");
            fs::write(target, b"local-component").expect("预置有效的本地组件成员");
        }
    }
    jchtools_foundation::xberg_settings::save(&component).expect("保存组件目录");
    super::readiness_inference_pack(root).expect("Xberg 前置条件必须就绪");
    let staging = root.join(".staging-review-remediation-worker");
    fs::create_dir_all(&staging).expect("创建初始化暂存目录");
    let error = super::initialize_staged(
        &manifest,
        &AtomicBool::new(false),
        &mut |_| {},
        &staging,
        root,
        &mut super::NetworkDownloader,
    )
    .expect_err("缺失或损坏 worker 的实际初始化必须失败，不得回退其他副本");
    assert!(
        error.contains("重新安装当前 JchTools 包"),
        "初始化失败必须保留重装指引：{error}"
    );
    (error, missing_reason)
}

// P3-18：缺失错误必须保留文件系统的具体原因，不能只剩通用修复文案。
#[test]
fn review_remediation_initialize_reports_missing_worker_reason() {
    let (error, missing_reason) = review_remediation_initialize_worker_failure(None, None);
    assert!(
        error.contains(&missing_reason),
        "初始化必须保留缺失 worker 的具体原因：{error}"
    );
}

// P3-18：大小错误与缺失、摘要错误必须可区分。
#[test]
fn review_remediation_initialize_reports_worker_size_mismatch() {
    let (error, _) = review_remediation_initialize_worker_failure(Some(b"short"), None);
    assert!(
        error.contains("大小 5，预期 14"),
        "初始化必须说明 worker 的实际大小与预期大小：{error}"
    );
}

// P3-18：等长损坏必须落到摘要原因，而不是大小错误或通用文案。
#[test]
fn review_remediation_initialize_reports_worker_digest_mismatch() {
    let (error, _) = review_remediation_initialize_worker_failure(Some(b"healthy-workXr"), None);
    assert!(
        error.contains("SHA256") && error.contains("预期"),
        "初始化必须说明 worker 摘要不匹配：{error}"
    );
}

// P3-18 / O-09：随包 worker 大小错误不得被有效缓存掩盖。
#[test]
fn review_remediation_initialize_reports_bundled_worker_size_without_fallback() {
    let (error, _) =
        review_remediation_initialize_worker_failure(Some(b"healthy-worker"), Some(b"short"));
    assert!(
        error.contains("大小 5，预期 14"),
        "随包 worker 损坏必须保留大小原因，不能回退缓存：{error}"
    );
}

// P3-18 / O-09：随包 worker 等长损坏也不得回退有效缓存。
#[test]
fn review_remediation_initialize_reports_bundled_worker_digest_without_fallback() {
    let (error, _) = review_remediation_initialize_worker_failure(
        Some(b"healthy-worker"),
        Some(b"healthy-workXr"),
    );
    assert!(
        error.contains("SHA256") && error.contains("预期"),
        "随包 worker 损坏必须保留摘要原因，不能回退缓存：{error}"
    );
}

// 覆盖 O-06：worker 校验后的取消不得提交许可证目录或替换已有 notice。
#[test]
fn initialize_cancellation_before_notice_install_preserves_existing_notice() {
    let guard = redirect_component_env();
    std::env::remove_var("JCHTOOLS_TEST_BUNDLED_WORKER");
    let root = guard.root.path();
    let mut manifest = super::load_manifest().expect("解析内置清单");
    let worker = &mut manifest.workers[0];
    let bytes = b"test-worker";
    worker.status = "ok".into();
    worker.size_bytes = bytes.len() as u64;
    worker.sha256 = sha256_bytes(bytes);
    let worker_path = root.join(&worker.install_path);
    fs::create_dir_all(worker_path.parent().expect("worker 有父目录"))
        .expect("创建缓存 worker 目录");
    fs::write(worker_path, bytes).expect("预置可校验的 worker");

    let component = root.join("component");
    let shared: serde_json::Value =
        serde_json::from_str(include_str!("../../../../resources/markdown-assets.json"))
            .expect("共享清单必须可解析");
    for member in shared["xberg"]["members"].as_array().expect("共享成员清单") {
        let relative = member["path"].as_str().expect("成员相对路径");
        if jchtools_xberg::xberg_runtime::asset_for_scenario(relative, "snapshot") {
            let target = component.join(relative);
            fs::create_dir_all(target.parent().expect("组件成员有父目录")).expect("创建组件目录");
            fs::write(target, b"local-component").expect("预置本地组件成员");
        }
    }
    jchtools_foundation::xberg_settings::save(&component).expect("保存组件目录");
    let licenses = root.join("licenses");
    fs::create_dir_all(&licenses).expect("创建原有许可证目录");
    let notice = licenses.join("THIRD_PARTY_NOTICES.md");
    fs::write(&notice, b"existing-notice").expect("预置原有 notice");
    let staging = root.join(".staging-cancel-notice");
    fs::create_dir_all(&staging).expect("创建本轮暂存目录");
    let cancel = AtomicBool::new(false);
    let result = super::initialize_staged(
        &manifest,
        &cancel,
        &mut |message| {
            if message == "复用已校验的工作进程" {
                cancel.store(true, std::sync::atomic::Ordering::Release);
            }
        },
        &staging,
        root,
        &mut super::NetworkDownloader,
    );
    let error = result.expect_err("提交 notice 前取消必须返回取消");
    assert!(error.contains("取消"), "{error}");
    assert_eq!(
        fs::read(notice).expect("读取原有 notice"),
        b"existing-notice"
    );
}

// ── B-1/B-2：staging 清理降级与残留兜底清理（与 markdown 侧同口径）──

// 覆盖 B-1：staging 清理失败时，已成功的关键变更不得被误报为失败（生产
// 触发：防护软件/索引器短暂持有 staging 内文件句柄，如推理组件包
// download.zip 约 291MB 长下载后解包阶段的句柄）。注入方式：staging
// 路径本身是普通文件时 remove_dir_all 确定性失败，无需平台句柄。
#[test]
fn finalize_staging_cleanup_failure_downgrades_to_warning() {
    let root = tempfile::tempdir().expect("创建测试目录");
    let staging = root.path().join(".staging-fake");
    fs::write(&staging, b"not-a-dir").expect("把 staging 预置为文件");
    let mut warnings = Vec::new();
    let mut progress = |message: String| warnings.push(message);

    let result = super::finalize_staging(Ok(()), &staging, &mut progress);

    assert!(result.is_ok(), "清理失败不得吞掉成功结果：{result:?}");
    assert_eq!(
        warnings
            .iter()
            .filter(|message| message.contains("警告：清理初始化临时目录失败"))
            .count(),
        1,
        "应恰好发出一条清理失败警告：{warnings:?}"
    );
}

// 覆盖 B-1 配套不变量：主结果失败时，清理失败不得覆盖原始错误
//（本用例为守护性断言，修复前后都应通过）。
#[test]
fn finalize_staging_keeps_original_error_over_cleanup_failure() {
    let root = tempfile::tempdir().expect("创建测试目录");
    let staging = root.path().join(".staging-fake");
    fs::write(&staging, b"not-a-dir").expect("把 staging 预置为文件");
    let mut progress = |_message: String| {};

    let result = super::finalize_staging(Err("原始失败".to_string()), &staging, &mut progress);

    assert_eq!(
        result.expect_err("原始错误必须保留"),
        "原始失败",
        "清理失败不得覆盖原始错误"
    );
}

// 覆盖 B-2：历史残留的 .staging-* 目录必须在 initialize 入口被兜底清理。
// 残留来源：readiness 提前返回、取消后清理失败、进程崩溃——修复前仅当次
// 收尾清理，入口不清理任何残留（download.zip 残留可达约 291MB）。
// 注入方式：cancel 预置为 true，initialize 在入口清理后即被取消返回，
// 不触网、不建新 staging，本用例只断言入口清理。
#[test]
fn initialize_cleans_stale_staging_residue_at_entry() {
    let guard = redirect_component_env();
    let root = guard.root.path();
    let residue = root.join(".staging-fake");
    fs::create_dir_all(residue.join("xberg-inference")).expect("预置残留目录");
    fs::write(
        residue.join("xberg-inference").join("download.zip"),
        b"residue",
    )
    .expect("预置残留文件");

    let cancel = AtomicBool::new(true);
    let _ = super::initialize(&cancel, |_message: String| {});

    assert!(
        !residue.exists(),
        "残留 staging 目录必须在 initialize 入口被兜底清理（B-2）"
    );
}

// 覆盖 B-2：单个残留目录清理失败（如被防护软件锁定）必须跳过继续，不得
// 影响其余残留清理，也不得让初始化整体失败。注入方式：对残留内文件持有
// 无 FILE_SHARE_DELETE 的句柄，remove_dir_all 稳定失败（目录不涉及改名，
// 句柄方案可行；对照 asset_util 的目录原子替换用例）。cancel 预置为 true
// 使初始化在入口清理后立即取消，避免真实联网。
// 平台门禁原因：共享模式句柄为 Windows 独有注入；无门禁的入口清理用例
// 覆盖主路径（本项目按 P-07 仅支持 Windows，门禁不影响生产平台覆盖）。
#[cfg(windows)]
#[test]
fn initialize_skips_undeletable_stale_staging_and_continues() {
    let guard = redirect_component_env();
    let root = guard.root.path();
    let locked = root.join(".staging-locked");
    fs::create_dir_all(&locked).expect("预置被锁残留目录");
    fs::write(locked.join("inner.txt"), b"x").expect("预置锁定文件");
    let normal = root.join(".staging-normal");
    fs::create_dir_all(&normal).expect("预置普通残留目录");
    fs::write(normal.join("inner.txt"), b"x").expect("预置普通残留文件");
    {
        use std::os::windows::fs::OpenOptionsExt;
        let _handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(locked.join("inner.txt"))
            .expect("持有无共享删除句柄");

        let cancel = AtomicBool::new(true);
        let _ = super::initialize(&cancel, |_message: String| {});
    }
    assert!(!normal.exists(), "普通残留必须被清理");
    assert!(
        locked.exists(),
        "被锁残留必须跳过（清理尽力而为，不阻塞初始化）"
    );
    let remaining: Vec<String> = fs::read_dir(root)
        .expect("枚举资产根")
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .into_string()
                .ok()
                .filter(|name| name.starts_with(".staging-"))
        })
        .collect();
    assert_eq!(
        remaining,
        vec![".staging-locked".to_string()],
        "除被锁残留外不得留下其他 .staging-* 条目"
    );
}

// 覆盖 B-2 扩展：write_expected_tag 写入失败/崩溃残留的
// xberg-inference/.expected-tag-<uuid> 临时文件必须在 initialize 入口被
// 兜底清扫——修复前入口只清 .staging-*，该残留永久滞留；.old-* 备份不在
// 清扫范围（由原子替换/prune 路径管理），必须保留。
#[test]
fn initialize_cleans_stale_expected_tag_residue_at_entry() {
    let guard = redirect_component_env();
    let root = guard.root.path();
    let inference = root.join("xberg-inference");
    fs::create_dir_all(&inference).expect("预置推理组件根目录");
    let residue = inference.join(".expected-tag-deadbeef");
    fs::write(&residue, b"v1\n").expect("预置 expected-tag 残留临时文件");
    let keep_backup = inference.join(".old-deadbeef");
    fs::create_dir_all(&keep_backup).expect("预置旧备份残留");

    let cancel = AtomicBool::new(true);
    let _ = super::initialize(&cancel, |_message: String| {});

    assert!(
        !residue.exists(),
        "残留的 .expected-tag-* 临时文件必须在 initialize 入口被兜底清理（B-2）"
    );
    assert!(
        keep_backup.exists(),
        ".old-* 备份不在初始化入口清扫范围（由原子替换路径管理），必须保留"
    );
}
