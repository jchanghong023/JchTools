use super::*;

// P3-06 / X-02 / S-04：不可无损表示的根目录子项只逐项跳过；
// 确认清点不得阻断正常卷集，正式执行须保留原始 UTF-16 名称/内容并说明跳过原因。
#[cfg(windows)]
#[test]
fn review_remediation_count_and_extract_skip_unpaired_surrogate_entries() {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let f = Fixture::new();
    let file_units: Vec<u16> = "skip-file-"
        .encode_utf16()
        .chain([0xd800])
        .chain(".txt".encode_utf16())
        .collect();
    let dir_units: Vec<u16> = "skip-dir-".encode_utf16().chain([0xdc00]).collect();
    let skipped_file = f.root.join(OsString::from_wide(&file_units));
    let skipped_dir = f.root.join(OsString::from_wide(&dir_units));
    assert!(skipped_file.to_str().is_none(), "前置：文件名含孤立高代理");
    assert!(skipped_dir.to_str().is_none(), "前置：目录名含孤立低代理");
    fs::write(&skipped_file, b"unrepresentable file payload").unwrap();
    fs::create_dir(&skipped_dir).unwrap();
    // 目录必须整树剪枝，其内可归组的缺首卷包也不能计入或被隔离。
    fs::write(
        skipped_dir.join("hidden.part02.rar"),
        b"untouched inner part2",
    )
    .unwrap();
    fs::write(
        skipped_dir.join("hidden.part03.rar"),
        b"untouched inner part3",
    )
    .unwrap();
    f.write("normal.part02.rar", b"normal part2");
    f.write("normal.part03.rar", b"normal part3");

    let count = jchtools_recursive_extract::engine::count_archives(&f.root, &base())
        .expect("P3-06：不可无损表示的无关项不得使确认清点整批失败");
    assert_eq!(count, 1, "只计正常名称的缺首卷组，不下钻不可表示目录");
    let task = jchtools_recursive_extract::engine::extract_run_at(
        &f.root,
        base(),
        Context::default(),
        &f.state,
        None,
    )
    .unwrap();
    assert_eq!(task.summary.archives_ok, 0);
    assert_eq!(task.summary.archives_failed, 1, "正常缺首卷组仍实际处理");
    for (name, bytes) in [
        ("normal.part02.rar", b"normal part2".as_slice()),
        ("normal.part03.rar", b"normal part3".as_slice()),
    ] {
        assert_eq!(
            fs::read(f.root.join("解压失败").join(name)).unwrap(),
            bytes,
            "正常卷集应整组原样隔离"
        );
        assert!(!f.root.join(name).exists(), "正常卷集原位不得残留");
    }
    assert_eq!(
        fs::read(&skipped_file).unwrap(),
        b"unrepresentable file payload",
        "跳过文件原字节不变"
    );
    for (name, bytes) in [
        ("hidden.part02.rar", b"untouched inner part2".as_slice()),
        ("hidden.part03.rar", b"untouched inner part3".as_slice()),
    ] {
        assert_eq!(
            fs::read(skipped_dir.join(name)).unwrap(),
            bytes,
            "跳过目录内的包不得移动或改写"
        );
    }
    let remaining_names: Vec<Vec<u16>> = fs::read_dir(&f.root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().encode_wide().collect())
        .collect();
    assert!(
        remaining_names.contains(&file_units),
        "文件名原 UTF-16 单元不变"
    );
    assert!(
        remaining_names.contains(&dir_units),
        "目录名原 UTF-16 单元不变"
    );
    assert_eq!(fs::read_dir(&skipped_dir).unwrap().count(), 2);

    let db = Database::open_existing(&task.directory).unwrap();
    // 任务入口规范化根目录；ScanSink.notes 按实际枚举路径落库，
    // Windows 上 source 因而可能带 \\?\ 前缀，不能拿原始 fixture 根路径做字符串匹配。
    let scanned_root = fs::canonicalize(&f.root).unwrap();
    for path in [&skipped_file, &skipped_dir] {
        let source = scanned_root.join(path.file_name().unwrap());
        let reason: String = db
            .conn
            .query_row(
                "SELECT reason FROM events WHERE phase='扫描' AND result='跳过' AND source=?1",
                [source.display().to_string()],
                |row| row.get(0),
            )
            .expect("每个不可表示项都必须有消费可见的扫描跳过记录");
        assert!(
            reason.contains("不能无损表示"),
            "逐项说明不能处理的真实原因：{reason}"
        );
    }
    let log = events(&task.directory);
    assert!(log.contains("缺首卷"), "正常包失败原因仍明确：{log}");
}

