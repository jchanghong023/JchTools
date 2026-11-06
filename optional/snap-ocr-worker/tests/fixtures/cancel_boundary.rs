use std::{
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
    let output = Arc::new(Mutex::new(io::stdout()));
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let output = output.clone();
        std::thread::spawn(move || {
            let command = field(&line, "command");
            let id = field(&line, "id");
            let gated = command == "ocr_snapshot" && field(&line, "image_base64") == "wait";
            if gated {
                std::fs::write("snapshot-started", b"1").unwrap();
                while !Path::new("release-snapshot").exists() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let payload = match command.as_str() {
                "capabilities" => "\"commands\":[\"extract\",\"ocr_snapshot\",\"transcribe\",\"cancel\"],\"protocol_version\":2,\"cancellation\":\"cooperative\",\"timeout_ms\":true,\"document_snapshot_concurrent\":true,\"paddle_ocr\":true,\"transcription\":true",
                "ocr_snapshot" => "\"text\":\"snapshot\"",
                "cancel" => "\"accepted\":true",
                _ => "\"state\":\"ready\"",
            };
            let mut out = output.lock().unwrap();
            writeln!(out, "{{\"id\":\"{id}\",\"ok\":true,{payload}}}").unwrap();
            out.flush().unwrap();
            if gated {
                std::fs::write("snapshot-terminal", b"1").unwrap();
            }
        });
    }
}
