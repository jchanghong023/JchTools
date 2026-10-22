//! MD 整理与 Git 工具行为用例（自 gui.rs 的 gui_tests 拆分）。
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

// 覆盖 M-08（小数大小换算后为正整数字节即合法：0.5 MB = 524288 字节；
// 0 与换算后非整数字节的输入仍必须拒绝）
#[test]
fn split_size_accepts_integral_fractions_and_rejects_zero() {
    assert_eq!(
        parse_size_bytes("0.5", 1),
        Ok(512 * 1024),
        "0.5 MB = 524288 字节，合法输入不得拒绝"
    );
    assert_eq!(parse_size_bytes("0.5", 0), Ok(512), "0.5 KB = 512 字节");
    assert_eq!(parse_size_bytes("1.5", 0), Ok(1536), "1.5 KB = 1536 字节");
    assert!(parse_size_bytes("0", 0).is_err(), "0 字节必须拒绝");
    assert!(
        parse_size_bytes("0.1", 0).is_err(),
        "0.1 KB = 102.4 字节换算后非整数，必须拒绝"
    );
    assert!(
        parse_size_bytes("0.0", 1).is_err(),
        "0.0 MB 换算后为 0，必须拒绝"
    );
}

// 覆盖 M-08/M-09/M-10：精确换算为整数字节的小数不受任意小数位数限制。
#[test]
fn split_size_accepts_exact_byte_fractions() {
    assert_eq!(parse_size_bytes(".5", 0), Ok(512));
    assert_eq!(parse_size_bytes("0.0009765625", 0), Ok(1));
    assert_eq!(parse_size_bytes("0.00000095367431640625", 1), Ok(1));
    assert_eq!(
        parse_size_bytes("1.000000000000000000000", 1),
        Ok(1024 * 1024)
    );
    assert!(parse_size_bytes("0.00000095367431640624", 1).is_err());
    assert!(parse_size_bytes("0.000", 0).is_err());
}

// 覆盖 M-08/M-09/M-10/U-12：真实拆分回调接受 1 字节的精确 KB 输入，产物无损。
#[test]
fn md_split_exact_byte_limit_runs_via_callback() {
    let dir = temp_test_dir("md-split-exact-byte-limit");
    let input = dir.join("doc.md");
    let parts = dir.join("parts");
    std::fs::write(&input, b"abc").unwrap();
    std::fs::create_dir_all(&parts).unwrap();
    let cleanup = dir.clone();
    with_gui(move |app| {
        app.ui.invoke_select_tool("md-organizer".into());
        app.ui.set_md_split_file(input.display().to_string().into());
        app.ui.set_md_split_dir(parts.display().to_string().into());
        app.ui.set_md_split_size("0.0009765625".into());
        app.ui.set_md_split_unit(0);
        app.ui.invoke_md_split_start();
        assert!(app.ui.get_busy(), "合法 1 字节限制必须启动后台拆分");
        assert!(pump_until(app, || !app.ui.get_busy()));
        assert!(app.ui.get_status().contains("拆分完成"));
        for (name, bytes) in [
            ("doc_001.md", b"a"),
            ("doc_002.md", b"b"),
            ("doc_003.md", b"c"),
        ] {
            assert_eq!(std::fs::read(parts.join(name)).unwrap(), bytes);
        }
        assert_eq!(std::fs::read(&input).unwrap(), b"abc", "原文件不得改动");
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(cleanup);
}

// 覆盖 M-11（同名分片确认覆盖后必须实际写出：确认不得再次弹框或空转）
#[test]
fn md_split_existing_output_requires_confirmation_then_overwrites() {
    let dir = temp_test_dir("md-split-confirm");
    let docs = dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    let content = "0123456789".repeat(150); // 1500 字节 → 1 KB 限制下 2 片
    std::fs::write(docs.join("doc.md"), &content).unwrap();
    let parts = dir.join("parts");
    std::fs::create_dir_all(&parts).unwrap();
    std::fs::write(parts.join("doc_001.md"), "旧内容").unwrap();
    let parts_text = parts.display().to_string();
    with_gui(move |app| {
        app.ui.invoke_select_tool("md-organizer".into());
        app.ui.set_md_subpage(1);
        app.ui
            .set_md_split_file(docs.join("doc.md").display().to_string().into());
        app.ui.set_md_split_size("1".into());
        app.ui.set_md_split_unit(0);
        app.ui.set_md_split_dir(parts_text.clone().into());
        app.ui.invoke_md_split_start();
        assert!(
            pump_until(app, || app.ui.get_confirm_kind() == 4),
            "同名分片已存在必须先弹确认框（M-11）"
        );
        assert_eq!(
            std::fs::read_to_string(parts.join("doc_001.md")).unwrap(),
            "旧内容",
            "确认前不得写入"
        );
        app.ui.set_acknowledge(true);
        app.ui.set_confirm_kind(0);
        confirm_md_override(&app.ui, &app.state, &app.pump.out);
        assert!(
            pump_until(app, || {
                !app.ui.get_busy() && app.ui.get_status().contains("拆分完成")
            }),
            "确认覆盖后必须完成写出而不是再次弹确认（M-11）：status={}",
            app.ui.get_status()
        );
        assert_eq!(
            std::fs::read_to_string(parts.join("doc_001.md")).unwrap(),
            &content[..1024],
            "确认后旧分片被覆盖为新内容前 1024 字节"
        );
        assert_eq!(
            std::fs::read_to_string(parts.join("doc_002.md")).unwrap(),
            &content[1024..],
            "第二片写出剩余内容"
        );
        let _ = std::fs::remove_dir_all(&dir);
    })
    .unwrap();
}

// 覆盖 H-02/U-06（回归：MD/Git/转换任务收尾必须复位 paused——否则残留的
// paused 会丢弃后续任务的普通状态事件，并把暂停按钮显示成「继续」）
#[test]
fn md_and_git_finish_reset_paused() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_busy(true);
        ui.set_paused(true);
        app.pump
            .out
            .send(Event::MdDone("合并完成：0 个文件".into()))
            .unwrap();
        app.pump.run(ui);
        assert!(!ui.get_busy(), "收尾清 busy");
        assert!(!ui.get_paused(), "MD 收尾必须复位 paused（修复前残留）");
        ui.set_busy(true);
        ui.set_paused(true);
        app.pump
            .out
            .send(Event::GitDone("Git 任务完成".into()))
            .unwrap();
        app.pump.run(ui);
        assert!(!ui.get_busy());
        assert!(!ui.get_paused(), "Git 收尾同样复位 paused");
    })
    .unwrap();
}

