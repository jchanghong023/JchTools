use super::*;
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

// 覆盖 M-04/M-05：独立分隔线不是 Setext 标题文本，其后的下划线不得改写它。
#[test]
fn merge_keeps_thematic_break_before_setext_underline() {
    for marker in ["---", "***", "___", "- - -", "* * *", "_ _ _"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let source = format!("{marker}\n===\n\n普通文本\n---\n");
        write(&root.join("break.md"), &source);
        let output = root.join("merged.md");
        merge_to(root, true, &output);
        assert_eq!(
            fs::read_to_string(&output).unwrap(),
            format!("# break.md\n\n{marker}\n===\n\n### 普通文本\n"),
            "分隔线不得当作标题文本；普通文本仍须转换"
        );
        assert_eq!(fs::read_to_string(root.join("break.md")).unwrap(), source);
    }
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

// 覆盖 M-05/M-06：闭合 fence 后的空格和制表符不延长代码块保护范围。
#[test]
fn merge_closes_fence_with_trailing_tabs() {
    for fence in ["```", "~~~"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let source = format!("{fence}text\n# 原样\n{fence} \t\n# 下移\n");
        write(&root.join("fence.md"), &source);
        let output = root.join("merged.md");
        merge_to(root, true, &output);
        assert_eq!(
            fs::read_to_string(&output).unwrap(),
            format!("# fence.md\n\n{fence}text\n# 原样\n{fence} \t\n## 下移\n")
        );
        assert_eq!(fs::read_to_string(root.join("fence.md")).unwrap(), source);
    }
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
    let entries = md_tools::scan_markdown(
        root,
        true,
        Some(&root.join("old-merged.md")),
        &Control::default(),
    )
    .unwrap();
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
    let again = md_tools::scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
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
    let entries = md_tools::scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
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
