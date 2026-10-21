//! 目录整理/递归解压启动与失败列表用例（自 gui.rs 的 gui_tests 拆分）。
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

/// 覆盖 C-11/H-02：就绪只由「后台快照 + 当前界面配置/目录」决定。快照对应的目录或库内
/// 配置已不是当前值时不得凭陈旧快照点亮执行；任务库读不到时既不可执行也不可勾选。
#[test]
fn plan_readiness_fails_closed_on_stale_snapshot() {
    let config = Config::default();
    let directory = "D:/data";
    // worker 侧的库内配置已剔除 theme（见 plan_snapshot），界面侧同样剔除后比较。
    let snapshot = |status: Option<&str>, requested: &str, root_matches: bool, db: &Config| {
        let mut config_json = serde_json::to_value(db).unwrap();
        config_json.as_object_mut().unwrap().remove("theme");
        PlanSnapshot {
            status: status.map(str::to_string),
            config_json: Some(config_json),
            root_matches,
            requested_directory: requested.to_string(),
            summary: None,
        }
    };
    // 目录、库内配置与根目录都一致且任务就绪：可执行。
    assert!(matches!(
        classify_plan_snapshot(
            Some(&snapshot(Some("ready"), directory, true, &config)),
            directory,
            &config
        ),
        PlanReadyState::Ready
    ));
    // 用户已改目录（去抖重算尚未回来）：不得用旧快照点亮执行。
    assert!(matches!(
        classify_plan_snapshot(
            Some(&snapshot(Some("ready"), "D:/other", true, &config)),
            directory,
            &config
        ),
        PlanReadyState::Changed
    ));
    // 规则已改或目录与任务库根目录不一致：按「已改变」处理（可继续勾选，但不能执行）。
    let other = Config {
        clean_temp: true,
        ..Config::default()
    };
    assert!(matches!(
        classify_plan_snapshot(
            Some(&snapshot(Some("ready"), directory, true, &other)),
            directory,
            &config
        ),
        PlanReadyState::Changed
    ));
    assert!(matches!(
        classify_plan_snapshot(
            Some(&snapshot(Some("ready"), directory, false, &config)),
            directory,
            &config
        ),
        PlanReadyState::Changed
    ));
    // 任务已结束：既不可执行也不可勾选（勾选必然写不进任务库）。
    assert!(matches!(
        classify_plan_snapshot(
            Some(&snapshot(Some("finished"), directory, true, &config)),
            directory,
            &config
        ),
        PlanReadyState::Finished
    ));
    // 任务库快照缺少状态元数据时不得误报成已结束。
    assert!(matches!(
        classify_plan_snapshot(
            Some(&snapshot(None, directory, true, &config)),
            directory,
            &config
        ),
        PlanReadyState::Unavailable
    ));
    // 任务库读不到（快照缺失）：fail-closed。
    assert!(matches!(
        classify_plan_snapshot(None, directory, &config),
        PlanReadyState::Unavailable
    ));
}

