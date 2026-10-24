use super::pipe::executable_kind;
use crate::control::Control;
use anyhow::{bail, Result};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
fn job_os_error(stage: &'static str) -> std::io::Error {
    let error = std::io::Error::last_os_error();
    tracing::error!(event = "subprocess_job_failed", stage, error_type = ?error.kind(), error_code = error.raw_os_error(), "子进程组系统调用失败");
    error
}

/// 单流捕获上限：nettest/proxy 等调用方的正常输出很小；
/// 超过上限说明输出异常膨胀，截断保存并标记 truncated，避免内存无界增长。
pub(crate) const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;

/// 捕获到的子进程输出（只读查询用；无 shell）。
#[derive(Debug)]
pub struct CapturedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// stdout 是否因超过捕获上限 `MAX_CAPTURE_BYTES` 被截断
    pub stdout_truncated: bool,
    /// stderr 是否因超过捕获上限 `MAX_CAPTURE_BYTES` 被截断
    pub stderr_truncated: bool,
}

impl CapturedOutput {
    /// 任一流超过捕获上限被截断时返回提示。截断的输出不完整，调用方不得把它
    /// 当完整结果解析（否则进程/端口列表会静默缺项），应报错或并入备注。
    pub fn truncation_note(&self) -> Option<String> {
        let which = match (self.stdout_truncated, self.stderr_truncated) {
            (true, true) => "stdout 与 stderr",
            (true, false) => "stdout",
            (false, true) => "stderr",
            _ => return None,
        };
        Some(format!(
            "子进程{which}输出不完整（超过捕获上限被截断，或子进程退出后管道未能排空、读取超时被放弃）"
        ))
    }
}

/// 子进程退出后等待管道读线程收尾的宽限：正常情况下进程退出管道随即 EOF，读线程几乎立刻结束；
/// 孙进程继承管道写端时 EOF 永不到来，若无上限的 join 会把"宿主总超时"承诺变成永久阻塞。
pub(crate) const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// 限时回收线程：超限后放弃（JoinHandle 落地即 detach，读线程仍在排空管道并会在
/// 写端全部关闭后自行退出），返回 None。仅用于子进程已退出/被回收之后的收尾。
pub fn join_with_deadline<T>(handle: thread::JoinHandle<T>, limit: Duration) -> Option<T> {
    let deadline = Instant::now() + limit;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            tracing::warn!(
                component = "process",
                event = "thread_drain_abandoned",
                stage = "join",
                timeout_ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX),
                "子进程收尾线程超时，保留原有分离策略"
            );
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if let Ok(value) = handle.join() {
        Some(value)
    } else {
        tracing::error!(
            component = "process",
            event = "thread_join_failed",
            stage = "join",
            error_type = "thread_panic",
            "子进程收尾线程异常退出"
        );
        None
    }
}

