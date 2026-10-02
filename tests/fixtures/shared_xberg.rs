// 共享进程集成测试专用，无模型；不打包。用可观察 PID、阻塞文件和请求响应验证代理。
use std::{collections::HashMap, sync::atomic::{AtomicBool, Ordering}, time::Instant, fs::OpenOptions, io::{self, BufRead, Write}, sync::{Arc, Mutex}, time::Duration};
fn field(line: &str, key: &str) -> String {
    line.split(&format!("\"{key}\":\"")).nth(1).unwrap_or("").split('"').next().unwrap_or("").to_owned()
}
fn main() {
    writeln!(OpenOptions::new().create(true).append(true).open("starts.txt").unwrap(), "{}", std::process::id()).unwrap();
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("extract") {
        println!("{{\"result\":{{\"content\":\"document\"}}}}");
        return;
    }
    let output = Arc::new(Mutex::new(io::stdout()));
    let pending = Arc::new(Mutex::new(HashMap::<String, Arc<AtomicBool>>::new()));
    for line in io::stdin().lock().lines() {
        let line = line.unwrap(); let id = field(&line, "id"); let command = field(&line, "command");
        let output = output.clone();
        let flag = Arc::new(AtomicBool::new(false));
        if command == "cancel" {
            if let Some(target) = pending.lock().unwrap().get(&field(&line, "target_id")) { target.store(true, Ordering::Release); }
        } else { pending.lock().unwrap().insert(id.clone(), flag.clone()); }
        let pending = pending.clone();
        std::thread::spawn(move || {
            let timeout = line.split("\"timeout_ms\":").nth(1).and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next()).and_then(|s| s.parse::<u64>().ok()).unwrap_or(60000);
            let end = Instant::now() + Duration::from_millis(timeout);
            let waiting = (command == "extract" && line.contains("long.txt")) || (command == "ocr_snapshot" && field(&line,"image_base64") == "wait");
            if waiting {
                std::fs::write(if command == "extract" {"document-started"} else {"snapshot-started"}, "1").unwrap();
                while !(flag.load(Ordering::Acquire) || Instant::now() >= end || command == "extract" && std::path::Path::new("release-document").exists()) { std::thread::sleep(Duration::from_millis(5)); }
            }
            if flag.load(Ordering::Acquire) || Instant::now() >= end {
                let kind = if Instant::now() >= end {"timeout"} else {"cancelled"};
                let mut out = output.lock().unwrap();
                writeln!(out, "{{\"id\":\"{id}\",\"ok\":false,\"error_kind\":\"{kind}\",\"error\":\"fixture stopped\"}}").unwrap();out.flush().unwrap();
                pending.lock().unwrap().remove(&id); return;
            }
            let payload = match command.as_str() {
                "extract" => {
                    // T-23 回归注入：写一行非法 JSON 后照常运行，复现「通信断裂
                    // 但引擎进程存活」的缺陷现场（生产：响应超消息上限）。
                    if line.contains("corrupt") {
                        let mut out = output.lock().unwrap();
                        writeln!(out, "{{\"id\":\"{id}\",broken-response-not-json").unwrap();
                        out.flush().unwrap();
                        pending.lock().unwrap().remove(&id);
                        return;
                    }
                    "\"document\":{\"content\":\"document\"}".to_owned()
                }
                "ocr_snapshot" => "\"text\":\"截图结果\",\"records\":1".into(),
                "transcribe" => "\"markdown\":\"media\"".into(),
                "snapshot_state" => "\"state\":\"ready\"".into(),
                "formats" => "\"formats\":[{\"extension\":\"txt\",\"mime_type\":\"text/plain\"}]".into(),
                "capabilities" => "\"commands\":[\"extract\",\"ocr_snapshot\",\"cancel\",\"formats\",\"transcribe\"],\"protocol_version\":2,\"cancellation\":\"cooperative\",\"timeout_ms\":true,\"document_snapshot_concurrent\":true,\"extract_modes\":[\"normal\",\"fast\"]".into(),
                "cancel" => "\"accepted\":true".into(),
                _ => "\"unknown\":true".into(),
            };
            let mut out = output.lock().unwrap();
            writeln!(out, "{{\"id\":\"{id}\",\"ok\":true,{payload}}}").unwrap();out.flush().unwrap();
            pending.lock().unwrap().remove(&id);
        });
    }
}