// 覆盖 X-10/X-02（回归：只有 a.part02/a.part03 缺 part1 与 b.7z.002/b.7z.003
// 缺 .001 的目录按「残缺但可归组的卷集」各计一包；修复前两者都被静默漏掉，
// 确认框报 0 个包）
#[test]
fn count_archives_counts_missing_entry_volume_groups() {
    let f = Fixture::new();
    f.write("a.part02.rar", b"part2");
    f.write("a.part03.rar", b"part3");
    f.write("b.7z.002", b"2");
    f.write("b.7z.003", b"3");
    f.write("c.zip", b"zip");
    assert_eq!(
        jchtools_recursive_extract::engine::count_archives(&f.root, &base()).unwrap(),
        3,
        "缺首卷 part rar 一组 + 缺入口数字尾卷一组 + 普通 zip 一包"
    );
}

// 覆盖 X-10/X-06（回归：缺首卷组在解压执行时整组按失败包隔离进「解压失败」
// 并记录缺首卷原因；缺首卷判定先于引擎调用，无捆绑引擎也可执行）
#[test]
fn extract_quarantines_missing_first_part_rar_group() {
    let f = Fixture::new();
    f.write("a.part02.rar", b"part2");
    f.write("a.part03.rar", b"part3");
    let task = jchtools_recursive_extract::engine::extract_run_at(
        &f.root,
        base(),
        Context::default(),
        &f.state,
        None,
    )
    .unwrap();
    assert_eq!(task.summary.archives_ok, 0);
    assert_eq!(task.summary.archives_failed, 1, "缺首卷卷集只判失败一次");
    assert_eq!(
        fs::read(f.root.join("解压失败/a.part02.rar")).unwrap(),
        b"part2",
        "余卷必须原样整组隔离，不得留在原位"
    );
    assert_eq!(
        fs::read(f.root.join("解压失败/a.part03.rar")).unwrap(),
        b"part3",
        "整组隔离不丢卷、不改内容"
    );
    assert!(!f.root.join("a.part02.rar").exists(), "原位不得残留余卷");
    assert!(!f.root.join("a.part03.rar").exists(), "原位不得残留余卷");
    let log = events(&task.directory);
    assert!(
        log.contains("缺首卷"),
        "失败原因必须指明缺首卷（X-10）：{log}"
    );
}

// 覆盖 X-08：上次任务遗留的工作区不是本次所有物，不得按历史年龄自动删除。
#[test]
fn review_extraction_preserves_foreign_staging() {
    let f = Fixture::new();
    let owner = "11111111-1111-4111-8111-111111111111";
    let work = f.dir(&format!(".jchtools-work/{owner}"));
    fs::write(work.join("OWNER"), owner).unwrap();
    fs::create_dir(work.join("content")).unwrap();
    fs::write(work.join("content/keep.txt"), b"foreign task payload").unwrap();
    filetime::set_file_mtime(&work, filetime::FileTime::from_unix_time(0, 0)).unwrap();
    let task = jchtools_recursive_extract::engine::extract_run_at(
        &f.root,
        base(),
        Context::default(),
        &f.state,
        None,
    )
    .unwrap();
    assert_eq!(task.summary.archives_ok + task.summary.archives_failed, 0);
    assert_eq!(
        fs::read(work.join("content/keep.txt")).unwrap(),
        b"foreign task payload",
        "无本次所有权的旧暂存资料必须保留"
    );
    assert_eq!(fs::read_to_string(work.join("OWNER")).unwrap(), owner);
    // 空扫描不会启动解压；再用纯文件系统缺首卷路径触发真实 extract_queued。
    f.write("trigger.part02.rar", b"missing first volume");
    let active = jchtools_recursive_extract::engine::extract_run_at(
        &f.root,
        base(),
        Context::default(),
        &f.state,
        None,
    )
    .unwrap();
    assert_eq!(active.summary.archives_ok, 0);
    assert_eq!(active.summary.archives_failed, 1);
    assert_eq!(
        fs::read(work.join("content/keep.txt")).unwrap(),
        b"foreign task payload",
        "处理真实队列同样不得清理其他任务历史工作区"
    );
    assert_eq!(fs::read_to_string(work.join("OWNER")).unwrap(), owner);
}

