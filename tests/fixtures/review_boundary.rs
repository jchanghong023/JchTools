use std::{
    fs::OpenOptions,
    io::{self, BufRead, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
fn field(line: &str, name: &str) -> String {
    line.split(&format!("\"{name}\":\""))
        .nth(1)
        .unwrap_or("")
        .split('"')
        .next()
        .unwrap_or("")
        .to_owned()
}
fn main() {
    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open("starts.txt")
            .unwrap(),
        "{}",
        std::process::id()
    )
    .unwrap();
    let output = Arc::new(Mutex::new(io::stdout()));
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let command = field(&line, "command");
        let id = field(&line, "id");
        let output = output.clone();
        std::thread::spawn(move || {
            if command == "ocr_snapshot" && field(&line, "image_base64") == "wait" {
                std::fs::write("snapshot-started", b"1").unwrap();
                // 模拟已经进入原生调用：取消 ACK 不会使原请求产生终态。
                while !Path::new("release-snapshot").exists() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                if Path::new("cancel-started").exists() {
                    let mut out = output.lock().unwrap();
                    writeln!(out, "{{\"id\":\"{id}\",\"ok\":false,\"error_kind\":\"cancelled\",\"error\":\"cancelled\"}}").unwrap();
                    out.flush().unwrap();
                    return;
                }
            }
            let payload = match command.as_str() {
                "capabilities" => format!(
                    "\"commands\":[\"extract\",\"ocr_snapshot\",\"transcribe\",\"cancel\"],\
                    \"protocol_version\":2,\"cancellation\":\"cooperative\",\"timeout_ms\":true,\
                    \"document_snapshot_concurrent\":true,\"paddle_ocr\":true,\"transcription\":{0},\"transcription_streaming_path\":{0}",
                    !Path::new("disable-transcription").exists()),
                "snapshot_state" => format!(
                    "\"state\":\"ready\",\"debug_crops_inherited\":{}",
                    std::env::var_os("XBERG_SNAPSHOT_DEBUG_CROPS").is_some()),
                "extract" | "transcribe" => {
                    if line.contains("long.") {
                        let marker = if command == "extract" {
                            "document-started"
                        } else { "media-started" };
                        std::fs::write(marker, b"1").unwrap();
                        while !Path::new("release-document").exists() {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                    if command == "extract" {
                        "\"document\":{\"content\":\"document\",\"images\":[],\"processing_warnings\":[]},\"warnings\":[]".into()
                    } else { "\"markdown\":\"media\"".into() }
                }
                "ocr_snapshot" => "\"text\":\"snapshot\"".into(),
                "cancel" => {
                    std::fs::write("cancel-started", b"1").unwrap();
                    "\"accepted\":true".into()
                },
                _ => "\"state\":\"ready\"".into(),
            };
            let mut out = output.lock().unwrap();
            writeln!(out, "{{\"id\":\"{id}\",\"ok\":true,{payload}}}").unwrap();
            out.flush().unwrap();
        });
    }
}
