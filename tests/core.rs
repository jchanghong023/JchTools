//! These tests mutate only tempfile fixtures. 删除一律为永久删除（S-02），不再有回收站注入点。
// 测试代码允许 unwrap/expect 与短名（f/p/q 夹具惯例）：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
use jchtools::{
    config::*,
    control::{Context, Control},
    db::Database,
    engine, fsutil, hashing,
    model::{ActionKind, FileRecord, Snapshot},
    platform::{self, DeleteResult},
    rules,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};
use tempfile::TempDir;
struct Fixture {
    temp: TempDir,
    root: PathBuf,
    state: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let state = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        Self { temp, root, state }
    }
    fn write(&self, name: &str, bytes: &[u8], time: i64) -> PathBuf {
        let p = self.root.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        filetime::set_file_mtime(&p, filetime::FileTime::from_unix_time(time, 0)).unwrap();
        p
    }
    fn plan(&self, cfg: Config) -> engine::TaskResult {
        engine::prepare_at(&self.root, cfg, Context::default(), &self.state).unwrap()
    }
    fn apply(task: &engine::TaskResult) -> engine::TaskResult {
        engine::apply(&task.directory, Context::default()).unwrap()
    }
}
fn base() -> Config {
    Config {
        global_delete: DeleteMode::Permanent,
        // 这些用例考的是记账 / 归类 / 排序 / 幂等等其它行为，需要「内容相同的不同名文件」
        // 也进入去重，故显式开启第三类；C-02 的默认值（不同名关闭）由
        // `different_names_not_deduped_by_default` 单独锚定。
        // 归类按 C-05 恒开启（大类一级），不再有 classify 开关。
        dedup_other_names: true,
        clean_copy_name: false,
        ..Config::default()
    }
}
/// 在根下按文件名递归查找（C-05 归类恒移动文件，断言“内容仍在”需按名找）。
fn exists_somewhere(root: &Path, name: &str) -> bool {
    fn walk(dir: &Path, name: &str) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if walk(&path, name) {
                    return true;
                }
            } else if entry.file_name().to_string_lossy() == name {
                return true;
            }
        }
        false
    }
    walk(root, name)
}
// 覆盖 S-02
#[test]
fn defaults_valid_and_roundtrip() {
    let cfg = Config::default();
    cfg.validate().unwrap();
    let text = serde_json::to_string(&cfg).unwrap();
    let back = Config::from_json_text(&text).unwrap();
    assert_eq!(
        serde_json::to_value(&cfg).unwrap(),
        serde_json::to_value(&back).unwrap()
    );
}
#[test]
fn reject_unknown_configuration() {
    assert!(Config::from_json_text(r#"{"delete_everything":true}"#).is_err());
}
// 覆盖 S-03, C-02（verify_bytes 已随合同第三批整体移除、旧值被剥除；同名版本取舍开关已按合同移除）
#[test]
fn legacy_config_with_removed_fields_still_loads() {
    let mut value = serde_json::to_value(Config::default()).unwrap();
    for (key, v) in [
        ("hash_algorithm", serde_json::json!("md5")),
        ("verify_bytes", serde_json::json!(false)),
        ("same_name_same_size", serde_json::json!(true)),
        ("same_size_keep", serde_json::json!("oldest")),
        ("same_name_different_size", serde_json::json!(true)),
        ("different_size_keep", serde_json::json!("oldest")),
        ("conflict_scope_directory", serde_json::json!(false)),
    ] {
        value.as_object_mut().unwrap().insert(key.into(), v);
    }
    Config::from_json_text(&value.to_string()).unwrap();
}
// 覆盖 R-01
#[test]
fn schema_matches_every_configuration_field() {
    let cfg = serde_json::to_value(Config::default()).unwrap();
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../resources/rules.json")).unwrap();
    let keys = schema.as_array().unwrap();
    let fields = cfg.as_object().unwrap();
    // 有意不进规则表的字段：theme 在「关于」页；detect_type 是合并行的影子键
    // （「修正扩展名」一行同时驱动 detect_type 与 fix_extension，引擎/旧任务库仍读原值）。
    let hidden = ["theme", "detect_type"];
    assert_eq!(fields.len(), keys.len() + hidden.len());
    for row in keys {
        assert!(fields.get(row["key"].as_str().unwrap()).is_some());
    }
    for key in hidden {
        assert!(fields.contains_key(key), "{key} 应作为配置字段保留");
    }
}
// 覆盖 R-03, X-08, C-08
#[test]
fn coupled_validation_and_bounds() {
    let mut c = base();
    // R-03：detect_type 是 fix_extension 的影子键，由 from_json_text / set_json 强制同值，
    // 校验不再把「只开 fix_extension」判为非法（直接改字段也无法构造持久化的不同步状态）。
    c.fix_extension = true;
    c.detect_type = true;
    assert!(c.validate().is_ok());
    let synced = Config::from_json_text(&serde_json::to_string(&c).unwrap()).unwrap();
    assert_eq!(
        synced.detect_type, synced.fix_extension,
        "影子键必须同值回读"
    );
    c.hash_workers = 0;
    assert!(c.validate().is_err());
    c.hash_workers = 2;
    c.reserve_bytes = u64::MAX;
    assert!(c.validate().is_err());
}
// 覆盖 S-05
#[test]
fn unsafe_paths_rejected() {
    for value in [
        "../x",
        "x/../../outside",
        "C:/x",
        "/etc/passwd",
        r"\\server\share\x",
        "file:stream",
        "CON.txt",
        "a/NUL",
        "x. ",
        "a\n.txt",
        "",
    ] {
        assert!(fsutil::safe_relative(value).is_err(), "{value:?}");
    }
}
// 覆盖 S-05
#[test]
fn relative_tar_and_unicode_paths_accepted() {
    assert_eq!(
        fsutil::safe_relative("./报告/a.pdf").unwrap(),
        PathBuf::from("报告/a.pdf")
    );
    assert_eq!(
        fsutil::safe_relative(r"资料\图片.png").unwrap(),
        PathBuf::from("资料/图片.png")
    );
}
// 覆盖 S-05
#[test]
fn windows_reserved_names_rejected() {
    for s in ["con", "COM1", "LPT9.txt", "nul", "AUX.jpg", "COM¹.txt"] {
        assert!(fsutil::validate_component(s).is_err());
    }
    assert!(fsutil::validate_component("COM10.txt").is_ok());
    assert!(fsutil::validate_component("COM0.txt").is_ok());
}
// 覆盖 S-05
#[test]
fn trailing_unicode_whitespace_rejected() {
    assert!(fsutil::validate_component("a\u{3000}").is_err());
    assert!(fsutil::validate_component("a\u{a0}").is_err());
    assert!(fsutil::validate_component("a b").is_ok());
}
// 覆盖 S-01
#[test]
fn rename_never_overwrites() {
    let f = Fixture::new();
    let a = f.write("a", b"A", 1);
    let b = f.write("b", b"B", 2);
    assert!(fsutil::rename_noreplace(&a, &b).is_err());
    assert_eq!(fs::read(a).unwrap(), b"A");
    assert_eq!(fs::read(b).unwrap(), b"B");
}
// 覆盖 C-05, S-01
#[test]
fn unique_name_preserves_extension() {
    let f = Fixture::new();
    let p = f.write("report.pdf", b"a", 1);
    let q = fsutil::unique_target(&f.root, &p).unwrap();
    assert_eq!(q.file_name().unwrap(), "report (1).pdf");
}
// 覆盖 C-05, H-07
#[test]
fn unique_name_truncates_long_stem_to_component_limit() {
    // 回归：基础名接近 255 个 UTF-16 单元且目标被占用时，“名 (N).扩展”候选名会超限，
    // validate_component 令 unique_target 整体失败——归类冲突回退因此把整次任务搞失败。
    // 应截断 stem 生成合法候选名（扩展名保持不变，H-07 只允许改文件名主体）。
    let f = Fixture::new();
    // 基础名 251+4=255 恰好合法；加「 (1)」后 260 超限，截断只能落在 stem 上。
    let stem = "a".repeat(251);
    let p = f.write(&format!("{stem}.txt"), b"a", 1);
    let q = fsutil::unique_target(&f.root, &p).unwrap();
    let name = q.file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        name.encode_utf16().count() <= 255,
        "分配名不得超 255 个 UTF-16 单元：{name}"
    );
    assert!(name.ends_with(" (1).txt"), "保持扩展名与序号后缀：{name}");
    assert_ne!(q, p);
    // 截断只发生在超限时：常规名不受影响。
    let r = f.write("b.pdf", b"b", 1);
    assert_eq!(
        fsutil::unique_target(&f.root, &r)
            .unwrap()
            .file_name()
            .unwrap(),
        "b (1).pdf"
    );
    // 复合扩展名与编号分卷整体保留：序号插在扩展名之前（H-07 例：资料.tar.gz → 资料 (1).tar.gz）。
    let gz = f.write("资料.tar.gz", b"g", 1);
    assert_eq!(
        fsutil::unique_target(&f.root, &gz)
            .unwrap()
            .file_name()
            .unwrap(),
        "资料 (1).tar.gz"
    );
    let vol = f.write("包.7z.001", b"v", 1);
    assert_eq!(
        fsutil::unique_target(&f.root, &vol)
            .unwrap()
            .file_name()
            .unwrap(),
        "包 (1).7z.001"
    );
}
// 覆盖 H-07（不得通过更换或追加扩展名腾名：无法保留扩展名时显式报错且不动原文件）
#[test]
fn unique_target_errors_when_extension_cannot_take_suffix() {
    let f = Fixture::new();
    // 扩展名 251 个单元：追加「 (1)」必然超 255 单元上限；截断扩展名等于改扩展名，禁止。
    let long_ext = "x".repeat(251);
    let p = f.write(&format!("a.{long_ext}"), b"a", 1);
    assert!(
        fsutil::unique_target(&f.root, &p).is_err(),
        "扩展名放不下序号时必须显式报错，不得截断/更换扩展名"
    );
    assert!(p.exists(), "分配失败不得改动原文件");
    // 常规扩展名照常分配：报错只针对放不下序号的超长扩展名。
    let ok = f.write("a.txt", b"a", 1);
    assert!(fsutil::unique_target(&f.root, &ok).is_ok());
}
// 覆盖 S-03
#[test]
fn hashes_known_vectors() {
    let f = Fixture::new();
    let p = f.write("a", b"abc", 1);
    let ctl = Control::default();
    assert_eq!(
        hashing::full_hash(&p, &ctl).unwrap(),
        "blake3:6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    );
}
// 覆盖 C-10（取消后不再继续读取）
#[test]
fn cancelled_hash_does_not_read() {
    let f = Fixture::new();
    let p = f.write("a", b"abc", 1);
    let ctl = Control::default();
    ctl.cancel();
    assert!(hashing::full_hash(&p, &ctl).is_err());
    assert_eq!(ctl.read_bytes.load(Ordering::Relaxed), 0);
}
// 覆盖 C-08（副本后缀清理的识别口径与输出名转换：`(N)` 转 `_N`，其余标记移除）
#[test]
fn copy_suffixes_and_nonempty_name() {
    for s in ["报告 (1).pdf", "报告 - Copy.pdf", "报告 副本.pdf"] {
        assert_eq!(rules::copy_key(s), "报告.pdf", "{s}");
    }
    // 输出转换：`(N)` 转为 `_N`（去前导零、全零保留 0）；`- Copy` / `副本` 移除。
    //（连续多标记的转换由 rules::copy_marker_conversion_matches_appendix_b 锚定。）
    assert_eq!(rules::clean_copy_output("报告 (1).pdf"), "报告_1.pdf");
    assert_eq!(rules::clean_copy_output("报告 (01).pdf"), "报告_1.pdf");
    assert_eq!(rules::clean_copy_output("报告 (0).pdf"), "报告_0.pdf");
    // 全角括号不是副本标记（附录 B：ASCII 圆括号才算），名称保持原样。
    assert_eq!(rules::clean_copy_output("报告（2）.pdf"), "报告（2）.pdf");
    // 基础主体为空时用 `_`（附录 B）。
    assert_eq!(rules::clean_copy_output("(1).pdf"), "__1.pdf");
}
// 覆盖 X-01（支持格式与分卷首卷识别）
#[test]
fn archive_first_volume_detection() {
    assert!(rules::archive_name("A.part01.rar"));
    assert!(!rules::archive_name("A.part02.rar"));
    assert!(rules::archive_name("A.7z.001"));
    assert!(!rules::archive_name("A.7z.002"));
    assert!(rules::archive_name("A.tar.gz"));
}
// 覆盖 S-02（保留方式不删除任何东西）
#[test]
fn keep_never_deletes() {
    let f = Fixture::new();
    let p = f.write("a", b"a", 1);
    assert_eq!(
        platform::remove(&p, DeleteMode::Keep, &Control::default()).unwrap(),
        DeleteResult::Kept
    );
    assert!(p.exists());
}
// 覆盖 C-07（MUST NOT 递归删除有内容的目录）
#[test]
fn nonempty_directory_cannot_be_removed() {
    let f = Fixture::new();
    f.write("sub/a", b"a", 1);
    assert!(platform::remove(
        &f.root.join("sub"),
        DeleteMode::Permanent,
        &Control::default()
    )
    .is_err());
    assert!(f.root.join("sub/a").exists());
}
// 覆盖 C-01, C-03
#[test]
fn analysis_no_changes_and_dedup_keeps_latest() {
    let f = Fixture::new();
    let old = f.write("old.txt", b"same", 10);
    let new = f.write("new.txt", b"same", 20);
    let task = f.plan(base());
    assert!(old.exists() && new.exists());
    assert_eq!(task.summary.planned_delete, 1);
    Fixture::apply(&task);
    assert!(!old.exists());
    assert!(
        exists_somewhere(&f.root, "new.txt"),
        "保留者随固定归类移动（C-05 恒开启），内容仍在"
    );
}
// 覆盖 R-02
#[test]
fn different_names_can_be_disabled() {
    let f = Fixture::new();
    f.write("a.txt", b"same", 10);
    f.write("b.txt", b"same", 20);
    let mut cfg = base();
    cfg.dedup_other_names = false;
    assert_eq!(f.plan(cfg).summary.planned_delete, 0);
}
// 覆盖 R-02
#[test]
fn same_names_different_directories_can_be_disabled() {
    let f = Fixture::new();
    f.write("a/test.txt", b"same", 10);
    f.write("b/test.txt", b"same", 20);
    let mut cfg = base();
    cfg.dedup_same_name = false;
    assert_eq!(f.plan(cfg).summary.planned_delete, 0);
}
// 覆盖 R-02
#[test]
fn copy_names_can_be_disabled_independently() {
    let f = Fixture::new();
    f.write("a.txt", b"same", 10);
    f.write("a (1).txt", b"same", 20);
    let mut cfg = base();
    cfg.dedup_copy_names = false;
    assert_eq!(f.plan(cfg).summary.planned_delete, 0);
}
// 覆盖 C-02, S-03
#[test]
fn equal_size_different_hash_not_deleted_when_rule_off() {
    let f = Fixture::new();
    f.write("a/report.txt", b"abcd", 10);
    f.write("b/report.txt", b"efgh", 20);
    assert_eq!(f.plan(base()).summary.planned_delete, 0);
}
// 覆盖 C-01（计划逐项可勾选/取消）
#[test]
fn unselecting_action_preserves_file() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let db = Database::open(&task.directory).unwrap();
    let action = db.actions_page(0, 10).unwrap().remove(0);
    db.set_selected(action.id, false).unwrap();
    drop(db);
    Fixture::apply(&task);
    assert!(
        exists_somewhere(&f.root, "a") && exists_somewhere(&f.root, "b"),
        "取消勾选的删除不执行，两份文件都随归类保留"
    );
}
// 覆盖 C-08
#[test]
fn cleanup_does_not_become_only_dedup_keeper() {
    let f = Fixture::new();
    f.write("keep.txt", b"content", 10);
    f.write("temporary.tmp", b"content", 20);
    let mut cfg = base();
    cfg.clean_temp = true;
    let task = f.plan(cfg);
    Fixture::apply(&task);
    assert!(exists_somewhere(&f.root, "keep.txt"));
    assert!(!exists_somewhere(&f.root, "temporary.tmp"));
}
// 覆盖 C-08
#[test]
fn cleanup_keep_files_are_not_dedup_deletions() {
    // 清理命中且删除方式为「保留」的文件由清理规则管辖（保留承诺）：去重路径同样不得删除。
    // 此前只防了「不得充当 keeper」，keeper 先注册时它会按重复规则（旧 duplicate_delete）被删，
    // 结果随 duplicate_order 排序翻转（本用例让 junk.tmp 排在 keeper 之后触发原缺陷）。
    let f = Fixture::new();
    f.write("normal.txt", b"payload", 20); // 较新 → 成为 keeper
    f.write("junk.tmp", b"payload", 10); // 较旧且命中 clean_temp → 修复前被按重复删除
    let mut cfg = base();
    cfg.clean_temp = true;
    // C-08：临时项清理的删除方式按类别独立覆盖（不再有单一 cleanup_delete 开关）。
    cfg.set_json("temp_delete", serde_json::json!("keep"))
        .unwrap();
    let task = f.plan(cfg);
    let actions = Database::open(&task.directory)
        .unwrap()
        .actions_page(0, 100)
        .unwrap();
    assert!(
        actions.iter().all(|a| a.kind != ActionKind::Delete),
        "清理保留的文件不得按重复规则删除"
    );
    Fixture::apply(&task);
    assert!(
        exists_somewhere(&f.root, "normal.txt") && exists_somewhere(&f.root, "junk.tmp"),
        "清理保留的两个文件都随归类保留"
    );
}
// 覆盖 C-04（硬链接执行的崩溃残留自愈，删除前校验 links>=2）
#[test]
fn stale_hardlink_temps_are_swept() {
    // 崩溃残留的硬链接临时文件被扫描永久剪枝且无其它回收路径：
    // 清扫只发生在执行确认之后（apply_with 执行前；C-01 分析阶段只读）。残留必是硬链接（内容仍由保留文件持有），
    // 清扫前校验链接数 >= 2，普通同名文件绝不能被当作残留删除。
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    let stale = f.root.join(".jchtools-link-deadbeef");
    if let Err(error) = fs::hard_link(f.root.join("a.txt"), &stale) {
        eprintln!(
            "hard_link failed: {error}; keeper={:?}",
            f.root.join("a.txt")
        );
        panic!("无法创建硬链接，无法验证残留清扫；请在支持硬链接的文件系统上运行测试");
    }
    filetime::set_file_mtime(&stale, filetime::FileTime::from_unix_time(0, 0)).unwrap();
    let task = f.plan(base());
    assert!(stale.exists(), "C-01：分析阶段不得删除残留（只读）");
    Fixture::apply(&task);
    assert!(!stale.exists(), "执行阶段必须清扫过期残留");
    assert!(
        exists_somewhere(&f.root, "a.txt"),
        "清扫残留不得影响 keeper 本体"
    );
    assert_eq!(task.summary.scanned, 1, "清扫不得影响正常文件的扫描");
}
// 覆盖 C-04（残留清扫不得误删用户文件）
#[test]
fn user_file_with_link_temp_prefix_is_never_swept() {
    // 回归：.jchtools-link- 前缀清扫此前不校验归属，同名用户文件（或从压缩包解出的
    // 同名成员）会被静默永久删除。崩溃残留必是硬链接（链接数 >= 2，内容仍有其他
    // 链接持有）；普通同名文件不属于本工具命名空间，必须原样保留。
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    let user = f.root.join(".jchtools-link-mydata");
    fs::write(&user, b"precious").unwrap();
    filetime::set_file_mtime(&user, filetime::FileTime::from_unix_time(0, 0)).unwrap();
    f.plan(base());
    assert!(user.exists(), "同名用户文件不是崩溃残留，绝不能被清扫");
}
// 覆盖 R-02（删除方式可按类覆盖）
#[test]
fn global_keep_prohibits_deletion() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let mut cfg = base();
    cfg.global_delete = DeleteMode::Keep;
    assert_eq!(f.plan(cfg).summary.planned_delete, 0);
}
// 注：原 class_override_beats_global_keep 依赖已删除的 duplicate_delete 覆盖键
//（C-04/S-02：副本处置随全局文件删除方式，无独立覆盖），“类别覆盖压过全局保留”
// 的行为由 cleanup_categories_override_delete_mode_independently 锚定。
// 注：原 copy_name_cleanup_uses_freed_original_name、classification_preserves_paths_and_is_idempotent、
// flatten_classification_allocates_nonconflicting_names 考的是已删除的
// “保留原路径 / 扁平化 / (N) 序号回退”归类形态；新归类（C-05/C-16/C-17）由
// classify_shape.rs 与 copy_name_cleanup_never_plans_self_move 等锚定。
// 覆盖 C-07
#[test]
fn empty_directory_cleanup_is_bottom_up() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("empty/nested")).unwrap();
    let cfg = base();
    let task = f.plan(cfg);
    Fixture::apply(&task);
    assert!(!f.root.join("empty").exists());
    assert!(f.root.exists());
}
// 覆盖 C-07, S-04
#[test]
fn empty_hidden_subdir_blocks_empty_directory_cleanup() {
    // 行为锚点：用户显式关闭隐藏项后，仅含一个空的隐藏（Windows）/点开头（Unix）子目录的
    // 目录，该子目录不会入库，目录对规划"并非实际为空"，不得计划为空目录。
    // 否则计划承诺落空：执行期实空复查只能跳过，skipped 虚增、该清的没清。
    // （默认 include_hidden=true 时该子目录会被扫描，属 S-04 的默认口径，见 hidden_dir_scanned_by_default。）
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("outer/.inner")).unwrap();
    #[cfg(windows)]
    set_hidden(&f.root.join("outer/.inner"), true);
    f.write("keep.txt", b"payload", 10);
    let mut cfg = base();
    cfg.include_hidden = false;
    let task = f.plan(cfg);
    assert_eq!(
        task.summary.planned_empty, 0,
        "含未入库子目录的目录不得计划为空目录"
    );
    assert!(f.root.join("outer").exists());
}
// 覆盖 S-04（默认包含隐藏/系统资料的开关口径）
#[test]
fn hidden_dir_scanned_by_default() {
    let f = Fixture::new();
    f.write("outer/.inner/hidden.txt", b"payload", 10);
    f.write("keep.txt", b"payload", 11);
    // Windows 的隐藏判定看文件属性位（点开头命名不算隐藏），显式打上属性。
    #[cfg(windows)]
    set_hidden(&f.root.join("outer/.inner"), true);
    assert_eq!(
        f.plan(base()).summary.scanned,
        2,
        "默认 include_hidden/include_system 为真：隐藏子目录照常扫描"
    );
    let mut off = base();
    off.include_hidden = false;
    assert_eq!(f.plan(off).summary.scanned, 1, "显式关闭隐藏项后才剪枝");
    #[cfg(windows)]
    set_hidden(&f.root.join("outer/.inner"), false);
}
// 覆盖 C-07
#[test]
fn empty_directory_with_underscore_not_blocked_by_similar_name() {
    // 行为契约：目录名含下划线/百分号时，空目录判定不得波及名字相似（仅差一两个字符）的邻居目录。
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("my_dir/nested_empty")).unwrap();
    f.write("myXdir/file.txt", b"payload", 10);
    let cfg = base();
    let task = f.plan(cfg);
    // 相似前缀目录仍在承载文件：它进空目录计划只能因为该文件有合法的归类移动，
    // 绝不能出现对文件的删除（误伤 = 相似名匹配把邻居当成空目录整树处理）。
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    let deletes = db.actions_page_filtered(0, 100, Some("delete")).unwrap();
    drop(db);
    assert!(
        moves.iter().any(|a| a.source == "myXdir/file.txt"),
        "邻居目录里的文件应有归类移动：{moves:?}"
    );
    assert!(
        deletes.iter().all(|a| a.source != "myXdir/file.txt"),
        "相似前缀目录里的文件不能被误伤：{deletes:?}"
    );
    Fixture::apply(&task);
    assert!(
        !f.root.join("my_dir").exists(),
        "含下划线的空目录必须能被清理"
    );
    assert!(
        exists_somewhere(&f.root, "file.txt"),
        "相似前缀目录里的文件内容仍在"
    );
}
// 覆盖 C-07
#[test]
fn empty_directory_with_percent_in_name_is_cleaned() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("100%done")).unwrap();
    f.write("100Xdone/file.txt", b"payload", 10);
    let cfg = base();
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let deletes = db.actions_page_filtered(0, 100, Some("delete")).unwrap();
    drop(db);
    assert!(
        deletes.iter().all(|a| a.source != "100Xdone/file.txt"),
        "相似前缀目录里的文件不能被误伤：{deletes:?}"
    );
    Fixture::apply(&task);
    assert!(!f.root.join("100%done").exists());
    assert!(exists_somewhere(&f.root, "file.txt"));
}
// 覆盖 C-05, C-07（归类与空目录清理同一计划：folder/ 随归类腾空并即时入空目录计划）
#[test]
fn classification_empty_dirs_planned_in_same_pass() {
    let f = Fixture::new();
    f.write("folder/a.pdf", b"pdf", 10);
    let cfg = base();
    let target = "文档/a.pdf".to_string();
    let task = f.plan(cfg.clone());
    assert_eq!(task.summary.planned_move, 1);
    assert_eq!(
        task.summary.planned_empty, 1,
        "folder/ becomes empty after the move and must be planned now"
    );
    Fixture::apply(&task);
    assert!(f.root.join(&target).exists());
    assert!(!f.root.join("folder").exists());
    let again = f.plan(cfg);
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.planned_move, 0);
    assert_eq!(again.summary.planned_empty, 0);
}
// 注：原 date_classification_is_idempotent 考的是已删除的 classify=Date 按修改日期归类
// 形态；「大类/年/月」时间归类与「大类/功能分类」两级功能归类均随 2026-09-23 需求变更
// 整体废除，由 classify_shape.rs 的 organize_parent_after_children_rebuilds_standard_shape
// 等锚定「大类」一级新形态。
// 覆盖 S-04
#[test]
fn excluded_tree_not_touched() {
    let f = Fixture::new();
    f.write("a.txt", b"same", 10);
    f.write("protected/a.txt", b"same", 20);
    let mut cfg = base();
    cfg.exclusions = "protected/**".into();
    assert_eq!(f.plan(cfg).summary.scanned, 1);
}
#[cfg(windows)]
fn set_hidden(path: &Path, hidden: bool) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, INVALID_FILE_ATTRIBUTES,
    };
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<u16>>();
    // SAFETY: wide 是以 NUL 结尾的 UTF-16 路径；调用只读取该缓冲区。
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    assert_ne!(
        attrs,
        INVALID_FILE_ATTRIBUTES,
        "读取属性失败：{}",
        path.display()
    );
    let next = if hidden {
        attrs | FILE_ATTRIBUTE_HIDDEN
    } else {
        attrs & !FILE_ATTRIBUTE_HIDDEN
    };
    assert_ne!(
        // SAFETY: 同上，wide 仍指向以 NUL 结尾的 UTF-16 路径。
        unsafe { SetFileAttributesW(wide.as_ptr(), next) },
        0,
        "设置属性失败：{}",
        path.display()
    );
}
// 覆盖 S-04
#[cfg(windows)]
#[test]
fn hidden_root_directory_still_scanned() {
    // 行为锚点：扫描从根目录的条目开始，根目录自身的隐藏属性不参与剪枝；
    // 根下的隐藏子目录在默认口径（include_hidden/include_system 默认真）下照常扫描，
    // 用户显式关闭隐藏项后才整树剪枝。
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    f.write("secret/b.txt", b"hidden", 20);
    set_hidden(&f.root, true);
    set_hidden(&f.root.join("secret"), true);
    let included = f.plan(base());
    let mut pruned = base();
    pruned.include_hidden = false;
    let excluded = f.plan(pruned);
    set_hidden(&f.root, false);
    set_hidden(&f.root.join("secret"), false);
    assert_eq!(
        included.summary.scanned, 2,
        "默认包含隐藏资料：隐藏根目录里的普通文件与隐藏子目录里的文件都必须被扫描到"
    );
    assert_eq!(
        excluded.summary.scanned, 1,
        "显式关闭隐藏项后剪枝隐藏子目录，隐藏根目录本身仍不剪枝"
    );
}
// 覆盖 S-04
#[cfg(not(windows))]
#[test]
fn hidden_root_directory_still_scanned() {
    // 行为锚点：扫描从根目录的条目开始，根目录自身的点开头命名不参与剪枝；
    // 根下的点开头子目录在默认口径（include_hidden 默认真）下照常扫描，
    // 用户显式关闭隐藏项后才剪枝。
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".hidden-root");
    fs::create_dir_all(root.join(".secret")).unwrap();
    fs::write(root.join("a.txt"), b"payload").unwrap();
    fs::write(root.join(".secret/b.txt"), b"hidden").unwrap();
    let state = temp.path().join("state");
    let included = engine::prepare_at(&root, base(), Context::default(), &state).unwrap();
    let mut pruned = base();
    pruned.include_hidden = false;
    let excluded = engine::prepare_at(&root, pruned, Context::default(), &state).unwrap();
    assert_eq!(
        included.summary.scanned, 2,
        "默认包含隐藏资料：点开头根目录里的普通文件与点开头子目录里的文件都必须被扫描到"
    );
    assert_eq!(
        excluded.summary.scanned, 1,
        "显式关闭隐藏项后剪枝点开头子目录，点开头根目录本身仍不剪枝"
    );
}
// 注：原 merge_directories_never_pulls_files_out_of_output 考的是已删除的
// merge_directories / output_dir 归类形态（C-05 固定归类后不再有输出目录与合并）；
// 跨轮幂等由 classify_shape.rs 的 organize_parent_after_children_rebuilds_standard_shape 锚定。
// 覆盖 C-08, C-17
#[test]
fn copy_name_cleanup_never_plans_self_move() {
    // 保留文件剥离副本标记后按 C-17 统一消解改名归类；计划里不得出现
    // source==target 的空转移动（执行后 moved 计数虚高、破坏幂等）。
    let f = Fixture::new();
    f.write("报告 (1).pdf", b"A", 30);
    f.write("报告 (2).pdf", b"A", 20);
    f.write("报告.pdf", b"B", 10);
    let mut cfg = base();
    cfg.clean_copy_name = true;
    let task = f.plan(cfg);
    assert_eq!(task.summary.planned_delete, 1, "同内容副本应被删除");
    let db = Database::open(&task.directory).unwrap();
    let actions = db.actions_page(0, 100).unwrap();
    drop(db);
    assert!(
        actions
            .iter()
            .all(|a| a.target.as_deref() != Some(a.source.as_str())),
        "不得生成 source==target 的空转移动：{actions:?}"
    );
    Fixture::apply(&task);
    let dir = f.root.join("文档");
    // C-16：报告 (1).pdf / 报告 (2).pdf 的输出名都是 报告_1.pdf；保留者按消解结果落位。
    assert_eq!(
        fs::read(dir.join("报告_1.pdf")).unwrap(),
        b"A",
        "副本名清理后的保留者内容仍在"
    );
    assert_eq!(fs::read(dir.join("报告.pdf")).unwrap(), b"B");
    assert!(
        !exists_somewhere(&f.root, "报告 (2).pdf"),
        "同内容副本应被删除"
    );
}
// 注：原 hardlink_does_not_claim_permanent_bytes 与 hardlink_mode_preserves_aliases
// 考的是已按 R-02 整体删除的硬链接替换模式（config 无 duplicate_action、
// Summary 无 planned_link/linked）；多硬链接源的记账由
// permanent_bytes_exclude_hardlinked_sources 与 existing_hardlinks_not_counted_twice 锚定。
// 覆盖 R-03（默认递归包含全部子目录，可关）
#[test]
fn no_recursion_leaves_subdirectories_untouched() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("sub/b", b"same", 20);
    let mut cfg = base();
    cfg.recursive = false;
    assert_eq!(f.plan(cfg).summary.scanned, 1);
}
// 覆盖 S-07, C-10（无任务重放）
#[test]
fn finished_plan_cannot_be_replayed() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    Fixture::apply(&task);
    assert!(engine::apply(&task.directory, Context::default()).is_err());
}
#[test]
fn task_lock_prevents_second_task() {
    let f = Fixture::new();
    let _guard = fsutil::RootGuard::acquire(&f.state).unwrap();
    assert!(engine::prepare_at(&f.root, base(), Context::default(), &f.state).is_err());
}
// 覆盖 C-10（用户取消不算失败）
#[test]
fn user_cancelled_analysis_records_cancelled_status() {
    // 用户取消不是故障：任务状态必须写成 cancelled，否则事后检查任务库会把主动取消当成失败。
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    let context = Context::default();
    context.control.cancel();
    assert!(engine::prepare_at(&f.root, base(), context, &f.state).is_err());
    let directory = fs::read_dir(f.state.join("tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let db = Database::open(&directory).unwrap();
    assert_eq!(db.get::<String>("status").unwrap(), "cancelled");
}
#[test]
fn huge_sizes_use_u64() {
    assert_eq!(jchtools::model::bytes(10u64 << 40), "10.00 TiB");
    let f = Fixture::new();
    let db = Database::create(&f.state).unwrap();
    let snapshot = Snapshot {
        size: 12u64 << 40,
        modified_ns: 1,
        created_ns: None,
        identity: "mock".into(),
        links: 1,
    };
    db.insert_file("large.bin", "large.bin", "large.bin", &snapshot)
        .unwrap();
    assert_eq!(db.file(1).unwrap().snapshot.size, 12u64 << 40);
}
// 覆盖 C-03
#[test]
fn deterministic_keeper_ties() {
    let record = |id, rel: &str| FileRecord {
        id,
        rel: rel.into(),
        name: "x".into(),
        normalized: "x".into(),
        snapshot: Snapshot {
            size: 1,
            modified_ns: 10,
            created_ns: None,
            identity: id.to_string(),
            links: 1,
        },
        hash: None,
        cleanable: false,
    };
    assert!(rules::compare(&record(1, "a/x"), &record(2, "b/x"), KeepPolicy::Newest).is_lt());
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
// 平台门禁原因：创建符号链接在 Windows 需管理员/开发者模式特权，Unix 无需特权即可稳定构造；
// safe_join 拒绝链接穿越的断言只能在 Unix 下验证。
// 覆盖 S-04
#[cfg(unix)]
#[test]
fn symlink_not_followed_or_deleted() {
    let f = Fixture::new();
    let outside = f.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("a"), b"a").unwrap();
    std::os::unix::fs::symlink(&outside, f.root.join("link")).unwrap();
    assert!(fsutil::safe_join(&f.root, "link/a").is_err());
    assert_eq!(f.plan(base()).summary.scanned, 0);
}
// 覆盖 C-04, S-06（已是硬链接的文件不重复计入逻辑大小）
#[test]
fn existing_hardlinks_not_counted_twice() {
    let f = Fixture::new();
    let a = f.write("a", b"same", 10);
    if let Err(error) = fs::hard_link(&a, f.root.join("b")) {
        // 静默 return 会让测试永远绿：硬链接失败必须显式失败（FAT32/exFAT 不支持时请换 NTFS/tmpfs 环境）。
        eprintln!("hard_link failed: {error}; a={a:?}; root={:?}", f.root);
        panic!("无法创建硬链接，无法验证去重行为；请在支持硬链接的文件系统上运行测试");
    }
    let task = f.plan(base());
    assert_eq!(task.summary.candidate_bytes, 0);
    assert_eq!(task.summary.planned_delete, 0);
    // C-04：可靠标识 + 链接数证明已是同一物理文件，两个目录项都保留且不重复计数
    //（R-02 已删除硬链接替换模式，同物理文件的去重豁免是仅存的硬链接感知行为）。
    Fixture::apply(&task);
    assert!(
        exists_somewhere(&f.root, "a") && exists_somewhere(&f.root, "b"),
        "同一物理文件的两个目录项都不得被删除"
    );
}

// ===== 核心公共接口缺口补测（分卷识别）=====
// 覆盖 X-01, X-06（分卷识别口径）
#[test]
fn multipart_name_covers_volume_detection_corners() {
    assert!(rules::multipart_name("x.part1.rar"));
    assert!(rules::multipart_name("x.part99.rar"));
    assert!(!rules::multipart_name("x.rar"));
    assert!(rules::multipart_name("x.7z.001"));
    assert!(!rules::multipart_name("x.7z.002"));
}

// 任务库缺少有效全局锁目录记录时，apply 不得退回「当前位置推导」
// 在陌生位置（极端为盘根）创建 organizer.lock，必须拒绝执行。
// 覆盖 C-10（任务目录移位后不得凭旧计划执行）
#[test]
fn apply_without_valid_state_record_refuses_foreign_lock() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = elsewhere.path().join("moved-task");
    // 先把任务目录移到任意位置，再让记录的锁目录失效（改名挪走）：
    // recorded 过滤失败后只剩「当前位置推导」这一条回退路径。
    fs::rename(&task.directory, &moved).unwrap();
    fs::rename(&f.state, f.temp.path().join("state-gone")).unwrap();
    let derived = moved.parent().unwrap().parent().unwrap().to_path_buf();
    let result = engine::apply(&moved, Context::default());
    assert!(
        result.is_err(),
        "缺少有效锁记录且任务目录已移位：必须拒绝执行"
    );
    assert!(
        !derived.join("organizer.lock").exists(),
        "不得在陌生位置创建锁文件：{}",
        derived.display()
    );
}

// 记录失效但任务目录处于 tasks/ 布局（如搬到别的机器后放回新状态目录的
// tasks/ 下）时，apply 必须仍可执行——锁退回 tasks 的上一级（新状态目录）。
// 覆盖 C-10
#[test]
fn apply_with_invalid_record_but_tasks_layout_still_runs() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = elsewhere.path().join("tasks").join("moved-task");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(&task.directory, &moved).unwrap();
    fs::rename(&f.state, f.temp.path().join("state-gone")).unwrap();
    let result = engine::apply(&moved, Context::default()).unwrap();
    assert_eq!(result.summary.deleted, 1, "tasks/ 布局下任务照常执行");
    assert!(
        moved
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("organizer.lock")
            .is_file(),
        "锁落在新状态目录（tasks 的上一级）"
    );
}

