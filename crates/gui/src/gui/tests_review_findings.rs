//! 评审发现的 F 系列缺陷回归用例（自 gui.rs 的 gui_tests 拆分）。
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

/// C-01/C-11：使用真实任务库与真实页面消费路径，不构造替代计划模型。
fn review_plan_fixture(
    app: &GuiTestApp,
    actions: Vec<jchtools_file_core::model::Action>,
) -> (tempfile::TempDir, PathBuf, Vec<i64>) {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().join("input");
    std::fs::create_dir(&root).unwrap();
    let task = workspace.path().join("task");
    let db = Database::create(&task).unwrap();
    db.set(
        "root",
        &jchtools_file_core::fsutil::path_string(&std::fs::canonicalize(&root).unwrap()).unwrap(),
    )
    .unwrap();
    db.set("config", &app.state.borrow().config).unwrap();
    db.set("status", &"ready").unwrap();
    let ids = actions
        .iter()
        .map(|action| db.add_action(action).unwrap())
        .collect();
    app.state.borrow_mut().task = Some(task.clone());
    app.ui.set_has_task(true);
    app.ui.set_directory(root.display().to_string().into());
    let sync = app.state.borrow().plan_load.clone();
    app.pump
        .out
        .send(Event::PlanPage(
            task.clone(),
            db.actions_page(0, 101).unwrap(),
            0,
            sync.page.load(Ordering::Acquire),
            None,
            sync.state.load(Ordering::Acquire),
            Some(plan_snapshot(&db, app.ui.get_directory().as_str(), false)),
        ))
        .unwrap();
    app.pump.run(&app.ui);
    assert!(app.ui.get_ready());
    (workspace, task, ids)
}

fn review_move(source: &str, target: &str) -> jchtools_file_core::model::Action {
    jchtools_file_core::model::Action {
        kind: ActionKind::Move,
        source: source.into(),
        target: Some(target.into()),
        selected: true,
        ..Default::default()
    }
}

fn review_assert_displayed_selection(app: &GuiTestApp, task: &Path) {
    let db = Database::open_existing(task).unwrap();
    for row in app.ui.get_plans().iter() {
        let action = db.action(row.id.parse().unwrap()).unwrap();
        assert_eq!(
            row.selected, action.selected,
            "C-01/C-11：呈现和执行授权必须来自同一版选择，source={}",
            row.source
        );
        assert_eq!(
            row.state.as_str(),
            plan_row_state(action.selected, &action.state)
        );
    }
}

// F02 / C-01、C-11：页查询先完成，连续两行勾选先落库，旧页却迟到。
// 持有真实查询结果/保存回包来固定交错顺序，不依赖线程速度。
#[test]
fn review_f02_late_page_cannot_restore_older_multirow_selection() {
    with_gui(|app| {
        let actions = ["first.txt", "second.txt"]
            .into_iter()
            .map(|source| jchtools_file_core::model::Action {
                source: source.into(),
                selected: false,
                ..Default::default()
            })
            .collect();
        let (_workspace, task, ids) = review_plan_fixture(app, actions);
        let db = Database::open_existing(&task).unwrap();
        let sync = app.state.borrow().plan_load.clone();
        let late_page = Event::PlanPage(
            task.clone(),
            db.actions_page(0, 101).unwrap(),
            0,
            sync.page.load(Ordering::Acquire),
            None,
            sync.state.load(Ordering::Acquire),
            Some(plan_snapshot(&db, app.ui.get_directory().as_str(), false)),
        );
        drop(db);
        for id in &ids {
            app.ui.invoke_plan_toggle(id.to_string().into(), true);
        }
        assert_eq!(app.state.borrow().pending_selection, 2);
        let completions: Vec<_> = (0..2)
            .map(|_| {
                let event = app
                    .pump
                    .receiver
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                assert!(matches!(&event, Event::SelectionSaved(_, Some(_), None)));
                event
            })
            .collect();
        app.pump.out.send(late_page).unwrap();
        for event in completions {
            app.pump.out.send(event).unwrap();
        }
        assert!(pump_until(app, || app.ui.get_ready()
            && app.state.borrow().pending_selection == 0
            && app.state.borrow().plan_recompute_inflight == 0));
        for id in ids {
            assert!(
                Database::open_existing(&task)
                    .unwrap()
                    .action(id)
                    .unwrap()
                    .selected
            );
        }
        review_assert_displayed_selection(app, &task);
    })
    .unwrap();
}

