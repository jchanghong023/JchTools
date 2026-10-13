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

//! 覆盖 H-07 / X-04：冲突只改文件名主体，保留原扩展名和复合扩展名。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools::fsutil;
use std::fs;

#[test]
fn archive_collision_preserves_compound_extension_and_existing_files() {
    let root = tempfile::tempdir().unwrap();
    let original = root.path().join("资料.tar.gz");
    let first = root.path().join("资料 (1).tar.gz");
    fs::write(&original, b"original").unwrap();
    fs::write(&first, b"first").unwrap();
    let target = fsutil::unique_target(root.path(), &original).unwrap();
    assert_eq!(target.file_name().unwrap(), "资料 (2).tar.gz");
    assert_eq!(fs::read(&original).unwrap(), b"original");
    assert_eq!(fs::read(&first).unwrap(), b"first");
}
#[test]
fn git_marker_name_is_case_insensitive() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join(".GIT"), b"gitdir: elsewhere").unwrap();
    assert!(fsutil::is_git_root(root.path()).unwrap());
}

// 覆盖 H-07 / X-04：合法 Unicode 空白属于原扩展名，冲突改名不得隐式修剪。
#[test]
fn archive_collision_preserves_unicode_whitespace_in_extension() {
    let root = tempfile::tempdir().unwrap();
    for whitespace in ['\u{00a0}', '\u{2003}', '\u{3000}'] {
        let name = format!("资料.txt{whitespace}");
        fsutil::validate_component(&name).unwrap();
        let original = root.path().join(&name);
        fs::write(&original, b"existing").unwrap();
        let target = fsutil::unique_target(root.path(), &original).unwrap();
        assert_eq!(
            target.file_name().unwrap().to_str().unwrap(),
            format!("资料 (1).txt{whitespace}"),
            "只允许修改主体，扩展名必须逐字符保留"
        );
        assert_eq!(fs::read(&original).unwrap(), b"existing");
    }
}
