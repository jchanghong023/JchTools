use super::*;

// ===== 2026-09-22 合同整改：解压语义回归（H-07 / X-01 / X-04 / X-05 / X-06 / X-07 / X-08 / H-06）=====
// 本段用例指向整改后的合同行为：完整落盘的原包与已佐证分卷永久删除、冲突一律只给新成员
// 改名、内容相同也要落盘、Git 目录树不动、空间不足停止整个任务而不是逐包隔离。
// 整改前这些断言必然失败，是「先红后绿」的红侧证据。

// 覆盖 X-05, X-01（回归：全部成员成功落盘后，原包与全部分卷都被永久删除；
// 已落盘的解压结果与既有文件不受影响）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn successful_extraction_deletes_source_and_all_volumes() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("a.txt"), b"one").unwrap();
    f.archive(&f.root.join("one.zip"), "-tzip");
    let volumes = f.split_set(&f.root.join("split.zip"));
    assert!(volumes >= 2, "测试前置：应生成至少两个分卷，实际 {volumes}");
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0, "正常包不得计入失败");
    assert_eq!(result.summary.archives_ok, 2, "普通包与分卷组各按一包计数");
    assert_eq!(result.summary.archives_quarantined, 0);
    assert_eq!(
        result.summary.deleted,
        u64::try_from(volumes).unwrap() + 1,
        "普通包与分卷组的每个卷都必须永久删除（X-05）"
    );
    assert!(
        !f.root.join("one.zip").exists(),
        "X-05：成功解压的原包必须永久删除"
    );
    assert!(f.root.join("a.txt").is_file(), "成员应解压落盘且不被删改");
    for index in 1..=volumes {
        assert!(
            !f.root.join(format!("split.zip.{index:03}")).exists(),
            "分卷 {index} 必须与主体一并删除（X-05）"
        );
    }
    for index in 0..6 {
        assert!(
            f.root.join(format!("f{index}.txt")).is_file(),
            "分卷成员 f{index}.txt 应解压落盘"
        );
    }
    assert!(
        !f.root.join(".jchtools-work").exists(),
        "本次所有权标记的临时工作区必须清理干净（X-08）"
    );
}

// 覆盖 X-04, X-01（回归：目标已有同内容文件时，本次解出的成员仍必须落盘为改名副本）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn identical_member_conflict_lands_as_a_new_copy() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("a.txt"), b"same bytes").unwrap();
    fs::write(f.root.join("a.txt"), b"same bytes").unwrap();
    f.pack(&f.root.join("one.zip"), &["-tzip"], &["a.txt"]);
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(
        result.summary.extracted, 1,
        "成员恰落盘一次（改名后仍是本次解出的那一份）"
    );
    assert_eq!(
        fs::read(f.root.join("a.txt")).unwrap(),
        b"same bytes",
        "既有文件不得改动（H-07/S-01）"
    );
    assert_eq!(
        fs::read(f.root.join("a (1).txt")).unwrap(),
        b"same bytes",
        "等内容的成员必须落盘为改名副本（X-04/H-07）"
    );
}

// 覆盖 X-04, X-01（回归：不同目录下的同内容成员不得按内容相同跳过落盘）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn identical_member_in_another_directory_is_not_skipped() {
    let f = ArchiveFixture::new();
    fs::create_dir_all(f.root.join("src")).unwrap();
    fs::write(f.root.join("src/x.txt"), b"identical payload").unwrap();
    fs::create_dir_all(f.input.join("dst")).unwrap();
    fs::write(f.input.join("dst/x.txt"), b"identical payload").unwrap();
    f.pack(&f.root.join("one.zip"), &["-tzip"], &["dst/x.txt"]);
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(
        fs::read(f.root.join("src/x.txt")).unwrap(),
        b"identical payload",
        "既有文件不得改动（H-07）"
    );
    assert_eq!(
        fs::read(f.root.join("dst/x.txt")).unwrap(),
        b"identical payload",
        "跨目录同内容成员必须照常落盘（X-04/X-01：不得跨目录按内容去重）"
    );
    assert_eq!(result.summary.extracted, 1);
}

