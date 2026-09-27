//! det_paddlex 的 oracle 验收与 image_ops 广谱差分测试（O-24 / 附录 B）。
//!
//! 数据（均不入库，`.tmp/` 本地资产）：
//! - oracle：`.tmp/ocr-assets/oracle/{normal-1920x1080,4k-3840x2160}.{json,png}`、
//!   模型 `.tmp/ocr-assets/oracle-models/PP-OCRv6_small_det/inference.onnx`、
//!   运行期 `ORT_DYLIB_PATH`（onnxruntime 1.28.0 x64）；
//! - 分级 dump：`.tmp/det-align/dump/`（python 探针对 paddlex 链路逐级导出）；
//! - 算子差分：`.tmp/ocr-assets/diff-ops/`（`diff_ops.py` 生成的逐位 dump）。
//!
//! 资产依赖测试显式 `#[ignore]`；手动以 `--ignored` 运行时，瓦片的
//! dump 和 manifest 记录必须齐全，缺失即失败，不能静默跳过。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::path::{Path, PathBuf};

use snap_ocr_worker::det_paddlex::{__test_resize_type0, DetPaddlex, __TEST_BINARIZE_THRESH};
use snap_ocr_worker::image_ops::{self, BgrImage};

/// 与冻结 TextSnap 一致的推理线程数（`_DEFAULT_ENGINE_CONFIG`）。
const NUM_THREADS: usize = 10;
/// tiling.py 默认参数（瓦片 1216、重叠 128）。
const TILE_SIZE: u32 = 1216;
const TILE_OVERLAP: u32 = 128;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn assets() -> PathBuf {
    repo_root().join(".tmp/ocr-assets")
}

fn dump_dir() -> PathBuf {
    repo_root().join(".tmp/det-align/dump")
}

fn diff_ops_dir() -> PathBuf {
    assets().join("diff-ops")
}

