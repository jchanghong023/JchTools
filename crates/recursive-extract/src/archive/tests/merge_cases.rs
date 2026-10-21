use super::*;

// 覆盖 X-03（成员就地解到包所在位置，含缺失父目录）
#[test]
    #[rustfmt::skip]
    fn merge_creates_missing_parent_directories() {
        // 回归：合入成员时把「父目录」传给只创建父级的 ensure_parent，实际只创建到祖父目录，
        // 带子目录的成员改名必然报「系统找不到指定的路径」，于是含子目录的整包（RAR 的子目录成员、
        // 分卷包里的 vols/ 目录）解压失败并留下半截空目录。合入必须创建到成员的父目录。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("file1.txt");
        fs::write(&source, b"member payload").unwrap();
        let target = root.join("archives/sub/dir1/file1.txt");

        let mut parents = DestinationParents::default();
        let outcome = merge_extracted(&root, &source, &target, &mut parents).unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Placed(_)),
            "目标空闲时成员应落到目标路径"
        );
        assert_eq!(fs::read(&target).unwrap(), b"member payload");
        assert!(!source.exists(), "合入后暂存文件应已改名离开");
    }

// 覆盖 X-04/X-10/H-07：重复卷族各自完整改主体，不把新尾卷拼入旧的残缺族。
#[test]
    #[rustfmt::skip]
    fn decoded_duplicate_volume_families_preserve_partial_existing_set() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let content = temp.path().join("content");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&content).unwrap();
        fs::write(root.join("bundle.zip.001"), b"old").unwrap();
        for (name, payload) in [
            ("bundle.zip.001", b"oneA"),
            ("bundle.zip.002", b"twoA"),
            ("bundle.zip_1.001", b"oneB"),
            ("bundle.zip_1.002", b"twoB"),
        ] {
            fs::write(content.join(name), payload).unwrap();
        }
        let mut names = DecoderNames::default();
        for name in [
            "bundle.zip.001",
            "bundle.zip.002",
            "bundle.zip.001",
            "bundle.zip.002",
        ] {
            names.record(name).unwrap();
        }
        let job = review_job(&root, &temp.path().join("task.sqlite3"), 0);
        let targets = plan_volume_renames(
            &job,
            &content,
            &root,
            &HashMap::new(),
            &names.renamed,
            &rules::build_exclusions("").unwrap(),
            &mut GitBoundaries::default(),
        )
        .unwrap();
        let mut parents = DestinationParents::default();
        for entry in staged_files(&content, &names.renamed, &job.context.control) {
            let (entry, relative) = entry.unwrap();
            merge_extracted(&root, entry.path(), targets.get(&relative).unwrap(), &mut parents)
                .unwrap();
        }
        assert_eq!(fs::read(root.join("bundle.zip.001")).unwrap(), b"old");
        assert!(!root.join("bundle.zip.002").exists());
        for (name, payload) in [
            ("bundle (1).zip.001", b"oneA"),
            ("bundle (1).zip.002", b"twoA"),
            ("bundle (2).zip.001", b"oneB"),
            ("bundle (2).zip.002", b"twoB"),
        ] {
            assert_eq!(fs::read(root.join(name)).unwrap(), payload);
        }
    }

// 覆盖 X-04, H-07（回归：目标已有同内容文件时仍必须落盘改名副本，不得以内容相同跳过）
#[test]
    #[rustfmt::skip]
    fn merge_conflict_with_identical_bytes_lands_a_renamed_copy() {
        // 合同 X-04/H-07：冲突即使内容完全相同，也不得跳过新文件落盘、不按内容淘汰；
        // 既有文件不动，新文件改用未占用的名字。整改前这里走「等字节即视为已落位」的
        // 快捷路径：成员不落盘、extracted 虚计一次，用户拿不到本次解出的那份副本。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let target = root.join("a.txt");
        fs::write(&target, b"same bytes").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("a.txt");
        fs::write(&source, b"same bytes").unwrap();
        let mut parents = DestinationParents::default();
        let outcome = merge_extracted(&root, &source, &target, &mut parents).unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Renamed(_)),
            "目标已存在时必须改名落盘而不是按内容跳过"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"same bytes",
            "既有文件必须原样保留（H-07）"
        );
        assert_eq!(
            fs::read(root.join("a (1).txt")).unwrap(),
            b"same bytes",
            "等内容的成员仍必须落盘为改名副本（X-04/H-07）"
        );
        assert!(!source.exists(), "成员已改名离开暂存区");
    }

