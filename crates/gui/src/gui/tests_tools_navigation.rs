//! 工具路由、规则面板与导航用例（自 gui.rs 的 gui_tests 拆分）。
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

// 覆盖 P-02, P-04, H-03
#[test]
fn initial_surface_lists_defaults() {
    with_gui(|app| {
        let ui = &app.ui;
        assert!(!ui.get_ready(), "初始状态不得就绪");
        assert_eq!(ui.get_theme(), 2, "默认深色主题（P-04）");
        // O-01：截图 OCR 与原有工具并列，导航至独立工具页。
        let tools = ui.get_tools();
        let snap_visible = (0..tools.row_count())
            .filter_map(|index| tools.row_data(index))
            .any(|tool| tool.id == "snap-ocr");
        assert!(snap_visible, "侧栏必须提供截图 OCR 入口");
        ui.invoke_select_tool("snap-ocr".into());
        assert_eq!(ui.get_screen(), 6, "截图 OCR 必须进入独立页面");
        assert_eq!(ui.get_tool_search().as_str(), "", "启动不得预置搜索过滤");
    })
    .unwrap();
}

// 覆盖 R-01, C-02
#[test]
fn section_switch_swaps_rule_rows() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_section(3); // 安全与性能（目录整理：0去重/1归类/2清理/3安全与性能）
        assert_eq!(ui.get_section(), 3);
        assert_eq!(
            ui.get_rules().row_count(),
            2,
            "安全与性能默认只显示 2 条基础规则（全局删除方式 + 递归扫描）"
        );
        assert!(
            rule_value_at(ui, "hash_workers").is_none(),
            "Hash 并行工作线程属于高级层"
        );
        ui.invoke_toggle_advanced(true);
        assert!(ui.get_show_advanced());
        assert_eq!(
            ui.get_rules().row_count(),
            6,
            "打开高级层后安全与性能应显示全部 6 条（隐藏/系统属性/排除 glob/Hash 线程；磁盘预留属于递归解压）"
        );
        assert_eq!(rule_value_at(ui, "hash_workers").as_deref(), Some("6"));
        assert!(
            rule_value_at(ui, "reserve_bytes").is_none(),
            "磁盘预留属于递归解压的工具规则"
        );
        ui.invoke_toggle_advanced(false);
        ui.invoke_select_section(0); // 去重
        assert!(
            rule_value_at(ui, "dedup_same_name").is_some(),
            "去重开关在去重分区"
        );
        ui.invoke_toggle_advanced(true);
        assert!(
            rule_value_at(ui, "same_name_same_size").is_none(),
            "C-02：版本取舍开关不得出现在规则面板"
        );
        assert!(
            rule_value_at(ui, "same_size_keep").is_none(),
            "C-02：版本取舍保留规则不得出现"
        );
        assert!(
            rule_value_at(ui, "theme").is_none(),
            "主题不再作为目录整理的规则行"
        );
        assert!(
            rule_value_at(ui, "max_depth").is_none(),
            "解压规则不得出现在目录整理的面板"
        );
    })
    .unwrap();
}

// 覆盖 R-01, X-05, X-08（解压侧只剩安全与性能项：处置已固定，高级层是防护上限）
#[test]
fn advanced_rows_hidden_until_toggled() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("recursive-extract".into());
        ui.invoke_select_section(0); // 解压（递归解压：0解压/1安全与性能）
        assert_eq!(
            ui.get_rules().row_count(),
            0,
            "解压分区没有基础规则：原包处置与冲突策略已按 X-05/X-04 固定移除"
        );
        assert!(
            rule_value_at(ui, "max_depth").is_none(),
            "最大嵌套层数属于高级层"
        );
        assert!(
            rule_value_at(ui, "nested_archives").is_none(),
            "R-02：嵌套解压开关不得出现（嵌套解压始终开启，受最大嵌套层数限制）"
        );
        assert!(
            rule_value_at(ui, "dedup_same_name").is_none(),
            "去重规则不得出现在递归解压的面板"
        );
        ui.invoke_toggle_advanced(true);
        assert!(ui.get_show_advanced());
        assert_eq!(
            ui.get_rules().row_count(),
            4,
            "打开高级层后解压分区应显示全部 4 条防护上限（层数/条目/比例/磁盘预留）"
        );
        assert_eq!(rule_value_at(ui, "max_depth").as_deref(), Some("16"));
        assert!(
            rule_value_at(ui, "archive_delete").is_none(),
            "X-05/R-02：原包处置不得再是可配置项"
        );
        ui.invoke_toggle_advanced(false);
        assert_eq!(ui.get_rules().row_count(), 0, "关闭高级层必须恢复基础视图");
    })
    .unwrap();
}

