use std::sync::atomic::AtomicBool;

use crate::orientation::{additional_rotations, select_best, RecognitionAttempt};
use crate::types::RecognizedSpan;

use super::backend::{CropRecord, OcrBackend, PipelineError};
use super::cancelled;
use super::checks::looks_like_dense_code_outline;
use super::{
    CODE_STRETCH_SCORE_TOLERANCE, RECOGNITION_BATCH_SIZE, SMALL_TEXT_CROP_HEIGHT,
    SMALL_TEXT_SCALE_FACTOR, WIDE_TEXT_MAX_HEIGHT, WIDE_TEXT_MIN_ASPECT_RATIO,
};

/// 单个不可中断识别批：调用后端并按 `_predict_recognition`（`ocr.py:813-819`）
/// 复核输出长度与分数范围。
fn predict_recognition<B: OcrBackend>(
    backend: &B,
    images: &[B::Image],
) -> Result<Vec<(String, f64)>, PipelineError> {
    let outputs = backend.recognize(images)?;
    if outputs.len() != images.len() {
        return Err(PipelineError::PredictorOutput);
    }
    for (_, score) in &outputs {
        // 范围判定基于偏序比较：NaN 与 ±∞ 一律不落在 0.0..=1.0 内。
        if !(0.0..=1.0).contains(score) {
            return Err(PipelineError::PredictorOutput);
        }
    }
    Ok(outputs)
}

/// 识别期尺寸：源高 < [`SMALL_TEXT_CROP_HEIGHT`] 时为 2× 增强后尺寸，
/// 否则等于源尺寸（`ocr.py:530-538` 的 `record.width`/`record.height`）。
fn recognition_dimensions(source_width: u32, source_height: u32) -> (u32, u32) {
    if source_height < SMALL_TEXT_CROP_HEIGHT {
        (
            source_width.saturating_mul(SMALL_TEXT_SCALE_FACTOR),
            source_height.saturating_mul(SMALL_TEXT_SCALE_FACTOR),
        )
    } else {
        (source_width, source_height)
    }
}

/// 初始识别：按 [`RECOGNITION_BATCH_SIZE`] 分批 flush（`ocr.py:699-712`）。
/// 批前与追加尝试后各有一个取消检查点。
fn recognize_initial<B: OcrBackend>(
    backend: &B,
    records: &mut [CropRecord<B::Image>],
    cancel: Option<&AtomicBool>,
) -> Result<(), PipelineError> {
    for start in (0..records.len()).step_by(RECOGNITION_BATCH_SIZE) {
        if cancelled(cancel) {
            return Err(PipelineError::Cancelled);
        }
        let end = (start + RECOGNITION_BATCH_SIZE).min(records.len());
        // 批次裁剪交给后端需要一次性借用连续切片，`Image: Clone` 即为此设。
        let images: Vec<B::Image> = records[start..end]
            .iter()
            .map(|record| record.crop.clone())
            .collect();
        let attempts = predict_recognition(backend, &images)?;
        for (record, (text, score)) in records[start..end].iter_mut().zip(attempts) {
            record.attempts.push(RecognitionAttempt {
                text,
                score,
                rotation_degrees: 0,
            });
        }
        if cancelled(cancel) {
            return Err(PipelineError::Cancelled);
        }
    }
    Ok(())
}

/// 密集代码批 flush（`ocr.py:722-743`）：pending 为空时直接成功（**不**检查
/// 取消）；否则检查取消 → 识别 → 逐条按替换规则回写 → 尾部再查一次取消。
/// `entries` 与 `images` 是并行数组（记录下标与拉伸后的裁剪）。
fn flush_dense_code_retries<B: OcrBackend>(
    backend: &B,
    entries: &mut Vec<usize>,
    images: &mut Vec<B::Image>,
    records: &mut [CropRecord<B::Image>],
    cancel: Option<&AtomicBool>,
) -> Result<bool, PipelineError> {
    if entries.is_empty() {
        return Ok(true);
    }
    if cancelled(cancel) {
        return Ok(false);
    }
    let attempts = predict_recognition(backend, images)?;
    if attempts.len() != entries.len() {
        return Err(PipelineError::PredictorOutput);
    }
    for (index, (text, score)) in entries.iter().zip(attempts) {
        let record = &mut records[*index];
        let initial = &record.attempts[0];
        let initial_score = initial.score;
        let initial_length = initial.text.chars().count();
        let text_length = text.chars().count();
        // 替换规则（ocr.py:746-762）：新文本非空、分数 ≥ 初分 − 0.02，
        // 且（更长，或同长且分数更高）时替换 0° 尝试。
        if !text.is_empty()
            && score >= initial_score - CODE_STRETCH_SCORE_TOLERANCE
            && (text_length > initial_length
                || (text_length == initial_length && score > initial_score))
        {
            record.attempts[0] = RecognitionAttempt {
                text,
                score,
                rotation_degrees: 0,
            };
        }
    }
    entries.clear();
    images.clear();
    Ok(!cancelled(cancel))
}

