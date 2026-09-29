//! 覆盖 XB-19：场景化固定清单校验（规则、与清单同步、缺失/大小/摘要失败路径）
//! 与 XB-16：共享引擎启动配置按场景声明模型目录、媒体可用性按文件在位判定。
//! 纯库口径，不启动代理或引擎进程。
#![allow(clippy::unwrap_used)]
use jchtools::xberg_runtime::{asset_for_scenario, startup_config, validate_assets};
use serde_json::Value;
use std::path::Path;

/// 与 `validate_assets` 内置的同一份固定清单（resources/markdown-assets.json）。
fn manifest_members() -> Vec<(String, u64)> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/resources/markdown-assets.json"
    ))
    .unwrap();
    let manifest: Value = serde_json::from_str(&text).unwrap();
    manifest["xberg"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| {
            (
                member["path"].as_str().unwrap().to_owned(),
                member["size_bytes"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// 清单顺序下第一个属于该场景的成员（validate_assets 首个被校验对象）。
fn first_required(scenario: &str) -> (String, u64) {
    manifest_members()
        .into_iter()
        .find(|(path, _)| asset_for_scenario(path, scenario))
        .unwrap()
}

// 覆盖 XB-19：路径归属规则——samples 与 xberg.cmd 不校验；截图、媒体、文档
// 模型互斥；公共 EXE/运行库对所有场景必校验；固定清单与规则保持同步。
#[test]
fn scenario_rules_assign_every_fixed_manifest_member() {
    let scenarios = ["snapshot", "media", "document"];
    let classified = |path: &str, scenario: &str| asset_for_scenario(path, scenario);
    for (path, snapshot, media, document) in [
        ("samples/readme.md", false, false, false),
        ("xberg.cmd", false, false, false),
        ("models/snapshot-ocr/infer.onnx", true, false, false),
        (
            "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx",
            false,
            true,
            false,
        ),
        ("models/vad/silero.onnx", false, true, false),
        ("sherpa-onnx/lib.dll", false, true, false),
        ("ffmpeg/avcodec-63.dll", false, true, false),
        ("models/doc-tiny/model.onnx", false, false, true),
        ("MSVCP140.dll", true, true, true),
        ("onnxruntime.dll", true, true, true),
        ("xberg.exe", true, true, true),
    ] {
        assert_eq!(classified(path, "snapshot"), snapshot, "{path}");
        assert_eq!(classified(path, "media"), media, "{path}");
        assert_eq!(classified(path, "document"), document, "{path}");
    }
    let mut exclusive = [false; 3];
    for (path, _) in manifest_members() {
        let matched: Vec<bool> = scenarios
            .iter()
            .map(|scenario| asset_for_scenario(&path, scenario))
            .collect();
        let count = matched.iter().filter(|hit| **hit).count();
        if count == 0 {
            assert!(
                path.starts_with("samples/") || path == "xberg.cmd",
                "清单成员 {path} 不被任何场景校验，须显式声明为 samples/xberg.cmd 例外"
            );
        } else if path.starts_with("models/") {
            assert_eq!(count, 1, "模型成员 {path} 必须恰好归属一个场景");
        }
        for (index, hit) in matched.iter().enumerate() {
            if *hit && count == 1 {
                exclusive[index] = true;
            }
        }
    }
    assert!(
        exclusive.iter().all(|has| *has),
        "每个场景都必须有专属模型成员，否则该场景的固定清单校验形同虚设"
    );
}

// 覆盖 XB-19：缺失、大小不符、摘要不符逐一明确报错，不得静默跳过或冒称就绪。
#[test]
fn validate_assets_fails_closed_on_missing_size_and_digest() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (path, size_bytes) = first_required("snapshot");
    let error = validate_assets(root, "snapshot").unwrap_err();
    assert!(error.contains(&path), "缺失报错须指认成员：{error}");
    assert!(error.contains("不可读"), "{error}");

    std::fs::create_dir_all(root.join(Path::new(&path).parent().unwrap())).unwrap();
    std::fs::write(
        root.join(&path),
        vec![b'x'; usize::try_from(size_bytes).unwrap() + 1],
    )
    .unwrap();
    let error = validate_assets(root, "snapshot").unwrap_err();
    assert!(
        error.contains(&path) && error.contains("大小与固定版本清单不符"),
        "{error}"
    );

    std::fs::write(
        root.join(&path),
        vec![0u8; usize::try_from(size_bytes).unwrap()],
    )
    .unwrap();
    let error = validate_assets(root, "snapshot").unwrap_err();
    assert!(
        error.contains(&path) && error.contains("摘要与固定版本清单不符"),
        "{error}"
    );
}

// 覆盖 XB-16：启动配置一次声明三个场景——截图模型目录指向共享运行目录，
// 媒体转录可用性由 SenseVoice 模型在位决定，文档配置来自内置基线。
#[test]
fn startup_config_declares_scenario_models_and_media_availability() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config = startup_config(root).unwrap();
    assert!(
        config.get("ocr").is_some(),
        "文档场景配置须来自内置基线（markdown-xberg.json）"
    );
    assert_eq!(
        config["snapshot_ocr"]["models_dir"].as_str(),
        root.join("models/snapshot-ocr").to_str(),
        "截图模型目录须指向共享运行目录内"
    );
    assert_eq!(config["transcription"]["enabled"], false);

    let model = root.join("models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx");
    std::fs::create_dir_all(model.parent().unwrap()).unwrap();
    std::fs::write(&model, b"model placeholder").unwrap();
    let config = startup_config(root).unwrap();
    assert_eq!(config["transcription"]["enabled"], true);
}
