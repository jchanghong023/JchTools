use super::*;

// 覆盖 C-01（多类清理规则同时命中：任一已启用规则请求永久删除即形成一次清理删除，
// 某类选择保留只取消该类的删除请求、不否决其他类别；计划列出实际导致删除的规则）
#[test]
fn multi_hit_cleanup_any_permanent_rule_deletes() {
    let f = Fixture::new();
    // `._x.tmp` 同时命中垃圾清理（`._` 前缀）与临时/备份清理（.tmp 扩展名）。
    f.write("._x.tmp", b"junk-temp", 10);
    let mut cfg = base();
    cfg.clean_junk = true;
    cfg.clean_temp = true;
    // 垃圾清理显式保留、临时清理跟随全局永久删除：首命中类别（Junk）的保留
    // 不得否决 Temp 的删除请求（C-01）。
    cfg.set_json("junk_delete", serde_json::json!("keep"))
        .unwrap();
    let task = f.plan(cfg);
    assert_eq!(task.summary.planned_delete, 1, "多规则命中只形成一次删除");
    let actions = Database::open(&task.directory)
        .unwrap()
        .actions_page(0, 100)
        .unwrap();
    let deletion = actions
        .iter()
        .find(|a| matches!(a.kind, ActionKind::Delete))
        .expect("Temp 类请求永久删除即形成一次清理删除");
    assert_eq!(deletion.source, "._x.tmp");
    assert!(
        deletion.reason.contains("临时/备份文件规则"),
        "列出实际导致删除的规则：{}",
        deletion.reason
    );
    assert!(
        !deletion.reason.contains("垃圾文件规则"),
        "选择了保留的规则不得列入删除原因：{}",
        deletion.reason
    );
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.deleted, 1, "实际删除也只记一次");
    assert!(
        !exists_somewhere(&f.root, "._x.tmp"),
        "任一 Permanent 类请求删除即删除且只删除一次"
    );
}
// 覆盖 C-01（取消勾选后仅用既有分析资料重算受影响的计划：
// 依赖已取消移动的空目录清理项禁止删除，重算转为未勾选；不新增用户勾选）
#[test]
fn unselecting_move_recomputes_dependent_empty_directory_rows() {
    let f = Fixture::new();
    f.write("a/b/x.txt", b"x", 10);
    let task = f.plan(base());
    let db = Database::open(&task.directory).unwrap();
    let actions = db.actions_page(0, 100).unwrap();
    let move_id = actions
        .iter()
        .find(|a| matches!(a.kind, ActionKind::Move))
        .expect("存在归类移动行")
        .id;
    let empty_rows: Vec<(i64, String)> = actions
        .iter()
        .filter(|a| matches!(a.kind, ActionKind::EmptyDirectory))
        .map(|a| (a.id, a.source.clone()))
        .collect();
    assert!(!empty_rows.is_empty(), "a/b 与 a 的空目录清理行入计划");
    db.set_selected(move_id, false).unwrap();
    drop(db);
    let cancelled = engine::recompute_plan(&task.directory).unwrap();
    assert_eq!(
        cancelled,
        empty_rows.len(),
        "依赖失效的空目录行全部重算取消"
    );
    let db = Database::open(&task.directory).unwrap();
    for (id, rel) in empty_rows {
        let row = db
            .actions_page(0, 100)
            .unwrap()
            .into_iter()
            .find(|a| a.id == id)
            .unwrap_or_else(|| panic!("行 {rel} 仍在计划中"));
        assert!(!row.selected, "空目录行 {rel} 转为未勾选");
    }
    // 幂等且不新增勾选：再次重算无可取消项。
    assert_eq!(engine::recompute_plan(&task.directory).unwrap(), 0);
}
// 覆盖 C-08
#[test]
fn cleanup_does_not_become_only_dedup_keeper() {
    let f = Fixture::new();
    f.write("keep.txt", b"content", 10);
    f.write("temporary.tmp", b"content", 20);
    let mut cfg = base();
    cfg.clean_temp = true;
    let task = f.plan(cfg);
    Fixture::apply(&task);
    assert!(exists_somewhere(&f.root, "keep.txt"));
    assert!(!exists_somewhere(&f.root, "temporary.tmp"));
}
// 覆盖 C-01（清理选择保留的文件仍可参加去重：与同内容副本一起按 C-03 决胜；
// 「已确定永久删除的清理项不再参与去重选保留者」只排除 Permanent 项，
// 无「清理保留豁免去重删除」的合同外保护）
#[test]
fn cleanup_keep_files_still_join_dedup() {
    let f = Fixture::new();
    f.write("normal.txt", b"payload", 20); // 较新 → C-03 默认保留最新者
    f.write("junk.tmp", b"payload", 10); // 较旧且命中 clean_temp，显式覆盖为保留
    let mut cfg = base();
    cfg.clean_temp = true;
    // C-08：临时项清理的删除方式按类别独立覆盖（不再有单一 cleanup_delete 开关）。
    cfg.set_json("temp_delete", serde_json::json!("keep"))
        .unwrap();
    let task = f.plan(cfg);
    let actions = Database::open(&task.directory)
        .unwrap()
        .actions_page(0, 100)
        .unwrap();
    let deletion = actions
        .iter()
        .find(|a| matches!(a.kind, ActionKind::Delete))
        .expect("清理保留的副本按 C-03 决胜仍生成去重删除");
    assert_eq!(
        deletion.source, "junk.tmp",
        "较旧的清理保留文件按重复规则删除：{deletion:?}"
    );
    Fixture::apply(&task);
    assert!(
        exists_somewhere(&f.root, "normal.txt"),
        "较新的同内容文件保留"
    );
    assert!(
        !exists_somewhere(&f.root, "junk.tmp"),
        "较旧的清理保留副本已删除"
    );
}
// 覆盖 C-04, S-04（旧内部前缀不是用户硬链接的删除授权）
#[test]
fn stale_hardlink_temps_are_swept() {
    // 保留历史用例名；C-04 已取消硬链接替换模式。仅名称和 links >= 2
    // 不能证明本次所有权，两个既有目录项均应扫描、归类并保留。
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    let stale = f.root.join(".jchtools-link-deadbeef");
    if let Err(error) = fs::hard_link(f.root.join("a.txt"), &stale) {
        eprintln!(
            "hard_link failed: {error}; keeper={:?}",
            f.root.join("a.txt")
        );
        panic!("无法创建硬链接，无法验证既有链接保护；请在支持硬链接的文件系统上运行测试");
    }
    filetime::set_file_mtime(&stale, filetime::FileTime::from_unix_time(0, 0)).unwrap();
    let task = f.plan(base());
    assert!(stale.exists(), "C-01：分析阶段不得删除用户目录项（只读）");
    Fixture::apply(&task);
    assert_eq!(task.summary.scanned, 2, "两个用户目录项均属于默认扫描范围");
    assert_eq!(task.summary.planned_delete, 0, "同物理文件不作副本删除");
    assert_eq!(
        fs::read(f.root.join("其他/.jchtools-link-deadbeef")).unwrap(),
        b"payload"
    );
    assert_eq!(fs::read(f.root.join("文档/a.txt")).unwrap(), b"payload");
}
// 覆盖 C-04（残留清扫不得误删用户文件）
#[test]
fn user_file_with_link_temp_prefix_is_never_swept() {
    // 旧内部前缀不是删除授权；普通同名用户文件默认照常扫描及归类（S-04/C-05）。
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    let user = f.root.join(".jchtools-link-mydata");
    fs::write(&user, b"precious").unwrap();
    filetime::set_file_mtime(&user, filetime::FileTime::from_unix_time(0, 0)).unwrap();
    let task = f.plan(base());
    assert!(user.exists(), "C-01：分析（只读）阶段不得删除用户文件");
    assert_eq!(task.summary.scanned, 2);
    assert_eq!(task.summary.planned_delete, 0);
    Fixture::apply(&task);
    assert_eq!(
        fs::read(f.root.join("其他/.jchtools-link-mydata")).unwrap(),
        b"precious",
        "用户文件归类后内容必须完整保留，不得按旧内部前缀清扫"
    );
}
// 覆盖 R-02（删除方式可按类覆盖）
#[test]
fn global_keep_prohibits_deletion() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let mut cfg = base();
    cfg.global_delete = DeleteMode::Keep;
    assert_eq!(f.plan(cfg).summary.planned_delete, 0);
}
// 注：原 class_override_beats_global_keep 依赖已删除的 duplicate_delete 覆盖键
//（C-04/S-02：副本处置随全局文件删除方式，无独立覆盖），“类别覆盖压过全局保留”
// 的行为由 cleanup_categories_override_delete_mode_independently 锚定。
// 注：原 copy_name_cleanup_uses_freed_original_name、classification_preserves_paths_and_is_idempotent、
// flatten_classification_allocates_nonconflicting_names 考的是已删除的
// “保留原路径 / 扁平化 / (N) 序号回退”归类形态；新归类（C-05/C-16/C-17）由
// classify_shape.rs 与 copy_name_cleanup_never_plans_self_move 等锚定。