// ===== H-11 并发与多进程访问：状态目录锁 / set_selected 竞态 / reserve_target =====
#[test]
fn prepare_and_apply_share_exclusive_state_lock() {
    // prepare 与 apply 共用同一 RootGuard：任一持有锁时，另一路必须失败（锁测试增强）。
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    {
        let _guard = fsutil::RootGuard::acquire(&f.state).unwrap();
        assert!(
            engine::prepare_at(&f.root, base(), Context::default(), &f.state).is_err(),
            "锁被持有时二次 prepare 必须失败"
        );
        assert!(
            engine::apply(&task.directory, Context::default()).is_err(),
            "锁被持有时 apply 必须失败"
        );
    }
    // 锁释放后 apply 可以正常完成
    Fixture::apply(&task);
}
// 覆盖 C-10（任务目录被移动后执行仍与 prepare 互斥）
#[test]
fn apply_after_task_dir_moved_still_uses_prepare_state_lock() {
    // 回归：apply 此前按「任务目录当前位置」推导锁位置；任务目录被移动到 state 之外后，
    // 旧计划的执行会与新的 prepare 失去互斥。prepare 把全局锁目录记进任务库后，
    // apply 必须优先锁记录的位置（记录失效时的回退语义见
    // apply_without_valid_state_record_refuses_foreign_lock：仅限 tasks/ 布局，否则拒绝）。
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let elsewhere = tempfile::tempdir().unwrap();
    let moved = elsewhere.path().join("moved-task");
    fs::rename(&task.directory, &moved).unwrap();
    {
        let _guard = fsutil::RootGuard::acquire(&f.state).unwrap();
        assert!(
            engine::apply(&moved, Context::default()).is_err(),
            "任务目录被移动后，apply 仍必须锁 prepare 记录的全局锁目录"
        );
    }
    // 记录位置与当前位置一致（移回原位）时正常流程不受影响
    fs::rename(&moved, &task.directory).unwrap();
    Fixture::apply(&task);
}
// 覆盖 C-10, C-11（执行开始后不得修改计划选择）
#[test]
fn set_selected_fails_once_apply_started() {
    // apply 启动后 status 变为 executing；此时修改选择必须失败（set_selected 竞态）。
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let context = Context::default();
    context.control.pause(true);
    let dir = task.directory.clone();
    let apply_handle = {
        let context = context.clone();
        std::thread::spawn(move || engine::apply(&dir, context))
    };
    // 轮询等待 apply 写入 status=executing（固定 sleep 在高负载/CI 抢占下可能误失败）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let db = Database::open(&task.directory).unwrap();
    loop {
        if db.get::<String>("status").unwrap() == "executing" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 apply 进入 executing 超时"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        db.get::<String>("status").unwrap(),
        "executing",
        "apply 已进入执行状态"
    );
    let id = db.actions_page(0, 10).unwrap().remove(0).id;
    assert!(
        db.set_selected(id, false).is_err(),
        "执行中不得修改计划选择"
    );
    context.control.pause(false);
    apply_handle.join().unwrap().unwrap();
}
// 覆盖 S-07, C-10（已结束的计划不可重放/改选）
#[test]
fn set_selected_rejected_after_apply_finished() {
    // apply 完成后 status=finished；set_selected 必须失败（不可重放旧计划的选择变更）。
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    Fixture::apply(&task);
    let db = Database::open(&task.directory).unwrap();
    assert_eq!(db.get::<String>("status").unwrap(), "finished");
    let id = db.actions_page(0, 10).unwrap().remove(0).id;
    assert!(db.set_selected(id, true).is_err());
}
// 覆盖 S-01
#[test]
fn reserve_target_is_case_insensitive_unique() {
    let f = Fixture::new();
    let db = Database::create(&f.state.join("reserve-db")).unwrap();
    assert!(
        db.reserve_target("Reports/Final.PDF", 1).unwrap(),
        "首次预留必须成功"
    );
    // 大小写折叠仅 Windows：大小写敏感文件系统上仅大小写不同的路径是不同目标。
    if cfg!(windows) {
        assert!(
            !db.reserve_target("reports/final.pdf", 2).unwrap(),
            "大小写不同的同一路径必须视为冲突"
        );
        assert!(!db.reserve_target("REPORTS/final.PDF", 3).unwrap());
    } else {
        assert!(
            db.reserve_target("reports/final.pdf", 2).unwrap(),
            "大小写敏感文件系统上不同大小写路径应可并存"
        );
        assert!(db.reserve_target("REPORTS/final.PDF", 3).unwrap());
    }
    assert!(
        db.reserve_target("reports/other.pdf", 4).unwrap(),
        "不同路径可以预留"
    );
    assert!(db.reserve_target("other.pdf", 5).unwrap());
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

// ===== 合同对齐回归（P-06 / C-02 / R-01 / E-05）：缺陷修复前必须失败 =====
// 覆盖 P-06
#[test]
fn no_cli_binary_is_declared() {
    let manifest = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    assert!(
        !manifest.contains("jchtools-cli"),
        "P-06：不得声明命令行二进制 jchtools-cli"
    );
    assert!(
        !manifest.contains("src/bin/cli.rs"),
        "P-06：不得保留 CLI 入口文件声明"
    );
}
// 覆盖 P-06
#[test]
fn no_csv_report_export_remains() {
    let db_src = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/db.rs")).unwrap();
    assert!(
        !db_src.contains("export_csv"),
        "P-06：任务库不得保留 CSV 报告导出 API"
    );
}
// 覆盖 C-02（MUST NOT 提供同名不同内容的淘汰开关）
#[test]
fn config_has_no_version_elimination_switches() {
    let value = serde_json::to_value(Config::default()).unwrap();
    for key in [
        "same_name_same_size",
        "same_size_keep",
        "same_name_different_size",
        "different_size_keep",
        "conflict_scope_directory",
    ] {
        assert!(
            value.get(key).is_none(),
            "C-02：配置不得提供版本取舍开关：{key}"
        );
    }
}
// 覆盖 R-01, C-02
#[test]
fn rules_table_has_no_version_or_removed_switches() {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../resources/rules.json")).unwrap();
    let rows = schema.as_array().unwrap();
    // R-01/C-08：规则面板不得再出现已移除的开关行，且三类清理的删除方式必须可独立配置。
    for key in [
        "same_name_same_size",
        "same_size_keep",
        "conflict_scope_directory",
        "archive_delete",
        "extract_conflict",
        "conflict_delete",
        "clean_empty_dirs",
        "nested_archives",
        "cleanup_delete",
    ] {
        assert!(
            rows.iter().all(|r| r["key"].as_str() != Some(key)),
            "规则面板不得提供已移除的开关行：{key}"
        );
    }
    for key in ["junk_delete", "temp_delete", "zero_delete"] {
        assert!(
            rows.iter().any(|r| r["key"].as_str() == Some(key)),
            "C-08：三类清理各自独立覆盖删除方式，规则面板必须有对应行：{key}"
        );
    }
}

// 覆盖 P-06：不提供整理报告及任何形式的报告导出——完成文案不得再引导用户「导出报告」。
#[test]
fn completion_status_text_has_no_export_reference() {
    let gui_src = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/gui.rs")).unwrap();
    assert!(
        !gui_src.contains("导出报告"),
        "P-06：界面文案不得引导使用已移除的报告导出"
    );
}

// ===== L6 control::pause / checkpoint 暂停语义 =====
// 覆盖 C-10（暂停/取消在安全边界生效）
#[test]
fn pause_defers_checkpoint_until_resume() {
    let ctl = std::sync::Arc::new(Control::default());
    ctl.checkpoint().unwrap(); // 未暂停时 checkpoint 应直接通过
    ctl.pause(true);
    assert!(ctl.is_paused());
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    // started 握手：worker 在调用 checkpoint 前先报到，避免调度延迟让「无 done」假通过
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let worker = {
        let c = ctl.clone();
        std::thread::spawn(move || {
            let _ = started_tx.send(());
            c.checkpoint().unwrap();
            let _ = done_tx.send(());
        })
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("worker 应已进入 checkpoint 前置");
    // 暂停期间 checkpoint 不应完成
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_millis(150))
            .is_err(),
        "暂停期间 checkpoint 不应返回"
    );
    ctl.pause(false);
    done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("恢复后 checkpoint 应完成");
    worker.join().unwrap();
}
// 覆盖 C-10
#[test]
fn cancel_while_paused_makes_checkpoint_fail() {
    let ctl = std::sync::Arc::new(Control::default());
    ctl.pause(true);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    // started 握手：worker 报到后再 cancel，覆盖「已进入 checkpoint 暂停环」路径
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let worker = {
        let c = ctl.clone();
        std::thread::spawn(move || {
            let _ = started_tx.send(());
            let failed = c.checkpoint().is_err();
            let _ = done_tx.send(failed);
        })
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("worker 应已进入 checkpoint 前置");
    // started 只表示即将调用 checkpoint；短暂等待让 worker 进入暂停等待环后再取消
    std::thread::sleep(std::time::Duration::from_millis(20));
    ctl.cancel();
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        "暂停中取消必须让 checkpoint 失败"
    );
    worker.join().unwrap();
}

