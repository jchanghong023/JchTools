use crate::{archive::{self, SevenZip}, config::{self, Config, DeleteMode}, control::{Context as TaskContext, Event},
    db::{Database, FILE_COLUMNS}, fsutil, hashing, model::{Action,ActionKind,Snapshot,Summary},
    planner, platform::{self,DeleteResult,NativeRecycler,Recycler}, rules};
use anyhow::{bail,Context,Result};
use rayon::prelude::*;
use rusqlite::params;
use std::{fs, path::{Path,PathBuf}, sync::{atomic::Ordering,Arc}};

pub struct Job {
    pub root: PathBuf,
    pub config: Config,
    pub context: TaskContext,
    pub db: Database,
    pub summary: Summary,
    pub archive_override: Option<crate::config::ConflictPolicy>,
    pub recycler: Arc<dyn Recycler>,
}
impl Job {
    pub fn log(&self,phase:&str,source:&str,target:&str,result:&str,reason:&str,size:u64)->Result<()> {
        self.db.log(phase,source,target,result,reason,size)?;
        self.context.emit(Event::Log(format!("[{phase}] {result} | {source}{} | {reason}",if target.is_empty(){String::new()}else{format!(" → {target}")})));
        Ok(())
    }
    pub fn delete_path(&mut self,path:&Path,expected:Option<&Snapshot>,mode:DeleteMode,reason:&str)->Result<DeleteResult> {
        let relative = fsutil::relative_string(&self.root,path)?;
        fsutil::safe_join(&self.root,&relative)?;
        let size = expected.map(|s|s.size).unwrap_or(0);
        // Record intent before a mutation. This is an audit trail, not a recovery journal.
        self.log("删除",&relative,"","准备",reason,size)?;
        let result = platform::remove(path,expected,mode,self.config.recycle_fallback,&self.context.control,self.recycler.as_ref())?;
        match result {
            DeleteResult::Kept => { self.summary.skipped += 1; },
            DeleteResult::Recycled => { self.summary.recycled += 1; self.summary.recycled_bytes = self.summary.recycled_bytes.saturating_add(size); },
            DeleteResult::Permanent => { self.summary.deleted += 1; if expected.is_none_or(|s|s.links <= 1) {
                self.summary.permanent_bytes = self.summary.permanent_bytes.saturating_add(size);
            } },
        }
        self.log("删除",&relative,"",match result {DeleteResult::Kept=>"保留",DeleteResult::Recycled=>"已回收",DeleteResult::Permanent if mode==DeleteMode::Recycle=>"回收失败，已按授权永久删除",DeleteResult::Permanent=>"已永久删除"},reason,size)?;
        Ok(result)
    }
}
#[derive(Debug,Clone)]
pub struct TaskResult { pub directory: PathBuf,pub summary: Summary }
pub fn prepare(root:&Path,config:Config,context:TaskContext)->Result<TaskResult> {
    prepare_at(root,config,context,&config::state_dir()?,None)
}
/// A separate state directory and explicit engine path make the core testable without the GUI.
pub fn prepare_at(root:&Path,config:Config,context:TaskContext,state:&Path,engine_path:Option<&Path>)->Result<TaskResult> {
    config.validate()?;
    let root = fsutil::normalize_root(root)?;
    let _guard = fsutil::RootGuard::acquire(state)?;
    let directory = state.join("tasks").join(format!("{}-{}",chrono::Utc::now().format("%Y%m%dT%H%M%S"),uuid::Uuid::new_v4()));
    let db = Database::create(&directory)?;
    db.set("root",&fsutil::path_string(&root)?)?; db.set("config",&config)?;
    db.set("status",&"analyzing")?; db.set("summary",&Summary::default())?;
    db.set("created",&chrono::Utc::now().to_rfc3339())?;
    let mut job = Job { root,config,context,db,summary:Summary::default(),archive_override:None,recycler:Arc::new(NativeRecycler) };
    let result = (|| {
        scan(&mut job,true,state)?;
        if job.config.extract {
            let count:i64 = job.db.conn.query_row("SELECT COUNT(*) FROM archives WHERE state='pending'",[],|r|r.get(0))?;
            if count > 0 {
                let engine = match engine_path {Some(path)=>SevenZip::with_executable(path)?,None=>SevenZip::from_bundle()?};
                archive::extract_queued(&mut job,&engine)?;
                // Only real extracted files participate in this analysis; no pre-extraction duplicate estimates.
                scan(&mut job,false,state)?;
            }
        }
        // Failure originals, skipped originals and explicitly kept sources are never silently deleted later.
        if job.config.archive_delete.resolve(job.config.global_delete) == DeleteMode::Keep {
            job.db.conn.execute("UPDATE files SET active=0 WHERE rel IN (SELECT rel FROM archives)",[])?;
        } else {
            job.db.conn.execute("UPDATE files SET active=0 WHERE rel IN (SELECT rel FROM archives WHERE state<>'done')",[])?;
        }
        job.db.conn.execute("UPDATE files SET active=0 WHERE rel IN (SELECT rel FROM protected_originals)",[])?;
        hash_candidates(&mut job)?;
        job.context.status("生成去重、冲突、归类和清理计划（尚未执行这些操作）");
        job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
        match planner::build(&mut job) {
            Ok(())=>job.db.conn.execute_batch("COMMIT")?,
            Err(error)=>{ let _=job.db.conn.execute_batch("ROLLBACK");return Err(error); }
        }
        job.db.set("summary",&job.summary)?; job.db.set("status",&"ready")?;
        job.log("计划","","","待确认","解压阶段已完成；去重、移动和清理等待确认",0)?;
        Ok(TaskResult { directory:directory.clone(),summary:job.summary.clone() })
    })();
    if let Err(error)=&result {
        let _=job.db.set("summary",&job.summary); let _=job.db.set("status",&"failed");
        let _=job.log("任务","","","失败",&format!("{error:#}"),0);
    }
    result
}
fn scan(job:&mut Job,enqueue:bool,state:&Path)->Result<()> {
    job.context.status(if enqueue {"扫描所选目录；此时不显示尚未解压文件的重复数"}else{"解压完成，重新扫描实际文件"});
    job.db.conn.execute_batch("DELETE FROM files; DELETE FROM directories;")?;
    job.summary.scanned=0;job.summary.scanned_bytes=0;
    job.context.control.scanned.store(0,Ordering::Relaxed);
    let root=job.root.clone();let config=job.config.clone();
    let excluded=rules::build_exclusions(&config.exclusions)?;
    let state=fs::canonicalize(state)?;
    let executable_dir=std::env::current_exe().ok().and_then(|p|p.parent().and_then(|d|fs::canonicalize(d).ok()));
    if let (Some(directory),Ok(target))=(&executable_dir,fs::canonicalize(&root)) {
        if target.starts_with(directory) {
            job.log("扫描","","","提示",&format!("所选目录位于程序自身目录（{}）内；为避免误处理程序文件，扫描会跳过这个目录里的条目",directory.display()),0)?;
        }
    }
    let walk=walkdir::WalkDir::new(&root).follow_links(false).min_depth(1)
        .max_depth(if config.recursive {usize::MAX}else{1}).into_iter().filter_entry(|entry| {
            let Ok(meta)=fs::symlink_metadata(entry.path()) else{return true;};
            if fsutil::is_link(&meta){return false;}
            if entry.path().starts_with(&state){return false;}
            if executable_dir.as_ref().is_some_and(|d|entry.path().starts_with(d)){return false;}
            let Ok(rel)=fsutil::relative_string(&root,entry.path()) else{return false;};
            if rel==".jchtools-work"||rel.starts_with(".jchtools-work/"){return false;}
            if excluded.is_match(&rel)||excluded.is_match(format!("{rel}/")){return false;}
            #[cfg(windows)] {
                use std::os::windows::fs::MetadataExt;
                if !config.include_hidden&&meta.file_attributes()&2!=0{return false;}
                if !config.include_system&&meta.file_attributes()&4!=0{return false;}
            }
            #[cfg(not(windows))] if !config.include_hidden&&entry.file_name().to_string_lossy().starts_with('.'){return false;}
            true
        });
    job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
    let result=(|| {
        let mut count=0;
        for entry in walk {
            job.context.control.checkpoint()?;
            let entry=match entry{Ok(entry)=>entry,Err(error)=>{job.summary.errors+=1;job.log("扫描","","","失败",&error.to_string(),0)?;continue;}};
            let scan_one=(|| {
                let relative=fsutil::relative_string(&root,entry.path())?;
                fsutil::safe_join(&root,&relative)?;
                if entry.file_type().is_dir(){
                    let name=entry.file_name().to_str().context("目录名称不能无损表示")?.to_lowercase();
                    job.db.conn.execute("INSERT INTO directories(rel,name,depth) VALUES(?1,?2,?3)",params![relative,name,entry.depth() as i64])?;
                    return Ok(());
                }
                if !entry.file_type().is_file(){return Ok(());}
                let snapshot=fsutil::snapshot(entry.path())?;
                let name=entry.file_name().to_str().context("文件名不能无损表示")?.to_string();
                job.db.insert_file(&relative,&name.to_lowercase(),&rules::normal_name(&name),&snapshot)?;
                job.summary.scanned+=1;
                job.summary.scanned_bytes=job.summary.scanned_bytes.saturating_add(snapshot.size);
                job.context.control.scanned.fetch_add(1,Ordering::Relaxed);
                if enqueue&&job.config.extract&&rules::archive_name(&name){archive::enqueue(job,entry.path(),0)?;}
                Ok::<_,anyhow::Error>(())
            })();
            if let Err(error)=scan_one{job.summary.errors+=1;job.log("扫描",&entry.path().display().to_string(),"","跳过",&format!("{error:#}"),0)?;}
            count+=1;
            if count%2048==0 {job.db.conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;}
        }
        Ok::<_,anyhow::Error>(())
    })();
    match result{Ok(())=>job.db.conn.execute_batch("COMMIT")?,Err(error)=>{let _=job.db.conn.execute_batch("ROLLBACK");return Err(error);}}
    Ok(())
}
fn hash_candidates(job:&mut Job)->Result<()> {
    if !job.config.dedup_same_name&&!job.config.dedup_copy_names&&!job.config.dedup_other_names&&!job.config.same_name_same_size{return Ok(());}
    let pool=rayon::ThreadPoolBuilder::new().num_threads(job.config.hash_workers).thread_name(|i|format!("organizer-hash-{i}")).build()?;
    for full in [false,true]{
        job.context.status(if full{"计算候选文件的完整 Hash"}else{"按文件大小分组并计算首尾预哈希"});
        job.db.conn.execute_batch("DROP TABLE IF EXISTS hash_candidates; CREATE TEMP TABLE hash_candidates(id INTEGER PRIMARY KEY);")?;
        if full {
            job.db.conn.execute("INSERT OR IGNORE INTO hash_candidates SELECT id FROM files WHERE active=1 AND (size,prehash) IN (SELECT size,prehash FROM files WHERE active=1 AND prehash IS NOT NULL GROUP BY size,prehash HAVING COUNT(*)>1)",[])?;
            if job.config.same_name_same_size {
                job.db.conn.execute("INSERT OR IGNORE INTO hash_candidates SELECT id FROM files WHERE active=1 AND (name,size) IN (SELECT name,size FROM files WHERE active=1 GROUP BY name,size HAVING COUNT(*)>1)",[])?;
            }
        } else {
            job.db.conn.execute("INSERT INTO hash_candidates SELECT id FROM files WHERE active=1 AND size IN (SELECT size FROM files WHERE active=1 GROUP BY size HAVING COUNT(*)>1)",[])?;
        }
        let mut cursor=0;
        loop{
            job.context.control.checkpoint()?;
            let sql=format!("SELECT {FILE_COLUMNS} FROM files WHERE id>?1 AND active=1 AND id IN (SELECT id FROM hash_candidates) ORDER BY id LIMIT ?2");
            let batch=job.db.files(&sql,params![cursor,(job.config.hash_workers*4).max(16) as i64])?;
            if batch.is_empty(){break;}
            cursor=batch.last().unwrap().id;
            let root=&job.root;let control=&job.context.control;let algorithm=job.config.hash_algorithm;
            let results:Vec<_>=pool.install(||batch.par_iter().map(|file|{
                let result=(||{let path=fsutil::safe_join(root,&file.rel)?;
                    if full{hashing::full_hash(&path,&file.snapshot,algorithm,control)}else{hashing::prehash(&path,&file.snapshot,control)}})();
                (file.id,file.rel.clone(),result)
            }).collect());
            job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
            let update=(||{
                for(id,rel,result)in results{
                    match result{
                        Ok(hash)=>{let sql=if full{"UPDATE files SET hash=?1 WHERE id=?2"}else{"UPDATE files SET prehash=?1 WHERE id=?2"};job.db.conn.execute(sql,params![hash,id])?;},
                        Err(error)=>{job.summary.errors+=1;job.db.conn.execute("UPDATE files SET active=0 WHERE id=?1",[id])?;job.log("Hash",&rel,"","跳过",&format!("{error:#}"),0)?;},
                    }
                }Ok::<_,anyhow::Error>(())
            })();
            match update{Ok(())=>job.db.conn.execute_batch("COMMIT")?,Err(error)=>{let _=job.db.conn.execute_batch("ROLLBACK");return Err(error);}}
        }
    }
    Ok(())
}
pub fn apply(directory:&Path,context:TaskContext)->Result<TaskResult>{apply_with(directory,context,Arc::new(NativeRecycler))}
pub fn apply_with(directory:&Path,context:TaskContext,recycler:Arc<dyn Recycler>)->Result<TaskResult>{
    let state=directory.parent().and_then(Path::parent).context("任务目录结构无效")?;
    let _guard=fsutil::RootGuard::acquire(state)?;
    let db=Database::open(directory)?;
    let status:String=db.get("status")?;
    if status!="ready"{bail!("任务不是待确认状态（{status}）；请重新扫描，不会盲目重放旧计划");}
    let root_text:String=db.get("root")?;
    let root=fsutil::normalize_root(Path::new(&root_text))?;
    let config=db.config()?;config.validate()?;
    let summary=db.summary()?;
    let mut job=Job{root,config,context,db,summary,archive_override:None,recycler};
    job.db.set("status",&"executing")?;
    let outcome=(||{
        let mut cursor=0;
        loop{
            let actions=job.db.actions_page(cursor,256)?;
            if actions.is_empty(){break;}
            for action in actions{
                cursor=action.id;job.context.control.checkpoint()?;
                if !action.selected{job.summary.skipped+=1;job.db.mark_action(action.id,"unselected")?;continue;}
                job.context.status(format!("执行 {:?}：{}",action.kind,action.source));
                match execute_action(&mut job,&action){
                    Ok(true)=>job.db.mark_action(action.id,"done")?,
                    Ok(false)=>{job.summary.skipped+=1;job.db.mark_action(action.id,"skipped")?;},
                    Err(error)=>{job.summary.errors+=1;job.db.mark_action(action.id,"failed")?;
                        job.log("执行",&action.source,action.target.as_deref().unwrap_or(""),"失败",&format!("{error:#}"),0)?;job.context.control.check_cancelled()?;},
                }
                job.context.control.completed.fetch_add(1,Ordering::Relaxed);
            }
        }
        Ok::<_,anyhow::Error>(())
    })();
    job.db.set("summary",&job.summary)?;
    job.db.set("status",&if outcome.is_ok(){"finished"}else if job.context.control.is_cancelled(){"cancelled"}else{"failed"})?;
    outcome?;
    Ok(TaskResult{directory:directory.to_path_buf(),summary:job.summary})
}
fn execute_action(job:&mut Job,action:&Action)->Result<bool>{
    let source=fsutil::safe_join(&job.root,&action.source)?;
    if let Some(expected)=&action.expected{fsutil::unchanged(&source,expected)?;}
    let keeper=if let Some((relative,snapshot))=&action.keeper{
        let path=fsutil::safe_join(&job.root,relative)?;fsutil::unchanged(&path,snapshot)?;Some((path,snapshot))
    }else{None};
    if let(Some((path,snapshot)),Some(expected),Some(_hash))=(&keeper,&action.expected,&action.hash){
        if job.config.verify_bytes&&!hashing::equal_bytes(&source,expected,path,snapshot,&job.context.control)?{
            bail!("逐字节复核不一致，不执行内容去重");
        }
    }
    match action.kind{
        ActionKind::Delete=>Ok(job.delete_path(&source,action.expected.as_ref(),action.mode,&action.reason)?!=DeleteResult::Kept),
        ActionKind::Move=>{
            let target=fsutil::safe_join(&job.root,action.target.as_deref().context("移动操作缺少目标")?)?;
            fsutil::ensure_parent(&job.root,&target)?;
            job.log("移动",&action.source,action.target.as_deref().unwrap_or(""),"准备",&action.reason,action.expected.as_ref().map(|s|s.size).unwrap_or(0))?;
            fsutil::rename_noreplace(&source,&target)?;
            job.summary.moved+=1;job.log("移动",&action.source,action.target.as_deref().unwrap_or(""),"成功",&action.reason,0)?;Ok(true)
        }
        ActionKind::Hardlink=>{
            let(keeper,_)=keeper.context("硬链接缺少保留文件")?;
            let temporary=source.parent().context("路径缺少父目录")?.join(format!(".jchtools-link-{}",uuid::Uuid::new_v4()));
            // Create the replacement link before removing any source data. Cross-volume links fail safely here.
            fs::hard_link(&keeper,&temporary).context("此位置不支持硬链接，原文件未删除")?;
            let removed=job.delete_path(&source,action.expected.as_ref(),action.mode,&action.reason);
            match removed{
                Ok(DeleteResult::Kept)=>{let _=fs::remove_file(&temporary);return Ok(false);},
                Err(error)=>{let _=fs::remove_file(&temporary);return Err(error);},
                _=>(),
            }
            if let Err(error)=fsutil::rename_noreplace(&temporary,&source){
                // Keep the replacement link under a visible name if a competing file appeared.
                let emergency=fsutil::unique_target(&job.root,&source)?;
                fsutil::rename_noreplace(&temporary,&emergency)?;
                job.log("硬链接",&action.source,&emergency.display().to_string(),"警告",&format!("目标被占用，保留链接副本：{error}"),0)?;
            }
            job.summary.linked+=1;Ok(true)
        }
        ActionKind::EmptyDirectory=>{
            if !source.try_exists()?||!source.is_dir()||fs::read_dir(&source)?.next().is_some(){return Ok(false);}
            Ok(job.delete_path(&source,None,action.mode,&action.reason)?!=DeleteResult::Kept)
        }
    }
}
pub fn history(state:&Path)->Result<Vec<(PathBuf,String)>>{
    let directory=state.join("tasks");if !directory.try_exists()?{return Ok(Vec::new());}
    let mut paths=fs::read_dir(directory)?.filter_map(|r|r.ok()).filter(|e|e.path().join("task.sqlite3").is_file())
        .map(|e|e.path()).collect::<Vec<_>>();
    paths.sort();paths.reverse();paths.truncate(100);
    let mut result=Vec::new();
    for path in paths{
        if let Ok(db)=Database::open(&path){
            let root: String=db.get("root").unwrap_or_default();let status:String=db.get("status").unwrap_or_else(|_|"unknown".into());
            let created:String=db.get("created").unwrap_or_default();
            result.push((path,format!("{created} | {status} | {root}")));
        }
    }Ok(result)
}
