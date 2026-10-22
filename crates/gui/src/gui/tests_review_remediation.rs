//! 评审整改回归：后台 unwind/目录 probe 时序用例（自 gui.rs 的 gui_tests 拆分）。
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
use super::*;
use slint::Model;
use std::sync::Mutex;

// P2-04/H-02/U-09：真实 Git worker unwind 后必须交付一次 GitDone，而非手工补事件。
#[test]
fn review_remediation_git_unwind_finishes_and_can_restart() {
    with_gui(|app| {
        let root = tempfile::tempdir().unwrap();
        app.ui.invoke_select_tool("git-tools".into());
        app.ui
            .set_git_repo(root.path().display().to_string().into());
        app.state.borrow_mut().review_remediation.git =
            Some(Arc::new(|| panic!("PRIVATE_PANIC_PAYLOAD_git_token")));
        for _ in 0..2 {
            app.ui.invoke_git_start();
            assert!(
                app.ui.get_busy(),
                "新任务必须真实启动，不能残留上次 busy 门禁"
            );
            assert!(
                pump_until(app, || !app.ui.get_busy()),
                "Git 执行边界 unwind 必须经 GitDone 清除 busy"
            );
            assert_eq!(app.ui.get_git_state().as_str(), "失败");
            assert_eq!(app.ui.get_git_stage().as_str(), "失败");
            assert!(!app.ui.get_paused());
            assert!(app.state.borrow().control.is_none());
            assert!(app.state.borrow().git_shared.is_none());
            assert!(app.state.borrow().runtime == RuntimeMode::Organizer);
            let message = "Git 后台任务意外退出；请检查仓库后重试";
            assert_eq!(app.ui.get_status().as_str(), message);
            app.pump.run(&app.ui);
            assert_eq!(
                app.state
                    .borrow()
                    .logs
                    .iter()
                    .filter(|line| line.contains(message))
                    .count(),
                1,
                "用户日志应恰好呈现一次 GitDone 终态"
            );
            assert!(!app.ui.get_log_text().contains("PRIVATE_PANIC_PAYLOAD"));
        }
    })
    .unwrap();
}

// P1/T-05/H-02：就绪 panic 仍用当前代际 READINESS=false，不能悬停在检查中。
#[test]
fn review_remediation_converter_readiness_unwind_delivers_readiness_result() {
    with_gui(|app| {
        let root = tempfile::tempdir().unwrap();
        review_remediation_configure_conversion(app, root.path());
        app.state.borrow_mut().review_remediation.readiness =
            Some(Arc::new(|| panic!("PRIVATE_PANIC_PAYLOAD_readiness")));
        app.ui.invoke_convert_selection_changed();
        assert!(
            pump_until(app, || !app.ui.get_convert_detail_text().is_empty()),
            "readiness panic 必须交付当前代际的未就绪结果"
        );
        assert!(!app.ui.get_convert_ready());
        assert_eq!(
            app.ui.get_convert_detail_text().as_str(),
            "转 Markdown 组件检查意外退出，请重试"
        );
        assert!(!app.ui.get_error_text().contains("PRIVATE_PANIC_PAYLOAD"));
        assert!(!app
            .ui
            .get_convert_detail_text()
            .contains("PRIVATE_PANIC_PAYLOAD"));
        app.state.borrow_mut().review_remediation.readiness = Some(Arc::new(|| Ok(())));
        app.ui.invoke_convert_selection_changed();
        assert!(
            pump_until(app, || app.ui.get_convert_ready()),
            "检查异常后可以重试"
        );
        assert!(!app.ui.get_busy());
        assert!(!app.ui.get_convert_preparing());
        assert!(app.state.borrow().convert_init_cancel.is_none());
    })
    .unwrap();
}

