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

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use crate::detection::consolidate_candidates;
use crate::orientation::{additional_rotations, select_best, RecognitionAttempt};
use crate::tiling::{generate_tiles, internal_edge_metrics, map_quad_to_global};
use crate::types::{DetectionCandidate, DomainError, Quad, RecognizedSpan, TileRegion};

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

/// 注入的推理与图像操作后端。
///
/// 所有图像处理（增强、拉伸、旋转）与真实推理由实现方完成；管线只按
/// Python 的触发条件调用它们，不检查像素。`Image` 是不透明句柄，测试中
/// 可用带标签的假图像模拟。`Clone` 约束用于把批次裁剪交给后端。
///
/// # 契约
/// - [`OcrBackend::enhance_small_crop`]：2× INTER_CUBIC 等比放大（宽高各乘
///   [`SMALL_TEXT_SCALE_FACTOR`]）。是否调用由管线按源高 < [`SMALL_TEXT_CROP_HEIGHT`]
///   判定，实现不得再检查高度。
/// - [`OcrBackend::stretch_recognition_crop`]：宽度 × [`WIDE_TEXT_HORIZONTAL_SCALE_FACTOR`]
///   （四舍五入取整）、高度不变，INTER_LANCZOS4。
/// - [`OcrBackend::rotate_90s`]：只接受 0/90/180/270；其他角度是实现方契约
///   违规（管线只传 [`additional_rotations`] 的输出，均为 90/180/270）。
/// - [`OcrBackend::recognize`]：返回长度必须等于 `batch.len()`，且每个分数
///   有限并处于 [0, 1]；违规时实现应返回 `Err`（管线也会复核并返回
///   [`PipelineError::PredictorOutput`]）。
/// - [`OcrBackend::detect`]：返回的四边形与分数列表长度必须一致；每个四边形
///   为**当前瓦片局部坐标**的四个顶点（顶点数由 `Quad` 类型保证），坐标必须
///   有限，分数必须有限且处于 [0, 1]；违规时实现应返回 `Err`。管线对同一
///   图像按 [`generate_tiles`] 的生成顺序每瓦片调用一次（batch=1），第 k 次
///   调用对应第 k 个瓦片；真实推理适配层负责按调用次序提供对应瓦片视图。
/// - 所有 `Err` 携带的消息不得包含截图内容、识别正文或用户路径（O-29/O-30）。
pub trait OcrBackend {
    /// 后端持有的图像句柄（检测输入、裁剪与变体的统一类型）。
    type Image: Clone;

    /// 小裁剪增强：2× INTER_CUBIC 等比放大。
    fn enhance_small_crop(&self, image: &Self::Image) -> Self::Image;

    /// 密集代码横向拉伸：宽 × 1.5 INTER_LANCZOS4，高度不变。
    fn stretch_recognition_crop(&self, image: &Self::Image) -> Self::Image;

    /// 旋转 90° 的整数倍（仅 0/90/180/270）。
    fn rotate_90s(&self, image: &Self::Image, degrees: u16) -> Self::Image;

    /// 批量识别一批裁剪，返回与批次等长的 (文本, 分数) 列表。
    ///
    /// # Errors
    /// 推理失败或输出违反契约时返回 [`PipelineError`]。
    fn recognize(&self, batch: &[Self::Image]) -> Result<Vec<(String, f64)>, PipelineError>;

    /// 从整张选区图裁出一个瓦片（对应 Python `ImageBackend.tile`）；实现必须
    /// 无状态、只依赖入参，不得以调用次数隐含瓦片。
    ///
    /// # Errors
    /// 裁剪失败（瓦片越界等）返回 [`PipelineError`]。
    fn tile(&self, image: &Self::Image, tile: &TileRegion) -> Result<Self::Image, PipelineError>;

    /// 对单个瓦片裁剪图做检测，返回**瓦片局部**四边形列表与等长分数列表。
    ///
    /// # Errors
    /// 推理失败或输出违反契约时返回 [`PipelineError`]。
    fn detect(&self, tile_image: &Self::Image) -> Result<(Vec<Quad>, Vec<f64>), PipelineError>;
}

