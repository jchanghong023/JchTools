use super::*;
// 覆盖 M-02（不递归：只处理直属 *.md；扩展名不区分大小写）
#[test]
fn scan_nonrecursive_takes_only_top_level_markdown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("a.md"), "# A");
    write(&root.join("b.MD"), "# B");
    write(&root.join("c.txt"), "not md");
    write(&root.join("sub").join("d.md"), "# D");
    let output = root.join("out").join("merged.md");
    let stats = merge_to(root, false, &output);
    assert_eq!(
        stats.files, 2,
        "非递归只合并直属 md（含大写 .MD），不含子目录"
    );
    let text = fs::read_to_string(&output).unwrap();
    assert!(text.contains("# a.md") && text.contains("# b.MD"), "{text}");
    assert!(
        !text.contains("# d.md"),
        "子目录文件不得进入非递归合并：{text}"
    );
    assert!(!text.contains("# c.txt"), "非 md 文件不得进入合并：{text}");
}

// 覆盖 M-02（递归：处理目录及全部子目录）
#[test]
fn scan_recursive_includes_subdirectories() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("a.md"), "# A");
    write(&root.join("x").join("y").join("b.md"), "# B");
    let output = root.join("merged.md");
    let stats = merge_to(root, true, &output);
    assert_eq!(stats.files, 2);
    let text = fs::read_to_string(&output).unwrap();
    assert!(text.contains("# a.md") && text.contains("# b.md"), "{text}");
}

// 覆盖 M-03（创建时间早→晚；相同时相对路径自然排序，2.md 在 10.md 前）
#[test]
fn merge_orders_by_creation_time_then_natural_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("10.md"), "ten");
    write(&root.join("2.md"), "two");
    write(&root.join("1.md"), "one");
    // 1.md 创建时间最早排最前；2.md 与 10.md 同一创建时间 → 触发自然排序决胜
    // （2.md 在 10.md 前）。若第二排序键退化为字典序，10.md 会排到 2.md 前。
    // （旧夹具把 10.md 设为最早，同时间对只剩 1/2——两种排序同序，检不出退化。）
    set_created(&root.join("1.md"), 100);
    set_created(&root.join("2.md"), 200);
    set_created(&root.join("10.md"), 200);
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    let text = fs::read_to_string(&output).unwrap();
    let first = text.find("# 1.md").unwrap();
    let second = text.find("# 2.md").unwrap();
    let third = text.find("# 10.md").unwrap();
    assert!(
        first < second && second < third,
        "排序应为 1.md(最早) → 2.md → 10.md（同时间按自然排序 2 在 10 前）：{text}"
    );
}

// 覆盖 M-03（自然排序：数字段按数值而不是字典序）
#[test]
fn natural_sort_compares_digits_numerically() {
    use std::cmp::Ordering;
    assert_eq!(md_tools::natural_cmp("2.md", "10.md"), Ordering::Less);
    assert_eq!(md_tools::natural_cmp("10.md", "2.md"), Ordering::Greater);
    assert_eq!(md_tools::natural_cmp("a2b", "a2b"), Ordering::Equal);
    assert_eq!(
        md_tools::natural_cmp("a02", "a2"),
        Ordering::Equal,
        "前导零不参与数值"
    );
    assert_eq!(
        md_tools::natural_cmp("v2/ch1.md", "v10/ch1.md"),
        Ordering::Less
    );
}

// 覆盖 M-03：非数字段必须先按完整段比较，不能把后续数字与另一段的标点混比。
#[test]
fn natural_sort_compares_complete_nondigit_segments() {
    use std::cmp::Ordering;
    for (first, second) in [("a2.md", "a!.md"), ("目录2.md", "目录!.md")] {
        assert_eq!(md_tools::natural_cmp(first, second), Ordering::Less);
        assert_eq!(md_tools::natural_cmp(second, first), Ordering::Greater);
    }
}