// 覆盖 H-06 / X-04：保护既有 Git 整树，新解出的同名目录另选未占用名称。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn member_targeting_existing_git_tree_is_renamed_and_tree_untouched() {
    let f = ArchiveFixture::new();
    fs::create_dir_all(f.root.join("repo/.git")).unwrap();
    fs::write(f.root.join("repo/.git/config"), b"original git metadata").unwrap();
    fs::write(f.root.join("repo/keep.txt"), b"user file").unwrap();
    fs::create_dir_all(f.input.join("repo")).unwrap();
    fs::write(f.input.join("repo/file.txt"), b"incoming member").unwrap();
    f.pack(&f.root.join("one.zip"), &["-tzip"], &["repo/file.txt"]);
    let result = f.run(config());
    assert_eq!(
        fs::read(f.root.join("repo/.git/config")).unwrap(),
        b"original git metadata",
        "Git 目录树内容不得改动（H-06）"
    );
    assert_eq!(
        fs::read(f.root.join("repo/keep.txt")).unwrap(),
        b"user file",
        "既有用户文件不得改动（H-07）"
    );
    assert!(
        !f.root.join("repo/file.txt").exists(),
        "成员不得落入既有 Git 目录树（H-06）"
    );
    assert_eq!(
        fs::read(f.root.join("repo (1)/file.txt")).unwrap(),
        b"incoming member",
        "新目录另选未占用位置，全部成员完整落盘（X-04）"
    );
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.deleted, 1);
    assert!(!f.root.join("one.zip").exists(), "完整落盘后删源（X-05）");
    assert!(!f.root.join("解压失败/one.zip").exists());
}

// 覆盖 S-04 / H-07 / X-04 / X-05：旧链接前缀不是保留命名空间或删除授权。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn old_link_prefix_members_are_extracted_without_touching_existing_files() {
    let f = ArchiveFixture::new();
    let filename = ".jchtools-link-user.txt";
    fs::write(f.root.join(filename), b"existing user bytes").unwrap();
    fs::write(f.input.join(filename), b"incoming member bytes").unwrap();
    fs::create_dir(f.input.join(".jchtools-link-folder")).unwrap();
    fs::write(
        f.input.join(".jchtools-link-folder/notes.txt"),
        b"ordinary nested member",
    )
    .unwrap();
    f.pack(
        &f.root.join("one.zip"),
        &["-tzip"],
        &[filename, ".jchtools-link-folder"],
    );
    let result = f.run(config());
    assert_eq!(
        fs::read(f.root.join(filename)).unwrap(),
        b"existing user bytes"
    );
    assert_eq!(
        fs::read(f.root.join(".jchtools-link-user (1).txt")).unwrap(),
        b"incoming member bytes"
    );
    assert_eq!(
        fs::read(f.root.join(".jchtools-link-folder/notes.txt")).unwrap(),
        b"ordinary nested member"
    );
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.deleted, 1);
    assert!(!f.root.join("one.zip").exists(), "完整落盘后才允许删源");
}

