use super::actions::*;
use super::planning::*;
use super::*;
use jchtools_file_core::{
    config::{Config, DeleteMode},
    control::Context as TaskContext,
    db::Database,
    fsutil,
    model::{Action, ActionKind, Summary},
    platform::DeleteResult,
    rules,
    task::Job,
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

// 平台门禁原因：仅 Windows 门禁测试使用（UNC/盘符前缀拼接），非 Windows 无使用者。
#[cfg(windows)]
    #[rustfmt::skip]
    const BSLASH: char = std::path::MAIN_SEPARATOR; // Windows 下为反斜杠

// 覆盖 C-10/C-11：取消不丢失同批已完成的删除结果。
#[test]
    #[rustfmt::skip]
    fn cancelled_parallel_run_records_completed_deletions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("cancelled.txt"), b"keep").unwrap();
        fs::write(root.join("completed.txt"), b"done").unwrap();
        let db = Database::create(&temp.path().join("state")).unwrap();
        let mut actions = Vec::new();
        for source in ["cancelled.txt", "completed.txt"] {
            let mut action = Action {
                source: source.into(),
                expected: Some(fsutil::snapshot(&root.join(source)).unwrap()),
                ..Action::default()
            };
            action.id = db.add_action(&action).unwrap();
            actions.push(action);
        }
        fs::remove_file(root.join("completed.txt")).unwrap();
        let context = TaskContext::default();
        context.control.cancel();
        let mut job = Job {
            root: root.clone(),
            config: Config::default(),
            context,
            db,
            summary: Summary::default(),
        };
        let result = settle_parallel_results(
            &mut job,
            &actions,
            vec![
                Err(anyhow::anyhow!("任务已取消")),
                Ok(Some(DeleteResult::Permanent)),
            ],
            &mut HashSet::new(),
        );
        assert!(result.is_err(), "任务仍应按取消收尾");
        assert_eq!(job.db.action(actions[1].id).unwrap().state, "done");
        assert_eq!(job.db.action(actions[0].id).unwrap().state, "pending");
        assert_eq!(job.summary.deleted, 1);
        assert_eq!(job.summary.permanent_bytes, 4);
        assert_eq!(job.summary.errors, 0);
        assert_eq!(job.context.control.completed.load(Ordering::Relaxed), 1);
        assert!(root.join("cancelled.txt").exists());
        assert!(!root.join("completed.txt").exists());
    }
#[test]
    #[rustfmt::skip]
    fn git_root_protection_uses_path_component_boundaries() {
        assert!(path_is_at_or_below("图片", "图片"));
        assert!(path_is_at_or_below("图片/logo.png", "图片"));
        assert!(!path_is_at_or_below("图片旧/logo.png", "图片"));
        assert!(!path_is_at_or_below("other/图片/logo.png", "图片"));
    }

#[test]
    #[rustfmt::skip]
    fn failed_git_move_blocks_tree_targets_but_not_independent_moves() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        std::fs::create_dir_all(root.join("图片/.git")).unwrap();
        std::fs::write(root.join("图片/.git/HEAD"), b"ref").unwrap();
        std::fs::write(root.join("photo.png"), b"photo").unwrap();
        std::fs::write(root.join("other.txt"), b"other").unwrap();

        let state = temp.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let db = Database::create(&state.join("task")).unwrap();
        db.set("git_roots", &vec!["图片".to_string()]).unwrap();
        let git_id = db
            .add_action(&Action {
                kind: ActionKind::Move,
                source: "图片".into(),
                target: Some("图片/inner".into()),
                reason: "test Git move failure".into(),
                ..Action::default()
            })
            .unwrap();
        let photo_id = db
            .add_action(&Action {
                kind: ActionKind::Move,
                source: "photo.png".into(),
                target: Some("图片/photo.png".into()),
                reason: "test protected-tree target".into(),
                ..Action::default()
            })
            .unwrap();
        let other_id = db
            .add_action(&Action {
                kind: ActionKind::Move,
                source: "other.txt".into(),
                target: Some("文档/other.txt".into()),
                reason: "independent move".into(),
                ..Action::default()
            })
            .unwrap();
        let mut job = Job {
            root: root.clone(),
            config: Config::default(),
            context: TaskContext::default(),
            db,
            summary: Summary::default(),
        };

        execute_planned_moves(&mut job).unwrap();

        assert_eq!(job.db.action(git_id).unwrap().state, "failed");
        assert_eq!(job.db.action(photo_id).unwrap().state, "failed");
        assert_eq!(job.db.action(other_id).unwrap().state, "done");
        assert!(root.join("图片/.git/HEAD").exists());
        assert!(root.join("photo.png").exists());
        assert!(!root.join("图片/photo.png").exists());
        assert!(root.join("文档/other.txt").exists());
    }
