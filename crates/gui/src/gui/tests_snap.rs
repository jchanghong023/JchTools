//! 截图 OCR 界面行为用例（自 gui.rs 的 gui_tests 拆分）。
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

// 覆盖 O-06/XB-24：已就绪时主动检查仍须重新执行离线校验；
// 隔离资产缺失时回到未就绪，而不是重新初始化或保留陈旧的就绪状态。
#[test]
fn snap_check_revalidates_already_ready_components() {
    with_gui(|app| {
        let ui = &app.ui;
        let _lock = jchtools_foundation::test_support::env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ui.set_snap_ready(true);
        ui.invoke_snap_initialize();
        assert!(!ui.get_snap_ready());
        assert!(!ui.get_snap_initializing(), "只读检查不重新初始化");
        assert!(app.state.borrow().snap_init_cancel.is_none());
        assert!(
            pump_until(app, || !ui.get_snap_asset_detail().is_empty()),
            "隔离资产缺失的离线检查必须返回未就绪原因"
        );
        assert!(!ui.get_snap_ready(), "缺失资产不得继续显示已就绪");
        assert!(!ui.get_snap_initializing());
        assert!(app.state.borrow().snap_init_cancel.is_none());
    })
    .unwrap();
}

// 覆盖 O-14/O-15/XB-24：保存一页设置不提交另一页未保存草稿。
#[test]
fn snap_settings_save_only_commits_current_page_draft() {
    with_gui(|app| {
        let ui = &app.ui;
        let (sender, receiver) = mpsc::channel();
        app.state.borrow_mut().snap_commands = Some(sender);
        ui.set_snap_hotkey("Ctrl+Alt+O".into());
        ui.set_snap_hotkey_draft("Ctrl+Alt+P".into());
        ui.set_snap_autostart(false);
        ui.set_snap_autostart_draft(true);
        ui.set_screen(7);
        ui.invoke_snap_save_settings();
        let SnapCommand::Request(request) = receiver.try_recv().unwrap() else {
            panic!("应提交设置");
        };
        assert_eq!(request["hotkey"], "Ctrl+Alt+O");
        assert_eq!(request["autostart"], true);
        ui.set_screen(6);
        ui.invoke_snap_save_settings();
        let SnapCommand::Request(request) = receiver.try_recv().unwrap() else {
            panic!("应提交设置");
        };
        assert_eq!(request["hotkey"], "Ctrl+Alt+P");
        assert_eq!(request["autostart"], false);
    })
    .unwrap();
}

// 覆盖 U-06（S3-04）：用户主动取消初始化不得显示为红色组件错误——取消只更新
// 中性状态行，红色错误文本与「组件未就绪」详情都必须保持空。
#[test]
fn snap_cancelled_initialize_is_not_shown_as_error() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_snap_error("".into());
        ui.set_snap_asset_detail("".into());
        let generation = app.state.borrow().snap_generation;
        app.state
            .borrow()
            .snap_sender
            .send(SnapMessage::Initialized(
                generation,
                Err("用户取消初始化".into()),
            ))
            .expect("发送取消消息");
        app.pump.apply_snap_messages(ui);
        assert!(
            ui.get_snap_error().is_empty(),
            "用户取消不得写入红色错误文本（U-06）"
        );
        assert_eq!(
            ui.get_snap_asset_detail().as_str(),
            "",
            "用户取消不得汇入「后台组件未就绪」红色详情（U-06）"
        );
        assert!(
            ui.get_snap_asset_status().contains("取消"),
            "取消应以中性状态行提示：{}",
            ui.get_snap_asset_status()
        );
        assert!(!ui.get_snap_ready());
    })
    .unwrap();
}

