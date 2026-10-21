//! 设置页与音频资产下载用例（自 gui.rs 的 gui_tests 拆分）。
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

// 并发保存旧来源时，GUI 仍只呈现一个完整的生效根，不按功能拼接路径。
#[test]
fn settings_payload_keeps_one_effective_directory_during_source_changes() {
    use jchtools_foundation::xberg_settings::{self, Source};
    let _lock = jchtools_foundation::test_support::env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir().unwrap();
    struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
    let keys = [
        "JCHTOOLS_TEST_STATE_DIR",
        "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
    ];
    let _restore = Restore(
        keys.into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect(),
    );
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.path().join("state"));
    std::env::set_var(
        "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
        root.path().join("markdown"),
    );
    let custom = root.path().join("用户目录");
    let downloaded = root.path().join("下载目录");
    for path in [&custom, &downloaded] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("xberg.exe"), b"public settings fixture").unwrap();
    }
    let empty: serde_json::Value = serde_json::from_str(&settings_payload().unwrap()).unwrap();
    assert!(empty["active"].is_null());
    xberg_settings::save_source(Source::Custom, &custom).unwrap();
    xberg_settings::save_source(Source::Downloaded, &downloaded).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let writer_barrier = barrier.clone();
    let writer = std::thread::spawn(move || {
        writer_barrier.wait();
        for _ in 0..400 {
            xberg_settings::save_source(Source::Custom, &custom).unwrap();
            std::thread::yield_now();
            xberg_settings::save_source(Source::Downloaded, &downloaded).unwrap();
            std::thread::yield_now();
        }
    });
    barrier.wait();
    let mut mismatch = None;
    let mut read_error = None;
    for _ in 0..400 {
        match settings_payload() {
            Ok(payload) => {
                let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
                let active = value["active"].as_str().unwrap_or_default();
                let expected_custom = jchtools_file_core::platform::display_path_text(
                    &root.path().join("用户目录").display().to_string(),
                );
                let expected_downloaded = jchtools_file_core::platform::display_path_text(
                    &root.path().join("下载目录").display().to_string(),
                );
                if active != expected_custom && active != expected_downloaded && mismatch.is_none()
                {
                    mismatch = Some(value);
                }
            }
            Err(error) => {
                read_error = Some(error);
                break;
            }
        }
        std::thread::yield_now();
    }
    writer.join().unwrap();
    assert!(read_error.is_none(), "读取真实配置失败：{read_error:?}");
    assert!(
        mismatch.is_none(),
        "GUI必须呈现一个已保存的完整生效目录：{mismatch:?}"
    );
}

// 覆盖 U-09（S1-01）：任务运行中迟到的设置收尾事件不得消费 close_after 提前
// 退出——关闭必须留给任务终态（CONVERTER_DONE/FAIL 等）统一完成。
#[test]
fn settings_result_while_task_running_keeps_close_after() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_busy(true);
        app.state.borrow_mut().close_after = true;
        app.pump
            .out
            .send(Event::Notice("SETTINGS_READY|0|{\"active\":\"\"}".into()))
            .unwrap();
        app.pump.run(ui);
        assert!(
            app.state.borrow().close_after,
            "任务运行中 SETTINGS_READY 不得消费 close_after（U-09）"
        );
        app.pump
            .out
            .send(Event::Error("SETTINGS_ERROR|0|校验失败".into()))
            .unwrap();
        app.pump.run(ui);
        assert!(
            app.state.borrow().close_after,
            "任务运行中 SETTINGS_ERROR 不得消费 close_after（U-09）"
        );
    })
    .unwrap();
}

// 覆盖 U-09/XB-20（S1-01）：启动线程的设置快照（SETTINGS_LOADED）不是设置
// 操作的终态——到达时不得抹掉进行中的初始化状态，也不得消费 close_after。
#[test]
fn startup_settings_snapshot_keeps_running_initialization_state() {
    with_gui(|app| {
        let ui = &app.ui;
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        ui.set_convert_initializing(true);
        app.state.borrow_mut().convert_init_cancel = Some(cancel);
        app.state.borrow_mut().close_after = true;
        app.pump
            .out
            .send(Event::Notice("SETTINGS_LOADED|0|{\"active\":\"\"}".into()))
            .unwrap();
        app.pump.run(ui);
        assert!(
            ui.get_convert_initializing(),
            "启动快照不得抹掉进行中的初始化状态"
        );
        assert!(
            app.state.borrow().convert_init_cancel.is_some(),
            "启动快照不得清掉初始化取消句柄"
        );
        assert!(
            app.state.borrow().close_after,
            "启动快照不得消费 close_after（U-09）"
        );
    })
    .unwrap();
}

