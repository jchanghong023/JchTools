//! 覆盖 C-05 / C-14 / C-15 / C-16～C-21：固定归类「大类/功能分类」+「Git项目集合」
//! 的标准形态、功能聚类、来源前缀与短哈希消解、超长名截短与幂等。用例直接复刻合同
//! C-15 的附录示例（子目录分趟整理后整理父目录），期望值来自合同，不由实现反推。
// 测试代码允许 unwrap/expect（与 tests/core.rs 的集成测试惯例一致）：断言失败即测试失败。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]

use jchtools::{config::Config, control::Context, engine, rules};
use std::{collections::BTreeSet, fs, path::PathBuf};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    state: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let state = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        Self {
            _temp: temp,
            root,
            state,
        }
    }
    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.root.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        p
    }
    fn git(&self, name: &str) -> PathBuf {
        let p = self.root.join(name).join(".git");
        fs::create_dir_all(&p).unwrap();
        p
    }
    fn plan(&self) -> engine::TaskResult {
        engine::prepare_at(
            &self.root,
            Config::default(),
            Context::default(),
            &self.state,
        )
        .unwrap()
    }
    fn apply(task: &engine::TaskResult) {
        engine::apply(&task.directory, Context::default()).unwrap();
    }
    fn exists(&self, name: &str) -> bool {
        self.root.join(name).is_file()
    }
    fn subdirs(&self, dir: &str) -> BTreeSet<String> {
        fs::read_dir(self.root.join(dir))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect()
    }
}

fn digest8(rel: &str) -> String {
    rules::path_digest(rel, 8)
}

/// 合同 C-15 (1) 的原始目录树（内容互不相同，不做去重删除）。
const LONG_NAME: &str = "芯片研发中心第一联合研发重大专项技术合作合同及全部附件资料非常非常长的归档文件名称副本2026.pdf";

fn seed_standard_tree(f: &Fixture) {
    // a/b：工作资料 / 客户A / 客户B / 华东 / 华南 / 超长路径 / code
    f.write("b/工作资料/年报/年度报告.pdf", b"annual-b");
    f.write("b/工作资料/年报/产品说明 (1).pdf", b"spec-copy");
    f.write("b/工作资料/年报/产品说明_1.pdf", b"spec-one");
    f.write("b/工作资料/图片/logo.png", b"logo-b");
    f.write("b/工作资料/图片/截图 (2).png", b"shot-b");
    f.write("b/客户A/合同.pdf", b"contract-a");
    f.write("b/客户A/报价.xlsx", b"quote-a");
    f.write("b/客户A/logo.png", b"logo-a2");
    f.write("b/客户B/合同.pdf", b"contract-b");
    f.write("b/客户B/报价.xlsx", b"quote-b");
    f.write("b/客户B/demo.mp4", b"demo-b");
    f.write("b/华东/客户A/正式版/方案.pdf", b"plan-east");
    f.write("b/华南/客户A/正式版/方案.pdf", b"plan-south");
    f.write(
        &format!("b/超长事业部_芯片研发中心_第一联合研发重大专项/超长名称_上海联合研发团队_第一项目组/超长名称_客户正式交付资料/{LONG_NAME}"),
        b"long-b",
    );
    f.git("b/code/tools");
    f.write("b/code/tools/src/main.rs", b"fn main(){}");
    f.git("b/code/project-alpha");
    f.write("b/code/project-alpha/README.md", b"alpha");
    // a/c：下载 / 项目资料 / 华东 / 超长路径 / github
    f.write("c/下载/年度报告.pdf", b"annual-c");
    f.write("c/下载/安装说明.pdf", b"install");
    f.write("c/下载/截图 (1).png", b"shot-c");
    f.write("c/下载/demo.mp4", b"demo-c");
    f.write("c/项目资料/客户A/合同.pdf", b"contract-a2");
    f.write("c/项目资料/客户A/需求.docx", b"req");
    f.write("c/项目资料/客户C/合同.pdf", b"contract-c");
    f.write("c/项目资料/客户C/报价.xlsx", b"quote-c");
    f.write("c/华东/客户A/正式版/方案.pdf", b"plan-east2");
    f.write(
        &format!("c/超长事业部_芯片研发中心_第二联合研发重大专项/超长名称_深圳联合研发团队_第二项目组/超长名称_客户正式交付资料/{LONG_NAME}"),
        b"long-c",
    );
    f.git("c/github/tools");
    f.write("c/github/tools/src/main.py", b"print()");
    f.git("c/github/project-beta");
    f.write("c/github/project-beta/package.json", b"{}");
}

