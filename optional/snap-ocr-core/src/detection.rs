//! 检测候选的接缝合并与重叠去重（对应冻结仓库 `src/textsnap/detection.py`）。
//!
//! 行为以 Python oracle 为准：浮点比较顺序、稳定排序的平局规则、并查集的
//! 合并方向与分组首现顺序均逐位保持一致；数值语义见
//! `docs/requirements/SNAP2TEXT.md` 附录 B（O-25）。
//!
//! # 阈值校验语义
//! Python 对非法阈值抛出 `ValueError`；本模块导出签名固定为返回
//! [`Vec`]（无 [`Result`]），因此以带
//! 消息的 [`panic!`] 表达调用方契约违规：`*_with` 变体的阈值参数必须处于
//! [0, 1]（NaN 同样拒绝），`edge_tolerance` 不得为负（NaN 与 Python 一致
//! 地放行，因为 Python 只检查 `edge_tolerance < 0`）。
//!
//! # 合并分支的不可达错误
//! 合并组件需要构造并集 [`TileRegion`] 与新的 [`DetectionCandidate`]，
//! 两者构造均返回 [`Result`]；而
//! [`merge_seam_fragments`] 系列签名不可失败。输入候选在构造时已通过全部
//! 校验，且同一批候选共享同一图像尺寸（一次选区截图必然如此），因此并集
//! 瓦片与合并候选必然合法；该前提失效属于不可达状态，代码以
//! [`unreachable!`] 终止并在调用点注明理由。

use std::cmp::Ordering;

use crate::geometry::{
    bounding_quad_of_quads, overlap_metrics, quad_bounds, quad_dimensions, vertical_overlap_ratio,
};
use crate::tiling::{internal_edge_metrics, EdgeMetrics};
use crate::types::{DetectionCandidate, DomainError, Quad, TileRegion};

/// 重复判定 IoU 阈值（Python `DEFAULT_IOU_THRESHOLD`）。
pub const DEFAULT_IOU_THRESHOLD: f64 = 0.4;
/// 交集/较小面积阈值（Python `DEFAULT_SMALLER_INTERSECTION_THRESHOLD`）。
pub const DEFAULT_SMALLER_INTERSECTION_THRESHOLD: f64 = 0.6;
/// 接缝合并垂直重叠阈值（Python `DEFAULT_VERTICAL_OVERLAP_THRESHOLD`）。
pub const DEFAULT_SEAM_VERTICAL_OVERLAP: f64 = 0.6;
/// 接缝合并高度相似阈值（Python `DEFAULT_HEIGHT_SIMILARITY`）。
pub const DEFAULT_SEAM_HEIGHT_SIMILARITY: f64 = 0.65;
/// 接缝边缘基础容差（像素；Python `merge_seam_fragments` 的 `edge_tolerance`
/// 默认值）。
pub const DEFAULT_SEAM_EDGE_TOLERANCE: f64 = 4.0;

/// 并查集（对应 Python `_DisjointSet`）：查找带路径压缩；合并时固定把第二
/// 个根挂到第一个根下，保证分组结果与 Python 完全一致。
struct DisjointSet {
    parents: Vec<usize>,
}

impl DisjointSet {
    fn new(size: usize) -> Self {
        Self {
            parents: (0..size).collect(),
        }
    }

    fn find(&mut self, value: usize) -> usize {
        let parent = self.parents[value];
        if parent != value {
            let root = self.find(parent);
            self.parents[value] = root;
        }
        self.parents[value]
    }

    fn union(&mut self, first: usize, second: usize) {
        let first_root = self.find(first);
        let second_root = self.find(second);
        if first_root != second_root {
            self.parents[second_root] = first_root;
        }
    }
}

