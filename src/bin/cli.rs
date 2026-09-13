use anyhow::{bail,Context as _,Result};
use jchtools::{config::{Config,ConflictPolicy},control::Context as TaskContext,db::Database,engine,model::ActionKind};
use std::ffi::{OsStr,OsString};
use std::path::{Path,PathBuf};
use std::sync::Arc;

const HELP:&str="JchTools CLI\n\
用法：\n\
  jchtools-cli defaults <配置文件.json> [--force]\n\
  jchtools-cli analyze <目录> [--config 配置文件.json] [--extract --yes] [--engine 完整引擎绝对路径] [--state 状态目录]\n\
  jchtools-cli inspect <任务目录>\n\
  jchtools-cli apply <任务目录> --yes\n\
  jchtools-cli report <任务目录> <新建CSV路径>\n\
  jchtools-cli --version\n\
\n\
说明：\n\
  - 不带 --extract 的 analyze 不修改待整理文件；CLI 不交互询问解压冲突，询问策略改为保留两个。\n\
  - defaults 在目标文件已存在时会拒绝覆盖，确认覆盖请附加 --force。\n\
  - analyze/apply 期间按 Ctrl+C 会请求取消任务，已写入的审计记录保留。\n";
const VERSION:&str=concat!("jchtools-cli ",env!("CARGO_PKG_VERSION"));

/// 解析后的命令行：带值 flag、开关 flag、有序位置参数。
/// 位置参数按出现顺序分配（如 report 的任务目录与 CSV 路径），flag 值不得以 `-` 开头。
struct CliArgs{
    positionals:Vec<OsString>,
    values:Vec<(String,OsString)>,
    switches:Vec<String>,
}
impl CliArgs{
    fn parse(args:&[OsString],value_flags:&[&str],bool_flags:&[&str])->Result<Self>{
        let mut out=Self{positionals:Vec::new(),values:Vec::new(),switches:Vec::new()};
        let mut i=0;
        while i<args.len(){
            let raw=&args[i];
            // 非 UTF-8 但以 - 开头：无法按 flag 识别，报错；其余按位置参数原样保留（路径可为任意编码）
            let Some(text)=raw.to_str()else{
                if raw.to_string_lossy().starts_with('-'){bail!("无法解析的参数（不是有效 UTF-8）：{}",raw.to_string_lossy());}
                out.positionals.push(raw.clone());i+=1;continue;
            };
            if text.starts_with('-')&&text!="-"{
                if value_flags.contains(&text){
                    let value=args.get(i+1).with_context(||format!("参数 {text} 缺少对应的值"))?;
                    let value_text=value.to_string_lossy();
                    if value_text.starts_with('-')&&value_text!="-"{
                        bail!("参数 {text} 的值不能以 - 开头：{value_text}");
                    }
                    out.values.push((text.to_string(),value.clone()));i+=2;continue;
                }
                if bool_flags.contains(&text){out.switches.push(text.to_string());i+=1;continue;}
                bail!("未知参数：{text}\n用法可用 jchtools-cli --help 查看");
            }
            out.positionals.push(raw.clone());i+=1;
        }
        Ok(out)
    }
    fn value(&self,name:&str)->Option<&OsStr>{
        self.values.iter().find(|(k,_)|k==name).map(|(_,v)|v.as_os_str())
    }
    fn has(&self,name:&str)->bool{self.switches.iter().any(|s|s==name)}
    fn positional(&self,index:usize,usage:&str)->Result<&OsStr>{
        self.positionals.get(index).map(|v|v.as_os_str()).with_context(||format!("用法：{usage}"))
    }
    /// 解析结束后校验位置参数个数：多余参数不再静默忽略。
    fn finish(self,usage:&str,expected:usize)->Result<Self>{
        if self.positionals.len()!=expected{
            bail!("用法：{usage}\n位置参数个数不符：期望 {expected}，实际收到 {}",self.positionals.len());
        }
        Ok(self)
    }
}

/// 打开任务库前确认 task.sqlite3 已存在，避免 Database::open 在缺失路径上新建空库。
fn ensure_task_db(task:&Path)->Result<()>{
    if !task.join("task.sqlite3").is_file(){
        bail!("任务目录缺少 task.sqlite3（目录不存在或不是有效任务目录）：{}",task.display());
    }
    Ok(())
}