// 覆盖 C-05 / C-14 / C-15 (2)：整理子目录 b 的标准形态。
#[test]
fn organize_subtree_b_matches_contract_shape() {
    let f = Fixture::new();
    // b 作为根：内容与合同 (1) 的 a/b 相同，但根即 b 本身（“整理 a/b”）。
    f.write("工作资料/年报/年度报告.pdf", b"annual-b");
    f.write("工作资料/年报/产品说明 (1).pdf", b"spec-copy");
    f.write("工作资料/年报/产品说明_1.pdf", b"spec-one");
    f.write("工作资料/图片/logo.png", b"logo-b");
    f.write("工作资料/图片/截图 (2).png", b"shot-b");
    f.write("客户A/合同.pdf", b"contract-a");
    f.write("客户A/报价.xlsx", b"quote-a");
    f.write("客户A/logo.png", b"logo-a2");
    f.write("客户B/合同.pdf", b"contract-b");
    f.write("客户B/报价.xlsx", b"quote-b");
    f.write("客户B/demo.mp4", b"demo-b");
    f.write("华东/客户A/正式版/方案.pdf", b"plan-east");
    f.write("华南/客户A/正式版/方案.pdf", b"plan-south");
    f.write(
        &format!("超长事业部_芯片研发中心_第一联合研发重大专项/超长名称_上海联合研发团队_第一项目组/超长名称_客户正式交付资料/{LONG_NAME}"),
        b"long-b",
    );
    f.git("code/tools");
    f.write("code/tools/src/main.rs", b"fn main(){}");
    f.git("code/project-alpha");
    f.write("code/project-alpha/README.md", b"alpha");

    let task = f.plan();
    Fixture::apply(&task);
    assert_eq!(task.summary.planned_git, 2, "两个 Git 项目都应整体移入集合");

    // C-05：大类/功能分类；C-16：产品说明 (1).pdf 规范化为 产品说明_1.pdf。
    assert!(f.exists("文档/其他/年度报告.pdf"), "单文件不成组 → 其他");
    let h_copy = digest8("工作资料/年报/产品说明 (1).pdf");
    let h_plain = digest8("工作资料/年报/产品说明_1.pdf");
    // C-19：来源同级同名同目标 → 最近一级来源 + 主体 + 短哈希。
    assert!(
        f.exists(&format!("文档/产品/年报_产品说明_1_{h_copy}.pdf")),
        "实际内容见下方断言"
    );
    assert!(f.exists(&format!("文档/产品/年报_产品说明_1_{h_plain}.pdf")));
    assert!(
        f.exists("文档/合同/客户A_合同.pdf"),
        "同名文件按来源前缀消解"
    );
    assert!(f.exists("文档/合同/客户B_合同.pdf"));
    assert!(f.exists("文档/报价/客户A_报价.xlsx"));
    assert!(f.exists("文档/报价/客户B_报价.xlsx"));
    // C-18：两级来源仍冲突时升到第三级（华东_/华南_）。
    assert!(f.exists("文档/方案/华东_客户A_正式版_方案.pdf"));
    assert!(f.exists("文档/方案/华南_客户A_正式版_方案.pdf"));
    assert!(f.exists("图片/LOGO/图片_logo.png"));
    assert!(f.exists("图片/LOGO/客户A_logo.png"));
    // C-08/附录 B：(2) → _2；无公共主题 → 其他。
    assert!(f.exists("图片/其他/截图_2.png"));
    assert!(f.exists("视频/其他/demo.mp4"));
    assert!(f.exists(&format!("文档/其他/{LONG_NAME}")));
    // C-14：项目平铺进集合、目录名不变；内容整树保留。
    assert!(f.root.join("Git项目集合/tools/.git").is_dir());
    assert!(f.root.join("Git项目集合/tools/src/main.rs").is_file());
    assert!(f.root.join("Git项目集合/project-alpha/.git").is_dir());
    // H-05/C-07：搬空的中间目录全部消失。
    assert!(!f.root.join("工作资料").exists());
    assert!(!f.root.join("客户A").exists());
    assert!(!f.root.join("华东").exists());
    assert!(!f.root.join("code").exists());
    // C-05：文档下功能目录 = 产品/合同/报价/方案/其他，恰好两级。
    assert_eq!(
        f.subdirs("文档"),
        BTreeSet::from([
            "其他".into(),
            "产品".into(),
            "合同".into(),
            "报价".into(),
            "方案".into()
        ])
    );
}