// 覆盖 P-02, R-01, X-01（两工具规则分区互不串扰）
#[test]
fn tool_routing_swaps_screens_and_sections() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("recursive-extract".into());
        assert_eq!(ui.get_screen(), 2, "递归解压工具页");
        assert_eq!(ui.get_active_tool_id().as_str(), "recursive-extract");
        assert_eq!(app.state.borrow().tool, Tool::Extract);
        assert_eq!(ui.get_section(), 0, "切工具回到第一个分区");
        ui.invoke_toggle_advanced(true);
        assert!(
            rule_value_at(ui, "max_depth").is_some(),
            "解压分区的高级层应显示防护上限"
        );
        assert!(
            rule_value_at(ui, "archive_delete").is_none(),
            "X-05/R-02：原包处置不得再是可配置项"
        );
        // 安全是两工具共享分区：递归解压视角不含 Hash 线程；磁盘预留在解压分区（高级层）
        ui.invoke_select_section(1);
        assert!(
            rule_value_at(ui, "hash_workers").is_none(),
            "Hash 线程属于目录整理"
        );
        assert!(
            rule_value_at(ui, "reserve_bytes").is_none(),
            "磁盘预留属于解压分区，不在安全与性能"
        );
        ui.invoke_select_section(0);
        assert!(
            rule_value_at(ui, "reserve_bytes").is_some(),
            "磁盘预留属于递归解压"
        );
        ui.invoke_toggle_advanced(false);
        ui.invoke_select_tool("directory-organizer".into());
        assert_eq!(ui.get_screen(), 0, "切回目录整理路由不串");
        assert_eq!(ui.get_active_tool_id().as_str(), "directory-organizer");
        assert_eq!(app.state.borrow().tool, Tool::Organizer);
        assert!(
            rule_value_at(ui, "dedup_same_name").is_some(),
            "默认分区是「去重」"
        );
        assert!(
            rule_value_at(ui, "nested_archives").is_none(),
            "解压规则不得出现在目录整理面板"
        );
    })
    .unwrap();
}

// 覆盖 C-11, U-11（回归 2026-09-19：运行中切换工具不得谎报「任务已结束」）。
// 复现：确认执行的同一拍 busy 已同步置位、任务库状态已是 executing，此时切走
// 再切回工具，on_select_tool → sync_ready_after_directory 的就绪重算会把一切非
// ready 状态归为「已结束」，状态栏谎报「上次任务已结束」；切到递归解压方向则谎报
// 「目录已就绪」。运行中文案由任务事件负责，工具切换不得改写。
#[test]
fn switching_tools_while_busy_keeps_running_status_text() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("data");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), b"same").unwrap();
    std::fs::write(root.join("b.txt"), b"same").unwrap();
    let state_dir = tmp.path().join("state");
    std::fs::create_dir(&state_dir).unwrap();
    let prepared =
        organizer_engine::prepare_at(&root, Config::default(), Context::default(), &state_dir)
            .unwrap();
    // 任务执行中：apply_with 执行期任务库状态为 executing（非 ready）。
    Database::open_existing(&prepared.directory)
        .unwrap()
        .set("status", &"executing".to_string())
        .unwrap();
    let task = prepared.directory;
    let directory = root.to_string_lossy().to_string();
    with_gui(move |app| {
        let ui = &app.ui;
        app.state.borrow_mut().task = Some(task.clone());
        ui.set_has_task(true);
        ui.set_busy(true);
        ui.set_directory(directory.clone().into());
        ui.set_status("正在执行已确认的整理计划".into());
        ui.invoke_select_tool("recursive-extract".into());
        ui.invoke_select_tool("directory-organizer".into());
        // 排空一段窗口：若工具切换确实发起了就绪重算，其快照会在这段时间落地并改写文案。
        for _ in 0..20 {
            app.pump.run(ui);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ui.get_busy(), "切换工具不得改变运行状态");
        assert_eq!(
            ui.get_status().as_str(),
            "正在执行已确认的整理计划",
            "运行中切换工具不得把状态文案改写为「已结束/已就绪」（C-11 如实展示）"
        );
    })
    .unwrap();
}

