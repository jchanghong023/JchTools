//! paddlex 3.7.2 文本检测（DBNet）前后处理 —— 与 oracle 逐位对齐（O-24，附录 B）。
//!
//! 权威实现（只读逐行对照）：
//! `.tmp/ocr-assets/venv/Lib/site-packages/paddlex/inference/models/text_detection/`
//! `processors.py` 的 `DetResizeForTest.resize_image_type0` / `NormalizeImage` /
//! `DBPostProcess.boxes_from_bitmap`。调用方为冻结 TextSnap `ocr.py:628-692`
//! （逐瓦片、batch=1、limit_type=max / limit_side_len=1216 / thresh=0.3 /
//! box_thresh=0.5 / unclip_ratio=1.5 / max_candidates=3000）。
//!
//! 为达到逐位一致，本模块按 OpenCV 4.10 源码移植了以下数值语义：
//! - `cv2.resize`（8UC3、INTER_LINEAR）：11 位定点权重（`f32` 计算 + 银行家舍入
//!   量化）、`i64` 横向点积、`((b0*(S0>>4))>>16 + (b1*(S1>>4))>>16 + 2) >> 2`
//!   两级截断移位（resize.cpp:1907-1933 的 `VResizeLinear<uchar,int,short>`
//!   特化；SIMD 路径与标量逐位一致，已在移植时逐行核对）；
//! - `cv2.findContours`（RETR_LIST + CHAIN_APPROX_SIMPLE）：1 像素零边框 +
//!   Suzuki 边界跟踪（contours_new.cpp `icvFetchContourEx`：轮廓树 `addChild`
//!   头插 + 迭代器 LIFO 出栈 → 输出为光栅发现序的**逆序**；
//!   CHAIN_APPROX_SIMPLE 压缩、原地 nbd 标记、1px 零边框与 -1,-1 去边框）；
//! - `cv2.minAreaRect` / `cv2.boxPoints`：Sklansky 凸包（convhull.cpp，
//!   凸性积在 `i64`）+ 旋转卡壳（rotcalipers.cpp，`f32` 域）+
//!   `RotatedRect::points`（types.cpp）；
//! - `cv2.fillPoly` + `cv2.mean`：边表扫描线栅格化（drawing.cpp
//!   `CollectPolyEdges`/`FillEdgeCollection`，16.16 定点、CmpEdges 全序、
//!   行末冒泡维护活动表）+ 8 连通 Bresenham 边界线（`LineIterator` 语义），
//!   均值按 `f64` 光栅序累加除以计数；
//! - `pyclipper`（Clipper 6.4.2）unclip：与 pyclipper 同源 C++（经 geo-clipper
//!   / clipper-sys FFI），坐标按 C 截断入 `i64`、delta 为 `f64`、JT_ROUND
//!   （ArcTolerance 0.25，pyclipper 默认）+ ET_CLOSEDPOLYGON；探针实测
//!   pyclipper 对坐标不做内部缩放（浮点输入 == 截断整数输入）、delta 不截断
//!   （2.5 与 2.5000001 输出不同）；`ClipperOffset::Execute` 自带
//!   `FixOrientations`，geo-clipper 前置的 ctDifference 定向预处理在单简单
//!   多边形上对结果点集无影响（下游凸包对点序不敏感）。
//!
//! 已知边界（附录 B 备注）：`cv2.resize` 的 INTER_LINEAR→INTER_AREA 特例（两轴
//! 恰为整数 2 倍下采样）未移植——本瓦片体系（边长 ≤1216）只会触发上采样或恒等。

use std::error::Error as StdError;
use std::fmt;
use std::sync::Mutex;

use geo_clipper::{ClipperInt, EndType, JoinType};
use geo_types::{Coord, LineString, Polygon};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;

/// 检测参数（O-24）：与冻结 TextSnap `ocr.py` `_detect` 一致。
const LIMIT_SIDE_LEN: u32 = 1216;
const MAX_SIDE_LIMIT: u32 = 4000;
const BINARIZE_THRESH: f32 = 0.3;
const BOX_THRESH: f64 = 0.5;
const UNCLIP_RATIO: f64 = 1.5;
const MAX_CANDIDATES: usize = 3000;
/// DBPostProcess 自带的最小边阈值：首轮 `min_size`、次轮 `min_size + 2`。
const MIN_SIZE: f32 = 3.0;

/// NormalizeImage 常量（det yml：mean/std 为 ImageNet 值、scale=1/255，BGR 序）。
const NORM_MEAN: [f64; 3] = [0.485, 0.456, 0.406];
const NORM_STD: [f64; 3] = [0.229, 0.224, 0.225];
const NORM_SCALE: f64 = 1.0 / 255.0;

/// 检测错误（附录 B：`detect_tile_detached` 的失败面）。
#[derive(Debug)]
pub enum DetError {
    /// ONNX Runtime 会话构建或执行失败（含动态库加载）。`ort::Error<Stage>`
    /// 非 Send/Sync 且阶段标记各异，统一落为净化消息字符串（O-29/O-30：诊断
    /// 不得含用户路径或内容）。
    Ort(String),
    /// 模型文件读取失败。
    Io(std::io::Error),
    /// 输入或输出形状非法（非 HxWx3 瓦片、概率图维度不符等）。
    InvalidShape(String),
}

impl fmt::Display for DetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DetError::Ort(m) => write!(f, "onnxruntime 检测会话错误: {m}"),
            DetError::Io(e) => write!(f, "检测模型读取失败: {e}"),
            DetError::InvalidShape(m) => write!(f, "检测输入/输出形状非法: {m}"),
        }
    }
}

impl StdError for DetError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            DetError::Ort(_) | DetError::InvalidShape(_) => None,
            DetError::Io(e) => Some(e),
        }
    }
}

impl<Stage> From<ort::Error<Stage>> for DetError {
    fn from(e: ort::Error<Stage>) -> Self {
        DetError::Ort(e.to_string())
    }
}

impl From<std::io::Error> for DetError {
    fn from(e: std::io::Error) -> Self {
        DetError::Io(e)
    }
}

/// paddlex 逐位对齐的 det 推理器：持有 ort 会话（经 `ORT_DYLIB_PATH` 动态加载
/// onnxruntime，与 vendored `xberg-paddle-ocr` 同源约束，O-08）。
///
/// 会话选项复刻冻结 TextSnap `_DEFAULT_ENGINE_CONFIG` + paddlex onnxruntime
/// 引擎默认值：`intra_op_num_threads` 由调用方给定（TextSnap 为 10）、
/// `inter_op_num_threads=1`、图优化 All。
pub struct DetPaddlex {
    session: Mutex<Session>,
}

/// ort 2.0-rc.13 `load-dynamic` 的动态库首次加载非线程安全：并行测试同时建会话
/// 会在原生层竞态（mutex_std 中毒）。进程级门串行化**所有**会话构建（模型加载
/// 低频，代价可忽略；O-24 验收以 `--test-threads=2` 并行运行）。
pub(crate) static SESSION_BUILD_GATE: Mutex<()> = Mutex::new(());

/// 检测框及其逐框概率分数。
type DetectionBoxes = (Vec<[[f64; 2]; 4]>, Vec<f64>);

impl DetPaddlex {
    /// 从 ONNX 字节构建（线程数语义同 `_DEFAULT_ENGINE_CONFIG` 的
    /// `intra_op_num_threads`）。
    ///
    /// # Errors
    /// 会话构建失败（模型损坏、动态库缺失等）时返回 [`DetError::Ort`]。
    pub fn from_memory(model_bytes: &[u8], intra_threads: usize) -> Result<Self, DetError> {
        // ort 2.0.0-rc.13 的 builder 配置方法返回 `Error<SessionBuilder>`，
        // 与 vendored `ort_backend` 一致地净化为消息字符串；构建全程持进程门。
        let _gate = SESSION_BUILD_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut builder = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(|e| DetError::Ort(e.to_string()))?
            .with_intra_threads(intra_threads)
            .map_err(|e| DetError::Ort(e.to_string()))?
            .with_inter_threads(1)
            .map_err(|e| DetError::Ort(e.to_string()))?;
        Ok(Self {
            session: Mutex::new(builder.commit_from_memory(model_bytes)?),
        })
    }

