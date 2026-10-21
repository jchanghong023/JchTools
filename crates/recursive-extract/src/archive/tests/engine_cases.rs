use super::*;

// 仅验证锁定引擎系统诊断协议与本机 FormatMessage 的连接；
// 不冒充真实子进程 WriteFile 故障或物理卷写满证据。
#[test]
    #[rustfmt::skip]
    fn review_f11_engine_space_diagnostic_requires_exact_system_block() {
        let messages = engine_space_messages();
        for (code, message) in &messages {
            let mut pending = false;
            assert!(engine_space_error_line(&mut pending, &messages, message).is_none());
            assert!(engine_space_error_line(&mut pending, &messages, "System ERROR:").is_none());
            let error = engine_space_error_line(&mut pending, &messages, message).unwrap();
            assert_eq!(error.raw_os_error(), Some(*code));
            assert!(stops_extraction(&anyhow::Error::new(error).context("真实解码输出边界")));
            assert!(engine_space_error_line(&mut pending, &messages, message).is_none());
            assert!(engine_space_error_line(&mut pending, &messages, "System ERROR:").is_none());
            assert!(engine_space_error_line(&mut pending, &messages, "Data Error").is_none());
            assert!(engine_space_error_line(&mut pending, &messages, message).is_none());
            let extract_result = engine_space_error_line(&mut pending, &messages, &format!("ERROR: {message}"));
            assert_eq!(extract_result.as_ref().and_then(std::io::Error::raw_os_error),
                (*code == 39).then_some(39));
            assert!(engine_space_error_line(&mut pending, &messages,
                &format!("ERROR: Data Error : {message}")).is_none());
        }
        let mut pending = false;
        assert!(engine_space_error_line(&mut pending, &messages, "System ERROR:").is_none());
        assert!(engine_space_error_line(&mut pending, &messages,
            &std::io::Error::from_raw_os_error(5).to_string()).is_none());
        assert!(engine_space_error_line(&mut pending, &messages, "7-Zip 退出码 2").is_none());
    }

#[test]
    #[rustfmt::skip]
    fn review_f13_folded_mapping_slices_original_non_bmp_boundaries() {
        let renames = HashMap::from([
            (fsutil::fold_rel("😀/σ"), "😀/σ (1)".to_string()),
        ]);
        assert_eq!(mapped_entry_rel(&renames, "😀/ς/child.txt"),
            Some("😀/σ (1)/child.txt".to_string()));
        assert_eq!(mapped_entry_rel(&renames, "😀/Σ"), Some("😀/σ (1)".to_string()));
        assert_eq!(mapped_entry_rel(&renames, "😀/σ-other/child.txt"), None);
    }

// 只在随机目录上继承拒绝创建子目录：OWNER 可写，content 创建必须真实失败。
#[test]
    #[rustfmt::skip]
    fn review_f15_content_initialization_failure_cleans_owned_directory() {
        let root = tempfile::tempdir().unwrap();
        let container = root.path().join(".jchtools-work");
        fs::create_dir(&container).unwrap();
        let status = Command::new("icacls.exe").arg(&container)
            .args(["/deny", "*S-1-1-0:(OI)(CI)(IO)(AD)"]).status().unwrap();
        assert!(status.success(), "测试前提：仅子目录继承拒绝创建目录的 ACE");
        let result = Staging::new(root.path(), &TaskContext::default());
        let error = result.err().expect("content 目录创建必须由真实 ACL 拒绝");
        assert!(format!("{error:#}").contains("无法初始化解压内容目录"), "{error:#}");
        assert_eq!(fs::read_dir(&container).unwrap().count(), 0,
            "content 初始化失败同样必须清理本次随机目录");
        assert!(container.is_dir(), "保留预存容器");
    }

