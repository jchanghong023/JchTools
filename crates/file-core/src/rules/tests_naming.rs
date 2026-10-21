use super::*;
use crate::config::{Config, KeepPolicy};
use crate::fsutil;
use crate::model::{FileRecord, Snapshot};
use std::cmp::Ordering;
use unicode_normalization::UnicodeNormalization;

#[test]
fn review_rule_unicode_globs() {
    assert!(build_exclusions("a/?.txt").unwrap().is_match("a/中.txt"));
    assert!(build_exclusions("a/?.txt").unwrap().is_match("a/😀.txt"));
    assert!(!build_exclusions("a/?.txt").unwrap().is_match("a/中文.txt"));
}

#[test]
fn review_rule_ordinal_globs() {
    assert!(build_exclusions("Ä.txt").unwrap().is_match("ä.txt"));
}

#[test]
fn review_rule_ordinal_copy_keys() {
    assert_eq!(normal_key("Σ (1).txt"), normal_key("ς.txt"));
    assert_ne!(normal_key("İ.txt"), normal_key("i\u{307}.txt"));
}
fn record(id: i64, name: &str, normalized: &str) -> FileRecord {
    FileRecord {
        id,
        rel: format!("dir/{name}"),
        name: name.into(),
        normalized: normalized.into(),
        snapshot: Snapshot {
            size: 1,
            modified_ns: 10,
            created_ns: None,
            identity: id.to_string(),
            links: 1,
        },
        hash: Some("h".into()),
        cleanable: false,
    }
}
/// 按 C-02 三类名称关系逐分支对照参考实现：
/// `(name=?2 AND ?4) OR (name<>?2 AND normal=?3 AND ?5) OR (name<>?2 AND normal<>?3 AND ?6)`
fn sql_match(a: &FileRecord, b: &FileRecord, cfg: &Config) -> bool {
    match (a.name == b.name, a.normalized == b.normalized) {
        (true, _) => cfg.dedup_same_name,
        (false, true) => cfg.dedup_copy_names,
        (false, false) => cfg.dedup_other_names,
    }
}

// 覆盖 C-04（同一物理文件判定：标识必须可靠，且链接数证明存在第二个目录项）
#[test]
fn identity_proves_same_file_requires_reliable_identity() {
    let pair = |identity: &str, links: u64| {
        let mut a = record(1, "a.txt", "a.txt");
        a.snapshot.identity = identity.into();
        a.snapshot.links = links;
        let mut b = record(2, "b.txt", "b.txt");
        b.snapshot.identity = identity.into();
        b.snapshot.links = links;
        (a, b)
    };
    // 真实硬链接：两侧标识相同且编号可靠、链接数 >= 2 → 证明同一物理文件。
    let reliable = if cfg!(windows) { "12:34:56" } else { "2049:99" };
    let (a, b) = pair(reliable, 2);
    assert!(
        identity_proves_same_file(&a, &b),
        "可靠标识 + 两个目录项必须判定为同一物理文件"
    );
    let (a, b) = pair(reliable, 3);
    assert!(identity_proves_same_file(&a, &b));
    // 标识相同但链接数只有 1：两个目录项不可能指向同一物理文件，不得据此跳过去重。
    let (a, b) = pair(reliable, 1);
    assert!(
        !identity_proves_same_file(&a, &b),
        "链接数不足以证明同一物理文件"
    );
    // 标识不同：不是同一物理文件。
    let (mut a, b) = pair(reliable, 2);
    a.snapshot.identity = "99:88:77".into();
    assert!(!identity_proves_same_file(&a, &b));
    // Windows 上部分文件系统索引恒为 0（hash_cache::identity_is_degenerate 同口径）：
    // 该标识无法区分不同文件，退化时不得作为跳过依据（C-04）。
    #[cfg(windows)]
    {
        let (a, b) = pair("12:0:0", 2);
        assert!(
            !identity_proves_same_file(&a, &b),
            "退化标识不得证明同一物理文件"
        );
    }
}

