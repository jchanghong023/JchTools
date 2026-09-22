//! Real-engine tests; intentionally ignored without an explicit JCHTOOLS_TEST_7ZIP path.
//! package-windows.ps1 runs these after downloading and verifying the bundled engine.
//! 2026-09-18 两工具拆分后：解压用例走 engine::extract_run_at（X 分区），
//! 涉及整理计划的用例先解压再 prepare/apply（C 分区）。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools::{config::*, control::Context, engine};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
struct ArchiveFixture {
    tmp: tempfile::TempDir,
    root: PathBuf,
    input: PathBuf,
    state: PathBuf,
    engine: PathBuf,
}
impl ArchiveFixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let input = tmp.path().join("input");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&input).unwrap();
        let state = tmp.path().join("state");
        let engine = PathBuf::from(
            std::env::var_os("JCHTOOLS_TEST_7ZIP")
                .expect("Set JCHTOOLS_TEST_7ZIP to the full 7z.exe or 7zz absolute path"),
        );
        assert!(engine.is_absolute());
        Self {
            tmp,
            root,
            input,
            state,
            engine,
        }
    }
    fn archive(&self, path: &Path, format: &str) {
        let status = Command::new(&self.engine)
            .current_dir(&self.input)
            .args(["a", "-y", format])
            .arg(path)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success());
    }
    /// 用给定格式链与显式成员打包（例如 `-tbzip2` 压缩一个已有 tar，或 `-txz` 压单个文件）。
    fn pack(&self, archive: &Path, formats: &[&str], sources: &[&str]) {
        let mut args = vec!["a", "-y"];
        args.extend_from_slice(formats);
        let status = Command::new(&self.engine)
            .current_dir(&self.input)
            .args(args)
            .arg(archive)
            .args(sources)
            .status()
            .unwrap();
        assert!(status.success());
    }
    /// 生成编号式分卷组（`-v1k` + 不可压缩数据才会真的切出多卷），返回卷文件数（含 `.001`）。
    /// 成员为 6 个 1500 字节的伪随机文件，合计约 9 KiB，与单卷 1 KiB 明显不同——
    /// 「展开比例」类的断言据此区分「按单卷算分母」与「按实际卷集合合计算分母」。
    fn split_set(&self, archive: &Path) -> usize {
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        let mut random_bytes = std::iter::repeat_with(move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            u8::try_from((seed >> 33) & 0xff).unwrap()
        });
        for i in 0..6 {
            let data: Vec<u8> = random_bytes.by_ref().take(1500).collect();
            fs::write(self.input.join(format!("f{i}.txt")), data).unwrap();
        }
        self.pack(archive, &["-tzip", "-v1k"], &["."]);
        let prefix = format!("{}.", archive.file_name().unwrap().to_string_lossy());
        fs::read_dir(archive.parent().unwrap())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .count()
    }
    /// 「递归解压」一段式运行（X-02）：解压 + 未完全解开的包隔离全部完成。
    fn run(&self, cfg: Config) -> engine::TaskResult {
        engine::extract_run_at(
            &self.root,
            cfg,
            Context::default(),
            &self.state,
            Some(&self.engine),
        )
        .unwrap()
    }
    fn apply(task: &engine::TaskResult) -> engine::TaskResult {
        engine::apply(&task.directory, Context::default()).unwrap()
    }
}
/// 解压用例的默认规则：X-05 之后成功原包一律永久删除、冲突一律只给新成员改名，
/// 处置类配置已整体移除（无删除选项可配），这里只关掉与本文件无关的容量限制（预留 0、展开比例不限）。
fn config() -> Config {
    Config {
        reserve_gib: 0,
        max_ratio: 0,
        ..Config::default()
    }
}
fn organizer() -> Config {
    Config {
        global_delete: DeleteMode::Permanent,
        classify: ClassifyMode::Category,
        ..Config::default()
    }
}
// 覆盖 X-05, H-07：RAR 新式编号由档案头决定，不能把 report1.r00 当成 report2.rar。
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
    fs::write(f.root.join("report1.rar"), first).unwrap();
    fs::write(f.root.join("report2.rar"), last).unwrap();
    fs::write(f.root.join("report1.r00"), b"unrelated old-style sibling").unwrap();
    let result = f.run(config());
    assert_eq!(
        fs::read(f.root.join("report1.r00")).unwrap(),
        b"unrelated old-style sibling"
    );
    assert!(!f.root.join("report1.rar").exists());
    assert!(!f.root.join("report2.rar").exists());
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.deleted, 2);
    assert_eq!(result.summary.errors, 0);
}