// 覆盖 H-06 / X-05 / X-06：入包 Git 子树固定整树排除，未全部落盘不得删源。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn git_subtree_members_are_skipped_as_a_whole() {
    let f = ArchiveFixture::new();
    // 完整清单中的 .git 建立固定边界：兄弟成员、后代及嵌套包同样不合入。
    fs::create_dir_all(f.input.join("project/.git")).unwrap();
    fs::create_dir_all(f.input.join("project/src")).unwrap();
    fs::write(
        f.input.join("project/.git/config"),
        b"incoming git metadata",
    )
    .unwrap();
    fs::write(f.input.join("project/README.md"), b"sibling of the git dir").unwrap();
    fs::write(
        f.input.join("project/src/notes.txt"),
        b"descendant of the git tree",
    )
    .unwrap();
    fs::write(f.input.join("project/inner.zip"), b"must not be decoded").unwrap();
    fs::write(f.input.join("normal.txt"), b"normal member").unwrap();
    f.pack(
        &f.root.join("one.zip"),
        &["-tzip"],
        &["project", "normal.txt"],
    );
    let original_archive = fs::read(f.root.join("one.zip")).unwrap();
    // H-06 的 Git 整树保护不依赖用户 glob。
    let cfg = Config {
        exclusions: String::new(),
        ..config()
    };
    let result = f.run(cfg);
    assert!(
        !f.root.join("project").exists(),
        "Git 子树整体不合入，不能只跳过标记而把兄弟或后代写出"
    );
    assert_eq!(
        fs::read(f.root.join("normal.txt")).unwrap(),
        b"normal member",
        "不属于 Git 子树的成员照常解压"
    );
    assert_eq!(
        result.summary.archives_ok, 0,
        "Git 成员未落盘，不能把安全子集冒报整包成功"
    );
    assert_eq!(result.summary.archives_failed, 1);
    assert_eq!(result.summary.archives_quarantined, 1);
    assert_eq!(result.summary.deleted, 0, "不满足 X-05 时不得删源");
    assert!(!f.root.join("one.zip").exists(), "失败原包按 X-06 隔离");
    assert_eq!(
        fs::read(f.root.join("解压失败/one.zip")).unwrap(),
        original_archive,
        "所有受保护成员仍在完整原包中，不能被丢弃"
    );
}

// 覆盖 H-06（回归：压缩包根直接含 .git 时，整个暂存根都不得解出，避免把选定目录
// 变成 Git 树、让两工具随后整体拒绝该目录）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn root_level_git_member_skips_the_whole_staged_root() {
    let f = ArchiveFixture::new();
    fs::create_dir_all(f.input.join(".git")).unwrap();
    fs::write(f.input.join(".git/config"), b"incoming git metadata").unwrap();
    fs::write(f.input.join("normal.txt"), b"normal member").unwrap();
    f.pack(
        &f.root.join("one.zip"),
        &["-tzip"],
        &[".git/config", "normal.txt"],
    );
    let cfg = Config {
        exclusions: String::new(),
        ..config()
    };
    let result = f.run(cfg);
    assert!(
        !f.root.join(".git").exists(),
        "不得创建/解出 Git 目录树（H-06：整树排除、不解压）"
    );
    assert!(
        !f.root.join("normal.txt").exists(),
        "暂存根直接含 .git：整个暂存根一律不解出，不得留下半个目录"
    );
    assert_eq!(
        result.summary.archives_ok, 0,
        "有成员未解开时不得宣称成功（X-06）"
    );
    assert!(
        !f.root.join("one.zip").exists(),
        "原包移入「解压失败」后不再留在原位置（X-06）"
    );
    assert!(
        f.root.join("解压失败/one.zip").is_file(),
        "含跳过条目的原包移入「解压失败」且不删除（X-06）"
    );
}

// 覆盖 X-06, X-08（回归：空间不足必须保留原包并停止整个任务，不得逐包隔离后继续）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn insufficient_space_stops_the_task_without_quarantine() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("a.txt"), b"one").unwrap();
    f.archive(&f.root.join("one.zip"), "-tzip");
    fs::write(f.input.join("b.txt"), b"two").unwrap();
    f.archive(&f.root.join("two.zip"), "-tzip");
    // 预留 ≈1 PiB：任何真实磁盘都不满足，稳定触发「空间不足」这条任务级中止路径。
    let cfg = Config {
        reserve_bytes: 1u64 << 50,
        ..config()
    };
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &f.root,
        cfg,
        Context::default(),
        &f.state,
        Some(&f.engine),
    );
    let error = result.expect_err("空间不足必须停止整个任务（X-08）");
    assert!(
        format!("{error:#}").contains("空间"),
        "错误说明必须指向空间不足：{error:#}"
    );
    assert!(
        f.root.join("one.zip").is_file() && f.root.join("two.zip").is_file(),
        "空间不足保留原包，不得移入「解压失败」（X-06/X-08）"
    );
    assert!(
        !f.root.join("解压失败").exists(),
        "空间不足不得隔离任何包，也不得继续批量处理后续包（X-08）"
    );
}