/// 候选优先级比较（Python `_candidate_rank` 的元组 `(not touches,
/// internal_edge_distance, detection_score)` 取 max）：不接触内部边缘优先，
/// 其次边缘距离更大，再次检测分数更高。相等值（包括正负零）保持
/// Python 的浮点数值比较语义。
fn rank_cmp(left: &DetectionCandidate, right: &DetectionCandidate) -> Ordering {
    (!left.touches_internal_edge())
        .cmp(&(!right.touches_internal_edge()))
        .then_with(|| {
            left.internal_edge_distance()
                .partial_cmp(&right.internal_edge_distance())
                .unwrap_or(Ordering::Equal)
        })
        .then_with(|| {
            left.detection_score()
                .partial_cmp(&right.detection_score())
                .unwrap_or(Ordering::Equal)
        })
}

/// 在同一组件内取最优候选下标；与 Python `max(component, key=...)` 一致，
/// 平局保留组内先出现者。
fn max_rank_index(items: &[DetectionCandidate], component: &[usize]) -> usize {
    let mut best = component[0];
    for &index in &component[1..] {
        if rank_cmp(&items[index], &items[best]) == Ordering::Greater {
            best = index;
        }
    }
    best
}

/// 校验阈值处于 [0, 1]（对应 Python 的 `ValueError`；NaN 同样拒绝）。
fn require_unit_range(name: &str, value: f64) {
    assert!(
        (0.0..=1.0).contains(&value),
        "{name} 必须处于 0 与 1 之间，得到 {value}"
    );
}

/// 按根下标把成员聚成组件；组件顺序与其首个成员的出现顺序一致（对应
/// Python `dict` 的插入序），成员按下标升序排列。
fn component_groups(size: usize, groups: &mut DisjointSet) -> Vec<Vec<usize>> {
    let mut roots: Vec<usize> = Vec::new();
    let mut components: Vec<Vec<usize>> = Vec::new();
    for index in 0..size {
        let root = groups.find(index);
        if let Some(slot) = roots
            .iter()
            .position(|candidate_root| *candidate_root == root)
        {
            components[slot].push(index);
        } else {
            roots.push(root);
            components.push(vec![index]);
        }
    }
    components
}

/// 候选框右缘是否触及源瓦片右内部接缝（Python `_touches_right_edge`）。
fn touches_right_edge(candidate: &DetectionCandidate, tolerance: f64) -> bool {
    let right = quad_bounds(candidate.quad()).2;
    let tile = candidate.source_tile();
    tile.has_internal_right() && (f64::from(tile.right()) - right).abs() <= tolerance
}

/// 候选框左缘是否触及源瓦片左内部接缝（Python `_touches_left_edge`）。
fn touches_left_edge(candidate: &DetectionCandidate, tolerance: f64) -> bool {
    let left = quad_bounds(candidate.quad()).0;
    let tile = candidate.source_tile();
    tile.has_internal_left() && (left - f64::from(tile.x())).abs() <= tolerance
}

/// 两个候选的源瓦片是否为水平相邻的重叠瓦片（Python
/// `_tiles_are_horizontal_neighbors`）。`max(0, min(bottom) - max(y)) <= 0`
/// 与原始差值 `<= 0` 等价，故直接比较差值。
fn tiles_are_horizontal_neighbors(first: &DetectionCandidate, second: &DetectionCandidate) -> bool {
    let first_tile = first.source_tile();
    let second_tile = second.source_tile();
    let tile_vertical_overlap = i64::from(first_tile.bottom().min(second_tile.bottom()))
        - i64::from(first_tile.y().max(second_tile.y()));
    if tile_vertical_overlap <= 0 || first_tile.x() == second_tile.x() {
        return false;
    }
    first_tile.x().max(second_tile.x()) < first_tile.right().min(second_tile.right())
}