// F47 / C-01：依赖已重算，但更新页仍由测试持有，独立就绪快照先到。
#[test]
fn review_f47_readiness_cannot_authorize_unpresented_recomputed_plan() {
    with_gui(|app| {
        let (_workspace, task, ids) = review_plan_fixture(
            app,
            vec![
                review_move("a.txt", "released.txt"),
                review_move("b.txt", "a.txt"),
            ],
        );
        let db = Database::open_existing(&task).unwrap();
        db.set_selected(ids[0], false).unwrap();
        let cancelled = organizer_engine::recompute_plan(&task).unwrap();
        assert_eq!(cancelled, 1);
        assert!(!db.action(ids[1]).unwrap().selected);
        assert!(app.ui.get_plans().row_data(1).unwrap().selected);
        app.state.borrow_mut().plan_recompute_inflight = 1;
        app.ui.set_ready(false);
        app.ui.set_plan_editable(false);
        // 分离事件泵输入与 worker 输出：页面必定尚未被消费。
        let (send, receiver) = control::event_channel();
        let (worker_send, worker_receiver) = control::event_channel();
        let pump = UiPump::new(receiver, app.state.clone(), EventSender::new(worker_send));
        send.send(Event::PlanRecomputed(task.clone(), Ok(cancelled)))
            .unwrap();
        pump.run(&app.ui);
        let page = loop {
            let event = worker_receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            if matches!(&event, Event::PlanPage(..)) {
                break event;
            }
        };
        let generation = app.state.borrow().plan_load.state.load(Ordering::Acquire);
        send.send(Event::PlanState(
            task.clone(),
            generation,
            Some(plan_snapshot(&db, app.ui.get_directory().as_str(), false)),
        ))
        .unwrap();
        pump.run(&app.ui);
        assert!(
            !app.ui.get_ready(),
            "C-01：最新快照不等于用户已看见最新计划，迟到页落地前不得恢复执行"
        );
        send.send(page).unwrap();
        pump.run(&app.ui);
        assert_eq!(
            app.ui.get_plans().row_count(),
            ids.len(),
            "C-01：迟到稳定页落地后必须完整呈现所有计划行"
        );
        review_assert_displayed_selection(app, &task);
        assert!(
            app.ui.get_ready(),
            "C-01：迟到稳定页完整呈现后必须恢复执行，不能永久禁用规避先前的门禁断言"
        );
    })
    .unwrap();
}

// F49 / C-01：真实 SQLite trigger 仅拒绝本批次的一行保存，其余选择必须重算。
#[test]
fn review_f49_partial_selection_save_failure_still_recomputes_dependencies() {
    with_gui(|app| {
        let (_workspace, task, ids) = review_plan_fixture(
            app,
            vec![
                review_move("a.txt", "released.txt"),
                review_move("b.txt", "a.txt"),
                jchtools_file_core::model::Action {
                    source: "independent.txt".into(),
                    selected: true,
                    ..Default::default()
                },
            ],
        );
        {
            let db = Database::open_existing(&task).unwrap();
            db.conn
                .execute_batch(&format!(
                    "CREATE TRIGGER review_f49_reject_one_selection
                     BEFORE UPDATE OF selected ON actions WHEN NEW.id={}
                     BEGIN SELECT RAISE(FAIL, 'review_f49 isolated write failure'); END;",
                    ids[2],
                ))
                .unwrap();
        }
        app.ui.invoke_plan_toggle(ids[0].to_string().into(), false);
        app.ui.invoke_plan_toggle(ids[2].to_string().into(), false);
        assert!(pump_until(app, || app.state.borrow().pending_selection
            == 0
            && app.state.borrow().plan_recompute_inflight == 0
            && app.ui.get_ready()));
        let db = Database::open_existing(&task).unwrap();
        assert!(!db.action(ids[0]).unwrap().selected, "第一行已成功取消");
        assert!(
            db.action(ids[2]).unwrap().selected,
            "故障行必须保持数据库原值"
        );
        assert!(!app.ui.get_error_text().is_empty(), "保存失败必须可见");
        assert!(
            !db.action(ids[1]).unwrap().selected,
            "C-01：另一行保存失败不能跳过已成功取消行的占位依赖重算"
        );
        review_assert_displayed_selection(app, &task);
    })
    .unwrap();
}