/// 等待子进程时的取消来源：既支持旧工具链路的 [`Control`]，
/// 也支持转 Markdown 链路的裸 [`AtomicBool`](std::sync::atomic::AtomicBool)。
pub(super) enum CancelSource<'a> {
    Control(&'a Control),
    Atomic(&'a std::sync::atomic::AtomicBool),
}

impl CancelSource<'_> {
    fn is_cancelled(&self) -> bool {
        match self {
            Self::Control(control) => control.is_cancelled(),
            Self::Atomic(flag) => flag.load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// 回收子进程：kill 之后必须 wait（成对不变量，拆开执行会残留进程或句柄）。
pub(super) fn reap(child: &mut Child) {
    let started = Instant::now();
    tracing::warn!(
        event = "subprocess_kill_started",
        peer_pid = child.id(),
        "开始终止并回收子进程"
    );
    match child.kill() {
        Ok(()) => tracing::debug!(
            event = "subprocess_kill_completed",
            peer_pid = child.id(),
            "子进程终止请求已执行"
        ),
        Err(error) => {
            tracing::warn!(event = "subprocess_kill_failed", peer_pid = child.id(), error_type = ?error.kind(), error_code = error.raw_os_error(), "子进程终止请求失败，继续回收");
        }
    }
    match child.wait() {
        Ok(status) => tracing::debug!(
            event = "subprocess_reap_completed",
            peer_pid = child.id(),
            success = status.success(),
            exit_code = status.code(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "子进程已回收"
        ),
        Err(error) => {
            tracing::error!(event = "subprocess_reap_failed", peer_pid = child.id(), error_type = ?error.kind(), error_code = error.raw_os_error(), elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "子进程回收失败");
        }
    }
}

/// 带宿主侧总超时地运行命令并捕获全部输出。
/// 超时后回收进程；Windows 下所有后代也归入本次拥有的 kill-on-close Job。
pub fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<CapturedOutput> {
    run_with_timeout_ext(command, None, Some(timeout), None, MAX_CAPTURE_BYTES, false)
}

/// 有界捕获 stdout/stderr，但不设命令 deadline，也不在等待期间响应取消。
/// Git 等需要保留正在执行的写操作的调用方应在命令自然结束后自行检查停止标志。
pub fn run_to_completion(command: &mut Command) -> Result<CapturedOutput> {
    run_with_timeout_ext(command, None, None, None, MAX_CAPTURE_BYTES, false)
}

/// 同 [`run_with_timeout`]，另支持外部取消标志；预取消不启动命令，
/// 启动期间或等待期间取消则回收进程树并返回取消错误（转 Markdown 格式探测等使用）。
pub fn run_with_timeout_cancel(
    command: &mut Command,
    timeout: Duration,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<CapturedOutput> {
    run_with_timeout_ext(
        command,
        None,
        Some(timeout),
        Some(&CancelSource::Atomic(cancel)),
        MAX_CAPTURE_BYTES,
        false,
    )
}

/// 只读判型查询：使用调用方的单流捕获上限及任务取消来源。
/// Windows 下先暂停创建，再加入 kill-on-close Job 后恢复，保证后代也在回收边界内。
/// 任一流超限视为失败，不返回可被误当完整结果解析的截断输出。
pub fn run_with_timeout_control_limit(
    command: &mut Command,
    timeout: Duration,
    control: &Control,
    capture_limit: usize,
) -> Result<CapturedOutput> {
    if let Err(error) = control.check_cancelled() {
        let span = jchtools_foundation::logging::operation_span("process", "capture");
        let _entered = span.enter();
        tracing::info!(
            event = "subprocess_cancelled",
            stage = "pre_cancelled",
            state = "cancelled",
            elapsed_ms = 0,
            "操作已取消，未启动子进程"
        );
        return Err(error);
    }
    run_with_timeout_ext(
        command,
        None,
        Some(timeout),
        Some(&CancelSource::Control(control)),
        capture_limit,
        true,
    )
}

#[cfg(windows)]
pub(super) struct ProcessTreeJob(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl ProcessTreeJob {
    pub(super) fn attach_and_resume(child: &Child) -> Result<Self> {
        use anyhow::Context;
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::{
                Diagnostics::ToolHelp::{
                    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                    THREADENTRY32,
                },
                JobObjects::{
                    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                },
                Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
            },
        };
        // SAFETY: 无继承的未命名 Job；成功后将唯一句柄交给 OwnedHandle 关闭。
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(job_os_error("job_create")).context("无法创建查询进程组");
        }
        // SAFETY: 句柄有效且尚未转移所有权。
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle.cast()) });
        let handle = job.0.as_raw_handle().cast();
        // SAFETY: C POD 结构允许全零初始化，配置结构与尺寸配套。
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let limits_size = u32::try_from(std::mem::size_of_val(&limits))?;
        // SAFETY: 有效 Job 句柄与配套 POD 配置指针，尺寸已经受检转换。
        if unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                limits_size,
            )
        } == 0
        {
            return Err(job_os_error("job_configure")).context("无法配置查询进程组");
        }
        // SAFETY: 子进程仍处于 CREATE_SUSPENDED，尚不能创建未受 Job 管理的后代。
        if unsafe { AssignProcessToJobObject(handle, child.as_raw_handle().cast()) } == 0 {
            return Err(job_os_error("job_assign")).context("无法加入查询进程组");
        }
        // Rust 的主线程句柄接口尚不稳定；暂停创建的子进程只有一个初始线程，
        // 用系统线程快照取得它，加入 Job 后才恢复执行。
        // SAFETY: 只读系统线程快照，失败的 INVALID_HANDLE_VALUE 不交给 OwnedHandle。
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(job_os_error("thread_snapshot")).context("无法查询暂停进程的线程");
        }
        // SAFETY: 成功的快照句柄有效且尚未转移所有权。
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot.cast()) };
        // SAFETY: C POD 结构允许全零初始化，dwSize 声明缓冲区实际大小。
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = u32::try_from(std::mem::size_of_val(&entry))?;
        // SAFETY: 有效快照句柄及配套线程信息缓冲区。
        let mut present = unsafe { Thread32First(snapshot.as_raw_handle().cast(), &raw mut entry) };
        while present != 0 {
            if entry.th32OwnerProcessID == child.id() {
                // SAFETY: 快照中的线程属于暂停子进程，仅请求恢复权限且不继承句柄。
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(job_os_error("thread_open")).context("无法打开暂停进程的线程");
                }
                // SAFETY: 成功的线程句柄有效且尚未转移所有权。
                let thread = unsafe { OwnedHandle::from_raw_handle(thread.cast()) };
                // SAFETY: 有效初始线程句柄，子进程已加入拥有的 Job。
                if unsafe { ResumeThread(thread.as_raw_handle().cast()) } == u32::MAX {
                    return Err(job_os_error("thread_resume")).context("无法恢复查询进程");
                }
                return Ok(job);
            }
            entry.dwSize = u32::try_from(std::mem::size_of_val(&entry))?;
            // SAFETY: 有效快照句柄及配套线程信息缓冲区。
            present = unsafe { Thread32Next(snapshot.as_raw_handle().cast(), &raw mut entry) };
        }
        tracing::error!(
            event = "subprocess_job_failed",
            stage = "thread_lookup",
            error_type = "initial_thread_missing",
            "找不到暂停子进程的初始线程"
        );
        bail!("找不到暂停查询进程的初始线程");
    }
}

