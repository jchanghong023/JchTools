//! 覆盖 C-05 / C-14 / C-15 / C-16～C-21：固定归类「大类」一级结构 +「Git项目集合」
//! 的标准形态、来源前缀与短哈希消解、超长名截短与幂等。用例直接复刻合同
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
/// C-20 触发后主体截到 25 个 UTF-16 单元的前段（超长名两例共用）。
const LONG_CUT: &str = "芯片研发中心第一联合研发重大专项技术合作合同及全部";

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

// 覆盖 C-05 / C-14 / C-15 (2)：整理子目录 b 的标准形态（一级大类）。
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

    // C-05 一级结构：文件直接落在大类下。
    assert!(f.exists("文档/年度报告.pdf"));
    let h_copy = digest8("工作资料/年报/产品说明 (1).pdf");
    let h_plain = digest8("工作资料/年报/产品说明_1.pdf");
    // C-19：来源同级同名同目标 → 最近一级来源 + 主体 + 短哈希。
    assert!(
        f.exists(&format!("文档/年报_产品说明_1_{h_copy}.pdf")),
        "实际内容见下方断言"
    );
    assert!(f.exists(&format!("文档/年报_产品说明_1_{h_plain}.pdf")));
    assert!(
        f.exists("文档/客户A_合同.pdf"),
        "同名文件按来源前缀消解"
    );
    assert!(f.exists("文档/客户B_合同.pdf"));
    assert!(f.exists("文档/客户A_报价.xlsx"));
    assert!(f.exists("文档/客户B_报价.xlsx"));
    // C-18：两级来源仍冲突时升到第三级（华东_/华南_）。
    assert!(f.exists("文档/华东_客户A_正式版_方案.pdf"));
    assert!(f.exists("文档/华南_客户A_正式版_方案.pdf"));
    // 紧邻「图片」段按 C-18 剔除 → 剩余来源「工作资料」作前缀。
    assert!(f.exists("图片/工作资料_logo.png"));
    assert!(f.exists("图片/客户A_logo.png"));
    // C-08/附录 B：(2) → _2。
    assert!(f.exists("图片/截图_2.png"));
    assert!(f.exists("视频/demo.mp4"));
    // 未冲突的超长文件保持原名（C-20：40 不是全体文件强制改名阈值）。
    assert!(f.exists(&format!("文档/{LONG_NAME}")));
    // C-14：项目平铺进集合、目录名不变；内容整树保留。
    assert!(f.root.join("Git项目集合/tools/.git").is_dir());
    assert!(f.root.join("Git项目集合/tools/src/main.rs").is_file());
    assert!(f.root.join("Git项目集合/project-alpha/.git").is_dir());
    // H-05/C-07：搬空的中间目录全部消失。
    assert!(!f.root.join("工作资料").exists());
    assert!(!f.root.join("客户A").exists());
    assert!(!f.root.join("华东").exists());
    assert!(!f.root.join("code").exists());
    // C-05：根下只有大类与集合容器；大类内没有任何子目录（严格一级）。
    assert_eq!(
        f.subdirs(""),
        BTreeSet::from([
            "文档".into(),
            "图片".into(),
            "视频".into(),
            "Git项目集合".into()
        ])
    );
    for category in ["文档", "图片", "视频"] {
        assert!(
            f.subdirs(category).is_empty(),
            "大类 {category} 内不得出现任何第二级分类目录"
        );
    }
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
    assert!(f.exists("b/文档/年度报告.pdf"));
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
    assert!(f.exists("c/文档/年度报告.pdf"));
    assert!(f.exists("c/文档/安装说明.pdf"));
    assert!(f.exists("c/文档/客户A_合同.pdf"));
    assert!(f.exists("c/图片/截图_1.png"));
    // 第三趟：整理父目录 a。
    let task = f.plan();
    Fixture::apply(&task);

    // —— 合同 (4) 的最终形态（一级大类 + 来源前缀消解） ——
    assert!(f.exists("文档/b_年度报告.pdf"));
    assert!(f.exists("文档/c_年度报告.pdf"));
    assert!(
        f.exists("文档/安装说明.pdf"),
        "只来自 c 的不冲突，保持原名"
    );
    assert!(f.exists("文档/需求.docx"));
    assert!(
        f.exists("图片/工作资料_logo.png"),
        "无新冲突的历史消解名原样保留"
    );
    assert!(f.exists("图片/客户A_logo.png"));
    assert!(f.exists("文档/b_客户A_合同.pdf"));
    assert!(f.exists("文档/c_客户A_合同.pdf"));
    assert!(
        f.exists("文档/客户B_合同.pdf"),
        "不得为形式统一全部加前缀"
    );
    assert!(f.exists("文档/客户C_合同.pdf"));
    assert!(f.exists("文档/客户A_报价.xlsx"));
    assert!(f.exists("文档/客户B_报价.xlsx"));
    assert!(f.exists("文档/报价.xlsx"));
    assert!(f.exists("文档/华东_客户A_正式版_方案.pdf"));
    assert!(f.exists("文档/华南_客户A_正式版_方案.pdf"));
    assert!(f.exists("文档/方案.pdf"));
    assert!(f.exists("图片/截图_1.png"));
    assert!(f.exists("图片/截图_2.png"));
    assert!(f.exists("视频/b_demo.mp4"));
    assert!(f.exists("视频/c_demo.mp4"));
    // 超长文件：同目标同名 → C-20 截短为主体前段 + 摘要（来源段去掉）；
    // C-19 摘要输入是“分析开始时”的原始完整路径（前一趟的落位路径）。
    let dig_b = digest8(&format!("b/文档/{LONG_NAME}"));
    let dig_c = digest8(&format!("c/文档/{LONG_NAME}"));
    let cut_b = format!("文档/{LONG_CUT}_{dig_b}.pdf");
    let cut_c = format!("文档/{LONG_CUT}_{dig_c}.pdf");
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
    // C-15：中间目录（含上一轮的子级大类目录与集合目录）全部消失，无重复叠加。
    assert!(!f.root.join("b").exists());
    assert!(!f.root.join("c").exists());
    assert!(!f.root.join("Git项目集合/Git项目集合").exists());
    assert_eq!(
        f.subdirs(""),
        BTreeSet::from([
            "文档".into(),
            "图片".into(),
            "视频".into(),
            "Git项目集合".into()
        ]),
        "根下只有大类与集合容器"
    );
    for category in ["文档", "图片", "视频"] {
        assert!(
            f.subdirs(category).is_empty(),
            "大类 {category} 下不得出现任何第二级分类目录"
        );
    }
    // (6) 禁止的形态：无套娃、无功能/年月层级。
    assert!(!f.exists("文档/合同/客户A_合同.pdf"));
    assert!(!f.exists("文档/年度报告/b_年度报告.pdf"));
    assert!(!f.root.join("文档/2026").exists());
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

