#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]

//! MD 整理核心逻辑测试（合同 M 分区）：合并（M-02~M-07）与拆分（M-08~M-11）。
//! 全部用临时目录；原文件不改动的断言前后逐字节对比。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools::{control::Control, md_tools};
use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn set_created(path: &Path, seconds: u64) {
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
    // 平台不支持创建时间时此处失败（M-03 的替代口径按修改时间）；
    // 本套用例在 Windows（P-07 唯一支持平台）上必须成功。
    jchtools::fsutil::set_created_time(path, time)
        .unwrap_or_else(|error| panic!("设置创建时间失败：{}：{error}", path.display()));
}

fn source_bytes_under(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut rows: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let bytes = fs::read(&path).unwrap();
        rows.push((path.display().to_string(), bytes));
    }
    rows.sort();
    rows
}

fn merge_to(root: &Path, recursive: bool, output: &Path) -> md_tools::MergeStats {
    let entries = md_tools::scan_markdown(root, recursive, Some(output)).unwrap();
    md_tools::merge_markdown_with_events(&entries, output, false, &Control::default(), &|_| Ok(()))
        .unwrap()
}

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
        let entries = md_tools::scan_markdown(root, recursive, Some(&output)).unwrap();
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

// 覆盖 M-04/M-05（文件名一级标题 + ATX 标题整体下移一级；六级封顶）
#[test]
fn merge_shifts_atx_headings_and_adds_file_header() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("abc.md"),
        "# 第一章\n\n正文\n\n## 说明\n\n### 三级\n\n#### 四级\n\n##### 五级\n\n###### 六级\n",
    );
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    let text = fs::read_to_string(&output).unwrap();
    let expected = "# abc.md\n\n## 第一章\n\n正文\n\n### 说明\n\n#### 三级\n\n##### 四级\n\n###### 五级\n\n###### 六级\n";
    assert_eq!(text, expected, "标题下移一级且六级封顶（不产生七级）");
    assert!(!text.contains("#######"), "不得出现七级标题");
}

// 覆盖 M-05（Setext 标题：= → ATX 二级、- → ATX 三级）
#[test]
fn merge_converts_setext_headings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("s.md"),
        "标题甲\n======\n\n正文段\n\n标题乙\n------\n\n尾段\n",
    );
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    let text = fs::read_to_string(&output).unwrap();
    let expected = "# s.md\n\n## 标题甲\n\n正文段\n\n### 标题乙\n\n尾段\n";
    assert_eq!(text, expected, "Setext 下划线转 ATX 且下划线行不再输出");
}
// 覆盖 M-05（小数点后的数字不是有序列表标记，后接 Setext 下划线仍须转换）
#[test]
fn merge_converts_setext_after_decimal_text() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("decimal.md"), "3.14\n---\n\n1. 列表项\n---\n");
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    let text = fs::read_to_string(&output).unwrap();
    assert_eq!(
        text, "# decimal.md\n\n### 3.14\n\n1. 列表项\n---\n",
        "普通数字文本后面的 Setext 下划线应转换，有序列表不应误转：{text:?}"
    );
}

// 覆盖 M-04/M-05（带 UTF-8 BOM 的文件：首行 ATX 标题仍须下移一级，BOM 剥除不残留）
#[test]
fn merge_shifts_first_atx_heading_in_bom_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Windows 旧版记事本、PowerShell 重定向等来源常写出带 UTF-8 BOM 的 .md
    let mut content = String::from("\u{FEFF}");
    content.push_str("# 标题\n\n正文\n");
    write(&root.join("bom.md"), &content);
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    let text = fs::read_to_string(&output).unwrap();
    assert_eq!(
        text, "# bom.md\n\n## 标题\n\n正文\n",
        "带 BOM 文件的首行 ATX 标题必须下移一级：{text:?}"
    );
    assert!(
        !text.contains('\u{FEFF}'),
        "文件开头 BOM 必须剥除，不得残留在输出中：{text:?}"
    );
}

