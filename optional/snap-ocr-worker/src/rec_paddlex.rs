//! paddlex `PP-OCRv6_small_rec` 识别链的逐位对齐实现（O-26）。
//!
//! 权威语义（冻结栈：paddlex 3.7.2 / paddleocr 3.7.0，经 TextSnap 的
//! `paddleocr.TextRecognition` 调用，`batch_size = 8`）：
//!
//! - 预处理 = paddlex `OCRReisizeNormImg`（`text_recognition/processors.py:56-79`）：
//!   单图 `max_wh_ratio = max(320/48, w/h)`；`imgW = int(48 * max_wh_ratio)` 上限
//!   3200（上限分支直接缩放到 3200，与「先夹 imgW 再取 `resized_w =
//!   min(ceil(48·w/h), imgW)`」等价——上限仅在 `w/h > 320/48` 触发，此时
//!   `ceil(48·w/h) ≥ int(48·w/h) > 3200`）；`cv2.resize((resized_w, 48))`
//!   默认 INTER_LINEAR（走 [`crate::image_ops::resize_inter_linear`]）；
//!   归一化运算序为 f32 的 `v/255 → -0.5 → /0.5`（不可换序）；CHW 布局零填充
//!   到 `imgW`（填充值 0.0，非 -1.0）。
//! - 批语义 = paddlex `ToBatch`：批内（输入序、每 8 张一批）补零到
//!   `max(imgW_i)` 后堆叠为 `(N, 3, 48, W)` 一次前向。填充列是模型输入的
//!   一部分（单图路径同样填充到自身 `imgW`），故按批复刻即可与 oracle 逐位
//!   一致；调用方按 TextSnap 的分批方式（`_recognize_initial` 等按 8 张切片）
//!   传入即可复现 oracle 的批构成。
//! - 解码 = paddleocr `CTCLabelDecode`：逐步 `argmax`（并列取先出现者）、
//!   折叠相邻重复、剔除 blank（idx 0）、`score = mean(保留步最大概率)`（f32
//!   pairwise 求和 / 个数，与 `np.mean` 一致），空串 score = 0。
//!   字典布局 `[blank] + dict(18708 行) + [space]`（类数 18710 =
//!   `MissingBlankAndSpace`，与 vendored `RecognitionDictionaryLayout` 同构）。
//!
//! 会话参数镜像 TextSnap `_DEFAULT_ENGINE_CONFIG`（graph 全优化、intra 线程、
//! inter 1）；`onnxruntime.dll` 经 `ORT_DYLIB_PATH` 或 exe 相邻目录加载
//! （`ort` load-dynamic 特性）。

use std::fmt;
use std::path::Path;
use std::sync::Mutex;

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor as OrtTensor;

use crate::image_ops::{resize_inter_linear, BgrImage};

/// rec_image_shape 的输入高度（yml `[3, 48, 320]`）。
const REC_IMAGE_HEIGHT: usize = 48;
/// 基线宽高比 `imgW/imgH = 320/48`。
const BASE_WH_RATIO: f64 = 320.0 / 48.0;
/// paddlex `OCRReisizeNormImg.max_imgW`。
const MAX_IMG_W: i64 = 3200;
/// TextSnap `RECOGNITION_BATCH_SIZE`（O-26）。
pub const RECOGNITION_BATCH_SIZE: usize = 8;

/// 识别链错误。
#[derive(Debug)]
pub enum RecError {
    /// 字典 / 模型文件读写或解析失败。
    Io(std::io::Error),
    /// ONNX Runtime 错误（建会话 / 前向 / 张量解析）。
    Ort(ort::Error),
    /// 字典行数与模型输出类数不符合 paddlex 布局约束。
    DictLayout {
        /// 字典条目数（不含 blank/space）。
        dict_entries: usize,
        /// 模型输出类数。
        class_count: usize,
    },
    /// 输出张量形状非法（非 `(N, T, C)` 或 N 不符）。
    OutputShape(String),
    /// 输入裁剪尺寸非法（零宽/零高，对应 paddlex `validate_text_rec_image_array`）。
    InvalidCrop(usize),
}

impl fmt::Display for RecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "rec asset io error: {e}"),
            Self::Ort(e) => write!(f, "onnx runtime error: {e}"),
            Self::DictLayout {
                dict_entries,
                class_count,
            } => write!(
                f,
                "rec dictionary has {dict_entries} entries but model output has {class_count} \
                 classes; expected classes == entries + 2 ([blank] + dict + [space])"
            ),
            Self::OutputShape(msg) => write!(f, "rec output tensor shape invalid: {msg}"),
            Self::InvalidCrop(index) => {
                write!(f, "rec crop at index {index} has zero width or height")
            }
        }
    }
}