/// 密集代码横向拉伸重试（`ocr.py:714-764`）。
///
/// 触发条件（全部基于增强前源尺寸与首试文本）：源高 ∈
/// [[`SMALL_TEXT_CROP_HEIGHT`], [`WIDE_TEXT_MAX_HEIGHT`]]、源宽/源高 ≥
/// [`WIDE_TEXT_MIN_ASPECT_RATIO`]，且 [`looks_like_dense_code_outline`] 成立。
/// 待重试项攒满一批即 flush，收尾再 flush 一次。
fn recognize_dense_code_retries<B: OcrBackend>(
    backend: &B,
    records: &mut [CropRecord<B::Image>],
    cancel: Option<&AtomicBool>,
) -> Result<(), PipelineError> {
    let mut entries: Vec<usize> = Vec::new();
    let mut images: Vec<B::Image> = Vec::new();
    for index in 0..records.len() {
        // 先判定并固化本记录的拉伸任务（owned），随后不再持有 records 借用，
        // 让同批次内的 flush 可以可变访问 records。
        let stretched = {
            let record = &records[index];
            let initial = &record.attempts[0];
            let dense = record.source_height >= SMALL_TEXT_CROP_HEIGHT
                && record.source_height <= WIDE_TEXT_MAX_HEIGHT
                && f64::from(record.source_width) / f64::from(record.source_height)
                    >= WIDE_TEXT_MIN_ASPECT_RATIO
                && looks_like_dense_code_outline(&initial.text);
            dense.then(|| backend.stretch_recognition_crop(&record.crop))
        };
        if let Some(stretched) = stretched {
            entries.push(index);
            images.push(stretched);
            if entries.len() == RECOGNITION_BATCH_SIZE
                && !flush_dense_code_retries(backend, &mut entries, &mut images, records, cancel)?
            {
                return Err(PipelineError::Cancelled);
            }
        }
    }
    let completed = flush_dense_code_retries(backend, &mut entries, &mut images, records, cancel)?;
    if !completed {
        return Err(PipelineError::Cancelled);
    }
    Ok(())
}

/// 方向重试批 flush（`ocr.py:768-790`）：**先**检查取消（pending 为空也要
/// 检查），再判空短路；识别后追加尝试，尾部再查一次取消。
/// `entries` 与 `images` 是并行数组（记录下标 + 旋转角，与旋转后的裁剪）。
fn flush_rotation_retries<B: OcrBackend>(
    backend: &B,
    entries: &mut Vec<(usize, u16)>,
    images: &mut Vec<B::Image>,
    records: &mut [CropRecord<B::Image>],
    cancel: Option<&AtomicBool>,
) -> Result<bool, PipelineError> {
    if cancelled(cancel) {
        return Ok(false);
    }
    if entries.is_empty() {
        return Ok(true);
    }
    let attempts = predict_recognition(backend, images)?;
    if attempts.len() != entries.len() {
        return Err(PipelineError::PredictorOutput);
    }
    for ((index, rotation), (text, score)) in entries.iter().zip(attempts) {
        records[*index].attempts.push(RecognitionAttempt {
            text,
            score,
            rotation_degrees: *rotation,
        });
    }
    entries.clear();
    images.clear();
    Ok(!cancelled(cancel))
}