    /// 从模型路径构建。
    ///
    /// # Errors
    /// 读取失败或会话构建失败。
    pub fn from_path(path: &std::path::Path, intra_threads: usize) -> Result<Self, DetError> {
        let bytes = std::fs::read(path)?;
        Self::from_memory(&bytes, intra_threads)
    }

    /// 对单张瓦片执行 paddlex 语义的检测（O-24，附录 B 契约）。
    ///
    /// 输入 `tile_bgr_hwc` 为 **BGR 顺序**的行主序 u8 像素（HxWx3；PNG 的 RGB
    /// 需调用方先行换序）。输出 `(boxes, scores)`：瓦片**局部**坐标四边形
    /// `[[x,y];4]`（TL/TR/BR/BL，已按 `[0, dest_w/h]` 钳制并完成回缩放，值为
    /// 整数坐标的 f64）与检测分（bbox 内概率均值，`cv2.mean` 的 f64 语义），
    /// 顺序同 `findContours` 输出序（cv2 4.10 = 光栅发现序的逆序）。
    ///
    /// # Errors
    /// 形状非法或推理失败。
    pub fn detect_tile_detached(
        &self,
        tile_bgr_hwc: &[u8],
        tile_w: u32,
        tile_h: u32,
    ) -> Result<DetectionBoxes, DetError> {
        if tile_w == 0 || tile_h == 0 {
            return Err(DetError::InvalidShape("瓦片尺寸为零".into()));
        }
        let expected = tile_w as usize * tile_h as usize * 3;
        if tile_bgr_hwc.len() != expected {
            return Err(DetError::InvalidShape(format!(
                "瓦片像素长度 {} 与 {}x{}x3 不符",
                tile_bgr_hwc.len(),
                tile_w,
                tile_h
            )));
        }
        let (resized, resize_h, resize_w) = resize_type0(tile_bgr_hwc, tile_w, tile_h);
        let tensor = normalize_chw(&resized, resize_w, resize_h);
        let pred = self.run_probmap(&tensor, resize_w, resize_h)?;
        Ok(boxes_from_bitmap(&pred, resize_w, resize_h, tile_w, tile_h))
    }

    /// 跑批并解包 `[1,1,H,W]` 概率图（与 paddlex `preds[0][0]` 等价）。
    /// `run` 需 `&mut`（ort rc.13），经 Mutex 串行化后对外保持 `&self`。
    fn run_probmap(&self, tensor: &[f32], w: u32, h: u32) -> Result<Vec<f32>, DetError> {
        let shape = vec![1_i64, 3, i64::from(h), i64::from(w)];
        let value = ort::value::Tensor::from_array((shape, tensor.to_vec()))?;
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outputs = session.run(ort::inputs![value])?;
        let output = outputs
            .values()
            .next()
            .ok_or_else(|| DetError::InvalidShape("模型无输出张量".into()))?;
        let (out_shape, out_data) = output.try_extract_tensor::<f32>()?;
        if out_shape.len() < 2 {
            return Err(DetError::InvalidShape(format!(
                "概率图维度异常: {out_shape:?}"
            )));
        }
        let raw_h = out_shape[out_shape.len() - 2];
        let raw_w = out_shape[out_shape.len() - 1];
        let map_h = usize::try_from(raw_h).ok();
        let map_w = usize::try_from(raw_w).ok();
        if map_h != Some(h as usize)
            || map_w != Some(w as usize)
            || out_data.len() < h as usize * w as usize
        {
            return Err(DetError::InvalidShape(format!(
                "概率图 {raw_w}x{raw_h} 与输入 {w}x{h} 不符"
            )));
        }
        Ok(out_data.to_vec())
    }
}

// ══════════════════ 前处理：DetResizeForTest + NormalizeImage ══════════════════

/// `resize_image_type0`（limit_type=max, 1216, max_side_limit=4000）：
/// 返回（resize 后 BGR 像素, resize_h, resize_w）。恒等时原样复制返回。
// Python int() truncates positive scaled dimensions; round() below uses ties-to-even.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn resize_type0(bgr: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    // PaddleX DetResizeForTest 先对 h + w < 64 的小图补零，再进入 type 0。
    // 补边后的 32×32 等尺寸走下方恒等分支，不能把原图内容拉伸到整张输入。
    if h + w < 64 {
        let padded_w = w.max(32);
        let padded_h = h.max(32);
        let (row_bytes, padded_row_bytes) = (w as usize * 3, padded_w as usize * 3);
        let mut padded = vec![0; padded_row_bytes * padded_h as usize];
        for y in 0..h as usize {
            let from = y * row_bytes;
            let to = y * padded_row_bytes;
            padded[to..to + row_bytes].copy_from_slice(&bgr[from..from + row_bytes]);
        }
        return resize_type0(&padded, padded_w, padded_h);
    }
    let ratio = if h.max(w) > LIMIT_SIDE_LEN {
        if h > w {
            f64::from(LIMIT_SIDE_LEN) / f64::from(h)
        } else {
            f64::from(LIMIT_SIDE_LEN) / f64::from(w)
        }
    } else {
        1.0
    };
    // Python `int()`：正数向零截断。
    let mut resize_h = (f64::from(h) * ratio) as u32;
    let mut resize_w = (f64::from(w) * ratio) as u32;
    if resize_h.max(resize_w) > MAX_SIDE_LIMIT {
        let ratio2 = f64::from(MAX_SIDE_LIMIT) / f64::from(resize_h.max(resize_w));
        resize_h = (f64::from(resize_h) * ratio2) as u32;
        resize_w = (f64::from(resize_w) * ratio2) as u32;
    }
    // Python `round()`：银行家舍入。
    resize_h = ((f64::from(resize_h) / 32.0).round_ties_even() as i64 * 32).max(32) as u32;
    resize_w = ((f64::from(resize_w) / 32.0).round_ties_even() as i64 * 32).max(32) as u32;
    if resize_h == h && resize_w == w {
        return (bgr.to_vec(), h, w);
    }
    (
        cv_resize_linear_bgr(bgr, w, h, resize_w, resize_h),
        resize_h,
        resize_w,
    )
}

/// `cv2.resize`（8UC3、INTER_LINEAR）的逐位移植（O-24）。
///
/// 依据 OpenCV 4.10 `resize.cpp`：权重在 `f32` 域计算后 `cvRound`（银行家
/// 舍入）量化为 11 位定点；横向 `i32` 点积（值域 ≤ 255·2048）；纵向走 8U
/// 特化 `dst = ((b0*(S0>>4))>>16 + (b1*(S1>>4))>>16 + 2) >> 2`（两级截断
/// 移位）。边界：`sx<0` 或 `sx≥sw-1` 时 `fx=0` 并钳制（越界列改用单抽头
/// `S[sx]*2048`）；纵向 `sy` 不钳制（可为 -1），行号经半开 `clip(sy,0,h)`
/// 取行（即 `min(max(sy,0), h-1)`）。
#[allow(clippy::too_many_lines)]
// OpenCV's float coefficients, signed sentinel row and saturated 8-bit output require
// these exact intermediate casts; replacing them changes quantization at boundaries.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn cv_resize_linear_bgr(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    /// 定点权重位数（`INTER_RESIZE_COEF_BITS = 11`）。
    const COEF_SCALE: f32 = 2048.0;
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let scale_x = sw as f64 / dw as f64;
    let scale_y = sh as f64 / dh as f64;

    // x 轴抽头与权重（hal::resize 系数循环的逐行移植）。
    let mut xofs = vec![0_usize; dw];
    let mut alpha = vec![(0_i32, 0_i32); dw];
    for dx in 0..dw {
        let fx0 = ((dx as f64 + 0.5) * scale_x - 0.5) as f32;
        let mut sx = fx0.floor() as i64;
        let mut fx = fx0 - sx as f32;
        if sx < 0 {
            fx = 0.0;
            sx = 0;
        }
        if sx >= sw as i64 - 1 {
            fx = 0.0;
            sx = sw as i64 - 1;
        }
        xofs[dx] = sx as usize;
        alpha[dx] = (
            cv_round_to_i32((1.0_f32 - fx) * COEF_SCALE),
            cv_round_to_i32(fx * COEF_SCALE),
        );
    }

    let mut out = vec![0_u8; dw * dh * 3];
    for dy in 0..dh {
        let fy0 = ((dy as f64 + 0.5) * scale_y - 0.5) as f32;
        let sy = fy0.floor() as i64;
        let fy = fy0 - sy as f32;
        let b0 = i64::from(cv_round_to_i32((1.0_f32 - fy) * COEF_SCALE));
        let b1 = i64::from(cv_round_to_i32(fy * COEF_SCALE));
        let r0 = sy.clamp(0, sh as i64 - 1) as usize;
        let r1 = (sy + 1).clamp(0, sh as i64 - 1) as usize;
        let row0 = &src[r0 * sw * 3..(r0 + 1) * sw * 3];
        let row1 = &src[r1 * sw * 3..(r1 + 1) * sw * 3];
        let dst_row = &mut out[dy * dw * 3..(dy + 1) * dw * 3];
        for dx in 0..dw {
            let (a0, a1) = alpha[dx];
            let sx = xofs[dx] * 3;
            let single_tap = xofs[dx] + 1 >= sw;
            for c in 0..3 {
                let (s0, s1) = if single_tap {
                    (
                        i64::from(row0[sx + c]) * 2048,
                        i64::from(row1[sx + c]) * 2048,
                    )
                } else {
                    (
                        i64::from(row0[sx + c]) * i64::from(a0)
                            + i64::from(row0[sx + 3 + c]) * i64::from(a1),
                        i64::from(row1[sx + c]) * i64::from(a0)
                            + i64::from(row1[sx + 3 + c]) * i64::from(a1),
                    )
                };
                // VResizeLinear<uchar,int,short> 特化：两级截断 + 舍入移位。
                let t = ((b0 * (s0 >> 4)) >> 16) + ((b1 * (s1 >> 4)) >> 16);
                dst_row[dx * 3 + c] = ((t + 2) >> 2) as u8;
            }
        }
    }
    out
}