// 覆盖 H-07, X-04（回归：冲突改名只改文件名主体，复合扩展名整体保留）
#[test]
    #[rustfmt::skip]
    fn merge_conflict_preserves_compound_extension() {
        // 合同 H-07 的例子：资料.tar.gz → 资料 (1).tar.gz；不得变成 资料.tar (1).gz，
        // 也不得通过更换/追加扩展名规避冲突。整改前该路径会按冲突策略删除既有文件
        // （或按策略丢弃新成员），两个断言都不成立。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let target = root.join("资料.tar.gz");
        fs::write(&target, b"old package").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("资料.tar.gz");
        fs::write(&source, b"brand new package content").unwrap();
        let mut parents = DestinationParents::default();
        let outcome = merge_extracted(&root, &source, &target, &mut parents).unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Renamed(_)),
            "目标已存在时必须改名落盘（H-07）"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"old package",
            "既有文件不得被覆盖或删除（H-07/S-01）"
        );
        assert_eq!(
            fs::read(root.join("资料 (1).tar.gz")).unwrap(),
            b"brand new package content",
            "冲突改名必须保留复合扩展名（H-07）"
        );
        assert!(
            !root.join("资料.tar (1).gz").exists(),
            "不得把复合扩展名拆开改名（H-07）"
        );
    }

#[test]
    #[rustfmt::skip]
    fn enqueue_replaces_pending_row_for_same_rel() {
        // 回归：archives 表唯一键在 fingerprint 上；同一路径的压缩包被另一个包的成员
        // 处理并按规则删除该包后，新行 snapshot 必然失败，把「解压失败 N 包」与错误
        // 计数虚高。入队时应先清同 rel 的未处理旧行。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let db_dir = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        let pack = root.join("pack.zip");
        fs::write(&pack, b"first version bytes").unwrap();

        let job = Job {
            root: root.clone(),
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&db_dir).unwrap(),
            summary: jchtools_file_core::model::Summary::default(),
        };
        enqueue_archive(&job, &pack, 0).unwrap();
        // 同路径包内容被覆盖（大小与内容都变了）后再次入队。
        fs::write(&pack, b"second version with different length").unwrap();
        enqueue_archive(&job, &pack, 0).unwrap();
        let rows: i64 = job
            .db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM archives WHERE rel='pack.zip'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "同一路径只应保留最新 fingerprint 的待处理行");
    }

