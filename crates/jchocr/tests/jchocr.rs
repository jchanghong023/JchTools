// CLI 仅支持 Windows；test-hooks 将全部配置重定向到自有临时目录。
#![cfg(all(windows, feature = "test-hooks"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! 覆盖 O-33～O-37：公开进程契约，不读取用户配置或依赖真实模型。
use std::{
    io::{Cursor, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::LazyLock,
};

struct Harness {
    root: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        static WORKSPACE: LazyLock<PathBuf> = LazyLock::new(|| {
            let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .find(|path| path.join("scripts/make_tmp.py").is_file())
                .expect("仓库根必须包含临时目录工厂");
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

    fn run(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_jchocr"))
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
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
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