/// 覆盖 O-18/O-23：20×30 合法小选区先补黑边到 32×32，原像素不得被拉伸。
#[test]
fn tiny_selection_is_padded_before_detection_resize() {
    let (w, h) = (20_usize, 30_usize);
    let src: Vec<u8> = (0..w * h * 3)
        .map(|i| u8::try_from(i % 251 + 1).unwrap())
        .collect();
    let (got, rh, rw) = __test_resize_type0(&src, 20, 30);
    assert_eq!((rw, rh), (32, 32));
    for y in 0..32_usize {
        for x in 0..32_usize {
            let pixel = &got[(y * 32 + x) * 3..(y * 32 + x + 1) * 3];
            if x < w && y < h {
                assert_eq!(pixel, &src[(y * w + x) * 3..(y * w + x + 1) * 3]);
            } else {
                assert_eq!(pixel, &[0, 0, 0]);
            }
        }
    }
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

/// `tiling._axis_origins` 的等价实现（步长 1088，尾起点满足 +1216 ≥ length）。
fn axis_origins(length: u32) -> Vec<u32> {
    if length <= TILE_SIZE {
        return vec![0];
    }
    let step = TILE_SIZE - TILE_OVERLAP;
    let mut origins = vec![0_u32];
    let mut last = 0_u32;
    while last + TILE_SIZE < length {
        last += step;
        origins.push(last);
    }
    origins
}

/// `generate_tiles`（先 y 后 x，index 连续；尾块裁至边界）。返回
/// `(index, x, y, w, h)`。
fn generate_tiles(width: u32, height: u32) -> Vec<(usize, u32, u32, u32, u32)> {
    let mut tiles = Vec::new();
    for &y in &axis_origins(height) {
        for &x in &axis_origins(width) {
            let (tw, th) = (TILE_SIZE.min(width - x), TILE_SIZE.min(height - y));
            tiles.push((tiles.len(), x, y, tw, th));
        }
    }
    tiles
}

/// PNG RGB → BGR 行主序 HxWx3。
fn tile_bgr(img: &image::RgbImage, x: u32, y: u32, tw: u32, th: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(tw as usize * th as usize * 3);
    for row in y..y + th {
        for col in x..x + tw {
            let p = img.get_pixel(col, row);
            out.extend_from_slice(&[p.0[2], p.0[1], p.0[0]]);
        }
    }
    out
}

/// 单用例全瓦片对照：返回 (逐瓦片 (实际框数, 期望框数), 最大坐标差, 最大分数差)。
fn run_case(det: &DetPaddlex, case: &str) -> (Vec<(usize, usize)>, f64, f64) {
    let oracle = read_json_sanitized(&assets().join("oracle").join(format!("{case}.json")));
    let img = image::open(assets().join("oracle").join(format!("{case}.png")))
        .unwrap()
        .to_rgb8();
    let (w, h) = img.dimensions();
    assert_eq!(
        (w, h),
        (
            u32::try_from(oracle["width"].as_u64().unwrap()).unwrap(),
            u32::try_from(oracle["height"].as_u64().unwrap()).unwrap()
        )
    );

    let det_calls = oracle["det_calls"].as_array().unwrap();
    let tiles = generate_tiles(w, h);
    assert_eq!(tiles.len(), det_calls.len(), "瓦片数与 det_calls 不一致");

    let (mut max_coord, mut max_score) = (0.0_f64, 0.0_f64);
    let mut counts = Vec::new();
    for &(index, x, y, tw, th) in &tiles {
        let bgr = tile_bgr(&img, x, y, tw, th);
        let (boxes, scores) = det.detect_tile_detached(&bgr, tw, th).unwrap();
        let expect_polys = det_calls[index]["polys"].as_array().unwrap();
        let expect_scores = det_calls[index]["scores"].as_array().unwrap();
        counts.push((boxes.len(), expect_polys.len()));
        assert_eq!(
            boxes.len(),
            expect_polys.len(),
            "{case} 瓦片 {index} 框数不符（期望 {}，实际 {}）",
            expect_polys.len(),
            boxes.len()
        );
        assert_eq!(
            scores.len(),
            expect_scores.len(),
            "{case} 瓦片 {index} 分数个数不符"
        );
        let (mut tile_coord, mut tile_score) = (0.0_f64, 0.0_f64);
        for (b, ep) in boxes.iter().zip(expect_polys) {
            let ep = ep.as_array().unwrap();
            for (pt, ept) in b.iter().zip(ep) {
                let ept = ept.as_array().unwrap();
                for (v, ev) in pt.iter().zip(ept) {
                    tile_coord = tile_coord.max((v - ev.as_f64().unwrap()).abs());
                }
            }
        }
        for (s, es) in scores.iter().zip(expect_scores) {
            tile_score = tile_score.max((s - es.as_f64().unwrap()).abs());
        }
        max_coord = max_coord.max(tile_coord);
        max_score = max_score.max(tile_score);
        println!(
            "{case} tile{index} ({tw}x{th}): boxes={} max_coord={tile_coord} max_score={tile_score:.6e}",
            boxes.len()
        );
    }
    (counts, max_coord, max_score)
}

fn load_det() -> DetPaddlex {
    DetPaddlex::from_path(
        &assets()
            .join("oracle-models")
            .join("PP-OCRv6_small_det")
            .join("inference.onnx"),
        NUM_THREADS,
    )
    .unwrap()
}

/// 验收：两用例全部瓦片候选四边形（同顺序、精确整数坐标）与检测分。
#[test]
#[ignore = "需本地 oracle 资产；由验收流程以 --ignored 显式运行"]
#[allow(clippy::float_cmp)]
fn oracle_det_two_cases_bitwise() {
    let det = load_det();
    for case in ["normal-1920x1080", "4k-3840x2160"] {
        let (counts, max_coord, max_score) = run_case(&det, case);
        println!(
            "== {case}: counts={counts:?} max_coord_diff={max_coord} max_score_diff={max_score:.6e}"
        );
        assert_eq!(max_coord, 0.0, "{case} 最大坐标差 {max_coord}");
        assert!(max_score <= 1e-6, "{case} 最大分数差 {max_score} > 1e-6");
    }
}

/// 分级对照 1：resize 输出与 paddlex 探针 dump 逐字节一致。
#[test]
#[ignore = "需本地分级 dump；由验收流程以 --ignored 显式运行"]
fn stage_resize_matches_dump() {
    for case in ["normal-1920x1080", "4k-3840x2160"] {
        let img = image::open(assets().join("oracle").join(format!("{case}.png")))
            .unwrap()
            .to_rgb8();
        let (w, h) = img.dimensions();
        for &(index, x, y, tw, th) in &generate_tiles(w, h) {
            let tag = format!("tile{index}_{tw}x{th}");
            let bin = dump_dir().join(case).join(format!("{tag}_resized.bin"));
            let expect = std::fs::read(&bin)
                .unwrap_or_else(|e| panic!("缺失 resize dump {}: {e}", bin.display()));
            let bgr = tile_bgr(&img, x, y, tw, th);
            let (resized, _rh, _rw) =
                snap_ocr_worker::det_paddlex::__test_resize_type0(&bgr, tw, th);
            assert_eq!(
                expect.len(),
                resized.len(),
                "{case}/{tag} resize dump 字节数"
            );
            assert!(
                expect.iter().zip(&resized).all(|(a, b)| a == b),
                "{case}/{tag} resize 与探针 dump 不一致"
            );
        }
    }
}

/// 分级对照 2：概率图与探针 dump 的最大绝对差（期望 0：同 dll、同线程数、
/// 同优化级别）。
#[test]
#[ignore = "需本地分级 dump；由验收流程以 --ignored 显式运行"]
fn stage_probmap_matches_dump() {
    let det = load_det();
    for case in ["normal-1920x1080", "4k-3840x2160"] {
        let img = image::open(assets().join("oracle").join(format!("{case}.png")))
            .unwrap()
            .to_rgb8();
        let (w, h) = img.dimensions();
        for &(index, x, y, tw, th) in &generate_tiles(w, h) {
            let tag = format!("tile{index}_{tw}x{th}");
            let bin = dump_dir().join(case).join(format!("{tag}_pred.bin"));
            let expect_bytes = std::fs::read(&bin)
                .unwrap_or_else(|e| panic!("缺失 probmap dump {}: {e}", bin.display()));
            let expect = read_f32_bin(&expect_bytes);
            let bgr = tile_bgr(&img, x, y, tw, th);
            let (_resized, _rh, _rw, pred) = det.__test_probmap(&bgr, tw, th);
            assert_eq!(pred.len(), expect.len());
            let max_diff = pred
                .iter()
                .zip(&expect)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0_f32, f32::max);
            println!("{case}/{tag}: probmap max_abs_diff = {max_diff:.9}");
            assert!(max_diff <= 1e-6, "{case}/{tag} 概率图偏差 {max_diff}");
        }
    }
}

