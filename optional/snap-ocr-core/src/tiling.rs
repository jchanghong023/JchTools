//! 确定性 OCR 瓦片生成与坐标映射（对应冻结仓库 `src/textsnap/tiling.py`）。
//!
//! 数值语义逐位对齐 Python：步长 = `tile_size - overlap`；尾块由
//! `min(tile_size, image_width - x)` 裁至选区边界，不回移；先 y 后 x 遍历，
//! `index` 按生成顺序从 0 连续编号。Python `ValueError` 分支对应
//! [`TilingError`]。

use std::fmt;

use crate::geometry::quad_bounds;
use crate::types::{DomainError, Quad, TileRegion};

/// 默认瓦片边长（Python `DEFAULT_TILE_SIZE`）。
pub const DEFAULT_TILE_SIZE: u32 = 1216;

/// 默认相邻瓦片重叠像素（Python `DEFAULT_TILE_OVERLAP`）。
pub const DEFAULT_TILE_OVERLAP: u32 = 128;

/// 内部接缝判定默认容差（Python `internal_edge_metrics` 的
/// `tolerance = 2.0`）。
pub const DEFAULT_EDGE_TOLERANCE: f64 = 2.0;

/// 瓦片生成错误（对应 Python `_axis_origins` 与 `TileRegion` 校验的
/// `ValueError` 分支）。
#[allow(clippy::module_name_repetitions)] // 附录 B 固定契约命名
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TilingError {
    /// 轴长度必须为正（Python: "axis length must be positive"；u32 下即 0）。
    NonPositiveAxisLength,
    /// tile_size 必须为正（Python: "tile_size must be positive"；u32 下即 0）。
    NonPositiveTileSize,
    /// overlap 必须满足 `0 <= overlap < tile_size`
    /// （Python: "overlap must satisfy 0 <= overlap < tile_size"；u32 下
    /// `overlap < 0` 不可能）。
    InvalidOverlap,
    /// [`TileRegion`] 构造校验失败（携带 [`DomainError`]）。
    InvalidTile(DomainError),
}

impl fmt::Display for TilingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonPositiveAxisLength => write!(f, "轴长度必须为正"),
            Self::NonPositiveTileSize => write!(f, "tile_size 必须为正"),
            Self::InvalidOverlap => write!(f, "overlap 必须满足 0 <= overlap < tile_size"),
            Self::InvalidTile(error) => write!(f, "瓦片区域校验失败：{error}"),
        }
    }
}

impl std::error::Error for TilingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidTile(error) => Some(error),
            _ => None,
        }
    }
}

impl From<DomainError> for TilingError {
    fn from(value: DomainError) -> Self {
        Self::InvalidTile(value)
    }
}

/// 单轴起点序列（Python `_axis_origins`）：步长 = `tile_size - overlap`，
/// 末个起点满足 `起点 + tile_size >= length`（尾块由调用方裁至边界）。
///
/// # Errors
/// `length == 0`、`tile_size == 0` 或 `overlap >= tile_size` 时返回
/// [`TilingError`]（校验分支与 Python 同序：length、tile_size、overlap）。
fn axis_origins(length: u32, tile_size: u32, overlap: u32) -> Result<Vec<u32>, TilingError> {
    if length == 0 {
        return Err(TilingError::NonPositiveAxisLength);
    }
    if tile_size == 0 {
        return Err(TilingError::NonPositiveTileSize);
    }
    if overlap >= tile_size {
        return Err(TilingError::InvalidOverlap);
    }
    if length <= tile_size {
        return Ok(vec![0]);
    }

    let step = tile_size - overlap;
    let mut origins = vec![0_u32];
    let mut last = 0_u32;
    // Python 大整数加法 `origins[-1] + tile_size < length` 对应 u32 的
    // checked_add：溢出即已越过 u32 上界，条件必然不成立，循环终止。
    while last.checked_add(tile_size).is_some_and(|end| end < length) {
        // 条件成立时 last + tile_size < length 且 step <= tile_size，
        // 故 last + step 不会溢出。
        last += step;
        origins.push(last);
    }
    Ok(origins)
}

/// 以显式瓦片参数覆盖图像（Python `generate_tiles` 的 tile_size / overlap
/// 关键字参数版本）。
///
/// 遍历顺序先 y 后 x，`index` 从 0 连续编号；尾块宽度/高度取
/// `min(tile_size, image_width - x)` / `min(tile_size, image_height - y)`，
/// 即尾部瓦片钉在选区右、下边界，不回移。
///
/// # Errors
/// 参数非法或 [`TileRegion`] 校验失败时返回 [`TilingError`]。
pub fn generate_tiles_with(
    image_width: u32,
    image_height: u32,
    tile_size: u32,
    overlap: u32,
) -> Result<Vec<TileRegion>, TilingError> {
    let x_origins = axis_origins(image_width, tile_size, overlap)?;
    let y_origins = axis_origins(image_height, tile_size, overlap)?;
    let mut tiles: Vec<TileRegion> = Vec::new();
    for &y in &y_origins {
        for &x in &x_origins {
            let width = tile_size.min(image_width - x);
            let height = tile_size.min(image_height - y);
            let index = tiles.len();
            tiles.push(TileRegion::new(
                index,
                x,
                y,
                width,
                height,
                image_width,
                image_height,
            )?);
        }
    }
    Ok(tiles)
}

