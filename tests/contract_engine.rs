//! engine.rs 合同回归：Git 整树排除（H-06/S-04）、整理收尾实空清理（H-05/C-07/C-09）、
//! 确认框清点与扫描同口径（X-02）、缓存降级（C-13）与状态口径（C-10/C-11）。
// 测试代码允许 unwrap/expect（与 tests/core.rs 的集成测试惯例一致）：断言失败即测试失败。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
use jchtools::{
    archive::QUARANTINE_DIR_NAME,
    config::{Config, DeleteMode},
    control::Context,
    db::Database,
    engine,
};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

struct Fixture {
    /// 只用于持有临时目录生命周期（测试经 root/state 访问内容）。
    _temp: TempDir,
    root: PathBuf,
    state: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let state = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        Self {
            _temp: temp,
            root,
            state,
        }
    }
    fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
    fn dir(&self, rel: &str) -> PathBuf {
        let path = self.root.join(rel);
        fs::create_dir_all(&path).unwrap();
        path
    }
    /// 目录形式（普通 clone）的 Git 标记。
    fn git_marker_dir(&self, rel: &str) -> PathBuf {
        let proj = self.dir(rel);
        fs::create_dir_all(proj.join(".git")).unwrap();
        fs::write(proj.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        proj
    }
    /// 文件形式（worktree / submodule）的 Git 标记。
    fn git_marker_file(&self, rel: &str) -> PathBuf {
        let proj = self.dir(rel);
        fs::write(proj.join(".git"), b"gitdir: ../.git/modules/x\n").unwrap();
        proj
    }
    fn plan(&self, config: Config) -> engine::TaskResult {
        engine::prepare_at(&self.root, config, Context::default(), &self.state).unwrap()
    }
}
fn base() -> Config {
    Config {
        global_delete: DeleteMode::Permanent,
        // 归类按 C-05 恒开启（大类一级），不再有 classify 开关。
        dedup_other_names: true,
        ..Config::default()
    }
}
fn status(directory: &Path) -> String {
    Database::open_existing(directory)
        .unwrap()
        .get("status")
        .unwrap()
}
fn events(directory: &Path) -> String {
    let db = Database::open_existing(directory).unwrap();
    let mut statement = db
        .conn
        .prepare("SELECT phase,source,target,result,reason FROM events")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok(format!(
                "[{}] {} | {} → {} | {}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(4)?
            ))
        })
        .unwrap();
    let values: Result<Vec<String>, _> = rows.collect();
    values.unwrap().join("\n")
}
#[cfg(windows)]
fn set_hidden(path: &Path, hidden: bool) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, INVALID_FILE_ATTRIBUTES,
    };
    let wide = |p: &Path| {
        let mut value: Vec<u16> = p.as_os_str().encode_wide().collect();
        value.push(0);
        value
    };
    let raw = wide(path);
    // SAFETY: raw 是调用期间有效且以 NUL 结尾的 UTF-16 路径，API 不保留指针。
    let attrs = unsafe { GetFileAttributesW(raw.as_ptr()) };
    assert_ne!(attrs, INVALID_FILE_ATTRIBUTES, "测试前置：读取属性失败");
    let next = if hidden {
        attrs | FILE_ATTRIBUTE_HIDDEN
    } else {
        attrs & !FILE_ATTRIBUTE_HIDDEN
    };
    assert_ne!(
        // SAFETY: raw 生命周期覆盖调用，next 由已读取属性仅切换隐藏位得到。
        unsafe { SetFileAttributesW(raw.as_ptr(), next) },
        0,
        "测试前置：写入隐藏属性失败"
    );
}