// ===== 合同覆盖补齐（AGENTS 3.3：每个合同条目至少被一个测试引用）=====

// 覆盖 C-03, R-02, S-08（保留规则各可选项的实际选择结果与决胜链）
#[test]
fn keeper_policy_variants_pick_expected_keepers() {
    // C-03（R-02 后）：只有最新/最旧/最短名称三种；同内容必然等大小，
    // 不再提供最大/最小（Largest/Smallest 已随硬链接模式删除）。
    for (policy, kept, deleted) in [
        (KeepPolicy::Oldest, "aa.txt", "zzzz.txt"),
        (KeepPolicy::Newest, "zzzz.txt", "aa.txt"),
        (KeepPolicy::ShortestName, "aa.txt", "zzzz.txt"),
    ] {
        let f = Fixture::new();
        f.write("aa.txt", b"same", 100);
        f.write("zzzz.txt", b"same", 200);
        let mut cfg = base();
        cfg.keep_duplicate = policy;
        let task = f.plan(cfg);
        assert_eq!(task.summary.planned_delete, 1, "{policy:?}");
        let db = Database::open(&task.directory).unwrap();
        let actions = db.actions_page(0, 10).unwrap();
        drop(db);
        assert_eq!(actions[0].source, deleted, "{policy:?} 应删除 {deleted}");
        assert!(
            actions[0].keeper.as_ref().is_some_and(|k| k.0 == kept),
            "{policy:?} 应保留 {kept}"
        );
        Fixture::apply(&task);
        assert!(
            exists_somewhere(&f.root, kept) && !exists_somewhere(&f.root, deleted),
            "{policy:?} 执行结果与计划一致"
        );
    }
}