#[test]
    #[rustfmt::skip]
    fn move_stage_guard_keeps_payload_when_cleanup_is_not_safe() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(".jchtools-move-work").join("owner");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("OWNER"), "owner").unwrap();
        std::fs::write(directory.join("唯一暂存副本"), b"payload").unwrap();
        {
            let _guard = MoveStageGuard {
                directory: directory.clone(),
                owner: "owner".into(),
                initialized: true,
            };
        }
        assert!(directory.join("唯一暂存副本").is_file());
        assert!(directory.join("OWNER").is_file());
    }

#[test]
    #[rustfmt::skip]
    fn review_f20_owned_stage_is_source_local_and_preserves_failed_payload() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("source");
        fs::create_dir(&parent).unwrap();
        let user_container = parent.join(".jchtools-work");
        fs::create_dir(&user_container).unwrap();
        fs::write(user_container.join("user.txt"), b"untouched").unwrap();
        let guard = MoveStageGuard::new(&parent).unwrap();
        let directory = guard.directory.clone();
        let owner = guard.owner.clone();
        let container = directory.parent().unwrap().to_path_buf();
        assert_eq!(container.parent(), Some(parent.as_path()));
        assert_ne!(container, user_container);
        assert!(fsutil::is_extraction_workspace(&directory).unwrap());
        let payload = directory.join("content/only.txt");
        fs::write(&payload, b"unique staged bytes").unwrap();
        drop(guard);
        assert_eq!(fs::read(&payload).unwrap(), b"unique staged bytes");
        assert!(fsutil::is_extraction_workspace(&directory).unwrap());
        assert_eq!(fs::read(user_container.join("user.txt")).unwrap(), b"untouched");
        fs::remove_file(payload).unwrap();
        drop(MoveStageGuard { directory, owner, initialized: true });
        assert!(!container.exists());
        assert!(user_container.exists());
    }

#[test]
    #[rustfmt::skip]
    fn review_f19_failed_deletion_does_not_reserve_success_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let left = root.join("left.tmp");
        let right = root.join("right.tmp");
        fs::write(&left, b"one payload").unwrap();
        fs::hard_link(&left, &right).unwrap();
        let db = Database::create(&temp.path().join("state")).unwrap();
        let mut actions = Vec::new();
        for source in ["left.tmp", "right.tmp"] {
            let mut action = Action {
                source: source.into(),
                expected: Some(fsutil::snapshot(&root.join(source)).unwrap()),
                ..Action::default()
            };
            action.id = db.add_action(&action).unwrap();
            actions.push(action);
        }
        let mut job = Job {
            root,
            config: Config::default(),
            context: TaskContext::default(),
            db,
            summary: Summary::default(),
        };
        let mut identities = HashSet::new();
        settle_parallel_results(
            &mut job,
            &actions[..1],
            vec![Err(anyhow::anyhow!("结算单元测试：第一项删除失败"))],
            &mut identities,
        ).unwrap();
        assert!(identities.is_empty());
        fs::remove_file(&right).unwrap();
        settle_parallel_results(
            &mut job,
            &actions[1..],
            vec![Ok(Some(DeleteResult::Permanent))],
            &mut identities,
        ).unwrap();
        assert_eq!(job.summary.deleted, 1);
        assert_eq!(job.summary.permanent_bytes, 11);
        assert_eq!(job.summary.errors, 1);
        assert!(left.exists());
        assert!(!right.exists());
    }