// 覆盖 C-08 / 附录 B：只有首位点的文件没有扩展名，不授权临时文件删除。
#[test]
fn temp_cleanup_preserves_extensionless_dotfiles() {
    let cfg = Config {
        clean_temp: true,
        ..Config::default()
    };
    for name in [".tmp", ".TEMP", "a/.bak"] {
        assert!(cleanup_hits(name, 1, &cfg).is_empty(), "{name}");
    }
    for name in ["file.tmp", "a/.profile.temp", "backup.bak"] {
        assert_eq!(
            cleanup_hits(name, 1, &cfg).first().map(|(kind, _)| *kind),
            Some(CleanupKind::Temp),
            "{name}"
        );
    }
}

// 覆盖 C-08（__MACOSX 的清理授权对象是目录名：名为 __MACOSX 的普通文件自身不命中）
#[test]
fn macosx_plain_file_is_not_junk() {
    let cfg = Config {
        clean_junk: true,
        ..Config::default()
    };
    assert!(
        cleanup_reason("__MACOSX", 5, &cfg).is_none(),
        "根直属名为 __MACOSX 的普通文件不命中垃圾清理"
    );
    assert!(
        cleanup_reason("a/__macosx", 5, &cfg).is_none(),
        "子目录内名为 __MACOSX 的普通文件不命中垃圾清理"
    );
    assert_eq!(
        cleanup_reason("a/__MACOSX/x.pdf", 5, &cfg).map(|(kind, _)| kind),
        Some(CleanupKind::Junk),
        "目录内的普通文件命中（大小写不敏感）"
    );
}

// 覆盖附录 B（回归：副本标记识别不得跨「小写化串/原串」复用字节偏移——
// U+0130 `İ` 的小写化改变字节长度，修复前 `İİİİİcopy` 直接越界 panic）
#[test]
fn copy_marker_scan_survives_width_changing_lowercase() {
    // 修复前在 strip_one_marker 的切片处 panic；不 panic 即通过。
    assert!(normal_key("İİİİİcopy.pdf").contains(&fsutil::fold_rel("copy")));
    // `- Copy` 标记仍被剥除（ASCII 忽略大小写只作用于字母，前缀主体保持原样）。
    assert_eq!(copy_key("İİİİİ-Copy.pdf"), "İİİİİ.pdf");
    assert_eq!(copy_key("plan - COPY.pdf"), "plan.pdf");
    assert_eq!(copy_key("资料 (2).pdf"), "资料.pdf");
    assert_eq!(clean_copy_output("资料 (2).pdf"), "资料_2.pdf");
}
// 覆盖附录 B（派生名对齐 Windows 保留设备名；修复 CLOCK$ 与 COM/LPT 上标数字遗漏）
#[test]
fn legalize_derived_windows_reserved_device_names() {
    for (input, expected) in [
        ("CON", "_CON"),
        ("CLOCK$", "_CLOCK$"),
        ("com1", "_com1"),
        ("lpt9", "_lpt9"),
        ("COM¹", "_COM¹"),
        ("LPT³", "_LPT³"),
    ] {
        assert_eq!(legalize_derived(input), expected, "{input}");
    }
}

// 覆盖 C-08（三类清理的识别与类别归属：planner 按类别解析删除方式，不比较中文原因串）
#[test]
fn cleanup_reason_reports_category() {
    let cfg = Config {
        clean_temp: true,
        clean_zero: true,
        ..Config::default()
    };
    assert_eq!(
        cleanup_reason("a/Thumbs.db", 1, &cfg).map(|(kind, _)| kind),
        Some(CleanupKind::Junk)
    );
    assert_eq!(
        cleanup_reason("a/__MACOSX/x", 1, &cfg).map(|(kind, _)| kind),
        Some(CleanupKind::Junk)
    );
    assert_eq!(
        cleanup_reason("a/x.tmp", 1, &cfg).map(|(kind, _)| kind),
        Some(CleanupKind::Temp)
    );
    assert_eq!(
        cleanup_reason("a/~$x.docx", 1, &cfg).map(|(kind, _)| kind),
        Some(CleanupKind::Temp)
    );
    assert_eq!(
        cleanup_reason("a/zero.dat", 0, &cfg).map(|(kind, _)| kind),
        Some(CleanupKind::Zero)
    );
    assert_eq!(
        cleanup_reason("a/keep.txt", 5, &cfg),
        None,
        "普通文件不清理"
    );
}

