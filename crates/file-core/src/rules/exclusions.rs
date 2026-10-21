use crate::fsutil;
use anyhow::{bail, Result};
use std::path::Path;

// ---------------------------------------------------------------------------
// 附录 C：扫描排除 glob（S-04 / R-03）
// ---------------------------------------------------------------------------

/// 附录 C：仅支持 * / ? 的 Unicode 匹配器；所有字面量复用 Windows 序数比较键。
pub struct Exclusions {
    patterns: regex::RegexSet,
}

impl Exclusions {
    pub fn is_match<P: AsRef<Path>>(&self, path: P) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        path.as_ref()
            .to_str()
            .is_some_and(|text| self.patterns.is_match(fsutil::fold_rel(text).as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

fn translate_exclusion_part(part: &str) -> String {
    let folded = fsutil::fold_rel(&part.replace('\\', "/"));
    let mut out = String::from(r"\A");
    let mut chars = folded.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                while chars.peek() == Some(&'*') {
                    chars.next();
                }
                out.push_str("(?s:.*)");
            }
            '?' if cfg!(windows) => {
                // fold_rel 把 UTF-16 代理单元无损映射到 E000..E7FF，原始 BMP
                // 字符整体避开此区间。一个 Unicode 字符对应一个非代理单元或
                // 一对代理单元；不能让 ? 只吃掉 emoji 的一半。
                out.push_str(r"(?:[\x{E000}-\x{E3FF}][\x{E400}-\x{E7FF}]|[^\x{E000}-\x{E7FF}/])");
            }
            '?' => out.push_str("[^/]"),
            c => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    out.push_str(r"\z");
    out
}

/// 附录 C 无效模式判定：返回 Some(原因) 表示该段含盘符、以 `/` 或 `\` 开头的
/// 绝对路径，或存在恰好等于 `..` 的路径段（`\` 同样按分隔符切分）。
fn invalid_exclusion_reason(part: &str) -> Option<&'static str> {
    let mut chars = part.chars();
    if matches!(chars.next(), Some(c) if c.is_ascii_alphabetic()) && chars.next() == Some(':') {
        return Some("含盘符");
    }
    if part.starts_with('/') || part.starts_with('\\') {
        return Some("是绝对路径");
    }
    if part.split(['/', '\\']).any(|segment| segment == "..") {
        return Some("含独立的 .. 路径段");
    }
    None
}

pub fn build_exclusions(text: &str) -> Result<Exclusions> {
    let mut patterns = Vec::new();
    for part in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        // 附录 C：盘符、绝对路径与独立 .. 段是无效模式，阻止开始并指出错误段，
        // 不悄悄忽略（否则该模式变成永不命中的死规则，排除保护静默失效）。
        if let Some(reason) = invalid_exclusion_reason(part) {
            bail!("排除规则段「{part}」无效：{reason}；不支持盘符、绝对路径或独立的 .. 路径段（附录 C）");
        }
        patterns.push(translate_exclusion_part(part));
    }
    Ok(Exclusions {
        patterns: regex::RegexSet::new(patterns)?,
    })
}