/// 以隐藏窗口 + SUSPENDED 状态创建子进程：统一 spawn 失败的日志与「无法启动」
/// 错误包装，成功后登记 peer_pid 并记录 spawn_completed。各入口的取消检查与
/// 管道装配留在调用方（两入口时机与阶段标签不同）。
pub(super) fn spawn_suspended(command: &mut Command) -> Result<Child> {
    // 与 run / run_with_idle_timeout 一致：GUI 下不弹控制台窗口。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 捕获策略不决定进程所有权：普通/Atomic 入口同样必须回收持管道的后代。
        command
            .creation_flags(0x0800_0000 | windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
    }
    // 错误必须带上程序名与系统原因：调用方（nettest/proxy）用 `to_string()` 展示，
    // 只取最外层文本；一旦只写"无法启动子进程"，用户就无从判断是哪个程序、为何失败。
    let child = command.spawn().map_err(|error| {
        tracing::error!(event = "spawn_failed", stage = "spawn", error_type = ?error.kind(), error_code = error.raw_os_error(), "子进程启动失败");
        anyhow::anyhow!(
            "无法启动 {}：{error}",
            command.get_program().to_string_lossy()
        )
    })?;
    tracing::Span::current().record("peer_pid", child.id());
    tracing::info!(
        event = "spawn_completed",
        peer_pid = child.id(),
        "子进程已创建"
    );
    Ok(child)
}

