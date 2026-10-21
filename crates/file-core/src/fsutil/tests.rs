use super::moveops::copy_noreplace;
#[cfg(windows)]
use super::moveops::offset_to_filetime;
use super::rootops::resolve_protected_root;
use super::*;
use std::fs;
use std::time::UNIX_EPOCH;

use super::*;

// 覆盖 X-08 / S-04：名字或单独 OWNER 不是完整工作区；真实标记不授予删除权限。
#[test]
fn workspace_detection_requires_matching_owner_and_plain_content_directory() {
    let root = tempfile::tempdir().unwrap();
    let wrapper = root.path().join(".JCHTOOLS-WORK");
    let owner = "11111111-1111-4111-8111-111111111111";
    let workspace = wrapper.join(owner);
    fs::create_dir_all(&workspace).unwrap();
    assert!(!is_extraction_workspace(&wrapper).unwrap());
    assert!(!is_extraction_workspace(&workspace).unwrap());
    fs::write(workspace.join("OWNER"), owner).unwrap();
    assert!(
        !is_extraction_workspace(&workspace).unwrap(),
        "缺少 content 的用户目录仍需纳入"
    );
    fs::write(workspace.join("content"), b"user file").unwrap();
    assert!(
        !is_extraction_workspace(&workspace).unwrap(),
        "content 普通文件不是工作区"
    );
    fs::remove_file(workspace.join("content")).unwrap();
    fs::create_dir(workspace.join("content")).unwrap();
    assert!(is_extraction_workspace(&workspace).unwrap());
    fs::write(
        workspace.join("OWNER"),
        "22222222-2222-4222-8222-222222222222",
    )
    .unwrap();
    assert!(
        !is_extraction_workspace(&workspace).unwrap(),
        "标记必须与目录 UUID 一致"
    );
    assert!(workspace.join("OWNER").is_file(), "识别不授予删除授权");
}

// 覆盖 X-08 / S-04：规范冲突容器和原容器共用标记协议；
// 直接选择其根或后代必须拒绝，非规范同名用户目录不被扩大排除。
#[test]
fn workspace_namespace_and_selected_root_boundaries_are_exact() {
    let root = tempfile::tempdir().unwrap();
    let owner = "11111111-1111-4111-8111-111111111111";
    for (index, (container, recognized)) in [
        (".jchtools-work", true),
        (".JCHTOOLS-WORK (2)", true),
        (".jchtools-work (0)", false),
        (".jchtools-work (02)", false),
        (".jchtools-work (2) extra", false),
    ]
    .into_iter()
    .enumerate()
    {
        let workspace = root
            .path()
            .join(format!("case-{index}"))
            .join(container)
            .join(owner);
        let content = workspace.join("content");
        let nested = content.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(workspace.join("OWNER"), owner).unwrap();
        assert_eq!(
            is_extraction_workspace(&workspace).unwrap(),
            recognized,
            "{container}"
        );
        for selected in [&workspace, &content, &nested] {
            let result = normalize_root_with(selected, None);
            if recognized {
                let error = result.expect_err("真实工作区及后代不能直接处理");
                assert!(format!("{error:#}").contains("解压工作区"), "{error:#}");
            } else {
                assert!(
                    result.is_ok(),
                    "普通非规范容器不得仅按名字拒绝：{container}"
                );
            }
        }
        assert_eq!(fs::read(workspace.join("OWNER")).unwrap(), owner.as_bytes());
    }
}

// 覆盖 H-07 / X-04：直接调用真实候选生成函数，不依赖 GUI 集成测试链接。
#[test]
fn suffix_keeps_legal_unicode_extension() {
    for whitespace in ['\u{00a0}', '\u{2003}', '\u{3000}'] {
        let extension = format!(".txt{whitespace}");
        validate_component(&format!("资料{extension}")).unwrap();
        assert_eq!(
            suffixed_candidate("资料", &extension, 1),
            format!("资料 (1){extension}"),
            "合法扩展名中的 Unicode 空白不得被修剪"
        );
    }
}

#[cfg(windows)]
#[test]
fn fold_rel_matches_windows_ordinal_case_and_order() {
    assert_ne!(fold_rel("\u{0130}"), fold_rel("i\u{0307}"));
    assert_eq!(fold_rel("\u{1f80}"), fold_rel("\u{1f88}"));
    assert!(
        fold_rel("\u{10000}") < fold_rel("\u{e000}"),
        "折叠键顺序必须保留 Windows UTF-16 序数顺序"
    );
}

