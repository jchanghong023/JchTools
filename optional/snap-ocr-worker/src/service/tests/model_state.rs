//! //! 模型状态机回归：取消/超时/进程退出/资产失效等终态对模型与重试入口的影响。
//! 从 `service.rs` 的内联测试模块原样搬移（仅调整 use 导入与自引用测试名）。

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use std::time::Instant;

// 覆盖 XB-08/O-19（E-3 回归）：取消走「终止进程」路径后，推理连接已死亡，
// 服务必须在取消结束的同一时刻触发后台重载（置回加载中并发 Work::Load），
// 让取消后的下一次截图按 O-13「加载中允许截图」等待模型，而不是把新请求
// 排在已被放弃的旧请求之后。
#[test]
fn cancelled_recognition_triggers_model_reload() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (mut service, work_rx) = ready_busy_service(temp.path());
    service.handle(Command::OcrFinished(
        Err(OcrError::Cancelled),
        (0, 0, 20, 20),
    ));
    assert_eq!(
        service.phase,
        ActivityPhase::Idle,
        "取消后当前任务应立即结束"
    );
    assert_eq!(
        service.model,
        ModelState::Loading,
        "取消终止子进程后必须触发重载（XB-08：旧连接已死，O-13：重载排队）"
    );
    assert_eq!(service.status()["model"], "loading");
    assert!(
        matches!(
            work_rx.recv_timeout(Duration::from_secs(1)),
            Ok(Work::Load(_))
        ),
        "应向推理线程发出 Work::Load 重新 spawn 子进程"
    );
    Ok(())
}

// 覆盖 O-13/O-20/O-30：单次推理错误只结束该任务，已预热的模型仍供下次截图。
#[test]
fn inference_failure_does_not_unload_ready_model() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (commands, _) = mpsc::channel();
    let (work, _) = mpsc::channel();
    let mut service = Service {
        root: temp.path().to_path_buf(),
        commands,
        self_weak: std::rc::Weak::new(),
        settings: Settings {
            hotkey: "Ctrl+Alt+O".into(),
            copy_hotkey: DEFAULT_COPY_HOTKEY.into(),
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
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
        copy_task: None,
        copy_jobs: std::collections::HashSet::new(),
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
        pending_copy_hotkey: None,
        gui_tasks: std::collections::HashMap::new(),
    };
    service.handle(Command::OcrFinished(
        Err(OcrError::Backend("一次性推理错误".into())),
        (0, 0, 20, 20),
    ));
    assert_eq!(service.model, ModelState::Ready);
    assert!(service.model_error.is_none());
    assert_eq!(service.phase, ActivityPhase::Idle);
    assert_eq!(service.status()["model"], "ready");
    Ok(())
}

#[test]
fn asset_invalid_downgrades_ready_model_and_reenables_retry(
) -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (mut service, _work_rx) = ready_busy_service(temp.path());
    service.handle(Command::OcrFinished(
        Err(OcrError::ModelFailure("模型资产失效".into())),
        (0, 0, 20, 20),
    ));
    assert_eq!(service.model, ModelState::Error);
    assert!(service.model_error.is_some());
    assert_eq!(service.phase, ActivityPhase::Idle);
    service.handle(Command::RetryModel);
    assert_eq!(service.model, ModelState::Loading);
    Ok(())
}

#[test]
fn communication_failure_downgrades_ready_model_and_reenables_retry(
) -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (mut service, _work_rx) = ready_busy_service(temp.path());
    service.handle(Command::OcrFinished(
        Err(OcrError::Communication("共享代理响应线程退出".into())),
        (0, 0, 20, 20),
    ));
    assert_eq!(service.model, ModelState::Error);
    assert!(service.model_error.is_some());
    service.handle(Command::RetryModel);
    assert_eq!(service.model, ModelState::Loading);
    Ok(())
}

