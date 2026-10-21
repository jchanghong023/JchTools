//! 转 Markdown 转换流程与状态用例（自 gui.rs 的 gui_tests 拆分）。
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

// 覆盖 T-01/T-05/T-07：公开导航进入转 Markdown 页面时，默认选项完整且未初始化不得启动；
// 切回旧工具后状态栏不得残留新工具文案。
#[test]
fn markdown_converter_navigation_is_isolated_before_initialization() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("markdown-converter".into());
        assert_eq!(ui.get_screen(), 5, "转 Markdown 必须进入独立页面");
        assert_eq!(ui.get_active_tool_id(), "markdown-converter");
        assert!(ui.get_convert_pdf(), "默认应启用 PDF");
        assert!(ui.get_convert_office(), "默认应启用 Office");
        assert!(ui.get_convert_images(), "默认应启用图片");
        assert!(ui.get_convert_media(), "默认应启用 MP4/M4A");
        assert!(ui.get_convert_other(), "默认应启用其他格式");
        assert!(!ui.get_convert_flat(), "默认应保留输入目录层级");
        assert_eq!(ui.get_convert_timeout_secs().as_str(), "21600");
        assert!(!ui.get_convert_ready(), "未初始化时组件不得标记为就绪");
        assert!(!ui.get_busy(), "未初始化时不得存在转换任务");

        // 通过公开回调尝试启动：未就绪必须被 GUI 门禁拦截。
        let before = ui.get_status().to_string();
        ui.invoke_convert_start();
        assert!(!ui.get_busy(), "未初始化时开始转换必须保持禁用");
        assert_eq!(ui.get_status().as_str(), before);

        // T-29：即使测试直接注入 ready，超出系统可表示范围的正整数也必须
        // 明确拒绝且不得进入 busy；不得恢复旧的静默钳制行为。
        let timeout_root = temp_test_dir("convert-navigation-timeout");
        let timeout_input = timeout_root.join("input");
        let timeout_output = timeout_root.join("output");
        std::fs::create_dir_all(&timeout_input).unwrap();
        std::fs::create_dir_all(&timeout_output).unwrap();
        ui.set_convert_ready(true);
        ui.set_convert_input_dir(timeout_input.display().to_string().into());
        ui.set_convert_output_dir(timeout_output.display().to_string().into());
        ui.set_convert_timeout_secs(u64::MAX.to_string().into());
        ui.invoke_convert_start();
        assert!(!ui.get_busy(), "不可表示的超时必须禁止启动转换");
        assert!(ui.get_error_text().contains("超出系统可表示范围"));
        let _ = std::fs::remove_dir_all(timeout_root);

        // 覆盖 T-23/T-24：部分失败和主动停止必须呈现不同的最终状态。
        ui.set_busy(true);
        // 覆盖 T-24：单文件失败的文件名和原因在批次收尾后仍可从 GUI 日志查看。
        app.pump
            .out
            .send(Event::Status(
                "CONVERTER_FILE_FINISHED|0|0|broken.pdf|解析失败".into(),
            ))
            .unwrap();
        app.pump.run(ui);
        app.pump
            .out
            .send(Event::MdDone("CONVERTER_DONE|0|0|1|0|0|0".into()))
            .unwrap();
        app.pump.run(ui);
        assert_eq!(ui.get_convert_status().as_str(), "转换部分失败");
        assert!(ui.get_convert_log_text().contains("broken.pdf"));
        assert!(ui.get_convert_log_text().contains("解析失败"));
        ui.set_busy(true);
        app.pump
            .out
            .send(Event::MdDone("CONVERTER_DONE|0|1|0|0|0|0".into()))
            .unwrap();
        app.pump.run(ui);
        assert_eq!(ui.get_convert_status().as_str(), "转换部分失败");
        ui.set_busy(true);
        app.pump
            .out
            .send(Event::MdDone("CONVERTER_DONE|0|0|0|0|0|1".into()))
            .unwrap();
        app.pump.run(ui);
        assert_eq!(ui.get_convert_status().as_str(), "已停止");

        ui.invoke_select_tool("directory-organizer".into());
        assert_eq!(ui.get_screen(), 0, "切回目录整理必须进入旧页面");
        assert_eq!(ui.get_status().as_str(), "请选择需要整理的目录");
        assert!(
            !ui.get_status().contains("转 Markdown"),
            "旧工具状态不得残留转 Markdown 文案"
        );
    })
    .unwrap();
}

