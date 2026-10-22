use super::*;
/// 复用真实引擎套件的 tar → gzip 打包链与公开 extract_run_at；不直接模拟 Staging 换代。
/// preexisting 为 Some 时容器在任务开始前已存在，空字节表示用户原有空目录。
fn assert_composite_staging_cleanup(suffix: &str, preexisting: Option<&[u8]>) {
    let tmp = fixture("review-composite-staging");
    let root = tmp.path().join("data");
    let input = tmp.path().join("input");
    fs::create_dir(&input).unwrap();
    let engine_path = std::env::var_os("JCHTOOLS_TEST_7ZIP")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            jchtools_file_core::engine_bundle::resolve_executable()
                .expect("复合流回归需要真实随包或内嵌 7-Zip 引擎")
        });
    let member_bytes = b"real composite archive member";
    fs::write(input.join("payload.txt"), member_bytes).unwrap();
    fs::write(root.join("payload.txt"), b"existing user file").unwrap();
    fs::write(root.join("bundle.notes"), b"unrelated same-stem user data").unwrap();
    let parent = root.join(".jchtools-work");
    if let Some(bytes) = preexisting {
        fs::create_dir(&parent).unwrap();
        if !bytes.is_empty() {
            fs::write(parent.join("keep.txt"), bytes).unwrap();
        }
    } else {
        assert!(!parent.exists(), "前置：此容器只能由本次任务新建");
    }
    let source = root.join(format!("bundle{suffix}"));
    for (archive, format, member) in [
        (input.join("bundle.tar"), "-ttar", "payload.txt"),
        (source.clone(), "-tgzip", "bundle.tar"),
    ] {
        let packed = std::process::Command::new(&engine_path)
            .current_dir(&input)
            .args(["a", "-y", format])
            .arg(archive)
            .arg(member)
            .output()
            .unwrap();
        assert!(
            packed.status.success(),
            "真实复合流夹具打包失败：{}",
            String::from_utf8_lossy(&packed.stderr)
        );
    }
    let source_bytes = fs::read(&source).unwrap();
    assert_eq!(
        &source_bytes[..2],
        b"\x1f\x8b",
        "前置：使用真实 gzip 流，不是脚本空成功包"
    );
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config {
            reserve_bytes: 0,
            max_ratio: 0,
            max_depth: 1,
            include_hidden: true,
            ..Config::default()
        },
        Context::default(),
        &state_of(&tmp),
        Some(&engine_path),
    )
    .unwrap();

    assert_eq!(
        result.summary.archives_ok, 1,
        "复合流内部换代仍只算一包、一层"
    );
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_quarantined, 0);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(result.summary.deleted, 1, "仅删除完整成功的复合源包");
    assert_eq!(result.summary.extracted, 1);
    assert!(!source.exists(), "完整成功的复合源包应永久删除");
    assert!(
        !root.join("bundle.tar").exists(),
        "中间 tar 只能存在于本次暂存区"
    );
    assert_eq!(
        fs::read(root.join("payload.txt")).unwrap(),
        b"existing user file"
    );
    assert_eq!(
        fs::read(root.join("payload (1).txt")).unwrap(),
        member_bytes
    );
    assert_eq!(
        fs::read(root.join("bundle.notes")).unwrap(),
        b"unrelated same-stem user data"
    );
    if let Some(bytes) = preexisting {
        assert!(
            parent.is_dir(),
            "不能删除用户原有的普通容器，包括原有空目录"
        );
        if bytes.is_empty() {
            assert_eq!(
                fs::read_dir(&parent).unwrap().count(),
                0,
                "用户空目录仍为空，无暂存残留"
            );
        } else {
            assert_eq!(fs::read(parent.join("keep.txt")).unwrap(), bytes);
            assert_eq!(
                fs::read_dir(&parent).unwrap().count(),
                1,
                "原有字节不动，只清理本次随机工作区"
            );
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), 4);
    } else {
        assert!(
            !parent.exists(),
            "复合流 stage=next 后，最终清理仍须移除本次新建的空暂存父容器"
        );
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            3,
            "只留下真实正式输出和原有用户文件"
        );
    }
}

// 覆盖 X-04/X-05/X-08/H-07/S-01/P-08/E-02：真实复合流的内部换代不能丢失新建父容器清理责任。
#[test]
fn review_fix_composite_stream_removes_owned_empty_staging_parent() {
    for suffix in [".tar.gz", ".tgz"] {
        assert_composite_staging_cleanup(suffix, None);
    }
}

// 覆盖 X-04/X-05/X-08/H-07/S-01/P-08/E-02：任务只清理自己新建的容器，不能删除用户原有目录。
#[test]
fn review_fix_composite_stream_retains_preexisting_staging_parent() {
    for suffix in [".tar.gz", ".tgz"] {
        for bytes in [b"".as_slice(), b"original user parent data".as_slice()] {
            assert_composite_staging_cleanup(suffix, Some(bytes));
        }
    }
}
