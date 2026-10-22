use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

/// 单文件合并的便捷入口：写入一个输入文件后按主流程（扫描排除输出 → 事件入口合并）执行。
fn merge_single(root: &Path, file: &str, content: &str) -> String {
    write(&root.join(file), content);
    let output = root.join("merged.md");
    let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    merge_markdown_with_events(&entries, &output, false, &Control::default(), &|_| Ok(())).unwrap();
    fs::read_to_string(&output).unwrap()
}

/// 用 `cmd /c mklink /J` 创建目录 junction（P-07：Windows 唯一支持平台）。
fn make_junction(link: &Path, target: &Path) {
    let output = std::process::Command::new("cmd")
        .arg("/c")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()
        .expect("启动 cmd 失败");
    assert!(
        output.status.success(),
        "mklink /J 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// 覆盖 M-02/S-04（根是 junction：拒绝整个任务，不读取目标树；普通目录正常扫描）
#[test]
fn scan_rejects_junction_root_and_accepts_real_directory() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("a.md"), "# 甲").unwrap();
    let link = dir.path().join("link");
    make_junction(&link, &real);
    let error = scan_markdown(&link, true, None, &Control::default())
        .expect_err("根是 junction 时必须拒绝任务");
    let text = format!("{error:#}");
    assert!(
        text.contains("链接") || text.contains("junction"),
        "错误信息须说明链接边界被拒绝：{text}"
    );
    // 正控：真实目录照常扫描（M-02 不因链接防护误伤普通目录）
    let entries = scan_markdown(&real, true, None, &Control::default()).unwrap();
    assert_eq!(entries.len(), 1, "普通目录必须正常扫描");
}

// 覆盖 M-02/S-04（祖先 junction：所选根的访问路径经过链接边界时同样拒绝）
#[test]
fn scan_rejects_junction_ancestor() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir_all(real.join("sub")).unwrap();
    fs::write(real.join("sub").join("a.md"), "# 甲").unwrap();
    let link = dir.path().join("link");
    make_junction(&link, &real);
    let error = scan_markdown(&link.join("sub"), true, None, &Control::default())
        .expect_err("访问路径经过 junction 时必须拒绝任务");
    let text = format!("{error:#}");
    assert!(
        text.contains("链接") || text.contains("junction"),
        "错误信息须说明链接边界被拒绝：{text}"
    );
}

// F22，M-02/S-04：相对根的隐含当前目录祖先也必须检查；不能 canonicalize 抹掉 junction。
// 同一测试 EXE 的子进程独占 cwd，避免修改并行测试共享的进程当前目录。
#[test]
fn review_f22_relative_root_rejects_implicit_junction_ancestor() {
    const CHILD_CWD: &str = "JCHTOOLS_MD_REVIEW_F22_CWD";
    const REAL_ROOT: &str = "JCHTOOLS_MD_REVIEW_F22_REAL_ROOT";
    if let Some(cwd) = std::env::var_os(CHILD_CWD) {
        let cwd = PathBuf::from(cwd);
        assert_eq!(
            std::env::current_dir().unwrap(),
            cwd,
            "夹具必须保留经过 junction 的 cwd 拼写，不能先解析为真实路径"
        );
        let real_root = PathBuf::from(std::env::var_os(REAL_ROOT).unwrap());
        for recursive in [false, true] {
            let entries = scan_markdown(&real_root, recursive, None, &Control::default()).unwrap();
            assert_eq!(entries.len(), 1, "正控：真实绝对目录必须可读取");
            assert_eq!(entries[0].rel, "source.md");
            for root in [Path::new("docs"), Path::new(r"..\work\docs")] {
                let result = scan_markdown(root, recursive, None, &Control::default());
                assert!(
                    result.is_err(),
                    "M-02/S-04：相对根 {} 的隐含祖先是 junction，必须拒绝整个扫描，\
                         不得读取其目标：{result:?}",
                    root.display()
                );
            }
        }
        assert_eq!(
            fs::read(real_root.join("source.md")).unwrap(),
            b"# protected source\n"
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    let real_root = real.join("work").join("docs");
    write(&real_root.join("source.md"), "# protected source\n");
    let link = dir.path().join("alias");
    make_junction(&link, &real);
    assert!(jchtools_file_core::fsutil::is_link(
        &fs::symlink_metadata(&link).unwrap()
    ));
    let cwd = link.join("work");
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "md_tools::tests::review_f22_relative_root_rejects_implicit_junction_ancestor",
            "--nocapture",
        ])
        .current_dir(&cwd)
        .env(CHILD_CWD, &cwd)
        .env(REAL_ROOT, &real_root)
        .output();
    // 即使子进程断言失败，也先删除 junction 本身，再交给 TempDir 清理本测试数据。
    fs::remove_dir(&link).unwrap();
    let output = result.expect("启动隔离 cwd 的 MD 扫描子进程失败");
    assert!(
        output.status.success(),
        "相对根 junction 边界回归失败：\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(real_root.join("source.md")).unwrap(),
        b"# protected source\n"
    );
}

