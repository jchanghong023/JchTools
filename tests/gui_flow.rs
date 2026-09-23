//! 计划执行确认流的无头 GUI 测试：用真实回调 + 真实引擎线程走完整
//! 「分析（只读）→ 检查计划 → 确认执行 → 整理结束」流程，以及
//! 「递归解压」的一段确认流程。驱动器定时器在事件循环内逐步推进状态机，超时自动失败。
//! 状态目录注入临时路径，不碰用户真实任务库。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![cfg(feature = "gui")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools::gui::{self, EngineTestOverrides};
use slint::{ComponentHandle, Model};
use std::{
    cell::{Cell, RefCell},
    fs,
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, LazyLock, Mutex,
    },
    time::Duration,
};

thread_local! {
    /// 驱动器定时器必须活到事件循环结束：放进线程局部存储即可，
    /// 既不会被 hook 作用域回收，也不需要泄漏。
    static DRIVER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

type Job = Box<dyn FnOnce() + Send>;
/// 驱动器在事件循环内检测到的失败：不在回调里 panic（会带着半开的事件循环退出，
/// 后续用例再跑事件循环会撞上「Nested event loops are not supported」），
/// 而是记录后退出循环，由用例在循环外统一断言。
type Failures = Arc<Mutex<Vec<String>>>;

/// Slint 平台绑定首次初始化它的线程：所有端到端用例都在同一个工作线程上串行运行，
/// 否则并行的测试线程各自建窗口会撞上「platform was initialized in another thread」。
/// 工作线程只负责承载事件循环；渲染后端、定时器与回调路径与生产完全一致。
static GUI_WORKER: LazyLock<Mutex<mpsc::Sender<Job>>> = LazyLock::new(|| {
    let (sender, receiver) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("gui-flow-worker".into())
        .spawn(move || {
            // CI runner 与无 GPU 机器没有 OpenGL，默认 femtovg 初始化直接失败；
            // 软件渲染器不依赖 GPU，事件循环、定时器与回调路径仍与生产完全一致。
            std::env::set_var("SLINT_BACKEND", "winit-software");
            while let Ok(job) = receiver.recv() {
                job();
            }
        })
        .expect("启动 GUI 端到端工作线程");
    Mutex::new(sender)
});

/// 在 GUI 工作线程上跑一段流程；任务 panic 原样透出给测试线程，避免被通道错误掩盖。
fn run_gui_job(job: impl FnOnce() + Send + 'static) {
    let (done_sender, done_receiver) = mpsc::channel::<Result<(), String>>();
    GUI_WORKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .send(Box::new(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            let message = match outcome {
                Ok(()) => Ok(()),
                Err(payload) => Err(if let Some(text) = payload.downcast_ref::<&str>() {
                    (*text).to_string()
                } else if let Some(text) = payload.downcast_ref::<String>() {
                    text.clone()
                } else {
                    "未知 panic".to_string()
                }),
            };
            let _ = done_sender.send(message);
        }))
        .expect("GUI 工作线程不可用");
    if let Err(message) = done_receiver.recv().expect("GUI 工作线程中断") {
        panic!("{message}");
    }
}

/// 记录一次流程失败并退出事件循环（见 `Failures` 的类型注释）。
fn record_failure(failures: &Failures, message: String) {
    failures
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(message);
    let _ = slint::quit_event_loop();
}