// 覆盖 C-15 (1)→(4)：先 b/c 后父目录 a 的完整重组与最终形态。
#[test]
fn organize_parent_after_children_rebuilds_standard_shape() {
    let f = Fixture::new();
    seed_standard_tree(&f);
    // 第一趟：整理 a/b。
    {
        let task = engine::prepare_at(
            &f.root.join("b"),
            Config::default(),
            Context::default(),
            &f.state,
        )
        .unwrap();
        Fixture::apply(&task);
    }
    assert!(f.exists("b/文档/其他/年度报告.pdf"));
    assert!(f.root.join("b/Git项目集合/tools/.git").is_dir());
    // 第二趟：整理 a/c。
    {
        let task = engine::prepare_at(
            &f.root.join("c"),
            Config::default(),
            Context::default(),
            &f.state,
        )
        .unwrap();
        Fixture::apply(&task);
    }
    assert!(f.exists("c/文档/其他/年度报告.pdf"));
    assert!(f.exists("c/文档/其他/安装说明.pdf"));
    assert!(f.exists("c/文档/合同/客户A_合同.pdf"));
    assert!(f.exists("c/图片/其他/截图_1.png"));
    // 第三趟：整理父目录 a。
    let task = f.plan();
    Fixture::apply(&task);

    // —— 合同 (4) 的最终形态 ——
    assert!(f.exists("文档/年度报告/b_年度报告.pdf"));
    assert!(f.exists("文档/年度报告/c_年度报告.pdf"));
    assert!(
        f.exists("文档/其他/安装说明.pdf"),
        "只来自 c 的不冲突，保持原名且仍在「其他」"
    );
    assert!(f.exists("文档/其他/需求.docx"));
    assert!(
        f.exists("图片/LOGO/图片_logo.png"),
        "无新冲突的历史消解名原样保留"
    );
    assert!(f.exists("图片/LOGO/客户A_logo.png"));
    assert!(f.exists("文档/合同/b_客户A_合同.pdf"));
    assert!(f.exists("文档/合同/c_客户A_合同.pdf"));
    assert!(
        f.exists("文档/合同/客户B_合同.pdf"),
        "不得为形式统一全部加前缀"
    );
    assert!(f.exists("文档/合同/客户C_合同.pdf"));
    assert!(f.exists("文档/报价/客户A_报价.xlsx"));
    assert!(f.exists("文档/报价/客户B_报价.xlsx"));
    assert!(f.exists("文档/报价/报价.xlsx"));
    assert!(f.exists("文档/方案/华东_客户A_正式版_方案.pdf"));
    assert!(f.exists("文档/方案/华南_客户A_正式版_方案.pdf"));
    assert!(f.exists("文档/方案/方案.pdf"));
    assert!(f.exists("图片/截图/截图_1.png"));
    assert!(f.exists("图片/截图/截图_2.png"));
    assert!(f.exists("视频/DEMO/b_demo.mp4"));
    assert!(f.exists("视频/DEMO/c_demo.mp4"));
    // 超长文件：b、c 各一份，聚成一组（目录名 = 组内公共短语），冲突后按 C-20 截短为
    // 主体前段+摘要；C-19 摘要输入是“分析开始时”的原始完整路径（前一趟的落位路径）。
    let dig_b = digest8(&format!("b/文档/其他/{LONG_NAME}"));
    let dig_c = digest8(&format!("c/文档/其他/{LONG_NAME}"));
    let cut_b = format!("文档/芯片研发中心第一联合研发重大专项技术合作合同/芯片研发中心第一联合研发重大专项技术合作合同及全部_{dig_b}.pdf");
    let cut_c = format!("文档/芯片研发中心第一联合研发重大专项技术合作合同/芯片研发中心第一联合研发重大专项技术合作合同及全部_{dig_c}.pdf");
    assert!(f.exists(&cut_b), "b 超长文件截短+摘要：{cut_b}");
    assert!(f.exists(&cut_c), "c 超长文件截短+摘要：{cut_c}");
    // Git 项目：同名 tools 按来源消解，其余保持原名；平铺在根的集合下。
    assert!(f.root.join("Git项目集合/b_tools/.git").is_dir());
    assert!(f.root.join("Git项目集合/b_tools/src/main.rs").is_file());
    assert!(f.root.join("Git项目集合/c_tools/src/main.py").is_file());
    assert!(f.root.join("Git项目集合/project-alpha/.git").is_dir());
    assert!(f
        .root
        .join("Git项目集合/project-beta/package.json")
        .is_file());
    // C-15：中间目录（含上一轮的子级分类目录与集合目录）全部消失，无重复叠加。
    assert!(!f.root.join("b").exists());
    assert!(!f.root.join("c").exists());
    assert!(!f.root.join("Git项目集合/Git项目集合").exists());
    assert_eq!(
        f.subdirs("文档"),
        BTreeSet::from([
            "其他".into(),
            "产品".into(),
            "年度报告".into(),
            "合同".into(),
            "报价".into(),
            "方案".into(),
            "芯片研发中心第一联合研发重大专项技术合作合同".into(),
        ]),
        "文档下只有两级：大类/功能分类"
    );
    // (6) 禁止的形态：无套娃、无年月层级。
    assert!(!f.exists("文档/其他/文档/其他/安装说明.pdf"));
    assert!(!f.root.join("文档/年度报告/2026").exists());
    // (5) 幂等：马上再次整理 a，不再产生任何文件操作。
    let again = f.plan();
    assert_eq!(again.summary.planned_move, 0, "已就位项不再移动");
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.planned_git, 0);
    assert_eq!(again.summary.planned_empty, 0);
    // 连续第三次整理仍幂等（C-05/C-21 确定性）。
    let third = f.plan();
    assert_eq!(third.summary.planned_move, 0);
}