// F50 / XB-20、T-30：真实下载回调与真实 MD worker 并存，取消下载不授权停止 MD。
#[test]
fn review_f50_cancel_audio_download_does_not_cancel_md_task() {
    with_gui(|app| {
        with_settings_fixture(|root| {
            let gate = Arc::new(ReviewProbeGate::default());
            let _release = ReviewProbeRelease(gate.clone());
            let worker_gate = gate.clone();
            app.state.borrow_mut().settings_download = Some(Arc::new(move |_| {
                worker_gate.probe(true);
                Err("download fixture released".into())
            }));
            deliver_settings_loaded(app);
            app.ui
                .set_convert_runtime_dir(root.display().to_string().into());
            app.ui.set_convert_runtime_confirmed(true);
            app.ui.invoke_settings_action("download".into());
            assert!(app.ui.get_convert_initializing());
            let download_cancel = app.state.borrow().convert_init_cancel.clone().unwrap();
            let input = root.join("input");
            let output = root.join("output");
            std::fs::create_dir(&input).unwrap();
            std::fs::create_dir(&output).unwrap();
            std::fs::write(input.join("one.md"), "# source\n\ncontent\n").unwrap();
            app.ui.invoke_select_tool("md-organizer".into());
            app.ui.set_md_input_dir(input.display().to_string().into());
            app.ui
                .set_md_output_dir(output.display().to_string().into());
            app.ui.set_md_output_name("merged.md".into());
            app.ui.invoke_md_merge_start();
            assert!(app.ui.get_busy());
            let md_control = app.state.borrow().control.clone().unwrap();
            // 不排空终态：无论真实 MD worker 此时进度如何，它都仍是界面中的在途任务。
            app.ui.invoke_settings_action("cancel".into());
            let cancelled_download = download_cancel.load(Ordering::Acquire);
            let cancelled_md = md_control.is_cancelled();
            gate.release();
            assert!(pump_until(app, || !app.ui.get_busy()
                && !app.ui.get_convert_initializing()));
            assert!(cancelled_download, "取消下载必须设置下载自身的取消信号");
            assert!(!cancelled_md, "取消下载不得向 MD 引擎发送取消");
            assert!(
                output.join("merged.md").is_file(),
                "MD 合并仍应正常提交输出"
            );
        })
    })
    .unwrap();
}

