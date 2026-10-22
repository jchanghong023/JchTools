use super::*;
// 覆盖 X-03, X-05（无引擎时的行为：E-05 明确报错，不静默跳过）
#[test]
fn extract_run_without_engine_fails_loudly() {
    // 本机存在可用引擎（resources/7zip 或已释放副本）时无法构造「缺引擎」环境：
    // CI（无引擎）上才真正执行断言；有引擎的机器跳过，不伪造红灯。
    if jchtools_recursive_extract::archive::SevenZip::from_bundle().is_ok() {
        eprintln!("skip：本机存在可用解压引擎，无法验证缺引擎路径（CI 覆盖）");
        return;
    }
    let tmp = fixture("noengine");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("pack.zip"), b"pretend archive bytes", 100);
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        None,
    );
    let error = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
    assert!(
        error.contains("引擎") || error.contains("7z") || error.contains("7-Zip"),
        "缺引擎必须给明确错误，而不是静默跳过压缩包（E-05）：{error}"
    );
    assert!(root.join("pack.zip").exists(), "缺引擎时原包不得被处置");
}

// 覆盖 H-06/S-01/X-05/X-08：合法空 ZIP 的公开解压不能被用户同名文件/Git 容器阻断。
// 复用空包脚本引擎隔离工作区落点；真实 7-Zip 验收仍由引擎套件承担。
#[test]
fn valid_empty_archive_survives_occupied_staging_container() {
    for git in [false, true] {
        let tmp = fixture("staging-container-occupied");
        let root = tmp.path().join("data");
        let fixed = root.join(".jchtools-work");
        if git {
            fs::create_dir_all(fixed.join(".git")).unwrap();
            fs::write(fixed.join(".git/HEAD"), b"original git metadata").unwrap();
            fs::write(fixed.join("keep.txt"), b"git project data").unwrap();
        } else {
            fs::write(&fixed, b"ordinary user file").unwrap();
        }
        let mut empty_zip = [0u8; 22];
        empty_zip[..4].copy_from_slice(b"PK\x05\x06");
        fs::write(root.join("empty.zip"), empty_zip).unwrap();
        let result = jchtools_recursive_extract::engine::extract_run_at(
            &root,
            Config {
                reserve_bytes: 0,
                max_ratio: 0,
                ..Config::default()
            },
            Context::default(),
            &state_of(&tmp),
            Some(&ok_engine(tmp.path())),
        )
        .unwrap();
        assert_eq!(result.summary.archives_ok, 1);
        assert_eq!(result.summary.archives_failed, 0);
        assert_eq!(result.summary.deleted, 1);
        assert!(!root.join("empty.zip").exists());
        assert!(!root.join("解压失败").exists());
        assert!(
            !root.join(".jchtools-work (1)").exists(),
            "本次自建备用容器应清理"
        );
        if git {
            assert_eq!(
                fs::read(fixed.join(".git/HEAD")).unwrap(),
                b"original git metadata"
            );
            assert_eq!(
                fs::read(fixed.join("keep.txt")).unwrap(),
                b"git project data"
            );
            assert_eq!(fs::read_dir(fixed).unwrap().count(), 2);
        } else {
            assert_eq!(fs::read(fixed).unwrap(), b"ordinary user file");
        }
    }
}

// 覆盖 X-01：自动解压白名单之外的文件完全不碰——引擎一次都不得被调用，
// 文件字节原样保留，也不得被当成失败包隔离；只有白名单内的包被处理并清理。
#[test]
fn non_whitelisted_containers_are_never_opened() {
    let tmp = fixture("whitelist");
    let root = tmp.path().join("data");
    // 内容故意用合法 ZIP：引擎确实能打开，语义上却是安装介质、文档或程序包。
    let containers: [(&str, &[u8]); 18] = [
        ("visproww.cab", b"zip payload, but an install cab"),
        ("windows.iso", b"zip payload, but a disk image"),
        ("boot.wim", b"zip payload, but a system image"),
        ("install.esd", b"zip payload, but a system image"),
        ("legacy.lzh", b"zip payload, but a legacy container"),
        ("archive.cpio", b"zip payload, but a cpio archive"),
        ("report.docx", b"zip payload, but a document"),
        ("package.msi", b"zip payload, but an installer"),
        ("setup.exe", b"zip payload, but an installer"),
        ("app.apk", b"zip payload, but an android package"),
        ("library.jar", b"zip payload, but a java package"),
        ("wheel.whl", b"zip payload, but a python package"),
        ("book.epub", b"zip payload, but an ebook"),
        ("addon.crx", b"zip payload, but a browser extension"),
        ("styles.xpi", b"zip payload, but a browser extension"),
        // X-09：白名单外格式的分卷与配不上主包的孤立编号文件，同样完全不碰。
        // （老式 zip/rar 族尾卷不在本列：缺主包时按 X-10 归组报告，见
        //   old_style_tails_without_main_pack_are_grouped_and_quarantined。）
        ("windows.iso.001", b"zip payload, but an image volume"),
        ("report.docx.001", b"zip payload, but a document volume"),
        ("data.001", b"zip payload, but an orphan numbered file"),
    ];
    for (index, (name, bytes)) in containers.iter().enumerate() {
        let stamp = 100 + i64::try_from(index).unwrap();
        write_with_mtime(&root.join(name), bytes, stamp);
    }
    write_with_mtime(&root.join("keep.zip"), b"the only whitelisted archive", 400);
    let engine = logging_engine(tmp.path());
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&engine),
    )
    .unwrap();
    let calls = fs::read_to_string(tmp.path().join("fake-log.txt")).unwrap_or_default();
    assert!(
        calls.contains("keep.zip"),
        "白名单内的包必须交给引擎处理：{calls}"
    );
    for (name, bytes) in containers {
        assert!(
            !calls.contains(name),
            "{name} 不属于自动解压白名单，引擎不得打开它：{calls}"
        );
        assert_eq!(
            fs::read(root.join(name)).unwrap(),
            bytes,
            "{name} 必须原样保留（不碰、不改、不移动）"
        );
    }
    assert!(
        !root.join("keep.zip").exists(),
        "白名单内的包完整成功后按 X-05 永久删除"
    );
    assert!(
        !root.join("解压失败").exists(),
        "白名单外的文件不是失败包，不得进隔离目录"
    );
    assert_eq!(result.summary.archives_ok, 1, "只应处理白名单内的那一个包");
    assert_eq!(result.summary.deleted, 1);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(result.summary.archives_failed, 0);
}