// 覆盖 T-23/T-24/XB-08：停止请求不能掩盖取消异常，原请求未结束的诊断应保留。
#[test]
fn converter_stop_with_failure_preserves_failed_final_state_and_diagnostic() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("markdown-converter".into());
        ui.set_busy(true);
        let diagnostic = "取消后原转换请求仍未结束，无法安全继续共享会话";
        app.pump
            .out
            .send(Event::Status(format!(
                "CONVERTER_FILE_FINISHED|0|0|blocked.pdf|{diagnostic}"
            )))
            .unwrap();
        app.pump
            .out
            .send(Event::MdDone("CONVERTER_DONE|0|0|1|0|0|1".into()))
            .unwrap();
        app.pump.run(ui);
        assert_eq!(ui.get_convert_status().as_str(), "已停止，转换失败");
        assert!(ui.get_status().contains("已停止"));
        assert!(ui.get_status().contains("失败"));
        assert!(ui.get_convert_progress_note().contains("失败"));
        assert!(ui.get_convert_metrics().contains("失败 1"));
        assert!(ui.get_convert_log_text().contains("blocked.pdf"));
        assert!(ui.get_convert_log_text().contains(diagnostic));
        assert!(!ui.get_busy());

        ui.set_busy(true);
        app.pump
            .out
            .send(Event::MdDone("CONVERTER_DONE|0|0|0|0|0|1".into()))
            .unwrap();
        app.pump.run(ui);
        assert_eq!(ui.get_convert_status().as_str(), "已停止");
        assert!(!ui.get_status().contains("失败"), "正常取消不报告转换失败");

        ui.set_busy(true);
        app.pump
            .out
            .send(Event::MdDone("CONVERTER_DONE|0|0|1|0|0|0".into()))
            .unwrap();
        app.pump.run(ui);
        assert_eq!(ui.get_convert_status().as_str(), "转换部分失败");
    })
    .unwrap();
}

// 覆盖 T-05/T-06/U：组件缺失的完整清单进入独立折叠详情，页面状态与任务日志
// 保持简短，不把十几项路径直接铺在页头、横幅或执行日志里。
#[test]
fn converter_readiness_long_detail_stays_in_log() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("markdown-converter".into());
        let generation = app.state.borrow().convert_readiness_generation;
        let detail = "文档转换组件未就绪：Xberg 运行目录缺失必需文件（13 项）：LICENSE；models/det.onnx；models/rec.onnx";
        app.pump
            .out
            .send(Event::Status(format!(
                "CONVERTER_READINESS|{generation}|0|{detail}"
            )))
            .unwrap();
        app.pump.run(ui);
        assert!(!ui.get_convert_ready());
        assert!(!ui.get_convert_status().contains(detail), "完整缺失清单只显示在折叠详情中");
        assert!(ui.get_notice_text().is_empty());
        assert!(
            ui.get_error_text().is_empty(),
            "组件就绪结果不应产生全局错误：{}",
            ui.get_error_text()
        );
        assert_eq!(ui.get_convert_detail_text().as_str(), detail);
        assert!(!ui.get_convert_detail_open());
        assert!(!ui.get_convert_log_text().contains(detail));
    })
    .unwrap();
}