// 覆盖 X-05, H-07：看似分卷的名字不等于引擎实际读取过的分卷。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn successful_cleanup_preserves_unused_volume_named_siblings() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"complete payload").unwrap();
    f.archive(&f.root.join("report.part1.rar"), "-tzip");
    fs::write(f.root.join("report.part2.rar"), b"unrelated user file").unwrap();
    fs::create_dir(f.root.join("report.part3.rar")).unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 1);
    assert!(!f.root.join("report.part1.rar").exists());
    assert_eq!(
        fs::read(f.root.join("report.part2.rar")).unwrap(),
        b"unrelated user file"
    );
    assert!(f.root.join("report.part3.rar").is_dir());
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"complete payload"
    );
    assert_eq!(result.summary.deleted, 1);
}

// 覆盖 X-05, H-07：真实分卷只删除引擎报告的数量，不删除断号后的同名尾卷或目录。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn successful_cleanup_preserves_unused_volume_tail_after_gap() {
    let f = ArchiveFixture::new();
    let count = f.split_set(&f.root.join("part.zip"));
    fs::write(f.root.join("part.zip.999"), b"unrelated tail").unwrap();
    fs::create_dir(f.root.join("part.zip.1000")).unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.deleted, u64::try_from(count).unwrap());
    for index in 1..=count {
        assert!(!f.root.join(format!("part.zip.{index:03}")).exists());
    }
    assert_eq!(
        fs::read(f.root.join("part.zip.999")).unwrap(),
        b"unrelated tail"
    );
    assert!(f.root.join("part.zip.1000").is_dir());
    for index in 0..6 {
        assert_eq!(
            fs::read(f.root.join(format!("f{index}.txt"))).unwrap(),
            fs::read(f.input.join(format!("f{index}.txt"))).unwrap()
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
    if makecab.is_file() {
        let status = Command::new(&makecab)
            .current_dir(&f.input)
            .args(["/D", "CompressionType=LZX", "payload.txt"])
            .arg(&cab)
            .status()
            .unwrap();
        assert!(status.success(), "makecab 应能生成 CAB");
    } else {
        // 无 makecab（非 Windows 环境）时退化为 ZIP 字节改名，仍然是「引擎可打开」的容器。
        fs::copy(f.root.join("keep.zip"), &cab).unwrap();
    }
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
    let result = f.run(config());
    assert_eq!(
        result.summary.archives_ok, 2,
        "外层 bz2 与中间 tar 都应处理"
    );
    assert!(
        !f.root.join("bundle.tar").exists() && !f.root.join("bundle.tar.bz2").exists(),
        "中间 tar 按去掉一层压缩后缀命名并作为嵌套包处理，两者都按 X-05 永久删除"
    );
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
    let result = f.run(config());
    assert_eq!(
        result.summary.archives_ok, 2,
        "外层 tgz 与中间 tar 都应处理"
    );
    assert!(
        !f.root.join("bundle.tar").exists() && !f.root.join("bundle.tgz").exists(),
        "tgz 还原出的中间 tar 被当作嵌套包再次处理，两者都按 X-05 永久删除"
    );
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
    let mut org = organizer();
    org.preserve_structure = false;
    let task = engine::prepare_at(&f.root, org.clone(), Context::default(), &f.state).unwrap();
    ArchiveFixture::apply(&task);
    assert!(f.root.join("文档/beta.txt").exists());
    let again = engine::prepare_at(&f.root, org, Context::default(), &f.state).unwrap();
    assert_eq!(again.summary.archives_ok, 0);
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.planned_move, 0);
    assert!(f.root.join("文档/beta.txt").exists());
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

// ===== 手工构造的特殊压缩包样本（重复条目 / GBK 文件名 / 空包 / ZST）=====
// 7-Zip 无法直接创建这些样本（同名条目、原始字节文件名、空包、zstd 帧），因此按
// PKWARE APPNOTE 与 RFC 8878 手工生成字节；全部为存储（未压缩）条目，无需压缩器。

/// 标准 CRC-32（poly 0xEDB8_8320，初值与输出取反）。
fn crc32(bytes: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        table[i as usize] = c;
    }
    let mut crc = !0u32;
    for &b in bytes {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}
/// 生成只含"存储"条目的极简 ZIP（本地文件头 + 中央目录 + EOCD）。
/// 文件名按原始字节写入，不带 UTF-8 标志：可构造 GBK 字节名与同名重复条目。
fn stored_zip(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut offset = 0u32;
    for (name, data) in entries {
        let crc = crc32(data);
        let header_offset = offset;
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes()); // 版本
        out.extend_from_slice(&0u16.to_le_bytes()); // 标志：无 UTF-8 位
        out.extend_from_slice(&0u16.to_le_bytes()); // 方法：存储
        out.extend_from_slice(&[0, 0, 0, 0]); // 时间/日期
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra 长度
        out.extend_from_slice(name);
        out.extend_from_slice(data);
        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&20u16.to_le_bytes()); // 制作版本
        central.extend_from_slice(&20u16.to_le_bytes()); // 需要版本
        central.extend_from_slice(&0u16.to_le_bytes()); // 标志
        central.extend_from_slice(&0u16.to_le_bytes()); // 方法
        central.extend_from_slice(&[0, 0, 0, 0]); // 时间/日期
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        central.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        central.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        central.extend_from_slice(&[0; 8]); // extra 长度/注释长度/起始盘号/内部属性
        central.extend_from_slice(&0u32.to_le_bytes()); // 外部属性
        central.extend_from_slice(&header_offset.to_le_bytes());
        central.extend_from_slice(name);
        offset += u32::try_from(30 + name.len() + data.len()).unwrap();
    }
    let central_offset = offset;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06"); // EOCD
    out.extend_from_slice(&[0, 0, 0, 0]); // 盘号
    out.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&u32::try_from(central.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // 注释长度
    out
}