/// 一个候选框的识别记录（对应 Python `_CropRecord`）。
///
/// `crop` 与 `source_width`/`source_height` 均为**增强前**的裁剪与其尺寸；
/// 识别期尺寸（参与方向判据，对应 Python `record.width`/`record.height`）
/// 由管线按增强规则推导：源高 < [`SMALL_TEXT_CROP_HEIGHT`] 时宽高各乘
/// [`SMALL_TEXT_SCALE_FACTOR`]，否则保持不变（`ocr.py:530-538`）。
pub struct CropRecord<I> {
    /// 该裁剪来源的检测候选（span 组装时提供全局四边形与检测分）。
    pub candidate: DetectionCandidate,
    /// 识别用裁剪图（管线可能在小裁剪增强时原地替换为增强后的图像）。
    pub crop: I,
    /// 增强前的源宽（像素）。
    pub source_width: u32,
    /// 增强前的源高（像素）。
    pub source_height: u32,
    /// 识别尝试列表；[`recognize_records`] 结束后每条记录至少含 0° 尝试。
    pub attempts: Vec<RecognitionAttempt>,
}

/// 编排层错误（对应 Python 的校验失败、推理失败与用户取消）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    /// 检测四边形非法：坐标非有限或四边形非凸（`ocr.py` 透视校验）。
    InvalidDetectionQuad,
    /// 检测四边形退化：顶点重复、有向面积为零或目标宽/高不足 2 像素。
    DegenerateDetectionQuad,
    /// 推理后端输出未通过校验（数量不匹配、坐标非有限或分数越界）。
    PredictorOutput,
    /// 数据边界校验失败（复用 crate 的 [`DomainError`]）。
    Domain(DomainError),
    /// 注入的后端报告错误；消息由实现方保证不含截图内容与用户路径。
    Backend(String),
    /// 用户已取消（O-19：在批次/瓦片检查点生效，不是故障）。
    Cancelled,
    /// 瓦片参数非法（尺寸为零或重叠越界；输入已由调用方校验时不可达）。
    Tiling(crate::tiling::TilingError),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineError::InvalidDetectionQuad => {
                write!(f, "检测四边形非法：坐标必须有限且四边形必须为凸")
            }
            PipelineError::DegenerateDetectionQuad => {
                write!(f, "检测四边形退化：顶点重复、面积为零或目标宽高不足 2 像素")
            }
            PipelineError::PredictorOutput => write!(f, "推理后端输出未通过校验"),
            PipelineError::Domain(error) => write!(f, "数据边界校验失败：{error}"),
            PipelineError::Backend(message) => write!(f, "推理后端错误：{message}"),
            PipelineError::Cancelled => write!(f, "用户已取消"),
            PipelineError::Tiling(error) => write!(f, "瓦片参数非法：{error}"),
        }
    }
}

impl std::error::Error for PipelineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PipelineError::Domain(error) => Some(error),
            PipelineError::Tiling(error) => Some(error),
            PipelineError::InvalidDetectionQuad
            | PipelineError::DegenerateDetectionQuad
            | PipelineError::PredictorOutput
            | PipelineError::Backend(_)
            | PipelineError::Cancelled => None,
        }
    }
}

impl From<DomainError> for PipelineError {
    fn from(error: DomainError) -> Self {
        PipelineError::Domain(error)
    }
}

impl From<crate::tiling::TilingError> for PipelineError {
    fn from(error: crate::tiling::TilingError) -> Self {
        PipelineError::Tiling(error)
    }
}

/// 是否已收到用户取消。
fn cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(AtomicOrdering::SeqCst))
}

/// 判断文本是否为「分隔符密集的代码轮廓行」（`ocr.py:295-304`）。
///
/// 只用于决定是否做横向拉伸重试，不修正文本本身：长度 ≥ 40、竖线 ≥ 3，
/// 且含 `::` 或下划线 ≥ 2。
pub fn looks_like_dense_code_outline(text: &str) -> bool {
    text.chars().count() >= 40
        && text.matches('|').count() >= 3
        && (text.contains("::") || text.matches('_').count() >= 2)
}

