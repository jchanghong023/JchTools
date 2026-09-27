//! 四边形几何辅助（对应冻结仓库 `src/textsnap/geometry.py`）。
//!
//! 全部为无依赖纯函数：面积用鞋带公式，凸多边形交集用 Sutherland–Hodgman
//! 裁剪。数值语义逐位对齐 Python（`f64`、`_EPSILON = 1e-9`、`atan2` 关键字
//! 稳定排序、分支与运算顺序一致）。

use std::fmt;

use crate::types::{Point, Quad};

/// 浮点比较容差（Python `_EPSILON`）。
const EPSILON: f64 = 1e-9;

/// 几何模块错误（对应 Python `ValueError` 分支）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryError {
    /// 多四边形外接框至少需要一个输入（Python: "at least one quad is
    /// required"）。
    EmptyQuadList,
}

impl fmt::Display for GeometryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyQuadList => write!(f, "至少需要一个四边形"),
        }
    }
}

impl std::error::Error for GeometryError {}

/// 返回绕质心按 `atan2` 关键字升序排序的顶点（Python `ordered_polygon`；
/// 排序稳定，与 Python `sorted` 一致）。少于 3 个点时原样返回。
#[allow(clippy::cast_precision_loss)] // 点数转 f64 参与质心均值，与 Python 精度语义一致
pub fn ordered_polygon(points: &[Point]) -> Vec<Point> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let center_x = points.iter().map(|point| point.0).sum::<f64>() / points.len() as f64;
    let center_y = points.iter().map(|point| point.1).sum::<f64>() / points.len() as f64;
    let mut ordered = points.to_vec();
    ordered.sort_by(|first, second| {
        let first_key = (first.1 - center_y).atan2(first.0 - center_x);
        let second_key = (second.1 - center_y).atan2(second.0 - center_x);
        // f64 全序排序对应 Python 的浮点比较全序。
        first_key.total_cmp(&second_key)
    });
    ordered
}

/// 任意多边形鞋带面积（Python `polygon_area`）；少于 3 个点返回 0.0。
pub fn polygon_area(points: &[Point]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    let mut total = 0.0;
    for (index, point) in points.iter().enumerate() {
        let next_point = points[(index + 1) % points.len()];
        total += point.0 * next_point.1 - next_point.0 * point.1;
    }
    total.abs() * 0.5
}

/// 四边形面积：先按 [`ordered_polygon`] 重排顶点再取鞋带面积
/// （Python `quad_area`）。
pub fn quad_area(quad: &Quad) -> f64 {
    polygon_area(&ordered_polygon(quad))
}