/// F51：真实离线 readiness 结果在 busy 时到达，终态后必须恢复检查结果。
fn review_missed_readiness_after_terminal(
    terminal: impl FnOnce(PathBuf) -> Event + Send + 'static,
) {
    with_gui(move |app| {
        with_settings_fixture(|root| {
            let (_workspace, task, _) = review_plan_fixture(app, Vec::new());
            // 空资产隔离根保证离线真实检查返回未就绪，不装模型、不联网、不假造结果。
            app.ui
                .set_convert_runtime_dir(root.join("markdown").display().to_string().into());
            app.ui.set_convert_runtime_confirmed(true);
            start_markdown_readiness(&app.ui, &app.state, &app.pump.out);
            let readiness = app
                .pump
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            assert!(matches!(&readiness,
            Event::Status(text) if text.starts_with("CONVERTER_READINESS|")));
            app.ui.set_busy(true);
            app.pump.out.send(readiness).unwrap();
            app.pump.run(&app.ui);
            assert!(app.state.borrow().convert_readiness_missed);
            assert!(app.ui.get_convert_detail_text().is_empty());
            app.pump.out.send(terminal(task)).unwrap();
            assert!(pump_until(app, || !app.ui.get_busy()));
            assert!(
                !app.state.borrow().convert_readiness_missed,
                "T-05/T-22/H-02：其他工具终态必须消费补查意图"
            );
            assert!(
                pump_until(app, || !app.ui.get_convert_detail_text().is_empty()),
                "终态后必须恢复真实组件检查结果，不能永久停留检查中"
            );
            assert!(!app.ui.get_convert_ready(), "隔离根没有资产，不能伪报就绪");
        })
    })
    .unwrap();
}

// F51 / T-05、T-22、H-02：覆盖全部 busy→idle 收尾，不仅转换自身的 DONE/FAIL。
#[test]
fn review_f51_md_terminal_recovers_missed_readiness() {
    review_missed_readiness_after_terminal(|_| Event::MdDone("MD completed".into()));
}

#[test]
fn review_f51_git_terminal_recovers_missed_readiness() {
    review_missed_readiness_after_terminal(|_| Event::GitDone("Git completed".into()));
}

#[test]
fn review_f51_extract_terminal_recovers_missed_readiness() {
    review_missed_readiness_after_terminal(|path| Event::ExtractDone(path, Summary::default()));
}

#[test]
fn review_f51_organizer_terminal_recovers_missed_readiness() {
    review_missed_readiness_after_terminal(|path| Event::Done(path, Summary::default()));
}

#[test]
fn review_f51_failed_terminal_recovers_missed_readiness() {
    review_missed_readiness_after_terminal(|_| Event::Failed("isolated task failure".into()));
}

// F52 / T-23、T-24、XB-17：停止意图不能把真实 worker 的 Err 变成已确认取消。
#[test]
fn review_f52_stop_intent_preserves_real_conversion_failure() {
    with_gui(|app| {
        with_settings_fixture(|root| {
            let gate = Arc::new(ReviewProbeGate::default());
            let _release = ReviewProbeRelease(gate.clone());
            let worker_gate = gate.clone();
            app.state.borrow_mut().review_remediation.conversion = Some(Arc::new(move || {
                worker_gate.probe(true);
            }));
            app.ui.set_screen(5);
            app.state.borrow_mut().convert_cancel =
                Some(Arc::new(std::sync::atomic::AtomicBool::new(false)));
            // 模拟预检通过后资产不可用：直接进入已有 worker 启动 API；
            // hook 只控制时序，实际错误由 markdown::run 的真实资产检查产生。
            launch_markdown_conversion(
                &app.ui,
                &app.state,
                &app.pump.out,
                markdown::Options {
                    input_dir: root.join("input"),
                    output_dir: root.join("output"),
                    flat: false,
                    groups: vec![FormatGroup::Pdf],
                    timeout_secs: 60,
                },
            );
            assert!(pump_until(app, || gate.entered.load(Ordering::Acquire)));
            app.ui.invoke_convert_stop();
            assert!(app
                .state
                .borrow()
                .convert_cancel
                .as_ref()
                .unwrap()
                .load(Ordering::Acquire));
            gate.release();
            let mut pending = Vec::new();
            let error = loop {
                let event = app
                    .pump
                    .receiver
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                match event {
                    Event::MdDone(text) => {
                        let error = text
                            .strip_prefix("CONVERTER_FAIL|")
                            .expect("真实缺失资产错误必须走失败终态")
                            .to_string();
                        for event in pending {
                            app.pump.out.send(event).unwrap();
                        }
                        app.pump.out.send(Event::MdDone(text)).unwrap();
                        break error;
                    }
                    other => pending.push(other),
                }
            };
            app.pump.run(&app.ui);
            assert!(!app.ui.get_busy());
            assert_eq!(
                app.ui.get_error_text().as_str(),
                error,
                "T-24：失败原因必须仍可见，取消意图不是底层已经确认取消"
            );
            assert!(
                app.ui.get_convert_metrics().contains("失败"),
                "真实失败不得在最终统计中伪装成正常停止"
            );
        })
    })
    .unwrap();
}

