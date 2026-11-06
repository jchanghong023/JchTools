//! 截图复制的独立内存处理（O-39/O-40/O-43），不写文件、不记录图像内容。
//!
//! 速度优先：先尝试原尺寸快速 RGB PNG；超限后仅补一次原色索引 PNG，避免文字图无谓缩小。
//! 仍超限时按实际编码长度与字节预算估算新尺寸，使用 Triangle 从原图等比缩放，不逐档搜索或减色。
//! 最多 3 次预算估算，随后最多 32 次对半缩小，最终尝试 1×1；最多 36 档尺寸、37 次编码。
//! 只在 1×1 的最终尺寸使用高等级 RGB 编码，避免快速编码封装开销误拒绝极小有效 PNG。
//! 原图 RGB、至多一档缩图及复用的 PNG 输出常驻内存；原色索引缓冲只在原尺寸需要时创建。

#![cfg(windows)]

use std::collections::HashMap;

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::imageops::{resize, FilterType as ResizeFilter};
use image::{ExtendedColorType, ImageEncoder, RgbImage};

use crate::capture_win::BgrImage;

/// 已按精确字节上限校验的完整 PNG 及其实际像素尺寸。
pub struct CompressedImage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// 与最终 PNG 完全一致的 RGB 像素，直接用于标准图片剪贴板，避免解码回读。
    pub(crate) rgb: Vec<u8>,
}

impl CompressedImage {
    /// 最终 PNG 的实际编码字节数，不是解码位图或剪贴板总大小。
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.png.len()
    }
}

/// 只处理当前任务的内存图像，成功时最终编码长度必定不超过 `max_bytes`。
///
/// 原尺寸快速编码达标即返回；超限后按预算跳跃缩图，允许文字变糊，不搜索最高颜色精度。
/// 每次编码、索引扫描及缩图前后检查取消；不发布已取消任务的迟到结果。
///
/// # Errors
/// 取消、无效尺寸、PNG 编码失败或上限无法容纳最小 PNG 时返回中文说明。
pub fn compress(
    image: &BgrImage,
    max_bytes: usize,
    cancelled: impl Fn() -> bool + Sync,
) -> Result<CompressedImage, String> {
    check_cancelled(&cancelled)?;
    if max_bytes == 0 {
        return Err("图片大小上限必须大于零".into());
    }
    let width = u32::try_from(image.width()).map_err(|_| "图像宽度超出 PNG 上限".to_string())?;
    let height = u32::try_from(image.height()).map_err(|_| "图像高度超出 PNG 上限".to_string())?;
    if width == 0 || height == 0 {
        return Err("截图尺寸必须大于零".into());
    }
    // 只复制一次原始像素用于 BGR → RGB；不会原地减色或改写 OCR 输入。
    let rgb = {
        jchtools_foundation::perf::perf_span!("snap_bgr_to_rgb");
        let mut rgb = image.bgr_pixels().to_vec();
        for pixel in rgb.as_chunks_mut::<3>().0 {
            pixel.swap(0, 2);
        }
        rgb
    };
    let original =
        RgbImage::from_raw(width, height, rgb).ok_or_else(|| "图像缓冲与尺寸不符".to_string())?;
    let mut png = Vec::new();
    let original_fits = {
        jchtools_foundation::perf::perf_span!("snap_image_original_rgb_png");
        encode_rgb(&original, &mut png, max_bytes, &cancelled)?
    };
    if original_fits {
        return finish_candidate(original, png, &cancelled);
    }
    let mut encoded_bytes = png.len();
    if encode_indexed(&original, &mut png, max_bytes, &cancelled)? {
        return finish_candidate(original, png, &cancelled);
    }

    let mut dimensions = original.dimensions();
    for attempt in 0..(3 + u32::BITS) {
        check_cancelled(&cancelled)?;
        if dimensions == (1, 1) {
            break;
        }
        dimensions = next_dimensions(
            original.dimensions(),
            dimensions.0.max(dimensions.1),
            encoded_bytes,
            max_bytes,
            attempt < 3,
        );
        // 候选次数由既有算法严格限制；仅逐候选打点，不逐像素打点。
        #[cfg(feature = "perf-tracing")]
        let _candidate_perf_guard = tracing::info_span!(
            target: "perf",
            "snap_image_scaled_candidate",
            attempt = attempt + 1,
            budget_estimate = attempt < 3,
            width = dimensions.0,
            height = dimensions.1
        )
        .entered();
        // 直接从原图缩放，避免累积取整和插值损失；Triangle 比 Lanczos3 更适合快速截图复制。
        let scaled = {
            #[cfg(feature = "perf-tracing")]
            let _resize_perf_guard = tracing::info_span!(
                target: "perf",
                "snap_image_resize",
                width = dimensions.0,
                height = dimensions.1
            )
            .entered();
            resize(
                &original,
                dimensions.0,
                dimensions.1,
                ResizeFilter::Triangle,
            )
        };
        if encode_rgb(&scaled, &mut png, max_bytes, &cancelled)? {
            return finish_candidate(scaled, png, &cancelled);
        }
        encoded_bytes = png.len();
    }
    check_cancelled(&cancelled)?;
    Err("设置的大小上限无法容纳最小 PNG 图片".into())
}