// 覆盖 X-10, 附录 B（回归：part rar 的 `.partN.rar` 是不可拆分后缀，
// 冲突改名的序号插在整个后缀之前，如 `资料 (1).part01.rar`）
#[test]
fn split_keeps_part_rar_suffix_whole() {
    assert_eq!(
        split_compound_name("资料.part01.rar"),
        ("资料", ".part01.rar")
    );
    assert_eq!(split_compound_name("a.part1.rar"), ("a", ".part1.rar"));
    // 仅含 “.part” 子串的普通包不按 part rar 后缀处理。
    assert_eq!(
        split_compound_name("report.partial.rar"),
        ("report.partial", ".rar")
    );
    // part 段必须带数字；`.partN.zip` 不属于 part rar 族。
    assert_eq!(split_compound_name("x.part.rar"), ("x.part", ".rar"));
    assert_eq!(split_compound_name("x.part01.zip"), ("x.part01", ".zip"));
    // X-10 的 N 是正整数；全零不是 part rar 卷集后缀。
    assert_eq!(split_compound_name("x.part0.rar"), ("x.part0", ".rar"));
    assert_eq!(split_compound_name("x.part000.rar"), ("x.part000", ".rar"));
}

// 覆盖 X-10, 附录 B（数字尾卷只并入前一白名单扩展名及一层 tar 压缩流；
// 主体中碰巧同名的白名单段不得继续并入）
#[test]
fn split_merges_whitelisted_components_behind_numbered_tail() {
    assert_eq!(split_compound_name("x.tar.gz.001"), ("x", ".tar.gz.001"));
    assert_eq!(split_compound_name("包.7z.001"), ("包", ".7z.001"));
    assert_eq!(split_compound_name("y.tbz2.001"), ("y", ".tbz2.001"));
    assert_eq!(split_compound_name("foo.bar.001"), ("foo.bar", ".001"));
    assert_eq!(split_compound_name("x.tar.foo.001"), ("x.tar.foo", ".001"));
    assert_eq!(split_compound_name("a.zip.7z.001"), ("a.zip", ".7z.001"));
    assert_eq!(
        split_compound_name("a.tar.tar.gz.001"),
        ("a.tar", ".tar.gz.001")
    );
}

// 覆盖 H-07（复合压缩流扩展名整体保留；既有行为不回归）
#[test]
fn split_keeps_compound_stream_suffix() {
    assert_eq!(split_compound_name("资料.tar.gz"), ("资料", ".tar.gz"));
    assert_eq!(
        split_compound_name("report.tar.pdf"),
        ("report.tar", ".pdf")
    );
    assert_eq!(
        split_compound_name("report.tar.zip"),
        ("report.tar", ".zip")
    );
    assert_eq!(split_compound_name("report.tar"), ("report", ".tar"));
    assert_eq!(split_compound_name("a.txt"), ("a", ".txt"));
    assert_eq!(split_compound_name("无扩展名"), ("无扩展名", ""));
    assert_eq!(split_compound_name("combo.z01"), ("combo", ".z01"));
}

// 覆盖 X-06, X-10（回归：冲突改名把序号插在整组卷后缀之前）
#[test]
fn unique_target_inserts_index_before_volume_suffix() {
    let temp = tempfile::tempdir().unwrap();
    let occupied = temp.path().join("资料.part01.rar");
    fs::write(&occupied, b"existing").unwrap();
    let target = unique_target(temp.path(), &occupied).unwrap();
    assert_eq!(
        target.file_name().and_then(|name| name.to_str()).unwrap(),
        "资料 (1).part01.rar"
    );
}

#[test]
fn numbered_volume_growth_split_preserves_complete_canonical_suffix() {
    for (name, expected) in [
        ("包.7z.999", ("包", ".7z.999")),
        ("包.7z.1000", ("包", ".7z.1000")),
        ("包.ZIP.1001", ("包", ".ZIP.1001")),
        ("x.tar.gz.1000", ("x", ".tar.gz.1000")),
        ("a.tar.tar.gz.1001", ("a.tar", ".tar.gz.1001")),
        ("a.zip.7z.1000", ("a.zip", ".7z.1000")),
        (
            "x.tbz2.18446744073709551616",
            ("x", ".tbz2.18446744073709551616"),
        ),
    ] {
        assert_eq!(split_compound_name(name), expected, "{name}");
    }
    for (name, expected) in [
        ("包.7z.0001", ("包.7z", ".0001")),
        ("包.7z.01000", ("包.7z", ".01000")),
        ("包.7z.1", ("包.7z", ".1")),
        ("包.7z.01", ("包.7z", ".01")),
        ("包.7z.１０００", ("包.7z", ".１０００")),
        ("包.7z.١٠٠٠", ("包.7z", ".١٠٠٠")),
        ("包.7z.1000.bak", ("包.7z.1000", ".bak")),
        ("foo.bar.1000", ("foo.bar", ".1000")),
        ("x.iso.1000", ("x.iso", ".1000")),
    ] {
        assert_eq!(split_compound_name(name), expected, "{name}");
    }
}

