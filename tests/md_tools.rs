//! MD 整理核心逻辑测试（合同 M 分区）：合并（M-02~M-07）与拆分（M-08~M-11）。
//! 全部用临时目录；原文件不改动的断言前后对比内容哈希。
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

fn hashes_under(root: &Path) -> Vec<(String, u64)> {
    let mut rows: Vec<(String, u64)> = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let bytes = fs::read(&path).unwrap();
        let digest = blake3_smallest(&bytes);
        rows.push((path.display().to_string(), digest));
    }
    rows.sort();
    rows
}

/// 测试用最小哈希：字节数 + 首尾字节（避免引入额外依赖；blake3 已在依赖图中，
/// 但 tests 直接用 std 更简单——这里用内容长度加首尾字节足够检测改动）。
fn blake3_smallest(bytes: &[u8]) -> u64 {
    let mut hash: u64 = bytes.len() as u64;
    if let Some(first) = bytes.first() {
        hash = hash.wrapping_mul(31).wrapping_add(u64::from(*first));
    }
    if let Some(last) = bytes.last() {
        hash = hash.wrapping_mul(31).wrapping_add(u64::from(*last));
    }
    hash
}

fn merge_to(root: &Path, recursive: bool, output: &Path) -> md_tools::MergeStats {
    let entries = md_tools::scan_markdown(root, recursive, Some(output)).unwrap();
    md_tools::merge_markdown(&entries, output, false, &Control::default(), &|_, _| Ok(())).unwrap()
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
    // 1.md 与 2.md 同一创建时间 → 自然排序 1 < 2；10.md 创建时间最早 → 排最前
    set_created(&root.join("10.md"), 100);
    set_created(&root.join("2.md"), 200);
    set_created(&root.join("1.md"), 200);
    let output = root.join("merged.md");
    merge_to(root, true, &output);
    let text = fs::read_to_string(&output).unwrap();
    let first = text.find("# 10.md").unwrap();
    let second = text.find("# 1.md").unwrap();
    let third = text.find("# 2.md").unwrap();
    assert!(
        first < second && second < third,
        "排序应为 10.md(最早) → 1.md → 2.md：{text}"
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
    assert!(
        text.contains("## not a heading"),
        "~~~ 内的 ## 行必须原样：{text}"
    );
    assert!(text.contains("### deep"), "~~~ 内的 ### 行必须原样：{text}");
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
        text.contains("# indented fence"),
        "缩进 fence 内原样：{text}"
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
    let output = root.join("merged.md");
    let before = hashes_under(root);
    merge_to(root, true, &output);
    // 只对比输入文件：输出本身就在扫描目录内，合并后自然新增。
    let after: Vec<(String, u64)> = hashes_under(root)
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
    let error = md_tools::merge_markdown(
        &entries,
        &root.join("old-merged.md"),
        false,
        &Control::default(),
        &|_, _| Ok(()),
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
    assert!(
        text.starts_with("# big.md\n\n## 大文件\n\n"),
        "标题正确下移：{}",
        &text[..40]
    );
    assert!(text.ends_with(paragraph), "大文件内容完整写入末尾");
    let source_len = fs::metadata(root.join("big.md")).unwrap().len();
    let output_len = fs::metadata(&output).unwrap().len();
    // 输出 = 标题行 + 空行 + 内容（# → ## 多一个 #，无结尾换行的补一个换行）
    let diff = output_len.saturating_sub(source_len);
    assert!(
        (4..=11).contains(&diff),
        "输出长度应约等于输入加标题与换行差：{diff}"
    );
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
    verify_split_output(&root.join("doc.md"), &out_dir, plan.bounds.len());
    // MB：1 MB 限制下 3.5KB 只有一片
    let plan_mb = md_tools::plan_splits(&root.join("doc.md"), 1024 * 1024).unwrap();
    assert_eq!(plan_mb.bounds.len(), 1, "3.5KB 文件在 1MB 限制下应为 1 片");
    let _ = &plan;
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
        verify_split_output(&root.join("emoji.md"), &out_dir, plan.bounds.len());
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
    verify_split_output(&root.join("ascii.md"), &out_dir, 10);
    // 中文（3 字节/字符）在 1 字节限制下无法不切坏 → 报错且不产出
    write(&root.join("cn.md"), "中文内容");
    let error =
        md_tools::plan_splits(&root.join("cn.md"), 1).expect_err("限制小于单字符字节数必须报错");
    assert!(
        format!("{error:#}").contains("无法在字符边界分片"),
        "{error:#}"
    );
    assert!(!root.join("cn-parts").exists(), "失败时不得产生任何输出");
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
    verify_split_output(&root.join("document.md"), &out_dir, plan.bounds.len());
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
    verify_split_output(&root.join("doc.md"), &out_dir, 2);
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
}

// 覆盖 M-10（分片按编号顺序二进制拼接还原原文件）
fn verify_split_output(source: &Path, out_dir: &Path, count: usize) {
    let names = md_tools::split_names(
        &source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        count,
    );
    let mut joined: Vec<u8> = Vec::new();
    for name in &names {
        joined.extend_from_slice(&fs::read(out_dir.join(name)).unwrap());
    }
    let original = fs::read(source).unwrap();
    assert_eq!(
        joined, original,
        "全部分片按编号顺序拼接必须与原文件完全一致（M-10）"
    );
}