// 覆盖 C-06（大文件单独归类，默认阈值 1 GiB）
#[test]
fn large_files_classified_into_own_directory() {
    let f = Fixture::new();
    // 逻辑大小达 1 GiB 的稀疏文件：set_len 只改元数据，不占实际磁盘
    let big = f.root.join("big.bin");
    let handle = fs::File::create(&big).unwrap();
    handle.set_len(1 << 30).unwrap();
    drop(handle);
    filetime::set_file_mtime(&big, filetime::FileTime::from_unix_time(100, 0)).unwrap();
    f.write("small.txt", b"s", 100);
    let mut cfg = base();
    cfg.large_files = true; // large_threshold_bytes 默认 1 GiB
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    let big_move = moves
        .iter()
        .find(|a| a.source == "big.bin")
        .expect("大文件应有归类移动计划");
    assert_eq!(
        big_move.target.as_deref(),
        Some("大文件/big.bin"),
        "C-06：大文件进入「大文件」大类并按一级结构落位"
    );
    assert!(
        moves.iter().all(|a| {
            a.target
                .as_deref()
                .is_none_or(|t| !t.starts_with("大文件") || a.source == "big.bin")
        }),
        "小文件不参与大文件归类：{moves:?}"
    );
    assert!(
        moves.iter().any(|a| a.source == "small.txt"
            && a.target.as_deref().is_some_and(|t| t.starts_with("文档/"))),
        "小文件仍按普通大类归类：{moves:?}"
    );
}
// 注：原 flatten_single_child_collapses_levels 与 merge_directories_joins_same_name_directories
// 考的是已删除的 flatten_single_child / merge_directories 归类形态（R-02）；来源目录
// 在 C-17/C-18 中只作为冲突消解前缀参与，由 classify_shape.rs 锚定。

