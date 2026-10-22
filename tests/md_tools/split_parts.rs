use super::*;
// 覆盖 M-08/M-09（KB/MB 换算与硬限制：每片 ≤ 限制）
#[test]
fn split_respects_kb_and_mb_limits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // 约 3.5KB 的中英混排内容
    let mut text = String::new();
    while text.len() < 3500 {
        text.push_str("中English混合内容2026！\n");
    }
    write(&root.join("doc.md"), &text);
    let out_dir = root.join("parts");
    // KB：1 KB = 1024 字节
    let plan = md_tools::plan_splits(&root.join("doc.md"), 1024, &Control::default()).unwrap();
    assert!(plan.bounds.len() >= 3, "3500 字节按 1KB 至少 3 片");
    md_tools::run_split(
        &root.join("doc.md"),
        &plan,
        &out_dir,
        false,
        &Control::default(),
        &|_, _| Ok(()),
    )
    .unwrap();
    let mut prev = 0u64;
    for bound in &plan.bounds {
        assert!(
            *bound - prev <= 1024,
            "第 {} 片 {} 字节超过 1KB 限制",
            plan.bounds
                .iter()
                .position(|b| b == bound)
                .map_or(0, |i| i + 1),
            *bound - prev
        );
        prev = *bound;
    }
    verify_split_output(&root.join("doc.md"), &out_dir, plan.bounds.len(), 1024);
    // MB：1 MB 限制下 3.5KB 只有一片
    let plan_mb =
        md_tools::plan_splits(&root.join("doc.md"), 1024 * 1024, &Control::default()).unwrap();
    assert_eq!(plan_mb.bounds.len(), 1, "3.5KB 文件在 1MB 限制下应为 1 片");
    let out_mb = root.join("parts-mb");
    md_tools::run_split(
        &root.join("doc.md"),
        &plan_mb,
        &out_mb,
        false,
        &Control::default(),
        &|_, _| Ok(()),
    )
    .unwrap();
    verify_split_output(&root.join("doc.md"), &out_mb, 1, 1024 * 1024);
}

// 覆盖 M-10（中文与 Emoji 不被切坏；分片顺序拼接无损还原）
#[test]
fn split_never_breaks_multibyte_and_rejoins_losslessly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut text = String::new();
    // 中文（3 字节）与 Emoji（4 字节）连续排布，边界极易落在多字节字符中间
    while text.len() < 2000 {
        text.push_str("中文内容测试🎉🚀dato混合🌈正文段落。\n");
    }
    write(&root.join("emoji.md"), &text);
    let bytes = fs::read(root.join("emoji.md")).unwrap();
    for limit in [7_u64, 13, 64, 100] {
        let out_dir = root.join(format!("parts-{limit}"));
        let plan =
            md_tools::plan_splits(&root.join("emoji.md"), limit, &Control::default()).unwrap();
        assert!(!plan.bounds.is_empty());
        md_tools::run_split(
            &root.join("emoji.md"),
            &plan,
            &out_dir,
            false,
            &Control::default(),
            &|_, _| Ok(()),
        )
        .unwrap();
        // 每片 ≤ limit 且是合法 UTF-8（String::from_utf8 校验不切坏）
        for (index, end) in plan.bounds.iter().enumerate() {
            let start = if index == 0 {
                0
            } else {
                plan.bounds[index - 1]
            };
            let size = usize::try_from(end - start).unwrap();
            assert!(
                u64::try_from(size).unwrap() <= limit,
                "第 {} 片 {} 字节超过限制 {}",
                index + 1,
                size,
                limit
            );
            let chunk = &bytes[usize::try_from(start).unwrap()..usize::try_from(*end).unwrap()];
            assert!(
                std::str::from_utf8(chunk).is_ok(),
                "第 {} 片切坏了多字节字符",
                index + 1
            );
        }
        verify_split_output(&root.join("emoji.md"), &out_dir, plan.bounds.len(), limit);
    }
}

// 覆盖 M-10（极小限制：ASCII 可以按 1 字节切；限制小于最大字符字节数时报错不产出）
#[test]
fn split_tiny_limit_ascii_ok_and_too_small_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("ascii.md"), "abcdefghij");
    let plan = md_tools::plan_splits(&root.join("ascii.md"), 1, &Control::default()).unwrap();
    assert_eq!(plan.bounds.len(), 10, "ASCII 每字节都是边界");
    let out_dir = root.join("parts");
    md_tools::run_split(
        &root.join("ascii.md"),
        &plan,
        &out_dir,
        false,
        &Control::default(),
        &|_, _| Ok(()),
    )
    .unwrap();
    verify_split_output(&root.join("ascii.md"), &out_dir, 10, 1);
    // 中文（3 字节/字符）在 1 字节限制下无法不切坏 → 报错且不产出
    write(&root.join("cn.md"), "中文内容");
    let before = source_bytes_under(root);
    let entry_count = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap())
        .count();
    let error = md_tools::plan_splits(&root.join("cn.md"), 1, &Control::default())
        .expect_err("限制小于单字符字节数必须报错");
    assert!(
        format!("{error:#}").contains("无法在字符边界分片"),
        "{error:#}"
    );
    assert_eq!(source_bytes_under(root), before, "计划失败不得修改源文件");
    assert_eq!(
        fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap())
            .count(),
        entry_count,
        "计划失败不得在实际输入目录新增输出条目（M-10）"
    );
}

