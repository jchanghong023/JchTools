//! ACP 模型服务界面行为用例（自 gui.rs 的 gui_tests 拆分）。
#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
use super::gui_tests::*;
#[cfg(windows)]
use super::tests_acp_control::acp_control_test;
use super::*;
use slint::Model;

// 覆盖 AH-08/AH-09/AH-12：GUI 消费真实类型快照，不将 saved 冒充 running。
#[test]
fn acp_status_preserves_running_config_and_edited_draft() {
    with_gui(|app| {
        let running = ServiceConfig {
            executable: "agent.exe".into(),
            arguments: vec!["含 空格".into()],
            port: 8765,
        };
        let saved = ServiceConfig {
            port: 8766,
            ..running.clone()
        };
        let status = ServiceStatus {
            phase: jchtools_acp_api::acp_api::ServicePhase::Ready,
            saved_config: Some(saved),
            running_config: Some(running),
            service_pid: Some(123),
            agent_pid: Some(456),
            executing: 2,
            waiting: 3,
            error: None,
        };
        apply_acp_status(&app.ui, &app.state, &status);
        assert!(app.ui.get_acp_ready());
        assert!(app.ui.get_acp_pending_apply());
        assert_eq!(app.ui.get_acp_port().as_str(), "8766");
        assert!(app.ui.get_acp_saved_config().contains("8766"));
        assert!(app.ui.get_acp_running_config().contains("8765"));
        assert_eq!(app.ui.get_acp_service_pid().as_str(), "123");
        assert_eq!(app.ui.get_acp_agent_pid().as_str(), "456");
        assert_eq!(app.ui.get_acp_executing(), 2);
        assert_eq!(app.ui.get_acp_waiting(), 3);
        app.ui.set_acp_port("invalid draft".into());
        app.ui.invoke_acp_config_edited();
        apply_acp_status(&app.ui, &app.state, &status);
        assert_eq!(app.ui.get_acp_port().as_str(), "invalid draft");
    })
    .unwrap();
}

// 覆盖 AH-09：保存完成后的迟到旧快照不得回退已保存值，单字段编辑也不得混入旧配置。
#[test]
fn acp_late_snapshot_after_save_keeps_saved_config_and_unedited_fields() {
    with_gui(|app| {
        let old = ServiceConfig {
            executable: "old-agent.exe".into(),
            arguments: vec!["--old".into()],
            port: 8765,
        };
        let saved = ServiceConfig {
            executable: "new-agent.exe".into(),
            arguments: vec!["--new".into(), "value with spaces".into()],
            port: 8766,
        };
        let old_snapshot = ServiceStatus {
            phase: jchtools_acp_api::acp_api::ServicePhase::Ready,
            saved_config: Some(old.clone()),
            running_config: Some(old.clone()),
            service_pid: Some(123),
            ..ServiceStatus::default()
        };
        let saved_snapshot = ServiceStatus {
            saved_config: Some(saved.clone()),
            ..old_snapshot.clone()
        };
        let sender = app.state.borrow().acp_sender.clone();
        let generation = app.state.borrow().acp_observation_generation.clone();
        let old_generation = generation.load(Ordering::Acquire);
        sender
            .send(AcpMessage::Snapshot(
                old_generation,
                Ok(old_snapshot.clone()),
            ))
            .unwrap();
        apply_acp_messages(&app.ui, &app.state);
        app.ui.set_acp_executable(saved.executable.clone().into());
        app.ui
            .set_acp_arguments(acp_settings::format_arguments(&saved.arguments).into());
        app.ui.set_acp_port(saved.port.to_string().into());
        app.ui.invoke_acp_config_edited();
        app.ui.set_acp_request_pending(true);
        let request = AcpRequest {
            id: 1,
            operation: AcpOperation::Save,
        };
        app.state.borrow_mut().acp_pending_request = Some(request);
        sender
            .send(AcpMessage::Completed(request, Ok(saved_snapshot.clone())))
            .unwrap();
        apply_acp_messages(&app.ui, &app.state);
        assert!(!app.ui.get_acp_request_pending());
        assert_eq!(app.ui.get_acp_executable().as_str(), saved.executable);
        assert_eq!(app.ui.get_acp_port().as_str(), "8766");

        // 旧读取已在保存前取到 A，但直到保存 B 的完成消息落地之后才送达。
        sender
            .send(AcpMessage::Snapshot(old_generation, Ok(old_snapshot)))
            .unwrap();
        apply_acp_messages(&app.ui, &app.state);
        let after_late_snapshot = (
            app.ui.get_acp_executable().to_string(),
            app.ui.get_acp_arguments().to_string(),
            app.ui.get_acp_port().to_string(),
            app.ui.get_acp_saved_config().to_string(),
            app.ui.get_acp_pending_apply(),
        );

        // 用户此后只改端口；正确的 B 快照不得把其余两个输入永久锁在 A。
        app.ui.set_acp_port("8767".into());
        app.ui.invoke_acp_config_edited();
        sender
            .send(AcpMessage::Snapshot(
                generation.load(Ordering::Acquire),
                Ok(saved_snapshot),
            ))
            .unwrap();
        apply_acp_messages(&app.ui, &app.state);
        assert_eq!(app.ui.get_acp_port().as_str(), "8767");
        assert_eq!(
            app.ui.get_acp_executable().as_str(),
            saved.executable,
            "AH-09：只编辑端口时启动程序仍应是刚保存的 B"
        );
        assert_eq!(
            app.ui.get_acp_arguments().as_str(),
            acp_settings::format_arguments(&saved.arguments),
            "AH-09：只编辑端口时启动参数仍应是刚保存的 B"
        );
        assert_eq!(after_late_snapshot.0, saved.executable);
        assert_eq!(
            after_late_snapshot.1,
            acp_settings::format_arguments(&saved.arguments)
        );
        assert_eq!(after_late_snapshot.2, "8766");
        assert!(
            after_late_snapshot.3.contains("new-agent.exe")
                && after_late_snapshot.3.contains("8766"),
            "AH-09：迟到 A 不能将已保存配置展示回退到 A"
        );
        assert!(
            after_late_snapshot.4,
            "AH-09：运行 A、已保存 B 的待应用入口不能被旧快照移除"
        );
        assert!(app.ui.get_acp_running_config().contains(&old.executable));
        assert!(app.ui.get_acp_saved_config().contains(&saved.executable));
        assert!(app.state.borrow().acp_observer.is_none());
    })
    .unwrap();
}