// 覆盖 C-08（扩展名修正的保守口径：别名与容器识别都不得触发改名）
#[test]
fn extension_fix_is_conservative() {
    for (old, detected) in [
        ("jpg", "jpg"),
        ("jpeg", "jpg"),
        ("jpe", "jpg"),
        ("jfif", "jpg"),
        ("tif", "tif"),
        ("tiff", "tif"),
        ("htm", "html"),
        ("html", "html"),
        ("mid", "midi"),
        ("midi", "midi"),
    ] {
        assert!(
            !extension_needs_fix(old, detected),
            "{old} -> {detected} 是同义扩展名，不得强制统一"
        );
    }
    // 复合/包格式：更粗的识别结果不得把专用扩展名改粗（C-08 例：Office ZIP 容器）。
    // 有效 ZIP/MIME 能区分真实宏/模板类型；只有外层 ZIP 的结果仍不能改粗。
    for old in [
        "docm", "dotx", "dotm", "xlsm", "xltx", "xltm", "xlsb", "potx", "potm", "ppsx", "ppsm",
    ] {
        for detected in ["zip", "docx", "xlsx", "pptx"] {
            assert_eq!(
                extension_needs_fix(old, detected),
                detected != "zip",
                "{old} 只在内型已确认且后缀确实错误时修正为 {detected}"
            );
        }
    }
    // ODF / EPUB / X-09 其他已列容器后缀：识别结果只有容器粒度时保留原后缀，不降级。
    for old in [
        "odt",
        "ods",
        "odp",
        "epub",
        "cab",
        "msi",
        "msix",
        "appx",
        "msixbundle",
        "appxbundle",
        "iso",
        "img",
        "wim",
        "esd",
        "vhd",
        "vhdx",
        "jar",
        "war",
        "ear",
        "apk",
        "aab",
        "whl",
        "nupkg",
        "vsix",
        "crx",
        "xpi",
        "ipa",
        "pkg",
        "dmg",
        "lz4",
        "br",
        "lz",
        "exe",
        "com",
    ] {
        for detected in ["zip", "docx", "xlsx", "pptx"] {
            assert_eq!(
                extension_needs_fix(old, detected),
                detected != "zip",
                "{old} 外层 ZIP 不降级，明确文档内型才可修正为 {detected}"
            );
        }
    }
    // Office 基础类型：内层类型未确认时保留原后缀；内容确认为另一文档类型时才修正。
    for old in ["docx", "xlsx", "pptx"] {
        assert!(
            !extension_needs_fix(old, "zip"),
            "{old} 内层类型未确认时不得降级为 .zip"
        );
    }
    assert!(
        extension_needs_fix("docx", "xlsx"),
        "内容确认为 xlsx 时 .docx 是错误扩展名，应修正"
    );
    // 旧式 Office 后缀同上：内容已是 ZIP 容器时不降级为 .zip。
    for old in ["doc", "xls", "ppt"] {
        assert!(
            !extension_needs_fix(old, "zip"),
            "{old} 是 Office 后缀，不得仅因识别到 ZIP 容器降级为 .zip"
        );
    }
    // 附录 A 未列出的后缀（含 ZIP 家族容器文档）：识别到的只是外层容器，
    // 无法确定真实类型，按 C-08「签名不足以确定实际类型时不改名」保留。
    for (old, detected) in [
        ("odg", "zip"),
        ("odf", "zip"),
        ("xps", "zip"),
        ("vsdx", "zip"),
        ("sldx", "zip"),
        ("thmx", "zip"),
        ("cbz", "zip"),
        ("kmz", "zip"),
        ("sxw", "zip"),
    ] {
        assert!(
            !extension_needs_fix(old, detected),
            "{old} 无法由签名证明后缀错误，不得按 {detected} 改名"
        );
    }
    // 包装格式（压缩流内层未知，如 svgz/tgz）识别到容器后缀时同样不是错误扩展名。
    for (old, detected) in [("svgz", "gz"), ("tgz", "gz"), ("tbz2", "bz2")] {
        assert!(
            !extension_needs_fix(old, detected),
            "{old} 不得按外层容器 {detected} 改名"
        );
    }
    // 附录 B 承诺可确定识别并修正的类型：旧后缀确是别的已知格式时必须改名。
    for (old, detected) in [
        ("txt", "zip"),
        ("mp4", "zip"),
        ("jpg", "7z"),
        ("mp3", "rar"),
        ("zip", "rar"),
        ("7z", "zip"),
        ("tar", "rar"),
        ("bin", "zip"),
        ("dat", "7z"),
        ("xyz", "rar"),
        ("ott", "odt"),
        ("ots", "ods"),
        ("otp", "odp"),
        ("odm", "odt"),
        ("", "png"),
        ("bin", "png"),
    ] {
        assert!(
            extension_needs_fix(old, detected),
            "{old} -> {detected} 是承诺可修正的错误扩展名，应改名"
        );
    }
    // 未承诺的类型不进行修正（附录 B 末句）：流格式、OLE 存储等识别结果不是改名依据。
    for detected in [
        "gz", "bz2", "xz", "zst", "tar", "cab", "iso", "wim", "lz", "lzma", "cpio", "lzh", "msi",
        "mp3", "mp4",
    ] {
        assert!(
            !extension_needs_fix("txt", detected),
            "未承诺的类型 {detected} 不得直接作为改名依据"
        );
    }
    // X-10 卷尾与复合后缀属于不可拆分后缀（附录 B）：识别到承诺容器也不得改名。
    for (old, detected) in [
        ("zip.001", "zip"),
        ("7z.001", "7z"),
        ("tar.gz", "gz"),
        ("r00", "rar"),
        ("z01", "zip"),
        ("part01", "rar"),
        ("001", "zip"),
    ] {
        assert!(
            !extension_needs_fix(old, detected),
            "卷尾/复合后缀 {old} 不得按 {detected} 改名"
        );
    }
    // 明确的错误扩展名仍必须修正（默认关闭，开启后列入计划）。
    for (old, detected) in [
        ("txt", "png"),
        ("mp4", "pdf"),
        ("jpg", "png"),
        ("mp4", "flac"),
        ("mkv", "wav"),
    ] {
        assert!(
            extension_needs_fix(old, detected),
            "{old} -> {detected} 是错误扩展名，应修正"
        );
    }
}