// 覆盖 R-01, R-02, C-02
#[test]
fn dependent_rows_follow_their_switches() {
    with_gui(|app| {
        let ui = &app.ui;
        // 清理：修正扩展名不再依赖独立的类型检测行（影子键随行联动），位于高级层
        ui.invoke_select_section(2);
        ui.invoke_toggle_advanced(true);
        assert!(
            rule_value_at(ui, "fix_extension").is_some(),
            "修正扩展名在清理分区的高级层可见"
        );
        assert!(
            rule_value_at(ui, "detect_type").is_none(),
            "类型检测不再单列为规则行"
        );
        ui.invoke_rule_bool("fix_extension".into(), true);
        assert!(
            app.state.borrow().config.detect_type,
            "开启修正扩展名必须自动开启类型检测"
        );
        ui.invoke_toggle_advanced(false);
        // 归类：R-02 已删除自定义分类规则与 classify 开关；大文件阈值（字节口径，
        // 高级层）依赖大文件单独归类开启后才显示
        ui.invoke_select_section(1);
        assert!(rule_value_at(ui, "custom_categories").is_none());
        assert!(
            rule_value_at(ui, "classify").is_none(),
            "R-02：归类方式开关已删除（固定「大类」一级结构）"
        );
        assert!(rule_value_at(ui, "large_threshold_bytes").is_none());
        ui.invoke_toggle_advanced(true);
        ui.invoke_rule_bool("large_files".into(), true);
        assert!(rule_value_at(ui, "large_threshold_bytes").is_some());
        ui.invoke_toggle_advanced(false);
        // 去重（R-02）：同名/副本名/不同名三个独立开关 + 重复组保留规则；
        // C-02 禁止版本取舍开关后，去重基础层固定 4 行，不再有随开关出现/收回的行。
        ui.invoke_select_section(0);
        assert!(
            rule_value_at(ui, "dedup_same_name").is_some()
                && rule_value_at(ui, "dedup_copy_names").is_some()
                && rule_value_at(ui, "dedup_other_names").is_some(),
            "去重三类开关必须同屏独立出现"
        );
        assert_eq!(
            ui.get_rules().row_count(),
            4,
            "去重基础层：三个去重开关 + 保留规则"
        );
        // R-02：解压侧不再有冲突删除方式行；目录整理任何分区都不得出现解压专用项。
        ui.invoke_select_tool("recursive-extract".into());
        ui.invoke_select_section(0);
        ui.invoke_toggle_advanced(true);
        assert!(
            rule_value_at(ui, "conflict_delete").is_none()
                && rule_value_at(ui, "extract_conflict").is_none(),
            "X-04/R-02：解压冲突处置已固定，解压面板不得再有冲突策略行"
        );
        ui.invoke_toggle_advanced(false);
        ui.invoke_select_tool("directory-organizer".into());
        for section in 0..4 {
            ui.invoke_select_section(section);
        }
        ui.invoke_toggle_advanced(true);
        assert!(
            rule_value_at(ui, "conflict_delete").is_none(),
            "目录整理面板不含解压覆盖删除方式行"
        );
        ui.invoke_toggle_advanced(false);
    })
    .unwrap();
}

// 覆盖 R-02, C-08
#[test]
fn merged_rows_write_shadow_fields() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_section(0); // 去重
                                     // R-02：同名/副本名/不同名同内容三类独立启停，互不联动。
        ui.invoke_rule_bool("dedup_same_name".into(), false);
        {
            let cfg = &app.state.borrow().config;
            assert!(!cfg.dedup_same_name, "同名去重开关只写自己");
            assert!(
                cfg.dedup_copy_names && !cfg.dedup_other_names,
                "副本名/不同名去重必须保持独立，不得被联动（各自保持默认值：副本名开、不同名关）"
            );
        }
        ui.invoke_rule_bool("dedup_copy_names".into(), false);
        ui.invoke_rule_bool("dedup_other_names".into(), true);
        {
            let cfg = &app.state.borrow().config;
            assert!(
                !cfg.dedup_copy_names && cfg.dedup_other_names,
                "两类开关各自独立生效（不同名默认关闭，可手动开启）"
            );
        }
        // C-02 禁止版本淘汰开关后，剩余合并行：修正扩展名一行仍驱动两个细粒度字段。
        ui.invoke_select_section(2);
        ui.invoke_rule_bool("fix_extension".into(), true);
        {
            let cfg = &app.state.borrow().config;
            assert!(
                cfg.fix_extension && cfg.detect_type,
                "修正扩展名一行必须同时开启类型检测"
            );
        }
    })
    .unwrap();
}