/// `cvRound`（float→int，银行家舍入；`saturate_cast<short>` 语义等价，
/// 值域 [0,2048])。
// cvRound is followed by OpenCV saturate_cast; coefficients are bounded to [0, 2048].
#[allow(clippy::cast_possible_truncation)]
fn cv_round_to_i32(v: f32) -> i32 {
    v.round_ties_even() as i32
}

/// `NormalizeImage`：BGR 逐通道 `u8→f32`，`(px * alpha) + beta`（两步 `f32`
/// 运算，与 numpy 逐通道 in-place 乘加一致），输出 CHW 行主序张量
/// （channel 0 = B）。
// NumPy computes constants as f64 then stores them in f32 channel arrays.
#[allow(clippy::cast_possible_truncation)]
fn normalize_chw(bgr: &[u8], w: u32, h: u32) -> Vec<f32> {
    let (w, h) = (w as usize, h as usize);
    let mut alpha = [0.0_f32; 3];
    let mut beta = [0.0_f32; 3];
    for c in 0..3 {
        alpha[c] = (NORM_SCALE / NORM_STD[c]) as f32;
        beta[c] = (-NORM_MEAN[c] / NORM_STD[c]) as f32;
    }
    let mut out = vec![0.0_f32; w * h * 3];
    for c in 0..3 {
        let plane = &mut out[c * w * h..(c + 1) * w * h];
        for (i, v) in plane.iter_mut().enumerate() {
            *v = (f32::from(bgr[i * 3 + c]) * alpha[c]) + beta[c];
        }
    }
    out
}

// ══════════════════ 后处理：DBPostProcess.boxes_from_bitmap ══════════════════

/// `boxes_from_bitmap`（box_type=quad, score_mode=fast, use_dilation=false）。
/// `pred` 为 resize 尺度概率图；`(tile_w, tile_h)` 为瓦片原尺寸（回缩放与
/// 钳制的上界）。
// NumPy's f32 image arithmetic and Clipper's integer contour conversion are intentional.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn boxes_from_bitmap(
    pred: &[f32],
    map_w: u32,
    map_h: u32,
    tile_w: u32,
    tile_h: u32,
) -> (Vec<[[f64; 2]; 4]>, Vec<f64>) {
    let (map_w, map_h) = (map_w as usize, map_h as usize);
    // Python `pred > thresh`：f32 严格大于（0.3 两侧同为 f32）。
    let mask: Vec<u8> = pred
        .iter()
        .map(|&p| if p > BINARIZE_THRESH { 255 } else { 0 })
        .collect();
    let contours = find_contours_list_simple(&mask, map_w, map_h);

    // Python `dest_width / width`：f64；NEP 50 下 f32 数组乘 Python 标量在
    // f32 域进行（标量先转 f32）。
    let scale_w = (f64::from(tile_w) / map_w as f64) as f32;
    let scale_h = (f64::from(tile_h) / map_h as f64) as f32;

    let mut boxes = Vec::new();
    let mut scores = Vec::new();
    for contour in contours.iter().take(MAX_CANDIDATES) {
        let (points, sside) = get_mini_boxes(contour);
        if sside < MIN_SIZE {
            continue;
        }
        let score = box_score_fast(pred, map_w, map_h, &points);
        if BOX_THRESH > score {
            continue;
        }
        let unclipped = unclip(&points);
        if unclipped.is_empty() {
            // paddlex 此处经 `get_mini_boxes(空)` 触发 cv2 异常；有效输入
            // （sside≥3 且 score≥0.5）下 distance>0 必有解，此分支仅防御。
            continue;
        }
        let unclipped: Vec<(i32, i32)> = unclipped
            .iter()
            .map(|&(x, y)| (x as i32, y as i32))
            .collect();
        let (box2, sside2) = get_mini_boxes(&unclipped);
        if sside2 < MIN_SIZE + 2.0 {
            continue;
        }
        let mut quad: [[f64; 2]; 4] = [[0.0; 2]; 4];
        for (i, &(x, y)) in box2.iter().enumerate() {
            // round() 银行家舍入后 `max(0, min(round, dest))`（上界含 dest
            // 本身，与 paddlex 一致）。
            let rx = (x * scale_w).round_ties_even().clamp(0.0, tile_w as f32);
            let ry = (y * scale_h).round_ties_even().clamp(0.0, tile_h as f32);
            quad[i] = [f64::from(rx), f64::from(ry)];
        }
        boxes.push(quad);
        scores.push(score);
    }
    (boxes, scores)
}

/// `get_mini_boxes`：`minAreaRect` → `boxPoints` → 按 x 稳定排序 →
/// index_1..4 重排为 TL/TR/BR/BL；`sside = min(w,h)`（f32）。
fn get_mini_boxes(points: &[(i32, i32)]) -> (Vec<(f32, f32)>, f32) {
    let rect = min_area_rect(points);
    let corners = box_points(&rect);
    // Python `sorted(..., key=lambda p: p[0])`：稳定排序，仅按 x。
    let mut sorted: Vec<(f32, f32)> = corners.to_vec();
    sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let (index_1, index_4) = if sorted[1].1 > sorted[0].1 {
        (0, 1)
    } else {
        (1, 0)
    };
    let (index_2, index_3) = if sorted[3].1 > sorted[2].1 {
        (2, 3)
    } else {
        (3, 2)
    };
    (
        vec![
            sorted[index_1],
            sorted[index_2],
            sorted[index_3],
            sorted[index_4],
        ],
        rect.width.min(rect.height),
    )
}

/// `cv::RotatedRect` 的 f32 表示（角度为度）。
struct RotatedRectF32 {
    center: (f32, f32),
    width: f32,
    height: f32,
    angle_deg: f32,
}

