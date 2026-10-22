use super::*;

// 覆盖 C-01, C-03
#[test]
fn analysis_no_changes_and_dedup_keeps_latest() {
    let f = Fixture::new();
    let old = f.write("old.txt", b"same", 10);
    let new = f.write("new.txt", b"same", 20);
    let task = f.plan(base());
    assert!(old.exists() && new.exists());
    assert_eq!(task.summary.planned_delete, 1);
    Fixture::apply(&task);
    assert!(!old.exists());
    assert!(
        exists_somewhere(&f.root, "new.txt"),
        "保留者随固定归类移动（C-05 恒开启），内容仍在"
    );
}
// 覆盖 R-02
#[test]
fn different_names_can_be_disabled() {
    let f = Fixture::new();
    f.write("a.txt", b"same", 10);
    f.write("b.txt", b"same", 20);
    let mut cfg = base();
    cfg.dedup_other_names = false;
    assert_eq!(f.plan(cfg).summary.planned_delete, 0);
}
// 覆盖 R-02
#[test]
fn same_names_different_directories_can_be_disabled() {
    let f = Fixture::new();
    f.write("a/test.txt", b"same", 10);
    f.write("b/test.txt", b"same", 20);
    let mut cfg = base();
    cfg.dedup_same_name = false;
    // 关闭不同名关系，避免 base() 的全尺寸候选覆盖被测同名开关（C-02）。
    cfg.dedup_other_names = false;
    assert_eq!(f.plan(cfg).summary.planned_delete, 0);
}
// 覆盖 R-02
#[test]
fn copy_names_can_be_disabled_independently() {
    let f = Fixture::new();
    f.write("a.txt", b"same", 10);
    f.write("a (1).txt", b"same", 20);
    let mut cfg = base();
    cfg.dedup_copy_names = false;
    // 关闭不同名关系，避免 base() 的全尺寸候选覆盖被测副本名开关（C-02）。
    cfg.dedup_other_names = false;
    assert_eq!(f.plan(cfg).summary.planned_delete, 0);
}
// 覆盖 H-01, C-02, C-12（只哈希启用的名称关系候选，避免读取关闭规则的文件）
#[test]
fn hash_candidates_respect_enabled_name_relationships() {
    let f = Fixture::new();
    f.write("same-a/report.txt", b"same", 10);
    f.write("same-b/report.txt", b"same", 20);
    f.write("copy-a/draft.txt", b"copy", 30);
    f.write("copy-b/draft (1).txt", b"copy", 40);
    let cfg = Config {
        dedup_same_name: true,
        dedup_copy_names: false,
        dedup_other_names: false,
        ..base()
    };
    let context = Context::default();
    let control = context.control.clone();
    let task = engine::prepare_at(&f.root, cfg, context, &f.state).unwrap();
    assert_eq!(task.summary.planned_delete, 1, "仅相同名称关系参与去重");
    assert_eq!(
        control.read_bytes.load(Ordering::Relaxed),
        8,
        "关闭副本名去重后，不应读取仅匹配副本关系的文件内容"
    );
}

// P3-07 / H-01, C-02, C-12：纯同名桶没有副本名边，prepare 不应读取内容；
// 每次使用独立状态，避免哈希缓存使无用读取回归被掩盖。
fn assert_copy_name_only_bucket_skips_hash(left_name: &str, right_name: &str) {
    let f = Fixture::new();
    f.write(&format!("a/{left_name}"), b"same", 10);
    f.write(&format!("b/{right_name}"), b"same", 20);
    let cfg = Config {
        dedup_same_name: false,
        dedup_copy_names: true,
        dedup_other_names: false,
        ..base()
    };
    let context = Context::default();
    let control = context.control.clone();
    let task = engine::prepare_at(&f.root, cfg, context, &f.state).unwrap();
    let read_bytes = control.read_bytes.load(Ordering::Relaxed);
    assert_eq!(task.summary.errors, 0);
    assert_eq!(task.summary.planned_delete, 0, "纯同名桶没有启用的去重关系");
    let applied = Fixture::apply(&task);
    assert_eq!(applied.summary.errors, 0);
    let contents: Vec<Vec<u8>> = fs::read_dir(f.root.join("文档"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            assert!(entry.file_type().unwrap().is_file());
            fs::read(entry.path()).unwrap()
        })
        .collect();
    assert_eq!(
        contents,
        vec![b"same".to_vec(), b"same".to_vec()],
        "归类及名称冲突消解后必须仍有两份完整内容"
    );
    assert_eq!(read_bytes, 0, "纯同名桶不可能建立副本名边，不得整文件哈希");
}

