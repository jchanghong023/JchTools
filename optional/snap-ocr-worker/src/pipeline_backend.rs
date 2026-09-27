//! `snap_ocr_core::pipeline::OcrBackend` 的真实实现（O-23/O-24/O-26）。
//!
//! 组合 [`DetPaddlex`]（检测）与 [`RecPaddlex`]（识别）两个常驻 ort 会话，
//! 图像句柄即 [`BgrImage`]；增强/拉伸/旋转直接映射到 [`crate::image_ops`]
//! 的 OpenCV 对照算子：
//!
//! - `tile`：`ocr.py:164-166` 的 `image[y:bottom, x:right]` 连续化视图；
//! - `enhance_small_crop`：2× INTER_CUBIC（`SMALL_TEXT_SCALE_FACTOR`）；
//! - `stretch_recognition_crop`：宽 ×1.5（半偶取整）INTER_LANCZOS4；
//! - `rotate_90s`：`numpy.rot90`；
//! - `recognize`：整批透传 [`RecPaddlex::recognize_crops`]（管线已按 8 切批，
//!   内部再按 8 幂等分批不改变批构成）；
//! - `detect`：透传 [`DetPaddlex::detect_tile_detached`]（瓦片局部坐标）。
//!
//! oracle 验收入口可按 `oracle_dump.py` 的 `_DetRecorder`/`_RecRecorder`
//! 语义记录逐调用明细；产品服务不保留截图裁剪摘要或正文副本。

use std::path::Path;
use std::sync::Mutex;

use snap_ocr_core::pipeline::{
    CropRecord, OcrBackend, PipelineError, SMALL_TEXT_SCALE_FACTOR,
    WIDE_TEXT_HORIZONTAL_SCALE_FACTOR,
};
use snap_ocr_core::types::Quad;
use snap_ocr_core::types::TileRegion;

use crate::det_paddlex::DetPaddlex;
use crate::image_ops::{self, BgrImage};
use crate::rec_paddlex::RecPaddlex;

/// 一次 det 调用的记录（`oracle_dump.py` det_calls 条目）。
#[derive(Debug, Clone)]
pub struct DetCallLog {
    /// 瓦片局部坐标多边形（原始输出，未做全局映射）。
    pub polys: Vec<Quad>,
    /// 与 `polys` 等长的检测分。
    pub scores: Vec<f64>,
}

/// 一个 rec 批调用的裁剪指纹。
#[derive(Debug, Clone)]
pub struct CropDigest {
    /// 裁剪宽（像素）。
    pub width: usize,
    /// 裁剪高（像素）。
    pub height: usize,
    /// BGR 行主序字节的 SHA-256（十六进制）。
    pub sha256: String,
}

/// 一次 rec 批调用的记录（`oracle_dump.py` rec_calls 条目）。
#[derive(Debug, Clone)]
pub struct RecCallLog {
    /// 批大小。
    pub batch: usize,
    /// 批内裁剪指纹（输入序）。
    pub crops: Vec<CropDigest>,
    /// 识别文本（与裁剪等长）。
    pub texts: Vec<String>,
    /// 识别分数（与裁剪等长）。
    pub scores: Vec<f64>,
}

/// 真实推理后端：DetPaddlex + RecPaddlex 常驻会话 + 调用记录。
pub struct WorkerOcrBackend {
    det: DetPaddlex,
    rec: RecPaddlex,
    record_calls: bool,
    det_log: Mutex<Vec<DetCallLog>>,
    rec_log: Mutex<Vec<RecCallLog>>,
}

impl WorkerOcrBackend {
    /// 加载检测/识别模型与字典。
    ///
    /// # Errors
    /// 任一模型或字典加载失败时返回 [`PipelineError::Backend`]（消息不含
    /// 截图内容与用户数据，O-29/O-30）。
    pub fn load(
        det_model: &Path,
        rec_model: &Path,
        dict: &Path,
        intra_threads: usize,
    ) -> Result<Self, PipelineError> {
        Self::load_with_logging(det_model, rec_model, dict, intra_threads, true)
    }

