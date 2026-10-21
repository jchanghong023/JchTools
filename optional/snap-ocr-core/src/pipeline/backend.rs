use std::fmt;

use crate::orientation::RecognitionAttempt;
use crate::types::{DetectionCandidate, DomainError, Quad, TileRegion};

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
///   有限，分数必须有限且处于 [0, 1]；违规时实现应返回 `Err`。管线按
///   [`generate_tiles`] 的生成顺序先调用 [`OcrBackend::tile`]，再将其返回的
///   瓦片裁剪图交给检测（batch=1）；后端不得以调用次数隐含瓦片。
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
/// 调用方须从原始选区按合并、去重后的全局四边形做透视重裁，不能传入
/// 瓦片片段或拼接文字。本模块只消费已裁剪的图像句柄，不执行或验证重裁。
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