// P3-07：不同目录中完全同名的文件不属于副本名候选。
#[test]
fn review_remediation_copy_name_only_skips_pure_same_name_bucket() {
    assert_copy_name_only_bucket_skips_hash("report.txt", "report.txt");
}

// P3-07：两份名称包含相同副本标记，仍是纯同名桶。
#[test]
fn review_remediation_copy_name_only_skips_identical_copy_marker_names() {
    assert_copy_name_only_bucket_skips_hash("report (1).txt", "report (1).txt");
}

// P3-07 / 附录 B：Windows 序数等价的 ASCII 大小写名称也是纯同名桶。
#[cfg(windows)]
#[test]
fn review_remediation_copy_name_only_skips_windows_ascii_equivalent_names() {
    assert_copy_name_only_bucket_skips_hash("Report.txt", "REPORT.TXT");
}

// P3-07 / 附录 B：Windows 序数键将希腊 sigma 的两种形式视为同名。
#[cfg(windows)]
#[test]
fn review_remediation_copy_name_only_skips_windows_sigma_equivalent_names() {
    assert_copy_name_only_bucket_skips_hash("Σ.txt", "ς.txt");
}

// 覆盖 C-02, C-12（仅副本名关系仍须保留同名节点作为连通组桥接项）
#[test]
fn copy_name_only_hashing_keeps_same_name_bridge_candidates() {
    let f = Fixture::new();
    f.write("a/report.txt", b"same", 10);
    f.write("b/report.txt", b"same", 20);
    f.write("c/report (1).txt", b"same", 30);
    let cfg = Config {
        dedup_same_name: false,
        dedup_copy_names: true,
        dedup_other_names: false,
        ..base()
    };
    let context = Context::default();
    let control = context.control.clone();
    let task = engine::prepare_at(&f.root, cfg, context, &f.state).unwrap();
    assert_eq!(
        task.summary.planned_delete, 2,
        "两份同名文件经副本名边连接后仍属于同一候选连通组"
    );
    assert_eq!(
        control.read_bytes.load(Ordering::Relaxed),
        12,
        "副本名候选连通组的所有桥接节点都需参与哈希"
    );
}
// 覆盖 C-02, S-03
#[test]
fn equal_size_different_hash_not_deleted_when_rule_off() {
    let f = Fixture::new();
    f.write("a/report.txt", b"abcd", 10);
    f.write("b/report.txt", b"efgh", 20);
    assert_eq!(f.plan(base()).summary.planned_delete, 0);
}

// 覆盖 C-03
#[test]
fn deterministic_keeper_ties() {
    let record = |id, rel: &str| FileRecord {
        id,
        rel: rel.into(),
        name: "x".into(),
        normalized: "x".into(),
        snapshot: Snapshot {
            size: 1,
            modified_ns: 10,
            created_ns: None,
            identity: id.to_string(),
            links: 1,
        },
        hash: None,
        cleanable: false,
    };
    assert!(rules::compare(&record(1, "a/x"), &record(2, "b/x"), KeepPolicy::Newest).is_lt());
}

// ===== 合同覆盖补齐（AGENTS 3.3：每个合同条目至少被一个测试引用）=====

