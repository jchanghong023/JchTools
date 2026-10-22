use super::*;

// 覆盖 H-06（选定根目录直接含 .git：整次处理不执行并明确提示）
#[test]
fn root_git_directory_blocks_prepare_count_and_extract() {
    let f = Fixture::new();
    f.git_marker_dir("");
    f.write("keep.txt", b"payload");
    let error = format!(
        "{:#}",
        engine::prepare_at(&f.root, base(), Context::default(), &f.state).unwrap_err()
    );
    assert!(
        error.contains(".git"),
        "根目录含 .git 必须明确提示：{error}"
    );
    let error = format!(
        "{:#}",
        jchtools_recursive_extract::engine::count_archives(&f.root, &base()).unwrap_err()
    );
    assert!(error.contains(".git"), "确认框清点同样必须拒绝：{error}");
    let error = format!(
        "{:#}",
        jchtools_recursive_extract::engine::extract_run_at(
            &f.root,
            base(),
            Context::default(),
            &f.state,
            None
        )
        .unwrap_err()
    );
    assert!(error.contains(".git"), "递归解压同样必须拒绝：{error}");
    let tasks = f.state.join("tasks");
    assert!(
        !tasks.exists() || fs::read_dir(&tasks).unwrap().next().is_none(),
        "被拒绝的任务不得留下任务目录（整次处理不执行）"
    );
}

// 覆盖 H-06（.git 为文件——worktree/submodule——同样触发整次拒绝）
#[test]
fn root_git_file_blocks_processing() {
    let f = Fixture::new();
    f.git_marker_file("");
    f.write("keep.txt", b"payload");
    assert!(engine::prepare_at(&f.root, base(), Context::default(), &f.state).is_err());
    assert!(jchtools_recursive_extract::engine::count_archives(&f.root, &base()).is_err());
}

// 覆盖 H-06（子目录含 .git：只排除该子树；祖先不得被改名/删除，内容不读取、不参与哈希去重）
#[test]
fn child_git_tree_excluded_and_ancestors_untouched() {
    let f = Fixture::new();
    f.write("other/keep.txt", b"same-content");
    // Git 子树里的内容与 other/keep.txt 内容相同：一旦被扫描/哈希，去重会计划删除
    // 其中一份，计划里就会出现 Git 路径——这是「不读取、不参与去重」的反证锚点。
    f.write("only_dir/proj/note.txt", b"same-content");
    f.git_marker_dir("only_dir/proj");
    f.write("only_file/proj/note.txt", b"payload");
    f.git_marker_file("only_file/proj");
    let task = f.plan(base());
    assert_eq!(
        task.summary.scanned, 1,
        "Git 子树内容不得进入盘点：{:#?}",
        task.summary
    );
    let db = Database::open_existing(&task.directory).unwrap();
    let git_files: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE rel LIKE 'only_dir/%' OR rel LIKE 'only_file/%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(git_files, 0, "Git 子树文件不得入库（也就不参与哈希）");
    let actions = db.actions_page(0, 1000).unwrap();
    drop(db);
    // Git 树内部不得被规划任何动作；项目根自身按 C-14 整体移入「Git项目集合」。
    assert!(
        actions
            .iter()
            .all(|action| !action.source.starts_with("only_dir/proj/")
                && !action.source.starts_with("only_file/proj/")),
        "Git 子树内部不得出现在计划里：{actions:#?}"
    );
    let git_moves = actions
        .iter()
        .filter(|a| {
            a.kind == jchtools_file_core::model::ActionKind::Move
                && (a.source == "only_dir/proj" || a.source == "only_file/proj")
        })
        .count();
    assert_eq!(git_moves, 2, "两个 Git 项目整体移入集合（C-14）");
    drop(actions);
    assert!(
        events(&task.directory).contains("Git"),
        "跳过 Git 目录必须明确提示：{}",
        events(&task.directory)
    );
    engine::apply(&task.directory, Context::default()).unwrap();
    // 同名项目按 C-17/C-18 加最近来源目录前缀消解，整树内容原样随移动。
    assert_eq!(
        fs::read(f.root.join("Git项目集合/only_dir_proj/note.txt")).unwrap(),
        b"same-content",
        "Git 项目内部内容必须原样随整树移动（H-06/C-14）"
    );
    assert_eq!(
        fs::read(f.root.join("Git项目集合/only_dir_proj/.git/HEAD")).unwrap(),
        b"ref: refs/heads/main\n"
    );
    assert_eq!(
        fs::read(f.root.join("Git项目集合/only_file_proj/note.txt")).unwrap(),
        b"payload",
        ".git 为文件（worktree 形态）的项目同样整树移动"
    );
    assert_eq!(
        fs::read(f.root.join("Git项目集合/only_file_proj/.git")).unwrap(),
        b"gitdir: ../.git/modules/x\n"
    );
    assert_eq!(
        fs::read(f.root.join("文档/keep.txt")).unwrap(),
        b"same-content",
        "Git 之外的普通文件照常归类保留，不能靠同名 Git 内文件冒充结果"
    );
}

