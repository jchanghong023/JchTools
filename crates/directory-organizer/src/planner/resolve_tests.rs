use super::resolve::{resolve_all, Item, NameParts};
use jchtools_file_core::{config::Config, control::Context, rules, task::create_task_db};

/// 构造直驱 `resolve_all` 的最小任务：临时状态目录建任务库，根目录只放置
/// 本测试显式写入的固定占用文件；Item 手工拼装以构造真实扫描不可能出现的
/// 输入（生产表结构保证 rel 唯一：files.rel UNIQUE、git_roots 主键）。
fn resolve_job() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    jchtools_file_core::fsutil::RootGuard,
    jchtools_file_core::task::Job,
) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let (guard, job) = create_task_db(
        state.path(),
        root.path(),
        Config::default(),
        Context::default(),
        "analyzing",
    )
    .unwrap();
    (state, root, guard, job)
}

/// 与 resolve 相同口径的 C-19/C-20 摘要候选名（无来源段，走 rules 公共实现）。
fn digest_name(rel: &str, stem: &str, hex_units: usize, index: Option<u32>) -> String {
    rules::digest_candidate(
        None,
        stem,
        &rules::path_digest(rel, hex_units),
        ".pdf",
        index,
        false,
    )
    .unwrap()
}

fn contract_parts() -> NameParts {
    NameParts {
        current_name: "合同.pdf".into(),
        stem: "合同".into(),
        extension: ".pdf".into(),
    }
}

// 覆盖 C-19：完整摘要仍冲突时，活动项按附录 B 原路径稳定升序分配各自最小可用
// 序号，避开所有保留项和已分配候选，不无限循环。生产输入的 rel 唯一，本测试
// 直接构造重复摘要输入（同 rel 三项），验证极端兜底在该前提下仍有限收敛、
// 不产生同名目标（S-01 不覆盖）。
#[test]
fn c19_full_digest_conflict_assigns_minimal_suffixes_per_path_order() {
    let (_state, root, _guard, job) = resolve_job();
    let documents = root.path().join("文档");
    std::fs::create_dir_all(&documents).unwrap();
    // 固定占用恰好挡住摘要 +_1 名：先到者应取 _2，其后依次 _3、_4。
    std::fs::write(
        documents.join(digest_name("合同.pdf", "合同", 64, Some(1))),
        b"x",
    )
    .unwrap();
    let mut items = vec![
        Item::new(1, "合同.pdf".into(), contract_parts(), "文档".into(), vec![], false),
        Item::new(2, "合同.pdf".into(), contract_parts(), "文档".into(), vec![], false),
        Item::new(3, "合同.pdf".into(), contract_parts(), "文档".into(), vec![], false),
    ];
    resolve_all(&mut items, &job, &[], None);
    for (i, item) in items.iter().enumerate() {
        assert!(item.failed.is_none(), "第 {i} 项不应失败：{:?}", item.failed);
    }
    let mut names: Vec<String> = items.iter().map(|item| item.candidate.clone()).collect();
    names.sort();
    let expected: Vec<String> = [2u32, 3, 4]
        .iter()
        .map(|&n| digest_name("合同.pdf", "合同", 64, Some(n)))
        .collect();
    assert_eq!(
        names, expected,
        "碰撞项必须各得其最小可用稳定序号；同步递增会产生同名目标"
    );
}

// 覆盖 C-19：单项沿摘要阶梯逐级被固定占用阻挡到 64 位后，取最小可用序号
// （行为保持检查：既有增量语义与新分配语义在单项场景结果一致）。
#[test]
fn c19_single_item_full_digest_conflict_takes_minimal_suffix() {
    let (_state, root, _guard, job) = resolve_job();
    let documents = root.path().join("文档");
    std::fs::create_dir_all(&documents).unwrap();
    std::fs::write(documents.join("合同.pdf"), b"x").unwrap();
    for step in 1..=29u32 {
        let hex_units = (6 + 2 * step).min(64) as usize;
        std::fs::write(
            documents.join(digest_name("合同.pdf", "合同", hex_units, None)),
            b"x",
        )
        .unwrap();
    }
    std::fs::write(
        documents.join(digest_name("合同.pdf", "合同", 64, Some(1))),
        b"x",
    )
    .unwrap();
    let mut items = vec![Item::new(
        7,
        "合同.pdf".into(),
        contract_parts(),
        "文档".into(),
        vec![],
        false,
    )];
    resolve_all(&mut items, &job, &[], None);
    assert!(items[0].failed.is_none(), "不应失败：{:?}", items[0].failed);
    assert_eq!(
        items[0].candidate,
        digest_name("合同.pdf", "合同", 64, Some(2)),
        "序号 _1 被固定占用挡住时应取最小可用的 _2"
    );
}