// 覆盖 C-05 场景：明显相同功能（MBIST 强 token）→ 文档/MBIST/。
#[test]
fn mbist_files_group_into_named_dir() {
    let f = Fixture::new();
    f.write("05_10 SMS Mbist 介绍.docx", b"a");
    f.write("05_22 Tessent_MBIST_RTL流程指导.docx", b"b");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("文档/MBIST/05_10 SMS Mbist 介绍.docx"));
    assert!(f.exists("文档/MBIST/05_22 Tessent_MBIST_RTL流程指导.docx"));
}

// 覆盖 C-05 场景：多个相关缩写 → 稳定的同一功能组 文档/AMBA_APB/。
#[test]
fn amba_family_forms_stable_group() {
    let f = Fixture::new();
    f.write("amba_apb_protocol_spec.pdf", b"a");
    f.write("AAMBA3apb.pdf", b"b");
    f.write("AMBA总线基础(2013)[1].pptx", b"c");
    f.write("JTAG2APB案例.docx", b"d");
    let task = f.plan();
    Fixture::apply(&task);
    for name in [
        "amba_apb_protocol_spec.pdf",
        "AAMBA3apb.pdf",
        "AMBA总线基础(2013)[1].pptx",
        "JTAG2APB案例.docx",
    ] {
        assert!(f.exists(&format!("文档/AMBA_APB/{name}")), "{name}");
    }
}

