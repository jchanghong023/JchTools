use super::*;

// 覆盖 X-08：未知Size仍按实际解码逻辑量限制比例，不得绕过防护并删源。
#[test]
    #[rustfmt::skip]
    fn review_unknown_size_stream_obeys_expansion_ratio() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("pack.gz"), b"x").unwrap();
        let engine = review_script_engine(
            temp.path(),
            "echo Path = payload.txt\r\necho Packed Size = 1\r\necho.",
            "echo 0123456789> \"%output%\\payload.txt\"",
        );
        let mut job = review_job(&root, &temp.path().join("state"), 1);
        assert!(engine.extract_one(&mut job, "pack.gz", 0).is_err());
        assert!(root.join("pack.gz").is_file(), "超比例不删源");
        assert!(!root.join("payload.txt").exists(), "检查完成前不得合入成员");
    }

// 覆盖 X-01/X-04/H-07：流中记录的成员名与源包同名时仍保留扩展名，只改冲突主体。
#[test]
    #[rustfmt::skip]
    fn stream_member_matching_source_preserves_recorded_extension() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("pack.gz"), b"source archive").unwrap();
        let engine = review_script_engine(
            temp.path(),
            "echo Path = pack.gz\r\necho Packed Size = 1\r\necho.",
            "echo payload> \"%output%\\pack.gz\"",
        );
        let mut job = review_job(&root, &temp.path().join("state"), 0);
        assert!(engine.extract_one(&mut job, "pack.gz", 0).unwrap());
        assert_eq!(
            fs::read(root.join("pack (1).gz")).unwrap(),
            b"payload\r\n",
            "成员记录的 .gz 扩展名不得因碰到源包而被剥掉"
        );
        assert!(!root.join("pack").exists());
        assert!(!root.join("pack.gz").exists(), "仅完整成功的源包应被删除");
    }

// 覆盖 X-08：整包实际逻辑量不符必须先拒绝，不能合入安全子集。
#[test]
    #[rustfmt::skip]
    fn review_staging_size_validation_precedes_every_merge() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("pack.7z"), b"x").unwrap();
        let engine = review_script_engine(
            temp.path(),
            "echo Path = payload.txt\r\necho Size = 99\r\necho.",
            "echo short> \"%output%\\payload.txt\"",
        );
        let mut job = review_job(&root, &temp.path().join("state"), 0);
        assert!(engine.extract_one(&mut job, "pack.7z", 0).is_err());
        assert!(root.join("pack.7z").is_file());
        assert!(
            !root.join("payload.txt").exists(),
            "总量检查失败的包不得合入任何成员"
        );
    }

// 覆盖 X-08：正常退出遇到暂存文件被占用，必须向界面报告残留位置及原因。
#[test]
    #[rustfmt::skip]
    fn staging_cleanup_failure_reports_location_and_reason() {
        use std::os::windows::fs::OpenOptionsExt;
        use jchtools_file_core::control::Event;
        let temp = tempfile::tempdir().unwrap();
        let (sender, receiver) = jchtools_file_core::control::event_channel();
        let context = TaskContext { events: Some(sender), ..TaskContext::default() };
        let stage = Staging::new(temp.path(), &context).unwrap();
        let directory = stage.directory.clone();
        let owner = stage.owner.clone();
        let locked = stage.content.join("locked.txt");
        fs::write(&locked, b"staged payload").unwrap();
        let guard = fs::OpenOptions::new().read(true).share_mode(1).open(&locked).unwrap();
        drop(stage);
        let event = receiver.try_recv().expect("清理失败必须显示给用户，不能只写诊断日志");
        match event {
            Event::Error(text) => {
                assert!(text.contains(&directory.display().to_string()), "{text}");
                assert!(text.contains("清理失败") && text.contains("原因"), "{text}");
            }
            other => panic!("清理失败应显示错误：{other:?}"),
        }
        assert_eq!(fs::read(&locked).unwrap(), b"staged payload");
        assert_eq!(
            fs::read_to_string(directory.join("OWNER")).unwrap(), owner,
            "内容清理失败时必须保留标记，后续扫描仍能保护残留工作区"
        );
        drop(guard);
    }

// 覆盖 P-08/X-07：指纹只用于入队唯一性，不作为执行时的外部变化复核门。
#[test]
    #[rustfmt::skip]
    fn queued_fingerprint_is_not_rechecked_before_extraction() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let archive = root.join("empty.tar");
        fs::write(&archive, b"archive").unwrap();
        let engine = review_script_engine(temp.path(), "", "");
        let mut job = review_job(&root, &temp.path().join("state"), 0);
        enqueue_archive(&job, &archive, 0).unwrap();
        job.db.conn.execute("UPDATE archives SET fingerprint='queue-identity-only'", []).unwrap();
        extract_queued(&mut job, || SevenZip::with_executable(&engine.executable)).unwrap();
        assert_eq!(job.summary.archives_ok, 1);
        assert_eq!(job.summary.skipped, 0);
        assert!(!archive.exists());
        let state: String = job.db.conn.query_row(
            "SELECT state FROM archives WHERE rel='empty.tar'", [], |row| row.get(0)
        ).unwrap();
        assert_eq!(state, "done");
    }