// 覆盖 H-06（选定根目录直接含 .git：整次处理不执行并明确提示）
#[test]
fn root_git_directory_blocks_prepare_count_and_extract() {
    let f = Fixture::new();
    f.git_marker_dir("");
    f.write("keep.txt", b"payload");
    let error = format!(
        "{:#}",
        engine::prepare_at(&f.root, base(), Context::default(), &f.state).unwrap_err()
    );
    assert!(
        error.contains(".git"),
        "根目录含 .git 必须明确提示：{error}"
    );
    let error = format!(
        "{:#}",
        engine::count_archives(&f.root, &base()).unwrap_err()
    );
    assert!(error.contains(".git"), "确认框清点同样必须拒绝：{error}");
    let error = format!(
        "{:#}",
        engine::extract_run_at(&f.root, base(), Context::default(), &f.state, None).unwrap_err()
    );
    assert!(error.contains(".git"), "递归解压同样必须拒绝：{error}");
    let tasks = f.state.join("tasks");
    assert!(
        !tasks.exists() || fs::read_dir(&tasks).unwrap().next().is_none(),
        "被拒绝的任务不得留下任务目录（整次处理不执行）"
    );
}

// 覆盖 H-06（.git 为文件——worktree/submodule——同样触发整次拒绝）
#[test]
fn root_git_file_blocks_processing() {
    let f = Fixture::new();
    f.git_marker_file("");
    f.write("keep.txt", b"payload");
    assert!(engine::prepare_at(&f.root, base(), Context::default(), &f.state).is_err());
    assert!(engine::count_archives(&f.root, &base()).is_err());
}

// 覆盖 H-06（子目录含 .git：只排除该子树；祖先不得被改名/删除，内容不读取、不参与哈希去重）
#[test]
fn child_git_tree_excluded_and_ancestors_untouched() {
    let f = Fixture::new();
    f.write("other/keep.txt", b"same-content");
    // Git 子树里的内容与 other/keep.txt 内容相同：一旦被扫描/哈希，去重会计划删除
    // 其中一份，计划里就会出现 Git 路径——这是「不读取、不参与去重」的反证锚点。
    f.write("only_dir/proj/note.txt", b"same-content");
    f.git_marker_dir("only_dir/proj");
    f.write("only_file/proj/note.txt", b"payload");
    f.git_marker_file("only_file/proj");
    let task = f.plan(base());
    assert_eq!(
        task.summary.scanned, 1,
        "Git 子树内容不得进入盘点：{:#?}",
        task.summary
    );
    let db = Database::open_existing(&task.directory).unwrap();
    let git_files: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE rel LIKE 'only_dir/%' OR rel LIKE 'only_file/%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(git_files, 0, "Git 子树文件不得入库（也就不参与哈希）");
    let actions = db.actions_page(0, 1000).unwrap();
    drop(db);
    // Git 树内部不得被规划任何动作；项目根自身按 C-14 整体移入「Git项目集合」。
    assert!(
        actions
            .iter()
            .all(|action| !action.source.starts_with("only_dir/proj/")
                && !action.source.starts_with("only_file/proj/")),
        "Git 子树内部不得出现在计划里：{actions:#?}"
    );
    let git_moves = actions
        .iter()
        .filter(|a| {
            a.kind == jchtools::model::ActionKind::Move
                && (a.source == "only_dir/proj" || a.source == "only_file/proj")
        })
        .count();
    assert_eq!(git_moves, 2, "两个 Git 项目整体移入集合（C-14）");
    drop(actions);
    assert!(
        events(&task.directory).contains("Git"),
        "跳过 Git 目录必须明确提示：{}",
        events(&task.directory)
    );
    engine::apply(&task.directory, Context::default()).unwrap();
    // 同名项目按 C-17/C-18 加最近来源目录前缀消解，整树内容原样随移动。
    assert!(f.root.join("Git项目集合/only_dir_proj/note.txt").exists());
    assert!(f.root.join("Git项目集合/only_dir_proj/.git/HEAD").exists());
    assert!(
        f.root.join("Git项目集合/only_file_proj/note.txt").exists(),
        ".git 为文件（worktree 形态）的项目同样整树移动"
    );
    assert!(f.root.join("Git项目集合/only_file_proj/.git").is_file());
    assert!(
        exists_somewhere(&f.root, "keep.txt"),
        "Git 之外的普通文件照常归类保留"
    );
}

/// 在根下按文件名递归查找（C-05 归类恒移动文件，断言“内容仍在”需按名找）。
fn exists_somewhere(root: &std::path::Path, name: &str) -> bool {
    fn walk(dir: &std::path::Path, name: &str) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if walk(&path, name) {
                    return true;
                }
            } else if entry.file_name().to_string_lossy() == name {
                return true;
            }
        }
        false
    }
    walk(root, name)
}