// 覆盖 X-08（回归：分卷包的展开比例分母是实际卷集合合计，不是主体单卷大小）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn split_set_ratio_limit_uses_total_volume_bytes() {
    let f = ArchiveFixture::new();
    let volumes = f.split_set(&f.root.join("split.zip"));
    assert!(volumes >= 2, "测试前置：应生成至少两个分卷，实际 {volumes}");
    // 单卷 1 KiB、成员合计约 9 KiB：以主体单卷为分母会把健康的分卷组误判为「展开比例
    // 超限」并打入「解压失败」；按实际卷集合合计（X-08）应正常解开。
    let cfg = Config {
        max_ratio: 2,
        ..config()
    };
    let result = f.run(cfg);
    assert_eq!(
        result.summary.archives_failed, 0,
        "分卷组不得因分母口径被判超限（X-08：分卷体积按实际卷集合合计）"
    );
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.archives_quarantined, 0);
    for index in 0..6 {
        assert!(
            f.root.join(format!("f{index}.txt")).is_file(),
            "成员 f{index}.txt 应解压落盘"
        );
    }
}

// 覆盖 X-05, X-07, H-04（回归：每个包只处理一次，包括本次解出的嵌套包；成功原包按 X-05 删除）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn nested_archives_are_processed_exactly_once() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"stream payload\n").unwrap();
    f.pack(&f.input.join("bundle.tar"), &["-ttar"], &["payload.txt"]);
    fs::copy(f.input.join("bundle.tar"), f.root.join("bundle.tar")).unwrap();
    f.pack(&f.root.join("bundle.tar.gz"), &["-tgzip"], &["bundle.tar"]);
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(
        result.summary.archives_ok, 2,
        "预置tar与复合tar.gz各处理一次，中间tar不成为正式嵌套包（X-01）"
    );
    assert_eq!(
        result.summary.deleted, 2,
        "两个用户包各自完整解开，复合包的中间tar不计删源"
    );
    assert!(
        !f.root.join("bundle.tar").exists()
            && !f.root.join("bundle.tar.gz").exists()
            && !f.root.join("bundle (1).tar").exists(),
        "只保留最终成员，中间层不在正式位置留下"
    );
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"stream payload\n"
    );
    assert!(
        f.root.join("payload (1).txt").is_file(),
        "两包解出同名成员时改名落盘（X-04）"
    );
}

// 覆盖 X-04（回归：子目录候选序号的占位检查必须落在真实父路径上
// （`{parent}/{candidate}`），不得双前缀拼接把检查落到不存在的路径上恒判空闲；
// 修复前嵌套子目录恒选序号 1，撞上已占用目标导致成员级改名甚至整包失败）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn nested_directory_picks_free_index_under_occupied_parent() {
    let f = ArchiveFixture::new();
    fs::create_dir_all(f.input.join("sub/inner")).unwrap();
    fs::write(f.input.join("sub/inner/file.txt"), b"nested payload").unwrap();
    f.archive(&f.root.join("pack.zip"), "-tzip");
    // 目标处的 sub 已是普通目录（X-04 合入语义，父目录不改名），其下 inner 与
    // inner (1) 被普通文件占用 → 子目录应选最小未占用序号 inner (2)。
    fs::create_dir_all(f.root.join("sub")).unwrap();
    fs::write(f.root.join("sub/inner"), b"occupied").unwrap();
    fs::write(f.root.join("sub/inner (1)"), b"occupied").unwrap();
    let result = f.run(config());
    assert_eq!(
        result.summary.archives_ok, 1,
        "嵌套目录按最小未占用序号映射后整包应完整成功（X-04）"
    );
    assert!(
        f.root.join("sub/inner (2)/file.txt").is_file(),
        "子目录在真实父路径下选择真正的最小未占用序号 inner (2)（X-04）"
    );
    assert_eq!(
        fs::read(f.root.join("sub/inner (2)/file.txt")).unwrap(),
        b"nested payload"
    );
    assert!(
        fs::read(f.root.join("sub/inner")).unwrap() == b"occupied"
            && fs::read(f.root.join("sub/inner (1)")).unwrap() == b"occupied",
        "既有占用文件必须原样保留（X-04/H-07）"
    );
}

