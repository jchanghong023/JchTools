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
            let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
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