// Real offline callback repairs only the notice and never enters the downloader or changes legacy source.
#[test]
fn installed_components_check_preserves_legacy_source_and_restores_notice_offline() {
    with_gui(|app| {
        with_settings_fixture(|root| {
            let engine = root.join("legacy");
            let manifest: serde_json::Value =
                serde_json::from_str(include_str!("../../../../resources/markdown-assets.json"))
                    .unwrap();
            for member in manifest["xberg"]["members"].as_array().unwrap() {
                let path = member["path"].as_str().unwrap();
                if jchtools_xberg::xberg_runtime::asset_for_scenario(path, "engine") {
                    let target = engine.join(path);
                    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                    std::fs::write(target, b"presence-only fixture").unwrap();
                }
            }
            xberg_settings::save(&engine).unwrap();
            let saved = xberg_settings::settings().unwrap();
            app.state.borrow_mut().settings_download =
                Some(Arc::new(|_| panic!("offline check downloaded assets")));
            let notice = root.join("markdown/licenses/THIRD_PARTY_NOTICES.md");
            assert!(!notice.exists());
            let ui = &app.ui;
            deliver_settings_loaded(app);
            ui.invoke_navigation(7);
            assert_eq!(ui.get_screen(), 7);
            assert!(ui.get_active_tool_id().is_empty());
            ui.invoke_settings_action("check".into());
            assert!(ui.get_convert_runtime_saving());
            assert!(pump_until(app, || !ui.get_convert_runtime_saving()));
            assert!(notice.is_file());
            assert!(ui.get_convert_runtime_confirmed());
            assert_eq!(ui.get_settings_document_status().as_str(), "组件缺失");
            assert_eq!(ui.get_settings_snapshot_status().as_str(), "组件缺失");
            assert_ne!(ui.get_settings_media_status().as_str(), "已就绪");
            let current = xberg_settings::settings().unwrap();
            assert_eq!(
                (current.source, &current.custom, &current.downloaded),
                (saved.source, &saved.custom, &saved.downloaded)
            );
            std::fs::remove_file(engine.join("xberg.exe")).unwrap();
            let active = ui.get_convert_runtime_dir();
            ui.invoke_settings_action("check".into());
            assert!(pump_until(app, || !ui.get_convert_runtime_saving()));
            assert_eq!(ui.get_convert_runtime_dir(), active);
            let current = xberg_settings::settings().unwrap();
            assert_eq!(
                (current.source, &current.custom, &current.downloaded),
                (saved.source, &saved.custom, &saved.downloaded)
            );
            ui.invoke_select_tool("snap-ocr".into());
            assert_eq!(ui.get_screen(), 6);
            assert_eq!(ui.get_active_tool_id(), "snap-ocr");
            // XB-23/O-30：主动退出是正常状态，不显示红色连接故障。
            app.state
                .borrow()
                .snap_sender
                .send(SnapMessage::Service(
                    Err("后台已从托盘退出；重新打开 JchTools 后自动启动".into()),
                    false,
                ))
                .unwrap();
            app.pump.apply_snap_messages(ui);
            assert!(!ui.get_snap_connected());
            assert!(
                ui.get_snap_error().is_empty(),
                "主动退出不应显示错误：{}",
                ui.get_snap_error()
            );
            assert!(ui.get_snap_service_status().contains("已从托盘退出"));
        })
    })
    .unwrap();
}

// 覆盖 C'-2：识别中点「截图识别」收到 {"ok":false,"error":"正在识别"} 后
// requested=true 写入红字；识别结束纯轮询（requested=false）收到 ok 且
// error 为 null 的正常状态时必须清除旧红字——修复前该分支只在「requested
// 或 error 非空」时写，红字会一直残留到下一次请求。
#[test]
fn snap_error_clears_when_polling_reports_recovery() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_snap_error("".into());
        app.state
            .borrow()
            .snap_sender
            .send(SnapMessage::Service(
                Ok(serde_json::json!({"ok": false, "error": "正在识别"})),
                true,
            ))
            .expect("发送请求失败消息");
        app.pump.run(ui);
        assert_eq!(
            ui.get_snap_error().as_str(),
            "正在识别",
            "请求被服务拒绝时必须显示错误文本"
        );
        app.state
            .borrow()
            .snap_sender
            .send(SnapMessage::Service(
                Ok(serde_json::json!({"ok": true, "model": "loaded", "task": "idle"})),
                false,
            ))
            .expect("发送轮询消息");
        app.pump.run(ui);
        assert_eq!(
            ui.get_snap_error().as_str(),
            "",
            "轮询恢复正常（ok 且 error 为 null）后旧红字必须清除（C'-2）"
        );
    })
    .unwrap();
}