// 覆盖 XB-18/XB-21：保存完成后迟到的启动读取不得恢复旧目录或旧错误。
#[test]
fn startup_settings_results_do_not_replace_newer_audio_state() {
    with_gui(|app| {
        let ui = &app.ui;
        app.state.borrow_mut().settings_generation = 1;
        ui.set_convert_runtime_dir("当前生效目录".into());
        ui.set_settings_ffmpeg_status("已就绪".into());
        ui.set_settings_audio_model_status("未下载".into());
        ui.set_settings_status("新配置已保存".into());
        app.pump
            .out
            .send(Event::Notice(
                "SETTINGS_LOADED|0|{\"active\":\"stale\",\"ffmpeg_status\":\"未下载\"}".into(),
            ))
            .unwrap();
        app.pump
            .out
            .send(Event::Error("SETTINGS_LOAD_ERR|0|旧读取失败".into()))
            .unwrap();
        app.pump.run(ui);
        assert_eq!(ui.get_convert_runtime_dir().as_str(), "当前生效目录");
        assert_eq!(ui.get_settings_ffmpeg_status().as_str(), "已就绪");
        assert_eq!(ui.get_settings_audio_model_status().as_str(), "未下载");
        assert_eq!(ui.get_settings_status().as_str(), "新配置已保存");
    })
    .unwrap();
}

// XB-20：组件初始化或共享目录保存期间，不允许旧的 ready 快照启动转换。
#[test]
fn convert_start_is_gated_during_settings_operations() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.invoke_select_tool("markdown-converter".into());
        ui.set_convert_ready(true);
        ui.set_convert_initializing(true);
        ui.invoke_convert_start();
        assert!(!ui.get_convert_preparing());
        assert!(ui.get_error_text().is_empty());

        ui.set_convert_initializing(false);
        ui.set_convert_runtime_saving(true);
        ui.invoke_convert_start();
        assert!(!ui.get_convert_preparing());
        assert!(ui.get_error_text().is_empty());
    })
    .unwrap();
}

#[test]
fn settings_startup_pending_audio_download_waits_for_loaded_directory() {
    with_gui(|app| {
        with_settings_fixture(|root| {
            let generation = app.state.borrow().settings_generation;
            let payload = settings_payload().unwrap();
            let (sent, received) = mpsc::channel();
            let target = root.join("unused-download");
            app.state.borrow_mut().settings_download = Some(Arc::new(move |_| {
                sent.send(()).unwrap();
                Ok(target.clone())
            }));
            let ui = &app.ui;
            assert!(!ui.get_settings_download_enabled());
            ui.invoke_settings_action("download".into());
            assert!(!ui.get_convert_initializing());
            assert!(received.try_recv().is_err());
            assert_eq!(app.state.borrow().settings_generation, generation);
            app.pump
                .out
                .send(Event::Notice(format!(
                    "SETTINGS_LOADED|{generation}|{payload}"
                )))
                .unwrap();
            app.pump.run(ui);
            assert!(ui.get_settings_loaded());
            assert!(
                !ui.get_settings_download_enabled(),
                "缺少有效目录不能下载音频资产"
            );
        })
    })
    .unwrap();
}

#[test]
fn settings_load_failure_keeps_audio_download_blocked_and_offline_check_recovers() {
    with_gui(|app| {
        with_settings_fixture(|root| {
            let state_root = xberg_settings::state_dir().unwrap();
            std::fs::create_dir_all(&state_root).unwrap();
            std::fs::write(state_root.join("config.sqlite3"), b"unreadable database").unwrap();
            let error = settings_payload().unwrap_err();
            let generation = app.state.borrow().settings_generation;
            app.pump
                .out
                .send(Event::Error(format!(
                    "SETTINGS_LOAD_ERR|{generation}|{error}"
                )))
                .unwrap();
            app.pump.run(&app.ui);
            let ui = &app.ui;
            assert!(ui.get_settings_status().contains(&error));
            assert!(!ui.get_settings_download_enabled());
            let (sent, received) = mpsc::channel();
            let target = root.join("unused-download");
            app.state.borrow_mut().settings_download = Some(Arc::new(move |_| {
                sent.send(()).unwrap();
                Ok(target.clone())
            }));
            ui.invoke_settings_action("download".into());
            assert!(!ui.get_convert_initializing());
            assert!(received.try_recv().is_err());
            std::fs::remove_file(state_root.join("config.sqlite3")).unwrap();
            ui.invoke_settings_action("check".into());
            assert!(pump_until(app, || !ui.get_convert_runtime_saving()));
            assert!(
                ui.get_settings_loaded(),
                "即使未找到引擎，离线恢复也必须结束读取失败状态"
            );
            assert!(!ui.get_settings_load_failed());
            assert!(!ui.get_settings_download_enabled());
            assert!(received.try_recv().is_err(), "离线恢复不得调用下载器");
            assert!(xberg_settings::effective_directory().unwrap().is_none());
        })
    })
    .unwrap();
}