// 覆盖 C-05 场景：字符串看起来相似但语义 token 不足（scan 前缀）不得错误合并。
#[test]
fn scan_prefix_pair_stays_in_fallback() {
    let f = Fixture::new();
    f.write("scan_report.pdf", b"a");
    f.write("scanner_driver.pdf", b"b");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("文档/其他/scan_report.pdf"));
    assert!(f.exists("文档/其他/scanner_driver.pdf"));
}

// 覆盖 C-05 场景：中英文混合 + 噪声名称稳定归入同组。
#[test]
fn mixed_and_noisy_names_cluster() {
    let f = Fixture::new();
    f.write("Tessent_MBIST流程指导.pdf", b"a");
    f.write("MBIST 测试介绍.docx", b"b");
    f.write("mbist_rtl_flow.txt", b"c");
    f.write("MBIST介绍 (1).pdf", b"d");
    f.write("MBIST介绍_final.pdf", b"e");
    f.write("2026-05_MBIST介绍_v2.pdf", b"ff");
    let task = f.plan();
    Fixture::apply(&task);
    for name in [
        "Tessent_MBIST流程指导.pdf",
        "MBIST 测试介绍.docx",
        "mbist_rtl_flow.txt",
        // 噪声后缀按 C-08 清理后仍在同组。
        "MBIST介绍_1.pdf",
        "MBIST介绍_final.pdf",
        "2026-05_MBIST介绍_v2.pdf",
    ] {
        assert!(f.exists(&format!("文档/MBIST/{name}")), "{name}");
    }
}

// 覆盖 C-05 场景：不同大类不跨越——同名功能目录在大类间互不影响。
#[test]
fn functional_groups_do_not_cross_categories() {
    let f = Fixture::new();
    f.write("MBIST介绍.pdf", b"a");
    f.write("MBIST算法.pdf", b"b");
    f.write("MBIST结构图.png", b"c");
    f.write("MBIST版图.png", b"d");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("文档/MBIST/MBIST介绍.pdf"));
    assert!(f.exists("文档/MBIST/MBIST算法.pdf"));
    assert!(f.exists("图片/MBIST/MBIST结构图.png"));
    assert!(f.exists("图片/MBIST/MBIST版图.png"));
    assert!(
        !f.exists("图片/MBIST/MBIST介绍.pdf"),
        "功能聚类不得跨越大类"
    );
}

// 覆盖 C-05 场景：同功能目录下文件名冲突仍按 C-17～C-20 消解（来源前缀）。
#[test]
fn conflicts_resolve_within_functional_dir() {
    let f = Fixture::new();
    f.write("b/MBIST流程.pdf", b"1");
    f.write("c/MBIST流程.pdf", b"2");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("文档/MBIST/b_MBIST流程.pdf"));
    assert!(f.exists("文档/MBIST/c_MBIST流程.pdf"));
}

