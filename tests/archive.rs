//! Real-engine tests; intentionally ignored without an explicit JCHTOOLS_TEST_7ZIP path.
//! package-windows.ps1 runs these after downloading and verifying the bundled engine.
use jchtools::{config::*,control::Context,engine,platform::{RecycleFailure,Recycler}};
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
struct FailRecycle;
impl Recycler for FailRecycle {fn recycle(&self,_:&Path)->Result<(),RecycleFailure>{Err(RecycleFailure::Failed("mock capacity full".into()))}}
fn config()->Config {Config {reserve_gib:0,global_delete:DeleteMode::Permanent,archive_delete:DeleteChoice::Keep,
    extract_conflict:ConflictPolicy::KeepBoth,max_ratio:0,..Config::default()}}
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn zip_extracts_before_generating_dedup_plan(){let f=ArchiveFixture::new();fs::write(f.input.join("inside.txt"),b"duplicate").unwrap();fs::write(f.root.join("existing.txt"),b"duplicate").unwrap();f.archive(&f.root.join("one.zip"),"-tzip");let result=f.run(config());assert_eq!(result.summary.archives_ok,1);assert!(f.root.join("inside.txt").exists());assert!(f.root.join("existing.txt").exists());assert_eq!(result.summary.planned_delete,1);}
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn solid_7z_is_decoded_in_one_pass(){let f=ArchiveFixture::new();for i in 0..12{fs::write(f.input.join(format!("{i}.txt")),vec![i as u8;8192]).unwrap();}f.archive(&f.root.join("solid.7z"),"-t7z");let result=f.run(config());assert_eq!(result.summary.archives_ok,1);assert_eq!(result.summary.extracted,12);assert!(!f.root.join(".jchtools-work").exists());}
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn successful_source_can_be_deleted(){let f=ArchiveFixture::new();fs::write(f.input.join("a.txt"),b"one").unwrap();f.archive(&f.root.join("one.zip"),"-tzip");let mut cfg=config();cfg.archive_delete=DeleteChoice::Permanent;let result=f.run(cfg);assert_eq!(result.summary.archives_ok,1);assert!(!f.root.join("one.zip").exists());assert!(f.root.join("a.txt").exists());}
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn skipped_conflict_always_preserves_original_archive(){let f=ArchiveFixture::new();fs::write(f.input.join("a.txt"),b"new").unwrap();fs::write(f.root.join("a.txt"),b"old").unwrap();f.archive(&f.root.join("one.zip"),"-tzip");let mut cfg=config();cfg.archive_delete=DeleteChoice::Permanent;cfg.extract_conflict=ConflictPolicy::Skip;let result=f.run(cfg);assert!(f.root.join("one.zip").exists());assert_eq!(fs::read(f.root.join("a.txt")).unwrap(),b"old");let db=jchtools::db::Database::open(&result.directory).unwrap();assert!(!db.actions_page(0,100).unwrap().iter().any(|a|a.source=="one.zip"));}
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn corrupt_archive_is_kept_and_logged(){let f=ArchiveFixture::new();fs::write(f.root.join("broken.zip"),b"not a zip").unwrap();let mut cfg=config();cfg.archive_delete=DeleteChoice::Permanent;let result=f.run(cfg);assert_eq!(result.summary.archives_failed,1);assert!(f.root.join("broken.zip").exists());}
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn encrypted_archive_is_not_silently_deleted(){let f=ArchiveFixture::new();fs::write(f.input.join("secret.txt"),b"secret").unwrap();let status=Command::new(&f.engine).current_dir(&f.input).args(["a","-t7z","-pTEST-ONLY-NOT-A-REAL-SECRET","-mhe=on"]).arg(f.root.join("secret.7z")).arg(".").status().unwrap();assert!(status.success());let mut cfg=config();cfg.archive_delete=DeleteChoice::Permanent;let result=f.run(cfg);assert_eq!(result.summary.archives_failed,1);assert!(f.root.join("secret.7z").exists());}
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn nested_archives_are_processed_recursively(){let f=ArchiveFixture::new();fs::write(f.input.join("payload.txt"),b"payload").unwrap();let inner=f._tmp.path().join("inner.zip");f.archive(&inner,"-tzip");fs::remove_file(f.input.join("payload.txt")).unwrap();fs::rename(inner,f.input.join("inner.zip")).unwrap();f.archive(&f.root.join("outer.zip"),"-tzip");let result=f.run(config());assert_eq!(result.summary.archives_ok,2);assert!(f.root.join("payload.txt").exists());}
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn nested_archive_depth_limit_retains_unprocessed_package(){let f=ArchiveFixture::new();fs::write(f.input.join("payload.txt"),b"payload").unwrap();let inner=f._tmp.path().join("inner.zip");f.archive(&inner,"-tzip");fs::remove_file(f.input.join("payload.txt")).unwrap();fs::rename(inner,f.input.join("inner.zip")).unwrap();f.archive(&f.root.join("outer.zip"),"-tzip");let mut cfg=config();cfg.max_depth=1;let result=f.run(cfg);assert_eq!(result.summary.archives_ok,1);assert_eq!(result.summary.archives_failed,1);assert!(f.root.join("inner.zip").exists());assert!(!f.root.join("payload.txt").exists());}
#[test] #[ignore = "Requires explicitly provided real 7-Zip engine"]
fn xz_single_stream_names_member_after_the_archive(){
    let f=ArchiveFixture::new();
    fs::write(f.input.join("payload.txt"),b"stream payload\n").unwrap();
    f.pack(&f.root.join("payload.txt.xz"),&["-txz"],&["payload.txt"]);
    let result=f.run(config());
    assert_eq!(result.summary.archives_failed,0);
    assert_eq!(fs::read(f.root.join("payload.txt")).unwrap(),b"stream payload\n");
}
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
