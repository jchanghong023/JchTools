//! pipeline_backend 完整链路的 L2/L3 oracle 对照（O-23/O-24/O-26，附录 B）。
//!
//! 数据（均不入库，`.tmp/` 本地资产）：
//! - oracle：`.tmp/ocr-assets/oracle/{normal-1920x1080,4k-3840x2160}.{json,png}`、
//!   裁剪图 `.tmp/ocr-assets/oracle/crops/rec-*.png`（cv2.imwrite 存的 BGR 数组，
//!   读回 RGB 后换序即原始模型输入字节）；
//! - 模型 `.tmp/ocr-assets/oracle-models/PP-OCRv6_small_{det,rec}/inference.onnx`、
//!   字典 `.tmp/ocr-assets/release-staging/dict.txt`（18708 行）；
//! - 运行期 `ORT_DYLIB_PATH`（onnxruntime 1.28.0 x64 动态库）。
//!
//! 资产依赖测试显式 `#[ignore]`；验收时以 `cargo test -- --ignored` 运行，
//! 缺少本地资产直接失败，不把「未运行」记录成通过。
//!
//! 覆盖：
//! - L2（`l2_warp_crops_bitwise_oracle`）：两用例全部 consolidated 四边形经
//!   `warp_perspective_cubic_replicate`（原 PNG BGR）裁剪，与 oracle 逐调用
//!   裁剪逐字节一致（批 k=⌊i/8⌋、序 j=i mod 8 对应 rec-{k}-{j}.png）；
//! - L3（`l3_full_chain_matches_oracle`）：oracle 四边形直驱下游——
//!   透视裁剪 → recognize_records（增强/密集重试/旋转由
//!   管线驱动后端）→ assemble_spans → build_layout，全文逐字符一致，且
//!   rec_calls 三层（批构成/裁剪 SHA-256/文本与分数 ≤1e-6）逐项一致。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use snap_ocr_core::layout::build_layout;
use snap_ocr_core::pipeline::{assemble_spans, recognize_records};
use snap_ocr_core::types::{DetectionCandidate, TileRegion};
use snap_ocr_worker::image_ops::{self, BgrImage};
use snap_ocr_worker::pipeline_backend::{build_record, WorkerOcrBackend};

/// 与冻结 TextSnap 一致的推理线程数（`_DEFAULT_ENGINE_CONFIG`）。
const NUM_THREADS: usize = 10;
/// rec 分数对照容差（目标 ≤1e-6；实测残差为 f32 均值 1 ulp ≈ 1.2e-16）。
const SCORE_TOLERANCE: f64 = 1e-6;

const CASES: [&str; 2] = ["normal-1920x1080", "4k-3840x2160"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn assets() -> PathBuf {
    repo_root().join(".tmp/ocr-assets")
}

/// serde_json 不接受 python json.dump 产出的 `Infinity`/`NaN` 字面量
/// （oracle consolidated 段含 `Infinity`）：读入前净化为 `null`。
fn read_json_sanitized(path: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).unwrap();
    let cleaned = raw
        .replace(": Infinity", ": null")
        .replace(": -Infinity", ": null")
        .replace(": NaN", ": null");
    serde_json::from_str(&cleaned).unwrap()
}

/// PNG（RGB 存储）→ BGR 行主序。
fn load_bgr_png(path: &Path) -> BgrImage {
    let rgb = image::open(path).unwrap().to_rgb8();
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let mut data = Vec::with_capacity(w * h * 3);
    for px in rgb.pixels() {
        data.push(px[2]);
        data.push(px[1]);
        data.push(px[0]);
    }
    BgrImage::from_vec(w, h, data).unwrap()
}

/// oracle consolidated 四边形 → 候选（含 source_tile 元数据，供管线编排）。
fn oracle_candidates(oracle: &serde_json::Value) -> Vec<DetectionCandidate> {
    oracle["consolidated"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            let quad: Vec<(f64, f64)> = entry["quad"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
                .collect();
            let t = &entry["source_tile"];
            let tile = TileRegion::new(
                usize::try_from(t["index"].as_u64().unwrap_or_default()).unwrap(),
                u32::try_from(t["x"].as_u64().unwrap_or_default()).unwrap(),
                u32::try_from(t["y"].as_u64().unwrap_or_default()).unwrap(),
                u32::try_from(t["width"].as_u64().unwrap_or_default()).unwrap(),
                u32::try_from(t["height"].as_u64().unwrap_or_default()).unwrap(),
                u32::try_from(t["image_width"].as_u64().unwrap_or_default()).unwrap(),
                u32::try_from(t["image_height"].as_u64().unwrap_or_default()).unwrap(),
            )
            .unwrap();
            DetectionCandidate::new(
                [quad[0], quad[1], quad[2], quad[3]],
                entry["detection_score"].as_f64().unwrap(),
                tile,
                entry["internal_edge_distance"].as_f64().unwrap_or_default(),
                entry["touches_internal_edge"].as_bool().unwrap_or(false),
                Vec::new(),
            )
            .unwrap()
        })
        .collect()
}

/// 载入一个用例：`(原图 BGR, 净化后的 oracle JSON)`。
fn load_case(case: &str) -> (BgrImage, serde_json::Value) {
    let base = assets().join("oracle");
    (
        load_bgr_png(&base.join(format!("{case}.png"))),
        read_json_sanitized(&base.join(format!("{case}.json"))),
    )
}

