// CLI 仅支持 Windows；test-hooks 将全部配置重定向到自有临时目录。
#![cfg(all(windows, feature = "test-hooks"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! 覆盖 O-33～O-37：公开进程契约，不读取用户配置或依赖真实模型。
use std::{
    io::{Cursor, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::LazyLock,
};

#[path = "../../../tests/common/engine_session.rs"]
mod engine_session;

fn repository_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|path| path.join("scripts/make_tmp.py").is_file())
        .expect("仓库根必须包含临时目录工厂")
}

struct Harness {
    root: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        static WORKSPACE: LazyLock<PathBuf> = LazyLock::new(|| {
            let repo = repository_root();
            let workspace = repo.join(".tmp/jchocr-contract");
            assert!(Command::new("python")
                .arg(repo.join("scripts/make_tmp.py"))
                .args(["workspace", "--destination"])
                .arg(&workspace)
                .current_dir(repo)
                .status()
                .unwrap()
                .success());
            workspace
        });
        Self {
            root: tempfile::tempdir_in(&*WORKSPACE).unwrap(),
        }
    }

    fn command(&self, args: &[&str], input: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_jchocr"));
        command
            .args(args)
            .current_dir(self.root.path())
            .env("JCHTOOLS_TEST_STATE_DIR", self.root.path().join("state"))
            .env("JCH_LOG_LEVEL", "INFO")
            .env(
                "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
                self.root.path().join("legacy-assets"),
            )
            .env(
                "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
                self.root.path().join("snapshot-assets"),
            )
            // 不连接真实代理，也不借用用户正在运行的引擎。
            .env(
                "JCHTOOLS_TEST_BROKER_EXE",
                self.root.path().join("absent-broker.exe"),
            )
            .stdin(if input { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        let mut child = self.command(args, input.is_some()).spawn().unwrap();
        if let Some(bytes) = input {
            child.stdin.take().unwrap().write_all(bytes).unwrap();
        }
        child.wait_with_output().unwrap()
    }
}

fn png() -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        1,
        1,
        image::Rgb([255, 255, 255]),
    ))
    .write_to(&mut bytes, image::ImageFormat::Png)
    .unwrap();
    bytes.into_inner()
}

fn failure(output: &Output, code: i32) -> &str {
    assert_eq!(output.status.code(), Some(code), "{output:?}");
    assert!(output.stdout.is_empty(), "failures must not fabricate OCR text");
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(!stderr.trim().is_empty(), "failure must explain itself on stderr");
    stderr
}

#[derive(serde::Deserialize)]
struct JsonError {
    ok: bool,
    error_kind: String,
    error: String,
}

#[test]
fn valid_image_without_selected_xberg_is_an_explained_asset_error() {
    let harness = Harness::new();
    let output = harness.run(&["--stdin"], Some(&png()));
    let error = failure(&output, 3);
    assert!(error.contains("Xberg"));
    assert!(error.contains("设置") || error.to_lowercase().contains("settings"));

    let output = harness.run(&["--stdin", "--json"], Some(&png()));
    let error: JsonError = serde_json::from_str(failure(&output, 3)).unwrap();
    assert!(!error.ok);
    assert_eq!(error.error_kind, "xberg");
    assert!(!error.error.trim().is_empty());
}

#[test]
fn help_is_concise_and_available_without_xberg() {
    let harness = Harness::new();
    for argument in ["--help", "help"] {
        let output = harness.run(&[argument], None);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let help = String::from_utf8(output.stdout).unwrap();
        for required in [
            "jchocr <图片绝对路径或HTTP(S) URL>",
            "jchocr --stdin",
            "jchocr --base64",
            "PNG、JPEG、BMP、GIF、TIFF、WebP",
            "--json stdout",
            "error_kind",
            "退出码：0 成功",
        ] {
            assert!(help.contains(required), "{argument}: missing {required}");
        }
        for unrelated in ["配置前提：", "程序调用示例", "PowerShell 示例", "WindowsApps"] {
            assert!(!help.contains(unrelated), "{argument}: contains {unrelated}");
        }
    }
}

#[test]
fn missing_members_in_a_selected_directory_are_an_asset_error() {
    let harness = Harness::new();
    let directory = harness.root.path().join("incomplete-xberg");
    let legacy = harness.root.path().join("legacy-assets");
    std::fs::create_dir(&directory).unwrap();
    std::fs::create_dir(&legacy).unwrap();
    // 经应用已有的旧文本到 SQLite 配置迁移入口构造缺资产场景。
    std::fs::write(
        legacy.join("xberg-runtime-path.txt"),
        directory.to_str().unwrap(),
    )
    .unwrap();
    let output = harness.run(&["--stdin", "--json"], Some(&png()));
    let error: JsonError = serde_json::from_str(failure(&output, 3)).unwrap();
    assert!(!error.ok);
    assert_eq!(error.error_kind, "xberg");
    assert!(!error.error.trim().is_empty());
}

