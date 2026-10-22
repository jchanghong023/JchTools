use super::probe::turn;
use super::*;

pub(crate) async fn serve(root: PathBuf, mode: String) -> agent_client_protocol::Result<()> {
    std::fs::create_dir_all(&root).unwrap();
    let burst_root = (mode == "new-response-config-update").then(|| root.clone());
    let state = Arc::new(Mutex::new(State {
        root,
        mode,
        sessions: HashMap::new(),
        next: 0,
    }));
    locked(&state).audit(&json!({"event":"spawn","pid":std::process::id(),"cwd":std::env::current_dir().unwrap(),"argv":std::env::args().collect::<Vec<_>>(),"omp_jchtools_discovery":std::env::var("OMP_JCHTOOLS_DISCOVERY").ok()}));
    if locked(&state).mode == "exit-immediately" {
        std::process::exit(74);
    }
    let agent = Agent
        .builder()
        .name("JchTools SDK fixture")
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: InitializeRequest, responder, cx| {
                    let release = {
                        let state = locked(&state);
                        state.audit(
                            &json!({"event":"initialize","capabilities":request.client_capabilities}),
                        );
                        if state.mode.starts_with("handshake-error") {
                            return responder
                                .respond_with_error(Error::new(-32044, "fixture initialize failed"));
                        }
                        if state.mode == "initialize-held" {
                            let release = FixtureRelease(state.root.join("release-initialize"));
                            state.audit(&json!({"event":"initialize-held","pid":std::process::id()}));
                            Some(release)
                        } else {
                            None
                        }
                    };
                    let response = InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(AgentCapabilities::new());
                    if let Some(release) = release {
                        // 不阻塞 SDK dispatch；cancel/其他会话仍可到达。锁已在上方词法块释放。
                        cx.spawn(async move {
                            let result = release.wait("initialize release timeout").await;
                            responder.respond_with_result(result.map(|()| response))
                        })
                    } else {
                        responder.respond(response)
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: NewSessionRequest, responder, cx| {
                    let mut state = locked(&state);
                    state.next += 1;
                    let id = SessionId::new(format!("fixture-session-{}", state.next));
                    state.audit(&json!({"event":"session","session":id,"cwd":request.cwd,"config_id":model_config_id(&state.mode)}));
                    state.sessions.insert(
                        id.to_string(),
                        Session {
                            model: MODEL_A.into(),
                            cwd: request.cwd,
                            cancelled: false,
                        },
                    );
                    let response = NewSessionResponse::new(id.clone());
                    let mut config_options = options(MODEL_A, &state.mode);
                    if state.next > 1 {
                        match state.mode.as_str() {
                            "new-category-missing" => config_options[0].category = None,
                            "new-category-unknown" => {
                                config_options[0].category = Some(
                                    SessionConfigOptionCategory::Other("_vendor_unknown".into()),
                                );
                            }
                            "new-response-config-update" => {
                                // 初始通知已更新提供方目录；新会话直接报告当前 A/C。
                                // 不能假设客户端在返回 new 响应时已处理下一条独立通知。
                                config_options =
                                    options_for(MODEL_A, &state.mode, &[MODEL_A, MODEL_C]);
                            }
                            _ => {}
                        }
                    }
                    state.audit(&json!({"event":"session-options","session":response.session_id,"config_options":config_options}));
                    responder.respond(if state.mode == "no-models" {
                        response
                    } else {
                        response.config_options(config_options)
                    })?;
                    if state.mode == "new-response-config-update" {
                        let config_options = options_for(MODEL_A, &state.mode, &[MODEL_A, MODEL_C]);
                        // 两次 SDK 发布之间无 await；stdout writer 收齐两帧后原样一次写入。
                        cx.send_notification(SessionNotification::new(
                            id.clone(),
                            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                                config_options.clone(),
                            )),
                        ))?;
                        state.audit(&json!({"event":"new-config-update","session":id,"config_options":config_options}));
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: SetSessionConfigOptionRequest, responder, cx| {
                    let value = serde_json::to_value(&request.value).unwrap();
                    let model = value
                        .as_str()
                        .or_else(|| value.get("value").and_then(Value::as_str))
                        .unwrap_or("");
                    let (config_options, release) = {
                        let mut state = locked(&state);
                        let allowed = if state.mode == "model-options-expand" {
                            vec![MODEL_A, MODEL_B, MODEL_C]
                        } else if state.mode == "new-response-config-update" {
                            vec![MODEL_A, MODEL_C]
                        } else {
                            vec![MODEL_A, MODEL_B]
                        };
                        if request.config_id.to_string() != model_config_id(&state.mode)
                            || !allowed.contains(&model)
                        {
                            return responder
                                .respond_with_error(Error::new(-32602, "unknown fixture model"));
                        }
                        state
                            .sessions
                            .get_mut(&request.session_id.to_string())
                            .unwrap()
                            .model = model.into();
                        let mut config_options = options(model, &state.mode);
                        match state.mode.as_str() {
                            "model-category-missing" => config_options[0].category = None,
                            "model-category-unknown" => {
                                config_options[0].category = Some(
                                    SessionConfigOptionCategory::Other("_vendor_unknown".into()),
                                );
                            }
                            "model-options-expand" | "new-response-config-update" => {
                                config_options = options_for(model, &state.mode, &[MODEL_A, MODEL_C]);
                            }
                            "model-options-update" if model == MODEL_B => {
                                config_options = options_for(model, &state.mode, &[MODEL_B]);
                            }
                            _ => {}
                        }
                        state.audit(&json!({"event":"model","session":request.session_id,"model":model,
                            "config_id":request.config_id,"config_options":config_options}));
                        let release_path = state.root.join("release-model-set");
                        let release = if state.mode == "model-set-held"
                            && state.root.join("hold-model-set").exists()
                            && !release_path.exists()
                        {
                            let release = FixtureRelease(release_path);
                            state.audit(&json!({"event":"model-set-held","session":request.session_id,"pid":std::process::id()}));
                            Some(release)
                        } else {
                            None
                        };
                        (config_options, release)
                    };
                    let response = SetSessionConfigOptionResponse::new(config_options);
                    if let Some(release) = release {
                        // 不持 MutexGuard 跨 await，也不把其它会话/cancel 卡在 SDK dispatch。
                        cx.spawn(async move {
                            let result = release.wait("model set release timeout").await;
                            responder.respond_with_result(result.map(|()| response))
                        })
                    } else {
                        responder.respond(response)
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: PromptRequest, responder, cx| {
                    let state = state.clone();
                    let task_cx = cx.clone();
                    cx.spawn(async move { turn(&state, &request, responder, &task_cx).await })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = state.clone();
                async move |request: CancelNotification, _cx| {
                    let mut state = locked(&state);
                    if let Some(session) = state.sessions.get_mut(&request.session_id.to_string()) {
                        session.cancelled = true;
                    }
                    state.audit(&json!({"event":"cancel-received","session":request.session_id,"pid":std::process::id()}));
                    if state.mode == "cancel-exit" {
                        // 锁仍持有，barrier轮次不能先观察cancelled并回正常终态。
                        state.audit(&json!({"event":"cancel-exit","session":request.session_id,"pid":std::process::id(),"code":75}));
                        std::process::exit(75);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        );
    if let Some(root) = burst_root {
        let (deadline, mut deadlines) = tokio::sync::watch::channel(None);
        let writer = NewResponseBurstWriter {
            writer: PipeStdout,
            root,
            pending: Vec::new(),
            response_len: 0,
            session: None,
            deadline,
        };
        let watchdog = async move {
            loop {
                let deadline = *deadlines.borrow_and_update();
                if let Some(deadline) = deadline {
                    tokio::select! {
                        () = tokio::time::sleep_until(deadline) => {
                            return Err(Error::internal_error().data("fixture response/update pair timed out"));
                        }
                        changed = deadlines.changed() => {
                            if changed.is_err() { return std::future::pending().await; }
                        }
                    }
                } else if deadlines.changed().await.is_err() {
                    return std::future::pending().await;
                }
            }
        };
        tokio::select! {
            result = agent.connect_to(agent_client_protocol::ByteStreams::new(writer, FixtureStdin::new())) => result,
            result = watchdog => result,
        }
    } else {
        agent.connect_to(Stdio::new()).await
    }
}
