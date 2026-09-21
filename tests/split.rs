//! 两工具拆分（CONTRACT X/C 分区，2026-09-18 重建）的引擎级回归：
//! - C-01 目录整理分析阶段只读：不解压、不改目录（解压职责已移交「递归解压」工具）；
//! - C-09 / X-07 扫描默认排除所选目录根下的「解压失败」子目录；
//! - X-06 解压失败的原包移入「解压失败」子目录（含分卷兄弟卷）；
//! - X-05 成功原包按处置策略处理（默认回收，见 tests/archive.rs 真实引擎用例）。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools::{config::*, control::Context, engine};
use std::{
    fs,
    path::{Path, PathBuf},
};

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
    let result = engine::prepare_at(&root, cfg, Context::default(), &state_of(&tmp)).unwrap();
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
    let result = engine::prepare_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
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
    );
    let error = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
    assert!(
        error.contains("引擎") || error.contains("7z") || error.contains("7-Zip"),
        "缺引擎必须给明确错误，而不是静默跳过压缩包（E-05）：{error}"
    );
    assert!(root.join("pack.zip").exists(), "缺引擎时原包不得被处置");
}

// 覆盖 X-06（隔离必须记录每个包的失败原因，日志字段可查）
#[test]
fn quarantined_archive_reason_is_recorded_in_log() {
    use jchtools::db::Database;
    let tmp = fixture("reason");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("broken.zip"), b"definitely not a zip", 100);
    let result = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
    )
    .unwrap();
    let db = Database::open(&result.directory).unwrap();
    let events = db.event_page(0, 100).unwrap();
    let line = events
        .iter()
        .find(|line| line.contains("移入解压失败"))
        .expect("日志必须记录隔离事件");
    assert!(line.contains("broken.zip"), "记录必须归属到原包：{line}");
    assert!(
        line.contains("解压失败"),
        "失败原因必须随隔离记录（界面可查）：{line}"
    );
}

