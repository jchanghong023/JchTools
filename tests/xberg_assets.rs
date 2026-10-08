//! 覆盖 XB-19：场景化固定清单校验（场景规则、清单同步、替换兼容与缺失路径）
//! 与 XB-16：共享引擎启动配置按场景声明模型目录、媒体可用性按文件在位判定。
//! 纯库口径，不启动代理或引擎进程。
#![allow(clippy::unwrap_used)]
use jchtools::xberg_runtime::{asset_for_scenario, startup_config, validate_assets};
use serde_json::Value;

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
            "models/paddleocr-onnx-models-LICENSE.txt",
            true,
            false,
            true,
        ),
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
        } else if path == "models/paddleocr-onnx-models-LICENSE.txt" {
            assert_eq!(
                matched,
                [true, false, true],
                "PaddleOCR 许可必须由截图与文档共用，媒体不依赖它"
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

// 覆盖 XB-19（XB-09 2026-10-02 修订后口径）：运行时对共享 Xberg 只做场景
// 成员存在性检查——缺失时明确报错指认成员；文件在场但字节与清单摘要不同
// （用户自行替换或更新引擎版本）必须放行，不再比对大小与摘要。
#[test]
fn validate_assets_presence_only_accepts_replaced_engine_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (path, _size_bytes) = first_required("snapshot");
    let error = validate_assets(root, "snapshot").unwrap_err();
    assert!(error.contains(&path), "缺失报错须指认成员：{error}");
    assert!(error.contains("缺失"), "缺失语义必须明确：{error}");

    // 全部 snapshot 成员以桩字节（大小与摘要均与清单不同）在场：等价于用户
    // 手动替换引擎文件后的目录状态，存在性口径下必须整单通过。
    let mut placed = 0_usize;
    for (member, _size) in manifest_members() {
        if !asset_for_scenario(&member, "snapshot") {
            continue;
        }
        let target = root.join(&member);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"replaced-engine").unwrap();
        placed += 1;
    }
    assert!(placed > 0, "清单须含 snapshot 场景成员");
    validate_assets(root, "snapshot").expect("替换引擎文件后应放行（运行时不比对摘要）");

    // 删除一个成员回到缺失：明确报错，不冒称就绪。
    std::fs::remove_file(root.join(&path)).unwrap();
    let error = validate_assets(root, "snapshot").unwrap_err();
    assert!(
        error.contains(&path) && error.contains("缺失"),
        "错误应指认被删除的成员：{error}"
    );
}

// 覆盖 XB-16：启动配置声明文档基线与媒体转录可用性；截图模型目录交还引擎
// exe 旁回退（同一共享运行目录布局），媒体可用性由 SenseVoice 模型在位决定。
#[test]
fn startup_config_declares_scenario_models_and_media_availability() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config = startup_config(root).unwrap();
    assert!(
        config.get("ocr").is_some(),
        "文档场景配置须来自内置基线（markdown-xberg.json）"
    );
    assert!(
        config.get("snapshot_ocr").is_none(),
        "截图模型目录交还引擎 exe 旁回退（同一共享运行目录布局）"
    );
    assert_eq!(config["transcription"]["enabled"], false);

    let model = root.join("models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx");
    std::fs::create_dir_all(model.parent().unwrap()).unwrap();
    std::fs::write(&model, b"model placeholder").unwrap();
    let config = startup_config(root).unwrap();
    assert_eq!(config["transcription"]["enabled"], true);
}

// 真实 Xberg 组件目录的端到端存在性验证（默认 #[ignore] 的真实引擎用例）：
// 设 JCHTOOLS_REAL_XBERG_DIR 指向本机组件目录（AGENTS.md §3 的 run54.1 测试
// 目录）后以 --ignored 运行，全部场景的存在性检查必须通过——锚定「真实完整
// 目录必须通过」的下限；替换引擎文件后的放行语义由
// validate_assets_presence_only_accepts_replaced_engine_files 覆盖。
#[test]
#[ignore = "需真实 Xberg 目录：设 JCHTOOLS_REAL_XBERG_DIR 后 --ignored 运行"]
fn real_xberg_dir_passes_presence_checks() {
    let dir = std::env::var("JCHTOOLS_REAL_XBERG_DIR").expect("设置 JCHTOOLS_REAL_XBERG_DIR");
    let root = std::path::PathBuf::from(dir);
    assert!(root.is_dir(), "目录必须存在：{}", root.display());
    for scenario in ["engine", "snapshot", "media", "document"] {
        validate_assets(&root, scenario).unwrap_or_else(|error| {
            panic!("真实目录在 {scenario} 场景必须通过存在性检查：{error}")
        });
    }
}
