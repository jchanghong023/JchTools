//! //! 托盘故障与框选交互回归：消息队列失败、托盘销毁后的任务终态与旧结果恢复。
//! 从 `service.rs` 的内联测试模块原样搬移（仅调整 use 导入与自引用测试名）。

use super::*;
use slint::ComponentHandle;
use std::sync::mpsc;
use std::time::{Duration, Instant};

// P3-31 / O-16 / O-20：在真实 message-only 托盘销毁前后各请求一次截图。
#[test]
#[cfg(feature = "test-hooks")]
fn review_remediation_dead_tray_capture_restores_result_and_allows_exit(
) -> Result<(), Box<dyn std::error::Error>> {
    review_remediation_dead_tray_case(true,
        "service::tests::tray_cases::review_remediation_dead_tray_capture_restores_result_and_allows_exit")
}

// P3-31：真实消息队列满时投递失败，以及销毁后新请求，均独立于排队丢失场景。
#[test]
#[cfg(feature = "test-hooks")]
fn review_remediation_failed_trigger_post_restores_result_and_allows_exit(
) -> Result<(), Box<dyn std::error::Error>> {
    review_remediation_dead_tray_case(false,
        "service::tests::tray_cases::review_remediation_failed_trigger_post_restores_result_and_allows_exit")
}

#[cfg(feature = "test-hooks")]
fn review_remediation_dead_tray_case(
    queued_before_death: bool,
    test_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    const CHILD: &str = "JCHTOOLS_REVIEW_OCR_DEAD_TRAY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let temp = tempfile::tempdir()?;
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, "1")
            .env("JCHTOOLS_TEST_STATE_DIR", temp.path().join("state"))
            .env("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", temp.path().join("ocr"))
            .env_remove("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT")
            .status()?;
        assert!(status.success(), "隔离托盘故障回归失败");
        return Ok(());
    }
    review_remediation_testing_backend(false)?;
    {
        let temp = tempfile::tempdir()?;
        let (events, event_rx) = mpsc::channel();
        let mut native = tray::ReviewTray::start(events.clone()).map_err(std::io::Error::other)?;
        let (mut service, work_rx) = ready_busy_service(temp.path());
        service.commands = events;
        service.tray = native.handle.clone();
        service.phase = ActivityPhase::Idle;
        let result = crate::result_window::ResultWindowHandle::show_for_monitor(
            "P3-31 旧结果",
            (0, 0, 800, 600),
            |_| Ok(()),
        )?;
        let old = result.as_weak();
        service.result = Some(result);
        if queued_before_death {
            service.pipe(&serde_json::json!({"command":"capture"}));
        }
        if !queued_before_death {
            native.fill_message_queue().map_err(std::io::Error::other)?;
            service.pipe(&serde_json::json!({"command":"capture"}));
            assert_eq!(
                service.status()["task"],
                "idle",
                "实际 PostMessageW 失败必须立即结束任务"
            );
            assert!(service.diagnostic_task.is_none());
            assert!(service
                .result
                .as_ref()
                .is_some_and(crate::result_window::ResultWindowHandle::is_visible));
        }
        native.terminate().map_err(std::io::Error::other)?;
        while let Ok(command) = event_rx.try_recv() {
            service.handle(command);
        }
        if !queued_before_death {
            // 句柄已由所属原生线程销毁；确认终止后不得再向可能复用的 HWND 投递。
            service.pipe(&serde_json::json!({"command":"capture"}));
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match event_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(command) => service.handle(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let state = service.pipe(&serde_json::json!({"command":"get-state"}));
        assert_eq!(
            state["task"], "idle",
            "托盘死亡不得永久占用任务（queued={queued_before_death}）：{state}"
        );
        assert!(
            service.diagnostic_task.is_none(),
            "未交付截图必须产生失败终态"
        );
        let window = old.upgrade().ok_or("旧结果不可丢失")?;
        assert!(window.window().is_visible(), "失败必须恢复原先可见结果");
        let root = temp.path().to_path_buf();
        let events = service.commands.clone();
        let cancel = service.cancel.clone();
        let worker = std::thread::spawn(move || super::worker(&root, &work_rx, &events, &cancel));
        // 不调用会弹出原生确认框的 shutdown；消费真实的“确认退出”回执。
        service.handle(Command::ExitDecision(true));
        service.exit_pending = Some(Instant::now() - Duration::from_secs(3));
        service.handle(Command::Pipe(
            serde_json::json!({"command":"get-state"}),
            mpsc::sync_channel(1).0,
            tracing::Span::none(),
        ));
        let stopped = event_rx.recv_timeout(Duration::from_secs(5));
        let (closed_work, _) = mpsc::channel();
        drop(std::mem::replace(&mut service.work, closed_work));
        worker.join().map_err(|_| "安全退出推理线程异常")?;
        let stopped = stopped?;
        assert!(
            matches!(&stopped, Command::WorkerStopped(_)),
            "实际后台停止必须完成，而非仅派发退出指令"
        );
        service.handle(stopped);
        assert_eq!(
            crate::xberg_runtime::background_control(false).map_err(std::io::Error::other)?
                ["running"],
            false
        );
        assert!(service.result.is_none(), "停止回执必须完成服务收尾");
    }
    Ok(())
}

// P3-31：托盘死亡仅结束未交付截图，不能误清已交给推理线程的任务。
#[test]
fn review_remediation_dead_tray_does_not_clear_inference() -> Result<(), Box<dyn std::error::Error>>
{
    review_remediation_testing_backend(false)?;
    let temp = tempfile::tempdir()?;
    let (events, event_rx) = mpsc::channel();
    let mut native = tray::ReviewTray::start(events).map_err(std::io::Error::other)?;
    let (mut service, work_rx) = ready_busy_service(temp.path());
    service.tray = native.handle.clone();
    let image = crate::capture_win::BgrImage::from_vec(8, 8, vec![0; 8 * 8 * 3])
        .map_err(std::io::Error::other)?;
    service.handle(Command::Image(image, (0, 0, 8, 8)));
    assert!(matches!(
        work_rx.recv_timeout(Duration::from_millis(500)),
        Ok(Work::Recognize(..))
    ));
    native.terminate().map_err(std::io::Error::other)?;
    while let Ok(command) = event_rx.try_recv() {
        service.handle(command);
    }
    assert_eq!(
        service.pipe(&serde_json::json!({"command":"get-state"}))["task"],
        "recognizing",
        "托盘故障不能伪造推理终态"
    );
    service.handle(Command::OcrFinished(Ok(None), (0, 0, 8, 8)));
    assert_eq!(service.status()["task"], "idle");
    Ok(())
}

// P3-31 / O-19：已进入框选的正常用户等待不能被托盘故障修复加上期限。
#[test]
fn review_remediation_live_selection_has_no_capture_deadline(
) -> Result<(), Box<dyn std::error::Error>> {
    review_remediation_testing_backend(false)?;
    let temp = tempfile::tempdir()?;
    let (events, event_rx) = mpsc::channel();
    let mut native = tray::ReviewTray::selecting(events).map_err(std::io::Error::other)?;
    let (mut service, _work_rx) = ready_busy_service(temp.path());
    service.tray = native.handle.clone();
    service.phase = ActivityPhase::Idle;
    service.result = Some(crate::result_window::ResultWindowHandle::show_for_monitor(
        "框选前的旧结果",
        (0, 0, 800, 600),
        |_| Ok(()),
    )?);
    service.pipe(&serde_json::json!({"command":"capture"}));
    native.wait_selection().map_err(std::io::Error::other)?;
    let wait_until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < wait_until {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(command) => service.handle(command),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => return Err(error.into()),
        }
        assert_eq!(
            service.pipe(&serde_json::json!({"command":"get-state"}))["task"],
            "recognizing",
            "用户仍在选择，不能超时伪造截图失败"
        );
        assert!(service.old_result_visible, "正常框选中旧结果应隐藏暂存");
    }
    native.finish_selection();
    service.handle(event_rx.recv_timeout(Duration::from_secs(2))?);
    assert_eq!(service.status()["task"], "idle");
    assert!(service
        .result
        .as_ref()
        .is_some_and(crate::result_window::ResultWindowHandle::is_visible));
    native.terminate().map_err(std::io::Error::other)?;
    Ok(())
}

