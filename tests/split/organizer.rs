use super::*;
// 覆盖 C-01
#[test]
fn organizer_analysis_is_read_only_and_never_extracts() {
    // 回归（2026-09-18 合同重建）：旧目录整理在「分析」阶段就实际解压（旧 C-01），
    // 无引擎时整个分析以「解压引擎缺失」失败。新合同把解压职责整体移交「递归解压」
    // 工具：目录整理的分析阶段必须只读——没有引擎也要成功生成计划，压缩包原样保留。
    let tmp = fixture("readonly");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("a.zip"), b"pretend archive bytes", 100);
    write_with_mtime(&root.join("b.txt"), b"loose file", 200);
    let cfg = Config::default(); // 组织器默认：不含解压
    let result = engine::prepare_at(&root, cfg, Context::default(), &state_of(&tmp)).unwrap();
    assert_eq!(result.summary.archives_ok, 0, "目录整理不得解压任何压缩包");
    assert_eq!(result.summary.archives_failed, 0);
    assert!(root.join("a.zip").exists(), "分析阶段不得动压缩包（只读）");
    // 压缩包作为普通文件参与计划（这里是唯一文件，无重复对，但必须被扫描到）
    assert_eq!(result.summary.scanned, 2, "压缩包与散文件都应被扫描");
    assert_eq!(
        fs::read(root.join("a.zip")).unwrap(),
        b"pretend archive bytes"
    );
    assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"loose file");
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        2,
        "只读分析不得新建、移动或删除待整理目录中的条目（C-01）"
    );
}

// 覆盖 C-13：三要素（文件标识/大小/修改时间）未变时跨运行复用哈希
#[test]
fn hash_cache_reuses_unchanged_files_across_runs() {
    // 反证构造：第一次分析后把副本原地改写成同长度的不同字节并还原修改时间——
    // 若第二次分析重新计算哈希，两文件内容不同、不再判重复；实测仍判重复，
    // 证明走的是缓存复用（C-13 的「三者未变即内容未变」信任假设）。
    let tmp = fixture("hashcache");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("a.txt"), b"same-bytes", 100);
    write_with_mtime(&root.join("sub").join("a.txt"), b"same-bytes", 100);
    let state = state_of(&tmp);
    let first = engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    assert_eq!(
        first.summary.planned_delete, 1,
        "一对真重复应生成一条删除计划"
    );
    let copy = root.join("sub").join("a.txt");
    let stamp = filetime::FileTime::from_last_modification_time(&fs::metadata(&copy).unwrap());
    fs::write(&copy, b"DIFF-BYTES").unwrap();
    filetime::set_file_mtime(&copy, stamp).unwrap();
    let second = engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    assert_eq!(
        second.summary.planned_delete, 1,
        "C-13：三要素未变时复用旧哈希——改写后（同长不同内容、还原时间戳）仍按缓存判重复"
    );
}

// 覆盖 C-13：大小变化必须重算，不得复用旧哈希
#[test]
fn hash_cache_recomputes_when_size_changes() {
    let tmp = fixture("hashcache2");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("a.txt"), b"same-bytes", 100);
    write_with_mtime(&root.join("sub").join("a.txt"), b"same-bytes", 100);
    let state = state_of(&tmp);
    let first = engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    assert_eq!(first.summary.planned_delete, 1);
    let copy = root.join("sub").join("a.txt");
    let stamp = filetime::FileTime::from_last_modification_time(&fs::metadata(&copy).unwrap());
    fs::write(&copy, b"same-bytes!").unwrap();
    // C-13：保留修改时间，使缓存失效只能由大小变化触发。
    filetime::set_file_mtime(&copy, stamp).unwrap();
    let second = engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    assert_eq!(
        second.summary.planned_delete, 0,
        "大小变化不在缓存键上，必须完整重算并发现内容已不同"
    );
}

// 覆盖 C-09
#[test]
fn organizer_scan_extracts_nothing_and_skips_failed_dir() {
    // 「解压失败」子目录默认整体排除（C-09）：里面的文件不参与去重/归类/清理。
    // 树里 root/dup.txt 与 root/解压失败/dup.txt 内容相同：若失败目录未被排除，
    // 去重会生成一条删除计划；排除后 root/dup.txt 是唯一候选，无删除计划。
    let tmp = fixture("skipdir");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("dup.txt"), b"same", 100);
    write_with_mtime(&root.join("解压失败").join("dup.txt"), b"same", 200);
    let result = engine::prepare_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
    )
    .unwrap();
    assert_eq!(
        result.summary.planned_delete, 0,
        "「解压失败」目录内的文件不得参与去重（C-09 默认排除）"
    );
    assert_eq!(
        result.summary.scanned, 1,
        "只有失败目录之外的一个文件进入扫描"
    );
    assert!(
        root.join("解压失败").join("dup.txt").exists(),
        "失败目录内容保持原样"
    );
}
