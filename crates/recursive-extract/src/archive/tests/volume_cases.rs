use super::*;

#[test]
    #[rustfmt::skip]
    fn numbered_volume_growth_actual_paths_and_precheck_cross_999() {
        let temp = tempfile::tempdir().unwrap();
        for number in 1..=1001 {
            fs::write(
                temp.path().join(format!("pack.tar.gz.{number:03}")),
                b"volume",
            )
            .unwrap();
        }
        let archive = temp.path().join("pack.tar.gz.001");
        let named = volume_set(&archive).unwrap();
        assert_eq!(named.scheme, VolumeScheme::Numbered);
        assert_eq!(named.paths.len(), 1001);
        x10_volume_precheck("pack.tar.gz.001", &archive, &named).unwrap();
        let info = ArchiveVolumes {
            kind: "split".to_string(),
            count: 1001,
            new_rar_names: false,
        };
        let actual = named.scheme.actual_paths(&archive, &info).unwrap();
        assert_eq!(actual.len(), 1001);
        assert_eq!(actual[998], temp.path().join("pack.tar.gz.999"));
        assert_eq!(actual[999], temp.path().join("pack.tar.gz.1000"));
        assert_eq!(actual[1000], temp.path().join("pack.tar.gz.1001"));
    }

#[test]
    #[rustfmt::skip]
    fn numbered_volume_growth_exact_family_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let included = ["pack.7z.001", "pack.7z.999", "pack.7z.1000"];
        let excluded = [
            "pack.7z.0001",
            "pack.7z.01000",
            "pack.7z.99",
            "pack.7z.１０００",
            "pack.7z.1000.bak",
            "pack.7z",
        ];
        for name in included.into_iter().chain(excluded) {
            fs::write(temp.path().join(name), b"volume").unwrap();
        }
        for entry in included {
            let named = volume_set(&temp.path().join(entry)).unwrap();
            assert_eq!(named.scheme, VolumeScheme::Numbered);
            assert_eq!(named.paths.len(), included.len());
            for member in included {
                assert!(named.paths.contains(&temp.path().join(member)));
            }
            for member in excluded {
                assert!(!named.paths.contains(&temp.path().join(member)));
            }
        }
        for name in excluded {
            assert!(!numbered_entry(name), "非规范数字后缀不属于族：{name}");
        }
    }

#[test]
    #[rustfmt::skip]
    fn numbered_volume_growth_gap_and_missing_entry_quarantine_whole_family() {
        for (entry, members, expected_reason) in [
            (
                "pack.tar.gz.001",
                vec!["pack.tar.gz.001", "pack.tar.gz.999", "pack.tar.gz.1001"],
                "分卷不全",
            ),
            (
                "pack.tar.gz.1000",
                vec!["pack.tar.gz.999", "pack.tar.gz.1000"],
                "缺起始卷",
            ),
            (
                "pack.tar.gz.1000",
                vec!["pack.tar.gz.000", "pack.tar.gz.1000"],
                "非法起始卷",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("data");
            let container = root.join(QUARANTINE_DIR_NAME);
            fs::create_dir_all(&container).unwrap();
            for name in &members {
                fs::write(root.join(name), name.as_bytes()).unwrap();
            }
            let spectator = root.join("pack.tar.gz.01000");
            fs::write(&spectator, b"not a volume").unwrap();
            let occupied = container.join(entry);
            fs::write(&occupied, b"existing archive").unwrap();
            let archive = root.join(entry);
            let named = volume_set(&archive).unwrap();
            assert_eq!(named.paths.len(), members.len());
            let error = x10_volume_precheck(entry, &archive, &named).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected_reason),
                "应报告 {expected_reason}：{error:#}"
            );
            let mut job = review_job(&root, &temp.path().join("state"), 0);
            quarantine(&mut job, entry, &error.to_string()).unwrap();
            assert_eq!(job.summary.archives_quarantined, members.len() as u64);
            for name in members {
                let suffix = name.strip_prefix("pack").unwrap();
                assert!(!root.join(name).exists());
                assert_eq!(
                    fs::read(container.join(format!("pack (1){suffix}"))).unwrap(),
                    name.as_bytes()
                );
            }
            assert_eq!(fs::read(occupied).unwrap(), b"existing archive");
            assert_eq!(fs::read(spectator).unwrap(), b"not a volume");
        }
    }