// 转窄前均已将结果限制在原始 u32 尺寸内；使用整数平方根避免浮点比例和取整漂移。
#[allow(clippy::cast_possible_truncation)]
fn next_dimensions(
    original: (u32, u32),
    current_long: u32,
    encoded_bytes: usize,
    max_bytes: usize,
    estimate: bool,
) -> (u32, u32) {
    let long = if estimate {
        let projected =
            (max_bytes as u128 * u128::from(current_long).pow(2) / encoded_bytes as u128).isqrt();
        // 按面积比估算边长，并预留约 19% 像素预算，减少编码后的反复微调。
        (projected * 9 / 10).clamp(1, u128::from(current_long - 1)) as u32
    } else {
        (current_long / 2).max(1)
    };
    let original_long = u64::from(original.0.max(original.1));
    let dimension = |value| {
        ((u64::from(value) * u64::from(long) + original_long / 2) / original_long).max(1) as u32
    };
    (dimension(original.0), dimension(original.1))
}

fn check_cancelled(cancelled: &impl Fn() -> bool) -> Result<(), String> {
    if cancelled() {
        Err("已取消图片处理".into())
    } else {
        Ok(())
    }
}

fn finish_candidate(
    image: RgbImage,
    png: Vec<u8>,
    cancelled: &impl Fn() -> bool,
) -> Result<CompressedImage, String> {
    check_cancelled(cancelled)?;
    let (width, height) = image.dimensions();
    Ok(CompressedImage {
        png,
        width,
        height,
        rgb: image.into_raw(),
    })
}