// 覆盖 H-05/C-07/C-09（成功整理后收尾清理新产生的空目录链与空的「解压失败」目录）
#[test]
fn final_cleanup_removes_newly_empty_chain_and_empty_quarantine() {
    let f = Fixture::new();
    f.write("keep.txt", b"payload");
    f.dir("vault/deep");
    f.dir(QUARANTINE_DIR_NAME);
    f.write("move_src/report.pdf", b"pdf");
    let config = base();
    // C-05 固定归类「大类」一级：report.pdf → 文档。
    let task = f.plan(config);
    engine::apply(&task.directory, Context::default()).unwrap();
    assert!(f.root.join("文档/report.pdf").exists(), "归类目标必须落盘");
    assert!(
        !f.root.join("move_src").exists(),
        "归类后变空的源目录必须清理"
    );
    assert!(!f.root.join("vault").exists(), "空目录链必须自底向上清理");
    assert!(
        !f.root.join(QUARANTINE_DIR_NAME).exists(),
        "实际为空的「解压失败」目录仍按 H-05 清理"
    );
    assert_eq!(fs::read(f.root.join("文档/keep.txt")).unwrap(), b"payload");
    assert_eq!(status(&task.directory), "finished");
}

// 覆盖 H-05/C-10（取消后不得继续删除空目录；状态如实为已取消）
#[test]
fn cancelled_apply_does_not_run_final_cleanup() {
    let f = Fixture::new();
    f.write("keep.txt", b"payload");
    f.dir("空目录/nested");
    let task = f.plan(base());
    assert!(task.summary.planned_empty > 0, "前置：空目录应进入计划");
    let context = Context::default();
    context.control.cancel();
    assert!(engine::apply(&task.directory, context).is_err());
    assert!(
        f.root.join("空目录/nested").exists(),
        "取消后不得为了清空目录继续删除"
    );
    assert_eq!(status(&task.directory), "cancelled", "用户取消不是失败");
}

// 覆盖 H-03/X-06（递归解压不做空目录清理：清理只属于目录整理）
#[test]
fn extraction_never_cleans_empty_directories() {
    let f = Fixture::new();
    f.write("keep.txt", b"payload");
    f.dir("empty_after_pack");
    let task = engine::extract_run_at(&f.root, base(), Context::default(), &f.state, None).unwrap();
    assert_eq!(
        task.summary.archives_ok + task.summary.archives_failed,
        0,
        "前置：没有压缩包就不进入解压"
    );
    assert!(
        f.root.join("empty_after_pack").exists(),
        "递归解压不得清理目录"
    );
}

// 覆盖 X-02/S-04（确认框清点与正式扫描同一过滤口径：Git 子树不计入）
#[test]
fn count_archives_matches_scan_filtering() {
    let f = Fixture::new();
    f.write("other/real.zip", b"not-a-zip");
    f.write("other/note.txt", b"text");
    f.write("proj/pack.zip", b"not-a-zip");
    f.git_marker_dir("proj");
    assert_eq!(
        engine::count_archives(&f.root, &base()).unwrap(),
        1,
        "Git 子树内的包不得计入确认框"
    );
    let task = f.plan(base());
    assert_eq!(task.summary.scanned, 2, "扫描与清点必须同一口径");
}

// 覆盖 S-04（默认覆盖隐藏与普通命名目录，不因目录名自动漏处理）
#[test]
fn default_scan_covers_hidden_and_ordinary_named_directories() {
    let f = Fixture::new();
    f.write("node_modules/pkg.js", b"js");
    f.write(".hidden_dir/sub.txt", b"hidden");
    // 目录名以 .git 开头但不是 .git（.github）：H-06 只按精确名 .git 判定，不得误剪枝。
    f.write(".github/config.yml", b"cfg");
    f.write("normal.txt", b"normal");
    #[cfg(windows)]
    set_hidden(&f.root.join(".hidden_dir"), true);
    let task = f.plan(Config::default());
    #[cfg(windows)]
    set_hidden(&f.root.join(".hidden_dir"), false);
    assert_eq!(
        task.summary.scanned, 4,
        "默认必须覆盖隐藏与普通命名目录（S-04）：{:#?}",
        task.summary
    );
}