// 覆盖 AH-08/AH-09：自动连接错误不能被无错误的 stopped 快照擦除，也不能消费保存门禁。
#[test]
fn acp_connection_error_survives_stopped_snapshot_while_save_is_pending() {
    with_gui(|app| {
        let sender = app.state.borrow().acp_sender.clone();
        let generation = app.state.borrow().acp_observation_generation.clone();
        let error = "Agent 启动失败：找不到 configured-agent.exe";
        app.ui.set_acp_request_pending(true);
        app.ui.set_acp_operation("正在校验并保存配置…".into());
        sender
            .send(AcpMessage::Completed(
                AcpRequest::CONNECT,
                Err(error.into()),
            ))
            .unwrap();
        apply_acp_messages(&app.ui, &app.state);
        assert!(format!(
            "{}{}",
            app.ui.get_acp_action_error().as_str(),
            app.ui.get_acp_service_error().as_str()
        )
        .contains(error));
        assert!(app.ui.get_acp_request_pending());
        sender
            .send(AcpMessage::Snapshot(
                generation.load(Ordering::Acquire),
                Ok(ServiceStatus {
                    phase: jchtools_acp_api::acp_api::ServicePhase::Stopped,
                    error: None,
                    ..ServiceStatus::default()
                }),
            ))
            .unwrap();
        apply_acp_messages(&app.ui, &app.state);
        assert_eq!(app.ui.get_acp_phase().as_str(), "stopped");

        // 走真实回调验证保存门禁，不能因连接完成或状态轮询又派发一次保存。
        assert!(
            app.ui.get_acp_request_pending(),
            "AH-09：连接完成和状态快照不能解除正在保存的门禁"
        );
        app.ui.invoke_acp_save_config();
        assert!(app.ui.get_acp_request_pending());
        assert_eq!(app.ui.get_acp_operation().as_str(), "正在校验并保存配置…");
        assert!(app.state.borrow().acp_observer.is_none());
        assert!(!app.ui.get_acp_ready());
        assert!(
            format!(
                "{}{}",
                app.ui.get_acp_action_error().as_str(),
                app.ui.get_acp_service_error().as_str()
            )
            .contains(error),
            "AH-08：连接失败详情须保留，不能被 stopped/error=None 轮询抹去"
        );
        // 真正就绪后才清空连接错误，观察回包仍不能消费正在保存的门禁。
        sender
            .send(AcpMessage::Snapshot(
                generation.load(Ordering::Acquire),
                Ok(ServiceStatus {
                    phase: jchtools_acp_api::acp_api::ServicePhase::Ready,
                    service_pid: Some(123),
                    ..ServiceStatus::default()
                }),
            ))
            .unwrap();
        apply_acp_messages(&app.ui, &app.state);
        assert!(app.ui.get_acp_ready());
        assert!(app.ui.get_acp_service_error().is_empty());
        assert!(app.ui.get_acp_request_pending());
        assert_eq!(app.ui.get_acp_operation().as_str(), "正在校验并保存配置…");
    })
    .unwrap();
}