// 覆盖 U-03, C-11（进度分母只含仍勾选且待执行的项）
#[test]
fn count_selected_pending_ignores_unselected_and_done() {
    use jchtools_file_core::config::DeleteMode;
    use jchtools_file_core::model::{Action, ActionKind};
    let dir = temp_test_dir("count-selected");
    let db = Database::create(&dir).unwrap();
    // 2 个仍勾选且待执行；1 个取消勾选；1 个已执行
    for (selected, state) in [
        (true, "pending"),
        (true, "pending"),
        (false, "pending"),
        (true, "done"),
    ] {
        let action = Action {
            id: 0,
            kind: ActionKind::Delete,
            source: format!("a-{selected}-{state}"),
            target: None,
            reason: String::new(),
            expected: None,
            keeper: None,
            hash: None,
            mode: DeleteMode::Keep,
            selected,
            state: state.to_string(),
        };
        let id = db.add_action(&action).unwrap();
        // add_action 固定写 pending，显式标记需要非 pending 的状态。
        if state != "pending" {
            db.mark_action(id, state).unwrap();
        }
    }
    drop(db);
    assert_eq!(count_selected_pending(&dir).unwrap(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

// 覆盖 S-02, R-02, C-08（整理确认逐项如实告知删除后果：按当前开关与删除方式，解压处置不再出现）
#[test]
fn organizer_warning_mentions_only_organizer_deletes() {
    let warning = Config::default().destructive_warning();
    assert!(
        warning.contains("重复副本") && warning.contains("永久删除"),
        "整理确认必须告知重复副本的删除方式：{warning}"
    );
    for category in ["系统附属文件", "临时与备份文件", "零字节文件"] {
        assert!(
            warning.contains(category),
            "C-08：整理确认必须逐项说明清理后果（{category}）：{warning}"
        );
    }
    assert!(
        warning.contains("空目录"),
        "H-05/C-07：空目录清理不可关闭，必须如实告知：{warning}"
    );
    assert!(
        warning.contains("不可由本软件恢复"),
        "S-02：必须明确告知永久删除不可恢复：{warning}"
    );
    for forbidden in ["原压缩包", "解压覆盖", "解压冲突"] {
        assert!(
            !warning.contains(forbidden),
            "R-02：解压侧的处置已固定，不得出现在整理确认里（{forbidden}）：{warning}"
        );
    }
    // 关闭的清理项如实显示为「不清理」，而不是宣称会删除。
    let config = Config {
        clean_temp: false,
        ..Config::default()
    };
    assert!(
        config
            .destructive_warning()
            .contains("临时与备份文件：不清理"),
        "关闭的清理类别不得宣称删除：{}",
        config.destructive_warning()
    );
}

// 覆盖 X-04, X-05, R-02, R-03, C-08, S-04（规则表与配置只暴露合同允许的开关与默认值）
#[test]
fn rules_and_config_expose_only_contracted_switches() {
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../../../../resources/rules.json")).unwrap();
    let keys: Vec<&str> = rows.iter().filter_map(|row| row["key"].as_str()).collect();
    let config = serde_json::to_value(Config::default()).unwrap();
    // X-05/X-04/R-02：原包处置、冲突策略、冲突删除方式与嵌套解压开关不再存在。
    for forbidden in [
        "nested_archives",
        "archive_delete",
        "extract_conflict",
        "conflict_delete",
        "clean_empty_dirs",
    ] {
        assert!(
            !keys.contains(&forbidden),
            "R-02：规则表不得再有 {forbidden} 行（行为已固定）"
        );
        assert!(
            config.get(forbidden).is_none(),
            "R-02：配置不得再有 {forbidden} 字段"
        );
    }
    // C-07/H-05：空目录清理不可关闭，因此不得再有总开关或统一删除方式行。
    assert!(
        !keys.contains(&"cleanup_delete"),
        "C-08：清理删除方式必须按项独立，不得再有一个覆盖全部清理项的行"
    );
    assert!(
        config.get("cleanup_delete").is_none(),
        "C-08：配置不得再有 cleanup_delete 字段"
    );
    // C-08 六类独立开关：规则表保留且默认值逐项与合同一致。
    for (key, expected) in [
        ("clean_junk", true),
        ("clean_temp", false),
        ("clean_zero", false),
        ("clean_copy_name", true),
        ("normalize_names", true),
        ("fix_extension", false),
    ] {
        assert!(keys.contains(&key), "C-08：规则表必须保留独立开关 {key}");
        assert_eq!(
            config[key].as_bool(),
            Some(expected),
            "C-08：{key} 的默认值必须为 {expected}"
        );
    }
    // C-08：涉及删除的清理项各自独立覆盖删除方式，默认跟随全局文件删除方式。
    for key in ["junk_delete", "temp_delete", "zero_delete"] {
        assert!(keys.contains(&key), "C-08：{key} 必须以独立删除方式行暴露");
        assert_eq!(
            config[key].as_str(),
            Some("global"),
            "C-08：{key} 默认必须跟随全局文件删除方式"
        );
    }
    // S-04/R-03：默认覆盖隐藏与系统属性资料，排除规则默认为空（不额外缩小范围）。
    assert_eq!(config["include_hidden"].as_bool(), Some(true));
    assert_eq!(config["include_system"].as_bool(), Some(true));
    assert_eq!(config["exclusions"].as_str(), Some(""));
    assert_eq!(config["recursive"].as_bool(), Some(true));
}

// 覆盖 R-03, S-04（主动关闭递归或排除资料时明确提示范围缩小；恢复默认范围后提示消失）
#[test]
fn narrowing_scope_shows_notice_and_restoring_clears_it() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("recursive-extract".into());
        assert_eq!(ui.get_notice_text().as_str(), "", "默认范围不提示缩小");
        ui.invoke_rule_bool("recursive".into(), false);
        assert!(
            ui.get_notice_text().contains("已缩小处理范围"),
            "R-03：关闭递归必须明确提示范围缩小：{}",
            ui.get_notice_text()
        );
        assert!(
            ui.get_notice_text().contains("子目录"),
            "提示须说明只处理所选目录的直接子项：{}",
            ui.get_notice_text()
        );
        ui.invoke_rule_bool("include_hidden".into(), false);
        assert!(
            ui.get_notice_text().contains("隐藏"),
            "S-04：不包含隐藏资料同样要提示范围缩小：{}",
            ui.get_notice_text()
        );
        ui.invoke_rule_bool("recursive".into(), true);
        ui.invoke_rule_bool("include_hidden".into(), true);
        assert_eq!(
            ui.get_notice_text().as_str(),
            "",
            "恢复默认范围后不得残留缩小提示"
        );
    })
    .unwrap();
}