// 转换启动预检与旧工具任务可并行；预检终态即使在 busy 时到达也必须清除
// pending 状态，不能让转换页面永久停在“正在检查”。
#[test]
fn converter_preflight_result_does_not_strand_when_other_task_runs() {
    with_gui(|app| {
        let ui = &app.ui;
        let generation = 7;
        {
            let mut state = app.state.borrow_mut();
            state.convert_preparing = true;
            state.convert_preflight_generation = generation;
            state.convert_pending_options = Some(markdown::Options {
                input_dir: PathBuf::new(),
                output_dir: PathBuf::new(),
                flat: false,
                groups: Vec::new(),
                timeout_secs: 1,
            });
            state.convert_cancel = Some(Arc::new(std::sync::atomic::AtomicBool::new(false)));
        }
        ui.set_convert_preparing(true);
        ui.set_busy(true);

        app.pump
            .out
            .send(Event::Status(format!(
                "CONVERTER_PREFLIGHT|{generation}|1|已通过"
            )))
            .unwrap();
        app.pump.run(ui);

        assert!(!ui.get_convert_preparing());
        assert!(!app.state.borrow().convert_preparing);
        assert!(app.state.borrow().convert_pending_options.is_none());
        assert!(app.state.borrow().convert_cancel.is_none());
        assert!(ui.get_busy(), "预检事件不得结束另一个工具的任务");
        assert!(
            ui.get_convert_status().contains("其他任务"),
            "转换页需明确提示其它任务运行中且可稍后重试"
        );
    })
    .unwrap();
}

// 覆盖 T-22/U-03：已跳过 99 个文件时第 100 个开始为 99%，结束即推进到 100%。
#[test]
fn converter_progress_counts_skips_and_finished_files() {
    with_gui(|app| {
        let ui = &app.ui;
        app.pump.apply_converter_status(ui, "CONVERTER_STARTED|100");
        assert_eq!(ui.get_convert_progress(), 0.0, "总数已知不显示未知进度");
        app.pump
            .apply_converter_status(ui, "CONVERTER_FILE_STARTED|100|100|last.pdf");
        assert!((ui.get_convert_progress() - 0.99).abs() < 0.001);
        app.pump
            .apply_converter_status(ui, "CONVERTER_FILE_FINISHED|0|1|last.pdf|");
        assert_eq!(ui.get_convert_progress(), 1.0);
        assert_eq!(ui.get_convert_progress_note().as_str(), "100 / 100");
        assert_eq!(app.state.borrow().convert_counts.success, 1);
        assert_eq!(app.state.borrow().convert_counts.skipped, 99);
    })
    .unwrap();
}

// 覆盖 T-22/T-24：扫描期失败与文件处理结果分别计入实际完成及运行统计。
#[test]
fn converter_progress_includes_scan_failures_and_partial_results() {
    with_gui(|app| {
        let ui = &app.ui;
        app.pump.apply_converter_status(ui, "CONVERTER_STARTED|3");
        app.pump
            .apply_converter_status(ui, "CONVERTER_FILE_FINISHED|0|0|failed.pdf|扫描失败");
        assert!((ui.get_convert_progress() - 1.0 / 3.0).abs() < 0.001);
        app.pump
            .apply_converter_status(ui, "CONVERTER_FILE_STARTED|3|3|partial.pdf");
        app.pump
            .apply_converter_status(ui, "CONVERTER_FILE_FINISHED|1|0|partial.pdf|部分内容");
        let state = app.state.borrow();
        assert_eq!(state.convert_counts.failed, 1);
        assert_eq!(state.convert_counts.partial, 1);
        assert_eq!(state.convert_counts.skipped, 1);
        assert_eq!(ui.get_convert_progress(), 1.0);
    })
    .unwrap();
}

// 覆盖 T-07：全部取消勾选后，旧场景的成功检查不得重新点亮开始。
#[test]
fn empty_conversion_selection_invalidates_pending_readiness() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_convert_runtime_dir("C:\\xberg".into());
        ui.set_convert_runtime_confirmed(true);
        let generation = app.state.borrow().convert_readiness_generation;
        ui.set_convert_pdf(false);
        ui.set_convert_office(false);
        ui.set_convert_images(false);
        ui.set_convert_media(false);
        ui.set_convert_other(false);
        ui.invoke_convert_selection_changed();
        app.pump
            .out
            .send(Event::Status(format!(
                "CONVERTER_READINESS|{generation}|1|已就绪"
            )))
            .unwrap();
        app.pump.run(ui);
        assert!(!ui.get_convert_ready());
        assert_eq!(ui.get_convert_status().as_str(), "至少选择一种转换类型");
    })
    .unwrap();
}

