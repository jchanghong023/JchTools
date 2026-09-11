//! Bounded, cancellable process I/O; no shell and no unbounded Command::output buffers.
use crate::control::Control;
use anyhow::{bail, Context, Result};
use std::{io::{BufRead, BufReader, Read}, process::{Command, Stdio}, sync::mpsc, thread, time::Duration};

#[derive(Debug)]
enum PipeMessage { Line(bool,String), Error(String) }
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
pub fn run(command: &mut Command, control: &Control, mut line: impl FnMut(bool,&str)->Result<()>, mut tick: impl FnMut()->Result<()>) -> Result<()> {
    control.checkpoint()?;
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)] { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
    let mut child = command.spawn().context("无法启动随包携带的 7-Zip")?;
    let (send,recv) = mpsc::sync_channel(64);
    let stdout = pump(child.stdout.take().context("缺少 stdout")?,false,send.clone());
    let stderr = pump(child.stderr.take().context("缺少 stderr")?,true,send);
    let mut tail = std::collections::VecDeque::new();
    let result = (|| {
        let mut last_tick = std::time::Instant::now();
        loop {
            // Pause is deliberately deferred until this whole archive finishes. We must drain pipes.
            control.check_cancelled()?;
            if last_tick.elapsed() > Duration::from_millis(500) { tick()?; last_tick = std::time::Instant::now(); }
            match recv.recv_timeout(Duration::from_millis(50)) {
                Ok(PipeMessage::Line(err,text)) => {
                    if !text.is_empty() { if tail.len() == 12 { tail.pop_front(); } tail.push_back(text.clone()); }
                    line(err,&text)?;
                }
                Ok(PipeMessage::Error(error)) => bail!("{error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = child.wait()?;
        if !status.success() { bail!("7-Zip 退出码 {:?}（警告也不视为完整成功）：{}", status.code(),tail.into_iter().collect::<Vec<_>>().join(" | ")); }
        Ok(())
    })();
    if result.is_err() { let _ = child.kill(); let _ = child.wait(); }
    drop(recv);
    let _ = stdout.join(); let _ = stderr.join();
    result
}
