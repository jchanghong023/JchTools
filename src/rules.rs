use crate::{config::{Config, KeepPolicy}, fsutil, model::FileRecord};
use anyhow::{bail, Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;
use std::{cmp::Ordering, collections::BTreeMap, path::Path, sync::OnceLock};
use unicode_normalization::UnicodeNormalization;

pub fn build_exclusions(text: &str) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for part in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        builder.add(GlobBuilder::new(part).case_insensitive(true).literal_separator(false).build()?);
    }
    Ok(builder.build()?)
}
pub fn parse_categories(text: &str) -> Result<BTreeMap<String,String>> {
    let mut map = BTreeMap::new();
    for item in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (category, extensions) = item.split_once('=').context("自定义分类格式：目录=pdf,docx;图片=jpg,png")?;
        fsutil::validate_component(category.trim())?;
        for ext in extensions.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !ext.chars().all(|c| c.is_ascii_alphanumeric()) { bail!("扩展名只能包含英文字母和数字：{ext}"); }
            map.insert(ext.to_lowercase(), category.trim().into());
        }
    }
    Ok(map)
}
pub fn strip_copy_name(name: &str) -> String {
    static COPY_SUFFIX: OnceLock<Regex> = OnceLock::new();
    // 拉丁字母紧贴（photocopy / MyCopy）不是副本命名，分隔符必须至少一个；
    // 中文「副本」紧贴是常见命名习惯（报告副本.pdf → 报告.pdf），允许无分隔符。
    let expression = COPY_SUFFIX.get_or_init(|| Regex::new(r"(?i)(?:\s*[（(]\d+[）)]|\s*[-_ ]+copy(?:\s*[（(]?\d+[）)]?)?|\s*[-_ ]*副本(?:\s*[（(]?\d+[）)]?)?)$").expect("constant regex"));
    let path = Path::new(name);
    let original = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let mut stem = original.to_string();
    loop {
        let next = expression.replace(&stem, "").trim().to_string();
        if next.is_empty() || next == stem { break; }
        stem = next;
    }
    match path.extension().and_then(|s| s.to_str()) { Some(ext) => format!("{stem}.{ext}"), None => stem }
}
pub fn normal_name(name: &str) -> String { strip_copy_name(name).nfc().collect::<String>().to_lowercase() }
pub fn normalize_name(name: &str) -> String { name.nfc().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ") }
pub fn duplicate_allowed(a: &FileRecord, b: &FileRecord, cfg: &Config) -> bool {
    if a.name == b.name { cfg.dedup_same_name }
    else if a.normalized == b.normalized { cfg.dedup_copy_names }
    else { cfg.dedup_other_names }
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
    primary.then_with(|| a.rel.chars().count().cmp(&b.rel.chars().count())).then_with(|| a.rel.cmp(&b.rel))
}
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
        "pdf"|"doc"|"docx"|"xls"|"xlsx"|"ppt"|"pptx"|"txt"|"md"|"csv"|"rtf"|"odt"|"epub" => "文档",
        "jpg"|"jpeg"|"png"|"webp"|"gif"|"bmp"|"svg"|"tif"|"tiff"|"heic"|"avif" => "图片",
        "mp4"|"mkv"|"avi"|"mov"|"wmv"|"webm"|"m4v" => "视频",
        "mp3"|"wav"|"flac"|"aac"|"ogg"|"m4a"|"wma"|"opus" => "音频",
        "zip"|"7z"|"rar"|"tar"|"gz"|"bz2"|"xz"|"zst"|"tgz" => "压缩包",
        "rs"|"py"|"c"|"cpp"|"h"|"hpp"|"js"|"ts"|"tsx"|"html"|"css"|"json"|"yaml"|"yml"|"toml"|"tcl"|"v"|"sv"|"vhd" => "代码",
        "exe"|"msi"|"msix"|"appx" => "安装包",
        _ => "其他",
    }
}
pub fn cleanup_reason(rel: &str, size: u64, cfg: &Config) -> Option<&'static str> {
    let file = Path::new(rel).file_name()?.to_str()?.to_lowercase();
    if cfg.clean_junk && (["thumbs.db", ".ds_store", "desktop.ini"].contains(&file.as_str())
        || file.starts_with("._") || rel.split('/').any(|s| s.eq_ignore_ascii_case("__MACOSX"))) { return Some("用户开启的垃圾文件规则"); }
    if cfg.clean_temp && (file.ends_with(".tmp") || file.ends_with(".temp") || file.ends_with(".bak") || file.starts_with("~$")) { return Some("用户开启的临时/备份文件规则"); }
    if cfg.clean_zero && size == 0 { return Some("用户开启的零字节文件规则"); }
    None
}
pub fn archive_name(name: &str) -> bool {
    let name = name.to_lowercase();
    if name.ends_with(".rar") {
        static PART: OnceLock<Regex> = OnceLock::new();
        let re = PART.get_or_init(|| Regex::new(r"\.part(\d+)\.rar$").expect("constant regex"));
        if let Some(caps) = re.captures(&name) { return caps[1].parse::<u64>().ok() == Some(1); }
        return true;
    }
    [".zip", ".7z", ".tar", ".gz", ".bz2", ".xz", ".zst", ".tgz", ".tbz2", ".txz", ".cab", ".iso", ".wim", ".lzh", ".cpio", ".7z.001", ".zip.001"].iter().any(|suffix| name.ends_with(suffix))
}
pub fn multipart_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.ends_with(".001") || (n.ends_with(".rar") && n.contains(".part"))
}