// 覆盖 AH-08/AH-09/AH-10：已保存有效配置的失败后台应提供恢复入口；主动退出保持禁用。
#[test]
fn acp_failed_background_without_running_config_offers_apply_but_respects_explicit_stop() {
    with_gui(|app| {
        let sender = app.state.borrow().acp_sender.clone();
        let generation = app.state.borrow().acp_observation_generation.clone();
        let failed = ServiceStatus {
            phase: jchtools_acp_api::acp_api::ServicePhase::Error,
            saved_config: Some(ServiceConfig {
                executable: "configured-agent.exe".into(),
                arguments: vec!["--mode".into(), "acp".into()],
                port: 8765,
            }),
            running_config: None,
            service_pid: Some(123),
            error: Some("ACP 初始化失败".into()),
            ..ServiceStatus::default()
        };
        sender
            .send(AcpMessage::Snapshot(
                generation.load(Ordering::Acquire),
                Ok(failed.clone()),
            ))
            .unwrap();
        apply_acp_messages(&app.ui, &app.state);
        let recovery_visible = app.ui.get_acp_pending_apply();
        assert!(app.ui.get_acp_status_known());
        assert!(!app.ui.get_acp_request_pending());
        assert!(!app.ui.get_acp_explicit_stopped());
        assert_eq!(app.ui.get_confirm_kind(), 0);
        assert_eq!(app.ui.get_acp_phase().as_str(), "error");
        assert!(!app.ui.get_acp_ready());
        assert_eq!(app.ui.get_acp_running_config().as_str(), "无");
        assert_eq!(app.ui.get_acp_service_error().as_str(), "ACP 初始化失败");
        assert!(app
            .ui
            .get_acp_saved_config()
            .contains("configured-agent.exe"));

        // 隔离地恢复「本 GUI 已主动退出」会话，不触发真实 Stop 或启动后台。
        app.state.borrow_mut().acp_explicit_stopped = true;
        app.ui.set_acp_explicit_stopped(true);
        let request = AcpRequest {
            id: 1,
            operation: AcpOperation::Stop,
        };
        app.state.borrow_mut().acp_pending_request = Some(request);
        sender
            .send(AcpMessage::Completed(
                request,
                Ok(ServiceStatus {
                    phase: jchtools_acp_api::acp_api::ServicePhase::Stopped,
                    service_pid: None,
                    error: None,
                    ..failed.clone()
                }),
            ))
            .unwrap();
        apply_acp_messages(&app.ui, &app.state);
        // 即使后来又看到失败后台，快照不得撤销用户退出意图并自动恢复。
        sender
            .send(AcpMessage::Snapshot(
                generation.load(Ordering::Acquire),
                Ok(failed),
            ))
            .unwrap();
        apply_acp_messages(&app.ui, &app.state);
        assert!(app.state.borrow().acp_explicit_stopped);
        assert!(app.ui.get_acp_explicit_stopped());
        assert!(!app.ui.get_acp_pending_apply());
        app.ui.invoke_acp_apply_and_restart();
        assert!(app.state.borrow().acp_explicit_stopped);
        assert!(app.ui.get_acp_explicit_stopped());
        assert!(!app.ui.get_acp_request_pending());
        assert_eq!(app.ui.get_acp_operation().as_str(), "退出已完成");
        assert!(app.state.borrow().acp_observer.is_none());
        assert!(
            recovery_visible,
            "AH-09：后台存在但 running_config=None 时，有效已保存配置必须提供应用恢复入口"
        );
    })
    .unwrap();
}

