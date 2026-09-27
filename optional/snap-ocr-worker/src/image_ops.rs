//! BGR 图像容器与冻结 TextSnap 所用 OpenCV 4.10 图像算子的逐位对齐实现（O-23/O-26）。
//!
//! 权威语义来源（冻结栈 = `opencv-contrib-python` 4.10.0，x86-64 基线 SIMD=SSE2，
//! 运行期 `WarpPerspectiveLine` 走 SSE4.1/标量双精度分支，两者数值等价）：
//!
//! - `cv2.resize` INTER_LINEAR / INTER_CUBIC / INTER_LANCZOS4（8uC3）：
//!   `modules/imgproc/src/resize.cpp` 的 `hal::resize` 定点路径用于 LINEAR、
//!   LANCZOS4 以及源图窄于四点的 CUBIC；水平/垂直系数以 Q11
//!   （`INTER_RESIZE_COEF_SCALE = 2^11`）短整型量化。通常的 CUBIC
//!   使用浮点四点卷积，保留在 Q11 量化时丢失的半整数附近精度。
//!   坐标映射为半像素中心；通用定点路径先以 double 计算再窄化 f32。
//!   INTER_LINEAR 在 8u 上水平与垂直均为纯整数运算（含右缘 `*2048` 单点分支），
//!   与 SIMD 向量路径整数等价；通用定点 INTER_CUBIC 垂直混合在 SSE2 基线
//!   下以 8 元素步进用 f32，尾段用 Q22 定点；INTER_LANCZ4 全程 Q22 定点
//!   （8u 无向量实现）。INTER_LINEAR 且两轴恰为整数 2× 时重定向到
//!   INTER_AREA fast（2×2 块均值 `(a+b+c+d+2)>>2`）。
//! - `cv2.warpPerspective` INTER_CUBIC + BORDER_REPLICATE（8uC3）：
//!   `modules/imgproc/src/imgwarp.cpp`。逆映射每输出块（默认 64×16）按行以
//!   双精度递推：`W = W0 + m20·x1 → 32/W`，`fX = (X0 + m00·x1)·W`，`cvRound`
//!   （半偶）取整后 `X>>5` 为整数坐标、低 5bit 为子像素；采样走 `remapBicubic`，
//!   权重表 `BicubicTab_i` 为 Q15 短整型并带 2×2 补偿校正。前置 3×3 由
//!   `getPerspectiveTransform`（8×8 列主元 LU，`DBL_EPSILON*100` 奇异阈）求出，
//!   再经 `invert`（3×3 伴随公式）求逆。
//! - `numpy.rot90`：TextSnap `rotate` 仅允许 90/180/270 度（逆时针）。
//!
//! 附录 B 取整：透视输出宽高用 `round_ties_even`（Python `round` 半偶），与
//! `snap-ocr-core::pipeline::validate_perspective` 逐位一致。所有浮点→整型
//! 舍入一律 `round_ties_even`（对应 x86 `cvtps/cvtsd` 与 OpenCV `cvRound`）；
//! `as` 截断仅用于已保证为整的值。

use std::fmt;
use std::sync::LazyLock;

/// `resize.cpp:905`：8u resize 定点系数位数（Q11）。
const INTER_RESIZE_COEF_BITS: u32 = 11;
/// Q11 标度 = 2048。
const INTER_RESIZE_COEF_SCALE: i32 = 1 << INTER_RESIZE_COEF_BITS;
/// `imgwarp.cpp:127`：remap 双三次权重表标度（Q15）。
const INTER_REMAP_COEF_SCALE: f32 = 32_768.0;
/// `imgwarp.cpp`：warp 定点子像素位数（5bit → 32 档）。
const INTER_BITS: u32 = 5;
const INTER_TAB_SIZE: i32 = 1 << INTER_BITS;

/// 图像算子错误（对齐 TextSnap `ocr.py:168-231` 的 `ValueError` 语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageOpError {
    /// 缓冲长度与宽高不符。
    InvalidBufferLen,
    /// 检测四边形无效（坐标非有限 / 顶点重复 / 非凸）。
    InvalidDetectionQuad,
    /// 检测四边形退化（有向面积为零或输出边 < 2 像素）。
    DegenerateDetectionQuad,
    /// 旋转角度不在 {90, 180, 270}。
    InvalidRotation(u32),
}

impl fmt::Display for ImageOpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBufferLen => write!(f, "image buffer length does not match dimensions"),
            Self::InvalidDetectionQuad => write!(f, "invalid detection quadrilateral"),
            Self::DegenerateDetectionQuad => write!(f, "degenerate detection quadrilateral"),
            Self::InvalidRotation(degrees) => {
                write!(
                    f,
                    "rotation must be 90, 180, or 270 degrees (got {degrees})"
                )
            }
        }
    }
}

impl std::error::Error for ImageOpError {}

/// BGR 交错的 8bit 图像（H×W×3 行主序，与 OpenCV `Mat` 布局一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgrImage {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl BgrImage {
    /// 全零图。
    #[must_use]
    pub fn zeroed(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![0; width * height * 3],
        }
    }

    /// 由现有缓冲构造，校验 `data.len() == width * height * 3`。
    ///
    /// # Errors
    /// 长度不符时返回 [`ImageOpError::InvalidBufferLen`]。
    pub fn from_vec(width: usize, height: usize, data: Vec<u8>) -> Result<Self, ImageOpError> {
        if data.len() != width * height * 3 {
            return Err(ImageOpError::InvalidBufferLen);
        }
        Ok(Self {
            width,
            height,
            data,
        })
    }

    /// 图宽（像素）。
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// 图高（像素）。
    #[must_use]
    pub fn height(&self) -> usize {
        self.height
    }

    /// 原始 BGR 行主序缓冲。
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// `(y, x)` 处的 `(B, G, R)`。
    #[must_use]
    pub fn pixel(&self, y: usize, x: usize) -> [u8; 3] {
        let base = (y * self.width + x) * 3;
        [self.data[base], self.data[base + 1], self.data[base + 2]]
    }
}

/// `resize.cpp:908` `interpolateCubic`（f32 逐式复刻，A = -0.75）。
fn interpolate_cubic(x: f32, coeffs: &mut [f32]) {
    let a = -0.75f32;
    coeffs[0] = ((a * (x + 1.0) - 5.0 * a) * (x + 1.0) + 8.0 * a) * (x + 1.0) - 4.0 * a;
    coeffs[1] = ((a + 2.0) * x - (a + 3.0)) * x * x + 1.0;
    coeffs[2] = ((a + 2.0) * (1.0 - x) - (a + 3.0)) * (1.0 - x) * (1.0 - x) + 1.0;
    coeffs[3] = 1.0 - coeffs[0] - coeffs[1] - coeffs[2];
}