/// 四边形轴对齐包围盒 `(min_x, min_y, max_x, max_y)`（Python `quad_bounds`）。
pub fn quad_bounds(quad: &Quad) -> (f64, f64, f64, f64) {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for &(x, y) in quad {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    (min_x, min_y, max_x, max_y)
}

/// 轴对齐宽高 `(max(0.0, right-left), max(0.0, bottom-top))`
/// （Python `quad_dimensions`）。
pub fn quad_dimensions(quad: &Quad) -> (f64, f64) {
    let (left, top, right, bottom) = quad_bounds(quad);
    ((right - left).max(0.0), (bottom - top).max(0.0))
}

/// 四顶点坐标均值的中心点（Python `quad_center`）。
pub fn quad_center(quad: &Quad) -> Point {
    let center_x = quad.iter().map(|point| point.0).sum::<f64>() / 4.0;
    let center_y = quad.iter().map(|point| point.1).sum::<f64>() / 4.0;
    (center_x, center_y)
}

/// 以两个最低顶点 y 的中位数近似基线（Python `quad_baseline`；
/// `statistics.median` 对两元素即 `(a + b) / 2`，故不用 `f64::midpoint`
/// 以保证逐位一致）。
pub fn quad_baseline(quad: &Quad) -> f64 {
    let mut ys = [quad[0].1, quad[1].1, quad[2].1, quad[3].1];
    // 降序排序对应 Python `sorted(..., reverse=True)`，取前两元素。
    ys.sort_by(|first, second| second.total_cmp(first));
    #[allow(clippy::manual_midpoint)] // 与 Python `(a + b) / 2` 逐位一致
    {
        (ys[0] + ys[1]) / 2.0
    }
}

/// 垂直重叠占较小高度的比例（Python `vertical_overlap_ratio`）；
/// 较小高度 `<= 1e-9` 时返回 0.0。
pub fn vertical_overlap_ratio(first: &Quad, second: &Quad) -> f64 {
    let (_, first_top, _, first_bottom) = quad_bounds(first);
    let (_, second_top, _, second_bottom) = quad_bounds(second);
    let overlap = (first_bottom.min(second_bottom) - first_top.max(second_top)).max(0.0);
    let smaller_height = (first_bottom - first_top).min(second_bottom - second_top);
    if smaller_height <= EPSILON {
        return 0.0;
    }
    overlap / smaller_height
}

/// 水平重叠占较小宽度的比例（Python `horizontal_overlap_ratio`）；
/// 较小宽度 `<= 1e-9` 时返回 0.0。
pub fn horizontal_overlap_ratio(first: &Quad, second: &Quad) -> f64 {
    let (first_left, _, first_right, _) = quad_bounds(first);
    let (second_left, _, second_right, _) = quad_bounds(second);
    let overlap = (first_right.min(second_right) - first_left.max(second_left)).max(0.0);
    let smaller_width = (first_right - first_left).min(second_right - second_left);
    if smaller_width <= EPSILON {
        return 0.0;
    }
    overlap / smaller_width
}

/// 叉积 `(second - first) × (third - first)`（Python `_cross`）。
fn cross(first: Point, second: Point, third: Point) -> f64 {
    (second.0 - first.0) * (third.1 - first.1) - (second.1 - first.1) * (third.0 - first.0)
}

/// 线段与裁剪边所在直线的交点；分母绝对值 `<= 1e-9` 时返回线段终点
/// （Python `_line_intersection`）。
// 变量名逐字沿用 Python 源（segment_dx/segment_dy/clip_dx/clip_dy），便于对审。
#[allow(clippy::similar_names)]
fn line_intersection(
    segment_start: Point,
    segment_end: Point,
    clip_start: Point,
    clip_end: Point,
) -> Point {
    let segment_dx = segment_end.0 - segment_start.0;
    let segment_dy = segment_end.1 - segment_start.1;
    let clip_dx = clip_end.0 - clip_start.0;
    let clip_dy = clip_end.1 - clip_start.1;
    let denominator = segment_dx * clip_dy - segment_dy * clip_dx;
    if denominator.abs() <= EPSILON {
        return segment_end;
    }
    let delta_x = clip_start.0 - segment_start.0;
    let delta_y = clip_start.1 - segment_start.1;
    let factor = (delta_x * clip_dy - delta_y * clip_dx) / denominator;
    (
        segment_start.0 + factor * segment_dx,
        segment_start.1 + factor * segment_dy,
    )
}

/// 用 Sutherland–Hodgman 算法将 `subject_points` 按 `clip_points` 凸裁剪
/// （Python `convex_intersection`）；任一多边形少于 3 个顶点时返回空。
pub fn convex_intersection(subject_points: &[Point], clip_points: &[Point]) -> Vec<Point> {
    let mut output = ordered_polygon(subject_points);
    let clip = ordered_polygon(clip_points);
    if output.len() < 3 || clip.len() < 3 {
        return Vec::new();
    }

    for (index, &clip_start) in clip.iter().enumerate() {
        let clip_end = clip[(index + 1) % clip.len()];
        let input_points = std::mem::take(&mut output);
        output = Vec::new();
        if input_points.is_empty() {
            break;
        }
        let mut previous = input_points[input_points.len() - 1];
        let mut previous_inside = cross(clip_start, clip_end, previous) >= -EPSILON;
        for &current in &input_points {
            let current_inside = cross(clip_start, clip_end, current) >= -EPSILON;
            if current_inside {
                if !previous_inside {
                    output.push(line_intersection(previous, current, clip_start, clip_end));
                }
                output.push(current);
            } else if previous_inside {
                output.push(line_intersection(previous, current, clip_start, clip_end));
            }
            previous = current;
            previous_inside = current_inside;
        }
    }
    output
}

/// 凸交集面积（Python `intersection_area`）。
pub fn intersection_area(first: &Quad, second: &Quad) -> f64 {
    polygon_area(&convex_intersection(first, second))
}

/// 重叠度量（Python `overlap_metrics` 返回 `(iou, smaller_ratio)`；交集面积
/// 按附录 B 固定契约一并导出）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlapMetrics {
    /// 凸交集面积；退化输入时为 0.0。
    pub intersection: f64,
    /// 交集 / 并集；任一面积退化或并集 `<= 1e-9` 时为 0.0。
    pub iou: f64,
    /// 交集 / 较小面积。
    pub smaller_ratio: f64,
}