/// 分级对照 3：Suzuki 轮廓（发现序的**逆序**，cv2 4.10 轮廓树 LIFO 输出 +
/// CHAIN_APPROX_SIMPLE 点列）与探针 manifest 的 contours_json 完全一致。
#[test]
#[ignore = "需本地分级 dump；由验收流程以 --ignored 显式运行"]
fn stage_contours_match_manifest() {
    let manifest_path = dump_dir().join("manifest.json");
    let manifest = read_json_sanitized(&manifest_path);
    for case in ["normal-1920x1080", "4k-3840x2160"] {
        let img = image::open(assets().join("oracle").join(format!("{case}.png")))
            .unwrap()
            .to_rgb8();
        let (w, h) = img.dimensions();
        let tiles_rec = &manifest["cases"][case]["tiles"];
        for &(index, _x, _y, tw, th) in &generate_tiles(w, h) {
            let rec = &tiles_rec[index.to_string()];
            let contours_json = rec["contours_json"]
                .as_array()
                .unwrap_or_else(|| panic!("{case} tile{index} 缺失 contours_json"));
            let tag = format!("tile{index}_{tw}x{th}");
            let bin = dump_dir().join(case).join(format!("{tag}_pred.bin"));
            let expect_bytes = std::fs::read(&bin)
                .unwrap_or_else(|e| panic!("缺失 probmap dump {}: {e}", bin.display()));
            let pred = read_f32_bin(&expect_bytes);
            let shape = rec["pred_shape"].as_array().unwrap();
            let map_h = usize::try_from(shape[2].as_i64().unwrap()).unwrap();
            let map_w = usize::try_from(shape[3].as_i64().unwrap()).unwrap();
            assert_eq!(
                pred.len(),
                map_h * map_w,
                "{case}/{tag} probmap dump 大小不符"
            );
            let mask: Vec<u8> = pred
                .iter()
                .map(|&p| if p > __TEST_BINARIZE_THRESH { 255 } else { 0 })
                .collect();
            let contours = snap_ocr_worker::det_paddlex::__test_find_contours(&mask, map_w, map_h);
            assert_eq!(
                contours.len(),
                contours_json.len(),
                "{case}/{tag} 轮廓数不一致"
            );
            for (ci, (mine, expect_c)) in contours.iter().zip(contours_json).enumerate() {
                let expect_pts = expect_c.as_array().unwrap();
                assert_eq!(
                    mine.len(),
                    expect_pts.len(),
                    "{case}/{tag} 轮廓 {ci} 点数不一致"
                );
                for (pi, &(mx, my)) in mine.iter().enumerate() {
                    let ep = expect_pts[pi].as_array().unwrap();
                    assert_eq!(
                        (mx, my),
                        (
                            i32::try_from(ep[0].as_i64().unwrap()).unwrap(),
                            i32::try_from(ep[1].as_i64().unwrap()).unwrap()
                        ),
                        "{case}/{tag} 轮廓 {ci} 点 {pi} 不一致"
                    );
                }
            }
            println!("{case}/{tag}: contours = {} 全一致", contours.len());
        }
    }
}