// apply 锁目录决策：recorded 有效优先；失效时仅 tasks/ 布局可回退推导。
#[test]
    #[rustfmt::skip]
    fn recorded_state_dir_takes_priority() {
        let temp = tempfile::tempdir().unwrap();
        let tasks = temp.path().join("tasks").join("t1");
        std::fs::create_dir_all(&tasks).unwrap();
        assert_eq!(
            lock_dir_for(&tasks, Some(temp.path())).unwrap(),
            *temp.path()
        );
    }
#[test]
    #[rustfmt::skip]
    fn invalid_record_with_tasks_layout_falls_back_to_parent() {
        let temp = tempfile::tempdir().unwrap();
        let tasks = temp.path().join("tasks").join("t1");
        std::fs::create_dir_all(&tasks).unwrap();
        let gone = temp.path().join("gone"); // 不存在 → recorded 过滤失效
        assert_eq!(lock_dir_for(&tasks, Some(&gone)).unwrap(), *temp.path());
        assert_eq!(lock_dir_for(&tasks, None).unwrap(), *temp.path());
    }
// 平台门禁原因：断言依赖 Windows 路径语义（盘符前缀与 \server\share UNC 前缀的
// components 解析），Unix 上分隔符不同、该解析不存在；OS 无关的三条决策测试保持全平台。
#[cfg(windows)]
    #[test]
    #[rustfmt::skip]
    fn tasks_layout_at_drive_root_is_rejected() {
        // 盘根/共享根不是状态目录：在那里落锁会留下陌生位置的锁文件（engine 不挂载
        // 任何盘，这里用纯路径验证决策逻辑，不触碰文件系统；UNC 前缀用 BSLASH 拼接，
        // 避免源码转义歧义）。
        assert!(
            lock_dir_for(Path::new(r"Q:\tasks\t1"), None).is_err(),
            "盘根 tasks 布局必须拒绝"
        );
        let unc = [BSLASH, BSLASH].iter().collect::<String>()
            + "server"
            + &BSLASH.to_string()
            + "share"
            + &BSLASH.to_string()
            + "tasks"
            + &BSLASH.to_string()
            + "t1";
        assert!(
            lock_dir_for(Path::new(&unc), None).is_err(),
            "UNC 共享根 tasks 布局必须拒绝"
        );
    }
// 平台门禁原因：同上，断言 Windows 前缀路径的解析结果。
#[cfg(windows)]
    #[test]
    #[rustfmt::skip]
    fn tasks_layout_deep_on_unc_share_is_allowed() {
        // UNC 上正常深度的状态目录（如 \\server\share\JchTools\data）必须放行：
        // 前缀 \\server\share 不可拆分，不能用「祖先层数少」误判为根。
        let unc = [BSLASH, BSLASH].iter().collect::<String>()
            + "server"
            + &BSLASH.to_string()
            + "share"
            + &BSLASH.to_string()
            + "JchTools"
            + &BSLASH.to_string()
            + "data"
            + &BSLASH.to_string()
            + "tasks"
            + &BSLASH.to_string()
            + "t1";
        let expected = [BSLASH, BSLASH].iter().collect::<String>()
            + "server"
            + &BSLASH.to_string()
            + "share"
            + &BSLASH.to_string()
            + "JchTools"
            + &BSLASH.to_string()
            + "data";
        assert_eq!(
            lock_dir_for(Path::new(&unc), None).unwrap(),
            PathBuf::from(expected),
            "UNC 深目录 tasks 布局必须放行"
        );
        let win = Path::new(r"C:\state\tasks\t1");
        assert_eq!(lock_dir_for(win, None).unwrap(), PathBuf::from(r"C:\state"));
    }