// 覆盖 C-08 / 附录 B（副本标记输出转换：`(N)` 转 `_N`，其余标记移除；复合扩展名与
// 编号分卷整体保留；`_1` 不是标记，转换幂等）
#[test]
fn copy_marker_conversion_matches_appendix_b() {
    for (input, expected) in [
        ("报告 (1) (02).pdf", "报告_1_2.pdf"),
        ("report - Copy (1).tar.gz", "report_1.tar.gz"),
        ("报告副本.pdf", "报告.pdf"),
        ("报告副本说明.pdf", "报告副本说明.pdf"),
        ("(1).pdf", "__1.pdf"),
        ("report(1).pdf", "report_1.pdf"),
        ("report (01).pdf", "report_1.pdf"),
        ("report (0).pdf", "report_0.pdf"),
        ("report - copy.pdf", "report.pdf"),
        ("report  副本.pdf", "report.pdf"),
        ("资料 (1).tar.gz", "资料_1.tar.gz"),
        ("包 (1).7z.001", "包_1.7z.001"),
        ("报告_1.pdf", "报告_1.pdf"),
        ("report.final (1).txt", "report.final_1.txt"),
    ] {
        assert_eq!(clean_copy_output(input), expected, "{input}");
    }
}

// 覆盖 C-02（副本名键：剥除全部标记的主体 + 原扩展名）
#[test]
fn copy_key_strips_all_markers() {
    for (input, expected) in [
        ("报告 (1) (02).pdf", "报告.pdf"),
        ("report - Copy (1).tar.gz", "report.tar.gz"),
        ("报告副本.pdf", "报告.pdf"),
        ("资料.tar.gz", "资料.tar.gz"),
        ("包.7z.001", "包.7z.001"),
    ] {
        assert_eq!(copy_key(input), expected, "{input}");
    }
}

// 覆盖 C-08（NFC 与连续空白规范化：单一开关、不改扩展名、压成 ASCII 空格）
#[test]
fn normalize_stem_collapses_whitespace_and_nfc() {
    let normalized = normalize_stem("  a　b  c　");
    assert_eq!(normalized, "a b c");
    // NFD 组合字符归并为 NFC 预组合形式。
    let nfd = "e\u{301}";
    assert_eq!(normalize_stem(nfd), "e\u{301}".nfc().to_string());
}