/// F48：在真实磁盘预检边界挂起，watchdog 仅负责解除挂起，不能帮 GUI 执行预检。
fn review_blocking_start_preflight(tool: Tool, split: bool) {
    with_gui(move |app| {
        app.ui.invoke_select_tool(
            match tool {
                Tool::Organizer => "directory-organizer",
                Tool::Extract => "recursive-extract",
                Tool::Md => "md-organizer",
                Tool::Git => "git-tools",
                _ => panic!("本夹具只覆盖文件工具"),
            }
            .into(),
        );
        let workspace = tempfile::tempdir().unwrap();
        let missing = workspace.path().join("missing");
        app.ui.set_directory(missing.display().to_string().into());
        app.ui
            .set_md_input_dir(missing.display().to_string().into());
        app.ui
            .set_md_output_dir(workspace.path().display().to_string().into());
        app.ui.set_md_output_name("merged.md".into());
        app.ui
            .set_md_split_file(missing.with_extension("md").display().to_string().into());
        app.ui
            .set_md_split_dir(workspace.path().display().to_string().into());
        app.ui.set_git_repo(missing.display().to_string().into());
        let gate = Arc::new(ReviewProbeGate::default());
        let _release = ReviewProbeRelease(gate.clone());
        let worker_gate = gate.clone();
        app.state.borrow_mut().review_remediation.path_preflight = Some(Arc::new(move || {
            worker_gate.probe(true);
        }));
        let (returned, callback_returned) = mpsc::channel();
        let (decision, responsive) = mpsc::channel();
        let watchdog_gate = gate.clone();
        let watchdog = std::thread::spawn(move || {
            let completed = callback_returned
                .recv_timeout(Duration::from_secs(2))
                .is_ok();
            if !completed {
                watchdog_gate.release();
            }
            decision.send(completed).unwrap();
        });
        match tool {
            Tool::Organizer => app.ui.invoke_request_start(),
            Tool::Extract => app.ui.invoke_request_extract_start(),
            Tool::Md if split => app.ui.invoke_md_split_start(),
            Tool::Md => app.ui.invoke_md_merge_start(),
            Tool::Git => app.ui.invoke_git_start(),
            _ => panic!("本夹具只覆盖文件工具的启动预检"),
        }
        // watchdog 超时后已丢弃通知接收端；失败通知不能遮蔽响应性断言。
        let _ = returned.send(());
        let completed_without_disk = responsive.recv().unwrap();
        watchdog.join().unwrap();
        // 先记录结果，再放行与消费后台收尾，避免预期红留下占用线程。
        gate.release();
        assert!(pump_until(app, || !app.ui.get_error_text().is_empty()));
        assert!(
            gate.entered.load(Ordering::Acquire),
            "必须确实经过真实预检边界"
        );
        assert!(
            completed_without_disk,
            "H-02/U-12：启动回调不得等待磁盘预检，路径阻塞期间必须返回事件循环"
        );
        assert!(!app.ui.get_busy(), "不存在的路径不得进入执行");
    })
    .unwrap();
}

// F48 / H-02、U-12：同一时序探针分别走五种真实启动回调，不替代文件系统结果。
#[test]
fn review_f48_extract_preflight_does_not_block_gui() {
    review_blocking_start_preflight(Tool::Extract, false);
}

#[test]
fn review_f48_organizer_preflight_does_not_block_gui() {
    review_blocking_start_preflight(Tool::Organizer, false);
}

