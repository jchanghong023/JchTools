use crate::storage::{self, Start};
use std::collections::VecDeque;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

const LOG_LIMIT: usize = 1_000_000;

#[derive(Default)]
struct Tail {
    chunks: VecDeque<(bool, Vec<u8>, usize)>,
    spare: Vec<Vec<u8>>,
    len: usize,
    truncated: bool,
    version: u64,
}

impl Tail {
    fn append(&mut self, stderr: bool, data: &[u8]) {
        if let Some((_, bytes, _)) = self
            .chunks
            .back_mut()
            .filter(|(channel, bytes, _)| *channel == stderr && bytes.len() + data.len() <= 8192)
        {
            bytes.extend_from_slice(data);
        } else {
            let mut chunk = self.spare.pop().unwrap_or_default();
            chunk.clear();
            chunk.extend_from_slice(data);
            self.chunks.push_back((stderr, chunk, 0));
        }
        self.len += data.len();
        self.version = self.version.wrapping_add(1);
        while self.len > LOG_LIMIT {
            let excess = self.len - LOG_LIMIT;
            if let Some((_, first, offset)) = self.chunks.front_mut() {
                let available = first.len() - *offset;
                if available <= excess {
                    self.len -= available;
                    if let Some((_, chunk, _)) = self.chunks.pop_front() {
                        if self.spare.len() < 2 {
                            self.spare.push(chunk);
                        }
                    }
                } else {
                    *offset += excess;
                    self.len -= excess;
                }
                self.truncated = true;
            }
        }
    }

    fn snapshot(&self, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) {
        stdout.clear();
        stderr.clear();
        for (is_stderr, bytes, offset) in &self.chunks {
            if *is_stderr {
                stderr.extend_from_slice(&bytes[*offset..]);
            } else {
                stdout.extend_from_slice(&bytes[*offset..]);
            }
        }
    }
}

fn capture<R: Read + Send + 'static>(
    mut reader: R,
    stderr: bool,
    run_id: i64,
    tail: Arc<Mutex<Tail>>,
    failed: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let mut output = tail
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    output.append(stderr, &buffer[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    tracing::warn!(
                        event = "scheduled_capture_failed",
                        run_id,
                        category = "io",
                        stream = if stderr { "stderr" } else { "stdout" },
                        "读取任务输出失败"
                    );
                    failed.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    })
}

fn flush_log(
    writer: &mut Option<storage::LogWriter>,
    run_id: i64,
    tail: &Mutex<Tail>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    saved_version: &mut Option<u64>,
    failed: &AtomicBool,
) {
    let Some(writer) = writer.as_mut() else {
        return;
    };
    let (version, truncated) = {
        let output = tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *saved_version == Some(output.version) {
            return;
        }
        output.snapshot(stdout, stderr);
        (output.version, output.truncated)
    };
    if writer.write(stdout, stderr, truncated).is_err() {
        tracing::warn!(
            event = "scheduled_log_write_failed",
            run_id,
            category = "storage",
            "保存任务日志失败"
        );
        failed.store(true, Ordering::Relaxed);
    } else {
        *saved_version = Some(version);
    }
}