// 覆盖 C-08（系统附属文件清理默认开启；__MACOSX 目录内容一并清理）
#[test]
fn junk_system_files_are_planned_for_cleanup_by_default() {
    let f = Fixture::new();
    for name in ["Thumbs.db", "desktop.ini", "keep.txt"] {
        f.write(name, b"x", 10);
    }
    fs::create_dir_all(f.root.join("__MACOSX")).unwrap();
    f.write("__MACOSX/junk.dat", b"x", 10);
    let cfg = base(); // clean_junk 默认 true
    let task = f.plan(cfg);
    assert_eq!(
        task.summary.planned_delete, 3,
        "Thumbs.db / desktop.ini / __MACOSX 内容应入清理计划"
    );
    Fixture::apply(&task);
    assert!(
        exists_somewhere(&f.root, "keep.txt"),
        "普通文件不受清理影响"
    );
    assert!(
        !f.root.join("Thumbs.db").exists() && !f.root.join("__MACOSX/junk.dat").exists(),
        "系统附属文件应被清理"
    );
}

// 覆盖 C-08（零字节文件清理默认关，开启后入计划）
#[test]
fn zero_byte_cleanup_follows_its_switch() {
    let f = Fixture::new();
    f.write("empty.dat", b"", 10);
    f.write("full.dat", b"x", 10);
    let mut cfg = base();
    assert_eq!(
        f.plan(cfg.clone()).summary.planned_delete,
        0,
        "默认关闭：零字节文件不得入清理计划"
    );
    cfg.clean_zero = true;
    assert_eq!(f.plan(cfg).summary.planned_delete, 1);
}

