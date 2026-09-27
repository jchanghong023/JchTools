//! 稳定、无依赖的数据边界（对应冻结仓库 `src/textsnap/domain.py`）。
//!
//! 校验语义与 Python `__post_init__` 一致：坐标必须有限、分数必须处于
//! \[0,1\]、瓦片不得越界；构造失败返回 [`DomainError`] 而不是 panic。

use std::fmt;

/// 二维点（选区全局坐标，物理像素）。
pub type Point = (f64, f64);

/// 检测四边形：保持检测后处理返回的顶点顺序，不重排。
pub type Quad = [Point; 4];

/// 数据边界校验错误（对应 Python `ValueError`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    /// 坐标或分数出现 NaN/无穷。
    NotFinite(&'static str),
    /// 分数越出 \[0,1\]。
    ScoreOutOfRange(&'static str),
    /// 瓦片尺寸、图像尺寸非正或瓦片越界。
    TileOutOfBounds(&'static str),
    /// 边缘距离为负。
    NegativeEdgeDistance,
    /// 旋转角度不是 0/90/180/270。
    InvalidRotation(u16),
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DomainError::NotFinite(field) => write!(f, "{field} 必须为有限值"),
            DomainError::ScoreOutOfRange(field) => write!(f, "{field} 必须处于 0 与 1 之间"),
            DomainError::TileOutOfBounds(detail) => write!(f, "瓦片参数非法：{detail}"),
            DomainError::NegativeEdgeDistance => write!(f, "internal_edge_distance 必须非负"),
            DomainError::InvalidRotation(deg) => {
                write!(f, "rotation_degrees 必须为 0/90/180/270，得到 {deg}")
            }
        }
    }
}

impl std::error::Error for DomainError {}

fn require_finite(value: f64, field: &'static str) -> Result<(), DomainError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(DomainError::NotFinite(field))
    }
}

fn validate_quad(quad: &Quad) -> Result<(), DomainError> {
    for &(x, y) in quad {
        require_finite(x, "quad x")?;
        require_finite(y, "quad y")?;
    }
    Ok(())
}
/// 选区内的一个物理像素瓦片（对应 `TileRegion`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRegion {
    index: usize,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    image_width: u32,
    image_height: u32,
}

impl TileRegion {
    /// 按冻结仓库校验规则构造瓦片。
    ///
    /// # Errors
    /// 尺寸非正或瓦片越界时返回 [`DomainError::TileOutOfBounds`]。
    pub fn new(
        index: usize,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        image_width: u32,
        image_height: u32,
    ) -> Result<Self, DomainError> {
        if width == 0 || height == 0 {
            return Err(DomainError::TileOutOfBounds(
                "tile dimensions must be positive",
            ));
        }
        if image_width == 0 || image_height == 0 {
            return Err(DomainError::TileOutOfBounds(
                "image dimensions must be positive",
            ));
        }
        let right = x
            .checked_add(width)
            .ok_or(DomainError::TileOutOfBounds("tile origin overflow"))?;
        let bottom = y
            .checked_add(height)
            .ok_or(DomainError::TileOutOfBounds("tile origin overflow"))?;
        if right > image_width {
            return Err(DomainError::TileOutOfBounds("tile exceeds image width"));
        }
        if bottom > image_height {
            return Err(DomainError::TileOutOfBounds("tile exceeds image height"));
        }
        Ok(Self {
            index,
            x,
            y,
            width,
            height,
            image_width,
            image_height,
        })
    }

    pub const fn index(&self) -> usize {
        self.index
    }

    pub const fn x(&self) -> u32 {
        self.x
    }