#[test]
fn invalid_arguments_are_rejected_before_network_or_engine_use() {
    let harness = Harness::new();
    for args in [
        vec![],
        vec!["relative.png"],
        vec!["--base64"],
        vec!["--stdin", "--base64", "AA=="],
        vec!["ftp://127.0.0.1/image.png"],
    ] {
        failure(&harness.run(&args, None), 2);
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/image.png", listener.local_addr().unwrap());
    failure(&harness.run(&[&url, "--stdin"], None), 2);
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn malformed_base64_and_empty_inputs_are_input_errors_without_xberg() {
    let harness = Harness::new();
    let file = harness.root.path().join("empty.png");
    std::fs::write(&file, []).unwrap();
    for (args, input) in [
        (vec!["--base64", "%%%", "--json"], None),
        (vec!["--base64", "-", "--json"], Some(b"%%%".as_slice())),
        (vec!["--stdin", "--json"], Some(b"".as_slice())),
        (vec![file.to_str().unwrap(), "--json"], None),
    ] {
        let output = harness.run(&args, input);
        let error: JsonError = serde_json::from_str(failure(&output, 2)).unwrap();
        assert!(!error.ok);
        assert_eq!(error.error_kind, "input");
        assert!(!error.error.trim().is_empty());
    }
}

// 覆盖 P-10 / O-36：整个公开 OCR 调用须有可关联的起止和耗时，且不记录输入。
#[test]
fn ocr_call_logs_correlated_start_and_result_without_input_content() {
    let harness = Harness::new();
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png());
    failure(&harness.run(&["--base64", &encoded], None), 3);
    let logs = std::fs::read_dir(harness.root.path().join("state/logs"))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let started = logs.lines().find(|line| line.contains("event=ocr_cli_started")).unwrap();
    let completed = logs.lines().find(|line| line.contains("event=ocr_cli_completed")).unwrap();
    let operation_id = |line: &str| {
        line.split_ascii_whitespace()
            .find_map(|field| field.strip_prefix("operation_id="))
            .unwrap()
            .to_owned()
    };
    assert_eq!(operation_id(started), operation_id(completed));
    assert!(completed.contains("exit_code=3"));
    assert!(completed.contains("elapsed_ms="));
    assert!(!logs.contains(&encoded));
    for line in [started, completed] {
        assert!(!line.contains(harness.root.path().to_str().unwrap()));
    }
}

// 覆盖 O-35/O-36：成员存在性检查通过后，引擎确认资产不可用仍须返回资产退出码。
// 使用真实 CLI、共享代理和 Windows 管道，但合成引擎不代表真实模型验证。
#[test]
fn engine_asset_failure_is_an_explained_xberg_error() {
    // 先取仓库级锁，最后释放；代理守卫和临时资产须先完成清理。
    let _session = engine_session::session_lock(repository_root());
    let harness = Harness::new();
    let runtime = harness.root.path().join("xberg");
    std::fs::create_dir(&runtime).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../resources/markdown-assets.json")).unwrap();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let relative = member["path"].as_str().unwrap();
        if relative != "xberg.exe"
            && jchtools_xberg::xberg_runtime::asset_for_scenario(relative, "snapshot")
        {
            let target = runtime.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, b"synthetic asset").unwrap();
        }
    }
    let mut compiler = Command::new("rustc")
        // 与仓库共享引擎夹具一致，避免占位 VCRUNTIME DLL 被加载器当成依赖。
        .args(["-C", "target-feature=+crt-static"])
        .args([
            "--edition=2021",
            "--crate-name",
            "jchocr_asset_failure",
            "-",
        ])
        .arg("-o")
        .arg(runtime.join("xberg.exe"))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    compiler
        .stdin
        .take()
        .unwrap()
        .write_all(include_bytes!("fixtures/asset_failure.rs"))
        .unwrap();
    assert!(compiler.wait().unwrap().success());
    let legacy = harness.root.path().join("legacy-assets");
    std::fs::create_dir(&legacy).unwrap();
    std::fs::write(
        legacy.join("xberg-runtime-path.txt"),
        runtime.to_str().unwrap(),
    )
    .unwrap();

    // 直接持有本测试代理，保证失败断言或 panic 后也只回收本测试的进程树。
    struct Broker(std::process::Child);
    impl Drop for Broker {
        fn drop(&mut self) {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = self.0.wait();
        }
    }
    let _broker = Broker(
        Command::new(env!("CARGO_BIN_EXE_jchocr"))
            .arg("--xberg-broker")
            .env("JCHTOOLS_TEST_STATE_DIR", harness.root.path().join("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let ready = std::fs::read_dir(harness.root.path().join("state/logs"))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .any(|log| log.contains("event=xberg_broker_listening"));
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "isolated broker did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = harness.run(&["--stdin", "--json"], Some(&png()));
    let error: JsonError = serde_json::from_str(failure(&output, 3)).unwrap();
    assert!(!error.ok);
    assert_eq!(error.error_kind, "xberg");
    assert!(error.error.contains("设置"));
    assert!(error.error.contains("Xberg"));
    assert!(error.error.contains("snapshot model asset missing"));
}

// P2-05 / O-34 / O-36：上限来自 512 MiB IPC frame，而非另设任意图片大小限制。
// 为 Windows 最长路径的 UTF-8/JSON 扩张及请求/诊断字段保留有界 256 KiB；
// Base64 占用 4 * ceil(raw / 3)，不是任意较小的图片产品容量限制。
const REVIEW_IMAGE_BUDGET: u64 = ((512 * 1024 * 1024 - 256 * 1024) / 4) * 3;

#[derive(Clone, Copy, Debug)]
enum ReviewFraming {
    ContentLength,
    Chunked,
    ConnectionClose,
}

fn review_send(result: std::io::Result<()>) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::NotConnected
                ),
                "loopback response failed instead of being rejected by the client: {error}"
            );
            false
        }
    }
}

