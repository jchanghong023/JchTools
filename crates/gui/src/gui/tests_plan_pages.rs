//! 计划分页/筛选与失败收尾重载用例（自 gui.rs 的 gui_tests 拆分）。
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

// 覆盖 C-11（页面结果只按当前页面代际与视图筛选应用；过期页面不落地）
#[test]
fn plan_page_event_rejects_stale_generation_and_filter() {
    // 低代际事件晚到：不得因 gen!=latest 而应用（列表不能被上一筛选/上一页的结果覆盖）。
    assert!(
        !plan_page_event_accepted(5, 6, None, None),
        "低代际事件必须拒绝"
    );
    // 高代际且 filter 与视图一致：可应用（翻页为「先加载、成功再提交」，不校验预置页码）。
    assert!(plan_page_event_accepted(
        6,
        6,
        Some("delete"),
        Some("delete")
    ));
    assert!(plan_page_event_accepted(6, 6, None, None));
    // 同代但用户已切筛选：拒绝。
    assert!(!plan_page_event_accepted(6, 6, None, Some("delete")));
}

// 覆盖 S-07, U-10
#[test]
fn failed_event_log_respects_300_cap() {
    // 回归（U-10）：Failed 收尾此前直接 push_back 不查上限；一次任务先积累 300 条
    // 日志再收到失败事件时界面日志会到 301 条。失败收尾必须与普通日志同受 300 条约束。
    let mut logs = VecDeque::new();
    for i in 0..300 {
        push_event_log(&mut logs, format!("日志 {i}"));
    }
    push_event_log(&mut logs, "任务失败：测试".into());
    assert_eq!(logs.len(), 300, "界面日志最多保留最近 300 条（U-10）");
    assert_eq!(
        logs.back().map(std::string::String::as_str),
        Some("任务失败：测试"),
        "最新一条在最上"
    );
    assert_eq!(
        logs.front().map(std::string::String::as_str),
        Some("日志 1"),
        "最旧一条被挤出"
    );
}

// 覆盖 U-06（失败收尾如实重载摘要，不得崩溃或停留旧数据）
#[test]
fn failed_reload_updates_summary_without_reborrow_panic() {
    // 回归：Failed 收尾此前把 task 提取放在 if-let scrutinee 里（edition 2021 下
    // borrow() 临时存活到整个语句结束），块内 borrow_mut 更新 planned 必 BorrowMutError
    // panic——用户在任务执行中点「取消」即崩掉整个应用，且无任何测试覆盖该分支。
    with_gui(|app| {
        let dir = temp_test_dir("failed-reload");
        let db = Database::create(&dir).unwrap();
        for i in 0..3 {
            let action = jchtools_file_core::model::Action {
                id: 0,
                kind: jchtools_file_core::model::ActionKind::Delete,
                source: format!("f{i}.txt"),
                target: None,
                reason: "测试".into(),
                expected: None,
                keeper: None,
                hash: None,
                mode: jchtools_file_core::config::DeleteMode::Permanent,
                selected: true,
                state: "pending".into(),
            };
            db.add_action(&action).unwrap();
        }
        let summary = jchtools_file_core::model::Summary {
            scanned: 5,
            scanned_bytes: 0,
            archives_ok: 0,
            archives_failed: 0,
            archives_quarantined: 0,
            extracted: 0,
            planned_delete: 3,
            planned_move: 0,
            planned_git: 0,
            planned_empty: 0,
            candidate_bytes: 0,
            deleted: 0,
            moved: 0,
            skipped: 0,
            errors: 0,
            permanent_bytes: 0,
        };
        db.set("summary", &summary).unwrap();
        drop(db);
        {
            let mut s = app.state.borrow_mut();
            s.task = Some(dir.clone());
        }
        // 失败收尾的摘要与计划页改由既有计划 worker 在同一次开库取回（界面线程不再开库，
        // H-02）：这里用真实通道 + 真实事件泵排空应用，路径与事件循环一致。
        reload_after_failed(&app.ui, &app.state, &app.pump.out);
        assert!(
            pump_until(app, || app.ui.get_plan_delete_count() == 3),
            "Failed 收尾必须从任务库重载摘要计数：summary={}",
            app.ui.get_summary()
        );
        let s = app.state.borrow();
        assert_eq!(s.planned, 3, "Failed 收尾应从任务库重载分母计数");
    })
    .unwrap();
}

// 「打开任务目录」按钮无任务时的安全无操作：不打开外部程序、不报错
//（入口只在 has-task 时渲染；无独立合同行，行为边界与 C-11 计划页一致）
#[test]
fn open_report_without_task_is_a_safe_noop() {
    with_gui(|app| {
        let before = app.ui.get_status().to_string();
        app.ui.invoke_open_report();
        assert_eq!(app.ui.get_status().as_str(), before);
        assert!(app.ui.get_error_text().is_empty());
    })
    .unwrap();
}