// 覆盖 AH-10：退出使用独立确认类型，未确认不能触发 stop 或改变会话标记。
#[test]
fn acp_stop_requires_its_own_confirmation_without_overriding_existing_modal() {
    with_gui(|app| {
        app.ui.set_acp_status_known(true);
        app.ui.set_acp_service_pid("123".into());
        app.ui.set_confirm_kind(3);
        app.ui.invoke_acp_request_stop();
        assert_eq!(app.ui.get_confirm_kind(), 3);
        app.ui.set_confirm_kind(0);
        app.ui.invoke_acp_request_stop();
        assert_eq!(app.ui.get_confirm_kind(), 5);
        assert!(app.state.borrow().acp_observer.is_none());
        assert!(!app.state.borrow().acp_explicit_stopped);
        app.ui.set_confirm_kind(0);
        app.ui.invoke_confirmed(5);
        assert!(!app.state.borrow().acp_explicit_stopped);
        assert!(app.state.borrow().acp_observer.is_none());
    })
    .unwrap();
}

// AH-09/AH-10：抢占后的 UI 门禁属于具体 Stop 请求，旧操作和重复结果都不得消费。
#[test]
fn acp_superseded_completions_preserve_stop_pending_and_result() {
    with_gui(|app| {
        let sender = app.state.borrow().acp_sender.clone();
        let apply_request = AcpRequest {
            id: 1,
            operation: AcpOperation::Apply,
        };
        let stop_request = AcpRequest {
            id: 2,
            operation: AcpOperation::Stop,
        };
        let obsolete_stop = AcpRequest {
            id: 0,
            operation: AcpOperation::Stop,
        };
        let ready = ServiceStatus {
            phase: jchtools_acp_api::acp_api::ServicePhase::Ready,
            service_pid: Some(123),
            ..ServiceStatus::default()
        };
        for stop_result in [
            Ok(ServiceStatus {
                phase: jchtools_acp_api::acp_api::ServicePhase::Stopped,
                ..ServiceStatus::default()
            }),
            Err("退出安全收尾失败".to_string()),
        ] {
            apply_acp_status(&app.ui, &app.state, &ready);
            {
                let mut state = app.state.borrow_mut();
                state.acp_explicit_stopped = true;
                state.acp_pending_request = Some(stop_request);
            }
            app.ui.set_acp_explicit_stopped(true);
            app.ui.set_acp_request_pending(true);
            app.ui.set_acp_apply_inflight(false);
            app.ui.set_acp_operation("正在退出…".into());
            app.ui.set_acp_action_error("".into());
            sender
                .send(AcpMessage::Completed(apply_request, Ok(ready.clone())))
                .unwrap();
            sender
                .send(AcpMessage::Completed(
                    obsolete_stop,
                    Err("旧退出失败".into()),
                ))
                .unwrap();
            sender
                .send(AcpMessage::Completed(
                    AcpRequest::CONNECT,
                    Err("迟到连接失败".into()),
                ))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            assert!(app.ui.get_acp_request_pending());
            assert_eq!(app.state.borrow().acp_pending_request, Some(stop_request));
            assert_eq!(app.ui.get_acp_operation().as_str(), "正在退出…");
            assert!(app.ui.get_acp_action_error().is_empty());

            let succeeded = stop_result.is_ok();
            sender
                .send(AcpMessage::Completed(stop_request, stop_result))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            assert!(!app.ui.get_acp_request_pending());
            assert!(app.state.borrow().acp_pending_request.is_none());
            assert_eq!(
                app.ui.get_acp_operation().as_str(),
                if succeeded {
                    "退出已完成"
                } else {
                    "退出未成功"
                },
            );
            if succeeded {
                assert_eq!(app.ui.get_acp_phase().as_str(), "stopped");
                assert!(app.ui.get_acp_service_pid().is_empty());
            } else {
                assert_eq!(app.ui.get_acp_action_error().as_str(), "退出安全收尾失败");
                assert_eq!(app.ui.get_acp_service_error().as_str(), "退出安全收尾失败");
                assert!(!app.ui.get_acp_status_known());
            }
            let terminal = (
                app.ui.get_acp_operation(),
                app.ui.get_acp_action_error(),
                app.ui.get_acp_service_error(),
                app.ui.get_acp_phase(),
                app.ui.get_acp_status_known(),
            );
            sender
                .send(AcpMessage::Completed(apply_request, Ok(ready.clone())))
                .unwrap();
            sender
                .send(AcpMessage::Completed(stop_request, Ok(ready.clone())))
                .unwrap();
            apply_acp_messages(&app.ui, &app.state);
            assert_eq!(
                terminal,
                (
                    app.ui.get_acp_operation(),
                    app.ui.get_acp_action_error(),
                    app.ui.get_acp_service_error(),
                    app.ui.get_acp_phase(),
                    app.ui.get_acp_status_known(),
                ),
            );
            app.ui.invoke_acp_request_stop();
            app.ui.invoke_acp_apply_and_restart();
            assert_eq!(app.ui.get_confirm_kind(), 0);
            assert!(app.ui.get_acp_explicit_stopped());
            assert!(!app.ui.get_acp_request_pending());
            assert!(app.state.borrow().acp_observer.is_none());
            if !succeeded {
                // Stop 可在写入 IPC 前失败；后续 Ready 观察必须恢复退出入口，
                // 但不能撤销本 GUI 的禁止自动重启意图。
                let generation = app
                    .state
                    .borrow()
                    .acp_observation_generation
                    .load(Ordering::Acquire);
                sender
                    .send(AcpMessage::Snapshot(generation, Ok(ready.clone())))
                    .unwrap();
                apply_acp_messages(&app.ui, &app.state);
                app.ui.invoke_acp_apply_and_restart();
                assert!(app.state.borrow().acp_explicit_stopped);
                assert!(app.ui.get_acp_explicit_stopped());
                assert!(!app.ui.get_acp_pending_apply());
                assert!(!app.ui.get_acp_request_pending());
                app.ui.invoke_acp_request_stop();
                assert_eq!(app.ui.get_confirm_kind(), 5);
                assert!(app.state.borrow().acp_observer.is_none());
            }
        }
    })
    .unwrap();
}

