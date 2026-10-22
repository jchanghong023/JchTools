use super::*;

/// events 表的测试观察入口：生产只写不读（读取 API 已随死代码清理移除），
/// 行格式与写入列序一致：`time | phase | result | source | target | reason`。
fn event_lines(db: &Database, limit: usize) -> Vec<String> {
    let mut statement = db
        .conn
        .prepare(
            "SELECT time,phase,result,source,target,reason FROM events ORDER BY id DESC LIMIT ?1",
        )
        .unwrap();
    let rows = statement
        .query_map([i64::try_from(limit).unwrap()], |row| {
            let values = (0..6)
                .map(|i| row.get::<_, String>(i))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(values.join(" | "))
        })
        .unwrap();
    rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

// 覆盖 C-07
#[test]
fn empty_directory_cleanup_is_bottom_up() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("empty/nested")).unwrap();
    let cfg = base();
    let task = f.plan(cfg);
    Fixture::apply(&task);
    assert!(!f.root.join("empty").exists());
    assert!(f.root.exists());
}
// 覆盖 C-07, S-04
#[test]
fn empty_hidden_subdir_blocks_empty_directory_cleanup() {
    // 行为锚点：用户显式关闭隐藏项后，仅含一个空的隐藏（Windows）/点开头（Unix）子目录的
    // 目录，该子目录不会入库，目录对规划"并非实际为空"，不得计划为空目录。
    // 否则计划承诺落空：执行期实空复查只能跳过，skipped 虚增、该清的没清。
    // （默认 include_hidden=true 时该子目录会被扫描，属 S-04 的默认口径，见 hidden_dir_scanned_by_default。）
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("outer/.inner")).unwrap();
    #[cfg(windows)]
    set_hidden(&f.root.join("outer/.inner"), true);
    f.write("keep.txt", b"payload", 10);
    let mut cfg = base();
    cfg.include_hidden = false;
    let task = f.plan(cfg);
    assert_eq!(
        task.summary.planned_empty, 0,
        "含未入库子目录的目录不得计划为空目录"
    );
    assert!(f.root.join("outer").exists());
}
// 覆盖 S-04（默认包含隐藏/系统资料的开关口径）
#[test]
fn hidden_dir_scanned_by_default() {
    let f = Fixture::new();
    f.write("outer/.inner/hidden.txt", b"payload", 10);
    f.write("keep.txt", b"payload", 11);
    // Windows 的隐藏判定看文件属性位（点开头命名不算隐藏），显式打上属性。
    #[cfg(windows)]
    set_hidden(&f.root.join("outer/.inner"), true);
    assert_eq!(
        f.plan(base()).summary.scanned,
        2,
        "默认 include_hidden/include_system 为真：隐藏子目录照常扫描"
    );
    let mut off = base();
    off.include_hidden = false;
    assert_eq!(f.plan(off).summary.scanned, 1, "显式关闭隐藏项后才剪枝");
    #[cfg(windows)]
    set_hidden(&f.root.join("outer/.inner"), false);
}
// 覆盖 C-07
#[test]
fn empty_directory_with_underscore_not_blocked_by_similar_name() {
    // 行为契约：目录名含下划线/百分号时，空目录判定不得波及名字相似（仅差一两个字符）的邻居目录。
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("my_dir/nested_empty")).unwrap();
    f.write("myXdir/file.txt", b"payload", 10);
    let cfg = base();
    let task = f.plan(cfg);
    // 相似前缀目录仍在承载文件：它进空目录计划只能因为该文件有合法的归类移动，
    // 绝不能出现对文件的删除（误伤 = 相似名匹配把邻居当成空目录整树处理）。
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    let deletes = db.actions_page_filtered(0, 100, Some("delete")).unwrap();
    drop(db);
    assert!(
        moves.iter().any(|a| a.source == "myXdir/file.txt"),
        "邻居目录里的文件应有归类移动：{moves:?}"
    );
    assert!(
        deletes.iter().all(|a| a.source != "myXdir/file.txt"),
        "相似前缀目录里的文件不能被误伤：{deletes:?}"
    );
    Fixture::apply(&task);
    assert!(
        !f.root.join("my_dir").exists(),
        "含下划线的空目录必须能被清理"
    );
    assert!(
        exists_somewhere(&f.root, "file.txt"),
        "相似前缀目录里的文件内容仍在"
    );
}
// 覆盖 C-07
#[test]
fn empty_directory_with_percent_in_name_is_cleaned() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("100%done")).unwrap();
    f.write("100Xdone/file.txt", b"payload", 10);
    let cfg = base();
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let deletes = db.actions_page_filtered(0, 100, Some("delete")).unwrap();
    drop(db);
    assert!(
        deletes.iter().all(|a| a.source != "100Xdone/file.txt"),
        "相似前缀目录里的文件不能被误伤：{deletes:?}"
    );
    Fixture::apply(&task);
    assert!(!f.root.join("100%done").exists());
    assert!(exists_somewhere(&f.root, "file.txt"));
}
// 覆盖 C-05, C-07（归类与空目录清理同一计划：folder/ 随归类腾空并即时入空目录计划）
#[test]
fn classification_empty_dirs_planned_in_same_pass() {
    let f = Fixture::new();
    f.write("folder/a.pdf", b"pdf", 10);
    let cfg = base();
    let target = "文档/a.pdf".to_string();
    let task = f.plan(cfg.clone());
    assert_eq!(task.summary.planned_move, 1);
    assert_eq!(
        task.summary.planned_empty, 1,
        "folder/ becomes empty after the move and must be planned now"
    );
    Fixture::apply(&task);
    assert!(f.root.join(&target).exists());
    assert!(!f.root.join("folder").exists());
    let again = f.plan(cfg);
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.planned_move, 0);
    assert_eq!(again.summary.planned_empty, 0);
}
// 注：原 date_classification_is_idempotent 考的是已删除的 classify=Date 按修改日期归类
// 形态；「大类/年/月」时间归类与「大类/功能分类」两级功能归类均随 2026-09-23 需求变更
// 整体废除，由 classify_shape.rs 的 organize_parent_after_children_rebuilds_standard_shape
// 等锚定「大类」一级新形态。
// 覆盖 S-04
#[test]
fn excluded_tree_not_touched() {
    let f = Fixture::new();
    f.write("a.txt", b"same", 10);
    f.write("protected/a.txt", b"same", 20);
    let mut cfg = base();
    cfg.exclusions = "protected".into();
    assert_eq!(f.plan(cfg).summary.scanned, 1);
}
#[cfg(windows)]
fn set_hidden(path: &Path, hidden: bool) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, INVALID_FILE_ATTRIBUTES,
    };
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<u16>>();
    // SAFETY: wide 是以 NUL 结尾的 UTF-16 路径；调用只读取该缓冲区。
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    assert_ne!(
        attrs,
        INVALID_FILE_ATTRIBUTES,
        "读取属性失败：{}",
        path.display()
    );
    let next = if hidden {
        attrs | FILE_ATTRIBUTE_HIDDEN
    } else {
        attrs & !FILE_ATTRIBUTE_HIDDEN
    };
    assert_ne!(
        // SAFETY: 同上，wide 仍指向以 NUL 结尾的 UTF-16 路径。
        unsafe { SetFileAttributesW(wide.as_ptr(), next) },
        0,
        "设置属性失败：{}",
        path.display()
    );
}
// 覆盖 S-04
#[cfg(windows)]
#[test]
fn hidden_root_directory_still_scanned() {
    // 行为锚点：扫描从根目录的条目开始，根目录自身的隐藏属性不参与剪枝；
    // 根下的隐藏子目录在默认口径（include_hidden/include_system 默认真）下照常扫描，
    // 用户显式关闭隐藏项后才整树剪枝。
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    f.write("secret/b.txt", b"hidden", 20);
    set_hidden(&f.root, true);
    set_hidden(&f.root.join("secret"), true);
    let included = f.plan(base());
    let mut pruned = base();
    pruned.include_hidden = false;
    let excluded = f.plan(pruned);
    set_hidden(&f.root, false);
    set_hidden(&f.root.join("secret"), false);
    assert_eq!(
        included.summary.scanned, 2,
        "默认包含隐藏资料：隐藏根目录里的普通文件与隐藏子目录里的文件都必须被扫描到"
    );
    assert_eq!(
        excluded.summary.scanned, 1,
        "显式关闭隐藏项后剪枝隐藏子目录，隐藏根目录本身仍不剪枝"
    );
}
// 覆盖 S-04
#[cfg(not(windows))]
#[test]
fn hidden_root_directory_still_scanned() {
    // 行为锚点：扫描从根目录的条目开始，根目录自身的点开头命名不参与剪枝；
    // 根下的点开头子目录在默认口径（include_hidden 默认真）下照常扫描，
    // 用户显式关闭隐藏项后才剪枝。
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".hidden-root");
    fs::create_dir_all(root.join(".secret")).unwrap();
    fs::write(root.join("a.txt"), b"payload").unwrap();
    fs::write(root.join(".secret/b.txt"), b"hidden").unwrap();
    let state = temp.path().join("state");
    let included = engine::prepare_at(&root, base(), Context::default(), &state).unwrap();
    let mut pruned = base();
    pruned.include_hidden = false;
    let excluded = engine::prepare_at(&root, pruned, Context::default(), &state).unwrap();
    assert_eq!(
        included.summary.scanned, 2,
        "默认包含隐藏资料：点开头根目录里的普通文件与点开头子目录里的文件都必须被扫描到"
    );
    assert_eq!(
        excluded.summary.scanned, 1,
        "显式关闭隐藏项后剪枝点开头子目录，点开头根目录本身仍不剪枝"
    );
}
// 注：原 merge_directories_never_pulls_files_out_of_output 考的是已删除的
// merge_directories / output_dir 归类形态（C-05 固定归类后不再有输出目录与合并）；
// 跨轮幂等由 classify_shape.rs 的 organize_parent_after_children_rebuilds_standard_shape 锚定。
// 覆盖 C-08, C-17
#[test]
fn copy_name_cleanup_never_plans_self_move() {
    // 保留文件剥离副本标记后按 C-17 统一消解改名归类；计划里不得出现
    // source==target 的空转移动（执行后 moved 计数虚高、破坏幂等）。
    let f = Fixture::new();
    f.write("报告 (1).pdf", b"A", 30);
    f.write("报告 (2).pdf", b"A", 20);
    f.write("报告.pdf", b"B", 10);
    let mut cfg = base();
    cfg.clean_copy_name = true;
    let task = f.plan(cfg);
    assert_eq!(task.summary.planned_delete, 1, "同内容副本应被删除");
    let db = Database::open(&task.directory).unwrap();
    let actions = db.actions_page(0, 100).unwrap();
    drop(db);
    assert!(
        actions
            .iter()
            .all(|a| a.target.as_deref() != Some(a.source.as_str())),
        "不得生成 source==target 的空转移动：{actions:?}"
    );
    Fixture::apply(&task);
    let dir = f.root.join("文档");
    // C-16：(1)/(2) 各自输出为 _1/_2；mtime 最新的 (1) 是本组保留者。
    assert_eq!(
        fs::read(dir.join("报告_1.pdf")).unwrap(),
        b"A",
        "副本名清理后的保留者内容仍在"
    );
    assert_eq!(fs::read(dir.join("报告.pdf")).unwrap(), b"B");
    assert!(
        !exists_somewhere(&f.root, "报告 (2).pdf"),
        "同内容副本应被删除"
    );
}

