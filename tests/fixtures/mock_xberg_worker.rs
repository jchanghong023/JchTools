//! 媒体转录协议测试的模拟 Xberg worker（仅 std，无依赖）。
//!
//! 该文件位于 `tests/fixtures/` 子目录，cargo 不会把它自动发现为集成测试目标；
//! 由 `src/markdown.rs` 的单测在测试期用 `rustc` 编译成独立子进程运行，用于
//! 驱动 `MediaWorker` 协议客户端，不实现任何真实推理。
//!
//! 用法：`mock_xberg_worker <mode>`，mode 取值：
//! - `ok`：每条 `transcribe` 回 `ok:true`，markdown 为 `MOCK MARKDOWN <id>`。
//! - `noaudio`：回 `ok:true`、`has_audio:false`，markdown 为 `MOCK NOAUDIO MARKDOWN`。
//! - `fail-then-ok`：第一条请求回 `ok:false`（模拟解码失败），之后恢复正常。
//! - `exit-after-first`：应答第一条请求后立即以退出码 3 退出（模拟进程崩溃）。
//! - `slow`：每条请求先睡 30 秒再响应（供单文件超时测试杀进程）。
//! - `garbage`：对每条请求回一行非法 JSON（模拟协议破坏）。
//! - `oversize`：第一条请求回一行超过 8 MiB 捕获上限的响应（模拟超长转录），
//!   之后恢复正常——用于验证客户端按「单文件失败」处理且进程继续复用。
//!
//! stdin EOF 后正常退出（退出码 0）。若环境变量 `MOCK_XBERG_WORKER_EOF_MARKER`
//! 指向可写路径，EOF 时写出一个标记文件，供测试证明「子进程收到 EOF 并自行退出」
//! 而非被强杀。

use std::io::{BufRead, Write};
use std::time::Duration;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut first = true;
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Some(id) = extract_id(&line) else {
            break;
        };
        let response = match mode.as_str() {
            "ok" => format!(
                r#"{{"id":{id},"ok":true,"markdown":"MOCK MARKDOWN {id}","segments":[],"duration_ms":1234,"has_audio":true}}"#
            ),
            "noaudio" => format!(
                r#"{{"id":{id},"ok":true,"markdown":"MOCK NOAUDIO MARKDOWN","segments":[],"duration_ms":0,"has_audio":false}}"#
            ),
            "fail-then-ok" if first => format!(
                r#"{{"id":{id},"ok":false,"error":"mock decode failure"}}"#
            ),
            "garbage" => "NOT-JSON-LINE".to_string(),
            "oversize" if first => {
                // 单行超过客户端的 8 MiB 捕获上限；仍是合法 JSON（超长 markdown）。
                let padding = "x".repeat(8 * 1024 * 1024 + 64);
                format!(
                    r#"{{"id":{id},"ok":true,"markdown":"{padding}","segments":[],"duration_ms":1,"has_audio":true}}"#
                )
            }
            "slow" => {
                std::thread::sleep(Duration::from_secs(30));
                format!(
                    r#"{{"id":{id},"ok":true,"markdown":"MOCK MARKDOWN {id}","segments":[],"duration_ms":1234,"has_audio":true}}"#
                )
            }
            "exit-after-first" if first => {
                // 先写出第一条响应，再以非零码退出：模拟「响应后进程崩溃」。
                let _ = writeln!(stdout, "{}", response_for_exit(id));
                let _ = stdout.flush();
                std::process::exit(3);
            }
            _ => format!(
                r#"{{"id":{id},"ok":true,"markdown":"MOCK MARKDOWN {id}","segments":[],"duration_ms":1234,"has_audio":true}}"#
            ),
        };
        first = false;
        if writeln!(stdout, "{response}").is_err() || stdout.flush().is_err() {
            // 客户端断连：结束进程而非空转（对齐 Xberg 断连语义）。
            std::process::exit(4);
        }
    }
    if let Ok(marker) = std::env::var("MOCK_XBERG_WORKER_EOF_MARKER") {
        let _ = std::fs::write(marker, b"eof");
    }
}

fn response_for_exit(id: u64) -> String {
    format!(
        r#"{{"id":{id},"ok":true,"markdown":"MOCK MARKDOWN {id}","segments":[],"duration_ms":1234,"has_audio":true}}"#
    )
}

/// 从请求行提取 `"id":` 后的整数（mock 只需回显 id；客户端发送的 id 是首字段）。
fn extract_id(line: &str) -> Option<u64> {
    let position = line.find("\"id\"")? + "\"id\"".len();
    let rest = line[position..].trim_start().strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}