/// 后端（两模型 + 字典；线程数同 TextSnap）。
fn load_backend() -> WorkerOcrBackend {
    WorkerOcrBackend::load(
        &assets().join("oracle-models/PP-OCRv6_small_det/inference.onnx"),
        &assets().join("oracle-models/PP-OCRv6_small_rec/inference.onnx"),
        &assets().join("release-staging/dict.txt"),
        NUM_THREADS,
    )
    .unwrap()
}

/// L2：两用例全部 consolidated 四边形的透视裁剪与 oracle 逐字节一致。
#[test]
#[ignore = "需本地 oracle 资产；由验收流程以 --ignored 显式运行"]
fn l2_warp_crops_bitwise_oracle() {
    let crops_dir = assets().join("oracle/crops");
    for case in CASES {
        let (bgr, oracle) = load_case(case);
        let candidates = oracle_candidates(&oracle);
        let rec_calls = oracle["rec_calls"].as_array().unwrap();
        // 编排顺序固定：初始批在前（⌈n/8⌉ 个），拉伸/旋转重试在后；
        // consolidated[i] ↔ rec_calls[⌊i/8⌋].crops[i mod 8] 仅对初始批成立。
        let initial_calls = candidates
            .len()
            .div_ceil(snap_ocr_core::pipeline::RECOGNITION_BATCH_SIZE);
        for (i, candidate) in candidates.iter().enumerate() {
            let crop = image_ops::warp_perspective_cubic_replicate(&bgr, candidate.quad()).unwrap();
            let call = i / snap_ocr_core::pipeline::RECOGNITION_BATCH_SIZE;
            let seq = i % snap_ocr_core::pipeline::RECOGNITION_BATCH_SIZE;
            assert!(
                call < initial_calls,
                "{case}: consolidated[{i}] 超出初始批范围（初始批 {initial_calls}）"
            );
            let entry = &rec_calls[call]["crops"].as_array().unwrap()[seq];
            let file = entry["file"].as_str().unwrap();
            let oracle_crop = load_bgr_png(&crops_dir.join(file));
            assert_eq!(
                (crop.width(), crop.height()),
                (oracle_crop.width(), oracle_crop.height()),
                "{case} consolidated[{i}] → {file} 尺寸不一致"
            );
            assert_eq!(
                crop.data(),
                oracle_crop.data(),
                "{case} consolidated[{i}] → {file} 逐字节比对失败"
            );
        }
    }
}

/// L3：oracle 四边形直驱下游完整链路，全文逐字符一致 +
/// rec_calls 三层（批构成/裁剪 SHA-256/文本与分数）逐项一致。
#[test]
#[ignore = "需本地 oracle 资产与 ORT_DYLIB_PATH；由验收流程以 --ignored 显式运行"]
fn l3_full_chain_matches_oracle() {
    let backend = load_backend();
    for case in CASES {
        let (bgr, oracle) = load_case(case);
        let candidates = oracle_candidates(&oracle);

        // 透视裁剪（ocr.py:530-551：校验失败/退化的候选跳过——oracle 用例无跳过）。
        let mut records = Vec::new();
        for candidate in candidates {
            let crop = image_ops::warp_perspective_cubic_replicate(&bgr, candidate.quad()).unwrap();
            records.push(build_record(candidate, crop));
        }

        recognize_records(&backend, &mut records, None).unwrap();
        let rec_calls = backend.take_rec_calls();
        let spans = assemble_spans(&records, None).unwrap();
        let text = if spans.is_empty() {
            String::new()
        } else {
            build_layout(&spans).text
        };

        // 全文逐字符（含空格/换行）。
        assert_eq!(
            text,
            oracle["text"].as_str().unwrap(),
            "{case}: L3 全文不一致"
        );

        // rec_calls 三层对照。
        let want_calls = oracle["rec_calls"].as_array().unwrap();
        assert_eq!(
            rec_calls.len(),
            want_calls.len(),
            "{case}: rec 调用数不一致"
        );
        for (ci, (mine, want)) in rec_calls.iter().zip(want_calls.iter()).enumerate() {
            assert_eq!(
                mine.batch,
                usize::try_from(want["batch"].as_u64().unwrap()).unwrap(),
                "{case} rec_calls[{ci}] 批大小不一致"
            );
            let want_crops = want["crops"].as_array().unwrap();
            assert_eq!(
                mine.crops.len(),
                want_crops.len(),
                "{case} rec_calls[{ci}] 裁剪数不一致"
            );
            for (j, (mc, wc)) in mine.crops.iter().zip(want_crops.iter()).enumerate() {
                assert_eq!(
                    mc.sha256,
                    wc["sha256"].as_str().unwrap(),
                    "{case} rec_calls[{ci}].crops[{j}] SHA-256 不一致（模型输入字节不同）"
                );
            }
            let want_texts = want["texts"].as_array().unwrap();
            let want_scores = want["scores"].as_array().unwrap();
            for (j, ((text_mine, score_mine), want_text)) in mine
                .texts
                .iter()
                .zip(&mine.scores)
                .zip(want_texts)
                .enumerate()
            {
                assert_eq!(
                    text_mine.as_str(),
                    want_text.as_str().unwrap(),
                    "{case} rec_calls[{ci}].texts[{j}] 不一致"
                );
                let score_want = want_scores[j].as_f64().unwrap();
                assert!(
                    (score_mine - score_want).abs() <= SCORE_TOLERANCE,
                    "{case} rec_calls[{ci}].scores[{j}] 偏差 {} 超过 {SCORE_TOLERANCE}",
                    score_mine - score_want
                );
            }
        }
    }
}