// AH-09/AH-10：真实 GUI 回调和真实 observer/IPC，控制端保持 Apply 未完成，
// 只有另一条 Stop 连接才能取消排空。不能用测试 sender 绕过串行控制线程。
#[cfg(windows)]
#[test]
fn acp_gui_confirmed_stop_preempts_pending_apply_over_independent_control_path() {
    use acp_control_test::ControlStub;

    with_gui(|app| {
        let mut control = ControlStub::new(app);
        apply_acp_status(&app.ui, &app.state, &control.status());
        assert!(app.ui.get_acp_pending_apply());
        app.ui.invoke_acp_apply_and_restart();
        assert!(ControlStub::wait_for(app, || control.count("Apply") == 1));
        assert!(ControlStub::wait_for(app, || app.ui.get_acp_phase() == "draining"));
        assert!(app.ui.get_acp_request_pending());
        assert_eq!(control.status().executing, 1);

        // 无关确认不能被退出入口覆盖，也不能借 confirmed(5) 派发 Stop。
        app.ui.set_confirm_kind(3);
        app.ui.invoke_acp_request_stop();
        let existing_modal_preserved = app.ui.get_confirm_kind() == 3;
        app.ui.invoke_confirmed(5);
        app.ui.set_confirm_kind(0);
        app.ui.invoke_confirmed(5);
        let no_unconfirmed_stop = control.count("Stop") == 0 && !app.ui.get_acp_explicit_stopped();

        app.ui.invoke_acp_request_stop();
        let first_confirmation_opened = app.ui.get_confirm_kind() == 5;
        let no_stop_before_confirmation = control.count("Stop") == 0;
        // 取消后再确认旧 kind 不得停止；同一窗口应仍能重新请求退出。
        app.ui.set_confirm_kind(0);
        app.ui.invoke_confirmed(5);
        let cancelled_confirmation_respected =
            control.count("Stop") == 0 && !app.ui.get_acp_explicit_stopped();
        app.ui.invoke_acp_request_stop();
        let second_confirmation_opened = app.ui.get_confirm_kind() == 5;
        app.ui.invoke_confirmed(5);
        app.ui.invoke_confirmed(5);

        // Apply 尚未收尾时必须已经被后台消费 Stop，而不是排到 Apply 后面。
        let stop_consumed_before_apply_release =
            ControlStub::wait_for(app, || control.count("Stop") == 1);
        let resources_released_by_stop = control.status().phase
            == jchtools_acp_api::acp_api::ServicePhase::Stopped
            && control.status().executing == 0
            && control.status().running_config.is_none();
        let explicit_stop_recorded = app.ui.get_acp_explicit_stopped();
        let completion_delivered = if stop_consumed_before_apply_release {
            ControlStub::wait_for(app, || {
                control.apply_finished() && !app.ui.get_acp_request_pending()
            })
        } else {
            false
        };
        app.ui.set_confirm_kind(0);
        app.ui.invoke_acp_apply_and_restart();
        let restarted = ControlStub::wait_for(app, || control.count("Apply") != 1);
        let no_restart_after_explicit_stop = !restarted && app.ui.get_acp_explicit_stopped();

        // 先完整走过取消、重新确认和完成回调，再执行能失败的断言。
        // 即使门禁导致 Stop 未发送，也释放 fixture 的 Apply 并有界收回 IPC worker。
        let cleanup_finished = control.finish(app);
        assert!(cleanup_finished, "隔离控制端和在途 Apply 必须有界收尾");
        assert!(existing_modal_preserved);
        assert!(no_unconfirmed_stop && no_stop_before_confirmation);
        assert!(cancelled_confirmation_respected);
        assert!(
            first_confirmation_opened && second_confirmation_opened,
            "AH-10：Apply 在途 Draining 时，真实退出回调必须仍打开独立退出确认"
        );
        assert!(
            stop_consumed_before_apply_release,
            "AH-10：确认退出必须经独立控制连接送达 Stop，不能等待 Apply 自然排空"
        );
        assert!(resources_released_by_stop && completion_delivered);
        assert!(explicit_stop_recorded && no_restart_after_explicit_stop);
    })
    .unwrap();
}