// 覆盖 C'-1：测试资产根覆盖必须同时隔离截图服务管道名——无头 GUI 装置的
// 监督线程 ping 测试派生名（不存在的管道）即失败返回，绝不触碰真实用户
// 会话的截图服务。修复前 pipe_name() 无视覆盖变量返回生产真名 → 本断言红
// （该真名可被真实服务应答，attach-main-exe 会把测试二进制写入其
// launcher.json，2026-09-29 本机已实际发生）。
#[test]
fn snap_asset_root_override_isolates_pipe_name() {
    let lock = jchtools_foundation::test_support::env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT");
    let root = snap_test_isolated_root();
    std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", &root);
    let pipe = jchtools_assets::snap_ocr_assets::pipe_name().unwrap();
    let asset_root = jchtools_assets::snap_ocr_assets::asset_root();
    match previous {
        Some(value) => std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", value),
        None => std::env::remove_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"),
    }
    drop(lock);
    assert_eq!(asset_root, root, "测试资产根覆盖必须同时接管资产根（C'-1）");
    assert!(
        pipe.starts_with(r"\\.\pipe\jchtools-snap-ocr-test-"),
        "管道名必须派生自隔离资产根的测试专用名，不得返回生产真名（C'-1）：{pipe}"
    );
}

// 覆盖 C'-1（端到端）：经真实 on_select_tool → ensure_snap_supervisor 链路
// 切换 snap-ocr 页，监督线程的 ping 只能命中测试专用管道名（无服务监听）、
// readiness 只能对 .tmp 隔离根失败，因此不得向真实 state dir 写入任何内容
// ——以真实 launcher.json 前后字节不变取证（该文件是既往污染的实际受害点）。
#[test]
fn gui_test_assembly_isolates_real_snap_service() {
    with_gui(|app| {
        let ui = &app.ui;
        let launcher = jchtools_file_core::config::state_dir()
            .ok()
            .map(|dir| dir.join("snap-ocr").join("launcher.json"));
        let before = launcher
            .as_deref()
            .and_then(|path| std::fs::read(path).ok());
        ui.set_snap_error("".into());
        ui.invoke_select_tool("snap-ocr".into());
        assert_eq!(ui.get_screen(), 6, "隔离不得改变页面导航语义（O-01）");
        // 隔离下 ensure 的确定结局：ping 测试专用名失败 → readiness 对空
        // 隔离根失败 → Service(Err) 经真实事件泵落地为错误文本。
        assert!(
            pump_until(app, || !ui.get_snap_error().is_empty()),
            "隔离下 ensure 必须以服务未连接错误落地（不得悬挂或触达真实服务）"
        );
        let after = launcher
            .as_deref()
            .and_then(|path| std::fs::read(path).ok());
        assert_eq!(
            before, after,
            "切换 snap-ocr 页不得读写真实 launcher.json（C'-1）"
        );
    })
    .unwrap();
}

// 覆盖 E-4（热键录制：Shift+数字行/标点在 US 布局上报为符号文本，必须反向映射
// 回基础键，不得静默丢弃）——通过真实回调端到端断言组合键字符串。
#[test]
fn snap_record_key_maps_shift_symbol_to_base_key() {
    with_gui(|app| {
        let ui = &app.ui;
        ui.set_snap_hotkey_draft("".into());
        ui.set_snap_recording(true);
        // Shift+7 经键盘布局映射上报为 "&"：录制结果必须是 Ctrl+Shift+7。
        ui.invoke_snap_record_key("&".into(), true, false, true, false);
        assert_eq!(
            ui.get_snap_hotkey_draft().as_str(),
            "Ctrl+Shift+7",
            "Shift+符号必须反向映射回基础键（E-4），不得静默丢键"
        );
        assert!(!ui.get_snap_recording(), "录制成功后必须退出录制态");
        assert_eq!(ui.get_snap_error().as_str(), "", "成功录制不得报错");
        // 对照：Shift+字母不受映射影响（布局上报已是大写字母）。
        ui.set_snap_recording(true);
        ui.invoke_snap_record_key("A".into(), true, false, true, false);
        assert_eq!(ui.get_snap_hotkey_draft().as_str(), "Ctrl+Shift+A");
        // 对照：不带 Shift 的符号不映射（保持既有行为，不扩大范围）。
        ui.set_snap_hotkey_draft("".into());
        ui.set_snap_recording(true);
        ui.invoke_snap_record_key("&".into(), true, false, false, false);
        assert_eq!(
            ui.get_snap_hotkey_draft().as_str(),
            "",
            "无 Shift 的符号主键仍不构成熟知组合，保持静默拒绝"
        );
        assert!(ui.get_snap_recording(), "未录制成功时保持录制态");
    })
    .unwrap();
}

