//! 两工具拆分（CONTRACT X/C 分区，2026-09-18 重建）的引擎级回归：
//! - C-01 目录整理分析阶段只读：不解压、不改目录（解压职责已移交「递归解压」工具）；
//! - C-09 / X-07 扫描默认排除所选目录根下的「解压失败」子目录；
//! - X-06 解压失败的原包移入「解压失败」子目录（含分卷兄弟卷）；
//! - X-05 成功原包按处置策略处理（默认回收，见 tests/archive.rs 真实引擎用例）。
use jchtools::{config::*, control::Context, engine};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

mod common;
use common::MoveRecycle;

/// 假引擎：一个存在但不可执行/非 PE 的文件。任何解压命令都会失败，
/// 用于在无真实 7-Zip 的环境（CI / 本地默认）驱动「解压失败 → 隔离」路径。
fn fake_engine(dir: &Path) -> PathBuf {
    let path = dir.join("fake-7z.exe");
    fs::write(&path, b"not an executable").unwrap();
    path
}

fn fixture(_tag: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("data");
    fs::create_dir(&root).unwrap();
    tmp
}

fn state_of(tmp: &tempfile::TempDir) -> PathBuf {
    let state = tmp.path().join("state");
    fs::create_dir_all(&state).unwrap();
    state
}

fn write_with_mtime(path: &Path, bytes: &[u8], mtime: i64) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(mtime, 0)).unwrap();
}

// 覆盖 C-01
#[test]
fn organizer_analysis_is_read_only_and_never_extracts() {
    // 回归（2026-09-18 合同重建）：旧目录整理在「分析」阶段就实际解压（旧 C-01），
    // 无引擎时整个分析以「解压引擎缺失」失败。新合同把解压职责整体移交「递归解压」
    // 工具：目录整理的分析阶段必须只读——没有引擎也要成功生成计划，压缩包原样保留。
    let tmp = fixture("readonly");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("a.zip"), b"pretend archive bytes", 100);
    write_with_mtime(&root.join("b.txt"), b"loose file", 200);
    let cfg = Config::default(); // 组织器默认：不含解压
    let result = engine::prepare_with(
        &root,
        cfg,
        Context::default(),
        &state_of(&tmp),
        Arc::new(MoveRecycle::new(tmp.path().join("bin"))),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 0, "目录整理不得解压任何压缩包");
    assert_eq!(result.summary.archives_failed, 0);
    assert!(root.join("a.zip").exists(), "分析阶段不得动压缩包（只读）");
    // 压缩包作为普通文件参与计划（这里是唯一文件，无重复对，但必须被扫描到）
    assert_eq!(result.summary.scanned, 2, "压缩包与散文件都应被扫描");
}

// 覆盖 C-09
#[test]
fn organizer_scan_extracts_nothing_and_skips_failed_dir() {
    // 「解压失败」子目录默认整体排除（C-09）：里面的文件不参与去重/归类/清理。
    // 树里 root/dup.txt 与 root/解压失败/dup.txt 内容相同：若失败目录未被排除，
    // 去重会生成一条删除计划；排除后 root/dup.txt 是唯一候选，无删除计划。
    let tmp = fixture("skipdir");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("dup.txt"), b"same", 100);
    write_with_mtime(&root.join("解压失败").join("dup.txt"), b"same", 200);
    let result = engine::prepare_with(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Arc::new(MoveRecycle::new(tmp.path().join("bin"))),
    )
    .unwrap();
    assert_eq!(
        result.summary.planned_delete, 0,
        "「解压失败」目录内的文件不得参与去重（C-09 默认排除）"
    );
    assert_eq!(
        result.summary.scanned, 1,
        "只有失败目录之外的一个文件进入扫描"
    );
    assert!(
        root.join("解压失败").join("dup.txt").exists(),
        "失败目录内容保持原样"
    );
}

// 覆盖 X-06, X-02
#[test]
fn failed_archive_is_moved_to_quarantine_directory() {
    // 假引擎必然解压失败：原包必须移入所选目录根下的「解压失败」子目录（X-06），
    // 且任务正常收尾（一段确认连续执行，失败包不算任务失败——X-06 是既定处置而非故障）。
    let tmp = fixture("quarantine");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("broken.zip"), b"definitely not a zip", 100);
    write_with_mtime(
        &root.join("deep").join("nest.rar"),
        b"also not an archive",
        200,
    );
    let cfg = Config::default();
    let result = engine::extract_run_at(
        &root,
        cfg,
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
        Arc::new(MoveRecycle::new(tmp.path().join("bin"))),
    )
    .unwrap();
    assert_eq!(
        result.summary.archives_failed, 2,
        "两个坏包都应记为解压失败"
    );
    assert!(
        root.join("解压失败").join("broken.zip").exists(),
        "失败原包必须移入「解压失败」（X-06）"
    );
    assert!(
        root.join("解压失败").join("nest.rar").exists(),
        "深层失败原包同样移到根下隔离目录"
    );
    assert!(!root.join("broken.zip").exists(), "原位置不得残留失败原包");
    assert!(!root.join("deep").join("nest.rar").exists());
}