// 覆盖 X-02/X-06/S-04：普通用户目录不能仅因名为 .jchtools-work 就从处理范围消失。
#[test]
fn ordinary_workspace_named_directory_remains_in_extraction_scope() {
    let f = Fixture::new();
    f.write(".jchtools-work/notes.txt", b"ordinary user data");
    f.write(
        ".jchtools-work/nested/missing.part02.rar",
        b"missing first volume",
    );
    let owner = "11111111-1111-4111-8111-111111111111";
    let workspace = format!(".jchtools-work/{owner}");
    f.write(&format!("{workspace}/OWNER"), owner.as_bytes());
    f.write(
        &format!("{workspace}/content/untouched.zip"),
        b"foreign task archive",
    );
    assert_eq!(
        jchtools_recursive_extract::engine::count_archives(&f.root, &base()).unwrap(),
        1,
        "没有 UUID/OWNER 所有权标记的普通目录照常参与清点"
    );
    let task = jchtools_recursive_extract::engine::extract_run_at(
        &f.root,
        base(),
        Context::default(),
        &f.state,
        None,
    )
    .unwrap();
    assert_eq!(task.summary.archives_failed, 1);
    assert_eq!(task.summary.archives_quarantined, 1);
    assert_eq!(
        fs::read(f.root.join(".jchtools-work/notes.txt")).unwrap(),
        b"ordinary user data"
    );
    assert_eq!(
        fs::read(f.root.join("解压失败/missing.part02.rar")).unwrap(),
        b"missing first volume"
    );
    assert_eq!(
        fs::read(f.root.join(workspace).join("content/untouched.zip")).unwrap(),
        b"foreign task archive",
        "真实所有权标记工作区仍须整树保护，不能交给引擎"
    );
}

// 覆盖 X-08/S-04：选定真实标记工作区或其中后代时拒绝；普通同名根不被误伤。
#[test]
fn extraction_rejects_selected_marked_workspace_and_descendants() {
    let f = Fixture::new();
    let owner = "11111111-1111-4111-8111-111111111111";
    for container in [".JCHTOOLS-WORK", ".jchtools-work (1)"] {
        let workspace = format!("{container}/{owner}");
        f.write(&format!("{workspace}/OWNER"), owner.as_bytes());
        let source = f.write(
            &format!("{workspace}/content/sub/untouched.zip"),
            b"foreign archive",
        );
        for suffix in ["", "/content", "/content/sub"] {
            let selected = f.root.join(format!("{workspace}{suffix}"));
            assert!(
                jchtools_recursive_extract::engine::count_archives(&selected, &base()).is_err(),
                "真实工作区或后代不得作为清点根：{selected:?}"
            );
            assert!(
                jchtools_recursive_extract::engine::extract_run_at(
                    &selected,
                    base(),
                    Context::default(),
                    &f.state,
                    None
                )
                .is_err(),
                "真实工作区或后代不得作为解压根：{selected:?}"
            );
        }
        assert_eq!(fs::read(source).unwrap(), b"foreign archive");
        assert_eq!(
            fs::read_to_string(f.root.join(&workspace).join("OWNER")).unwrap(),
            owner
        );
    }
    assert!(
        !f.state.join("tasks").exists(),
        "根边界拒绝必须先于创建任务库"
    );

    f.write(
        ".jchtools-work/ordinary/missing.part02.rar",
        b"missing first volume",
    );
    let ordinary = f.root.join(".jchtools-work/ordinary");
    assert_eq!(
        jchtools_recursive_extract::engine::count_archives(&ordinary, &base()).unwrap(),
        1
    );
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &ordinary,
        base(),
        Context::default(),
        &f.state,
        None,
    )
    .unwrap();
    assert_eq!(result.summary.archives_failed, 1);
    assert_eq!(
        fs::read(ordinary.join("解压失败/missing.part02.rar")).unwrap(),
        b"missing first volume"
    );
}