/// 判定两个候选是否为同一接缝切割出的片段（Python `_is_seam_pair`）。
fn is_seam_pair(
    first: &DetectionCandidate,
    second: &DetectionCandidate,
    vertical_overlap_threshold: f64,
    height_similarity: f64,
    edge_tolerance: f64,
) -> bool {
    if !tiles_are_horizontal_neighbors(first, second) {
        return false;
    }
    // Python `sorted(..., key=tile.x)` 为稳定排序；相等时保留 (first,
    // second)（实际不会发生：瓦片 x 相等已被水平相邻检查排除）。
    let (left_tile_candidate, right_tile_candidate) =
        if first.source_tile().x() <= second.source_tile().x() {
            (first, second)
        } else {
            (second, first)
        };
    let first_height = quad_dimensions(first.quad()).1;
    let second_height = quad_dimensions(second.quad()).1;
    let larger_height = first_height.max(second_height);
    if larger_height <= 0.0 || first_height.min(second_height) / larger_height < height_similarity {
        return false;
    }
    // 检测模型可能在重叠瓦片中漏掉被接缝裁掉一半的边缘字形，导致检测框
    // 整体内缩约半个字高，固定 4 像素容差无法重新连接两段。把尺度感知
    // 余量限制在观测字高的一半以内，使其只作用于接缝附近，不桥接普通
    // 栏间距。（与 Python 内注释一致。）
    // Python max(edge_tolerance, h*0.5) 在首参为 NaN 时返回 NaN（比较恒 False
    // 保留首个参数），使后续 `<= NaN` 恒 False、永不合并；Rust f64::max 会丢弃
    // NaN，须显式传播以对齐。
    let effective_edge_tolerance = if edge_tolerance.is_nan() {
        f64::NAN
    } else {
        edge_tolerance.max(larger_height * 0.5)
    };
    if !touches_right_edge(left_tile_candidate, effective_edge_tolerance) {
        return false;
    }
    if !touches_left_edge(right_tile_candidate, effective_edge_tolerance) {
        return false;
    }
    if vertical_overlap_ratio(first.quad(), second.quad()) < vertical_overlap_threshold {
        return false;
    }
    let left_bounds = quad_bounds(left_tile_candidate.quad());
    let right_bounds = quad_bounds(right_tile_candidate.quad());
    // 水平相邻已保证 right.x < left.right；saturating_sub 对应 Python 的
    // max(0, ...) 钳制。
    let tile_overlap = f64::from(
        left_tile_candidate
            .source_tile()
            .right()
            .saturating_sub(right_tile_candidate.source_tile().x()),
    );
    let horizontal_gap = right_bounds.0 - left_bounds.2;
    horizontal_gap <= tile_overlap + larger_height * 2.0
}

/// 成员四边形的整体包围框直接交给 `geometry::bounding_quad_of_quads`
/// （对应 Python `bounding_quad(所有成员四边形)`，IEEE 位模式级已与
/// oracle 差分验证）。
///
/// 合并一个重叠/接缝组件（Python `_merge_component`）：外接矩形、分数取
/// 最大、成员瓦片并集矩形上重算内部边缘量、源瓦片取 `(x, y, index)` 最大
/// 者、瓦片下标拼接后由构造函数去重排序。
fn merge_component(
    items: &[DetectionCandidate],
    component: &[usize],
) -> Result<DetectionCandidate, DomainError> {
    let mut source = items[component[0]].source_tile();
    let mut union_x = source.x();
    let mut union_y = source.y();
    let mut union_right = source.right();
    let mut union_bottom = source.bottom();
    let mut max_score = items[component[0]].detection_score();
    for &index in &component[1..] {
        let candidate = &items[index];
        let tile = candidate.source_tile();
        if (tile.x(), tile.y(), tile.index()) > (source.x(), source.y(), source.index()) {
            source = tile;
        }
        union_x = union_x.min(tile.x());
        union_y = union_y.min(tile.y());
        union_right = union_right.max(tile.right());
        union_bottom = union_bottom.max(tile.bottom());
        max_score = max_score.max(candidate.detection_score());
    }
    let member_quads: Vec<Quad> = component
        .iter()
        .map(|index| *items[*index].quad())
        .collect();
    let merged_quad = match bounding_quad_of_quads(&member_quads) {
        Ok(quad) => quad,
        // 分组逻辑保证组件非空，外接框输入不可能为空。
        Err(error) => unreachable!("合并组件至少包含一个候选：{error}"),
    };
    let union_tile = TileRegion::new(
        source.index(),
        union_x,
        union_y,
        union_right - union_x,
        union_bottom - union_y,
        source.image_width(),
        source.image_height(),
    )?;
    let EdgeMetrics { distance, touches } = internal_edge_metrics(&merged_quad, &union_tile);
    let indices = component
        .iter()
        .flat_map(|index| items[*index].source_tile_indices().iter().copied())
        .collect::<Vec<_>>();
    DetectionCandidate::new(merged_quad, max_score, *source, distance, touches, indices)
}

