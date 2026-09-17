//! Real-engine tests; intentionally ignored without an explicit JCHTOOLS_TEST_7ZIP path.
//! package-windows.ps1 runs these after downloading and verifying the bundled engine.
mod common;
use common::FailRecycle;
use jchtools::{config::*,control::Context,engine};
use std::{fs,path::{Path,PathBuf},process::Command,sync::Arc};
struct ArchiveFixture { _tmp:tempfile::TempDir, root:PathBuf, input:PathBuf, state:PathBuf, engine:PathBuf }
impl ArchiveFixture {
    fn new()->Self {let tmp=tempfile::tempdir().unwrap();let root=tmp.path().join("data");let input=tmp.path().join("input");fs::create_dir(&root).unwrap();fs::create_dir(&input).unwrap();let state=tmp.path().join("state");let engine=PathBuf::from(std::env::var_os("JCHTOOLS_TEST_7ZIP").expect("Set JCHTOOLS_TEST_7ZIP to the full 7z.exe or 7zz absolute path"));assert!(engine.is_absolute());Self{_tmp:tmp,root,input,state,engine}}
    fn archive(&self,path:&Path,format:&str){let status=Command::new(&self.engine).current_dir(&self.input).args(["a","-y",format]).arg(path).arg(".").status().unwrap();assert!(status.success());}
    /// 用给定格式链与显式成员打包（例如 `-tbzip2` 压缩一个已有 tar，或 `-txz` 压单个文件）。
    fn pack(&self,archive:&Path,formats:&[&str],sources:&[&str]){let mut args=vec!["a","-y"];args.extend_from_slice(formats);let status=Command::new(&self.engine).current_dir(&self.input).args(args).arg(archive).args(sources).status().unwrap();assert!(status.success());}
    fn run(&self,cfg:Config)->engine::TaskResult {engine::prepare_at(&self.root,cfg,Context::default(),&self.state,Some(&self.engine)).unwrap()}
    fn apply(&self,task:&engine::TaskResult)->engine::TaskResult {engine::apply_with(&task.directory,Context::default(),Arc::new(FailRecycle)).unwrap()}
}
fn config()->Config {Config {reserve_gib:0,global_delete:DeleteMode::Permanent,archive_delete:DeleteChoice::Keep,
    extract_conflict:ConflictPolicy::KeepBoth,max_ratio:0,..Config::default()}}