/// `cv2.minAreaRect`（整数输入：凸包在 i32 域、卡壳在 f32 域）。
// OpenCV converts hull coordinates to f32, then f64 transcendental results back to f32.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::manual_midpoint
)]
fn min_area_rect(points: &[(i32, i32)]) -> RotatedRectF32 {
    let hull = convex_hull_ccw(points);
    let hull_f32: Vec<(f32, f32)> = hull
        .iter()
        .map(|&i| (points[i].0 as f32, points[i].1 as f32))
        .collect();
    let n = hull_f32.len();
    let mut rect = RotatedRectF32 {
        center: (0.0, 0.0),
        width: 0.0,
        height: 0.0,
        angle_deg: 0.0,
    };
    if n > 2 {
        let out = rotating_calipers_min_area_rect(&hull_f32);
        rect.center.0 = out[0].0 + (out[1].0 + out[2].0) * 0.5_f32;
        rect.center.1 = out[0].1 + (out[1].1 + out[2].1) * 0.5_f32;
        rect.width = (f64::from(out[1].0) * f64::from(out[1].0)
            + f64::from(out[1].1) * f64::from(out[1].1))
        .sqrt() as f32;
        rect.height = (f64::from(out[2].0) * f64::from(out[2].0)
            + f64::from(out[2].1) * f64::from(out[2].1))
        .sqrt() as f32;
        let radians = f64::from(out[1].1).atan2(f64::from(out[1].0)) as f32;
        rect.angle_deg = (f64::from(radians * 180.0_f32) / std::f64::consts::PI) as f32;
    } else if n == 2 {
        rect.center.0 = (hull_f32[0].0 + hull_f32[1].0) * 0.5_f32;
        rect.center.1 = (hull_f32[0].1 + hull_f32[1].1) * 0.5_f32;
        let dx = f64::from(hull_f32[1].0) - f64::from(hull_f32[0].0);
        let dy = f64::from(hull_f32[1].1) - f64::from(hull_f32[0].1);
        rect.width = (dx * dx + dy * dy).sqrt() as f32;
        rect.height = 0.0;
        let radians = dy.atan2(dx) as f32;
        rect.angle_deg = (f64::from(radians * 180.0_f32) / std::f64::consts::PI) as f32;
    } else if n == 1 {
        rect.center = hull_f32[0];
    }
    rect
}

/// `RotatedRect::points`（types.cpp:173-187，f32 运算链照抄）。
// RotatedRect::points rounds sine and cosine to f32 before the remaining operations.
#[allow(clippy::cast_possible_truncation)]
fn box_points(rect: &RotatedRectF32) -> [(f32, f32); 4] {
    let angle = f64::from(rect.angle_deg) * std::f64::consts::PI / 180.0;
    let b = (angle.cos() as f32) * 0.5_f32;
    let a = (angle.sin() as f32) * 0.5_f32;
    let (cx, cy) = rect.center;
    let (hw, hh) = (rect.width, rect.height);
    let p0x = cx - a * hh - b * hw;
    let p0y = cy + b * hh - a * hw;
    let p1x = cx + a * hh - b * hw;
    let p1y = cy - b * hh - a * hw;
    [
        (p0x, p0y),
        (p1x, p1y),
        (2.0 * cx - p0x, 2.0 * cy - p0y),
        (2.0 * cx - p1x, 2.0 * cy - p1y),
    ]
}

/// `cv::convexHull(clockwise=false)`（convhull.cpp:135-310 的 i32 分支）：
/// 点按 `(x, y, 原下标)` 全序排序（对应 `CHullCmpPoints` 的指针决胜），
/// 四段 Sklansky 拼接 + 共线镜像截断 + 循环移位规范化，返回**原始点列下标**
/// （升序语义无关紧要，关键是顶点循环序）。
#[allow(clippy::too_many_lines)]
// Compass-pair names (top-left/right, bottom-left/right) mirror Sklansky's four chains.
#[allow(
    clippy::similar_names,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn convex_hull_ccw(points: &[(i32, i32)]) -> Vec<usize> {
    let total = points.len();
    if total == 0 {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..total).collect();
    order.sort_by(|&a, &b| (points[a].0, points[a].1, a).cmp(&(points[b].0, points[b].1, b)));

    // 排序后数组上的 miny/maxy 位置（严格比较，首见优先）。
    let mut miny_ind = 0_usize;
    let mut maxy_ind = 0_usize;
    for i in 1..total {
        let y = points[order[i]].1;
        if points[order[miny_ind]].1 > y {
            miny_ind = i;
        }
        if points[order[maxy_ind]].1 < y {
            maxy_ind = i;
        }
    }

    let mut hullbuf: Vec<usize> = Vec::with_capacity(total);
    if points[order[0]] == points[order[total - 1]] {
        hullbuf.push(order[0]);
        return hullbuf;
    }

    // 上半：左链 = [0, maxy]，右链 = [total-1, maxy]；convexHull(clockwise=false)
    // 在拼接前交换两段（convhull.cpp:211-215）→ 输出逆时针。
    let mut tl_stack = vec![0_i64; total + 2];
    let tl_count = sklansky(&order, points, 0, maxy_ind as i64, &mut tl_stack, -1, 1);
    let mut tr_stack = vec![0_i64; total + 2];
    let tr_count = sklansky(
        &order,
        points,
        total as i64 - 1,
        maxy_ind as i64,
        &mut tr_stack,
        -1,
        -1,
    );
    // !clockwise → 交换（tl 承担右链、tr 承担左链）。
    let (tl_stack, tl_count, tr_stack, tr_count) = (tr_stack, tr_count, tl_stack, tl_count);
    for i in 0..tl_count.saturating_sub(1) {
        hullbuf.push(order[tl_stack[i] as usize]);
    }
    let mut i = tr_count as i64 - 1;
    while i > 0 {
        hullbuf.push(order[tr_stack[i as usize] as usize]);
        i -= 1;
    }
    let stop_idx: i64 = if tr_count > 2 {
        tr_stack[1]
    } else if tl_count > 2 {
        tl_stack[tl_count - 2]
    } else {
        -1
    };

    // 下半：bl = [0, miny]，br = [total-1, miny]；!clockwise → 不交换。
    let mut bl_stack = vec![0_i64; total + 2];
    let bl_count = sklansky(&order, points, 0, miny_ind as i64, &mut bl_stack, 1, -1);
    let mut br_stack = vec![0_i64; total + 2];
    let br_count = sklansky(
        &order,
        points,
        total as i64 - 1,
        miny_ind as i64,
        &mut br_stack,
        1,
        1,
    );
    let (mut bl_count, mut br_count) = (bl_count, br_count);

    if stop_idx >= 0 {
        let check_idx: i64 = if bl_count > 2 {
            bl_stack[1]
        } else if bl_count + br_count > 2 {
            br_stack[2 - bl_count]
        } else {
            -1
        };
        if check_idx == stop_idx
            || (check_idx >= 0
                && points[order[check_idx as usize]] == points[order[stop_idx as usize]])
        {
            // 全部共线：下半为上半镜像（两段各保留 ≤2 点）。
            bl_count = bl_count.min(2);
            br_count = br_count.min(2);
        }
    }
    for i in 0..bl_count.saturating_sub(1) {
        hullbuf.push(order[bl_stack[i] as usize]);
    }
    let mut i = br_count as i64 - 1;
    while i > 0 {
        hullbuf.push(order[br_stack[i as usize] as usize]);
        i -= 1;
    }
    maybe_cyclic_shift(&mut hullbuf);
    hullbuf
}

/// convhull.cpp:263-297 的循环移位规范化（条件与断点照抄；提前 break 时
/// 沿用循环内已累计的 min/max 状态，与 C++ 一致）。
// OpenCV's integer index normalization uses signed counters and its original comparisons.
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
fn maybe_cyclic_shift(hullbuf: &mut Vec<usize>) {
    let nout = hullbuf.len();
    if nout < 3 {
        return;
    }
    let mut min_idx = 0;
    let mut max_idx = 0;
    let mut lt = 0_i32;
    for i in 1..nout {
        let idx = hullbuf[i];
        lt += i32::from(hullbuf[i - 1] < idx);
        if lt > 1 && lt <= i as i32 - 2 {
            break;
        }
        if idx < hullbuf[min_idx] {
            min_idx = i;
        }
        if idx > hullbuf[max_idx] {
            max_idx = i;
        }
    }
    let mmdist = ((max_idx as i64) - (min_idx as i64)).abs();
    if (mmdist == 1 || mmdist == nout as i64 - 1) && (lt <= 1 || lt >= nout as i32 - 2) {
        let ascending = (max_idx + 1) % nout == min_idx;
        let i0 = if ascending { min_idx } else { max_idx };
        if i0 > 0 {
            let mut shifted = Vec::with_capacity(nout);
            let mut j = i0;
            for i in 0..nout {
                let curr_idx = hullbuf[j];
                let next_j = if j + 1 < nout { j + 1 } else { 0 };
                let next_idx = hullbuf[next_j];
                if i < nout - 1 && (ascending != (curr_idx < next_idx)) {
                    return;
                }
                shifted.push(curr_idx);
                j = next_j;
            }
            *hullbuf = shifted;
        }
    }
}