// 覆盖 H-06（Git 树内的崩溃残留不得被清扫）
#[test]
fn stale_link_temp_inside_git_tree_is_not_swept() {
    let f = Fixture::new();
    f.write("keep.txt", b"payload");
    f.git_marker_dir("proj");
    let stale = f.root.join("proj/.jchtools-link-deadbeef");
    fs::hard_link(f.root.join("keep.txt"), &stale).unwrap();
    filetime::set_file_mtime(&stale, filetime::FileTime::from_unix_time(0, 0)).unwrap();
    let task = f.plan(base());
    engine::apply(&task.directory, Context::default()).unwrap();
    // 项目随 C-14 整体移入集合；树内的崩溃残留随树移动且绝不被清扫（H-06）。
    assert!(
        f.root
            .join("Git项目集合/proj/.jchtools-link-deadbeef")
            .exists(),
        "Git 树内的一切内容都不得被删除（H-06）"
    );
}

// 覆盖 H-06（Git 树内的空目录不得被整理清理，树本身只随项目整体移动）
#[test]
fn empty_directories_inside_git_tree_are_preserved() {
    let f = Fixture::new();
    f.write("keep.txt", b"payload");
    f.git_marker_dir("proj");
    f.dir("proj/emptydir");
    f.dir("proj/sub/deep");
    let task = f.plan(base());
    engine::apply(&task.directory, Context::default()).unwrap();
    let moved = f.root.join("Git项目集合/proj");
    assert!(moved.join("emptydir").is_dir());
    assert!(moved.join("sub/deep").is_dir());
    assert!(moved.is_dir());
    assert!(moved.join(".git/HEAD").is_file());
}

// 覆盖 H-06/S-01（回归：目标大类目录「图片」是被留在原地的 Git 项目时——集合
// 容器被普通文件占用导致项目无法整体移入——普通图片文件不得被移入该 Git 树，
// 相关归类失败并保留源项；修复前 planner 把普通文件照常计划移入受保护树内）
#[test]
fn classify_refuses_target_dir_occupied_by_git_project() {
    let f = Fixture::new();
    // 大类目录「图片」本身是 Git 项目；「Git项目集合」被普通文件占用（S-01），
    // 项目因此留在原地（gather_dir_plans 修复后它构成 blocked 目标）。
    f.git_marker_dir("图片");
    f.write("Git项目集合", b"occupier");
    f.write("照片/logo.png", b"png");
    let task = f.plan(base());
    let db = Database::open_existing(&task.directory).unwrap();
    let moves_into_git_tree: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM actions WHERE target LIKE '图片/%' OR target = '图片'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        moves_into_git_tree,
        0,
        "不得有任何文件被计划移入受保护的 Git 树：{}",
        events(&task.directory)
    );
    drop(db);
    // 源文件保留在原位（分类失败不移动）
    assert!(
        f.root.join("照片/logo.png").is_file(),
        "归类失败的文件必须保留源项"
    );
    // Git 树内零改动
    assert!(f.root.join("图片/.git/HEAD").is_file());
    let entries: Vec<_> = fs::read_dir(f.root.join("图片"))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(entries.len(), 1, "Git 树内不得新增任何条目");
}