// 覆盖 R-02 / 附录 D：容量按最终整数字节校验，不因十进制中间量溢出拒绝合法输入。
#[test]
fn capacity_rules_accept_exact_byte_fractions_without_intermediate_overflow() {
    for text in [
        "0.00000095367431640625 MiB",
        "0.000000000931322574615478515625 GiB",
        "0.0000000000009094947017729282379150390625 TiB",
    ] {
        assert_eq!(parse_capacity(text, true), Ok(1), "{text}");
    }
    assert_eq!(parse_capacity("0.5 MiB", true), Ok(524288));
    assert_eq!(
        parse_capacity("1.0000000000000000000000000000000000000000 B", true),
        Ok(1)
    );
    assert!(parse_capacity("0.1 KiB", true).is_err());
    assert!(parse_capacity("18446744073709551616 B", true).is_err());
    assert!(parse_capacity("16777216 TiB", true).is_err());
    with_gui(|app| {
        app.ui.invoke_select_tool("recursive-extract".into());
        app.ui.invoke_select_section(0);
        app.ui.invoke_toggle_advanced(true);
        app.ui.invoke_rule_text(
            "reserve_bytes".into(),
            "0.000000000931322574615478515625 GiB".into(),
        );
        assert_eq!(app.state.borrow().config.reserve_bytes, 1);
        assert!(app.ui.get_error_text().is_empty());
        assert_eq!(
            rule_value_at(&app.ui, "reserve_bytes").as_deref(),
            Some("1")
        );
    })
    .unwrap();
}

// 覆盖 U-06 / 附录 D：非法输入保留供修正，并阻止以旧配置开始。
#[test]
fn invalid_number_input_reports_error_preserves_value_and_blocks_start() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("recursive-extract".into());
        ui.invoke_toggle_advanced(true); // 最大嵌套层数属于高级层，先让它显示出来
        ui.invoke_rule_text("max_depth".into(), "abc".into());
        assert!(
            ui.get_error_text().contains("非负整数"),
            "必须提示非法输入：{}",
            ui.get_error_text()
        );
        assert_eq!(
            rule_value_at(ui, "max_depth").as_deref(),
            Some("abc"),
            "非法输入必须保留，不能替用户恢复旧配置"
        );
        let root = temp_test_dir("invalid-number-start");
        ui.set_directory(root.display().to_string().into());
        ui.invoke_request_extract_start();
        assert_eq!(ui.get_confirm_kind(), 0, "非法数字不得启动解压清点");
        assert!(!ui.get_busy(), "非法数字不得启动解压任务");
        ui.invoke_rule_text("max_depth".into(), "16".into());
        assert!(ui.get_error_text().is_empty(), "修正输入后必须恢复合法状态");
    })
    .unwrap();
}

#[test]
fn invalid_number_draft_survives_navigation_and_empty_and_overflow_inputs() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("recursive-extract".into());
        ui.invoke_toggle_advanced(true);
        for text in ["", "18446744073709551616", "4294967296"] {
            ui.invoke_rule_text("max_depth".into(), text.into());
            ui.invoke_select_section(1);
            ui.invoke_select_section(0);
            assert_eq!(rule_value_at(ui, "max_depth").as_deref(), Some(text));
            ui.invoke_request_extract_start();
            assert_eq!(ui.get_confirm_kind(), 0);
            assert!(!ui.get_error_text().is_empty(), "空值/溢出须显示字段错误");
        }
    })
    .unwrap();
}

#[test]
fn invalid_large_threshold_is_ignored_only_until_rule_reenabled() {
    let root = temp_test_dir("invalid-large-threshold");
    with_gui(move |app| {
        let ui = &app.ui;
        ui.invoke_select_tool("directory-organizer".into());
        ui.invoke_select_section(1);
        ui.invoke_toggle_advanced(true);
        ui.invoke_rule_bool("large_files".into(), true);
        ui.invoke_rule_text("large_threshold_bytes".into(), "abc".into());
        ui.invoke_rule_bool("large_files".into(), false);
        assert!(
            ui.get_error_text().is_empty(),
            "关闭附属规则不应阻止无关处理"
        );
        ui.invoke_select_tool("recursive-extract".into());
        ui.set_directory(root.display().to_string().into());
        ui.invoke_request_extract_start();
        assert_eq!(ui.get_confirm_kind(), 1, "整理草稿不得阻止独立解压工具");
        ui.set_confirm_kind(0);
        ui.invoke_select_tool("directory-organizer".into());
        ui.invoke_select_section(1);
        ui.invoke_rule_bool("large_files".into(), true);
        ui.invoke_request_start();
        assert!(!ui.get_busy(), "重新启用非法附属字段必须禁止分析");
        assert!(!ui.get_error_text().is_empty());
        assert_eq!(
            rule_value_at(ui, "large_threshold_bytes").as_deref(),
            Some("abc")
        );
    })
    .unwrap();
}