// 覆盖 X-04/X-05/X-08/H-07/S-01/P-08：本次暂存容器不能吞掉已落盘的同名普通空目录。
// 使用真实 ZIP 与公开解压入口；源包成功清理后仍须保留全部成员，不模拟外部目录变动。
#[test]
fn review_fix_empty_workspace_member_survives_source_cleanup() {
    let f = Fixture::new();
    let old_payload = b"existing user bytes";
    let new_payload = b"archive member bytes";
    f.write("payload.txt", old_payload);
    assert!(
        !f.root.join(".jchtools-work").exists(),
        "前置：没有既有临时容器"
    );

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.add_directory(".jchtools-work/", options).unwrap();
    zip.start_file("payload.txt", options).unwrap();
    std::io::Write::write_all(&mut zip, new_payload).unwrap();
    let archive_bytes = zip.finish().unwrap().into_inner();
    let source = f.write("healthy.zip", &archive_bytes);
    assert_eq!(fs::read(&source).unwrap(), archive_bytes);
    // E-02/E-05：显式真实引擎沿用引擎验收夹具；未提供时走产品的随包/内嵌解析，
    // 缺引擎必须失败，不能跳过此回归。
    let engine = std::env::var_os("JCHTOOLS_TEST_7ZIP").map(PathBuf::from);
    let result = jchtools_recursive_extract::engine::extract_run_at(
        &f.root,
        Config {
            reserve_bytes: 0,
            max_ratio: 0,
            include_hidden: true,
            ..base()
        },
        Context::default(),
        &f.state,
        engine.as_deref(),
    )
    .unwrap();

    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_quarantined, 0);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(result.summary.deleted, 1);
    assert_eq!(result.summary.extracted, 1);
    assert!(!source.exists(), "全部成员落盘后成功源包应永久清理");
    assert_eq!(fs::read(f.root.join("payload.txt")).unwrap(), old_payload);
    assert_eq!(
        fs::read(f.root.join("payload (1).txt")).unwrap(),
        new_payload
    );
    // 本次自建暂存区不是用户资料：允许沿用原名，或按 X-04 避开暂存占位改名；
    // 两种落点都必须是唯一、真正为空的正式目录，不能把随机工作区残留当成输出。
    let ordinary_directories: Vec<PathBuf> = fs::read_dir(&f.root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(
        ordinary_directories.len(),
        1,
        "成功源包删除后，同名普通空目录不得被暂存 Drop 删除"
    );
    let ordinary_directory = &ordinary_directories[0];
    assert!(
        [".jchtools-work", ".jchtools-work (1)"]
            .iter()
            .any(|name| ordinary_directory == &f.root.join(name)),
        "正式目录应保留原名或按 X-04 采用最小冲突序号：{ordinary_directory:?}"
    );
    assert_eq!(
        fs::read_dir(ordinary_directory).unwrap().count(),
        0,
        "只留下普通空目录，不留下所有权标记或随机工作区"
    );
    assert_eq!(
        fs::read_dir(&f.root).unwrap().count(),
        3,
        "正式成员全部保留，且无暂存或隔离残留"
    );
}