#[test]
    #[rustfmt::skip]
    fn numbered_volume_growth_nested_rename_preserves_all_suffixes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let content = temp.path().join("content");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&content).unwrap();
        let members = ["pack.tar.gz.001", "pack.tar.gz.999", "pack.tar.gz.1000"];
        for name in members {
            fs::write(content.join(name), name.as_bytes()).unwrap();
        }
        fs::write(root.join("pack.tar.gz.1000"), b"old tail").unwrap();
        let job = review_job(&root, &temp.path().join("state"), 0);
        let targets = plan_volume_renames(
            &job,
            &content,
            &root,
            &HashMap::new(),
            &HashMap::new(),
            &rules::build_exclusions("").unwrap(),
            &mut GitBoundaries::default(),
        )
        .unwrap();
        assert_eq!(targets.len(), members.len());
        let mut parents = DestinationParents::default();
        for name in members {
            let suffix = name.strip_prefix("pack").unwrap();
            let target = targets.get(name).unwrap();
            assert_eq!(*target, root.join(format!("pack (1){suffix}")));
            merge_extracted(&root, &content.join(name), target, &mut parents).unwrap();
            assert_eq!(fs::read(target).unwrap(), name.as_bytes());
        }
        assert_eq!(fs::read(root.join("pack.tar.gz.1000")).unwrap(), b"old tail");
        assert!(!root.join("pack.tar.gz.001").exists());
        assert!(!root.join("pack.tar.gz.999").exists());
    }

#[test]
    #[rustfmt::skip]
    fn numbered_volume_growth_composite_stream_and_nested_candidates() {
        for suffix in [".tgz", ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"] {
            for digits in ["001", "999", "1000", "10000"] {
                let name = format!("pack{suffix}.{digits}");
                assert!(composite_stream(Path::new(&name)), "{name}");
                assert!(potential_archive_member(&name), "{name}");
            }
            for digits in ["0001", "01000", "99", "１０００", "1000.bak"] {
                let name = format!("pack{suffix}.{digits}");
                assert!(!composite_stream(Path::new(&name)), "{name}");
                assert!(!potential_archive_member(&name), "{name}");
            }
        }
    }

#[test]
    #[rustfmt::skip]
    fn numbered_volume_growth_nested_missing_entry_is_enqueued_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let mut members = BTreeMap::new();
        for name in ["pack.tar.gz.1000", "pack.tar.gz.1001", "pack.tar.gz.01000"] {
            let path = nested.join(name);
            fs::write(&path, b"volume").unwrap();
            members.insert(name.to_string(), path);
        }
        let job = review_job(&root, &temp.path().join("state"), 0);
        enqueue_nested_groups(&job, BTreeMap::from([(nested, members)]), 3).unwrap();
        let queued: (i64, String, u32) = job
            .db
            .conn
            .query_row(
                "SELECT COUNT(*),rel,depth FROM archives WHERE state='pending'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(queued, (1, "nested/pack.tar.gz.1000".to_string(), 3));
    }

// 覆盖 X-10（回归：数字尾卷族按最少三位补零、超过三位自然增长的编号识别，
// 复合扩展名（如 .tar.gz.001）的卷集同样成组，不再只认 .7z.001/.zip.001）
#[test]
    #[rustfmt::skip]
    fn volume_set_groups_compound_numbered_volumes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("资料.tar.gz.001"), b"vol1").unwrap();
        fs::write(root.join("资料.tar.gz.002"), b"vol2").unwrap();
        // 完整主包名自身不加入数字尾卷族（X-10），也不得被吸入卷集。
        fs::write(root.join("资料.tar.gz"), b"standalone").unwrap();
        let set = volume_set(&root.join("资料.tar.gz.001")).unwrap();
        assert_eq!(set.scheme, VolumeScheme::Numbered);
        assert_eq!(set.paths.len(), 2, "同主干 .001/.002 应识别为同一卷集");
    }

// 覆盖 X-10（回归：普通扩展名 + .NNN 同样按数字尾卷族识别，主干为去掉 .NNN）
#[test]
    #[rustfmt::skip]
    fn volume_set_recognizes_any_three_digit_numbered_entry() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("x.rar.001"), b"vol1").unwrap();
        fs::write(root.join("x.rar.002"), b"vol2").unwrap();
        let set = volume_set(&root.join("x.rar.001")).unwrap();
        assert_eq!(set.scheme, VolumeScheme::Numbered);
        assert_eq!(set.paths.len(), 2);
    }

// 覆盖 X-10（回归：.000 即使与其他分卷同组也必须在引擎解析前按非法起始卷拒绝）。
#[test]
    #[rustfmt::skip]
    fn numbered_volume_zero_is_rejected_by_precheck() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let zero = root.join("x.7z.000");
        fs::write(&zero, b"invalid start").unwrap();
        fs::write(root.join("x.7z.002"), b"later volume").unwrap();

        let named = volume_set(&zero).unwrap();
        assert!(
            x10_volume_precheck("x.7z.000", &zero, &named).is_err(),
            ".000 是可识别的非法起始卷，必须在引擎解析前走整组失败隔离（X-10）"
        );

        let second = root.join("x.7z.002");
        let named = volume_set(&second).unwrap();
        assert!(
            x10_volume_precheck("x.7z.002", &second, &named).is_err(),
            "从后续卷检查时也必须识别同组的非法 .000 起始卷（X-10）"
        );
    }

