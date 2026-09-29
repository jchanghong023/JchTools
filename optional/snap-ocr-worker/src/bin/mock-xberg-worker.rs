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
//! - `slow`：`ocr_snapshot` 延迟 1 秒后成功（供取消测试）。
//! - `exit-after-first`：应答第一条请求后以退出码 3 退出（供进程退出测试）。
//! - `half-line`：对首条请求写一条不带换行终止的完整 JSON 响应后立即退出
//!   （模拟协议帧截断：进程已死但读侧先收到残缺数据，供「半行截断必须升级为
//!   进程退出分类」回归测试使用）。
//! - `console-probe`：任何请求都以成功响应报告自身是否持有控制台窗口
//!   （`console-present` / `no-console`），供「子进程不弹黑窗
//!   （CREATE_NO_WINDOW）」回归测试探测。
//! - `hang`：启动后长眠 60 秒，不读 stdin、不写 stdout——模拟卡死在不可中断
//!   推理中（读不到 stdin EOF）的 xberg，供「服务进程异常死亡后子进程必须被
//!   内核回收」回归测试探测；有界时长避免测试异常时遗留永久孤儿。
//! - `env-probe`：任何请求都以成功响应回报受关注环境变量的实际取值
//!   （`k=v;…`，缺失记 `<unset>`），供「spawn 注入离线开关（XB-04）」回归
//!   测试探测。
//!
//! EOF（stdin 关闭）后退出码 0；空白行跳过；未知命令回 `ok:false`。

use std::io::{BufRead, Write};
use std::time::Duration;

/// 全部模式名：生产 `XbergWorkerClient::spawn` 会先传 `worker`、
/// `--no-config-discovery`、`--config-json` 等 xberg 风格参数，mock 从全部
/// 参数里取第一个已知模式，其余参数忽略——生产 spawn 路径可直接驱动 mock；
/// 参数中无模式时回退读环境变量 `MOCK_XBERG_MODE`（经继承环境注入，专供
/// 生产 spawn 路径的回归测试选择探针模式）。
const KNOWN_MODES: [&str; 10] = [
    "ok",
    "state-uninit",
    "state-error",
    "fail",
    "slow",
    "exit-after-first",
    "half-line",
    "console-probe",
    "hang",
    "env-probe",
];

/// env-probe 模式回报的受关注环境变量：ORT_DYLIB_PATH（路径型）+ 与
/// src/markdown.rs `media_worker_environment` 同口径的 7 个纯开关（XB-04）。
const WATCHED_ENV_KEYS: [&str; 8] = [
    "ORT_DYLIB_PATH",
    "HF_HUB_OFFLINE",
    "HUGGINGFACE_HUB_OFFLINE",
    "TRANSFORMERS_OFFLINE",
    "HF_DATASETS_OFFLINE",
    "NO_COLOR",
    "XBERG_ORT_EP",
    "XBERG_MAX_CONCURRENT_REQUESTS",
];

/// 当前进程是否持有控制台窗口：被 CREATE_NO_WINDOW 启动的控制台程序没有
/// 控制台句柄（GetConsoleWindow 返回 NULL），否则继承或新建控制台。
#[cfg(windows)]
fn has_console_window() -> bool {
    // SAFETY: GetConsoleWindow 无参数，只读取本进程的控制台状态。
    unsafe { !windows_sys::Win32::System::Console::GetConsoleWindow().is_null() }
}
#[cfg(not(windows))]
fn has_console_window() -> bool {
    true
}

fn main() {
    let mode = std::env::args()
        .skip(1)
        .find(|arg| KNOWN_MODES.contains(&arg.as_str()))
        .or_else(|| std::env::var("MOCK_XBERG_MODE").ok())
        .unwrap_or_default();
    if mode == "hang" {
        // 模拟卡死在不可中断推理中的 xberg：管道对它无意义，长眠后有界退出。
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
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
        if mode == "half-line" {
            // 协议帧截断：写出完整 JSON 但不带换行终止，随后立即退出——读侧先
            // 收到「残缺帧」再观察到进程死亡，模拟半死连接的最后一口数据。
            let _ = write!(
                stdout,
                "{{\"id\":{id},\"ok\":false,\"error\":\"truncated response\"}}"
            );
            let _ = stdout.flush();
            std::process::exit(0);
        }
        let response = if mode == "console-probe" {
            serde_json::json!({
                "id": id,
                "ok": true,
                "text": if has_console_window() { "console-present" } else { "no-console" },
            })
        } else if mode == "env-probe" {
            let report = WATCHED_ENV_KEYS
                .iter()
                .map(|key| {
                    let value = std::env::var(key).unwrap_or_else(|_| "<unset>".to_string());
                    format!("{key}={value}")
                })
                .collect::<Vec<_>>()
                .join(";");
            serde_json::json!({"id": id, "ok": true, "text": report})
        } else {
            mock_response(&mode, &request, &id, &mut recognized)
        };
        if writeln!(stdout, "{response}").is_err() || stdout.flush().is_err() {
            // 客户端断连：结束进程而非空转（对齐 Xberg 断连语义）。
            std::process::exit(4);
        }
        if mode == "exit-after-first"
            && request.get("command").and_then(|c| c.as_str()) == Some("ocr_snapshot")
        {
            // 首个识别响应已写出，现在模拟「应答后进程崩溃」。
            std::process::exit(3);
        }
    }
}

/// 按模式构造协议响应（console-probe 模式在 main 中先行短路）。
fn mock_response(
    mode: &str,
    request: &serde_json::Value,
    id: &serde_json::Value,
    recognized: &mut bool,
) -> serde_json::Value {
    if let Some("ocr_snapshot") = request.get("command").and_then(|c| c.as_str()) {
        if mode == "slow" {
            std::thread::sleep(Duration::from_secs(1));
        }
        *recognized = true;
        return response_line(id, mode, request);
    }
    match request.get("command").and_then(|c| c.as_str()) {
        Some("snapshot_state") if mode == "state-error" => serde_json::json!({
            "id": id,
            "ok": true,
            "state": "error",
            "error": "snapshot model asset missing (rec)",
        }),
        Some("snapshot_state") => serde_json::json!({
            "id": id,
            "ok": true,
            "state": if *recognized { "ready" } else if mode == "state-uninit" { "uninitialized" } else { "ready" },
            "error": null,
        }),
        _ => serde_json::json!({"id": id, "ok": false, "error": "unsupported command"}),
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
