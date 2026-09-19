//! Real-engine tests; intentionally ignored without an explicit JCHTOOLS_TEST_7ZIP path.
//! package-windows.ps1 runs these after downloading and verifying the bundled engine.
//! 2026-09-18 两工具拆分后：解压用例走 engine::extract_run_at（X 分区），
//! 涉及整理计划的用例先解压再 prepare/apply（C 分区）。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::FailRecycle;
use jchtools::{config::*, control::Context, engine};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
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
    /// 「递归解压」一段式运行（X-02）：解压 + 原包处置/隔离全部完成。
    fn run(&self, cfg: Config) -> engine::TaskResult {
        engine::extract_run_at(
            &self.root,
            cfg,
            Context::default(),
            &self.state,
            Some(&self.engine),
            Arc::new(common::MoveRecycle::new(self.state.join("bin"))),
        )
        .unwrap()
    }
    fn apply(task: &engine::TaskResult) -> engine::TaskResult {
        engine::apply_with(&task.directory, Context::default(), Arc::new(FailRecycle)).unwrap()
    }
}
fn config() -> Config {
    Config {
        reserve_gib: 0,
        global_delete: DeleteMode::Permanent,
        archive_delete: ArchiveDispose::Keep,
        extract_conflict: ConflictPolicy::KeepBoth,
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
// 覆盖 X-03
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
}
// 覆盖 C-02, C-01：先解压（工具一）再分析（工具二，只读），两工具分工后仍能衔接出删除计划。
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
    let plan = engine::prepare_at(&f.root, organizer(), Context::default(), &f.state).unwrap();
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
// 覆盖 X-05
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn successful_source_is_disposed_by_policy() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("a.txt"), b"one").unwrap();
    f.archive(&f.root.join("one.zip"), "-tzip");
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Permanent;
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        !f.root.join("one.zip").exists(),
        "成功原包按处置策略移除（X-05）"
    );
    assert!(f.root.join("a.txt").exists());
}
// 覆盖 X-05：默认处置是回收站；回收实现注入 MoveRecycle，落 bin 即视为已回收。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn successful_source_defaults_to_recycle() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("a.txt"), b"one").unwrap();
    f.archive(&f.root.join("one.zip"), "-tzip");
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Recycle;
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_ok, 1);
    assert!(!f.root.join("one.zip").exists());
    let bin = f.state.join("bin");
    assert!(
        fs::read_dir(&bin).unwrap().count() >= 1,
        "成功原包应进入（mock）回收站"
    ); // mock 未实现 bin_count，平台层按「回收未验证」诚实记账（S-06）：计入 deleted 而非 recycled。
    assert_eq!(
        result.summary.recycled + result.summary.deleted,
        1,
        "处置计数入账（回收或回收未验证）"
    );
}
// 覆盖 X-04, X-06
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn skipped_conflict_moves_original_to_quarantine() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("a.txt"), b"new").unwrap();
    fs::write(f.root.join("a.txt"), b"old").unwrap();
    f.archive(&f.root.join("one.zip"), "-tzip");
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Permanent;
    cfg.extract_conflict = ConflictPolicy::Skip;
    let result = f.run(cfg);
    assert!(
        result.summary.archives_quarantined >= 1,
        "跳过未完全解开应计入隔离数"
    );
    assert!(
        !f.root.join("one.zip").exists(),
        "跳过策略下原包不得留在原位置"
    );
    assert!(
        f.root.join("解压失败").join("one.zip").exists(),
        "未完全解开的原包移入「解压失败」（X-06）"
    );
    assert_eq!(fs::read(f.root.join("a.txt")).unwrap(), b"old");
}
// 覆盖 X-06, X-05
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn corrupt_archive_is_quarantined_and_logged() {
    let f = ArchiveFixture::new();
    fs::write(f.root.join("broken.zip"), b"not a zip").unwrap();
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Permanent;
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
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Permanent;
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
        f.root.join("bundle.tar").is_file(),
        "中间成员按去掉一层压缩后缀命名，保留策略下留在原地"
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
        f.root.join("bundle.tar").is_file(),
        "tgz 应还原为中间 tar 而不是再次解压"
    );
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"stream payload\n"
    );
}
// 覆盖 X-04
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn deleted_same_name_file_does_not_abort_a_later_stream_archive() {
    // 回归：根目录里的 bundle.tar 先被解压并（按授权）删除，随后 bundle.tar.gz 解出的成员名同样是
    // bundle.tar。早期实现会在“同名同内容”查找里读取这个刚被删除的路径，把整包解压判为失败。
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"stream payload\n").unwrap();
    f.pack(&f.input.join("bundle.tar"), &["-ttar"], &["payload.txt"]);
    fs::copy(f.input.join("bundle.tar"), f.root.join("bundle.tar")).unwrap();
    f.pack(&f.root.join("bundle.tar.gz"), &["-tgzip"], &["bundle.tar"]);
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Permanent;
    let result = f.run(cfg);
    assert_eq!(
        result.summary.archives_failed, 0,
        "同名文件被删除后，后续流式压缩包仍应解压成功"
    );
    assert!(result.summary.archives_ok >= 2);
    assert_eq!(
        fs::read(f.root.join("payload.txt")).unwrap(),
        b"stream payload\n"
    );
}
// 覆盖 X-04, C-05：先解压（等量包冲突裁决）再整理（归类），两阶段各自幂等。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn equal_content_archives_dispose_and_classify_is_stable() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("beta.txt"), b"beta member\n").unwrap();
    f.archive(&f.root.join("base.zip"), "-tzip");
    f.archive(&f.root.join("base.tar"), "-ttar");
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Permanent;
    cfg.extract_conflict = ConflictPolicy::Largest;
    let extracted = f.run(cfg);
    assert_eq!(
        extracted.summary.archives_ok, 2,
        "both archives should extract and dispose"
    );
    assert!(!f.root.join("base.zip").exists());
    assert!(!f.root.join("base.tar").exists());
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
// 覆盖 X-05, C-05
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn kept_source_rerun_does_not_recreate_classified_duplicate() {
    // 源包处置=保留时，归类搬走内容后再次解压不得在源目录旁重新落盘同内容文件。
    let f = ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"), b"payload for kept source\n").unwrap();
    f.archive(&f.root.join("kept.zip"), "-tzip");
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Keep;
    let extracted = f.run(cfg.clone());
    assert_eq!(extracted.summary.archives_ok, 1);
    assert!(
        f.root.join("kept.zip").exists(),
        "处置=保留：原包留在原位置"
    );
    assert!(f.root.join("payload.txt").exists());
    let mut org = organizer();
    org.preserve_structure = false;
    let task = engine::prepare_at(&f.root, org.clone(), Context::default(), &f.state).unwrap();
    ArchiveFixture::apply(&task);
    assert!(!f.root.join("payload.txt").exists());
    assert!(f.root.join("文档/payload.txt").exists());
    let again = f.run(cfg);
    assert_eq!(again.summary.archives_ok, 1, "保留的源包可再次完整解压");
    assert!(
        !f.root.join("payload.txt").exists(),
        "kept archive must not rewrite classified content beside itself"
    );
    assert!(f.root.join("文档/payload.txt").exists());
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
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Permanent;
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.extracted, 0);
    assert!(
        !f.root.join("empty.zip").exists(),
        "空包成功解压后源包同样按规则处理"
    );
}
// 覆盖 X-05
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn empty_7z_with_only_a_directory_entry_restores_the_directory() {
    // 只含一个空目录条目的 7z：解压后应还原出目录本身。
    let f = ArchiveFixture::new();
    fs::create_dir(f.input.join("空目录")).unwrap();
    f.pack(&f.root.join("empty.7z"), &["-t7z"], &["空目录"]);
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Permanent;
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.extracted, 0);
    assert!(
        f.root.join("空目录").is_dir(),
        "仅目录条目的 7z 也应还原出目录"
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
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Permanent;
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
        !f.root.join("deep.7z").exists(),
        "成员可见后原包应按规则处置（complete 未被误置 false）"
    );
}

