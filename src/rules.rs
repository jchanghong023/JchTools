use crate::{
    config::{Config, KeepPolicy},
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
    #[allow(clippy::expect_used)] // 常量正则语法错误只可能是开发期笔误，按不可达处理
    let expression = COPY_SUFFIX.get_or_init(|| Regex::new(r"(?i)(?:\s*[（(]\d+[）)]|\s*[-_ ]+copy(?:\s*[（(]?\d+[）)]?)?|\s*[-_ ]*副本(?:\s*[（(]?\d+[）)]?)?)$").expect("constant regex"));
    let path = Path::new(name);
    let original = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let mut stem = original.to_string();
    loop {
        let next = expression.replace(&stem, "").trim().to_string();
        if next.is_empty() || next == stem {
            break;
        }
        stem = next;
    }
    match path.extension().and_then(|s| s.to_str()) {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem,
    }
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
pub fn cleanup_reason(rel: &str, size: u64, cfg: &Config) -> Option<&'static str> {
    let file = Path::new(rel).file_name()?.to_str()?.to_lowercase();
    if cfg.clean_junk
        && (["thumbs.db", ".ds_store", "desktop.ini"].contains(&file.as_str())
            || file.starts_with("._")
            || rel.split('/').any(|s| s.eq_ignore_ascii_case("__MACOSX")))
    {
        return Some("用户开启的垃圾文件规则");
    }
    if cfg.clean_temp
        && (has_ext(&file, "tmp")
            || has_ext(&file, "temp")
            || has_ext(&file, "bak")
            || file.starts_with("~$"))
    {
        return Some("用户开启的临时/备份文件规则");
    }
    if cfg.clean_zero && size == 0 {
        return Some("用户开启的零字节文件规则");
    }
    None
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
        #[allow(clippy::expect_used)] // 常量正则语法错误只可能是开发期笔误，按不可达处理
        let re = PART.get_or_init(|| Regex::new(r"\.part(\d+)\.rar$").expect("constant regex"));
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
    #[allow(clippy::expect_used)] // 常量正则语法错误只可能是开发期笔误，按不可达处理
    let re = PART.get_or_init(|| Regex::new(r"\.part(\d+)\.rar$").expect("constant regex"));
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
