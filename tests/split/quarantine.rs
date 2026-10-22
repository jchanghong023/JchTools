use super::*;
// 覆盖 S-04, X-06（回归：「解压失败」容器位置是 junction/符号链接时不得穿透链接
// 把失败包移出所选根——按「容器被占用」走隔离失败原地保留）
#[cfg(windows)]
#[test]
fn quarantine_refuses_junction_container() {
    let tmp = fixture("quarantine-junction");
    let root = tmp.path().join("data");
    fs::create_dir_all(&root).unwrap();
    write_with_mtime(&root.join("broken.zip"), b"definitely not a zip", 100);
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(root.join("解压失败"))
        .arg(&outside)
        .status()
        .unwrap();
    assert!(status.success(), "无法创建 junction，用例前置条件不成立");
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_failed, 1);
    assert!(
        root.join("broken.zip").exists(),
        "链接占位时失败原包原地保留（S-04：不穿透链接）"
    );
    assert!(
        !outside.join("broken.zip").exists(),
        "失败包不得经 junction 被移出所选根"
    );
}

// 覆盖 X-10（回归：同主干混用补零宽度的 part rar 是命名歧义组——列出全部歧义卷、
// 不猜测归属、不解码删源，整组按失败包隔离保留）
#[test]
fn ambiguous_part_rar_group_is_quarantined_without_decoding() {
    use jchtools_file_core::db::Database;
    let tmp = fixture("part-ambiguity");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("a.part1.rar"), b"width-1 volume", 100);
    write_with_mtime(&root.join("a.part01.rar"), b"width-2 one!", 200);
    write_with_mtime(&root.join("a.part02.rar"), b"width-2 two!", 300);
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_ok, 0, "歧义组不得解码删源");
    let quarantined = root.join("解压失败");
    assert!(quarantined.join("a.part1.rar").exists(), "歧义卷整组隔离");
    assert!(quarantined.join("a.part01.rar").exists());
    assert!(quarantined.join("a.part02.rar").exists());
    assert!(!root.join("a.part1.rar").exists(), "原位置不得残留歧义卷");
    let db = Database::open(&result.directory).unwrap();
    let events = event_lines(&db, 100);
    assert!(
        events.iter().any(|line| line.contains("命名歧义")),
        "失败原因必须列出命名歧义：{events:?}"
    );
}