// 覆盖 C-08（NFC 规范化 + 合并连续空白，默认开；不删正常字符）
#[test]
fn normalize_names_collapses_whitespace_and_applies_nfc() {
    let f = Fixture::new();
    // U+0065 + U+0301（分解形式）应规范为 U+00E9（合成形式）；双空格合并为单空格、
    // 主体首尾空白去除（扩展名前的空格属于主体尾部，一并去掉）。
    let raw_name = "cafe\u{0301}  report .txt";
    f.write(raw_name, b"payload", 10);
    let cfg = base();
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    let expected = "文档/café report.txt".to_string();
    assert_eq!(moves.len(), 1, "规范化默认开启，改名应入移动计划");
    assert_eq!(moves[0].target.as_deref(), Some(expected.as_str()));
    Fixture::apply(&task);
    assert!(
        f.root.join(&expected).exists(),
        "执行后应为规范化名（并按 C-05 归类落位）"
    );
    assert!(!f.root.join(raw_name).exists(), "旧分解形式名称不得残留");
}

// ===== 本轮合同对齐回归（先红后绿：空目录强制清理 / Git 整树 / 自嵌套 / 复合扩展名 / 分类删除方式）=====

// 覆盖 C-07（空目录清理是强制步骤：不受全局/清理删除方式影响，也没有开关）
#[test]
fn empty_cleanup_ignores_file_delete_modes() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("empty")).unwrap();
    f.write("keep.txt", b"payload", 10);
    let mut cfg = base();
    cfg.global_delete = DeleteMode::Keep;
    let task = f.plan(cfg);
    assert_eq!(
        task.summary.planned_empty, 1,
        "空目录清理不属于文件删除方式可关的项目（C-07）"
    );
    Fixture::apply(&task);
    assert!(
        !f.root.join("empty").exists(),
        "全局保留也不得阻止空目录清理"
    );
    assert!(exists_somewhere(&f.root, "keep.txt"));
}

// 覆盖 C-07, R-03, H-05（递归关闭：未纳入的子目录不参与空目录清理）
#[test]
fn recursion_disabled_never_plans_empty_subdirectories() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("empty")).unwrap();
    f.write("full/x.txt", b"payload", 10);
    let mut cfg = base();
    cfg.recursive = false;
    let task = f.plan(cfg);
    assert_eq!(task.summary.scanned, 0, "递归关闭：只处理根目录自身的条目");
    assert_eq!(
        task.summary.planned_empty, 0,
        "未下钻的子目录内容未知，不得把猜测写成计划行"
    );
    Fixture::apply(&task);
    assert!(
        f.root.join("empty").is_dir(),
        "H-05：关闭递归后未纳入的后代不参与空目录清理"
    );
    assert!(
        f.root.join("full/x.txt").exists() && f.root.join("full").exists(),
        "有内容的子目录及其内容不得被触碰"
    );
}