// 覆盖 X-04
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn zip_with_duplicate_entries_extracts_both_via_auto_rename() {
    // 同名两条目（内容与大小都不同）：7z 以 -aou 解压时会把第二个同名条目自动改名落盘，
    // 两个内容都保留；若引擎改为覆盖，总量校验（13+18 ≠ 13）会失败并隔离原包。
    let f = ArchiveFixture::new();
    fs::write(
        f.root.join("dup.zip"),
        stored_zip(&[
            (b"dup.txt", b"first-10bytes"),
            (b"dup.txt", b"second-payload-16B"),
        ]),
    )
    .unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(
        result.summary.extracted, 2,
        "两个同名条目都应落盘（-aou 自动改名）"
    );
    // dup_1.txt 的改名格式依赖 7-Zip -aou 的实现；引擎版本由 fetch-7zip.ps1 固定，行为稳定。
    assert_eq!(fs::read(f.root.join("dup.txt")).unwrap(), b"first-10bytes");
    assert_eq!(
        fs::read(f.root.join("dup_1.txt")).unwrap(),
        b"second-payload-16B"
    );
}
// 覆盖 X-08
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn gbk_filename_zip_extracts_without_data_loss() {
    // GBK 编码文件名（"报告.txt" 的 GBK 字节）且不带 UTF-8 标志的旧式 zip：
    // 文件名可能与本机代码页不一致而呈乱码，但内容必须完整落盘且路径组件安全。
    let f = ArchiveFixture::new();
    let payload = b"gbk-payload\n!";
    let gbk_name: &[u8] = &[0xB1, 0xA8, 0xB8, 0xE6, b'.', b't', b'x', b't']; // GBK"报告.txt"
    fs::write(f.root.join("gbk.zip"), stored_zip(&[(gbk_name, payload)])).unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.extracted, 1);
    let extracted: Vec<_> = fs::read_dir(&f.root)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("txt")))
        .collect();
    assert_eq!(
        extracted.len(),
        1,
        "应恰好落盘一个 .txt 文件（名字可能呈乱码）"
    );
    assert_eq!(fs::read(&extracted[0]).unwrap(), payload);
}
// 覆盖 X-05
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn empty_zip_archive_extracts_cleanly() {
    // 只有 EOCD 的空 zip：合法归档，零成员零字节，按成功解压处理。
    let f = ArchiveFixture::new();
    fs::write(f.root.join("empty.zip"), stored_zip(&[])).unwrap();
    let cfg = config();
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.extracted, 0);
    assert!(
        !f.root.join("empty.zip").exists(),
        "零成员的空包同样算完整解开：原包按 X-05 永久删除"
    );
    assert_eq!(result.summary.deleted, 1, "空包删除同样计入删除计数");
}
// 覆盖 X-05
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn empty_7z_with_only_a_directory_entry_restores_the_directory() {
    // 只含一个空目录条目的 7z：解压后应还原出目录本身。
    let f = ArchiveFixture::new();
    fs::create_dir(f.input.join("空目录")).unwrap();
    f.pack(&f.root.join("empty.7z"), &["-t7z"], &["空目录"]);
    let cfg = config();
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.extracted, 0);
    assert!(
        f.root.join("空目录").is_dir(),
        "仅目录条目的 7z 也应还原出目录"
    );
    assert!(
        !f.root.join("empty.7z").exists(),
        "只含目录条目的包同样完整落盘（含空目录）：原包按 X-05 永久删除"
    );
}
// 覆盖 X-03
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn zstd_stream_archive_extracts_with_content() {
    // 7-Zip 只解不建 .zst，样本按 RFC 8878 手工构造：
    // magic + 单段帧头（1 字节帧内容大小）+ 一个"最后一块、原始块"，内容直接内联。
    let f = ArchiveFixture::new();
    let payload = b"zst payload here\n";
    let mut zst = b"\x28\xB5\x2F\xFD".to_vec(); // Zstandard magic
    zst.push(0x20); // 单段帧 + 帧内容大小 1 字节
    zst.push(u8::try_from(payload.len()).unwrap()); // Frame_Content_Size
    let block_header = 1u32 | u32::try_from(payload.len()).unwrap() << 3; // 最后一块 · 原始块 · 大小
    zst.extend_from_slice(&block_header.to_le_bytes()[..3]);
    zst.extend_from_slice(payload);
    fs::write(f.root.join("payload.txt.zst"), zst).unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(fs::read(f.root.join("payload.txt")).unwrap(), payload);
}