/// 以默认瓦片参数（[`DEFAULT_TILE_SIZE`] / [`DEFAULT_TILE_OVERLAP`]）覆盖
/// 图像，尾部瓦片钉在选区右、下边界（Python `generate_tiles` 默认签名）。
///
/// # Errors
/// 同 [`generate_tiles_with`]。
pub fn generate_tiles(image_width: u32, image_height: u32) -> Result<Vec<TileRegion>, TilingError> {
    generate_tiles_with(
        image_width,
        image_height,
        DEFAULT_TILE_SIZE,
        DEFAULT_TILE_OVERLAP,
    )
}

/// 将瓦片局部坐标四边形平移到选区全局坐标（Python
/// `tiling.map_quad_to_global`：逐点 `+ (tile.x, tile.y)`，顶点顺序不变）。
///
/// 注意：Python 中把检测点夹取到 `[0, tile.width] × [0, tile.height]` 的
/// 逻辑位于 `ocr.py:671-674`（识别输出校验段），不属于本函数；调用方须先
/// 完成夹取再传入。
pub fn map_quad_to_global(quad: &Quad, tile: &TileRegion) -> Quad {
    let offset_x = f64::from(tile.x());
    let offset_y = f64::from(tile.y());
    quad.map(|(x, y)| (x + offset_x, y + offset_y))
}

/// 内部接缝度量（Python `internal_edge_metrics` 返回的
/// `(distance, touches)` 二元组）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeMetrics {
    /// 到最近内部瓦片边的距离；瓦片无任何内部边时为 `f64::INFINITY`
    /// （Python `math.inf`）。
    pub distance: f64,
    /// 最近距离是否不超过容差。
    pub touches: bool,
}

/// 计算全局坐标四边形到瓦片内部接缝的最近距离与是否触及，容差取
/// [`DEFAULT_EDGE_TOLERANCE`]（Python `internal_edge_metrics` 默认值）。
pub fn internal_edge_metrics(quad: &Quad, tile: &TileRegion) -> EdgeMetrics {
    internal_edge_metrics_with(quad, tile, DEFAULT_EDGE_TOLERANCE)
}

/// 同 [`internal_edge_metrics`]，显式指定容差。
///
/// # Panics
/// Python 对 `tolerance < 0` 或非有限容差抛出
/// `ValueError("tolerance must be a finite non-negative number")`；本函数
/// 签名按附录 B 固定为返回 [`EdgeMetrics`]，故非法容差视为调用方契约违规，
/// 立即 panic（与 Python 的异常中止语义对应）。
pub fn internal_edge_metrics_with(quad: &Quad, tile: &TileRegion, tolerance: f64) -> EdgeMetrics {
    assert!(
        tolerance.is_finite() && tolerance >= 0.0,
        "tolerance 必须是有限的非负数"
    );
    let (left, top, right, bottom) = quad_bounds(quad);
    let mut nearest = f64::INFINITY;
    if tile.has_internal_left() {
        nearest = nearest.min((left - f64::from(tile.x())).abs());
    }
    if tile.has_internal_top() {
        nearest = nearest.min((top - f64::from(tile.y())).abs());
    }
    if tile.has_internal_right() {
        nearest = nearest.min((f64::from(tile.right()) - right).abs());
    }
    if tile.has_internal_bottom() {
        nearest = nearest.min((f64::from(tile.bottom()) - bottom).abs());
    }
    EdgeMetrics {
        distance: nearest,
        touches: nearest <= tolerance,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

    use super::*;

    // 覆盖 O-24
    #[test]
    fn test_small_image_uses_one_exact_tile() {
        assert_eq!(
            generate_tiles(640, 480).unwrap(),
            vec![TileRegion::new(0, 0, 0, 640, 480, 640, 480).unwrap()]
        );
    }

    // 覆盖 O-24
    #[test]
    fn test_large_axis_has_128_pixel_overlap_and_tail_coverage() {
        let tiles = generate_tiles(2500, 600).unwrap();
        let xs: Vec<u32> = tiles.iter().map(TileRegion::x).collect();
        let widths: Vec<u32> = tiles.iter().map(TileRegion::width).collect();
        assert_eq!(xs, [0, 1088, 2176]);
        assert_eq!(widths, [1216, 1216, 324]);
        assert_eq!(tiles.last().unwrap().right(), 2500);
        assert_eq!(tiles[0].right() - tiles[1].x(), 128);
        assert_eq!(tiles[1].right() - tiles[2].x(), 128);
    }

    // 覆盖 O-24
    #[test]
    fn test_two_dimensional_tiles_cover_every_boundary() {
        let tiles = generate_tiles(2305, 2305).unwrap();
        assert_eq!(tiles.len(), 9);
        assert_eq!(tiles.iter().map(TileRegion::right).max(), Some(2305));
        assert_eq!(tiles.iter().map(TileRegion::bottom).max(), Some(2305));
        assert!(tiles
            .iter()
            .any(|tile| tile.x() == 2176 && tile.y() == 2176));
    }

    // 覆盖 O-24
    #[test]
    fn test_invalid_overlap_is_rejected() {
        assert!(generate_tiles_with(100, 100, 128, 128).is_err());
    }

    // 覆盖 O-24
    #[test]
    fn test_mapping_and_internal_edge_distance() {
        let tile = generate_tiles(2000, 400).unwrap()[1];
        let local: Quad = [(0.0, 10.0), (50.0, 10.0), (50.0, 30.0), (0.0, 30.0)];
        let global_quad = map_quad_to_global(&local, &tile);
        assert_eq!(global_quad[0], (1088.0, 10.0));
        let metrics = internal_edge_metrics(&global_quad, &tile);
        assert_eq!(metrics.distance, 0.0);
        assert!(metrics.touches);
    }
}