// 覆盖 X-02/R-02：递归解压入口必须按解压专属规则校验并在错误时不启动。
#[test]
fn extract_start_validates_extract_configuration() {
    let root = temp_test_dir("extract-config-validation");
    with_gui(move |app| {
        let ui = &app.ui;
        ui.set_directory(root.display().to_string().into());
        app.state.borrow_mut().config.max_depth = 0;

        start_extract(ui, &app.state, &app.pump.out);

        assert!(
            ui.get_error_text().contains("嵌套层数"),
            "无效解压配置必须显示对应错误：{}",
            ui.get_error_text()
        );
        assert!(!ui.get_busy(), "校验失败不得启动解压任务");
        let _ = std::fs::remove_dir_all(&root);
    })
    .unwrap();
}

// 覆盖 X-02, S-05：后台预检期间不可确认，受保护目录失败后关闭待确认框。
// 平台门禁原因：S-05 的安装目录保护分支依赖 Windows 环境变量与路径语义。
#[cfg(windows)]
#[test]
fn extract_start_rejects_protected_directory_before_confirmation() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("recursive-extract".into());
        let protected = std::env::var("SystemRoot").unwrap();
        ui.set_directory(protected.clone().into());
        ui.invoke_request_extract_start();
        assert!(
            ui.get_confirm_pending(),
            "后台清点/预检尚未完成，不得允许确认"
        );
        assert!(
            pump_until(app, || !ui.get_error_text().is_empty()),
            "后台预检必须报告受保护目录错误"
        );
        assert_ne!(ui.get_confirm_kind(), 1, "预检失败后必须关闭待确认框");
        assert!(
            !ui.get_error_text().is_empty(),
            "必须给出明确错误：{}",
            ui.get_error_text()
        );
    })
    .unwrap();
}

// 覆盖 C-11, C-01（另一工具失败/取消后切回整理：仍就绪的计划必须保持可勾选）
#[test]
fn plan_editable_recovers_when_ready_task_is_shown_again() {
    // 回归：plan-editable 曾只在任务事件里刷新，而 Event::Failed 是两个工具共用的收尾
    // 事件——解压取消会把禁用态带到整理工具，切回后计划可执行却勾不动（C-01 逐项可勾选）。
    with_gui(|app| {
        let task = temp_test_dir("plan-editable-task");
        let root = temp_test_dir("plan-editable-root");
        let canonical = std::fs::canonicalize(&root).unwrap();
        let db = Database::create(&task).unwrap();
        db.set(
            "root",
            &jchtools_file_core::fsutil::path_string(&canonical).unwrap(),
        )
        .unwrap();
        db.set("config", &jchtools_file_core::config::Config::default())
            .unwrap();
        db.set("status", &"ready".to_string()).unwrap();
        drop(db);
        {
            let mut s = app.state.borrow_mut();
            s.task = Some(task.clone());
        }
        let ui = &app.ui;
        ui.set_has_task(true);
        ui.set_directory(root.to_string_lossy().to_string().into());
        // 模拟解压取消/失败后残留的禁用态（Event::Failed 收尾时置为 false）。
        ui.set_plan_editable(false);
        sync_ready_after_directory(ui, &app.state.borrow(), &app.pump.out);
        // 就绪判定在后台 worker 上（开库 + canonicalize 不进界面线程）：等快照落地。
        assert!(
            pump_until(app, || ui.get_plan_editable() && ui.get_ready()),
            "仍就绪的整理计划在切回后必须保持可勾选（C-01/C-11）：status={}",
            ui.get_status()
        );
        assert!(ui.get_ready(), "同一状态下主按钮应可用");
        // 反向：任务已取消/失败（任务库状态非 ready）后勾选必须禁用，
        // 否则用户点到的是必然被 db::set_selected 拒绝的行（C-11）。
        let db = Database::open_existing(&task).unwrap();
        db.set("status", &"cancelled".to_string()).unwrap();
        drop(db);
        sync_ready_after_directory(ui, &app.state.borrow(), &app.pump.out);
        assert!(
            pump_until(app, || !ui.get_plan_editable()),
            "已取消任务的计划不得再可勾选（C-11）：status={}",
            ui.get_status()
        );
        assert!(!ui.get_ready(), "已取消任务不得再显示为可执行");
    })
    .unwrap();
}

