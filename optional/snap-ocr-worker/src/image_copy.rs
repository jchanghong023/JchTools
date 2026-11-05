//! 截图复制的独立内存处理（O-39/O-40/O-43），不写文件、不记录图像内容。
//!
//! 先以最高等级无损 PNG 保留原尺寸与颜色，再依次保留每通道 7、6、5 位。
//! 仍超限才从原图分别缩至 90%、80%、70%、60%、50%，每档重新优先完整颜色。
//! 缩图使用 Lanczos3，覆盖整个选区；每边不得低于原边长与 64 像素中的较小值。
//! 最多 6 档尺寸、每档 4 次编码，共 24 次；不以无限缩小或灰度化换取达标。

#![cfg(windows)]

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::imageops::{resize, FilterType as ResizeFilter};
use image::{ExtendedColorType, ImageEncoder, RgbImage};

use crate::capture_win::BgrImage;

/// 已按精确字节上限校验的完整 PNG 及其实际像素尺寸。
pub struct CompressedImage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
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
/// 候选按尺寸优先、同尺寸颜色精度优先排列；每次编码前后均检查取消。
/// 当前不可中断的缩放或编码返回后，取消结果不会继续作为成功返回。
///
/// # Errors
/// 取消、无效尺寸、PNG 编码失败或达到可读性底线仍超限时返回中文说明。
pub fn compress(
    image: &BgrImage,
    max_bytes: usize,
    cancelled: impl Fn() -> bool,
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
    let mut rgb = image.bgr_pixels().to_vec();
    for pixel in rgb.as_chunks_mut::<3>().0 {
        pixel.swap(0, 2);
    }
    let original =
        RgbImage::from_raw(width, height, rgb).ok_or_else(|| "图像缓冲与尺寸不符".to_string())?;
    let mut png = Vec::new();
    let mut reduced = Vec::new();
    if encode_candidates(&original, &mut png, &mut reduced, max_bytes, &cancelled)? {
        return Ok(CompressedImage { png, width, height });
    }

    let minimum_width = width.min(64);
    let minimum_height = height.min(64);
    let mut previous = (width, height);
    for percent in [90, 80, 70, 60, 50] {
        check_cancelled(&cancelled)?;
        // 使用同一比例从原图计算；商余分解避免尺寸乘法溢出，允许半像素取整。
        let scaled_width = scaled_dimension(width, percent);
        let scaled_height = scaled_dimension(height, percent);
        if scaled_width < minimum_width || scaled_height < minimum_height {
            break;
        }
        if previous == (scaled_width, scaled_height) {
            continue;
        }
        previous = (scaled_width, scaled_height);
        // 每档直接从完整颜色原图重采样，不累积前一档的减色或缩放损失。
        let scaled = resize(
            &original,
            scaled_width,
            scaled_height,
            ResizeFilter::Lanczos3,
        );
        if encode_candidates(&scaled, &mut png, &mut reduced, max_bytes, &cancelled)? {
            return Ok(CompressedImage {
                png,
                width: scaled_width,
                height: scaled_height,
            });
        }
    }
    check_cancelled(&cancelled)?;
    Err("图片在可读性底线内仍超过大小上限，请提高上限或缩小截图区域".into())
}

fn scaled_dimension(value: u32, percent: u32) -> u32 {
    (value / 100) * percent + ((value % 100) * percent + 50) / 100
}

fn check_cancelled(cancelled: &impl Fn() -> bool) -> Result<(), String> {
    if cancelled() {
        Err("已取消图片处理".into())
    } else {
        Ok(())
    }
}

fn encode_candidates(
    image: &RgbImage,
    png: &mut Vec<u8>,
    reduced: &mut Vec<u8>,
    max_bytes: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<bool, String> {
    let (width, height) = image.dimensions();
    if encode(image.as_raw(), width, height, png, max_bytes, cancelled)? {
        return Ok(true);
    }
    for removed_bits in [1, 2, 3] {
        check_cancelled(cancelled)?;
        reduced.clear();
        // 独立压缩三色通道，复制高位填充低位，保留纯黑、纯白及彩色标记；
        // 最多去除 3 位（每通道至少 32 级），不引入灰度、抖动或内容改写。
        reduced.extend(
            image
                .as_raw()
                .iter()
                .map(|value| (value & (u8::MAX << removed_bits)) | (value >> (8 - removed_bits))),
        );
        if encode(reduced, width, height, png, max_bytes, cancelled)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn encode(
    rgb: &[u8],
    width: u32,
    height: u32,
    png: &mut Vec<u8>,
    max_bytes: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<bool, String> {
    check_cancelled(cancelled)?;
    png.clear();
    PngEncoder::new_with_quality(&mut *png, CompressionType::Best, FilterType::Adaptive)
        .write_image(rgb, width, height, ExtendedColorType::Rgb8)
        .map_err(|error| format!("截图 PNG 编码失败：{error}"))?;
    check_cancelled(cancelled)?;
    // 仅编码完成后的精确长度能决定成功，恰好等于上限也合格。
    Ok(png.len() <= max_bytes)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

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
        match compress(&image, lower_limit, || false) {
            Ok(result) => {
                let _decoded = decoded(&result, lower_limit)?;
            }
            Err(error) => assert!(error.contains("提高上限或缩小截图区域")),
        }
        Ok(())
    }

    #[test]
    fn impossible_limit_fails_with_actionable_message() -> Result<(), String> {
        let image = fixture(2, 160, 120)?;
        let error = compress(&image, 1, || false)
            .err()
            .ok_or("极低上限不应成功")?;
        assert!(error.contains("提高上限或缩小截图区域"));
        Ok(())
    }

    #[test]
    fn cancellation_before_or_after_encoding_never_returns_an_image() -> Result<(), String> {
        let image = fixture(0, 32, 24)?;
        let immediate = compress(&image, usize::MAX, || true)
            .err()
            .ok_or("取消后不应成功")?;
        assert!(immediate.contains("取消"));
        let checks = Cell::new(0);
        let during = compress(&image, usize::MAX, || {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        })
        .err()
        .ok_or("处理中取消不应返回图像")?;
        assert!(during.contains("取消"));
        Ok(())
    }
}
