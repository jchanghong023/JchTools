use super::*;

// 覆盖 X-03, X-05：全部成员成功落盘后永久删除原包。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn members_in_subdirectories_merge_into_created_parents() {
    // 回归：含子目录的包此前只创建到祖父目录，成员改名以「系统找不到指定的路径」失败，
    // 整包被记为失败并留下半截空目录（含目录的 RAR 完全解不出来）。
    let f = ArchiveFixture::new();
    fs::create_dir_all(f.input.join("sub/dir1")).unwrap();
    fs::create_dir_all(f.input.join("sub/dir2")).unwrap();
    fs::write(f.input.join("sub/dir1/file1.txt"), b"one").unwrap();
    fs::write(f.input.join("sub/dir2/file2.txt"), b"two").unwrap();
    fs::write(f.input.join("top.txt"), b"top").unwrap();
    f.archive(&f.root.join("pack.zip"), "-tzip");
    let result = f.run(config());
    assert_eq!(
        result.summary.archives_failed, 0,
        "含子目录的包不得计入失败"
    );
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(fs::read(f.root.join("sub/dir1/file1.txt")).unwrap(), b"one");
    assert_eq!(fs::read(f.root.join("sub/dir2/file2.txt")).unwrap(), b"two");
    assert_eq!(fs::read(f.root.join("top.txt")).unwrap(), b"top");
    assert!(
        !f.root.join("pack.zip").exists(),
        "X-05：全部成员成功落盘后必须永久删除原压缩包"
    );
}

// P3-05 / X-04 / S-01：同包兄弟共享深父链，仍保留冲突文件和空目录。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn shared_destination_parents_preserve_conflicts_and_empty_directories() {
    let f = ArchiveFixture::new();
    let relative = Path::new("shared/level1/level2/level3");
    fs::create_dir_all(f.input.join(relative).join("empty/leaf")).unwrap();
    fs::create_dir_all(f.root.join("shared/level1")).unwrap();
    for index in 0..64 {
        fs::write(
            f.input
                .join(relative)
                .join(format!("member-{index:02}.txt")),
            format!("member payload {index}"),
        )
        .unwrap();
    }
    fs::create_dir_all(f.root.join(relative)).unwrap();
    fs::write(
        f.root.join(relative).join("member-00.txt"),
        b"existing user content",
    )
    .unwrap();
    f.archive(&f.root.join("shared.7z"), "-t7z");

    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.extracted, 64);
    assert_eq!(result.summary.deleted, 1);
    assert_eq!(
        fs::read(f.root.join(relative).join("member-00.txt")).unwrap(),
        b"existing user content",
        "共享缓存不得把已存在的最终名视为可覆盖"
    );
    for index in 0..64 {
        let name = if index == 0 {
            "member-00 (1).txt".to_owned()
        } else {
            format!("member-{index:02}.txt")
        };
        assert_eq!(
            fs::read(f.root.join(relative).join(name)).unwrap(),
            format!("member payload {index}").as_bytes(),
            "每个兄弟成员均完整落盘"
        );
    }
    assert!(f.root.join(relative).join("empty/leaf").is_dir());
    assert!(!f.root.join("shared.7z").exists());
}

