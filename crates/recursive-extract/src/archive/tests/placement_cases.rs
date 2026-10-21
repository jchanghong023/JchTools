use super::*;

// 覆盖 X-04/X-10：改名目录不能占用同包另一个目录的原名，避免嵌套卷族混合。
#[test]
    #[rustfmt::skip]
    fn directory_rename_preserves_incoming_sibling_names() {
        let root = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        fs::write(root.path().join("sub"), b"existing file").unwrap();
        fs::create_dir_all(stage.path().join("sub/child")).unwrap();
        fs::create_dir_all(stage.path().join("sub (1)/child")).unwrap();
        fs::write(stage.path().join("sub (2)"), b"incoming file").unwrap();
        let renames =
            plan_directory_renames(&Control::default(), stage.path(), root.path(), &mut HashSet::new()).unwrap();
        assert_eq!(
            mapped_entry_rel(&renames, "sub/child/payload.txt"),
            Some("sub (3)/child/payload.txt".to_string()),
            "新目录须避开同包已占用的目录名及文件名"
        );
        assert_eq!(mapped_entry_rel(&renames, "sub (1)/child/payload.txt"), None);
        assert_eq!(fs::read(root.path().join("sub")).unwrap(), b"existing file");
    }

// 覆盖 X-06/X-08：取消后不得开始隔离移动，源包及每个分卷留在原位。
#[test]
    #[rustfmt::skip]
    fn quarantine_stops_before_moving_when_cancelled() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        for name in ["pack.7z.001", "pack.7z.002"] {
            fs::write(root.join(name), name.as_bytes()).unwrap();
        }
        let mut job = review_job(&root, &temp.path().join("state"), 0);
        job.context.control.cancel();
        let error = quarantine(&mut job, "pack.7z.001", "测试：损坏分卷").unwrap_err();
        assert!(error.to_string().contains("取消"));
        assert_eq!(job.summary.archives_quarantined, 0);
        assert!(!root.join(QUARANTINE_DIR_NAME).exists());
        for name in ["pack.7z.001", "pack.7z.002"] {
            assert_eq!(fs::read(root.join(name)).unwrap(), name.as_bytes());
        }
    }

// 覆盖 H-06：已知祖先不在 Git 树内，不代表后代也没有独立 Git 边界。
#[test]
    #[rustfmt::skip]
    fn git_cache_checks_descendants_of_an_allowed_directory() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let repo = parent.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let mut boundaries = GitBoundaries::default();
        assert!(!boundaries.blocked(root.path(), &parent).unwrap());
        assert!(boundaries.blocked(root.path(), &repo).unwrap());
    }

// 覆盖 H-06：Git 保护只向后代传播，不能错误排除 Git 树外的兄弟目录。
#[test]
    #[rustfmt::skip]
    fn git_cache_does_not_protect_siblings_of_a_repository() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("parent/repo");
        let sibling = root.path().join("parent/ordinary");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let mut boundaries = GitBoundaries::default();
        assert!(boundaries.blocked(root.path(), &repo).unwrap());
        assert!(!boundaries.blocked(root.path(), &sibling).unwrap());
    }

// 覆盖 H-06（回归：「解压失败」容器自身是 Git 项目（.git 为目录）时，
// 坏包不得移入其中；修复前容器规划只拒链接/同名文件，失败包被腾入 Git 树）
#[test]
    #[rustfmt::skip]
    fn quarantine_refuses_git_project_container_directory() {
        quarantine_git_container_case("dir");
    }

// 覆盖 H-06（回归：同上，.git 为文件形态——is_git_root 两种形态都保护整树）
#[test]
    #[rustfmt::skip]
    fn quarantine_refuses_git_project_container_file() {
        quarantine_git_container_case("file");
    }

// 覆盖 X-04/H-06：普通目录若是 Git 项目，应为新目录另选名字。
#[test]
    #[rustfmt::skip]
    fn review_directory_collision_with_git_tree_uses_new_name() {
        let root = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("repo/.git")).unwrap();
        fs::write(root.path().join("repo/keep.txt"), b"original").unwrap();
        fs::create_dir_all(stage.path().join("repo/sub")).unwrap();
        let renames =
            plan_directory_renames(&Control::default(), stage.path(), root.path(), &mut HashSet::new()).unwrap();
        assert_eq!(
            mapped_entry_rel(&renames, "repo/sub/member.txt"),
            Some("repo (1)/sub/member.txt".to_string())
        );
        assert_eq!(
            fs::read(root.path().join("repo/keep.txt")).unwrap(),
            b"original"
        );
        assert!(!root.path().join("repo/sub").exists());
        fs::create_dir(root.path().join(QUARANTINE_DIR_NAME)).unwrap();
        fs::write(root.path().join("解压失败/old.zip"), b"old user archive").unwrap();
        fs::create_dir(stage.path().join(QUARANTINE_DIR_NAME)).unwrap();
        let renames =
            plan_directory_renames(&Control::default(), stage.path(), root.path(), &mut HashSet::new()).unwrap();
        assert_eq!(
            mapped_entry_rel(&renames, "解压失败/new.zip"),
            Some("解压失败 (1)/new.zip".to_string()),
            "新目录不得向已有隔离容器合入"
        );
        assert_eq!(
            fs::read(root.path().join("解压失败/old.zip")).unwrap(),
            b"old user archive"
        );
    }

