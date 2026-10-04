//! 覆盖 XB-18：独立进程迁移旧目录配置，并从 SQLite 恢复；旧文件不删除。
use std::process::Command;

/// 覆盖 XB-03/XB-16/XB-19：不同场景不要求对方模型，也不改写对方配置。
#[test]
fn scenario_assets_and_configuration_are_independent() {
    use jchtools::xberg_runtime::{asset_for_scenario, startup_config};
    let root = tempfile::tempdir().unwrap();
    let config = startup_config(root.path()).unwrap();
    assert!(
        config.get("snapshot_ocr").is_none(),
        "截图模型目录交还引擎 exe 旁回退（同一共享运行目录布局）"
    );
    assert_eq!(config["transcription"]["enabled"], false);
    // T-13（2026-10-04 确认）：调参细节交还引擎默认；启动基线只保留与引擎
    // 默认不同的关键差异——Markdown 输出、中文 OCR 与图片字节通道。
    assert_eq!(config["ocr"]["language"][0], "ch");
    assert_eq!(config["images"]["include_data_base64"], true);
    for scenario in ["document", "snapshot", "media"] {
        assert!(asset_for_scenario("xberg.exe", scenario));
        assert_eq!(
            asset_for_scenario("models/snapshot-ocr/det.onnx", scenario),
            scenario == "snapshot"
        );
        assert_eq!(
            asset_for_scenario(
                "models/models--xberg-io--paddleocr-onnx-models/v6/det/tiny/model.onnx",
                scenario
            ),
            scenario == "document"
        );
        assert_eq!(
            asset_for_scenario(
                "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx",
                scenario
            ),
            scenario == "media"
        );
        assert_eq!(
            asset_for_scenario("ffmpeg/avcodec-63.dll", scenario),
            scenario == "media"
        );
    }
}

#[test]
fn persisted_directory_survives_process_restart() {
    if let Some(root) = std::env::var_os("JCHTOOLS_SETTINGS_TEST_CHILD") {
        let expected = std::path::PathBuf::from(root).join("用户 Xberg");
        assert_eq!(
            jchtools::markdown_assets::load_saved_runtime_dir().unwrap(),
            Some(expected)
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("markdown-assets");
    std::fs::create_dir_all(&root).unwrap();
    let legacy = root.join("xberg-runtime-path.txt");
    std::fs::write(&legacy, temp.path().join("用户 Xberg").to_str().unwrap()).unwrap();
    let run = || {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "persisted_directory_survives_process_restart",
                "--nocapture",
            ])
            .env("JCHTOOLS_SETTINGS_TEST_CHILD", temp.path())
            .env("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", &root)
            .env("JCHTOOLS_TEST_STATE_DIR", temp.path())
            .status()
            .unwrap()
    };
    assert!(run().success());
    assert!(
        temp.path().join("config.sqlite3").is_file(),
        "必须迁入应用级 SQLite"
    );
    assert!(legacy.is_file(), "必须保留旧配置");
    std::fs::write(&legacy, "C:/旧配置不得覆盖 SQLite").unwrap();
    assert!(run().success(), "新进程必须优先读取已持久化的 SQLite 值");
}

/// 覆盖 XB-18/XB-19：真实保存入口写 SQLite，完全退出后读取；失败保存保留旧值。
#[test]
fn saved_directory_is_restored_and_invalid_save_preserves_it() {
    if let Some(root) = std::env::var_os("JCHTOOLS_SAVE_TEST_CHILD") {
        let root = std::path::PathBuf::from(root);
        let directory = root.join("中文 Xberg");
        if std::env::var("JCHTOOLS_SAVE_TEST_ACTION").unwrap() == "save" {
            jchtools::xberg_settings::save(&directory).unwrap();
            assert!(jchtools::xberg_settings::save(&root.join("不存在")).is_err());
        } else {
            assert_eq!(
                jchtools::markdown_assets::load_saved_runtime_dir().unwrap(),
                Some(directory.canonicalize().unwrap())
            );
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("中文 Xberg");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("xberg.exe"), b"settings-only fixture").unwrap();
    for action in ["save", "read"] {
        assert!(Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "saved_directory_is_restored_and_invalid_save_preserves_it",
                "--nocapture"
            ])
            .env("JCHTOOLS_SAVE_TEST_CHILD", root.path())
            .env("JCHTOOLS_SAVE_TEST_ACTION", action)
            .env("JCHTOOLS_TEST_STATE_DIR", root.path())
            .status()
            .unwrap()
            .success());
    }
    let header = std::fs::read(root.path().join("config.sqlite3")).unwrap();
    assert!(header.starts_with(b"SQLite format 3\0"));
    assert!(!root
        .path()
        .join("markdown-assets/xberg-runtime-path.txt")
        .exists());
}