#[test]
fn numbered_volume_growth_unique_target_preserves_numbered_suffix() {
    let temp = tempfile::tempdir().unwrap();
    for (name, first, second) in [
        ("资料.7z.1000", "资料 (1).7z.1000", "资料 (2).7z.1000"),
        (
            "资料.tar.gz.1001",
            "资料 (1).tar.gz.1001",
            "资料 (2).tar.gz.1001",
        ),
        ("资料.7z.01000", "资料.7z (1).01000", "资料.7z (2).01000"),
    ] {
        let requested = temp.path().join(name);
        fs::write(&requested, b"existing").unwrap();
        let target = unique_target(temp.path(), &requested).unwrap();
        assert_eq!(
            target.file_name().and_then(|name| name.to_str()).unwrap(),
            first
        );
        fs::write(&target, b"collision").unwrap();
        let target = unique_target(temp.path(), &requested).unwrap();
        assert_eq!(
            target.file_name().and_then(|name| name.to_str()).unwrap(),
            second
        );
    }
}

/// F28 夹具：写一个临时文件并返回路径。
fn f28_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("t.txt");
    fs::write(&file, b"x").unwrap();
    (temp, file)
}

/// 1970 纪元到 1601 纪元的换算常数（100ns 单位），即 1601-01-01 相对
/// UNIX_EPOCH 的纳秒偏移 ÷ 100。
const F28_DELTA_NS: u64 = 11_644_473_600_000_000_000;

// 平台门禁原因：验证对象是 Windows FILETIME 语义与 SetFileTime 写回，
// 非 Windows 分支不写创建时间，断言无意义。
// 覆盖 S-01, C-21（回归：1969-12-31 的负偏移必须忠实写回；
// 修复前 to_filetime 对 duration_since 的 Err 分支一律返回 1970-01-01）
#[cfg(windows)]
#[test]
fn f28_pre_epoch_created_time_is_written_faithfully() {
    let (_temp, file) = f28_file();
    let target = UNIX_EPOCH
        .checked_sub(std::time::Duration::from_hours(24))
        .unwrap();
    set_created_time(&file, target).unwrap();
    let back = fs::metadata(&file).unwrap().created().unwrap();
    assert_eq!(
        UNIX_EPOCH.duration_since(back).unwrap(),
        std::time::Duration::from_hours(24),
        "1969-12-31 的创建时间必须原样写回，不得被抹成 1970-01-01"
    );
}

// 平台门禁原因：同上，FILETIME 1601 下界是 Windows 语义。
// 覆盖 S-01, C-21（回归：恰好 1601-01-01 是 FILETIME 合法下界，必须接受）
#[cfg(windows)]
#[test]
fn f28_accepts_exact_1601_boundary() {
    let (_temp, file) = f28_file();
    let boundary = UNIX_EPOCH
        .checked_sub(std::time::Duration::from_nanos(F28_DELTA_NS))
        .unwrap();
    assert!(
        set_created_time(&file, boundary).is_ok(),
        "恰好 1601-01-01 00:00:00 UTC 可表示，必须成功写回"
    );
}

// 平台门禁原因：同上。
// 覆盖 S-01, C-21（回归：早于 1601-01-01 一个 100ns 单位必须显式报错；
// 修复前负偏移一律被静默钳制成 1970。SystemTime 在 Windows 上无法表示早于
// 1601 的时刻（checked_sub 返回 None），越界值经纯函数核心直接构造）
#[cfg(windows)]
#[test]
fn f28_rejects_one_tick_before_1601() {
    assert!(
        offset_to_filetime(Err(std::time::Duration::from_nanos(F28_DELTA_NS + 100))).is_err(),
        "早于 1601-01-01 一个 100ns 单位超出 FILETIME 范围，必须报错而非钳制"
    );
}

// 平台门禁原因：同上。
// 覆盖 S-01, C-21（纪元本身 1970-01-01 必须原样写回）
#[cfg(windows)]
#[test]
fn f28_epoch_itself_is_written() {
    let (_temp, file) = f28_file();
    set_created_time(&file, UNIX_EPOCH).unwrap();
    let back = fs::metadata(&file).unwrap().created().unwrap();
    assert_eq!(
        back.duration_since(UNIX_EPOCH).unwrap(),
        std::time::Duration::ZERO
    );
}