// 覆盖 C-08（系统附属文件清理默认开启；__MACOSX 目录内容一并清理）
#[test]
fn junk_system_files_are_planned_for_cleanup_by_default() {
    let f = Fixture::new();
    for name in ["Thumbs.db", "desktop.ini", "keep.txt"] {
        f.write(name, b"x", 10);
    }
    fs::create_dir_all(f.root.join("__MACOSX")).unwrap();
    f.write("__MACOSX/junk.dat", b"x", 10);
    let cfg = base(); // clean_junk 默认 true
    let task = f.plan(cfg);
    assert_eq!(
        task.summary.planned_delete, 3,
        "Thumbs.db / desktop.ini / __MACOSX 内容应入清理计划"
    );
    Fixture::apply(&task);
    assert!(
        exists_somewhere(&f.root, "keep.txt"),
        "普通文件不受清理影响"
    );
    assert!(
        !f.root.join("Thumbs.db").exists() && !f.root.join("__MACOSX/junk.dat").exists(),
        "系统附属文件应被清理"
    );
}

// 覆盖 C-08（零字节文件清理默认关，开启后入计划）
#[test]
fn zero_byte_cleanup_follows_its_switch() {
    let f = Fixture::new();
    f.write("empty.dat", b"", 10);
    f.write("full.dat", b"x", 10);
    let mut cfg = base();
    assert_eq!(
        f.plan(cfg.clone()).summary.planned_delete,
        0,
        "默认关闭：零字节文件不得入清理计划"
    );
    cfg.clean_zero = true;
    assert_eq!(f.plan(cfg).summary.planned_delete, 1);
}