pub(crate) fn execute(start: &Start, shutdown: &AtomicBool) {
    let id = start.task.id;
    let run_id = start.run_id;
    tracing::info!(
        event = "scheduled_run_started",
        task_id = id,
        run_id,
        category = "start",
        elapsed_ms = 0,
        "定时任务开始执行"
    );
    let (status, code) = execute_inner(start, shutdown);
    let category = match status {
        "成功" => "success",
        "超时" => "timeout",
        "已停止" => "stopped",
        _ => "failure",
    };
    tracing::info!(
        event = "scheduled_run_finished",
        task_id = id,
        run_id,
        category,
        elapsed_ms = storage::now().saturating_sub(start.started),
        "定时任务执行结束"
    );
    // 数据库暂时繁忙时重试，避免已回收的执行一直显示运行中。
    for _ in 0..20 {
        if storage::finish(id, run_id, start.started, status, code).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    tracing::error!(
        event = "scheduled_run_finish_failed",
        task_id = id,
        run_id,
        category = "database",
        elapsed_ms = storage::now().saturating_sub(start.started),
        "定时任务执行结果保存失败"
    );
}

fn execute_inner(start: &Start, shutdown: &AtomicBool) -> (&'static str, Option<i32>) {
    let task = &start.task;
    let mut command = Command::new(&task.shell);
    if task.shell.eq_ignore_ascii_case("cmd.exe") {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.args(["/D", "/S", "/C"]);
            // cmd 采用自身的引号规则，保留用户的完整命令。
            command.raw_arg(format!("\"{}\"", task.command));
        }
        #[cfg(not(windows))]
        command.args(["/D", "/S", "/C", &task.command]);
    } else {
        let script = format!(
            "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding; {}",
            task.command
        );
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ]);
    }
    if !task.working_directory.is_empty() {
        command.current_dir(&task.working_directory);
    }
    command
        .envs(&task.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    let spawned = jchtools_file_core::process::spawn_suspended(&mut command).map_err(|_| ());
    #[cfg(not(windows))]
    let spawned = command.spawn().map_err(|_| ());
    let Ok(mut child) = spawned else {
        tracing::error!(
            event = "scheduled_run_launch_failed",
            task_id = task.id,
            run_id = start.run_id,
            category = "spawn",
            "定时任务启动失败"
        );
        return ("失败", None);
    };
    #[cfg(windows)]
    let Ok(job) = jchtools_file_core::process::attach_job_or_reap(&mut child, false) else {
        tracing::error!(
            event = "scheduled_run_launch_failed",
            task_id = task.id,
            run_id = start.run_id,
            category = "job",
            "定时任务进程组关联失败"
        );
        return ("失败", None);
    };
    let Some(stdout) = child.stdout.take() else {
        tracing::error!(
            event = "scheduled_run_launch_failed",
            task_id = task.id,
            run_id = start.run_id,
            category = "stdout_pipe",
            "定时任务输出管道不可用"
        );
        let _ = child.kill();
        let _ = child.wait();
        return ("失败", None);
    };
    let Some(stderr) = child.stderr.take() else {
        tracing::error!(
            event = "scheduled_run_launch_failed",
            task_id = task.id,
            run_id = start.run_id,
            category = "stderr_pipe",
            "定时任务错误管道不可用"
        );
        let _ = child.kill();
        let _ = child.wait();
        return ("失败", None);
    };
    let tail = Arc::new(Mutex::new(Tail::default()));
    let failed = Arc::new(AtomicBool::new(false));
    let mut writer = storage::LogWriter::new(start.run_id).ok();
    if writer.is_none() {
        tracing::warn!(
            event = "scheduled_log_open_failed",
            task_id = task.id,
            run_id = start.run_id,
            category = "storage",
            "创建任务日志失败"
        );
        failed.store(true, Ordering::Relaxed);
    }
    let mut stdout_buffer = Vec::new();
    let mut stderr_buffer = Vec::new();
    let mut saved_version = None;
    let mut last_flush = Instant::now();
    let out = capture(
        stdout,
        false,
        start.run_id,
        Arc::clone(&tail),
        Arc::clone(&failed),
    );
    let err = capture(
        stderr,
        true,
        start.run_id,
        Arc::clone(&tail),
        Arc::clone(&failed),
    );
    let began = Instant::now();
    let mut exit = None;
    let mut outcome = None;
    let mut stop_error_logged = false;
    loop {
        if last_flush.elapsed() >= Duration::from_millis(250) {
            flush_log(
                &mut writer,
                start.run_id,
                &tail,
                &mut stdout_buffer,
                &mut stderr_buffer,
                &mut saved_version,
                &failed,
            );
            last_flush = Instant::now();
        }
        if exit.is_none() {
            if let Ok(value) = child.try_wait() {
                exit = value;
            } else {
                tracing::error!(
                    event = "scheduled_run_wait_failed",
                    task_id = task.id,
                    run_id = start.run_id,
                    category = "process",
                    "查询任务进程失败"
                );
                outcome = Some("失败");
            }
        }
        #[cfg(windows)]
        let active = if let Ok(count) = job.active_processes() {
            count != 0
        } else {
            tracing::error!(
                event = "scheduled_run_wait_failed",
                task_id = task.id,
                run_id = start.run_id,
                category = "job",
                "查询任务进程组失败"
            );
            outcome = Some("失败");
            true
        };
        #[cfg(not(windows))]
        let active = exit.is_none();
        if !active && exit.is_some() {
            break;
        }
        let stop = if let Ok(stop) = storage::stop_requested(task.id, start.run_id) {
            stop_error_logged = false;
            stop
        } else {
            if !stop_error_logged {
                tracing::warn!(
                    event = "scheduled_stop_query_failed",
                    task_id = task.id,
                    run_id = start.run_id,
                    category = "database",
                    "读取停止请求失败"
                );
                stop_error_logged = true;
            }
            false
        };
        if shutdown.load(Ordering::Relaxed) || stop {
            outcome = Some("已停止");
        } else if task
            .timeout_seconds
            .is_some_and(|seconds| began.elapsed() >= Duration::from_secs(seconds))
        {
            outcome = Some("超时");
        }
        if outcome.is_some() {
            #[cfg(windows)]
            if job.terminate().is_err() {
                // 关闭强制终止的 Job 仍能保证清理进程树。
                drop(job);
                if exit.is_none() {
                    exit = child.wait().ok();
                }
                break;
            }
            #[cfg(not(windows))]
            let _ = child.kill();
            if exit.is_none() {
                exit = child.wait().ok();
            }
            #[cfg(windows)]
            while job.active_processes().is_ok_and(|count| count != 0) {
                thread::sleep(Duration::from_millis(20));
            }
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if out.join().is_err() {
        tracing::error!(
            event = "scheduled_capture_failed",
            run_id = start.run_id,
            category = "thread",
            stream = "stdout",
            "输出读取线程异常"
        );
        failed.store(true, Ordering::Relaxed);
    }
    if err.join().is_err() {
        tracing::error!(
            event = "scheduled_capture_failed",
            run_id = start.run_id,
            category = "thread",
            stream = "stderr",
            "错误读取线程异常"
        );
        failed.store(true, Ordering::Relaxed);
    }
    flush_log(
        &mut writer,
        start.run_id,
        &tail,
        &mut stdout_buffer,
        &mut stderr_buffer,
        &mut saved_version,
        &failed,
    );
    let code = exit.and_then(|value| value.code());
    let result = outcome.unwrap_or_else(|| {
        if failed.load(Ordering::Relaxed) || !exit.is_some_and(|value| value.success()) {
            "失败"
        } else {
            "成功"
        }
    });
    (result, code)
}
