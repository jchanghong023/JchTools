//! Bounded, cancellable process I/O; no shell and no unbounded Command::output buffers.
use crate::control::Control;
use anyhow::{bail, Result};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug)]
enum PipeMessage { Line(bool,String), Error(String) }
/// 7-Zip 失败时 tail 里混着 stdout 的元数据（`Path = …`）和 stderr 的报错；
/// 只把这些当成"给用户看的原因"，否则用户拿到的是几十行条目字段。
fn is_metadata_line(line: &str) -> bool {
    const KEYS: [&str; 27] = ["Path","Type","Physical Size","Headers Size","Size","Packed Size","Modified","Created","Accessed",
        "Attributes","Encrypted","Comment","CRC","Method","Characteristics","Host OS","Version","Volume Index","Folders","Files",
        "Solid","Blocks","Hard Links","Alternate Stream","Symbolic Link","Reparse","Offset"];
    line.split_once('=').is_some_and(|(key,_)| KEYS.contains(&key.trim()))
}
fn summarize_failure(stderr: &std::collections::VecDeque<String>, stdout: &std::collections::VecDeque<String>) -> String {
    let pick = |lines: &std::collections::VecDeque<String>| -> Vec<String> {
        lines.iter().map(|line| line.trim()).filter(|line| !line.is_empty() && !is_metadata_line(line))
            .map(|line| line.chars().take(240).collect::<String>().replace(r"\\?\", "")).collect::<Vec<_>>()
    };
    let errors = pick(stderr);
    let chosen = if !errors.is_empty() { errors } else { pick(stdout) };
    let mut text = chosen.join(" | ");
    if text.chars().count() > 400 { text = text.chars().take(400).collect::<String>() + "…"; }
    text
}
fn pump<R: Read + Send + 'static>(reader: R, err: bool, sender: mpsc::SyncSender<PipeMessage>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut input = BufReader::with_capacity(65536,reader);
        let mut line = Vec::with_capacity(1024);
        // Windows 上 7-Zip 用 CRLF 输出；'\r' 和 '\n' 会各触发一次扫描，必须吃掉紧跟其后的 '\n'，
        // 否则每行后面都会多出一个空行，把 -slt 的条目元数据提前刷新成不完整字段。
        let mut skip_lf = false;
        loop {
            let (consume, terminated) = {
                let bytes = match input.fill_buf() { Ok(bytes) => bytes, Err(e) => { let _ = sender.send(PipeMessage::Error(e.to_string())); break; } };
                if bytes.is_empty() {
                    if !line.is_empty() {
                        let message = match String::from_utf8(std::mem::take(&mut line)) { Ok(text)=>PipeMessage::Line(err,text),Err(_)=>PipeMessage::Error("7-Zip 输出不是 UTF-8".into()) };
                        let _ = sender.send(message);
                    }
                    break;
                }
                let skip = if skip_lf && bytes.first() == Some(&b'\n') { 1 } else { 0 };
                let rest = &bytes[skip..];
                match rest.iter().position(|b| *b == b'\n' || *b == b'\r') {
                    Some(index) => { line.extend_from_slice(&rest[..index]); (skip + index + 1, Some(rest[index] == b'\r')) }
                    None => { line.extend_from_slice(rest); (bytes.len(), None) }
                }
            };
            skip_lf = terminated == Some(true);
            input.consume(consume);
            if line.len() > 64 * 1024 { let _ = sender.send(PipeMessage::Error("7-Zip 输出行超过 64 KiB，已拒绝解析".into())); break; }
            if terminated.is_some() {
                let text = match String::from_utf8(std::mem::take(&mut line)) { Ok(s) => s,
                    Err(_) => { let _ = sender.send(PipeMessage::Error("7-Zip 未返回有效 UTF-8，无法安全解析文件路径".into())); break; } };
                if sender.send(PipeMessage::Line(err,text)).is_err() { break; }
            }
        }
    })
}
/// Windows 系统工具绝对路径（`%SystemRoot%\System32\<name>`）。
/// 避免按短名启动时被 PATH 中的同名伪造程序劫持。
/// `name` 可含子目录，例如 `WindowsPowerShell\v1.0\powershell.exe`。
#[cfg(windows)]
pub fn system_tool(name: &str) -> PathBuf {
    // 空串与缺失等价：否则会得到相对路径 `System32\...`，可被 CWD 劫持。
    let root = match std::env::var("SystemRoot") {
        Ok(root) if !root.trim().is_empty() => root,
        _ => "C:\\Windows".into(),
    };
    PathBuf::from(root).join("System32").join(name)
}

