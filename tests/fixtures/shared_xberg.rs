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
            let waiting = (command == "extract" && (line.contains("long.txt") || line.contains("unresponsive.txt"))) || (command == "transcribe" && line.contains("long.mp4")) || (command == "ocr_snapshot" && field(&line,"image_base64") == "wait");
            if waiting {
                std::fs::write(match command.as_str() {"extract" => "document-started", "transcribe" => "media-started", _ => "snapshot-started"}, "1").unwrap();
                let unresponsive = command == "extract" && line.contains("unresponsive.txt");
                while unresponsive || !(flag.load(Ordering::Acquire) || Instant::now() >= end || matches!(command.as_str(), "extract" | "transcribe") && std::path::Path::new("release-document").exists()) { std::thread::sleep(Duration::from_millis(5)); }
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
                    // 2026-10-04 协议：document.content 为引擎最终 Markdown，
                    // warnings 恒在；images 可缺席（空数组为超集情形）。
                    "\"document\":{\"content\":\"document\",\"images\":[]},\"warnings\":[]".to_owned()
                }
                "ocr_snapshot" => "\"text\":\"截图结果\",\"records\":1".into(),
                "transcribe" => "\"markdown\":\"media\"".into(),
                "snapshot_state" => "\"state\":\"ready\"".into(),
                // T-08：所选引擎仅声明必需文档格式与 txt，不能由内嵌清单补入 rtf。
                "formats" => "\"formats\":[{\"extension\":\"pdf\",\"mime_type\":\"application/pdf\"},{\"extension\":\"docx\",\"mime_type\":\"application/vnd.openxmlformats-officedocument.wordprocessingml.document\"},{\"extension\":\"pptx\",\"mime_type\":\"application/vnd.openxmlformats-officedocument.presentationml.presentation\"},{\"extension\":\"xlsx\",\"mime_type\":\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet\"},{\"extension\":\"txt\",\"mime_type\":\"text/plain\"}]".into(),
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
