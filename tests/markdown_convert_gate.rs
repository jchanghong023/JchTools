//! 转 Markdown 页数预检公开入口回归（F18/F21）。
//! 覆盖 T-18：页数探测只做有界结构读取；元数据不可信或预算耗尽时回退常规模式，
//! 不把整份容器读入内存，也不按不可信元数据预留大额内存。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools::markdown::{
    test_media_dir_name, test_scan_count_with_cancel, test_scan_media_dirs,
    test_write_new_markdown, FormatGroup, Options,
};
use jchtools::markdown_document::{page_count, Deadline, MediaFile};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
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

// 覆盖 T-10/T-14/T-25：空白折叠不能让不同结果共享媒体目录；层级输出按实际目标父目录
// 分配并在同一批次内保留唯一目录。
#[test]
fn media_directories_are_unique_after_name_sanitization() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("input");
    let output = temp.path().join("output");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(input.join("a b.docx"), b"one").unwrap();
    std::fs::write(input.join("a_b.docx"), b"two").unwrap();
    let options = Options {
        input_dir: input,
        output_dir: output,
        flat: false,
        groups: vec![FormatGroup::Office],
        timeout_secs: 1,
    };
    let dirs = test_scan_media_dirs(&options).unwrap();
    assert_eq!(dirs.len(), 2);
    assert_ne!(dirs[0], dirs[1]);
}

// 覆盖 T-14/T-25/S-01：已有媒体图片不得被覆盖，也不得在失败回滚时删除。
#[test]
fn media_commit_refuses_existing_image_without_deleting_it() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("a_docx.md");
    let media_dir = temp.path().join("a_docx_media");
    std::fs::create_dir_all(&media_dir).unwrap();
    let image = media_dir.join("image_0.png");
    std::fs::write(&image, b"old").unwrap();
    let media = vec![MediaFile {
        relative: "a_docx_media/image_0.png".to_string(),
        bytes: b"new".to_vec(),
    }];
    let error = test_write_new_markdown(temp.path(), &target, "body", &media).unwrap_err();
    assert!(error.contains("已存在"));
    assert_eq!(std::fs::read(&image).unwrap(), b"old");
    assert!(!target.exists());
}

// 覆盖 T-14：媒体目录占用检查必须作用于目标所在层级，而不是只检查输出根。
#[test]
fn media_directory_occupancy_uses_target_parent() {
    let temp = tempfile::tempdir().unwrap();
    let docs = temp.path().join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a_docx_media"), b"occupied").unwrap();
    let target = docs.join("a_docx.md");
    assert_eq!(
        test_media_dir_name(temp.path(), &target).unwrap(),
        "a_docx_2_media"
    );
}

// 覆盖 T-22/T-23：扫描入口在开始前收到停止请求时必须立即取消。
#[test]
fn scan_honors_cancellation_before_traversal() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("input");
    let output = temp.path().join("output");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::create_dir_all(&output).unwrap();
    let options = Options {
        input_dir: input,
        output_dir: output,
        flat: false,
        groups: vec![FormatGroup::Pdf],
        timeout_secs: 1,
    };
    let cancel = AtomicBool::new(true);
    let error = test_scan_count_with_cancel(&options, &cancel).unwrap_err();
    assert!(error.contains("取消"));
}