// 覆盖 C-08（NFC 规范化 + 合并连续空白，默认开；不删正常字符）
#[test]
fn normalize_names_collapses_whitespace_and_applies_nfc() {
    let f = Fixture::new();
    // U+0065 + U+0301（分解形式）应规范为 U+00E9（合成形式）；双空格合并为单空格、
    // 主体首尾空白去除（扩展名前的空格属于主体尾部，一并去掉）。
    let raw_name = "cafe\u{0301}  report .txt";
    f.write(raw_name, b"payload", 10);
    let cfg = base();
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    let expected = "文档/café report.txt".to_string();
    assert_eq!(moves.len(), 1, "规范化默认开启，改名应入移动计划");
    assert_eq!(moves[0].target.as_deref(), Some(expected.as_str()));
    Fixture::apply(&task);
    assert!(
        f.root.join(&expected).exists(),
        "执行后应为规范化名（并按 C-05 归类落位）"
    );
    assert!(!f.root.join(raw_name).exists(), "旧分解形式名称不得残留");
}

// 覆盖 C-08（按内容签名修正错误扩展名：改名列入计划可见）
#[test]
fn fix_extension_plans_rename_to_detected_type() {
    let f = Fixture::new();
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
    f.write("photo.txt", &png, 10);
    let mut cfg = base();
    cfg.detect_type = true;
    cfg.fix_extension = true;
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    let expected = "图片/photo.png".to_string();
    assert_eq!(moves.len(), 1, "错误扩展名应产生改名计划");
    assert_eq!(
        moves[0].target.as_deref(),
        Some(expected.as_str()),
        "计划里可见改名为内容识别出的真实类型（并按修正后扩展名归类到「图片」）"
    );
    Fixture::apply(&task);
    assert!(f.root.join(&expected).exists());
}

// 覆盖 C-08（只修正「错误」扩展名：同类容器与别名扩展名不得按更粗的识别结果改粗）
#[test]
fn fix_extension_keeps_specialized_container_and_alias_extensions() {
    // 最小 ZIP 头：签名 + 26 字节本地文件头 + 条目名（infer 只看 0x1E 起的条目名）。
    fn minimal_zip_entry(entry: &str) -> Vec<u8> {
        let mut bytes = Vec::from(*b"PK\x03\x04");
        bytes.extend_from_slice(&[0u8; 26]);
        bytes.extend_from_slice(entry.as_bytes());
        bytes
    }
    let f = Fixture::new();
    // 条目名为 word/：infer 对 OOXML 家族（含宏启用/模板变体）只识别到 docx 这一粒度，
    // 此前 dotx/docm 会被当作「错误扩展名」改名成 docx。两个 OOXML 样本内嵌不同条目，
    // 避免互为同内容副本被去重删除（本用例只考扩展名修正口径）。
    f.write("报告.docm", &minimal_zip_entry("word/document.xml"), 10);
    f.write("模板.dotx", &minimal_zip_entry("word/footer.xml"), 15);
    // 条目名非 OOXML：识别结果只有容器类型 zip，whl 的扩展名本身是正确信息。
    f.write("包.whl", &minimal_zip_entry("data.txt"), 18);
    // gzip 流：识别结果只有容器类型 gz（svgz 是压缩 SVG）。
    f.write("图标.svgz", &[0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00], 19);
    // 同义别名：htm→html、mid→midi 都不算「错误扩展名」。
    f.write(
        "index.htm",
        b"<!DOCTYPE html><html><body>hi</body></html>",
        20,
    );
    f.write("歌曲.mid", b"MThd\x00\x00\x00\x06", 22);
    let mut cfg = base();
    cfg.detect_type = true;
    cfg.fix_extension = true;
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    // C-05 归类恒开启：这些文件都有归类移动；断言收敛为「文件名逐字节不变」——
    // 正确扩展名不得按更粗的识别结果改名。
    assert!(
        moves.iter().all(|a| {
            let source_name = Path::new(&a.source)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let target_name = a
                .target
                .as_deref()
                .and_then(|t| Path::new(t).file_name())
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            source_name == target_name
        }),
        "这些都是正确扩展名，不得按更粗的识别结果改名：{moves:?}"
    );
    assert_eq!(moves.len(), 6, "六个文件都应只有归类移动：{moves:#?}");
    for name in [
        "报告.docm",
        "模板.dotx",
        "包.whl",
        "图标.svgz",
        "index.htm",
        "歌曲.mid",
    ] {
        assert!(exists_somewhere(&f.root, name), "{name} 必须保留原名");
    }
}

// 覆盖 C-08 / 附录 B（已知格式后缀 + 承诺可修正的 ZIP / 7z / RAR 内容时改名并归「压缩包」；
// 无法由签名证明后缀错误的后缀与 X-10 卷尾后缀保留）
#[test]
fn fix_extension_corrects_promised_containers_but_not_volume_tails() {
    // 最小 ZIP 头：签名 + 26 字节本地文件头 + 条目名（infer 只看 0x1E 起的条目名）。
    fn minimal_zip_entry(entry: &str) -> Vec<u8> {
        let mut bytes = Vec::from(*b"PK\x03\x04");
        bytes.extend_from_slice(&[0u8; 26]);
        bytes.extend_from_slice(entry.as_bytes());
        bytes
    }
    let f = Fixture::new();
    // 已知格式后缀 + 真实压缩包内容：识别结果为 zip / 7z / rar，应按识别结果改名。
    f.write("备份.txt", &minimal_zip_entry("data.txt"), 10);
    f.write(
        "数据.mp4",
        &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 0, 0, 0],
        12,
    );
    f.write("老包.png", b"Rar!\x1A\x07\x00payload", 14);
    // X-10 数字尾卷：整组后缀不可拆，识别到 zip 也不得改名（内容与其它样本不同，避免去重）。
    f.write("分卷.zip.001", &minimal_zip_entry("part.bin"), 16);
    // 附录 A 未列出的后缀：识别到的只是外层容器，无法确定真实类型，保留原后缀（C-08）。
    f.write("图纸.odg", &minimal_zip_entry("content.xml"), 18);
    let mut cfg = base();
    cfg.detect_type = true;
    cfg.fix_extension = true;
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    let planned: Vec<(String, String)> = moves
        .iter()
        .map(|a| (a.source.clone(), a.target.clone().unwrap_or_default()))
        .collect();
    for (source, expected) in [
        ("备份.txt", "压缩包/备份.zip".to_string()),
        ("数据.mp4", "压缩包/数据.7z".to_string()),
        ("老包.png", "压缩包/老包.rar".to_string()),
        ("分卷.zip.001", "压缩包/分卷.zip.001".to_string()),
        ("图纸.odg", "其他/图纸.odg".to_string()),
    ] {
        let actual = planned.iter().find(|(s, _)| s == source).map(|(_, t)| t);
        assert_eq!(
            actual.map(String::as_str),
            Some(expected.as_str()),
            "{source} 应计划为 {expected}；实际 {planned:#?}"
        );
    }
}

// 平台门禁原因：用例需要 junction（mklink /J）复现「分类目录名被重定向目录占用」；
// 非 Windows 无 reparse point 语义（Unix 侧链接拒绝穿越由 symlink_not_followed_or_deleted 覆盖）。
#[cfg(windows)]
#[test]
fn junction_named_like_category_skips_item_instead_of_failing_plan() {
    let f = Fixture::new();
    let outside = f.temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(f.root.join("图片"))
        .arg(&outside)
        .status()
        .unwrap();
    assert!(status.success(), "无法创建 junction，用例前置条件不成立");
    f.write("photo.png", b"not really a png", 10);
    // 默认规则按大类归类：photo.png 的目标是 图片/photo.png，大类段正是 junction。
    let task = f.plan(Config::default());
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    let events = db.event_page(i64::MAX, 20).unwrap();
    drop(db);
    assert!(
        moves.is_empty(),
        "目标路径穿过链接的文件必须按项跳过，不得整次分析失败：{moves:?}"
    );
    assert!(f.root.join("photo.png").exists(), "跳过的文件必须原地保留");
    assert!(
        events.iter().any(|event| event.contains("目标路径不可用")),
        "按项跳过必须留日志：{events:?}"
    );
}

// 覆盖 C-10（计划生成后目录被移动：执行必须拒绝，不得把动作落到别处）
#[test]
fn apply_refuses_when_root_directory_moved_after_plan() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    let moved = f.temp.path().join("data-moved");
    fs::rename(&f.root, &moved).unwrap();
    let result = engine::apply(&task.directory, Context::default());
    let error = format!("{:#}", result.err().unwrap());
    assert!(
        error.contains("无法访问目标目录") || error.contains("目录位置已改变"),
        "执行前必须发现目录移位：{error}"
    );
    assert!(
        moved.join("a").exists() && moved.join("b").exists(),
        "新位置内容不得被触碰"
    );
}

