//! 文件系统边界回归（S-01 / S-04）：
//! - F01：所选根或其祖先含符号链接/junction 时两工具拒绝开始（S-04 主句，
//!   修复前 normalize_root 先 canonicalize，reparse 身份丢失、链接目标树被处理）。
//! - F02：执行前清理 `.jchtools-link-*` 崩溃残留不得越出本次处理范围（S-01，
//!   修复前 apply 用独立全盘 WalkDir 静默删除范围外文件）。
//!
//! 全部数据在 tempfile 内生成；junction 用 `cmd /C mklink /J`（无需管理员）。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
use jchtools::{config::Config, control::Context, control::Control, engine};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;

/// 用 `cmd /C mklink /J` 创建 junction（无需管理员权限）。
fn make_junction(link: &Path, target: &Path) -> std::io::Result<()> {
    let out = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "mklink /J 失败：{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

/// 用 `cmd /C mklink /D` 创建目录符号链接（需要管理员或开发者模式）；
/// 权限不足时返回 Err，调用方按运行时条件跳过并注明原因（不使用 #[ignore]）。
fn make_symlink(link: &Path, target: &Path) -> std::io::Result<()> {
    let out = Command::new("cmd")
        .args(["/C", "mklink", "/D"])
        .arg(link)
        .arg(target)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "mklink /D 失败：{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

/// 状态目录下已创建的任务库数量（0 = 未创建任何计划）。
fn count_task_dirs(state: &Path) -> usize {
    fs::read_dir(state.join("tasks")).map_or(0, |entries| {
        entries.filter_map(std::result::Result::ok).count()
    })
}

/// 造一个「疑似上次执行崩溃残留」：`.jchtools-link-<标记>` 硬链接（链接数 ≥ 2），
/// 修改时间设为 25 小时前（超过 24 小时阈值）。
fn make_stale_link_temp(dir: &Path, keeper: &Path, tag: &str) -> PathBuf {
    let path = dir.join(format!(".jchtools-link-{tag}"));
    fs::hard_link(keeper, &path).unwrap();
    let old = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - Duration::from_hours(25),
    );
    filetime::set_file_mtime(&path, old).unwrap();
    path
}

/// 为文件设置隐藏属性（Windows attrib；测试内联目录数据时使用）。
fn set_hidden(path: &Path) {
    let out = Command::new("attrib").arg("+h").arg(path).output().unwrap();
    assert!(
        out.status.success(),
        "测试前置：设置隐藏属性失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

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
}

// ---------------------------------------------------------------------------
// F01 · S-04：所选根或其访问路径经过链接边界时拒绝开始
// ---------------------------------------------------------------------------

// 覆盖 S-04（回归：所选根本身是 junction 时目录整理必须拒绝开始；修复前
// canonicalize 解析掉 reparse 身份，任务在链接目标树里正常建库扫描）
#[test]
fn f01_prepare_rejects_root_junction() {
    let f = Fixture::new();
    let target = f.temp.path().join("real");
    let payload = target.join("资料.txt");
    fs::create_dir_all(&target).unwrap();
    fs::write(&payload, "内容").unwrap();
    let link = f.temp.path().join("入口");
    make_junction(&link, &target).unwrap();

    let error = engine::prepare_at(&link, Config::default(), Context::default(), &f.state)
        .err()
        .map(|e| format!("{e:#}"));
    let error = error.expect("junction 根必须被拒绝开始（S-04）");
    assert!(
        error.contains("S-04") && error.contains("符号链接"),
        "拒绝文案必须明示 S-04 语义：{error}"
    );
    assert!(payload.exists(), "拒绝时链接目标树零改动");
    assert_eq!(count_task_dirs(&f.state), 0, "拒绝时不创建计划、不扫描");
}

// 覆盖 S-04（回归：祖先组件是 junction 时同样拒绝；访问路径经过链接边界）
#[test]
fn f01_prepare_rejects_ancestor_junction() {
    let f = Fixture::new();
    let target = f.temp.path().join("real");
    let payload = target.join("sub").join("文件.txt");
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::write(&payload, "内容").unwrap();
    let link = f.temp.path().join("链路");
    make_junction(&link, &target).unwrap();
    let selected = link.join("sub");

    let error = engine::prepare_at(&selected, Config::default(), Context::default(), &f.state)
        .err()
        .map(|e| format!("{e:#}"));
    let error = error.expect("祖先为 junction 的根必须被拒绝开始（S-04）");
    assert!(
        error.contains("S-04"),
        "拒绝文案必须明示 S-04 语义：{error}"
    );
    assert!(payload.exists(), "拒绝时链接目标树零改动");
    assert_eq!(count_task_dirs(&f.state), 0, "拒绝时不创建计划、不扫描");
}

// 覆盖 S-04（对照组：普通目录不受链接边界检查影响，正常分析）
#[test]
fn f01_prepare_accepts_plain_directory() {
    let f = Fixture::new();
    fs::write(f.root.join("a.txt"), b"a").unwrap();
    let task = engine::prepare_at(&f.root, Config::default(), Context::default(), &f.state)
        .expect("普通目录必须正常分析");
    assert_eq!(count_task_dirs(&f.state), 1);
    assert_eq!(task.summary.scanned, 1);
}

// 覆盖 S-04（回归：确认框清点入口同样拒绝 junction 根，清点不进入链接目标树）
#[test]
fn f01_count_archives_rejects_root_junction() {
    let f = Fixture::new();
    let target = f.temp.path().join("real");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("pack.zip"), b"fake").unwrap();
    let link = f.temp.path().join("入口");
    make_junction(&link, &target).unwrap();

    let error = engine::count_archives(&link, &Config::default())
        .err()
        .map(|e| format!("{e:#}"));
    let error = error.expect("junction 根的清点必须被拒绝（S-04）");
    assert!(
        error.contains("S-04"),
        "拒绝文案必须明示 S-04 语义：{error}"
    );
    assert!(target.join("pack.zip").exists(), "拒绝时零改动");
}

// 覆盖 S-04（回归：递归解压入口拒绝 junction 根，不创建任务库）
#[test]
fn f01_extract_run_rejects_root_junction() {
    let f = Fixture::new();
    let target = f.temp.path().join("real");
    fs::create_dir_all(&target).unwrap();
    let pack = target.join("pack.zip");
    fs::write(&pack, b"fake zip bytes").unwrap();
    let link = f.temp.path().join("入口");
    make_junction(&link, &target).unwrap();

    let error =
        engine::extract_run_at(&link, Config::default(), Context::default(), &f.state, None)
            .err()
            .map(|e| format!("{e:#}"));
    let error = error.expect("junction 根的解压必须被拒绝开始（S-04）");
    assert!(
        error.contains("S-04"),
        "拒绝必须发生在引擎阶段之前：{error}"
    );
    assert_eq!(count_task_dirs(&f.state), 0, "拒绝时不创建任务库");
    assert!(pack.exists(), "拒绝时链接目标树零改动");
}

// 覆盖 S-04（根符号链接；权限不足无法构造夹具时运行时跳过并注明原因，
// 不使用 #[ignore]——跳过原因随测试输出留档）
#[test]
fn f01_prepare_rejects_root_symlink() {
    let f = Fixture::new();
    let target = f.temp.path().join("real");
    fs::create_dir_all(&target).unwrap();
    let payload = target.join("资料.txt");
    fs::write(&payload, "内容").unwrap();
    let link = f.temp.path().join("软链");
    if let Err(reason) = make_symlink(&link, &target) {
        eprintln!("跳过原因：本机无法创建符号链接（mklink /D 需管理员或开发者模式）：{reason}");
        return;
    }

    let error = engine::prepare_at(&link, Config::default(), Context::default(), &f.state)
        .err()
        .map(|e| format!("{e:#}"));
    let error = error.expect("symlink 根必须被拒绝开始（S-04）");
    assert!(
        error.contains("S-04"),
        "拒绝文案必须明示 S-04 语义：{error}"
    );
    assert!(payload.exists(), "拒绝时链接目标树零改动");
    assert_eq!(count_task_dirs(&f.state), 0, "拒绝时不创建计划、不扫描");
}

// ---------------------------------------------------------------------------
// F02 · S-01：`.jchtools-link-*` 残留清理不得越出本次处理范围
// ---------------------------------------------------------------------------

// 覆盖 S-01（回归：glob 排除子树内的 .jchtools-link-* 硬链接不得在执行前被清理；
// 修复前 apply 的独立全盘 WalkDir 不应用本次范围设置，静默删除）
#[test]
fn f02_keeps_link_temps_inside_glob_excluded_subtree() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("excl")).unwrap();
    let keeper = f.root.join("keeper.txt");
    fs::write(&keeper, "唯一内容").unwrap();
    let residue = make_stale_link_temp(&f.root.join("excl"), &keeper, "glob");

    let cfg = Config {
        exclusions: "excl".into(),
        ..Config::default()
    };
    let task = engine::prepare_at(&f.root, cfg, Context::default(), &f.state).unwrap();
    engine::apply(&task.directory, Context::default()).unwrap();

    assert!(
        residue.exists(),
        "排除子树内的 .jchtools-link-* 不在本次范围内，不得删除（S-01）"
    );
    assert_eq!(fs::read_to_string(&residue).unwrap(), "唯一内容");
}

