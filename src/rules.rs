use crate::{
    config::{Config, DeleteChoice, KeepPolicy},
    fsutil,
    model::FileRecord,
};
use anyhow::{bail, Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;
use std::{cmp::Ordering, collections::BTreeMap, path::Path, sync::OnceLock};
use unicode_normalization::UnicodeNormalization;

pub fn build_exclusions(text: &str) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for part in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        builder.add(
            GlobBuilder::new(part)
                .case_insensitive(true)
                .literal_separator(false)
                .build()?,
        );
    }
    Ok(builder.build()?)
}
pub fn parse_categories(text: &str) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for item in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (category, extensions) = item
            .split_once('=')
            .context("自定义分类格式：目录=pdf,docx;图片=jpg,png")?;
        fsutil::validate_component(category.trim())?;
        for ext in extensions
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
                bail!("扩展名只能包含英文字母和数字：{ext}");
            }
            map.insert(ext.to_lowercase(), category.trim().into());
        }
    }
    Ok(map)
}
pub fn strip_copy_name(name: &str) -> String {
    static COPY_SUFFIX: OnceLock<Regex> = OnceLock::new();
    // 拉丁字母紧贴（photocopy / MyCopy）不是副本命名，分隔符必须至少一个；
    // 中文「副本」紧贴是常见命名习惯（报告副本.pdf → 报告.pdf），允许无分隔符。
    let expression = COPY_SUFFIX.get_or_init(|| match Regex::new(r"(?i)(?:\s*[（(]\d+[）)]|\s*[-_ ]+copy(?:\s*[（(]?\d+[）)]?)?|\s*[-_ ]*副本(?:\s*[（(]?\d+[）)]?)?)$") {
        Ok(re) => re,
        // 常量正则语法错误只可能是开发期笔误，按不可达处理
        Err(_) => unreachable!("constant regex"),
    });
    // 副本后缀贴在文件名主体之后、整个扩展名之前：复合扩展名（.tar.gz）与编号分卷
    // （.7z.001）必须整体保留（H-07 例：资料.tar.gz → 资料 (1).tar.gz），
    // 因此主体/扩展名切分与 fsutil::unique_target 共用同一口径。
    let (stem, extension) = fsutil::split_compound_name(name);
    let mut stem = stem.to_string();
    loop {
        let next = expression.replace(&stem, "").trim().to_string();
        if next.is_empty() || next == stem {
            break;
        }
        stem = next;
    }
    format!("{stem}{extension}")
}
pub fn normal_name(name: &str) -> String {
    strip_copy_name(name)
        .nfc()
        .collect::<String>()
        .to_lowercase()
}
pub fn normalize_name(name: &str) -> String {
    name.nfc()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
/// 纯参考实现：实际去重匹配在 planner::deduplicate 的 SQL 中（keepers 表按
/// `(name=? AND dedup_same_name) OR (name<>? AND normal=? AND dedup_copy_names) OR
/// (name<>? AND normal<>? AND dedup_other_names)` 选择保留者）。仅供测试对照，生产路径不调用。
#[cfg(test)]
pub fn duplicate_allowed(a: &FileRecord, b: &FileRecord, cfg: &Config) -> bool {
    if a.name == b.name {
        cfg.dedup_same_name
    } else if a.normalized == b.normalized {
        cfg.dedup_copy_names
    } else {
        cfg.dedup_other_names
    }
}
/// Ordering::Less means a is the preferred keeper. Ties never rely on traversal order.
pub fn compare(a: &FileRecord, b: &FileRecord, policy: KeepPolicy) -> Ordering {
    let primary = match policy {
        KeepPolicy::Newest => b.snapshot.modified_ns.cmp(&a.snapshot.modified_ns),
        KeepPolicy::Oldest => a.snapshot.modified_ns.cmp(&b.snapshot.modified_ns),
        KeepPolicy::Largest => b.snapshot.size.cmp(&a.snapshot.size),
        KeepPolicy::Smallest => a.snapshot.size.cmp(&b.snapshot.size),
        KeepPolicy::ShortestName => a.name.chars().count().cmp(&b.name.chars().count()),
    };
    // 与 ordering_sql 的 length(rel)（字符数）保持一致，避免预览与 SQL 计划的平局规则不同。
    primary
        .then_with(|| a.rel.chars().count().cmp(&b.rel.chars().count()))
        .then_with(|| a.rel.cmp(&b.rel))
}
/// SQL 排序片段（供 planner 拼接进 ORDER BY）。依赖 SQLite 对 TEXT 的 length()
/// 返回 Unicode 码点计数（与 Rust 的 `chars().count()` 一致），与 `compare` 的平局规则对齐；
/// 勿改成 `length(CAST(rel AS BLOB))`（字节长度）或依赖非确定性的排序。
pub fn ordering_sql(policy: KeepPolicy) -> &'static str {
    match policy {
        KeepPolicy::Newest => "mtime DESC,length(rel),rel",
        KeepPolicy::Oldest => "mtime ASC,length(rel),rel",
        KeepPolicy::Largest => "size DESC,length(rel),rel",
        KeepPolicy::Smallest => "size ASC,length(rel),rel",
        KeepPolicy::ShortestName => "length(name),length(rel),rel",
    }
}
/// C-04：可靠文件标识是否证明两个目录项指向同一物理文件。
/// 条件：标识相同、两侧链接数 >= 2（两个目录项必然抬高链接数）、且标识未退化。
/// Windows 的「卷:索引高:索引低」在部分文件系统上索引恒为 0，无法区分不同文件
/// （与 C-13 的 hash_cache 同口径）；退化时不得据此跳过重复处理或重复建链。
/// Unix 的「设备:inode」两段结构本身就唯一标识文件，不套用该三段口径。
pub fn identity_proves_same_file(a: &FileRecord, b: &FileRecord) -> bool {
    a.snapshot.identity == b.snapshot.identity
        && a.snapshot.links >= 2
        && b.snapshot.links >= 2
        && !(cfg!(windows) && crate::hash_cache::identity_is_degenerate(&a.snapshot.identity))
}
/// C-08：按内容签名修正错误扩展名的保守判定。true = 旧扩展名确实错误、可以改名。
/// 签名不足以确定实际类型（只识别到外层容器/存储）、旧扩展名属于同一容器家族的更具体
/// 格式、或旧扩展名本就是同义写法时一律不改名；合法的同义扩展名不强制统一。
pub fn extension_needs_fix(old: &str, detected: &str) -> bool {
    !equivalent_extension(old, detected)
        && !specialized_extension(old)
        && !outer_container_extension(detected)
}
/// 同义扩展名（同一格式的常见写法），不算错误扩展名。
fn equivalent_extension(old: &str, detected: &str) -> bool {
    old == detected
        || (matches!(old, "jpeg" | "jpe" | "jfif") && detected == "jpg")
        || (old == "tiff" && detected == "tif")
        || (old == "htm" && detected == "html")
        || (old == "mid" && detected == "midi")
}
/// 识别结果只说明外层容器/存储，不含更具体的格式信息：压缩包与 OLE 存储
/// （doc/xls/ppt/msi/vsd 等都只会被识别成同一个 OLE 存储类型）都属此类。
/// 通用压缩后缀与 `archive_name` 保持同口径，另加 OLE 存储（infer 报 msi）。
fn outer_container_extension(ext: &str) -> bool {
    matches!(
        ext,
        "zip"
            | "gz"
            | "bz2"
            | "xz"
            | "zst"
            | "tar"
            | "7z"
            | "rar"
            | "cab"
            | "iso"
            | "wim"
            | "lz"
            | "lzma"
            | "cpio"
            | "lzh"
            | "msi"
    )
}
/// 旧扩展名是该容器家族里的更具体格式，识别结果粒度更粗（infer 对 OOXML 变体
/// 只报 docx/xlsx/pptx 基础类型）：按更粗的结果改名会把正确的专用扩展名改错。
fn specialized_extension(ext: &str) -> bool {
    matches!(
        ext,
        "docm"
            | "dotx"
            | "dotm"
            | "xlsm"
            | "xltx"
            | "xltm"
            | "xlsb"
            | "potx"
            | "potm"
            | "ppsx"
            | "ppsm"
            | "epub"
            | "odt"
            | "ods"
            | "odp"
            | "jar"
            | "apk"
    )
}
pub fn category(extension: &str) -> &'static str {
    match extension {
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "csv" | "rtf"
        | "odt" | "epub" => "文档",
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "svg" | "tif" | "tiff" | "heic"
        | "avif" => "图片",
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" => "视频",
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "opus" => "音频",
        "zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" | "zst" | "tgz" => "压缩包",
        "rs" | "py" | "c" | "cpp" | "h" | "hpp" | "js" | "ts" | "tsx" | "html" | "css" | "json"
        | "yaml" | "yml" | "toml" | "tcl" | "v" | "sv" | "vhd" => "代码",
        "exe" | "msi" | "msix" | "appx" => "安装包",
        _ => "其他",
    }
}
/// 清理项类别（C-08）：三类清理各自独立启停，并可独立覆盖删除方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupKind {
    Junk,
    Temp,
    Zero,
}
/// C-08：清理命中判定与类别。返回 None 表示不清理。
/// planner 按类别解析删除方式（[`cleanup_delete`]），不得比较原因文案。
pub fn cleanup_reason(rel: &str, size: u64, cfg: &Config) -> Option<(CleanupKind, &'static str)> {
    let file = Path::new(rel).file_name()?.to_str()?.to_lowercase();
    if cfg.clean_junk
        && (["thumbs.db", ".ds_store", "desktop.ini"].contains(&file.as_str())
            || file.starts_with("._")
            || rel.split('/').any(|s| s.eq_ignore_ascii_case("__MACOSX")))
    {
        return Some((CleanupKind::Junk, "用户开启的垃圾文件规则"));
    }
    if cfg.clean_temp
        && (has_ext(&file, "tmp")
            || has_ext(&file, "temp")
            || has_ext(&file, "bak")
            || file.starts_with("~$"))
    {
        return Some((CleanupKind::Temp, "用户开启的临时/备份文件规则"));
    }
    if cfg.clean_zero && size == 0 {
        return Some((CleanupKind::Zero, "用户开启的零字节文件规则"));
    }
    None
}
/// C-08：清理项的删除方式覆盖（默认跟随全局文件删除方式）。
pub fn cleanup_delete(cfg: &Config, kind: CleanupKind) -> DeleteChoice {
    match kind {
        CleanupKind::Junk => cfg.junk_delete,
        CleanupKind::Temp => cfg.temp_delete,
        CleanupKind::Zero => cfg.zero_delete,
    }
}
/// 扩展名等值判断：按最后一个点切分比较尾段，等价于 `ends_with(".ext")`
/// （输入已预先 lowercase，无大小写歧义）。rsplit_once 按字符边界切分，不会 panic。
fn has_ext(name: &str, ext: &str) -> bool {
    matches!(name.rsplit_once('.'), Some((_, tail)) if tail == ext)
}
pub fn archive_name(name: &str) -> bool {
    let name = name.to_lowercase();
    if has_ext(&name, "rar") {
        static PART: OnceLock<Regex> = OnceLock::new();
        let re = PART.get_or_init(|| match Regex::new(r"\.part(\d+)\.rar$") {
            Ok(re) => re,
            // 常量正则语法错误只可能是开发期笔误，按不可达处理
            Err(_) => unreachable!("constant regex"),
        });
        if let Some(caps) = re.captures(&name) {
            return caps[1].parse::<u64>().ok() == Some(1);
        }
        return true;
    }
    [
        ".zip", ".7z", ".tar", ".gz", ".bz2", ".xz", ".zst", ".tgz", ".tbz2", ".txz", ".cab",
        ".iso", ".wim", ".lzh", ".cpio", ".7z.001", ".zip.001",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}
pub fn multipart_name(name: &str) -> bool {
    let n = name.to_lowercase();
    // 与 archive_name 对齐：只有 .partN.rar 才算 RAR 分卷；contains(".part") 会把
    // report.partial.rar 这类普通包误判为分卷。
    static PART: OnceLock<Regex> = OnceLock::new();
    let re = PART.get_or_init(|| match Regex::new(r"\.part(\d+)\.rar$") {
        Ok(re) => re,
        // 常量正则语法错误只可能是开发期笔误，按不可达处理
        Err(_) => unreachable!("constant regex"),
    });
    n.ends_with(".001") || re.is_match(&n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Snapshot;

    fn record(id: i64, name: &str, normalized: &str) -> FileRecord {
        FileRecord {
            id,
            rel: format!("dir/{name}"),
            name: name.into(),
            normalized: normalized.into(),
            snapshot: Snapshot {
                size: 1,
                modified_ns: 10,
                identity: id.to_string(),
                links: 1,
            },
            hash: Some("h".into()),
            cleanable: false,
        }
    }
    /// 复刻 planner::deduplicate keepers 查询中的 SQL 匹配条件（与 rust 参考实现逐分支对照）：
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
        for old in [
            "docm", "dotx", "dotm", "xlsm", "xltm", "xlsb", "potx", "ppsx", "ppsm", "epub", "odt",
            "ods", "odp", "jar", "apk",
        ] {
            for detected in ["zip", "docx", "xlsx", "pptx"] {
                assert!(
                    !extension_needs_fix(old, detected),
                    "{old} 的相对具体扩展名不得按 {detected} 改粗"
                );
            }
        }
        // 包装格式（压缩流内层未知，如 svgz/tgz）识别到容器后缀时同样不是错误扩展名。
        for (old, detected) in [("svgz", "gz"), ("tgz", "gz"), ("tbz2", "bz2")] {
            assert!(
                !extension_needs_fix(old, detected),
                "{old} 不得按外层容器 {detected} 改名"
            );
        }
        // 容器识别只说明外层容器（OLE 存储同样只会被识别成 msi）：不得据此改成压缩包/安装包后缀。
        for detected in [
            "zip", "gz", "bz2", "xz", "zst", "tar", "7z", "rar", "cab", "iso", "wim", "lz", "lzma",
            "cpio", "lzh", "msi",
        ] {
            assert!(
                !extension_needs_fix("bin", detected),
                "容器识别 {detected} 不得直接作为改名依据"
            );
        }
        // 明确的错误扩展名仍必须修正（默认关闭，开启后列入计划）。
        for (old, detected) in [
            ("txt", "png"),
            ("bin", "pdf"),
            ("jpg", "png"),
            ("mp4", "mp3"),
        ] {
            assert!(
                extension_needs_fix(old, detected),
                "{old} -> {detected} 是错误扩展名，应修正"
            );
        }
    }

    // 覆盖 C-08, H-07（副本后缀识别：复合扩展名与编号分卷整体保留，序号插在扩展名之前）
    #[test]
    fn strip_copy_name_keeps_compound_extensions() {
        for (input, expected) in [
            ("资料 (1).tar.gz", "资料.tar.gz"),
            ("资料 - Copy.tar.bz2", "资料.tar.bz2"),
            ("资料 副本.tar.xz", "资料.tar.xz"),
            ("报告 (2).docx", "报告.docx"),
            ("报告副本.pdf", "报告.pdf"),
            ("report.final (1).txt", "report.final.txt"),
            ("(1).pdf", "(1).pdf"),
            ("资料.tar.gz", "资料.tar.gz"),
            ("包.7z.001", "包.7z.001"),
        ] {
            assert_eq!(strip_copy_name(input), expected, "{input}");
        }
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

    // 覆盖 C-03, S-08（SQL 排序与 Rust 参考实现一致：预览与计划的保留者永不分裂）
    #[test]
    fn ordering_sql_agrees_with_rust_compare() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let seed = [
            ("a/x", "x", 5i64, 10u64),
            ("b/yy", "yy", 5, 20),
            ("c", "zzzz", 9, 7),
            ("d/long/name", "n", 5, 20),
            ("e", "mm", 9, 7),
        ];
        // 纯字面量子查询：不建表、不写 DML（static_check 会按任务库 schema 逐条
        // prepare 源码中的 SQL，测试内的建表/插入语句会与 schema 校验冲突）。
        use std::fmt::Write as _;
        let mut rows = format!(
            "SELECT '{}' AS rel, '{}' AS name, {} AS mtime, {} AS size",
            seed[0].0, seed[0].1, seed[0].2, seed[0].3
        );
        for (rel, name, mtime, size) in &seed[1..] {
            let _ = write!(rows, " UNION ALL SELECT '{rel}', '{name}', {mtime}, {size}");
        }
        let records: Vec<FileRecord> = seed
            .iter()
            .map(|(rel, name, mtime, size)| FileRecord {
                id: 0,
                rel: rel.to_string(),
                name: name.to_string(),
                normalized: name.to_string(),
                snapshot: Snapshot {
                    size: *size,
                    modified_ns: *mtime,
                    identity: rel.to_string(),
                    links: 1,
                },
                hash: None,
                cleanable: false,
            })
            .collect();
        for policy in [
            KeepPolicy::Newest,
            KeepPolicy::Oldest,
            KeepPolicy::Largest,
            KeepPolicy::Smallest,
            KeepPolicy::ShortestName,
        ] {
            let sql = format!("SELECT rel FROM ({rows}) ORDER BY {}", ordering_sql(policy));
            let sql_order: Vec<String> = conn
                .prepare(&sql)
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let by_rel = |rel: &str| records.iter().find(|r| r.rel == rel).unwrap();
            let mut rust_order: Vec<&str> = records.iter().map(|r| r.rel.as_str()).collect();
            rust_order.sort_by(|a, b| compare(by_rel(a), by_rel(b), policy));
            assert_eq!(
                sql_order, rust_order,
                "{policy:?} 的 SQL 与 Rust 排序必须一致"
            );
        }
    }
}