// 覆盖 X-10/E-05（同名目录不是老式分卷族的主包文件，缺引擎时也须在预检拒绝）。
#[test]
    #[rustfmt::skip]
    fn old_style_main_directory_does_not_satisfy_precheck() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let main = root.join("x.rar");
        fs::create_dir(&main).unwrap();
        let tail = root.join("x.r00");

        assert!(
            missing_old_style_main(&tail),
            "同名目录不能代替老式分卷族的主包文件（X-10/E-05）"
        );

        fs::remove_dir(&main).unwrap();
        fs::write(&main, b"main archive").unwrap();
        assert!(
            !missing_old_style_main(&tail),
            "实际普通文件主包应满足老式分卷族入口检查"
        );
    }

// 覆盖 X-10（回归：混用最小补零宽度即使卷号不同也属于命名歧义）。
#[test]
    #[rustfmt::skip]
    fn part_rar_mixed_padding_widths_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("x.part01.rar"), b"padded first volume").unwrap();
        let second = root.join("x.part2.rar");
        fs::write(&second, b"unpadded second volume").unwrap();

        let named = volume_set(&second).unwrap();
        assert!(
            named.part_ambiguity.is_some(),
            "part01 与 part2 混用补零模式，即使卷号不同也必须识别为歧义（X-10）"
        );
        assert!(
            x10_volume_precheck("x.part2.rar", &second, &named).is_err(),
            "命名歧义组必须在引擎解析前按整组失败处置（X-10）"
        );
    }

// 覆盖 X-09/X-10：相似但不属于精确卷族的文件不得加入隔离集合。
#[test]
    #[rustfmt::skip]
    fn review_exact_volume_families_exclude_wide_numeric_suffixes() {
        let temp = tempfile::tempdir().unwrap();
        for (entry, included, excluded) in [
            ("a.zip", "a.z01", "a.z001"),
            ("b.rar", "b.r00", "b.r000"),
            ("c.7z.001", "c.7z.002", "c.7z.01000"),
        ] {
            for name in [entry, included, excluded] {
                fs::write(temp.path().join(name), b"volume").unwrap();
            }
            let set = volume_set(&temp.path().join(entry)).unwrap();
            assert!(set.paths.contains(&temp.path().join(included)));
            assert!(
                !set.paths.contains(&temp.path().join(excluded)),
                "非精确卷族文件不得隔离：{excluded}"
            );
        }
    }

// 覆盖 X-10：非 part 名字中的数字不能授权吸入另一个独立 rar。
#[test]
    #[rustfmt::skip]
    fn review_rar_numeric_stem_does_not_authorize_other_archives() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("report1.rar");
        fs::write(&archive, b"first").unwrap();
        fs::write(root.path().join("report2.rar"), b"independent").unwrap();
        let info = ArchiveVolumes {
            kind: "rar5".to_string(),
            count: 2,
            new_rar_names: true,
        };
        assert!(
            VolumeScheme::OldRar.actual_paths(&archive, &info).is_err(),
            "非精确族的第二个独立包不得成为删源集合"
        );
        assert_eq!(
            fs::read(root.path().join("report2.rar")).unwrap(),
            b"independent"
        );
    }

// X-10：Unicode 小写扩展不是 Windows 序数等价，独立族不得参与彼此删源。
#[test]
    #[rustfmt::skip]
    fn review_ordinal_volume_families() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("i\u{0307}.zip.001");
        let unrelated = root.path().join("\u{0130}.zip.002");
        fs::write(&first, b"selected volume").unwrap();
        fs::write(&unrelated, b"independent user volume").unwrap();
        assert_ne!(
            fsutil::fold_rel("i\u{0307}.zip"),
            fsutil::fold_rel("\u{0130}.zip"),
        );
        assert_eq!(volume_set(&first).unwrap().paths, vec![first]);
        assert_eq!(fs::read(unrelated).unwrap(), b"independent user volume");
        let one = root.path().join("σ.zip.001");
        let two = root.path().join("ς.zip.002");
        fs::write(&one, b"first").unwrap();
        fs::write(&two, b"second").unwrap();
        assert_eq!(volume_set(&one).unwrap().paths, vec![one, two]);
    }