    /// 常驻产品服务只保留模型会话，不计算/缓存用于开发对照的裁剪指纹和正文副本。
    pub fn load_for_service(
        det_model: &Path,
        rec_model: &Path,
        dict: &Path,
        intra_threads: usize,
    ) -> Result<Self, PipelineError> {
        Self::load_with_logging(det_model, rec_model, dict, intra_threads, false)
    }

    fn load_with_logging(
        det_model: &Path,
        rec_model: &Path,
        dict: &Path,
        intra_threads: usize,
        record_calls: bool,
    ) -> Result<Self, PipelineError> {
        let det = DetPaddlex::from_path(det_model, intra_threads)
            .map_err(|e| PipelineError::Backend(e.to_string()))?;
        let rec = RecPaddlex::load(rec_model, dict, intra_threads)
            .map_err(|e| PipelineError::Backend(e.to_string()))?;
        Ok(Self {
            det,
            rec,
            record_calls,
            det_log: Mutex::new(Vec::new()),
            rec_log: Mutex::new(Vec::new()),
        })
    }

    /// 取走 det 调用记录（`detect_candidates` 之后调用一次）。
    pub fn take_det_calls(&self) -> Vec<DetCallLog> {
        self.det_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    /// 取走 rec 批调用记录（`recognize_records` 之后调用一次）。
    pub fn take_rec_calls(&self) -> Vec<RecCallLog> {
        self.rec_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }
}

impl OcrBackend for WorkerOcrBackend {
    type Image = BgrImage;

    fn enhance_small_crop(&self, image: &Self::Image) -> Self::Image {
        image_ops::resize_inter_cubic(
            image,
            image.width() * SMALL_TEXT_SCALE_FACTOR as usize,
            image.height() * SMALL_TEXT_SCALE_FACTOR as usize,
        )
    }

    // OpenCV fixture path retains the original usize → u32 narrowing and Python
    // round-to-even float → integer conversion (including Rust's saturating cast).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn stretch_recognition_crop(&self, image: &Self::Image) -> Self::Image {
        // ocr.py:262 用 Python round()（半偶）后取整。
        let width = (f64::from(image.width() as u32) * WIDE_TEXT_HORIZONTAL_SCALE_FACTOR)
            .round_ties_even() as usize;
        image_ops::resize_inter_lanczos4(image, width, image.height())
    }

    fn rotate_90s(&self, image: &Self::Image, degrees: u16) -> Self::Image {
        match degrees {
            0 => image.clone(),
            // 管线只传 additional_rotations 的 90/180/270；rot90 对 1..=3 不可能
            // 失败，防御分支按契约违规原样返回（不产生错误数据流）。
            degrees => {
                image_ops::rot90(image, u32::from(degrees / 90)).unwrap_or_else(|_| image.clone())
            }
        }
    }

    fn recognize(&self, batch: &[Self::Image]) -> Result<Vec<(String, f64)>, PipelineError> {
        let results = self
            .rec
            .recognize_crops(batch)
            .map_err(|e| PipelineError::Backend(e.to_string()))?;
        if self.record_calls {
            let log = RecCallLog {
                batch: batch.len(),
                crops: batch
                    .iter()
                    .map(|crop| CropDigest {
                        width: crop.width(),
                        height: crop.height(),
                        sha256: sha256_hex(crop.data()),
                    })
                    .collect(),
                texts: results.iter().map(|(t, _)| t.clone()).collect(),
                scores: results.iter().map(|(_, s)| *s).collect(),
            };
            self.rec_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(log);
        }
        Ok(results)
    }