// 覆盖 M-07（输出路径用「..」拼写时仍必须与普通拼写同口径排除本次输出）
#[test]
fn scan_excludes_output_written_with_dotdot_alias() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("a.md"), "# 甲");
    write(&root.join("merged.md"), "旧输出");
    let output = root.join("sub").join("..").join("merged.md");
    let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    let rels: Vec<&str> = entries.iter().map(|entry| entry.rel.as_str()).collect();
    assert_eq!(
        rels,
        vec!["a.md"],
        "「..」拼写的输出路径必须被排除（M-07）：{rels:?}"
    );
}

// 覆盖 M-07（输出路径用 `/` 分隔符拼写时仍必须被排除）
#[test]
fn scan_excludes_output_written_with_forward_slashes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("a.md"), "# 甲");
    write(&root.join("merged.md"), "旧输出");
    let output = PathBuf::from(format!("{}/merged.md", root.display()));
    let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    let rels: Vec<&str> = entries.iter().map(|entry| entry.rel.as_str()).collect();
    assert_eq!(
        rels,
        vec!["a.md"],
        "分隔符变体拼写的输出必须被排除：{rels:?}"
    );
}

// 覆盖 M-07：扩展路径与普通路径指向同一输出，双向拼写均须排除输出并允许确认覆盖。
#[test]
fn merge_excludes_output_across_verbatim_path_spellings() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = dir.path().canonicalize().unwrap();
    let ordinary = PathBuf::from(jchtools_file_core::platform::display_path_text(
        &canonical.to_string_lossy(),
    ));
    assert_ne!(canonical, ordinary, "Windows canonicalize 应产生扩展路径");
    let original = "# 原始正文\n";
    write(&ordinary.join("a.md"), original);
    for (root, output_dir) in [(&ordinary, &canonical), (&canonical, &ordinary)] {
        let output = output_dir.join("merged.md");
        fs::write(&output, "旧输出不得参与合并\n").unwrap();
        let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.rel.as_str())
                .collect::<Vec<_>>(),
            ["a.md"],
            "普通/扩展路径拼写不得把输出本身当作输入"
        );
        merge_markdown_with_events(&entries, &output, true, &Control::default(), &|_| Ok(()))
            .unwrap();
        assert_eq!(
            fs::read_to_string(&output).unwrap(),
            "# a.md\n\n## 原始正文\n"
        );
        assert_eq!(fs::read_to_string(ordinary.join("a.md")).unwrap(), original);
        assert_eq!(
            fs::read_dir(&ordinary).unwrap().count(),
            2,
            "不得残留临时文件"
        );
    }
}

// 覆盖 M-07：UNC 只比较路径文本，不访问网络共享；不同共享及不同文件不得误排除。
#[test]
fn output_path_identity_matches_verbatim_unc_without_network_access() {
    let ordinary = Path::new(r"\\server\share\docs\merged.md");
    let verbatim = Path::new(r"\\?\UNC\SERVER\SHARE\docs\merged.md");
    assert!(same_file_path(ordinary, verbatim));
    assert!(same_file_path(verbatim, ordinary));
    assert!(!same_file_path(
        ordinary,
        Path::new(r"\\?\UNC\server\other\docs\merged.md")
    ));
    assert!(!same_file_path(
        ordinary,
        Path::new(r"\\?\UNC\server\share\docs\source.md")
    ));
}