// 覆盖 T-29：编辑时显示字段错误，修正后清除；启动仍使用同一校验。
#[test]
fn convert_timeout_edits_validate_without_starting_task() {
    with_gui(|app| {
        let ui = &app.ui;
        for value in ["0", "abc", "18446744073709551615"] {
            ui.set_convert_timeout_secs(value.into());
            ui.invoke_convert_timeout_edited();
            assert!(!ui.get_convert_timeout_error().is_empty());
            assert!(!ui.get_busy() && !ui.get_convert_preparing());
        }
        ui.set_convert_timeout_secs("21600".into());
        ui.invoke_convert_timeout_edited();
        assert!(ui.get_convert_timeout_error().is_empty());
    })
    .unwrap();
}

// 覆盖 U-10/S-07（S1-03）：CONVERTER_LOG 只置脏、由轮询尾部按面板可见性统一
// 重建——页面不在转换页时不得逐条整段重排上屏；回到转换页后的第一次刷新
// 按环形缓冲补齐最新日志。
#[test]
fn converter_log_rebuild_is_throttled_to_visible_page() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("markdown-converter".into());
        app.pump
            .out
            .send(Event::Status("CONVERTER_LOG|screen5-line".into()))
            .unwrap();
        app.pump.run(ui);
        assert!(
            ui.get_convert_log_text().contains("screen5-line"),
            "转换页可见时日志必须在轮询尾部上屏"
        );
        ui.invoke_select_tool("directory-organizer".into());
        assert_eq!(ui.get_screen(), 0);
        app.pump
            .out
            .send(Event::Status("CONVERTER_LOG|away-line".into()))
            .unwrap();
        app.pump.run(ui);
        assert!(
            !ui.get_convert_log_text().contains("away-line"),
            "面板不可见时 CONVERTER_LOG 不得逐条整段重建上屏（S1-03）"
        );
        ui.invoke_select_tool("markdown-converter".into());
        app.pump.run(ui);
        assert!(
            ui.get_convert_log_text().contains("away-line"),
            "回到转换页后的刷新必须按环形缓冲补齐最新日志"
        );
    })
    .unwrap();
}

// 覆盖 T-05/T-06（S1-05）：busy 期间到达的当前代就绪结果被丢弃后，任务终态
// （DONE/FAIL 收尾）必须补查一次就绪，页面不得长期停留在过期的就绪状态。
#[test]
fn converter_done_rechecks_readiness_dropped_while_busy() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("markdown-converter".into());
        ui.set_convert_runtime_dir("C:\\xberg".into());
        ui.set_convert_runtime_confirmed(true);
        let generation = app.state.borrow().convert_readiness_generation;
        ui.set_busy(true);
        app.pump
            .out
            .send(Event::Status(format!(
                "CONVERTER_READINESS|{generation}|0|组件缺失"
            )))
            .unwrap();
        app.pump.run(ui);
        assert!(!ui.get_convert_ready(), "busy 期间就绪结果不得直接上屏");
        app.pump
            .out
            .send(Event::MdDone("CONVERTER_DONE|1|0|0|0|0|0".into()))
            .unwrap();
        app.pump.run(ui);
        assert!(
            app.state.borrow().convert_readiness_generation > generation,
            "任务终态必须对 busy 期间被丢弃的就绪结果发起补查（S1-05/T-05）"
        );
        assert_eq!(
            ui.get_convert_status().as_str(),
            "正在检查已安装组件…",
            "补查发起后状态应进入检查中"
        );
    })
    .unwrap();
}