// 平台门禁原因：验证对象是 Windows 路径长度语义（LongPathsEnabled=0 时 >260 普通路径的
// 裸 Win32 调用直接失败）与 \\?\ verbatim 前缀拼接，只能在 Windows 上构造。
// 覆盖 X-08, S-04（隐藏属性剥离，避免成员成为扫描不可见的影子文件）
#[cfg(windows)]
    #[test]
    #[rustfmt::skip]
    fn normalize_strips_hidden_on_plain_long_path() {
        // 直测锚点：集成测试经 prepare_at 的 root 已 canonicalize（verbatim），裸调用恰好总是
        // 成功，锁不住「普通路径 + 超长」这一原始缺陷形态；这里显式传非 verbatim 的 >260 路径，
        // LongPathsEnabled=0 的机器上修复前（裸调用 + 吞错记成功）必失败。
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
        let temp = tempfile::tempdir().unwrap();
        let mut dir = temp.path().to_path_buf();
        let seg = "n".repeat(40);
        for i in 0..7 {
            dir.push(format!("{seg}{i}"));
        } // 相对段 ≈300 字符，基路径短但总长 >260
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("deep-hidden.txt");
        fs::write(&file, b"payload").unwrap();
        // 植入隐藏属性：这条路径本身 >260，植入同样要走 \\?\（裸调用会以同样方式失败）。
        let mut wide: Vec<u16> = r"\\".encode_utf16().chain(r"?\".encode_utf16()).collect();
        wide.extend(file.as_os_str().encode_wide());
        wide.push(0);
        assert_ne!(
            // SAFETY: wide 是以 NUL 结尾的 UTF-16 verbatim 路径；调用只读取该缓冲区。
            unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) },
            0,
            "测试前置：植入隐藏属性失败"
        );
        let config = Config {
            include_hidden: false,
            ..Config::default()
        };
        assert!(
            normalize_new_member_attributes(&file, &config),
            "普通超长路径的属性剥离必须成功"
        );
        let attrs = fs::symlink_metadata(&file).unwrap().file_attributes();
        assert_eq!(attrs & FILE_ATTRIBUTE_HIDDEN, 0, "隐藏属性必须被剥离");
    }

// 覆盖 X-06, X-08（回归：空间不足是任务级中止，不得再走隔离；取消与包级失败口径不变）
#[test]
    #[rustfmt::skip]
    fn failure_classification_separates_stop_from_quarantine() {
        // 空间不足必须被识别为「停止整个解压任务」：即使被 with_context 包装（解压
        // 回调的实际形态）也要命中——判定走错误链，而不是顶层 downcast；否则空间不足
        // 又会退回「逐包隔离并继续处理后续包」。
        let space: anyhow::Error = StopExtraction("可用空间不足：本包需 1 GiB".into()).into();
        assert!(stops_extraction(&space), "空间不足必须停止整个任务（X-08）");
        let wrapped = Err::<(), _>(StopExtraction("磁盘剩余空间低于预留阈值".into()))
            .context("解压失败（可能已损坏、加密或格式不受支持）")
            .unwrap_err();
        assert!(
            stops_extraction(&wrapped),
            "被 with_context 包装后仍必须识别（解压回调的实际形态）"
        );
        // 防护上限属于包级失败：按 X-06 隔离，不得停止整个任务。
        let capped = anyhow::anyhow!("压缩包条目数量超过用户设置的上限");
        assert!(!stops_extraction(&capped), "防护上限走包级隔离口径（X-06）");
        // 用户取消：既不是空间不足，也不隔离——沿用 control 的取消口径。
        let control = Control::default();
        control.cancel();
        let cancelled = control.check_cancelled().unwrap_err();
        assert!(!stops_extraction(&cancelled), "取消不得被当成空间不足");
    }

// 覆盖 X-05, X-08（取消在删除的安全边界生效）：已取消时不启动任何删除，
// 错误沿用取消口径（不是「停止任务」信号，不隔离、不虚计成功）。
#[test]
    #[rustfmt::skip]
    fn delete_successful_source_stops_on_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let volume = root.join("pack.zip");
        fs::write(&volume, b"archive bytes").unwrap();
        let mut job = Job {
            root,
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&temp.path().join("state")).unwrap(),
            summary: jchtools_file_core::model::Summary::default(),
        };
        job.context.control.cancel();

        let error = delete_successful_source(&mut job, "pack.zip", std::slice::from_ref(&volume))
            .unwrap_err();
        assert!(
            !stops_extraction(&error),
            "取消沿用取消口径，不得被当成任务级中止信号处理：{error:#}"
        );
        assert!(volume.is_file(), "取消后不得启动任何删除");
        assert_eq!(job.summary.deleted, 0, "取消后不得虚计删除");
    }