// 覆盖 E-4（映射表逐对审计：US 布局 Shift 符号全部还原回基础键；
// Shift+字母不映射；无 Shift 的符号不映射——与 worker 侧表必须逐对一致）。
#[test]
fn snap_shift_symbol_map_covers_every_pair() {
    let pairs = [
        ('!', '1'),
        ('@', '2'),
        ('#', '3'),
        ('$', '4'),
        ('%', '5'),
        ('^', '6'),
        ('&', '7'),
        ('*', '8'),
        ('(', '9'),
        (')', '0'),
        ('~', '`'),
        ('_', '-'),
        ('+', '='),
        ('{', '['),
        ('}', ']'),
        ('|', '\\'),
        (':', ';'),
        ('"', '\''),
        ('<', ','),
        ('>', '.'),
        ('?', '/'),
    ];
    assert_eq!(
        pairs.len(),
        SNAP_SHIFT_SYMBOL_BASE.len(),
        "审计表与实现表长度一致"
    );
    for (symbol, base) in pairs {
        assert_eq!(
            normalize_recorded_primary_key(&symbol.to_string(), true),
            Some(base),
            "Shift+{symbol} 必须映射回基础键 {base}"
        );
    }
    // Shift+字母：布局上报已是大写字母，不在映射表内，必须原样保留。
    for letter in ['A', 'Z', 'a', 'z', '7'] {
        assert_eq!(
            normalize_recorded_primary_key(&letter.to_string(), true),
            Some(letter),
            "Shift+{letter} 不得被符号映射改写"
        );
    }
    // 无 Shift 的符号不映射（不扩大既有录制范围）。
    for symbol in ['&', '~', '/', '\''] {
        assert_eq!(
            normalize_recorded_primary_key(&symbol.to_string(), false),
            Some(symbol),
            "无 Shift 的 {symbol} 必须原样返回"
        );
    }
    // 多字符文本（输入法组合串）不构成可录制主键。
    assert_eq!(normalize_recorded_primary_key("ab", true), None);
    assert_eq!(normalize_recorded_primary_key("", true), None);
}

// 覆盖 C-1（snap 管道读响应必须有 deadline：服务接受连接后挂起不响应时，
// 请求必须按超时错误返回，监督线程不得永久阻塞导致页面按钮永久禁用，O-11/O-30）。
// 用注入的管道名与 300ms 小超时模拟服务挂起；生产默认 10 秒路径同一实现。
#[cfg(windows)]
#[test]
fn snap_pipe_read_times_out_when_service_hangs() {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    // 测试专用管道名（按进程隔离）：不得占用真实服务管道名，避免与用户
    // 正在运行的截图服务互抢实例或向其发送请求。
    let name = format!(r"\\.\pipe\jchtools-gui-test-hang-{}", std::process::id());
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: wide 以 NUL 结尾且调用期间存活；security 属性传空表示默认安全描述符。
    let pipe = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    assert!(pipe != INVALID_HANDLE_VALUE, "创建测试管道服务端失败");
    // 服务端线程：完成连接后挂起（永不响应），模拟服务卡死。
    #[derive(Clone, Copy)]
    struct SendPipe(windows_sys::Win32::Foundation::HANDLE);
    // SAFETY: 句柄经包装后仅移入服务端线程独占使用，不再被创建线程触碰；
    // Win32 句柄本身可跨线程使用，Send 仅解除裸指针的编译期限制。
    unsafe impl Send for SendPipe {}
    fn serve_hung_pipe(pipe: SendPipe) {
        let SendPipe(pipe) = pipe;
        // SAFETY: pipe 为本线程独占的有效句柄；同步模式不使用 overlapped，
        // 客户端先连上时返回 FALSE + ERROR_PIPE_CONNECTED(535)，同为已连接。
        unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) };
        // 挂起远超测试墙钟：模拟服务进程活着但永不响应（真实挂起场景）。
        // 线程随测试进程退出终止，用例不 join，避免拖慢测试。
        std::thread::sleep(Duration::from_secs(30));
        // SAFETY: 关闭本线程独占的句柄，无未完成 I/O 需要等待。
        unsafe { CloseHandle(pipe) };
    }
    let pipe = SendPipe(pipe);
    // 函数边界传递整个 SendPipe：闭包按整值捕获（Send），避免精确捕获裸句柄字段。
    // 服务端线程随测试进程退出终止（挂起即测试目的，不 join）。
    std::thread::spawn(move || serve_hung_pipe(pipe));
    let started = Instant::now();
    let result = snap_pipe_request_on(
        &name,
        &serde_json::json!({"command": "ping"}),
        Duration::from_millis(300),
    );
    let elapsed = started.elapsed();
    let message = result.expect_err("服务挂起时读响应必须以错误返回，不得永久阻塞");
    assert!(
        message.contains("超时"),
        "错误必须说明是响应超时（C-1）：{message}"
    );
    assert!(
        elapsed >= Duration::from_millis(250),
        "必须等到 deadline 才返回（提前失败说明走的是其他错误路径）：{elapsed:.1?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "必须在超时 + 余量内返回（C-1 修复前此处永久挂起）：{elapsed:.1?}"
    );
}