// 覆盖 C-11（计划行「已执行/已跳过/执行失败」必须以中文显示，不得透出英文状态值；
// 任务库存 done/skipped/failed，读取链必须翻译）
#[test]
fn plan_row_state_maps_terminal_states_to_chinese() {
    assert_eq!(plan_row_state(true, "done"), "已执行");
    assert_eq!(plan_row_state(true, "skipped"), "已跳过");
    assert_eq!(plan_row_state(true, "failed"), "执行失败");
    // 既有中文旧值别名保持稳定
    assert_eq!(plan_row_state(true, "已执行"), "已执行");
    assert_eq!(plan_row_state(true, "已跳过"), "已跳过");
    assert_eq!(plan_row_state(true, "执行失败"), "执行失败");
    // 未执行语义不回归：勾选即时生效口径保持
    assert_eq!(plan_row_state(true, "pending"), "待执行");
    assert_eq!(plan_row_state(false, "pending"), "已取消勾选");
    assert_eq!(plan_row_state(true, "unselected"), "待执行");
    assert_eq!(plan_row_state(false, "unselected"), "已取消勾选");
    // 勾选门禁联动（ui/app.slint 只认「待执行/已取消勾选」可勾选）：
    // 补映射后已执行/已跳过/执行失败行仍不得落入可勾选状态。
    for terminal in ["done", "skipped", "failed", "已执行", "已跳过", "执行失败"] {
        assert!(
            !matches!(plan_row_state(true, terminal), "待执行" | "已取消勾选"),
            "终态 {terminal} 不得映射为可勾选状态（勾选门禁失效）"
        );
    }
}

// 覆盖 X-02（清点为 0 时必须提示无可处理包：不解除清点门禁、确认保持不可用，
// 不得启动空解压任务）
#[test]
fn extract_count_zero_keeps_confirm_gated() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("keep.txt"), b"keep me").unwrap();
    let data_text = data.display().to_string();
    with_gui(move |app| {
        let ui = &app.ui;
        ui.invoke_select_tool("recursive-extract".into());
        ui.set_directory(data_text.into());
        ui.invoke_request_extract_start();
        assert!(
            pump_until(app, || {
                ui.get_confirm_kind() == 1 && ui.get_confirm_text().contains("未发现")
            }),
            "清点为 0 必须给出「未发现可处理包」的如实提示：{}",
            ui.get_confirm_text()
        );
        assert!(
            ui.get_confirm_pending(),
            "清点为 0 不得解除清点门禁（确认必须保持不可用，X-02 不启动空解压任务）"
        );
        assert!(
            !ui.get_confirm_text().contains("永久删除"),
            "零包提示不得保留破坏性告知（没有任务可启动）：{}",
            ui.get_confirm_text()
        );
        // 返回检查：门禁状态随确认框关闭一并复位，可重新发起
        ui.set_confirm_kind(0);
        ui.set_confirm_pending(false);
    })
    .unwrap();
}

