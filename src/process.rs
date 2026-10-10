//! Bounded, cancellable process I/O; no shell and no unbounded Command::output buffers.
use crate::control::Control;
use anyhow::{bail, Result};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug)]
enum PipeMessage {
    Line(bool, String),
    Error(String),
}
/// 7-Zip 失败时 tail 里混着 stdout 的元数据（`Path = …`）和 stderr 的报错；
/// 只把这些当成"给用户看的原因"，否则用户拿到的是几十行条目字段。
fn is_metadata_line(line: &str) -> bool {
    const KEYS: [&str; 27] = [
        "Path",
        "Type",
        "Physical Size",
        "Headers Size",
        "Size",
        "Packed Size",
        "Modified",
        "Created",
        "Accessed",
        "Attributes",
        "Encrypted",
        "Comment",
        "CRC",
        "Method",
        "Characteristics",
        "Host OS",
        "Version",
        "Volume Index",
        "Folders",
        "Files",
        "Solid",
        "Blocks",
        "Hard Links",
        "Alternate Stream",
        "Symbolic Link",
        "Reparse",
        "Offset",
    ];
    line.split_once('=')
        .is_some_and(|(key, _)| KEYS.contains(&key.trim()))
}
fn summarize_failure(
    stderr: &std::collections::VecDeque<String>,
    stdout: &std::collections::VecDeque<String>,
) -> String {
    let pick = |lines: &std::collections::VecDeque<String>| -> Vec<String> {
        lines
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !is_metadata_line(line))
            .map(|line| {
                line.chars()
                    .take(240)
                    .collect::<String>()
                    .replace(r"\\?\", "")
            })
            .collect::<Vec<_>>()
    };
    let errors = pick(stderr);
    let chosen = if errors.is_empty() {
        pick(stdout)
    } else {
        errors
    };
    let mut text = chosen.join(" | ");
    if text.chars().count() > 400 {
        text = text.chars().take(400).collect::<String>() + "…";
    }
    text
}
fn pump<R: Read + Send + 'static>(
    reader: R,
    err: bool,
    sender: mpsc::SyncSender<PipeMessage>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut input = BufReader::with_capacity(65536, reader);
        let mut line = Vec::with_capacity(1024);
        // Windows 上 7-Zip 用 CRLF 输出；'\r' 和 '\n' 会各触发一次扫描，必须吃掉紧跟其后的 '\n'，
        // 否则每行后面都会多出一个空行，把 -slt 的条目元数据提前刷新成不完整字段。
        let mut skip_lf = false;
        loop {
            let (consume, terminated) = {
                let bytes = match input.fill_buf() {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        let _ = sender.send(PipeMessage::Error(e.to_string()));
                        break;
                    }
                };
                if bytes.is_empty() {
                    if !line.is_empty() {
                        let message = match String::from_utf8(std::mem::take(&mut line)) {
                            Ok(text) => PipeMessage::Line(err, text),
                            Err(_) => PipeMessage::Error("7-Zip 输出不是 UTF-8".into()),
                        };
                        let _ = sender.send(message);
                    }
                    break;
                }
                let skip = usize::from(skip_lf && bytes.first() == Some(&b'\n'));
                let rest = &bytes[skip..];
                if let Some(index) = rest.iter().position(|b| *b == b'\n' || *b == b'\r') {
                    line.extend_from_slice(&rest[..index]);
                    (skip + index + 1, Some(rest[index] == b'\r'))
                } else {
                    line.extend_from_slice(rest);
                    (bytes.len(), None)
                }
            };
            skip_lf = terminated == Some(true);
            input.consume(consume);
            if line.len() > 64 * 1024 {
                let _ = sender.send(PipeMessage::Error(
                    "7-Zip 输出行超过 64 KiB，已拒绝解析".into(),
                ));
                break;
            }
            if terminated.is_some() {
                let Ok(text) = String::from_utf8(std::mem::take(&mut line)) else {
                    let _ = sender.send(PipeMessage::Error(
                        "7-Zip 未返回有效 UTF-8，无法安全解析文件路径".into(),
                    ));
                    break;
                };
                if sender.send(PipeMessage::Line(err, text)).is_err() {
                    break;
                }
            }
        }
    })
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
pub(crate) fn join_with_deadline<T>(handle: thread::JoinHandle<T>, limit: Duration) -> Option<T> {
    let deadline = Instant::now() + limit;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
    handle.join().ok()
}

