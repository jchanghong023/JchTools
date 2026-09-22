//! 两工具拆分（CONTRACT X/C 分区，2026-09-18 重建）的引擎级回归：
//! - C-01 目录整理分析阶段只读：不解压、不改目录（解压职责已移交「递归解压」工具）；
//! - C-09 / X-07 扫描默认排除所选目录根下的「解压失败」子目录；
//! - X-06 解压失败的原包移入「解压失败」子目录（含分卷兄弟卷）；
//! - X-05 成功原包及已佐证分卷在完整落盘后永久删除；失败、部分解开或取消时保留，
//!   仅同主干的无关文件不随包处置。
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

// 覆盖 C-13：三要素（文件标识/大小/修改时间）未变时跨运行复用哈希
#[test]
fn hash_cache_reuses_unchanged_files_across_runs() {
    // 反证构造：第一次分析后把副本原地改写成同长度的不同字节并还原修改时间——
    // 若第二次分析重新计算哈希，两文件内容不同、不再判重复；实测仍判重复，
    // 证明走的是缓存复用（C-13 的「三者未变即内容未变」信任假设）。
    let tmp = fixture("hashcache");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("a.txt"), b"same-bytes", 100);
    write_with_mtime(&root.join("sub").join("a.txt"), b"same-bytes", 100);
    let state = state_of(&tmp);
    let first = engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    assert_eq!(
        first.summary.planned_delete, 1,
        "一对真重复应生成一条删除计划"
    );
    let copy = root.join("sub").join("a.txt");
    let stamp = filetime::FileTime::from_last_modification_time(&fs::metadata(&copy).unwrap());
    fs::write(&copy, b"DIFF-BYTES").unwrap();
    filetime::set_file_mtime(&copy, stamp).unwrap();
    let second = engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    assert_eq!(
        second.summary.planned_delete, 1,
        "C-13：三要素未变时复用旧哈希——改写后（同长不同内容、还原时间戳）仍按缓存判重复"
    );
}

// 覆盖 C-13：大小变化必须重算，不得复用旧哈希
#[test]
fn hash_cache_recomputes_when_size_changes() {
    let tmp = fixture("hashcache2");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("a.txt"), b"same-bytes", 100);
    write_with_mtime(&root.join("sub").join("a.txt"), b"same-bytes", 100);
    let state = state_of(&tmp);
    let first = engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    assert_eq!(first.summary.planned_delete, 1);
    fs::write(root.join("sub").join("a.txt"), b"same-bytes!").unwrap();
    let second = engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    assert_eq!(
        second.summary.planned_delete, 0,
        "大小变化不在缓存键上，必须完整重算并发现内容已不同"
    );
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

/// 记录调用的恒成功引擎：每次调用把命令行追加到脚本同目录的 `fake-log.txt`，
/// 用于证明「哪些文件真的被引擎打开过」（不经引擎 = 完全不碰）。
fn logging_engine(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let (path, bytes) = (
        dir.join("fake-log.bat"),
        b"@echo off\r\n>> \"%~dp0fake-log.txt\" echo %*\r\nexit /b 0\r\n".as_slice(),
    );
    #[cfg(not(windows))]
    let (path, bytes) = (
        dir.join("fake-log.sh"),
        b"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/fake-log.txt\"\nexit 0\n"
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

// 覆盖 X-01：自动解压白名单之外的文件完全不碰——引擎一次都不得被调用，
// 文件字节原样保留，也不得被当成失败包隔离；只有白名单内的包被处理并清理。
#[test]
fn non_whitelisted_containers_are_never_opened() {
    let tmp = fixture("whitelist");
    let root = tmp.path().join("data");
    // 内容故意用合法 ZIP：引擎确实能打开，语义上却是安装介质、文档或程序包。
    let containers: [(&str, &[u8]); 15] = [
        ("visproww.cab", b"zip payload, but an install cab"),
        ("windows.iso", b"zip payload, but a disk image"),
        ("boot.wim", b"zip payload, but a system image"),
        ("install.esd", b"zip payload, but a system image"),
        ("legacy.lzh", b"zip payload, but a legacy container"),
        ("archive.cpio", b"zip payload, but a cpio archive"),
        ("report.docx", b"zip payload, but a document"),
        ("package.msi", b"zip payload, but an installer"),
        ("setup.exe", b"zip payload, but an installer"),
        ("app.apk", b"zip payload, but an android package"),
        ("library.jar", b"zip payload, but a java package"),
        ("wheel.whl", b"zip payload, but a python package"),
        ("book.epub", b"zip payload, but an ebook"),
        ("addon.crx", b"zip payload, but a browser extension"),
        ("styles.xpi", b"zip payload, but a browser extension"),
    ];
    for (index, (name, bytes)) in containers.iter().enumerate() {
        let stamp = 100 + i64::try_from(index).unwrap();
        write_with_mtime(&root.join(name), bytes, stamp);
    }
    write_with_mtime(&root.join("keep.zip"), b"the only whitelisted archive", 400);
    let engine = logging_engine(tmp.path());
    let result = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&engine),
    )
    .unwrap();
    let calls = fs::read_to_string(tmp.path().join("fake-log.txt")).unwrap_or_default();
    assert!(
        calls.contains("keep.zip"),
        "白名单内的包必须交给引擎处理：{calls}"
    );
    for (name, bytes) in containers {
        assert!(
            !calls.contains(name),
            "{name} 不属于自动解压白名单，引擎不得打开它：{calls}"
        );
        assert_eq!(
            fs::read(root.join(name)).unwrap(),
            bytes,
            "{name} 必须原样保留（不碰、不改、不移动）"
        );
    }
    assert!(
        !root.join("keep.zip").exists(),
        "白名单内的包完整成功后按 X-05 永久删除"
    );
    assert!(
        !root.join("解压失败").exists(),
        "白名单外的文件不是失败包，不得进隔离目录"
    );
    assert_eq!(result.summary.archives_ok, 1, "只应处理白名单内的那一个包");
    assert_eq!(result.summary.deleted, 1);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(result.summary.archives_failed, 0);
}