// 覆盖 X-02, S-02, X-04, X-05, X-06, H-07（发现数 > 0 的确认文案必须完整披露：
// 原包及分卷永久删除且不可恢复、失败/取消保留、已有文件保留、冲突只为新文件自动改名、
// 失败包去向，且不得出现覆盖或冲突策略授权）。该文案由 extract_confirm_text 构造，
// 端到端零包用例无法覆盖（目录无压缩包时按 X-02 不再进入确认执行），故在此逐条断言。
#[test]
fn extract_confirm_text_discloses_deletion_rules() {
    let text = extract_confirm_text(
        "清点到 2 个压缩包（按当前扫描范围，已排除「解压失败」目录）。\n",
        "D:/data",
    );
    assert!(
        text.contains("永久删除"),
        "X-02/S-02：确认文案必须说明完整成功后原包及分卷永久删除：{text}"
    );
    assert!(
        text.contains("不可恢复"),
        "X-02/S-02：确认文案必须说明永久删除不可恢复：{text}"
    );
    assert!(
        text.contains("保留"),
        "X-05：确认文案必须说明失败/部分/取消时保留原包：{text}"
    );
    assert!(
        text.contains("已有文件"),
        "X-05/H-07：确认文案必须说明已有文件保留：{text}"
    );
    assert!(
        text.contains("自动改文件名"),
        "X-04/H-07：确认文案必须说明冲突只为新文件自动改名：{text}"
    );
    assert!(
        text.contains("「解压失败」"),
        "X-06：确认文案必须说明失败包去向：{text}"
    );
    assert!(
        !text.contains("覆盖") && !text.contains("冲突策略"),
        "X-04/H-07：解压确认不得出现覆盖或冲突策略授权：{text}"
    );
}

// 覆盖 U-10, X-06, S-07（本次解压失败项分页列表）：每个失败包/卷集一项，显示原路径、
// 失败原因与隔离后位置；列表显示失败项总数；成功包与任务级记录不出现；源包清理失败
// 单独标明阶段；无隔离记录时如实留空，不编造「已隔离」；切换工具不清空该列表。
#[test]
fn fail_list_groups_failed_events_per_volume_set() {
    let task = make_extract_task_db("fail-list-basic");
    let task_for_cleanup = task.clone();
    with_gui(move |app| {
        app.state.borrow_mut().extract_task = Some(task.clone());
        app.state.borrow_mut().extract_generation = 5;
        // 模拟 watcher 回传任务目录发现（watcher 自身另有专测）；代际须与当前一致。
        let sender = app.state.borrow().fail_sender.clone();
        sender.send(FailChannelMsg::TaskFound(task, 5)).unwrap();
        assert!(
            pump_until(app, || {
                app.ui.get_fail_ready() && app.ui.get_fail_total() == 3
            }),
            "失败列表必须显示失败项总数 3：total={} ready={}",
            app.ui.get_fail_total(),
            app.ui.get_fail_ready()
        );
        let rows = app.ui.get_fail_rows();
        assert_eq!(rows.row_count(), 3, "一页应包含全部 3 个失败项");
        let first = rows.row_data(0).unwrap();
        assert_eq!(first.stage.as_str(), "解压失败");
        assert_eq!(first.source.as_str(), "bad.7z", "U-10：必须显示原路径");
        assert_eq!(
            first.reason.as_str(),
            "压缩包已损坏",
            "U-10：必须显示失败原因"
        );
        assert!(
            first.target.as_str().starts_with("解压失败/bad.7z") && first.target.contains("2 个卷"),
            "整组卷集隔离必须显示隔离后位置与卷数：{}",
            first.target
        );
        let second = rows.row_data(1).unwrap();
        assert_eq!(second.stage.as_str(), "未完全解开");
        assert_eq!(second.source.as_str(), "part.zip");
        assert_eq!(second.target.as_str(), "解压失败/part.zip");
        let third = rows.row_data(2).unwrap();
        assert_eq!(
            third.stage.as_str(),
            "源包清理失败（X-05）",
            "源包清理失败必须单独标明阶段，不冒充坏包"
        );
        assert_eq!(third.source.as_str(), "ok2.zip");
        assert_eq!(
            third.target.as_str(),
            "",
            "无隔离记录必须如实留空，不得编造「已隔离」"
        );
        assert!(rows.row_data(0).is_some());
        assert!(
            !app.ui.get_fail_prev_enabled() && !app.ui.get_fail_next_enabled(),
            "只有一页时两个翻页按钮都必须禁用"
        );
        // 切换工具不清空该列表（U-10）
        app.ui.invoke_select_tool("git-tools".into());
        assert_eq!(app.ui.get_fail_total(), 3, "切换工具不得清空失败列表");
        assert_eq!(app.ui.get_fail_rows().row_count(), 3);
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&task_for_cleanup);
}

// 覆盖 U-10 / X-06：隔离失败与已移动的每个分卷位置必须留在完整分页明细中，
// 不得只依赖会被最近 300 条淘汰的运行日志，也不能把部分隔离说成整组成功。
#[test]
fn fail_list_retains_partial_quarantine_error_and_all_moved_locations() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create(directory.path()).unwrap();
    db.log("解压", "bad.7z.001", "", "失败", "压缩包损坏", 0)
        .unwrap();
    for suffix in ["001", "002"] {
        db.log(
            "解压",
            &format!("bad.7z.{suffix}"),
            &format!("解压失败/bad (1).7z.{suffix}"),
            "移入解压失败",
            "压缩包损坏",
            0,
        )
        .unwrap();
    }
    let isolation_error = "未移动的源项 bad.7z.003 仍在原位置：拒绝访问";
    db.log("解压", "bad.7z.001", "", "隔离失败", isolation_error, 0)
        .unwrap();
    db.log("解压", "next.zip", "", "失败", "下一包错误", 0)
        .unwrap();
    drop(db);
    let page = query_fail_page(directory.path(), 0, 0, 1).unwrap();
    assert_eq!(page.total, 2, "隔离失败是原失败包的详情，不另计一包");
    let first = &page.items[0];
    assert!(first.reason.contains("压缩包损坏"));
    assert!(first.reason.contains(isolation_error), "{}", first.reason);
    assert!(first.target.contains("解压失败/bad (1).7z.001"));
    assert!(first.target.contains("解压失败/bad (1).7z.002"));
    assert!(!first.target.contains("整组"), "部分移动不能宣称整组已隔离");
    assert_eq!(page.items[1].reason, "下一包错误");
    assert!(page.items[1].target.is_empty());
}