/// 等待子进程时的取消来源：既支持旧工具链路的 [`Control`]，
/// 也支持转 Markdown 链路的裸 [`AtomicBool`](std::sync::atomic::AtomicBool)。
enum CancelSource<'a> {
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
fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 带宿主侧总超时地运行命令并捕获全部输出。
/// 超时后回收进程；Windows 下所有后代也归入本次拥有的 kill-on-close Job。
pub fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<CapturedOutput> {
    run_with_timeout_ext(command, None, timeout, None, MAX_CAPTURE_BYTES, false)
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
        timeout,
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
    control.check_cancelled()?;
    run_with_timeout_ext(
        command,
        None,
        timeout,
        Some(&CancelSource::Control(control)),
        capture_limit,
        true,
    )
}

#[cfg(windows)]
struct ProcessTreeJob(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl ProcessTreeJob {
    fn attach_and_resume(child: &Child) -> Result<Self> {
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
            return Err(std::io::Error::last_os_error()).context("无法创建查询进程组");
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
            return Err(std::io::Error::last_os_error()).context("无法配置查询进程组");
        }
        // SAFETY: 子进程仍处于 CREATE_SUSPENDED，尚不能创建未受 Job 管理的后代。
        if unsafe { AssignProcessToJobObject(handle, child.as_raw_handle().cast()) } == 0 {
            return Err(std::io::Error::last_os_error()).context("无法加入查询进程组");
        }
        // Rust 的主线程句柄接口尚不稳定；暂停创建的子进程只有一个初始线程，
        // 用系统线程快照取得它，加入 Job 后才恢复执行。
        // SAFETY: 只读系统线程快照，失败的 INVALID_HANDLE_VALUE 不交给 OwnedHandle。
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error()).context("无法查询暂停进程的线程");
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
                    return Err(std::io::Error::last_os_error()).context("无法打开暂停进程的线程");
                }
                // SAFETY: 成功的线程句柄有效且尚未转移所有权。
                let thread = unsafe { OwnedHandle::from_raw_handle(thread.cast()) };
                // SAFETY: 有效初始线程句柄，子进程已加入拥有的 Job。
                if unsafe { ResumeThread(thread.as_raw_handle().cast()) } == u32::MAX {
                    return Err(std::io::Error::last_os_error()).context("无法恢复查询进程");
                }
                return Ok(job);
            }
            entry.dwSize = u32::try_from(std::mem::size_of_val(&entry))?;
            // SAFETY: 有效快照句柄及配套线程信息缓冲区。
            present = unsafe { Thread32Next(snapshot.as_raw_handle().cast(), &raw mut entry) };
        }
        bail!("找不到暂停查询进程的初始线程");
    }
}