/// 恒成功引擎：任何子命令都 0 退出且无输出（0 条目 → 解压「成功」）。
/// 与 dispose_failure_does_not_abort_remaining_archives 的内联版本同构，供分卷回归复用。
fn ok_engine(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let (path, bytes) = (
        dir.join("fake-ok.bat"),
        b"@exit /b 0
"
        .as_slice(),
    );
    #[cfg(not(windows))]
    let (path, bytes) = (
        dir.join("fake-ok.sh"),
        b"#!/bin/sh
exit 0
"
        .as_slice(),
    );
    fs::write(&path, bytes).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// 恒成功引擎，且不带 -ba 的 `l` 子命令输出带多卷标志的档案头块（`--` 与
/// `----------` 之间）：模拟 7-Zip 对真多卷包的档案级 -slt 输出（zip 的
/// Volume Index 仅 IsMultiVol 时出现、rar 的 Is Volume 仅卷标志置位时出现），
/// 用于在无真实引擎环境驱动「档案级多卷佐证为真 → 宽命名兄弟卷整组处置」路径。
/// 带 -ba 的条目列表与解压子命令静默成功（0 条目）。
fn volume_index_engine(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let (path, body) = (
        dir.join("fake-volume-list.bat"),
        concat!(
            "@echo off
",
            "if not \"%~1\"==\"l\" exit /b 0
",
            "if \"%~3\"==\"-ba\" exit /b 0
",
            "echo --
",
            "echo Type = rar
",
            "echo Multivolume = +
",
            "echo Volume Index = 0
",
            "echo ----------
",
            "exit /b 0
",
        ),
    );
    #[cfg(not(windows))]
    let (path, body) = (
        dir.join("fake-volume-list.sh"),
        concat!(
            "#!/bin/sh
",
            "[ \"$1\" = \"l\" ] || exit 0
",
            "[ \"$3\" = \"-ba\" ] && exit 0
",
            "printf '%s\n' '--' 'Type = rar' 'Multivolume = +' 'Volume Index = 0' '----------'
",
            "exit 0
",
        ),
    );
    fs::write(&path, body).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// 伪造注释头假引擎：不带 -ba 的 `l` 输出带多卷键的档案注释块（`{`…`}` 内），
/// 模拟 crafted 档案把 `Volumes = 9` 等键写进 zip/rar 档案注释、借 7-Zip 原样
/// 输出伪造多卷佐证的对抗形态。修复后注释块内的键不得参与判定。
fn comment_forging_engine(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let (path, body) = (
        dir.join("fake-comment-forgery.bat"),
        concat!(
            "@echo off
",
            "if not \"%~1\"==\"l\" exit /b 0
",
            "if \"%~3\"==\"-ba\" exit /b 0
",
            "echo --
",
            "echo Type = rar
",
            "echo Comment = 
",
            "echo {
",
            "echo Volumes = 9
",
            "echo Volume Index = 0
",
            "echo Multivolume = +
",
            "echo }
",
            "echo ----------
",
            "exit /b 0
",
        ),
    );
    #[cfg(not(windows))]
    let (path, body) = (
        dir.join("fake-comment-forgery.sh"),
        concat!(
            "#!/bin/sh
",
            "[ \"$1\" = \"l\" ] || exit 0
",
            "[ \"$3\" = \"-ba\" ] && exit 0
",
            "printf '%s\n' '--' 'Type = rar' 'Comment = ' '{' 'Volumes = 9' 'Volume Index = 0' 'Multivolume = +' '}' '----------'
",
            "exit 0
",
        ),
    );
    fs::write(&path, body).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

// 覆盖 X-05, S-01（回归 2026-09-19：crafted 档案注释伪造多卷佐证）。
// 7-Zip 把档案注释在 -slt 头块内以 {...} 原样逐行输出；注释里的
// `Volumes = 9` 等键若参与佐证判定，恶意档案即可让同主干无辜 .rNN/.zNN
// 随成功包被回收甚至永久删除。注释块内的键必须被忽略。真实引擎同型回归见
// tests/archive.rs real_engine_forged_comment_cannot_enable_sweep。
#[test]
fn forged_comment_corroboration_is_ignored() {
    let tmp = fixture("forged-comment");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.rar"), b"standalone rar", 100);
    write_with_mtime(&root.join("report.r00"), b"innocent bystander", 200);
    let result = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&comment_forging_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        root.join("report.r00").exists(),
        "档案注释里伪造的多卷键不得成为处置佐证"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "只有主体原包被永久处置，无辜 .r00 不得计入（S-02）"
    );
}

/// 伪造注释头假引擎（嵌套 } 变体）：7-Zip 把档案注释以 {...} 原样逐行输出，
/// 注释内容本身可以含 } 行——若解析器遇到注释内的 } 就退出注释跳过，其后
/// 攻击者控制的伪造键会被当作档案头键解析。修复后 } 结束注释的同时必须结束
/// 档案头块（真实输出中 Comment 是头块最后一个字段，键不会出现在其后）。
fn comment_escape_forging_engine(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let (path, body) = (
        dir.join("fake-comment-escape.bat"),
        concat!(
            "@echo off
",
            "if not \"%~1\"==\"l\" exit /b 0
",
            "if \"%~3\"==\"-ba\" exit /b 0
",
            "echo --
",
            "echo Type = rar
",
            "echo Comment = 
",
            "echo {
",
            "echo }
",
            "echo Volume Index = 0
",
            "echo Volumes = 9
",
            "echo Multivolume = +
",
            "echo }
",
            "echo ----------
",
            "exit /b 0
",
        ),
    );
    #[cfg(not(windows))]
    let (path, body) = (
        dir.join("fake-comment-escape.sh"),
        concat!(
            "#!/bin/sh
",
            "[ \"$1\" = \"l\" ] || exit 0
",
            "[ \"$3\" = \"-ba\" ] && exit 0
",
            "printf '%s\n' '--' 'Type = rar' 'Comment = ' '{' '}' 'Volume Index = 0' 'Volumes = 9' 'Multivolume = +' '}' '----------'
",
            "exit 0
",
        ),
    );
    fs::write(&path, body).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

// 覆盖 X-05, S-01（回归 2026-09-19：注释内 } 行绕过注释跳过——crafted 对抗变体）。
#[test]
fn forged_comment_with_brace_line_cannot_enable_sweep() {
    let tmp = fixture("forged-comment-brace");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.rar"), b"standalone rar", 100);
    write_with_mtime(&root.join("report.r00"), b"innocent bystander", 200);
    let result = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&comment_escape_forging_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        root.join("report.r00").exists(),
        "注释内 }} 行后的伪造键不得成为处置佐证"
    );
}

// 覆盖 X-05, S-01（回归 2026-09-19：宽命名兄弟卷误处置——zip 变体）。
// 完整单卷 zip 替换旧分卷集合后残留的同主干 .z01 不是该包的分卷成员：处置
// （不可逆，含永久删除模式）必须以引擎档案级属性证实多卷（条目级 Volume Index
// 对 zip 单卷包也无条件输出，不能作证），否则无辜文件会随成功包一并被回收甚至
// 永久删除。真实引擎下的同型回归见 tests/archive.rs real_engine_standalone_zip。
#[test]
fn stale_z_sibling_is_not_disposed_with_standalone_zip() {
    let tmp = fixture("stale-z");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.zip"), b"standalone complete zip", 100);
    write_with_mtime(
        &root.join("report.z01"),
        b"stale fragment of an old set",
        200,
    );
    let result = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&ok_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        !root.join("report.zip").exists(),
        "成功原包按 X-05/S-02 直接永久删除"
    );
    assert!(
        root.join("report.z01").exists(),
        "无分卷佐证的同主干 .z01 是无辜文件，不得随包处置"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "成功原包按 X-05/S-02 直接永久删除并计入"
    );
    assert!(
        root.join("report.z01").exists(),
        "无辜 .z01 不得被处置（S-02：只按佐证处置主体）"
    );
    // 无佐证保留必须有用户可见日志（否则残留无声、用户不可感知）。
    let db = jchtools::db::Database::open(&result.directory).unwrap();
    let events = db.event_page(0, 100).unwrap();
    assert!(
        events.iter().any(|line| line.contains("未能证实分卷关系")),
        "无佐证保留兄弟卷必须留日志：{events:?}"
    );
}

// 覆盖 X-05, S-01（回归 2026-09-19：宽命名兄弟卷误处置——旧式 RAR 变体）。
// 同上，但走 oldrar 分支：完整单卷 report.rar 旁边的同主干 .r00 不因命名巧合
// 被认定为其分卷成员。
#[test]
fn stale_r_sibling_is_not_disposed_with_standalone_rar() {
    let tmp = fixture("stale-r");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.rar"), b"standalone complete rar", 100);
    write_with_mtime(
        &root.join("report.r00"),
        b"stale fragment of an old set",
        200,
    );
    let result = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&ok_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        root.join("report.r00").exists(),
        "无分卷佐证的同主干 .r00 是无辜文件，不得随包处置"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "只有主体原包被永久处置，无辜 .r00 不得计入（S-02）"
    );
}