// 平台门禁原因：normalize_root 对 Windows 安装目录/盘根的拒绝分支依赖 Windows 环境变量
// 与盘符路径语义；Unix 侧只验证「根目录不可整理」。
// 覆盖 S-05（始终拒绝整理 Windows 安装/系统数据目录与磁盘根目录，无开关可放开）
#[cfg(windows)]
#[test]
fn normalize_root_rejects_protected_locations() {
    let system_root = std::env::var("SystemRoot").unwrap();
    assert!(
        fsutil::normalize_root(Path::new(&system_root)).is_err(),
        "不得整理 Windows 系统目录"
    );
    let program_files = std::env::var("ProgramFiles").unwrap();
    assert!(
        fsutil::normalize_root(Path::new(&program_files)).is_err(),
        "不得整理程序安装目录"
    );
    assert!(
        fsutil::normalize_root(Path::new(r"C:\")).is_err(),
        "不得整理磁盘根目录"
    );
}
// 覆盖 S-05
#[cfg(not(windows))]
#[test]
fn normalize_root_rejects_root_directory() {
    assert!(
        fsutil::normalize_root(Path::new("/")).is_err(),
        "不得整理根目录"
    );
}

// 覆盖 S-06（摘要只按逻辑大小报告已永久删除的项，MUST NOT 声称等于文件系统实际释放量）
#[test]
fn summary_counts_permanent_bytes_only() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base()); // global_delete = Permanent
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.deleted, 1);
    assert_eq!(result.summary.permanent_bytes, 4, "「same」=4 字节");
}

// 覆盖 R-01（规则仅会话内生效：状态目录不得出现独立设置文件）
#[test]
fn prepare_writes_no_standalone_settings_file() {
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    let _ = f.plan(base());
    let entries: Vec<String> = fs::read_dir(&f.state)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries
            .iter()
            .all(|name| name == "tasks" || name == "organizer.lock" || name == "hash-cache.sqlite3"),
        "状态目录只应含任务库、锁文件与跨运行哈希缓存（C-13），不得出现独立设置文件（不落盘/无导入导出）：{entries:?}"
    );
}

// 覆盖 P-03（完全离线：依赖清单与源码不得引入联网能力）
#[test]
fn offline_sources_have_no_network_capabilities() {
    let manifest = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    for crate_name in [
        "reqwest",
        "ureq",
        "hyper",
        "curl",
        "isahc",
        "surf",
        "attohttpc",
        "websocket",
        "ftp",
        "tokio-tungstenite",
    ] {
        assert!(
            !manifest.contains(crate_name),
            "P-03：依赖清单不得引入联网 crate：{crate_name}"
        );
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for entry in [
        "src/main.rs",
        "src/lib.rs",
        "src/engine.rs",
        "src/gui.rs",
        "src/platform.rs",
        "src/db.rs",
        "src/process.rs",
        "build.rs",
    ] {
        let text = fs::read_to_string(manifest_dir.join(entry)).unwrap();
        for needle in ["TcpStream", "UdpSocket", "lookup_host"] {
            assert!(
                !text.contains(needle),
                "P-03：{entry} 不得出现网络 API：{needle}"
            );
        }
    }
}

// 覆盖 X-01：自动解压只认用户意义上的归档文件（白名单）；安装资源、系统镜像、
// 语言包、安装包、文档容器即使 7-Zip 能打开，也不得自动解压。
#[test]
fn archive_name_only_accepts_the_user_archive_whitelist() {
    for name in [
        "a.zip",
        "a.7Z",
        "b.tar",
        "b.gz",
        "b.bz2",
        "b.xz",
        "b.zst",
        "b.lzma",
        "b.z",
        "b.tgz",
        "b.tbz2",
        "b.txz",
        "b.tzst",
        "f.tar.gz",
        "f.tar.bz2",
        "f.tar.xz",
        "f.tar.zst",
        "f.tar.lzma",
        "f.tar.z",
        "d.7z.001",
        "d.zip.001",
        "e.part1.rar",
    ] {
        assert!(
            rules::archive_name(name),
            "{name} 在白名单内，应识别为压缩包"
        );
    }
    for name in [
        "a.txt",
        "a.png",
        "a.zipx",
        "e.part2.rar",
        "archive",
        // 引擎没有解码器的流格式：放行只会把文件一律推进「解压失败」，故不碰
        "b.lz4",
        "b.lz",
        "b.br",
        "f.tar.lz4",
        "f.tar.lz",
        "f.tar.br",
        // X-09：白名单外格式的分卷与配不上主包的孤立编号文件同样不碰
        "windows.iso.001",
        "report.docx.001",
        "setup.msi.001",
        "data.001",
        "report.z01",
        "report.r00",
        // Windows 安装资源与安装包
        "data.cab",
        "setup.msi",
        "app.msix",
        "bundle.msixbundle",
        // 磁盘 / 系统镜像
        "windows.iso",
        "boot.wim",
        "install.esd",
        "disk.img",
        "disk.vhd",
        "disk.vhdx",
        // Java / Android / Python / .NET / 扩展包
        "library.jar",
        "service.war",
        "module.ear",
        "app.apk",
        "app.aab",
        "wheel.whl",
        "pack.nupkg",
        "ext.vsix",
        "addon.crx",
        "addon.xpi",
        // Apple 软件包与旧式容器
        "app.ipa",
        "installer.pkg",
        "disk.dmg",
        "legacy.lzh",
        "archive.cpio",
        // Office / OpenDocument / 电子书容器
        "report.docx",
        "sheet.xlsx",
        "slides.pptx",
        "macro.docm",
        "macro.xlsm",
        "doc.odt",
        "book.epub",
        // 可执行文件与自解压包
        "setup.exe",
        "self-extract.com",
    ] {
        assert!(
            !rules::archive_name(name),
            "{name} 不在自动解压白名单内，不得被当作压缩包处理"
        );
    }
}

// 覆盖 X-07（隔离目录中的包被用户移出后，下次扫描自然重试）
#[test]
fn quarantine_move_out_is_rescanned() {
    let f = Fixture::new();
    f.write("解压失败/fix.zip", b"not a zip", 10);
    let cfg = base();
    assert_eq!(
        engine::count_archives(&f.root, &cfg).unwrap(),
        0,
        "隔离目录内的包不计入确认框计数"
    );
    fs::rename(f.root.join("解压失败/fix.zip"), f.root.join("fix.zip")).unwrap();
    assert_eq!(
        engine::count_archives(&f.root, &cfg).unwrap(),
        1,
        "用户补救（移出隔离目录）后下次扫描自然重试"
    );
}

// 覆盖 R-03, S-04（性能默认值：哈希线程 6、磁盘预留 1 GiB、递归默认包含子目录、默认不缩小范围）
#[test]
fn performance_defaults_match_contract() {
    let cfg = Config::default();
    assert_eq!(cfg.hash_workers, 6, "R-03：默认 6（1–16 可配）");
    assert_eq!(
        cfg.reserve_bytes,
        1024 * 1024 * 1024,
        "磁盘预留默认 1 GiB（字节口径）"
    );
    assert!(cfg.recursive);
    assert!(cfg.include_hidden, "S-04：默认覆盖隐藏资料");
    assert!(cfg.include_system, "S-04：默认覆盖系统属性资料");
    assert_eq!(
        cfg.exclusions, "",
        "S-04：默认不设 glob 排除；Git 整树排除由引擎无条件执行，不靠默认排除表"
    );
}
// 覆盖 C-07, H-07, X-04, X-05, R-02（已移除的开关不得复活）
#[test]
fn removed_extraction_and_cleanup_switches_are_gone() {
    let value = serde_json::to_value(Config::default()).unwrap();
    for key in [
        "archive_delete",
        "extract_conflict",
        "conflict_delete",
        "clean_empty_dirs",
        "nested_archives",
        "cleanup_delete",
        // 2026-09 归类引擎重写（R-02/R-04）删除的键：旧任务库 JSON 剥除后加载，
        // 不得在配置里复活。
        "classify",
        "output_dir",
        "preserve_structure",
        "custom_categories",
        "merge_directories",
        "flatten_single_child",
        "duplicate_action",
        "duplicate_delete",
        "max_unpacked_gib",
        "max_file_gib",
        "large_threshold_gib",
        "reserve_gib",
    ] {
        assert!(value.get(key).is_none(), "已移除的配置不得复活：{key}");
    }
    for key in ["junk_delete", "temp_delete", "zero_delete"] {
        assert!(
            value.get(key).is_some(),
            "C-08：三类清理必须各自独立覆盖删除方式：{key}"
        );
    }
}

// 覆盖 C-01（分析阶段 MUST NOT 改动任何文件：清扫只能发生在执行确认之后）
#[test]
fn analysis_phase_keeps_stale_link_temps_until_execution() {
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    let stale = f.root.join(".jchtools-link-deadbeef");
    if let Err(error) = fs::hard_link(f.root.join("a.txt"), &stale) {
        panic!("无法创建硬链接，无法验证残留清扫时序；请在支持硬链接的文件系统上运行测试：{error}");
    }
    filetime::set_file_mtime(&stale, filetime::FileTime::from_unix_time(0, 0)).unwrap();
    let task = f.plan(base());
    assert!(
        stale.exists(),
        "C-01：分析（只读）阶段不得删除任何文件——即使是本工具的崩溃残留"
    );
    assert_eq!(task.summary.scanned, 1, "残留被扫描剪枝，不影响计数");
    // 执行确认后（apply）才允许清扫；apply 的执行前清理承担该职责。
    Fixture::apply(&task);
    assert!(!stale.exists(), "执行阶段应清扫过期残留");
    assert!(
        exists_somewhere(&f.root, "a.txt"),
        "清扫不得影响 keeper 本体"
    );
}

// 覆盖 S-06（永久删除字节与 candidate_bytes 同口径：多硬链接的源不重复计入逻辑大小）
#[test]
fn permanent_bytes_exclude_hardlinked_sources() {
    let f = Fixture::new();
    let a = f.write("a", b"same", 10);
    if let Err(error) = fs::hard_link(&a, f.root.join("c")) {
        panic!("无法创建硬链接：{error}；请在支持硬链接的文件系统上运行测试");
    }
    filetime::set_file_mtime(&a, filetime::FileTime::from_unix_time(10, 0)).unwrap();
    f.write("b", b"same", 20); // 更新 → 保留者
    let task = f.plan(base());
    assert!(
        task.summary.planned_delete >= 1,
        "a/c 与 b 内容相同应生成删除计划"
    );
    assert_eq!(task.summary.candidate_bytes, 0, "计划侧已排除多链接文件");
    let result = Fixture::apply(&task);
    assert!(
        result.summary.deleted >= 1,
        "删除确实发生了（项数记账不受影响）"
    );
    assert_eq!(
        result.summary.permanent_bytes, 0,
        "被删副本仍有其他硬链接持有内容，永久删除字节不得计入"
    );
}

// 注：原 flatten_with_classification_is_idempotent_across_rounds 考的是已删除的
// flatten_single_child 归类形态（R-02）；归类目录内文件的跨轮稳定性由
// classify_shape.rs 的 in_place_normalized_collision_resolves_once_and_stays 等锚定。

// 覆盖 C-12, S-03（执行阶段 MUST NOT 读取文件内容：去重删除只依据分析期算出的整文件哈希）
#[test]
fn execution_phase_reads_no_file_content() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    assert_eq!(task.summary.planned_delete, 1, "同内容同尺寸应生成删除计划");
    let context = Context::default();
    let control = context.control.clone();
    let result = engine::apply(&task.directory, context).unwrap();
    assert_eq!(result.summary.deleted, 1);
    assert_eq!(
        control.read_bytes.load(Ordering::Relaxed),
        0,
        "执行阶段不得读取文件内容（C-12/S-03）：删除只依据分析期算出的整文件哈希"
    );
}