/// `Sklansky_`（convhull.cpp:48-118，`_Tp=i32`、`_DotTp=i64`）。`order` 为
/// 排序后下标；`stack` 存排序位置（含退出哨兵值，i64 承载）；返回
/// `stacksize - 1`。
#[allow(clippy::too_many_lines)]
// Signed sentinel indices and the branch layout follow OpenCV's Sklansky_ state machine.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::if_not_else
)]
fn sklansky(
    order: &[usize],
    points: &[(i32, i32)],
    start: i64,
    end: i64,
    stack: &mut [i64],
    nsign: i32,
    sign2: i32,
) -> usize {
    let at = |i: i64| points[order[i as usize]];
    let incr: i64 = if end > start { 1 } else { -1 };
    let mut pprev = start;
    let mut pcur = pprev + incr;
    let mut pnext = pcur + incr;
    let mut stacksize = 3_usize;

    if start == end || at(start) == at(end) {
        stack[0] = start;
        return 1;
    }
    stack[0] = pprev;
    stack[1] = pcur;
    stack[2] = pnext;
    let end_after = end + incr;

    while pnext != end_after {
        let cury = at(pcur).1;
        let nexty = at(pnext).1;
        let by = nexty - cury;
        // CV_SIGN 返回 -1/0/+1；共线边必须保留 0，不能折合为 +1。
        let sign_by = by.signum();
        if sign_by != nsign {
            let ax = at(pcur).0 - at(pprev).0;
            let bx = at(pnext).0 - at(pcur).0;
            let ay = cury - at(pprev).1;
            let convexity = i64::from(ay) * i64::from(bx) - i64::from(ax) * i64::from(by);
            let sign_cv = convexity.signum() as i32;
            if sign_cv == sign2 && (ax != 0 || ay != 0) {
                pprev = pcur;
                pcur = pnext;
                pnext += incr;
                stack[stacksize] = pnext;
                stacksize += 1;
            } else if pprev == start {
                pcur = pnext;
                stack[1] = pcur;
                pnext += incr;
                stack[2] = pnext;
            } else {
                stack[stacksize - 2] = pnext;
                pcur = pprev;
                pprev = stack[stacksize - 4];
                stacksize -= 1;
            }
        } else {
            pnext += incr;
            stack[stacksize - 1] = pnext;
        }
    }
    stacksize - 1
}

/// `rotatingCalipers` CALIPERS_MINAREARECT（rotcalipers.cpp:118-355，f32 域）。
/// 返回 `[corner, width_vec, height_vec]` 三点（minAreaRect 的 out[0..2]）。
#[allow(clippy::too_many_lines)]
// Caliper vectors round f64 intermediates to f32; signed stored indices follow OpenCV.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn rotating_calipers_min_area_rect(points: &[(f32, f32)]) -> [(f32, f32); 3] {
    let n = points.len();
    let mut inv_vect_length = vec![0.0_f32; n];
    let mut vect = vec![(0.0_f32, 0.0_f32); n];
    let mut left = 0;
    let mut bottom = 0;
    let mut right = 0;
    let mut top = 0;
    let mut left_x = points[0].0;
    let mut right_x = points[0].0;
    let mut top_y = points[0].1;
    let mut bottom_y = points[0].1;
    for i in 0..n {
        let pt0 = points[i];
        if pt0.0 < left_x {
            left_x = pt0.0;
            left = i;
        }
        if pt0.0 > right_x {
            right_x = pt0.0;
            right = i;
        }
        if pt0.1 > top_y {
            top_y = pt0.1;
            top = i;
        }
        if pt0.1 < bottom_y {
            bottom_y = pt0.1;
            bottom = i;
        }
        let pt = points[(i + 1) % n];
        // dx/dy 在 double 域计算；vect 存 f32，倒数长度用 double 值。
        let dx = f64::from(pt.0) - f64::from(pt0.0);
        let dy = f64::from(pt.1) - f64::from(pt0.1);
        vect[i] = (dx as f32, dy as f32);
        inv_vect_length[i] = (1.0 / (dx * dx + dy * dy).sqrt()) as f32;
    }

    // 凸包定向块在 OpenCV 原文中只喂给 base_a 的死初始化（四分支绝对赋值，
    // 首轮旋转即覆盖；C 不告警），移植中略去，数值行为不变。
    // OpenCV 移植说明：base_a/base_b 在每轮旋转的主元素分支内被全覆盖后读取；
    // 初始值仅为满足定义，NaN 哨兵防止意外依赖（rotcalipers.cpp 同构）。
    #[allow(clippy::items_after_statements)]
    let mut base_a;
    #[allow(clippy::items_after_statements)]
    let mut base_b;

    let mut seq = [bottom, right, top, left];
    let mut minarea = f32::MAX;
    // buffer 布局：[idx_left(int), base_a, width, base_b, height, idx_bottom(int), area]。
    let mut buf_idx_left = 0_i32;
    let mut buf_idx_bottom = 0_i32;
    let mut buf_base_a = 0.0_f32;
    let mut buf_base_b = 0.0_f32;
    let mut buf_width = 0.0_f32;
    let mut buf_height = 0.0_f32;

    let rotate90cw = |v: (f32, f32)| (v.1, -v.0);

    for _k in 0..n {
        let rot_vect = [
            vect[seq[0]],
            rotate90cw(vect[seq[1]]),
            (-vect[seq[2]].0, -vect[seq[2]].1),
            (-vect[seq[3]].1, vect[seq[3]].0),
        ];
        let is_right = |v1: (f32, f32), v2: (f32, f32)| {
            let t = rotate90cw(v1);
            t.0 * v2.0 + t.1 * v2.1 < 0.0
        };
        let mut main_element = 0;
        for i in 1..4 {
            if is_right(rot_vect[i], rot_vect[main_element]) {
                main_element = i;
            }
        }
        let pindex = seq[main_element];
        let lead_x = vect[pindex].0 * inv_vect_length[pindex];
        let lead_y = vect[pindex].1 * inv_vect_length[pindex];
        match main_element {
            0 => {
                base_a = lead_x;
                base_b = lead_y;
            }
            1 => {
                base_a = lead_y;
                base_b = -lead_x;
            }
            2 => {
                base_a = -lead_x;
                base_b = -lead_y;
            }
            _ => {
                base_a = -lead_y;
                base_b = lead_x;
            }
        }
        seq[main_element] += 1;
        if seq[main_element] == n {
            seq[main_element] = 0;
        }

        // 面积（f32 域，照抄）。
        let dx = points[seq[1]].0 - points[seq[3]].0;
        let dy = points[seq[1]].1 - points[seq[3]].1;
        let width = dx * base_a + dy * base_b;
        let dx2 = points[seq[2]].0 - points[seq[0]].0;
        let dy2 = points[seq[2]].1 - points[seq[0]].1;
        let height = -dx2 * base_b + dy2 * base_a;
        let area = width * height;
        if area <= minarea {
            minarea = area;
            buf_idx_left = seq[3] as i32;
            buf_base_a = base_a;
            buf_width = width;
            buf_base_b = base_b;
            buf_height = height;
            buf_idx_bottom = seq[0] as i32;
        }
    }

    let a1 = buf_base_a;
    let b1 = buf_base_b;
    let a2 = -buf_base_b;
    let b2 = buf_base_a;
    let c1 = a1 * points[buf_idx_left as usize].0 + points[buf_idx_left as usize].1 * b1;
    let c2 = a2 * points[buf_idx_bottom as usize].0 + points[buf_idx_bottom as usize].1 * b2;
    let idet = 1.0_f32 / (a1 * b2 - a2 * b1);
    let px = (c1 * b2 - c2 * b1) * idet;
    let py = (a1 * c2 - a2 * c1) * idet;
    [
        (px, py),
        (a1 * buf_width, b1 * buf_width),
        (a2 * buf_height, b2 * buf_height),
    ]
}

