//! 截图复制的独立内存处理（O-39/O-40/O-43），不写文件、不记录图像内容。
//!
//! 优先以精确调色板或 RGB 最高等级无损 PNG 保留原尺寸与原色，再尝试每通道 7、6、5 位。
//! 三档减色按固定质量顺序选优，最多 min(3, 可用并行度) 个临时工作线程；单核串行。
//! 减色共享完整颜色输入，每个工作线程仅保留当前候选的 RGB 缓冲；仅胜者就地生成最终 RGB。
//! 原尺寸仍超限才从原图分别缩至 95%、90%、…、50%，每档重新优先完整颜色。
//! 缩图使用 Lanczos3，覆盖整个选区；每边不得低于原边长与 64 像素中的较小值。
//! 最多 11 档尺寸、每档 1 次索引、1 次 RGB、3 次减色，共 55 次编码；尺寸之间不并行。
//! 内存保留原图 RGB、至多一档缩图、至多三个 PNG 输出及各线程当前候选的减色 RGB 缓冲。
//! 索引编码另用每像素一字节的临时索引及至多 256 色的映射，结束后释放再尝试 RGB 或减色。

#![cfg(windows)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread;

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
/// 候选按尺寸优先、同尺寸颜色精度优先排列；索引与 RGB 都保留完整原色。
/// 每次编码前后及分块减色时检查取消；等待全部临时线程退出后才返回结果。
/// 当前不可中断的缩放或编码返回后，取消结果不会继续作为成功返回。
///
/// # Errors
/// 取消、无效尺寸、PNG 编码失败或达到可读性底线仍超限时返回中文说明。
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
    let mut rgb = image.bgr_pixels().to_vec();
    for pixel in rgb.as_chunks_mut::<3>().0 {
        pixel.swap(0, 2);
    }
    let original =
        RgbImage::from_raw(width, height, rgb).ok_or_else(|| "图像缓冲与尺寸不符".to_string())?;
    if let Some(candidate) = encode_candidates(&original, max_bytes, &cancelled)? {
        return finish_candidate(original, candidate, &cancelled);
    }

    let minimum_width = width.min(64);
    let minimum_height = height.min(64);
    let mut previous = (width, height);
    for percent in [95, 90, 85, 80, 75, 70, 65, 60, 55, 50] {
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
        if let Some(candidate) = encode_candidates(&scaled, max_bytes, &cancelled)? {
            return finish_candidate(scaled, candidate, &cancelled);
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

struct EncodedCandidate {
    png: Vec<u8>,
    removed_bits: u8,
}

fn finish_candidate(
    image: RgbImage,
    candidate: EncodedCandidate,
    cancelled: &impl Fn() -> bool,
) -> Result<CompressedImage, String> {
    check_cancelled(cancelled)?;
    let (width, height) = image.dimensions();
    let mut rgb = image.into_raw();
    if candidate.removed_bits != 0 {
        for chunk in rgb.chunks_mut(4096) {
            check_cancelled(cancelled)?;
            for value in chunk {
                *value = reduce_channel(*value, candidate.removed_bits);
            }
        }
    }
    check_cancelled(cancelled)?;
    Ok(CompressedImage {
        png: candidate.png,
        width,
        height,
        rgb,
    })
}

fn encode_candidates(
    image: &RgbImage,
    max_bytes: usize,
    cancelled: &(impl Fn() -> bool + Sync),
) -> Result<Option<EncodedCandidate>, String> {
    let mut png = Vec::new();
    if encode_indexed(image, &mut png, max_bytes, cancelled)?
        || encode_rgb(image, &mut png, max_bytes, cancelled)?
    {
        return Ok(Some(EncodedCandidate {
            png,
            removed_bits: 0,
        }));
    }
    // 无损结果不再需要，避免与三档减色的输出同时常驻。
    drop(png);
    encode_reduced_candidates(image, max_bytes, cancelled)
}

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
    png.clear();
    {
        let mut encoder = png::Encoder::new(&mut *png, image.width(), image.height());
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(palette);
        encoder.set_compression(png::Compression::High);
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
    Ok(png.len() <= max_bytes)
}

fn encode_rgb(
    image: &RgbImage,
    png: &mut Vec<u8>,
    max_bytes: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<bool, String> {
    check_cancelled(cancelled)?;
    png.clear();
    PngEncoder::new_with_quality(&mut *png, CompressionType::Best, FilterType::Adaptive)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|error| format!("截图 PNG 编码失败：{error}"))?;
    check_cancelled(cancelled)?;
    // 仅编码完成后的精确长度能决定成功，恰好等于上限也合格。
    Ok(png.len() <= max_bytes)
}

fn encode_reduced_candidates(
    image: &RgbImage,
    max_bytes: usize,
    cancelled: &(impl Fn() -> bool + Sync),
) -> Result<Option<EncodedCandidate>, String> {
    let parallelism = thread::available_parallelism().map_or(1, |count| count.get().min(3));
    // 数值越小质量越高；已达标的细候选可停止不可能胜出的粗候选。
    let best_bits = AtomicU8::new(4);
    for batch in [1_u8, 2, 3].chunks(parallelism) {
        check_cancelled(cancelled)?;
        let candidate = if batch.len() == 1 {
            encode_reduced(image, batch[0], max_bytes, cancelled, &best_bits)?
        } else {
            thread::scope(|scope| {
                let mut handles = Vec::with_capacity(batch.len());
                let mut failure = None;
                for &removed_bits in batch {
                    let best_bits = &best_bits;
                    match thread::Builder::new().spawn_scoped(scope, move || {
                        encode_reduced(image, removed_bits, max_bytes, cancelled, best_bits)
                    }) {
                        Ok(handle) => handles.push(handle),
                        Err(error) => {
                            best_bits.store(0, Ordering::Relaxed);
                            failure = Some(format!("截图减色工作线程启动失败：{error}"));
                            break;
                        }
                    }
                }
                let mut best = None;
                // 必须逐一 join，不能因前一个错误或成功而遗留作用域内的线程。
                // 按质量顺序收集，绝不以最先完成者决定最终颜色精度。
                for handle in handles {
                    match handle.join() {
                        Ok(Ok(candidate)) => {
                            if best.is_none() {
                                best = candidate;
                            }
                        }
                        Ok(Err(error)) => {
                            best_bits.store(0, Ordering::Relaxed);
                            failure.get_or_insert(error);
                        }
                        Err(_) => {
                            best_bits.store(0, Ordering::Relaxed);
                            failure.get_or_insert_with(|| "截图减色工作线程异常退出".into());
                        }
                    }
                }
                if let Some(error) = failure {
                    Err(error)
                } else {
                    Ok(best)
                }
            })?
        };
        check_cancelled(cancelled)?;
        if candidate.is_some() {
            return Ok(candidate);
        }
    }
    Ok(None)
}

fn reduce_channel(value: u8, removed_bits: u8) -> u8 {
    // 复制高位填充低位，保留纯黑、纯白及彩色标记；不引入灰度或抖动。
    (value & (u8::MAX << removed_bits)) | (value >> (8 - removed_bits))
}

fn encode_reduced(
    image: &RgbImage,
    removed_bits: u8,
    max_bytes: usize,
    cancelled: &(impl Fn() -> bool + Sync),
    best_bits: &AtomicU8,
) -> Result<Option<EncodedCandidate>, String> {
    check_cancelled(cancelled)?;
    if removed_bits > best_bits.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let mut png = Vec::new();
    {
        // 流式 PNG 编码的压缩结果可能更大，导致同一预算选择更低颜色精度。
        // 每个工作线程仅生成一次必要的减色缓冲，沿用完整颜色的最高等级编码。
        let mut reduced = Vec::with_capacity(image.as_raw().len());
        for source in image.as_raw().chunks(4096) {
            check_cancelled(cancelled)?;
            if removed_bits > best_bits.load(Ordering::Relaxed) {
                return Ok(None);
            }
            reduced.extend(
                source
                    .iter()
                    .map(|&value| reduce_channel(value, removed_bits)),
            );
        }
        check_cancelled(cancelled)?;
        if removed_bits > best_bits.load(Ordering::Relaxed) {
            return Ok(None);
        }
        PngEncoder::new_with_quality(&mut png, CompressionType::Best, FilterType::Adaptive)
            .write_image(
                &reduced,
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .map_err(|error| format!("截图减色 PNG 编码失败：{error}"))?;
    }
    check_cancelled(cancelled)?;
    if png.len() > max_bytes || removed_bits > best_bits.load(Ordering::Relaxed) {
        return Ok(None);
    }
    best_bits.fetch_min(removed_bits, Ordering::Relaxed);
    Ok(Some(EncodedCandidate { png, removed_bits }))
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