// 覆盖 S-02（旧任务库里的 recycle 取值按永久删除读取，不得让旧配置整体判非法）
#[test]
fn legacy_recycle_mode_loads_as_permanent() {
    let mut value = serde_json::to_value(Config::default()).unwrap();
    // duplicate_delete 已删除（C-04：副本处置随全局）；回收站历史值只可能出现在
    // 全局与三类清理的删除方式键上。
    for key in ["global_delete", "junk_delete", "temp_delete", "zero_delete"] {
        value[key] = serde_json::json!("recycle");
    }
    let cfg = Config::from_json_text(&value.to_string()).unwrap();
    assert_eq!(cfg.global_delete, DeleteMode::Permanent);
    assert_eq!(
        cfg.junk_delete.resolve(cfg.global_delete),
        DeleteMode::Permanent
    );
    assert_eq!(
        cfg.temp_delete.resolve(cfg.global_delete),
        DeleteMode::Permanent
    );
    assert_eq!(
        cfg.zero_delete.resolve(cfg.global_delete),
        DeleteMode::Permanent
    );
}

// 覆盖 C-02（不同名去重默认关闭：不同名的同内容文件默认不得被自动淘汰，可手动开启）
#[test]
fn different_names_not_deduped_by_default() {
    let cfg = Config {
        clean_copy_name: false,
        global_delete: DeleteMode::Permanent,
        ..Config::default()
    };
    assert!(!cfg.dedup_other_names, "C-02：不同名去重默认关闭");
    let f = Fixture::new();
    f.write("a.txt", b"same", 10);
    f.write("z.txt", b"same", 20);
    let task = f.plan(cfg.clone());
    assert_eq!(
        task.summary.planned_delete, 0,
        "默认配置下不同名的同内容文件不得被自动淘汰（C-02）"
    );
    // 手动开启后照旧生效：同一份数据必须产出删除计划。
    let mut on = cfg;
    on.dedup_other_names = true;
    let f2 = Fixture::new();
    f2.write("a.txt", b"same", 10);
    f2.write("z.txt", b"same", 20);
    assert_eq!(
        f2.plan(on).summary.planned_delete,
        1,
        "开启不同名去重后应生成删除计划"
    );
}

// ===== 本轮合同对齐回归（先红后绿：空目录强制清理 / Git 整树 / 自嵌套 / 复合扩展名 / 分类删除方式）=====

// 覆盖 C-07（空目录清理是强制步骤：不受全局/清理删除方式影响，也没有开关）
#[test]
fn empty_cleanup_ignores_file_delete_modes() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("empty")).unwrap();
    f.write("keep.txt", b"payload", 10);
    let mut cfg = base();
    cfg.global_delete = DeleteMode::Keep;
    let task = f.plan(cfg);
    assert_eq!(
        task.summary.planned_empty, 1,
        "空目录清理不属于文件删除方式可关的项目（C-07）"
    );
    Fixture::apply(&task);
    assert!(
        !f.root.join("empty").exists(),
        "全局保留也不得阻止空目录清理"
    );
    assert!(exists_somewhere(&f.root, "keep.txt"));
}

// 覆盖 C-07, R-03（递归关闭：未下钻的子目录内容未知，不得把猜测写成计划行；
// 实际为空的目录仍按 H-05 在收尾清理，有内容的子目录一律不碰）
#[test]
fn recursion_disabled_never_plans_empty_subdirectories() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("empty")).unwrap();
    f.write("full/x.txt", b"payload", 10);
    let mut cfg = base();
    cfg.recursive = false;
    let task = f.plan(cfg);
    assert_eq!(task.summary.scanned, 0, "递归关闭：只处理根目录自身的条目");
    assert_eq!(
        task.summary.planned_empty, 0,
        "未下钻的子目录内容未知，不得把猜测写成计划行"
    );
    Fixture::apply(&task);
    assert!(
        !f.root.join("empty").exists(),
        "执行期复查后确认实际为空的目录照常清理（H-05）"
    );
    assert!(
        f.root.join("full/x.txt").exists() && f.root.join("full").exists(),
        "有内容的子目录及其内容不得被触碰"
    );
}

// 覆盖 C-07, S-04（排除树不参与空目录清理）
#[test]
fn excluded_tree_is_not_cleaned_as_empty() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("protected/nested")).unwrap();
    f.write("keep.txt", b"payload", 10);
    let mut cfg = base();
    cfg.exclusions = "protected/**".into();
    let task = f.plan(cfg);
    assert_eq!(task.summary.scanned, 1);
    assert_eq!(task.summary.planned_empty, 0, "排除树不得进入空目录计划");
    Fixture::apply(&task);
    assert!(f.root.join("protected/nested").exists());
    assert!(exists_somewhere(&f.root, "keep.txt"));
}
// 注：原 category_merge_does_not_nest_category_directory_inside_itself、
// category_merge_flatten_rerun_is_idempotent 与 flatten_allocation_preserves_compound_extension
// 考的是已删除的 merge_directories / flatten_single_child / (N) 序号回退归类形态（R-02）；
// 同名冲突的复合扩展名保留由 rules::digest_candidate 与 classify_shape.rs 锚定，
// “来源目录名与大类同名不再套层”由 C-18 的来源链剔除规则覆盖。

// 覆盖 H-06, C-14（Git 工作树整体移入集合；归类不写进移动后的 Git 树内部）
#[test]
fn classification_target_never_enters_git_tree() {
    let f = Fixture::new();
    // 与大类同名的 Git 工作树：C-14 先把项目整体移入「Git项目集合」，
    // 归类目录随后复用腾出的「图片」名字——但绝不写入移动后的 Git 树内部。
    f.write(
        "图片/.git/config",
        b"[core]\nrepositoryformatversion = 0\n",
        10,
    );
    f.write("photo.png", b"photo", 20);
    f.write("doc/a.pdf", b"pdf", 30);
    let cfg = base();
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    let git_moves = db
        .actions_page_filtered(0, 100, Some("move"))
        .unwrap()
        .into_iter()
        .filter(|a| a.source == "图片")
        .collect::<Vec<_>>();
    let events = db.event_page(i64::MAX, 50).unwrap();
    drop(db);
    assert_eq!(
        task.summary.scanned, 2,
        "Git 整树排除：只应扫描 photo.png 与 doc/a.pdf"
    );
    assert_eq!(task.summary.planned_git, 1, "Git 项目整体移入集合（C-14）");
    assert_eq!(git_moves.len(), 1);
    assert_eq!(
        git_moves[0].target.as_deref(),
        Some("Git项目集合/图片"),
        "项目目录名不变，整树移入集合"
    );
    // 归类目标不进入「Git项目集合」内部（H-06：不写入项目树）。
    assert!(
        moves.iter().filter(|a| a.source != "图片").all(|a| a
            .target
            .as_deref()
            .is_none_or(|t| !t.starts_with("Git项目集合/"))),
        "归类目标不得写进 Git 集合内部：{moves:?}"
    );
    assert!(
        events.iter().any(|event| event.contains("Git")),
        "Git 整树排除必须留日志：{events:?}"
    );
    Fixture::apply(&task);
    assert!(
        f.root.join("图片/photo.png").exists(),
        "归类目录复用项目腾出的名字（C-14）"
    );
    assert!(
        f.root.join("Git项目集合/图片/.git/config").exists(),
        "Git 树随项目整体移动，内容不得被归类写入或改动"
    );
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
    assert!(
        f.root.join("Git项目集合/proj/.git/HEAD").exists()
            && f.root.join("Git项目集合/proj/src/main.rs").exists(),
        "Git 树必须随项目原样移动"
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

// 覆盖 C-08（三类清理各自独立覆盖删除方式；与全局方式解耦）
#[test]
fn cleanup_categories_override_delete_mode_independently() {
    let f = Fixture::new();
    f.write("Thumbs.db", b"junk", 10);
    f.write("note.tmp", b"temp", 11);
    f.write("empty.dat", b"", 12);
    f.write("keep.txt", b"payload", 13);
    let mut cfg = base();
    cfg.clean_temp = true;
    cfg.clean_zero = true;
    // C-08 新增的三类独立覆盖键；未显式覆盖的类别跟随全局文件删除方式。
    cfg.set_json("junk_delete", serde_json::json!("keep"))
        .unwrap();
    cfg.set_json("zero_delete", serde_json::json!("keep"))
        .unwrap();
    let task = f.plan(cfg);
    assert_eq!(
        task.summary.planned_delete, 1,
        "只有跟随全局的临时项入删除计划"
    );
    Fixture::apply(&task);
    assert!(
        exists_somewhere(&f.root, "Thumbs.db"),
        "junk_delete=keep 不得删除"
    );
    assert!(
        !exists_somewhere(&f.root, "note.tmp"),
        "临时项默认跟随全局永久删除"
    );
    assert!(
        exists_somewhere(&f.root, "empty.dat"),
        "zero_delete=keep 不得删除"
    );
    assert!(exists_somewhere(&f.root, "keep.txt"));

    // 全局保留 + 临时项显式永久删除：按类别覆盖必须能压过全局方式。
    let g = Fixture::new();
    g.write("note.tmp", b"temp", 11);
    g.write("keep.txt", b"payload", 13);
    let mut cfg = base();
    cfg.global_delete = DeleteMode::Keep;
    cfg.clean_temp = true;
    cfg.set_json("temp_delete", serde_json::json!("permanent"))
        .unwrap();
    Fixture::apply(&g.plan(cfg));
    assert!(
        !exists_somewhere(&g.root, "note.tmp"),
        "类别覆盖压过全局保留"
    );
    assert!(exists_somewhere(&g.root, "keep.txt"));
}
