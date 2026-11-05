//! O-39/O-40/O-43：只经公开接口验证内存截图的文字保真、颜色优先与最终字节上限。

#![cfg(windows)]

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder, RgbImage};
use snap_ocr_worker::capture_win::BgrImage;
use snap_ocr_worker::image_copy::{compress, CompressedImage};

fn next_rgb(state: &mut u32) -> [u8; 3] {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    let [red, green, blue, _] = state.to_le_bytes();
    [red, green, blue]
}

fn as_bgr(source: &RgbImage) -> Result<BgrImage, String> {
    let mut pixels = source.as_raw().clone();
    for pixel in pixels.as_chunks_mut::<3>().0 {
        pixel.swap(0, 2);
    }
    BgrImage::from_vec(
        usize::try_from(source.width()).map_err(|_| "测试图像宽度无法表示")?,
        usize::try_from(source.height()).map_err(|_| "测试图像高度无法表示")?,
        pixels,
    )
}

fn encode_rgb(source: &RgbImage) -> Result<Vec<u8>, String> {
    let mut png = Vec::new();
    PngEncoder::new_with_quality(&mut png, CompressionType::Best, FilterType::Adaptive)
        .write_image(
            source.as_raw(),
            source.width(),
            source.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|error| format!("测试 RGB PNG 编码失败：{error}"))?;
    Ok(png)
}

fn decoded(result: &CompressedImage, limit: usize) -> Result<RgbImage, String> {
    assert!(result.byte_len() <= limit, "最终 PNG 必须满足精确字节上限");
    assert_eq!(result.byte_len(), result.png.len());
    let output = image::load_from_memory_with_format(&result.png, image::ImageFormat::Png)
        .map_err(|error| format!("最终 PNG 必须完整可解码：{error}"))?
        .to_rgb8();
    assert_eq!(output.dimensions(), (result.width, result.height));
    Ok(output)
}

fn palette_text() -> RgbImage {
    let mut state = 0x9e37_79b9_u32;
    let mut palette: [[u8; 3]; 256] = std::array::from_fn(|_| next_rgb(&mut state));
    for (entry, level) in palette.iter_mut().take(16).zip(0_u8..16) {
        *entry = [level * 17; 3];
    }
    palette[16] = [231, 29, 53];
    palette[17] = [13, 187, 163];
    palette[18] = [37, 83, 229];

    // 可重复高熵索引铺底，使 RGB 的三通道编码明显大于每像素一个索引。
    // 黑色 H 的笔画只有一像素，邻接的多级灰边模拟文字抗锯齿；彩线也只有一像素。
    RgbImage::from_fn(256, 192, |x, y| {
        let background = usize::from(next_rgb(&mut state)[0]);
        let local_x = x % 16;
        let local_y = y % 24;
        let stem_height = (5..=18).contains(&local_y);
        let index = if x == 128 {
            16
        } else if y == 96 {
            17
        } else if x == y + 32 {
            18
        } else if (stem_height && (local_x == 4 || local_x == 10))
            || (local_y == 11 && (4..=10).contains(&local_x))
        {
            0
        } else if stem_height && (local_x == 3 || local_x == 11) {
            3
        } else if stem_height && (local_x == 5 || local_x == 9) {
            9
        } else if (local_y == 4 || local_y == 19) && (3..=11).contains(&local_x) {
            12
        } else {
            background
        };
        image::Rgb(palette[index])
    })
}

#[test]
fn palette_text_keeps_every_original_pixel_under_fixed_budget() -> Result<(), String> {
    // O-39/O-40/O-43：256×192 的索引数据加行过滤字节仅 49,344 字节，
    // 再加至多 768 字节调色板和 PNG/压缩封装，56 KiB 留有余量。
    // 预算固定，不依赖某版编码器的精确输出，也不放宽为“能返回图片即可”。
    const LIMIT: usize = 56 * 1024;
    let source = palette_text();
    assert!(
        encode_rgb(&source)?.len() > LIMIT,
        "夹具必须让原尺寸 RGB PNG 超限，才能覆盖无损索引候选"
    );
    let input = as_bgr(&source)?;
    let result = compress(&input, LIMIT, || false)?;
    let output = decoded(&result, LIMIT)?;
    assert_eq!(
        output.dimensions(),
        source.dimensions(),
        "预算足够无损索引 PNG 时不能缩小一像素文字和彩线"
    );
    assert!(
        output.as_raw() == source.as_raw(),
        "预算足够无损索引 PNG 时必须逐像素保留原色及抗锯齿层级"
    );
    Ok(())
}
