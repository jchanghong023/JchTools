#[cfg(windows)]
use super::capture::attach_job_or_reap;
use super::capture::{
    join_with_deadline, reap, spawn_suspended, wait_child_with_deadline, CancelSource,
    PIPE_DRAIN_GRACE,
};
use super::pipe::{executable_kind, pump, summarize_failure, PipeMessage};
use crate::control::Control;
use anyhow::{bail, Result};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 默认空闲上限：连续无输出超过该时长视为挂死并 kill。
/// 正常任务（含超大压缩包）在工作期间会持续吐进度/条目输出，不会长时间静默；
/// 取 10 分钟可覆盖杀软扫描等短暂静默，又避免挂死进程长期占住 RootGuard。
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_mins(10);

pub fn run(
    command: &mut Command,
    control: &Control,
    line: impl FnMut(bool, &str) -> Result<()>,
    tick: impl FnMut() -> Result<()>,
) -> Result<()> {
    run_with_idle_timeout(command, control, DEFAULT_IDLE_TIMEOUT, line, tick)
}

/// 同 [`run`]，可自定义连续无输出空闲上限；超过则 kill 子进程并返回错误。
/// 调用方取消（`Control`）路径不受影响，仍可随时中断。
pub fn run_with_idle_timeout(
    command: &mut Command,
    control: &Control,
    idle_timeout: Duration,
    line: impl FnMut(bool, &str) -> Result<()>,
    tick: impl FnMut() -> Result<()>,
) -> Result<()> {
    let operation = jchtools_foundation::logging::operation_span("process", "stream");
    let _operation = operation.enter();
    let span = tracing::info_span!(
        "subprocess",
        executable = executable_kind(command),
        peer_pid = tracing::field::Empty
    );
    let _entered = span.enter();
    let started = Instant::now();
    tracing::info!(
        event = "spawn_started",
        idle_timeout_ms = u64::try_from(idle_timeout.as_millis()).unwrap_or(u64::MAX),
        "开始启动流式子进程"
    );
    let result = run_with_idle_timeout_inner(command, control, idle_timeout, line, tick);
    match &result {
        Ok(()) => tracing::info!(
            event = "subprocess_completed",
            state = "success",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "流式子进程执行完成"
        ),
        Err(_) if control.is_cancelled() => tracing::info!(
            event = "subprocess_cancelled",
            stage = "terminal",
            state = "cancelled",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "流式子进程操作已取消"
        ),
        Err(_) => tracing::error!(
            event = "subprocess_failed",
            state = "failed",
            error_type = "subprocess_error",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "流式子进程执行失败，详见关联阶段事件"
        ),
    }
    result
}