// 覆盖 T-05/T-06/XB-19（S3-01）：入口移除后，纯媒体场景的就绪状态仍按勾选
// 场景计算——只勾 MP4/M4A 时走正常就绪检查，不要求文档组件初始化。
#[test]
fn media_only_selection_computes_readiness_without_page_initialize() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("markdown-converter".into());
        ui.set_convert_runtime_dir("C:\\xberg".into());
        ui.set_convert_runtime_confirmed(true);
        ui.set_convert_pdf(false);
        ui.set_convert_office(false);
        ui.set_convert_images(false);
        ui.set_convert_media(true);
        ui.set_convert_other(false);
        let generation = app.state.borrow().convert_readiness_generation;
        ui.invoke_convert_selection_changed();
        assert!(
            app.state.borrow().convert_readiness_generation > generation,
            "纯媒体勾选必须触发按场景就绪检查（XB-19）"
        );
        assert_eq!(
            ui.get_convert_status().as_str(),
            "正在检查已安装组件…",
            "就绪检查应正常发起，不得要求文档组件初始化"
        );
    })
    .unwrap();
}

// 覆盖 T-22（转 Markdown：任务状态须含耗时——运行期间实时显示，收尾统计含总耗时）。
// 事件驱动走真实事件泵：FILE_STARTED 后同一次 pump.run 的周期刷新即须带耗时，
// DONE 收尾统计行须含从任务起算的总耗时。
#[test]
fn convert_metrics_show_elapsed_while_running_and_total_on_done() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_busy(true);
        app.state.borrow_mut().runtime = RuntimeMode::MarkdownConverter;
        let stale = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .expect("系统运行时间不足 2 秒，无法构造旧时钟");
        app.state.borrow_mut().started = stale;
        app.pump
            .out
            .send(Event::Status("CONVERTER_STARTED|2".into()))
            .unwrap();
        app.pump
            .out
            .send(Event::Status(
                "CONVERTER_FILE_STARTED|1|2|docs/a.pdf".into(),
            ))
            .unwrap();
        app.pump.run(ui);
        let running = ui.get_convert_metrics().to_string();
        assert!(
            running.contains("正在处理 docs/a.pdf"),
            "运行行必须保留当前文件：{running}"
        );
        assert!(
            running.contains("耗时"),
            "转换运行期间实时指标必须显示耗时（T-22）：{running}"
        );
        assert!(
            elapsed_in_metrics(&running).is_some_and(|secs| secs >= 2.0),
            "耗时必须从任务起算（不得显示 0.0s 新时钟）：{running}"
        );
        app.pump
            .out
            .send(Event::MdDone("CONVERTER_DONE|1|0|0|0|0|0".into()))
            .unwrap();
        app.pump.run(ui);
        let final_metrics = ui.get_convert_metrics().to_string();
        assert!(
            final_metrics.contains("成功 1"),
            "收尾统计保留既有计数：{final_metrics}"
        );
        assert!(
            final_metrics.contains("总耗时"),
            "转换收尾统计必须含总耗时（T-22）：{final_metrics}"
        );
        assert!(
            elapsed_in_metrics(&final_metrics).is_some_and(|secs| secs >= 2.0),
            "总耗时必须从任务起算到收尾：{final_metrics}"
        );
    })
    .unwrap();
}