#[cfg(not(windows))]
pub fn system_tool(name: &str) -> PathBuf {
    PathBuf::from(name)
}

/// 单流捕获上限：nettest/proxy 等调用方的正常输出很小；
/// 超过上限说明输出异常膨胀，截断保存并标记 truncated，避免内存无界增长。
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;

/// 捕获到的子进程输出（只读查询用；无 shell）。
pub struct CapturedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// stdout 是否因超过 [`MAX_CAPTURE_BYTES`] 被截断
    pub stdout_truncated: bool,
    /// stderr 是否因超过 [`MAX_CAPTURE_BYTES`] 被截断
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
            "子进程{which}输出不完整（超过 {} MiB 捕获上限被截断，或子进程退出后管道未能排空、读取超时被放弃）",
            MAX_CAPTURE_BYTES / (1024 * 1024)
        ))
    }
}

/// 子进程退出后等待管道读线程收尾的宽限：正常情况下进程退出管道随即 EOF，读线程几乎立刻结束；
/// 孙进程继承管道写端时 EOF 永不到来，若无上限的 join 会把"宿主总超时"承诺变成永久阻塞。
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// 限时回收线程：超限后放弃（JoinHandle 落地即 detach，读线程仍在排空管道并会在
/// 写端全部关闭后自行退出），返回 None。仅用于子进程已退出/被回收之后的收尾。
fn join_with_deadline<T>(handle: thread::JoinHandle<T>, limit: Duration) -> Option<T> {
    let deadline = Instant::now() + limit;
    while !handle.is_finished() {
        if Instant::now() >= deadline { return None; }
        thread::sleep(Duration::from_millis(10));
    }
    handle.join().ok()
}

/// 带宿主侧总超时地运行命令并捕获全部输出。
/// 超时后 kill + wait，避免子进程卡死导致永久阻塞。
pub fn run_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> Result<CapturedOutput> {
    run_with_timeout_input(command, None, timeout)
}

