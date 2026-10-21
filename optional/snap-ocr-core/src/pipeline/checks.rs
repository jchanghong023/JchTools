use crate::types::Quad;

use super::backend::PipelineError;

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
    // [quality-baseline approved 2026-10-03] 冻结规格逐位镜像豁免，经用户裁定保留
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
    // [quality-baseline approved 2026-10-03] 冻结规格逐位镜像豁免，经用户裁定保留
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
    // [quality-baseline approved 2026-10-03] 冻结规格逐位镜像豁免，经用户裁定保留
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