/// 恢复执行前把子进程加入本次拥有的 kill-on-close Job；挂接或恢复失败时回收
/// 子进程，保证 kill-on-close 回收不变量。`report_failure` 控制是否记录
/// subprocess_job_failed 日志：捕获入口（原路径本就记日志）传 true，
/// 流式入口（原路径静默 reap）传 false，两处日志行为与抽取前逐字一致。
#[cfg(windows)]
pub(super) fn attach_job_or_reap(
    child: &mut Child,
    report_failure: bool,
) -> Result<ProcessTreeJob> {
    match ProcessTreeJob::attach_and_resume(child) {
        Ok(job) => Ok(job),
        Err(error) => {
            if report_failure {
                tracing::error!(
                    event = "subprocess_job_failed",
                    stage = "job_attach_resume",
                    error_code = error
                        .downcast_ref::<std::io::Error>()
                        .and_then(std::io::Error::raw_os_error),
                    "子进程组加入或恢复失败"
                );
            }
            reap(child);
            Err(error)
        }
    }
}

pub(super) fn run_with_timeout_ext(
    command: &mut Command,
    stdin_data: Option<&[u8]>,
    timeout: Option<Duration>,
    cancel: Option<&CancelSource<'_>>,
    capture_limit: usize,
    reject_truncation: bool,
) -> Result<CapturedOutput> {
    let operation = jchtools_foundation::logging::operation_span("process", "capture");
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
        timeout_ms = timeout.map(|limit| u64::try_from(limit.as_millis()).unwrap_or(u64::MAX)),
        capture_limit,
        stdin_present = stdin_data.is_some(),
        "开始启动捕获子进程"
    );
    let result = run_with_timeout_inner(
        command,
        stdin_data,
        timeout,
        cancel,
        capture_limit,
        reject_truncation,
    );
    match &result {
        Ok(output) => tracing::info!(
            event = "subprocess_completed",
            success = output.status.success(),
            exit_code = output.status.code(),
            stdout_bytes = output.stdout.len(),
            stderr_bytes = output.stderr.len(),
            stdout_truncated = output.stdout_truncated,
            stderr_truncated = output.stderr_truncated,
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "捕获子进程执行结束"
        ),
        Err(_) if cancel.is_some_and(CancelSource::is_cancelled) => tracing::info!(
            event = "subprocess_cancelled",
            stage = "terminal",
            state = "cancelled",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "捕获子进程操作已取消"
        ),
        Err(_) => tracing::error!(
            event = "subprocess_failed",
            state = "failed",
            error_type = "subprocess_error",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "捕获子进程执行失败，详见关联阶段事件"
        ),
    }
    result
}