// 覆盖 M-07（输出与输入是同一实体的硬链接：扫描期拒绝任务，绝不进入覆盖确认，
// 更不得在确认覆盖后经输出名截断另一名字下的原始输入）
#[test]
fn merge_rejects_output_hardlinked_to_input() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let original = b"# \xe5\x8e\x9f\xe5\xa7\x8b\xe5\x86\x85\xe5\xae\xb9\n"; // "# 原始内容\n"
    fs::write(root.join("a.md"), original).unwrap();
    let output = root.join("out.md");
    fs::hard_link(root.join("a.md"), &output).unwrap();
    let error = scan_markdown(root, true, Some(&output), &Control::default())
        .expect_err("输出与输入是同一实体的硬链接：必须拒绝整个任务");
    assert!(
        format!("{error:#}").contains("同一个文件"),
        "错误信息须说明输出与输入是同一个文件：{error:#}"
    );
    assert_eq!(
        fs::read(root.join("a.md")).unwrap(),
        original,
        "原始输入字节必须原封不动"
    );
}

// 覆盖 M-08/M-11（回归：分片目标与输入是同一实体的硬链接时，确认覆盖经
// 临时文件 + rename 替换目录项，输入字节原封不动；修复前 `File::create`
// 直接截断共享 inode，把输入也清空，随后读取提前结束）
#[test]
fn split_overwrite_hardlinked_part_keeps_input_intact() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let content = "# 拆分内容\n".repeat(64);
    let input = root.join("document.md");
    fs::write(&input, content.as_bytes()).unwrap();
    let plan = plan_splits(&input, 1024, &Control::default()).unwrap();
    let out_dir = root.join("out");
    fs::create_dir_all(&out_dir).unwrap();
    let names = split_names("document.md", plan.bounds.len());
    // 预置的分片恰好是输入的硬链接（用户曾用其他工具硬链接过）。
    fs::hard_link(&input, out_dir.join(&names[0])).unwrap();
    let written = run_split(
        &input,
        &plan,
        &out_dir,
        true, // 用户已在界面确认覆盖既有分片
        &Control::default(),
        &|_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(written, content.len() as u64, "写入字节应等于输入大小");
    assert_eq!(
        fs::read(&input).unwrap(),
        content.as_bytes(),
        "输入与分片曾是同一实体的硬链接：覆盖分片必须只替换目录项，输入字节原封不动（M-08）"
    );
    let mut rejoined = Vec::new();
    let mut index = 1u32;
    loop {
        let path = out_dir.join(format!("document_{index:03}.md"));
        let Ok(bytes) = fs::read(&path) else {
            break;
        };
        rejoined.extend_from_slice(&bytes);
        index += 1;
    }
    assert_eq!(
        rejoined,
        content.as_bytes(),
        "按编号拼接必须还原输入（M-10）"
    );
    let leftovers: Vec<_> = fs::read_dir(&out_dir)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.to_ascii_lowercase().ends_with(".tmp").then_some(name)
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "成功路径不得残留临时文件：{leftovers:?}"
    );
}

// 覆盖 M-07（正常覆盖既有输出：确认后经临时文件原子替换，不留临时文件，输入不动）
#[test]
fn merge_atomic_overwrite_existing_output_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let a_before = b"# \xe7\x94\xb2\n".to_vec(); // "# 甲\n"
    let b_before = b"# \xe4\xb9\x99\n".to_vec(); // "# 乙\n"
    fs::write(root.join("a.md"), &a_before).unwrap();
    fs::write(root.join("b.md"), &b_before).unwrap();
    let output = root.join("merged.md");
    fs::write(&output, "旧输出").unwrap();
    let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    assert_eq!(entries.len(), 2);
    merge_markdown_with_events(&entries, &output, true, &Control::default(), &|_| Ok(())).unwrap();
    let text = fs::read_to_string(&output).unwrap();
    assert!(text.contains("# a.md") && text.contains("# b.md"), "{text}");
    assert!(
        !text.contains("旧输出"),
        "确认覆盖后旧内容必须被替换：{text}"
    );
    assert_eq!(
        fs::read(root.join("a.md")).unwrap(),
        a_before,
        "输入不得被改动"
    );
    assert_eq!(
        fs::read(root.join("b.md")).unwrap(),
        b_before,
        "输入不得被改动"
    );
    for entry in fs::read_dir(root).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(".jchtools-md-out-"),
            "任务结束后不得残留临时文件：{name}"
        );
    }
}