impl std::error::Error for RecError {}

impl From<std::io::Error> for RecError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<ort::Error> for RecError {
    fn from(e: ort::Error) -> Self {
        Self::Ort(e)
    }
}

/// `SessionBuilder` 各配置方法返回按 builder 状态参数化的 `ort::Error`，
/// 统一降维为 [`RecError::Ort`]（与 vendored `ort_backend.rs` 同法）。
#[allow(clippy::needless_pass_by_value)] // map_err passes owned ort errors to this adapter.
fn map_builder_err<S>(e: ort::Error<S>) -> RecError {
    RecError::Ort(ort::Error::new(e.message()))
}

/// paddlex `OCRReisizeNormImg.resize_norm_img` 的几何（O-26/附录 B 取整）。
///
/// 返回 `(imgW, resized_w)`：`imgW = int(48 * max(320/48, w/h))` 截断取整、
/// 上限 3200；`resized_w = min(ceil(48 * w/h), imgW)`。上限分支 paddlex 直接
/// 缩放到 3200，与本函数的统一式等价（见模块文档论证）。
// The oracle computes w/h as f64, then Python int()/ceil(); changing to integer
// arithmetic or checking overflow would change its rounding at large dimensions.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(crate) fn rec_resize_geometry(width: usize, height: usize) -> (usize, usize) {
    let ratio = width as f64 / height as f64; // paddlex: w * 1.0 / h
    let max_wh_ratio = if ratio > BASE_WH_RATIO {
        ratio
    } else {
        BASE_WH_RATIO
    };
    let img_w = (REC_IMAGE_HEIGHT as f64 * max_wh_ratio) as i64; // python int() 截断
    if img_w > MAX_IMG_W {
        (MAX_IMG_W as usize, MAX_IMG_W as usize)
    } else {
        let ceil = (REC_IMAGE_HEIGHT as f64 * ratio).ceil() as i64;
        let resized_w = if ceil > img_w { img_w } else { ceil };
        (img_w as usize, resized_w as usize)
    }
}

/// 单张裁剪的归一化（`resize_norm_img` 数值部分）。
///
/// 返回 `(resized_w, CHW f32)`：先 INTER_LINEAR 缩放到 `(resized_w, 48)`，
/// 再按 paddlex 运算序 `v/255 → -0.5 → /0.5`（f32 逐序，不可换序），
/// 布局 `[c][y][x] = c*(48*resized_w) + y*resized_w + x`。
fn resize_norm_img(crop: &BgrImage) -> (usize, Vec<f32>) {
    let (img_w, resized_w) = rec_resize_geometry(crop.width(), crop.height());
    let _ = img_w; // 批内统一补零到 max(imgW)，由调用方处理
    let resized = resize_inter_linear(crop, resized_w, REC_IMAGE_HEIGHT);
    let mut buf = vec![0f32; 3 * REC_IMAGE_HEIGHT * resized_w];
    let plane = REC_IMAGE_HEIGHT * resized_w;
    for y in 0..REC_IMAGE_HEIGHT {
        for x in 0..resized_w {
            let px = resized.pixel(y, x);
            for (c, v) in px.iter().enumerate() {
                let mut f = f32::from(*v) / 255.0;
                f -= 0.5;
                f /= 0.5;
                buf[c * plane + y * resized_w + x] = f;
            }
        }
    }
    (resized_w, buf)
}

/// numpy `np.add.reduce` 的 f32 pairwise 求和复刻（`loops_utils.h`）：
/// `n < 8` 顺序累加；`n ≤ 128` 用 8 路累加器 + `((r0+r1)+(r2+r3)) +
/// ((r4+r5)+(r6+r7))` 合并 + 尾段；更大的 `n` 在 8 的倍数中点二分递归。
fn np_sum_f32(a: &[f32]) -> f32 {
    let n = a.len();
    if n < 8 {
        let mut r = 0f32;
        for v in a {
            r += v;
        }
        return r;
    }
    if n <= 128 {
        let mut r = [0f32; 8];
        r.copy_from_slice(&a[..8]);
        let mut i = 8;
        while i < n - n % 8 {
            for k in 0..8 {
                r[k] += a[i + k];
            }
            i += 8;
        }
        let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        while i < n {
            res += a[i];
            i += 1;
        }
        return res;
    }
    let mut n2 = n / 2;
    n2 -= n2 % 8;
    np_sum_f32(&a[..n2]) + np_sum_f32(&a[n2..])
}