    pub const fn y(&self) -> u32 {
        self.y
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub const fn image_width(&self) -> u32 {
        self.image_width
    }

    pub const fn image_height(&self) -> u32 {
        self.image_height
    }

    pub const fn right(&self) -> u32 {
        self.x + self.width
    }

    pub const fn bottom(&self) -> u32 {
        self.y + self.height
    }

    /// 左侧存在内部接缝（瓦片不在选区最左）。
    pub const fn has_internal_left(&self) -> bool {
        self.x > 0
    }

    /// 顶部存在内部接缝。
    pub const fn has_internal_top(&self) -> bool {
        self.y > 0
    }

    /// 右侧存在内部接缝。
    pub const fn has_internal_right(&self) -> bool {
        self.right() < self.image_width
    }

    /// 底部存在内部接缝。
    pub const fn has_internal_bottom(&self) -> bool {
        self.bottom() < self.image_height
    }
}

/// 从单个瓦片映射到选区全局坐标的检测候选（对应 `DetectionCandidate`）。
#[derive(Debug, Clone, PartialEq)]
pub struct DetectionCandidate {
    quad: Quad,
    detection_score: f64,
    source_tile: TileRegion,
    internal_edge_distance: f64,
    touches_internal_edge: bool,
    source_tile_indices: Vec<usize>,
}

impl DetectionCandidate {
    /// 构造候选；`source_tile_indices` 为空时归一化为 `[source_tile.index]`，
    /// 否则排序去重（与 Python `__post_init__` 一致）。
    ///
    /// # Errors
    /// 四边形坐标、分数或边缘距离非法时返回 [`DomainError`]。
    pub fn new(
        quad: Quad,
        detection_score: f64,
        source_tile: TileRegion,
        internal_edge_distance: f64,
        touches_internal_edge: bool,
        source_tile_indices: Vec<usize>,
    ) -> Result<Self, DomainError> {
        validate_quad(&quad)?;
        require_finite(detection_score, "detection_score")?;
        if !(0.0..=1.0).contains(&detection_score) {
            return Err(DomainError::ScoreOutOfRange("detection_score"));
        }
        if internal_edge_distance.is_nan() {
            return Err(DomainError::NotFinite("internal_edge_distance"));
        }
        if internal_edge_distance < 0.0 {
            return Err(DomainError::NegativeEdgeDistance);
        }
        let mut indices = if source_tile_indices.is_empty() {
            vec![source_tile.index]
        } else {
            source_tile_indices
        };
        indices.sort_unstable();
        indices.dedup();
        Ok(Self {
            quad,
            detection_score,
            source_tile,
            internal_edge_distance,
            touches_internal_edge,
            source_tile_indices: indices,
        })
    }

    pub const fn quad(&self) -> &Quad {
        &self.quad
    }

    pub const fn detection_score(&self) -> f64 {
        self.detection_score
    }

    pub const fn source_tile(&self) -> &TileRegion {
        &self.source_tile
    }

    pub const fn internal_edge_distance(&self) -> f64 {
        self.internal_edge_distance
    }

    pub const fn touches_internal_edge(&self) -> bool {
        self.touches_internal_edge
    }

    pub fn source_tile_indices(&self) -> &[usize] {
        &self.source_tile_indices
    }
}

/// 单个已识别文本片段及其最终选区全局几何（对应 `RecognizedSpan`）。
#[derive(Debug, Clone, PartialEq)]
pub struct RecognizedSpan {
    quad: Quad,
    text: String,
    detection_score: f64,
    recognition_score: f64,
    rotation_degrees: u16,
}

impl RecognizedSpan {
    /// 构造片段；旋转角度只接受 0/90/180/270。
    ///
    /// # Errors
    /// 四边形坐标、分数或旋转角度非法时返回 [`DomainError`]。
    pub fn new(
        quad: Quad,
        text: String,
        detection_score: f64,
        recognition_score: f64,
        rotation_degrees: u16,
    ) -> Result<Self, DomainError> {
        validate_quad(&quad)?;
        require_finite(detection_score, "detection_score")?;
        if !(0.0..=1.0).contains(&detection_score) {
            return Err(DomainError::ScoreOutOfRange("detection_score"));
        }
        require_finite(recognition_score, "recognition_score")?;
        if !(0.0..=1.0).contains(&recognition_score) {
            return Err(DomainError::ScoreOutOfRange("recognition_score"));
        }
        if !matches!(rotation_degrees, 0 | 90 | 180 | 270) {
            return Err(DomainError::InvalidRotation(rotation_degrees));
        }
        Ok(Self {
            quad,
            text,
            detection_score,
            recognition_score,
            rotation_degrees,
        })
    }