// 覆盖 S-01（回归：关闭递归后深层目录不在扫描范围内，其中的残留不得清理）
#[test]
fn f02_keeps_link_temps_below_nonrecursive_scope() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("sub")).unwrap();
    let keeper = f.root.join("keeper.txt");
    fs::write(&keeper, "唯一内容").unwrap();
    let residue = make_stale_link_temp(&f.root.join("sub"), &keeper, "deep");

    let cfg = Config {
        recursive: false,
        ..Config::default()
    };
    let task = engine::prepare_at(&f.root, cfg, Context::default(), &f.state).unwrap();
    engine::apply(&task.directory, Context::default()).unwrap();

    assert!(residue.exists(), "非递归范围之外的深层残留不得删除（S-01）");
}

// 覆盖 S-01（回归：隐藏属性范围外的残留（未勾选包含隐藏）不得清理）
#[test]
fn f02_keeps_hidden_link_temps_when_hidden_excluded() {
    let f = Fixture::new();
    let keeper = f.root.join("keeper.txt");
    fs::write(&keeper, "唯一内容").unwrap();
    let residue = make_stale_link_temp(&f.root, &keeper, "hidden");
    set_hidden(&residue);

    let cfg = Config {
        include_hidden: false,
        ..Config::default()
    };
    let task = engine::prepare_at(&f.root, cfg, Context::default(), &f.state).unwrap();
    engine::apply(&task.directory, Context::default()).unwrap();

    assert!(
        residue.exists(),
        "隐藏范围外的 .jchtools-link-* 不得删除（S-01）"
    );
}

