use super::*;
use jchtools_file_core::task::QUARANTINE_DIR_NAME;

// 覆盖 X-07（所选根本身名为「解压失败」时不开始解压：清点与解压都拒绝并明确提示）
#[test]
fn root_named_quarantine_blocks_count_and_extract() {
    let f = Fixture::new();
    f.write("解压失败/待重试.zip", b"retry me");
    let quarantine_root = f.root.join(QUARANTINE_DIR_NAME);
    let error = format!(
        "{:#}",
        jchtools_recursive_extract::engine::count_archives(&quarantine_root, &base()).unwrap_err()
    );
    assert!(
        error.contains("解压失败") && error.contains("移出"),
        "根本身为隔离容器时清点必须拒绝并提示移出待重试包：{error}"
    );
    let error = format!(
        "{:#}",
        jchtools_recursive_extract::engine::extract_run_at(
            &quarantine_root,
            base(),
            Context::default(),
            &f.state,
            None
        )
        .unwrap_err()
    );
    assert!(
        error.contains("解压失败") && error.contains("移出"),
        "根本身为隔离容器时解压必须拒绝并提示移出待重试包：{error}"
    );
    // 与 root_git 拒绝用例同口径：被拒绝的任务不得留下任务目录（整次处理不执行）。
    let tasks = f.state.join("tasks");
    assert!(
        !tasks.exists() || fs::read_dir(&tasks).unwrap().next().is_none(),
        "被拒绝的任务不得留下任务目录（整次处理不执行）"
    );
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

// 覆盖 C-09/X-07/X-02（任意层级的「解压失败」容器都整树剪枝：子目录单独解压按
// X-06 天然在子目录里留下隔离容器，整理父目录时容器内的包不得计入清点、不入盘点、
// 不参与归类搬移或清理，也不得被读取）
#[test]
fn nested_quarantine_container_is_pruned_at_any_depth() {
    let f = Fixture::new();
    f.write("子/解压失败/坏包.7z", b"broken package");
    f.write("keep.txt", b"payload");
    assert_eq!(
        jchtools_recursive_extract::engine::count_archives(&f.root, &base()).unwrap(),
        0,
        "嵌套隔离容器内的包不得计入确认框清点（X-02/X-07）"
    );
    let task = f.plan(base());
    assert_eq!(
        task.summary.scanned, 1,
        "嵌套隔离容器内容不得进入盘点（C-09）：{:#?}",
        task.summary
    );
    engine::apply(&task.directory, Context::default()).unwrap();
    assert_eq!(
        fs::read(f.root.join("子/解压失败/坏包.7z")).unwrap(),
        b"broken package",
        "嵌套隔离容器内的包不得被搬移或删除（C-09）"
    );
    assert!(
        !events(&task.directory).contains("坏包.7z"),
        "嵌套隔离容器内的包不得参与归类或清理（C-09）：{}",
        events(&task.directory)
    );
    assert!(
        f.root.join("子/解压失败").is_dir(),
        "非空的隔离容器不得被收尾清理（C-09：仅实际为空才可按 H-05 清理）"
    );
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
    let task = jchtools_recursive_extract::engine::extract_run_at(
        &f.root,
        base(),
        Context::default(),
        &f.state,
        None,
    )
    .unwrap();
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
        jchtools_recursive_extract::engine::count_archives(&f.root, &base()).unwrap(),
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
    let remaining: Vec<_> = fs::read_dir(f.root.join("文档"))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(remaining.len(), 1, "缓存故障后必须恰好保留一个副本");
    let survivor = &remaining[0];
    assert!(
        survivor.file_name() == "a.txt" || survivor.file_name() == "b.txt",
        "保留者必须来自原重复组"
    );
    assert_eq!(fs::read(survivor.path()).unwrap(), b"same");
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