// S-04 / X-10 / X-06：扩展缺首卷组不能重新吸入用户明确排除的兄弟卷。
#[test]
    #[rustfmt::skip]
    fn review_f01_missing_entry_preserves_excluded_volume() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        for name in ["pack.part02.rar", "pack.part03.rar"] {
            fs::write(root.join(name), name.as_bytes()).unwrap();
        }
        let mut job = review_job(&root, &temp.path().join("state"), 0);
        job.config.exclusions = "pack.part03.rar".to_string();
        enqueue_archive(&job, &root.join("pack.part02.rar"), 0).unwrap();
        extract_queued(&mut job, || {
            panic!("缺首卷且跨越显式排除边界的卷组不得访问引擎")
        })
        .unwrap();
        assert_eq!(
            fs::read(root.join("pack.part03.rar")).unwrap(),
            b"pack.part03.rar",
            "被排除的卷必须留在原位置，不能被隔离或删除"
        );
        assert!(!root.join("解压失败/pack.part03.rar").exists());
        assert_eq!(fs::read(root.join("pack.part02.rar")).unwrap(), b"pack.part02.rar");
        assert!(!root.join(QUARANTINE_DIR_NAME).exists(), "范围受限组不能隔离任何卷");
        assert_eq!(job.summary.deleted, 0);
    }

// X-08：未知体积在开始解码前仍须满足预留；无需写满或模拟磁盘。
#[test]
    #[rustfmt::skip]
    fn review_f10_unknown_size_checks_reserve_before_decoding() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("keep.gz"), b"untouched source").unwrap();
        let mut job = review_job(&root, &temp.path().join("state"), 0);
        job.config.reserve_bytes = 9_223_372_036_854_775_807;
        assert!(fs2::available_space(&root).unwrap() < job.config.reserve_bytes);
        let result = check_expansion_space(&job, None, 16, 0);
        assert!(
            result.as_ref().is_err_and(stops_extraction),
            "未知体积也必须在预留已不足时返回任务停止信号：{result:?}"
        );
        assert_eq!(fs::read(root.join("keep.gz")).unwrap(), b"untouched source");
        assert!(!root.join(".jchtools-work").exists());
    }

// X-06 / U-10：首卷已移动、其余卷被真实共享句柄阻止移动时，失败明细须列全位置。
#[test]
    #[rustfmt::skip]
    fn review_f14_partial_quarantine_reports_every_remaining_volume() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let names = ["pack.7z.002", "pack.7z.003", "pack.7z.004"];
        for name in names {
            fs::write(root.join(name), name.as_bytes()).unwrap();
        }
        let guards: Vec<_> = names[1..]
            .iter()
            .map(|name| fs::OpenOptions::new().read(true).share_mode(1).open(root.join(name)).unwrap())
            .collect();
        let mut job = review_job(&root, &temp.path().join("state"), 0);
        enqueue_archive(&job, &root.join(names[0]), 0).unwrap();
        extract_queued(&mut job, || panic!("缺首卷预检不应启动引擎")).unwrap();
        assert_eq!(fs::read(root.join("解压失败").join(names[0])).unwrap(), names[0].as_bytes());
        let detail: String = job.db.conn.query_row(
            "SELECT group_concat(source || '|' || target || '|' || reason, char(10)) FROM events",
            [], |row| row.get(0),
        ).unwrap();
        for name in &names[1..] {
            assert_eq!(fs::read(root.join(name)).unwrap(), name.as_bytes());
            assert!(
                detail.contains(name),
                "隔离失败明细必须包含每个未移动源卷，不能只声称代表卷原地保留：{name}\n{detail}"
            );
        }
        assert_eq!(job.summary.deleted, 0);
        drop(guards);
    }

// X-08：真实继承 ACL 允许分配随机目录、拒绝创建 OWNER；初始化失败也须收尾。
#[test]
    #[rustfmt::skip]
    fn review_f15_staging_initialization_failure_cleans_owned_directory() {
        let root = tempfile::tempdir().unwrap();
        let container = root.path().join(".jchtools-work");
        fs::create_dir(&container).unwrap();
        let status = Command::new("icacls.exe")
            .arg(&container)
            .args(["/deny", "*S-1-1-0:(OI)(CI)(WD)"])
            .status()
            .unwrap();
        assert!(status.success(), "测试前提：仅拒绝创建文件，允许创建/删除目录");
        let context = TaskContext::default();
        assert!(Staging::new(root.path(), &context).is_err(), "OWNER 写入必须由 ACL 确实拒绝");
        let remaining: Vec<_> = fs::read_dir(&container).unwrap().map(|entry| entry.unwrap().path()).collect();
        assert!(
            remaining.is_empty(),
            "初始化失败必须删除本次独占创建且可删除的目录，不能留下无完整所有权标记的工作区：{remaining:?}"
        );
        assert!(container.is_dir(), "不能删除本来就存在的用户容器");
    }

