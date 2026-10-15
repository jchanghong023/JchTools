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

//! 覆盖 XB-18：独立进程迁移旧目录配置，并从 SQLite 恢复；旧文件不删除。
use std::process::Command;

/// 覆盖 XB-03/XB-16/XB-19：场景成员隔离，不要求对方模型。
#[test]
fn scenario_assets_are_independent() {
    use jchtools_xberg::xberg_runtime::asset_for_scenario;
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
            jchtools_markdown::markdown_assets::load_saved_runtime_dir().unwrap(),
            Some(expected)
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("用户 Xberg");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::write(runtime.join("xberg.exe"), b"valid legacy runtime fixture").unwrap();
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
            jchtools_foundation::xberg_settings::save(&directory).unwrap();
            assert!(jchtools_foundation::xberg_settings::save(&root.join("不存在")).is_err());
        } else {
            assert_eq!(
                jchtools_markdown::markdown_assets::load_saved_runtime_dir().unwrap(),
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

/// 覆盖 XB-18/XB-21：首次保存下载来源也迁移未读取的旧文本，并跨进程保留双来源。
#[test]
fn downloaded_save_preserves_unloaded_legacy_text() {
    use jchtools_foundation::xberg_settings::{self, Source};
    if let Some(root) = std::env::var_os("JCHTOOLS_MIGRATION_TEST_CHILD") {
        let root = std::path::PathBuf::from(root);
        let custom = root.join("旧用户目录");
        let downloaded = root.join("下载目录");
        if std::env::var("JCHTOOLS_MIGRATION_TEST_ACTION").unwrap() == "save" {
            xberg_settings::save_source(Source::Downloaded, &downloaded).unwrap();
        } else {
            let saved = xberg_settings::settings().unwrap();
            assert_eq!(saved.source, Source::Downloaded);
            assert_eq!(saved.custom, Some(custom));
            assert_eq!(saved.downloaded, Some(downloaded.canonicalize().unwrap()));
            assert_eq!(
                xberg_settings::required().unwrap(),
                downloaded.canonicalize().unwrap()
            );
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let custom = root.path().join("旧用户目录");
    let downloaded = root.path().join("下载目录");
    std::fs::create_dir(&downloaded).unwrap();
    std::fs::write(downloaded.join("xberg.exe"), b"configuration fixture").unwrap();
    let legacy_root = root.path().join("markdown-assets");
    std::fs::create_dir(&legacy_root).unwrap();
    let legacy = legacy_root.join("xberg-runtime-path.txt");
    std::fs::write(&legacy, custom.to_str().unwrap()).unwrap();
    for action in ["save", "read"] {
        assert!(Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "downloaded_save_preserves_unloaded_legacy_text",
                "--nocapture"
            ])
            .env("JCHTOOLS_MIGRATION_TEST_CHILD", root.path())
            .env("JCHTOOLS_MIGRATION_TEST_ACTION", action)
            .env("JCHTOOLS_TEST_STATE_DIR", root.path())
            .env("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", &legacy_root)
            .status()
            .unwrap()
            .success());
    }
    assert_eq!(
        std::fs::read_to_string(legacy).unwrap(),
        custom.to_str().unwrap()
    );
}

/// XB-18/XB-21：无 GUI 发现、旧来源优先及保存失败在真实独立进程间保持一致。
#[test]
fn bundled_discovery_and_activation_preserve_existing_configuration() {
    use jchtools_foundation::xberg_settings::{self, Source};
    use std::path::PathBuf;
    if let Some(root) = std::env::var_os("JCHTOOLS_BUNDLE_SETTINGS_CHILD") {
        let root = PathBuf::from(root);
        let bundled = root.join("xberg");
        let old = root.join("旧用户目录");
        let downloaded = root.join("旧下载目录");
        match std::env::var("JCHTOOLS_BUNDLE_SETTINGS_ACTION")
            .unwrap()
            .as_str()
        {
            "fresh" => {
                assert!(!root.join("state/config.sqlite3").exists());
                assert_eq!(
                    xberg_settings::bundled_directory().unwrap(),
                    Some(bundled.clone())
                );
                assert_eq!(
                    xberg_settings::effective_directory().unwrap(),
                    Some(bundled.canonicalize().unwrap())
                );
                assert_eq!(
                    xberg_settings::required().unwrap(),
                    bundled.canonicalize().unwrap()
                );
                assert_eq!(xberg_settings::settings().unwrap().source, Source::Bundled);
            }
            "stored" => {
                assert_eq!(
                    xberg_settings::required().unwrap(),
                    bundled.canonicalize().unwrap()
                );
                assert_eq!(
                    xberg_settings::load().unwrap(),
                    Some(bundled.canonicalize().unwrap())
                );
            }
            "legacy" => {
                assert_eq!(xberg_settings::required().unwrap(), old);
                assert_eq!(xberg_settings::settings().unwrap().source, Source::Custom);
                xberg_settings::save_source(Source::Downloaded, &downloaded).unwrap();
                assert_eq!(
                    xberg_settings::required().unwrap(),
                    downloaded.canonicalize().unwrap()
                );
                xberg_settings::save_bundled_directory(&bundled).unwrap();
                let saved = xberg_settings::settings().unwrap();
                assert_eq!(saved.source, Source::Bundled);
                assert_eq!(saved.custom, Some(old));
                assert_eq!(saved.downloaded, Some(downloaded.canonicalize().unwrap()));
                assert_eq!(saved.bundled, Some(bundled.canonicalize().unwrap()));
                assert_eq!(
                    xberg_settings::required().unwrap(),
                    bundled.canonicalize().unwrap()
                );
            }
            "invalid" => {
                let legacy =
                    std::fs::read_to_string(root.join("legacy/xberg-runtime-path.txt")).unwrap();
                assert_eq!(
                    xberg_settings::required().unwrap(),
                    bundled.canonicalize().unwrap()
                );
                let saved = xberg_settings::settings().unwrap();
                assert_eq!(saved.custom, Some(PathBuf::from(legacy.trim())));
                assert_eq!(saved.source, Source::Bundled);
            }
            "save-failure" => {
                xberg_settings::save(&old).unwrap();
                let db = xberg_settings::open(&root.join("state")).unwrap();
                db.execute_batch(
                    "CREATE TRIGGER reject_bundle BEFORE INSERT ON app_settings
                     WHEN NEW.key='xberg_source' AND NEW.value='bundled'
                     BEGIN SELECT RAISE(ABORT, '拒绝切换'); END;",
                )
                .unwrap();
                assert!(xberg_settings::save_bundled_directory(&bundled).is_err());
                assert_eq!(
                    xberg_settings::required().unwrap(),
                    old.canonicalize().unwrap()
                );
                let saved = xberg_settings::settings().unwrap();
                assert_eq!(saved.source, Source::Custom);
                assert!(saved.bundled.is_none(), "失败事务不能留下半写入的随包来源");
                db.execute_batch(
                    "DROP TRIGGER reject_bundle;
                     UPDATE app_settings SET value='unknown' WHERE key='xberg_source';",
                )
                .unwrap();
                assert!(xberg_settings::effective_directory().is_err());
                assert!(xberg_settings::save_bundled_directory(&bundled).is_err());
                assert_eq!(
                    xberg_settings::load().unwrap(),
                    Some(old.canonicalize().unwrap())
                );
            }
            "discovery-save-failure" => {
                let db = xberg_settings::open(&root.join("state")).unwrap();
                db.execute_batch(
                    "CREATE TRIGGER reject_bundle BEFORE INSERT ON app_settings
                     WHEN NEW.key='xberg_source'
                     BEGIN SELECT RAISE(ABORT, '拒绝默认配置'); END;",
                )
                .unwrap();
                assert!(xberg_settings::effective_directory().is_err());
                assert!(xberg_settings::load().unwrap().is_none());
                assert!(xberg_settings::settings().unwrap().bundled.is_none());
            }
            "broken-storage" => {
                let database = root.join("state/config.sqlite3");
                let original = std::fs::read(&database).unwrap();
                assert!(xberg_settings::effective_directory().is_err());
                assert_eq!(std::fs::read(database).unwrap(), original);
            }
            "legacy-read-failure" => {
                assert!(xberg_settings::effective_directory().is_err());
                let db = xberg_settings::open(&root.join("state")).unwrap();
                let count: u64 = db
                    .query_row(
                        "SELECT COUNT(*) FROM app_settings WHERE key LIKE 'xberg_%'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(count, 0, "读取失败不能被随包默认覆盖");
            }
            "isolated" => {
                assert!(xberg_settings::bundled_directory().unwrap().is_none());
                assert!(xberg_settings::effective_directory().unwrap().is_none());
            }
            action => panic!("未知测试操作：{action}"),
        }
        return;
    }
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = repository.join(".tmp/bundled-settings-contract");
    assert!(Command::new("python")
        .arg(repository.join("scripts/make_tmp.py"))
        .args(["workspace", "--destination"])
        .arg(&workspace)
        .current_dir(repository)
        .status()
        .unwrap()
        .success());
    let run = |root: &std::path::Path, action: &str, bundle: bool| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "bundled_discovery_and_activation_preserve_existing_configuration",
                "--nocapture",
            ])
            .env("JCHTOOLS_BUNDLE_SETTINGS_CHILD", root)
            .env("JCHTOOLS_BUNDLE_SETTINGS_ACTION", action)
            .env("JCHTOOLS_TEST_STATE_DIR", root.join("state"))
            .env("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", root.join("legacy"));
        if bundle {
            command.env("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", root.join("xberg"));
        } else {
            command.env_remove("JCHTOOLS_TEST_BUNDLED_XBERG_DIR");
        }
        assert!(command.status().unwrap().success(), "操作失败：{action}");
    };
    for action in [
        "fresh",
        "legacy",
        "invalid",
        "relative-legacy",
        "save-failure",
        "discovery-save-failure",
        "broken-storage",
        "legacy-read-failure",
        "isolated",
    ] {
        let root = tempfile::tempdir_in(&workspace).unwrap();
        for directory in ["xberg", "旧用户目录", "旧下载目录"] {
            let directory = root.path().join(directory);
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(directory.join("xberg.exe"), "配置行为夹具，非真实引擎").unwrap();
        }
        if matches!(
            action,
            "legacy" | "invalid" | "relative-legacy" | "legacy-read-failure"
        ) {
            std::fs::create_dir(root.path().join("legacy")).unwrap();
            let legacy = root.path().join("legacy/xberg-runtime-path.txt");
            match action {
                "legacy-read-failure" => std::fs::create_dir(legacy).unwrap(),
                "relative-legacy" => std::fs::write(legacy, "relative-old-runtime").unwrap(),
                "invalid" => {
                    std::fs::write(legacy, root.path().join("不存在").to_str().unwrap()).unwrap()
                }
                _ => std::fs::write(legacy, root.path().join("旧用户目录").to_str().unwrap())
                    .unwrap(),
            }
        }
        if action == "broken-storage" {
            std::fs::create_dir(root.path().join("state")).unwrap();
            std::fs::write(root.path().join("state/config.sqlite3"), "不是 SQLite").unwrap();
        }
        run(
            root.path(),
            if action == "relative-legacy" {
                "invalid"
            } else {
                action
            },
            action != "isolated",
        );
        if action == "fresh" {
            run(root.path(), "stored", false);
        }
    }
}