/// `box_score_fast`：mini box（f32 四角）裁 pred，fillPoly 掩码内均值
/// （`cv2.mean` 语义：f64 光栅序累加 / 计数，返回 f64）。
// OpenCV floor/ceil and ROI pixel conversion deliberately truncate in different domains.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn box_score_fast(pred: &[f32], map_w: usize, map_h: usize, box_pts: &[(f32, f32)]) -> f64 {
    let w = map_w as i64;
    let h = map_h as i64;
    let xmin = 0_i64.max(
        box_pts
            .iter()
            .map(|p| p.0.floor() as i64)
            .min()
            .unwrap_or(0)
            .min(w - 1),
    );
    let xmax = 0_i64.max(
        box_pts
            .iter()
            .map(|p| p.0.ceil() as i64)
            .max()
            .unwrap_or(0)
            .min(w - 1),
    );
    let ymin = 0_i64.max(
        box_pts
            .iter()
            .map(|p| p.1.floor() as i64)
            .min()
            .unwrap_or(0)
            .min(h - 1),
    );
    let ymax = 0_i64.max(
        box_pts
            .iter()
            .map(|p| p.1.ceil() as i64)
            .max()
            .unwrap_or(0)
            .min(h - 1),
    );

    let roi_w = (xmax - xmin + 1) as usize;
    let roi_h = (ymax - ymin + 1) as usize;
    let mut mask = vec![0_u8; roi_w * roi_h];
    // 坐标平移（f32）后按 C 截断转 i32（`astype(np.int32)`）。
    let poly: Vec<(i32, i32)> = box_pts
        .iter()
        .map(|&(x, y)| ((x - xmin as f32) as i32, (y - ymin as f32) as i32))
        .collect();
    fill_poly(&mut mask, roi_w, roi_h, &poly);

    let mut sum = 0.0_f64;
    let mut count = 0_u64;
    for row in 0..roi_h {
        let ysrc = (ymin + row as i64) as usize;
        for col in 0..roi_w {
            if mask[row * roi_w + col] != 0 {
                sum += f64::from(pred[ysrc * map_w + (xmin + col as i64) as usize]);
                count += 1;
            }
        }
    }
    if count == 0 {
        0.0
    } else {
        sum / (count as f64)
    }
}

/// `cv2.fillPoly`（8UC1、LINE_8、shift=0）：边表收集（16.16 定点）+ 扫描线
/// 填充 + 8 连通 Bresenham 边界线（drawing.cpp 移植）。
#[allow(clippy::too_many_lines)]
// OpenCV scanline coordinates use signed 16.16 fixed point; mask indices are clipped.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::similar_names
)]
fn fill_poly(mask: &mut [u8], mw: usize, mh: usize, pts: &[(i32, i32)]) {
    /// `XY_SHIFT`。
    const SHIFT: u32 = 16;
    const ONE_HALF: i64 = 1 << (SHIFT - 1);

    struct PolyEdge {
        y0: i32,
        y1: i32,
        x: i64,
        dx: i64,
    }

    let (mw_i, mh_i) = (mw as i32, mh as i32);
    let n = pts.len();
    if n == 0 {
        return;
    }
    let mut edges: Vec<PolyEdge> = Vec::with_capacity(n);
    // CollectPolyEdges：prev 从最后一个顶点出发，逐边收集。
    let mut prev = pts[n - 1];
    for &cur in pts {
        let mut t0 = (prev.0, prev.1);
        let mut t1 = (cur.0, cur.1);
        // 定点端点（整数输入下 == 原值；+ 半像素仅在未裁剪分支）。
        let mut p0c_x = i64::from(prev.0) << SHIFT;
        let mut p1c_x = i64::from(cur.0) << SHIFT;
        let mut p0c_y = i64::from(prev.1);
        let mut p1c_y = i64::from(cur.1);
        let out_of_bounds = t0.0 < 0
            || t0.0 >= mw_i
            || t0.1 < 0
            || t0.1 >= mh_i
            || t1.0 < 0
            || t1.0 >= mw_i
            || t1.1 < 0
            || t1.1 >= mh_i;
        if out_of_bounds {
            clip_line(mw_i, mh_i, &mut t0, &mut t1);
            if t0.1 != t1.1 {
                p0c_y = i64::from(t0.1);
                p1c_y = i64::from(t1.1);
                p0c_x = i64::from(t0.0) << SHIFT;
                p1c_x = i64::from(t1.0) << SHIFT;
            }
        } else {
            p0c_x += ONE_HALF;
            p1c_x += ONE_HALF;
        }
        draw_line8(mask, mw_i, mh_i, t0, t1);
        if p0c_y == p1c_y {
            prev = cur;
            continue;
        }
        let dx = (p1c_x - p0c_x) / (p1c_y - p0c_y);
        let edge = if p0c_y < p1c_y {
            PolyEdge {
                y0: p0c_y as i32,
                y1: p1c_y as i32,
                x: p0c_x,
                dx,
            }
        } else {
            PolyEdge {
                y0: p1c_y as i32,
                y1: p0c_y as i32,
                x: p1c_x,
                dx,
            }
        };
        edges.push(edge);
        prev = cur;
    }

    // FillEdgeCollection。
    let total = edges.len();
    if total < 2 {
        return;
    }
    let mut y_min = i32::MAX;
    let mut y_max = i32::MIN;
    let mut x_min = i64::MAX;
    let mut x_max = i64::MIN;
    for e in &edges {
        let x1 = e.x + i64::from(e.y1 - e.y0) * e.dx;
        y_min = y_min.min(e.y0);
        y_max = y_max.max(e.y1);
        x_min = x_min.min(e.x).min(x1);
        x_max = x_max.max(e.x).max(x1);
    }
    if y_max < 0 || y_min >= mh_i || x_max < 0 || x_min >= (i64::from(mw_i)) << SHIFT {
        return;
    }
    // CmpEdges：(y0, x, dx) 升序（严格全序，std::sort 与稳定排序结果一致）。
    edges.sort_by_key(|e| (e.y0, e.x, e.dx));

    // e 游标哨兵（C++ 在 edges[total] 压入 y0=INT_MAX 的 tmp 副本）。
    edges.push(PolyEdge {
        y0: i32::MAX,
        y1: i32::MAX,
        x: 0,
        dx: 0,
    });
    // 活动边表以 next 链表承载；`tmp`（表头哨兵）占独立下标。
    let tmp = edges.len();
    let null = usize::MAX;
    let mut next: Vec<usize> = vec![null; edges.len() + 1];
    let mut i = 0_usize;
    let y_max_c = y_max.min(mh_i);
    let mut y = edges[0].y0;
    while y < y_max_c {
        let mut draw = false;
        let clipline = y < 0;
        let mut prelast = tmp;
        let mut last = next[tmp];
        while last != null || edges[i].y0 == y {
            if last != null && edges[last].y1 == y {
                // 到达下端点：从活动表摘除（不触发 draw 翻转）。
                next[prelast] = next[last];
                last = next[last];
                continue;
            }
            let keep_prelast = prelast;
            if last != null && (edges[i].y0 > y || edges[last].x < edges[i].x) {
                prelast = last;
                last = next[last];
            } else if i < total {
                next[prelast] = i;
                next[i] = last;
                prelast = i;
                i += 1;
            } else {
                break;
            }
            if draw {
                if !clipline {
                    let (x1, x2) = if edges[keep_prelast].x > edges[prelast].x {
                        (edges[prelast].x >> SHIFT, edges[keep_prelast].x >> SHIFT)
                    } else {
                        (edges[keep_prelast].x >> SHIFT, edges[prelast].x >> SHIFT)
                    };
                    // LINE_8 的 delta = 0：左端不加偏置。
                    if x1 < mw as i64 && x2 >= 0 {
                        let xa = x1.max(0) as usize;
                        let xb = x2.min(mw as i64 - 1) as usize;
                        for x in xa..=xb {
                            mask[y as usize * mw + x] = 1;
                        }
                    }
                }
                edges[keep_prelast].x += edges[keep_prelast].dx;
                edges[prelast].x += edges[prelast].dx;
            }
            draw = !draw;
        }
        // 行末按 x 冒泡排序活动表（do-while + last_exchange 标记，照抄）。
        let mut keep: usize = null;
        loop {
            let mut prelast = tmp;
            let mut last = next[tmp];
            let mut last_exchange = null;
            while last != keep && last != null && next[last] != null {
                let te = next[last];
                if edges[last].x > edges[te].x {
                    // 交换后 last 仍指向原节点（C++ 仅在 else 分支前进）。
                    next[prelast] = te;
                    next[last] = next[te];
                    next[te] = last;
                    prelast = te;
                    last_exchange = te;
                } else {
                    prelast = last;
                    last = te;
                }
            }
            if last_exchange == null {
                break;
            }
            keep = last_exchange;
            if keep == next[tmp] || keep == tmp {
                break;
            }
        }
        y += 1;
    }
}