/// `resize.cpp:918` `interpolateLanczos4`（f32 系数、f64 三角函数，a = 4）。
// OpenCV's eight fixed taps use signed offsets and narrow f64 trigonometric
// results back to f32 coefficients.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
fn interpolate_lanczos4(x: f32, coeffs: &mut [f32]) {
    let s45 = std::f64::consts::FRAC_1_SQRT_2;
    let cs = [
        (1.0, 0.0),
        (-s45, -s45),
        (0.0, 1.0),
        (s45, -s45),
        (-1.0, 0.0),
        (s45, s45),
        (0.0, -1.0),
        (-s45, s45),
    ];
    let cv_pi = std::f64::consts::PI;
    let mut sum = 0f32;
    let y0 = -(f64::from(x) + 3.0) * cv_pi * 0.25;
    let s0 = y0.sin();
    let c0 = y0.cos();
    for (i, coeff) in coeffs.iter_mut().enumerate().take(8) {
        let y0_ = x + (3_i32 - i as i32) as f32;
        if y0_.abs() >= 1e-6f32 {
            let y = -f64::from(y0_) * cv_pi * 0.25;
            *coeff = ((cs[i].0 * s0 + cs[i].1 * c0) / (y * y)) as f32;
        } else {
            *coeff = 1e30f32;
        }
        sum += *coeff;
    }
    sum = 1.0f32 / sum;
    for coeff in coeffs.iter_mut().take(8) {
        *coeff *= sum;
    }
}

/// OpenCV `saturate_cast<short>(float)`：`cvRound`（半偶）后饱和。
// OpenCV saturate_cast rounds in f32 before narrowing to signed 16-bit.
#[allow(clippy::cast_possible_truncation)]
fn sat_short(v: f32) -> i16 {
    v.round_ties_even() as i16
}

/// `clip(v, 0, hi)`（`imgwarp.cpp:320`）：夹到 `[0, hi-1]`。
fn clip(v: i32, hi: i32) -> i32 {
    if v >= 0 {
        if v < hi {
            v
        } else {
            hi - 1
        }
    } else {
        0
    }
}

/// OpenCV `saturate_cast<uchar>(int)`：夹到 `[0, 255]`。
#[allow(clippy::cast_sign_loss)]
fn sat_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// `resize.cpp` 中的插值模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interp {
    /// INTER_LINEAR（ksize = 2）。
    Linear,
    /// INTER_CUBIC（ksize = 4）。
    Cubic,
    /// INTER_LANCZOS4（ksize = 8）。
    Lanczos4,
}