// 覆盖 M-03/M-04：同创建时间时，按完整非数字段决胜的顺序须落入实际合并正文。
#[test]
fn merge_orders_by_complete_nondigit_segments() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in ["a!.md", "a2.md"] {
        write(&root.join(name), name);
        set_created(&root.join(name), 100);
    }
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    assert_eq!(
        fs::read_to_string(&output).unwrap(),
        "# a2.md\n\na2.md\n\n# a!.md\n\na!.md\n",
        "先比较非数字段 a 与 a!，不能直接把后续 2 与 ! 比较"
    );
}

// 与 organizer_fs_boundaries 的真实 GUID reparse 夹具同口径，不依赖符号链接特权。
fn mark_other_reparse(path: &Path) {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT},
        System::IO::DeviceIoControl,
    };
    let handle = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .unwrap();
    let mut data = [0u8; 24];
    data[..4].copy_from_slice(&0x0000_0042u32.to_le_bytes());
    data[8..].copy_from_slice(&[
        0x18, 0x3e, 0xca, 0x84, 0x9b, 0xe8, 0x46, 0x42, 0x90, 0x34, 0xf2, 0x17, 0x27, 0x26, 0x68,
        0x1c,
    ]);
    let mut returned = 0;
    // SAFETY: 句柄在同步调用期间有效；输入是完整 GUID reparse 头，输出指针不保留。
    let success = unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            0x0009_00a4,
            data.as_ptr().cast(),
            24,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(
        success,
        0,
        "设置真实 reparse 夹具失败：{}",
        std::io::Error::last_os_error()
    );
}

// 覆盖 M-02/S-04：FILE_ATTRIBUTE_REPARSE_POINT 不限于 symlink/junction，
// 文件与目录均须跳过；不能读取受保护目录的后代或把受保护 .md 纳入合并。
#[test]
fn scan_skips_non_symlink_reparse_entries() {
    use std::os::windows::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("plain.md"), "普通正文\n");
    write(&root.join("protected.md"), "受保护正文\n");
    fs::create_dir(root.join("protected-tree")).unwrap();
    for path in [root.join("protected.md"), root.join("protected-tree")] {
        mark_other_reparse(&path);
        let meta = fs::symlink_metadata(&path).unwrap();
        assert_ne!(meta.file_attributes() & 0x400, 0);
        assert!(!meta.file_type().is_symlink(), "必须覆盖非符号链接属性分支");
    }
    let output = root.join("merged.md");
    for recursive in [false, true] {
        let entries =
            md_tools::scan_markdown(root, recursive, Some(&output), &Control::default()).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.rel.as_str())
                .collect::<Vec<_>>(),
            ["plain.md"],
            "普通文件正常纳入，其他 reparse 文件与目录整树排除"
        );
    }
    merge_to(root, true, &output);
    assert_eq!(
        fs::read_to_string(output).unwrap(),
        "# plain.md\n\n普通正文\n"
    );
}

// 覆盖 M-02/M-07：Unicode 小写展开相同不代表 Windows 同名；新输出不得漏排普通输入。
#[test]
fn scan_keeps_distinct_unicode_name_when_excluding_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let input = root.join("\u{0130}.md");
    let output = root.join("i\u{0307}.md");
    write(&input, "# 原文\n");
    let entries = md_tools::scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    assert_eq!(entries.len(), 1, "不同 Windows 文件名不得被当作输出排除");
    assert_eq!(entries[0].path, input);
    md_tools::merge_markdown_with_events(
        &entries,
        &output,
        false,
        &Control::default(),
        &|_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(&output).unwrap(),
        "# \u{0130}.md\n\n## 原文\n"
    );
    assert_eq!(fs::read_to_string(&input).unwrap(), "# 原文\n");
}

// 覆盖 H-02/U-12：已取消任务在只读扫描阶段即停止，不等到合并写入阶段。
#[test]
fn scan_honors_cancelled_control_before_reading_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("doc.md");
    write(&input, "# 原文\n");
    let control = Control::default();
    control.cancel();
    let error = md_tools::scan_markdown(dir.path(), true, None, &control)
        .expect_err("扫描阶段必须响应停止");
    assert!(format!("{error:#}").contains("取消"), "{error:#}");
    assert_eq!(fs::read_to_string(&input).unwrap(), "# 原文\n");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}
