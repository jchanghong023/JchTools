    use super::{
        case_fold_key, compare_paths, compare_paths_insensitive, media_dir_name, occupancy_key,
        parse_formats, run_formats_probe, scan, selected_xberg_extension, write_new_markdown,
        FormatGroup, OccupiedIndex, Options,
    };
    use crate::markdown_document::{Deadline, MediaFile};
    use jchtools_file_core::platform::compare_names_ordinal_ignore_case as compare_names;
    use std::thread;
    use std::time::Instant;
    use std::{
        cmp::Ordering,
        collections::BTreeSet,
        ffi::{OsStr, OsString},
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
        time::Duration,
    };

    fn supported() -> BTreeSet<String> {
        ["pdf", "docx"].into_iter().map(str::to_string).collect()
    }

    // 覆盖 T-23/T-24：仅明确取消终态不计失败，能力包装不能吞掉取消接口异常。
    #[test]
    fn cancellation_classification_preserves_actual_errors() {
        let capability_prefix = "Xberg 共享接口不可用：能力握手失败（不会启动备用引擎）：";
        for message in [
            "请求已取消",
            "用户已取消初始化",
            "Xberg cancelled：fixture stopped",
        ] {
            assert!(super::confirmed_cancellation(message));
            assert!(super::confirmed_cancellation(&format!(
                "{capability_prefix}{message}"
            )));
        }
        for message in [
            "请求已取消但取消接口未在 1 秒内确认；代理仍保留未结束任务，阻止同场景重入",
            "请求已取消且取消已确认，原请求未在 1 秒内结束；代理仍保留未结束任务，阻止同场景重入",
            "取消接口失败：连接失败",
            "Xberg timeout：fixture stopped",
            "同步 Markdown 失败：磁盘写入错误",
        ] {
            assert!(!super::confirmed_cancellation(message));
            assert!(!super::confirmed_cancellation(&format!(
                "{capability_prefix}{message}"
            )));
        }
    }

    // 覆盖 T-07、T-08：仅 MP4/M4A 进入媒体链路，Xberg 清单中的其他媒体格式不得误入文档转换。
    #[test]
    fn other_xberg_media_formats_are_not_selected() {
        let groups = [FormatGroup::Other, FormatGroup::Media];
        assert!(!selected_xberg_extension("wmv", "video/x-ms-wmv", &groups));
        assert!(!selected_xberg_extension("mp3", "audio/mpeg", &groups));
        assert!(selected_xberg_extension("mp4", "video/mp4", &groups));
        assert!(selected_xberg_extension("m4a", "audio/mp4", &groups));
    }

    fn options(input_dir: PathBuf, output_dir: PathBuf, flat: bool) -> Options {
        Options {
            input_dir,
            output_dir,
            flat,
            groups: vec![FormatGroup::Pdf, FormatGroup::Office],
            timeout_secs: 21_600,
        }
    }

    // 覆盖 T-09、T-10、T-11：只读纳入 Git 树，输出子树排除，保留层级。
    #[test]
    fn scan_includes_git_project_but_excludes_output_subtree() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = input.join("results");
        fs::create_dir_all(input.join("repo/.git")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(input.join("repo/a.PDF"), b"pdf").unwrap();
        fs::write(output.join("generated.pdf"), b"pdf").unwrap();

        let plan = scan(&options(input, output.clone(), false), &supported()).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].relative, PathBuf::from("repo/a.PDF"));
        assert_eq!(
            plan.items[0].target,
            fs::canonicalize(&output).unwrap().join("repo/a_pdf.md")
        );
    }

    // 覆盖 T-11：平铺同名按稳定路径排序只保留第一份。
    #[test]
    fn flat_collision_keeps_sorted_first_and_counts_skip() {
        assert_eq!(
            compare_paths(
                PathBuf::from("A/Z/x.PDF").as_path(),
                PathBuf::from("a/a/x.pdf").as_path(),
            ),
            Ordering::Greater
        );
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(input.join("A")).unwrap();
        fs::create_dir_all(input.join("b")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(input.join("A/x.PDF"), b"first").unwrap();
        fs::write(input.join("b/x.pdf"), b"second").unwrap();

        let plan = scan(&options(input, output.clone(), true), &supported()).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].relative, PathBuf::from("A/x.PDF"));
        assert_eq!(
            plan.items[0].target,
            fs::canonicalize(&output).unwrap().join("x_pdf.md")
        );
        assert_eq!(plan.summary.skipped_duplicate, 1);
    }

    // 覆盖 T-11（A'-低危2）：平铺决胜段（逐段大小写不敏感全等价、同分段结构）
    // 里「逐段原始 UTF-16（首段分出胜负即停）」与合同「原始路径 UTF-16 单元
    // 序列」必须选出同序——等价性依据：决胜段内各段两两大小写不敏感相等，而
    // 大小写不敏感相等的两段不可能互为真前缀（逐单元折叠不消灭单元、段内无
    // 分隔符），故两种口径的第一差异单元必落在同一段内同一位置。本用例跨多段、
    // 各段大小写形态不同（A\b\x.pdf vs a\B\x.pdf），固定该性质防回归。
    #[test]
    fn flat_tiebreak_per_component_matches_whole_path_sequence() {
        for (left, right) in [
            ("A/b/x.PDF", "a/B/x.pdf"),
            ("a/A/x.pdf", "a/a/x.pdf"),
            ("Aa/b/x.pdf", "aA/B/x.pdf"),
            ("deep/A/b/x.PDF", "deep/a/B/x.pdf"),
        ] {
            let left = Path::new(left);
            let right = Path::new(right);
            assert_eq!(
                compare_paths_insensitive(left, right),
                Ordering::Equal,
                "用例必须处于决胜段（逐段大小写不敏感等价）：{left:?} vs {right:?}"
            );
            assert_eq!(
                compare_paths(left, right),
                whole_path_original_order(left, right),
                "逐段决胜必须与合同整路径序列决胜一致：{left:?} vs {right:?}"
            );
        }
        assert_eq!(
            compare_paths(Path::new("A/b/x.PDF"), Path::new("a/B/x.pdf")),
            Ordering::Less,
            "首段大写 A（U+0041）原始序数小于小写 a（U+0061），平铺跳过时保留前者"
        );
    }

    /// 合同 T-11 的决胜口径参照实现：整条相对路径的原始 UTF-16 单元序列比较。
    fn whole_path_original_order(left: &Path, right: &Path) -> Ordering {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            left.as_os_str()
                .encode_wide()
                .cmp(right.as_os_str().encode_wide())
        }
        #[cfg(not(windows))]
        {
            // 非 Windows 仅作参照：ASCII 用例下按 Unicode 标量比较与 UTF-16
            // 单元序列同序。
            left.as_os_str()
                .to_string_lossy()
                .cmp(&right.as_os_str().to_string_lossy())
        }
    }

    // 覆盖 T-11、T-12、T-24（A'-1 回归，键语义单元级）：层级模式的占用键 =
    // 「相对父目录+结果名」且大小写不敏感比较——同父目录内大小写变体结果名
    // （对应输入 a.PDF 与 a.pdf 在大小写敏感目录共存时的等价目标）必须命中
    // 跳过，父目录大小写变体同样等价；不同父目录不得挤占（T-10）；平铺键
    // 不含父目录（既有平铺决胜序不变）。
    // 物理场景说明：默认 NTFS 大小写不敏感，同目录两个大小写变体文件无法
    // 共存（第二次写入覆盖同一文件；fsutil 启用目录级大小写敏感需要管理员
    // 权限、CreateFileW(FILE_FLAG_POSIX_SEMANTICS) 也无法绕过），故同键跳过
    // 以本单元断言 + 不同目录集成护栏（hierarchical_duplicate_in_different_
    // directories_both_convert）共同锁定；scan 接线由两者与既有平铺测试约束。
    #[test]
    fn hierarchical_occupancy_key_collides_only_within_same_parent() {
        let mut occupied = OccupiedIndex::default();
        occupied.insert(occupancy_key(
            false,
            Path::new("docs"),
            OsStr::new("a_pdf.md"),
        ));
        assert!(
            occupied.contains(
                occupancy_key(false, Path::new("docs"), OsStr::new("A_PDF.MD")).as_os_str()
            ),
            "同父目录的结果名大小写变体必须命中占用（A'-1：第二个计同名跳过而非提交失败）"
        );
        assert!(
            occupied.contains(
                occupancy_key(false, Path::new("Docs"), OsStr::new("a_pdf.md")).as_os_str()
            ),
            "父目录大小写变体属于同一输出父目录，同样必须命中"
        );
        assert!(
            !occupied.contains(
                occupancy_key(false, Path::new("other"), OsStr::new("a_pdf.md")).as_os_str()
            ),
            "不同父目录不得挤占（T-10 层级保留）"
        );
        assert!(
            !occupied.contains(
                occupancy_key(true, Path::new("docs"), OsStr::new("A_PDF.MD")).as_os_str()
            ),
            "平铺键不含父目录，不得被层级键误命中（平铺决胜序不受影响）"
        );
    }

    // 覆盖 T-10、T-11（A'-1 伴随护栏）：层级模式的占用登记只约束同一输出父
    // 目录；不同子目录的同名文件各自独立转换，不得因全局重名被误跳过。
    #[test]
    fn hierarchical_duplicate_in_different_directories_both_convert() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(input.join("docs")).unwrap();
        fs::create_dir_all(input.join("notes")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(input.join("docs/a.PDF"), b"one").unwrap();
        fs::write(input.join("notes/a.pdf"), b"two").unwrap();

        let plan = scan(&options(input, output, false), &supported()).unwrap();
        assert_eq!(plan.items.len(), 2, "不同子目录的同名文件互不挤占");
        assert_eq!(plan.summary.skipped_duplicate, 0);
    }

    // 覆盖 T-07、T-09（输出目录是输入目录的祖先时：输入子树不被前缀判定排除，
    // 递归处理全部匹配文件——只有输出位于输入子树内时才排除输出子树）
    #[test]
    fn scan_keeps_input_subtree_when_output_is_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("data").join("reports");
        let output = temp.path().join("data");
        fs::create_dir_all(input.join("sub")).unwrap();
        fs::write(input.join("top.PDF"), b"pdf").unwrap();
        fs::write(input.join("sub/inner.pdf"), b"pdf").unwrap();

        let plan = scan(&options(input, output.clone(), false), &supported()).unwrap();
        assert_eq!(
            plan.items.len(),
            2,
            "输出为输入祖先时子树全部纳入递归处理：{:?}",
            plan.items
        );
        assert_eq!(plan.summary.skipped_existing, 0);
        assert_eq!(plan.summary.skipped_duplicate, 0);
    }

    // 覆盖 T-09：输入输出重合时扫描前拒绝。
    #[test]
    fn same_input_and_output_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().to_path_buf();
        let error = scan(&options(directory.clone(), directory, false), &supported()).unwrap_err();
        assert!(error.contains("不能相同"));
    }

    // 覆盖 T-12、T-25：提交时已有结果不得被覆盖，临时文件须清除。
    #[test]
    fn atomic_commit_refuses_existing_result() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("a_pdf.md");
        fs::write(&target, b"old").unwrap();
        let error = write_new_markdown(temp.path(), &target, "new", &[], &unbounded_deadline())
            .unwrap_err();
        assert!(error.contains("已有结果不会覆盖"));
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    // 覆盖 T-14/T-25：媒体文件先落盘、md 最后提交；引用的图片与 md 同批可见。
    #[test]
    fn media_files_written_before_markdown_commit() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("a_docx.md");
        let media = vec![MediaFile {
            relative: "a_docx_media/image_0.png".to_string(),
            bytes: b"img".to_vec(),
        }];
        write_new_markdown(temp.path(), &target, "body", &media, &unbounded_deadline()).unwrap();
        assert_eq!(
            fs::read(temp.path().join("a_docx_media").join("image_0.png")).unwrap(),
            b"img"
        );
        assert_eq!(fs::read(&target).unwrap(), b"body");
    }

    // 覆盖 T-25：媒体写入失败时本次已写图片回滚、md 不提交，不留半成品。
    #[test]
    fn media_write_failure_rolls_back_and_fails_file() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("a_docx.md");
        fs::write(temp.path().join("a_docx_media"), b"occupied").unwrap();
        let media = vec![MediaFile {
            relative: "a_docx_media/image_0.png".to_string(),
            bytes: b"img".to_vec(),
        }];
        let error = write_new_markdown(temp.path(), &target, "body", &media, &unbounded_deadline())
            .unwrap_err();
        assert!(
            error.contains("不是目录") || error.contains("已存在"),
            "{error}"
        );
        assert!(!target.exists(), "md 不得提交");
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            1,
            "既有占用项不得被删除"
        );
        assert_eq!(
            fs::read(temp.path().join("a_docx_media")).unwrap(),
            b"occupied"
        );
    }

    // 覆盖 T-14：媒体目录名折叠空白、避开与既有普通文件的占用（追加序号）。
    #[test]
    fn media_dir_name_sanitizes_and_avoids_file_occupancy() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("my report_docx.md");
        assert_eq!(
            media_dir_name(temp.path(), &target).unwrap(),
            "my_report_docx_media",
            "空白折叠为下划线"
        );
        fs::write(temp.path().join("my_report_docx_media"), "占用".as_bytes()).unwrap();
        assert_eq!(
            media_dir_name(temp.path(), &target).unwrap(),
            "my_report_docx_2_media",
            "与普通文件同名时追加序号让路"
        );
    }

    // ── S4-02：单文件超时必须约束落盘，慢盘不得把超时文件计成功 ──

    /// 已耗尽的预算（Duration::ZERO 即刻到期；remaining 恒为零）。
    fn expired_deadline() -> Deadline {
        Deadline::new(Duration::ZERO)
    }

    /// 无界预算（checked_add 溢出退化为不限时），供不受超时约束的既有用例使用。
    fn unbounded_deadline() -> Deadline {
        Deadline::new(Duration::MAX)
    }

    // 覆盖 T-29（S4-02 回归）：预算耗尽后不得再写图片——图片写入前检查点命中，
    // 失败带回滚清理本次输出，不留半成品、不提交 md。
    #[test]
    fn expired_deadline_blocks_media_write_and_rolls_back() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("a_docx.md");
        let media = vec![MediaFile {
            relative: "a_docx_media/image_0.png".to_string(),
            bytes: b"img".to_vec(),
        }];
        let error = write_new_markdown(temp.path(), &target, "body", &media, &expired_deadline())
            .unwrap_err();
        assert!(error.contains("超时"), "应按超时报错：{error}");
        assert!(!target.exists(), "md 不得提交");
        assert!(
            !temp.path().join("a_docx_media").exists(),
            "媒体目录不得残留"
        );
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            0,
            "不留半成品（T-29/T-25）"
        );
    }

    // 覆盖 T-29（S4-02 回归）：无图片时最终提交前同样受预算约束——超时不得
    // 走完 temp 写入与改名提交，失败后目录内无残留临时文件。
    #[test]
    fn expired_deadline_blocks_final_commit_without_temp_leftover() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("a_docx.md");
        let error =
            write_new_markdown(temp.path(), &target, "body", &[], &expired_deadline()).unwrap_err();
        assert!(
            error.contains("超时") && error.contains("提交"),
            "提交前检查点应按超时报错：{error}"
        );
        assert!(!target.exists(), "md 不得提交");
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            0,
            "不得残留 .jch-markdown-*.tmp 临时文件"
        );
    }

    // 覆盖 T-29（S4-02 护栏）：预算充足时落盘行为不变（图片 + md 正常提交）。
    #[test]
    fn unbounded_deadline_keeps_normal_commit() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("a_docx.md");
        let media = vec![MediaFile {
            relative: "a_docx_media/image_0.png".to_string(),
            bytes: b"img".to_vec(),
        }];
        write_new_markdown(temp.path(), &target, "body", &media, &unbounded_deadline()).unwrap();
        assert_eq!(
            fs::read(temp.path().join("a_docx_media").join("image_0.png")).unwrap(),
            b"img"
        );
        assert_eq!(fs::read(&target).unwrap(), b"body");
    }

    // ── S4-03：扫描期单文件失败不得中止整批（T-24）──

    /// 在 link 处创建目录链接：Windows 用 junction（mklink /J，无需管理员），
    /// 其他平台用符号链接；`fsutil::is_link` 两者都识别。
    #[cfg(windows)]
    fn make_dir_link(link: &Path, target: &Path) -> std::io::Result<()> {
        let output = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map_err(|error| {
                std::io::Error::other(format!("无法启动 cmd 创建 junction：{error}"))
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "mklink /J 失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    #[cfg(not(windows))]
    fn make_dir_link(link: &Path, target: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    // 覆盖 T-09/T-12/T-24、S-04：已有层级输出目录为 junction 时，必须先拒绝
    // 该文件，不能穿过链接读取旧结果并把它当作正常的已有结果跳过。
    #[test]
    fn output_parent_link_fails_file_before_existing_result_check() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        let outside = temp.path().join("outside");
        fs::create_dir_all(input.join("chapter")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(input.join("chapter/a.pdf"), b"input").unwrap();
        fs::write(input.join("b.pdf"), b"other input").unwrap();
        fs::write(outside.join("a_pdf.md"), b"old result").unwrap();
        make_dir_link(&output.join("chapter"), &outside).expect("创建目录链接");

        let plan = scan(&options(input, output, false), &supported()).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].relative, PathBuf::from("b.pdf"));
        assert_eq!(plan.summary.failed, 1);
        assert_eq!(plan.summary.skipped_existing, 0);
        assert_eq!(plan.scan_failures.len(), 1);
        assert_eq!(plan.scan_failures[0].0, PathBuf::from("chapter/a.pdf"));
        assert!(plan.scan_failures[0].1.contains("junction"));
        assert_eq!(fs::read(outside.join("a_pdf.md")).unwrap(), b"old result");
    }

    // 覆盖 T-24（S4-03 回归）：单个文件的媒体目录名被链接占用时只失败该文件
    // ——批次继续、失败计入统计、失败文件与原因可呈现；不得把整批扫描变成 Err。
    #[test]
    fn media_dir_reservation_failure_fails_file_not_batch() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(input.join("a.docx"), b"one").unwrap();
        fs::write(input.join("b.docx"), b"two").unwrap();
        make_dir_link(&output.join("b_docx_media"), temp.path()).expect("创建目录链接");

        let plan = scan(&options(input, output, false), &supported()).unwrap();
        assert_eq!(
            plan.items.len(),
            1,
            "批次必须继续：其余文件保留在计划中（T-24）"
        );
        assert_eq!(plan.items[0].relative, PathBuf::from("a.docx"));
        assert_eq!(plan.summary.failed, 1, "单文件失败计入统计");
        assert_eq!(plan.scan_failures.len(), 1);
        assert_eq!(plan.scan_failures[0].0, PathBuf::from("b.docx"));
        assert!(
            plan.scan_failures[0].1.contains("媒体目录"),
            "失败原因须可呈现并指明阶段：{}",
            plan.scan_failures[0].1
        );
    }

    // ── F21：格式清单探测必须有界、可取消，输出截断不得当完整清单 ──

    #[cfg(windows)]
    fn hanging_command() -> Command {
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"]);
        command
    }

    #[cfg(not(windows))]
    fn hanging_command() -> Command {
        let mut command = Command::new("sleep");
        command.arg("60");
        command
    }

    #[cfg(windows)]
    fn spewing_command(mib: usize) -> Command {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-Command",
            &format!("[Console]::Out.Write(('x' * {}))", mib * 1024 * 1024),
        ]);
        command
    }

    // 覆盖 T-22/T-24（F21）：探测进程挂起时必须受总超时约束，宿主在预算内恢复。
    #[test]
    fn formats_probe_times_out_on_hanging_process() {
        let mut command = hanging_command();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let result = run_formats_probe(&mut command, Duration::from_millis(400), &cancel);
        assert!(result.is_err(), "挂起进程必须在探测超时后失败");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "探测超时后应立即返回，实际 {:?}",
            started.elapsed()
        );
    }

    // 覆盖 T-23（F21）：停止请求必须立刻中断格式探测，不等满超时。
    #[test]
    fn formats_probe_stops_promptly_on_cancel() {
        use std::sync::Arc;
        let mut command = hanging_command();
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            setter.store(true, AtomicOrdering::Relaxed);
        });
        let started = Instant::now();
        let result = run_formats_probe(&mut command, Duration::from_secs(60), &cancel);
        assert!(result.is_err(), "取消后探测应失败");
        assert!(cancel.load(AtomicOrdering::Relaxed));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "取消后应立即返回，实际 {:?}",
            started.elapsed()
        );
    }

    // 覆盖 T-24（F21）：探测输出超过捕获上限时报截断错误，不得当完整清单解析。
    #[cfg(windows)]
    #[test]
    fn formats_probe_reports_truncated_output() {
        let mut command = spewing_command(9);
        let cancel = AtomicBool::new(false);
        let result = run_formats_probe(&mut command, Duration::from_secs(60), &cancel);
        let error = result.expect_err("截断的输出不得当成功");
        assert!(
            error.contains("截断") || error.contains("不完整"),
            "应说明输出不完整：{error}"
        );
    }

    // 覆盖 T-07/T-08（F21 守护）：探测正常返回时捕获完整输出。
    #[cfg(windows)]
    #[test]
    fn formats_probe_captures_valid_output() {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-Command",
            "[Console]::Out.Write('[{\"extension\":\".pdf\",\"mime_type\":\"application/pdf\"}]')",
        ]);
        let cancel = AtomicBool::new(false);
        let output = run_formats_probe(&mut command, Duration::from_secs(60), &cancel)
            .expect("正常探测应成功");
        assert!(String::from_utf8_lossy(&output.stdout).contains("pdf"));
    }

    // 覆盖 T-07/T-08：清单行按分组与媒体例外筛入支持集（纯函数回归）。
    #[test]
    fn parse_formats_filters_rows_by_group() {
        let stdout: &[u8] = concat!(
            r#"[{"extension":".pdf","mime_type":"application/pdf"},"#,
            r#"{"extension":".MP4","mime_type":"video/mp4"},"#,
            r#"{"extension":"wmv","mime_type":"video/x-ms-wmv"},"#,
            r#"{"extension":"","mime_type":"application/pdf"},"#,
            r#"{"extension":"tar.gz","mime_type":"application/x-tar"},"#,
            r#"{"extension":".docx","mime_type":"application/vnd.wordprocessing"}]"#
        )
        .as_bytes();
        let groups = vec![FormatGroup::Pdf, FormatGroup::Office, FormatGroup::Media];
        let selected = parse_formats(stdout, &groups).expect("合法清单应解析");
        let expected: BTreeSet<String> = ["pdf", "mp4", "docx"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(selected, expected);
    }

    // ── F29：平铺占用名查询不随规模线性增长比较次数 ──

    // 覆盖 T-11（F29 性能修复，语义不变）：1 千/1 万互异目标的比较次数都近似为零，
    // 增长阶不再随占用名规模线性上升。
    #[test]
    fn occupied_index_compare_count_does_not_grow_with_size() {
        for size in [1_000usize, 10_000] {
            let mut index = OccupiedIndex::default();
            for i in 0..size {
                index.insert(OsString::from(format!("file{i}_pdf.md")));
            }
            let compares = {
                let mut compares = 0usize;
                let mut probe = |a: &OsStr, b: &OsStr| {
                    compares += 1;
                    compare_names(a, b) == Ordering::Equal
                };
                for i in 0..100 {
                    let name = OsString::from(format!("probe{i}_pdf.md"));
                    assert!(!index.contains_with(&name, &mut probe));
                }
                compares
            };
            assert_eq!(
                compares, 0,
                "互异目标应命中不同桶，规模 {size} 时不应发生逐一比较"
            );
        }
    }

    // 覆盖 T-11（S4-04 回归）：占用折叠键与 `CompareStringOrdinal` 忽略大小写
    // 口径必须一致。运行时直接询问操作系统比较器（不依赖记忆中的 Unicode 表）：
    // 带 ypogegrammeni 的希腊预组合小写（U+1F80 段、U+1FB3/1FC3/1FF3）被 OS 按
    // 简单（单单元）大写表折到对应大写变体——Rust `to_uppercase` 是全映射（多
    // 字符），`fold_unit` 若保持原样会把 OS 判等的对分进不同桶，占用检查漏比
    // （第二文件提交期失败而非占用跳过，违反 T-11 分类语义）。
    #[cfg(windows)]
    #[test]
    fn fold_key_agrees_with_compare_string_ordinal_on_greek_ypogegrammeni() {
        use std::os::windows::ffi::OsStringExt;
        fn wide_name(unit: u16) -> OsString {
            std::ffi::OsString::from_wide(&[unit])
        }
        // OS 判等（忽略大小写相等）的样本对：折叠键必须同桶，否则漏比。
        // 全部经本机 CompareStringOrdinal 运行时核实（S4-04）。
        let equal_pairs: &[(u16, u16)] = &[
            (0x1F80, 0x1F88),
            (0x1F81, 0x1F89),
            (0x1F82, 0x1F8A),
            (0x1F84, 0x1F8C),
            (0x1F86, 0x1F8E),
            (0x1F90, 0x1F98),
            (0x1F91, 0x1F99),
            (0x1F95, 0x1F9D),
            (0x1FA0, 0x1FA8),
            (0x1FA2, 0x1FAA),
            (0x1FA5, 0x1FAD),
            (0x1FB3, 0x1FBC),
            (0x1FC3, 0x1FCC),
            (0x1FF3, 0x1FFC),
        ];
        for (lower, upper) in equal_pairs {
            let left = wide_name(*lower);
            let right = wide_name(*upper);
            assert_eq!(
                compare_names(left.as_os_str(), right.as_os_str()),
                Ordering::Equal,
                "前置：CompareStringOrdinal 应把 {lower:#06x} 与 {upper:#06x} 判为忽略大小写相等"
            );
            assert_eq!(
                case_fold_key(left.as_os_str()),
                case_fold_key(right.as_os_str()),
                "OS 判等的对必须落入同一折叠桶（S4-04：不得漏比）"
            );
        }
        // OS 不判等的对照对：折叠不得把不同字母误并桶（多比无害，误并会造成
        // 假「已占用」跳过）。两组运行时核实结论（S4-04 探针）：①OS 折叠在
        // ypogegrammeni 系列内部按简单大写 +8（见 equal_pairs），②带额外声调
        // 的变体（1FB2/1FB4/1FB7 等，其 UnicodeData 简单大写指向无 ypogegrammeni
        // 的大写形如 1FBA）一律不判等——折叠表不得映射它们。
        let distinct_pairs: &[(u16, u16)] = &[
            (0x1F88, 0x1F08), // 标题形大写 ≠ 无附加符号的普通大写
            (0x1F80, 0x1F81), // 附加符号不同的两个小写
            (0x1FB2, 0x1FB3), // 变体声调不同（1FB2 的简单大写 1FBA 不参与 OS 折叠）
            (0x1FB2, 0x1FBA), // OS 不跨 ypogegrammeni 边界折叠到裸大写形
            (0x1FB4, 0x1FBB),
            (0x1FC2, 0x1FCA),
            (0x1FC4, 0x1FCB),
            (0x1FF2, 0x1FFA),
            (0x1FF4, 0x1FFB),
            (0x1FB7, 0x1FB6),
            (0x1FC7, 0x1FC6),
            (0x1FF7, 0x1FF6),
        ];
        for (left_unit, right_unit) in distinct_pairs {
            let left = wide_name(*left_unit);
            let right = wide_name(*right_unit);
            assert_ne!(
                compare_names(left.as_os_str(), right.as_os_str()),
                Ordering::Equal,
                "前置：CompareStringOrdinal 不应把 {left_unit:#06x} 与 {right_unit:#06x} 判等"
            );
            assert_ne!(
                case_fold_key(left.as_os_str()),
                case_fold_key(right.as_os_str()),
                "OS 不判等的对不得被折叠误并同桶"
            );
        }
    }

    // 覆盖 T-11（F29）：大小写等价变体落入同桶，恰好一次精确比较即判重。
    #[test]
    fn occupied_index_case_variant_hits_single_bucket() {
        let mut index = OccupiedIndex::default();
        index.insert(OsString::from("Report_pdf.md"));
        let compares = {
            let mut compares = 0usize;
            let mut probe = |a: &OsStr, b: &OsStr| {
                compares += 1;
                compare_names(a, b) == Ordering::Equal
            };
            let duplicate = OsString::from("REPORT_pdf.md");
            assert!(index.contains_with(&duplicate, &mut probe));
            let distinct = OsString::from("Reports_pdf.md");
            assert!(!index.contains_with(&distinct, &mut probe));
            compares
        };
        assert_eq!(compares, 1, "大小写变体应在同桶内一次精确确认");
    }
