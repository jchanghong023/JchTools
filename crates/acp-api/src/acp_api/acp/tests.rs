use super::*;
use agent_client_protocol::schema::v1::SessionConfigSelectGroup;
// 覆盖 AH-04 / AH-A08：完整历史含实际 assistant 输出；不重放历史。
#[test]
fn history_requires_exact_prefix_and_new_message() {
    let previous = vec![
        ChatMessage {
            role: MessageRole::User,
            text: "一".into(),
        },
        ChatMessage {
            role: MessageRole::Assistant,
            text: "二".into(),
        },
    ];
    let mut valid = previous.clone();
    valid.push(ChatMessage {
        role: MessageRole::User,
        text: "三".into(),
    });
    assert!(validate_history(&previous, &valid).is_ok());
    assert!(validate_history(&previous, &previous).is_err());
    valid[1].text = "不同".into();
    assert!(validate_history(&previous, &valid).is_err());
}
// 覆盖 AH-04：首次通过 Model 分类发现 Select，平铺和分组的真实 value id。
#[test]
fn model_discovery_uses_flat_and_grouped_select_values() {
    let choice = SessionConfigSelectOption::new("真实-model", "模型");
    let flat = SessionConfigOption::select("model", "模型", "真实-model", vec![choice.clone()])
        .category(SessionConfigOptionCategory::Model);
    assert_eq!(
        model_selector(&[flat], None).unwrap().values[0].id,
        "真实-model"
    );
    let grouped = SessionConfigOption::select(
        "model",
        "模型",
        "真实-model",
        vec![SessionConfigSelectGroup::new("group", "分组", vec![choice])],
    )
    .category(SessionConfigOptionCategory::Model);
    assert_eq!(
        model_selector(&[grouped], None).unwrap().values[0].id,
        "真实-model"
    );
    assert_eq!(
        model_selector(&[], None).err().unwrap().kind,
        ServiceErrorKind::ModelsUnavailable
    );
}

// 覆盖 AH-02/AH-04：可选 category 不改变已知 ID；仍验证每个新会话的真实选项。
#[test]
fn new_session_model_discovery_retains_negotiated_identity() {
    let negotiated_id = SessionConfigId::new("negotiated-selector");
    let option = SessionConfigOption::select(
        negotiated_id.clone(),
        "模型",
        "session-model",
        vec![SessionConfigSelectOption::new("session-model", "会话模型")],
    );
    for category in [
        None,
        Some(SessionConfigOptionCategory::Other("_vendor_unknown".into())),
    ] {
        let mut current = option.clone();
        current.category = category;
        assert!(model_selector(&[current.clone()], None).is_err());
        let selector = model_selector(&[current], Some(&negotiated_id)).unwrap();
        assert_eq!(selector.id, negotiated_id);
        assert_eq!(selector.current, "session-model");
        assert_eq!(selector.values[0].id, "session-model");
    }
    let different = SessionConfigOption::select(
        "session-specific-selector",
        "模型",
        "different-model",
        vec![SessionConfigSelectOption::new(
            "different-model",
            "不同模型",
        )],
    );
    assert!(model_selector(&[different.clone()], Some(&negotiated_id)).is_err());
    let selector = model_selector(
        &[
            option,
            different.category(SessionConfigOptionCategory::Model),
        ],
        Some(&negotiated_id),
    )
    .unwrap();
    assert_eq!(selector.id.to_string(), "session-specific-selector");
    assert_eq!(selector.values[0].id, "different-model");
    assert!(model_selector(&[], Some(&negotiated_id)).is_err());
    let invalid = SessionConfigOption::select(
        negotiated_id.clone(),
        "模型",
        "unlisted-model",
        vec![SessionConfigSelectOption::new("session-model", "会话模型")],
    );
    assert!(model_selector(&[invalid], Some(&negotiated_id)).is_err());
    let non_select = SessionConfigOption::boolean(negotiated_id.clone(), "不是模型选择", true);
    assert!(model_selector(&[non_select], Some(&negotiated_id)).is_err());
}