// 覆盖 C-11（计划页经真实回调翻页与类型筛选：按钮只在对应页真实存在时可用；
// 末页再点下一页保持原页；切换类型筛选回到第一页且只显示所选类型）
#[test]
fn plan_pagination_and_kind_filter_via_real_callbacks() {
    use jchtools_file_core::config::DeleteMode;
    use jchtools_file_core::model::{Action, ActionKind};
    let dir = temp_test_dir("plan-paging");
    {
        let db = Database::create(&dir).unwrap();
        for i in 0..120 {
            db.add_action(&Action {
                id: 0,
                kind: ActionKind::Delete,
                source: format!("del-{i}.txt"),
                target: None,
                reason: String::new(),
                expected: None,
                keeper: None,
                hash: None,
                mode: DeleteMode::Keep,
                selected: true,
                state: "pending".into(),
            })
            .unwrap();
        }
        for i in 0..10 {
            db.add_action(&Action {
                id: 0,
                kind: ActionKind::Move,
                source: format!("mv-{i}.txt"),
                target: None,
                reason: String::new(),
                expected: None,
                keeper: None,
                hash: None,
                mode: DeleteMode::Keep,
                selected: true,
                state: "pending".into(),
            })
            .unwrap();
        }
    }
    let cleanup = dir.clone();
    with_gui(move |app| {
        let ui = &app.ui;
        app.state.borrow_mut().task = Some(dir.clone());
        app.state.borrow_mut().page = 0;
        app.state.borrow_mut().page_starts = vec![0];
        ui.invoke_filter_plan("".into());
        assert!(
            pump_until(app, || ui.get_plans().row_count() == 100),
            "全部筛选的第一页应显示 100 条：{}",
            ui.get_plans().row_count()
        );
        assert!(!ui.get_plan_prev_enabled() && ui.get_plan_next_enabled());
        assert!(ui.get_plan_page_label().contains("第 1 页"));

        ui.invoke_plan_page(1);
        assert!(
            pump_until(app, || ui.get_plan_page_label().contains("第 2 页")),
            "下一页应提交第二页：{}",
            ui.get_plan_page_label()
        );
        assert_eq!(ui.get_plans().row_count(), 30, "第二页应只剩 30 条");
        assert!(ui.get_plan_prev_enabled() && !ui.get_plan_next_enabled());

        // 末页再点下一页：没有可发起的请求，页面保持第二页
        ui.invoke_plan_page(1);
        app.pump.run(ui);
        assert!(ui.get_plan_page_label().contains("第 2 页"));
        assert_eq!(app.state.borrow().page, 1);

        ui.invoke_plan_page(-1);
        assert!(
            pump_until(app, || ui.get_plan_page_label().contains("第 1 页")),
            "上一页应回到第一页"
        );
        assert_eq!(ui.get_plans().row_count(), 100);

        // 类型筛选：回到第一页且只显示移动项（C-11）
        ui.invoke_filter_plan("move".into());
        assert!(
            pump_until(app, || ui.get_plans().row_count() == 10),
            "筛选 move 后应只显示 10 条：{}",
            ui.get_plans().row_count()
        );
        assert!(ui.get_plan_page_label().contains("第 1 页"));
        assert!(!ui.get_plan_prev_enabled() && !ui.get_plan_next_enabled());
        let rows = ui.get_plans();
        for i in 0..rows.row_count() {
            assert_eq!(
                rows.row_data(i).unwrap().kind.as_str(),
                "移动/重命名",
                "筛选后不得混入其他类型"
            );
        }
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&cleanup);
}

// 覆盖 R-01（规则面板 choice 行经真实回调取值：索引映射 choices 值并同步
// 行内下拉；越界索引安全忽略，不改动当前值）
#[test]
fn rule_choice_updates_value_and_ignores_out_of_range_index() {
    use jchtools_file_core::config::DeleteMode;
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_section(3); // 安全与性能：全局删除方式在此分区
                                     // choice 行的取值以配置与行内下拉索引为口径（row.value 不随选择就地改写）
        let choice_index = |ui: &AppWindow| -> i32 {
            let rules = ui.get_rules();
            (0..rules.row_count())
                .find_map(|i| {
                    let row = rules.row_data(i).unwrap();
                    (row.key.as_str() == "global_delete").then_some(row.index)
                })
                .unwrap_or(-1)
        };
        assert_eq!(
            app.state.borrow().config.global_delete,
            DeleteMode::Permanent,
            "默认值应为永久删除"
        );
        assert_eq!(choice_index(ui), 1, "行内下拉应停在默认选项");
        ui.invoke_rule_choice("global_delete".into(), 0);
        assert_eq!(
            app.state.borrow().config.global_delete,
            DeleteMode::Keep,
            "选择索引 0 应切换为保留"
        );
        assert_eq!(choice_index(ui), 0, "行内下拉应同步到所选选项");
        ui.invoke_rule_choice("global_delete".into(), 7);
        assert_eq!(
            app.state.borrow().config.global_delete,
            DeleteMode::Keep,
            "越界索引必须安全忽略"
        );
        assert_eq!(choice_index(ui), 0, "越界索引不得改动行内下拉");
        ui.invoke_rule_choice("global_delete".into(), 1);
        assert_eq!(
            app.state.borrow().config.global_delete,
            DeleteMode::Permanent
        );
        assert_eq!(choice_index(ui), 1);
    })
    .unwrap();
}
