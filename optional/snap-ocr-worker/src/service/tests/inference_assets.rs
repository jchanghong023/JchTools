//! //! 推理资产与加载决策回归：内置字体清单、预热失败分类与重载复用判定。
//! 从 `service.rs` 的内联测试模块原样搬移（仅调整 use 导入与自引用测试名）。

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[test]
fn stop_background_retries_are_bounded_on_error() {
    let mut calls = 0;
    let result = super::stop_background_with_retry(|| {
        calls += 1;
        Err("代理不可用".to_string())
    });
    assert!(result.is_err());
    assert_eq!(calls, super::WORKER_STOP_ATTEMPTS);
}

#[test]
// 覆盖 O-05/O-21、XB-25：新安装没有字体缓存也能离线就绪。
fn font_readiness_works_without_external_cache() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    assert!(
        super::verify_font_assets(temp.path()).is_ok(),
        "内置字体不得要求先初始化缓存"
    );
    assert!(
        temp.path().read_dir()?.next().is_none(),
        "字体检查不得生成缓存文件"
    );
    Ok(())
}

#[test]
fn font_readiness_rejects_corrupt_embedded_assets() {
    assert!(matches!(
        super::verify_manifest_file(b"bad", 4, &"00".repeat(32), "截图字体"),
        Err(super::LoadFailure::Failed(message)) if message.contains("大小")
    ));
    assert!(matches!(
        super::verify_manifest_file(b"bad", 3, &"00".repeat(32), "截图字体"),
        Err(super::LoadFailure::Failed(message)) if message.contains("摘要")
    ));
}

// 覆盖 O-09/O-13：共享模型已就绪不豁免本服务字体资产校验。
#[test]
fn reuse_uses_verified_embedded_font_without_cache() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let outcome = super::reuse_inference(temp.path(), temp.path());
    assert!(
        outcome.is_ok(),
        "复用常驻模型应直接校验内置字体，无需外部缓存"
    );
    Ok(())
}

#[test]
fn stale_selection_cancel_generation_cannot_cancel_new_task() {
    let cancel = AtomicBool::new(true);
    let generation = AtomicU64::new(2);
    assert!(super::selection_cancel_is_current(&cancel, &generation, 2));
    generation.store(3, Ordering::Release);
    cancel.store(false, Ordering::Release);
    assert!(!super::selection_cancel_is_current(&cancel, &generation, 2));
}

// 覆盖 SNAP-05/SNAP-15（修复1回归）：预热失败分类优先读结构化 error_kind。
// 真实 Xberg 错误文本是英文（"snapshot model asset missing"），旧实现只按
// 中文「模型」子串匹配必然落空——kind=asset_invalid（模型/资产加载失败）
// 必须命中「模型加载失败」类文案；kind 缺失时回退子串启发式保持兼容。
#[test]
fn warm_up_failure_classifies_by_error_kind() {
    // kind 命中：英文消息也必须分类为模型加载失败（修复前落入「组件预热失败」）。
    let by_kind = super::warm_up_failure(Some("asset_invalid"), "snapshot model asset missing");
    assert!(
        by_kind.contains("推理模型加载失败"),
        "asset_invalid 应分类为模型加载失败，实际 {by_kind}"
    );
    assert!(
        by_kind.contains("snapshot model asset missing"),
        "原始错误摘要应保留，实际 {by_kind}"
    );
    // 其余 kind（input_invalid/cancelled/internal 等）：组件预热失败。
    for kind in ["input_invalid", "cancelled", "internal"] {
        let message = super::warm_up_failure(Some(kind), "boom");
        assert!(
            message.contains("推理组件预热失败"),
            "kind={kind} 应分类为组件预热失败，实际 {message}"
        );
    }
    // kind 缺失回退子串启发式（兼容旧版 Xberg）：中文「模型」仍命中。
    let legacy = super::warm_up_failure(None, "模型文件损坏");
    assert!(legacy.contains("推理模型加载失败"), "实际 {legacy}");
    // kind 缺失且英文消息：预热失败（修复前的既行为，非 kind 场景不改变）。
    let fallback = super::warm_up_failure(None, "warmup crashed");
    assert!(fallback.contains("推理组件预热失败"), "实际 {fallback}");
}

// 覆盖 S8-02：协作取消（XB-14）后引擎仍就绪时免重预热——重载决策只认
// snapshot_state=Ready 为可复用；未配置/探测失败/非就绪仍完整重载。
#[test]
fn reload_plan_reuses_only_ready_engine() {
    use super::reload_plan;
    use crate::shared_xberg::{ClientError, SnapshotState};
    assert_eq!(
        reload_plan(Some(&Ok(SnapshotState::Ready))),
        super::ReloadPlan::Reuse,
        "引擎仍就绪时取消后不得重复预热模型（S8-02）；字体仍需校验（O-09）"
    );
    let uninitialized = Ok(SnapshotState::Uninitialized);
    let loading = Ok(SnapshotState::Loading);
    let errored = Ok(SnapshotState::Error("上次加载失败".into()));
    let unreachable = Err(ClientError::Io("代理不可用".into()));
    for probe in [
        Some(&uninitialized),
        Some(&loading),
        Some(&errored),
        Some(&unreachable),
        None,
    ] {
        assert_eq!(
            reload_plan(probe),
            super::ReloadPlan::Full,
            "非就绪/探测失败必须完整重载（O-13）"
        );
    }
}

#[test]
fn xberg_validation_errors_hide_the_user_directory() {
    let directory = Path::new(r"C:\Users\example\Documents\xberg");
    let error = format!(
        "Xberg 资产 models/snapshot-ocr/rec.onnx 缺失（目录 {}）",
        directory.display()
    );
    assert_eq!(
        super::redact_user_path(&error, directory),
        "Xberg 资产 models/snapshot-ocr/rec.onnx 缺失（目录 共享推理目录）"
    );
}
