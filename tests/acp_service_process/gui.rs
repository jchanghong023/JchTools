use super::*;

thread_local! {
    // 和既有gui_flow一致：驱动定时器必须活过pre-loop hook，绝不占硬件鼠标。
    static GUI_DRIVER: std::cell::RefCell<Option<slint::Timer>> = const {std::cell::RefCell::new(None)};
}
fn gui_child_operation() -> bool {
    if std::env::var("JCHTOOLS_ACP_PROCESS_ACTION").as_deref() != Ok("gui") {
        return false;
    }
    use jchtools_gui::gui::{self, EngineTestOverrides};
    use slint::ComponentHandle;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    let root = PathBuf::from(std::env::var_os(CHILD_ROOT).unwrap());
    let config: ServiceConfig =
        serde_json::from_str(&std::env::var("JCHTOOLS_ACP_PROCESS_CONFIG").unwrap()).unwrap();
    let mode = std::env::var("JCHTOOLS_ACP_GUI_MODE").unwrap_or_else(|_| "reopen".into());
    std::env::set_var("SLINT_BACKEND", "winit-software");
    let snapshot_assets = root.join("synthetic-snapshot-assets");
    std::fs::create_dir_all(&snapshot_assets).unwrap();
    std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", snapshot_assets);
    let failure = Rc::new(RefCell::new(None::<String>));
    let observed = failure.clone();
    let state_dir = root.clone();
    let result_path = std::env::var_os("JCHTOOLS_ACP_PROCESS_RESULT").unwrap();
    gui::run_with_engine_overrides(move |ui| {
        ui.invoke_select_tool("acp-http".into());
        assert_eq!(ui.get_screen(),8);
        ui.window().set_size(slint::LogicalSize::new(1120.0,720.0));
        if mode=="save" {
            ui.set_acp_executable(config.executable.clone().into());
            ui.set_acp_arguments(settings::format_arguments(&config.arguments).into());
            ui.set_acp_port(config.port.to_string().into());
            ui.invoke_acp_config_edited();ui.invoke_acp_save_config();
        }
        let ui=ui.as_weak();let stage=Rc::new(Cell::new(0_u8));let stopped_at=Rc::new(Cell::new(None::<Instant>));
        let started=Instant::now();let timer=slint::Timer::default();
        timer.start(slint::TimerMode::Repeated,Duration::from_millis(50),move || {
            let Some(ui)=ui.upgrade() else {return;};
            let fail=|message:String| {*failure.borrow_mut()=Some(message);let _quit=slint::quit_event_loop();};
            if started.elapsed()>Duration::from_secs(45) {fail(format!("GUI流程超时 phase={} action={} error={}",ui.get_acp_phase(),ui.get_acp_operation(),ui.get_acp_action_error()));return;}
            if stage.get()==0 && ui.get_acp_ready() && !ui.get_acp_request_pending() {
                if ui.get_acp_executable().as_str()!=config.executable || settings::parse_arguments(ui.get_acp_arguments().as_str()).unwrap()!=config.arguments || ui.get_acp_port().as_str()!=config.port.to_string() {
                    fail("GUI保存/恢复的程序、argv或port不一致".into());return;
                }
                let snapshot=json!({"service_pid":ui.get_acp_service_pid().to_string(),"agent_pid":ui.get_acp_agent_pid().to_string(),"phase":ui.get_acp_phase().to_string(),
                    "saved":ui.get_acp_saved_config().to_string(),"running":ui.get_acp_running_config().to_string(),"ready":ui.get_acp_ready()});
                std::fs::write(root.join("gui-ready.json"),snapshot.to_string()).unwrap();stage.set(1);
            }
            if stage.get()==1 && root.join("gui-stop").exists() {
                ui.invoke_acp_request_stop();
                if ui.get_confirm_kind()!=5 {fail("独立模型服务退出未显示确认层5".into());return;}
                std::fs::write(root.join("gui-confirmed.txt"),ui.get_confirm_text().as_str()).unwrap();
                ui.invoke_confirmed(5);ui.set_confirm_kind(0);stage.set(2);
            }
            if stage.get()==2 && ui.get_acp_phase().as_str()=="stopped" && !ui.get_acp_request_pending() {
                if !ui.get_acp_explicit_stopped() {fail("当前GUI未记住显式退出".into());return;}
                stopped_at.set(Some(Instant::now()));stage.set(3);
            }
            if stage.get()==3 && stopped_at.get().is_some_and(|at|at.elapsed()>Duration::from_secs(1)) {
                if ui.get_acp_phase().as_str()!="stopped" || ui.get_acp_ready() || !ui.get_acp_service_pid().is_empty() || !ui.get_acp_agent_pid().is_empty() {
                    fail("显式退出后当前GUI自动拉起服务或残留PID".into());return;
                }
                std::fs::write(root.join("gui-stopped.json"),json!({"phase":ui.get_acp_phase().to_string(),"explicit":ui.get_acp_explicit_stopped()}).to_string()).unwrap();stage.set(4);
            }
            if stage.get()==1 && root.join("gui-save-next.json").exists() {
                let changed:ServiceConfig=serde_json::from_slice(&std::fs::read(root.join("gui-save-next.json")).unwrap()).unwrap();
                ui.set_acp_executable(changed.executable.into());ui.set_acp_arguments(settings::format_arguments(&changed.arguments).into());ui.set_acp_port(changed.port.to_string().into());
                ui.invoke_acp_config_edited();ui.invoke_acp_save_config();stage.set(5);
            }
            if stage.get()==5 && !ui.get_acp_request_pending() && ui.get_acp_pending_apply() {
                std::fs::write(root.join("gui-pending.json"),json!({"agent_pid":ui.get_acp_agent_pid().to_string(),"saved":ui.get_acp_saved_config().to_string(),"running":ui.get_acp_running_config().to_string()}).to_string()).unwrap();stage.set(6);
            }
            if stage.get()==6 && root.join("gui-apply").exists() {ui.invoke_acp_apply_and_restart();stage.set(7);}
            if stage.get()==7 && ui.get_acp_phase().as_str()=="draining" && !root.join("gui-draining.json").exists() {
                std::fs::write(root.join("gui-draining.json"),json!({"responsive":true,"agent_pid":ui.get_acp_agent_pid().to_string()}).to_string()).unwrap();
            }
            if stage.get()==7 && ui.get_acp_ready() && !ui.get_acp_request_pending() && !ui.get_acp_pending_apply() {
                std::fs::write(root.join("gui-applied.json"),json!({"agent_pid":ui.get_acp_agent_pid().to_string(),"saved":ui.get_acp_saved_config().to_string(),"running":ui.get_acp_running_config().to_string()}).to_string()).unwrap();stage.set(8);
            }
            if stage.get()>=1 && root.join("gui-close").exists() {
                // 和gui_flow标题栏路径一致，分发窗口内的合成事件，不移动系统鼠标。
                let position=slint::LogicalPosition::new(1097.0,23.0);
                for event in [slint::platform::WindowEvent::PointerMoved{position},
                    slint::platform::WindowEvent::PointerPressed{position,button:slint::platform::PointerEventButton::Left},
                    slint::platform::WindowEvent::PointerReleased{position,button:slint::platform::PointerEventButton::Left}] {
                    ui.window().dispatch_event(event);
                }
            }
        });
        GUI_DRIVER.with(|driver|*driver.borrow_mut()=Some(timer));
    },Some(EngineTestOverrides{state_dir})).unwrap();
    GUI_DRIVER.with(|driver| *driver.borrow_mut() = None);
    assert!(
        observed.borrow().is_none(),
        "真实GUI失败: {:?}",
        observed.borrow()
    );
    std::fs::write(result_path, json!({"gui":"closed"}).to_string()).unwrap();
    true
}