// 覆盖 X-07, X-06
#[test]
fn rerun_skips_quarantined_archives_and_reports_count() {
    // 第二次解压默认排除「解压失败」子目录（X-07）：隔离后的包不再重试；
    // 确认对话所用的包计数也不得把它们算进去。
    let tmp = fixture("rerun");
    let root = tmp.path().join("data");
    write_with_mtime(
        &root.join("解压失败").join("broken.zip"),
        b"definitely not a zip",
        100,
    );
    write_with_mtime(&root.join("fresh.zip"), b"also not a zip", 200);
    // 只数包（确认框内容），不解压：fresh.zip 计 1，隔离目录里的 broken.zip 不计。
    let count = engine::count_archives(&root, &Config::default()).unwrap();
    assert_eq!(count, 1, "重跑计数必须排除「解压失败」目录（X-07）");
    // 全量跑一遍：只有 fresh.zip 被尝试（并失败移入隔离目录），broken.zip 原地不动。
    let before = fs::metadata(root.join("解压失败").join("broken.zip"))
        .unwrap()
        .modified()
        .unwrap();
    let result = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
        Arc::new(MoveRecycle::new(tmp.path().join("bin"))),
    )
    .unwrap();
    assert_eq!(
        result.summary.archives_failed, 1,
        "隔离目录中的包不得重试（X-07）"
    );
    let after = fs::metadata(root.join("解压失败").join("broken.zip"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after, "隔离目录中的包保持原样，未被搬动或重试");
    assert!(
        root.join("解压失败").join("fresh.zip").exists(),
        "新失败包同样进入隔离目录"
    );
}

// 覆盖 X-06（分卷兄弟卷一并隔离）
#[test]
fn multipart_siblings_move_to_quarantine_together() {
    // 分卷来源不确定（X-06）：主体失败时，同目录下的兄弟卷一并移入「解压失败」，
    // 否则跑完后目录里仍残留压缩包，违反 X 分区「不残留压缩包」的总体约束。
    let tmp = fixture("multipart");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.part1.rar"), b"vol1-not-real", 100);
    write_with_mtime(&root.join("report.part2.rar"), b"vol2-not-real", 200);
    let result = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
        Arc::new(MoveRecycle::new(tmp.path().join("bin"))),
    )
    .unwrap();
    assert!(result.summary.archives_failed >= 1);
    assert!(
        root.join("解压失败").join("report.part1.rar").exists(),
        "分卷主体必须隔离"
    );
    assert!(
        root.join("解压失败").join("report.part2.rar").exists(),
        "兄弟卷必须一并隔离"
    );
    assert!(
        !root.join("report.part1.rar").exists() && !root.join("report.part2.rar").exists(),
        "目录里不得残留任何分卷（X 分区总体约束）"
    );
}

// 覆盖 X-03, X-05（无引擎时的行为：E-05 明确报错，不静默跳过）
#[test]
fn extract_run_without_engine_fails_loudly() {
    // 本机存在可用引擎（resources/7zip 或已释放副本）时无法构造「缺引擎」环境：
    // CI（无引擎）上才真正执行断言；有引擎的机器跳过，不伪造红灯。
    if jchtools::archive::SevenZip::from_bundle().is_ok() {
        eprintln!("skip：本机存在可用解压引擎，无法验证缺引擎路径（CI 覆盖）");
        return;
    }
    let tmp = fixture("noengine");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("pack.zip"), b"pretend archive bytes", 100);
    let result = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        None,
        Arc::new(MoveRecycle::new(tmp.path().join("bin"))),
    );
    let error = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
    assert!(
        error.contains("引擎") || error.contains("7z") || error.contains("7-Zip"),
        "缺引擎必须给明确错误，而不是静默跳过压缩包（E-05）：{error}"
    );
    assert!(root.join("pack.zip").exists(), "缺引擎时原包不得被处置");
}
