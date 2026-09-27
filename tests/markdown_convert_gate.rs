//! 转 Markdown 页数预检公开入口回归（F18/F21）。
//! 覆盖 T-18：页数探测只做有界结构读取；元数据不可信或预算耗尽时回退常规模式，
//! 不把整份容器读入内存，也不按不可信元数据预留大额内存。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools::markdown_document::{page_count, Deadline};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn build_docx(temp: &Path, name: &str, pages: usize) -> PathBuf {
    let path = temp.join(name);
    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("docProps/app.xml", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(format!("<Properties><Pages>{pages}</Pages></Properties>").as_bytes())
        .unwrap();
    zip.finish().unwrap();
    path
}

// 覆盖 T-18：正常 200/201 页文档页数正确，分流判定（>200 走快速模式）依赖该值。
#[test]
fn public_page_count_reports_exact_pages() {
    let temp = tempfile::tempdir().unwrap();
    let deadline = Deadline::new(Duration::from_secs(30));
    assert_eq!(
        page_count(&build_docx(temp.path(), "a200.docx", 200), &deadline),
        Some(200)
    );
    assert_eq!(
        page_count(&build_docx(temp.path(), "a201.docx", 201), &deadline),
        Some(201)
    );
}

// 覆盖 T-18（F18）：损坏容器回退常规模式（None），不得报错或崩溃。
#[test]
fn public_page_count_falls_back_on_corrupt_container() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("broken.docx");
    std::fs::write(&path, b"definitely not a zip").unwrap();
    let deadline = Deadline::new(Duration::from_secs(30));
    assert_eq!(page_count(&path, &deadline), None);
}

// 覆盖 T-29（F21）：页数预检共享单文件预算，预算耗尽时立即回退且快速返回。
#[test]
fn public_page_count_respects_expired_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let path = build_docx(temp.path(), "budget.docx", 500);
    let deadline = Deadline::new(Duration::ZERO);
    let started = std::time::Instant::now();
    assert_eq!(page_count(&path, &deadline), None);
    assert!(started.elapsed() < Duration::from_secs(1));
}
