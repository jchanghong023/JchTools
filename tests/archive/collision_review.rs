use super::*;

// X-04/X-10：一个卷的落点冲突不得拆散新卷族或混入既有缺首卷组。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn review_nested_volume_collision_keeps_families_independent() {
    let f = ArchiveFixture::new();
    let count = f.split_set(&f.input.join("z-nested.zip"));
    assert!(count > 2);
    let mut names: Vec<String> = fs::read_dir(&f.input)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("z-nested.zip."))
        .collect();
    names.sort();
    let sources: Vec<&str> = names.iter().map(String::as_str).collect();
    f.pack(&f.root.join("a-outer.7z"), &["-t7z"], &sources);
    let old = b"independent old user tail";
    fs::write(f.root.join("z-nested.zip.002"), old).unwrap();
    let result = f.run(config());
    assert_eq!(
        result.summary.archives_ok, 2,
        "outer 与新完整 nested 族都须成功"
    );
    assert_eq!(result.summary.archives_failed, 1, "仅初始旧缺首卷组失败");
    assert_eq!(
        fs::read(f.root.join("解压失败/z-nested.zip.002")).unwrap(),
        old,
        "旧组原始字节不得被新卷族消费或替换"
    );
    for index in 0..6 {
        assert_eq!(
            fs::read(f.root.join(format!("f{index}.txt"))).unwrap(),
            fs::read(f.input.join(format!("f{index}.txt"))).unwrap(),
            "新卷族成员必须经真实引擎完整校验并落盘"
        );
    }
    assert!(!f.root.join("a-outer.7z").exists());
}

// 覆盖 X-04/X-05：冲突目录须避开同包其它成员的原名，不能将两个来源目录合并。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn directory_collision_preserves_incoming_sibling_directories() {
    let f = ArchiveFixture::new();
    fs::create_dir_all(f.input.join("sub/empty")).unwrap();
    fs::create_dir_all(f.input.join("sub (1)/empty")).unwrap();
    fs::write(f.input.join("sub/payload.txt"), b"first directory").unwrap();
    fs::write(f.input.join("sub (1)/payload.txt"), b"second directory").unwrap();
    fs::write(f.input.join("sub (2)"), b"incoming sibling file").unwrap();
    f.archive(&f.root.join("pack.zip"), "-tzip");
    fs::write(f.root.join("sub"), b"existing occupier").unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.extracted, 3);
    assert_eq!(fs::read(f.root.join("sub")).unwrap(), b"existing occupier");
    assert_eq!(
        fs::read(f.root.join("sub (3)/payload.txt")).unwrap(),
        b"first directory"
    );
    assert_eq!(
        fs::read(f.root.join("sub (1)/payload.txt")).unwrap(),
        b"second directory"
    );
    assert_eq!(
        fs::read(f.root.join("sub (2)")).unwrap(),
        b"incoming sibling file"
    );
    assert!(f.root.join("sub (3)/empty").is_dir());
    assert!(f.root.join("sub (1)/empty").is_dir());
    assert!(!f.root.join("sub (1)/payload (1).txt").exists());
    assert!(!f.root.join("pack.zip").exists());
}

// 覆盖 X-01/X-09/H-07/附录 B：单点开头的 .zip/.7z/.rar 没有扩展名，即使内容有效也不处理。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn extensionless_archive_dotfiles_stay_untouched_by_public_extraction() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"ordinary archive member").unwrap();
    let zip = f.tmp.path().join("seed.zip");
    let seven_zip = f.tmp.path().join("seed.7z");
    f.archive(&zip, "-tzip");
    f.archive(&seven_zip, "-t7z");
    fs::copy(&zip, f.root.join(".zip")).unwrap();
    fs::copy(&seven_zip, f.root.join(".7z")).unwrap();
    // 合法 RAR4 空包：主头与结束头都带低 16 位 CRC-32，不使用模拟引擎。
    let mut rar = b"Rar!\x1a\x07\x00".to_vec();
    for header in [
        &[0x73u8, 0, 0, 0x0d, 0, 0, 0, 0, 0, 0, 0][..],
        &[0x7bu8, 0, 0, 7, 0][..],
    ] {
        rar.extend_from_slice(&crc32(header).to_le_bytes()[..2]);
        rar.extend_from_slice(header);
    }
    fs::write(f.root.join(".rar"), rar).unwrap();
    let originals: Vec<_> = [".zip", ".7z", ".rar"]
        .into_iter()
        .map(|name| {
            let path = f.root.join(name);
            let status = Command::new(&f.engine)
                .args(["t", "-p-", "--"])
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success(), "真实引擎必须确认 {name} 是有效归档");
            (name, fs::read(path).unwrap())
        })
        .collect();
    f.archive(&f.root.join("ordinary.zip"), "-tzip");
    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 1, "只处理普通扩展名归档");
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.deleted, 1);
    assert_eq!(result.summary.extracted, 1);
    assert_eq!(result.summary.archives_quarantined, 0);
    for (name, bytes) in originals {
        assert_eq!(
            fs::read(f.root.join(name)).unwrap(),
            bytes,
            "{name} 原样保留"
        );
    }
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"ordinary archive member"
    );
    assert!(!f.root.join("ordinary.zip").exists());
    assert!(!f.root.join("解压失败").exists());
}