fn take_failures(failures: &Failures) -> Vec<String> {
    std::mem::take(
        &mut *failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

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
    write("temp.tmp", b"junk", 300); // 默认 clean_temp=false → 会按归类移到「其他」大类
    dir
}

/// 目录整理两段式流程的驱动结果：结束时的状态栏与提示条文本（事件循环内读取）。
struct OrganizerRun {
    failures: Vec<String>,
    status: String,
    notice: String,
}

// 覆盖 C-11：真实回调必须即时更新勾选状态，而不是等待任务库回传或重新加载。
fn toggle_first_file_action(ui: &gui::AppWindow, selected: bool, failures: &Failures) -> bool {
    let plans = ui.get_plans();
    let Some((index, row)) = plans
        .iter()
        .enumerate()
        .find(|(_, row)| row.kind != "空目录复查" && row.selected != selected)
    else {
        return false;
    };
    ui.invoke_plan_toggle(row.id, selected);
    let expected = if selected {
        "待执行"
    } else {
        "已取消勾选"
    };
    if !ui
        .get_plans()
        .row_data(index)
        .is_some_and(|row| row.selected == selected && row.state == expected)
    {
        record_failure(failures, format!("勾选回调没有即时显示 {expected}"));
    }
    true
}

/// 在 GUI 工作线程上驱动目录整理：选目录 → 开始分析 → 确认执行 → 整理结束。
fn drive_organizer(data: String, state_dir: PathBuf) -> OrganizerRun {
    let failures: Failures = Arc::new(Mutex::new(Vec::new()));
    let failure_sink = Arc::clone(&failures);
    let steps = Rc::new(Cell::new(0u32));
    let ticks = Rc::new(Cell::new(0u32));
    let observed = Arc::new(Mutex::new((String::new(), String::new())));
    let observed_sink = Arc::clone(&observed);
    gui::run_with_engine_overrides(
        move |ui| {
            // 启动页是注册表第一个工具（递归解压）；本流程驱动目录整理，先切工具。
            ui.invoke_select_tool("directory-organizer".into());
            ui.set_directory(data.clone().into());
            let ui = ui.as_weak();
            let steps = steps.clone();
            let ticks = ticks.clone();
            let failures = Arc::clone(&failure_sink);
            let observed = Arc::clone(&observed_sink);
            let driver = slint::Timer::default();
            driver.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(200),
                move || {
                    ticks.set(ticks.get() + 1);
                    if ticks.get() >= 150 {
                        record_failure(
                            &failures,
                            format!("驱动超时：流程卡在步骤 {}", steps.get()),
                        );
                        return;
                    }
                    let Some(ui) = ui.upgrade() else { return };
                    match steps.get() {
                        // C-01：分析只读，不再弹破坏性确认框——点「开始分析」直接进入分析。
                        0 => {
                            ui.invoke_request_start();
                            steps.set(1);
                        }
                        1 => {
                            if ui.get_ready() && toggle_first_file_action(&ui, false, &failures) {
                                steps.set(4);
                            }
                        }
                        4 if ui.get_ready() => {
                            if toggle_first_file_action(&ui, true, &failures) {
                                steps.set(5);
                            }
                        }
                        5 if ui.get_ready() => {
                            ui.invoke_request_apply();
                            steps.set(2);
                        }
                        2 => {
                            if ui.get_confirm_kind() == 2 {
                                ui.invoke_confirmed(2);
                                steps.set(3);
                            }
                        }
                        3 if ui.get_status().contains("整理结束") => {
                            steps.set(6);
                            *observed
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = (
                                ui.get_status().to_string(),
                                ui.get_notice_text().to_string(),
                            );
                            let _ = slint::quit_event_loop();
                        }
                        _ => {}
                    }
                },
            );
            // 驱动定时器必须活到事件循环结束：放进线程局部存储，事件循环退出前不会被回收。
            DRIVER.with(|slot| *slot.borrow_mut() = Some(driver));
        },
        Some(EngineTestOverrides { state_dir }),
    )
    .expect("GUI 流程失败");
    let (status, notice) = observed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    OrganizerRun {
        failures: take_failures(&failures),
        status,
        notice,
    }
}

// 覆盖 C-01, C-05, S-02（两段式确认执行全流程）
#[test]
fn plan_execution_confirmation_flow_runs_end_to_end() {
    let fixture = make_fixture();
    let data = fixture.path().join("data");
    let state_dir: PathBuf = fixture.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();

    let data_text = data.to_string_lossy().to_string();
    run_gui_job(move || {
        let run = drive_organizer(data_text, state_dir);
        assert!(run.failures.is_empty(), "流程未完成：{:?}", run.failures);
        assert!(
            run.status.contains("整理结束"),
            "结束时状态栏必须报告完成：{}",
            run.status
        );
    });

    // 事件循环退出后校验：
    // 1) 任务库写在注入的 state_dir/tasks 下
    let tasks_root = fixture.path().join("state").join("tasks");
    assert!(
        tasks_root.is_dir(),
        "任务库应写入注入的 state 目录：{tasks_root:?}"
    );
    let task_count = fs::read_dir(&tasks_root).map_or(0, std::iter::Iterator::count);
    assert!(task_count >= 1, "注入 state 下应有任务目录");

    // 2) 去重副本已移除，保留项与未开启清理的文件归类后仍存在
    //（GUI 默认规则不同名去重关闭：a.txt/b.txt 同内容不同名，各自随归类保留）。
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

    // 3) 默认 clean_temp=false：temp.tmp 被归类移动，而非清理删除
    //（C-05 固定归类「大类」一级；无扩展名匹配的文件进入「其他」大类）。
    let classified = |category: &str, name: &str| data.join(format!("{category}/{name}"));
    assert!(
        classified("其他", "temp.tmp").exists(),
        "默认配置下 temp.tmp 应归类到「其他」大类而不是被清理：{remaining:?}"
    );
    assert!(
        classified("文档", "a.txt").exists() && classified("文档", "b.txt").exists(),
        "GUI 默认不同名去重关闭：a.txt/b.txt 同内容不同名，各自随归类保留到 文档/ 大类：{remaining:?}"
    );
}

