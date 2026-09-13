//! Bounded, cancellable process I/O; no shell and no unbounded Command::output buffers.
use crate::control::Control;
use anyhow::{bail, Context, Result};
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
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join(name)
}

#[cfg(not(windows))]
pub fn system_tool(name: &str) -> PathBuf {
    PathBuf::from(name)
}

/// 捕获到的子进程输出（只读查询用；无 shell）。
pub struct CapturedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
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
    let stdout_thread = thread::spawn(move || read_all(stdout_pipe));
    let stderr_thread = thread::spawn(move || read_all(stderr_pipe));
    match wait_child_with_deadline(&mut child, timeout) {
        Ok(status) => {
            let stdout = stdout_thread.join().unwrap_or_default();
            let stderr = stderr_thread.join().unwrap_or_default();
            // stdin 写入失败：子进程已成功退出时多半是提前关掉 stdin（EPIPE），不必判失败；
            // 子进程未成功时上报写入错误，便于定位管道问题。
            if let Some(handle) = stdin_thread {
                if let Ok(Err(error)) = handle.join() {
                    if !status.success() {
                        bail!("向子进程写入 stdin 失败：{error}");
                    }
                }
            }
            Ok(CapturedOutput { status, stdout, stderr })
        }
        Err(error) => {
            // 超时/等待失败：先确保子进程回收，再结束读线程（管道关闭后退出）。
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            if let Some(handle) = stdin_thread {
                let _ = handle.join();
            }
            Err(error)
        }
    }
}

fn read_all<R: Read>(mut reader: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}

fn wait_child_with_deadline(child: &mut Child, timeout: Duration) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
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

pub fn run(command: &mut Command, control: &Control, mut line: impl FnMut(bool,&str)->Result<()>, mut tick: impl FnMut()->Result<()>) -> Result<()> {
    control.checkpoint()?;
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
    let mut child = command.spawn().context("无法启动随包携带的 7-Zip")?;
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
        loop {
            // Pause is deliberately deferred until this whole archive finishes. We must drain pipes.
            control.check_cancelled()?;
            if last_tick.elapsed() > Duration::from_millis(500) { tick()?; last_tick = std::time::Instant::now(); }
            match recv.recv_timeout(Duration::from_millis(50)) {
                Ok(PipeMessage::Line(err,text)) => {
                    if !text.is_empty() {
                        let sink = if err { &mut stderr_tail } else { &mut stdout_tail };
                        if sink.len() == 12 { sink.pop_front(); } sink.push_back(text.clone());
                    }
                    line(err,&text)?;
                }
                Ok(PipeMessage::Error(error)) => bail!("{error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = child.wait()?;
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
    let _ = stdout.join(); let _ = stderr.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

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
}
