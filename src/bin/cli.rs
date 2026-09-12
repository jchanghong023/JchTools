use anyhow::{bail,Context,Result};
use jchtools::{config::{Config,ConflictPolicy},control::Context as TaskContext,db::Database,engine};
use std::path::{Path,PathBuf};
fn argument(args:&[String],flag:&str)->Result<Option<String>>{
    match args.iter().position(|s|s==flag){
        Some(i)=>match args.get(i+1){
            Some(v)=>Ok(Some(v.clone())),
            None=>bail!("参数 {flag} 缺少对应的值"),
        },
        None=>Ok(None),
    }
}
fn run()->Result<()> {
    let args:Vec<String>=std::env::args().skip(1).collect();
    match args.first().map(String::as_str){
        Some("defaults")=>{let path=args.get(1).context("用法：jchtools-cli defaults rules.json")?;Config::default().save(Path::new(path))?;},
        Some("analyze")=>{
            let root=args.get(1).context("用法：jchtools-cli analyze <目录> [--config rules.json] [--extract --yes] [--engine /absolute/7zz]")?;
            let mut cfg=if let Some(path)=argument(&args,"--config")?{Config::load(Path::new(&path))?}else{Config::default()};
            cfg.extract=args.iter().any(|v|v=="--extract");
            if cfg.extract&&!args.iter().any(|v|v=="--yes"){bail!("解压会修改目录，需要显式 --extract --yes；不解压时仅生成计划");}
            if cfg.extract&&cfg.extract_conflict==ConflictPolicy::Ask{cfg.extract_conflict=ConflictPolicy::KeepBoth;}
            let engine_path=argument(&args,"--engine")?.map(PathBuf::from);
            let state=argument(&args,"--state")?.map(PathBuf::from).map(Ok).unwrap_or_else(jchtools::config::state_dir)?;
            eprintln!("{}",cfg.destructive_warning());
            let result=engine::prepare_at(Path::new(root),cfg,TaskContext::default(),&state,engine_path.as_deref())?;
            println!("任务目录：{}\n{}\n尚未执行去重/归类/清理。",result.directory.display(),result.summary.description());
        }
        Some("apply")=>{
            let task=args.get(1).context("用法：jchtools-cli apply <任务目录> --yes")?;
            if !args.iter().any(|s|s=="--yes"){bail!("必须先检查计划，再以 --yes 明确确认文件变更");}
            let db=Database::open(Path::new(task))?;println!("{}",db.config()?.destructive_warning());drop(db);
            let result=engine::apply(Path::new(task),TaskContext::default())?;println!("{}",result.summary.description());
        }
        Some("inspect")=>{let task=args.get(1).context("需要任务目录")?;let db=Database::open(Path::new(task))?;
            println!("{}",db.summary()?.description());for action in db.actions_page(0,100)?{
                let kind=match action.kind{jchtools::model::ActionKind::Delete=>"删除",jchtools::model::ActionKind::Move=>"移动",jchtools::model::ActionKind::Hardlink=>"硬链接",jchtools::model::ActionKind::EmptyDirectory=>"空目录复查"};
                println!("{} {} {} -> {} | {} [{}]",action.id,kind,action.source,action.target.unwrap_or_else(||"-".into()),action.reason,action.state);
            }},
        Some("report")=>{let task=args.get(1).context("需要任务目录")?;let output=args.get(2).context("需要新的 CSV 文件名")?;Database::open(Path::new(task))?.export_csv(Path::new(output))?;},
        _=>{println!("JchTools CLI\n  defaults <rules.json>\n  analyze <目录> [--config rules.json] [--extract --yes] [--engine 完整引擎绝对路径]\n  inspect <任务目录>\n  apply <任务目录> --yes\n  report <任务目录> <新建CSV路径>\n不带 --extract 的 analyze 不修改待整理文件；CLI 不交互询问解压冲突，询问策略改为保留两个。");
            std::process::exit(1);},
    }Ok(())
}
fn main(){if let Err(error)=run(){eprintln!("错误：{error:#}");std::process::exit(1);}}