// 覆盖 X-06（回归：固定「解压失败」容器被普通文件占用时，失败包原地保留并记录
// 隔离失败，其余包可继续处理——隔离失败不得中止整次任务）
#[test]
fn quarantine_failure_keeps_other_archives_processing() {
    use jchtools_file_core::db::Database;
    let tmp = fixture("quarantine-occupied");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("broken.zip"), b"definitely not a zip", 100);
    write_with_mtime(
        &root.join("deep").join("nest.rar"),
        b"also not an archive",
        200,
    );
    // 容器位置被普通文件占用：quarantine 无法建立「解压失败」子目录。
    write_with_mtime(&root.join("解压失败"), b"occupied by a plain file", 50);
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
    )
    .unwrap();
    // 修复前：第一个坏包的隔离失败直接中止整次任务（extract_run_at 返回 Err）；
    // 修复后两个包都完成「失败 → 隔离失败 → 原地保留」处置，任务正常收尾。
    assert_eq!(
        result.summary.archives_failed, 2,
        "两个坏包都按失败处置，不因隔离失败中止"
    );
    assert!(
        root.join("broken.zip").exists(),
        "隔离失败时失败原包原地保留（X-06）"
    );
    assert!(
        root.join("deep").join("nest.rar").exists(),
        "隔离失败后其余包照常处理并同样原地保留"
    );
    // U-10：隔离失败必须如实记录（不得写成已隔离），原因可查。
    let db = Database::open(&result.directory).unwrap();
    let events = event_lines(&db, 100);
    assert!(
        events.iter().any(|line| line.contains("隔离失败")),
        "日志必须记录隔离失败事件：{events:?}"
    );
    assert!(
        !events.iter().any(|line| {
            let fields: Vec<&str> = line.split(" | ").collect();
            fields.len() == 6 && fields[2] == "移入解压失败" && fields[3].contains("broken.zip")
        }),
        "容器被占用时不得出现「移入解压失败」的虚假成功记录"
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
    let result = jchtools_recursive_extract::engine::extract_run_at(
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
    let count =
        jchtools_recursive_extract::engine::count_archives(&root, &Config::default()).unwrap();
    assert_eq!(count, 1, "重跑计数必须排除「解压失败」目录（X-07）");
    // 全量跑一遍：只有 fresh.zip 被尝试（并失败移入隔离目录），broken.zip 原地不动。
    let before = fs::metadata(root.join("解压失败").join("broken.zip"))
        .unwrap()
        .modified()
        .unwrap();
    let result = jchtools_recursive_extract::engine::extract_run_at(
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
    let result = jchtools_recursive_extract::engine::extract_run_at(
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

// 覆盖 X-06（隔离必须记录每个包的失败原因，日志字段可查）
#[test]
fn quarantined_archive_reason_is_recorded_in_log() {
    use jchtools_file_core::db::Database;
    let tmp = fixture("reason");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("broken.zip"), b"definitely not a zip", 100);
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
    )
    .unwrap();
    let db = Database::open(&result.directory).unwrap();
    let events = event_lines(&db, 100);
    let line = events
        .iter()
        .find(|line| line.contains("移入解压失败"))
        .expect("日志必须记录隔离事件");
    // 行格式为 time | phase | result | source | target | reason（event_lines 助手）。
    // 「解压失败」是「移入解压失败」的子串——contains 断言对它恒真；X-06/U-10 要求
    // 的是 reason 列有实际失败原因，必须按列拆开核对。
    let fields: Vec<&str> = line.split(" | ").collect();
    assert_eq!(
        fields.len(),
        6,
        "事件行应为六列（time|phase|result|source|target|reason）：{line}"
    );
    assert_eq!(fields[2], "移入解压失败", "结果列应为隔离：{line}");
    assert!(
        fields[3].contains("broken.zip"),
        "记录必须归属到原包：{line}"
    );
    assert!(
        !fields[5].trim().is_empty(),
        "失败原因列不得为空（X-06：逐包记录原因，界面可查）：{line}"
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
    let result = jchtools_recursive_extract::engine::extract_run_at(
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

// 覆盖 X-06, X-10（回归：隔离按整组一次规划目标名——「解压失败」内已有同名
// 占用时整组统一改主干，组员不再分家）
#[test]
fn quarantined_split_zip_group_renames_stem_as_a_whole() {
    // 修复前：combo.zip → combo (1).zip，combo.z01 → combo.z01，组员主干不一致；
    // 修复后：按 X-10 统一为 combo (1).zip / combo (1).z01，后缀原样保留。
    let tmp = fixture("quarantine-group-zip");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("combo.zip"), b"broken main", 100);
    write_with_mtime(&root.join("combo.z01"), b"broken volume", 200);
    write_with_mtime(
        &root.join("解压失败").join("combo.zip"),
        b"earlier failure",
        300,
    );
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_failed, 1);
    assert_eq!(
        fs::read(root.join("解压失败").join("combo.zip")).unwrap(),
        b"earlier failure",
        "既有占用文件必须原样保留（X-06 不覆盖）"
    );
    assert!(
        !root.join("解压失败").join("combo.z01").exists(),
        "组员不得再以原名落盘：整组统一改主干（X-10）"
    );
    assert!(
        root.join("解压失败").join("combo (1).zip").is_file(),
        "主体以统一改后的主干隔离（X-10）"
    );
    assert!(
        root.join("解压失败").join("combo (1).z01").is_file(),
        "兄弟卷跟随同一主干，后缀原样保留（X-10）"
    );
}

// 覆盖 X-06, X-10（回归：编号卷集冲突时整组统一改主干，编号与补零原样保留）
#[test]
fn quarantined_numbered_group_renames_stem_as_a_whole() {
    // 修复前：data.7z.001 原名进入「解压失败」，data.7z.002 因占用改成
    // data (1).7z.002，整组分家；修复后统一为 data (1).7z.001 / data (1).7z.002。
    let tmp = fixture("quarantine-group-numbered");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("data.7z.001"), b"vol1", 100);
    write_with_mtime(&root.join("data.7z.002"), b"vol2", 200);
    write_with_mtime(
        &root.join("解压失败").join("data.7z.002"),
        b"earlier failure",
        300,
    );
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_failed, 1);
    assert!(
        !root.join("解压失败").join("data.7z.001").exists(),
        "组员不得再以原名落盘：整组统一改主干（X-10）"
    );
    assert_eq!(
        fs::read(root.join("解压失败").join("data.7z.002")).unwrap(),
        b"earlier failure",
        "既有占用文件必须原样保留（X-06 不覆盖）"
    );
    assert!(root.join("解压失败").join("data (1).7z.001").is_file());
    assert!(root.join("解压失败").join("data (1).7z.002").is_file());
}

// 覆盖 X-02/X-06/X-09/X-10：只剩自然增长尾卷时仍清点一包，缺入口整组隔离。
#[test]
fn numbered_volume_growth_tail_only_group_counts_once_and_preserves_bytes() {
    let tmp = fixture("numbered-growth-tail-only");
    let root = tmp.path().join("data");
    for (name, bytes) in [
        ("资料.7z.1000", b"volume 1000".as_slice()),
        ("资料.7z.1001", b"volume 1001".as_slice()),
        ("资料.7z.01000", b"noncanonical".as_slice()),
        ("资料.iso.1000", b"outside whitelist".as_slice()),
        ("资料.7z.1000.bak", b"backup".as_slice()),
    ] {
        write_with_mtime(&root.join(name), bytes, 100);
    }
    let cfg = Config::default();
    assert_eq!(
        jchtools_recursive_extract::engine::count_archives(&root, &cfg).unwrap(),
        1
    );
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        cfg,
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_failed, 1);
    assert_eq!(result.summary.archives_ok, 0);
    assert_eq!(result.summary.deleted, 0);
    for (name, bytes) in [
        ("资料.7z.1000", b"volume 1000"),
        ("资料.7z.1001", b"volume 1001"),
    ] {
        assert_eq!(fs::read(root.join("解压失败").join(name)).unwrap(), bytes);
        assert!(!root.join(name).exists());
    }
    for (name, bytes) in [
        ("资料.7z.01000", b"noncanonical".as_slice()),
        ("资料.iso.1000", b"outside whitelist".as_slice()),
        ("资料.7z.1000.bak", b"backup".as_slice()),
    ] {
        assert_eq!(fs::read(root.join(name)).unwrap(), bytes);
    }
    let db = jchtools_file_core::db::Database::open(&result.directory).unwrap();
    assert!(event_lines(&db, 100)
        .iter()
        .any(|line| line.contains("缺起始卷")));
}