// 覆盖 C-02, R-02（三类名称关系独立启停，SQL 与参考实现一致）
#[test]
fn duplicate_allowed_matches_planner_sql() {
    // 覆盖三种名称关系（同名 / 副本名 / 不同名）与三种开关组合。
    let pairs = [
        (record(1, "a.txt", "a.txt"), record(2, "a.txt", "a.txt")), // 同名
        (record(1, "a.txt", "a.txt"), record(2, "a (1).txt", "a.txt")), // 副本名
        (record(1, "a.txt", "a.txt"), record(2, "b.txt", "b.txt")), // 不同名
    ];
    for dedup_same_name in [false, true] {
        for dedup_copy_names in [false, true] {
            for dedup_other_names in [false, true] {
                let cfg = Config {
                    dedup_same_name,
                    dedup_copy_names,
                    dedup_other_names,
                    ..Config::default()
                };
                for (a, b) in &pairs {
                    assert_eq!(
                        duplicate_allowed(a, b, &cfg), sql_match(a, b, &cfg),
                        "a={:?} b={:?} flags=({dedup_same_name},{dedup_copy_names},{dedup_other_names})",
                        a.name, b.name
                    );
                }
            }
        }
    }
}

// 覆盖 C-03 / 附录 B：路径平局按 Windows 忽略大小写序数升序，再按原始 UTF-16 决胜。
#[test]
fn compare_path_tie_break_uses_case_insensitive_ordinal_order() {
    let mut uppercase_z = record(1, "z.txt", "z.txt");
    uppercase_z.rel = "Z/file".into();
    let mut lowercase_a = record(2, "a.txt", "a.txt");
    lowercase_a.rel = "a/file".into();
    assert_eq!(
        compare(&lowercase_a, &uppercase_z, KeepPolicy::Newest),
        Ordering::Less
    );

    let mut uppercase_a = record(3, "a.txt", "a.txt");
    uppercase_a.rel = "A/file".into();
    let mut lowercase_a = record(4, "a.txt", "a.txt");
    lowercase_a.rel = "a/file".into();
    assert_eq!(
        compare(&uppercase_a, &lowercase_a, KeepPolicy::Newest),
        Ordering::Less
    );
}

// 覆盖附录 A（大类映射：复合后缀、分卷族、.ts 归视频、未匹配归其他）
#[test]
fn category_matches_appendix_a() {
    for (name, expected) in [
        ("a.mp4", "视频"),
        ("a.TS", "视频"),
        ("a.m2ts", "视频"),
        ("a.mp3", "音频"),
        ("a.opus", "音频"),
        ("a.jpg", "图片"),
        ("a.HEIC", "图片"),
        ("a.raw", "图片"),
        ("a.pdf", "文档"),
        ("a.azw3", "文档"),
        ("a.chm", "文档"),
        ("a.zip", "压缩包"),
        ("a.tar.gz", "压缩包"),
        ("a.TAR.ZST", "压缩包"),
        ("a.lz4", "压缩包"),
        ("a.br", "压缩包"),
        ("a.7z.001", "压缩包"),
        ("a.part01.rar", "压缩包"),
        ("b.r00", "压缩包"),
        ("b.z02", "压缩包"),
        ("b.z00", "其他"),
        ("a.exe", "程序"),
        ("a.ps1", "程序"),
        ("a.msixbundle", "程序"),
        ("a.unknownext", "其他"),
        ("noext", "其他"),
        ("data.001", "其他"),
    ] {
        assert_eq!(category_for(&name.to_lowercase()), expected, "{name}");
    }
}