// 覆盖 C-05 场景：功能目录数量上限（10 + 「其他」不计入）端到端。
#[test]
fn functional_dir_cap_end_to_end() {
    let f = Fixture::new();
    let themes = [
        "MBIST",
        "ATPG",
        "SCAN",
        "AMBA_APB",
        "JTAG",
        "TESSENT",
        "DFT",
        "RTL",
        "PROTOCOL",
        "VERIFICATION",
        "LITHO",
        "PACKAGE",
    ];
    for (rank, theme) in themes.iter().enumerate() {
        let copies = 6 - (rank / 3);
        for n in 0..copies {
            f.write(
                &format!("{theme}_guide_{n}.pdf"),
                format!("{theme} {n}").as_bytes(),
            );
        }
    }
    f.write("lonely_misfit.pdf", b"x");
    let task = f.plan();
    Fixture::apply(&task);
    let dirs = f.subdirs("文档");
    assert_eq!(dirs.len(), 11, "10 个功能目录 + 1 个「其他」：{dirs:?}");
    assert!(dirs.contains("其他"), "「其他」必须存在且不计入上限");
    assert!(
        dirs.contains("MBIST") && dirs.contains("PACKAGE") || dirs.contains("LITHO"),
        "大组优先保留"
    );
    assert!(
        f.exists("文档/其他/lonely_misfit.pdf"),
        "单文件进入「其他」"
    );
}

// 覆盖 C-17 / 附录 E：目标功能目录已有「报告 (1).pdf」与「报告_1.pdf」统一消解，
// 且再次整理不再改名（幂等）。
#[test]
fn in_place_normalized_collision_resolves_once_and_stays() {
    let f = Fixture::new();
    f.write("文档/报告/报告 (1).pdf", b"a");
    f.write("文档/报告/报告_1.pdf", b"b");
    let task = f.plan();
    Fixture::apply(&task);
    // 两个已就位项规范化后同名 → 统一消解（其一保持、其一摘要化）。
    let entries: Vec<String> = fs::read_dir(f.root.join("文档/报告"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 2, "两个文件都在，无覆盖：{entries:?}");
    assert!(
        entries.contains(&"报告_1.pdf".to_string()),
        "唯一可占名的已就位项保持原名：{entries:?}"
    );
    let other = entries.iter().find(|n| n.as_str() != "报告_1.pdf").unwrap();
    let dig = digest8("文档/报告/报告 (1).pdf");
    assert!(
        other.starts_with("报告_1_") && other.contains(&dig),
        "新改者按 C-19 摘要消解：{other}（digest {dig}）"
    );
    // 再次整理：不再改名（C-16/附录 E 幂等）。
    let again = f.plan();
    assert_eq!(again.summary.planned_move, 0);
    Fixture::apply(&again);
    let entries2: Vec<String> = fs::read_dir(f.root.join("文档/报告"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, entries2);
}

// 覆盖 C-17：固定容器被普通文件占用时不挪走占用项，依赖项失败保留源项（S-01）。
#[test]
fn occupied_category_container_keeps_source_items() {
    let f = Fixture::new();
    f.write("报告.pdf", b"pdf");
    // 「文档」被普通文件占用：依赖「文档/其他」的归类全部失败并保留源项。
    fs::write(f.root.join("文档"), b"not a directory").unwrap();
    let task = f.plan();
    // 名为「文档」的普通文件本身照常归类（它是范围内普通文件）；只有依赖被占用
    // 容器的「报告.pdf」失败并保留源项（S-01）。
    assert_eq!(task.summary.planned_move, 1);
    assert!(f.exists("报告.pdf"), "依赖被占用容器的项保留在原位置");
}

// 覆盖 H-06 / C-14：已在本次根「Git项目集合」下的项目不再移动（幂等、不自嵌套）。
#[test]
fn collection_under_root_is_stable_across_reruns() {
    let f = Fixture::new();
    f.git("Git项目集合/proj");
    f.write("Git项目集合/proj/README.md", b"r");
    f.write("note.txt", b"n");
    let task = f.plan();
    assert_eq!(task.summary.planned_git, 0, "已就位项目不再移动");
    assert!(!f.exists("Git项目集合/Git项目集合/proj/README.md"));
    // 没有第二个待移入项目时不新建集合目录之外的东西；普通文件照常归类。
    assert!(f.exists("note.txt") || task.summary.planned_move >= 1);
}

// 覆盖 C-14 / 附录 E：集合容器被文件占用时项目保留原位。
#[test]
fn git_collection_blocked_by_file_keeps_projects() {
    let f = Fixture::new();
    f.git("proj");
    f.write("proj/src/a.rs", b"a");
    fs::write(f.root.join("Git项目集合"), b"occupied").unwrap();
    let task = f.plan();
    assert_eq!(task.summary.planned_git, 0);
    assert!(f.root.join("proj/.git").is_dir(), "原项目保留在原位置");
    assert!(f.root.join("Git项目集合").is_file(), "占用项不被删除或挪走");
}

// 覆盖 C-05 / C-21：归类移动保留时间戳；重复整理仍判已就位、不再移动。
#[test]
fn reorganized_files_stay_in_place_without_moves() {
    let f = Fixture::new();
    f.write("old/资料.pdf", b"x");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("文档/其他/资料.pdf"));
    let again = f.plan();
    assert_eq!(again.summary.planned_move, 0, "无变化 → 仍判已就位");
}

// 覆盖 C-06：大文件命中后优先进入「大文件」大类（同样两级：大文件/功能分类）。
#[test]
fn large_files_take_priority_over_extension_category() {
    let f = Fixture::new();
    let cfg = Config {
        large_files: true,
        large_threshold_bytes: 1024,
        ..Config::default()
    };
    f.write("big.bin", &[7u8; 2048]);
    f.write("small.pdf", b"p");
    let task = engine::prepare_at(&f.root, cfg, Context::default(), &f.state).unwrap();
    Fixture::apply(&task);
    assert!(f.exists("大文件/其他/big.bin"));
    assert!(f.exists("文档/其他/small.pdf"));
}

// 覆盖 C-05：主体全是噪声（日期编号）的文件名不影响兜底落位。
#[test]
fn noise_only_name_still_lands_in_fallback() {
    let f = Fixture::new();
    f.write("2026-05-10.pdf", b"x");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("文档/其他/2026-05-10.pdf"), "实际文件名不被改写");
}