#[test]
fn directory_dialog_uses_own_field_start() {
    let root = temp_test_dir("directory-dialog-fields");
    let fields = [
        "shared",
        "md-input",
        "md-output",
        "md-split",
        "git",
        "convert-input",
        "convert-output",
    ];
    for field in fields {
        std::fs::create_dir_all(root.join(field)).unwrap();
    }
    with_gui(move |app| {
        let ui = &app.ui;
        ui.set_directory(root.join("shared").display().to_string().into());
        ui.set_md_input_dir(root.join("md-input").display().to_string().into());
        ui.set_md_output_dir(root.join("md-output").display().to_string().into());
        ui.set_md_split_dir(root.join("md-split").display().to_string().into());
        ui.set_git_repo(root.join("git").display().to_string().into());
        ui.set_convert_input_dir(root.join("convert-input").display().to_string().into());
        ui.set_convert_output_dir(root.join("convert-output").display().to_string().into());
        for entered in [
            ui.get_md_input_dir(),
            ui.get_md_output_dir(),
            ui.get_md_split_dir(),
            ui.get_git_repo(),
            ui.get_convert_input_dir(),
            ui.get_convert_output_dir(),
        ] {
            assert_eq!(
                directory_dialog_start(entered.as_str()),
                Some(PathBuf::from(entered.as_str())),
                "U-07：每个目录选择须使用当前字段，而不是其他工具的目录"
            );
        }
        assert_eq!(
            directory_dialog_start(""),
            None,
            "空输入不得回退其他工具字段"
        );
    })
    .unwrap();
}

// 覆盖 C-10, P-04
#[test]
fn theme_choice_keeps_plan_ready_but_rule_change_invalidates() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_ready(true);
        ui.invoke_select_theme(2); // 深色（「关于」页的主题选择）
        assert_eq!(ui.get_theme(), 2);
        assert!(ui.get_ready(), "纯外观的主题切换不得使已生成的计划失效");
        ui.invoke_rule_bool("clean_temp".into(), false);
        assert!(!ui.get_ready(), "实际规则变更必须使计划失效并要求重新分析");
    })
    .unwrap();
}

// 覆盖 C-10, C-11, R-04（切换工具不是规则或目录改动，就绪计划必须保持可执行）
#[test]
fn switching_tools_keeps_ready_plan_executable() {
    with_gui(|app| {
        let ui = &app.ui;
        let dir = temp_test_dir("switch-keeps-ready");
        let root = jchtools_file_core::fsutil::normalize_root(&dir).unwrap();
        {
            let db = Database::create(&dir).unwrap();
            db.set(
                "root",
                &jchtools_file_core::fsutil::path_string(&root).unwrap(),
            )
            .unwrap();
            db.set("config", &app.state.borrow().config.clone())
                .unwrap();
            db.set("status", &"ready").unwrap();
        }
        app.state.borrow_mut().task = Some(dir.clone());
        ui.set_directory(dir.display().to_string().into());
        ui.set_has_task(true);
        ui.set_ready(true);
        ui.invoke_select_tool("recursive-extract".into());
        ui.invoke_select_tool("directory-organizer".into());
        // 就绪重算在后台 worker 上完成（开库 + canonicalize 不进界面线程）：等快照落地。
        assert!(
            pump_until(app, || ui.get_ready()),
            "任务、配置与目录都没变，切工具往返后计划必须仍可执行：status={}",
            ui.get_status()
        );
        assert!(
            !ui.get_status().contains("已改变"),
            "不得谎报规则或目录已改变：{}",
            ui.get_status()
        );
        let _ = std::fs::remove_dir_all(&dir);
    })
    .unwrap();
}

