use super::*;
// 覆盖 X-05 / S-01：注释伪造多卷信息时，X-10 精确命名族之外的旁路文件不得被处置。
#[test]
fn forged_comment_corroboration_is_ignored() {
    let tmp = fixture("forged-comment");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.rar"), b"standalone rar", 100);
    // X-10 的旧式 rar 尾卷是 .r00～.r99；.r000 不是该命名族的实际卷。
    write_with_mtime(&root.join("report.r000"), b"innocent bystander", 200);
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&comment_forging_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        root.join("report.r000").exists(),
        "档案注释里伪造的多卷键不得把命名族之外的 .r000 纳入处置集合"
    );
    assert!(
        !root.join("report.rar").exists(),
        "完整解开的主体按 X-05 永久删除"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "删除集合只含主体自身：伪造佐证不得把旁路文件算进来"
    );
}

// 覆盖 X-05, S-01（回归 2026-09-19：注释内 } 行绕过注释跳过——crafted 对抗变体）。
#[test]
fn forged_comment_with_brace_line_cannot_enable_sweep() {
    let tmp = fixture("forged-comment-brace");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.rar"), b"standalone rar", 100);
    // .r00 按 X-10 已是实际卷；用不属于精确命名族的 .r000 作为无关文件。
    write_with_mtime(&root.join("report.r000"), b"innocent bystander", 200);
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&comment_escape_forging_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        root.join("report.r000").exists(),
        "注释内 }} 行后的伪造键不得把命名族之外的 .r000 纳入处置集合"
    );
    assert!(
        !root.join("report.rar").exists(),
        "完整解开的主体按 X-05 永久删除"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "删除集合只含主体自身，伪造佐证不得扩大处置范围"
    );
}

// 覆盖 X-05：引擎档案级佐证为真的分卷组成功解压后，主体与全部分卷一并永久删除。
#[test]
fn corroborated_volume_siblings_are_deleted_together() {
    let tmp = fixture("corroborated");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.rar"), b"multi-volume main", 100);
    write_with_mtime(&root.join("report.r00"), b"volume 0", 200);
    write_with_mtime(&root.join("report.r01"), b"volume 1", 300);
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&volume_index_engine(tmp.path(), 3)),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 1);
    assert!(!root.join("report.rar").exists(), "主体必须删除（X-05）");
    assert!(
        !root.join("report.r00").exists() && !root.join("report.r01").exists(),
        "已佐证的分卷必须随主体一并删除（X-05）"
    );
    assert_eq!(result.summary.deleted, 3, "主体与两个分卷各计一次删除");
    assert_eq!(result.summary.errors, 0, "正常删除不得计为错误");
}

// 覆盖 X-05（回归 2026-09-22：成功原包的删除失败必须如实报错，不得虚报成功、不得隔离）。
// 旧口径「原包无需删除权限」已随 X-05 取消：完整解开但删不掉时，解压结果与原包全部保留，
// 任务以明确错误结束，不把清理失败当成坏包隔离。
// 平台门禁原因：只有 Windows 的共享模式（FILE_SHARE_READ 拒绝删除）能构造该失败。
#[cfg(windows)]
#[test]
fn read_locked_archives_surface_cleanup_failure_and_are_preserved() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = fixture("locked-source");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("a.zip"), b"fake archive a", 100);
    write_with_mtime(&root.join("b.zip"), b"fake archive b", 200);
    // 恒成功引擎：空输出表示空包，完整落盘后进入 X-05 删除。
    let engine_path = ok_engine(tmp.path());
    // Windows 读取共享句柄禁止删除：两个包的删除都必然失败（与处理顺序无关）。
    let _guards: Vec<fs::File> = ["a.zip", "b.zip"]
        .iter()
        .map(|name| {
            let mut opts = fs::OpenOptions::new();
            opts.read(true).share_mode(1); // FILE_SHARE_READ：拒绝写入与删除
            opts.open(root.join(name)).unwrap()
        })
        .collect();
    let state = state_of(&tmp);
    let _error = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state,
        Some(&engine_path),
    )
    .expect_err("源包删除失败必须报错（X-05）");
    assert_eq!(fs::read(root.join("a.zip")).unwrap(), b"fake archive a");
    assert_eq!(fs::read(root.join("b.zip")).unwrap(), b"fake archive b");
    assert!(
        !root.join("解压失败").exists(),
        "清理失败不得被当成坏包隔离（X-05）"
    );
    let db = task_db(&state);
    let summary = db.summary().unwrap();
    assert_eq!(summary.archives_ok, 0, "清理失败不得虚计成功（X-05）");
    assert_eq!(summary.archives_failed, 0, "不得把清理失败记为坏包");
    assert!(summary.errors > 0, "清理失败必须计入错误项");
    assert_eq!(running_rows(&db), 0, "包行不得停留在 running");
}

// 覆盖 X-05（部分删除的中止口径）：已佐证分卷组里主体删除成功、随后某个分卷删除失败时，
// 不得隔离这组「已部分删除」的包、不得回滚已删除的卷、也不得虚计成功；未删除的分卷保留，
// 错误明确说明剩余数量，任务停止且不再处理后续包。
// 平台门禁原因：只有 Windows 的共享模式（FILE_SHARE_READ 拒绝删除）能构造该失败。
#[cfg(windows)]
#[test]
fn partially_deleted_volume_group_reports_failure_without_quarantine() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = fixture("locked-volume");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.rar"), b"multi-volume main", 100);
    write_with_mtime(&root.join("report.r00"), b"volume 0", 200);
    // 佐证为真的分卷组：删除集合是 [主体, .r00]；只锁住 .r00，主体必然先被删除。
    let _guard = {
        let mut opts = fs::OpenOptions::new();
        opts.read(true).share_mode(1); // FILE_SHARE_READ：拒绝写入与删除
        opts.open(root.join("report.r00")).unwrap()
    };
    let state = state_of(&tmp);
    let _error = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state,
        Some(&volume_index_engine(tmp.path(), 2)),
    )
    .expect_err("分卷删除失败必须报错（X-05）");
    assert!(
        !root.join("report.rar").exists(),
        "先删除的主体不回滚（X-05 不承诺多卷删除事务性）"
    );
    assert!(
        root.join("report.r00").exists(),
        "未删除的分卷保留在原位，不得顺手隔离或删除"
    );
    assert!(
        !root.join("解压失败").exists(),
        "已部分删除的包组不得被当成坏包隔离（X-05）"
    );
    let db = task_db(&state);
    let summary = db.summary().unwrap();
    assert_eq!(summary.archives_ok, 0, "清理失败不得虚计成功（X-05）");
    assert_eq!(summary.deleted, 1, "只计真实发生的那一次删除");
    assert!(summary.errors > 0, "清理失败必须计入错误项");
    assert_eq!(running_rows(&db), 0, "包行不得停留在 running");
}