    fn tile(&self, image: &Self::Image, tile: &TileRegion) -> Result<Self::Image, PipelineError> {
        let (x, y) = (tile.x() as usize, tile.y() as usize);
        let (width, height) = (tile.width() as usize, tile.height() as usize);
        if x + width > image.width() || y + height > image.height() {
            return Err(PipelineError::Backend("tile out of image bounds".into()));
        }
        let mut data = Vec::with_capacity(width * height * 3);
        for row in y..y + height {
            let base = (row * image.width() + x) * 3;
            data.extend_from_slice(&image.data()[base..base + width * 3]);
        }
        BgrImage::from_vec(width, height, data).map_err(|_| PipelineError::PredictorOutput)
    }

    // The detector interface takes u32 dimensions; retain the original narrowing.
    #[allow(clippy::cast_possible_truncation)]
    fn detect(&self, tile_image: &Self::Image) -> Result<(Vec<Quad>, Vec<f64>), PipelineError> {
        let (polys, scores) = self
            .det
            .detect_tile_detached(
                tile_image.data(),
                tile_image.width() as u32,
                tile_image.height() as u32,
            )
            .map_err(|e| PipelineError::Backend(e.to_string()))?;
        let polys = polys
            .into_iter()
            .map(|poly| {
                [
                    (poly[0][0], poly[0][1]),
                    (poly[1][0], poly[1][1]),
                    (poly[2][0], poly[2][1]),
                    (poly[3][0], poly[3][1]),
                ]
            })
            .collect::<Vec<_>>();
        if self.record_calls {
            self.det_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(DetCallLog {
                    polys: polys.clone(),
                    scores: scores.clone(),
                });
        }
        Ok((polys, scores))
    }
}

/// 为透视裁剪后的候选框组装初始识别记录。
///
/// 增强由 [`snap_ocr_core::pipeline::recognize_records`] 按源高判定并经
/// [`OcrBackend::enhance_small_crop`] 原地替换，此处只装填增强前裁剪。
#[allow(clippy::cast_possible_truncation)] // CropRecord stores dimensions as u32.
#[must_use]
pub fn build_record(
    candidate: snap_ocr_core::types::DetectionCandidate,
    crop: BgrImage,
) -> CropRecord<BgrImage> {
    let source_width = crop.width() as u32;
    let source_height = crop.height() as u32;
    CropRecord {
        candidate,
        crop,
        source_width,
        source_height,
        attempts: Vec::new(),
    }
}

/// 紧凑 SHA-256（FIPS 180-4），为对照输出与验收测试提供裁剪指纹；无外部依赖。
#[must_use]
#[allow(clippy::many_single_char_names)] // SHA-256 names its eight working registers a through h.
#[allow(clippy::expect_used)] // String's fmt::Write implementation is infallible.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for (i, k) in K.iter().enumerate() {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut digest = String::with_capacity(64);
    for word in h {
        use std::fmt::Write as _;
        write!(digest, "{word:08x}").expect("writing to String cannot fail");
    }
    digest
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn sha256_matches_reference_vectors() {
        // FIPS 180-4 / RFC 6234 标准向量。
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn backend_ops_geometry() {
        // 无模型的算子几何：拉伸宽度半偶取整、旋转与瓦片裁剪。
        let img = BgrImage::from_vec(
            3,
            2,
            vec![
                10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160, 170, 180,
            ],
        )
        .unwrap();
        let tile = snap_ocr_core::types::TileRegion::new(0, 1, 0, 2, 2, 3, 2).unwrap();
        // tile() 与 build_record 需要 backend 实例（会话），几何由 image_ops
        // 单测覆盖；此处仅校验 build_record 的字段装填。
        let candidate = snap_ocr_core::types::DetectionCandidate::new(
            [(0.0, 0.0), (2.0, 0.0), (2.0, 1.0), (0.0, 1.0)],
            0.5,
            tile,
            1.0,
            false,
            Vec::new(),
        )
        .unwrap();
        let record = build_record(candidate, img.clone());
        assert_eq!(record.source_width, 3);
        assert_eq!(record.source_height, 2);
        assert!(record.attempts.is_empty());
        assert_eq!(record.crop, img);
    }
}
