use crate::{config::{ConflictPolicy, DeleteMode}, control::ConflictInfo, engine::Job, fsutil,
    hashing, model::bytes, platform::DeleteResult, process, rules};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use sha2::Digest;
use std::{collections::BTreeMap, fs, io::Read, path::{Path,PathBuf}, process::Command};

pub struct SevenZip { executable: PathBuf }
impl SevenZip {
    pub fn from_bundle() -> Result<Self> {
        let directory = std::env::current_exe()?.parent().context("程序路径缺少目录")?.join("resources/7zip");
        let executable = directory.join(if cfg!(windows) { "7z.exe" } else { "7zz" });
        if !executable.is_file() { bail!("未找到随包携带的 7-Zip。请使用 scripts/package-windows.ps1 构建发布包；不需要终端用户安装 7-Zip。"); }
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
        let files = manifest["files"].as_array().context("7-Zip 校验清单无效")?;
        let mut verified = Vec::new();
        for entry in files {
            let name = entry["name"].as_str().context("引擎文件名无效")?;
            fsutil::validate_component(name)?;
            let mut file = fsutil::open_stable_read(&directory.join(name))?;
            let mut hash = sha2::Sha256::new(); let mut buffer = [0u8;65536];
            loop { let count = file.read(&mut buffer)?; if count == 0 { break; } hash.update(&buffer[..count]); }
            let actual = hex::encode(hash.finalize());
            if entry["sha256"].as_str() != Some(actual.as_str()) { bail!("随包引擎校验失败：{name}"); }
            verified.push(name.to_owned());
        }
        if cfg!(windows) && (!verified.iter().any(|x| x == "7z.exe") || !verified.iter().any(|x| x == "7z.dll")) {
            bail!("7-Zip 清单必须包括完整的 7z.exe 和 7z.dll；不能使用不支持 RAR 的 7za 替代");
        }
        Ok(Self { executable })
    }
    /// Explicit test/development injection, never inferred from the system PATH.
    pub fn with_executable(executable: &Path) -> Result<Self> {
        Ok(Self { executable: fs::canonicalize(executable).context("指定的测试解压引擎不存在")? })
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        if let Some(directory) = self.executable.parent() { command.current_dir(directory); }
        command
    }
    fn list(&self, archive: &Path, job: &mut Job) -> Result<u64> {
        job.db.conn.execute("DELETE FROM archive_members",[])?;
        let mut command = self.command();
        command.args(["l","-slt","-ba","-sccUTF-8","-p-","--"]).arg(archive);
        let mut fields = BTreeMap::<String,String>::new();
        let mut total = 0u64; let mut count = 0u64;
        let cfg = job.config.clone(); let db = &job.db;
        let mut flush = |fields: &mut BTreeMap<String,String>| -> Result<()> {
            let Some(raw) = fields.remove("Path") else { fields.clear(); return Ok(()); };
            if raw == "." || raw == "./" { fields.clear(); return Ok(()); }
            let relative = fsutil::safe_relative(&raw)?;
            let raw = fsutil::path_string(&relative)?.replace('\\',"/");
            if raw.split('/').any(|s|s.eq_ignore_ascii_case(".jchtools-work") || s.eq_ignore_ascii_case("$RECYCLE.BIN") || s.eq_ignore_ascii_case("System Volume Information")) {
                bail!("拒绝压缩包中的程序工作区/系统目录条目：{raw}");
            }
            if fields.get("Encrypted").is_some_and(|s| s == "+") { bail!("加密压缩包需要人工处理；没有把密码写入进程命令行"); }
            for field in ["Symbolic Link", "Hard Link", "Reparse", "Alternate Stream"] {
                if fields.get(field).is_some_and(|s| !s.is_empty() && s != "-") { bail!("拒绝带链接、reparse 或备用数据流的压缩包：{raw}"); }
            }
            let attr = fields.get("Attributes").cloned().unwrap_or_default();
            if attr.split_whitespace().any(|s| s.starts_with('l')) { bail!("拒绝 Unix 符号链接条目：{raw}"); }
            let directory = fields.get("Folder").is_some_and(|s| s == "+") || attr.starts_with('D') || attr.starts_with('d');
            let size = match fields.get("Size") { Some(s) if !s.is_empty() => s.parse::<u64>().context("压缩包条目大小无效")?,
                _ if directory => 0, _ => bail!("压缩包缺少可靠的文件大小元数据，已跳过：{raw}") };
            count = count.checked_add(1).context("条目计数溢出")?;
            total = total.checked_add(size).context("解压总大小溢出")?;
            if count > cfg.max_entries { bail!("压缩包条目数量超过用户设置的上限"); }
            if cfg.max_file_gib > 0 && size > cfg.max_file_gib * (1<<30) { bail!("文件展开大小超过用户上限：{raw}"); }
            if cfg.max_unpacked_gib > 0 && total > cfg.max_unpacked_gib * (1<<30) { bail!("压缩包展开总量超过用户上限"); }
            db.conn.execute("INSERT INTO archive_members(path,size,directory) VALUES(?1,?2,?3)", params![raw,i64::try_from(size)?,directory])?;
            fields.clear(); Ok(())
        };
        let ctl = job.context.control.clone();
        db.conn.execute_batch("BEGIN IMMEDIATE")?;
        let listed = process::run(&mut command,&ctl,|err,line| {
            if err { return Ok(()); }
            if line.trim().is_empty() { return flush(&mut fields); }
            if let Some((key,value)) = line.split_once(" = ") {
                if fields.len() >= 100 { bail!("压缩包元数据异常"); }
                if fields.insert(key.to_string(),value.to_string()).is_some() { bail!("压缩包元数据有重复字段，无法安全解析"); }
            }
            Ok(())
        },|| Ok(())).and_then(|_|flush(&mut fields));
        drop(flush);
        match listed {
            Ok(())=>db.conn.execute_batch("COMMIT")?,
            Err(error)=>{let _=db.conn.execute_batch("ROLLBACK");return Err(error);}
        }
        let packed = fs::metadata(archive)?.len().max(1);
        if cfg.max_ratio > 0 && total / packed > cfg.max_ratio { bail!("压缩包展开比例超过用户设置的上限"); }
        Ok(total)
    }
    fn extract_one(&self, job: &mut Job, archive_rel: &str, depth: u32) -> Result<()> {
        let archive = fsutil::safe_join(&job.root,archive_rel)?;
        let source_snapshot = fsutil::snapshot(&archive)?;
        let multipart = protect_volumes(job,&archive)?;
        // Keep an open, write-denying source handle on Windows during listing/extraction.
        let source_guard = fsutil::open_stable_read(&archive)?;
        job.context.status(format!("检查压缩包：{archive_rel}"));
        let total = self.list(&archive,job)?;
        let reserve = job.config.reserve_gib * (1<<30);
        let free = fs2::available_space(&job.root)?;
        if total.checked_add(reserve).context("容量计算溢出")? > free {
            bail!("可用空间不足：本包需 {}，预留 {}，当前 {}。未写入任何解压文件",bytes(total),bytes(reserve),bytes(free));
        }
        let stage = Staging::new(&job.root)?;
        let mut command = self.command();
        command.args(["x","-aou","-y","-bb0","-bsp1","-bso1","-bse2","-sccUTF-8","-p-","-mmt=2"])
            .arg(format!("-o{}",fsutil::path_string(&stage.content)?)).arg("--").arg(&archive);
        let ctl = job.context.control.clone(); let context = job.context.clone(); let root = job.root.clone();
        process::run(&mut command,&ctl,|err,line| {
            if !err && line.contains('%') { context.status(format!("正在解压 {archive_rel} · {}",line.trim())); }
            Ok(())
        },|| {
            if fs2::available_space(&root)? < reserve { bail!("磁盘剩余空间低于预留阈值，停止解压并保留原包"); }
            Ok(())
        })?;
        fsutil::unchanged(&archive,&source_snapshot)?;
        drop(source_guard);
        let mut complete = true;
        let exclusions = rules::build_exclusions(&job.config.exclusions)?;
        let mut expanded = 0u64;
        // One archive is decoded once, including solid archives. Final placement is rename, never copy.
        for entry in walkdir::WalkDir::new(&stage.content).follow_links(false).min_depth(1) {
            job.context.control.checkpoint()?;
            let entry = entry?;
            let meta = fs::symlink_metadata(entry.path())?;
            if fsutil::is_link(&meta) { bail!("解压结果出现链接，已停止合入"); }
            if meta.is_dir() { continue; }
            if !meta.is_file() { bail!("解压结果含非普通文件"); }
            expanded = expanded.checked_add(meta.len()).context("解压字节计数溢出")?;
            if expanded > total { bail!("实际解压量超过压缩包声明，已停止合入"); }
            let relative = fsutil::relative_string(&stage.content,entry.path())?;
            let base = archive.parent().context("压缩包缺少父目录")?;
            let mut destination = base.join(fsutil::safe_relative(&relative)?);
            // An archive may contain its own basename. Never overwrite the still-needed source archive.
            if fsutil::path_string(&destination)?.to_lowercase() == fsutil::path_string(&archive)?.to_lowercase() {
                destination = fsutil::unique_target(&job.root,&destination)?;
            }
            let destination_rel = fsutil::relative_string(&job.root,&destination)?;
            fsutil::safe_join(&job.root,&destination_rel)?;
            if exclusions.is_match(&destination_rel) || excluded_destination(&job.root,&destination,&job.config) {
                complete=false;job.summary.skipped+=1;
                job.log("解压",archive_rel,&destination_rel,"跳过","目标命中排除/隐藏/系统文件设置；原包保留",meta.len())?;
                continue;
            }
            match merge_extracted(job,entry.path(),&destination)? {
                Some(final_path) => {
                    job.summary.extracted += 1;
                    job.log("解压",archive_rel,&fsutil::relative_string(&job.root,&final_path)?,"成功","解压并同卷移动",meta.len())?;
                    if job.config.nested_archives && rules::archive_name(&final_path.to_string_lossy()) {
                        enqueue(job,&final_path,depth+1)?;
                    }
                }
                None => { complete = false; job.summary.skipped += 1;
                    job.log("解压",archive_rel,&destination_rel,"跳过","目标冲突未采用新文件；原包强制保留",meta.len())?; }
            }
        }
        if expanded != total { bail!("解压总量与条目清单不一致，原包保留"); }
        // Preserve empty archive directories too. Do not merge them before checking for file/dir collisions.
        for entry in walkdir::WalkDir::new(&stage.content).follow_links(false).min_depth(1) {
            let entry = entry?;
            if entry.file_type().is_dir() {
                let rel = fsutil::relative_string(&stage.content,entry.path())?;
                let dest = archive.parent().unwrap().join(fsutil::safe_relative(&rel)?);
                let root_rel = fsutil::relative_string(&job.root,&dest)?;
                fsutil::safe_join(&job.root,&root_rel)?;
                if !dest.try_exists()? { fs::create_dir_all(&dest)?; }
                else if !dest.is_dir() { complete = false; }
            }
        }
        if complete && !multipart {
            job.delete_path(&archive,Some(&source_snapshot),job.config.archive_delete.resolve(job.config.global_delete),"解压后原压缩包")?;
        } else if job.config.archive_delete.resolve(job.config.global_delete) != DeleteMode::Keep {
            job.log("解压",archive_rel,"","保留","有跳过条目或属于分卷包；不能安全确认所有源卷均可删除",source_snapshot.size)?;
        }
        if archive.try_exists()? {
            job.db.conn.execute("INSERT OR IGNORE INTO protected_originals(rel) VALUES(?1)",[archive_rel])?;
        }
        job.summary.archives_ok += 1;
        Ok(())
    }
}
/// 只判断根目录以下的层级。用户选定的根目录本身（例如位于隐藏的 AppData 之下）不参与隐藏/系统判定，
/// 否则整棵树都会被上级目录的属性判定为隐藏，所有解压结果都会被跳过。
fn excluded_destination(root: &Path, path: &Path, config: &crate::config::Config) -> bool {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate == root { break; }
        if !config.include_hidden && is_hidden(candidate) { return true; }
        if !config.include_system && is_system(candidate) { return true; }
        current = candidate.parent();
    }
    false
}
#[cfg(windows)]
fn is_hidden(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_attributes() & 2 != 0)
}
#[cfg(not(windows))]
fn is_hidden(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok() && path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.'))
}
#[cfg(windows)]
fn is_system(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_attributes() & 4 != 0)
}
#[cfg(not(windows))]
fn is_system(_path: &Path) -> bool { false }
fn protect_volumes(job: &Job, archive: &Path) -> Result<bool> {
    let name=archive.file_name().and_then(|s|s.to_str()).context("无效压缩包名称")?.to_lowercase();
    let stem = if let Some((stem,_))=name.rsplit_once(".part") { Some((stem.to_string(),"rar")) }
        else if name.ends_with(".7z.001") || name.ends_with(".zip.001") { Some((name[..name.len()-4].to_string(),"numbered")) }
        else if let Some(stem)=name.strip_suffix(".rar") {Some((stem.to_string(),"oldrar"))}
        else if let Some(stem)=name.strip_suffix(".zip") {Some((stem.to_string(),"splitzip"))}
        else {None};
    let Some((stem,kind))=stem else{return Ok(false);};
    let mut found=false;
    for entry in fs::read_dir(archive.parent().context("压缩包缺少目录")?)? {
        let entry=entry?;let candidate=entry.file_name().to_string_lossy().to_lowercase();
        let matches=match kind {
            "rar"=>candidate.starts_with(&format!("{stem}.part")) && candidate.ends_with(".rar"),
            "numbered"=>candidate.strip_prefix(&format!("{stem}.")).is_some_and(|v|v.len()==3 && v.chars().all(|c|c.is_ascii_digit())),
            "oldrar"=>candidate.strip_prefix(&format!("{stem}.r")).is_some_and(|v|v.len()==2 && v.chars().all(|c|c.is_ascii_digit())),
            "splitzip"=>candidate.strip_prefix(&format!("{stem}.z")).is_some_and(|v|v.len()==2 && v.chars().all(|c|c.is_ascii_digit())),
            _=>false,
        };
        if matches {
            found=true;
            let rel=fsutil::relative_string(&job.root,&entry.path())?;
            job.db.conn.execute("INSERT OR IGNORE INTO protected_originals(rel) VALUES(?1)",[rel])?;
        }
    }
    if found || rules::multipart_name(&name) {
        job.db.conn.execute("INSERT OR IGNORE INTO protected_originals(rel) VALUES(?1)",[fsutil::relative_string(&job.root,archive)?])?;
        return Ok(true);
    }
    Ok(false)
}
fn merge_extracted(job: &mut Job, source: &Path, target: &Path) -> Result<Option<PathBuf>> {
    let incoming = fsutil::snapshot(source)?;
    fsutil::ensure_parent(&job.root,target)?;
    if !target.try_exists()? { fsutil::rename_noreplace(source,target)?; return Ok(Some(target.to_path_buf())); }
    let meta = fs::symlink_metadata(target)?;
    if !meta.is_file() || fsutil::is_link(&meta) {
        let renamed = fsutil::unique_target(&job.root,target)?;
        fsutil::rename_noreplace(source,&renamed)?; return Ok(Some(renamed));
    }
    let existing = fsutil::snapshot(target)?;
    let policy = job.archive_override.unwrap_or(job.config.extract_conflict);
    let policy = if policy == ConflictPolicy::Ask {
        let answer = (job.context.decisions)(ConflictInfo {
            incoming: source.display().to_string(), existing: target.display().to_string(),
            incoming_size: incoming.size, existing_size: existing.size,
            incoming_time: incoming.modified_ns, existing_time: existing.modified_ns,
        })?;
        if answer.apply_all { job.archive_override = Some(answer.policy); }
        answer.policy
    } else { policy };
    job.context.control.checkpoint()?;
    let use_new = match policy {
        ConflictPolicy::Overwrite => true,
        ConflictPolicy::Newest => incoming.modified_ns > existing.modified_ns,
        ConflictPolicy::Largest => incoming.size > existing.size,
        ConflictPolicy::Skip => false,
        ConflictPolicy::KeepBoth | ConflictPolicy::Ask => {
            let renamed = fsutil::unique_target(&job.root,target)?;
            fsutil::rename_noreplace(source,&renamed)?; return Ok(Some(renamed));
        }
    };
    if !use_new { return Ok(None); }
    // An exact duplicate need not replace the existing file; retain the user's existing path.
    if incoming.size == existing.size && hashing::equal_bytes(source,&incoming,target,&existing,&job.context.control)? {
        return Ok(Some(target.to_path_buf()));
    }
    let mode = job.config.conflict_delete.resolve(job.config.global_delete);
    if mode == DeleteMode::Keep { return Ok(None); }
    let removed = job.delete_path(target,Some(&existing),mode,"解压覆盖旧文件")?;
    if removed == DeleteResult::Kept { return Ok(None); }
    if let Err(error) = fsutil::rename_noreplace(source,target) {
        // The extracted source remains in an owned staging directory until this function returns.
        // Preserve it under a separate visible name rather than losing it when cleanup runs.
        let emergency = fsutil::unique_target(&job.root,target)?;
        fsutil::rename_noreplace(source,&emergency).context("目标删除后合入失败；原压缩包仍保留")?;
        job.log("解压",&source.display().to_string(),&emergency.display().to_string(),"警告",&format!("目标发生竞争，改用不冲突名称：{error}"),incoming.size)?;
        return Ok(Some(emergency));
    }
    Ok(Some(target.to_path_buf()))
}
pub fn enqueue(job: &Job, archive: &Path, depth: u32) -> Result<()> {
    let snapshot = fsutil::snapshot(archive)?;
    let relative = fsutil::relative_string(&job.root,archive)?;
    let fingerprint = format!("{}:{}:{}:{}",relative,snapshot.size,snapshot.modified_ns,snapshot.identity);
    job.db.conn.execute("INSERT OR IGNORE INTO archives(rel,fingerprint,depth) VALUES(?1,?2,?3)",params![relative,fingerprint,depth])?;
    Ok(())
}
pub fn extract_queued(job: &mut Job, engine: &SevenZip) -> Result<()> {
    loop {
        job.context.control.checkpoint()?;
        let next: Option<(i64,String,u32)> = job.db.conn.query_row(
            "SELECT id,rel,depth FROM archives WHERE state='pending' ORDER BY depth,id LIMIT 1",[],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((id,relative,depth)) = next else { break; };
        job.db.conn.execute("UPDATE archives SET state='running' WHERE id=?1",[id])?;
        let result = if depth >= job.config.max_depth { Err(anyhow::anyhow!("达到最大嵌套层数")) }
            else { engine.extract_one(job,&relative,depth) };
        match result {
            Ok(()) => { job.db.conn.execute("UPDATE archives SET state='done' WHERE id=?1",[id])?; }
            Err(error) => {
                job.db.conn.execute("UPDATE archives SET state='failed' WHERE id=?1",[id])?;
                job.summary.archives_failed += 1; job.summary.errors += 1;
                job.log("解压",&relative,"","失败",&format!("{error:#}"),0)?;
                job.context.control.check_cancelled()?;
            }
        }
    }
    Ok(())
}
struct Staging { directory: PathBuf, content: PathBuf, owner: String }
impl Staging {
    fn new(root: &Path) -> Result<Self> {
        let owner = uuid::Uuid::new_v4().to_string();
        let relative = format!(".jchtools-work/{owner}");
        let directory = fsutil::safe_join(root,&relative)?;
        fs::create_dir_all(&directory)?;
        let marker = directory.join("OWNER");
        fs::write(&marker,&owner)?;
        let content = directory.join("content"); fs::create_dir(&content)?;
        Ok(Self { directory,content,owner })
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        if fs::read_to_string(self.directory.join("OWNER")).ok().as_deref() == Some(self.owner.as_str()) {
            // Only this freshly generated staging tree, never a user's original directory.
            let _ = fs::remove_dir_all(&self.directory);
            if let Some(parent) = self.directory.parent() { let _ = fs::remove_dir(parent); }
        }
    }
}