// 覆盖 H-05 / S-01 / R-03：移动失败后只回收本次实际新建的空目标目录。
// Windows 共享模式拒绝 DELETE，确定性模拟移动失败；不改变扫描后的数据。
#[cfg(windows)]
#[test]
fn nonrecursive_cleanup_keeps_old_directories_and_removes_only_created_targets() {
    use std::os::windows::fs::OpenOptionsExt;

    for existing_target in [false, true] {
        let f = Fixture::new();
        let source = f.write("locked.txt", b"original bytes", 10);
        fs::create_dir(f.root.join("old-empty")).unwrap();
        if existing_target {
            fs::create_dir(f.root.join("文档")).unwrap();
        }
        let mut cfg = base();
        cfg.recursive = false;
        let task = f.plan(cfg);
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&source)
            .unwrap();
        let result = engine::apply(&task.directory, Context::default()).unwrap();
        drop(held);
        assert!(
            result.summary.errors > 0,
            "被占用的源无法移动，不能假报执行成功"
        );
        assert_eq!(fs::read(&source).unwrap(), b"original bytes");
        assert!(f.root.join("old-empty").is_dir(), "未纳入旧目录不参与收尾");
        assert_eq!(
            f.root.join("文档").is_dir(),
            existing_target,
            "仅 fs::create_dir 成功的本轮目标容器参与非递归收尾"
        );
    }
}

// 覆盖 C-07, S-04（排除树不参与空目录清理）
#[test]
fn excluded_tree_is_not_cleaned_as_empty() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("protected/nested")).unwrap();
    f.write("keep.txt", b"payload", 10);
    let mut cfg = base();
    cfg.exclusions = "protected".into();
    let task = f.plan(cfg);
    assert_eq!(task.summary.scanned, 1);
    assert_eq!(task.summary.planned_empty, 0, "排除树不得进入空目录计划");
    Fixture::apply(&task);
    assert!(f.root.join("protected/nested").exists());
    assert!(exists_somewhere(&f.root, "keep.txt"));
}
// 注：原 category_merge_does_not_nest_category_directory_inside_itself、
// category_merge_flatten_rerun_is_idempotent 与 flatten_allocation_preserves_compound_extension
// 考的是已删除的 merge_directories / flatten_single_child / (N) 序号回退归类形态（R-02）；
// 同名冲突的复合扩展名保留由 rules::digest_candidate 与 classify_shape.rs 锚定，
// “来源目录名与大类同名不再套层”由 C-18 的来源链剔除规则覆盖。

// 覆盖 C-08（三类清理各自独立覆盖删除方式；与全局方式解耦）
#[test]
fn cleanup_categories_override_delete_mode_independently() {
    let f = Fixture::new();
    f.write("Thumbs.db", b"junk", 10);
    f.write("note.tmp", b"temp", 11);
    f.write("empty.dat", b"", 12);
    f.write("keep.txt", b"payload", 13);
    let mut cfg = base();
    cfg.clean_temp = true;
    cfg.clean_zero = true;
    // C-08 新增的三类独立覆盖键；未显式覆盖的类别跟随全局文件删除方式。
    cfg.set_json("junk_delete", serde_json::json!("keep"))
        .unwrap();
    cfg.set_json("zero_delete", serde_json::json!("keep"))
        .unwrap();
    let task = f.plan(cfg);
    assert_eq!(
        task.summary.planned_delete, 1,
        "只有跟随全局的临时项入删除计划"
    );
    Fixture::apply(&task);
    assert!(
        exists_somewhere(&f.root, "Thumbs.db"),
        "junk_delete=keep 不得删除"
    );
    assert!(
        !exists_somewhere(&f.root, "note.tmp"),
        "临时项默认跟随全局永久删除"
    );
    assert!(
        exists_somewhere(&f.root, "empty.dat"),
        "zero_delete=keep 不得删除"
    );
    assert!(exists_somewhere(&f.root, "keep.txt"));

    // 全局保留 + 临时项显式永久删除：按类别覆盖必须能压过全局方式。
    let g = Fixture::new();
    g.write("note.tmp", b"temp", 11);
    g.write("keep.txt", b"payload", 13);
    let mut cfg = base();
    cfg.global_delete = DeleteMode::Keep;
    cfg.clean_temp = true;
    cfg.set_json("temp_delete", serde_json::json!("permanent"))
        .unwrap();
    Fixture::apply(&g.plan(cfg));
    assert!(
        !exists_somewhere(&g.root, "note.tmp"),
        "类别覆盖压过全局保留"
    );
    assert!(exists_somewhere(&g.root, "keep.txt"));
}
