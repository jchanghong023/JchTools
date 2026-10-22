use super::*;

// 覆盖 C-01（计划逐项可勾选/取消）
#[test]
fn unselecting_action_preserves_file() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let db = Database::open(&task.directory).unwrap();
    let action = db
        .actions_page(0, 10)
        .unwrap()
        .into_iter()
        .find(|action| matches!(action.kind, ActionKind::Delete))
        .expect("存在待取消的去重删除行");
    db.set_selected(action.id, false).unwrap();
    drop(db);
    Fixture::apply(&task);
    assert!(
        exists_somewhere(&f.root, "a") && exists_somewhere(&f.root, "b"),
        "取消勾选的删除不执行，两份文件都随归类保留"
    );
}
// 覆盖 C-17, C-18（回归：已确定删除的重复副本名字执行后腾空，不构成固定占用——
// 幸存者归类不得因此多加一级来源前缀，违反「最少级数」）
#[test]
fn doomed_duplicate_name_does_not_force_source_prefix() {
    let f = Fixture::new();
    f.write("文档/合同.pdf", b"same", 10); // 较旧 → 去重删除，名字将腾空
    f.write("下载/合同.pdf", b"same", 20); // 较新 → 保留者，归类进 文档/
    let task = f.plan(base());
    let db = Database::open(&task.directory).unwrap();
    let actions = db.actions_page(0, 100).unwrap();
    let moved = actions
        .iter()
        .find(|a| matches!(a.kind, ActionKind::Move))
        .expect("幸存者的归类移动");
    assert_eq!(
        moved.target.as_deref(),
        Some("文档/合同.pdf"),
        "待删项名字腾空后幸存者直接占用（C-18 最少级数）：{:?}",
        moved.target
    );
    drop(db);
    Fixture::apply(&task);
    assert!(f.root.join("文档/合同.pdf").is_file());
    assert_eq!(fs::read(f.root.join("文档/合同.pdf")).unwrap(), b"same");
}

// 覆盖 C-17/C-18（归类时移出目标目录的现有文件应先腾出其名称；
// 目标目录的幸存者不得因该项即将移走而多加来源前缀）。
#[test]
fn outgoing_category_item_frees_name_for_incoming_item() {
    let f = Fixture::new();
    // 大文件规则把「文档」中的旧项移到「大文件」，为同名迁入项腾出文档目录中的名称。
    f.write("文档/big.txt", b"old", 10);
    f.write("来源/big.txt", b"n", 20);
    let mut cfg = base();
    cfg.large_files = true;
    cfg.large_threshold_bytes = 2;
    let task = f.plan(cfg);
    let actions = Database::open(&task.directory)
        .unwrap()
        .actions_page(0, 100)
        .unwrap();
    assert!(
        actions.iter().any(|action| {
            matches!(action.kind, ActionKind::Move)
                && action.source == "文档/big.txt"
                && action.target.as_deref() == Some("大文件/big.txt")
        }),
        "原目标目录中的文件应被规划移出：{actions:?}"
    );
    let incoming = actions
        .iter()
        .find(|action| action.source == "来源/big.txt")
        .expect("存在同名迁入项");
    assert_eq!(
        incoming.target.as_deref(),
        Some("文档/big.txt"),
        "移出项执行后释放原名称，迁入项应保持最短名称：{incoming:?}"
    );
    Fixture::apply(&task);
    assert!(f.root.join("大文件/big.txt").is_file());
    assert!(f.root.join("文档/big.txt").is_file());
    assert!(!f.root.join("文档/来源_big.txt").exists());
}

// 覆盖 C-17/C-18：移出项的目标容器被普通文件阻塞时，原名称仍被占用，
// 不能提前腾名并让后续迁入项规划到同一位置。
#[test]
fn blocked_outgoing_category_keeps_its_source_name_occupied() {
    let f = Fixture::new();
    f.write("文档/big.txt", b"old", 10);
    f.write("来源/big.txt", b"n", 20);
    f.write("大文件", b"container occupied", 30);
    let mut cfg = base();
    cfg.large_files = true;
    cfg.large_threshold_bytes = 2;
    let task = f.plan(cfg);
    let actions = Database::open(&task.directory)
        .unwrap()
        .actions_page(0, 100)
        .unwrap();
    let incoming = actions
        .iter()
        .find(|action| action.source == "来源/big.txt")
        .expect("迁入项仍有计划行");
    assert_ne!(
        incoming.target.as_deref(),
        Some("文档/big.txt"),
        "移出项失败后仍占用原名：{actions:?}"
    );
}