// 覆盖 AH-12 / AH-A08：4 个执行 lane 饱和后，同会话等待也计入 16 个容量。
#[test]
fn admission_counts_same_session_waiters_and_preserves_fifo() {
    let (status, _) = watch::channel(ServiceStatus::default());
    let mut actor = Actor {
        status,
        sessions: HashMap::new(),
        requests: HashMap::new(),
        queue: VecDeque::new(),
        active: HashSet::new(),
        models: Arc::new(Mutex::new(Ok(ModelSelector {
            id: SessionConfigId::new("model"),
            values: vec![ModelDescriptor {
                id: "可选模型".into(),
                name: "模型".into(),
            }],
            current: "可选模型".into(),
        }))),
        sealed: false,
        stopping: false,
        acknowledgements: Vec::new(),
    };
    for lane in 0..MAX_EXECUTING {
        actor.active.insert(SessionKey(format!("lane-{lane}")));
    }
    let session = SessionKey("lane-0".into());
    let mut ids = Vec::new();
    for index in 0..MAX_WAITING {
        let id = RequestId::new();
        let (events, _receiver) = mpsc::unbounded_channel();
        actor
            .admit(
                id.clone(),
                PromptInput {
                    cwd: std::env::temp_dir(),
                    model: "可选模型".into(),
                    messages: vec![ChatMessage {
                        role: MessageRole::User,
                        text: index.to_string(),
                    }],
                    session: Some(session.clone()),
                },
                events,
                CancellationToken::new(),
            )
            .unwrap();
        ids.push(id);
    }
    assert_eq!(actor.queue.iter().cloned().collect::<Vec<_>>(), ids);
    let (events, _receiver) = mpsc::unbounded_channel();
    let failure = actor
        .admit(
            RequestId::new(),
            PromptInput {
                cwd: std::env::temp_dir(),
                model: "可选模型".into(),
                messages: vec![ChatMessage {
                    role: MessageRole::User,
                    text: "溢出".into(),
                }],
                session: Some(session),
            },
            events,
            CancellationToken::new(),
        )
        .unwrap_err();
    assert_eq!(failure.kind, ServiceErrorKind::QueueFull);
    assert_eq!(actor.queue.len(), MAX_WAITING);
}

fn lifecycle_session(id: &str) -> SessionState {
    SessionState {
        id: SessionId::new(id),
        cwd: std::env::temp_dir(),
        model: Arc::new(Mutex::new(Ok(ModelSelector {
            id: SessionConfigId::new("model"),
            values: vec![ModelDescriptor {
                id: "可选模型".into(),
                name: "模型".into(),
            }],
            current: "可选模型".into(),
        }))),
        history: vec![
            ChatMessage {
                role: MessageRole::User,
                text: "已提交".into(),
            },
            ChatMessage {
                role: MessageRole::Assistant,
                text: "已完成".into(),
            },
        ],
    }
}