/// 注册 Ctrl+C：触发 control.cancel()，engine 在检查点优雅停止。
fn install_ctrlc(ctx:&TaskContext){
    let control=Arc::clone(&ctx.control);
    // 进程内只能注册一次；重复注册忽略即可（单次运行只走一个子命令）
    let _=ctrlc::set_handler(move||control.cancel());
}

/// 向 stderr 打印计划动作统计（含已勾选数），作为 apply 前的轻量 dry-run 预览。
fn print_plan_stats(db:&Database)->Result<()>{
    let mut cursor=0i64;
    let mut delete=0u64;let mut moved=0u64;let mut link=0u64;let mut empty=0u64;
    let mut sel_delete=0u64;let mut sel_moved=0u64;let mut sel_link=0u64;let mut sel_empty=0u64;
    loop{
        let actions=db.actions_page(cursor,256)?;
        if actions.is_empty(){break;}
        cursor=actions.last().unwrap().id;
        for action in &actions{
            match action.kind{
                ActionKind::Delete=>{delete+=1;if action.selected{sel_delete+=1;}}
                ActionKind::Move=>{moved+=1;if action.selected{sel_moved+=1;}}
                ActionKind::Hardlink=>{link+=1;if action.selected{sel_link+=1;}}
                ActionKind::EmptyDirectory=>{empty+=1;if action.selected{sel_empty+=1;}}
            }
        }
    }
    eprintln!("计划统计：删除 {delete}（已选 {sel_delete}）、移动 {moved}（已选 {sel_moved}）、硬链接 {link}（已选 {sel_link}）、空目录复查 {empty}（已选 {sel_empty}）");
    Ok(())
}