// 覆盖 M-04/M-05（带 UTF-8 BOM 的文件：首行 Setext 标题转换为二级 ATX 且不含 BOM 字节）
#[test]
fn merge_converts_first_setext_heading_in_bom_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut content = String::from("\u{FEFF}");
    content.push_str("标题\n====\n\n正文\n");
    write(&root.join("bom.md"), &content);
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    let text = fs::read_to_string(&output).unwrap();
    assert_eq!(
        text, "# bom.md\n\n## 标题\n\n正文\n",
        "带 BOM 文件的首行 Setext 标题必须转换为二级 ATX：{text:?}"
    );
    assert!(
        !text.contains('\u{FEFF}'),
        "文件开头 BOM 必须剥除，不得嵌进转换后的标题文本：{text:?}"
    );
}

// 覆盖 M-04/M-05（BOM 只在文件开头剥除一次：无 BOM 文件行为不变，
// 文件中间出现的 U+FEFF 字节按普通文本原样保留）
#[test]
fn merge_strips_bom_only_at_file_start() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("plain.md"), "# 普通\n\n正文\n");
    write(&root.join("mid.md"), "开头\n\n零宽\u{FEFF}夹在中间\n");
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    let text = fs::read_to_string(&output).unwrap();
    assert!(
        text.contains("# plain.md\n\n## 普通\n\n正文\n"),
        "无 BOM 文件行为不变：{text:?}"
    );
    assert!(
        text.contains("零宽\u{FEFF}夹在中间"),
        "文件中间的 U+FEFF 不是 BOM，必须原样保留：{text:?}"
    );
}

// 覆盖 M-05（Setext 不误伤：空行后的 --- 是分隔线不是标题；列表项后的 --- 不是标题）
#[test]
fn merge_keeps_thematic_break_and_list_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("t.md"), "段落一\n\n---\n\n- 列表项\n\n---\n");
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    let text = fs::read_to_string(&output).unwrap();
    assert!(text.contains("\n---\n"), "thematic break 保持原样：{text}");
    assert!(text.contains("- 列表项"), "列表项保持原样：{text}");
    assert!(
        !text.contains("## 段落一"),
        "空行后的 --- 不得把上一段变成标题：{text}"
    );
    assert!(
        !text.contains("### - 列表项"),
        "列表项后的 --- 不是 Setext：{text}"
    );
}

// 覆盖 M-06（fenced code block 内形似标题的内容原样保留；``` 与 ~~~ 都支持）
#[test]
fn merge_protects_fenced_code_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("c.md"),
        "# 真标题\n\n```bash\n# shell comment\necho hello\n```\n\n~~~\n## not a heading\n### deep\n~~~\n\n## after\n",
    );
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    let text = fs::read_to_string(&output).unwrap();
    assert!(text.contains("## 真标题"), "代码块外标题正常下移：{text}");
    assert!(
        text.contains("# shell comment"),
        "``` 内的 # 行必须原样：{text}"
    );
    // 正向 contains 在「漏识别 fence、内容被下移一级」的回归下照样成立
    // （"## shell comment" 包含子串 "# shell comment"），必须同时断言下移后的
    // 形态不出现，M-06 的保护才有检测能力。
    assert!(
        !text.contains("## shell comment"),
        "fence 内的 # 行被当作标题下移了：{text}"
    );
    assert!(
        text.contains("## not a heading"),
        "~~~ 内的 ## 行必须原样：{text}"
    );
    assert!(
        !text.contains("### not a heading"),
        "fence 内的 ## 行被当作标题下移了：{text}"
    );
    assert!(text.contains("### deep"), "~~~ 内的 ### 行必须原样：{text}");
    assert!(
        !text.contains("#### deep"),
        "fence 内的 ### 行被当作标题下移了：{text}"
    );
    assert!(
        text.contains("### after"),
        "代码块结束后标题恢复下移：{text}"
    );
}

