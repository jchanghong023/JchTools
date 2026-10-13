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

//! C-02 回归：启用副本名、关闭同名时，候选关系必须按连通组处理。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use filetime::{set_file_mtime, FileTime};
use jchtools_file_core::config::{Config, DeleteMode, KeepPolicy};
use jchtools_file_core::control::Context;
use jchtools_directory_organizer::engine;
use std::fs;
use tempfile::TempDir;

/// 覆盖 C-02/C-03：A/report.txt 与 B/report.txt 仅通过
/// C/report (1).txt 的副本名关系连成一个候选组；同名关系关闭后，
/// 三个同内容文件仍应在连通组中保留一份、删除两份。
#[test]
fn copy_name_bridge_forms_one_candidate_component() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("data");
    let state = temp.path().join("state");
    fs::create_dir_all(root.join("A")).unwrap();
    fs::create_dir_all(root.join("B")).unwrap();
    fs::create_dir_all(root.join("C")).unwrap();

    // 最旧优先使 A、B 先于桥接项 C 排序；修复前的逐个保留者匹配会错误地同时保留 A、B。
    for (path, seconds) in [
        (root.join("A/report.txt"), 10),
        (root.join("B/report.txt"), 20),
        (root.join("C/report (1).txt"), 30),
    ] {
        fs::write(&path, b"same-content").unwrap();
        set_file_mtime(&path, FileTime::from_unix_time(seconds, 0)).unwrap();
    }

    let mut config = Config {
        global_delete: DeleteMode::Permanent,
        dedup_same_name: false,
        dedup_copy_names: true,
        dedup_other_names: false,
        keep_duplicate: KeepPolicy::Oldest,
        ..Config::default()
    };
    // 关闭清理条件，使夹具只检验 C-02 的候选连通组语义。
    config.clean_junk = false;
    config.clean_temp = false;
    config.clean_zero = false;
    config.clean_copy_name = false;

    let task = engine::prepare_at(&root, config, Context::default(), &state).unwrap();
    assert_eq!(
        task.summary.planned_delete, 2,
        "C-02 候选连通组应保留一份并删除其余两份"
    );
    let db = jchtools_file_core::db::Database::open(&task.directory).unwrap();
    let deletions: Vec<_> = db
        .actions_page(0, 100)
        .unwrap()
        .into_iter()
        .filter(|action| matches!(action.kind, jchtools_file_core::model::ActionKind::Delete))
        .collect();
    assert_eq!(deletions.len(), 2);
    assert_eq!(
        deletions
            .iter()
            .map(|action| action.source.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["B/report.txt", "C/report (1).txt"]),
        "C-03：只能删除保留者以外的两个副本"
    );
    assert!(
        deletions.iter().all(|action| {
            action.keeper.as_ref().map(|(rel, _)| rel.as_str()) == Some("A/report.txt")
        }),
        "C-03 保留者必须由最旧时间决胜：{deletions:?}"
    );
}

/// 覆盖 C-03：最短名称按扫描时原始 UTF-16 长度，而非小写折叠后长度决胜。
#[test]
fn shortest_name_uses_original_utf16_length_after_case_fold() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("data");
    let state = temp.path().join("state");
    fs::create_dir_all(root.join("B")).unwrap();
    fs::create_dir_all(root.join("long")).unwrap();
    fs::write(root.join("B/aa.txt"), b"same-content").unwrap();
    fs::write(root.join("long/İ.txt"), b"same-content").unwrap();
    let mut config = Config {
        global_delete: DeleteMode::Permanent,
        dedup_same_name: false,
        dedup_copy_names: false,
        dedup_other_names: true,
        keep_duplicate: KeepPolicy::ShortestName,
        ..Config::default()
    };
    config.clean_junk = false;
    config.clean_temp = false;
    config.clean_zero = false;
    config.clean_copy_name = false;
    let task = engine::prepare_at(&root, config, Context::default(), &state).unwrap();
    let db = jchtools_file_core::db::Database::open(&task.directory).unwrap();
    let deletions: Vec<_> = db
        .actions_page(0, 100)
        .unwrap()
        .into_iter()
        .filter(|action| matches!(action.kind, jchtools_file_core::model::ActionKind::Delete))
        .collect();
    assert_eq!(deletions.len(), 1, "同内容二项必须仅删除一份：{deletions:?}");
    assert_eq!(task.summary.planned_delete, 1);
    let deletion = &deletions[0];
    assert_eq!(deletion.source, "B/aa.txt", "原始名称 İ.txt 更短");
    assert_eq!(
        deletion.keeper.as_ref().map(|(rel, _)| rel.as_str()),
        Some("long/İ.txt")
    );
}

