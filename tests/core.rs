//! These tests mutate only tempfile fixtures. Recycle Bin operations are injected mocks.
mod common;
use common::FailRecycle;
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
struct CancelRecycle;
impl Recycler for CancelRecycle {fn recycle(&self,_:&Path)->Result<(),RecycleFailure>{Err(RecycleFailure::Cancelled)}}
struct MoveRecycle {target:PathBuf,calls:AtomicUsize}
impl Recycler for MoveRecycle {fn recycle(&self,p:&Path)->Result<(),RecycleFailure>{self.calls.fetch_add(1,Ordering::Relaxed);fs::rename(p,&self.target).map_err(|e|RecycleFailure::Failed(e.to_string()))}
    fn bin_count(&self,_:&Path)->Option<i64>{Some(self.calls.load(Ordering::Relaxed) as i64)}}
#[test] fn defaults_valid_and_roundtrip(){let cfg=Config::default();cfg.validate().unwrap();assert!(cfg.recycle_fallback);let dir=tempfile::tempdir().unwrap();let path=dir.path().join("config.json");cfg.save(&path).unwrap();let loaded=Config::load(&path).unwrap();assert_eq!(serde_json::to_value(&cfg).unwrap(),serde_json::to_value(&loaded).unwrap());}
#[test] fn reject_unknown_configuration(){let dir=tempfile::tempdir().unwrap();let path=dir.path().join("config.json");fs::write(&path,r#"{"delete_everything":true}"#).unwrap();assert!(Config::load(&path).is_err());}
#[test] fn legacy_config_with_removed_fields_still_loads(){let dir=tempfile::tempdir().unwrap();let path=dir.path().join("config.json");let mut value=serde_json::to_value(Config::default()).unwrap();value.as_object_mut().unwrap().insert("hash_algorithm".into(),"md5".into());value.as_object_mut().unwrap().insert("verify_bytes".into(),false.into());fs::write(&path,serde_json::to_vec(&value).unwrap()).unwrap();Config::load(&path).unwrap();}
#[test] fn schema_matches_every_configuration_field(){let cfg=serde_json::to_value(Config::default()).unwrap();let schema:serde_json::Value=serde_json::from_str(include_str!("../resources/rules.json")).unwrap();let keys=schema.as_array().unwrap();let fields=cfg.as_object().unwrap();
    // 有意不进规则表的字段：theme 在「关于」页；其余是界面合并行的影子键（一行驱动多个细粒度字段，引擎/CLI/旧任务库仍读原值）。
    // 去重三类（同名/副本名/不同名同内容）已按 R-04 拆为独立规则行，不再是影子键。
    let hidden=["theme","same_name_different_size","different_size_keep","detect_type"];
    assert_eq!(fields.len(),keys.len()+hidden.len());
    for row in keys{assert!(fields.get(row["key"].as_str().unwrap()).is_some());}
    for key in hidden{assert!(fields.contains_key(key),"{key} 应作为配置字段保留");}}
#[test] fn coupled_validation_and_bounds(){let mut c=base();c.fix_extension=true;assert!(c.validate().is_err());c.detect_type=true;assert!(c.validate().is_ok());c.hash_workers=0;assert!(c.validate().is_err());c.hash_workers=2;c.reserve_gib=u64::MAX;assert!(c.validate().is_err());}
#[test] fn unsafe_paths_rejected(){for value in ["../x","x/../../outside","C:/x","/etc/passwd",r"\\server\share\x","file:stream","CON.txt","a/NUL","x. ","a\n.txt",""]{assert!(fsutil::safe_relative(value).is_err(),"{value:?}");}}
#[test] fn relative_tar_and_unicode_paths_accepted(){assert_eq!(fsutil::safe_relative("./报告/a.pdf").unwrap(),PathBuf::from("报告/a.pdf"));assert_eq!(fsutil::safe_relative(r"资料\图片.png").unwrap(),PathBuf::from("资料/图片.png"));}
#[test] fn windows_reserved_names_rejected(){for s in ["con","COM1","LPT9.txt","nul","AUX.jpg","COM¹.txt"]{assert!(fsutil::validate_component(s).is_err());}assert!(fsutil::validate_component("COM10.txt").is_ok());assert!(fsutil::validate_component("COM0.txt").is_ok());}
#[test] fn trailing_unicode_whitespace_rejected(){assert!(fsutil::validate_component("a\u{3000}").is_err());assert!(fsutil::validate_component("a\u{a0}").is_err());assert!(fsutil::validate_component("a b").is_ok());}
#[test] fn recycle_without_bin_counts_as_unverified(){let f=Fixture::new();let p=f.write("a",b"a",1);let q=f.write("b",b"b",1);struct NoBinCount;impl Recycler for NoBinCount{fn recycle(&self,p:&Path)->Result<(),RecycleFailure>{fs::rename(p,p.with_extension("gone")).map_err(|e|RecycleFailure::Failed(e.to_string()))}}let s=fsutil::snapshot(&p).unwrap();let t=fsutil::snapshot(&q).unwrap();
    // 回收「成功」但拿不到回收站条目数（无论是否允许降级）都必须如实记为未验证。
    assert_eq!(platform::remove(&p,Some(&s),DeleteMode::Recycle,false,&Control::default(),&NoBinCount).unwrap(),DeleteResult::RecycledUnverified);
    assert_eq!(platform::remove(&q,Some(&t),DeleteMode::Recycle,true,&Control::default(),&NoBinCount).unwrap(),DeleteResult::RecycledUnverified);
    assert!(!p.exists()&&!q.exists());}