// Review regressions deliberately have no ignore: run explicitly with the real engine selected.
// UTF-8 位同时写入本地头和中央目录；沿用真实 ZIP 字节夹具，不依赖系统代码页猜名字。
fn review_utf8_stored_zip(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut zip = stored_zip(entries);
    let mut offset = 0;
    for (name, data) in entries {
        zip[offset + 6..offset + 8].copy_from_slice(&0x0800u16.to_le_bytes());
        offset += 30 + name.len() + data.len();
    }
    for (name, _) in entries {
        zip[offset + 8..offset + 10].copy_from_slice(&0x0800u16.to_le_bytes());
        offset += 46 + name.len();
    }
    zip
}

// S-04 / X-05 / X-10：完整卷组也不能访问、隔离或删掉范围外的实际卷。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn review_f01_complete_volume_set_preserves_excluded_source() {
    let f = ArchiveFixture::new();
    let count = f.split_set(&f.root.join("pack.zip"));
    assert!(count >= 3);
    let originals: Vec<_> = (1..=count)
        .map(|index| {
            let name = format!("pack.zip.{index:03}");
            let bytes = fs::read(f.root.join(&name)).unwrap();
            (name, bytes)
        })
        .collect();
    let result = f.run(Config {
        exclusions: "pack.zip.002".to_string(),
        ..config()
    });
    for (name, bytes) in originals {
        assert_eq!(
            fs::read(f.root.join(&name)).unwrap(),
            bytes,
            "不允许通过卷族扩展跨越排除边界，整组不得未经授权读取、隔离或删源：{name}"
        );
    }
    assert_eq!(result.summary.deleted, 0);
    assert_eq!(result.summary.archives_quarantined, 0);
    assert!(
        !f.root.join("f0.txt").exists(),
        "不可解码跨越范围边界的卷组"
    );
}

// S-04 / CONTRACT 附录 C：a/* 匹配真实后代，不匹配普通文件 a 的虚构 a/ 形态。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn review_f06_glob_uses_real_member_path_without_trailing_separator() {
    let f = ArchiveFixture::new();
    fs::write(f.root.join("pack.zip"), stored_zip(&[(b"a", b"root file")])).unwrap();
    let result = f.run(Config {
        exclusions: "a/*".to_string(),
        ..config()
    });
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(fs::read(f.root.join("a")).unwrap(), b"root file");
    assert_eq!(result.summary.archives_ok, 1);
    assert!(!f.root.join("pack.zip").exists());
    assert!(!f.root.join("解压失败").exists());
}

// H-04 / H-07 / X-04：非 BMP 名字只用于真实路径，Windows 比较键不能回拼成磁盘名。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn review_f09_duplicate_non_bmp_members_preserve_real_names() {
    for name in ["😀.txt", "😀/same.txt"] {
        let f = ArchiveFixture::new();
        fs::write(
            f.root.join("pack.zip"),
            review_utf8_stored_zip(&[(name.as_bytes(), b"first"), (name.as_bytes(), b"second")]),
        )
        .unwrap();
        let result = f.run(config());
        assert_eq!(
            result.summary.archives_failed, 0,
            "有效重复成员不得因比较键被隔离：{name}"
        );
        assert_eq!(result.summary.archives_ok, 1);
        assert_eq!(result.summary.extracted, 2);
        assert_eq!(fs::read(f.root.join(name)).unwrap(), b"first");
        let renamed = Path::new(name).with_file_name(format!(
            "{} (1).txt",
            Path::new(name).file_stem().unwrap().to_str().unwrap()
        ));
        assert_eq!(fs::read(f.root.join(renamed)).unwrap(), b"second");
        assert!(!f.root.join("pack.zip").exists());
        assert!(!f.root.join("解压失败").exists());
    }
}

