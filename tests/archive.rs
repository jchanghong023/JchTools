//! Real-engine tests; intentionally ignored without an explicit MYTOOLS_TEST_7ZIP path.
//! package-windows.ps1 runs these after downloading and verifying the bundled engine.
use jchtools::{config::*,control::Context,engine};
use std::{fs,path::{Path,PathBuf},process::Command};
struct ArchiveFixture { _tmp:tempfile::TempDir, root:PathBuf, input:PathBuf, state:PathBuf, engine:PathBuf }
impl ArchiveFixture {
    fn new()->Self {let tmp=tempfile::tempdir().unwrap();let root=tmp.path().join("data");let input=tmp.path().join("input");fs::create_dir(&root).unwrap();fs::create_dir(&input).unwrap();let state=tmp.path().join("state");let engine=PathBuf::from(std::env::var_os("MYTOOLS_TEST_7ZIP").expect("Set MYTOOLS_TEST_7ZIP to the full 7z.exe or 7zz absolute path"));assert!(engine.is_absolute());Self{_tmp:tmp,root,input,state,engine}}
    fn archive(&self,path:&Path,format:&str){let status=Command::new(&self.engine).current_dir(&self.input).args(["a","-y",format]).arg(path).arg(".").status().unwrap();assert!(status.success());}
    fn run(&self,cfg:Config)->engine::TaskResult {engine::prepare_at(&self.root,cfg,Context::default(),&self.state,Some(&self.engine)).unwrap()}
}
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