// P1/T-22～T-25：预检 panic 必须消费 PREFLIGHT=false 并释放专属挂起参数/取消句柄。
#[test]
fn review_remediation_converter_preflight_unwind_releases_pending_state() {
    with_gui(|app| {
        let root = tempfile::tempdir().unwrap();
        review_remediation_configure_conversion(app, root.path());
        app.ui.set_convert_ready(true);
        app.state.borrow_mut().review_remediation.preflight =
            Some(Arc::new(|| panic!("PRIVATE_PANIC_PAYLOAD_preflight")));
        for _ in 0..2 {
            app.ui.invoke_convert_start();
            assert!(app.ui.get_convert_preparing(), "预检异常后必须可再次开始");
            assert!(
                pump_until(app, || !app.ui.get_convert_preparing()),
                "preflight panic 必须交付原协议终态，不能只发通用 Failed"
            );
            let state = app.state.borrow();
            assert!(!state.convert_preparing);
            assert!(state.convert_pending_options.is_none());
            assert!(state.convert_cancel.is_none());
            assert!(state.runtime == RuntimeMode::Organizer);
            assert!(!app.ui.get_busy());
            assert_eq!(
                app.ui.get_convert_status().as_str(),
                "开始前检查失败，请修正后重试"
            );
            assert_eq!(
                app.ui.get_error_text().as_str(),
                "转 Markdown 开始前检查意外退出，请重试"
            );
            assert!(!app.ui.get_error_text().contains("PRIVATE_PANIC_PAYLOAD"));
        }
    })
    .unwrap();
}

// P1/T-22～T-25：批次 panic 必须经 CONVERTER_FAIL 收尾，不删除此前成品。
#[test]
fn review_remediation_converter_run_unwind_finishes_and_preserves_outputs() {
    with_gui(|app| {
        let root = tempfile::tempdir().unwrap();
        review_remediation_configure_conversion(app, root.path());
        let completed = root.path().join("output/completed.md");
        std::fs::write(&completed, "already completed").unwrap();
        app.ui.set_convert_ready(true);
        {
            let mut state = app.state.borrow_mut();
            state.review_remediation.preflight = Some(Arc::new(|| Ok(())));
        }
        for _ in 0..2 {
            let gate = Arc::new(ReviewProbeGate::default());
            let _release = ReviewProbeRelease(gate.clone());
            let worker_gate = gate.clone();
            app.state.borrow_mut().review_remediation.conversion = Some(Arc::new(move || {
                worker_gate.probe(true);
                panic!("PRIVATE_PANIC_PAYLOAD_run");
            }));
            app.ui.invoke_convert_start();
            assert!(
                pump_until(app, || app.ui.get_busy()),
                "真实预检成功后应进入转换"
            );
            gate.release();
            assert!(
                pump_until(app, || !app.ui.get_busy()),
                "转换 worker panic 必须交付 CONVERTER_FAIL 并清 busy"
            );
            assert!(!app.ui.get_convert_preparing());
            assert!(!app.ui.get_paused());
            let state = app.state.borrow();
            assert!(state.convert_cancel.is_none());
            assert!(state.convert_pending_options.is_none());
            assert!(state.convert_current.is_empty());
            assert!(state.runtime == RuntimeMode::Organizer);
            assert_eq!(app.ui.get_convert_status().as_str(), "转换失败，请查看日志");
            assert!(app.ui.get_convert_metrics().contains("转换失败"));
            assert_eq!(
                app.ui.get_error_text().as_str(),
                "转 Markdown 后台任务意外退出；已完成的输出保留"
            );
            assert!(!app
                .ui
                .get_convert_log_text()
                .contains("PRIVATE_PANIC_PAYLOAD"));
            assert_eq!(
                std::fs::read_to_string(&completed).unwrap(),
                "already completed"
            );
        }
    })
    .unwrap();
}

