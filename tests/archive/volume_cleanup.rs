use super::*;

// 覆盖 X-05/X-10/H-07：精确 part 族卷集完整删源，非精确旧式后缀旁观文件保留。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn successful_cleanup_uses_rar_header_volume_naming() {
    let f = ArchiveFixture::new();
    // 合法 RAR4 空分卷：主头含 Volume/NewVolName，头 CRC 与末卷编号均完整。
    let first = [
        0x52, 0x61, 0x72, 0x21, 0x1a, 0x07, 0x00, 0x5a, 0x6e, 0x73, 0x11, 0x01, 0x0d, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0xed, 0x45, 0x7b, 0x09, 0x00, 0x09, 0x00, 0x00, 0x00,
    ];
    let last = [
        0x52, 0x61, 0x72, 0x21, 0x1a, 0x07, 0x00, 0x19, 0x7a, 0x73, 0x11, 0x00, 0x0d, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0xa7, 0x7b, 0x08, 0x00, 0x09, 0x00, 0x01, 0x00,
    ];
    fs::write(f.root.join("report.part1.rar"), first).unwrap();
    fs::write(f.root.join("report.part2.rar"), last).unwrap();
    fs::write(
        f.root.join("report.part1.r000"),
        b"unrelated old-style sibling",
    )
    .unwrap();
    let result = f.run(config());
    assert_eq!(
        fs::read(f.root.join("report.part1.r000")).unwrap(),
        b"unrelated old-style sibling"
    );
    assert!(!f.root.join("report.part1.rar").exists());
    assert!(!f.root.join("report.part2.rar").exists());
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.deleted, 2);
    assert_eq!(result.summary.errors, 0);
}

// 覆盖 X-05/X-09/X-10：备份后缀和同名目录不是实际 part 族文件，不得纳入删源集合。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn successful_cleanup_preserves_unused_volume_named_siblings() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"complete payload").unwrap();
    f.archive(&f.root.join("report.part1.rar"), "-tzip");
    fs::write(f.root.join("report.part2.rar.bak"), b"unrelated user file").unwrap();
    fs::create_dir(f.root.join("report.part3.rar")).unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 1);
    assert!(!f.root.join("report.part1.rar").exists());
    assert_eq!(
        fs::read(f.root.join("report.part2.rar.bak")).unwrap(),
        b"unrelated user file"
    );
    assert!(f.root.join("report.part3.rar").is_dir());
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"complete payload"
    );
    assert_eq!(result.summary.deleted, 1);
}

// 覆盖 X-05/X-09/X-10：非规范补零后缀旁观项不动；精确卷族断号整组失败。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn successful_cleanup_preserves_unused_volume_tail_after_gap() {
    let f = ArchiveFixture::new();
    let count = f.split_set(&f.root.join("part.zip"));
    fs::write(f.root.join("part.zip.01000"), b"unrelated tail").unwrap();
    fs::create_dir(f.root.join("part.zip.01001")).unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.deleted, u64::try_from(count).unwrap());
    for index in 1..=count {
        assert!(!f.root.join(format!("part.zip.{index:03}")).exists());
    }
    assert_eq!(
        fs::read(f.root.join("part.zip.01000")).unwrap(),
        b"unrelated tail"
    );
    assert!(f.root.join("part.zip.01001").is_dir());
    for index in 0..6 {
        assert_eq!(
            fs::read(f.root.join(format!("f{index}.txt"))).unwrap(),
            fs::read(f.input.join(format!("f{index}.txt"))).unwrap()
        );
    }
    let f = ArchiveFixture::new();
    let count = f.split_set(&f.root.join("gap.zip"));
    assert!(count >= 3);
    fs::remove_file(f.root.join("gap.zip.002")).unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 0);
    assert_eq!(result.summary.archives_failed, 1, "断号整组计一个失败包");
    assert_eq!(result.summary.deleted, 0, "断号不得删源");
    for index in (1..=count).filter(|index| *index != 2) {
        assert!(f
            .root
            .join(format!("解压失败/gap.zip.{index:03}"))
            .is_file());
        assert!(!f.root.join(format!("gap.zip.{index:03}")).exists());
    }
}
// 覆盖 X-02/X-05/X-06/X-09/X-10/S-01：真实 7-Zip 的 .999 → .1000 边界，
// 完整包验证成品并删全部源卷，损坏/断号包整组保留且隔离不覆盖。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn numbered_volume_growth_real_engine_success_and_failure() {
    for scenario in ["complete", "corrupt", "gap"] {
        let f = ArchiveFixture::new();
        let payload: Vec<u8> = (0..1_024_000)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect();
        fs::write(f.input.join("payload.txt"), &payload).unwrap();
        f.pack(
            &f.root.join("资料.7z"),
            &["-t7z", "-mx0", "-v1k"],
            &["payload.txt"],
        );
        let count = fs::read_dir(&f.root).unwrap().count();
        assert_eq!(count, 1001, "夹具必须跨过 .999/.1000，且有完整末卷");
        if scenario == "corrupt" {
            fs::write(f.root.join("资料.7z.1001"), b"corrupt final volume").unwrap();
        } else if scenario == "gap" {
            fs::remove_file(f.root.join("资料.7z.1000")).unwrap();
        }
        let source: Vec<_> = (1..=count)
            .filter(|index| scenario != "gap" || *index != 1000)
            .map(|index| {
                let name = format!("资料.7z.{index:03}");
                let bytes = fs::read(f.root.join(&name)).unwrap();
                (name, bytes)
            })
            .collect();
        fs::write(f.root.join("资料.7z.01000"), b"noncanonical padding").unwrap();
        fs::write(f.root.join("资料.7z.1000.bak"), b"backup").unwrap();
        if scenario != "complete" {
            fs::create_dir(f.root.join("解压失败")).unwrap();
            fs::write(f.root.join("解压失败/资料.7z.1001"), b"earlier failure").unwrap();
        }
        let result = f.run(config());
        if scenario == "complete" {
            assert_eq!(result.summary.archives_ok, 1);
            assert_eq!(result.summary.archives_failed, 0);
            assert_eq!(result.summary.errors, 0);
            assert_eq!(result.summary.deleted, 1001);
            assert_eq!(fs::read(f.root.join("payload.txt")).unwrap(), payload);
            assert!(!f.root.join("解压失败").exists());
        } else {
            assert_eq!(result.summary.archives_ok, 0, "{scenario}");
            assert_eq!(result.summary.archives_failed, 1, "{scenario}");
            assert_eq!(result.summary.deleted, 0, "{scenario}");
            assert!(!f.root.join("payload.txt").exists(), "失败不得合入成品");
            assert_eq!(
                fs::read(f.root.join("解压失败/资料.7z.1001")).unwrap(),
                b"earlier failure"
            );
            for (name, bytes) in &source {
                let target = name.replacen("资料.", "资料 (1).", 1);
                assert_eq!(
                    fs::read(f.root.join("解压失败").join(target)).unwrap(),
                    *bytes,
                    "失败卷须完整保留：{name}"
                );
            }
        }
        for (name, _) in &source {
            assert!(!f.root.join(name).exists(), "源卷不得漏处理：{name}");
        }
        assert_eq!(
            fs::read(f.root.join("资料.7z.01000")).unwrap(),
            b"noncanonical padding"
        );
        assert_eq!(
            fs::read(f.root.join("资料.7z.1000.bak")).unwrap(),
            b"backup"
        );
    }
}