/// 恒成功引擎，且不带 -ba 的 `l` 子命令输出带多卷标志的档案头块（`--` 与
/// `----------` 之间）：模拟 7-Zip 对真多卷包的档案级 -slt 输出（zip 的
/// Volume Index 仅 IsMultiVol 时出现、rar 的 Is Volume 仅卷标志置位时出现），
/// 用于在无真实引擎环境驱动「档案级多卷佐证为真 → 宽命名兄弟卷整组处置」路径。
/// 带 -ba 的条目列表与解压子命令静默成功（0 条目）。
fn volume_index_engine(dir: &Path, volumes: usize) -> PathBuf {
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
            "echo Volumes = @COUNT@\n",
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
            "printf '%s\n' '--' 'Type = rar' 'Volumes = @COUNT@' 'Multivolume = +' 'Volume Index = 0' '----------'
",
            "exit 0
",
        ),
    );
    fs::write(&path, body.replace("@COUNT@", &volumes.to_string())).unwrap();
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

// 覆盖 X-05 / S-01：即使档案注释伪造多卷信息，也只有主体自身被删除，旁路文件不得被处置。
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
        "档案注释里伪造的多卷键不得成为处置佐证：无辜 .r00 必须留在原地"
    );
    assert!(
        !root.join("report.rar").exists(),
        "完整解开的主体按 X-05 永久删除"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "删除集合只含主体自身：伪造佐证不得把旁路文件算进来"
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
        "注释内 }} 行后的伪造键不得成为处置佐证：无辜 .r00 必须留在原地"
    );
    assert!(
        !root.join("report.rar").exists(),
        "完整解开的主体按 X-05 永久删除"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "删除集合只含主体自身，伪造佐证不得扩大处置范围"
    );
}

// 覆盖 X-05, S-01：单卷 ZIP 完整解开后主体删除，同主干的旧分卷文件必须原样留在原地。
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
        "成功原包按 X-05 永久删除"
    );
    assert!(
        root.join("report.z01").exists(),
        "无分卷佐证的同主干 .z01 是无辜文件，不得随包处置"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "删除集合只含主体自身（X-05：不得误删仅同主干的文件）"
    );
    assert_eq!(
        fs::read(root.join("report.z01")).unwrap(),
        b"stale fragment of an old set"
    );
}

// 覆盖 X-05, S-01（回归 2026-09-19：宽命名兄弟卷误处置——旧式 RAR 变体）。
// 同上，但走 oldrar 分支：完整单卷 report.rar 旁边的同主干 .r00 不因命名巧合
// 被认定为其分卷成员，主体成功删除，旁观文件原样保留。
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
        !root.join("report.rar").exists(),
        "成功原包按 X-05 永久删除"
    );
    assert!(
        root.join("report.r00").exists(),
        "无分卷佐证的同主干 .r00 是无辜文件，不得随包处置"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "删除集合只含主体自身（X-05：不得误删仅同主干的文件）"
    );
    assert_eq!(
        fs::read(root.join("report.r00")).unwrap(),
        b"stale fragment of an old set"
    );
}

// 覆盖 X-05：引擎档案级佐证为真的分卷组成功解压后，主体与全部分卷一并永久删除。
#[test]
fn corroborated_volume_siblings_are_deleted_together() {
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
        Some(&volume_index_engine(tmp.path(), 3)),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 1);
    assert!(!root.join("report.rar").exists(), "主体必须删除（X-05）");
    assert!(
        !root.join("report.r00").exists() && !root.join("report.r01").exists(),
        "已佐证的分卷必须随主体一并删除（X-05）"
    );
    assert_eq!(result.summary.deleted, 3, "主体与两个分卷各计一次删除");
    assert_eq!(result.summary.errors, 0, "正常删除不得计为错误");
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