fn run()->Result<()>{
    // 用 args_os 保留非 UTF-8 路径，避免 Windows 上 std::env::args() 直接 panic
    let args:Vec<OsString>=std::env::args_os().skip(1).collect();
    // -h/--help 与 --version：仅当首个参数（无子命令前）才短路。
    // 若在 apply/analyze 等子命令后出现，禁止 exit 0 伪成功（会落到未知参数错误）。
    match args.first().and_then(|a|a.to_str()){
        Some("-h")|Some("--help")=>{print!("{HELP}");std::process::exit(0);}
        Some("--version")=>{println!("{VERSION}");std::process::exit(0);}
        _=>{}
    }
    let Some(sub)=args.first()else{eprint!("{HELP}");std::process::exit(1);};
    let sub=sub.to_str().context("子命令名必须是有效 UTF-8")?;
    let rest=&args[1..];
    match sub{
        "defaults"=>{
            let usage="jchtools-cli defaults <配置文件.json> [--force]";
            let parsed=CliArgs::parse(rest,&[],&["--force"])?.finish(usage,1)?;
            let path=PathBuf::from(parsed.positional(0,usage)?);
            // 已存在则拒绝覆盖，确认后附加 --force；目录路径一律拒绝（避免对目录做原子写）。
            if path.is_dir(){
                bail!("目标是目录而不是配置文件：{}",path.display());
            }
            if path.exists()&&!parsed.has("--force"){
                bail!("配置文件已存在：{}；如确认覆盖请附加 --force",path.display());
            }
            Config::default().save(&path)?;
        }
        "analyze"=>{
            let usage="jchtools-cli analyze <目录> [--config 配置文件.json] [--extract --yes] [--engine 完整引擎绝对路径] [--state 状态目录]";
            let parsed=CliArgs::parse(rest,&["--config","--engine","--state"],&["--extract","--yes"])?.finish(usage,1)?;
            let root=PathBuf::from(parsed.positional(0,usage)?);
            let mut cfg=if let Some(path)=parsed.value("--config"){Config::load(Path::new(path))?}else{Config::default()};
            cfg.extract=parsed.has("--extract");
            if cfg.extract&&!parsed.has("--yes"){bail!("解压会修改目录，需要显式 --extract --yes；不解压时仅生成计划");}
            if !cfg.extract&&parsed.has("--yes"){eprintln!("提示：不带 --extract 时不会修改文件，--yes 被忽略；执行计划请使用 apply <任务目录> --yes");}
            if cfg.extract&&cfg.extract_conflict==ConflictPolicy::Ask{cfg.extract_conflict=ConflictPolicy::KeepBoth;}
            let engine_path=parsed.value("--engine").map(PathBuf::from);
            let state=match parsed.value("--state"){
                Some(p)=>PathBuf::from(p),
                None=>jchtools::config::state_dir()?,
            };
            eprintln!("{}",cfg.destructive_warning());
            let ctx=TaskContext::default();
            install_ctrlc(&ctx);
            // 失败/取消时 prepare_at 仍会创建任务目录，但 API 只在成功时返回路径；
            // 先记录已有任务目录，失败后把新增目录路径写到 stderr，便于事后检查。
            let tasks_root=state.join("tasks");
            let before:std::collections::HashSet<PathBuf>=std::fs::read_dir(&tasks_root).ok().map(|rd|rd.flatten().filter(|e|e.path().is_dir()).map(|e|e.path()).collect()).unwrap_or_default();
            let prepared=engine::prepare_at(&root,cfg,ctx,&state,engine_path.as_deref());
            match prepared{
                Ok(result)=>{
                    println!("任务目录：{}\n{}\n尚未执行去重/归类/清理。",result.directory.display(),result.summary.description());
                }
                Err(error)=>{
                    let created:Vec<PathBuf>=std::fs::read_dir(&tasks_root).ok().map(|rd|rd.flatten().filter(|e|e.path().is_dir()).map(|e|e.path()).filter(|p|!before.contains(p)).collect()).unwrap_or_default();
                    if let Some(dir)=created.into_iter().max_by_key(|p|std::fs::metadata(p).and_then(|m|m.modified()).ok()){
                        eprintln!("任务目录：{}（analyze 失败或已取消，审计记录保留）",dir.display());
                    }
                    return Err(error);
                }
            }
        }
        "apply"=>{
            let usage="jchtools-cli apply <任务目录> --yes";
            let parsed=CliArgs::parse(rest,&[],&["--yes"])?.finish(usage,1)?;
            let task=PathBuf::from(parsed.positional(0,usage)?);
            if !parsed.has("--yes"){bail!("必须先检查计划，再以 --yes 明确确认文件变更");}
            ensure_task_db(&task)?;
            let db=Database::open_existing(&task)?;
            // 警告与计划统计走 stderr，避免污染可重定向的 stdout 结果
            eprintln!("{}",db.config()?.destructive_warning());
            print_plan_stats(&db)?;
            drop(db);
            let ctx=TaskContext::default();
            install_ctrlc(&ctx);
            let result=engine::apply(&task,ctx)?;
            println!("{}",result.summary.description());
        }
        "inspect"=>{
            let usage="jchtools-cli inspect <任务目录>";
            let parsed=CliArgs::parse(rest,&[],&[])?.finish(usage,1)?;
            let task=PathBuf::from(parsed.positional(0,usage)?);
            ensure_task_db(&task)?;
            let db=Database::open_existing(&task)?;
            println!("{}",db.summary()?.description());
            // 分页取完所有计划项：只取前 100 条会静默丢掉大计划里的大部分动作。
            let mut cursor=0i64;
            loop{
                let actions=db.actions_page(cursor,256)?;
                if actions.is_empty(){break;}
                cursor=actions.last().unwrap().id;
                for action in &actions{
                    let kind=match action.kind{ActionKind::Delete=>"删除",ActionKind::Move=>"移动",ActionKind::Hardlink=>"硬链接",ActionKind::EmptyDirectory=>"空目录复查"};
                    // 对齐 GUI：target 为空时回落显示保留文件（keeper），避免计划表出现空目标列。
                    let target=action.target.clone().unwrap_or_else(||
                        action.keeper.as_ref().map(|v|format!("保留 {}",v.0)).unwrap_or_default());
                    println!("{} {} {} -> {} | {} [{}] selected={}",action.id,kind,action.source,target,action.reason,action.state,action.selected);
                }
            }
        }
        "report"=>{
            let usage="jchtools-cli report <任务目录> <新建CSV路径>";
            let parsed=CliArgs::parse(rest,&[],&[])?.finish(usage,2)?;
            let task=PathBuf::from(parsed.positional(0,usage)?);
            let output=PathBuf::from(parsed.positional(1,usage)?);
            ensure_task_db(&task)?;
            Database::open_existing(&task)?.export_csv(&output)?;
        }
        other=>{
            eprintln!("未知子命令：{other}");
            eprint!("{HELP}");
            std::process::exit(1);
        }
    }
    Ok(())
}
fn main(){if let Err(error)=run(){eprintln!("错误：{error:#}");std::process::exit(1);}}
