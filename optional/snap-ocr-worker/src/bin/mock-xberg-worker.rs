//! 协议测试的模拟 Xberg 子进程：按模式模拟 `xberg worker` 的 stdout 行为。
//!
//! 仅供 `tests/xberg_client.rs` 经 `CARGO_BIN_EXE_mock-xberg-worker` 启动；
//! 不实现真实推理，只复刻协议形状（id 回显、串行、EOF 退出码 0）。
//!
//! 用法：`mock-xberg-worker <mode>`，mode 取值：
//! - `ok`：`snapshot_state` → ready；`ocr_snapshot` → 成功文本 `MOCK 布局文本`。
//! - `state-uninit`：首个 `snapshot_state` → uninitialized，识别成功后 → ready。
//! - `state-error`：`snapshot_state` → error（携带错误摘要）。
//! - `fail`：`ocr_snapshot` → `ok:false`，`error_kind:"asset_invalid"`。
//! - `slow`：`ocr_snapshot` 延迟 2 秒后成功（供取消测试）。
//! - `exit-after-first`：应答第一条请求后以退出码 3 退出（供进程退出测试）。
//!
//! EOF（stdin 关闭）后退出码 0；空白行跳过；未知命令回 `ok:false`。

use std::io::{BufRead, Write};
use std::time::Duration;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut recognized = false;
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
            break;
        };
        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let response = match request.get("command").and_then(|c| c.as_str()) {
            Some("snapshot_state") if mode == "state-error" => serde_json::json!({
                "id": id,
                "ok": true,
                "state": "error",
                "error": "snapshot model asset missing (rec)",
            }),
            Some("snapshot_state") => serde_json::json!({
                "id": id,
                "ok": true,
                "state": if recognized { "ready" } else if mode == "state-uninit" { "uninitialized" } else { "ready" },
                "error": null,
            }),
            Some("ocr_snapshot") => {
                if mode == "slow" {
                    std::thread::sleep(Duration::from_secs(1));
                }
                if mode == "exit-after-first" {
                    // 先写响应再退出，模拟「响应后进程崩溃」。
                    let _ = writeln!(stdout, "{}", response_line(&id, &mode, &request));
                    let _ = stdout.flush();
                    std::process::exit(3);
                }
                recognized = true;
                response_line(&id, &mode, &request)
            }
            _ => serde_json::json!({"id": id, "ok": false, "error": "unsupported command"}),
        };
        if writeln!(stdout, "{response}").is_err() || stdout.flush().is_err() {
            // 客户端断连：结束进程而非空转（对齐 Xberg 断连语义）。
            std::process::exit(4);
        }
    }
}

/// 按模式给出 `ocr_snapshot` 的响应行；base64 `Tk9URVhU`（"NOTEXT"）表示无文字图。
fn response_line(
    id: &serde_json::Value,
    mode: &str,
    request: &serde_json::Value,
) -> serde_json::Value {
    if mode == "fail" {
        serde_json::json!({
            "id": id,
            "ok": false,
            "error": "snapshot model asset mismatch (det)",
            "error_kind": "asset_invalid",
        })
    } else if request.get("image_base64").and_then(|v| v.as_str()) == Some("Tk9URVhU") {
        serde_json::json!({
            "id": id,
            "ok": true,
            "text": "",
            "records": 0,
            "elapsed_ms": 1,
            "error_kind": "no_text",
        })
    } else {
        serde_json::json!({
            "id": id,
            "ok": true,
            "text": "MOCK 布局文本",
            "records": 2,
            "elapsed_ms": 12,
        })
    }
}