// 覆盖 C-02
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn members_in_subdirectories_merge_into_created_parents(){
    // 回归：含子目录的包此前只创建到祖父目录，成员改名以「系统找不到指定的路径」失败，
    // 整包被记为失败并留下半截空目录（含目录的 RAR 完全解不出来）。
    let f=ArchiveFixture::new();
    fs::create_dir_all(f.input.join("sub/dir1")).unwrap();
    fs::create_dir_all(f.input.join("sub/dir2")).unwrap();
    fs::write(f.input.join("sub/dir1/file1.txt"),b"one").unwrap();
    fs::write(f.input.join("sub/dir2/file2.txt"),b"two").unwrap();
    fs::write(f.input.join("top.txt"),b"top").unwrap();
    f.archive(&f.root.join("pack.zip"),"-tzip");
    let result=f.run(config());
    assert_eq!(result.summary.archives_failed,0,"含子目录的包不得计入失败");
    assert_eq!(result.summary.archives_ok,1);
    assert_eq!(fs::read(f.root.join("sub/dir1/file1.txt")).unwrap(),b"one");
    assert_eq!(fs::read(f.root.join("sub/dir2/file2.txt")).unwrap(),b"two");
    assert_eq!(fs::read(f.root.join("top.txt")).unwrap(),b"top");
}
// 覆盖 C-04, C-01
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn zip_extracts_before_generating_dedup_plan(){let f=ArchiveFixture::new();fs::write(f.input.join("inside.txt"),b"duplicate").unwrap();fs::write(f.root.join("existing.txt"),b"duplicate").unwrap();f.archive(&f.root.join("one.zip"),"-tzip");let result=f.run(config());assert_eq!(result.summary.archives_ok,1);assert!(f.root.join("inside.txt").exists());assert!(f.root.join("existing.txt").exists());assert_eq!(result.summary.planned_delete,1);}
// 覆盖 C-02, S-06
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn solid_7z_is_decoded_in_one_pass(){let f=ArchiveFixture::new();for i in 0..12{fs::write(f.input.join(format!("{i}.txt")),vec![i as u8;8192]).unwrap();}f.archive(&f.root.join("solid.7z"),"-t7z");let result=f.run(config());assert_eq!(result.summary.archives_ok,1);assert_eq!(result.summary.extracted,12);assert!(!f.root.join(".jchtools-work").exists());}
// 覆盖 R-03
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn successful_source_can_be_deleted(){let f=ArchiveFixture::new();fs::write(f.input.join("a.txt"),b"one").unwrap();f.archive(&f.root.join("one.zip"),"-tzip");let mut cfg=config();cfg.archive_delete=DeleteChoice::Permanent;let result=f.run(cfg);assert_eq!(result.summary.archives_ok,1);assert!(!f.root.join("one.zip").exists());assert!(f.root.join("a.txt").exists());}
// 覆盖 C-03, R-03
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn skipped_conflict_always_preserves_original_archive(){let f=ArchiveFixture::new();fs::write(f.input.join("a.txt"),b"new").unwrap();fs::write(f.root.join("a.txt"),b"old").unwrap();f.archive(&f.root.join("one.zip"),"-tzip");let mut cfg=config();cfg.archive_delete=DeleteChoice::Permanent;cfg.extract_conflict=ConflictPolicy::Skip;let result=f.run(cfg);assert!(f.root.join("one.zip").exists());assert_eq!(fs::read(f.root.join("a.txt")).unwrap(),b"old");let db=jchtools::db::Database::open(&result.directory).unwrap();assert!(!db.actions_page(0,100).unwrap().iter().any(|a|a.source=="one.zip"));}
// 覆盖 S-05, R-03
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn corrupt_archive_is_kept_and_logged(){let f=ArchiveFixture::new();fs::write(f.root.join("broken.zip"),b"not a zip").unwrap();let mut cfg=config();cfg.archive_delete=DeleteChoice::Permanent;let result=f.run(cfg);assert_eq!(result.summary.archives_failed,1);assert!(f.root.join("broken.zip").exists());}
// 覆盖 S-05
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn encrypted_archive_is_not_silently_deleted(){let f=ArchiveFixture::new();fs::write(f.input.join("secret.txt"),b"secret").unwrap();let status=Command::new(&f.engine).current_dir(&f.input).args(["a","-t7z","-pTEST-ONLY-NOT-A-REAL-SECRET","-mhe=on"]).arg(f.root.join("secret.7z")).arg(".").status().unwrap();assert!(status.success());let mut cfg=config();cfg.archive_delete=DeleteChoice::Permanent;let result=f.run(cfg);assert_eq!(result.summary.archives_failed,1);assert!(f.root.join("secret.7z").exists());}
// 覆盖 C-02
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn nested_archives_are_processed_recursively(){let f=ArchiveFixture::new();fs::write(f.input.join("payload.txt"),b"payload").unwrap();let inner=f._tmp.path().join("inner.zip");f.archive(&inner,"-tzip");fs::remove_file(f.input.join("payload.txt")).unwrap();fs::rename(inner,f.input.join("inner.zip")).unwrap();f.archive(&f.root.join("outer.zip"),"-tzip");let result=f.run(config());assert_eq!(result.summary.archives_ok,2);assert!(f.root.join("payload.txt").exists());}
// 覆盖 R-02
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn nested_archive_depth_limit_retains_unprocessed_package(){let f=ArchiveFixture::new();fs::write(f.input.join("payload.txt"),b"payload").unwrap();let inner=f._tmp.path().join("inner.zip");f.archive(&inner,"-tzip");fs::remove_file(f.input.join("payload.txt")).unwrap();fs::rename(inner,f.input.join("inner.zip")).unwrap();f.archive(&f.root.join("outer.zip"),"-tzip");let mut cfg=config();cfg.max_depth=1;let result=f.run(cfg);assert_eq!(result.summary.archives_ok,1);assert_eq!(result.summary.archives_failed,1);assert!(f.root.join("inner.zip").exists());assert!(!f.root.join("payload.txt").exists());}
// 覆盖 C-02
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn xz_single_stream_names_member_after_the_archive(){
    let f=ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"),b"stream payload\n").unwrap();
    f.pack(&f.root.join("payload.txt.xz"),&["-txz"],&["payload.txt"]);
    let result=f.run(config());
    assert_eq!(result.summary.archives_failed,0);
    assert_eq!(fs::read(f.root.join("payload.txt")).unwrap(),b"stream payload\n");
}
// 覆盖 C-02
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn bzip2_wrapped_tar_extracts_through_the_named_intermediate_tar(){
    let f=ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"),b"stream payload\n").unwrap();
    f.pack(&f.input.join("bundle.tar"),&["-ttar"],&["payload.txt"]);
    f.pack(&f.root.join("bundle.tar.bz2"),&["-tbzip2"],&["bundle.tar"]);
    let result=f.run(config());
    assert_eq!(result.summary.archives_ok,2,"外层 bz2 与中间 tar 都应处理");
    assert!(f.root.join("bundle.tar").is_file(),"中间成员按去掉一层压缩后缀命名，保留策略下留在原地");
    assert_eq!(fs::read(f.root.join("payload.txt")).unwrap(),b"stream payload\n");
}
// 覆盖 C-02
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn tgz_shorthand_restores_the_tar_suffix(){
    let f=ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"),b"stream payload\n").unwrap();
    f.pack(&f.input.join("bundle.tar"),&["-ttar"],&["payload.txt"]);
    f.pack(&f.root.join("bundle.tgz"),&["-tgzip"],&["bundle.tar"]);
    let result=f.run(config());
    assert_eq!(result.summary.archives_ok,2,"外层 tgz 与中间 tar 都应处理");
    assert!(f.root.join("bundle.tar").is_file(),"tgz 应还原为中间 tar 而不是再次解压");
    assert_eq!(fs::read(f.root.join("payload.txt")).unwrap(),b"stream payload\n");
}
// 覆盖 C-02
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn deleted_same_name_file_does_not_abort_a_later_stream_archive(){
    // 回归：根目录里的 bundle.tar 先被解压并（按授权）删除，随后 bundle.tar.gz 解出的成员名同样是
    // bundle.tar。早期实现会在“同名同内容”查找里读取这个刚被删除的路径，把整包解压判为失败。
    let f=ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"),b"stream payload\n").unwrap();
    f.pack(&f.input.join("bundle.tar"),&["-ttar"],&["payload.txt"]);
    fs::copy(f.input.join("bundle.tar"),f.root.join("bundle.tar")).unwrap();
    f.pack(&f.root.join("bundle.tar.gz"),&["-tgzip"],&["bundle.tar"]);
    let mut cfg=config();
    cfg.archive_delete=DeleteChoice::Permanent;
    let result=f.run(cfg);
    assert_eq!(result.summary.archives_failed,0,"同名文件被删除后，后续流式压缩包仍应解压成功");
    assert!(result.summary.archives_ok>=2);
    assert_eq!(fs::read(f.root.join("payload.txt")).unwrap(),b"stream payload\n");
}
// 覆盖 C-03, C-04, C-14
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn equal_content_archives_delete_and_classify_is_stable(){
    let f=ArchiveFixture::new();
    fs::write(f.input.join("beta.txt"),b"beta member\n").unwrap();
    f.archive(&f.root.join("base.zip"),"-tzip");
    f.archive(&f.root.join("base.tar"),"-ttar");
    let mut cfg=config();
    cfg.archive_delete=DeleteChoice::Permanent;
    cfg.extract_conflict=ConflictPolicy::Largest;
    cfg.classify=ClassifyMode::Category;
    cfg.preserve_structure=false;
    let task=f.run(cfg.clone());
    assert_eq!(task.summary.archives_ok,2,"both archives should extract and delete");
    assert!(!f.root.join("base.zip").exists());
    assert!(!f.root.join("base.tar").exists());
    f.apply(&task);
    assert!(!f.root.join("beta.txt").exists());
    assert!(f.root.join("文档/beta.txt").exists());
    let again=f.run(cfg);
    assert_eq!(again.summary.archives_ok,0);
    assert_eq!(again.summary.planned_delete,0);
    assert_eq!(again.summary.planned_move,0);
    assert!(f.root.join("文档/beta.txt").exists());
}
// 覆盖 C-14, C-05, R-03
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn kept_source_reextract_does_not_recreate_classified_duplicate(){
    // 源包保留（分卷/Keep）时，归类搬走内容后再次解压不得在源目录旁重新落盘同内容文件。
    let f=ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"),b"payload for kept source\n").unwrap();
    f.archive(&f.root.join("kept.zip"),"-tzip");
    let mut cfg=config();
    cfg.archive_delete=DeleteChoice::Keep;
    cfg.classify=ClassifyMode::Category;
    cfg.preserve_structure=false;
    let task=f.run(cfg.clone());
    assert_eq!(task.summary.archives_ok,1);
    assert!(f.root.join("kept.zip").exists());
    assert!(f.root.join("payload.txt").exists());
    f.apply(&task);
    assert!(!f.root.join("payload.txt").exists());
    assert!(f.root.join("文档/payload.txt").exists());
    let again=f.run(cfg);
    assert!(!f.root.join("payload.txt").exists(),"kept archive must not rewrite classified content beside itself");
    assert!(f.root.join("文档/payload.txt").exists());
    assert_eq!(again.summary.planned_delete,0);
    assert_eq!(again.summary.planned_move,0);
}

