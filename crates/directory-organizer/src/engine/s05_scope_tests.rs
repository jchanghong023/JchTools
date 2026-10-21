use super::cleanup::final_empty_cleanup;
use super::*;
use jchtools_file_core::{
    config::Config, control::Context as TaskContext, db::Database, task::Job,
};
use std::{fs, path::PathBuf};

/// S-05 合成受保护子树夹具：root/WinFake 内含「系统样」文件，root 内含普通文件。
    /// 不用真实系统目录做破坏性实验；返回（临时目录、根、受保护目录的规范化路径）。
    #[rustfmt::skip]
    fn synthetic_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir_all(root.join("WinFake/System32")).unwrap();
        fs::write(root.join("WinFake/System32/a.dll"), b"sys").unwrap();
        fs::write(root.join("normal.txt"), b"n").unwrap();
        let protected = fs::canonicalize(root.join("WinFake")).unwrap();
        (temp, root, protected)
    }
// 覆盖 S-05（端到端：合成受保护子树位于普通根内——分析扫描不进入、
// 确认计数只含范围内的文件、范围提示写明已排除系统目录）
#[test]
    #[rustfmt::skip]
    fn prepare_excludes_synthetic_protected_subtree() {
        let (temp, root, protected) = synthetic_fixture();
        let state = temp.path().join("state");
        let result = prepare_at_with(
            &root,
            Config::default(),
            TaskContext::default(),
            &state,
            Some(&protected),
        )
        .unwrap();
        assert_eq!(result.summary.scanned, 1, "受保护子树内文件不入盘点");
        let db = Database::open_existing(&result.directory).unwrap();
        let text = db
            .conn
            .query_row(
                "SELECT group_concat(reason, ' | ') FROM events",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert!(
            text.contains("已排除 Windows 系统目录"),
            "范围提示必须写明已排除系统目录：{text}"
        );
    }
// 覆盖 S-05, H-05（收尾空目录清理不进入受保护子树；范围外普通空目录仍正常清理）
#[test]
    #[rustfmt::skip]
    fn final_cleanup_keeps_synthetic_protected_subtree() {
        let (temp, root, protected) = synthetic_fixture();
        fs::create_dir_all(root.join("WinFake/内空")).unwrap();
        fs::create_dir_all(root.join("普通空")).unwrap();
        let canonical_root = fs::canonicalize(&root).unwrap();
        let state = temp.path().join("state");
        let mut job = Job {
            root: canonical_root,
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&state).unwrap(),
            summary: jchtools_file_core::model::Summary::default(),
        };
        final_empty_cleanup(&mut job, Some(&protected)).unwrap();
        assert!(
            root.join("WinFake/System32/a.dll").is_file(),
            "受保护子树内容零改动"
        );
        assert!(
            root.join("WinFake/内空").is_dir(),
            "受保护子树内的空目录同样不得清理"
        );
        assert!(
            !root.join("普通空").exists(),
            "范围外的普通空目录仍按 H-05 清理"
        );
    }