#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(
        target = "perf",
        name = "snap_image_indexed_png",
        skip_all,
        fields(
            width = image.width(),
            height = image.height(),
            max_bytes = max_bytes,
            palette_colors = tracing::field::Empty,
            encoded_bytes = tracing::field::Empty
        )
    )
)]
fn encode_indexed(
    image: &RgbImage,
    png: &mut Vec<u8>,
    max_bytes: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<bool, String> {
    check_cancelled(cancelled)?;
    let mut colors = HashMap::with_capacity(256);
    let mut palette = Vec::with_capacity(256 * 3);
    // 不预分配整图索引：复杂彩色图遇到第 257 色即停止扫描。
    let mut indices = Vec::new();
    for (position, pixel) in image.as_raw().as_chunks::<3>().0.iter().enumerate() {
        if position % 4096 == 0 {
            check_cancelled(cancelled)?;
        }
        let key = u32::from_le_bytes([pixel[0], pixel[1], pixel[2], 0]);
        let index = if let Some(&index) = colors.get(&key) {
            index
        } else {
            let Ok(index) = u8::try_from(colors.len()) else {
                return Ok(false);
            };
            colors.insert(key, index);
            palette.extend_from_slice(pixel);
            index
        };
        indices.push(index);
    }
    check_cancelled(cancelled)?;
    #[cfg(feature = "perf-tracing")]
    tracing::Span::current().record("palette_colors", colors.len());
    png.clear();
    {
        jchtools_foundation::perf::perf_span!("snap_image_indexed_png_encode");
        let mut encoder = png::Encoder::new(&mut *png, image.width(), image.height());
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(palette);
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::Filter::Adaptive);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("截图索引 PNG 编码失败：{error}"))?;
        writer
            .write_image_data(&indices)
            .map_err(|error| format!("截图索引 PNG 编码失败：{error}"))?;
        writer
            .finish()
            .map_err(|error| format!("截图索引 PNG 编码失败：{error}"))?;
    }
    check_cancelled(cancelled)?;
    #[cfg(feature = "perf-tracing")]
    tracing::Span::current().record("encoded_bytes", png.len());
    Ok(png.len() <= max_bytes)
}