#[test]
fn review_f48_md_merge_preflight_does_not_block_gui() {
    review_blocking_start_preflight(Tool::Md, false);
}

#[test]
fn review_f48_md_split_preflight_does_not_block_gui() {
    review_blocking_start_preflight(Tool::Md, true);
}

#[test]
fn review_f48_git_preflight_does_not_block_gui() {
    review_blocking_start_preflight(Tool::Git, false);
}

// F53 / AH-10：Ensure 已越过退出检查后挂起；真实 Stop 线程不能先完成副作用。
#[test]
fn review_f53_ensure_and_stop_side_effect_boundaries_are_serialized() {
    with_gui(|app| with_settings_fixture(|_| {
        let gate = Arc::new(ReviewProbeGate::default());
        // 先放行真实 Ensure，再显式 join 观察及两个操作线程；panic 路径同样执行。
        // 单独 drop observer 只设置 closed，不证明它们已停止访问隔离环境。
        struct JoinObserver {
            state: Rc<RefCell<State>>,
            gate: Arc<ReviewProbeGate>,
        }
        impl Drop for JoinObserver {
            fn drop(&mut self) {
                self.gate.release();
                let observer = self.state.borrow_mut().acp_observer.take();
                if let Some(mut observer) = observer {
                    observer.closed.store(true, Ordering::Release);
                    let mut failed = false;
                    for thread in observer.threads.drain(..) {
                        failed |= thread.join().is_err();
                    }
                    if !std::thread::panicking() {
                        assert!(!failed, "F53 观察/操作线程必须正常退出");
                    }
                }
            }
        }
        let ensure_gate = gate.clone();
        let (completed, completion) = mpsc::channel();
        {
            let mut state = app.state.borrow_mut();
            state.review_remediation.acp_before_ensure =
                Some(Arc::new(move || { ensure_gate.probe(true); }));
            state.review_remediation.acp_completed =
                Some(Arc::new(move |operation| { completed.send(operation).unwrap(); }));
        }
        let join_observer = JoinObserver { state: app.state.clone(), gate: gate.clone() };
        // 全新隔离配置，无 Agent/服务/外网；调用的仍是真实 runtime Ensure/Stop。
        // 检测的是允许启动的临界区顺序，而不是用假 Agent 冒充一次模型调用。
        start_acp_observer(&app.state, true);
        assert!(pump_until(app, || gate.entered.load(Ordering::Acquire)));
        request_acp_action(&app.ui, &app.state, AcpCommand::Stop, "review stop");
        let first = completion.recv_timeout(Duration::from_secs(2)).ok();
        gate.release();
        let first = first.unwrap_or_else(|| completion.recv_timeout(Duration::from_secs(10)).unwrap());
        let second = completion.recv_timeout(Duration::from_secs(10)).unwrap();
        // 在行为断言前完成 join；若此前等待或断言 unwind，RAII 仍先于 fixture Restore 清理。
        drop(join_observer);
        assert!(matches!(first, AcpOperation::Ensure) && matches!(second, AcpOperation::Stop),
            "AH-10：已获准的 Ensure 必须在 Stop 前完成；不能 Stop 完成后才调用启动接口；实际={first:?}, {second:?}");
    })).unwrap();
}