// 保存门禁不能被「允许 Stop 抢占 Apply」的修复一并放开。
#[cfg(windows)]
#[test]
fn acp_gui_stop_respects_pending_save_then_callbacks_remain_usable() {
    use acp_control_test::ControlStub;

    with_gui(|app| {
        let mut control = ControlStub::new(app);
        apply_acp_status(&app.ui, &app.state, &control.status());
        control.hold_status_queries();
        app.ui.invoke_acp_save_config();
        assert!(ControlStub::wait_for(app, || {
            control.count("Status") > 0 && acp_settings::load_config().unwrap().is_some()
        }));
        assert!(app.ui.get_acp_request_pending());
        app.ui.invoke_acp_request_stop();
        let no_save_confirmation = app.ui.get_confirm_kind() == 0;
        app.ui.invoke_confirmed(5);
        let save_did_not_stop = control.count("Stop") == 0
            && !app.ui.get_acp_explicit_stopped()
            && app.ui.get_acp_request_pending();

        control.release_status_queries();
        let save_completed = ControlStub::wait_for(app, || !app.ui.get_acp_request_pending());
        app.ui.invoke_acp_request_stop();
        let later_confirmation_opened = app.ui.get_confirm_kind() == 5;
        app.ui.invoke_confirmed(5);
        let later_stop_consumed = ControlStub::wait_for(app, || control.count("Stop") == 1);
        let stop_completed = ControlStub::wait_for(app, || !app.ui.get_acp_request_pending());
        app.ui.set_confirm_kind(0);
        app.ui.invoke_acp_apply_and_restart();
        let restarted = ControlStub::wait_for(app, || control.count("Apply") != 0);
        let stayed_stopped = app.ui.get_acp_explicit_stopped()
            && !restarted
            && control.status().phase == jchtools_acp_api::acp_api::ServicePhase::Stopped;
        let cleanup_finished = control.finish(app);
        assert!(cleanup_finished);
        assert!(
            no_save_confirmation && save_did_not_stop,
            "AH-09：保存中不得弹出退出确认或派发 Stop"
        );
        assert!(save_completed && later_confirmation_opened);
        assert!(later_stop_consumed && stop_completed && stayed_stopped);
    })
    .unwrap();
}