#[test]
fn cancelled_image_arriving_late_does_not_start_recognition(
) -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (mut service, work_rx) = ready_busy_service(temp.path());
    service.cancel.store(true, Ordering::Release);
    let image = crate::capture_win::BgrImage::from_vec(8, 8, vec![0; 8 * 8 * 3])
        .map_err(std::io::Error::other)?;
    service.handle(Command::Image(image, (0, 0, 8, 8)));
    assert_eq!(service.phase, ActivityPhase::Idle);
    assert!(matches!(work_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    Ok(())
}

// 覆盖 O-13：未配置与失败的加载结果分别映射到未初始化与错误状态，
// 两者都保留各自的界面入口（初始化 / 重试），不冒称就绪。
#[test]
fn load_failures_map_to_distinct_model_states() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (commands, _commands_rx) = mpsc::channel();
    let (work, _work_rx) = mpsc::channel();
    let mut service = Service {
        root: temp.path().to_path_buf(),
        commands,
        self_weak: std::rc::Weak::new(),
        settings: Settings {
            hotkey: "Ctrl+Alt+O".into(),
            copy_hotkey: DEFAULT_COPY_HOTKEY.into(),
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            main_exe: None,
            warning: None,
        },
        tray: tray::TrayHandle::for_test(),
        work,
        cancel: Arc::new(AtomicBool::new(false)),
        cancel_generation: Arc::new(AtomicU64::new(0)),
        model: ModelState::Loading,
        model_error: None,
        pending_image: None,
        phase: ActivityPhase::Idle,
        copy_task: None,
        copy_jobs: std::collections::HashSet::new(),
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
        pending_copy_hotkey: None,
        gui_tasks: std::collections::HashMap::new(),
    };
    service.handle(Command::ModelLoaded(Err(
        super::LoadFailure::NotConfigured("推理组件未配置".into()),
    )));
    assert_eq!(service.model, ModelState::Uninitialized);
    assert!(service
        .model_error
        .as_deref()
        .is_some_and(|reason| reason.contains("推理组件未配置")));

    // 重试入口把状态置回加载中（Work::Load 保留在通道里由真实线程消费）。
    service.handle(Command::RetryModel);
    assert_eq!(service.model, ModelState::Loading);
    service.handle(Command::ModelLoaded(Err(super::LoadFailure::Failed(
        "推理组件不完整：缺少 models/snapshot-ocr/rec.onnx".into(),
    ))));
    assert_eq!(service.model, ModelState::Error);
    assert_eq!(service.status()["model"], "error");
    Ok(())
}

// 覆盖 O-13/O-30（E-2 回归）：识别中推理子进程退出属于连接死亡，必须把
// 模型从就绪降级为错误并保留重试入口；对照用例
// inference_failure_does_not_unload_ready_model 证明单次 Backend 失败不降级，
// 两个分类不得互相吞并。
#[test]
fn process_exit_downgrades_ready_model_and_reenables_retry(
) -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (mut service, _work_rx) = ready_busy_service(temp.path());
    service.handle(Command::OcrFinished(
        Err(OcrError::ProcessExited(
            "推理子进程已退出（退出码 3）".into(),
        )),
        (0, 0, 20, 20),
    ));
    assert_eq!(
        service.model,
        ModelState::Error,
        "子进程死亡后模型必须降级，不得继续冒称就绪"
    );
    assert!(service
        .model_error
        .as_deref()
        .is_some_and(|reason| reason.contains("推理子进程已退出")));
    assert_eq!(service.phase, ActivityPhase::Idle);
    assert_eq!(service.status()["model"], "error");
    // Error 状态下重试入口恢复可用（O-13）：retry 把状态置回加载中。
    service.handle(Command::RetryModel);
    assert_eq!(service.model, ModelState::Loading);
    Ok(())
}

