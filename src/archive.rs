use crate::{config::{ConflictPolicy, DeleteMode}, control::ConflictInfo, engine::Job, fsutil,
    hashing, model::bytes, platform::DeleteResult, process, rules};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use std::{collections::BTreeMap, fs, path::{Path,PathBuf}, process::Command, time::{Duration, SystemTime}};

/// 无 Size 元数据的流式包（gzip/bzip2/xz 等）展开量的内置硬顶（GiB）。
/// 这类包无法按声明总量做预检（sizes_complete=false 会跳过 max_ratio 与
/// 「声明总量 vs 可用空间」检查），必须有兜底上限；用户设置了更小的
/// max_unpacked_gib 时取两者较小值——只能收紧，不能放宽。
const STREAM_UNPACKED_CAP_GIB: u64 = 50;

pub struct SevenZip { executable: PathBuf }
impl SevenZip {
    pub fn from_bundle() -> Result<Self> {
        // 随包目录与内嵌引擎统一走 resolve_executable：随包路径与 AGENTS §3.1 一致
        // 仅警告放行（LGPL 允许替换外部 7-Zip 引擎），内嵌释放路径保留硬校验。
        // 不再对随包路径做硬哈希 bail——那会拒绝用户自备/替换的引擎，与 LGPL 冲突。
        Ok(Self { executable: crate::engine_bundle::resolve_executable()? })
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
    /// 列出压缩包条目。返回 (声明总大小, 大小元数据是否完整)。
    /// 7-Zip 对 bzip2/xz 等流式格式可能不输出成员 Path，甚至不输出 Size；此时不能把条目静默丢掉，
    /// 否则 total=0 会在合入阶段误报「实际解压量超过压缩包声明」。
    fn list(&self, archive: &Path, job: &mut Job) -> Result<(u64, bool)> {
        job.db.conn.execute("DELETE FROM archive_members",[])?;
        let mut command = self.command();
        command.args(["l","-slt","-ba","-sccUTF-8","-p-","--"]).arg(archive);
        let mut fields = BTreeMap::<String,String>::new();
        let mut total = 0u64; let mut count = 0u64; let mut sizes_complete = true;
        let cfg = job.config.clone(); let db = &job.db;
        let mut flush = |fields: &mut BTreeMap<String,String>| -> Result<()> {
            let raw = match fields.remove("Path") {
                Some(raw) => raw,
                None => {
                    // 流式格式（bzip2/xz）可能没有 Path；若块内仍有成员元数据则用包名合成。
                    if fields.is_empty() { return Ok(()); }
                    let has_meta = fields.contains_key("Size") || fields.contains_key("Packed Size")
                        || fields.contains_key("Folder") || fields.contains_key("Encrypted")
                        || fields.contains_key("Attributes");
                    if !has_meta { fields.clear(); return Ok(()); }
                    stream_member_name(archive)
                }
            };
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
            let size = match fields.get("Size") {
                // 目录条目一律按 0 计入总量：部分引擎会给目录填非 0 Size，导致合入后 expanded!=total 必败。
                _ if directory => 0,
                Some(s) if !s.is_empty() => s.parse::<u64>().context("压缩包条目大小无效")?,
                _ => {
                    // 流式单文件包可能不声明展开大小：记为不完整，合入阶段跳过精确大小校验。
                    sizes_complete = false;
                    0
                }
            };
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
        // 用乘法比较避免整数除法截断导致边界上更宽松。
        if cfg.max_ratio > 0 && sizes_complete {
            let limit = packed.checked_mul(cfg.max_ratio).unwrap_or(u64::MAX);
            if total > limit {
                bail!("压缩包展开比例超过用户设置的上限");
            }
        }
        Ok((total, sizes_complete))
    }
    fn extract_one(&self, job: &mut Job, archive_rel: &str, depth: u32) -> Result<()> {
        let archive = fsutil::safe_join(&job.root,archive_rel)?;
        let source_snapshot = fsutil::snapshot(&archive)?;
        let multipart = protect_volumes(job,&archive)?;
        // Keep an open, write-denying source handle on Windows during listing/extraction.
        let source_guard = fsutil::open_stable_read(&archive)?;
        job.context.status(format!("检查压缩包：{archive_rel}"));
        let (total, sizes_complete) = self.list(&archive,job)?;
        let reserve = job.config.reserve_gib * (1<<30);
        let free = fs2::available_space(&job.root)?;
        // 大小元数据不完整时只校验预留空间，避免对流式格式误报容量不足。
        if sizes_complete && total.checked_add(reserve).context("容量计算溢出")? > free {
            bail!("可用空间不足：本包需 {}，预留 {}，当前 {}。未写入任何解压文件",bytes(total),bytes(reserve),bytes(free));
        }
        // 流式包没有 Size 元数据时无法按声明总量预检：为它启用内置硬顶
        // （用户 max_unpacked_gib 可进一步收紧），避免解压体量几乎无上限。
        let stream_cap_bytes: Option<u64> = if sizes_complete { None } else {
            let builtin = STREAM_UNPACKED_CAP_GIB.checked_mul(1 << 30).context("流式上限计算溢出")?;
            Some(if job.config.max_unpacked_gib > 0 {
                builtin.min(job.config.max_unpacked_gib * (1 << 30))
            } else { builtin })
        };
        let stage = Staging::new(&job.root)?;
        let mut command = self.command();
        command.args(["x","-aou","-y","-bb0","-bsp1","-bso1","-bse2","-sccUTF-8","-p-","-mmt=2"])
            .arg(format!("-o{}",fsutil::path_string(&stage.content)?)).arg("--").arg(&archive);
        let ctl = job.context.control.clone(); let context = job.context.clone(); let root = job.root.clone();
        let stage_probe = stage.content.clone();
        process::run(&mut command,&ctl,|err,line| {
            if !err && line.contains('%') { context.status(format!("正在解压 {archive_rel} · {}",line.trim())); }
            Ok(())
        },|| {
            if fs2::available_space(&root)? < reserve { bail!("磁盘剩余空间低于预留阈值，停止解压并保留原包"); }
            // 无 Size 元数据的包在解压过程中累计暂存量，超过硬顶立即停止（与预留空间联动）。
            if let Some(cap) = stream_cap_bytes {
                let mut staged = 0u64;
                for entry in walkdir::WalkDir::new(&stage_probe).follow_links(false).min_depth(1) {
                    let entry = entry?;
                    if entry.file_type().is_file() {
                        staged = staged.saturating_add(entry.metadata()?.len());
                        if staged > cap { bail!("流式压缩包解压量超过上限 {}，已停止并保留原包",bytes(cap)); }
                    }
                }
            }
            Ok(())
        }).with_context(|| "解压失败（可能已损坏、加密或格式不受支持）")?;
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
            if sizes_complete && expanded > total { bail!("实际解压量超过压缩包声明，已停止合入"); }
            // 合入阶段兜底：流式包解压期间的抽样检查可能漏掉峰值，这里按最终字节量强制卡住硬顶。
            if let Some(cap) = stream_cap_bytes {
                if expanded > cap { bail!("流式压缩包实际解压量超过上限 {}，已停止合入并保留原包",bytes(cap)); }
            }
            // sizes_complete=false 时 list 阶段拿不到成员大小，单文件上限改在合入阶段检查。
            if !sizes_complete && job.config.max_file_gib > 0 && meta.len() > job.config.max_file_gib * (1<<30) {
                bail!("文件展开大小超过用户上限：{}",fsutil::relative_string(&job.root,entry.path())?);
            }
            let relative = fsutil::relative_string(&stage.content,entry.path())?;
            let base = archive.parent().context("压缩包缺少父目录")?;
            let mut destination = base.join(fsutil::safe_relative(&relative)?);
            // 压缩包里含有与压缩包同名的成员（gzip 头会记录原始文件名，base.tgz 里就可能是 base.tgz）：
            // 绝不能覆盖仍在使用的源包。流式包的解压结果其实就是去掉一层压缩后的内容，
            // 用真实名字（base.tar）落盘并按正常冲突策略处理；其他格式改名放置。
            // 路径相等判断仅在 Windows 上忽略大小写（NTFS 不区分）；其他平台区分大小写。
            // Windows 必须用 Unicode 大小写折叠（to_lowercase），不能退回 ASCII 比较：
            // NTFS 大小写折叠是 Unicode 表驱动的，Ä/ä 这类非 ASCII 对在文件系统层视为同一路径。
            let collides_with_source = if cfg!(windows) {
                fsutil::path_string(&destination)?.to_lowercase()
                    == fsutil::path_string(&archive)?.to_lowercase()
            } else {
                destination == archive
            };
            if collides_with_source {
                match stream_stem(&archive).map(|stem| base.join(stem)) {
                    Some(candidate) => destination = candidate,
                    None => destination = fsutil::unique_target(&job.root,&destination)?,
                }
            }
            let destination_rel = fsutil::relative_string(&job.root,&destination)?;
            fsutil::safe_join(&job.root,&destination_rel)?;
            if exclusions.is_match(&destination_rel) || excluded_destination(&job.root,&destination,&job.config) {
                complete=false;job.summary.skipped+=1;
                job.log("解压",archive_rel,&destination_rel,"跳过","目标命中排除/隐藏/系统文件设置；原包保留",meta.len())?;
                continue;
            }
            // 分卷/保留源包会在下次分析时再次解压。若归类已把同名同内容文件搬走，
            // 在源目录旁再写一份只会制造“删除+移动”循环；树内已有相同字节则不再落盘。
            if !destination.try_exists()? {
                let incoming = fsutil::snapshot(entry.path())?;
                if let Some(equivalent) = find_identical_elsewhere(job,entry.path(),&incoming,&relative)? {
                    job.summary.extracted += 1;
                    let shown = fsutil::relative_string(&job.root,&equivalent)?;
                    job.log("解压",archive_rel,&shown,"成功","树内已有相同内容，未在源目录重复写入；原包按规则处理",meta.len())?;
                    continue;
                }
            }
            match merge_extracted(job,entry.path(),&destination)? {
                Some(final_path) => {
                    job.summary.extracted += 1;
                    job.log("解压",archive_rel,&fsutil::relative_string(&job.root,&final_path)?,"成功","解压并同卷移动",meta.len())?;
                    if job.config.nested_archives && rules::archive_name(&final_path.to_string_lossy()) {
                        enqueue(job,&final_path,depth+1)?;
                    }
                }
                None => {
                    job.summary.skipped += 1;
                    // Newest/Largest/Skip 下「已有文件胜出」是策略结果：目标内容已就位，
                    // 不应把 complete 置 false 导致原包永久保留并在下次分析重复解压。
                    // Skip 仍保留原包（用户选择不采用新文件）；Newest/Largest 可按规则删原包。
                    let policy = job.archive_override.unwrap_or(job.config.extract_conflict);
                    if policy == ConflictPolicy::Skip {
                        complete = false;
                        job.log("解压",archive_rel,&destination_rel,"跳过","目标冲突未采用新文件；原包强制保留",meta.len())?;
                    } else {
                        job.log("解压",archive_rel,&destination_rel,"跳过","目标冲突：已有文件按策略保留；原包按规则处理",meta.len())?;
                    }
                }
            }
        }
        if sizes_complete && expanded != total { bail!("解压总量与条目清单不一致，原包保留"); }
        // Preserve empty archive directories too. Do not merge them before checking for file/dir collisions.
        for entry in walkdir::WalkDir::new(&stage.content).follow_links(false).min_depth(1) {
            let entry = entry?;
            if entry.file_type().is_dir() {
                let rel = fsutil::relative_string(&stage.content,entry.path())?;
                let dest = archive.parent().unwrap().join(fsutil::safe_relative(&rel)?);
                let root_rel = fsutil::relative_string(&job.root,&dest)?;
                fsutil::safe_join(&job.root,&root_rel)?;
                // 与文件合入/扫描同一过滤口径：X/** 不匹配 bare X，需补 X/ 变体。
                if exclusions.is_match(&root_rel) || exclusions.is_match(format!("{root_rel}/"))
                    || excluded_destination(&job.root,&dest,&job.config) {
                    complete=false;job.summary.skipped+=1;
                    job.log("解压",archive_rel,&root_rel,"跳过","目标命中排除/隐藏/系统文件设置；原包保留",0)?;
                    continue;
                }
                if !dest.try_exists()? { fs::create_dir_all(&dest)?; }
                else if !dest.is_dir() {
                    complete=false;job.summary.skipped+=1;
                    job.log("解压",archive_rel,&root_rel,"跳过","空目录名与目标处已有文件冲突；原包保留",0)?;
                }
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
/// 流式压缩包去掉**一层**压缩后缀后的名字；不是流式格式时返回 None。
fn stream_stem(archive: &Path) -> Option<String> {
    let name = archive.file_name()?.to_str()?.to_string();
    let lower = name.to_ascii_lowercase();
    for suffix in [".tgz", ".tbz2", ".tbz", ".txz", ".bz2", ".gz", ".xz", ".lzma", ".zst"] {
        if lower.ends_with(suffix) {
            let stem = &name[..name.len() - suffix.len()];
            if stem.is_empty() { return None; }
            // tgz/tbz/txz 本质是 tar 容器，名字里补回 .tar
            if matches!(suffix, ".tgz" | ".tbz" | ".tbz2" | ".txz")
                && !stem.to_ascii_lowercase().ends_with(".tar")
            {
                return Some(format!("{stem}.tar"));
            }
            return Some(stem.to_string());
        }
    }
    None
}
/// 流式压缩包（bzip2/xz/gzip）成员名：7-Zip 有时不输出 Path，用包名去掉**一层**压缩后缀合成。
fn stream_member_name(archive: &Path) -> String {
    if let Some(stem) = stream_stem(archive) { return stem; }
    let name = archive.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "content".into());
    match archive.file_stem() {
        Some(stem) => stem.to_string_lossy().into_owned(),
        None => name,
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
/// 将小写文件名解析为 RAR 新式分卷主干（去掉末尾 `.partN.rar` 后的部分）。
/// 与 rules::multipart_name 使用的 `\.part(\d+)\.rar$` 对齐：
/// report.partial.rar 这类仅含 “.part” 子串的普通包不得当作分卷。
fn rar_part_stem(name: &str) -> Option<&str> {
    let base = name.strip_suffix(".rar")?;
    let (stem, part) = base.rsplit_once(".part")?;
    (!part.is_empty() && part.chars().all(|c| c.is_ascii_digit())).then_some(stem)
}
fn protect_volumes(job: &Job, archive: &Path) -> Result<bool> {
    let name=archive.file_name().and_then(|s|s.to_str()).context("无效压缩包名称")?.to_lowercase();
    // 分卷识别只走 rar_part_stem：旧实现 rsplit_once(".part") 会把 report.partial.rar
    // 误判为分卷并保护整目录的 report.part* 文件。
    let stem = if let Some(stem)=rar_part_stem(&name) { Some((stem.to_string(),"rar")) }
        else if name.ends_with(".7z.001") || name.ends_with(".zip.001") { Some((name[..name.len()-4].to_string(),"numbered")) }
        else if let Some(stem)=name.strip_suffix(".rar") {Some((stem.to_string(),"oldrar"))}
        else if let Some(stem)=name.strip_suffix(".zip") {Some((stem.to_string(),"splitzip"))}
        else {None};
    let Some((stem,kind))=stem else{return Ok(false);};
    let mut found=false;
    for entry in fs::read_dir(archive.parent().context("压缩包缺少目录")?)? {
        let entry=entry?;let candidate=entry.file_name().to_string_lossy().to_lowercase();
        let matches=match kind {
            // 与主体识别同口径：只认同主干的 .partN.rar，不用 starts_with 宽匹配。
            "rar"=>rar_part_stem(&candidate).is_some_and(|s|s==stem),
            // 7-Zip 多卷可到 .1000+：按最少位数匹配（001/1000 都算兄弟卷），不写死恰好 3 位。
            "numbered"=>candidate.strip_prefix(&format!("{stem}.")).is_some_and(|v|v.len()>=3 && v.chars().all(|c|c.is_ascii_digit())),
            "oldrar"=>candidate.strip_prefix(&format!("{stem}.r")).is_some_and(|v|v.len()>=2 && v.chars().all(|c|c.is_ascii_digit())),
            "splitzip"=>candidate.strip_prefix(&format!("{stem}.z")).is_some_and(|v|v.len()>=2 && v.chars().all(|c|c.is_ascii_digit())),
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
/// 在已扫描文件中查找与暂存条目字节相同的副本（按文件名+大小预筛，再逐字节确认）。
fn find_identical_elsewhere(job: &Job, source: &Path, incoming: &crate::model::Snapshot, member_rel: &str) -> Result<Option<PathBuf>> {
    let name = Path::new(member_rel).file_name().and_then(|s|s.to_str()).context("无效压缩包成员名")?.to_lowercase();
    let size = i64::try_from(incoming.size).context("成员大小超出范围")?;
    let candidates: Vec<String> = {
        let mut statement = job.db.conn.prepare("SELECT rel FROM files WHERE active=1 AND name=?1 AND size=?2 ORDER BY id LIMIT 32")?;
        let rows = statement.query_map(params![name,size],|r|r.get::<_,String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let source_rel = fsutil::relative_string(&job.root,source)?;
    for rel in candidates {
        if rel == source_rel { continue; }
        let path = fsutil::safe_join(&job.root,&rel)?;
        // 候选可能已经在本次任务里被删除或移动（例如同名的原压缩包刚被回收），
        // 这种情况直接跳过：磁盘状态才是准的，不能让整个压缩包因此解压失败。
        let Ok(existing) = fsutil::snapshot(&path) else { continue; };
        if existing.size == incoming.size && hashing::equal_bytes(source,incoming,&path,&existing,&job.context.control)? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}
fn merge_extracted(job: &mut Job, source: &Path, target: &Path) -> Result<Option<PathBuf>> {
    let incoming = fsutil::snapshot(source).with_context(|| format!("读取暂存解压结果失败：{}",source.display()))?;
    fsutil::ensure_parent(&job.root,target)?;
    if !target.try_exists()? {
        match fsutil::rename_noreplace(source,target) {
            Ok(()) => return Ok(Some(target.to_path_buf())),
            Err(error) => {
                // TOCTOU：目标在 try_exists 与 rename 之间出现。与冲突删除路径一致，
                // 改用唯一名落盘，而不是整包失败。
                let emergency = fsutil::unique_target(&job.root,target)?;
                fsutil::rename_noreplace(source,&emergency).map_err(|_| error)?;
                return Ok(Some(emergency));
            }
        }
    }
    let meta = fs::symlink_metadata(target).with_context(|| format!("读取目标状态失败：{}",target.display()))?;
    if !meta.is_file() || fsutil::is_link(&meta) {
        let renamed = fsutil::unique_target(&job.root,target)?;
        fsutil::rename_noreplace(source,&renamed)?; return Ok(Some(renamed));
    }
    let existing = fsutil::snapshot(target).with_context(|| format!("读取已有目标失败：{}",target.display()))?;
    // Identical bytes are already at the destination. Treat as success so the source archive
    // can be deleted; otherwise Largest/Newest/Skip on equal size keep the archive forever,
    // and the next run re-extracts after classification moved the file away.
    if incoming.size == existing.size && hashing::equal_bytes(source,&incoming,target,&existing,&job.context.control)? {
        return Ok(Some(target.to_path_buf()));
    }
    let policy = job.archive_override.unwrap_or(job.config.extract_conflict);
    let policy = if policy == ConflictPolicy::Ask {
        let answer = (job.context.decisions)(ConflictInfo {
            existing: crate::platform::display_path_text(&target.display().to_string()),
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
    let mode = job.config.conflict_delete.resolve(job.config.global_delete);
    if mode == DeleteMode::Keep { return Ok(None); }
    // 冲突策略决定删除已有文件时写带策略名的明确原因，便于在日志/审计中看出
    // 是策略自动处理（默认 Newest 也会在用户未显式选择时替换旧文件）。
    let reason = match policy {
        ConflictPolicy::Overwrite => "解压冲突策略（覆盖）：已有文件将被解压结果替换".to_string(),
        ConflictPolicy::Newest => format!("解压冲突策略（较新）：已有文件较旧，将被替换（旧 {} 字节 / 新 {} 字节）",existing.size,incoming.size),
        ConflictPolicy::Largest => format!("解压冲突策略（较大）：已有文件较小，将被替换（旧 {} 字节 / 新 {} 字节）",existing.size,incoming.size),
        _ => "解压覆盖旧文件".to_string(),
    };
    let removed = job.delete_path(target,Some(&existing),mode,&reason)?;
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
    // 崩溃/强杀后 Drop 不会执行，.jchtools-work 下可能残留孤儿暂存目录；
    // 解压开始前清理超过 24 小时的残留（阈值远大于正常解压时长，避免误伤并发任务）。
    if let Ok(removed) = clean_orphan_staging(&job.root, Duration::from_secs(24 * 3600)) {
        if removed > 0 { job.context.status(format!("已清理 {removed} 个残留解压暂存目录")); }
    }
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
                // 归档行先标 failed，避免永久停在 running。
                // 用户主动取消：上抛取消错误，不累加 archives_failed/errors、不写失败日志
                // （与 apply_with 的取消口径一致）。单出口，避免双写 failed/误计失败。
                job.db.conn.execute("UPDATE archives SET state='failed' WHERE id=?1",[id])?;
                if job.context.control.is_cancelled() {
                    job.context.control.check_cancelled()?;
                }
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
/// 清理崩溃/断电后残留的孤儿暂存目录（<root>/.jchtools-work/<uuid>）。
/// 只删除带 OWNER 标记且内容与目录名一致的条目（Staging::new 写入的归属标记），
/// 并且目录年龄超过 max_age 才处理——阈值须远大于正常解压时长，避免误删并发任务
/// 正在使用的暂存区。返回清理数量；错误一律跳过单个条目，不影响主流程。
pub fn clean_orphan_staging(root: &Path, max_age: Duration) -> Result<usize> {
    let Ok(work) = fsutil::safe_join(root, ".jchtools-work") else { return Ok(0); };
    let Ok(entries) = fs::read_dir(&work) else { return Ok(0); };
    let now = SystemTime::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; }
        // OWNER 内容必须与目录名一致：这是 Staging::new 写入的归属标记。
        let owner = fs::read_to_string(path.join("OWNER")).unwrap_or_default();
        if owner != entry.file_name().to_string_lossy() { continue; }
        let Ok(meta) = fs::metadata(&path) else { continue; };
        let Ok(modified) = meta.modified() else { continue; };
        let Ok(age) = now.duration_since(modified) else { continue; };
        if age < max_age { continue; }
        if fs::remove_dir_all(&path).is_ok() { removed += 1; }
    }
    if removed > 0 { let _ = fs::remove_dir(&work); } // 仅当父目录已空时才会成功
    Ok(removed)
}
