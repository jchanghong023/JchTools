#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
use anyhow::{Context as _, Result};
use jchtools::{config::{self,Config,ConflictPolicy}, control::{ConflictAnswer,Context,Control,Event},
    db::Database, engine, model::{bytes,ActionKind}, platform, registry};
use serde::Deserialize;
use slint::{ComponentHandle,Model,ModelRc,SharedString,VecModel};
use std::{cell::RefCell,collections::VecDeque,path::{Path,PathBuf},rc::Rc,
    sync::{atomic::Ordering,mpsc,Arc},time::{Duration,Instant}};
slint::include_modules!();

#[derive(Clone,Deserialize)]
struct RuleSpec {section:String,key:String,title:String,hint:String,kind:String,choices:Vec<Vec<String>>}
struct State {
    config:Config,specs:Vec<RuleSpec>,section:String,task:Option<PathBuf>,history:Vec<PathBuf>,
    control:Option<Arc<Control>>,conflict:Option<mpsc::SyncSender<ConflictAnswer>>,
    logs:VecDeque<String>,page:usize,page_starts:Vec<i64>,started:Instant,close_after:bool,pending_selection:usize,applying:bool,planned:u64,
    plan_filter:Option<String>,archives_failed:u64,
}
struct WindowDrag {origin:(f64,f64),press:(f64,f64),restoring:bool}
/// 光标的屏幕坐标（物理像素）。拖动必须基于屏幕坐标：窗口自身移动不会改变它，因此不会出现
/// “移动窗口 → 局部坐标回跳 → 窗口被拉回去”的反馈抖动。
#[cfg(windows)]
fn pointer_position()->Option<(f64,f64)>{
    use windows_sys::Win32::{Foundation::POINT,UI::WindowsAndMessaging::GetCursorPos};
    let mut point=POINT{x:0,y:0};
    (unsafe{GetCursorPos(&mut point)}!=0).then_some((point.x as f64,point.y as f64))
}
#[cfg(not(windows))]
fn pointer_position()->Option<(f64,f64)>{None}
/// 启动时把窗口居中：上下留白相等、左右留白相等，同时保证不越过工作区（任务栏）。
#[cfg(windows)]
fn center_window(window:&slint::Window){
    use windows_sys::Win32::{Foundation::RECT,UI::WindowsAndMessaging::{GetSystemMetrics,SM_CXSCREEN,SM_CYSCREEN,SystemParametersInfoW,SPI_GETWORKAREA}};
    let mut work=RECT{left:0,top:0,right:0,bottom:0};
    let ok=unsafe{SystemParametersInfoW(SPI_GETWORKAREA,0,&mut work as *mut RECT as *mut core::ffi::c_void,0)};
    if ok==0{return;}
    let screen_w=unsafe{GetSystemMetrics(SM_CXSCREEN)}.max(1);
    let screen_h=unsafe{GetSystemMetrics(SM_CYSCREEN)}.max(1);
    let size=window.size();
    let width=size.width as i32;
    let height=size.height as i32;
    // 以整块屏幕计算对称留白，再夹回工作区，避免压住任务栏或跑到屏幕外
    let x=(screen_w-width)/2;
    let y=(screen_h-height)/2;
    let x=x.clamp(work.left,(work.right-width).max(work.left));
    let y=y.clamp(work.top,(work.bottom-height).max(work.top));
    window.set_position(slint::PhysicalPosition::new(x,y));
}
#[cfg(not(windows))]
fn center_window(_window:&slint::Window){}
/// 读取系统“应用使用浅色主题”设置，供自绘配色在“跟随系统”时保持一致。
#[cfg(windows)]
fn system_dark()->bool{
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let path:Vec<u16>="Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize".encode_utf16().chain(std::iter::once(0)).collect();
    let name:Vec<u16>="AppsUseLightTheme".encode_utf16().chain(std::iter::once(0)).collect();
    let mut value:u32=1;
    let mut size=std::mem::size_of::<u32>() as u32;
    let status=unsafe{RegGetValueW(HKEY_CURRENT_USER,path.as_ptr(),name.as_ptr(),RRF_RT_REG_DWORD,
        std::ptr::null_mut(),&mut value as *mut u32 as *mut core::ffi::c_void,&mut size)};
    status==0&&value==0
}
#[cfg(not(windows))]
fn system_dark()->bool{false}
/// 选项可带第三项作为该选项的说明；否则回落到规则默认 hint。
/// 用于「选中项改变后，下面那行注释文字跟着变」。
fn hint_for(spec:&RuleSpec,value:&serde_json::Value)->String{
    if spec.kind=="choice"{
        if let Some(pos)=spec.choices.iter().position(|c|Some(c[0].as_str())==value.as_str()){
            if let Some(hint)=spec.choices[pos].get(2){return hint.clone();}
        }
    }
    spec.hint.clone()
}
fn rule_row(spec:&RuleSpec,data:&serde_json::Value)->RuleRow {
    let value=&data[&spec.key];
    RuleRow {key:spec.key.clone().into(),title:spec.title.clone().into(),hint:hint_for(spec,value).into(),
        kind:if spec.kind=="bool"{0}else if spec.kind=="choice"{1}else{2},
        checked:value.as_bool().unwrap_or(false),
        index:spec.choices.iter().position(|c|Some(c[0].as_str())==value.as_str()).unwrap_or(0) as i32,
        options:Rc::new(VecModel::from(spec.choices.iter().map(|c|SharedString::from(c[1].as_str())).collect::<Vec<_>>())).into(),
        value:value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string()).into()}
}
fn rule_rows(state:&State)->Result<ModelRc<RuleRow>> {
    let data=serde_json::to_value(&state.config)?;
    let rows=state.specs.iter().filter(|s|s.section==state.section).map(|spec|rule_row(spec,&data)).collect::<Vec<_>>();
    Ok(Rc::new(VecModel::from(rows)).into())
}
/// 「设置与规则预设」页要能一次看完所有规则，不受左侧分区筛选影响。
/// 「应用」分区的规则（当前只有主题）排在最前：它是全窗口生效、最常改的一项。
fn all_rule_rows(state:&State)->Result<ModelRc<RuleRow>> {
    let data=serde_json::to_value(&state.config)?;
    let mut specs=state.specs.iter().collect::<Vec<_>>();
    specs.sort_by_key(|spec|if spec.section=="应用"{0}else{1});
    let rows=specs.into_iter().map(|spec|rule_row(spec,&data)).collect::<Vec<_>>();
    Ok(Rc::new(VecModel::from(rows)).into())
}
fn refresh(ui:&AppWindow,state:&State) {
    if let Ok(rows)=rule_rows(state){ui.set_rules(rows);}
    if let Ok(rows)=all_rule_rows(state){ui.set_all_rules(rows);}
    ui.set_theme(match state.config.theme.as_str(){"light"=>1,"dark"=>2,_=>0});
}
fn invalidate(ui:&AppWindow){
    ui.set_ready(false);
    if ui.get_has_task(){ui.set_status("规则或目录已改变，请重新解压与分析后再执行".into());return;}
    if ui.get_directory().is_empty(){ui.set_status("请选择需要整理的目录".into());}
    else{ui.set_status("目录已就绪；修改规则后可开始解压与分析".into());}
}
fn apply_theme(ui:&AppWindow,state:&State){
    ui.set_theme(match state.config.theme.as_str(){"light"=>1,"dark"=>2,_=>0});
}
/// 只改单行显示，避免整表重建打断 ComboBox/CheckBox（用户点选后文字停在旧值的根因）。
/// 主页面与「设置与规则预设」页共用同一份规则数据，两边都要就地更新。
fn patch_rule_row(ui:&AppWindow,key:&str,mutate:impl Fn(&mut RuleRow)){
    for rules in [ui.get_rules(),ui.get_all_rules()] {
        let Some(model)=rules.as_any().downcast_ref::<VecModel<RuleRow>>() else{continue;};
        for i in 0..model.row_count(){
            if let Some(mut row)=model.row_data(i){
                if row.key.as_str()==key{mutate(&mut row);model.set_row_data(i,row);break;}
            }
        }
    }
}
fn changed(ui:&AppWindow,state:&Rc<RefCell<State>>,key:&str,value:serde_json::Value,rebuild:bool)->bool{
    let mut state=state.borrow_mut();
    match state.config.set_json(key,value){
        Ok(())=>{
            // 立刻反馈规则之间的依赖（例如“修正扩展名”需要先开“检测真实类型”），
            // 不要让用户等到点“开始解压与分析”才知道配置不成立。
            match state.config.validate(){
                Ok(())=>ui.set_error_text("".into()),
                Err(error)=>ui.set_error_text(format!("{error:#}").into()),
            }
            invalidate(ui);
            if rebuild{refresh(ui,&state);}
            else if key=="theme"{apply_theme(ui,&state);}
            true
        },
        Err(error)=>{ui.set_error_text(format!("{error:#}").into());false}
    }
}
fn async_work(sender:mpsc::SyncSender<Event>,work:impl FnOnce()->Result<Event>+Send+'static){
    std::thread::spawn(move||{
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
        let event=match result{Ok(Ok(event))=>event,Ok(Err(error))=>Event::Error(format!("{error:#}")),
            Err(_)=>Event::Error("后台操作意外退出；请查看记录，未执行的步骤不会继续".into())};
        let _=sender.send(event);
    });
}
fn load_plan_filtered(sender:mpsc::SyncSender<Event>,path:PathBuf,start:i64,page:usize,kind:Option<String>){
    async_work(sender,move||Ok(Event::PlanPage(path.clone(),Database::open(&path)?.actions_page_filtered(start,101,kind.as_deref())?,page)));
}
fn start_task(ui:&AppWindow,state:&Rc<RefCell<State>>,sender:&mpsc::SyncSender<Event>,apply:bool){
    if ui.get_busy() || state.borrow().pending_selection != 0 {return;}
    let (configuration,task)={let s=state.borrow();(s.config.clone(),s.task.clone())};
    if let Err(error)=configuration.validate(){ui.set_error_text(format!("{error:#}").into());return;}
    let directory=PathBuf::from(ui.get_directory().as_str());
    if apply&&task.is_none(){ui.set_error_text("还没有可以执行的计划".into());return;}
    let control=Arc::new(Control::default());
    {
        let mut s=state.borrow_mut();s.control=Some(control.clone());s.started=Instant::now();
        s.logs.clear();s.page=0;s.page_starts=vec![0];s.conflict=None;s.applying=apply;
    }
    ui.set_busy(true);ui.set_ready(false);ui.set_paused(false);ui.set_error_text("".into());ui.set_notice_text("".into());ui.set_panel(2);
    ui.set_log_text("".into());ui.set_status(if apply{"正在执行已确认的整理计划"}else{"准备扫描与解压；不会提前执行去重或归类"}.into());
    let ask_sender=sender.clone();let ask_control=control.clone();
    let context=Context{control:control.clone(),events:Some(sender.clone()),decisions:Arc::new(move|info|{
        let(reply_sender,reply_receiver)=mpsc::sync_channel(1);
        ask_sender.send(Event::Conflict(info,reply_sender)).context("界面已经关闭")?;
        loop{
            ask_control.check_cancelled()?;
            match reply_receiver.recv_timeout(Duration::from_millis(100)){
                Ok(reply)=>return Ok(reply),Err(mpsc::RecvTimeoutError::Timeout)=>(),
                Err(_)=>anyhow::bail!("冲突选择窗口已关闭"),
            }
        }
    })};
    let sender=sender.clone();
    std::thread::spawn(move||{
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{
            if apply{engine::apply(task.as_ref().unwrap(),context)}else{engine::prepare(&directory,configuration,context)}
        }));
        let event=match result{
            Ok(Ok(result))=>if apply{Event::Done(result.directory,result.summary)}else{Event::Ready(result.directory,result.summary)},
            Ok(Err(error))=>Event::Failed(format!("{error:#}")),
            Err(_)=>Event::Failed("整理线程意外退出；请查看本机任务记录，不会自动重放任务".into()),
        };
        let _=sender.send(event);
    });
}
fn show_error(ui:&AppWindow,error:impl std::fmt::Display){ui.set_error_text(error.to_string().into());}
/// 规则文件、报告的默认落盘位置：桌面（没有桌面目录就退到主目录）。
/// 不用“上次用过的目录”，否则默认会把配置或报告写进正在整理的目录里。
fn user_file_directory()->Option<PathBuf>{
    directories_next::UserDirs::new().and_then(|dirs|dirs.desktop_dir().map(Path::to_path_buf).or_else(||Some(dirs.home_dir().to_path_buf())))
}
fn run()->Result<()> {
    let ui=AppWindow::new()?;
    ui.set_system_dark(system_dark());
    let specs:Vec<RuleSpec>=serde_json::from_str(include_str!("../resources/rules.json"))?;
    let state=Rc::new(RefCell::new(State{config:Config::default(),specs,section:"解压".into(),task:None,history:Vec::new(),
        control:None,conflict:None,logs:VecDeque::new(),page:0,page_starts:vec![0],started:Instant::now(),close_after:false,pending_selection:0,applying:false,planned:0,
        plan_filter:None,archives_failed:0}));
    let(sender,receiver)=mpsc::sync_channel::<Event>(256);
    let tools=registry::tools().iter().map(|tool|ToolRow{id:tool.id.into(),name:tool.name.into(),summary:tool.summary.into()}).collect::<Vec<_>>();
    ui.set_tool_count(registry::tools().len() as i32);
    ui.set_tools(Rc::new(VecModel::from(tools)).into());refresh(&ui,&state.borrow());
    {
        let weak=ui.as_weak();
        ui.on_choose_directory(move||{if let Some(ui)=weak.upgrade(){
            let mut dialog=rfd::FileDialog::new().set_title("选择需要整理的目录");
            // 已经输入过目录时从这里开始，省掉用户重新导航一遍。
            let entered=PathBuf::from(ui.get_directory().as_str());
            if entered.is_dir(){ dialog=dialog.set_directory(&entered); }
            if let Some(path)=dialog.pick_folder(){
                ui.set_directory(path.display().to_string().into());invalidate(&ui);
            }
        }});
    }
    {let weak=ui.as_weak();ui.on_root_edited(move||{if let Some(ui)=weak.upgrade(){invalidate(&ui);}});}
    {
        let weak=ui.as_weak();let state=state.clone();
        ui.on_select_tool(move|id|{if id.as_str()=="directory-organizer"{if let Some(ui)=weak.upgrade(){
            ui.set_screen(0);state.borrow_mut().section="解压".into();ui.set_section(0);refresh(&ui,&state.borrow());
        }}});
    }
    {
        let weak=ui.as_weak();
        ui.on_search_tools(move|query|{if let Some(ui)=weak.upgrade(){
            let query=query.to_lowercase();let tools=registry::tools().iter().filter(|t|format!("{} {}",t.name,t.summary).to_lowercase().contains(&query))
                .map(|t|ToolRow{id:t.id.into(),name:t.name.into(),summary:t.summary.into()}).collect::<Vec<_>>();
            ui.set_tools(Rc::new(VecModel::from(tools)).into());
        }});
    }
    {
        let weak=ui.as_weak();let state=state.clone();let sender=sender.clone();
        ui.on_navigation(move|screen|{if let Some(ui)=weak.upgrade(){ui.set_screen(screen);
            if screen==2{state.borrow_mut().section="应用".into();refresh(&ui,&state.borrow());}
            if screen==1{async_work(sender.clone(),||Ok(Event::History(engine::history(&config::state_dir()?)?)));}
        }});
    }
    {
        let weak=ui.as_weak();let state=state.clone();
        ui.on_select_section(move|index|{if let Some(section)=["解压","去重","冲突","归类","清理","安全与性能"].get(index.max(0)as usize){
            state.borrow_mut().section=section.to_string();if let Some(ui)=weak.upgrade(){ui.set_section(index);refresh(&ui,&state.borrow());}
        }});
    }
    // 布尔/下拉：不整表重建 rules。重建会在 selected/toggled 回调里销毁 ListView 里的
    // ComboBox，用户点选后的文字会停在旧值；改为配置写入 + 就地更新该行。
    {let weak=ui.as_weak();let state=state.clone();ui.on_rule_bool(move|key,value|{
        if let Some(ui)=weak.upgrade(){
            if changed(&ui,&state,key.as_str(),value.into(),false){
                patch_rule_row(&ui,key.as_str(),|row|row.checked=value);
            }
        }
    });}
    {
        let weak=ui.as_weak();let state=state.clone();
        ui.on_rule_choice(move|key,index|{if let Some(ui)=weak.upgrade(){
            let selected=state.borrow().specs.iter().find(|s|s.key==key.as_str()).and_then(|s|s.choices.get(index.max(0)as usize)).map(|c|c[0].clone());
            if let Some(value)=selected{
                let hint=state.borrow().specs.iter().find(|s|s.key==key.as_str())
                    .map(|s|hint_for(s,&serde_json::Value::from(value.as_str())));
                if changed(&ui,&state,key.as_str(),value.into(),false){
                    patch_rule_row(&ui,key.as_str(),|row|{
                        row.index=index;
                        if let Some(hint)=&hint{row.hint=hint.clone().into();}
                    });
                }
            }
        }});
    }
    {
        let weak=ui.as_weak();let state=state.clone();
        ui.on_rule_text(move|key,value|{if let Some(ui)=weak.upgrade(){
            let numeric=state.borrow().specs.iter().find(|s|s.key==key.as_str()).is_some_and(|s|s.kind=="number");
            let parsed=if numeric{match value.parse::<u64>(){Ok(v)=>serde_json::Value::from(v),Err(_)=>{show_error(&ui,"该设置需要输入非负整数");return;}}}
                else{serde_json::Value::from(value.to_string())};
            if changed(&ui,&state,key.as_str(),parsed.clone(),false){
                // 模型行里的 value 是重建列表（切分区、恢复默认规则）时的唯一来源，必须跟着更新，
                // 否则重建后这一行会拿旧值覆盖刚改好的设置。
                let canonical=parsed.as_u64().map(|v|v.to_string()).unwrap_or_else(||value.to_string());
                patch_rule_row(&ui,key.as_str(),|row|row.value=canonical.clone().into());
            }
        }});
    }
    {
        let weak=ui.as_weak();let state=state.clone();
        ui.on_request_start(move||{if let Some(ui)=weak.upgrade(){
            if let Err(error)=state.borrow().config.validate(){show_error(&ui,error);return;}
            ui.set_confirm_text(format!("目标目录：{}\n\n现在将实际解压压缩包，并按规则处理解压冲突和原压缩包。\n解压后再计算 Hash；后续去重、归类和清理仍会等待第二次确认。\n\n{}",ui.get_directory(),state.borrow().config.destructive_warning()).into());
            ui.set_acknowledge(false);ui.set_confirm_kind(1);
        }});
    }
    {
        let weak=ui.as_weak();let state=state.clone();
        ui.on_request_apply(move||{if let Some(ui)=weak.upgrade(){
            if !ui.get_ready(){return;}
            ui.set_confirm_text(format!("目标目录：{}\n执行“整理计划”中已勾选的操作；取消勾选的项目不会执行。\n\n{}\n\n{}",ui.get_directory(),state.borrow().config.destructive_warning(),ui.get_summary()).into());
            ui.set_acknowledge(false);ui.set_confirm_kind(2);
        }});
    }
    {
        let weak=ui.as_weak();let state=state.clone();let sender=sender.clone();
        ui.on_confirmed(move|kind|{if let Some(ui)=weak.upgrade(){
            if kind==3{state.borrow_mut().close_after=true;if let Some(control)=&state.borrow().control{control.cancel();}
                ui.set_status("取消任务中，完成当前安全操作后关闭".into());ui.set_conflict_visible(false);}
            else{start_task(&ui,&state,&sender,kind==2);}
        }});
    }
    {
        let weak=ui.as_weak();let state=state.clone();ui.on_pause_task(move||{if let Some(ui)=weak.upgrade(){
            if let Some(control)=&state.borrow().control{let pause=!control.is_paused();control.pause(pause);ui.set_paused(pause);
                ui.set_status(if pause{"已请求暂停；正在运行的压缩包在完成后暂停，Hash 和整理操作在分块/文件边界暂停"}else{"继续处理"}.into());}
        }});
    }
    {
        let weak=ui.as_weak();let state=state.clone();ui.on_cancel_task(move||{if let Some(control)=&state.borrow().control{control.cancel();}
            if let Some(ui)=weak.upgrade(){ui.set_status("正在取消；不会继续后续删除和移动".into());ui.set_conflict_visible(false);}
        });
    }
    {
        let weak=ui.as_weak();let state=state.clone();
        ui.on_answer_conflict(move|index,all|{let policy=match index{0=>ConflictPolicy::Overwrite,1=>ConflictPolicy::Skip,2=>ConflictPolicy::Newest,3=>ConflictPolicy::Largest,_=>ConflictPolicy::KeepBoth};
            if let Some(reply)=state.borrow_mut().conflict.take(){let _=reply.send(ConflictAnswer{policy,apply_all:all});}
            if let Some(ui)=weak.upgrade(){ui.set_conflict_visible(false);}
        });
    }
    {
        let state=state.clone();let sender=sender.clone();let weak=ui.as_weak();
        ui.on_plan_toggle(move|id,selected|{let task=state.borrow().task.clone();if let(Some(task),Ok(id))=(task,id.parse::<i64>()){
            if let Some(ui)=weak.upgrade(){if ui.get_busy(){return;}ui.set_ready(false);}
            state.borrow_mut().pending_selection += 1;
            let sender=sender.clone();
            std::thread::spawn(move||{
                let result=Database::open(&task).and_then(|db|db.set_selected(id,selected));
                let _=sender.send(Event::SelectionSaved(task,result.err().map(|e|format!("{e:#}"))));
            });
        }});
    }
    {
        let state=state.clone();let sender=sender.clone();
        ui.on_plan_page(move|direction|{let mut state=state.borrow_mut();let Some(task)=state.task.clone()else{return;};
            let next=if direction<0{state.page.saturating_sub(1)}else{state.page+1};
            if let Some(start)=state.page_starts.get(next).copied(){state.page=next;load_plan_filtered(sender.clone(),task,start,next,state.plan_filter.clone());}
        });
    }
    {
        // 计划类型筛选：""=全部，否则 snake_case kind
        let state=state.clone();let sender=sender.clone();let weak=ui.as_weak();
        ui.on_filter_plan(move|kind|{let mut s=state.borrow_mut();let Some(task)=s.task.clone()else{return;};
            s.plan_filter=if kind.is_empty(){None}else{Some(kind.to_string())};
            s.page=0;s.page_starts=vec![0];
            if let Some(ui)=weak.upgrade(){ui.set_plan_prev_enabled(false);ui.set_plan_next_enabled(false);}
            load_plan_filtered(sender.clone(),task,0,0,s.plan_filter.clone());
        });
    }
    {
        let state=state.clone();let weak=ui.as_weak();
        ui.on_open_report(move||{if let Some(path)=&state.borrow().task{if let Err(error)=open::that(path){if let Some(ui)=weak.upgrade(){show_error(&ui,error);}}}});
    }
    {
        let state=state.clone();let sender=sender.clone();
        ui.on_export_report(move||{let task=state.borrow().task.clone();if let Some(task)=task{
            let mut dialog=rfd::FileDialog::new().set_file_name("整理报告.csv").add_filter("CSV",&["csv"]);
            if let Some(directory)=user_file_directory(){ dialog=dialog.set_directory(&directory); }
            if let Some(path)=dialog.save_file(){
                async_work(sender.clone(),move||{Database::open(&task)?.export_csv(&path)?;Ok(Event::Notice(format!("已导出：{}",path.display())))});
            }
        }});
    }
    {
        let state=state.clone();let sender=sender.clone();let weak=ui.as_weak();
        ui.on_load_history(move|index|{if weak.upgrade().is_none_or(|ui|ui.get_busy()) || state.borrow().pending_selection>0{return;}
          if let Some(path)=state.borrow().history.get(index.max(0)as usize).cloned(){
            async_work(sender.clone(),move||{let db=Database::open(&path)?;let status:String=db.get("status")?;
                Ok(Event::LoadedTask(path,db.get("root")?,db.config()?,db.summary()?,status=="ready"))});
        }});
    }
    {
        let state=state.clone();let sender=sender.clone();let weak=ui.as_weak();
        ui.on_presets(move|kind|{
            let cfg=state.borrow().config.clone();
            let path=match kind{
                0=>config::state_dir().ok().map(|p|p.join("config.json")),
                1=>{let mut dialog=rfd::FileDialog::new().set_file_name("jchtools-rules.json");
                    if let Some(directory)=user_file_directory(){dialog=dialog.set_directory(&directory);} dialog.save_file()},
                2=>{let mut dialog=rfd::FileDialog::new().add_filter("JSON",&["json"]);
                    if let Some(directory)=user_file_directory(){dialog=dialog.set_directory(&directory);} dialog.pick_file()},
                _=>{state.borrow_mut().config=Config::default();if let Some(ui)=weak.upgrade(){refresh(&ui,&state.borrow());invalidate(&ui);}return;}
            };
            if let Some(path)=path{async_work(sender.clone(),move||{
                if kind==2{Ok(Event::ConfigLoaded(Some(path.clone()),Config::load(&path)?))}
                else{cfg.save(&path)?;Ok(Event::Notice(format!("规则已保存：{}",path.display())))}
            });}
        });
    }
    {
        let weak=ui.as_weak();let drag=Rc::new(RefCell::new(None::<WindowDrag>));
        let anchor=drag.clone();
        ui.on_window_drag_start(move|x,y|{if let Some(ui)=weak.upgrade(){
            let window=ui.window();let restoring=window.is_maximized();
            if restoring{window.set_maximized(false);}
            let position=window.position();
            *anchor.borrow_mut()=Some(WindowDrag{origin:(position.x as f64,position.y as f64),
                press:pointer_position().unwrap_or((x as f64,y as f64)),restoring});
        }});
        let weak=ui.as_weak();
        ui.on_window_drag_move(move|x,y|{
            let Some(ui)=weak.upgrade()else{return;};
            let mut state=drag.borrow_mut();let Some(drag)=state.as_mut()else{return;};
            let pointer=pointer_position().unwrap_or((x as f64,y as f64));
            if drag.restoring{
                // 还原尚未生效：等窗口离开最大化后再重新锚定，避免和系统还原位置互相覆盖
                if ui.window().is_maximized(){return;}
                let position=ui.window().position();
                drag.origin=(position.x as f64,position.y as f64);drag.press=pointer;drag.restoring=false;
                return;
            }
            ui.window().set_position(slint::PhysicalPosition::new(
                (drag.origin.0+pointer.0-drag.press.0).round() as i32,
                (drag.origin.1+pointer.1-drag.press.1).round() as i32));
        });
    }
    {
        let state=state.clone();let weak=ui.as_weak();
        ui.window().on_close_requested(move||{
            if let Some(ui)=weak.upgrade(){if ui.get_busy(){
                ui.set_confirm_text("任务仍在处理文件。确认后会请求取消，等待正在进行的操作结束，再关闭窗口。已经完成的操作不会自动回滚。".into());
                ui.set_confirm_kind(3);ui.set_acknowledge(false);return slint::CloseRequestResponse::KeepWindowShown;
            }}
            if let Some(control)=&state.borrow().control{control.cancel();}
            slint::CloseRequestResponse::HideWindow
        });
    }
    let timer=slint::Timer::default();
    {
        let weak=ui.as_weak();let state=state.clone();let sender=sender.clone();
        timer.start(slint::TimerMode::Repeated,Duration::from_millis(100),move||{
            let Some(ui)=weak.upgrade()else{return;};
            let mut log_changed=false;
            for event in receiver.try_iter().take(256){
                match event{
                    Event::Status(text)=>{if !ui.get_paused(){ui.set_status(text.into());}},
                    Event::Log(text)=>{let mut s=state.borrow_mut();if s.logs.len()==300{s.logs.pop_front();}s.logs.push_back(text);log_changed=true;},
                    Event::Conflict(info,reply)=>{
                        state.borrow_mut().conflict=Some(reply);
                        let show=platform::display_path_text;
                        ui.set_conflict_text(format!("目标：{}\n\n现有文件：{} · 修改时间 {}\n新解压文件：{} · 修改时间 {}\n\n覆盖旧文件仍遵守已配置的回收站 / 永久删除策略；选择前不会继续后续解压。",
                            show(&info.existing),bytes(info.existing_size),platform::display_time_text(info.existing_time),
                            bytes(info.incoming_size),platform::display_time_text(info.incoming_time)).into());
                        ui.set_conflict_choice(4);ui.set_conflict_all(false);ui.set_conflict_visible(true);
                        ui.set_status("正在等待你选择解压冲突策略；选择前不会继续后续解压".into());
                    }
                    Event::Ready(path,summary)|Event::Done(path,summary)=>{
                        let ready=Database::open(&path).and_then(|db|db.get::<String>("status")).is_ok_and(|v|v=="ready");
                        let filter={let mut s=state.borrow_mut();s.task=Some(path.clone());s.page=0;s.page_starts=vec![0];s.conflict=None;s.control=None;
                         s.applying=false;s.planned=summary.planned_delete+summary.planned_move+summary.planned_link+summary.planned_empty;
                         s.archives_failed=summary.archives_failed;s.plan_filter.clone()};
                        ui.set_busy(false);ui.set_paused(false);ui.set_conflict_visible(false);ui.set_ready(ready);ui.set_has_task(true);
                        ui.set_summary(summary.description().into());ui.set_panel(1);
                        ui.set_archives_failed(summary.archives_failed as i32);
                        ui.set_plan_delete_count(summary.planned_delete as i32);
                        ui.set_plan_move_count(summary.planned_move as i32);
                        ui.set_plan_link_count(summary.planned_link as i32);
                        ui.set_plan_empty_count(summary.planned_empty as i32);
                        // 任务结束后停止实时计时，改写最终统计，避免「耗时」空闲继续增长
                        ui.set_metrics(format!("扫描 {} 个文件 · 解压成功 {} / 失败 {} · 错误 {} 项 · 已回收 {} 项",
                            summary.scanned,summary.archives_ok,summary.archives_failed,summary.errors,summary.recycled).into());
                        ui.set_progress(-1.0);ui.set_progress_note("".into());
                        ui.set_plan_prev_enabled(false);ui.set_plan_next_enabled(false);
                        ui.set_status(if ready{
                            "解压与分析完成。请检查计划，然后确认执行整理。".into()
                        }else{
                            format!("整理结束：已回收 {} 项 · 永久删除 {} 项 · 错误 {} 项；完整记录见「进度与日志」或导出报告。",
                                summary.recycled,summary.deleted,summary.errors)
                        }.into());
                        load_plan_filtered(sender.clone(),path,0,0,filter);
                        if state.borrow().close_after{let _=slint::quit_event_loop();}
                    }
                    Event::Failed(error)=>{
                        // 用户点了“取消任务”时不要用红色错误条报同一个消息：取消是预期操作，不是故障。
                        let cancelled={let s=state.borrow();s.control.as_ref().is_some_and(|control|control.is_cancelled())};
                        {let mut s=state.borrow_mut();s.control=None;s.conflict=None;s.logs.push_back(error.clone());log_changed=true;}
                        ui.set_busy(false);ui.set_ready(false);ui.set_paused(false);ui.set_conflict_visible(false);
                        if cancelled{
                            ui.set_notice_text(error.into());
                            ui.set_status("任务已取消；已完成的操作不会自动回滚，详情见任务记录".into());
                        }else{
                            ui.set_error_text(error.into());
                            ui.set_status("任务已停止；已完成的操作不会自动回滚，详情见任务记录".into());
                        }
                        ui.set_progress(-1.0);ui.set_progress_note("".into());
                        if state.borrow().close_after{let _=slint::quit_event_loop();}
                    }
                    Event::SelectionSaved(path,error)=>{
                        let mut s=state.borrow_mut();s.pending_selection=s.pending_selection.saturating_sub(1);
                        let failed=error.is_some();
                        if let Some(error)=error {ui.set_error_text(error.into());}
                        if s.task.as_ref()==Some(&path) && s.pending_selection==0 && !ui.get_busy(){
                            let same_config=Database::open(&path).and_then(|db|Ok(db.get::<String>("status")?=="ready" && serde_json::to_value(db.config()?)?==serde_json::to_value(&s.config)?)).unwrap_or(false);
                            ui.set_ready(!failed && same_config && Database::open(&path).and_then(|db|db.get::<String>("root")).is_ok_and(|root|std::fs::canonicalize(Path::new(ui.get_directory().as_str())).is_ok_and(|selected|selected==Path::new(&root))));
                        }
                    }
                    Event::PlanPage(path,mut actions,page)=>{
                        if state.borrow().task.as_ref()!=Some(&path){continue;}
                        let mut s=state.borrow_mut();
                        let more=actions.len()>100;actions.truncate(100);
                        if more&&s.page_starts.len()<=page+1{s.page_starts.push(actions.last().unwrap().id);}
                        s.page=page;
                        // 只有真的还有上一页/下一页时才让按钮可用，避免点了没有任何反应。
                        ui.set_plan_prev_enabled(page>0);
                        ui.set_plan_next_enabled(more);
                        ui.set_plan_page_label(format!("第 {} 页 · 每页最多 100 条",page+1).into());
                        let rows=actions.into_iter().map(|a|PlanRow{id:a.id.to_string().into(),selected:a.selected,
                            kind:match a.kind{ActionKind::Delete=>"删除",ActionKind::Move=>"移动/重命名",ActionKind::Hardlink=>"硬链接",ActionKind::EmptyDirectory=>"空目录复查"}.into(),
                            source:a.source.into(),target:a.target.unwrap_or_else(||a.keeper.as_ref().map(|v|format!("保留 {}",v.0)).unwrap_or_default()).into(),reason:a.reason.into(),
                            state:match a.state.as_str(){"pending"=>"待执行","done"=>"已执行","skipped"=>"已跳过","unselected"=>"已取消勾选","failed"=>"执行失败",other=>other}.into()}).collect::<Vec<_>>();
                        ui.set_plans(Rc::new(VecModel::from(rows)).into());
                    }
                    Event::History(items)=>{state.borrow_mut().history=items.iter().map(|(p,_)|p.clone()).collect();ui.set_history_items(Rc::new(VecModel::from(items.into_iter().map(|(_,s)|SharedString::from(s)).collect::<Vec<_>>())).into());},
                    Event::LoadedTask(path,root,cfg,summary,ready)=>{
                        if ui.get_busy() || state.borrow().pending_selection>0{continue;}
                        let filter={let mut s=state.borrow_mut();s.config=cfg;s.task=Some(path.clone());s.page=0;s.page_starts=vec![0];s.section="解压".into();
                         s.applying=false;s.planned=summary.planned_delete+summary.planned_move+summary.planned_link+summary.planned_empty;
                         s.archives_failed=summary.archives_failed;s.plan_filter=None;s.plan_filter.clone()};
                        ui.set_directory(platform::display_path_text(&root).into());ui.set_summary(summary.description().into());ui.set_ready(ready);ui.set_has_task(true);ui.set_screen(0);ui.set_panel(1);
                        ui.set_archives_failed(summary.archives_failed as i32);
                        ui.set_plan_delete_count(summary.planned_delete as i32);
                        ui.set_plan_move_count(summary.planned_move as i32);
                        ui.set_plan_link_count(summary.planned_link as i32);
                        ui.set_plan_empty_count(summary.planned_empty as i32);
                        ui.set_plan_filter(0);
                        refresh(&ui,&state.borrow());load_plan_filtered(sender.clone(),path,0,0,filter);
                    }
                    Event::ConfigLoaded(source,config)=>{state.borrow_mut().config=config;refresh(&ui,&state.borrow());invalidate(&ui);
                        // 导入成功也要有反馈：否则用户不知道文件究竟有没有生效（启动读取本机配置时不提示）。
                        if let Some(path)=source{ui.set_notice_text(format!("已导入规则：{}",path.display()).into());}},
                    Event::Notice(text)=>ui.set_notice_text(text.into()),
                    Event::Error(text)=>ui.set_error_text(text.into()),
                }
            }
            if log_changed{ui.set_log_text(state.borrow().logs.iter().rev().cloned().collect::<Vec<_>>().join("\n").into());}
            let busy=ui.get_busy();
            let s=state.borrow();
            // 只在真正运行时刷新实时指标；空闲时不碰 metrics，避免耗时一直涨
            if busy {
                if let Some(control)=&s.control{
                    let read=control.read_bytes.load(Ordering::Relaxed);let elapsed=s.started.elapsed().as_secs_f64().max(0.001);
                    let done=control.completed.load(Ordering::Relaxed);let scanned=control.scanned.load(Ordering::Relaxed);
                    // 执行阶段不再扫描，写“扫描 0 个文件”只会让人误以为没扫到东西，这里只报执行进度。
                    if s.applying{
                        ui.set_metrics(format!("执行中：已处理 {} / {} 项 · 耗时 {:.1}s",done,s.planned,elapsed).into());
                    }else{
                        ui.set_metrics(format!("扫描 {} 个文件 · 读取 {} · 已处理 {} 个计划项 · 耗时 {:.1}s · 平均读取 {:.1} MiB/s",scanned,bytes(read),done,elapsed,read as f64/elapsed/1048576.0).into());
                    }
                    // 执行阶段按计划项计数；分析阶段总量未知（扫描/哈希/解压包大小不能提前预知）
                    if s.applying&&s.planned>0{
                        ui.set_progress((done as f32/s.planned as f32).clamp(0.0,1.0));
                        ui.set_progress_note(format!("{done} / {} 项",s.planned).into());
                    }else{
                        ui.set_progress(-1.0);
                        ui.set_progress_note(format!("已扫描 {scanned} 个文件 · 读取 {}",bytes(read)).into());
                    }
                }else{
                    ui.set_progress(-1.0);ui.set_progress_note("准备中".into());
                }
            }
        });
    }
    // First show the window; reading a small user configuration happens afterwards on a worker.
    ui.show()?;
    center_window(&ui.window());
    // 窗口刚映射时系统还会套用默认位置，稍后再居中一次，保证首屏就是居中的
    let centered=ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(120),move||{
        if let Some(ui)=centered.upgrade(){center_window(&ui.window());}
    });
    async_work(sender.clone(),||{
        let path=config::state_dir()?.join("config.json");
        if path.try_exists()?{Ok(Event::ConfigLoaded(None,Config::load(&path)?))}else{Ok(Event::Status("请选择需要整理的目录".into()))}
    });
    slint::run_event_loop()?;
    Ok(())
}
fn main(){
    if let Err(error)=run(){
        let _=rfd::MessageDialog::new().set_title("JchTools 启动失败").set_description(format!("{error:#}\n\n可尝试设置 SLINT_BACKEND=winit-software 后启动。"))
            .set_level(rfd::MessageLevel::Error).show();
    }
}
