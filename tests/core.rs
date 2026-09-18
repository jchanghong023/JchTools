//! These tests mutate only tempfile fixtures. Recycle Bin operations are injected mocks.
// 测试代码允许 unwrap/expect 与短名（f/p/q 夹具惯例）：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
mod common;
use common::FailRecycle;
use jchtools::{
    config::*,
    control::{Context, Control},
    db::Database,
    engine, fsutil, hashing,
    model::{ActionKind, FileRecord, Snapshot},
    platform::{self, DeleteResult, RecycleFailure, Recycler},
    rules,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
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
        engine::apply_with(&task.directory, Context::default(), Arc::new(FailRecycle)).unwrap()
    }
}
fn base() -> Config {
    Config {
        global_delete: DeleteMode::Permanent,
        clean_empty_dirs: false,
        clean_copy_name: false,
        classify: ClassifyMode::Off,
        ..Config::default()
    }
}
struct CancelRecycle;
impl Recycler for CancelRecycle {
    fn recycle(&self, _: &Path) -> Result<(), RecycleFailure> {
        Err(RecycleFailure::Cancelled)
    }
}
// 平台门禁原因：构造方均为 Windows 门禁用例（回收条目计数验证是 Windows 专属
// 实现）；非 Windows 编译时无构造方，属预期 dead_code。
#[allow(dead_code)]
struct MoveRecycle {
    target: PathBuf,
    calls: AtomicUsize,
}
impl Recycler for MoveRecycle {
    fn recycle(&self, p: &Path) -> Result<(), RecycleFailure> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        fs::rename(p, &self.target).map_err(|e| RecycleFailure::Failed(e.to_string()))
    }
    fn bin_count(&self, _: &Path) -> Option<i64> {
        Some(i64::try_from(self.calls.load(Ordering::Relaxed)).unwrap())
    }
}
// 覆盖 S-02
#[test]
fn defaults_valid_and_roundtrip() {
    let cfg = Config::default();
    cfg.validate().unwrap();
    assert!(cfg.recycle_fallback);
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
// 覆盖 S-03, C-02（verify_bytes 写死不可关闭；同名版本取舍开关已按合同移除）
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
    c.fix_extension = true;
    assert!(c.validate().is_err());
    c.detect_type = true;
    assert!(c.validate().is_ok());
    c.hash_workers = 0;
    assert!(c.validate().is_err());
    c.hash_workers = 2;
    c.reserve_gib = u64::MAX;
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
// 覆盖 S-02
#[test]
fn recycle_without_bin_counts_as_unverified() {
    let f = Fixture::new();
    let p = f.write("a", b"a", 1);
    let q = f.write("b", b"b", 1);
    struct NoBinCount;
    impl Recycler for NoBinCount {
        fn recycle(&self, p: &Path) -> Result<(), RecycleFailure> {
            fs::rename(p, p.with_extension("gone"))
                .map_err(|e| RecycleFailure::Failed(e.to_string()))
        }
    }
    let s = fsutil::snapshot(&p).unwrap();
    let t = fsutil::snapshot(&q).unwrap();
    // 回收「成功」但拿不到回收站条目数（无论是否允许降级）都必须如实记为未验证。
    assert_eq!(
        platform::remove(
            &p,
            Some(&s),
            DeleteMode::Recycle,
            false,
            &Control::default(),
            &NoBinCount
        )
        .unwrap(),
        DeleteResult::RecycledUnverified
    );
    assert_eq!(
        platform::remove(
            &q,
            Some(&t),
            DeleteMode::Recycle,
            true,
            &Control::default(),
            &NoBinCount
        )
        .unwrap(),
        DeleteResult::RecycledUnverified
    );
    assert!(!p.exists() && !q.exists());
}
// 回收的「条目计数验证」是 Windows 专属实现：platform::volume_root 在非 Windows 恒为 None，
// 引擎不会去询问 bin_count，回收成功也只能记为 RecycledUnverified。下面两个用例断言的就是
// 这条 Windows 路径；非 Windows 上「回收成功但无法验证」由 recycle_without_bin_counts_as_unverified 覆盖。
// 覆盖 S-02
#[cfg(windows)]
#[test]
fn recycle_error_after_move_with_verified_bin_counts_as_recycled() {
    let f = Fixture::new();
    let p = f.write("a", b"a", 1);
    struct LateFail {
        target: PathBuf,
        calls: AtomicUsize,
    }
    impl Recycler for LateFail {
        fn recycle(&self, p: &Path) -> Result<(), RecycleFailure> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            fs::rename(p, &self.target).map_err(|e| RecycleFailure::Failed(e.to_string()))?;
            Err(RecycleFailure::Failed("late failure".into()))
        }
        fn bin_count(&self, _: &Path) -> Option<i64> {
            Some(i64::try_from(self.calls.load(Ordering::Relaxed)).unwrap())
        }
    }
    let r = LateFail {
        target: f.root.join("mock-bin-late"),
        calls: AtomicUsize::new(0),
    };
    let s = fsutil::snapshot(&p).unwrap();
    assert_eq!(
        platform::remove(
            &p,
            Some(&s),
            DeleteMode::Recycle,
            true,
            &Control::default(),
            &r
        )
        .unwrap(),
        DeleteResult::Recycled
    );
    assert!(!p.exists() && r.target.exists());
}
// 覆盖 C-10（源/目标变化后快照失效，执行期跳过而非覆盖）
#[test]
fn metadata_change_invalidates_snapshot() {
    let f = Fixture::new();
    let p = f.write("a", b"one", 1);
    let s = fsutil::snapshot(&p).unwrap();
    fsutil::unchanged(&p, &s).unwrap();
    fs::write(&p, b"different").unwrap();
    assert!(fsutil::unchanged(&p, &s).is_err());
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
// 覆盖 C-05
#[test]
fn unique_name_truncates_long_stem_to_component_limit() {
    // 回归：基础名接近 255 个 UTF-16 单元且目标被占用时，“名 (N).扩展”候选名会超限，
    // validate_component 令 unique_target 整体失败——归类/解压冲突回退因此把整次任务
    // 或整包解压搞失败。应截断 stem 生成合法候选名，而不是放弃分配。
    let f = Fixture::new();
    // 基础名 251+4=255 恰好合法；加「 (1)」后 260 超限，旧行为会让 unique_target 失败。
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
    // 直接钉住扩展名截断后的尾随空白剥离：截断点落在 NBSP 之后时不得留下 NBSP 结尾
    // （validate_component 拒一切 Unicode 尾随空白，trim 集必须与其同口径）。
    let ext = format!(".{}\u{a0}z", "x".repeat(248));
    let n = fsutil::suffixed_candidate("a", &ext, 1);
    assert!(n.encode_utf16().count() <= 255, "{n}");
    assert!(
        !n.chars().last().is_some_and(char::is_whitespace),
        "不得以任何空白结尾：{n:?}"
    );
    assert!(n.ends_with('x'), "截断剥掉 NBSP 后应露出的最后字符：{n:?}");
    // 扩展名超长（252 单元）同样不得放弃分配：序号优先，其次扩展名，再截 stem。
    let long_ext = "x".repeat(251);
    let p2 = f.write(&format!("a.{long_ext}"), b"a", 1);
    let q2 = fsutil::unique_target(&f.root, &p2).unwrap();
    let n2 = q2.file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        n2.encode_utf16().count() <= 255,
        "分配名不得超 255 个 UTF-16 单元：{n2}"
    );
    assert!(
        n2.contains(" (1).") && n2.starts_with('a'),
        "保留序号与扩展名起点：{n2}"
    );
}
// 覆盖 S-03
#[test]
fn hashes_known_vectors() {
    let f = Fixture::new();
    let p = f.write("a", b"abc", 1);
    let s = fsutil::snapshot(&p).unwrap();
    let ctl = Control::default();
    assert_eq!(
        hashing::full_hash(&p, &s, &ctl).unwrap(),
        "blake3:6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    );
}
// 覆盖 S-03
#[test]
fn same_prehash_different_middle_is_not_duplicate() {
    let f = Fixture::new();
    let a = vec![7u8; 300_000];
    let mut b = a.clone();
    b[150_000] = 8;
    let ap = f.write("a", &a, 1);
    let bp = f.write("b", &b, 2);
    let sa = fsutil::snapshot(&ap).unwrap();
    let sb = fsutil::snapshot(&bp).unwrap();
    let ctl = Control::default();
    assert_eq!(
        hashing::prehash(&ap, &sa, &ctl).unwrap(),
        hashing::prehash(&bp, &sb, &ctl).unwrap()
    );
    assert_ne!(
        hashing::full_hash(&ap, &sa, &ctl).unwrap(),
        hashing::full_hash(&bp, &sb, &ctl).unwrap()
    );
    assert_eq!(f.plan(base()).summary.planned_delete, 0);
}
// 覆盖 C-10（取消后不再继续读取）
#[test]
fn cancelled_hash_does_not_read() {
    let f = Fixture::new();
    let p = f.write("a", b"abc", 1);
    let ctl = Control::default();
    ctl.cancel();
    assert!(hashing::full_hash(&p, &fsutil::snapshot(&p).unwrap(), &ctl).is_err());
    assert_eq!(ctl.read_bytes.load(Ordering::Relaxed), 0);
}
// 覆盖 C-08（副本后缀清理的识别口径）
#[test]
fn copy_suffixes_and_nonempty_name() {
    for s in [
        "报告 (1).pdf",
        "报告（2）.pdf",
        "报告 - Copy.pdf",
        "报告 副本.pdf",
    ] {
        assert_eq!(rules::strip_copy_name(s), "报告.pdf");
    }
    assert_eq!(rules::strip_copy_name("(1).pdf"), "(1).pdf");
}
// 覆盖 C-05
#[test]
fn category_and_custom_rules() {
    assert_eq!(rules::category("pdf"), "文档");
    assert_eq!(rules::category("ts"), "代码");
    assert_eq!(
        rules::parse_categories("工程=rs,sv;资料=pdf").unwrap()["sv"],
        "工程"
    );
    assert!(rules::parse_categories("../x=pdf").is_err());
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
// 覆盖 S-02
#[test]
fn recycle_failure_without_permission_keeps_file() {
    let f = Fixture::new();
    let p = f.write("a", b"a", 1);
    let s = fsutil::snapshot(&p).unwrap();
    assert!(platform::remove(
        &p,
        Some(&s),
        DeleteMode::Recycle,
        false,
        &Control::default(),
        &FailRecycle
    )
    .is_err());
    assert!(p.exists());
}
// 覆盖 S-02
#[test]
fn recycle_failure_with_permission_deletes() {
    let f = Fixture::new();
    let p = f.write("a", b"a", 1);
    let s = fsutil::snapshot(&p).unwrap();
    assert_eq!(
        platform::remove(
            &p,
            Some(&s),
            DeleteMode::Recycle,
            true,
            &Control::default(),
            &FailRecycle
        )
        .unwrap(),
        DeleteResult::Permanent
    );
    assert!(!p.exists());
}
// 覆盖 S-02
#[test]
fn user_cancel_never_falls_back_to_delete() {
    let f = Fixture::new();
    let p = f.write("a", b"a", 1);
    assert!(platform::remove(
        &p,
        None,
        DeleteMode::Recycle,
        true,
        &Control::default(),
        &CancelRecycle
    )
    .is_err());
    assert!(p.exists());
}
// 覆盖 S-02
#[cfg(windows)]
#[test]
fn successful_recycle_not_permanent() {
    let f = Fixture::new();
    let p = f.write("a", b"a", 1);
    let bin = MoveRecycle {
        target: f.root.join("mock-bin"),
        calls: AtomicUsize::new(0),
    };
    assert_eq!(
        platform::remove(
            &p,
            None,
            DeleteMode::Recycle,
            true,
            &Control::default(),
            &bin
        )
        .unwrap(),
        DeleteResult::Recycled
    );
    assert!(bin.target.exists());
    assert_eq!(bin.calls.load(Ordering::Relaxed), 1);
}
// 覆盖 S-02
#[test]
fn keep_never_calls_recycler() {
    let f = Fixture::new();
    let p = f.write("a", b"a", 1);
    assert_eq!(
        platform::remove(
            &p,
            None,
            DeleteMode::Keep,
            true,
            &Control::default(),
            &CancelRecycle
        )
        .unwrap(),
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
        None,
        DeleteMode::Permanent,
        true,
        &Control::default(),
        &FailRecycle
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
    assert!(!old.exists() && new.exists());
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
    assert!(f.root.join("a").exists() && f.root.join("b").exists());
}
// 覆盖 C-10
#[test]
fn changed_source_after_plan_is_skipped() {
    let f = Fixture::new();
    let a = f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let task = f.plan(base());
    fs::write(&a, b"brand new").unwrap();
    let result = Fixture::apply(&task);
    assert!(a.exists());
    assert_eq!(result.summary.errors, 1);
}
// 覆盖 C-10
#[test]
fn changed_keeper_after_plan_prevents_delete() {
    let f = Fixture::new();
    let a = f.write("a", b"same", 10);
    let b = f.write("b", b"same", 20);
    let task = f.plan(base());
    fs::write(&b, b"new keeper").unwrap();
    let result = Fixture::apply(&task);
    assert!(a.exists());
    assert_eq!(result.summary.errors, 1);
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
    assert!(f.root.join("keep.txt").exists());
    assert!(!f.root.join("temporary.tmp").exists());
}
// 覆盖 C-08
#[test]
fn cleanup_keep_files_are_not_dedup_deletions() {
    // cleanup_delete=Keep 的清理命中文件由清理规则管辖（保留承诺）：去重路径同样不得删除。
    // 此前只防了「不得充当 keeper」，keeper 先注册时它作为重复项会按 duplicate_delete 被删，
    // 结果随 duplicate_order 排序翻转（本用例让 junk.tmp 排在 keeper 之后触发原缺陷）。
    let f = Fixture::new();
    f.write("normal.txt", b"payload", 20); // 较新 → 成为 keeper
    f.write("junk.tmp", b"payload", 10); // 较旧且命中 clean_temp → 修复前被按重复删除
    let mut cfg = base();
    cfg.clean_temp = true;
    cfg.cleanup_delete = DeleteChoice::Keep;
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
    assert!(f.root.join("normal.txt").exists() && f.root.join("junk.tmp").exists());
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
        f.root.join("a.txt").exists(),
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
// 覆盖 R-02
#[test]
fn class_override_beats_global_keep() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let mut cfg = base();
    cfg.global_delete = DeleteMode::Keep;
    cfg.duplicate_delete = DeleteChoice::Permanent;
    assert_eq!(f.plan(cfg).summary.planned_delete, 1);
}
// 覆盖 C-08
#[test]
fn copy_name_cleanup_uses_freed_original_name() {
    let f = Fixture::new();
    f.write("a.pdf", b"same", 10);
    f.write("a (1).pdf", b"same", 20);
    let mut cfg = base();
    cfg.clean_copy_name = true;
    let task = f.plan(cfg);
    Fixture::apply(&task);
    assert!(f.root.join("a.pdf").exists());
    assert!(!f.root.join("a (1).pdf").exists());
}
// 覆盖 C-05
#[test]
fn classification_preserves_paths_and_is_idempotent() {
    let f = Fixture::new();
    f.write("folder/a.pdf", b"pdf", 10);
    let mut cfg = base();
    cfg.classify = ClassifyMode::Extension;
    let task = f.plan(cfg.clone());
    Fixture::apply(&task);
    assert!(f.root.join("PDF/folder/a.pdf").exists());
    let again = f.plan(cfg);
    assert_eq!(again.summary.planned_move, 0);
}
// 覆盖 C-05
#[test]
fn flatten_classification_allocates_nonconflicting_names() {
    let f = Fixture::new();
    f.write("x/a.pdf", b"left", 10);
    f.write("y/a.pdf", b"right", 20);
    let mut cfg = base();
    cfg.classify = ClassifyMode::Extension;
    cfg.preserve_structure = false;
    let task = f.plan(cfg);
    Fixture::apply(&task);
    assert!(f.root.join("PDF/a.pdf").exists());
    assert!(f.root.join("PDF/a (1).pdf").exists());
}
// 覆盖 C-07
#[test]
fn empty_directory_cleanup_is_bottom_up() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("empty/nested")).unwrap();
    let mut cfg = base();
    cfg.clean_empty_dirs = true;
    let task = f.plan(cfg);
    Fixture::apply(&task);
    assert!(!f.root.join("empty").exists());
    assert!(f.root.exists());
}
// 覆盖 C-07, S-04
#[test]
fn empty_hidden_subdir_blocks_empty_directory_cleanup() {
    // 行为锚点：仅含一个空的隐藏（Windows）/点开头（Unix）子目录的目录，
    // 该子目录不会入库，目录对规划"并非实际为空"，不得计划为空目录。
    // 否则计划承诺落空：执行期实空复查只能跳过，skipped 虚增、该清的没清。
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("outer/.inner")).unwrap();
    #[cfg(windows)]
    set_hidden(&f.root.join("outer/.inner"), true);
    f.write("keep.txt", b"payload", 10);
    let mut cfg = base();
    cfg.clean_empty_dirs = true;
    let task = f.plan(cfg);
    assert_eq!(
        task.summary.planned_empty, 0,
        "含未入库子目录的目录不得计划为空目录"
    );
    assert!(f.root.join("outer").exists());
}
// 覆盖 C-07
#[test]
fn empty_directory_with_underscore_not_blocked_by_similar_name() {
    // 行为契约：目录名含下划线/百分号时，空目录判定不得波及名字相似（仅差一两个字符）的邻居目录。
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("my_dir/nested_empty")).unwrap();
    f.write("myXdir/file.txt", b"payload", 10);
    let mut cfg = base();
    cfg.clean_empty_dirs = true;
    Fixture::apply(&f.plan(cfg));
    assert!(
        !f.root.join("my_dir").exists(),
        "含下划线的空目录必须能被清理"
    );
    assert!(
        f.root.join("myXdir/file.txt").exists(),
        "相似前缀目录里的文件不能被误伤"
    );
}
// 覆盖 C-07
#[test]
fn empty_directory_with_percent_in_name_is_cleaned() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("100%done")).unwrap();
    f.write("100Xdone/file.txt", b"payload", 10);
    let mut cfg = base();
    cfg.clean_empty_dirs = true;
    Fixture::apply(&f.plan(cfg));
    assert!(!f.root.join("100%done").exists());
    assert!(f.root.join("100Xdone/file.txt").exists());
}
// 覆盖 C-05, C-07
#[test]
fn classification_empty_dirs_planned_in_same_pass() {
    let f = Fixture::new();
    f.write("folder/a.pdf", b"pdf", 10);
    let mut cfg = base();
    cfg.classify = ClassifyMode::Extension;
    cfg.clean_empty_dirs = true;
    cfg.preserve_structure = true;
    let task = f.plan(cfg.clone());
    assert_eq!(task.summary.planned_move, 1);
    assert_eq!(
        task.summary.planned_empty, 1,
        "folder/ becomes empty after the move and must be planned now"
    );
    Fixture::apply(&task);
    assert!(f.root.join("PDF/folder/a.pdf").exists());
    assert!(!f.root.join("folder").exists());
    let again = f.plan(cfg);
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.planned_move, 0);
    assert_eq!(again.summary.planned_empty, 0);
}
// 覆盖 C-05
#[test]
fn date_classification_is_idempotent() {
    let f = Fixture::new();
    f.write("folder/x.txt", b"payload", 1_700_000_000);
    let mut cfg = base();
    cfg.classify = ClassifyMode::Date;
    let task = f.plan(cfg.clone());
    assert_eq!(task.summary.planned_move, 1);
    let target = Database::open(&task.directory)
        .unwrap()
        .actions_page(0, 10)
        .unwrap()
        .remove(0)
        .target
        .unwrap();
    let parts: Vec<&str> = target.split('/').collect();
    assert_eq!(parts.len(), 4, "年/月/folder/x.txt");
    assert_eq!((parts[0].len(), parts[1].len()), (4, 2), "年月目录");
    assert_eq!(parts[3], "x.txt");
    Fixture::apply(&task);
    let again = f.plan(cfg);
    assert_eq!(
        again.summary.planned_move, 0,
        "重新分析不得把年/月目录再套一层"
    );
}
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
    // 行为锚点：扫描以 walkdir min_depth(1) 运行，根条目不会被产出，filter_entry 谓词
    // 因此从不作用于用户选定的根目录——隐藏根目录不会整树剪枝；而根目录下的隐藏
    // 子目录仍按隐藏判定剪枝。两条 walkdir 语义在此一并钉住。
    let f = Fixture::new();
    f.write("a.txt", b"payload", 10);
    f.write("secret/b.txt", b"hidden", 20);
    set_hidden(&f.root, true);
    set_hidden(&f.root.join("secret"), true);
    let task = f.plan(base());
    set_hidden(&f.root, false);
    set_hidden(&f.root.join("secret"), false);
    assert_eq!(
        task.summary.scanned, 1,
        "隐藏根目录里的普通文件必须能被扫描到；隐藏子目录必须被剪枝"
    );
}
// 覆盖 S-04
#[cfg(not(windows))]
#[test]
fn hidden_root_directory_still_scanned() {
    // 行为锚点：扫描以 walkdir min_depth(1) 运行，根条目不会被产出，filter_entry 谓词
    // 因此从不作用于用户选定的根目录——点开头根目录不会整树剪枝；根下的点开头
    // 子目录仍按隐藏判定剪枝。
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".hidden-root");
    fs::create_dir_all(root.join(".secret")).unwrap();
    fs::write(root.join("a.txt"), b"payload").unwrap();
    fs::write(root.join(".secret/b.txt"), b"hidden").unwrap();
    let state = temp.path().join("state");
    let task = engine::prepare_at(&root, base(), Context::default(), &state).unwrap();
    assert_eq!(
        task.summary.scanned, 1,
        "点开头根目录里的普通文件必须能被扫描到；点开头子目录必须被剪枝"
    );
}
// 覆盖 C-05, C-06（合并同名目录的收敛性/幂等）
#[test]
fn merge_directories_never_pulls_files_out_of_output() {
    // 输出目录内的分类子目录与树中同名外部目录重名时，合并不得把已归类文件拉回
    // 外部目录：否则归类与合并跨运行互相拉扯，计划永不收敛。
    let f = Fixture::new();
    f.write("文档/old/a.pdf", b"pdf", 10);
    let mut cfg = base();
    cfg.classify = ClassifyMode::Extension;
    cfg.preserve_structure = true;
    cfg.merge_directories = true;
    cfg.output_dir = "整理".into();
    let task = f.plan(cfg.clone());
    assert_eq!(task.summary.planned_move, 1);
    Fixture::apply(&task);
    assert!(f.root.join("整理/PDF/文档/old/a.pdf").exists());
    let again = f.plan(cfg);
    assert_eq!(
        again.summary.planned_move, 0,
        "第二次分析不得把已归类文件再移回外部同名目录"
    );
}
// 覆盖 C-08
#[test]
fn copy_name_cleanup_never_plans_self_move() {
    // 保留文件剥离副本名后原名被其它内容占用时，回退序号不得撞回自身当前名称：
    // source==target 的空转移动破坏计划幂等（执行后 moved 计数虚高）。
    let f = Fixture::new();
    f.write("报告 (1).pdf", b"A", 30);
    f.write("报告 (2).pdf", b"A", 20);
    f.write("报告.pdf", b"B", 10);
    let mut cfg = base();
    cfg.clean_copy_name = true;
    let task = f.plan(cfg);
    assert_eq!(task.summary.planned_delete, 1, "同内容副本应被删除");
    assert_eq!(
        task.summary.planned_move, 0,
        "保留者已在合理位置，不得生成移动动作"
    );
    Fixture::apply(&task);
    assert!(f.root.join("报告 (1).pdf").exists());
    assert!(!f.root.join("报告 (2).pdf").exists());
    assert!(f.root.join("报告.pdf").exists());
}
// 覆盖 C-04, S-06
#[test]
fn hardlink_does_not_claim_permanent_bytes() {
    // 硬链接去重不销毁内容（源目录项由指向 keeper 的链接顶替），物理占用不变：
    // 即使删除模式解析为 Permanent，也不得把源大小计入 permanent_bytes。
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let mut cfg = base();
    cfg.duplicate_action = DuplicateAction::Hardlink;
    let task = f.plan(cfg);
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.linked, 1);
    assert_eq!(result.summary.deleted, 1, "硬链接替换仍按删除项数记账");
    assert_eq!(
        result.summary.permanent_bytes, 0,
        "硬链接不释放物理空间，不得计入永久删除字节"
    );
}
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
    assert!(
        engine::apply_with(&task.directory, Context::default(), Arc::new(FailRecycle)).is_err()
    );
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
}
// 覆盖 C-04
#[test]
fn hardlink_mode_preserves_aliases() {
    let f = Fixture::new();
    f.write("a", b"same", 10);
    f.write("b", b"same", 20);
    let mut cfg = base();
    cfg.duplicate_action = DuplicateAction::Hardlink;
    let task = f.plan(cfg);
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.linked, 1);
    assert_eq!(
        fsutil::snapshot(&f.root.join("a")).unwrap().identity,
        fsutil::snapshot(&f.root.join("b")).unwrap().identity
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
    let result = engine::apply_with(&moved, Context::default(), Arc::new(FailRecycle));
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
    let result = engine::apply_with(&moved, Context::default(), Arc::new(FailRecycle)).unwrap();
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
            engine::apply_with(&task.directory, Context::default(), Arc::new(FailRecycle)).is_err(),
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
            engine::apply_with(&moved, Context::default(), Arc::new(FailRecycle)).is_err(),
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
        std::thread::spawn(move || engine::apply_with(&dir, context, Arc::new(FailRecycle)))
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
    let mut cfg = base();
    cfg.classify = ClassifyMode::Extension;
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].kind, ActionKind::Move);
    let deletes = db.actions_page_filtered(0, 100, Some("delete")).unwrap();
    assert!(deletes.is_empty(), "纯归类任务不应有删除动作");
    let all = db.actions_page(0, 100).unwrap();
    assert_eq!(all.len(), moves.len() + deletes.len());
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
fn rules_table_has_exactly_39_rows_without_version_switches() {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../resources/rules.json")).unwrap();
    let rows = schema.as_array().unwrap();
    // 2026-09-18 两工具拆分：「递归解压压缩包」总开关随 X-01 工具化移除（39 项）。
    assert_eq!(
        rows.len(),
        39,
        "R-01：规则表必须恰好 39 项，实际 {}",
        rows.len()
    );
    for key in [
        "same_name_same_size",
        "same_size_keep",
        "conflict_scope_directory",
    ] {
        assert!(
            rows.iter().all(|r| r["key"].as_str() != Some(key)),
            "C-02：规则面板不得提供版本取舍开关行：{key}"
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

// 覆盖 C-02, S-03（逐字节复核的判定本身：等长同内容 / 等长异内容 / 长度不同）
#[test]
fn equal_bytes_matches_only_identical_content() {
    let f = Fixture::new();
    let a = f.write("a", b"same content", 1);
    let b = f.write("b", b"same content", 1);
    let c = f.write("c", b"other length", 1);
    let d = f.write("d", b"samelength!!", 1);
    let sa = fsutil::snapshot(&a).unwrap();
    let sb = fsutil::snapshot(&b).unwrap();
    let sc = fsutil::snapshot(&c).unwrap();
    let sd = fsutil::snapshot(&d).unwrap();
    let ctl = Control::default();
    assert!(hashing::equal_bytes(&a, &sa, &b, &sb, &ctl).unwrap());
    assert!(
        !hashing::equal_bytes(&a, &sa, &c, &sc, &ctl).unwrap(),
        "长度不同直接判不等"
    );
    assert!(
        !hashing::equal_bytes(&a, &sa, &d, &sd, &ctl).unwrap(),
        "等长不同内容必须逐字节判不等"
    );
}

// 覆盖 C-02, S-03（删除前逐字节复核不可关闭：元数据未变但内容分歧时拒绝删除）
#[test]
fn byte_level_recheck_blocks_delete_when_content_diverges() {
    let f = Fixture::new();
    let a = f.write("a", b"same", 10); // 旧 → 计划删除项
    f.write("b", b"same", 20); // 新 → 保留者
    let task = f.plan(base());
    assert_eq!(task.summary.planned_delete, 1);
    // 篡改待删项内容但保持长度与 mtime（绕过 unchanged 的元数据校验）：
    // 逐字节复核必须发现两份内容不同并拒绝执行删除。
    fs::write(&a, b"diff").unwrap();
    filetime::set_file_mtime(&a, filetime::FileTime::from_unix_time(10, 0)).unwrap();
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 1, "复核不一致必须计为执行失败");
    assert!(
        a.exists() && f.root.join("b").exists(),
        "两份内容都必须原样保留，不得以覆盖方式「自动修复」（C-10）"
    );
}

// 覆盖 C-03, R-02, S-08（保留规则各可选项的实际选择结果与决胜链）
#[test]
fn keeper_policy_variants_pick_expected_keepers() {
    for (policy, kept, deleted) in [
        (KeepPolicy::Oldest, "aa.txt", "zzzz.txt"),
        (KeepPolicy::Newest, "zzzz.txt", "aa.txt"),
        // 同内容必然等大小：Largest/Smallest 的主键全平局，按 C-03 决胜链（路径长度）落位
        (KeepPolicy::Largest, "aa.txt", "zzzz.txt"),
        (KeepPolicy::Smallest, "aa.txt", "zzzz.txt"),
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
            f.root.join(kept).exists() && !f.root.join(deleted).exists(),
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
    cfg.large_files = true; // large_threshold_gib 默认 1（GiB）
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    let big_move = moves
        .iter()
        .find(|a| a.source == "big.bin")
        .expect("大文件应有归类移动计划");
    assert_eq!(big_move.target.as_deref(), Some("大文件/big.bin"));
    assert!(
        moves.iter().all(|a| a.source == "big.bin"),
        "小文件不参与大文件归类"
    );
}

// 覆盖 C-06（消除只有一个子项的目录层级）
#[test]
fn flatten_single_child_collapses_levels() {
    let f = Fixture::new();
    f.write("alpha/only/report.pdf", b"pdf", 10);
    let mut cfg = base();
    cfg.classify = ClassifyMode::Extension;
    cfg.preserve_structure = true;
    cfg.flatten_single_child = true;
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    let target = moves
        .iter()
        .find(|a| a.source == "alpha/only/report.pdf")
        .expect("文件应有移动计划")
        .target
        .clone()
        .unwrap();
    assert_eq!(target, "PDF/report.pdf", "单子项目录链应被消除：{target}");
}

// 覆盖 C-06（合并不同位置的同名目录：两处 report/ 的文件进同一物理目录）
#[test]
fn merge_directories_joins_same_name_directories() {
    let f = Fixture::new();
    f.write("alpha/report/a.pdf", b"one", 10);
    f.write("beta/report/b.pdf", b"two", 20);
    let mut cfg = base();
    cfg.classify = ClassifyMode::Extension;
    cfg.preserve_structure = true;
    cfg.merge_directories = true;
    let task = f.plan(cfg);
    Fixture::apply(&task);
    assert!(
        f.root.join("PDF/alpha/report/a.pdf").exists(),
        "a.pdf 应随合并落位"
    );
    assert!(
        f.root.join("PDF/alpha/report/b.pdf").exists(),
        "同名目录必须合并到同一位置，b.pdf 不得留在另一棵 report/ 下"
    );
}

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
    assert!(f.root.join("keep.txt").exists(), "普通文件不受清理影响");
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
    // U+0065 + U+0301（分解形式）应规范为 U+00E9（合成形式）；双空格合并为单空格
    let raw_name = "cafe\u{0301}  report .txt";
    f.write(raw_name, b"payload", 10);
    let cfg = base();
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    assert_eq!(moves.len(), 1, "规范化默认开启，改名应入移动计划");
    assert_eq!(moves[0].target.as_deref(), Some("café report .txt"));
    Fixture::apply(&task);
    assert!(
        f.root.join("café report .txt").exists(),
        "执行后应为规范化名"
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
    assert_eq!(moves.len(), 1, "错误扩展名应产生改名计划");
    assert_eq!(
        moves[0].target.as_deref(),
        Some("photo.png"),
        "计划里可见改名为内容识别出的真实类型"
    );
    Fixture::apply(&task);
    assert!(f.root.join("photo.png").exists());
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
    let result = engine::apply_with(&task.directory, Context::default(), Arc::new(FailRecycle));
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

// 覆盖 S-06（摘要区分「回收」与「永久删除」的逻辑大小，不得互相串账）
#[test]
fn summary_separates_recycled_and_permanent_bytes() {
    // 永久删除模式（全平台）：计入 permanent_bytes，不计回收
    {
        let f = Fixture::new();
        f.write("a", b"same", 10);
        f.write("b", b"same", 20);
        let task = f.plan(base()); // global_delete = Permanent
        let result = Fixture::apply(&task);
        assert_eq!(result.summary.deleted, 1);
        assert_eq!(result.summary.permanent_bytes, 4, "「same」=4 字节");
        assert_eq!(result.summary.recycled_bytes, 0);
        assert_eq!(result.summary.recycled, 0);
    }
    // 回收模式：Windows 上条目计数可验证，计入 recycled_bytes 且不算永久删除。
    // 平台门禁原因：回收「条目计数验证」是 Windows 专属实现（volume_root 在
    // 非 Windows 恒为 None，只能记 RecycledUnverified），该口径由本用例 Windows 分支锚定。
    #[cfg(windows)]
    {
        let f = Fixture::new();
        f.write("a", b"same", 10);
        f.write("b", b"same", 20);
        let mut cfg = base();
        cfg.global_delete = DeleteMode::Recycle;
        let recycler = Arc::new(MoveRecycle {
            target: f.root.join("mock-bin"),
            calls: AtomicUsize::new(0),
        });
        let task =
            engine::prepare_with(&f.root, cfg, Context::default(), &f.state, recycler.clone())
                .unwrap();
        let result = engine::apply_with(&task.directory, Context::default(), recycler).unwrap();
        assert_eq!(result.summary.recycled, 1, "回收成功且计数验证通过");
        assert_eq!(result.summary.recycled_bytes, 4);
        assert_eq!(result.summary.permanent_bytes, 0);
        assert_eq!(result.summary.deleted, 0);
    }
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
            .all(|name| name == "tasks" || name == "organizer.lock"),
        "状态目录只应含任务库与锁文件，不得出现独立设置文件（不落盘/无导入导出）：{entries:?}"
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

// 覆盖 X-01（支持格式候选清单与大小写不敏感；非压缩包后缀不误判）
#[test]
fn archive_name_covers_supported_formats() {
    for name in [
        "a.zip",
        "a.7Z",
        "b.tar",
        "b.gz",
        "b.bz2",
        "b.xz",
        "b.zst",
        "b.tgz",
        "b.tbz2",
        "b.txz",
        "c.cab",
        "c.iso",
        "c.wim",
        "c.lzh",
        "c.cpio",
        "d.7z.001",
        "d.zip.001",
        "e.part1.rar",
    ] {
        assert!(rules::archive_name(name), "{name} 应识别为压缩包");
    }
    for name in ["a.txt", "a.png", "a.zipx", "e.part2.rar", "archive"] {
        assert!(!rules::archive_name(name), "{name} 不应识别为压缩包");
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

// 覆盖 R-03（性能默认值：哈希线程 2、磁盘预留 1 GiB、递归默认包含子目录）
#[test]
fn performance_defaults_match_contract() {
    let cfg = Config::default();
    assert_eq!(cfg.hash_workers, 2);
    assert_eq!(cfg.reserve_gib, 1);
    assert!(cfg.recursive);
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
    assert!(f.root.join("a.txt").exists(), "清扫不得影响 keeper 本体");
}

// 覆盖 S-06（回收字节与永久字节同口径：多硬链接的源不重复计入逻辑大小）
// 平台门禁原因：断言依赖回收条目计数验证（volume_root 仅 Windows 提供），
// 非 Windows 上回收只能记 RecycledUnverified（走已有 links<=1 守卫）。
#[cfg(windows)]
#[test]
fn recycled_bytes_exclude_hardlinked_sources() {
    let f = Fixture::new();
    let a = f.write("a", b"same", 10);
    if let Err(error) = fs::hard_link(&a, f.root.join("c")) {
        panic!("无法创建硬链接：{error}；请在支持硬链接的文件系统上运行测试");
    }
    filetime::set_file_mtime(&a, filetime::FileTime::from_unix_time(10, 0)).unwrap();
    f.write("b", b"same", 20); // 更新 → 保留者
    let mut cfg = base();
    cfg.global_delete = DeleteMode::Recycle;
    let recycler = Arc::new(MoveRecycle {
        target: f.root.join("mock-bin"),
        calls: AtomicUsize::new(0),
    });
    let task =
        engine::prepare_with(&f.root, cfg, Context::default(), &f.state, recycler.clone()).unwrap();
    assert!(
        task.summary.planned_delete >= 1,
        "a/c 与 b 内容相同应生成删除计划"
    );
    assert_eq!(task.summary.candidate_bytes, 0, "计划侧已排除多链接文件");
    let result = engine::apply_with(&task.directory, Context::default(), recycler).unwrap();
    assert!(
        result.summary.recycled >= 1,
        "回收确实发生了（项数记账不受影响）"
    );
    assert_eq!(
        result.summary.recycled_bytes, 0,
        "被删副本仍有其他硬链接持有内容，回收字节不得计入（与 permanent_bytes/candidate_bytes 同口径）"
    );
}

// 覆盖 C-05, C-10（flatten 与分类同时开启时重复整理幂等，不得把文件移出分类目录）
#[test]
fn flatten_with_classification_is_idempotent_across_rounds() {
    let f = Fixture::new();
    f.write("alpha/only/report.pdf", b"pdf", 10);
    let mut cfg = base();
    cfg.classify = ClassifyMode::Extension;
    cfg.preserve_structure = true;
    cfg.flatten_single_child = true;
    // 第一轮：单子项目录链被消除，文件归入 PDF/。
    let first = f.plan(cfg.clone());
    assert_eq!(first.summary.planned_move, 1);
    Fixture::apply(&first);
    assert!(f.root.join("PDF/report.pdf").exists(), "首轮应归入 PDF/");
    // 第二轮：PDF/ 只含一个文件（恰为 flatten 的"单子项"形态），
    // 不得把文件从分类目录里抽回根下——否则第三轮又移回去，无限往复。
    let second = f.plan(cfg.clone());
    assert_eq!(
        second.summary.planned_move, 0,
        "已在分类目录内的文件必须稳定，不得被 flatten 抽出（幂等）"
    );
    Fixture::apply(&second);
    assert!(
        f.root.join("PDF/report.pdf").exists(),
        "二轮执行后文件仍在分类目录"
    );
    // 第三轮继续稳定（无奇偶振荡）。
    let third = f.plan(cfg.clone());
    assert_eq!(third.summary.planned_move, 0, "第三轮同样不得产生移动");
}

// 覆盖 X-06, C-11（失败口径按"包"计：分卷组的每卷文件数不得报成包数）
#[test]
fn extract_summary_counts_failed_packages_not_volumes() {
    use jchtools::model::Summary;
    let summary = Summary {
        archives_ok: 1,
        archives_failed: 1,
        archives_quarantined: 3, // 三卷分卷组整组隔离：文件数 3、包数 1
        ..Summary::default()
    };
    let text = summary.extract_description();
    assert!(
        text.contains("失败并移入「解压失败」1 包"),
        "用户可见口径必须是包数（1），不是卷文件数（3）：{text}"
    );
}