// 覆盖 M-07（输出目标被目录占用：必须明确报错，不得留下临时文件或破坏该目录）
#[test]
fn merge_rename_failure_reports_and_cleans_temp() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("a.md"), "# 甲");
    let blocker = root.join("blocker.md");
    fs::create_dir(&blocker).unwrap();
    let entries = scan_markdown(root, true, Some(&blocker), &Control::default()).unwrap();
    assert_eq!(entries.len(), 1);
    let error =
        merge_markdown_with_events(&entries, &blocker, true, &Control::default(), &|_| Ok(()))
            .expect_err("输出目标被目录占用时必须报错");
    assert!(
        format!("{error:#}").contains("失败"),
        "错误信息须明确失败上下文：{error:#}"
    );
    assert!(blocker.is_dir(), "被占用的目录不得被破坏");
    for entry in fs::read_dir(root).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(".jchtools-md-out-"),
            "失败收尾不得残留临时文件：{name}"
        );
    }
}

fn rows_for(rels: &[&str]) -> Vec<(SystemTime, MergeEntry)> {
    rels.iter()
        .map(|rel| {
            (
                SystemTime::UNIX_EPOCH,
                MergeEntry {
                    path: PathBuf::from(format!("/{rel}")),
                    rel: (*rel).to_string(),
                    file_name: rel.rsplit('/').next().unwrap().to_string(),
                },
            )
        })
        .collect()
}

fn rels_of(rows: &[(SystemTime, MergeEntry)]) -> Vec<String> {
    rows.iter().map(|(_, entry)| entry.rel.clone()).collect()
}

// 覆盖 M-03（自然排序对数值相等但写法不同的路径不构成全序：追加归一化文本决胜后，
// 任意初始枚举顺序必须得到同一最终顺序）
#[test]
fn sort_is_total_order_across_permutations() {
    // 1.md / 01.md / 001.md 数值段剥零比较全部相等——旧实现直接返回 Equal，
    // 顺序退化为枚举序。全部 6 种初始排列排序后必须得到同一结果。
    let permutations: Vec<Vec<&str>> = vec![
        vec!["1.md", "01.md", "001.md"],
        vec!["1.md", "001.md", "01.md"],
        vec!["01.md", "1.md", "001.md"],
        vec!["01.md", "001.md", "1.md"],
        vec!["001.md", "1.md", "01.md"],
        vec!["001.md", "01.md", "1.md"],
    ];
    let mut orders: Vec<Vec<String>> = Vec::new();
    for permutation in &permutations {
        let mut rows = rows_for(permutation);
        sort_rows(&mut rows);
        orders.push(rels_of(&rows));
    }
    for order in &orders[1..] {
        assert_eq!(
            order, &orders[0],
            "不同初始顺序必须得到同一最终顺序（M-03）"
        );
    }
    assert_eq!(
        orders[0],
        vec![
            "001.md".to_string(),
            "01.md".to_string(),
            "1.md".to_string()
        ],
        "决胜顺序必须确定（归一化文本字节序）"
    );
    // 混入数值不同的路径：数值比较仍优先于文本决胜
    let mut rows = rows_for(&["10.md", "9.md", "010.md", "009.md"]);
    sort_rows(&mut rows);
    assert_eq!(
        rels_of(&rows),
        vec![
            "009.md".to_string(),
            "9.md".to_string(),
            "010.md".to_string(),
            "10.md".to_string(),
        ],
        "数值序优先：9 < 10；同数值按归一化文本决胜：009 < 010、009 < 9"
    );
}

