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
