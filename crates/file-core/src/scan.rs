mod emit;
mod records;
mod run;
mod scope;
mod walk;

pub use run::scan;
pub use scope::{
    ensure_git_boundary_checked, program_self_directory_hint, special_prefixes, ScopeFilter,
};
pub use walk::io_pool;

/// 预览格式化：取前 3 项以「、」连接，超出 3 项时追加「 等」（扫描日志共用）。
fn preview3(items: &[String]) -> String {
    let shown = items.iter().take(3).cloned().collect::<Vec<_>>().join("、");
    let more = if items.len() > 3 { " 等" } else { "" };
    format!("{shown}{more}")
}

#[cfg(test)]
use crate::rules;
#[cfg(test)]
use crate::task::{Job, QUARANTINE_DIR_NAME};
#[cfg(test)]
use std::fs;

/// 扫描日志预览格式的特征测试：锁定 preview3 的「前 3 项、顿号连接、超出追加
/// 『 等』」口径（Git 整树提示使用同一格式）。
#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod preview_format_tests {
    use super::preview3;

    #[test]
    fn preview3_shows_three_items_and_marks_more_with_deng() {
        assert_eq!(preview3(&[]), "");
        assert_eq!(preview3(&["a".into()]), "a");
        assert_eq!(preview3(&["a".into(), "b".into(), "c".into()]), "a、b、c");
        assert_eq!(
            preview3(&["a".into(), "b".into(), "c".into(), "d".into()]),
            "a、b、c 等"
        );
    }
}
#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod s05_scope_tests {
    use super::*;
    // 覆盖 S-05（受保护子树剪枝：命中相对前缀整树排除，兄弟目录不受影响）
    #[test]
    fn scope_filter_prunes_protected_subtree_only() {
        let excluded = rules::build_exclusions("").unwrap();
        let filter = ScopeFilter {
            excluded: &excluded,
            include_hidden: true,
            include_system: true,
            quarantine: Some(QUARANTINE_DIR_NAME),
            state_prefix: None,
            exe_prefix: None,
            protected_prefix: Some("WinRoot".to_string()),
            root_under_special: false,
        };
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("x.txt");
        fs::write(&probe, b"x").unwrap();
        let meta = fs::symlink_metadata(&probe).unwrap();
        assert!(
            filter.prunes("WinRoot", "WinRoot", &meta),
            "受保护子树根必须剪枝"
        );
        assert!(
            filter.prunes("WinRoot/System32/a.dll", "a.dll", &meta),
            "受保护子树后代必须剪枝"
        );
        assert!(
            !filter.prunes("WinRootBackup/a.txt", "a.txt", &meta),
            "不得按字符串前缀误伤兄弟目录"
        );
        assert!(!filter.prunes("other/a.txt", "a.txt", &meta));
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn failed_git_boundary_check_aborts_processing() {
        let error = ensure_git_boundary_checked(Some("access denied")).unwrap_err();
        assert!(error.to_string().contains("H-06"));
        assert!(ensure_git_boundary_checked(None).is_ok());
    }

    // 覆盖 X-02 / X-10 / 附录 B：扫描入队与确认清点使用同一序数忽略大小写主干。
    // Windows 专属原因：Unicode 名称等价按产品平台的 Windows 序数语义判定。
    #[cfg(windows)]
    #[test]
    fn old_style_volume_queue_uses_ordinal_stems() {
        for names in [
            ["Ä.zip", "ä.z01"],
            ["ä.z01", "Ä.z02"],
            ["Ä.rar", "ä.r00"],
            ["ä.r00", "Ä.r01"],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("data");
            let state = temp.path().join("state");
            fs::create_dir(&root).unwrap();
            for name in names {
                fs::write(root.join(name), b"original volume").unwrap();
            }
            let db = crate::db::Database::create(&state.join("tasks/probe")).unwrap();
            let mut job = Job {
                root: root.clone(),
                config: crate::config::Config::default(),
                context: crate::control::Context::default(),
                db,
                summary: crate::model::Summary::default(),
            };
            scan(&mut job, true, &state, None).unwrap();
            let queued: i64 = job
                .db
                .conn
                .query_row("SELECT COUNT(*) FROM archives", [], |row| row.get(0))
                .unwrap();
            assert_eq!(queued, 1, "同一老式分卷族只能入队一次：{names:?}");
            assert_eq!(job.summary.scanned, 2);
            assert_eq!(job.summary.errors, 0);
            for name in names {
                assert_eq!(fs::read(root.join(name)).unwrap(), b"original volume");
            }
        }
    }
}
