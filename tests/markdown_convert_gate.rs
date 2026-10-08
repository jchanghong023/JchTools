//! 转 Markdown 批处理公开入口回归（媒体目录分配与扫描取消）。
//! T-18 页数探测与自动分流已于 2026-10-04 内化 Xberg 引擎（`auto_fast_pages`
//! 阈值探测，降级经 `processing_warnings` 的 auto_mode 来源披露），本项目不再
//! 实现页数预检，也不再发送 mode；相关覆盖随跨仓接口改造移至 Xberg fulltest。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use jchtools::markdown::{
    test_media_dir_name, test_scan_count_with_cancel, test_scan_media_dirs,
    test_write_new_markdown, FormatGroup, Options,
};
use jchtools::markdown_document::MediaFile;
use std::sync::atomic::AtomicBool;

// 覆盖 T-10/T-14/T-25：空白折叠不能让不同结果共享媒体目录；层级输出按实际目标父目录
// 分配并在同一批次内保留唯一目录。
#[test]
fn media_directories_are_unique_after_name_sanitization() {
    let temp = tempfile::tempdir().unwrap();
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
    let temp = tempfile::tempdir().unwrap();
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
    let temp = tempfile::tempdir().unwrap();
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
    let temp = tempfile::tempdir().unwrap();
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
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    common::mock_engine_copy("tests/fixtures/shared_xberg.rs", &root.join("xberg.exe"));
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
    jchtools::markdown_assets::save_runtime_dir(root).unwrap();
    let input = root.join("input");
    let output = root.join("output");
    std::fs::create_dir(&input).unwrap();
    std::fs::create_dir(&output).unwrap();
    std::fs::write(input.join("a.txt"), b"synthetic text").unwrap();
    std::fs::write(input.join("b.rtf"), b"synthetic unsupported input").unwrap();
    let state = jchtools::xberg_runtime::request(
        root,
        serde_json::json!({"command":"snapshot_state"}),
        std::time::Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .and_then(jchtools::xberg_runtime::checked)
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
    let summary = jchtools::markdown::run(&options, &AtomicBool::new(false), |event| {
        if let jchtools::markdown::Event::FileStarted { relative, .. } = event {
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
        std::fs::read(input.join("b.rtf")).unwrap(),
        b"synthetic unsupported input"
    );
}
