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

    fn tile(&self, _image: &Self::Image, tile: &TileRegion) -> Result<Self::Image, PipelineError> {
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
    let candidate = DetectionCandidate::new(candidate_quad, score, tile, 0.0, false, Vec::new())
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

// 覆盖 O-24 / O-25：瓦片局部四边形 → 全局映射 → 接缝合并，
// 且检测按瓦片顺序逐个调用；假图像不能验证原图重裁或透视重采样。
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

    // 注入与合并框尺寸对应的假裁剪（350×21，源高 ≥ 20 不增强）。
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
    assert_eq!(backend.rotations_called(), [] as [u16; 0]);
    let spans = assemble_spans(&records, None).expect("组装应成功");
    assert_eq!(
        span_texts(&spans),
        vec![
            "@0|__init__.py|TARGET_AST_UNAVAILABLE;M1|__init__.py::module|UNAVAILABLE|".to_string()
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
    let backend = FakeBackend::new().with_recognize(|_| Ok(vec![("left  right".to_string(), 0.9)]));
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