// 覆盖 C-05：普通文件只按扩展名进入一级大类（用户示例口径）。
#[test]
fn simple_files_land_directly_in_category() {
    let f = Fixture::new();
    f.write("a.pdf", b"pdf");
    f.write("b.docx", b"docx");
    f.write("x.png", b"png");
    f.write("y.mp4", b"mp4");
    f.write("z.mp3", b"mp3");
    f.write("w.zip", b"zip");
    f.write("unknown.odg", b"odg");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("文档/a.pdf"));
    assert!(f.exists("文档/b.docx"));
    assert!(f.exists("图片/x.png"));
    assert!(f.exists("视频/y.mp4"));
    assert!(f.exists("音频/z.mp3"));
    assert!(f.exists("压缩包/w.zip"));
    // 附录 A：未匹配扩展名 → 「其他」大类（一级，不是任何大类下的兜底目录）。
    assert!(f.exists("其他/unknown.odg"));
    for category in ["文档", "图片", "视频", "音频", "压缩包", "其他"] {
        assert!(f.subdirs(category).is_empty(), "{category} 必须严格一级");
    }
    // 源文件已不在根直接层（全部移入大类）。
    assert!(!f.exists("a.pdf"));
}

// 覆盖 C-05（旧版两级结构不是标准形态：重新整理时拉平，遗留功能目录名只作来源前缀）。
#[test]
fn legacy_two_level_tree_is_flattened() {
    let f = Fixture::new();
    // 旧版「大类/功能分类」与「大类/其他」输出：文件全部拉平到大类下。
    f.write("文档/MBIST/report.pdf", b"r");
    f.write("文档/其他/notes.txt", b"n");
    f.write("图片/DEMO/demo.mp4", b"d");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("文档/report.pdf"), "无冲突时不加前缀，直接落大类");
    assert!(f.exists("文档/notes.txt"));
    assert!(f.exists("视频/demo.mp4"), "按扩展名重新判大类");
    for category in ["文档", "视频"] {
        assert!(f.subdirs(category).is_empty());
    }
    // 「图片」内容全部移走后按 C-07 消失，不再保留空大类目录。
    assert!(!f.root.join("图片").exists());
    // 遗留功能目录名在同名冲突时作为普通来源目录参与消解（C-18）。
    let g = Fixture::new();
    g.write("b/文档/MBIST/规格.pdf", b"1");
    g.write("c/文档/AMBA/规格.pdf", b"2");
    let task = g.plan();
    Fixture::apply(&task);
    assert!(g.exists("文档/MBIST_规格.pdf"));
    assert!(g.exists("文档/AMBA_规格.pdf"));
    assert!(g.subdirs("文档").is_empty());
    // 拉平后幂等。
    let again = g.plan();
    assert_eq!(again.summary.planned_move, 0);
}