/// 透视裁剪的纯逻辑校验（`ocr.py:168-230` 中与图像无关的部分）。
///
/// 依次校验：四个顶点坐标有限；顶点两两互异；相邻边叉积全部同号（凸）；
/// shoelace 有向面积非零。目标宽取上/下边较长者、高取左/右边较长者，
/// 按附录 B「取整」；任一边 < 2 像素判退化。取整为 Python `round` 的半偶
/// （银行家）舍入（`round_ties_even`），与 ocr.py:209-210 逐位一致。
///
/// # Errors
/// 校验失败返回 [`PipelineError::InvalidDetectionQuad`] 或
/// [`PipelineError::DegenerateDetectionQuad`]。
pub fn validate_perspective(quad: &Quad) -> Result<(f64, f64), PipelineError> {
    // ocr.py:170 先 asarray(dtype=float32)：顶点先舍入到 f32，后续互异/凸性/
    // 面积/边长都在舍入后的值上进行（numpy.linalg.norm 在 f32 数组上以 f32
    // 求值后 float() 转 f64）。坐标已在 DetectionCandidate 校验中保证有限，
    // 截断即 oracle 的语义（asarray 舍入）。
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let points_f32: [(f32, f32); 4] = [
        (quad[0].0 as f32, quad[0].1 as f32),
        (quad[1].0 as f32, quad[1].1 as f32),
        (quad[2].0 as f32, quad[2].1 as f32),
        (quad[3].0 as f32, quad[3].1 as f32),
    ];
    for (x, y) in &points_f32 {
        if !x.is_finite() || !y.is_finite() {
            return Err(PipelineError::InvalidDetectionQuad);
        }
    }
    // 互异：f32 值域上的唯一性（等价 numpy.unique(axis=0)）。
    for i in 0..4 {
        for j in (i + 1)..4 {
            if points_f32[i] == points_f32[j] {
                return Err(PipelineError::DegenerateDetectionQuad);
            }
        }
    }
    // 凸性：每条边与下一条边的叉积必须全部严格同号（ocr.py:177-187）；
    // 验证值 = f32 舍入后回到 f64（astype(float64)）。
    let points: [(f64, f64); 4] = points_f32.map(|(x, y)| (f64::from(x), f64::from(y)));
    let mut crosses = [0.0_f64; 4];
    for (index, cross) in crosses.iter_mut().enumerate() {
        let p0 = points[index];
        let p1 = points[(index + 1) % 4];
        let p2 = points[(index + 2) % 4];
        let e0 = (p1.0 - p0.0, p1.1 - p0.1);
        let e1 = (p2.0 - p1.0, p2.1 - p1.1);
        *cross = e0.0 * e1.1 - e0.1 * e1.0;
    }
    let all_positive = crosses.iter().all(|cross| *cross > 0.0);
    let all_negative = crosses.iter().all(|cross| *cross < 0.0);
    if !(all_positive || all_negative) {
        return Err(PipelineError::InvalidDetectionQuad);
    }
    // 与 Python 一致：shoelace 有向面积恰为零才判退化（有意精确比较）。
    #[allow(clippy::float_cmp)]
    if (0..4)
        .map(|index| {
            let p0 = points[index];
            let p1 = points[(index + 1) % 4];
            p0.0 * p1.1 - p0.1 * p1.0
        })
        .sum::<f64>()
        == 0.0
    {
        return Err(PipelineError::DegenerateDetectionQuad);
    }
    // 边长在 f32 值上以 f32 算术求范数后转 f64（numpy.linalg.norm 语义）。
    #[allow(clippy::cast_possible_truncation)]
    fn distance_f32(a: (f64, f64), b: (f64, f64)) -> f64 {
        let dx = (a.0 as f32) - (b.0 as f32);
        let dy = (a.1 as f32) - (b.1 as f32);
        f64::from((dx * dx + dy * dy).sqrt())
    }
    // ocr.py:209-210 用 Python round()（半偶/银行家舍入）后取整；round_ties_even
    // 与之逐位一致，不用 floor(x+0.5)（半上取整会在 x.5 处偏离 oracle）。
    let output_width = f64::max(
        distance_f32(points[0], points[1]),
        distance_f32(points[3], points[2]),
    )
    .round_ties_even();
    let output_height = f64::max(
        distance_f32(points[0], points[3]),
        distance_f32(points[1], points[2]),
    )
    .round_ties_even();
    if output_width < 2.0 || output_height < 2.0 {
        return Err(PipelineError::DegenerateDetectionQuad);
    }
    Ok((output_width, output_height))
}

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
    // 覆盖 O-26（密集代码/批次/取消）与 O-24（tiles_global）。
    // 翻译冻结仓库 tests/test_ocr.py 的纯逻辑子集，用例名保持对应；
    // 假后端语义仿照 Python FakePredictor/FakeImageBackend。
    // 测试内允许 unwrap/expect；分数均为字面量构造，允许精确比较。
    // 回归（覆盖附录 B 透视裁剪）：斜边凸四边形必须通过校验。
    // 修复前凸性第二条边 y 分量笔误（p2.1-p1.0）使本用例返回 Invalid。
    #[test]
    fn perspective_accepts_convex_quad_with_slanted_edges() {
        let quad = [(0.0, 0.0), (10.0, -2.0), (12.0, 0.0), (0.0, 4.0)];
        let (width, height) = validate_perspective(&quad).expect("斜边凸四边形应通过");
        assert!((width - 13.0).abs() < 1e-9, "width={width}");
        assert!((height - 4.0).abs() < 1e-9, "height={height}");
    }

    // 回归：凹四边形必须被拒绝（任一叉积变号）。
    #[test]
    fn perspective_rejects_concave_quad() {
        let quad = [(0.0, 0.0), (10.0, 0.0), (5.0, 1.0), (0.0, 10.0)];
        assert!(matches!(
            validate_perspective(&quad),
            Err(PipelineError::InvalidDetectionQuad)
        ));
    }

    // 回归：顶点重复（f32 值域）判退化；近零面积同样拒绝。
    #[test]
    fn perspective_rejects_duplicate_and_degenerate_quads() {
        let duplicated = [(0.0, 0.0), (10.0, 0.0), (10.0, 8.0), (0.0, 0.0)];
        assert!(matches!(
            validate_perspective(&duplicated),
            Err(PipelineError::DegenerateDetectionQuad)
        ));
        let tiny = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        assert!(matches!(
            validate_perspective(&tiny),
            Err(PipelineError::DegenerateDetectionQuad)
        ));
    }

    use super::*;
    use crate::types::TileRegion;
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;

    /// 带结构标签的假图像，模拟 Python 测试里的 `image.tag`。
    #[derive(Debug, Clone, PartialEq)]
    enum FakeImage {
        /// 检测输入（整张截图句柄）。
        Canvas,
        /// 瓦片裁剪（携带瓦片索引，检测脚本按索引取结果）。
        Tile { index: usize },
        /// 识别裁剪：记录左上 x 与源高，供识别脚本区分记录。
        Crop { left: f64, height: u32 },
        /// 小裁剪 2× 增强结果。
        Enhanced(Box<FakeImage>),
        /// 密集代码 1.5× 横向拉伸结果。
        Stretched(Box<FakeImage>),
        /// 旋转结果。
        Rotated { degrees: u16, inner: Box<FakeImage> },
    }

    /// 检测脚本类型（按调用次序给出结果）。
    type DetectScript = Box<dyn Fn(usize) -> Result<(Vec<Quad>, Vec<f64>), PipelineError>>;
    /// 识别脚本类型（按批次内容给出结果）。
    type RecognizeScript = Box<dyn Fn(&[FakeImage]) -> Result<Vec<(String, f64)>, PipelineError>>;

    /// 假后端：检测按调用次序取脚本结果；识别按批次内容取脚本结果。
    struct FakeBackend {
        detect_script: DetectScript,
        recognize_script: RecognizeScript,
        detect_calls: Cell<usize>,
        recognize_calls: RefCell<Vec<Vec<String>>>,
        rotations_called: RefCell<Vec<u16>>,
        enhanced_calls: Cell<usize>,
        stretched_calls: Cell<usize>,
    }

    fn label(image: &FakeImage) -> String {
        match image {
            FakeImage::Canvas => "canvas".to_string(),
            FakeImage::Tile { index } => format!("tile({index})"),
            FakeImage::Crop { left, height } => format!("crop({left},{height})"),
            FakeImage::Enhanced(inner) => format!("enh({})", label(inner)),
            FakeImage::Stretched(inner) => format!("stretch({})", label(inner)),
            FakeImage::Rotated { degrees, inner } => {
                format!("rot({degrees},{})", label(inner))
            }
        }
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                detect_script: Box::new(|_| Ok((Vec::new(), Vec::new()))),
                recognize_script: Box::new(|batch| {
                    Ok(batch.iter().map(|_| ("text".to_string(), 0.9)).collect())
                }),
                detect_calls: Cell::new(0),
                recognize_calls: RefCell::new(Vec::new()),
                rotations_called: RefCell::new(Vec::new()),
                enhanced_calls: Cell::new(0),
                stretched_calls: Cell::new(0),
            }
        }

        fn with_detect(
            mut self,
            script: impl Fn(usize) -> Result<(Vec<Quad>, Vec<f64>), PipelineError> + 'static,
        ) -> Self {
            self.detect_script = Box::new(script);
            self
        }

        fn with_recognize(
            mut self,
            script: impl Fn(&[FakeImage]) -> Result<Vec<(String, f64)>, PipelineError> + 'static,
        ) -> Self {
            self.recognize_script = Box::new(script);
            self
        }

        fn detect_calls(&self) -> usize {
            self.detect_calls.get()
        }

        fn enhanced_calls(&self) -> usize {
            self.enhanced_calls.get()
        }

        fn stretched_calls(&self) -> usize {
            self.stretched_calls.get()
        }

        fn recognize_batches(&self) -> Vec<Vec<String>> {
            self.recognize_calls.borrow().clone()
        }

        fn rotations_called(&self) -> Vec<u16> {
            self.rotations_called.borrow().clone()
        }
    }

    impl OcrBackend for FakeBackend {
        type Image = FakeImage;

        fn enhance_small_crop(&self, image: &Self::Image) -> Self::Image {
            self.enhanced_calls.set(self.enhanced_calls.get() + 1);
            FakeImage::Enhanced(Box::new(image.clone()))
        }

        fn stretch_recognition_crop(&self, image: &Self::Image) -> Self::Image {
            self.stretched_calls.set(self.stretched_calls.get() + 1);
            FakeImage::Stretched(Box::new(image.clone()))
        }

        fn rotate_90s(&self, image: &Self::Image, degrees: u16) -> Self::Image {
            self.rotations_called.borrow_mut().push(degrees);
            FakeImage::Rotated {
                degrees,
                inner: Box::new(image.clone()),
            }
        }

        fn recognize(&self, batch: &[Self::Image]) -> Result<Vec<(String, f64)>, PipelineError> {
            self.recognize_calls
                .borrow_mut()
                .push(batch.iter().map(label).collect());
            (self.recognize_script)(batch)
        }

        fn tile(
            &self,
            _image: &Self::Image,
            tile: &TileRegion,
        ) -> Result<Self::Image, PipelineError> {
            Ok(FakeImage::Tile {
                index: tile.index(),
            })
        }

        fn detect(&self, tile_image: &Self::Image) -> Result<(Vec<Quad>, Vec<f64>), PipelineError> {
            let FakeImage::Tile { index } = tile_image else {
                return Err(PipelineError::Backend("detect 只接受瓦片裁剪图".into()));
            };
            let call = self.detect_calls.get();
            self.detect_calls.set(call + 1);
            (self.detect_script)(*index)
        }
    }

    /// 对应 Python 测试的 `_quad(left, top, right, bottom)`。
    fn quad(left: f64, top: f64, right: f64, bottom: f64) -> Quad {
        [(left, top), (right, top), (right, bottom), (left, bottom)]
    }

    // 特征测试：非有限分数（NaN/±∞ 不落在 0.0..=1.0 的偏序范围内）必须报
    // PredictorOutput，不得变成其他错误变体或被放行。
    #[test]
    fn detect_rejects_non_finite_score_as_predictor_output() {
        for score in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let backend = FakeBackend::new()
                .with_detect(move |_| Ok((vec![quad(10.0, 10.0, 30.0, 30.0)], vec![score])));
            let outcome = detect_candidates(&backend, &FakeImage::Canvas, 2500, 400, None);
            assert!(
                matches!(outcome, Err(PipelineError::PredictorOutput)),
                "score={score} 应报 PredictorOutput，实际 {outcome:?}"
            );
        }
    }

    /// 构造一条识别记录：整图假瓦片 + 候选 + 带标签裁剪。
    fn crop_record(
        candidate_quad: Quad,
        score: f64,
        source_width: u32,
        source_height: u32,
    ) -> CropRecord<FakeImage> {
        let tile = TileRegion::new(
            0,
            0,
            0,
            source_width,
            source_height,
            source_width,
            source_height,
        )
        .expect("测试瓦片合法");
        let candidate =
            DetectionCandidate::new(candidate_quad, score, tile, 0.0, false, Vec::new())
                .expect("测试候选合法");
        CropRecord {
            candidate,
            crop: FakeImage::Crop {
                left: candidate_quad[0].0,
                height: source_height,
            },
            source_width,
            source_height,
            attempts: Vec::new(),
        }
    }

    fn span_texts(spans: &[RecognizedSpan]) -> Vec<String> {
        spans.iter().map(|span| span.text().to_string()).collect()
    }

    // 覆盖 O-24（test_tiles_map_to_global_and_seam_fragments_are_recropped）：
    // 瓦片局部四边形 → 全局映射 → 接缝合并，且检测按瓦片顺序逐个调用。
    #[test]
    fn tiles_map_to_global_and_seam_fragments_are_consolidated() {
        let first_tile_quad = quad(1000.0, 100.0, 1216.0, 120.0);
        let second_tile_quad = quad(0.0, 101.0, 262.0, 121.0);
        let backend = FakeBackend::new().with_detect(move |call| match call {
            0 => Ok((vec![first_tile_quad], vec![0.9])),
            1 => Ok((vec![second_tile_quad], vec![0.9])),
            _ => Ok((Vec::new(), Vec::new())),
        });
        let candidates =
            detect_candidates(&backend, &FakeImage::Canvas, 2500, 400, None).expect("检测应成功");
        // 400×2500 选区：瓦片原点 x = 0、1088、2176，共 3 次检测调用。
        assert_eq!(backend.detect_calls(), 3);
        // 两个接缝片段先合并后去重，得到整体包围盒（Python oracle 同值）。
        assert_eq!(candidates.len(), 1);
        assert_eq!(*candidates[0].quad(), quad(1000.0, 100.0, 1350.0, 121.0));
        assert_eq!(candidates[0].detection_score(), 0.9);

        // 继续识别链路：合并后的框从原始选区重裁（350×21，源高 ≥ 20 不增强）。
        let mut records = vec![crop_record(*candidates[0].quad(), 0.9, 350, 21)];
        recognize_records(&backend, &mut records, None).expect("识别应成功");
        let spans = assemble_spans(&records, None).expect("组装应成功");
        assert_eq!(span_texts(&spans), vec!["text".to_string()]);
        assert_eq!(backend.enhanced_calls(), 0);
    }

    // 覆盖 O-26（test_orientation_policy_and_recognition_batch_size）：
    // 竖排加试 90/270、低置信度加试 180、高置信度不重试，批次 ≤ 8。
    #[test]
    fn orientation_policy_and_recognition_batch_size() {
        let boxes = [
            quad(0.0, 0.0, 20.0, 40.0),
            quad(40.0, 0.0, 140.0, 20.0),
            quad(0.0, 60.0, 100.0, 80.0),
        ];
        let scores = [0.9_f64, 0.8, 0.7];
        // 候选按附录 B 输出顺序（顶部、左边、瓦片 index）对应的源裁剪尺寸。
        let dimensions = [(20_u32, 40_u32), (100, 20), (100, 20)];
        let backend = FakeBackend::new()
            .with_detect(move |call| match call {
                0 => Ok((boxes.to_vec(), scores.to_vec())),
                _ => Ok((Vec::new(), Vec::new())),
            })
            .with_recognize(|batch| {
                Ok(batch
                    .iter()
                    .map(|image| match image {
                        FakeImage::Crop {
                            left: 0.0,
                            height: 40,
                        } => ("vertical-bad".to_string(), 0.4),
                        FakeImage::Crop {
                            left: 40.0,
                            height: 20,
                        } => ("low-bad".to_string(), 0.2),
                        FakeImage::Crop { .. } => ("confident".to_string(), 0.8),
                        FakeImage::Rotated { degrees: 90, inner }
                            if matches!(
                                inner.as_ref(),
                                FakeImage::Crop {
                                    left: 0.0,
                                    height: 40
                                }
                            ) =>
                        {
                            ("vertical".to_string(), 0.95)
                        }
                        FakeImage::Rotated {
                            degrees: 180,
                            inner,
                        } if matches!(
                            inner.as_ref(),
                            FakeImage::Crop {
                                left: 40.0,
                                height: 20
                            }
                        ) =>
                        {
                            ("upright".to_string(), 0.9)
                        }
                        _ => ("alternative".to_string(), 0.5),
                    })
                    .collect())
            });
        let candidates =
            detect_candidates(&backend, &FakeImage::Canvas, 200, 100, None).expect("检测应成功");
        assert_eq!(candidates.len(), 3);
        let mut records: Vec<CropRecord<FakeImage>> = candidates
            .iter()
            .zip(dimensions)
            .map(|(candidate, (width, height))| {
                crop_record(
                    *candidate.quad(),
                    candidate.detection_score(),
                    width,
                    height,
                )
            })
            .collect();
        recognize_records(&backend, &mut records, None).expect("识别应成功");
        let spans = assemble_spans(&records, None).expect("组装应成功");
        // 每条记录各取最优方向：竖排 90°、低置信度 180°、高置信度原方向。
        assert_eq!(
            span_texts(&spans),
            vec![
                "vertical".to_string(),
                "upright".to_string(),
                "confident".to_string(),
            ]
        );
        for batch in backend.recognize_batches() {
            assert!(batch.len() <= RECOGNITION_BATCH_SIZE);
        }
        let mut rotations = backend.rotations_called();
        rotations.sort_unstable();
        rotations.dedup();
        assert_eq!(rotations, vec![90, 180, 270]);
    }

    // 覆盖 O-26（test_dense_code_outline_uses_targeted_horizontal_retry）：
    // 密集代码行做一次 1.5× 拉伸重试，更长且分数达容差的文本替换 0° 尝试。
    #[test]
    fn dense_code_outline_uses_targeted_horizontal_retry() {
        let initial_text =
            "@0|_init_.py|TARGET_AST_UNAVAILABLE;M1|_init_.py::module|UNAVAILABLE|".to_string();
        let improved_text =
            "@0|__init__.py|TARGET_AST_UNAVAILABLE;M1|__init__.py::module|UNAVAILABLE|".to_string();
        let mut records = vec![crop_record(quad(0.0, 0.0, 500.0, 20.0), 0.9, 500, 20)];
        let backend = FakeBackend::new().with_recognize(move |batch| {
            Ok(batch
                .iter()
                .map(|image| match image {
                    FakeImage::Stretched(_) => (improved_text.clone(), 0.98),
                    _ => (initial_text.clone(), 0.99),
                })
                .collect())
        });
        recognize_records(&backend, &mut records, None).expect("识别应成功");
        let batches = backend.recognize_batches();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0], vec!["crop(0,20)".to_string()]);
        assert_eq!(batches[1], vec!["stretch(crop(0,20))".to_string()]);
        assert_eq!(backend.stretched_calls(), 1);
        assert!(backend.rotations_called().is_empty());
        let spans = assemble_spans(&records, None).expect("组装应成功");
        assert_eq!(
            span_texts(&spans),
            vec![
                "@0|__init__.py|TARGET_AST_UNAVAILABLE;M1|__init__.py::module|UNAVAILABLE|"
                    .to_string()
            ]
        );
    }

    // 覆盖 O-26（test_dense_code_retry_does_not_replace_a_longer_initial_result）：
    // 更短的重试文本即使分数更高也不替换初次结果。
    #[test]
    fn dense_code_retry_does_not_replace_a_longer_initial_result() {
        let initial_text =
            "@0|entry.py::module|UNAVAILABLE|;M1|entry.py::module|UNAVAILABLE|".to_string();
        let shorter_retry = initial_text
            .strip_suffix('|')
            .expect("测试文本以 | 结尾")
            .to_string();
        let final_text = initial_text.clone();
        let mut records = vec![crop_record(quad(0.0, 0.0, 500.0, 20.0), 0.9, 500, 20)];
        let backend = FakeBackend::new().with_recognize(move |batch| {
            Ok(batch
                .iter()
                .map(|image| match image {
                    FakeImage::Stretched(_) => (shorter_retry.clone(), 0.995),
                    _ => (initial_text.clone(), 0.99),
                })
                .collect())
        });
        recognize_records(&backend, &mut records, None).expect("识别应成功");
        assert_eq!(backend.recognize_batches().len(), 2);
        assert_eq!(backend.stretched_calls(), 1);
        let spans = assemble_spans(&records, None).expect("组装应成功");
        assert_eq!(span_texts(&spans), vec![final_text]);
    }

    // 覆盖 O-26（test_only_exact_empty_string_is_dropped_before_blank_layout）：
    // 只丢弃恰为空串的文本，单个空格保留。
    #[test]
    fn only_exact_empty_string_is_dropped_before_blank_layout() {
        let mut records = vec![
            crop_record(quad(0.0, 0.0, 20.0, 20.0), 0.9, 20, 20),
            crop_record(quad(40.0, 0.0, 60.0, 20.0), 0.9, 20, 20),
        ];
        let backend = FakeBackend::new().with_recognize(|batch| {
            Ok(batch
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    if index == 0 {
                        (String::new(), 0.9)
                    } else {
                        (" ".to_string(), 0.9)
                    }
                })
                .collect())
        });
        recognize_records(&backend, &mut records, None).expect("识别应成功");
        let spans = assemble_spans(&records, None).expect("组装应成功");
        assert_eq!(span_texts(&spans), vec![" ".to_string()]);
    }

    // 覆盖 O-26（test_visible_text_keeps_internal_whitespace）：
    // 识别文本内部空白原样保留。
    #[test]
    fn visible_text_keeps_internal_whitespace() {
        let mut records = vec![crop_record(quad(0.0, 0.0, 80.0, 20.0), 0.9, 80, 20)];
        let backend =
            FakeBackend::new().with_recognize(|_| Ok(vec![("left  right".to_string(), 0.9)]));
        recognize_records(&backend, &mut records, None).expect("识别应成功");
        let spans = assemble_spans(&records, None).expect("组装应成功");
        assert_eq!(span_texts(&spans), vec!["left  right".to_string()]);
    }

    // 覆盖 O-26（test_cancellation_is_observed_after_uninterruptible_tile_call）：
    // 检测中取消：当前不可中断调用返回后，在候选收集检查点生效。
    #[test]
    fn cancellation_is_observed_after_uninterruptible_tile_call() {
        let cancel = Arc::new(AtomicBool::new(false));
        let arm = cancel.clone();
        let backend = FakeBackend::new().with_detect(move |_| {
            arm.store(true, AtomicOrdering::SeqCst);
            Ok((Vec::new(), Vec::new()))
        });
        let outcome = detect_candidates(
            &backend,
            &FakeImage::Canvas,
            2500,
            400,
            Some(cancel.as_ref()),
        );
        assert_eq!(outcome, Err(PipelineError::Cancelled));
        // 第 1 个瓦片的检测调用完成后即取消，不发起后续瓦片。
        assert_eq!(backend.detect_calls(), 1);
    }

    // 覆盖 O-26（test_cancellation_is_observed_between_recognition_batches）：
    // 识别中取消：首批 8 张完整返回后，在批次检查点生效，不发起第 2 批。
    #[test]
    fn cancellation_is_observed_between_recognition_batches() {
        let cancel = Arc::new(AtomicBool::new(false));
        let arm = cancel.clone();
        let mut records: Vec<CropRecord<FakeImage>> = (0..9)
            .map(|index| {
                let left = f64::from(index * 30);
                crop_record(quad(left, 0.0, left + 20.0, 20.0), 0.9, 20, 20)
            })
            .collect();
        let backend = FakeBackend::new().with_recognize(move |batch| {
            arm.store(true, AtomicOrdering::SeqCst);
            Ok(batch.iter().map(|_| ("text".to_string(), 0.9)).collect())
        });
        let outcome = recognize_records(&backend, &mut records, Some(cancel.as_ref()));
        assert_eq!(outcome, Err(PipelineError::Cancelled));
        let batches = backend.recognize_batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 8);
    }

    // 覆盖 O-26（test_cancellation_during_final_empty_span_scan_wins_over_empty）：
    // 全部文本为空且已取消时，取消优先于「无文字」结果。
    #[test]
    fn cancellation_during_final_empty_span_scan_wins_over_empty() {
        let mut records = vec![crop_record(quad(0.0, 0.0, 20.0, 20.0), 0.9, 20, 20)];
        let backend = FakeBackend::new().with_recognize(|_| Ok(vec![(String::new(), 0.9)]));
        recognize_records(&backend, &mut records, None).expect("识别应成功");
        let cancel = Arc::new(AtomicBool::new(false));
        cancel.store(true, AtomicOrdering::SeqCst);
        let outcome = assemble_spans(&records, Some(cancel.as_ref()));
        assert_eq!(outcome, Err(PipelineError::Cancelled));
    }
}