/// 合并水平相邻瓦片接缝处被切断的文字片段（Python
/// `merge_seam_fragments` 默认阈值版本）。
#[must_use]
pub fn merge_seam_fragments(candidates: &[DetectionCandidate]) -> Vec<DetectionCandidate> {
    merge_seam_fragments_with(
        candidates,
        DEFAULT_SEAM_VERTICAL_OVERLAP,
        DEFAULT_SEAM_HEIGHT_SIMILARITY,
        DEFAULT_SEAM_EDGE_TOLERANCE,
    )
}

/// 合并接缝片段（可调阈值）。输出按 (顶部, 左边) 稳定排序。
///
/// # Panics
/// `vertical_overlap_threshold` 或 `height_similarity` 不在 [0, 1]、或
/// `edge_tolerance` 为负时（对应 Python `ValueError`）。
pub fn merge_seam_fragments_with(
    candidates: &[DetectionCandidate],
    vertical_overlap_threshold: f64,
    height_similarity: f64,
    edge_tolerance: f64,
) -> Vec<DetectionCandidate> {
    require_unit_range("vertical_overlap_threshold", vertical_overlap_threshold);
    require_unit_range("height_similarity", height_similarity);
    // Python 只检查 `edge_tolerance < 0`，NaN 由此放行；显式列出 NaN，
    // 避免对偏序类型取反比较。
    assert!(
        edge_tolerance >= 0.0 || edge_tolerance.is_nan(),
        "edge_tolerance 必须非负，得到 {edge_tolerance}"
    );

    let mut groups = DisjointSet::new(candidates.len());
    for (first_index, first) in candidates.iter().enumerate() {
        for (second_index, second) in candidates.iter().enumerate().skip(first_index + 1) {
            if is_seam_pair(
                first,
                second,
                vertical_overlap_threshold,
                height_similarity,
                edge_tolerance,
            ) {
                groups.union(first_index, second_index);
            }
        }
    }

    let components = component_groups(candidates.len(), &mut groups);
    let mut merged: Vec<DetectionCandidate> = Vec::with_capacity(components.len());
    for component in &components {
        if component.len() == 1 {
            merged.push(candidates[component[0]].clone());
        } else {
            match merge_component(candidates, component) {
                Ok(candidate) => merged.push(candidate),
                Err(error) => unreachable!(
                    "组件成员均已通过构造校验且共享同一图像尺寸，并集瓦片与合并候选必然合法：{error}"
                ),
            }
        }
    }

    // 输出排序（Python `sorted` 稳定序）：顶部、左边。
    let mut decorated: Vec<(f64, f64, DetectionCandidate)> = merged
        .into_iter()
        .map(|candidate| {
            let bounds = quad_bounds(candidate.quad());
            (bounds.1, bounds.0, candidate)
        })
        .collect();
    decorated.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.1.partial_cmp(&right.1).unwrap_or(Ordering::Equal))
    });
    decorated
        .into_iter()
        .map(|(_, _, candidate)| candidate)
        .collect()
}