#[test]
fn audio_download_callback_is_single_flight_and_cancel_preserves_active_source() {
    with_gui(|app| {
        with_settings_fixture(|root| {
            let legacy = root.join("legacy");
            std::fs::create_dir_all(&legacy).unwrap();
            std::fs::write(legacy.join("xberg.exe"), b"legacy").unwrap();
            xberg_settings::save(&legacy).unwrap();
            let saved = xberg_settings::settings().unwrap();
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = calls.clone();
            let (sent, received) = mpsc::channel();
            app.state.borrow_mut().settings_download = Some(Arc::new(move |cancel| {
                observed.fetch_add(1, Ordering::SeqCst);
                sent.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !cancel.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert!(cancel.load(Ordering::Acquire), "download was not cancelled");
                Err("下载已取消".into())
            }));
            let ui = &app.ui;
            deliver_settings_loaded(app);
            ui.set_convert_runtime_dir(legacy.display().to_string().into());
            ui.set_convert_runtime_confirmed(true);
            let active = ui.get_convert_runtime_dir();
            ui.invoke_settings_action("download".into());
            assert!(ui.get_convert_initializing());
            assert!(!ui.get_settings_download_enabled());
            let generation = app.state.borrow().settings_generation;
            ui.invoke_settings_action("download".into());
            ui.invoke_settings_action("check".into());
            assert_eq!(app.state.borrow().settings_generation, generation);
            received.recv_timeout(Duration::from_secs(2)).unwrap();
            app.pump
                .out
                .send(Event::Notice(
                    "SETTINGS_READY|0|{\"active\":\"stale\"}".into(),
                ))
                .unwrap();
            app.pump
                .out
                .send(Event::Error("SETTINGS_ERROR|0|stale".into()))
                .unwrap();
            app.pump.run(ui);
            assert!(ui.get_convert_initializing());
            ui.invoke_settings_action("cancel".into());
            assert!(pump_until(app, || !ui.get_convert_initializing()));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(received.try_recv().is_err());
            assert!(app.state.borrow().convert_init_cancel.is_none());
            let current = xberg_settings::settings().unwrap();
            assert_eq!(
                (current.source, &current.custom, &current.downloaded),
                (saved.source, &saved.custom, &saved.downloaded)
            );
            assert_eq!(ui.get_convert_runtime_dir(), active);
            assert!(ui.get_convert_runtime_confirmed());
            assert!(ui.get_settings_audio_failed(), "取消收尾后保留重试入口");
        })
    })
    .unwrap();
}