/// CTC 解码单样本（`CTCLabelDecode.__call__` + `BaseRecLabelDecode.decode`）。
///
/// `probs` 为 `(T, C)` 行主序的 f32 概率；返回 `(text, score)`。折叠/剔除
/// 语义：`selection[0]` 恒真、`selection[t] = idx[t] != idx[t-1]`（对原始
/// 索引、含 blank），再剔除 blank（0）；score 为保留步概率的 f32 均值
/// （pairwise 求和），空串 score = 0。
#[allow(clippy::cast_precision_loss)] // NumPy divides f32 confidence by a f32 count.
fn ctc_decode_sample(probs: &[f32], classes: usize, characters: &[String]) -> (String, f64) {
    let timesteps = probs.len() / classes;
    let mut text = String::new();
    let mut confs: Vec<f32> = Vec::new();
    let mut last: Option<usize> = None;
    for t in 0..timesteps {
        let row = &probs[t * classes..(t + 1) * classes];
        let mut max_index = 0usize;
        let mut max_value = f32::NEG_INFINITY;
        for (i, &v) in row.iter().enumerate() {
            if v > max_value {
                max_value = v;
                max_index = i;
            }
        }
        let selected = max_index != 0 && last != Some(max_index);
        if selected {
            if let Some(ch) = characters.get(max_index) {
                text.push_str(ch);
            }
            confs.push(max_value);
        }
        last = Some(max_index);
    }
    let score = if confs.is_empty() {
        0.0
    } else {
        f64::from(np_sum_f32(&confs) / confs.len() as f32)
    };
    (text, score)
}

/// 已加载的 paddlex 识别器（ONNX 会话 + 字典）。
pub struct RecPaddlex {
    session: Mutex<Session>,
    /// `[blank]+dict+[space]` 全布局字符表（索引即输出类号）。
    characters: Vec<String>,
}

impl RecPaddlex {
    /// 加载 rec ONNX 与字典（18708 行、UTF-8、LF）。
    ///
    /// 会话镜像 TextSnap 引擎配置：图优化 All、`num_thread` intra 线程、
    /// 1 inter 线程。运行期需 `ORT_DYLIB_PATH` 指向 onnxruntime 动态库。
    ///
    /// # Errors
    /// 文件读写 / 会话构建失败返回 [`RecError`]。
    pub fn load(model_path: &Path, dict_path: &Path, num_thread: usize) -> Result<Self, RecError> {
        let session = {
            let _gate = crate::det_paddlex::SESSION_BUILD_GATE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut builder = Session::builder()?
                .with_optimization_level(GraphOptimizationLevel::All)
                .map_err(map_builder_err)?
                .with_intra_threads(num_thread)
                .map_err(map_builder_err)?
                .with_inter_threads(1)
                .map_err(map_builder_err)?;
            builder.commit_from_file(model_path)?
        };
        let content = std::fs::read_to_string(dict_path)?;
        let mut keys: Vec<String> = content.split('\n').map(str::to_string).collect();
        if keys.last().is_some_and(String::is_empty) {
            keys.pop(); // 文件以换行结尾 → 恰好 18708 行
        }
        let mut characters = Vec::with_capacity(keys.len() + 2);
        characters.push(String::new()); // idx 0 = blank，永不输出
        characters.extend(keys);
        characters.push(" ".to_string()); // idx = dict + 1 = space
        Ok(Self {
            session: Mutex::new(session),
            characters,
        })
    }

    /// 识别一批裁剪（BGR），返回与输入同序的 `(text, score)`。
    ///
    /// 预处理/批语义/解码逐位复刻 paddlex（见模块文档）；内部按
    /// [`RECOGNITION_BATCH_SIZE`]（8）以输入序分批，单批内补零到
    /// `max(imgW_i)` 后一次前向——与 TextSnap `_recognize_initial` 的
    /// 分批方式一致。空输入返回空。
    ///
    /// # Errors
    /// 裁剪尺寸非法、类数与字典不符或前向失败返回 [`RecError`]。
    pub fn recognize_crops(&self, crops: &[BgrImage]) -> Result<Vec<(String, f64)>, RecError> {
        let mut results = Vec::with_capacity(crops.len());
        for batch in crops.chunks(RECOGNITION_BATCH_SIZE) {
            results.extend(self.recognize_batch(batch)?);
        }
        Ok(results)
    }