/// 重叠去重（Python `deduplicate_candidates` 默认阈值版本）。
#[must_use]
pub fn deduplicate_candidates(candidates: &[DetectionCandidate]) -> Vec<DetectionCandidate> {
    deduplicate_candidates_with(
        candidates,
        DEFAULT_IOU_THRESHOLD,
        DEFAULT_SMALLER_INTERSECTION_THRESHOLD,
    )
}

/// 重叠去重（可调阈值）。按传递关系分组后每组保留计划定义的最优候选，
/// 输出按 (顶部, 左边, 源瓦片下标) 稳定排序。
///
/// # Panics
/// 任一阈值不在 [0, 1] 时（对应 Python `ValueError`）。
pub fn deduplicate_candidates_with(
    candidates: &[DetectionCandidate],
    iou_threshold: f64,
    smaller_intersection_threshold: f64,
) -> Vec<DetectionCandidate> {
    require_unit_range("iou_threshold", iou_threshold);
    require_unit_range(
        "smaller_intersection_threshold",
        smaller_intersection_threshold,
    );

    let mut groups = DisjointSet::new(candidates.len());
    for (first_index, first) in candidates.iter().enumerate() {
        for (second_index, second) in candidates.iter().enumerate().skip(first_index + 1) {
            let metrics = overlap_metrics(first.quad(), second.quad());
            if metrics.iou >= iou_threshold
                || metrics.smaller_ratio >= smaller_intersection_threshold
            {
                groups.union(first_index, second_index);
            }
        }
    }

    let components = component_groups(candidates.len(), &mut groups);
    // 输出排序（Python `sorted` 稳定序）：顶部、左边、源瓦片下标。
    let mut decorated: Vec<(f64, f64, usize, DetectionCandidate)> = components
        .iter()
        .map(|component| {
            let best = &candidates[max_rank_index(candidates, component)];
            let bounds = quad_bounds(best.quad());
            (bounds.1, bounds.0, best.source_tile().index(), best.clone())
        })
        .collect();
    decorated.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.1.partial_cmp(&right.1).unwrap_or(Ordering::Equal))
            .then_with(|| left.2.cmp(&right.2))
    });
    decorated
        .into_iter()
        .map(|(_, _, _, candidate)| candidate)
        .collect()
}

/// 完整整理：先合并接缝片段、后重叠去重（Python `consolidate_candidates`）。
///
/// 跨接缝的长行可能自身满足重复阈值；先合并可保住两端外缘，再去重清除
/// 残余的整框重复。
#[must_use]
pub fn consolidate_candidates(candidates: &[DetectionCandidate]) -> Vec<DetectionCandidate> {
    deduplicate_candidates(&merge_seam_fragments(candidates))
}

#[cfg(test)]
mod tests {
    // 覆盖 O-25：12 个构造坐标用例逐字翻译自冻结仓库
    // tests/test_detection.py，期望值与断言不增不减。
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

    use super::{consolidate_candidates, deduplicate_candidates, merge_seam_fragments};
    use crate::types::{DetectionCandidate, Quad, TileRegion};

    /// 对应 Python `_quad(left, top, right, bottom)`。
    fn rect_quad(left: f64, top: f64, right: f64, bottom: f64) -> Quad {
        [(left, top), (right, top), (right, bottom), (left, bottom)]
    }

    /// 对应 Python `_candidate(...)`；未显式给出 `source_tile_indices`，
    /// 由构造函数归一化为 `[source_tile.index]`。
    fn candidate(
        quad: Quad,
        score: f64,
        tile: TileRegion,
        distance: f64,
        touching: bool,
    ) -> DetectionCandidate {
        DetectionCandidate::new(quad, score, tile, distance, touching, Vec::new()).unwrap()
    }