// 覆盖 U-11/H-02：整理任务后台收尾时，不得把用户当前选中的另一工具面板顶走。
#[test]
fn organizer_finish_does_not_replace_another_tool_panel() {
    with_gui(|app| {
        let ui = &app.ui;
        let dir = temp_test_dir("finish-while-other-tool-visible");
        ui.invoke_select_tool("md-organizer".into());
        ui.set_status("MD 工具正在显示".into());
        assert_eq!(ui.get_panel(), 0);
        app.pump
            .finish_task(ui, dir.clone(), &Summary::default(), true);
        assert_eq!(ui.get_active_tool_id().as_str(), "md-organizer");
        assert_eq!(ui.get_panel(), 0, "后台整理收尾不得切换其它工具面板");
        assert_eq!(ui.get_status().as_str(), "MD 工具正在显示");
        let _ = std::fs::remove_dir_all(dir);
    })
    .unwrap();
}

// 覆盖 C-01：依赖重算在途时拒绝新的勾选保存，避免两个重算线程并发修改同一任务库。
#[test]
fn plan_toggle_is_gated_during_dependency_recompute() {
    with_gui(|app| {
        let dir = temp_test_dir("plan-recompute-toggle-gate");
        app.state.borrow_mut().task = Some(dir.clone());
        app.state.borrow_mut().plan_recompute_inflight = 1;
        app.ui.invoke_plan_toggle("1".into(), false);
        assert_eq!(app.state.borrow().pending_selection, 0);
        assert_eq!(app.state.borrow().plan_recompute_inflight, 1);
        let _ = std::fs::remove_dir_all(dir);
    })
    .unwrap();
}

// C-01：快速连续勾选按用户顺序串行保存，数据库与即时 UI 最终都保留最后一次选择。
#[test]
fn rapid_plan_toggles_persist_last_intent() {
    with_gui(|app| {
        let task = temp_test_dir("plan-toggle-order");
        let id = {
            let db = Database::create(&task).unwrap();
            db.set("status", &"ready".to_string()).unwrap();
            db.add_action(&jchtools_file_core::model::Action {
                id: 0,
                kind: jchtools_file_core::model::ActionKind::Delete,
                source: "synthetic.txt".into(),
                target: None,
                reason: "测试".into(),
                expected: None,
                keeper: None,
                hash: None,
                mode: jchtools_file_core::config::DeleteMode::Permanent,
                selected: false,
                state: "pending".into(),
            })
            .unwrap()
        };
        app.state.borrow_mut().task = Some(task.clone());
        app.ui.set_plans(
            Rc::new(VecModel::from(vec![PlanRow {
                id: id.to_string().into(),
                selected: false,
                kind: "删除".into(),
                source: "synthetic.txt".into(),
                target: "".into(),
                reason: "测试".into(),
                state: "已取消勾选".into(),
            }]))
            .into(),
        );
        app.ui.invoke_plan_toggle(id.to_string().into(), true);
        app.ui.invoke_plan_toggle(id.to_string().into(), false);
        assert_eq!(app.state.borrow().pending_selection, 2);
        assert!(
            !app.ui.get_plans().row_data(0).unwrap().selected,
            "最后一次取消勾选必须即时生效"
        );
        // 阻止测试计划进入依赖重算；保存路径仍是 GUI 回调与真实任务库。
        app.state.borrow_mut().plan_recompute_inflight = 1;
        assert!(
            pump_until(app, || app.state.borrow().pending_selection == 0),
            "排队的勾选写入应收尾"
        );
        assert!(
            !Database::open_existing(&task)
                .unwrap()
                .action(id)
                .unwrap()
                .selected,
            "数据库必须保留最后一次明确取消勾选"
        );
        assert!(
            !app.ui.get_plans().row_data(0).unwrap().selected,
            "较早保存完成不得把更新的 UI 勾选改回"
        );
        let _ = std::fs::remove_dir_all(task);
    })
    .unwrap();
}

// C-01：即使较早成功事件先到，仍在途的较新意图也不得被回写覆盖。
#[test]
fn stale_selection_completion_keeps_latest_ui_choice() {
    with_gui(|app| {
        let task = PathBuf::from("stale-selection-event");
        app.state.borrow_mut().task = Some(task.clone());
        app.state.borrow_mut().pending_selection = 2;
        app.ui.set_busy(true);
        app.ui.set_plans(
            Rc::new(VecModel::from(vec![PlanRow {
                id: "41".into(),
                selected: false,
                kind: "删除".into(),
                source: "synthetic.txt".into(),
                target: "".into(),
                reason: "测试".into(),
                state: "已取消勾选".into(),
            }]))
            .into(),
        );

        app.pump
            .out
            .send(Event::SelectionSaved(task.clone(), Some((41, true)), None))
            .unwrap();
        app.pump.run(&app.ui);
        assert_eq!(app.state.borrow().pending_selection, 1);
        assert!(
            !app.ui.get_plans().row_data(0).unwrap().selected,
            "较早勾选完成事件不得撤销用户较新的取消勾选"
        );

        app.pump
            .out
            .send(Event::SelectionSaved(task, Some((41, false)), None))
            .unwrap();
        app.pump.run(&app.ui);
        assert!(!app.ui.get_plans().row_data(0).unwrap().selected);
    })
    .unwrap();
}