// P3-23/H-02/R-04：真实目录回调不可等待 probe；代际/路径/工具过期结果不得落地。
#[test]
fn review_remediation_idle_probe_keeps_edits_and_navigation_responsive() {
    with_gui(|app| {
        let root = tempfile::tempdir().unwrap();
        for tool in [
            "directory-organizer",
            "recursive-extract",
            "md-organizer",
            "git-tools",
        ] {
            app.ui.set_directory("".into());
            app.ui.invoke_select_tool(tool.into());
            let old = root.path().join(format!("{tool}-old"));
            let latest = root.path().join(format!("{tool}-latest"));
            let old_gate = Arc::new(ReviewProbeGate::default());
            let latest_gate = Arc::new(ReviewProbeGate::default());
            let mut probes = ReviewProbeRegistration::new();
            probes.insert(old.clone(), old_gate.clone(), false);
            probes.insert(latest.clone(), latest_gate.clone(), true);
            app.ui.set_ready(true);
            app.ui.set_directory(old.display().to_string().into());
            review_remediation_edit_without_waiting(&app.ui, old_gate.clone());
            assert!(!app.ui.get_ready(), "编辑必须同步撤销旧执行权");
            assert!(pump_until(app, || old_gate.entered.load(Ordering::Acquire)));
            app.ui.set_directory(latest.display().to_string().into());
            app.ui.invoke_root_edited();
            assert_eq!(
                app.ui.get_directory().as_str(),
                latest.display().to_string()
            );
            assert!(!app.ui.get_ready());
            old_gate.release();
            assert!(pump_until(app, || latest_gate
                .entered
                .load(Ordering::Acquire)));
            app.pump.run(&app.ui);
            assert!(
                !app.ui.get_status().contains("目录不存在"),
                "旧路径的失败结果不能覆盖仍在检查的新路径"
            );
            app.ui.invoke_select_tool("recursive-extract".into());
            assert_eq!(
                app.ui.get_active_tool_id().as_str(),
                "recursive-extract",
                "probe 未放行时仍必须可以导航"
            );
            latest_gate.release();
            assert!(
                pump_until(app, || app.ui.get_status().contains("开始解压")),
                "当前工具的结果必须胜出，不能被旧工具的中性状态覆盖"
            );
            assert_eq!(
                app.ui.get_directory().as_str(),
                latest.display().to_string()
            );
            drop(probes);
        }
    })
    .unwrap();
}

// P3-23：相同路径/工具再次编辑亦是新代际，旧失败不得穿过路径相等检查。
#[test]
fn review_remediation_idle_probe_rejects_old_generation_for_same_path() {
    with_gui(|app| {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("same-path");
        let old_gate = Arc::new(ReviewProbeGate::default());
        let latest_gate = Arc::new(ReviewProbeGate::default());
        let mut probes = ReviewProbeRegistration::new();
        probes.paths.push(path.clone());
        probes.gates.extend([old_gate.clone(), latest_gate.clone()]);
        let scripted = Mutex::new(VecDeque::from([
            (old_gate.clone(), false),
            (latest_gate.clone(), true),
        ]));
        review_remediation_directory_probes()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                path.clone(),
                Arc::new(move || {
                    let (gate, result) = scripted
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pop_front()
                        .expect("目录检查脚本只供本用例的两次编辑");
                    gate.probe(result)
                }),
            );
        app.ui.invoke_select_tool("directory-organizer".into());
        app.ui.set_directory(path.display().to_string().into());
        review_remediation_edit_without_waiting(&app.ui, old_gate.clone());
        assert!(pump_until(app, || old_gate.entered.load(Ordering::Acquire)));
        app.ui.invoke_root_edited();
        assert!(!app.ui.get_ready());
        old_gate.release();
        assert!(pump_until(app, || latest_gate
            .entered
            .load(Ordering::Acquire)));
        app.pump.run(&app.ui);
        assert!(
            !app.ui.get_status().contains("目录不存在"),
            "即使路径和工具相等，旧代际结果也不能覆盖新检查"
        );
        latest_gate.release();
        assert!(pump_until(app, || app.ui.get_status()
            == "目录已就绪；「分析」只读不改文件，随时可以开始"));
    })
    .unwrap();
}

// 覆盖 R-04/H-02、P3-23：编辑后立即撤销就绪；异步检查仍须报告不存在目录。
#[test]
fn review_remediation_idle_missing_directory_still_reports_failure() {
    with_gui(|app| {
        let root = tempfile::tempdir().unwrap();
        app.ui.invoke_select_tool("directory-organizer".into());
        app.ui.set_ready(true);
        app.ui
            .set_directory(root.path().join("not-created").display().to_string().into());
        app.ui.invoke_root_edited();
        assert!(!app.ui.get_ready());
        assert!(pump_until(app, || app.ui.get_status()
            == "目录不存在或无法访问，请检查路径"));
    })
    .unwrap();
}
