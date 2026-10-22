use super::*;
// F21/CR03：普通用户可创建的真实不可解析输出目录项，覆盖 exists() 误判边界。
// 私有 GUID name-surrogate reparse 不是 file-symlink；独立的 file-symlink 特例仍未验证。
// 不改变此前已有 symlink 测试，也不以普通文件、权限不足跳过或 ignore 伪造覆盖。

// F21，M-11：冲突预览必须包含普通文件与不可解析 reparse 目录项。
#[test]
fn review_f21_conflict_preview_includes_unresolvable_output_entry() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("parts");
    fs::create_dir(&out_dir).unwrap();
    let names = md_tools::split_names("document.md", 3);
    let ordinary = out_dir.join(&names[0]);
    let unresolvable = out_dir.join(&names[1]);
    write(&ordinary, "已有分片\n");
    let before = unresolvable_output::create(&unresolvable);
    assert!(md_tools::output_entry_exists(&ordinary));
    assert!(md_tools::output_entry_exists(&unresolvable));
    assert!(md_tools::output_entry_exists(&out_dir));
    assert!(!md_tools::output_entry_exists(&out_dir.join(&names[2])));

    assert_eq!(
        md_tools::conflicting_outputs(&out_dir, &names),
        vec![ordinary.clone(), unresolvable.clone()],
        "M-11：确认框预览不得遗漏已存在但无法跟随的 reparse 输出目录项"
    );
    assert_eq!(fs::read(&ordinary).unwrap(), "已有分片\n".as_bytes());
    assert_eq!(unresolvable_output::snapshot(&unresolvable), before);
    assert!(!out_dir.join(&names[2]).exists());
    assert_eq!(fs::read_dir(&out_dir).unwrap().count(), 2);
}

// F21，M-07：未授权时必须保留原始不可解析目录项，授权后替换为正确普通合并输出。
#[test]
fn review_f21_merge_preserves_unresolvable_output_without_overwrite_authorization() {
    let dir = tempfile::tempdir().unwrap();
    let input_dir = dir.path().join("input");
    let input = input_dir.join("source.md");
    let source = "# 原始内容\n";
    write(&input, source);
    let output = dir.path().join("merged.md");
    let before = unresolvable_output::create(&output);
    let control = Control::default();
    let entries = md_tools::scan_markdown(&input_dir, true, Some(&output), &control).unwrap();
    let events = std::cell::RefCell::new(Vec::new());

    let result =
        md_tools::merge_markdown_with_events(&entries, &output, false, &control, &|event| {
            events.borrow_mut().push(event);
            Ok(())
        });
    assert_eq!(
        unresolvable_output::snapshot(&output),
        before,
        "M-07：没有覆盖授权时，合并不得更改或替换原始输出目录项；结果：{result:?}"
    );
    let error = result.expect_err("M-07：已有输出目录项必须先请求覆盖授权");
    assert!(error.to_string().contains("未确认覆盖"), "{error:#}");
    assert!(events.borrow().is_empty(), "冲突预检失败时不得开始处理输入");
    assert_eq!(fs::read(&input).unwrap(), source.as_bytes());
    assert_eq!(fs::read_dir(&input_dir).unwrap().count(), 1);
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        2,
        "未授权任务不得留下任何新输出"
    );

    md_tools::merge_markdown_with_events(&entries, &output, true, &control, &|_| Ok(())).unwrap();
    unresolvable_output::assert_regular_output(&output, "# source.md\n\n## 原始内容\n".as_bytes());
    assert_eq!(fs::read(&input).unwrap(), source.as_bytes());
    assert_eq!(fs::read_dir(&input_dir).unwrap().count(), 1);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

// F21，M-08/M-11：第二分片的不可解析目录项是整次冲突，未经确认不能先写第一片。
#[test]
fn review_f21_split_preserves_unresolvable_output_without_overwrite_authorization() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("document.md");
    let source = "0123456789";
    write(&input, source);
    let out_dir = dir.path().join("parts");
    fs::create_dir(&out_dir).unwrap();
    let first = out_dir.join("document_001.md");
    let unresolvable = out_dir.join("document_002.md");
    let before = unresolvable_output::create(&unresolvable);
    let control = Control::default();
    let plan = md_tools::plan_splits(&input, 5, &control).unwrap();
    let events = std::cell::RefCell::new(Vec::new());

    let result =
        md_tools::run_split_with_events(&input, &plan, &out_dir, false, &control, &|event| {
            events.borrow_mut().push(event);
            Ok(())
        });
    assert_eq!(
        unresolvable_output::snapshot(&unresolvable),
        before,
        "M-11：没有覆盖授权时，拆分不得更改或替换原始输出目录项；结果：{result:?}"
    );
    let error = result.expect_err("M-11：已有输出目录项必须先请求覆盖授权");
    assert!(error.to_string().contains("未确认覆盖"), "{error:#}");
    assert!(
        !first.exists(),
        "M-11：发现后续分片冲突时，第一片也不能写入"
    );
    assert!(events.borrow().is_empty(), "冲突预检失败时不得开始写分片");
    assert_eq!(fs::read_dir(&out_dir).unwrap().count(), 1);
    assert_eq!(fs::read(&input).unwrap(), source.as_bytes());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);

    assert_eq!(
        md_tools::run_split_with_events(&input, &plan, &out_dir, true, &control, &|_| Ok(()))
            .unwrap(),
        10
    );
    unresolvable_output::assert_regular_output(&first, b"01234");
    unresolvable_output::assert_regular_output(&unresolvable, b"56789");
    verify_split_output(&input, &out_dir, 2, 5);
    assert_eq!(fs::read(&input).unwrap(), source.as_bytes());
    assert_eq!(fs::read_dir(&out_dir).unwrap().count(), 2);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}