/// 覆盖 AH-01/AH-06～AH-10/U-09：真实winit窗口回调保存，关窗/终止GUI后HTTP继续；重开复用；独立确认退出。
/// 合成SDK窗口E2E补充覆盖，AH-A02/AH-A06指定OpenCode和真实模型仍NOT RUN。
#[test]
fn real_gui_save_close_kill_reopen_and_confirmed_service_exit() {
    if gui_child_operation() || child_operation() {
        return;
    }
    let test = "gui::real_gui_save_close_kill_reopen_and_confirmed_service_exit";
    let harness = Harness::new();
    let config = harness.config("normal");
    let launch_gui = |mode: &str| {
        for name in [
            "gui-ready.json",
            "gui-stopped.json",
            "gui-stop",
            "gui-close",
            "gui-confirmed.txt",
        ] {
            let path = harness.root.join(name);
            if path.exists() {
                std::fs::remove_file(path).unwrap();
            }
        }
        let result = harness
            .temp
            .path()
            .join(format!("gui-result-{}.json", uuid::Uuid::new_v4()));
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD_ROOT, &harness.root)
            .env("JCHTOOLS_ACP_PROCESS_ACTION", "gui")
            .env("JCHTOOLS_ACP_GUI_MODE", mode)
            .env(
                "JCHTOOLS_ACP_PROCESS_CONFIG",
                serde_json::to_string(&config).unwrap(),
            )
            .env("JCHTOOLS_ACP_PROCESS_RESULT", &result)
            .env("JCHTOOLS_TEST_STATE_DIR", &harness.root)
            .env(
                "JCHTOOLS_TEST_ACP_SERVICE_EXE",
                env!("CARGO_BIN_EXE_JchTools"),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        (ConsumerChild(child), result)
    };
    let (gui, result) = launch_gui("save");
    let ready = wait_file(&harness.root.join("gui-ready.json"));
    assert_eq!(ready["phase"], "ready");
    let agent = ready["agent_pid"].as_str().unwrap().parse::<u32>().unwrap();
    let service = ready["service_pid"]
        .as_str()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert_eq!(
        harness.call(test, "load", None)["config"],
        serde_json::to_value(&config).unwrap()
    );
    let permission = Plan::new("permission");
    let allowed = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(
            &permission,
            false,
            &harness.root.join("acp-workspace"),
        )),
        None,
    )
    .json();
    let allowed: Value = serde_json::from_str(completion(&allowed)).unwrap();
    assert_eq!(allowed["outcome"]["optionId"], "always");
    let mut first = Plan::new("text");
    first.barrier = true;
    harness.register_hold(&first);
    let mut response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&first, true, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(response.delta(), first.chunks[0]);
    std::fs::write(harness.root.join("gui-close"), b"close").unwrap();
    assert_eq!(finish_child(gui, &result)["gui"], "closed");
    release(&harness.audit, &first);
    assert_eq!(response.finish_stream(), first.chunks[1]);
    let after_close = Plan::new("text");
    assert_eq!(
        completion(
            &Http::open(
                harness.port,
                "POST",
                "/v1/chat/completions",
                Some(&body(
                    &after_close,
                    false,
                    &harness.root.join("acp-workspace")
                )),
                None
            )
            .json()
        ),
        after_close.text()
    );
    let permission = Plan::new("permission");
    let allowed = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(
            &permission,
            false,
            &harness.root.join("acp-workspace"),
        )),
        None,
    )
    .json();
    let allowed: Value = serde_json::from_str(completion(&allowed)).unwrap();
    assert_eq!(
        allowed["outcome"]["optionId"], "always",
        "无GUI时仍必须最大授权"
    );
    let file = Plan::new("files");
    let value = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&file, false, &harness.root.join("acp-workspace"))),
        None,
    )
    .json();
    assert_eq!(completion(&value).trim_end_matches('\n'), file.tag);
    assert_eq!(
        std::fs::read_to_string(
            harness
                .root
                .join("acp-workspace")
                .join(format!("{}.txt", file.tag))
        )
        .unwrap(),
        format!("first\n{}\nlast\n", file.tag)
    );
    let (mut gui, _result) = launch_gui("reopen");
    let reopened = wait_file(&harness.root.join("gui-ready.json"));
    assert_eq!(reopened["agent_pid"], ready["agent_pid"]);
    assert_eq!(reopened["service_pid"], ready["service_pid"]);
    let mut active = Plan::new("text");
    active.barrier = true;
    harness.register_hold(&active);
    let mut response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&active, true, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(response.delta(), active.chunks[0]);
    gui.kill().unwrap();
    let killed = gui.wait().unwrap();
    assert!(!killed.success(), "本次确实终止GUI进程");
    release(&harness.audit, &active);
    assert_eq!(response.finish_stream(), active.chunks[1]);
    let new = Plan::new("text");
    assert_eq!(
        completion(
            &Http::open(
                harness.port,
                "POST",
                "/v1/chat/completions",
                Some(&body(&new, false, &harness.root.join("acp-workspace"))),
                None
            )
            .json()
        ),
        new.text()
    );
    assert_eq!(
        wait_event(&harness.audit, "started", &new.tag)["pid"],
        agent
    );
    let (gui, result) = launch_gui("reopen");
    let again = wait_file(&harness.root.join("gui-ready.json"));
    assert_eq!(again["service_pid"], ready["service_pid"]);
    std::fs::write(harness.root.join("gui-stop"), b"confirmed-stop").unwrap();
    let stopped = wait_file(&harness.root.join("gui-stopped.json"));
    assert_eq!(stopped["explicit"], true);
    let confirmation = std::fs::read_to_string(harness.root.join("gui-confirmed.txt")).unwrap();
    assert!(
        confirmation.contains("截图")
            && confirmation.contains("Xberg")
            && confirmation.contains("不自动强杀")
    );
    wait_gone(agent);
    wait_gone(service);
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|entry| entry["event"] == "spawn")
            .count(),
        1,
        "当前GUI不得自动拉起"
    );
    std::fs::write(harness.root.join("gui-close"), b"close").unwrap();
    assert_eq!(finish_child(gui, &result)["gui"], "closed");
    let (gui, result) = launch_gui("reopen");
    let restarted = wait_file(&harness.root.join("gui-ready.json"));
    assert_ne!(restarted["agent_pid"], ready["agent_pid"]);
    let next = Plan::new("text");
    assert_eq!(
        completion(
            &Http::open(
                harness.port,
                "POST",
                "/v1/chat/completions",
                Some(&body(&next, false, &harness.root.join("acp-workspace"))),
                None
            )
            .json()
        ),
        next.text()
    );
    std::fs::write(harness.root.join("gui-close"), b"close").unwrap();
    assert_eq!(finish_child(gui, &result)["gui"], "closed");
}