// 平台门禁原因：触发条件本身是 Windows 路径长度语义（>260 字符 + LongPathsEnabled 默认 0 时
// 裸 Win32 调用失败），且植入隐藏属性需要 SetFileAttributesW，均无法在非 Windows 复现。
// 覆盖 S-04(扫描隐藏), X-08
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
#[cfg(windows)]
fn long_path_hidden_member_is_stripped_and_archive_completes() {
    // 管线级行为锚点：>260 字符且存储了隐藏属性的成员（7z 会还原属性），在默认
    // include_hidden=false 下必须被剥离成可见文件，且 complete 不得被误置——
    // 否则成员成扫描不可见的影子文件而原包被删除，用户视角即内容丢失。
    // 注意：本测试经 extract_run_at 的 root 已 canonicalize（verbatim），锁的是管线级契约；
    // 「普通路径 + 超长」的原始缺陷形态由 archive.rs 内对 normalize 的直测锚点锁定。
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
    let f = ArchiveFixture::new();
    let mut dir = f.input.clone();
    let seg = "l".repeat(40);
    for i in 0..7 {
        dir.push(format!("{seg}{i}"));
    } // 相对路径 ≈300 字符，远超 260
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("deep-hidden.txt");
    fs::write(&file, b"deep hidden payload").unwrap();
    // 测试自身植入属性必须走 \\?\ 前缀：LongPathsEnabled=0 的机器上裸调用同样会失败
    //（这正是本次整改所针对的产品缺陷，前置若用裸调用连夹具都搭不起来）。
    let mut wide: Vec<u16> = r"\\?\".encode_utf16().collect();
    wide.extend(file.as_os_str().encode_wide());
    wide.push(0);
    // SAFETY: wide 是以 NUL 结尾的 UTF-16 verbatim 路径；调用只读取该缓冲区。
    let ok = unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) };
    assert!(ok != 0, "测试前置：植入隐藏属性失败");
    let meta = fs::symlink_metadata(&file).unwrap();
    assert_ne!(meta.file_attributes() & 2, 0, "测试前置：隐藏属性应已植入");
    f.archive(&f.root.join("deep.7z"), "-t7z");
    // 隐藏开关仍按 S-04 保留给用户显式调整：本用例验证关闭时成员属性被剥离（默认
    // 开启包含隐藏文件，属性本就无需剥离）。
    let cfg = Config {
        include_hidden: false,
        ..config()
    };
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0, "超长路径包不得计入失败");
    assert_eq!(result.summary.extracted, 1);
    let rel = dir.strip_prefix(&f.input).unwrap();
    let dest = f.root.join(rel).join("deep-hidden.txt");
    let dest_meta = fs::symlink_metadata(&dest)
        .unwrap_or_else(|e| panic!("成员应按原相对路径落盘 {dest:?}: {e}"));
    assert_eq!(
        dest_meta.file_attributes() & 2,
        0,
        "隐藏属性必须被剥离，否则成员成扫描不可见的影子文件"
    );
    assert!(
        !f.root.join("deep.7z").exists() && !f.root.join("解压失败/deep.7z").exists(),
        "complete 为真时原包按 X-05 永久删除；complete 被误置 false 的包会留在「解压失败」"
    );
}