    /// 对应 Python `setUp` 中的 `tile0/tile1/tile2`。
    fn base_tiles() -> (TileRegion, TileRegion, TileRegion) {
        (
            TileRegion::new(0, 0, 0, 1216, 400, 2500, 400).unwrap(),
            TileRegion::new(1, 1088, 0, 1216, 400, 2500, 400).unwrap(),
            TileRegion::new(2, 2176, 0, 324, 400, 2500, 400).unwrap(),
        )
    }

    // 覆盖 O-25：数值相等的浮点平局（包括正负零）保留首个候选。
    #[test]
    fn duplicate_rank_keeps_first_for_signed_zero_score_tie() {
        let (tile0, tile1, _) = base_tiles();
        let quad = rect_quad(100.0, 20.0, 300.0, 50.0);
        let first = candidate(quad, -0.0, tile1, 1.0, false);
        let second = candidate(quad, 0.0, tile0, 1.0, false);
        assert_eq!(
            deduplicate_candidates(&[first.clone(), second]).as_slice(),
            &[first]
        );
    }

    // 覆盖 O-25：test_duplicate_prefers_candidate_away_from_internal_edge
    #[test]
    fn duplicate_prefers_candidate_away_from_internal_edge() {
        let (tile0, tile1, _) = base_tiles();
        let edge = candidate(rect_quad(100.0, 20.0, 300.0, 50.0), 0.99, tile0, 0.0, true);
        let interior = candidate(
            rect_quad(102.0, 20.0, 302.0, 50.0),
            0.55,
            tile1,
            30.0,
            false,
        );
        assert_eq!(
            deduplicate_candidates(&[edge, interior.clone()]).as_slice(),
            &[interior]
        );
    }

    // 覆盖 O-25：test_duplicate_then_prefers_distance_then_score
    #[test]
    fn duplicate_then_prefers_distance_then_score() {
        let (tile0, tile1, _) = base_tiles();
        let near = candidate(
            rect_quad(100.0, 20.0, 300.0, 50.0),
            0.99,
            tile0,
            10.0,
            false,
        );
        let far = candidate(
            rect_quad(100.0, 20.0, 300.0, 50.0),
            0.50,
            tile1,
            20.0,
            false,
        );
        assert_eq!(
            deduplicate_candidates(&[near, far.clone()]).as_slice(),
            &[far]
        );
    }

    // 覆盖 O-25：test_transitive_duplicate_group_is_single_candidate
    #[test]
    fn transitive_duplicate_group_is_single_candidate() {
        let (tile0, tile1, tile2) = base_tiles();
        let first = candidate(rect_quad(0.0, 0.0, 100.0, 20.0), 0.6, tile0, 5.0, false);
        let middle = candidate(rect_quad(30.0, 0.0, 130.0, 20.0), 0.7, tile1, 6.0, false);
        let last = candidate(rect_quad(60.0, 0.0, 160.0, 20.0), 0.8, tile2, 7.0, false);
        assert_eq!(
            deduplicate_candidates(&[first, middle, last.clone()]).as_slice(),
            &[last]
        );
    }