/// 覆盖 AH-01/AH-09：真实窗口修改保存显示待应用；事件循环在draining中继续运行，显式应用等待HTTP完成。
#[test]
fn real_gui_pending_configuration_applies_only_after_inflight_completion() {
    if gui_child_operation() || child_operation() {
        return;
    }
    let test = "gui::real_gui_pending_configuration_applies_only_after_inflight_completion";
    let harness = Harness::new();
    let config = harness.config("normal");
    assert!(harness
        .call(test, "save", Some(&config))
        .get("error")
        .is_none());
    let status = harness.ready(test);
    let result = harness.temp.path().join("gui-apply-result.json");
    let gui = ConsumerChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD_ROOT, &harness.root)
            .env("JCHTOOLS_ACP_PROCESS_ACTION", "gui")
            .env("JCHTOOLS_ACP_GUI_MODE", "reopen")
            .env(
                "JCHTOOLS_ACP_PROCESS_CONFIG",
                serde_json::to_string(&config).unwrap(),
            )
            .env("JCHTOOLS_ACP_PROCESS_RESULT", &result)
            .env("JCHTOOLS_TEST_STATE_DIR", &harness.root)
            .env(
                "JCHTOOLS_TEST_ACP_SERVICE_EXE",
                env!("CARGO_BIN_EXE_JchTools"),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let ready = wait_file(&harness.root.join("gui-ready.json"));
    assert_eq!(ready["agent_pid"], status.agent_pid.unwrap().to_string());
    let mut plan = Plan::new("text");
    plan.barrier = true;
    harness.register_hold(&plan);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, true, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(stream.delta(), plan.chunks[0]);
    let mut changed = config.clone();
    changed.port = unused_port();
    let next_audit = harness.temp.path().join("GUI应用后 中文 参数");
    std::fs::create_dir(&next_audit).unwrap();
    changed.arguments[1] = next_audit.to_string_lossy().into_owned();
    std::fs::write(
        harness.root.join("gui-save-next.json"),
        serde_json::to_vec(&changed).unwrap(),
    )
    .unwrap();
    let pending = wait_file(&harness.root.join("gui-pending.json"));
    assert_eq!(pending["agent_pid"], ready["agent_pid"]);
    assert!(pending["saved"]
        .as_str()
        .unwrap()
        .contains(&changed.port.to_string()));
    assert!(pending["running"]
        .as_str()
        .unwrap()
        .contains(&config.port.to_string()));
    assert_eq!(
        harness.call(test, "load", None)["config"],
        serde_json::to_value(&changed).unwrap()
    );
    std::fs::write(harness.root.join("gui-apply"), b"apply").unwrap();
    let draining = wait_file(&harness.root.join("gui-draining.json"));
    assert_eq!(draining["responsive"], true);
    assert_eq!(draining["agent_pid"], ready["agent_pid"]);
    assert!(!harness.root.join("gui-applied.json").exists());
    assert!(audit(&next_audit).is_empty());
    assert_admission_closed(harness.port, &harness.root.join("acp-workspace"));
    release(&harness.audit, &plan);
    assert_eq!(stream.finish_stream(), plan.chunks[1]);
    let applied = wait_file(&harness.root.join("gui-applied.json"));
    assert_ne!(applied["agent_pid"], ready["agent_pid"]);
    assert_eq!(applied["saved"], applied["running"]);
    let next = Plan::new("text");
    let response = Http::open(
        changed.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&next, false, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), next.text());
    wait_gone(status.agent_pid.unwrap());
    std::fs::write(harness.root.join("gui-close"), b"close").unwrap();
    assert_eq!(finish_child(gui, &result)["gui"], "closed");
}