// ===== 手工构造的特殊压缩包样本（重复条目 / GBK 文件名 / 空包 / ZST）=====
// 7-Zip 无法直接创建这些样本（同名条目、原始字节文件名、空包、zstd 帧），因此按
// PKWARE APPNOTE 与 RFC 8878 手工生成字节；全部为存储（未压缩）条目，无需压缩器。

/// 标准 CRC-32（poly 0xEDB88320，初值与输出取反）。
fn crc32(bytes: &[u8]) -> u32 {
    let mut table=[0u32;256];
    for i in 0..256u32 {
        let mut c=i;
        for _ in 0..8 { c=if c&1!=0 {0xEDB88320^(c>>1)} else {c>>1}; }
        table[i as usize]=c;
    }
    let mut crc=!0u32;
    for &b in bytes { crc=table[(((crc^b as u32))&0xFF) as usize]^(crc>>8); }
    !crc
}
/// 生成只含"存储"条目的极简 ZIP（本地文件头 + 中央目录 + EOCD）。
/// 文件名按原始字节写入，不带 UTF-8 标志：可构造 GBK 字节名与同名重复条目。
fn stored_zip(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out=Vec::new();
    let mut central=Vec::new();
    let mut offset=0u32;
    for (name,data) in entries {
        let crc=crc32(data);
        let header_offset=offset;
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());            // 版本
        out.extend_from_slice(&0u16.to_le_bytes());             // 标志：无 UTF-8 位
        out.extend_from_slice(&0u16.to_le_bytes());             // 方法：存储
        out.extend_from_slice(&[0,0,0,0]);                      // 时间/日期
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());             // extra 长度
        out.extend_from_slice(name);
        out.extend_from_slice(data);
        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&20u16.to_le_bytes());        // 制作版本
        central.extend_from_slice(&20u16.to_le_bytes());        // 需要版本
        central.extend_from_slice(&0u16.to_le_bytes());         // 标志
        central.extend_from_slice(&0u16.to_le_bytes());         // 方法
        central.extend_from_slice(&[0,0,0,0]);                  // 时间/日期
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0;8]);                      // extra 长度/注释长度/起始盘号/内部属性
        central.extend_from_slice(&0u32.to_le_bytes());         // 外部属性
        central.extend_from_slice(&header_offset.to_le_bytes());
        central.extend_from_slice(name);
        offset += (30+name.len()+data.len()) as u32;
    }
    let central_offset=offset;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");                       // EOCD
    out.extend_from_slice(&[0,0,0,0]);                          // 盘号
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());                 // 注释长度
    out
}