// 覆盖 X-01/X-08/S-04：用户仅以工作区容器名命名的普通目录不获得固定排除身份。
#[test]
    #[rustfmt::skip]
    fn ordinary_workspace_named_members_are_extracted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("pack.tar"), b"archive").unwrap();
        let engine = review_script_engine(
            temp.path(),
            "echo Path = .jchtools-work/notes.txt\r\necho Packed Size = 1\r\necho.",
            "mkdir \"%output%\\.jchtools-work\"\r\necho payload> \"%output%\\.jchtools-work\\notes.txt\"",
        );
        let mut job = review_job(&root, &temp.path().join("state"), 0);
        assert!(engine.extract_one(&mut job, "pack.tar", 0).unwrap());
        assert_eq!(fs::read(root.join(".jchtools-work/notes.txt")).unwrap(), b"payload\r\n");
        assert!(!root.join("pack.tar").exists());
    }

// 覆盖 X-04/X-08：带所有权标记的历史工作区不接受合入，普通容器本身仍可复用。
#[test]
    #[rustfmt::skip]
    fn marked_workspace_destination_uses_a_new_directory() {
        let root = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        let owner = "11111111-1111-4111-8111-111111111111";
        let relative = format!(".jchtools-work/{owner}");
        let workspace = root.path().join(&relative);
        fs::create_dir_all(workspace.join("content")).unwrap();
        fs::write(workspace.join("OWNER"), owner).unwrap();
        fs::write(workspace.join("content/keep.txt"), b"foreign payload").unwrap();
        fs::create_dir_all(stage.path().join(&relative)).unwrap();
        fs::write(stage.path().join(&relative).join("fresh.txt"), b"incoming").unwrap();
        let renames = plan_directory_renames(
            &Control::default(), stage.path(), root.path(), &mut HashSet::new()
        ).unwrap();
        assert_eq!(
            mapped_entry_rel(&renames, &format!("{relative}/fresh.txt")),
            Some(format!(".jchtools-work/{owner} (1)/fresh.txt"))
        );
        assert_eq!(fs::read(workspace.join("content/keep.txt")).unwrap(), b"foreign payload");
        assert_eq!(fs::read_to_string(workspace.join("OWNER")).unwrap(), owner);
    }

// 覆盖 H-06/X-08：Git 边界先于工作区标记检查，不进入 Git 树读取 OWNER。
#[test]
    #[rustfmt::skip]
    fn git_boundary_precedes_workspace_marker_reads() {
        let root = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        let owner = "11111111-1111-4111-8111-111111111111";
        let relative = format!(".jchtools-work/{owner}");
        let git = root.path().join(&relative);
        fs::create_dir_all(git.join(".git")).unwrap();
        // 误把 OWNER 目录当工作区标记时 helper 会报错；Git 检查应直接保护整树。
        fs::create_dir(git.join("OWNER")).unwrap();
        fs::create_dir(git.join("content")).unwrap();
        assert!(GitBoundaries::default().blocked(root.path(), &git).unwrap());
        fs::create_dir_all(stage.path().join(&relative)).unwrap();
        let renames = plan_directory_renames(
            &Control::default(), stage.path(), root.path(), &mut HashSet::new()
        ).unwrap();
        assert_eq!(
            mapped_entry_rel(&renames, &relative),
            Some(format!(".jchtools-work/{owner} (1)"))
        );
        assert_eq!(crate::engine::count_archives(root.path(), &Config::default()).unwrap(), 0);
        assert!(git.join("OWNER").is_dir());
    }

// 覆盖 H-06/S-01/X-08：工作区容器被文件或 Git 占用时同卷另选空闲名称。
#[test]
    #[rustfmt::skip]
    fn staging_avoids_file_and_git_container_occupants() {
        for git in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let fixed = root.path().join(".jchtools-work");
            if git {
                fs::create_dir_all(fixed.join(".git")).unwrap();
                fs::write(fixed.join(".git/HEAD"), b"original git metadata").unwrap();
            } else {
                fs::write(&fixed, b"ordinary user file").unwrap();
            }
            fs::create_dir(root.path().join(".jchtools-work (1)")).unwrap();
            let stage = Staging::new(root.path(), &TaskContext::default()).unwrap();
            assert_eq!(stage.directory.parent().unwrap(), root.path().join(".jchtools-work (2)"));
            assert!(fsutil::is_extraction_workspace(&stage.directory).unwrap());
            drop(stage);
            assert!(root.path().join(".jchtools-work (1)").is_dir(), "不删用户已有空容器");
            assert!(!root.path().join(".jchtools-work (2)").exists(), "只清理本次自建容器");
            if git {
                assert_eq!(fs::read(fixed.join(".git/HEAD")).unwrap(), b"original git metadata");
                assert_eq!(fs::read_dir(&fixed).unwrap().count(), 1, "不能进入 Git 树暂存");
            } else {
                assert_eq!(fs::read(&fixed).unwrap(), b"ordinary user file");
            }
        }
    }

// 覆盖 X-08/H-07：复用的普通用户容器不因暂存子树清空而被删除。
#[test]
    #[rustfmt::skip]
    fn staging_preserves_preexisting_plain_container() {
        let root = tempfile::tempdir().unwrap();
        let fixed = root.path().join(".jchtools-work");
        fs::create_dir(&fixed).unwrap();
        let stage = Staging::new(root.path(), &TaskContext::default()).unwrap();
        assert_eq!(stage.directory.parent().unwrap(), fixed);
        drop(stage);
        assert!(fixed.is_dir());
        assert_eq!(fs::read_dir(fixed).unwrap().count(), 0);
    }

// 覆盖 X-08：选择临时容器前响应取消，不创建任何后续目录。
#[test]
    #[rustfmt::skip]
    fn cancelled_staging_does_not_create_a_container() {
        let root = tempfile::tempdir().unwrap();
        let context = TaskContext::default();
        context.control.cancel();
        let error = Staging::new(root.path(), &context).err().expect("取消后不得创建工作区");
        assert!(error.to_string().contains("取消"), "{error:#}");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