// 覆盖 X-05（成功的编号式分卷组整组处置）：真分卷集解压成功后所有卷一并回收，
// 目录不残留压缩包；与 tests/split.rs 的 stale_* 回归共同锁定分卷组语义
// （单卷包旁的无佐证同主干文件不得处置，佐证为真/命名精确的分卷组必须整组处置）。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn successful_numbered_split_set_disposes_every_volume() {
    let f = ArchiveFixture::new();
    // 伪随机（LCG）数据不可压缩，确保 -v1k 真正切出多个卷（可压缩数据会压进单卷）。
    let mut seed = 0x2545_F491_4F6C_DD1D_u64;
    let mut random_bytes = std::iter::repeat_with(move || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        u8::try_from((seed >> 33) & 0xff).unwrap()
    });
    for i in 0..6 {
        let data: Vec<u8> = random_bytes.by_ref().take(1500).collect();
        fs::write(f.input.join(format!("f{i}.txt")), data).unwrap();
    }
    f.pack(&f.root.join("split.zip"), &["-tzip", "-v1k"], &["."]);
    let volumes = fs::read_dir(&f.root)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("split.zip."))
        .count();
    assert!(volumes >= 2, "应生成至少两个分卷，实际 {volumes}");
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Recycle;
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0, "分卷集不得计入失败");
    assert_eq!(result.summary.archives_ok, 1, "分卷组按一个包计数");
    for i in 0..6 {
        assert!(
            f.root.join(format!("f{i}.txt")).exists(),
            "成员 f{i}.txt 应解压落盘"
        );
    }
    let left = fs::read_dir(&f.root)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("split.zip."))
        .count();
    assert_eq!(
        left, 0,
        "成功分卷组的所有卷都不得残留在目录里（X-05 与 X 分区总体约束）"
    );
    let recycled = fs::read_dir(f.state.join("bin"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains("split.zip."))
        .count();
    assert_eq!(recycled, volumes, "全部分卷应进入回收站");
}