// 覆盖 S-01 / C-05 / C-14：固定大类被 Git 项目占用时保留；
// 不占固定容器的其他项目仍正常整体归类，不能因保护一棵树取消全部归类。
#[test]
fn fixed_category_git_occupant_is_preserved_while_other_projects_are_collected() {
    let f = Fixture::new();
    f.git_marker_dir("图片");
    f.git_marker_dir("ordinary-project");
    f.write("照片/logo.png", b"png");
    let task = f.plan(base());
    let db = Database::open_existing(&task.directory).unwrap();
    let protected_actions: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM actions WHERE source='图片' OR target LIKE '图片/%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let ordinary_project_moved: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM actions WHERE source='ordinary-project' AND target='Git项目集合/ordinary-project'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(db);
    assert_eq!(
        protected_actions,
        0,
        "不得通过腾空固定容器挪动占用 Git 树：{}",
        events(&task.directory)
    );
    assert_eq!(ordinary_project_moved, 1, "其他项目仍须按 C-14 整体归类");
    let result = engine::apply(&task.directory, Context::default()).unwrap();
    assert!(result.summary.errors > 0, "固定容器占用必须明确报告");
    assert_eq!(fs::read(f.root.join("照片/logo.png")).unwrap(), b"png");
    assert_eq!(
        fs::read(f.root.join("图片/.git/HEAD")).unwrap(),
        b"ref: refs/heads/main\n"
    );
    assert!(!f.root.join("Git项目集合/图片").exists());
    assert!(!f.root.join("图片/logo.png").exists());
    assert_eq!(
        fs::read(f.root.join("Git项目集合/ordinary-project/.git/HEAD")).unwrap(),
        b"ref: refs/heads/main\n"
    );
    assert!(
        !f.root.join("ordinary-project").exists(),
        "普通项目须完成整体移动"
    );
}

// 覆盖 C-14/C-21（回归：名为「Git项目集合」的 Git 项目不得生成移入自身的
// 自嵌套计划——集合目录不自嵌套；项目保留原位，其余文件正常归类）
#[test]
fn git_collection_named_project_never_nests_into_itself() {
    let f = Fixture::new();
    f.git_marker_dir("Git项目集合");
    f.write("a.txt", b"x");
    let task = f.plan(base());
    let db = Database::open_existing(&task.directory).unwrap();
    let nesting: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM actions WHERE target LIKE 'Git项目集合/%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let moved_itself: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM actions WHERE source='Git项目集合'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(db);
    assert_eq!(
        nesting,
        0,
        "不得出现集合目录自嵌套计划：{}",
        events(&task.directory)
    );
    assert_eq!(moved_itself, 0, "名为集合容器的项目自身不得被移动");
    // 项目原地保留、树内零改动
    assert!(f.root.join("Git项目集合/.git/HEAD").is_file());
    // 普通文件照常归类（.txt 按附录 A 归「文档」大类）
    engine::apply(&task.directory, Context::default()).unwrap();
    assert!(f.root.join("文档/a.txt").is_file(), "普通文件归类不受影响");
    assert!(
        f.root.join("Git项目集合/.git/HEAD").is_file(),
        "项目在集合位置原地保留"
    );
}

// 覆盖 C-14/C-21：名称等于集合容器的嵌套 Git 项目仍应平铺移入根集合；
// 仅根目录本身的集合容器不得移动到自身内部。
#[test]
fn nested_git_project_named_collection_is_moved() {
    let f = Fixture::new();
    f.git_marker_dir("nested/Git项目集合");
    f.write("nested/Git项目集合/README.md", b"project");

    let task = f.plan(base());
    assert_eq!(task.summary.planned_git, 1);
    engine::apply(&task.directory, Context::default()).unwrap();

    let moved = f.root.join("Git项目集合/Git项目集合");
    assert!(moved.join(".git/HEAD").is_file(), "项目名按原样保留");
    assert!(
        moved.join("README.md").is_file(),
        "Git 项目必须作为整树移动"
    );
    assert!(
        !f.root.join("nested/Git项目集合").exists(),
        "源项目移动后不得残留"
    );
}

// 覆盖 C-18/C-14：项目直接集合容器被剔除后，紧邻的标准大类目录也应剔除。
#[test]
fn git_project_sources_skip_collection_and_category_levels() {
    let f = Fixture::new();
    f.git_marker_dir("branchA/文档/Git项目集合/tools");
    f.git_marker_dir("branchB/文档/Git项目集合/tools");

    let task = f.plan(base());
    assert_eq!(task.summary.planned_git, 2);
    engine::apply(&task.directory, Context::default()).unwrap();

    assert!(f.root.join("Git项目集合/branchA_tools/.git").is_dir());
    assert!(f.root.join("Git项目集合/branchB_tools/.git").is_dir());
    assert!(
        !f.root.join("branchA/文档/Git项目集合").exists()
            && !f.root.join("branchB/文档/Git项目集合").exists(),
        "移动后旧的类别与集合路径应清空"
    );
}