// 覆盖 S-01（回归：用户已请求停止时不清理残留——本次执行的删除授权尚未生效）
#[test]
fn f02_keeps_link_temps_when_stop_requested_before_apply() {
    let f = Fixture::new();
    let keeper = f.root.join("keeper.txt");
    fs::write(&keeper, "唯一内容").unwrap();
    let residue = make_stale_link_temp(&f.root, &keeper, "cancel");

    let task =
        engine::prepare_at(&f.root, Config::default(), Context::default(), &f.state).unwrap();
    let control = Control::default();
    control.cancel();
    let cancelled = Context {
        control: Arc::new(control),
        events: None,
    };
    let result = engine::apply(&task.directory, cancelled);
    assert!(result.is_err(), "已请求停止的任务必须失败收尾，不得报成功");
    assert!(
        residue.exists(),
        "已请求停止时不得清理残留（用户尚未授权本次执行的任何删除）"
    );
}

// 覆盖 S-01（正例：确属程序残留——位于本次范围内、修改超 24 小时、仍是硬链接
// （内容另有保留文件持有）——执行开始时清理；同时是「程序自有残留」正例）
#[test]
fn f02_removes_registered_stale_residue_within_scope() {
    let f = Fixture::new();
    let keeper = f.root.join("keeper.txt");
    fs::write(&keeper, "唯一内容").unwrap();
    let residue = make_stale_link_temp(&f.root, &keeper, "true");

    let task =
        engine::prepare_at(&f.root, Config::default(), Context::default(), &f.state).unwrap();
    engine::apply(&task.directory, Context::default()).unwrap();

    assert!(!residue.exists(), "范围内的确属残留必须在执行开始时清理");
    let moved = f.root.join("文档").join("keeper.txt");
    assert!(
        moved.exists() && fs::read_to_string(&moved).unwrap() == "唯一内容",
        "保留文件内容必须完好（S-01：删除残留不得影响其他链接持有的内容）"
    );
}

// 覆盖 S-01（分析阶段必须把疑似残留明示：数量与清理条件进入任务日志，
// 不得在用户确认前静默决定删除范围）
#[test]
fn f02_analysis_logs_residue_notice_before_confirmation() {
    let f = Fixture::new();
    let keeper = f.root.join("keeper.txt");
    fs::write(&keeper, "唯一内容").unwrap();
    make_stale_link_temp(&f.root, &keeper, "notice");

    let task =
        engine::prepare_at(&f.root, Config::default(), Context::default(), &f.state).unwrap();
    let conn = rusqlite::Connection::open(task.directory.join("task.sqlite3")).unwrap();
    let reasons: Vec<String> = conn
        .prepare("SELECT reason FROM events")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains(".jchtools-link")),
        "分析日志必须明示疑似残留及清理条件：{reasons:?}"
    );
}
