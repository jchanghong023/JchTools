//! 内部验收入口：同一 PNG 上测量真实模型加载与识别，并保存布局原文。
//! 用法：cargo run --release --manifest-path optional/snap-ocr-worker/Cargo.toml
//!       --example ocr_compare -- <det.onnx> <rec.onnx> <dict.txt> <image.png> <report.json> [runs]

use std::error::Error;
use std::path::Path;
use std::time::Instant;

use snap_ocr_core::layout::build_layout;
use snap_ocr_core::pipeline::{assemble_spans, detect_candidates, recognize_records, OcrBackend};
use snap_ocr_worker::image_ops::{self, BgrImage};
use snap_ocr_worker::pipeline_backend::{build_record, WorkerOcrBackend};

fn recognize(backend: &WorkerOcrBackend, image: &BgrImage) -> Result<String, Box<dyn Error>> {
    let width = u32::try_from(image.width())?;
    let height = u32::try_from(image.height())?;
    let candidates = detect_candidates(backend, image, width, height, None)?;
    let mut records = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if let Ok(crop) = image_ops::warp_perspective_cubic_replicate(image, candidate.quad()) {
            records.push(build_record(candidate, crop));
        }
    }
    recognize_records(backend, &mut records, None)?;
    let spans = assemble_spans(&records, None)?;
    Ok(build_layout(&spans).text)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(5..=6).contains(&args.len()) {
        return Err("需要 det rec dict image report [runs] 六个位置参数".into());
    }
    let runs = args.get(5).map_or(Ok(3), |value| value.parse::<usize>())?;
    if runs == 0 {
        return Err("runs 必须大于零".into());
    }
    let rgb = image::open(&args[3])?.to_rgb8();
    let mut data = Vec::with_capacity(rgb.len());
    for pixel in rgb.pixels() {
        data.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    let image = BgrImage::from_vec(rgb.width() as usize, rgb.height() as usize, data)?;

    let started = Instant::now();
    let backend = WorkerOcrBackend::load_for_service(
        Path::new(&args[0]),
        Path::new(&args[1]),
        Path::new(&args[2]),
        10,
    )?;
    let white = BgrImage::from_vec(64, 64, vec![255; 64 * 64 * 3])?;
    let _ = backend.detect(&white)?;
    let rec_white = BgrImage::from_vec(64, 32, vec![255; 64 * 32 * 3])?;
    let _ = backend.recognize(&[rec_white])?;
    let initialization_seconds = started.elapsed().as_secs_f64();

    let mut recognition_seconds = Vec::with_capacity(runs);
    let mut actual_text = String::new();
    for _ in 0..runs {
        let started = Instant::now();
        let text = recognize(&backend, &image)?;
        recognition_seconds.push(started.elapsed().as_secs_f64());
        if !actual_text.is_empty() && actual_text != text {
            return Err("重复识别结果不稳定".into());
        }
        actual_text = text;
    }
    let report = serde_json::json!({
        "status": "success",
        "engine": "rust",
        "image": args[3],
        "initialization_seconds": initialization_seconds,
        "recognition_seconds": recognition_seconds,
        "actual_text": actual_text,
    });
    std::fs::write(&args[4], serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}