impl Interp {
    fn ksize(self) -> usize {
        match self {
            Self::Linear => 2,
            Self::Cubic => 4,
            Self::Lanczos4 => 8,
        }
    }
}
/// IPP's 8-bit cubic resize keeps the interpolation weights in floating point
/// rather than rounding each axis to OpenCV's Q11 generic-resize coefficients.
/// Evaluate the same four-tap kernel in f64 so values close to a half-integer
/// retain their sub-Q11 contribution until the final byte conversion.
// IPP converts image dimensions and pixel centers to f64, then rounds and
// clamps signed sample coordinates before converting them to buffer indices.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn resize_cubic_precise(src: &BgrImage, dw: usize, dh: usize) -> BgrImage {
    fn weights(x: f64) -> [f64; 4] {
        let a = -0.75;
        let c0 = ((a * (x + 1.0) - 5.0 * a) * (x + 1.0) + 8.0 * a) * (x + 1.0) - 4.0 * a;
        let c1 = ((a + 2.0) * x - (a + 3.0)) * x * x + 1.0;
        let y = 1.0 - x;
        let c2 = ((a + 2.0) * y - (a + 3.0)) * y * y + 1.0;
        [c0, c1, c2, 1.0 - c0 - c1 - c2]
    }

    let (sw, sh) = (src.width, src.height);
    let scale_x = 1.0 / (dw as f64 / sw as f64);
    let scale_y = 1.0 / (dh as f64 / sh as f64);
    let mut xs = vec![[0usize; 4]; dw];
    let mut xweights = vec![[0.0; 4]; dw];
    for (dx, (indices, coeffs)) in xs.iter_mut().zip(&mut xweights).enumerate() {
        let x = (dx as f64 + 0.5) * scale_x - 0.5;
        let sx = x.floor() as isize;
        *coeffs = weights(x - sx as f64);
        for (k, index) in indices.iter_mut().enumerate() {
            *index = (sx + k as isize - 1).clamp(0, sw as isize - 1) as usize;
        }
    }

    let mut horizontal = vec![0.0f64; 4 * dw * 3];
    let mut cached_rows = [None; 4];
    let mut out = BgrImage::zeroed(dw, dh);
    for dy in 0..dh {
        let y = (dy as f64 + 0.5) * scale_y - 0.5;
        let sy = y.floor() as isize;
        let coeffs = weights(y - sy as f64);
        let indices = [
            (sy - 1).clamp(0, sh as isize - 1) as usize,
            sy.clamp(0, sh as isize - 1) as usize,
            (sy + 1).clamp(0, sh as isize - 1) as usize,
            (sy + 2).clamp(0, sh as isize - 1) as usize,
        ];
        for &src_y in &indices {
            let slot = src_y % 4;
            if cached_rows[slot] == Some(src_y) {
                continue;
            }
            let row = &mut horizontal[slot * dw * 3..(slot + 1) * dw * 3];
            for dx in 0..dw {
                let indices = xs[dx];
                let coeffs = xweights[dx];
                for c in 0..3 {
                    let sample = |k: usize| f64::from(src.data[(src_y * sw + indices[k]) * 3 + c]);
                    row[dx * 3 + c] = sample(0) * coeffs[0]
                        + sample(1) * coeffs[1]
                        + sample(2) * coeffs[2]
                        + sample(3) * coeffs[3];
                }
            }
            cached_rows[slot] = Some(src_y);
        }
        for dx in 0..dw {
            for c in 0..3 {
                let sample = |k: usize| horizontal[((indices[k] % 4) * dw + dx) * 3 + c];
                let value = sample(0) * coeffs[0]
                    + sample(1) * coeffs[1]
                    + sample(2) * coeffs[2]
                    + sample(3) * coeffs[3];
                out.data[(dy * dw + dx) * 3 + c] = value.round_ties_even().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// `cv2.resize` 通用定点路径（`hal::resize` → `resizeGeneric_`），8uC3 专用。
///
/// 坐标映射（半像素中心，`resize.cpp:4045`）：
/// `fx = float((dx + 0.5) * (src_w/dst_w) - 0.5)`，`sx = floor(fx)`，
/// `fx -= sx`；线性模式在越界处夹 `fx = 0`（边缘复制），三次/Lanczos 保持
/// 原始分数交由水平采样的通道内回绕处理。`xmin`/`xmax` 标出可四点/八点
/// 直接寻址的安全区（元素域，即像素域 × 3 通道）。
// OpenCV rounds f64 pixel centers to f32 before flooring to signed offsets;
// replacing these conversions changes samples at half-integer boundaries.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::similar_names
)]
fn resize_impl(src: &BgrImage, dst_width: usize, dst_height: usize, interp: Interp) -> BgrImage {
    let (sw, sh) = (src.width, src.height);
    let (dw, dh) = (dst_width, dst_height);
    // IPP declines cubic images with fewer than four taps on either axis;
    // OpenCV's generic Q11 kernel handles those narrow inputs instead.
    if interp == Interp::Cubic && sw >= 4 && sh >= 4 {
        return resize_cubic_precise(src, dw, dh);
    }

    let cn = 3usize;
    let sw_elems = sw * cn;
    let dw_elems = dw * cn;
    // cv::resize 包装层：inv_scale = (double)dsize / ssize；hal 层再取倒数。
    let inv_fx = dw as f64 / sw as f64;
    let inv_fy = dh as f64 / sh as f64;
    let scale_x = 1.0 / inv_fx;
    let scale_y = 1.0 / inv_fy;

    let mut out = BgrImage::zeroed(dw, dh);

    // INTER_LINEAR 的 2× 整数倍降采样重定向到 INTER_AREA fast（resize.cpp:3955）。
    if interp == Interp::Linear {
        let iscale_x = scale_x.round_ties_even() as i64;
        let iscale_y = scale_y.round_ties_even() as i64;
        let area_fast = (scale_x - iscale_x as f64).abs() < f64::EPSILON
            && (scale_y - iscale_y as f64).abs() < f64::EPSILON;
        if area_fast && iscale_x == 2 && iscale_y == 2 {
            resize_area_fast_2x2(src, &mut out);
            return out;
        }
    }

    let ksize = interp.ksize();
    let ksize2 = ksize / 2;
    let linear = interp == Interp::Linear;

    // ── 系数构建（resize.cpp:4041-4134，fixpt = 8u）────────────────────────
    let mut xofs = vec![0i32; dw_elems];
    let mut ialpha = vec![0i16; dw_elems * ksize];
    let mut yofs = vec![0i32; dh];
    let mut ibeta = vec![0i16; dh * ksize];
    let mut xmin_e = 0usize;
    let mut xmax_e = dw_elems;
    let mut cbuf = [0f32; 8];

    for dx in 0..dw {
        let fx_d = (dx as f64 + 0.5) * scale_x - 0.5;
        let mut fx = fx_d as f32;
        let mut sx = fx.floor() as i32;
        fx -= sx as f32;
        if sx < (ksize2 as i64 - 1) as i32 {
            xmin_e = (dx + 1) * cn;
            if sx < 0 && linear {
                fx = 0.0;
                sx = 0;
            }
        }
        if sx + ksize2 as i32 >= sw as i32 {
            xmax_e = xmax_e.min(dx * cn);
            if sx >= sw as i32 - 1 && linear {
                fx = 0.0;
                sx = sw as i32 - 1;
            }
        }
        for k in 0..cn {
            xofs[dx * cn + k] = sx * cn as i32 + k as i32;
        }
        match interp {
            Interp::Linear => {
                cbuf[0] = 1.0 - fx;
                cbuf[1] = fx;
            }
            Interp::Cubic => interpolate_cubic(fx, &mut cbuf[..4]),
            Interp::Lanczos4 => interpolate_lanczos4(fx, &mut cbuf[..8]),
        }
        for k in 0..ksize {
            ialpha[dx * cn * ksize + k] = sat_short(cbuf[k] * 2048.0f32);
        }
        for k in ksize..cn * ksize {
            let v = ialpha[dx * cn * ksize + k - ksize];
            ialpha[dx * cn * ksize + k] = v;
        }
    }
    for (dy, yof) in yofs.iter_mut().enumerate() {
        let fy_d = (dy as f64 + 0.5) * scale_y - 0.5;
        let mut fy = fy_d as f32;
        let sy = fy.floor() as i32;
        fy -= sy as f32;
        *yof = sy;
        match interp {
            Interp::Linear => {
                cbuf[0] = 1.0 - fy;
                cbuf[1] = fy;
            }
            Interp::Cubic => interpolate_cubic(fy, &mut cbuf[..4]),
            Interp::Lanczos4 => interpolate_lanczos4(fy, &mut cbuf[..8]),
        }
        for k in 0..ksize {
            ibeta[dy * ksize + k] = sat_short(cbuf[k] * 2048.0f32);
        }
    }

    // ── 逐输出行：水平重采样到 ksize 行 i32 中间缓冲，再垂直混合 ──────────
    let mut rows: Vec<Vec<i32>> = vec![vec![0i32; dw_elems]; ksize];
    for dy in 0..dh {
        let sy0 = yofs[dy];
        for (k, row) in rows.iter_mut().enumerate() {
            let sy = clip(sy0 - ksize2 as i32 + 1 + k as i32, sh as i32) as usize;
            let s = &src.data[sy * sw_elems..(sy + 1) * sw_elems];
            hresize(s, row, &xofs, &ialpha, interp, sw_elems, xmin_e, xmax_e);
        }
        vresize(
            &rows,
            &mut out.data[dy * dw_elems..(dy + 1) * dw_elems],
            &ibeta[dy * ksize..],
            interp,
        );
    }
    out
}

/// 水平重采样（`HResizeLinear` / `HResizeCubic` / `HResizeLanczos4`，元素域）。
// OpenCV represents element offsets as signed i32 until border replication
// has brought them into the nonnegative source-row range.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
#[allow(clippy::too_many_arguments)]
fn hresize(
    s: &[u8],
    dst: &mut [i32],
    xofs: &[i32],
    ialpha: &[i16],
    interp: Interp,
    sw_elems: usize,
    xmin_e: usize,
    xmax_e: usize,
) {
    let cn = 3usize;
    match interp {
        Interp::Linear => {
            for (e, d) in dst.iter_mut().enumerate() {
                let sx = xofs[e] as usize;
                if e < xmax_e {
                    *d = i32::from(s[sx]) * i32::from(ialpha[e * 2])
                        + i32::from(s[sx + cn]) * i32::from(ialpha[e * 2 + 1]);
                } else {
                    *d = i32::from(s[sx]) * INTER_RESIZE_COEF_SCALE;
                }
            }
        }
        Interp::Cubic => {
            for (e, d) in dst.iter_mut().enumerate() {
                let a = &ialpha[e * 4..e * 4 + 4];
                if e < xmin_e || e >= xmax_e {
                    let base = xofs[e] - cn as i32;
                    let mut v = 0i32;
                    for (j, aj) in a.iter().enumerate() {
                        let mut sxj = base + j as i32 * cn as i32;
                        if sxj < 0 || sxj >= sw_elems as i32 {
                            while sxj < 0 {
                                sxj += cn as i32;
                            }
                            while sxj >= sw_elems as i32 {
                                sxj -= cn as i32;
                            }
                        }
                        v += i32::from(s[sxj as usize]) * i32::from(*aj);
                    }
                    *d = v;
                } else {
                    let sx = xofs[e] as usize;
                    *d = i32::from(s[sx - cn]) * i32::from(a[0])
                        + i32::from(s[sx]) * i32::from(a[1])
                        + i32::from(s[sx + cn]) * i32::from(a[2])
                        + i32::from(s[sx + 2 * cn]) * i32::from(a[3]);
                }
            }
        }
        Interp::Lanczos4 => {
            for (e, d) in dst.iter_mut().enumerate() {
                let a = &ialpha[e * 8..e * 8 + 8];
                if e < xmin_e || e >= xmax_e {
                    let base = xofs[e] - 3 * cn as i32;
                    let mut v = 0i32;
                    for (j, aj) in a.iter().enumerate() {
                        let mut sxj = base + j as i32 * cn as i32;
                        if sxj < 0 || sxj >= sw_elems as i32 {
                            while sxj < 0 {
                                sxj += cn as i32;
                            }
                            while sxj >= sw_elems as i32 {
                                sxj -= cn as i32;
                            }
                        }
                        v += i32::from(s[sxj as usize]) * i32::from(*aj);
                    }
                    *d = v;
                } else {
                    let sx = xofs[e] as usize;
                    *d = i32::from(s[sx - 3 * cn]) * i32::from(a[0])
                        + i32::from(s[sx - 2 * cn]) * i32::from(a[1])
                        + i32::from(s[sx - cn]) * i32::from(a[2])
                        + i32::from(s[sx]) * i32::from(a[3])
                        + i32::from(s[sx + cn]) * i32::from(a[4])
                        + i32::from(s[sx + 2 * cn]) * i32::from(a[5])
                        + i32::from(s[sx + 3 * cn]) * i32::from(a[6])
                        + i32::from(s[sx + 4 * cn]) * i32::from(a[7]);
                }
            }
        }
    }
}

/// 垂直混合（8u 各模式的 `VResize*` 特化，元素域宽度 `dst.len()`）。
// SSE2's cubic vertical path narrows signed intermediate sums to f32.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn vresize(rows: &[Vec<i32>], dst: &mut [u8], beta: &[i16], interp: Interp) {
    let width = dst.len();
    match interp {
        // VResizeLinear<uchar,int,short,...>：((b0*(S0>>4)>>16)+(b1*(S1>>4)>>16)+2)>>2。
        Interp::Linear => {
            let b0 = i32::from(beta[0]);
            let b1 = i32::from(beta[1]);
            let (r0, r1) = (&rows[0], &rows[1]);
            for (e, d) in dst.iter_mut().enumerate() {
                let t = ((b0 * (r0[e] >> 4)) >> 16) + ((b1 * (r1[e] >> 4)) >> 16) + 2;
                *d = (t >> 2) as u8;
            }
        }
        // VResizeCubicVec_32s8u（SSE2 基线，8 元素步进）前段 f32（求和序按
        // 向量表达式自内向外），尾段 FixedPtCast<int,uchar,22>。
        Interp::Cubic => {
            let scale = 1.0f32 / (2048.0f32 * 2048.0f32);
            let b = [
                f32::from(beta[0]) * scale,
                f32::from(beta[1]) * scale,
                f32::from(beta[2]) * scale,
                f32::from(beta[3]) * scale,
            ];
            let mut e = 0usize;
            while e + 8 <= width {
                for lane in 0..8 {
                    let s0 = rows[0][e + lane] as f32;
                    let s1 = rows[1][e + lane] as f32;
                    let s2 = rows[2][e + lane] as f32;
                    let s3 = rows[3][e + lane] as f32;
                    let t = s3 * b[3];
                    let t = s2 * b[2] + t;
                    let t = s1 * b[1] + t;
                    let t = s0 * b[0] + t;
                    dst[e + lane] = sat_u8(t.round_ties_even() as i32);
                }
                e += 8;
            }
            for (i, d) in dst.iter_mut().enumerate().skip(e) {
                let sum = rows[0][i] * i32::from(beta[0])
                    + rows[1][i] * i32::from(beta[1])
                    + rows[2][i] * i32::from(beta[2])
                    + rows[3][i] * i32::from(beta[3]);
                *d = sat_u8((sum + (1 << 21)) >> 22);
            }
        }
        // VResizeLanczos4<uchar,...>（8u 无向量实现）：Q22 定点左结合。
        Interp::Lanczos4 => {
            for (e, d) in dst.iter_mut().enumerate() {
                let mut sum = rows[0][e] * i32::from(beta[0]);
                for (k, row) in rows.iter().enumerate().skip(1) {
                    sum += row[e] * i32::from(beta[k]);
                }
                *d = sat_u8((sum + (1 << 21)) >> 22);
            }
        }
    }
}