    // 覆盖 O-25：test_seam_fragments_merge_across_overlapping_tiles
    #[test]
    fn seam_fragments_merge_across_overlapping_tiles() {
        let (tile0, tile1, _) = base_tiles();
        let left = candidate(
            rect_quad(1000.0, 100.0, 1216.0, 120.0),
            0.8,
            tile0,
            0.0,
            true,
        );
        let right = candidate(
            rect_quad(1088.0, 101.0, 1350.0, 121.0),
            0.9,
            tile1,
            0.0,
            true,
        );
        let merged = merge_seam_fragments(&[left, right]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].quad(), &rect_quad(1000.0, 100.0, 1350.0, 121.0));
        assert_eq!(merged[0].source_tile_indices(), &[0_usize, 1]);
    }

    /// 对应 Python `_four_tile_seam_fragments`。
    fn four_tile_seam_fragments() -> Vec<DetectionCandidate> {
        let tiles = [
            TileRegion::new(0, 0, 0, 1216, 400, 3840, 400).unwrap(),
            TileRegion::new(1, 1088, 0, 1216, 400, 3840, 400).unwrap(),
            TileRegion::new(2, 2176, 0, 1216, 400, 3840, 400).unwrap(),
            TileRegion::new(3, 3264, 0, 576, 400, 3840, 400).unwrap(),
        ];
        let quads = [
            rect_quad(1000.0, 100.0, 1216.0, 120.0),
            rect_quad(1088.0, 100.0, 2304.0, 120.0),
            rect_quad(2176.0, 100.0, 3392.0, 120.0),
            rect_quad(3264.0, 100.0, 3500.0, 120.0),
        ];
        // 与 Python `0.8 + index * 0.01` 逐位相同的字面量。
        let scores = [0.8, 0.8 + 0.01, 0.8 + 0.02, 0.8 + 0.03];
        tiles
            .iter()
            .zip(quads.iter().zip(scores))
            .map(|(tile, (quad, score))| candidate(*quad, score, *tile, 0.0, true))
            .collect()
    }

    /// 对应 Python `_assert_four_tile_seam_chain`。
    fn assert_four_tile_seam_chain(fragments: &[DetectionCandidate]) {
        let consolidated = consolidate_candidates(fragments);
        assert_eq!(consolidated.len(), 1);
        assert_eq!(
            consolidated[0].quad(),
            &rect_quad(1000.0, 100.0, 3500.0, 120.0)
        );
        assert_eq!(consolidated[0].source_tile_indices(), &[0_usize, 1, 2, 3]);
    }

    // 覆盖 O-25：test_four_tile_seam_chain_merges_in_forward_order
    #[test]
    fn four_tile_seam_chain_merges_in_forward_order() {
        assert_four_tile_seam_chain(&four_tile_seam_fragments());
    }

    // 覆盖 O-25：test_four_tile_seam_chain_merges_in_reverse_order
    #[test]
    fn four_tile_seam_chain_merges_in_reverse_order() {
        let mut fragments = four_tile_seam_fragments();
        fragments.reverse();
        assert_four_tile_seam_chain(&fragments);
    }

    // 覆盖 O-25：test_overlap_threshold_does_not_discard_one_seam_outer_edge
    #[test]
    fn overlap_threshold_does_not_discard_one_seam_outer_edge() {
        let (tile0, tile1, _) = base_tiles();
        let left = candidate(
            rect_quad(1016.0, 100.0, 1216.0, 120.0),
            0.8,
            tile0,
            0.0,
            true,
        );
        let right = candidate(
            rect_quad(1088.0, 100.0, 1300.0, 120.0),
            0.9,
            tile1,
            0.0,
            true,
        );
        let consolidated = consolidate_candidates(&[left, right]);
        assert_eq!(consolidated.len(), 1);
        assert_eq!(
            consolidated[0].quad(),
            &rect_quad(1016.0, 100.0, 1300.0, 120.0)
        );
        assert_eq!(consolidated[0].source_tile_indices(), &[0_usize, 1]);
    }

    // 覆盖 O-25：test_complete_height_duplicate_beats_row_seam_clipped_merge
    #[test]
    fn complete_height_duplicate_beats_row_seam_clipped_merge() {
        let tiles = [
            TileRegion::new(0, 0, 0, 1216, 1216, 2500, 2160).unwrap(),
            TileRegion::new(1, 1088, 0, 1216, 1216, 2500, 2160).unwrap(),
            TileRegion::new(3, 0, 1088, 1216, 1072, 2500, 2160).unwrap(),
            TileRegion::new(4, 1088, 1088, 1216, 1072, 2500, 2160).unwrap(),
        ];
        let quads = [
            rect_quad(1000.0, 1200.0, 1216.0, 1216.0),
            rect_quad(1088.0, 1200.0, 1400.0, 1216.0),
            rect_quad(1000.0, 1200.0, 1216.0, 1230.0),
            rect_quad(1088.0, 1200.0, 1400.0, 1230.0),
        ];
        let candidates: Vec<DetectionCandidate> = quads
            .iter()
            .zip(tiles.iter())
            .map(|(quad, tile)| candidate(*quad, 0.9, *tile, 0.0, true))
            .collect();
        let consolidated = consolidate_candidates(&candidates);
        assert_eq!(consolidated.len(), 1);
        assert_eq!(
            consolidated[0].quad(),
            &rect_quad(1000.0, 1200.0, 1400.0, 1230.0)
        );
    }

    // 覆盖 O-25：test_interior_fragment_does_not_replace_reconstructed_seam_line
    #[test]
    fn interior_fragment_does_not_replace_reconstructed_seam_line() {
        let (tile0, tile1, tile2) = base_tiles();
        let left = candidate(
            rect_quad(20.0, 100.0, 1214.0, 120.0),
            0.88,
            tile0,
            2.0,
            true,
        );
        let middle = candidate(
            rect_quad(1089.0, 100.0, 2302.0, 120.0),
            0.91,
            tile1,
            1.0,
            true,
        );
        let short_interior = candidate(
            rect_quad(2184.0, 100.0, 2368.0, 120.0),
            0.89,
            tile2,
            8.0,
            false,
        );
        let consolidated = consolidate_candidates(&[left, middle, short_interior]);
        assert_eq!(consolidated.len(), 1);
        assert_eq!(
            consolidated[0].quad(),
            &rect_quad(20.0, 100.0, 2368.0, 120.0)
        );
        assert_eq!(consolidated[0].source_tile_indices(), &[0_usize, 1, 2]);
    }

    // 覆盖 O-25：test_seam_fragment_can_start_half_text_height_inside_tile
    #[test]
    fn seam_fragment_can_start_half_text_height_inside_tile() {
        let (tile0, tile1, _) = base_tiles();
        let left = candidate(
            rect_quad(20.0, 100.0, 1214.0, 134.0),
            0.86,
            tile0,
            2.0,
            true,
        );
        let right = candidate(
            rect_quad(1105.0, 101.0, 2303.0, 135.0),
            0.85,
            tile1,
            1.0,
            true,
        );
        let consolidated = consolidate_candidates(&[left, right]);
        assert_eq!(consolidated.len(), 1);
        assert_eq!(
            consolidated[0].quad(),
            &rect_quad(20.0, 100.0, 2303.0, 135.0)
        );
        assert_eq!(consolidated[0].source_tile_indices(), &[0_usize, 1]);
    }

    // 覆盖 O-25：test_different_rows_do_not_merge_at_seam
    #[test]
    fn different_rows_do_not_merge_at_seam() {
        let (tile0, tile1, _) = base_tiles();
        let first = candidate(rect_quad(1000.0, 10.0, 1216.0, 30.0), 0.8, tile0, 0.0, true);
        let second = candidate(rect_quad(1088.0, 40.0, 1350.0, 60.0), 0.8, tile1, 0.0, true);
        assert_eq!(merge_seam_fragments(&[first, second]).len(), 2);
    }

    // 覆盖 O-25：test_full_consolidation_keeps_unrelated_candidate
    #[test]
    fn full_consolidation_keeps_unrelated_candidate() {
        let (tile0, tile1, _) = base_tiles();
        let duplicate1 = candidate(rect_quad(20.0, 20.0, 100.0, 40.0), 0.5, tile0, 10.0, false);
        let duplicate2 = candidate(rect_quad(21.0, 20.0, 101.0, 40.0), 0.6, tile1, 20.0, false);
        let unrelated = candidate(
            rect_quad(500.0, 80.0, 600.0, 100.0),
            0.7,
            tile0,
            50.0,
            false,
        );
        let result =
            consolidate_candidates(&[duplicate1.clone(), duplicate2.clone(), unrelated.clone()]);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&duplicate2));
        assert!(result.contains(&unrelated));
    }
}