// 覆盖 M-03（同一创建时间的 1.md/01.md/001.md：端到端扫描顺序确定）
#[test]
fn scan_orders_equal_creation_times_deterministically() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in ["1.md", "01.md", "001.md"] {
        write(&root.join(name), "x");
        jchtools_file_core::fsutil::set_created_time(
            &root.join(name),
            SystemTime::UNIX_EPOCH + Duration::from_secs(100),
        )
        .unwrap();
    }
    let output = root.join("merged.md");
    let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    assert_eq!(
        rels_of(
            &entries
                .into_iter()
                .map(|entry| (SystemTime::UNIX_EPOCH, entry))
                .collect::<Vec<_>>()
        ),
        vec![
            "001.md".to_string(),
            "01.md".to_string(),
            "1.md".to_string()
        ],
        "同创建时间必须按确定的第二排序键排出全序（M-03）"
    );
}

// 覆盖 M-05（Setext 判定要求下划线行紧跟「非空文本行」：*important* 是强调文本
// 不是列表（标记后无空格），其后的 === 必须转换为 ATX 二级标题）
#[test]
fn merge_converts_setext_after_emphasis_like_text() {
    let dir = tempfile::tempdir().unwrap();
    let text = merge_single(dir.path(), "emph.md", "*important*\n===\n");
    assert_eq!(
        text, "# emph.md\n\n## *important*\n",
        "强调文本行是普通文本，=== 下划线必须照常转换：{text:?}"
    );
}

// 覆盖 M-05（+name / -text 同为非列表文本：其后的 Setext 下划线必须转换）
#[test]
fn merge_converts_setext_after_plus_and_dash_text() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("plus.md"), "+name\n===\n");
    write(&root.join("dash.md"), "-text\n---\n");
    let output = root.join("merged.md");
    let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    merge_markdown_with_events(&entries, &output, false, &Control::default(), &|_| Ok(())).unwrap();
    let text = fs::read_to_string(&output).unwrap();
    assert!(
        text.contains("## +name"),
        "+name 不是列表（标记后无空格），=== 必须转换：{text:?}"
    );
    assert!(
        text.contains("### -text"),
        "-text 不是列表（标记后无空格），--- 必须转换：{text:?}"
    );
}

// 覆盖 M-05/M-06（真列表与围栏内部不因新判定被误转换）
#[test]
fn merge_keeps_real_list_and_fence_interior_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("list.md"), "- 真列表项\n\n---\n");
    let text_list = {
        let output = root.join("merged.md");
        let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
        merge_markdown_with_events(&entries, &output, false, &Control::default(), &|_| Ok(()))
            .unwrap();
        fs::read_to_string(&output).unwrap()
    };
    assert!(
        text_list.contains("- 真列表项") && text_list.contains("\n---\n"),
        "真列表与 thematic break 保持原样：{text_list:?}"
    );
    assert!(
        !text_list.contains("### "),
        "真列表后的 --- 不是 Setext：{text_list:?}"
    );

    let dir2 = tempfile::tempdir().unwrap();
    let text_fence = merge_single(dir2.path(), "fence.md", "```\n*important*\n===\n```\n");
    assert_eq!(
        text_fence, "# fence.md\n\n```\n*important*\n===\n```\n",
        "围栏代码块内部的相同文本必须原样（M-06）：{text_fence:?}"
    );
}

// 覆盖 U-03/U-12（单文件任务：FileStarted 发出时完成计数不得把当前文件算进去，
// 事件序列必须是 Started → Completed）
#[test]
fn merge_events_started_before_completed_single_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("a.md"), "# 甲\n");
    let output = root.join("merged.md");
    let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    assert_eq!(entries.len(), 1);
    let completed = std::cell::Cell::new(0usize);
    let seen = std::cell::RefCell::new(Vec::new());
    merge_markdown_with_events(&entries, &output, false, &Control::default(), &|event| {
        match event {
            MdProgress::FileStarted(index, total) => {
                assert_eq!((index, total), (1, 1));
                assert_eq!(
                    completed.get(),
                    index - 1,
                    "FileStarted 发出时完成计数不得已包含当前文件（单文件开局不得显示 1/1）"
                );
            }
            MdProgress::FileCompleted(index, _total) => {
                completed.set(index);
            }
        }
        seen.borrow_mut().push(event);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        seen.into_inner(),
        vec![
            MdProgress::FileStarted(1, 1),
            MdProgress::FileCompleted(1, 1)
        ],
        "事件序列必须先 Started 后 Completed"
    );
    assert_eq!(completed.into_inner(), 1, "任务完成后完成计数必须达到总数");
}

