use std::sync::atomic::AtomicBool;

use crate::detection::consolidate_candidates;
use crate::tiling::{generate_tiles, internal_edge_metrics, map_quad_to_global};
use crate::types::{DetectionCandidate, Quad};

use super::backend::{OcrBackend, PipelineError};
use super::cancelled;

/// 瓦片检测编排（对应 `ocr.py:628-692` 的 `_detect` + `_predict_detection`）。
///
/// 流程：[`generate_tiles`] 生成瓦片 → 每瓦片先 [`OcrBackend::tile`] 裁剪、
/// 再对裁剪图调用 [`OcrBackend::detect`]（batch=1）→ 四边形夹取到瓦片范围 →
/// [`map_quad_to_global`] 映射回全局 → [`internal_edge_metrics`] 内部边缘
/// 度量 → [`DetectionCandidate`] → 全部瓦片完成后
/// [`consolidate_candidates`]（先接缝合并后去重）。
///
/// 取消检查点与 Python 一致：每个瓦片检测前与候选收集后各检查一次。
///
/// # Errors
/// 后端错误透传；输出违反契约返回 [`PipelineError::PredictorOutput`]；
/// 用户取消返回 [`PipelineError::Cancelled`]。
pub fn detect_candidates<B: OcrBackend>(
    backend: &B,
    image: &B::Image,
    image_width: u32,
    image_height: u32,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<DetectionCandidate>, PipelineError> {
    let mut candidates: Vec<DetectionCandidate> = Vec::new();
    for tile in generate_tiles(image_width, image_height)? {
        if cancelled(cancel) {
            return Err(PipelineError::Cancelled);
        }
        let tile_image = backend.tile(image, &tile)?;
        let (quads, scores) = backend.detect(&tile_image)?;
        if quads.len() != scores.len() {
            return Err(PipelineError::PredictorOutput);
        }
        for (quad, score) in quads.iter().zip(scores) {
            // 范围判定基于偏序比较：NaN 与 ±∞ 一律不落在 0.0..=1.0 内，
            // 无需前置 is_finite。
            if !(0.0..=1.0).contains(&score) {
                return Err(PipelineError::PredictorOutput);
            }
            for (x, y) in quad {
                if !x.is_finite() || !y.is_finite() {
                    return Err(PipelineError::PredictorOutput);
                }
            }
            let mut local: Quad = *quad;
            for (x, y) in &mut local {
                *x = x.clamp(0.0, f64::from(tile.width()));
                *y = y.clamp(0.0, f64::from(tile.height()));
            }
            let global = map_quad_to_global(&local, &tile);
            let metrics = internal_edge_metrics(&global, &tile);
            candidates.push(DetectionCandidate::new(
                global,
                score,
                tile,
                metrics.distance,
                metrics.touches,
                Vec::new(),
            )?);
        }
        if cancelled(cancel) {
            return Err(PipelineError::Cancelled);
        }
    }
    Ok(consolidate_candidates(&candidates))
}