// 覆盖 P-02/U-11（新工具导航注册与切工具回默认子页）
#[test]
fn md_and_git_tools_navigate_and_reset_subpage() {
    with_gui(|app| {
        app.ui.invoke_select_tool("md-organizer".into());
        assert_eq!(app.ui.get_screen(), 3, "MD 整理应进入独立页面（M-01）");
        assert_eq!(app.ui.get_active_tool_id(), "md-organizer");
        app.ui.set_md_subpage(1);
        app.ui.invoke_select_tool("git-tools".into());
        assert_eq!(app.ui.get_screen(), 4, "Git 工具应进入独立页面（G-01）");
        app.ui.invoke_select_tool("markdown-converter".into());
        assert_eq!(app.ui.get_screen(), 5, "转 Markdown 应进入独立页面（T-01）");
        assert_eq!(app.ui.get_active_tool_id(), "markdown-converter");
        // U-11：切回 MD 整理必须回到默认子页「合并 MD」，不得停留在拆分子页
        app.ui.invoke_select_tool("md-organizer".into());
        assert_eq!(app.ui.get_md_subpage(), 0, "切工具回默认子页（U-11）");
        // 现有工具不受影响（H-03：不删除、不隐藏、不改名）
        app.ui.invoke_select_tool("recursive-extract".into());
        assert_eq!(app.ui.get_screen(), 2);
        app.ui.invoke_select_tool("directory-organizer".into());
        assert_eq!(app.ui.get_screen(), 0);
    })
    .unwrap();
}