// 覆盖 U-12（合并：第 1 个文件完成后取消，第 2 个文件不得开始，输出不落盘）
#[test]
fn merge_stop_between_files_never_starts_next() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("a.md"), "# 甲\n");
    write(&root.join("b.md"), "# 乙\n");
    let output = root.join("merged.md");
    let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    assert_eq!(entries.len(), 2);
    let control = Control::default();
    let seen = std::cell::RefCell::new(Vec::new());
    let error = merge_markdown_with_events(&entries, &output, false, &control, &|event| {
        seen.borrow_mut().push(event);
        if event == MdProgress::FileCompleted(1, 2) {
            control.cancel();
        }
        Ok(())
    })
    .expect_err("完成后取消必须中止任务");
    assert!(
        format!("{error:#}").contains("取消"),
        "取消必须以明确错误收尾：{error:#}"
    );
    assert!(
        !seen.borrow().contains(&MdProgress::FileStarted(2, 2)),
        "取消后不得开始下一个输入：{:?}",
        seen.borrow()
    );
    assert!(!output.exists(), "中止的合并不得留下半成品输出");
}

// 覆盖 M-07/U-12：最后一个文件完成回调收到取消时，正式提交前仍须停止，保护旧输出。
#[test]
fn merge_cancelled_on_final_completion_keeps_existing_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("a.md"), "# 甲\n");
    let output = root.join("merged.md");
    let original = b"existing output";
    fs::write(&output, original).unwrap();
    let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    let control = Control::default();
    let error = merge_markdown_with_events(&entries, &output, true, &control, &|event| {
        if event == MdProgress::FileCompleted(1, 1) {
            control.cancel();
        }
        Ok(())
    })
    .expect_err("正式提交前取消必须停止合并");
    assert!(format!("{error:#}").contains("取消"), "{error:#}");
    assert_eq!(fs::read(&output).unwrap(), original, "取消不得覆盖旧输出");
    assert_eq!(fs::read(root.join("a.md")).unwrap(), "# 甲\n".as_bytes());
    for entry in fs::read_dir(root).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(".jchtools-md-out-"),
            "取消收尾不得残留临时文件：{name}"
        );
    }
}