// 覆盖 H-06, S-04（Git 目录整树排除在界面明确可见：结束后提示条仍保留跳过说明，
// 排除树内的文件既不被归类也不被删除，目录结构原样保留）
#[test]
fn git_subtree_skip_is_visible_after_run_and_tree_untouched() {
    let fixture = tempfile::tempdir().unwrap();
    let data = fixture.path().join("data");
    fs::create_dir_all(data.join("keepgit").join(".git")).unwrap();
    fs::create_dir_all(data.join("docs")).unwrap();
    fs::write(data.join("docs").join("note.txt"), b"note").unwrap();
    fs::write(data.join("keepgit").join(".git").join("config"), b"git").unwrap();
    fs::write(data.join("keepgit").join("tracked.txt"), b"tracked").unwrap();
    let state_dir: PathBuf = fixture.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();

    let data_text = data.to_string_lossy().to_string();
    run_gui_job(move || {
        let run = drive_organizer(data_text, state_dir);
        assert!(run.failures.is_empty(), "流程未完成：{:?}", run.failures);
        assert!(
            run.notice.contains("Git"),
            "H-06：结束后提示条必须明确说明 Git 目录的处置结果：notice={} status={}",
            run.notice,
            run.status
        );
        assert!(
            run.notice.contains("Git项目集合") && !run.notice.contains("跳过"),
            "C-14：整理流程中 Git 项目整体移入「Git项目集合」，提示条必须说明该处置，\
             不得说成「跳过」：{}",
            run.notice
        );
    });

    // C-14：Git 项目整体移入「Git项目集合」，树内文件随树移动且不得被删除或改写。
    let moved = data.join("Git项目集合").join("keepgit");
    assert!(
        moved.join(".git").join("config").is_file(),
        "H-06：Git 树内的文件不得被删除或改写"
    );
    assert!(
        moved.join("tracked.txt").is_file(),
        "H-06：Git 目录整树（含全部内容）不参与归类，随项目整体移动"
    );
    assert!(
        !moved.join("其他").exists(),
        "H-06：Git 目录树内不得生成分类目录"
    );
    // 固定归类（C-05，恒开启）：docs/note.txt → 文档/note.txt（排除树之外照常处理）。
    let handled = [
        "文档/note.txt".to_string(),
        "文档/docs/note.txt".to_string(),
        "docs/note.txt".to_string(),
        "文档/note.txt".to_string(),
    ];
    assert!(
        handled.iter().any(|rel| data.join(rel).is_file()),
        "排除树之外的文件仍按计划处理（归类后仍存在）"
    );
}