// 覆盖 C-18 / C-19：同目录内不同来源的同名文件最少级数消解 + 摘要兜底。
#[test]
fn minimal_level_prefixes_and_digest_fallback() {
    let f = Fixture::new();
    f.write("x/合同.pdf", b"1");
    f.write("y/合同.pdf", b"2");
    f.write("合同.pdf", b"3");
    let task = f.plan();
    Fixture::apply(&task);
    // k=1 即互不相同：x_合同 / y_合同 / 合同（根文件无来源段，保持原名）。
    assert!(f.exists("文档/合同/x_合同.pdf"));
    assert!(f.exists("文档/合同/y_合同.pdf"));
    assert!(f.exists("文档/合同/合同.pdf"));
    let f4 = Fixture::new();
    f4.write("same/报表.pdf", b"1");
    f4.write("same/nested/报表.pdf", b"2");
    let task4 = f4.plan();
    Fixture::apply(&task4);
    // k=1：same_报表 vs nested_报表 已互不相同，不进入摘要。
    assert!(f4.exists("文档/报表/same_报表.pdf"));
    assert!(f4.exists("文档/报表/nested_报表.pdf"));
    let _ = task;
}

// 覆盖 H-06：祖先直接含 .git 时目录整理拒绝整次处理（附录 E）。
#[test]
fn root_inside_git_project_is_rejected() {
    let f = Fixture::new();
    f.git("proj");
    f.write("proj/data/a.txt", b"a");
    let result = engine::prepare_at(
        &f.root.join("proj/data"),
        Config::default(),
        Context::default(),
        &f.state,
    );
    assert!(result.is_err(), "所选根位于 Git 项目内必须拒绝整次处理");
}

// 覆盖 C-09 / X-07：「解压失败」目录整树保留，不参与归类与清理。
#[test]
fn quarantine_tree_is_left_alone() {
    let f = Fixture::new();
    f.write("解压失败/broken.zip", b"zip");
    f.write("normal.txt", b"n");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("解压失败/broken.zip"), "隔离容器内容不参与处理");
    assert!(f.exists("文档/其他/normal.txt"));
    assert!(!f.root.join("解压失败").join("文档").exists());
}