/// INTER_AREA fast（恰为 2×2 整数降采样，`resizeAreaFast_` 8u cn=3）。
///
/// 完整列（源宽一半）用 `(a+b+c+d+2)>>2`；右/下边缘列按可得像素求均值
/// （`(float)sum/count` 半偶取整），越界列输出 0。
// OpenCV uses signed coordinates and a rounded f32 edge average here.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn resize_area_fast_2x2(src: &BgrImage, out: &mut BgrImage) {
    let (sw, sh) = (src.width as i32, src.height as i32);
    let (dw, dh) = (out.width, out.height);
    let cn = 3usize;
    let dwidth1 = (sw / 2) as usize * cn;
    let step = sw as usize * cn;
    for dy in 0..dh {
        let sy0 = (dy * 2) as i32;
        let dst_row = &mut out.data[dy * dw * cn..(dy + 1) * dw * cn];
        let full_w = if sy0 + 2 <= sh { dwidth1 } else { 0 };
        let s0 = sy0 as usize;
        let s1 = ((sy0 + 1) as usize).min((sh as usize).saturating_sub(1));
        for (e, d) in dst_row.iter_mut().enumerate().take(full_w) {
            let px = e / cn;
            let k = e % cn;
            let idx = 6 * px + k; // xofs[e] = 2*(3*px) + k
            let sum = i32::from(src.data[s0 * step + idx])
                + i32::from(src.data[s0 * step + idx + cn])
                + i32::from(src.data[s1 * step + idx])
                + i32::from(src.data[s1 * step + idx + cn]);
            *d = ((sum + 2) >> 2) as u8;
        }
        for (e, d) in dst_row.iter_mut().enumerate().skip(full_w) {
            let sx0 = 2 * (e - e % cn) + e % cn;
            if sx0 >= step {
                *d = 0;
                continue;
            }
            let mut sum = 0i32;
            let mut count = 0i32;
            for sy in 0..2 {
                let yy = sy0 + sy;
                if yy >= sh {
                    break;
                }
                let row = &src.data[yy as usize * step..(yy as usize + 1) * step];
                for sx in (0..2 * cn).step_by(cn) {
                    if sx0 + sx >= step {
                        break;
                    }
                    sum += i32::from(row[sx0 + sx]);
                    count += 1;
                }
            }
            if count > 0 {
                *d = sat_u8((sum as f32 / count as f32).round_ties_even() as i32);
            }
        }
    }
}