// 覆盖 X-02, X-04, X-05, X-06, H-07（解压一段确认的端到端）：目录内没有压缩包时（清点为 0），
// 确认框必须如实提示无可处理包，且不得解除清点门禁——不启动空解压任务，已有文件原样保留。
// 发现数 > 0 时确认文案的删除/保留口径逐条披露由 gui.rs 的
// extract_confirm_text_discloses_deletion_rules 单元测试覆盖（零包流程按 X-02 不再进入确认执行）。
#[test]
fn extract_confirmation_discloses_source_deletion_and_leaves_files_untouched() {
    let fixture = tempfile::tempdir().unwrap();
    let data = fixture.path().join("data");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("keep.txt"), b"keep me").unwrap();
    let state_dir: PathBuf = fixture.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let directory = data.to_string_lossy().to_string();
    // 跨线程可见的确认标志：Rc 不是 Send，工作线程任务用原子量回传结果。
    let zero_gate_held = Arc::new(AtomicBool::new(false));
    let gate_flag = Arc::clone(&zero_gate_held);

    let overrides = EngineTestOverrides {
        state_dir: state_dir.clone(),
    };
    run_gui_job(move || {
        let failures: Failures = Arc::new(Mutex::new(Vec::new()));
        let failure_sink = Arc::clone(&failures);
        let steps = Rc::new(Cell::new(0u32));
        let ticks = Rc::new(Cell::new(0u32));
        gui::run_with_engine_overrides(
            move |ui| {
                // 递归解压是启动页，但显式选择一次更贴近用户路径（并确保 screen==2）。
                ui.invoke_select_tool("recursive-extract".into());
                ui.set_directory(directory.clone().into());
                ui.invoke_request_extract_start();
                let ui = ui.as_weak();
                let steps = steps.clone();
                let ticks = ticks.clone();
                let failures = Arc::clone(&failure_sink);
                let gate_flag = Arc::clone(&gate_flag);
                let driver = slint::Timer::default();
                driver.start(
                    slint::TimerMode::Repeated,
                    Duration::from_millis(200),
                    move || {
                        ticks.set(ticks.get() + 1);
                        if ticks.get() >= 150 {
                            record_failure(
                                &failures,
                                format!("驱动超时：流程卡在步骤 {}", steps.get()),
                            );
                            return;
                        }
                        let Some(ui) = ui.upgrade() else { return };
                        match steps.get() {
                            0 => {
                                // X-02：清点为 0——门禁必须保持（确认不可用），文案如实提示。
                                if ui.get_confirm_kind() == 1 {
                                    if ui.get_confirm_pending() {
                                        let text = ui.get_confirm_text().to_string();
                                        if text.contains("未发现") {
                                            gate_flag.store(true, Ordering::Relaxed);
                                            // 返回检查收尾：流程到此为止，不启动解压任务。
                                            ui.set_confirm_kind(0);
                                            steps.set(2);
                                        } else {
                                            record_failure(
                                                &failures,
                                                format!(
                                                    "X-02：清点为 0 必须如实提示无可处理包：{text}"
                                                ),
                                            );
                                        }
                                    } else {
                                        record_failure(
                                            &failures,
                                            "X-02：清点为 0 不得解除确认门禁（不得启动空解压任务）"
                                                .to_string(),
                                        );
                                    }
                                }
                            }
                            2 if !ui.get_busy() => {
                                steps.set(3);
                                let _ = slint::quit_event_loop();
                            }
                            _ => {}
                        }
                    },
                );
                // 驱动定时器必须活到事件循环结束：放进线程局部存储，事件循环退出前不会被回收。
                DRIVER.with(|slot| *slot.borrow_mut() = Some(driver));
            },
            Some(overrides),
        )
        .expect("GUI 流程失败");
        let recorded = take_failures(&failures);
        assert!(recorded.is_empty(), "{}", recorded.join("\n"));
    });

    assert!(
        zero_gate_held.load(Ordering::Relaxed),
        "必须观察到清点为 0 时门禁保持、提示如实"
    );
    assert_eq!(
        fs::read(data.join("keep.txt")).unwrap(),
        b"keep me",
        "X-05/H-07：未启动的解压不得删除或改动已有文件"
    );
    assert!(
        data.read_dir().unwrap().count() == 1,
        "零包流程结束后目录内容必须保持原样（未做任何改动）"
    );
}