// 覆盖 X-08：根目录条目虽不创建目录，仍计入单包条目上限，重复条目也不豁免。
#[test]
    #[rustfmt::skip]
    fn root_directory_entries_count_toward_entry_limit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let archive = root.join("pack.tar");
        fs::write(&archive, b"archive").unwrap();
        let engine = review_script_engine(
            temp.path(),
            "echo Path = .\r\necho Folder = +\r\necho.\r\n\
             echo Path = ./\r\necho Folder = +\r\necho.",
            "",
        );
        let mut job = review_job(&root, &temp.path().join("state"), 0);
        job.config.max_entries = 2;
        assert_eq!(engine.list(&archive, &mut job).unwrap().total, 0);
        job.config.max_entries = 1;
        let error = engine.list(&archive, &mut job).err().expect("根目录条目不得绕过条目上限");
        assert!(error.to_string().contains("条目数量超过"), "{error:#}");
        assert_eq!(fs::read(&archive).unwrap(), b"archive");
        assert!(!root.join(".jchtools-work").exists(), "超限应在解码前拒绝");

        // 公开入口同样必须在解码前拒绝、整包隔离且不删源，不能把空输出视为成功。
        let result = crate::engine::extract_run_at(
            &root,
            Config { max_entries: 1, reserve_bytes: 0, max_ratio: 0, ..Config::default() },
            TaskContext::default(),
            &temp.path().join("public-state"),
            Some(&engine.executable),
        ).unwrap();
        assert_eq!(result.summary.archives_ok, 0);
        assert_eq!(result.summary.archives_failed, 1);
        assert_eq!(result.summary.deleted, 0);
        assert_eq!(result.summary.archives_quarantined, 1);
        assert_eq!(fs::read(root.join("解压失败/pack.tar")).unwrap(), b"archive");
    }

// 覆盖 X-06/X-08：磁盘写满的原生错误即使被上下文包装，也必须停止而非隔离源包。
// 只验证错误链分类；真实 7-Zip 写满卷仍需独立运行环境，不能据此冒报。
#[test]
    #[rustfmt::skip]
    fn review_f11_native_disk_full_stops_but_other_io_errors_do_not() {
        for code in [112, 39] {
            let error = anyhow::Error::new(std::io::Error::from_raw_os_error(code))
                .context("解码输出写入失败");
            assert!(
                stops_extraction(&error),
                "Windows 磁盘写满错误 {code} 不得按坏包隔离"
            );
        }
        for code in [5, 32, 13] {
            let error = anyhow::Error::new(std::io::Error::from_raw_os_error(code))
                .context("解码输出失败");
            assert!(
                !stops_extraction(&error),
                "非磁盘写满错误 {code} 不得被吞为任务级空间停止"
            );
        }
    }

// 这是发布 I/O 故障的确定性队列回归，不是物理磁盘写满或子进程 WriteFile 注入。
#[test]
    #[ignore = "Requires explicitly provided real 7-Zip engine"]
    #[rustfmt::skip]
    fn review_f11_disk_full_publish_stops_real_queue() {
        review_f11_real_queue_publish_failure(112, true);
    }

#[test]
    #[ignore = "Requires explicitly provided real 7-Zip engine"]
    #[rustfmt::skip]
    fn review_f11_handle_disk_full_publish_stops_real_queue() {
        review_f11_real_queue_publish_failure(39, true);
    }

#[test]
    #[ignore = "Requires explicitly provided real 7-Zip engine"]
    #[rustfmt::skip]
    fn review_f11_access_denied_publish_continues_real_queue() {
        review_f11_real_queue_publish_failure(5, false);
    }