/// INTER_LINEAR 缩放（与 `cv2.resize` 默认插值逐位一致，含 2×2 面积重定向）。
#[must_use]
pub fn resize_inter_linear(img: &BgrImage, dst_width: usize, dst_height: usize) -> BgrImage {
    resize_impl(img, dst_width, dst_height, Interp::Linear)
}

/// INTER_CUBIC 缩放（A = -0.75，半像素中心；TextSnap 小裁剪 2× 增强所用）。
#[must_use]
pub fn resize_inter_cubic(img: &BgrImage, dst_width: usize, dst_height: usize) -> BgrImage {
    resize_impl(img, dst_width, dst_height, Interp::Cubic)
}

/// INTER_LANCZOS4 缩放（a = 4；TextSnap 密集代码行 1.5× 横向拉伸所用）。
#[must_use]
pub fn resize_inter_lanczos4(img: &BgrImage, dst_width: usize, dst_height: usize) -> BgrImage {
    resize_impl(img, dst_width, dst_height, Interp::Lanczos4)
}

/// `remapBicubic` 的 Q15 权重表（`initInterTab2D(INTER_CUBIC, fixpt)`）。
///
/// 表项 `(ay*32 + ax)*16 + r*4 + c` 为
/// `sat_short(interpolateCubic(ay/32)[r] * interpolateCubic(ax/32)[c] * 2^15)`，
/// 且当 16 项之和 ≠ 32768 时对索引 {2,3}² 子块中的最大/最小项补偿差值。
// The compensated Q15 table writes corrected weights back as signed shorts.
#[allow(clippy::cast_possible_truncation)]
fn bicubic_remap_tab() -> &'static [i16] {
    static TAB: LazyLock<Vec<i16>> = LazyLock::new(|| {
        let mut tab1d = [0f32; 32 * 4];
        let scale = 1.0f32 / 32.0;
        for (i, chunk) in tab1d.chunks_mut(4).enumerate() {
            // tab1d 固定 32*4，enumerate 只产生 0..32 的下标。
            interpolate_cubic(f32::from(i as u8) * scale, chunk);
        }
        let target = 32_768i32;
        let mut itab = vec![0i16; 1024 * 16];
        for i in 0..32usize {
            for j in 0..32usize {
                let base = (i * 32 + j) * 16;
                let mut isum = 0i32;
                for k1 in 0..4usize {
                    let vy = tab1d[i * 4 + k1];
                    for k2 in 0..4usize {
                        let v = vy * tab1d[j * 4 + k2];
                        let q = sat_short(v * INTER_REMAP_COEF_SCALE);
                        itab[base + k1 * 4 + k2] = q;
                        isum += i32::from(q);
                    }
                }
                if isum != target {
                    let diff = isum - target;
                    let (mut lo1, mut lo2, mut hi1, mut hi2) = (2usize, 2usize, 2usize, 2usize);
                    for k1 in 2..4usize {
                        for k2 in 2..4usize {
                            let cur = itab[base + k1 * 4 + k2];
                            if cur < itab[base + lo1 * 4 + lo2] {
                                lo1 = k1;
                                lo2 = k2;
                            } else if cur > itab[base + hi1 * 4 + hi2] {
                                hi1 = k1;
                                hi2 = k2;
                            }
                        }
                    }
                    if diff < 0 {
                        let idx = base + hi1 * 4 + hi2;
                        itab[idx] = (i32::from(itab[idx]) - diff) as i16;
                    } else {
                        let idx = base + lo1 * 4 + lo2;
                        itab[idx] = (i32::from(itab[idx]) - diff) as i16;
                    }
                }
            }
        }
        itab
    });
    &TAB
}

/// 透视裁剪校验（`ocr.py:168-231`，与 `snap-ocr-core::pipeline::validate_perspective`
/// 逐位一致），返回 `(output_width, output_height)`。
// Python returns f64 dimensions here; both are positive integral values after rounding.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn validate_perspective(points_f64: &[(f64, f64); 4]) -> Result<(usize, usize), ImageOpError> {
    // numpy.linalg.norm 的平方和开方（不用 hypot），round 为 Python 半偶。
    #[allow(clippy::suboptimal_flops)]
    fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
        ((b.0 - a.0) * (b.0 - a.0) + (b.1 - a.1) * (b.1 - a.1)).sqrt()
    }
    for (x, y) in points_f64 {
        if !x.is_finite() || !y.is_finite() {
            return Err(ImageOpError::InvalidDetectionQuad);
        }
    }
    for i in 0..4 {
        for j in (i + 1)..4 {
            if points_f64[i] == points_f64[j] {
                return Err(ImageOpError::DegenerateDetectionQuad);
            }
        }
    }
    let mut crosses = [0.0f64; 4];
    for (index, cross) in crosses.iter_mut().enumerate() {
        let p0 = points_f64[index];
        let p1 = points_f64[(index + 1) % 4];
        let p2 = points_f64[(index + 2) % 4];
        let e0 = (p1.0 - p0.0, p1.1 - p0.1);
        let e1 = (p2.0 - p1.0, p2.1 - p1.1);
        *cross = e0.0 * e1.1 - e0.1 * e1.0;
    }
    if !(crosses.iter().all(|c| *c > 0.0) || crosses.iter().all(|c| *c < 0.0)) {
        return Err(ImageOpError::InvalidDetectionQuad);
    }
    #[allow(clippy::float_cmp)]
    if (0..4)
        .map(|index| {
            let p0 = points_f64[index];
            let p1 = points_f64[(index + 1) % 4];
            p0.0 * p1.1 - p0.1 * p1.0
        })
        .sum::<f64>()
        == 0.0
    {
        return Err(ImageOpError::DegenerateDetectionQuad);
    }
    let (tl, tr, br, bl) = (points_f64[0], points_f64[1], points_f64[2], points_f64[3]);
    let w = distance(tl, tr).max(distance(bl, br)).round_ties_even();
    let h = distance(tl, bl).max(distance(tr, br)).round_ties_even();
    if w < 2.0 || h < 2.0 {
        return Err(ImageOpError::DegenerateDetectionQuad);
    }
    Ok((w as usize, h as usize))
}