async fn assert_invalid_round_releases_selector(failure: ServiceError, terminal: RequestEvent) {
    let callbacks = Callbacks::new();
    let invalid = lifecycle_session("invalid");
    let invalid_id = invalid.id.clone();
    let invalid_model = Arc::downgrade(&invalid.model);
    let valid = lifecycle_session("valid");
    let valid_id = valid.id.clone();
    let valid_model = Arc::downgrade(&valid.model);
    let valid_history = valid.history.clone();
    let key = SessionKey("invalid-key".into());
    let valid_key = SessionKey("valid-key".into());
    let id = RequestId::new();
    let (events, mut receiver) = mpsc::unbounded_channel();
    {
        let mut routes = callbacks
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for state in [&invalid, &valid] {
            routes.known.insert(state.id.clone());
            routes.models.insert(state.id.clone(), state.model.clone());
            routes
                .model_ids
                .insert(state.id.clone(), SessionConfigId::new("model"));
            routes.active.insert(
                state.id.clone(),
                Route {
                    events: events.clone(),
                    cancellation: CancellationToken::new(),
                    text: String::new(),
                    failure: None,
                },
            );
        }
    }
    let (status, _) = watch::channel(ServiceStatus::default());
    let mut actor = Actor {
        status,
        sessions: HashMap::from([
            (
                key.clone(),
                SessionSlot {
                    cwd: std::env::temp_dir(),
                    state: None,
                    invalid: false,
                },
            ),
            (
                valid_key.clone(),
                SessionSlot {
                    cwd: std::env::temp_dir(),
                    state: Some(valid),
                    invalid: false,
                },
            ),
        ]),
        requests: HashMap::from([(
            id.clone(),
            Pending {
                key: key.clone(),
                input: None,
                events,
                cancellation: CancellationToken::new(),
                started: std::time::Instant::now(),
            },
        )]),
        queue: VecDeque::new(),
        active: HashSet::from([key.clone()]),
        models: lifecycle_session("catalog").model,
        sealed: false,
        stopping: false,
        acknowledgements: Vec::new(),
    };
    let result = finalize_round(&callbacks, id, key.clone(), invalid, Err(failure), true).await;
    actor.finish(result);
    assert_eq!(receiver.try_recv().unwrap(), terminal);
    assert!(actor.sessions[&key].invalid);
    assert!(actor.sessions[&key].state.is_none());
    assert!(!actor.active.contains(&key));
    let retained = actor.sessions[&valid_key].state.as_ref().unwrap();
    assert!(!actor.sessions[&valid_key].invalid);
    assert_eq!(retained.id, valid_id);
    assert_eq!(retained.history, valid_history);
    assert!(valid_model.upgrade().is_some());
    {
        let routes = callbacks
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            routes.known.contains(&invalid_id),
            "失效会话仍需拒绝 SDK id 复用"
        );
        assert!(routes.known.contains(&valid_id));
        assert!(!routes.active.contains_key(&invalid_id));
        assert!(routes.active.contains_key(&valid_id));
        assert!(routes.models.contains_key(&valid_id));
        assert!(!routes.model_ids.contains_key(&invalid_id));
        assert!(routes.model_ids.contains_key(&valid_id));
    }
    assert!(
        invalid_model.upgrade().is_none(),
        "失效轮次只保留 known/invalid 墓碑，不得让 routes.models 持有完整 selector"
    );
}

#[tokio::test]
async fn failed_round_releases_selector_and_preserves_session_tombstones() {
    let failure = error(ServiceErrorKind::Internal, "合成轮次失败");
    assert_invalid_round_releases_selector(failure.clone(), RequestEvent::Failed(failure)).await;
}

#[tokio::test]
async fn cancelled_round_releases_selector_and_preserves_session_tombstones() {
    assert_invalid_round_releases_selector(
        error(ServiceErrorKind::Cancelled, "合成 prompt 取消"),
        RequestEvent::Cancelled,
    )
    .await;
}

#[tokio::test]
async fn completed_round_preserves_selector_and_valid_history() {
    let callbacks = Callbacks::new();
    let state = lifecycle_session("completed");
    let session = state.id.clone();
    let history = state.history.clone();
    let model = Arc::downgrade(&state.model);
    {
        let mut routes = callbacks
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        routes.known.insert(session.clone());
        routes.models.insert(session.clone(), state.model.clone());
        routes
            .model_ids
            .insert(session.clone(), SessionConfigId::new("model"));
    }
    let result = finalize_round(
        &callbacks,
        RequestId::new(),
        SessionKey("completed-key".into()),
        state,
        Ok(FinishReason::Stop),
        true,
    )
    .await;
    assert_eq!(
        result.terminal,
        RequestEvent::Completed {
            reason: FinishReason::Stop
        }
    );
    let retained = result.state.unwrap();
    assert_eq!(retained.id, session);
    assert_eq!(retained.history, history);
    assert!(model.upgrade().is_some());
    let routes = callbacks
        .routes
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    assert!(routes.known.contains(&session));
    assert!(Arc::ptr_eq(&routes.models[&session], &retained.model));
    assert_eq!(routes.model_ids[&session], SessionConfigId::new("model"));
}