// 覆盖 S-04（缩小范围必须明确提示）
#[test]
fn reduced_scope_is_announced() {
    let f = Fixture::new();
    f.write("top.txt", b"top");
    f.write("sub/deep.txt", b"deep");
    let mut config = base();
    config.recursive = false;
    config.include_hidden = false;
    let task = f.plan(config);
    let events = events(&task.directory);
    assert!(events.contains("范围"), "缩小范围必须明确提示：{events}");
    assert!(events.contains("递归"), "提示必须说明递归被关闭：{events}");
}

// 覆盖 C-13（缓存故障只降级，不中止整理、不改变去重结果）
#[test]
fn broken_hash_cache_degrades_without_failing() {
    let f = Fixture::new();
    f.write("a.txt", b"same");
    f.write("b.txt", b"same");
    fs::create_dir_all(&f.state).unwrap();
    fs::write(f.state.join("hash-cache.sqlite3"), b"not a database").unwrap();
    let task = f.plan(base());
    assert_eq!(task.summary.planned_delete, 1, "缓存故障不得改变去重结果");
    assert!(
        events(&task.directory).contains("哈希缓存"),
        "缓存降级必须如实说明：{}",
        events(&task.directory)
    );
    engine::apply(&task.directory, Context::default()).unwrap();
    // 保留者随 C-05 固定归类移动；按内容判断恰好保留一个副本。
    let remaining = fs::read(f.root.join("文档").join("a.txt"))
        .ok()
        .or_else(|| fs::read(f.root.join("文档").join("b.txt")).ok());
    assert_eq!(remaining, Some(b"same".to_vec()), "恰好保留一个副本");
}

// 覆盖 H-06（Git 树内的崩溃残留不得被清扫）
#[test]
fn stale_link_temp_inside_git_tree_is_not_swept() {
    let f = Fixture::new();
    f.write("keep.txt", b"payload");
    f.git_marker_dir("proj");
    let stale = f.root.join("proj/.jchtools-link-deadbeef");
    fs::hard_link(f.root.join("keep.txt"), &stale).unwrap();
    filetime::set_file_mtime(&stale, filetime::FileTime::from_unix_time(0, 0)).unwrap();
    let task = f.plan(base());
    engine::apply(&task.directory, Context::default()).unwrap();
    // 项目随 C-14 整体移入集合；树内的崩溃残留随树移动且绝不被清扫（H-06）。
    assert!(
        f.root
            .join("Git项目集合/proj/.jchtools-link-deadbeef")
            .exists(),
        "Git 树内的一切内容都不得被删除（H-06）"
    );
}

// 覆盖 H-06（Git 树内的空目录不得被整理清理，树本身只随项目整体移动）
#[test]
fn empty_directories_inside_git_tree_are_preserved() {
    let f = Fixture::new();
    f.write("keep.txt", b"payload");
    f.git_marker_dir("proj");
    f.dir("proj/emptydir");
    f.dir("proj/sub/deep");
    let task = f.plan(base());
    engine::apply(&task.directory, Context::default()).unwrap();
    let moved = f.root.join("Git项目集合/proj");
    assert!(moved.join("emptydir").is_dir());
    assert!(moved.join("sub/deep").is_dir());
    assert!(moved.is_dir());
    assert!(moved.join(".git/HEAD").is_file());
}

// 覆盖 H-05/C-07：无法删除实际空目录时，必须报告未完成而非成功。
// Windows 共享模式可稳定模拟已有目录句柄占用；不修改处理期间的文件内容或目录结构。
#[cfg(windows)]
#[test]
fn failed_empty_directory_cleanup_reports_an_incomplete_task() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    let f = Fixture::new();
    let empty = f.dir("empty");
    let guard = fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&empty)
        .unwrap();
    let task = f.plan(base());
    let result = engine::apply(&task.directory, Context::default());
    drop(guard);
    assert!(result.is_err(), "空目录清理失败不得报告为成功：{result:?}");
    assert_eq!(status(&task.directory), "failed");
    assert!(empty.is_dir());
}