// 覆盖 C-01/C-17（取消腾名移动后，依赖该名称的迁入项固定点重算为未勾选）。
#[test]
fn cancelling_outgoing_move_cancels_blocked_incoming_move() {
    let f = Fixture::new();
    f.write("文档/big.txt", b"old", 10);
    f.write("来源/big.txt", b"n", 20);
    let mut cfg = base();
    cfg.large_files = true;
    cfg.large_threshold_bytes = 2;
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let actions = db.actions_page(0, 100).unwrap();
    let outgoing = actions
        .iter()
        .find(|action| action.source == "文档/big.txt")
        .expect("存在腾名移动")
        .id;
    db.set_selected(outgoing, false).unwrap();
    drop(db);
    let cancelled = engine::recompute_plan(&task.directory).unwrap();
    assert!(cancelled >= 1, "至少取消被阻挡的迁入项");
    let db = Database::open(&task.directory).unwrap();
    let incoming = db
        .actions_page(0, 100)
        .unwrap()
        .into_iter()
        .find(|action| action.source == "来源/big.txt")
        .expect("存在迁入项");
    assert!(!incoming.selected, "原目标名称仍被保留，迁入项必须取消");
}

// 覆盖 C-17（移动目标互为源时使用临时暂存完成交换，且不覆盖任一源文件）。
#[test]
fn cyclic_moves_are_staged_without_losing_either_source() {
    let f = Fixture::new();
    f.write("a/one.txt", b"one", 10);
    f.write("b/two.txt", b"two", 20);
    let task = f.plan(base());
    let db = Database::open(&task.directory).unwrap();
    let moves: Vec<_> = db
        .actions_page(0, 100)
        .unwrap()
        .into_iter()
        .filter(|action| matches!(action.kind, ActionKind::Move))
        .collect();
    assert!(moves.len() >= 2, "需要两条归类移动构造交换环");
    let first = &moves[0];
    let second = &moves[1];
    let mut first_body = first.clone();
    let mut second_body = second.clone();
    first_body.target = Some(second.source.clone());
    second_body.target = Some(first.source.clone());
    let first_json = serde_json::to_string(&first_body).unwrap();
    let second_json = serde_json::to_string(&second_body).unwrap();
    db.conn
        .execute(
            "UPDATE actions SET target=?1, body=?2 WHERE id=?3",
            params![second.source, first_json, first.id],
        )
        .unwrap();
    db.conn
        .execute(
            "UPDATE actions SET target=?1, body=?2 WHERE id=?3",
            params![first.source, second_json, second.id],
        )
        .unwrap();
    drop(db);
    Fixture::apply(&task);
    assert_eq!(fs::read(f.root.join("a/one.txt")).unwrap(), b"two");
    assert_eq!(fs::read(f.root.join("b/two.txt")).unwrap(), b"one");
    assert!(!task.directory.join("move-stage").exists());
}

// 覆盖 H-03/C-05：用户原有的隐藏目录即使与旧暂存名相同，也属于默认扫描范围。
#[test]
fn user_directory_named_like_move_stage_is_scanned() {
    let f = Fixture::new();
    f.write(".jchtools-move-work/user.txt", b"user data", 10);
    let task = f.plan(base());
    let actions = Database::open(&task.directory)
        .unwrap()
        .actions_page(0, 100)
        .unwrap();
    assert!(
        actions
            .iter()
            .any(|a| a.source == ".jchtools-move-work/user.txt"),
        "用户原有隐藏目录不能因内部暂存名而被静默跳过：{actions:?}"
    );
}

// 覆盖 C-11
#[test]
fn plan_pagination_is_bounded() {
    let f = Fixture::new();
    for i in 0..260 {
        f.write(&format!("{i:04}.txt"), b"same", i + 100);
    }
    let task = f.plan(base());
    let db = Database::open(&task.directory).unwrap();
    let first = db.actions_page(0, 100).unwrap();
    let second = db.actions_page(first.last().unwrap().id, 100).unwrap();
    assert_eq!(first.len(), 100);
    assert_eq!(second.len(), 100);
    assert!(first.last().unwrap().id < second.first().unwrap().id);
    assert!(first.iter().all(|a| a.kind == ActionKind::Delete));
}

// ===== L5 actions_page_filtered 直接测 =====
// 覆盖 C-11
#[test]
fn actions_page_filtered_by_kind_and_rejects_unknown() {
    let f = Fixture::new();
    f.write("folder/a.pdf", b"pdf", 10);
    let cfg = base();
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].kind, ActionKind::Move);
    assert_eq!(
        moves[0].target.as_deref(),
        Some("文档/a.pdf"),
        "C-05：归类目标是「大类」一级（a.pdf → 文档）"
    );
    let deletes = db.actions_page_filtered(0, 100, Some("delete")).unwrap();
    assert!(deletes.is_empty(), "纯归类任务不应有删除动作");
    // folder/ 随归类被清空：空目录清理是强制步骤（C-07），行以 empty_directory 类型入计划。
    let empties = db
        .actions_page_filtered(0, 100, Some("empty_directory"))
        .unwrap();
    assert_eq!(empties.len(), 1);
    assert_eq!(empties[0].source, "folder");
    let all = db.actions_page(0, 100).unwrap();
    assert_eq!(all.len(), moves.len() + deletes.len() + empties.len());
    assert!(
        db.actions_page_filtered(0, 100, Some("unknown")).is_err(),
        "白名单外的 kind 必须直接报错"
    );
}