// 覆盖 X-06, X-10（回归：part rar 族隔离示例 data (1).part01.rar / data (1).part02.rar，
// `.partN.rar` 整体作为不可拆分后缀，序号插在主干之后）
#[test]
fn quarantined_part_rar_group_renames_stem_as_a_whole() {
    let tmp = fixture("quarantine-group-part");
    let root = tmp.path().join("data");
    write_with_mtime(&root.join("data.part01.rar"), b"vol1", 100);
    write_with_mtime(&root.join("data.part02.rar"), b"vol2", 200);
    write_with_mtime(
        &root.join("解压失败").join("data.part01.rar"),
        b"earlier failure",
        300,
    );
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &root,
        Config::default(),
        Context::default(),
        &state_of(&tmp),
        Some(&fake_engine(tmp.path())),
    )
    .unwrap();
    assert_eq!(result.summary.archives_failed, 1);
    assert!(
        root.join("解压失败").join("data (1).part01.rar").is_file()
            && root.join("解压失败").join("data (1).part02.rar").is_file(),
        "整组统一改主干，`.partN.rar` 后缀原样保留（X-10）"
    );
    assert!(
        !root.join("解压失败").join("data.part01 (1).rar").exists(),
        "不得把 part 段留在主体里按普通扩展名改名（X-10/附录 B）"
    );
}