// 覆盖 X-05, S-01（回归 2026-09-19 返工：zip 的条目级 Volume Index 无条件输出，
// 单卷包也全为 0，不能作分卷佐证；佐证必须取档案级属性块）。真实引擎下完整独立的
// report.zip 旁边残留的同主干 report.z01 是无辜文件：成功路径只能删除主体自身，
// 不得移动、改名或删除这一旁观文件。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn real_engine_standalone_zip_deletes_source_and_keeps_stale_z_sibling() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("one.txt"), b"real standalone payload").unwrap();
    fs::write(f.input.join("two.txt"), b"second member").unwrap();
    f.archive(&f.root.join("report.zip"), "-tzip");
    fs::write(
        f.root.join("report.z01"),
        b"stale fragment of a replaced split set",
    )
    .unwrap();
    let cfg = config();
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        f.root.join("one.txt").exists() && f.root.join("two.txt").exists(),
        "成员应正常解压落盘"
    );
    assert!(
        !f.root.join("report.zip").exists(),
        "成功原包按 X-05 永久删除"
    );
    assert!(
        f.root.join("report.z01").exists(),
        "真实引擎下无档案级多卷佐证的同主干 .z01 是无辜文件，不得被移动或删除"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "删除集合只含主体自身：无辜 .z01 不得计入"
    );
}

// 覆盖 X-08, S-01（回归 2026-09-19：注释内 } 行绕过注释跳过——真实引擎变体）。
// 7-Zip 把档案注释以 {...} 原样输出，注释内容可含 } 行；若 } 只结束注释模式
// 不结束档案头块，其后的伪造键会被当作头块键解析。真实输出中 Comment 是头块
// 最后一个字段，} 结束注释时一并结束头块是 fail-closed 的。
// 这里的可观察后果是展开比例的分母：伪造键若能证实「多卷」，旁置的大号同主干
// 兄弟卷就会被算进分母，比例上限随之放宽，恶意档案可借此绕过 X-08 的展开防护。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn real_engine_brace_in_comment_cannot_inflate_ratio_denominator() {
    fn add_zip_comment(path: &std::path::Path, comment: &[u8]) {
        let mut data = fs::read(path).unwrap();
        let signature = [0x50u8, 0x4b, 0x05, 0x06];
        let eocd = data
            .windows(4)
            .rposition(|window| window == signature)
            .expect("zip EOCD 签名");
        assert_eq!(
            data.len(),
            eocd + 22,
            "测试前置：期望无既有注释的 EOCD 结尾"
        );
        let length = u16::try_from(comment.len()).expect("注释长度须在 u16 内");
        data[eocd + 20] = (length & 0xff) as u8;
        data[eocd + 21] = (length >> 8) as u8;
        data.extend_from_slice(comment);
        fs::write(path, data).unwrap();
    }
    let f = ArchiveFixture::new();
    // 可压缩内容：声明总量 3 × 4096 = 12 KiB，实际包体只有几百字节。
    for i in 0..3 {
        fs::write(f.input.join(format!("z{i}.txt")), vec![0u8; 4096]).unwrap();
    }
    f.archive(&f.root.join("report.zip"), "-tzip");
    add_zip_comment(
        &f.root.join("report.zip"),
        b"benign first line
}
Volume Index = 0
Volumes = 9
",
    );
    // 旁置的大号同主干兄弟卷：分母只应计主体与「有佐证的」分卷，无佐证不得计入。
    fs::write(f.root.join("report.z01"), vec![0u8; 1_000_000]).unwrap();
    let packed = fs::metadata(f.root.join("report.zip")).unwrap().len();
    assert!(packed * 2 < 12_288, "测试前置：包体应远小于声明总量");
    let cfg = Config {
        max_ratio: 2,
        ..config()
    };
    let result = f.run(cfg);
    assert_eq!(
        result.summary.archives_failed, 1,
        "分母只应计主体（{packed} 字节）：12 KiB 声明量必然超过比例上限"
    );
    assert!(
        f.root.join("解压失败/report.zip").is_file(),
        "判超限的包移入「解压失败」（X-06）"
    );
}

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

