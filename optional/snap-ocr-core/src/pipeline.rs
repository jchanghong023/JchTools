//! 识别编排（对应冻结仓库 `src/textsnap/ocr.py` 的可注入编排段）。
//!
//! 从 `ocr.py` 精读移植的编排语义：初始批 8 张 flush、小裁剪 2× 增强、
//! 密集代码 1.5× 横向拉伸重试（含 0.02 分数容差与替换规则）、方向重试、
//! 瓦片检测（夹取→全局映射→内部边缘度量→先合并后去重）、span 只丢空串，
//! 以及每个 flush 前后的用户取消检查点。真实推理与图像操作经
//! [`OcrBackend`] 注入，本模块保持纯逻辑、可离线构造测试。
//!
//! 取消语义：[`PipelineError::Cancelled`] 表达用户取消（O-19：取消在当前
//! 不可中断推理调用返回后的批次/瓦片检查点生效，不算故障），由上层映射为
//! 「已取消」结果而不是错误提示。

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

#[cfg(test)]
use crate::types::{DetectionCandidate, Quad, RecognizedSpan};

mod backend;
mod checks;
mod detect;
mod recognize;

#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod tests;

pub use backend::{CropRecord, OcrBackend, PipelineError};
pub use checks::{looks_like_dense_code_outline, validate_perspective};
pub use detect::detect_candidates;
pub use recognize::{assemble_spans, recognize_records};

/// 识别批次固定大小（O-26：batch 固定为 8）。
pub const RECOGNITION_BATCH_SIZE: usize = 8;

/// 小裁剪高度阈值：源高低于该值时在送识别前做 2× 等比增强（附录 B）。
pub const SMALL_TEXT_CROP_HEIGHT: u32 = 20;

/// 小裁剪增强的等比放大倍数（INTER_CUBIC，由后端实现执行）。
pub const SMALL_TEXT_SCALE_FACTOR: u32 = 2;

/// 密集代码拉伸重试的源高上限（与 [`SMALL_TEXT_CROP_HEIGHT`] 构成闭区间）。
pub const WIDE_TEXT_MAX_HEIGHT: u32 = 40;

/// 密集代码拉伸重试的最小宽高比（源宽/源高 ≥ 12.0）。
pub const WIDE_TEXT_MIN_ASPECT_RATIO: f64 = 12.0;

/// 密集代码拉伸的横向放大倍数（INTER_LANCZOS4，高度不变）。
pub const WIDE_TEXT_HORIZONTAL_SCALE_FACTOR: f64 = 1.5;

/// 密集代码重试替换初次结果所允许的分数容差（新分 ≥ 初分 − 0.02）。
pub const CODE_STRETCH_SCORE_TOLERANCE: f64 = 0.02;

/// 是否已收到用户取消。
fn cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(AtomicOrdering::SeqCst))
}