    pub const fn quad(&self) -> &Quad {
        &self.quad
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn detection_score(&self) -> f64 {
        self.detection_score
    }

    pub const fn recognition_score(&self) -> f64 {
        self.recognition_score
    }

    pub const fn rotation_degrees(&self) -> u16 {
        self.rotation_degrees
    }
}

#[cfg(test)]
mod tests {
    // 覆盖 O-25/O-26：数据边界校验与冻结仓库 domain.py 语义一致。
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn tile() -> TileRegion {
        TileRegion::new(3, 100, 200, 1216, 1088, 4000, 3000).unwrap()
    }

    #[test]
    fn tile_rejects_nonpositive_dimensions() {
        assert!(TileRegion::new(0, 0, 0, 0, 10, 100, 100).is_err());
        assert!(TileRegion::new(0, 0, 0, 10, 0, 100, 100).is_err());
        assert!(TileRegion::new(0, 0, 0, 10, 10, 0, 100).is_err());
    }

    #[test]
    fn tile_rejects_out_of_bounds() {
        assert!(TileRegion::new(0, 990, 0, 1216, 100, 1000, 1000).is_err());
        assert!(TileRegion::new(0, 0, 990, 1216, 100, 1000, 1000).is_err());
        assert!(TileRegion::new(0, 900, 0, 100, 100, 1000, 1000).is_ok());
    }

    #[test]
    fn tile_internal_edges_follow_image_bounds() {
        let t = tile();
        assert!(t.has_internal_left());
        assert!(t.has_internal_top());
        assert!(t.has_internal_right());
        assert!(t.has_internal_bottom());
        let corner = TileRegion::new(0, 0, 0, 100, 100, 100, 100).unwrap();
        assert!(!corner.has_internal_left());
        assert!(!corner.has_internal_top());
        assert!(!corner.has_internal_right());
        assert!(!corner.has_internal_bottom());
    }

    #[test]
    fn candidate_normalizes_tile_indices() {
        let c = DetectionCandidate::new(
            [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
            0.5,
            tile(),
            3.0,
            false,
            vec![7, 3, 3, 5],
        )
        .unwrap();
        assert_eq!(c.source_tile_indices(), &[3, 5, 7]);
        let d = DetectionCandidate::new(
            [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
            0.5,
            tile(),
            3.0,
            false,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(d.source_tile_indices(), &[3]);
    }

    #[test]
    fn candidate_rejects_bad_scores_and_edges() {
        let q = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        assert!(DetectionCandidate::new(q, 1.5, tile(), 0.0, false, vec![]).is_err());
        assert!(DetectionCandidate::new(q, f64::NAN, tile(), 0.0, false, vec![]).is_err());
        assert!(DetectionCandidate::new(q, 0.5, tile(), -1.0, false, vec![]).is_err());
        let bad_q = [(0.0, 0.0), (10.0, f64::INFINITY), (10.0, 10.0), (0.0, 10.0)];
        assert!(DetectionCandidate::new(bad_q, 0.5, tile(), 0.0, false, vec![]).is_err());
    }

    #[test]
    fn span_rejects_invalid_rotation() {
        let q = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        assert!(RecognizedSpan::new(q, "文本".into(), 0.5, 0.9, 90).is_ok());
        assert!(RecognizedSpan::new(q, "文本".into(), 0.5, 0.9, 45).is_err());
        assert!(RecognizedSpan::new(q, "文本".into(), 0.5, 1.2, 0).is_err());
    }
}