// 覆盖 T-22 对称性：CONVERTER_FAIL 收尾必须清掉运行中文案——修复前 FAIL
// 分支只重置 progress/note/status 不碰 metrics，失败后界面长期残留
//「正在处理 X」或「正在扫描…」（与 DONE 分支的收尾统计不对称）。
#[test]
fn convert_fail_clears_running_metrics_text() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_busy(true);
        app.state.borrow_mut().runtime = RuntimeMode::MarkdownConverter;
        app.pump
            .out
            .send(Event::Status("CONVERTER_STARTED|2".into()))
            .unwrap();
        app.pump
            .out
            .send(Event::Status(
                "CONVERTER_FILE_STARTED|1|2|docs/a.pdf".into(),
            ))
            .unwrap();
        app.pump.run(ui);
        let running = ui.get_convert_metrics().to_string();
        assert!(
            running.contains("正在处理"),
            "前置：运行中指标应含运行中文案，实测：{running}"
        );
        app.pump
            .out
            .send(Event::MdDone("CONVERTER_FAIL|组件不可用".into()))
            .unwrap();
        app.pump.run(ui);
        let final_metrics = ui.get_convert_metrics().to_string();
        assert!(
            !final_metrics.contains("正在处理") && !final_metrics.contains("正在扫描"),
            "失败收尾后不得残留运行中文案（正在处理/正在扫描），实测：{final_metrics}"
        );
        assert!(
            final_metrics.contains("总耗时"),
            "失败收尾统计应与 DONE 分支同口径含总耗时，实测：{final_metrics}"
        );
        // 扫描阶段失败：metrics 残留「正在扫描…」同样必须被收尾清除。
        ui.set_convert_metrics("正在扫描…".into());
        app.pump
            .out
            .send(Event::MdDone("CONVERTER_FAIL|扫描失败".into()))
            .unwrap();
        app.pump.run(ui);
        let scan_fail = ui.get_convert_metrics().to_string();
        assert!(
            !scan_fail.contains("正在扫描") && !scan_fail.contains("正在处理"),
            "扫描阶段失败的收尾同样不得残留运行中文案，实测：{scan_fail}"
        );
    })
    .unwrap();
}

// 覆盖 U-11 例外：转换任务运行中切页再切回，进度条不得被无条件重置为
// 不确定态——修复前 on_select_tool 无条件 set_convert_progress(-1)，长任务
// 期间离开再回来进度条到下一个 CONVERTER_FILE_STARTED 前保持不确定。
#[test]
fn select_tool_keeps_convert_progress_while_running() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_busy(true);
        app.state.borrow_mut().runtime = RuntimeMode::MarkdownConverter;
        app.pump
            .out
            .send(Event::Status(
                "CONVERTER_FILE_STARTED|2|4|docs/a.pdf".into(),
            ))
            .unwrap();
        app.pump.run(ui);
        let before = ui.get_convert_progress();
        let note_before = ui.get_convert_progress_note().to_string();
        assert!(
            (before - 0.25).abs() < 0.01,
            "前置：注入 2/4 进度应约为 0.25，实测：{before}"
        );
        // 切走再切回：运行态保留实时进度。
        ui.invoke_select_tool("md-organizer".into());
        ui.invoke_select_tool("markdown-converter".into());
        assert!(
            (ui.get_convert_progress() - before).abs() < f32::EPSILON,
            "转换运行中切页再切回必须保留实时进度：{before} → {}",
            ui.get_convert_progress()
        );
        assert_eq!(
            ui.get_convert_progress_note().as_str(),
            note_before,
            "运行中切页不得清空进度注释"
        );
        // 非运行态切页：保持原重置口径（上次任务残留进度不得带到新会话）。
        ui.set_busy(false);
        app.state.borrow_mut().runtime = RuntimeMode::Organizer;
        ui.invoke_select_tool("md-organizer".into());
        ui.invoke_select_tool("markdown-converter".into());
        assert!(
            (ui.get_convert_progress() + 1.0).abs() < f32::EPSILON,
            "非运行态切页仍须重置进度为不确定态，实测：{}",
            ui.get_convert_progress()
        );
    })
    .unwrap();
}

// 覆盖 U-01（进度口径）：CONVERTER_FILE_STARTED 的大批次进度必须先做比例
// 再落属性——修复前分子分母各自 u16 饱和后再相除，>65535 文件时
// 70000/100000 被显示为 1.0（饱和失真）。
#[test]
fn convert_file_started_progress_ratio_survives_large_totals() {
    with_gui(|app| {
        let ui = &app.ui;
        app.pump
            .out
            .send(Event::Status(
                "CONVERTER_FILE_STARTED|70001|100000|docs/big.pdf".into(),
            ))
            .unwrap();
        app.pump.run(ui);
        let progress = ui.get_convert_progress();
        assert!(
            (progress - 0.7).abs() < 0.01,
            "70000/100000 的进度应约为 0.7，实测：{progress}（u16 饱和会失真为 1.0）"
        );
        // 钳制口径不变：超出上限的比值仍不得超过 1.0。
        app.pump
            .out
            .send(Event::Status(
                "CONVERTER_FILE_STARTED|200001|100000|docs/big.pdf".into(),
            ))
            .unwrap();
        app.pump.run(ui);
        assert!(
            ui.get_convert_progress() <= 1.0,
            "进度比值必须保持 0.0-1.0 钳制，实测：{}",
            ui.get_convert_progress()
        );
    })
    .unwrap();
}