// 覆盖 X-05（分卷佐证为真 → 兄弟卷整组处置）：条目带 Volume Index 的多卷包
// （zip/rar/rar5 多卷时 7-Zip 才输出该字段）成功后兄弟卷必须一并处置，
// 目录不残留压缩包（X 分区总体约束）。
#[test]
fn corroborated_volume_siblings_are_disposed_together() {
    let tmp = fixture("corroborated");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.rar"), b"multi-volume main", 100);
    write_with_mtime(&root.join("report.r00"), b"volume 0", 200);
    write_with_mtime(&root.join("report.r01"), b"volume 1", 300);
    let result = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&volume_index_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        !root.join("report.rar").exists()
            && !root.join("report.r00").exists()
            && !root.join("report.r01").exists(),
        "佐证为真的分卷组必须整组处置，目录不残留压缩包"
    );
    assert_eq!(
        result.summary.deleted, 3,
        "佐证为真的分卷组（主体 + 两个兄弟卷）整组永久处置"
    );
}

// 覆盖 X-06（失败路径的宽命名兄弟卷仍整组隔离）：隔离是可逆改名而非删除；
// 真 PKZIP/旧 RAR 分卷集失败时 7-Zip 常无法从主体确认成员关系，若失败路径也
// 要求佐证，真兄弟卷会残留在原目录（.zNN/.rNN 不是扫描口径内的压缩包，永远
// 不会再被处理）。宁可整组隔离（可还原、有日志），处置路径才设佐证门。
#[test]
fn failed_archive_still_quarantines_wide_named_siblings() {
    let tmp = fixture("quarantine-wide");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("combo.zip"), b"corrupt standalone zip", 100);
    write_with_mtime(&root.join("combo.z01"), b"possibly related fragment", 200);
    let result = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_failed, 1);
    assert!(
        root.join("解压失败").join("combo.zip").exists(),
        "失败主体必须移入「解压失败」（X-06）"
    );
    assert!(
        root.join("解压失败").join("combo.z01").exists(),
        "宽命名兄弟卷随主体整组隔离（可逆），不得残留在原目录"
    );
}