/// 覆盖 C-02/C-04：连通组中同一物理文件的既有硬链接不计为待删副本，
/// 另一份独立文件仍按哈希结果删除。
#[test]
fn connected_component_keeps_existing_hardlink_and_deletes_independent_copy() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("data");
    let state = temp.path().join("state");
    for dir in ["A", "B", "C"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::write(root.join("A/report.txt"), b"same-content").unwrap();
    fs::hard_link(root.join("A/report.txt"), root.join("B/report.txt")).unwrap();
    fs::write(root.join("C/report (1).txt"), b"same-content").unwrap();
    let mut config = Config {
        global_delete: DeleteMode::Permanent,
        dedup_same_name: false,
        dedup_copy_names: true,
        dedup_other_names: false,
        keep_duplicate: KeepPolicy::ShortestName,
        ..Config::default()
    };
    config.clean_junk = false;
    config.clean_temp = false;
    config.clean_zero = false;
    config.clean_copy_name = false;
    let task = engine::prepare_at(&root, config, Context::default(), &state).unwrap();
    let db = jchtools_file_core::db::Database::open(&task.directory).unwrap();
    let deletions: Vec<_> = db
        .actions_page(0, 100)
        .unwrap()
        .into_iter()
        .filter(|action| matches!(action.kind, jchtools_file_core::model::ActionKind::Delete))
        .collect();
    assert_eq!(deletions.len(), 1, "硬链接不另计删除计划：{deletions:?}");
    assert_eq!(deletions[0].source, "C/report (1).txt");
}

/// 覆盖 C-02/C-03：不同名开启后，同尺寸候选不再受同名、副本名开关限制。
#[test]
fn other_names_enabled_deduplicates_same_and_copy_names() {
    for names in [
        ["A/report.txt", "B/report.txt"],
        ["A/report.txt", "B/report (1).txt"],
    ] {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("data");
        let state = temp.path().join("state");
        for (index, name) in names.iter().enumerate() {
            let source = root.join(name);
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::write(&source, b"same-content").unwrap();
            set_file_mtime(
                &source,
                FileTime::from_unix_time(10 + i64::try_from(index).unwrap(), 0),
            )
            .unwrap();
        }
        let config = Config {
            dedup_same_name: false,
            dedup_copy_names: false,
            dedup_other_names: true,
            clean_junk: false,
            clean_copy_name: false,
            ..Config::default()
        };
        let task = engine::prepare_at(&root, config, Context::default(), &state).unwrap();
        let db = jchtools_file_core::db::Database::open(&task.directory).unwrap();
        let deletions = db.actions_page_filtered(0, 100, Some("delete")).unwrap();
        assert_eq!(task.summary.planned_delete, 1, "同尺寸同内容必须去重：{names:?}");
        assert_eq!(deletions.len(), 1);
        assert_eq!(deletions[0].source, names[0]);
        assert_eq!(deletions[0].keeper.as_ref().unwrap().0, names[1]);
        drop(db);
        let applied = engine::apply(&task.directory, Context::default()).unwrap();
        assert_eq!(applied.summary.errors, 0);
        assert!(!root.join(names[0]).exists(), "只删除较旧副本");
        let kept_name = std::path::Path::new(names[1]).file_name().unwrap();
        assert_eq!(fs::read(root.join("文档").join(kept_name)).unwrap(), b"same-content");
    }
}

/// 覆盖 C-02/S-03：放开名称范围不能把等尺寸、不同内容文件判成副本。
#[test]
fn other_names_enabled_keeps_same_size_different_content() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("data");
    let state = temp.path().join("state");
    fs::create_dir_all(root.join("A")).unwrap();
    fs::create_dir_all(root.join("B")).unwrap();
    fs::write(root.join("A/report.txt"), b"first").unwrap();
    fs::write(root.join("B/report (1).txt"), b"other").unwrap();
    let config = Config {
        dedup_same_name: false,
        dedup_copy_names: false,
        dedup_other_names: true,
        clean_junk: false,
        clean_copy_name: false,
        ..Config::default()
    };
    let task = engine::prepare_at(&root, config, Context::default(), &state).unwrap();
    assert_eq!(task.summary.planned_delete, 0);
    let applied = engine::apply(&task.directory, Context::default()).unwrap();
    assert_eq!(applied.summary.errors, 0);
    assert_eq!(fs::read(root.join("文档/report.txt")).unwrap(), b"first");
    assert_eq!(fs::read(root.join("文档/report (1).txt")).unwrap(), b"other");
}
