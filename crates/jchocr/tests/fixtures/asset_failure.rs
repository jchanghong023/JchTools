// O-35/O-36 regression fixture: no model loading, only worker protocol responses.
use std::io::{self, BufRead, Write};

fn field<'a>(line: &'a str, prefix: &str) -> &'a str {
    line.split(prefix)
        .nth(1)
        .unwrap_or("")
        .split('"')
        .next()
        .unwrap_or("")
}

fn main() {
    let mut stdout = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let id = field(&line, "\"id\":\"");
        let command = field(&line, "\"command\":\"");
        let payload = if command == "ocr_snapshot" {
            "\"ok\":false,\"error_kind\":\"asset_invalid\",\"error\":\"snapshot model asset missing\""
        } else {
            "\"ok\":true,\"commands\":[\"ocr_snapshot\",\"cancel\"],\"protocol_version\":2,\"cancellation\":\"cooperative\",\"timeout_ms\":true,\"document_snapshot_concurrent\":true"
        };
        writeln!(stdout, "{{\"id\":\"{id}\",{payload}}}").unwrap();
        stdout.flush().unwrap();
    }
}