/// `clipLine(Size)`（drawing.cpp:93-146，整数端点版；double 域插值后 C 截断）。
#[allow(clippy::too_many_lines)]
// clipLine interpolates in double precision then truncates to integral pixel endpoints.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn clip_line(mw: i32, mh: i32, p1: &mut (i32, i32), p2: &mut (i32, i32)) -> bool {
    let right = i64::from(mw) - 1;
    let bottom = i64::from(mh) - 1;
    let (mut x1, mut y1) = (i64::from(p1.0), i64::from(p1.1));
    let (mut x2, mut y2) = (i64::from(p2.0), i64::from(p2.1));
    let c1_0 = i32::from(x1 < 0)
        + i32::from(x1 > right) * 2
        + i32::from(y1 < 0) * 4
        + i32::from(y1 > bottom) * 8;
    let c2_0 = i32::from(x2 < 0)
        + i32::from(x2 > right) * 2
        + i32::from(y2 < 0) * 4
        + i32::from(y2 > bottom) * 8;
    let (mut c1, mut c2) = (c1_0, c2_0);

    if (c1 & c2) == 0 && (c1 | c2) != 0 {
        let mut a: i64;
        if (c1 & 0b1100) != 0 {
            a = if c1 < 8 { 0 } else { bottom };
            x1 += ((a - y1) as f64 * (x2 - x1) as f64 / (y2 - y1) as f64) as i64;
            y1 = a;
        }
        if (c2 & 0b1100) != 0 {
            a = if c2 < 8 { 0 } else { bottom };
            x2 += ((a - y2) as f64 * (x2 - x1) as f64 / (y2 - y1) as f64) as i64;
            y2 = a;
        }
        c1 = i32::from(x1 < 0) + i32::from(x1 > right) * 2;
        c2 = i32::from(x2 < 0) + i32::from(x2 > right) * 2;
        if (c1 & c2) == 0 && (c1 | c2) != 0 {
            if c1 != 0 {
                a = if c1 == 1 { 0 } else { right };
                y1 += ((a - x1) as f64 * (y2 - y1) as f64 / (x2 - x1) as f64) as i64;
                x1 = a;
                c1 = 0;
            }
            if c2 != 0 {
                a = if c2 == 1 { 0 } else { right };
                y2 += ((a - x2) as f64 * (y2 - y1) as f64 / (x2 - x1) as f64) as i64;
                x2 = a;
                c2 = 0;
            }
        }
    }
    *p1 = (x1 as i32, y1 as i32);
    *p2 = (x2 as i32, y2 as i32);
    (c1 | c2) == 0
}

/// 8 连通 Bresenham 线（`LineIterator` + `Line`，leftToRight=true）：把线上
/// 像素（含两端点）置 1。err 语义：每步先按当前 err 决定对角步，再
/// `err += -2*minor + (2*major 若 err<0)`。
// Bresenham uses signed deltas and converts only in-bounds coordinates to mask indices.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn draw_line8(mask: &mut [u8], mw: i32, mh: i32, p1: (i32, i32), p2: (i32, i32)) {
    let mut pt1 = p1;
    let mut pt2 = p2;
    // LineIterator::init 的二次裁剪（端点越界时）。
    if pt1.0 < 0
        || pt1.0 >= mw
        || pt1.1 < 0
        || pt1.1 >= mh
        || pt2.0 < 0
        || pt2.0 >= mw
        || pt2.1 < 0
        || pt2.1 >= mh
    {
        clip_line(mw, mh, &mut pt1, &mut pt2);
    }
    let mut dx = i64::from(pt2.0) - i64::from(pt1.0);
    let mut dy = i64::from(pt2.1) - i64::from(pt1.1);
    // leftToRight=true：dx<0 时交换端点（dy 一并取反）。
    if dx < 0 {
        dx = -dx;
        dy = -dy;
        std::mem::swap(&mut pt1, &mut pt2);
    }
    let mut delta_y = 1_i64;
    if dy < 0 {
        dy = -dy;
        delta_y = -1;
    }
    let vert = dy > dx;
    if vert {
        // 交换后 dx = 主轴（原 |dy|），dy = 次轴。
        std::mem::swap(&mut dx, &mut dy);
    }
    let count = (dx + 1) as usize;
    let mut err = dx - (dy + dy);
    let (mut x, mut y) = (pt1.0, pt1.1);
    for _ in 0..count {
        if x >= 0 && x < mw && y >= 0 && y < mh {
            mask[y as usize * mw as usize + x as usize] = 1;
        }
        let diag = err < 0;
        if vert {
            y += delta_y as i32;
            if diag {
                x += 1;
            }
        } else {
            x += 1;
            if diag {
                y += delta_y as i32;
            }
        }
        err += if diag {
            (dx + dx) - (dy + dy)
        } else {
            -(dy + dy)
        };
    }
}

/// `unclip`（processors.py:421-432）：`contourArea`（double 域 Green 定理）与
/// `arcLength`（**f32** 域 hypot 后逐段以 double 累加）→ `distance =
/// area*1.5/len` → Clipper 6.4.2 JT_ROUND/ET_CLOSEDPOLYGON 偏移（pyclipper
/// 语义：坐标 C 截断入整数域、delta 为 double、ArcTolerance 0.25）。
// pyclipper truncates f32 polygon vertices toward zero on conversion to Clipper int64.
#[allow(clippy::cast_possible_truncation)]
fn unclip(points: &[(f32, f32)]) -> Vec<(i64, i64)> {
    let n = points.len();
    if n == 0 {
        return Vec::new();
    }
    // contourArea（oriented=False → |a00|；每项 double 乘减）。
    let mut a00 = 0.0_f64;
    let mut prev = points[n - 1];
    for &p in points {
        a00 += f64::from(prev.0) * f64::from(p.1) - f64::from(prev.1) * f64::from(p.0);
        prev = p;
    }
    let area = (a00 * 0.5).abs();
    // arcLength（closed=True）：dx/dy 与平方和均在 f32 域，sqrt 后升 double 累加。
    let mut perimeter = 0.0_f64;
    let mut prev = points[n - 1];
    for &p in points {
        let dx = p.0 - prev.0;
        let dy = p.1 - prev.1;
        perimeter += f64::from((dx * dx + dy * dy).sqrt());
        prev = p;
    }
    let distance = area * UNCLIP_RATIO / perimeter;

    // pyclipper：f32 坐标按 C 截断到整数域。
    let coords: Vec<Coord<i64>> = points
        .iter()
        .map(|&(x, y)| Coord {
            x: x as i64,
            y: y as i64,
        })
        .collect();
    let poly = Polygon::new(LineString::new(coords), vec![]);
    let solution = poly.offset(distance, JoinType::Round(0.25), EndType::ClosedPolygon);
    // geo-types closes every Polygon exterior by repeating its first vertex.
    // Clipper/pyclipper paths do not include that closing vertex; retain only
    // the actual offset points when flattening the result.
    let mut out = Vec::new();
    for polygon in &solution.0 {
        let ring = &polygon.exterior().0;
        let end = if ring.len() > 1 && ring.first() == ring.last() {
            ring.len() - 1
        } else {
            ring.len()
        };
        out.extend(ring[..end].iter().map(|c| (c.x, c.y)));
    }
    out
}

// ══════════════════ findContours（Suzuki 边界跟踪） ══════════════════