fn run_with_timeout_inner(
    command: &mut Command,
    stdin_data: Option<&[u8]>,
    timeout: Option<Duration>,
    cancel: Option<&CancelSource<'_>>,
    capture_limit: usize,
    reject_truncation: bool,
) -> Result<CapturedOutput> {
    if cancel.is_some_and(CancelSource::is_cancelled) {
        tracing::warn!(
            event = "subprocess_cancelled",
            stage = "before_spawn",
            "操作已取消，未启动子进程"
        );
        bail!("操作已取消，未启动子进程");
    }
    if stdin_data.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn_suspended(command)?;
    // spawn 是同步系统调用；若取消在创建期间到达，不再启动读写线程或恢复暂停进程。
    if cancel.is_some_and(CancelSource::is_cancelled) {
        tracing::warn!(
            event = "subprocess_cancelled",
            stage = "after_spawn",
            "创建期间收到取消，回收子进程"
        );
        reap(&mut child);
        bail!("操作已取消，子进程已终止");
    }
    #[cfg(windows)]
    let job = attach_job_or_reap(&mut child, true)?;
    if cancel.is_some_and(CancelSource::is_cancelled) {
        tracing::warn!(
            event = "subprocess_cancelled",
            stage = "after_resume",
            "恢复期间收到取消，回收子进程树"
        );
        #[cfg(windows)]
        {
            tracing::warn!(
                event = "subprocess_tree_release",
                reason = "cancelled",
                "关闭进程组，按既有策略回收子进程树"
            );
            drop(job);
        }
        reap(&mut child);
        bail!("操作已取消，子进程已终止");
    }

    // stdin 必须在独立线程写入：子进程可能在读完 stdin 前持续写 stdout。
    // 若在本线程同步 write_all，双方会分别卡在 stdin/stdout 管道满上形成互锁，
    // 永远进不了 wait_child_with_deadline，timeout 形同虚设，可无限期挂死。
    // 复制一份数据以满足线程的 'static 要求；写完后 drop stdin 发送 EOF。
    let stdin_owned = stdin_data.map(<[u8]>::to_vec);
    let stdin_span = tracing::Span::current();
    let stdin_dispatcher = tracing::dispatcher::get_default(Clone::clone);
    let stdin_thread = match (stdin_owned, child.stdin.take()) {
        (Some(data), Some(mut stdin)) => Some(thread::spawn(move || {
            let _dispatcher = tracing::dispatcher::set_default(&stdin_dispatcher);
            let _entered = stdin_span.enter();
            let result = stdin.write_all(&data).and_then(|()| stdin.flush());
            if let Err(error) = &result {
                tracing::warn!(event = "pipe_write_failed", stream = "stdin", error_type = ?error.kind(), error_code = error.raw_os_error(), "子进程输入管道写入失败，按退出状态判断结果");
            }
            drop(stdin);
            result
        })),
        (Some(_), None) => {
            tracing::error!(
                event = "pipe_missing",
                stream = "stdin",
                stage = "capture_setup",
                "子进程缺少输入管道，保持既有无写入行为"
            );
            None
        }
        _ => None,
    };

    // take 失败时必须 kill+wait 回收子进程，并等 stdin 线程结束，避免残留与悬挂。
    let Some(stdout_pipe) = child.stdout.take() else {
        tracing::error!(
            event = "pipe_missing",
            stream = "stdout",
            stage = "capture_setup",
            "子进程缺少输出管道"
        );
        reap(&mut child);
        if let Some(handle) = stdin_thread {
            if handle.join().is_err() {
                tracing::error!(
                    event = "thread_join_failed",
                    stream = "stdin",
                    stage = "capture_setup_cleanup",
                    error_type = "thread_panic",
                    "输入管道清理线程异常退出"
                );
            }
        }
        bail!("缺少 stdout");
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        tracing::error!(
            event = "pipe_missing",
            stream = "stderr",
            stage = "capture_setup",
            "子进程缺少错误管道"
        );
        reap(&mut child);
        drop(stdout_pipe);
        if let Some(handle) = stdin_thread {
            if handle.join().is_err() {
                tracing::error!(
                    event = "thread_join_failed",
                    stream = "stdin",
                    stage = "capture_setup_cleanup",
                    error_type = "thread_panic",
                    "输入管道清理线程异常退出"
                );
            }
        }
        bail!("缺少 stderr");
    };
    // 截断/读错误必须让调用方感知：超限时继续 drain 到 EOF（避免子进程写满管道卡死），
    // 旧入口仍只保留 MAX_CAPTURE_BYTES；判型入口超限时通知等待循环回收进程树。
    let exceeded =
        reject_truncation.then(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let stdout_exceeded = exceeded.clone();
    let stdout_span = tracing::Span::current();
    let stdout_dispatcher = tracing::dispatcher::get_default(Clone::clone);
    let stdout_thread = thread::spawn(move || {
        let _dispatcher = tracing::dispatcher::set_default(&stdout_dispatcher);
        let _entered = stdout_span.enter();
        let _stream = tracing::info_span!("pipe_capture", stream = "stdout").entered();
        match stdout_exceeded.as_deref() {
            Some(exceeded) => read_all_capped_ext(stdout_pipe, capture_limit, Some(exceeded)),
            None => read_all_capped(stdout_pipe, capture_limit),
        }
    });
    let stderr_exceeded = exceeded.clone();
    let stderr_span = tracing::Span::current();
    let stderr_dispatcher = tracing::dispatcher::get_default(Clone::clone);
    let stderr_thread = thread::spawn(move || {
        let _dispatcher = tracing::dispatcher::set_default(&stderr_dispatcher);
        let _entered = stderr_span.enter();
        let _stream = tracing::info_span!("pipe_capture", stream = "stderr").entered();
        match stderr_exceeded.as_deref() {
            Some(exceeded) => read_all_capped_ext(stderr_pipe, capture_limit, Some(exceeded)),
            None => read_all_capped(stderr_pipe, capture_limit),
        }
    });
    let status = match timeout {
        Some(timeout) => wait_child_with_deadline(&mut child, timeout, cancel, exceeded.as_deref()),
        None => child.wait().map_err(|error| {
            tracing::error!(event = "subprocess_wait_failed", peer_pid = child.id(), stage = "wait", error_type = ?error.kind(), error_code = error.raw_os_error(), "等待子进程自然结束失败");
            anyhow::anyhow!("等待子进程自然结束失败：{error}")
        }),
    };
    match status {
        Ok(status) => {
            tracing::info!(
                event = "subprocess_exited",
                peer_pid = child.id(),
                success = status.success(),
                exit_code = status.code(),
                "子进程已退出，开始收尾管道"
            );
            // 初始进程已退出仍可能有持管道的后代；先关闭 Job 再排空输出。
            #[cfg(windows)]
            {
                tracing::debug!(
                    event = "subprocess_tree_release",
                    reason = "root_exited",
                    "根进程已退出，关闭进程组以回收持管道后代"
                );
                drop(job);
            }
            // 读线程被放弃时输出不完整：如实标记截断，不得把半截输出当成完整结果。
            let drain_abandoned = |handle: thread::JoinHandle<ReadCapture>,
                                   stream: &'static str| {
                let _stream = tracing::info_span!("pipe_drain", stream).entered();
                join_with_deadline(handle, PIPE_DRAIN_GRACE).unwrap_or_else(|| ReadCapture {
                    truncated: true,
                    ..Default::default()
                })
            };
            let stdout = drain_abandoned(stdout_thread, "stdout");
            let stderr = drain_abandoned(stderr_thread, "stderr");
            if reject_truncation && (stdout.truncated || stderr.truncated) {
                tracing::error!(
                    event = "capture_rejected",
                    stage = "pipe_drain",
                    stdout_truncated = stdout.truncated,
                    stderr_truncated = stderr.truncated,
                    "子进程输出不完整，拒绝解析"
                );
                bail!("查询子进程输出超过捕获上限或管道未能排空，已拒绝解析");
            }
            // stdin 写入失败：子进程已成功退出时多半是提前关掉 stdin（EPIPE），不必判失败；
            // 子进程未成功时上报写入错误，便于定位管道问题。
            if let Some(handle) = stdin_thread {
                let _stream = tracing::info_span!("pipe_drain", stream = "stdin").entered();
                if let Some(Err(error)) = join_with_deadline(handle, PIPE_DRAIN_GRACE) {
                    if !status.success() {
                        tracing::error!(event = "subprocess_pipe_failed", stage = "stdin_write", error_type = ?error.kind(), error_code = error.raw_os_error(), "子进程失败且输入管道写入失败");
                        bail!("向子进程写入 stdin 失败：{error}");
                    }
                    tracing::warn!(
                        event = "pipe_write_failure_ignored",
                        stream = "stdin",
                        result = "child_success",
                        "子进程成功，保持提前关闭输入管道的既有容错"
                    );
                }
            }
            // 管道读失败必须上抛：静默吞掉会让调用方把半截输出当成完整结果。
            if let Some(error) = stdout.error {
                tracing::error!(
                    event = "subprocess_pipe_failed",
                    stage = "stdout_read",
                    error_type = "io_error",
                    "输出管道读取失败，拒绝不完整结果"
                );
                bail!("读取子进程 stdout 失败：{error}");
            }
            if let Some(error) = stderr.error {
                tracing::error!(
                    event = "subprocess_pipe_failed",
                    stage = "stderr_read",
                    error_type = "io_error",
                    "错误管道读取失败，拒绝不完整结果"
                );
                bail!("读取子进程 stderr 失败：{error}");
            }
            Ok(CapturedOutput {
                status,
                stdout: stdout.data,
                stderr: stderr.data,
                stdout_truncated: stdout.truncated,
                stderr_truncated: stderr.truncated,
            })
        }
        Err(error) => {
            // 超时/取消/等待失败：先关闭 Job 回收整树，再 wait 根进程并限时收尾读写线程。
            #[cfg(windows)]
            {
                tracing::warn!(
                    event = "subprocess_tree_release",
                    reason = "wait_failed",
                    "等待失败，关闭进程组以回收子进程树"
                );
                drop(job);
            }
            reap(&mut child);
            let _ = join_with_deadline(stdout_thread, PIPE_DRAIN_GRACE);
            let _ = join_with_deadline(stderr_thread, PIPE_DRAIN_GRACE);
            if let Some(handle) = stdin_thread {
                let _ = join_with_deadline(handle, PIPE_DRAIN_GRACE);
            }
            Err(error)
        }
    }
}

