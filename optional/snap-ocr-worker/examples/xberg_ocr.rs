//! 同图对照入口：对一张图片字节走 XbergWorkerClient 识别路径（与服务的
//! recognize 相同：内存 PNG → base64 → `ocr_snapshot`），输出布局文本 JSON。
//!
//! 用法（参数：Xberg 组件目录、图片路径、输出 JSON 路径）：
//! `cargo run --manifest-path optional/snap-ocr-worker/Cargo.toml --example xberg_ocr -- \
//!  <组件目录> <image.png> <输出.json>`
//!
//! 仅用于开发验收的合成截图对照（tests/ocr_fixtures），不是产品入口。

use std::sync::atomic::AtomicBool;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let Some(component) = args.next() else {
        return Err("缺少 Xberg 组件目录参数".into());
    };
    let Some(image_path) = args.next() else {
        return Err("缺少图片路径参数".into());
    };
    let Some(output) = args.next() else {
        return Err("缺少输出 JSON 路径参数".into());
    };
    let png = std::fs::read(&image_path)?;
    let mut client =
        snap_ocr_worker::xberg_worker::XbergWorkerClient::spawn(std::path::Path::new(&component))
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    let started = std::time::Instant::now();
    let text = client.recognize(&png, &AtomicBool::new(false))?;
    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let value = serde_json::json!({
        "text": text.unwrap_or_default(),
        "elapsed_ms": elapsed,
    });
    std::fs::write(&output, serde_json::to_vec_pretty(&value)?)?;
    client.shutdown(std::time::Duration::from_secs(10))?;
    eprintln!("识别完成：{} 字节 PNG，耗时 {elapsed} ms", png.len());
    Ok(())
}