// 回收的「条目计数验证」是 Windows 专属实现：platform::volume_root 在非 Windows 恒为 None，
// 引擎不会去询问 bin_count，回收成功也只能记为 RecycledUnverified。下面两个用例断言的就是
// 这条 Windows 路径；非 Windows 上「回收成功但无法验证」由 recycle_without_bin_counts_as_unverified 覆盖。
#[cfg(windows)]
#[test] fn recycle_error_after_move_with_verified_bin_counts_as_recycled(){let f=Fixture::new();let p=f.write("a",b"a",1);struct LateFail{target:PathBuf,calls:AtomicUsize}impl Recycler for LateFail{fn recycle(&self,p:&Path)->Result<(),RecycleFailure>{self.calls.fetch_add(1,Ordering::Relaxed);fs::rename(p,&self.target).map_err(|e|RecycleFailure::Failed(e.to_string()))?;Err(RecycleFailure::Failed("late failure".into()))}fn bin_count(&self,_:&Path)->Option<i64>{Some(self.calls.load(Ordering::Relaxed) as i64)}}let r=LateFail{target:f.root.join("mock-bin-late"),calls:AtomicUsize::new(0)};let s=fsutil::snapshot(&p).unwrap();assert_eq!(platform::remove(&p,Some(&s),DeleteMode::Recycle,true,&Control::default(),&r).unwrap(),DeleteResult::Recycled);assert!(!p.exists()&&r.target.exists());}
#[test] fn metadata_change_invalidates_snapshot(){let f=Fixture::new();let p=f.write("a",b"one",1);let s=fsutil::snapshot(&p).unwrap();fsutil::unchanged(&p,&s).unwrap();fs::write(&p,b"different").unwrap();assert!(fsutil::unchanged(&p,&s).is_err());}
#[test] fn rename_never_overwrites(){let f=Fixture::new();let a=f.write("a",b"A",1);let b=f.write("b",b"B",2);assert!(fsutil::rename_noreplace(&a,&b).is_err());assert_eq!(fs::read(a).unwrap(),b"A");assert_eq!(fs::read(b).unwrap(),b"B");}
#[test] fn unique_name_preserves_extension(){let f=Fixture::new();let p=f.write("report.pdf",b"a",1);let q=fsutil::unique_target(&f.root,&p).unwrap();assert_eq!(q.file_name().unwrap(),"report (1).pdf");}
#[test] fn unique_name_truncates_long_stem_to_component_limit(){
    // 回归：基础名接近 255 个 UTF-16 单元且目标被占用时，“名 (N).扩展”候选名会超限，
    // validate_component 令 unique_target 整体失败——归类/解压冲突回退因此把整次任务
    // 或整包解压搞失败。应截断 stem 生成合法候选名，而不是放弃分配。
    let f=Fixture::new();
    // 基础名 251+4=255 恰好合法；加「 (1)」后 260 超限，旧行为会让 unique_target 失败。
    let stem="a".repeat(251);
    let p=f.write(&format!("{stem}.txt"),b"a",1);
    let q=fsutil::unique_target(&f.root,&p).unwrap();
    let name=q.file_name().unwrap().to_str().unwrap().to_string();
    assert!(name.encode_utf16().count()<=255,"分配名不得超 255 个 UTF-16 单元：{name}");
    assert!(name.ends_with(" (1).txt"),"保持扩展名与序号后缀：{name}");
    assert_ne!(q,p);
    // 截断只发生在超限时：常规名不受影响。
    let r=f.write("b.pdf",b"b",1);
    assert_eq!(fsutil::unique_target(&f.root,&r).unwrap().file_name().unwrap(),"b (1).pdf");
    // 直接钉住扩展名截断后的尾随空白剥离：截断点落在 NBSP 之后时不得留下 NBSP 结尾
    // （validate_component 拒一切 Unicode 尾随空白，trim 集必须与其同口径）。
    let ext=format!(".{}\u{a0}z","x".repeat(248));
    let n=fsutil::suffixed_candidate("a",&ext,1);
    assert!(n.encode_utf16().count()<=255,"{n}");
    assert!(!n.chars().last().is_some_and(|c|c.is_whitespace()),"不得以任何空白结尾：{n:?}");
    assert!(n.ends_with('x'),"截断剥掉 NBSP 后应露出的最后字符：{n:?}");
    // 扩展名超长（252 单元）同样不得放弃分配：序号优先，其次扩展名，再截 stem。
    let long_ext="x".repeat(251);
    let p2=f.write(&format!("a.{long_ext}"),b"a",1);
    let q2=fsutil::unique_target(&f.root,&p2).unwrap();
    let n2=q2.file_name().unwrap().to_str().unwrap().to_string();
    assert!(n2.encode_utf16().count()<=255,"分配名不得超 255 个 UTF-16 单元：{n2}");
    assert!(n2.contains(" (1).")&&n2.starts_with('a'),"保留序号与扩展名起点：{n2}");}