// 覆盖 X-10（老式 zip/rar 族只发现尾卷、没有主包时仍归组，报告缺主包）、
// X-02（残缺但可归组的卷集计一包）、X-06（整组移入「解压失败」并记录原因）
#[test]
fn old_style_tails_without_main_pack_are_grouped_and_quarantined() {
    use jchtools_file_core::db::Database;
    let tmp = fixture("oldstyle");
    let root = tmp.path().join("data");
    // 老式 zip 族：report.z01 + report.z02，无 report.zip；老式 rar 族：orphan.r00，
    // 无 orphan.rar。按 X-10 各自是一组失败包（缺主包），整组隔离并报告缺主包。
    write_with_mtime(&root.join("report.z01"), b"old-style zip tail one", 100);
    write_with_mtime(&root.join("report.z02"), b"old-style zip tail two", 101);
    write_with_mtime(&root.join("orphan.r00"), b"old-style rar tail", 102);
    // X-09 对照：白名单外孤立编号文件仍完全不碰。
    write_with_mtime(
        &root.join("windows.iso.001"),
        b"zip payload, but an image volume",
        103,
    );
    // X-02：确认数量按「残缺但可归组的卷集计一包」——zip 组 + rar 组 = 2。
    assert_eq!(
        jchtools_recursive_extract::engine::count_archives(&root, &Config::default()).unwrap(),
        2,
        "缺主包的老式族尾卷组应各计一包（X-02/X-10）"
    );
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(
        result.summary.archives_failed, 2,
        "两组缺主包尾卷各按一个失败包计"
    );
    assert_eq!(
        result.summary.archives_quarantined, 3,
        "整组隔离：report.z01+z02 与 orphan.r00"
    );
    assert!(
        !root.join("report.z01").exists(),
        "尾卷组应整组移入「解压失败」"
    );
    assert!(!root.join("report.z02").exists());
    assert!(!root.join("orphan.r00").exists());
    assert!(root.join("解压失败").join("report.z01").is_file());
    assert!(root.join("解压失败").join("report.z02").is_file());
    assert!(root.join("解压失败").join("orphan.r00").is_file());
    assert!(
        root.join("windows.iso.001").is_file(),
        "X-09 对照项原样保留（不碰、不隔离）"
    );
    let db = Database::open(&result.directory).unwrap();
    let events = event_lines(&db, 100);
    let quarantined: Vec<&String> = events
        .iter()
        .filter(|line| line.contains("移入解压失败"))
        .collect();
    assert!(
        quarantined.iter().any(|line| line.contains("缺主包")),
        "失败原因必须报告缺主包（X-10）：{quarantined:?}"
    );
}

// 覆盖 X-05 / X-10：ZIP 完整解开后主体删除，精确尾卷族以外的近似后缀文件原样保留。
#[test]
fn stale_z_sibling_is_not_disposed_with_standalone_zip() {
    let tmp = fixture("stale-z");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.zip"), b"standalone complete zip", 100);
    write_with_mtime(
        &root.join("report.z001"),
        b"stale fragment of an old set",
        200,
    );
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&ok_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        !root.join("report.zip").exists(),
        "成功原包按 X-05 永久删除"
    );
    assert!(
        root.join("report.z001").exists(),
        "X-10 旧式 zip 尾卷只有 .z01～.z99，近似后缀 .z001 不得随包处置"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "删除集合只含主体自身（X-05：不得误删仅同主干的文件）"
    );
    assert_eq!(
        fs::read(root.join("report.z001")).unwrap(),
        b"stale fragment of an old set"
    );
}

// 覆盖 X-05 / X-10：旧式 RAR 精确尾卷族以外的近似后缀文件不得被处置。
// .r00～.r99 按需求属于实际卷；使用 .r000 区分仅名称近似的无关文件。
#[test]
fn stale_r_sibling_is_not_disposed_with_standalone_rar() {
    let tmp = fixture("stale-r");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.rar"), b"standalone complete rar", 100);
    write_with_mtime(
        &root.join("report.r000"),
        b"stale fragment of an old set",
        200,
    );
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&ok_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        !root.join("report.rar").exists(),
        "成功原包按 X-05 永久删除"
    );
    assert!(
        root.join("report.r000").exists(),
        "X-10 旧式 rar 尾卷只有 .r00～.r99，近似后缀 .r000 不得随包处置"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "删除集合只含主体自身（X-05：不得误删仅同主干的文件）"
    );
    assert_eq!(
        fs::read(root.join("report.r000")).unwrap(),
        b"stale fragment of an old set"
    );
}