// 覆盖 T-22/U-03（真实回调启动转换必须重置耗时时钟：不得沿用上次任务的旧时钟）。
// 组件未就绪环境：启动后真实 worker 快速以 CONVERTER_FAIL 收尾，收尾前同步断言时钟。
#[test]
fn convert_start_resets_started_clock() {
    let dir = temp_test_dir("convert-clock");
    let input = dir.join("input");
    let output = dir.join("output");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::create_dir_all(&output).unwrap();
    let input_text = input.display().to_string();
    let output_text = output.display().to_string();
    with_gui(move |app| {
        let ui = &app.ui;
        ui.invoke_select_tool("markdown-converter".into());
        ui.set_convert_ready(true);
        ui.set_convert_input_dir(input_text.clone().into());
        ui.set_convert_output_dir(output_text.clone().into());
        let stale = Instant::now()
            .checked_sub(Duration::from_secs(600))
            .expect("系统运行时间不足 600 秒，无法构造旧时钟");
        app.state.borrow_mut().started = stale;
        ui.invoke_convert_start();
        let generation = app.state.borrow().convert_preflight_generation;
        app.pump
            .out
            .send(Event::Status(format!(
                "CONVERTER_PREFLIGHT|{generation}|1|已通过"
            )))
            .unwrap();
        app.pump.run(ui);
        assert!(ui.get_busy(), "组件就绪 + 目录有效时启动必须真正进入运行态");
        assert!(
            app.state.borrow().started > stale,
            "转换启动必须重置耗时时钟（T-22/U-03），不得沿用 600 秒前旧时钟"
        );
        // 收尾不等待真实组件探测（本机可能装有 Xberg，探测耗时不可控）：
        // 置取消标志让真实 worker 在首个检查点自行退出，同时用合成停止事件
        // 驱动与生产完全相同的 CONVERTER_DONE 收尾路径清 busy。
        if let Some(cancel) = app.state.borrow().convert_cancel.clone() {
            cancel.store(true, Ordering::Release);
        }
        app.pump
            .out
            .send(Event::MdDone("CONVERTER_DONE|0|0|0|0|0|1".into()))
            .unwrap();
        assert!(
            pump_until(app, || !ui.get_busy()),
            "停止事件必须走生产收尾路径清除 busy，测试不得悬挂"
        );
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// 覆盖 T-09：输入与输出目录相同必须在进入转换 busy 前拒绝；修复前
// start_markdown_conversion 先置 busy，再由后台 scan 返回失败。
#[test]
fn convert_same_input_output_rejected_before_busy() {
    let dir = temp_test_dir("convert-same-dir");
    std::fs::create_dir_all(&dir).unwrap();
    let text = dir.display().to_string();
    with_gui(move |app| {
        let ui = &app.ui;
        ui.invoke_select_tool("markdown-converter".into());
        ui.set_convert_ready(true);
        ui.set_convert_input_dir(text.clone().into());
        ui.set_convert_output_dir(text.into());
        ui.invoke_convert_start();
        assert!(pump_until(app, || ui
            .get_error_text()
            .contains("输入与输出目录不能相同")));
        assert!(!ui.get_busy(), "相同输入输出目录不得进入转换 busy 状态");
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// T-07/H-02：目录元数据检查必须留在后台预检，网络路径等慢 I/O 不得阻塞 GUI。
#[test]
fn convert_directory_validation_runs_in_background_preflight() {
    let root = temp_test_dir("convert-directory-preflight");
    let output = root.join("output");
    std::fs::create_dir_all(&output).unwrap();
    let input = root.join("missing-input");
    let input_text = input.display().to_string();
    let output_text = output.display().to_string();
    with_gui(move |app| {
        let ui = &app.ui;
        ui.invoke_select_tool("markdown-converter".into());
        ui.set_convert_ready(true);
        ui.set_convert_input_dir(input_text.into());
        ui.set_convert_output_dir(output_text.into());

        ui.invoke_convert_start();
        assert!(ui.get_convert_preparing(), "目录检查应先进入后台预检");
        assert!(!ui.get_busy(), "目录预检失败前不应开始转换");
        assert!(pump_until(app, || ui
            .get_error_text()
            .contains("无法访问输入目录")));
        assert!(!ui.get_busy(), "无效输入目录不得启动转换任务");
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

// 覆盖 T-29：超出 Instant 可表示范围的正整数必须如实报错，不钳制后
// 进入后台；修复前 u64::MAX 会在 Deadline::new 的 Instant 加法处 panic，
// 转换线程不回传终态而使界面永久 busy。
#[test]
fn convert_timeout_overflow_rejected_before_busy() {
    let dir = temp_test_dir("convert-timeout-overflow");
    std::fs::create_dir_all(&dir).unwrap();
    let text = dir.display().to_string();
    with_gui(move |app| {
        let ui = &app.ui;
        ui.invoke_select_tool("markdown-converter".into());
        ui.set_convert_ready(true);
        ui.set_convert_input_dir(text.clone().into());
        ui.set_convert_output_dir(text.into());
        ui.set_convert_timeout_secs(u64::MAX.to_string().into());
        ui.invoke_convert_start();
        assert!(!ui.get_busy(), "不可表示的超时不得进入转换 busy 状态");
        assert!(
            ui.get_error_text().contains("超出系统可表示范围"),
            "应如实报告超时范围错误：{}",
            ui.get_error_text()
        );
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// 覆盖 T-23（停止转 Markdown：运行/启动前检查/初始化三种在途阶段文案如实；
// 跨工具停止共享任务时按通用取消口径，不改写转换页状态行）
#[test]
fn convert_stop_reports_each_phase_and_cancels_shared_task() {
    with_gui(|app| {
        let ui = &app.ui;
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.state.borrow_mut().convert_cancel = Some(cancel.clone());
        ui.invoke_convert_stop();
        assert!(cancel.load(Ordering::Acquire), "停止必须写入转换取消原子量");
        assert_eq!(
            ui.get_convert_status().as_str(),
            "正在停止；正在取消当前文件"
        );
        assert!(ui.get_status().contains("正在停止转 Markdown"));

        let prep_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.state.borrow_mut().convert_cancel = Some(prep_cancel.clone());
        ui.set_convert_preparing(true);
        ui.invoke_convert_stop();
        assert!(prep_cancel.load(Ordering::Acquire));
        assert!(ui.get_convert_status().contains("正在取消启动前检查"));

        ui.set_convert_preparing(false);
        ui.set_convert_initializing(true);
        let init_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.state.borrow_mut().convert_cancel = None;
        app.state.borrow_mut().convert_init_cancel = Some(init_cancel.clone());
        ui.invoke_settings_action("cancel".into());
        assert!(init_cancel.load(Ordering::Acquire));
        assert!(ui.get_settings_status().contains("正在取消音频资产下载"));

        // 其他工具共享任务运行中：control 命中即取消，文案按通用口径且不改写转换状态行
        ui.set_convert_initializing(false);
        let control = Arc::new(Control::default());
        app.state.borrow_mut().control = Some(control.clone());
        ui.invoke_convert_stop();
        assert!(control.is_cancelled());
        assert_eq!(
            ui.get_status().as_str(),
            "正在取消；当前操作完成后停止，不会继续后续操作"
        );
        assert!(
            ui.get_convert_status().contains("正在取消启动前检查"),
            "停止共享任务不得改写转换页状态行"
        );
    })
    .unwrap();
}