#[test] fn hashes_known_vectors(){let f=Fixture::new();let p=f.write("a",b"abc",1);let s=fsutil::snapshot(&p).unwrap();let ctl=Control::default();assert_eq!(hashing::full_hash(&p,&s,&ctl).unwrap(),"blake3:6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85");}
#[test] fn same_prehash_different_middle_is_not_duplicate(){let f=Fixture::new();let a=vec![7u8;300_000];let mut b=a.clone();b[150_000]=8;let ap=f.write("a",&a,1);let bp=f.write("b",&b,2);let sa=fsutil::snapshot(&ap).unwrap();let sb=fsutil::snapshot(&bp).unwrap();let ctl=Control::default();assert_eq!(hashing::prehash(&ap,&sa,&ctl).unwrap(),hashing::prehash(&bp,&sb,&ctl).unwrap());assert_ne!(hashing::full_hash(&ap,&sa,&ctl).unwrap(),hashing::full_hash(&bp,&sb,&ctl).unwrap());assert_eq!(f.plan(base()).summary.planned_delete,0);}
#[test] fn cancelled_hash_does_not_read(){let f=Fixture::new();let p=f.write("a",b"abc",1);let ctl=Control::default();ctl.cancel();assert!(hashing::full_hash(&p,&fsutil::snapshot(&p).unwrap(),&ctl).is_err());assert_eq!(ctl.read_bytes.load(Ordering::Relaxed),0);}
#[test] fn copy_suffixes_and_nonempty_name(){for s in ["报告 (1).pdf","报告（2）.pdf","报告 - Copy.pdf","报告 副本.pdf"]{assert_eq!(rules::strip_copy_name(s),"报告.pdf");}assert_eq!(rules::strip_copy_name("(1).pdf"),"(1).pdf");}
#[test] fn category_and_custom_rules(){assert_eq!(rules::category("pdf"),"文档");assert_eq!(rules::category("ts"),"代码");assert_eq!(rules::parse_categories("工程=rs,sv;资料=pdf").unwrap()["sv"],"工程");assert!(rules::parse_categories("../x=pdf").is_err());}
#[test] fn archive_first_volume_detection(){assert!(rules::archive_name("A.part01.rar"));assert!(!rules::archive_name("A.part02.rar"));assert!(rules::archive_name("A.7z.001"));assert!(!rules::archive_name("A.7z.002"));assert!(rules::archive_name("A.tar.gz"));}
#[test] fn recycle_failure_without_permission_keeps_file(){let f=Fixture::new();let p=f.write("a",b"a",1);let s=fsutil::snapshot(&p).unwrap();assert!(platform::remove(&p,Some(&s),DeleteMode::Recycle,false,&Control::default(),&FailRecycle).is_err());assert!(p.exists());}
#[test] fn recycle_failure_with_permission_deletes(){let f=Fixture::new();let p=f.write("a",b"a",1);let s=fsutil::snapshot(&p).unwrap();assert_eq!(platform::remove(&p,Some(&s),DeleteMode::Recycle,true,&Control::default(),&FailRecycle).unwrap(),DeleteResult::Permanent);assert!(!p.exists());}
#[test] fn user_cancel_never_falls_back_to_delete(){let f=Fixture::new();let p=f.write("a",b"a",1);assert!(platform::remove(&p,None,DeleteMode::Recycle,true,&Control::default(),&CancelRecycle).is_err());assert!(p.exists());}
#[cfg(windows)]
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
#[test] fn cleanup_keep_files_are_not_conflict_versions(){
    // cleanup_delete=Keep 的清理命中文件由清理规则管辖（保留承诺）：
    // 不得被选为冲突 keeper（否则正常版本被删、垃圾留下），也不得作为冲突版本删除。
    let f=Fixture::new();
    f.write("x/a.txt",b"",20);
    f.write("y/a.txt",b"payload",10);
    let mut cfg=base();
    cfg.clean_zero=true;
    cfg.cleanup_delete=DeleteChoice::Keep;
    cfg.same_name_different_size=true;
    cfg.conflict_scope_directory=false;
    let task=f.plan(cfg);
    let actions=Database::open(&task.directory).unwrap().actions_page(0,100).unwrap();
    assert!(actions.iter().all(|a|a.kind!=ActionKind::Delete),"冲突取舍不得删除清理保留的文件");
    f.apply(&task);
    assert!(f.root.join("x/a.txt").exists()&&f.root.join("y/a.txt").exists());
}
#[test] fn cleanup_keep_files_are_not_dedup_deletions(){
    // cleanup_delete=Keep 的清理命中文件由清理规则管辖（保留承诺）：去重路径同样不得删除。
    // 此前只防了「不得充当 keeper」，keeper 先注册时它作为重复项会按 duplicate_delete 被删，
    // 结果随 duplicate_order 排序翻转（本用例让 junk.tmp 排在 keeper 之后触发原缺陷）。
    let f=Fixture::new();
    f.write("normal.txt",b"payload",20);   // 较新 → 成为 keeper
    f.write("junk.tmp",b"payload",10);     // 较旧且命中 clean_temp → 修复前被按重复删除
    let mut cfg=base();
    cfg.clean_temp=true;
    cfg.cleanup_delete=DeleteChoice::Keep;
    let task=f.plan(cfg);
    let actions=Database::open(&task.directory).unwrap().actions_page(0,100).unwrap();
    assert!(actions.iter().all(|a|a.kind!=ActionKind::Delete),"清理保留的文件不得按重复规则删除");
    f.apply(&task);
    assert!(f.root.join("normal.txt").exists()&&f.root.join("junk.tmp").exists());
}
#[test] fn stale_hardlink_temps_are_swept(){
    // 崩溃残留的硬链接临时文件被扫描永久剪枝且无其它回收路径：
    // prepare/apply 前必须清扫过期残留。残留必是硬链接（内容仍由保留文件持有），
    // 清扫前校验链接数 >= 2，普通同名文件绝不能被当作残留删除。
    let f=Fixture::new();f.write("a.txt",b"payload",10);
    let stale=f.root.join(".jchtools-link-deadbeef");
    if let Err(error)=fs::hard_link(f.root.join("a.txt"),&stale){
        eprintln!("hard_link failed: {error}; keeper={:?}",f.root.join("a.txt"));
        panic!("无法创建硬链接，无法验证残留清扫；请在支持硬链接的文件系统上运行测试");
    }
    filetime::set_file_mtime(&stale,filetime::FileTime::from_unix_time(0,0)).unwrap();
    let task=f.plan(base());
    assert!(!stale.exists(),"过期残留必须被清扫");
    assert!(f.root.join("a.txt").exists(),"清扫残留不得影响 keeper 本体");
    assert_eq!(task.summary.scanned,1,"清扫不得影响正常文件的扫描");
}
#[test] fn user_file_with_link_temp_prefix_is_never_swept(){
    // 回归：.jchtools-link- 前缀清扫此前不校验归属，同名用户文件（或从压缩包解出的
    // 同名成员）会被静默永久删除。崩溃残留必是硬链接（链接数 >= 2，内容仍有其他
    // 链接持有）；普通同名文件不属于本工具命名空间，必须原样保留。
    let f=Fixture::new();f.write("a.txt",b"payload",10);
    let user=f.root.join(".jchtools-link-mydata");
    fs::write(&user,b"precious").unwrap();
    filetime::set_file_mtime(&user,filetime::FileTime::from_unix_time(0,0)).unwrap();
    f.plan(base());
    assert!(user.exists(),"同名用户文件不是崩溃残留，绝不能被清扫");
}
#[test] fn global_keep_prohibits_deletion(){let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);let mut cfg=base();cfg.global_delete=DeleteMode::Keep;assert_eq!(f.plan(cfg).summary.planned_delete,0);}
#[test] fn class_override_beats_global_keep(){let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);let mut cfg=base();cfg.global_delete=DeleteMode::Keep;cfg.duplicate_delete=DeleteChoice::Permanent;assert_eq!(f.plan(cfg).summary.planned_delete,1);}
#[test] fn copy_name_cleanup_uses_freed_original_name(){let f=Fixture::new();f.write("a.pdf",b"same",10);f.write("a (1).pdf",b"same",20);let mut cfg=base();cfg.clean_copy_name=true;let task=f.plan(cfg);f.apply(&task);assert!(f.root.join("a.pdf").exists());assert!(!f.root.join("a (1).pdf").exists());}
#[test] fn classification_preserves_paths_and_is_idempotent(){let f=Fixture::new();f.write("folder/a.pdf",b"pdf",10);let mut cfg=base();cfg.classify=ClassifyMode::Extension;let task=f.plan(cfg.clone());f.apply(&task);assert!(f.root.join("PDF/folder/a.pdf").exists());let again=f.plan(cfg);assert_eq!(again.summary.planned_move,0);}
#[test] fn flatten_classification_allocates_nonconflicting_names(){let f=Fixture::new();f.write("x/a.pdf",b"left",10);f.write("y/a.pdf",b"right",20);let mut cfg=base();cfg.classify=ClassifyMode::Extension;cfg.preserve_structure=false;let task=f.plan(cfg);f.apply(&task);assert!(f.root.join("PDF/a.pdf").exists());assert!(f.root.join("PDF/a (1).pdf").exists());}
#[test] fn empty_directory_cleanup_is_bottom_up(){let f=Fixture::new();fs::create_dir_all(f.root.join("empty/nested")).unwrap();let mut cfg=base();cfg.clean_empty_dirs=true;let task=f.plan(cfg);f.apply(&task);assert!(!f.root.join("empty").exists());assert!(f.root.exists());}
#[test] fn empty_hidden_subdir_blocks_empty_directory_cleanup(){
    // 行为锚点：仅含一个空的隐藏（Windows）/点开头（Unix）子目录的目录，
    // 该子目录不会入库，目录对规划"并非实际为空"，不得计划为空目录。
    // 否则计划承诺落空：执行期实空复查只能跳过，skipped 虚增、该清的没清。
    let f=Fixture::new();
    fs::create_dir_all(f.root.join("outer/.inner")).unwrap();
    #[cfg(windows)]
    set_hidden(&f.root.join("outer/.inner"),true);
    f.write("keep.txt",b"payload",10);
    let mut cfg=base();cfg.clean_empty_dirs=true;
    let task=f.plan(cfg);
    assert_eq!(task.summary.planned_empty,0,"含未入库子目录的目录不得计划为空目录");
    assert!(f.root.join("outer").exists());
}
#[test] fn empty_directory_with_underscore_not_blocked_by_similar_name(){
    // 行为契约：目录名含下划线/百分号时，空目录判定不得波及名字相似（仅差一两个字符）的邻居目录。
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
#[cfg(windows)]
fn set_hidden(path:&Path,hidden:bool){
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW,SetFileAttributesW,FILE_ATTRIBUTE_HIDDEN,INVALID_FILE_ATTRIBUTES};
    let wide=path.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let attrs=unsafe{GetFileAttributesW(wide.as_ptr())};
    assert_ne!(attrs,INVALID_FILE_ATTRIBUTES,"读取属性失败：{path:?}");
    let next=if hidden{attrs|FILE_ATTRIBUTE_HIDDEN}else{attrs&!FILE_ATTRIBUTE_HIDDEN};
    assert_ne!(unsafe{SetFileAttributesW(wide.as_ptr(),next)},0,"设置属性失败：{path:?}");
}
#[cfg(windows)]
#[test] fn hidden_root_directory_still_scanned(){
    // 行为锚点：扫描以 walkdir min_depth(1) 运行，根条目不会被产出，filter_entry 谓词
    // 因此从不作用于用户选定的根目录——隐藏根目录不会整树剪枝；而根目录下的隐藏
    // 子目录仍按隐藏判定剪枝。两条 walkdir 语义在此一并钉住。
    let f=Fixture::new();f.write("a.txt",b"payload",10);f.write("secret/b.txt",b"hidden",20);
    set_hidden(&f.root,true);
    set_hidden(&f.root.join("secret"),true);
    let task=f.plan(base());
    set_hidden(&f.root,false);
    set_hidden(&f.root.join("secret"),false);
    assert_eq!(task.summary.scanned,1,"隐藏根目录里的普通文件必须能被扫描到；隐藏子目录必须被剪枝");
}
#[cfg(not(windows))]
#[test] fn hidden_root_directory_still_scanned(){
    // 行为锚点：扫描以 walkdir min_depth(1) 运行，根条目不会被产出，filter_entry 谓词
    // 因此从不作用于用户选定的根目录——点开头根目录不会整树剪枝；根下的点开头
    // 子目录仍按隐藏判定剪枝。
    let temp=tempfile::tempdir().unwrap();
    let root=temp.path().join(".hidden-root");
    fs::create_dir_all(root.join(".secret")).unwrap();
    fs::write(root.join("a.txt"),b"payload").unwrap();
    fs::write(root.join(".secret/b.txt"),b"hidden").unwrap();
    let state=temp.path().join("state");
    let task=engine::prepare_at(&root,base(),Context::default(),&state,None).unwrap();
    assert_eq!(task.summary.scanned,1,"点开头根目录里的普通文件必须能被扫描到；点开头子目录必须被剪枝");
}
#[test] fn merge_directories_never_pulls_files_out_of_output(){
    // 输出目录内的分类子目录与树中同名外部目录重名时，合并不得把已归类文件拉回
    // 外部目录：否则归类与合并跨运行互相拉扯，计划永不收敛。
    let f=Fixture::new();
    f.write("文档/old/a.pdf",b"pdf",10);
    let mut cfg=base();
    cfg.classify=ClassifyMode::Extension;
    cfg.preserve_structure=true;
    cfg.merge_directories=true;
    cfg.output_dir="整理".into();
    let task=f.plan(cfg.clone());
    assert_eq!(task.summary.planned_move,1);
    f.apply(&task);
    assert!(f.root.join("整理/PDF/文档/old/a.pdf").exists());
    let again=f.plan(cfg);
    assert_eq!(again.summary.planned_move,0,"第二次分析不得把已归类文件再移回外部同名目录");
}
#[test] fn copy_name_cleanup_never_plans_self_move(){
    // 保留文件剥离副本名后原名被其它内容占用时，回退序号不得撞回自身当前名称：
    // source==target 的空转移动破坏计划幂等（执行后 moved 计数虚高）。
    let f=Fixture::new();
    f.write("报告 (1).pdf",b"A",30);
    f.write("报告 (2).pdf",b"A",20);
    f.write("报告.pdf",b"B",10);
    let mut cfg=base();cfg.clean_copy_name=true;
    let task=f.plan(cfg);
    assert_eq!(task.summary.planned_delete,1,"同内容副本应被删除");
    assert_eq!(task.summary.planned_move,0,"保留者已在合理位置，不得生成移动动作");
    f.apply(&task);
    assert!(f.root.join("报告 (1).pdf").exists());
    assert!(!f.root.join("报告 (2).pdf").exists());
    assert!(f.root.join("报告.pdf").exists());
}
#[test] fn hardlink_does_not_claim_permanent_bytes(){
    // 硬链接去重不销毁内容（源目录项由指向 keeper 的链接顶替），物理占用不变：
    // 即使删除模式解析为 Permanent，也不得把源大小计入 permanent_bytes。
    let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);
    let mut cfg=base();cfg.duplicate_action=DuplicateAction::Hardlink;
    let task=f.plan(cfg);
    let result=f.apply(&task);
    assert_eq!(result.summary.linked,1);
    assert_eq!(result.summary.deleted,1,"硬链接替换仍按删除项数记账");
    assert_eq!(result.summary.permanent_bytes,0,"硬链接不释放物理空间，不得计入永久删除字节");
}
#[test] fn no_recursion_leaves_subdirectories_untouched(){let f=Fixture::new();f.write("a",b"same",10);f.write("sub/b",b"same",20);let mut cfg=base();cfg.recursive=false;assert_eq!(f.plan(cfg).summary.scanned,1);}
#[test] fn finished_plan_cannot_be_replayed(){let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);let task=f.plan(base());f.apply(&task);assert!(engine::apply_with(&task.directory,Context::default(),Arc::new(FailRecycle)).is_err());}
#[test] fn task_lock_prevents_second_task(){let f=Fixture::new();let _guard=fsutil::RootGuard::acquire(&f.state).unwrap();assert!(engine::prepare_at(&f.root,base(),Context::default(),&f.state,None).is_err());}
#[test] fn user_cancelled_analysis_records_cancelled_status(){
    // 用户取消不是故障：任务状态必须写成 cancelled，否则事后检查任务库会把主动取消当成失败。
    let f=Fixture::new();f.write("a.txt",b"payload",10);
    let context=Context::default();context.control.cancel();
    assert!(engine::prepare_at(&f.root,base(),context,&f.state,None).is_err());
    let directory=fs::read_dir(f.state.join("tasks")).unwrap().next().unwrap().unwrap().path();
    let db=Database::open(&directory).unwrap();
    assert_eq!(db.get::<String>("status").unwrap(),"cancelled");
}
#[test] fn report_does_not_overwrite(){let f=Fixture::new();let task=f.plan(base());let report=f.root.join("report.csv");let db=Database::open(&task.directory).unwrap();db.export_csv(&report).unwrap();assert!(db.export_csv(&report).is_err());}
#[test] fn huge_sizes_use_u64(){assert_eq!(jchtools::model::bytes(10u64<<40),"10.00 TiB");let f=Fixture::new();let db=Database::create(&f.state).unwrap();let snapshot=Snapshot{size:12u64<<40,modified_ns:1,identity:"mock".into(),links:1};db.insert_file("large.bin","large.bin","large.bin",&snapshot).unwrap();assert_eq!(db.file(1).unwrap().snapshot.size,12u64<<40);}
#[test] fn deterministic_keeper_ties(){let record=|id,rel:&str|FileRecord{id,rel:rel.into(),name:"x".into(),normalized:"x".into(),snapshot:Snapshot{size:1,modified_ns:10,identity:id.to_string(),links:1},hash:None,cleanable:false};assert!(rules::compare(&record(1,"a/x"),&record(2,"b/x"),KeepPolicy::Newest).is_lt());}
#[test] fn plan_pagination_is_bounded(){let f=Fixture::new();for i in 0..260{f.write(&format!("{i:04}.txt"),b"same",i+100);}let task=f.plan(base());let db=Database::open(&task.directory).unwrap();let first=db.actions_page(0,100).unwrap();let second=db.actions_page(first.last().unwrap().id,100).unwrap();assert_eq!(first.len(),100);assert_eq!(second.len(),100);assert!(first.last().unwrap().id<second.first().unwrap().id);assert!(first.iter().all(|a|a.kind==ActionKind::Delete));}
// 平台门禁原因：创建符号链接在 Windows 需管理员/开发者模式特权，Unix 无需特权即可稳定构造；
// safe_join 拒绝链接穿越的断言只能在 Unix 下验证。
#[cfg(unix)]
#[test] fn symlink_not_followed_or_deleted(){let f=Fixture::new();let outside=f._temp.path().join("outside");fs::create_dir(&outside).unwrap();fs::write(outside.join("a"),b"a").unwrap();std::os::unix::fs::symlink(&outside,f.root.join("link")).unwrap();assert!(fsutil::safe_join(&f.root,"link/a").is_err());assert_eq!(f.plan(base()).summary.scanned,0);}
#[test] fn existing_hardlinks_not_counted_twice(){
    let f=Fixture::new();let a=f.write("a",b"same",10);
    if let Err(error)=fs::hard_link(&a,f.root.join("b")){
        // 静默 return 会让测试永远绿：硬链接失败必须显式失败（FAT32/exFAT 不支持时请换 NTFS/tmpfs 环境）。
        eprintln!("hard_link failed: {error}; a={a:?}; root={:?}",f.root);
        panic!("无法创建硬链接，无法验证去重行为；请在支持硬链接的文件系统上运行测试");
    }
    let task=f.plan(base());assert_eq!(task.summary.candidate_bytes,0);assert_eq!(task.summary.planned_delete,0);
}
#[test] fn hardlink_mode_preserves_aliases(){let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);let mut cfg=base();cfg.duplicate_action=DuplicateAction::Hardlink;let task=f.plan(cfg);let result=f.apply(&task);assert_eq!(result.summary.linked,1);assert_eq!(fsutil::snapshot(&f.root.join("a")).unwrap().identity,fsutil::snapshot(&f.root.join("b")).unwrap().identity);}