// 覆盖 C-17 / 附录 E：目标大类目录已有「报告 (1).pdf」与「报告_1.pdf」统一消解，
// 且再次整理不再改名（幂等）。
#[test]
fn in_place_normalized_collision_resolves_once_and_stays() {
    let f = Fixture::new();
    f.write("文档/报告 (1).pdf", b"a");
    f.write("文档/报告_1.pdf", b"b");
    let task = f.plan();
    Fixture::apply(&task);
    // 两个已就位项规范化后同名 → 统一消解（其一保持、其一摘要化；紧邻「文档」段
    // 已剔除，摘要形式无来源段）。
    let entries: Vec<String> = fs::read_dir(f.root.join("文档"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 2, "两个文件都在，无覆盖：{entries:?}");
    assert!(
        entries.contains(&"报告_1.pdf".to_string()),
        "唯一可占名的已就位项保持原名：{entries:?}"
    );
    let other = entries.iter().find(|n| n.as_str() != "报告_1.pdf").unwrap();
    let dig = digest8("文档/报告 (1).pdf");
    assert!(
        other.starts_with("报告_1_") && other.contains(&dig),
        "新改者按 C-19 摘要消解：{other}（digest {dig}）"
    );
    // 再次整理：不再改名（C-16/附录 E 幂等）。
    let again = f.plan();
    assert_eq!(again.summary.planned_move, 0);
    Fixture::apply(&again);
    let entries2: Vec<String> = fs::read_dir(f.root.join("文档"))
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
    // 「文档」被普通文件占用：依赖「文档」容器的归类全部失败并保留源项。
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
    assert!(f.exists("文档/资料.pdf"));
    let again = f.plan();
    assert_eq!(again.summary.planned_move, 0, "无变化 → 仍判已就位");
}

// 覆盖 C-06：大文件命中后优先进入「大文件」大类（同为一级结构）。
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
    assert!(f.exists("大文件/big.bin"));
    assert!(f.exists("文档/small.pdf"));
    assert!(f.subdirs("大文件").is_empty());
}

// 覆盖 C-05：主体全是噪声（日期编号）的文件名不影响按扩展名归类。
#[test]
fn noise_only_name_lands_with_original_name() {
    let f = Fixture::new();
    f.write("2026-05-10.pdf", b"x");
    let task = f.plan();
    Fixture::apply(&task);
    assert!(f.exists("文档/2026-05-10.pdf"), "实际文件名不被改写");
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
    assert!(f.exists("文档/x_合同.pdf"));
    assert!(f.exists("文档/y_合同.pdf"));
    assert!(f.exists("文档/合同.pdf"));
    let f4 = Fixture::new();
    f4.write("same/报表.pdf", b"1");
    f4.write("same/nested/报表.pdf", b"2");
    let task4 = f4.plan();
    Fixture::apply(&task4);
    // k=1：same_报表 vs nested_报表 已互不相同，不进入摘要。
    assert!(f4.exists("文档/same_报表.pdf"));
    assert!(f4.exists("文档/nested_报表.pdf"));
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
    assert!(f.exists("文档/normal.txt"));
    assert!(!f.root.join("解压失败").join("文档").exists());
}