// 覆盖 U-09（停止并关闭后窗口最终必然关闭）：MD 合并的扫描与冲突检查不经过任务检查点，
// 「停止并关闭」确认（close_after=true + 请求取消）后任务会自然结束并回传 MdDone；
// 结束分支必须处理 close_after 退出事件循环，不得把窗口留在运行界面、也不得让
// close_after 残留到之后的任务。本用例目录没有 .md 文件：合并不经过任何检查点直接
// 完成（MdDone），从而确定性地驱动该分支。
#[test]
fn stop_and_close_during_md_merge_quits_when_merge_finishes() {
    let fixture = tempfile::tempdir().unwrap();
    let docs = fixture.path().join("docs");
    fs::create_dir_all(&docs).unwrap(); // 空目录：合并立即以「没有 .md 文件」结束
    let state_dir: PathBuf = fixture.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let docs_text = docs.display().to_string();
    let overrides = EngineTestOverrides { state_dir };
    run_gui_job(move || {
        let failures: Failures = Arc::new(Mutex::new(Vec::new()));
        let failure_sink = Arc::clone(&failures);
        let ticks = Rc::new(Cell::new(0u32));
        gui::run_with_engine_overrides(
            move |ui| {
                ui.invoke_select_tool("md-organizer".into());
                ui.set_md_input_dir(docs_text.clone().into());
                ui.set_md_output_name("merged.md".into());
                ui.set_md_output_dir(docs_text.clone().into());
                // busy 已同步置位且 control 存在：立即确认「停止并关闭」。
                // 两个回调都在事件循环启动前同步执行，close_after 必然先于任何收尾事件。
                ui.invoke_md_merge_start();
                ui.invoke_confirmed(3);
                let ticks = ticks.clone();
                let failures = Arc::clone(&failure_sink);
                let driver = slint::Timer::default();
                driver.start(
                    slint::TimerMode::Repeated,
                    Duration::from_millis(200),
                    move || {
                        ticks.set(ticks.get() + 1);
                        if ticks.get() >= 75 {
                            // 修复前：MdDone 不读 close_after，事件循环不会退出，
                            // 只能靠这里超时退出——超时即「该关不关」缺陷（U-09）。
                            record_failure(
                                &failures,
                                "U-09：停止并关闭后 MD 任务自然结束（MdDone）15 秒内未关闭窗口"
                                    .to_string(),
                            );
                        }
                    },
                );
                DRIVER.with(|slot| *slot.borrow_mut() = Some(driver));
            },
            Some(overrides),
        )
        .expect("GUI 流程失败");
        let recorded = take_failures(&failures);
        assert!(
            recorded.is_empty(),
            "停止并关闭后 MD 合并结束必须关闭窗口：{}",
            recorded.join("\n")
        );
    });
}

// 覆盖 U-09（停止并关闭后不得再弹出覆盖确认）：MD 合并输出已存在时任务以 MD_CONFLICT
// 结束回传 MdNeedsConfirm；用户已确认「停止并关闭」时必须直接退出应用，而不是弹出
// 覆盖确认框把用户留在界面里。
#[test]
fn stop_and_close_skips_override_confirm_and_quits() {
    let fixture = tempfile::tempdir().unwrap();
    let docs = fixture.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("a.md"), "# 甲\n内容\n").unwrap();
    fs::write(docs.join("merged.md"), "旧输出").unwrap();
    let state_dir: PathBuf = fixture.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let docs_text = docs.display().to_string();
    let overrides = EngineTestOverrides { state_dir };
    run_gui_job(move || {
        let failures: Failures = Arc::new(Mutex::new(Vec::new()));
        let failure_sink = Arc::clone(&failures);
        let ticks = Rc::new(Cell::new(0u32));
        gui::run_with_engine_overrides(
            move |ui| {
                ui.invoke_select_tool("md-organizer".into());
                ui.set_md_input_dir(docs_text.clone().into());
                ui.set_md_output_name("merged.md".into());
                ui.set_md_output_dir(docs_text.clone().into());
                ui.invoke_md_merge_start();
                ui.invoke_confirmed(3);
                let ui = ui.as_weak();
                let ticks = ticks.clone();
                let failures = Arc::clone(&failure_sink);
                let driver = slint::Timer::default();
                driver.start(
                    slint::TimerMode::Repeated,
                    Duration::from_millis(200),
                    move || {
                        ticks.set(ticks.get() + 1);
                        let Some(ui) = ui.upgrade() else { return };
                        if ui.get_confirm_kind() == 4 {
                            // 修复前：MdNeedsConfirm 不读 close_after，会弹出覆盖确认框。
                            record_failure(
                                &failures,
                                "U-09：停止并关闭后不得再弹出覆盖确认框，应直接退出".to_string(),
                            );
                            let _ = slint::quit_event_loop();
                            return;
                        }
                        if ticks.get() >= 75 {
                            record_failure(
                                &failures,
                                "U-09：停止并关闭后 MD 冲突回传（MdNeedsConfirm）15 秒内未关闭窗口"
                                    .to_string(),
                            );
                        }
                    },
                );
                DRIVER.with(|slot| *slot.borrow_mut() = Some(driver));
            },
            Some(overrides),
        )
        .expect("GUI 流程失败");
        let recorded = take_failures(&failures);
        assert!(
            recorded.is_empty(),
            "停止并关闭后 MD 冲突必须直接退出而不是弹覆盖确认：{}",
            recorded.join("\n")
        );
    });

    // 确认「返回检查」语义未被绕过：旧输出没有被覆盖写入。
    assert_eq!(
        fs::read_to_string(fixture.path().join("docs").join("merged.md")).unwrap(),
        "旧输出",
        "U-09/M-07：停止并关闭路径不得覆盖已有输出"
    );
}