/// 分级对照 4：逐轮廓的 `get_mini_boxes`（四角/sside）与 `unclip`
/// （area/length/distance/点数）对探针 manifest details 的数值一致性
/// （容差 1e-4：f32 链路的 json 往返）。
#[test]
#[ignore = "需本地分级 dump；由验收流程以 --ignored 显式运行"]
fn stage_boxes_matches_manifest() {
    use snap_ocr_worker::det_paddlex::{__test_mini_boxes, __test_unclip};
    let manifest = read_json_sanitized(&dump_dir().join("manifest.json"));
    for case in ["normal-1920x1080", "4k-3840x2160"] {
        let img = image::open(assets().join("oracle").join(format!("{case}.png")))
            .unwrap()
            .to_rgb8();
        let (w, h) = img.dimensions();
        let tiles_rec = &manifest["cases"][case]["tiles"];
        for &(index, _x, _y, tw, th) in &generate_tiles(w, h) {
            let rec = &tiles_rec[index.to_string()];
            let details = rec["details"]
                .as_array()
                .unwrap_or_else(|| panic!("{case} tile{index} 缺失 details"));
            let tag = format!("tile{index}_{tw}x{th}");
            let pred_path = dump_dir().join(case).join(format!("{tag}_pred.bin"));
            let pred_bytes = std::fs::read(&pred_path)
                .unwrap_or_else(|e| panic!("缺失 probmap dump {}: {e}", pred_path.display()));
            let pred = read_f32_bin(&pred_bytes);
            let shape = rec["pred_shape"].as_array().unwrap();
            let map_h = usize::try_from(shape[2].as_i64().unwrap()).unwrap();
            let map_w = usize::try_from(shape[3].as_i64().unwrap()).unwrap();
            assert_eq!(
                pred.len(),
                map_h * map_w,
                "{case}/{tag} probmap dump 大小不符"
            );
            let mask: Vec<u8> = pred
                .iter()
                .map(|&p| if p > __TEST_BINARIZE_THRESH { 255 } else { 0 })
                .collect();
            let contours = snap_ocr_worker::det_paddlex::__test_find_contours(&mask, map_w, map_h);
            assert_eq!(contours.len(), details.len(), "{case}/{tag} details 数不符");
            for (ci, contour) in contours.iter().enumerate() {
                let d = &details[ci];
                let (mine_pts, mine_sside) = __test_mini_boxes(contour);
                let exp_sside = d["sside"].as_f64().unwrap_or(f64::NAN);
                if exp_sside.is_finite() {
                    assert!(
                        (f64::from(mine_sside) - exp_sside).abs() <= 1e-4,
                        "{case}/{tag} c{ci} sside {mine_sside} vs {exp_sside}"
                    );
                }
                if let Some(exp_pts) = d["points"].as_array() {
                    assert_eq!(mine_pts.len(), exp_pts.len(), "{case}/{tag} c{ci} 角点数");
                    for (mp, ep) in mine_pts.iter().zip(exp_pts) {
                        let ep = ep.as_array().unwrap();
                        assert!(
                            (f64::from(mp.0) - ep[0].as_f64().unwrap()).abs() <= 1e-4
                                && (f64::from(mp.1) - ep[1].as_f64().unwrap()).abs() <= 1e-4,
                            "{case}/{tag} c{ci} 角点 ({}, {}) vs ({}, {})",
                            mp.0,
                            mp.1,
                            ep[0],
                            ep[1]
                        );
                    }
                    let (area, length, distance, unclipped) = __test_unclip(&mine_pts);
                    assert!(
                        (area - d["area"].as_f64().unwrap_or(f64::NAN)).abs() <= 1e-3,
                        "{case}/{tag} c{ci} area {area} vs {} points {mine_pts:?} vs {}",
                        d["area"],
                        d["points"]
                    );
                    assert!(
                        (length - d["length"].as_f64().unwrap_or(f64::NAN)).abs() <= 1e-3,
                        "{case}/{tag} c{ci} length {length}"
                    );
                    assert!(
                        (distance - d["distance"].as_f64().unwrap_or(f64::NAN)).abs() <= 1e-4,
                        "{case}/{tag} c{ci} distance {distance}"
                    );
                    if let Some(exp_npts) = d["unclip_npts"].as_i64() {
                        assert_eq!(
                            i64::try_from(unclipped.len()).unwrap(),
                            exp_npts,
                            "{case}/{tag} c{ci} unclip 点数"
                        );
                    }
                }
            }
            println!(
                "{case}/{tag}: mini-box/unclip 全一致（{} 轮廓）",
                contours.len()
            );
        }
    }
}
/// 广谱差分：`.tmp/ocr-assets/diff-ops/`（`diff_ops.py` 生成）逐位比对
/// resize(LINEAR/CUBIC/LANCZOS4)、warpPerspective(CUBIC+REPLICATE)、rot90。
#[test]
#[ignore = "需本地差分资产（diff_ops.py 生成）；由验收流程以 --ignored 显式运行"]
fn diff_ops_bitwise() {
    let manifest_path = diff_ops_dir().join("cases.json");
    let manifest = read_json_sanitized(&manifest_path);
    assert!(
        !manifest.as_object().unwrap().is_empty(),
        "差分 manifest 为空"
    );
    let mut per_op = std::collections::BTreeMap::<&str, (usize, usize)>::default();
    for (op, cases) in manifest.as_object().unwrap() {
        let cases = cases.as_array().unwrap();
        assert!(!cases.is_empty(), "{op} 差分用例为空");
        let mut ok = 0_usize;
        for case in cases {
            let input = BgrImage::from_vec(
                usize::try_from(case["w"].as_u64().unwrap()).unwrap(),
                usize::try_from(case["h"].as_u64().unwrap()).unwrap(),
                std::fs::read(diff_ops_dir().join(case["input"].as_str().unwrap())).unwrap(),
            )
            .unwrap();
            let expect =
                std::fs::read(diff_ops_dir().join(case["output"].as_str().unwrap())).unwrap();
            let got: Vec<u8> = match op.as_str() {
                "resize_linear" => {
                    let dw = usize::try_from(case["dw"].as_u64().unwrap()).unwrap();
                    let dh = usize::try_from(case["dh"].as_u64().unwrap()).unwrap();
                    image_ops::resize_inter_linear(&input, dw, dh)
                        .data()
                        .to_vec()
                }
                "resize_cubic" => {
                    let dw = usize::try_from(case["dw"].as_u64().unwrap()).unwrap();
                    let dh = usize::try_from(case["dh"].as_u64().unwrap()).unwrap();
                    image_ops::resize_inter_cubic(&input, dw, dh)
                        .data()
                        .to_vec()
                }
                "resize_lanczos4" => {
                    let dw = usize::try_from(case["dw"].as_u64().unwrap()).unwrap();
                    let dh = usize::try_from(case["dh"].as_u64().unwrap()).unwrap();
                    image_ops::resize_inter_lanczos4(&input, dw, dh)
                        .data()
                        .to_vec()
                }
                "warp_perspective" => {
                    let quad: Vec<(f64, f64)> = case["quad"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| {
                            let p = p.as_array().unwrap();
                            (p[0].as_f64().unwrap(), p[1].as_f64().unwrap())
                        })
                        .collect();
                    let q = [quad[0], quad[1], quad[2], quad[3]];
                    image_ops::warp_perspective_cubic_replicate(&input, &q)
                        .unwrap()
                        .data()
                        .to_vec()
                }
                "rot90" => {
                    let k = u32::try_from(case["k"].as_u64().unwrap()).unwrap();
                    image_ops::rot90(&input, k).unwrap().data().to_vec()
                }
                other => panic!("未知算子 {other}"),
            };
            assert_eq!(got.len(), expect.len(), "{op} 输出长度不一致");
            let mismatches = got.iter().zip(&expect).filter(|(a, b)| a != b).count();
            assert_eq!(
                mismatches,
                0,
                "{op} 用例 {}（{}x{}）失配 {mismatches}/{} 字节",
                case["input"].as_str().unwrap(),
                case["w"],
                case["h"],
                expect.len()
            );
            ok += 1;
        }
        per_op.insert(op.as_str(), (ok, cases.len()));
        println!("{op}: {ok}/{} 组逐位一致", cases.len());
    }
    for (op, (ok, total)) in &per_op {
        assert_eq!(ok, total, "{op} 存在未通过用例");
    }
    println!("差分汇总: {per_op:?}");
}

fn read_f32_bin(bytes: &[u8]) -> Vec<f32> {
    assert_eq!(bytes.len() % 4, 0, "f32 dump 字节数必须为 4 的倍数");
    let mut v = vec![0.0_f32; bytes.len() / 4];
    for (i, slot) in v.iter_mut().enumerate() {
        let b: [u8; 4] = bytes[i * 4..i * 4 + 4].try_into().unwrap();
        *slot = f32::from_le_bytes(b);
    }
    v
}
