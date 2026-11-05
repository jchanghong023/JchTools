//! O-39/O-40/O-43：只经公开接口验证内存截图的文字保真、颜色优先与最终字节上限。

#![cfg(windows)]

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::imageops::{resize, FilterType as ResizeFilter};
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

fn reduced_color_candidates(source: &RgbImage) -> [RgbImage; 3] {
    [1_u8, 2, 3].map(|removed_bits| {
        let mut reduced = source.clone();
        for pixel in reduced.pixels_mut() {
            for channel in &mut pixel.0 {
                *channel =
                    (*channel & (u8::MAX << removed_bits)) | (*channel >> (8 - removed_bits));
            }
        }
        reduced
    })
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

#[test]
fn multiple_fitting_color_candidates_always_choose_highest_precision() -> Result<(), String> {
    // O-39/O-40/O-43：完整颜色超限，但原尺寸的 7/6/5 位候选均能达标。
    // 重复调用只观察最终像素，不依赖线程数、调度时序或编码字节恰好相同。
    let mut state = 0x6a09_e667_u32;
    let source = RgbImage::from_fn(192, 128, |_, _| image::Rgb(next_rgb(&mut state)));
    let candidates = reduced_color_candidates(&source);
    let mut limit = 0;
    for candidate in &candidates {
        limit = limit.max(encode_rgb(candidate)?.len());
    }
    assert!(
        encode_rgb(&source)?.len() > limit,
        "夹具必须排除原颜色候选，且让三档有限减色同时达标"
    );
    assert_ne!(source.as_raw(), candidates[0].as_raw());
    let input = as_bgr(&source)?;
    for _ in 0..3 {
        let result = compress(&input, limit, || false)?;
        let output = decoded(&result, limit)?;
        assert_eq!(output.dimensions(), source.dimensions());
        assert!(
            output.as_raw() == candidates[0].as_raw(),
            "多个候选达标时必须稳定选择每通道 7 位，不能先完成先返回低质量候选"
        );
    }
    Ok(())
}

#[test]
fn first_fitting_resize_preserves_more_readable_pixels() -> Result<(), String> {
    // O-39/O-40：原尺寸所有颜色档超限时，95% 已可达标就不能直接跳到 90%。
    // 一像素字干和彩线的采样位置必须来自完整选区，不能裁边或累积缩放损失。
    // 平滑彩色渐变叠加固定种子细纹，保留超过 256 种原色及一像素 H/短彩线。
    // 随机 RGB 缩放后会增加滤波熵，可能使 95% 图反而大于原尺寸减色图，
    // 无法稳定覆盖“原尺寸全部超限、95% 首次达标”的分支。
    let mut state = 0xbb67_ae85_u32;
    let source = RgbImage::from_fn(320, 240, |x, y| {
        // 边缘和笔画覆盖的像素也消耗随机数，与已测夹具逐像素一致。
        let noise = next_rgb(&mut state);
        let horizontal = (x * 255 / 319).to_le_bytes()[0];
        let vertical = (y * 255 / 239).to_le_bytes()[0];
        let midpoint = horizontal.midpoint(vertical);
        let background = [
            horizontal.saturating_add(noise[0] & 3),
            vertical.saturating_add(noise[1] & 3),
            midpoint.saturating_add(noise[2] & 3),
        ];
        let stem = (113..=126).contains(&y) && (x == 154 || x == 160);
        let crossbar = y == 119 && (154..=160).contains(&x);
        let color = if x == 0 {
            [233, 41, 67]
        } else if x == 319 {
            [17, 193, 151]
        } else if y == 0 {
            [43, 79, 239]
        } else if y == 239 {
            [241, 181, 23]
        } else if x == 80 && (60..=70).contains(&y) {
            [237, 31, 59]
        } else if y == 160 && (213..=223).contains(&x) {
            [19, 189, 163]
        } else if stem || crossbar {
            [0; 3]
        } else {
            background
        };
        image::Rgb(color)
    });
    let colors: std::collections::HashSet<_> = source.pixels().map(|pixel| pixel.0).collect();
    assert!(
        colors.len() > 256,
        "复杂彩色夹具不能由原色索引 PNG 绕过缩放"
    );

    // 独立编码完整的 95% 参考图，取其中最小长度作为精确预算；
    // 不绑定编码器版本的固定字节数，也不依赖哪个并行候选先完成。
    let scaled = resize(&source, 304, 228, ResizeFilter::Lanczos3);
    let scaled_reduced = reduced_color_candidates(&scaled);
    let candidates = [
        &scaled,
        &scaled_reduced[0],
        &scaled_reduced[1],
        &scaled_reduced[2],
    ];
    let scaled_lengths = [
        encode_rgb(candidates[0])?.len(),
        encode_rgb(candidates[1])?.len(),
        encode_rgb(candidates[2])?.len(),
        encode_rgb(candidates[3])?.len(),
    ];
    let limit = *scaled_lengths.iter().min().ok_or("95% 参考候选不能为空")?;
    let original_reduced = reduced_color_candidates(&source);
    let original_lengths = [
        encode_rgb(&source)?.len(),
        encode_rgb(&original_reduced[0])?.len(),
        encode_rgb(&original_reduced[1])?.len(),
        encode_rgb(&original_reduced[2])?.len(),
    ];
    for length in &original_lengths {
        assert!(
            *length > limit,
            "夹具必须使原尺寸完整颜色及每通道 7/6/5 位候选全部超限：original_lengths={original_lengths:?}, scaled_lengths={scaled_lengths:?}, limit={limit}"
        );
    }
    let expected = candidates
        .iter()
        .zip(&scaled_lengths)
        .find_map(|(candidate, length)| (*length <= limit).then_some(*candidate))
        .ok_or("95% 参考图必须至少有一档精度达标")?;

    let input = as_bgr(&source)?;
    let result = compress(&input, limit, || false)?;
    let output = decoded(&result, limit)?;
    assert_eq!(
        output.dimensions(),
        (304, 228),
        "95% 已达标时必须保留更多文字像素，不能跳到 90% 或更低"
    );
    assert!(
        output.width() * output.height() > 288 * 216,
        "必须比 90% 的 288×216 输出保留更多可读像素"
    );
    assert_eq!(
        output.width() * source.height(),
        output.height() * source.width(),
        "缩放必须保持完整选区的宽高比"
    );
    assert!(
        output.as_raw() == expected.as_raw(),
        "全图像素必须等于完整选区的 Lanczos3 参考，包含四边标记、一像素字干和彩线"
    );
    Ok(())
}