// P3-05：只校验被排除成员的父链，不提前创建目录；获准的兄弟成员照常落盘。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn excluded_members_do_not_create_validated_parent_chains() {
    let f = ArchiveFixture::new();
    fs::create_dir_all(f.input.join("blocked/deep/empty")).unwrap();
    for index in 0..16 {
        fs::write(
            f.input.join(format!("blocked/deep/member-{index}.txt")),
            format!("excluded payload {index}"),
        )
        .unwrap();
    }
    fs::write(f.input.join("allowed.txt"), b"allowed member").unwrap();
    let archive = f.root.join("filtered.7z");
    f.archive(&archive, "-t7z");
    let original = fs::read(&archive).unwrap();
    let cfg = Config {
        // 附录 C / F06：命中真实目录 blocked 才整树排除；blocked/** 不匹配目录自身。
        exclusions: "blocked".into(),
        ..config()
    };

    let result = f.run(cfg);
    assert_eq!(result.summary.archives_ok, 0, "部分排除不能算完整解开");
    assert_eq!(result.summary.archives_failed, 1);
    assert_eq!(result.summary.archives_quarantined, 1);
    assert_eq!(result.summary.extracted, 1);
    assert_eq!(result.summary.deleted, 0, "原包仍含未解出的成员");
    assert_eq!(
        fs::read(f.root.join("allowed.txt")).unwrap(),
        b"allowed member"
    );
    assert!(
        !f.root.join("blocked").exists(),
        "过滤前不得创建任何父链或空目录"
    );
    assert_eq!(
        fs::read(f.root.join("解压失败/filtered.7z")).unwrap(),
        original,
        "部分排除的源包逐字节保留"
    );
}
// 覆盖 C-02, C-01：先解压（工具一）再分析（工具二，只读），两工具分工后仍能衔接出删除计划。
// C-02（2026-09-21 第四批）：不同名去重默认关闭，本用例显式开启后再验证衔接。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn extract_then_organize_generates_dedup_plan() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("inside.txt"), b"duplicate").unwrap();
    fs::write(f.root.join("existing.txt"), b"duplicate").unwrap();
    f.archive(&f.root.join("one.zip"), "-tzip");
    let extracted = f.run(config());
    assert_eq!(extracted.summary.archives_ok, 1);
    assert!(f.root.join("inside.txt").exists());
    assert!(f.root.join("existing.txt").exists());
    let mut org = organizer();
    org.dedup_other_names = true;
    let plan = engine::prepare_at(&f.root, org, Context::default(), &f.state).unwrap();
    assert_eq!(plan.summary.planned_delete, 1);
}
// 覆盖 X-08
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn solid_7z_is_decoded_in_one_pass() {
    let f = ArchiveFixture::new();
    for i in 0..12 {
        fs::write(
            f.input.join(format!("{i}.txt")),
            vec![u8::try_from(i).unwrap(); 8192],
        )
        .unwrap();
    }
    f.archive(&f.root.join("solid.7z"), "-t7z");
    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.extracted, 12);
    assert!(!f.root.join(".jchtools-work").exists());
}
// 覆盖 X-06, X-05
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn corrupt_archive_is_quarantined_and_logged() {
    let f = ArchiveFixture::new();
    fs::write(f.root.join("broken.zip"), b"not a zip").unwrap();
    let cfg = config();
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 1);
    assert!(!f.root.join("broken.zip").exists(), "损坏包不得留在原位置");
    assert!(
        f.root.join("解压失败").join("broken.zip").exists(),
        "损坏包移入「解压失败」（X-06）"
    );
}
// 覆盖 X-06
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn encrypted_archive_is_quarantined_not_deleted() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("secret.txt"), b"secret").unwrap();
    let status = Command::new(&f.engine)
        .current_dir(&f.input)
        .args(["a", "-t7z", "-pTEST-ONLY-NOT-A-REAL-SECRET", "-mhe=on"])
        .arg(f.root.join("secret.7z"))
        .arg(".")
        .status()
        .unwrap();
    assert!(status.success());
    let cfg = config();
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 1);
    assert!(
        f.root.join("解压失败").join("secret.7z").exists(),
        "加密包移入「解压失败」而非被删除"
    );
}
// 覆盖 X-08
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn nested_archives_are_processed_recursively() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"payload").unwrap();
    let inner = f.tmp.path().join("inner.zip");
    f.archive(&inner, "-tzip");
    fs::remove_file(f.input.join("payload.txt")).unwrap();
    fs::rename(inner, f.input.join("inner.zip")).unwrap();
    f.archive(&f.root.join("outer.zip"), "-tzip");
    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 2);
    assert!(f.root.join("payload.txt").exists());
    // 覆盖 R-03：关闭递归时新解出的子目录包保留，但根层包仍可处理。
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"nested payload").unwrap();
    let inner = f.tmp.path().join("inner.zip");
    f.archive(&inner, "-tzip");
    fs::remove_file(f.input.join("payload.txt")).unwrap();
    fs::create_dir(f.input.join("sub")).unwrap();
    fs::rename(inner, f.input.join("sub/inner.zip")).unwrap();
    f.archive(&f.root.join("outer.zip"), "-tzip");
    let result = f.run(Config {
        recursive: false,
        ..config()
    });
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.archives_failed, 0);
    assert!(f.root.join("sub/inner.zip").is_file());
    assert!(!f.root.join("sub/payload.txt").exists());
}
// 覆盖 X-08, X-06
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn nested_archive_depth_limit_quarantines_unprocessed_package() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"payload").unwrap();
    let inner = f.tmp.path().join("inner.zip");
    f.archive(&inner, "-tzip");
    fs::remove_file(f.input.join("payload.txt")).unwrap();
    fs::rename(inner, f.input.join("inner.zip")).unwrap();
    f.archive(&f.root.join("outer.zip"), "-tzip");
    let mut cfg = config();
    cfg.max_depth = 1;
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.archives_failed, 1);
    assert!(
        f.root.join("解压失败").join("inner.zip").exists(),
        "超限包移入「解压失败」并记录原因（X-08）"
    );
    assert!(!f.root.join("payload.txt").exists());
    // 覆盖 X-10/H-04：本次解出的缺首卷组仍须发现并整组隔离。
    let f = ArchiveFixture::new();
    fs::create_dir(f.input.join("sub")).unwrap();
    fs::write(f.input.join("sub/missing.part02.rar"), b"part2").unwrap();
    fs::write(f.input.join("sub/missing.part03.rar"), b"part3").unwrap();
    f.archive(&f.root.join("outer.zip"), "-tzip");
    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.archives_failed, 1);
    assert!(f.root.join("解压失败/missing.part02.rar").is_file());
    assert!(f.root.join("解压失败/missing.part03.rar").is_file());
    assert!(!f.root.join("sub/missing.part02.rar").exists());
    assert!(!f.root.join("sub/missing.part03.rar").exists());
}
// 覆盖 X-03
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn xz_single_stream_names_member_after_the_archive() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"stream payload\n").unwrap();
    f.pack(&f.root.join("payload.txt.xz"), &["-txz"], &["payload.txt"]);
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"stream payload\n"
    );
}
// 覆盖 X-03
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn bzip2_wrapped_tar_extracts_through_the_named_intermediate_tar() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"stream payload\n").unwrap();
    f.pack(&f.input.join("bundle.tar"), &["-ttar"], &["payload.txt"]);
    f.pack(
        &f.root.join("bundle.tar.bz2"),
        &["-tbzip2"],
        &["bundle.tar"],
    );
    let result = f.run(Config {
        max_depth: 1,
        ..config()
    });
    assert_eq!(
        result.summary.archives_ok, 1,
        "复合流整体只计一个包和一层（X-01）"
    );
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.deleted, 1, "只删源复合包，中间tar不正式落盘");
    assert!(!f.root.join("bundle.tar").exists());
    assert!(!f.root.join("bundle.tar.bz2").exists());
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"stream payload\n"
    );
}
// 覆盖 X-03
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn tgz_shorthand_restores_the_tar_suffix() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"stream payload\n").unwrap();
    f.pack(&f.input.join("bundle.tar"), &["-ttar"], &["payload.txt"]);
    f.pack(&f.root.join("bundle.tgz"), &["-tgzip"], &["bundle.tar"]);
    let result = f.run(Config {
        max_depth: 1,
        ..config()
    });
    assert_eq!(
        result.summary.archives_ok, 1,
        "tgz整体只计一个包和一层（X-01）"
    );
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.deleted, 1);
    assert!(!f.root.join("bundle.tar").exists());
    assert!(!f.root.join("bundle.tgz").exists());
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"stream payload\n"
    );
}
// 覆盖 X-04, X-05, C-05：等量包各自解压（同名成员冲突只改名），随后整理归类稳定。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn equal_content_archives_extract_both_and_classify_is_stable() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("beta.txt"), b"beta member\n").unwrap();
    f.archive(&f.root.join("base.zip"), "-tzip");
    f.archive(&f.root.join("base.tar"), "-ttar");
    let cfg = config();
    let extracted = f.run(cfg);
    assert_eq!(extracted.summary.archives_ok, 2, "两个包都应完全解开");
    assert_eq!(extracted.summary.archives_failed, 0);
    assert_eq!(extracted.summary.archives_quarantined, 0);
    assert_eq!(
        extracted.summary.deleted, 2,
        "两个包都完整解开：原包各自按 X-05 永久删除"
    );
    assert!(
        !f.root.join("base.zip").exists() && !f.root.join("base.tar").exists(),
        "成功原包必须被永久删除（X-05）"
    );
    assert_eq!(fs::read(f.root.join("beta.txt")).unwrap(), b"beta member\n");
    assert_eq!(
        fs::read(f.root.join("beta (1).txt")).unwrap(),
        b"beta member\n",
        "两个包解出同名成员：第二个按 H-07 改名落盘，内容相同也不跳过"
    );
    // 归档会恢复成员的 mtime；后解出不代表时间更新。显式建立 C-03 的严格顺序，
    // 不依赖包处理顺序、时钟精度或引擎是否保留时间。
    filetime::set_file_mtime(
        f.root.join("beta.txt"),
        filetime::FileTime::from_unix_time(100, 0),
    )
    .unwrap();
    filetime::set_file_mtime(
        f.root.join("beta (1).txt"),
        filetime::FileTime::from_unix_time(200, 0),
    )
    .unwrap();
    // 整理：C-05 固定归类「大类」一级恒开启。
    let org = organizer();
    let task = engine::prepare_at(&f.root, org.clone(), Context::default(), &f.state).unwrap();
    ArchiveFixture::apply(&task);
    // C-03：上方显式设置 beta (1).txt 的修改时间较新，保留它并按
    // C-08 清理副本标记，最终名 beta_1.txt；beta.txt 作为重复副本被永久删除。
    let classified = "文档/beta_1.txt".to_string();
    assert!(
        f.root.join(&classified).exists(),
        "保留者（beta (1).txt → beta_1.txt）归类落位，重复的 beta.txt 已删除"
    );
    let again = engine::prepare_at(&f.root, org, Context::default(), &f.state).unwrap();
    assert_eq!(again.summary.archives_ok, 0);
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.planned_move, 0);
    assert!(f.root.join(&classified).exists());
}
// 覆盖 X-05, X-07
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn rerun_of_a_recreated_source_lands_a_new_copy() {
    // X-05 删除成功原包；X-07 不承诺幂等，也不按历史成功记录跳过。用户重新放入同名包
    // 属于新任务：内容照旧完整解压、冲突只给新成员改名，既有文件一份都不动。
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"payload for kept source\n").unwrap();
    f.archive(&f.root.join("kept.zip"), "-tzip");
    let cfg = config();
    let extracted = f.run(cfg.clone());
    assert_eq!(extracted.summary.archives_ok, 1);
    assert!(
        !f.root.join("kept.zip").exists(),
        "成功解压后原包被永久删除（X-05）"
    );
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"payload for kept source\n"
    );
    // 模拟内容被搬走（例如目录整理归类到别的子目录）。
    fs::create_dir(f.root.join("文档")).unwrap();
    fs::rename(f.root.join("payload.txt"), f.root.join("文档/payload.txt")).unwrap();
    // 用户重新放入同名包：新任务必须真的重新解压，不得按历史成功记录跳过。
    f.archive(&f.root.join("kept.zip"), "-tzip");
    let again = f.run(cfg);
    assert_eq!(again.summary.archives_ok, 1, "新任务中的包必须重新完整解压");
    assert_eq!(again.summary.archives_failed, 0);
    assert!(
        !f.root.join("kept.zip").exists(),
        "新任务里同样按 X-05 删除原包"
    );
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"payload for kept source\n",
        "重跑重新落盘本次解出的内容（X-07：不按历史成功记录跳过）"
    );
    assert!(
        f.root.join("文档/payload.txt").exists(),
        "上一份内容不被动到（两个工具各管一段）"
    );
}