/// 有上限的整流捕获结果。
#[derive(Default)]
pub(crate) struct ReadCapture {
    pub(crate) data: Vec<u8>,
    pub(crate) truncated: bool,
    pub(crate) error: Option<String>,
}

/// 读满到 `limit` 后截断并继续 drain 到 EOF；读错误记入 `error`，不再静默丢弃。
pub(crate) fn read_all_capped<R: Read>(reader: R, limit: usize) -> ReadCapture {
    read_all_capped_ext(reader, limit, None)
}

fn read_all_capped_ext<R: Read>(
    mut reader: R,
    limit: usize,
    exceeded: Option<&std::sync::atomic::AtomicBool>,
) -> ReadCapture {
    let mut capture = ReadCapture {
        data: Vec::new(),
        truncated: false,
        error: None,
    };
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if capture.truncated {
                    // 已截断：继续丢弃读完，保证管道排空、子进程不会写满阻塞。
                    continue;
                }
                let room = limit.saturating_sub(capture.data.len());
                if n <= room {
                    capture.data.extend_from_slice(&chunk[..n]);
                } else {
                    capture.data.extend_from_slice(&chunk[..room]);
                    tracing::warn!(
                        event = "pipe_capture_truncated",
                        limit_bytes = limit,
                        "子进程输出超过捕获上限，继续排空管道"
                    );
                    capture.truncated = true;
                    if let Some(exceeded) = exceeded {
                        exceeded.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
            }
            Err(error) => {
                tracing::error!(event = "pipe_read_failed", error_type = ?error.kind(), error_code = error.raw_os_error(), "子进程管道读取失败");
                capture.error = Some(error.to_string());
                break;
            }
        }
    }
    capture
}