// 覆盖 X-01, X-09：白名单之外的容器即使能被引擎打开也完全不碰（真实引擎）。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn non_whitelisted_containers_stay_untouched_beside_a_real_archive() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"payload for the only archive").unwrap();
    f.archive(&f.root.join("keep.zip"), "-tzip");
    // 真 CAB（Windows 自带 makecab；7-Zip 只能读不能写 cab）：证明「不碰」不是打不开。
    let makecab = PathBuf::from(std::env::var_os("SYSTEMROOT").unwrap_or_default())
        .join("System32")
        .join("makecab.exe");
    let cab = f.root.join("visproww.cab");
    // 缺少 makecab 属于夹具准备失败，不能用改后缀的 ZIP 冒充真实 CAB 验收。
    let status = Command::new(&makecab)
        .current_dir(&f.input)
        .args(["/D", "CompressionType=LZX", "payload.txt"])
        .arg(&cab)
        .status()
        .unwrap();
    assert!(status.success(), "makecab 应能生成 CAB");
    let cab_bytes = fs::read(&cab).unwrap();
    // 测试前提：引擎确实能打开这个 CAB——白名单判定不依赖内容类型探测。
    let listed = Command::new(&f.engine)
        .arg("l")
        .arg("-ba")
        .arg("--")
        .arg(&cab)
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "测试前提：7-Zip 能打开该 CAB（否则本用例证明不了「能打开也不解压」）"
    );
    // 其余容器用合法 ZIP 字节改名：7-Zip 同样能打开，语义上仍不是用户要整理的归档。
    let archive_bytes = fs::read(f.root.join("keep.zip")).unwrap();
    let containers = [
        "windows.iso",
        "boot.wim",
        "install.esd",
        "legacy.lzh",
        "archive.cpio",
        "report.docx",
        "setup.msi",
        "setup.exe",
        "app.apk",
        "library.jar",
        "wheel.whl",
        "book.epub",
        "addon.crx",
        "styles.xpi",
    ];
    for name in containers {
        fs::write(f.root.join(name), &archive_bytes).unwrap();
    }
    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 1, "只有白名单内的包应被处理");
    assert_eq!(result.summary.deleted, 1);
    assert_eq!(result.summary.errors, 0);
    assert!(
        !f.root.join("keep.zip").exists(),
        "白名单内的包完整成功后按 X-05 删除"
    );
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"payload for the only archive"
    );
    assert_eq!(
        fs::read(f.root.join("visproww.cab")).unwrap(),
        cab_bytes,
        "真实 CAB 必须原样保留：安装介质不是待整理的压缩包"
    );
    for name in containers {
        assert_eq!(
            fs::read(f.root.join(name)).unwrap(),
            archive_bytes,
            "{name} 必须原样保留：不解压、不删除、不移动"
        );
    }
    assert!(
        !f.root.join("解压失败").exists(),
        "白名单外的文件不是失败包，不得进隔离目录"
    );
}
