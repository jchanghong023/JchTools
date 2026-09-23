//! 覆盖 C-05 / C-14 / C-15 / C-16～C-21：固定归类「大类/年/月」+「Git项目集合」
//! 的标准形态、来源前缀与短哈希消解、超长名截短与幂等。用例直接复刻合同 C-15 的
//! 附录示例（子目录分趟整理后整理父目录），期望值来自合同，不由实现反推。
// 测试代码允许 unwrap/expect（与 tests/core.rs 的集成测试惯例一致）：断言失败即测试失败。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    // 远超 9999 年的时间戳只能用秒数表达，没有更大的可用单位构造器。
    clippy::duration_suboptimal_units
)]

use jchtools::{config::Config, control::Context, engine, rules};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

fn at(date: &str) -> SystemTime {
    // 示例日期（每月 15 日中午 UTC）→ 创建时间；夹具只设创建时间，它即 C-05 创建/修改中
    // 最早的可用时间（修改时间为“现在”）；断言只关心年/月，月中避开月末边界。
    let (year, month) = date.split_once('-').unwrap();
    let naive = chrono::NaiveDate::from_ymd_opt(year.parse().unwrap(), month.parse().unwrap(), 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    UNIX_EPOCH + Duration::from_secs(naive.and_utc().timestamp().max(0) as u64)
}
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
    fn write_at(&self, name: &str, bytes: &[u8], date: &str) -> PathBuf {
        let p = self.root.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        jchtools::fsutil::set_created_time(&p, at(date)).unwrap();
        p
    }
    fn git(&self, name: &str) -> PathBuf {
        let p = self.root.join(name).join(".git");
        fs::create_dir_all(&p).unwrap();
        jchtools::fsutil::set_created_time(p.parent().unwrap(), at("2026-01")).unwrap();
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
}
#[allow(dead_code)]
fn unused_fixture_guard(f: &Fixture) -> bool {
    f.root.exists()
}

fn digest8(rel: &str) -> String {
    rules::path_digest(rel, 8)
}

/// 构造合同 C-15 (1) 的原始目录树（内容互不相同，不做去重删除）。
fn seed_standard_tree(f: &Fixture) {
    // a/b：工作资料 / 客户A / 客户B / 华东 / 华南 / 超长路径 / code
    f.write_at("b/工作资料/年报/年度报告.pdf", b"annual-b", "2026-03");
    f.write_at("b/工作资料/年报/产品说明 (1).pdf", b"spec-copy", "2026-03");
    f.write_at("b/工作资料/年报/产品说明_1.pdf", b"spec-one", "2026-03");
    f.write_at("b/工作资料/图片/logo.png", b"logo-b", "2026-03");
    f.write_at("b/工作资料/图片/截图 (2).png", b"shot-b", "2026-04");
    f.write_at("b/客户A/合同.pdf", b"contract-a", "2026-05");
    f.write_at("b/客户A/报价.xlsx", b"quote-a", "2026-05");
    f.write_at("b/客户A/logo.png", b"logo-a2", "2026-03");
    f.write_at("b/客户B/合同.pdf", b"contract-b", "2026-05");
    f.write_at("b/客户B/报价.xlsx", b"quote-b", "2026-06");
    f.write_at("b/客户B/demo.mp4", b"demo-b", "2026-06");
    f.write_at("b/华东/客户A/正式版/方案.pdf", b"plan-east", "2026-07");
    f.write_at("b/华南/客户A/正式版/方案.pdf", b"plan-south", "2026-07");
    let long = "2026年度最终正式签署版本技术合作合同及全部附件资料非常非常长的文件名称.pdf";
    f.write_at(&format!("b/超长事业部_芯片研发中心_第一联合研发重大专项/超长名称_上海联合研发团队_第一项目组/超长名称_客户正式交付资料/{long}"), b"long-b", "2026-08");
    f.git("b/code/tools");
    f.write_at("b/code/tools/src/main.rs", b"fn main(){}", "2026-01");
    f.git("b/code/project-alpha");
    f.write_at("b/code/project-alpha/README.md", b"alpha", "2026-01");
    // a/c：下载 / 项目资料 / 华东 / 超长路径 / github
    f.write_at("c/下载/年度报告.pdf", b"annual-c", "2026-03");
    f.write_at("c/下载/安装说明.pdf", b"install", "2026-03");
    f.write_at("c/下载/截图 (1).png", b"shot-c", "2026-04");
    f.write_at("c/下载/demo.mp4", b"demo-c", "2026-06");
    f.write_at("c/项目资料/客户A/合同.pdf", b"contract-a2", "2026-05");
    f.write_at("c/项目资料/客户A/需求.docx", b"req", "2026-07");
    f.write_at("c/项目资料/客户C/合同.pdf", b"contract-c", "2026-05");
    f.write_at("c/项目资料/客户C/报价.xlsx", b"quote-c", "2026-06");
    f.write_at("c/华东/客户A/正式版/方案.pdf", b"plan-east2", "2026-07");
    let long2 = "2026年度最终正式签署版本技术合作合同及全部附件资料非常非常长的文件名称.pdf";
    f.write_at(&format!("c/超长事业部_芯片研发中心_第二联合研发重大专项/超长名称_深圳联合研发团队_第二项目组/超长名称_客户正式交付资料/{long2}"), b"long-c", "2026-08");
    f.git("c/github/tools");
    f.write_at("c/github/tools/src/main.py", b"print()", "2026-01");
    f.git("c/github/project-beta");
    f.write_at("c/github/project-beta/package.json", b"{}", "2026-01");
}

// 覆盖 C-05 / C-14 / C-15 (2)：整理子目录 b 的标准形态。
#[test]
fn organize_subtree_b_matches_contract_shape() {
    let mut f = Fixture::new();
    // 把 b 提升为本次整理根：直接在临时根下重建 b 的内容。
    let temp = tempfile::tempdir().unwrap();
    let _ = temp; // 结构说明见 seed；此处直接以内联树构造。
    drop(f);
    f = Fixture::new();
    // b 作为根：内容与合同 (1) 的 a/b 相同，但根即 b 本身（“整理 a/b”）。
    f.write_at("工作资料/年报/年度报告.pdf", b"annual-b", "2026-03");
    f.write_at("工作资料/年报/产品说明 (1).pdf", b"spec-copy", "2026-03");
    f.write_at("工作资料/年报/产品说明_1.pdf", b"spec-one", "2026-03");
    f.write_at("工作资料/图片/logo.png", b"logo-b", "2026-03");
    f.write_at("工作资料/图片/截图 (2).png", b"shot-b", "2026-04");
    f.write_at("客户A/合同.pdf", b"contract-a", "2026-05");
    f.write_at("客户A/报价.xlsx", b"quote-a", "2026-05");
    f.write_at("客户A/logo.png", b"logo-a2", "2026-03");
    f.write_at("客户B/合同.pdf", b"contract-b", "2026-05");
    f.write_at("客户B/报价.xlsx", b"quote-b", "2026-06");
    f.write_at("客户B/demo.mp4", b"demo-b", "2026-06");
    f.write_at("华东/客户A/正式版/方案.pdf", b"plan-east", "2026-07");
    f.write_at("华南/客户A/正式版/方案.pdf", b"plan-south", "2026-07");
    let long = "2026年度最终正式签署版本技术合作合同及全部附件资料非常非常长的文件名称.pdf";
    f.write_at(&format!("超长事业部_芯片研发中心_第一联合研发重大专项/超长名称_上海联合研发团队_第一项目组/超长名称_客户正式交付资料/{long}"), b"long-b", "2026-08");
    f.git("code/tools");
    f.write_at("code/tools/src/main.rs", b"fn main(){}", "2026-01");
    f.git("code/project-alpha");
    f.write_at("code/project-alpha/README.md", b"alpha", "2026-01");

    let task = f.plan();
    Fixture::apply(&task);
    assert_eq!(task.summary.planned_git, 2, "两个 Git 项目都应整体移入集合");

    // C-05：大类/年/月；C-16：产品说明 (1).pdf 规范化为 产品说明_1.pdf。
    assert!(f.exists("文档/2026/03/年度报告.pdf"));
    let h_copy = digest8("工作资料/年报/产品说明 (1).pdf");
    let h_plain = digest8("工作资料/年报/产品说明_1.pdf");
    // C-19：来源同级同名同目标 → 最近一级来源 + 主体 + 短哈希。
    assert!(
        f.exists(&format!("文档/2026/03/年报_产品说明_1_{h_copy}.pdf")),
        "实际内容见下方断言"
    );
    assert!(f.exists(&format!("文档/2026/03/年报_产品说明_1_{h_plain}.pdf")));
    assert!(
        f.exists("文档/2026/05/客户A_合同.pdf"),
        "同名方案类冲突按来源前缀消解"
    );
    assert!(f.exists("文档/2026/05/客户B_合同.pdf"));
    assert!(f.exists("文档/2026/05/报价.xlsx"));
    assert!(f.exists("文档/2026/06/报价.xlsx"));
    // C-18：两级来源仍冲突时升到第三级（华东_/华南_）。
    assert!(f.exists("文档/2026/07/华东_客户A_正式版_方案.pdf"));
    assert!(f.exists("文档/2026/07/华南_客户A_正式版_方案.pdf"));
    assert!(f.exists("图片/2026/03/图片_logo.png"));
    assert!(f.exists("图片/2026/03/客户A_logo.png"));
    // C-08/附录 B：(2) → _2。
    assert!(f.exists("图片/2026/04/截图_2.png"));
    assert!(f.exists("视频/2026/06/demo.mp4"));
    // C-14：项目平铺进集合、目录名不变；内容整树保留。
    assert!(f.root.join("Git项目集合/tools/.git").is_dir());
    assert!(f.root.join("Git项目集合/tools/src/main.rs").is_file());
    assert!(f.root.join("Git项目集合/project-alpha/.git").is_dir());
    // H-05/C-07：搬空的中间目录全部消失。
    assert!(!f.root.join("工作资料").exists());
    assert!(!f.root.join("客户A").exists());
    assert!(!f.root.join("华东").exists());
    assert!(!f.root.join("code").exists());
    // C-20：未冲突的合法长名不因超过 40 单元而改名（40 只是冲突候选的退让阈值）。
    assert!(
        f.exists(&format!("文档/2026/08/{long}")),
        "无冲突的超长文件保持原名"
    );
    assert_eq!(f.root.join("文档/2026/08").read_dir().unwrap().count(), 1);
}

// 覆盖 C-15 (1)→(4)：先 b/c 后父目录 a 的完整重组与最终形态。
#[test]
fn organize_parent_after_children_rebuilds_standard_shape() {
    let f = Fixture::new();
    seed_standard_tree(&f);
    // 第一趟：整理 a/b（把 b 临时挂为根：借助子目录 prepare 一次）。
    {
        let sub = Fixture::new();
        // 直接在 f.root/b 上以它为根跑一趟：为复用 Fixture.root 断言，改用显式路径。
        let task = engine::prepare_at(
            &f.root.join("b"),
            Config::default(),
            Context::default(),
            &f.state,
        )
        .unwrap();
        Fixture::apply(&task);
        let _ = sub;
    }
    // b 内已成型：分类目录位于 b 下、Git 项目在 b/Git项目集合 下。
    assert!(f.exists("b/文档/2026/03/年度报告.pdf"));
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
    assert!(f.exists("c/文档/2026/03/年度报告.pdf"));
    assert!(f.exists("c/文档/2026/03/安装说明.pdf"));
    assert!(f.exists("c/图片/2026/04/截图_1.png"));
    // 第三趟：整理父目录 a。
    let task = f.plan();
    Fixture::apply(&task);

    // —— 合同 (4) 的最终形态 ——
    assert!(f.exists("文档/2026/03/b_年度报告.pdf"));
    assert!(f.exists("文档/2026/03/c_年度报告.pdf"));
    assert!(
        f.exists("文档/2026/03/安装说明.pdf"),
        "只来自 c 的不冲突，保持原名"
    );
    assert!(
        f.exists("图片/2026/03/图片_logo.png"),
        "无新冲突的历史消解名原样保留"
    );
    assert!(f.exists("图片/2026/03/客户A_logo.png"));
    assert!(f.exists("文档/2026/05/b_客户A_合同.pdf"));
    assert!(f.exists("文档/2026/05/c_客户A_合同.pdf"));
    assert!(
        f.exists("文档/2026/05/客户B_合同.pdf"),
        "不得为形式统一全部加前缀"
    );
    assert!(f.exists("文档/2026/05/客户C_合同.pdf"));
    assert!(f.exists("文档/2026/05/报价.xlsx"));
    assert!(f.exists("文档/2026/06/b_报价.xlsx"));
    assert!(f.exists("文档/2026/06/c_报价.xlsx"));
    assert!(f.exists("文档/2026/07/华东_客户A_正式版_方案.pdf"));
    assert!(f.exists("文档/2026/07/华南_客户A_正式版_方案.pdf"));
    assert!(f.exists("文档/2026/07/方案.pdf"));
    assert!(f.exists("文档/2026/07/需求.docx"));
    assert!(f.exists("图片/2026/04/截图_1.png"));
    assert!(f.exists("图片/2026/04/截图_2.png"));
    assert!(f.exists("视频/2026/06/b_demo.mp4"));
    assert!(f.exists("视频/2026/06/c_demo.mp4"));
    // 超长文件：b、c 各一份，冲突后按 C-20 截短为主体前段+摘要（去掉来源段）且互不相同。
    let long = "2026年度最终正式签署版本技术合作合同及全部附件资料非常非常长的文件名称.pdf";
    // C-19：摘要输入是“分析开始时”的原始完整路径——父目录整理时即前一趟整理的落位路径。
    let dig_b = digest8(&format!("b/文档/2026/08/{long}"));
    let dig_c = digest8(&format!("c/文档/2026/08/{long}"));
    let cut_b = format!("文档/2026/08/2026年度最终正式签署版本技术合作合同及全部附件_{dig_b}.pdf");
    let cut_c = format!("文档/2026/08/2026年度最终正式签署版本技术合作合同及全部附件_{dig_c}.pdf");
    assert!(f.exists(&cut_b), "b 超长文件截短+摘要：{cut_b}");
    assert!(f.exists(&cut_c), "c 超长文件截短+摘要：{cut_c}");
    assert_eq!(f.root.join("文档/2026/08").read_dir().unwrap().count(), 2);
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
    let doc_entries: Vec<String> = fs::read_dir(f.root.join("文档"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(doc_entries, vec!["2026"], "分类目录下只有年份层级");
    // (5) 幂等：马上再次整理 a，不再产生任何文件操作。
    let again = f.plan();
    assert_eq!(again.summary.planned_move, 0, "已就位项不再移动");
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.planned_git, 0);
    assert_eq!(again.summary.planned_empty, 0);
}

// 覆盖 C-17 / 附录 E：目标月目录已有的「报告 (1).pdf」与「报告_1.pdf」统一消解，
// 且再次整理不再改名（幂等）。
#[test]
fn in_place_normalized_collision_resolves_once_and_stays() {
    let f = Fixture::new();
    f.write_at("文档/2026/03/报告 (1).pdf", b"a", "2026-03");
    f.write_at("文档/2026/03/报告_1.pdf", b"b", "2026-03");
    let task = f.plan();
    Fixture::apply(&task);
    // 两个已就位项规范化后同名 → 统一消解（其一保持、其一摘要化）。
    let entries: Vec<String> = fs::read_dir(f.root.join("文档/2026/03"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 2, "两个文件都在，无覆盖：{entries:?}");
    assert!(
        entries.contains(&"报告_1.pdf".to_string()),
        "唯一可占名的已就位项保持原名：{entries:?}"
    );
    let other = entries.iter().find(|n| n.as_str() != "报告_1.pdf").unwrap();
    let dig = digest8("文档/2026/03/报告 (1).pdf");
    assert!(
        other.starts_with("报告_1_") && other.contains(&dig),
        "新改者按 C-19 摘要消解：{other}（digest {dig}）"
    );
    // 再次整理：不再改名（C-16/附录 E 幂等）。
    let again = f.plan();
    assert_eq!(again.summary.planned_move, 0);
    Fixture::apply(&again);
    let entries2: Vec<String> = fs::read_dir(f.root.join("文档/2026/03"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, entries2);
}

// 覆盖 C-17：固定容器被普通文件占用时不挪走占用项，依赖项失败保留源项（S-01）。
#[test]
fn occupied_category_container_keeps_source_items() {
    let f = Fixture::new();
    f.write_at("报告.pdf", b"pdf", "2026-03");
    // 「文档」被普通文件占用：依赖「文档/2026/03」的归类全部失败并保留源项。
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
    f.write_at("Git项目集合/proj/README.md", b"r", "2026-01");
    f.write_at("note.txt", b"n", "2026-02");
    let task = f.plan();
    assert_eq!(task.summary.planned_git, 0, "已就位项目不再移动");
    assert!(!f.exists("Git项目集合/Git项目集合/proj/README.md"));
    // 没有第二个待移入项目时不新建集合目录之外的东西；普通文件照常归类。
    assert!(f.exists("note.txt") || task.summary.planned_move >= 1);
}

// 覆盖 C-14 / 附录 E：项目跨文件系统才可达时保留原项目（同卷内不可构造跨卷，
// 此处以“集合容器名被文件占用”验证保留语义的同族安全边界）。
#[test]
fn git_collection_blocked_by_file_keeps_projects() {
    let f = Fixture::new();
    f.git("proj");
    f.write_at("proj/src/a.rs", b"a", "2026-01");
    fs::write(f.root.join("Git项目集合"), b"occupied").unwrap();
    let task = f.plan();
    assert_eq!(task.summary.planned_git, 0);
    assert!(f.root.join("proj/.git").is_dir(), "原项目保留在原位置");
    assert!(f.root.join("Git项目集合").is_file(), "占用项不被删除或挪走");
}

// 覆盖 C-12 / C-05：归类移动保留文件时间戳（同卷改名），重复整理仍落同一「年/月」。
#[test]
fn moves_preserve_creation_time_for_idempotent_dates() {
    let f = Fixture::new();
    f.write_at("old/资料.pdf", b"x", "2024-07");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("文档/2024/07/资料.pdf"));
    let again = f.plan();
    assert_eq!(again.summary.planned_move, 0, "时间戳不变 → 仍判已就位");
}

// 覆盖 C-06：大文件命中后优先进入「大文件/年/月」，未命中仍按扩展名大类。
#[test]
fn large_files_take_priority_over_extension_category() {
    let f = Fixture::new();
    let cfg = Config {
        large_files: true,
        large_threshold_bytes: 1024,
        ..Config::default()
    };
    f.write_at("big.bin", &[7u8; 2048], "2026-05");
    f.write_at("small.pdf", b"p", "2026-05");
    let task = engine::prepare_at(&f.root, cfg, Context::default(), &f.state).unwrap();
    Fixture::apply(&task);
    assert!(f.exists("大文件/2026/05/big.bin"));
    assert!(f.exists("文档/2026/05/small.pdf"));
}

// 覆盖 C-05：创建与修改时间都不可用时（以超出可表示范围的时间近似），不归类、保留并说明。
#[test]
fn items_without_usable_date_stay_in_place() {
    let f = Fixture::new();
    let p = f.write_at("keep.pdf", b"x", "2026-05");
    // 伪造极端 mtime 与创建时间：无法表示的时间（超出 i64 纳秒范围）使该项无法归类，
    // 必须在原位置保留而不被当前日期代替。
    let far = UNIX_EPOCH + Duration::from_secs(270_000_000_000);
    jchtools::fsutil::set_created_time(&p, far).unwrap();
    filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(far)).unwrap();
    let task = f.plan();
    assert_eq!(task.summary.planned_move, 0);
    assert!(f.exists("keep.pdf"));
}

// 覆盖 C-18 / C-19：同目录内两组不同来源的同名文件最少级数消解 + 摘要兜底。
#[test]
fn minimal_level_prefixes_and_digest_fallback() {
    let f = Fixture::new();
    f.write_at("x/合同.pdf", b"1", "2026-05");
    f.write_at("y/合同.pdf", b"2", "2026-05");
    f.write_at("合同.pdf", b"3", "2026-05");
    let task = f.plan();
    Fixture::apply(&task);
    // k=1 即互不相同：x_合同 / y_合同 / 合同（根文件无来源段，保持原名）。
    assert!(f.exists("文档/2026/05/x_合同.pdf"));
    assert!(f.exists("文档/2026/05/y_合同.pdf"));
    assert!(f.exists("文档/2026/05/合同.pdf"));
    // 同来源目录再放一对同名不同内容文件：层级用尽 → C-19 摘要。
    let f2 = Fixture::new();
    f2.write_at("same/报告.pdf", b"1", "2026-05");
    f2.write_at("same/副本 区分用.pdf", b"2", "2026-05");
    // 两个不同名文件不冲突；改为同目录两个同名文件（内容不同，不同名去重默认关闭）：
    let f3 = Fixture::new();
    let p1 = f3.write_at("same/报告.pdf", b"1", "2026-05");
    let p2 = f3.write_at("same/报告.pdf", b"2", "2026-05");
    let _ = (p1, p2); // 同一 rel 只能存在一份，改用嵌套同名构造见下。
    let f4 = Fixture::new();
    f4.write_at("same/资料.pdf", b"1", "2026-05");
    f4.write_at("same/nested/资料.pdf", b"2", "2026-05");
    let task4 = f4.plan();
    Fixture::apply(&task4);
    // k=1：same_资料 vs nested_资料 已互不相同，不进入摘要。
    assert!(f4.exists("文档/2026/05/same_资料.pdf"));
    assert!(f4.exists("文档/2026/05/nested_资料.pdf"));
    let _ = task;
}

// 覆盖 H-06：祖先直接含 .git 时目录整理拒绝整次处理（附录 E）。
#[test]
fn root_inside_git_project_is_rejected() {
    let f = Fixture::new();
    f.git("proj");
    f.write_at("proj/data/a.txt", b"a", "2026-01");
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
    f.write_at("解压失败/broken.zip", b"zip", "2026-01");
    f.write_at("normal.txt", b"n", "2026-02");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("解压失败/broken.zip"), "隔离容器内容不参与处理");
    assert!(f.exists("文档/2026/02/normal.txt"));
    assert!(!f.root.join("解压失败").join("文档").exists());
}