// 覆盖 M-04/M-06（带语言标注的 fence 与缩进 fence）
#[test]
fn merge_handles_info_string_and_indented_fence() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("i.md"),
        "```rust\n# [dependencies]\n```\n\n   ```text\n   # indented fence\n   ```\n\n# 尾标题\n",
    );
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    let text = fs::read_to_string(&output).unwrap();
    assert!(
        text.contains("# [dependencies]"),
        "info string fence 内原样：{text}"
    );
    assert!(
        !text.contains("## [dependencies]"),
        "info string fence 内被当作标题下移了：{text}"
    );
    assert!(
        text.contains("# indented fence"),
        "缩进 fence 内原样：{text}"
    );
    assert!(
        !text.contains("## indented fence"),
        "缩进 fence 内被当作标题下移了：{text}"
    );
    assert!(text.contains("## 尾标题"), "fence 后标题下移：{text}");
}

// 覆盖 M-02/M-04/M-08/M-10（原文件不修改；无换行结尾的文件之间不粘连）
#[test]
fn merge_keeps_sources_untouched_and_separates_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("a.md"), "# 甲\n\n内容甲没有结尾换行");
    write(&root.join("b.md"), "# 乙\n\n内容乙\n");
    // M-03 按创建时间排序；显式设置顺序，不依赖文件系统时间戳精度。
    set_created(&root.join("a.md"), 100);
    set_created(&root.join("b.md"), 200);
    let output = root.join("merged.md");
    let before = source_bytes_under(root);
    merge_to(root, true, &output);
    // 只对比输入文件：输出本身就在扫描目录内，合并后自然新增。
    let after: Vec<(String, Vec<u8>)> = source_bytes_under(root)
        .into_iter()
        .filter(|(name, _)| !name.ends_with("merged.md"))
        .collect();
    assert_eq!(before, after, "合并不得修改任何原文件（M-02）");
    let text = fs::read_to_string(&output).unwrap();
    assert!(
        text.contains("内容甲没有结尾换行\n\n# b.md"),
        "上一文件末行与下一文件标题之间必须有空行：{text:?}"
    );
}

// 覆盖 M-07（输出文件位于被扫描目录中时不作为输入；已存在且未确认覆盖时拒绝写入）
#[test]
fn merge_excludes_own_output_and_refuses_silent_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("a.md"), "# 甲");
    write(&root.join("old-merged.md"), "上次合并的输出");
    // 第一次：输出到 old-merged.md（已存在）——不得静默覆盖
    let entries = md_tools::scan_markdown(root, true, Some(&root.join("old-merged.md"))).unwrap();
    let error = md_tools::merge_markdown_with_events(
        &entries,
        &root.join("old-merged.md"),
        false,
        &Control::default(),
        &|_| Ok(()),
    )
    .expect_err("已存在且未确认覆盖必须报错");
    assert!(error.to_string().contains("未确认覆盖"), "{error:#}");
    assert_eq!(
        fs::read_to_string(root.join("old-merged.md")).unwrap(),
        "上次合并的输出",
        "拒绝写入时不得破坏已有文件"
    );
    // 第二次：输出 merged.md 不存在 → 生成，且 old-merged.md 作为普通输入参与
    let output = root.join("merged.md");
    let stats = merge_to(root, true, &output);
    assert_eq!(
        stats.files, 2,
        "旧输出（非本次输出）作为普通输入参与：{stats:?}"
    );
    let text = fs::read_to_string(&output).unwrap();
    assert!(text.contains("# old-merged.md"), "{text}");
    // 第三次：重复合并到同一输出——扫描阶段排除自身，不再滚雪球
    let again = md_tools::scan_markdown(root, true, Some(&output)).unwrap();
    assert_eq!(again.len(), 2, "本次输出文件不得作为输入参与合并（M-07）");
}