/// `cv::getPerspectiveTransform`（`imgwarp.cpp:3509`，DECOMP_LU → 8×8
/// `LUImpl` double，`DBL_EPSILON*100` 奇异阈）。
///
/// 输入点先按 TextSnap 语义窄化 f32（`numpy.float32`）；乘积 `-src.x*dst.x`
/// 在 f32 域舍入后再拓宽为 f64（C++ `float*float`）。
fn get_perspective_transform(src: &[(f32, f32); 4], dst: &[(f32, f32); 4]) -> Option<[f64; 9]> {
    let mut a = [0.0f64; 64];
    let mut b = [0.0f64; 8];
    for i in 0..4 {
        let (sx, sy) = src[i];
        let (dx, dy) = dst[i];
        a[i * 8] = f64::from(sx);
        a[(i + 4) * 8 + 3] = f64::from(sx);
        a[i * 8 + 1] = f64::from(sy);
        a[(i + 4) * 8 + 4] = f64::from(sy);
        a[i * 8 + 2] = 1.0;
        a[(i + 4) * 8 + 5] = 1.0;
        a[i * 8 + 6] = f64::from(-sx * dx);
        a[i * 8 + 7] = f64::from(-sy * dx);
        a[(i + 4) * 8 + 6] = f64::from(-sx * dy);
        a[(i + 4) * 8 + 7] = f64::from(-sy * dy);
        b[i] = f64::from(dx);
        b[i + 4] = f64::from(dy);
    }
    if !lu_solve8(&mut a, &mut b) {
        return None;
    }
    let mut m = [0.0f64; 9];
    m[..8].copy_from_slice(&b);
    m[8] = 1.0;
    Some(m)
}

/// `LUImpl`（`matrix_decomp.cpp`，double，eps = `DBL_EPSILON*100`）的 8×8
/// 单右端复刻：列主元（严格大于）、行交换、消元只更新 `i+1..m` 列，
/// 回代逐行除以对角元。
// The matrix/vector and short loop indices follow OpenCV's LU convention.
#[allow(clippy::many_single_char_names)]
fn lu_solve8(a: &mut [f64; 64], b: &mut [f64; 8]) -> bool {
    let m = 8usize;
    let eps = f64::EPSILON * 100.0;
    for i in 0..m {
        let mut k = i;
        for j in i + 1..m {
            if a[j * m + i].abs() > a[k * m + i].abs() {
                k = j;
            }
        }
        if a[k * m + i].abs() < eps {
            return false;
        }
        if k != i {
            for j in i..m {
                a.swap(i * m + j, k * m + j);
            }
            b.swap(i, k);
        }
        let d = -1.0 / a[i * m + i];
        for j in i + 1..m {
            let alpha = a[j * m + i] * d;
            for kk in i + 1..m {
                a[j * m + kk] += alpha * a[i * m + kk];
            }
            b[j] += alpha * b[i];
        }
    }
    for i in (0..m).rev() {
        let mut s = b[i];
        for k in i + 1..m {
            s -= a[i * m + k] * b[k];
        }
        b[i] = s / a[i * m + i];
    }
    true
}

/// `cv::invert` 3×3 CV_64F DECOMP_LU（`lapack.cpp:944`）：det3 + 伴随公式。
/// 奇异（det == 0）时按 OpenCV 语义返回全零矩阵。
fn invert3x3(m: &[f64; 9]) -> [f64; 9] {
    let det = m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6])
        + m[2] * (m[3] * m[7] - m[4] * m[6]);
    if det == 0.0 {
        return [0.0; 9];
    }
    let d = 1.0 / det;
    [
        (m[4] * m[8] - m[5] * m[7]) * d,
        (m[2] * m[7] - m[1] * m[8]) * d,
        (m[1] * m[5] - m[2] * m[4]) * d,
        (m[5] * m[6] - m[3] * m[8]) * d,
        (m[0] * m[8] - m[2] * m[6]) * d,
        (m[2] * m[3] - m[0] * m[5]) * d,
        (m[3] * m[7] - m[4] * m[6]) * d,
        (m[1] * m[6] - m[0] * m[7]) * d,
        (m[0] * m[4] - m[1] * m[3]) * d,
    ]
}