pub(super) fn wait_child_with_deadline(
    child: &mut Child,
    timeout: Duration,
    cancel: Option<&CancelSource<'_>>,
    exceeded: Option<&std::sync::atomic::AtomicBool>,
) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(source) = cancel {
            if source.is_cancelled() {
                tracing::warn!(
                    event = "subprocess_cancelled",
                    peer_pid = child.id(),
                    stage = "wait",
                    "等待期间收到取消"
                );
                bail!("操作已取消，子进程已终止");
            }
        }
        if exceeded.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
            tracing::error!(
                event = "capture_rejected",
                peer_pid = child.id(),
                stage = "wait",
                error_type = "capture_limit",
                "子进程输出超过捕获上限"
            );
            bail!("查询子进程输出超过捕获上限，已拒绝解析");
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    tracing::warn!(
                        event = "subprocess_timeout",
                        peer_pid = child.id(),
                        stage = "wait",
                        timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                        "等待子进程超过总超时"
                    );
                    bail!(
                        "子进程超过 {} 毫秒未结束，已按超时处理",
                        timeout.as_millis()
                    );
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                tracing::error!(event = "subprocess_wait_failed", peer_pid = child.id(), stage = "wait", error_type = ?error.kind(), error_code = error.raw_os_error(), "查询子进程退出状态失败");
                bail!("等待子进程状态失败：{error}");
            }
        }
    }
}