// 覆盖 M-02/M-04/M-07（MD 合并的真实回调端到端：扫描→合并→完成文案与输出内容）
#[test]
fn md_merge_flow_runs_end_to_end_via_real_callbacks() {
    let dir = temp_test_dir("md-merge-flow");
    let docs = dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(
        docs.join("a.md"),
        "# 甲

内容A",
    )
    .unwrap();
    std::fs::write(
        docs.join("b.md"),
        "# 乙
内容B
",
    )
    .unwrap();
    let output = docs.join("merged.md");
    let docs_text = docs.display().to_string();
    with_gui(move |app| {
        app.ui.invoke_select_tool("md-organizer".into());
        app.ui.set_md_input_dir(docs_text.clone().into());
        app.ui.set_md_recursive(true);
        app.ui.set_md_output_name("merged.md".into());
        app.ui.set_md_output_dir(docs_text.clone().into());
        app.ui.invoke_md_merge_start();
        assert!(
            pump_until(app, || !app.ui.get_busy()
                && app.ui.get_status().contains("合并完成")),
            "合并应经真实回调完成：status={}",
            app.ui.get_status()
        );
        let merged = std::fs::read_to_string(&output).unwrap();
        assert!(
            merged.contains(
                "# a.md

## 甲"
            ),
            "文件名标题+内部标题下移：{merged}"
        );
        assert!(
            merged.contains(
                "## 乙
内容B"
            ),
            "{merged}"
        );
        assert!(
            merged.contains(
                "内容A

# b.md"
            ),
            "文件间必须空行分隔：{merged:?}"
        );
        assert!(
            std::fs::read_to_string(docs.join("a.md"))
                .unwrap()
                .starts_with("# 甲"),
            "原文件不得被改动（M-02）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    })
    .unwrap();
}

// 覆盖 M-07（输出已存在：先确认、不静默覆盖；确认后完成覆盖）
#[test]
fn md_merge_existing_output_requires_confirmation() {
    let dir = temp_test_dir("md-merge-confirm");
    let docs = dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(
        docs.join("a.md"),
        "# 甲
",
    )
    .unwrap();
    std::fs::write(docs.join("merged.md"), "旧输出").unwrap();
    let docs_text = docs.display().to_string();
    with_gui(move |app| {
        app.ui.invoke_select_tool("md-organizer".into());
        app.ui.set_md_input_dir(docs_text.clone().into());
        app.ui.set_md_output_name("merged.md".into());
        app.ui.set_md_output_dir(docs_text.clone().into());
        app.ui.invoke_md_merge_start();
        // 冲突 → 确认框（kind=4），旧输出未被破坏
        assert!(
            pump_until(app, || app.ui.get_confirm_kind() == 4),
            "输出已存在必须弹确认框（M-07）"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("docs").join("merged.md")).unwrap(),
            "旧输出",
            "确认前不得破坏已有文件"
        );
        app.ui.set_acknowledge(true);
        app.ui.set_confirm_kind(0);
        confirm_md_override(&app.ui, &app.state, &app.pump.out);
        assert!(
            pump_until(app, || !app.ui.get_busy()
                && app.ui.get_status().contains("合并完成")),
            "确认覆盖后应完成：status={}",
            app.ui.get_status()
        );
        let merged = std::fs::read_to_string(dir.join("docs").join("merged.md")).unwrap();
        assert!(merged.contains("# a.md"), "确认后输出被覆盖写入：{merged}");
        let _ = std::fs::remove_dir_all(&dir);
    })
    .unwrap();
}

// 覆盖 U-10/U-12、M-02/M-04：展开日志期间真实合并继续完成，事件泵仍交付日志。
#[test]
fn expanded_log_panel_keeps_real_md_task_and_terminal_logs() {
    let (docs, out_dir, dir) = make_md_fixture("md-expanded-log", 1);
    let source = std::fs::read_to_string(docs.join("doc000.md")).unwrap();
    let expected = format!("# doc000.md\n\n{}", source.replace("# 标题", "## 标题"));
    let merged = out_dir.join("merged.md");
    let input_text = docs.display().to_string();
    let output_text = out_dir.display().to_string();
    with_gui(move |app| {
        let ui = &app.ui;
        ui.invoke_select_tool("md-organizer".into());
        ui.set_md_input_dir(input_text.into());
        ui.set_md_output_dir(output_text.into());
        ui.set_md_output_name("merged.md".into());
        // 点击与 Esc 的真实窗口内事件由 gui_log_panel 单独覆盖；此处验证任务边界。
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        ui.invoke_md_merge_start();
        assert!(ui.get_busy());
        ui.set_log_expanded(true);
        assert!(
            pump_until(app, || !ui.get_busy()
                && ui.get_status().contains("合并完成")),
            "展开期间任务应完成：{}",
            ui.get_status()
        );
        assert!(ui.get_log_expanded(), "任务收尾不能擅自关闭展开日志");
        assert!(ui.get_log_text().contains("合并完成：1 个文件"));
        ui.set_log_expanded(false);
        assert!(ui.get_log_text().contains("合并完成：1 个文件"));
    })
    .unwrap();
    assert_eq!(std::fs::read_to_string(&merged).unwrap(), expected);
    assert_eq!(
        std::fs::read_to_string(docs.join("doc000.md")).unwrap(),
        source
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// 覆盖 U-03：真实完成事件使用本次任务时钟，合并/拆分都不能继承旧 started。
#[test]
fn md_start_resets_started_clock() {
    let (docs, out_dir, dir) = make_md_fixture("md-clock-merge", 1);
    let merged_output = out_dir.join("merged.md");
    let docs_text = docs.display().to_string();
    let out_text = out_dir.display().to_string();
    with_gui(move |app| {
        app.ui.invoke_select_tool("md-organizer".into());
        let stale = Instant::now()
            .checked_sub(Duration::from_secs(600))
            .expect("系统运行时间不足 600 秒，无法构造旧时钟");
        app.state.borrow_mut().started = stale;
        app.ui.set_md_input_dir(docs_text.into());
        app.ui.set_md_output_name("merged.md".into());
        app.ui.set_md_output_dir(out_text.into());
        app.ui.invoke_md_merge_start();
        assert!(
            app.state.borrow().started > stale,
            "合并启动必须同步重置任务时钟"
        );
        assert!(
            pump_until(app, || app.ui.get_status().contains("合并完成")),
            "真实合并完成结果应上屏：{}",
            app.ui.get_status()
        );
    })
    .unwrap();
    assert!(merged_output.is_file(), "合并完成必须生成真实输出");
    let _ = std::fs::remove_dir_all(&dir);

    let (docs, out_dir, dir) = make_md_fixture("md-clock-split", 1);
    let input = docs.join("doc000.md");
    let input_text = input.display().to_string();
    let out_text = out_dir.display().to_string();
    with_gui(move |app| {
        app.ui.invoke_select_tool("md-organizer".into());
        app.ui.set_md_subpage(1);
        let stale = Instant::now()
            .checked_sub(Duration::from_secs(600))
            .expect("系统运行时间不足 600 秒，无法构造旧时钟");
        app.state.borrow_mut().started = stale;
        app.ui.set_md_split_file(input_text.into());
        app.ui.set_md_split_size("1".into());
        app.ui.set_md_split_unit(1);
        app.ui.set_md_split_dir(out_text.into());
        app.ui.invoke_md_split_start();
        assert!(
            app.state.borrow().started > stale,
            "拆分启动必须同步重置任务时钟"
        );
        assert!(
            pump_until(app, || app.ui.get_status().contains("拆分完成")),
            "真实拆分完成结果应上屏：{}",
            app.ui.get_status()
        );
    })
    .unwrap();
    assert!(
        std::fs::read_dir(&out_dir)
            .unwrap()
            .any(|entry| entry.unwrap().path().is_file()),
        "拆分完成必须生成真实分片"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// 覆盖 U-09（MD/Git 任务启动时必须重置 close_after：一次「停止并关闭」只作用于
// 当次任务收尾，不得残留到之后的任务让收尾时意外退出应用）
#[test]
fn md_and_git_start_reset_close_after() {
    let (docs, out_dir, dir) = make_md_fixture("md-close-after-merge", 2);
    let docs_text = docs.display().to_string();
    let out_text = out_dir.display().to_string();
    with_gui(move |app| {
        app.ui.invoke_select_tool("md-organizer".into());
        app.state.borrow_mut().close_after = true;
        app.ui.set_md_input_dir(docs_text.into());
        app.ui.set_md_output_name("merged.md".into());
        app.ui.set_md_output_dir(out_text.into());
        app.ui.invoke_md_merge_start();
        assert!(
            !app.state.borrow().close_after,
            "MD 合并启动必须重置 close_after（U-09）：残留会让之后的任务收尾时意外关闭应用"
        );
        assert!(pump_until(app, || !app.ui.get_busy()), "合并应正常收尾");
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    // 拆分
    let (docs, out_dir, dir) = make_md_fixture("md-close-after-split", 2);
    let input_text = docs.join("doc000.md").display().to_string();
    let out_text = out_dir.display().to_string();
    with_gui(move |app| {
        app.ui.invoke_select_tool("md-organizer".into());
        app.ui.set_md_subpage(1);
        app.state.borrow_mut().close_after = true;
        app.ui.set_md_split_file(input_text.into());
        app.ui.set_md_split_size("1".into());
        app.ui.set_md_split_unit(0);
        app.ui.set_md_split_dir(out_text.into());
        app.ui.invoke_md_split_start();
        assert!(
            !app.state.borrow().close_after,
            "MD 拆分启动必须重置 close_after（U-09）"
        );
        assert!(pump_until(app, || !app.ui.get_busy()), "拆分应正常收尾");
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    // Git：仓库无效也会快速收尾，启动路径的重置必须同样生效
    let git_dir = temp_test_dir("git-close-after");
    let git_text = git_dir.display().to_string();
    with_gui(move |app| {
        app.ui.invoke_select_tool("git-tools".into());
        app.state.borrow_mut().close_after = true;
        app.ui.set_git_repo(git_text.into());
        app.ui.set_git_branch("旧分支".into());
        app.ui.set_git_upstream("旧远端".into());
        app.ui.set_git_current("旧文件.md".into());
        app.ui.set_git_stage("push".into());
        app.ui.set_git_retry(3);
        app.ui.set_git_retry_wait(80);
        app.ui.set_git_total(12);
        app.ui.set_git_done(9);
        app.ui.set_git_state("完成".into());
        app.ui.invoke_git_start();
        assert_eq!(app.ui.get_git_branch().as_str(), "", "新任务不得保留旧分支");
        assert_eq!(
            app.ui.get_git_upstream().as_str(),
            "",
            "新任务不得保留旧 upstream"
        );
        assert_eq!(
            app.ui.get_git_current().as_str(),
            "",
            "新任务不得保留旧文件"
        );
        assert_eq!(app.ui.get_git_retry(), 0, "新任务重试次数从零开始");
        assert_eq!(app.ui.get_git_retry_wait(), 0, "新任务不得保留旧退避计时");
        assert_eq!(app.ui.get_git_total(), 0, "新任务待处理总数从零开始");
        assert_eq!(app.ui.get_git_done(), 0, "新任务完成数从零开始");
        assert_eq!(app.ui.get_git_state().as_str(), "检查仓库");
        assert!(
            !app.state.borrow().close_after,
            "Git 启动必须重置 close_after（U-09）"
        );
        assert!(pump_until(app, || !app.ui.get_busy()), "Git 任务应快速收尾");
        assert_eq!(
            app.ui.get_git_state().as_str(),
            "失败",
            "无效仓库收尾不得残留检查仓库状态"
        );
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&git_dir);
}

// 覆盖 G-03~G-08、G-13（GUI 入口端到端）：从 git-tools 页面启动任务，经引擎对
// 真实本地仓库逐文件提交并推送到裸远端，收尾把共享进度定格上屏。引擎行为已由
// tests/git_tools.rs 覆盖；本用例补 GUI 启动接缝（目录属性读取、事件回传、
// 完成态与 busy 复位）与真实推送结果的组合验证。
#[test]
fn git_start_pushes_real_repository_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let git = jchtools_git_tools::git_tools::find_git().expect("PATH 中需有可用 git");
    let base = dir.path();
    let run_git = |cwd: &std::path::Path, args: &[&str]| {
        let ok = std::process::Command::new(&git)
            // 固定夹具默认分支为 master（与 tests/git_tools.rs::git_ok 一致，
            // 只作用于本次调用）：全局 init.defaultBranch 非 master 时，
            // 后续 push master 与裸远端 HEAD 指向都会失败。
            .args(["-c", "init.defaultBranch=master"])
            .args(args)
            .current_dir(cwd)
            .status()
            .is_ok_and(|status| status.success());
        assert!(ok, "夹具 git 调用失败：git {args:?} @ {}", cwd.display());
    };
    // seed（含初始提交）→ 裸远端 → clone 出带 upstream 的工作仓库（与
    // tests/git_tools.rs::fixture 同构）；仓库级提交身份对无全局配置的环境生效。
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    run_git(&seed, &["init", "-q"]);
    // 种子仓库同样需要仓库级提交身份：CI runner 与无全局 user.name/user.email
    // 的环境下首个提交会因缺身份失败（回归：CI run 37135458960）。
    run_git(&seed, &["config", "user.name", "JchTools Test"]);
    run_git(&seed, &["config", "user.email", "test@jchtools.local"]);
    std::fs::write(seed.join("README.md"), "init\n").unwrap();
    run_git(&seed, &["add", "README.md"]);
    run_git(&seed, &["commit", "-q", "-m", "init"]);
    let remote = base.join("remote.git");
    run_git(
        base,
        &["init", "-q", "--bare", &remote.display().to_string()],
    );
    run_git(
        &seed,
        &["push", "-q", &remote.display().to_string(), "master"],
    );
    let repo = base.join("repo");
    run_git(
        base,
        &["clone", "-q", &remote.display().to_string(), "repo"],
    );
    run_git(&repo, &["config", "user.name", "JchTools Test"]);
    run_git(&repo, &["config", "user.email", "test@jchtools.local"]);
    // 未提交的新文件是任务的全部输入。
    std::fs::write(repo.join("feature.md"), "from gui\n").unwrap();

    let repo_text = repo.display().to_string();
    with_gui(move |app| {
        app.ui.invoke_select_tool("git-tools".into());
        app.ui.set_git_repo(repo_text.into());
        app.ui.invoke_git_start();
        // G-13：让真实 worker 先完成，再交付同一批真实事件，确定性覆盖
        // GitDone 先于 refresh_runtime 的时序（不创建或激活 OS 窗口）。
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut pending = Vec::new();
        loop {
            let event = app
                .pump
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("真实 Git 任务必须回传终态");
            let terminal = matches!(&event, Event::GitDone(_));
            pending.push(event);
            if terminal {
                break;
            }
        }
        assert!(pending.len() <= 256, "单文件夹具应在一次事件泵内收尾");
        for event in pending {
            app.pump.out.send(event).unwrap();
        }
        app.pump.run(&app.ui);
        assert!(!app.ui.get_busy(), "Git 任务应正常收尾");
        assert_eq!(app.ui.get_git_branch().as_str(), "master");
        assert_eq!(app.ui.get_git_upstream().as_str(), "origin/master");
        assert_eq!(app.ui.get_git_current().as_str(), "feature.md");
        assert_eq!(app.ui.get_git_retry(), 0);
        assert_eq!(app.ui.get_git_retry_wait(), 0);
        assert_eq!(
            app.ui.get_git_state().as_str(),
            "完成",
            "收尾必须把共享状态定格上屏（G-13）：git_state={}",
            app.ui.get_git_state()
        );
        assert_eq!(app.ui.get_git_done(), 1, "单个文件应计入完成数");
        assert!(app.ui.get_git_total() >= 1, "总量应至少包含该文件");
    })
    .unwrap();

    // 推送结果以远端为准（G-08）：裸远端 HEAD 与工作仓库一致，且工作区已干净。
    let head = |cwd: &std::path::Path| {
        let out = std::process::Command::new(&git)
            .args(["rev-parse", "HEAD"])
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success(), "rev-parse 失败");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    assert_eq!(
        head(&repo),
        head(&remote),
        "GUI 启动的任务必须把提交推送到远端"
    );
    let status = std::process::Command::new(&git)
        .args(["status", "--porcelain"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(
        status.status.success() && status.stdout.is_empty(),
        "提交后工作区应干净：{}",
        String::from_utf8_lossy(&status.stdout)
    );
}

// 覆盖 G-15（Git 停止：真正取消共享控制器、不再处理新文件；跨工具停止共享
// 任务与转 Markdown 回落分支的文案如实区分）
#[test]
fn git_stop_cancels_control_and_reports_per_runtime() {
    with_gui(|app| {
        let ui = &app.ui;
        let control = Arc::new(Control::default());
        app.state.borrow_mut().control = Some(control.clone());
        app.state.borrow_mut().runtime = RuntimeMode::Git;
        ui.invoke_git_stop();
        assert!(control.is_cancelled(), "停止必须真正取消 Git 任务（G-15）");
        assert!(ui.get_status().contains("已成功推送的文件保持成功"));

        // 其他工具的共享任务（整理/解压/MD）：通用取消口径
        app.state.borrow_mut().runtime = RuntimeMode::Organizer;
        let shared = Arc::new(Control::default());
        app.state.borrow_mut().control = Some(shared.clone());
        ui.invoke_git_stop();
        assert!(shared.is_cancelled());
        assert!(ui.get_status().contains("正在取消；当前操作完成后停止"));

        // control 为 None：回落取消转 Markdown 任务（转换页「停止任务」同源）
        let convert_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.state.borrow_mut().control = None;
        app.state.borrow_mut().convert_cancel = Some(convert_cancel.clone());
        ui.invoke_git_stop();
        assert!(convert_cancel.load(Ordering::Acquire));
        assert!(ui.get_status().contains("正在停止转 Markdown"));
    })
    .unwrap();
}

// 覆盖 U-11 / M-01（MD 子页页签经真实回调切换；无效页号安全忽略）
#[test]
fn md_subpage_tabs_switch_via_real_callback() {
    with_gui(|app| {
        app.ui.invoke_md_select_subpage(1);
        assert_eq!(app.ui.get_md_subpage(), 1, "页签应切到拆分子页");
        app.ui.invoke_md_select_subpage(0);
        assert_eq!(app.ui.get_md_subpage(), 0, "页签应切回合并子页");
        app.ui.invoke_md_select_subpage(2);
        assert_eq!(app.ui.get_md_subpage(), 0, "无效页号必须安全忽略");
    })
    .unwrap();
}

// 覆盖 U-12 / H-02 / U-06：停止扫描或拆分规划必须先于冲突确认生效；
// 合并与拆分使用启动时同一个 Control，取消收尾不能显示红色错误。
#[test]
fn cancelled_md_preparation_does_not_request_overwrite_or_report_error() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("doc.md");
    let output = directory.path().join("merged.md");
    let parts = directory.path().join("parts");
    std::fs::write(&input, b"synthetic source").unwrap();
    std::fs::write(&output, b"existing merge").unwrap();
    std::fs::create_dir(&parts).unwrap();
    std::fs::write(parts.join("doc_001.md"), b"existing split").unwrap();
    let control = Arc::new(Control::default());
    control.cancel();
    let (sender, _receiver) = control::event_channel();
    let out = EventSender::new(sender);
    let merge_error =
        md_merge_run(directory.path(), false, &output, false, &control, &out).unwrap_err();
    let split_error = md_split_run(&input, 1024, &parts, false, &control, &out).unwrap_err();
    for error in [&merge_error, &split_error] {
        assert!(!error.starts_with("MD_CONFLICT "), "{error}");
        assert!(error.contains("取消"), "{error}");
    }
    assert_eq!(std::fs::read(&input).unwrap(), b"synthetic source");
    assert_eq!(std::fs::read(&output).unwrap(), b"existing merge");
    assert_eq!(
        std::fs::read(parts.join("doc_001.md")).unwrap(),
        b"existing split"
    );
    with_gui(move |app| {
        app.state.borrow_mut().control = Some(control);
        app.state.borrow_mut().runtime = RuntimeMode::Md;
        app.ui.set_busy(true);
        app.pump.out.send(Event::Failed(split_error)).unwrap();
        app.pump.run(&app.ui);
        assert!(!app.ui.get_busy());
        assert_eq!(app.ui.get_confirm_kind(), 0);
        assert!(app.ui.get_error_text().is_empty());
        assert!(app.ui.get_notice_text().contains("取消"));
        assert!(app.ui.get_status().contains("已取消"));
    })
    .unwrap();
}

// 覆盖 U-03：短任务在首次周期刷新前完成，也必须显示最终计数。
#[test]
fn gui_findings_md_merge_final_metrics() {
    let dir = temp_test_dir("findings-merge-metrics");
    std::fs::write(dir.join("a.md"), "# A").unwrap();
    std::fs::write(dir.join("b.md"), "# B").unwrap();
    let cleanup = dir.clone();
    with_gui(move |app| {
        app.ui.invoke_select_tool("md-organizer".into());
        app.ui.set_md_input_dir(dir.display().to_string().into());
        app.ui.set_md_output_dir(dir.display().to_string().into());
        app.ui.invoke_md_merge_start();
        assert!(pump_until(app, || !app.ui.get_busy()));
        assert!(app.ui.get_status().contains("合并完成"));
        assert!(
            app.ui.get_metrics().contains("已处理 2 / 2"),
            "{}",
            app.ui.get_metrics()
        );
    })
    .unwrap();
    std::fs::remove_dir_all(cleanup).unwrap();
}

// 覆盖 M-10 / U-03：收尾分片计数不得停在零或倒数第二片。
#[test]
fn gui_findings_md_split_final_metrics() {
    let dir = temp_test_dir("findings-split-metrics");
    let input = dir.join("input.md");
    std::fs::write(&input, "a".repeat(2500)).unwrap();
    let cleanup = dir.clone();
    with_gui(move |app| {
        app.ui.invoke_select_tool("md-organizer".into());
        app.ui.set_md_split_file(input.display().to_string().into());
        app.ui.set_md_split_dir(dir.display().to_string().into());
        app.ui.invoke_md_split_start();
        assert!(pump_until(app, || !app.ui.get_busy()));
        assert!(app.ui.get_status().contains("拆分完成"));
        assert!(
            app.ui.get_metrics().contains("已处理 3 / 3"),
            "{}",
            app.ui.get_metrics()
        );
    })
    .unwrap();
    std::fs::remove_dir_all(cleanup).unwrap();
}

// 覆盖 U-11：已结束任务的共享输出不得带入未运行的其他工具。
#[test]
fn gui_findings_idle_tool_switch_does_not_leak_output() {
    with_gui(|app| {
        app.ui.invoke_select_tool("md-organizer".into());
        push_event_log(&mut app.state.borrow_mut().logs, "MD_PRIVATE_RESULT".into());
        app.ui.set_log_text("MD_PRIVATE_RESULT".into());
        app.ui.set_metrics("已处理 0 / 5".into());
        app.ui.invoke_select_tool("directory-organizer".into());
        app.pump.run(&app.ui);
        assert!(!app.ui.get_metrics().contains("0 / 5"));
        assert!(!app.ui.get_log_text().contains("MD_PRIVATE_RESULT"));
        app.ui.invoke_select_tool("git-tools".into());
        app.pump.run(&app.ui);
        assert!(!app.ui.get_log_text().contains("MD_PRIVATE_RESULT"));
    })
    .unwrap();
}

// 覆盖 M-08：编辑字段与任务启动使用相同的精确换算回调。
#[test]
fn gui_findings_md_size_validation_callback() {
    with_gui(|app| {
        for value in ["0", "-1", "abc", "0.1"] {
            assert!(
                !app.ui
                    .invoke_md_split_size_error(value.into(), 0)
                    .is_empty(),
                "{value}"
            );
        }
        for (value, unit) in [("1", 0), ("0.5", 1), ("0.0009765625", 0)] {
            assert!(
                app.ui
                    .invoke_md_split_size_error(value.into(), unit)
                    .is_empty(),
                "{value}"
            );
        }
        assert!(!app.ui.get_busy());
    })
    .unwrap();
}

// 覆盖 U-09：MD 自然完成/输出冲突与停止关闭可能交错。
// 确定性终态注入只补足分支覆盖；真实窗口退出仍由 tests/gui_flow.rs 断言。
#[test]
fn md_terminal_transitions_preserve_stop_and_close_confirmation() {
    for conflict in [false, true] {
        with_gui(move |app| {
            app.ui.invoke_select_tool("md-organizer".into());
            app.ui.set_busy(true);
            app.ui.set_confirm_kind(3);
            let control = Arc::new(Control::default());
            app.state.borrow_mut().control = Some(control.clone());
            app.state.borrow_mut().runtime = RuntimeMode::Md;
            app.ui.invoke_confirmed(3);
            assert!(control.is_cancelled());
            // 与 Slint 确认按钮相同：回调接受停止关闭后移除当前确认层。
            app.ui.set_confirm_kind(0);
            let terminal = if conflict {
                Event::MdNeedsConfirm("输出文件已存在".into())
            } else {
                Event::MdDone("没有 Markdown 输入，未生成输出".into())
            };
            app.pump.out.send(terminal).unwrap();
            app.pump.run(&app.ui);
            assert!(!app.ui.get_busy(), "任务必须已结束");
            assert_eq!(app.ui.get_confirm_kind(), 0, "不得再向用户要求覆盖确认");
            assert!(app.ui.get_error_text().is_empty());
            assert!(
                !app.state.borrow().close_after,
                "终态必须消费已接受的关闭请求"
            );
        })
        .unwrap();
    }
    with_gui(|app| {
        app.ui.invoke_select_tool("md-organizer".into());
        app.ui.set_busy(true);
        app.ui.set_confirm_kind(3);
        app.pump
            .out
            .send(Event::MdNeedsConfirm("输出文件已存在".into()))
            .unwrap();
        app.pump.run(&app.ui);
        assert!(!app.ui.get_busy());
        assert_eq!(
            app.ui.get_confirm_kind(),
            3,
            "用户尚未确认关闭时不得被覆盖弹窗顶替"
        );
        assert!(app.ui.get_error_text().is_empty());
    })
    .unwrap();
}