// ===== 核心公共接口缺口补测（配置原子覆盖写 / CSV 注入转义 / 分卷识别）=====
#[test] fn config_save_overwrites_existing_file(){
    // write_json_atomic 的覆盖分支：同一 config.json 第二次保存必须生效（临时文件 + 原子替换）。
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("config.json");
    let mut first=Config::default();first.hash_workers=3;
    let mut second=Config::default();second.hash_workers=5;
    first.save(&path).unwrap();second.save(&path).unwrap();
    let loaded=Config::load(&path).unwrap();
    assert_eq!(loaded.hash_workers,5,"后一次保存必须覆盖前一次");
}
#[test] fn export_csv_escapes_formula_prefixes(){
    // 表格软件会把以 = 开头的单元格当公式执行：导出 CSV 时不可信字段必须转义。
    // 同时覆盖不可见前缀伪装：U+FEFF 等格式字符跟在危险字符前时仍须判定为公式注入。
    let f=Fixture::new();let task=f.plan(base());
    let db=Database::open(&task.directory).unwrap();
    db.log("删除","=cmd|'/c calc'!a1","","准备","注入尝试",7).unwrap();
    db.log("删除","\u{FEFF}=cmd|'/c calc'!a1","\u{200B}+cmd","准备","不可见前缀注入",9).unwrap();
    db.log("删除"," \u{FEFF}=1+2","","准备","空白格式包裹注入",9).unwrap();
    let csv=f.root.join("inject.csv");
    db.export_csv(&csv).unwrap();
    let text=fs::read_to_string(&csv).unwrap();
    let plain=text.lines().find(|line|line.contains("calc")&&!line.contains("不可见")).expect("注入样本应出现在导出中");
    assert!(plain.contains("'=cmd"),"以 = 开头的不可信字段必须被转义：{plain}");
    let stealth=text.lines().find(|line|line.contains("不可见")).expect("不可见前缀样本应出现在导出中");
    assert!(stealth.contains("'\u{FEFF}=cmd"),"U+FEFF 前缀伪装的公式必须被转义：{stealth}");
    assert!(stealth.contains("'\u{200B}+cmd"),"U+200B 前缀伪装的公式必须被转义：{stealth}");
    let wrapped=text.lines().find(|line|line.contains("空白格式包裹")).expect("空白包裹样本应出现在导出中");
    assert!(wrapped.contains("' \u{FEFF}=1+2"),"空白在格式字符之前的伪装公式必须被转义：{wrapped}");
}
#[test] fn multipart_name_covers_volume_detection_corners(){
    assert!(rules::multipart_name("x.part1.rar"));
    assert!(rules::multipart_name("x.part99.rar"));
    assert!(!rules::multipart_name("x.rar"));
    assert!(rules::multipart_name("x.7z.001"));
    assert!(!rules::multipart_name("x.7z.002"));
}