#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(
        target = "perf",
        name = "snap_image_rgb_png_encode",
        skip_all,
        fields(
            width = image.width(),
            height = image.height(),
            max_bytes = max_bytes,
            encoded_bytes = tracing::field::Empty
        )
    )
)]
fn encode_rgb(
    image: &RgbImage,
    png: &mut Vec<u8>,
    max_bytes: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<bool, String> {
    check_cancelled(cancelled)?;
    png.clear();
    let compression = if image.dimensions() == (1, 1) {
        CompressionType::Best
    } else {
        CompressionType::Fast
    };
    PngEncoder::new_with_quality(&mut *png, compression, FilterType::Adaptive)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|error| format!("截图 PNG 编码失败：{error}"))?;
    #[cfg(feature = "perf-tracing")]
    tracing::Span::current().record("encoded_bytes", png.len());
    check_cancelled(cancelled)?;
    // 仅编码完成后的精确长度能决定成功，恰好等于上限也合格。
    Ok(png.len() <= max_bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::thread;

    use super::{compress, CompressedImage};
    use crate::capture_win::BgrImage;

    // 五类合成内容均为内存数据，不依赖桌面、字体、模型或临时图片。
    fn fixture(class: u8, width: usize, height: usize) -> Result<BgrImage, String> {
        let mut data = Vec::with_capacity(width * height * 3);
        let mut noise = 0x9e37_79b9_u32;
        for y in 0..height {
            for x in 0..width {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                let [red, green, blue, _] = noise.to_le_bytes();
                let rgb = match class {
                    1 => {
                        if (x / 80 + y / 60) % 2 == 0 {
                            [240, 248, 255]
                        } else {
                            [32, 96, 180]
                        }
                    }
                    2 => {
                        let grid = x % 80 == 0 || y % 40 == 0;
                        let glyph = x % 12 < 2 && y % 40 > 12 && y % 40 < 28;
                        if grid || glyph {
                            [16, 24, 32]
                        } else {
                            [255; 3]
                        }
                    }
                    3 => {
                        if x % 47 == 0 || y % 53 == 0 || x == y {
                            [220, 28, 44]
                        } else {
                            [255; 3]
                        }
                    }
                    _ => [red, green, blue],
                };
                data.extend_from_slice(&[rgb[2], rgb[1], rgb[0]]);
            }
        }
        BgrImage::from_vec(width, height, data)
    }

    fn decoded(result: &CompressedImage, limit: usize) -> Result<image::RgbImage, String> {
        assert!(result.byte_len() <= limit, "不能返回超限 PNG");
        assert_eq!(result.byte_len(), result.png.len());
        let decoded = image::load_from_memory_with_format(&result.png, image::ImageFormat::Png)
            .map_err(|error| format!("输出 PNG 无法解码：{error}"))?
            .to_rgb8();
        assert_eq!(decoded.dimensions(), (result.width, result.height));
        assert_eq!(
            decoded.as_raw(),
            &result.rgb,
            "兼容位图必须复用最终处理像素"
        );
        Ok(decoded)
    }

    #[test]
    fn five_image_classes_return_decodable_size_limited_png() -> Result<(), String> {
        for (class, width, height) in [
            (0, 16, 12),
            (1, 1280, 720),
            (2, 640, 480),
            (3, 512, 384),
            (4, 512, 384),
        ] {
            let image = fixture(class, width, height)?;
            let original = image.clone();
            let first = compress(&image, usize::MAX, || false)?;
            // 复杂彩色图必须在更紧的实际编码预算下走减色或缩图分支。
            let limit = if class == 4 {
                first.byte_len() / 2
            } else {
                first.byte_len()
            };
            let result = compress(&image, limit, || false)?;
            let output = decoded(&result, limit)?;
            assert_eq!(image, original, "复制处理不能修改 OCR 原图");
            let width = u64::try_from(width).map_err(|_| "测试图像宽度无法表示")?;
            let height = u64::try_from(height).map_err(|_| "测试图像高度无法表示")?;
            let aspect_error =
                (u64::from(output.width()) * height).abs_diff(u64::from(output.height()) * width);
            assert!(
                aspect_error * 2 <= width + height,
                "缩放只允许整数像素取整误差"
            );
            if class != 4 {
                assert_eq!(u64::from(output.width()), width);
                assert_eq!(u64::from(output.height()), height);
                for (rgb, bgr) in output
                    .as_raw()
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .zip(image.bgr_pixels().as_chunks::<3>().0)
                {
                    assert_eq!(*rgb, [bgr[2], bgr[1], bgr[0]], "无损候选应保留原色");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn exact_byte_limit_accepts_but_one_byte_less_never_returns_overlimit() -> Result<(), String> {
        let image = fixture(0, 32, 24)?;
        let first = compress(&image, usize::MAX, || false)?;
        let exact = compress(&image, first.byte_len(), || false)?;
        assert_eq!(exact.png, first.png, "等于精确上限时不应继续降质");
        let _decoded = decoded(&exact, first.byte_len())?;
        let lower_limit = first.byte_len() - 1;
        if let Ok(result) = compress(&image, lower_limit, || false) {
            let _decoded = decoded(&result, lower_limit)?;
        }
        Ok(())
    }

    #[test]
    fn impossible_limit_fails_with_actionable_message() -> Result<(), String> {
        let image = fixture(2, 160, 120)?;
        assert!(
            compress(&image, 1, || false).is_err(),
            "无法容纳有效 PNG 的上限不能成功"
        );
        Ok(())
    }

    #[test]
    fn cancellation_before_or_after_encoding_never_returns_an_image() -> Result<(), String> {
        let image = fixture(0, 32, 24)?;
        let immediate = compress(&image, usize::MAX, || true)
            .err()
            .ok_or("取消后不应成功")?;
        assert!(immediate.contains("取消"));
        let state = AtomicUsize::new(0);
        let entered = Barrier::new(2);
        let requested = Barrier::new(2);
        // 在初次取消检查放行后由另一线程请求取消，不绑定编码器的内部检查次数。
        let during = thread::scope(|scope| {
            let worker = scope.spawn(|| {
                compress(&image, usize::MAX, || {
                    if state
                        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        entered.wait();
                        requested.wait();
                        false
                    } else {
                        state.load(Ordering::Acquire) == 2
                    }
                })
            });
            entered.wait();
            state.store(2, Ordering::Release);
            requested.wait();
            worker
                .join()
                .map_err(|_| "取消测试工作线程异常退出".to_string())?
        })
        .err()
        .ok_or("处理中取消不应返回图像")?;
        assert!(during.contains("取消"));
        Ok(())
    }
}