// 覆盖 R-03（默认递归包含全部子目录，可关）
#[test]
fn no_recursion_leaves_subdirectories_untouched() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("sub/b", b"same", 20);
    let mut cfg = base();
    cfg.recursive = false;
    assert_eq!(f.plan(cfg).summary.scanned, 1);
}

// 平台门禁原因：创建符号链接在 Windows 需管理员/开发者模式特权，Unix 无需特权即可稳定构造；
// safe_join 拒绝链接穿越的断言只能在 Unix 下验证。
// 覆盖 S-04
#[cfg(unix)]
#[test]
fn symlink_not_followed_or_deleted() {
    let f = Fixture::new();
    let outside = f.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("a"), b"a").unwrap();
    std::os::unix::fs::symlink(&outside, f.root.join("link")).unwrap();
    assert!(fsutil::safe_join(&f.root, "link/a").is_err());
    assert_eq!(f.plan(base()).summary.scanned, 0);
}

// 覆盖 C-06（大文件单独归类，默认阈值 1 GiB）
#[test]
fn large_files_classified_into_own_directory() {
    let f = Fixture::new();
    // 逻辑大小达 1 GiB 的稀疏文件：set_len 只改元数据，不占实际磁盘
    let big = f.root.join("big.bin");
    let handle = fs::File::create(&big).unwrap();
    handle.set_len(1 << 30).unwrap();
    drop(handle);
    filetime::set_file_mtime(&big, filetime::FileTime::from_unix_time(100, 0)).unwrap();
    f.write("small.txt", b"s", 100);
    let mut cfg = base();
    cfg.large_files = true; // large_threshold_bytes 默认 1 GiB
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    let big_move = moves
        .iter()
        .find(|a| a.source == "big.bin")
        .expect("大文件应有归类移动计划");
    assert_eq!(
        big_move.target.as_deref(),
        Some("大文件/big.bin"),
        "C-06：大文件进入「大文件」大类并按一级结构落位"
    );
    assert!(
        moves.iter().all(|a| {
            a.target
                .as_deref()
                .is_none_or(|t| !t.starts_with("大文件") || a.source == "big.bin")
        }),
        "小文件不参与大文件归类：{moves:?}"
    );
    assert!(
        moves.iter().any(|a| a.source == "small.txt"
            && a.target.as_deref().is_some_and(|t| t.starts_with("文档/"))),
        "小文件仍按普通大类归类：{moves:?}"
    );
}
// 注：原 flatten_single_child_collapses_levels 与 merge_directories_joins_same_name_directories
// 考的是已删除的 flatten_single_child / merge_directories 归类形态（R-02）；来源目录
// 在 C-17/C-18 中只作为冲突消解前缀参与，由 classify_shape.rs 锚定。

// 平台门禁原因：用例需要 junction（mklink /J）复现「分类目录名被重定向目录占用」；
// 非 Windows 无 reparse point 语义（Unix 侧链接拒绝穿越由 symlink_not_followed_or_deleted 覆盖）。
#[cfg(windows)]
#[test]
fn junction_named_like_category_skips_item_instead_of_failing_plan() {
    let f = Fixture::new();
    let outside = f.temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(f.root.join("图片"))
        .arg(&outside)
        .status()
        .unwrap();
    assert!(status.success(), "无法创建 junction，用例前置条件不成立");
    f.write("photo.png", b"not really a png", 10);
    // 默认规则按大类归类：photo.png 的目标是 图片/photo.png，大类段正是 junction。
    let task = f.plan(Config::default());
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    let events = event_lines(&db, 20);
    drop(db);
    assert!(
        moves.is_empty(),
        "目标路径穿过链接的文件必须按项跳过，不得整次分析失败：{moves:?}"
    );
    assert!(f.root.join("photo.png").exists(), "跳过的文件必须原地保留");
    assert!(
        events.iter().any(|event| event.contains("目标路径不可用")),
        "按项跳过必须留日志：{events:?}"
    );
}
