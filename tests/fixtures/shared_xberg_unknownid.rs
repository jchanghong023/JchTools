// S6-04 回归专用模拟引擎（tests/xberg_unknown_response.rs 使用）：对触发请求
// 先输出一条从未提交过的「幽灵」响应（未知 id），且对触发请求自身（含取消）
// 永不回应，复现代理静默丢弃未知 id 响应后等待者只能超时、场景 lane 被在途
// 项永久占用的缺陷现场。仅测试编译使用，不参与打包。
use std::io::{self, BufRead, Write};

fn field(line: &str, key: &str) -> String {
    line.split(&format!("\"{key}\":\""))
        .nth(1)
        .unwrap_or("")
        .split('"')
        .next()
        .unwrap_or("")
        .to_owned()
}

fn main() {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let id = field(&line, "id");
        let command = field(&line, "command");
        let respond = |out: &mut io::StdoutLock, id: &str, payload: &str| {
            let _ = writeln!(out, "{{\"id\":\"{id}\",\"ok\":true,{payload}}}");
            let _ = out.flush();
        };
        match command.as_str() {
            "capabilities" => respond(
                &mut out,
                &id,
                "\"commands\":[\"extract\",\"ocr_snapshot\",\"cancel\",\"formats\",\"transcribe\"],\"protocol_version\":2,\"cancellation\":\"cooperative\",\"timeout_ms\":true,\"document_snapshot_concurrent\":true,\"extract_modes\":[\"normal\",\"fast\"]",
            ),
            "snapshot_state" => respond(&mut out, &id, "\"state\":\"ready\""),
            "ocr_snapshot" if field(&line, "image_base64") == "phantom" => {
                // 缺陷注入：先回一条无人认领（未知 id）的幽灵响应，再对触发
                // 请求保持沉默，等待者在代理侧不再有任何可达终态。
                let _ = writeln!(
                    out,
                    "{{\"id\":\"ghost-unclaimed\",\"ok\":true,\"text\":\"phantom\"}}"
                );
                let _ = out.flush();
            }
            "ocr_snapshot" => respond(&mut out, &id, "\"text\":\"截图结果\",\"records\":1"),
            // 取消目标（触发请求）永不结束：修复前取消接口只能超时，突出
            // 「等待者收不到终态」的缺陷现场；修复后幽灵响应先行触发断裂交付。
            "cancel" => {}
            _ => respond(&mut out, &id, "\"unknown\":true"),
        }
    }
}