// 覆盖 O-06/U-06（在途初始化不重入、取消信号与收尾状态）及命令按钮即时反馈。
// 已就绪时的设置页离线检查由 snap_check_revalidates_already_ready_components 覆盖。
#[test]
fn snap_buttons_immediate_feedback_and_initialize_reentry_gating() {
    with_gui(|app| {
        let ui = &app.ui;
        // 控制在途任务的完成时机：重复点击必须保留当前进度和取消目标。
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.state.borrow_mut().snap_init_cancel = Some(cancel.clone());
        ui.set_snap_ready(false);
        ui.set_snap_initializing(true);
        ui.set_snap_progress("当前组件进度".into());
        ui.set_snap_asset_status("当前组件初始化在途".into());
        let progress = ui.get_snap_progress();
        let status = ui.get_snap_asset_status();
        ui.invoke_snap_initialize();
        assert!(ui.get_snap_initializing());
        assert!(!ui.get_snap_ready());
        assert_eq!(
            ui.get_snap_progress(),
            progress,
            "重复初始化不得重置当前进度"
        );
        assert_eq!(
            ui.get_snap_asset_status(),
            status,
            "重复初始化不得替换当前状态"
        );
        assert!(!cancel.load(Ordering::Acquire), "重复初始化不得取消原任务");

        ui.invoke_snap_cancel_initialize();
        assert!(cancel.load(Ordering::Acquire), "取消必须作用于原在途初始化");
        assert!(ui.get_snap_initializing(), "取消请求不能冒充任务已结束");
        assert_ne!(ui.get_snap_asset_status(), status);
        let generation = app.state.borrow().snap_generation;
        app.state
            .borrow()
            .snap_sender
            .send(SnapMessage::Initialized(
                generation,
                Err("用户取消初始化".into()),
            ))
            .unwrap();
        app.pump.apply_snap_messages(ui);
        assert!(!ui.get_snap_initializing());
        assert!(!ui.get_snap_ready());
        assert!(app.state.borrow().snap_init_cancel.is_none());
        assert!(ui.get_snap_error().is_empty(), "用户取消不得显示为组件故障");
        assert!(ui.get_snap_asset_detail().is_empty());

        // 管道按钮：点击即置请求在途标志并给出新的状态反馈。
        let service_status = ui.get_snap_service_status();
        ui.invoke_snap_connect();
        assert!(ui.get_snap_request_pending());
        assert_ne!(ui.get_snap_service_status(), service_status);
        ui.set_snap_request_pending(false);
        ui.invoke_snap_capture();
        assert!(ui.get_snap_request_pending());
        ui.set_snap_request_pending(false);
        ui.invoke_snap_retry_load();
        assert!(ui.get_snap_request_pending());
        ui.set_snap_request_pending(false);
        ui.invoke_snap_save_settings();
        assert!(ui.get_snap_request_pending());
        // 自启动草稿开关：真实回调写草稿值（生效另经 save-settings 命令）
        ui.invoke_snap_draft_autostart(true);
        assert!(ui.get_snap_autostart_draft());
        ui.invoke_snap_draft_autostart(false);
        assert!(!ui.get_snap_autostart_draft());
    })
    .unwrap();
}