/// 计算两个四边形的重叠度量；任一面积 `<= 1e-9`（退化零面积）时全 0
/// （Python `overlap_metrics`）。
pub fn overlap_metrics(first: &Quad, second: &Quad) -> OverlapMetrics {
    let first_area = quad_area(first);
    let second_area = quad_area(second);
    if first_area <= EPSILON || second_area <= EPSILON {
        return OverlapMetrics {
            intersection: 0.0,
            iou: 0.0,
            smaller_ratio: 0.0,
        };
    }
    let intersection = intersection_area(first, second);
    let union = first_area + second_area - intersection;
    let iou = if union > EPSILON {
        intersection / union
    } else {
        0.0
    };
    let smaller_ratio = intersection / first_area.min(second_area);
    OverlapMetrics {
        intersection,
        iou,
        smaller_ratio,
    }
}

/// 单个四边形的轴对齐外接框，顶点顺序与 Python `bounding_quad` 输出一致：
/// `((left, top), (right, top), (right, bottom), (left, bottom))`。
///
/// 这是 Python `bounding_quad(quads)` 在单元素输入下的特化（固定契约）；
/// 多四边形版本见 [`bounding_quad_of_quads`]。
pub fn bounding_quad(quad: &Quad) -> Quad {
    let (left, top, right, bottom) = quad_bounds(quad);
    [(left, top), (right, top), (right, bottom), (left, bottom)]
}

/// 多个四边形的轴对齐外接框，顶点顺序同 [`bounding_quad`]
/// （Python `bounding_quad(quads: Iterable[Quad])` 的忠实移植）。
///
/// # Errors
/// 输入为空时返回 [`GeometryError::EmptyQuadList`]（对应 Python
/// `ValueError("at least one quad is required")`）。
pub fn bounding_quad_of_quads(quads: &[Quad]) -> Result<Quad, GeometryError> {
    if quads.is_empty() {
        return Err(GeometryError::EmptyQuadList);
    }
    let mut left = f64::INFINITY;
    let mut top = f64::INFINITY;
    let mut right = f64::NEG_INFINITY;
    let mut bottom = f64::NEG_INFINITY;
    for quad in quads {
        let (quad_left, quad_top, quad_right, quad_bottom) = quad_bounds(quad);
        left = left.min(quad_left);
        top = top.min(quad_top);
        right = right.max(quad_right);
        bottom = bottom.max(quad_bottom);
    }
    Ok([(left, top), (right, top), (right, bottom), (left, bottom)])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

    use super::*;

    /// Python `assertAlmostEqual`（默认 places=7，即 `round(a-b, 7) == 0`）
    /// 的等价断言。
    fn assert_almost_equal(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 0.5e-7,
            "actual={actual:?} expected={expected:?}"
        );
    }

    // 覆盖 O-25
    #[test]
    fn test_rotated_quad_area_and_self_overlap() {
        let quad: Quad = [(0.0, 1.0), (1.0, 0.0), (2.0, 1.0), (1.0, 2.0)];
        assert_almost_equal(quad_area(&quad), 2.0);
        let metrics = overlap_metrics(&quad, &quad);
        assert_almost_equal(metrics.iou, 1.0);
        assert_almost_equal(metrics.smaller_ratio, 1.0);
    }

    // 覆盖 O-25
    #[test]
    fn test_partial_axis_aligned_overlap() {
        let first: Quad = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let second: Quad = [(5.0, 0.0), (15.0, 0.0), (15.0, 10.0), (5.0, 10.0)];
        let metrics = overlap_metrics(&first, &second);
        assert_almost_equal(metrics.iou, 1.0 / 3.0);
        assert_almost_equal(metrics.smaller_ratio, 0.5);
    }

    // 覆盖 O-25
    #[test]
    fn test_vertical_overlap_uses_smaller_height() {
        let first: Quad = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let second: Quad = [(20.0, 5.0), (30.0, 5.0), (30.0, 25.0), (20.0, 25.0)];
        assert_almost_equal(vertical_overlap_ratio(&first, &second), 0.5);
    }

    // 覆盖 O-25
    #[test]
    fn test_bounding_quad() {
        let first: Quad = [(2.0, 3.0), (4.0, 3.0), (4.0, 5.0), (2.0, 5.0)];
        let second: Quad = [(-1.0, 6.0), (8.0, 6.0), (8.0, 9.0), (-1.0, 9.0)];
        assert_eq!(
            bounding_quad_of_quads(&[first, second]).unwrap(),
            [(-1.0, 3.0), (8.0, 3.0), (8.0, 9.0), (-1.0, 9.0)]
        );
    }
}
