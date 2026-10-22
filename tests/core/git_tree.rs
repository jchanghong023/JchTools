use super::*;

// 覆盖 H-06, S-01, C-14（固定容器占用项目原位保留，普通项目整体移动；
// 普通文件既不进入保留的 Git 树，也不进入集合内的项目）
#[test]
fn classification_target_never_enters_git_tree() {
    let f = Fixture::new();
    let marker = b"[core]\nrepositoryformatversion = 0\n";
    f.write("图片/.git/config", marker, 10);
    f.write("图片/keep.png", b"protected image", 11);
    f.write("projects/camera/.git/config", marker, 12);
    f.write("projects/camera/src/main.rs", b"fn main() {}", 13);
    f.write("photo.png", b"photo", 20);
    f.write("doc/a.pdf", b"pdf", 30);
    let cfg = base();
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    let git_moves = moves
        .iter()
        .filter(|action| action.source == "projects/camera")
        .collect::<Vec<_>>();
    drop(db);
    assert_eq!(
        task.summary.scanned, 2,
        "Git 整树排除：只应扫描 photo.png 与 doc/a.pdf"
    );
    assert_eq!(task.summary.planned_git, 1, "普通项目整体移入集合（C-14）");
    assert_eq!(git_moves.len(), 1);
    assert_eq!(
        git_moves[0].target.as_deref(),
        Some("Git项目集合/camera"),
        "项目目录名不变，整树移入集合"
    );
    assert!(
        moves
            .iter()
            .filter(|action| action.source != "projects/camera")
            .all(|action| action.target.as_deref().is_none_or(|target| {
                !target.starts_with("图片/") && !target.starts_with("Git项目集合/")
            })),
        "归类目标不得进入保留或移动后的 Git 项目：{moves:?}"
    );
    assert!(
        moves.iter().all(|action| action.source != "图片"
            && !action.source.starts_with("图片/")
            && !action.source.starts_with("projects/camera/")),
        "固定容器占用项目不挪走，两棵 Git 树内部均不规划操作"
    );
    let result = Fixture::apply(&task);
    assert!(
        result.summary.errors > 0,
        "图片容器受保护时须如实报告相关归类失败"
    );
    assert_eq!(fs::read(f.root.join("图片/.git/config")).unwrap(), marker);
    assert_eq!(
        fs::read(f.root.join("图片/keep.png")).unwrap(),
        b"protected image"
    );
    assert_eq!(fs::read(f.root.join("photo.png")).unwrap(), b"photo");
    assert!(!f.root.join("图片/photo.png").exists());
    assert_eq!(fs::read(f.root.join("文档/a.pdf")).unwrap(), b"pdf");
    assert_eq!(
        fs::read(f.root.join("Git项目集合/camera/.git/config")).unwrap(),
        marker
    );
    assert_eq!(
        fs::read(f.root.join("Git项目集合/camera/src/main.rs")).unwrap(),
        b"fn main() {}"
    );
    assert!(!f.root.join("projects").exists());
}