// 覆盖 C-03, R-02, S-08（保留规则各可选项的实际选择结果与决胜链）
#[test]
fn keeper_policy_variants_pick_expected_keepers() {
    // C-03（R-02 后）：只有最新/最旧/最短名称三种；同内容必然等大小，
    // 不再提供最大/最小（Largest/Smallest 已随硬链接模式删除）。
    for (policy, kept, deleted) in [
        (KeepPolicy::Oldest, "aa.txt", "zzzz.txt"),
        (KeepPolicy::Newest, "zzzz.txt", "aa.txt"),
        (KeepPolicy::ShortestName, "aa.txt", "zzzz.txt"),
    ] {
        let f = Fixture::new();
        f.write("aa.txt", b"same", 100);
        f.write("zzzz.txt", b"same", 200);
        let mut cfg = base();
        cfg.keep_duplicate = policy;
        let task = f.plan(cfg);
        assert_eq!(task.summary.planned_delete, 1, "{policy:?}");
        let db = Database::open(&task.directory).unwrap();
        let actions = db.actions_page(0, 10).unwrap();
        drop(db);
        assert_eq!(actions[0].source, deleted, "{policy:?} 应删除 {deleted}");
        assert!(
            actions[0].keeper.as_ref().is_some_and(|k| k.0 == kept),
            "{policy:?} 应保留 {kept}"
        );
        Fixture::apply(&task);
        assert!(
            exists_somewhere(&f.root, kept) && !exists_somewhere(&f.root, deleted),
            "{policy:?} 执行结果与计划一致"
        );
    }
}

// 注：原 hardlink_does_not_claim_permanent_bytes 与 hardlink_mode_preserves_aliases
// 考的是已按 R-02 整体删除的硬链接替换模式（config 无 duplicate_action、
// Summary 无 planned_link/linked）；多硬链接源的记账由
// permanent_bytes_exclude_hardlinked_sources 与 existing_hardlinks_not_counted_twice 锚定。

// 覆盖 C-04, S-06（已是硬链接的文件不重复计入逻辑大小）
#[test]
fn existing_hardlinks_not_counted_twice() {
    let f = Fixture::new();
    let a = f.write("a", b"same", 10);
    if let Err(error) = fs::hard_link(&a, f.root.join("b")) {
        // 静默 return 会让测试永远绿：硬链接失败必须显式失败（FAT32/exFAT 不支持时请换 NTFS/tmpfs 环境）。
        eprintln!("hard_link failed: {error}; a={a:?}; root={:?}", f.root);
        panic!("无法创建硬链接，无法验证去重行为；请在支持硬链接的文件系统上运行测试");
    }
    let task = f.plan(base());
    assert_eq!(task.summary.candidate_bytes, 0);
    assert_eq!(task.summary.planned_delete, 0);
    // C-04：可靠标识 + 链接数证明已是同一物理文件，两个目录项都保留且不重复计数
    //（R-02 已删除硬链接替换模式，同物理文件的去重豁免是仅存的硬链接感知行为）。
    Fixture::apply(&task);
    assert!(
        exists_somewhere(&f.root, "a") && exists_somewhere(&f.root, "b"),
        "同一物理文件的两个目录项都不得被删除"
    );
}

// 覆盖 C-02（不同名去重默认关闭：不同名的同内容文件默认不得被自动淘汰，可手动开启）
#[test]
fn different_names_not_deduped_by_default() {
    let cfg = Config {
        clean_copy_name: false,
        global_delete: DeleteMode::Permanent,
        ..Config::default()
    };
    assert!(!cfg.dedup_other_names, "C-02：不同名去重默认关闭");
    let f = Fixture::new();
    f.write("a.txt", b"same", 10);
    f.write("z.txt", b"same", 20);
    let task = f.plan(cfg.clone());
    assert_eq!(
        task.summary.planned_delete, 0,
        "默认配置下不同名的同内容文件不得被自动淘汰（C-02）"
    );
    // 手动开启后照旧生效：同一份数据必须产出删除计划。
    let mut on = cfg;
    on.dedup_other_names = true;
    let f2 = Fixture::new();
    f2.write("a.txt", b"same", 10);
    f2.write("z.txt", b"same", 20);
    assert_eq!(
        f2.plan(on).summary.planned_delete,
        1,
        "开启不同名去重后应生成删除计划"
    );
}