#[test]
    #[rustfmt::skip]
    fn non_tasks_layout_without_record_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("t1");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            lock_dir_for(&dir, None).is_err(),
            "非 tasks 布局且无记录必须拒绝"
        );
    }
#[rustfmt::skip]
    fn r01_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let state = temp.path().join("state");
        (temp, root, state)
    }
// 覆盖 S-04/H-05/附录 C：真实公开入口验证不附加虚构末尾分隔符。
#[test]
    #[rustfmt::skip]
    fn r01_exclusions_use_actual_relative_paths() {
        let (_temp, root, state) = r01_fixture();
        fs::create_dir(root.join("cache")).unwrap();
        fs::write(root.join("keep.txt"), b"retained").unwrap();
        let task = prepare_at(&root, Config {
            exclusions: "cache/*;keep.txt/".into(),
            ..Config::default()
        }, TaskContext::default(), &state).unwrap();
        assert_eq!(task.summary.scanned, 1);
        assert_eq!(task.summary.planned_empty, 1);
        apply(&task.directory, TaskContext::default()).unwrap();
        assert!(!root.join("cache").exists());
        assert_eq!(fs::read(root.join("文档/keep.txt")).unwrap(), b"retained");
    }

// 覆盖 C-09：选择隔离容器本身也不能纳入其普通文件。
#[test]
    #[rustfmt::skip]
    fn r01_selected_quarantine_root_keeps_payloads() {
        let (_temp, root, state) = r01_fixture();
        let selected = root.join("解压失败");
        fs::create_dir(&selected).unwrap();
        fs::write(selected.join("Thumbs.db"), b"original").unwrap();
        fs::write(selected.join("report.txt"), b"original").unwrap();
        let context = TaskContext::default();
        let control = context.control.clone();
        let task = prepare_at(&selected, Config {
            dedup_other_names: true,
            ..Config::default()
        }, context, &state).unwrap();
        assert_eq!(task.summary.scanned, 0);
        assert_eq!(task.summary.planned_delete, 0);
        assert_eq!(task.summary.planned_move, 0);
        assert_eq!(control.read_bytes.load(Ordering::Relaxed), 0);
        apply(&task.directory, TaskContext::default()).unwrap();
        assert_eq!(fs::read(selected.join("Thumbs.db")).unwrap(), b"original");
        assert_eq!(fs::read(selected.join("report.txt")).unwrap(), b"original");
    }

// 覆盖 C-09：真实扫描产生的跳过提示包含深层隔离目录路径。
#[test]
    #[rustfmt::skip]
    fn r01_nested_quarantine_skip_reports_path() {
        let (_temp, root, state) = r01_fixture();
        fs::create_dir_all(root.join("outer/解压失败")).unwrap();
        fs::write(root.join("outer/解压失败/Thumbs.db"), b"original").unwrap();
        let task = prepare_at(&root, Config::default(), TaskContext::default(), &state).unwrap();
        assert_eq!(task.summary.scanned, 0);
        let db = Database::open_existing(&task.directory).unwrap();
        let notices: i64 = db.conn.query_row(
            "SELECT count(*) FROM events WHERE source=?1 AND reason LIKE '%跳过%'",
            ["outer/解压失败"], |row| row.get(0),
        ).unwrap();
        assert!(notices > 0, "必须明确提示发现的隔离路径");
        drop(db);
        apply(&task.directory, TaskContext::default()).unwrap();
        assert_eq!(fs::read(root.join("outer/解压失败/Thumbs.db")).unwrap(), b"original");
    }