// 覆盖 C-19（命名摘要：SHA-256 大写十六进制、前 8 位起、可扩展）
#[test]
fn path_digest_is_uppercase_sha256_prefix() {
    let d8 = path_digest("a/b/合同.pdf", 8);
    assert_eq!(d8.len(), 8);
    assert!(d8
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    let d10 = path_digest("a/b/合同.pdf", 10);
    assert_eq!(&d10[..8], &d8);
    // 相同输入相同摘要；不同输入（通常）不同。
    assert_eq!(path_digest("a/b/合同.pdf", 8), d8);
}

// 覆盖 附录C（S-04/R-03）：通配符只有 * 与 ?，其余元字符一律字面匹配；
// ? 恰好一个字符且不跨 /；* 可跨 /；连续 * 等同单个 *；\ 是分隔符不是转义符。
#[test]
fn exclusion_globs_follow_appendix_c_literal_semantics() {
    let matches = |text: &str, rel: &str| {
        build_exclusions(text)
            .unwrap_or_else(|error| panic!("模式 {text} 应合法：{error:#}"))
            .is_match(rel)
    };
    // 字符类与花括号展开不支持：含这些元字符的目录名按字面命中（Windows 常见
    // 形如 movie [1080p] 的目录名不得静默失去排除保护）。模式匹配整个相对路径，
    // 目录整树排除靠剪枝实现，不要求子路径逐个命中。
    assert!(matches("movie [1080p]", "movie [1080p]"));
    assert!(
        !matches("movie [1080p]", "movie 1"),
        "[1080p] 不得按字符类展开命中无关项"
    );
    assert!(
        !matches("movie [1080p]", "movie 1/a.txt"),
        "[1080p] 不得按字符类展开命中无关子路径"
    );
    assert!(matches("[ab]", "[ab]"));
    assert!(
        !matches("[ab]", "a"),
        "附录 C：[ab] 匹配字面 [ab]，不是 a 或 b"
    );
    assert!(!matches("[ab]", "b"));
    assert!(matches("{a,b}.txt", "{a,b}.txt"));
    assert!(!matches("{a,b}.txt", "a.txt"));
    assert!(!matches("{a,b}.txt", "b.txt"));
    // ? 恰好一个字符，不匹配 /，也不跨层级（附录 C 例）。
    assert!(matches("a/?.txt", "a/x.txt"));
    assert!(!matches("a/?.txt", "a/xy.txt"));
    assert!(!matches("a/?.txt", "a/b/x.txt"));
    assert!(!matches("a/?.txt", "a/.txt"), "? 必须恰好一个字符");
    assert!(!matches("a/?.txt", "a/x/y.txt"));
    assert!(!matches("a?b", "a/b"), "? 不得跨 /");
    assert!(matches("a?b", "axb"));
    // * 为零个或多个字符，可跨 /。
    assert!(matches("*.tmp", "x.tmp"));
    assert!(matches("*.tmp", "a/b/c.tmp"));
    // 连续多个星号等同一个星号，** 没有独立递归语义（不得匹配根级 cache）。
    assert!(matches("**/cache", "a/cache"));
    assert!(matches("**/cache", "a/b/cache"));
    assert!(!matches("**/cache", "cache"));
    // 附录 C 组合例保持不变。
    assert!(matches("cache;*/cache", "cache"));
    assert!(matches("cache;*/cache", "a/cache"));
    assert!(matches("cache;*/cache", "a/b/cache"));
    assert!(!matches("cache;*/cache", "cachex"));
    // 用户模式中的 \ 视为目录分隔符，不作为转义符。
    assert!(matches("a\\b\\x.tmp", "a/b/x.tmp"));
}

// 覆盖 附录C：盘符、绝对路径与独立 .. 段是无效模式，阻止开始并指出错误段，
// 不悄悄忽略（否则这些模式变成永不命中的死规则，排除保护静默失效）。
#[test]
fn exclusion_globs_reject_drive_absolute_and_parent_segments() {
    for bad in [
        "D:\\备份\\私人资料",
        "c:/x",
        "/abs/path",
        "\\rooted",
        "a/../b",
        "a/b/..",
        "..",
    ] {
        let error = build_exclusions(bad)
            .err()
            .unwrap_or_else(|| panic!("模式 {bad} 应被判定为无效"));
        let text = error.to_string();
        assert!(text.contains(bad), "错误信息必须指出错误段原文：{text}");
    }
    // 非上述三类的一律合法：.. 的超集串、纯文件名模式都正常构建。
    for good in [
        "cache;*/cache",
        "a/?.txt",
        "movie [1080p]",
        "a..b",
        "...",
        "a/..b",
    ] {
        build_exclusions(good).unwrap_or_else(|error| panic!("模式 {good} 应合法：{error:#}"));
    }
}

// 覆盖 C-19 / C-20：完整摘要候选未超过 40 时不提前截短主体。
#[test]
fn digest_candidate_preserves_full_stem_below_yield_threshold() {
    for stem in ["abcdefghijklmnopqrstuvwxyz", "abcdefghijklmnopqrstuvwxyz0"] {
        assert_eq!(
            digest_candidate(None, stem, "A83F21C7", ".pdf", None, false),
            Some(format!("{stem}_A83F21C7.pdf"))
        );
    }
    assert_eq!(
        digest_candidate(
            None,
            "abcdefghijklmnopqrstuvwxyz01",
            "A83F21C7",
            ".pdf",
            None,
            false
        ),
        Some("abcdefghijklmnopqrstuvwxy_A83F21C7.pdf".to_string())
    );
}

// 覆盖 C-20：带来源的完整候选超 40 后必须去掉来源，再按 25 单元截短。
#[test]
fn digest_candidate_drops_source_before_truncating_stem() {
    assert_eq!(
        digest_candidate(
            Some("a"),
            "abcdefghijklmnopqrstuvwxyz",
            "A83F21C7",
            ".pdf",
            None,
            false
        ),
        Some("abcdefghijklmnopqrstuvwxy_A83F21C7.pdf".to_string())
    );
}

// 覆盖 C-18 / C-20：已进入长度降级时，无来源候选即使能放下也须截到 25。
#[test]
fn digest_candidate_forced_truncation_keeps_c20_cap() {
    assert_eq!(
        digest_candidate(
            None,
            "abcdefghijklmnopqrstuvwxyz",
            "A83F21C7",
            ".pdf",
            None,
            true
        ),
        Some("abcdefghijklmnopqrstuvwxy_A83F21C7.pdf".to_string())
    );
}

// 覆盖 C-20（长度受限摘要候选：≤40 退让、扩展名不截断、超限返回 None）
#[test]
fn digest_candidate_respects_unit_budget() {
    let long_stem = "二".repeat(60);
    // C-20：带来源形式超 40 时去掉来源段，只保留主体前段+摘要+扩展名。
    let candidate =
        digest_candidate(Some("年报"), &long_stem, "A83F21C7", ".pdf", None, false).unwrap();
    assert!(candidate.encode_utf16().count() <= CONFLICT_YIELD_UNITS);
    assert!(candidate.starts_with("二"));
    assert!(candidate.ends_with("_A83F21C7.pdf"));
    assert!(!candidate.contains('年'));
    // 短主体带来源：保留 C-19 完整形式。
    let with_source =
        digest_candidate(Some("年报"), "产品说明_1", "A13F72C4", ".pdf", None, false).unwrap();
    assert_eq!(with_source, "年报_产品说明_1_A13F72C4.pdf");
    // 无来源段时省略来源。
    let none_source = digest_candidate(None, "资料", "A83F21C7", ".pdf", None, false).unwrap();
    assert_eq!(none_source, "资料_A83F21C7.pdf");
    // 扩展名与摘要自身挤爆预算：允许超过 40 但不得超过 255；再放不下则 None。
    let huge_ext = format!(".{}", "e".repeat(300));
    assert!(digest_candidate(None, "x", "A83F21C7", &huge_ext, None, false).is_none());
}

// 特征测试（C-20 带内路径）：预算被摘要/扩展名挤到 40 以下时，主体保留一个
// 完整字符（结果超 40 但 ≤255），而不是返回 None。
#[test]
fn digest_candidate_keeps_one_character_when_budget_exhausted() {
    // fixed = 1(分隔符) + 35(摘要) + 4(扩展名) = 40：预算 0，仍保留单字符主体。
    let candidate = digest_candidate(None, "abc", &"d".repeat(35), ".txt", None, false).unwrap();
    assert_eq!(candidate, format!("a_{}.txt", "d".repeat(35)));
    assert!(candidate.encode_utf16().count() > CONFLICT_YIELD_UNITS);
    assert!(candidate.encode_utf16().count() <= SEGMENT_LIMIT_UNITS);
    // 首字符是代理对（2 单元）时同样保留完整字符，不拆成空。
    let surrogate_stem = "\u{20BB7}xx";
    let paired =
        digest_candidate(None, surrogate_stem, &"d".repeat(35), ".txt", None, false).unwrap();
    assert!(paired.starts_with('\u{20BB7}'));
    // 主体为空（或连第一个字符都构不成）时安全失败。
    assert!(digest_candidate(None, "", &"d".repeat(35), ".txt", None, false).is_none());
}