// Stop 是完整 drain：已完成的首错必须保留，但仍安全等待尚未结束的 cleanup。
#[tokio::test]
async fn stop_drain_preserves_first_cleanup_error_and_waits_for_live_cleanup() {
    let first = tokio::spawn(async { Err(callbacks::io_error("首个 retired cleanup 失败")) });
    let (release, held) = oneshot::channel();
    let (finished, mut observed_finished) = oneshot::channel();
    let live = tokio::spawn(async move {
        // sender panic/drop 同样自然放行，不 abort/kill live cleanup。
        let _released = held.await;
        let _observed = finished.send(());
        Ok(())
    });
    let later = tokio::spawn(async { Ok(()) });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !first.is_finished() || !later.is_finished() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut retired = vec![first, live, later];
    let mut first_failure = None;
    reap_retired(&mut retired, &mut first_failure);
    assert_eq!(retired.len(), 1, "运行期只保留尚未结束的 cleanup");
    assert!(first_failure.is_some(), "运行期回收不能吞掉已完成项的首错");
    let failure = {
        let draining = finish_retired(&mut retired, &mut first_failure);
        tokio::pin!(draining);
        assert!(futures::poll!(draining.as_mut()).is_pending());
        assert!(matches!(
            observed_finished.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), draining)
            .await
            .unwrap()
            .unwrap_err()
    };
    observed_finished.await.unwrap();
    assert!(retired.is_empty());
    assert_eq!(failure.kind, ServiceErrorKind::Io);
    assert_eq!(failure.message, "ACP 旧回调或终端资源收尾失败");
}
#[tokio::test]
async fn pre_prompt_cancel_preserves_confirmed_selector_and_history() {
    let callbacks = Callbacks::new();
    let state = lifecycle_session("not-published");
    let history = state.history.clone();
    let model = Arc::downgrade(&state.model);
    {
        let mut routes = callbacks
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        routes.known.insert(state.id.clone());
        routes.models.insert(state.id.clone(), state.model.clone());
        routes
            .model_ids
            .insert(state.id.clone(), SessionConfigId::new("model"));
    }
    let result = finalize_round(
        &callbacks,
        RequestId::new(),
        SessionKey("not-published-key".into()),
        state,
        Err(error(
            ServiceErrorKind::Cancelled,
            "set 已确认；prompt 未发布",
        )),
        false,
    )
    .await;
    assert_eq!(result.terminal, RequestEvent::Cancelled);
    assert_eq!(result.state.unwrap().history, history);
    assert!(model.upgrade().is_some());
}