// 覆盖 U-11（切工具回到默认面板；共享面板索引不得停留在另一工具才有的面板）
#[test]
fn switching_tools_resets_shared_panel_to_default() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_panel(2);
        ui.invoke_select_tool("recursive-extract".into());
        assert_eq!(
            ui.get_panel(),
            0,
            "递归解压没有第三面板，panel=2 会让内容区整块空白"
        );
        ui.set_panel(1);
        ui.invoke_select_tool("directory-organizer".into());
        assert_eq!(
            ui.get_panel(),
            0,
            "切回目录整理必须回到默认面板「处理规则」"
        );
    })
    .unwrap();
}

// 覆盖 C-11（已结束任务不得谎报「规则或目录已改变」，状态须如实）
#[test]
fn switching_back_to_finished_task_reports_finished_status() {
    with_gui(|app| {
        let ui = &app.ui;
        let dir = temp_test_dir("switch-finished");
        let root = jchtools_file_core::fsutil::normalize_root(&dir).unwrap();
        {
            let db = Database::create(&dir).unwrap();
            db.set(
                "root",
                &jchtools_file_core::fsutil::path_string(&root).unwrap(),
            )
            .unwrap();
            db.set("config", &app.state.borrow().config.clone())
                .unwrap();
            db.set("status", &"finished").unwrap();
        }
        app.state.borrow_mut().task = Some(dir.clone());
        ui.set_directory(dir.display().to_string().into());
        ui.set_has_task(true);
        ui.invoke_select_tool("recursive-extract".into());
        ui.invoke_select_tool("directory-organizer".into());
        // 就绪重算在后台 worker 上完成：等快照落地后按库真值断言（C-11 如实展示）。
        assert!(
            pump_until(app, || ui.get_status().contains("已结束")),
            "任务只是已结束，状态必须如实说明而不是谎报改动：{}",
            ui.get_status()
        );
        assert!(!ui.get_ready(), "已结束的任务不得重新点亮执行");
        assert!(
            !ui.get_plan_editable(),
            "已结束的任务不得再允许勾选（勾选必然写不进任务库）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    })
    .unwrap();
}

// 覆盖 P-02
#[test]
fn search_tools_filters_registry() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_search_tools("目录".into());
        assert_eq!(ui.get_tools().row_count(), 1);
        ui.invoke_search_tools("不存在的工具名".into());
        assert_eq!(ui.get_tools().row_count(), 0);
    })
    .unwrap();
}

