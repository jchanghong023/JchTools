#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
use anyhow::{Context as _, Result};
use jchtools::{config::{self,Config,ConflictPolicy}, control::{ConflictAnswer,Context,Control,Event},
    db::Database, engine, model::{bytes,ActionKind}, registry};
use serde::Deserialize;
use slint::{ComponentHandle,ModelRc,SharedString,VecModel};
use std::{cell::RefCell,collections::VecDeque,path::{Path,PathBuf},rc::Rc,
    sync::{atomic::Ordering,mpsc,Arc},time::{Duration,Instant}};
slint::include_modules!();

#[derive(Clone,Deserialize)]
struct RuleSpec {section:String,key:String,title:String,hint:String,kind:String,choices:Vec<Vec<String>>}
struct State {
    config:Config,specs:Vec<RuleSpec>,section:String,task:Option<PathBuf>,history:Vec<PathBuf>,
    control:Option<Arc<Control>>,conflict:Option<mpsc::SyncSender<ConflictAnswer>>,
    logs:VecDeque<String>,page:usize,page_starts:Vec<i64>,started:Instant,close_after:bool,pending_selection:usize,applying:bool,planned:u64,
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
fn rule_rows(state:&State)->Result<ModelRc<RuleRow>> {
    let data=serde_json::to_value(&state.config)?;
    let rows=state.specs.iter().filter(|s|s.section==state.section).map(|spec| {
        let value=&data[&spec.key];
        RuleRow {key:spec.key.clone().into(),title:spec.title.clone().into(),hint:spec.hint.clone().into(),
            kind:if spec.kind=="bool"{0}else if spec.kind=="choice"{1}else{2},
            checked:value.as_bool().unwrap_or(false),
            index:spec.choices.iter().position(|c|Some(c[0].as_str())==value.as_str()).unwrap_or(0) as i32,
            options:Rc::new(VecModel::from(spec.choices.iter().map(|c|SharedString::from(c[1].as_str())).collect::<Vec<_>>())).into(),
            value:value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string()).into(),}
    }).collect::<Vec<_>>();
    Ok(Rc::new(VecModel::from(rows)).into())
}
fn refresh(ui:&AppWindow,state:&State) {
    if let Ok(rows)=rule_rows(state){ui.set_rules(rows);}
    ui.set_theme(match state.config.theme.as_str(){"light"=>1,"dark"=>2,_=>0});
}
fn invalidate(ui:&AppWindow){
    ui.set_ready(false);
    if ui.get_has_task(){ui.set_status("规则或目录已改变，请重新解压与分析后再执行".into());}
}
fn changed(ui:&AppWindow,state:&Rc<RefCell<State>>,key:&str,value:serde_json::Value,rebuild:bool){
    let mut state=state.borrow_mut();
    match state.config.set_json(key,value){
        Ok(())=>{ui.set_error_text("".into());invalidate(ui);if rebuild{refresh(ui,&state);}},
        Err(error)=>ui.set_error_text(format!("{error:#}").into()),
    }
}
fn async_work(sender:mpsc::SyncSender<Event>,work:impl FnOnce()->Result<Event>+Send+'static){
    std::thread::spawn(move||{
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
        let event=match result{Ok(Ok(event))=>event,Ok(Err(error))=>Event::Notice(format!("{error:#}")),
            Err(_)=>Event::Notice("后台操作意外退出；请查看记录，未执行的步骤不会继续".into())};
        let _=sender.send(event);
    });
}
fn load_plan(sender:mpsc::SyncSender<Event>,path:PathBuf,start:i64,page:usize){
    async_work(sender,move||Ok(Event::PlanPage(path.clone(),Database::open(&path)?.actions_page(start,101)?,page)));
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
    ui.set_busy(true);ui.set_ready(false);ui.set_paused(false);ui.set_error_text("".into());ui.set_panel(2);
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
fn run()->Result<()> {
    let ui=AppWindow::new()?;
    ui.set_system_dark(system_dark());
    let specs:Vec<RuleSpec>=serde_json::from_str(include_str!("../resources/rules.json"))?;
    let state=Rc::new(RefCell::new(State{config:Config::default(),specs,section:"解压".into(),task:None,history:Vec::new(),
        control:None,conflict:None,logs:VecDeque::new(),page:0,page_starts:vec![0],started:Instant::now(),close_after:false,pending_selection:0,applying:false,planned:0}));
    let(sender,receiver)=mpsc::sync_channel::<Event>(256);
    let tools=registry::tools().iter().map(|tool|ToolRow{id:tool.id.into(),name:tool.name.into(),summary:tool.summary.into()}).collect::<Vec<_>>();
    ui.set_tool_count(registry::tools().len() as i32);
    ui.set_tools(Rc::new(VecModel::from(tools)).into());refresh(&ui,&state.borrow());
    {
        let weak=ui.as_weak();
        ui.on_choose_directory(move||{if let Some(ui)=weak.upgrade(){
            if let Some(path)=rfd::FileDialog::new().set_title("选择需要整理的目录").pick_folder(){
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
    {let weak=ui.as_weak();let state=state.clone();ui.on_rule_bool(move|key,value|{
        if let Some(ui)=weak.upgrade(){changed(&ui,&state,key.as_str(),value.into(),true);}
    });}
    {
        let weak=ui.as_weak();let state=state.clone();
        ui.on_rule_choice(move|key,index|{if let Some(ui)=weak.upgrade(){
            let value=state.borrow().specs.iter().find(|s|s.key==key.as_str()).and_then(|s|s.choices.get(index.max(0)as usize)).map(|c|c[0].clone());
            if let Some(value)=value{changed(&ui,&state,key.as_str(),value.into(),true);}
        }});
    }
    {
        let weak=ui.as_weak();let state=state.clone();
        ui.on_rule_text(move|key,value|{if let Some(ui)=weak.upgrade(){
            let numeric=state.borrow().specs.iter().find(|s|s.key==key.as_str()).is_some_and(|s|s.kind=="number");
            let parsed=if numeric{match value.parse::<u64>(){Ok(v)=>serde_json::Value::from(v),Err(_)=>{show_error(&ui,"该设置需要输入非负整数");return;}}}
                else{serde_json::Value::from(value.to_string())};
            changed(&ui,&state,key.as_str(),parsed,false);
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
            if let Some(start)=state.page_starts.get(next).copied(){state.page=next;load_plan(sender.clone(),task,start,next);}
        });
    }
    {
        let state=state.clone();let weak=ui.as_weak();
        ui.on_open_report(move||{if let Some(path)=&state.borrow().task{if let Err(error)=open::that(path){if let Some(ui)=weak.upgrade(){show_error(&ui,error);}}}});
    }
    {
        let state=state.clone();let sender=sender.clone();
        ui.on_export_report(move||{let task=state.borrow().task.clone();if let Some(task)=task{
            if let Some(path)=rfd::FileDialog::new().set_file_name("整理报告.csv").add_filter("CSV",&["csv"]).save_file(){
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
                1=>rfd::FileDialog::new().set_file_name("jchtools-rules.json").save_file(),
                2=>rfd::FileDialog::new().add_filter("JSON",&["json"]).pick_file(),
                _=>{state.borrow_mut().config=Config::default();if let Some(ui)=weak.upgrade(){refresh(&ui,&state.borrow());invalidate(&ui);}return;}
            };
            if let Some(path)=path{async_work(sender.clone(),move||{
                if kind==2{Ok(Event::ConfigLoaded(Config::load(&path)?))}else{cfg.save(&path)?;Ok(Event::Notice(format!("规则已保存：{}",path.display())))}
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
                        ui.set_conflict_text(format!("现有文件：{}\n大小：{}；修改时间(ns)：{}\n\n新解压文件：{}\n大小：{}；修改时间(ns)：{}\n\n覆盖旧文件仍遵守已配置的回收站 / 永久删除策略。",info.existing,bytes(info.existing_size),info.existing_time,info.incoming,bytes(info.incoming_size),info.incoming_time).into());
                        ui.set_conflict_choice(4);ui.set_conflict_all(false);ui.set_conflict_visible(true);
                    }
                    Event::Ready(path,summary)|Event::Done(path,summary)=>{
                        let ready=Database::open(&path).and_then(|db|db.get::<String>("status")).is_ok_and(|v|v=="ready");
                        {let mut s=state.borrow_mut();s.task=Some(path.clone());s.page=0;s.page_starts=vec![0];s.conflict=None;
                         s.applying=false;s.planned=summary.planned_delete+summary.planned_move+summary.planned_link+summary.planned_empty;}
                        ui.set_busy(false);ui.set_paused(false);ui.set_conflict_visible(false);ui.set_ready(ready);ui.set_has_task(true);
                        ui.set_summary(summary.description().into());ui.set_panel(1);
                        ui.set_status(if ready{"解压与分析完成。请检查计划，然后确认执行整理。"}else{"整理结束；请查看成功、跳过及失败记录。"}.into());
                        load_plan(sender.clone(),path,0,0);
                        if state.borrow().close_after{let _=slint::quit_event_loop();}
                    }
                    Event::Failed(error)=>{
                        ui.set_busy(false);ui.set_ready(false);ui.set_paused(false);ui.set_conflict_visible(false);
                        ui.set_error_text(error.clone().into());ui.set_status("任务已停止；已完成的操作不会自动回滚，详情见任务记录".into());
                        let mut s=state.borrow_mut();s.logs.push_back(error);s.conflict=None;log_changed=true;
                        if s.close_after{let _=slint::quit_event_loop();}
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
                        ui.set_plan_page_label(format!("第 {} 页 · 每页最多 100 条",page+1).into());
                        let rows=actions.into_iter().map(|a|PlanRow{id:a.id.to_string().into(),selected:a.selected,
                            kind:match a.kind{ActionKind::Delete=>"删除",ActionKind::Move=>"移动/重命名",ActionKind::Hardlink=>"硬链接",ActionKind::EmptyDirectory=>"空目录复查"}.into(),
                            source:a.source.into(),target:a.target.unwrap_or_else(||a.keeper.as_ref().map(|v|format!("保留 {}",v.0)).unwrap_or_default()).into(),reason:a.reason.into(),state:a.state.into()}).collect::<Vec<_>>();
                        ui.set_plans(Rc::new(VecModel::from(rows)).into());
                    }
                    Event::History(items)=>{state.borrow_mut().history=items.iter().map(|(p,_)|p.clone()).collect();ui.set_history_items(Rc::new(VecModel::from(items.into_iter().map(|(_,s)|SharedString::from(s)).collect::<Vec<_>>())).into());},
                    Event::LoadedTask(path,root,cfg,summary,ready)=>{
                        if ui.get_busy() || state.borrow().pending_selection>0{continue;}
                        {let mut s=state.borrow_mut();s.config=cfg;s.task=Some(path.clone());s.page=0;s.page_starts=vec![0];s.section="解压".into();
                         s.applying=false;s.planned=summary.planned_delete+summary.planned_move+summary.planned_link+summary.planned_empty;}
                        ui.set_directory(root.into());ui.set_summary(summary.description().into());ui.set_ready(ready);ui.set_has_task(true);ui.set_screen(0);ui.set_panel(1);refresh(&ui,&state.borrow());load_plan(sender.clone(),path,0,0);
                    }
                    Event::ConfigLoaded(config)=>{state.borrow_mut().config=config;refresh(&ui,&state.borrow());invalidate(&ui);},
                    Event::Notice(text)=>ui.set_error_text(text.into()),
                }
            }
            if log_changed{ui.set_log_text(state.borrow().logs.iter().cloned().collect::<Vec<_>>().join("\n").into());}
            let busy=ui.get_busy();
            let s=state.borrow();
            if let Some(control)=&s.control{
                let read=control.read_bytes.load(Ordering::Relaxed);let elapsed=s.started.elapsed().as_secs_f64().max(0.001);
                let done=control.completed.load(Ordering::Relaxed);let scanned=control.scanned.load(Ordering::Relaxed);
                ui.set_metrics(format!("扫描 {} 个文件 · 读取 {} · 已处理 {} 个计划项 · 耗时 {:.1}s · 平均读取 {:.1} MiB/s",scanned,bytes(read),done,elapsed,read as f64/elapsed/1048576.0).into());
                if busy{
                    // 执行阶段按计划项计数；分析阶段总量未知（扫描/哈希/解压包大小不能提前预知）
                    if s.applying&&s.planned>0{
                        ui.set_progress((done as f32/s.planned as f32).clamp(0.0,1.0));
                        ui.set_progress_note(format!("{done} / {} 项",s.planned).into());
                    }else{
                        ui.set_progress(-1.0);
                        ui.set_progress_note(format!("已扫描 {scanned} 个文件 · 读取 {}",bytes(read)).into());
                    }
                }else{ui.set_progress(-1.0);ui.set_progress_note("".into());}
            }else if busy{ui.set_progress(-1.0);ui.set_progress_note("准备中".into());}
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
        if path.try_exists()?{Ok(Event::ConfigLoaded(Config::load(&path)?))}else{Ok(Event::Status("请选择需要整理的目录".into()))}
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
