use super::*;

// 覆盖 C-10（计划生成后目录被移动：执行必须拒绝，不得把动作落到别处）
#[test]
fn apply_refuses_when_root_directory_moved_after_plan() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let moved = f.temp.path().join("data-moved");
    fs::rename(&f.root, &moved).unwrap();
    let result = engine::apply(&task.directory, Context::default());
    let error = format!("{:#}", result.err().unwrap());
    assert!(
        error.contains("无法访问目标目录") || error.contains("目录位置已改变"),
        "执行前必须发现目录移位：{error}"
    );
    assert!(
        moved.join("a").exists() && moved.join("b").exists(),
        "新位置内容不得被触碰"
    );
}

// 覆盖 S-05（拒绝 Windows 安装目录；拒绝范围不扩大到 Program Files 或盘根）
#[cfg(windows)]
#[test]
fn normalize_root_rejects_protected_locations() {
    let system_root = std::env::var("SystemRoot").unwrap();
    assert!(
        fsutil::normalize_root(Path::new(&system_root)).is_err(),
        "不得整理 Windows 系统目录"
    );
    // S-05：拒绝系统目录的规则不自动扩大为拒绝 Program Files，仍受其余范围与权限规则约束。
    let program_files = std::env::var("ProgramFiles").unwrap();
    assert!(
        fsutil::normalize_root(Path::new(&program_files)).is_ok(),
        "Program Files 不属于 S-05 授权的扩大拒绝范围"
    );
    let volume_root = Path::new(&system_root).ancestors().last().unwrap();
    assert!(
        fsutil::normalize_root(volume_root).is_ok(),
        "S-05：盘根不属于自动扩大拒绝范围；扫描时另行排除系统目录"
    );
}
// 覆盖 S-05
#[cfg(not(windows))]
#[test]
fn normalize_root_rejects_root_directory() {
    assert!(
        fsutil::normalize_root(Path::new("/")).is_err(),
        "不得整理根目录"
    );
}

// 覆盖 S-06（摘要只按逻辑大小报告已永久删除的项，MUST NOT 声称等于文件系统实际释放量）
#[test]
fn summary_counts_permanent_bytes_only() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base()); // global_delete = Permanent
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.deleted, 1);
    assert_eq!(result.summary.permanent_bytes, 4, "「same」=4 字节");
}

// 覆盖 R-01（规则仅会话内生效：状态目录不得出现独立设置文件）
#[test]
fn prepare_writes_no_standalone_settings_file() {
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    let _ = f.plan(base());
    let entries: Vec<String> = fs::read_dir(&f.state)
        .unwrap()
        .map(|entry| entry.expect("无法读取状态目录项"))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries
            .iter()
            .all(|name| name == "tasks" || name == "organizer.lock" || name == "hash-cache.sqlite3"),
        "状态目录只应含任务库、锁文件与跨运行哈希缓存（C-13），不得出现独立设置文件（不落盘/无导入导出）：{entries:?}"
    );
}

#[test]
fn huge_sizes_use_u64() {
    assert_eq!(jchtools_file_core::model::bytes(10u64 << 40), "10.00 TiB");
    let f = Fixture::new();
    let db = Database::create(&f.state).unwrap();
    let snapshot = Snapshot {
        size: 12u64 << 40,
        modified_ns: 1,
        created_ns: None,
        identity: "mock".into(),
        links: 1,
    };
    db.insert_file("large.bin", "large.bin", "large.bin", &snapshot)
        .unwrap();
    assert_eq!(db.file(1).unwrap().snapshot.size, 12u64 << 40);
}