// ===== H-11 并发与多进程访问：状态目录锁 / set_selected 竞态 / reserve_target =====
#[test] fn prepare_and_apply_share_exclusive_state_lock(){
    // prepare 与 apply 共用同一 RootGuard：任一持有锁时，另一路必须失败（锁测试增强）。
    let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);
    let task=f.plan(base());
    {
        let _guard=fsutil::RootGuard::acquire(&f.state).unwrap();
        assert!(engine::prepare_at(&f.root,base(),Context::default(),&f.state,None).is_err(),"锁被持有时二次 prepare 必须失败");
        assert!(engine::apply_with(&task.directory,Context::default(),Arc::new(FailRecycle)).is_err(),"锁被持有时 apply 必须失败");
    }
    // 锁释放后 apply 可以正常完成
    f.apply(&task);
}
#[test] fn apply_after_task_dir_moved_still_uses_prepare_state_lock(){
    // 回归：apply 此前按「任务目录当前位置」推导锁位置；任务目录被移动到 state 之外后，
    // 旧计划的执行会与新的 prepare 失去互斥。prepare 把全局锁目录记进任务库后，
    // apply 必须优先锁记录的位置（记录缺失或该目录已不存在时才退回当前位置推导）。
    let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);
    let task=f.plan(base());
    let elsewhere=tempfile::tempdir().unwrap();let moved=elsewhere.path().join("moved-task");
    fs::rename(&task.directory,&moved).unwrap();
    {
        let _guard=fsutil::RootGuard::acquire(&f.state).unwrap();
        assert!(engine::apply_with(&moved,Context::default(),Arc::new(FailRecycle)).is_err(),
            "任务目录被移动后，apply 仍必须锁 prepare 记录的全局锁目录");
    }
    // 记录位置与当前位置一致（移回原位）时正常流程不受影响
    fs::rename(&moved,&task.directory).unwrap();
    f.apply(&task);
}
#[test] fn set_selected_fails_once_apply_started(){
    // apply 启动后 status 变为 executing；此时修改选择必须失败（set_selected 竞态）。
    let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);
    let task=f.plan(base());
    let context=Context::default();
    context.control.pause(true);
    let dir=task.directory.clone();
    let apply_handle={
        let context=context.clone();
        std::thread::spawn(move||engine::apply_with(&dir,context,Arc::new(FailRecycle)))
    };
    // 轮询等待 apply 写入 status=executing（固定 sleep 在高负载/CI 抢占下可能误失败）
    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
    let db=Database::open(&task.directory).unwrap();
    loop{
        if db.get::<String>("status").unwrap()=="executing"{break;}
        assert!(std::time::Instant::now()<deadline,"等待 apply 进入 executing 超时");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(db.get::<String>("status").unwrap(),"executing","apply 已进入执行状态");
    let id=db.actions_page(0,10).unwrap().remove(0).id;
    assert!(db.set_selected(id,false).is_err(),"执行中不得修改计划选择");
    context.control.pause(false);
    apply_handle.join().unwrap().unwrap();
}
#[test] fn set_selected_rejected_after_apply_finished(){
    // apply 完成后 status=finished；set_selected 必须失败（不可重放旧计划的选择变更）。
    let f=Fixture::new();f.write("a",b"same",10);f.write("b",b"same",20);
    let task=f.plan(base());f.apply(&task);
    let db=Database::open(&task.directory).unwrap();
    assert_eq!(db.get::<String>("status").unwrap(),"finished");
    let id=db.actions_page(0,10).unwrap().remove(0).id;
    assert!(db.set_selected(id,true).is_err());
}
#[test] fn reserve_target_is_case_insensitive_unique(){
    let f=Fixture::new();let db=Database::create(&f.state.join("reserve-db")).unwrap();
    assert!(db.reserve_target("Reports/Final.PDF",1).unwrap(),"首次预留必须成功");
    // 大小写折叠仅 Windows：大小写敏感文件系统上仅大小写不同的路径是不同目标。
    if cfg!(windows) {
        assert!(!db.reserve_target("reports/final.pdf",2).unwrap(),"大小写不同的同一路径必须视为冲突");
        assert!(!db.reserve_target("REPORTS/final.PDF",3).unwrap());
    } else {
        assert!(db.reserve_target("reports/final.pdf",2).unwrap(),"大小写敏感文件系统上不同大小写路径应可并存");
        assert!(db.reserve_target("REPORTS/final.PDF",3).unwrap());
    }
    assert!(db.reserve_target("reports/other.pdf",4).unwrap(),"不同路径可以预留");
    assert!(db.reserve_target("other.pdf",5).unwrap());
}