// 覆盖 O-20/O-22/O-29：关闭结果释放组件引用，隐藏暂存则保留以便失败恢复。
#[test]
fn closed_result_is_released_but_hidden_old_result_is_retained(
) -> Result<(), Box<dyn std::error::Error>> {
    slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("software".into()),
            ..Default::default()
        },
    )))?;
    let temp = tempfile::tempdir()?;
    let (mut service, _work_rx) = ready_busy_service(temp.path());
    service.phase = ActivityPhase::Idle;
    let result = crate::result_window::ResultWindowHandle::show_for_monitor(
        "旧结果",
        (10, 20, 1920, 1080),
        |_| Ok(()),
    )?;
    let weak = result.as_weak();
    service.result = Some(result);
    service.hide_old();
    service.release_closed_result();
    assert!(service.result.is_some(), "O-20 暂存不能当作关闭释放");
    service.restore_old();
    {
        let window = weak.upgrade().ok_or("结果组件意外释放")?;
        assert!(window.window().is_visible(), "失败恢复旧结果");
        window.invoke_close_requested();
    }
    service.release_closed_result();
    assert!(service.result.is_none());
    assert!(
        weak.upgrade().is_none(),
        "O-22/O-29 关闭后释放组件及其惰性文本缓存"
    );
    Ok(())
}
