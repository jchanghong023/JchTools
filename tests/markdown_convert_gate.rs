#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]

//! 转 Markdown 批处理公开入口回归（媒体目录分配与扫描取消）。
//! T-18 页数探测与自动分流已于 2026-10-04 内化 Xberg 引擎（`auto_fast_pages`
//! 阈值探测，降级经 `processing_warnings` 的 auto_mode 来源披露），本项目不再
//! 实现页数预检，也不再发送 mode；相关覆盖随跨仓接口改造移至 Xberg fulltest。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use jchtools_markdown::markdown::{
    test_media_dir_name, test_scan_count_with_cancel, test_scan_media_dirs,
    test_write_new_markdown, FormatGroup, Options,
};
use jchtools_markdown::markdown_document::MediaFile;
use std::sync::atomic::AtomicBool;

fn test_dir() -> tempfile::TempDir {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".tmp");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

struct EnvironmentGuard {
    state_dir: Option<std::ffi::OsString>,
    broker_exe: Option<std::ffi::OsString>,
}

impl EnvironmentGuard {
    fn capture() -> Self {
        Self {
            state_dir: std::env::var_os("JCHTOOLS_TEST_STATE_DIR"),
            broker_exe: std::env::var_os("JCHTOOLS_TEST_BROKER_EXE"),
        }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        for (name, value) in [
            ("JCHTOOLS_TEST_STATE_DIR", &self.state_dir),
            ("JCHTOOLS_TEST_BROKER_EXE", &self.broker_exe),
        ] {
            if let Some(value) = value {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
    }
}

// 覆盖 T-10/T-14/T-25：空白折叠不能让不同结果共享媒体目录；层级输出按实际目标父目录
// 分配并在同一批次内保留唯一目录。
#[test]
fn media_directories_are_unique_after_name_sanitization() {
    let temp = test_dir();
    let input = temp.path().join("input");
    let output = temp.path().join("output");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(input.join("a b.docx"), b"one").unwrap();
    std::fs::write(input.join("a_b.docx"), b"two").unwrap();
    let options = Options {
        input_dir: input,
        output_dir: output,
        flat: false,
        groups: vec![FormatGroup::Office],
        timeout_secs: 1,
    };
    let dirs = test_scan_media_dirs(&options).unwrap();
    assert_eq!(dirs.len(), 2);
    assert_ne!(dirs[0], dirs[1]);
}

// 覆盖 T-14/T-25/S-01：已有媒体图片不得被覆盖，也不得在失败回滚时删除。
#[test]
fn media_commit_refuses_existing_image_without_deleting_it() {
    let temp = test_dir();
    let target = temp.path().join("a_docx.md");
    let media_dir = temp.path().join("a_docx_media");
    std::fs::create_dir_all(&media_dir).unwrap();
    let image = media_dir.join("image_0.png");
    std::fs::write(&image, b"old").unwrap();
    let media = vec![MediaFile {
        relative: "a_docx_media/image_0.png".to_string(),
        bytes: b"new".to_vec(),
    }];
    let error = test_write_new_markdown(temp.path(), &target, "body", &media).unwrap_err();
    assert!(error.contains("已存在"));
    assert_eq!(std::fs::read(&image).unwrap(), b"old");
    assert!(!target.exists());
}

// 覆盖 T-14：媒体目录占用检查必须作用于目标所在层级，而不是只检查输出根。
#[test]
fn media_directory_occupancy_uses_target_parent() {
    let temp = test_dir();
    let docs = temp.path().join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a_docx_media"), b"occupied").unwrap();
    let target = docs.join("a_docx.md");
    assert_eq!(
        test_media_dir_name(temp.path(), &target).unwrap(),
        "a_docx_2_media"
    );
}

// 覆盖 T-22/T-23：扫描入口在开始前收到停止请求时必须立即取消。
#[test]
fn scan_honors_cancellation_before_traversal() {
    let temp = test_dir();
    let input = temp.path().join("input");
    let output = temp.path().join("output");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::create_dir_all(&output).unwrap();
    let options = Options {
        input_dir: input,
        output_dir: output,
        flat: false,
        groups: vec![FormatGroup::Pdf],
        timeout_secs: 1,
    };
    let cancel = AtomicBool::new(true);
    let error = test_scan_count_with_cancel(&options, &cancel).unwrap_err();
    assert!(error.contains("取消"));
}

// 覆盖 T-08/XB-09/XB-26：使用当前配置引擎的格式集合，不把内置发布清单
// 当作用户替换后的能力；共享合成引擎支持 txt 和必需文档类型，不支持 rtf。
#[test]
fn batch_uses_selected_engine_formats() {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temp = test_dir();
    let root = temp.path();
    common::mock_engine_copy("tests/fixtures/shared_xberg.rs", &root.join("xberg.exe"));
    // 在持有会话锁期间恢复进程级测试配置，包括断言 panic 的路径。
    let _environment = EnvironmentGuard::capture();
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.join("state"));
    std::env::set_var("JCHTOOLS_TEST_BROKER_EXE", env!("CARGO_BIN_EXE_JchTools"));
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../resources/markdown-assets.json")).unwrap();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let relative = member["path"].as_str().unwrap();
        if relative != "xberg.exe" {
            let target = root.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, b"synthetic asset").unwrap();
        }
    }
    jchtools_markdown::markdown_assets::save_runtime_dir(root).unwrap();
    let input = root.join("input");
    let output = root.join("output");
    std::fs::create_dir(&input).unwrap();
    std::fs::create_dir(&output).unwrap();
    std::fs::write(input.join("a.txt"), b"synthetic text").unwrap();
    std::fs::write(input.join("b.rtf"), b"synthetic unsupported input").unwrap();
    let state = jchtools_xberg::xberg_runtime::request(
        root,
        serde_json::json!({"command":"snapshot_state"}),
        std::time::Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .and_then(jchtools_xberg::xberg_runtime::checked)
    .unwrap();
    struct BrokerGuard(u64);
    impl Drop for BrokerGuard {
        fn drop(&mut self) {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &self.0.to_string(), "/T", "/F"])
                .output();
        }
    }
    let _broker = BrokerGuard(state["jchtools_broker_pid"].as_u64().unwrap());
    let options = Options {
        input_dir: input.clone(),
        output_dir: output.clone(),
        flat: false,
        groups: vec![FormatGroup::Other],
        timeout_secs: 15,
    };
    let mut started = Vec::new();
    let summary = jchtools_markdown::markdown::run(&options, &AtomicBool::new(false), |event| {
        if let jchtools_markdown::markdown::Event::FileStarted { relative, .. } = event {
            started.push(relative);
        }
    })
    .unwrap();
    assert_eq!(summary.success, 1, "引擎未声明的 rtf 不得入队转换");
    assert_eq!(summary.failed, 0);
    assert_eq!(started, vec![std::path::PathBuf::from("a.txt")]);
    assert_eq!(std::fs::read(output.join("a_txt.md")).unwrap(), b"document");
    assert!(!output.join("b_rtf.md").exists());
    assert_eq!(
        std::fs::read(input.join("a.txt")).unwrap(),
        b"synthetic text"
    );
    assert_eq!(
        std::fs::read(input.join("b.rtf")).unwrap(),
        b"synthetic unsupported input"
    );
}