// 大响应只持有一个重复块，不创建 384 MiB 图片文件或同等大小的服务端缓冲区。
fn review_image_server(
    framing: ReviewFraming,
    length: u64,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/image.png", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("loopback accept failed: {error}"),
            }
        };
        // Windows 的 accepted socket 可能继承 listener 的非阻塞状态；
        // 大流写入必须阻塞等待客户端消费，不能把 WouldBlock 误当容量拒绝。
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            if stream.read_exact(&mut byte).is_err() {
                return;
            }
            request.push(byte[0]);
            assert!(request.len() <= 16 * 1024, "unexpected oversized HTTP request");
        }
        let header = match framing {
            ReviewFraming::ContentLength => format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
            ),
            ReviewFraming::Chunked => String::from(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            ),
            ReviewFraming::ConnectionClose => String::from(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n"
            ),
        };
        if !review_send(stream.write_all(header.as_bytes())) {
            return;
        }
        if matches!(framing, ReviewFraming::ContentLength) && length > REVIEW_IMAGE_BUDGET {
            // 已声明超限时无需发送 384 MiB：客户端必须只凭 headers 及时拒绝，
            // 不能等待读取超时后恰巧也得到 network。旧无界 reader 在此不能通过。
            match stream.read(&mut byte) {
                Ok(0) => {}
                Err(error) if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                ) => {}
                result => panic!("declared oversized image was not rejected before its body: {result:?}"),
            }
            return;
        }
        let image = png();
        let mut block = [0_u8; 64 * 1024];
        block[..image.len()].copy_from_slice(&image);
        let mut remaining = length;
        while remaining > 0 {
            let count = remaining.min(block.len() as u64) as usize;
            if matches!(framing, ReviewFraming::Chunked)
                && !review_send(write!(stream, "{count:X}\r\n"))
            {
                return;
            }
            if !review_send(stream.write_all(&block[..count])) {
                // 流式超限时可在预算+1处关闭。
                return;
            }
            if matches!(framing, ReviewFraming::Chunked)
                && !review_send(stream.write_all(b"\r\n"))
            {
                return;
            }
            if remaining == length {
                block.fill(0);
            }
            remaining -= count as u64;
        }
        if matches!(framing, ReviewFraming::Chunked) {
            review_send(stream.write_all(b"0\r\n\r\n"));
        }
    });
    (url, server)
}

fn review_selected_runtime(harness: &Harness) -> PathBuf {
    let runtime = harness.root.path().join("review-xberg");
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../resources/markdown-assets.json")).unwrap();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let relative = member["path"].as_str().unwrap();
        if jchtools_xberg::xberg_runtime::asset_for_scenario(relative, "snapshot") {
            let target = runtime.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, b"synthetic asset").unwrap();
        }
    }
    let legacy = harness.root.path().join("legacy-assets");
    std::fs::create_dir(&legacy).unwrap();
    std::fs::write(
        legacy.join("xberg-runtime-path.txt"),
        runtime.to_str().unwrap(),
    )
    .unwrap();
    runtime
}