// 覆盖 C-19/C-20：长度退让后的真实计划只扩展摘要、不恢复来源。
#[test]
    #[rustfmt::skip]
    fn r01_conflict_yield_keeps_source_free_digest() {
        let (_temp, root, state) = r01_fixture();
        let first = format!("{}/x/report.txt", "a".repeat(35));
        let second = format!("{}/x/report.txt", "b".repeat(35));
        for (name, bytes) in [(&first, b"first".as_slice()), (&second, b"second".as_slice())] {
            fs::create_dir_all(root.join(name).parent().unwrap()).unwrap();
            fs::write(root.join(name), bytes).unwrap();
        }
        fs::create_dir(root.join("文档")).unwrap();
        let fixed = format!("文档/report_{}.txt", rules::path_digest(&first, 8));
        fs::write(root.join(&fixed), b"occupied").unwrap();
        let task = prepare_at(&root, Config {
            global_delete: DeleteMode::Keep,
            dedup_same_name: false,
            dedup_copy_names: false,
            dedup_other_names: false,
            ..Config::default()
        }, TaskContext::default(), &state).unwrap();
        let db = Database::open_existing(&task.directory).unwrap();
        let expected = format!("文档/report_{}.txt", rules::path_digest(&first, 10));
        let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
        assert_eq!(moves.iter().find(|action| action.source == first).unwrap().target.as_deref(), Some(expected.as_str()));
        drop(db);
        apply(&task.directory, TaskContext::default()).unwrap();
        assert_eq!(fs::read(root.join(expected)).unwrap(), b"first");
        assert_eq!(fs::read(root.join(fixed)).unwrap(), b"occupied");
    }

// 覆盖 C-01/C-12：清理判断先于哈希，单一存活者无需读取内容。
#[test]
    #[rustfmt::skip]
    fn r01_cleanup_precedes_candidate_hashing() {
        let (_temp, root, state) = r01_fixture();
        fs::write(root.join("keep.txt"), b"identical").unwrap();
        fs::write(root.join("temporary.tmp"), b"identical").unwrap();
        let context = TaskContext::default();
        let control = context.control.clone();
        let task = prepare_at(&root, Config {
            clean_temp: true,
            dedup_other_names: true,
            ..Config::default()
        }, context, &state).unwrap();
        assert_eq!(control.read_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(task.summary.planned_delete, 1);
        apply(&task.directory, TaskContext::default()).unwrap();
        assert_eq!(fs::read(root.join("文档/keep.txt")).unwrap(), b"identical");
        assert!(!root.join("temporary.tmp").exists());
    }
// 覆盖 P-10/S-01：独立进程安装真实磁盘logger，目标容器被文件占用仍保留源项。
#[test]
    #[rustfmt::skip]
    fn r01_failed_item_reason_is_written_to_isolated_disk_log() {
        const CHILD_ENV: &str = "JCH_R01_DIAGNOSTIC_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "engine::lock_tests::r01_failed_item_reason_is_written_to_isolated_disk_log", "--nocapture"])
                .env(CHILD_ENV, "1")
                .output().unwrap();
            assert!(output.status.success(), "隔离子进程失败：{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            return;
        }
        let (_temp, root, state) = r01_fixture();
        let guard = jchtools_foundation::logging::init(&state).expect("独立测试进程必须初始化隔离磁盘日志");
        fs::write(root.join("文档"), b"container occupied").unwrap();
        fs::write(root.join("blocked.txt"), b"source preserved").unwrap();
        let planned = prepare_at(&root, Config::default(), TaskContext::default(), &state).unwrap();
        assert!(planned.summary.errors > 0);
        assert_eq!(planned.summary.planned_move, 0);
        let applied = apply(&planned.directory, TaskContext::default()).unwrap();
        assert!(applied.summary.errors > 0);
        assert_eq!(fs::read(root.join("blocked.txt")).unwrap(), b"source preserved");
        assert_eq!(fs::read(root.join("文档")).unwrap(), b"container occupied");
        drop(guard);
        let found = fs::read_dir(state.join(jchtools_foundation::logging::LOG_DIR)).unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("log"))
            .any(|entry| fs::read_to_string(entry.path()).unwrap().lines().any(|line| {
                line.contains("task_item_failed") && line.contains("blocked.txt")
                    && line.contains("reason") && line.contains("同名文件占用")
                    && line.contains("已保留源项")
            }));
        assert!(found, "实际磁盘逐文件失败日志必须包含去敏原因，不能只给source/result");
    }
