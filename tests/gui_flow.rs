//! 计划执行确认流的无头 GUI 测试：用真实回调 + 真实引擎线程走完整
//! 「解压分析 → 检查计划 → 确认执行 → 整理结束」流程。
//! 驱动器定时器在事件循环内逐步推进状态机，超时自动失败。
use jchtools::gui;
use slint::ComponentHandle;
use std::{cell::Cell,fs,rc::Rc,time::{Duration,Instant}};

fn make_fixture()->tempfile::TempDir {
    let dir=tempfile::tempdir().unwrap();
    let data=dir.path().join("data");fs::create_dir_all(&data).unwrap();
    let write=|name:&str,bytes:&[u8],mtime:i64|{
        let p=data.join(name);fs::write(&p,bytes).unwrap();
        filetime::set_file_mtime(&p,filetime::FileTime::from_unix_time(mtime,0)).unwrap();
    };
    write("a.txt",b"same content",100);        // 旧 → 去重时被删除
    write("b.txt",b"same content",200);        // 新 → 保留
    write("temp.tmp",b"junk",300);             // 临时文件 → 清理 1 项
    dir
}

#[test]
fn plan_execution_confirmation_flow_runs_end_to_end(){
    let fixture=make_fixture();
    let data=fixture.path().join("data").to_string_lossy().to_string();
    let steps=Rc::new(Cell::new(0u32));
    let ticks=Rc::new(Cell::new(0u32));

    gui::run_with_pre_loop_hook(move|ui|{
        ui.set_directory(data.clone().into());
        let ui=ui.as_weak();
        let steps=steps.clone();let ticks=ticks.clone();
        let driver=slint::Timer::default();
        driver.start(slint::TimerMode::Repeated,Duration::from_millis(200),move||{
            ticks.set(ticks.get()+1);
            assert!(ticks.get()<150,"驱动超时：流程卡在步骤 {}",steps.get());
            let Some(ui)=ui.upgrade() else {return};
            match steps.get(){
                0=>{ ui.invoke_request_start(); steps.set(1); }
                1=>{ if ui.get_confirm_kind()==1 && !ui.get_acknowledge(){
                        ui.invoke_confirmed(1); steps.set(2); } }
                2=>{ if ui.get_ready(){ ui.invoke_request_apply(); steps.set(3); } }
                3=>{ if ui.get_confirm_kind()==2 { ui.invoke_confirmed(2); steps.set(4); } }
                4=>{ if ui.get_status().contains("整理结束") {
                        steps.set(5); let _=slint::quit_event_loop(); } }
                _=>{}
            }
        });
        std::mem::forget(driver); // 事件循环运行期间必须保持驱动定时器存活
    }).expect("GUI 流程失败");

    // 事件循环退出后校验文件系统最终状态：
    // b.txt（较新）保留；a.txt（同内容重复）与 temp.tmp（临时文件）按默认回收站策略移除
    let data=fixture.path().join("data");
    let remaining:Vec<String>=fs::read_dir(&data).unwrap()
        .filter_map(|e|e.ok()).map(|e|format!("{}{}",e.file_name().to_string_lossy(),if e.path().is_dir(){"/"}else{""}))
        .collect();
    // GUI 默认开启归类：去重保留项 b.txt 应随分类移入 文档/，重复项与临时文件被移除
    assert!(data.join("文档").join("b.txt").exists(),"去重保留项应随归类移动到 文档/ 下：{remaining:?}");
    assert!(!data.join("a.txt").exists(),"重复项应被移除");
    assert!(!data.join("temp.tmp").exists(),"临时文件应被清理");
}