// 覆盖 M-04/M-05：一至三格缩进的列表和引用仍是块结构，不得误改成 Setext 标题。
#[test]
fn merge_keeps_indented_block_starts_untouched() {
    for indent in 1..=3 {
        for block in [
            "- 列表项",
            "* 列表项",
            "+ 列表项",
            "> 引用",
            "1. 列表项",
            "1) 列表项",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let source = format!("{}{block}\n---\n", " ".repeat(indent));
            let text = merge_single(dir.path(), "block.md", &source);
            assert_eq!(
                text,
                format!("# block.md\n\n{source}"),
                "缩进块结构不得转换成标题：{source:?}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let spaces = " ".repeat(indent);
        let source = format!("{spaces}普通文本\n===\n");
        let text = merge_single(dir.path(), "text.md", &source);
        assert_eq!(
            text,
            format!("# text.md\n\n{spaces}## 普通文本\n"),
            "缩进普通文本的 Setext 转换必须保留"
        );
    }
}

// 覆盖 U-12（合并的文件内循环必须有停止检查点：暂停 rendezvous 证明任务停在
// 大文件中途；取消后及时以错误收尾、不开始下一输入、不留临时文件）
#[test]
fn merge_stop_is_timely_inside_large_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // 40MB / 两千万行：行级处理足够慢，无文件内检查点时整个文件的合并远超等待窗口
    let mut big = String::with_capacity(40 * 1024 * 1024 + 16);
    while big.len() < 40 * 1024 * 1024 {
        big.push_str("x\n");
    }
    fs::write(root.join("big.md"), big).unwrap();
    write(&root.join("small.md"), "# 小\n");
    let output = root.join("merged.md");
    let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    assert_eq!(entries.len(), 2);
    let control = Arc::new(Control::default());
    let events: Arc<Mutex<Vec<MdProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let worker_control = Arc::clone(&control);
    let worker_events = Arc::clone(&events);
    let (result_tx, result_rx) = mpsc::channel();
    let worker_output = output.clone();
    std::thread::spawn(move || {
        let result =
            merge_markdown_with_events(&entries, &worker_output, true, &worker_control, &|event| {
                worker_events.lock().unwrap().push(event);
                if event == MdProgress::FileStarted(1, 2) {
                    // 合流点：文件内检查点应让任务立刻停在大文件开头附近
                    worker_control.pause(true);
                }
                Ok(())
            });
        let _ = result_tx.send(result.map(|_| ()).map_err(|error| format!("{error:#}")));
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let started = events
            .lock()
            .unwrap()
            .contains(&MdProgress::FileStarted(1, 2));
        if started {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 FileStarted(1,2) 超时"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // 等待窗口内任务必须停在文件内（未完成第 1 个文件）
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !events
            .lock()
            .unwrap()
            .contains(&MdProgress::FileCompleted(1, 2)),
        "暂停期间大文件不得被处理完成（文件内检查点缺失）"
    );
    control.cancel();
    let result = result_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("取消后任务必须及时收尾");
    let error = result.expect_err("取消中的合并必须失败收尾");
    assert!(error.contains("取消"), "取消必须以明确错误收尾：{error}");
    assert!(
        !events
            .lock()
            .unwrap()
            .contains(&MdProgress::FileStarted(2, 2)),
        "取消后不得开始下一个输入"
    );
    assert!(!output.exists(), "中止的合并不得留下半成品输出");
    for entry in fs::read_dir(root).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(".jchtools-md-out-"),
            "中止收尾不得残留临时文件：{name}"
        );
    }
}

// 覆盖 U-03/U-12（拆分的事件序列：Started → Completed 成对出现，完成计数由 Completed 驱动）
#[test]
fn split_events_started_before_completed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("doc.md"), "0123456789");
    let plan = plan_splits(&root.join("doc.md"), 5, &Control::default()).unwrap();
    assert_eq!(plan.bounds.len(), 2);
    let out_dir = root.join("parts");
    let completed = std::cell::Cell::new(0usize);
    let seen = std::cell::RefCell::new(Vec::new());
    run_split_with_events(
        &root.join("doc.md"),
        &plan,
        &out_dir,
        false,
        &Control::default(),
        &|event| {
            match event {
                MdProgress::FileStarted(index, total) => {
                    assert_eq!(total, 2);
                    assert_eq!(
                        index,
                        completed.get() + 1,
                        "FileStarted 发出时完成计数不得已包含当前分片"
                    );
                }
                MdProgress::FileCompleted(index, _total) => {
                    completed.set(index);
                }
            }
            seen.borrow_mut().push(event);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        seen.into_inner(),
        vec![
            MdProgress::FileStarted(1, 2),
            MdProgress::FileCompleted(1, 2),
            MdProgress::FileStarted(2, 2),
            MdProgress::FileCompleted(2, 2),
        ],
        "每个分片必须先 Started 后 Completed"
    );
    assert_eq!(completed.into_inner(), 2);
}

// 覆盖 U-12（拆分：第 1 片完成后取消，第 2 片不得开始）
#[test]
fn split_stop_between_parts_never_starts_next() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("doc.md"), "0123456789");
    let plan = plan_splits(&root.join("doc.md"), 5, &Control::default()).unwrap();
    assert_eq!(plan.bounds.len(), 2);
    let out_dir = root.join("parts");
    let control = Control::default();
    let seen = std::cell::RefCell::new(Vec::new());
    let error = run_split_with_events(
        &root.join("doc.md"),
        &plan,
        &out_dir,
        false,
        &control,
        &|event| {
            seen.borrow_mut().push(event);
            if event == MdProgress::FileCompleted(1, 2) {
                control.cancel();
            }
            Ok(())
        },
    )
    .expect_err("完成后取消必须中止任务");
    assert!(
        format!("{error:#}").contains("取消"),
        "取消必须以明确错误收尾：{error:#}"
    );
    assert!(
        !seen.borrow().contains(&MdProgress::FileStarted(2, 2)),
        "取消后不得开始下一分片：{:?}",
        seen.borrow()
    );
    assert!(
        !out_dir.join("doc_002.md").exists(),
        "取消后不得写出后续分片"
    );
}