// 覆盖 X-05, S-01（回归 2026-09-19 返工：zip 的条目级 Volume Index 无条件输出，
// 单卷包也全为 0，不能作分卷佐证；佐证必须取档案级属性块）。真实引擎下完整独立的
// report.zip 旁边残留的同主干 report.z01 不得随包处置（回收/永久删除均不可）。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn real_engine_standalone_zip_keeps_stale_z_sibling() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("one.txt"), b"real standalone payload").unwrap();
    fs::write(f.input.join("two.txt"), b"second member").unwrap();
    f.archive(&f.root.join("report.zip"), "-tzip");
    fs::write(
        f.root.join("report.z01"),
        b"stale fragment of a replaced split set",
    )
    .unwrap();
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Recycle;
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        f.root.join("one.txt").exists() && f.root.join("two.txt").exists(),
        "成员应正常解压落盘"
    );
    assert!(
        !f.root.join("report.zip").exists(),
        "成功原包按 X-05 移入回收站"
    );
    assert!(
        f.root.join("report.z01").exists(),
        "真实引擎下无档案级多卷佐证的同主干 .z01 是无辜文件，不得随包处置"
    );
    let bin = f.state.join("bin");
    let recycled: Vec<String> = fs::read_dir(&bin)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        recycled.iter().any(|n| n.ends_with("report.zip")),
        "原包应进入回收：{recycled:?}"
    );
    assert!(
        !recycled.iter().any(|n| n.contains("report.z01")),
        "无辜 .z01 不得进入回收：{recycled:?}"
    );
}

// 覆盖 X-05, S-01（回归 2026-09-19：crafted 档案注释伪造多卷佐证——真实引擎形态）。
// zip 档案注释由 7-Zip 在 -slt 档案头块内以 {...} 原样逐行输出；把
// `Volumes = 9` 写进注释若能骗过佐证判定，恶意档案即可让同主干无辜 .z01 随成功
// 包处置（Permanent 模式即永久删除）。注释块内的键必须被忽略。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn real_engine_forged_comment_cannot_enable_sweep() {
    /// 给 zip 附加档案注释：注释是 EOCD 固定 22 字节之后的尾部字节，长度写在
    /// EOCD 偏移 20..22（小端）。只改注释不改其余结构，7-Zip 照常列出与解压。
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
    fs::write(f.input.join("one.txt"), b"payload").unwrap();
    f.archive(&f.root.join("report.zip"), "-tzip");
    add_zip_comment(
        &f.root.join("report.zip"),
        b"benign first line
Volumes = 9
Volume Index = 0
Multivolume = +
",
    );
    fs::write(f.root.join("report.z01"), b"innocent bystander").unwrap();
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Permanent;
    let result = f.run(cfg);
    assert_eq!(
        result.summary.archives_failed, 0,
        "带注释的合法 zip 不得失败"
    );
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        f.root.join("one.txt").exists(),
        "成员应正常解压落盘（注释不影响解压）"
    );
    assert!(!f.root.join("report.zip").exists(), "成功原包按 X-05 处置");
    assert!(
        f.root.join("report.z01").exists(),
        "档案注释里伪造的多卷键不得让无辜 .z01 被永久删除"
    );
}

// 覆盖 X-05, S-01（回归 2026-09-19：注释内 } 行绕过注释跳过——真实引擎变体）。
// 7-Zip 把档案注释以 {...} 原样输出，注释内容可含 } 行；若 } 只结束注释模式
// 不结束档案头块，其后的伪造键会被当作头块键解析。真实输出中 Comment 是头块
// 最后一个字段，} 结束注释时一并结束头块是 fail-closed 的。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn real_engine_brace_in_comment_cannot_enable_sweep() {
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
    fs::write(f.input.join("one.txt"), b"payload").unwrap();
    f.archive(&f.root.join("report.zip"), "-tzip");
    add_zip_comment(
        &f.root.join("report.zip"),
        b"benign first line
}
Volume Index = 0
Volumes = 9
",
    );
    fs::write(f.root.join("report.z01"), b"innocent bystander").unwrap();
    let mut cfg = config();
    cfg.archive_delete = ArchiveDispose::Permanent;
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        f.root.join("report.z01").exists(),
        "注释内 }} 行后的伪造键不得让无辜 .z01 被永久删除"
    );
}
