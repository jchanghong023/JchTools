//! These tests mutate only tempfile fixtures. Recycle Bin operations are injected mocks.
use jchtools::{config::*, control::{Context,Control}, db::Database, engine, fsutil, hashing,
    model::{ActionKind,FileRecord,Snapshot}, platform::{self,DeleteResult,RecycleFailure,Recycler},rules};
use std::{fs,path::{Path,PathBuf},sync::{Arc,atomic::{AtomicUsize,Ordering}}};
use tempfile::TempDir;
struct Fixture { _temp:TempDir, root:PathBuf,state:PathBuf }
impl Fixture {
    fn new()->Self {let temp=tempfile::tempdir().unwrap();let root=temp.path().join("data");let state=temp.path().join("state");fs::create_dir(&root).unwrap();Self{_temp:temp,root,state}}
    fn write(&self,name:&str,bytes:&[u8],time:i64)->PathBuf {let p=self.root.join(name);fs::create_dir_all(p.parent().unwrap()).unwrap();fs::write(&p,bytes).unwrap();filetime::set_file_mtime(&p,filetime::FileTime::from_unix_time(time,0)).unwrap();p}
    fn plan(&self,cfg:Config)->engine::TaskResult {engine::prepare_at(&self.root,cfg,Context::default(),&self.state,None).unwrap()}
    fn apply(&self,task:&engine::TaskResult)->engine::TaskResult {engine::apply_with(&task.directory,Context::default(),Arc::new(FailRecycle)).unwrap()}
}
fn base()->Config {Config{extract:false,global_delete:DeleteMode::Permanent,same_name_same_size:false,
    same_name_different_size:false,clean_empty_dirs:false,clean_copy_name:false,classify:ClassifyMode::Off,..Config::default()}}