fn run_with_idle_timeout_inner(
    command: &mut Command,
    control: &Control,
    idle_timeout: Duration,
    mut line: impl FnMut(bool, &str) -> Result<()>,
    mut tick: impl FnMut() -> Result<()>,
) -> Result<()> {
    control.checkpoint().inspect_err(|_| {
        tracing::warn!(
            event = "subprocess_cancelled",
            stage = "checkpoint",
            "子进程启动前控制检查未通过"
        );
    })?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_suspended(command)?;
    if let Err(error) = control.check_cancelled() {
        reap(&mut child);
        return Err(error);
    }
    // 与捕获入口相同：恢复执行前加入本次拥有的 Job，失败/取消时回收整棵进程树。
    // 流式入口原路径失败时静默回收，report_failure 传 false 保持日志行为不变。
    #[cfg(windows)]
    let mut job = Some(attach_job_or_reap(&mut child, false)?);
    let (send, recv) = mpsc::sync_channel(64);
    // spawn 成功后若 take 失败，必须 kill+wait 回收子进程，避免残留。
    let Some(stdout_pipe) = child.stdout.take() else {
        tracing::error!(
            event = "pipe_missing",
            stream = "stdout",
            stage = "stream_setup",
            "子进程缺少输出管道"
        );
        #[cfg(windows)]
        drop(job.take());
        reap(&mut child);
        bail!("缺少 stdout");
    };
    let stdout = pump(stdout_pipe, false, send.clone());
    let Some(stderr_pipe) = child.stderr.take() else {
        tracing::error!(
            event = "pipe_missing",
            stream = "stderr",
            stage = "stream_setup",
            "子进程缺少错误管道"
        );
        #[cfg(windows)]
        drop(job.take());
        reap(&mut child);
        drop(send);
        drop(recv);
        let _ = join_with_deadline(stdout, PIPE_DRAIN_GRACE);
        bail!("缺少 stderr");
    };
    let stderr = pump(stderr_pipe, true, send);
    let mut stderr_tail = std::collections::VecDeque::new();
    let mut stdout_tail = std::collections::VecDeque::new();
    let result = (|| {
        let mut last_tick = std::time::Instant::now();
        // 空闲计时：只要管道还在产出就刷新；连续静默超过 idle_timeout 才判挂死，
        // 而不是限制总时长，避免误杀合法的超大任务。
        let mut last_output = Instant::now();
        loop {
            // Pause is deliberately deferred until this whole archive finishes. We must drain pipes.
            control.check_cancelled().inspect_err(|_| {
                tracing::warn!(
                    event = "subprocess_cancelled",
                    stage = "stream",
                    "流式处理期间收到取消"
                );
            })?;
            if last_tick.elapsed() > Duration::from_millis(500) {
                // 即使后代持续输出也检查根退出；释放本次 Job 后再排空剩余管道。
                #[cfg(windows)]
                if job.is_some() && child.try_wait()?.is_some() {
                    drop(job.take());
                }
                tick().inspect_err(|_| {
                    tracing::error!(
                        event = "subprocess_callback_failed",
                        stage = "tick",
                        error_type = "callback_error",
                        "子进程进度回调失败"
                    );
                })?;
                last_tick = std::time::Instant::now();
            }
            match recv.recv_timeout(Duration::from_millis(50)) {
                Ok(PipeMessage::Line(err, text)) => {
                    last_output = Instant::now();
                    if !text.is_empty() {
                        let sink = if err {
                            &mut stderr_tail
                        } else {
                            &mut stdout_tail
                        };
                        if sink.len() == 12 {
                            sink.pop_front();
                        }
                        sink.push_back(text.clone());
                    }
                    line(err, &text).inspect_err(|_| {
                        tracing::error!(
                            event = "subprocess_callback_failed",
                            stage = "line",
                            stream = if err { "stderr" } else { "stdout" },
                            error_type = "callback_error",
                            "子进程输出处理回调失败"
                        );
                    })?;
                }
                Ok(PipeMessage::Error(error)) => {
                    tracing::error!(
                        event = "subprocess_pipe_failed",
                        stage = "stream",
                        error_type = "pipe_error",
                        "子进程输出管道处理失败"
                    );
                    bail!("{error}");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if last_output.elapsed() >= idle_timeout {
                        tracing::warn!(
                            event = "subprocess_timeout",
                            stage = "idle",
                            timeout_ms =
                                u64::try_from(idle_timeout.as_millis()).unwrap_or(u64::MAX),
                            "子进程连续无输出超过空闲上限"
                        );
                        bail!(
                            "7-Zip 连续 {} 秒无输出，疑似挂死，已强制终止",
                            idle_timeout.as_secs()
                        );
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // 管道已断开后子进程仍可能挂死：限时等待，禁止无界 child.wait()。
        let status = wait_child_with_deadline(
            &mut child,
            idle_timeout,
            Some(&CancelSource::Control(control)),
            None,
        )?;
        tracing::info!(
            event = "subprocess_exited",
            peer_pid = child.id(),
            success = status.success(),
            exit_code = status.code(),
            "流式子进程已退出"
        );
        if !status.success() {
            tracing::error!(
                event = "subprocess_exit_failed",
                stage = "exit",
                exit_code = status.code(),
                error_type = "nonzero_exit",
                "子进程退出状态不成功"
            );
            let code = status
                .code()
                .map_or_else(|| "未知".into(), |code| code.to_string());
            let summary = summarize_failure(&stderr_tail, &stdout_tail);
            if summary.is_empty() {
                bail!("7-Zip 退出码 {code}，且没有输出可读的错误行；压缩包可能已损坏或不完整");
            }
            bail!("7-Zip 退出码 {code}（警告也不视为完整成功）：{summary}");
        }
        Ok(())
    })();
    #[cfg(windows)]
    drop(job.take());
    if result.is_err() {
        reap(&mut child);
    }
    drop(recv);
    // 7z 退出后管道应立即 EOF；限时收尾防止孙进程继承写端时 join 永久阻塞。
    let _ = join_with_deadline(stdout, PIPE_DRAIN_GRACE);
    let _ = join_with_deadline(stderr, PIPE_DRAIN_GRACE);
    result
}