// ===== L5 actions_page_filtered 直接测 =====
#[test] fn actions_page_filtered_by_kind_and_rejects_unknown(){
    let f=Fixture::new();f.write("folder/a.pdf",b"pdf",10);
    let mut cfg=base();cfg.classify=ClassifyMode::Extension;
    let task=f.plan(cfg);
    let db=Database::open(&task.directory).unwrap();
    let moves=db.actions_page_filtered(0,100,Some("move")).unwrap();
    assert_eq!(moves.len(),1);
    assert_eq!(moves[0].kind,ActionKind::Move);
    let deletes=db.actions_page_filtered(0,100,Some("delete")).unwrap();
    assert!(deletes.is_empty(),"纯归类任务不应有删除动作");
    let all=db.actions_page(0,100).unwrap();
    assert_eq!(all.len(),moves.len()+deletes.len());
    assert!(db.actions_page_filtered(0,100,Some("unknown")).is_err(),"白名单外的 kind 必须直接报错");
}

// ===== L6 control::pause / checkpoint 暂停语义 =====
#[test] fn pause_defers_checkpoint_until_resume(){
    let ctl=std::sync::Arc::new(Control::default());
    ctl.checkpoint().unwrap(); // 未暂停时 checkpoint 应直接通过
    ctl.pause(true);
    assert!(ctl.is_paused());
    let (done_tx,done_rx)=std::sync::mpsc::channel();
    // started 握手：worker 在调用 checkpoint 前先报到，避免调度延迟让「无 done」假通过
    let (started_tx,started_rx)=std::sync::mpsc::channel();
    let worker={
        let c=ctl.clone();
        std::thread::spawn(move||{let _=started_tx.send(());c.checkpoint().unwrap();let _=done_tx.send(());})
    };
    started_rx.recv_timeout(std::time::Duration::from_secs(2)).expect("worker 应已进入 checkpoint 前置");
    // 暂停期间 checkpoint 不应完成
    assert!(done_rx.recv_timeout(std::time::Duration::from_millis(150)).is_err(),"暂停期间 checkpoint 不应返回");
    ctl.pause(false);
    done_rx.recv_timeout(std::time::Duration::from_secs(2)).expect("恢复后 checkpoint 应完成");
    worker.join().unwrap();
}
#[test] fn cancel_while_paused_makes_checkpoint_fail(){
    let ctl=std::sync::Arc::new(Control::default());
    ctl.pause(true);
    let (done_tx,done_rx)=std::sync::mpsc::channel();
    // started 握手：worker 报到后再 cancel，覆盖「已进入 checkpoint 暂停环」路径
    let (started_tx,started_rx)=std::sync::mpsc::channel();
    let worker={
        let c=ctl.clone();
        std::thread::spawn(move||{let _=started_tx.send(());let failed=c.checkpoint().is_err();let _=done_tx.send(failed);})
    };
    started_rx.recv_timeout(std::time::Duration::from_secs(2)).expect("worker 应已进入 checkpoint 前置");
    // started 只表示即将调用 checkpoint；短暂等待让 worker 进入暂停等待环后再取消
    std::thread::sleep(std::time::Duration::from_millis(20));
    ctl.cancel();
    assert_eq!(done_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),true,"暂停中取消必须让 checkpoint 失败");
    worker.join().unwrap();
}