// 覆盖 C-01, C-04, S-04（分析只读；旧内部前缀不授予后续执行删除权限）
#[test]
fn analysis_phase_keeps_stale_link_temps_until_execution() {
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    let stale = f.root.join(".jchtools-link-deadbeef");
    if let Err(error) = fs::hard_link(f.root.join("a.txt"), &stale) {
        panic!("无法创建硬链接，无法验证分析只读与既有链接保护；请在支持硬链接的文件系统上运行测试：{error}");
    }
    filetime::set_file_mtime(&stale, filetime::FileTime::from_unix_time(0, 0)).unwrap();
    let task = f.plan(base());
    assert!(stale.exists(), "C-01：分析（只读）阶段不得删除任何文件");
    assert_eq!(task.summary.scanned, 2, "不能凭名称剪枝既有用户目录项");
    assert_eq!(task.summary.planned_delete, 0, "同物理文件不作副本删除");
    Fixture::apply(&task);
    assert_eq!(
        fs::read(f.root.join("其他/.jchtools-link-deadbeef")).unwrap(),
        b"payload"
    );
    assert_eq!(fs::read(f.root.join("文档/a.txt")).unwrap(), b"payload");
}

// 覆盖 C-04 / S-06：可靠硬链接的两个路径均受保护，即使独立副本较新；
// 独立重复组仍实际删除并按逻辑大小记账，不借链接保护取消无关删除。
#[test]
fn permanent_bytes_exclude_hardlinked_sources() {
    let f = Fixture::new();
    let a = f.write("a", b"same", 10);
    if let Err(error) = fs::hard_link(&a, f.root.join("c")) {
        panic!("无法创建硬链接：{error}；请在支持硬链接的文件系统上运行测试");
    }
    filetime::set_file_mtime(&a, filetime::FileTime::from_unix_time(10, 0)).unwrap();
    f.write("b", b"same", 20); // 独立的新保留者不能取消 a/c 的硬链接保护。
    f.write("d", b"independent", 10);
    f.write("e", b"independent", 20);
    let task = f.plan(base());
    let deletions = Database::open(&task.directory)
        .unwrap()
        .actions_page_filtered(0, 100, Some("delete"))
        .unwrap();
    assert_eq!(task.summary.planned_delete, 1, "仅独立重复组生成删除");
    assert_eq!(deletions.len(), 1);
    assert_eq!(deletions[0].source, "d", "a/c 两个路径均不得被判重删除");
    assert_eq!(task.summary.candidate_bytes, 11, "只计独立副本的逻辑大小");
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(result.summary.deleted, 1, "独立重复副本确实被删除");
    assert_eq!(
        result.summary.permanent_bytes, 11,
        "硬链接保护不影响独立删除记账"
    );
    for name in ["a", "b", "c"] {
        assert_eq!(fs::read(f.root.join("其他").join(name)).unwrap(), b"same");
    }
    let left = fsutil::snapshot(&f.root.join("其他/a")).unwrap();
    let right = fsutil::snapshot(&f.root.join("其他/c")).unwrap();
    assert_eq!(left.identity, right.identity, "归类不能拆散既有硬链接");
    assert!(left.links >= 2 && right.links >= 2);
    assert!(!exists_somewhere(&f.root, "d"));
    assert_eq!(fs::read(f.root.join("其他/e")).unwrap(), b"independent");
}

// 注：原 flatten_with_classification_is_idempotent_across_rounds 考的是已删除的
// flatten_single_child 归类形态（R-02）；归类目录内文件的跨轮稳定性由
// classify_shape.rs 的 in_place_normalized_collision_resolves_once_and_stays 等锚定。

// 覆盖 C-12, S-03（执行阶段 MUST NOT 读取文件内容：去重删除只依据分析期算出的整文件哈希）
#[test]
fn execution_phase_reads_no_file_content() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    assert_eq!(task.summary.planned_delete, 1, "同内容同尺寸应生成删除计划");
    let context = Context::default();
    let control = context.control.clone();
    let result = engine::apply(&task.directory, context).unwrap();
    assert_eq!(result.summary.deleted, 1);
    assert_eq!(
        control.read_bytes.load(Ordering::Relaxed),
        0,
        "执行阶段不得读取文件内容（C-12/S-03）：删除只依据分析期算出的整文件哈希"
    );
}