fn run_with_timeout_ext(
    command: &mut Command,
    stdin_data: Option<&[u8]>,
    timeout: Duration,
    cancel: Option<&CancelSource<'_>>,
    capture_limit: usize,
    reject_truncation: bool,
) -> Result<CapturedOutput> {
    if cancel.is_some_and(CancelSource::is_cancelled) {
        bail!("操作已取消，未启动子进程");
    }
    if stdin_data.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
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
    let mut child = command.spawn().map_err(|error| {
        anyhow::anyhow!(
            "无法启动 {}：{error}",
            command.get_program().to_string_lossy()
        )
    })?;
    // spawn 是同步系统调用；若取消在创建期间到达，不再启动读写线程或恢复暂停进程。
    if cancel.is_some_and(CancelSource::is_cancelled) {
        reap(&mut child);
        bail!("操作已取消，子进程已终止");
    }
    #[cfg(windows)]
    let job = match ProcessTreeJob::attach_and_resume(&child) {
        Ok(job) => job,
        Err(error) => {
            reap(&mut child);
            return Err(error);
        }
    };
    if cancel.is_some_and(CancelSource::is_cancelled) {
        #[cfg(windows)]
        drop(job);
        reap(&mut child);
        bail!("操作已取消，子进程已终止");
    }

    // stdin 必须在独立线程写入：子进程可能在读完 stdin 前持续写 stdout。
    // 若在本线程同步 write_all，双方会分别卡在 stdin/stdout 管道满上形成互锁，
    // 永远进不了 wait_child_with_deadline，timeout 形同虚设，可无限期挂死。
    // 复制一份数据以满足线程的 'static 要求；写完后 drop stdin 发送 EOF。
    let stdin_owned = stdin_data.map(<[u8]>::to_vec);
    let stdin_thread = match (stdin_owned, child.stdin.take()) {
        (Some(data), Some(mut stdin)) => Some(thread::spawn(move || {
            let result = stdin.write_all(&data).and_then(|()| stdin.flush());
            drop(stdin);
            result
        })),
        _ => None,
    };

    // take 失败时必须 kill+wait 回收子进程，并等 stdin 线程结束，避免残留与悬挂。
    let Some(stdout_pipe) = child.stdout.take() else {
        reap(&mut child);
        if let Some(handle) = stdin_thread {
            let _ = handle.join();
        }
        bail!("缺少 stdout");
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        reap(&mut child);
        drop(stdout_pipe);
        if let Some(handle) = stdin_thread {
            let _ = handle.join();
        }
        bail!("缺少 stderr");
    };
    // 截断/读错误必须让调用方感知：超限时继续 drain 到 EOF（避免子进程写满管道卡死），
    // 旧入口仍只保留 MAX_CAPTURE_BYTES；判型入口超限时通知等待循环回收进程树。
    let exceeded =
        reject_truncation.then(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let stdout_exceeded = exceeded.clone();
    let stdout_thread = thread::spawn(move || match stdout_exceeded.as_deref() {
        Some(exceeded) => read_all_capped_ext(stdout_pipe, capture_limit, Some(exceeded)),
        None => read_all_capped(stdout_pipe, capture_limit),
    });
    let stderr_exceeded = exceeded.clone();
    let stderr_thread = thread::spawn(move || match stderr_exceeded.as_deref() {
        Some(exceeded) => read_all_capped_ext(stderr_pipe, capture_limit, Some(exceeded)),
        None => read_all_capped(stderr_pipe, capture_limit),
    });
    match wait_child_with_deadline(&mut child, timeout, cancel, exceeded.as_deref()) {
        Ok(status) => {
            // 初始进程已退出仍可能有持管道的后代；先关闭 Job 再排空输出。
            #[cfg(windows)]
            drop(job);
            // 读线程被放弃时输出不完整：如实标记截断，不得把半截输出当成完整结果。
            let drain_abandoned = |handle: thread::JoinHandle<ReadCapture>| {
                join_with_deadline(handle, PIPE_DRAIN_GRACE).unwrap_or_else(|| ReadCapture {
                    truncated: true,
                    ..Default::default()
                })
            };
            let stdout = drain_abandoned(stdout_thread);
            let stderr = drain_abandoned(stderr_thread);
            if reject_truncation && (stdout.truncated || stderr.truncated) {
                bail!("查询子进程输出超过捕获上限或管道未能排空，已拒绝解析");
            }
            // stdin 写入失败：子进程已成功退出时多半是提前关掉 stdin（EPIPE），不必判失败；
            // 子进程未成功时上报写入错误，便于定位管道问题。
            if let Some(handle) = stdin_thread {
                if let Some(Err(error)) = join_with_deadline(handle, PIPE_DRAIN_GRACE) {
                    if !status.success() {
                        bail!("向子进程写入 stdin 失败：{error}");
                    }
                }
            }
            // 管道读失败必须上抛：静默吞掉会让调用方把半截输出当成完整结果。
            if let Some(error) = stdout.error {
                bail!("读取子进程 stdout 失败：{error}");
            }
            if let Some(error) = stderr.error {
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
            drop(job);
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
                    capture.truncated = true;
                    if let Some(exceeded) = exceeded {
                        exceeded.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
            }
            Err(error) => {
                capture.error = Some(error.to_string());
                break;
            }
        }
    }
    capture
}

fn wait_child_with_deadline(
    child: &mut Child,
    timeout: Duration,
    cancel: Option<&CancelSource<'_>>,
    exceeded: Option<&std::sync::atomic::AtomicBool>,
) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(source) = cancel {
            if source.is_cancelled() {
                bail!("操作已取消，子进程已终止");
            }
        }
        if exceeded.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
            bail!("查询子进程输出超过捕获上限，已拒绝解析");
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    bail!(
                        "子进程超过 {} 毫秒未结束，已按超时处理",
                        timeout.as_millis()
                    );
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => bail!("等待子进程状态失败：{error}"),
        }
    }
}

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
    mut line: impl FnMut(bool, &str) -> Result<()>,
    mut tick: impl FnMut() -> Result<()>,
) -> Result<()> {
    control.checkpoint()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(|error| {
        anyhow::anyhow!(
            "无法启动 {}：{error}",
            command.get_program().to_string_lossy()
        )
    })?;
    let (send, recv) = mpsc::sync_channel(64);
    // spawn 成功后若 take 失败，必须 kill+wait 回收子进程，避免残留。
    let Some(stdout_pipe) = child.stdout.take() else {
        reap(&mut child);
        bail!("缺少 stdout");
    };
    let stdout = pump(stdout_pipe, false, send.clone());
    let Some(stderr_pipe) = child.stderr.take() else {
        reap(&mut child);
        drop(send);
        drop(recv);
        let _ = stdout.join();
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
            control.check_cancelled()?;
            if last_tick.elapsed() > Duration::from_millis(500) {
                tick()?;
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
                    line(err, &text)?;
                }
                Ok(PipeMessage::Error(error)) => bail!("{error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if last_output.elapsed() >= idle_timeout {
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
        if !status.success() {
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
    if result.is_err() {
        reap(&mut child);
    }
    drop(recv);
    // 7z 退出后管道应立即 EOF；限时收尾防止孙进程继承写端时 join 永久阻塞。
    let _ = join_with_deadline(stdout, PIPE_DRAIN_GRACE);
    let _ = join_with_deadline(stderr, PIPE_DRAIN_GRACE);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{collection, prelude::*};
    use std::collections::VecDeque;
    use std::sync::Arc;

    /// 关闭失败持久化：默认会在源码旁写 proptest-regressions 文件，违反仓库的 .tmp/ 规则；
    /// 失败用例由 panic 消息里的 minimal failing input 直接给出，无需落盘重放。
    fn config() -> ProptestConfig {
        let mut config = ProptestConfig::with_cases(256);
        config.failure_persistence = None;
        config
    }

    /// 行内容字符：允许任意非 CR/LF 字符（含中文、控制字符、等号等），
    /// 只排除会改变分行语义的两个字符本身。
    fn any_line() -> impl Strategy<Value = String> {
        collection::vec(
            any::<char>().prop_filter("排除 CR/LF", |c| *c != '\r' && *c != '\n'),
            0..16,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }
    /// 生成无歧义的（行内容, 行结束符种子）组合：最后一段非空（「末尾空行」与
    /// 「结尾终止符」在字节上不可区分），且排除「\r 分隔 + 空行 + \n 分隔」——
    /// 该组合会拼接成一个 CRLF（单终止符，这是正确语义），留着会把正确合并当吞行。
    /// 策略提到命名函数：proptest 语句糖的参数列表里出现闭包管道符会解析失败。
    fn unambiguous_lines_and_seps() -> impl Strategy<Value = (Vec<String>, Vec<u8>)> {
        (
            collection::vec(any_line(), 1..12).prop_filter("最后一段非空", |v| {
                v.last().is_some_and(|l| !l.is_empty())
            }),
            collection::vec(any::<u8>(), 12),
        )
            .prop_filter("排除跨空行合并歧义", |(lines, seps)| {
                (1..lines.len().saturating_sub(1)).all(|j| {
                    !(seps[j % seps.len()] % 3 == 2
                        && lines[j].is_empty()
                        && seps[(j + 1) % seps.len()] % 3 == 1)
                })
            })
    }

    // 属性化回归（2026-09-11 CRLF 缺陷）：任意 \r\n / \n / \r 混排分隔下，
    // 行内容与顺序必须原样保留，不得多出空行或吞行。
    proptest! {
        #![proptest_config(config())]
        #[test]
        fn pump_splits_on_any_line_ending_combination(combined in unambiguous_lines_and_seps()) {
            let (lines, seps) = combined;
            let mut data = String::new();
            for (index, line) in lines.iter().enumerate() {
                if index > 0 {
                    data.push_str(match seps[index % seps.len()] % 3 { 0 => "\r\n", 1 => "\n", _ => "\r" });
                }
                data.push_str(line);
            }
            let (send, recv) = mpsc::sync_channel(1024);
            let handle = pump(std::io::Cursor::new(data.clone().into_bytes()), false, send);
            handle.join().unwrap();
            let mut seen = Vec::new();
            while let Ok(message) = recv.try_recv() {
                match message {
                    PipeMessage::Line(err, text) => { assert!(!err); seen.push(text); }
                    PipeMessage::Error(error) => panic!("合法 UTF-8 输入不应报错：{error}"),
                }
            }
            prop_assert_eq!(seen, lines, "输入：{:?}", data);
        }

        // 健壮性下界：任意字节序列（含非法 UTF-8、NUL、超长无换行）都不得让 pump panic。
        #[test]
        fn pump_never_panics_on_arbitrary_bytes(data in collection::vec(any::<u8>(), 0..4096)) {
            let (send, recv) = mpsc::sync_channel(1024);
            let handle = pump(std::io::Cursor::new(data.clone()), false, send);
            handle.join().unwrap(); // 线程内 panic 会在此暴露
            while recv.try_recv().is_ok() {}
        }
    }

    /// is_metadata_line：空行/普通错误行不是元数据；-slt 的 Path/Type/… 字段是元数据。
    #[test]
    fn is_metadata_line_classifies_common_lines() {
        assert!(!is_metadata_line(""));
        assert!(!is_metadata_line("   "));
        assert!(!is_metadata_line("ERROR: Cannot open the file as archive"));
        assert!(!is_metadata_line(
            "7-Zip [64] 23.01: Copyright (c) 1999-2023 Igor Pavlov"
        ));
        assert!(!is_metadata_line("foo = bar"), "非白名单键不得当成元数据");
        assert!(is_metadata_line("Path = archive.7z"));
        assert!(is_metadata_line("Type = 7z"));
        assert!(is_metadata_line("Physical Size = 12345"));
        assert!(is_metadata_line("Method = LZMA2:19"));
        assert!(
            is_metadata_line(" Path = leading-space-key"),
            "键两侧空白应被 trim"
        );
        // 超长行：只做键匹配，不得 panic，也不得把普通长行误判为元数据
        let long_meta = format!("Path = {}", "a".repeat(100_000));
        assert!(is_metadata_line(&long_meta));
        let long_plain = format!("{} = value", "x".repeat(10_000));
        assert!(!is_metadata_line(&long_plain));
    }

    /// summarize_failure：优先 stderr、过滤元数据、单行/总长截断、空输入返回空串。
    #[test]
    fn summarize_failure_prefers_stderr_and_truncates() {
        let mut stderr = VecDeque::new();
        let mut stdout = VecDeque::new();
        stdout.push_back("stdout only error".into());
        stderr.push_back("Path = x".into()); // 元数据必须被过滤
        stderr.push_back("ERROR: Cannot open file".into());
        let summary = summarize_failure(&stderr, &stdout);
        assert!(summary.contains("ERROR: Cannot open file"));
        assert!(
            !summary.contains("stdout only error"),
            "stderr 有可用行时不回退 stdout"
        );
        assert!(!summary.contains("Path ="), "元数据行不应进入用户可见摘要");

        // stderr 全是元数据时才回退 stdout
        let mut stderr = VecDeque::new();
        stderr.push_back("Type = 7z".into());
        let mut stdout = VecDeque::new();
        stdout.push_back("Open ERROR: invalid archive".into());
        let summary = summarize_failure(&stderr, &stdout);
        assert!(summary.contains("Open ERROR: invalid archive"));

        // 空输入
        assert_eq!(summarize_failure(&VecDeque::new(), &VecDeque::new()), "");

        // 总长截断：多行拼接超过 400 字符时截断并加省略号（单行已在 pick 阶段截到 240）
        let mut stderr = VecDeque::new();
        stderr.push_back("A".repeat(240));
        stderr.push_back("B".repeat(240));
        let summary = summarize_failure(&stderr, &VecDeque::new());
        assert_eq!(summary.chars().count(), 401, "400 字符 + 省略号");
        assert!(summary.ends_with('…'));

        // 单行截断到 240 字符
        let mut stderr = VecDeque::new();
        stderr.push_back("W".repeat(300));
        let summary = summarize_failure(&stderr, &VecDeque::new());
        assert_eq!(summary.chars().count(), 240);
    }

    /// pump：CRLF/LF 混排、无结尾换行、非 UTF-8 行的错误上报。
    #[test]
    fn pump_handles_crlf_lf_and_trailing_line() {
        let (send, recv) = mpsc::sync_channel(64);
        let data = b"line1\r\nline2\nline3".to_vec();
        let handle = pump(std::io::Cursor::new(data), false, send);
        handle.join().unwrap();
        let mut lines = Vec::new();
        while let Ok(message) = recv.try_recv() {
            match message {
                PipeMessage::Line(err, text) => {
                    assert!(!err);
                    lines.push(text);
                }
                PipeMessage::Error(error) => panic!("不应出现错误：{error}"),
            }
        }
        assert_eq!(lines, vec!["line1", "line2", "line3"]);
    }

    // 覆盖 X-08（无法解码的文件名必须报错，不得静默落盘）
    #[test]
    fn pump_reports_non_utf8_as_error() {
        let (send, recv) = mpsc::sync_channel(64);
        let data = vec![0xff, 0xfe, b'\n'];
        let handle = pump(std::io::Cursor::new(data), true, send);
        handle.join().unwrap();
        let mut saw_error = false;
        while let Ok(message) = recv.try_recv() {
            if let PipeMessage::Error(error) = message {
                assert!(error.contains("UTF-8"), "{error}");
                saw_error = true;
            }
        }
        assert!(saw_error, "非 UTF-8 行必须上报错误，不得静默丢弃");
    }

    /// read_all_capped：未超限完整读取；超限截断并标记；读错误传播。
    #[test]
    fn read_all_capped_truncates_and_reports_errors() {
        // 未超限
        let capture = read_all_capped(std::io::Cursor::new(b"hello".to_vec()), 64);
        assert_eq!(capture.data, b"hello");
        assert!(!capture.truncated);
        assert!(capture.error.is_none());

        // 超限：只保留前 limit 字节并标记 truncated，继续 drain 不报错
        let capture = read_all_capped(std::io::Cursor::new(vec![b'x'; 100]), 10);
        assert_eq!(capture.data.len(), 10);
        assert!(capture.truncated);
        assert!(capture.error.is_none());

        // 恰好等于 limit：不算截断
        let capture = read_all_capped(std::io::Cursor::new(vec![b'y'; 10]), 10);
        assert_eq!(capture.data.len(), 10);
        assert!(!capture.truncated);

        // 读错误必须记录，不得吞掉
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("模拟读错误"))
            }
        }
        let capture = read_all_capped(FailingReader, 64);
        assert!(capture
            .error
            .as_deref()
            .is_some_and(|e| e.contains("模拟读错误")));
        assert!(capture.data.is_empty());
    }

    /// 空闲超时测试用的静默单进程：避免 cmd 外壳把管道句柄传给孙进程导致 join 挂死。
    #[cfg(windows)]
    fn silent_hung_command() -> Command {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"]);
        c
    }
    #[cfg(not(windows))]
    fn silent_hung_command() -> Command {
        let mut c = Command::new("sleep");
        c.arg("60");
        c
    }

    /// run_with_idle_timeout：连续无输出超过 idle 上限时应 kill 并返回错误，不得无限阻塞。
    #[test]
    fn run_idle_timeout_kills_silent_child() {
        let mut command = silent_hung_command();
        let control = Control::default();
        let start = Instant::now();
        let result = run_with_idle_timeout(
            &mut command,
            &control,
            Duration::from_millis(300),
            |_, _| Ok(()),
            || Ok(()),
        );
        let elapsed = start.elapsed();
        assert!(result.is_err(), "空闲超时应返回错误");
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("无输出") || message.contains("挂死"),
            "错误应说明空闲超时：{message}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "应在空闲超时后尽快返回，实际 {elapsed:?}"
        );
    }

    /// run_with_idle_timeout：取消路径仍可用——先启动再 cancel，应返回取消错误。
    // 覆盖 C-10（用户取消在安全边界生效）
    #[test]
    fn run_idle_timeout_supports_cancel() {
        let mut command = silent_hung_command();
        let control = Arc::new(Control::default());
        let ctl = control.clone();
        let cancel_after = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            ctl.cancel();
        });
        let result = run_with_idle_timeout(
            &mut command,
            control.as_ref(),
            Duration::from_secs(60),
            |_, _| Ok(()),
            || Ok(()),
        );
        let _ = cancel_after.join();
        assert!(result.is_err(), "取消后应返回错误");
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("取消"), "应是取消错误：{message}");
    }

    // 覆盖 F21：总超时到达时 kill+wait 并限时收尾读线程，不得永久阻塞。
    #[test]
    fn run_with_timeout_kills_hung_child() {
        let mut command = silent_hung_command();
        let started = Instant::now();
        let result = run_with_timeout(&mut command, Duration::from_millis(300));
        assert!(result.is_err(), "挂死子进程应按总超时失败");
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("超时"), "错误应说明超时：{message}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "超时后应尽快返回，实际 {:?}",
            started.elapsed()
        );
    }

    // 覆盖 F21：外部取消标志在等待期间生效，先于总超时返回。
    #[test]
    fn run_with_timeout_cancel_stops_promptly() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Atomic 入口也必须在 spawn 前检查；不存在的程序不能掩盖预取消错误。
        let pre_cancelled = AtomicBool::new(true);
        let error = run_with_timeout_cancel(
            &mut Command::new("jchtools-must-not-spawn-pre-cancelled.exe"),
            Duration::from_secs(60),
            &pre_cancelled,
        )
        .unwrap_err();
        assert!(error.to_string().contains("取消"), "{error}");

        let mut command = silent_hung_command();
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        let cancel_after = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            let cancelled_at = Instant::now();
            setter.store(true, Ordering::Relaxed);
            cancelled_at
        });
        let started = Instant::now();
        let result = run_with_timeout_cancel(&mut command, Duration::from_secs(60), &cancel);
        let finished = Instant::now();
        let cancelled_at = cancel_after.join().unwrap();
        assert!(result.is_err(), "取消后应返回错误");
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("取消"), "应是取消错误：{message}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "取消后应尽快返回，总耗时 {:?}，取消至返回 {:?}",
            started.elapsed(),
            finished.saturating_duration_since(cancelled_at)
        );

        // 再从真实根/后代 PID 握手之后取消，单独测取消到整树释放；不替代上面的原 5s 断言。
        // 修复前 Atomic 入口只杀根进程，-NoNewWindow 后代继续持有 stdout/stderr 写端。
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
            use windows_sys::Win32::{
                Foundation::WAIT_OBJECT_0,
                System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
            };

            std::fs::create_dir_all(".tmp/parallel-review").unwrap();
            let dir = tempfile::Builder::new()
                .prefix("atomic-process-tree-")
                .tempdir_in(".tmp/parallel-review")
                .unwrap();
            let pid_path = dir.path().join("ready.pid");
            let path = pid_path.to_string_lossy().replace('\'', "''");
            let script = format!(
                "$child = Start-Process -FilePath \"$PSHOME\\powershell.exe\" \
                 -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 60' \
                 -NoNewWindow -PassThru; \
                 [IO.File]::WriteAllText('{path}', \"$PID $($child.Id)\"); \
                 Start-Sleep -Seconds 60"
            );
            let mut command = Command::new("powershell");
            command.args(["-NoProfile", "-Command", &script]);
            let cancel = Arc::new(AtomicBool::new(false));
            let setter = cancel.clone();
            let cancel_thread = thread::spawn(move || {
                let ready = (|| -> std::result::Result<[OwnedHandle; 2], String> {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        if let Ok(text) = std::fs::read_to_string(&pid_path) {
                            let mut pids = text.split_whitespace();
                            if let (Some(root), Some(descendant), None) =
                                (pids.next(), pids.next(), pids.next())
                            {
                                if let (Ok(root), Ok(descendant)) =
                                    (root.parse::<u32>(), descendant.parse::<u32>())
                                {
                                    let open = |pid| {
                                        // SAFETY: 只打开本用例已握手进程的同步句柄，不继承或改状态。
                                        let handle =
                                            unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
                                        if handle.is_null() {
                                            return Err(format!(
                                                "取消前无法打开合成进程 {pid}：{}",
                                                std::io::Error::last_os_error()
                                            ));
                                        }
                                        // SAFETY: 成功句柄的唯一所有权交给 OwnedHandle，防 PID 复用。
                                        Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
                                    };
                                    return Ok([open(root)?, open(descendant)?]);
                                }
                            }
                        }
                        if Instant::now() >= deadline {
                            return Err("合成根进程及后代未完成真实 PID 握手".into());
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                })();
                // 即使握手失败也先发取消并由宿主回收，避免失败断言留下挂死进程。
                let cancelled_at = Instant::now();
                setter.store(true, Ordering::Relaxed);
                (cancelled_at, ready)
            });
            let result = run_with_timeout_cancel(&mut command, Duration::from_secs(60), &cancel);
            let (cancelled_at, ready) = cancel_thread.join().unwrap();
            let error = result.expect_err("取消真实进程树必须返回错误");
            assert!(error.to_string().contains("取消"), "{error}");
            for handle in ready.expect("取消前必须确认根进程和后代已经真实启动")
            {
                // SAFETY: 仅有界等待取消前持有的合成进程同步句柄。
                let status = unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), 2000) };
                assert_eq!(status, WAIT_OBJECT_0, "取消后根进程或后代仍存活");
            }
            assert!(
                cancelled_at.elapsed() < Duration::from_secs(5),
                "取消到整树释放应保留 5s 上限，实际 {:?}",
                cancelled_at.elapsed()
            );
        }
    }

    // 覆盖 F21：超过捕获上限的输出标记 truncated，不得静默当完整结果。
    #[cfg(windows)]
    #[test]
    fn run_with_timeout_marks_truncated_huge_output() {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-Command",
            "[Console]::Out.Write(('x' * 9437184))",
        ]);
        let output = run_with_timeout(&mut command, Duration::from_secs(60))
            .expect("大输出进程正常退出应成功");
        assert!(output.stdout_truncated, "stdout 超限必须标记截断");
        assert!(output.truncation_note().is_some());

        // C-08：只读判型入口必须支持预取消、双流独立上限及成功查询。
        let cancelled = Control::default();
        cancelled.cancel();
        let error = run_with_timeout_control_limit(
            &mut Command::new("jchtools-must-not-spawn-pre-cancelled.exe"),
            Duration::from_secs(10),
            &cancelled,
            64 * 1024,
        )
        .unwrap_err();
        assert!(error.to_string().contains("取消"), "{error}");
        for stream in ["Out", "Error"] {
            let mut command = Command::new("powershell");
            command.args([
                "-NoProfile",
                "-Command",
                &format!("[Console]::{stream}.Write(('x' * 65537)); Start-Sleep -Seconds 30"),
            ]);
            let error = run_with_timeout_control_limit(
                &mut command,
                Duration::from_secs(10),
                &Control::default(),
                64 * 1024,
            )
            .unwrap_err();
            assert!(error.to_string().contains("捕获上限"), "{stream}: {error}");
        }
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-Command", "[Console]::Out.Write('ok')"]);
        let output = run_with_timeout_control_limit(
            &mut command,
            Duration::from_secs(10),
            &Control::default(),
            64 * 1024,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ok");
        assert!(!output.stdout_truncated && !output.stderr_truncated);

        // C-08：真实后代不能在取消、超时或超限返回后仍存活。
        // 仅在开发临时区内生成公开合成 PID 握手，不读取用户数据或清理其他临时目录。
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0,
            System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        };
        std::fs::create_dir_all(".tmp/parallel-review").unwrap();
        for mode in ["取消", "超时", "捕获上限"] {
            let dir = tempfile::Builder::new()
                .prefix("process-tree-")
                .tempdir_in(".tmp/parallel-review")
                .unwrap();
            let pid_path = dir.path().join("child.pid");
            let path = pid_path.to_string_lossy().replace('\'', "''");
            let ending = if mode == "捕获上限" {
                "[Console]::Out.Write(('x' * 65537)); Start-Sleep -Seconds 30"
            } else {
                "Start-Sleep -Seconds 30"
            };
            let script = format!(
                "$child = Start-Process -FilePath \"$PSHOME\\powershell.exe\" \
                 -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 30' \
                 -NoNewWindow -PassThru; \
                 [IO.File]::WriteAllText('{path}', [string]$child.Id); {ending}"
            );
            let control = Arc::new(Control::default());
            let cancel_thread = if mode == "取消" {
                let control = control.clone();
                let path = pid_path.clone();
                Some(thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while Instant::now() < deadline {
                        if std::fs::read_to_string(&path)
                            .ok()
                            .and_then(|pid| pid.trim().parse::<u32>().ok())
                            .is_some()
                        {
                            break;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    control.cancel();
                }))
            } else {
                None
            };
            let mut command = Command::new("powershell");
            command.args(["-NoProfile", "-Command", &script]);
            let error = run_with_timeout_control_limit(
                &mut command,
                Duration::from_secs(if mode == "取消" { 10 } else { 5 }),
                &control,
                64 * 1024,
            )
            .unwrap_err();
            if let Some(handle) = cancel_thread {
                handle.join().unwrap();
            }
            assert!(error.to_string().contains(mode), "{mode}: {error}");
            let pid: u32 = std::fs::read_to_string(&pid_path)
                .expect("后代必须真实启动并写入握手")
                .trim()
                .parse()
                .unwrap();
            // SAFETY: 仅打开本用例合成后代的同步句柄，不继承、不修改其状态。
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            if handle.is_null() {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER.cast_signed()),
                    "只能将后代 PID 已消失视为退出，不能把权限失败当成成功"
                );
            } else {
                // SAFETY: 有效同步句柄唯一所有权交给 OwnedHandle。
                let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
                // SAFETY: 仅有界等待本用例后代的有效同步句柄，不修改进程状态。
                let status = unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), 2000) };
                assert_eq!(status, WAIT_OBJECT_0, "{mode}后后代仍存活");
            }
        }
    }
}