// 覆盖 M-11（三位编号起步；超过 999 自动扩宽且排序正确、不重名）
#[test]
fn split_names_use_three_digits_and_widen_beyond_999() {
    let names = md_tools::split_names("document.md", 5);
    assert_eq!(
        names,
        [
            "document_001.md",
            "document_002.md",
            "document_003.md",
            "document_004.md",
            "document_005.md"
        ],
        "至少三位编号"
    );
    let big = md_tools::split_names("document.md", 1200);
    assert_eq!(big.len(), 1200);
    assert_eq!(big[0], "document_0001.md", "超过 999 时统一四位宽度");
    assert_eq!(big[999], "document_1000.md");
    assert_eq!(big[1199], "document_1200.md");
    let mut sorted = big.clone();
    sorted.sort();
    assert_eq!(sorted, big, "文件名字典序与编号数值序一致（排序正确）");
    let unique: std::collections::HashSet<_> = big.iter().collect();
    assert_eq!(unique.len(), big.len(), "互不重名");
}

// 覆盖 M-11（实际写出 >999 片：命名与排序落地）
#[test]
fn split_many_parts_on_disk_sorted_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let content = "0123456789abcdefghij"; // 20 字节
    let mut text = String::new();
    while text.len() < 12_000 {
        text.push_str(content);
    }
    write(&root.join("document.md"), &text);
    let out_dir = root.join("parts");
    let plan = md_tools::plan_splits(&root.join("document.md"), 10, &Control::default()).unwrap();
    assert!(plan.bounds.len() > 999, "应有超过 999 片");
    md_tools::run_split(
        &root.join("document.md"),
        &plan,
        &out_dir,
        false,
        &Control::default(),
        &|_, _| Ok(()),
    )
    .unwrap();
    let mut names: Vec<String> = fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let expected: Vec<String> = md_tools::split_names("document.md", plan.bounds.len());
    assert_eq!(names, expected, "磁盘文件名与预期一致（四位宽度统一）");
    verify_split_output(&root.join("document.md"), &out_dir, plan.bounds.len(), 10);
}

// 覆盖 M-11（已存在同名分片：写入前检测；未确认覆盖时拒绝写入）
#[test]
fn split_detects_conflicts_and_refuses_without_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("doc.md"), "0123456789");
    let out_dir = root.join("parts");
    fs::create_dir_all(&out_dir).unwrap();
    fs::write(out_dir.join("doc_001.md"), "旧内容").unwrap();
    let names = md_tools::split_names("doc.md", 2);
    let hits = md_tools::conflicting_outputs(&out_dir, &names);
    assert_eq!(hits.len(), 1, "写入前检测到同名分片");
    let plan = md_tools::plan_splits(&root.join("doc.md"), 5, &Control::default()).unwrap();
    let error = md_tools::run_split(
        &root.join("doc.md"),
        &plan,
        &out_dir,
        false,
        &Control::default(),
        &|_, _| Ok(()),
    )
    .expect_err("未确认覆盖必须拒绝");
    assert!(error.to_string().contains("未确认覆盖"), "{error:#}");
    assert_eq!(
        fs::read_to_string(out_dir.join("doc_001.md")).unwrap(),
        "旧内容",
        "不得破坏已有文件"
    );
    assert!(
        !out_dir.join("doc_002.md").exists(),
        "检测到冲突时不得写出任何分片"
    );
    // 确认覆盖后（overwrite=true）写出全部
    md_tools::run_split(
        &root.join("doc.md"),
        &plan,
        &out_dir,
        true,
        &Control::default(),
        &|_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(out_dir.join("doc_001.md")).unwrap(),
        "01234",
        "确认后正常覆盖"
    );
    verify_split_output(&root.join("doc.md"), &out_dir, 2, 5);
}

// 覆盖 M-10（原文件不修改、不删除）
#[test]
fn split_keeps_source_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("doc.md"), "保留原样，不修改、不删除。");
    let before = fs::read(root.join("doc.md")).unwrap();
    let out_dir = root.join("parts");
    let plan = md_tools::plan_splits(&root.join("doc.md"), 8, &Control::default()).unwrap();
    md_tools::run_split(
        &root.join("doc.md"),
        &plan,
        &out_dir,
        false,
        &Control::default(),
        &|_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(
        fs::read(root.join("doc.md")).unwrap(),
        before,
        "拆分不得改动原文件"
    );
    verify_split_output(&root.join("doc.md"), &out_dir, plan.bounds.len(), 8);
}

// 覆盖 H-02/U-12/M-10：拆分规划响应停止，不创建任何分片或修改输入。
#[test]
fn split_plan_honors_cancelled_control_before_planning() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("doc.md");
    write(&input, "中文🎉正文\n");
    let control = Control::default();
    control.cancel();
    let error = md_tools::plan_splits(&input, 4, &control).expect_err("拆分规划阶段必须响应停止");
    assert!(format!("{error:#}").contains("取消"), "{error:#}");
    assert_eq!(fs::read_to_string(&input).unwrap(), "中文🎉正文\n");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}