// 覆盖 C-03
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn zip_with_duplicate_entries_extracts_both_via_auto_rename(){
    // 同名两条目（内容与大小都不同）：7z 以 -aou 解压时会把第二个同名条目自动改名落盘，
    // 两个内容都保留；若引擎改为覆盖，总量校验（13+18 ≠ 13）会失败并保留原包。
    let f=ArchiveFixture::new();
    fs::write(f.root.join("dup.zip"),stored_zip(&[(b"dup.txt",b"first-10bytes"),(b"dup.txt",b"second-payload-16B")])).unwrap();
    let result=f.run(config());
    assert_eq!(result.summary.archives_failed,0);
    assert_eq!(result.summary.archives_ok,1);
    assert_eq!(result.summary.extracted,2,"两个同名条目都应落盘（-aou 自动改名）");
    // dup_1.txt 的改名格式依赖 7-Zip -aou 的实现；引擎版本由 fetch-7zip.ps1 固定，行为稳定。
    assert_eq!(fs::read(f.root.join("dup.txt")).unwrap(),b"first-10bytes");
    assert_eq!(fs::read(f.root.join("dup_1.txt")).unwrap(),b"second-payload-16B");
}
// 覆盖 S-05
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn gbk_filename_zip_extracts_without_data_loss(){
    // GBK 编码文件名（"报告.txt" 的 GBK 字节）且不带 UTF-8 标志的旧式 zip：
    // 文件名可能与本机代码页不一致而呈乱码，但内容必须完整落盘且路径组件安全。
    let f=ArchiveFixture::new();
    let payload=b"gbk-payload\n!";
    let gbk_name: &[u8]=&[0xB1,0xA8,0xB8,0xE6,b'.',b't',b'x',b't']; // GBK"报告.txt"
    fs::write(f.root.join("gbk.zip"),stored_zip(&[(gbk_name,payload)])).unwrap();
    let result=f.run(config());
    assert_eq!(result.summary.archives_failed,0);
    assert_eq!(result.summary.archives_ok,1);
    assert_eq!(result.summary.extracted,1);
    let extracted:Vec<_>=fs::read_dir(&f.root).unwrap().filter_map(|e|e.ok())
        .map(|e|e.path()).filter(|p|p.is_file()&&p.extension().is_some_and(|x|x.eq_ignore_ascii_case("txt"))).collect();
    assert_eq!(extracted.len(),1,"应恰好落盘一个 .txt 文件（名字可能呈乱码）");
    assert_eq!(fs::read(&extracted[0]).unwrap(),payload);
}
// 覆盖 C-02
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn empty_zip_archive_extracts_cleanly(){
    // 只有 EOCD 的空 zip：合法归档，零成员零字节，按成功解压处理。
    let f=ArchiveFixture::new();
    fs::write(f.root.join("empty.zip"),stored_zip(&[])).unwrap();
    let mut cfg=config(); cfg.archive_delete=DeleteChoice::Permanent;
    let result=f.run(cfg);
    assert_eq!(result.summary.archives_failed,0);
    assert_eq!(result.summary.archives_ok,1);
    assert_eq!(result.summary.extracted,0);
    assert!(!f.root.join("empty.zip").exists(),"空包成功解压后源包同样按规则处理");
}
// 覆盖 C-02
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn empty_7z_with_only_a_directory_entry_restores_the_directory(){
    // 只含一个空目录条目的 7z：解压后应还原出目录本身。
    let f=ArchiveFixture::new();
    fs::create_dir(f.input.join("空目录")).unwrap();
    f.pack(&f.root.join("empty.7z"),&["-t7z"],&["空目录"]);
    let mut cfg=config(); cfg.archive_delete=DeleteChoice::Permanent;
    let result=f.run(cfg);
    assert_eq!(result.summary.archives_failed,0);
    assert_eq!(result.summary.archives_ok,1);
    assert_eq!(result.summary.extracted,0);
    assert!(f.root.join("空目录").is_dir(),"仅目录条目的 7z 也应还原出目录");
}
// 覆盖 C-02
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn zstd_stream_archive_extracts_with_content(){
    // 7-Zip 只解不建 .zst，样本按 RFC 8878 手工构造：
    // magic + 单段帧头（1 字节帧内容大小）+ 一个"最后一块、原始块"，内容直接内联。
    let f=ArchiveFixture::new();
    let payload=b"zst payload here\n";
    let mut zst=b"\x28\xB5\x2F\xFD".to_vec();                    // Zstandard magic
    zst.push(0x20);                                              // 单段帧 + 帧内容大小 1 字节
    zst.push(payload.len() as u8);                               // Frame_Content_Size
    let block_header=1u32 | (payload.len() as u32)<<3;           // 最后一块 · 原始块 · 大小
    zst.extend_from_slice(&block_header.to_le_bytes()[..3]);
    zst.extend_from_slice(payload);
    fs::write(f.root.join("payload.txt.zst"),zst).unwrap();
    let result=f.run(config());
    assert_eq!(result.summary.archives_failed,0);
    assert_eq!(result.summary.archives_ok,1);
    assert_eq!(fs::read(f.root.join("payload.txt")).unwrap(),payload);
}

