#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]

//! MD 整理核心逻辑测试（合同 M 分区）：合并（M-02~M-07）与拆分（M-08~M-11）。
//! 全部用临时目录；原文件不改动的断言前后逐字节对比。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools_file_core::control::Control;
use jchtools_md_tools::md_tools;
use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

#[path = "md_tools/merge.rs"]
mod merge;
#[path = "md_tools/review_f21.rs"]
mod review_f21;
#[path = "md_tools/scan.rs"]
mod scan;
#[path = "md_tools/split_parts.rs"]
mod split_parts;
#[path = "fixtures/unresolvable_output.rs"]
mod unresolvable_output;

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn set_created(path: &Path, seconds: u64) {
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
    // 平台不支持创建时间时此处失败（M-03 的替代口径按修改时间）；
    // 本套用例在 Windows（P-07 唯一支持平台）上必须成功。
    jchtools_file_core::fsutil::set_created_time(path, time)
        .unwrap_or_else(|error| panic!("设置创建时间失败：{}：{error}", path.display()));
}

fn source_bytes_under(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut rows: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let bytes = fs::read(&path).unwrap();
        rows.push((path.display().to_string(), bytes));
    }
    rows.sort();
    rows
}

fn merge_to(root: &Path, recursive: bool, output: &Path) -> md_tools::MergeStats {
    let entries =
        md_tools::scan_markdown(root, recursive, Some(output), &Control::default()).unwrap();
    md_tools::merge_markdown_with_events(&entries, output, false, &Control::default(), &|_| Ok(()))
        .unwrap()
}

// 覆盖 M-10（分片按编号顺序二进制拼接还原原文件）
fn verify_split_output(source: &Path, out_dir: &Path, count: usize, limit: u64) {
    let names = md_tools::split_names(
        &source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        count,
    );
    let mut joined: Vec<u8> = Vec::new();
    for name in &names {
        let chunk = fs::read(out_dir.join(name)).unwrap();
        assert!(
            u64::try_from(chunk.len()).unwrap() <= limit,
            "实际分片 {name} 为 {} 字节，超过 {limit} 字节硬限制（M-09）",
            chunk.len()
        );
        assert!(
            std::str::from_utf8(&chunk).is_ok(),
            "实际分片 {name} 切坏了 UTF-8 字符（M-10）"
        );
        joined.extend_from_slice(&chunk);
    }
    let original = fs::read(source).unwrap();
    assert_eq!(
        joined, original,
        "全部分片按编号顺序拼接必须与原文件完全一致（M-10）"
    );
}
