use std::io::{BufRead, BufReader, Read};
use std::process::Command;
use std::sync::mpsc;
use std::thread;

// 只记录已知程序类别；自定义程序名也可能含用户数据，不落盘原路径或参数。
pub(super) fn executable_kind(command: &Command) -> &'static str {
    let name = std::path::Path::new(command.get_program())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    match name {
        name if name.eq_ignore_ascii_case("git.exe") || name.eq_ignore_ascii_case("git") => "git",
        name if name.eq_ignore_ascii_case("7z.exe") || name.eq_ignore_ascii_case("7z") => "7zip",
        name if name.eq_ignore_ascii_case("powershell.exe") => "powershell",
        name if name.eq_ignore_ascii_case("pandoc.exe") => "pandoc",
        name if name.eq_ignore_ascii_case("cmd.exe") => "cmd",
        _ => "other",
    }
}

#[derive(Debug)]
pub(super) enum PipeMessage {
    Line(bool, String),
    Error(String),
}

/// 7-Zip 失败时 tail 里混着 stdout 的元数据（`Path = …`）和 stderr 的报错；
/// 只把这些当成"给用户看的原因"，否则用户拿到的是几十行条目字段。
pub(super) fn is_metadata_line(line: &str) -> bool {
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

pub(super) fn summarize_failure(
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

pub(super) fn pump<R: Read + Send + 'static>(
    reader: R,
    err: bool,
    sender: mpsc::SyncSender<PipeMessage>,
) -> thread::JoinHandle<()> {
    let span = tracing::Span::current();
    let dispatcher = tracing::dispatcher::get_default(Clone::clone);
    thread::spawn(move || {
        let _dispatcher = tracing::dispatcher::set_default(&dispatcher);
        let _entered = span.enter();
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
                        tracing::error!(event = "pipe_read_failed", stream = if err { "stderr" } else { "stdout" }, error_type = ?e.kind(), error_code = e.raw_os_error(), "子进程管道读取失败");
                        let _ = sender.send(PipeMessage::Error(e.to_string()));
                        break;
                    }
                };
                if bytes.is_empty() {
                    if !line.is_empty() {
                        let message = if let Ok(text) = String::from_utf8(std::mem::take(&mut line))
                        {
                            PipeMessage::Line(err, text)
                        } else {
                            tracing::error!(
                                event = "pipe_decode_failed",
                                stream = if err { "stderr" } else { "stdout" },
                                error_type = "invalid_utf8",
                                "子进程管道解码失败"
                            );
                            PipeMessage::Error("7-Zip 输出不是 UTF-8".into())
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
                tracing::error!(
                    event = "pipe_line_limit_exceeded",
                    stream = if err { "stderr" } else { "stdout" },
                    limit_bytes = 64 * 1024,
                    "子进程输出行超过解析上限"
                );
                let _ = sender.send(PipeMessage::Error(
                    "7-Zip 输出行超过 64 KiB，已拒绝解析".into(),
                ));
                break;
            }
            if terminated.is_some() {
                let Ok(text) = String::from_utf8(std::mem::take(&mut line)) else {
                    tracing::error!(
                        event = "pipe_decode_failed",
                        stream = if err { "stderr" } else { "stdout" },
                        error_type = "invalid_utf8",
                        "子进程管道解码失败"
                    );
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