/// 任务以错误结束（没有 TaskResult）时，打开本次运行的任务库：摘要与行状态都在里面。
#[cfg(windows)]
fn task_db(state: &Path) -> jchtools::db::Database {
    let task_dir = fs::read_dir(state.join("tasks"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .next()
        .expect("任务库目录应已创建");
    jchtools::db::Database::open(&task_dir).unwrap()
}

/// 任务库里停留在 running 的包行数（任务结束时必须为 0）。
#[cfg(windows)]
fn running_rows(db: &jchtools::db::Database) -> i64 {
    db.conn
        .query_row(
            "SELECT COUNT(*) FROM archives WHERE state='running'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

// 覆盖 X-05（回归 2026-09-22：成功原包的删除失败必须如实报错，不得虚报成功、不得隔离）。
// 旧口径「原包无需删除权限」已随 X-05 取消：完整解开但删不掉时，解压结果与原包全部保留，
// 任务以明确错误结束，不把清理失败当成坏包隔离。
// 平台门禁原因：只有 Windows 的共享模式（FILE_SHARE_READ 拒绝删除）能构造该失败。
#[cfg(windows)]
#[test]
fn read_locked_archives_surface_cleanup_failure_and_are_preserved() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = fixture("locked-source");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("a.zip"), b"fake archive a", 100);
    write_with_mtime(&root.join("b.zip"), b"fake archive b", 200);
    // 恒成功引擎：空输出表示空包，完整落盘后进入 X-05 删除。
    let engine_path = ok_engine(tmp.path());
    // Windows 读取共享句柄禁止删除：两个包的删除都必然失败（与处理顺序无关）。
    let _guards: Vec<fs::File> = ["a.zip", "b.zip"]
        .iter()
        .map(|name| {
            let mut opts = fs::OpenOptions::new();
            opts.read(true).share_mode(1); // FILE_SHARE_READ：拒绝写入与删除
            opts.open(root.join(name)).unwrap()
        })
        .collect();
    let state = state_of(&tmp);
    let _error = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state,
        Some(&engine_path),
    )
    .expect_err("源包删除失败必须报错（X-05）");
    assert_eq!(fs::read(root.join("a.zip")).unwrap(), b"fake archive a");
    assert_eq!(fs::read(root.join("b.zip")).unwrap(), b"fake archive b");
    assert!(
        !root.join("解压失败").exists(),
        "清理失败不得被当成坏包隔离（X-05）"
    );
    let db = task_db(&state);
    let summary = db.summary().unwrap();
    assert_eq!(summary.archives_ok, 0, "清理失败不得虚计成功（X-05）");
    assert_eq!(summary.archives_failed, 0, "不得把清理失败记为坏包");
    assert!(summary.errors > 0, "清理失败必须计入错误项");
    assert_eq!(running_rows(&db), 0, "包行不得停留在 running");
}

// 覆盖 X-05（部分删除的中止口径）：已佐证分卷组里主体删除成功、随后某个分卷删除失败时，
// 不得隔离这组「已部分删除」的包、不得回滚已删除的卷、也不得虚计成功；未删除的分卷保留，
// 错误明确说明剩余数量，任务停止且不再处理后续包。
// 平台门禁原因：只有 Windows 的共享模式（FILE_SHARE_READ 拒绝删除）能构造该失败。
#[cfg(windows)]
#[test]
fn partially_deleted_volume_group_reports_failure_without_quarantine() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = fixture("locked-volume");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("report.rar"), b"multi-volume main", 100);
    write_with_mtime(&root.join("report.r00"), b"volume 0", 200);
    // 佐证为真的分卷组：删除集合是 [主体, .r00]；只锁住 .r00，主体必然先被删除。
    let _guard = {
        let mut opts = fs::OpenOptions::new();
        opts.read(true).share_mode(1); // FILE_SHARE_READ：拒绝写入与删除
        opts.open(root.join("report.r00")).unwrap()
    };
    let state = state_of(&tmp);
    let _error = engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state,
        Some(&volume_index_engine(tmp.path(), 2)),
    )
    .expect_err("分卷删除失败必须报错（X-05）");
    assert!(
        !root.join("report.rar").exists(),
        "先删除的主体不回滚（X-05 不承诺多卷删除事务性）"
    );
    assert!(
        root.join("report.r00").exists(),
        "未删除的分卷保留在原位，不得顺手隔离或删除"
    );
    assert!(
        !root.join("解压失败").exists(),
        "已部分删除的包组不得被当成坏包隔离（X-05）"
    );
    let db = task_db(&state);
    let summary = db.summary().unwrap();
    assert_eq!(summary.archives_ok, 0, "清理失败不得虚计成功（X-05）");
    assert_eq!(summary.deleted, 1, "只计真实发生的那一次删除");
    assert!(summary.errors > 0, "清理失败必须计入错误项");
    assert_eq!(running_rows(&db), 0, "包行不得停留在 running");
}