// 平台门禁原因：同上。
// 覆盖 S-01, C-21（现代值正偏移路径不回归）
#[cfg(windows)]
#[test]
fn f28_modern_time_is_written() {
    let (_temp, file) = f28_file();
    let modern = UNIX_EPOCH + std::time::Duration::from_secs(1_768_000_000);
    set_created_time(&file, modern).unwrap();
    let back = fs::metadata(&file).unwrap().created().unwrap();
    assert_eq!(
        back.duration_since(UNIX_EPOCH).unwrap().as_secs(),
        1_768_000_000
    );
}

// 平台门禁原因：同上。
// 覆盖 S-01, C-21（越界值：远早于 1601 必须报错而非钳制）
#[cfg(windows)]
#[test]
fn f28_rejects_far_pre_1601_value() {
    assert!(
        offset_to_filetime(Err(std::time::Duration::from_hours(400 * 366 * 24))).is_err(),
        "远早于 1601 的越界值必须报错而非钳制"
    );
}

// 覆盖 S-01（回归：跨卷移动的复制核心对已存在目标必须失败且不截断既有
// 字节；修复前 fs::copy 以 create+truncate 打开目标，D→C 实测静默覆盖旧
// 内容后删除源文件）
#[test]
fn copy_noreplace_refuses_existing_target_without_truncation() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("src.txt");
    let target = temp.path().join("dst.txt");
    fs::write(&source, b"new content from source").unwrap();
    fs::write(&target, b"existing target bytes").unwrap();
    let error = copy_noreplace(&source, &target).expect_err("目标已存在必须失败");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::AlreadyExists,
        "失败原因必须是目标已存在"
    );
    assert_eq!(
        fs::read(&target).unwrap(),
        b"existing target bytes",
        "既有目标字节不得被截断或改写"
    );
    assert_eq!(
        fs::read(&source).unwrap(),
        b"new content from source",
        "源文件保持原状"
    );
    // 正常路径：目标不存在时完整复制。
    let fresh = temp.path().join("fresh.txt");
    copy_noreplace(&source, &fresh).unwrap();
    assert_eq!(fs::read(&fresh).unwrap(), b"new content from source");
}

// 覆盖 S-05（SystemRoot 缺失或指向不可访问路径时必须报错拒绝开始，
// 不得静默失去系统目录保护——修复前该分支被静默跳过）
#[test]
fn resolve_protected_root_errors_when_unavailable() {
    assert!(
        resolve_protected_root(None).is_err(),
        "受保护目录信息缺失必须报错，不得静默放行"
    );
    assert!(
        resolve_protected_root(Some(std::ffi::OsStr::new(r"Z:\不存在的受保护目录"))).is_err(),
        "受保护目录无法规范化必须报错，不得静默放行"
    );
}

// 覆盖 S-05（受保护目录参数化：根在其内或即其本身拒绝；根是严格上层时不拒绝、
// 返回需整树剪枝的子树；无关根正常放行）
#[test]
fn normalize_root_with_synthetic_protected_dir() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("data");
    fs::create_dir_all(root.join("win/System32")).unwrap();
    // 受保护目录 = root/win（位于根内，用于「严格上层」分支）。
    let win = fs::canonicalize(root.join("win")).unwrap();
    let inside = win.join("System32");
    fs::create_dir_all(&inside).unwrap();

    assert!(
        normalize_root_with(&inside, Some(&win)).is_err(),
        "根位于受保护目录内必须拒绝整次任务"
    );
    assert!(
        normalize_root_with(&win, Some(&win)).is_err(),
        "根即受保护目录必须拒绝整次任务"
    );
    let (got, prune) = normalize_root_with(&root, Some(&win)).unwrap();
    assert_eq!(got, fs::canonicalize(&root).unwrap());
    assert_eq!(
        prune.as_deref(),
        Some(win.as_path()),
        "根是严格上层时不拒绝，返回需整树剪枝的子树"
    );
    // 与根无关的受保护目录：不产生剪枝。
    let outside = temp.path().join("elsewhere");
    fs::create_dir_all(&outside).unwrap();
    let outside = fs::canonicalize(&outside).unwrap();
    let (_, prune) = normalize_root_with(&root, Some(&outside)).unwrap();
    assert!(prune.is_none(), "受保护目录不在根内时不产生剪枝");
}