// 覆盖 CA-01 / CA-04 / CA-06：TestingBackend 上的真实导航、保存回调及去敏状态呈现；
// 后台真实 DPAPI / SQLite / 单实例生命周期由 tests/codeagent.rs 承接。
#[test]
fn codeagent_page_uses_real_callbacks_and_safe_status() {
    with_gui(|app| {
        let (commands, receiver) = mpsc::channel();
        let (sender, messages) = mpsc::channel();
        app.state.borrow_mut().codeagent_observer = Some(CodeAgentObserver { commands, messages });
        app.ui.invoke_select_tool("codeagent".into());
        assert_eq!(app.ui.get_screen(), 8);
        assert_eq!(app.ui.get_active_tool_id().as_str(), "codeagent");
        assert_eq!(app.ui.get_panel(), 0);
        assert!(matches!(
            receiver.try_recv().unwrap(),
            CodeAgentCommand::Ensure
        ));
        let status = jchtools_codeagent::codeagent::Status {
            running: true,
            pid: Some(12345),
            interval_seconds: 60,
            last_check: Some("2026-10-12T12:34:56+08:00".into()),
            result: jchtools_codeagent::codeagent::CheckResult::Unchanged,
        };
        sender
            .send(CodeAgentMessage {
                saved: None,
                status: Ok((status, 60)),
            })
            .unwrap();
        app.pump.run(&app.ui);
        assert!(app.ui.get_codeagent_running());
        assert_eq!(
            app.ui.get_codeagent_last_check().as_str(),
            "2026-10-12T12:34:56+08:00"
        );
        assert_eq!(
            app.ui.get_codeagent_result().as_str(),
            "Token 未变化，无需写入"
        );
        app.ui.set_codeagent_interval("2".into());
        app.ui.set_codeagent_draft_edited(true);
        app.ui.invoke_codeagent_save_interval();
        assert!(app.ui.get_codeagent_pending());
        assert!(
            matches!(receiver.try_recv().unwrap(), CodeAgentCommand::Save(value) if value == "2")
        );
        let status = jchtools_codeagent::codeagent::Status {
            running: true,
            pid: Some(12345),
            interval_seconds: 2,
            last_check: Some("2026-10-12T12:34:58+08:00".into()),
            result: jchtools_codeagent::codeagent::CheckResult::DecryptFailed,
        };
        sender
            .send(CodeAgentMessage {
                saved: Some(Ok(())),
                status: Ok((status, 2)),
            })
            .unwrap();
        app.pump.run(&app.ui);
        assert!(!app.ui.get_codeagent_pending());
        assert!(!app.ui.get_codeagent_draft_edited());
        assert!(app.ui.get_codeagent_check_failed());
        assert_eq!(app.ui.get_codeagent_interval().as_str(), "2");
        assert_eq!(
            app.ui.get_codeagent_result().as_str(),
            "CurrentUser DPAPI 解密失败；保留已有 Token"
        );
        assert!(app.ui.get_codeagent_effective_interval().contains("2 秒"));
    })
    .unwrap();
}

// 覆盖 P-02
#[test]
fn navigation_switches_screen() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_navigation(1);
        assert_eq!(ui.get_screen(), 1, "关于页");
        ui.invoke_navigation(0);
        assert_eq!(ui.get_screen(), 0);
    })
    .unwrap();
}

// 覆盖 R-04：选择新目录后必须立即撤销旧计划执行权，不能等磁盘读取返回。
#[test]
fn directory_recheck_disarms_before_worker_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("old");
    let other = tmp.path().join("new");
    let storage = tmp.path().join("state");
    for path in [&root, &other, &storage] {
        std::fs::create_dir(path).unwrap();
    }
    let prepared =
        organizer_engine::prepare_at(&root, Config::default(), Context::default(), &storage)
            .unwrap();
    with_gui(move |app| {
        app.state.borrow_mut().task = Some(prepared.directory);
        app.state.borrow_mut().tool = Tool::Organizer;
        app.ui.set_has_task(true);
        app.ui.set_ready(true);
        app.ui.set_directory(other.display().to_string().into());
        sync_ready_after_directory(&app.ui, &app.state.borrow(), &app.pump.out);
        assert!(!app.ui.get_ready(), "目录已改变时旧计划不得继续执行");
        assert!(pump_until(app, || {
            app.ui.get_status().as_str() == PlanReadyState::Changed.status_text()
        }));
    })
    .unwrap();
}

// 覆盖 R-04、C-11：翻页请求顶替目录检查时，仍须解释为何计划已经失效。
#[test]
fn directory_recheck_status_survives_superseding_page_request() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("old");
    let other = tmp.path().join("new");
    let storage = tmp.path().join("state");
    for path in [&root, &other, &storage] {
        std::fs::create_dir(path).unwrap();
    }
    let prepared =
        organizer_engine::prepare_at(&root, Config::default(), Context::default(), &storage)
            .unwrap();
    with_gui(move |app| {
        app.state.borrow_mut().task = Some(prepared.directory.clone());
        app.state.borrow_mut().tool = Tool::Organizer;
        app.ui.set_has_task(true);
        app.ui.set_ready(true);
        app.ui
            .set_status(PlanReadyState::Ready.status_text().into());
        app.ui.set_directory(other.display().to_string().into());
        sync_ready_after_directory(&app.ui, &app.state.borrow(), &app.pump.out);
        load_plan_state(
            &app.pump.out,
            &app.state.borrow().plan_load,
            prepared.directory,
            app.ui.get_directory().to_string(),
            PlanQuery::page(0, 0, None, false),
        );
        assert!(
            pump_until(app, || {
                app.ui.get_status().as_str() == PlanReadyState::Changed.status_text()
            }),
            "较新的页面结果不得遗失目录改变后的状态反馈"
        );
        assert!(!app.ui.get_ready());
    })
    .unwrap();
}

// ---- 纯函数回归：执行分母计数与就绪判定（不依赖 GUI 工作线程）----