// F21 / M-07：真实不可解析 reparse 目录项须先覆盖确认；不依赖符号链接提权。
#[cfg(windows)]
#[test]
fn review_f21_md_merge_requires_confirmation_before_replacing_unresolvable_output() {
    with_gui(|app| {
        let workspace = tempfile::tempdir().unwrap();
        let input = workspace.path().join("input");
        let output = workspace.path().join("output");
        std::fs::create_dir(&input).unwrap();
        std::fs::create_dir(&output).unwrap();
        let source = input.join("source.md");
        let content = "# source\n\nretained original\n";
        std::fs::write(&source, content).unwrap();
        let output_file = output.join("merged.md");
        let original_entry = review_unresolvable_output::create(&output_file);
        app.ui.invoke_select_tool("md-organizer".into());
        app.ui.set_md_input_dir(input.display().to_string().into());
        app.ui
            .set_md_output_dir(output.display().to_string().into());
        app.ui.set_md_output_name("merged.md".into());
        app.ui.invoke_md_merge_start();
        assert!(pump_until(app, || !app.ui.get_busy()));
        assert_eq!(
            review_unresolvable_output::snapshot(&output_file),
            original_entry,
            "M-07：未经覆盖确认必须完整保留既存 reparse 目录项"
        );
        assert_eq!(
            app.ui.get_confirm_kind(),
            4,
            "GUI 必须提供覆盖确认，不得把引擎拒绝转换为普通失败"
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), content);
        app.ui.set_acknowledge(true);
        app.ui.set_confirm_kind(0);
        app.ui.invoke_confirmed(4);
        assert!(
            pump_until(app, || !app.ui.get_busy()
                && app.ui.get_status().contains("合并完成")),
            "M-07：真实确认后应完成合并，status={}, error={}",
            app.ui.get_status(),
            app.ui.get_error_text()
        );
        review_unresolvable_output::assert_regular_output(
            &output_file,
            b"# source.md\n\n## source\n\nretained original\n",
        );
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            content,
            "授权覆盖输出不能修改源文件"
        );
    })
    .unwrap();
}

#[test]
fn review_f51_analysis_terminal_recovers_missed_readiness() {
    review_missed_readiness_after_terminal(|path| Event::Ready(path, Summary::default()));
}

#[test]
fn review_f51_md_conflict_terminal_recovers_missed_readiness() {
    review_missed_readiness_after_terminal(|_| Event::MdNeedsConfirm("output exists".into()));
}

// F47：页面失败须 fail-closed；随后真实重载成功应恢复，不依赖重新打开窗口。
#[test]
fn review_f47_failed_page_blocks_snapshot_until_real_page_recovers() {
    with_gui(|app| {
        let (_workspace, task, ids) = review_plan_fixture(app, vec![review_move("a", "b")]);
        let sync = app.state.borrow().plan_load.clone();
        app.pump
            .out
            .send(Event::PlanLoadFailed(
                "fixture page read failed".into(),
                sync.page.load(Ordering::Acquire),
                None,
            ))
            .unwrap();
        app.pump.run(&app.ui);
        assert!(!app.ui.get_ready());
        let db = Database::open_existing(&task).unwrap();
        app.pump
            .out
            .send(Event::PlanState(
                task.clone(),
                sync.state.load(Ordering::Acquire),
                Some(plan_snapshot(&db, app.ui.get_directory().as_str(), false)),
            ))
            .unwrap();
        app.pump.run(&app.ui);
        assert!(!app.ui.get_ready(), "独立快照不能掩盖页面失败");
        load_plan_state(
            &app.pump.out,
            &sync,
            task.clone(),
            app.ui.get_directory().to_string(),
            PlanQuery::page(0, 0, None, false),
        );
        assert!(pump_until(app, || app.ui.get_ready()));
        assert_eq!(app.ui.get_plans().row_count(), ids.len());
        review_assert_displayed_selection(app, &task);
    })
    .unwrap();
}

#[test]
fn review_f34_settings_shows_unsaved_migration_warning() {
    with_gui(|app| {
        app.pump.apply_settings_snapshot(
            &app.ui,
            &serde_json::json!({
                "active": null,
                "status": "离线检查完成",
                "migration_warning": "旧配置仍可使用，但尚未保存到新配置库",
            })
            .to_string(),
            false,
        );
        assert!(app.ui.get_settings_loaded());
        assert_eq!(
            app.ui.get_settings_status().as_str(),
            "旧配置仍可使用，但尚未保存到新配置库"
        );
    })
    .unwrap();
}