// 覆盖 X-02, X-06（单包处置失败不得中止整个解压任务：逐包隔离，任务继续）
#[test]
fn dispose_failure_does_not_abort_remaining_archives() {
    let tmp = fixture("dispose-fail");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("a.zip"), b"fake archive a", 100);
    write_with_mtime(&root.join("b.zip"), b"fake archive b", 200);
    // 恒成功引擎：空输出 → 0 条目 → 解压"成功"，随后原包处置走回收站
    let engine_path = {
        #[cfg(windows)]
        {
            let path = tmp.path().join("fake-ok.bat");
            fs::write(&path, b"@exit /b 0\r\n").unwrap();
            path
        }
        #[cfg(not(windows))]
        {
            let path = tmp.path().join("fake-ok.sh");
            fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    };
    // 处置失败必须保留原包、不得中止任务。故障注入：持有 FILE_SHARE_READ 打开句柄，
    // 永久删除以共享冲突失败（与「原包被其他程序占用」同因）。
    // 平台门禁原因（P-07 仅支持 Windows）：只有 Windows 的共享模式能阻止删除；
    // Unix 上打开句柄不阻止 unlink，故该平台不断言「处置失败」这一分支。
    #[cfg(windows)]
    let _guards: Vec<fs::File> = ["a.zip", "b.zip"]
        .iter()
        .map(|name| {
            use std::os::windows::fs::OpenOptionsExt;
            let mut opts = fs::OpenOptions::new();
            opts.read(true).share_mode(1); // FILE_SHARE_READ：拒绝写入与删除
            opts.open(root.join(name)).unwrap()
        })
        .collect();
    let cfg = Config::default();
    let result = engine::extract_run_at(
        &root,
        cfg,
        Context::default(),
        &state_of(&tmp),
        Some(&engine_path),
    );
    let summary = result.expect("单包处置失败不得中止任务").summary;
    assert_eq!(summary.archives_ok, 2, "两个包都应解压成功");
    #[cfg(windows)]
    assert!(summary.errors >= 1, "处置失败必须如实记为错误（而非静默）");
    #[cfg(not(windows))]
    assert_eq!(
        summary.errors, 0,
        "非 Windows 无共享冲突语义：处置正常完成，不得报错"
    );
    #[cfg(windows)]
    assert!(
        root.join("a.zip").exists() && root.join("b.zip").exists(),
        "处置失败时原包保留原地（重跑可自愈）"
    );
}