// 平台门禁原因：触发条件本身是 Windows 路径长度语义（>260 字符 + LongPathsEnabled 默认 0 时
// 裸 Win32 调用失败），且植入隐藏属性需要 SetFileAttributesW，均无法在非 Windows 复现。
// 覆盖 R-07, C-02
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
#[cfg(windows)]
fn long_path_hidden_member_is_stripped_and_archive_completes(){
    // 管线级行为锚点：>260 字符且存储了隐藏属性的成员（7z 会还原属性），在默认
    // include_hidden=false 下必须被剥离成可见文件，且 complete 不得被误置——
    // 否则成员成扫描不可见的影子文件而原包仍被删除，用户视角即内容丢失。
    // 注意：本测试经 prepare_at 的 root 已 canonicalize（verbatim），锁的是管线级契约；
    // 「普通路径 + 超长」的原始缺陷形态由 archive.rs 内对 normalize 的直测锚点锁定。
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN,SetFileAttributesW};
    let f=ArchiveFixture::new();
    let mut dir=f.input.clone();
    let seg="l".repeat(40);
    for i in 0..7 { dir.push(format!("{seg}{i}")); }             // 相对路径 ≈300 字符，远超 260
    fs::create_dir_all(&dir).unwrap();
    let file=dir.join("deep-hidden.txt");
    fs::write(&file,b"deep hidden payload").unwrap();
    // 测试自身植入属性必须走 \\?\ 前缀：LongPathsEnabled=0 的机器上裸调用同样会失败
    //（这正是本次整改所针对的产品缺陷，前置若用裸调用连夹具都搭不起来）。
    let mut wide: Vec<u16>=r"\\?\".encode_utf16().collect();
    wide.extend(file.as_os_str().encode_wide());
    wide.push(0);
    let ok=unsafe{SetFileAttributesW(wide.as_ptr(),FILE_ATTRIBUTE_HIDDEN)};
    assert!(ok!=0,"测试前置：植入隐藏属性失败");
    let meta=fs::symlink_metadata(&file).unwrap();
    assert_ne!(meta.file_attributes()&2,0,"测试前置：隐藏属性应已植入");
    f.archive(&f.root.join("deep.7z"),"-t7z");
    let mut cfg=config();
    cfg.archive_delete=DeleteChoice::Permanent;
    let result=f.run(cfg);
    assert_eq!(result.summary.archives_failed,0,"超长路径包不得计入失败");
    assert_eq!(result.summary.extracted,1);
    let rel=dir.strip_prefix(&f.input).unwrap();
    let dest=f.root.join(rel).join("deep-hidden.txt");
    let dest_meta=fs::symlink_metadata(&dest).unwrap_or_else(|e|panic!("成员应按原相对路径落盘 {dest:?}: {e}"));
    assert_eq!(dest_meta.file_attributes()&2,0,"隐藏属性必须被剥离，否则成员成扫描不可见的影子文件");
    assert!(!f.root.join("deep.7z").exists(),"成员可见后原包应按规则删除（complete 未被误置 false）");
}