// 覆盖 X-04：祖先改名必须传递至任意深度的占用规划。
#[test]
    #[rustfmt::skip]
    fn review_directory_mapping_propagates_through_unrenamed_parents() {
        let root = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), b"occupier").unwrap();
        fs::create_dir_all(stage.path().join("a/b/c")).unwrap();
        let renames =
            plan_directory_renames(&Control::default(), stage.path(), root.path(), &mut HashSet::new()).unwrap();
        assert_eq!(
            mapped_entry_rel(&renames, "a/b/c/member.txt"),
            Some("a (1)/b/c/member.txt".to_string())
        );
    }

// 覆盖 X-10：冲突序号不能以截断分卷后缀来腾位。
#[test]
    #[rustfmt::skip]
    fn review_quarantine_preserves_long_volume_suffix_or_fails() {
        let root = tempfile::tempdir().unwrap();
        let ext = format!(".part{}.rar", "1".repeat(245));
        fs::write(root.path().join(format!("a{ext}")), b"occupier").unwrap();
        assert!(
            quarantine_targets(root.path(), &[("a".to_string(), ext)]).is_err(),
            "不能完整保留后缀时必须安全失败"
        );
    }

// 覆盖 X-10：精确命名族缺中间卷时，解码前直接拒绝整组。
#[test]
    #[rustfmt::skip]
    fn review_exact_volume_gaps_are_rejected_before_engine() {
        let root = tempfile::tempdir().unwrap();
        for (first, later) in [
            ("numbered.7z.001", "numbered.7z.003"),
            ("part.part1.rar", "part.part3.rar"),
            ("old.rar", "old.r01"),
            ("zip.zip", "zip.z02"),
        ] {
            let archive = root.path().join(first);
            fs::write(&archive, b"first").unwrap();
            fs::write(root.path().join(later), b"later").unwrap();
            let named = volume_set(&archive).unwrap();
            assert!(
                x10_volume_precheck(first, &archive, &named).is_err(),
                "中间断号必须拒绝：{first} / {later}"
            );
        }
    }

// 覆盖 S-05：只读目标判定不得把系统属性“包含”解释成允许写入系统目录。
#[test]
    #[rustfmt::skip]
    fn review_system_directory_destination_is_excluded() {
        let protected = fsutil::protected_root().unwrap();
        let root = protected.parent().unwrap();
        let destination = protected.join("jchtools-review-never-created.txt");
        assert!(
            excluded_destination(
                root,
                &destination,
                &Config::default(),
                &mut GitBoundaries::default(),
            )
            .unwrap(),
            "实际系统目录及其后代必须排除；本测试只读路径，不创建任何系统文件"
        );
    }

// 覆盖 H-06：归档内同目录的大小写变体仍属于同一 Git 排除子树。
#[test]
    #[rustfmt::skip]
    fn review_git_subtree_matches_windows_case_variants() {
        let subtrees = HashSet::from([git_boundary_prefix("project/.GIT/config").unwrap()]);
        assert!(inside_git_subtree(&subtrees, "PROJECT/README.md"));
        assert!(inside_git_subtree(&subtrees, "Project/src/member.txt"));
        assert!(!inside_git_subtree(&subtrees, "project-other/README.md"));
    }

#[test]
    #[rustfmt::skip]
    fn review_quarantine_common_long_family_stem() {
        let dir = tempfile::tempdir().unwrap();
        let stem = "x".repeat(244);
        let names = vec![
            (stem.clone(), ".part1.rar".to_string()),
            (stem.clone(), ".part10.rar".to_string()),
        ];
        let old = dir.path().join(format!("{stem}.part1.rar"));
        fs::write(&old, b"old user archive").unwrap();
        let targets = quarantine_targets(dir.path(), &names).unwrap().unwrap();
        let one = targets[0].file_name().unwrap().to_str().unwrap();
        let two = targets[1].file_name().unwrap().to_str().unwrap();
        assert!(one.ends_with(".part1.rar"));
        assert!(two.ends_with(".part10.rar"));
        assert_eq!(
            fsutil::split_compound_name(one).0,
            fsutil::split_compound_name(two).0
        );
        assert_eq!(fs::read(old).unwrap(), b"old user archive");
    }

#[test]
    #[rustfmt::skip]
    fn review_placement_common_long_family_stem() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let content = temp.path().join("content");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&content).unwrap();
        let stem = "x".repeat(244);
        let one = format!("{stem}.part1.rar");
        let two = format!("{stem}.part10.rar");
        fs::write(content.join(&one), b"new first").unwrap();
        fs::write(content.join(&two), b"new tenth").unwrap();
        fs::write(root.join(&one), b"old user archive").unwrap();
        let job = review_job(&root, &temp.path().join("task.sqlite3"), 0);
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
        let first = targets
            .get(&one)
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        let tenth = targets
            .get(&two)
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(first.ends_with(".part1.rar"));
        assert!(tenth.ends_with(".part10.rar"));
        assert_eq!(
            fsutil::split_compound_name(first).0,
            fsutil::split_compound_name(tenth).0
        );
        assert_eq!(fs::read(root.join(&one)).unwrap(), b"old user archive");
    }