// 覆盖 U-12（拆分的分片复制循环必须有停止检查点：暂停 rendezvous 证明任务停在
// 复制中途；取消后及时以错误收尾）
#[test]
fn split_stop_is_timely_inside_part_copy() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let big = vec![b'a'; 40 * 1024 * 1024];
    fs::write(root.join("big.md"), &big).unwrap();
    let plan = plan_splits(&root.join("big.md"), 64 * 1024 * 1024, &Control::default()).unwrap();
    assert_eq!(plan.bounds.len(), 1, "40MB 在 64MB 限制下应为单片");
    let out_dir = root.join("parts");
    let control = Arc::new(Control::default());
    let events: Arc<Mutex<Vec<MdProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let worker_control = Arc::clone(&control);
    let worker_events = Arc::clone(&events);
    let (result_tx, result_rx) = mpsc::channel();
    let input = root.join("big.md");
    std::thread::spawn(move || {
        let result =
            run_split_with_events(&input, &plan, &out_dir, false, &worker_control, &|event| {
                worker_events.lock().unwrap().push(event);
                if event == MdProgress::FileStarted(1, 1) {
                    // 合流点：分片复制循环内的检查点应让任务立刻停在开头
                    worker_control.pause(true);
                }
                Ok(())
            });
        let _ = result_tx.send(result.map(|_| ()).map_err(|error| format!("{error:#}")));
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let started = events
            .lock()
            .unwrap()
            .contains(&MdProgress::FileStarted(1, 1));
        if started {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 FileStarted(1,1) 超时"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !events
            .lock()
            .unwrap()
            .contains(&MdProgress::FileCompleted(1, 1)),
        "暂停期间单片复制不得完成（分片复制循环内检查点缺失）"
    );
    control.cancel();
    let result = result_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("取消后任务必须及时收尾");
    let error = result.expect_err("取消中的拆分必须失败收尾");
    assert!(error.contains("取消"), "取消必须以明确错误收尾：{error}");
}

// 覆盖 M-07/U-12：超大无换行正文仍须在文件内响应暂停/取消，
// 不能等整行读取、转换完并发出 FileCompleted 后才停止。
#[test]
fn merge_stop_is_timely_inside_single_long_line() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(&root.join("long.md"), &"x".repeat(2 * 1024 * 1024));
    let output = root.join("merged.md");
    let entries = scan_markdown(root, true, Some(&output), &Control::default()).unwrap();
    let control = Arc::new(Control::default());
    let worker_control = Arc::clone(&control);
    let worker_output = output.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (completed_tx, completed_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = merge_markdown_with_events(
            &entries,
            &worker_output,
            false,
            &worker_control,
            &|event| {
                match event {
                    MdProgress::FileStarted(..) => {
                        worker_control.pause(true);
                        started_tx.send(()).unwrap();
                    }
                    MdProgress::FileCompleted(..) => {
                        completed_tx.send(()).unwrap();
                    }
                }
                Ok(())
            },
        );
        result_tx
            .send(result.map_err(|error| format!("{error:#}")))
            .unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let premature_completion = completed_rx.recv_timeout(Duration::from_millis(400));
    control.cancel();
    let result = result_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    assert!(
        premature_completion.is_err(),
        "暂停期间不能把无换行的大文件处理完成"
    );
    assert!(result.unwrap_err().contains("取消"));
    assert!(!output.exists(), "取消不得落盘输出");
    assert_eq!(
        fs::read(root.join("long.md")).unwrap(),
        vec![b'x'; 2 * 1024 * 1024]
    );
    assert_eq!(
        fs::read_dir(root).unwrap().count(),
        1,
        "取消须清理自有临时文件"
    );
}