/// 透视裁剪（`cv2.warpPerspective` INTER_CUBIC + BORDER_REPLICATE，8uC3）。
///
/// 与 TextSnap `ocr.py:168-231` 相同的调用形态：四边形（TL, TR, BR, BL）先
/// 窄化 f32，校验凸性/面积后按上/下边与左/右边最大长度（半偶取整）定输出
/// 尺寸，目标角点为 `(0,0),(w-1,0),(w-1,h-1),(0,h-1)`；随后用
/// `getPerspectiveTransform` → `invert` 得到的逆矩阵做块状（64×16）双精度
/// 逆映射 + Q15 双三次采样（`remapBicubic` + `BORDER_REPLICATE`）。
///
/// # Errors
/// 校验失败返回 [`ImageOpError::InvalidDetectionQuad`] /
/// [`ImageOpError::DegenerateDetectionQuad`]。
// Preserve numpy's f64→f32 vertices and OpenCV's signed fixed-point warp
// coordinates; index casts follow the same border clipping as remapBicubic.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
pub fn warp_perspective_cubic_replicate(
    img: &BgrImage,
    quad: &[(f64, f64); 4],
) -> Result<BgrImage, ImageOpError> {
    let pts32 = [
        (quad[0].0 as f32, quad[0].1 as f32),
        (quad[1].0 as f32, quad[1].1 as f32),
        (quad[2].0 as f32, quad[2].1 as f32),
        (quad[3].0 as f32, quad[3].1 as f32),
    ];
    let pts64 = [
        (f64::from(pts32[0].0), f64::from(pts32[0].1)),
        (f64::from(pts32[1].0), f64::from(pts32[1].1)),
        (f64::from(pts32[2].0), f64::from(pts32[2].1)),
        (f64::from(pts32[3].0), f64::from(pts32[3].1)),
    ];
    let (dw, dh) = validate_perspective(&pts64)?;
    let dst = [
        (0.0f32, 0.0f32),
        ((dw - 1) as f32, 0.0f32),
        ((dw - 1) as f32, (dh - 1) as f32),
        (0.0f32, (dh - 1) as f32),
    ];
    let forward =
        get_perspective_transform(&pts32, &dst).ok_or(ImageOpError::DegenerateDetectionQuad)?;
    let m = invert3x3(&forward);

    let sw = img.width as i32;
    let sh = img.height as i32;
    let sw_elems = img.width * 3;
    let width1 = (sw - 3).max(0);
    let height1 = (sh - 3).max(0);
    let tab = bicubic_remap_tab();
    let mut out = BgrImage::zeroed(dw, dh);

    // 块几何（WarpPerspectiveInvoker）：bh0 = min(16, h) → bw0 = min(1024/bh0, w)
    // → bh0 = min(1024/bw0, h)。X0 的求值分组依赖块原点，须逐一复刻。
    let bh0_init = 16usize.min(dh);
    let bw0 = (1024 / bh0_init).min(dw);
    let bh0 = (1024 / bw0).min(dh);

    let (m0, m1, m2, m3, m4, m5, m6, m7, m8) =
        (m[0], m[1], m[2], m[3], m[4], m[5], m[6], m[7], m[8]);

    let mut y = 0usize;
    while y < dh {
        let mut x = 0usize;
        while x < dw {
            let bw = bw0.min(dw - x);
            let bh = bh0.min(dh - y);
            for y1 in 0..bh {
                let yy = (y + y1) as f64;
                let xf = x as f64;
                let x0 = m0 * xf + m1 * yy + m2;
                let y0 = m3 * xf + m4 * yy + m5;
                let w0 = m6 * xf + m7 * yy + m8;
                let out_row = &mut out.data[((y + y1) * dw + x) * 3..((y + y1) * dw + x + bw) * 3];
                for x1 in 0..bw {
                    let mut w = w0 + m6 * x1 as f64;
                    w = if w == 0.0 {
                        0.0
                    } else {
                        f64::from(INTER_TAB_SIZE) / w
                    };
                    let fx = (x0 + m0 * x1 as f64) * w;
                    let fy = (y0 + m3 * x1 as f64) * w;
                    let fxc = fx.clamp(f64::from(i32::MIN), f64::from(i32::MAX));
                    let fyc = fy.clamp(f64::from(i32::MIN), f64::from(i32::MAX));
                    let xi = fxc.round_ties_even() as i32;
                    let yi = fyc.round_ties_even() as i32;
                    let sx = (xi >> INTER_BITS).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) - 1;
                    let sy = (yi >> INTER_BITS).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) - 1;
                    let alpha = ((yi & (INTER_TAB_SIZE - 1)) * INTER_TAB_SIZE
                        + (xi & (INTER_TAB_SIZE - 1))) as usize;
                    let wtab = &tab[alpha * 16..alpha * 16 + 16];
                    for k in 0..3usize {
                        let sum = if (0..width1).contains(&sx) && (0..height1).contains(&sy) {
                            let base = sy as usize * sw_elems + sx as usize * 3 + k;
                            let mut acc = 0i32;
                            for r in 0..4usize {
                                for c in 0..4usize {
                                    acc += i32::from(img.data[base + r * sw_elems + c * 3])
                                        * i32::from(wtab[r * 4 + c]);
                                }
                            }
                            acc
                        } else {
                            let mut acc = 0i32;
                            for r in 0..4usize {
                                let yr = clip(sy + r as i32, sh);
                                for c in 0..4usize {
                                    let xc = clip(sx + c as i32, sw);
                                    acc += i32::from(
                                        img.data[yr as usize * sw_elems + xc as usize * 3 + k],
                                    ) * i32::from(wtab[r * 4 + c]);
                                }
                            }
                            acc
                        };
                        out_row[x1 * 3 + k] = sat_u8((sum + 16_384) >> 15);
                    }
                }
            }
            x += bw0;
        }
        y += bh0;
    }
    Ok(out)
}