    /// 单批前向（paddlex `ToBatch` + `runner(x=...)` + `CTCLabelDecode`）。
    // ONNX tensor dimensions are i64; preserve the original conversion of the
    // model's dimensions back to native indexing sizes.
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn recognize_batch(&self, batch: &[BgrImage]) -> Result<Vec<(String, f64)>, RecError> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let mut geometries = Vec::with_capacity(batch.len());
        for (index, crop) in batch.iter().enumerate() {
            if crop.width() == 0 || crop.height() == 0 {
                return Err(RecError::InvalidCrop(index));
            }
            geometries.push(rec_resize_geometry(crop.width(), crop.height()));
        }
        let mut norms = Vec::with_capacity(batch.len());
        for crop in batch {
            norms.push(resize_norm_img(crop));
        }
        let batch_w = geometries
            .iter()
            .map(|(img_w, _)| *img_w)
            .max()
            .unwrap_or(320);

        // ToBatch：补零到 max(imgW) 后堆叠 (N, 3, 48, W)。
        let n = batch.len();
        let plane = REC_IMAGE_HEIGHT * batch_w;
        let mut x = vec![0f32; n * 3 * plane];
        for (i, (resized_w, norm)) in norms.iter().enumerate() {
            for c in 0..3usize {
                for y in 0..REC_IMAGE_HEIGHT {
                    let dst = (i * 3 + c) * plane + y * batch_w;
                    let src = c * REC_IMAGE_HEIGHT * resized_w + y * resized_w;
                    x[dst..dst + resized_w].copy_from_slice(&norm[src..src + resized_w]);
                }
            }
        }
        let shape = vec![n as i64, 3, REC_IMAGE_HEIGHT as i64, batch_w as i64];
        let input = OrtTensor::from_array((shape, x))?;
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outputs = session.run(ort::inputs![input])?;
        let output = outputs
            .values()
            .next()
            .ok_or_else(|| RecError::OutputShape("no output tensor".to_string()))?;
        let (out_shape, out_data) = output.try_extract_tensor::<f32>()?;
        if out_shape.len() != 3 {
            return Err(RecError::OutputShape(format!(
                "expected (N, T, C), got {out_shape:?}"
            )));
        }
        let classes = out_shape[2] as usize;
        if classes != self.characters.len() {
            return Err(RecError::DictLayout {
                dict_entries: self.characters.len() - 2,
                class_count: classes,
            });
        }
        if out_shape[0] as usize != n {
            return Err(RecError::OutputShape(format!(
                "expected batch {n}, got {}",
                out_shape[0]
            )));
        }
        let timesteps = out_shape[1] as usize;
        let per_sample = timesteps * classes;
        let mut results = Vec::with_capacity(n);
        for (i, item) in out_data.chunks(per_sample).take(n).enumerate() {
            let _ = i;
            results.push(ctc_decode_sample(item, classes, &self.characters));
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn geometry_matches_paddlex_rules() {
        // 窄图：max_wh_ratio = 320/48 → int(48*(320/48)) = 320（f64 仍恰为 320）。
        assert_eq!(rec_resize_geometry(602, 51), (566, 566));
        // 极窄：ratio < 320/48 → imgW 夹 320，resized_w = ceil(48*ratio)。
        assert_eq!(rec_resize_geometry(100, 100), (320, 48));
        assert_eq!(rec_resize_geometry(155, 28), (320, 266));
        // 宽图：imgW = int(48*w/h)，ceil 与 int 相等时取自身。
        assert_eq!(rec_resize_geometry(1364, 30), (2182, 2182));
        // ceil 边界：48*1000/22 = 2181.81… → imgW=2181、resized_w=2182 > imgW → 夹 2181。
        assert_eq!(rec_resize_geometry(1000, 22), (2181, 2181));
        // 3200 上限：48*16000/22 ≈ 34909 > 3200 → (3200, 3200)。
        assert_eq!(rec_resize_geometry(16000, 22), (3200, 3200));
        // int(48*(320/48)) 恰为 320（无 f64 截断漂移）。
        assert_eq!((48.0f64 * BASE_WH_RATIO) as i64, 320);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn normalization_follows_paddlex_operation_order() {
        // 固定 1×2 图：(B,G,R) = (0, 127, 255)。手算（f32 逐序）：
        // 0 → 0/255=0 → -0.5 → /0.5 = -1.0
        // 127 → 127/255 = 0.49803922 → -0.0019607844 → /0.5 = -0.0039215689
        // 255 → 1.0 → 0.5 → 1.0
        // 2×48 竖图（ratio = 1/24 → resized_w = ceil(48/24) = 2，scale=1 恒等缩放）。
        // 第 0 列恒 (B,G,R) = (0,127,255)，第 1 列恒 (8,9,10)。
        let mut data = Vec::with_capacity(2 * 48 * 3);
        for _ in 0..48 {
            data.extend_from_slice(&[0, 127, 255, 8, 9, 10]);
        }
        let img = BgrImage::from_vec(2, 48, data).unwrap();
        let (resized_w, buf) = resize_norm_img(&img);
        assert_eq!(resized_w, 2);
        assert_eq!(buf.len(), 3 * 48 * 2);
        // CHW：[c][y][x]，任意行同值。
        assert_eq!(buf[0], -1.0f32); // B=0 → (0/255-0.5)/0.5
        assert!((buf[48 * 2] - (-0.003_921_569_f32)).abs() < 1e-9); // G=127
        assert_eq!(buf[2 * 48 * 2], 1.0f32); // R=255
                                             // 第 1 列 (8,9,10)：(v/255-0.5)/0.5。
        let expect = (
            (8f32 / 255.0 - 0.5) / 0.5,
            (9f32 / 255.0 - 0.5) / 0.5,
            (10f32 / 255.0 - 0.5) / 0.5,
        );
        assert!((buf[1] - expect.0).abs() < 1e-7);
        assert!((buf[48 * 2 + 1] - expect.1).abs() < 1e-7);
        assert!((buf[2 * 48 * 2 + 1] - expect.2).abs() < 1e-7);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn ctc_decode_collapse_blank_mean_and_empty() {
        // 5 步 × 4 类（blank, 'a', 'b', ' '）。字符表按 [blank]+dict+[space]。
        let characters = vec![
            String::new(), // blank
            "a".to_string(),
            "b".to_string(),
            " ".to_string(),
        ];
        // 步序列 idx: [1, 1, 0, 1, 2]（每行最大值落在该类）→
        // 折叠相邻 + 去 blank → "aab"；保留步 [0,3,4] → mean(.9,.6,.5)。
        let probs: Vec<f32> = vec![
            0.05, 0.9, 0.03, 0.02, //
            0.1, 0.8, 0.05, 0.05, //
            0.7, 0.1, 0.1, 0.1, //
            0.2, 0.6, 0.1, 0.1, //
            0.1, 0.2, 0.5, 0.2,
        ];
        let (text, score) = ctc_decode_sample(&probs, 4, &characters);
        assert_eq!(text, "aab");
        let expect = f64::from((0.9f32 + 0.6f32 + 0.5f32) / 3.0f32);
        assert!((score - expect).abs() < 1e-12);
        // 全 blank → 空串 score 0。
        let blank: Vec<f32> = (0..12)
            .map(|i| if i % 4 == 0 { 0.9 } else { 0.03 })
            .collect();
        let (t2, s2) = ctc_decode_sample(&blank, 4, &characters);
        assert_eq!(t2, "");
        assert_eq!(s2, 0.0);
        // blank 分隔的重复保留：idx [1, 0, 1] → "aa"（mean(.9,.9)）。
        let sep: Vec<f32> = vec![
            0.05, 0.9, 0.03, 0.02, //
            0.8, 0.1, 0.05, 0.05, //
            0.05, 0.9, 0.03, 0.02,
        ];
        let (t3, s3) = ctc_decode_sample(&sep, 4, &characters);
        assert_eq!(t3, "aa");
        assert!((s3 - f64::from(0.9f32.midpoint(0.9f32))).abs() < 1e-12);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn np_sum_matches_sequential_for_small_inputs() {
        let a = vec![1.5f32, 2.5, 3.5, 4.5, 5.5];
        let expect = a.iter().copied().sum::<f32>();
        assert_eq!(np_sum_f32(&a), expect);
        // 8 元素：r 初始化后无循环、无尾段 → 与顺序和一致。
        let b: Vec<f32> = (0..8u8).map(|i| f32::from(i) * 0.25).collect();
        assert_eq!(np_sum_f32(&b), b.iter().copied().sum::<f32>());
    }
}
