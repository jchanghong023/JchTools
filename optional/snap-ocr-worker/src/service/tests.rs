//! `service` 测试公共根：共享的服务构造夹具与按域拆分的测试子模块。
//! 从 `service.rs` 的内联测试模块搬移；子模块内测试体保持原样。

use crate::service::autostart::{
    autostart_enabled, autostart_registry_path, autostart_update_required, classify_autostart,
    read_autostart_value, set_autostart, set_autostart_value, AutostartValue, RegCloseKey,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, StoredAutostartValue, HKEY_CURRENT_USER,
    VALUE,
};
use crate::service::clipboard::{
    copy_text, CloseClipboard, GlobalLock, GlobalUnlock, OpenClipboard,
};
#[cfg(feature = "test-hooks")]
use crate::service::engine_session;
use crate::service::inference::{
    ocr_error, redact_user_path, reload_plan, reuse_inference, stop_background_with_retry,
    verify_font_assets, verify_manifest_file, warm_up_failure, worker, ReloadPlan,
    WORKER_STOP_ATTEMPTS,
};
use crate::service::protocol;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use crate::service::{
    run_service_inner, selection_cancel_is_current, tray, ActivityPhase, Command, LoadFailure,
    ModelState, OcrError, PendingHotkey, Service, Settings, Work,
};

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes,
)]
mod lifecycle;

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes,
)]
mod tray_cases;

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes,
)]
mod hotkey;

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes,
)]
mod model_state;

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes,
)]
mod inference_assets;

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes,
)]
mod autostart;

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes,
)]
mod clipboard;

thread_local! {
    pub(super) static REVIEW_F37_STARTUP_CHECKPOINT: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// 构造可直接喂 [`Command::OcrFinished`] 的最小服务（模型就绪、任务进行中），
/// 返回服务与 Work 接收端（供断言后台重载指令）。
fn ready_busy_service(root: &Path) -> (Service, mpsc::Receiver<Work>) {
    let (commands, _) = mpsc::channel();
    let (work, work_rx) = mpsc::channel();
    let service = Service {
        root: root.to_path_buf(),
        commands,
        self_weak: std::rc::Weak::new(),
        settings: Settings {
            hotkey: "Ctrl+Alt+O".into(),
            main_exe: None,
            warning: None,
        },
        tray: tray::TrayHandle::for_test(),
        work,
        cancel: Arc::new(AtomicBool::new(false)),
        cancel_generation: Arc::new(AtomicU64::new(0)),
        model: ModelState::Ready,
        model_error: None,
        pending_image: None,
        phase: ActivityPhase::Processing,
        diagnostic_task: None,
        result: None,
        old_result_visible: false,
        progress: None,
        settings_window: None,
        last_theme_sync: Instant::now(),
        exit_pending: None,
        exit_confirming: None,
        exit_stop_sent: false,
        stop_retry_at: None,
        pending_hotkey: None,
        gui_tasks: std::collections::HashMap::new(),
    };
    (service, work_rx)
}

fn review_remediation_testing_backend(threading: bool) -> Result<(), Box<dyn std::error::Error>> {
    slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            threading,
            renderer_name: Some("software".into()),
            ..Default::default()
        },
    )))?;
    Ok(())
}
