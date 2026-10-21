//! 跨工具杂项时序用例（自 gui.rs 的 gui_tests 拆分）。
#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
use super::gui_tests::*;
use super::*;

// 覆盖 T-05/T-06/U：上一用例迟到的后台事件不得串入新用例的界面。
// S1-04（复审修正）：测试 harness 每个用例新建独立窗口与事件通道，receiver
// 随上一用例的装配一起 drop——这一结构性事实就是隔离本身。原版与第一版
// 重写都只在断言端「排空后无红字」，而旧 sender 的 send 必然 Err 且被吞，
// 断言对「共享通道」回归不敏感（恒真）。现直接锁住结构性事实：新用例中
// 上一用例的 sender 必须 send Err（断连）；若有人把 harness 改成共享通道，
// 该断言即红。阳性对照证明「错误事件送达本用例通道必然上屏」，保证断言
// 机制本身有效。
#[test]
fn gui_test_cases_do_not_share_late_background_errors() {
    // 阳性对照：错误事件送达「本用例」通道时必然上屏为红字。
    with_gui(|app| {
        app.pump
            .out
            .send(Event::Error("上一用例的迟到错误".into()))
            .unwrap();
        app.pump.run(&app.ui);
        assert_eq!(
            app.ui.get_error_text().as_str(),
            "上一用例的迟到错误",
            "阳性对照失败：送达本用例通道的错误必须上屏"
        );
    })
    .unwrap();
    // 取出上一用例的 sender（经 with_gui 返回值带出），带进下一个用例
    // 验证它已随旧装配断连。
    let stale = with_gui(|app| app.pump.out.clone()).unwrap();
    with_gui(move |app| {
        let outcome = stale.send(Event::Error("上一用例的迟到错误".into()));
        assert!(
            outcome.is_err(),
            "上一用例的 sender 仍然连通：用例间事件通道未隔离，迟到事件可污染新用例"
        );
        // 防御性保持：即便未来出现其它迟到路径，新用例界面也不得有红字。
        let ui = &app.ui;
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            app.pump.run(ui);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            ui.get_error_text().is_empty(),
            "旧事件污染新界面：{}",
            ui.get_error_text()
        );
    })
    .unwrap();
}

// U-09：重叠的 GUI 任务与组件操作都完成前不得消费关闭请求。
#[test]
fn close_after_waits_for_concurrent_task_and_converter_initialization() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_busy(true);
        ui.set_convert_initializing(true);
        app.state.borrow_mut().close_after = true;

        app.pump
            .out
            .send(Event::MdDone("其他任务完成".into()))
            .unwrap();
        app.pump.run(ui);
        assert!(!ui.get_busy());
        assert!(ui.get_convert_initializing());
        assert!(
            app.state.borrow().close_after,
            "任务收尾不得越过仍在进行的组件初始化关闭窗口（U-09）"
        );

        ui.set_busy(true);
        app.pump
            .out
            .send(Event::Notice("SETTINGS_READY|0|{\"active\":\"\"}".into()))
            .unwrap();
        app.pump.run(ui);
        assert!(!ui.get_convert_initializing());
        assert!(
            app.state.borrow().close_after,
            "初始化收尾不得越过仍在运行的任务关闭窗口（U-09）"
        );

        app.pump
            .out
            .send(Event::MdDone("剩余任务完成".into()))
            .unwrap();
        app.pump.run(ui);
        assert!(
            !app.state.borrow().close_after,
            "全部 GUI 管理的操作收尾后应消费关闭请求"
        );
        // 其它无 busy 标志的 GUI 阶段也必须挡住关闭；只有全部空闲时才消费请求。
        app.state.borrow_mut().close_after = true;
        ui.set_convert_preparing(true);
        assert!(!app.pump.close_after_if_idle(ui), "转换预检在途时不得关闭");
        assert!(app.state.borrow().close_after);
        ui.set_convert_preparing(false);

        ui.set_convert_runtime_saving(true);
        assert!(!app.pump.close_after_if_idle(ui), "配置保存在途时不得关闭");
        assert!(app.state.borrow().close_after);
        ui.set_convert_runtime_saving(false);

        ui.set_snap_initializing(true);
        assert!(
            !app.pump.close_after_if_idle(ui),
            "截图字体初始化在途时不得关闭"
        );
        assert!(app.state.borrow().close_after);
        ui.set_snap_initializing(false);
        assert!(
            app.pump.close_after_if_idle(ui),
            "所有操作空闲后应消费关闭请求"
        );
    })
    .unwrap();
}
