//! 计划执行确认流的无头 GUI 测试：用真实回调 + 真实引擎线程走完整
//! 「解压分析 → 检查计划 → 确认执行 → 整理结束」流程。
//! 驱动器定时器在事件循环内逐步推进状态机，超时自动失败。
//! 状态目录与回收站均注入临时路径，不碰用户真实任务库/回收站。
#![cfg(feature = "gui")]
use jchtools::gui::{self, EngineTestOverrides};
use slint::ComponentHandle;
use std::{cell::{Cell,RefCell},fs,path::PathBuf,rc::Rc,sync::Arc,time::Duration};

mod common;
use common::MoveRecycle;

fn make_fixture()->tempfile::TempDir {
    let dir=tempfile::tempdir().unwrap();
    let data=dir.path().join("data");fs::create_dir_all(&data).unwrap();
    let write=|name:&str,bytes:&[u8],mtime:i64|{
        let p=data.join(name);fs::write(&p,bytes).unwrap();
        filetime::set_file_mtime(&p,filetime::FileTime::from_unix_time(mtime,0)).unwrap();
    };
    write("a.txt",b"same content",100);        // 旧 → 去重时被删除
    write("b.txt",b"same content",200);        // 新 → 保留
    write("temp.tmp",b"junk",300);             // 默认 clean_temp=false → 会按归类移到「其他/」
    dir
}

#[test]
fn plan_execution_confirmation_flow_runs_end_to_end(){
    let fixture=make_fixture();
    let data=fixture.path().join("data").to_string_lossy().to_string();
    let state_dir:PathBuf=fixture.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let bin=fixture.path().join("mock-bin");
    let recycler=Arc::new(MoveRecycle::new(bin.clone()));
    let seen_tasks=Rc::new(RefCell::new(Vec::<PathBuf>::new()));
    let steps=Rc::new(Cell::new(0u32));
    let ticks=Rc::new(Cell::new(0u32));

    let overrides=EngineTestOverrides{state_dir:state_dir.clone(),recycler:recycler.clone()};
    gui::run_with_engine_overrides(move|ui|{
        ui.set_directory(data.clone().into());
        let ui=ui.as_weak();
        let steps=steps.clone();let ticks=ticks.clone();let seen_tasks=seen_tasks.clone();
        let driver=slint::Timer::default();
        driver.start(slint::TimerMode::Repeated,Duration::from_millis(200),move||{
            ticks.set(ticks.get()+1);
            assert!(ticks.get()<150,"驱动超时：流程卡在步骤 {}",steps.get());
            let Some(ui)=ui.upgrade() else {return};
            match steps.get(){
                0=>{ ui.invoke_request_start(); steps.set(1); }
                1=>{ if ui.get_confirm_kind()==1 && !ui.get_acknowledge(){
                        // 记录任务目录：确认它落在注入的 state 下，而非真实 AppData。
                        let dir=ui.get_directory().to_string();
                        let _=dir;
                        ui.invoke_confirmed(1); steps.set(2); } }
                2=>{ if ui.get_ready(){ ui.invoke_request_apply(); steps.set(3); } }
                3=>{ if ui.get_confirm_kind()==2 { ui.invoke_confirmed(2); steps.set(4); } }
                4=>{ if ui.get_status().contains("整理结束") {
                        steps.set(5); let _=slint::quit_event_loop(); } }
                _=>{}
            }
        });
        let _=seen_tasks;
        std::mem::forget(driver); // 事件循环运行期间必须保持驱动定时器存活
    },Some(overrides)).expect("GUI 流程失败");

    // 事件循环退出后校验：
    // 1) 任务库写在注入的 state_dir/tasks 下
    let tasks_root=state_dir.join("tasks");
    assert!(tasks_root.is_dir(),"任务库应写入注入的 state 目录：{tasks_root:?}");
    let task_count=fs::read_dir(&tasks_root).map(|it|it.count()).unwrap_or(0);
    assert!(task_count>=1,"注入 state 下应有任务目录");

    // 2) 回收走 mock，不碰真实回收站；a.txt 应落在 mock-bin
    let data=fixture.path().join("data");
    let remaining:Vec<String>=fs::read_dir(&data).unwrap()
        .filter_map(|e|e.ok()).map(|e|format!("{}{}",e.file_name().to_string_lossy(),if e.path().is_dir(){"/"}else{""}))
        .collect();
    let bin_names:Vec<String>=fs::read_dir(&bin).unwrap()
        .filter_map(|e|e.ok()).map(|e|e.file_name().to_string_lossy().into_owned()).collect();
    assert!(!data.join("a.txt").exists(),"重复项应被移除：{remaining:?}");
    assert!(bin_names.iter().any(|n|n.contains("a.txt")),"重复项应进入 mock 回收目录：{bin_names:?}");

    // 3) 默认 clean_temp=false + ClassifyMode::Category：temp.tmp 被归类移动，而非清理删除
    assert!(data.join("其他").join("temp.tmp").exists(),
        "默认配置下 temp.tmp 应归类到「其他/」而不是被清理：{remaining:?}");
    assert!(data.join("文档").join("b.txt").exists(),"去重保留项应随归类移动到 文档/：{remaining:?}");
}