// 覆盖 T-14/T-17/T-25：公开批处理入口接收含复杂 title 与尖括号目标的
// 引擎正文后，图片/普通链接的字面 title 不改写，真实引用加媒体目录前缀并一起落盘。
#[test]
fn batch_preserves_image_titles_and_commits_angle_bracket_media() {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temp = test_dir();
    let root = temp.path();
    let content = "![外图](external_(1).png \"说明 ) ![字面](image_0.png)\")\n[外链](external.md \"说明 ![字面](image_0.png)\")\n![图](<image_0.png> \"说明 )\")";
    let payload = serde_json::json!({
        "document": {
            "content": content,
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}],
            "processing_warnings": []
        },
        "warnings": []
    })
    .to_string();
    // 复用已有 worker 协议夹具，只替换 extract 的合成返回内容；源文件只读。
    let fixture = include_str!("fixtures/shared_xberg.rs");
    let original = r#""\"document\":{\"content\":\"document\",\"images\":[],\"processing_warnings\":[]},\"warnings\":[]".to_owned()"#;
    assert_eq!(fixture.matches(original).count(), 1, "夹具响应替换必须唯一");
    let fields = payload
        .strip_prefix('{')
        .unwrap()
        .strip_suffix('}')
        .unwrap();
    let engine_source = root.join("markdown_media_links.rs");
    std::fs::write(
        &engine_source,
        fixture.replace(original, &format!("{fields:?}.to_owned()")),
    )
    .unwrap();
    common::mock_engine_copy(engine_source.to_str().unwrap(), &root.join("xberg.exe"));
    let _environment = EnvironmentGuard::capture();
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.join("state"));
    std::env::set_var("JCHTOOLS_TEST_BROKER_EXE", env!("CARGO_BIN_EXE_JchTools"));
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../resources/markdown-assets.json")).unwrap();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let relative = member["path"].as_str().unwrap();
        if relative != "xberg.exe" {
            let target = root.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, b"synthetic asset").unwrap();
        }
    }
    jchtools_markdown::markdown_assets::save_runtime_dir(root).unwrap();
    let state = jchtools_xberg::xberg_runtime::request(
        root,
        serde_json::json!({"command":"snapshot_state"}),
        std::time::Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .and_then(jchtools_xberg::xberg_runtime::checked)
    .unwrap();
    struct BrokerGuard(u64);
    impl Drop for BrokerGuard {
        fn drop(&mut self) {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &self.0.to_string(), "/T", "/F"])
                .output();
        }
    }
    let _broker = BrokerGuard(state["jchtools_broker_pid"].as_u64().unwrap());
    let input = root.join("input");
    let output = root.join("output");
    std::fs::create_dir(&input).unwrap();
    std::fs::create_dir(&output).unwrap();
    std::fs::write(input.join("a.txt"), b"synthetic source").unwrap();
    let options = Options {
        input_dir: input.clone(),
        output_dir: output.clone(),
        flat: false,
        groups: vec![FormatGroup::Other],
        timeout_secs: 15,
    };
    let summary =
        jchtools_markdown::markdown::run(&options, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(summary.success, 1);
    assert_eq!(summary.partial, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(std::fs::read_to_string(output.join("a_txt.md")).unwrap(),
        "![外图](external_(1).png \"说明 ) ![字面](image_0.png)\")\n[外链](external.md \"说明 ![字面](image_0.png)\")\n![图](<a_txt_media/image_0.png> \"说明 )\")");
    assert_eq!(
        std::fs::read(output.join("a_txt_media/image_0.png")).unwrap(),
        b"hello"
    );
    assert_eq!(
        std::fs::read(input.join("a.txt")).unwrap(),
        b"synthetic source"
    );
    assert_eq!(
        std::fs::read_dir(&output).unwrap().count(),
        2,
        "无未完成临时结果"
    );
}

fn review_fix_run_link_batch(
    content: &str,
    images: serde_json::Value,
    expected_markdown: &str,
    expected_media: &[(&str, &[u8])],
) {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temp = test_dir();
    let root = temp.path();
    let payload = serde_json::json!({
        "document": {
            "content": content,
            "images": images,
            "processing_warnings": []
        },
        "warnings": []
    })
    .to_string();
    // 复用真实共享代理协议夹具；只提供合成引擎最终正文，不模拟改写或落盘。
    let fixture = include_str!("fixtures/shared_xberg.rs");
    let original = r#""\"document\":{\"content\":\"document\",\"images\":[],\"processing_warnings\":[]},\"warnings\":[]".to_owned()"#;
    assert_eq!(fixture.matches(original).count(), 1, "夹具响应替换必须唯一");
    let fields = payload
        .strip_prefix('{')
        .unwrap()
        .strip_suffix('}')
        .unwrap();
    let engine_source = root.join("review_fix_markdown_links.rs");
    std::fs::write(
        &engine_source,
        fixture.replace(original, &format!("{fields:?}.to_owned()")),
    )
    .unwrap();
    common::mock_engine_copy(engine_source.to_str().unwrap(), &root.join("xberg.exe"));
    let _environment = EnvironmentGuard::capture();
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.join("state"));
    std::env::set_var("JCHTOOLS_TEST_BROKER_EXE", env!("CARGO_BIN_EXE_JchTools"));
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../resources/markdown-assets.json")).unwrap();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let relative = member["path"].as_str().unwrap();
        if relative != "xberg.exe" {
            let target = root.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, b"synthetic asset").unwrap();
        }
    }
    jchtools_markdown::markdown_assets::save_runtime_dir(root).unwrap();
    let state = jchtools_xberg::xberg_runtime::request(
        root,
        serde_json::json!({"command":"snapshot_state"}),
        std::time::Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .and_then(jchtools_xberg::xberg_runtime::checked)
    .unwrap();
    struct BrokerGuard(u64);
    impl Drop for BrokerGuard {
        fn drop(&mut self) {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &self.0.to_string(), "/T", "/F"])
                .output();
        }
    }
    let _broker = BrokerGuard(state["jchtools_broker_pid"].as_u64().unwrap());
    let input = root.join("input");
    let output = root.join("output");
    std::fs::create_dir(&input).unwrap();
    std::fs::create_dir(&output).unwrap();
    let source_bytes = b"synthetic source kept byte-for-byte";
    std::fs::write(input.join("a.txt"), source_bytes).unwrap();
    let options = Options {
        input_dir: input.clone(),
        output_dir: output.clone(),
        flat: false,
        groups: vec![FormatGroup::Other],
        timeout_secs: 15,
    };
    let summary =
        jchtools_markdown::markdown::run(&options, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(summary.success, 1);
    assert_eq!(summary.partial, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(
        std::fs::read(output.join("a_txt.md")).unwrap(),
        expected_markdown.as_bytes(),
        "除真实媒体目标之外，引擎最终 Markdown 必须逐字节保留"
    );
    assert_eq!(std::fs::read(input.join("a.txt")).unwrap(), source_bytes);
    let mut expected_entries =
        std::collections::BTreeSet::from([std::ffi::OsString::from("a_txt.md")]);
    if expected_media.is_empty() {
        assert!(
            !output.join("a_txt_media").exists(),
            "代码字面量不得产生媒体目录"
        );
    } else {
        expected_entries.insert(std::ffi::OsString::from("a_txt_media"));
        for &(filename, bytes) in expected_media {
            assert_eq!(
                std::fs::read(output.join("a_txt_media").join(filename)).unwrap(),
                bytes
            );
        }
        let media_entries: std::collections::BTreeSet<_> =
            std::fs::read_dir(output.join("a_txt_media"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
        let expected_files: std::collections::BTreeSet<_> = expected_media
            .iter()
            .map(|(name, _)| std::ffi::OsString::from(*name))
            .collect();
        assert_eq!(
            media_entries, expected_files,
            "title 字面量不落盘且无媒体中间文件"
        );
    }
    let output_entries: std::collections::BTreeSet<_> = std::fs::read_dir(&output)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        output_entries, expected_entries,
        "只保留完整提交结果，无中间文件"
    );
}

// 覆盖 T-14/T-17/T-25：公开批处理入口不得把普通链接 label 中开启的
// 跨行行内代码当作图片；最终 Markdown 完全原样且不产生媒体成品。
#[test]
fn review_fix_batch_preserves_link_label_multiline_code_without_media() {
    let content = "[说明 `字面](external.md)\n![代码](image_0.png)`";
    review_fix_run_link_batch(
        content,
        serde_json::json!([{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]),
        content,
        &[],
    );
}

// 覆盖 T-14/T-17/T-25：公开入口对非法外层链接中的真实图片和合法 label
// 图片分别落盘；合法 title 内的字面图片不生成文件，输入与正文其余部分不变。
#[test]
fn review_fix_batch_materializes_invalid_link_image_and_preserves_valid_titles() {
    let content = concat!(
        "[说明](external.md ![图](image_0.png))\n",
        "[外链](external.md \"说明 ![字面](image_1.png)\")\n",
        "[![标签](image_2.png)](external.md '![字面](image_1.png)')"
    );
    let expected = concat!(
        "[说明](external.md ![图](a_txt_media/image_0.png))\n",
        "[外链](external.md \"说明 ![字面](image_1.png)\")\n",
        "[![标签](a_txt_media/image_2.png)](external.md '![字面](image_1.png)')"
    );
    review_fix_run_link_batch(
        content,
        serde_json::json!([
            {"image_index": 0, "format": "png", "data_base64": "aGVsbG8="},
            {"image_index": 1, "format": "png", "data": [222, 173]},
            {"image_index": 2, "format": "png", "data": [0, 255, 37]}
        ]),
        expected,
        &[("image_0.png", b"hello"), ("image_2.png", &[0, 255, 37])],
    );
}