// 仅替换下载边界；同根资产状态、真实回调及取消后重试均走生产装配。
#[test]
fn audio_download_completion_refreshes_same_root_without_changing_bundled_status() {
    with_gui(|app| {
        with_settings_fixture(|root| {
            let installed = root.join("bundled");
            let manifest: serde_json::Value =
                serde_json::from_str(include_str!("../../../../resources/markdown-assets.json"))
                    .unwrap();
            for member in manifest["xberg"]["members"].as_array().unwrap() {
                let path = member["path"].as_str().unwrap();
                if jchtools_xberg::xberg_runtime::asset_for_scenario(path, "document")
                    || jchtools_xberg::xberg_runtime::asset_for_scenario(path, "snapshot")
                {
                    let target = installed.join(path);
                    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                    std::fs::write(target, b"presence-only fixture").unwrap();
                }
            }
            xberg_settings::save_bundled_directory(&installed).unwrap();
            let expected = xberg_settings::effective_directory().unwrap().unwrap();
            let target_root = installed.clone();
            app.state.borrow_mut().settings_download = Some(Arc::new(move |cancel| {
                assert!(!cancel.load(Ordering::Acquire));
                let manifest: serde_json::Value = serde_json::from_str(include_str!(
                    "../../../../resources/markdown-assets.json"
                ))
                .unwrap();
                for member in manifest["xberg"]["members"].as_array().unwrap() {
                    let path = member["path"].as_str().unwrap();
                    if jchtools_xberg::xberg_runtime::asset_for_scenario(path, "media") {
                        let target = target_root.join(path);
                        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                        if !target.is_file() {
                            std::fs::write(target, b"presence-only fixture").unwrap();
                        }
                    }
                }
                Ok(target_root.clone())
            }));
            let ui = &app.ui;
            deliver_settings_loaded(app);
            assert_eq!(ui.get_settings_document_status().as_str(), "已安装");
            assert_eq!(ui.get_settings_snapshot_status().as_str(), "已安装");
            assert_eq!(ui.get_settings_ffmpeg_status().as_str(), "未下载");
            assert_eq!(ui.get_settings_audio_model_status().as_str(), "未下载");
            // 单独装有 FFmpeg 时模型仍未就绪，不能将两者混为同一个开关。
            for member in manifest["xberg"]["members"].as_array().unwrap() {
                let path = member["path"].as_str().unwrap();
                if path.starts_with("ffmpeg/") {
                    let target = installed.join(path);
                    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                    std::fs::write(target, b"presence-only FFmpeg fixture").unwrap();
                }
            }
            app.pump
                .apply_settings_snapshot(ui, &settings_payload().unwrap(), false);
            assert_eq!(ui.get_settings_ffmpeg_status().as_str(), "已就绪");
            assert_eq!(ui.get_settings_audio_model_status().as_str(), "未下载");
            assert_ne!(ui.get_settings_media_status().as_str(), "已就绪");
            let active = ui.get_convert_runtime_dir();
            ui.set_settings_audio_failed(true);
            ui.invoke_settings_action("download".into());
            assert!(ui.get_convert_initializing());
            assert!(pump_until(app, || !ui.get_convert_initializing()));
            assert!(ui.get_convert_runtime_confirmed());
            assert_eq!(ui.get_convert_runtime_dir(), active);
            assert_eq!(
                xberg_settings::effective_directory().unwrap().unwrap(),
                expected
            );
            assert_eq!(ui.get_settings_document_status().as_str(), "已安装");
            assert_eq!(ui.get_settings_snapshot_status().as_str(), "已安装");
            assert_eq!(ui.get_settings_ffmpeg_status().as_str(), "已就绪");
            assert_eq!(ui.get_settings_audio_model_status().as_str(), "已就绪");
            assert_eq!(ui.get_settings_media_status().as_str(), "已就绪");
            assert!(!ui.get_settings_audio_failed());
        })
    })
    .unwrap();
}

// 覆盖 XB-20（设置页操作门禁：任务、启动前检查等在途时不接受新操作；
// 未知操作以设置页状态行报错，不静默）
#[test]
fn settings_actions_are_gated_while_busy_and_reject_unknown_action() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_settings_loaded(true);
        ui.set_convert_runtime_confirmed(true);
        ui.set_convert_runtime_dir("fixture-runtime".into());
        let generation = app.state.borrow().settings_generation;
        ui.set_busy(true);
        ui.invoke_settings_action("download".into());
        assert!(
            !ui.get_convert_runtime_saving() && !ui.get_convert_initializing(),
            "任务运行中设置操作必须被门禁"
        );
        ui.set_busy(false);
        ui.set_convert_preparing(true);
        ui.invoke_settings_action("check".into());
        assert!(!ui.get_convert_runtime_saving(), "预检在途同样门禁");
        assert_eq!(app.state.borrow().settings_generation, generation);
        ui.set_convert_preparing(false);
        for blocked in 0..4 {
            ui.set_snap_initializing(blocked == 0);
            ui.set_confirm_kind(if blocked == 1 { 1 } else { 0 });
            ui.set_convert_runtime_saving(blocked == 2);
            app.state.borrow_mut().close_after = blocked == 3;
            let generation = app.state.borrow().settings_generation;
            ui.invoke_settings_action("download".into());
            assert_eq!(app.state.borrow().settings_generation, generation);
            assert!(!ui.get_convert_initializing());
        }
        ui.set_snap_initializing(false);
        ui.set_confirm_kind(0);
        ui.set_convert_runtime_saving(false);
        app.state.borrow_mut().close_after = false;
        app.pump
            .out
            .send(Event::Notice("SETTINGS_LOADED|0|{\"active\":null}".into()))
            .unwrap();
        app.pump.run(ui);
        ui.invoke_settings_action("bogus".into());
        assert!(
            pump_until(app, || ui.get_settings_status().contains("未知设置操作")),
            "未知操作必须报错：{}",
            ui.get_settings_status()
        );
        assert!(
            !ui.get_convert_runtime_saving(),
            "失败收尾必须复位保存中标志"
        );
    })
    .unwrap();
}