/// 逆时针旋转 90° 的 `quarters` 倍（`numpy.rot90`，`ocr.py:272-275`）。
///
/// # Errors
/// `quarters` 不在 1..=3 时返回 [`ImageOpError::InvalidRotation`]。
pub fn rot90(img: &BgrImage, quarters: u32) -> Result<BgrImage, ImageOpError> {
    if quarters == 0 || quarters > 3 {
        return Err(ImageOpError::InvalidRotation(quarters * 90));
    }
    let (w, h) = (img.width, img.height);
    let (nw, nh) = if quarters % 2 == 1 { (h, w) } else { (w, h) };
    let mut out = BgrImage::zeroed(nw, nh);
    for y in 0..h {
        for x in 0..w {
            let px = img.pixel(y, x);
            let (tx, ty) = match quarters {
                // rot90(m, 1)[i][j] = m[j][w-1-i]。
                1 => (y, w - 1 - x),
                2 => (w - 1 - x, h - 1 - y),
                _ => (h - 1 - y, x),
            };
            let base = (ty * nw + tx) * 3;
            out.data[base..base + 3].copy_from_slice(&px);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    /// 固定 3×2 图（非随机）：便于手算。
    fn tiny() -> BgrImage {
        BgrImage::from_vec(
            3,
            2,
            vec![
                10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160, 170, 180,
            ],
        )
        .unwrap()
    }

    #[test]
    fn buffer_length_is_validated() {
        assert!(BgrImage::from_vec(2, 2, vec![0; 11]).is_err());
        assert!(BgrImage::from_vec(2, 2, vec![0; 12]).is_ok());
    }

    #[test]
    fn resize_identity_returns_same_pixels_for_all_interpolations() {
        let img = tiny();
        let src = resize_inter_cubic(&img, 5, 4);
        assert_eq!(resize_inter_linear(&src, 5, 4).data(), src.data());
        assert_eq!(resize_inter_cubic(&src, 5, 4).data(), src.data());
        assert_eq!(resize_inter_lanczos4(&src, 5, 4).data(), src.data());
    }

    #[test]
    fn resize_linear_upscale_hand_value() {
        // 2×1 → 4×2，源左黑右白。scale_x = scale_y = 0.5（半像素中心）：
        // fx(dx) = (dx+0.5)*0.5-0.5 → dx=0: -0.25（sx=-1 <0 线性夹 (0, fx=0)），
        // dx=1: 0.25 → 权重 (1536,512)，dx=2: 0.75 → (512,1536)，
        // dx=3: 1.25 → sx=1=sw-1 → 夹 (1, fx=0)。
        // 垂直同构（单行源复制）。dx=1 手算：水平 S = 0*1536 + 255*512 = 130560；
        // 垂直 b=(1536,512)：(1536*8160)>>16 = 191，(512*8160)>>16 = 63，
        // (191+63+2)>>2 = 64（真值 63.75 → 就近舍入 64）。
        let src = BgrImage::from_vec(2, 1, vec![0, 0, 0, 255, 255, 255]).unwrap();
        let out = resize_inter_linear(&src, 4, 2);
        assert_eq!((out.width(), out.height()), (4, 2));
        assert_eq!(out.pixel(0, 0), [0, 0, 0]);
        assert_eq!(out.pixel(0, 1), [64, 64, 64]);
        assert_eq!(out.pixel(1, 1), [64, 64, 64]);
        assert_eq!(out.pixel(0, 2), [191, 191, 191]);
        assert_eq!(out.pixel(0, 3), [255, 255, 255]);
    }

    #[test]
    fn resize_linear_downscale_2x_area_fast() {
        // 4×2 → 2×1：两轴恰为整数 2× → INTER_AREA fast（(a+b+c+d+2)>>2）。
        let src = BgrImage::from_vec(
            4,
            2,
            vec![
                0, 0, 0, 10, 10, 10, 20, 20, 20, 30, 30, 30, 40, 40, 40, 50, 50, 50, 60, 60, 60,
                70, 70, 70,
            ],
        )
        .unwrap();
        let out = resize_inter_linear(&src, 2, 1);
        // dst(0,0) ← 源 2×2 块 (0,10,40,50)：均值 25。
        assert_eq!(out.pixel(0, 0), [25, 25, 25]);
        // dst(1,0) ← (20,30,60,70)：均值 45。
        assert_eq!(out.pixel(0, 1), [45, 45, 45]);
    }

    #[test]
    fn resize_lanczos4_kernel_is_normalized_at_integers() {
        let mut c = [0f32; 8];
        interpolate_lanczos4(0.0, &mut c);
        assert!((c[3] - 1.0).abs() < 1e-6);
        assert!(c.iter().enumerate().all(|(i, v)| i == 3 || v.abs() < 1e-6));
    }

    #[test]
    fn warp_axis_aligned_corners_sample_exact_pixels() {
        // 轴对齐矩形 (1,1)-(5,1)-(5,4)-(1,4)：边长 4×3 → 输出 4×3。
        // 角点逆映射到整数源坐标（子像素 α=0，双三次权重为 δ，
        // 即 [0,0,1,0]²）→ 四角逐字节等于源角像素。
        // 注：TextSnap 语义下输出尺寸 = round(边长)，目标角在 (w-1,h-1)，
        // 故「输出 == 源图」的严格恒等不可构造（输出总比四边形小 1）。
        // 6×5 确定性图：像素 (y,x) = (y*37 + x*11, y*37 + x*11 + 1, y*37 + x*11 + 2) mod 256。
        let mut data = Vec::with_capacity(6 * 5 * 3);
        for y in 0..5u32 {
            for x in 0..6u32 {
                let base = (y * 37 + x * 11) % 256;
                data.extend_from_slice(&[
                    u8::try_from(base).unwrap(),
                    u8::try_from(base + 1).unwrap(),
                    u8::try_from(base + 2).unwrap(),
                ]);
            }
        }
        let img = BgrImage::from_vec(6, 5, data).unwrap();
        let quad = [(1.0f64, 1.0), (5.0, 1.0), (5.0, 4.0), (1.0, 4.0)];
        let out = warp_perspective_cubic_replicate(&img, &quad).unwrap();
        assert_eq!((out.width(), out.height()), (4, 3));
        assert_eq!(out.pixel(0, 0), img.pixel(1, 1)); // ← TL
        assert_eq!(out.pixel(0, 3), img.pixel(1, 5)); // ← TR
        assert_eq!(out.pixel(2, 0), img.pixel(4, 1)); // ← BL
        assert_eq!(out.pixel(2, 3), img.pixel(4, 5)); // ← BR
    }

    #[test]
    fn warp_rejects_invalid_quads() {
        let img = tiny();
        let dup = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)];
        assert_eq!(
            warp_perspective_cubic_replicate(&img, &dup).unwrap_err(),
            ImageOpError::DegenerateDetectionQuad
        );
        let concave = [(0.0, 0.0), (4.0, 0.0), (1.0, 1.0), (0.0, 4.0)];
        assert_eq!(
            warp_perspective_cubic_replicate(&img, &concave).unwrap_err(),
            ImageOpError::InvalidDetectionQuad
        );
        let nan = [(0.0, 0.0), (1.0, 0.0), (1.0, f64::NAN), (0.0, 1.0)];
        assert_eq!(
            warp_perspective_cubic_replicate(&img, &nan).unwrap_err(),
            ImageOpError::InvalidDetectionQuad
        );
    }

    #[test]
    fn rot90_roundtrip_and_orientation() {
        let img = tiny(); // 3×2
        let r1 = rot90(&img, 1).unwrap();
        assert_eq!((r1.width(), r1.height()), (2, 3));
        // rot90(m,1)[i][j] = m[j][w-1-i]：目标 (0,0) ← 源 (0,2) = (70,80,90)。
        assert_eq!(r1.pixel(0, 0), [70, 80, 90]);
        // 目标 (2,1) ← 源 (1,0) = (100,110,120)。
        assert_eq!(r1.pixel(2, 1), [100, 110, 120]);
        let back = rot90(
            &rot90(&rot90(&rot90(&img, 1).unwrap(), 1).unwrap(), 1).unwrap(),
            1,
        )
        .unwrap();
        assert_eq!(back.data(), img.data());
        for quarters in 2..=3 {
            let twice = rot90(&rot90(&img, quarters).unwrap(), 4 - quarters).unwrap();
            assert_eq!(twice.data(), img.data());
        }
        assert!(rot90(&img, 0).is_err());
        assert!(rot90(&img, 4).is_err());
    }
}