fn review_run_bounded(harness: &Harness, args: &[&str]) -> Output {
    let mut child = harness.command(args, false).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!("URL capacity rejection did not finish within 30 seconds: {output:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn review_reject_oversized_url(framing: ReviewFraming) {
    // 同一测试进程串行大响应；避免三个旧行为子进程同时分配数百 MiB。
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let harness = Harness::new();
    review_selected_runtime(&harness);
    let (url, server) = review_image_server(framing, REVIEW_IMAGE_BUDGET + 1);
    let output = review_run_bounded(&harness, &[&url, "--json"]);
    server.join().unwrap();
    let error: JsonError = serde_json::from_str(failure(&output, 1)).unwrap();
    assert!(!error.ok, "{framing:?}");
    assert_eq!(error.error_kind, "network", "{framing:?}: {output:?}");
    assert!(!error.error.trim().is_empty(), "{framing:?}");
}

// P2-05：声明长度已超预算也不得继续进入 OCR。
#[test]
fn review_remediation_url_content_length_budget_plus_one_is_network_error() {
    review_reject_oversized_url(ReviewFraming::ContentLength);
}

// P2-05：不允许依赖 Content-Length，chunked 的第预算+1字节必须拒绝。
#[test]
fn review_remediation_url_chunked_budget_plus_one_is_network_error() {
    review_reject_oversized_url(ReviewFraming::Chunked);
}

// P2-05：HTTP/1.1 关闭连接定界的响应同样受容量边界保护。
#[test]
fn review_remediation_url_without_length_budget_plus_one_is_network_error() {
    review_reject_oversized_url(ReviewFraming::ConnectionClose);
}

// P2-05 / O-35：容量保护不能阻断合法小图片；使用真实 CLI/共享代理/管道，
// 合成引擎仅提供模型无关的 OCR 消费结果，不代表真实模型验证。
#[test]
fn review_remediation_small_url_preserves_successful_ocr_text() {
    let _session = engine_session::session_lock(repository_root());
    let harness = Harness::new();
    let runtime = review_selected_runtime(&harness);
    const ENGINE: &str = r#"
use std::io::{self, BufRead, Write};

fn field<'a>(line: &'a str, prefix: &str) -> &'a str {
    line.split(prefix).nth(1).unwrap_or("").split('"').next().unwrap_or("")
}

fn main() {
    let mut stdout = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let id = field(&line, "\"id\":\"");
        let command = field(&line, "\"command\":\"");
        let payload = if command == "ocr_snapshot" {
            "\"ok\":true,\"text\":\"  synthetic OCR result\\n\""
        } else {
            "\"ok\":true,\"commands\":[\"ocr_snapshot\",\"cancel\"],\"protocol_version\":2,\"cancellation\":\"cooperative\",\"timeout_ms\":true,\"document_snapshot_concurrent\":true"
        };
        writeln!(stdout, "{{\"id\":\"{id}\",{payload}}}").unwrap();
        stdout.flush().unwrap();
    }
}
"#;
    let mut compiler = Command::new("rustc")
        .args(["-C", "target-feature=+crt-static"])
        .args(["--edition=2021", "--crate-name", "jchocr_review_ocr", "-"])
        .arg("-o")
        .arg(runtime.join("xberg.exe"))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    compiler.stdin.take().unwrap().write_all(ENGINE.as_bytes()).unwrap();
    assert!(compiler.wait().unwrap().success());

    struct Broker(std::process::Child);
    impl Drop for Broker {
        fn drop(&mut self) {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = self.0.wait();
        }
    }
    let _broker = Broker(
        Command::new(env!("CARGO_BIN_EXE_jchocr"))
            .arg("--xberg-broker")
            .env("JCHTOOLS_TEST_STATE_DIR", harness.root.path().join("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let ready = std::fs::read_dir(harness.root.path().join("state/logs"))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .any(|log| log.contains("event=xberg_broker_listening"));
        if ready {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "isolated broker did not start");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    for framing in [
        ReviewFraming::ContentLength,
        ReviewFraming::Chunked,
        ReviewFraming::ConnectionClose,
    ] {
        let (url, server) = review_image_server(framing, png().len() as u64);
        let output = review_run_bounded(&harness, &[&url, "--json"]);
        server.join().unwrap();
        assert_eq!(output.status.code(), Some(0), "{framing:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{framing:?}: {output:?}");
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            result,
            serde_json::json!({"ok": true, "text": "  synthetic OCR result\n"}),
            "{framing:?}"
        );
    }
}