// F48/U-09：磁盘边界仍挂起时，真实停止并关闭回调必须先接受取消。
#[test]
fn review_f48_blocked_preflight_accepts_stop_and_close_for_all_workers() {
    for (tool, split) in [
        ("directory-organizer", false),
        ("md-organizer", false),
        ("md-organizer", true),
        ("git-tools", false),
    ] {
        with_gui(move |app| {
            let workspace = tempfile::tempdir().unwrap();
            let missing = workspace.path().join("missing");
            app.ui.invoke_select_tool(tool.into());
            app.ui.set_directory(missing.display().to_string().into());
            app.ui
                .set_md_input_dir(missing.display().to_string().into());
            app.ui
                .set_md_output_dir(workspace.path().display().to_string().into());
            app.ui
                .set_md_split_file(missing.with_extension("md").display().to_string().into());
            app.ui
                .set_md_split_dir(workspace.path().display().to_string().into());
            app.ui.set_git_repo(missing.display().to_string().into());
            let gate = Arc::new(ReviewProbeGate::default());
            let _release = ReviewProbeRelease(gate.clone());
            let worker_gate = gate.clone();
            app.state.borrow_mut().review_remediation.path_preflight = Some(Arc::new(move || {
                worker_gate.probe(true);
            }));
            match tool {
                "directory-organizer" => app.ui.invoke_request_start(),
                "md-organizer" if split => app.ui.invoke_md_split_start(),
                "md-organizer" => app.ui.invoke_md_merge_start(),
                "git-tools" => app.ui.invoke_git_start(),
                _ => unreachable!(),
            }
            assert!(pump_until(app, || gate.entered.load(Ordering::Acquire)));
            let control = app.state.borrow().control.clone().unwrap();
            app.ui.set_confirm_kind(3);
            app.ui.set_acknowledge(true);
            app.ui.invoke_confirmed(3);
            app.ui.set_confirm_kind(0);
            assert!(
                control.is_cancelled(),
                "后台预检尚未返回，也必须立即接受取消"
            );
            assert!(
                app.state.borrow().close_after,
                "不能把取消意图当作任务已结束"
            );
            gate.release();
            assert!(pump_until(app, || !app.ui.get_busy()));
            assert!(
                !app.state.borrow().close_after,
                "真实终态必须完成停止并关闭收尾"
            );
            assert!(!workspace.path().join("merged.md").exists());
        })
        .unwrap();
    }
}

#[test]
fn review_f48_dismissed_extract_preflight_ignores_real_late_count() {
    with_gui(|app| {
        with_settings_fixture(|_| {
            let workspace = tempfile::tempdir().unwrap();
            app.ui.invoke_select_tool("recursive-extract".into());
            app.ui
                .set_directory(workspace.path().display().to_string().into());
            let gate = Arc::new(ReviewProbeGate::default());
            let _release = ReviewProbeRelease(gate.clone());
            let worker_gate = gate.clone();
            app.state.borrow_mut().review_remediation.path_preflight = Some(Arc::new(move || {
                worker_gate.probe(true);
            }));
            app.ui.invoke_request_extract_start();
            assert!(pump_until(app, || gate.entered.load(Ordering::Acquire)));
            assert!(app.ui.get_confirm_pending());
            // 与 Slint「返回检查」按钮相同：未确认的只读预检不成为关窗后执行任务。
            app.ui.set_confirm_kind(0);
            app.ui.set_confirm_pending(false);
            app.ui.set_acknowledge(false);
            gate.release();
            let event = app
                .pump
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            assert!(matches!(&event, Event::ExtractCount(_, Ok(0))));
            app.pump.out.send(event).unwrap();
            app.pump.run(&app.ui);
            assert_eq!(app.ui.get_confirm_kind(), 0);
            assert!(!app.ui.get_confirm_pending());
            assert!(!app.ui.get_busy());
            assert!(app.ui.get_error_text().is_empty());
        })
    })
    .unwrap();
}