// 覆盖 X-04, X-03（回归：新目录名被既有文件占用时，为新目录选最小未占用序号
// `目录 (1)`，该目录全部成员整体映射到新目录；既有文件不动，整包完整成功。
// 修复前：含文件成员经 ensure_dir 报「目录已存在同名文件」整包失败并隔离）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn directory_name_taken_by_file_maps_members_to_a_new_directory() {
    let f = ArchiveFixture::new();
    fs::create_dir_all(f.input.join("sub/empty")).unwrap();
    fs::write(f.input.join("sub/file.txt"), b"member payload").unwrap();
    f.archive(&f.root.join("pack.zip"), "-tzip");
    // 目标处的 sub 被普通文件占用。
    fs::write(f.root.join("sub"), b"existing file occupies the name").unwrap();
    let result = f.run(config());
    assert_eq!(
        result.summary.archives_ok, 1,
        "目录改名映射后整包应完整成功（X-04）"
    );
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(
        fs::read(f.root.join("sub")).unwrap(),
        b"existing file occupies the name",
        "既有文件必须原样保留（X-04/H-07）"
    );
    assert_eq!(
        fs::read(f.root.join("sub (1)/file.txt")).unwrap(),
        b"member payload",
        "成员必须整体映射到改名后的新目录（X-04）"
    );
    assert!(
        f.root.join("sub (1)/empty").is_dir(),
        "空目录条目同样落在改名后的新目录内（X-04）"
    );
    assert!(
        !f.root.join("解压失败/pack.zip").exists(),
        "目录名冲突按 X-04 改名映射后不得再按失败隔离"
    );
}

// 覆盖 X-04（回归：空目录名被既有文件占用时同样按最小未占用序号改名，
// 包不判失败；修复前空目录被跳过、整包判未完全解开并隔离）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn empty_directory_name_taken_by_file_lands_under_a_new_number() {
    let f = ArchiveFixture::new();
    fs::create_dir(f.input.join("空目录")).unwrap();
    f.pack(&f.root.join("empty.7z"), &["-t7z"], &["空目录"]);
    fs::write(f.root.join("空目录"), b"occupied").unwrap();
    let result = f.run(config());
    assert_eq!(
        result.summary.archives_ok, 1,
        "空目录改名落盘后包应完整成功（X-04）"
    );
    assert_eq!(
        fs::read(f.root.join("空目录")).unwrap(),
        b"occupied",
        "既有文件必须原样保留（X-04/H-07）"
    );
    assert!(
        f.root.join("空目录 (1)").is_dir(),
        "空目录应改用最小未占用序号（X-04）"
    );
    assert!(
        !f.root.join("解压失败/empty.7z").exists(),
        "空目录改名落盘后不得再按失败隔离"
    );
}

// 覆盖 X-07/X-05：新解出的隔离容器正常落盘，仅停止其中嵌套包的后续递归。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn quarantine_component_member_in_subdir_archive_is_rejected_as_dangerous() {
    let f = ArchiveFixture::new();
    fs::create_dir(f.input.join("解压失败")).unwrap();
    fs::write(f.input.join("解压失败/x.txt"), b"shadow").unwrap();
    fs::write(f.input.join("解压失败/inner.zip"), b"must not be decoded").unwrap();
    fs::create_dir(f.root.join("sub")).unwrap();
    f.archive(&f.root.join("sub").join("a.zip"), "-tzip");
    let result = f.run(config());
    assert_eq!(
        fs::read(f.root.join("sub/解压失败/x.txt")).unwrap(),
        b"shadow"
    );
    assert_eq!(
        fs::read(f.root.join("sub/解压失败/inner.zip")).unwrap(),
        b"must not be decoded"
    );
    assert_eq!(result.summary.archives_ok, 1, "外层全部成员落盘即完整成功");
    assert_eq!(
        result.summary.archives_failed, 0,
        "容器内包属于范围排除，不是新失败"
    );
    assert_eq!(result.summary.archives_quarantined, 0);
    assert_eq!(result.summary.deleted, 1);
    assert!(!f.root.join("sub/a.zip").exists());
}