// 覆盖 U-10/X-06：watcher 未及时发现任务时，解压终态必须按返回的真实任务库加载失败列表。
#[test]
fn extract_done_associates_failure_list_without_watcher() {
    let task = make_extract_task_db("extract-done-failure-list");
    let cleanup = task.clone();
    with_gui(move |app| {
        app.ui.set_busy(true);
        app.state.borrow_mut().extracting = true;
        app.state.borrow_mut().runtime = RuntimeMode::Extract;
        app.pump
            .out
            .send(Event::ExtractDone(task.clone(), Summary::default()))
            .unwrap();
        app.pump.drain(&app.ui);
        assert_eq!(app.state.borrow().extract_task.as_ref(), Some(&task));
        assert!(app.ui.get_fail_ready());
        assert!(pump_until(app, || app.ui.get_fail_total() == 3));
        assert_eq!(app.ui.get_fail_rows().row_count(), 3);
        let rows = app.ui.get_fail_rows();
        let generation = app.state.borrow().fail_gen;
        let sender = app.state.borrow().fail_sender.clone();
        sender
            .send(FailChannelMsg::TaskFound(
                task,
                app.state.borrow().extract_generation,
            ))
            .unwrap();
        app.pump.apply_fail_messages(&app.ui);
        assert_eq!(
            app.state.borrow().fail_gen,
            generation,
            "迟到的同任务发现不得重置已加载列表"
        );
        assert_eq!(app.ui.get_fail_rows().row_count(), rows.row_count());
        assert_eq!(app.ui.get_fail_total(), 3);
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(cleanup);
}

// 覆盖 U-10（附录 E：超过 300 项也不得截断，全部可翻页查询；翻页按钮只在对应页
// 真实存在时启用；最后一项仍能翻页查到）
#[test]
fn fail_list_pages_beyond_single_page_limit() {
    let dir = temp_test_dir("fail-list-paging");
    {
        let db = Database::create(&dir).unwrap();
        for i in 0..305 {
            db.log(
                "解压",
                &format!("bad{i}.zip"),
                "",
                "失败",
                &format!("损坏 {i}"),
                0,
            )
            .unwrap();
        }
    }
    let dir_text = dir.display().to_string();
    with_gui(move |app| {
        app.state.borrow_mut().extract_task = Some(PathBuf::from(&dir_text));
        request_fail_page(&app.state, 0);
        assert!(
            pump_until(app, || {
                app.ui.get_fail_total() == 305 && app.ui.get_fail_rows().row_count() == 100
            }),
            "首页应取 100 项且总数如实显示 305：total={}",
            app.ui.get_fail_total()
        );
        assert!(app.ui.get_fail_next_enabled(), "305 项必须有下一页");
        assert!(!app.ui.get_fail_prev_enabled(), "首页没有上一页");
        app.ui.invoke_fail_page(1);
        assert!(
            pump_until(app, || {
                app.ui.get_fail_rows().row_count() == 100
                    && app.ui.get_fail_page_label().contains("第 2 页")
            }),
            "第二页应取 100 项：{}",
            app.ui.get_fail_page_label()
        );
        app.ui.invoke_fail_page(2);
        assert!(
            pump_until(app, || {
                app.ui.get_fail_rows().row_count() == 100
                    && app.ui.get_fail_page_label().contains("第 3 页")
            }),
            "第三页应取 100 项：{}",
            app.ui.get_fail_page_label()
        );
        assert!(app.ui.get_fail_prev_enabled() && app.ui.get_fail_next_enabled());
        app.ui.invoke_fail_page(3);
        assert!(
            pump_until(app, || {
                app.ui.get_fail_rows().row_count() == 5
                    && !app.ui.get_fail_next_enabled()
                    && app.ui.get_fail_page_label().contains("第 4 页")
            }),
            "最后一页 5 项且不再有下一页（按钮只在对应页真实存在时启用）：{}",
            app.ui.get_fail_page_label()
        );
        let rows = app.ui.get_fail_rows();
        let last = rows.row_data(4).unwrap();
        assert_eq!(
            last.source.as_str(),
            "bad304.zip",
            "附录 E：最后一项仍能翻页查到，不因 300 条截断"
        );
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// 覆盖 U-10（失败列表只保留「本次」任务：启动下一次解压任务前清空上一任务的列表）
#[test]
fn reset_fail_list_clears_for_next_task() {
    let task = make_extract_task_db("fail-list-reset");
    let task_for_cleanup = task.clone();
    with_gui(move |app| {
        app.state.borrow_mut().extract_task = Some(task.clone());
        app.state.borrow_mut().extract_generation = 1;
        let sender = app.state.borrow().fail_sender.clone();
        sender.send(FailChannelMsg::TaskFound(task, 1)).unwrap();
        assert!(
            pump_until(app, || app.ui.get_fail_ready()
                && app.ui.get_fail_total() == 3),
            "前置：失败列表已填充"
        );
        let mut s = app.state.borrow_mut();
        reset_fail_list(&app.ui, &mut s);
        drop(s);
        assert!(
            !app.ui.get_fail_ready() && app.ui.get_fail_total() == 0,
            "新任务开始必须清空上一任务的失败列表"
        );
        assert_eq!(app.ui.get_fail_rows().row_count(), 0);
        assert!(app.state.borrow().extract_task.is_none());
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&task_for_cleanup);
}

// 覆盖 U-10（失败列表的数据源发现：状态目录下出现新任务目录时 watcher 回传路径与代际）
#[test]
fn extract_task_watcher_discovers_new_task_dir() {
    let state_root = temp_test_dir("watch-extract-task");
    let tasks_root = state_root.join("tasks");
    std::fs::create_dir_all(tasks_root.join("old-task")).unwrap();
    let known = vec![tasks_root.join("old-task")];
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        watch_extract_task(&tasks_root, &known, 7, &sender, Duration::from_secs(10));
    });
    // 模拟引擎在任务开始的最初几步创建任务库目录
    std::thread::sleep(Duration::from_millis(150));
    std::fs::create_dir_all(state_root.join("tasks").join("new-task")).unwrap();
    match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(FailChannelMsg::TaskFound(path, generation)) => {
            assert!(
                path.ends_with("new-task"),
                "应发现快照之外的新任务目录：{path:?}"
            );
            assert_eq!(generation, 7, "发现消息必须携带发起时的清点代际");
        }
        other => panic!("watcher 应回传任务目录发现消息：{other:?}"),
    }
    let _ = std::fs::remove_dir_all(&state_root);
}

// 覆盖 H-02 / G-13（暂停按钮：整理/解压任务经真实 Control 暂停与继续；
// Git、转 Markdown 与无任务时如实提示不支持，不翻转 paused 显示）
#[test]
fn pause_button_toggles_organizer_and_reports_unsupported_runtimes() {
    with_gui(|app| {
        let ui = &app.ui;
        // 无任务（control=None）：不得伪装暂停成功
        ui.invoke_pause_task();
        assert!(
            ui.get_status().contains("不支持暂停"),
            "无任务时暂停必须如实提示"
        );
        // 目录整理任务：真实 Control 上暂停/继续切换（H-02 处理状态可见）
        let control = Arc::new(Control::default());
        app.state.borrow_mut().control = Some(control.clone());
        app.state.borrow_mut().runtime = RuntimeMode::Organizer;
        ui.invoke_pause_task();
        assert!(ui.get_paused(), "暂停后界面必须呈现已暂停");
        assert!(control.is_paused(), "暂停必须写入真实控制通道");
        assert!(ui.get_status().contains("已请求暂停"));
        ui.invoke_pause_task();
        assert!(!ui.get_paused(), "再次点击恢复处理");
        assert!(!control.is_paused());
        assert_eq!(ui.get_status().as_str(), "继续处理");
        // Git / 转 Markdown 运行口径不接入暂停检查点（G-13）：如实提示且不动 paused
        for unsupported in [RuntimeMode::Git, RuntimeMode::MarkdownConverter] {
            app.state.borrow_mut().runtime = unsupported;
            ui.invoke_pause_task();
            assert!(ui.get_status().contains("不支持暂停"));
            assert!(!ui.get_paused(), "不支持的工具不得翻转 paused 显示");
        }
    })
    .unwrap();
}

// 覆盖 H-02（取消入口可用：control 命中即取消；无共享控制器的转换、组件初始化
// 与截图初始化各自回落独立取消原子量，不谎报取消而任务继续）
#[test]
fn cancel_button_cancels_control_and_falls_back_to_dedicated_atoms() {
    with_gui(|app| {
        let ui = &app.ui;
        let control = Arc::new(Control::default());
        app.state.borrow_mut().control = Some(control.clone());
        ui.invoke_cancel_task();
        assert!(control.is_cancelled(), "取消必须写入真实控制通道");
        assert!(ui.get_status().contains("正在取消"));

        let convert_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.state.borrow_mut().control = None;
        app.state.borrow_mut().convert_cancel = Some(convert_cancel.clone());
        ui.invoke_cancel_task();
        assert!(
            convert_cancel.load(Ordering::Acquire),
            "转 Markdown 任务必须经独立原子量取消"
        );

        let init_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.state.borrow_mut().convert_cancel = None;
        app.state.borrow_mut().convert_init_cancel = Some(init_cancel.clone());
        ui.invoke_cancel_task();
        assert!(
            init_cancel.load(Ordering::Acquire),
            "转 Markdown 组件初始化必须可取消"
        );

        let snap_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.state.borrow_mut().convert_init_cancel = None;
        app.state.borrow_mut().snap_init_cancel = Some(snap_cancel.clone());
        ui.invoke_cancel_task();
        assert!(
            snap_cancel.load(Ordering::Acquire),
            "截图 OCR 初始化取消入口必须可用（O-06）"
        );
        let control = Arc::new(Control::default());
        let convert_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let convert_init_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let snap_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut state = app.state.borrow_mut();
            state.control = Some(control.clone());
            state.convert_cancel = Some(convert_cancel.clone());
            state.convert_init_cancel = Some(convert_init_cancel.clone());
            state.snap_init_cancel = Some(snap_cancel.clone());
        }
        ui.set_busy(true);
        ui.set_convert_preparing(true);
        ui.set_convert_initializing(true);
        ui.set_snap_initializing(true);
        ui.invoke_cancel_task();
        assert!(control.is_cancelled());
        assert!(convert_cancel.load(Ordering::Acquire));
        assert!(convert_init_cancel.load(Ordering::Acquire));
        assert!(snap_cancel.load(Ordering::Acquire));

        let control = Arc::new(Control::default());
        let convert_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let convert_init_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let snap_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut state = app.state.borrow_mut();
            state.control = Some(control.clone());
            state.convert_cancel = Some(convert_cancel.clone());
            state.convert_init_cancel = Some(convert_init_cancel.clone());
            state.snap_init_cancel = Some(snap_cancel.clone());
        }
        ui.invoke_confirmed(3);
        assert!(control.is_cancelled());
        assert!(convert_cancel.load(Ordering::Acquire));
        assert!(convert_init_cancel.load(Ordering::Acquire));
        assert!(snap_cancel.load(Ordering::Acquire));
        ui.set_busy(false);
        ui.set_convert_preparing(false);
        ui.set_convert_initializing(false);
        ui.set_snap_initializing(false);
        let mut state = app.state.borrow_mut();
        state.control = None;
        state.convert_cancel = None;
        state.convert_init_cancel = None;
        state.snap_init_cancel = None;
    })
    .unwrap();
}