/// 同 [`run_with_timeout`]，可选写入 stdin 后再关闭管道（供 `bash -s` 类脚本）。
pub fn run_with_timeout_input(
    command: &mut Command,
    stdin_data: Option<&[u8]>,
    timeout: Duration,
) -> Result<CapturedOutput> {
    if stdin_data.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    // 与 run / run_with_idle_timeout 一致：GUI 下不弹控制台窗口。
    #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
    // 错误必须带上程序名与系统原因：调用方（nettest/proxy）用 `to_string()` 展示，
    // 只取最外层文本；一旦只写"无法启动子进程"，用户就无从判断是哪个程序、为何失败。
    let mut child = command.spawn().map_err(|error| {
        anyhow::anyhow!("无法启动 {}：{error}", command.get_program().to_string_lossy())
    })?;

    // stdin 必须在独立线程写入：子进程可能在读完 stdin 前持续写 stdout。
    // 若在本线程同步 write_all，双方会分别卡在 stdin/stdout 管道满上形成互锁，
    // 永远进不了 wait_child_with_deadline，timeout 形同虚设，可无限期挂死。
    // 复制一份数据以满足线程的 'static 要求；写完后 drop stdin 发送 EOF。
    let stdin_owned = stdin_data.map(|data| data.to_vec());
    let stdin_thread = match (stdin_owned, child.stdin.take()) {
        (Some(data), Some(mut stdin)) => Some(thread::spawn(move || {
            let result = stdin.write_all(&data).and_then(|_| stdin.flush());
            drop(stdin);
            result
        })),
        _ => None,
    };

    // take 失败时必须 kill+wait 回收子进程，并等 stdin 线程结束，避免残留与悬挂。
    let Some(stdout_pipe) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        if let Some(handle) = stdin_thread { let _ = handle.join(); }
        bail!("缺少 stdout");
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        drop(stdout_pipe);
        if let Some(handle) = stdin_thread { let _ = handle.join(); }
        bail!("缺少 stderr");
    };
    // 截断/读错误必须让调用方感知：超限时继续 drain 到 EOF（避免子进程写满管道卡死），
    // 但只保留前 MAX_CAPTURE_BYTES；读错误在最终结果里传播，不再用 let _ 吞掉。
    let stdout_thread = thread::spawn(move || read_all_capped(stdout_pipe, MAX_CAPTURE_BYTES));
    let stderr_thread = thread::spawn(move || read_all_capped(stderr_pipe, MAX_CAPTURE_BYTES));
    match wait_child_with_deadline(&mut child, timeout, None) {
        Ok(status) => {
            // 读线程被放弃时输出不完整：如实标记截断，不得把半截输出当成完整结果。
            let stdout = join_with_deadline(stdout_thread, PIPE_DRAIN_GRACE)
                .unwrap_or_else(|| ReadCapture { truncated: true, ..Default::default() });
            let stderr = join_with_deadline(stderr_thread, PIPE_DRAIN_GRACE)
                .unwrap_or_else(|| ReadCapture { truncated: true, ..Default::default() });
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
            // 超时/等待失败：先确保子进程回收，再限时收尾读线程（孙进程持写端时按放弃处理）。
            let _ = child.kill();
            let _ = child.wait();
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
struct ReadCapture {
    data: Vec<u8>,
    truncated: bool,
    error: Option<String>,
}

/// 读满到 `limit` 后截断并继续 drain 到 EOF；读错误记入 `error`，不再静默丢弃。
fn read_all_capped<R: Read>(mut reader: R, limit: usize) -> ReadCapture {
    let mut capture = ReadCapture { data: Vec::new(), truncated: false, error: None };
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

fn wait_child_with_deadline(child: &mut Child, timeout: Duration, control: Option<&Control>) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(control) = control {
            if control.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                bail!("操作已取消，子进程已终止");
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    bail!("子进程超过 {} 毫秒未结束，已按超时处理", timeout.as_millis());
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
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub fn run(command: &mut Command, control: &Control, line: impl FnMut(bool,&str)->Result<()>, tick: impl FnMut()->Result<()>) -> Result<()> {
    run_with_idle_timeout(command, control, DEFAULT_IDLE_TIMEOUT, line, tick)
}

/// 同 [`run`]，可自定义连续无输出空闲上限；超过则 kill 子进程并返回错误。
/// 调用方取消（`Control`）路径不受影响，仍可随时中断。
pub fn run_with_idle_timeout(
    command: &mut Command,
    control: &Control,
    idle_timeout: Duration,
    mut line: impl FnMut(bool,&str)->Result<()>,
    mut tick: impl FnMut()->Result<()>,
) -> Result<()> {
    control.checkpoint()?;
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
    let mut child = command.spawn().map_err(|error| {
        anyhow::anyhow!("无法启动 {}：{error}", command.get_program().to_string_lossy())
    })?;
    let (send,recv) = mpsc::sync_channel(64);
    // spawn 成功后若 take 失败，必须 kill+wait 回收子进程，避免残留。
    let stdout_pipe = match child.stdout.take() {
        Some(pipe) => pipe,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("缺少 stdout");
        }
    };
    let stdout = pump(stdout_pipe,false,send.clone());
    let stderr_pipe = match child.stderr.take() {
        Some(pipe) => pipe,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            drop(send);
            drop(recv);
            let _ = stdout.join();
            bail!("缺少 stderr");
        }
    };
    let stderr = pump(stderr_pipe,true,send);
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
            if last_tick.elapsed() > Duration::from_millis(500) { tick()?; last_tick = std::time::Instant::now(); }
            match recv.recv_timeout(Duration::from_millis(50)) {
                Ok(PipeMessage::Line(err,text)) => {
                    last_output = Instant::now();
                    if !text.is_empty() {
                        let sink = if err { &mut stderr_tail } else { &mut stdout_tail };
                        if sink.len() == 12 { sink.pop_front(); } sink.push_back(text.clone());
                    }
                    line(err,&text)?;
                }
                Ok(PipeMessage::Error(error)) => bail!("{error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if last_output.elapsed() >= idle_timeout {
                        bail!("7-Zip 连续 {} 秒无输出，疑似挂死，已强制终止", idle_timeout.as_secs());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // 管道已断开后子进程仍可能挂死：限时等待，禁止无界 child.wait()。
        let status = wait_child_with_deadline(&mut child, idle_timeout, Some(control))?;
        if !status.success() {
            let code = status.code().map(|code| code.to_string()).unwrap_or_else(|| "未知".into());
            let summary = summarize_failure(&stderr_tail,&stdout_tail);
            if summary.is_empty() { bail!("7-Zip 退出码 {code}，且没有输出可读的错误行；压缩包可能已损坏或不完整"); }
            bail!("7-Zip 退出码 {code}（警告也不视为完整成功）：{summary}");
        }
        Ok(())
    })();
    if result.is_err() { let _ = child.kill(); let _ = child.wait(); }
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
        collection::vec(any::<char>().prop_filter("排除 CR/LF", |c| *c != '\r' && *c != '\n'), 0..16)
            .prop_map(|chars| chars.into_iter().collect())
    }
    /// 生成无歧义的（行内容, 行结束符种子）组合：最后一段非空（「末尾空行」与
    /// 「结尾终止符」在字节上不可区分），且排除「\r 分隔 + 空行 + \n 分隔」——
    /// 该组合会拼接成一个 CRLF（单终止符，这是正确语义），留着会把正确合并当吞行。
    /// 策略提到命名函数：proptest 语句糖的参数列表里出现闭包管道符会解析失败。
    fn unambiguous_lines_and_seps() -> impl Strategy<Value = (Vec<String>, Vec<u8>)> {
        (collection::vec(any_line(), 1..12)
                .prop_filter("最后一段非空", |v| v.last().is_some_and(|l| !l.is_empty())),
            collection::vec(any::<u8>(), 12))
            .prop_filter("排除跨空行合并歧义", |(lines, seps)| {
                (1..lines.len().saturating_sub(1)).all(|j| {
                    !(seps[j % seps.len()] % 3 == 2 && lines[j].is_empty()
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
            while let Ok(_) = recv.try_recv() {}
        }
    }

    /// is_metadata_line：空行/普通错误行不是元数据；-slt 的 Path/Type/… 字段是元数据。
    #[test]
    fn is_metadata_line_classifies_common_lines() {
        assert!(!is_metadata_line(""));
        assert!(!is_metadata_line("   "));
        assert!(!is_metadata_line("ERROR: Cannot open the file as archive"));
        assert!(!is_metadata_line("7-Zip [64] 23.01: Copyright (c) 1999-2023 Igor Pavlov"));
        assert!(!is_metadata_line("foo = bar"), "非白名单键不得当成元数据");
        assert!(is_metadata_line("Path = archive.7z"));
        assert!(is_metadata_line("Type = 7z"));
        assert!(is_metadata_line("Physical Size = 12345"));
        assert!(is_metadata_line("Method = LZMA2:19"));
        assert!(is_metadata_line(" Path = leading-space-key"), "键两侧空白应被 trim");
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
        assert!(!summary.contains("stdout only error"), "stderr 有可用行时不回退 stdout");
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
                PipeMessage::Line(err, text) => { assert!(!err); lines.push(text); }
                PipeMessage::Error(error) => panic!("不应出现错误：{error}"),
            }
        }
        assert_eq!(lines, vec!["line1", "line2", "line3"]);
    }

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
                Err(std::io::Error::new(std::io::ErrorKind::Other, "模拟读错误"))
            }
        }
        let capture = read_all_capped(FailingReader, 64);
        assert!(capture.error.as_deref().is_some_and(|e| e.contains("模拟读错误")));
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
        assert!(message.contains("无输出") || message.contains("挂死"), "错误应说明空闲超时：{message}");
        assert!(elapsed < Duration::from_secs(10), "应在空闲超时后尽快返回，实际 {elapsed:?}");
    }

    /// run_with_idle_timeout：取消路径仍可用——先启动再 cancel，应返回取消错误。
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
}