// 覆盖 O-13/O-30（修复2回归，服务侧）：识别超时是连接级故障，模型从就绪
// 降级为错误并保留重试入口；与用户取消（自动后台重载）区分——超时不得
// 自动发 Work::Load 循环重试挂起的推理环境。
#[test]
fn recognition_timeout_downgrades_model_and_reenables_retry(
) -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (mut service, work_rx) = ready_busy_service(temp.path());
    service.handle(Command::OcrFinished(
        Err(OcrError::TimedOut(
            "当前截图识别超时；共享引擎及其他功能保持运行".into(),
        )),
        (0, 0, 20, 20),
    ));
    assert_eq!(
        service.phase,
        ActivityPhase::Idle,
        "超时后当前任务应立即结束"
    );
    assert_eq!(
        service.model,
        ModelState::Error,
        "超时是故障：模型必须降级为错误，不得冒称就绪"
    );
    assert!(service
        .model_error
        .as_deref()
        .is_some_and(|reason| reason.contains("识别超时")));
    assert_eq!(service.status()["model"], "error");
    // 与取消的自动重载区分：超时不自动发 Work::Load（对照用例
    // cancelled_recognition_triggers_model_reload 断言取消会发）。
    assert!(
        matches!(work_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "超时不得自动触发后台重载（故障降级，非用户意图）"
    );
    // Error 状态下重试入口恢复可用（O-13）：retry 把状态置回加载中。
    service.handle(Command::RetryModel);
    assert_eq!(service.model, ModelState::Loading);
    Ok(())
}

#[test]
fn image_copy_dispatch_bypasses_every_model_state() -> Result<(), Box<dyn std::error::Error>> {
    review_remediation_testing_backend(false)?;
    let temp = tempfile::tempdir()?;
    for model in [
        ModelState::Uninitialized,
        ModelState::Loading,
        ModelState::Error,
        ModelState::Ready,
    ] {
        let (mut service, work_rx) = ready_busy_service(temp.path());
        let (commands, events) = mpsc::channel();
        service.commands = commands;
        service.model = model;
        service.phase = ActivityPhase::Selecting;
        service.copy_task = Some(CopyTask {
            generation: 9,
            // 正上限不可产出 PNG：证明进入真实编码分支，但绝不写系统剪贴板。
            max_image_bytes: 1,
            sequence: 0,
            valid: Arc::new(std::sync::Mutex::new(true)),
        });
        let image = crate::capture_win::BgrImage::from_vec(8, 8, vec![0; 8 * 8 * 3])
            .map_err(std::io::Error::other)?;
        service.handle(Command::Image(image, (0, 0, 8, 8)));
        assert_eq!(service.status()["task"], "copying");
        assert_eq!(service.model, model);
        assert!(service.pending_image.is_none());
        assert!(matches!(work_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let event = events.recv_timeout(Duration::from_secs(5))?;
        assert!(matches!(&event, Command::CopyFinished(9, Err(_))));
        service.handle(event);
        assert_eq!(service.phase, ActivityPhase::Idle);
        assert!(service.copy_jobs.is_empty());
        assert_eq!(service.model, model);
    }
    Ok(())
}

#[test]
fn cancelled_copy_receipt_cannot_finish_new_task() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (mut service, work_rx) = ready_busy_service(temp.path());
    let valid = Arc::new(std::sync::Mutex::new(true));
    service.phase = ActivityPhase::Copying;
    service.copy_task = Some(CopyTask {
        generation: 1,
        max_image_bytes: 200_000,
        sequence: 0,
        valid: Arc::clone(&valid),
    });
    service.copy_jobs.insert(1);
    service.cancel_recognition();
    assert!(!*valid.lock().map_err(|_| "取消状态锁损坏")?);
    assert_eq!(service.phase, ActivityPhase::Idle);
    service.phase = ActivityPhase::Selecting;
    let next_valid = Arc::new(std::sync::Mutex::new(true));
    service.copy_task = Some(CopyTask {
        generation: 2,
        max_image_bytes: 300_000,
        sequence: 0,
        valid: Arc::clone(&next_valid),
    });
    service.handle(Command::CopyFinished(1, Ok(100_000)));
    assert_eq!(service.status()["task"], "selecting");
    assert_eq!(
        service.copy_task.as_ref().map(|task| task.generation),
        Some(2)
    );
    assert!(*next_valid.lock().map_err(|_| "新任务状态锁损坏")?);
    assert!(service.copy_jobs.is_empty());
    assert!(matches!(work_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    Ok(())
}
