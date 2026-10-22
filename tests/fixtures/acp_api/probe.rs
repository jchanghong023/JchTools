use super::*;

async fn callbacks(
    plan: &Plan,
    id: &SessionId,
    cwd: &Path,
    cx: &ConnectionTo<Client>,
) -> agent_client_protocol::Result<String> {
    match plan.action.as_str() {
        "permission" | "permission-denied" => {
            let mut choices = vec![PermissionOption::new(
                "reject",
                "拒绝",
                PermissionOptionKind::RejectOnce,
            )];
            if plan.action == "permission" {
                choices.push(PermissionOption::new(
                    "once",
                    "一次",
                    PermissionOptionKind::AllowOnce,
                ));
                choices.push(PermissionOption::new(
                    "always",
                    "始终",
                    PermissionOptionKind::AllowAlways,
                ));
            }
            let response = cx
                .send_request(RequestPermissionRequest::new(
                    id.clone(),
                    ToolCallUpdate::new(
                        "fixture-tool",
                        ToolCallUpdateFields::new()
                            .title("权限探针")
                            .kind(ToolKind::Execute),
                    ),
                    choices,
                ))
                .block_task()
                .await?;
            Ok(serde_json::to_string(&response).unwrap())
        }
        "files" | "files-from-chunks" => {
            let path = cwd.join(format!("{}.txt", plan.tag));
            let content = if plan.action == "files-from-chunks" {
                plan.text()
            } else {
                format!("first\n{}\nlast\n", plan.tag)
            };
            cx.send_request(WriteTextFileRequest::new(id.clone(), path.clone(), content))
                .block_task()
                .await?;
            let response = cx
                .send_request(ReadTextFileRequest::new(id.clone(), path).line(2).limit(1))
                .block_task()
                .await?;
            Ok(response.content)
        }
        "file-missing" => {
            let response = cx
                .send_request(ReadTextFileRequest::new(
                    id.clone(),
                    cwd.join(format!("missing-{}", plan.tag)),
                ))
                .block_task()
                .await?;
            Ok(response.content)
        }
        "elicitation" => {
            let response = cx
                .send_request(CreateElicitationRequest::new(
                    ElicitationFormMode::new(
                        ElicitationSessionScope::new(id.clone()),
                        ElicitationSchema::new().string("answer", true),
                    ),
                    "不要编造回答",
                ))
                .block_task()
                .await?;
            Ok(serde_json::to_string(&response).unwrap())
        }
        // P3-20/AH-13：None 确实省略线协议字段，所有输出均由真实终端写入管道。
        "terminal-limit-default"
        | "terminal-limit-small"
        | "terminal-limit-large"
        | "terminal-limit-zero" => {
            let create = CreateTerminalRequest::new(
                id.clone(),
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            )
            .args(vec![
                "--terminal".into(),
                cwd.to_string_lossy().into_owned(),
                plan.tag.clone(),
                plan.action.clone(),
            ]);
            let create = match plan.action.as_str() {
                "terminal-limit-small" => create.output_byte_limit(137),
                "terminal-limit-large" => create.output_byte_limit(2 * 1024 * 1024),
                "terminal-limit-zero" => create.output_byte_limit(0),
                _ => create,
            };
            let terminal = cx.send_request(create).block_task().await?.terminal_id;
            let exited = cx
                .send_request(WaitForTerminalExitRequest::new(
                    id.clone(),
                    terminal.clone(),
                ))
                .block_task()
                .await?;
            let tail = format!("尾部:{}:完成🙂\n", plan.tag);
            let first = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let output = cx
                        .send_request(TerminalOutputRequest::new(id.clone(), terminal.clone()))
                        .block_task()
                        .await?;
                    // wait 不保证已读尽管道；末尾标记证明单一 stdout 已完整消费。
                    // 零预算不可能保留标记，观察真实 truncated 后由 release 等待完整 drain。
                    if output.output.ends_with(&tail)
                        || (plan.action == "terminal-limit-zero" && output.truncated)
                    {
                        return Ok::<_, Error>(output);
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .map_err(|_| Error::new(-32043, "限额终端输出等待超时"))??;
            let second = cx
                .send_request(TerminalOutputRequest::new(id.clone(), terminal.clone()))
                .block_task()
                .await?;
            cx.send_request(ReleaseTerminalRequest::new(id.clone(), terminal.clone()))
                .block_task()
                .await?;
            let released = cx
                .send_request(TerminalOutputRequest::new(id.clone(), terminal))
                .block_task()
                .await;
            Ok(json!({
                "wait": exited, "first": first, "second": second,
                "released_error": released.is_err(),
            })
            .to_string())
        }
        "terminal"
        | "terminal-kill"
        | "terminal-held"
        | "terminal-pwd"
        | "terminal-pwd-explicit" => {
            let terminal_cwd = if plan.action == "terminal-pwd-explicit" {
                cwd.with_file_name("终端-显式指定目录")
            } else {
                cwd.to_path_buf()
            };
            let create = CreateTerminalRequest::new(
                id.clone(),
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            )
            .args(vec![
                "--terminal".into(),
                terminal_cwd.to_string_lossy().into_owned(),
                plan.tag.clone(),
                plan.action.clone(),
            ])
            .output_byte_limit(4096);
            let create = if plan.action == "terminal-pwd" {
                create
            } else {
                create.cwd(terminal_cwd)
            };
            let child = cx.send_request(create).block_task().await?;
            let terminal = child.terminal_id;
            if plan.action == "terminal-held" {
                return Ok(serde_json::to_string(&terminal).unwrap());
            }
            if plan.action == "terminal-kill" {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while !cwd.join(format!("terminal-{}.pid", plan.tag)).exists() {
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::new(-32001, "终端未启动"));
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                cx.send_request(KillTerminalRequest::new(id.clone(), terminal.clone()))
                    .block_task()
                    .await?;
            }
            let exited = cx
                .send_request(WaitForTerminalExitRequest::new(
                    id.clone(),
                    terminal.clone(),
                ))
                .block_task()
                .await?;
            let first = if plan.action == "terminal" {
                let stdout = format!("stdout:{}\n", plan.tag);
                let stderr = format!("stderr:{}\n", plan.tag);
                // 合成命令只写这两行；两条管道均已观察到完整行才查询第二次累计输出。
                // 以实际产物作为完成协议，不依赖 EOF、管道读取调度或固定 sleep。
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let output = cx
                            .send_request(TerminalOutputRequest::new(id.clone(), terminal.clone()))
                            .block_task()
                            .await?;
                        if output.output.contains(&stdout) && output.output.contains(&stderr) {
                            return Ok::<_, Error>(output);
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .map_err(|_| Error::new(-32043, "终端完整输出等待超时"))??
            } else {
                cx.send_request(TerminalOutputRequest::new(id.clone(), terminal.clone()))
                    .block_task()
                    .await?
            };
            let second = cx
                .send_request(TerminalOutputRequest::new(id.clone(), terminal.clone()))
                .block_task()
                .await?;
            cx.send_request(ReleaseTerminalRequest::new(id.clone(), terminal.clone()))
                .block_task()
                .await?;
            let released = cx
                .send_request(TerminalOutputRequest::new(id.clone(), terminal))
                .block_task()
                .await;
            Ok(json!({"wait":exited,"first":first,"second":second,"released_error":released.is_err()}).to_string())
        }
        _ => Ok(plan.text()),
    }
}
pub(crate) async fn turn(
    state: &Mutex<State>,
    request: &PromptRequest,
    responder: Responder<PromptResponse>,
    cx: &ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let input = request
        .prompt
        .iter()
        .filter_map(|block| {
            if let ContentBlock::Text(text) = block {
                Some(text.text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let plan: Plan = input
        .rsplit("AH_FIXTURE ")
        .next()
        .and_then(|text| text.lines().next())
        .and_then(|text| serde_json::from_str(text).ok())
        .ok_or_else(|| Error::new(-32602, "缺少有效探针命令"))?;
    let cancellation = responder.cancellation();
    let (root, cwd, model) = {
        let mut state = locked(state);
        let session = state
            .sessions
            .get_mut(&request.session_id.to_string())
            .unwrap();
        session.cancelled = false;
        let cwd = session.cwd.clone();
        let model = session.model.clone();
        state.audit(&json!({"event":"started","tag":plan.tag,"session":request.session_id,"pid":std::process::id(),"model":model,"prompt":input,"prompt_blocks":request.prompt}));
        (state.root.clone(), cwd, model)
    };
    if plan.action == "crash" {
        std::process::exit(73);
    }
    if plan.action == "disconnect" {
        return Err(Error::new(-32045, "fixture connection retired"));
    }
    if plan.action == "error" {
        return responder
            .respond_with_error(Error::new(-32042, format!("fixture failure {}", plan.tag)));
    }
    if plan.action == "permission-after-cancel" {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !locked(state).sessions[&request.session_id.to_string()].cancelled {
            if std::time::Instant::now() >= deadline {
                return responder.respond_with_error(Error::new(-32043, "cancel timeout"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let outcome = cx
            .send_request(RequestPermissionRequest::new(
                request.session_id.clone(),
                ToolCallUpdate::new("cancelled-tool", ToolCallUpdateFields::new()),
                vec![PermissionOption::new(
                    "always",
                    "始终",
                    PermissionOptionKind::AllowAlways,
                )],
            ))
            .block_task()
            .await;
        let outcome = match outcome {
            Ok(response) => json!({"response":response}),
            Err(error) => json!({"error":error}),
        };
        locked(state)
            .audit(&json!({"event":"permission-after-cancel","tag":plan.tag,"outcome":outcome}));
        while !root.join(format!("release-{}", plan.tag)).exists() {
            if std::time::Instant::now() >= deadline {
                return responder
                    .respond_with_error(Error::new(-32043, "permission release timeout"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        return responder.respond(PromptResponse::new(StopReason::Cancelled));
    }
    if plan.action == "extension-interaction" {
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            cx.send_request(UntypedMessage::new(
                "_fixture/ask_user",
                json!({"sessionId":request.session_id,"question":"合成交互"}),
            )?)
            .block_task(),
        )
        .await;
        let outcome = match outcome {
            Ok(Ok(answer)) => json!({"answer":answer}),
            Ok(Err(error)) => json!({"error":error}),
            Err(_) => json!({"timed_out":true}),
        };
        locked(state).audit(&json!({"event":"extension-result","tag":plan.tag,"outcome":outcome}));
        let release = FixtureRelease(root.join(format!("release-{}", plan.tag)));
        if let Err(error) = release.wait("extension release timeout").await {
            return responder.respond_with_error(error);
        }
        locked(state).audit(&json!({"event":"extension-terminal","tag":plan.tag,"session":request.session_id,"pid":std::process::id()}));
        // Agent 忽略交互失败仍返回 EndTurn：适配层必须保留回调失败，不能伪报 HTTP 成功。
        return responder.respond(PromptResponse::new(StopReason::EndTurn));
    }
    if locked(state).mode == "model-options-update" {
        let config_options = options(&model, "model-options-update");
        cx.send_notification(SessionNotification::new(
            request.session_id.clone(),
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(config_options.clone())),
        ))?;
        locked(state).audit(
            &json!({"event":"config-update","tag":plan.tag,"config_options":config_options}),
        );
    }
    if plan.action == "tool-events" {
        let tool_id = format!("fixture-tool-{}", plan.tag);
        cx.send_notification(SessionNotification::new(
            request.session_id.clone(),
            SessionUpdate::ToolCall(
                ToolCall::new(tool_id.clone(), "工具事件探针")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::InProgress),
            ),
        ))?;
        locked(state).audit(&json!({"event":"tool-event","tag":plan.tag,"session":request.session_id,"kind":"tool_call","tool_call_id":tool_id}));
        cx.send_notification(SessionNotification::new(
            request.session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_id.clone(),
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            )),
        ))?;
        locked(state).audit(&json!({"event":"tool-event","tag":plan.tag,"session":request.session_id,"kind":"tool_call_update","tool_call_id":tool_id}));
    }
    // 首段在外部 barrier 释放前发送；消费者必须实际收到它才能释放 barrier。
    if plan.action == "text" {
        if let Some(first) = plan.chunks.first() {
            delta(cx, &request.session_id, first.clone())?;
        }
    }
    if plan.barrier {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !root.join(format!("release-{}", plan.tag)).exists() {
            let cancelled = cancellation.is_cancelled()
                || locked(state).sessions[&request.session_id.to_string()].cancelled;
            if cancelled {
                locked(state)
                    .audit(&json!({"event":"cancelled","tag":plan.tag,"pid":std::process::id()}));
                return responder.respond(PromptResponse::new(StopReason::Cancelled));
            }
            if locked(state).mode != "barrier-held" && std::time::Instant::now() >= deadline {
                return responder.respond_with_error(Error::new(-32043, "barrier timeout"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let result = if plan.action == "native-command" {
        if let Some(arguments) = input
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("/fixture-add "))
        {
            // 真正消费命令参数并计算结果；角色标签使首行不匹配，走普通文本分支。
            arguments
                .split_once(' ')
                .and_then(|(left, right)| {
                    left.parse::<i64>()
                        .ok()?
                        .checked_add(right.parse::<i64>().ok()?)
                })
                .map(|sum| json!({"command":"fixture-add","sum":sum}).to_string())
                .ok_or_else(|| Error::new(-32602, "求和命令需要两个有效整数"))
        } else {
            Ok(plan.text())
        }
    } else {
        callbacks(&plan, &request.session_id, &cwd, cx).await
    };
    match result {
        Ok(text) => {
            if plan.action == "text" || plan.action == "tool-events" {
                let first = usize::from(plan.action == "text");
                for chunk in plan.chunks.iter().skip(first) {
                    delta(cx, &request.session_id, chunk.clone())?;
                }
            } else {
                delta(cx, &request.session_id, text)?;
            }
            locked(state).audit(
                &json!({"event":"completed","tag":plan.tag,"model":model,"pid":std::process::id()}),
            );
            responder.respond(PromptResponse::new(StopReason::EndTurn))
        }
        Err(error) => responder.respond_with_error(error),
    }
}