// 覆盖 H-06, C-01, C-07（取消普通 Git 项目整体移动后，依赖其腾空的
// 祖先清理自动取消；独立归类保留勾选并正常执行，不删除保留项目）
#[test]
fn cancelling_git_move_cancels_cleanup_of_retained_git_tree_ancestors() {
    let f = Fixture::new();
    let marker = b"[core]\nrepositoryformatversion = 0\n";
    f.write("old/proj/.git/config", marker, 10);
    f.write("old/proj/keep.png", b"protected image", 11);
    f.write("photo.png", b"photo", 20);
    let task = f.plan(base());
    let db = Database::open(&task.directory).unwrap();
    let actions = db.actions_page(0, 100).unwrap();
    let git_move = actions
        .iter()
        .find(|action| {
            action.kind == ActionKind::Move
                && action.source == "old/proj"
                && action.target.as_deref() == Some("Git项目集合/proj")
        })
        .expect("普通 Git 项目整体移动（C-14）");
    let ancestor_cleanup = actions
        .iter()
        .find(|action| action.kind == ActionKind::EmptyDirectory && action.source == "old")
        .expect("项目移走后原祖先应清理（C-07）");
    let photo_move = actions
        .iter()
        .find(|action| {
            action.kind == ActionKind::Move
                && action.source == "photo.png"
                && action.target.as_deref() == Some("图片/photo.png")
        })
        .expect("独立的普通文件归类");
    assert!(git_move.selected && ancestor_cleanup.selected && photo_move.selected);
    db.set_selected(git_move.id, false).unwrap();
    let git_move_id = git_move.id;
    let cleanup_id = ancestor_cleanup.id;
    let photo_move_id = photo_move.id;
    drop(db);

    engine::recompute_plan(&task.directory).unwrap();
    let db = Database::open(&task.directory).unwrap();
    assert!(
        !db.action(git_move_id).unwrap().selected,
        "不得新增用户勾选"
    );
    assert!(
        !db.action(cleanup_id).unwrap().selected,
        "保留项目使原祖先不再腾空，必须取消依赖的清理"
    );
    for action in db.actions_page(0, 100).unwrap() {
        if action.kind == ActionKind::EmptyDirectory {
            assert!(
                !action.selected,
                "保留项目相关的根目录和祖先清理均不得执行：{}",
                action.source
            );
        }
    }
    assert!(
        db.action(photo_move_id).unwrap().selected,
        "独立归类不得取消"
    );
    drop(db);
    assert_eq!(engine::recompute_plan(&task.directory).unwrap(), 0);
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(
        fs::read(f.root.join("old/proj/.git/config")).unwrap(),
        marker
    );
    assert_eq!(
        fs::read(f.root.join("old/proj/keep.png")).unwrap(),
        b"protected image"
    );
    assert!(f.root.join("old").is_dir());
    assert!(!f.root.join("Git项目集合").exists());
    assert_eq!(fs::read(f.root.join("图片/photo.png")).unwrap(), b"photo");
    assert!(!f.root.join("photo.png").exists());
}

// 覆盖 H-06, C-07（Git 树整树随项目移动且内容原样；移走后腾空的祖先按 C-07 消失）
#[test]
fn git_tree_ancestors_survive_empty_directory_cleanup() {
    let f = Fixture::new();
    f.write("only/proj/.git/HEAD", b"ref: refs/heads/main\n", 10);
    f.write("only/proj/src/main.rs", b"fn main() {}", 11);
    f.write("keep.txt", b"payload", 12);
    fs::create_dir_all(f.root.join("empty/deep")).unwrap();
    let cfg = base();
    // 默认排除表为空：Git 排除只能靠引擎识别 .git 边界，而不是默认 glob。
    let task = f.plan(cfg);
    assert_eq!(
        task.summary.scanned, 1,
        "Git 整树排除：只有 keep.txt 参与扫描"
    );
    let db = Database::open(&task.directory).unwrap();
    let actions = db.actions_page(0, 200).unwrap();
    drop(db);
    assert_eq!(task.summary.planned_git, 1, "项目整体移入集合（C-14）");
    // Git 树内部不得被规划任何动作（H-06：不进项目内部）；唯一的“only/”动作
    // 是项目根自身的整体移动（source == "only/proj"）。
    assert!(
        actions
            .iter()
            .all(|action| !action.source.starts_with("only/proj/")),
        "Git 树内部不得被规划任何动作：{actions:?}"
    );
    Fixture::apply(&task);
    assert_eq!(
        fs::read(f.root.join("Git项目集合/proj/.git/HEAD")).unwrap(),
        b"ref: refs/heads/main\n",
        "Git 标记内容不得改写"
    );
    assert_eq!(
        fs::read(f.root.join("Git项目集合/proj/src/main.rs")).unwrap(),
        b"fn main() {}",
        "Git 项目内部文件必须原样移动"
    );
    assert!(
        !f.root.join("only").exists(),
        "项目移走后腾空的原始祖先必须清理（C-07/H-05）"
    );
    assert!(
        !f.root.join("Git项目集合/proj").join("文档").exists(),
        "归类目录不得建进 Git 项目内部（H-06）"
    );
    assert!(
        exists_somewhere(&f.root, "keep.txt"),
        "Git 之外的普通文件照常归类保留"
    );
    assert!(
        !f.root.join("empty").exists(),
        "Git 之外的实空空目录照常清理（C-07）"
    );
}