/// 方向重试（`ocr.py:766-800`）：按记录当前（增强后）宽高与首试分调用
/// [`additional_rotations`]，旋转图攒满一批即 flush，收尾再 flush 一次。
/// 密集代码替换发生在首试 `attempts[0]` 上，因此这里读到的首试分已包含
/// 替换结果（附录 B：替换后分数参与 180° 加试判据）。
fn recognize_rotations<B: OcrBackend>(
    backend: &B,
    records: &mut [CropRecord<B::Image>],
    cancel: Option<&AtomicBool>,
) -> Result<(), PipelineError> {
    let mut entries: Vec<(usize, u16)> = Vec::new();
    let mut images: Vec<B::Image> = Vec::new();
    for index in 0..records.len() {
        // 先固化本记录的全部旋转任务（owned），随后不再持有 records 借用，
        // 让同批次内的 flush 可以可变访问 records。
        let mut jobs: Vec<(u16, B::Image)> = {
            let record = &records[index];
            let initial_score = record.attempts[0].score;
            let (crop_width, crop_height) =
                recognition_dimensions(record.source_width, record.source_height);
            // 密集代码替换发生在首试 attempts[0] 上，因此这里的首试分已包含
            // 替换结果（附录 B：替换后分数参与 180° 加试判据）。
            additional_rotations(f64::from(crop_height), f64::from(crop_width), initial_score)
                .into_iter()
                .map(|rotation| {
                    let rotated = backend.rotate_90s(&record.crop, rotation);
                    (rotation, rotated)
                })
                .collect()
        };
        for (rotation, rotated) in jobs.drain(..) {
            entries.push((index, rotation));
            images.push(rotated);
            if entries.len() == RECOGNITION_BATCH_SIZE
                && !flush_rotation_retries(backend, &mut entries, &mut images, records, cancel)?
            {
                return Err(PipelineError::Cancelled);
            }
        }
    }
    let completed = flush_rotation_retries(backend, &mut entries, &mut images, records, cancel)?;
    if !completed {
        return Err(PipelineError::Cancelled);
    }
    Ok(())
}

/// 对全部记录执行识别编排（对应 `ocr.py` 的三个 `_recognize_*` 阶段）。
///
/// 顺序与 Python 一致：小裁剪增强（源高 < [`SMALL_TEXT_CROP_HEIGHT`]，在送
/// 识别前原地替换裁剪）→ 初始批 8 flush → 密集代码拉伸重试 → 方向重试。
/// 完成后每条记录的 `attempts` 至少含一个 0° 尝试；最终文本用
/// [`assemble_spans`] 定稿。
///
/// # Errors
/// 后端错误/输出违规透传为 [`PipelineError`]；任一取消检查点命中时返回
/// [`PipelineError::Cancelled`]（用户取消不是故障，由上层映射）。
pub fn recognize_records<B: OcrBackend>(
    backend: &B,
    records: &mut [CropRecord<B::Image>],
    cancel: Option<&AtomicBool>,
) -> Result<(), PipelineError> {
    for record in records.iter_mut() {
        if record.source_height < SMALL_TEXT_CROP_HEIGHT {
            record.crop = backend.enhance_small_crop(&record.crop);
        }
    }
    recognize_initial(backend, records, cancel)?;
    recognize_dense_code_retries(backend, records, cancel)?;
    recognize_rotations(backend, records, cancel)?;
    Ok(())
}

/// span 组装（`ocr.py:558-584`）：每条记录用 [`select_best`] 定稿，只丢弃
/// 恰为空串的文本；全部为空时若已取消，取消优先于「无文字」结果。
/// 不调用后端，因此只按记录的图像类型泛化。
///
/// # Errors
/// span 数据边界校验失败返回 [`PipelineError::Domain`]；span 全空且用户已
/// 取消时返回 [`PipelineError::Cancelled`]。
pub fn assemble_spans<I>(
    records: &[CropRecord<I>],
    cancel: Option<&AtomicBool>,
) -> Result<Vec<RecognizedSpan>, PipelineError> {
    let mut spans: Vec<RecognizedSpan> = Vec::new();
    for record in records {
        let attempt = select_best(&record.attempts);
        if attempt.text.is_empty() {
            continue;
        }
        spans.push(RecognizedSpan::new(
            *record.candidate.quad(),
            attempt.text.clone(),
            record.candidate.detection_score(),
            attempt.score,
            attempt.rotation_degrees,
        )?);
    }
    if spans.is_empty() {
        if cancelled(cancel) {
            return Err(PipelineError::Cancelled);
        }
        return Ok(Vec::new());
    }
    Ok(spans)
}