/// `cv2.findContours(RETR_LIST, CHAIN_APPROX_SIMPLE)`（8UC1）逐位移植
/// （contours_new.cpp：1 像素零边框、光栅扫描发现、`icvFetchContourEx` 的
/// CHAIN_APPROX_SIMPLE 压缩与原地 nbd 标记；输出经轮廓树 LIFO 出栈 =
/// 发现序的**逆序**，与 cv2 4.10 一致）。返回轮廓点列（原图坐标）。
#[allow(clippy::too_many_lines)]
// The one-pixel border bounds all contour origins to OpenCV's signed coordinates.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn find_contours_list_simple(mask: &[u8], w: usize, h: usize) -> Vec<Vec<(i32, i32)>> {
    // 1 像素零边框（copyMakeBorder → 扫描域 x,y ∈ [1, w|h]）。
    let pw = w + 2;
    let ph = h + 2;
    let mut img: Vec<i8> = vec![0; pw * ph];
    for y in 0..h {
        for x in 0..w {
            img[(y + 1) * pw + (x + 1)] = i8::from(mask[y * w + x] != 0);
        }
    }

    let mut contours: Vec<Vec<(i32, i32)>> = Vec::new();
    let width = pw - 1;
    let mut x = 1_usize;
    let mut prev: i8 = 0;
    for y in 1..ph - 1 {
        while x < width {
            let p = img[y * pw + x];
            if p == prev {
                x += 1;
                continue;
            }
            // 外轮廓：prev==0 && p==1；否则孔轮廓要求 p==0 && prev≥1。
            let is_hole = if prev == 0 && p == 1 {
                false
            } else {
                if p != 0 || prev < 1 {
                    // resume_scan（lnbd 仅供父级追踪，RETR_LIST 下跳过）。
                    prev = p;
                    x += 1;
                    continue;
                }
                true
            };
            let origin = (x as i32 - i32::from(is_hole), y as i32);
            contours.push(fetch_contour(&mut img, pw, origin, is_hole));
            // 返回后 prev = 起点像素的标记值，扫描从 x+1 继续。
            prev = img[y * pw + x];
            x += 1;
        }
        x = 1;
        prev = 0;
    }
    // cv2 4.10 实际走 contours_new.cpp：轮廓树 `addChild` 头插 + 迭代器
    // LIFO 出栈 → 输出为发现序的**逆序**（RETR_LIST 下全部轮廓为根子节点）。
    contours.reverse();
    contours
}

/// `icvFetchContour`（CHAIN_APPROX_SIMPLE）：从 origin 沿边界跟踪并压缩，
/// 原地标记 `nbd=2` / `nbd|-128`（右界规则）。
#[allow(clippy::too_many_lines)]
// The padded zero border keeps boundary-following neighbor coordinates nonnegative.
#[allow(clippy::cast_sign_loss)]
fn fetch_contour(img: &mut [i8], pw: usize, origin: (i32, i32), is_hole: bool) -> Vec<(i32, i32)> {
    /// `icvCodeDeltas`（与 `CV_INIT_3X3_DELTAS` 同序：E/NE/N/NW/W/SW/S/SE）。
    const CODE_D: [(i32, i32); 8] = [
        (1, 0),
        (1, -1),
        (0, -1),
        (-1, -1),
        (-1, 0),
        (-1, 1),
        (0, 1),
        (1, 1),
    ];
    let nbd: i8 = 2;
    let at = |img: &[i8], x: i32, y: i32| img[y as usize * pw + x as usize];
    let mut pts = Vec::new();
    let (mut px, mut py) = origin;
    let s_start = if is_hole { 0_usize } else { 4_usize };

    // 初始逆时针搜索（do-while：非零邻点或回到 s_end 退出）。
    let mut s = s_start;
    loop {
        s = (s + 7) & 7;
        let (dx, dy) = CODE_D[s];
        if at(img, px + dx, py + dy) != 0 {
            break;
        }
        if s == s_start {
            // 单像素域。
            img[py as usize * pw + px as usize] = nbd | -128;
            pts.push((px - 1, py - 1));
            return pts;
        }
    }

    let (i1x, i1y) = (px + CODE_D[s].0, py + CODE_D[s].1);
    let (mut i3x, mut i3y) = (px, py);
    let mut prev_s = s ^ 4;
    loop {
        let s_end = s;
        // 顺时针搜索（s2 ∈ (s_end, 15]，经镜像覆盖 8 邻域）。
        let mut s2 = s_end;
        let mut i4x;
        let mut i4y;
        loop {
            s2 += 1;
            let (dx, dy) = CODE_D[s2 & 7];
            i4x = i3x + dx;
            i4y = i3y + dy;
            if at(img, i4x, i4y) != 0 || s2 >= 15 {
                break;
            }
        }
        let s_new = s2 & 7;
        // 右界标记：`(unsigned)(s-1) < (unsigned)s_end` ⟺ 1 ≤ s ≤ s_end。
        let cur = at(img, i3x, i3y);
        if s_new >= 1 && s_new - 1 < s_end {
            img[i3y as usize * pw + i3x as usize] = nbd | -128;
        } else if cur == 1 {
            img[i3y as usize * pw + i3x as usize] = nbd;
        }
        // CHAIN_APPROX_SIMPLE：方向变化才记录当前点。
        if s_new != prev_s {
            pts.push((px - 1, py - 1));
            prev_s = s_new;
        }
        px += CODE_D[s_new].0;
        py += CODE_D[s_new].1;
        if i4x == origin.0 && i4y == origin.1 && i3x == i1x && i3y == i1y {
            break;
        }
        i3x = i4x;
        i3y = i4y;
        s = (s_new + 4) & 7;
    }
    pts
}

// ══════════════════ 测试挂钩（doc(hidden)：集成测试经 lib 访问）══════════════════

/// 分级对照（探针 dump）：`DetResizeForTest` 的 resize 决策与像素输出。
#[doc(hidden)]
#[must_use]
pub fn __test_resize_type0(bgr: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    resize_type0(bgr, w, h)
}

/// 分级对照（探针 dump）：Suzuki 轮廓（8UC1 mask）。
#[doc(hidden)]
#[must_use]
pub fn __test_find_contours(mask: &[u8], w: usize, h: usize) -> Vec<Vec<(i32, i32)>> {
    find_contours_list_simple(mask, w, h)
}

/// 分级对照：二值化阈值（构建探针 mask 用）。
#[doc(hidden)]
pub const __TEST_BINARIZE_THRESH: f32 = BINARIZE_THRESH;

#[doc(hidden)]
impl DetPaddlex {
    /// 分级对照（探针 dump）：resize → normalize → 概率图全链。
    #[allow(clippy::unwrap_used)]
    pub fn __test_probmap(&self, bgr: &[u8], tw: u32, th: u32) -> (Vec<u8>, u32, u32, Vec<f32>) {
        let (resized, rh, rw) = resize_type0(bgr, tw, th);
        let tensor = normalize_chw(&resized, rw, rh);
        let pred = self.run_probmap(&tensor, rw, rh).unwrap_or_default();
        (resized, rh, rw, pred)
    }
}
/// 分级对照（探针 manifest details）：`get_mini_boxes` 的四角与 sside。
#[doc(hidden)]
#[must_use]
pub fn __test_mini_boxes(points: &[(i32, i32)]) -> (Vec<(f32, f32)>, f32) {
    get_mini_boxes(points)
}

/// 分级对照（探针 manifest details）：unclip 的 (area, length, distance, 点列)。
#[doc(hidden)]
#[must_use]
pub fn __test_unclip(points: &[(f32, f32)]) -> (f64, f64, f64, Vec<(i64, i64)>) {
    let n = points.len();
    let mut a00 = 0.0_f64;
    let mut prev = points[n - 1];
    for &p in points {
        a00 += f64::from(prev.0) * f64::from(p.1) - f64::from(prev.1) * f64::from(p.0);
        prev = p;
    }
    let area = (a00 * 0.5).abs();
    let mut perimeter = 0.0_f64;
    let mut prev = points[n - 1];
    for &p in points {
        let dx = p.0 - prev.0;
        let dy = p.1 - prev.1;
        perimeter += f64::from((dx * dx + dy * dy).sqrt());
        prev = p;
    }
    let distance = area * UNCLIP_RATIO / perimeter;
    (area, perimeter, distance, unclip(points))
}