struct FailRecycle;
impl Recycler for FailRecycle {fn recycle(&self,_:&Path)->Result<(),RecycleFailure>{Err(RecycleFailure::Failed("mock capacity full".into()))}}
struct CancelRecycle;
impl Recycler for CancelRecycle {fn recycle(&self,_:&Path)->Result<(),RecycleFailure>{Err(RecycleFailure::Cancelled)}}
struct MoveRecycle {target:PathBuf,calls:AtomicUsize}
impl Recycler for MoveRecycle {fn recycle(&self,p:&Path)->Result<(),RecycleFailure>{self.calls.fetch_add(1,Ordering::Relaxed);fs::rename(p,&self.target).map_err(|e|RecycleFailure::Failed(e.to_string()))}}
#[test] fn defaults_valid_and_roundtrip(){let cfg=Config::default();cfg.validate().unwrap();assert!(cfg.recycle_fallback);let dir=tempfile::tempdir().unwrap();let path=dir.path().join("config.json");cfg.save(&path).unwrap();let loaded=Config::load(&path).unwrap();assert_eq!(serde_json::to_value(&cfg).unwrap(),serde_json::to_value(&loaded).unwrap());}
#[test] fn reject_unknown_configuration(){let dir=tempfile::tempdir().unwrap();let path=dir.path().join("config.json");fs::write(&path,r#"{"delete_everything":true}"#).unwrap();assert!(Config::load(&path).is_err());}
#[test] fn schema_matches_every_configuration_field(){let cfg=serde_json::to_value(Config::default()).unwrap();let schema:serde_json::Value=serde_json::from_str(include_str!("../resources/rules.json")).unwrap();let keys=schema.as_array().unwrap();assert_eq!(keys.len(),cfg.as_object().unwrap().len());for row in keys{assert!(cfg.get(row["key"].as_str().unwrap()).is_some());}}
#[test] fn coupled_validation_and_bounds(){let mut c=base();c.fix_extension=true;assert!(c.validate().is_err());c.detect_type=true;assert!(c.validate().is_ok());c.hash_workers=0;assert!(c.validate().is_err());c.hash_workers=2;c.reserve_gib=u64::MAX;assert!(c.validate().is_err());}
#[test] fn unsafe_paths_rejected(){for value in ["../x","x/../../outside","C:/x","/etc/passwd",r"\\server\share\x","file:stream","CON.txt","a/NUL","x. ","a\n.txt",""]{assert!(fsutil::safe_relative(value).is_err(),"{value:?}");}}
#[test] fn relative_tar_and_unicode_paths_accepted(){assert_eq!(fsutil::safe_relative("./报告/a.pdf").unwrap(),PathBuf::from("报告/a.pdf"));assert_eq!(fsutil::safe_relative(r"资料\图片.png").unwrap(),PathBuf::from("资料/图片.png"));}
#[test] fn windows_reserved_names_rejected(){for s in ["con","COM1","LPT9.txt","nul","AUX.jpg","COM¹.txt"]{assert!(fsutil::validate_component(s).is_err());}assert!(fsutil::validate_component("COM10.txt").is_ok());}
#[test] fn metadata_change_invalidates_snapshot(){let f=Fixture::new();let p=f.write("a",b"one",1);let s=fsutil::snapshot(&p).unwrap();fsutil::unchanged(&p,&s).unwrap();fs::write(&p,b"different").unwrap();assert!(fsutil::unchanged(&p,&s).is_err());}
#[test] fn rename_never_overwrites(){let f=Fixture::new();let a=f.write("a",b"A",1);let b=f.write("b",b"B",2);assert!(fsutil::rename_noreplace(&a,&b).is_err());assert_eq!(fs::read(a).unwrap(),b"A");assert_eq!(fs::read(b).unwrap(),b"B");}
#[test] fn unique_name_preserves_extension(){let f=Fixture::new();let p=f.write("report.pdf",b"a",1);let q=fsutil::unique_target(&f.root,&p).unwrap();assert_eq!(q.file_name().unwrap(),"report (1).pdf");}
#[test] fn hashes_known_vectors(){let f=Fixture::new();let p=f.write("a",b"abc",1);let s=fsutil::snapshot(&p).unwrap();let ctl=Control::default();assert_eq!(hashing::full_hash(&p,&s,HashAlgorithm::Md5,&ctl).unwrap(),"md5:900150983cd24fb0d6963f7d28e17f72");assert_eq!(hashing::full_hash(&p,&s,HashAlgorithm::Sha256,&ctl).unwrap(),"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");assert!(hashing::full_hash(&p,&s,HashAlgorithm::Blake3,&ctl).unwrap().starts_with("blake3:"));}
#[test] fn same_prehash_different_middle_is_not_duplicate(){let f=Fixture::new();let a=vec![7u8;300_000];let mut b=a.clone();b[150_000]=8;let ap=f.write("a",&a,1);let bp=f.write("b",&b,2);let sa=fsutil::snapshot(&ap).unwrap();let sb=fsutil::snapshot(&bp).unwrap();let ctl=Control::default();assert_eq!(hashing::prehash(&ap,&sa,&ctl).unwrap(),hashing::prehash(&bp,&sb,&ctl).unwrap());assert_ne!(hashing::full_hash(&ap,&sa,HashAlgorithm::Blake3,&ctl).unwrap(),hashing::full_hash(&bp,&sb,HashAlgorithm::Blake3,&ctl).unwrap());assert_eq!(f.plan(base()).summary.planned_delete,0);}
#[test] fn cancelled_hash_does_not_read(){let f=Fixture::new();let p=f.write("a",b"abc",1);let ctl=Control::default();ctl.cancel();assert!(hashing::full_hash(&p,&fsutil::snapshot(&p).unwrap(),HashAlgorithm::Blake3,&ctl).is_err());assert_eq!(ctl.read_bytes.load(Ordering::Relaxed),0);}
#[test] fn copy_suffixes_and_nonempty_name(){for s in ["报告 (1).pdf","报告（2）.pdf","报告 - Copy.pdf","报告 副本.pdf"]{assert_eq!(rules::strip_copy_name(s),"报告.pdf");}assert_eq!(rules::strip_copy_name("(1).pdf"),"(1).pdf");}
#[test] fn category_and_custom_rules(){assert_eq!(rules::category("pdf"),"文档");assert_eq!(rules::category("ts"),"代码");assert_eq!(rules::parse_categories("工程=rs,sv;资料=pdf").unwrap()["sv"],"工程");assert!(rules::parse_categories("../x=pdf").is_err());}
#[test] fn archive_first_volume_detection(){assert!(rules::archive_name("A.part01.rar"));assert!(!rules::archive_name("A.part02.rar"));assert!(rules::archive_name("A.7z.001"));assert!(!rules::archive_name("A.7z.002"));assert!(rules::archive_name("A.tar.gz"));}
#[test] fn recycle_failure_without_permission_keeps_file(){let f=Fixture::new();let p=f.write("a",b"a",1);let s=fsutil::snapshot(&p).unwrap();assert!(platform::remove(&p,Some(&s),DeleteMode::Recycle,false,&Control::default(),&FailRecycle).is_err());assert!(p.exists());}
#[test] fn recycle_failure_with_permission_deletes(){let f=Fixture::new();let p=f.write("a",b"a",1);let s=fsutil::snapshot(&p).unwrap();assert_eq!(platform::remove(&p,Some(&s),DeleteMode::Recycle,true,&Control::default(),&FailRecycle).unwrap(),DeleteResult::Permanent);assert!(!p.exists());}
#[test] fn user_cancel_never_falls_back_to_delete(){let f=Fixture::new();let p=f.write("a",b"a",1);assert!(platform::remove(&p,None,DeleteMode::Recycle,true,&Control::default(),&CancelRecycle).is_err());assert!(p.exists());}
#[test] fn successful_recycle_not_permanent(){let f=Fixture::new();let p=f.write("a",b"a",1);let bin=MoveRecycle{target:f.root.join("mock-bin"),calls:AtomicUsize::new(0)};assert_eq!(platform::remove(&p,None,DeleteMode::Recycle,true,&Control::default(),&bin).unwrap(),DeleteResult::Recycled);assert!(bin.target.exists());assert_eq!(bin.calls.load(Ordering::Relaxed),1);}
#[test] fn keep_never_calls_recycler(){let f=Fixture::new();let p=f.write("a",b"a",1);assert_eq!(platform::remove(&p,None,DeleteMode::Keep,true,&Control::default(),&CancelRecycle).unwrap(),DeleteResult::Kept);assert!(p.exists());}
#[test] fn nonempty_directory_cannot_be_removed(){let f=Fixture::new();f.write("sub/a",b"a",1);assert!(platform::remove(&f.root.join("sub"),None,DeleteMode::Permanent,true,&Control::default(),&FailRecycle).is_err());assert!(f.root.join("sub/a").exists());}
#[test] fn analysis_no_changes_and_dedup_keeps_latest(){let f=Fixture::new();let old=f.write("old.txt",b"same",10);let new=f.write("new.txt",b"same",20);let task=f.plan(base());assert!(old.exists()&&new.exists());assert_eq!(task.summary.planned_delete,1);f.apply(&task);assert!(!old.exists()&&new.exists());}
#[test] fn different_names_can_be_disabled(){let f=Fixture::new();f.write("a.txt",b"same",10);f.write("b.txt",b"same",20);let mut cfg=base();cfg.dedup_other_names=false;assert_eq!(f.plan(cfg).summary.planned_delete,0);}
#[test] fn same_names_different_directories_can_be_disabled(){let f=Fixture::new();f.write("a/test.txt",b"same",10);f.write("b/test.txt",b"same",20);let mut cfg=base();cfg.dedup_same_name=false;assert_eq!(f.plan(cfg).summary.planned_delete,0);}
#[test] fn copy_names_can_be_disabled_independently(){let f=Fixture::new();f.write("a.txt",b"same",10);f.write("a (1).txt",b"same",20);let mut cfg=base();cfg.dedup_copy_names=false;assert_eq!(f.plan(cfg).summary.planned_delete,0);}
#[test] fn equal_size_different_hash_not_deleted_when_rule_off(){let f=Fixture::new();f.write("a/report.txt",b"abcd",10);f.write("b/report.txt",b"efgh",20);assert_eq!(f.plan(base()).summary.planned_delete,0);}
#[test] fn same_name_equal_size_version_keeps_latest(){let f=Fixture::new();let a=f.write("a/report.txt",b"abcd",10);let b=f.write("b/report.txt",b"efgh",20);let mut cfg=base();cfg.same_name_same_size=true;cfg.conflict_scope_directory=false;let task=f.plan(cfg);assert_eq!(task.summary.planned_delete,1);f.apply(&task);assert!(!a.exists()&&b.exists());}
#[test] fn same_name_different_size_version_keeps_newest(){let f=Fixture::new();let a=f.write("a/report.txt",b"abc",20);let b=f.write("b/report.txt",b"longest",10);let mut cfg=base();cfg.same_name_different_size=true;cfg.conflict_scope_directory=false;assert_eq!(cfg.different_size_keep,KeepPolicy::Newest);let task=f.plan(cfg);f.apply(&task);assert!(a.exists()&&!b.exists());}
#[test] fn directory_scoped_conflicts_do_not_cross_directories(){let f=Fixture::new();f.write("a/report.txt",b"abc",20);f.write("b/report.txt",b"longest",10);let mut cfg=base();cfg.same_name_different_size=true;cfg.conflict_scope_directory=true;assert_eq!(f.plan(cfg).summary.planned_delete,0);}
#[test] fn unselecting_action_preserves_file(){let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);let task=f.plan(base());let db=Database::open(&task.directory).unwrap();let action=db.actions_page(0,10).unwrap().remove(0);db.set_selected(action.id,false).unwrap();drop(db);f.apply(&task);assert!(f.root.join("a").exists()&&f.root.join("b").exists());}
#[test] fn changed_source_after_plan_is_skipped(){let f=Fixture::new();let a=f.write("a",b"same",10);f.write("b",b"same",20);let task=f.plan(base());fs::write(&a,b"brand new").unwrap();let result=f.apply(&task);assert!(a.exists());assert_eq!(result.summary.errors,1);}
#[test] fn changed_keeper_after_plan_prevents_delete(){let f=Fixture::new();let a=f.write("a",b"same",10);let b=f.write("b",b"same",20);let task=f.plan(base());fs::write(&b,b"new keeper").unwrap();let result=f.apply(&task);assert!(a.exists());assert_eq!(result.summary.errors,1);}
#[test] fn cleanup_does_not_become_only_dedup_keeper(){let f=Fixture::new();f.write("keep.txt",b"content",10);f.write("temporary.tmp",b"content",20);let mut cfg=base();cfg.clean_temp=true;let task=f.plan(cfg);f.apply(&task);assert!(f.root.join("keep.txt").exists());assert!(!f.root.join("temporary.tmp").exists());}
#[test] fn global_keep_prohibits_deletion(){let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);let mut cfg=base();cfg.global_delete=DeleteMode::Keep;assert_eq!(f.plan(cfg).summary.planned_delete,0);}
#[test] fn class_override_beats_global_keep(){let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);let mut cfg=base();cfg.global_delete=DeleteMode::Keep;cfg.duplicate_delete=DeleteChoice::Permanent;assert_eq!(f.plan(cfg).summary.planned_delete,1);}
#[test] fn copy_name_cleanup_uses_freed_original_name(){let f=Fixture::new();f.write("a.pdf",b"same",10);f.write("a (1).pdf",b"same",20);let mut cfg=base();cfg.clean_copy_name=true;let task=f.plan(cfg);f.apply(&task);assert!(f.root.join("a.pdf").exists());assert!(!f.root.join("a (1).pdf").exists());}
#[test] fn classification_preserves_paths_and_is_idempotent(){let f=Fixture::new();f.write("folder/a.pdf",b"pdf",10);let mut cfg=base();cfg.classify=ClassifyMode::Extension;let task=f.plan(cfg.clone());f.apply(&task);assert!(f.root.join("PDF/folder/a.pdf").exists());let again=f.plan(cfg);assert_eq!(again.summary.planned_move,0);}
#[test] fn flatten_classification_allocates_nonconflicting_names(){let f=Fixture::new();f.write("x/a.pdf",b"left",10);f.write("y/a.pdf",b"right",20);let mut cfg=base();cfg.classify=ClassifyMode::Extension;cfg.preserve_structure=false;let task=f.plan(cfg);f.apply(&task);assert!(f.root.join("PDF/a.pdf").exists());assert!(f.root.join("PDF/a (1).pdf").exists());}
#[test] fn empty_directory_cleanup_is_bottom_up(){let f=Fixture::new();fs::create_dir_all(f.root.join("empty/nested")).unwrap();let mut cfg=base();cfg.clean_empty_dirs=true;let task=f.plan(cfg);f.apply(&task);assert!(!f.root.join("empty").exists());assert!(f.root.exists());}
#[test] fn empty_directory_with_underscore_not_blocked_by_similar_name(){
    // LIKE 的 `_` 是单字符通配：不转义时 my_dir/% 会匹配到 myXdir 下的文件。
    let f=Fixture::new();
    fs::create_dir_all(f.root.join("my_dir/nested_empty")).unwrap();
    f.write("myXdir/file.txt",b"payload",10);
    let mut cfg=base();cfg.clean_empty_dirs=true;
    f.apply(&f.plan(cfg));
    assert!(!f.root.join("my_dir").exists(),"含下划线的空目录必须能被清理");
    assert!(f.root.join("myXdir/file.txt").exists(),"相似前缀目录里的文件不能被误伤");
}
#[test] fn empty_directory_with_percent_in_name_is_cleaned(){
    let f=Fixture::new();
    fs::create_dir_all(f.root.join("100%done")).unwrap();
    f.write("100Xdone/file.txt",b"payload",10);
    let mut cfg=base();cfg.clean_empty_dirs=true;
    f.apply(&f.plan(cfg));
    assert!(!f.root.join("100%done").exists());
    assert!(f.root.join("100Xdone/file.txt").exists());
}
#[test] fn classification_empty_dirs_planned_in_same_pass(){
    let f=Fixture::new();
    f.write("folder/a.pdf",b"pdf",10);
    let mut cfg=base();
    cfg.classify=ClassifyMode::Extension;
    cfg.clean_empty_dirs=true;
    cfg.preserve_structure=true;
    let task=f.plan(cfg.clone());
    assert_eq!(task.summary.planned_move,1);
    assert_eq!(task.summary.planned_empty,1,"folder/ becomes empty after the move and must be planned now");
    f.apply(&task);
    assert!(f.root.join("PDF/folder/a.pdf").exists());
    assert!(!f.root.join("folder").exists());
    let again=f.plan(cfg);
    assert_eq!(again.summary.planned_delete,0);
    assert_eq!(again.summary.planned_move,0);
    assert_eq!(again.summary.planned_empty,0);
}
#[test] fn date_classification_is_idempotent(){
    let f=Fixture::new();
    f.write("folder/x.txt",b"payload",1_700_000_000);
    let mut cfg=base();
    cfg.classify=ClassifyMode::Date;
    let task=f.plan(cfg.clone());
    assert_eq!(task.summary.planned_move,1);
    let target=Database::open(&task.directory).unwrap().actions_page(0,10).unwrap().remove(0).target.unwrap();
    let parts:Vec<&str>=target.split('/').collect();
    assert_eq!(parts.len(),4,"年/月/folder/x.txt");
    assert_eq!((parts[0].len(),parts[1].len()),(4,2),"年月目录");
    assert_eq!(parts[3],"x.txt");
    f.apply(&task);
    let again=f.plan(cfg);
    assert_eq!(again.summary.planned_move,0,"重新分析不得把年/月目录再套一层");
}
#[test] fn excluded_tree_not_touched(){let f=Fixture::new();f.write("a.txt",b"same",10);f.write("protected/a.txt",b"same",20);let mut cfg=base();cfg.exclusions="protected/**".into();assert_eq!(f.plan(cfg).summary.scanned,1);}
#[test] fn no_recursion_leaves_subdirectories_untouched(){let f=Fixture::new();f.write("a",b"same",10);f.write("sub/b",b"same",20);let mut cfg=base();cfg.recursive=false;assert_eq!(f.plan(cfg).summary.scanned,1);}
#[test] fn finished_plan_cannot_be_replayed(){let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);let task=f.plan(base());f.apply(&task);assert!(engine::apply_with(&task.directory,Context::default(),Arc::new(FailRecycle)).is_err());}
#[test] fn task_lock_prevents_second_task(){let f=Fixture::new();let _guard=fsutil::RootGuard::acquire(&f.state).unwrap();assert!(engine::prepare_at(&f.root,base(),Context::default(),&f.state,None).is_err());}
#[test] fn user_cancelled_analysis_records_cancelled_status(){
    // 用户取消不是故障：任务记录必须写成 cancelled，否则回看记录时会把主动取消当成失败。
    let f=Fixture::new();f.write("a.txt",b"payload",10);
    let mut context=Context::default();context.control.cancel();
    assert!(engine::prepare_at(&f.root,base(),context,&f.state,None).is_err());
    let directory=fs::read_dir(f.state.join("tasks")).unwrap().next().unwrap().unwrap().path();
    let db=Database::open(&directory).unwrap();
    assert_eq!(db.get::<String>("status").unwrap(),"cancelled");
}
#[test] fn report_does_not_overwrite(){let f=Fixture::new();let task=f.plan(base());let report=f.root.join("report.csv");let db=Database::open(&task.directory).unwrap();db.export_csv(&report).unwrap();assert!(db.export_csv(&report).is_err());}
#[test] fn huge_sizes_use_u64(){assert_eq!(jchtools::model::bytes(10u64<<40),"10.00 TiB");let f=Fixture::new();let db=Database::create(&f.state).unwrap();let snapshot=Snapshot{size:12u64<<40,modified_ns:1,identity:"mock".into(),links:1};db.insert_file("large.bin","large.bin","large.bin",&snapshot).unwrap();assert_eq!(db.file(1).unwrap().snapshot.size,12u64<<40);}
#[test] fn deterministic_keeper_ties(){let record=|id,rel:&str|FileRecord{id,rel:rel.into(),name:"x".into(),normalized:"x".into(),snapshot:Snapshot{size:1,modified_ns:10,identity:id.to_string(),links:1},hash:None,cleanable:false};assert!(rules::compare(&record(1,"a/x"),&record(2,"b/x"),KeepPolicy::Newest).is_lt());}
#[test] fn plan_pagination_is_bounded(){let f=Fixture::new();for i in 0..260{f.write(&format!("{i:04}.txt"),b"same",i+100);}let task=f.plan(base());let db=Database::open(&task.directory).unwrap();let first=db.actions_page(0,100).unwrap();let second=db.actions_page(first.last().unwrap().id,100).unwrap();assert_eq!(first.len(),100);assert_eq!(second.len(),100);assert!(first.last().unwrap().id<second.first().unwrap().id);assert!(first.iter().all(|a|a.kind==ActionKind::Delete));}
#[cfg(unix)]
#[test] fn symlink_not_followed_or_deleted(){let f=Fixture::new();let outside=f._temp.path().join("outside");fs::create_dir(&outside).unwrap();fs::write(outside.join("a"),b"a").unwrap();std::os::unix::fs::symlink(&outside,f.root.join("link")).unwrap();assert!(fsutil::safe_join(&f.root,"link/a").is_err());assert_eq!(f.plan(base()).summary.scanned,0);}
#[test] fn existing_hardlinks_not_counted_twice(){let f=Fixture::new();let a=f.write("a",b"same",10);if fs::hard_link(&a,f.root.join("b")).is_err(){return;}let task=f.plan(base());assert_eq!(task.summary.candidate_bytes,0);assert_eq!(task.summary.planned_delete,0);}
#[test] fn hardlink_mode_preserves_aliases(){let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);let mut cfg=base();cfg.duplicate_action=DuplicateAction::Hardlink;let task=f.plan(cfg);let result=f.apply(&task);assert_eq!(result.summary.linked,1);assert_eq!(fsutil::snapshot(&f.root.join("a")).unwrap().identity,fsutil::snapshot(&f.root.join("b")).unwrap().identity);}
