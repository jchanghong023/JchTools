//! 计划执行确认流的无头 GUI 测试：用真实回调 + 真实引擎线程走完整
//! 「分析（只读）→ 检查计划 → 确认执行 → 整理结束」流程。
//! 驱动器定时器在事件循环内逐步推进状态机，超时自动失败。
//! 状态目录与回收站均注入临时路径，不碰用户真实任务库/回收站。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![cfg(feature = "gui")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools::gui::{self, EngineTestOverrides};
use slint::ComponentHandle;
use std::{
    cell::{Cell, RefCell},
    fs,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

mod common;
use common::MoveRecycle;

fn make_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let write = |name: &str, bytes: &[u8], mtime: i64| {
        let p = data.join(name);
        fs::write(&p, bytes).unwrap();
        filetime::set_file_mtime(&p, filetime::FileTime::from_unix_time(mtime, 0)).unwrap();
    };
    write("a.txt", b"same content", 100); // 旧 → 去重时被删除
    write("b.txt", b"same content", 200); // 新 → 保留
    write("temp.tmp", b"junk", 300); // 默认 clean_temp=false → 会按归类移到「其他/」
    dir
}

// 覆盖 C-01, C-05, S-02（两段式确认执行全流程 + 回收走注入实现）
#[test]
fn plan_execution_confirmation_flow_runs_end_to_end() {
    // SLINT_BACKEND 是进程级环境变量；后续在同文件新增 GUI 测试时必须先拿到这把锁，
    // 避免并行线程在彼此的事件循环启动后改写渲染后端。
    static GUI_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = GUI_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // CI runner 与无 GPU 机器没有 OpenGL，默认 femtovg 初始化直接失败；
    // 软件渲染器不依赖 GPU，事件循环、定时器与回调路径仍与生产完全一致。
    std::env::set_var("SLINT_BACKEND", "winit-software");
    let fixture = make_fixture();
    let data = fixture.path().join("data").to_string_lossy().to_string();
    let state_dir: PathBuf = fixture.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let bin = fixture.path().join("mock-bin");
    let recycler = Arc::new(MoveRecycle::new(bin.clone()));
    let seen_tasks = Rc::new(RefCell::new(Vec::<PathBuf>::new()));
    let steps = Rc::new(Cell::new(0u32));
    let ticks = Rc::new(Cell::new(0u32));

    let overrides = EngineTestOverrides {
        state_dir: state_dir.clone(),
        recycler: recycler.clone(),
    };
    gui::run_with_engine_overrides(
        move |ui| {
            // 启动页是注册表第一个工具（递归解压）；本用例驱动目录整理，先切工具。
            ui.invoke_select_tool("directory-organizer".into());
            ui.set_directory(data.clone().into());
            let ui = ui.as_weak();
            let steps = steps.clone();
            let ticks = ticks.clone();
            let seen_tasks = seen_tasks.clone();
            let driver = slint::Timer::default();
            driver.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(200),
                move || {
                    ticks.set(ticks.get() + 1);
                    assert!(ticks.get() < 150, "驱动超时：流程卡在步骤 {}", steps.get());
                    let Some(ui) = ui.upgrade() else { return };
                    match steps.get() {
                        // C-01：分析只读，不再弹破坏性确认框——点「开始分析」直接进入分析。
                        0 => {
                            ui.invoke_request_start();
                            steps.set(1);
                        }
                        1 => {
                            if ui.get_ready() {
                                ui.invoke_request_apply();
                                steps.set(2);
                            }
                        }
                        2 => {
                            if ui.get_confirm_kind() == 2 {
                                ui.invoke_confirmed(2);
                                steps.set(3);
                            }
                        }
                        3 if ui.get_status().contains("整理结束") => {
                            steps.set(4);
                            let _ = slint::quit_event_loop();
                        }
                        _ => {}
                    }
                },
            );
            let _ = seen_tasks;
            // 有意泄漏（与 mem::forget 同义但走惯用 API）：事件循环运行期间必须保持驱动定时器存活，
            // 不得让 Timer 在闭包结束时 Drop 停摆；泄漏量恒为一个 Timer，进程随即退出。
            let _driver_leaked: &'static mut slint::Timer = Box::leak(Box::new(driver));
        },
        Some(overrides),
    )
    .expect("GUI 流程失败");

    // 事件循环退出后校验：
    // 1) 任务库写在注入的 state_dir/tasks 下
    let tasks_root = state_dir.join("tasks");
    assert!(
        tasks_root.is_dir(),
        "任务库应写入注入的 state 目录：{tasks_root:?}"
    );
    let task_count = fs::read_dir(&tasks_root).map_or(0, std::iter::Iterator::count);
    assert!(task_count >= 1, "注入 state 下应有任务目录");

    // 2) 回收走 mock，不碰真实回收站；a.txt 应落在 mock-bin
    let data = fixture.path().join("data");
    let remaining: Vec<String> = fs::read_dir(&data)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| {
            format!(
                "{}{}",
                e.file_name().to_string_lossy(),
                if e.path().is_dir() { "/" } else { "" }
            )
        })
        .collect();
    let bin_names: Vec<String> = fs::read_dir(&bin)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !data.join("a.txt").exists(),
        "重复项应被移除：{remaining:?}"
    );
    assert!(
        bin_names.iter().any(|n| n.contains("a.txt")),
        "重复项应进入 mock 回收目录：{bin_names:?}"
    );

    // 3) 默认 clean_temp=false + ClassifyMode::Category：temp.tmp 被归类移动，而非清理删除
    assert!(
        data.join("其他").join("temp.tmp").exists(),
        "默认配置下 temp.tmp 应归类到「其他/」而不是被清理：{remaining:?}"
    );
    assert!(
        data.join("文档").join("b.txt").exists(),
        "去重保留项应随归类移动到 文档/：{remaining:?}"
    );
}

// 覆盖 C-01（第二段确认的「我已确认」门禁）。该门禁是 Slint 声明式绑定，
// 无头测试只能直接调用回调、绕不过它，因此这里锁定声明本身不被误删/改弱。
#[test]
fn acknowledge_gate_is_declared_in_ui() {
    let ui = include_str!("../ui/app.slint");
    assert!(
        ui.contains("enabled: root.confirm-kind == 3 || root.acknowledge;"),
        "确认按钮的「我已确认」门禁声明缺失或被改动（C-07）"
    );
}