// 覆盖 H-06（回归：目标位于既有 Git 目录树内的成员必须跳过，该树整树不动）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn member_targeting_existing_git_tree_is_skipped_and_tree_untouched() {
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
        "目标位于 Git 目录树内的成员必须跳过（H-06：整树排除、不解压）"
    );
    assert_eq!(
        result.summary.archives_ok, 0,
        "有成员未解开时不得宣称成功（X-06）"
    );
    assert!(
        f.root.join("解压失败/one.zip").is_file(),
        "未完全解开的原包移入「解压失败」且不删除（X-06）"
    );
}

// 覆盖 H-06（回归：压缩包内某目录含 .git 时，该子树连同 .git 的兄弟条目整树不解出）
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn git_subtree_members_are_skipped_as_a_whole() {
    let f = ArchiveFixture::new();
    // project/ 直接含 .git：project 整树（含 .git 的兄弟文件）都不得解出；
    // normal.txt 不属于该子树，照常解压。
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
    fs::write(f.input.join("normal.txt"), b"normal member").unwrap();
    f.pack(
        &f.root.join("one.zip"),
        &["-tzip"],
        &["project", "normal.txt"],
    );
    // 显式清空排除规则：H-06 的 Git 排除不是「默认排除串碰巧命中」，不得依赖用户
    // 可改的 glob 规则（.git/** 在默认串里，但用户可自行删除）。
    let cfg = Config {
        exclusions: String::new(),
        ..config()
    };
    let result = f.run(cfg);
    assert!(
        !f.root.join("project").exists(),
        "含 .git 的目录及全部后代必须整树排除（H-06：不解压）"
    );
    assert!(
        !f.root.join("project/README.md").exists(),
        ".git 的兄弟条目同样属于被排除的子树（H-06：该目录及全部后代）"
    );
    assert!(
        !f.root.join("project/src/notes.txt").exists(),
        ".git 子树的后代不得解出（H-06）"
    );
    assert_eq!(
        fs::read(f.root.join("normal.txt")).unwrap(),
        b"normal member",
        "不属于 Git 子树的成员照常解压"
    );
    assert_eq!(
        result.summary.archives_ok, 0,
        "有成员被跳过时不得宣称已全部解压（X-06/H-04）"
    );
    assert!(
        f.root.join("解压失败/one.zip").is_file(),
        "未完全解开的原包移入「解压失败」且不删除（X-06）"
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
        reserve_gib: 1 << 20,
        ..config()
    };
    let result =
        engine::extract_run_at(&f.root, cfg, Context::default(), &f.state, Some(&f.engine));
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
        result.summary.archives_ok, 3,
        "预置 tar、tar.gz 与本次解出的嵌套 tar 各处理一次，不得反复入队"
    );
    assert_eq!(
        result.summary.deleted, 3,
        "三个包各自完整解开：原包都按 X-05 永久删除"
    );
    assert!(
        !f.root.join("bundle.tar").exists()
            && !f.root.join("bundle.tar.gz").exists()
            && !f.root.join("bundle (1).tar").exists(),
        "无论成员以哪个名字落盘，它都是被处理的包并在成功后删除"
    );
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"stream payload\n"
    );
    assert!(
        f.root.join("payload (1).txt").is_file(),
        "嵌套包成员与既有文件冲突时改名落盘（X-04）"
    );
}