// X-04 / X-05：文件的冲突编号必须避开同包尚未发布的空目录，既有文件保持原样。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn review_f12_file_collision_reserves_incoming_empty_directory_name() {
    let f = ArchiveFixture::new();
    fs::write(f.root.join("a.txt"), b"existing").unwrap();
    fs::write(f.input.join("a.txt"), b"incoming").unwrap();
    fs::create_dir(f.input.join("a (1).txt")).unwrap();
    f.archive(&f.root.join("pack.zip"), "-tzip");
    let result = f.run(config());
    assert_eq!(fs::read(f.root.join("a.txt")).unwrap(), b"existing");
    assert_eq!(
        result.summary.archives_failed, 0,
        "正常文件/目录冲突不能变成坏包"
    );
    assert!(f.root.join("a (1).txt").is_dir(), "包内空目录必须完整落盘");
    assert_eq!(fs::read(f.root.join("a (2).txt")).unwrap(), b"incoming");
    assert_eq!(result.summary.archives_ok, 1);
    assert!(!f.root.join("pack.zip").exists());
    assert!(!f.root.join("解压失败").exists());
}

// H-07 / X-04 / CONTRACT 附录 B：同一 Windows 目录的大小写变体共用改名映射。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn review_f13_directory_mapping_uses_windows_ordinal_case_folding() {
    let f = ArchiveFixture::new();
    fs::write(f.root.join("sub"), b"existing file").unwrap();
    fs::write(
        f.root.join("pack.zip"),
        stored_zip(&[(b"sub/a.txt", b"first"), (b"SUB/a.txt", b"second")]),
    )
    .unwrap();
    let result = f.run(config());
    assert_eq!(fs::read(f.root.join("sub")).unwrap(), b"existing file");
    assert_eq!(
        result.summary.archives_failed, 0,
        "大小写变体不能逃逸目录改名映射"
    );
    assert_eq!(fs::read(f.root.join("sub (1)/a.txt")).unwrap(), b"first");
    assert_eq!(
        fs::read(f.root.join("sub (1)/a (1).txt")).unwrap(),
        b"second"
    );
    assert_eq!(result.summary.archives_ok, 1);
    assert!(!f.root.join("pack.zip").exists());
}

// H-04 / X-05 / X-08：文件全部发布后才在空目录阶段遇真实权限错误；
// 无论 WalkDir 文件顺序如何，inner.zip 都已落盘，外层失败不能丢失它的本次入队。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn review_f16_merge_failure_still_processes_published_nested_archive() {
    let f = ArchiveFixture::new();
    let inner = stored_zip(&[(b"payload.txt", b"nested payload")]);
    let outer = stored_zip(&[(b"inner.zip", &inner), (b"blocked/empty/", b"")]);
    fs::write(f.root.join("outer.zip"), &outer).unwrap();
    let blocked = f.root.join("blocked");
    fs::create_dir(&blocked).unwrap();
    let status = Command::new("icacls.exe")
        .arg(&blocked)
        .args(["/deny", "*S-1-1-0:(AD)"])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "测试前提：真实 ACL 只拒绝在 blocked 下创建目录"
    );
    let result = f.run(config());
    assert!(
        !blocked.join("empty").exists(),
        "真实合入失败前提不得被权限豁免绕过"
    );
    assert_eq!(
        fs::read(f.root.join("解压失败/outer.zip")).unwrap(),
        outer,
        "外层未全部落盘不能删源，原始全部成员仍在隔离包中"
    );
    assert_eq!(result.summary.archives_failed, 1);
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"nested payload",
        "已发布且在范围内的嵌套包必须在同次任务继续处理"
    );
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        !f.root.join("inner.zip").exists(),
        "只有内层自身完整成功才删除内层源包"
    );
}