// 真正启动现有 SDK fixture，贯穿 run_backend 异常/恢复/Stop；不是退役算法副本。
// 要求正常恢复期间回收已完成项，未完成项仍安全托管到自然退出及最终 Stop。
#[cfg(windows)]
#[tokio::test]
async fn backend_reclaims_completed_retirements_during_recovery_without_dropping_live_cleanup() {
    use crate::acp_api::{agent_process_channel, AgentConnection, AgentExit, AgentProcessCommand};
    use std::{process::Stdio, sync::Weak, time::Duration};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    #[derive(Clone)]
    struct Observation {
        retained: usize,
        completed: usize,
        live: usize,
        routes: Vec<Weak<Mutex<callbacks::Routes>>>,
    }
    struct ReleaseOnDrop {
        release: PathBuf,
        pid_marker: PathBuf,
    }
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, b"release");
            if let Ok(value) = std::fs::read_to_string(&self.pid_marker) {
                if let Ok(pid) = value.parse::<u32>() {
                    while alive(pid) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }
    }
    fn alive(pid: u32) -> bool {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_FAILED, WAIT_TIMEOUT},
            System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        };
        // SAFETY: 只查询测试自有进程，不修改/终止，随后关闭本次唯一拥有的句柄。
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return false;
        }
        // SAFETY: handle 是本函数刚取得的有效同步句柄，零超时只查询完成状态。
        let waited = unsafe { WaitForSingleObject(handle, 0) };
        // SAFETY: 本函数独占该有效句柄，查询后仅在这里关闭一次。
        unsafe { CloseHandle(handle) };
        assert_ne!(waited, WAIT_FAILED);
        waited == WAIT_TIMEOUT
    }
    let executable = std::env::current_exe().unwrap();
    let fixture = executable
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("jchtools-acp-fixture.exe");
    assert!(
        fixture.is_file(),
        "先构建真实 SDK fixture：{}",
        fixture.display()
    );
    let root = tempfile::tempdir().unwrap();
    let release = ReleaseOnDrop {
        release: root.path().join("terminal-release-retirement-held"),
        pid_marker: root.path().join("terminal-retirement-held.pid"),
    };
    let (processes, mut connections) = agent_process_channel();
    let (controls, mut controllers) = mpsc::unbounded_channel();
    let fixture_root = root.path().to_path_buf();
    let provider = tokio::spawn(async move {
        let mut children = Vec::new();
        while let Some(AgentProcessCommand::Connect { reply }) = connections.recv().await {
            let mut child = tokio::process::Command::new(&fixture)
                .args(["--agent", fixture_root.to_str().unwrap(), "normal"])
                .current_dir(&fixture_root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let pid = child.id().unwrap();
            let transport = agent_client_protocol::ByteStreams::new(
                child.stdin.take().unwrap().compat_write(),
                child.stdout.take().unwrap().compat(),
            );
            let (exit, exited) = watch::channel(None);
            let (finished, done) = oneshot::channel();
            let (kill, killed) = oneshot::channel();
            controls.send((kill, done)).unwrap();
            reply
                .send(Ok(AgentConnection {
                    transport,
                    pid,
                    exit: exited,
                    finished,
                }))
                .unwrap();
            children.push(tokio::spawn(async move {
                let result = tokio::select! {
                    result = child.wait() => result,
                    _ = killed => {
                        child.start_kill().unwrap();
                        child.wait().await
                    }
                };
                let _ = exit.send(Some(AgentExit {
                    code: result
                        .as_ref()
                        .ok()
                        .and_then(std::process::ExitStatus::code),
                    error: result.err().map(|error| error.to_string()),
                }));
            }));
        }
        for child in children {
            child.await.unwrap();
        }
    });
    let (status, _) = watch::channel(ServiceStatus::default());
    let (backend, commands) = backend_channel(status.subscribe());
    let mut phases = backend.subscribe_status();
    let (observations, mut snapshots) = mpsc::unbounded_channel();
    let mut routes = Vec::new();
    let observer: RetirementObserver = Box::new(move |retired, callbacks| {
        if let Some(callbacks) = callbacks {
            routes.push(Arc::downgrade(&callbacks.routes));
        }
        let completed = retired.iter().filter(|task| task.is_finished()).count();
        let _ = observations.send(Observation {
            retained: retired.len(),
            completed,
            live: retired.len() - completed,
            routes: routes.clone(),
        });
    });
    let running = tokio::spawn(run_backend_inner(
        processes,
        root.path().to_path_buf(),
        status,
        commands,
        Some(observer),
        None,
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
            // 首个 Submit 不负责启动握手：等待真实 initialize + 模型探测完成后的 Ready。
            while phases.borrow_and_update().phase != ServicePhase::Ready {
                phases.changed().await.unwrap();
            }
            assert_eq!(backend.models().await.unwrap().len(), 2);
            for index in 0..3 {
                let action = if index == 1 { "terminal-held" } else { "text" };
                let tag = if index == 1 { "retirement-held".into() } else { format!("retirement-{index}") };
                let plan = serde_json::json!({
                    "tag": tag, "action": action, "barrier": false, "chunks": ["真实恢复输出"],
                });
                let mut request = backend.submit(PromptInput {
                    cwd: root.path().to_path_buf(),
                    model: "fixture-model-a".into(),
                    messages: vec![ChatMessage {
                        role: MessageRole::User, text: format!("AH_FIXTURE {plan}"),
                    }],
                    session: None,
                }).await.unwrap();
                loop {
                    match request.events.recv().await.unwrap() {
                        RequestEvent::Completed { .. } => { request.cancellation.disarm(); break; }
                        RequestEvent::Failed(failure) => panic!("真实 fixture 轮次失败：{failure:?}"),
                        RequestEvent::Cancelled => panic!("真实 fixture 不应被取消"),
                        RequestEvent::TextDelta(_) => {}
                    }
                }
                let (kill, done) = controllers.recv().await.unwrap();
                kill.send(()).unwrap();
                assert_eq!(done.await.unwrap().unwrap_err().kind, ServiceErrorKind::AgentDisconnected);
            }
            // 第四个真实 Agent 正常恢复并服务 Models；旧 held terminal 不得阻塞。
            let plan = serde_json::json!({
                "tag": "retirement-recovered", "action": "text", "barrier": false, "chunks": ["恢复成功"],
            });
            let mut request = backend.submit(PromptInput {
                cwd: root.path().to_path_buf(),
                model: "fixture-model-a".into(), session: None,
                messages: vec![ChatMessage {
                    role: MessageRole::User, text: format!("AH_FIXTURE {plan}"),
                }],
            }).await.unwrap();
            while let Some(event) = request.events.recv().await {
                if matches!(event, RequestEvent::Completed { .. }) {
                    request.cancellation.disarm();
                    break;
                }
                assert!(!matches!(event, RequestEvent::Failed(_) | RequestEvent::Cancelled));
            }
            let (_keep_alive, mut finished) = controllers.recv().await.unwrap();
            let held = loop {
                assert_eq!(backend.models().await.unwrap().len(), 2);
                let mut latest = None;
                while let Ok(snapshot) = snapshots.try_recv() { latest = Some(snapshot); }
                if let Some(snapshot) = latest {
                    if snapshot.routes.len() == 3
                        && snapshot.live == 1
                        && snapshot.routes[0].upgrade().is_none()
                        && snapshot.routes[1].upgrade().is_some()
                        && snapshot.routes[2].upgrade().is_none()
                    { break snapshot; }
                }
                tokio::task::yield_now().await;
            };
            let terminal_pid = loop {
                if let Ok(value) = std::fs::read_to_string(root.path().join("terminal-retirement-held.pid")) {
                    break value.parse::<u32>().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            assert!(alive(terminal_pid), "恢复不得终止旧自有终端");
            assert_eq!(held.live, 1);
            std::fs::write(&release.release, b"release").unwrap();
            let reclaimed = loop {
                backend.models().await.unwrap();
                let mut latest = None;
                while let Ok(snapshot) = snapshots.try_recv() { latest = Some(snapshot); }
                if let Some(snapshot) = latest {
                    // Weak 失效早于 Tokio 发布 JoinHandle 完成位；必须另观测全部任务已完成。
                    // 只等 live==0，不等 retained==0：未回收的完成项仍会触发下面的零留存断言。
                    if snapshot.routes.len() == 3
                        && snapshot.live == 0
                        && snapshot.routes.iter().all(|route| route.upgrade().is_none())
                    {
                        break snapshot;
                    }
                }
                tokio::task::yield_now().await;
            };
            assert!(!alive(terminal_pid), "回收必须等待真实自有终端退出");
            // Stop 和当前连接 finished 的真实 oneshot 均必须成功，不能在正常回收后丢失 ACK。
            assert!(matches!(finished.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
            backend.stop().await.unwrap();
            finished.await.unwrap().unwrap();
            running.await.unwrap();
            provider.await.unwrap();
            // 清理已完成后才报告红，断言失败不留下 held terminal 或挂起 Stop。
            assert_eq!(held.retained, 1,
                "仅 live cleanup 可留存；观察到 completed={}、live={}", held.completed, held.live);
            assert_eq!(reclaimed.retained, 0,
                "正常服务/恢复期间应回收 completed={} 个真实 cleanup", reclaimed.completed);
        }).await.expect("10s 仅为回归 watchdog，不修改产品退出等待");
}

// P3-19/AH-13/AH-A09：真实 SDK foreground 的本地收尾错误不能冒充 Agent 断连。
#[cfg(windows)]
#[tokio::test]
async fn review_remediation_local_shutdown_failure_reaches_backend_consumer_as_io() {
    use crate::acp_api::{agent_process_channel, AgentConnection, AgentExit, AgentProcessCommand};
    use std::{process::Stdio, time::Duration};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    struct ReleaseOnDrop(PathBuf);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"release");
        }
    }
    fn alive(pid: u32) -> bool {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_FAILED, WAIT_TIMEOUT},
            System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        };
        // SAFETY: 只查询测试自有终端进程，并在查询后关闭唯一拥有的句柄。
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(ERROR_INVALID_PARAMETER as i32)
            );
            return false;
        }
        // SAFETY: 有效同步句柄，零超时只查询是否退出。
        let waited = unsafe { WaitForSingleObject(handle, 0) };
        // SAFETY: 本函数独占本次取得的句柄。
        unsafe { CloseHandle(handle) };
        assert_ne!(waited, WAIT_FAILED);
        waited == WAIT_TIMEOUT
    }
    let executable = std::env::current_exe().unwrap();
    let fixture = executable
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("jchtools-acp-fixture.exe");
    assert!(
        fixture.is_file(),
        "先构建真实 SDK fixture：{}",
        fixture.display()
    );
    let root = tempfile::tempdir().unwrap();
    let release = ReleaseOnDrop(root.path().join("terminal-release-local-shutdown"));
    let (processes, mut connections) = agent_process_channel();
    let fixture_root = root.path().to_path_buf();
    let provider = tokio::spawn(async move {
        let AgentProcessCommand::Connect { reply } = connections.recv().await.unwrap();
        let mut child = tokio::process::Command::new(fixture)
            .args(["--agent", fixture_root.to_str().unwrap(), "normal"])
            .current_dir(&fixture_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let transport = agent_client_protocol::ByteStreams::new(
            child.stdin.take().unwrap().compat_write(),
            child.stdout.take().unwrap().compat(),
        );
        let (exit, exited) = watch::channel(None);
        let (finished, done) = oneshot::channel();
        reply
            .send(Ok(AgentConnection {
                transport,
                pid,
                exit: exited,
                finished,
            }))
            .unwrap();
        // 不注入 EOF/exit：SDK 客户端真实退役后 Agent 自然退出并被 wait 回收。
        let result = child.wait().await.unwrap();
        let _ = exit.send(Some(AgentExit {
            code: result.code(),
            error: None,
        }));
        done.await.unwrap()
    });
    let (status, _) = watch::channel(ServiceStatus::default());
    let (backend, commands) = backend_channel(status.subscribe());
    let mut phases = backend.subscribe_status();
    let running = tokio::spawn(run_backend_inner(
        processes,
        root.path().to_path_buf(),
        status,
        commands,
        None,
        Some(|callbacks| callbacks.terminals.review_remediation_fail_shutdown()),
    ));
    tokio::time::timeout(Duration::from_secs(15), async {
        while phases.borrow_and_update().phase != ServicePhase::Ready {
            phases.changed().await.unwrap();
        }
        let plan = serde_json::json!({
            "tag": "local-shutdown", "action": "terminal-held",
            "barrier": false, "chunks": [],
        });
        let mut request = backend
            .submit(PromptInput {
                cwd: root.path().to_path_buf(),
                model: "fixture-model-a".into(),
                session: None,
                messages: vec![ChatMessage {
                    role: MessageRole::User,
                    text: format!("AH_FIXTURE {plan}"),
                }],
            })
            .await
            .unwrap();
        loop {
            match request.events.recv().await.unwrap() {
                RequestEvent::Completed { .. } => {
                    request.cancellation.disarm();
                    break;
                }
                RequestEvent::TextDelta(_) => {}
                event => panic!("正常 SDK 终端轮次未完成：{event:?}"),
            }
        }
        let pid = loop {
            if let Ok(value) =
                std::fs::read_to_string(root.path().join("terminal-local-shutdown.pid"))
            {
                break value.parse::<u32>().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(alive(pid), "注入之前真实终端必须仍在等待自然退出");
        std::fs::write(&release.0, b"release").unwrap();
        let stopped = backend.stop().await;
        running.await.unwrap();
        let finished = provider.await.unwrap();
        assert!(!alive(pid), "本地失败仍需完成真实终端收尾");
        // 两个真实消费者都在后台、Agent、终端完成退役后观察原始错误归属。
        assert_eq!(stopped.unwrap_err().kind, ServiceErrorKind::Io);
        assert_eq!(finished.unwrap_err().kind, ServiceErrorKind::Io);
    })
    .await
    .expect("15s 仅为测试 watchdog，不改变产品收尾等待");
}