// 覆盖 M-07（输出排除不依赖路径大小写拼写：Windows 路径不区分大小写，
// 输入框与输出框对同一目录的大小写拼写不同时仍必须排除输出自身）
#[test]
fn merge_excludes_output_regardless_of_path_casing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("a.md"), "# 甲");
    std::fs::write(root.join("merged.md"), "旧输出").unwrap();
    // 输出路径按全小写拼写（与磁盘真实大小写不同），但指向同一文件
    let lowered = std::path::PathBuf::from(root.display().to_string().to_lowercase());
    let output = lowered.join("merged.md");
    let entries = md_tools::scan_markdown(root, true, Some(&output)).unwrap();
    let rels: Vec<String> = entries.iter().map(|e| e.rel.clone()).collect();
    assert_eq!(
        rels,
        vec!["a.md".to_string()],
        "大小写拼写不同的同一输出文件必须被排除（M-07）：{rels:?}"
    );
}

// 覆盖 M-07（大文件流式处理正确性：5MB 输入的合并输出包含全部内容）
#[test]
fn merge_handles_large_file_streaming() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let paragraph = "这一段是用于构造大文件的中文内容，长度固定，重复填充。\n";
    let mut big = String::with_capacity(5 * 1024 * 1024 + 256);
    big.push_str("# 大文件\n\n");
    while big.len() < 5 * 1024 * 1024 {
        big.push_str(paragraph);
    }
    write(&root.join("big.md"), &big);
    let output = root.join("merged.md");
    let stats = merge_to(root, true, &output);
    assert_eq!(stats.files, 1);
    let text = fs::read_to_string(&output).unwrap();
    // M-04/M-05/M-07：逐字节核对全部正文，避免只看首尾和近似长度漏掉中间改动。
    let expected = format!("# big.md\n\n#{big}");
    assert_eq!(text, expected, "大文件标题下移且全部正文完整保留");
}

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
    let plan = md_tools::plan_splits(&root.join("doc.md"), 1024).unwrap();
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
    let plan_mb = md_tools::plan_splits(&root.join("doc.md"), 1024 * 1024).unwrap();
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
        let plan = md_tools::plan_splits(&root.join("emoji.md"), limit).unwrap();
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
    let plan = md_tools::plan_splits(&root.join("ascii.md"), 1).unwrap();
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
    let entry_count = fs::read_dir(root).unwrap().map(|entry| entry.unwrap()).count();
    let error =
        md_tools::plan_splits(&root.join("cn.md"), 1).expect_err("限制小于单字符字节数必须报错");
    assert!(
        format!("{error:#}").contains("无法在字符边界分片"),
        "{error:#}"
    );
    assert_eq!(source_bytes_under(root), before, "计划失败不得修改源文件");
    assert_eq!(
        fs::read_dir(root).unwrap().map(|entry| entry.unwrap()).count(),
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
    let plan = md_tools::plan_splits(&root.join("document.md"), 10).unwrap();
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
    let plan = md_tools::plan_splits(&root.join("doc.md"), 5).unwrap();
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
    let plan = md_tools::plan_splits(&root.join("doc.md"), 8).unwrap();
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

// 覆盖 M-10（分片按编号顺序二进制拼接还原原文件）
fn verify_split_output(source: &Path, out_dir: &Path, count: usize, limit: u64) {
    let names = md_tools::split_names(
        &source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        count,
    );
    let mut joined: Vec<u8> = Vec::new();
    for name in &names {
        let chunk = fs::read(out_dir.join(name)).unwrap();
        assert!(
            u64::try_from(chunk.len()).unwrap() <= limit,
            "实际分片 {name} 为 {} 字节，超过 {limit} 字节硬限制（M-09）",
            chunk.len()
        );
        assert!(
            std::str::from_utf8(&chunk).is_ok(),
            "实际分片 {name} 切坏了 UTF-8 字符（M-10）"
        );
        joined.extend_from_slice(&chunk);
    }
    let original = fs::read(source).unwrap();
    assert_eq!(
        joined, original,
        "全部分片按编号顺序拼接必须与原文件完全一致（M-10）"
    );
}
